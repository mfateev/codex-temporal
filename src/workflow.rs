//! The Temporal workflow that drives the codex agentic loop.
//!
//! This is a long-lived, interactive workflow that supports multi-turn
//! conversation, tool approval, and event streaming — all driven by the
//! `AgentSession` interface on the client side.
//!
//! ## Protocol
//!
//! - **`receive_user_turn` signal**: Queues a new user turn for processing.
//! - **`receive_approval` signal**: Resolves a pending tool-call approval.
//! - **`request_shutdown` signal**: Requests graceful workflow termination.
//! - **`get_events_since` query**: Returns JSON-serialized events from a
//!   given index (for client polling).
//!
//! The `#[run]` method loops: wait for a user turn → run the agentic loop →
//! emit `TurnComplete` → repeat, until shutdown is requested.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use codex_core::config::Config;
use codex_core::entropy::{EntropyProviders, ENTROPY};
use codex_core::models_manager::manager::ModelsManager;
use codex_core::{
    EventSink, Prompt, Session, StorageBackend, ToolSpec,
    TurnContext, TurnDiffTracker, ToolsConfig, ToolsConfigParams, build_specs,
    try_run_sampling_request,
};
use codex_protocol::models::{BaseInstructions, ContentItem, ResponseItem};
use codex_protocol::protocol::{Event, EventMsg, TurnCompleteEvent, TurnStartedEvent};
use codex_protocol::ThreadId;
use temporalio_macros::{workflow, workflow_methods};
use temporalio_sdk::{SyncWorkflowContext, WorkflowContext, WorkflowContextView, WorkflowResult};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::entropy::{TemporalClock, TemporalRandomSource};
use crate::sink::BufferEventSink;
use crate::storage::InMemoryStorage;
use crate::streamer::TemporalModelStreamer;
use crate::tools::TemporalToolHandler;
use crate::types::{
    ApprovalInput, CodexWorkflowInput, CodexWorkflowOutput, PendingApproval, UserTurnInput,
};

/// Maximum number of model→tool loop iterations per turn.
const MAX_ITERATIONS: u32 = 50;

#[workflow]
pub struct CodexWorkflow {
    input: CodexWorkflowInput,
    pub(crate) events: Arc<BufferEventSink>,
    /// Queue of user turns waiting to be processed.
    user_turns: Vec<UserTurnInput>,
    /// Pending tool-call approval (set by tool handler, resolved by signal).
    pub(crate) pending_approval: Option<PendingApproval>,
    /// When true the workflow will exit after the current turn completes.
    shutdown_requested: bool,
}

#[workflow_methods]
impl CodexWorkflow {
    #[init]
    pub fn new(_ctx: &WorkflowContextView, input: CodexWorkflowInput) -> Self {
        // If the workflow input contains a non-empty user_message, seed it as
        // the first turn so the workflow starts processing immediately.
        let initial_turns = if input.user_message.is_empty() {
            Vec::new()
        } else {
            vec![UserTurnInput {
                turn_id: "turn-0".to_string(),
                message: input.user_message.clone(),
            }]
        };

        Self {
            input,
            events: Arc::new(BufferEventSink::new()),
            user_turns: initial_turns,
            pending_approval: None,
            shutdown_requested: false,
        }
    }

    // ----- signals -----

    /// Queue a new user turn for processing.
    #[signal]
    pub fn receive_user_turn(
        &mut self,
        _ctx: &mut SyncWorkflowContext<Self>,
        input: UserTurnInput,
    ) {
        self.user_turns.push(input);
    }

    /// Resolve a pending tool-call approval.
    #[signal]
    pub fn receive_approval(
        &mut self,
        _ctx: &mut SyncWorkflowContext<Self>,
        input: ApprovalInput,
    ) {
        if let Some(ref mut pa) = self.pending_approval {
            if pa.call_id == input.call_id {
                pa.decision = Some(input.approved);
            }
        }
    }

    /// Request graceful shutdown after the current turn finishes.
    #[signal]
    pub fn request_shutdown(&mut self, _ctx: &mut SyncWorkflowContext<Self>) {
        self.shutdown_requested = true;
    }

    // ----- queries -----

    /// Return JSON-serialized events starting from `from_index`.
    ///
    /// Returns `(events_json[], new_watermark)` encoded as a JSON string.
    #[query]
    pub fn get_events_since(&self, _ctx: &WorkflowContextView, from_index: usize) -> String {
        let (events, watermark) = self.events.events_since(from_index);
        serde_json::json!({
            "events": events,
            "watermark": watermark,
        })
        .to_string()
    }

    // ----- run -----

    #[run]
    pub async fn run(ctx: &mut WorkflowContext<Self>) -> WorkflowResult<CodexWorkflowOutput> {
        let input = ctx.state(|s| s.input.clone());
        let events = ctx.state(|s| s.events.clone());

        // --- deterministic entropy ---
        let seed = ctx.random_seed();
        let wf_time = ctx.workflow_time().unwrap_or(SystemTime::UNIX_EPOCH);
        let entropy = EntropyProviders {
            random: Arc::new(TemporalRandomSource::new(seed)),
            clock: Arc::new(TemporalClock::new(wf_time)),
        };

        // --- config ---
        let codex_home = PathBuf::from("/tmp/codex-temporal");
        let mut config = Config::for_harness(codex_home)
            .map_err(|e| anyhow::anyhow!("failed to build config: {e}"))?;
        config.model = Some(input.model.clone());
        let config = Arc::new(config);

        // --- model info ---
        let model_slug = ModelsManager::get_model_offline_for_tests(config.model.as_deref());
        let model_info =
            ModelsManager::construct_model_info_offline_for_tests(&model_slug, &config);

        // --- session ---
        let conversation_id = ThreadId::new();
        let event_sink: Arc<dyn EventSink> = events.clone();
        let storage: Arc<dyn StorageBackend> = Arc::new(InMemoryStorage::new());

        let sess = Session::new_minimal(
            conversation_id,
            Arc::clone(&config),
            event_sink,
            storage,
        )
        .await;

        // --- tools ---
        // Use codex-core's build_specs to get the full set of tool specs
        // (shell, apply_patch, read_file, list_dir, grep_files, etc.).
        let tools_config = ToolsConfig::new(&ToolsConfigParams {
            model_info: &model_info,
            features: &config.features,
            web_search_mode: None,
        });
        let builder = build_specs(&tools_config, None, None, &[]);
        let (configured_specs, _registry) = builder.build();
        let tools: Vec<ToolSpec> = configured_specs.into_iter().map(|cs| cs.spec).collect();
        let base_instructions = BaseInstructions {
            text: input.instructions.clone(),
        };

        let mut total_iterations = 0u32;
        let mut last_agent_message: Option<String> = None;
        let mut turn_counter = 0u32;

        // === main loop: wait for turns, process them, repeat ===
        ENTROPY
            .scope(entropy, async {
                loop {
                    // Wait until we have a user turn to process or shutdown.
                    ctx.wait_condition(|s| !s.user_turns.is_empty() || s.shutdown_requested)
                        .await;

                    // Check shutdown before processing.
                    let shutdown = ctx.state(|s| s.shutdown_requested);
                    if shutdown {
                        let remaining = ctx.state(|s| s.user_turns.is_empty());
                        if remaining {
                            break;
                        }
                    }

                    // Dequeue the next turn.
                    let turn = ctx.state_mut(|s| s.user_turns.remove(0));
                    turn_counter += 1;

                    let turn_id = turn.turn_id.clone();

                    // Emit TurnStarted
                    events.emit_event_sync(Event {
                        id: turn_id.clone(),
                        msg: EventMsg::TurnStarted(TurnStartedEvent {
                            turn_id: turn_id.clone(),
                            model_context_window: None,
                            collaboration_mode_kind: Default::default(),
                        }),
                    });

                    // Seed user message into session history.
                    let user_item = ResponseItem::Message {
                        id: None,
                        role: "user".to_string(),
                        content: vec![ContentItem::InputText {
                            text: turn.message.clone(),
                        }],
                        end_turn: None,
                        phase: None,
                    };

                    let turn_context = Arc::new(TurnContext::new_minimal(
                        turn_id.clone(),
                        model_info.clone(),
                        Arc::clone(&config),
                    ));

                    sess.record_items(&turn_context, &[user_item]).await;

                    // --- run the agentic loop for this turn ---
                    let mut streamer = TemporalModelStreamer::new(ctx.clone());
                    let handler = TemporalToolHandler::new(
                        ctx.clone(),
                        events.clone(),
                        turn_id.clone(),
                        input.approval_policy,
                        input.model.clone(),
                        config.cwd.to_string_lossy().to_string(),
                    );

                    let diff_tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));
                    let cancellation_token = CancellationToken::new();
                    let mut iterations = 0u32;

                    loop {
                        if iterations >= MAX_ITERATIONS {
                            tracing::warn!("max iterations reached ({MAX_ITERATIONS}), stopping turn");
                            break;
                        }

                        // Rebuild prompt from accumulated session history.
                        let history = sess.history_items().await;
                        let prompt = Prompt {
                            input: history,
                            tools: tools.clone(),
                            parallel_tool_calls: false,
                            base_instructions: base_instructions.clone(),
                            personality: None,
                            output_schema: None,
                        };

                        let mut server_model_warning_emitted = false;
                        let result = try_run_sampling_request(
                            Arc::clone(&sess),
                            Arc::clone(&turn_context),
                            &mut streamer,
                            &handler,
                            None,
                            Arc::clone(&diff_tracker),
                            &mut server_model_warning_emitted,
                            &prompt,
                            cancellation_token.child_token(),
                        )
                        .await;

                        iterations += 1;
                        total_iterations += 1;

                        match result {
                            Ok(outcome) => {
                                if let Some(msg) = outcome.last_agent_message {
                                    last_agent_message = Some(msg);
                                }
                                if !outcome.needs_follow_up {
                                    break;
                                }
                                tracing::info!(
                                    iteration = iterations,
                                    "follow-up needed, continuing loop"
                                );
                            }
                            Err(e) => {
                                tracing::error!(error = %e, "try_run_sampling_request failed");
                                break;
                            }
                        }
                    }

                    // Emit TurnComplete
                    events.emit_event_sync(Event {
                        id: turn_id.clone(),
                        msg: EventMsg::TurnComplete(TurnCompleteEvent {
                            turn_id,
                            last_agent_message: last_agent_message.clone(),
                        }),
                    });

                    // Check if shutdown was requested during this turn.
                    let shutdown = ctx.state(|s| s.shutdown_requested);
                    if shutdown {
                        break;
                    }
                }
            })
            .await;

        // Emit ShutdownComplete
        events.emit_event_sync(Event {
            id: String::new(),
            msg: EventMsg::ShutdownComplete,
        });

        Ok(CodexWorkflowOutput {
            last_agent_message,
            iterations: total_iterations,
        })
    }

    /// Legacy query — returns debug-formatted events (kept for backward compat).
    #[query]
    pub fn get_events(&self, _ctx: &WorkflowContextView) -> Vec<String> {
        self.events.drain().iter().map(|e| format!("{e:?}")).collect()
    }
}

/// Re-export the macro-generated `Run` marker type so other modules (e.g.
/// `session.rs`) can parameterize `WorkflowHandle<Client, CodexWorkflowRun>`.
pub use codex_workflow::Run as CodexWorkflowRun;
