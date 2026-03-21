//! The `CodexHarness` workflow — a long-lived, per-user session registry.
//!
//! This workflow maintains a list of known sessions (started via
//! `CodexWorkflow`) and exposes queries to list/get them and signals to
//! register/update/remove entries.  It does no activities; it's purely
//! a state container.
//!
//! Workflow ID convention: `codex-harness-<user>`.

use std::time::Duration;

use temporalio_common::protos::coresdk::workflow_commands::ContinueAsNewWorkflowExecution;
use temporalio_common::protos::coresdk::AsJsonPayloadExt;
use temporalio_macros::{workflow, workflow_methods};
use temporalio_sdk::{
    ActivityOptions, SyncWorkflowContext, WorkflowContext, WorkflowContextView, WorkflowResult,
    WorkflowTermination,
};

use crate::activities::CodexActivities;
use crate::types::{HarnessInput, HarnessState, SessionEntry, SessionStatus};

#[workflow]
pub struct CodexHarness {
    sessions: Vec<SessionEntry>,
    credentials_available: Option<bool>,
}

#[workflow_methods]
impl CodexHarness {
    #[init]
    pub fn new(_ctx: &WorkflowContextView, input: HarnessInput) -> Self {
        let (sessions, credentials_available) = match input.continued_state {
            Some(s) => (s.sessions, s.credentials_available),
            None => (Vec::new(), None),
        };
        Self {
            sessions,
            credentials_available,
        }
    }

    // ----- signals -----

    /// Register (or upsert) a session entry.
    #[signal]
    pub fn register_session(&mut self, _ctx: &mut SyncWorkflowContext<Self>, entry: SessionEntry) {
        if let Some(existing) = self
            .sessions
            .iter_mut()
            .find(|s| s.session_id == entry.session_id)
        {
            *existing = entry;
        } else {
            self.sessions.push(entry);
        }
    }

    /// Update the status of an existing session.
    #[signal]
    pub fn update_session_status(
        &mut self,
        _ctx: &mut SyncWorkflowContext<Self>,
        payload: (String, SessionStatus),
    ) {
        let (session_id, status) = payload;
        if let Some(s) = self
            .sessions
            .iter_mut()
            .find(|s| s.session_id == session_id)
        {
            s.status = status;
        }
    }

    /// Remove a session from the registry.
    #[signal]
    pub fn remove_session(&mut self, _ctx: &mut SyncWorkflowContext<Self>, session_id: String) {
        self.sessions.retain(|s| s.session_id != session_id);
    }

    // ----- queries -----

    /// Return all sessions as a JSON array.
    #[query]
    pub fn list_sessions(&self, _ctx: &WorkflowContextView) -> String {
        serde_json::to_string(&self.sessions).unwrap_or_default()
    }

    /// Return a single session entry by ID, or `"null"`.
    #[query]
    pub fn get_session(&self, _ctx: &WorkflowContextView, session_id: String) -> String {
        let entry = self.sessions.iter().find(|s| s.session_id == session_id);
        serde_json::to_string(&entry).unwrap_or_else(|_| "null".to_string())
    }

    /// Whether the worker has API credentials available.
    #[query]
    pub fn credentials_available(&self, _ctx: &WorkflowContextView) -> bool {
        self.credentials_available.unwrap_or(false)
    }

    // ----- run -----

    /// The harness sits idle, maintaining state via signals/queries.
    /// It triggers continue-as-new when the server suggests it (to keep
    /// history bounded).
    #[run]
    pub async fn run(ctx: &mut WorkflowContext<Self>) -> WorkflowResult<()> {
        // Check worker credentials once at startup (skipped on CAN if already known).
        if ctx.state(|s| s.credentials_available.is_none()) {
            let cred_result: Result<bool, _> = ctx
                .start_activity(
                    CodexActivities::check_credentials,
                    (),
                    ActivityOptions {
                        schedule_to_close_timeout: Some(Duration::from_secs(10)),
                        ..Default::default()
                    },
                )
                .await;
            ctx.state_mut(|s| s.credentials_available = Some(cred_result.unwrap_or(false)));
        }

        // Wait until the server suggests CAN (history too large).
        ctx.wait_condition(|_s| ctx.continue_as_new_suggested())
            .await;

        // Carry sessions and credentials forward.
        let (sessions, credentials_available) =
            ctx.state(|s| (s.sessions.clone(), s.credentials_available));
        let can_input = HarnessInput {
            continued_state: Some(HarnessState {
                sessions,
                credentials_available,
            }),
        };

        Err(WorkflowTermination::continue_as_new(
            ContinueAsNewWorkflowExecution {
                arguments: vec![can_input.as_json_payload().map_err(|e| {
                    WorkflowTermination::failed(anyhow::anyhow!(
                        "failed to serialize CAN input: {e}"
                    ))
                })?],
                ..Default::default()
            },
        ))
    }
}

/// Re-export the macro-generated `Run` marker type so other modules can
/// parameterize `WorkflowHandle<Client, CodexHarnessRun>`.
pub use codex_harness::Run as CodexHarnessRun;
