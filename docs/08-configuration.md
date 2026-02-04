# Configuration & Settings

## Overview

Codex uses a layered configuration system that merges settings from multiple sources: user config files, project-specific settings, CLI overrides, profiles, and cloud-enforced requirements. Configuration controls model selection, approval policies, sandbox modes, MCP servers, and feature flags.

## Key Components

| Component | File | Purpose |
|-----------|------|---------|
| `Config` | `config/mod.rs:113` | Runtime configuration struct |
| `ConfigToml` | `config/mod.rs` | TOML file deserialization |
| `ConfigBuilder` | `config/mod.rs:369` | Builder for loading config |
| `ConfigProfile` | `config/profile.rs` | Named configuration presets |
| `Features` | `features.rs:163` | Feature flag management |
| `ConfigLayerStack` | `config_loader/` | Configuration layer merging |

## Configuration Sources

### Layer Priority (High → Low)

```
1. CLI Overrides        (--model, --sandbox-mode, etc.)
2. Harness Overrides    (programmatic overrides)
3. Cloud Requirements   (enterprise enforcement)
4. Profile Settings     ([profiles.myprofile] in config.toml)
5. Project Config       (.codex/config.toml in repo)
6. User Config          (~/.codex/config.toml)
7. Built-in Defaults    (hardcoded in binary)
```

### Config File Locations

```
~/.codex/
├── config.toml          # User configuration
├── history.jsonl        # Message history
├── sessions/            # Session rollouts
└── skills/              # User skills

<project>/.codex/
├── config.toml          # Project-specific settings
├── skills/              # Project skills
└── AGENTS.md            # Project instructions (or CLAUDE.md)
```

## Config Structure

### ConfigToml (File Format)

```toml
# Model configuration
model = "gpt-5.2-codex"
model_provider = "openai"
model_reasoning_effort = "medium"
model_reasoning_summary = "concise"

# Approval and sandbox
approval_policy = "on-request"
sandbox_mode = "workspace-write"

# User interface
personality = "pragmatic"
hide_agent_reasoning = false
animations = true

# MCP servers
[mcp_servers.filesystem]
command = "npx"
args = ["-y", "@anthropic/mcp-server-filesystem"]
env = { "MCP_ROOT" = "/workspace" }

# Model providers
[model_providers.custom]
name = "Custom Provider"
base_url = "https://api.example.com/v1"
api_key_env_var = "CUSTOM_API_KEY"

# Features
[features]
undo = true
web_search_cached = false

# Profiles
[profiles.fast]
model = "gpt-4o-mini"
model_reasoning_effort = "low"

[profiles.deep]
model = "o1-preview"
model_reasoning_effort = "high"
```

### Runtime Config Struct

```rust
struct Config {
    // Model settings
    model: Option<String>,
    model_context_window: Option<i64>,
    model_auto_compact_token_limit: Option<i64>,
    model_provider_id: String,
    model_provider: ModelProviderInfo,
    model_reasoning_effort: Option<ReasoningEffort>,
    model_reasoning_summary: ReasoningSummary,
    model_verbosity: Option<Verbosity>,

    // Policies
    approval_policy: Constrained<AskForApproval>,
    sandbox_policy: Constrained<SandboxPolicy>,
    enforce_residency: Constrained<Option<ResidencyRequirement>>,

    // Instructions
    user_instructions: Option<String>,       // From AGENTS.md
    base_instructions: Option<String>,       // System prompt override
    developer_instructions: Option<String>,  // Injected per-turn
    compact_prompt: Option<String>,          // Compaction prompt

    // MCP
    mcp_servers: Constrained<HashMap<String, McpServerConfig>>,

    // Environment
    cwd: PathBuf,
    codex_home: PathBuf,

    // Feature flags
    features: Features,

    // UI settings
    personality: Option<Personality>,
    hide_agent_reasoning: bool,
    animations: bool,
    tui_alternate_screen: AltScreenMode,

    // ... many more fields
}
```

## Approval Policy

Controls when user approval is required for tool execution:

```rust
enum AskForApproval {
    /// Only "known safe" read-only commands auto-approved.
    /// Everything else asks user.
    UnlessTrusted,

    /// Auto-approve in sandbox, escalate on failure.
    OnFailure,

    /// Model decides when to ask (default).
    OnRequest,

    /// Never ask - failures return to model.
    Never,
}
```

### Trust Levels

Projects can be marked as trusted or untrusted:

```rust
enum TrustLevel {
    Trusted,    // More permissive
    Untrusted,  // More restrictive
}
```

Project trust is determined by:
1. Explicit trust setting in config
2. Git repository detection
3. Directory location

## Sandbox Policy

Controls execution environment restrictions:

```rust
enum SandboxMode {
    /// No writes allowed (default).
    ReadOnly,

    /// Writes allowed in workspace only.
    WorkspaceWrite,

    /// Full access (dangerous).
    DangerFullAccess,
}

struct SandboxPolicy {
    // For WorkspaceWrite mode
    writable_roots: Vec<PathBuf>,
    network_access: NetworkAccess,
    exclude_tmpdir_env_var: bool,
    exclude_slash_tmp: bool,
}

enum NetworkAccess {
    None,       // No outbound network
    Limited,    // Some allowed
    Full,       // All allowed
}
```

### Sandbox Configuration

```toml
sandbox_mode = "workspace-write"

[sandbox_workspace_write]
writable_roots = ["/workspace", "/tmp"]
network_access = "limited"
exclude_tmpdir_env_var = false
exclude_slash_tmp = false
```

## MCP Server Configuration

### Transport Types

```rust
enum McpServerTransportConfig {
    /// Stdio-based communication
    Stdio {
        command: String,
        args: Vec<String>,
        env: Option<HashMap<String, String>>,
        env_vars: Vec<String>,  // Passthrough vars
        cwd: Option<PathBuf>,
    },

    /// HTTP-based communication
    StreamableHttp {
        url: String,
        bearer_token_env_var: Option<String>,
        http_headers: Option<HashMap<String, String>>,
        env_http_headers: Option<HashMap<String, String>>,
    },
}
```

### MCP Config Options

```rust
struct McpServerConfig {
    transport: McpServerTransportConfig,
    enabled: bool,
    startup_timeout_sec: Option<Duration>,
    tool_timeout_sec: Option<Duration>,
    enabled_tools: Option<Vec<String>>,   // Allow-list
    disabled_tools: Option<Vec<String>>,  // Deny-list
    scopes: Option<Vec<String>>,          // OAuth scopes
}
```

### Example MCP Configuration

```toml
[mcp_servers.filesystem]
command = "npx"
args = ["-y", "@anthropic/mcp-server-filesystem"]
env = { "MCP_ROOT" = "/workspace" }
startup_timeout_sec = 30
tool_timeout_sec = 60
enabled_tools = ["read_file", "write_file", "list_directory"]

[mcp_servers.database]
url = "https://mcp.example.com/db"
bearer_token_env_var = "MCP_DB_TOKEN"
http_headers = { "X-Org-Id" = "my-org" }

[mcp_servers.legacy]
enabled = false  # Disabled server
```

## Feature Flags

### Feature Definition

```rust
enum Feature {
    // Stable
    GhostCommit,      // Undo support
    ShellTool,        // Enable shell tool

    // Experimental
    UnifiedExec,      // PTY-backed exec
    ApplyPatchFreeform,
    WebSearchRequest,
    WebSearchCached,
    ExecPolicy,       // Exec policy enforcement
    RequestRule,      // Model-proposed rules
    WindowsSandbox,
    RemoteCompaction,
    RemoteModels,
    ShellSnapshot,
    Sqlite,
    Collab,           // Sub-agents
    // ... more
}

struct FeatureSpec {
    id: Feature,
    key: &'static str,       // Config key
    stage: Stage,            // Stable/Experimental/Deprecated
    default_enabled: bool,
}
```

### Feature Configuration

```toml
[features]
undo = true                    # Enable ghost commits
shell_tool = true              # Enable shell (default)
web_search_cached = false      # Disable web search
collab = true                  # Enable sub-agents
remote_compaction = true       # Use remote compaction
```

### Feature API

```rust
impl Features {
    fn with_defaults() -> Self;
    fn enabled(&self, f: Feature) -> bool;
    fn enable(&mut self, f: Feature);
    fn disable(&mut self, f: Feature);
}
```

## Configuration Profiles

Named configuration presets for quick switching:

```rust
struct ConfigProfile {
    model: Option<String>,
    model_provider: Option<String>,
    approval_policy: Option<AskForApproval>,
    sandbox_mode: Option<SandboxMode>,
    model_reasoning_effort: Option<ReasoningEffort>,
    model_reasoning_summary: Option<ReasoningSummary>,
    model_verbosity: Option<Verbosity>,
    personality: Option<Personality>,
    features: Option<FeaturesToml>,
    // ... more
}
```

### Profile Configuration

```toml
[profiles.fast]
model = "gpt-4o-mini"
model_reasoning_effort = "low"
approval_policy = "on-failure"

[profiles.deep]
model = "o1-preview"
model_reasoning_effort = "high"
model_reasoning_summary = "detailed"

[profiles.offline]
model_provider = "ollama"
model = "llama3"
```

### Profile Activation

```bash
# Via CLI
codex --profile deep

# Via config
active_profile = "fast"
```

## Model Provider Configuration

### Built-in Providers

```rust
fn built_in_model_providers() -> HashMap<String, ModelProviderInfo> {
    // OpenAI, Anthropic, Azure, Ollama, LMStudio, etc.
}
```

### Custom Provider

```toml
[model_providers.custom]
name = "Custom Provider"
base_url = "https://api.example.com/v1"
api_key_env_var = "CUSTOM_API_KEY"
models = ["model-a", "model-b"]
default_model = "model-a"
```

### Provider Info

```rust
struct ModelProviderInfo {
    name: String,
    base_url: String,
    api_key_env_var: Option<String>,
    models: Vec<String>,
    default_model: Option<String>,
    supports_streaming: bool,
    supports_tools: bool,
    // ... more
}
```

## Configuration Loading

### ConfigBuilder

```rust
impl ConfigBuilder {
    pub fn codex_home(self, path: PathBuf) -> Self;
    pub fn cli_overrides(self, overrides: Vec<(String, TomlValue)>) -> Self;
    pub fn harness_overrides(self, overrides: ConfigOverrides) -> Self;
    pub fn loader_overrides(self, overrides: LoaderOverrides) -> Self;
    pub fn cloud_requirements(self, req: CloudRequirementsLoader) -> Self;
    pub fn fallback_cwd(self, cwd: Option<PathBuf>) -> Self;

    pub async fn build(self) -> std::io::Result<Config>;
}
```

### Loading Flow

```
┌─────────────────────────────────────────────────────────────────────────┐
│  ConfigBuilder::build()                                                  │
│                                                                          │
│  1. Determine codex_home (~/.codex)                                      │
│  2. Load config layers:                                                  │
│     ├── User config (~/.codex/config.toml)                               │
│     ├── Project config (<cwd>/.codex/config.toml)                        │
│     ├── CLI overrides                                                    │
│     └── Cloud requirements                                               │
│  3. Merge layers (later wins)                                            │
│  4. Deserialize to ConfigToml                                            │
│  5. Apply constraints and defaults                                       │
│  6. Build final Config                                                   │
│                                                                          │
└─────────────────────────────────────────────────────────────────────────┘
```

## Constrained Configuration

Some settings can be constrained by enterprise requirements:

```rust
struct Constrained<T> {
    value: T,
    constraint: Option<Constraint>,
}

enum Constraint {
    /// Value was set by cloud requirements and cannot be changed.
    CloudEnforced { source: String },
    /// Value has a minimum/maximum bound.
    Bounded { min: Option<T>, max: Option<T> },
}

impl<T> Constrained<T> {
    fn is_constrained(&self) -> bool;
    fn can_modify(&self) -> bool;
    fn try_set(&mut self, value: T) -> Result<(), ConstraintError>;
}
```

## UI Settings

```rust
// Personality affects response style
enum Personality {
    Friendly,   // More conversational
    Pragmatic,  // More direct
}

// Alternate screen mode
enum AltScreenMode {
    Auto,    // Detect (disable in Zellij)
    Always,  // Always use alternate screen
    Never,   // Never use alternate screen
}

// Notification settings
struct Notifications {
    on_approval: bool,
    on_turn_complete: bool,
}

enum NotificationMethod {
    Osc9,   // OSC 9 escape sequence
    Bel,    // Terminal bell
}
```

## History Settings

```rust
struct History {
    /// If true, history entries will not be written to disk.
    save_history: bool,

    /// Maximum bytes for history file.
    max_bytes: Option<usize>,

    /// Number of recent entries to keep.
    max_entries: Option<usize>,
}
```

## Environment Variables

Key environment variables:

| Variable | Purpose |
|----------|---------|
| `CODEX_HOME` | Override config directory |
| `OPENAI_API_KEY` | OpenAI API authentication |
| `ANTHROPIC_API_KEY` | Anthropic API authentication |
| `CODEX_SANDBOX_MODE` | Override sandbox mode |
| `CODEX_MODEL` | Override model selection |

## Deeper Investigation Pointers

| Topic | Where to Look |
|-------|---------------|
| Config struct | `config/mod.rs` |
| Config types | `config/types.rs` |
| Profiles | `config/profile.rs` |
| Features | `features.rs` |
| Config loading | `config_loader/` |
| MCP config | `config/types.rs` - McpServerConfig |
| Constraints | `config/constraint.rs` |
| Cloud requirements | `config_loader/config_requirements.rs` |
