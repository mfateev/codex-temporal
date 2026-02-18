//! [`ToolCallHandler`] implementation that dispatches tool executions as
//! Temporal activities.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use codex_core::error::CodexErr;
use codex_core::ToolCallHandler;
use codex_core::ToolCall;
use codex_protocol::models::ResponseInputItem;
use temporalio_sdk::{ActivityOptions, BaseWorkflowContext};
use tokio_util::sync::CancellationToken;

use crate::activities::CodexActivities;
use crate::types::ToolExecInput;

/// A [`ToolCallHandler`] that dispatches tool calls as Temporal activities.
pub struct TemporalToolHandler {
    ctx: BaseWorkflowContext,
}

impl TemporalToolHandler {
    pub fn new(ctx: BaseWorkflowContext) -> Self {
        Self { ctx }
    }
}

impl ToolCallHandler for TemporalToolHandler {
    // LocalBoxFuture — !Send, which is fine in the single-threaded workflow executor.
    type Future = Pin<Box<dyn Future<Output = Result<ResponseInputItem, CodexErr>> + 'static>>;

    fn handle_tool_call(
        &self,
        call: ToolCall,
        _cancellation_token: CancellationToken,
    ) -> Self::Future {
        let ctx = self.ctx.clone();

        let input = ToolExecInput {
            tool_name: call.tool_name.clone(),
            call_id: call.call_id.clone(),
            // Serialize the tool call arguments for the activity.
            arguments: format!("{:?}", call.payload),
        };

        Box::pin(async move {
            let opts = ActivityOptions {
                start_to_close_timeout: Some(Duration::from_secs(600)),
                heartbeat_timeout: Some(Duration::from_secs(30)),
                ..Default::default()
            };

            let output = ctx
                .start_activity(CodexActivities::tool_exec, input, opts)
                .await
                .map_err(|e| {
                    CodexErr::Fatal(format!("tool_exec activity failed: {e}"))
                })?;

            Ok(output.into_response_input_item())
        })
    }
}
