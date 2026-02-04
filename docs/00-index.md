# Codex Feature Documentation

## Overview

This documentation reverse-engineers the OpenAI Codex agent architecture for implementing a clean-room agentic harness. The focus is on extracting architectural patterns, data structures, and workflows - not on copying implementation details.

**Target Use Case:** Building a Temporal-compliant agentic system that supports durable execution, conversation state persistence, tool calling, and multi-agent collaboration.

## Document Index

| # | Document | Description |
|---|----------|-------------|
| 00 | [Overview](./00-overview.md) | High-level architecture overview |
| 01 | [Agent Loop](./01-agent-loop.md) | Core agent loop, turn processing, streaming |
| 02 | [Model Integration](./02-model-integration.md) | Model client, API protocols, providers |
| 03 | [Tool System](./03-tool-system.md) | Tool routing, handlers, execution |
| 04 | [Conversation State](./04-conversation-state.md) | History management, compaction, persistence |
| 05 | [Plan Mode](./05-plan-mode.md) | Collaboration modes, plan-before-act workflow |
| 06 | [Sub-Agents](./06-sub-agents.md) | Child threads, agent spawning, coordination |
| 07 | [Skills & Prompts](./07-skills-prompts.md) | Custom prompts, skill injection |
| 08 | [Configuration](./08-configuration.md) | Settings, profiles, feature flags |
| 09 | [Extension Points](./09-extension-points.md) | Dynamic tools, events, embedding API |

## Architecture Summary

### Core Components

```
┌─────────────────────────────────────────────────────────────────────────┐
│                         Codex Agent Architecture                         │
│                                                                          │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │  ThreadManager                                                   │    │
│  │    └─ CodexThread (session)                                      │    │
│  │         ├─ ContextManager (conversation history)                 │    │
│  │         ├─ RolloutRecorder (persistence)                         │    │
│  │         └─ AgentControl (sub-agent coordination)                 │    │
│  └─────────────────────────────────────────────────────────────────┘    │
│                              │                                           │
│                              ▼                                           │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │  Agentic Loop (run_turn)                                         │    │
│  │    1. Build prompt (history + tools + instructions)              │    │
│  │    2. Stream to model                                            │    │
│  │    3. Process response items                                     │    │
│  │    4. Execute tool calls                                         │    │
│  │    5. Loop until no more tool calls                              │    │
│  └─────────────────────────────────────────────────────────────────┘    │
│                              │                                           │
│                              ▼                                           │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │  Tool System                                                     │    │
│  │    ├─ Built-in tools (shell, file ops, etc.)                     │    │
│  │    ├─ MCP tools (external servers)                               │    │
│  │    └─ Dynamic tools (embedding callbacks)                        │    │
│  └─────────────────────────────────────────────────────────────────┘    │
│                                                                          │
└─────────────────────────────────────────────────────────────────────────┘
```

### Key Patterns

1. **Streaming Response Processing**
   - Items arrive via streaming (message, tool_call, reasoning)
   - Tool calls are dispatched as they complete
   - Multiple tool calls can execute in parallel

2. **Conversation State**
   - History stored as `Vec<ResponseItem>`
   - Auto-compaction when context window fills
   - Persisted to JSONL for resumption

3. **Tool Execution**
   - Approval policies (untrusted, on-request, never)
   - Sandbox modes (read-only, workspace-write, full-access)
   - Parallel execution with locks for mutating tools

4. **Multi-Agent**
   - Parent agents spawn child agents
   - Depth limits prevent infinite recursion
   - Shared ThreadManager coordinates all threads

5. **Configuration Layers**
   - User config → Project config → CLI overrides
   - Profiles for quick switching
   - Feature flags for gradual rollout

## Temporal Mapping

For Temporal implementation, map these concepts:

| Codex Concept | Temporal Concept |
|---------------|------------------|
| Turn | Single workflow execution |
| Tool call | Activity invocation |
| Conversation history | Workflow state |
| Auto-compact | Workflow continue-as-new |
| Sub-agent | Child workflow |
| Approval | Signal wait |
| Rollout persistence | Event sourcing (built-in) |

## Getting Started

1. Start with [00-overview.md](./00-overview.md) for high-level architecture
2. Read [01-agent-loop.md](./01-agent-loop.md) to understand the core flow
3. Study [03-tool-system.md](./03-tool-system.md) for tool execution patterns
4. Review [04-conversation-state.md](./04-conversation-state.md) for state management
5. Study [09-extension-points.md](./09-extension-points.md) for embedding options

## Key Types Reference

### Core Types

```rust
// Turn context
struct TurnContext {
    client: Arc<dyn ModelClient>,
    cwd: PathBuf,
    approval_policy: AskForApproval,
    sandbox_policy: SandboxPolicy,
    collaboration_mode: CollaborationMode,
    // ...
}

// Conversation item
enum ResponseItem {
    Message { role, content },
    FunctionCall { call_id, name, arguments },
    FunctionCallOutput { call_id, output },
    Reasoning { content },
    // ...
}

// Tool call
struct ToolCall {
    tool_name: String,
    call_id: String,
    payload: ToolPayload,
}

// Agent status
enum AgentStatus {
    PendingInit,
    Running,
    Completed(Option<String>),
    Errored(String),
    Shutdown,
    NotFound,
}
```

### Configuration Types

```rust
// Approval policy
enum AskForApproval {
    UnlessTrusted,  // Ask for non-safe commands
    OnFailure,      // Auto-approve in sandbox
    OnRequest,      // Model decides (default)
    Never,          // Fully autonomous
}

// Sandbox mode
enum SandboxMode {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

// Collaboration mode
enum ModeKind {
    Plan,            // Think-before-act
    Code,            // Default coding
    PairProgramming, // Interactive
    Execute,         // Autonomous
}
```

## Notes

- This documentation is based on code analysis, not official documentation
- Codex evolves rapidly; some details may change
- Focus on patterns and concepts over specific implementations
- Use this as a reference for building similar systems, not as a spec to follow exactly
