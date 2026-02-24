//! Config.toml loading for the codex-temporal harness.
//!
//! Provides shared config loading used by both CLI and TUI binaries.
//! Config is loaded client-side (outside the workflow sandbox) and
//! extracted into [`CodexWorkflowInput`] fields that flow through
//! Temporal's serialization boundary.

use codex_core::config::ConfigBuilder;
use codex_core::ModelProviderInfo;
use codex_protocol::config_types::{Personality, ReasoningSummary, WebSearchMode};
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AskForApproval;

use crate::types::CodexWorkflowInput;

/// Holds the result of loading config.toml: a template
/// [`CodexWorkflowInput`] and the resolved model provider info.
pub struct HarnessConfig {
    /// Template workflow input with fields populated from config.toml.
    /// The `user_message` field is left empty — callers fill it in.
    pub base_input: CodexWorkflowInput,
    /// Model provider resolved from config.toml (for the worker/activity).
    pub model_provider: ModelProviderInfo,
}

/// Load the codex config.toml via [`ConfigBuilder`] and extract
/// relevant fields into a [`HarnessConfig`].
///
/// This performs file I/O (reads config layers from disk) and must
/// be called outside the Temporal workflow sandbox.
pub async fn load_harness_config() -> Result<HarnessConfig, Box<dyn std::error::Error>> {
    let config = ConfigBuilder::default().build().await?;

    // --- model ---
    let model = config
        .model
        .clone()
        .unwrap_or_else(|| "gpt-4o".to_string());

    // --- instructions ---
    let instructions = config
        .base_instructions
        .clone()
        .unwrap_or_else(|| "You are a helpful coding assistant.".to_string());

    // --- developer instructions ---
    let developer_instructions = config.developer_instructions.clone();

    // --- approval policy ---
    let approval_policy: AskForApproval = *config.permissions.approval_policy.get();

    // --- web search mode ---
    // Config uses Constrained<WebSearchMode> where Disabled means off.
    // Workflow input uses Option<WebSearchMode> where None means off.
    let web_search_mode = match *config.web_search_mode.get() {
        WebSearchMode::Disabled => None,
        other => Some(other),
    };

    // --- reasoning ---
    let reasoning_effort = config.model_reasoning_effort;
    let reasoning_summary = config.model_reasoning_summary;

    // --- personality ---
    let personality = config.personality;

    // --- model provider ---
    let model_provider = config.model_provider.clone();

    let base_input = CodexWorkflowInput {
        user_message: String::new(),
        model,
        instructions,
        approval_policy,
        web_search_mode,
        reasoning_effort,
        reasoning_summary,
        personality,
        developer_instructions,
        model_provider: None, // Set by caller from HarnessConfig.model_provider
        continued_state: None,
    };

    Ok(HarnessConfig {
        base_input,
        model_provider,
    })
}

/// Apply environment variable overrides on top of config.toml values.
///
/// Environment variables take highest priority for backward compatibility
/// with the pre-config.toml workflow.
pub fn apply_env_overrides(input: &mut CodexWorkflowInput) {
    // CODEX_MODEL
    if let Ok(model) = std::env::var("CODEX_MODEL") {
        input.model = model;
    }

    // CODEX_APPROVAL_POLICY
    if let Ok(val) = std::env::var("CODEX_APPROVAL_POLICY") {
        input.approval_policy = match val.as_str() {
            "never" => AskForApproval::Never,
            "untrusted" => AskForApproval::UnlessTrusted,
            "on-failure" => AskForApproval::OnFailure,
            _ => input.approval_policy,
        };
    }

    // CODEX_WEB_SEARCH
    if let Ok(val) = std::env::var("CODEX_WEB_SEARCH") {
        input.web_search_mode = match val.as_str() {
            "live" => Some(WebSearchMode::Live),
            "cached" => Some(WebSearchMode::Cached),
            "disabled" => None,
            _ => input.web_search_mode,
        };
    }

    // CODEX_EFFORT
    if let Ok(val) = std::env::var("CODEX_EFFORT") {
        input.reasoning_effort = match val.as_str() {
            "low" => Some(ReasoningEffort::Low),
            "medium" => Some(ReasoningEffort::Medium),
            "high" => Some(ReasoningEffort::High),
            _ => input.reasoning_effort,
        };
    }

    // CODEX_REASONING_SUMMARY
    if let Ok(val) = std::env::var("CODEX_REASONING_SUMMARY") {
        input.reasoning_summary = match val.as_str() {
            "concise" => ReasoningSummary::Concise,
            "detailed" => ReasoningSummary::Detailed,
            _ => input.reasoning_summary,
        };
    }

    // CODEX_PERSONALITY
    if let Ok(val) = std::env::var("CODEX_PERSONALITY") {
        input.personality = match val.as_str() {
            "friendly" => Some(Personality::Friendly),
            "pragmatic" => Some(Personality::Pragmatic),
            _ => input.personality,
        };
    }
}
