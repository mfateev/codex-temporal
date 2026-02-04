# Model Integration

## Overview

Codex supports multiple AI model providers through an abstraction layer that handles different API formats, authentication methods, and transport protocols.

## Key Components

| Component | File | Purpose |
|-----------|------|---------|
| `ModelClient` | `client.rs:91` | Immutable client configuration |
| `ModelClientSession` | `client.rs:95` | Per-turn mutable session state |
| `ModelProviderInfo` | `model_provider_info.rs:55` | Provider configuration |
| `WireApi` | `model_provider_info.rs:43` | API protocol enum |
| `Prompt` | `client_common.rs:26` | Request payload structure |
| `ResponseEvent` | `codex_api` | Streaming event types |

## Wire API Protocols

Codex supports two API protocols:

### Responses API (`WireApi::Responses`)

- OpenAI's modern API at `/v1/responses`
- Native support for tool calls, streaming, reasoning
- Supports WebSocket transport for lower latency
- Used by: OpenAI, Azure OpenAI (responses endpoint)

### Chat Completions API (`WireApi::Chat`)

- Classic OpenAI-compatible API at `/v1/chat/completions`
- Widely supported by third-party providers
- SSE-only streaming
- Used by: Most third-party providers (Anthropic, local models, etc.)

```rust
enum WireApi {
    Responses,  // OpenAI Responses API
    Chat,       // Chat Completions API (default)
}
```

## Provider Configuration

Providers are defined in `~/.codex/config.toml`:

```toml
[model_providers.my_provider]
name = "My Provider"
base_url = "https://api.provider.com/v1"
env_key = "MY_PROVIDER_API_KEY"
wire_api = "responses"  # or "chat"
supports_websockets = true

# Optional settings
stream_max_retries = 5
stream_idle_timeout_ms = 300000
request_max_retries = 4

# Custom headers
[model_providers.my_provider.http_headers]
"X-Custom-Header" = "value"

# Headers from environment
[model_providers.my_provider.env_http_headers]
"X-Api-Version" = "API_VERSION_ENV_VAR"
```

### Provider Fields

| Field | Type | Description |
|-------|------|-------------|
| `name` | String | Display name |
| `base_url` | Option<String> | API endpoint (default: OpenAI) |
| `env_key` | Option<String> | Environment variable for API key |
| `wire_api` | WireApi | Protocol to use |
| `requires_openai_auth` | bool | Use OpenAI/ChatGPT auth flow |
| `supports_websockets` | bool | Enable WebSocket transport |
| `stream_max_retries` | u64 | Retry count for stream errors |
| `stream_idle_timeout_ms` | u64 | Stream idle timeout |
| `request_max_retries` | u64 | Retry count for requests |
| `http_headers` | Map | Static headers |
| `env_http_headers` | Map | Headers from env vars |

## ModelClient Architecture

```
┌─────────────────────────────────────────────────────────────┐
│  ModelClient (immutable, cloneable)                         │
│                                                             │
│  ├── config: Config                                         │
│  ├── auth_manager: AuthManager                              │
│  ├── model_info: ModelInfo                                  │
│  ├── provider: ModelProviderInfo                            │
│  ├── otel_manager: OtelManager                              │
│  └── transport_manager: TransportManager                    │
│                                                             │
│  Methods:                                                   │
│  • new_session() → ModelClientSession                       │
│  • get_model() → String                                     │
│  • get_model_info() → ModelInfo                             │
│  • compact_conversation_history() → Vec<ResponseItem>       │
│                                                             │
└─────────────────────────────────────────────────────────────┘
                          │
                          │ new_session()
                          ▼
┌─────────────────────────────────────────────────────────────┐
│  ModelClientSession (mutable, per-turn)                     │
│                                                             │
│  ├── connection: Option<WebSocketConnection>                │
│  ├── websocket_last_items: Vec<ResponseItem>                │
│  ├── transport_manager: TransportManager                    │
│  └── turn_state: OnceLock<String>  (sticky routing)         │
│                                                             │
│  Methods:                                                   │
│  • stream(prompt) → ResponseStream                          │
│  • try_switch_fallback_transport() → bool                   │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

## Streaming Flow

### stream() Method

```rust
pub async fn stream(&mut self, prompt: &Prompt) -> Result<ResponseStream> {
    match self.state.provider.wire_api {
        WireApi::Responses => {
            if websocket_enabled {
                self.stream_responses_websocket(prompt).await
            } else {
                self.stream_responses_api(prompt).await
            }
        }
        WireApi::Chat => {
            let api_stream = self.stream_chat_completions(prompt).await?;
            // Optionally aggregate reasoning tokens
            Ok(map_response_stream(api_stream, otel_manager))
        }
    }
}
```

### Transport Priority

1. **WebSocket** (if supported and enabled) - lowest latency
2. **SSE/HTTPS** - fallback if WebSocket fails
3. **Automatic fallback** after max retries

## Prompt Structure

```rust
struct Prompt {
    /// Conversation history
    input: Vec<ResponseItem>,

    /// Available tools
    tools: Vec<ToolSpec>,

    /// Allow parallel tool calls
    parallel_tool_calls: bool,

    /// System instructions
    base_instructions: BaseInstructions,

    /// Model personality
    personality: Option<Personality>,

    /// Structured output schema
    output_schema: Option<Value>,
}
```

## Response Events

The streaming response produces these events:

```rust
enum ResponseEvent {
    /// Response started
    Created,

    /// New output item (message, tool call) started
    OutputItemAdded(ResponseItem),

    /// Text chunk for streaming display
    OutputTextDelta(String),

    /// Output item completed
    OutputItemDone(ResponseItem),

    /// Reasoning content (extended thinking)
    ReasoningSummaryDelta { delta: String, summary_index: u32 },
    ReasoningSummaryPartAdded { summary_index: u32 },

    /// Whether server included reasoning in response
    ServerReasoningIncluded(bool),

    /// Rate limit information
    RateLimits(RateLimitSnapshot),

    /// Models list updated
    ModelsEtag(String),

    /// Response completed
    Completed {
        response_id: String,
        token_usage: Option<TokenUsage>,
    },
}
```

## Authentication

### Auth Modes

| Mode | Description |
|------|-------------|
| `ApiKey` | Environment variable API key |
| `Chatgpt` | ChatGPT login token |
| `None` | No authentication |

### Auth Flow

```
1. Check provider.requires_openai_auth
2. If true → use AuthManager (handles login, token refresh)
3. If false → use env_key from environment
4. Build Authorization header: "Bearer <token>"
```

## Retry and Error Handling

### Retryable Conditions

- Network errors
- 5xx server errors
- Stream disconnection
- Idle timeout exceeded

### Retry Configuration

```rust
struct RetryConfig {
    max_attempts: u64,       // Default: 4
    base_delay: Duration,    // Default: 200ms
    retry_429: bool,         // Rate limit retry
    retry_5xx: bool,         // Server error retry
    retry_transport: bool,   // Network error retry
}
```

### Backoff Strategy

```rust
fn backoff(retries: u32) -> Duration {
    // Exponential backoff with jitter
    let base = Duration::from_millis(200);
    let exp = 2u64.pow(retries);
    base * exp + random_jitter()
}
```

### Transport Fallback

```rust
// After max retries, try switching transport
if retries >= max_retries && client_session.try_switch_fallback_transport() {
    retries = 0;  // Reset for new transport
    continue;
}
```

## Compaction

The model client provides a compaction API for reducing conversation history:

```rust
impl ModelClient {
    pub async fn compact_conversation_history(
        &self,
        prompt: &Prompt,
    ) -> Result<Vec<ResponseItem>> {
        // Calls /v1/compact endpoint
        // Returns condensed history
    }
}
```

## Model Information

```rust
struct ModelInfo {
    slug: String,                    // e.g., "gpt-4o"
    context_window: Option<i64>,     // Token limit
    supports_parallel_tool_calls: bool,
    effective_context_window_percent: i64,  // For auto-compact threshold
    // ...
}
```

## Deeper Investigation Pointers

| Topic | Where to Look |
|-------|---------------|
| WebSocket implementation | `client.rs` - `stream_responses_websocket` |
| SSE streaming | `codex_api` crate |
| Provider registry | `model_provider_info.rs` - built-in providers |
| Auth manager | `auth.rs`, `auth_manager.rs` |
| Request telemetry | `client.rs` - `build_request_telemetry` |
| Tool serialization | `tools/spec.rs` - `create_tools_json_*` |
| Response parsing | `codex_api` crate - event parsing |
