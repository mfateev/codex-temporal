//! Background watcher that calls `get_state_update` in a loop.
//!
//! Each call blocks server-side until the workflow has new events, then
//! returns them.  This replaces the old client-side query + exponential
//! backoff polling loop.
//!
//! ## SDK bug workaround: "Update poll returned no outcome"
//!
//! The Temporal server uses a ~20-second long-poll interval for update
//! RPCs.  `execute_update` internally calls `UpdateWorkflowExecution`
//! (which blocks for one ~20s long poll), and if the update hasn't
//! completed yet, falls through to `PollWorkflowExecutionUpdate` in
//! `get_result()` (one more ~20s long poll).  If the update *still*
//! hasn't completed after those two cycles (~40s total), `get_result()`
//! returns `Err("Update poll returned no outcome")` instead of retrying.
//!
//! This is a bug in the Rust SDK (`WorkflowUpdateHandle::get_result` in
//! `crates/client/src/workflow_handle.rs` — it calls
//! `poll_workflow_execution_update` once and gives up).
//! Tracked in: <https://github.com/temporalio/sdk-core/issues/1149>
//!
//! Until the SDK is fixed, we catch this specific error and retry
//! `execute_update` with the same `update_id` so the server
//! deduplicates and we rejoin the in-flight update.

use std::time::Duration;

use codex_protocol::protocol::Event;
use temporalio_client::{Client, WorkflowExecuteUpdateOptions};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::types::{StateUpdateRequest, StateUpdateResponse};
use crate::workflow::{AgentWorkflow, AgentWorkflowRun};

/// Result of a single watch cycle.
pub enum WatcherEvent {
    /// New events arrived.
    Events(Vec<Event>, usize /* watermark */),
    /// Workflow completed — client should stop.
    Completed,
    /// Connection error — watcher keeps retrying, session can show status.
    Error(String),
}

/// Background watcher that receives events from an `AgentWorkflow` via the
/// blocking `get_state_update` update handler.
pub struct Watcher {
    client: Client,
    workflow_id: String,
}

/// How long a single watch cycle may block before we give up and retry
/// from scratch.  Prevents indefinite hangs when the worker is down and
/// the workflow cannot make progress.
const WATCH_TIMEOUT: Duration = Duration::from_secs(3600);

impl Watcher {
    pub fn new(client: Client, workflow_id: String) -> Self {
        Self {
            client,
            workflow_id,
        }
    }

    /// Single blocking call.  Returns when the workflow has new state or
    /// after `WATCH_TIMEOUT` — whichever comes first.
    ///
    /// Retries on "no outcome" errors (see module-level doc for details).
    async fn watch(
        &self,
        since_index: usize,
        update_id: &str,
    ) -> Result<StateUpdateResponse, String> {
        let deadline = tokio::time::Instant::now() + WATCH_TIMEOUT;

        loop {
            let handle = self
                .client
                .get_workflow_handle::<AgentWorkflowRun>(&self.workflow_id);

            let fut = handle.execute_update(
                AgentWorkflow::get_state_update,
                StateUpdateRequest { since_index },
                WorkflowExecuteUpdateOptions::builder()
                    .update_id(update_id.to_string())
                    .build(),
            );

            match tokio::time::timeout_at(deadline, fut).await {
                Ok(Ok(resp)) => return Ok(resp),
                Ok(Err(e)) => {
                    let msg = format!("{e}");
                    if msg.contains("no outcome") {
                        // Server long-poll expired before the update handler
                        // completed (~40s).  Retry with the same update_id
                        // so the server deduplicates.
                        tracing::debug!("watcher: poll returned no outcome, retrying");
                        continue;
                    }
                    return Err(format!("update failed: {e}"));
                }
                Err(_) => {
                    return Err(
                        "update timed out (workflow may be stalled)".to_string(),
                    );
                }
            }
        }
    }

    /// Continuous loop.  Sends parsed events on `tx`.  Each iteration blocks
    /// until the workflow has new data (no polling, no backoff for success).
    /// Returns only when the workflow completes or the receiver is dropped.
    /// All errors (RPC failures, local timeouts) retry indefinitely.
    pub async fn run_watching(self, tx: mpsc::Sender<WatcherEvent>) {
        let mut since_index: usize = 0;
        // Stable update ID per logical watch call.  Reused on timeout retries
        // so the server deduplicates and we rejoin the in-flight update
        // instead of creating a new handler invocation.
        let mut update_id = Uuid::new_v4().to_string();

        loop {
            match self.watch(since_index, &update_id).await {
                Ok(resp) => {
                    since_index = resp.watermark;
                    let completed = resp.completed;
                    // New logical watch — rotate the update ID.
                    update_id = Uuid::new_v4().to_string();

                    // Parse JSON event strings into Event objects.
                    let events: Vec<Event> = resp
                        .events
                        .iter()
                        .filter_map(|s| serde_json::from_str(s).ok())
                        .collect();

                    if (!events.is_empty() || completed)
                        && tx
                            .send(WatcherEvent::Events(events, since_index))
                            .await
                            .is_err()
                    {
                        return; // receiver dropped
                    }

                    if completed {
                        let _ = tx.send(WatcherEvent::Completed).await;
                        return;
                    }
                }
                Err(e) => {
                    let is_timeout = e.contains("timed out");
                    if is_timeout {
                        tracing::debug!(
                            error = %e,
                            "watcher: update timed out, retrying"
                        );
                    } else {
                        // Non-timeout error — rotate ID so we don't rejoin a
                        // failed update.
                        update_id = Uuid::new_v4().to_string();
                        tracing::warn!(
                            error = %e,
                            "watcher: update call failed, retrying"
                        );
                        // Notify the session so it can show a status message.
                        if tx.send(WatcherEvent::Error(e)).await.is_err() {
                            return; // receiver dropped
                        }
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
            }
        }
    }
}
