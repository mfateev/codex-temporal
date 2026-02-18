//! Temporal activity implementations for the codex harness.

use temporalio_macros::activities;
use temporalio_sdk::activities::{ActivityContext, ActivityError};

use crate::types::{ModelCallInput, ModelCallOutput, ToolExecInput, ToolExecOutput};

/// Activity implementations for the codex workflow.
pub struct CodexActivities;

#[activities]
impl CodexActivities {
    /// Call the OpenAI Responses API and return collected items.
    #[activity]
    pub async fn model_call(
        _ctx: ActivityContext,
        input: ModelCallInput,
    ) -> Result<ModelCallOutput, ActivityError> {
        tracing::info!(
            model = %input.model,
            input_items = input.input.len(),
            tools = input.tools_json.len(),
            "model_call activity invoked"
        );

        // TODO: Implement actual OpenAI Responses API call via reqwest.
        // For now return an empty response so the workflow loop terminates.
        Ok(ModelCallOutput { items: vec![] })
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

        // Run the command.
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
