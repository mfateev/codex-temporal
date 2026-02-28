//! The `SessionWorkflow` (parent / control-plane workflow).
//!
//! Manages agent lifecycle within a session:
//! - Loads config, project context, and MCP tools once
//! - Starts a "main" `AgentWorkflow` child
//! - Accepts `spawn_agent` signals to start additional agent children
//! - Enforces `max_agents` limit
//! - Supports graceful shutdown and continue-as-new

use std::collections::HashMap;

use temporalio_common::protos::coresdk::AsJsonPayloadExt;
use temporalio_macros::{workflow, workflow_methods};
use temporalio_sdk::{
    ActivityOptions, ChildWorkflowOptions, SyncWorkflowContext, WorkflowContext,
    WorkflowContextView, WorkflowResult, WorkflowTermination,
};
use temporalio_common::protos::coresdk::workflow_commands::ContinueAsNewWorkflowExecution;
use temporalio_common::protos::temporal::api::enums::v1::ParentClosePolicy;

use crate::activities::CodexActivities;
use crate::config_loader::config_from_toml;
use crate::types::{
    AgentLifecycle, AgentRecord, AgentSummary, AgentWorkflowInput, ConfigOutput,
    McpDiscoverInput, McpDiscoverOutput, ProjectContextOutput, ResolveRoleConfigInput,
    SessionContinueAsNewState, SessionWorkflowInput, SessionWorkflowOutput, SpawnAgentInput,
};

const TASK_QUEUE: &str = "codex-temporal";

/// Default maximum number of concurrent agents per session.
const DEFAULT_MAX_AGENTS: usize = 8;

#[workflow]
pub struct SessionWorkflow {
    input: SessionWorkflowInput,
    /// Workflow ID of this session (e.g. "codex-session-{uuid}").
    session_id: String,
    agents: Vec<AgentRecord>,
    config_toml: Option<String>,
    project_context: Option<ProjectContextOutput>,
    mcp_tools: HashMap<String, serde_json::Value>,
    spawn_queue: Vec<SpawnAgentInput>,
    agent_counter: u32,
    max_agents: usize,
    shutdown_requested: bool,
}

#[workflow_methods]
impl SessionWorkflow {
    #[init]
    pub fn new(ctx: &WorkflowContextView, input: SessionWorkflowInput) -> Self {
        let session_id = ctx.workflow_id.clone();

        // If restoring from continue-as-new, use the carried-over state.
        if let Some(ref state) = input.continued_state {
            return Self {
                session_id,
                agents: state.agents.clone(),
                config_toml: Some(state.config_toml.clone()),
                project_context: Some(state.project_context.clone()),
                mcp_tools: state.mcp_tools.clone(),
                spawn_queue: Vec::new(),
                agent_counter: state.agents.len() as u32,
                max_agents: DEFAULT_MAX_AGENTS,
                shutdown_requested: false,
                input,
            };
        }

        Self {
            session_id,
            input,
            agents: Vec::new(),
            config_toml: None,
            project_context: None,
            mcp_tools: HashMap::new(),
            spawn_queue: Vec::new(),
            agent_counter: 0,
            max_agents: DEFAULT_MAX_AGENTS,
            shutdown_requested: false,
        }
    }

    // ----- signals -----

    /// Signal to spawn a new agent with the given role and message.
    #[signal]
    pub fn spawn_agent(&mut self, _ctx: &mut SyncWorkflowContext<Self>, input: SpawnAgentInput) {
        self.spawn_queue.push(input);
    }

    /// Signal to request graceful shutdown of all agents.
    #[signal]
    pub fn shutdown(&mut self, _ctx: &mut SyncWorkflowContext<Self>) {
        self.shutdown_requested = true;
    }

    // ----- queries -----

    /// Return JSON-serialized list of tracked agents.
    #[query]
    pub fn list_agents(&self, _ctx: &WorkflowContextView) -> String {
        serde_json::to_string(&self.agents).unwrap_or_else(|_| "[]".to_string())
    }

    // ----- run -----

    #[run]
    pub async fn run(ctx: &mut WorkflowContext<Self>) -> WorkflowResult<SessionWorkflowOutput> {
        let input = ctx.state(|s| s.input.clone());
        let session_id = ctx.state(|s| s.session_id.clone());

        // --- Phase 1: load config + project context (or restore from CAN) ---
        let (config_toml, project_context, mcp_tools) = {
            let existing = ctx.state(|s| {
                (
                    s.config_toml.clone(),
                    s.project_context.clone(),
                    s.mcp_tools.clone(),
                )
            });

            if let (Some(ct), Some(pc)) = (existing.0, existing.1) {
                tracing::info!("restoring config/context from continue-as-new state");
                (ct, pc, existing.2)
            } else {
                // Load config and project context via activities (in parallel).
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

                // MCP discovery.
                let mcp_discover_input = McpDiscoverInput {
                    config_toml: config_output.config_toml.clone(),
                    cwd: project_context.cwd.clone(),
                };
                let mcp_output: McpDiscoverOutput = ctx
                    .start_activity(
                        CodexActivities::discover_mcp_tools,
                        mcp_discover_input,
                        ActivityOptions {
                            schedule_to_close_timeout: Some(std::time::Duration::from_secs(60)),
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

                (
                    config_output.config_toml,
                    project_context,
                    mcp_output.tools,
                )
            }
        };

        // Cache in workflow state for CAN and queries.
        ctx.state_mut(|s| {
            s.config_toml = Some(config_toml.clone());
            s.project_context = Some(project_context.clone());
            s.mcp_tools = mcp_tools.clone();
        });

        // Parse max_agents from config.
        if let Ok(config) = config_from_toml(
            &config_toml,
            std::path::Path::new(&project_context.cwd),
            None,
        ) {
            if let Some(max_threads) = config.agent_max_threads {
                if max_threads > 0 {
                    ctx.state_mut(|s| s.max_agents = max_threads);
                }
            }
        }

        // --- Phase 2: start the main agent ---
        let main_agent_id = format!("{session_id}/main");
        let main_input = AgentWorkflowInput {
            user_message: input.user_message.clone(),
            model: input.model.clone(),
            instructions: input.instructions.clone(),
            approval_policy: input.approval_policy,
            web_search_mode: input.web_search_mode,
            reasoning_effort: input.reasoning_effort,
            reasoning_summary: input.reasoning_summary,
            personality: input.personality,
            developer_instructions: input.developer_instructions.clone(),
            model_provider: input.model_provider.clone(),
            continued_state: None,
            role: "default".to_string(),
            config_toml: Some(config_toml.clone()),
            project_context: Some(project_context.clone()),
            mcp_tools: mcp_tools.clone(),
            dynamic_tools: Vec::new(),
        };

        let child = ctx.child_workflow(ChildWorkflowOptions {
            workflow_id: main_agent_id.clone(),
            workflow_type: "AgentWorkflow".to_string(),
            task_queue: Some(TASK_QUEUE.to_string()),
            input: vec![main_input.as_json_payload().map_err(|e| {
                WorkflowTermination::failed(anyhow::anyhow!(
                    "failed to serialize main agent input: {e}"
                ))
            })?],
            parent_close_policy: ParentClosePolicy::Terminate,
            ..Default::default()
        });

        let pending = child.start().await;
        let _started = pending.into_started().ok_or_else(|| {
            WorkflowTermination::failed(anyhow::anyhow!(
                "failed to start main agent workflow"
            ))
        })?;

        ctx.state_mut(|s| {
            s.agents.push(AgentRecord {
                agent_id: main_agent_id.clone(),
                workflow_id: main_agent_id,
                role: "default".to_string(),
                status: AgentLifecycle::Running,
            });
            s.agent_counter = 1;
        });

        tracing::info!("main agent started");

        // --- Phase 3: control loop ---
        loop {
            ctx.wait_condition(|s| {
                !s.spawn_queue.is_empty() || s.shutdown_requested
            })
            .await;

            // Check shutdown.
            let shutdown = ctx.state(|s| s.shutdown_requested);
            if shutdown {
                tracing::info!("shutdown requested, exiting control loop");
                break;
            }

            // Process spawn queue.
            let spawn_requests: Vec<SpawnAgentInput> =
                ctx.state_mut(|s| std::mem::take(&mut s.spawn_queue));

            for spawn_input in spawn_requests {
                let (current_count, max) =
                    ctx.state(|s| (s.agents.len(), s.max_agents));

                if current_count >= max {
                    tracing::warn!(
                        current = current_count,
                        max = max,
                        role = %spawn_input.role,
                        "max_agents limit reached, ignoring spawn request"
                    );
                    continue;
                }

                let agent_num = ctx.state_mut(|s| {
                    s.agent_counter += 1;
                    s.agent_counter
                });

                let agent_role = spawn_input.role.clone();
                let agent_id = format!("{session_id}/{}-{}", agent_role, agent_num);

                // Resolve role config if not "default".
                let (resolved_config_toml, resolved_model, resolved_instructions) =
                    if agent_role != "default" {
                        let resolve_input = ResolveRoleConfigInput {
                            config_toml: config_toml.clone(),
                            cwd: project_context.cwd.clone(),
                            role_name: agent_role.clone(),
                        };
                        match ctx
                            .start_activity(
                                CodexActivities::resolve_role_config,
                                resolve_input,
                                ActivityOptions {
                                    schedule_to_close_timeout: Some(
                                        std::time::Duration::from_secs(30),
                                    ),
                                    ..Default::default()
                                },
                            )
                            .await
                        {
                            Ok(resolved) => (
                                resolved.config_toml,
                                resolved.model.unwrap_or_else(|| input.model.clone()),
                                resolved
                                    .instructions
                                    .unwrap_or_else(|| input.instructions.clone()),
                            ),
                            Err(e) => {
                                tracing::error!(
                                    role = %agent_role,
                                    error = %e,
                                    "failed to resolve role config, skipping spawn"
                                );
                                continue;
                            }
                        }
                    } else {
                        (
                            config_toml.clone(),
                            input.model.clone(),
                            input.instructions.clone(),
                        )
                    };

                let child_input = AgentWorkflowInput {
                    user_message: spawn_input.message,
                    model: resolved_model,
                    instructions: resolved_instructions,
                    approval_policy: input.approval_policy,
                    web_search_mode: input.web_search_mode,
                    reasoning_effort: input.reasoning_effort,
                    reasoning_summary: input.reasoning_summary,
                    personality: input.personality,
                    developer_instructions: input.developer_instructions.clone(),
                    model_provider: input.model_provider.clone(),
                    continued_state: None,
                    role: agent_role.clone(),
                    config_toml: Some(resolved_config_toml),
                    project_context: Some(project_context.clone()),
                    mcp_tools: mcp_tools.clone(),
                    dynamic_tools: Vec::new(),
                };

                let child = ctx.child_workflow(ChildWorkflowOptions {
                    workflow_id: agent_id.clone(),
                    workflow_type: "AgentWorkflow".to_string(),
                    task_queue: Some(TASK_QUEUE.to_string()),
                    input: vec![child_input.as_json_payload().map_err(|e| {
                        WorkflowTermination::failed(anyhow::anyhow!(
                            "failed to serialize agent input: {e}"
                        ))
                    })?],
                    parent_close_policy: ParentClosePolicy::Terminate,
                    ..Default::default()
                });

                match child.start().await.into_started() {
                    Some(_started) => {
                        ctx.state_mut(|s| {
                            s.agents.push(AgentRecord {
                                agent_id: agent_id.clone(),
                                workflow_id: agent_id.clone(),
                                role: agent_role,
                                status: AgentLifecycle::Running,
                            });
                        });
                        tracing::info!(agent_id = %agent_id, "child agent started");
                    }
                    None => {
                        tracing::error!(
                            agent_id = %agent_id,
                            "failed to start child agent"
                        );
                    }
                }
            }

            // Check if CAN is suggested.
            if ctx.continue_as_new_suggested() {
                tracing::info!("server suggested continue-as-new for session");
                let agents = ctx.state(|s| s.agents.clone());
                let state = SessionContinueAsNewState {
                    agents,
                    config_toml: config_toml.clone(),
                    project_context: project_context.clone(),
                    mcp_tools: mcp_tools.clone(),
                };

                let mut can_input = input.clone();
                can_input.user_message = String::new();
                can_input.continued_state = Some(state);

                return Err(WorkflowTermination::continue_as_new(
                    ContinueAsNewWorkflowExecution {
                        arguments: vec![can_input.as_json_payload().map_err(|e| {
                            WorkflowTermination::failed(anyhow::anyhow!(
                                "failed to serialize CAN input: {e}"
                            ))
                        })?],
                        ..Default::default()
                    },
                ));
            }
        }

        // Build output summary.
        let agents = ctx.state(|s| {
            s.agents
                .iter()
                .map(|a| AgentSummary {
                    agent_id: a.agent_id.clone(),
                    role: a.role.clone(),
                    status: a.status,
                    iterations: 0,
                    token_usage: None,
                })
                .collect()
        });

        Ok(SessionWorkflowOutput { agents })
    }
}

/// Re-export the macro-generated `Run` marker type.
pub use session_workflow::Run as SessionWorkflowRun;
