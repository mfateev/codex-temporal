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
use codex_temporal::harness::CodexHarness;
use codex_temporal::session_workflow::SessionWorkflow;
use codex_temporal::workflow::AgentWorkflow;

const TASK_QUEUE: &str = "codex-temporal";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The apply_patch tool handler spawns a subprocess using the current
    // binary with `--codex-run-as-apply-patch <patch>` as arguments.
    // Detect that and delegate immediately, matching the pattern from
    // codex-rs/arg0: consume the flag, take the next arg as the patch
    // text, and call `apply_patch()` directly.
    {
        let mut args = std::env::args_os();
        let _argv0 = args.next();
        if let Some(arg1) = args.next() {
            if arg1.to_str() == Some(codex_apply_patch::CODEX_CORE_APPLY_PATCH_ARG1) {
                let exit_code = match args.next().and_then(|s| s.into_string().ok()) {
                    Some(patch_arg) => {
                        let mut stdout = std::io::stdout();
                        let mut stderr = std::io::stderr();
                        match codex_apply_patch::apply_patch(&patch_arg, &mut stdout, &mut stderr)
                        {
                            Ok(()) => 0,
                            Err(_) => 1,
                        }
                    }
                    None => {
                        eprintln!(
                            "Error: {} requires a UTF-8 PATCH argument.",
                            codex_apply_patch::CODEX_CORE_APPLY_PATCH_ARG1
                        );
                        1
                    }
                };
                std::process::exit(exit_code);
            }
        }
    }

    if std::env::var("OPENAI_API_KEY").map_or(true, |v| v.is_empty()) {
        eprintln!(
            "Error: OPENAI_API_KEY environment variable is not set.\n\n\
             The worker needs an OpenAI API key to make model calls.\n\
             Get one at https://platform.openai.com/api-keys\n\n\
             Usage: OPENAI_API_KEY=\"sk-...\" cargo run --bin codex-temporal-worker"
        );
        std::process::exit(1);
    }

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
        .register_workflow::<SessionWorkflow>()
        .register_workflow::<AgentWorkflow>()
        .register_workflow::<CodexHarness>()
        .register_activities(CodexActivities::new())
        .build();

    let mut worker = Worker::new(&runtime, client, worker_options)?;

    tracing::info!("worker ready, polling for tasks…");
    worker.run().await?;

    Ok(())
}
