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

## Benefits

1. **No dead fields** — workflow constructs exactly what it needs
2. **Type-safe separation** — compiler enforces that workflow code can't access services
3. **Testable** — `SessionCore` can be unit tested without Temporal or heavy services
4. **Codex stays clean** — `Session` wraps `SessionCore` via `Deref`, existing code unchanged
5. **Feature parity path** — as codex adds state to `SessionCore`, Temporal gets it automatically
6. **No duplication** — approval logic, history management, prompt building shared between codex and Temporal
