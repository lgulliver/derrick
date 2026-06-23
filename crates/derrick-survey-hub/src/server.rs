//! The [`HubServer`]: an rmcp [`ServerHandler`] exposing the four survey tools
//! over streamable HTTP, each routed to a workspace by a required `workspace`
//! argument. Query and banner logic is shared with the stdio server via
//! [`derrick_survey::tools`].

use std::sync::Arc;

use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ServerCapabilities, ServerInfo};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

use derrick_survey::tools;

use crate::config::{HubConfig, WorkspaceId};
use crate::hub::{Hub, HubError, WorkspaceEntry};

#[derive(Debug, Deserialize, JsonSchema)]
struct WorkspaceQueryParams {
    /// Workspace id to route this call to (see the server instructions).
    workspace: String,
    /// Search terms (matched against symbol names and signatures).
    query: String,
    /// Maximum number of entry-point hits (default 20).
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WorkspaceImpactParams {
    /// Workspace id to route this call to (see the server instructions).
    workspace: String,
    /// Symbol name to resolve.
    symbol: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WorkspaceStatusParams {
    /// Workspace id to route this call to (see the server instructions).
    workspace: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WorkspaceRefreshParams {
    /// Workspace id to route this call to (see the server instructions).
    workspace: String,
}

/// MCP server fronting a [`Hub`] of survey indexes.
#[derive(Clone)]
pub struct HubServer {
    hub: Hub,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl HubServer {
    /// Wrap a built [`Hub`] in an MCP server.
    pub fn new(hub: Hub) -> Self {
        Self {
            hub,
            tool_router: Self::tool_router(),
        }
    }

    /// Resolve a `workspace` argument to its entry, or a clear MCP error naming
    /// the hosted workspaces.
    async fn resolve(&self, workspace: &str) -> Result<WorkspaceEntry, McpError> {
        let id = WorkspaceId::new(workspace.to_owned())
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        match self.hub.entry(&id).await {
            Some(entry) => Ok(entry),
            None => {
                let known: Vec<String> = self
                    .hub
                    .workspace_ids()
                    .await
                    .iter()
                    .map(ToString::to_string)
                    .collect();
                Err(McpError::invalid_params(
                    format!(
                        "unknown workspace {:?}; hosted workspaces: [{}]",
                        workspace,
                        known.join(", ")
                    ),
                    None,
                ))
            }
        }
    }

    /// Poll-on-query freshness: bring `entry` up to date if its TTL has lapsed,
    /// mapping any rebuild failure to an MCP internal error. A no-op within the
    /// TTL window, and self-flighting under concurrency (see
    /// [`WorkspaceEntry::ensure_fresh`]).
    async fn ensure_fresh(&self, entry: &WorkspaceEntry) -> Result<(), McpError> {
        entry
            .ensure_fresh(self.hub.freshness_ttl())
            .await
            .map_err(tools::internal)
    }

    #[tool(
        description = "Full-text search over indexed symbol names and signatures \
        in the given workspace. Requires a `workspace` argument. Returns matching \
        symbols with file:line locations."
    )]
    async fn derrick_survey_search(
        &self,
        params: Parameters<WorkspaceQueryParams>,
    ) -> Result<CallToolResult, McpError> {
        let entry = self.resolve(&params.0.workspace).await?;
        self.ensure_fresh(&entry).await?;
        tools::answer_search(&entry.survey, &entry.dirty, &params.0.query, params.0.limit).await
    }

    #[tool(
        description = "Resolve a query to entry-point symbols plus the symbols \
        they reference in the given workspace — the one-call answer to an \
        architecture question. Requires a `workspace` argument."
    )]
    async fn derrick_survey_context(
        &self,
        params: Parameters<WorkspaceQueryParams>,
    ) -> Result<CallToolResult, McpError> {
        let entry = self.resolve(&params.0.workspace).await?;
        self.ensure_fresh(&entry).await?;
        tools::answer_context(&entry.survey, &entry.dirty, &params.0.query, params.0.limit).await
    }

    #[tool(
        description = "Show the direct callers and callees of a symbol in the \
        given workspace — its impact radius before you change it. Requires a \
        `workspace` argument. Matching is by name, so results may include \
        unrelated symbols that share the name."
    )]
    async fn derrick_survey_impact(
        &self,
        params: Parameters<WorkspaceImpactParams>,
    ) -> Result<CallToolResult, McpError> {
        let entry = self.resolve(&params.0.workspace).await?;
        self.ensure_fresh(&entry).await?;
        tools::answer_impact(&entry.survey, &entry.dirty, &params.0.symbol).await
    }

    #[tool(
        description = "Index freshness and size summary for the given workspace, \
        including files that differ from the working tree. Requires a \
        `workspace` argument. The response includes a `freshness` field \
        (\"fresh\" | \"rebuilding\" | \"stale since <ts>\") and an optional \
        `last_build_ts` (Unix seconds)."
    )]
    async fn derrick_survey_status(
        &self,
        params: Parameters<WorkspaceStatusParams>,
    ) -> Result<CallToolResult, McpError> {
        let entry = self.resolve(&params.0.workspace).await?;
        self.ensure_fresh(&entry).await?;
        tools::answer_status(&entry.survey, &entry.dirty).await
    }

    #[tool(
        description = "Force an incremental rebuild of the given workspace's index \
        now, then return its post-build status. Requires a `workspace` argument. \
        Use this after a known change (e.g. from CI) to reconcile the index \
        immediately instead of waiting for the poll-on-query freshness window."
    )]
    async fn derrick_survey_refresh(
        &self,
        params: Parameters<WorkspaceRefreshParams>,
    ) -> Result<CallToolResult, McpError> {
        let entry = self.resolve(&params.0.workspace).await?;
        let status = entry.force_refresh().await.map_err(tools::internal)?;
        let json = serde_json::to_string_pretty(&status)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for HubServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.instructions = Some(
            "Query derrick's native code-graph indexes across several repositories. \
             Every tool takes a required `workspace` argument selecting which repo's \
             index to query. Use derrick_survey_search to find symbols, \
             derrick_survey_context for architecture questions, derrick_survey_impact \
             before changing a symbol, and derrick_survey_status to check freshness. \
             Indexes self-heal on a freshness TTL, so reads stay current without \
             intervention; call derrick_survey_refresh to proactively rebuild a \
             workspace right after a known change (e.g. from CI) instead of waiting \
             for the next poll."
                .to_owned(),
        );
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info
    }
}

/// Build the hub from `config`, then serve the survey tools over rmcp's
/// streamable-HTTP transport bound to `config.bind` (a loopback address) until
/// the process is shut down.
///
/// No auth, no per-repo watcher — freshness is connect-time build plus
/// poll-on-query against `config.freshness_ttl_secs`, with an explicit
/// `derrick_survey_refresh` tool for proactive rebuilds. The connect-time build
/// happens inside [`Hub::build`] before the listener is opened.
pub async fn serve(config: &HubConfig) -> Result<(), HubError> {
    let hub = Hub::build(config).await?;
    let service = StreamableHttpService::new(
        move || Ok(HubServer::new(hub.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );

    let app = axum::Router::new().fallback_service(service);
    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .map_err(|source| HubError::Bind {
            addr: config.bind.to_string(),
            source,
        })?;
    tracing::info!(addr = %config.bind, "survey hub listening");
    axum::serve(listener, app).await.map_err(HubError::Serve)?;
    Ok(())
}
