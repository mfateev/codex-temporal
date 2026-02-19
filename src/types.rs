//! Serializable I/O types for Temporal activities.
//!
//! These types are sent across the Temporal activity boundary, so they must
//! implement `Serialize` + `Deserialize`.  They mirror the codex-core types
//! that are not themselves serializable (e.g. `Prompt`, `ResponseStream`).

use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Model call activity I/O
// ---------------------------------------------------------------------------

/// Input to the `model_call` activity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCallInput {
    /// Conversation context items sent to the model.
    pub input: Vec<ResponseItem>,
    /// JSON-serialized tool definitions.
    pub tools_json: Vec<serde_json::Value>,
    /// Whether parallel tool calls are permitted.
    pub parallel_tool_calls: bool,
    /// Base instructions for the model.
    pub instructions: String,
    /// Model slug (e.g. "gpt-4o").
    pub model: String,
}

/// Output from the `model_call` activity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCallOutput {
    /// The collected response events from the model, represented as
    /// response items (OutputItemDone payloads).
    pub items: Vec<ResponseItem>,
}

// ---------------------------------------------------------------------------
// Tool exec activity I/O
// ---------------------------------------------------------------------------

/// Input to the `tool_exec` activity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExecInput {
    /// The tool name (e.g. "shell", "container.exec").
    pub tool_name: String,
    /// The call ID from the model.
    pub call_id: String,
    /// The tool arguments as a JSON string.
    pub arguments: String,
}

/// Output from the `tool_exec` activity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExecOutput {
    /// The response input item to feed back to the model.
    pub call_id: String,
    /// stdout + stderr combined output.
    pub output: String,
    /// Process exit code (0 = success).
    pub exit_code: i32,
}

impl ToolExecOutput {
    /// Convert this output into a `ResponseInputItem` for the model.
    pub fn into_response_input_item(self) -> ResponseInputItem {
        use codex_protocol::models::{FunctionCallOutputBody, FunctionCallOutputPayload};

        let text = serde_json::json!({
            "output": self.output,
            "metadata": {
                "exit_code": self.exit_code,
                "duration_seconds": 0.0
            }
        })
        .to_string();

        ResponseInputItem::FunctionCallOutput {
            call_id: self.call_id,
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text(text),
                success: Some(self.exit_code == 0),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Signal payloads
// ---------------------------------------------------------------------------

/// Signal payload for submitting a new user turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserTurnInput {
    /// Unique identifier for this turn (used to correlate events).
    pub turn_id: String,
    /// The user's message text.
    pub message: String,
}

/// Signal payload for approving or denying a tool execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalInput {
    /// The call_id from the ExecApprovalRequest event.
    pub call_id: String,
    /// Whether the tool execution is approved.
    pub approved: bool,
}

/// Pending approval state tracked inside the workflow.
#[derive(Debug, Clone)]
pub struct PendingApproval {
    /// The call_id awaiting approval.
    pub call_id: String,
    /// Set to `Some(true)` or `Some(false)` when the client responds.
    pub decision: Option<bool>,
}

// ---------------------------------------------------------------------------
// Workflow I/O
// ---------------------------------------------------------------------------

/// Input to the codex workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexWorkflowInput {
    /// The user message to process.
    pub user_message: String,
    /// Model to use (e.g. "gpt-4o").
    pub model: String,
    /// Base instructions / system prompt.
    pub instructions: String,
}

/// Output from the codex workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexWorkflowOutput {
    /// The final assistant message, if any.
    pub last_agent_message: Option<String>,
    /// Number of model→tool loop iterations executed.
    pub iterations: u32,
}
