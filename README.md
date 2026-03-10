# codex-temporal

Temporal durable execution harness for the [OpenAI Codex CLI](https://github.com/openai/codex) agent.

Wraps the Codex agentic loop in a Temporal workflow so that multi-turn conversations, tool calls, and approval flows survive process failures and can be replayed deterministically.

## Features

- **Multi-turn conversations** with signal/query protocol — user turns, approvals, interrupts, context overrides
- **Full tool ecosystem** via codex-core's ToolRegistry — shell, apply_patch, read_file, list_dir, grep_files, view_image, web search, request_user_input
- **Tool approval gating** with three-tier command safety classification (safe/dangerous/unknown) and policy modes (Never, OnRequest, OnFailure, UnlessTrusted)
- **MCP server support** — stdio and streamable HTTP transports, tool discovery, elicitation capture and resolution
- **Dynamic tools** — client-defined tools via signal/wait
- **Patch approval flow** — dedicated apply_patch approval events with patch text
- **Session resume** via long-lived `CodexHarness` registry workflow with interactive TUI picker
- **Continue-as-new** with rolling event buffer, watermark continuity, and full state carry-forward
- **Project context injection** — AGENTS.md docs and git info injected into model prompts
- **Config.toml loading** via activity — real sandbox policy, features, MCP servers from user config
- **Token usage tracking** and prompt cache metrics
- **Reasoning effort/summary/personality** — workflow-level defaults with per-turn overrides
- **Multi-agent support** — session workflow with crew types and subagent scoping
- **Activity-level security** — worker token authentication, input validation, config-driven sandbox policy
- **Full TUI reuse** via `codex_tui::run_with_session()` — same ChatWidget as the standard Codex CLI

## Prerequisites

- **Rust** (1.85+, edition 2024)
- **Temporal CLI** (`temporal server start-dev`) or a running Temporal server
- **Protocol Buffers compiler** (`protoc`) — required by the Temporal SDK
- **OpenAI API key** (`OPENAI_API_KEY` env var)
- **Sibling repos** (only needed for local development — see [Local development](#local-development) below):

```
parent/
  codex/           # github.com/openai/codex   (branch: task/codex-temporal)
  sdk-core/        # github.com/mfateev/sdk-core (branch: master)
  codex-temporal/  # this repo
```

By default, all dependencies are fetched from git. You only need sibling checkouts if you want to iterate on codex crates locally.

## Quick start

### 1. Start a local Temporal server

```bash
temporal server start-dev
```

This starts a dev server on `localhost:7233` with a web UI at `http://localhost:8233`.

### 2. Start the worker

The worker runs workflows (`AgentWorkflow`, `CodexHarness`, `SessionWorkflow`) and activities (model calls, tool execution, MCP, config loading) on the `codex-temporal` task queue.

```bash
export OPENAI_API_KEY="sk-..."
cargo run --bin codex-temporal-worker
```

### 3. Run a one-shot workflow

```bash
cargo run --bin codex-temporal-client -- "What is 2+2? Reply with just the number."
```

For tool-use prompts:

```bash
cargo run --bin codex-temporal-client -- "Use shell to run 'echo hello world' and tell me the output."
```

List running sessions:

```bash
cargo run --bin codex-temporal-client -- list
```

### 4. Run the TUI

The TUI binary reuses the Codex ChatWidget but connects to a Temporal workflow instead of an in-process agent. The worker holds the API key and makes LLM calls.

```bash
cargo run --bin codex-temporal-tui -- "Explain what Temporal is in two sentences."
```

Launch without an initial prompt for interactive use:

```bash
cargo run --bin codex-temporal-tui
```

Resume a previous session:

```bash
cargo run --bin codex-temporal-tui -- --resume           # interactive picker
cargo run --bin codex-temporal-tui -- --resume <session_id>  # direct resume
```

### 5. Configuration

#### Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `OPENAI_API_KEY` | (required) | OpenAI API key for model calls |
| `OPENAI_BASE_URL` | `https://api.openai.com/v1` | Base URL for the OpenAI API |
| `CODEX_MODEL` | `gpt-4o` | Model slug passed to the Responses API |
| `CODEX_APPROVAL_POLICY` | `on-request` | Tool approval mode: `never`, `untrusted`, `on-failure`, `on-request` |
| `CODEX_WEB_SEARCH` | (disabled) | Web search mode: `live` or `cached` |
| `CODEX_EFFORT` | (model default) | Reasoning effort: `low`, `medium`, `high` |
| `CODEX_REASONING_SUMMARY` | (default) | Reasoning summary mode |
| `CODEX_PERSONALITY` | (default) | Agent personality override |
| `TEMPORAL_ADDRESS` | `http://localhost:7233` | Temporal server gRPC endpoint |
| `RUST_LOG` | `info` | Tracing filter (e.g. `codex_temporal=debug`) |

#### config.toml

The worker loads config from `~/.codex/config.toml` (same format as the standard Codex CLI). All settings — model, sandbox policy, approval policy, MCP servers, features — flow through to the workflow. Environment variables override config.toml values.

## Building binaries

### Build release binaries

```bash
cargo build --release
```

This produces three binaries in `target/release/`:

| Binary | Description |
|--------|-------------|
| `codex-temporal-worker` | Temporal worker — runs workflows and activities |
| `codex-temporal-client` | CLI client — start workflows, list sessions |
| `codex-temporal-tui` | TUI — interactive Codex interface over Temporal |

### Install to PATH

```bash
cargo install --path .
```

This installs all three binaries to `~/.cargo/bin/` (which should already be on your `PATH` if you have Rust installed via rustup).

### Using the installed binaries

Once installed (or using the binaries from `target/release/` directly), you no longer need `cargo run`:

```bash
# Start the worker
export OPENAI_API_KEY="sk-..."
codex-temporal-worker

# Run a one-shot prompt
codex-temporal-client "What is 2+2?"

# List sessions
codex-temporal-client list

# Launch the TUI
codex-temporal-tui

# Resume a session
codex-temporal-tui --resume
```

### Cross-compilation

To build for a different target (e.g. `x86_64-unknown-linux-musl` for a static binary):

```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

The binary will be at `target/x86_64-unknown-linux-musl/release/codex-temporal-worker` (and similarly for the other binaries).

All dependencies (codex crates, Temporal SDK) are fetched from git — no sibling repo checkouts required for building.

### Local development

For iterating on codex crates locally, create `.cargo/config.toml` to override the git dependencies with local paths:

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

## Tests

### Unit tests (106 tests)

```bash
cargo test --lib --test unit_tests
```

Covers: entropy, event sink (rolling buffer, eviction, CAN snapshot), storage, types serde roundtrips (all I/O types, backward compat), signal payloads, session/context constructors, safety classification, config loading, MCP types, picker conversions, tool dispatch (shell, read_file, list_dir, sandbox policy, input validation).

### E2E tests (28 test cases)

E2E tests spin up an ephemeral Temporal server, create an in-process worker, and exercise the full workflow against the real OpenAI API.

```bash
OPENAI_API_KEY="sk-..." cargo test --test e2e_test -- --nocapture
```

**Requires `OPENAI_API_KEY`** — tests fail if the key is not set.

Key test cases include: model-only turns, tool approval flow, multi-turn conversation, model selection, web search, prompt caching, reasoning effort, continue-as-new, session resume, project context injection, config loading, sandbox policy enforcement, command safety classification, MCP tool calls, policy amendment, interrupt, request_user_input, patch approval, dynamic tools, override turn context, MCP elicitation, session workflow with crew types and subagent spawning.

### TUI E2E tests

```bash
OPENAI_API_KEY="sk-..." cargo test --test tui_e2e_test -- --nocapture
```

Verifies the full ChatWidget → `wire_session` → `TemporalAgentSession` → Temporal workflow round-trip.

## Architecture

```
codex-temporal/src/
  lib.rs              Module declarations
  types.rs            Serializable I/O types, signal payloads, harness types
  entropy.rs          Deterministic RandomSource backed by workflow context
  sink.rs             BufferEventSink — rolling event buffer with indexed polling and CAN snapshots
  storage.rs          InMemoryStorage (in-memory StorageBackend)
  streamer.rs         ModelStreamer impl dispatching to model_call activity
  tools.rs            ToolCallHandler impl — three-tier safety, approval gating, MCP/dynamic routing
  config_loader.rs    Config loading — load_harness_config, apply_env_overrides, config_from_toml
  mcp.rs              HarnessMcpManager — persistent MCP server connections, tool discovery + execution
  activities.rs       Activities — model_call, tool_exec, load_config, collect_project_context,
                        discover_mcp_tools, mcp_tool_call, get_worker_token, check_credentials,
                        resolve_role_config
  workflow.rs         AgentWorkflow — multi-turn interactive workflow with signals/queries,
                        project context, MCP tools, approval, interrupt, CAN
  harness.rs          CodexHarness — long-lived per-user session registry workflow
  session_workflow.rs SessionWorkflow — multi-agent sessions with crew types and subagent scoping
  picker.rs           TUI picker integration — session-to-thread conversion, ID extraction
  session.rs          TemporalAgentSession — AgentSession impl with resume support
  bin/
    worker.rs         Temporal worker binary — runs all workflows + activities
    client.rs         CLI — start workflows, list sessions via harness
    tui.rs            TUI binary — full Codex TUI via codex_tui::run_with_session()
```

### Protocol

The workflow uses a generic `receive_op` signal that dispatches all client operations:

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

| Query | Purpose |
|-------|---------|
| `get_events_since(index)` | Return new events since the given watermark (for client polling) |
| `get_events` | Return all events from index 0 (convenience) |

`TemporalAgentSession` implements the `AgentSession` trait by mapping `submit(Op)` to signals and `next_event()` to query polling with adaptive backoff.

### Workflow execution flow

```
Client                    Temporal                     Worker
  |                         |                            |
  |-- start_workflow ------>|                            |
  |                         |-- WorkflowTask ----------->|
  |                         |                            |-- get_worker_token activity
  |                         |                            |-- load_config activity
  |                         |                            |-- collect_project_context activity
  |                         |                            |-- discover_mcp_tools activity
  |                         |                            |-- model_call activity
  |                         |<-- ScheduleActivity -------|
  |                         |-- ActivityTask ----------->|
  |                         |<-- ActivityResult ---------|  (model response)
  |                         |-- WorkflowTask ----------->|
  |                         |                            |-- emit ExecApprovalRequest
  |<-- query events --------|                            |   (if tool call needs approval)
  |-- signal approval ----->|                            |
  |                         |-- WorkflowTask ----------->|
  |                         |                            |-- tool_exec activity (token verified)
  |                         |<-- ScheduleActivity -------|
  |                         |-- ActivityTask ----------->|
  |                         |<-- ActivityResult ---------|  (tool output)
  |                         |-- WorkflowTask ----------->|
  |                         |                            |-- model_call activity
  |                         |                            |   (model sees tool result)
  |                         |                            |-- emit TurnComplete
  |<-- query events --------|                            |
```

## Security

- **Sandbox policy** from user's `config.toml` is enforced in tool activities (not hardcoded)
- **Worker token authentication** prevents unauthorized `tool_exec` dispatch from rogue workflows on shared task queues
- **Input validation** in `dispatch_tool()` — cwd must exist and be a directory, tool_name must be non-empty
- **Three-tier command safety** classification at the workflow level (safe auto-approve, dangerous always prompt, unknown defers to policy)
- **Approval handled at workflow level** before activity dispatch — activities run with `AskForApproval::Never` by design

## License

MIT
