//! Unit and integration tests for the codex-temporal harness.

use std::collections::BTreeMap;
use std::sync::Arc;

use codex_temporal::entropy::TemporalRandomSource;
use codex_temporal::sink::BufferEventSink;
use codex_temporal::storage::InMemoryStorage;
use codex_temporal::types::{
    ApprovalInput, CodexWorkflowInput, CodexWorkflowOutput, ConfigOutput, CrewAgentDef,
    CrewInputSpec, CrewMode, CrewType, HarnessInput, HarnessState, ModelCallOutput,
    PendingApproval, ProjectContextOutput, SessionEntry, SessionStatus, ToolExecOutput,
    UserTurnInput,
};

use codex_core::entropy::RandomSource;
use codex_core::{EventSink, StorageBackend};
use codex_protocol::protocol::RolloutItem;

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


// ---------------------------------------------------------------------------
// EventSink tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn buffer_event_sink_collects_and_drains() {
    let sink = BufferEventSink::new(4096, 0);
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

    // After drain, offset should have advanced — watermark reflects drained count.
    let (_events, watermark) = sink.events_since(0);
    assert_eq!(watermark, 2, "offset should advance after drain");
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
        role: "default".to_string(),
        config_toml: None,
        project_context: None,
        mcp_tools: std::collections::HashMap::new(),
        dynamic_tools: Vec::new(),
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

    let event_sink: Arc<dyn EventSink> = Arc::new(BufferEventSink::new(4096, 0));
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
    let sink = BufferEventSink::new(4096, 0);

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
    let sink = BufferEventSink::new(4096, 0);

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

#[tokio::test]
async fn buffer_event_sink_rolling_window_evicts_oldest() {
    let capacity = 10;
    let sink = BufferEventSink::new(capacity, 0);

    use codex_protocol::protocol::{Event, EventMsg, TurnStartedEvent};

    // Push capacity + 5 events.
    let total = capacity + 5;
    for i in 0..total {
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

    // Buffer should contain exactly `capacity` events.
    assert_eq!(sink.len(), capacity);

    // events_since(0) returns only the window (client is behind).
    let (events, watermark) = sink.events_since(0);
    assert_eq!(events.len(), capacity);
    assert_eq!(watermark, total);

    // The first event in the buffer should be ev-5 (0..4 were evicted).
    let first: Event = serde_json::from_str(&events[0]).unwrap();
    assert_eq!(first.id, "ev-5");

    // events_since with the correct watermark returns empty.
    let (events, watermark2) = sink.events_since(watermark);
    assert!(events.is_empty());
    assert_eq!(watermark2, watermark);
}

#[tokio::test]
async fn buffer_event_sink_snapshot_and_restore() {
    let sink = BufferEventSink::new(100, 0);

    use codex_protocol::protocol::{Event, EventMsg, TurnStartedEvent};

    for i in 0..5 {
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

    // Snapshot.
    let (offset, snapshot) = sink.snapshot();
    assert_eq!(offset, 0);
    assert_eq!(snapshot.len(), 5);

    // Restore into a new sink.
    let restored = BufferEventSink::from_snapshot(offset, snapshot, 100);
    assert_eq!(restored.len(), 5);

    // events_since should return the same results.
    let (orig_events, orig_wm) = sink.events_since(0);
    let (rest_events, rest_wm) = restored.events_since(0);
    assert_eq!(orig_events, rest_events);
    assert_eq!(orig_wm, rest_wm);

    // events_since(3) on restored should return last 2 events.
    let (events, watermark) = restored.events_since(3);
    assert_eq!(events.len(), 2);
    assert_eq!(watermark, 5);
}

#[tokio::test]
async fn buffer_event_sink_offset_continuity_across_can() {
    use codex_protocol::protocol::{Event, EventMsg, TurnStartedEvent};

    // Simulate first CAN run: push 20 events into a capacity-10 buffer.
    let sink1 = BufferEventSink::new(10, 0);
    for i in 0..20 {
        let event = Event {
            id: format!("run1-ev-{i}"),
            msg: EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: format!("turn-{i}"),
                model_context_window: None,
                collaboration_mode_kind: Default::default(),
            }),
        };
        sink1.emit_event(event).await;
    }

    // Client polled and has watermark=20.
    let (_events, client_watermark) = sink1.events_since(0);
    assert_eq!(client_watermark, 20);

    // Snapshot for CAN.
    let (offset, snapshot) = sink1.snapshot();
    assert_eq!(offset, 10); // 10 events were evicted

    // Restore in new run.
    let sink2 = BufferEventSink::from_snapshot(offset, snapshot, 10);

    // Push 5 more events in the new run.
    for i in 0..5 {
        let event = Event {
            id: format!("run2-ev-{i}"),
            msg: EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: format!("turn-{}", 20 + i),
                model_context_window: None,
                collaboration_mode_kind: Default::default(),
            }),
        };
        sink2.emit_event(event).await;
    }

    // Client polls with old watermark=20 — should get the 5 new events.
    let (events, new_watermark) = sink2.events_since(20);
    assert_eq!(events.len(), 5);
    assert_eq!(new_watermark, 25); // monotonically increasing

    // Verify watermark is monotonic.
    assert!(new_watermark > client_watermark);

    // Client polls with new watermark — should get empty.
    let (events, wm) = sink2.events_since(new_watermark);
    assert!(events.is_empty());
    assert_eq!(wm, 25);

    // Client that fell far behind (watermark=5) gets whatever is in the buffer.
    let (events, wm) = sink2.events_since(5);
    // Buffer has 10 old + 5 new = 15, but capacity is 10, so 5 were evicted.
    // Buffer contains events from offset=15..25 (10 events).
    assert_eq!(events.len(), 10);
    assert_eq!(wm, 25);
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
        role: "default".to_string(),
        config_toml: None,
        project_context: None,
        mcp_tools: std::collections::HashMap::new(),
        dynamic_tools: Vec::new(),
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
        role: "default".to_string(),
        config_toml: None,
        project_context: None,
        mcp_tools: std::collections::HashMap::new(),
        dynamic_tools: Vec::new(),
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
        role: "default".to_string(),
        config_toml: None,
        project_context: None,
        mcp_tools: std::collections::HashMap::new(),
        dynamic_tools: Vec::new(),
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
        role: "default".to_string(),
        config_toml: None,
        project_context: None,
        mcp_tools: std::collections::HashMap::new(),
        dynamic_tools: Vec::new(),
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
        mcp_tools: std::collections::HashMap::new(),
        approval_policy_override: None,
        model_override: None,
        effort_override: None,
        summary_override: None,
        personality_override: None,
        event_offset: 0,
        event_snapshot: vec![],
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
            mcp_tools: std::collections::HashMap::new(),
            approval_policy_override: None,
            model_override: None,
            effort_override: None,
            summary_override: None,
            personality_override: None,
            event_offset: 0,
            event_snapshot: vec![],
        }),
        role: "default".to_string(),
        config_toml: None,
        project_context: None,
        mcp_tools: std::collections::HashMap::new(),
        dynamic_tools: Vec::new(),
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
        worker_token: None,
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
        worker_token: None,
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
        worker_token: None,
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
// Input validation tests for dispatch_tool
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dispatch_tool_rejects_empty_tool_name() {
    let input = ToolExecInput {
        tool_name: "".to_string(),
        call_id: "test-empty".to_string(),
        arguments: "{}".to_string(),
        model: "gpt-4o".to_string(),
        cwd: "/tmp".to_string(),
        config_toml: Some(FULL_ACCESS_TOML.to_string()),
        worker_token: None,
    };

    let result = dispatch_tool(input).await;
    assert!(result.is_err(), "empty tool_name should be rejected");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("tool_name must not be empty"),
        "error should mention tool_name: {err}"
    );
}

#[tokio::test]
async fn dispatch_tool_rejects_nonexistent_cwd() {
    let input = ToolExecInput {
        tool_name: "shell".to_string(),
        call_id: "test-bad-cwd".to_string(),
        arguments: r#"{"command":["echo","hi"]}"#.to_string(),
        model: "gpt-4o".to_string(),
        cwd: "/nonexistent/path/that/does/not/exist".to_string(),
        config_toml: Some(FULL_ACCESS_TOML.to_string()),
        worker_token: None,
    };

    let result = dispatch_tool(input).await;
    assert!(result.is_err(), "nonexistent cwd should be rejected");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("cwd does not exist"),
        "error should mention cwd: {err}"
    );
}

#[tokio::test]
async fn dispatch_tool_rejects_cwd_that_is_file() {
    // Create a temporary file to use as a non-directory cwd.
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let file_path = tmp.path().to_string_lossy().to_string();

    let input = ToolExecInput {
        tool_name: "shell".to_string(),
        call_id: "test-file-cwd".to_string(),
        arguments: r#"{"command":["echo","hi"]}"#.to_string(),
        model: "gpt-4o".to_string(),
        cwd: file_path,
        config_toml: Some(FULL_ACCESS_TOML.to_string()),
        worker_token: None,
    };

    let result = dispatch_tool(input).await;
    assert!(result.is_err(), "file as cwd should be rejected");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("cwd is not a directory"),
        "error should mention not a directory: {err}"
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
        session_source: codex_protocol::protocol::SessionSource::Exec,
    });
    let builder = build_specs(&tools_config, None, None, &[]);
    let (_specs, registry) = builder.build();

    let conversation_id = ThreadId::new();
    let event_sink: Arc<dyn codex_core::EventSink> = Arc::new(BufferEventSink::new(4096, 0));
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
                } => {
                    let text = output.body.to_text().unwrap_or_default();
                    let success = output.success.unwrap_or(true);
                    (text, if success { 0 } else { 1 })
                }
                other => (format!("{other:?}"), 0),
            }
        }
        Err(e) => (format!("dispatch error: {e}"), 1),
    }
}

#[tokio::test]
async fn dispatch_read_file_reads_system_file() {
    let file_path = if cfg!(target_os = "linux") {
        "/etc/hostname"
    } else {
        "/etc/hosts" // exists on both macOS and Linux
    };

    let args = format!(r#"{{"file_path":"{}"}}"#, file_path);
    let (output, exit_code) = dispatch_with_experimental_tools(
        "read_file",
        &args,
        &["read_file"],
    )
    .await;

    assert_eq!(exit_code, 0, "read_file should succeed: {output}");
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
        crew_type: None,
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
            crew_type: None,
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
                crew_type: None,
            },
            SessionEntry {
                session_id: "s2".to_string(),
                name: None,
                model: "gpt-4o-mini".to_string(),
                created_at_millis: 200,
                status: SessionStatus::Completed,
                crew_type: None,
            },
        ],
        credentials_available: None,
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

#[test]
fn harness_state_credentials_available_roundtrips() {
    let state = HarnessState {
        sessions: vec![],
        credentials_available: Some(true),
    };
    let json = serde_json::to_string(&state).unwrap();
    let back: HarnessState = serde_json::from_str(&json).unwrap();
    assert_eq!(back.credentials_available, Some(true));
}

#[test]
fn harness_state_credentials_available_defaults_to_none() {
    // JSON without credentials_available should deserialize to None (backward compat).
    let json = r#"{"sessions":[]}"#;
    let back: HarnessState = serde_json::from_str(json).unwrap();
    assert!(
        back.credentials_available.is_none(),
        "credentials_available should default to None when absent"
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
        role: "default".to_string(),
        config_toml: None,
        project_context: None,
        mcp_tools: std::collections::HashMap::new(),
        dynamic_tools: Vec::new(),
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
        role: "default".to_string(),
        config_toml: None,
        project_context: None,
        mcp_tools: std::collections::HashMap::new(),
        dynamic_tools: Vec::new(),
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
        worker_token: Some("test-token-123".to_string()),
    };
    let json = serde_json::to_string(&original).unwrap();
    let back: ConfigOutput = serde_json::from_str(&json).unwrap();
    assert_eq!(back.config_toml, original.config_toml);
    assert_eq!(back.worker_token, original.worker_token);

    // Backward compat: old JSON without worker_token should default to None.
    let old_json = r#"{"config_toml":"model = \"gpt-4o\"\n"}"#;
    let back_old: ConfigOutput = serde_json::from_str(old_json).unwrap();
    assert!(back_old.worker_token.is_none(), "missing worker_token should default to None");
}

#[test]
fn tool_exec_input_config_toml_roundtrip() {
    // With config_toml and worker_token present.
    let input_with = ToolExecInput {
        tool_name: "shell".to_string(),
        call_id: "c1".to_string(),
        arguments: "{}".to_string(),
        model: "gpt-4o".to_string(),
        cwd: "/tmp".to_string(),
        config_toml: Some("model = \"gpt-4o\"\n".to_string()),
        worker_token: Some("tok-abc".to_string()),
    };
    let json_with = serde_json::to_string(&input_with).unwrap();
    assert!(
        json_with.contains("config_toml"),
        "config_toml should be serialized when Some"
    );
    assert!(
        json_with.contains("worker_token"),
        "worker_token should be serialized when Some"
    );
    let back_with: ToolExecInput = serde_json::from_str(&json_with).unwrap();
    assert_eq!(back_with.config_toml, input_with.config_toml);
    assert_eq!(back_with.worker_token, input_with.worker_token);

    // With config_toml and worker_token absent (backward compat).
    let input_none = ToolExecInput {
        tool_name: "shell".to_string(),
        call_id: "c2".to_string(),
        arguments: "{}".to_string(),
        model: "gpt-4o".to_string(),
        cwd: "/tmp".to_string(),
        config_toml: None,
        worker_token: None,
    };
    let json_none = serde_json::to_string(&input_none).unwrap();
    assert!(
        !json_none.contains("config_toml"),
        "config_toml:None should be skipped in serialization"
    );
    assert!(
        !json_none.contains("worker_token"),
        "worker_token:None should be skipped in serialization"
    );

    // Deserializing old-format JSON without config_toml/worker_token should default to None.
    let old_json = r#"{"tool_name":"shell","call_id":"c3","arguments":"{}","model":"gpt-4o","cwd":"/tmp"}"#;
    let back_old: ToolExecInput = serde_json::from_str(old_json).unwrap();
    assert!(
        back_old.config_toml.is_none(),
        "missing config_toml should default to None"
    );
    assert!(
        back_old.worker_token.is_none(),
        "missing worker_token should default to None"
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
            AskForApproval::UnlessTrusted
            | AskForApproval::OnRequest
            | AskForApproval::OnFailure
            | AskForApproval::Reject(_) => true,
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

// ---------------------------------------------------------------------------
// MCP types tests
// ---------------------------------------------------------------------------

#[test]
fn mcp_discover_output_serde_roundtrip() {
    use codex_temporal::types::McpDiscoverOutput;

    let mut tools = std::collections::HashMap::new();
    tools.insert(
        "mcp__echo__echo".to_string(),
        serde_json::json!({
            "name": "echo",
            "inputSchema": {"type": "object", "properties": {"message": {"type": "string"}}}
        }),
    );

    let output = McpDiscoverOutput { tools };
    let json = serde_json::to_string(&output).unwrap();
    let back: McpDiscoverOutput = serde_json::from_str(&json).unwrap();
    assert_eq!(back.tools.len(), 1);
    assert!(back.tools.contains_key("mcp__echo__echo"));
}

#[test]
fn mcp_tool_call_input_serde_roundtrip() {
    use codex_temporal::types::McpToolCallInput;

    let input = McpToolCallInput {
        qualified_name: "mcp__echo__echo".to_string(),
        call_id: "call-123".to_string(),
        arguments: r#"{"message":"hello"}"#.to_string(),
    };

    let json = serde_json::to_string(&input).unwrap();
    let back: McpToolCallInput = serde_json::from_str(&json).unwrap();
    assert_eq!(back.qualified_name, "mcp__echo__echo");
    assert_eq!(back.call_id, "call-123");
    assert_eq!(back.arguments, r#"{"message":"hello"}"#);
}

#[test]
fn mcp_tool_call_output_serde_roundtrip_ok() {
    use codex_temporal::types::McpToolCallOutput;

    let output = McpToolCallOutput {
        call_id: "call-456".to_string(),
        result: Ok(serde_json::json!({
            "content": [{"type": "text", "text": "hello back"}],
        })),
        elicitation: None,
    };

    let json = serde_json::to_string(&output).unwrap();
    let back: McpToolCallOutput = serde_json::from_str(&json).unwrap();
    assert_eq!(back.call_id, "call-456");
    assert!(back.result.is_ok());
}

#[test]
fn mcp_tool_call_output_serde_roundtrip_err() {
    use codex_temporal::types::McpToolCallOutput;

    let output = McpToolCallOutput {
        call_id: "call-789".to_string(),
        result: Err("tool not found".to_string()),
        elicitation: None,
    };

    let json = serde_json::to_string(&output).unwrap();
    let back: McpToolCallOutput = serde_json::from_str(&json).unwrap();
    assert_eq!(back.call_id, "call-789");
    assert!(back.result.is_err());
    assert_eq!(back.result.unwrap_err(), "tool not found");
}

#[test]
fn mcp_tool_call_output_into_response_item_ok() {
    use codex_temporal::types::McpToolCallOutput;
    use codex_protocol::models::ResponseInputItem;

    let output = McpToolCallOutput {
        call_id: "call-resp".to_string(),
        result: Ok(serde_json::json!({
            "content": [{"type": "text", "text": "result text"}],
        })),
        elicitation: None,
    };

    let item = output.into_response_input_item();
    match item {
        ResponseInputItem::McpToolCallOutput { call_id, result } => {
            assert_eq!(call_id, "call-resp");
            assert!(result.is_ok());
            let ctr = result.unwrap();
            assert_eq!(ctr.content.len(), 1);
        }
        other => panic!("expected McpToolCallOutput, got {:?}", other),
    }
}

#[test]
fn mcp_tool_call_output_into_response_item_err() {
    use codex_temporal::types::McpToolCallOutput;
    use codex_protocol::models::ResponseInputItem;

    let output = McpToolCallOutput {
        call_id: "call-err".to_string(),
        result: Err("server crashed".to_string()),
        elicitation: None,
    };

    let item = output.into_response_input_item();
    match item {
        ResponseInputItem::McpToolCallOutput { call_id, result } => {
            assert_eq!(call_id, "call-err");
            assert!(result.is_err());
            assert_eq!(result.unwrap_err(), "server crashed");
        }
        other => panic!("expected McpToolCallOutput, got {:?}", other),
    }
}

#[test]
fn continue_as_new_state_with_mcp_tools() {
    use codex_temporal::types::ContinueAsNewState;

    let mut mcp_tools = std::collections::HashMap::new();
    mcp_tools.insert(
        "mcp__srv__tool".to_string(),
        serde_json::json!({"name": "tool", "inputSchema": {"type": "object"}}),
    );

    let state = ContinueAsNewState {
        rollout_items: vec![],
        pending_user_turns: vec![],
        cumulative_turn_count: 1,
        cumulative_iterations: 2,
        cumulative_token_usage: None,
        mcp_tools: mcp_tools.clone(),
        approval_policy_override: None,
        model_override: None,
        effort_override: None,
        summary_override: None,
        personality_override: None,
        event_offset: 0,
        event_snapshot: vec![],
    };

    let json = serde_json::to_string(&state).unwrap();
    let back: ContinueAsNewState = serde_json::from_str(&json).unwrap();
    assert_eq!(back.mcp_tools.len(), 1);
    assert!(back.mcp_tools.contains_key("mcp__srv__tool"));
}

#[test]
fn continue_as_new_state_mcp_tools_default_when_missing() {
    use codex_temporal::types::ContinueAsNewState;

    // JSON without mcp_tools field — should default to empty.
    let json = r#"{
        "rollout_items": [],
        "pending_user_turns": [],
        "cumulative_turn_count": 0,
        "cumulative_iterations": 0
    }"#;

    let state: ContinueAsNewState = serde_json::from_str(json).unwrap();
    assert!(state.mcp_tools.is_empty());
}

#[test]
fn continue_as_new_state_with_approval_policy_roundtrips() {
    use codex_temporal::types::ContinueAsNewState;
    use codex_protocol::protocol::AskForApproval;

    let state = ContinueAsNewState {
        rollout_items: vec![],
        pending_user_turns: vec![],
        cumulative_turn_count: 1,
        cumulative_iterations: 2,
        cumulative_token_usage: None,
        mcp_tools: std::collections::HashMap::new(),
        approval_policy_override: Some(AskForApproval::Never),
        model_override: None,
        effort_override: None,
        summary_override: None,
        personality_override: None,
        event_offset: 0,
        event_snapshot: vec![],
    };

    let json = serde_json::to_string(&state).unwrap();
    let back: ContinueAsNewState = serde_json::from_str(&json).unwrap();
    assert_eq!(back.approval_policy_override, Some(AskForApproval::Never));
}

#[test]
fn continue_as_new_state_approval_policy_default_when_missing() {
    use codex_temporal::types::ContinueAsNewState;

    // JSON without approval_policy_override field — should default to None.
    let json = r#"{
        "rollout_items": [],
        "pending_user_turns": [],
        "cumulative_turn_count": 0,
        "cumulative_iterations": 0
    }"#;

    let state: ContinueAsNewState = serde_json::from_str(json).unwrap();
    assert!(state.approval_policy_override.is_none());
}

#[tokio::test]
async fn config_toml_roundtrip_preserves_mcp_servers() {
    use codex_core::config::ConfigBuilder;
    use codex_temporal::config_loader::config_from_toml;

    let tmp = std::env::temp_dir().join(format!(
        "codex-mcp-config-test-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(
        tmp.join("config.toml"),
        r#"
sandbox_mode = "danger-full-access"

[mcp_servers.testserver]
command = "echo"
args = ["hello"]
"#,
    )
    .unwrap();

    // SAFETY: unit tests using CODEX_HOME must be sequential.
    let prev = std::env::var("CODEX_HOME").ok();
    unsafe { std::env::set_var("CODEX_HOME", &tmp) };

    let toml_string = ConfigBuilder::default()
        .build_toml_string()
        .await
        .expect("build_toml_string failed");

    // Restore CODEX_HOME immediately.
    unsafe {
        match &prev {
            Some(val) => std::env::set_var("CODEX_HOME", val),
            None => std::env::remove_var("CODEX_HOME"),
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);

    // Verify mcp_servers survived the round-trip.
    assert!(
        toml_string.contains("mcp_servers") || toml_string.contains("testserver"),
        "TOML output should contain mcp_servers section, got:\n{}",
        toml_string
    );

    let cwd = std::path::PathBuf::from("/tmp");
    let config = config_from_toml(&toml_string, &cwd, None)
        .expect("config_from_toml failed");
    let servers = config.mcp_servers.get();
    assert!(
        servers.contains_key("testserver"),
        "expected 'testserver' in mcp_servers, got keys: {:?}",
        servers.keys().collect::<Vec<_>>()
    );
}

#[test]
fn mcp_tool_name_detection() {
    use codex_core::mcp::split_qualified_tool_name;

    // Valid MCP tool names.
    let result = split_qualified_tool_name("mcp__echo__echo");
    assert_eq!(result, Some(("echo".to_string(), "echo".to_string())));

    let result = split_qualified_tool_name("mcp__server__nested__tool");
    assert_eq!(
        result,
        Some(("server".to_string(), "nested__tool".to_string()))
    );

    // Non-MCP tool names should return None.
    assert_eq!(split_qualified_tool_name("shell"), None);
    assert_eq!(split_qualified_tool_name("read_file"), None);
    assert_eq!(split_qualified_tool_name("other__server__tool"), None);

    // Malformed MCP names.
    assert_eq!(split_qualified_tool_name("mcp__server__"), None);
    assert_eq!(split_qualified_tool_name("mcp__"), None);
}

// ---------------------------------------------------------------------------
// Multi-agent type serde tests
// ---------------------------------------------------------------------------

#[test]
fn session_workflow_input_roundtrips_through_json() {
    use codex_temporal::types::SessionWorkflowInput;

    let input = SessionWorkflowInput {
        user_message: "Hello".to_string(),
        model: "gpt-4o".to_string(),
        instructions: "Be helpful.".to_string(),
        approval_policy: Default::default(),
        web_search_mode: None,
        reasoning_effort: None,
        reasoning_summary: codex_protocol::config_types::ReasoningSummary::Auto,
        personality: None,
        developer_instructions: None,
        model_provider: None,
        crew_agents: BTreeMap::new(),
        continued_state: None,
    };

    let json = serde_json::to_string(&input).unwrap();
    let back: SessionWorkflowInput = serde_json::from_str(&json).unwrap();

    assert_eq!(back.user_message, "Hello");
    assert_eq!(back.model, "gpt-4o");
    assert_eq!(back.instructions, "Be helpful.");
}

#[test]
fn session_workflow_input_backward_compat_defaults() {
    use codex_temporal::types::SessionWorkflowInput;

    // Minimal JSON without optional fields.
    let json = r#"{"user_message":"hi","model":"gpt-4o","instructions":"test"}"#;
    let input: SessionWorkflowInput = serde_json::from_str(json).unwrap();

    assert_eq!(input.user_message, "hi");
    assert!(input.continued_state.is_none());
    assert!(input.developer_instructions.is_none());
    assert!(input.model_provider.is_none());
}

#[test]
fn agent_workflow_input_backward_compat_without_new_fields() {
    // JSON without the new multi-agent fields should deserialize fine.
    let json = r#"{
        "user_message": "test",
        "model": "gpt-4o",
        "instructions": "be helpful"
    }"#;
    let input: CodexWorkflowInput = serde_json::from_str(json).unwrap();

    assert_eq!(input.role, "default");
    assert!(input.config_toml.is_none());
    assert!(input.project_context.is_none());
    assert!(input.mcp_tools.is_empty());
}

#[test]
fn agent_workflow_input_with_new_fields_roundtrips() {
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
        model_provider: None,
        continued_state: None,
        role: "explorer".to_string(),
        config_toml: Some("model = \"gpt-5\"".to_string()),
        project_context: Some(ProjectContextOutput {
            cwd: "/tmp".to_string(),
            user_instructions: None,
            git_info: None,
        }),
        mcp_tools: {
            let mut m = std::collections::HashMap::new();
            m.insert("mcp__echo__echo".to_string(), serde_json::json!({"name": "echo"}));
            m
        },
        dynamic_tools: Vec::new(),
    };

    let json = serde_json::to_string(&input).unwrap();
    let back: CodexWorkflowInput = serde_json::from_str(&json).unwrap();

    assert_eq!(back.role, "explorer");
    assert_eq!(back.config_toml.as_deref(), Some("model = \"gpt-5\""));
    assert!(back.project_context.is_some());
    assert_eq!(back.mcp_tools.len(), 1);
}

#[test]
fn session_continue_as_new_state_roundtrips() {
    use codex_temporal::types::SessionContinueAsNewState;
    use codex_temporal::types::AgentRecord;
    use codex_temporal::types::AgentLifecycle;

    let state = SessionContinueAsNewState {
        agents: vec![AgentRecord {
            agent_id: "session/main".to_string(),
            workflow_id: "session/main".to_string(),
            role: "default".to_string(),
            status: AgentLifecycle::Running,
        }],
        config_toml: "model = \"gpt-4o\"".to_string(),
        project_context: ProjectContextOutput {
            cwd: "/tmp".to_string(),
            user_instructions: None,
            git_info: None,
        },
        mcp_tools: std::collections::HashMap::new(),
        crew_agents: BTreeMap::new(),
    };

    let json = serde_json::to_string(&state).unwrap();
    let back: SessionContinueAsNewState = serde_json::from_str(&json).unwrap();

    assert_eq!(back.agents.len(), 1);
    assert_eq!(back.agents[0].agent_id, "session/main");
    assert_eq!(back.config_toml, "model = \"gpt-4o\"");
}

#[test]
fn spawn_agent_input_roundtrips() {
    use codex_temporal::types::SpawnAgentInput;

    let input = SpawnAgentInput {
        role: "explorer".to_string(),
        message: "Find the auth module".to_string(),
    };

    let json = serde_json::to_string(&input).unwrap();
    let back: SpawnAgentInput = serde_json::from_str(&json).unwrap();

    assert_eq!(back.role, "explorer");
    assert_eq!(back.message, "Find the auth module");
}

#[test]
fn agent_record_roundtrips() {
    use codex_temporal::types::{AgentRecord, AgentLifecycle};

    let record = AgentRecord {
        agent_id: "session/explorer-1".to_string(),
        workflow_id: "session/explorer-1".to_string(),
        role: "explorer".to_string(),
        status: AgentLifecycle::Completed,
    };

    let json = serde_json::to_string(&record).unwrap();
    let back: AgentRecord = serde_json::from_str(&json).unwrap();

    assert_eq!(back.agent_id, "session/explorer-1");
    assert_eq!(back.role, "explorer");
    assert_eq!(back.status, AgentLifecycle::Completed);
}

#[test]
fn agent_summary_roundtrips() {
    use codex_temporal::types::{AgentSummary, AgentLifecycle};

    let summary = AgentSummary {
        agent_id: "session/main".to_string(),
        role: "default".to_string(),
        status: AgentLifecycle::Running,
        iterations: 5,
        token_usage: None,
    };

    let json = serde_json::to_string(&summary).unwrap();
    let back: AgentSummary = serde_json::from_str(&json).unwrap();

    assert_eq!(back.agent_id, "session/main");
    assert_eq!(back.iterations, 5);
    assert_eq!(back.status, AgentLifecycle::Running);
}

#[test]
fn resolve_role_config_input_roundtrips() {
    use codex_temporal::types::ResolveRoleConfigInput;

    let input = ResolveRoleConfigInput {
        config_toml: "model = \"gpt-4o\"".to_string(),
        cwd: "/home/user/project".to_string(),
        role_name: "explorer".to_string(),
    };

    let json = serde_json::to_string(&input).unwrap();
    let back: ResolveRoleConfigInput = serde_json::from_str(&json).unwrap();

    assert_eq!(back.role_name, "explorer");
    assert_eq!(back.cwd, "/home/user/project");
}

#[test]
fn resolve_role_config_output_roundtrips() {
    use codex_temporal::types::ResolveRoleConfigOutput;
    use codex_protocol::openai_models::ReasoningEffort;

    let output = ResolveRoleConfigOutput {
        config_toml: "model = \"gpt-5\"".to_string(),
        model: Some("gpt-5".to_string()),
        instructions: Some("Be an explorer.".to_string()),
        reasoning_effort: Some(ReasoningEffort::Medium),
        reasoning_summary: codex_protocol::config_types::ReasoningSummary::Auto,
        personality: None,
        developer_instructions: None,
        model_provider: None,
    };

    let json = serde_json::to_string(&output).unwrap();
    let back: ResolveRoleConfigOutput = serde_json::from_str(&json).unwrap();

    assert_eq!(back.model.as_deref(), Some("gpt-5"));
    assert_eq!(back.reasoning_effort, Some(ReasoningEffort::Medium));
}

#[test]
fn agent_lifecycle_serde() {
    use codex_temporal::types::AgentLifecycle;

    for status in [AgentLifecycle::Running, AgentLifecycle::Completed, AgentLifecycle::Failed] {
        let json = serde_json::to_string(&status).unwrap();
        let back: AgentLifecycle = serde_json::from_str(&json).unwrap();
        assert_eq!(back, status);
    }
}

#[test]
fn session_workflow_output_roundtrips() {
    use codex_temporal::types::{SessionWorkflowOutput, AgentSummary, AgentLifecycle};

    let output = SessionWorkflowOutput {
        agents: vec![
            AgentSummary {
                agent_id: "session/main".to_string(),
                role: "default".to_string(),
                status: AgentLifecycle::Completed,
                iterations: 10,
                token_usage: None,
            },
            AgentSummary {
                agent_id: "session/explorer-1".to_string(),
                role: "explorer".to_string(),
                status: AgentLifecycle::Completed,
                iterations: 3,
                token_usage: None,
            },
        ],
    };

    let json = serde_json::to_string(&output).unwrap();
    let back: SessionWorkflowOutput = serde_json::from_str(&json).unwrap();

    assert_eq!(back.agents.len(), 2);
    assert_eq!(back.agents[0].agent_id, "session/main");
    assert_eq!(back.agents[1].role, "explorer");
}

#[test]
fn backward_compat_type_aliases_work() {
    // Verify the type aliases compile and are equivalent.
    let _input: CodexWorkflowInput = CodexWorkflowInput {
        user_message: "test".to_string(),
        model: "gpt-4o".to_string(),
        instructions: "test".to_string(),
        approval_policy: Default::default(),
        web_search_mode: None,
        reasoning_effort: None,
        reasoning_summary: codex_protocol::config_types::ReasoningSummary::Auto,
        personality: None,
        developer_instructions: None,
        model_provider: None,
        continued_state: None,
        role: "default".to_string(),
        config_toml: None,
        project_context: None,
        mcp_tools: std::collections::HashMap::new(),
        dynamic_tools: Vec::new(),
    };

    let _output: CodexWorkflowOutput = CodexWorkflowOutput {
        last_agent_message: None,
        iterations: 0,
        token_usage: None,
    };
}

#[test]
fn codex_workflow_input_converts_to_session_workflow_input() {
    use codex_temporal::types::SessionWorkflowInput;

    let agent_input = CodexWorkflowInput {
        user_message: "hello".to_string(),
        model: "gpt-4o".to_string(),
        instructions: "be helpful".to_string(),
        approval_policy: Default::default(),
        web_search_mode: None,
        reasoning_effort: None,
        reasoning_summary: codex_protocol::config_types::ReasoningSummary::Auto,
        personality: None,
        developer_instructions: Some("dev note".to_string()),
        model_provider: None,
        continued_state: None,
        role: "default".to_string(),
        config_toml: None,
        project_context: None,
        mcp_tools: std::collections::HashMap::new(),
        dynamic_tools: Vec::new(),
    };

    let session_input: SessionWorkflowInput = agent_input.into();

    assert_eq!(session_input.user_message, "hello");
    assert_eq!(session_input.model, "gpt-4o");
    assert_eq!(
        session_input.developer_instructions.as_deref(),
        Some("dev note")
    );
    assert!(session_input.continued_state.is_none());
}

// ---------------------------------------------------------------------------
// Crew type tests
// ---------------------------------------------------------------------------

#[test]
fn crew_type_serde_roundtrip() {
    let toml_str = r#"
name = "git-bug-fixer"
description = "Watches a git repo for new issues and fixes bugs"
mode = "autonomous"
initial_prompt = "Monitor {repo_url} for new issues on branch {branch} and fix them."
main_agent = "coordinator"

[inputs.repo_url]
description = "Git repository URL to watch"
required = true

[inputs.branch]
description = "Branch to work on"
default = "main"

[agents.coordinator]
model = "gpt-4o"
instructions = "You are a bug-fixing coordinator for {repo_url} on branch {branch}."
description = "Coordinates bug fixing work across the repo"

[agents.fixer]
role = "explorer"
description = "Investigates and fixes individual bugs"
"#;

    let crew: CrewType = toml::from_str(toml_str).expect("parse crew TOML");
    assert_eq!(crew.name, "git-bug-fixer");
    assert_eq!(crew.mode, CrewMode::Autonomous);
    assert_eq!(crew.main_agent, "coordinator");
    assert_eq!(crew.inputs.len(), 2);
    assert_eq!(crew.agents.len(), 2);

    // Verify coordinator agent.
    let coord = &crew.agents["coordinator"];
    assert_eq!(coord.model.as_deref(), Some("gpt-4o"));
    assert!(coord.instructions.as_ref().unwrap().contains("{repo_url}"));

    // Verify fixer agent.
    let fixer = &crew.agents["fixer"];
    assert_eq!(fixer.role.as_deref(), Some("explorer"));
    assert!(fixer.model.is_none());

    // JSON roundtrip.
    let json = serde_json::to_string(&crew).unwrap();
    let back: CrewType = serde_json::from_str(&json).unwrap();
    assert_eq!(back.name, "git-bug-fixer");
    assert_eq!(back.agents.len(), 2);
}

#[test]
fn crew_type_minimal() {
    let toml_str = r#"
name = "minimal"
description = "A minimal crew with no agents or inputs"
"#;

    let crew: CrewType = toml::from_str(toml_str).expect("parse minimal crew TOML");
    assert_eq!(crew.name, "minimal");
    assert_eq!(crew.description, "A minimal crew with no agents or inputs");
    assert_eq!(crew.mode, CrewMode::Interactive);
    assert!(crew.initial_prompt.is_none());
    assert!(crew.inputs.is_empty());
    assert_eq!(crew.main_agent, "default");
    assert!(crew.agents.is_empty());
    assert!(crew.approval_policy.is_none());
}

#[test]
fn crew_input_spec_defaults() {
    // `required` defaults to true when not specified.
    let toml_str = r#"
description = "Some input"
"#;
    let spec: CrewInputSpec = toml::from_str(toml_str).unwrap();
    assert!(spec.required);
    assert!(spec.default.is_none());

    // Explicit optional with default value.
    let toml_str2 = r#"
description = "Some optional input"
required = false
default = "hello"
"#;
    let spec2: CrewInputSpec = toml::from_str(toml_str2).unwrap();
    assert!(!spec2.required);
    assert_eq!(spec2.default.as_deref(), Some("hello"));
}

#[test]
fn crew_agent_def_inline_vs_role() {
    // Inline agent: model + instructions.
    let toml_str = r#"
model = "gpt-4o"
instructions = "Be helpful."
description = "A helpful agent"
"#;
    let def: CrewAgentDef = toml::from_str(toml_str).unwrap();
    assert!(def.role.is_none());
    assert_eq!(def.model.as_deref(), Some("gpt-4o"));
    assert_eq!(def.instructions.as_deref(), Some("Be helpful."));

    // Role reference.
    let toml_str2 = r#"
role = "explorer"
description = "Explores the code"
"#;
    let def2: CrewAgentDef = toml::from_str(toml_str2).unwrap();
    assert_eq!(def2.role.as_deref(), Some("explorer"));
    assert!(def2.model.is_none());
    assert!(def2.instructions.is_none());
}

#[test]
fn apply_crew_type_interpolates_placeholders() {
    use std::collections::BTreeMap;
    use codex_temporal::config_loader::apply_crew_type;
    use codex_temporal::types::SessionWorkflowInput;

    let crew = CrewType {
        name: "test-crew".to_string(),
        description: "test".to_string(),
        mode: CrewMode::Autonomous,
        initial_prompt: Some("Monitor {repo_url} on {branch}".to_string()),
        inputs: {
            let mut m = BTreeMap::new();
            m.insert("repo_url".to_string(), CrewInputSpec {
                description: "repo".to_string(),
                required: true,
                default: None,
            });
            m.insert("branch".to_string(), CrewInputSpec {
                description: "branch".to_string(),
                required: false,
                default: Some("main".to_string()),
            });
            m
        },
        main_agent: "coordinator".to_string(),
        agents: {
            let mut m = BTreeMap::new();
            m.insert("coordinator".to_string(), CrewAgentDef {
                role: None,
                model: Some("gpt-4o-mini".to_string()),
                instructions: Some("Fix bugs on {repo_url} branch {branch}".to_string()),
                description: Some("Coordinates".to_string()),
            });
            m
        },
        approval_policy: None,
    };

    let mut inputs = BTreeMap::new();
    inputs.insert("repo_url".to_string(), "https://github.com/test/repo".to_string());

    let mut base = SessionWorkflowInput {
        user_message: String::new(),
        model: "gpt-4o".to_string(),
        instructions: "default instructions".to_string(),
        approval_policy: codex_protocol::protocol::AskForApproval::OnRequest,
        web_search_mode: None,
        reasoning_effort: None,
        reasoning_summary: codex_protocol::config_types::ReasoningSummary::Auto,
        personality: None,
        developer_instructions: None,
        model_provider: None,
        crew_agents: BTreeMap::new(),
        continued_state: None,
    };

    apply_crew_type(&crew, &inputs, &mut base).unwrap();

    // Model overridden from main agent.
    assert_eq!(base.model, "gpt-4o-mini");
    // Instructions interpolated.
    assert_eq!(
        base.instructions,
        "Fix bugs on https://github.com/test/repo branch main"
    );
    // User message set from initial_prompt (autonomous mode).
    assert_eq!(
        base.user_message,
        "Monitor https://github.com/test/repo on main"
    );
}

#[test]
fn apply_crew_type_rejects_missing_required_input() {
    use std::collections::BTreeMap;
    use codex_temporal::config_loader::apply_crew_type;
    use codex_temporal::types::SessionWorkflowInput;

    let crew = CrewType {
        name: "test".to_string(),
        description: "test".to_string(),
        mode: CrewMode::Autonomous,
        initial_prompt: Some("{repo_url}".to_string()),
        inputs: {
            let mut m = BTreeMap::new();
            m.insert("repo_url".to_string(), CrewInputSpec {
                description: "repo".to_string(),
                required: true,
                default: None,
            });
            m
        },
        main_agent: "default".to_string(),
        agents: BTreeMap::new(),
        approval_policy: None,
    };

    let empty_inputs = BTreeMap::new();
    let mut base = SessionWorkflowInput {
        user_message: String::new(),
        model: "gpt-4o".to_string(),
        instructions: String::new(),
        approval_policy: codex_protocol::protocol::AskForApproval::OnRequest,
        web_search_mode: None,
        reasoning_effort: None,
        reasoning_summary: codex_protocol::config_types::ReasoningSummary::Auto,
        personality: None,
        developer_instructions: None,
        model_provider: None,
        crew_agents: BTreeMap::new(),
        continued_state: None,
    };

    let err = apply_crew_type(&crew, &empty_inputs, &mut base);
    assert!(err.is_err());
    assert!(
        err.unwrap_err().to_string().contains("missing required input"),
        "should mention missing required input"
    );
}

#[test]
fn apply_crew_type_uses_input_defaults() {
    use std::collections::BTreeMap;
    use codex_temporal::config_loader::apply_crew_type;
    use codex_temporal::types::SessionWorkflowInput;

    let crew = CrewType {
        name: "test".to_string(),
        description: "test".to_string(),
        mode: CrewMode::Autonomous,
        initial_prompt: Some("branch: {branch}".to_string()),
        inputs: {
            let mut m = BTreeMap::new();
            m.insert("branch".to_string(), CrewInputSpec {
                description: "branch".to_string(),
                required: false,
                default: Some("develop".to_string()),
            });
            m
        },
        main_agent: "default".to_string(),
        agents: BTreeMap::new(),
        approval_policy: None,
    };

    let empty_inputs = BTreeMap::new();
    let mut base = SessionWorkflowInput {
        user_message: String::new(),
        model: "gpt-4o".to_string(),
        instructions: String::new(),
        approval_policy: codex_protocol::protocol::AskForApproval::OnRequest,
        web_search_mode: None,
        reasoning_effort: None,
        reasoning_summary: codex_protocol::config_types::ReasoningSummary::Auto,
        personality: None,
        developer_instructions: None,
        model_provider: None,
        crew_agents: BTreeMap::new(),
        continued_state: None,
    };

    apply_crew_type(&crew, &empty_inputs, &mut base).unwrap();

    // Default value "develop" should be used.
    assert_eq!(base.user_message, "branch: develop");
}

#[test]
fn discover_crew_types_reads_directory() {
    use codex_temporal::config_loader::discover_crew_types;

    // Set CODEX_HOME to a temp directory with a crews/ subdirectory.
    let tmp = tempfile::tempdir().unwrap();
    let crews_dir = tmp.path().join("crews");
    std::fs::create_dir_all(&crews_dir).unwrap();

    // Write two crew TOML files.
    std::fs::write(
        crews_dir.join("alpha.toml"),
        r#"
name = "alpha"
description = "Alpha crew"
"#,
    )
    .unwrap();
    std::fs::write(
        crews_dir.join("beta.toml"),
        r#"
name = "beta"
description = "Beta crew"
mode = "autonomous"
initial_prompt = "Do beta things."
"#,
    )
    .unwrap();

    // Write a non-TOML file (should be ignored).
    std::fs::write(crews_dir.join("readme.txt"), "ignore me").unwrap();

    // Temporarily override CODEX_HOME.
    let _guard = EnvGuard::set("CODEX_HOME", tmp.path().to_str().unwrap());

    let crews = discover_crew_types().unwrap();
    assert_eq!(crews.len(), 2);
    assert_eq!(crews[0].name, "alpha");
    assert_eq!(crews[1].name, "beta");
    assert_eq!(crews[1].mode, CrewMode::Autonomous);
}

/// RAII guard that sets an env var for the duration of its lifetime
/// and restores the previous value on drop.
struct EnvGuard {
    key: String,
    previous: Option<String>,
}

impl EnvGuard {
    fn set(key: &str, value: &str) -> Self {
        let previous = std::env::var(key).ok();
        // SAFETY: test code — no concurrent env access.
        unsafe { std::env::set_var(key, value) };
        Self {
            key: key.to_string(),
            previous,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.previous {
            // SAFETY: test code — no concurrent env access.
            Some(v) => unsafe { std::env::set_var(&self.key, v) },
            None => unsafe { std::env::remove_var(&self.key) },
        }
    }
}

// ---------------------------------------------------------------------------
// Crew subagent scoping tests
// ---------------------------------------------------------------------------

#[test]
fn session_workflow_input_crew_agents_roundtrip() {
    use codex_temporal::types::SessionWorkflowInput;

    let mut crew_agents = BTreeMap::new();
    crew_agents.insert(
        "helper".to_string(),
        CrewAgentDef {
            role: None,
            model: Some("gpt-4o-mini".to_string()),
            instructions: Some("Help with tasks.".to_string()),
            description: Some("A helpful assistant".to_string()),
        },
    );
    crew_agents.insert(
        "reviewer".to_string(),
        CrewAgentDef {
            role: Some("explorer".to_string()),
            model: None,
            instructions: None,
            description: Some("Reviews code changes".to_string()),
        },
    );

    let input = SessionWorkflowInput {
        user_message: "test".to_string(),
        model: "gpt-4o".to_string(),
        instructions: "Be helpful.".to_string(),
        approval_policy: Default::default(),
        web_search_mode: None,
        reasoning_effort: None,
        reasoning_summary: codex_protocol::config_types::ReasoningSummary::Auto,
        personality: None,
        developer_instructions: None,
        model_provider: None,
        crew_agents: crew_agents.clone(),
        continued_state: None,
    };

    let json = serde_json::to_string(&input).unwrap();
    let back: SessionWorkflowInput = serde_json::from_str(&json).unwrap();

    assert_eq!(back.crew_agents.len(), 2);
    assert!(back.crew_agents.contains_key("helper"));
    assert!(back.crew_agents.contains_key("reviewer"));

    let helper = &back.crew_agents["helper"];
    assert_eq!(helper.model.as_deref(), Some("gpt-4o-mini"));
    assert_eq!(helper.instructions.as_deref(), Some("Help with tasks."));
    assert!(helper.role.is_none());

    let reviewer = &back.crew_agents["reviewer"];
    assert_eq!(reviewer.role.as_deref(), Some("explorer"));
    assert!(reviewer.model.is_none());
}

#[test]
fn session_workflow_input_crew_agents_default_when_missing() {
    use codex_temporal::types::SessionWorkflowInput;

    // JSON without crew_agents field — should default to empty.
    let json = r#"{
        "user_message": "hello",
        "model": "gpt-4o",
        "instructions": "test"
    }"#;

    let input: SessionWorkflowInput = serde_json::from_str(json).unwrap();
    assert!(
        input.crew_agents.is_empty(),
        "crew_agents should default to empty"
    );
}

#[test]
fn apply_crew_type_populates_crew_agents() {
    use codex_temporal::config_loader::apply_crew_type;
    use codex_temporal::types::SessionWorkflowInput;

    let crew = CrewType {
        name: "test".to_string(),
        description: "test crew".to_string(),
        mode: CrewMode::Autonomous,
        initial_prompt: Some("Work on {task}".to_string()),
        inputs: {
            let mut m = BTreeMap::new();
            m.insert(
                "task".to_string(),
                CrewInputSpec {
                    description: "The task".to_string(),
                    required: true,
                    default: None,
                },
            );
            m
        },
        main_agent: "coordinator".to_string(),
        agents: {
            let mut m = BTreeMap::new();
            m.insert(
                "coordinator".to_string(),
                CrewAgentDef {
                    role: None,
                    model: Some("gpt-4o".to_string()),
                    instructions: Some("Coordinate {task}".to_string()),
                    description: Some("Main coordinator".to_string()),
                },
            );
            m.insert(
                "helper".to_string(),
                CrewAgentDef {
                    role: None,
                    model: Some("gpt-4o-mini".to_string()),
                    instructions: Some("Help with {task}".to_string()),
                    description: Some("Helper agent".to_string()),
                },
            );
            m.insert(
                "fixer".to_string(),
                CrewAgentDef {
                    role: Some("explorer".to_string()),
                    model: None,
                    instructions: None,
                    description: Some("Fixes bugs".to_string()),
                },
            );
            m
        },
        approval_policy: None,
    };

    let mut inputs = BTreeMap::new();
    inputs.insert("task".to_string(), "bug-fixing".to_string());

    let mut base = SessionWorkflowInput {
        user_message: String::new(),
        model: "default-model".to_string(),
        instructions: "default".to_string(),
        approval_policy: codex_protocol::protocol::AskForApproval::OnRequest,
        web_search_mode: None,
        reasoning_effort: None,
        reasoning_summary: codex_protocol::config_types::ReasoningSummary::Auto,
        personality: None,
        developer_instructions: None,
        model_provider: None,
        crew_agents: BTreeMap::new(),
        continued_state: None,
    };

    apply_crew_type(&crew, &inputs, &mut base).unwrap();

    // Main agent (coordinator) should NOT be in crew_agents.
    assert!(
        !base.crew_agents.contains_key("coordinator"),
        "main agent should not be in crew_agents"
    );

    // Non-main agents should be in crew_agents.
    assert_eq!(base.crew_agents.len(), 2);
    assert!(base.crew_agents.contains_key("helper"));
    assert!(base.crew_agents.contains_key("fixer"));

    // Helper should have interpolated instructions.
    let helper = &base.crew_agents["helper"];
    assert_eq!(helper.model.as_deref(), Some("gpt-4o-mini"));
    assert_eq!(
        helper.instructions.as_deref(),
        Some("Help with bug-fixing")
    );
    assert!(helper.role.is_none());

    // Fixer should have role reference but no interpolation (no instructions template).
    let fixer = &base.crew_agents["fixer"];
    assert_eq!(fixer.role.as_deref(), Some("explorer"));
    assert!(fixer.instructions.is_none());
    assert_eq!(fixer.description.as_deref(), Some("Fixes bugs"));
}

#[test]
fn inject_crew_roles_into_toml_adds_roles() {
    use codex_temporal::config_loader::inject_crew_roles_into_toml;

    let base_toml = r#"
model = "gpt-4o"
"#;

    let mut crew_agents = BTreeMap::new();
    crew_agents.insert(
        "helper".to_string(),
        CrewAgentDef {
            role: None,
            model: Some("gpt-4o-mini".to_string()),
            instructions: Some("Help out".to_string()),
            description: Some("A helper agent".to_string()),
        },
    );
    crew_agents.insert(
        "reviewer".to_string(),
        CrewAgentDef {
            role: None,
            model: None,
            instructions: None,
            description: None,
        },
    );

    let result = inject_crew_roles_into_toml(base_toml, &crew_agents).unwrap();

    // Parse result and verify [agents] entries exist.
    let doc: toml::Value = toml::from_str(&result).unwrap();
    let agents = doc.get("agents").and_then(|v| v.as_table()).unwrap();

    assert!(agents.contains_key("helper"), "helper should be in agents table");
    assert!(agents.contains_key("reviewer"), "reviewer should be in agents table");

    // Helper should have description.
    let helper = agents.get("helper").and_then(|v| v.as_table()).unwrap();
    assert_eq!(
        helper.get("description").and_then(|v| v.as_str()),
        Some("A helper agent")
    );

    // Reviewer has no description — table exists but may be empty.
    assert!(agents.contains_key("reviewer"));
}

#[test]
fn session_continue_as_new_state_crew_agents_roundtrip() {
    use codex_temporal::types::{AgentLifecycle, AgentRecord, SessionContinueAsNewState};

    let mut crew_agents = BTreeMap::new();
    crew_agents.insert(
        "worker".to_string(),
        CrewAgentDef {
            role: None,
            model: Some("gpt-4o-mini".to_string()),
            instructions: Some("Do work.".to_string()),
            description: Some("Worker agent".to_string()),
        },
    );

    let state = SessionContinueAsNewState {
        agents: vec![AgentRecord {
            agent_id: "session/main".to_string(),
            workflow_id: "session/main".to_string(),
            role: "default".to_string(),
            status: AgentLifecycle::Running,
        }],
        config_toml: "model = \"gpt-4o\"".to_string(),
        project_context: ProjectContextOutput {
            cwd: "/tmp".to_string(),
            user_instructions: None,
            git_info: None,
        },
        mcp_tools: std::collections::HashMap::new(),
        crew_agents: crew_agents.clone(),
    };

    let json = serde_json::to_string(&state).unwrap();
    let back: SessionContinueAsNewState = serde_json::from_str(&json).unwrap();

    assert_eq!(back.crew_agents.len(), 1);
    assert!(back.crew_agents.contains_key("worker"));
    let worker = &back.crew_agents["worker"];
    assert_eq!(worker.model.as_deref(), Some("gpt-4o-mini"));
    assert_eq!(worker.instructions.as_deref(), Some("Do work."));
}
