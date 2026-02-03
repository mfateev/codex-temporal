//! Temporal workflow definition for the Codex agent.

use std::time::Duration;

use temporalio_common::protos::coresdk::{AsJsonPayloadExt, FromJsonPayloadExt};
use temporalio_sdk::{ActivityOptions, WfContext, WfExitValue};

use crate::activities::{http_fetch_tool_def, HttpFetchInput, HttpFetchOutput, ModelInput, ModelOutput};
use crate::types::{AgentInput, AgentOutput, InputItem, ToolCallMessage};

/// Main agent workflow - runs the agentic loop inside Temporal.
///
/// This workflow:
/// 1. Receives a prompt and configuration as input
/// 2. Calls the model via an activity
/// 3. Processes tool calls via activities
/// 4. Continues until the model returns a final response or max turns is reached
///
/// All non-deterministic operations (model calls, tool execution) are
/// performed in activities, ensuring workflow replay correctness.
pub async fn agent_workflow(ctx: WfContext) -> Result<WfExitValue<AgentOutput>, anyhow::Error> {
    // Parse input
    let args = ctx.get_args();
    let input: AgentInput = if args.is_empty() {
        return Err(anyhow::anyhow!("Workflow input is required"));
    } else {
        AgentInput::from_json_payload(&args[0])?
    };

    let max_turns = input.max_turns.unwrap_or(10);
    let tools = vec![http_fetch_tool_def()];

    tracing::info!(
        prompt = %input.prompt,
        model = %input.model,
        max_turns,
        "Starting agent workflow"
    );

    // Initialize history with user message (Responses API format)
    let mut history: Vec<InputItem> = vec![InputItem::Message {
        role: "user".to_string(),
        content: input.prompt.clone(),
    }];

    let mut model_calls = 0u32;
    let mut tool_calls_count = 0u32;

    // Agent loop
    for turn in 0..max_turns {
        tracing::info!(turn, "Starting agent turn");

        // 1. Call model activity
        let model_input = ModelInput {
            model: input.model.clone(),
            instructions: input.instructions.clone(),
            input: history.clone(),
            tools: tools.clone(),
        };

        let model_output: ModelOutput =
            call_activity(&ctx, "invoke_model", model_input, Duration::from_secs(120)).await?;

        model_calls += 1;

        // 2. If we got content, add assistant message to history
        if let Some(ref content) = model_output.content {
            history.push(InputItem::Message {
                role: "assistant".to_string(),
                content: content.clone(),
            });
        }

        // 3. If no tool calls, return final response
        if model_output.tool_calls.is_empty() {
            tracing::info!(
                model_calls,
                tool_calls = tool_calls_count,
                "Agent completed - no more tool calls"
            );

            return Ok(WfExitValue::Normal(AgentOutput {
                response: model_output.content.unwrap_or_default(),
                model_calls,
                tool_calls: tool_calls_count,
            }));
        }

        // 4. Execute tool calls and add results to history
        for tc in &model_output.tool_calls {
            // First, echo the function call back to the API
            history.push(InputItem::FunctionCall {
                call_id: tc.id.clone(),
                name: tc.name.clone(),
                arguments: tc.arguments.clone(),
            });

            // Execute the tool
            let result = execute_tool(&ctx, tc).await?;
            tool_calls_count += 1;

            // Add function call output (Responses API format)
            history.push(InputItem::FunctionCallOutput {
                call_id: tc.id.clone(),
                output: result,
            });
        }

        // Continue loop
    }

    Err(anyhow::anyhow!(
        "Max turns ({max_turns}) exceeded without final response"
    ))
}

/// Execute a single tool call and return the result as a string.
async fn execute_tool(ctx: &WfContext, tool_call: &ToolCallMessage) -> Result<String, anyhow::Error> {
    tracing::info!(
        tool = %tool_call.name,
        call_id = %tool_call.id,
        "Executing tool call"
    );

    match tool_call.name.as_str() {
        "http_fetch" => {
            let args: HttpFetchInput = serde_json::from_str(&tool_call.arguments)?;
            let output: HttpFetchOutput =
                call_activity(ctx, "http_fetch", args, Duration::from_secs(30)).await?;
            Ok(format!("Status: {}\n\n{}", output.status, output.body))
        }
        other => Ok(format!("Unknown tool: {other}")),
    }
}

/// Helper to call an activity with JSON serialization.
async fn call_activity<I, O>(
    ctx: &WfContext,
    activity_type: &str,
    input: I,
    timeout: Duration,
) -> Result<O, anyhow::Error>
where
    I: serde::Serialize + AsJsonPayloadExt,
    O: serde::de::DeserializeOwned + FromJsonPayloadExt,
{
    let input_payload = input.as_json_payload()?;

    let resolution = ctx
        .activity(ActivityOptions {
            activity_type: activity_type.to_string(),
            input: input_payload,
            start_to_close_timeout: Some(timeout),
            heartbeat_timeout: Some(Duration::from_secs(30)),
            ..Default::default()
        })
        .await;

    let payload = resolution
        .success_payload_or_error()?
        .ok_or_else(|| anyhow::anyhow!("Activity returned no payload"))?;

    Ok(O::from_json_payload(&payload)?)
}

// Legacy types and workflow for backward compatibility

use serde::{Deserialize, Serialize};

use crate::activities::{ModelActivityInput, ModelActivityOutput};

/// Input for the Codex workflow (legacy).
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

/// Output from the Codex workflow (legacy).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexWorkflowOutput {
    /// The model's final response.
    pub response: String,
    /// Number of model calls made.
    pub model_calls: u32,
    /// Whether any tool calls were made.
    pub had_tool_calls: bool,
}

/// Legacy Codex workflow - simple single-turn prompt.
pub async fn codex_workflow(ctx: WfContext) -> Result<WfExitValue<CodexWorkflowOutput>, anyhow::Error> {
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
        "Starting Codex workflow (legacy)"
    );

    // Create model activity input
    let model_input = ModelActivityInput {
        prompt: input.prompt.clone(),
        model: input.model.clone(),
        system_instructions: input.system_instructions.clone(),
    };

    let model_result: ModelActivityOutput =
        call_activity(&ctx, "model_stream", model_input, Duration::from_secs(120)).await?;

    tracing::info!(
        response_len = model_result.response.len(),
        has_tool_calls = model_result.has_tool_calls,
        "Model activity completed"
    );

    Ok(WfExitValue::Normal(CodexWorkflowOutput {
        response: model_result.response,
        model_calls: 1,
        had_tool_calls: model_result.has_tool_calls,
    }))
}
