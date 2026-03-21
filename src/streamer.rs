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
            heartbeat_timeout: Some(Duration::from_secs(20)),
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
                // Extract the full error message chain from the Temporal
                // Failure.  ActivityExecutionError::Failed wraps a Failure
                // proto whose Display only shows the top-level `message`
                // field (often just "Activity task failed").  The actual
                // error from the activity (e.g. "Quota exceeded") is in
                // the `cause` chain.  Walk it to build a complete message.
                let msg = unwrap_activity_error_message(&e);
                // Preserve non-retryable semantics: if the activity error
                // message matches a known fatal condition, map to the
                // corresponding non-retryable CodexErr so the workflow
                // does not attempt its own retry loop.
                if msg.contains("Quota exceeded") {
                    codex_core::error::CodexErr::QuotaExceeded
                } else if msg.contains("upgrade to Plus") || msg.contains("UsageNotIncluded") {
                    codex_core::error::CodexErr::UsageNotIncluded
                } else {
                    codex_core::error::CodexErr::Stream(
                        format!("model_call activity failed: {msg}"),
                        None,
                    )
                }
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

/// Extract the full error message chain from an [`ActivityExecutionError`].
///
/// `ActivityExecutionError::Failed` wraps a Temporal `Failure` proto whose
/// `Display` only prints the top-level `message` (often a generic
/// "Activity task failed").  The original error (e.g. "Quota exceeded.")
/// is in the `cause` chain.  This function walks the chain and joins all
/// non-empty messages with `: `, mirroring how `anyhow`/`std::error::Error`
/// chains are typically rendered.  If any `Failure` in the chain has a
/// non-empty `stack_trace` (common with non-Rust activities/child
/// workflows), it is appended.
fn unwrap_activity_error_message(
    e: &temporalio_sdk::ActivityExecutionError,
) -> String {
    use temporalio_sdk::ActivityExecutionError;
    let failure = match e {
        ActivityExecutionError::Failed(f) | ActivityExecutionError::Cancelled(f) => f,
        other => return format!("{other}"),
    };

    let mut parts = Vec::new();
    let mut stack_trace: Option<&str> = None;

    if !failure.message.is_empty() {
        parts.push(failure.message.as_str());
    }
    if !failure.stack_trace.is_empty() {
        stack_trace = Some(&failure.stack_trace);
    }

    let mut current = failure.cause.as_deref();
    while let Some(cause) = current {
        if !cause.message.is_empty() {
            parts.push(&cause.message);
        }
        if stack_trace.is_none() && !cause.stack_trace.is_empty() {
            stack_trace = Some(&cause.stack_trace);
        }
        current = cause.cause.as_deref();
    }

    if parts.is_empty() {
        return format!("{e}");
    }

    let mut msg = parts.join(": ");
    if let Some(trace) = stack_trace {
        msg.push_str("\n");
        msg.push_str(trace);
    }
    msg
}
