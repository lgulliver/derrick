//! `derrick-survey` — native code-graph index (DESIGN.md §9.B.8, D54/D55).
//!
//! A SQLite + FTS5 index of repository symbols, references, and call
//! relationships that AI agents query directly instead of fanning out across
//! `grep`/`glob`/`Read`. The index lives at `.derrick/index.db`, distinct from
//! the substrate DB, and is queried over an MCP server or the `derrick survey`
//! CLI subcommands.

use std::path::PathBuf;

mod build;
mod db;
mod mcp;
mod model;
mod parse;
mod query;
mod walk;
mod watch;

pub use mcp::serve_stdio;

pub use model::{
    BuildOptions, BuildReport, ImpactSet, IndexStatus, Lang, PendingFile, RefKind, SymbolContext,
    SymbolHit, SymbolKind,
};

/// Errors raised by the survey crate.
#[derive(Debug, thiserror::Error)]
pub enum SurveyError {
    /// A SQLite operation failed.
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// An I/O operation failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// A worker task panicked or was cancelled.
    #[error("background task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
    /// The filesystem watcher failed.
    #[error("watch error: {0}")]
    Watch(#[from] notify::Error),
    /// The on-disk schema is newer than this binary supports.
    #[error("index schema version {found} is newer than supported version {supported}")]
    SchemaTooNew {
        /// Version found on disk.
        found: u32,
        /// Highest version this binary understands.
        supported: u32,
    },
    /// An invariant was violated.
    #[error("{0}")]
    Internal(String),
}

/// Configuration for opening a [`Survey`] index.
#[derive(Clone, Debug)]
pub struct SurveyConfig {
    /// Path to the index SQLite file (conventionally `.derrick/index.db`).
    pub db_path: PathBuf,
    /// Repository root that the index covers.
    pub repo_root: PathBuf,
    /// Number of pooled read-only connections.
    pub reader_pool: usize,
}

impl SurveyConfig {
    /// Default reader-pool size, mirroring `derrick-substrate-native`.
    pub const DEFAULT_READER_POOL: usize = 4;
}

/// A handle to an open survey index.
///
/// Serializes writes through a tokio mutex token and opens a fresh writer
/// connection per write (mirroring `derrick-substrate-native`); reads go
/// through a pool of read-only connections. All SQLite and tree-sitter work
/// runs inside `spawn_blocking`, since both are synchronous.
#[derive(Clone)]
pub struct Survey {
    inner: std::sync::Arc<SurveyInner>,
}

struct SurveyInner {
    repo_root: PathBuf,
    db_path: PathBuf,
    write_lock: tokio::sync::Mutex<()>,
    readers: std::sync::Arc<db::ReaderPool>,
}

impl Survey {
    /// Open (creating and migrating if necessary) the index at the configured
    /// path. The parent directory must already exist.
    pub async fn open(config: SurveyConfig) -> Result<Self, SurveyError> {
        let SurveyConfig {
            db_path,
            repo_root,
            reader_pool,
        } = config;
        let inner = tokio::task::spawn_blocking(move || -> Result<SurveyInner, SurveyError> {
            // Create the file, migrate, and enable WAL before any reader opens.
            drop(db::open_writer_connection(&db_path)?);
            let readers = std::sync::Arc::new(db::ReaderPool::new(db_path.clone(), reader_pool)?);
            Ok(SurveyInner {
                repo_root,
                db_path,
                write_lock: tokio::sync::Mutex::new(()),
                readers,
            })
        })
        .await??;
        Ok(Self {
            inner: std::sync::Arc::new(inner),
        })
    }

    /// Repository root this index covers.
    pub fn repo_root(&self) -> &std::path::Path {
        &self.inner.repo_root
    }

    /// (Re)build the index from the working tree.
    pub async fn build(&self, options: BuildOptions) -> Result<BuildReport, SurveyError> {
        let _guard = self.inner.write_lock.lock().await;
        let db_path = self.inner.db_path.clone();
        let repo_root = self.inner.repo_root.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = db::open_writer_connection(&db_path)?;
            build::run(&mut connection, &repo_root, options)
        })
        .await?
    }

    /// Full-text search over symbol names and signatures.
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<SymbolHit>, SurveyError> {
        let readers = std::sync::Arc::clone(&self.inner.readers);
        let query = query.to_owned();
        tokio::task::spawn_blocking(move || {
            let lease = readers.lease()?;
            query::search(lease.connection()?, &query, limit)
        })
        .await?
    }

    /// Resolve a query to entry-point symbols plus the symbols they reference.
    pub async fn context(&self, query: &str, limit: usize) -> Result<SymbolContext, SurveyError> {
        let readers = std::sync::Arc::clone(&self.inner.readers);
        let query = query.to_owned();
        tokio::task::spawn_blocking(move || {
            let lease = readers.lease()?;
            query::context(lease.connection()?, &query, limit)
        })
        .await?
    }

    /// Resolve a symbol name to its direct callers and callees.
    pub async fn impact(&self, symbol: &str) -> Result<Option<ImpactSet>, SurveyError> {
        let readers = std::sync::Arc::clone(&self.inner.readers);
        let symbol = symbol.to_owned();
        tokio::task::spawn_blocking(move || {
            let lease = readers.lease()?;
            query::impact(lease.connection()?, &symbol)
        })
        .await?
    }

    /// Freshness and size summary, including files that differ from the index.
    pub async fn status(&self) -> Result<IndexStatus, SurveyError> {
        let readers = std::sync::Arc::clone(&self.inner.readers);
        let repo_root = self.inner.repo_root.clone();
        tokio::task::spawn_blocking(move || {
            let lease = readers.lease()?;
            query::status(lease.connection()?, &repo_root)
        })
        .await?
    }
}
