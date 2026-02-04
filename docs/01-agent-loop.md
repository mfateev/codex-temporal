# Agent Loop

## Overview

The agent loop is the core execution engine that processes user input, calls the model, executes tools, and repeats until the task is complete.

## Key Functions

| Function | File | Purpose |
|----------|------|---------|
| `run_turn` | `codex.rs:3267` | Orchestrates a complete turn |
| `run_sampling_request` | `codex.rs:3600` | Single model API call + tool execution |
| `try_run_sampling_request` | `codex.rs:4142` | Actual streaming and processing |
| `handle_output_item_done` | `stream_events_utils.rs:45` | Process each output item |

## Turn Lifecycle

A **turn** represents one user input through to agent completion:

```
User Input
    │
    ▼
┌─────────────────────────────────────────────────────────────┐
│  run_turn()                                                  │
│                                                              │
│  1. Check token usage → maybe trigger auto-compaction        │
│  2. Load skills for current working directory                │
│  3. Collect MCP tools                                        │
│  4. Detect skill mentions in user input                      │
│  5. Inject skill prompts into context                        │
│  6. Record user prompt in history                            │
│                                                              │
│  ┌────────────────────────────────────────────────────────┐  │
│  │  INNER LOOP: run_sampling_request()                    │  │
│  │                                                        │  │
│  │  • Call model API                                      │  │
│  │  • Process streaming response                          │  │
│  │  • Execute tool calls                                  │  │
│  │  • Check if follow-up needed                           │  │
│  │  • If follow-up: continue loop                         │  │
│  │  • If done: break                                      │  │
│  │                                                        │  │
│  │  Also: check token limit, run compaction if needed     │  │
│  └────────────────────────────────────────────────────────┘  │
│                                                              │
│  7. Emit turn complete notification                          │
│                                                              │
└─────────────────────────────────────────────────────────────┘
    │
    ▼
Final Response
```

## Sampling Request Flow

Each `run_sampling_request` does:

```
┌─────────────────────────────────────────────────────────────┐
│  run_sampling_request()                                      │
│                                                              │
│  1. List available MCP tools                                 │
│  2. Build ToolRouter with all tools                          │
│  3. Construct Prompt { input, tools, instructions }          │
│  4. Call try_run_sampling_request()                          │
│                                                              │
│  Retry Logic:                                                │
│  • On retryable error → exponential backoff                  │
│  • On persistent failure → switch transport (WS → HTTPS)     │
│  • Max retries per provider configuration                    │
│                                                              │
└─────────────────────────────────────────────────────────────┘
```

## Streaming Response Processing

The model response is a stream of events:

```rust
enum ResponseEvent {
    Created,                    // Response started
    OutputItemAdded(item),      // New item (message, tool call) started
    OutputTextDelta(delta),     // Text chunk for streaming display
    OutputItemDone(item),       // Item completed
    ReasoningSummaryDelta,      // Reasoning content (for extended thinking)
    RateLimits(snapshot),       // Rate limit info
    Completed { token_usage },  // Response finished
}
```

Processing loop:

```
while let Some(event) = stream.next() {
    match event {
        OutputItemAdded → start tracking active item
        OutputTextDelta → emit to UI for streaming display
        OutputItemDone → {
            if tool_call → queue tool execution, set needs_follow_up
            if message → record in history
        }
        Completed → break with result
    }
}
```

## Tool Execution

When a tool call is detected:

1. **Build ToolCall** from response item (`ToolRouter::build_tool_call`)
2. **Queue execution** as async future (`tool_runtime.handle_tool_call`)
3. **Collect results** after stream completes
4. **Add to history** as function call output
5. **Set needs_follow_up = true** to continue the loop

Tool calls are processed in parallel using `FuturesOrdered`:

```rust
let mut in_flight: FuturesOrdered<BoxFuture<ResponseInputItem>> = FuturesOrdered::new();

// During stream processing:
if let Some(tool_call) = detect_tool_call(item) {
    let future = tool_runtime.handle_tool_call(tool_call, cancel_token);
    in_flight.push_back(Box::pin(future));
}

// After stream completes:
while let Some(result) = in_flight.next().await {
    history.push(result);  // Tool output added to conversation
}
```

## Loop Termination Conditions

The agent loop ends when:

| Condition | Result |
|-----------|--------|
| Model returns only message (no tools) | **Success** - final response |
| `needs_follow_up = false` | **Success** - turn complete |
| Cancellation token triggered | **Aborted** - user interrupted |
| Max retries exceeded | **Error** - API failure |
| Context window exceeded | **Error** - need compaction |
| Non-retryable error | **Error** - fatal |

## Context Management

### Token Tracking

```rust
let auto_compact_limit = model_info.auto_compact_token_limit();
let total_usage = sess.get_total_token_usage().await;

if total_usage >= auto_compact_limit {
    run_auto_compact(&sess, &turn_context).await;
}
```

### Auto-Compaction Trigger

Compaction runs automatically when:
- Token usage exceeds model's threshold
- Typically ~80% of context window
- Happens between sampling requests, not mid-stream

## Cancellation

Cancellation is cooperative via `CancellationToken`:

```rust
// Check cancellation at key points:
let event = stream.next()
    .or_cancel(&cancellation_token)
    .await;

match event {
    Err(Cancelled) => break Err(CodexErr::TurnAborted),
    Ok(event) => process(event),
}
```

Cancellation triggers:
- User interrupt (`Op::Interrupt`)
- Timeout
- Parent scope cancelled

## Events Emitted

During a turn, these events are sent to observers:

| Event | When |
|-------|------|
| `TurnStarted` | Turn begins |
| `AgentMessageContentDelta` | Streaming text chunk |
| `ReasoningContentDelta` | Streaming reasoning |
| `ItemStarted` | Output item begins |
| `ItemCompleted` | Output item done |
| `ToolCallStarted` | Tool execution begins |
| `ToolCallCompleted` | Tool execution done |
| `TokenCount` | After each sampling request |
| `TurnCompleted` | Turn finished |

## Error Handling

### Retryable Errors

```rust
if err.is_retryable() {
    retries += 1;
    let delay = backoff(retries);  // Exponential backoff
    tokio::time::sleep(delay).await;
    continue;
}
```

Retryable conditions:
- Network errors
- 5xx server errors
- 429 rate limit errors
- Stream disconnection

### Transport Fallback

If WebSocket fails repeatedly, fall back to HTTPS:

```rust
if retries >= max_retries && client_session.try_switch_fallback_transport() {
    // Reset retry counter, try with HTTPS
    retries = 0;
    continue;
}
```

## Key Data Structures

### TurnContext

Per-turn configuration snapshot:

```rust
struct TurnContext {
    sub_id: String,              // Turn identifier
    cwd: PathBuf,                // Working directory
    client: ModelClient,          // Model client
    collaboration_mode: Mode,     // Plan/Execute/etc
    approval_policy: Policy,      // Tool approval rules
    sandbox_policy: Policy,       // Execution sandbox
    tools_config: ToolsConfig,    // Available tools
    dynamic_tools: Vec<Tool>,     // Runtime-registered tools
    // ...
}
```

### SamplingRequestResult

```rust
struct SamplingRequestResult {
    needs_follow_up: bool,         // Continue loop?
    last_agent_message: Option<String>,  // Final text
}
```

### OutputItemResult

```rust
struct OutputItemResult {
    last_agent_message: Option<String>,
    needs_follow_up: bool,
    tool_future: Option<InFlightFuture>,  // Queued tool execution
}
```

## Deeper Investigation Pointers

| Topic | Where to Look |
|-------|---------------|
| Prompt construction | `codex.rs` - `Prompt` struct creation |
| Tool routing logic | `tools/router.rs` |
| Parallel tool execution | `tools/parallel.rs` |
| History management | `conversation_history.rs` |
| Event emission | `Session::send_event` |
| Retry/backoff logic | `run_sampling_request` retry loop |
| Stream event handling | `try_run_sampling_request` match arms |
