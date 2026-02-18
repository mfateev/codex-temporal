//! Temporal worker binary for the codex harness.
//!
//! Runs both workflow and activity workers on the same task queue.
//! The workflow worker executes the codex agentic loop deterministically,
//! while the activity worker performs real I/O (model calls, tool exec).

use std::str::FromStr;

use temporalio_client::{Client, ClientOptions, Connection, ConnectionOptions};
use temporalio_common::telemetry::TelemetryOptions;
use temporalio_common::worker::WorkerTaskTypes;
use temporalio_sdk::{Worker, WorkerOptions};
use temporalio_sdk_core::{CoreRuntime, RuntimeOptions, Url};

use codex_temporal::activities::CodexActivities;
use codex_temporal::workflow::CodexWorkflow;

const TASK_QUEUE: &str = "codex-temporal";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".parse().unwrap()),
        )
        .init();

    let server_url = std::env::var("TEMPORAL_ADDRESS")
        .unwrap_or_else(|_| "http://localhost:7233".to_string());

    tracing::info!(%server_url, task_queue = TASK_QUEUE, "starting codex-temporal worker");

    // Connect to the Temporal server.
    let connection_options = ConnectionOptions::new(
        Url::from_str(&server_url)?,
    )
    .identity("codex-temporal-worker")
    .build();
    let telemetry_options = TelemetryOptions::builder().build();
    let runtime_options = RuntimeOptions::builder()
        .telemetry_options(telemetry_options)
        .build()?;
    let runtime = CoreRuntime::new_assume_tokio(runtime_options)?;

    let connection = Connection::connect(connection_options).await?;
    let client = Client::new(
        connection,
        ClientOptions::new("default").build(),
    )?;

    // Build the worker with both workflow and activity registrations.
    let worker_options = WorkerOptions::new(TASK_QUEUE)
        .task_types(WorkerTaskTypes::all())
        .register_workflow::<CodexWorkflow>()
        .register_activities(CodexActivities)
        .build();

    let mut worker = Worker::new(&runtime, client, worker_options)?;

    tracing::info!("worker ready, polling for tasks…");
    worker.run().await?;

    Ok(())
}
