//! MCP (Model Context Protocol) server management for the Temporal harness.
//!
//! [`HarnessMcpManager`] holds persistent connections to user-configured MCP
//! servers, supporting tool discovery and tool execution as Temporal activities.

use std::collections::HashMap;
use std::ffi::OsString;
use std::time::Duration;

use anyhow::{Result, anyhow};
use codex_core::config::types::{McpServerConfig, McpServerTransportConfig};
use codex_core::mcp::split_qualified_tool_name;
use codex_rmcp_client::{OAuthCredentialsStoreMode, RmcpClient, SendElicitation};
use rmcp::model::{InitializeRequestParams, Tool};

const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(60);

/// Manages persistent MCP server connections for the worker.
pub struct HarnessMcpManager {
    clients: HashMap<String, ManagedMcpServer>,
}

struct ManagedMcpServer {
    client: RmcpClient,
    /// Raw tool name → rmcp Tool (unqualified names).
    tools: HashMap<String, Tool>,
    tool_timeout: Option<Duration>,
}

/// No-op elicitation callback (harness runs headless, no UI for elicitations).
fn no_op_elicitation() -> SendElicitation {
    Box::new(|_id, _params| {
        Box::pin(async { Err(anyhow!("elicitation not supported in temporal harness")) })
    })
}

impl HarnessMcpManager {
    /// Create an empty manager (no servers connected yet).
    pub fn new() -> Self {
        Self {
            clients: HashMap::new(),
        }
    }

    /// Connect to all enabled MCP servers and discover their tools.
    ///
    /// Returns a map of qualified tool names (`mcp__server__tool`) to
    /// `rmcp::model::Tool` objects.
    pub async fn initialize(
        &mut self,
        servers: &HashMap<String, McpServerConfig>,
    ) -> Result<HashMap<String, Tool>> {
        let mut all_tools = HashMap::new();

        for (server_name, server_config) in servers {
            if !server_config.enabled {
                tracing::info!(server = %server_name, "MCP server disabled, skipping");
                continue;
            }

            match self
                .connect_server(server_name, server_config)
                .await
            {
                Ok(tools) => {
                    for (qualified_name, tool) in tools {
                        all_tools.insert(qualified_name, tool);
                    }
                }
                Err(e) => {
                    if server_config.required {
                        return Err(anyhow!(
                            "required MCP server '{}' failed to initialize: {}",
                            server_name,
                            e
                        ));
                    }
                    tracing::warn!(
                        server = %server_name,
                        error = %e,
                        "MCP server failed to initialize (not required), skipping"
                    );
                }
            }
        }

        Ok(all_tools)
    }

    /// Connect to a single MCP server and discover its tools.
    async fn connect_server(
        &mut self,
        server_name: &str,
        config: &McpServerConfig,
    ) -> Result<HashMap<String, Tool>> {
        let startup_timeout = config.startup_timeout_sec.or(Some(DEFAULT_STARTUP_TIMEOUT));
        let tool_timeout = config.tool_timeout_sec.or(Some(DEFAULT_TOOL_TIMEOUT));

        let client = match &config.transport {
            McpServerTransportConfig::Stdio {
                command,
                args,
                env,
                env_vars,
                cwd,
            } => {
                RmcpClient::new_stdio_client(
                    OsString::from(command),
                    args.iter().map(OsString::from).collect(),
                    env.clone(),
                    env_vars,
                    cwd.clone(),
                )
                .await
                .map_err(|e| anyhow!("failed to spawn stdio MCP server '{}': {}", server_name, e))?
            }
            McpServerTransportConfig::StreamableHttp {
                url,
                bearer_token_env_var,
                http_headers,
                env_http_headers,
            } => {
                // Resolve bearer token from env var if specified.
                let bearer_token = bearer_token_env_var
                    .as_ref()
                    .and_then(|var| std::env::var(var).ok());

                RmcpClient::new_streamable_http_client(
                    server_name,
                    url,
                    bearer_token,
                    http_headers.clone(),
                    env_http_headers.clone(),
                    OAuthCredentialsStoreMode::Auto,
                )
                .await
                .map_err(|e| {
                    anyhow!(
                        "failed to create HTTP MCP client for '{}': {}",
                        server_name,
                        e
                    )
                })?
            }
        };

        // Initialize the MCP handshake.
        let init_params = InitializeRequestParams::default();
        client
            .initialize(init_params, startup_timeout, no_op_elicitation())
            .await
            .map_err(|e| anyhow!("MCP server '{}' initialize failed: {}", server_name, e))?;

        // Discover tools.
        let list_result = client
            .list_tools(None, startup_timeout)
            .await
            .map_err(|e| anyhow!("MCP server '{}' list_tools failed: {}", server_name, e))?;

        let mut server_tools = HashMap::new();
        let mut qualified_tools = HashMap::new();

        for tool in list_result.tools {
            let raw_name = tool.name.to_string();

            // Apply enabled/disabled filters.
            if let Some(ref enabled) = config.enabled_tools {
                if !enabled.contains(&raw_name) {
                    continue;
                }
            }
            if let Some(ref disabled) = config.disabled_tools {
                if disabled.contains(&raw_name) {
                    continue;
                }
            }

            let qualified_name = format!("mcp__{}__{}", server_name, raw_name);
            qualified_tools.insert(qualified_name, tool.clone());
            server_tools.insert(raw_name, tool);
        }

        tracing::info!(
            server = %server_name,
            tools = qualified_tools.len(),
            "MCP server connected and tools discovered"
        );

        self.clients.insert(
            server_name.to_string(),
            ManagedMcpServer {
                client,
                tools: server_tools,
                tool_timeout,
            },
        );

        Ok(qualified_tools)
    }

    /// Call a tool on the appropriate MCP server.
    ///
    /// `qualified_name` must be in the format `mcp__server__tool`.
    pub async fn call_tool(
        &self,
        qualified_name: &str,
        arguments: Option<serde_json::Value>,
    ) -> Result<codex_protocol::mcp::CallToolResult> {
        let (server_name, tool_name) = split_qualified_tool_name(qualified_name)
            .ok_or_else(|| anyhow!("invalid MCP tool name: {}", qualified_name))?;

        let server = self
            .clients
            .get(&server_name)
            .ok_or_else(|| anyhow!("MCP server '{}' not found", server_name))?;

        if !server.tools.contains_key(&tool_name) {
            return Err(anyhow!(
                "tool '{}' not found on MCP server '{}'",
                tool_name,
                server_name
            ));
        }

        let rmcp_result = server
            .client
            .call_tool(tool_name, arguments, server.tool_timeout)
            .await
            .map_err(|e| anyhow!("MCP tool call failed: {}", e))?;

        // Convert rmcp::model::CallToolResult → codex_protocol::mcp::CallToolResult
        // via JSON round-trip.
        let value = serde_json::to_value(&rmcp_result)
            .map_err(|e| anyhow!("failed to serialize rmcp CallToolResult: {}", e))?;
        let result: codex_protocol::mcp::CallToolResult = serde_json::from_value(value)
            .map_err(|e| anyhow!("failed to deserialize into protocol CallToolResult: {}", e))?;

        Ok(result)
    }

    /// Returns true if any MCP tools were discovered.
    pub fn has_tools(&self) -> bool {
        self.clients.values().any(|s| !s.tools.is_empty())
    }
}
