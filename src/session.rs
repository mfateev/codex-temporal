//! Temporal-backed [`AgentSession`] implementation.
//!
//! This module will contain the adapter that routes `submit`/`next_event`
//! through Temporal workflow activities.

use async_trait::async_trait;
use codex_core::AgentSession;
use codex_core::protocol::{Event, Op};
use codex_core::error::Result as CodexResult;

/// A Temporal-backed agent session that routes operations through
/// Temporal workflow activities for durable execution.
pub struct TemporalAgentSession {
    // TODO: Temporal workflow handle, channels, etc.
}

#[async_trait]
impl AgentSession for TemporalAgentSession {
    async fn submit(&self, _op: Op) -> CodexResult<String> {
        todo!("dispatch op as Temporal activity")
    }

    async fn next_event(&self) -> CodexResult<Event> {
        todo!("receive event from Temporal workflow")
    }
}
