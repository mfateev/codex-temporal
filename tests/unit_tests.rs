//! Unit and integration tests for the codex-temporal harness.

use std::sync::Arc;

use codex_temporal::entropy::{TemporalClock, TemporalRandomSource};
use codex_temporal::sink::BufferEventSink;
use codex_temporal::storage::InMemoryStorage;
use codex_temporal::types::{
    ApprovalInput, CodexWorkflowInput, CodexWorkflowOutput, ConfigOutput, HarnessInput,
    HarnessState, ModelCallOutput, PendingApproval, ProjectContextOutput, SessionEntry,
    SessionStatus, ToolExecOutput, UserTurnInput,
};

use codex_core::entropy::{Clock, RandomSource};
use codex_core::{EventSink, StorageBackend};
use codex_protocol::protocol::RolloutItem;
use std::time::SystemTime;

// ---------------------------------------------------------------------------
// Entropy tests
// ---------------------------------------------------------------------------

#[test]
fn temporal_random_source_is_deterministic() {
    let r1 = TemporalRandomSource::new(42);
    let r2 = TemporalRandomSource::new(42);

    let uuids1: Vec<String> = (0..10).map(|_| r1.uuid()).collect();
    let uuids2: Vec<String> = (0..10).map(|_| r2.uuid()).collect();

    assert_eq!(uuids1, uuids2, "same seed must produce same UUIDs");
}

#[test]
fn temporal_random_source_different_seeds_differ() {
    let r1 = TemporalRandomSource::new(42);
    let r2 = TemporalRandomSource::new(99);

    assert_ne!(r1.uuid(), r2.uuid(), "different seeds should produce different UUIDs");
}

#[test]
fn temporal_random_f64_in_range() {
    let r = TemporalRandomSource::new(123);
    for _ in 0..100 {
        let v = r.f64();
        assert!((0.0..1.0).contains(&v), "f64 should be in [0, 1), got {v}");
    }
}

#[test]
fn temporal_clock_wall_time_advances() {
    let epoch = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    let clock = TemporalClock::new(epoch);

    let t1 = clock.wall_time();
    let t2 = clock.wall_time();

    assert!(t2 > t1, "wall_time should advance monotonically");
}

#[test]
fn temporal_clock_unix_millis_reasonable() {
    let epoch = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    let clock = TemporalClock::new(epoch);
    let millis = clock.unix_millis();

    assert!(millis >= 1_700_000_000_000, "should be after epoch, got {millis}");
}

// ---------------------------------------------------------------------------
// EventSink tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn buffer_event_sink_collects_and_drains() {
    let sink = BufferEventSink::new();
    assert!(sink.is_empty());

    // Use a real event type that we can construct.
    use codex_protocol::protocol::{Event, EventMsg, TurnStartedEvent};
    let event = Event {
        id: "test-1".to_string(),
        msg: EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "turn-0".to_string(),
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
        }),
    };
    sink.emit_event(event.clone()).await;
    sink.emit_event(event.clone()).await;

    assert_eq!(sink.len(), 2);

    let drained = sink.drain();
    assert_eq!(drained.len(), 2);
    assert!(sink.is_empty(), "drain should clear the buffer");
}

// ---------------------------------------------------------------------------
// StorageBackend tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn in_memory_storage_saves_and_reads() {
    let storage = InMemoryStorage::new();

    assert!(storage.items().is_empty());

    let item = RolloutItem::Compacted(codex_protocol::protocol::CompactedItem {
        message: "test".to_string(),
        replacement_history: None,
    });
    storage.save(&[item.clone()]).await;

    assert_eq!(storage.items().len(), 1);

    storage.save(&[item.clone(), item.clone()]).await;
    assert_eq!(storage.items().len(), 3);
}

// ---------------------------------------------------------------------------
// ToolExecOutput tests
// ---------------------------------------------------------------------------

#[test]
fn tool_exec_output_to_response_input_item() {
    let output = ToolExecOutput {
        call_id: "call-123".to_string(),
        output: "hello world".to_string(),
        exit_code: 0,
    };

    let item = output.into_response_input_item();
    match item {
        codex_protocol::models::ResponseInputItem::FunctionCallOutput { call_id, output } => {
            assert_eq!(call_id, "call-123");
            assert!(output.success == Some(true));
        }
        other => panic!("expected FunctionCallOutput, got {other:?}"),
    }
}

#[test]
fn tool_exec_output_failure_sets_success_false() {
    let output = ToolExecOutput {
        call_id: "call-456".to_string(),
        output: "error: not found".to_string(),
        exit_code: 1,
    };

    let item = output.into_response_input_item();
    match item {
        codex_protocol::models::ResponseInputItem::FunctionCallOutput { output, .. } => {
            assert!(output.success == Some(false));
        }
        other => panic!("expected FunctionCallOutput, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Workflow I/O serialization tests
// ---------------------------------------------------------------------------

#[test]
fn workflow_input_roundtrips_through_json() {
    let input = CodexWorkflowInput {
        user_message: "Write a hello world program".to_string(),
        model: "gpt-4o".to_string(),
        instructions: "You are a coding assistant.".to_string(),
        approval_policy: Default::default(),
        web_search_mode: None,
        reasoning_effort: None,
        reasoning_summary: codex_protocol::config_types::ReasoningSummary::Auto,
        personality: None,
        developer_instructions: None,
        model_provider: None,
        continued_state: None,
    };

    let json = serde_json::to_string(&input).unwrap();
    let back: CodexWorkflowInput = serde_json::from_str(&json).unwrap();

    assert_eq!(back.user_message, input.user_message);
    assert_eq!(back.model, input.model);
    assert_eq!(back.instructions, input.instructions);
}

#[test]
fn workflow_output_roundtrips_through_json() {
    let output = CodexWorkflowOutput {
        last_agent_message: Some("Hello!".to_string()),
        iterations: 3,
        token_usage: None,
    };

    let json = serde_json::to_string(&output).unwrap();
    let back: CodexWorkflowOutput = serde_json::from_str(&json).unwrap();

    assert_eq!(back.last_agent_message, output.last_agent_message);
    assert_eq!(back.iterations, output.iterations);
    assert!(back.token_usage.is_none());
}

// ---------------------------------------------------------------------------
// Session + TurnContext construction tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn session_new_minimal_creates_usable_session() {
    let config = codex_core::config::ConfigBuilder::default()
        .codex_home(std::env::temp_dir().join("codex-temporal-test"))
        .build()
        .await
        .expect("config");
    let config = Arc::new(config);

    let event_sink: Arc<dyn EventSink> = Arc::new(BufferEventSink::new());
    let storage: Arc<dyn StorageBackend> = Arc::new(InMemoryStorage::new());

    let session = codex_core::Session::new_minimal(
        codex_protocol::ThreadId::new(),
        config,
        event_sink,
        storage,
    )
    .await;

    // Session constructed successfully — the important thing is it doesn't panic.
    drop(session);
}

#[tokio::test]
async fn turn_context_new_minimal_creates_usable_context() {
    let config = codex_core::config::ConfigBuilder::default()
        .codex_home(std::env::temp_dir().join("codex-temporal-test-tc"))
        .build()
        .await
        .expect("config");
    let config = Arc::new(config);

    let model_slug = codex_core::models_manager::manager::ModelsManager::get_model_offline_for_tests(
        config.model.as_deref(),
    );
    let model_info = codex_core::models_manager::manager::ModelsManager::construct_model_info_offline_for_tests(
        &model_slug,
        &config,
    );

    let _tc = codex_core::TurnContext::new_minimal(
        "test-turn".to_string(),
        model_info,
        config,
    );
    // TurnContext constructed successfully — fields are pub(crate) but
    // the important thing is that new_minimal() doesn't panic.
}

// ---------------------------------------------------------------------------
// New signal payload types tests
// ---------------------------------------------------------------------------

#[test]
fn user_turn_input_roundtrips_through_json() {
    use codex_protocol::config_types::{Personality, ReasoningSummary};
    use codex_protocol::openai_models::ReasoningEffort;

    let input = UserTurnInput {
        turn_id: "turn-42".to_string(),
        message: "What is 2+2?".to_string(),
        effort: Some(ReasoningEffort::High),
        summary: ReasoningSummary::Detailed,
        personality: Some(Personality::Friendly),
    };

    let json = serde_json::to_string(&input).unwrap();
    let back: UserTurnInput = serde_json::from_str(&json).unwrap();

    assert_eq!(back.turn_id, "turn-42");
    assert_eq!(back.message, "What is 2+2?");
    assert_eq!(back.effort, Some(ReasoningEffort::High));
    assert_eq!(back.summary, ReasoningSummary::Detailed);
    assert_eq!(back.personality, Some(Personality::Friendly));
}

#[test]
fn approval_input_roundtrips_through_json() {
    let input = ApprovalInput {
        call_id: "call-123".to_string(),
        approved: true,
    };

    let json = serde_json::to_string(&input).unwrap();
    let back: ApprovalInput = serde_json::from_str(&json).unwrap();

    assert_eq!(back.call_id, "call-123");
    assert!(back.approved);
}

#[test]
fn pending_approval_decision_lifecycle() {
    let mut pa = PendingApproval {
        call_id: "call-abc".to_string(),
        decision: None,
    };

    assert!(pa.decision.is_none(), "initially no decision");

    pa.decision = Some(true);
    assert_eq!(pa.decision, Some(true));
}

// ---------------------------------------------------------------------------
// BufferEventSink::events_since tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn buffer_event_sink_events_since_returns_subset() {
    let sink = BufferEventSink::new();

    use codex_protocol::protocol::{Event, EventMsg, TurnStartedEvent};

    // Push 3 events.
    for i in 0..3 {
        let event = Event {
            id: format!("ev-{i}"),
            msg: EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: format!("turn-{i}"),
                model_context_window: None,
                collaboration_mode_kind: Default::default(),
            }),
        };
        sink.emit_event(event).await;
    }

    // events_since(0) should return all 3.
    let (events, watermark) = sink.events_since(0);
    assert_eq!(events.len(), 3);
    assert_eq!(watermark, 3);

    // events_since(2) should return only the last one.
    let (events, watermark) = sink.events_since(2);
    assert_eq!(events.len(), 1);
    assert_eq!(watermark, 3);

    // events_since(3) should return empty.
    let (events, watermark) = sink.events_since(3);
    assert!(events.is_empty());
    assert_eq!(watermark, 3);

    // events_since(100) should return empty.
    let (events, watermark) = sink.events_since(100);
    assert!(events.is_empty());
    assert_eq!(watermark, 3);

    // Verify events are valid JSON and can be deserialized back.
    let (events, _) = sink.events_since(0);
    for json_str in &events {
        let event: Event = serde_json::from_str(json_str)
            .expect("events_since should return valid JSON");
        assert!(event.id.starts_with("ev-"));
    }
}

#[tokio::test]
async fn buffer_event_sink_emit_event_sync_works() {
    let sink = BufferEventSink::new();

    use codex_protocol::protocol::{Event, EventMsg};

    sink.emit_event_sync(Event {
        id: "sync-1".to_string(),
        msg: EventMsg::ShutdownComplete,
    });

    assert_eq!(sink.len(), 1);

    let (events, watermark) = sink.events_since(0);
    assert_eq!(events.len(), 1);
    assert_eq!(watermark, 1);
}

// ---------------------------------------------------------------------------
// Approval policy tests
// ---------------------------------------------------------------------------

use codex_protocol::protocol::AskForApproval;

#[test]
fn workflow_input_approval_policy_defaults_to_on_request() {
    // Deserializing JSON without an approval_policy field should default to
    // OnRequest (the codex default).
    let json = r#"{"user_message":"hi","model":"gpt-4o","instructions":"test"}"#;
    let input: CodexWorkflowInput = serde_json::from_str(json).unwrap();
    assert!(
        matches!(input.approval_policy, AskForApproval::OnRequest),
        "expected OnRequest, got {:?}",
        input.approval_policy,
    );
}

#[test]
fn workflow_input_approval_policy_never_roundtrips() {
    let input = CodexWorkflowInput {
        user_message: "test".to_string(),
        model: "gpt-4o".to_string(),
        instructions: "test".to_string(),
        approval_policy: AskForApproval::Never,
        web_search_mode: None,
        reasoning_effort: None,
        reasoning_summary: codex_protocol::config_types::ReasoningSummary::Auto,
        personality: None,
        developer_instructions: None,
        model_provider: None,
        continued_state: None,
    };
    let json = serde_json::to_string(&input).unwrap();
    let back: CodexWorkflowInput = serde_json::from_str(&json).unwrap();
    assert!(matches!(back.approval_policy, AskForApproval::Never));
}

#[test]
fn workflow_input_approval_policy_untrusted_roundtrips() {
    let input = CodexWorkflowInput {
        user_message: "test".to_string(),
        model: "gpt-4o".to_string(),
        instructions: "test".to_string(),
        approval_policy: AskForApproval::UnlessTrusted,
        web_search_mode: None,
        reasoning_effort: None,
        reasoning_summary: codex_protocol::config_types::ReasoningSummary::Auto,
        personality: None,
        developer_instructions: None,
        model_provider: None,
        continued_state: None,
    };
    let json = serde_json::to_string(&input).unwrap();
    let back: CodexWorkflowInput = serde_json::from_str(&json).unwrap();
    assert!(matches!(back.approval_policy, AskForApproval::UnlessTrusted));
}

#[test]
fn workflow_input_web_search_mode_defaults_to_none() {
    let json = r#"{"user_message":"hi","model":"gpt-4o","instructions":"test"}"#;
    let input: CodexWorkflowInput = serde_json::from_str(json).unwrap();
    assert!(
        input.web_search_mode.is_none(),
        "expected None, got {:?}",
        input.web_search_mode,
    );
}

#[test]
fn workflow_input_web_search_mode_live_roundtrips() {
    use codex_protocol::config_types::WebSearchMode;

    let input = CodexWorkflowInput {
        user_message: "test".to_string(),
        model: "gpt-4o".to_string(),
        instructions: "test".to_string(),
        approval_policy: AskForApproval::Never,
        web_search_mode: Some(WebSearchMode::Live),
        reasoning_effort: None,
        reasoning_summary: codex_protocol::config_types::ReasoningSummary::Auto,
        personality: None,
        developer_instructions: None,
        model_provider: None,
        continued_state: None,
    };
    let json = serde_json::to_string(&input).unwrap();
    let back: CodexWorkflowInput = serde_json::from_str(&json).unwrap();
    assert!(
        matches!(back.web_search_mode, Some(WebSearchMode::Live)),
        "expected Some(Live), got {:?}",
        back.web_search_mode,
    );
}

#[test]
fn workflow_input_reasoning_effort_roundtrips() {
    use codex_protocol::config_types::{Personality, ReasoningSummary};
    use codex_protocol::openai_models::ReasoningEffort;

    let input = CodexWorkflowInput {
        user_message: "test".to_string(),
        model: "gpt-4o".to_string(),
        instructions: "test".to_string(),
        approval_policy: AskForApproval::Never,
        web_search_mode: None,
        reasoning_effort: Some(ReasoningEffort::Low),
        reasoning_summary: ReasoningSummary::Concise,
        personality: Some(Personality::Pragmatic),
        developer_instructions: None,
        model_provider: None,
        continued_state: None,
    };
    let json = serde_json::to_string(&input).unwrap();
    let back: CodexWorkflowInput = serde_json::from_str(&json).unwrap();

    assert_eq!(back.reasoning_effort, Some(ReasoningEffort::Low));
    assert_eq!(back.reasoning_summary, ReasoningSummary::Concise);
    assert_eq!(back.personality, Some(Personality::Pragmatic));
}

#[test]
fn workflow_input_reasoning_fields_default_when_missing() {
    // JSON without the new fields should deserialize with defaults.
    let json = r#"{"user_message":"hi","model":"gpt-4o","instructions":"test"}"#;
    let input: CodexWorkflowInput = serde_json::from_str(json).unwrap();
    assert!(input.reasoning_effort.is_none());
    assert_eq!(
        input.reasoning_summary,
        codex_protocol::config_types::ReasoningSummary::Auto
    );
    assert!(input.personality.is_none());
    assert!(input.continued_state.is_none());
}

#[test]
fn continue_as_new_state_roundtrips_through_json() {
    use codex_protocol::protocol::{RolloutItem, TokenUsage};
    use codex_protocol::models::ResponseItem;
    use codex_temporal::types::{ContinueAsNewState, UserTurnInput};

    let state = ContinueAsNewState {
        rollout_items: vec![RolloutItem::ResponseItem(ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![],
            end_turn: None,
            phase: None,
        })],
        pending_user_turns: vec![UserTurnInput {
            turn_id: "turn-5".to_string(),
            message: "hello".to_string(),
            effort: None,
            summary: codex_protocol::config_types::ReasoningSummary::Auto,
            personality: None,
        }],
        cumulative_turn_count: 5,
        cumulative_iterations: 42,
        cumulative_token_usage: Some(TokenUsage {
            input_tokens: 100,
            cached_input_tokens: 50,
            output_tokens: 25,
            reasoning_output_tokens: 0,
            total_tokens: 125,
        }),
    };

    let json = serde_json::to_string(&state).unwrap();
    let back: ContinueAsNewState = serde_json::from_str(&json).unwrap();

    assert_eq!(back.rollout_items.len(), 1);
    assert_eq!(back.pending_user_turns.len(), 1);
    assert_eq!(back.pending_user_turns[0].turn_id, "turn-5");
    assert_eq!(back.cumulative_turn_count, 5);
    assert_eq!(back.cumulative_iterations, 42);
    assert!(back.cumulative_token_usage.is_some());
}

#[test]
fn workflow_input_with_continued_state_roundtrips() {
    use codex_temporal::types::ContinueAsNewState;

    let input = CodexWorkflowInput {
        user_message: String::new(),
        model: "gpt-4o".to_string(),
        instructions: "test".to_string(),
        approval_policy: Default::default(),
        web_search_mode: None,
        reasoning_effort: None,
        reasoning_summary: codex_protocol::config_types::ReasoningSummary::Auto,
        personality: None,
        developer_instructions: None,
        model_provider: None,
        continued_state: Some(ContinueAsNewState {
            rollout_items: vec![],
            pending_user_turns: vec![],
            cumulative_turn_count: 3,
            cumulative_iterations: 10,
            cumulative_token_usage: None,
        }),
    };

    let json = serde_json::to_string(&input).unwrap();
    let back: CodexWorkflowInput = serde_json::from_str(&json).unwrap();

    assert!(back.continued_state.is_some());
    let state = back.continued_state.unwrap();
    assert_eq!(state.cumulative_turn_count, 3);
    assert_eq!(state.cumulative_iterations, 10);
}

#[test]
fn is_known_safe_command_classifies_read_only_commands() {
    use codex_shell_command::is_safe_command::is_known_safe_command;

    // Safe commands should be auto-approved under UnlessTrusted.
    assert!(is_known_safe_command(&["ls".to_string()]));
    assert!(is_known_safe_command(&["cat".to_string(), "foo.txt".to_string()]));
    assert!(is_known_safe_command(&["pwd".to_string()]));
    assert!(is_known_safe_command(&["whoami".to_string()]));

    // Unsafe commands should require approval under UnlessTrusted.
    assert!(!is_known_safe_command(&["rm".to_string(), "-rf".to_string(), "/".to_string()]));
    assert!(!is_known_safe_command(&["curl".to_string(), "https://example.com".to_string()]));
    assert!(!is_known_safe_command(&["python".to_string(), "script.py".to_string()]));
}

// ---------------------------------------------------------------------------
// Token usage serialization tests
// ---------------------------------------------------------------------------

#[test]
fn model_call_output_with_token_usage_roundtrips() {
    use codex_protocol::protocol::TokenUsage;

    let output = ModelCallOutput {
        items: vec![],
        token_usage: Some(TokenUsage {
            input_tokens: 100,
            cached_input_tokens: 50,
            output_tokens: 30,
            reasoning_output_tokens: 0,
            total_tokens: 130,
        }),
    };

    let json = serde_json::to_string(&output).unwrap();
    let back: ModelCallOutput = serde_json::from_str(&json).unwrap();

    let usage = back.token_usage.expect("token_usage should be Some");
    assert_eq!(usage.input_tokens, 100);
    assert_eq!(usage.cached_input_tokens, 50);
    assert_eq!(usage.output_tokens, 30);
    assert_eq!(usage.total_tokens, 130);
}

#[test]
fn model_call_output_token_usage_defaults_to_none() {
    // JSON without token_usage should deserialize to None (backward compat).
    let json = r#"{"items":[]}"#;
    let output: ModelCallOutput = serde_json::from_str(json).unwrap();
    assert!(
        output.token_usage.is_none(),
        "expected None, got {:?}",
        output.token_usage
    );
}

#[test]
fn workflow_output_with_token_usage_roundtrips() {
    use codex_protocol::protocol::TokenUsage;

    let output = CodexWorkflowOutput {
        last_agent_message: Some("Done".to_string()),
        iterations: 2,
        token_usage: Some(TokenUsage {
            input_tokens: 200,
            cached_input_tokens: 150,
            output_tokens: 60,
            reasoning_output_tokens: 10,
            total_tokens: 270,
        }),
    };

    let json = serde_json::to_string(&output).unwrap();
    let back: CodexWorkflowOutput = serde_json::from_str(&json).unwrap();

    let usage = back.token_usage.expect("token_usage should be Some");
    assert_eq!(usage.input_tokens, 200);
    assert_eq!(usage.cached_input_tokens, 150);
    assert_eq!(usage.output_tokens, 60);
    assert_eq!(usage.reasoning_output_tokens, 10);
    assert_eq!(usage.total_tokens, 270);
}

#[test]
fn workflow_output_token_usage_defaults_to_none() {
    // JSON without token_usage should deserialize to None (backward compat).
    let json = r#"{"last_agent_message":"hi","iterations":1}"#;
    let output: CodexWorkflowOutput = serde_json::from_str(json).unwrap();
    assert!(
        output.token_usage.is_none(),
        "expected None, got {:?}",
        output.token_usage
    );
}

// ---------------------------------------------------------------------------
// Tool dispatch integration tests
// ---------------------------------------------------------------------------
// These exercise the full build_specs → ToolRegistry::dispatch pipeline
// without a Temporal worker or model API call.

use codex_temporal::activities::dispatch_tool;
use codex_temporal::types::ToolExecInput;

/// Default TOML config for tests that need unrestricted shell access.
const FULL_ACCESS_TOML: &str = "model = \"gpt-4o\"\nsandbox_mode = \"danger-full-access\"\n";

/// Helper to build a ToolExecInput for a given tool.
fn tool_input(tool_name: &str, arguments: &str) -> ToolExecInput {
    ToolExecInput {
        tool_name: tool_name.to_string(),
        call_id: format!("test-call-{}", uuid::Uuid::new_v4()),
        arguments: arguments.to_string(),
        model: "gpt-4o".to_string(),
        cwd: "/tmp".to_string(),
        config_toml: Some(FULL_ACCESS_TOML.to_string()),
    }
}

#[tokio::test]
async fn dispatch_shell_echo() {
    let input = tool_input(
        "shell",
        r#"{"command":["echo","hello from dispatch"]}"#,
    );

    let output = dispatch_tool(input).await.expect("dispatch_tool failed");

    assert_eq!(output.exit_code, 0, "echo should succeed: {}", output.output);
    assert!(
        output.output.contains("hello from dispatch"),
        "output should contain the echoed text, got: {}",
        output.output,
    );
}

#[tokio::test]
async fn dispatch_shell_failing_command() {
    let input = tool_input(
        "shell",
        r#"{"command":["false"]}"#,
    );

    let output = dispatch_tool(input).await.expect("dispatch_tool failed");

    assert_ne!(output.exit_code, 0, "`false` should return non-zero exit code");
}

#[tokio::test]
async fn dispatch_unknown_tool_returns_error() {
    let input = tool_input(
        "nonexistent_tool",
        r#"{"foo":"bar"}"#,
    );

    let output = dispatch_tool(input).await.expect("dispatch_tool failed");

    // The registry returns a dispatch error for unknown tools.
    assert_eq!(output.exit_code, 1);
    assert!(
        output.output.contains("unsupported") || output.output.contains("error"),
        "expected error message for unknown tool, got: {}",
        output.output,
    );
}

#[tokio::test]
async fn dispatch_shell_with_cwd() {
    let input = ToolExecInput {
        tool_name: "shell".to_string(),
        call_id: "test-cwd".to_string(),
        arguments: r#"{"command":["pwd"]}"#.to_string(),
        model: "gpt-4o".to_string(),
        cwd: "/tmp".to_string(),
        config_toml: Some(FULL_ACCESS_TOML.to_string()),
    };

    let output = dispatch_tool(input).await.expect("dispatch_tool failed");

    assert_eq!(output.exit_code, 0, "pwd should succeed: {}", output.output);
    // The shell handler should respect the configured cwd.
    assert!(
        output.output.contains("/tmp"),
        "pwd output should contain /tmp, got: {}",
        output.output,
    );
}

/// Verify that `dispatch_tool` respects the sandbox policy from config TOML
/// instead of overriding it with `DangerFullAccess`.
///
/// We pass `sandbox_mode = "read-only"` via `config_toml` and attempt a write
/// command. The sandbox should block it, producing a non-zero exit code.
#[tokio::test]
async fn dispatch_tool_respects_config_sandbox_policy() {
    // Build a TOML string that sets sandbox_mode to read-only.
    let toml_str = r#"
model = "gpt-4o"
sandbox_mode = "read-only"
"#;

    let input = ToolExecInput {
        tool_name: "shell".to_string(),
        call_id: format!("test-sandbox-{}", uuid::Uuid::new_v4()),
        arguments: r#"{"command":["touch","/tmp/codex-sandbox-test-file"]}"#.to_string(),
        model: "gpt-4o".to_string(),
        cwd: "/tmp".to_string(),
        config_toml: Some(toml_str.to_string()),
    };

    let output = dispatch_tool(input).await.expect("dispatch_tool failed");

    // With read-only sandbox, a write command should be blocked.
    assert_ne!(
        output.exit_code, 0,
        "write command should fail under read-only sandbox, got output: {}",
        output.output,
    );
}

// ---------------------------------------------------------------------------
// Registry-level tests for tools that need experimental_supported_tools
// ---------------------------------------------------------------------------

use codex_core::{
    Session, TurnContext, TurnDiffTracker,
    ToolInvocation, ToolPayload, ToolsConfig, ToolsConfigParams, build_specs,
};
use codex_core::models_manager::manager::ModelsManager;
use codex_protocol::ThreadId;
use tokio::sync::Mutex;

/// Build a ToolsConfig that enables read_file, list_dir, grep_files via
/// `experimental_supported_tools`, then dispatch a tool invocation through
/// the resulting registry.
async fn dispatch_with_experimental_tools(
    tool_name: &str,
    arguments: &str,
    extra_tools: &[&str],
) -> (String, i32) {
    let codex_home = std::path::PathBuf::from("/tmp/codex-temporal");
    let mut config = codex_core::config::Config::for_harness(codex_home).unwrap();
    config.model = Some("gpt-4o".to_string());
    config.cwd = std::path::PathBuf::from("/tmp");
    let config = Arc::new(config);

    let model_slug = ModelsManager::get_model_offline_for_tests(config.model.as_deref());
    let mut model_info =
        ModelsManager::construct_model_info_offline_for_tests(&model_slug, &config);

    // Inject experimental tools into model_info so ToolsConfig picks them up.
    model_info.experimental_supported_tools = extra_tools.iter().map(|s| s.to_string()).collect();

    let tools_config = ToolsConfig::new(&ToolsConfigParams {
        model_info: &model_info,
        features: &config.features,
        web_search_mode: None,
    });
    let builder = build_specs(&tools_config, None, None, &[]);
    let (_specs, registry) = builder.build();

    let conversation_id = ThreadId::new();
    let event_sink: Arc<dyn codex_core::EventSink> = Arc::new(BufferEventSink::new());
    let storage: Arc<dyn codex_core::StorageBackend> = Arc::new(InMemoryStorage::new());

    let session = Session::new_minimal(
        conversation_id,
        Arc::clone(&config),
        event_sink,
        storage,
    )
    .await;

    let turn_context = Arc::new(TurnContext::new_minimal(
        "test".to_string(),
        model_info,
        Arc::clone(&config),
    ));

    let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));

    let invocation = ToolInvocation {
        session,
        turn: turn_context,
        tracker,
        call_id: "test-call".to_string(),
        tool_name: tool_name.to_string(),
        payload: ToolPayload::Function {
            arguments: arguments.to_string(),
        },
    };

    match registry.dispatch(invocation).await {
        Ok(item) => {
            // Extract text from the response item.
            match &item {
                codex_protocol::models::ResponseInputItem::FunctionCallOutput { output, .. } => {
                    let text = output.body.to_text().unwrap_or_default();
                    let success = output.success.unwrap_or(true);
                    (text, if success { 0 } else { 1 })
                }
                codex_protocol::models::ResponseInputItem::CustomToolCallOutput {
                    output, ..
                } => (output.clone(), 0),
                other => (format!("{other:?}"), 0),
            }
        }
        Err(e) => (format!("dispatch error: {e}"), 1),
    }
}

#[tokio::test]
async fn dispatch_read_file_reads_etc_hostname() {
    let (output, exit_code) = dispatch_with_experimental_tools(
        "read_file",
        r#"{"file_path":"/etc/hostname"}"#,
        &["read_file"],
    )
    .await;

    assert_eq!(exit_code, 0, "read_file should succeed: {output}");
    // /etc/hostname should contain some text (the container hostname).
    assert!(
        !output.is_empty(),
        "read_file output should not be empty",
    );
}

#[tokio::test]
async fn dispatch_list_dir_lists_tmp() {
    let (output, exit_code) = dispatch_with_experimental_tools(
        "list_dir",
        r#"{"dir_path":"/tmp"}"#,
        &["list_dir"],
    )
    .await;

    assert_eq!(exit_code, 0, "list_dir should succeed: {output}");
    assert!(
        !output.is_empty(),
        "list_dir output should not be empty",
    );
}

#[tokio::test]
async fn dispatch_read_file_nonexistent_returns_error() {
    let (output, exit_code) = dispatch_with_experimental_tools(
        "read_file",
        r#"{"file_path":"/tmp/this-file-does-not-exist-xyz-123"}"#,
        &["read_file"],
    )
    .await;

    // Should report an error (either exit_code=1 or error text in output).
    assert!(
        exit_code != 0 || output.to_lowercase().contains("error") || output.to_lowercase().contains("no such file"),
        "expected error for nonexistent file, got exit_code={exit_code}, output: {output}",
    );
}

// ---------------------------------------------------------------------------
// Harness type serde tests
// ---------------------------------------------------------------------------

#[test]
fn session_entry_roundtrips_through_json() {
    let entry = SessionEntry {
        session_id: "codex-session-abc123".to_string(),
        name: Some("My session".to_string()),
        model: "gpt-4o".to_string(),
        created_at_millis: 1_700_000_000_000,
        status: SessionStatus::Running,
    };

    let json = serde_json::to_string(&entry).unwrap();
    let back: SessionEntry = serde_json::from_str(&json).unwrap();

    assert_eq!(back.session_id, "codex-session-abc123");
    assert_eq!(back.name, Some("My session".to_string()));
    assert_eq!(back.model, "gpt-4o");
    assert_eq!(back.created_at_millis, 1_700_000_000_000);
    assert_eq!(back.status, SessionStatus::Running);
}

#[test]
fn session_entry_status_variants_roundtrip() {
    for status in [
        SessionStatus::Running,
        SessionStatus::Completed,
        SessionStatus::Failed,
    ] {
        let entry = SessionEntry {
            session_id: "s1".to_string(),
            name: None,
            model: "gpt-4o-mini".to_string(),
            created_at_millis: 0,
            status,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: SessionEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.status, status, "status {status:?} should roundtrip");
    }
}

#[test]
fn harness_state_roundtrips() {
    let state = HarnessState {
        sessions: vec![
            SessionEntry {
                session_id: "s1".to_string(),
                name: Some("first".to_string()),
                model: "gpt-4o".to_string(),
                created_at_millis: 100,
                status: SessionStatus::Running,
            },
            SessionEntry {
                session_id: "s2".to_string(),
                name: None,
                model: "gpt-4o-mini".to_string(),
                created_at_millis: 200,
                status: SessionStatus::Completed,
            },
        ],
    };

    let json = serde_json::to_string(&state).unwrap();
    let back: HarnessState = serde_json::from_str(&json).unwrap();

    assert_eq!(back.sessions.len(), 2);
    assert_eq!(back.sessions[0].session_id, "s1");
    assert_eq!(back.sessions[1].status, SessionStatus::Completed);
}

#[test]
fn harness_input_defaults_when_empty() {
    let json = r#"{}"#;
    let input: HarnessInput = serde_json::from_str(json).unwrap();
    assert!(
        input.continued_state.is_none(),
        "continued_state should default to None"
    );
}

// ---------------------------------------------------------------------------
// ProjectContextOutput serde tests
// ---------------------------------------------------------------------------

#[test]
fn project_context_output_roundtrips() {
    use codex_protocol::protocol::GitInfo;

    let ctx = ProjectContextOutput {
        cwd: "/home/user/project".to_string(),
        user_instructions: Some("Do not modify tests.".to_string()),
        git_info: Some(GitInfo {
            commit_hash: Some("abc123".to_string()),
            branch: Some("main".to_string()),
            repository_url: Some("https://github.com/user/project".to_string()),
        }),
    };

    let json = serde_json::to_string(&ctx).unwrap();
    let back: ProjectContextOutput = serde_json::from_str(&json).unwrap();

    assert_eq!(back.cwd, "/home/user/project");
    assert_eq!(back.user_instructions.as_deref(), Some("Do not modify tests."));
    let git = back.git_info.unwrap();
    assert_eq!(git.commit_hash.as_deref(), Some("abc123"));
    assert_eq!(git.branch.as_deref(), Some("main"));
    assert_eq!(git.repository_url.as_deref(), Some("https://github.com/user/project"));
}

#[test]
fn project_context_output_defaults() {
    // Missing optional fields should default to None (backward compat).
    let json = r#"{"cwd":"/tmp"}"#;
    let ctx: ProjectContextOutput = serde_json::from_str(json).unwrap();

    assert_eq!(ctx.cwd, "/tmp");
    assert!(ctx.user_instructions.is_none());
    assert!(ctx.git_info.is_none());
}

// ---------------------------------------------------------------------------
// build_context_items tests
// ---------------------------------------------------------------------------

use codex_temporal::workflow::build_context_items;

#[test]
fn build_context_items_with_instructions() {
    let ctx = ProjectContextOutput {
        cwd: "/home/user/project".to_string(),
        user_instructions: Some("Always use cargo test.".to_string()),
        git_info: None,
    };

    let items = build_context_items(&ctx);

    // Should have 2 items: user instructions + environment context.
    assert_eq!(items.len(), 2, "expected 2 context items, got {}", items.len());

    // First item: user instructions.
    let text = extract_message_text(&items[0]);
    assert!(
        text.starts_with("# AGENTS.md instructions for /home/user/project"),
        "user instructions item should start with prefix, got: {text}"
    );
    assert!(
        text.contains("Always use cargo test."),
        "user instructions item should contain the instructions text"
    );
    assert!(
        text.contains("<INSTRUCTIONS>") && text.contains("</INSTRUCTIONS>"),
        "user instructions item should be wrapped in INSTRUCTIONS tags"
    );

    // Second item: environment context.
    let env_text = extract_message_text(&items[1]);
    assert!(
        env_text.contains("<environment_context>"),
        "env context should have open tag"
    );
    assert!(
        env_text.contains("<cwd>/home/user/project</cwd>"),
        "env context should contain cwd"
    );
    assert!(
        env_text.contains("<shell>bash</shell>"),
        "env context should contain shell"
    );
}

#[test]
fn build_context_items_without_instructions() {
    let ctx = ProjectContextOutput {
        cwd: "/tmp".to_string(),
        user_instructions: None,
        git_info: None,
    };

    let items = build_context_items(&ctx);

    // Should have only 1 item: environment context (no user instructions).
    assert_eq!(items.len(), 1, "expected 1 context item (env only), got {}", items.len());

    let env_text = extract_message_text(&items[0]);
    assert!(
        env_text.contains("<cwd>/tmp</cwd>"),
        "env context should contain cwd"
    );
}

#[test]
fn build_context_items_with_git_info() {
    use codex_protocol::protocol::GitInfo;

    let ctx = ProjectContextOutput {
        cwd: "/repo".to_string(),
        user_instructions: None,
        git_info: Some(GitInfo {
            commit_hash: Some("deadbeef".to_string()),
            branch: Some("feature/test".to_string()),
            repository_url: Some("https://github.com/org/repo".to_string()),
        }),
    };

    let items = build_context_items(&ctx);
    assert_eq!(items.len(), 1, "expected 1 context item (env only), got {}", items.len());

    let env_text = extract_message_text(&items[0]);
    assert!(
        env_text.contains("<git>"),
        "env context should contain git section"
    );
    assert!(
        env_text.contains("<branch>feature/test</branch>"),
        "env context should contain branch"
    );
    assert!(
        env_text.contains("<commit>deadbeef</commit>"),
        "env context should contain commit"
    );
    assert!(
        env_text.contains("<repository_url>https://github.com/org/repo</repository_url>"),
        "env context should contain repository_url"
    );
}

/// Helper to extract the text content from a ResponseItem::Message.
fn extract_message_text(item: &codex_protocol::models::ResponseItem) -> String {
    match item {
        codex_protocol::models::ResponseItem::Message { content, .. } => {
            content
                .iter()
                .filter_map(|c| match c {
                    codex_protocol::models::ContentItem::InputText { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        other => panic!("expected ResponseItem::Message, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Config-loading new fields serde tests
// ---------------------------------------------------------------------------

use codex_temporal::types::ModelCallInput;

#[test]
fn workflow_input_developer_instructions_roundtrips() {
    let input = CodexWorkflowInput {
        user_message: "test".to_string(),
        model: "gpt-4o".to_string(),
        instructions: "test".to_string(),
        approval_policy: Default::default(),
        web_search_mode: None,
        reasoning_effort: None,
        reasoning_summary: codex_protocol::config_types::ReasoningSummary::Auto,
        personality: None,
        developer_instructions: Some("Always respond in JSON format.".to_string()),
        model_provider: None,
        continued_state: None,
    };

    let json = serde_json::to_string(&input).unwrap();
    let back: CodexWorkflowInput = serde_json::from_str(&json).unwrap();

    assert_eq!(
        back.developer_instructions.as_deref(),
        Some("Always respond in JSON format."),
    );
}

#[test]
fn workflow_input_model_provider_roundtrips() {
    use codex_core::ModelProviderInfo;

    let provider = ModelProviderInfo {
        name: "test-provider".to_string(),
        base_url: Some("https://api.example.com/v1".to_string()),
        env_key: Some("TEST_API_KEY".to_string()),
        env_key_instructions: None,
        experimental_bearer_token: None,
        wire_api: Default::default(),
        query_params: None,
        http_headers: None,
        env_http_headers: None,
        request_max_retries: None,
        stream_max_retries: None,
        stream_idle_timeout_ms: None,
        requires_openai_auth: false,
        supports_websockets: false,
    };

    let input = CodexWorkflowInput {
        user_message: "test".to_string(),
        model: "gpt-4o".to_string(),
        instructions: "test".to_string(),
        approval_policy: Default::default(),
        web_search_mode: None,
        reasoning_effort: None,
        reasoning_summary: codex_protocol::config_types::ReasoningSummary::Auto,
        personality: None,
        developer_instructions: None,
        model_provider: Some(provider.clone()),
        continued_state: None,
    };

    let json = serde_json::to_string(&input).unwrap();
    let back: CodexWorkflowInput = serde_json::from_str(&json).unwrap();

    let back_provider = back.model_provider.expect("model_provider should be Some");
    assert_eq!(back_provider.name, "test-provider");
    assert_eq!(back_provider.base_url.as_deref(), Some("https://api.example.com/v1"));
    assert_eq!(back_provider.env_key.as_deref(), Some("TEST_API_KEY"));
}

#[test]
fn workflow_input_backward_compat_no_provider() {
    // JSON without the new fields should deserialize with None defaults.
    let json = r#"{"user_message":"hi","model":"gpt-4o","instructions":"test"}"#;
    let input: CodexWorkflowInput = serde_json::from_str(json).unwrap();

    assert!(
        input.developer_instructions.is_none(),
        "developer_instructions should default to None"
    );
    assert!(
        input.model_provider.is_none(),
        "model_provider should default to None"
    );
}

#[test]
fn model_call_input_provider_roundtrips() {
    use codex_core::ModelProviderInfo;

    let provider = ModelProviderInfo {
        name: "custom-provider".to_string(),
        base_url: Some("https://custom.api/v1".to_string()),
        env_key: None,
        env_key_instructions: None,
        experimental_bearer_token: None,
        wire_api: Default::default(),
        query_params: None,
        http_headers: None,
        env_http_headers: None,
        request_max_retries: None,
        stream_max_retries: None,
        stream_idle_timeout_ms: None,
        requires_openai_auth: false,
        supports_websockets: false,
    };

    let input = ModelCallInput {
        conversation_id: "conv-1".to_string(),
        input: vec![],
        tools: vec![],
        parallel_tool_calls: false,
        instructions: "test".to_string(),
        model_info: codex_core::models_manager::manager::ModelsManager::construct_model_info_offline_for_tests(
            "gpt-4o",
            &codex_core::config::Config::for_harness(std::path::PathBuf::from("/tmp/codex-test")).unwrap(),
        ),
        effort: None,
        summary: codex_protocol::config_types::ReasoningSummary::Auto,
        personality: None,
        provider: Some(provider),
    };

    let json = serde_json::to_string(&input).unwrap();
    let back: ModelCallInput = serde_json::from_str(&json).unwrap();

    let back_provider = back.provider.expect("provider should be Some");
    assert_eq!(back_provider.name, "custom-provider");
    assert_eq!(back_provider.base_url.as_deref(), Some("https://custom.api/v1"));
    assert!(back_provider.env_key.is_none());
}

#[test]
fn model_call_input_provider_defaults_to_none() {
    // Serialize a ModelCallInput without provider, then deserialize to verify
    // the field defaults to None when absent.
    let input = ModelCallInput {
        conversation_id: "c".to_string(),
        input: vec![],
        tools: vec![],
        parallel_tool_calls: false,
        instructions: "t".to_string(),
        model_info: codex_core::models_manager::manager::ModelsManager::construct_model_info_offline_for_tests(
            "gpt-4o",
            &codex_core::config::Config::for_harness(std::path::PathBuf::from("/tmp/codex-test")).unwrap(),
        ),
        effort: None,
        summary: codex_protocol::config_types::ReasoningSummary::Auto,
        personality: None,
        provider: None,
    };

    let json = serde_json::to_string(&input).unwrap();
    // The "provider" field should be absent due to skip_serializing_if.
    assert!(
        !json.contains("\"provider\""),
        "provider:None should be skipped in serialization, got: {json}"
    );

    let back: ModelCallInput = serde_json::from_str(&json).unwrap();
    assert!(
        back.provider.is_none(),
        "provider should default to None, got {:?}",
        back.provider,
    );
}

// ---------------------------------------------------------------------------
// Config activity tests
// ---------------------------------------------------------------------------

#[test]
fn config_output_roundtrips() {
    let original = ConfigOutput {
        config_toml: "model = \"gpt-4o\"\napproval_policy = \"on-request\"\n".to_string(),
    };
    let json = serde_json::to_string(&original).unwrap();
    let back: ConfigOutput = serde_json::from_str(&json).unwrap();
    assert_eq!(back.config_toml, original.config_toml);
}

#[test]
fn tool_exec_input_config_toml_roundtrip() {
    // With config_toml present.
    let input_with = ToolExecInput {
        tool_name: "shell".to_string(),
        call_id: "c1".to_string(),
        arguments: "{}".to_string(),
        model: "gpt-4o".to_string(),
        cwd: "/tmp".to_string(),
        config_toml: Some("model = \"gpt-4o\"\n".to_string()),
    };
    let json_with = serde_json::to_string(&input_with).unwrap();
    assert!(
        json_with.contains("config_toml"),
        "config_toml should be serialized when Some"
    );
    let back_with: ToolExecInput = serde_json::from_str(&json_with).unwrap();
    assert_eq!(back_with.config_toml, input_with.config_toml);

    // With config_toml absent (backward compat).
    let input_none = ToolExecInput {
        tool_name: "shell".to_string(),
        call_id: "c2".to_string(),
        arguments: "{}".to_string(),
        model: "gpt-4o".to_string(),
        cwd: "/tmp".to_string(),
        config_toml: None,
    };
    let json_none = serde_json::to_string(&input_none).unwrap();
    assert!(
        !json_none.contains("config_toml"),
        "config_toml:None should be skipped in serialization"
    );

    // Deserializing old-format JSON without config_toml should default to None.
    let old_json = r#"{"tool_name":"shell","call_id":"c3","arguments":"{}","model":"gpt-4o","cwd":"/tmp"}"#;
    let back_old: ToolExecInput = serde_json::from_str(old_json).unwrap();
    assert!(
        back_old.config_toml.is_none(),
        "missing config_toml should default to None"
    );
}

#[test]
fn config_from_toml_constructs_features() {
    use codex_core::features::Feature;
    use codex_temporal::config_loader::config_from_toml;

    // Build a Config from a minimal TOML string.
    let toml_str = r#"
model = "gpt-4o"
approval_policy = "never"
"#;
    let config = config_from_toml(toml_str, std::path::Path::new("/tmp"), None)
        .expect("config_from_toml should succeed");

    // Model should be set from the TOML.
    assert_eq!(
        config.model.as_deref(),
        Some("gpt-4o"),
        "model should be set from TOML"
    );

    // The ShellTool feature should be enabled by default — this confirms
    // that Features were properly constructed (not empty).
    assert!(
        config.features.enabled(Feature::ShellTool),
        "ShellTool feature should be enabled by default"
    );
}

// ---------------------------------------------------------------------------
// Command safety three-tier classification tests
// ---------------------------------------------------------------------------

use codex_shell_command::is_dangerous_command::command_might_be_dangerous;
use codex_shell_command::is_safe_command::is_known_safe_command;

/// Mirror of the three-tier `needs_approval` logic in `TemporalToolHandler`.
fn needs_approval(command: &[String], policy: AskForApproval) -> bool {
    if is_known_safe_command(command) {
        false
    } else if command_might_be_dangerous(command) {
        !matches!(policy, AskForApproval::Never)
    } else {
        match policy {
            AskForApproval::Never => false,
            AskForApproval::UnlessTrusted => true,
            AskForApproval::OnRequest | AskForApproval::OnFailure => true,
        }
    }
}

#[test]
fn dangerous_command_needs_approval_under_unless_trusted() {
    let cmd: Vec<String> = vec!["rm", "-rf", "/"]
        .into_iter()
        .map(String::from)
        .collect();
    assert!(
        needs_approval(&cmd, AskForApproval::UnlessTrusted),
        "dangerous command should require approval under UnlessTrusted"
    );
}

#[test]
fn dangerous_command_needs_approval_under_on_failure() {
    let cmd: Vec<String> = vec!["rm", "-rf", "/"]
        .into_iter()
        .map(String::from)
        .collect();
    assert!(
        needs_approval(&cmd, AskForApproval::OnFailure),
        "dangerous command should require approval under OnFailure"
    );
}

#[test]
fn safe_command_skips_approval_under_on_failure() {
    let cmd: Vec<String> = vec!["ls"].into_iter().map(String::from).collect();
    assert!(
        !needs_approval(&cmd, AskForApproval::OnFailure),
        "safe command should skip approval under OnFailure"
    );
}

#[test]
fn safe_command_skips_approval_under_on_request() {
    let cmd: Vec<String> = vec!["ls"].into_iter().map(String::from).collect();
    assert!(
        !needs_approval(&cmd, AskForApproval::OnRequest),
        "safe command should skip approval under OnRequest"
    );
}
