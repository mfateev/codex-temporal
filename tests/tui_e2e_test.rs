//! End-to-end test for the Temporal TUI integration.
//!
//! Verifies that the `codex_tui::run_with_session` API surface works correctly
//! with a `TemporalAgentSession`.
//!
//! **Requires** `OPENAI_API_KEY` — tests will fail if the env var is absent.

use std::str::FromStr;
use std::time::Duration;

use codex_core::AgentSession;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::protocol::{
    AskForApproval, EventMsg, Op, SandboxPolicy,
};
use codex_protocol::user_input::UserInput;

use codex_temporal::activities::CodexActivities;
use codex_temporal::harness::CodexHarness;
use codex_temporal::session::TemporalAgentSession;
use codex_temporal::session_workflow::SessionWorkflow;
use codex_temporal::types::SessionWorkflowInput;
use codex_temporal::workflow::CodexWorkflow;

use temporalio_client::{
    Client, ClientOptions, Connection, ConnectionOptions,
};
use temporalio_common::telemetry::TelemetryOptions;
use temporalio_common::worker::WorkerTaskTypes;
use temporalio_sdk::{Worker, WorkerOptions};
use temporalio_sdk_core::ephemeral_server::{TemporalDevServerConfig, default_cached_download};
use temporalio_sdk_core::{CoreRuntime, RuntimeOptions, Url};

const TASK_QUEUE: &str = "codex-temporal";

/// Return the path to the temporal CLI binary that the SDK downloads to temp_dir.
fn temporal_cli() -> std::path::PathBuf {
    std::env::temp_dir().join("temporal-sdk-rust-0.1.0")
}

/// Kill **all** orphaned ephemeral Temporal server processes by walking `/proc`.
fn cleanup_all_leaked_servers() {
    let temporal_exe = temporal_cli();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return;
    };
    let mut killed = 0u32;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else { continue };
        if !name_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let exe_link = entry.path().join("exe");
        if let Ok(exe) = std::fs::read_link(&exe_link) {
            if exe == temporal_exe {
                let pid_str = name_str.to_string();
                let _ = std::process::Command::new("kill")
                    .args(["-9", &pid_str])
                    .output();
                killed += 1;
            }
        }
    }
    if killed > 0 {
        eprintln!("  [server] killed {killed} orphaned ephemeral Temporal server(s)");
    }
}

// ---------------------------------------------------------------------------
// Infrastructure
// ---------------------------------------------------------------------------

/// Build an `Op::UserTurn` with the given text message.
fn user_turn_op(text: &str) -> Op {
    Op::UserTurn {
        items: vec![UserInput::Text {
            text: text.to_string(),
            text_elements: vec![],
        }],
        cwd: std::env::current_dir().unwrap_or_else(|_| "/tmp".into()),
        approval_policy: AskForApproval::OnRequest,
        sandbox_policy: SandboxPolicy::DangerFullAccess,
        model: "gpt-4o".to_string(),
        effort: None,
        summary: Some(ReasoningSummary::Auto),
        service_tier: None,
        final_output_json_schema: None,
        collaboration_mode: None,
        personality: None,
    }
}

/// Create a `TemporalAgentSession` with a unique workflow ID.
fn new_session(client: &Client, model: &str) -> TemporalAgentSession {
    let workflow_id = format!("tui-e2e-{}", uuid::Uuid::new_v4());
    let base_input = SessionWorkflowInput {
        user_message: String::new(),
        model: model.to_string(),
        instructions: "You are a helpful coding assistant. Be concise.".to_string(),
        approval_policy: Default::default(),
        web_search_mode: None,
        reasoning_effort: None,
        reasoning_summary: ReasoningSummary::Auto,
        personality: None,
        developer_instructions: None,
        model_provider: None,
        crew_agents: Default::default(),
        continued_state: None,
    };
    TemporalAgentSession::new(client.clone(), workflow_id, base_input)
}

/// Drain events until `ShutdownComplete` or timeout.
async fn drain_shutdown(session: &TemporalAgentSession) {
    let _ = session.submit(Op::Shutdown).await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if tokio::time::Instant::now() > deadline {
            break;
        }
        match session.next_event().await {
            Ok(event) if matches!(event.msg, EventMsg::ShutdownComplete) => break,
            Err(_) => break,
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tui_session_smoke_test() {
    tokio::time::timeout(Duration::from_secs(20), tui_session_smoke_test_inner())
        .await
        .expect("tui_session_smoke_test timed out after 20s");
}

async fn tui_session_smoke_test_inner() {
    std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY must be set to run TUI E2E tests");
    cleanup_all_leaked_servers();

    // --- Start ephemeral Temporal server ---
    let server_result = tokio::time::timeout(Duration::from_secs(60), async {
        let config = TemporalDevServerConfig::builder()
            .exe(default_cached_download())
            .build();
        config.start_server().await
    })
    .await;

    let mut _server = match server_result {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => panic!("failed to start ephemeral server: {e}"),
        Err(_) => panic!("ephemeral server startup timed out (60s)"),
    };

    let server_target = _server.target.clone();

    // --- Client connection ---
    let conn_opts = ConnectionOptions::new(
        Url::from_str(&format!("http://{}", server_target)).expect("bad URL"),
    )
    .identity("tui-e2e-test-client")
    .build();
    let telemetry_options = TelemetryOptions::builder().build();
    let runtime_options = RuntimeOptions::builder()
        .telemetry_options(telemetry_options)
        .build()
        .expect("runtime options");
    let _runtime = CoreRuntime::new_assume_tokio(runtime_options).expect("runtime");

    let connection = tokio::time::timeout(
        Duration::from_secs(10),
        Connection::connect(conn_opts),
    )
    .await
    .expect("connection to ephemeral server timed out (10s)")
    .expect("failed to connect to ephemeral server");
    let client = Client::new(connection, ClientOptions::new("default").build())
        .expect("failed to create client");

    // --- Worker on dedicated thread (Worker future is !Send) ---
    let worker_target = server_target.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("worker tokio runtime");

        rt.block_on(async move {
            let tel = TelemetryOptions::builder().build();
            let rt_opts = RuntimeOptions::builder()
                .telemetry_options(tel)
                .build()
                .expect("worker runtime options");
            let worker_runtime =
                CoreRuntime::new_assume_tokio(rt_opts).expect("worker CoreRuntime");

            let conn = ConnectionOptions::new(
                Url::from_str(&format!("http://{}", worker_target)).expect("bad URL"),
            )
            .identity("tui-e2e-test-worker")
            .build();

            let connection = match tokio::time::timeout(
                Duration::from_secs(10),
                Connection::connect(conn),
            )
            .await
            {
                Ok(Ok(c)) => c,
                Ok(Err(e)) => {
                    let _ = ready_tx.send(Err(format!("worker connect failed: {e}")));
                    return;
                }
                Err(_) => {
                    let _ = ready_tx.send(Err("worker connect timed out (10s)".into()));
                    return;
                }
            };

            let worker_client =
                Client::new(connection, ClientOptions::new("default").build())
                    .expect("worker: failed to create client");

            let opts = WorkerOptions::new(TASK_QUEUE)
                .task_types(WorkerTaskTypes::all())
                .register_workflow::<SessionWorkflow>()
                .register_workflow::<CodexWorkflow>()
                .register_workflow::<CodexHarness>()
                .register_activities(CodexActivities::new())
                .build();
            let mut worker =
                Worker::new(&worker_runtime, worker_client, opts)
                    .expect("failed to create worker");

            let _ = ready_tx.send(Ok(()));

            if let Err(e) = worker.run().await {
                eprintln!("worker error: {e}");
            }
        });
    });

    match ready_rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("worker failed to start: {e}"),
        Err(_) => panic!("worker thread died before becoming ready"),
    }

    // --- Set up CODEX_HOME with danger-full-access ---
    let codex_home = std::env::temp_dir().join(format!(
        "codex-tui-e2e-home-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&codex_home).expect("failed to create CODEX_HOME");
    std::fs::write(
        codex_home.join("config.toml"),
        "sandbox_mode = \"danger-full-access\"\n",
    )
    .expect("failed to write config.toml");
    // SAFETY: This test runs sequentially.
    unsafe { std::env::set_var("CODEX_HOME", &codex_home) };

    // --- Create session and exercise the AgentSession trait ---
    let session = new_session(&client, "gpt-4o");

    // Submit a simple user turn.
    session
        .submit(user_turn_op("What is 2+2? Reply with just the number."))
        .await
        .expect("submit failed");

    // Collect events until TurnComplete.
    let mut saw_session_configured = false;
    #[allow(unused_assignments)]
    let mut saw_turn_complete = false;
    let timeout = tokio::time::Instant::now() + Duration::from_secs(120);

    loop {
        if tokio::time::Instant::now() > timeout {
            panic!("timed out waiting for TurnComplete");
        }
        let event = session.next_event().await.expect("next_event failed");
        match &event.msg {
            EventMsg::SessionConfigured(_) => saw_session_configured = true,
            EventMsg::TurnComplete(_) => {
                saw_turn_complete = true;
                break;
            }
            _ => {}
        }
    }

    assert!(saw_session_configured, "expected SessionConfigured event");
    assert!(saw_turn_complete, "expected TurnComplete event");

    // Shutdown.
    drain_shutdown(&session).await;
}
