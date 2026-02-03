//! Temporal workflow integration for OpenAI Codex.
//!
//! This crate provides the infrastructure to run Codex's agentic loop inside
//! Temporal workflows for durable execution.
//!
//! # Architecture
//!
//! The POC demonstrates running model calls as Temporal activities:
//!
//! ```text
//! ┌────────────────────────────────────────────────────────────────┐
//! │                     Temporal Workflow                           │
//! │                                                                 │
//! │  1. Receive prompt from workflow input                          │
//! │  2. Call model_stream activity                                  │
//! │  3. (Future) Process tool calls via tool_execute activity       │
//! │  4. Return response                                             │
//! │                                                                 │
//! └────────────────────────────────────────────────────────────────┘
//!                              │
//!                              ▼
//! ┌────────────────────────────────────────────────────────────────┐
//! │                        Activities                               │
//! │                                                                 │
//! │  - model_stream: Makes HTTP calls to OpenAI, buffers response   │
//! │  - tool_execute: Executes tool calls (stub for POC)             │
//! │                                                                 │
//! └────────────────────────────────────────────────────────────────┘
//! ```

pub mod activities;
pub mod adapters;
pub mod workflow;

// Re-export key types
pub use activities::{ModelActivityInput, ModelActivityOutput, model_stream_activity};
pub use adapters::entropy::{WorkflowClock, WorkflowRandomSource};
pub use workflow::{CodexWorkflowInput, CodexWorkflowOutput};
