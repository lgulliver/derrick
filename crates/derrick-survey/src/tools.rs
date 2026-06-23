//! Shared MCP tool answer layer (DESIGN.md §9.B.8, D80).
//!
//! These functions hold the query + response-shaping logic for the four survey
//! tools, decoupled from any particular MCP server. The single-instance stdio
//! server ([`crate::mcp`]) and the multi-repo HTTP hub (`derrick-survey-hub`)
//! both delegate to these so the query and staleness-banner behaviour stays
//! identical across transports.
//!
//! Each `answer_*` function takes a `&Survey`, the watcher's `dirty` flag, and
//! the already-parsed parameters, and returns a ready-to-send
//! [`CallToolResult`]. The [`respond`] helper prefixes a staleness banner only
//! while the index is flagged dirty, keeping the common path cheap.

use std::sync::atomic::{AtomicBool, Ordering};

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};

use crate::{Survey, SurveyError};

/// Default number of entry-point hits when a tool call omits `limit`.
pub const DEFAULT_LIMIT: usize = 20;

/// Map a [`SurveyError`] to an MCP internal error.
pub fn internal(error: SurveyError) -> McpError {
    McpError::internal_error(error.to_string(), None)
}

/// Full-text search over symbol names and signatures.
pub async fn answer_search(
    survey: &Survey,
    dirty: &AtomicBool,
    query: &str,
    limit: Option<usize>,
) -> Result<CallToolResult, McpError> {
    let limit = limit.unwrap_or(DEFAULT_LIMIT);
    let hits = survey.search(query, limit).await.map_err(internal)?;
    respond(survey, dirty, &hits).await
}

/// Resolve a query to entry-point symbols plus the symbols they reference.
pub async fn answer_context(
    survey: &Survey,
    dirty: &AtomicBool,
    query: &str,
    limit: Option<usize>,
) -> Result<CallToolResult, McpError> {
    let limit = limit.unwrap_or(DEFAULT_LIMIT);
    let context = survey.context(query, limit).await.map_err(internal)?;
    respond(survey, dirty, &context).await
}

/// Show the direct callers and callees of a symbol.
pub async fn answer_impact(
    survey: &Survey,
    dirty: &AtomicBool,
    symbol: &str,
) -> Result<CallToolResult, McpError> {
    let impact = survey.impact(symbol).await.map_err(internal)?;
    respond(survey, dirty, &impact).await
}

/// Index freshness and size summary.
pub async fn answer_status(
    survey: &Survey,
    dirty: &AtomicBool,
) -> Result<CallToolResult, McpError> {
    let rebuilding = dirty.load(Ordering::Relaxed);
    let status = survey
        .status_with_flag(rebuilding)
        .await
        .map_err(internal)?;
    respond(survey, dirty, &status).await
}

/// Serialize a result to JSON, prefixing a staleness banner only when the
/// watcher has flagged the index dirty (so the common path stays cheap).
pub async fn respond<T: serde::Serialize>(
    survey: &Survey,
    dirty: &AtomicBool,
    value: &T,
) -> Result<CallToolResult, McpError> {
    let mut contents = Vec::new();
    let rebuilding = dirty.load(Ordering::Relaxed);
    if rebuilding {
        if let Ok(status) = survey.status_with_flag(true).await {
            if !status.pending.is_empty() {
                let sample: Vec<&str> = status
                    .pending
                    .iter()
                    .take(10)
                    .map(|p| p.path.as_str())
                    .collect();
                contents.push(Content::text(format!(
                    "STALE: {} file(s) differ from the index (e.g. {}). \
                     A rebuild is in progress; Read these files directly if you need current contents.",
                    status.pending.len(),
                    sample.join(", ")
                )));
            }
        }
    }
    let json = serde_json::to_string_pretty(value)
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
    contents.push(Content::text(json));
    Ok(CallToolResult::success(contents))
}
