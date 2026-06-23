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
//! per-repo filesystem watcher and no auth; the server binds to a loopback
//! address from the registry.

mod config;
mod hub;
mod server;

pub use config::{ConfigError, HubConfig, WorkspaceConfig, WorkspaceId, WorkspaceIdError};
pub use hub::{Hub, HubError, WorkspaceEntry};
pub use server::{HubServer, serve};
