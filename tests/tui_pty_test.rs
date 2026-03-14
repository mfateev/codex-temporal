//! PTY-based end-to-end tests for the `codex-temporal-tui` binary.
//!
//! These tests spawn the actual TUI binary attached to a pseudo-terminal,
//! following the same pattern as codex-rs/tui/tests/suite/ (PTY spawn,
//! cursor-position reply, Ctrl+C shutdown).
//!
//! An ephemeral Temporal server and in-process worker are started first,
//! then the TUI binary is spawned via PTY with `TEMPORAL_ADDRESS` pointing
//! at the ephemeral server.
//!
//! **Requires** `OPENAI_API_KEY` — tests will fail if the env var is absent.

use std::collections::HashMap;
use std::time::Duration;

use tokio::select;
use tokio::time::timeout;

use codex_temporal::activities::CodexActivities;
use codex_temporal::harness::CodexHarness;
use codex_temporal::session_workflow::SessionWorkflow;
use codex_temporal::workflow::CodexWorkflow;

use temporalio_client::{Client, ClientOptions, Connection, ConnectionOptions};
use temporalio_common::telemetry::TelemetryOptions;
use temporalio_common::worker::WorkerTaskTypes;
use temporalio_sdk::{Worker, WorkerOptions};
use temporalio_sdk_core::ephemeral_server::{TemporalDevServerConfig, default_cached_download};
use temporalio_sdk_core::{CoreRuntime, RuntimeOptions, Url};

const TASK_QUEUE: &str = "codex-temporal";

// ---------------------------------------------------------------------------
// Infrastructure
// ---------------------------------------------------------------------------

/// Result of running the TUI binary via PTY.
struct TuiOutput {
    exit_code: i32,
    output: String,
}

/// Start an ephemeral Temporal dev server, returning its `host:port` target.
async fn start_ephemeral_server(
) -> temporalio_sdk_core::ephemeral_server::EphemeralServer {
    let server_result = timeout(Duration::from_secs(60), async {
        let config = TemporalDevServerConfig::builder()
            .exe(default_cached_download())
            .build();
        config.start_server().await
    })
    .await;

    match server_result {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => panic!("failed to start ephemeral server: {e}"),
        Err(_) => panic!("ephemeral server startup timed out (60s)"),
    }
}

/// Spawn a worker on a dedicated thread (Worker future is !Send).
///
/// Returns after the worker signals readiness.
fn spawn_worker(server_target: &str) {
    let worker_target = server_target.to_string();
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
                Url::parse(&format!("http://{}", worker_target)).expect("bad URL"),
            )
            .identity("tui-pty-test-worker")
            .build();

            let connection = match timeout(
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
}

/// Spawn the `codex-temporal-tui` binary via PTY with the given environment.
///
/// Follows the codex pattern from `codex-rs/tui/tests/suite/no_panic_on_startup.rs`:
/// - Spawns via `codex_utils_pty::spawn_pty_process`
/// - Replies to cursor-position queries (ESC[6n → ESC[1;1R)
/// - Collects all PTY output
/// - Sends Ctrl+C after a delay to trigger shutdown
/// - Returns exit code and collected output
async fn run_tui_binary(
    env: &HashMap<String, String>,
    args: &[String],
    ctrl_c_delay: Duration,
    overall_timeout: Duration,
) -> anyhow::Result<TuiOutput> {
    let tui_bin = env!("CARGO_BIN_EXE_codex-temporal-tui");
    let cwd = std::env::current_dir().unwrap_or_else(|_| "/tmp".into());

    let spawned = codex_utils_pty::spawn_pty_process(
        tui_bin,
        args,
        &cwd,
        env,
        &None,
        codex_utils_pty::TerminalSize { rows: 24, cols: 80 },
    )
    .await?;

    let codex_utils_pty::SpawnedProcess {
        session,
        stdout_rx,
        stderr_rx,
        exit_rx,
    } = spawned;

    let mut output_rx = codex_utils_pty::combine_output_receivers(stdout_rx, stderr_rx);
    let mut exit_rx = exit_rx;
    let writer_tx = session.writer_sender();
    let mut output = Vec::new();

    // Schedule Ctrl+C after a delay (same pattern as codex's
    // model_availability_nux.rs — sends multiple Ctrl+C with spacing).
    let interrupt_writer = writer_tx.clone();
    let interrupt_task = tokio::spawn(async move {
        tokio::time::sleep(ctrl_c_delay).await;
        for _ in 0..4 {
            let _ = interrupt_writer.send(vec![3]).await; // Ctrl+C = 0x03
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });

    let exit_code_result = timeout(overall_timeout, async {
        loop {
            select! {
                result = output_rx.recv() => match result {
                    Ok(chunk) => {
                        // Reply to cursor-position queries so the TUI can
                        // initialize without a real terminal.
                        if chunk.windows(4).any(|window| window == b"\x1b[6n") {
                            let _ = writer_tx.send(b"\x1b[1;1R".to_vec()).await;
                        }
                        output.extend_from_slice(&chunk);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        break exit_rx.await
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                },
                result = &mut exit_rx => break result,
            }
        }
    })
    .await;

    interrupt_task.abort();

    let exit_code = match exit_code_result {
        Ok(Ok(code)) => code,
        Ok(Err(err)) => return Err(err.into()),
        Err(_) => {
            session.terminate();
            anyhow::bail!("timed out waiting for codex-temporal-tui to exit");
        }
    };

    // Drain any output that raced with the exit notification.
    while let Ok(chunk) = output_rx.try_recv() {
        output.extend_from_slice(&chunk);
    }

    let output = String::from_utf8_lossy(&output).to_string();
    Ok(TuiOutput { exit_code, output })
}

/// Build the common environment map for the TUI binary.
fn tui_env(server_target: &str, codex_home: &std::path::Path) -> HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert(
        "TEMPORAL_ADDRESS".to_string(),
        format!("http://{}", server_target),
    );
    env.insert(
        "OPENAI_API_KEY".to_string(),
        std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY must be set"),
    );
    env.insert(
        "CODEX_HOME".to_string(),
        codex_home.display().to_string(),
    );
    // Suppress analytics/telemetry in tests.
    env.insert("CODEX_DISABLE_ANALYTICS".to_string(), "1".to_string());
    env
}

/// Create a temp CODEX_HOME directory with a config.toml that grants
/// full sandbox access (needed for tool execution in tests).
fn setup_codex_home() -> tempfile::TempDir {
    let codex_home = tempfile::tempdir().expect("failed to create temp dir");
    std::fs::write(
        codex_home.path().join("config.toml"),
        "sandbox_mode = \"danger-full-access\"\n",
    )
    .expect("failed to write config.toml");
    codex_home
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Verify that the `codex-temporal-tui` binary starts up, renders its TUI,
/// and exits cleanly when interrupted with Ctrl+C.
///
/// This is the equivalent of codex's `no_panic_on_startup` test — it confirms
/// the full binary lifecycle (connect to Temporal, initialize TUI, shutdown)
/// works end-to-end through a real pseudo-terminal.
#[tokio::test]
async fn tui_startup_and_shutdown() {
    match timeout(
        Duration::from_secs(300),
        tui_startup_and_shutdown_inner(),
    )
    .await
    {
        Ok(()) => {}
        Err(_) => panic!("tui_startup_and_shutdown timed out after 300s"),
    }
}

async fn tui_startup_and_shutdown_inner() {
    std::env::var("OPENAI_API_KEY")
        .expect("OPENAI_API_KEY must be set to run PTY TUI tests");

    // --- Start ephemeral Temporal server + worker ---
    let mut _server = start_ephemeral_server().await;
    let server_target = _server.target.clone();
    spawn_worker(&server_target);

    // --- Set up CODEX_HOME ---
    let codex_home = setup_codex_home();
    let env = tui_env(&server_target, codex_home.path());

    // --- Spawn TUI via PTY (no prompt — just start and Ctrl+C) ---
    // Give the TUI 5 seconds to start up before sending Ctrl+C,
    // then allow 30 seconds total for the process to exit.
    let result = run_tui_binary(
        &env,
        &[], // no arguments — start in interactive mode
        Duration::from_secs(5),
        Duration::from_secs(30),
    )
    .await
    .expect("failed to run TUI binary");

    // Exit code 0 (clean) or 130 (SIGINT) are both acceptable.
    assert!(
        result.exit_code == 0 || result.exit_code == 130,
        "unexpected exit code {}: output:\n{}",
        result.exit_code,
        result.output,
    );

    // The TUI should have produced some terminal output (ANSI escape sequences,
    // ratatui rendering, etc.) — a completely empty output indicates the binary
    // failed to start or render anything.
    assert!(
        !result.output.is_empty(),
        "TUI produced no output — binary may have failed silently",
    );
}
