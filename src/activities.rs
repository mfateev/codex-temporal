//! Temporal activity implementations for the codex harness.
//!
//! Activities run outside the deterministic workflow sandbox — they can
//! perform real I/O (HTTP calls, shell commands, etc.).  Results are
//! recorded in the workflow history for deterministic replay.

use std::path::PathBuf;
use std::sync::Arc;

use codex_core::auth::AuthCredentialsStoreMode;
use codex_core::config::ConfigBuilder;
use codex_core::models_manager::manager::{ModelsManager, RefreshStrategy};
use codex_core::models_manager::collaboration_mode_presets::CollaborationModesConfig;
use codex_core::{
    AuthManager, EventSink, ModelClient, ModelProviderInfo, Prompt, ResponseEvent, Session,
    StorageBackend, ToolPayload, TurnContext, TurnDiffTracker, ToolsConfig, ToolsConfigParams,
    built_in_model_providers,
};
use codex_core::error::CodexErr;
use codex_core::tools::router::{ToolCall, ToolCallSource, ToolRouter, ToolRouterParams};
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_otel::SessionTelemetry;
use codex_protocol::models::{BaseInstructions, ResponseItem};
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::protocol::SessionSource;
use codex_protocol::ThreadId;
use futures::StreamExt;
use temporalio_macros::activities;
use temporalio_sdk::activities::{ActivityContext, ActivityError};
use tokio::sync::Mutex;

use crate::config_loader::config_from_toml;
use crate::mcp::HarnessMcpManager;
use crate::sink::BufferEventSink;
use crate::storage::InMemoryStorage;
use crate::types::{
    ConfigOutput, McpDiscoverInput, McpDiscoverOutput, McpToolCallInput, McpToolCallOutput,
    ModelCallInput, ModelCallOutput, ProjectContextOutput, ResolveModelInfoInput,
    ResolveRoleConfigInput, ResolveRoleConfigOutput, ToolExecInput, ToolExecOutput,
};

/// Resolve the model provider to use for the activity.
///
/// Starts from the built-in OpenAI provider but overrides it to use API-key
/// auth (`OPENAI_API_KEY` env var) instead of ChatGPT OAuth — activities run
/// headless so there is no interactive login flow.
fn resolve_provider() -> ModelProviderInfo {
    let mut provider = built_in_model_providers(None)
        .remove("openai")
        .expect("built-in openai provider must exist");

    // Switch from ChatGPT OAuth to API-key auth.
    provider.requires_openai_auth = false;
    provider.env_key = Some("OPENAI_API_KEY".to_string());

    // Honour explicit base-URL override.
    if let Ok(base) = std::env::var("OPENAI_BASE_URL") {
        provider.base_url = Some(base);
    }

    // If a bearer token is supplied directly, prefer it over the env_key
    // mechanism (useful for programmatic / test scenarios).
    if let Ok(token) = std::env::var("OPENAI_BEARER_TOKEN") {
        provider.experimental_bearer_token = Some(token);
    }

    provider
}

/// Activity implementations for the codex workflow.
pub struct CodexActivities {
    provider: ModelProviderInfo,
    /// Auth manager for model API calls (uses API-key from env, ephemeral store).
    /// Held here to keep the `Arc` alive for `ModelsManager`.
    _auth_manager: Arc<AuthManager>,
    /// Models manager backed by bundled catalog + API refresh.
    models_manager: Arc<ModelsManager>,
    /// Persistent MCP server connections (initialized via `discover_mcp_tools`).
    mcp_manager: Arc<Mutex<HarnessMcpManager>>,
}

impl Default for CodexActivities {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexActivities {
    /// Create a new `CodexActivities` with the provider resolved once.
    pub fn new() -> Self {
        let provider = resolve_provider();
        let codex_home = codex_core::config::find_codex_home()
            .unwrap_or_else(|_| PathBuf::from("/tmp/codex-temporal"));
        let auth_manager = Arc::new(AuthManager::new(
            codex_home.clone(),
            /* enable_codex_api_key_env */ true,
            AuthCredentialsStoreMode::Ephemeral,
        ));
        let models_manager = Arc::new(ModelsManager::new_with_provider(
            codex_home,
            Arc::clone(&auth_manager),
            /* model_catalog */ None,
            CollaborationModesConfig::default(),
            provider.clone(),
        ));
        Self {
            provider,
            _auth_manager: auth_manager,
            models_manager,
            mcp_manager: Arc::new(Mutex::new(HarnessMcpManager::new())),
        }
    }
}

#[activities]
impl CodexActivities {
    /// Call the model API using the full codex client stack (provider
    /// resolution, retries, auth refresh, etc.) and return collected output
    /// items.
    #[activity]
    pub async fn model_call(
        self: Arc<Self>,
        ctx: ActivityContext,
        input: ModelCallInput,
    ) -> Result<ModelCallOutput, ActivityError> {
        // Use provider from workflow input (config.toml) if present,
        // falling back to the activity-level provider. Always apply
        // the same headless-auth fixup that resolve_provider() does:
        // activities run without interactive login, so switch to
        // API-key auth and honour env-var overrides.
        let provider = {
            let mut p = input.provider.clone().unwrap_or_else(|| self.provider.clone());
            // Ensure API-key auth works (activities have no interactive login).
            p.requires_openai_auth = false;
            if p.env_key.is_none() {
                p.env_key = Some("OPENAI_API_KEY".to_string());
            }
            if let Ok(base) = std::env::var("OPENAI_BASE_URL") {
                p.base_url = Some(base);
            }
            if let Ok(token) = std::env::var("OPENAI_BEARER_TOKEN") {
                p.experimental_bearer_token = Some(token);
            }
            p
        };
        let conversation_id = ThreadId::from_string(&input.conversation_id)
            .map_err(|e| anyhow::anyhow!("invalid conversation_id: {e}"))?;

        let model_client = ModelClient::new(
            None, // no AuthManager — uses env_key / bearer token
            conversation_id,
            provider,
            SessionSource::Exec,
            None,  // model_verbosity
            false, // responses_websockets_enabled_by_feature
            false, // request compression
            false, // timing metrics
            None,  // beta features header
        );

        let mut session = model_client.new_session();

        let prompt = Prompt {
            input: input.input,
            tools: input.tools,
            parallel_tool_calls: input.parallel_tool_calls,
            base_instructions: BaseInstructions {
                text: input.instructions,
            },
            personality: input.personality,
            output_schema: None,
        };

        let session_telemetry = SessionTelemetry::new(
            conversation_id,
            &input.model_info.slug,
            &input.model_info.slug,
            None,
            None,
            None,
            "temporal-activity".to_string(),
            false,
            "activity".to_string(),
            SessionSource::Exec,
        );

        tracing::debug!(
            model = %input.model_info.slug,
            input_items = prompt.input.len(),
            tools = prompt.tools.len(),
            effort = ?input.effort,
            "calling model via codex client"
        );

        let mut stream = session
            .stream(
                &prompt,
                &input.model_info,
                &session_telemetry,
                input.effort,
                input.summary,
                None,
                None,
            )
            .await
            .map_err(codex_err_to_activity_error)?;

        let mut items: Vec<ResponseItem> = Vec::new();
        let mut token_usage = None;
        while let Some(event) = stream.next().await {
            // Heartbeat on every stream event so the server knows the
            // activity is still alive while waiting for the model.
            ctx.record_heartbeat(vec![]);
            match event {
                Ok(ResponseEvent::OutputItemDone(item)) => {
                    items.push(item);
                }
                Ok(ResponseEvent::Completed { token_usage: usage, .. }) => {
                    token_usage = usage;
                    break;
                }
                Ok(_) => {} // Created, Delta, etc.
                Err(e) => {
                    return Err(codex_err_to_activity_error(e));
                }
            }
        }

        tracing::debug!(
            output_items = items.len(),
            ?token_usage,
            "model_call completed"
        );

        Ok(ModelCallOutput { items, token_usage })
    }

    /// Execute a tool using codex-core's full ToolRegistry dispatch.
    ///
    /// Supports all tools registered by `build_specs`: shell, apply_patch,
    /// read_file, list_dir, grep_files, etc.
    #[activity]
    pub async fn tool_exec(
        self: Arc<Self>,
        _ctx: ActivityContext,
        input: ToolExecInput,
    ) -> Result<ToolExecOutput, ActivityError> {
        tracing::debug!(
            tool = %input.tool_name,
            call_id = %input.call_id,
            cwd = %input.cwd,
            has_config = input.config_toml.is_some(),
            "tool_exec activity invoked"
        );

        dispatch_tool(input)
            .await
            .map_err(|e| anyhow::anyhow!("tool_exec failed: {e}").into())
    }

    /// Load the merged config.toml from the worker's environment.
    ///
    /// Runs `ConfigBuilder::build_toml_string()` to read and merge all
    /// config layers from disk, returning the effective config as a TOML
    /// string. The workflow deserializes this into `ConfigToml` and builds
    /// a full `Config` using `Config::from_toml()`.
    ///
    /// Also returns the worker's activity token so the workflow can include
    /// it in subsequent `ToolExecInput` calls for authentication.
    #[activity]
    pub async fn load_config(
        self: Arc<Self>,
        _ctx: ActivityContext,
        _input: (),
    ) -> Result<ConfigOutput, ActivityError> {
        tracing::debug!("load_config activity invoked");

        let toml_string = ConfigBuilder::default()
            .build_toml_string()
            .await
            .map_err(|e| anyhow::anyhow!("config loading failed: {e}"))?;

        tracing::debug!(
            toml_len = toml_string.len(),
            "config loaded successfully"
        );

        Ok(ConfigOutput {
            config_toml: toml_string,
        })
    }

    /// Discover MCP tools from configured servers.
    ///
    /// Connects to all enabled MCP servers from config.toml, performs the
    /// MCP handshake, and lists available tools. Results are cached in the
    /// `mcp_manager` for subsequent `mcp_tool_call` invocations.
    #[activity]
    pub async fn discover_mcp_tools(
        self: Arc<Self>,
        _ctx: ActivityContext,
        input: McpDiscoverInput,
    ) -> Result<McpDiscoverOutput, ActivityError> {
        tracing::debug!("discover_mcp_tools activity invoked");

        let cwd = std::path::PathBuf::from(&input.cwd);
        let config = config_from_toml(&input.config_toml, &cwd, None)
            .map_err(|e| anyhow::anyhow!("failed to build config from TOML: {e}"))?;

        let mcp_servers = config.mcp_servers.get().clone();
        if mcp_servers.is_empty() {
            tracing::debug!("no MCP servers configured");
            return Ok(McpDiscoverOutput {
                tools: std::collections::HashMap::new(),
            });
        }

        tracing::debug!(servers = mcp_servers.len(), "initializing MCP servers");

        let mut manager = self.mcp_manager.lock().await;
        let discovered = manager
            .initialize(&mcp_servers)
            .await
            .map_err(|e| anyhow::anyhow!("MCP discovery failed: {e}"))?;

        // Serialize each rmcp::model::Tool to serde_json::Value for the
        // activity boundary.
        let tools = discovered
            .into_iter()
            .filter_map(|(name, tool)| {
                match serde_json::to_value(&tool) {
                    Ok(value) => Some((name, value)),
                    Err(e) => {
                        tracing::warn!(tool = %name, error = %e, "failed to serialize MCP tool");
                        None
                    }
                }
            })
            .collect();

        tracing::debug!("MCP discovery complete");
        Ok(McpDiscoverOutput { tools })
    }

    /// Execute a tool call on an MCP server.
    ///
    /// Routes the call to the appropriate server based on the qualified
    /// tool name prefix (`mcp__server__tool`).
    #[activity]
    pub async fn mcp_tool_call(
        self: Arc<Self>,
        _ctx: ActivityContext,
        input: McpToolCallInput,
    ) -> Result<McpToolCallOutput, ActivityError> {
        tracing::debug!(
            tool = %input.qualified_name,
            call_id = %input.call_id,
            "mcp_tool_call activity invoked"
        );

        let arguments: Option<serde_json::Value> = if input.arguments.is_empty() {
            None
        } else {
            match serde_json::from_str(&input.arguments) {
                Ok(v) => Some(v),
                Err(e) => {
                    return Ok(McpToolCallOutput {
                        call_id: input.call_id,
                        result: Err(format!("invalid JSON arguments: {e}")),
                        elicitation: None,
                    });
                }
            }
        };

        let manager = self.mcp_manager.lock().await;
        // Clear any previously captured elicitation.
        manager.take_captured_elicitation().await;

        let result = match manager
            .call_tool(&input.qualified_name, arguments)
            .await
        {
            Ok(call_result) => match serde_json::to_value(&call_result) {
                Ok(v) => Ok(v),
                Err(e) => Err(format!("failed to serialize CallToolResult: {e}")),
            },
            Err(e) => Err(format!("{e}")),
        };

        // Check if an elicitation was captured during this call.
        let elicitation = manager.take_captured_elicitation().await;

        Ok(McpToolCallOutput {
            call_id: input.call_id,
            result,
            elicitation,
        })
    }

    /// Collect project context from the worker's environment.
    ///
    /// Reads AGENTS.md project docs (hierarchical chain from git root to cwd)
    /// and git repository info (commit, branch, remote URL).  Runs once per
    /// workflow start; the result is replayed deterministically on recovery.
    #[activity]
    pub async fn collect_project_context(
        _ctx: ActivityContext,
    ) -> Result<ProjectContextOutput, ActivityError> {
        let cwd = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/tmp"));
        let cwd_str = cwd.to_string_lossy().to_string();

        tracing::debug!(cwd = %cwd_str, "collecting project context");

        // Build a minimal config pointing at the worker's cwd.
        let codex_home = PathBuf::from("/tmp/codex-temporal");
        let mut config = codex_core::config::Config::for_harness(codex_home)
            .map_err(|e| anyhow::anyhow!("failed to build config for project context: {e}"))?;
        config.cwd = cwd.clone();

        // Read AGENTS.md chain (git root → cwd).
        let user_instructions = codex_core::project_doc::read_project_docs(&config)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "failed to read project docs");
                None
            });

        // Collect git info (commit, branch, remote URL).
        let git_info = codex_core::git_info::collect_git_info(&cwd).await;

        tracing::debug!(
            has_instructions = user_instructions.is_some(),
            has_git = git_info.is_some(),
            "project context collected"
        );

        Ok(ProjectContextOutput {
            cwd: cwd_str,
            user_instructions,
            git_info,
        })
    }

    /// Check if the worker has API credentials available.
    ///
    /// Returns `true` if `OPENAI_API_KEY` or `OPENAI_BEARER_TOKEN` is set
    /// and non-empty in the worker's environment.
    #[activity]
    pub async fn check_credentials(
        self: Arc<Self>,
        _ctx: ActivityContext,
        _input: (),
    ) -> Result<bool, ActivityError> {
        let has_env_key = std::env::var("OPENAI_API_KEY")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .is_some();
        let has_bearer = std::env::var("OPENAI_BEARER_TOKEN")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .is_some();
        Ok(has_env_key || has_bearer)
    }

    /// Resolve role-specific config by applying a role overlay on top of the
    /// base config.
    ///
    /// Uses `codex_core::agent::role::apply_role_to_config` to merge the
    /// role's config layer (built-in or user-defined) into the base config,
    /// then extracts the resolved fields.
    #[activity]
    pub async fn resolve_role_config(
        _ctx: ActivityContext,
        input: ResolveRoleConfigInput,
    ) -> Result<ResolveRoleConfigOutput, ActivityError> {
        tracing::debug!(role = %input.role_name, "resolve_role_config activity invoked");

        let cwd = PathBuf::from(&input.cwd);
        let mut config = config_from_toml(&input.config_toml, &cwd, None)
            .map_err(|e| anyhow::anyhow!("failed to build config from TOML: {e}"))?;

        codex_core::apply_role_to_config(&mut config, Some(&input.role_name))
            .await
            .map_err(|e| anyhow::anyhow!("failed to apply role '{}': {e}", input.role_name))?;

        // Extract the merged TOML string from the config layer stack.
        let merged_toml = toml::to_string(&config.config_layer_stack.effective_config())
            .map_err(|e| anyhow::anyhow!("failed to serialize merged config: {e}"))?;

        let output = ResolveRoleConfigOutput {
            config_toml: merged_toml,
            model: config.model.clone(),
            instructions: config.base_instructions.clone(),
            reasoning_effort: config.model_reasoning_effort,
            reasoning_summary: config.model_reasoning_summary.unwrap_or_default(),
            personality: config.personality,
            developer_instructions: config.developer_instructions.clone(),
            model_provider: Some(config.model_provider.clone()),
        };

        tracing::debug!(
            role = %input.role_name,
            model = ?output.model,
            "role config resolved"
        );

        Ok(output)
    }

    /// Resolve model metadata from the API (with bundled catalog fallback).
    ///
    /// Refreshes the model catalog from the `/models` API (if not already
    /// cached), then looks up the specified model. Returns full `ModelInfo`
    /// with `apply_patch_tool_type`, `experimental_supported_tools`, etc.
    #[activity]
    pub async fn resolve_model_info(
        self: Arc<Self>,
        _ctx: ActivityContext,
        input: ResolveModelInfoInput,
    ) -> Result<ModelInfo, ActivityError> {
        tracing::debug!(model = %input.model, "resolve_model_info activity invoked");

        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/tmp"));
        let config = config_from_toml(&input.config_toml, &cwd, None)
            .map_err(|e| anyhow::anyhow!("config parse failed: {e}"))?;

        // Refresh from API (uses bundled catalog as fallback on failure).
        let _ = self
            .models_manager
            .list_models(RefreshStrategy::OnlineIfUncached)
            .await;

        let mut model_info = self.models_manager.get_model_info(&input.model, &config).await;
        backfill_experimental_tools(&mut model_info);

        tracing::debug!(
            model = %model_info.slug,
            apply_patch = ?model_info.apply_patch_tool_type,
            experimental_tools = model_info.experimental_supported_tools.len(),
            "model info resolved"
        );

        Ok(model_info)
    }
}

/// Known file tools that codex models support.
///
/// The bundled `models.json` ships with `experimental_supported_tools: []`
/// because the ChatGPT `/models` API is expected to populate them at runtime.
/// That API is only available with ChatGPT OAuth — not API-key auth.  This
/// constant lists the tools that all codex-class models support so the
/// harness can inject them when the API isn't available.
const CODEX_FILE_TOOLS: &[&str] = &["read_file", "list_dir", "grep_files"];

/// Ensure `experimental_supported_tools` is populated for codex-class models.
///
/// If the model has `apply_patch_tool_type` set (indicating it's a
/// codex-capable model) but `experimental_supported_tools` is empty (no
/// ChatGPT API data), inject the standard file tools so that `build_specs`
/// includes them in the tool list.
pub fn backfill_experimental_tools(model_info: &mut ModelInfo) {
    if model_info.apply_patch_tool_type.is_some()
        && model_info.experimental_supported_tools.is_empty()
    {
        model_info.experimental_supported_tools = CODEX_FILE_TOOLS
            .iter()
            .map(|s| (*s).to_string())
            .collect();
    }
}

/// Convert a [`CodexErr`] into the appropriate [`ActivityError`] variant.
///
/// Non-retryable errors (quota exceeded, auth failures, invalid requests, etc.)
/// are returned as `ActivityError::NonRetryable` so Temporal does not retry them.
/// Retryable errors (transient network/stream failures) use the default
/// `ActivityError::Retryable` path.
fn codex_err_to_activity_error(e: CodexErr) -> ActivityError {
    if e.is_retryable() {
        ActivityError::from(anyhow::anyhow!(e))
    } else {
        ActivityError::NonRetryable(Box::new(e))
    }
}

/// Core tool dispatch logic, independent of the Temporal `ActivityContext`.
///
/// Public so that integration tests can exercise the full `build_specs` →
/// `ToolRegistry::dispatch` pipeline without starting a Temporal worker.
pub async fn dispatch_tool(input: ToolExecInput) -> Result<ToolExecOutput, anyhow::Error> {
    use codex_core::config::Constrained;
    use codex_protocol::protocol::AskForApproval;

    // --- Input validation ---
    if input.tool_name.is_empty() {
        anyhow::bail!("tool_name must not be empty");
    }

    let cwd = PathBuf::from(&input.cwd);
    if !cwd.exists() {
        anyhow::bail!(
            "cwd does not exist: {}",
            cwd.display()
        );
    }
    if !cwd.is_dir() {
        anyhow::bail!(
            "cwd is not a directory: {}",
            cwd.display()
        );
    }

    if input.config_toml.is_none() {
        tracing::warn!(
            tool = %input.tool_name,
            "dispatch_tool: no config_toml provided — \
             falling back to Config::for_harness() with default sandbox policy"
        );
    }

    // Build Config: prefer the activity-loaded config TOML when present,
    // falling back to Config::for_harness() for backward compatibility.
    let mut config = if let Some(ref toml_str) = input.config_toml {
        config_from_toml(toml_str, &cwd, None)
            .map_err(|e| anyhow::anyhow!("failed to build config from TOML: {e}"))?
    } else {
        let codex_home = PathBuf::from("/tmp/codex-temporal");
        let mut c = codex_core::config::Config::for_harness(codex_home)
            .map_err(|e| anyhow::anyhow!("failed to build config: {e}"))?;
        c.cwd = cwd;
        c
    };
    config.model = Some(input.model.clone());
    // When the workflow already obtained user approval, override the config's
    // approval_policy to OnRequest so codex-core tool handlers (e.g.
    // exec_command with sandbox_permissions=require_escalated) don't re-check
    // and reject the command.  Approval was already granted at the workflow
    // level by TemporalToolHandler.
    if input.already_approved {
        let _ = config
            .permissions
            .approval_policy
            .set(codex_protocol::protocol::AskForApproval::OnRequest);
    }
    // --- Activity-side approval gate ---
    // When the workflow did NOT already obtain user approval, the activity
    // acts as a second gate using the full execpolicy pipeline (which has
    // access to .rules files on disk) and `assess_patch_safety` (which can
    // do file I/O for path-constraint checking).
    if !input.already_approved {
        use codex_core::exec_policy::{ExecPolicyManager, ExecApprovalRequest};
        use codex_core::ExecApprovalRequirement;
        use codex_protocol::models::SandboxPermissions;
        use codex_protocol::permissions::FileSystemSandboxPolicy;

        let approval_policy = config.permissions.approval_policy.get().clone();
        let sandbox_policy = config.permissions.sandbox_policy.get();
        let fs_sandbox_policy = FileSystemSandboxPolicy::from(sandbox_policy);

        let is_shell_tool = matches!(
            input.tool_name.as_str(),
            "shell" | "container.exec" | "local_shell" | "shell_command" | "unified_exec"
        );

        if is_shell_tool {
            // Parse command from arguments.
            let command: Vec<String> = serde_json::from_str::<serde_json::Value>(&input.arguments)
                .ok()
                .and_then(|v| {
                    v.get("command")?
                        .as_array()?
                        .iter()
                        .map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_else(|| vec![input.arguments.clone()]);

            // Try to load execpolicy rules from config.
            let exec_policy_mgr = ExecPolicyManager::load(&config.config_layer_stack)
                .await
                .unwrap_or_default();

            let requirement = exec_policy_mgr
                .create_exec_approval_requirement_for_command(ExecApprovalRequest {
                    command: &command,
                    approval_policy: approval_policy.clone(),
                    sandbox_policy,
                    file_system_sandbox_policy: &fs_sandbox_policy,
                    sandbox_permissions: SandboxPermissions::default(),
                    prefix_rule: None,
                })
                .await;

            match requirement {
                ExecApprovalRequirement::NeedsApproval { reason, .. } => {
                    anyhow::bail!(
                        "APPROVAL_REQUIRED:{}",
                        reason.unwrap_or_else(|| "execpolicy requires approval".to_string())
                    );
                }
                ExecApprovalRequirement::Forbidden { reason } => {
                    return Ok(ToolExecOutput {
                        call_id: input.call_id,
                        output: format!("Command forbidden: {reason}"),
                        exit_code: 1,
                    });
                }
                ExecApprovalRequirement::Skip { .. } => {
                    // Allowed — proceed with execution.
                }
            }
        }

        if input.tool_name == "apply_patch" {
            use codex_core::safety::assess_patch_safety;
            use codex_apply_patch::{MaybeApplyPatchVerified, maybe_parse_apply_patch_verified};

            // Parse and verify the patch (has file I/O for content verification).
            let patch_text: String = serde_json::from_str::<serde_json::Value>(&input.arguments)
                .ok()
                .and_then(|v| v.get("patch")?.as_str().map(String::from))
                .unwrap_or_else(|| input.arguments.clone());

            let argv = vec!["apply_patch".to_string(), patch_text];
            if let MaybeApplyPatchVerified::Body(action) =
                maybe_parse_apply_patch_verified(&argv, &config.cwd)
            {
                let safety = assess_patch_safety(
                    &action,
                    approval_policy,
                    sandbox_policy,
                    &fs_sandbox_policy,
                    &config.cwd,
                    WindowsSandboxLevel::Disabled,
                );
                match safety {
                    codex_core::safety::SafetyCheck::AskUser => {
                        anyhow::bail!("APPROVAL_REQUIRED:patch requires user approval");
                    }
                    codex_core::safety::SafetyCheck::Reject { reason } => {
                        return Ok(ToolExecOutput {
                            call_id: input.call_id,
                            output: format!("Patch rejected: {reason}"),
                            exit_code: 1,
                        });
                    }
                    codex_core::safety::SafetyCheck::AutoApprove { .. } => {
                        // Allowed — proceed with execution.
                    }
                }
            }
            // If parsing failed, let the tool router handle the error naturally.
        }
    }

    config.permissions.approval_policy = Constrained::allow_any(AskForApproval::Never);

    // The apply_patch tool handler spawns a subprocess using
    // `codex_linux_sandbox_exe` (falling back to `current_exe()`).  In the
    // worker binary this works because it handles `--codex-run-as-apply-patch`.
    // In the test harness however, `current_exe()` is the test binary which
    // does NOT handle that flag.  Point to the worker binary instead.
    if config.codex_linux_sandbox_exe.is_none() {
        if let Ok(exe) = std::env::current_exe() {
            // The worker binary may be a sibling (same dir) or one level up
            // (test binaries live in target/debug/deps/, worker in target/debug/).
            let candidates = [
                exe.with_file_name("codex-temporal-worker"),
                exe.parent()
                    .and_then(|p| p.parent())
                    .map(|p| p.join("codex-temporal-worker"))
                    .unwrap_or_default(),
            ];
            for candidate in &candidates {
                if candidate.exists() {
                    config.codex_linux_sandbox_exe = Some(candidate.clone());
                    break;
                }
            }
        }
    }

    let config = Arc::new(config);

    // Resolve model info from the bundled catalog.
    let model_slug = config.model.clone().unwrap_or_else(|| "gpt-4o".to_string());
    let mut model_info = ModelsManager::resolve_from_bundled_catalog(&model_slug, &config);
    backfill_experimental_tools(&mut model_info);

    // Build the tool router with all standard tools enabled.
    let sandbox_policy = config.permissions.sandbox_policy.get();
    let tools_config = ToolsConfig::new(&ToolsConfigParams {
        model_info: &model_info,
        available_models: &vec![],
        features: &config.features,
        web_search_mode: None,
        session_source: SessionSource::Exec,
        sandbox_policy,
        windows_sandbox_level: WindowsSandboxLevel::Disabled,
    });
    let router = ToolRouter::from_config(&tools_config, ToolRouterParams {
        mcp_tools: None,
        app_tools: None,
        discoverable_tools: None,
        dynamic_tools: &[],
    });

    // Construct minimal Session + TurnContext for the dispatch.
    let conversation_id = ThreadId::new();
    let event_sink: Arc<dyn EventSink> = Arc::new(BufferEventSink::new(4096, 0));
    let storage: Arc<dyn StorageBackend> = Arc::new(InMemoryStorage::new());

    let session = Session::new_minimal(
        conversation_id,
        Arc::clone(&config),
        event_sink,
        storage,
    )
    .await;

    let turn_context = Arc::new(TurnContext::new_minimal(
        input.call_id.clone(),
        model_info,
        Arc::clone(&config),
    ));

    let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));

    // Build a ToolCall for the router, reconstructing the correct payload
    // variant from the serialized payload_kind.
    let payload = match input.payload_kind.as_str() {
        "custom" => ToolPayload::Custom {
            input: input.arguments.clone(),
        },
        _ => ToolPayload::Function {
            arguments: input.arguments.clone(),
        },
    };
    let tool_call = ToolCall {
        tool_name: input.tool_name.clone(),
        tool_namespace: None,
        call_id: input.call_id.clone(),
        payload,
    };

    // Dispatch via the router.
    match router
        .dispatch_tool_call(session, turn_context, tracker, tool_call, ToolCallSource::Direct)
        .await
    {
        Ok(response_item) => {
            let (output, exit_code) = extract_tool_output(&response_item);
            Ok(ToolExecOutput {
                call_id: input.call_id,
                output,
                exit_code,
            })
        }
        Err(e) => {
            Ok(ToolExecOutput {
                call_id: input.call_id,
                output: format!("tool dispatch error: {e}"),
                exit_code: 1,
            })
        }
    }
}

/// Extract output text and an exit code from a tool dispatch response.
fn extract_tool_output(
    item: &codex_protocol::models::ResponseInputItem,
) -> (String, i32) {
    use codex_protocol::models::ResponseInputItem;

    match item {
        ResponseInputItem::FunctionCallOutput { output, .. } => {
            let text = output.body.to_text().unwrap_or_default();
            let success = output.success.unwrap_or(true);
            (text, if success { 0 } else { 1 })
        }
        ResponseInputItem::CustomToolCallOutput { output, .. } => {
            let text = output.body.to_text().unwrap_or_default();
            let success = output.success.unwrap_or(true);
            (text, if success { 0 } else { 1 })
        }
        other => {
            (format!("{other:?}"), 0)
        }
    }
}
