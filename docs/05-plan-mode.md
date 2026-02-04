# Plan Mode (Collaboration Modes)

## Overview

Codex supports multiple "collaboration modes" that control how the agent interacts with the user and whether it can execute mutating actions. Plan Mode specifically implements a "think before act" pattern where the agent explores and plans before executing.

## Key Components

| Component | File | Purpose |
|-----------|------|---------|
| `ModeKind` | `protocol/config_types.rs:173` | Enum of available modes |
| `CollaborationMode` | `protocol/config_types.rs` | Mode + settings combination |
| `CollaborationModeMask` | `protocol/config_types.rs` | Partial mode override |
| Mode presets | `collaboration_mode_presets.rs` | Built-in mode definitions |
| Mode templates | `templates/collaboration_mode/` | Instructions per mode |

## Collaboration Modes

### ModeKind Enum

```rust
enum ModeKind {
    Plan,           // Think-before-act, no mutations
    Code,           // Default coding mode (default)
    PairProgramming, // Interactive coding with user
    Execute,        // Autonomous execution
}
```

### Mode Characteristics

| Mode | Mutations | User Interaction | Use Case |
|------|-----------|------------------|----------|
| **Plan** | ❌ Forbidden | High (questions, refinement) | Complex planning |
| **Code** | ✅ Allowed | Medium | Default coding |
| **PairProgramming** | ✅ Allowed | High (collaborative) | Interactive development |
| **Execute** | ✅ Allowed | Low (progress updates) | Autonomous tasks |

## Plan Mode Details

### Philosophy

Plan Mode enforces a structured approach:

1. **Explore first** - Gather facts from the environment before asking questions
2. **Ask about intent** - Clarify what the user actually wants
3. **Discuss implementation** - Refine the approach until decision-complete
4. **Propose plan** - Output a structured plan for approval

### Execution Rules

**Allowed (non-mutating):**
- Reading/searching files, configs, schemas
- Static analysis and repo exploration
- Dry-run commands that don't edit tracked files
- Tests/builds that only write to caches/artifacts

**Forbidden (mutating):**
- Editing or writing files
- Running formatters/linters that rewrite files
- Applying patches, migrations, codegen
- Side-effectful commands that implement the plan

### Plan Output Format

Plans are wrapped in a special tag for client rendering:

```markdown
<proposed_plan>
# Plan Title

## Summary
Brief overview of what will be implemented...

## API Changes
- New endpoint: `POST /api/users`
- Modified type: `UserConfig`

## Implementation Steps
1. Create database migration
2. Add model layer
3. Implement API handler
4. Add tests

## Test Cases
- [ ] Happy path: create user successfully
- [ ] Error case: duplicate email
- [ ] Edge case: optional fields

## Assumptions
- Using existing auth middleware
- PostgreSQL database
</proposed_plan>
```

## CollaborationMode Configuration

### Structure

```rust
struct CollaborationMode {
    mode: ModeKind,
    settings: Settings,
}

struct Settings {
    model: String,                              // Model to use
    reasoning_effort: Option<ReasoningEffort>,  // Extended thinking level
    developer_instructions: Option<String>,      // Mode-specific instructions
}
```

### Presets

Built-in presets combine mode with appropriate settings:

```rust
fn plan_preset() -> CollaborationModeMask {
    CollaborationModeMask {
        name: "Plan".to_string(),
        mode: Some(ModeKind::Plan),
        model: None,
        reasoning_effort: Some(Some(ReasoningEffort::Medium)),
        developer_instructions: Some(Some(PLAN_INSTRUCTIONS.to_string())),
    }
}

fn execute_preset() -> CollaborationModeMask {
    CollaborationModeMask {
        name: "Execute".to_string(),
        mode: Some(ModeKind::Execute),
        model: None,
        reasoning_effort: Some(Some(ReasoningEffort::High)),
        developer_instructions: Some(Some(EXECUTE_INSTRUCTIONS.to_string())),
    }
}
```

### CollaborationModeMask

Masks allow partial overrides:

```rust
struct CollaborationModeMask {
    name: String,
    mode: Option<ModeKind>,                     // Override mode
    model: Option<String>,                       // Override model
    reasoning_effort: Option<Option<ReasoningEffort>>,  // Option<None> clears
    developer_instructions: Option<Option<String>>,     // Option<None> clears
}
```

## Mode-Specific Tools

### update_plan Tool

A progress tracking tool for non-Plan modes:

```rust
static PLAN_TOOL: LazyLock<ToolSpec> = LazyLock::new(|| {
    ToolSpec::Function(ResponsesApiTool {
        name: "update_plan".to_string(),
        description: "Updates the task plan with steps and status",
        parameters: JsonSchema::Object {
            properties: {
                "explanation": JsonSchema::String,
                "plan": JsonSchema::Array {
                    items: {
                        "step": String,
                        "status": "pending|in_progress|completed"
                    }
                }
            }
        }
    })
});
```

**Important:** The `update_plan` tool is NOT for Plan Mode. It's for tracking progress in Execute/Code modes. Using it in Plan Mode returns an error.

### request_user_input Tool

For asking structured questions:

```rust
// In Plan Mode, the agent should prefer this tool for questions
// Provides multiple-choice options rather than open-ended questions
request_user_input({
    "question": "Which authentication method should we use?",
    "options": [
        {"label": "JWT", "description": "Stateless, good for microservices"},
        {"label": "Session", "description": "Simpler, server-side state"},
        {"label": "OAuth2", "description": "Third-party auth delegation"}
    ]
})
```

## Mode Switching

### During Session

Modes can be changed via operations:

```rust
enum Op {
    // ...
    SetCollaborationMode(CollaborationMode),
    // ...
}
```

### Per-Turn Context

Each turn captures the active mode:

```rust
struct TurnContext {
    collaboration_mode: CollaborationMode,
    // ...
}
```

The mode affects:
- Which tools are available
- Whether mutating actions are allowed
- What instructions are injected into the prompt

## Reasoning Effort

Associated with modes to control extended thinking:

```rust
enum ReasoningEffort {
    Low,
    Medium,
    High,
}
```

| Mode | Default Effort | Rationale |
|------|----------------|-----------|
| Plan | Medium | Balanced exploration |
| Code | None | Quick responses |
| PairProgramming | Medium | Thoughtful collaboration |
| Execute | High | Complex autonomous tasks |

## Architecture Flow

```
┌──────────────────────────────────────────────────────────────────┐
│                    Mode Selection                                 │
│                                                                   │
│  User/Config → CollaborationMode → TurnContext                    │
│                                                                   │
└──────────────────────────────────┬───────────────────────────────┘
                                   │
                                   ▼
┌──────────────────────────────────────────────────────────────────┐
│                    Prompt Construction                            │
│                                                                   │
│  base_instructions + mode.developer_instructions                  │
│                                                                   │
│  Mode: Plan                                                       │
│  ├── Inject plan mode instructions (explore/ask/propose)          │
│  ├── Set reasoning_effort = Medium                                │
│  └── Filter out mutating tools (optional)                         │
│                                                                   │
│  Mode: Execute                                                    │
│  ├── Inject execute instructions (assumptions-first)              │
│  ├── Set reasoning_effort = High                                  │
│  └── Add update_plan tool                                         │
│                                                                   │
└──────────────────────────────────┬───────────────────────────────┘
                                   │
                                   ▼
┌──────────────────────────────────────────────────────────────────┐
│                    Tool Execution                                 │
│                                                                   │
│  Mode: Plan                                                       │
│  └── Block mutating tools → Return error to model                 │
│                                                                   │
│  Mode: Execute/Code                                               │
│  └── Allow all tools → Execute normally                           │
│                                                                   │
└──────────────────────────────────────────────────────────────────┘
```

## Plan Mode Phases

### Phase 1: Ground in Environment

```
┌─────────────────────────────────────────┐
│  Explore First, Ask Second              │
│                                         │
│  • Search files, configs, schemas       │
│  • Read code to understand context      │
│  • Identify what's already there        │
│  • Only ask if truly undiscoverable     │
│                                         │
└─────────────────────────────────────────┘
```

### Phase 2: Intent Chat

```
┌─────────────────────────────────────────┐
│  What They Actually Want                │
│                                         │
│  Clarify:                               │
│  • Goal + success criteria              │
│  • Audience                             │
│  • In/out of scope                      │
│  • Constraints                          │
│  • Key preferences/tradeoffs            │
│                                         │
│  Rule: If ambiguity remains, ask first  │
│                                         │
└─────────────────────────────────────────┘
```

### Phase 3: Implementation Chat

```
┌─────────────────────────────────────────┐
│  What/How We'll Build                   │
│                                         │
│  Define:                                │
│  • Approach                             │
│  • Interfaces (APIs/schemas/I/O)        │
│  • Data flow                            │
│  • Edge cases/failure modes             │
│  • Testing + acceptance criteria        │
│  • Rollout/monitoring                   │
│  • Migrations/compat constraints        │
│                                         │
│  Output: Decision-complete spec         │
│                                         │
└─────────────────────────────────────────┘
```

## Deeper Investigation Pointers

| Topic | Where to Look |
|-------|---------------|
| Mode templates | `templates/collaboration_mode/*.md` |
| Mode presets | `models_manager/collaboration_mode_presets.rs` |
| Mode config types | `protocol/config_types.rs` |
| Plan tool handler | `tools/handlers/plan.rs` |
| Request user input | `tools/handlers/request_user_input.rs` |
| Mode switching | `codex.rs` - Op handling |
