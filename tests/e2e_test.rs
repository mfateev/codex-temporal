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
use codex_protocol::openai_models::ReasoningEffort;
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
        reasoning_effort: None,
        reasoning_summary: ReasoningSummary::Auto,
        personality: None,
        continued_state: None,
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
        reasoning_effort: None,
        reasoning_summary: ReasoningSummary::Auto,
        personality: None,
        continued_state: None,
    };
    TemporalAgentSession::new(client.clone(), workflow_id, base_input)
}

/// Create a session with a specific reasoning effort.
fn new_session_with_effort(
    client: &Client,
    model: &str,
    effort: ReasoningEffort,
) -> TemporalAgentSession {
    let workflow_id = format!("e2e-effort-{}", uuid::Uuid::new_v4());
    let base_input = CodexWorkflowInput {
        user_message: String::new(),
        model: model.to_string(),
        instructions: "You are a helpful coding assistant. Be concise.".to_string(),
        approval_policy: AskForApproval::Never,
        web_search_mode: None,
        reasoning_effort: Some(effort),
        reasoning_summary: ReasoningSummary::Auto,
        personality: None,
        continued_state: None,
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

/// Create a session with a long system prompt to exceed the 1024-token
/// minimum for OpenAI prompt caching.
fn new_session_for_caching(client: &Client) -> TemporalAgentSession {
    let workflow_id = format!("e2e-cache-{}", uuid::Uuid::new_v4());
    // Build instructions > 1024 tokens so the prefix is cacheable.
    let long_instructions = format!(
        "You are a helpful coding assistant. Be concise. \
         Always reply in plain text without markdown formatting. \
         Below is reference context that you should keep in mind:\n\n{}",
        "The quick brown fox jumps over the lazy dog. ".repeat(100)
    );
    let base_input = CodexWorkflowInput {
        user_message: String::new(),
        model: "gpt-4o-mini".to_string(),
        instructions: long_instructions,
        approval_policy: AskForApproval::Never,
        web_search_mode: None,
        reasoning_effort: None,
        reasoning_summary: ReasoningSummary::Auto,
        personality: None,
        continued_state: None,
    };
    TemporalAgentSession::new(client.clone(), workflow_id, base_input)
}

async fn prompt_caching_multi_turn(client: &Client) {
    let session = new_session_for_caching(client);

    // Turn 1: establish context
    session
        .submit(user_turn_op_with_model(
            "Remember: the secret word is 'pineapple'. Reply with just 'OK'.",
            "gpt-4o-mini",
        ))
        .await
        .expect("submit turn 1 failed");

    // Collect events from turn 1, looking for TokenCount
    let mut turn1_token_count_events = Vec::new();
    let timeout = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        if tokio::time::Instant::now() > timeout {
            panic!("timed out waiting for TurnComplete in turn 1");
        }
        let event = session.next_event().await.expect("next_event failed");
        if let EventMsg::TokenCount(ref tc) = event.msg {
            turn1_token_count_events.push(tc.clone());
        }
        if matches!(event.msg, EventMsg::TurnComplete(_)) {
            break;
        }
    }

    assert!(
        !turn1_token_count_events.is_empty(),
        "expected at least one TokenCount event in turn 1"
    );

    // Verify turn 1 has valid token counts
    let turn1_last = turn1_token_count_events.last().unwrap();
    let turn1_info = turn1_last.info.as_ref().expect("turn 1 TokenCount.info should be Some");
    eprintln!(
        "turn 1 last_token_usage: input={}, cached={}, output={}, total={}",
        turn1_info.last_token_usage.input_tokens,
        turn1_info.last_token_usage.cached_input_tokens,
        turn1_info.last_token_usage.output_tokens,
        turn1_info.last_token_usage.total_tokens,
    );
    assert!(
        turn1_info.last_token_usage.input_tokens > 0,
        "expected input_tokens > 0 in turn 1"
    );
    assert!(
        turn1_info.last_token_usage.total_tokens > 0,
        "expected total_tokens > 0 in turn 1"
    );

    // Turn 2: same conversation, should benefit from prompt caching
    session
        .submit(user_turn_op_with_model(
            "What secret word did I tell you? Reply with just the word.",
            "gpt-4o-mini",
        ))
        .await
        .expect("submit turn 2 failed");

    // Collect events from turn 2
    let mut turn2_token_count_events = Vec::new();
    let timeout2 = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        if tokio::time::Instant::now() > timeout2 {
            panic!("timed out waiting for TurnComplete in turn 2");
        }
        let event = session.next_event().await.expect("next_event failed");
        if let EventMsg::TokenCount(ref tc) = event.msg {
            turn2_token_count_events.push(tc.clone());
        }
        if matches!(event.msg, EventMsg::TurnComplete(_)) {
            break;
        }
    }

    assert!(
        !turn2_token_count_events.is_empty(),
        "expected at least one TokenCount event in turn 2"
    );

    // Verify turn 2 has valid token counts and check for caching
    let last_tc = turn2_token_count_events.last().unwrap();
    let info = last_tc.info.as_ref().expect("turn 2 TokenCount.info should be Some");
    eprintln!(
        "turn 2 last_token_usage: input={}, cached={}, output={}, total={}",
        info.last_token_usage.input_tokens,
        info.last_token_usage.cached_input_tokens,
        info.last_token_usage.output_tokens,
        info.last_token_usage.total_tokens,
    );
    eprintln!(
        "turn 2 total_token_usage: input={}, cached={}, output={}, total={}",
        info.total_token_usage.input_tokens,
        info.total_token_usage.cached_input_tokens,
        info.total_token_usage.output_tokens,
        info.total_token_usage.total_tokens,
    );

    // Token tracking must work (non-zero values).
    assert!(
        info.last_token_usage.input_tokens > 0,
        "expected input_tokens > 0 in turn 2"
    );
    assert!(
        info.last_token_usage.total_tokens > 0,
        "expected total_tokens > 0 in turn 2"
    );

    // Cumulative totals should exceed turn 2 alone.
    assert!(
        info.total_token_usage.total_tokens > info.last_token_usage.total_tokens,
        "cumulative total_tokens ({}) should exceed turn 2 alone ({})",
        info.total_token_usage.total_tokens,
        info.last_token_usage.total_tokens,
    );

    // With a long system prompt (>1024 tokens), cached_input_tokens should
    // be > 0 on the second turn. This validates prompt caching is active.
    assert!(
        info.last_token_usage.cached_input_tokens > 0,
        "expected cached_input_tokens > 0 in turn 2 (got {}); \
         prompt caching requires >1024 token prefix",
        info.last_token_usage.cached_input_tokens,
    );

    drain_shutdown(&session).await;
}

async fn reasoning_effort_turn(client: &Client) {
    let session = new_session_with_effort(client, "gpt-4o", ReasoningEffort::Low);

    session
        .submit(user_turn_op("What is 2+2? Reply with just the number."))
        .await
        .expect("submit failed");

    let msg = wait_for_turn_complete(&session).await;
    assert!(
        msg.is_some(),
        "expected agent message with reasoning effort Low"
    );

    drain_shutdown(&session).await;
}

/// Test: compact triggers continue-as-new and conversation context is preserved.
///
/// 1. Submit turn asking to remember a word.
/// 2. Wait for TurnComplete.
/// 3. Submit Op::Compact (triggers CAN behind the scenes).
/// 4. Wait for ContextCompactedEvent.
/// 5. After CAN, submit another turn asking for the remembered word.
/// 6. Verify the response contains the word (context was preserved).
async fn compact_triggers_continue_as_new(client: &Client) {
    let session = new_session(client, "gpt-4o-mini");

    // Turn 1: ask the model to remember a word.
    session
        .submit(user_turn_op_with_model(
            "Remember this word exactly: 'elephant'. Just confirm you've remembered it.",
            "gpt-4o-mini",
        ))
        .await
        .expect("submit turn 1 failed");

    let msg = wait_for_turn_complete(&session).await;
    assert!(msg.is_some(), "expected agent message from turn 1");

    // Submit compact — this triggers continue-as-new.
    session
        .submit(Op::Compact)
        .await
        .expect("submit compact failed");

    // Wait for ContextCompactedEvent (emitted before CAN).
    let timeout = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if tokio::time::Instant::now() > timeout {
            panic!("timed out waiting for ContextCompactedEvent");
        }
        let event = session.next_event().await.expect("next_event failed");
        if matches!(event.msg, EventMsg::ContextCompacted(_)) {
            break;
        }
    }

    // After CAN, the workflow has restarted with carried-over state.
    // Give a brief pause for the new run to be ready.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Turn 2: ask for the remembered word (tests context preservation).
    session
        .submit(user_turn_op_with_model(
            "What word did I ask you to remember? Reply with just the word.",
            "gpt-4o-mini",
        ))
        .await
        .expect("submit turn 2 failed");

    let msg = wait_for_turn_complete(&session).await;
    let response = msg.expect("expected agent message from turn 2");
    assert!(
        response.to_lowercase().contains("elephant"),
        "expected 'elephant' in response, got: {response}"
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

    eprintln!("--- test: prompt_caching_multi_turn ---");
    prompt_caching_multi_turn(&client).await;

    eprintln!("--- test: reasoning_effort_turn ---");
    reasoning_effort_turn(&client).await;

    eprintln!("--- test: compact_triggers_continue_as_new ---");
    compact_triggers_continue_as_new(&client).await;

    // --- teardown ---
    drop(client);
    drop(runtime);
    let _ = server.shutdown().await;
}
