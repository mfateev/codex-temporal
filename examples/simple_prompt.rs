//! Example: Run a simple prompt through the Codex workflow.
//!
//! This example demonstrates:
//! 1. Setting up a Temporal worker
//! 2. Registering the Codex workflow and activities
//! 3. Starting a workflow execution
//!
//! Prerequisites:
//! - Temporal server running locally (temporal server start-dev)
//!
//! Usage:
//! ```bash
//! cargo run --example simple_prompt
//! ```

use std::sync::Arc;
use temporalio_sdk::{Worker, sdk_client_options};
use temporalio_sdk_core::{init_worker, CoreRuntime, RuntimeOptions, Url};
use temporalio_common::{
    telemetry::TelemetryOptions,
    worker::{WorkerConfig, WorkerTaskTypes, WorkerVersioningStrategy},
};

use codex_temporal::{
    activities::model_stream_activity,
    workflow::codex_workflow,
};

const TASK_QUEUE: &str = "codex-queue";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing
    tracing_subscriber::fmt::init();

    println!("Starting Codex-Temporal worker example...");

    // Connect to Temporal server
    let server_options = sdk_client_options(Url::parse("http://localhost:7233")?).build();

    let telemetry_options = TelemetryOptions::builder().build();
    let runtime_options = RuntimeOptions::builder()
        .telemetry_options(telemetry_options)
        .build()
        .unwrap();
    let runtime = CoreRuntime::new_assume_tokio(runtime_options)?;

    let client = server_options.connect("default", None).await?;

    // Configure worker
    let worker_config = WorkerConfig::builder()
        .namespace("default")
        .task_queue(TASK_QUEUE)
        .task_types(WorkerTaskTypes::all())
        .versioning_strategy(WorkerVersioningStrategy::None {
            build_id: "codex-temporal-v0.1".to_owned(),
        })
        .build()
        .unwrap();

    let core_worker = init_worker(&runtime, worker_config, client)?;

    let mut worker = Worker::new_from_core(Arc::new(core_worker), TASK_QUEUE);

    // Register workflow
    worker.register_wf("codex_workflow", codex_workflow);

    // Register activities
    worker.register_activity("model_stream", model_stream_activity);

    println!("Worker registered. Polling on task queue: {}", TASK_QUEUE);
    println!("To start a workflow, use the Temporal CLI:");
    println!("  temporal workflow start \\");
    println!("    --task-queue {} \\", TASK_QUEUE);
    println!("    --type codex_workflow \\");
    println!("    --input '{{\"prompt\": \"Hello world\", \"model\": \"gpt-4\"}}'");
    println!();
    println!("Press Ctrl+C to stop the worker.");

    // Run worker
    worker.run().await?;

    Ok(())
}
