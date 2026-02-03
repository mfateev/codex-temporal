//! Temporal activities for Codex operations.
//!
//! Activities are the non-deterministic operations that run outside the
//! workflow sandbox. They can make HTTP calls, access the filesystem, etc.

mod model;

pub use model::{ModelActivityInput, ModelActivityOutput, model_stream_activity};
