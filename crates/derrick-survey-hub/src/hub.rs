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

use crate::config::{ConfigError, HubConfig, WorkspaceId, WorkspaceIdError, WorkspaceSourceConfig};

/// How a hosted workspace's index is sourced (D82), with paths already resolved
/// to absolute form. The query layer (`search`/`context`/`impact`/`status`) is
/// identical across sources; only the build/refresh path branches.
#[derive(Clone, Debug)]
pub enum WorkspaceSource {
    /// The hub holds a working tree and builds/refreshes the index itself.
    Local {
        /// Absolute repository root the index covers.
        root: PathBuf,
    },
    /// The hub serves a prebuilt `.db` placed on disk by an operator or CI, and
    /// hot-swaps it atomically when the file changes.
    Pushed {
        /// Absolute path to the prebuilt index `.db`.
        db_path: PathBuf,
    },
}

impl WorkspaceSource {
    /// A short, log-friendly tag for the source kind.
    pub fn kind(&self) -> &'static str {
        match self {
            WorkspaceSource::Local { .. } => "local",
            WorkspaceSource::Pushed { .. } => "pushed",
        }
    }

    /// Whether this workspace is backed by a working tree, which decides whether
    /// the tree-vs-index staleness banner is meaningful. `Local` holds a tree;
    /// `Pushed` serves a prebuilt `.db` with no tree, so the banner is bogus and
    /// must be suppressed.
    pub fn is_tree_backed(&self) -> bool {
        matches!(self, WorkspaceSource::Local { .. })
    }
}

/// One hosted workspace: its open index plus the dirty flag that drives the
/// staleness banner. `dirty` is wired in for parity with the stdio server's
/// `respond` contract: phase 2's poll-on-query refresh flips it while a rebuild
/// is in flight so the banner is accurate.
///
/// The entry is cloned per request, but its mutable freshness state lives behind
/// shared `Arc`s so all clones observe the same `last_checked` instant and share
/// the same single-flight `refresh_lock`. Holding a clone never holds the hub's
/// entry-map lock, so a rebuild `.await` cannot block routing of other requests.
///
/// The served index lives behind an `RwLock` so a Pushed workspace can hot-swap
/// it without tearing in-flight queries: the query path clones the [`Survey`]
/// out under a short read guard (the clone is a cheap `Arc` bump) and then runs
/// the query lock-free, a Local incremental rebuild runs under a read guard
/// (`Survey::build` takes `&self`), and a Pushed reload swaps the handle under a
/// write guard. No lock is ever held across the query `.await`.
#[derive(Clone)]
pub struct WorkspaceEntry {
    /// The open survey index, swappable for Pushed hot-reload.
    survey: Arc<RwLock<Survey>>,
    /// Whether a rebuild/reload is believed to be in progress (arms the banner).
    pub dirty: Arc<AtomicBool>,
    /// How this workspace is sourced; the build/refresh path branches on it.
    source: WorkspaceSource,
    /// Workspace id, retained for log lines on rebuild.
    pub id: WorkspaceId,
    /// Instant of the last freshness probe; the poll-on-query TTL gate reads it.
    last_checked: Arc<Mutex<Instant>>,
    /// Single-flight guard: only one refresh of this workspace runs at a time.
    /// Concurrent callers serialize here, then re-check `last_checked` and skip.
    refresh_lock: Arc<Mutex<()>>,
    /// Pushed-only: `(len, mtime)` of the prebuilt `.db` as of the last load, so
    /// the freshness probe can detect an external rewrite and reload. `None` for
    /// Local workspaces and until the first Pushed load records a stat.
    last_pushed_stat: Arc<Mutex<Option<(u64, u128)>>>,
}

/// RAII guard that arms the `dirty` banner for the duration of a rebuild and
/// clears it on drop. Using a guard (rather than a manual `store(false)` after
/// the `.await`) keeps the flag correct even if the rebuild future is cancelled
/// mid-await — otherwise a dropped request would leave the workspace marked as
/// rebuilding forever.
struct DirtyGuard<'a>(&'a AtomicBool);

impl<'a> DirtyGuard<'a> {
    fn arm(flag: &'a AtomicBool) -> Self {
        flag.store(true, Ordering::Relaxed);
        Self(flag)
    }
}

impl Drop for DirtyGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

impl WorkspaceEntry {
    /// Clone the served [`Survey`] handle out from under a short read guard.
    ///
    /// The clone is a cheap `Arc` bump; the caller then runs the (async) query
    /// lock-free, so a concurrent Pushed hot-swap never tears an in-flight read.
    /// The read guard is released before this returns, so it is never held
    /// across a query `.await`.
    pub async fn survey(&self) -> Survey {
        self.survey.read().await.clone()
    }

    /// How this workspace is sourced (Local vs Pushed), for diagnostics.
    pub fn source(&self) -> &WorkspaceSource {
        &self.source
    }

    /// Freshness and size summary for this workspace, branching on its source.
    ///
    /// - **Local** — `status_with_flag(dirty)`, which diffs the working tree the
    ///   hub holds, so `pending` reflects real drift between the tree and the
    ///   index; the `dirty` flag propagates the in-flight-rebuild state into the
    ///   freshness label (preserving the `answer_status` contract).
    /// - **Pushed** — `stats()`, which reports the prebuilt index's counts
    ///   without a tree diff. A pushed index has no working tree (its
    ///   `repo_root` is just the DB's parent dir), so `status()` would
    ///   spuriously report every indexed file as `deleted` and read `stale`;
    ///   `stats()` returns an empty `pending` and a fresh label, which is
    ///   correct — a pushed index *is* exactly what was built.
    ///
    /// Clones the served [`Survey`] out under a short read guard, then runs the
    /// query lock-free (see [`Self::survey`]).
    pub async fn status(&self) -> Result<IndexStatus, SurveyError> {
        let survey = self.survey().await;
        match &self.source {
            WorkspaceSource::Local { .. } => {
                let rebuilding = self.dirty.load(Ordering::Relaxed);
                survey.status_with_flag(rebuilding).await
            }
            WorkspaceSource::Pushed { .. } => survey.stats().await,
        }
    }

    /// Ensure the index is fresh enough to answer a query, honouring `ttl`.
    ///
    /// Fast path: if less than `ttl` has elapsed since the last probe, return
    /// immediately. Otherwise take the single-flight `refresh_lock`, re-check the
    /// elapsed time (another caller may have just refreshed), and if still due,
    /// reconcile by source:
    /// - **Local** — run a cheap `status()` probe and only run an incremental
    ///   `build` when the working tree differs.
    /// - **Pushed** — stat the prebuilt `.db` and only reload+swap when its
    ///   `mtime`/`len` changed since the last load.
    ///
    /// `last_checked` is bumped regardless so a clean workspace is not re-probed
    /// until the next TTL window. A `ttl` of zero means "always probe": the
    /// elapsed gate never short-cuts. Under concurrency exactly one caller
    /// reconciles; the rest wait on the lock, observe the fresh `last_checked`,
    /// and return without work.
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

        match &self.source {
            WorkspaceSource::Local { .. } => {
                // Cheap staleness probe. Only rebuild when the tree differs.
                let status = self.survey().await.status().await?;
                if !status.pending.is_empty() {
                    tracing::debug!(
                        workspace = %self.id,
                        source = "local",
                        pending = status.pending.len(),
                        "survey hub poll-on-query rebuild"
                    );
                    let survey = self.survey().await;
                    let _dirty = DirtyGuard::arm(&self.dirty);
                    survey.build(BuildOptions::default()).await?;
                    tracing::info!(
                        workspace = %self.id,
                        source = "local",
                        "survey hub rebuilt workspace (poll-on-query)"
                    );
                }
            }
            WorkspaceSource::Pushed { db_path } => {
                self.reload_pushed_if_changed(db_path).await?;
            }
        }

        *self.last_checked.lock().await = Instant::now();
        Ok(())
    }

    /// Force a refresh now, regardless of the TTL window, and return the
    /// post-refresh status. Backs the `derrick_survey_refresh` tool.
    ///
    /// - **Local** — unconditional incremental rebuild (CI/git-hook reconcile).
    /// - **Pushed** — unconditional reload+swap of the prebuilt `.db` from disk.
    pub async fn force_refresh(&self) -> Result<IndexStatus, SurveyError> {
        let _refresh = self.refresh_lock.lock().await;
        match &self.source {
            WorkspaceSource::Local { .. } => {
                tracing::info!(workspace = %self.id, source = "local", "survey hub forced refresh");
                let survey = self.survey().await;
                let _report: BuildReport = {
                    let _dirty = DirtyGuard::arm(&self.dirty);
                    survey.build(BuildOptions::default()).await?
                };
            }
            WorkspaceSource::Pushed { db_path } => {
                tracing::info!(
                    workspace = %self.id,
                    source = "pushed",
                    "survey hub forced reload"
                );
                self.reload_pushed(db_path).await?;
            }
        }
        *self.last_checked.lock().await = Instant::now();
        self.status().await
    }

    /// Reload a Pushed workspace only when the on-disk `.db` changed.
    ///
    /// Compares cheap stat metadata (`mtime` + `len`) of the currently served
    /// index against the file on disk and reloads only on a difference. The stat
    /// of the served index uses the same `db_path` (the file the swap reads
    /// from), so this is a self-comparison that detects an external rewrite.
    async fn reload_pushed_if_changed(&self, db_path: &std::path::Path) -> Result<(), SurveyError> {
        // Snapshot what we last loaded vs. what's on disk now. We compare the
        // file's own previous stat (recorded at load) against its current stat.
        let current = stat_db(db_path);
        let last = *self.last_pushed_stat.lock().await;
        if current == last {
            tracing::debug!(
                workspace = %self.id,
                source = "pushed",
                "survey hub pushed db unchanged; no reload"
            );
            return Ok(());
        }
        self.reload_pushed(db_path).await
    }

    /// Open a fresh [`Survey`] over the prebuilt `.db` (with a stat that matches
    /// the opened handle — see [`open_pushed_survey_stable`]) and atomically swap
    /// it in, dropping the old handle. Arms the banner for the duration of the
    /// swap, and records the loaded stat *before* swapping so a concurrent probe
    /// after the swap compares against the right baseline.
    async fn reload_pushed(&self, db_path: &std::path::Path) -> Result<(), SurveyError> {
        let _dirty = DirtyGuard::arm(&self.dirty);
        let (fresh, loaded_stat) = open_pushed_survey_stable(db_path).await?;
        *self.last_pushed_stat.lock().await = loaded_stat;
        *self.survey.write().await = fresh;
        tracing::info!(
            workspace = %self.id,
            source = "pushed",
            db = %db_path.display(),
            "survey hub reloaded pushed index"
        );
        Ok(())
    }
}

/// Cheap change-detection stat for a pushed `.db`: `(len, mtime_nanos)`, or
/// `None` if the file is missing/unstattable. A change in either field triggers
/// a reload. Nanosecond precision (not whole seconds) so two pushes within the
/// same second with an identical byte-length are still detected.
fn stat_db(path: &std::path::Path) -> Option<(u64, u128)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0_u128, |d| d.as_nanos());
    Some((meta.len(), mtime))
}

/// Open a [`Survey`] over a prebuilt index `.db` for serving (Pushed mode).
///
/// `repo_root` is set to the DB's parent directory: it is only consulted by the
/// working-tree-diffing `status()` query, which has no tree to diff against for
/// a pushed index, so search/context/impact are unaffected. Schema portability
/// is enforced by the survey crate: a pushed DB whose `user_version` exceeds the
/// supported schema surfaces as `SurveyError::SchemaTooNew`.
///
/// The file must already exist: a Pushed source serves an externally-produced
/// DB, so a missing path is an operator error, not a request to create an empty
/// index. We surface that as a clear I/O error rather than letting SQLite
/// create-and-migrate a blank DB that would silently serve no symbols. Returns
/// a [`SurveyError`] so the reload path (which has no `HubError`) shares it; the
/// connect-time caller wraps it in [`HubError::Workspace`] to name the workspace.
async fn open_pushed_survey(db_path: &std::path::Path) -> Result<Survey, SurveyError> {
    if !db_path.is_file() {
        return Err(SurveyError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("pushed index db not found: {}", db_path.display()),
        )));
    }
    let repo_root = db_path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    Survey::open(SurveyConfig {
        db_path: db_path.to_path_buf(),
        repo_root,
        reader_pool: SurveyConfig::DEFAULT_READER_POOL,
    })
    .await
}

/// Open the pushed DB and return a `(len, mtime)` stat that is stable across the
/// open, so the recorded stat always matches the served handle.
///
/// Guards a time-of-check/time-of-use race shared by the connect-time open and
/// the hot-reload path: a producer that atomically replaces `db_path` *between*
/// the open and the stat would leave the caller serving the older DB while
/// recording the newer file's stat — so every later probe sees "unchanged" and
/// never hot-swaps the newer index. We re-open until the pre- and post-open
/// stats match (bounded); a perpetually-churning file falls back to the pre-open
/// stat, which simply biases the next probe toward reloading again rather than
/// skipping.
async fn open_pushed_survey_stable(
    db_path: &std::path::Path,
) -> Result<(Survey, Option<(u64, u128)>), SurveyError> {
    const MAX_TRIES: usize = 5;
    let mut chosen: Option<(Survey, Option<(u64, u128)>)> = None;
    for _ in 0..MAX_TRIES {
        let before = stat_db(db_path);
        let fresh = open_pushed_survey(db_path).await?;
        let after = stat_db(db_path);
        if before == after {
            // Stable across the open: the recorded stat matches this DB.
            return Ok((fresh, after));
        }
        // Replaced mid-open: keep this handle but record the pre-open stat so
        // the next probe still sees a difference and reloads again.
        chosen = Some((fresh, before));
        tracing::debug!(
            db = %db_path.display(),
            "survey hub pushed db changed during open; retrying"
        );
    }
    Ok(chosen.expect("the open loop runs at least once"))
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
            let workspace_err = |source: SurveyError| HubError::Workspace {
                id: id.to_string(),
                source,
            };
            let source = workspace.source()?;
            let (survey, resolved_source, last_pushed_stat) = match source {
                WorkspaceSourceConfig::Local { root, db_path } => {
                    // Ensure the index DB's parent directory exists, open it, and
                    // run a connect-time build so the index is fresh on startup.
                    if let Some(parent) = db_path.parent() {
                        std::fs::create_dir_all(parent).map_err(|err| HubError::CreateDir {
                            id: id.to_string(),
                            path: parent.to_path_buf(),
                            source: err,
                        })?;
                    }
                    let survey = Survey::open(SurveyConfig {
                        db_path,
                        repo_root: root.clone(),
                        reader_pool: SurveyConfig::DEFAULT_READER_POOL,
                    })
                    .await
                    .map_err(workspace_err)?;
                    survey
                        .build(BuildOptions::default())
                        .await
                        .map_err(workspace_err)?;
                    tracing::info!(
                        workspace = %id,
                        source = "local",
                        root = %root.display(),
                        "survey hub opened workspace"
                    );
                    (survey, WorkspaceSource::Local { root }, None)
                }
                WorkspaceSourceConfig::Pushed { db_path } => {
                    // Serve the prebuilt DB as-is; do NOT build from a root. A
                    // schema-too-new or open failure surfaces as a workspace error.
                    // The stable open records a stat matching the served handle,
                    // closing the same TOCTOU the reload path guards against.
                    let (survey, stat) = open_pushed_survey_stable(&db_path)
                        .await
                        .map_err(workspace_err)?;
                    tracing::info!(
                        workspace = %id,
                        source = "pushed",
                        db = %db_path.display(),
                        "survey hub opened workspace"
                    );
                    (survey, WorkspaceSource::Pushed { db_path }, stat)
                }
            };
            // Connect-time open (Local also built) just completed: index is fresh.
            entries.insert(
                id.clone(),
                WorkspaceEntry {
                    survey: Arc::new(RwLock::new(survey)),
                    dirty: Arc::new(AtomicBool::new(false)),
                    source: resolved_source,
                    id,
                    last_checked: Arc::new(Mutex::new(Instant::now())),
                    refresh_lock: Arc::new(Mutex::new(())),
                    last_pushed_stat: Arc::new(Mutex::new(last_pushed_stat)),
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
