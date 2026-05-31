//! MCP server exposing the survey index to coding agents (DESIGN.md §9.B.8).
//!
//! Four tools (`derrick_survey_search` / `_context` / `_impact` / `_status`)
//! over stdio. Freshness has three layers: (1) a debounced [`crate::watch`]
//! rebuild, (2) a per-response staleness banner emitted only while the index is
//! dirty, and (3) a connect-time incremental build before the first query.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ServerCapabilities, ServerInfo};
use rmcp::service::ServiceExt;
use rmcp::transport::stdio;
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::model::BuildOptions;
use crate::{Survey, SurveyError};

const DEFAULT_LIMIT: usize = 20;

#[derive(Debug, Deserialize, JsonSchema)]
struct QueryParams {
    /// Search terms (matched against symbol names and signatures).
    query: String,
    /// Maximum number of entry-point hits (default 20).
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ImpactParams {
    /// Symbol name to resolve.
    symbol: String,
}

/// The MCP server state: a shared [`Survey`] plus the watcher's dirty flag.
#[derive(Clone)]
pub(crate) struct SurveyServer {
    survey: Survey,
    dirty: Arc<AtomicBool>,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl SurveyServer {
    fn new(survey: Survey, dirty: Arc<AtomicBool>) -> Self {
        Self {
            survey,
            dirty,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Full-text search over indexed symbol names and signatures. \
        Returns matching symbols with file:line locations."
    )]
    async fn derrick_survey_search(
        &self,
        params: Parameters<QueryParams>,
    ) -> Result<CallToolResult, McpError> {
        let limit = params.0.limit.unwrap_or(DEFAULT_LIMIT);
        let hits = self
            .survey
            .search(&params.0.query, limit)
            .await
            .map_err(internal)?;
        self.respond(&hits).await
    }

    #[tool(
        description = "Resolve a query to entry-point symbols plus the symbols \
        they reference — the one-call answer to an architecture question."
    )]
    async fn derrick_survey_context(
        &self,
        params: Parameters<QueryParams>,
    ) -> Result<CallToolResult, McpError> {
        let limit = params.0.limit.unwrap_or(DEFAULT_LIMIT);
        let context = self
            .survey
            .context(&params.0.query, limit)
            .await
            .map_err(internal)?;
        self.respond(&context).await
    }

    #[tool(description = "Show the direct callers and callees of a symbol — its \
        impact radius before you change it. Matching is by name, so results may \
        include unrelated symbols that share the name.")]
    async fn derrick_survey_impact(
        &self,
        params: Parameters<ImpactParams>,
    ) -> Result<CallToolResult, McpError> {
        let impact = self
            .survey
            .impact(&params.0.symbol)
            .await
            .map_err(internal)?;
        self.respond(&impact).await
    }

    #[tool(
        description = "Index freshness and size summary, including files that \
        differ from the working tree."
    )]
    async fn derrick_survey_status(&self) -> Result<CallToolResult, McpError> {
        let status = self.survey.status().await.map_err(internal)?;
        self.respond(&status).await
    }

    /// Serialize a result to JSON, prefixing a staleness banner only when the
    /// watcher has flagged the index dirty (so the common path stays cheap).
    async fn respond<T: serde::Serialize>(&self, value: &T) -> Result<CallToolResult, McpError> {
        let mut contents = Vec::new();
        if self.dirty.load(Ordering::Relaxed) {
            if let Ok(status) = self.survey.status().await {
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
}

// Reuse the router built once in `new()` rather than the macro default of
// rebuilding it via `Self::tool_router()` on every call.
#[tool_handler(router = self.tool_router)]
impl ServerHandler for SurveyServer {
    fn get_info(&self) -> ServerInfo {
        // `ServerInfo` is `#[non_exhaustive]`, so build from the default and
        // assign the fields we care about rather than using a struct literal.
        let mut info = ServerInfo::default();
        info.instructions = Some(
            "Query derrick's native code-graph index instead of fanning out across \
             grep/glob/read. Use derrick_survey_search to find symbols, \
             derrick_survey_context for architecture questions, derrick_survey_impact \
             before changing a symbol, and derrick_survey_status to check freshness."
                .to_owned(),
        );
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info
    }
}

fn internal(error: SurveyError) -> McpError {
    McpError::internal_error(error.to_string(), None)
}

/// Run the MCP server over stdio until the client disconnects.
///
/// Performs a connect-time incremental build (freshness layer 3), spawns the
/// debounced watcher (layer 1), then serves the four tools.
pub async fn serve_stdio(survey: Survey) -> Result<(), SurveyError> {
    // Layer 3: reconcile the index with the tree before answering anything.
    survey.build(BuildOptions::default()).await?;

    // Layer 1: background watcher keeps the index fresh and flips the dirty flag.
    let dirty = Arc::new(AtomicBool::new(false));
    tokio::spawn(watch_task(survey.clone(), Arc::clone(&dirty)));

    let server = SurveyServer::new(survey, dirty);
    let running = server
        .serve(stdio())
        .await
        .map_err(|e| SurveyError::Internal(format!("mcp serve failed: {e}")))?;
    running
        .waiting()
        .await
        .map_err(SurveyError::Join)
        .map(|_| ())
}

async fn watch_task(survey: Survey, dirty: Arc<AtomicBool>) {
    if let Err(error) = crate::watch::watch_loop(survey, dirty).await {
        tracing::warn!(%error, "survey watcher stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SurveyConfig, SurveyError};
    use rmcp::model::CallToolRequestParams;
    use rmcp::service::ServiceExt;
    use serde_json::json;

    async fn temp_survey() -> (tempfile::TempDir, Survey) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        std::fs::write(
            repo.join("lib.rs"),
            "pub fn helper() {}\npub fn caller() {\n    helper();\n}\n",
        )
        .unwrap();
        std::fs::create_dir_all(repo.join(".derrick")).unwrap();
        let survey = Survey::open(SurveyConfig {
            db_path: repo.join(".derrick/index.db"),
            repo_root: repo.to_path_buf(),
            reader_pool: SurveyConfig::DEFAULT_READER_POOL,
        })
        .await
        .unwrap();
        survey.build(BuildOptions::default()).await.unwrap();
        (tmp, survey)
    }

    /// Spawn `server` over an in-memory duplex transport and return a connected
    /// client plus the server task handle.
    async fn connect(
        server: SurveyServer,
    ) -> (
        rmcp::service::RunningService<rmcp::RoleClient, ()>,
        tokio::task::JoinHandle<()>,
    ) {
        let (server_io, client_io) = tokio::io::duplex(8192);
        let handle = tokio::spawn(async move {
            let running = server.serve(server_io).await.unwrap();
            let _ = running.waiting().await;
        });
        let client = rmcp::serve_client((), client_io).await.unwrap();
        (client, handle)
    }

    /// Call `tool` with `args` and return the concatenated text content.
    async fn call_text(
        client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
        tool: &str,
        args: serde_json::Value,
    ) -> String {
        // `CallToolRequestParams` is `#[non_exhaustive]`; build from default.
        let mut req = CallToolRequestParams::default();
        req.name = tool.to_owned().into();
        req.arguments = args.as_object().cloned();
        let result = client.call_tool(req).await.unwrap();
        result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mcp_lists_tools_and_reports_server_info() -> Result<(), SurveyError> {
        let (_tmp, survey) = temp_survey().await;
        let server = SurveyServer::new(survey, Arc::new(AtomicBool::new(false)));
        let (client, handle) = connect(server).await;

        let tools = client.list_all_tools().await.unwrap();
        let names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
        assert!(names.contains(&"derrick_survey_search".to_owned()));
        assert!(names.contains(&"derrick_survey_context".to_owned()));
        assert!(names.contains(&"derrick_survey_impact".to_owned()));
        assert!(names.contains(&"derrick_survey_status".to_owned()));

        // `get_info` is delivered to the client during initialize.
        let info = client.peer_info();
        assert!(
            info.and_then(|i| i.instructions.as_deref())
                .is_some_and(|i| i.contains("derrick_survey_search"))
        );

        client.cancel().await.unwrap();
        handle.abort();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mcp_each_tool_returns_results() -> Result<(), SurveyError> {
        let (_tmp, survey) = temp_survey().await;
        let server = SurveyServer::new(survey, Arc::new(AtomicBool::new(false)));
        let (client, handle) = connect(server).await;

        let search = call_text(
            &client,
            "derrick_survey_search",
            json!({ "query": "helper" }),
        )
        .await;
        assert!(search.contains("helper"), "search: {search}");

        // `caller` references `helper`, so context should surface both.
        let context = call_text(
            &client,
            "derrick_survey_context",
            json!({ "query": "caller" }),
        )
        .await;
        assert!(context.contains("caller"), "context: {context}");
        assert!(context.contains("helper"), "context: {context}");

        let impact = call_text(
            &client,
            "derrick_survey_impact",
            json!({ "symbol": "helper" }),
        )
        .await;
        assert!(
            impact.contains("caller"),
            "impact should list the caller: {impact}"
        );

        let status = call_text(&client, "derrick_survey_status", json!({})).await;
        assert!(status.contains("\"files\""), "status: {status}");
        assert!(status.contains("\"symbols\""), "status: {status}");

        client.cancel().await.unwrap();
        handle.abort();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mcp_emits_staleness_banner_when_dirty_and_pending() -> Result<(), SurveyError> {
        let (tmp, survey) = temp_survey().await;
        // Create an as-yet-unindexed file so `status().pending` is non-empty,
        // then flag the index dirty to arm the banner path.
        std::fs::write(tmp.path().join("extra.rs"), "pub fn added() {}\n").unwrap();
        let dirty = Arc::new(AtomicBool::new(true));
        let server = SurveyServer::new(survey, dirty);
        let (client, handle) = connect(server).await;

        let body = call_text(
            &client,
            "derrick_survey_search",
            json!({ "query": "helper" }),
        )
        .await;
        assert!(body.contains("STALE:"), "expected staleness banner: {body}");
        assert!(
            body.contains("extra.rs"),
            "banner should name the pending file: {body}"
        );
        // The actual result is still appended after the banner.
        assert!(body.contains("helper"), "result still present: {body}");

        client.cancel().await.unwrap();
        handle.abort();
        Ok(())
    }
}
