//! Temporal-backed TUI for the Codex agent.
//!
//! This binary reuses the codex TUI rendering (ChatWidget) but connects to a
//! Temporal workflow instead of an in-process CodexThread. It does not need
//! config files, auth credentials, or HTTP calls — the worker has the API key.
//!
//! Usage:
//!   codex-temporal-tui "your prompt here"
//!
//! Environment variables:
//!   TEMPORAL_ADDRESS  — Temporal server URL (default: http://localhost:7233)
//!   CODEX_MODEL       — Model name to display in the TUI header (default: gpt-4o)

use std::io::stdout;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use codex_core::config::Config;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::SessionConfiguredEvent;
use codex_protocol::ThreadId;

use codex_temporal::auth_stub::NoopAuthProvider;
use codex_temporal::models_stub::FixedModelsProvider;
use codex_temporal::session::TemporalAgentSession;
use codex_temporal::types::CodexWorkflowInput;

use codex_tui::app_event::AppEvent;
use codex_tui::app_event_sender::AppEventSender;
use codex_tui::bottom_pane::FeedbackAudience;
use codex_tui::chatwidget::ChatWidget;
use codex_tui::chatwidget::ChatWidgetInit;
use codex_tui::chatwidget::wire_session;
use codex_tui::custom_terminal::Terminal;
use codex_tui::tui::FrameRequester;

use crossterm::event::Event as CrosstermEvent;
use crossterm::event::EventStream;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use codex_tui::render::renderable::Renderable;

use temporalio_client::{Client, ClientOptions, Connection, ConnectionOptions};
use temporalio_common::telemetry::TelemetryOptions;
use temporalio_sdk_core::{CoreRuntime, RuntimeOptions, Url};

use tokio::sync::broadcast;
use tokio::sync::mpsc::unbounded_channel;
use tokio_stream::StreamExt;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".parse().unwrap()),
        )
        .with_writer(std::io::stderr)
        .init();

    let initial_prompt = std::env::args().nth(1);

    let server_url = std::env::var("TEMPORAL_ADDRESS")
        .unwrap_or_else(|_| "http://localhost:7233".to_string());
    let model = std::env::var("CODEX_MODEL").unwrap_or_else(|_| "gpt-4o".to_string());
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    // --- Connect to Temporal ---
    let connection_options = ConnectionOptions::new(Url::from_str(&server_url)?)
        .identity("codex-temporal-tui")
        .build();
    let telemetry_options = TelemetryOptions::builder().build();
    let runtime_options = RuntimeOptions::builder()
        .telemetry_options(telemetry_options)
        .build()?;
    let _runtime = CoreRuntime::new_assume_tokio(runtime_options)?;
    let connection = Connection::connect(connection_options).await?;
    let client = Client::new(connection, ClientOptions::new("default").build())?;

    // --- Create TemporalAgentSession ---
    let workflow_id = format!("codex-tui-{}", uuid::Uuid::new_v4());
    let base_input = CodexWorkflowInput {
        user_message: String::new(),
        model: model.clone(),
        instructions: "You are a helpful coding assistant.".to_string(),
    };
    let session = Arc::new(TemporalAgentSession::new(
        client,
        workflow_id,
        base_input,
    ));

    // --- Build SessionConfiguredEvent ---
    let session_configured = SessionConfiguredEvent {
        session_id: ThreadId::new(),
        forked_from_id: None,
        thread_name: None,
        model: model.clone(),
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

    // --- Set up TUI terminal ---
    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::with_options(backend)?;

    // --- Create event channels ---
    let (app_event_tx_raw, mut app_event_rx) = unbounded_channel::<AppEvent>();
    let app_event_tx = AppEventSender::new(app_event_tx_raw);

    // --- Wire session → get op_tx ---
    let op_tx = wire_session(session, session_configured, app_event_tx.clone());

    // --- Create stub providers ---
    let auth_manager: Arc<dyn codex_core::AuthProvider> = Arc::new(NoopAuthProvider);
    let models_manager: Arc<dyn codex_core::ModelsProvider> =
        Arc::new(FixedModelsProvider::new(model.clone()));

    // --- Build Config ---
    let codex_home = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".codex");
    let mut config = Config::for_harness(codex_home)?;
    config.model = Some(model.clone());
    config.cwd = cwd;

    // --- Build FrameRequester ---
    let (draw_tx, mut draw_rx) = broadcast::channel::<()>(16);
    let frame_requester = FrameRequester::new(draw_tx);

    // --- Build OtelManager ---
    let otel_manager = codex_otel::OtelManager::new(
        ThreadId::new(),
        &model,
        &model,
        None,
        None,
        None,
        "codex-temporal-tui".to_string(),
        false,
        "temporal-tui".to_string(),
        codex_protocol::protocol::SessionSource::Cli,
    );

    // --- Build ChatWidgetInit ---
    let initial_user_message = initial_prompt.map(|text| text.into());
    let init = ChatWidgetInit {
        config,
        frame_requester: frame_requester.clone(),
        app_event_tx: app_event_tx.clone(),
        initial_user_message,
        enhanced_keys_supported: false,
        auth_manager,
        models_manager,
        feedback: codex_feedback::CodexFeedback::new(),
        is_first_run: false,
        feedback_audience: FeedbackAudience::External,
        model: Some(model),
        status_line_invalid_items_warned: Arc::new(AtomicBool::new(false)),
        otel_manager,
    };

    // --- Create ChatWidget ---
    let mut chat_widget = ChatWidget::new_with_op_sender(init, op_tx);

    // --- Initial draw ---
    // The custom terminal starts with a zero-sized viewport. Set it to the
    // full screen size so the widget renders and knows its dimensions.
    let screen_size = terminal.size()?;
    terminal.set_viewport_area(ratatui::layout::Rect::new(
        0,
        0,
        screen_size.width,
        screen_size.height,
    ));
    terminal.clear()?;
    terminal.draw(|f| {
        let area = f.area();
        chat_widget.render(area, f.buffer_mut());
    })?;

    // --- Event loop ---
    let mut crossterm_events = EventStream::new();

    loop {
        tokio::select! {
            // App events from the agent session
            app_event = app_event_rx.recv() => {
                match app_event {
                    Some(AppEvent::CodexEvent(event)) => {
                        chat_widget.handle_codex_event(event);
                        frame_requester.schedule_frame();
                    }
                    Some(AppEvent::Exit(..)) | Some(AppEvent::FatalExitRequest(..)) | None => {
                        break;
                    }
                    Some(_other) => {
                        // Ignore other app events for now
                        frame_requester.schedule_frame();
                    }
                }
            }

            // Terminal key/resize events
            crossterm_event = crossterm_events.next() => {
                match crossterm_event {
                    Some(Ok(CrosstermEvent::Key(key_event))) => {
                        chat_widget.handle_key_event(key_event);
                        frame_requester.schedule_frame();
                    }
                    Some(Ok(CrosstermEvent::Resize(_, _))) => {
                        frame_requester.schedule_frame();
                    }
                    Some(Err(_)) | None => {
                        break;
                    }
                    _ => {}
                }
            }

            // Draw events from frame requester
            _ = draw_rx.recv() => {
                terminal.draw(|f| {
                    let area = f.area();
                    chat_widget.render(area, f.buffer_mut());
                })?;
            }
        }
    }

    // --- Restore terminal ---
    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;

    Ok(())
}
