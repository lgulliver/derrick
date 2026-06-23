//! The [`Hub`]: a set of open survey indexes keyed by [`WorkspaceId`].
//!
//! Each workspace is opened and built once at startup (connect-time freshness).
//! The map lives behind an [`RwLock`] so the HTTP handler can clone the entry
//! it needs per request without holding the lock across the (async) query.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use derrick_survey::{BuildOptions, Survey, SurveyConfig, SurveyError};
use tokio::sync::RwLock;

use crate::config::{ConfigError, HubConfig, WorkspaceId, WorkspaceIdError};

/// One hosted workspace: its open index plus the dirty flag that drives the
/// staleness banner. `dirty` is wired in for parity with the stdio server's
/// `respond` contract; phase 1 has no watcher to flip it, but a per-call status
/// probe can still surface pending files through the same path.
#[derive(Clone)]
pub struct WorkspaceEntry {
    /// The open survey index.
    pub survey: Survey,
    /// Whether a rebuild is believed to be in progress (arms the banner).
    pub dirty: Arc<AtomicBool>,
    /// Repository root, retained for diagnostics.
    pub root: PathBuf,
}

/// Errors raised while building or querying the hub.
#[derive(Debug, thiserror::Error)]
pub enum HubError {
    /// Opening or building a workspace index failed.
    #[error("workspace {id}: {source}")]
    Workspace {
        /// The workspace that failed.
        id: String,
        /// The underlying survey error.
        source: SurveyError,
    },
    /// Creating the `.derrick` directory failed.
    #[error("workspace {id}: create {path}: {source}")]
    CreateDir {
        /// The workspace that failed.
        id: String,
        /// The directory that could not be created.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// A configured workspace id was invalid.
    #[error("invalid workspace id: {0}")]
    WorkspaceId(#[from] WorkspaceIdError),
    /// The config failed validation (loopback bind, duplicate ids, ...).
    #[error("config: {0}")]
    Config(#[from] ConfigError),
    /// Binding the HTTP listener failed.
    #[error("bind {addr}: {source}")]
    Bind {
        /// The address that could not be bound.
        addr: String,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// The HTTP server stopped with an error.
    #[error("serve: {0}")]
    Serve(#[source] std::io::Error),
}

/// A live set of open survey indexes, one per workspace.
#[derive(Clone)]
pub struct Hub {
    entries: Arc<RwLock<BTreeMap<WorkspaceId, WorkspaceEntry>>>,
}

impl Hub {
    /// Open and build every workspace in `config`.
    ///
    /// For each workspace: ensure the index DB's parent directory exists, open
    /// the index, run a connect-time build, and insert it into the map. Any
    /// failure aborts startup with the offending workspace named.
    pub async fn build(config: &HubConfig) -> Result<Self, HubError> {
        // `build` is public, so re-validate even though `HubConfig::load`
        // already does: this enforces the loopback bind and rejects duplicate
        // ids (which would otherwise silently overwrite in the map below)
        // before any workspace is opened.
        config.validate()?;
        let mut entries = BTreeMap::new();
        for workspace in &config.workspaces {
            let id = WorkspaceId::new(workspace.id.clone())?;
            let db_path = workspace.resolved_db_path();
            if let Some(parent) = db_path.parent() {
                std::fs::create_dir_all(parent).map_err(|source| HubError::CreateDir {
                    id: id.to_string(),
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
            let survey = Survey::open(SurveyConfig {
                db_path,
                repo_root: workspace.root.clone(),
                reader_pool: SurveyConfig::DEFAULT_READER_POOL,
            })
            .await
            .map_err(|source| HubError::Workspace {
                id: id.to_string(),
                source,
            })?;
            // Connect-time freshness: reconcile the index with the tree once.
            survey
                .build(BuildOptions::default())
                .await
                .map_err(|source| HubError::Workspace {
                    id: id.to_string(),
                    source,
                })?;
            tracing::info!(workspace = %id, root = %workspace.root.display(), "survey hub opened workspace");
            entries.insert(
                id,
                WorkspaceEntry {
                    survey,
                    dirty: Arc::new(AtomicBool::new(false)),
                    root: workspace.root.clone(),
                },
            );
        }
        Ok(Self {
            entries: Arc::new(RwLock::new(entries)),
        })
    }

    /// Clone the entry for `id`, if hosted. The clone is cheap (`Survey` is an
    /// `Arc` handle) and lets the caller release the read lock before awaiting
    /// the query.
    pub async fn entry(&self, id: &WorkspaceId) -> Option<WorkspaceEntry> {
        self.entries.read().await.get(id).cloned()
    }

    /// The ids of all hosted workspaces, sorted.
    pub async fn workspace_ids(&self) -> Vec<WorkspaceId> {
        self.entries.read().await.keys().cloned().collect()
    }
}
