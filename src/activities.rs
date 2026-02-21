//! Temporal activity implementations for the codex harness.
//!
//! Activities run outside the deterministic workflow sandbox — they can
//! perform real I/O (HTTP calls, shell commands, etc.).  Results are
//! recorded in the workflow history for deterministic replay.

use codex_core::{ModelClient, ModelProviderInfo, Prompt, ResponseEvent, built_in_model_providers};
use codex_otel::OtelManager;
use codex_protocol::models::{BaseInstructions, ResponseItem};
use codex_protocol::protocol::SessionSource;
use codex_protocol::ThreadId;
use futures::StreamExt;
use temporalio_macros::activities;
use temporalio_sdk::activities::{ActivityContext, ActivityError};

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

    /// Execute a shell command and return stdout/stderr/exit_code.
    #[activity]
    pub async fn tool_exec(
        ctx: ActivityContext,
        input: ToolExecInput,
    ) -> Result<ToolExecOutput, ActivityError> {
        tracing::info!(
            tool = %input.tool_name,
            call_id = %input.call_id,
            "tool_exec activity invoked"
        );

        // Parse the command from the arguments JSON.
        let args: serde_json::Value =
            serde_json::from_str(&input.arguments).unwrap_or_default();
        let command = args
            .get("command")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        if command.is_empty() {
            return Ok(ToolExecOutput {
                call_id: input.call_id,
                output: "error: no command provided".to_string(),
                exit_code: 1,
            });
        }

        let output = tokio::process::Command::new(&command[0])
            .args(&command[1..])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("failed to spawn process: {e}"))?;

        if ctx.is_cancelled() {
            return Err(ActivityError::cancelled());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = if stderr.is_empty() {
            stdout.to_string()
        } else {
            format!("{stdout}\n--- stderr ---\n{stderr}")
        };

        Ok(ToolExecOutput {
            call_id: input.call_id,
            output: combined,
            exit_code: output.status.code().unwrap_or(1),
        })
    }
}
