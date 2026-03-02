//! Temporal-backed TUI for the Codex agent.
//!
//! This binary reuses the full codex TUI (`App` + `Tui`) but connects to a
//! Temporal workflow instead of an in-process CodexThread via
//! [`codex_tui::run_with_session`].
//!
//! Usage:
//!   codex-temporal-tui "your prompt here"
//!   codex-temporal-tui --resume <session_id>
//!
//! Environment variables:
//!   TEMPORAL_ADDRESS  — Temporal server URL (default: http://localhost:7233)
//!   CODEX_MODEL       — Model name to display in the TUI header (default: gpt-4o)

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use codex_core::auth::AuthCredentialsStoreMode;
use codex_core::config::Config;
use codex_core::models_manager::collaboration_mode_presets::CollaborationModesConfig;
use codex_core::models_manager::manager::ModelsManager;
use codex_core::AuthManager;
use codex_protocol::config_types::{Personality, ReasoningSummary};
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::SessionConfiguredEvent;
use codex_protocol::ThreadId;

use codex_temporal::harness::{CodexHarness, CodexHarnessRun};
use codex_temporal::session::TemporalAgentSession;
use codex_temporal::types::{
    HarnessInput, SessionEntry, SessionStatus, SessionWorkflowInput,
};

use temporalio_client::{
    Client, ClientOptions, Connection, ConnectionOptions,
    WorkflowSignalOptions, WorkflowStartOptions,
};
use temporalio_common::protos::temporal::api::enums::v1::WorkflowIdConflictPolicy;
use temporalio_common::telemetry::TelemetryOptions;
use temporalio_sdk_core::{CoreRuntime, RuntimeOptions, Url};

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
    let options = WorkflowStartOptions::new("codex-temporal", &harness_id)
        .id_conflict_policy(WorkflowIdConflictPolicy::UseExisting)
        .build();
    client
        .start_workflow(CodexHarness::run, input, options)
        .await?;
    Ok(())
}

/// Register a new session with the harness.
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

/// Parse --resume flag from args.
///
/// Returns:
/// - `None` — no --resume flag (new session)
/// - `Some(id)` — --resume <session_id>
fn parse_resume_arg() -> (Option<String>, Option<String>) {
    let args: Vec<String> = std::env::args().collect();
    let mut resume = None;
    let mut prompt = None;

    let mut i = 1;
    while i < args.len() {
        if args[i] == "--resume" {
            if i + 1 < args.len() && !args[i + 1].starts_with('-') {
                resume = Some(args[i + 1].clone());
                i += 2;
            } else {
                eprintln!("--resume requires a session ID argument");
                std::process::exit(1);
            }
        } else if prompt.is_none() {
            prompt = Some(args[i].clone());
            i += 1;
        } else {
            i += 1;
        }
    }

    (resume, prompt)
}

/// Filter replayed events into the set suitable for `initial_messages`.
fn filter_initial_events(events: Vec<EventMsg>) -> Vec<EventMsg> {
    events
        .into_iter()
        .filter(|e| {
            matches!(
                e,
                EventMsg::AgentMessage(_)
                    | EventMsg::TurnStarted(_)
                    | EventMsg::TurnComplete(_)
                    | EventMsg::ExecApprovalRequest(_)
                    | EventMsg::ExecCommandBegin(_)
            )
        })
        .collect()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".parse().unwrap()),
        )
        .with_writer(std::io::stderr)
        .init();

    let (resume_arg, initial_prompt) = parse_resume_arg();

    let server_url = std::env::var("TEMPORAL_ADDRESS")
        .unwrap_or_else(|_| "http://localhost:7233".to_string());
    let model = std::env::var("CODEX_MODEL").unwrap_or_else(|_| "gpt-4o".to_string());
    let approval_policy = match std::env::var("CODEX_APPROVAL_POLICY")
        .unwrap_or_default()
        .as_str()
    {
        "never" => AskForApproval::Never,
        "untrusted" => AskForApproval::UnlessTrusted,
        "on-failure" => AskForApproval::OnFailure,
        _ => AskForApproval::OnRequest,
    };
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

    // --- Parse env-based config ---
    let web_search_mode = match std::env::var("CODEX_WEB_SEARCH")
        .unwrap_or_default()
        .as_str()
    {
        "live" => Some(codex_protocol::config_types::WebSearchMode::Live),
        "cached" => Some(codex_protocol::config_types::WebSearchMode::Cached),
        _ => None,
    };

    let reasoning_effort = match std::env::var("CODEX_EFFORT")
        .unwrap_or_default()
        .as_str()
    {
        "low" => Some(ReasoningEffort::Low),
        "medium" => Some(ReasoningEffort::Medium),
        "high" => Some(ReasoningEffort::High),
        _ => None,
    };

    let reasoning_summary = match std::env::var("CODEX_REASONING_SUMMARY")
        .unwrap_or_default()
        .as_str()
    {
        "concise" => ReasoningSummary::Concise,
        "detailed" => ReasoningSummary::Detailed,
        _ => ReasoningSummary::Auto,
    };

    let personality = match std::env::var("CODEX_PERSONALITY")
        .unwrap_or_default()
        .as_str()
    {
        "friendly" => Some(Personality::Friendly),
        "pragmatic" => Some(Personality::Pragmatic),
        _ => None,
    };

    let base_input = SessionWorkflowInput {
        user_message: String::new(),
        model: model.clone(),
        instructions: "You are a helpful coding assistant.".to_string(),
        approval_policy,
        web_search_mode,
        reasoning_effort,
        reasoning_summary,
        personality,
        developer_instructions: None,
        model_provider: None,
        crew_agents: Default::default(),
        continued_state: None,
    };

    // --- Ensure harness is running ---
    ensure_harness(&client).await?;

    // --- Build Config ---
    let codex_home = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".codex");

    // --- Determine session (new or resume) ---
    let (session, initial_messages) = if let Some(session_id) = resume_arg {
        tracing::info!(session_id = %session_id, "resuming session");
        let session = TemporalAgentSession::resume(client.clone(), session_id, base_input);

        // Fetch existing events to seed initial_messages.
        let events = session.fetch_initial_events().await?;
        let initial_messages = filter_initial_events(events);

        (session, Some(initial_messages))
    } else {
        // New session mode.
        let workflow_id = format!("codex-tui-{}", uuid::Uuid::new_v4());
        let session = TemporalAgentSession::new(client.clone(), workflow_id.clone(), base_input);

        // Register with harness.
        let now_millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let entry = SessionEntry {
            session_id: workflow_id,
            name: initial_prompt.clone(),
            model: model.clone(),
            created_at_millis: now_millis,
            status: SessionStatus::Running,
            crew_type: None,
        };
        if let Err(e) = register_with_harness(&client, entry).await {
            tracing::warn!(%e, "failed to register session with harness");
        }

        (session, None)
    };

    run_tui_session(
        session,
        model,
        approval_policy,
        cwd,
        reasoning_effort,
        codex_home,
        initial_prompt,
        initial_messages,
    )
    .await
}

/// Build the session-configured event, config, and run the main TUI.
#[allow(clippy::too_many_arguments)]
async fn run_tui_session(
    session: TemporalAgentSession,
    model: String,
    approval_policy: AskForApproval,
    cwd: PathBuf,
    reasoning_effort: Option<ReasoningEffort>,
    codex_home: PathBuf,
    initial_prompt: Option<String>,
    initial_messages: Option<Vec<EventMsg>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let session = Arc::new(session);

    // --- Build SessionConfiguredEvent ---
    let session_configured = SessionConfiguredEvent {
        session_id: ThreadId::new(),
        forked_from_id: None,
        thread_name: None,
        model: model.clone(),
        model_provider_id: "openai".to_string(),
        approval_policy,
        sandbox_policy: SandboxPolicy::DangerFullAccess,
        cwd: cwd.clone(),
        reasoning_effort,
        history_log_id: 0,
        history_entry_count: 0,
        initial_messages,
        network_proxy: None,
        rollout_path: None,
    };

    // --- Build Config ---
    let mut config = Config::for_harness(codex_home.clone())?;
    config.cwd = cwd;

    // --- Create real AuthManager and ModelsManager ---
    let auth_manager = AuthManager::shared(
        codex_home.clone(),
        false,
        AuthCredentialsStoreMode::default(),
    );
    let models_manager = Arc::new(ModelsManager::new(
        codex_home,
        auth_manager.clone(),
        None,
        CollaborationModesConfig {
            default_mode_request_user_input: false,
        },
    ));

    // --- Run TUI ---
    codex_tui::run_with_session(
        session,
        session_configured,
        config,
        auth_manager,
        models_manager,
        model,
        initial_prompt,
    )
    .await?;

    Ok(())
}
