//! [`AgentSession`] implementation backed by a Temporal workflow.
//!
//! `TemporalAgentSession` translates the `submit(Op)` / `next_event()` API
//! into Temporal signals and blocking updates against the active
//! [`AgentWorkflow`] child, while lifecycle operations go to the parent
//! [`SessionWorkflow`].
//!
//! ## Protocol mapping
//!
//! | Op variant        | Temporal action                                      |
//! |-------------------|------------------------------------------------------|
//! | `UserTurn`        | start SessionWorkflow (first) or signal AgentWorkflow |
//! | `Shutdown`        | signal both SessionWorkflow and AgentWorkflow         |
//! | all other Ops     | signal active AgentWorkflow                          |
//!
//! Events are received via a background [`Watcher`] that calls the blocking
//! `get_state_update` update handler in a loop.

use std::sync::{Arc, Mutex};

/// Extension trait to reduce `.get()` boilerplate.
trait MutexExt<T> {
    fn get(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> MutexExt<T> for std::sync::Mutex<T> {
    fn get(&self) -> std::sync::MutexGuard<'_, T> {
        self.lock().expect("lock poisoned")
    }
}

use codex_core::error::{CodexErr, Result as CodexResult};
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::protocol::{
    BackgroundEventEvent, Event, EventMsg, Op, SandboxPolicy, SessionConfiguredEvent,
    TurnAbortReason, TurnAbortedEvent,
};
use temporalio_client::{
    Client, WorkflowExecuteUpdateOptions, WorkflowQueryOptions, WorkflowSignalOptions,
    WorkflowStartOptions,
};
use tokio_util::sync::CancellationToken;

use crate::harness::{CodexHarness, CodexHarnessRun};
use crate::session_workflow::{SessionWorkflow, SessionWorkflowRun};
use crate::types::{SessionWorkflowInput, SpawnAgentInput, StateUpdateRequest, extract_message};
use crate::watcher::{Watcher, WatcherEvent};
use crate::workflow::{AgentWorkflow, AgentWorkflowRun};

const TASK_QUEUE: &str = "codex-temporal";

/// An [`AgentSession`] that backs the TUI with a Temporal workflow.
///
/// Routes events through the active `AgentWorkflow` child, while lifecycle
/// operations (start, shutdown, spawn) go to the parent `SessionWorkflow`.
pub struct TemporalAgentSession {
    client: Client,
    /// Workflow ID of the parent SessionWorkflow (mutable for session switching).
    session_workflow_id: Mutex<String>,
    /// Workflow ID of the currently watched AgentWorkflow.
    active_agent_workflow_id: Mutex<String>,
    /// Session-level input (model, instructions, etc.).
    base_input: SessionWorkflowInput,
    /// Whether the session workflow has been started.
    started: Mutex<bool>,
    /// Local buffer of deserialized events not yet returned to the caller.
    event_buffer: Arc<Mutex<Vec<Event>>>,
    /// Notified when new events are pushed into `event_buffer`.
    event_notify: Arc<tokio::sync::Notify>,
    /// Set when shutdown has been signaled.
    shutdown: Mutex<bool>,
    /// Monotonically increasing generation counter for stale-data detection.
    generation: Arc<Mutex<u64>>,
    /// Handle to cancel the background Watcher when switching sessions.
    watch_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Cancellation token for the in-flight submit retry loop.
    /// Cancelled by `Op::Interrupt` so the user can abort a pending submit.
    submit_cancel: Mutex<Option<CancellationToken>>,
    /// Workflow ID of the CodexHarness (for querying session list).
    harness_workflow_id: String,
}

impl TemporalAgentSession {
    /// Create a new session. The workflow is not started until the first
    /// `Op::UserTurn` is submitted.
    ///
    /// Accepts both `SessionWorkflowInput` and `CodexWorkflowInput` (via
    /// the `Into<SessionWorkflowInput>` conversion).
    pub fn new(
        client: Client,
        workflow_id: String,
        base_input: impl Into<SessionWorkflowInput>,
    ) -> Self {
        Self::new_with_harness(client, workflow_id, base_input, None)
    }

    /// Create a new session with an explicit harness workflow ID.
    ///
    /// When `harness_id` is `None`, the default user-derived ID is used.
    pub fn new_with_harness(
        client: Client,
        workflow_id: String,
        base_input: impl Into<SessionWorkflowInput>,
        harness_id: Option<String>,
    ) -> Self {
        let base_input = base_input.into();
        let agent_workflow_id = format!("{workflow_id}/main");
        let harness_workflow_id = harness_id.unwrap_or_else(derive_harness_workflow_id);
        Self {
            client,
            session_workflow_id: Mutex::new(workflow_id),
            active_agent_workflow_id: Mutex::new(agent_workflow_id),
            base_input,
            started: Mutex::new(false),
            event_buffer: Arc::new(Mutex::new(Vec::new())),
            event_notify: Arc::new(tokio::sync::Notify::new()),
            shutdown: Mutex::new(false),
            generation: Arc::new(Mutex::new(0)),
            watch_handle: Mutex::new(None),
            submit_cancel: Mutex::new(None),
            harness_workflow_id,
        }
    }

    /// Attach to an existing running workflow.
    ///
    /// Unlike `new()`, the workflow is assumed to already be running, so the
    /// first `Op::UserTurn` will be signalled rather than starting a workflow.
    ///
    /// Accepts both `SessionWorkflowInput` and `CodexWorkflowInput`.
    pub fn resume(
        client: Client,
        session_id: String,
        base_input: impl Into<SessionWorkflowInput>,
    ) -> Self {
        Self::resume_with_harness(client, session_id, base_input, None)
    }

    /// Attach to an existing running workflow with an explicit harness workflow ID.
    pub fn resume_with_harness(
        client: Client,
        session_id: String,
        base_input: impl Into<SessionWorkflowInput>,
        harness_id: Option<String>,
    ) -> Self {
        let base_input = base_input.into();
        let agent_workflow_id = format!("{session_id}/main");
        let harness_workflow_id = harness_id.unwrap_or_else(derive_harness_workflow_id);
        let session = Self {
            client,
            session_workflow_id: Mutex::new(session_id),
            active_agent_workflow_id: Mutex::new(agent_workflow_id),
            base_input,
            started: Mutex::new(true), // already running
            event_buffer: Arc::new(Mutex::new(Vec::new())),
            event_notify: Arc::new(tokio::sync::Notify::new()),
            shutdown: Mutex::new(false),
            generation: Arc::new(Mutex::new(0)),
            watch_handle: Mutex::new(None),
            submit_cancel: Mutex::new(None),
            harness_workflow_id,
        };
        session.start_watching();
        session
    }

    /// Return the session workflow ID.
    pub fn session_id(&self) -> String {
        self.session_workflow_id
            .lock()
            .expect("lock poisoned")
            .clone()
    }

    /// Build a synthetic `SessionConfigured` event from `base_input`.
    ///
    /// The Temporal workflow does not emit this event itself; instead the
    /// client-side session injects it into the event buffer so that
    /// consumers (TUI, tests) see the same protocol as codex-core.
    fn build_session_configured_event(&self, input: &SessionWorkflowInput) -> Event {
        use codex_protocol::ThreadId;
        Event {
            id: String::new(),
            msg: EventMsg::SessionConfigured(SessionConfiguredEvent {
                session_id: ThreadId::new(),
                forked_from_id: None,
                thread_name: None,
                initial_messages: None,
                model: input.model.clone(),
                model_provider_id: "openai".into(),
                approvals_reviewer: Default::default(),
                approval_policy: input.approval_policy,
                sandbox_policy: SandboxPolicy::DangerFullAccess,
                cwd: std::env::current_dir().unwrap_or_default(),
                reasoning_effort: input.reasoning_effort,
                history_log_id: 0,
                history_entry_count: 0,
                network_proxy: None,
                rollout_path: None,
                service_tier: None,
            }),
        }
    }

    /// Return the active agent workflow ID.
    pub fn active_agent_id(&self) -> String {
        self.active_agent_workflow_id
            .lock()
            .expect("lock poisoned")
            .clone()
    }

    /// Switch the active agent to a different child workflow.
    ///
    /// Stops the current watcher, clears the buffer, and starts watching
    /// the new agent.
    pub fn switch_agent(&self, agent_workflow_id: String) {
        self.stop_watching();
        *self
            .active_agent_workflow_id
            .lock()
            .expect("lock poisoned") = agent_workflow_id;
        self.event_buffer.get().clear();
        self.start_watching();
    }

    /// Switch to a different session entirely.
    ///
    /// Bumps the generation counter (so in-flight data is discarded),
    /// resets all internal state to point at the new session, and starts
    /// a new watcher.
    pub fn switch_session(&self, new_session_id: String) {
        self.stop_watching();
        *self.generation.get() += 1;
        *self.session_workflow_id.get() = new_session_id.clone();
        *self.active_agent_workflow_id.get() =
            format!("{new_session_id}/main");
        self.event_buffer.get().clear();
        *self.started.get() = true;
        self.start_watching();
    }

    /// Start the background watcher for the active agent workflow.
    fn start_watching(&self) {
        self.stop_watching();
        let client = self.client.clone();
        let workflow_id = self.active_agent_id();
        let buffer = Arc::clone(&self.event_buffer);
        let notify = Arc::clone(&self.event_notify);
        let generation = Arc::clone(&self.generation);
        let gen_at_start = *generation.get();

        let handle = tokio::spawn(async move {
            let watcher = Watcher::new(client, workflow_id);
            let (tx, mut rx) = tokio::sync::mpsc::channel(64);

            tokio::spawn(async move {
                watcher.run_watching(tx).await;
            });

            drain_watcher_into_buffer(&mut rx, &buffer, &notify, &generation, gen_at_start).await;
        });

        *self.watch_handle.get() = Some(handle);
    }

    /// Stop the background watcher.
    fn stop_watching(&self) {
        if let Some(handle) = self.watch_handle.get().take() {
            handle.abort();
        }
    }

    /// Query the harness for the list of known sessions.
    pub async fn query_sessions(&self) -> CodexResult<Vec<crate::types::SessionEntry>> {
        let handle = self
            .client
            .get_workflow_handle::<CodexHarnessRun>(&self.harness_workflow_id);

        let json: String = handle
            .query(
                CodexHarness::list_sessions,
                (),
                WorkflowQueryOptions::default(),
            )
            .await
            .map_err(|e| CodexErr::Fatal(format!("failed to query sessions: {e}")))?;

        let entries: Vec<crate::types::SessionEntry> =
            serde_json::from_str(&json).unwrap_or_default();
        Ok(entries)
    }

    /// Signal the SessionWorkflow to spawn a new agent.
    pub async fn spawn_agent(&self, input: SpawnAgentInput) -> CodexResult<()> {
        let session_id = self.session_id();
        let handle = self
            .client
            .get_workflow_handle::<SessionWorkflowRun>(&session_id);

        handle
            .signal(
                SessionWorkflow::spawn_agent,
                input,
                WorkflowSignalOptions::default(),
            )
            .await
            .map_err(|e| CodexErr::Fatal(format!("failed to signal spawn_agent: {e}")))?;

        Ok(())
    }

    /// Fetch all existing events from the workflow via a one-shot
    /// `get_state_update` call.
    ///
    /// Call **before** launching the TUI to populate `initial_messages` so
    /// the user sees the full conversation history immediately on resume.
    pub async fn fetch_initial_events(&self) -> CodexResult<Vec<EventMsg>> {
        let agent_id = self.active_agent_id();
        let handle = self
            .client
            .get_workflow_handle::<AgentWorkflowRun>(&agent_id);

        let resp = match handle
            .execute_update(
                AgentWorkflow::get_state_update,
                StateUpdateRequest {
                    since_index: 0,
                },
                WorkflowExecuteUpdateOptions::default(),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                // Workflow may have already completed (e.g. reconnecting to a
                // session that was shut down).  Return empty history so the TUI
                // starts fresh rather than crashing.
                let msg = e.to_string().to_lowercase();
                if msg.contains("not found") || msg.contains("already completed") {
                    return Ok(vec![]);
                }
                return Err(CodexErr::Fatal(format!("failed to fetch initial events: {e}")));
            }
        };

        let events: Vec<Event> = resp
            .events
            .iter()
            .filter_map(|s| serde_json::from_str(s).ok())
            .collect();

        Ok(events.into_iter().map(|e| e.msg).collect())
    }

    /// Signal an operation to the active agent workflow.
    async fn signal_agent_op(&self, op: Op) -> CodexResult<String> {
        let agent_id = self.active_agent_id();
        let handle = self
            .client
            .get_workflow_handle::<AgentWorkflowRun>(&agent_id);

        handle
            .signal(
                AgentWorkflow::receive_op,
                op,
                WorkflowSignalOptions::default(),
            )
            .await
            .map_err(|e| CodexErr::Fatal(format!("failed to signal op to agent: {e}")))?;

        Ok("ok".to_string())
    }

}

/// Drain watcher events into a shared buffer until the channel closes,
/// the workflow completes, or the generation counter changes (indicating
/// the session has been switched).
async fn drain_watcher_into_buffer(
    rx: &mut tokio::sync::mpsc::Receiver<WatcherEvent>,
    buffer: &Arc<Mutex<Vec<Event>>>,
    notify: &Arc<tokio::sync::Notify>,
    generation: &Arc<Mutex<u64>>,
    gen_at_start: u64,
) {
    let mut stall_notified = false;
    while let Some(result) = rx.recv().await {
        if *generation.get() != gen_at_start {
            return;
        }
        match result {
            WatcherEvent::Events(events, _) => {
                stall_notified = false;
                if !events.is_empty() {
                    buffer.get().extend(events);
                    notify.notify_one();
                }
            }
            WatcherEvent::Completed => {
                buffer.get().push(Event {
                    id: String::new(),
                    msg: EventMsg::ShutdownComplete,
                });
                notify.notify_one();
                return;
            }
            WatcherEvent::Error(e) => {
                tracing::warn!(error = %e, "watcher: connection error, retrying");
                if !stall_notified {
                    stall_notified = true;
                    buffer.get().push(Event {
                        id: String::new(),
                        msg: EventMsg::BackgroundEvent(BackgroundEventEvent {
                            message: "Connecting to backend...".to_string(),
                        }),
                    });
                    notify.notify_one();
                }
            }
        }
    }
}

/// Derive the harness workflow ID for the current user.
fn derive_harness_workflow_id() -> String {
    let user = std::env::var("USER").unwrap_or_else(|_| "default".to_string());
    format!("codex-harness-{user}")
}

/// Filter replayed events into the set suitable for `initial_messages`.
pub fn filter_initial_events(events: Vec<EventMsg>) -> Vec<EventMsg> {
    events
        .into_iter()
        .filter(|e| {
            matches!(
                e,
                EventMsg::AgentMessage(_)
                    | EventMsg::TurnStarted(_)
                    | EventMsg::TurnComplete(_)
                    | EventMsg::ExecApprovalRequest(_)
                    | EventMsg::ExecCommandBegin(_)
            )
        })
        .collect()
}

#[async_trait::async_trait]
impl codex_core::AgentSession for TemporalAgentSession {
    async fn submit(&self, op: Op) -> CodexResult<String> {
        // --- Op::Interrupt: immediate abort back to prompt ---
        if matches!(op, Op::Interrupt) {
            // Cancel any in-flight submit retry loop.
            if let Some(token) = self.submit_cancel.get().take() {
                token.cancel();
            }
            // Push synthetic TurnAborted(Interrupted) so the TUI returns to the prompt.
            {
                let mut buf = self.event_buffer.get();
                buf.push(Event {
                    id: String::new(),
                    msg: EventMsg::TurnAborted(TurnAbortedEvent {
                        turn_id: None,
                        reason: TurnAbortReason::Interrupted,
                    }),
                });
            }
            self.event_notify.notify_one();
            // Fire-and-forget signal to workflow (best effort).
            let op_clone = op.clone();
            let client = self.client.clone();
            let agent_id = self.active_agent_id();
            tokio::spawn(async move {
                let handle = client.get_workflow_handle::<AgentWorkflowRun>(&agent_id);
                let _ = handle
                    .signal(
                        AgentWorkflow::receive_op,
                        op_clone,
                        WorkflowSignalOptions::default(),
                    )
                    .await;
            });
            return Ok("ok".to_string());
        }

        // --- First UserTurn starts the SessionWorkflow; subsequent ops go to agent ---
        if let Op::UserTurn {
            ref items,
            effort,
            summary,
            personality,
            ..
        } = op
        {
            let started = *self.started.get();
            if !started {
                let message = extract_message(items);
                // Retry start_workflow in a background task with cancellation.
                let cancel = CancellationToken::new();
                *self.submit_cancel.get() = Some(cancel.clone());

                let client = self.client.clone();
                let session_id = self.session_id();
                let base_input = self.base_input.clone();
                let buffer = Arc::clone(&self.event_buffer);
                let notify = Arc::clone(&self.event_notify);

                // Build the input once.
                let input = SessionWorkflowInput {
                    user_message: message,
                    model: base_input.model.clone(),
                    instructions: base_input.instructions.clone(),
                    approval_policy: base_input.approval_policy,
                    web_search_mode: base_input.web_search_mode,
                    reasoning_effort: effort.or(base_input.reasoning_effort),
                    reasoning_summary: if summary.unwrap_or_default()
                        != ReasoningSummary::default()
                    {
                        summary.unwrap_or_default()
                    } else {
                        base_input.reasoning_summary
                    },
                    personality: personality.or(base_input.personality),
                    developer_instructions: base_input.developer_instructions.clone(),
                    model_provider: base_input.model_provider.clone(),
                    crew_agents: base_input.crew_agents.clone(),
                    continued_state: None,
                    max_iterations: base_input.max_iterations,
                };

                // Try once synchronously first.
                let options =
                    WorkflowStartOptions::new(TASK_QUEUE, &session_id).build();
                match client
                    .start_workflow(SessionWorkflow::run, input.clone(), options)
                    .await
                {
                    Ok(_) => {
                        *self.started.get() = true;
                        *self.submit_cancel.get() = None;
                        // Inject SessionConfigured before the watcher starts
                        // so it is the first event consumers see.
                        {
                            let evt = self.build_session_configured_event(&input);
                            self.event_buffer.get().push(evt);
                            self.event_notify.notify_one();
                        }
                        self.start_watching();
                        tracing::info!(
                            session_workflow_id = %session_id,
                            "session workflow started"
                        );
                        return Ok("started".to_string());
                    }
                    Err(first_err) => {
                        tracing::warn!(error = %first_err, "start_workflow failed, retrying in background");
                        // Push connecting status.
                        {
                            let mut buf = buffer.get();
                            buf.push(Event {
                                id: String::new(),
                                msg: EventMsg::BackgroundEvent(BackgroundEventEvent {
                                    message: "Connecting to backend...".to_string(),
                                }),
                            });
                        }
                        notify.notify_one();

                        // Spawn retry loop.
                        let cancel2 = cancel.clone();
                        let client2 = client.clone();
                        let session_id2 = session_id.clone();
                        let buffer2 = Arc::clone(&self.event_buffer);
                        let notify2 = Arc::clone(&self.event_notify);
                        let session_configured_evt = self.build_session_configured_event(&input);
                        let generation = Arc::clone(&self.generation);
                        let gen_at_start = *generation.get();
                        let active_agent_id = self.active_agent_id();

                        tokio::spawn(async move {
                            let mut delay = std::time::Duration::from_millis(500);
                            let max_delay = std::time::Duration::from_secs(5);

                            loop {
                                tokio::select! {
                                    _ = cancel2.cancelled() => {
                                        tracing::debug!("submit retry loop cancelled by interrupt");
                                        return;
                                    }
                                    _ = tokio::time::sleep(delay) => {}
                                }

                                // Check generation for staleness.
                                if *generation.get() != gen_at_start {
                                    return;
                                }

                                let options =
                                    WorkflowStartOptions::new(TASK_QUEUE, &session_id2).build();
                                match client2
                                    .start_workflow(SessionWorkflow::run, input.clone(), options)
                                    .await
                                {
                                    Ok(_) => {
                                        tracing::debug!(
                                            session_workflow_id = %session_id2,
                                            "session workflow started (after retry)"
                                        );
                                        // Inject SessionConfigured before the watcher starts.
                                        {
                                            buffer2.get().push(session_configured_evt);
                                            notify2.notify_one();
                                        }
                                        // Start a watcher inline since we can't call self.start_watching().
                                        let watcher = Watcher::new(client2.clone(), active_agent_id.clone());
                                        let (wtx, mut wrx) = tokio::sync::mpsc::channel(64);
                                        tokio::spawn(async move {
                                            watcher.run_watching(wtx).await;
                                        });
                                        drain_watcher_into_buffer(&mut wrx, &buffer2, &notify2, &generation, gen_at_start).await;
                                        return;
                                    }
                                    Err(e) => {
                                        tracing::warn!(error = %e, "start_workflow retry failed");
                                        delay = (delay * 2).min(max_delay);
                                    }
                                }
                            }
                        });

                        // Return immediately — the retry loop runs in the background.
                        // We need to set `started` to true so subsequent UserTurns
                        // go through signal_agent_op instead of trying to start again.
                        *self.started.get() = true;
                        return Ok("started".to_string());
                    }
                }
            } else {
                // Already started — retry signal_agent_op for UserTurn.
                let cancel = CancellationToken::new();
                *self.submit_cancel.get() = Some(cancel.clone());

                match self.signal_agent_op(op.clone()).await {
                    Ok(result) => {
                        *self.submit_cancel.get() = None;
                        return Ok(result);
                    }
                    Err(first_err) => {
                        tracing::warn!(error = %first_err, "signal_agent_op failed for UserTurn, retrying in background");
                        // Push connecting status.
                        {
                            let mut buf = self.event_buffer.get();
                            buf.push(Event {
                                id: String::new(),
                                msg: EventMsg::BackgroundEvent(BackgroundEventEvent {
                                    message: "Connecting to backend...".to_string(),
                                }),
                            });
                        }
                        self.event_notify.notify_one();

                        // Spawn retry loop.
                        let client = self.client.clone();
                        let agent_id = self.active_agent_id();
                        let op_clone = op.clone();
                        tokio::spawn(async move {
                            let mut delay = std::time::Duration::from_millis(500);
                            let max_delay = std::time::Duration::from_secs(5);
                            loop {
                                tokio::select! {
                                    _ = cancel.cancelled() => {
                                        tracing::debug!("signal retry loop cancelled by interrupt");
                                        return;
                                    }
                                    _ = tokio::time::sleep(delay) => {}
                                }
                                let handle = client.get_workflow_handle::<AgentWorkflowRun>(&agent_id);
                                match handle
                                    .signal(
                                        AgentWorkflow::receive_op,
                                        op_clone.clone(),
                                        WorkflowSignalOptions::default(),
                                    )
                                    .await
                                {
                                    Ok(_) => {
                                        tracing::debug!("signal_agent_op succeeded after retry");
                                        return;
                                    }
                                    Err(e) => {
                                        tracing::warn!(error = %e, "signal retry failed");
                                        delay = (delay * 2).min(max_delay);
                                    }
                                }
                            }
                        });

                        return Ok("ok".to_string());
                    }
                }
            }
        }

        // Track shutdown locally so next_event() can detect it.
        if matches!(op, Op::Shutdown) {
            // Cancel any in-flight submit retry loop.
            if let Some(token) = self.submit_cancel.get().take() {
                token.cancel();
            }
            *self.shutdown.get() = true;
            // Push ShutdownComplete immediately so the TUI can exit without
            // waiting for a gRPC round-trip that may hang if the backend is
            // unreachable (the "connecting to backend..." scenario).
            {
                let mut buf = self.event_buffer.get();
                buf.push(Event {
                    id: String::new(),
                    msg: EventMsg::ShutdownComplete,
                });
            }
            self.event_notify.notify_one();
            // Fire-and-forget signal to both workflows (best effort).
            let op_clone = op.clone();
            let client = self.client.clone();
            let agent_id = self.active_agent_id();
            let session_id = self.session_id();
            tokio::spawn(async move {
                let agent_handle =
                    client.get_workflow_handle::<AgentWorkflowRun>(&agent_id);
                let _ = agent_handle
                    .signal(
                        AgentWorkflow::receive_op,
                        op_clone,
                        WorkflowSignalOptions::default(),
                    )
                    .await;
                let session_handle =
                    client.get_workflow_handle::<SessionWorkflowRun>(&session_id);
                let _ = session_handle
                    .signal(
                        SessionWorkflow::shutdown,
                        (),
                        WorkflowSignalOptions::default(),
                    )
                    .await;
            });
            return Ok("ok".to_string());
        }

        // Apply a timeout to the signal call so a stuck gRPC connection
        // doesn't permanently block the op-forwarding task (and prevent
        // subsequent ops like Shutdown from being processed).
        match tokio::time::timeout(
            std::time::Duration::from_secs(10),
            self.signal_agent_op(op),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => {
                tracing::warn!("signal_agent_op timed out after 10s");
                Err(CodexErr::Fatal("signal_agent_op timed out".to_string()))
            }
        }
    }

    async fn next_event(&self) -> CodexResult<Event> {
        loop {
            // Check the local buffer first.
            {
                let mut buf = self.event_buffer.get();
                if !buf.is_empty() {
                    return Ok(buf.remove(0));
                }
            }

            // If not started, wait a bit (watcher not yet running).
            let started = *self.started.get();
            if !started {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }

            // Wait for the watcher or submit logic to push events.
            // Use a 1-second timeout so that callers whose own timeout
            // deadline is checked outside of this function (e.g. the e2e
            // test loops that call `next_event` in a loop with a manual
            // `Instant::now() > deadline` guard) are not blocked forever
            // when the watcher is in a persistent error-stall state.
            tokio::select! {
                _ = self.event_notify.notified() => {}
                _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
            }
        }
    }
}

#[async_trait::async_trait]
impl codex_tui::ExternalAgentBrowser for TemporalAgentSession {
    async fn list_sessions(&self) -> Vec<codex_tui::ExternalSessionEntry> {
        let current_id = self.session_id();
        match self.query_sessions().await {
            Ok(entries) => entries
                .into_iter()
                .map(|e| {
                    let is_current = e.session_id == current_id;
                    let is_closed = e.status != crate::types::SessionStatus::Running;
                    let description = {
                        let status_str = if is_closed { " [closed]" } else { "" };
                        Some(format!("{}{}", e.model, status_str))
                    };
                    codex_tui::ExternalSessionEntry {
                        id: e.session_id,
                        name: e.name.unwrap_or_else(|| "(unnamed)".to_string()),
                        description,
                        is_current,
                        is_closed,
                    }
                })
                .collect(),
            Err(err) => {
                tracing::error!(%err, "failed to query sessions from harness");
                Vec::new()
            }
        }
    }

    async fn switch_to(
        &self,
        session_id: &str,
    ) -> color_eyre::eyre::Result<codex_tui::ExternalSwitchResult> {
        use codex_protocol::ThreadId;

        self.switch_session(session_id.to_string());

        // Fetch existing events from the new session to seed initial_messages.
        let events = self
            .fetch_initial_events()
            .await
            .map_err(|e| color_eyre::eyre::eyre!("failed to fetch initial events: {e}"))?;
        let filtered = filter_initial_events(events);

        Ok(codex_tui::ExternalSwitchResult {
            session_configured: SessionConfiguredEvent {
                session_id: ThreadId::new(),
                forked_from_id: None,
                thread_name: None,
                initial_messages: Some(filtered),
                model: self.base_input.model.clone(),
                model_provider_id: "openai".into(),
                approvals_reviewer: Default::default(),
                approval_policy: self.base_input.approval_policy,
                sandbox_policy: SandboxPolicy::DangerFullAccess,
                cwd: std::env::current_dir().unwrap_or_default(),
                reasoning_effort: self.base_input.reasoning_effort,
                history_log_id: 0,
                history_entry_count: 0,
                network_proxy: None,
                rollout_path: None,
                service_tier: None,
            },
        })
    }

    fn current_session_id(&self) -> Option<String> {
        Some(self.session_id())
    }
}
