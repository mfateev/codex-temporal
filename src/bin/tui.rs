//! Temporal-backed TUI for the Codex agent.
//!
//! This binary reuses the full codex TUI (`App` + `Tui`) but connects to a
//! Temporal workflow instead of an in-process CodexThread via
//! [`codex_tui::run_with_session`].
//!
//! Usage:
//!   codex-temporal-tui "your prompt here"
//!
//! Environment variables:
//!   TEMPORAL_ADDRESS  — Temporal server URL (default: http://localhost:7233)
//!   CODEX_MODEL       — Model name to display in the TUI header (default: gpt-4o)

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use codex_core::config::Config;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::SessionConfiguredEvent;
use codex_protocol::ThreadId;

use codex_temporal::auth_stub::NoopAuthProvider;
use codex_temporal::models_stub::FixedModelsProvider;
use codex_temporal::session::TemporalAgentSession;
use codex_temporal::types::CodexWorkflowInput;

use temporalio_client::{Client, ClientOptions, Connection, ConnectionOptions};
use temporalio_common::telemetry::TelemetryOptions;
use temporalio_sdk_core::{CoreRuntime, RuntimeOptions, Url};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".parse().unwrap()),
        )
        .with_writer(std::io::stderr)
        .init();

    let initial_prompt = std::env::args().nth(1);

    let server_url = std::env::var("TEMPORAL_ADDRESS")
        .unwrap_or_else(|_| "http://localhost:7233".to_string());
    let model = std::env::var("CODEX_MODEL").unwrap_or_else(|_| "gpt-4o".to_string());
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    // --- Connect to Temporal ---
    let connection_options = ConnectionOptions::new(Url::from_str(&server_url)?)
        .identity("codex-temporal-tui")
        .build();
    let telemetry_options = TelemetryOptions::builder().build();
    let runtime_options = RuntimeOptions::builder()
        .telemetry_options(telemetry_options)
        .build()?;
    let _runtime = CoreRuntime::new_assume_tokio(runtime_options)?;
    let connection = Connection::connect(connection_options).await?;
    let client = Client::new(connection, ClientOptions::new("default").build())?;

    // --- Create TemporalAgentSession ---
    let workflow_id = format!("codex-tui-{}", uuid::Uuid::new_v4());
    let base_input = CodexWorkflowInput {
        user_message: String::new(),
        model: model.clone(),
        instructions: "You are a helpful coding assistant.".to_string(),
    };
    let session = Arc::new(TemporalAgentSession::new(
        client,
        workflow_id,
        base_input,
    ));

    // --- Build SessionConfiguredEvent ---
    let session_configured = SessionConfiguredEvent {
        session_id: ThreadId::new(),
        forked_from_id: None,
        thread_name: None,
        model: model.clone(),
        model_provider_id: "openai".to_string(),
        approval_policy: AskForApproval::OnFailure,
        sandbox_policy: SandboxPolicy::DangerFullAccess,
        cwd: cwd.clone(),
        reasoning_effort: None,
        history_log_id: 0,
        history_entry_count: 0,
        initial_messages: None,
        network_proxy: None,
        rollout_path: None,
    };

    // --- Build Config ---
    let codex_home = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".codex");
    let mut config = Config::for_harness(codex_home)?;
    config.cwd = cwd;

    // --- Stub providers ---
    let auth_provider: Arc<dyn codex_core::AuthProvider> = Arc::new(NoopAuthProvider);
    let models_provider: Arc<dyn codex_core::ModelsProvider> =
        Arc::new(FixedModelsProvider::new(model.clone()));

    // --- Run TUI ---
    codex_tui::run_with_session(
        session,
        session_configured,
        config,
        auth_provider,
        models_provider,
        model,
        initial_prompt,
    )
    .await?;

    Ok(())
}
