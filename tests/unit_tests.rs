//! Unit and integration tests for the codex-temporal harness.

use std::sync::Arc;

use codex_temporal::entropy::{TemporalClock, TemporalRandomSource};
use codex_temporal::sink::BufferEventSink;
use codex_temporal::storage::InMemoryStorage;
use codex_temporal::types::{
    ApprovalInput, CodexWorkflowInput, CodexWorkflowOutput, PendingApproval, ToolExecOutput,
    UserTurnInput,
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
    };

    let json = serde_json::to_string(&output).unwrap();
    let back: CodexWorkflowOutput = serde_json::from_str(&json).unwrap();

    assert_eq!(back.last_agent_message, output.last_agent_message);
    assert_eq!(back.iterations, output.iterations);
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
    let input = UserTurnInput {
        turn_id: "turn-42".to_string(),
        message: "What is 2+2?".to_string(),
    };

    let json = serde_json::to_string(&input).unwrap();
    let back: UserTurnInput = serde_json::from_str(&json).unwrap();

    assert_eq!(back.turn_id, "turn-42");
    assert_eq!(back.message, "What is 2+2?");
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
