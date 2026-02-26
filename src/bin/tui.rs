//! Temporal-backed TUI for the Codex agent.
//!
//! **NOTE**: This binary is temporarily disabled. The upstream `codex-tui` crate
//! made the `resume_picker` and `tui` modules private and removed the
//! `run_with_session` entry point. This binary needs to be rewritten against
//! the new `codex_tui::run_main` API.

fn main() {
    eprintln!(
        "codex-temporal-tui is temporarily disabled.\n\
         The upstream codex-tui API has changed and this binary needs updating.\n\
         Use `codex-temporal-client` for now."
    );
    std::process::exit(1);
}
