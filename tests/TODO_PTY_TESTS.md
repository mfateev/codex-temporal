# PTY TUI E2E Test Coverage

## Tested

- **Startup + shutdown** — `tui_startup_and_shutdown`
- **Session reconnect** — `tui_session_reconnect` (`--resume`)
- **Session switch** — `tui_session_switch` (`/session` command)
- **Tool approval (exec_command)** — `tui_tool_approval` (verifies ExecApproval signal reaches workflow)
- **request_user_input** — `tui_request_user_input` (verifies round-trip: overlay → Enter → model responds). Bug fix: workflow matched `Op::UserInputAnswer.id` against `call_id`, but TUI overlay sends `turn_id` (matching upstream codex-core's `notify_user_input_response` which uses `sub_id`/`turn_id`).
- **Smoke test** — `tui_session_smoke_test` (non-PTY, direct session API)
- **Tool approval (direct API)** — `tui_tool_approval_test` (non-PTY, direct session API)

## Not Tested

### High Priority (same thread_id bug pattern)

1. **`apply_patch` approval** — Model writes a file via `apply_patch`, user approves the patch. Same `thread_id.unwrap_or_default()` pattern at `chatwidget.rs:2981`. The `submit_op_to_thread` fix should cover this, but needs verification.

### Medium Priority

3. **Esc to interrupt a turn** — User presses Esc while the model is working. Should send `Op::Interrupt`, workflow returns `TurnAborted`, TUI shows interrupted state and accepts new input.

4. **Multi-turn conversation** — Send a prompt, wait for response, send another prompt. Verifies the session stays alive across turns and the second turn completes correctly.

5. **Error recovery** — Model call fails (e.g. quota exceeded), TUI shows error message and still accepts the next prompt without hanging.

### Lower Priority

6. **`/compact`** — Context compaction triggers continue-as-new. Session survives the CAN and subsequent turns work.

7. **MCP tool execution** — If MCP servers are configured, tool calls route through `mcp_tool_call` activity and return results.

8. **Dynamic tools** — Client-defined tools dispatched via `DynamicToolCallRequest` event and `Op::DynamicToolResponse` signal.

9. **Elicitation** — MCP elicitation flow: server requests user input during tool call, user responds. Same `thread_id.unwrap_or_default()` pattern at `chatwidget.rs:3004`.
