# Codex-Temporal Design: Session Integration

## Problem

Codex's `Session` is a monolithic "god object" that owns everything needed to run the agent in a single process: conversation state, model client, tool execution, MCP connections, auth, analytics, sandbox, file watching, network proxy, agent lifecycle, and more.

The Temporal harness only needs the **state management** portion. `Session::new_minimal()` constructs a full `Session` with most service fields stubbed to defaults/no-ops. This works and is the chosen approach — it avoids splitting `Session` into separate types, which would require moving ~20 methods, updating all callers, and touching 16+ files for marginal benefit.

## Approach: Single Session with Stubs

Rather than extracting a `SessionCore` base type, we keep a single `Session` struct and use `Session::new_minimal()` to construct it for the Temporal workflow. The stub fields carry no meaningful runtime cost and the constructor is straightforward.

```
Session (used by both codex CLI and Temporal workflow)
├── conversation_id
├── state (Mutex<SessionState>)        ← conversation history, token info
├── features
├── active_turn
├── event_sink (dyn EventSink)         ← ChannelEventSink (codex) / BufferEventSink (Temporal)
├── storage (dyn StorageBackend)       ← RolloutFileStorage (codex) / InMemoryStorage (Temporal)
├── next_internal_sub_id
├── tx_event
├── agent_status
├── pending_mcp_server_refresh_config
├── js_repl
└── services: SessionServices
    ├── model_client                   ← stubbed in Temporal (unused — activities do model calls)
    ├── mcp_connection_manager         ← stubbed (default empty)
    ├── auth_manager                   ← stubbed (API key "harness")
    ├── otel_manager                   ← no-op (no telemetry in workflow)
    ├── ... (24 fields total)          ← all stubbed to defaults/no-ops
    └── show_raw_agent_reasoning       ← false
```

### Why this works

1. **`event_sink` and `storage`** are injected via traits — Temporal provides its own implementations.
2. **`EntropyProviders`** are scoped per-turn via task-local `ENTROPY` — Temporal injects deterministic implementations.
3. **`ModelStreamer` and `ToolCallHandler`** are generic parameters on `try_run_sampling_request` — Temporal provides its own activity-backed implementations.
4. **The 24 stubbed service fields** are never accessed in the Temporal workflow path. They're dead weight (~few KB) but cause no correctness issues.

### Why not SessionCore

We explored extracting a `SessionCore` struct (containing only the fields `try_run_sampling_request` needs) and having `Session` wrap it via `Deref`. This required:
- Moving ~20 methods from `impl Session` to `impl SessionCore`
- Updating every call site that accesses `services.otel_manager` (16+ files, 40+ references)
- Changing `try_run_sampling_request` signature from `Arc<Session>` to `Arc<SessionCore>`
- Updating all plan-mode helpers, `HandleOutputCtx`, `drain_in_flight`, etc.

The refactoring touched too many files for too little benefit. The single-Session approach with stubs is simpler, works today, and can be revisited if the stub overhead ever becomes a real problem.

## How It Maps to Temporal

Temporal decomposes `Session`'s responsibilities across its own primitives. `Session::new_minimal()` runs in the workflow; `SessionServices` functionality is replaced by activities:

```
┌─────────────────────────────────────────────────────────┐
│ Temporal Workflow (deterministic)                       │
│                                                         │
│  Session (via new_minimal)                              │
│  ├── conversation history    (workflow state)           │
│  ├── event sink              (BufferEventSink)          │
│  ├── storage                 (InMemoryStorage)          │
│  ├── config                  (Config::for_harness)      │
│  └── services                (all stubbed)              │
│                                                         │
│  try_run_sampling_request(sess, streamer, handler)      │
│       │                          │            │         │
│       │                   ┌──────┘     ┌──────┘         │
│       ▼                   ▼            ▼                │
│  Session             ModelStreamer  ToolCallHandler      │
│  (state + stubs)     (trait)       (trait)               │
└───────────────────────┬───────────────┬─────────────────┘
                        │               │
                  ┌─────▼─────┐   ┌─────▼─────┐
                  │ Activity  │   │ Activity  │
                  │model_call │   │ tool_exec │
                  │           │   │           │
                  │ModelClient│   │shell/patch│
                  │(codex)    │   │/read/grep │
                  └───────────┘   └───────────┘
```

### What runs where

| Responsibility | Codex (single process) | Temporal |
|---|---|---|
| Conversation history | `Session.state` | `Session::new_minimal()` in workflow |
| Prompt building | `Session` methods | Same `Session` methods in workflow |
| Model calls | `Session.services.model_client` | `model_call` activity (uses `ModelClient`) |
| Tool execution | `Session.services` (shell, sandbox) | `tool_exec` activity (subprocess/file I/O) |
| Event delivery | `Session.event_sink` (channel) | `BufferEventSink` + query polling |
| Persistence | `Session.services.rollout` (files) | Temporal workflow history (built-in) |
| Fault tolerance | None (process dies = state lost) | Workflow replay (automatic) |
| Auth | `Session.services.auth_manager` | Worker env var (`OPENAI_API_KEY`) |
| Entropy | System random + clock | Deterministic `TemporalRandomSource` / `TemporalClock` |

## Codex-Core Extension Points (already done)

No `SessionCore` extraction needed. The following traits/abstractions are already in place:

1. **`EventSink` trait** — pluggable event delivery (`ChannelEventSink` / `BufferEventSink`)
2. **`StorageBackend` trait** — pluggable rollout persistence (`RolloutFileStorage` / `InMemoryStorage`)
3. **`ModelStreamer` trait** — pluggable model calls (generic on `try_run_sampling_request`)
4. **`ToolCallHandler` trait** — pluggable tool execution (generic on `try_run_sampling_request`)
5. **`EntropyProviders`** — pluggable random/clock via task-local `ENTROPY`
6. **`AgentSession` trait** — pluggable agent backend for TUI
7. **`Session::new_minimal()`** — zero-service constructor for external harnesses
8. **`Config::for_harness()`** — zero-I/O config constructor

## Codex-Temporal Implementation

### Workflow

- Uses `Session::new_minimal()` to hold conversation state
- Approval decisions via `ExecApprovalRequest` signal + `wait_condition`

### Activities

- `model_call` — uses codex `ModelClient` / `ModelClientSession`
- `dispatch_tool` — builds a `ToolRouter` from `ToolsConfig` and dispatches any tool call through the standard codex routing pipeline (`ToolRouter::dispatch()`), so all built-in tools (shell, apply_patch, read_file, list_dir, grep, etc.) are supported without per-tool handler code

## MCP Server Management

MCP server connections are managed by `HarnessMcpManager` (in `mcp.rs`), which holds persistent `RmcpClient` connections to user-configured MCP servers. It supports tool discovery (returning qualified tool names like `mcp__server__tool`) and tool execution within activities. Elicitation requests from MCP servers are captured and surfaced to the workflow for approval.

## Future: Worker-Level State for Persistent Processes

Some codex features require **long-lived processes** that outlive individual activity calls: PTY sessions, JS REPL kernels. Activities are stateless one-shot functions — they can't hold a subprocess or connection open between invocations.

The planned solution is **worker-level state**: the worker process would own persistent resources (PTY sessions, JS REPL kernels, sandbox config), and activities would access them by reference. This would require sticky task routing — assigning each workflow to a specific worker — which is the natural model for a coding agent since the worker runs on the machine with the code.

This is not yet implemented. Currently, MCP connections are the only persistent resource, managed at the activity level by `HarnessMcpManager`.
