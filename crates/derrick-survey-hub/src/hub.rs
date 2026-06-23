//! The [`Hub`]: a set of open survey indexes keyed by [`WorkspaceId`].
//!
//! Each workspace is opened and built once at startup (connect-time freshness).
//! The map lives behind an [`RwLock`] so the HTTP handler can clone the entry
//! it needs per request without holding the lock across the (async) query.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use derrick_survey::{BuildOptions, BuildReport, IndexStatus, Survey, SurveyConfig, SurveyError};
use tokio::sync::{Mutex, RwLock};
use tokio::time::Instant;

use crate::config::{ConfigError, HubConfig, WorkspaceId, WorkspaceIdError};

/// One hosted workspace: its open index plus the dirty flag that drives the
/// staleness banner. `dirty` is wired in for parity with the stdio server's
/// `respond` contract: phase 2's poll-on-query refresh flips it while a rebuild
/// is in flight so the banner is accurate.
///
/// The entry is cloned per request, but its mutable freshness state lives behind
/// shared `Arc`s so all clones observe the same `last_checked` instant and share
/// the same single-flight `refresh_lock`. Holding a clone never holds the hub's
/// entry-map lock, so a rebuild `.await` cannot block routing of other requests.
#[derive(Clone)]
pub struct WorkspaceEntry {
    /// The open survey index.
    pub survey: Survey,
    /// Whether a rebuild is believed to be in progress (arms the banner).
    pub dirty: Arc<AtomicBool>,
    /// Repository root, retained for diagnostics.
    pub root: PathBuf,
    /// Workspace id, retained for log lines on rebuild.
    pub id: WorkspaceId,
    /// Instant of the last freshness probe; the poll-on-query TTL gate reads it.
    last_checked: Arc<Mutex<Instant>>,
    /// Single-flight guard: only one refresh of this workspace runs at a time.
    /// Concurrent callers serialize here, then re-check `last_checked` and skip.
    refresh_lock: Arc<Mutex<()>>,
}

impl WorkspaceEntry {
    /// Ensure the index is fresh enough to answer a query, honouring `ttl`.
    ///
    /// Fast path: if less than `ttl` has elapsed since the last probe, return
    /// immediately. Otherwise take the single-flight `refresh_lock`, re-check the
    /// elapsed time (another caller may have just refreshed), and if still due,
    /// run a cheap `survey.status()` probe. Only when that reports pending files
    /// is an incremental `survey.build` run; `last_checked` is bumped regardless
    /// so a clean tree is not re-probed until the next TTL window.
    ///
    /// A `ttl` of zero means "always probe": the elapsed gate never short-cuts.
    /// Under concurrency exactly one caller rebuilds; the rest wait on the lock,
    /// observe the fresh `last_checked`, and return without rebuilding.
    pub async fn ensure_fresh(&self, ttl: Duration) -> Result<(), SurveyError> {
        // Fast path: within the TTL window, nothing to do. A zero TTL disables
        // this short-cut (elapsed is always >= zero).
        if ttl > Duration::ZERO {
            let last = *self.last_checked.lock().await;
            if last.elapsed() < ttl {
                return Ok(());
            }
        }

        // Single-flight: serialize concurrent refreshers of this workspace.
        let _refresh = self.refresh_lock.lock().await;

        // Re-check after acquiring the lock: a peer may have refreshed while we
        // waited, in which case we are within the window again and can skip.
        if ttl > Duration::ZERO {
            let last = *self.last_checked.lock().await;
            if last.elapsed() < ttl {
                return Ok(());
            }
        }

        // Cheap staleness probe. Only rebuild when the tree actually differs.
        let status = self.survey.status().await?;
        if !status.pending.is_empty() {
            tracing::debug!(
                workspace = %self.id,
                pending = status.pending.len(),
                "survey hub poll-on-query rebuild"
            );
            self.dirty.store(true, Ordering::Relaxed);
            let result = self.survey.build(BuildOptions::default()).await;
            self.dirty.store(false, Ordering::Relaxed);
            result?;
            tracing::info!(workspace = %self.id, "survey hub rebuilt workspace (poll-on-query)");
        }

        *self.last_checked.lock().await = Instant::now();
        Ok(())
    }

    /// Force an incremental rebuild now, regardless of the TTL window, and
    /// return the post-build status. Backs the `derrick_survey_refresh` tool so
    /// CI can proactively reconcile the index after a known change.
    pub async fn force_refresh(&self) -> Result<IndexStatus, SurveyError> {
        let _refresh = self.refresh_lock.lock().await;
        tracing::info!(workspace = %self.id, "survey hub forced refresh");
        self.dirty.store(true, Ordering::Relaxed);
        let result: Result<BuildReport, SurveyError> =
            self.survey.build(BuildOptions::default()).await;
        self.dirty.store(false, Ordering::Relaxed);
        result?;
        *self.last_checked.lock().await = Instant::now();
        self.survey.status().await
    }
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
    /// Poll-on-query freshness TTL, resolved from the registry config.
    freshness_ttl: Duration,
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
            // Connect-time build just completed, so the index is fresh now.
            entries.insert(
                id.clone(),
                WorkspaceEntry {
                    survey,
                    dirty: Arc::new(AtomicBool::new(false)),
                    root: workspace.root.clone(),
                    id,
                    last_checked: Arc::new(Mutex::new(Instant::now())),
                    refresh_lock: Arc::new(Mutex::new(())),
                },
            );
        }
        Ok(Self {
            entries: Arc::new(RwLock::new(entries)),
            freshness_ttl: Duration::from_secs(config.freshness_ttl_secs),
        })
    }

    /// The poll-on-query freshness TTL this hub was built with.
    pub fn freshness_ttl(&self) -> Duration {
        self.freshness_ttl
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
