//! Temporal activity implementations for the codex harness.
//!
//! Activities run outside the deterministic workflow sandbox — they can
//! perform real I/O (HTTP calls, shell commands, etc.).  Results are
//! recorded in the workflow history for deterministic replay.

use std::path::PathBuf;
use std::sync::Arc;

use codex_core::config::ConfigBuilder;
use codex_core::models_manager::manager::ModelsManager;
use codex_core::{
    EventSink, ModelClient, ModelProviderInfo, Prompt, ResponseEvent, Session, StorageBackend,
    ToolInvocation, ToolPayload, TurnContext, TurnDiffTracker, ToolsConfig, ToolsConfigParams,
    build_specs, built_in_model_providers,
};
use codex_otel::OtelManager;
use codex_protocol::models::{BaseInstructions, ResponseItem};
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
    ModelCallInput, ModelCallOutput, ProjectContextOutput, ToolExecInput, ToolExecOutput,
};

/// Resolve the model provider to use for the activity.
///
/// Starts from the built-in OpenAI provider but overrides it to use API-key
/// auth (`OPENAI_API_KEY` env var) instead of ChatGPT OAuth — activities run
/// headless so there is no interactive login flow.
fn resolve_provider() -> ModelProviderInfo {
    let mut provider = built_in_model_providers()
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
    /// Persistent MCP server connections (initialized via `discover_mcp_tools`).
    mcp_manager: Arc<Mutex<HarnessMcpManager>>,
}

impl CodexActivities {
    /// Create a new `CodexActivities` with the provider resolved once.
    pub fn new() -> Self {
        Self {
            provider: resolve_provider(),
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
        _ctx: ActivityContext,
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
            conversation_id.clone(),
            provider,
            SessionSource::Exec,
            None,  // model_verbosity
            false, // websockets
            false, // websockets v2
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

        let otel_manager = OtelManager::new(
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

        tracing::info!(
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
                &otel_manager,
                input.effort,
                input.summary,
                None,
            )
            .await
            .map_err(|e| anyhow::anyhow!("model stream failed: {e}"))?;

        let mut items: Vec<ResponseItem> = Vec::new();
        let mut token_usage = None;
        while let Some(event) = stream.next().await {
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
                    return Err(anyhow::anyhow!("model stream error: {e}").into());
                }
            }
        }

        tracing::info!(
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
        _ctx: ActivityContext,
        input: ToolExecInput,
    ) -> Result<ToolExecOutput, ActivityError> {
        tracing::info!(
            tool = %input.tool_name,
            call_id = %input.call_id,
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
    #[activity]
    pub async fn load_config(
        _ctx: ActivityContext,
    ) -> Result<ConfigOutput, ActivityError> {
        tracing::info!("load_config activity invoked");

        let toml_string = ConfigBuilder::default()
            .build_toml_string()
            .await
            .map_err(|e| anyhow::anyhow!("config loading failed: {e}"))?;

        tracing::info!(
            toml_len = toml_string.len(),
            "config loaded successfully"
        );

        Ok(ConfigOutput { config_toml: toml_string })
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
        tracing::info!("discover_mcp_tools activity invoked");

        let cwd = std::path::PathBuf::from(&input.cwd);
        let config = config_from_toml(&input.config_toml, &cwd, None)
            .map_err(|e| anyhow::anyhow!("failed to build config from TOML: {e}"))?;

        let mcp_servers = config.mcp_servers.get().clone();
        if mcp_servers.is_empty() {
            tracing::info!("no MCP servers configured");
            return Ok(McpDiscoverOutput {
                tools: std::collections::HashMap::new(),
            });
        }

        tracing::info!(servers = mcp_servers.len(), "initializing MCP servers");

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

        tracing::info!("MCP discovery complete");
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
        tracing::info!(
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
                    });
                }
            }
        };

        let manager = self.mcp_manager.lock().await;
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

        Ok(McpToolCallOutput {
            call_id: input.call_id,
            result,
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

        tracing::info!(cwd = %cwd_str, "collecting project context");

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

        tracing::info!(
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
}

/// Core tool dispatch logic, independent of the Temporal `ActivityContext`.
///
/// Public so that integration tests can exercise the full `build_specs` →
/// `ToolRegistry::dispatch` pipeline without starting a Temporal worker.
pub async fn dispatch_tool(input: ToolExecInput) -> Result<ToolExecOutput, anyhow::Error> {
    use codex_core::config::Constrained;
    use codex_protocol::protocol::AskForApproval;

    let cwd = PathBuf::from(&input.cwd);

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
    // Approval is handled at the workflow level by TemporalToolHandler, so
    // the activity itself runs with no approval prompts. Sandbox policy comes
    // from the user's config.toml (or defaults to read-only).
    config.permissions.approval_policy = Constrained::allow_any(AskForApproval::Never);
    let config = Arc::new(config);

    // Resolve model info.
    let model_slug = ModelsManager::get_model_offline_for_tests(config.model.as_deref());
    let model_info =
        ModelsManager::construct_model_info_offline_for_tests(&model_slug, &config);

    // Build the tool registry with all standard tools enabled.
    let tools_config = ToolsConfig::new(&ToolsConfigParams {
        model_info: &model_info,
        features: &config.features,
        web_search_mode: None,
    });
    let builder = build_specs(&tools_config, None, None, &[]);
    let (_specs, registry) = builder.build();

    // Construct minimal Session + TurnContext for the dispatch.
    let conversation_id = ThreadId::new();
    let event_sink: Arc<dyn EventSink> = Arc::new(BufferEventSink::new());
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

    // Build the tool invocation.
    let invocation = ToolInvocation {
        session,
        turn: turn_context,
        tracker,
        call_id: input.call_id.clone(),
        tool_name: input.tool_name.clone(),
        payload: ToolPayload::Function {
            arguments: input.arguments.clone(),
        },
    };

    // Dispatch via the registry.
    match registry.dispatch(invocation).await {
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
            (output.clone(), 0)
        }
        other => {
            (format!("{other:?}"), 0)
        }
    }
}
