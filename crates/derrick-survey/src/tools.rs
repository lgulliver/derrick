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

/// Whether the served index has a working tree behind it, which decides
/// whether the dirty-state staleness banner (a tree-vs-index diff) is
/// meaningful. A Pushed hub index has no tree, so the banner is suppressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BannerMode {
    /// The index is backed by a working tree; emit the tree-vs-index staleness
    /// banner while the index is flagged dirty (stdio server, Local hub).
    TreeBacked,
    /// The index has no working tree (Pushed hub); a tree diff is meaningless,
    /// so never emit the staleness banner.
    None,
}

/// Map a [`SurveyError`] to an MCP internal error.
pub fn internal(error: SurveyError) -> McpError {
    McpError::internal_error(error.to_string(), None)
}

/// Full-text search over symbol names and signatures.
pub async fn answer_search(
    survey: &Survey,
    dirty: &AtomicBool,
    banner: BannerMode,
    query: &str,
    limit: Option<usize>,
) -> Result<CallToolResult, McpError> {
    let limit = limit.unwrap_or(DEFAULT_LIMIT);
    let hits = survey.search(query, limit).await.map_err(internal)?;
    respond(survey, dirty, banner, &hits).await
}

/// Resolve a query to entry-point symbols plus the symbols they reference.
pub async fn answer_context(
    survey: &Survey,
    dirty: &AtomicBool,
    banner: BannerMode,
    query: &str,
    limit: Option<usize>,
) -> Result<CallToolResult, McpError> {
    let limit = limit.unwrap_or(DEFAULT_LIMIT);
    let context = survey.context(query, limit).await.map_err(internal)?;
    respond(survey, dirty, banner, &context).await
}

/// Show the direct callers and callees of a symbol.
pub async fn answer_impact(
    survey: &Survey,
    dirty: &AtomicBool,
    banner: BannerMode,
    symbol: &str,
) -> Result<CallToolResult, McpError> {
    let impact = survey.impact(symbol).await.map_err(internal)?;
    respond(survey, dirty, banner, &impact).await
}

/// Index freshness and size summary.
pub async fn answer_status(
    survey: &Survey,
    dirty: &AtomicBool,
    banner: BannerMode,
) -> Result<CallToolResult, McpError> {
    let rebuilding = dirty.load(Ordering::Relaxed);
    let status = survey
        .status_with_flag(rebuilding)
        .await
        .map_err(internal)?;
    respond(survey, dirty, banner, &status).await
}

/// Serialize a result to JSON, prefixing a staleness banner only when the
/// watcher has flagged the index dirty (so the common path stays cheap) and the
/// served index is backed by a working tree.
///
/// The banner is a tree-vs-index diff, so it is only meaningful for a
/// tree-backed index ([`BannerMode::TreeBacked`]): the stdio server and a Local
/// hub workspace. A Pushed hub index has no working tree (its `repo_root` is
/// just the DB's parent dir), so the diff would report every indexed file as
/// differing; [`BannerMode::None`] suppresses the banner entirely and the value
/// is serialized and returned exactly as for the tree-backed case.
pub async fn respond<T: serde::Serialize>(
    survey: &Survey,
    dirty: &AtomicBool,
    banner: BannerMode,
    value: &T,
) -> Result<CallToolResult, McpError> {
    let mut contents = Vec::new();
    let rebuilding = banner == BannerMode::TreeBacked && dirty.load(Ordering::Relaxed);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::BuildOptions;
    use crate::{Survey, SurveyConfig};
    use std::sync::atomic::AtomicBool;

    /// Concatenate the text content of a [`CallToolResult`].
    fn text(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect()
    }

    /// Open and build a survey over a temp repo, then add an unindexed file so a
    /// working-tree diff reports a non-empty `pending` set (a divergent tree).
    async fn divergent_survey() -> (tempfile::TempDir, Survey) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        std::fs::write(repo.join("lib.rs"), "pub fn helper() {}\n").unwrap();
        std::fs::create_dir_all(repo.join(".derrick")).unwrap();
        let survey = Survey::open(SurveyConfig {
            db_path: repo.join(".derrick/index.db"),
            repo_root: repo.to_path_buf(),
            reader_pool: SurveyConfig::DEFAULT_READER_POOL,
        })
        .await
        .unwrap();
        survey.build(BuildOptions::default()).await.unwrap();
        // Diverge the tree from the index: an as-yet-unindexed file.
        std::fs::write(repo.join("extra.rs"), "pub fn added() {}\n").unwrap();
        (tmp, survey)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tree_backed_dirty_emits_stale_banner() {
        let (_tmp, survey) = divergent_survey().await;
        let dirty = AtomicBool::new(true);
        let result = respond(&survey, &dirty, BannerMode::TreeBacked, &"payload")
            .await
            .unwrap();
        let body = text(&result);
        assert!(
            body.contains("STALE:"),
            "tree-backed + dirty + divergent tree must emit the banner: {body}"
        );
        assert!(
            body.contains("extra.rs"),
            "banner should name the pending file: {body}"
        );
        // The serialized payload is still present after the banner.
        assert!(body.contains("payload"), "payload still present: {body}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn none_mode_dirty_suppresses_banner() {
        let (_tmp, survey) = divergent_survey().await;
        let dirty = AtomicBool::new(true);
        let result = respond(&survey, &dirty, BannerMode::None, &"payload")
            .await
            .unwrap();
        let body = text(&result);
        assert!(
            !body.contains("STALE:"),
            "BannerMode::None must suppress the banner even when dirty: {body}"
        );
        // Exactly the serialized payload, nothing prepended.
        assert!(body.contains("payload"), "payload still present: {body}");
        assert_eq!(
            result.content.len(),
            1,
            "None mode emits only the JSON payload content: {body}"
        );
    }
}
