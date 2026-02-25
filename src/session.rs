//! [`AgentSession`] implementation backed by a Temporal workflow.
//!
//! `TemporalAgentSession` translates the `submit(Op)` / `next_event()` API
//! into Temporal signals and queries against a [`CodexWorkflow`] instance.
//!
//! ## Protocol mapping
//!
//! | Op variant        | Temporal action                              |
//! |-------------------|----------------------------------------------|
//! | `UserTurn`        | start workflow (first) or signal `receive_op` |
//! | all other Ops     | signal `receive_op`                          |
//!
//! Events are retrieved by polling `get_events_since` with adaptive backoff.

use std::sync::Mutex;

use codex_core::error::{CodexErr, Result as CodexResult};
use codex_protocol::config_types::{Personality, ReasoningSummary};
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::{Event, EventMsg, Op};
use codex_protocol::user_input::UserInput;
use temporalio_client::{
    Client, WorkflowQueryOptions, WorkflowSignalOptions, WorkflowStartOptions,
};

use crate::types::CodexWorkflowInput;
use crate::workflow::{CodexWorkflow, CodexWorkflowRun};

const TASK_QUEUE: &str = "codex-temporal";

/// An [`AgentSession`] that backs the TUI with a Temporal workflow.
pub struct TemporalAgentSession {
    client: Client,
    workflow_id: String,
    /// Workflow input template (model, instructions). The user_message field
    /// is populated from the first `Op::UserTurn`.
    base_input: CodexWorkflowInput,
    /// Whether the workflow has been started.
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
    pub fn new(client: Client, workflow_id: String, base_input: CodexWorkflowInput) -> Self {
        Self {
            client,
            workflow_id,
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
    pub fn resume(client: Client, session_id: String, base_input: CodexWorkflowInput) -> Self {
        Self {
            client,
            workflow_id: session_id,
            base_input,
            started: Mutex::new(true), // already running
            events_index: Mutex::new(0),
            event_buffer: Mutex::new(Vec::new()),
            shutdown: Mutex::new(false),
        }
    }

    /// Return the workflow ID (session ID) for this session.
    pub fn session_id(&self) -> &str {
        &self.workflow_id
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

    /// Start the workflow with the first user message.
    async fn start_workflow(
        &self,
        message: String,
        effort: Option<ReasoningEffort>,
        summary: ReasoningSummary,
        personality: Option<Personality>,
    ) -> CodexResult<String> {
        let input = CodexWorkflowInput {
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
            continued_state: None,
        };

        let options = WorkflowStartOptions::new(TASK_QUEUE, &self.workflow_id).build();

        self.client
            .start_workflow(CodexWorkflow::run, input, options)
            .await
            .map_err(|e| CodexErr::Fatal(format!("failed to start workflow: {e}")))?;

        *self.started.lock().expect("lock poisoned") = true;

        tracing::info!(
            workflow_id = %self.workflow_id,
            "workflow started"
        );

        Ok("started".to_string())
    }

    /// Signal an operation to the running workflow.
    async fn signal_op(&self, op: Op) -> CodexResult<String> {
        let handle = self
            .client
            .get_workflow_handle::<CodexWorkflowRun>(&self.workflow_id);

        handle
            .signal(
                CodexWorkflow::receive_op,
                op,
                WorkflowSignalOptions::default(),
            )
            .await
            .map_err(|e| CodexErr::Fatal(format!("failed to signal op: {e}")))?;

        Ok("ok".to_string())
    }

    /// Poll the workflow for new events via query.
    async fn poll_events(&self) -> CodexResult<Vec<Event>> {
        let from_index = *self.events_index.lock().expect("lock poisoned");

        let handle = self
            .client
            .get_workflow_handle::<CodexWorkflowRun>(&self.workflow_id);

        let result_json: String = handle
            .query(
                CodexWorkflow::get_events_since,
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
        // First UserTurn starts the workflow; all other ops are signaled.
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
                    .start_workflow(message, effort, summary, personality)
                    .await;
            }
        }

        // Track shutdown locally so next_event() can detect it.
        if matches!(op, Op::Shutdown) {
            *self.shutdown.lock().expect("lock poisoned") = true;
        }

        self.signal_op(op).await
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
