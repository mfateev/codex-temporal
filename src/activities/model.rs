//! Model activity - calls OpenAI Responses API.

use serde::{Deserialize, Serialize};
use temporalio_sdk::{ActContext, ActivityError};

use crate::types::{InputItem, ToolCallMessage, ToolDef};

/// Input for the invoke_model activity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInput {
    /// Model to use (e.g., "gpt-4o").
    pub model: String,
    /// Optional system instructions.
    pub instructions: Option<String>,
    /// Conversation history (Responses API format).
    pub input: Vec<InputItem>,
    /// Available tools.
    pub tools: Vec<ToolDef>,
}

/// Output from the invoke_model activity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelOutput {
    /// Text content from the model (if any).
    pub content: Option<String>,
    /// Tool calls requested by the model.
    pub tool_calls: Vec<ToolCallMessage>,
}

/// Activity that calls the OpenAI Responses API.
///
/// This activity:
/// 1. Builds the request for the Responses API
/// 2. Sends the request via HTTP
/// 3. Parses the response to extract content and tool calls
/// 4. Returns the structured output
pub async fn invoke_model_activity(
    ctx: ActContext,
    input: ModelInput,
) -> Result<ModelOutput, ActivityError> {
    let api_key = std::env::var("OPENAI_API_KEY").map_err(|_| {
        ActivityError::NonRetryable(anyhow::anyhow!(
            "OPENAI_API_KEY environment variable not set"
        ))
    })?;

    let client = reqwest::Client::new();

    // Build request body for Responses API
    let mut body = serde_json::json!({
        "model": input.model,
        "input": input.input,
    });

    if let Some(instructions) = &input.instructions {
        body["instructions"] = serde_json::json!(instructions);
    }

    if !input.tools.is_empty() {
        body["tools"] = serde_json::to_value(&input.tools).map_err(|e| {
            ActivityError::NonRetryable(anyhow::anyhow!("Failed to serialize tools: {e}"))
        })?;
    }

    tracing::info!(
        model = %input.model,
        message_count = input.input.len(),
        tool_count = input.tools.len(),
        "Calling OpenAI Responses API"
    );

    // Heartbeat before the potentially long API call
    ctx.record_heartbeat(vec![]);

    let response = client
        .post("https://api.openai.com/v1/responses")
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            // Network errors are retryable
            ActivityError::Retryable {
                source: anyhow::anyhow!("HTTP request failed: {e}"),
                explicit_delay: None,
            }
        })?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        let err = anyhow::anyhow!("OpenAI API error {status}: {text}");

        // Retry on 5xx or 429 (rate limit)
        if status.as_u16() >= 500 || status.as_u16() == 429 {
            return Err(ActivityError::Retryable {
                source: err,
                explicit_delay: None,
            });
        } else {
            return Err(ActivityError::NonRetryable(err));
        }
    }

    // Parse response
    let json: serde_json::Value = response.json().await.map_err(|e| {
        ActivityError::NonRetryable(anyhow::anyhow!("Failed to parse JSON response: {e}"))
    })?;

    tracing::debug!(response = ?json, "Received API response");

    // Extract output from Responses API format
    let output = json.get("output").and_then(|v| v.as_array());

    let mut content = None;
    let mut tool_calls = Vec::new();

    if let Some(items) = output {
        for item in items {
            let item_type = item.get("type").and_then(|v| v.as_str());
            match item_type {
                Some("message") => {
                    // Extract text content from message
                    if let Some(c) = item.get("content").and_then(|v| v.as_array()) {
                        for part in c {
                            if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                                content = Some(text.to_string());
                            }
                        }
                    }
                }
                Some("function_call") => {
                    // Extract tool call from Responses API format
                    if let (Some(id), Some(name), Some(args)) = (
                        item.get("call_id").and_then(|v| v.as_str()),
                        item.get("name").and_then(|v| v.as_str()),
                        item.get("arguments").and_then(|v| v.as_str()),
                    ) {
                        tool_calls.push(ToolCallMessage {
                            id: id.to_string(),
                            name: name.to_string(),
                            arguments: args.to_string(),
                        });
                    }
                }
                _ => {}
            }
        }
    }

    tracing::info!(
        has_content = content.is_some(),
        tool_call_count = tool_calls.len(),
        "Model response parsed"
    );

    Ok(ModelOutput { content, tool_calls })
}

// Legacy types for backward compatibility with existing workflow
// TODO: Remove once workflow is updated

/// Input for the model_stream activity (legacy).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelActivityInput {
    /// The prompt to send to the model.
    pub prompt: String,
    /// Model configuration (e.g., model name, temperature).
    pub model: String,
    /// Optional system instructions.
    pub system_instructions: Option<String>,
}

/// Output from the model_stream activity (legacy).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelActivityOutput {
    /// The model's response text.
    pub response: String,
    /// Whether the response includes tool calls (for future use).
    pub has_tool_calls: bool,
    /// Usage statistics.
    pub usage: Option<UsageStats>,
}

/// Token usage statistics (legacy).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageStats {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

/// Legacy activity that calls the model API (stub for backward compatibility).
pub async fn model_stream_activity(
    ctx: ActContext,
    input: ModelActivityInput,
) -> Result<ModelActivityOutput, ActivityError> {
    tracing::info!(
        prompt = %input.prompt,
        model = %input.model,
        "Executing model_stream activity (legacy)"
    );

    // Heartbeat
    ctx.record_heartbeat(vec![]);

    // Stub response for backward compatibility
    let response = format!(
        "This is a stub response. Your prompt was: '{}'. \
         Use the new agent workflow for real API calls.",
        input.prompt
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
