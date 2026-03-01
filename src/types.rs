//! Serializable I/O types for Temporal activities.
//!
//! These types are sent across the Temporal activity boundary, so they must
//! implement `Serialize` + `Deserialize`.

use std::collections::{BTreeMap, HashMap};

use codex_core::{ModelProviderInfo, ToolSpec};
use codex_protocol::config_types::{Personality, ReasoningSummary};
use codex_protocol::models::{ResponseInputItem, ResponseItem};
use codex_protocol::openai_models::{ModelInfo, ReasoningEffort};
use codex_protocol::protocol::{AskForApproval, GitInfo, RolloutItem, TokenUsage};
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
    /// Optional model provider info override (from config.toml).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ModelProviderInfo>,
}

/// Output from the `model_call` activity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCallOutput {
    /// The collected response events from the model, represented as
    /// response items (OutputItemDone payloads).
    pub items: Vec<ResponseItem>,
    /// Token usage from the model response (includes cached_input_tokens).
    #[serde(default)]
    pub token_usage: Option<codex_protocol::protocol::TokenUsage>,
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
    /// Merged config TOML string (from load_config activity).
    /// When present, used to reconstruct a full Config with real features,
    /// sandbox policy, etc. When absent, falls back to Config::for_harness().
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_toml: Option<String>,
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
// Project context activity I/O
// ---------------------------------------------------------------------------

/// Output from the `collect_project_context` activity.
///
/// Captures project-level context (AGENTS.md docs, git info, cwd) from the
/// worker's environment so the workflow can inject it into model prompts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectContextOutput {
    /// Working directory the worker is running in.
    pub cwd: String,
    /// Concatenated AGENTS.md content (hierarchical, from git root to cwd).
    #[serde(default)]
    pub user_instructions: Option<String>,
    /// Git repository info (commit, branch, remote URL).
    #[serde(default)]
    pub git_info: Option<GitInfo>,
}

// ---------------------------------------------------------------------------
// Config activity I/O
// ---------------------------------------------------------------------------

/// Output from the `load_config` activity.
///
/// Contains the merged config.toml content as a TOML string, which can be
/// deserialized into `ConfigToml` and used to construct a full `Config` via
/// `Config::from_toml()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigOutput {
    /// Merged config.toml content as a TOML string.
    pub config_toml: String,
}

// ---------------------------------------------------------------------------
// MCP discovery activity I/O
// ---------------------------------------------------------------------------

/// Input to the `discover_mcp_tools` activity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpDiscoverInput {
    /// Merged config.toml content as a TOML string.
    pub config_toml: String,
    /// Working directory (needed to build Config).
    pub cwd: String,
}

/// Output from the `discover_mcp_tools` activity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpDiscoverOutput {
    /// Qualified tool name ("mcp__server__tool") → serialized `rmcp::model::Tool`.
    pub tools: HashMap<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// MCP tool call activity I/O
// ---------------------------------------------------------------------------

/// Input to the `mcp_tool_call` activity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolCallInput {
    /// Qualified MCP tool name (e.g. "mcp__echo__echo").
    pub qualified_name: String,
    /// The call_id from the model.
    pub call_id: String,
    /// JSON string of tool arguments from the model.
    pub arguments: String,
}

/// Output from the `mcp_tool_call` activity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolCallOutput {
    /// The call_id echoed back.
    pub call_id: String,
    /// Serialized `codex_protocol::mcp::CallToolResult`, or error string.
    pub result: Result<serde_json::Value, String>,
    /// If the MCP server requested elicitation during this call, the
    /// details are captured here for the workflow to handle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elicitation: Option<CapturedElicitation>,
}

/// Elicitation details captured from an MCP server during a tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapturedElicitation {
    /// Name of the MCP server.
    pub server_name: String,
    /// Request ID from the MCP protocol.
    pub request_id: codex_protocol::mcp::RequestId,
    /// Human-readable elicitation message.
    pub message: String,
}

impl McpToolCallOutput {
    /// Convert this output into a `ResponseInputItem::McpToolCallOutput`.
    pub fn into_response_input_item(self) -> ResponseInputItem {
        let result = match self.result {
            Ok(value) => {
                match serde_json::from_value::<codex_protocol::mcp::CallToolResult>(value) {
                    Ok(ctr) => Ok(ctr),
                    Err(e) => Err(format!("failed to deserialize CallToolResult: {e}")),
                }
            }
            Err(e) => Err(e),
        };

        ResponseInputItem::McpToolCallOutput {
            call_id: self.call_id,
            result,
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
    /// Optional per-turn reasoning effort override.
    #[serde(default)]
    pub effort: Option<ReasoningEffort>,
    /// Per-turn reasoning summary mode.
    #[serde(default)]
    pub summary: ReasoningSummary,
    /// Optional per-turn personality override.
    #[serde(default)]
    pub personality: Option<Personality>,
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

/// Pending `request_user_input` state tracked inside the workflow.
#[derive(Debug, Clone)]
pub struct PendingUserInput {
    /// The call_id for the `request_user_input` tool call.
    pub call_id: String,
    /// Set to `Some(...)` when the client responds via `Op::UserInputAnswer`.
    pub response: Option<codex_protocol::request_user_input::RequestUserInputResponse>,
}

/// Pending `apply_patch` approval state tracked inside the workflow.
#[derive(Debug, Clone)]
pub struct PendingPatchApproval {
    /// The call_id awaiting approval.
    pub call_id: String,
    /// Set to `Some(true)` or `Some(false)` when the client responds.
    pub decision: Option<bool>,
}

/// Pending dynamic tool call state tracked inside the workflow.
#[derive(Debug, Clone)]
pub struct PendingDynamicTool {
    /// The call_id for the dynamic tool call.
    pub call_id: String,
    /// Set when the client responds via `Op::DynamicToolResponse`.
    pub response: Option<codex_protocol::dynamic_tools::DynamicToolResponse>,
}

/// Pending MCP elicitation state tracked inside the workflow.
#[derive(Debug, Clone)]
pub struct PendingElicitation {
    /// Name of the MCP server that issued the request.
    pub server_name: String,
    /// Request identifier from the MCP server.
    pub request_id: codex_protocol::mcp::RequestId,
    /// Set when the client responds via `Op::ResolveElicitation`.
    pub response: Option<codex_protocol::approvals::ElicitationAction>,
}

// ---------------------------------------------------------------------------
// Crew type definitions
// ---------------------------------------------------------------------------

/// Mode for a crew type — interactive or autonomous.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CrewMode {
    #[default]
    Interactive,
    Autonomous,
}

/// A crew type definition — reusable template for starting sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrewType {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub mode: CrewMode,
    /// Initial prompt template (autonomous mode). `{input}` placeholders.
    #[serde(default)]
    pub initial_prompt: Option<String>,
    /// Required/optional inputs with descriptions and defaults.
    #[serde(default)]
    pub inputs: BTreeMap<String, CrewInputSpec>,
    /// Which agent is the entry point.
    #[serde(default = "default_main_agent")]
    pub main_agent: String,
    /// Agent definitions — inline config or role reference.
    #[serde(default)]
    pub agents: BTreeMap<String, CrewAgentDef>,
    /// Session-level approval policy override.
    #[serde(default)]
    pub approval_policy: Option<AskForApproval>,
}

fn default_main_agent() -> String {
    "default".to_string()
}

fn default_true() -> bool {
    true
}

/// Specification for a crew input parameter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrewInputSpec {
    pub description: String,
    #[serde(default = "default_true")]
    pub required: bool,
    #[serde(default)]
    pub default: Option<String>,
}

/// Agent definition within a crew type.
///
/// Either fully inline (model + instructions) or a reference to an existing
/// role.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrewAgentDef {
    /// Reference to an existing role (from config.toml or built-in).
    /// If set, role config is used as base; inline fields override.
    #[serde(default)]
    pub role: Option<String>,
    /// Model override for this agent.
    #[serde(default)]
    pub model: Option<String>,
    /// Instructions template for this agent (`{input}` placeholders).
    #[serde(default)]
    pub instructions: Option<String>,
    /// Human-readable description (shown in spawn_agent tool spec).
    #[serde(default)]
    pub description: Option<String>,
}

// ---------------------------------------------------------------------------
// Harness types (session registry)
// ---------------------------------------------------------------------------

/// A session entry in the harness registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    /// Workflow ID of the session (e.g. "codex-session-<uuid>").
    pub session_id: String,
    /// Optional human-readable name.
    pub name: Option<String>,
    /// Model slug used by this session.
    pub model: String,
    /// Session creation time as Unix millis.
    pub created_at_millis: u64,
    /// Current session status.
    pub status: SessionStatus,
    /// Crew type name, if this session was started from a crew.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crew_type: Option<String>,
}

/// Status of a tracked session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionStatus {
    Running,
    Completed,
    Failed,
}

/// Input to the `CodexHarness` workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessInput {
    /// State carried over from a previous continue-as-new execution.
    #[serde(default)]
    pub continued_state: Option<HarnessState>,
}

/// State carried across a continue-as-new boundary for the harness.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessState {
    /// Known sessions tracked by the harness.
    pub sessions: Vec<SessionEntry>,
}

// ---------------------------------------------------------------------------
// Workflow I/O (AgentWorkflow — formerly CodexWorkflow)
// ---------------------------------------------------------------------------

/// Input to the agent workflow.
///
/// Renamed from `CodexWorkflowInput`; the old name is kept as a type alias
/// for backward compatibility.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentWorkflowInput {
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
    /// Optional reasoning effort level for the model.
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Reasoning summary mode.
    #[serde(default)]
    pub reasoning_summary: ReasoningSummary,
    /// Optional personality for the model.
    #[serde(default)]
    pub personality: Option<Personality>,
    /// Developer instructions from config.toml, injected as a separate
    /// developer-role message in the prompt context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub developer_instructions: Option<String>,
    /// Model provider info from config.toml (base URL, auth, etc.).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_provider: Option<ModelProviderInfo>,
    /// State carried over from a previous continue-as-new execution.
    /// `None` on the first run, `Some(...)` on subsequent runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continued_state: Option<ContinueAsNewState>,

    // --- multi-agent fields (all optional for backward compat) ---

    /// Agent role name (e.g. "default", "explorer", "worker").
    #[serde(default = "default_role")]
    pub role: String,
    /// Pre-resolved merged config TOML from SessionWorkflow.
    /// When `Some`, the agent skips the `load_config` activity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_toml: Option<String>,
    /// Pre-collected project context from SessionWorkflow.
    /// When `Some`, the agent skips the `collect_project_context` activity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_context: Option<ProjectContextOutput>,
    /// Pre-discovered MCP tools from SessionWorkflow.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub mcp_tools: HashMap<String, serde_json::Value>,
    /// Client-defined dynamic tools (handled via signal/wait).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dynamic_tools: Vec<codex_protocol::dynamic_tools::DynamicToolSpec>,
}

fn default_role() -> String {
    "default".to_string()
}

/// Backward-compatible alias.
pub type CodexWorkflowInput = AgentWorkflowInput;

/// Output from the agent workflow.
///
/// Renamed from `CodexWorkflowOutput`; the old name is kept as a type alias
/// for backward compatibility.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentWorkflowOutput {
    /// The final assistant message, if any.
    pub last_agent_message: Option<String>,
    /// Number of model→tool loop iterations executed.
    pub iterations: u32,
    /// Cumulative token usage across all model calls.
    #[serde(default)]
    pub token_usage: Option<TokenUsage>,
}

/// Backward-compatible alias.
pub type CodexWorkflowOutput = AgentWorkflowOutput;

// ---------------------------------------------------------------------------
// SessionWorkflow I/O (parent / control plane)
// ---------------------------------------------------------------------------

/// Input to the `SessionWorkflow` (parent workflow that manages agents).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionWorkflowInput {
    /// The initial user message to forward to the main agent.
    pub user_message: String,
    /// Model to use (e.g. "gpt-4o").
    pub model: String,
    /// Base instructions / system prompt.
    pub instructions: String,
    /// Tool approval policy.
    #[serde(default)]
    pub approval_policy: AskForApproval,
    /// Web search mode.
    #[serde(default)]
    pub web_search_mode: Option<codex_protocol::config_types::WebSearchMode>,
    /// Optional reasoning effort level.
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Reasoning summary mode.
    #[serde(default)]
    pub reasoning_summary: ReasoningSummary,
    /// Optional personality.
    #[serde(default)]
    pub personality: Option<Personality>,
    /// Developer instructions from config.toml.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub developer_instructions: Option<String>,
    /// Model provider info from config.toml.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_provider: Option<ModelProviderInfo>,
    /// Crew agent definitions (non-main agents from a crew type).
    /// Keyed by agent name (e.g. "helper", "fixer").
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub crew_agents: BTreeMap<String, CrewAgentDef>,
    /// State carried over from a previous continue-as-new execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continued_state: Option<SessionContinueAsNewState>,
}

impl From<AgentWorkflowInput> for SessionWorkflowInput {
    fn from(input: AgentWorkflowInput) -> Self {
        Self {
            user_message: input.user_message,
            model: input.model,
            instructions: input.instructions,
            approval_policy: input.approval_policy,
            web_search_mode: input.web_search_mode,
            reasoning_effort: input.reasoning_effort,
            reasoning_summary: input.reasoning_summary,
            personality: input.personality,
            developer_instructions: input.developer_instructions,
            model_provider: input.model_provider,
            crew_agents: BTreeMap::new(),
            continued_state: None,
        }
    }
}

/// Output from the `SessionWorkflow`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionWorkflowOutput {
    /// Summary of all agents that ran in this session.
    pub agents: Vec<AgentSummary>,
}

/// Summary of an agent's execution within a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSummary {
    /// Unique agent identifier (e.g. "{session_id}/main").
    pub agent_id: String,
    /// Role name (e.g. "default", "explorer").
    pub role: String,
    /// Final lifecycle status.
    pub status: AgentLifecycle,
    /// Number of model→tool loop iterations.
    pub iterations: u32,
    /// Cumulative token usage.
    #[serde(default)]
    pub token_usage: Option<TokenUsage>,
}

/// Lifecycle status of an agent within a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentLifecycle {
    Running,
    Completed,
    Failed,
}

/// State carried across a continue-as-new boundary for `SessionWorkflow`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionContinueAsNewState {
    /// Tracked agents from the previous run.
    pub agents: Vec<AgentRecord>,
    /// Cached config TOML string.
    pub config_toml: String,
    /// Cached project context.
    pub project_context: ProjectContextOutput,
    /// Cached MCP tools.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub mcp_tools: HashMap<String, serde_json::Value>,
    /// Crew agent definitions carried across CAN.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub crew_agents: BTreeMap<String, CrewAgentDef>,
}

/// Record of a child agent workflow tracked by `SessionWorkflow`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRecord {
    /// Unique agent identifier (workflow ID of the child AgentWorkflow).
    pub agent_id: String,
    /// Workflow ID of the child AgentWorkflow (same as agent_id).
    pub workflow_id: String,
    /// Role name.
    pub role: String,
    /// Current lifecycle status.
    pub status: AgentLifecycle,
}

/// Signal payload for spawning a new agent in the session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnAgentInput {
    /// Role to assign to the new agent (e.g. "explorer", "worker").
    pub role: String,
    /// Initial message for the new agent.
    pub message: String,
}

// ---------------------------------------------------------------------------
// Role resolution activity I/O
// ---------------------------------------------------------------------------

/// Input to the `resolve_role_config` activity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolveRoleConfigInput {
    /// Merged config TOML string (base config).
    pub config_toml: String,
    /// Working directory.
    pub cwd: String,
    /// Role name to resolve (e.g. "explorer").
    pub role_name: String,
}

/// Output from the `resolve_role_config` activity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolveRoleConfigOutput {
    /// Merged config TOML string after applying the role overlay.
    pub config_toml: String,
    /// Resolved model slug.
    #[serde(default)]
    pub model: Option<String>,
    /// Resolved instructions.
    #[serde(default)]
    pub instructions: Option<String>,
    /// Resolved reasoning effort.
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Resolved reasoning summary.
    #[serde(default)]
    pub reasoning_summary: ReasoningSummary,
    /// Resolved personality.
    #[serde(default)]
    pub personality: Option<Personality>,
    /// Resolved developer instructions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub developer_instructions: Option<String>,
    /// Resolved model provider info.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_provider: Option<ModelProviderInfo>,
}

// ---------------------------------------------------------------------------
// Continue-as-new state
// ---------------------------------------------------------------------------

/// State carried across a continue-as-new boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContinueAsNewState {
    /// Accumulated conversation history (RolloutItems from InMemoryStorage).
    pub rollout_items: Vec<RolloutItem>,
    /// Queued user turns not yet processed.
    pub pending_user_turns: Vec<UserTurnInput>,
    /// Cumulative turn count across all CAN runs.
    pub cumulative_turn_count: u32,
    /// Cumulative model-to-tool loop iteration count.
    pub cumulative_iterations: u32,
    /// Cumulative token usage across all CAN runs.
    #[serde(default)]
    pub cumulative_token_usage: Option<TokenUsage>,
    /// Discovered MCP tool schemas (carried across CAN to avoid re-discovery).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub mcp_tools: HashMap<String, serde_json::Value>,
    /// Overridden approval policy (from `Op::OverrideTurnContext`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_policy_override: Option<AskForApproval>,
    /// Overridden model slug (from `Op::OverrideTurnContext`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_override: Option<String>,
    /// Overridden reasoning effort (from `Op::OverrideTurnContext`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort_override: Option<Option<ReasoningEffort>>,
    /// Overridden reasoning summary (from `Op::OverrideTurnContext`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_override: Option<ReasoningSummary>,
    /// Overridden personality (from `Op::OverrideTurnContext`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub personality_override: Option<Personality>,
}
