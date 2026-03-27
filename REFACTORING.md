# Refactoring Opportunities

## 1. `tools.rs` — repeated "wait for signal resolution" pattern

`handle_tool_call` (~650 lines) repeats the same state machine **5 times** across MCP elicitation, `request_user_input`, dynamic tools, exec approval, and patch approval:

```
set pending state → emit event → wait_condition(resolved || interrupted) → check interrupt → extract response → clear pending
```

Each instance is 20-30 lines of boilerplate. A generic helper would eliminate ~100 lines:

```rust
async fn wait_for_resolution<T>(
    ctx: &WorkflowContext<AgentWorkflow>,
    get_resolved: impl Fn(&AgentWorkflow) -> Option<T>,
    clear: impl FnOnce(&mut AgentWorkflow),
) -> Option<T>  // None = interrupted
```

The "execute activity with cancellation + error mapping" pattern also repeats 3 times (MCP call, patch exec, shell exec) and could be a small helper.

## 2. `session.rs` — duplicated watcher event loop

The `match result { WatcherEvent::Events(..) | Completed | Error(..) }` block with stall-detection bookkeeping is **copy-pasted verbatim** between `start_watching()` (lines 252-289) and the retry-spawn path inside `submit()` (lines 616-650). Extract to:

```rust
async fn drain_watcher_into_buffer(
    rx: &mut mpsc::Receiver<WatcherEvent>,
    buffer: &Arc<Mutex<Vec<Event>>>,
    notify: &Arc<Notify>,
    generation: &Arc<Mutex<u64>>,
    gen_at_start: u64,
)
```

## 3. Duplicate `extract_message`

Identical function exists in both `workflow.rs` and `session.rs`. Move to a shared location (e.g. `types.rs`).
