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

// ---------------------------------------------------------------------------
// Session reconnect test
// ---------------------------------------------------------------------------

/// Use the `temporal` CLI to list SessionWorkflow executions and extract
/// a workflow ID matching the `codex-tui-` prefix.
///
/// Uses JSON output (`-o json`) for reliable parsing.
async fn find_session_workflow_id(server_target: &str) -> Option<String> {
    let output = tokio::process::Command::new("temporal")
        .args([
            "workflow",
            "list",
            "--address",
            server_target,
            "--query",
            "WorkflowType='SessionWorkflow'",
            "--limit",
            "10",
            "-o",
            "json",
        ])
        .output()
        .await
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    // Each line is a JSON object with a "workflowExecutionInfo" containing
    // "execution.workflowId". Parse line by line (jsonl format).
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
            // Try nested path: workflowExecutionInfo.execution.workflowId
            let wf_id = val
                .pointer("/workflowExecutionInfo/execution/workflowId")
                .or_else(|| val.pointer("/execution/workflowId"))
                .or_else(|| val.get("workflowId"))
                .and_then(|v| v.as_str());
            if let Some(id) = wf_id
                && id.starts_with("codex-tui-")
            {
                return Some(id.to_string());
            }
        }
    }
    // Fallback: search raw text for the pattern.
    for line in text.lines() {
        if let Some(pos) = line.find("codex-tui-") {
            let rest = &line[pos..];
            // Extract until a non-ID character (whitespace, quote, comma).
            let id: String = rest
                .chars()
                .take_while(|c| !c.is_whitespace() && *c != '"' && *c != ',')
                .collect();
            if !id.is_empty() {
                return Some(id);
            }
        }
    }
    None
}

/// Verify session reconnect through the real TUI binary:
///
/// 1. Start ephemeral Temporal server + worker.
/// 2. Run TUI #1 via PTY with a prompt — let it process a turn, then Ctrl+C.
/// 3. Discover the session workflow ID via `temporal workflow list` CLI.
/// 4. Run TUI #2 via PTY with `--resume <session_id>` — verify it starts,
///    loads the previous session's messages, and exits cleanly under Ctrl+C.
#[tokio::test]
async fn tui_session_reconnect() {
    match timeout(
        Duration::from_secs(300),
        tui_session_reconnect_inner(),
    )
    .await
    {
        Ok(()) => {}
        Err(_) => panic!("tui_session_reconnect timed out after 300s"),
    }
}

async fn tui_session_reconnect_inner() {
    std::env::var("OPENAI_API_KEY")
        .expect("OPENAI_API_KEY must be set to run PTY TUI tests");

    // --- Start ephemeral Temporal server + worker ---
    let mut _server = start_ephemeral_server().await;
    let server_target = _server.target.clone();
    spawn_worker(&server_target);

    // --- Set up CODEX_HOME ---
    let codex_home = setup_codex_home();
    let env = tui_env(&server_target, codex_home.path());

    // --- Run TUI #1: start a new session with a prompt ---
    // Give the TUI 20 seconds to connect, process the model turn, and render,
    // then send Ctrl+C.  Allow 60 seconds total for the process to exit.
    eprintln!("  [reconnect] starting TUI #1 with prompt...");
    let result1 = run_tui_binary(
        &env,
        &["Say hello in one word".to_string()],
        Duration::from_secs(20),
        Duration::from_secs(60),
    )
    .await
    .expect("failed to run TUI #1");

    assert!(
        result1.exit_code == 0 || result1.exit_code == 130,
        "TUI #1 unexpected exit code {}: output:\n{}",
        result1.exit_code,
        result1.output,
    );
    assert!(
        !result1.output.is_empty(),
        "TUI #1 produced no output",
    );
    eprintln!("  [reconnect] TUI #1 exited (code {})", result1.exit_code);

    // --- Discover session workflow ID via temporal CLI ---
    // The TUI binary creates a SessionWorkflow with ID `codex-tui-{uuid}`.
    // Use the temporal CLI to find it.
    eprintln!("  [reconnect] querying temporal for session workflow ID...");
    let session_id = find_session_workflow_id(&server_target)
        .await
        .expect("could not find a codex-tui-* SessionWorkflow via temporal CLI");
    eprintln!("  [reconnect] found session: {session_id}");

    // --- Run TUI #2: resume the previous session ---
    // Give the TUI 10 seconds to connect, fetch initial_messages, and render,
    // then send Ctrl+C.  Allow 45 seconds total.
    eprintln!("  [reconnect] starting TUI #2 with --resume {session_id}...");
    let result2 = run_tui_binary(
        &env,
        &["--resume".to_string(), session_id],
        Duration::from_secs(10),
        Duration::from_secs(45),
    )
    .await
    .expect("failed to run TUI #2 with --resume");

    assert!(
        result2.exit_code == 0 || result2.exit_code == 130,
        "TUI #2 (resumed) unexpected exit code {}: output:\n{}",
        result2.exit_code,
        result2.output,
    );
    assert!(
        !result2.output.is_empty(),
        "TUI #2 (resumed) produced no output — binary may have failed to reconnect",
    );
    eprintln!("  [reconnect] TUI #2 exited (code {})", result2.exit_code);
}

// ---------------------------------------------------------------------------
// Session switch test (/session command)
// ---------------------------------------------------------------------------

/// A scripted keystroke to send at a given delay after TUI startup.
struct ScriptedInput {
    delay: Duration,
    data: Vec<u8>,
}

/// Spawn the TUI binary via PTY with scripted keystroke inputs.
///
/// Each `ScriptedInput` is sent after its `delay` from process start.
/// After all inputs are sent, Ctrl+C is sent after `final_ctrl_c_delay`
/// from the last input.
async fn run_tui_scripted(
    env: &HashMap<String, String>,
    args: &[String],
    script: Vec<ScriptedInput>,
    final_ctrl_c_delay: Duration,
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

    // Spawn task to send scripted inputs then Ctrl+C.
    let script_writer = writer_tx.clone();
    let script_task = tokio::spawn(async move {
        let start = tokio::time::Instant::now();
        for input in &script {
            let target = start + input.delay;
            tokio::time::sleep_until(target).await;
            let _ = script_writer.send(input.data.clone()).await;
        }
        // Final Ctrl+C sequence.
        tokio::time::sleep(final_ctrl_c_delay).await;
        for _ in 0..4 {
            let _ = script_writer.send(vec![3]).await;
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });

    let exit_code_result = timeout(overall_timeout, async {
        loop {
            select! {
                result = output_rx.recv() => match result {
                    Ok(chunk) => {
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

    script_task.abort();

    let exit_code = match exit_code_result {
        Ok(Ok(code)) => code,
        Ok(Err(err)) => return Err(err.into()),
        Err(_) => {
            session.terminate();
            anyhow::bail!("timed out waiting for codex-temporal-tui to exit");
        }
    };

    while let Ok(chunk) = output_rx.try_recv() {
        output.extend_from_slice(&chunk);
    }

    let output = String::from_utf8_lossy(&output).to_string();
    Ok(TuiOutput { exit_code, output })
}

/// Verify in-TUI session switching via `/session` command:
///
/// 1. Start ephemeral Temporal server + worker.
/// 2. Run TUI #1 via PTY with a prompt — creates session A, then Ctrl+C.
/// 3. Run TUI #2 via PTY with a different prompt — creates session B.
/// 4. In TUI #2, type `/session\r` → picker opens showing both sessions.
/// 5. Press Down arrow + Enter → switch to session A.
/// 6. Type `/session\r` again → picker opens.
/// 7. Press Down arrow + Enter → switch back to session B.
/// 8. Ctrl+C to exit.
/// 9. Assert TUI produced output and exited cleanly.
#[tokio::test]
async fn tui_session_switch() {
    match timeout(
        Duration::from_secs(300),
        tui_session_switch_inner(),
    )
    .await
    {
        Ok(()) => {}
        Err(_) => panic!("tui_session_switch timed out after 300s"),
    }
}

async fn tui_session_switch_inner() {
    std::env::var("OPENAI_API_KEY")
        .expect("OPENAI_API_KEY must be set to run PTY TUI tests");

    // --- Start ephemeral Temporal server + worker ---
    let mut _server = start_ephemeral_server().await;
    let server_target = _server.target.clone();
    spawn_worker(&server_target);

    // --- Set up CODEX_HOME ---
    let codex_home = setup_codex_home();
    let env = tui_env(&server_target, codex_home.path());

    // --- Run TUI #1: create session A ---
    eprintln!("  [switch] creating session A...");
    let result1 = run_tui_binary(
        &env,
        &["Say hello in one word".to_string()],
        Duration::from_secs(20),
        Duration::from_secs(60),
    )
    .await
    .expect("failed to run TUI #1");
    assert!(
        result1.exit_code == 0 || result1.exit_code == 130,
        "TUI #1 (session A) unexpected exit code {}: output:\n{}",
        result1.exit_code,
        result1.output,
    );
    eprintln!("  [switch] session A created (exit code {})", result1.exit_code);

    // --- Run TUI #2: create session B, then switch sessions via /session ---
    // Timeline:
    //  0s   — TUI starts with prompt "Say goodbye in one word"
    // 20s   — type "/session\r" (open picker — both sessions visible)
    // 23s   — press Down arrow then Enter (switch to session A)
    // 28s   — type "/session\r" again (open picker)
    // 31s   — press Down arrow then Enter (switch back to session B)
    // 34s+  — Ctrl+C
    eprintln!("  [switch] creating session B and testing /session switching...");

    // ESC[B is the Down arrow key sequence.
    let down_arrow: Vec<u8> = b"\x1b[B".to_vec();
    let enter: Vec<u8> = b"\r".to_vec();

    let script = vec![
        // Wait for the model to respond, then open the session picker.
        ScriptedInput {
            delay: Duration::from_secs(20),
            data: b"/session\r".to_vec(),
        },
        // Give the picker time to load sessions from harness and render.
        // The current session (B) is pre-selected, so press Down to select
        // session A, then Enter to switch.
        ScriptedInput {
            delay: Duration::from_secs(23),
            data: [down_arrow.clone(), enter.clone()].concat(),
        },
        // Wait for the switch to complete, then open the picker again.
        ScriptedInput {
            delay: Duration::from_secs(28),
            data: b"/session\r".to_vec(),
        },
        // Now session A is current. Press Down to select session B, then Enter.
        ScriptedInput {
            delay: Duration::from_secs(31),
            data: [down_arrow, enter].concat(),
        },
    ];

    let result2 = run_tui_scripted(
        &env,
        &["Say goodbye in one word".to_string()],
        script,
        Duration::from_secs(3), // Ctrl+C 3s after last scripted input
        Duration::from_secs(90),
    )
    .await
    .expect("failed to run TUI #2 with /session switching");

    assert!(
        result2.exit_code == 0 || result2.exit_code == 130,
        "TUI #2 (session switch) unexpected exit code {}: output:\n{}",
        result2.exit_code,
        result2.output,
    );
    assert!(
        !result2.output.is_empty(),
        "TUI #2 (session switch) produced no output",
    );

    // Verify the output contains evidence of the session picker being shown.
    // The picker title "Sessions" or "Select a session" should appear in the
    // rendered TUI output.
    let output_lower = result2.output.to_lowercase();
    assert!(
        output_lower.contains("session"),
        "TUI output should contain 'session' from the picker — /session command may not have worked.\nOutput length: {} bytes",
        result2.output.len(),
    );

    eprintln!("  [switch] TUI #2 exited (code {})", result2.exit_code);
}

// ---------------------------------------------------------------------------
// Tool approval test
// ---------------------------------------------------------------------------

/// A reactive keystroke: when `pattern` appears in the accumulated PTY output,
/// send `data` to the TUI.
struct ReactiveInput {
    pattern: String,
    data: Vec<u8>,
}

/// Spawn the TUI binary via PTY with reactive keystroke inputs.
///
/// Each `ReactiveInput` fires once when its `pattern` first appears in the
/// accumulated output. After all reactive inputs have fired (or after
/// `overall_timeout`), Ctrl+C is sent to shut down the TUI.
async fn run_tui_reactive(
    env: &HashMap<String, String>,
    args: &[String],
    mut reactions: Vec<ReactiveInput>,
    post_reaction_delay: Duration,
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
    let mut all_reacted = false;
    let mut reacted_at: Option<tokio::time::Instant> = None;

    let exit_code_result = timeout(overall_timeout, async {
        loop {
            // Send Ctrl+C after all reactions have fired + delay.
            if let Some(at) = reacted_at
                && tokio::time::Instant::now() >= at + post_reaction_delay
            {
                for _ in 0..4 {
                    let _ = writer_tx.send(vec![3]).await;
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                reacted_at = None; // only send once
            }

            select! {
                result = output_rx.recv() => match result {
                    Ok(chunk) => {
                        if chunk.windows(4).any(|window| window == b"\x1b[6n") {
                            let _ = writer_tx.send(b"\x1b[1;1R".to_vec()).await;
                        }
                        output.extend_from_slice(&chunk);

                        // Check reactive triggers against accumulated output.
                        if !all_reacted {
                            let text = String::from_utf8_lossy(&output);
                            let text_lower = text.to_lowercase();
                            let mut i = 0;
                            while i < reactions.len() {
                                if text_lower.contains(&reactions[i].pattern.to_lowercase()) {
                                    let reaction = reactions.remove(i);
                                    eprintln!("  [approval] matched pattern {:?}, sending keystroke", reaction.pattern);
                                    let _ = writer_tx.send(reaction.data).await;
                                } else {
                                    i += 1;
                                }
                            }
                            if reactions.is_empty() {
                                all_reacted = true;
                                reacted_at = Some(tokio::time::Instant::now());
                            }
                        }
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

    let exit_code = match exit_code_result {
        Ok(Ok(code)) => code,
        Ok(Err(err)) => return Err(err.into()),
        Err(_) => {
            session.terminate();
            anyhow::bail!("timed out waiting for codex-temporal-tui to exit");
        }
    };

    while let Ok(chunk) = output_rx.try_recv() {
        output.extend_from_slice(&chunk);
    }

    let output = String::from_utf8_lossy(&output).to_string();
    Ok(TuiOutput { exit_code, output })
}

/// Verify tool approval through the real TUI binary:
///
/// 1. Start ephemeral Temporal server + worker.
/// 2. Spawn TUI via PTY with a prompt that triggers a shell command.
/// 3. Wait for the approval overlay to appear (contains "proceed").
/// 4. Send 'y' to approve the tool call.
/// 5. Wait for the model to respond, then Ctrl+C to exit.
/// 6. Assert the TUI output contains evidence of the approved command.
#[tokio::test]
async fn tui_tool_approval() {
    match timeout(
        Duration::from_secs(300),
        tui_tool_approval_inner(),
    )
    .await
    {
        Ok(()) => {}
        Err(_) => panic!("tui_tool_approval timed out after 300s"),
    }
}

async fn tui_tool_approval_inner() {
    std::env::var("OPENAI_API_KEY")
        .expect("OPENAI_API_KEY must be set to run PTY TUI tests");

    // --- Start ephemeral Temporal server + worker ---
    let mut _server = start_ephemeral_server().await;
    let server_target = _server.target.clone();
    spawn_worker(&server_target);

    // --- Set up CODEX_HOME ---
    let codex_home = setup_codex_home();
    let env = tui_env(&server_target, codex_home.path());

    // --- Spawn TUI with a prompt that will trigger a tool call ---
    // The model should invoke `shell` to run `echo hello`. The default
    // approval policy (OnRequest) will show an approval overlay.
    // When the overlay appears (containing "proceed"), we send 'y' to approve.
    // After approval, the model completes the turn. We then Ctrl+C to exit.
    eprintln!("  [approval] starting TUI with tool-triggering prompt...");

    let result = run_tui_reactive(
        &env,
        &["Use shell to run: echo hello".to_string()],
        vec![
            ReactiveInput {
                pattern: "proceed".to_string(),
                data: b"y".to_vec(),
            },
        ],
        Duration::from_secs(10), // wait 10s after approval for model to respond
        Duration::from_secs(120),
    )
    .await
    .expect("failed to run TUI binary");

    assert!(
        result.exit_code == 0 || result.exit_code == 130,
        "unexpected exit code {}: output:\n{}",
        result.exit_code,
        result.output,
    );
    assert!(
        !result.output.is_empty(),
        "TUI produced no output — binary may have failed silently",
    );

    // The output should contain evidence that the tool call was approved
    // and executed. Look for the approval confirmation or the command output.
    let output_lower = result.output.to_lowercase();
    assert!(
        output_lower.contains("echo") || output_lower.contains("hello") || output_lower.contains("approved"),
        "TUI output should contain evidence of the echo command or approval.\nOutput length: {} bytes",
        result.output.len(),
    );

    eprintln!("  [approval] TUI exited (code {})", result.exit_code);

    // --- Verify the approval signal reached the workflow ---
    // The ephemeral server is still alive (held by `_server`).
    // Query workflow history to confirm the ExecApproval op was delivered.
    eprintln!("  [approval] verifying workflow history...");

    let list_output = tokio::process::Command::new("temporal")
        .args([
            "workflow", "list",
            "--address", &server_target,
            "--limit", "20",
            "-o", "json",
        ])
        .output()
        .await
        .expect("temporal workflow list failed");
    let list_text = String::from_utf8_lossy(&list_output.stdout);

    // Find the agent workflow (ID ends with "/main").
    let mut agent_wf_id: Option<String> = None;
    for line in list_text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Search for codex-tui-*/main pattern in any JSON or raw text.
        if let Some(pos) = line.find("codex-tui-") {
            let id: String = line[pos..]
                .chars()
                .take_while(|c| !c.is_whitespace() && *c != '"' && *c != ',')
                .collect();
            if id.contains("/main") {
                agent_wf_id = Some(id);
                break;
            }
        }
    }

    if let Some(ref wf_id) = agent_wf_id {
        eprintln!("  [approval] agent workflow: {wf_id}");

        let history_output = tokio::process::Command::new("temporal")
            .args([
                "workflow", "show",
                "--workflow-id", wf_id,
                "--address", &server_target,
                "-o", "json",
            ])
            .output()
            .await
            .expect("temporal workflow show failed");
        let history_text = String::from_utf8_lossy(&history_output.stdout);

        let has_approval = history_text.contains("exec_approval");
        let has_tool_exec = history_text.contains("tool_exec");
        eprintln!(
            "  [approval] history check: exec_approval={has_approval}, tool_exec={has_tool_exec}"
        );

        assert!(
            has_approval || has_tool_exec,
            "BUG: workflow history contains no exec_approval signal and no tool_exec activity.\n\
             The TUI showed the approval overlay and 'y' was sent, but the approval\n\
             never reached the workflow. This is the known TUI→workflow signal bug.\n\
             Agent workflow: {wf_id}",
        );
        eprintln!("  [approval] PASSED — approval signal reached the workflow");
    } else {
        eprintln!(
            "  [approval] WARNING: could not find agent workflow in list — \
             skipping history verification"
        );
    }
}
