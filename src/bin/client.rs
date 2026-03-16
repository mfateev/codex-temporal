//! CLI client to start a codex-temporal workflow.
//!
//! Usage:
//!   codex-temporal-client "your prompt here"   → start new session
//!   codex-temporal-client list                 → list sessions from harness
//!   codex-temporal-client crews                → list available crew types
//!   codex-temporal-client start-crew <name> [--input key=value]...

use std::collections::BTreeMap;
use std::str::FromStr;

use temporalio_client::{
    Client, ClientOptions, Connection, ConnectionOptions,
    WorkflowQueryOptions, WorkflowSignalOptions, WorkflowStartOptions,
};
use temporalio_common::protos::temporal::api::enums::v1::WorkflowIdConflictPolicy;
use temporalio_common::telemetry::TelemetryOptions;
use temporalio_sdk_core::{CoreRuntime, RuntimeOptions, Url};

use codex_temporal::config_loader;
use codex_temporal::harness::{CodexHarness, CodexHarnessRun};
use codex_temporal::session_workflow::SessionWorkflow;
use codex_temporal::types::{
    CrewMode, HarnessInput, SessionEntry, SessionStatus,
};

const TASK_QUEUE: &str = "codex-temporal";

/// Derive the harness workflow ID for the current user.
fn harness_workflow_id() -> String {
    let user = std::env::var("USER").unwrap_or_else(|_| "default".to_string());
    format!("codex-harness-{user}")
}

/// Ensure the harness workflow is running (start if not).
async fn ensure_harness(client: &Client) -> Result<(), Box<dyn std::error::Error>> {
    let harness_id = harness_workflow_id();
    let input = HarnessInput {
        continued_state: None,
    };
    let options = WorkflowStartOptions::new(TASK_QUEUE, &harness_id)
        .id_conflict_policy(WorkflowIdConflictPolicy::UseExisting)
        .build();
    client
        .start_workflow(CodexHarness::run, input, options)
        .await?;
    Ok(())
}

/// Query the harness for sessions and print them.
async fn list_sessions(client: &Client) -> Result<(), Box<dyn std::error::Error>> {
    ensure_harness(client).await?;

    let harness_id = harness_workflow_id();
    let handle = client.get_workflow_handle::<CodexHarnessRun>(&harness_id);
    let json: String = handle
        .query(
            CodexHarness::list_sessions,
            (),
            WorkflowQueryOptions::default(),
        )
        .await?;

    let sessions: Vec<SessionEntry> = serde_json::from_str(&json).unwrap_or_default();
    if sessions.is_empty() {
        println!("No sessions found.");
        return Ok(());
    }

    println!(
        "{:<40} {:<12} {:<16} STATUS",
        "SESSION ID", "MODEL", "CREATED"
    );
    for s in &sessions {
        let status = match s.status {
            SessionStatus::Running => "Running",
            SessionStatus::Completed => "Completed",
            SessionStatus::Failed => "Failed",
        };
        let created = format_millis_ago(s.created_at_millis);
        println!(
            "{:<40} {:<12} {:<16} {}",
            s.session_id, s.model, created, status
        );
    }

    Ok(())
}

/// Format a Unix-millis timestamp as a human-readable "N ago" string.
fn format_millis_ago(millis: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    if millis == 0 || millis > now {
        return "just now".to_string();
    }
    let diff_secs = (now - millis) / 1000;
    if diff_secs < 60 {
        format!("{diff_secs}s ago")
    } else if diff_secs < 3600 {
        format!("{}m ago", diff_secs / 60)
    } else {
        format!("{}h ago", diff_secs / 3600)
    }
}

/// Register a session with the harness.
async fn register_with_harness(
    client: &Client,
    entry: SessionEntry,
) -> Result<(), Box<dyn std::error::Error>> {
    let harness_id = harness_workflow_id();
    let handle = client.get_workflow_handle::<CodexHarnessRun>(&harness_id);
    handle
        .signal(
            CodexHarness::register_session,
            entry,
            WorkflowSignalOptions::default(),
        )
        .await?;
    Ok(())
}

/// List available crew types (local only, no Temporal connection needed).
fn list_crew_types() -> Result<(), Box<dyn std::error::Error>> {
    let crews = config_loader::discover_crew_types()?;
    if crews.is_empty() {
        println!("No crew types found. Place TOML files in {{CODEX_HOME}}/crews/.");
        return Ok(());
    }

    println!(
        "{:<24} {:<14} {:<40} REQUIRED INPUTS",
        "NAME", "MODE", "DESCRIPTION"
    );
    for crew in &crews {
        let mode = match crew.mode {
            CrewMode::Interactive => "interactive",
            CrewMode::Autonomous => "autonomous",
        };
        let required: Vec<&String> = crew
            .inputs
            .iter()
            .filter(|(_, spec)| spec.required && spec.default.is_none())
            .map(|(name, _)| name)
            .collect();
        let inputs_str = if required.is_empty() {
            "(none)".to_string()
        } else {
            required.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        };
        println!(
            "{:<24} {:<14} {:<40} {}",
            crew.name, mode, crew.description, inputs_str
        );
    }

    Ok(())
}

/// Parse `--input key=value` pairs from CLI arguments.
fn parse_input_args(args: &[String]) -> BTreeMap<String, String> {
    let mut inputs = BTreeMap::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--input"
            && let Some(kv) = args.get(i + 1)
        {
            if let Some((k, v)) = kv.split_once('=') {
                inputs.insert(k.to_string(), v.to_string());
            }
            i += 2;
            continue;
        }
        i += 1;
    }
    inputs
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".parse().unwrap()),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    let first_arg = args.get(1).map(|s| s.as_str());

    // --- crews subcommand (no Temporal connection needed) ---
    if first_arg == Some("crews") {
        return list_crew_types();
    }

    // --- start-crew subcommand ---
    let is_start_crew = first_arg == Some("start-crew");
    let is_list = first_arg == Some("list");

    let server_url = std::env::var("TEMPORAL_ADDRESS")
        .unwrap_or_else(|_| "http://localhost:7233".to_string());

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

    if is_list {
        return list_sessions(&client).await;
    }

    if is_start_crew {
        let crew_name = args.get(2).ok_or("usage: start-crew <name> [--input key=value]...")?;
        let crew = config_loader::load_crew_type(crew_name)?;
        let crew_inputs = parse_input_args(&args[3..]);

        // Load base config and apply env overrides.
        let harness_config = config_loader::load_harness_config().await?;
        let mut input = harness_config.base_input;
        config_loader::apply_env_overrides(&mut input);
        input.model_provider = Some(harness_config.model_provider);

        // Apply crew type overrides.
        config_loader::apply_crew_type(&crew, &crew_inputs, &mut input)?;

        let model = input.model.clone();
        let workflow_id = format!("codex-session-{}", uuid::Uuid::new_v4());
        tracing::info!(workflow_id = %workflow_id, crew = %crew_name, "starting crew workflow");

        let options = WorkflowStartOptions::new(TASK_QUEUE, &workflow_id).build();
        let handle = client
            .start_workflow(SessionWorkflow::run, input, options)
            .await?;

        tracing::info!(
            workflow_id = %workflow_id,
            run_id = ?handle.run_id(),
            crew = %crew_name,
            "crew workflow started — use Temporal UI to monitor"
        );

        // Register with harness (best-effort).
        if ensure_harness(&client).await.is_ok() {
            let now_millis = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            let entry = SessionEntry {
                session_id: workflow_id.clone(),
                name: Some(format!("[{crew_name}]")),
                model,
                created_at_millis: now_millis,
                status: SessionStatus::Running,
                crew_type: Some(crew_name.clone()),
            };
            if let Err(e) = register_with_harness(&client, entry).await {
                tracing::warn!(%e, "failed to register session with harness");
            }
        }

        return Ok(());
    }

    // --- start new session (default) ---
    let user_message = first_arg
        .map(|s| s.to_string())
        .unwrap_or_else(|| "Hello, Codex!".to_string());

    tracing::info!(server_url = %server_url, user_message = %user_message, "starting codex workflow");

    // Load config.toml and apply env-var overrides.
    let harness_config = config_loader::load_harness_config().await?;
    let mut input = harness_config.base_input;
    config_loader::apply_env_overrides(&mut input);
    input.user_message = user_message.clone();
    input.model_provider = Some(harness_config.model_provider);
    let model = input.model.clone();

    let workflow_id = format!("codex-session-{}", uuid::Uuid::new_v4());
    tracing::info!(workflow_id = %workflow_id, "starting workflow");

    let options = WorkflowStartOptions::new(TASK_QUEUE, &workflow_id).build();

    // Start the SessionWorkflow (which spawns the main AgentWorkflow child).
    let handle = client
        .start_workflow(SessionWorkflow::run, input, options)
        .await?;

    tracing::info!(
        workflow_id = %workflow_id,
        run_id = ?handle.run_id(),
        "workflow started — use Temporal UI to monitor"
    );

    // Register with harness (best-effort).
    if ensure_harness(&client).await.is_ok() {
        let now_millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let entry = SessionEntry {
            session_id: workflow_id.clone(),
            name: Some(user_message),
            model,
            created_at_millis: now_millis,
            status: SessionStatus::Running,
            crew_type: None,
        };
        if let Err(e) = register_with_harness(&client, entry).await {
            tracing::warn!(%e, "failed to register session with harness");
        }
    }

    Ok(())
}
