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
    /// Workflow ID of the parent SessionWorkflow.
    session_workflow_id: String,
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
        let base_input = base_input.into();
        let agent_workflow_id = format!("{workflow_id}/main");
        Self {
            client,
            session_workflow_id: workflow_id,
            active_agent_workflow_id: Mutex::new(agent_workflow_id),
            base_input,
            started: Mutex::new(false),
            events_index: Mutex::new(0),
            event_buffer: Mutex::new(Vec::new()),
            shutdown: Mutex::new(false),
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
        let base_input = base_input.into();
        let agent_workflow_id = format!("{session_id}/main");
        Self {
            client,
            session_workflow_id: session_id,
            active_agent_workflow_id: Mutex::new(agent_workflow_id),
            base_input,
            started: Mutex::new(true), // already running
            events_index: Mutex::new(0),
            event_buffer: Mutex::new(Vec::new()),
            shutdown: Mutex::new(false),
        }
    }

    /// Return the session workflow ID.
    pub fn session_id(&self) -> &str {
        &self.session_workflow_id
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

    /// Signal the SessionWorkflow to spawn a new agent.
    pub async fn spawn_agent(&self, input: SpawnAgentInput) -> CodexResult<()> {
        let handle = self
            .client
            .get_workflow_handle::<SessionWorkflowRun>(&self.session_workflow_id);

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
            WorkflowStartOptions::new(TASK_QUEUE, &self.session_workflow_id).build();

        self.client
            .start_workflow(SessionWorkflow::run, input, options)
            .await
            .map_err(|e| CodexErr::Fatal(format!("failed to start workflow: {e}")))?;

        *self.started.lock().expect("lock poisoned") = true;

        tracing::info!(
            session_workflow_id = %self.session_workflow_id,
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
        let handle = self
            .client
            .get_workflow_handle::<SessionWorkflowRun>(&self.session_workflow_id);

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
    async fn poll_events(&self) -> CodexResult<Vec<Event>> {
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
