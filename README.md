# codex-temporal

Temporal durable execution harness for the [OpenAI Codex CLI](https://github.com/openai/codex) agent.

Wraps the Codex agentic loop in a Temporal workflow so that multi-turn conversations, tool calls, and approval flows survive process failures and can be replayed deterministically.

## Prerequisites

- **Rust** (1.85+, edition 2024)
- **Temporal CLI** (`temporal server start-dev`) or a running Temporal server
- **Protocol Buffers compiler** (`protoc`) — required by the Temporal SDK
- **OpenAI API key** (`OPENAI_API_KEY` env var)

## Codex dependency

This project depends on a modified fork of [openai/codex](https://github.com/openai/codex) on the **`task/codex-temporal`** branch. That branch adds abstraction traits (`AgentSession`, `ModelStreamer`, `ToolCallHandler`, `EventSink`, `StorageBackend`, etc.) and minimal constructors that let an external orchestrator drive the Codex agent loop.

See [`CODEX_TEMPORAL_CHANGES.md`](https://github.com/mfateev/codex/blob/task/codex-temporal/CODEX_TEMPORAL_CHANGES.md) in the codex repo for the full list of changes.

By default, all codex crates are fetched from git. For local iteration, clone the codex repo on the `task/codex-temporal` branch next to this repo and create `.cargo/config.toml`:

```bash
git clone -b task/codex-temporal https://github.com/openai/codex.git ../codex
```

```toml
[patch."https://github.com/mfateev/codex.git"]
codex-core          = { path = "../codex/codex-rs/core" }
codex-feedback      = { path = "../codex/codex-rs/feedback" }
codex-protocol      = { path = "../codex/codex-rs/protocol" }
codex-otel          = { path = "../codex/codex-rs/otel" }
codex-tui           = { path = "../codex/codex-rs/tui" }
codex-shell-command = { path = "../codex/codex-rs/shell-command" }
codex-apply-patch   = { path = "../codex/codex-rs/apply-patch" }
codex-rmcp-client   = { path = "../codex/codex-rs/rmcp-client" }
```

This file is gitignored. Remove or rename it to build from remote git sources.

## Quick start

### 1. Start a local Temporal server

```bash
temporal server start-dev
```

This starts a dev server on `localhost:7233` with a web UI at `http://localhost:8233`.

### 2. Start the worker

```bash
export OPENAI_API_KEY="sk-..."
cargo run --bin codex-temporal-worker
```

The worker runs workflows (`AgentWorkflow`, `CodexHarness`, `SessionWorkflow`) and activities (model calls, tool execution, MCP, config loading) on the `codex-temporal` task queue.

### 3. Run the TUI

```bash
cargo run --bin codex-temporal-tui
```

Resume a previous session:

```bash
cargo run --bin codex-temporal-tui -- --resume           # interactive picker
cargo run --bin codex-temporal-tui -- --resume <session_id>  # direct resume
```

### Configuration

| Variable | Default | Description |
|----------|---------|-------------|
| `TEMPORAL_ADDRESS` | `http://localhost:7233` | Temporal server gRPC endpoint |
| `RUST_LOG` | `info` | Tracing filter (e.g. `codex_temporal=debug`) |

All standard Codex environment variables (`OPENAI_API_KEY`, `CODEX_MODEL`, `CODEX_APPROVAL_POLICY`, etc.) and `~/.codex/config.toml` settings are supported — see the [Codex CLI docs](https://github.com/openai/codex) for details.

## Building

```bash
cargo build --release
```

Produces two binaries in `target/release/`:

| Binary | Description |
|--------|-------------|
| `codex-temporal-worker` | Temporal worker — runs workflows and activities |
| `codex-temporal-tui` | TUI — interactive Codex interface over Temporal |

## Tests

### Unit tests

```bash
cargo test --lib --test unit_tests
```

### E2E tests

Spins up an ephemeral Temporal server, creates an in-process worker, and exercises the full workflow against the real OpenAI API.

```bash
OPENAI_API_KEY="sk-..." cargo test --test e2e_test -- --nocapture
```

### TUI E2E tests

```bash
OPENAI_API_KEY="sk-..." cargo test --test tui_e2e_test -- --nocapture
```

## Architecture

```
src/
  lib.rs              Module declarations
  types.rs            Serializable I/O types, signal payloads, harness types
  entropy.rs          Deterministic RandomSource backed by workflow context
  sink.rs             BufferEventSink — rolling event buffer with watermark-based reads and CAN snapshots
  storage.rs          InMemoryStorage (in-memory StorageBackend)
  streamer.rs         ModelStreamer impl dispatching to model_call activity
  tools.rs            ToolCallHandler impl — safety classification, approval gating, MCP/dynamic routing
  config_loader.rs    Config loading — load_harness_config, apply_env_overrides, config_from_toml
  mcp.rs              HarnessMcpManager — persistent MCP server connections, tool discovery + execution
  activities.rs       Activities — model_call, tool_exec, load_config, collect_project_context,
                        discover_mcp_tools, mcp_tool_call, get_worker_token, check_credentials,
                        resolve_role_config
  workflow.rs         AgentWorkflow — multi-turn workflow with signals/updates, approval, interrupt, CAN
  harness.rs          CodexHarness — long-lived per-user session registry workflow
  session_workflow.rs SessionWorkflow — multi-agent sessions with crew types and subagent scoping
  picker.rs           TUI picker integration — session-to-thread conversion, ID extraction
  session.rs          TemporalAgentSession — AgentSession impl with resume support
  bin/
    worker.rs         Temporal worker binary
    tui.rs            TUI binary — Codex ChatWidget over Temporal via codex_tui::run_with_session()
```

### Workflow types

The system defines three workflow types, registered on a shared `codex-temporal` task queue:

```
CodexHarness (per-user session registry)
  │
  │  register / list / get
  ▼
SessionWorkflow (per-session parent)
  │
  │  spawns child workflows
  ├──────────────┬──────────────┐
  ▼              ▼              ▼
AgentWorkflow  AgentWorkflow  AgentWorkflow
 (main)         (role A)       (role B)
```

**CodexHarness** (`src/harness.rs`) — A long-lived, per-user workflow (`codex-harness-<user>`) that acts as a session registry. It stores a list of `SessionEntry` records and exposes `register_session` / `update_session_status` / `remove_session` signals and `list_sessions` / `get_session` queries. It has no activities of its own and uses continue-as-new to keep its history bounded. The harness also performs a one-time `check_credentials` activity to verify the worker has API keys.

**SessionWorkflow** (`src/session_workflow.rs`) — A per-session parent workflow (`codex-session-<uuid>`) that loads shared state once — merged config, project context, and MCP tool schemas — then spawns and tracks child `AgentWorkflow` instances. It always starts a "main" agent and accepts `spawn_agent` signals to create additional agents with role-based configuration (including crew agent definitions). A `max_agents` limit (default 8) is enforced. The parent close policy is `Terminate`, so shutting down the session terminates all its agents.

**AgentWorkflow** (`src/workflow.rs`) — The core workflow that drives the Codex agentic loop. Each instance runs a deterministic model→tool cycle: call the model, execute approved tools, feed results back, repeat until the turn is complete. It supports multi-turn conversations via `UserTurn` signals, tool/patch approval gating, MCP elicitation, dynamic tool calls, interruption, and mid-workflow overrides (model, approval policy, effort, personality). State is streamed to clients through a `BufferEventSink` with watermark-based reads exposed via a `get_state_update` blocking update. The workflow uses continue-as-new (triggered by a `Compact` signal) to carry forward full conversation state when history grows large.

**Relationships:**
- CodexHarness ↔ SessionWorkflow — The harness tracks sessions but does not parent them; they are independent workflows linked by signals/queries.
- SessionWorkflow → AgentWorkflow — True parent-child. The session pre-resolves config and context, passes it to children via input, and terminates children on close.
- AgentWorkflow ↔ AgentWorkflow — Siblings share no direct link. Multi-agent coordination flows through the parent session.

### Protocol

The workflow uses a `receive_op` signal that dispatches all client operations:

| Op variant | Purpose |
|------------|---------|
| `UserTurn` | Queue a new user message for processing |
| `ExecApproval` | Approve or deny a pending tool call |
| `PatchApproval` | Approve or deny a pending apply_patch call |
| `UserInputAnswer` | Respond to a request_user_input tool call |
| `ResolveElicitation` | Respond to an MCP elicitation request |
| `DynamicToolResponse` | Return output for a client-defined dynamic tool call |
| `OverrideTurnContext` | Update approval policy, model, effort, summary, or personality mid-workflow |
| `Compact` | Trigger continue-as-new with full state carry-forward |
| `Interrupt` | Cancel the current turn |
| `Shutdown` | Gracefully terminate after the current turn |

| Blocking update | Purpose |
|-----------------|---------|
| `get_state_update(since_index)` | Block until new events are available, then return them with an updated watermark |

`TemporalAgentSession` implements the `AgentSession` trait by mapping `submit(Op)` to signals and `next_event()` to a background watcher that long-polls via the `get_state_update` blocking update.

### Workflow execution flow

```
TUI
 │
 │  start session
 ▼
CodexHarness
 │  register_session signal
 │
SessionWorkflow
 │  1. load_config activity
 │  2. collect_project_context activity
 │  3. discover_mcp_tools activity
 │  4. spawn main AgentWorkflow (passes pre-loaded config/context/tools)
 │  5. on spawn_agent signal → resolve_role_config activity → spawn additional AgentWorkflow
 │
AgentWorkflow
 │  (if not pre-resolved by parent: load_config, collect_project_context, discover_mcp_tools)
 │  1. wait for UserTurn signal
 │  2. model_call activity              ←─┐
 │  3. if tool needs approval:            │
 │  │    emit ExecApprovalRequest         │
 │  │    wait for ExecApproval signal     │
 │  4. tool_exec activity                 │  agentic
 │  │   (or mcp_tool_call activity)       │  loop
 │  5. feed tool result back to model  ───┘
 │  6. emit TurnComplete
 │  7. goto 1 (next turn) or shutdown
 │
 │  on Compact signal → continue-as-new with full state
 │
TUI ◄── get_state_update blocking update (streams events back)
```

## License

MIT
