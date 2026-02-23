# Continue-As-New & Generic Op Signal Design

## Overview

Two related changes:

1. **Generic `receive_op` signal** — replace the three dedicated signals
   (`receive_user_turn`, `receive_approval`, `request_shutdown`) with a single
   `receive_op(Op)` signal. This makes adding new Op support trivial — just
   forward the Op from the client and handle it in the workflow dispatcher.

2. **Continue-as-new (CAN)** — reset Temporal event history for long-running
   sessions by continuing as a new workflow run under the same workflow ID,
   carrying conversation state as input arguments.

### Design Decisions

| Decision | Choice |
|----------|--------|
| **Signal architecture** | Single generic `receive_op(Op)` signal |
| **CAN trigger** | `continue_as_new_suggested` (mandatory) + after `/compact` |
| **History carry** | Full carry in CAN arguments (external storage later) |
| **Client handling** | Transparent — client uses workflow ID without run ID |
| **Summarization** | Mechanical (codex-core compaction) now, LLM later |
| **Compact event** | Blocking — emit `ContextCompactedEvent` before CAN |

---

## 1. Generic `receive_op` Signal

### Current state (3 dedicated signals)

```rust
#[signal] fn receive_user_turn(&mut self, ..., input: UserTurnInput)
#[signal] fn receive_approval(&mut self, ..., input: ApprovalInput)
#[signal] fn request_shutdown(&mut self, ...)
```

Each new Op requires: a new signal method, a new payload type, and session-side
wiring. Adding `/compact` would mean a 4th signal, and future Ops (undo,
review, etc.) would each add more.

### New state (1 generic signal)

```rust
#[signal]
pub fn receive_op(&mut self, _ctx: &mut SyncWorkflowContext<Self>, op: Op) {
    match op {
        Op::UserTurn { items, effort, summary, personality, .. } => {
            let message = extract_message(&items);
            self.user_turns.push(UserTurnInput {
                turn_id: format!("turn-{}", self.turn_counter),
                message,
                effort: effort.map(Into::into),
                summary: summary.into(),
                personality,
            });
            self.turn_counter += 1;
        }
        Op::ExecApproval { id, decision, .. } => {
            if let Some(ref mut pa) = self.pending_approval {
                if pa.call_id == id {
                    let approved = matches!(
                        decision,
                        ReviewDecision::Approved
                            | ReviewDecision::ApprovedForSession
                            | ReviewDecision::ApprovedExecpolicyAmendment { .. }
                    );
                    pa.decision = Some(approved);
                }
            }
        }
        Op::Shutdown => {
            self.shutdown_requested = true;
        }
        Op::Compact => {
            self.compact_requested = true;
        }
        _ => {
            // Unknown/unhandled Op — ignore.
        }
    }
}
```

`Op` is already `Serialize + Deserialize` (serde-tagged enum), so it works
directly as a signal payload.

### Workflow struct

```rust
#[workflow]
pub struct CodexWorkflow {
    input: CodexWorkflowInput,
    pub(crate) events: Arc<BufferEventSink>,
    user_turns: Vec<UserTurnInput>,
    turn_counter: u32,                           // moved from run() locals
    pub(crate) pending_approval: Option<PendingApproval>,
    shutdown_requested: bool,
    compact_requested: bool,                     // NEW
}
```

Note: `turn_counter` moves into the struct so the signal handler can generate
turn IDs. Previously the counter lived as a local in `run()` and turn IDs came
from the session side.

### Session-side simplification

```rust
impl codex_core::AgentSession for TemporalAgentSession {
    async fn submit(&self, op: Op) -> CodexResult<String> {
        // First UserTurn starts the workflow.
        if let Op::UserTurn { .. } = &op {
            let started = *self.started.lock().expect("lock poisoned");
            if !started {
                return self.start_workflow(&op).await;
            }
        }

        // All other Ops: forward as generic signal.
        self.signal_op(op).await
    }
}

impl TemporalAgentSession {
    async fn signal_op(&self, op: Op) -> CodexResult<String> {
        let handle = self.client
            .get_workflow_handle::<CodexWorkflowRun>(&self.workflow_id);
        handle
            .signal(CodexWorkflow::receive_op, op, WorkflowSignalOptions::default())
            .await
            .map_err(|e| CodexErr::Fatal(format!("failed to signal op: {e}")))?;
        Ok("ok".to_string())
    }
}
```

The big match statement in `submit()` is replaced by: check if first UserTurn
→ start workflow, otherwise → forward Op. No per-variant wiring needed.

The `start_workflow` method still embeds the first user message in
`CodexWorkflowInput` (so the workflow starts processing immediately without
waiting for a signal).

### Backward compatibility

The old dedicated signal methods are removed. No external clients depend on
specific signal names — the `TemporalAgentSession` is the only caller.

---

## 2. CAN Triggers

### 2a. `continue_as_new_suggested` (server-driven)

The Temporal server sets `continue_as_new_suggested = true` on workflow task
started events when the event history grows too large. The SDK exposes this via
`ctx.continue_as_new_suggested()`.

**Check point:** After each turn completes (after emitting `TurnComplete`),
before waiting for the next turn.

### 2b. After `/compact` (user-driven)

When the user invokes `/compact`, the workflow runs compaction (via codex-core
Session) and then triggers CAN. This is the natural CAN point because:

1. The conversation history has just been shrunk — ideal for serializing as CAN
   arguments.
2. Many Temporal events have likely accumulated — good time to reset.

**Flow:** `Op::Compact` signal → workflow wakes → run compaction → emit
`ContextCompactedEvent` → CAN.

---

## 3. State Carried Across CAN Boundary

### `ContinueAsNewState` struct

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContinueAsNewState {
    /// Accumulated conversation history (RolloutItems from InMemoryStorage).
    pub rollout_items: Vec<RolloutItem>,

    /// Queued user turns not yet processed.
    pub pending_user_turns: Vec<UserTurnInput>,

    /// Cumulative turn count across all CAN runs.
    pub cumulative_turn_count: u32,

    /// Cumulative model->tool loop iteration count.
    pub cumulative_iterations: u32,

    /// Cumulative token usage across all CAN runs.
    pub cumulative_token_usage: Option<TokenUsage>,
}
```

### Embedding in workflow input

```rust
pub struct CodexWorkflowInput {
    // ... existing fields ...

    /// State from a previous continue-as-new execution.
    /// `None` on the first run, `Some(...)` on subsequent runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continued_state: Option<ContinueAsNewState>,
}
```

The same `CodexWorkflowInput` type is reused — the first run has
`continued_state: None`, and CAN runs have `Some(state)`.

### What is NOT carried

- **Event buffer** (`BufferEventSink`) — starts fresh each run. The client
  resets its watermark naturally.
- **Pending approval** — CAN only happens at turn boundaries, so there is no
  pending approval.
- **Shutdown flag** — starts `false` in new run.
- **Entropy seed** — Temporal provides a new seed per run.

---

## 4. Workflow Changes

### 4a. Initialization — restore from `ContinueAsNewState`

In `new()` (the `#[init]` method), if `input.continued_state` is `Some(state)`:

1. **Restore user turn queue** — set `user_turns = state.pending_user_turns`.
2. **Restore turn counter** — set `turn_counter = state.cumulative_turn_count`.

In `run()`, if `input.continued_state` is `Some(state)`:

1. **Replay rollout items** into the `InMemoryStorage` — call
   `storage.save(&state.rollout_items)`.
2. **Reconstruct Session history** — use
   `sess.reconstruct_history_from_rollout()` (or equivalent) to rebuild the
   in-memory conversation from the rollout items.
3. **Restore counters** — set `total_iterations = state.cumulative_iterations`.

### 4b. Main loop with CAN and compact checks

```
loop {
    // Wait for work.
    wait_condition(|s| {
        !s.user_turns.is_empty() || s.shutdown_requested || s.compact_requested
    });

    // --- compact check (highest priority — exits workflow) ---
    if compact_requested {
        run_compact(&sess, &turn_context);
        emit ContextCompactedEvent;
        clear compact_requested;
        return do_continue_as_new(...);
    }

    // --- shutdown check ---
    if shutdown_requested && user_turns.is_empty() {
        break;
    }

    // --- process next turn ---
    dequeue turn;
    emit TurnStarted;
    run agentic loop;
    emit TurnComplete;

    // --- CAN check at turn boundary ---
    if ctx.continue_as_new_suggested() {
        return do_continue_as_new(...);
    }
}
```

### 4c. `do_continue_as_new` helper

```rust
fn do_continue_as_new(
    ctx: &WorkflowContext<CodexWorkflow>,
    input: &CodexWorkflowInput,
    storage: &InMemoryStorage,
    pending_turns: Vec<UserTurnInput>,
    total_iterations: u32,
    turn_counter: u32,
    token_usage: Option<TokenUsage>,
) -> WorkflowResult<CodexWorkflowOutput> {
    let state = ContinueAsNewState {
        rollout_items: storage.items(),
        pending_user_turns: pending_turns,
        cumulative_turn_count: turn_counter,
        cumulative_iterations: total_iterations,
        cumulative_token_usage: token_usage,
    };

    let can_input = CodexWorkflowInput {
        continued_state: Some(state),
        ..input.clone()
    };

    Err(WorkflowTermination::continue_as_new(
        ContinueAsNewWorkflowExecution {
            arguments: vec![can_input.as_json_payload()?],
            ..Default::default()
        },
    ))
}
```

---

## 5. Compaction Implementation

### How compaction runs in the workflow

The codex-core `Session` already supports compaction through its internal
machinery. The model call during compaction goes through `TemporalModelStreamer`,
which dispatches it as a `model_call` activity. The compaction flow:

1. Session gets current history.
2. Session calls the model to summarize (via `TemporalModelStreamer` →
   `model_call` activity).
3. Session replaces in-memory history with compacted version.
4. Session persists `RolloutItem::Compacted` to `InMemoryStorage`.

We need to determine whether `Session::spawn_task(CompactTask)` works in the
Temporal workflow context, or whether we need to call the inline auto-compact
path (`run_inline_auto_compact_task` / `run_inline_remote_auto_compact_task`).

**Preferred approach:** Use the inline auto-compact functions from codex-core,
which run compaction synchronously (within the calling async context) rather
than spawning a background task. This is compatible with Temporal's deterministic
execution model.

---

## 6. Client Transparency

### Why no client changes are needed for CAN

The `TemporalAgentSession` uses **workflow ID without run ID** for all
operations:

```rust
self.client.get_workflow_handle::<CodexWorkflowRun>(&self.workflow_id)
```

Temporal routes signals and queries sent to a workflow ID (without run ID) to
the **current (latest) run**. After CAN:

- The old run completes with `ContinuedAsNew` status.
- A new run starts immediately with the same workflow ID.
- Subsequent signals and queries automatically target the new run.

### Watermark reset

After CAN, the new run's `BufferEventSink` starts empty. The client's
`events_index` holds a stale watermark from the old run.

When the client queries `get_events_since(old_watermark)` on the new run, the
new run's event buffer has fewer events than `old_watermark`, so
`events_since()` returns `(empty, current_len)` — effectively resetting the
watermark.

This is safe: the client sees no events briefly, then picks up new events from
the new run. No events are lost because:

1. The workflow emits `ContextCompactedEvent` **before** CAN (client sees it).
2. The first events in the new run are from the next turn.

### Timing gap

Between the old run completing and the new run's first query response, there's
a brief window where queries may fail (workflow not yet started). The client's
`poll_events` already handles query errors with backoff retry
(`session.rs:358-369`), so this is handled naturally.

---

## 7. Session History Reconstruction

When the new workflow run starts with `continued_state: Some(state)`:

1. **Create `InMemoryStorage`** and pre-populate it:
   ```rust
   let storage = InMemoryStorage::new();
   storage.save(&state.rollout_items).await;
   ```

2. **Create `Session` via `new_minimal()`** as usual.

3. **Replay history into Session** — call a method that reconstructs
   conversation context from the rollout items. This is analogous to
   `reconstruct_history_from_rollout()` in codex-core. The Session processes
   `RolloutItem::ResponseItem` (regular messages) and
   `RolloutItem::Compacted` (compacted summaries) to rebuild the in-memory
   conversation state.

4. **Resume normal operation** — the workflow enters its main loop and
   processes queued turns.

### Note on payload size

Temporal's default payload size limit is ~2 MB. For very long conversations
(even after compaction), the serialized `ContinueAsNewState` could approach
this limit. This design uses full carry for simplicity. A future enhancement
will offload large state to external storage (e.g., write to blob store via
activity, carry only a reference in CAN arguments).

---

## 8. Testing

### E2E test: compact triggers CAN

1. Create a session, submit a user turn, wait for `TurnComplete`.
2. Submit `Op::Compact`.
3. Wait for `ContextCompactedEvent`.
4. After CAN, submit another user turn.
5. Wait for `TurnComplete` — verify the agent responds correctly (conversation
   context was preserved across the CAN boundary).

### E2E test: generic signal

1. Verify `Op::UserTurn`, `Op::ExecApproval`, `Op::Shutdown` still work
   through the generic signal path (existing tests should pass after refactor).

### E2E test: continue_as_new_suggested

This is harder to test in e2e because it requires the server to set the flag.
Options:
- Unit test with mocked workflow context.
- Integration test with a Temporal server configuration that sets the threshold
  very low.

### Unit tests

- `ContinueAsNewState` serialization round-trip.
- `CodexWorkflowInput` with `continued_state` serialization.
- History reconstruction from rollout items.

---

## 9. Implementation Order

1. **Refactor to generic `receive_op` signal** — workflow.rs, session.rs
   - Replace 3 dedicated signals with `receive_op(Op)`
   - Move `turn_counter` into workflow struct
   - Move approval/shutdown dispatch into signal handler
   - Simplify session `submit()` to forward Ops
   - Verify all existing tests pass

2. **Add `ContinueAsNewState` and update `CodexWorkflowInput`** — types.rs

3. **Add `compact_requested` handling** — workflow.rs
   - Add flag to struct, handle `Op::Compact` in `receive_op`
   - Update wait condition

4. **Implement history restoration on workflow start** — workflow.rs

5. **Implement `do_continue_as_new` helper** — workflow.rs

6. **Add CAN check at turn boundary** (`continue_as_new_suggested`) —
   workflow.rs

7. **Implement compact → CAN flow** — workflow.rs

8. **Add unit tests** — unit_tests.rs

9. **Add e2e test** (compact → CAN → conversation continuity) — e2e_test.rs

10. **Fix all warnings** — `cargo check --tests`

---

## 10. Future Work

- **External storage for large state** — offload rollout items to blob store
  before CAN, carry only a reference.
- **LLM-based summarization** — run a model call to produce an intelligent
  summary before CAN instead of mechanical truncation.
- **Auto-compaction integration** — detect when codex-core's Session runs
  auto-compaction (token limit triggered) and use that as a CAN trigger too.
- **More Op support** — as new Ops are needed (Undo, Review, etc.), just add
  a match arm in `receive_op` — no new signals required.
