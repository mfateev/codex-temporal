//! Temporal durable execution harness for the OpenAI Codex CLI agent.
//!
//! This crate provides a Temporal-backed implementation of the `codex_core`
//! extension traits (`AgentSession`, `ToolCallHandler`, etc.) so that
//! Codex agent workflows can survive process failures and be replayed
//! deterministically using Temporal's workflow engine.

pub mod session;
