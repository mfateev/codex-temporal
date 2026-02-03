//! HTTP fetch activity - simple HTTP GET tool for demonstrating tool calls.

use serde::{Deserialize, Serialize};
use temporalio_sdk::{ActContext, ActivityError};

use crate::types::ToolDef;

/// Input for the http_fetch activity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpFetchInput {
    /// The URL to fetch.
    pub url: String,
}

/// Output from the http_fetch activity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpFetchOutput {
    /// HTTP status code.
    pub status: u16,
    /// Response body (potentially truncated).
    pub body: String,
}

/// Activity that performs an HTTP GET request.
///
/// This is a simple tool to demonstrate the tool call flow.
/// It fetches a URL and returns the response body.
pub async fn http_fetch_activity(
    ctx: ActContext,
    input: HttpFetchInput,
) -> Result<HttpFetchOutput, ActivityError> {
    tracing::info!(url = %input.url, "Fetching URL");

    // Heartbeat before HTTP call
    ctx.record_heartbeat(vec![]);

    let client = reqwest::Client::new();
    let response = client
        .get(&input.url)
        .send()
        .await
        .map_err(|e| ActivityError::Retryable {
            source: anyhow::anyhow!("HTTP request failed: {e}"),
            explicit_delay: None,
        })?;

    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();

    // Truncate body if too large (keep first 10KB)
    let body = if body.len() > 10000 {
        format!("{}... [truncated, {} total bytes]", &body[..10000], body.len())
    } else {
        body
    };

    tracing::info!(status, body_len = body.len(), "URL fetched");

    Ok(HttpFetchOutput { status, body })
}

/// Returns the tool definition for http_fetch.
///
/// This is used to tell the model what tools are available.
pub fn http_fetch_tool_def() -> ToolDef {
    ToolDef {
        tool_type: "function".to_string(),
        name: "http_fetch".to_string(),
        description: "Fetch content from a URL via HTTP GET".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The URL to fetch"
                }
            },
            "required": ["url"]
        }),
    }
}
