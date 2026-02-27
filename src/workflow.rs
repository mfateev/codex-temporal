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

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use codex_core::entropy::{EntropyProviders, ENTROPY};
use codex_core::models_manager::manager::ModelsManager;
use codex_core::{
    EventSink, Prompt, Session, StorageBackend, ToolSpec,
    TurnContext, TurnDiffTracker, ToolsConfig, ToolsConfigParams, build_specs,
    try_run_sampling_request,
};
use codex_protocol::config_types::{Personality, ReasoningSummary};
use codex_protocol::models::{BaseInstructions, ContentItem, ResponseItem};
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::{
    AskForApproval, ContextCompactedEvent, Event, EventMsg, Op, ReviewDecision,
    TurnAbortReason, TurnAbortedEvent, TurnCompleteEvent, TurnStartedEvent,
};
use codex_protocol::user_input::UserInput;
use codex_protocol::ThreadId;
use temporalio_macros::{workflow, workflow_methods};
use temporalio_common::protos::coresdk::workflow_commands::ContinueAsNewWorkflowExecution;
use temporalio_common::protos::coresdk::AsJsonPayloadExt;
use temporalio_sdk::{
    ActivityOptions, SyncWorkflowContext, WorkflowContext, WorkflowContextView, WorkflowResult,
    WorkflowTermination,
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::config_loader::config_from_toml;
use crate::entropy::TemporalRandomSource;
use crate::sink::BufferEventSink;
use crate::storage::InMemoryStorage;
use crate::streamer::TemporalModelStreamer;
use crate::tools::TemporalToolHandler;
use crate::activities::CodexActivities;
use crate::types::{
    AgentWorkflowInput, AgentWorkflowOutput, ConfigOutput, ContinueAsNewState, McpDiscoverInput,
    McpDiscoverOutput, PendingApproval, PendingDynamicTool, PendingElicitation,
    PendingPatchApproval, PendingUserInput, ProjectContextOutput, UserTurnInput,
};

/// Maximum number of model→tool loop iterations per turn.
const MAX_ITERATIONS: u32 = 50;

#[workflow]
pub struct AgentWorkflow {
    input: AgentWorkflowInput,
    pub(crate) events: Arc<BufferEventSink>,
    /// Queue of user turns waiting to be processed.
    user_turns: Vec<UserTurnInput>,
    /// Counter for generating turn IDs.
    turn_counter: u32,
    /// Pending tool-call approval (set by tool handler, resolved by signal).
    pub(crate) pending_approval: Option<PendingApproval>,
    /// Pending `request_user_input` tool call (set by tool handler, resolved
    /// by `Op::UserInputAnswer` signal).
    pub(crate) pending_user_input: Option<PendingUserInput>,
    /// Pending `apply_patch` approval (set by tool handler, resolved by
    /// `Op::PatchApproval` signal).
    pub(crate) pending_patch_approval: Option<PendingPatchApproval>,
    /// Pending MCP elicitation (set by tool handler, resolved by
    /// `Op::ResolveElicitation` signal).
    pub(crate) pending_elicitation: Option<PendingElicitation>,
    /// Pending dynamic tool call (set by tool handler, resolved by
    /// `Op::DynamicToolResponse` signal).
    pub(crate) pending_dynamic_tool: Option<PendingDynamicTool>,
    /// When true the workflow will exit after the current turn completes.
    shutdown_requested: bool,
    /// When true the workflow will run compaction and then continue-as-new.
    compact_requested: bool,
    /// Overridden approval policy (from `Op::OverrideTurnContext`).
    /// `None` means use `input.approval_policy`.
    approval_policy_override: Option<AskForApproval>,
    /// Overridden model slug (from `Op::OverrideTurnContext`).
    model_override: Option<String>,
    /// Overridden reasoning effort (from `Op::OverrideTurnContext`).
    /// `Some(Some(e))` = set effort, `Some(None)` = clear effort, `None` = unchanged.
    effort_override: Option<Option<ReasoningEffort>>,
    /// Overridden reasoning summary (from `Op::OverrideTurnContext`).
    summary_override: Option<ReasoningSummary>,
    /// Overridden personality (from `Op::OverrideTurnContext`).
    personality_override: Option<Personality>,
    /// Set by `Op::Interrupt`; checked between loop iterations and during
    /// approval waits.
    pub(crate) interrupt_requested: bool,
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

/// Build ephemeral context items from project context, matching codex-core's
/// `UserInstructions` and `EnvironmentContext` patterns.
///
/// These items are prepended to the prompt each iteration (not stored in
/// session history), giving the model awareness of the project's AGENTS.md
/// docs and working directory.
pub fn build_context_items(ctx: &ProjectContextOutput) -> Vec<ResponseItem> {
    let mut items = Vec::new();

    // User instructions (AGENTS.md) — matches codex-core's UserInstructions format.
    if let Some(ref instructions) = ctx.user_instructions {
        items.push(ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: format!(
                    "# AGENTS.md instructions for {cwd}\n\n<INSTRUCTIONS>\n{contents}\n</INSTRUCTIONS>",
                    cwd = ctx.cwd,
                    contents = instructions,
                ),
            }],
            end_turn: None,
            phase: None,
        });
    }

    // Environment context — matches codex-core's EnvironmentContext XML format.
    let mut env_lines = vec!["<environment_context>".to_string()];
    env_lines.push(format!("  <cwd>{}</cwd>", ctx.cwd));
    env_lines.push("  <shell>bash</shell>".to_string());
    if let Some(ref git) = ctx.git_info {
        env_lines.push("  <git>".to_string());
        if let Some(ref branch) = git.branch {
            env_lines.push(format!("    <branch>{branch}</branch>"));
        }
        if let Some(ref commit) = git.commit_hash {
            env_lines.push(format!("    <commit>{commit}</commit>"));
        }
        if let Some(ref url) = git.repository_url {
            env_lines.push(format!("    <repository_url>{url}</repository_url>"));
        }
        env_lines.push("  </git>".to_string());
    }
    env_lines.push("</environment_context>".to_string());

    items.push(ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: env_lines.join("\n"),
        }],
        end_turn: None,
        phase: None,
    });

    items
}

/// Build `ContinueAsNewState` from the current workflow state and return
/// `Err(WorkflowTermination::ContinueAsNew(...))` to trigger CAN.
fn do_continue_as_new(
    input: &AgentWorkflowInput,
    storage: &InMemoryStorage,
    pending_turns: Vec<UserTurnInput>,
    total_iterations: u32,
    turn_counter: u32,
    token_usage: Option<codex_protocol::protocol::TokenUsage>,
    mcp_tools: HashMap<String, serde_json::Value>,
    approval_policy_override: Option<AskForApproval>,
    model_override: Option<String>,
    effort_override: Option<Option<ReasoningEffort>>,
    summary_override: Option<ReasoningSummary>,
    personality_override: Option<Personality>,
) -> WorkflowResult<AgentWorkflowOutput> {
    let state = ContinueAsNewState {
        rollout_items: storage.items(),
        pending_user_turns: pending_turns,
        cumulative_turn_count: turn_counter,
        cumulative_iterations: total_iterations,
        cumulative_token_usage: token_usage,
        mcp_tools,
        approval_policy_override,
        model_override,
        effort_override,
        summary_override,
        personality_override,
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
impl AgentWorkflow {
    #[init]
    pub fn new(_ctx: &WorkflowContextView, input: AgentWorkflowInput) -> Self {
        // If restoring from continue-as-new, use the carried-over state.
        if let Some(ref state) = input.continued_state {
            return Self {
                user_turns: state.pending_user_turns.clone(),
                turn_counter: state.cumulative_turn_count,
                events: Arc::new(BufferEventSink::new()),
                pending_approval: None,
                pending_user_input: None,
                pending_patch_approval: None,
                pending_elicitation: None,
                pending_dynamic_tool: None,
                shutdown_requested: false,
                compact_requested: false,
                approval_policy_override: state.approval_policy_override,
                model_override: state.model_override.clone(),
                effort_override: state.effort_override,
                summary_override: state.summary_override,
                personality_override: state.personality_override,
                interrupt_requested: false,
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
            pending_user_input: None,
            pending_patch_approval: None,
            pending_elicitation: None,
            pending_dynamic_tool: None,
            shutdown_requested: false,
            compact_requested: false,
            approval_policy_override: None,
            model_override: None,
            effort_override: None,
            summary_override: None,
            personality_override: None,
            interrupt_requested: false,
        }
    }

    /// Return the effective approval policy, preferring the override.
    pub fn effective_approval_policy(&self) -> AskForApproval {
        self.approval_policy_override
            .unwrap_or(self.input.approval_policy)
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
            Op::UserInputAnswer { id, response, .. } => {
                if let Some(ref mut pui) = self.pending_user_input {
                    if pui.call_id == id {
                        pui.response = Some(response);
                    }
                }
            }
            Op::Shutdown => {
                self.shutdown_requested = true;
            }
            Op::Compact => {
                self.compact_requested = true;
            }
            Op::PatchApproval { id, decision, .. } => {
                if let Some(ref mut pa) = self.pending_patch_approval {
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
            Op::OverrideTurnContext {
                approval_policy,
                model,
                effort,
                summary,
                personality,
                ..
            } => {
                if let Some(policy) = approval_policy {
                    self.approval_policy_override = Some(policy);
                }
                if let Some(m) = model {
                    self.model_override = Some(m);
                }
                if let Some(e) = effort {
                    self.effort_override = Some(e);
                }
                if let Some(s) = summary {
                    self.summary_override = Some(s);
                }
                if let Some(p) = personality {
                    self.personality_override = Some(p);
                }
            }
            Op::DynamicToolResponse { id, response } => {
                if let Some(ref mut pdt) = self.pending_dynamic_tool {
                    if pdt.call_id == id {
                        pdt.response = Some(response);
                    }
                }
            }
            Op::ResolveElicitation {
                server_name,
                request_id,
                decision,
            } => {
                if let Some(ref mut pe) = self.pending_elicitation {
                    if pe.server_name == server_name && pe.request_id == request_id {
                        pe.response = Some(decision);
                    }
                }
            }
            Op::Interrupt => {
                self.interrupt_requested = true;
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
    pub async fn run(ctx: &mut WorkflowContext<Self>) -> WorkflowResult<AgentWorkflowOutput> {
        let input = ctx.state(|s| s.input.clone());
        let events = ctx.state(|s| s.events.clone());

        // --- deterministic entropy ---
        let seed = ctx.random_seed();
        let entropy = EntropyProviders {
            random: Arc::new(TemporalRandomSource::new(seed)),
        };

        // --- startup: load config, project context, MCP tools ---
        // If pre-resolved by SessionWorkflow, use directly; otherwise fall
        // back to activities (backward compat for standalone tests).
        let (config_output, project_context, mcp_tools) =
            if input.config_toml.is_some() && input.project_context.is_some() {
                let config_output = ConfigOutput {
                    config_toml: input.config_toml.clone().unwrap(),
                };
                let project_context = input.project_context.clone().unwrap();
                let mcp_tools = input.mcp_tools.clone();
                tracing::info!("using pre-resolved config/context from parent workflow");
                (config_output, project_context, mcp_tools)
            } else {
                // Launch load_config and collect_project_context concurrently.
                let config_activity = ctx.start_activity(
                    CodexActivities::load_config,
                    (),
                    ActivityOptions {
                        schedule_to_close_timeout: Some(std::time::Duration::from_secs(30)),
                        ..Default::default()
                    },
                );
                let project_context_activity = ctx.start_activity(
                    CodexActivities::collect_project_context,
                    (),
                    ActivityOptions {
                        schedule_to_close_timeout: Some(std::time::Duration::from_secs(30)),
                        ..Default::default()
                    },
                );

                let (config_result, project_context_result) =
                    futures::future::join(config_activity, project_context_activity).await;

                let config_output: ConfigOutput = config_result
                    .map_err(|e| anyhow::anyhow!("load_config failed: {e}"))?;
                let project_context: ProjectContextOutput = project_context_result
                    .map_err(|e| anyhow::anyhow!("collect_project_context failed: {e}"))?;

                // MCP discovery: reuse from CAN state or run activity.
                let mcp_tools: HashMap<String, serde_json::Value> =
                    if let Some(ref state) = input.continued_state {
                        if !state.mcp_tools.is_empty() {
                            tracing::info!(
                                tools = state.mcp_tools.len(),
                                "reusing MCP tools from continue-as-new state"
                            );
                        }
                        state.mcp_tools.clone()
                    } else {
                        let mcp_discover_input = McpDiscoverInput {
                            config_toml: config_output.config_toml.clone(),
                            cwd: project_context.cwd.clone(),
                        };
                        let mcp_output: McpDiscoverOutput = ctx
                            .start_activity(
                                CodexActivities::discover_mcp_tools,
                                mcp_discover_input,
                                ActivityOptions {
                                    schedule_to_close_timeout: Some(
                                        std::time::Duration::from_secs(60),
                                    ),
                                    ..Default::default()
                                },
                            )
                            .await
                            .unwrap_or_else(|e| {
                                tracing::warn!("MCP discovery failed: {e}");
                                McpDiscoverOutput {
                                    tools: HashMap::new(),
                                }
                            });
                        mcp_output.tools
                    };

                (config_output, project_context, mcp_tools)
            };

        let context_items = build_context_items(&project_context);

        if !mcp_tools.is_empty() {
            tracing::info!(tools = mcp_tools.len(), "MCP tools available");
        }

        // --- config ---
        // Build a full Config from the activity-loaded TOML, with user
        // instructions (AGENTS.md) from project context.
        let mut config = config_from_toml(
            &config_output.config_toml,
            std::path::Path::new(&project_context.cwd),
            project_context.user_instructions.clone(),
        )
        .map_err(|e| anyhow::anyhow!("failed to build config from TOML: {e}"))?;
        config.model = Some(input.model.clone());
        config.model_reasoning_effort = input.reasoning_effort;
        config.model_reasoning_summary = Some(input.reasoning_summary);
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
            sess.replace_history(history_items, None).await;
        }

        // --- tools ---
        // Use codex-core's build_specs to get the full set of tool specs
        // (shell, apply_patch, read_file, list_dir, grep_files, etc.).
        let tools_config = ToolsConfig::new(&ToolsConfigParams {
            model_info: &model_info,
            features: &config.features,
            web_search_mode: input.web_search_mode,
            session_source: codex_protocol::protocol::SessionSource::Exec,
        });

        // Convert serialized MCP tools back to rmcp::model::Tool for build_specs.
        let rmcp_tools: Option<HashMap<String, rmcp::model::Tool>> = if mcp_tools.is_empty() {
            None
        } else {
            let mut map = HashMap::new();
            for (name, value) in &mcp_tools {
                match serde_json::from_value::<rmcp::model::Tool>(value.clone()) {
                    Ok(tool) => {
                        map.insert(name.clone(), tool);
                    }
                    Err(e) => {
                        tracing::warn!(tool = %name, error = %e, "failed to deserialize MCP tool, skipping");
                    }
                }
            }
            if map.is_empty() { None } else { Some(map) }
        };

        // Collect MCP tool names for the tool handler.
        let mcp_tool_names: HashSet<String> = rmcp_tools
            .as_ref()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();

        // Collect dynamic tool names for the tool handler to intercept.
        let dynamic_tool_names: HashSet<String> = input
            .dynamic_tools
            .iter()
            .map(|dt| dt.name.clone())
            .collect();

        let builder = build_specs(&tools_config, rmcp_tools, None, &input.dynamic_tools);
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
        let can_result: Option<WorkflowResult<AgentWorkflowOutput>> = ENTROPY
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
                        let (
                            policy_override,
                            can_model_override,
                            can_effort_override,
                            can_summary_override,
                            can_personality_override,
                        ) = ctx.state(|s| {
                            (
                                s.approval_policy_override,
                                s.model_override.clone(),
                                s.effort_override,
                                s.summary_override,
                                s.personality_override,
                            )
                        });
                        break Some(do_continue_as_new(
                            &input,
                            &storage,
                            pending,
                            total_iterations,
                            turn_count,
                            events.latest_token_usage(),
                            mcp_tools.clone(),
                            policy_override,
                            can_model_override,
                            can_effort_override,
                            can_summary_override,
                            can_personality_override,
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

                    // Read persistent overrides from workflow state.
                    let (
                        persistent_effort,
                        persistent_summary,
                        persistent_personality,
                        persistent_model,
                    ) = ctx.state(|s| {
                        (
                            s.effort_override,
                            s.summary_override,
                            s.personality_override,
                            s.model_override.clone(),
                        )
                    });

                    // Per-turn config: per-turn overrides from UserTurnInput take
                    // priority; persistent overrides (from OverrideTurnContext) are
                    // used as fallback.
                    let turn_config = {
                        let mut c = (*config).clone();
                        // Per-turn overrides take priority.
                        if turn.effort.is_some() {
                            c.model_reasoning_effort = turn.effort;
                        } else if let Some(e) = persistent_effort {
                            c.model_reasoning_effort = e;
                        }
                        if turn.summary != ReasoningSummary::default() {
                            c.model_reasoning_summary = Some(turn.summary);
                        } else if let Some(s) = persistent_summary {
                            c.model_reasoning_summary = Some(s);
                        }
                        if turn.personality.is_some() {
                            c.personality = turn.personality;
                        } else if let Some(p) = persistent_personality {
                            c.personality = Some(p);
                        }
                        // Model override: update config model field.
                        if let Some(ref m) = persistent_model {
                            c.model = Some(m.clone());
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
                    let mut streamer = TemporalModelStreamer::new(
                        ctx.clone(),
                        conversation_id.to_string(),
                        input.model_provider.clone(),
                    );

                    let diff_tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));
                    let cancellation_token = CancellationToken::new();
                    let mut iterations = 0u32;
                    let mut turn_aborted = false;

                    // Reset interrupt flag at the start of each turn.
                    ctx.state_mut(|s| s.interrupt_requested = false);

                    loop {
                        // Check for interrupt at iteration boundary.
                        let interrupted = ctx.state(|s| s.interrupt_requested);
                        if interrupted {
                            tracing::info!("interrupt requested, aborting turn");
                            turn_aborted = true;
                            ctx.state_mut(|s| s.interrupt_requested = false);
                            break;
                        }

                        if iterations >= MAX_ITERATIONS {
                            tracing::warn!("max iterations reached ({MAX_ITERATIONS}), stopping turn");
                            break;
                        }

                        // Create handler inside the loop so it picks up the
                        // effective approval policy (which may change mid-turn
                        // via OverrideTurnContext signals).
                        let effective_policy =
                            ctx.state(|s| s.effective_approval_policy());
                        let effective_model = persistent_model
                            .as_deref()
                            .unwrap_or(&input.model)
                            .to_string();
                        let handler = TemporalToolHandler::new(
                            ctx.clone(),
                            events.clone(),
                            turn_id.clone(),
                            effective_policy,
                            effective_model,
                            config.cwd.to_string_lossy().to_string(),
                            Some(config_output.config_toml.clone()),
                            mcp_tool_names.clone(),
                            dynamic_tool_names.clone(),
                        );

                        // Rebuild prompt from accumulated session history,
                        // prepending ephemeral context items (AGENTS.md +
                        // environment context) so the model sees project docs.
                        let history = sess.history_items().await;
                        let mut input_items = context_items.clone();

                        // Inject developer instructions (from config.toml)
                        // after project context, before conversation history.
                        if let Some(ref dev_instructions) = input.developer_instructions {
                            input_items.push(ResponseItem::Message {
                                id: None,
                                role: "developer".to_string(),
                                content: vec![ContentItem::InputText {
                                    text: dev_instructions.clone(),
                                }],
                                end_turn: None,
                                phase: None,
                            });
                        }

                        input_items.extend(history);
                        let prompt = Prompt {
                            input: input_items,
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

                    if turn_aborted {
                        // Emit TurnAborted event.
                        events.emit_event_sync(Event {
                            id: turn_id.clone(),
                            msg: EventMsg::TurnAborted(TurnAbortedEvent {
                                turn_id: Some(turn_id),
                                reason: TurnAbortReason::Interrupted,
                            }),
                        });
                    } else {
                        // Emit TurnComplete
                        events.emit_event_sync(Event {
                            id: turn_id.clone(),
                            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                                turn_id,
                                last_agent_message: last_agent_message.clone(),
                            }),
                        });
                    }

                    // Check if server suggests continue-as-new (history too large).
                    if ctx.continue_as_new_suggested() {
                        tracing::info!("server suggested continue-as-new, triggering CAN");
                        let pending = ctx.state(|s| s.user_turns.clone());
                        let turn_count = ctx.state(|s| s.turn_counter);
                        let (
                            policy_override,
                            can_model_override,
                            can_effort_override,
                            can_summary_override,
                            can_personality_override,
                        ) = ctx.state(|s| {
                            (
                                s.approval_policy_override,
                                s.model_override.clone(),
                                s.effort_override,
                                s.summary_override,
                                s.personality_override,
                            )
                        });
                        break Some(do_continue_as_new(
                            &input,
                            &storage,
                            pending,
                            total_iterations,
                            turn_count,
                            events.latest_token_usage(),
                            mcp_tools.clone(),
                            policy_override,
                            can_model_override,
                            can_effort_override,
                            can_summary_override,
                            can_personality_override,
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

        Ok(AgentWorkflowOutput {
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
/// `session.rs`) can parameterize `WorkflowHandle<Client, AgentWorkflowRun>`.
pub use agent_workflow::Run as AgentWorkflowRun;

/// Backward-compatible aliases.
pub type CodexWorkflow = AgentWorkflow;
pub type CodexWorkflowRun = AgentWorkflowRun;
