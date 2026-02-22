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
        approval_policy: Default::default(),
        web_search_mode: None,
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
// Tool dispatch integration tests
// ---------------------------------------------------------------------------
// These exercise the full build_specs → ToolRegistry::dispatch pipeline
// without a Temporal worker or model API call.

use codex_temporal::activities::dispatch_tool;
use codex_temporal::types::ToolExecInput;

/// Helper to build a ToolExecInput for a given tool.
fn tool_input(tool_name: &str, arguments: &str) -> ToolExecInput {
    ToolExecInput {
        tool_name: tool_name.to_string(),
        call_id: format!("test-call-{}", uuid::Uuid::new_v4()),
        arguments: arguments.to_string(),
        model: "gpt-4o".to_string(),
        cwd: "/tmp".to_string(),
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
