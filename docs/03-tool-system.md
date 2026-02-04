# Tool System

## Overview

The tool system enables the agent to execute actions in the real world. It supports multiple tool types: built-in functions, MCP (Model Context Protocol) servers, and dynamic tools registered at runtime.

## Key Components

| Component | File | Purpose |
|-----------|------|---------|
| `ToolRouter` | `tools/router.rs:29` | Routes tool calls to handlers |
| `ToolRegistry` | `tools/registry.rs:46` | Maps tool names to handlers |
| `ToolHandler` | `tools/registry.rs:22` | Trait for tool implementations |
| `ToolCallRuntime` | `tools/parallel.rs:24` | Manages parallel/sequential execution |
| `McpConnectionManager` | `mcp_connection_manager.rs` | Manages MCP server connections |

## Tool Types

### 1. Function Tools

Built-in tools with JSON schema parameters:

```rust
enum ToolPayload {
    Function { arguments: String },  // JSON arguments
    // ...
}
```

Examples: `shell`, `read_file`, `write_file`, `search`

### 2. Custom Tools

Tools using the custom tool protocol (Responses API format):

```rust
enum ToolPayload {
    Custom { input: String },
    // ...
}
```

### 3. MCP Tools

External tools from MCP servers:

```rust
enum ToolPayload {
    Mcp {
        server: String,       // Server name
        tool: String,         // Tool name on server
        raw_arguments: String // JSON arguments
    },
}
```

### 4. LocalShell Tools

Special shell execution format:

```rust
enum ToolPayload {
    LocalShell { params: ShellToolCallParams },
}

struct ShellToolCallParams {
    command: Vec<String>,
    workdir: Option<PathBuf>,
    timeout_ms: Option<u64>,
    sandbox_permissions: Option<SandboxPermissions>,
}
```

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                    Model Response                                │
│          (FunctionCall, CustomToolCall, LocalShellCall)         │
└─────────────────────────────────┬───────────────────────────────┘
                                  │
                                  ▼
┌─────────────────────────────────────────────────────────────────┐
│  ToolRouter::build_tool_call(item)                              │
│                                                                  │
│  • Parse response item                                           │
│  • Determine tool type (Function, MCP, Custom, LocalShell)       │
│  • Build ToolCall { tool_name, call_id, payload }                │
│                                                                  │
└─────────────────────────────────┬───────────────────────────────┘
                                  │
                                  ▼
┌─────────────────────────────────────────────────────────────────┐
│  ToolCallRuntime::handle_tool_call(call, cancel_token)          │
│                                                                  │
│  • Check parallel support                                        │
│  • Acquire execution lock (RwLock)                               │
│  • Spawn execution task                                          │
│  • Handle cancellation                                           │
│                                                                  │
└─────────────────────────────────┬───────────────────────────────┘
                                  │
                                  ▼
┌─────────────────────────────────────────────────────────────────┐
│  ToolRouter::dispatch_tool_call(invocation)                     │
│                                                                  │
│  • Look up handler in registry                                   │
│  • Validate payload type matches handler                         │
│  • Call handler.handle(invocation)                               │
│  • Format response                                               │
│                                                                  │
└─────────────────────────────────┬───────────────────────────────┘
                                  │
                                  ▼
┌─────────────────────────────────────────────────────────────────┐
│  ToolHandler::handle(invocation) → ToolOutput                   │
│                                                                  │
│  Implementations:                                                │
│  • ShellHandler → exec.rs                                        │
│  • FileReadHandler → file operations                             │
│  • McpHandler → MCP server call                                  │
│  • DynamicHandler → external callback                            │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

## ToolHandler Trait

```rust
#[async_trait]
pub trait ToolHandler: Send + Sync {
    /// What kind of payload this handler accepts
    fn kind(&self) -> ToolKind;

    /// Check if payload matches handler's expected type
    fn matches_kind(&self, payload: &ToolPayload) -> bool;

    /// Whether this tool might mutate the environment
    /// (affects execution ordering)
    async fn is_mutating(&self, invocation: &ToolInvocation) -> bool;

    /// Execute the tool and return output
    async fn handle(&self, invocation: ToolInvocation)
        -> Result<ToolOutput, FunctionCallError>;
}

enum ToolKind {
    Function,
    Mcp,
}
```

## Tool Registration

### Building the Registry

```rust
impl ToolRouter {
    pub fn from_config(
        config: &ToolsConfig,
        mcp_tools: Option<HashMap<String, Tool>>,
        dynamic_tools: &[DynamicToolSpec],
    ) -> Self {
        let builder = build_specs(config, mcp_tools, dynamic_tools);
        let (specs, registry) = builder.build();
        Self { registry, specs }
    }
}
```

### ToolRegistryBuilder

```rust
impl ToolRegistryBuilder {
    pub fn push_spec(&mut self, spec: ToolSpec);
    pub fn push_spec_with_parallel_support(&mut self, spec: ToolSpec, parallel: bool);
    pub fn register_handler(&mut self, name: String, handler: Arc<dyn ToolHandler>);
    pub fn build(self) -> (Vec<ConfiguredToolSpec>, ToolRegistry);
}
```

## Parallel vs Sequential Execution

Tools are executed based on their parallelism support:

```rust
pub(crate) struct ToolCallRuntime {
    parallel_execution: Arc<RwLock<()>>,
    // ...
}

impl ToolCallRuntime {
    pub fn handle_tool_call(self, call: ToolCall, cancel: CancellationToken) {
        let supports_parallel = self.router.tool_supports_parallel(&call.tool_name);

        tokio::spawn(async move {
            // Acquire lock based on parallel support
            let _guard = if supports_parallel {
                Either::Left(lock.read().await)   // Shared lock
            } else {
                Either::Right(lock.write().await) // Exclusive lock
            };

            router.dispatch_tool_call(session, turn, tracker, call).await
        });
    }
}
```

**Parallel tools**: Can run concurrently (read lock)
**Sequential tools**: Run one at a time (write lock)

## MCP Integration

### Tool Naming

MCP tools use qualified names: `mcp__<server>__<tool>`

```rust
const MCP_TOOL_NAME_DELIMITER: &str = "__";

// Example: mcp__filesystem__read_file
fn qualify_tools(tools: I) -> HashMap<String, ToolInfo> {
    let qualified_name = format!(
        "mcp{}{}{}{}",
        MCP_TOOL_NAME_DELIMITER, server_name,
        MCP_TOOL_NAME_DELIMITER, tool_name
    );
    // Sanitize for Responses API: ^[a-zA-Z0-9_-]+$
    sanitize_responses_api_tool_name(&qualified_name)
}
```

### McpConnectionManager

Manages connections to all configured MCP servers:

```rust
impl McpConnectionManager {
    /// List all tools from all connected servers
    pub async fn list_all_tools() -> HashMap<String, ToolInfo>;

    /// Call a tool on a specific server
    pub async fn call_tool(
        server: &str,
        tool: &str,
        arguments: Value,
    ) -> Result<CallToolResult>;

    /// List resources from servers
    pub async fn list_resources() -> ListResourcesResult;

    /// Read a specific resource
    pub async fn read_resource(uri: &str) -> ReadResourceResult;
}
```

### MCP Server Configuration

```toml
[mcp_servers.filesystem]
command = "npx"
args = ["-y", "@anthropic/mcp-server-filesystem"]
env = { "MCP_ROOT" = "/workspace" }
timeout_seconds = 30
```

## Tool Invocation Context

```rust
struct ToolInvocation {
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    tracker: SharedTurnDiffTracker,  // For file change tracking
    call_id: String,
    tool_name: String,
    payload: ToolPayload,
}
```

## Tool Output

```rust
enum ToolOutput {
    Function {
        content: String,
        content_items: Option<Vec<ContentItem>>,  // Structured content
        success: Option<bool>,
    },
    Mcp {
        result: Result<CallToolResult, String>,
    },
}

impl ToolOutput {
    fn into_response(self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        match self {
            ToolOutput::Function { content, .. } => {
                if matches!(payload, ToolPayload::Custom { .. }) {
                    ResponseInputItem::CustomToolCallOutput { call_id, output: content }
                } else {
                    ResponseInputItem::FunctionCallOutput { call_id, output: ... }
                }
            }
            ToolOutput::Mcp { result } => {
                ResponseInputItem::McpToolCallOutput { call_id, result }
            }
        }
    }
}
```

## Tool Gate (Mutating Tools)

For tools that modify the environment, a gate ensures ordering:

```rust
async fn dispatch(&self, invocation: ToolInvocation) -> Result<ResponseInputItem> {
    if handler.is_mutating(&invocation).await {
        // Wait for any pending approvals or checks
        invocation.turn.tool_call_gate.wait_ready().await;
    }
    handler.handle(invocation).await
}
```

## Error Handling

```rust
enum FunctionCallError {
    /// Fatal error - abort the turn
    Fatal(String),

    /// Respond to model with error message
    RespondToModel(String),

    /// Missing required call ID
    MissingLocalShellCallId,
}
```

Errors are converted to tool output and sent back to the model:

```rust
fn failure_response(call_id: String, err: FunctionCallError) -> ResponseInputItem {
    ResponseInputItem::FunctionCallOutput {
        call_id,
        output: FunctionCallOutputPayload {
            content: err.to_string(),
            success: Some(false),
        },
    }
}
```

## Tool Specification

Tools are described to the model via `ToolSpec`:

```rust
enum ToolSpec {
    Function {
        name: String,
        description: String,
        parameters: JsonSchema,  // JSON Schema for arguments
    },
    Freeform {
        name: String,
        description: String,
        // No schema - free text input
    },
}
```

## Adding a New Tool

1. **Create handler** implementing `ToolHandler`:

```rust
struct MyToolHandler;

#[async_trait]
impl ToolHandler for MyToolHandler {
    fn kind(&self) -> ToolKind { ToolKind::Function }

    async fn is_mutating(&self, _: &ToolInvocation) -> bool { false }

    async fn handle(&self, inv: ToolInvocation) -> Result<ToolOutput> {
        let args: MyArgs = serde_json::from_str(&inv.payload.arguments())?;
        let result = do_something(args);
        Ok(ToolOutput::Function {
            content: result,
            content_items: None,
            success: Some(true),
        })
    }
}
```

2. **Register in builder**:

```rust
builder.push_spec(ToolSpec::Function {
    name: "my_tool".to_string(),
    description: "Does something".to_string(),
    parameters: my_schema(),
});
builder.register_handler("my_tool", Arc::new(MyToolHandler));
```

## Deeper Investigation Pointers

| Topic | Where to Look |
|-------|---------------|
| Shell execution | `exec.rs` |
| File operations | `tools/file_*.rs` |
| MCP protocol | `codex_rmcp_client` crate |
| Tool specs/schemas | `tools/spec.rs` |
| Sandbox/approval | `sandboxing/`, `approval/` |
| Dynamic tools | `dynamic_tools.rs` |
