# codex-temporal

Temporal durable execution harness for the [OpenAI Codex CLI](https://github.com/openai/codex) agent.

Wraps the Codex agentic loop in a Temporal workflow so that multi-turn conversations, tool calls, and approval flows survive process failures and can be replayed deterministically.

## Prerequisites

- **Rust** (1.85+, edition 2024)
- **Temporal CLI** (`temporal server start-dev`) or a running Temporal server
- **OpenAI API key** (`OPENAI_API_KEY` env var)
- **Sibling repos** checked out next to this one:

```
parent/
  codex/           # github.com/openai/codex   (branch: task/codex-temporal)
  sdk-core/        # github.com/mfateev/sdk-core (branch: fix/wait-condition-wakers)
  codex-temporal/  # this repo                  (branch: task/temporal-harness)
```

## Quick start

### 1. Start a local Temporal server

```bash
temporal server start-dev
```

This starts a dev server on `localhost:7233` with a web UI at `http://localhost:8233`.

### 2. Start the worker

The worker runs both the `CodexWorkflow` and the `CodexActivities` (model calls, shell execution) on the `codex-temporal` task queue.

```bash
export OPENAI_API_KEY="sk-..."

# Optional: override model (default: gpt-4o)
# export CODEX_MODEL="gpt-4o-mini"

# Optional: override Temporal address (default: http://localhost:7233)
# export TEMPORAL_ADDRESS="http://localhost:7233"

cargo run --bin codex-temporal-worker
```

### 3. Run a one-shot workflow

The client binary starts a single workflow and exits. Monitor progress in the Temporal UI.

```bash
cargo run --bin codex-temporal-client -- "What is 2+2? Reply with just the number."
```

For tool-use prompts:

```bash
cargo run --bin codex-temporal-client -- "Use shell to run 'echo hello world' and tell me the output."
```

### 4. Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `OPENAI_API_KEY` | (required) | OpenAI API key for model calls |
| `OPENAI_BASE_URL` | `https://api.openai.com/v1` | Base URL for the OpenAI API |
| `CODEX_MODEL` | `gpt-4o` | Model slug passed to the Responses API |
| `TEMPORAL_ADDRESS` | `http://localhost:7233` | Temporal server gRPC endpoint |
| `RUST_LOG` | `info` | Tracing filter (e.g. `codex_temporal=debug`) |

## Tests

### Unit tests

```bash
cargo test --test unit_tests
```

18 tests covering entropy, clock, event sink, storage, types, signal payloads, and session/context constructors.

### E2E tests

E2E tests spin up an ephemeral Temporal server (auto-downloads the CLI binary), create an in-process worker, and exercise the full `TemporalAgentSession` flow against the real OpenAI API.

```bash
OPENAI_API_KEY="sk-..." cargo test --test e2e_test -- --nocapture
```

Tests are skipped gracefully when `OPENAI_API_KEY` is not set.

Two test cases run sequentially:

| Test | What it does |
|------|-------------|
| `model_only_turn` | Submits a simple prompt, asserts `TurnStarted` and `TurnComplete` with a non-empty agent message |
| `tool_approval_flow` | Submits a prompt that triggers a shell tool call, waits for `ExecApprovalRequest`, sends approval, asserts `TurnComplete` after tool execution |

## Architecture

```
codex-temporal/src/
  lib.rs              Module declarations
  types.rs            Serializable I/O types and signal payloads
  entropy.rs          Deterministic RandomSource/Clock backed by workflow context
  sink.rs             BufferEventSink (in-memory EventSink with indexed polling)
  storage.rs          InMemoryStorage (in-memory StorageBackend)
  streamer.rs         ModelStreamer impl dispatching to model_call activity
  tools.rs            ToolCallHandler impl with approval gating + tool_exec activity
  activities.rs       Activity definitions (model_call, tool_exec)
  workflow.rs         CodexWorkflow (multi-turn interactive workflow)
  session.rs          TemporalAgentSession (AgentSession impl for client-side use)
  bin/
    worker.rs         Temporal worker binary
    client.rs         CLI to start a one-shot workflow
```

### Protocol

The workflow exposes a signal/query interface for multi-turn interaction:

| Signal | Purpose |
|--------|---------|
| `receive_user_turn` | Queue a new user message for processing |
| `receive_approval` | Approve or deny a pending tool call |
| `request_shutdown` | Gracefully terminate after the current turn |

| Query | Purpose |
|-------|---------|
| `get_events_since(index)` | Return new events since the given watermark (for client polling) |

`TemporalAgentSession` implements the `AgentSession` trait by mapping `submit(Op)` to signals and `next_event()` to query polling with adaptive backoff.

### Workflow execution flow

```
Client                    Temporal                     Worker
  |                         |                            |
  |-- start_workflow ------>|                            |
  |                         |-- WorkflowTask ----------->|
  |                         |                            |-- model_call activity
  |                         |<-- ScheduleActivity -------|
  |                         |-- ActivityTask ----------->|
  |                         |<-- ActivityResult ---------|  (model response)
  |                         |-- WorkflowTask ----------->|
  |                         |                            |-- emit ExecApprovalRequest
  |<-- query events --------|                            |   (if tool call)
  |-- signal approval ----->|                            |
  |                         |-- WorkflowTask ----------->|
  |                         |                            |-- tool_exec activity
  |                         |<-- ScheduleActivity -------|
  |                         |-- ActivityTask ----------->|
  |                         |<-- ActivityResult ---------|  (tool output)
  |                         |-- WorkflowTask ----------->|
  |                         |                            |-- model_call activity
  |                         |                            |   (model sees tool result)
  |                         |                            |-- emit TurnComplete
  |<-- query events --------|                            |
```

## License

MIT
