//! `derrick-survey-hub` — centralised multi-repo survey hub (DESIGN.md §9.B.8a,
//! D80, phase 1).
//!
//! One process hosts several [`derrick_survey::Survey`] indexes, one per
//! workspace, behind a single rmcp streamable-HTTP MCP server. The four survey
//! tools each carry a required `workspace` argument that selects which repo's
//! index answers the call; the query and staleness-banner logic is shared with
//! the single-repo stdio server via [`derrick_survey::tools`].
//!
//! Freshness is hybrid: each workspace is built once at startup, then a
//! poll-on-query TTL (`freshness_ttl_secs`) re-probes and incrementally rebuilds
//! a workspace when its working tree has drifted, single-flighted so concurrent
//! queries trigger at most one rebuild. The `derrick_survey_refresh` tool forces
//! a rebuild immediately for callers that know a change just landed. There is no
//! per-repo filesystem watcher.
//!
//! Auth is optional (D83): with no `auth` section the server binds loopback-only
//! and requires no token (unchanged behaviour); with an `auth` section of ≥1
//! bearer token it may bind a non-loopback address and authenticates each
//! request, authorizing every tool call against the token's workspace scope and
//! capabilities. TLS is terminated by a reverse proxy; the hub speaks plain HTTP.

mod auth;
mod config;
mod hub;
mod server;

pub use config::{
    AuthConfig, Capability, ConfigError, HubConfig, TokenConfig, WorkspaceConfig, WorkspaceId,
    WorkspaceIdError, WorkspaceScope, WorkspaceSourceConfig,
};
pub use hub::{Hub, HubError, WorkspaceEntry, WorkspaceSource};
pub use server::{HubServer, build_router, serve};
