//! Temporal activity implementations for the codex harness.
//!
//! Activities run outside the deterministic workflow sandbox — they can
//! perform real I/O (HTTP calls, shell commands, etc.).  Results are
//! recorded in the workflow history for deterministic replay.

use codex_protocol::models::ResponseItem;
use temporalio_macros::activities;
use temporalio_sdk::activities::{ActivityContext, ActivityError};

use crate::types::{ModelCallInput, ModelCallOutput, ToolExecInput, ToolExecOutput};

/// The OpenAI Responses API endpoint path.
const RESPONSES_PATH: &str = "responses";

/// Activity implementations for the codex workflow.
pub struct CodexActivities;

/// Non-streaming response body from the OpenAI Responses API.
#[derive(Debug, serde::Deserialize)]
struct ResponsesApiResponse {
    #[allow(dead_code)]
    id: String,
    output: Vec<ResponseItem>,
}

#[activities]
impl CodexActivities {
    /// Call the OpenAI Responses API (non-streaming) and return collected
    /// output items.
    ///
    /// Requires the `OPENAI_API_KEY` env var on the activity worker.
    /// Optionally reads `OPENAI_BASE_URL` (defaults to
    /// `https://api.openai.com/v1`).
    #[activity]
    pub async fn model_call(
        _ctx: ActivityContext,
        input: ModelCallInput,
    ) -> Result<ModelCallOutput, ActivityError> {
        let api_key = std::env::var("OPENAI_API_KEY")
            .map_err(|_| anyhow::anyhow!("OPENAI_API_KEY not set"))?;

        let base_url = std::env::var("OPENAI_BASE_URL")
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
        let base_url = base_url.trim_end_matches('/');
        let url = format!("{base_url}/{RESPONSES_PATH}");

        tracing::info!(
            model = %input.model,
            input_items = input.input.len(),
            tools = input.tools_json.len(),
            %url,
            "calling OpenAI Responses API"
        );

        let body = serde_json::json!({
            "model": input.model,
            "instructions": input.instructions,
            "input": input.input,
            "tools": input.tools_json,
            "tool_choice": "auto",
            "parallel_tool_calls": input.parallel_tool_calls,
            "store": false,
            "stream": false,
        });

        let client = reqwest::Client::new();
        let response = client
            .post(&url)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("HTTP request failed: {e}"))?;

        let status = response.status();
        if !status.is_success() {
            let error_body = response
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable>".to_string());
            return Err(anyhow::anyhow!(
                "OpenAI API returned {status}: {error_body}"
            ).into());
        }

        let api_response: ResponsesApiResponse = response
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("failed to parse response: {e}"))?;

        tracing::info!(
            output_items = api_response.output.len(),
            "model_call completed"
        );

        Ok(ModelCallOutput {
            items: api_response.output,
        })
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
