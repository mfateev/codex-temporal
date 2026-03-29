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
//! - **`get_state_update` update**: Blocking handler that returns new events
//!   when the event watermark advances.  Replaces the old query-based polling.
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
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::models::{BaseInstructions, ContentItem, ResponseItem};
use codex_protocol::protocol::{
    AgentMessageEvent, AskForApproval, ContextCompactedEvent, Event, EventMsg, Op,
    ReviewDecision, TurnAbortReason, TurnAbortedEvent, TurnCompleteEvent, TurnStartedEvent,
};
use codex_protocol::ThreadId;
use temporalio_macros::{workflow, workflow_methods};
use temporalio_common::protos::coresdk::workflow_commands::ContinueAsNewWorkflowExecution;
use temporalio_common::protos::coresdk::AsJsonPayloadExt;
use temporalio_sdk::{
    SyncWorkflowContext, WorkflowContext, WorkflowContextView, WorkflowResult,
    WorkflowTermination,
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::config_loader::config_from_toml;
use crate::entropy::TemporalRandomSource;
use crate::sink::{BufferEventSink, DEFAULT_EVENT_BUFFER_CAPACITY};
use crate::storage::InMemoryStorage;
use crate::streamer::TemporalModelStreamer;
use crate::tools::TemporalToolHandler;
use crate::activities::{CodexActivities, activity_opts};
use crate::types::{
    AgentWorkflowInput, AgentWorkflowOutput, ConfigOutput, ContinueAsNewState, PendingApproval,
    PendingDynamicTool, PendingElicitation,
    PendingPatchApproval, PendingUserInput, ProjectContextOutput, ResolveModelInfoInput,
    StateUpdateRequest, StateUpdateResponse, TurnOverrides, UserTurnInput, extract_message,
};

/// Default maximum number of model→tool loop iterations per turn.
const DEFAULT_MAX_ITERATIONS: u32 = 50;

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
    /// Persistent turn-context overrides (from `Op::OverrideTurnContext`).
    overrides: TurnOverrides,
    /// Set by `Op::Interrupt`; checked between loop iterations and during
    /// approval waits.
    pub(crate) interrupt_requested: bool,
    /// Cancellation token for the current in-flight turn.  Cancelled by
    /// `Op::Interrupt` to abort in-flight activities immediately.
    current_turn_cancellation: Option<CancellationToken>,
    /// Monotonically increasing counter bumped on every mutation visible to
    /// external observers.
    state_version: u64,
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
            env_lines.push(format!("    <commit>{}</commit>", commit.0));
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
    events: &BufferEventSink,
    pending_turns: Vec<UserTurnInput>,
    total_iterations: u32,
    turn_counter: u32,
    token_usage: Option<codex_protocol::protocol::TokenUsage>,
    mcp_tools: HashMap<String, serde_json::Value>,
    overrides: TurnOverrides,
) -> WorkflowResult<AgentWorkflowOutput> {
    let (event_offset, event_snapshot) = events.snapshot();
    let state = ContinueAsNewState {
        rollout_items: storage.items(),
        pending_user_turns: pending_turns,
        cumulative_turn_count: turn_counter,
        cumulative_iterations: total_iterations,
        cumulative_token_usage: token_usage,
        mcp_tools,
        overrides,
        event_offset,
        event_snapshot,
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
                events: Arc::new(BufferEventSink::from_snapshot(
                    state.event_offset,
                    state.event_snapshot.clone(),
                    DEFAULT_EVENT_BUFFER_CAPACITY,
                )),
                pending_approval: None,
                pending_user_input: None,
                pending_patch_approval: None,
                pending_elicitation: None,
                pending_dynamic_tool: None,
                shutdown_requested: false,
                compact_requested: false,
                overrides: state.overrides.clone(),
                interrupt_requested: false,
                current_turn_cancellation: None,
                state_version: 0,
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
            events: Arc::new(BufferEventSink::new(DEFAULT_EVENT_BUFFER_CAPACITY, 0)),
            user_turns: initial_turns,
            turn_counter,
            pending_approval: None,
            pending_user_input: None,
            pending_patch_approval: None,
            pending_elicitation: None,
            pending_dynamic_tool: None,
            shutdown_requested: false,
            compact_requested: false,
            overrides: TurnOverrides::default(),
            interrupt_requested: false,
            current_turn_cancellation: None,
            state_version: 0,
        }
    }

    /// Return the effective approval policy, preferring the override.
    pub fn effective_approval_policy(&self) -> AskForApproval {
        self.overrides
            .approval_policy
            .unwrap_or(self.input.approval_policy)
    }

    /// Bump the monotonic state version counter.
    pub(crate) fn bump_version(&mut self) {
        self.state_version += 1;
    }

    /// Emit an event and bump the state version in one call.
    pub(crate) fn emit_and_bump(
        ctx: &WorkflowContext<Self>,
        events: &BufferEventSink,
        event: Event,
    ) {
        events.emit_event_sync(event);
        ctx.state_mut(|s| s.bump_version());
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
                    summary: summary.unwrap_or_default(),
                    personality,
                });
                self.bump_version();
            }
            Op::ExecApproval { id, decision, .. } => {
                if let Some(ref mut pa) = self.pending_approval
                    && pa.call_id == id
                {
                    let approved = matches!(
                        decision,
                        ReviewDecision::Approved
                            | ReviewDecision::ApprovedForSession
                            | ReviewDecision::ApprovedExecpolicyAmendment { .. }
                    );
                    pa.decision = Some(approved);
                    self.bump_version();
                }
            }
            Op::UserInputAnswer { id, response, .. } => {
                if let Some(ref mut pui) = self.pending_user_input
                    && (pui.turn_id == id || pui.call_id == id)
                {
                    pui.response = Some(response);
                    self.bump_version();
                }
            }
            Op::Shutdown => {
                self.shutdown_requested = true;
                self.bump_version();
            }
            Op::Compact => {
                self.compact_requested = true;
                self.bump_version();
            }
            Op::PatchApproval { id, decision, .. } => {
                if let Some(ref mut pa) = self.pending_patch_approval
                    && pa.call_id == id
                {
                    let approved = matches!(
                        decision,
                        ReviewDecision::Approved
                            | ReviewDecision::ApprovedForSession
                            | ReviewDecision::ApprovedExecpolicyAmendment { .. }
                    );
                    pa.decision = Some(approved);
                    self.bump_version();
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
                    self.overrides.approval_policy = Some(policy);
                }
                if let Some(m) = model {
                    self.overrides.model = Some(m);
                }
                if let Some(e) = effort {
                    self.overrides.effort = Some(e);
                }
                if let Some(s) = summary {
                    self.overrides.summary = Some(s);
                }
                if let Some(p) = personality {
                    self.overrides.personality = Some(p);
                }
                self.bump_version();
            }
            Op::DynamicToolResponse { id, response } => {
                if let Some(ref mut pdt) = self.pending_dynamic_tool
                    && pdt.call_id == id
                {
                    pdt.response = Some(response);
                    self.bump_version();
                }
            }
            Op::ResolveElicitation {
                server_name,
                request_id,
                decision,
                ..
            } => {
                if let Some(ref mut pe) = self.pending_elicitation
                    && pe.server_name == server_name && pe.request_id == request_id
                {
                    pe.response = Some(decision);
                    self.bump_version();
                }
            }
            Op::Interrupt => {
                self.interrupt_requested = true;
                if let Some(ref token) = self.current_turn_cancellation {
                    token.cancel();
                }
                self.bump_version();
            }
            _ => {
                // Unknown/unhandled Op — ignore.
            }
        }
    }

    // ----- updates -----

    /// Blocking update handler: returns new events when the workflow state
    /// changes.  Replaces the old `get_events_since` query + client-side
    /// polling loop.
    #[update]
    pub async fn get_state_update(
        ctx: &mut WorkflowContext<Self>,
        req: StateUpdateRequest,
    ) -> Result<StateUpdateResponse, Box<dyn std::error::Error + Send + Sync>> {
        // Check if data is immediately available.
        let (events, watermark) = ctx.state(|s| s.events.events_since(req.since_index));
        let shutdown = ctx.state(|s| s.shutdown_requested);
        if !events.is_empty() || shutdown {
            return Ok(StateUpdateResponse {
                events,
                watermark,
                completed: shutdown,
            });
        }

        // Block until new events arrive (watermark advances) or shutdown.
        let entry_watermark = watermark;
        ctx.wait_condition(|s| {
            s.events.watermark() != entry_watermark || s.shutdown_requested
        })
        .await;

        // Re-read after wakeup.
        let (events, watermark) = ctx.state(|s| s.events.events_since(req.since_index));
        let shutdown = ctx.state(|s| s.shutdown_requested);
        Ok(StateUpdateResponse {
            events,
            watermark,
            completed: shutdown,
        })
    }

    // ----- run -----

    #[run]
    pub async fn run(ctx: &mut WorkflowContext<Self>) -> WorkflowResult<AgentWorkflowOutput> {
        let input = ctx.state(|s| s.input.clone());
        let events = ctx.state(|s| s.events.clone());

        let seed = ctx.random_seed();
        let entropy = EntropyProviders {
            random: Arc::new(TemporalRandomSource::new(seed)),
        };

        let mut rt = WorkflowRuntime::initialize(ctx, &input, &events).await?;

        let can_result: Option<WorkflowResult<AgentWorkflowOutput>> = ENTROPY
            .scope(entropy, async {
                loop {
                    ctx.wait_condition(|s| {
                        !s.user_turns.is_empty()
                            || s.shutdown_requested
                            || s.compact_requested
                    })
                    .await;

                    if ctx.state(|s| s.compact_requested) {
                        break Some(rt.handle_compact(ctx));
                    }

                    if ctx.state(|s| s.shutdown_requested && s.user_turns.is_empty()) {
                        break None;
                    }

                    let turn = ctx.state_mut(|s| s.user_turns.remove(0));
                    let overrides = ctx.state(|s| s.overrides.clone());

                    match rt.process_turn(ctx, turn, &overrides).await {
                        TurnOutcome::ContinueAsNew(r) => break Some(r),
                        TurnOutcome::Shutdown => break None,
                        TurnOutcome::Completed => continue,
                    }
                }
            })
            .await;

        if let Some(can) = can_result {
            return can;
        }

        AgentWorkflow::emit_and_bump(ctx, &events, Event {
            id: String::new(),
            msg: EventMsg::ShutdownComplete,
        });

        Ok(AgentWorkflowOutput {
            last_agent_message: rt.last_agent_message,
            iterations: rt.total_iterations,
            token_usage: events.latest_token_usage(),
        })
    }

}

// ---------------------------------------------------------------------------
// WorkflowRuntime — post-initialization state and per-turn processing
// ---------------------------------------------------------------------------

/// Outcome of processing a single user turn.
enum TurnOutcome {
    /// Turn completed normally; continue waiting for the next turn.
    Completed,
    /// Workflow should continue-as-new.
    ContinueAsNew(WorkflowResult<AgentWorkflowOutput>),
    /// Shutdown was requested during this turn.
    Shutdown,
}

/// Holds all state produced during workflow initialization and provides
/// methods for per-turn processing.  Lives outside the `#[workflow_methods]`
/// impl so it is unconstrained by the Temporal macro's signature rules.
struct WorkflowRuntime {
    input: AgentWorkflowInput,
    events: Arc<BufferEventSink>,
    config: Arc<codex_core::config::Config>,
    config_toml: String,
    sess: Arc<Session>,
    storage: Arc<InMemoryStorage>,
    tools: Vec<ToolSpec>,
    base_instructions: BaseInstructions,
    context_items: Vec<ResponseItem>,
    model_info: codex_protocol::openai_models::ModelInfo,
    conversation_id: ThreadId,
    mcp_tools: HashMap<String, serde_json::Value>,
    mcp_tool_names: HashSet<String>,
    dynamic_tool_names: HashSet<String>,
    max_iterations: u32,
    total_iterations: u32,
    last_agent_message: Option<String>,
}

impl WorkflowRuntime {
    /// Run all startup activities (config, project context, MCP discovery,
    /// model info, session creation, CAN state restoration, tool building).
    async fn initialize(
        ctx: &mut WorkflowContext<AgentWorkflow>,
        input: &AgentWorkflowInput,
        events: &Arc<BufferEventSink>,
    ) -> Result<Self, WorkflowTermination> {
        let max_iterations = input.max_iterations.unwrap_or(DEFAULT_MAX_ITERATIONS);

        // --- startup: load config, project context, MCP tools ---
        let (config_output, project_context, mcp_tools) =
            if input.config_toml.is_some() && input.project_context.is_some() {
                let config_output = ConfigOutput {
                    config_toml: input.config_toml.clone().unwrap(),
                };
                let project_context = input.project_context.clone().unwrap();
                let mcp_tools = input.mcp_tools.clone();
                tracing::debug!("using pre-resolved config/context from parent workflow");
                (config_output, project_context, mcp_tools)
            } else {
                let (config_output, project_context) =
                    crate::startup::load_config_and_context!(ctx)?;

                // MCP discovery: reuse from CAN state or run activity.
                let mcp_tools = if let Some(ref state) = input.continued_state {
                    if !state.mcp_tools.is_empty() {
                        tracing::debug!(
                            tools = state.mcp_tools.len(),
                            "reusing MCP tools from continue-as-new state"
                        );
                    }
                    state.mcp_tools.clone()
                } else {
                    crate::startup::discover_mcp!(
                        ctx,
                        config_output.config_toml,
                        project_context.cwd
                    )
                };

                (config_output, project_context, mcp_tools)
            };

        let context_items = build_context_items(&project_context);

        if !mcp_tools.is_empty() {
            tracing::debug!(tools = mcp_tools.len(), "MCP tools available");
        }

        // --- config ---
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
        let mut model_info = ctx
            .start_activity(
                CodexActivities::resolve_model_info,
                ResolveModelInfoInput {
                    model: input.model.clone(),
                    config_toml: config_output.config_toml.clone(),
                },
                activity_opts(30),
            )
            .await
            .unwrap_or_else(|e| {
                tracing::warn!("resolve_model_info failed: {e} — falling back to bundled catalog");
                ModelsManager::resolve_from_bundled_catalog(&input.model, &config)
            });
        crate::activities::backfill_experimental_tools(&mut model_info);

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
            storage.save(&state.rollout_items).await;

            let mut history_items: Vec<ResponseItem> = Vec::new();
            for item in &state.rollout_items {
                match item {
                    codex_protocol::protocol::RolloutItem::ResponseItem(ri) => {
                        history_items.push(ri.clone());
                    }
                    codex_protocol::protocol::RolloutItem::Compacted(compacted) => {
                        if let Some(ref replacement) = compacted.replacement_history {
                            history_items = replacement.clone();
                        } else {
                            history_items.clear();
                            history_items.push(compacted.clone().into());
                        }
                    }
                    _ => {}
                }
            }
            sess.replace_history(history_items, None).await;
        }

        // --- tools ---
        let sandbox_policy = config.permissions.sandbox_policy.get();
        let tools_config = ToolsConfig::new(&ToolsConfigParams {
            model_info: &model_info,
            available_models: &vec![],
            features: &config.features,
            web_search_mode: input.web_search_mode,
            session_source: codex_protocol::protocol::SessionSource::Exec,
            sandbox_policy,
            windows_sandbox_level: codex_protocol::config_types::WindowsSandboxLevel::Disabled,
        })
        .with_agent_roles(config.agent_roles.clone());

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

        let mcp_tool_names: HashSet<String> = rmcp_tools
            .as_ref()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();

        let dynamic_tool_names: HashSet<String> = input
            .dynamic_tools
            .iter()
            .map(|dt| dt.name.clone())
            .collect();

        let builder = build_specs(&tools_config, rmcp_tools, None, &input.dynamic_tools);
        let (configured_specs, _registry) = builder.build();
        let tools: Vec<ToolSpec> = configured_specs.into_iter().map(|cs| cs.spec).collect();

        let total_iterations = input
            .continued_state
            .as_ref()
            .map_or(0, |s| s.cumulative_iterations);

        Ok(Self {
            input: input.clone(),
            events: Arc::clone(events),
            config,
            config_toml: config_output.config_toml,
            sess,
            storage,
            tools,
            base_instructions: BaseInstructions {
                text: input.instructions.clone(),
            },
            context_items,
            model_info,
            conversation_id,
            mcp_tools,
            mcp_tool_names,
            dynamic_tool_names,
            max_iterations,
            total_iterations,
            last_agent_message: None,
        })
    }

    /// Handle a compact request: emit event and trigger continue-as-new.
    fn handle_compact(
        &self,
        ctx: &mut WorkflowContext<AgentWorkflow>,
    ) -> WorkflowResult<AgentWorkflowOutput> {
        tracing::info!("compact requested — triggering continue-as-new");
        ctx.state_mut(|s| s.compact_requested = false);

        AgentWorkflow::emit_and_bump(ctx, &self.events, Event {
            id: String::new(),
            msg: EventMsg::ContextCompacted(ContextCompactedEvent),
        });

        self.trigger_continue_as_new(ctx)
    }

    /// Build `ContinueAsNewState` from the current runtime + workflow state
    /// and return the CAN termination result.
    fn trigger_continue_as_new(
        &self,
        ctx: &WorkflowContext<AgentWorkflow>,
    ) -> WorkflowResult<AgentWorkflowOutput> {
        let pending = ctx.state(|s| s.user_turns.clone());
        let turn_count = ctx.state(|s| s.turn_counter);
        let overrides = ctx.state(|s| s.overrides.clone());

        do_continue_as_new(
            &self.input,
            &self.storage,
            &self.events,
            pending,
            self.total_iterations,
            turn_count,
            self.events.latest_token_usage(),
            self.mcp_tools.clone(),
            overrides,
        )
    }

    /// Process a single user turn: emit events, run the agentic iteration
    /// loop, and return the outcome.
    async fn process_turn(
        &mut self,
        ctx: &mut WorkflowContext<AgentWorkflow>,
        turn: UserTurnInput,
        overrides: &TurnOverrides,
    ) -> TurnOutcome {
        let turn_id = turn.turn_id.clone();

        // Emit TurnStarted.
        AgentWorkflow::emit_and_bump(ctx, &self.events, Event {
            id: turn_id.clone(),
            msg: EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: turn_id.clone(),
                model_context_window: None,
                collaboration_mode_kind: Default::default(),
            }),
        });

        // Record the user message in session history.
        let user_item = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: turn.message.clone(),
            }],
            end_turn: None,
            phase: None,
        };

        let turn_config = self.build_turn_config(&turn, overrides);
        let turn_context = Arc::new(TurnContext::new_minimal(
            turn_id.clone(),
            self.model_info.clone(),
            Arc::clone(&turn_config),
        ));
        self.sess.record_items(&turn_context, &[user_item]).await;

        // Run the agentic iteration loop.
        let (turn_aborted, turn_error) =
            self.run_agentic_loop(ctx, &turn_id, &turn_config, &turn_context, overrides)
                .await;

        // Emit turn-end events.
        self.emit_turn_end_events(ctx, &turn_id, turn_aborted, turn_error.as_ref());

        // Check if server suggests continue-as-new.
        if ctx.continue_as_new_suggested() {
            tracing::info!("server suggested continue-as-new, triggering CAN");
            return TurnOutcome::ContinueAsNew(self.trigger_continue_as_new(ctx));
        }

        if ctx.state(|s| s.shutdown_requested) {
            return TurnOutcome::Shutdown;
        }

        TurnOutcome::Completed
    }

    /// Build a per-turn `Config`, merging per-turn overrides (from
    /// `UserTurnInput`) with persistent overrides (from
    /// `Op::OverrideTurnContext`).  Per-turn values take priority.
    fn build_turn_config(
        &self,
        turn: &UserTurnInput,
        overrides: &TurnOverrides,
    ) -> Arc<codex_core::config::Config> {
        let mut c = (*self.config).clone();

        if turn.effort.is_some() {
            c.model_reasoning_effort = turn.effort;
        } else if let Some(e) = overrides.effort {
            c.model_reasoning_effort = e;
        }

        if turn.summary != ReasoningSummary::default() {
            c.model_reasoning_summary = Some(turn.summary);
        } else if let Some(s) = overrides.summary {
            c.model_reasoning_summary = Some(s);
        }

        if turn.personality.is_some() {
            c.personality = turn.personality;
        } else if let Some(p) = overrides.personality {
            c.personality = Some(p);
        }

        if let Some(ref m) = overrides.model {
            c.model = Some(m.clone());
        }

        Arc::new(c)
    }

    /// Build the full prompt (context items + developer instructions + history + tools).
    fn build_prompt(
        &self,
        history: Vec<ResponseItem>,
        turn_config: &codex_core::config::Config,
    ) -> Prompt {
        let mut input_items = self.context_items.clone();

        if let Some(ref dev_instructions) = self.input.developer_instructions {
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

        Prompt {
            input: input_items,
            tools: self.tools.clone(),
            parallel_tool_calls: false,
            base_instructions: self.base_instructions.clone(),
            personality: turn_config.personality,
            output_schema: None,
        }
    }

    /// Inner model→tool iteration loop for a single turn.
    async fn run_agentic_loop(
        &mut self,
        ctx: &mut WorkflowContext<AgentWorkflow>,
        turn_id: &str,
        turn_config: &Arc<codex_core::config::Config>,
        turn_context: &Arc<TurnContext>,
        overrides: &TurnOverrides,
    ) -> (bool, Option<codex_core::error::CodexErr>) {
        let cancellation_token = CancellationToken::new();
        ctx.state_mut(|s| {
            s.current_turn_cancellation = Some(cancellation_token.clone());
            s.interrupt_requested = false;
        });

        let mut streamer = TemporalModelStreamer::new(
            ctx.clone(),
            self.conversation_id.to_string(),
            self.input.model_provider.clone(),
            cancellation_token.clone(),
        );

        let diff_tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));
        let mut iterations = 0u32;
        let mut turn_aborted = false;
        let mut turn_error: Option<codex_core::error::CodexErr> = None;

        loop {
            let interrupted = ctx.state(|s| s.interrupt_requested);
            if interrupted {
                tracing::info!("interrupt requested, aborting turn");
                turn_aborted = true;
                ctx.state_mut(|s| s.interrupt_requested = false);
                break;
            }

            if iterations >= self.max_iterations {
                tracing::warn!(
                    "max iterations reached ({}), stopping turn",
                    self.max_iterations
                );
                AgentWorkflow::emit_and_bump(ctx, &self.events, Event {
                    id: turn_id.to_string(),
                    msg: EventMsg::AgentMessage(AgentMessageEvent {
                        message: format!(
                            "⚠️ Maximum iterations ({}) reached. \
                             The model was still processing tool calls. \
                             You can continue with a follow-up message.",
                            self.max_iterations
                        ),
                        phase: None,
                        memory_citation: None,
                    }),
                });
                self.last_agent_message = Some(format!(
                    "Maximum iterations ({}) reached — \
                     the model was still processing. \
                     You can continue with a follow-up message.",
                    self.max_iterations
                ));
                break;
            }

            // Create handler inside the loop so it picks up the effective
            // approval policy (which may change mid-turn via signals).
            let effective_policy = ctx.state(|s| s.effective_approval_policy());
            let effective_model = overrides
                .model
                .as_deref()
                .unwrap_or(&self.input.model)
                .to_string();
            let handler = TemporalToolHandler::new(
                ctx.clone(),
                self.events.clone(),
                turn_id.to_string(),
                effective_policy,
                effective_model,
                self.config.cwd.to_string_lossy().to_string(),
                Some(self.config_toml.clone()),
                self.mcp_tool_names.clone(),
                self.dynamic_tool_names.clone(),
                self.config.permissions.sandbox_policy.get().clone(),
            );

            let history = self.sess.history_items().await;
            let prompt = self.build_prompt(history, turn_config);

            let mut server_model_warning_emitted = false;
            let result = try_run_sampling_request(
                Arc::clone(&self.sess),
                Arc::clone(turn_context),
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
            self.total_iterations += 1;

            match result {
                Ok(outcome) => {
                    if let Some(msg) = outcome.last_agent_message {
                        self.last_agent_message = Some(msg);
                    }
                    if !outcome.needs_follow_up {
                        break;
                    }
                    tracing::debug!(iteration = iterations, "follow-up needed, continuing loop");
                }
                Err(codex_core::error::CodexErr::TurnAborted) => {
                    tracing::debug!("try_run_sampling_request returned TurnAborted");
                    turn_aborted = true;
                    ctx.state_mut(|s| s.interrupt_requested = false);
                    break;
                }
                Err(e) => {
                    tracing::error!(error = %e, "try_run_sampling_request failed");
                    turn_error = Some(e);
                    break;
                }
            }
        }

        ctx.state_mut(|s| s.current_turn_cancellation = None);
        (turn_aborted, turn_error)
    }

    /// Emit the appropriate turn-end event (TurnAborted, Error, or TurnComplete).
    fn emit_turn_end_events(
        &self,
        ctx: &mut WorkflowContext<AgentWorkflow>,
        turn_id: &str,
        aborted: bool,
        error: Option<&codex_core::error::CodexErr>,
    ) {
        if aborted {
            AgentWorkflow::emit_and_bump(ctx, &self.events, Event {
                id: turn_id.to_string(),
                msg: EventMsg::TurnAborted(TurnAbortedEvent {
                    turn_id: Some(turn_id.to_string()),
                    reason: TurnAbortReason::Interrupted,
                }),
            });
        } else {
            if let Some(err) = error {
                let error_event = err.to_error_event(None);
                AgentWorkflow::emit_and_bump(ctx, &self.events, Event {
                    id: turn_id.to_string(),
                    msg: EventMsg::Error(error_event),
                });
            }

            AgentWorkflow::emit_and_bump(ctx, &self.events, Event {
                id: turn_id.to_string(),
                msg: EventMsg::TurnComplete(TurnCompleteEvent {
                    turn_id: turn_id.to_string(),
                    last_agent_message: self.last_agent_message.clone(),
                }),
            });
        }
    }
}

/// Re-export the macro-generated `Run` marker type so other modules (e.g.
/// `session.rs`) can parameterize `WorkflowHandle<Client, AgentWorkflowRun>`.
pub use agent_workflow::Run as AgentWorkflowRun;

/// Backward-compatible aliases.
pub type CodexWorkflow = AgentWorkflow;
pub type CodexWorkflowRun = AgentWorkflowRun;
