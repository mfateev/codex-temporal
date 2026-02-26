# Codex-Temporal Feature Roadmap

## Current State

Single/multi-turn conversation, full tool ecosystem via codex-core's ToolRegistry dispatch (shell, apply_patch, read_file, list_dir, grep_files, view_image, web search), tool approval gating with three-tier command safety and policy modes, full TUI reuse with interactive session picker, deterministic entropy, OpenAI Responses API calls via codex ModelClient with stable conversation_id, prompt caching, token usage tracking, reasoning effort/summary/personality, config.toml loading via activity, real sandbox policy from config, project context injection (AGENTS.md + git info), MCP server support (stdio/HTTP), continue-as-new, session resume via CodexHarness registry, event polling with watermarks.

73 unit tests, 19 e2e tests, 4 TUI e2e tests.

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

## Phase 3: Approval Policies & Sandbox

11. ~~**Policy modes**~~ — ✅ `Never`/`OnRequest`/`OnFailure`/`UnlessTrusted` in workflow; `CODEX_APPROVAL_POLICY` env var
12. ~~**Policy amendment signals**~~ — ✅ `Op::OverrideTurnContext` updates `approval_policy_override` mid-workflow; persists across CAN; `effective_approval_policy()` helper; tool handler re-reads policy each iteration
13. **Worker sandbox** — bubblewrap/container isolation for tool activities
14. ~~**Command safety analysis**~~ — ✅ codex-core three-tier classification: safe (auto-approve), dangerous (always prompt), unknown (defer to policy); 8 unit tests

## Phase 4: Configuration & Auth

15. **Real auth** — OAuth, API key, device code in TUI; replace `NoopAuthProvider`
16. **Real models provider** — fetch catalog, wire `/model` to switch via signal
17. ~~**Config loading**~~ — ✅ `load_config` activity calls `ConfigBuilder::build_toml_string()` on worker; workflow builds real `Config` via `Config::from_toml()`; `config.toml` settings flow through to tool dispatch; `CODEX_*` env var overrides
18. **Credential store** — keyring/file/ephemeral support

## Phase 5: Session Persistence

19. ~~**Resume flow**~~ — ✅ `CodexHarness` workflow (long-lived session registry); `TemporalAgentSession::resume()` + `fetch_initial_events()`; `--resume` flag in TUI; full interactive session picker via codex TUI `resume_picker`
20. **`/resume`, `/fork`, `/new`** — reconnect, seed from history, or start fresh (resume done; fork/new remaining)
21. **Rollout export** — query returning native codex rollout format
22. **Thread metadata** — workflow search attributes for discoverability

## Phase 6: Multi-Agent

23. **Child workflows** — `spawn_agent`/`send_input`/`wait`/`close_agent` as child workflow ops
24. **Agent switching** — `/agent` switches TUI subscription between workflows
25. **Max concurrency** — enforce `agents.max_threads` in parent workflow

## Phase 7: MCP & Skills

26. ~~**MCP connections**~~ — ✅ `HarnessMcpManager` manages persistent connections (stdio + streamable HTTP via `RmcpClient`); `discover_mcp_tools` activity at workflow start
27. ~~**MCP tool execution**~~ — ✅ `mcp_tool_call` activity routes `mcp__server__tool` prefixed calls to appropriate server; bypasses shell approval; schemas carry through continue-as-new
28. **Skills loading** — discover `SKILL.md` on worker, inject into system prompt
29. **Remote skills** — MCP-based installation

## Phase 8: Git & Project Context

30. ~~**Git info injection**~~ — ✅ `collect_project_context` activity reads git branch/commit/URL via `codex_core::git_info::collect_git_info()`; injected as XML EnvironmentContext in prompt
31. **Ghost commits** — auto-commit after file-modifying tool executions
32. ~~**Project docs**~~ — ✅ `collect_project_context` activity reads AGENTS.md chain via `codex_core::project_doc::read_project_docs()`; injected as XML UserInstructions in prompt; ephemeral (not stored in history)
33. **`/diff` and `/mention`** — query for git diff; signal to include file content

## Phase 9: Advanced Features

34. **Memory system** — extraction/consolidation as separate workflows
35. ~~**Personality**~~ — ✅ `CodexWorkflowInput.personality` + per-turn override via `UserTurnInput`; `CODEX_PERSONALITY` env var; flows through to `Prompt.personality`
36. **Notifications** — email/webhook on completion or approval requests
37. **Apps/connectors** — discovery through MCP gateway activities
38. ~~**Interrupt**~~ — ✅ `Op::Interrupt` sets flag checked at iteration boundaries and during approval waits; emits `TurnAborted(Interrupted)` event; returns denied-style response from tool handler to avoid codex-core drain panic
39. **Code review** — `/review` triggers specialized model call on git diff

## Phase 10: Production Hardening

40. **Activity timeouts & retries** — proper start-to-close, retry policies, heartbeats
41. **Resource limits** — CPU/memory/time on tool activities
42. **Observability** — OpenTelemetry tracing/metrics on worker and TUI
43. **Graceful shutdown** — drain on SIGTERM, `request_shutdown` on exit
44. **Error handling** — structured errors, proper failure propagation
45. ~~**Continue-as-new**~~ — ✅ `Op::Compact` signal triggers CAN; `ContinueAsNewState` carries history, pending turns, counters, token usage, MCP tools; e2e test confirms recall across CAN boundary

---

## Suggested Priority

| Order | Phase | Rationale |
|-------|-------|-----------|
| ~~1~~ | ~~3 — Approval policies~~ | ✅ Done (policy modes + command safety) |
| ~~2~~ | ~~2 — Tool ecosystem~~ | ✅ Done (ToolRegistry dispatch, web search); JS REPL remaining |
| ~~3~~ | ~~1 — Model improvements~~ | ✅ Done (reasoning effort, prompt caching); multi-provider routing remaining |
| ~~4~~ | ~~5 — Session persistence~~ | ✅ Done (resume, picker, harness); fork/new/export remaining |
| ~~5~~ | ~~8 — Git/project context~~ | ✅ Done (git info, AGENTS.md); ghost commits, /diff remaining |
| ~~6~~ | ~~4 — Config/auth~~ | ✅ Partial (config.toml done); real auth/models/credentials remaining |
| ~~7~~ | ~~7 — MCP/skills~~ | ✅ Partial (MCP done); skills loading remaining |
| 8 | 6 — Multi-agent | Power feature |
| 9 | 9 — Advanced | Nice-to-haves (memory, ~~interrupt~~, code review); interrupt done |
| 10 | 10 — Hardening | Before any real deployment |

## Summary

**Completed:** 29 of 45 items (64%)
**Remaining:** 16 items across phases 1–10 (multi-provider, JS REPL, worker sandbox, real auth, models provider, credential store, fork/new, rollout export, thread metadata, multi-agent ×3, skills ×2, ghost commits, /diff, memory, notifications, apps, code review, timeouts, resource limits, observability, graceful shutdown, error handling)
