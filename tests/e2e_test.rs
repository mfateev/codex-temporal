//! End-to-end tests for `TemporalAgentSession`.
//!
//! All test cases run inside a single `#[tokio::test]` so they share one tokio
//! runtime, one ephemeral Temporal server, and one worker.  Each case creates
//! its own `TemporalAgentSession` with a unique workflow ID.
//!
//! **Requires** `OPENAI_API_KEY` — tests will fail if the env var is absent.

use std::str::FromStr;
use std::time::Duration;

use codex_core::AgentSession;
use codex_protocol::config_types::{ReasoningSummary, WebSearchMode};
use codex_protocol::protocol::{
    AskForApproval, EventMsg, Op, ReviewDecision, SandboxPolicy,
};
use codex_protocol::user_input::UserInput;

use codex_temporal::activities::CodexActivities;
use codex_temporal::session::TemporalAgentSession;
use codex_temporal::types::CodexWorkflowInput;
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
        summary: ReasoningSummary::Auto,
        final_output_json_schema: None,
        collaboration_mode: None,
        personality: None,
    }
}

/// Create a `TemporalAgentSession` with a unique workflow ID.
fn new_session(client: &Client, model: &str) -> TemporalAgentSession {
    let workflow_id = format!("e2e-{}", uuid::Uuid::new_v4());
    let base_input = CodexWorkflowInput {
        user_message: String::new(),
        model: model.to_string(),
        instructions: "You are a helpful coding assistant. Be concise.".to_string(),
        approval_policy: Default::default(),
        web_search_mode: None,
    };
    TemporalAgentSession::new(client.clone(), workflow_id, base_input)
}

/// Drain events until `ShutdownComplete` or timeout (best-effort).
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
// Test cases
// ---------------------------------------------------------------------------

async fn model_only_turn(client: &Client) {
    let session = new_session(client, "gpt-4o");

    session
        .submit(user_turn_op("Say hello in one sentence."))
        .await
        .expect("submit failed");

    let mut saw_turn_started = false;
    let turn_complete_msg;
    let timeout = tokio::time::Instant::now() + Duration::from_secs(120);

    loop {
        if tokio::time::Instant::now() > timeout {
            panic!("timed out waiting for TurnComplete");
        }
        let event = session.next_event().await.expect("next_event failed");
        match &event.msg {
            EventMsg::TurnStarted(_) => saw_turn_started = true,
            EventMsg::TurnComplete(tc) => {
                turn_complete_msg = tc.last_agent_message.clone();
                break;
            }
            _ => {}
        }
    }

    assert!(saw_turn_started, "expected TurnStarted event");
    assert!(
        turn_complete_msg.is_some(),
        "expected non-empty last_agent_message in TurnComplete"
    );

    drain_shutdown(&session).await;
}

async fn tool_approval_flow(client: &Client) {
    let session = new_session(client, "gpt-4o");

    session
        .submit(user_turn_op(
            "Use shell to run 'echo hello world' and tell me the output.",
        ))
        .await
        .expect("submit failed");

    // Wait for ExecApprovalRequest.
    let call_id;
    let timeout = tokio::time::Instant::now() + Duration::from_secs(120);

    loop {
        if tokio::time::Instant::now() > timeout {
            panic!("timed out waiting for ExecApprovalRequest");
        }
        let event = session.next_event().await.expect("next_event failed");
        if let EventMsg::ExecApprovalRequest(req) = &event.msg {
            call_id = req.call_id.clone();
            break;
        }
    }

    // Approve.
    session
        .submit(Op::ExecApproval {
            id: call_id,
            turn_id: None,
            decision: ReviewDecision::Approved,
        })
        .await
        .expect("approval submit failed");

    // Wait for TurnComplete (auto-approve any further tool calls).
    let turn_complete_msg;
    let timeout2 = tokio::time::Instant::now() + Duration::from_secs(120);

    loop {
        if tokio::time::Instant::now() > timeout2 {
            panic!("timed out waiting for TurnComplete after approval");
        }
        let event = session.next_event().await.expect("next_event failed");
        match &event.msg {
            EventMsg::TurnComplete(tc) => {
                turn_complete_msg = tc.last_agent_message.clone();
                break;
            }
            EventMsg::ExecApprovalRequest(req) => {
                session
                    .submit(Op::ExecApproval {
                        id: req.call_id.clone(),
                        turn_id: None,
                        decision: ReviewDecision::Approved,
                    })
                    .await
                    .expect("subsequent approval failed");
            }
            _ => {}
        }
    }

    assert!(
        turn_complete_msg.is_some(),
        "expected non-empty last_agent_message in TurnComplete"
    );

    drain_shutdown(&session).await;
}

/// Build an `Op::UserTurn` with the given text message and model.
fn user_turn_op_with_model(text: &str, model: &str) -> Op {
    Op::UserTurn {
        items: vec![UserInput::Text {
            text: text.to_string(),
            text_elements: vec![],
        }],
        cwd: std::env::current_dir().unwrap_or_else(|_| "/tmp".into()),
        approval_policy: AskForApproval::Never,
        sandbox_policy: SandboxPolicy::DangerFullAccess,
        model: model.to_string(),
        effort: None,
        summary: ReasoningSummary::Auto,
        final_output_json_schema: None,
        collaboration_mode: None,
        personality: None,
    }
}

/// Wait for TurnComplete, auto-approving any tool calls along the way.
/// Returns the `last_agent_message` from TurnComplete.
async fn wait_for_turn_complete(session: &TemporalAgentSession) -> Option<String> {
    let timeout = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        if tokio::time::Instant::now() > timeout {
            panic!("timed out waiting for TurnComplete");
        }
        let event = session.next_event().await.expect("next_event failed");
        match &event.msg {
            EventMsg::TurnComplete(tc) => return tc.last_agent_message.clone(),
            EventMsg::ExecApprovalRequest(req) => {
                session
                    .submit(Op::ExecApproval {
                        id: req.call_id.clone(),
                        turn_id: None,
                        decision: ReviewDecision::Approved,
                    })
                    .await
                    .expect("approval failed");
            }
            _ => {}
        }
    }
}

async fn model_selection_gpt4o_mini(client: &Client) {
    let session = new_session(client, "gpt-4o-mini");

    session
        .submit(user_turn_op_with_model(
            "What is 2+2? Reply with just the number.",
            "gpt-4o-mini",
        ))
        .await
        .expect("submit failed");

    let msg = wait_for_turn_complete(&session).await;
    assert!(msg.is_some(), "expected agent message from gpt-4o-mini");

    drain_shutdown(&session).await;
}

async fn multi_turn_conversation(client: &Client) {
    let session = new_session(client, "gpt-4o-mini");

    // Turn 1
    session
        .submit(user_turn_op_with_model(
            "Remember the word 'orange'. Reply with just 'OK'.",
            "gpt-4o-mini",
        ))
        .await
        .expect("submit turn 1 failed");

    let msg1 = wait_for_turn_complete(&session).await;
    assert!(msg1.is_some(), "expected agent message for turn 1");

    // Turn 2 — references turn 1 context
    session
        .submit(user_turn_op_with_model(
            "What word did I ask you to remember? Reply with just the word.",
            "gpt-4o-mini",
        ))
        .await
        .expect("submit turn 2 failed");

    let msg2 = wait_for_turn_complete(&session).await;
    assert!(msg2.is_some(), "expected agent message for turn 2");
    let text = msg2.unwrap().to_lowercase();
    assert!(
        text.contains("orange"),
        "turn 2 should recall 'orange', got: {text}"
    );

    drain_shutdown(&session).await;
}

async fn model_selection_with_tool_use(client: &Client) {
    let session = new_session(client, "gpt-4o-mini");

    session
        .submit(user_turn_op_with_model(
            "Use shell to run 'echo model-test-ok' and tell me the output.",
            "gpt-4o-mini",
        ))
        .await
        .expect("submit failed");

    let msg = wait_for_turn_complete(&session).await;
    assert!(msg.is_some(), "expected agent message from gpt-4o-mini with tool use");
    let text = msg.unwrap().to_lowercase();
    assert!(
        text.contains("model-test-ok"),
        "expected tool output in response, got: {text}"
    );

    drain_shutdown(&session).await;
}

/// Create a session with web search enabled.
fn new_session_with_web_search(client: &Client, model: &str) -> TemporalAgentSession {
    let workflow_id = format!("e2e-ws-{}", uuid::Uuid::new_v4());
    let base_input = CodexWorkflowInput {
        user_message: String::new(),
        model: model.to_string(),
        instructions: "You are a helpful assistant. Be concise.".to_string(),
        approval_policy: AskForApproval::Never,
        web_search_mode: Some(WebSearchMode::Live),
    };
    TemporalAgentSession::new(client.clone(), workflow_id, base_input)
}

async fn web_search_turn(client: &Client) {
    let session = new_session_with_web_search(client, "gpt-4o");

    session
        .submit(user_turn_op(
            "Search the web: what is the latest stable Rust version number? Reply with just the version.",
        ))
        .await
        .expect("submit failed");

    let turn_complete_msg;
    let timeout = tokio::time::Instant::now() + Duration::from_secs(120);

    loop {
        if tokio::time::Instant::now() > timeout {
            panic!("timed out waiting for TurnComplete in web_search_turn");
        }
        let event = session.next_event().await.expect("next_event failed");
        match &event.msg {
            EventMsg::TurnComplete(tc) => {
                turn_complete_msg = tc.last_agent_message.clone();
                break;
            }
            EventMsg::ExecApprovalRequest(req) => {
                // Auto-approve any tool calls (shouldn't happen with Never policy).
                session
                    .submit(Op::ExecApproval {
                        id: req.call_id.clone(),
                        turn_id: None,
                        decision: ReviewDecision::Approved,
                    })
                    .await
                    .expect("approval failed");
            }
            _ => {}
        }
    }

    assert!(
        turn_complete_msg.is_some(),
        "expected non-empty last_agent_message in web search TurnComplete"
    );

    drain_shutdown(&session).await;
}

// ---------------------------------------------------------------------------
// Single test entry-point
// ---------------------------------------------------------------------------

#[tokio::test]
async fn e2e_tests() {
    match tokio::time::timeout(Duration::from_secs(360), e2e_tests_inner()).await {
        Ok(()) => {}
        Err(_) => panic!("e2e_tests timed out after 360s"),
    }
}

async fn e2e_tests_inner() {
    std::env::var("OPENAI_API_KEY")
        .expect("OPENAI_API_KEY must be set to run E2E tests");

    // --- start ephemeral server (fail fast if download/start hangs) ---
    let server_result = tokio::time::timeout(Duration::from_secs(60), async {
        let config = TemporalDevServerConfig::builder()
            .exe(default_cached_download())
            .build();
        config.start_server().await
    })
    .await;

    let mut server = match server_result {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => panic!("failed to start ephemeral server: {e}"),
        Err(_) => panic!("ephemeral server startup timed out (60s)"),
    };

    let server_target = server.target.clone();

    // --- client connection ---
    let conn_opts = ConnectionOptions::new(
        Url::from_str(&format!("http://{}", server_target)).expect("bad URL"),
    )
    .identity("e2e-test-client")
    .build();
    let telemetry_options = TelemetryOptions::builder().build();
    let runtime_options = RuntimeOptions::builder()
        .telemetry_options(telemetry_options)
        .build()
        .expect("runtime options");
    let runtime = CoreRuntime::new_assume_tokio(runtime_options).expect("runtime");

    let connection = tokio::time::timeout(
        Duration::from_secs(10),
        Connection::connect(conn_opts),
    )
    .await
    .expect("connection to ephemeral server timed out (10s)")
    .expect("failed to connect to ephemeral server");
    let client = Client::new(connection, ClientOptions::new("default").build())
        .expect("failed to create client");

    // --- worker on dedicated thread (Worker future is !Send) ---
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
            .identity("e2e-test-worker")
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
                .register_workflow::<CodexWorkflow>()
                .register_activities(CodexActivities::new())
                .build();
            let mut worker =
                Worker::new(&worker_runtime, worker_client, opts).expect("failed to create worker");

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

    // --- run test cases ---
    eprintln!("--- test: model_only_turn ---");
    model_only_turn(&client).await;

    eprintln!("--- test: tool_approval_flow ---");
    tool_approval_flow(&client).await;

    eprintln!("--- test: web_search_turn ---");
    web_search_turn(&client).await;

    eprintln!("--- test: model_selection_gpt4o_mini ---");
    model_selection_gpt4o_mini(&client).await;

    eprintln!("--- test: multi_turn_conversation ---");
    multi_turn_conversation(&client).await;

    eprintln!("--- test: model_selection_with_tool_use ---");
    model_selection_with_tool_use(&client).await;

    // --- teardown ---
    drop(client);
    drop(runtime);
    let _ = server.shutdown().await;
}
