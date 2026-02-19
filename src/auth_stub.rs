//! Stub [`AuthProvider`] for the Temporal TUI binary.
//!
//! The Temporal worker holds the API key and makes LLM calls, so the TUI
//! process does not need real authentication. This provider always returns
//! `None` / no-op.

use async_trait::async_trait;
use codex_core::auth::AuthProvider;
use codex_core::auth::CodexAuth;

/// A no-op auth provider that never has credentials.
pub struct NoopAuthProvider;

#[async_trait]
impl AuthProvider for NoopAuthProvider {
    fn auth_cached(&self) -> Option<CodexAuth> {
        None
    }

    async fn auth(&self) -> Option<CodexAuth> {
        None
    }

    fn reload(&self) -> bool {
        false
    }
}
