//! Example: Run an agent worker that can execute the full agentic loop.
//!
//! This example demonstrates:
//! 1. Setting up a Temporal worker
//! 2. Registering the agent workflow and all activities
//! 3. Running the worker to process workflow tasks
//!
//! Prerequisites:
//! - Temporal server running locally (temporal server start-dev)
//! - OPENAI_API_KEY environment variable set
//!
//! Usage:
//! ```bash
//! OPENAI_API_KEY=sk-... cargo run --example agent_worker
//! ```
//!
//! Then start a workflow:
//! ```bash
//! temporal workflow start \
//!   --task-queue codex-agent-queue \
//!   --type agent \
//!   --input '{"prompt": "Fetch https://httpbin.org/json and summarize it", "model": "gpt-4o"}'
//! ```

use std::sync::Arc;

use temporalio_client::ClientOptions;
use temporalio_common::{
    telemetry::TelemetryOptions,
    worker::{WorkerConfig, WorkerTaskTypes, WorkerVersioningStrategy},
};
use temporalio_sdk::Worker;
use temporalio_sdk_core::{init_worker, CoreRuntime, RuntimeOptions, Url};

use codex_temporal::{
    agent_workflow, http_fetch_activity, invoke_model_activity, model_stream_activity,
    codex_workflow,
};

const TASK_QUEUE: &str = "codex-agent-queue";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing
    tracing_subscriber::fmt::init();

    println!("🚀 Starting Codex Agent Worker...");
    println!();

    // Check for API key
    if std::env::var("OPENAI_API_KEY").is_err() {
        println!("⚠️  Warning: OPENAI_API_KEY not set. Model calls will fail.");
        println!("   Set it with: export OPENAI_API_KEY=sk-...");
        println!();
    }

    // Setup runtime
    let telemetry_options = TelemetryOptions::builder().build();
    let runtime_options = RuntimeOptions::builder()
        .telemetry_options(telemetry_options)
        .build()
        .unwrap();
    let runtime = CoreRuntime::new_assume_tokio(runtime_options)?;

    // Connect to Temporal server
    let client_options = ClientOptions::builder()
        .target_url(Url::parse("http://localhost:7233")?)
        .client_name("codex-temporal")
        .client_version(env!("CARGO_PKG_VERSION"))
        .identity("codex-agent-worker")
        .build();

    let client = client_options.connect("default", None).await?;

    // Configure worker
    let worker_config = WorkerConfig::builder()
        .namespace("default")
        .task_queue(TASK_QUEUE)
        .task_types(WorkerTaskTypes::all())
        .versioning_strategy(WorkerVersioningStrategy::None {
            build_id: "codex-agent-v0.1".to_owned(),
        })
        .build()
        .unwrap();

    let core_worker = init_worker(&runtime, worker_config, client)?;
    let mut worker = Worker::new_from_core(Arc::new(core_worker), TASK_QUEUE);

    // Register workflows
    worker.register_wf("agent", agent_workflow);
    worker.register_wf("codex_workflow", codex_workflow); // Legacy

    // Register activities
    worker.register_activity("invoke_model", invoke_model_activity);
    worker.register_activity("http_fetch", http_fetch_activity);
    worker.register_activity("model_stream", model_stream_activity); // Legacy

    println!("✅ Worker registered on task queue: {TASK_QUEUE}");
    println!();
    println!("To start a workflow, use the Temporal CLI:");
    println!();
    println!("  temporal workflow start \\");
    println!("    --task-queue {TASK_QUEUE} \\");
    println!("    --type agent \\");
    println!("    --input '{{\"prompt\": \"Fetch https://httpbin.org/json and summarize it\", \"model\": \"gpt-4o\"}}'");
    println!();
    println!("Or for a simple prompt without tools:");
    println!();
    println!("  temporal workflow start \\");
    println!("    --task-queue {TASK_QUEUE} \\");
    println!("    --type agent \\");
    println!("    --input '{{\"prompt\": \"What is 2+2?\", \"model\": \"gpt-4o\"}}'");
    println!();
    println!("Press Ctrl+C to stop the worker.");
    println!();

    // Run worker (blocks until shutdown)
    worker.run().await?;

    Ok(())
}
