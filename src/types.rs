//! Serializable I/O types for Temporal activities.
//!
//! These types are sent across the Temporal activity boundary, so they must
//! implement `Serialize` + `Deserialize`.

use codex_core::ToolSpec;
use codex_protocol::config_types::{Personality, ReasoningSummary};
use codex_protocol::models::{ResponseInputItem, ResponseItem};
use codex_protocol::openai_models::{ModelInfo, ReasoningEffort};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Model call activity I/O
// ---------------------------------------------------------------------------

/// Input to the `model_call` activity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCallInput {
    /// Stable conversation ID (workflow-scoped) for prompt caching.
    pub conversation_id: String,
    /// Conversation context items sent to the model.
    pub input: Vec<ResponseItem>,
    /// Tool definitions (carried as typed specs, not pre-serialized JSON).
    pub tools: Vec<ToolSpec>,
    /// Whether parallel tool calls are permitted.
    pub parallel_tool_calls: bool,
    /// Base instructions for the model.
    pub instructions: String,
    /// Full model metadata (slug, capabilities, etc.).
    pub model_info: ModelInfo,
    /// Optional reasoning effort level.
    #[serde(default)]
    pub effort: Option<ReasoningEffort>,
    /// Reasoning summary mode.
    #[serde(default)]
    pub summary: ReasoningSummary,
    /// Optional personality for the model.
    #[serde(default)]
    pub personality: Option<Personality>,
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
    /// Model slug (needed to build ToolsConfig for the registry).
    pub model: String,
    /// Working directory for tool execution.
    pub cwd: String,
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
    /// Tool approval policy — controls when tool executions require user
    /// approval before running.
    #[serde(default)]
    pub approval_policy: codex_protocol::protocol::AskForApproval,
    /// Web search mode — controls whether the model can perform web searches.
    /// Defaults to `None` (disabled). Set to `Cached` or `Live` to enable.
    #[serde(default)]
    pub web_search_mode: Option<codex_protocol::config_types::WebSearchMode>,
}

/// Output from the codex workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexWorkflowOutput {
    /// The final assistant message, if any.
    pub last_agent_message: Option<String>,
    /// Number of model→tool loop iterations executed.
    pub iterations: u32,
}
