//! Temporal activity implementations for the codex harness.
//!
//! Activities run outside the deterministic workflow sandbox — they can
//! perform real I/O (HTTP calls, shell commands, etc.).  Results are
//! recorded in the workflow history for deterministic replay.

use std::path::PathBuf;
use std::sync::Arc;

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

use crate::sink::BufferEventSink;
use crate::storage::InMemoryStorage;
use crate::types::{ModelCallInput, ModelCallOutput, ToolExecInput, ToolExecOutput};

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
pub struct CodexActivities;

#[activities]
impl CodexActivities {
    /// Call the model API using the full codex client stack (provider
    /// resolution, retries, auth refresh, etc.) and return collected output
    /// items.
    #[activity]
    pub async fn model_call(
        _ctx: ActivityContext,
        input: ModelCallInput,
    ) -> Result<ModelCallOutput, ActivityError> {
        let provider = resolve_provider();
        let conversation_id = ThreadId::new();

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
        while let Some(event) = stream.next().await {
            match event {
                Ok(ResponseEvent::OutputItemDone(item)) => {
                    items.push(item);
                }
                Ok(ResponseEvent::Completed { .. }) => break,
                Ok(_) => {} // Created, Delta, etc.
                Err(e) => {
                    return Err(anyhow::anyhow!("model stream error: {e}").into());
                }
            }
        }

        tracing::info!(output_items = items.len(), "model_call completed");

        Ok(ModelCallOutput { items })
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
}

/// Core tool dispatch logic, independent of the Temporal `ActivityContext`.
///
/// Public so that integration tests can exercise the full `build_specs` →
/// `ToolRegistry::dispatch` pipeline without starting a Temporal worker.
pub async fn dispatch_tool(input: ToolExecInput) -> Result<ToolExecOutput, anyhow::Error> {
    use codex_core::config::Constrained;
    use codex_protocol::protocol::{AskForApproval, SandboxPolicy};

    // Build a Config with the right model and cwd.
    let cwd = PathBuf::from(&input.cwd);
    let codex_home = PathBuf::from("/tmp/codex-temporal");
    let mut config = codex_core::config::Config::for_harness(codex_home)
        .map_err(|e| anyhow::anyhow!("failed to build config: {e}"))?;
    config.model = Some(input.model.clone());
    config.cwd = cwd;
    // Approval is handled at the workflow level by TemporalToolHandler, so
    // the activity itself runs with full access and no approval prompts.
    config.permissions.sandbox_policy = Constrained::allow_any(SandboxPolicy::DangerFullAccess);
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
