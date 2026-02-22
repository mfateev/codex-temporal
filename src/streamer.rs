//! [`ModelStreamer`] implementation that dispatches model calls as Temporal
//! activities.

use std::time::Duration;

use codex_core::{ModelStreamer, Prompt, ResponseEvent, ResponseStream};
use codex_otel::OtelManager;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::openai_models::{ModelInfo, ReasoningEffort};
use temporalio_sdk::{ActivityOptions, WorkflowContext};
use tokio::sync::mpsc;

use crate::activities::CodexActivities;
use crate::types::ModelCallInput;
use crate::workflow::CodexWorkflow;

/// A [`ModelStreamer`] that dispatches model calls as Temporal activities.
pub struct TemporalModelStreamer {
    ctx: WorkflowContext<CodexWorkflow>,
    conversation_id: String,
}

impl TemporalModelStreamer {
    pub fn new(ctx: WorkflowContext<CodexWorkflow>, conversation_id: String) -> Self {
        Self { ctx, conversation_id }
    }
}

impl ModelStreamer for TemporalModelStreamer {
    async fn stream(
        &mut self,
        prompt: &Prompt,
        model_info: &ModelInfo,
        _otel_manager: &OtelManager,
        effort: Option<ReasoningEffort>,
        summary: ReasoningSummary,
        _turn_metadata_header: Option<&str>,
    ) -> codex_core::error::Result<ResponseStream> {
        let input = ModelCallInput {
            conversation_id: self.conversation_id.clone(),
            input: prompt.input.clone(),
            tools: prompt.tools.clone(),
            parallel_tool_calls: prompt.parallel_tool_calls,
            instructions: prompt.base_instructions.text.clone(),
            model_info: model_info.clone(),
            effort,
            summary,
            personality: prompt.personality,
        };

        let opts = ActivityOptions {
            start_to_close_timeout: Some(Duration::from_secs(300)),
            ..Default::default()
        };

        let output = self
            .ctx
            .start_activity(CodexActivities::model_call, input, opts)
            .await
            .map_err(|e| {
                codex_core::error::CodexErr::Stream(
                    format!("model_call activity failed: {e}"),
                    None,
                )
            })?;

        // Synthesize the event sequence: Created → OutputItemDone* → Completed
        let (tx, rx) =
            mpsc::channel::<codex_core::error::Result<ResponseEvent>>(output.items.len() + 2);

        tx.send(Ok(ResponseEvent::Created)).await.ok();
        for item in output.items {
            tx.send(Ok(ResponseEvent::OutputItemDone(item))).await.ok();
        }
        tx.send(Ok(ResponseEvent::Completed {
            response_id: String::new(),
            token_usage: None,
            can_append: false,
        }))
        .await
        .ok();

        Ok(ResponseStream::from_receiver(rx))
    }
}
