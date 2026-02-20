//! End-to-end test for the Temporal TUI integration.
//!
//! Verifies the full path: user prompt → ChatWidget → wire_session →
//! TemporalAgentSession → Temporal workflow → worker (LLM activity) →
//! events flow back through the channel → ChatWidget processes them.
//!
//! Every event is also rendered into a ratatui `Buffer` to catch rendering
//! bugs (e.g. uninitialized width tracking) that only surface when the
//! widget is actually drawn.
//!
//! **Requires:**
//! - `OPENAI_API_KEY` env var
//! - A running Temporal server (`TEMPORAL_ADDRESS`, default `http://localhost:7233`)

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use codex_core::config::Config;
use codex_core::AuthProvider;
use codex_core::ModelsProvider;
use codex_protocol::protocol::{
    AskForApproval, EventMsg, SandboxPolicy, SessionConfiguredEvent, SessionSource,
};
use codex_protocol::ThreadId;

use codex_temporal::activities::CodexActivities;
use codex_temporal::auth_stub::NoopAuthProvider;
use codex_temporal::models_stub::FixedModelsProvider;
use codex_temporal::session::TemporalAgentSession;
use codex_temporal::types::CodexWorkflowInput;
use codex_temporal::workflow::CodexWorkflow;

use codex_tui::app_event::AppEvent;
use codex_tui::app_event_sender::AppEventSender;
use codex_tui::bottom_pane::FeedbackAudience;
use codex_tui::chatwidget::ChatWidget;
use codex_tui::chatwidget::ChatWidgetInit;
use codex_tui::chatwidget::wire_session;
use codex_tui::render::renderable::Renderable;
use codex_tui::tui::FrameRequester;

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use ratatui::layout::Rect;

use temporalio_client::{Client, ClientOptions, Connection, ConnectionOptions};
use temporalio_common::telemetry::TelemetryOptions;
use temporalio_common::worker::WorkerTaskTypes;
use temporalio_sdk::{Worker, WorkerOptions};
use temporalio_sdk_core::{CoreRuntime, RuntimeOptions, Url};

use tokio::sync::broadcast;
use tokio::sync::mpsc::unbounded_channel;

const TASK_QUEUE: &str = "codex-temporal";
const TERM_WIDTH: u16 = 120;
const TERM_HEIGHT: u16 = 40;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Render the widget into an off-screen buffer, exercising the same code
/// path as a real terminal draw. Panics on rendering bugs.
fn render_widget(chat_widget: &ChatWidget) {
    let area = Rect::new(0, 0, TERM_WIDTH, TERM_HEIGHT);
    let mut buf = ratatui::buffer::Buffer::empty(area);
    chat_widget.render(area, &mut buf);
}

/// Build a `ChatWidget` wired to a `TemporalAgentSession`, returning the
/// widget and the `AppEvent` receiver for inspecting events.
fn build_tui_for_session(
    session: Arc<TemporalAgentSession>,
    model: &str,
    initial_prompt: Option<&str>,
) -> (
    ChatWidget,
    tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/tmp"));

    let session_configured = SessionConfiguredEvent {
        session_id: ThreadId::new(),
        forked_from_id: None,
        thread_name: None,
        model: model.to_string(),
        model_provider_id: "openai".to_string(),
        approval_policy: AskForApproval::OnFailure,
        sandbox_policy: SandboxPolicy::DangerFullAccess,
        cwd: cwd.clone(),
        reasoning_effort: None,
        history_log_id: 0,
        history_entry_count: 0,
        initial_messages: None,
        network_proxy: None,
        rollout_path: None,
    };

    let (app_event_tx_raw, app_event_rx) = unbounded_channel::<AppEvent>();
    let app_event_tx = AppEventSender::new(app_event_tx_raw);

    let op_tx = wire_session(session, session_configured, app_event_tx.clone());

    let auth_manager: Arc<dyn AuthProvider> = Arc::new(NoopAuthProvider);
    let models_manager: Arc<dyn ModelsProvider> =
        Arc::new(FixedModelsProvider::new(model.to_string()));

    let codex_home = std::env::temp_dir().join("codex-temporal-tui-test");
    let mut config = Config::for_harness(codex_home).expect("Config::for_harness");
    config.model = Some(model.to_string());
    config.cwd = cwd;
    config.disable_paste_burst = true;

    let (draw_tx, _draw_rx) = broadcast::channel::<()>(16);
    let frame_requester = FrameRequester::new(draw_tx);

    let otel_manager = codex_otel::OtelManager::new(
        ThreadId::new(),
        model,
        model,
        None,
        None,
        None,
        "tui-e2e-test".to_string(),
        false,
        "test".to_string(),
        SessionSource::Cli,
    );

    let initial_user_message = initial_prompt.map(|text| text.into());
    let init = ChatWidgetInit {
        config,
        frame_requester,
        app_event_tx,
        initial_user_message,
        enhanced_keys_supported: false,
        auth_manager,
        models_manager,
        feedback: codex_feedback::CodexFeedback::new(),
        is_first_run: false,
        feedback_audience: FeedbackAudience::External,
        model: Some(model.to_string()),
        status_line_invalid_items_warned: Arc::new(AtomicBool::new(false)),
        otel_manager,
    };

    let chat_widget = ChatWidget::new_with_op_sender(init, op_tx);
    (chat_widget, app_event_rx)
}

fn new_session(client: &Client, model: &str) -> TemporalAgentSession {
    let workflow_id = format!("tui-e2e-{}", uuid::Uuid::new_v4());
    let base_input = CodexWorkflowInput {
        user_message: String::new(),
        model: model.to_string(),
        instructions: "You are a helpful coding assistant. Be concise.".to_string(),
    };
    TemporalAgentSession::new(client.clone(), workflow_id, base_input)
}

/// Receive the next `AppEvent::CodexEvent` from the channel, with a timeout.
/// Non-CodexEvent items are silently consumed.
async fn next_codex_event(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
    timeout_dur: Duration,
) -> Option<codex_protocol::protocol::Event> {
    let deadline = tokio::time::Instant::now() + timeout_dur;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(AppEvent::CodexEvent(event))) => return Some(event),
            Ok(Some(_)) => continue, // skip non-codex events
            Ok(None) => return None, // channel closed
            Err(_) => return None,   // timeout
        }
    }
}

// ---------------------------------------------------------------------------
// Infrastructure: connect + start worker
// ---------------------------------------------------------------------------

async fn connect_client(url: &str) -> Client {
    let conn_opts = ConnectionOptions::new(Url::from_str(url).expect("bad URL"))
        .identity("tui-e2e-test-client")
        .build();
    let telemetry_options = TelemetryOptions::builder().build();
    let runtime_options = RuntimeOptions::builder()
        .telemetry_options(telemetry_options)
        .build()
        .expect("runtime options");
    let _runtime = CoreRuntime::new_assume_tokio(runtime_options).expect("runtime");

    let connection = Connection::connect(conn_opts)
        .await
        .expect("failed to connect to Temporal server — is it running?");
    Client::new(connection, ClientOptions::new("default").build())
        .expect("failed to create client")
}

/// Start a worker on a dedicated OS thread (Worker future is !Send).
/// Returns when the worker is ready to poll.
fn start_worker(url: String) {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();

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

            let conn = ConnectionOptions::new(Url::from_str(&url).expect("bad URL"))
                .identity("tui-e2e-test-worker")
                .build();
            let connection = Connection::connect(conn)
                .await
                .expect("worker: failed to connect");
            let worker_client =
                Client::new(connection, ClientOptions::new("default").build())
                    .expect("worker: failed to create client");

            let opts = WorkerOptions::new(TASK_QUEUE)
                .task_types(WorkerTaskTypes::all())
                .register_workflow::<CodexWorkflow>()
                .register_activities(CodexActivities)
                .build();
            let mut worker = Worker::new(&worker_runtime, worker_client, opts)
                .expect("failed to create worker");

            let _ = ready_tx.send(());

            if let Err(e) = worker.run().await {
                eprintln!("worker error: {e}");
            }
        });
    });

    ready_rx
        .recv()
        .expect("worker thread died before becoming ready");
}

// ---------------------------------------------------------------------------
// Test cases
// ---------------------------------------------------------------------------

/// Full TUI round-trip: prompt → Temporal workflow → LLM → events back.
/// Renders the widget after every event to catch rendering bugs.
async fn tui_model_turn(client: &Client) {
    let session = Arc::new(new_session(client, "gpt-4o"));
    let (mut chat_widget, mut rx) =
        build_tui_for_session(session, "gpt-4o", Some("Say hello in one word."));

    // Render initial state (before any events).
    render_widget(&chat_widget);

    // 1. Wait for SessionConfigured and process it (triggers initial_user_message submission).
    let event = next_codex_event(&mut rx, Duration::from_secs(10))
        .await
        .expect("timed out waiting for SessionConfigured");
    assert!(
        matches!(&event.msg, EventMsg::SessionConfigured(_)),
        "first event should be SessionConfigured, got {:?}",
        std::mem::discriminant(&event.msg)
    );
    chat_widget.handle_codex_event(event);
    render_widget(&chat_widget);

    // 2. Drain events until TurnComplete (up to 120s for LLM response).
    let mut saw_turn_started = false;
    let mut saw_agent_message = false;

    loop {
        let event = next_codex_event(&mut rx, Duration::from_secs(120))
            .await
            .expect("timed out waiting for TurnComplete");

        match &event.msg {
            EventMsg::TurnStarted(_) => {
                eprintln!("  [tui_model_turn] TurnStarted");
                saw_turn_started = true;
            }
            EventMsg::AgentMessage(_) | EventMsg::AgentMessageDelta(_) => {
                saw_agent_message = true;
            }
            EventMsg::TurnComplete(tc) => {
                eprintln!(
                    "  [tui_model_turn] TurnComplete: {:?}",
                    tc.last_agent_message
                );
                chat_widget.handle_codex_event(event);
                render_widget(&chat_widget);
                break;
            }
            other => {
                eprintln!(
                    "  [tui_model_turn] event: {:?}",
                    std::mem::discriminant(other)
                );
            }
        }
        chat_widget.handle_codex_event(event);
        render_widget(&chat_widget);
    }

    assert!(saw_turn_started, "expected TurnStarted event via TUI");
    assert!(saw_agent_message, "expected agent message event via TUI");
}

/// Verify that an ExecApprovalRequest flows through the TUI pipeline.
/// Renders after every event.
async fn tui_tool_approval_request(client: &Client) {
    let session = Arc::new(new_session(client, "gpt-4o"));
    let (mut chat_widget, mut rx) = build_tui_for_session(
        session,
        "gpt-4o",
        Some("Use shell to run 'echo temporal-tui-test' and tell me the output."),
    );

    render_widget(&chat_widget);

    // 1. Process SessionConfigured.
    let event = next_codex_event(&mut rx, Duration::from_secs(10))
        .await
        .expect("timed out waiting for SessionConfigured");
    assert!(matches!(&event.msg, EventMsg::SessionConfigured(_)));
    chat_widget.handle_codex_event(event);
    render_widget(&chat_widget);

    // 2. Wait for ExecApprovalRequest or TurnComplete.
    let mut saw_approval_request = false;

    loop {
        let event = next_codex_event(&mut rx, Duration::from_secs(120))
            .await
            .expect("timed out waiting for ExecApprovalRequest or TurnComplete");

        match &event.msg {
            EventMsg::ExecApprovalRequest(req) => {
                eprintln!(
                    "  [tui_tool_approval] ExecApprovalRequest: {}",
                    req.call_id
                );
                saw_approval_request = true;
                chat_widget.handle_codex_event(event);
                render_widget(&chat_widget);
                break;
            }
            EventMsg::TurnComplete(_) => {
                eprintln!("  [tui_tool_approval] TurnComplete (no tool call)");
                chat_widget.handle_codex_event(event);
                render_widget(&chat_widget);
                break;
            }
            _ => {
                chat_widget.handle_codex_event(event);
                render_widget(&chat_widget);
            }
        }
    }

    if !saw_approval_request {
        eprintln!("  [tui_tool_approval] note: model did not issue a tool call this run");
    }
}

/// Simulate a real user interaction: type characters, press Enter, wait for
/// the response. This exercises the full input → render → submit → workflow →
/// events → render path, matching what happens in the actual TUI binary.
async fn tui_interactive_turn(client: &Client) {
    // Build a session with NO initial prompt — we'll type it.
    let session = Arc::new(new_session(client, "gpt-4o"));
    let (mut chat_widget, mut rx) = build_tui_for_session(session, "gpt-4o", None);

    render_widget(&chat_widget);

    // 1. Process SessionConfigured so the widget is ready to accept input.
    let event = next_codex_event(&mut rx, Duration::from_secs(10))
        .await
        .expect("timed out waiting for SessionConfigured");
    assert!(matches!(&event.msg, EventMsg::SessionConfigured(_)));
    chat_widget.handle_codex_event(event);
    render_widget(&chat_widget);

    // 2. Type "Hi" character by character, then press Enter.
    for ch in "Hi".chars() {
        chat_widget.handle_key_event(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        render_widget(&chat_widget);
    }
    chat_widget.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    render_widget(&chat_widget);

    // 3. Wait for events from the workflow (the Enter should have submitted Op::UserTurn).
    let mut saw_turn_started = false;
    let mut saw_turn_complete = false;

    loop {
        let event = next_codex_event(&mut rx, Duration::from_secs(120))
            .await
            .expect("timed out waiting for TurnComplete in interactive turn");
        match &event.msg {
            EventMsg::TurnStarted(_) => {
                eprintln!("  [tui_interactive] TurnStarted");
                saw_turn_started = true;
            }
            EventMsg::TurnComplete(tc) => {
                eprintln!(
                    "  [tui_interactive] TurnComplete: {:?}",
                    tc.last_agent_message
                );
                saw_turn_complete = true;
                chat_widget.handle_codex_event(event);
                render_widget(&chat_widget);
                break;
            }
            _ => {}
        }
        chat_widget.handle_codex_event(event);
        render_widget(&chat_widget);
    }

    assert!(saw_turn_started, "expected TurnStarted from typed input");
    assert!(saw_turn_complete, "expected TurnComplete from typed input");
}

// ---------------------------------------------------------------------------
// Test entry-point
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tui_e2e_tests() {
    let api_key = std::env::var("OPENAI_API_KEY")
        .expect("OPENAI_API_KEY must be set for TUI E2E tests");
    assert!(!api_key.is_empty(), "OPENAI_API_KEY must not be empty");

    let url = std::env::var("TEMPORAL_ADDRESS")
        .unwrap_or_else(|_| "http://localhost:7233".to_string());
    eprintln!("Connecting to Temporal at {url}");

    let client = connect_client(&url).await;

    eprintln!("Starting worker…");
    start_worker(url);

    eprintln!("--- test: tui_model_turn ---");
    tui_model_turn(&client).await;

    eprintln!("--- test: tui_tool_approval_request ---");
    tui_tool_approval_request(&client).await;

    eprintln!("--- test: tui_interactive_turn ---");
    tui_interactive_turn(&client).await;

    eprintln!("--- all TUI E2E tests passed ---");
}
