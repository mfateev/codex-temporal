//! Temporal workflow integration for OpenAI Codex.
//!
//! This crate provides the infrastructure to run Codex's agentic loop inside
//! Temporal workflows for durable execution.
//!
//! # Architecture
//!
//! The agent workflow runs a model-as-activity pattern:
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────────────────┐
//! │                         Temporal Workflow                                 │
//! │                                                                           │
//! │  agent_workflow(input: AgentInput)                                        │
//! │    ├─ Initialize conversation history                                     │
//! │    └─ Agent Loop (deterministic):                                         │
//! │        ├─ invoke_model_activity(history, tools) → ModelOutput             │
//! │        │   └─ Returns: text + tool_calls[]                                │
//! │        ├─ If tool_calls:                                                  │
//! │        │   └─ http_fetch_activity(url) → response body                    │
//! │        │   └─ Add result to history, continue loop                        │
//! │        └─ If no tool_calls: return final response                         │
//! └──────────────────────────────────────────────────────────────────────────┘
//!
//! Activities:
//! ├─ invoke_model_activity: OpenAI Responses API call
//! └─ http_fetch_activity: Simple HTTP GET (tool demo)
//! ```

pub mod activities;
pub mod adapters;
pub mod types;
pub mod workflow;

// Re-export key types for convenient access
pub use activities::{
    http_fetch_activity, http_fetch_tool_def, invoke_model_activity, model_stream_activity,
    HttpFetchInput, HttpFetchOutput, ModelActivityInput, ModelActivityOutput, ModelInput,
    ModelOutput,
};
pub use adapters::entropy::{WorkflowClock, WorkflowRandomSource};
pub use types::{AgentInput, AgentOutput, FunctionCall, FunctionDef, InputItem, ToolCallMessage, ToolDef};
pub use workflow::{agent_workflow, codex_workflow, CodexWorkflowInput, CodexWorkflowOutput};
