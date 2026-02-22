//! [`AgentSession`] implementation backed by a Temporal workflow.
//!
//! `TemporalAgentSession` translates the `submit(Op)` / `next_event()` API
//! into Temporal signals and queries against a [`CodexWorkflow`] instance.
//!
//! ## Protocol mapping
//!
//! | Op variant        | Temporal action                              |
//! |-------------------|----------------------------------------------|
//! | `UserTurn`        | start workflow (first) or signal `receive_user_turn` |
//! | `ExecApproval`    | signal `receive_approval`                    |
//! | `Shutdown`        | signal `request_shutdown`                    |
//! | other             | logged + ignored                             |
//!
//! Events are retrieved by polling `get_events_since` with adaptive backoff.

use std::sync::Mutex;

use codex_core::error::{CodexErr, Result as CodexResult};
use codex_protocol::protocol::{Event, EventMsg, Op, ReviewDecision};
use temporalio_client::{
    Client, WorkflowQueryOptions, WorkflowSignalOptions, WorkflowStartOptions,
};

use crate::types::{ApprovalInput, CodexWorkflowInput, UserTurnInput};
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
    /// Turn counter for generating turn IDs.
    turn_counter: Mutex<u32>,
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
            turn_counter: Mutex::new(0),
            shutdown: Mutex::new(false),
        }
    }

    /// Extract the text message from user input items.
    fn extract_message(items: &[codex_protocol::user_input::UserInput]) -> String {
        items
            .iter()
            .filter_map(|item| match item {
                codex_protocol::user_input::UserInput::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Generate a new turn ID.
    fn next_turn_id(&self) -> String {
        let mut counter = self.turn_counter.lock().expect("lock poisoned");
        *counter += 1;
        format!("turn-{}", *counter)
    }

    /// Start the workflow with the first user message.
    async fn start_workflow(&self, message: String) -> CodexResult<String> {
        let turn_id = {
            let mut counter = self.turn_counter.lock().expect("lock poisoned");
            *counter += 1;
            format!("turn-{}", *counter)
        };

        let input = CodexWorkflowInput {
            user_message: message,
            model: self.base_input.model.clone(),
            instructions: self.base_input.instructions.clone(),
            approval_policy: self.base_input.approval_policy,
            web_search_mode: self.base_input.web_search_mode,
        };

        let options = WorkflowStartOptions::new(TASK_QUEUE, &self.workflow_id).build();

        self.client
            .start_workflow(CodexWorkflow::run, input, options)
            .await
            .map_err(|e| CodexErr::Fatal(format!("failed to start workflow: {e}")))?;

        *self.started.lock().expect("lock poisoned") = true;

        tracing::info!(
            workflow_id = %self.workflow_id,
            turn_id = %turn_id,
            "workflow started"
        );

        Ok(turn_id)
    }

    /// Signal a new user turn to an already-running workflow.
    async fn signal_user_turn(&self, message: String) -> CodexResult<String> {
        let turn_id = self.next_turn_id();

        let handle = self
            .client
            .get_workflow_handle::<CodexWorkflowRun>(&self.workflow_id);

        let input = UserTurnInput {
            turn_id: turn_id.clone(),
            message,
        };

        handle
            .signal(
                CodexWorkflow::receive_user_turn,
                input,
                WorkflowSignalOptions::default(),
            )
            .await
            .map_err(|e| CodexErr::Fatal(format!("failed to signal user turn: {e}")))?;

        Ok(turn_id)
    }

    /// Signal approval/denial for a pending tool call.
    async fn signal_approval(
        &self,
        call_id: String,
        approved: bool,
    ) -> CodexResult<String> {
        let handle = self
            .client
            .get_workflow_handle::<CodexWorkflowRun>(&self.workflow_id);

        let input = ApprovalInput {
            call_id: call_id.clone(),
            approved,
        };

        handle
            .signal(
                CodexWorkflow::receive_approval,
                input,
                WorkflowSignalOptions::default(),
            )
            .await
            .map_err(|e| CodexErr::Fatal(format!("failed to signal approval: {e}")))?;

        Ok(call_id)
    }

    /// Signal shutdown to the workflow.
    async fn signal_shutdown(&self) -> CodexResult<String> {
        let handle = self
            .client
            .get_workflow_handle::<CodexWorkflowRun>(&self.workflow_id);

        handle
            .signal(
                CodexWorkflow::request_shutdown,
                (),
                WorkflowSignalOptions::default(),
            )
            .await
            .map_err(|e| CodexErr::Fatal(format!("failed to signal shutdown: {e}")))?;

        *self.shutdown.lock().expect("lock poisoned") = true;

        Ok("shutdown".to_string())
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
        match op {
            Op::UserTurn { items, .. } => {
                let message = Self::extract_message(&items);
                let started = *self.started.lock().expect("lock poisoned");
                if started {
                    self.signal_user_turn(message).await
                } else {
                    self.start_workflow(message).await
                }
            }

            Op::ExecApproval { id, decision, .. } => {
                let approved = matches!(
                    decision,
                    ReviewDecision::Approved
                        | ReviewDecision::ApprovedForSession
                        | ReviewDecision::ApprovedExecpolicyAmendment { .. }
                );
                self.signal_approval(id, approved).await
            }

            Op::Shutdown => self.signal_shutdown().await,

            Op::Interrupt => {
                tracing::warn!("Op::Interrupt not yet implemented for Temporal session");
                Ok("interrupt-noop".to_string())
            }

            // The TUI sends these during normal operation. In the Temporal
            // context they are no-ops — the worker handles execution, there
            // are no local MCP servers, custom prompts, skills, or history.
            Op::ListCustomPrompts
            | Op::ListSkills { .. }
            | Op::ListMcpTools
            | Op::AddToHistory { .. }
            | Op::CleanBackgroundTerminals
            | Op::DropMemories
            | Op::UpdateMemories
            | Op::ReloadUserConfig
            | Op::Review { .. }
            | Op::RunUserShellCommand { .. }
            | Op::Compact
            | Op::SetThreadName { .. }
            | Op::Undo => Ok("noop".to_string()),

            other => {
                tracing::warn!(?other, "unhandled Op variant in TemporalAgentSession");
                Ok("noop".to_string())
            }
        }
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
