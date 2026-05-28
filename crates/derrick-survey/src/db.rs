//! SQLite connection management for the survey index.
//!
//! Mirrors `derrick-substrate-native`'s single-writer + reader-pool pattern,
//! but opens the database in WAL journal mode: the survey index is read-heavy
//! with many concurrent worktree readers (D38) and an infrequent writer, so WAL
//! lets readers proceed without blocking the build (rust-architect sign-off).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OpenFlags};

use crate::SurveyError;

/// Highest schema version this binary understands.
pub(crate) const SCHEMA_VERSION: u32 = 1;

const MIGRATION_0001: &str = include_str!("../migrations/0001_initial.sql");

/// A pool of read-only connections handed out under lease.
pub(crate) struct ReaderPool {
    db_path: PathBuf,
    max_size: usize,
    connections: Mutex<Vec<Connection>>,
}

impl ReaderPool {
    pub(crate) fn new(db_path: PathBuf, size: usize) -> Result<Self, SurveyError> {
        let mut connections = Vec::with_capacity(size);
        for _ in 0..size {
            connections.push(open_reader_connection(&db_path)?);
        }
        Ok(Self {
            db_path,
            max_size: size,
            connections: Mutex::new(connections),
        })
    }

    pub(crate) fn lease(self: &Arc<Self>) -> Result<ReaderLease, SurveyError> {
        let connection = {
            let mut connections = self
                .connections
                .lock()
                .map_err(|_| SurveyError::Internal("reader pool mutex poisoned".to_owned()))?;
            connections.pop()
        };
        Ok(ReaderLease {
            connection: Some(match connection {
                Some(connection) => connection,
                None => open_reader_connection(&self.db_path)?,
            }),
            pool: Arc::clone(self),
        })
    }

    fn put(&self, connection: Connection) {
        if let Ok(mut connections) = self.connections.lock() {
            if connections.len() < self.max_size {
                connections.push(connection);
            }
        }
    }
}

/// A borrowed read-only connection, returned to the pool on drop.
pub(crate) struct ReaderLease {
    connection: Option<Connection>,
    pool: Arc<ReaderPool>,
}

impl ReaderLease {
    pub(crate) fn connection(&self) -> Result<&Connection, SurveyError> {
        self.connection
            .as_ref()
            .ok_or_else(|| SurveyError::Internal("reader lease missing connection".to_owned()))
    }
}

impl Drop for ReaderLease {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take() {
            self.pool.put(connection);
        }
    }
}

/// Open the single writable connection, run migrations, and leave it ready.
pub(crate) fn open_writer_connection(path: &Path) -> Result<Connection, SurveyError> {
    let mut connection = Connection::open(path)?;
    configure_common_pragmas(&connection)?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    migrate(&mut connection)?;
    Ok(connection)
}

fn open_reader_connection(path: &Path) -> Result<Connection, SurveyError> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    configure_common_pragmas(&connection)?;
    connection.pragma_update(None, "query_only", "ON")?;
    Ok(connection)
}

fn configure_common_pragmas(connection: &Connection) -> Result<(), SurveyError> {
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "busy_timeout", 5000)?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(())
}

fn migrate(connection: &mut Connection) -> Result<(), SurveyError> {
    let version: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(SurveyError::SchemaTooNew {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
    if version == 0 {
        connection.execute_batch(MIGRATION_0001)?;
    }
    Ok(())
}
