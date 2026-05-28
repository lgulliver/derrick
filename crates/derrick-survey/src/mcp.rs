//! MCP server exposing the survey index to coding agents (DESIGN.md §9.B.8).
//!
//! Four tools (`derrick_survey_search` / `_context` / `_impact` / `_status`)
//! over stdio. Freshness has three layers: (1) a debounced [`crate::watch`]
//! rebuild, (2) a per-response staleness banner emitted only while the index is
//! dirty, and (3) a connect-time incremental build before the first query.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ServerCapabilities, ServerInfo};
use rmcp::service::ServiceExt;
use rmcp::transport::stdio;
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};
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

#[tool_handler]
impl ServerHandler for SurveyServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Query derrick's native code-graph index instead of fanning out across \
                 grep/glob/read. Use derrick_survey_search to find symbols, \
                 derrick_survey_context for architecture questions, derrick_survey_impact \
                 before changing a symbol, and derrick_survey_status to check freshness."
                    .to_owned(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
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
    use rmcp::model::CallToolRequestParam;
    use rmcp::service::ServiceExt;

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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mcp_lists_and_calls_tools() -> Result<(), SurveyError> {
        let (_tmp, survey) = temp_survey().await;
        let (server_io, client_io) = tokio::io::duplex(8192);

        let server = SurveyServer::new(survey, Arc::new(AtomicBool::new(false)));
        let server_handle = tokio::spawn(async move {
            let running = server.serve(server_io).await.unwrap();
            let _ = running.waiting().await;
        });

        let client = rmcp::serve_client((), client_io).await.unwrap();

        let tools = client.list_all_tools().await.unwrap();
        let names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
        assert!(names.contains(&"derrick_survey_search".to_owned()));
        assert!(names.contains(&"derrick_survey_context".to_owned()));
        assert!(names.contains(&"derrick_survey_impact".to_owned()));
        assert!(names.contains(&"derrick_survey_status".to_owned()));

        let result = client
            .call_tool(CallToolRequestParam {
                name: "derrick_survey_search".into(),
                arguments: serde_json::json!({ "query": "helper", "limit": 5 })
                    .as_object()
                    .cloned(),
            })
            .await
            .unwrap();
        let body: String = result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect();
        assert!(
            body.contains("helper"),
            "search result should mention helper: {body}"
        );

        client.cancel().await.unwrap();
        server_handle.abort();
        Ok(())
    }
}
