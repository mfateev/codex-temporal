//! Temporal workflow definition for Codex.

use serde::{Deserialize, Serialize};
use temporalio_sdk::{WfContext, WfExitValue, ActivityOptions};
use temporalio_common::protos::coresdk::{AsJsonPayloadExt, FromJsonPayloadExt};
use std::time::Duration;

use crate::activities::{ModelActivityInput, ModelActivityOutput};

/// Input for the Codex workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexWorkflowInput {
    /// The user's prompt.
    pub prompt: String,
    /// Model to use (e.g., "gpt-4").
    pub model: String,
    /// Optional system instructions.
    pub system_instructions: Option<String>,
    /// Timeout for auto-approval (in seconds). Default: 5
    pub approval_timeout_secs: Option<u64>,
}

/// Output from the Codex workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexWorkflowOutput {
    /// The model's final response.
    pub response: String,
    /// Number of model calls made.
    pub model_calls: u32,
    /// Whether any tool calls were made.
    pub had_tool_calls: bool,
}

/// Main Codex workflow - runs the agentic loop inside Temporal.
///
/// This workflow:
/// 1. Receives a prompt as input
/// 2. Calls the model via an activity
/// 3. (Future) Processes tool calls via activities
/// 4. Returns the final response
///
/// All non-deterministic operations (model calls, tool execution) are
/// performed in activities, ensuring workflow replay correctness.
pub async fn codex_workflow(
    ctx: WfContext,
) -> Result<WfExitValue<CodexWorkflowOutput>, anyhow::Error> {
    // Get workflow input
    let args = ctx.get_args();
    let input: CodexWorkflowInput = if args.is_empty() {
        return Err(anyhow::anyhow!("Workflow input is required"));
    } else {
        CodexWorkflowInput::from_json_payload(&args[0])?
    };

    tracing::info!(
        prompt = %input.prompt,
        model = %input.model,
        "Starting Codex workflow"
    );

    // Create model activity input
    let model_input = ModelActivityInput {
        prompt: input.prompt.clone(),
        model: input.model.clone(),
        system_instructions: input.system_instructions.clone(),
    };

    // Serialize input to payload
    let input_payload = model_input.as_json_payload()?;

    // Call model activity
    let activity_options = ActivityOptions {
        activity_type: "model_stream".to_string(),
        input: input_payload,
        start_to_close_timeout: Some(Duration::from_secs(120)),
        heartbeat_timeout: Some(Duration::from_secs(30)),
        ..Default::default()
    };

    let resolution = ctx.activity(activity_options).await;

    // Extract result from resolution
    let result_payload = resolution.success_payload_or_error()?
        .ok_or_else(|| anyhow::anyhow!("Activity returned no payload"))?;

    let model_result: ModelActivityOutput = ModelActivityOutput::from_json_payload(&result_payload)?;

    tracing::info!(
        response_len = model_result.response.len(),
        has_tool_calls = model_result.has_tool_calls,
        "Model activity completed"
    );

    // For POC: Simple single-turn response
    // Future: Loop for tool calls, approval timers, etc.

    Ok(WfExitValue::Normal(CodexWorkflowOutput {
        response: model_result.response,
        model_calls: 1,
        had_tool_calls: model_result.has_tool_calls,
    }))
}
