//! Background watcher that calls `get_state_update` in a loop.
//!
//! Each call blocks server-side until the workflow has new events, then
//! returns them.  This replaces the old client-side query + exponential
//! backoff polling loop.

use std::time::Duration;

use codex_protocol::protocol::Event;
use temporalio_client::{Client, WorkflowExecuteUpdateOptions};
use tokio::sync::mpsc;

use crate::types::{StateUpdateRequest, StateUpdateResponse};
use crate::workflow::{AgentWorkflow, AgentWorkflowRun};

/// Result of a single watch cycle.
pub enum WatcherEvent {
    /// New events arrived.
    Events(Vec<Event>, usize /* watermark */, u64 /* version */),
    /// Workflow completed — client should stop.
    Completed,
    /// Unrecoverable error after retries.
    Error(String),
}

/// Background watcher that receives events from an `AgentWorkflow` via the
/// blocking `get_state_update` update handler.
pub struct Watcher {
    client: Client,
    workflow_id: String,
}

const MAX_CONSECUTIVE_ERRORS: u32 = 3;

impl Watcher {
    pub fn new(client: Client, workflow_id: String) -> Self {
        Self {
            client,
            workflow_id,
        }
    }

    /// Single blocking call.  Returns when the workflow has new state.
    async fn watch(
        &self,
        since_index: usize,
        since_version: u64,
    ) -> Result<StateUpdateResponse, String> {
        let handle = self
            .client
            .get_workflow_handle::<AgentWorkflowRun>(&self.workflow_id);

        let resp: StateUpdateResponse = handle
            .execute_update(
                AgentWorkflow::get_state_update,
                StateUpdateRequest {
                    since_index,
                    since_version,
                },
                WorkflowExecuteUpdateOptions::default(),
            )
            .await
            .map_err(|e| format!("update failed: {e}"))?;

        Ok(resp)
    }

    /// Continuous loop.  Sends parsed events on `tx`.  Each iteration blocks
    /// until the workflow has new data (no polling, no backoff for success).
    /// Returns when the workflow completes or after MAX_CONSECUTIVE_ERRORS.
    pub async fn run_watching(self, tx: mpsc::Sender<WatcherEvent>) {
        let mut since_index: usize = 0;
        let mut since_version: u64 = 0;
        let mut consecutive_errors: u32 = 0;

        loop {
            match self.watch(since_index, since_version).await {
                Ok(resp) => {
                    consecutive_errors = 0;
                    since_index = resp.watermark;
                    since_version = resp.state_version;
                    let completed = resp.completed;

                    // Parse JSON event strings into Event objects.
                    let events: Vec<Event> = resp
                        .events
                        .iter()
                        .filter_map(|s| serde_json::from_str(s).ok())
                        .collect();

                    if !events.is_empty() || completed {
                        if tx
                            .send(WatcherEvent::Events(events, since_index, since_version))
                            .await
                            .is_err()
                        {
                            return; // receiver dropped
                        }
                    }

                    if completed {
                        let _ = tx.send(WatcherEvent::Completed).await;
                        return;
                    }
                }
                Err(e) => {
                    consecutive_errors += 1;
                    tracing::warn!(
                        error = %e,
                        consecutive = consecutive_errors,
                        "watcher: update call failed"
                    );
                    if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                        let _ = tx.send(WatcherEvent::Error(e)).await;
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
        }
    }
}
