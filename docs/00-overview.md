# Codex Feature Extraction: Overview

## Purpose

This document series reverse-engineers the key features of OpenAI Codex (the CLI agent) to enable a clean-room implementation of a general-purpose agentic harness.

## What is Codex?

Codex is an **agentic AI system** that:
- Takes user prompts and executes multi-step tasks
- Calls AI models to decide what actions to take
- Executes tools (shell commands, file operations, API calls)
- Loops until the task is complete or user intervenes

## Core Concepts

### Agent Loop

The fundamental pattern: **Model → Decision → Tool Execution → Repeat**

```
User Input
    ↓
┌─────────────────────────────────────┐
│           AGENT LOOP                │
│                                     │
│  ┌─────────────┐                    │
│  │ Call Model  │ ←──────────────┐   │
│  └──────┬──────┘                │   │
│         ↓                       │   │
│  ┌─────────────┐                │   │
│  │Parse Output │                │   │
│  └──────┬──────┘                │   │
│         ↓                       │   │
│    ┌────┴────┐                  │   │
│    │ Tools?  │──No──→ Done      │   │
│    └────┬────┘                  │   │
│         │ Yes                   │   │
│         ↓                       │   │
│  ┌─────────────┐                │   │
│  │Execute Tools│                │   │
│  └──────┬──────┘                │   │
│         │                       │   │
│         └───────────────────────┘   │
│                                     │
└─────────────────────────────────────┘
    ↓
Final Response
```

### Turn vs Sampling Request

| Term | Definition |
|------|------------|
| **Turn** | One user input → agent works until done or blocked |
| **Sampling Request** | One API call to the model |
| **Agent Loop Iteration** | Model call + tool execution (if any) |

A single turn may involve multiple sampling requests (model calls) as the agent executes tools and continues.

### Conversation

A **conversation** (also called **thread**) is:
- A persistent session with history
- Has a unique ID
- Can be resumed, forked, or archived
- Contains configuration (model, policies, etc.)

### Tools

**Tools** are actions the agent can take:
- **Built-in**: Shell execution, file read/write, search
- **MCP Tools**: External tools via Model Context Protocol
- **Dynamic Tools**: User-defined tools registered at runtime

### Model Providers

Codex supports multiple model providers:
- OpenAI (primary)
- Anthropic
- Others via configuration

Each provider has different:
- API formats
- Capabilities (streaming, tool calling, etc.)
- Rate limits

## Architecture Layers

```
┌─────────────────────────────────────────────────────────┐
│                    Entry Points                          │
│         (CLI, App Server, TUI, MCP Server)              │
└─────────────────────────────┬───────────────────────────┘
                              │
┌─────────────────────────────▼───────────────────────────┐
│                 Message Processing                       │
│    (Request handling, thread management, routing)        │
└─────────────────────────────┬───────────────────────────┘
                              │
┌─────────────────────────────▼───────────────────────────┐
│                    Codex Core                            │
│                                                          │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐   │
│  │    Session   │  │  Agent Loop  │  │    Tools     │   │
│  │  Management  │  │   (run_turn) │  │   (Router)   │   │
│  └──────────────┘  └──────────────┘  └──────────────┘   │
│                                                          │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐   │
│  │    Model     │  │ Conversation │  │   Config     │   │
│  │   Client     │  │   History    │  │  Management  │   │
│  └──────────────┘  └──────────────┘  └──────────────┘   │
│                                                          │
└─────────────────────────────────────────────────────────┘
```

## Key Source Files Reference

| Component | Primary Files | Purpose |
|-----------|--------------|---------|
| Agent Loop | `core/src/codex.rs` | `run_turn`, `run_sampling_request` |
| Model Client | `core/src/client.rs` | API calls, streaming |
| Tool Router | `core/src/tools/router.rs` | Tool dispatch |
| Tool Execution | `core/src/tools/parallel.rs` | Parallel/sequential execution |
| MCP Integration | `core/src/mcp_connection_manager.rs` | External tools |
| History | `core/src/conversation_history.rs` | Context management |
| Config | `core/src/config.rs` | Settings |
| Session | `core/src/codex.rs` (Session struct) | State management |

## Document Index

1. **[Agent Loop](01-agent-loop.md)** - Core execution mechanics
2. **[Model Integration](02-model-integration.md)** - Provider abstraction, streaming
3. **[Tool System](03-tool-system.md)** - Tool abstraction, MCP, routing
4. **[Conversation State](04-conversation-state.md)** - History, compaction
5. **[Plan Mode](05-plan-mode.md)** - Think-before-act
6. **[Sub-Agents](06-sub-agents.md)** - Child threads, delegation
7. **[Skills & Prompts](07-skills-prompts.md)** - Custom prompts
8. **[Configuration](08-configuration.md)** - Settings, policies
9. **[Extension Points](09-extension-points.md)** - Customization hooks

## Terminology

| Term | Definition |
|------|------------|
| **Agent** | The AI system that processes prompts and executes tasks |
| **Turn** | One cycle of user input → agent response |
| **Sampling Request** | One API call to the model |
| **Tool Call** | Model requesting execution of a tool |
| **MCP** | Model Context Protocol - standard for external tools |
| **Conversation/Thread** | Persistent session with history |
| **Context Window** | Maximum tokens the model can process |
| **Compaction** | Reducing history size to fit context window |
| **Skill** | Pre-defined prompt/instruction package |
| **Plan Mode** | Mode where agent plans before executing |
