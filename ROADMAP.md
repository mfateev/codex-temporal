# Codex-Temporal Feature Roadmap

## Current State (POC)

Single/multi-turn conversation, shell tool execution as durable activities, tool approval gating, full TUI reuse, deterministic entropy, non-streaming OpenAI Responses API calls, event polling with watermarks.

---

## Phase 1: Model & Streaming

1. **Streaming responses** — chunked delivery via activity heartbeats so TUI shows tokens incrementally
2. **Reasoning effort** — pass low/medium/high through to Responses API; wire `/model` command
3. **Multi-provider** — route `model_call` to OpenAI/Ollama/LM Studio/custom based on workflow input
4. **Prompt caching** — cache control headers for long conversations

## Phase 2: Tool Ecosystem

5. **Apply-patch** — register tool spec, activity applies diffs on worker
6. **File read/write** — activities for file ops within sandbox boundary
7. **Search (BM25)** — activity wrapping file search with ranking
8. **View image** — activity returns base64 for model
9. **JS REPL** — persistent Node kernel on worker
10. **Web search** — cached/live modes via activity

## Phase 3: Approval Policies & Sandbox

11. **Policy modes** — `untrusted`/`on-request`/`never` in workflow
12. **Policy amendment signals** — update policies mid-workflow
13. **Worker sandbox** — bubblewrap/container isolation for tool activities
14. **Command safety analysis** — reuse `command_safety` to classify before approval

## Phase 4: Configuration & Auth

15. **Real auth** — OAuth, API key, device code in TUI; replace `NoopAuthProvider`
16. **Real models provider** — fetch catalog, wire `/model` to switch via signal
17. **Config loading** — read `config.toml`, pass settings as workflow input
18. **Credential store** — keyring/file/ephemeral support

## Phase 5: Session Persistence

19. **Resume flow** — reconnect TUI to existing workflow by ID
20. **`/resume`, `/fork`, `/new`** — reconnect, seed from history, or start fresh
21. **Rollout export** — query returning native codex rollout format
22. **Thread metadata** — workflow search attributes for discoverability

## Phase 6: Multi-Agent

23. **Child workflows** — `spawn_agent`/`send_input`/`wait`/`close_agent` as child workflow ops
24. **Agent switching** — `/agent` switches TUI subscription between workflows
25. **Max concurrency** — enforce `agents.max_threads` in parent workflow

## Phase 7: MCP & Skills

26. **MCP connections** — worker manages MCP servers (stdio/SSE), discovers tools at workflow start
27. **MCP tool execution** — dispatched as activities through worker's MCP connections
28. **Skills loading** — discover `SKILL.md` on worker, inject into system prompt
29. **Remote skills** — MCP-based installation

## Phase 8: Git & Project Context

30. **Git info injection** — worker reads git status/diff, injects into prompt
31. **Ghost commits** — auto-commit after file-modifying tool executions
32. **Project docs** — discover `AGENTS.md`/`README.md`, inject into system prompt
33. **`/diff` and `/mention`** — query for git diff; signal to include file content

## Phase 9: Advanced Features

34. **Memory system** — extraction/consolidation as separate workflows
35. **Personality** — pass config through workflow input into system prompt
36. **Notifications** — email/webhook on completion or approval requests
37. **Apps/connectors** — discovery through MCP gateway activities
38. **Interrupt** — signal cancels current activity, returns control to user
39. **Code review** — `/review` triggers specialized model call on git diff

## Phase 10: Production Hardening

40. **Activity timeouts & retries** — proper start-to-close, retry policies, heartbeats
41. **Resource limits** — CPU/memory/time on tool activities
42. **Observability** — OpenTelemetry tracing/metrics on worker and TUI
43. **Graceful shutdown** — drain on SIGTERM, `request_shutdown` on exit
44. **Error handling** — structured errors, proper failure propagation
45. **Continue-as-new** — avoid unbounded history for long conversations

---

## Suggested Priority

| Order | Phase | Rationale |
|-------|-------|-----------|
| 1 | 3 — Approval policies | Biggest usability win; `never` mode removes friction |
| 2 | 2 — Tool ecosystem | Apply-patch alone unlocks most coding workflows |
| 3 | 1 — Streaming | Makes TUI feel responsive |
| 4 | 5 — Session persistence | Resume is critical for real usage |
| 5 | 4 — Config/auth | Needed for multi-user deployment |
| 6 | 8 — Git/project context | Important for coding tasks |
| 7 | 6 — Multi-agent | Power feature |
| 8 | 7 — MCP/skills | Extensibility |
| 9 | 9 — Advanced | Nice-to-haves |
| 10 | 10 — Hardening | Before any real deployment |
