//! [`ToolCallHandler`] implementation that dispatches tool executions as
//! Temporal activities, with approval gating.
//!
//! When a tool call arrives the handler:
//! 1. Sets `pending_approval` in workflow state
//! 2. Emits an `ExecApprovalRequest` event to the event sink
//! 3. Waits for the approval decision via `wait_condition`
//! 4. If approved, executes the tool as a Temporal activity
//! 5. If denied, returns an error response

use std::collections::HashSet;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use codex_core::error::CodexErr;
use codex_core::exec_policy::render_decision_for_unmatched_command;
use codex_core::ExecPolicyDecision as Decision;
use codex_core::ToolCall;
use codex_core::ToolCallHandler;
use codex_protocol::models::{FunctionCallOutputPayload, ResponseInputItem, SandboxPermissions};
use codex_protocol::approvals::{ElicitationRequest, ElicitationRequestEvent};
use codex_protocol::dynamic_tools::DynamicToolCallRequest;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::protocol::{
    AskForApproval, ApplyPatchApprovalRequestEvent, Event, EventMsg, ExecApprovalRequestEvent,
    SandboxPolicy,
};
use codex_protocol::request_user_input::{RequestUserInputArgs, RequestUserInputEvent};
use temporalio_common::protos::coresdk::workflow_commands::ActivityCancellationType;
use temporalio_sdk::{ActivityOptions, CancellableFuture, WorkflowContext};
use tokio_util::sync::CancellationToken;

use crate::activities::CodexActivities;
use crate::sink::BufferEventSink;
use crate::types::{
    McpToolCallInput, PendingApproval, PendingDynamicTool, PendingElicitation,
    PendingPatchApproval, PendingUserInput, ToolExecInput,
};
use crate::workflow::AgentWorkflow;

/// A [`ToolCallHandler`] that gates tool calls on client approval, then
/// dispatches approved calls as Temporal activities.
///
/// Shell approval uses codex-core's `render_decision_for_unmatched_command`
/// which accounts for sandbox type, file-system sandbox policy, dangerous
/// command detection, and granular config.
///
/// Patch approval uses a workflow-side policy pre-check with full
/// `assess_patch_safety` deferred to the activity.
pub struct TemporalToolHandler {
    ctx: WorkflowContext<AgentWorkflow>,
    events: Arc<BufferEventSink>,
    turn_id: String,
    approval_policy: AskForApproval,
    model: String,
    cwd: String,
    /// Merged config TOML string to forward to tool-execution activities.
    config_toml: Option<String>,
    /// Set of qualified MCP tool names (e.g. "mcp__echo__echo").
    /// Tool calls matching these names bypass approval and route to MCP.
    mcp_tool_names: HashSet<String>,
    /// Set of dynamic tool names (client-defined tools handled via signal/wait).
    dynamic_tool_names: HashSet<String>,
    /// Sandbox policy from the user's config — used by
    /// `render_decision_for_unmatched_command` to determine approval.
    sandbox_policy: SandboxPolicy,
    /// Derived file-system sandbox policy for approval decisions.
    file_system_sandbox_policy: FileSystemSandboxPolicy,
}

impl TemporalToolHandler {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        ctx: WorkflowContext<AgentWorkflow>,
        events: Arc<BufferEventSink>,
        turn_id: String,
        approval_policy: AskForApproval,
        model: String,
        cwd: String,
        config_toml: Option<String>,
        mcp_tool_names: HashSet<String>,
        dynamic_tool_names: HashSet<String>,
        sandbox_policy: SandboxPolicy,
    ) -> Self {
        let file_system_sandbox_policy = FileSystemSandboxPolicy::from(&sandbox_policy);
        Self {
            ctx,
            events,
            turn_id,
            approval_policy,
            model,
            cwd,
            config_toml,
            mcp_tool_names,
            dynamic_tool_names,
            sandbox_policy,
            file_system_sandbox_policy,
        }
    }
}

impl ToolCallHandler for TemporalToolHandler {
    type Future = Pin<Box<dyn Future<Output = Result<ResponseInputItem, CodexErr>> + 'static>>;

    fn handle_tool_call(
        &self,
        call: ToolCall,
        cancellation_token: CancellationToken,
    ) -> Self::Future {
        let ctx = self.ctx.clone();
        let events = self.events.clone();
        let turn_id = self.turn_id.clone();
        let approval_policy = self.approval_policy;
        let sandbox_policy = self.sandbox_policy.clone();
        let file_system_sandbox_policy = self.file_system_sandbox_policy.clone();

        let (arguments, payload_kind) = match &call.payload {
            codex_core::ToolPayload::Function { arguments } => {
                (arguments.clone(), "function".to_string())
            }
            codex_core::ToolPayload::Custom { input } => {
                (input.clone(), "custom".to_string())
            }
            codex_core::ToolPayload::Mcp { raw_arguments, .. } => {
                (raw_arguments.clone(), "mcp".to_string())
            }
            codex_core::ToolPayload::LocalShell { params } => {
                (params.command.join(" "), "local_shell".to_string())
            }
            codex_core::ToolPayload::ToolSearch { arguments } => {
                (arguments.query.clone(), "tool_search".to_string())
            }
        };

        let call_id = call.call_id.clone();
        let tool_name = call.tool_name.clone();
        let model = self.model.clone();
        let cwd = self.cwd.clone();
        let config_toml = self.config_toml.clone();
        let is_mcp_tool = self.mcp_tool_names.contains(&tool_name);
        let is_dynamic_tool = self.dynamic_tool_names.contains(&tool_name);

        // Parse command from arguments for the approval request event.
        // Handles both `shell`-style {"command": [...]} and
        // `exec_command`-style {"cmd": "..."} argument formats.
        let command: Vec<String> = serde_json::from_str(&arguments)
            .ok()
            .and_then(|v: serde_json::Value| {
                // `shell` tool uses {"command": ["cmd", "arg", ...]}
                if let Some(arr) = v.get("command").and_then(|c| c.as_array()) {
                    let items: Option<Vec<String>> = arr
                        .iter()
                        .map(|v| v.as_str().map(String::from))
                        .collect();
                    return items;
                }
                // `exec_command` uses {"cmd": "cmd arg ..."}
                if let Some(cmd_str) = v.get("cmd").and_then(|c| c.as_str()) {
                    return Some(cmd_str.split_whitespace().map(String::from).collect());
                }
                None
            })
            .unwrap_or_else(|| vec![arguments.clone()]);

        Box::pin(async move {
            // MCP tools bypass the approval flow — the user explicitly
            // configured these servers, so they are trusted.
            if is_mcp_tool {
                let mcp_input = McpToolCallInput {
                    qualified_name: tool_name,
                    call_id: call_id.clone(),
                    arguments,
                };

                let opts = ActivityOptions {
                    start_to_close_timeout: Some(Duration::from_secs(120)),
                    heartbeat_timeout: Some(Duration::from_secs(30)),
                    cancellation_type: ActivityCancellationType::TryCancel,
                    ..Default::default()
                };

                let activity =
                    ctx.start_activity(CodexActivities::mcp_tool_call, mcp_input, opts);
                tokio::pin!(activity);
                let mut output = tokio::select! {
                    biased;
                    _ = cancellation_token.cancelled() => {
                        activity.cancel();
                        return Ok(denied_response(call_id));
                    }
                    result = &mut activity => result.map_err(|e| {
                        CodexErr::Fatal(format!("mcp_tool_call activity failed: {e}"))
                    })?,
                };

                // Check if the MCP server requested elicitation during this call.
                if let Some(elicitation) = output.elicitation.take() {
                    // Set pending state.
                    ctx.state_mut(|s| {
                        s.pending_elicitation = Some(PendingElicitation {
                            server_name: elicitation.server_name.clone(),
                            request_id: elicitation.request_id.clone(),
                            response: None,
                        });
                    });

                    // Emit ElicitationRequest event.
                    events.emit_event_sync(Event {
                        id: turn_id.clone(),
                        msg: EventMsg::ElicitationRequest(ElicitationRequestEvent {
                            turn_id: Some(turn_id.clone()),
                            server_name: elicitation.server_name,
                            id: elicitation.request_id,
                            request: ElicitationRequest::Form {
                                meta: None,
                                message: elicitation.message,
                                requested_schema: serde_json::Value::Object(Default::default()),
                            },
                        }),
                    });
                    ctx.state_mut(|s| s.bump_version());

                    // Wait for resolution or interrupt.
                    ctx.wait_condition(|s| {
                        s.pending_elicitation
                            .as_ref()
                            .is_none_or(|p| p.response.is_some())
                            || s.interrupt_requested
                    })
                    .await;

                    // Handle interrupt.
                    let interrupted = ctx.state(|s| s.interrupt_requested);
                    if interrupted {
                        ctx.state_mut(|s| s.pending_elicitation = None);
                        return Ok(denied_response(call_id));
                    }

                    // Clear pending state.
                    ctx.state_mut(|s| s.pending_elicitation = None);
                }

                return Ok(output.into_response_input_item());
            }
            // request_user_input — intercept and handle via signal/wait
            // (same pattern as exec approval).
            if tool_name == "request_user_input" {
                let args: RequestUserInputArgs = serde_json::from_str(&arguments)
                    .map_err(|e| {
                        CodexErr::Fatal(format!("invalid request_user_input args: {e}"))
                    })?;

                // Set pending state.
                ctx.state_mut(|s| {
                    s.pending_user_input = Some(PendingUserInput {
                        call_id: call_id.clone(),
                        turn_id: turn_id.clone(),
                        response: None,
                    });
                });

                // Emit event to client.
                events.emit_event_sync(Event {
                    id: turn_id.clone(),
                    msg: EventMsg::RequestUserInput(RequestUserInputEvent {
                        call_id: call_id.clone(),
                        turn_id,
                        questions: args.questions,
                    }),
                });
                ctx.state_mut(|s| s.bump_version());

                // Wait for response or interrupt.
                ctx.wait_condition(|s| {
                    s.pending_user_input
                        .as_ref()
                        .is_none_or(|p| p.response.is_some())
                        || s.interrupt_requested
                })
                .await;

                // Handle interrupt.
                let interrupted = ctx.state(|s| s.interrupt_requested);
                if interrupted {
                    ctx.state_mut(|s| s.pending_user_input = None);
                    let output = serde_json::json!({"error": "interrupted"}).to_string();
                    return Ok(ResponseInputItem::FunctionCallOutput {
                        call_id,
                        output: FunctionCallOutputPayload::from_text(output),
                    });
                }

                // Extract response and return as tool output.
                let response = ctx.state_mut(|s| {
                    let resp = s
                        .pending_user_input
                        .as_ref()
                        .and_then(|p| p.response.clone());
                    s.pending_user_input = None;
                    resp
                });

                let output = serde_json::to_string(&response).unwrap_or_default();
                return Ok(ResponseInputItem::FunctionCallOutput {
                    call_id,
                    output: FunctionCallOutputPayload::from_text(output),
                });
            }

            // Dynamic tools — intercept and handle via signal/wait.
            if is_dynamic_tool {
                // Parse arguments as JSON value.
                let args_value: serde_json::Value =
                    serde_json::from_str(&arguments).unwrap_or(serde_json::Value::Null);

                // Set pending state.
                ctx.state_mut(|s| {
                    s.pending_dynamic_tool = Some(PendingDynamicTool {
                        call_id: call_id.clone(),
                        response: None,
                    });
                });

                // Emit DynamicToolCallRequest event.
                events.emit_event_sync(Event {
                    id: turn_id.clone(),
                    msg: EventMsg::DynamicToolCallRequest(DynamicToolCallRequest {
                        call_id: call_id.clone(),
                        turn_id: turn_id.clone(),
                        tool: tool_name,
                        arguments: args_value,
                    }),
                });
                ctx.state_mut(|s| s.bump_version());

                // Wait for response or interrupt.
                ctx.wait_condition(|s| {
                    s.pending_dynamic_tool
                        .as_ref()
                        .is_none_or(|p| p.response.is_some())
                        || s.interrupt_requested
                })
                .await;

                // Handle interrupt.
                let interrupted = ctx.state(|s| s.interrupt_requested);
                if interrupted {
                    ctx.state_mut(|s| s.pending_dynamic_tool = None);
                    return Ok(denied_response(call_id));
                }

                // Extract response.
                let response = ctx.state_mut(|s| {
                    let resp = s
                        .pending_dynamic_tool
                        .as_ref()
                        .and_then(|p| p.response.clone());
                    s.pending_dynamic_tool = None;
                    resp
                });

                // Convert DynamicToolResponse to FunctionCallOutput.
                if let Some(resp) = response {
                    let text = resp
                        .content_items
                        .iter()
                        .filter_map(|item| {
                            use codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem;
                            match item {
                                DynamicToolCallOutputContentItem::InputText { text } => {
                                    Some(text.as_str())
                                }
                                _ => None,
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    return Ok(ResponseInputItem::FunctionCallOutput {
                        call_id,
                        output: FunctionCallOutputPayload {
                            body: FunctionCallOutputBody::Text(text),
                            success: Some(resp.success),
                        },
                    });
                }

                return Ok(denied_response(call_id));
            }

            // apply_patch — policy-based pre-check in the workflow, with
            // full safety analysis deferred to the activity.
            if tool_name == "apply_patch" {
                // Extract the patch text from the arguments JSON.
                let patch_text: String = serde_json::from_str::<serde_json::Value>(&arguments)
                    .ok()
                    .and_then(|v| v.get("patch")?.as_str().map(String::from))
                    .unwrap_or_else(|| arguments.clone());

                // Workflow-side policy pre-check (pure, no I/O).
                // Full path-constraint checking is deferred to the activity.
                let patch_needs_approval = workflow_assess_patch_needs_approval(
                    patch_text.is_empty(),
                    approval_policy,
                    &sandbox_policy,
                );

                match patch_needs_approval {
                    PatchDecision::Reject(reason) => {
                        let text = serde_json::json!({
                            "output": reason,
                            "metadata": { "exit_code": 1, "duration_seconds": 0.0 }
                        })
                        .to_string();
                        return Ok(ResponseInputItem::FunctionCallOutput {
                            call_id,
                            output: FunctionCallOutputPayload {
                                body: FunctionCallOutputBody::Text(text),
                                success: Some(false),
                            },
                        });
                    }
                    PatchDecision::AskUser => {
                        // Set pending state.
                        ctx.state_mut(|s| {
                            s.pending_patch_approval = Some(PendingPatchApproval {
                                call_id: call_id.clone(),
                                decision: None,
                            });
                        });

                        // Emit ApplyPatchApprovalRequest event.
                        events.emit_event_sync(Event {
                            id: turn_id.clone(),
                            msg: EventMsg::ApplyPatchApprovalRequest(ApplyPatchApprovalRequestEvent {
                                call_id: call_id.clone(),
                                turn_id: turn_id.clone(),
                                changes: std::collections::HashMap::new(),
                                reason: Some(patch_text),
                                grant_root: None,
                            }),
                        });
                        ctx.state_mut(|s| s.bump_version());

                        // Wait for decision or interrupt.
                        ctx.wait_condition(|s| {
                            s.pending_patch_approval
                                .as_ref()
                                .is_none_or(|p| p.decision.is_some())
                                || s.interrupt_requested
                        })
                        .await;

                        // Handle interrupt.
                        let interrupted = ctx.state(|s| s.interrupt_requested);
                        if interrupted {
                            ctx.state_mut(|s| s.pending_patch_approval = None);
                            return Ok(denied_response(call_id));
                        }

                        // Check decision.
                        let approved = ctx.state_mut(|s| {
                            let decision = s
                                .pending_patch_approval
                                .as_ref()
                                .and_then(|p| p.decision)
                                .unwrap_or(false);
                            s.pending_patch_approval = None;
                            decision
                        });

                        if !approved {
                            return Ok(denied_response(call_id));
                        }
                    }
                    PatchDecision::AutoApprove => {
                        // No approval needed — fall through to execute.
                    }
                }

                // Execute via activity (with already_approved flag).
                let already_approved = matches!(patch_needs_approval, PatchDecision::AskUser | PatchDecision::AutoApprove);
                let input = ToolExecInput {
                    tool_name,
                    call_id: call_id.clone(),
                    arguments,
                    model,
                    cwd,
                    config_toml,
                    already_approved,
                    payload_kind: payload_kind.clone(),
                };

                let opts = ActivityOptions {
                    start_to_close_timeout: Some(Duration::from_secs(600)),
                    heartbeat_timeout: Some(Duration::from_secs(30)),
                    cancellation_type: ActivityCancellationType::TryCancel,
                    ..Default::default()
                };

                let activity =
                    ctx.start_activity(CodexActivities::tool_exec, input, opts);
                tokio::pin!(activity);
                let output = tokio::select! {
                    biased;
                    _ = cancellation_token.cancelled() => {
                        activity.cancel();
                        return Ok(denied_response(call_id));
                    }
                    result = &mut activity => result.map_err(|e| {
                        CodexErr::Fatal(format!("tool_exec activity failed: {e}"))
                    })?,
                };

                return Ok(output.into_response_input_item());
            }

            // Determine whether this call needs user approval.
            //
            // In codex-core, only shell commands and apply_patch go through
            // the approval orchestrator.  All other tools (read_file,
            // list_dir, grep_files, etc.) execute directly with no approval.
            // apply_patch is already handled above, so here we only apply the
            // three-tier shell classification to shell-type tools.
            let is_shell_tool = matches!(
                tool_name.as_str(),
                "shell"
                    | "container.exec"
                    | "local_shell"
                    | "shell_command"
                    | "unified_exec"
                    | "exec_command"
            );

            // Determine approval requirement using codex-core's
            // `render_decision_for_unmatched_command`, which accounts for
            // sandbox type, file-system sandbox policy, and granular config.
            let shell_decision = if !is_shell_tool {
                // Non-shell tools (file tools, etc.) never need approval —
                // matches codex-core where handlers dispatch directly.
                Decision::Allow
            } else {
                render_decision_for_unmatched_command(
                    approval_policy,
                    &sandbox_policy,
                    &file_system_sandbox_policy,
                    &command,
                    SandboxPermissions::default(),
                    false, // used_complex_parsing
                )
            };

            // Handle Forbidden immediately — return an error response to the
            // model without prompting or executing.
            if shell_decision == Decision::Forbidden {
                let reason = format!(
                    "Command rejected by policy: {}",
                    command.join(" ")
                );
                let text = serde_json::json!({
                    "output": reason,
                    "metadata": { "exit_code": 1, "duration_seconds": 0.0 }
                })
                .to_string();
                return Ok(ResponseInputItem::FunctionCallOutput {
                    call_id,
                    output: FunctionCallOutputPayload {
                        body: FunctionCallOutputBody::Text(text),
                        success: Some(false),
                    },
                });
            }

            let needs_approval = shell_decision == Decision::Prompt;

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
                        proposed_network_policy_amendments: None,
                        additional_permissions: None,
                        skill_metadata: None,
                        available_decisions: None,
                        parsed_cmd: Vec::new(),
                    }),
                };
                events.emit_event_sync(approval_event);
                ctx.state_mut(|s| s.bump_version());

                // 3. Wait for approval decision or interrupt
                ctx.wait_condition(|s| {
                    s.pending_approval
                        .as_ref()
                        .is_none_or(|p| p.decision.is_some())
                        || s.interrupt_requested
                })
                .await;

                // 4. Check for interrupt first — return a denied-style
                // response rather than Err(TurnAborted) to avoid panicking
                // codex-core's in-flight tool future drain. The workflow
                // loop will catch `interrupt_requested` at the iteration
                // boundary and emit TurnAborted.
                let interrupted = ctx.state(|s| s.interrupt_requested);
                if interrupted {
                    ctx.state_mut(|s| s.pending_approval = None);
                    return Ok(denied_response(call_id));
                }

                // 5. Check decision
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
                model,
                cwd,
                config_toml,
                already_approved: needs_approval,
                payload_kind: payload_kind.clone(),
            };

            let opts = ActivityOptions {
                start_to_close_timeout: Some(Duration::from_secs(600)),
                heartbeat_timeout: Some(Duration::from_secs(30)),
                cancellation_type: ActivityCancellationType::TryCancel,
                ..Default::default()
            };

            let activity = ctx.start_activity(CodexActivities::tool_exec, input, opts);
            tokio::pin!(activity);
            let output = tokio::select! {
                biased;
                _ = cancellation_token.cancelled() => {
                    activity.cancel();
                    return Ok(denied_response(call_id));
                }
                result = &mut activity => result.map_err(|e| {
                    CodexErr::Fatal(format!("tool_exec activity failed: {e}"))
                })?,
            };

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

// -----------------------------------------------------------------------
// Workflow-side patch safety pre-check
// -----------------------------------------------------------------------

/// Result of the workflow-side patch safety pre-check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PatchDecision {
    /// Auto-approve — sandbox or policy permits without user interaction.
    AutoApprove,
    /// Prompt the user for approval.
    AskUser,
    /// Reject the patch outright.
    Reject(&'static str),
}

/// Pure (no I/O) policy pre-check for `apply_patch` in the workflow.
///
/// Mirrors the logic in `codex_core::safety::assess_patch_safety` but
/// without requiring an `ApplyPatchAction` (which needs file I/O to
/// construct). The full path-constraint verification is deferred to the
/// activity via `assess_patch_safety` with the real `ApplyPatchAction`.
pub(crate) fn workflow_assess_patch_needs_approval(
    patch_is_empty: bool,
    policy: AskForApproval,
    sandbox_policy: &SandboxPolicy,
) -> PatchDecision {
    if patch_is_empty {
        return PatchDecision::Reject("empty patch");
    }

    // UnlessTrusted always asks the user (matches codex-core).
    if matches!(policy, AskForApproval::UnlessTrusted) {
        return PatchDecision::AskUser;
    }

    // DangerFullAccess / ExternalSandbox bypass sandboxing entirely.
    if matches!(
        sandbox_policy,
        SandboxPolicy::DangerFullAccess | SandboxPolicy::ExternalSandbox { .. }
    ) {
        return PatchDecision::AutoApprove;
    }

    // On Linux the platform sandbox (seccomp) is always available, so
    // auto-approve for OnFailure / Never / OnRequest / Granular policies.
    // The activity will do full `assess_patch_safety` with real file I/O
    // as a second gate.
    if cfg!(target_os = "linux") || cfg!(target_os = "macos") {
        return PatchDecision::AutoApprove;
    }

    // On other platforms without a guaranteed sandbox, ask.
    PatchDecision::AskUser
}
