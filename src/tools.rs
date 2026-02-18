//! [`ToolCallHandler`] implementation that dispatches tool executions as
//! Temporal activities.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use codex_core::error::CodexErr;
use codex_core::ToolCall;
use codex_core::ToolCallHandler;
use codex_protocol::models::ResponseInputItem;
use temporalio_sdk::{ActivityOptions, WorkflowContext};
use tokio_util::sync::CancellationToken;

use crate::activities::CodexActivities;
use crate::types::ToolExecInput;

/// A [`ToolCallHandler`] that dispatches tool calls as Temporal activities.
pub struct TemporalToolHandler<W: 'static> {
    ctx: WorkflowContext<W>,
}

impl<W: 'static> TemporalToolHandler<W> {
    pub fn new(ctx: WorkflowContext<W>) -> Self {
        Self { ctx }
    }
}

impl<W: 'static> ToolCallHandler for TemporalToolHandler<W> {
    type Future = Pin<Box<dyn Future<Output = Result<ResponseInputItem, CodexErr>> + 'static>>;

    fn handle_tool_call(&self, call: ToolCall, _cancellation_token: CancellationToken) -> Self::Future {
        let ctx = self.ctx.clone();

        let input = ToolExecInput {
            tool_name: call.tool_name.clone(),
            call_id: call.call_id.clone(),
            arguments: serde_json::to_string(&serde_json::json!({
                "command": [format!("{:?}", call.payload)]
            }))
            .unwrap_or_default(),
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
                .map_err(|e| CodexErr::Fatal(format!("tool_exec activity failed: {e}")))?;

            Ok(output.into_response_input_item())
        })
    }
}
