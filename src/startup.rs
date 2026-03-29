//! Shared startup helpers for loading config, project context, and MCP tools.

/// Load config and project context via Temporal activities (in parallel).
///
/// Returns `Result<(ConfigOutput, ProjectContextOutput), anyhow::Error>`.
///
/// `$ctx` must be a `&mut WorkflowContext<T>` that supports `start_activity`.
macro_rules! load_config_and_context {
    ($ctx:expr) => {{
        use crate::activities::{activity_opts, CodexActivities};
        use crate::types::{ConfigOutput, ProjectContextOutput};

        let config_activity =
            $ctx.start_activity(CodexActivities::load_config, (), activity_opts(30));
        let project_context_activity = $ctx.start_activity(
            CodexActivities::collect_project_context,
            (),
            activity_opts(30),
        );

        let (config_result, project_context_result) =
            futures::future::join(config_activity, project_context_activity).await;

        let config_output: ConfigOutput =
            config_result.map_err(|e| anyhow::anyhow!("load_config failed: {e}"))?;
        let project_context: ProjectContextOutput = project_context_result
            .map_err(|e| anyhow::anyhow!("collect_project_context failed: {e}"))?;

        Ok::<(ConfigOutput, ProjectContextOutput), anyhow::Error>((config_output, project_context))
    }};
}

/// Discover MCP tools via a Temporal activity.
///
/// Returns `HashMap<String, Value>`, falling back to an empty map on failure.
///
/// `$ctx` must be a `&mut WorkflowContext<T>` that supports `start_activity`.
macro_rules! discover_mcp {
    ($ctx:expr, $config_toml:expr, $cwd:expr) => {{
        use crate::activities::{activity_opts, CodexActivities};
        use crate::types::{McpDiscoverInput, McpDiscoverOutput};
        use std::collections::HashMap;

        let mcp_discover_input = McpDiscoverInput {
            config_toml: $config_toml.clone(),
            cwd: $cwd.clone(),
        };
        let mcp_output: McpDiscoverOutput = $ctx
            .start_activity(
                CodexActivities::discover_mcp_tools,
                mcp_discover_input,
                activity_opts(60),
            )
            .await
            .unwrap_or_else(|e| {
                tracing::warn!("MCP discovery failed: {e}");
                McpDiscoverOutput {
                    tools: HashMap::new(),
                }
            });
        mcp_output.tools
    }};
}

/// Load config, project context, and MCP tools via Temporal activities.
///
/// Starts `load_config` and `collect_project_context` in parallel, awaits both,
/// then runs `discover_mcp_tools`. Returns
/// `Result<(ConfigOutput, ProjectContextOutput, HashMap<String, Value>), anyhow::Error>`.
///
/// `$ctx` must be a `&mut WorkflowContext<T>` that supports `start_activity`.
macro_rules! load_startup_context {
    ($ctx:expr) => {{
        let (config_output, project_context) = crate::startup::load_config_and_context!($ctx)?;
        let mcp_tools =
            crate::startup::discover_mcp!($ctx, config_output.config_toml, project_context.cwd);
        Ok::<
            (
                crate::types::ConfigOutput,
                crate::types::ProjectContextOutput,
                std::collections::HashMap<String, serde_json::Value>,
            ),
            anyhow::Error,
        >((config_output, project_context, mcp_tools))
    }};
}

pub(crate) use discover_mcp;
pub(crate) use load_config_and_context;
pub(crate) use load_startup_context;
