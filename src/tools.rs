//! [`ToolCallHandler`] implementation that dispatches tool executions as
//! Temporal activities, with approval gating.
//!
//! When a tool call arrives the handler:
//! 1. Sets `pending_approval` in workflow state
//! 2. Emits an `ExecApprovalRequest` event to the event sink
//! 3. Waits for the approval decision via `wait_condition`
//! 4. If approved, executes the tool as a Temporal activity
//! 5. If denied, returns an error response

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use codex_core::error::CodexErr;
use codex_core::ToolCall;
use codex_core::ToolCallHandler;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::{AskForApproval, Event, EventMsg, ExecApprovalRequestEvent};
use codex_shell_command::is_safe_command::is_known_safe_command;
use temporalio_sdk::{ActivityOptions, WorkflowContext};
use tokio_util::sync::CancellationToken;

use crate::activities::CodexActivities;
use crate::sink::BufferEventSink;
use crate::types::{PendingApproval, ToolExecInput};
use crate::workflow::CodexWorkflow;

/// A [`ToolCallHandler`] that gates tool calls on client approval, then
/// dispatches approved calls as Temporal activities.
///
/// The approval behavior depends on the configured [`AskForApproval`] policy:
///
/// - **`Never`** — execute immediately, no approval prompt.
/// - **`OnRequest`** / **`OnFailure`** — always prompt for approval.
/// - **`UnlessTrusted`** — auto-approve commands that pass
///   `is_known_safe_command()`; prompt for everything else.
pub struct TemporalToolHandler {
    ctx: WorkflowContext<CodexWorkflow>,
    events: Arc<BufferEventSink>,
    turn_id: String,
    approval_policy: AskForApproval,
}

impl TemporalToolHandler {
    pub fn new(
        ctx: WorkflowContext<CodexWorkflow>,
        events: Arc<BufferEventSink>,
        turn_id: String,
        approval_policy: AskForApproval,
    ) -> Self {
        Self {
            ctx,
            events,
            turn_id,
            approval_policy,
        }
    }
}

impl ToolCallHandler for TemporalToolHandler {
    type Future = Pin<Box<dyn Future<Output = Result<ResponseInputItem, CodexErr>> + 'static>>;

    fn handle_tool_call(
        &self,
        call: ToolCall,
        _cancellation_token: CancellationToken,
    ) -> Self::Future {
        let ctx = self.ctx.clone();
        let events = self.events.clone();
        let turn_id = self.turn_id.clone();
        let approval_policy = self.approval_policy;

        let arguments = match &call.payload {
            codex_core::ToolPayload::Function { arguments } => arguments.clone(),
            other => format!("{other:?}"),
        };

        let call_id = call.call_id.clone();
        let tool_name = call.tool_name.clone();

        // Parse command from arguments for the approval request event.
        let command: Vec<String> = serde_json::from_str(&arguments)
            .ok()
            .and_then(|v: serde_json::Value| {
                v.get("command")?
                    .as_array()?
                    .iter()
                    .map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_else(|| vec![arguments.clone()]);

        Box::pin(async move {
            // Determine whether this call needs user approval based on policy.
            let needs_approval = match approval_policy {
                AskForApproval::Never => false,
                AskForApproval::UnlessTrusted => !is_known_safe_command(&command),
                // OnRequest and OnFailure both require explicit approval.
                AskForApproval::OnRequest | AskForApproval::OnFailure => true,
            };

            if needs_approval {
                // 1. Set pending approval in workflow state
                ctx.state_mut(|s| {
                    s.pending_approval = Some(PendingApproval {
                        call_id: call_id.clone(),
                        decision: None,
                    });
                });

                // 2. Emit ExecApprovalRequest event
                let approval_event = Event {
                    id: turn_id.clone(),
                    msg: EventMsg::ExecApprovalRequest(ExecApprovalRequestEvent {
                        call_id: call_id.clone(),
                        approval_id: Some(call_id.clone()),
                        turn_id,
                        command,
                        cwd: PathBuf::from("/tmp"),
                        reason: None,
                        network_approval_context: None,
                        proposed_execpolicy_amendment: None,
                        parsed_cmd: Vec::new(),
                    }),
                };
                events.emit_event_sync(approval_event);

                // 3. Wait for approval decision
                ctx.wait_condition(|s| {
                    s.pending_approval
                        .as_ref()
                        .map_or(true, |p| p.decision.is_some())
                })
                .await;

                // 4. Check decision
                let approved = ctx.state_mut(|s| {
                    let decision = s
                        .pending_approval
                        .as_ref()
                        .and_then(|p| p.decision)
                        .unwrap_or(false);
                    s.pending_approval = None;
                    decision
                });

                if !approved {
                    return Ok(denied_response(call_id));
                }
            }

            // 5. Execute tool as activity
            let input = ToolExecInput {
                tool_name,
                call_id: call_id.clone(),
                arguments,
            };

            let opts = ActivityOptions {
                start_to_close_timeout: Some(Duration::from_secs(600)),
                heartbeat_timeout: Some(Duration::from_secs(30)),
                ..Default::default()
            };

            let output = ctx
                .start_activity(CodexActivities::tool_exec, input, opts)
                .await
                .map_err(|e| CodexErr::Fatal(format!("tool_exec activity failed: {e}")))?;

            Ok(output.into_response_input_item())
        })
    }
}

/// Build a function_call_output indicating the tool call was denied.
fn denied_response(call_id: String) -> ResponseInputItem {
    use codex_protocol::models::{FunctionCallOutputBody, FunctionCallOutputPayload};

    let text = serde_json::json!({
        "output": "Tool execution was denied by the user.",
        "metadata": { "exit_code": 1, "duration_seconds": 0.0 }
    })
    .to_string();

    ResponseInputItem::FunctionCallOutput {
        call_id,
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text(text),
            success: Some(false),
        },
    }
}
