# Codex-Temporal Design: SessionCore Refactoring

## Problem

Codex's `Session` is a monolithic "god object" that owns everything needed to run the agent in a single process: conversation state, model client, tool execution, MCP connections, auth, analytics, sandbox, file watching, network proxy, agent lifecycle, and more.

The Temporal harness only needs the **state management** portion. Currently `Session::new_minimal()` constructs a full `Session` with most fields stubbed — this works but is conceptually wrong (a Session that isn't really a Session) and carries 24 dead fields in the workflow.

## Solution: Composition via SessionCore

Split `Session` into a base (`SessionCore`) and a full variant (`Session`) using Rust composition:

```
SessionCore                          Session
├── conversation_id                  ├── core: SessionCore
├── config                           └── services: SessionServices
├── history                              ├── model_client
├── event_sink (dyn EventSink)           ├── mcp_connection_manager
├── storage (dyn StorageBackend)         ├── auth_manager
└── approval_store                       ├── unified_exec_manager
                                         ├── zsh_exec_bridge
                                         ├── user_shell
                                         ├── exec_policy
                                         ├── analytics
                                         ├── hooks
                                         ├── rollout
                                         ├── file_watcher
                                         ├── agent_control
                                         ├── network_proxy
                                         ├── skills_manager
                                         ├── models_manager
                                         ├── otel_manager
                                         └── ... (24 fields total)
```

### SessionCore — the "base class"

Contains only what the agentic loop (`try_run_sampling_request`) needs:

```rust
pub struct SessionCore {
    pub conversation_id: ThreadId,
    pub config: Arc<Config>,
    history: Mutex<Vec<ResponseItem>>,
    event_sink: Arc<dyn EventSink>,
    storage: Arc<dyn StorageBackend>,
    approval_store: Mutex<ApprovalStore>,
}

impl SessionCore {
    // Conversation state
    pub async fn record_items(&self, ctx: &TurnContext, items: &[ResponseItem]);
    pub async fn history_items(&self) -> Vec<ResponseItem>;

    // Approval logic (deterministic, no I/O)
    pub fn check_tool_approval(&self, command: &[String], policy: AskForApproval) -> ExecApprovalRequirement;
    pub fn record_tool_approval(&self, key: &str, decision: ReviewDecision);
}
```

### Session — the "subclass"

Wraps `SessionCore` and adds heavy services for the full codex CLI:

```rust
pub struct Session {
    pub core: SessionCore,
    pub(crate) services: SessionServices,
}

impl Deref for Session {
    type Target = SessionCore;
    fn deref(&self) -> &SessionCore { &self.core }
}
```

Full codex constructs `Session` (which contains `SessionCore`). Temporal workflow constructs `SessionCore` directly — no stubs, no defaults, no dead fields.

## How It Maps to Temporal

Temporal decomposes `Session`'s responsibilities across its own primitives. `SessionCore` runs in the workflow; `SessionServices` functionality is replaced by activities:

```
┌─────────────────────────────────────────────────────────┐
│ Temporal Workflow (deterministic)                       │
│                                                         │
│  SessionCore                                            │
│  ├── conversation history    (workflow state)            │
│  ├── approval store          (workflow state)            │
│  ├── event sink              (BufferEventSink)           │
│  ├── config                  (workflow input)            │
│  └── approval logic          (deterministic checks)     │
│                                                         │
│  try_run_sampling_request(sess_core, streamer, handler)  │
│       │                          │            │          │
│       │                   ┌──────┘     ┌──────┘          │
│       ▼                   ▼            ▼                 │
│  SessionCore        ModelStreamer  ToolCallHandler        │
│  (state only)       (trait)       (trait)                │
└───────────────────────┬───────────────┬──────────────────┘
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
| Conversation history | `Session.history` | `SessionCore` in workflow state |
| Prompt building | `Session` methods | `SessionCore` in workflow |
| Approval decisions | `Session.services.tool_approvals` | `SessionCore.approval_store` in workflow |
| Command safety | `Session.services.exec_policy` | `is_known_safe_command()` in workflow |
| Model calls | `Session.services.model_client` | `model_call` activity (uses `ModelClient`) |
| Tool execution | `Session.services` (shell, sandbox) | `tool_exec` activity (subprocess/file I/O) |
| Event delivery | `Session.event_sink` channel | `BufferEventSink` + query polling |
| Persistence | `Session.services.rollout` files | Temporal workflow history (built-in) |
| Fault tolerance | None (process dies = state lost) | Workflow replay (automatic) |
| Auth | `Session.services.auth_manager` | Worker env var (`OPENAI_API_KEY`) |

## Codex-Core Changes Required

### 1. Extract SessionCore (~100 lines moved)

Move from `Session` to `SessionCore`:
- `conversation_id`, `config`, history state, `event_sink`, `storage`
- `record_items()`, `history_items()` methods
- Add `approval_store` + approval check methods

### 2. Update try_run_sampling_request signature

```rust
// Before:
pub async fn try_run_sampling_request<M, T>(sess: Arc<Session>, ...)
// After:
pub async fn try_run_sampling_request<M, T>(sess: Arc<SessionCore>, ...)
```

### 3. Export approval types (4 items)

```rust
// In lib.rs:
pub use tools::sandboxing::{ApprovalStore, ExecApprovalRequirement, default_exec_approval_requirement};
pub use exec_policy::render_decision_for_unmatched_command;
```

### 4. Update codex callers

Full codex passes `Arc::clone(&session.core)` (or relies on `Deref`) to `try_run_sampling_request`. No behavior change — just passing the inner type.

## Codex-Temporal Changes

### Workflow

- Construct `SessionCore` directly (no `Session::new_minimal()`)
- Use `sess_core.check_tool_approval()` before dispatching tool activities
- Only emit `ExecApprovalRequest` signal when policy requires user input
- Cache approval decisions in `sess_core.approval_store`

### Activities

- `model_call` — already uses `ModelClient` (done)
- `tool_exec` — add handlers for `apply_patch`, `read_file`, `list_dir`, `grep_files`
- Each tool handler is straightforward I/O (~10-150 lines each)

## Worker-Level State for Persistent Processes

Some codex features require **long-lived processes** that outlive individual activity calls: PTY sessions, JS REPL kernels, MCP server connections. Activities are stateless one-shot functions — they can't hold a subprocess or connection open between invocations.

The solution is **worker-level state**: the worker process owns persistent resources, and activities access them by reference.

```
┌──────────────────────────────────────────────────────┐
│ Worker Process (long-lived)                          │
│                                                      │
│  WorkerResources (shared across all activities)      │
│  ├── mcp_connections: HashMap<String, McpConnection> │
│  ├── pty_sessions: HashMap<String, PtySession>       │
│  ├── js_repl: Option<JsReplKernel>                   │
│  └── sandbox_config: SandboxConfig                   │
│                                                      │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐  │
│  │ Activity    │  │ Activity    │  │ Activity    │  │
│  │ tool_exec   │  │ model_call  │  │ tool_exec   │  │
│  │  accesses   │  │             │  │  accesses   │  │
│  │  PTY #3     │  │  ModelClient│  │  MCP "git"  │  │
│  └─────────────┘  └─────────────┘  └─────────────┘  │
└──────────────────────────────────────────────────────┘
```

### How it works

1. **Worker startup**: The worker process creates `WorkerResources` — starts MCP servers, initializes sandbox config, prepares PTY/REPL infrastructure.

2. **Activity access**: Activities receive a reference to `WorkerResources` via the activity context. They look up or create persistent resources by key (e.g., workflow ID → PTY session).

3. **Lifecycle**: Resources are tied to the worker process lifetime. When the worker restarts, resources are re-created. The workflow is unaffected — it replays and activities re-establish connections.

### What this enables

| Feature | Worker resource | Activity usage |
|---|---|---|
| **MCP tools** | `McpConnection` per server | Activity sends request, gets response |
| **JS REPL** | `JsReplKernel` per workflow | Activity evaluates code, returns result |
| **PTY / unified exec** | `PtySession` per terminal | Activity writes command, reads output |
| **Background terminals** | Multiple `PtySession`s | `/ps` queries workflow state, execution via activities |
| **Sandbox (bubblewrap)** | `SandboxConfig` | Activity wraps subprocess in bubblewrap |
| **Network proxy** | `ManagedProxy` | Activity routes requests through proxy |

### Trade-off: Sticky task routing

Worker-level state means a workflow's activities must run on the **same worker** that holds its resources. This requires sticky task routing — assigning each workflow to a specific worker via a workflow-specific task queue or session-based routing.

This breaks the "any worker can handle any task" assumption, but is the correct trade-off for a coding agent:
- The worker runs on the **machine with the code** (filesystem access required)
- Coding agents are inherently single-machine (you edit files on one machine)
- The worker IS the machine — sticky routing is the natural model

For multi-machine deployments, each machine runs its own worker with its own task queue. The TUI connects to the workflow, which routes activities to the right worker.

### Implementation

```rust
/// Shared state held by the worker process.
pub struct WorkerResources {
    mcp: Mutex<HashMap<String, McpConnection>>,
    pty: Mutex<HashMap<String, PtySession>>,
    js_repl: Mutex<Option<JsReplKernel>>,
    sandbox: SandboxConfig,
}

/// Activities access worker resources via the activity struct.
pub struct CodexActivities {
    resources: Arc<WorkerResources>,
}

#[activities]
impl CodexActivities {
    #[activity]
    pub async fn tool_exec(&self, ctx: ActivityContext, input: ToolExecInput)
        -> Result<ToolExecOutput, ActivityError>
    {
        match input.tool_name.as_str() {
            "shell" => self.run_shell(&input).await,
            "apply_patch" => self.apply_patch(&input).await,
            "js_repl" => {
                // Access persistent kernel from worker resources
                let mut repl = self.resources.js_repl.lock().await;
                let kernel = repl.get_or_insert_with(|| JsReplKernel::new());
                kernel.eval(&input.arguments).await
            }
            name if self.resources.is_mcp_tool(name) => {
                // Route to persistent MCP connection
                let conn = self.resources.mcp_connection(name).await?;
                conn.call_tool(&input.arguments).await
            }
            _ => Err(anyhow::anyhow!("unknown tool: {}", input.tool_name).into()),
        }
    }
}
```

## Benefits

1. **No dead fields** — workflow constructs exactly what it needs
2. **Type-safe separation** — compiler enforces that workflow code can't access services
3. **Testable** — `SessionCore` can be unit tested without Temporal or heavy services
4. **Codex stays clean** — `Session` wraps `SessionCore` via `Deref`, existing code unchanged
5. **Feature parity path** — as codex adds state to `SessionCore`, Temporal gets it automatically
6. **No duplication** — approval logic, history management, prompt building shared between codex and Temporal
7. **Persistent processes** — worker-level state enables MCP, PTY, JS REPL, sandbox without architectural mismatch
8. **Natural deployment** — sticky routing matches the reality that coding agents work on a single machine
