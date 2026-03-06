//! [`AgentSession`] implementation backed by a Temporal workflow.
//!
//! `TemporalAgentSession` translates the `submit(Op)` / `next_event()` API
//! into Temporal signals and queries against the active [`AgentWorkflow`]
//! child, while lifecycle operations go to the parent [`SessionWorkflow`].
//!
//! ## Protocol mapping
//!
//! | Op variant        | Temporal action                                      |
//! |-------------------|------------------------------------------------------|
//! | `UserTurn`        | start SessionWorkflow (first) or signal AgentWorkflow |
//! | `Shutdown`        | signal both SessionWorkflow and AgentWorkflow         |
//! | all other Ops     | signal active AgentWorkflow                          |
//!
//! Events are retrieved by polling `get_events_since` on the active AgentWorkflow.

use std::sync::Mutex;

use codex_core::error::{CodexErr, Result as CodexResult};
use codex_protocol::config_types::{Personality, ReasoningSummary};
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::{Event, EventMsg, Op};
use codex_protocol::user_input::UserInput;
use temporalio_client::{
    Client, WorkflowQueryOptions, WorkflowSignalOptions, WorkflowStartOptions,
};

use crate::harness::{CodexHarness, CodexHarnessRun};
use crate::session_workflow::{SessionWorkflow, SessionWorkflowRun};
use crate::types::{SessionWorkflowInput, SpawnAgentInput};
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
    /// Workflow ID of the currently polled AgentWorkflow.
    active_agent_workflow_id: Mutex<String>,
    /// Session-level input (model, instructions, etc.).
    base_input: SessionWorkflowInput,
    /// Whether the session workflow has been started.
    started: Mutex<bool>,
    /// Monotonically increasing event watermark for query-based polling.
    events_index: Mutex<usize>,
    /// Local buffer of deserialized events not yet returned to the caller.
    event_buffer: Mutex<Vec<Event>>,
    /// Set when shutdown has been signaled.
    shutdown: Mutex<bool>,
    /// Monotonically increasing generation counter for stale-poll detection.
    generation: Mutex<u64>,
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
            events_index: Mutex::new(0),
            event_buffer: Mutex::new(Vec::new()),
            shutdown: Mutex::new(false),
            generation: Mutex::new(0),
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
        Self {
            client,
            session_workflow_id: Mutex::new(session_id),
            active_agent_workflow_id: Mutex::new(agent_workflow_id),
            base_input,
            started: Mutex::new(true), // already running
            events_index: Mutex::new(0),
            event_buffer: Mutex::new(Vec::new()),
            shutdown: Mutex::new(false),
            generation: Mutex::new(0),
            harness_workflow_id,
        }
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
    /// Resets the event watermark and clears the buffer so poll_events
    /// starts fresh from the new agent.
    pub fn switch_agent(&self, agent_workflow_id: String) {
        *self
            .active_agent_workflow_id
            .lock()
            .expect("lock poisoned") = agent_workflow_id;
        *self.events_index.lock().expect("lock poisoned") = 0;
        self.event_buffer.lock().expect("lock poisoned").clear();
    }

    /// Switch to a different session entirely.
    ///
    /// Bumps the generation counter (so in-flight polls are discarded),
    /// resets all internal state to point at the new session, and marks
    /// the session as started (the target session is already running).
    pub fn switch_session(&self, new_session_id: String) {
        let mut generation = self.generation.lock().expect("lock poisoned");
        *generation += 1;
        *self.session_workflow_id.lock().expect("lock poisoned") = new_session_id.clone();
        *self.active_agent_workflow_id.lock().expect("lock poisoned") =
            format!("{new_session_id}/main");
        *self.events_index.lock().expect("lock poisoned") = 0;
        self.event_buffer.lock().expect("lock poisoned").clear();
        *self.started.lock().expect("lock poisoned") = true;
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

    /// Fetch all existing events from the workflow and advance the watermark.
    ///
    /// Call **before** launching the TUI to populate `initial_messages` so
    /// the user sees the full conversation history immediately on resume.
    pub async fn fetch_initial_events(&self) -> CodexResult<Vec<EventMsg>> {
        let events = self.poll_events().await?;
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

    /// Poll the active agent workflow for new events via query.
    ///
    /// Uses a generation guard to discard stale results when a session
    /// switch occurs mid-query.
    async fn poll_events(&self) -> CodexResult<Vec<Event>> {
        let gen_before = *self.generation.lock().expect("lock poisoned");
        let from_index = *self.events_index.lock().expect("lock poisoned");
        let agent_id = self.active_agent_id();

        let handle = self
            .client
            .get_workflow_handle::<AgentWorkflowRun>(&agent_id);

        let result_json: String = handle
            .query(
                AgentWorkflow::get_events_since,
                from_index,
                WorkflowQueryOptions::default(),
            )
            .await
            .map_err(|e| {
                CodexErr::Fatal(format!("failed to query events: {e}"))
            })?;

        // Check generation after the (potentially slow) query completes.
        let gen_after = *self.generation.lock().expect("lock poisoned");
        if gen_before != gen_after {
            // Session switched while this query was in flight — discard.
            return Ok(Vec::new());
        }

        // Parse the response: { "events": [...], "watermark": N }
        let result: serde_json::Value =
            serde_json::from_str(&result_json).map_err(|e| {
                CodexErr::Fatal(format!("failed to parse query response: {e}"))
            })?;

        let watermark = result["watermark"]
            .as_u64()
            .unwrap_or(from_index as u64) as usize;

        let event_strings = result["events"]
            .as_array()
            .cloned()
            .unwrap_or_default();

        let mut events = Vec::new();
        for val in event_strings {
            let json_str = val.as_str().unwrap_or("");
            if let Ok(event) = serde_json::from_str::<Event>(json_str) {
                events.push(event);
            }
        }

        // Update watermark.
        *self.events_index.lock().expect("lock poisoned") = watermark;

        Ok(events)
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
        // First check the local buffer.
        {
            let mut buf = self.event_buffer.lock().expect("lock poisoned");
            if !buf.is_empty() {
                return Ok(buf.remove(0));
            }
        }

        // Poll with adaptive backoff.
        let mut backoff_ms = 50u64;
        let max_backoff_ms = 500u64;

        loop {
            let started = *self.started.lock().expect("lock poisoned");
            if !started {
                // Workflow not started yet — wait for a submit.
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }

            match self.poll_events().await {
                Ok(events) if !events.is_empty() => {
                    let mut buf = self.event_buffer.lock().expect("lock poisoned");
                    buf.extend(events);
                    return Ok(buf.remove(0));
                }
                Ok(_) => {
                    // No new events — backoff and retry.
                    tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                    backoff_ms = (backoff_ms * 2).min(max_backoff_ms);
                }
                Err(e) => {
                    // Query failed — could be workflow completed. Check shutdown.
                    let shutdown = *self.shutdown.lock().expect("lock poisoned");
                    if shutdown {
                        return Ok(Event {
                            id: String::new(),
                            msg: EventMsg::ShutdownComplete,
                        });
                    }
                    tracing::warn!(error = %e, "event poll failed, retrying");
                    tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                    backoff_ms = (backoff_ms * 2).min(max_backoff_ms);
                }
            }
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
            },
        })
    }

    fn current_session_id(&self) -> Option<String> {
        Some(self.session_id())
    }
}
