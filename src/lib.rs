//! Temporal durable execution harness for the OpenAI Codex CLI agent.
//!
//! This crate provides Temporal-backed implementations of the `codex_core`
//! extension traits (`ModelStreamer`, `ToolCallHandler`, `EventSink`,
//! `StorageBackend`, `RandomSource`, `Clock`) so that the Codex agentic
//! loop can survive process failures and be replayed deterministically
//! using Temporal's workflow engine.

pub mod activities;
pub mod auth_stub;
pub mod entropy;
pub mod models_stub;
pub mod session;
pub mod sink;
pub mod storage;
pub mod streamer;
pub mod tools;
pub mod types;
pub mod workflow;
