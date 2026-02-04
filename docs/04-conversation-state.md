# Conversation State

## Overview

Codex maintains conversation state across turns, including history, token tracking, and automatic compaction when the context window fills up. State can be persisted to disk and resumed later.

## Key Components

| Component | File | Purpose |
|-----------|------|---------|
| `ContextManager` | `context_manager/history.rs:22` | In-memory conversation history |
| `SessionState` | `state/session.rs:15` | Per-session mutable state |
| `RolloutRecorder` | `rollout/recorder.rs:59` | Persists state to JSONL files |
| Token tracking | `context_manager/history.rs:244` | Tracks token usage |
| Compaction | `compact.rs` | Reduces history size |

## ContextManager (Conversation History)

The `ContextManager` holds the in-memory conversation transcript:

```rust
struct ContextManager {
    /// Items ordered oldest → newest
    items: Vec<ResponseItem>,
    /// Token usage information
    token_info: Option<TokenUsageInfo>,
}

impl ContextManager {
    /// Record new items into history
    fn record_items(&mut self, items: I, policy: TruncationPolicy);

    /// Get history formatted for model prompt
    fn for_prompt(self) -> Vec<ResponseItem>;

    /// Get raw items (includes ghost snapshots)
    fn raw_items(&self) -> &[ResponseItem];

    /// Estimate token count from history
    fn estimate_token_count(&self, turn_context: &TurnContext) -> Option<i64>;

    /// Get total token usage (for auto-compact check)
    fn get_total_token_usage(&self, server_reasoning_included: bool) -> i64;

    /// Replace entire history (after compaction)
    fn replace(&mut self, items: Vec<ResponseItem>);
}
```

### Item Types in History

```rust
enum ResponseItem {
    Message { role, content, ... },      // User/assistant messages
    FunctionCall { call_id, name, arguments, ... },
    FunctionCallOutput { call_id, output },
    CustomToolCall { call_id, name, input },
    CustomToolCallOutput { call_id, output },
    LocalShellCall { call_id, params },
    Reasoning { encrypted_content, ... },  // Extended thinking
    Compaction { encrypted_content },       // Compacted context marker
    GhostSnapshot { ... },                  // For undo support
    WebSearchCall { ... },
    Other,
}
```

### History Normalization

Before sending to the model, history is normalized:

```rust
fn normalize_history(&mut self) {
    // 1. Ensure every call has a corresponding output
    normalize::ensure_call_outputs_present(&mut self.items);

    // 2. Remove orphan outputs without corresponding calls
    normalize::remove_orphan_outputs(&mut self.items);
}
```

This handles cases like interrupted tool calls.

## Token Tracking

### TokenUsageInfo

Tracks token consumption across the conversation:

```rust
struct TokenUsageInfo {
    /// Most recent token usage from API
    last_token_usage: TokenUsage,
    /// Model's context window size
    context_window: Option<i64>,
}

struct TokenUsage {
    input_tokens: i64,
    output_tokens: i64,
    total_tokens: i64,
}
```

### Token Estimation

For reasoning tokens and history size estimation:

```rust
impl ContextManager {
    fn estimate_token_count(&self, turn_context: &TurnContext) -> Option<i64> {
        // Base instructions tokens
        let base_tokens = approx_token_count(&base_instructions);

        // Sum over history items
        let items_tokens = self.items.iter().fold(0i64, |acc, item| {
            acc + match item {
                ResponseItem::Reasoning { encrypted_content, .. } => {
                    // Estimate from encrypted content length
                    approx_tokens_from_byte_count(estimate_reasoning_length(len))
                }
                item => {
                    // JSON serialization heuristic
                    approx_token_count(&serde_json::to_string(item))
                }
            }
        });

        Some(base_tokens + items_tokens)
    }
}
```

## Auto-Compaction

### Trigger Condition

Compaction triggers when token usage exceeds a threshold:

```rust
// In run_turn()
let auto_compact_limit = model_info.auto_compact_token_limit();
let total_usage = sess.get_total_token_usage().await;

if total_usage >= auto_compact_limit {
    run_auto_compact(&sess, &turn_context).await;
}
```

The limit is typically ~80% of the model's context window.

### Compaction Strategies

#### 1. Local (Inline) Compaction

Uses the model itself to summarize the conversation:

```rust
async fn run_inline_auto_compact_task(sess, turn_context) {
    // 1. Get the compaction prompt
    let prompt = turn_context.compact_prompt().to_string();

    // 2. Call the model to summarize
    drain_to_completed(&sess, &turn_context, &prompt).await;

    // 3. Extract summary from model response
    let summary_text = get_last_assistant_message_from_turn(history_items);

    // 4. Collect user messages to preserve
    let user_messages = collect_user_messages(history_items);

    // 5. Build new compacted history
    let new_history = build_compacted_history(
        initial_context,
        &user_messages,
        &summary_text,
    );

    // 6. Replace history
    sess.replace_history(new_history).await;
    sess.recompute_token_usage(&turn_context).await;
}
```

#### 2. Remote Compaction

OpenAI-specific API endpoint for compaction:

```rust
async fn run_remote_compact_task_inner_impl(sess, turn_context) {
    let history = sess.clone_history().await;

    // Call OpenAI's compact endpoint
    let new_history = turn_context.client
        .compact_conversation_history(&prompt)
        .await?;

    sess.replace_history(new_history).await;
    sess.recompute_token_usage(&turn_context).await;
}
```

### Compacted History Structure

After compaction, history contains:

```rust
fn build_compacted_history(
    initial_context: Vec<ResponseItem>,
    user_messages: &[String],
    summary_text: &str,
) -> Vec<ResponseItem> {
    let mut history = initial_context;

    // Add preserved user messages (most recent, token-limited)
    for message in selected_messages {
        history.push(ResponseItem::Message {
            role: "user",
            content: message,
        });
    }

    // Add summary as final "user" message
    history.push(ResponseItem::Message {
        role: "user",
        content: format!("{SUMMARY_PREFIX}\n{summary_text}"),
    });

    history
}
```

The `SUMMARY_PREFIX` marks the summary so future compactions can identify it.

## Session State Persistence

### SessionState

Wraps all mutable per-session state:

```rust
struct SessionState {
    session_configuration: SessionConfiguration,
    history: ContextManager,
    latest_rate_limits: Option<RateLimitSnapshot>,
    server_reasoning_included: bool,
    dependency_env: HashMap<String, String>,
    mcp_dependency_prompted: HashSet<String>,
    initial_context_seeded: bool,
}
```

### RolloutRecorder

Persists state to JSONL files for resumption:

```rust
struct RolloutRecorder {
    tx: Sender<RolloutCmd>,           // Channel to writer task
    rollout_path: PathBuf,            // ~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl
    state_db: Option<StateDbHandle>,  // SQLite index (optional)
}
```

### Rollout File Format

JSONL with timestamped lines:

```jsonl
{"timestamp":"2025-01-15T10:30:00.123Z","session_meta":{...}}
{"timestamp":"2025-01-15T10:30:01.456Z","response_item":{"role":"user","content":[...]}}
{"timestamp":"2025-01-15T10:30:05.789Z","response_item":{"role":"assistant","content":[...]}}
{"timestamp":"2025-01-15T10:30:10.012Z","turn_context":{...}}
{"timestamp":"2025-01-15T10:31:00.345Z","compacted":{"message":"...","replacement_history":null}}
```

### RolloutItem Types

```rust
enum RolloutItem {
    SessionMeta(SessionMetaLine),  // Session metadata + git info
    ResponseItem(ResponseItem),     // Conversation items
    Compacted(CompactedItem),       // Compaction marker
    TurnContext(TurnContextItem),   // Per-turn configuration
    EventMsg(EventMsg),             // Events (for debugging)
}
```

### Session Resume Flow

```rust
impl RolloutRecorder {
    async fn get_rollout_history(path: &Path) -> Result<InitialHistory> {
        // 1. Load all items from JSONL file
        let (items, thread_id, _) = Self::load_rollout_items(path).await?;

        // 2. Return as ResumedHistory
        Ok(InitialHistory::Resumed(ResumedHistory {
            conversation_id: thread_id,
            history: items,
            rollout_path: path.to_path_buf(),
        }))
    }
}

// During session init:
match initial_history {
    InitialHistory::New => {
        // Start fresh
    }
    InitialHistory::Resumed(resumed) => {
        // Replay items into ContextManager
        for item in resumed.history {
            match item {
                RolloutItem::ResponseItem(ri) => history.record_items(&[ri], policy),
                RolloutItem::Compacted(c) => {
                    if let Some(replacement) = c.replacement_history {
                        history.replace(replacement);
                    }
                }
                // ...
            }
        }
    }
}
```

## Message History (Global)

Separate from conversation state, there's a global message history file:

```rust
// ~/.codex/history.jsonl
struct HistoryEntry {
    session_id: String,   // Thread ID
    ts: u64,              // Unix timestamp
    text: String,         // Message content
}
```

This provides:
- Cross-session message lookup
- History search functionality
- Auto-trimming when exceeding `max_bytes`

## Truncation Policies

Tool outputs are truncated to prevent context overflow:

```rust
enum TruncationPolicy {
    None,
    Tokens(usize),        // Max tokens
    Characters(usize),    // Max characters
}

fn truncate_text(text: &str, policy: TruncationPolicy) -> String {
    match policy {
        TruncationPolicy::None => text.to_string(),
        TruncationPolicy::Tokens(max) => {
            // Approximate truncation based on token estimate
            truncate_to_tokens(text, max)
        }
        TruncationPolicy::Characters(max) => {
            // Direct character truncation
            text.chars().take(max).collect()
        }
    }
}
```

## Ghost Snapshots (Undo Support)

Ghost snapshots enable undo functionality:

```rust
ResponseItem::GhostSnapshot {
    snapshot_id: String,
    // ... serialized state
}
```

- Stored in history but filtered out before sending to model
- Preserved through compaction
- Enable rolling back to previous states

## Architecture Diagram

```
┌──────────────────────────────────────────────────────────────────┐
│                         Session                                   │
│                                                                   │
│  ┌──────────────────────────────────────────────────────────┐    │
│  │  SessionState                                             │    │
│  │                                                           │    │
│  │  ┌─────────────────────────────────────────────────────┐ │    │
│  │  │  ContextManager (history)                            │ │    │
│  │  │                                                      │ │    │
│  │  │  items: [ResponseItem, ...]  ←─── record_items()     │ │    │
│  │  │  token_info: TokenUsageInfo                          │ │    │
│  │  │                                                      │ │    │
│  │  │  for_prompt() ───────────────────→ Vec<ResponseItem> │ │    │
│  │  └─────────────────────────────────────────────────────┘ │    │
│  │                                                           │    │
│  │  rate_limits, server_reasoning_included, ...              │    │
│  └──────────────────────────────────────────────────────────┘    │
│                              │                                    │
│                              │ persist                            │
│                              ▼                                    │
│  ┌──────────────────────────────────────────────────────────┐    │
│  │  RolloutRecorder                                          │    │
│  │                                                           │    │
│  │  tx ──→ [RolloutCmd] ──→ Writer Task ──→ rollout.jsonl    │    │
│  │                                                           │    │
│  └──────────────────────────────────────────────────────────┘    │
│                                                                   │
└──────────────────────────────────────────────────────────────────┘
                              │
                              │ load_rollout_items()
                              ▼
┌──────────────────────────────────────────────────────────────────┐
│  ~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl                     │
│                                                                   │
│  Line 1: {"session_meta": {...}}                                  │
│  Line 2: {"response_item": {"role":"user",...}}                   │
│  Line 3: {"response_item": {"role":"assistant",...}}              │
│  Line N: {"compacted": {...}}                                     │
│                                                                   │
└──────────────────────────────────────────────────────────────────┘
```

## Deeper Investigation Pointers

| Topic | Where to Look |
|-------|---------------|
| History normalization | `context_manager/normalize.rs` |
| Token estimation | `truncate.rs` |
| Compaction prompts | `templates/compact/prompt.md` |
| Rollout file discovery | `rollout/list.rs` |
| Session metadata | `rollout/metadata.rs` |
| State database (SQLite) | `state_db.rs` |
| Undo implementation | `tasks/undo.rs` |
