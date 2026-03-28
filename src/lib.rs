//! Temporal durable execution harness for the OpenAI Codex CLI agent.
//!
//! This crate provides Temporal-backed implementations of the `codex_core`
//! extension traits (`ModelStreamer`, `ToolCallHandler`, `EventSink`,
//! `StorageBackend`, `RandomSource`) so that the Codex agentic
//! loop can survive process failures and be replayed deterministically
//! using Temporal's workflow engine.

pub mod activities;
pub mod config_loader;
pub mod entropy;
pub mod harness;
pub mod mcp;
pub mod picker;
pub mod session;
pub mod session_workflow;
pub mod sink;
#[macro_use]
mod startup;
pub mod storage;
pub mod streamer;
pub mod tools;
pub mod types;
pub mod watcher;
pub mod workflow;

#[cfg(test)]
mod unit_tests;
