//! The Temporal workflow that drives the codex agentic loop.

use std::sync::Arc;
use std::time::SystemTime;

use futures::FutureExt;
use temporalio_macros::{workflow, workflow_methods};
use temporalio_sdk::{WorkflowContext, WorkflowContextView, WorkflowResult};

use crate::entropy::{TemporalClock, TemporalRandomSource};
use crate::sink::BufferEventSink;
use crate::types::{CodexWorkflowInput, CodexWorkflowOutput};

/// Maximum number of model→tool loop iterations.
const MAX_ITERATIONS: u32 = 50;

#[workflow]
pub struct CodexWorkflow {
    /// The input that was passed to the workflow.
    input: CodexWorkflowInput,
    /// Buffered events for query access.
    events: Arc<BufferEventSink>,
}

#[workflow_methods]
impl CodexWorkflow {
    #[init]
    pub fn new(_ctx: &WorkflowContextView, input: CodexWorkflowInput) -> Self {
        Self {
            input,
            events: Arc::new(BufferEventSink::new()),
        }
    }

    #[run]
    pub async fn run(
        ctx: &mut WorkflowContext<Self>,
    ) -> WorkflowResult<CodexWorkflowOutput> {
        let input = ctx.state(|s| s.input.clone());

        // Set up deterministic entropy from the workflow context.
        let seed = ctx.random_seed();
        let wf_time = ctx.workflow_time().unwrap_or(SystemTime::UNIX_EPOCH);
        let _random = Arc::new(TemporalRandomSource::new(seed));
        let _clock = Arc::new(TemporalClock::new(wf_time));

        tracing::info!(
            model = %input.model,
            message_len = input.user_message.len(),
            "codex workflow started"
        );

        // The full implementation will:
        // 1. Build a Prompt from the user message
        // 2. Create Session::new_minimal(...)
        // 3. Create TurnContext::new_minimal(...)
        // 4. Create TemporalModelStreamer + TemporalToolHandler
        // 5. Scope ENTROPY with the deterministic providers
        // 6. Call try_run_sampling_request in a loop
        //
        // For now, return a placeholder to validate the full compilation
        // and Temporal registration pipeline.

        let output = CodexWorkflowOutput {
            last_agent_message: None,
            iterations: 0,
        };

        Ok(output)
    }

    #[query]
    pub fn get_events(&self, _ctx: &WorkflowContextView) -> Vec<String> {
        self.events
            .drain()
            .iter()
            .map(|e| format!("{e:?}"))
            .collect()
    }
}
