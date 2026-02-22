//! CLI client to start a codex-temporal workflow.
//!
//! Usage:
//!   codex-temporal-client "your prompt here"

use std::str::FromStr;

use temporalio_client::{Client, ClientOptions, Connection, ConnectionOptions, WorkflowStartOptions};
use temporalio_common::telemetry::TelemetryOptions;
use temporalio_sdk_core::{CoreRuntime, RuntimeOptions, Url};

use codex_protocol::config_types::WebSearchMode;
use codex_protocol::protocol::AskForApproval;
use codex_temporal::types::CodexWorkflowInput;
use codex_temporal::workflow::CodexWorkflow;

const TASK_QUEUE: &str = "codex-temporal";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".parse().unwrap()),
        )
        .init();

    let user_message = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "Hello, Codex!".to_string());

    let server_url = std::env::var("TEMPORAL_ADDRESS")
        .unwrap_or_else(|_| "http://localhost:7233".to_string());

    tracing::info!(server_url = %server_url, user_message = %user_message, "starting codex workflow");

    // Connect to the Temporal server.
    let connection_options = ConnectionOptions::new(
        Url::from_str(&server_url)?,
    )
    .identity("codex-temporal-client")
    .build();
    let telemetry_options = TelemetryOptions::builder().build();
    let runtime_options = RuntimeOptions::builder()
        .telemetry_options(telemetry_options)
        .build()?;
    let _runtime = CoreRuntime::new_assume_tokio(runtime_options)?;

    let connection = Connection::connect(connection_options).await?;
    let client = Client::new(
        connection,
        ClientOptions::new("default").build(),
    )?;

    let approval_policy = match std::env::var("CODEX_APPROVAL_POLICY")
        .unwrap_or_default()
        .as_str()
    {
        "never" => AskForApproval::Never,
        "untrusted" => AskForApproval::UnlessTrusted,
        "on-failure" => AskForApproval::OnFailure,
        _ => AskForApproval::OnRequest,
    };

    let web_search_mode = match std::env::var("CODEX_WEB_SEARCH")
        .unwrap_or_default()
        .as_str()
    {
        "live" => Some(WebSearchMode::Live),
        "cached" => Some(WebSearchMode::Cached),
        "disabled" => None,
        _ => None,
    };

    let input = CodexWorkflowInput {
        user_message: user_message.clone(),
        model: std::env::var("CODEX_MODEL").unwrap_or_else(|_| "gpt-4o".to_string()),
        instructions: "You are a helpful coding assistant.".to_string(),
        approval_policy,
        web_search_mode,
    };

    let workflow_id = format!("codex-{}", uuid::Uuid::new_v4());
    tracing::info!(workflow_id = %workflow_id, "starting workflow");

    let options = WorkflowStartOptions::new(TASK_QUEUE, &workflow_id).build();

    // Start the workflow using the CodexWorkflow's run definition.
    let handle = client
        .start_workflow(CodexWorkflow::run, input, options)
        .await?;

    tracing::info!(
        workflow_id = %workflow_id,
        run_id = ?handle.run_id(),
        "workflow started — use Temporal UI to monitor"
    );

    Ok(())
}
