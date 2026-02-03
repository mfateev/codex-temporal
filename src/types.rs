//! Common types for agent workflow I/O.

use serde::{Deserialize, Serialize};

/// Workflow input for the agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInput {
    /// The user's prompt.
    pub prompt: String,
    /// Model to use (e.g., "gpt-4o").
    pub model: String,
    /// Optional system instructions.
    pub instructions: Option<String>,
    /// Maximum number of agent loop turns (default: 10).
    pub max_turns: Option<u32>,
}

/// Workflow output from the agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentOutput {
    /// The final response from the agent.
    pub response: String,
    /// Number of model API calls made.
    pub model_calls: u32,
    /// Number of tool calls executed.
    pub tool_calls: u32,
}

/// Input item for the Responses API.
///
/// The Responses API accepts an array of typed items as input, not the
/// role-tagged messages used by Chat Completions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum InputItem {
    /// A message from the user or assistant.
    #[serde(rename = "message")]
    Message {
        role: String,
        content: String,
    },

    /// A function call from the model (echoed back in history).
    #[serde(rename = "function_call")]
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },

    /// Output from a function call (tool result).
    #[serde(rename = "function_call_output")]
    FunctionCallOutput {
        call_id: String,
        output: String,
    },
}

/// A tool call from the model (used in ModelOutput).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallMessage {
    /// Unique ID for this tool call.
    pub id: String,
    /// Name of the function.
    pub name: String,
    /// JSON-encoded arguments.
    pub arguments: String,
}

/// Function call details (legacy, kept for compatibility).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    /// Name of the function to call.
    pub name: String,
    /// JSON-encoded arguments.
    pub arguments: String,
}

/// Tool definition (OpenAI Responses API format).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    /// Type of tool (always "function" for now).
    #[serde(rename = "type")]
    pub tool_type: String,
    /// Name of the function.
    pub name: String,
    /// Description for the model.
    pub description: String,
    /// JSON Schema for parameters.
    pub parameters: serde_json::Value,
}

/// Function definition (used for documentation, not serialization).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionDef {
    /// Name of the function.
    pub name: String,
    /// Description for the model.
    pub description: String,
    /// JSON Schema for parameters.
    pub parameters: serde_json::Value,
}
