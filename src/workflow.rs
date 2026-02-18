//! The Temporal workflow that drives the codex agentic loop.
//!
//! This workflow calls [`try_run_sampling_request`] in a loop, dispatching
//! model calls and tool executions as Temporal activities.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use codex_core::config::Config;
use codex_core::entropy::{EntropyProviders, ENTROPY};
use codex_core::models_manager::manager::ModelsManager;
use codex_core::{
    EventSink, Prompt, Session, StorageBackend, TurnContext, TurnDiffTracker,
    try_run_sampling_request,
};
use codex_protocol::models::{BaseInstructions, ContentItem, ResponseItem};
use codex_protocol::ThreadId;
use temporalio_macros::{workflow, workflow_methods};
use temporalio_sdk::{WorkflowContext, WorkflowContextView, WorkflowResult};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::entropy::{TemporalClock, TemporalRandomSource};
use crate::sink::BufferEventSink;
use crate::storage::InMemoryStorage;
use crate::streamer::TemporalModelStreamer;
use crate::tools::TemporalToolHandler;
use crate::types::{CodexWorkflowInput, CodexWorkflowOutput};

/// Maximum number of model→tool loop iterations.
const MAX_ITERATIONS: u32 = 50;

#[workflow]
pub struct CodexWorkflow {
    input: CodexWorkflowInput,
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
    pub async fn run(ctx: &mut WorkflowContext<Self>) -> WorkflowResult<CodexWorkflowOutput> {
        let input = ctx.state(|s| s.input.clone());
        let events = ctx.state(|s| s.events.clone());

        // --- deterministic entropy ---
        let seed = ctx.random_seed();
        let wf_time = ctx.workflow_time().unwrap_or(SystemTime::UNIX_EPOCH);
        let entropy = EntropyProviders {
            random: Arc::new(TemporalRandomSource::new(seed)),
            clock: Arc::new(TemporalClock::new(wf_time)),
        };

        // --- config ---
        // Use for_harness() — zero I/O, safe inside the deterministic workflow sandbox.
        let codex_home = PathBuf::from("/tmp/codex-temporal");
        let mut config = Config::for_harness(codex_home)
            .map_err(|e| anyhow::anyhow!("failed to build config: {e}"))?;
        config.model = Some(input.model.clone());
        let config = Arc::new(config);

        // --- model info ---
        let model_slug =
            ModelsManager::get_model_offline_for_tests(config.model.as_deref());
        let model_info =
            ModelsManager::construct_model_info_offline_for_tests(&model_slug, &config);

        // --- session + turn context ---
        let conversation_id = ThreadId::new();
        let event_sink: Arc<dyn EventSink> = events.clone();
        let storage: Arc<dyn StorageBackend> = Arc::new(InMemoryStorage::new());

        let sess = Session::new_minimal(
            conversation_id,
            Arc::clone(&config),
            event_sink,
            storage,
        )
        .await;

        let turn_context = Arc::new(TurnContext::new_minimal(
            "turn-0".to_string(),
            model_info,
            Arc::clone(&config),
        ));

        // --- trait impls ---
        let mut streamer = TemporalModelStreamer::new(ctx.clone());
        let handler = TemporalToolHandler::new(ctx.clone());

        // --- prompt ---
        let user_item = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: input.user_message.clone(),
            }],
            end_turn: None,
            phase: None,
        };
        let prompt = Prompt {
            input: vec![user_item],
            tools: vec![],
            parallel_tool_calls: false,
            base_instructions: BaseInstructions {
                text: input.instructions.clone(),
            },
            personality: None,
            output_schema: None,
        };

        // --- run the agentic loop ---
        let diff_tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));
        let cancellation_token = CancellationToken::new();
        let mut iterations = 0u32;
        let mut last_agent_message: Option<String> = None;

        ENTROPY
            .scope(entropy, async {
                loop {
                    if iterations >= MAX_ITERATIONS {
                        tracing::warn!("max iterations reached ({MAX_ITERATIONS}), stopping");
                        break;
                    }

                    let result = try_run_sampling_request(
                        Arc::clone(&sess),
                        Arc::clone(&turn_context),
                        &mut streamer,
                        &handler,
                        None,
                        Arc::clone(&diff_tracker),
                        &prompt,
                        cancellation_token.child_token(),
                    )
                    .await;

                    iterations += 1;

                    match result {
                        Ok(outcome) => {
                            if let Some(msg) = outcome.last_agent_message {
                                last_agent_message = Some(msg);
                            }
                            if !outcome.needs_follow_up {
                                break;
                            }
                            tracing::info!(iteration = iterations, "follow-up needed, continuing loop");
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "try_run_sampling_request failed");
                            break;
                        }
                    }
                }
            })
            .await;

        Ok(CodexWorkflowOutput {
            last_agent_message,
            iterations,
        })
    }

    #[query]
    pub fn get_events(&self, _ctx: &WorkflowContextView) -> Vec<String> {
        self.events.drain().iter().map(|e| format!("{e:?}")).collect()
    }
}
