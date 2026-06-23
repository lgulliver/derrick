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
///
/// A workspace is sourced in exactly one of two ways (D82):
/// - **Local** — `root` points at a working tree on the hub's disk; the hub
///   builds and refreshes the index itself. `db_path` is optional and defaults
///   to `<root>/.derrick/index.db`. This is the original, unchanged shape:
///   `{ id, root, db_path? }`.
/// - **Pushed** — `pushed_db` points at a prebuilt `.db` that an operator or CI
///   places on disk; the hub opens and serves it (no `root`, no building). The
///   shape is `{ id, pushed_db }`.
///
/// Kept as a flat struct (rather than a tagged enum) so the historical
/// `{ id, root }` / `{ id, root, db_path }` YAML keeps parsing byte-for-byte;
/// the Local-xor-Pushed invariant is enforced in [`WorkspaceConfig::source`]
/// and re-checked by [`HubConfig::validate`].
#[derive(Clone, Debug, Deserialize)]
pub struct WorkspaceConfig {
    /// Identifier used to route tool calls to this workspace.
    pub id: String,
    /// Local mode: repository root the index covers. Mutually exclusive with
    /// `pushed_db`.
    #[serde(default)]
    pub root: Option<PathBuf>,
    /// Local mode only: index DB path. Defaults to `<root>/.derrick/index.db`.
    /// Ignored (and meaningless) for Pushed workspaces, which name their DB via
    /// `pushed_db`.
    #[serde(default)]
    pub db_path: Option<PathBuf>,
    /// Pushed mode: path to a prebuilt index `.db` to open and serve. Mutually
    /// exclusive with `root`.
    #[serde(default)]
    pub pushed_db: Option<PathBuf>,
}

/// How a workspace's index is sourced, resolved from a [`WorkspaceConfig`].
///
/// This is the config-layer twin of `hub::WorkspaceSource`; it carries the
/// paths exactly as written in `hub.yaml` (the hub resolves them to absolute
/// paths when it opens the workspace).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceSourceConfig {
    /// Build and refresh the index from a working tree on disk.
    Local {
        /// Repository root the index covers.
        root: PathBuf,
        /// Resolved index DB path (`db_path` override or the convention).
        db_path: PathBuf,
    },
    /// Serve a prebuilt `.db` placed on disk by an operator or CI.
    Pushed {
        /// Path to the prebuilt index `.db`.
        db_path: PathBuf,
    },
}

impl WorkspaceConfig {
    /// Resolve the source for this workspace, enforcing the Local-xor-Pushed
    /// invariant: exactly one of `root` / `pushed_db` must be set.
    ///
    /// For Local, the DB path follows `db_path` or the `<root>/.derrick/index.db`
    /// convention. For Pushed, `db_path` is rejected (it only applies to Local).
    pub fn source(&self) -> Result<WorkspaceSourceConfig, ConfigError> {
        match (&self.root, &self.pushed_db) {
            (Some(_), Some(_)) => Err(ConfigError::SourceBothSet {
                id: self.id.clone(),
            }),
            (None, None) => Err(ConfigError::SourceNeitherSet {
                id: self.id.clone(),
            }),
            (Some(root), None) => Ok(WorkspaceSourceConfig::Local {
                root: root.clone(),
                db_path: self
                    .db_path
                    .clone()
                    .unwrap_or_else(|| default_db_path(root)),
            }),
            (None, Some(pushed_db)) => {
                if self.db_path.is_some() {
                    return Err(ConfigError::PushedDbPathSet {
                        id: self.id.clone(),
                    });
                }
                Ok(WorkspaceSourceConfig::Pushed {
                    db_path: pushed_db.clone(),
                })
            }
        }
    }

    /// Resolve the index DB path. For Local this is the `db_path` override or
    /// `<root>/.derrick/index.db`; for Pushed it is `pushed_db`. Returns `None`
    /// only when neither source is configured (caught by validation).
    pub fn resolved_db_path(&self) -> Option<PathBuf> {
        self.source().ok().map(|source| match source {
            WorkspaceSourceConfig::Local { db_path, .. } => db_path,
            WorkspaceSourceConfig::Pushed { db_path } => db_path,
        })
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
    /// A workspace set both `root` and `pushed_db`; the source is ambiguous.
    #[error("workspace {id}: set exactly one of `root` (Local) or `pushed_db` (Pushed), not both")]
    SourceBothSet {
        /// The offending workspace id.
        id: String,
    },
    /// A workspace set neither `root` nor `pushed_db`; there is no index to serve.
    #[error("workspace {id}: set one of `root` (Local) or `pushed_db` (Pushed)")]
    SourceNeitherSet {
        /// The offending workspace id.
        id: String,
    },
    /// A Pushed workspace set `db_path`, which is only meaningful for Local.
    #[error("workspace {id}: `db_path` applies only to Local workspaces; Pushed uses `pushed_db`")]
    PushedDbPathSet {
        /// The offending workspace id.
        id: String,
    },
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
            // Enforce the Local-xor-Pushed invariant up front so the hub never
            // half-starts on an ambiguous workspace.
            workspace.source()?;
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
            config.workspaces[0].resolved_db_path().unwrap(),
            Path::new("/srv/a/.derrick/index.db")
        );
        assert_eq!(
            config.workspaces[1].resolved_db_path().unwrap(),
            Path::new("/custom/b.db")
        );
        // The historical `{ id, root }` shape resolves to a Local source.
        assert!(matches!(
            config.workspaces[0].source().unwrap(),
            WorkspaceSourceConfig::Local { .. }
        ));
    }

    #[test]
    fn parses_a_pushed_workspace() {
        let yaml =
            "bind: 127.0.0.1:7777\nworkspaces:\n  - id: pushed\n    pushed_db: /srv/prebuilt.db\n";
        let config: HubConfig = serde_yaml::from_str(yaml).unwrap();
        config.validate().unwrap();
        assert_eq!(
            config.workspaces[0].source().unwrap(),
            WorkspaceSourceConfig::Pushed {
                db_path: PathBuf::from("/srv/prebuilt.db"),
            }
        );
        assert_eq!(
            config.workspaces[0].resolved_db_path().unwrap(),
            Path::new("/srv/prebuilt.db")
        );
    }

    #[test]
    fn rejects_both_root_and_pushed_db() {
        let yaml = "bind: 127.0.0.1:7777\nworkspaces:\n  - id: a\n    root: /srv/a\n    pushed_db: /srv/a.db\n";
        let config: HubConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(matches!(
            config.validate(),
            Err(ConfigError::SourceBothSet { .. })
        ));
    }

    #[test]
    fn rejects_neither_root_nor_pushed_db() {
        let yaml = "bind: 127.0.0.1:7777\nworkspaces:\n  - id: a\n";
        let config: HubConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(matches!(
            config.validate(),
            Err(ConfigError::SourceNeitherSet { .. })
        ));
    }

    #[test]
    fn rejects_db_path_on_pushed_workspace() {
        let yaml = "bind: 127.0.0.1:7777\nworkspaces:\n  - id: a\n    pushed_db: /srv/a.db\n    db_path: /srv/other.db\n";
        let config: HubConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(matches!(
            config.validate(),
            Err(ConfigError::PushedDbPathSet { .. })
        ));
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
