//! [`ModelStreamer`] implementation that dispatches model calls as Temporal
//! activities.

use std::time::Duration;

use codex_core::{ModelProviderInfo, ModelStreamer, Prompt, ResponseEvent, ResponseStream};
use codex_otel::SessionTelemetry;
use codex_protocol::config_types::{ReasoningSummary, ServiceTier};
use codex_protocol::openai_models::{ModelInfo, ReasoningEffort};
use temporalio_common::protos::coresdk::workflow_commands::ActivityCancellationType;
use temporalio_sdk::{ActivityOptions, CancellableFuture, WorkflowContext};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::activities::CodexActivities;
use crate::types::ModelCallInput;
use crate::workflow::AgentWorkflow;

/// A [`ModelStreamer`] that dispatches model calls as Temporal activities.
pub struct TemporalModelStreamer {
    ctx: WorkflowContext<AgentWorkflow>,
    conversation_id: String,
    provider: Option<ModelProviderInfo>,
    cancellation_token: CancellationToken,
}

impl TemporalModelStreamer {
    pub fn new(
        ctx: WorkflowContext<AgentWorkflow>,
        conversation_id: String,
        provider: Option<ModelProviderInfo>,
        cancellation_token: CancellationToken,
    ) -> Self {
        Self { ctx, conversation_id, provider, cancellation_token }
    }
}

impl ModelStreamer for TemporalModelStreamer {
    async fn stream(
        &mut self,
        prompt: &Prompt,
        model_info: &ModelInfo,
        _session_telemetry: &SessionTelemetry,
        effort: Option<ReasoningEffort>,
        summary: ReasoningSummary,
        _service_tier: Option<ServiceTier>,
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
            provider: self.provider.clone(),
        };

        let opts = ActivityOptions {
            start_to_close_timeout: Some(Duration::from_secs(300)),
            cancellation_type: ActivityCancellationType::TryCancel,
            ..Default::default()
        };

        let activity = self
            .ctx
            .start_activity(CodexActivities::model_call, input, opts);
        tokio::pin!(activity);
        let output = tokio::select! {
            biased;
            _ = self.cancellation_token.cancelled() => {
                activity.cancel();
                return Err(codex_core::error::CodexErr::TurnAborted);
            }
            result = &mut activity => result.map_err(|e| {
                codex_core::error::CodexErr::Stream(
                    format!("model_call activity failed: {e}"),
                    None,
                )
            })?,
        };

        // Synthesize the event sequence: Created → OutputItemDone* → Completed
        let (tx, rx) =
            mpsc::channel::<codex_core::error::Result<ResponseEvent>>(output.items.len() + 2);

        tx.send(Ok(ResponseEvent::Created)).await.ok();
        for item in output.items {
            tx.send(Ok(ResponseEvent::OutputItemDone(item))).await.ok();
        }
        tx.send(Ok(ResponseEvent::Completed {
            response_id: String::new(),
            token_usage: output.token_usage,
        }))
        .await
        .ok();

        Ok(ResponseStream::from_receiver(rx))
    }
}
