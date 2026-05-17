//! SQLite-backed `Substrate` implementation. See DESIGN.md §8.2.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use derrick_config::Site;
use derrick_substrate::{
    Batch, BatchName, Event, EventKind, ForemanMode, ForemanStatus, Hand, HandId, HandKind, Link,
    LinkKind, NewEvent, NewTicket, Substrate, SubstrateError, Ticket, TicketFilter, TicketId,
    TicketState,
};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Transaction};
use tokio::task;
use uuid::Uuid;

const SCHEMA_VERSION: u32 = 1;
const READER_POOL_SIZE: usize = 4;
const MIGRATION_0001: &str = include_str!("../migrations/0001_initial.sql");

/// Configuration for opening the native substrate.
#[derive(Clone, Debug)]
pub struct NativeConfig {
    /// Path to the SQLite file.
    pub db_path: PathBuf,
    /// Directory where per-run worktrees are reserved.
    pub worktree_root: PathBuf,
}

/// SQLite-backed substrate implementation.
///
/// Foreman mode is v1-derived from persisted pid only: no pid reports
/// `Stopped`, and any pid reports `Detached`. `Attached` has no native write
/// path until the T008 foreman loop extends the schema.
#[derive(Clone)]
pub struct NativeSubstrate {
    site: Site,
    db_path: PathBuf,
    worktree_root: PathBuf,
    writer: Arc<tokio::sync::Mutex<()>>,
    readers: Arc<ReaderPool>,
}

/// Persisted worktree bookkeeping row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeRecord {
    /// Run identifier that owns the worktree.
    pub run_id: String,
    /// Git branch planned for the worktree.
    pub branch: String,
    /// Absolute or configured path for the worktree.
    pub path: PathBuf,
    /// Reservation timestamp.
    pub created_at: DateTime<Utc>,
    /// Closure timestamp, when closed.
    pub closed_at: Option<DateTime<Utc>>,
}

impl NativeSubstrate {
    /// Open or create the substrate and run migrations to the current schema.
    pub async fn open(config: NativeConfig, site: Site) -> Result<Self, SubstrateError> {
        let db_path = config.db_path.clone();
        let site_for_open = site.clone();
        let readers = task::spawn_blocking(move || {
            let mut connection = open_writer_connection(&db_path)?;
            migrate(&mut connection)?;
            enforce_site_singleton(&connection, &site_for_open)?;
            ReaderPool::new(db_path, READER_POOL_SIZE)
        })
        .await
        .map_err(join_error)??;

        Ok(Self {
            site,
            db_path: config.db_path,
            worktree_root: absolute_path(config.worktree_root)?,
            writer: Arc::new(tokio::sync::Mutex::new(())),
            readers: Arc::new(readers),
        })
    }

    /// Close the substrate. Connections are closed when this value is dropped.
    pub async fn close(self) -> Result<(), SubstrateError> {
        Ok(())
    }

    /// Reserve a worktree slot and return the planned path.
    pub async fn reserve_worktree(
        &self,
        run_id: &str,
        branch: &str,
    ) -> Result<PathBuf, SubstrateError> {
        let run_id = run_id.to_owned();
        let branch = branch.to_owned();
        let path = self.worktree_root.join(&run_id);
        let path_for_db = path.clone();

        self.run_write(move |connection| {
            let live_branch: Option<String> = connection
                .query_row(
                    "SELECT run_id FROM worktrees WHERE branch = ?1 AND closed_at IS NULL",
                    params![branch],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sql_error)?;
            if let Some(existing) = live_branch {
                return Err(SubstrateError::Conflict {
                    message: format!("branch already has a live worktree: {existing}"),
                });
            }

            let now = now_text();
            connection
                .execute(
                    "INSERT INTO worktrees (run_id, branch, path, created_at, closed_at)
                     VALUES (?1, ?2, ?3, ?4, NULL)",
                    params![run_id, branch, path_for_db.to_string_lossy(), now],
                )
                .map_err(conflict_or_sql)?;
            insert_event(
                connection,
                EventKind::Note,
                None,
                &format!("WorktreeReserved {run_id}"),
            )?;
            Ok(path)
        })
        .await
    }

    /// Record that a reserved worktree was successfully created by the caller.
    pub async fn finalize_worktree(&self, run_id: &str) -> Result<(), SubstrateError> {
        let run_id = run_id.to_owned();
        self.run_write(move |connection| {
            ensure_worktree_exists(connection, &run_id)?;
            insert_event(
                connection,
                EventKind::Note,
                None,
                &format!("WorktreeFinalized {run_id}"),
            )?;
            Ok(())
        })
        .await
    }

    /// Delete a reservation after worktree creation fails.
    pub async fn rollback_worktree(&self, run_id: &str) -> Result<(), SubstrateError> {
        let run_id = run_id.to_owned();
        self.run_write(move |connection| {
            let changed = connection
                .execute("DELETE FROM worktrees WHERE run_id = ?1", params![run_id])
                .map_err(sql_error)?;
            if changed == 1 {
                insert_event(
                    connection,
                    EventKind::Note,
                    None,
                    &format!("WorktreeRolledBack {run_id}"),
                )?;
            }
            Ok(())
        })
        .await
    }

    /// Mark a worktree closed without deleting its directory.
    pub async fn close_worktree(&self, run_id: &str) -> Result<(), SubstrateError> {
        let run_id = run_id.to_owned();
        self.run_write(move |connection| {
            ensure_worktree_exists(connection, &run_id)?;
            let changed = connection
                .execute(
                    "UPDATE worktrees SET closed_at = ?1 WHERE run_id = ?2 AND closed_at IS NULL",
                    params![now_text(), run_id],
                )
                .map_err(sql_error)?;
            if changed == 1 {
                insert_event(
                    connection,
                    EventKind::Note,
                    None,
                    &format!("WorktreeClosed {run_id}"),
                )?;
            }
            Ok(())
        })
        .await
    }

    /// List tracked worktrees.
    pub async fn list_worktrees(
        &self,
        include_closed: bool,
    ) -> Result<Vec<WorktreeRecord>, SubstrateError> {
        self.run_read(move |connection| {
            let sql = if include_closed {
                "SELECT run_id, branch, path, created_at, closed_at FROM worktrees
                 ORDER BY created_at, run_id"
            } else {
                "SELECT run_id, branch, path, created_at, closed_at FROM worktrees
                 WHERE closed_at IS NULL ORDER BY created_at, run_id"
            };
            let mut statement = connection.prepare(sql).map_err(sql_error)?;
            let rows = statement
                .query_map([], worktree_from_row)
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?;
            Ok(rows)
        })
        .await
    }

    async fn run_read<F, R>(&self, operation: F) -> Result<R, SubstrateError>
    where
        F: FnOnce(&Connection) -> Result<R, SubstrateError> + Send + 'static,
        R: Send + 'static,
    {
        let readers = Arc::clone(&self.readers);
        task::spawn_blocking(move || {
            let lease = readers.lease()?;
            operation(lease.connection()?)
        })
        .await
        .map_err(join_error)?
    }

    async fn run_write<F, R>(&self, operation: F) -> Result<R, SubstrateError>
    where
        F: FnOnce(&mut Connection) -> Result<R, SubstrateError> + Send + 'static,
        R: Send + 'static,
    {
        let guard = Arc::clone(&self.writer).lock_owned().await;
        let db_path = self.db_path.clone();
        let result = task::spawn_blocking(move || {
            let _guard = guard;
            let mut connection = open_writer_connection(&db_path)?;
            operation(&mut connection)
        })
        .await
        .map_err(join_error)?;
        result
    }

    #[cfg(test)]
    async fn writer_foreign_keys_enabled_for_test(&self) -> Result<bool, SubstrateError> {
        self.run_write(|connection| {
            let value: i64 = connection
                .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
                .map_err(sql_error)?;
            Ok(value == 1)
        })
        .await
    }

    #[cfg(test)]
    async fn reader_foreign_keys_enabled_for_test(&self) -> Result<bool, SubstrateError> {
        self.run_read(|connection| {
            let value: i64 = connection
                .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
                .map_err(sql_error)?;
            Ok(value == 1)
        })
        .await
    }

    #[cfg(test)]
    async fn reader_insert_fails_for_test(&self) -> Result<(), SubstrateError> {
        self.run_read(|connection| {
            connection
                .execute(
                    "INSERT INTO batches (name, created_at, closed_at) VALUES ('bad', ?1, NULL)",
                    params![now_text()],
                )
                .map(|_| ())
                .map_err(sql_error)
        })
        .await
    }
}

#[async_trait::async_trait]
impl Substrate for NativeSubstrate {
    async fn site(&self) -> Result<Site, SubstrateError> {
        Ok(self.site.clone())
    }

    async fn create_ticket(&self, ticket: NewTicket) -> Result<Ticket, SubstrateError> {
        self.run_write(move |connection| {
            let transaction = connection.transaction().map_err(sql_error)?;
            if let Some(batch) = &ticket.batch {
                let closed_at: Option<Option<String>> = transaction
                    .query_row(
                        "SELECT closed_at FROM batches WHERE name = ?1",
                        params![batch.as_str()],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(sql_error)?;
                match closed_at {
                    Some(Some(_)) => {
                        return Err(SubstrateError::Conflict {
                            message: format!("batch is closed: {}", batch.as_str()),
                        });
                    }
                    Some(None) => {}
                    None => {
                        return Err(SubstrateError::NotFound {
                            kind: "batch",
                            id: batch.to_string(),
                        });
                    }
                }
            }

            let now = now_text();
            transaction
                .execute(
                    "INSERT INTO tickets
                     (id, batch, ordinal, title, body, state, owner, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, 'ready', NULL, ?6, ?6)",
                    params![
                        ticket.id.as_str(),
                        ticket.batch.as_ref().map(BatchName::as_str),
                        ticket.ordinal.map(i64::from),
                        ticket.title,
                        ticket.body,
                        now
                    ],
                )
                .map_err(conflict_or_sql)?;
            for label in unique_labels(ticket.labels) {
                transaction
                    .execute(
                        "INSERT OR IGNORE INTO ticket_labels (ticket_id, label) VALUES (?1, ?2)",
                        params![ticket.id.as_str(), label],
                    )
                    .map_err(sql_error)?;
            }
            insert_event(&transaction, EventKind::TicketCreated, Some(&ticket.id), "")?;
            let persisted = select_ticket(&transaction, &ticket.id)?;
            transaction.commit().map_err(sql_error)?;
            Ok(persisted)
        })
        .await
    }

    async fn get_ticket(&self, id: &TicketId) -> Result<Option<Ticket>, SubstrateError> {
        let id = id.clone();
        self.run_read(move |connection| select_optional_ticket(connection, &id))
            .await
    }

    async fn list_tickets(&self, filter: TicketFilter) -> Result<Vec<Ticket>, SubstrateError> {
        self.run_read(move |connection| {
            let ids = select_ticket_ids(connection, &filter)?;
            ids.into_iter()
                .map(|id| select_ticket(connection, &id))
                .collect()
        })
        .await
    }

    async fn set_ticket_state(
        &self,
        id: &TicketId,
        state: TicketState,
        reason: Option<String>,
    ) -> Result<Ticket, SubstrateError> {
        let id = id.clone();
        self.run_write(move |connection| {
            let transaction = connection.transaction().map_err(sql_error)?;
            let batch: Option<String> = transaction
                .query_row(
                    "SELECT batch FROM tickets WHERE id = ?1",
                    params![id.as_str()],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sql_error)?
                .ok_or_else(|| SubstrateError::NotFound {
                    kind: "ticket",
                    id: id.to_string(),
                })?;
            transaction
                .execute(
                    "UPDATE tickets SET state = ?1, updated_at = ?2 WHERE id = ?3",
                    params![state.to_string(), now_text(), id.as_str()],
                )
                .map_err(sql_error)?;
            insert_event(
                &transaction,
                EventKind::TicketStateChanged,
                Some(&id),
                reason.as_deref().unwrap_or_default(),
            )?;
            if state.is_terminal() {
                if let Some(batch_name) = batch {
                    maybe_auto_close_batch(&transaction, &batch_name)?;
                }
            }
            let ticket = select_ticket(&transaction, &id)?;
            transaction.commit().map_err(sql_error)?;
            Ok(ticket)
        })
        .await
    }

    async fn assign_ticket(
        &self,
        id: &TicketId,
        owner: Option<HandId>,
    ) -> Result<Ticket, SubstrateError> {
        let id = id.clone();
        self.run_write(move |connection| {
            let transaction = connection.transaction().map_err(sql_error)?;
            let changed = transaction
                .execute(
                    "UPDATE tickets SET owner = ?1, updated_at = ?2 WHERE id = ?3",
                    params![owner.as_ref().map(HandId::as_str), now_text(), id.as_str()],
                )
                .map_err(conflict_or_sql)?;
            if changed == 0 {
                return Err(SubstrateError::NotFound {
                    kind: "ticket",
                    id: id.to_string(),
                });
            }
            insert_event(&transaction, EventKind::TicketAssigned, Some(&id), "")?;
            let ticket = select_ticket(&transaction, &id)?;
            transaction.commit().map_err(sql_error)?;
            Ok(ticket)
        })
        .await
    }

    async fn add_label(&self, id: &TicketId, label: &str) -> Result<(), SubstrateError> {
        let id = id.clone();
        let label = label.to_owned();
        self.run_write(move |connection| {
            ensure_ticket_exists(connection, &id)?;
            connection
                .execute(
                    "INSERT OR IGNORE INTO ticket_labels (ticket_id, label) VALUES (?1, ?2)",
                    params![id.as_str(), label],
                )
                .map_err(sql_error)?;
            insert_event(
                connection,
                EventKind::Note,
                Some(&id),
                &format!("LabelAdded {label}"),
            )?;
            Ok(())
        })
        .await
    }

    async fn remove_label(&self, id: &TicketId, label: &str) -> Result<(), SubstrateError> {
        let id = id.clone();
        let label = label.to_owned();
        self.run_write(move |connection| {
            ensure_ticket_exists(connection, &id)?;
            connection
                .execute(
                    "DELETE FROM ticket_labels WHERE ticket_id = ?1 AND label = ?2",
                    params![id.as_str(), label],
                )
                .map_err(sql_error)?;
            insert_event(
                connection,
                EventKind::Note,
                Some(&id),
                &format!("LabelRemoved {label}"),
            )?;
            Ok(())
        })
        .await
    }

    async fn link(
        &self,
        from: &TicketId,
        to: &TicketId,
        kind: LinkKind,
    ) -> Result<(), SubstrateError> {
        let from = from.clone();
        let to = to.clone();
        self.run_write(move |connection| {
            connection
                .execute(
                    "INSERT OR IGNORE INTO links (from_ticket, to_ticket, kind) VALUES (?1, ?2, ?3)",
                    params![from.as_str(), to.as_str(), kind.to_string()],
                )
                .map_err(conflict_or_sql)?;
            insert_event(
                connection,
                EventKind::Note,
                Some(&from),
                &format!("LinkCreated {} {}", kind, to.as_str()),
            )?;
            Ok(())
        })
        .await
    }

    async fn unlink(
        &self,
        from: &TicketId,
        to: &TicketId,
        kind: LinkKind,
    ) -> Result<(), SubstrateError> {
        let from = from.clone();
        let to = to.clone();
        self.run_write(move |connection| {
            connection
                .execute(
                    "DELETE FROM links WHERE from_ticket = ?1 AND to_ticket = ?2 AND kind = ?3",
                    params![from.as_str(), to.as_str(), kind.to_string()],
                )
                .map_err(sql_error)?;
            insert_event(
                connection,
                EventKind::Note,
                Some(&from),
                &format!("LinkRemoved {} {}", kind, to.as_str()),
            )?;
            Ok(())
        })
        .await
    }

    async fn outgoing_links(&self, id: &TicketId) -> Result<Vec<Link>, SubstrateError> {
        let id = id.clone();
        self.run_read(move |connection| {
            select_links(
                connection,
                "SELECT from_ticket, to_ticket, kind FROM links WHERE from_ticket = ?1",
                &id,
            )
        })
        .await
    }

    async fn incoming_links(&self, id: &TicketId) -> Result<Vec<Link>, SubstrateError> {
        let id = id.clone();
        self.run_read(move |connection| {
            select_links(
                connection,
                "SELECT from_ticket, to_ticket, kind FROM links WHERE to_ticket = ?1",
                &id,
            )
        })
        .await
    }

    async fn create_batch(&self, name: BatchName) -> Result<Batch, SubstrateError> {
        self.run_write(move |connection| {
            let now = now_text();
            connection
                .execute(
                    "INSERT INTO batches (name, created_at, closed_at) VALUES (?1, ?2, NULL)",
                    params![name.as_str(), now],
                )
                .map_err(conflict_or_sql)?;
            insert_event(connection, EventKind::BatchCreated, None, name.as_str())?;
            select_batch(connection, &name)
        })
        .await
    }

    async fn get_batch(&self, name: &BatchName) -> Result<Option<Batch>, SubstrateError> {
        let name = name.clone();
        self.run_read(move |connection| select_optional_batch(connection, &name))
            .await
    }

    async fn list_batches(&self, include_closed: bool) -> Result<Vec<Batch>, SubstrateError> {
        self.run_read(move |connection| {
            let sql = if include_closed {
                "SELECT name, created_at, closed_at FROM batches ORDER BY created_at, name"
            } else {
                "SELECT name, created_at, closed_at FROM batches WHERE closed_at IS NULL ORDER BY created_at, name"
            };
            let mut statement = connection.prepare(sql).map_err(sql_error)?;
            let batches = statement
                .query_map([], batch_from_row)
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?;
            Ok(batches)
        })
        .await
    }

    async fn close_batch(&self, name: &BatchName) -> Result<Batch, SubstrateError> {
        let name = name.clone();
        self.run_write(move |connection| {
            let transaction = connection.transaction().map_err(sql_error)?;
            let before = select_optional_batch(&transaction, &name)?.ok_or_else(|| {
                SubstrateError::NotFound {
                    kind: "batch",
                    id: name.to_string(),
                }
            })?;
            if before.closed_at.is_some() {
                return Ok(before);
            }

            let open_ids = open_ticket_ids_in_batch(&transaction, name.as_str())?;
            transaction
                .execute(
                    "UPDATE batches SET closed_at = ?1 WHERE name = ?2 AND closed_at IS NULL",
                    params![now_text(), name.as_str()],
                )
                .map_err(sql_error)?;
            insert_event(
                &transaction,
                EventKind::BatchClosed,
                None,
                &serde_json::to_string(&open_ids).map_err(json_error)?,
            )?;
            let batch = select_batch(&transaction, &name)?;
            transaction.commit().map_err(sql_error)?;
            Ok(batch)
        })
        .await
    }

    async fn tickets_in_batch(&self, name: &BatchName) -> Result<Vec<Ticket>, SubstrateError> {
        let name = name.clone();
        self.run_read(move |connection| {
            let mut statement = connection
                .prepare(
                    "SELECT id FROM tickets WHERE batch = ?1
                     ORDER BY ordinal IS NULL, ordinal, created_at",
                )
                .map_err(sql_error)?;
            let ids = statement
                .query_map(params![name.as_str()], |row| row.get::<_, String>(0))
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?;
            ids.into_iter()
                .map(|id| {
                    TicketId::new(id).and_then(|ticket_id| select_ticket(connection, &ticket_id))
                })
                .collect()
        })
        .await
    }

    async fn register_hand(&self, hand: Hand) -> Result<(), SubstrateError> {
        self.run_write(move |connection| {
            connection
                .execute(
                    "INSERT INTO hands (id, kind, last_seen) VALUES (?1, ?2, ?3)
                     ON CONFLICT(id) DO UPDATE SET kind = excluded.kind, last_seen = excluded.last_seen",
                    params![
                        hand.id.as_str(),
                        hand.kind.to_string(),
                        hand.last_seen.map(format_time)
                    ],
                )
                .map_err(sql_error)?;
            insert_event(connection, EventKind::Note, None, &format!("HandRegistered {}", hand.id))?;
            Ok(())
        })
        .await
    }

    async fn list_hands(&self) -> Result<Vec<Hand>, SubstrateError> {
        self.run_read(move |connection| {
            let mut statement = connection
                .prepare("SELECT id, kind, last_seen FROM hands ORDER BY id")
                .map_err(sql_error)?;
            let hands = statement
                .query_map([], hand_from_row)
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?;
            Ok(hands)
        })
        .await
    }

    async fn heartbeat(&self, id: &HandId) -> Result<(), SubstrateError> {
        let id = id.clone();
        self.run_write(move |connection| {
            let changed = connection
                .execute(
                    "UPDATE hands SET last_seen = ?1 WHERE id = ?2",
                    params![now_text(), id.as_str()],
                )
                .map_err(sql_error)?;
            if changed == 0 {
                return Err(SubstrateError::NotFound {
                    kind: "hand",
                    id: id.to_string(),
                });
            }
            insert_event(
                connection,
                EventKind::Note,
                None,
                &format!("HandHeartbeat {id}"),
            )?;
            Ok(())
        })
        .await
    }

    async fn record_event(&self, event: NewEvent) -> Result<Event, SubstrateError> {
        self.run_write(move |connection| {
            let id = insert_event(
                connection,
                event.kind,
                event.ticket.as_ref(),
                event.body.as_str(),
            )?;
            select_event(connection, id)
        })
        .await
    }

    async fn tail_events(
        &self,
        since: Option<DateTime<Utc>>,
        limit: usize,
    ) -> Result<Vec<Event>, SubstrateError> {
        self.run_read(move |connection| {
            let capped_limit = i64::try_from(limit).map_err(|error| SubstrateError::Invalid {
                field: "limit".to_owned(),
                message: error.to_string(),
            })?;
            let since_text = since.map(format_time);
            let mut statement = connection
                .prepare(
                    "SELECT id, at, kind, ticket, body FROM events
                     WHERE (?1 IS NULL OR at > ?1)
                     ORDER BY at DESC, id DESC
                     LIMIT ?2",
                )
                .map_err(sql_error)?;
            let events = statement
                .query_map(params![since_text, capped_limit], event_from_row)
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?;
            Ok(events)
        })
        .await
    }

    async fn foreman_status(&self) -> Result<ForemanStatus, SubstrateError> {
        self.run_read(move |connection| {
            let row: Option<(Option<i64>, Option<String>)> = connection
                .query_row("SELECT pid, started_at FROM foreman LIMIT 1", [], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
                .optional()
                .map_err(sql_error)?;
            let Some((pid, started_at)) = row else {
                return Ok(stopped_foreman());
            };
            let Some(pid) = pid else {
                return Ok(stopped_foreman());
            };
            let pid = u32::try_from(pid).map_err(|error| SubstrateError::Invalid {
                field: "foreman.pid".to_owned(),
                message: error.to_string(),
            })?;
            Ok(ForemanStatus {
                pid: Some(pid),
                started_at: parse_optional_time(started_at)?,
                mode: ForemanMode::Detached,
            })
        })
        .await
    }

    async fn record_foreman_start(&self, pid: u32) -> Result<(), SubstrateError> {
        let site_name = self.site.name().to_owned();
        self.run_write(move |connection| {
            connection
                .execute(
                    "INSERT INTO foreman (site, pid, started_at) VALUES (?1, ?2, ?3)
                     ON CONFLICT(site) DO UPDATE SET pid = excluded.pid, started_at = excluded.started_at",
                    params![site_name, i64::from(pid), now_text()],
                )
                .map_err(sql_error)?;
            insert_event(connection, EventKind::ForemanStarted, None, &pid.to_string())?;
            Ok(())
        })
        .await
    }

    async fn record_foreman_stop(&self) -> Result<(), SubstrateError> {
        let site_name = self.site.name().to_owned();
        self.run_write(move |connection| {
            connection
                .execute(
                    "INSERT INTO foreman (site, pid, started_at) VALUES (?1, NULL, NULL)
                     ON CONFLICT(site) DO UPDATE SET pid = NULL, started_at = NULL",
                    params![site_name],
                )
                .map_err(sql_error)?;
            insert_event(connection, EventKind::ForemanStopped, None, "")?;
            Ok(())
        })
        .await
    }
}

struct ReaderPool {
    db_path: PathBuf,
    max_size: usize,
    connections: Mutex<Vec<Connection>>,
}

impl ReaderPool {
    fn new(db_path: PathBuf, size: usize) -> Result<Self, SubstrateError> {
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

    fn lease(self: &Arc<Self>) -> Result<ReaderLease, SubstrateError> {
        let connection = {
            let mut connections = self.connections.lock().map_err(mutex_error)?;
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

struct ReaderLease {
    connection: Option<Connection>,
    pool: Arc<ReaderPool>,
}

impl ReaderLease {
    fn connection(&self) -> Result<&Connection, SubstrateError> {
        self.connection.as_ref().ok_or_else(|| {
            SubstrateError::Backend(Box::new(std::io::Error::other(
                "reader lease missing connection",
            )))
        })
    }
}

impl Drop for ReaderLease {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take() {
            self.pool.put(connection);
        }
    }
}

fn open_writer_connection(path: &Path) -> Result<Connection, SubstrateError> {
    let connection = Connection::open(path).map_err(sql_error)?;
    configure_common_pragmas(&connection)?;
    Ok(connection)
}

fn open_reader_connection(path: &Path) -> Result<Connection, SubstrateError> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(sql_error)?;
    configure_common_pragmas(&connection)?;
    connection
        .pragma_update(None, "query_only", "ON")
        .map_err(sql_error)?;
    Ok(connection)
}

fn configure_common_pragmas(connection: &Connection) -> Result<(), SubstrateError> {
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .map_err(sql_error)?;
    connection
        .pragma_update(None, "busy_timeout", 5000)
        .map_err(sql_error)?;
    connection
        .pragma_update(None, "synchronous", "NORMAL")
        .map_err(sql_error)?;
    Ok(())
}

fn migrate(connection: &mut Connection) -> Result<(), SubstrateError> {
    let version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(sql_error)?;
    if version > SCHEMA_VERSION {
        return Err(SubstrateError::Invalid {
            field: "schema_version".to_owned(),
            message: format!(
                "DB schema version {version} is newer than supported version {SCHEMA_VERSION}"
            ),
        });
    }
    if version == 0 {
        connection
            .execute_batch(MIGRATION_0001)
            .map_err(sql_error)?;
    }
    Ok(())
}

fn enforce_site_singleton(connection: &Connection, site: &Site) -> Result<(), SubstrateError> {
    let count: i64 = connection
        .query_row("SELECT COUNT(*) FROM site", [], |row| row.get(0))
        .map_err(sql_error)?;
    match count {
        0 => {
            connection
                .execute(
                    "INSERT INTO site (name, prefix, created_at) VALUES (?1, ?2, ?3)",
                    params![site.name(), site.prefix(), now_text()],
                )
                .map_err(sql_error)?;
            Ok(())
        }
        1 => {
            let (name, prefix): (String, String) = connection
                .query_row("SELECT name, prefix FROM site LIMIT 1", [], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
                .map_err(sql_error)?;
            if name == site.name() && prefix == site.prefix() {
                Ok(())
            } else {
                Err(SubstrateError::Invalid {
                    field: "site".to_owned(),
                    message: format!(
                        "DB site {name}/{prefix} does not match config site {}/{}; refusing to open",
                        site.name(),
                        site.prefix()
                    ),
                })
            }
        }
        _ => Err(SubstrateError::Invalid {
            field: "site".to_owned(),
            message: "DB has multiple site rows; corrupted state, refusing to open".to_owned(),
        }),
    }
}

fn maybe_auto_close_batch(
    transaction: &Transaction<'_>,
    batch_name: &str,
) -> Result<(), SubstrateError> {
    let open_count: i64 = transaction
        .query_row(
            "SELECT COUNT(*) FROM tickets
             WHERE batch = ?1 AND state NOT IN ('done', 'rejected')",
            params![batch_name],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    if open_count == 0 {
        let changed = transaction
            .execute(
                "UPDATE batches SET closed_at = ?1 WHERE name = ?2 AND closed_at IS NULL",
                params![now_text(), batch_name],
            )
            .map_err(sql_error)?;
        if changed == 1 {
            insert_event(transaction, EventKind::BatchClosed, None, "")?;
        }
    }
    Ok(())
}

fn select_ticket_ids(
    connection: &Connection,
    filter: &TicketFilter,
) -> Result<Vec<TicketId>, SubstrateError> {
    let limit = filter.limit.map_or(i64::MAX, |limit| {
        i64::try_from(limit.get()).map_or(i64::MAX, |limit| limit)
    });
    let mut statement = connection
        .prepare(
            "SELECT DISTINCT t.id
             FROM tickets t
             LEFT JOIN ticket_labels l ON l.ticket_id = t.id
             WHERE (?1 IS NULL OR t.state = ?1)
               AND (?2 IS NULL OR t.batch = ?2)
               AND (?3 IS NULL OR t.owner = ?3)
               AND (?4 IS NULL OR l.label = ?4)
             ORDER BY t.created_at, t.id
             LIMIT ?5",
        )
        .map_err(sql_error)?;
    let rows = statement
        .query_map(
            params![
                filter.state.map(|state| state.to_string()),
                filter.batch.as_ref().map(BatchName::as_str),
                filter.owner.as_ref().map(HandId::as_str),
                filter.label.as_deref(),
                limit,
            ],
            |row| row.get::<_, String>(0),
        )
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)?;
    rows.into_iter().map(TicketId::new).collect()
}

fn select_optional_ticket(
    connection: &Connection,
    id: &TicketId,
) -> Result<Option<Ticket>, SubstrateError> {
    let row = connection
        .query_row(
            "SELECT id, batch, ordinal, title, body, state, owner, created_at, updated_at
             FROM tickets WHERE id = ?1",
            params![id.as_str()],
            ticket_base_from_row,
        )
        .optional()
        .map_err(sql_error)?;
    row.map(|mut ticket| {
        ticket.labels = select_labels(connection, &ticket.id)?;
        Ok(ticket)
    })
    .transpose()
}

fn select_ticket(connection: &Connection, id: &TicketId) -> Result<Ticket, SubstrateError> {
    select_optional_ticket(connection, id)?.ok_or_else(|| SubstrateError::NotFound {
        kind: "ticket",
        id: id.to_string(),
    })
}

fn ensure_ticket_exists(connection: &Connection, id: &TicketId) -> Result<(), SubstrateError> {
    let exists: Option<i64> = connection
        .query_row(
            "SELECT 1 FROM tickets WHERE id = ?1",
            params![id.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    if exists.is_some() {
        Ok(())
    } else {
        Err(SubstrateError::NotFound {
            kind: "ticket",
            id: id.to_string(),
        })
    }
}

fn select_labels(connection: &Connection, id: &TicketId) -> Result<Vec<String>, SubstrateError> {
    let mut statement = connection
        .prepare("SELECT label FROM ticket_labels WHERE ticket_id = ?1 ORDER BY label")
        .map_err(sql_error)?;
    let labels = statement
        .query_map(params![id.as_str()], |row| row.get(0))
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)?;
    Ok(labels)
}

fn select_links(
    connection: &Connection,
    sql: &str,
    id: &TicketId,
) -> Result<Vec<Link>, SubstrateError> {
    let mut statement = connection.prepare(sql).map_err(sql_error)?;
    let links = statement
        .query_map(params![id.as_str()], link_from_row)
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)?;
    Ok(links)
}

fn select_optional_batch(
    connection: &Connection,
    name: &BatchName,
) -> Result<Option<Batch>, SubstrateError> {
    connection
        .query_row(
            "SELECT name, created_at, closed_at FROM batches WHERE name = ?1",
            params![name.as_str()],
            batch_from_row,
        )
        .optional()
        .map_err(sql_error)
}

fn select_batch(connection: &Connection, name: &BatchName) -> Result<Batch, SubstrateError> {
    select_optional_batch(connection, name)?.ok_or_else(|| SubstrateError::NotFound {
        kind: "batch",
        id: name.to_string(),
    })
}

fn open_ticket_ids_in_batch(
    connection: &Connection,
    batch_name: &str,
) -> Result<Vec<String>, SubstrateError> {
    let mut statement = connection
        .prepare(
            "SELECT id FROM tickets
             WHERE batch = ?1 AND state NOT IN ('done', 'rejected')
             ORDER BY id",
        )
        .map_err(sql_error)?;
    let ids = statement
        .query_map(params![batch_name], |row| row.get(0))
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)?;
    Ok(ids)
}

fn insert_event(
    connection: &Connection,
    kind: EventKind,
    ticket: Option<&TicketId>,
    body: &str,
) -> Result<Uuid, SubstrateError> {
    let id = Uuid::new_v4();
    connection
        .execute(
            "INSERT INTO events (id, at, kind, ticket, body) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                id.as_bytes().as_slice(),
                now_text(),
                kind.to_string(),
                ticket.map(TicketId::as_str),
                body
            ],
        )
        .map_err(sql_error)?;
    Ok(id)
}

fn select_event(connection: &Connection, id: Uuid) -> Result<Event, SubstrateError> {
    connection
        .query_row(
            "SELECT id, at, kind, ticket, body FROM events WHERE id = ?1",
            params![id.as_bytes().as_slice()],
            event_from_row,
        )
        .map_err(sql_error)
}

fn ensure_worktree_exists(connection: &Connection, run_id: &str) -> Result<(), SubstrateError> {
    let exists: Option<i64> = connection
        .query_row(
            "SELECT 1 FROM worktrees WHERE run_id = ?1",
            params![run_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    if exists.is_some() {
        Ok(())
    } else {
        Err(SubstrateError::NotFound {
            kind: "worktree",
            id: run_id.to_owned(),
        })
    }
}

fn ticket_base_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Ticket> {
    let id: String = row.get(0)?;
    let batch: Option<String> = row.get(1)?;
    let ordinal: Option<i64> = row.get(2)?;
    let state: String = row.get(5)?;
    let owner: Option<String> = row.get(6)?;
    let created_at: String = row.get(7)?;
    let updated_at: String = row.get(8)?;

    Ok(Ticket {
        id: parse_in_row(id, TicketId::new)?,
        batch: parse_optional_in_row(batch, BatchName::new)?,
        ordinal: ordinal.and_then(|value| u32::try_from(value).ok()),
        title: row.get(3)?,
        body: row.get(4)?,
        state: parse_in_row(state, |value| TicketState::from_str(&value))?,
        labels: Vec::new(),
        owner: parse_optional_in_row(owner, HandId::new)?,
        created_at: parse_time_in_row(created_at)?,
        updated_at: parse_time_in_row(updated_at)?,
    })
}

fn batch_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Batch> {
    let name: String = row.get(0)?;
    let created_at: String = row.get(1)?;
    let closed_at: Option<String> = row.get(2)?;
    Ok(Batch {
        name: parse_in_row(name, BatchName::new)?,
        created_at: parse_time_in_row(created_at)?,
        closed_at: parse_optional_time_in_row(closed_at)?,
    })
}

fn link_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Link> {
    let from: String = row.get(0)?;
    let to: String = row.get(1)?;
    let kind: String = row.get(2)?;
    Ok(Link {
        from: parse_in_row(from, TicketId::new)?,
        to: parse_in_row(to, TicketId::new)?,
        kind: parse_in_row(kind, |value| LinkKind::from_str(&value))?,
    })
}

fn hand_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Hand> {
    let id: String = row.get(0)?;
    let kind: String = row.get(1)?;
    let last_seen: Option<String> = row.get(2)?;
    Ok(Hand {
        id: parse_in_row(id, HandId::new)?,
        kind: parse_in_row(kind, |value| HandKind::from_str(&value))?,
        last_seen: parse_optional_time_in_row(last_seen)?,
    })
}

fn event_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Event> {
    let id_bytes: Vec<u8> = row.get(0)?;
    let at: String = row.get(1)?;
    let kind: String = row.get(2)?;
    let ticket: Option<String> = row.get(3)?;
    let id = Uuid::from_slice(&id_bytes).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Blob, Box::new(error))
    })?;
    Ok(Event {
        id,
        at: parse_time_in_row(at)?,
        kind: parse_in_row(kind, |value| EventKind::from_str(&value))?,
        ticket: parse_optional_in_row(ticket, TicketId::new)?,
        body: row.get(4)?,
    })
}

fn worktree_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorktreeRecord> {
    let created_at: String = row.get(3)?;
    let closed_at: Option<String> = row.get(4)?;
    Ok(WorktreeRecord {
        run_id: row.get(0)?,
        branch: row.get(1)?,
        path: PathBuf::from(row.get::<_, String>(2)?),
        created_at: parse_time_in_row(created_at)?,
        closed_at: parse_optional_time_in_row(closed_at)?,
    })
}

fn parse_in_row<T, E, F>(value: String, parser: F) -> rusqlite::Result<T>
where
    E: std::error::Error + Send + Sync + 'static,
    F: FnOnce(String) -> Result<T, E>,
{
    parser(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })
}

fn parse_optional_in_row<T, E, F>(value: Option<String>, parser: F) -> rusqlite::Result<Option<T>>
where
    E: std::error::Error + Send + Sync + 'static,
    F: FnOnce(String) -> Result<T, E>,
{
    value.map(|value| parse_in_row(value, parser)).transpose()
}

fn parse_time_in_row(value: String) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&value)
        .map(|time| time.with_timezone(&Utc))
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
}

fn parse_optional_time_in_row(value: Option<String>) -> rusqlite::Result<Option<DateTime<Utc>>> {
    value.map(parse_time_in_row).transpose()
}

fn parse_optional_time(value: Option<String>) -> Result<Option<DateTime<Utc>>, SubstrateError> {
    value
        .map(|value| {
            DateTime::parse_from_rfc3339(&value)
                .map(|time| time.with_timezone(&Utc))
                .map_err(|error| SubstrateError::Invalid {
                    field: "timestamp".to_owned(),
                    message: error.to_string(),
                })
        })
        .transpose()
}

fn unique_labels(labels: Vec<String>) -> Vec<String> {
    let mut labels = labels;
    labels.sort();
    labels.dedup();
    labels
}

fn stopped_foreman() -> ForemanStatus {
    ForemanStatus {
        pid: None,
        started_at: None,
        mode: ForemanMode::Stopped,
    }
}

fn absolute_path(path: PathBuf) -> Result<PathBuf, SubstrateError> {
    if path.is_absolute() {
        Ok(path)
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|error| SubstrateError::Backend(Box::new(error)))
    }
}

fn now_text() -> String {
    format_time(Utc::now())
}

fn format_time(time: DateTime<Utc>) -> String {
    time.to_rfc3339()
}

fn sql_error(error: rusqlite::Error) -> SubstrateError {
    SubstrateError::Backend(Box::new(error))
}

fn conflict_or_sql(error: rusqlite::Error) -> SubstrateError {
    match error {
        rusqlite::Error::SqliteFailure(ref sqlite_error, _)
            if sqlite_error.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY
                || sqlite_error.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
                || sqlite_error.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY =>
        {
            SubstrateError::Conflict {
                message: error.to_string(),
            }
        }
        _ => sql_error(error),
    }
}

fn json_error(error: serde_json::Error) -> SubstrateError {
    SubstrateError::Backend(Box::new(error))
}

fn join_error(error: task::JoinError) -> SubstrateError {
    SubstrateError::Backend(Box::new(error))
}

fn mutex_error<T>(error: std::sync::PoisonError<T>) -> SubstrateError {
    SubstrateError::Backend(Box::new(std::io::Error::other(error.to_string())))
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use derrick_config::Config;
    use derrick_substrate::{HandKind, Substrate};
    use tempfile::TempDir;

    use super::*;

    fn site() -> Site {
        Config::defaults().site().clone()
    }

    fn native_config(tempdir: &TempDir) -> NativeConfig {
        NativeConfig {
            db_path: tempdir.path().join("derrick.db"),
            worktree_root: tempdir.path().join("worktrees"),
        }
    }

    async fn open(tempdir: &TempDir) -> Result<NativeSubstrate, SubstrateError> {
        NativeSubstrate::open(native_config(tempdir), site()).await
    }

    fn ticket_id(value: &str) -> Result<TicketId, SubstrateError> {
        TicketId::new(value)
    }

    fn batch_name(value: &str) -> Result<BatchName, SubstrateError> {
        BatchName::new(value)
    }

    fn hand_id(value: &str) -> Result<HandId, SubstrateError> {
        HandId::new(value)
    }

    fn new_ticket(value: &str) -> Result<NewTicket, SubstrateError> {
        NewTicket::new(ticket_id(value)?, None, None, "title", "body", Vec::new())
    }

    fn new_batched_ticket(
        value: &str,
        batch: &str,
        ordinal: Option<u32>,
    ) -> Result<NewTicket, SubstrateError> {
        NewTicket::new(
            ticket_id(value)?,
            Some(batch_name(batch)?),
            ordinal,
            "title",
            "body",
            Vec::new(),
        )
    }

    #[tokio::test]
    async fn site_initialises_from_config() -> Result<(), SubstrateError> {
        let tempdir = tempfile::tempdir().map_err(io_error)?;
        let substrate = open(&tempdir).await?;

        assert_eq!(substrate.site().await?, site());
        Ok(())
    }

    #[tokio::test]
    async fn create_ticket_persists_and_missing_returns_none() -> Result<(), SubstrateError> {
        let tempdir = tempfile::tempdir().map_err(io_error)?;
        let substrate = open(&tempdir).await?;
        let ticket = substrate.create_ticket(new_ticket("drk-1")?).await?;

        assert_eq!(ticket.id, ticket_id("drk-1")?);
        assert!(substrate.get_ticket(&ticket_id("drk-1")?).await?.is_some());
        assert!(substrate.get_ticket(&ticket_id("drk-2")?).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn create_ticket_into_closed_batch_returns_conflict() -> Result<(), SubstrateError> {
        let tempdir = tempfile::tempdir().map_err(io_error)?;
        let substrate = open(&tempdir).await?;
        substrate.create_batch(batch_name("batch-1")?).await?;
        substrate.close_batch(&batch_name("batch-1")?).await?;

        let result = substrate
            .create_ticket(new_batched_ticket("drk-1", "batch-1", Some(1))?)
            .await;

        assert!(matches!(result, Err(SubstrateError::Conflict { .. })));
        Ok(())
    }

    #[tokio::test]
    async fn create_ticket_duplicate_id_returns_conflict() -> Result<(), SubstrateError> {
        let tempdir = tempfile::tempdir().map_err(io_error)?;
        let substrate = open(&tempdir).await?;
        substrate.create_ticket(new_ticket("drk-1")?).await?;

        let result = substrate.create_ticket(new_ticket("drk-1")?).await;

        assert!(matches!(result, Err(SubstrateError::Conflict { .. })));
        Ok(())
    }

    #[tokio::test]
    async fn list_tickets_respects_filters_and_limit() -> Result<(), SubstrateError> {
        let tempdir = tempfile::tempdir().map_err(io_error)?;
        let substrate = open(&tempdir).await?;
        substrate.create_batch(batch_name("batch-1")?).await?;
        substrate
            .register_hand(Hand {
                id: hand_id("copilot-1")?,
                kind: HandKind::Copilot,
                last_seen: None,
            })
            .await?;
        substrate
            .create_ticket(NewTicket::new(
                ticket_id("drk-1")?,
                Some(batch_name("batch-1")?),
                Some(1),
                "a",
                "",
                vec!["ui".to_owned()],
            )?)
            .await?;
        substrate.create_ticket(new_ticket("drk-2")?).await?;
        substrate
            .set_ticket_state(&ticket_id("drk-1")?, TicketState::Blocked, None)
            .await?;
        substrate
            .assign_ticket(&ticket_id("drk-1")?, Some(hand_id("copilot-1")?))
            .await?;

        let filter = TicketFilter {
            state: Some(TicketState::Blocked),
            batch: Some(batch_name("batch-1")?),
            owner: Some(hand_id("copilot-1")?),
            label: Some("ui".to_owned()),
            limit: NonZeroUsize::new(1),
        };
        let tickets = substrate.list_tickets(filter).await?;
        let all = substrate
            .list_tickets(TicketFilter {
                limit: None,
                ..TicketFilter::default()
            })
            .await?;

        assert_eq!(tickets.len(), 1);
        assert_eq!(tickets[0].id, ticket_id("drk-1")?);
        assert_eq!(all.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn set_ticket_state_writes_event_and_autocloses_last_ticket() -> Result<(), SubstrateError>
    {
        let tempdir = tempfile::tempdir().map_err(io_error)?;
        let substrate = open(&tempdir).await?;
        substrate.create_batch(batch_name("batch-1")?).await?;
        substrate
            .create_ticket(new_batched_ticket("drk-1", "batch-1", Some(1))?)
            .await?;

        substrate
            .set_ticket_state(
                &ticket_id("drk-1")?,
                TicketState::Done,
                Some("done".to_owned()),
            )
            .await?;
        let batch = substrate.get_batch(&batch_name("batch-1")?).await?;
        let events = substrate.tail_events(None, 100).await?;

        assert!(batch.and_then(|batch| batch.closed_at).is_some());
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == EventKind::BatchClosed)
                .count(),
            1
        );
        assert!(events
            .iter()
            .any(|event| event.kind == EventKind::TicketStateChanged));
        Ok(())
    }

    #[tokio::test]
    async fn set_ticket_state_does_not_autoclose_if_others_open() -> Result<(), SubstrateError> {
        let tempdir = tempfile::tempdir().map_err(io_error)?;
        let substrate = open(&tempdir).await?;
        substrate.create_batch(batch_name("batch-1")?).await?;
        substrate
            .create_ticket(new_batched_ticket("drk-1", "batch-1", Some(1))?)
            .await?;
        substrate
            .create_ticket(new_batched_ticket("drk-2", "batch-1", Some(2))?)
            .await?;

        substrate
            .set_ticket_state(&ticket_id("drk-1")?, TicketState::Done, None)
            .await?;
        let batch = substrate.get_batch(&batch_name("batch-1")?).await?;

        assert!(batch.and_then(|batch| batch.closed_at).is_none());
        Ok(())
    }

    #[tokio::test]
    async fn assign_ticket_and_labels_round_trip() -> Result<(), SubstrateError> {
        let tempdir = tempfile::tempdir().map_err(io_error)?;
        let substrate = open(&tempdir).await?;
        substrate
            .register_hand(Hand {
                id: hand_id("human-1")?,
                kind: HandKind::Human,
                last_seen: None,
            })
            .await?;
        substrate.create_ticket(new_ticket("drk-1")?).await?;
        substrate
            .assign_ticket(&ticket_id("drk-1")?, Some(hand_id("human-1")?))
            .await?;
        substrate.add_label(&ticket_id("drk-1")?, "alpha").await?;
        substrate.add_label(&ticket_id("drk-1")?, "alpha").await?;
        substrate.add_label(&ticket_id("drk-1")?, "beta").await?;
        substrate.remove_label(&ticket_id("drk-1")?, "beta").await?;

        let ticket = substrate
            .get_ticket(&ticket_id("drk-1")?)
            .await?
            .ok_or_else(|| SubstrateError::NotFound {
                kind: "ticket",
                id: "drk-1".to_owned(),
            })?;

        assert_eq!(ticket.owner, Some(hand_id("human-1")?));
        assert_eq!(ticket.labels, vec!["alpha".to_owned()]);
        Ok(())
    }

    #[tokio::test]
    async fn link_and_unlink_round_trip() -> Result<(), SubstrateError> {
        let tempdir = tempfile::tempdir().map_err(io_error)?;
        let substrate = open(&tempdir).await?;
        substrate.create_ticket(new_ticket("drk-1")?).await?;
        substrate.create_ticket(new_ticket("drk-2")?).await?;

        substrate
            .link(&ticket_id("drk-1")?, &ticket_id("drk-2")?, LinkKind::Blocks)
            .await?;
        assert_eq!(
            substrate.outgoing_links(&ticket_id("drk-1")?).await?.len(),
            1
        );
        assert_eq!(
            substrate.incoming_links(&ticket_id("drk-2")?).await?.len(),
            1
        );
        substrate
            .unlink(&ticket_id("drk-1")?, &ticket_id("drk-2")?, LinkKind::Blocks)
            .await?;
        assert!(substrate
            .outgoing_links(&ticket_id("drk-1")?)
            .await?
            .is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn batch_lifecycle_and_ordering() -> Result<(), SubstrateError> {
        let tempdir = tempfile::tempdir().map_err(io_error)?;
        let substrate = open(&tempdir).await?;
        substrate.create_batch(batch_name("batch-1")?).await?;
        assert!(substrate
            .get_batch(&batch_name("batch-1")?)
            .await?
            .is_some());
        substrate
            .create_ticket(new_batched_ticket("drk-2", "batch-1", None)?)
            .await?;
        substrate
            .create_ticket(new_batched_ticket("drk-1", "batch-1", Some(1))?)
            .await?;
        let tickets = substrate.tickets_in_batch(&batch_name("batch-1")?).await?;

        assert_eq!(tickets[0].id, ticket_id("drk-1")?);
        assert_eq!(tickets[1].id, ticket_id("drk-2")?);
        assert_eq!(substrate.list_batches(false).await?.len(), 1);
        let first_close = substrate.close_batch(&batch_name("batch-1")?).await?;
        let second_close = substrate.close_batch(&batch_name("batch-1")?).await?;
        assert_eq!(first_close.closed_at, second_close.closed_at);
        Ok(())
    }

    #[tokio::test]
    async fn force_close_batch_lists_open_ticket_ids_in_event_body() -> Result<(), SubstrateError> {
        let tempdir = tempfile::tempdir().map_err(io_error)?;
        let substrate = open(&tempdir).await?;
        substrate.create_batch(batch_name("batch-1")?).await?;
        substrate
            .create_ticket(new_batched_ticket("drk-1", "batch-1", Some(1))?)
            .await?;

        substrate.close_batch(&batch_name("batch-1")?).await?;
        let events = substrate.tail_events(None, 20).await?;

        assert!(events
            .iter()
            .any(|event| event.kind == EventKind::BatchClosed && event.body.contains("drk-1")));
        Ok(())
    }

    #[tokio::test]
    async fn hands_and_heartbeat_round_trip() -> Result<(), SubstrateError> {
        let tempdir = tempfile::tempdir().map_err(io_error)?;
        let substrate = open(&tempdir).await?;
        substrate
            .register_hand(Hand {
                id: hand_id("claude-1")?,
                kind: HandKind::Claude,
                last_seen: None,
            })
            .await?;
        substrate.heartbeat(&hand_id("claude-1")?).await?;

        let hands = substrate.list_hands().await?;

        assert_eq!(hands.len(), 1);
        assert!(hands[0].last_seen.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn record_event_and_tail_events_respect_since_limit_order() -> Result<(), SubstrateError>
    {
        let tempdir = tempfile::tempdir().map_err(io_error)?;
        let substrate = open(&tempdir).await?;
        let first = substrate
            .record_event(NewEvent {
                kind: EventKind::Note,
                ticket: None,
                body: "first".to_owned(),
            })
            .await?;
        let second = substrate
            .record_event(NewEvent {
                kind: EventKind::Note,
                ticket: None,
                body: "second".to_owned(),
            })
            .await?;
        let events = substrate.tail_events(Some(first.at), 1).await?;

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, second.id);
        Ok(())
    }

    #[tokio::test]
    async fn foreman_status_reports_stopped_and_detached() -> Result<(), SubstrateError> {
        let tempdir = tempfile::tempdir().map_err(io_error)?;
        let substrate = open(&tempdir).await?;

        assert_eq!(substrate.foreman_status().await?.mode, ForemanMode::Stopped);
        substrate.record_foreman_start(123).await?;
        let running = substrate.foreman_status().await?;
        assert_eq!(running.pid, Some(123));
        assert_eq!(running.mode, ForemanMode::Detached);
        substrate.record_foreman_stop().await?;
        assert_eq!(substrate.foreman_status().await?.mode, ForemanMode::Stopped);
        Ok(())
    }

    #[tokio::test]
    async fn worktree_lifecycle_round_trip() -> Result<(), SubstrateError> {
        let tempdir = tempfile::tempdir().map_err(io_error)?;
        let substrate = open(&tempdir).await?;
        let path = substrate
            .reserve_worktree("run-1", "derrick/feature-run-1")
            .await?;
        assert_eq!(path, tempdir.path().join("worktrees").join("run-1"));
        assert!(matches!(
            substrate
                .reserve_worktree("run-1", "derrick/other-run-1")
                .await,
            Err(SubstrateError::Conflict { .. })
        ));
        assert!(matches!(
            substrate
                .reserve_worktree("run-2", "derrick/feature-run-1")
                .await,
            Err(SubstrateError::Conflict { .. })
        ));
        substrate.finalize_worktree("run-1").await?;
        assert_eq!(substrate.list_worktrees(false).await?.len(), 1);
        substrate.close_worktree("run-1").await?;
        assert!(substrate.list_worktrees(false).await?.is_empty());
        assert_eq!(substrate.list_worktrees(true).await?.len(), 1);
        substrate
            .reserve_worktree("run-2", "derrick/feature-run-2")
            .await?;
        substrate.rollback_worktree("run-2").await?;
        substrate.rollback_worktree("run-2").await?;
        assert_eq!(substrate.list_worktrees(true).await?.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn open_rejects_mismatched_site_and_multiple_site_rows() -> Result<(), SubstrateError> {
        let tempdir = tempfile::tempdir().map_err(io_error)?;
        let config = native_config(&tempdir);
        let substrate = NativeSubstrate::open(config.clone(), site()).await?;
        substrate.close().await?;

        let mismatched = site();
        write_site_row_for_test(&config.db_path, "other", "oth", true)?;
        let result = NativeSubstrate::open(config.clone(), mismatched.clone()).await;
        assert!(matches!(result, Err(SubstrateError::Invalid { .. })));

        let connection = open_writer_connection(&config.db_path)?;
        connection
            .execute(
                "INSERT INTO site (name, prefix, created_at) VALUES ('second', 'sec', ?1)",
                params![now_text()],
            )
            .map_err(sql_error)?;
        let result = NativeSubstrate::open(config, mismatched).await;
        assert!(matches!(result, Err(SubstrateError::Invalid { .. })));
        Ok(())
    }

    #[tokio::test]
    async fn pragmas_set_on_every_connection_and_reader_is_query_only() -> Result<(), SubstrateError>
    {
        let tempdir = tempfile::tempdir().map_err(io_error)?;
        let substrate = open(&tempdir).await?;

        assert!(substrate.writer_foreign_keys_enabled_for_test().await?);
        assert!(substrate.reader_foreign_keys_enabled_for_test().await?);
        assert!(matches!(
            substrate.reader_insert_fails_for_test().await,
            Err(SubstrateError::Backend(_))
        ));
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_terminal_writes_emit_one_batch_closed_event() -> Result<(), SubstrateError>
    {
        let tempdir = tempfile::tempdir().map_err(io_error)?;
        let substrate = Arc::new(open(&tempdir).await?);
        substrate.create_batch(batch_name("batch-1")?).await?;
        substrate
            .create_ticket(new_batched_ticket("drk-1", "batch-1", Some(1))?)
            .await?;
        substrate
            .create_ticket(new_batched_ticket("drk-2", "batch-1", Some(2))?)
            .await?;

        let first = Arc::clone(&substrate);
        let second = Arc::clone(&substrate);
        let first_task = tokio::spawn(async move {
            first
                .set_ticket_state(&ticket_id("drk-1")?, TicketState::Done, None)
                .await
        });
        let second_task = tokio::spawn(async move {
            second
                .set_ticket_state(&ticket_id("drk-2")?, TicketState::Done, None)
                .await
        });
        join_ticket(first_task).await?;
        join_ticket(second_task).await?;
        let events = substrate.tail_events(None, 100).await?;

        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == EventKind::BatchClosed)
                .count(),
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_writes_serialise() -> Result<(), SubstrateError> {
        let tempdir = tempfile::tempdir().map_err(io_error)?;
        let substrate = Arc::new(open(&tempdir).await?);
        let mut tasks = Vec::new();
        for index in 0..10 {
            let substrate = Arc::clone(&substrate);
            tasks.push(tokio::spawn(async move {
                substrate
                    .create_ticket(new_ticket(&format!("drk-{index}"))?)
                    .await
            }));
        }
        for task in tasks {
            join_ticket(task).await?;
        }

        let tickets = substrate
            .list_tickets(TicketFilter {
                limit: None,
                ..TicketFilter::default()
            })
            .await?;
        assert_eq!(tickets.len(), 10);
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_reads_dont_block_on_write_mutex() -> Result<(), SubstrateError> {
        let tempdir = tempfile::tempdir().map_err(io_error)?;
        let substrate = open(&tempdir).await?;
        substrate.create_ticket(new_ticket("drk-1")?).await?;
        let guard = Arc::clone(&substrate.writer).lock_owned().await;
        let read = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            substrate.get_ticket(&ticket_id("drk-1")?),
        )
        .await
        .map_err(|error| SubstrateError::Backend(Box::new(error)))?;
        drop(guard);

        assert!(read?.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn migration_runs_skips_and_refuses_newer_schema() -> Result<(), SubstrateError> {
        let tempdir = tempfile::tempdir().map_err(io_error)?;
        let config = native_config(&tempdir);
        NativeSubstrate::open(config.clone(), site()).await?;
        NativeSubstrate::open(config.clone(), site()).await?;
        let connection = open_writer_connection(&config.db_path)?;
        let version: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(sql_error)?;
        assert_eq!(version, SCHEMA_VERSION);
        connection
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .map_err(sql_error)?;
        let result = NativeSubstrate::open(config, site()).await;
        assert!(matches!(result, Err(SubstrateError::Invalid { .. })));
        Ok(())
    }

    fn write_site_row_for_test(
        db_path: &Path,
        name: &str,
        prefix: &str,
        clear_existing: bool,
    ) -> Result<(), SubstrateError> {
        let connection = open_writer_connection(db_path)?;
        if clear_existing {
            connection
                .execute("DELETE FROM site", [])
                .map_err(sql_error)?;
        }
        connection
            .execute(
                "INSERT INTO site (name, prefix, created_at) VALUES (?1, ?2, ?3)",
                params![name, prefix, now_text()],
            )
            .map_err(sql_error)?;
        Ok(())
    }

    async fn join_ticket(
        task: tokio::task::JoinHandle<Result<Ticket, SubstrateError>>,
    ) -> Result<Ticket, SubstrateError> {
        task.await.map_err(join_error)?
    }

    fn io_error(error: std::io::Error) -> SubstrateError {
        SubstrateError::Backend(Box::new(error))
    }
}
