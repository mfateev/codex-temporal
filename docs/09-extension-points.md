# Extension Points & Customization

## Overview

Codex provides several extension points for customization: dynamic tools (external tool handlers), MCP server integration, skills, event callbacks, custom prompts, and the client/server protocol. This enables building custom agents, IDE integrations, and specialized workflows.

## Key Components

| Component | File | Purpose |
|-----------|------|---------|
| `Op` | `protocol/protocol.rs:89` | Operations sent to the agent |
| `EventMsg` | `protocol/protocol.rs:690` | Events emitted by the agent |
| `DynamicToolSpec` | `protocol/dynamic_tools.rs` | External tool definitions |
| `McpServerConfig` | `config/types.rs` | MCP server configuration |
| `SkillMetadata` | `skills/model.rs` | Skill definitions |

## Operations (Op)

Operations are commands sent to the Codex agent:

```rust
enum Op {
    /// Abort current task
    Interrupt,

    /// Legacy user input
    UserInput {
        items: Vec<UserInput>,
        final_output_json_schema: Option<Value>,
    },

    /// Full turn context (preferred)
    UserTurn {
        items: Vec<UserInput>,
        cwd: PathBuf,
        approval_policy: AskForApproval,
        sandbox_policy: SandboxPolicy,
        model: String,
        effort: Option<ReasoningEffortConfig>,
        summary: ReasoningSummaryConfig,
        final_output_json_schema: Option<Value>,
        collaboration_mode: Option<CollaborationMode>,
    },

    /// Respond to approval request
    ExecApprovalResponse {
        call_id: String,
        approved: bool,
    },

    /// Respond to user input request
    UserInputResponse {
        call_id: String,
        response: UserInputResponsePayload,
    },

    /// Respond to dynamic tool call
    DynamicToolResponse(DynamicToolResponse),

    /// Request conversation compaction
    RequestCompact,

    /// Undo last N turns
    RequestUndo { num_user_messages_to_undo: usize },

    /// Request history entry
    GetHistoryEntryRequest { sequence_id: u64 },

    /// Set collaboration mode
    SetCollaborationMode(CollaborationMode),

    /// Shutdown the agent
    Shutdown,
}
```

## Events (EventMsg)

Events are emitted by the agent for UI/external handling:

### Turn Lifecycle Events

```rust
// Turn started
TurnStarted(TurnStartedEvent)

// Turn completed successfully
TurnComplete(TurnCompleteEvent)

// Turn was aborted
TurnAborted(TurnAbortedEvent)
```

### Message Events

```rust
// Complete agent message
AgentMessage(AgentMessageEvent)

// Streaming message delta
AgentMessageDelta(AgentMessageDeltaEvent)

// User/system message
UserMessage(UserMessageEvent)

// Reasoning events
AgentReasoning(AgentReasoningEvent)
AgentReasoningDelta(AgentReasoningDeltaEvent)
```

### Tool Execution Events

```rust
// Shell command events
ExecCommandBegin(ExecCommandBeginEvent)
ExecCommandOutputDelta(ExecCommandOutputDeltaEvent)
ExecCommandEnd(ExecCommandEndEvent)

// MCP tool events
McpToolCallBegin(McpToolCallBeginEvent)
McpToolCallEnd(McpToolCallEndEvent)

// Patch application
PatchApplyBegin(PatchApplyBeginEvent)
PatchApplyEnd(PatchApplyEndEvent)
```

### Approval Events

```rust
// Request approval for execution
ExecApprovalRequest(ExecApprovalRequestEvent)

// Request user input
RequestUserInput(RequestUserInputEvent)

// Request patch approval
ApplyPatchApprovalRequest(ApplyPatchApprovalRequestEvent)

// Dynamic tool call request
DynamicToolCallRequest(DynamicToolCallRequest)
```

### Sub-Agent Events

```rust
// Agent spawn lifecycle
CollabAgentSpawnBegin(CollabAgentSpawnBeginEvent)
CollabAgentSpawnEnd(CollabAgentSpawnEndEvent)

// Agent interaction
CollabAgentInteractionBegin(CollabAgentInteractionBeginEvent)
CollabAgentInteractionEnd(CollabAgentInteractionEndEvent)

// Wait for agents
CollabWaitingBegin(CollabWaitingBeginEvent)
CollabWaitingEnd(CollabWaitingEndEvent)
```

## Dynamic Tools

External tools handled by the embedding application:

### Tool Specification

```rust
struct DynamicToolSpec {
    /// Tool name (must be unique)
    name: String,

    /// Description for the model
    description: String,

    /// JSON Schema for input parameters
    input_schema: serde_json::Value,
}
```

### Tool Call Flow

```
┌─────────────────────────────────────────────────────────────────────────┐
│  Model                                                                   │
│                                                                          │
│  1. Model decides to call dynamic tool                                   │
│     ▼                                                                    │
└─────────────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────────────┐
│  Codex Core                                                              │
│                                                                          │
│  2. Emit DynamicToolCallRequest event                                    │
│     { call_id, turn_id, tool, arguments }                                │
│     ▼                                                                    │
└─────────────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────────────┐
│  External Handler (your code)                                            │
│                                                                          │
│  3. Receive event, execute tool logic                                    │
│  4. Send Op::DynamicToolResponse                                         │
│     { call_id, output, success }                                         │
│     ▼                                                                    │
└─────────────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────────────┐
│  Codex Core                                                              │
│                                                                          │
│  5. Feed tool result to model                                            │
│  6. Continue turn                                                        │
│                                                                          │
└─────────────────────────────────────────────────────────────────────────┘
```

### Dynamic Tool Protocol

```rust
// Request event
struct DynamicToolCallRequest {
    call_id: String,       // Unique call identifier
    turn_id: String,       // Turn this belongs to
    tool: String,          // Tool name
    arguments: JsonValue,  // Parsed arguments
}

// Response operation
struct DynamicToolResponse {
    call_id: String,       // Must match request
    output: String,        // Tool output
    success: bool,         // Success/failure
}
```

### Registering Dynamic Tools

```rust
// When configuring the session
let dynamic_tools = vec![
    DynamicToolSpec {
        name: "custom_search".to_string(),
        description: "Search internal knowledge base".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query"
                }
            },
            "required": ["query"]
        }),
    },
];

// Pass to Config/Session initialization
config.dynamic_tools = dynamic_tools;
```

## MCP Server Extension

### Adding MCP Servers

```toml
# In config.toml
[mcp_servers.my_server]
command = "/path/to/my-mcp-server"
args = ["--port", "3000"]
env = { "MY_VAR" = "value" }
startup_timeout_sec = 30
```

### MCP Tool Filtering

```toml
[mcp_servers.limited]
command = "npx"
args = ["-y", "@example/mcp-server"]
enabled_tools = ["safe_tool_1", "safe_tool_2"]
disabled_tools = ["dangerous_tool"]
```

### MCP Server Implementation

Implement the MCP protocol:

```typescript
// TypeScript MCP server example
import { Server } from '@modelcontextprotocol/sdk/server';

const server = new Server({
  name: 'my-custom-server',
  version: '1.0.0',
});

server.setRequestHandler('tools/list', async () => ({
  tools: [{
    name: 'my_tool',
    description: 'Does something useful',
    inputSchema: {
      type: 'object',
      properties: { input: { type: 'string' } },
    },
  }],
}));

server.setRequestHandler('tools/call', async (request) => {
  const { name, arguments: args } = request.params;
  // Handle tool call
  return { content: [{ type: 'text', text: 'Result' }] };
});
```

## Skills Extension

### Creating Custom Skills

Create `.codex/skills/my-skill.skill.md`:

```markdown
---
name: my-skill
description: Custom workflow for my use case
allowed_tools:
  - shell
  - read_file
  - write_file
mcp_tools:
  - server: my_server
    tool: my_tool
---

# My Skill Instructions

When this skill is invoked ($my-skill), follow these steps:

1. First, do X
2. Then, do Y
3. Finally, do Z

## Important Guidelines

- Always check for errors
- Use the custom tool when needed
```

### Skill Scopes

| Scope | Location | Priority |
|-------|----------|----------|
| User | `~/.codex/skills/` | Highest |
| Repo | `<repo>/.codex/skills/` | Medium |
| System | Built-in | Lowest |

## Custom Prompts

### Project Instructions (AGENTS.md)

Create `AGENTS.md` (or `CLAUDE.md`) in project root:

```markdown
# Project Instructions

## Code Style
- Use 4-space indentation
- Follow PEP 8 for Python

## Testing
- All new code must have tests
- Run `pytest` before committing

## Architecture
- Use dependency injection
- Avoid global state
```

### Base Instructions Override

In config:

```toml
# Override the system prompt
model_instructions_file = "/path/to/custom_instructions.md"
```

### Developer Instructions

Per-turn instructions injected into conversation:

```rust
let turn = Op::UserTurn {
    items: vec![...],
    collaboration_mode: Some(CollaborationMode {
        mode: ModeKind::Code,
        settings: Settings {
            developer_instructions: Some("Focus on security".to_string()),
            ...
        },
    }),
    ...
};
```

## Embedding API

### Session Creation

```rust
// Create a new session
let config = Config::load_with_cli_overrides(vec![]).await?;
let session = Session::new(config).await?;

// Start event loop
let events = session.subscribe_events();
tokio::spawn(async move {
    while let Ok(event) = events.recv().await {
        handle_event(event);
    }
});

// Send operations
session.send_op(Op::UserTurn { ... }).await;
```

### Event Handling

```rust
async fn handle_event(event: EventMsg) {
    match event {
        EventMsg::AgentMessage(msg) => {
            println!("Agent: {}", msg.content);
        }
        EventMsg::ExecApprovalRequest(req) => {
            // Auto-approve or prompt user
            let approved = should_approve(&req);
            session.send_op(Op::ExecApprovalResponse {
                call_id: req.call_id,
                approved,
            }).await;
        }
        EventMsg::DynamicToolCallRequest(req) => {
            // Handle custom tool
            let result = execute_dynamic_tool(&req);
            session.send_op(Op::DynamicToolResponse(DynamicToolResponse {
                call_id: req.call_id,
                output: result,
                success: true,
            })).await;
        }
        EventMsg::TurnComplete(_) => {
            println!("Turn finished");
        }
        _ => {}
    }
}
```

## Approval Callbacks

### Custom Approval Logic

```rust
async fn handle_approval(session: &Session, req: ExecApprovalRequestEvent) {
    // Implement custom approval logic
    let approved = match &req.command {
        cmd if cmd.starts_with("git ") => true,        // Auto-approve git
        cmd if cmd.contains("rm ") => false,           // Deny deletions
        _ => prompt_user(&req),                        // Ask user
    };

    session.send_op(Op::ExecApprovalResponse {
        call_id: req.call_id,
        approved,
    }).await;
}
```

### User Input Callbacks

```rust
async fn handle_user_input_request(session: &Session, req: RequestUserInputEvent) {
    // Present options to user
    let selected = show_ui_dialog(&req.options);

    session.send_op(Op::UserInputResponse {
        call_id: req.call_id,
        response: UserInputResponsePayload {
            selected_options: vec![selected],
            custom_text: None,
        },
    }).await;
}
```

## Thread Management

### Creating Threads

```rust
let thread_manager = ThreadManager::new();

// Create a new thread
let new_thread = thread_manager.start_thread(config).await?;
let thread_id = new_thread.thread_id;

// Send to thread
new_thread.send_op(Op::UserTurn { ... }).await;
```

### Resuming Sessions

```rust
// Resume from rollout file
let resumed = thread_manager.resume_thread_from_rollout(
    config,
    PathBuf::from("~/.codex/sessions/2025/01/15/rollout-abc123.jsonl"),
    auth_manager,
).await?;
```

### Forking Threads

```rust
// Fork at a specific point in history
let forked = thread_manager.fork_thread(
    3,  // Fork after 3rd user message
    config,
    rollout_path,
).await?;
```

## Configuration Hooks

### Cloud Requirements

Enterprise deployments can enforce configuration:

```rust
struct CloudRequirementsLoader {
    // Loads requirements from enterprise server
    async fn load(&self) -> ConfigRequirements;
}

struct ConfigRequirements {
    // Force specific MCP servers
    mcp_servers: Vec<McpServerRequirement>,

    // Enforce residency requirements
    residency: Option<ResidencyRequirement>,

    // Constrain approval policies
    min_approval_policy: Option<AskForApproval>,
}
```

### Config Layer Overrides

```rust
// Programmatic overrides
let overrides = ConfigOverrides {
    cwd: Some(PathBuf::from("/custom/path")),
    model: Some("gpt-4o".to_string()),
    sandbox_mode: Some(SandboxMode::ReadOnly),
    dynamic_tools: vec![...],
};

let config = Config::load_from_base_config_with_overrides(
    base_config,
    overrides,
    codex_home,
)?;
```

## Extension Architecture

```
┌─────────────────────────────────────────────────────────────────────────┐
│                          Your Application                                │
│                                                                          │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐                   │
│  │ Event Handler│  │ Approval     │  │ Dynamic Tool │                   │
│  │              │  │ Logic        │  │ Handlers     │                   │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘                   │
│         │                 │                 │                            │
│         └─────────────────┴─────────────────┘                            │
│                           │                                              │
│                           ▼                                              │
│  ┌────────────────────────────────────────────────────────────────────┐ │
│  │                    Session / Thread API                             │ │
│  │                                                                     │ │
│  │  subscribe_events() → Receiver<EventMsg>                            │ │
│  │  send_op(Op) → Result<()>                                           │ │
│  │                                                                     │ │
│  └────────────────────────────────────────────────────────────────────┘ │
│                                                                          │
└──────────────────────────────────┬───────────────────────────────────────┘
                                   │
                                   ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                          Codex Core                                      │
│                                                                          │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐    │
│  │ Tool Router │  │ MCP Manager │  │ Model Client│  │ State Mgmt  │    │
│  └─────────────┘  └─────────────┘  └─────────────┘  └─────────────┘    │
│                                                                          │
└─────────────────────────────────────────────────────────────────────────┘
                                   │
                                   ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                       External Services                                  │
│                                                                          │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐                      │
│  │ OpenAI API  │  │ MCP Servers │  │ File System │                      │
│  └─────────────┘  └─────────────┘  └─────────────┘                      │
│                                                                          │
└─────────────────────────────────────────────────────────────────────────┘
```

## Deeper Investigation Pointers

| Topic | Where to Look |
|-------|---------------|
| Op/Event protocol | `protocol/protocol.rs` |
| Dynamic tools | `protocol/dynamic_tools.rs`, `tools/handlers/dynamic.rs` |
| Session management | `codex.rs` |
| Thread management | `thread_manager.rs` |
| MCP integration | `mcp_connection_manager.rs` |
| Tool routing | `tools/router.rs` |
| Config loading | `config/mod.rs`, `config_loader/` |
| Skills | `skills/` |
