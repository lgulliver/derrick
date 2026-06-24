//! The [`HubServer`]: an rmcp [`ServerHandler`] exposing the four survey tools
//! over streamable HTTP, each routed to a workspace by a required `workspace`
//! argument. Query and banner logic is shared with the stdio server via
//! [`derrick_survey::tools`].

use std::sync::Arc;

use axum::http::request::Parts;
use rmcp::handler::server::common::Extension;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ServerCapabilities, ServerInfo};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

use derrick_survey::tools;

use crate::auth::{AuthRegistry, AuthzError, Principal, require_bearer};
use crate::config::{Capability, HubConfig, WorkspaceId};
use crate::hub::{Hub, HubError, WorkspaceEntry};

#[derive(Debug, Deserialize, JsonSchema)]
struct WorkspaceQueryParams {
    /// Workspace id to route this call to. Optional when this endpoint is
    /// already pinned to a workspace (addressed via a `/w/<id>` path); required
    /// on the root endpoint (see the server instructions).
    #[serde(default)]
    workspace: Option<String>,
    /// Search terms (matched against symbol names and signatures).
    query: String,
    /// Maximum number of entry-point hits (default 20).
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WorkspaceImpactParams {
    /// Workspace id to route this call to. Optional on a `/w/<id>` endpoint,
    /// required on the root endpoint (see the server instructions).
    #[serde(default)]
    workspace: Option<String>,
    /// Symbol name to resolve.
    symbol: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WorkspaceStatusParams {
    /// Workspace id to route this call to. Optional on a `/w/<id>` endpoint,
    /// required on the root endpoint (see the server instructions).
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WorkspaceRefreshParams {
    /// Workspace id to route this call to. Optional on a `/w/<id>` endpoint,
    /// required on the root endpoint (see the server instructions).
    #[serde(default)]
    workspace: Option<String>,
}

/// Discovery tool params: no `workspace` argument — it lists the ids the caller
/// may reach.
#[derive(Debug, Deserialize, JsonSchema)]
struct ListWorkspacesParams {}

/// MCP server fronting a [`Hub`] of survey indexes.
///
/// A server is either **unpinned** (the root mount, where every tool requires a
/// `workspace` argument) or **pinned** to one [`WorkspaceId`] (a `/w/<id>`
/// mount, where the `workspace` argument is optional and, if present, must match
/// the pin). [`build_router`] mounts one unpinned root plus one pinned mount per
/// configured workspace.
#[derive(Clone)]
pub struct HubServer {
    hub: Hub,
    /// The workspace this server is pinned to, if any. `None` for the root
    /// mount (every tool then requires an explicit `workspace`); `Some(id)` for
    /// a `/w/<id>` mount (the `workspace` argument becomes optional and is
    /// defaulted to the pin).
    pinned: Option<WorkspaceId>,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl HubServer {
    /// Wrap a built [`Hub`] in an unpinned MCP server (the root mount). Every
    /// tool call must carry an explicit `workspace` argument.
    pub fn new(hub: Hub) -> Self {
        Self {
            hub,
            pinned: None,
            tool_router: Self::tool_router(),
        }
    }

    /// Wrap a built [`Hub`] in a server pinned to `id` (a `/w/<id>` mount). Tool
    /// calls may omit the `workspace` argument — it defaults to the pin — and a
    /// present argument must equal `id` or the call errors rather than silently
    /// crossing workspaces.
    pub fn pinned(hub: Hub, id: WorkspaceId) -> Self {
        Self {
            hub,
            pinned: Some(id),
            tool_router: Self::tool_router(),
        }
    }

    /// Resolve the effective workspace id for a call from the optional
    /// `workspace` argument and this server's pin.
    ///
    /// - explicit argument present → use it, but if the server is pinned and the
    ///   argument names a *different* workspace, reject the call rather than
    ///   silently crossing workspaces;
    /// - argument absent → use the pin if pinned, else a clear "workspace
    ///   required" error.
    ///
    /// This runs before [`Self::authorize`]/[`Self::resolve`], so authorization
    /// always applies to the *resolved* id (D83 unchanged).
    fn workspace_for(&self, argument: Option<&str>) -> Result<String, McpError> {
        match (argument, &self.pinned) {
            (Some(arg), Some(pin)) if arg != pin.as_str() => Err(McpError::invalid_params(
                format!(
                    "workspace argument {arg:?} does not match this endpoint's pinned \
                     workspace {:?}; omit the argument or address the root endpoint",
                    pin.as_str()
                ),
                None,
            )),
            (Some(arg), _) => Ok(arg.to_owned()),
            (None, Some(pin)) => Ok(pin.to_string()),
            (None, None) => Err(McpError::invalid_params(
                "workspace required: pass a `workspace` argument or address the hub \
                 via a /w/<id> path"
                    .to_owned(),
                None,
            )),
        }
    }

    /// Authorize a tool call against the authenticated principal, if any.
    ///
    /// When auth is enabled, the bearer middleware has already injected a
    /// [`Principal`] into the request `parts`; this checks it reaches
    /// `workspace` and holds `capability`, mapping a refusal to a clear MCP
    /// error (the authz equivalent of 403). When auth is disabled there is no
    /// principal and the call is allowed unconditionally — preserving the
    /// pre-auth, loopback-only behaviour. The workspace authz check runs
    /// *before* [`Self::resolve`], so an out-of-scope id is reported as
    /// forbidden rather than leaking whether it is hosted.
    fn authorize(parts: &Parts, workspace: &str, capability: Capability) -> Result<(), McpError> {
        let Some(principal) = Principal::from_parts(parts) else {
            // Auth disabled: no principal was injected, so nothing to enforce.
            return Ok(());
        };
        match principal.authorize(workspace, capability) {
            Ok(()) => Ok(()),
            Err(AuthzError::ForbiddenWorkspace) => {
                tracing::debug!(%workspace, "survey hub: workspace not in token scope");
                Err(McpError::invalid_request(
                    format!("forbidden: token is not authorized for workspace {workspace:?}"),
                    None,
                ))
            }
            Err(AuthzError::MissingCapability(cap)) => {
                tracing::debug!(%workspace, ?cap, "survey hub: token lacks capability");
                Err(McpError::invalid_request(
                    format!("forbidden: token lacks the {cap:?} capability for this tool"),
                    None,
                ))
            }
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

    /// The staleness-banner mode for `entry`, derived from its source. A Local
    /// workspace is tree-backed so the tree-vs-index banner is meaningful; a
    /// Pushed workspace has no working tree, so the banner is suppressed (it
    /// would otherwise fire bogusly during every reload window).
    fn banner_mode(entry: &WorkspaceEntry) -> tools::BannerMode {
        if entry.source().is_tree_backed() {
            tools::BannerMode::TreeBacked
        } else {
            tools::BannerMode::None
        }
    }

    #[tool(
        description = "Full-text search over indexed symbol names and signatures \
        in the given workspace. The `workspace` argument is required on the root \
        endpoint and optional on a `/w/<id>` endpoint (where any value passed \
        must match the pinned id). Returns matching \
        symbols with file:line locations."
    )]
    async fn derrick_survey_search(
        &self,
        params: Parameters<WorkspaceQueryParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let workspace = self.workspace_for(params.0.workspace.as_deref())?;
        Self::authorize(&parts, &workspace, Capability::Read)?;
        let entry = self.resolve(&workspace).await?;
        self.ensure_fresh(&entry).await?;
        let banner = Self::banner_mode(&entry);
        let survey = entry.survey().await;
        tools::answer_search(
            &survey,
            &entry.dirty,
            banner,
            &params.0.query,
            params.0.limit,
        )
        .await
    }

    #[tool(
        description = "Resolve a query to entry-point symbols plus the symbols \
        they reference in the given workspace — the one-call answer to an \
        architecture question. The `workspace` argument is required on the root \
        endpoint and optional on a `/w/<id>` endpoint (where any value passed \
        must match the pinned id)."
    )]
    async fn derrick_survey_context(
        &self,
        params: Parameters<WorkspaceQueryParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let workspace = self.workspace_for(params.0.workspace.as_deref())?;
        Self::authorize(&parts, &workspace, Capability::Read)?;
        let entry = self.resolve(&workspace).await?;
        self.ensure_fresh(&entry).await?;
        let banner = Self::banner_mode(&entry);
        let survey = entry.survey().await;
        tools::answer_context(
            &survey,
            &entry.dirty,
            banner,
            &params.0.query,
            params.0.limit,
        )
        .await
    }

    #[tool(
        description = "Show the direct callers and callees of a symbol in the \
        given workspace — its impact radius before you change it. The \
        `workspace` argument is required on the root endpoint and optional on a \
        `/w/<id>` endpoint (where any value passed must match the pinned id). \
        Matching is by name, so results may include \
        unrelated symbols that share the name."
    )]
    async fn derrick_survey_impact(
        &self,
        params: Parameters<WorkspaceImpactParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let workspace = self.workspace_for(params.0.workspace.as_deref())?;
        Self::authorize(&parts, &workspace, Capability::Read)?;
        let entry = self.resolve(&workspace).await?;
        self.ensure_fresh(&entry).await?;
        let banner = Self::banner_mode(&entry);
        let survey = entry.survey().await;
        tools::answer_impact(&survey, &entry.dirty, banner, &params.0.symbol).await
    }

    #[tool(
        description = "Index freshness and size summary for the given workspace, \
        including files that differ from the working tree. The `workspace` \
        argument is required on the root endpoint and optional on a `/w/<id>` \
        endpoint (where any value passed must match the pinned id). The response includes a `freshness` field \
        (\"fresh\" | \"rebuilding\" | \"stale since <ts>\") and an optional \
        `last_build_ts` (Unix seconds)."
    )]
    async fn derrick_survey_status(
        &self,
        params: Parameters<WorkspaceStatusParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let workspace = self.workspace_for(params.0.workspace.as_deref())?;
        Self::authorize(&parts, &workspace, Capability::Read)?;
        let entry = self.resolve(&workspace).await?;
        self.ensure_fresh(&entry).await?;
        // Source-aware status: Local diffs the working tree, Pushed reports the
        // prebuilt index's counts without a (nonexistent) tree diff. The shared
        // `respond` prefixes the staleness banner while a rebuild is in flight
        // only for a tree-backed (Local) source; a Pushed reload window passes
        // `BannerMode::None` so it never emits a bogus tree-vs-index banner.
        let banner = Self::banner_mode(&entry);
        let status = entry.status().await.map_err(tools::internal)?;
        let survey = entry.survey().await;
        tools::respond(&survey, &entry.dirty, banner, &status).await
    }

    #[tool(
        description = "Force an incremental rebuild of the given workspace's index \
        now, then return its post-build status. The `workspace` argument is \
        required on the root endpoint and optional on a `/w/<id>` endpoint \
        (where any value passed must match the pinned id). \
        Use this after a known change (e.g. from CI) to reconcile the index \
        immediately instead of waiting for the poll-on-query freshness window."
    )]
    async fn derrick_survey_refresh(
        &self,
        params: Parameters<WorkspaceRefreshParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let workspace = self.workspace_for(params.0.workspace.as_deref())?;
        Self::authorize(&parts, &workspace, Capability::Refresh)?;
        let entry = self.resolve(&workspace).await?;
        let status = entry.force_refresh().await.map_err(tools::internal)?;
        let json = serde_json::to_string_pretty(&status)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "List the workspace ids this endpoint can reach — call this \
        first to discover what to pass as the `workspace` argument. Takes no \
        arguments. The result is scoped to the caller's token: a wildcard token \
        (or no auth) sees every hosted workspace, a scoped token sees only its \
        own. A /w/<id> endpoint lists just its pinned workspace."
    )]
    async fn derrick_survey_list_workspaces(
        &self,
        _params: Parameters<ListWorkspacesParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        // On a pinned (/w/<id>) mount the listing is exactly that one id —
        // provided the principal can reach it; otherwise it is empty, matching
        // how the other tools refuse to confirm a workspace outside scope. On
        // the root mount the listing is every configured id, intersected with
        // the principal's scope.
        let candidates: Vec<String> = match &self.pinned {
            Some(pin) => vec![pin.to_string()],
            None => self
                .hub
                .workspace_ids()
                .await
                .iter()
                .map(ToString::to_string)
                .collect(),
        };
        let visible = match Principal::from_parts(&parts) {
            // Auth disabled: no principal, so every candidate is visible.
            None => candidates,
            // Auth enabled: keep only the ids the token's scope reaches.
            Some(principal) => principal.visible_ids(candidates.iter().map(String::as_str)),
        };
        let json = serde_json::to_string_pretty(&visible)
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
             Call derrick_survey_list_workspaces first to discover which workspace \
             ids you can reach. Each query tool then takes a `workspace` argument \
             selecting which repo's index to query: required on the root endpoint, \
             but optional when you address the hub via a /w/<id> path (which pins \
             the endpoint to that workspace; a `workspace` argument passed there \
             must match the pinned id). Use derrick_survey_search to find symbols, \
             derrick_survey_context for architecture questions, derrick_survey_impact \
             before changing a symbol, and derrick_survey_status to check freshness. \
             Workspaces are either Local (the hub holds the working tree and builds \
             the index itself) or Pushed (the hub serves a prebuilt index placed on \
             disk by CI). Indexes self-heal on a freshness TTL, so reads stay current \
             without intervention; call derrick_survey_refresh to reconcile a \
             workspace immediately after a known change — for Local workspaces it \
             rebuilds from the working tree, for Pushed workspaces it reloads the \
             prebuilt index from disk."
                .to_owned(),
        );
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info
    }
}

/// Assemble the axum [`Router`](axum::Router) that serves `hub`'s survey tools
/// over rmcp's streamable-HTTP transport, wiring the bearer-auth middleware when
/// `config.auth` holds at least one token.
///
/// This is the single source of truth for the serve wiring: [`serve`] binds it
/// to `config.bind`, and integration tests bind it to an ephemeral port so they
/// exercise the exact production middleware stack rather than re-deriving it.
///
/// Validates `config` first: [`AuthRegistry::build`] assumes the auth section
/// already passed [`HubConfig::validate`] (no empty/whitespace/duplicate tokens,
/// no wildcard mixed with explicit ids). Because this fn is re-exported, it
/// re-validates at the boundary so a caller that bypasses [`Hub::build`] cannot
/// smuggle in an unvalidated config — e.g. an ambiguous `["*", "repo-a"]` scope,
/// which validation rejects (and which `AuthRegistry::build` would in any case
/// treat as fail-closed deny-all, never `WorkspaceScope::All`).
///
/// Routing (D84): the root path serves an *unpinned* [`HubServer`] (every tool
/// needs an explicit `workspace` argument); additionally, each configured
/// workspace gets a *pinned* mount at the literal path `/w/<id>` backed by a
/// `HubServer::pinned`, where the `workspace` argument is optional. Literal
/// paths avoid path-parameter extraction. The bearer-auth layer is applied last,
/// so it wraps the root mount and every `/w/<id>` mount uniformly — authorization
/// still runs in-handler against the *resolved* workspace (D83 unchanged).
pub fn build_router(hub: Hub, config: &HubConfig) -> Result<axum::Router, HubError> {
    config.validate()?;
    let new_service = |server: HubServer| {
        StreamableHttpService::new(
            move || Ok(server.clone()),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default(),
        )
    };

    let mut app = axum::Router::new();
    // Pinned per-workspace mounts at literal `/w/<id>` paths. `config.validate()`
    // has already accepted every id, so `WorkspaceId::new` cannot fail here.
    for workspace in &config.workspaces {
        let id = WorkspaceId::new(workspace.id.clone())?;
        let path = format!("/w/{id}");
        let pinned = new_service(HubServer::pinned(hub.clone(), id));
        app = app.nest_service(&path, pinned);
    }
    // Root mount (unpinned): every tool requires an explicit `workspace`.
    app = app.fallback_service(new_service(HubServer::new(hub)));

    if let Some(auth) = &config.auth {
        if !auth.tokens.is_empty() {
            let registry = Arc::new(AuthRegistry::build(auth));
            app = app.layer(axum::middleware::from_fn(move |req, next| {
                let registry = registry.clone();
                require_bearer(registry, req, next)
            }));
            tracing::info!(
                tokens = auth.tokens.len(),
                "survey hub: bearer auth enabled"
            );
        }
    }
    Ok(app)
}

/// Build the hub from `config`, then serve the survey tools over rmcp's
/// streamable-HTTP transport bound to the validated `config.bind` address until
/// the process is shut down. The bind may be non-loopback only when auth is
/// configured (D83); otherwise validation keeps it loopback-only.
///
/// No per-repo watcher — freshness is connect-time build plus poll-on-query
/// against `config.freshness_ttl_secs`, with an explicit `derrick_survey_refresh`
/// tool for proactive rebuilds. The connect-time build happens inside
/// [`Hub::build`] before the listener is opened.
///
/// Auth (D83): when `config.auth` holds ≥1 token, the rmcp service is wrapped in
/// the [`require_bearer`] middleware so every request is authenticated before
/// dispatch and each tool call is authorized in-handler against the token's
/// scope; otherwise the service is served as-is (loopback-only, no token).
pub async fn serve(config: &HubConfig) -> Result<(), HubError> {
    let hub = Hub::build(config).await?;
    let app = build_router(hub, config)?;
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
