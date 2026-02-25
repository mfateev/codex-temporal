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
use codex_temporal::harness::{CodexHarness, CodexHarnessRun};
use codex_temporal::picker::{extract_session_id_from_path, sessions_to_threads_page};
use codex_temporal::session::TemporalAgentSession;
use codex_temporal::types::{
    CodexWorkflowInput, HarnessInput, SessionEntry, SessionStatus,
};
use codex_temporal::workflow::CodexWorkflow;

use temporalio_client::{
    Client, ClientOptions, Connection, ConnectionOptions,
    WorkflowQueryOptions, WorkflowSignalOptions, WorkflowStartOptions,
};
use temporalio_common::protos::temporal::api::enums::v1::WorkflowIdConflictPolicy;
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
        developer_instructions: None,
        model_provider: None,
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
        developer_instructions: None,
        model_provider: None,
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
        developer_instructions: None,
        model_provider: None,
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
        developer_instructions: None,
        model_provider: None,
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

/// Test: session resume via harness workflow.
///
/// 1. Start harness workflow.
/// 2. Start session workflow, register with harness.
/// 3. Submit turn 1 ("Remember 'blue-moon-42', reply OK"), wait for TurnComplete.
/// 4. Query harness list_sessions() — assert session appears with Running status.
/// 5. Create TemporalAgentSession::resume(), call fetch_initial_events() — verify replayed events.
/// 6. Submit turn 2 via resumed session ("What code did I tell you?").
/// 7. Assert response contains "blue-moon-42".
/// 8. Shutdown.
async fn session_resume(client: &Client) {
    // 1. Start harness workflow (idempotent).
    let harness_id = format!("e2e-harness-{}", uuid::Uuid::new_v4());
    let harness_input = HarnessInput {
        continued_state: None,
    };
    let harness_options = WorkflowStartOptions::new(TASK_QUEUE, &harness_id)
        .id_conflict_policy(WorkflowIdConflictPolicy::UseExisting)
        .build();
    client
        .start_workflow(CodexHarness::run, harness_input, harness_options)
        .await
        .expect("failed to start harness workflow");

    // 2. Start a regular session workflow.
    let session = new_session(client, "gpt-4o-mini");
    let session_id = session.session_id().to_string();

    // Register with harness.
    let now_millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let entry = SessionEntry {
        session_id: session_id.clone(),
        name: Some("resume test".to_string()),
        model: "gpt-4o-mini".to_string(),
        created_at_millis: now_millis,
        status: SessionStatus::Running,
    };
    let harness_handle = client.get_workflow_handle::<CodexHarnessRun>(&harness_id);
    harness_handle
        .signal(
            CodexHarness::register_session,
            entry,
            WorkflowSignalOptions::default(),
        )
        .await
        .expect("failed to register session with harness");

    // 3. Submit turn 1.
    session
        .submit(user_turn_op_with_model(
            "Remember this exact code: 'blue-moon-42'. Reply with just 'OK'.",
            "gpt-4o-mini",
        ))
        .await
        .expect("submit turn 1 failed");

    let msg1 = wait_for_turn_complete(&session).await;
    assert!(msg1.is_some(), "expected agent message from turn 1");

    // 4. Query harness — session should appear as Running.
    let sessions_json: String = harness_handle
        .query(
            CodexHarness::list_sessions,
            (),
            WorkflowQueryOptions::default(),
        )
        .await
        .expect("failed to query list_sessions");
    let sessions: Vec<SessionEntry> =
        serde_json::from_str(&sessions_json).expect("invalid sessions JSON");
    let found = sessions
        .iter()
        .find(|s| s.session_id == session_id)
        .expect("session should appear in harness registry");
    assert_eq!(
        found.status,
        SessionStatus::Running,
        "session status should be Running"
    );

    // 5. Resume: create a new TemporalAgentSession attached to the same workflow.
    let base_input = CodexWorkflowInput {
        user_message: String::new(),
        model: "gpt-4o-mini".to_string(),
        instructions: "You are a helpful coding assistant. Be concise.".to_string(),
        approval_policy: Default::default(),
        web_search_mode: None,
        reasoning_effort: None,
        reasoning_summary: ReasoningSummary::Auto,
        personality: None,
        developer_instructions: None,
        model_provider: None,
        continued_state: None,
    };
    let resumed = TemporalAgentSession::resume(client.clone(), session_id.clone(), base_input);

    // fetch_initial_events should return events from turn 1.
    let initial_events = resumed
        .fetch_initial_events()
        .await
        .expect("fetch_initial_events failed");
    assert!(
        !initial_events.is_empty(),
        "expected replayed events from turn 1, got empty"
    );
    // Verify we see a TurnComplete in the replayed events.
    let has_turn_complete = initial_events
        .iter()
        .any(|e| matches!(e, EventMsg::TurnComplete(_)));
    assert!(
        has_turn_complete,
        "expected TurnComplete in replayed events"
    );

    // 6. Submit turn 2 via resumed session.
    resumed
        .submit(user_turn_op_with_model(
            "What code did I tell you to remember? Reply with just the code.",
            "gpt-4o-mini",
        ))
        .await
        .expect("submit turn 2 via resumed session failed");

    let msg2 = wait_for_turn_complete(&resumed).await;
    let response = msg2.expect("expected agent message from turn 2");

    // 7. Assert response contains the remembered code.
    assert!(
        response.to_lowercase().contains("blue-moon-42"),
        "expected 'blue-moon-42' in response, got: {response}"
    );

    // 8. Shutdown.
    drain_shutdown(&resumed).await;
}

/// Test: project context (AGENTS.md) is injected into model prompts.
///
/// 1. Write a temporary AGENTS.md in the worker's cwd with a unique marker.
/// 2. Start a workflow asking "What do your project instructions say?"
/// 3. Verify the model's response references the marker string.
/// 4. Clean up the temporary AGENTS.md.
async fn project_context_injected(client: &Client) {
    let marker = format!("TEMPORAL_CONTEXT_MARKER_{}", uuid::Uuid::new_v4());
    let agents_md_content = format!(
        "# Project Instructions\n\n\
         This project uses a special marker for testing: {marker}\n\n\
         Always mention this marker when asked about project instructions.\n"
    );

    // Write AGENTS.md in the process cwd (which is the worker's cwd).
    let cwd = std::env::current_dir().expect("failed to get cwd");
    let agents_md_path = cwd.join("AGENTS.md");
    std::fs::write(&agents_md_path, &agents_md_content)
        .expect("failed to write AGENTS.md");

    // Guard to ensure cleanup even if the test panics.
    struct CleanupGuard(std::path::PathBuf);
    impl Drop for CleanupGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _guard = CleanupGuard(agents_md_path);

    let session = new_session(client, "gpt-4o-mini");

    session
        .submit(user_turn_op_with_model(
            "What do your AGENTS.md project instructions say? \
             Include the exact marker string in your response.",
            "gpt-4o-mini",
        ))
        .await
        .expect("submit failed");

    let msg = wait_for_turn_complete(&session).await;
    let response = msg.expect("expected agent message about project instructions");

    assert!(
        response.contains(&marker),
        "expected model response to contain marker '{marker}', got: {response}"
    );

    drain_shutdown(&session).await;
}

/// Test: config.toml loading flows through to the workflow.
///
/// 1. Write a temp config.toml with `model = "gpt-4o-mini"` and custom instructions.
/// 2. Set CODEX_HOME to the temp dir.
/// 3. Load config via `load_harness_config()` + `apply_env_overrides()`.
/// 4. Start a workflow asking "Say hello", verify the response is influenced by
///    the custom instructions (proving config.toml was loaded and flowed through).
/// 5. Clean up.
async fn config_file_loaded(client: &Client) {
    use codex_temporal::config_loader;

    // Create a temp dir to act as CODEX_HOME.
    let temp_dir = std::env::temp_dir().join(format!("codex-cfg-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).expect("failed to create temp dir");

    // Write instructions to a file referenced by config.toml.
    let instructions_file = temp_dir.join("instructions.md");
    std::fs::write(
        &instructions_file,
        "Always respond in French. Tu dois répondre en français.",
    )
    .expect("failed to write instructions file");

    // Write config.toml with custom model and model_instructions_file.
    let config_content = format!(
        "model = \"gpt-4o-mini\"\nmodel_instructions_file = \"{}\"\n",
        instructions_file.to_string_lossy().replace('\\', "/"),
    );
    std::fs::write(temp_dir.join("config.toml"), &config_content)
        .expect("failed to write config.toml");

    // Guard to ensure cleanup even if the test panics.
    struct CleanupGuard(std::path::PathBuf);
    impl Drop for CleanupGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _guard = CleanupGuard(temp_dir.clone());

    // Set CODEX_HOME to the temp dir so ConfigBuilder picks it up.
    let original_codex_home = std::env::var("CODEX_HOME").ok();
    // SAFETY: e2e tests run sequentially in a single thread within e2e_tests_inner.
    unsafe { std::env::set_var("CODEX_HOME", &temp_dir) };

    // Clear CODEX_MODEL env var so config.toml value is used.
    let original_codex_model = std::env::var("CODEX_MODEL").ok();
    // SAFETY: e2e tests run sequentially in a single thread within e2e_tests_inner.
    unsafe { std::env::remove_var("CODEX_MODEL") };

    let harness_config = config_loader::load_harness_config()
        .await
        .expect("load_harness_config failed");
    let mut input = harness_config.base_input;
    config_loader::apply_env_overrides(&mut input);
    input.model_provider = Some(harness_config.model_provider);

    // Restore env vars.
    // SAFETY: e2e tests run sequentially in a single thread within e2e_tests_inner.
    unsafe {
        match original_codex_home {
            Some(val) => std::env::set_var("CODEX_HOME", val),
            None => std::env::remove_var("CODEX_HOME"),
        }
        match original_codex_model {
            Some(val) => std::env::set_var("CODEX_MODEL", val),
            None => {} // was already unset
        }
    }

    // Verify config.toml values were picked up.
    assert_eq!(
        input.model, "gpt-4o-mini",
        "model should come from config.toml"
    );
    assert!(
        input.instructions.contains("français") || input.instructions.contains("French"),
        "instructions should come from config.toml, got: {}",
        input.instructions,
    );

    // Start a workflow with these config-loaded settings.
    let workflow_id = format!("e2e-cfg-{}", uuid::Uuid::new_v4());
    input.user_message = "Say hello.".to_string();

    let session = TemporalAgentSession::new(client.clone(), workflow_id, input);

    session
        .submit(user_turn_op_with_model("Say hello.", "gpt-4o-mini"))
        .await
        .expect("submit failed");

    let msg = wait_for_turn_complete(&session).await;
    assert!(msg.is_some(), "expected agent message");
    let response = msg.unwrap().to_lowercase();

    // The model should respond in French due to the instructions.
    assert!(
        response.contains("bonjour")
            || response.contains("salut")
            || response.contains("french")
            || response.contains("français"),
        "expected French response due to config.toml instructions, got: {response}"
    );

    drain_shutdown(&session).await;
}

/// Test: config activity loads features from worker-side config.toml.
///
/// 1. Write a config.toml with features enabled to a temp CODEX_HOME.
/// 2. Start a workflow that uses a shell tool.
/// 3. The load_config activity on the worker picks up the config.
/// 4. Verify the tool executes successfully (proving config flowed through).
async fn config_activity_loads_features(client: &Client) {
    // Create a temp dir to act as CODEX_HOME for the worker.
    let temp_dir = std::env::temp_dir().join(format!(
        "codex-cfg-activity-test-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&temp_dir).expect("failed to create temp dir");

    // Write a config.toml with features section to the temp CODEX_HOME.
    let config_content = r#"
model = "gpt-4o-mini"

[features]
shell_tool = true
"#;
    std::fs::write(temp_dir.join("config.toml"), config_content)
        .expect("failed to write config.toml");

    // Guard to ensure cleanup even if the test panics.
    struct CleanupGuard(std::path::PathBuf);
    impl Drop for CleanupGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _guard = CleanupGuard(temp_dir.clone());

    // Set CODEX_HOME so the worker's load_config activity finds our config.
    let original_codex_home = std::env::var("CODEX_HOME").ok();
    // SAFETY: e2e tests run sequentially in a single thread within e2e_tests_inner.
    unsafe { std::env::set_var("CODEX_HOME", &temp_dir) };

    // Start a workflow — the load_config activity will load config from disk.
    let workflow_id = format!("e2e-cfg-activity-{}", uuid::Uuid::new_v4());
    let base_input = CodexWorkflowInput {
        user_message: String::new(),
        model: "gpt-4o-mini".to_string(),
        instructions: "You are a helpful assistant. Be concise.".to_string(),
        approval_policy: AskForApproval::Never,
        web_search_mode: None,
        reasoning_effort: None,
        reasoning_summary: ReasoningSummary::Auto,
        personality: None,
        developer_instructions: None,
        model_provider: None,
        continued_state: None,
    };
    let session = TemporalAgentSession::new(client.clone(), workflow_id, base_input);

    // Ask the model to use a shell tool — this exercises the full pipeline:
    // load_config activity → config_from_toml in workflow → tool dispatch.
    session
        .submit(user_turn_op_with_model(
            "Run 'echo config-activity-test' in the shell and tell me the output.",
            "gpt-4o-mini",
        ))
        .await
        .expect("submit failed");

    let msg = wait_for_turn_complete(&session).await;
    assert!(
        msg.is_some(),
        "expected agent message after tool execution"
    );
    let response = msg.unwrap();
    assert!(
        response.contains("config-activity-test"),
        "expected shell echo output in response, got: {response}"
    );

    drain_shutdown(&session).await;

    // Restore CODEX_HOME.
    // SAFETY: e2e tests run sequentially in a single thread within e2e_tests_inner.
    unsafe {
        match original_codex_home {
            Some(val) => std::env::set_var("CODEX_HOME", val),
            None => std::env::remove_var("CODEX_HOME"),
        }
    }
}

/// Test: session picker data conversion pipeline.
///
/// 1. Start harness workflow.
/// 2. Register two sessions with the harness.
/// 3. Query harness for running sessions.
/// 4. Convert via `sessions_to_threads_page()`.
/// 5. Verify both sessions appear in the `ThreadsPage` items.
/// 6. Extract session IDs from synthetic PathBufs and verify they match.
async fn session_picker_conversion(client: &Client) {
    // 1. Start harness workflow (idempotent).
    let harness_id = format!("e2e-picker-harness-{}", uuid::Uuid::new_v4());
    let harness_input = HarnessInput {
        continued_state: None,
    };
    let harness_options = WorkflowStartOptions::new(TASK_QUEUE, &harness_id)
        .id_conflict_policy(WorkflowIdConflictPolicy::UseExisting)
        .build();
    client
        .start_workflow(CodexHarness::run, harness_input, harness_options)
        .await
        .expect("failed to start harness workflow");

    let harness_handle = client.get_workflow_handle::<CodexHarnessRun>(&harness_id);

    // 2. Register two sessions.
    let now_millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let session_id_1 = format!("picker-test-{}", uuid::Uuid::new_v4());
    let session_id_2 = format!("picker-test-{}", uuid::Uuid::new_v4());

    let entry1 = SessionEntry {
        session_id: session_id_1.clone(),
        name: Some("Fix the bug".to_string()),
        model: "gpt-4o".to_string(),
        created_at_millis: now_millis,
        status: SessionStatus::Running,
    };
    let entry2 = SessionEntry {
        session_id: session_id_2.clone(),
        name: Some("Add feature".to_string()),
        model: "gpt-4o-mini".to_string(),
        created_at_millis: now_millis + 1000,
        status: SessionStatus::Running,
    };

    harness_handle
        .signal(
            CodexHarness::register_session,
            entry1,
            WorkflowSignalOptions::default(),
        )
        .await
        .expect("register entry1");
    harness_handle
        .signal(
            CodexHarness::register_session,
            entry2,
            WorkflowSignalOptions::default(),
        )
        .await
        .expect("register entry2");

    // 3. Query harness for running sessions.
    // Allow a moment for signals to be processed.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let sessions_json: String = harness_handle
        .query(
            CodexHarness::list_sessions,
            (),
            WorkflowQueryOptions::default(),
        )
        .await
        .expect("query list_sessions");
    let sessions: Vec<SessionEntry> =
        serde_json::from_str(&sessions_json).expect("invalid sessions JSON");
    let running: Vec<SessionEntry> = sessions
        .into_iter()
        .filter(|s| s.status == SessionStatus::Running)
        .collect();

    // 4. Convert via sessions_to_threads_page.
    let page = sessions_to_threads_page(running.clone());

    // 5. Verify both sessions appear.
    let found_1 = page.items.iter().any(|item| {
        extract_session_id_from_path(&item.path).as_deref() == Some(session_id_1.as_str())
    });
    let found_2 = page.items.iter().any(|item| {
        extract_session_id_from_path(&item.path).as_deref() == Some(session_id_2.as_str())
    });
    assert!(found_1, "session_id_1 should appear in ThreadsPage items");
    assert!(found_2, "session_id_2 should appear in ThreadsPage items");

    // 6. Verify no pagination (all in one page).
    assert!(
        page.next_cursor.is_none(),
        "Temporal picker page should have no next_cursor"
    );

    // 7. Verify first_user_message is populated from session name.
    let item_1 = page
        .items
        .iter()
        .find(|item| {
            extract_session_id_from_path(&item.path).as_deref() == Some(session_id_1.as_str())
        })
        .expect("item_1 not found");
    assert_eq!(
        item_1.first_user_message.as_deref(),
        Some("Fix the bug"),
        "first_user_message should come from session name"
    );
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
                .register_workflow::<CodexHarness>()
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

    eprintln!("--- test: session_resume ---");
    session_resume(&client).await;

    eprintln!("--- test: session_picker_conversion ---");
    session_picker_conversion(&client).await;

    eprintln!("--- test: project_context_injected ---");
    project_context_injected(&client).await;

    eprintln!("--- test: config_file_loaded ---");
    config_file_loaded(&client).await;

    eprintln!("--- test: config_activity_loads_features ---");
    config_activity_loads_features(&client).await;

    // --- teardown ---
    drop(client);
    drop(runtime);
    let _ = server.shutdown().await;
}
