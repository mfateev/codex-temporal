//! End-to-end test for the Temporal TUI integration.
//!
//! **TEMPORARILY DISABLED**: The upstream `codex-tui` crate made several
//! modules private (`app_event`, `chatwidget`, `resume_picker`, `tui`,
//! `render`, etc.) and removed the `wire_session` function. These tests
//! need to be rewritten against the new public API when it stabilizes.
//!
//! See git history for the original test implementation.
