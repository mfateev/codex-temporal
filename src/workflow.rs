//! The Temporal workflow that drives the codex agentic loop.
//!
//! This is a long-lived, interactive workflow that supports multi-turn
//! conversation, tool approval, and event streaming — all driven by the
//! `AgentSession` interface on the client side.
//!
//! ## Protocol
//!
//! - **`receive_op` signal**: Generic signal that dispatches all client
//!   operations (user turns, approvals, shutdown, compact, etc.).
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
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::models::{BaseInstructions, ContentItem, ResponseItem};
use codex_protocol::protocol::{
    ContextCompactedEvent, Event, EventMsg, Op, ReviewDecision, TurnCompleteEvent,
    TurnStartedEvent,
};
use codex_protocol::user_input::UserInput;
use codex_protocol::ThreadId;
use temporalio_macros::{workflow, workflow_methods};
use temporalio_common::protos::coresdk::workflow_commands::ContinueAsNewWorkflowExecution;
use temporalio_common::protos::coresdk::AsJsonPayloadExt;
use temporalio_sdk::{
    SyncWorkflowContext, WorkflowContext, WorkflowContextView, WorkflowResult, WorkflowTermination,
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::entropy::{TemporalClock, TemporalRandomSource};
use crate::sink::BufferEventSink;
use crate::storage::InMemoryStorage;
use crate::streamer::TemporalModelStreamer;
use crate::tools::TemporalToolHandler;
use crate::types::{
    CodexWorkflowInput, CodexWorkflowOutput, ContinueAsNewState, PendingApproval, UserTurnInput,
};

/// Maximum number of model→tool loop iterations per turn.
const MAX_ITERATIONS: u32 = 50;

#[workflow]
pub struct CodexWorkflow {
    input: CodexWorkflowInput,
    pub(crate) events: Arc<BufferEventSink>,
    /// Queue of user turns waiting to be processed.
    user_turns: Vec<UserTurnInput>,
    /// Counter for generating turn IDs.
    turn_counter: u32,
    /// Pending tool-call approval (set by tool handler, resolved by signal).
    pub(crate) pending_approval: Option<PendingApproval>,
    /// When true the workflow will exit after the current turn completes.
    shutdown_requested: bool,
    /// When true the workflow will run compaction and then continue-as-new.
    compact_requested: bool,
}

/// Extract the text message from user input items.
fn extract_message(items: &[UserInput]) -> String {
    items
        .iter()
        .filter_map(|item| match item {
            UserInput::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build `ContinueAsNewState` from the current workflow state and return
/// `Err(WorkflowTermination::ContinueAsNew(...))` to trigger CAN.
fn do_continue_as_new(
    input: &CodexWorkflowInput,
    storage: &InMemoryStorage,
    pending_turns: Vec<UserTurnInput>,
    total_iterations: u32,
    turn_counter: u32,
    token_usage: Option<codex_protocol::protocol::TokenUsage>,
) -> WorkflowResult<CodexWorkflowOutput> {
    let state = ContinueAsNewState {
        rollout_items: storage.items(),
        pending_user_turns: pending_turns,
        cumulative_turn_count: turn_counter,
        cumulative_iterations: total_iterations,
        cumulative_token_usage: token_usage,
    };

    let mut can_input = input.clone();
    can_input.user_message = String::new(); // Not needed after first run.
    can_input.continued_state = Some(state);

    Err(WorkflowTermination::continue_as_new(
        ContinueAsNewWorkflowExecution {
            arguments: vec![can_input.as_json_payload().map_err(|e| {
                WorkflowTermination::failed(anyhow::anyhow!("failed to serialize CAN input: {e}"))
            })?],
            ..Default::default()
        },
    ))
}

#[workflow_methods]
impl CodexWorkflow {
    #[init]
    pub fn new(_ctx: &WorkflowContextView, input: CodexWorkflowInput) -> Self {
        // If restoring from continue-as-new, use the carried-over state.
        if let Some(ref state) = input.continued_state {
            return Self {
                user_turns: state.pending_user_turns.clone(),
                turn_counter: state.cumulative_turn_count,
                events: Arc::new(BufferEventSink::new()),
                pending_approval: None,
                shutdown_requested: false,
                compact_requested: false,
                input,
            };
        }

        // If the workflow input contains a non-empty user_message, seed it as
        // the first turn so the workflow starts processing immediately.
        let (initial_turns, turn_counter) = if input.user_message.is_empty() {
            (Vec::new(), 0)
        } else {
            (
                vec![UserTurnInput {
                    turn_id: "turn-0".to_string(),
                    message: input.user_message.clone(),
                    effort: input.reasoning_effort,
                    summary: input.reasoning_summary,
                    personality: input.personality,
                }],
                1,
            )
        };

        Self {
            input,
            events: Arc::new(BufferEventSink::new()),
            user_turns: initial_turns,
            turn_counter,
            pending_approval: None,
            shutdown_requested: false,
            compact_requested: false,
        }
    }

    // ----- signals -----

    /// Generic signal that dispatches all client operations.
    #[signal]
    pub fn receive_op(&mut self, _ctx: &mut SyncWorkflowContext<Self>, op: Op) {
        match op {
            Op::UserTurn {
                items,
                effort,
                summary,
                personality,
                ..
            } => {
                let message = extract_message(&items);
                let turn_id = format!("turn-{}", self.turn_counter);
                self.turn_counter += 1;
                self.user_turns.push(UserTurnInput {
                    turn_id,
                    message,
                    effort,
                    summary,
                    personality,
                });
            }
            Op::ExecApproval { id, decision, .. } => {
                if let Some(ref mut pa) = self.pending_approval {
                    if pa.call_id == id {
                        let approved = matches!(
                            decision,
                            ReviewDecision::Approved
                                | ReviewDecision::ApprovedForSession
                                | ReviewDecision::ApprovedExecpolicyAmendment { .. }
                        );
                        pa.decision = Some(approved);
                    }
                }
            }
            Op::Shutdown => {
                self.shutdown_requested = true;
            }
            Op::Compact => {
                self.compact_requested = true;
            }
            _ => {
                // Unknown/unhandled Op — ignore.
            }
        }
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
        config.model_reasoning_effort = input.reasoning_effort;
        config.model_reasoning_summary = input.reasoning_summary;
        config.personality = input.personality;
        let config = Arc::new(config);

        // --- model info ---
        let model_slug = ModelsManager::get_model_offline_for_tests(config.model.as_deref());
        let model_info =
            ModelsManager::construct_model_info_offline_for_tests(&model_slug, &config);

        // --- session ---
        let conversation_id = ThreadId::new();
        let event_sink: Arc<dyn EventSink> = events.clone();
        let storage = Arc::new(InMemoryStorage::new());
        let storage_backend: Arc<dyn StorageBackend> = Arc::clone(&storage) as _;

        let sess = Session::new_minimal(
            conversation_id,
            Arc::clone(&config),
            event_sink,
            storage_backend,
        )
        .await;

        // --- restore state from continue-as-new (if any) ---
        if let Some(ref state) = input.continued_state {
            // Pre-populate storage so it reflects the full rollout history.
            storage.save(&state.rollout_items).await;

            // Rebuild in-memory conversation history from rollout items.
            let mut history_items: Vec<ResponseItem> = Vec::new();
            for item in &state.rollout_items {
                match item {
                    codex_protocol::protocol::RolloutItem::ResponseItem(ri) => {
                        history_items.push(ri.clone());
                    }
                    codex_protocol::protocol::RolloutItem::Compacted(compacted) => {
                        if let Some(ref replacement) = compacted.replacement_history {
                            // Remote compaction: use the pre-computed replacement.
                            history_items = replacement.clone();
                        } else {
                            // Local compaction: the summary becomes an assistant
                            // message. Clear prior history and use the compacted
                            // message as the new baseline.
                            history_items.clear();
                            history_items.push(compacted.clone().into());
                        }
                    }
                    _ => {} // Skip SessionMeta, TurnContext, EventMsg
                }
            }
            sess.replace_history(history_items).await;
        }

        // --- tools ---
        // Use codex-core's build_specs to get the full set of tool specs
        // (shell, apply_patch, read_file, list_dir, grep_files, etc.).
        let tools_config = ToolsConfig::new(&ToolsConfigParams {
            model_info: &model_info,
            features: &config.features,
            web_search_mode: input.web_search_mode,
        });
        let builder = build_specs(&tools_config, None, None, &[]);
        let (configured_specs, _registry) = builder.build();
        let tools: Vec<ToolSpec> = configured_specs.into_iter().map(|cs| cs.spec).collect();
        let base_instructions = BaseInstructions {
            text: input.instructions.clone(),
        };

        let mut total_iterations = input
            .continued_state
            .as_ref()
            .map_or(0, |s| s.cumulative_iterations);
        let mut last_agent_message: Option<String> = None;

        // === main loop: wait for turns, process them, repeat ===
        let can_result: Option<WorkflowResult<CodexWorkflowOutput>> = ENTROPY
            .scope(entropy, async {
                loop {
                    // Wait until we have a user turn to process or shutdown.
                    ctx.wait_condition(|s| {
                        !s.user_turns.is_empty()
                            || s.shutdown_requested
                            || s.compact_requested
                    })
                    .await;

                    // Check compact — emit event then continue-as-new.
                    let compact = ctx.state(|s| s.compact_requested);
                    if compact {
                        tracing::info!("compact requested — triggering continue-as-new");
                        ctx.state_mut(|s| s.compact_requested = false);

                        // TODO: run actual compaction through Session here.
                        // For now, skip model-based summarization and just
                        // trigger CAN with the full history as-is.

                        // Emit ContextCompactedEvent so clients know compact
                        // completed (blocking event before CAN).
                        events.emit_event_sync(Event {
                            id: String::new(),
                            msg: EventMsg::ContextCompacted(ContextCompactedEvent),
                        });

                        let pending = ctx.state(|s| s.user_turns.clone());
                        let turn_count = ctx.state(|s| s.turn_counter);
                        break Some(do_continue_as_new(
                            &input,
                            &storage,
                            pending,
                            total_iterations,
                            turn_count,
                            events.latest_token_usage(),
                        ));
                    }

                    // Check shutdown before processing.
                    let shutdown = ctx.state(|s| s.shutdown_requested);
                    if shutdown {
                        let remaining = ctx.state(|s| s.user_turns.is_empty());
                        if remaining {
                            break None;
                        }
                    }

                    // Dequeue the next turn.
                    let turn = ctx.state_mut(|s| s.user_turns.remove(0));

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

                    // Per-turn config: apply overrides from UserTurnInput if
                    // they differ from workflow-level defaults.
                    let turn_config = {
                        let mut c = (*config).clone();
                        if turn.effort.is_some() {
                            c.model_reasoning_effort = turn.effort;
                        }
                        if turn.summary != ReasoningSummary::default() {
                            c.model_reasoning_summary = turn.summary;
                        }
                        if turn.personality.is_some() {
                            c.personality = turn.personality;
                        }
                        Arc::new(c)
                    };

                    let turn_context = Arc::new(TurnContext::new_minimal(
                        turn_id.clone(),
                        model_info.clone(),
                        Arc::clone(&turn_config),
                    ));

                    sess.record_items(&turn_context, &[user_item]).await;

                    // --- run the agentic loop for this turn ---
                    let mut streamer = TemporalModelStreamer::new(ctx.clone(), conversation_id.to_string());
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
                            personality: turn_config.personality,
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

                    // Check if server suggests continue-as-new (history too large).
                    if ctx.continue_as_new_suggested() {
                        tracing::info!("server suggested continue-as-new, triggering CAN");
                        let pending = ctx.state(|s| s.user_turns.clone());
                        let turn_count = ctx.state(|s| s.turn_counter);
                        break Some(do_continue_as_new(
                            &input,
                            &storage,
                            pending,
                            total_iterations,
                            turn_count,
                            events.latest_token_usage(),
                        ));
                    }

                    // Check if shutdown was requested during this turn.
                    let shutdown = ctx.state(|s| s.shutdown_requested);
                    if shutdown {
                        break None;
                    }
                }
            })
            .await;

        // If the loop returned a CAN result, propagate it.
        if let Some(can) = can_result {
            return can;
        }

        // Normal shutdown path.
        events.emit_event_sync(Event {
            id: String::new(),
            msg: EventMsg::ShutdownComplete,
        });

        Ok(CodexWorkflowOutput {
            last_agent_message,
            iterations: total_iterations,
            token_usage: events.latest_token_usage(),
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
