//! [`ModelStreamer`] implementation that dispatches model calls as Temporal
//! activities.

use std::time::Duration;

use codex_core::{ModelStreamer, Prompt, ResponseStream};
use codex_core::ResponseEvent;
use codex_protocol::openai_models::ModelInfo;
use codex_otel::OtelManager;
use temporalio_sdk::{ActivityOptions, BaseWorkflowContext};
use tokio::sync::mpsc;

use crate::activities::CodexActivities;
use crate::types::ModelCallInput;

/// A [`ModelStreamer`] that dispatches model calls as Temporal activities.
pub struct TemporalModelStreamer {
    ctx: BaseWorkflowContext,
}

impl TemporalModelStreamer {
    pub fn new(ctx: BaseWorkflowContext) -> Self {
        Self { ctx }
    }
}

impl ModelStreamer for TemporalModelStreamer {
    async fn stream(
        &mut self,
        prompt: &Prompt,
        model_info: &ModelInfo,
        _otel_manager: &OtelManager,
        _effort: Option<codex_protocol::openai_models::ReasoningEffort>,
        _summary: codex_protocol::config_types::ReasoningSummary,
        _turn_metadata_header: Option<&str>,
    ) -> codex_core::error::Result<ResponseStream> {
        // Serialize the prompt into the activity input.
        let tools_json: Vec<serde_json::Value> = prompt
            .tools
            .iter()
            .filter_map(|t| serde_json::to_value(t).ok())
            .collect();

        let input = ModelCallInput {
            input: prompt.input.clone(),
            tools_json,
            parallel_tool_calls: prompt.parallel_tool_calls,
            instructions: prompt.base_instructions.text.clone(),
            model: model_info.slug.clone(),
        };

        // Dispatch as an activity with a generous timeout for model calls.
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

        // Convert collected items into a ResponseStream.
        // Synthesize: Created → OutputItemDone* → Completed
        let (tx, rx) = mpsc::channel::<codex_core::error::Result<ResponseEvent>>(
            output.items.len() + 2,
        );

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
