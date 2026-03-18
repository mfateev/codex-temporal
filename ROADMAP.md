# Codex-Temporal Feature Roadmap

## Current State

Single/multi-turn conversation, full tool ecosystem via codex-core's ToolRegistry dispatch (shell, apply_patch, read_file, list_dir, grep_files, view_image, web search, request_user_input), tool approval gating with three-tier command safety and policy modes, full TUI reuse with interactive session picker, deterministic entropy, OpenAI Responses API calls via codex ModelClient with stable conversation_id, prompt caching, token usage tracking, reasoning effort/summary/personality, config.toml loading via activity, real sandbox policy from config, project context injection (AGENTS.md + git info), MCP server support (stdio/HTTP) with elicitation capture, continue-as-new, session resume via CodexHarness registry, event polling with watermarks, patch approval flow, OverrideTurnContext completion (model/effort/summary/personality), dynamic tools via signal/wait.

85+ unit tests, 24 e2e tests, 4 TUI e2e tests.

---

## Phase 1: Model Improvements

1. ~~**Streaming responses**~~ — *out of scope*
2. ~~**Reasoning effort**~~ — ✅ low/medium/high through Responses API; per-turn overrides via `UserTurnInput`; `CODEX_EFFORT` env var
3. **Multi-provider** — route `model_call` to OpenAI/Ollama/LM Studio/custom based on workflow input (provider config plumbed through `CodexWorkflowInput.model_provider` → `ModelCallInput`; actual multi-provider routing deferred to codex-core `ModelClient`)
4. ~~**Prompt caching**~~ — ✅ stable `conversation_id` per workflow, cached provider in `CodexActivities`, `TokenUsage` tracking with `cached_input_tokens`; e2e test confirms 89% cache hit rate on second turn

## Phase 2: Tool Ecosystem

5. ~~**Apply-patch**~~ — ✅ dispatched via codex-core ToolRegistry; worker handles `--codex-run-as-apply-patch` subprocess
6. ~~**File read/write**~~ — ✅ `read_file`, `list_dir`, `grep_files` via ToolRegistry dispatch (enabled by `experimental_supported_tools`)
7. ~~**Search (BM25)**~~ — ✅ available via ToolRegistry when `Feature::Apps` is enabled
8. ~~**View image**~~ — ✅ registered by `build_specs` unconditionally
9. **JS REPL** — persistent Node kernel on worker
10. ~~**Web search**~~ — ✅ server-side via Responses API; `CODEX_WEB_SEARCH=live|cached` env var
10b. ~~**request_user_input**~~ — ✅ tool intercepted in workflow via signal/wait (same pattern as exec approval); emits `RequestUserInput` event, accepts `Op::UserInputAnswer` signal; supports interrupt

## Phase 3: Approval Policies & Sandbox

11. ~~**Policy modes**~~ — ✅ `Never`/`OnRequest`/`OnFailure`/`UnlessTrusted` in workflow; `CODEX_APPROVAL_POLICY` env var
12. ~~**Policy amendment signals**~~ — ✅ `Op::OverrideTurnContext` updates `approval_policy_override` mid-workflow; persists across CAN; `effective_approval_policy()` helper; tool handler re-reads policy each iteration
12b. ~~**`OverrideTurnContext` completion**~~ — ✅ `model`, `effort`, `summary`, `personality` overrides handled; persistent overrides applied as fallbacks (per-turn takes priority); all carried across CAN; `cwd`, `sandbox_policy`, `collaboration_mode` remain unhandled (not relevant to temporal harness)
13. **Worker sandbox** — bubblewrap/container isolation for tool activities
14. ~~**Command safety analysis**~~ — ⚠️ Partial: uses `is_known_safe_command`/`command_might_be_dangerous` but diverges from codex-core's full `exec_policy` pipeline — see "Known Gaps" section below; non-shell tools (file tools) correctly skip approval
14b. ~~**Patch approval flow**~~ — ⚠️ Partial: signal/wait pattern works, but always prompts — codex-core auto-approves safe patches via `assess_patch_safety()` (see "Known Gaps")

## Phase 4: Configuration & Auth

15. **Real auth** — OAuth, API key, device code in TUI; replace `NoopAuthProvider`
16. **Real models provider** — fetch catalog, wire `Op::ListModels` to query handler
17. ~~**Config loading**~~ — ✅ `load_config` activity calls `ConfigBuilder::build_toml_string()` on worker; workflow builds real `Config` via `Config::from_toml()`; `config.toml` settings flow through to tool dispatch; `CODEX_*` env var overrides
17b. **Config reload** — handle `Op::ReloadUserConfig` signal to reload config.toml mid-workflow without restart
18. **Credential store** — keyring/file/ephemeral support

## Phase 5: Session Persistence

19. ~~**Resume flow**~~ — ✅ `CodexHarness` workflow (long-lived session registry); `TemporalAgentSession::resume()` + `fetch_initial_events()`; `--resume` flag in TUI; full interactive session picker via codex TUI `resume_picker`
20. **`/resume`, `/fork`, `/new`** — reconnect, seed from history, or start fresh (resume done; fork/new remaining)
21. **Rollout export** — query returning native codex rollout format
22. **Thread metadata** — handle `Op::SetThreadName` signal to update session name in harness registry; workflow search attributes for discoverability
22b. **History access** — handle `Op::AddToHistory` and `Op::GetHistoryEntryRequest` signals; emit `GetHistoryEntryResponse` events
22c. **Undo / rollback** — handle `Op::Undo` and `Op::ThreadRollback` signals; emit `UndoStarted`/`UndoCompleted`/`ThreadRolledBack` events

## Phase 6: Multi-Agent

23. **Child workflows** — `spawn_agent`/`send_input`/`wait`/`close_agent` as child workflow ops; intercept multi-agent tool calls (`spawn_agent`, `send_input`, `resume_agent`, `wait`, `close_agent`) and route to child workflows instead of codex-core runtime; emit `CollabAgent*` events
23b. **Batch agent jobs** — `spawn_agents_on_csv`/`report_agent_job_result` tool handling via child workflows
24. **Agent switching** — `/agent` switches TUI subscription between workflows
25. **Max concurrency** — enforce `agents.max_threads` in parent workflow

## Phase 7: MCP & Skills

26. ~~**MCP connections**~~ — ✅ `HarnessMcpManager` manages persistent connections (stdio + streamable HTTP via `RmcpClient`); `discover_mcp_tools` activity at workflow start
27. ~~**MCP tool execution**~~ — ✅ `mcp_tool_call` activity routes `mcp__server__tool` prefixed calls to appropriate server; bypasses shell approval; schemas carry through continue-as-new
27b. ~~**MCP elicitation**~~ — ✅ `HarnessMcpManager` captures elicitation via `capturing_elicitation()` callback; surfaced in `McpToolCallOutput.elicitation`; workflow emits `ElicitationRequest` event; handles `Op::ResolveElicitation` signal; `PendingElicitation` state type
27c. **MCP resource access** — handle `list_mcp_resources`, `list_mcp_resource_templates`, `read_mcp_resource` tools via MCP connections in activity; handle `Op::ListMcpTools` / `Op::RefreshMcpServers` signals
28. **Skills loading** — discover `SKILL.md` on worker, inject into system prompt; handle `Op::ListSkills` signal; emit `ListSkillsResponse` event
29. **Remote skills** — MCP-based installation; handle `Op::ListRemoteSkills` and `Op::DownloadRemoteSkill` signals; emit `ListRemoteSkillsResponse`/`RemoteSkillDownloaded` events

## Phase 8: Git & Project Context

30. ~~**Git info injection**~~ — ✅ `collect_project_context` activity reads git branch/commit/URL via `codex_core::git_info::collect_git_info()`; injected as XML EnvironmentContext in prompt
31. **Ghost commits** — auto-commit after file-modifying tool executions
32. ~~**Project docs**~~ — ✅ `collect_project_context` activity reads AGENTS.md chain via `codex_core::project_doc::read_project_docs()`; injected as XML UserInstructions in prompt; ephemeral (not stored in history)
33. **`/diff` and `/mention`** — query for git diff; signal to include file content

## Phase 9: Advanced Features

34. **Memory system** — extraction/consolidation as separate workflows; handle `Op::DropMemories` and `Op::UpdateMemories` signals
35. ~~**Personality**~~ — ✅ `CodexWorkflowInput.personality` + per-turn override via `UserTurnInput`; `CODEX_PERSONALITY` env var; flows through to `Prompt.personality`
36. **Notifications** — email/webhook on completion or approval requests
37. **Apps/connectors** — discovery through MCP gateway activities
38. ~~**Interrupt**~~ — ✅ `Op::Interrupt` sets flag checked at iteration boundaries and during approval waits; emits `TurnAborted(Interrupted)` event; returns denied-style response from tool handler to avoid codex-core drain panic
39. **Code review** — handle `Op::Review` signal; emit `EnteredReviewMode`/`ExitedReviewMode` events; trigger specialized model call on git diff
39b. ~~**Dynamic tools**~~ — ✅ `dynamic_tools: Vec<DynamicToolSpec>` on `AgentWorkflowInput`; passed to `build_specs()`; `TemporalToolHandler` intercepts matching calls, emits `DynamicToolCallRequest`, handles `Op::DynamicToolResponse` signal; `PendingDynamicTool` state type; note: codex protocol does not support inline agent definitions — agents are configured via the role system only
39c. **Custom prompts** — handle `Op::ListCustomPrompts` signal; emit `ListCustomPromptsResponse` event
39d. **Realtime conversation** — handle `Op::RealtimeConversationStart`/`Audio`/`Text`/`Close` signals; emit corresponding events; requires WebSocket or audio streaming support
39e. **Shell command execution** — handle `Op::RunUserShellCommand` for client-requested shell commands outside the model loop

## Phase 10: Production Hardening

40. **Activity timeouts & retries** — proper start-to-close, retry policies, heartbeats
41. **Resource limits** — CPU/memory/time on tool activities
42. **Observability** — OpenTelemetry tracing/metrics on worker and TUI
43. **Graceful shutdown** — drain on SIGTERM, `request_shutdown` on exit
44. **Error handling** — structured errors, proper failure propagation
45. ~~**Continue-as-new**~~ — ✅ `Op::Compact` signal triggers CAN; `ContinueAsNewState` carries history, pending turns, counters, token usage, MCP tools; e2e test confirms recall across CAN boundary

---

## Known Gaps: Approval Logic vs Codex-Core

The `TemporalToolHandler` reimplements tool approval at the workflow level
(needed for durable signal/wait), but the logic diverges from codex-core's
`ToolOrchestrator` + `exec_policy` pipeline in several ways.

### 1. Execpolicy rules are ignored

Codex-core evaluates user-configured execpolicy rules (`~/.codex/rules/`)
via `exec_policy.create_exec_approval_requirement_for_command()` before
falling back to `is_known_safe_command` / `command_might_be_dangerous`.
The harness skips execpolicy entirely and goes straight to the fallback.

**Impact**: Users who have configured allow/prompt/forbidden prefix rules
will not see them honored by the harness.

### 2. `OnFailure` policy is implemented incorrectly

In codex-core, `OnFailure` means "run the command first (in sandbox),
only ask the user if it fails." The approval happens *after* a sandbox
failure, not before. `render_decision_for_unmatched_command` returns
`Decision::Allow` for `OnFailure`.

The harness treats `OnFailure` the same as `OnRequest` — it prompts
*before* execution for unknown commands. This contradicts the intended
behavior of "run first, ask later."

**Impact**: `OnFailure` users are prompted unnecessarily for every
unknown command instead of relying on the sandbox.

### 3. `OnRequest` with unrestricted sandbox auto-allows in codex but prompts in harness

In codex-core, `render_decision_for_unmatched_command` returns
`Decision::Allow` for `OnRequest` when `FileSystemSandboxKind` is
`Unrestricted` or `ExternalSandbox` (the user opted into full access).
The harness always prompts under `OnRequest` regardless of sandbox policy.

**Impact**: Users with `sandbox_mode = "danger-full-access"` + `OnRequest`
are prompted for every unknown command when codex would auto-allow.

### 4. Dangerous commands under `Never` should be `Forbidden`, not silently allowed

In codex-core, dangerous commands + `AskForApproval::Never` →
`Decision::Forbidden` (the command is rejected outright since we can't
prompt). The harness sets `needs_approval = false` and runs the command,
relying solely on the sandbox. This is less safe — a dangerous command
could execute without any warning.

**Impact**: Dangerous commands may execute unsafely under `Never` policy
when codex would have rejected them.

### 5. `apply_patch` always prompts instead of auto-approving safe patches

In codex-core, `assess_patch_safety()` auto-approves patches that are
constrained to writable paths when a sandbox is available (most cases
under `Never`/`OnFailure`/`OnRequest`/`Granular`). Only `UnlessTrusted`
always asks. `DangerFullAccess` sandbox auto-approves unconditionally.

The harness always emits `ApplyPatchApprovalRequestEvent` and waits for
a signal, regardless of policy, sandbox, or patch content.

**Impact**: apply_patch always requires manual approval in the harness,
even when codex would auto-approve.

### 6. `Granular` policy nuances are not handled

Codex-core's `Granular` policy supports `allows_sandbox_approval()` which
can reject commands that would otherwise prompt. The harness treats
`Granular` the same as `OnRequest`.

### Path to fix

The cleanest fix would be to extract codex-core's approval decision into
a reusable function that returns `Skip`/`NeedsApproval`/`Forbidden`
without coupling to the orchestrator's execution model. The harness
could then call it and map to its signal/wait pattern. This requires
making `exec_policy.create_exec_approval_requirement_for_command()` and
`assess_patch_safety()` (or wrappers) public in codex-core.

---

## Suggested Priority

| Order | Phase | Rationale |
|-------|-------|-----------|
| ~~1~~ | ~~3 — Approval policies~~ | ⚠️ Partial (policy modes work, but approval logic diverges from codex-core — see Known Gaps) |
| ~~2~~ | ~~2 — Tool ecosystem~~ | ✅ Done (ToolRegistry dispatch, web search); JS REPL remaining |
| ~~3~~ | ~~1 — Model improvements~~ | ✅ Done (reasoning effort, prompt caching); multi-provider routing remaining |
| ~~4~~ | ~~5 — Session persistence~~ | ✅ Done (resume, picker, harness); fork/new/export remaining |
| ~~5~~ | ~~8 — Git/project context~~ | ✅ Done (git info, AGENTS.md); ghost commits, /diff remaining |
| ~~6~~ | ~~4 — Config/auth~~ | ✅ Partial (config.toml done); real auth/models/credentials remaining |
| ~~7~~ | ~~7 — MCP/skills~~ | ✅ Partial (MCP + elicitation done); resources, skills remaining |
| ~~8~~ | ~~3b/9b — Patch approval, OverrideTurnContext, dynamic tools~~ | ⚠️ Signal/wait items done, but patch auto-approve missing (see Known Gaps) |
| 9 | 6 — Multi-agent | Power feature; requires child workflow architecture |
| 10 | 9 — Advanced | Memory, code review, realtime |
| 11 | 10 — Hardening | Before any real deployment |

## Summary

**Completed:** 34 of 60 items (57%)
**Remaining:** 26 items across phases 1–10

**Medium priority (new infrastructure needed):**
- Multi-agent child workflows (23, 23b), MCP resources (27c), history access (22b), undo/rollback (22c), config reload (17b)

**Lower priority (large scope or niche):**
- Realtime conversation (39d), memory system (34), skills (28, 29), JS REPL (9), worker sandbox (13), real auth (15), code review (39)

**Note on agent definitions:** The codex protocol does not support inline agent definitions (passing model/instructions/tools at spawn time). Agents are configured exclusively through the role system — built-in roles (`default`, `explorer`, `worker`, `awaiter`) or user-defined roles in `config.toml` with role config files.

**Note on agent roles:** Built-in and user-defined roles have identical capabilities — both are `AgentRoleConfig { description, config_file }` resolved via `apply_role_to_config()`. User-defined roles take lookup priority and can override built-in roles by name. Current built-in roles have minimal config: `explorer.toml` is empty (0 bytes), `worker` has no config file, `awaiter` is commented out. The role system is purely a config-layering mechanism with no privileged code paths for built-in roles.
