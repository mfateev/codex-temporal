//! [`AgentSession`] implementation backed by a Temporal workflow.
//!
//! `TemporalAgentSession` translates the `submit(Op)` / `next_event()` API
//! into Temporal signals and blocking updates against the active
//! [`AgentWorkflow`] child, while lifecycle operations go to the parent
//! [`SessionWorkflow`].
//!
//! ## Protocol mapping
//!
//! | Op variant        | Temporal action                                      |
//! |-------------------|------------------------------------------------------|
//! | `UserTurn`        | start SessionWorkflow (first) or signal AgentWorkflow |
//! | `Shutdown`        | signal both SessionWorkflow and AgentWorkflow         |
//! | all other Ops     | signal active AgentWorkflow                          |
//!
//! Events are received via a background [`Watcher`] that calls the blocking
//! `get_state_update` update handler in a loop.

use std::sync::{Arc, Mutex};

use codex_core::error::{CodexErr, Result as CodexResult};
use codex_protocol::config_types::{Personality, ReasoningSummary};
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::{Event, EventMsg, Op};
use codex_protocol::user_input::UserInput;
use temporalio_client::{
    Client, WorkflowExecuteUpdateOptions, WorkflowQueryOptions, WorkflowSignalOptions,
    WorkflowStartOptions,
};

use crate::harness::{CodexHarness, CodexHarnessRun};
use crate::session_workflow::{SessionWorkflow, SessionWorkflowRun};
use crate::types::{SessionWorkflowInput, SpawnAgentInput, StateUpdateRequest};
use crate::watcher::{Watcher, WatcherEvent};
use crate::workflow::{AgentWorkflow, AgentWorkflowRun};

const TASK_QUEUE: &str = "codex-temporal";

/// An [`AgentSession`] that backs the TUI with a Temporal workflow.
///
/// Routes events through the active `AgentWorkflow` child, while lifecycle
/// operations (start, shutdown, spawn) go to the parent `SessionWorkflow`.
pub struct TemporalAgentSession {
    client: Client,
    /// Workflow ID of the parent SessionWorkflow (mutable for session switching).
    session_workflow_id: Mutex<String>,
    /// Workflow ID of the currently watched AgentWorkflow.
    active_agent_workflow_id: Mutex<String>,
    /// Session-level input (model, instructions, etc.).
    base_input: SessionWorkflowInput,
    /// Whether the session workflow has been started.
    started: Mutex<bool>,
    /// Local buffer of deserialized events not yet returned to the caller.
    event_buffer: Arc<Mutex<Vec<Event>>>,
    /// Notified when new events are pushed into `event_buffer`.
    event_notify: Arc<tokio::sync::Notify>,
    /// Set when shutdown has been signaled.
    shutdown: Mutex<bool>,
    /// Monotonically increasing generation counter for stale-data detection.
    generation: Arc<Mutex<u64>>,
    /// Handle to cancel the background Watcher when switching sessions.
    watch_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Workflow ID of the CodexHarness (for querying session list).
    harness_workflow_id: String,
}

impl TemporalAgentSession {
    /// Create a new session. The workflow is not started until the first
    /// `Op::UserTurn` is submitted.
    ///
    /// Accepts both `SessionWorkflowInput` and `CodexWorkflowInput` (via
    /// the `Into<SessionWorkflowInput>` conversion).
    pub fn new(
        client: Client,
        workflow_id: String,
        base_input: impl Into<SessionWorkflowInput>,
    ) -> Self {
        Self::new_with_harness(client, workflow_id, base_input, None)
    }

    /// Create a new session with an explicit harness workflow ID.
    ///
    /// When `harness_id` is `None`, the default user-derived ID is used.
    pub fn new_with_harness(
        client: Client,
        workflow_id: String,
        base_input: impl Into<SessionWorkflowInput>,
        harness_id: Option<String>,
    ) -> Self {
        let base_input = base_input.into();
        let agent_workflow_id = format!("{workflow_id}/main");
        let harness_workflow_id = harness_id.unwrap_or_else(derive_harness_workflow_id);
        Self {
            client,
            session_workflow_id: Mutex::new(workflow_id),
            active_agent_workflow_id: Mutex::new(agent_workflow_id),
            base_input,
            started: Mutex::new(false),
            event_buffer: Arc::new(Mutex::new(Vec::new())),
            event_notify: Arc::new(tokio::sync::Notify::new()),
            shutdown: Mutex::new(false),
            generation: Arc::new(Mutex::new(0)),
            watch_handle: Mutex::new(None),
            harness_workflow_id,
        }
    }

    /// Attach to an existing running workflow.
    ///
    /// Unlike `new()`, the workflow is assumed to already be running, so the
    /// first `Op::UserTurn` will be signalled rather than starting a workflow.
    ///
    /// Accepts both `SessionWorkflowInput` and `CodexWorkflowInput`.
    pub fn resume(
        client: Client,
        session_id: String,
        base_input: impl Into<SessionWorkflowInput>,
    ) -> Self {
        Self::resume_with_harness(client, session_id, base_input, None)
    }

    /// Attach to an existing running workflow with an explicit harness workflow ID.
    pub fn resume_with_harness(
        client: Client,
        session_id: String,
        base_input: impl Into<SessionWorkflowInput>,
        harness_id: Option<String>,
    ) -> Self {
        let base_input = base_input.into();
        let agent_workflow_id = format!("{session_id}/main");
        let harness_workflow_id = harness_id.unwrap_or_else(derive_harness_workflow_id);
        let session = Self {
            client,
            session_workflow_id: Mutex::new(session_id),
            active_agent_workflow_id: Mutex::new(agent_workflow_id),
            base_input,
            started: Mutex::new(true), // already running
            event_buffer: Arc::new(Mutex::new(Vec::new())),
            event_notify: Arc::new(tokio::sync::Notify::new()),
            shutdown: Mutex::new(false),
            generation: Arc::new(Mutex::new(0)),
            watch_handle: Mutex::new(None),
            harness_workflow_id,
        };
        session.start_watching();
        session
    }

    /// Return the session workflow ID.
    pub fn session_id(&self) -> String {
        self.session_workflow_id
            .lock()
            .expect("lock poisoned")
            .clone()
    }

    /// Return the active agent workflow ID.
    pub fn active_agent_id(&self) -> String {
        self.active_agent_workflow_id
            .lock()
            .expect("lock poisoned")
            .clone()
    }

    /// Switch the active agent to a different child workflow.
    ///
    /// Stops the current watcher, clears the buffer, and starts watching
    /// the new agent.
    pub fn switch_agent(&self, agent_workflow_id: String) {
        self.stop_watching();
        *self
            .active_agent_workflow_id
            .lock()
            .expect("lock poisoned") = agent_workflow_id;
        self.event_buffer.lock().expect("lock poisoned").clear();
        self.start_watching();
    }

    /// Switch to a different session entirely.
    ///
    /// Bumps the generation counter (so in-flight data is discarded),
    /// resets all internal state to point at the new session, and starts
    /// a new watcher.
    pub fn switch_session(&self, new_session_id: String) {
        self.stop_watching();
        *self.generation.lock().expect("lock poisoned") += 1;
        *self.session_workflow_id.lock().expect("lock poisoned") = new_session_id.clone();
        *self.active_agent_workflow_id.lock().expect("lock poisoned") =
            format!("{new_session_id}/main");
        self.event_buffer.lock().expect("lock poisoned").clear();
        *self.started.lock().expect("lock poisoned") = true;
        self.start_watching();
    }

    /// Start the background watcher for the active agent workflow.
    fn start_watching(&self) {
        self.stop_watching();
        let client = self.client.clone();
        let workflow_id = self.active_agent_id();
        let buffer = Arc::clone(&self.event_buffer);
        let notify = Arc::clone(&self.event_notify);
        let generation = Arc::clone(&self.generation);
        let gen_at_start = *generation.lock().expect("lock poisoned");

        let handle = tokio::spawn(async move {
            let watcher = Watcher::new(client, workflow_id);
            let (tx, mut rx) = tokio::sync::mpsc::channel(64);

            tokio::spawn(async move {
                watcher.run_watching(tx).await;
            });

            while let Some(result) = rx.recv().await {
                // Check generation for staleness.
                if *generation.lock().expect("lock poisoned") != gen_at_start {
                    return;
                }
                match result {
                    WatcherEvent::Events(events, _watermark, _version) => {
                        if !events.is_empty() {
                            buffer.lock().expect("lock poisoned").extend(events);
                            notify.notify_one();
                        }
                    }
                    WatcherEvent::Completed => {
                        buffer.lock().expect("lock poisoned").push(Event {
                            id: String::new(),
                            msg: EventMsg::ShutdownComplete,
                        });
                        notify.notify_one();
                        return;
                    }
                    WatcherEvent::Error(e) => {
                        tracing::error!(error = %e, "watcher terminated with error");
                        notify.notify_one();
                        return;
                    }
                }
            }
        });

        *self.watch_handle.lock().expect("lock poisoned") = Some(handle);
    }

    /// Stop the background watcher.
    fn stop_watching(&self) {
        if let Some(handle) = self.watch_handle.lock().expect("lock poisoned").take() {
            handle.abort();
        }
    }

    /// Query the harness for the list of known sessions.
    pub async fn query_sessions(&self) -> CodexResult<Vec<crate::types::SessionEntry>> {
        let handle = self
            .client
            .get_workflow_handle::<CodexHarnessRun>(&self.harness_workflow_id);

        let json: String = handle
            .query(
                CodexHarness::list_sessions,
                (),
                WorkflowQueryOptions::default(),
            )
            .await
            .map_err(|e| CodexErr::Fatal(format!("failed to query sessions: {e}")))?;

        let entries: Vec<crate::types::SessionEntry> =
            serde_json::from_str(&json).unwrap_or_default();
        Ok(entries)
    }

    /// Signal the SessionWorkflow to spawn a new agent.
    pub async fn spawn_agent(&self, input: SpawnAgentInput) -> CodexResult<()> {
        let session_id = self.session_id();
        let handle = self
            .client
            .get_workflow_handle::<SessionWorkflowRun>(&session_id);

        handle
            .signal(
                SessionWorkflow::spawn_agent,
                input,
                WorkflowSignalOptions::default(),
            )
            .await
            .map_err(|e| CodexErr::Fatal(format!("failed to signal spawn_agent: {e}")))?;

        Ok(())
    }

    /// Fetch all existing events from the workflow via a one-shot
    /// `get_state_update` call.
    ///
    /// Call **before** launching the TUI to populate `initial_messages` so
    /// the user sees the full conversation history immediately on resume.
    pub async fn fetch_initial_events(&self) -> CodexResult<Vec<EventMsg>> {
        let agent_id = self.active_agent_id();
        let handle = self
            .client
            .get_workflow_handle::<AgentWorkflowRun>(&agent_id);

        let resp = handle
            .execute_update(
                AgentWorkflow::get_state_update,
                StateUpdateRequest {
                    since_index: 0,
                    since_version: 0,
                },
                WorkflowExecuteUpdateOptions::default(),
            )
            .await
            .map_err(|e| CodexErr::Fatal(format!("failed to fetch initial events: {e}")))?;

        let events: Vec<Event> = resp
            .events
            .iter()
            .filter_map(|s| serde_json::from_str(s).ok())
            .collect();

        Ok(events.into_iter().map(|e| e.msg).collect())
    }

    /// Extract the text message from user input items.
    fn extract_message(items: &[UserInput]) -> String {
        items
            .iter()
            .filter_map(|item| match item {
                UserInput::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Start the SessionWorkflow with the first user message.
    async fn start_workflow(
        &self,
        message: String,
        effort: Option<ReasoningEffort>,
        summary: ReasoningSummary,
        personality: Option<Personality>,
    ) -> CodexResult<String> {
        let session_id = self.session_id();
        let input = SessionWorkflowInput {
            user_message: message,
            model: self.base_input.model.clone(),
            instructions: self.base_input.instructions.clone(),
            approval_policy: self.base_input.approval_policy,
            web_search_mode: self.base_input.web_search_mode,
            reasoning_effort: effort.or(self.base_input.reasoning_effort),
            reasoning_summary: if summary != ReasoningSummary::default() {
                summary
            } else {
                self.base_input.reasoning_summary
            },
            personality: personality.or(self.base_input.personality),
            developer_instructions: self.base_input.developer_instructions.clone(),
            model_provider: self.base_input.model_provider.clone(),
            crew_agents: self.base_input.crew_agents.clone(),
            continued_state: None,
        };

        let options =
            WorkflowStartOptions::new(TASK_QUEUE, &session_id).build();

        self.client
            .start_workflow(SessionWorkflow::run, input, options)
            .await
            .map_err(|e| CodexErr::Fatal(format!("failed to start workflow: {e}")))?;

        *self.started.lock().expect("lock poisoned") = true;
        self.start_watching();

        tracing::info!(
            session_workflow_id = %session_id,
            "session workflow started"
        );

        Ok("started".to_string())
    }

    /// Signal an operation to the active agent workflow.
    async fn signal_agent_op(&self, op: Op) -> CodexResult<String> {
        let agent_id = self.active_agent_id();
        let handle = self
            .client
            .get_workflow_handle::<AgentWorkflowRun>(&agent_id);

        handle
            .signal(
                AgentWorkflow::receive_op,
                op,
                WorkflowSignalOptions::default(),
            )
            .await
            .map_err(|e| CodexErr::Fatal(format!("failed to signal op to agent: {e}")))?;

        Ok("ok".to_string())
    }

    /// Signal shutdown to the SessionWorkflow.
    async fn signal_session_shutdown(&self) -> CodexResult<()> {
        let session_id = self.session_id();
        let handle = self
            .client
            .get_workflow_handle::<SessionWorkflowRun>(&session_id);

        handle
            .signal(
                SessionWorkflow::shutdown,
                (),
                WorkflowSignalOptions::default(),
            )
            .await
            .map_err(|e| {
                CodexErr::Fatal(format!("failed to signal session shutdown: {e}"))
            })?;

        Ok(())
    }
}

/// Derive the harness workflow ID for the current user.
fn derive_harness_workflow_id() -> String {
    let user = std::env::var("USER").unwrap_or_else(|_| "default".to_string());
    format!("codex-harness-{user}")
}

/// Filter replayed events into the set suitable for `initial_messages`.
pub fn filter_initial_events(events: Vec<EventMsg>) -> Vec<EventMsg> {
    events
        .into_iter()
        .filter(|e| {
            matches!(
                e,
                EventMsg::AgentMessage(_)
                    | EventMsg::TurnStarted(_)
                    | EventMsg::TurnComplete(_)
                    | EventMsg::ExecApprovalRequest(_)
                    | EventMsg::ExecCommandBegin(_)
            )
        })
        .collect()
}

#[async_trait::async_trait]
impl codex_core::AgentSession for TemporalAgentSession {
    async fn submit(&self, op: Op) -> CodexResult<String> {
        // First UserTurn starts the SessionWorkflow; subsequent ops go to agent.
        if let Op::UserTurn {
            ref items,
            effort,
            summary,
            personality,
            ..
        } = op
        {
            let started = *self.started.lock().expect("lock poisoned");
            if !started {
                let message = Self::extract_message(items);
                return self
                    .start_workflow(
                        message,
                        effort,
                        summary.unwrap_or_default(),
                        personality,
                    )
                    .await;
            }
        }

        // Track shutdown locally so next_event() can detect it.
        if matches!(op, Op::Shutdown) {
            *self.shutdown.lock().expect("lock poisoned") = true;
            // Signal shutdown to both the agent and session workflows.
            let _ = self.signal_agent_op(op.clone()).await;
            let _ = self.signal_session_shutdown().await;
            return Ok("ok".to_string());
        }

        self.signal_agent_op(op).await
    }

    async fn next_event(&self) -> CodexResult<Event> {
        loop {
            // Check the local buffer first.
            {
                let mut buf = self.event_buffer.lock().expect("lock poisoned");
                if !buf.is_empty() {
                    return Ok(buf.remove(0));
                }
            }

            // If not started, wait a bit (watcher not yet running).
            let started = *self.started.lock().expect("lock poisoned");
            if !started {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }

            // Wait for the watcher to push events.
            self.event_notify.notified().await;
        }
    }
}

#[async_trait::async_trait]
impl codex_tui::ExternalAgentBrowser for TemporalAgentSession {
    async fn list_sessions(&self) -> Vec<codex_tui::ExternalSessionEntry> {
        let current_id = self.session_id();
        match self.query_sessions().await {
            Ok(entries) => entries
                .into_iter()
                .map(|e| {
                    let is_current = e.session_id == current_id;
                    let is_closed = e.status != crate::types::SessionStatus::Running;
                    let description = {
                        let status_str = if is_closed { " [closed]" } else { "" };
                        Some(format!("{}{}", e.model, status_str))
                    };
                    codex_tui::ExternalSessionEntry {
                        id: e.session_id,
                        name: e.name.unwrap_or_else(|| "(unnamed)".to_string()),
                        description,
                        is_current,
                        is_closed,
                    }
                })
                .collect(),
            Err(err) => {
                tracing::error!(%err, "failed to query sessions from harness");
                Vec::new()
            }
        }
    }

    async fn switch_to(
        &self,
        session_id: &str,
    ) -> color_eyre::eyre::Result<codex_tui::ExternalSwitchResult> {
        use codex_protocol::protocol::{SandboxPolicy, SessionConfiguredEvent};
        use codex_protocol::ThreadId;

        self.switch_session(session_id.to_string());

        // Fetch existing events from the new session to seed initial_messages.
        let events = self
            .fetch_initial_events()
            .await
            .map_err(|e| color_eyre::eyre::eyre!("failed to fetch initial events: {e}"))?;
        let filtered = filter_initial_events(events);

        Ok(codex_tui::ExternalSwitchResult {
            session_configured: SessionConfiguredEvent {
                session_id: ThreadId::new(),
                forked_from_id: None,
                thread_name: None,
                initial_messages: Some(filtered),
                model: self.base_input.model.clone(),
                model_provider_id: "openai".into(),
                approval_policy: self.base_input.approval_policy,
                sandbox_policy: SandboxPolicy::DangerFullAccess,
                cwd: std::env::current_dir().unwrap_or_default(),
                reasoning_effort: self.base_input.reasoning_effort,
                history_log_id: 0,
                history_entry_count: 0,
                network_proxy: None,
                rollout_path: None,
                service_tier: None,
            },
        })
    }

    fn current_session_id(&self) -> Option<String> {
        Some(self.session_id())
    }
}
