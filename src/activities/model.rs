//! Model activity - makes HTTP calls to the model API.

use serde::{Deserialize, Serialize};
use temporalio_sdk::{ActContext, ActivityError};

/// Input for the model_stream activity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelActivityInput {
    /// The prompt to send to the model.
    pub prompt: String,
    /// Model configuration (e.g., model name, temperature).
    pub model: String,
    /// Optional system instructions.
    pub system_instructions: Option<String>,
}

/// Output from the model_stream activity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelActivityOutput {
    /// The model's response text.
    pub response: String,
    /// Whether the response includes tool calls (for future use).
    pub has_tool_calls: bool,
    /// Usage statistics.
    pub usage: Option<UsageStats>,
}

/// Token usage statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageStats {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

/// Activity that calls the model API and returns the buffered response.
///
/// This activity:
/// 1. Creates an HTTP client
/// 2. Sends the prompt to the model API
/// 3. Buffers the streaming response
/// 4. Returns the complete response
///
/// The activity uses heartbeats to stay alive during long-running requests.
pub async fn model_stream_activity(
    ctx: ActContext,
    input: ModelActivityInput,
) -> Result<ModelActivityOutput, ActivityError> {
    // For POC: Return a stub response
    // TODO: Implement actual OpenAI API call

    tracing::info!(
        prompt = %input.prompt,
        model = %input.model,
        "Executing model_stream activity"
    );

    // Simulate some work and heartbeat (empty payload for progress)
    ctx.record_heartbeat(vec![]);

    // Stub response for POC
    let response = format!(
        "This is a stub response from the model activity. \
         Your prompt was: '{}'. \
         In a real implementation, this would call the {} API.",
        input.prompt, input.model
    );

    Ok(ModelActivityOutput {
        response,
        has_tool_calls: false,
        usage: Some(UsageStats {
            prompt_tokens: input.prompt.len() as u32,
            completion_tokens: 50,
            total_tokens: input.prompt.len() as u32 + 50,
        }),
    })
}
