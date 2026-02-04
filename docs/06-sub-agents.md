# Sub-Agents (Child Threads)

## Overview

Codex supports spawning child agents (sub-agents) that run as separate threads with their own conversation context. This enables delegation of subtasks, parallel execution, and specialized agent roles.

## Key Components

| Component | File | Purpose |
|-----------|------|---------|
| `ThreadManager` | `thread_manager.rs:50` | Manages all active threads |
| `AgentControl` | `agent/control.rs:20` | Control plane for multi-agent ops |
| `CollabHandler` | `tools/handlers/collab.rs:29` | Tool handlers for agent operations |
| `Guards` | `agent/guards.rs` | Enforces spawn limits |
| `AgentStatus` | `agent/mod.rs` | Agent lifecycle states |

## Agent Management Architecture

```
┌─────────────────────────────────────────────────────────────────────────┐
│                         ThreadManager                                    │
│                                                                          │
│  threads: HashMap<ThreadId, Arc<CodexThread>>                            │
│                                                                          │
│  ┌────────────┐  ┌────────────┐  ┌────────────┐                         │
│  │  Thread A  │  │  Thread B  │  │  Thread C  │  ...                    │
│  │  (parent)  │  │ (sub-agent)│  │ (sub-agent)│                         │
│  └─────┬──────┘  └─────┬──────┘  └─────┬──────┘                         │
│        │               │               │                                 │
│        └───────────────┴───────────────┘                                 │
│                        │                                                 │
│                        ▼                                                 │
│             AgentControl (shared)                                        │
│             ├── spawn_agent()                                            │
│             ├── send_prompt()                                            │
│             ├── get_status()                                             │
│             ├── interrupt_agent()                                        │
│             └── shutdown_agent()                                         │
│                                                                          │
└─────────────────────────────────────────────────────────────────────────┘
```

## AgentControl

The control plane for multi-agent operations:

```rust
struct AgentControl {
    /// Weak reference to avoid cycles
    manager: Weak<ThreadManagerState>,
    /// Shared spawn guards
    state: Arc<Guards>,
}

impl AgentControl {
    /// Spawn a new agent thread and submit initial prompt
    async fn spawn_agent(
        &self,
        config: Config,
        prompt: String,
        session_source: Option<SessionSource>,
    ) -> Result<ThreadId>;

    /// Send a prompt to an existing agent
    async fn send_prompt(
        &self,
        agent_id: ThreadId,
        prompt: String,
    ) -> Result<String>;

    /// Interrupt the current task
    async fn interrupt_agent(&self, agent_id: ThreadId) -> Result<String>;

    /// Shutdown an agent
    async fn shutdown_agent(&self, agent_id: ThreadId) -> Result<String>;

    /// Get agent status
    async fn get_status(&self, agent_id: ThreadId) -> AgentStatus;

    /// Subscribe to status updates
    async fn subscribe_status(
        &self,
        agent_id: ThreadId,
    ) -> Result<Receiver<AgentStatus>>;
}
```

## Collaboration Tools

Tools exposed to the model for agent management:

### spawn_agent

Creates a new sub-agent:

```rust
struct SpawnAgentArgs {
    message: String,           // Initial prompt
    agent_type: Option<AgentRole>,  // Specialized role
}

struct SpawnAgentResult {
    agent_id: String,
}
```

Usage:
```json
{
  "tool": "spawn_agent",
  "arguments": {
    "message": "Search the codebase for all usages of deprecated API",
    "agent_type": "search"
  }
}
```

### send_input

Sends a message to an existing agent:

```rust
struct SendInputArgs {
    id: String,        // Agent ID
    message: String,   // Message to send
    interrupt: bool,   // Interrupt current task first?
}

struct SendInputResult {
    submission_id: String,
}
```

### wait

Waits for agents to reach a final status:

```rust
struct WaitArgs {
    ids: Vec<String>,      // Agent IDs to wait for
    timeout_ms: Option<i64>,  // Default: 30000, Min: 10000, Max: 300000
}

struct WaitResult {
    status: HashMap<ThreadId, AgentStatus>,
    timed_out: bool,
}
```

### close_agent

Shuts down an agent:

```rust
struct CloseAgentArgs {
    id: String,
}

struct CloseAgentResult {
    status: AgentStatus,
}
```

## Agent Status Lifecycle

```rust
enum AgentStatus {
    PendingInit,           // Just created
    Running,               // Processing a turn
    Completed(Option<String>),  // Turn finished with message
    Errored(String),       // Error occurred
    Shutdown,              // Gracefully shut down
    NotFound,              // Agent doesn't exist
}
```

### Status Transitions

```
                ┌──────────────┐
                │ PendingInit  │
                └──────┬───────┘
                       │ First prompt received
                       ▼
                ┌──────────────┐
          ┌────►│   Running    │◄────┐
          │     └──────┬───────┘     │
          │            │             │
          │     ┌──────┴──────┐      │
          │     │             │      │
          │     ▼             ▼      │
     ┌────┴──────┐     ┌──────┴────┐ │
     │ Completed │     │  Errored  │ │
     │   (msg)   │     │   (err)   │ │
     └────┬──────┘     └─────┬─────┘ │
          │                  │       │
          │    New prompt    │       │
          └──────────────────┴───────┘
                       │
                       │ Op::Shutdown
                       ▼
                ┌──────────────┐
                │   Shutdown   │
                └──────────────┘
```

## Spawn Depth Limits

To prevent infinite recursion, there's a maximum spawn depth:

```rust
const MAX_THREAD_SPAWN_DEPTH: i32 = 3;

fn exceeds_thread_spawn_depth_limit(depth: i32) -> bool {
    depth > MAX_THREAD_SPAWN_DEPTH
}

fn next_thread_spawn_depth(session_source: &SessionSource) -> i32 {
    match session_source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { depth, .. }) => depth + 1,
        _ => 1,
    }
}
```

When max depth is reached:
- Collab tools are disabled for that agent
- Agent must solve the task itself

## Session Source Tracking

Each thread knows its origin:

```rust
enum SessionSource {
    Cli,                    // User CLI
    VSCode,                 // VS Code extension
    Exec,                   // Programmatic API
    SubAgent(SubAgentSource),  // Spawned by another agent
    // ...
}

enum SubAgentSource {
    ThreadSpawn {
        parent_thread_id: ThreadId,
        depth: i32,
    },
}
```

## Agent Spawn Configuration

Child agents inherit parent's configuration with overrides:

```rust
fn build_agent_spawn_config(
    base_instructions: &BaseInstructions,
    turn: &TurnContext,
    child_depth: i32,
) -> Result<Config> {
    let mut config = turn.client.config().clone();

    // Inherit from parent
    config.base_instructions = Some(base_instructions.text.clone());
    config.model = Some(turn.client.get_model());
    config.model_provider = turn.client.get_provider();
    config.developer_instructions = turn.developer_instructions.clone();
    config.cwd = turn.cwd.clone();
    config.approval_policy = turn.approval_policy;
    config.sandbox_policy = turn.sandbox_policy.clone();

    // Disable collab if at max depth
    if exceeds_thread_spawn_depth_limit(child_depth + 1) {
        config.features.disable(Feature::Collab);
    }

    Ok(config)
}
```

## Agent Roles

Specialized agent configurations:

```rust
enum AgentRole {
    Default,    // General purpose
    Search,     // Optimized for codebase search
    // ... other roles
}

impl AgentRole {
    fn apply_to_config(&self, config: &mut Config) -> Result<()> {
        match self {
            AgentRole::Search => {
                // Configure for search operations
            }
            // ...
        }
        Ok(())
    }
}
```

## Events

Agent operations emit events for UI tracking:

```rust
// Spawn events
CollabAgentSpawnBeginEvent {
    call_id: String,
    sender_thread_id: ThreadId,
    prompt: String,
}

CollabAgentSpawnEndEvent {
    call_id: String,
    sender_thread_id: ThreadId,
    new_thread_id: Option<ThreadId>,
    prompt: String,
    status: AgentStatus,
}

// Interaction events
CollabAgentInteractionBeginEvent {
    call_id: String,
    sender_thread_id: ThreadId,
    receiver_thread_id: ThreadId,
    prompt: String,
}

CollabAgentInteractionEndEvent {
    call_id: String,
    sender_thread_id: ThreadId,
    receiver_thread_id: ThreadId,
    prompt: String,
    status: AgentStatus,
}

// Wait events
CollabWaitingBeginEvent {
    sender_thread_id: ThreadId,
    receiver_thread_ids: Vec<ThreadId>,
    call_id: String,
}

CollabWaitingEndEvent {
    sender_thread_id: ThreadId,
    call_id: String,
    statuses: HashMap<ThreadId, AgentStatus>,
}

// Close events
CollabCloseBeginEvent { ... }
CollabCloseEndEvent { ... }
```

## Guards (Resource Limits)

Prevents resource exhaustion:

```rust
struct Guards {
    /// Tracks spawned threads per session
    spawned_threads: Mutex<HashSet<ThreadId>>,
}

impl Guards {
    /// Reserve a spawn slot (returns error if at limit)
    fn reserve_spawn_slot(&self, max_threads: usize) -> Result<SpawnReservation>;

    /// Release a thread slot after shutdown
    fn release_spawned_thread(&self, thread_id: ThreadId);
}
```

Configuration:
```toml
[agents]
max_threads = 5  # Maximum sub-agents per session
```

## Typical Workflow

```
┌────────────────────────────────────────────────────────────────────────┐
│  Parent Agent                                                          │
│                                                                         │
│  1. Receive complex task                                                │
│     ▼                                                                   │
│  2. Decide to delegate                                                  │
│     ▼                                                                   │
│  3. spawn_agent({ message: "Search for X", agent_type: "search" })      │
│     └──────────────────────────────────────────────────┐                │
│                                                         ▼                │
│                                           ┌───────────────────────────┐ │
│                                           │  Sub-Agent (search)       │ │
│                                           │                           │ │
│                                           │  4. Execute search task   │ │
│                                           │  5. Return results        │ │
│                                           │  6. Status → Completed    │ │
│                                           └───────────────────────────┘ │
│     ┌──────────────────────────────────────────────────┘                │
│     ▼                                                                   │
│  7. wait({ ids: [agent_id], timeout_ms: 30000 })                        │
│     ▼                                                                   │
│  8. Get results from completed agent                                    │
│     ▼                                                                   │
│  9. close_agent({ id: agent_id })                                       │
│     ▼                                                                   │
│  10. Continue with results                                              │
│                                                                         │
└────────────────────────────────────────────────────────────────────────┘
```

## Thread Manager Operations

```rust
impl ThreadManager {
    /// Create a new thread
    async fn start_thread(&self, config: Config) -> Result<NewThread>;

    /// Resume a thread from rollout file
    async fn resume_thread_from_rollout(
        &self,
        config: Config,
        rollout_path: PathBuf,
        auth_manager: Arc<AuthManager>,
    ) -> Result<NewThread>;

    /// Fork a thread at a specific point
    async fn fork_thread(
        &self,
        nth_user_message: usize,
        config: Config,
        path: PathBuf,
    ) -> Result<NewThread>;

    /// Get a thread by ID
    async fn get_thread(&self, thread_id: ThreadId) -> Result<Arc<CodexThread>>;

    /// Remove a thread
    async fn remove_thread(&self, thread_id: &ThreadId) -> Option<Arc<CodexThread>>;

    /// Subscribe to new thread notifications
    fn subscribe_thread_created(&self) -> Receiver<ThreadId>;
}
```

## Deeper Investigation Pointers

| Topic | Where to Look |
|-------|---------------|
| Thread lifecycle | `thread_manager.rs` |
| Agent control | `agent/control.rs` |
| Collab tools | `tools/handlers/collab.rs` |
| Spawn guards | `agent/guards.rs` |
| Agent status | `agent/status.rs` |
| Agent roles | `agent/role.rs` |
| Session source | `protocol/protocol.rs` - SessionSource |
