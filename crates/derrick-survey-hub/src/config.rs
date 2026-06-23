//! Hub registry config: the `hub.yaml` parsed into a bind address plus the list
//! of workspaces the hub should host.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// A validated workspace identifier: non-empty and free of whitespace, so it is
/// safe to surface in tool errors and log lines without ambiguity.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkspaceId(String);

/// Reasons a [`WorkspaceId`] string is rejected.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WorkspaceIdError {
    /// The identifier was empty.
    #[error("workspace id must not be empty")]
    Empty,
    /// The identifier contained whitespace.
    #[error("workspace id must not contain whitespace: {0:?}")]
    Whitespace(String),
}

impl WorkspaceId {
    /// Validate and wrap a workspace id.
    pub fn new(value: impl Into<String>) -> Result<Self, WorkspaceIdError> {
        let value = value.into();
        if value.is_empty() {
            return Err(WorkspaceIdError::Empty);
        }
        if value.chars().any(char::is_whitespace) {
            return Err(WorkspaceIdError::Whitespace(value));
        }
        Ok(Self(value))
    }

    /// The underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One workspace entry as written in `hub.yaml`.
#[derive(Clone, Debug, Deserialize)]
pub struct WorkspaceConfig {
    /// Identifier used to route tool calls to this workspace.
    pub id: String,
    /// Repository root the index covers.
    pub root: PathBuf,
    /// Index DB path. Defaults to `<root>/.derrick/index.db`.
    #[serde(default)]
    pub db_path: Option<PathBuf>,
}

impl WorkspaceConfig {
    /// Resolve the index DB path, defaulting to `<root>/.derrick/index.db` to
    /// match the single-repo CLI's `index_db_path`.
    pub fn resolved_db_path(&self) -> PathBuf {
        self.db_path
            .clone()
            .unwrap_or_else(|| default_db_path(&self.root))
    }
}

/// Default index DB path for a repo root: `<root>/.derrick/index.db` (D11).
pub fn default_db_path(root: &Path) -> PathBuf {
    root.join(".derrick").join("index.db")
}

/// The parsed `hub.yaml` registry.
#[derive(Clone, Debug, Deserialize)]
pub struct HubConfig {
    /// Loopback address to bind the HTTP server to (e.g. `127.0.0.1:7000`).
    pub bind: SocketAddr,
    /// Poll-on-query freshness TTL, in seconds.
    ///
    /// A read tool only re-probes a workspace for staleness once this many
    /// seconds have elapsed since its last check; within the window, queries
    /// skip the probe and answer from the open index. A value of `0` means
    /// "always probe on every query" (no caching of the freshness check).
    ///
    /// Defaults to [`HubConfig::DEFAULT_FRESHNESS_TTL_SECS`]. Omitting the
    /// field in `hub.yaml` is supported for backward compatibility.
    #[serde(default = "HubConfig::default_freshness_ttl_secs")]
    pub freshness_ttl_secs: u64,
    /// Workspaces this hub hosts.
    pub workspaces: Vec<WorkspaceConfig>,
}

/// Reasons loading a [`HubConfig`] fails.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// Reading the file failed.
    #[error("read {path}: {source}")]
    Read {
        /// The path that could not be read.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// Parsing the YAML failed.
    #[error("parse hub config: {0}")]
    Parse(#[from] serde_yaml::Error),
    /// The registry listed no workspaces.
    #[error("hub config lists no workspaces")]
    NoWorkspaces,
    /// A workspace id was invalid.
    #[error("invalid workspace id: {0}")]
    WorkspaceId(#[from] WorkspaceIdError),
    /// Two workspaces shared an id.
    #[error("duplicate workspace id: {0}")]
    DuplicateId(String),
    /// The bind address was not a loopback address. Phase 1 has no auth, so a
    /// non-loopback bind would expose every workspace's tools on the network.
    #[error("hub bind must be a loopback address in phase 1 (no auth yet): {0}")]
    NonLoopbackBind(SocketAddr),
}

impl HubConfig {
    /// Default poll-on-query freshness TTL when `hub.yaml` omits the field.
    pub const DEFAULT_FRESHNESS_TTL_SECS: u64 = 60;

    /// Serde default for [`HubConfig::freshness_ttl_secs`].
    fn default_freshness_ttl_secs() -> u64 {
        Self::DEFAULT_FRESHNESS_TTL_SECS
    }

    /// Load and validate a `hub.yaml` from disk.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let config: HubConfig = serde_yaml::from_str(&text)?;
        config.validate()?;
        Ok(config)
    }

    /// Reject empty registries, invalid ids, and duplicate ids early so the
    /// hub never half-starts.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !self.bind.ip().is_loopback() {
            return Err(ConfigError::NonLoopbackBind(self.bind));
        }
        if self.workspaces.is_empty() {
            return Err(ConfigError::NoWorkspaces);
        }
        let mut seen = std::collections::HashSet::new();
        for workspace in &self.workspaces {
            let id = WorkspaceId::new(workspace.id.clone())?;
            if !seen.insert(id.as_str().to_owned()) {
                return Err(ConfigError::DuplicateId(id.to_string()));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_id_rejects_empty_and_whitespace() {
        assert_eq!(WorkspaceId::new(""), Err(WorkspaceIdError::Empty));
        assert!(matches!(
            WorkspaceId::new("has space"),
            Err(WorkspaceIdError::Whitespace(_))
        ));
        assert_eq!(WorkspaceId::new("repo-a").unwrap().as_str(), "repo-a");
    }

    #[test]
    fn default_db_path_matches_convention() {
        let root = Path::new("/srv/repo");
        assert_eq!(
            default_db_path(root),
            Path::new("/srv/repo/.derrick/index.db")
        );
    }

    #[test]
    fn parses_a_minimal_registry() {
        let yaml = "bind: 127.0.0.1:7777\nworkspaces:\n  - id: a\n    root: /srv/a\n  - id: b\n    root: /srv/b\n    db_path: /custom/b.db\n";
        let config: HubConfig = serde_yaml::from_str(yaml).unwrap();
        config.validate().unwrap();
        assert_eq!(config.bind.port(), 7777);
        assert_eq!(config.workspaces.len(), 2);
        assert_eq!(
            config.workspaces[0].resolved_db_path(),
            Path::new("/srv/a/.derrick/index.db")
        );
        assert_eq!(
            config.workspaces[1].resolved_db_path(),
            Path::new("/custom/b.db")
        );
    }

    #[test]
    fn rejects_duplicate_ids() {
        let yaml = "bind: 127.0.0.1:7777\nworkspaces:\n  - id: a\n    root: /srv/a\n  - id: a\n    root: /srv/a2\n";
        let config: HubConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(matches!(
            config.validate(),
            Err(ConfigError::DuplicateId(_))
        ));
    }

    #[test]
    fn rejects_empty_registry() {
        let yaml = "bind: 127.0.0.1:7777\nworkspaces: []\n";
        let config: HubConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(matches!(config.validate(), Err(ConfigError::NoWorkspaces)));
    }

    #[test]
    fn freshness_ttl_defaults_when_omitted() {
        // Backward compatibility: a registry written before the TTL field still
        // parses, falling back to the documented default.
        let yaml = "bind: 127.0.0.1:7777\nworkspaces:\n  - id: a\n    root: /srv/a\n";
        let config: HubConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            config.freshness_ttl_secs,
            HubConfig::DEFAULT_FRESHNESS_TTL_SECS
        );
    }

    #[test]
    fn freshness_ttl_is_parsed_when_present() {
        let yaml = "bind: 127.0.0.1:7777\nfreshness_ttl_secs: 0\nworkspaces:\n  - id: a\n    root: /srv/a\n";
        let config: HubConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.freshness_ttl_secs, 0);
    }

    #[test]
    fn rejects_non_loopback_bind() {
        let yaml = "bind: 0.0.0.0:7777\nworkspaces:\n  - id: a\n    root: /srv/a\n";
        let config: HubConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(matches!(
            config.validate(),
            Err(ConfigError::NonLoopbackBind(_))
        ));
    }
}
