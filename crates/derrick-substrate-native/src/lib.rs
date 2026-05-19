//! SQLite-backed `Substrate` implementation. See DESIGN.md §8.2.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use derrick_config::Site;
use derrick_substrate::{
    Batch, BatchName, BlockReason, Event, EventId, EventKind, EventScope, ForemanMode,
    ForemanStatus, Hand, HandId, HandKind, InReviewMetadata, Link, LinkKind, ManualDoneAttestation,
    NewEvent, NewTicket, Substrate, SubstrateError, Ticket, TicketFilter, TicketId, TicketState,
    TypedEvent,
};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Transaction};
use tokio::task;
use uuid::Uuid;

const SCHEMA_VERSION: u32 = 2;
const READER_POOL_SIZE: usize = 4;
const MIGRATION_0001: &str = include_str!("../migrations/0001_initial.sql");
const MIGRATION_0002: &str = include_str!("../migrations/0002_state_machine_integrity.sql");

/// Configuration for opening the native substrate.
#[derive(Clone, Debug)]
pub struct NativeConfig {
    /// Path to the SQLite file.
    pub db_path: PathBuf,
    /// Directory where per-run worktrees are reserved.
    pub worktree_root: PathBuf,
}

/// SQLite-backed substrate implementation.
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
    ///
    /// If a row already exists for `run_id` (e.g. from a previous run that was
    /// closed or interrupted), the row is re-opened rather than failing — this
    /// allows `resume` to re-reserve the same worktree.
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
            // Try to re-open an existing row for this run_id first.
            let updated = connection
                .execute(
                    "UPDATE worktrees SET branch = ?1, path = ?2, created_at = ?3, closed_at = NULL
                     WHERE run_id = ?4",
                    params![branch, path_for_db.to_string_lossy(), now_text(), run_id],
                )
                .map_err(sql_error)?;

            if updated == 1 {
                insert_typed_event_raw(
                    connection,
                    &EventScope::Worktree {
                        run_id: run_id.clone(),
                    },
                    &EventKind::WorktreeReserved {
                        run_id: run_id.clone(),
                        branch: branch.clone(),
                    },
                )?;
                return Ok(path);
            }

            // No existing row — fresh reservation with duplicate-branch check.
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
            insert_typed_event_raw(
                connection,
                &EventScope::Worktree {
                    run_id: run_id.clone(),
                },
                &EventKind::WorktreeReserved {
                    run_id: run_id.clone(),
                    branch: branch.clone(),
                },
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
            insert_typed_event_raw(
                connection,
                &EventScope::Worktree {
                    run_id: run_id.clone(),
                },
                &EventKind::WorktreeFinalized {
                    run_id: run_id.clone(),
                },
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
                insert_typed_event_raw(
                    connection,
                    &EventScope::Worktree {
                        run_id: run_id.clone(),
                    },
                    &EventKind::WorktreeAbandoned {
                        run_id: run_id.clone(),
                        reason: "rollback".to_owned(),
                    },
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
                insert_typed_event_raw(
                    connection,
                    &EventScope::Worktree {
                        run_id: run_id.clone(),
                    },
                    &EventKind::WorktreeFinalized {
                        run_id: run_id.clone(),
                    },
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

    /// Delete a worktree row outright. Used by the foreman cleanup pass after
    /// it has pruned the on-disk directory.
    pub(crate) async fn delete_worktree_row(&self, run_id: &str) -> Result<(), SubstrateError> {
        let run_id = run_id.to_owned();
        self.run_write(move |connection| {
            connection
                .execute("DELETE FROM worktrees WHERE run_id = ?1", params![run_id])
                .map_err(sql_error)?;
            Ok(())
        })
        .await
    }

    /// Open worktree rows whose `created_at` is older than `threshold`.
    pub(crate) async fn list_stale_open_worktrees(
        &self,
        threshold: DateTime<Utc>,
    ) -> Result<Vec<WorktreeRecord>, SubstrateError> {
        let threshold_text = format_time(threshold);
        self.run_read(move |connection| {
            let mut statement = connection
                .prepare(
                    "SELECT run_id, branch, path, created_at, closed_at FROM worktrees
                     WHERE closed_at IS NULL AND created_at < ?1
                     ORDER BY created_at, run_id",
                )
                .map_err(sql_error)?;
            let rows = statement
                .query_map(params![threshold_text], worktree_from_row)
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?;
            Ok(rows)
        })
        .await
    }

    /// Hand ids whose `last_seen` predates `threshold` (or who have never
    /// reported a heartbeat).
    pub(crate) async fn list_stale_hands(
        &self,
        threshold: DateTime<Utc>,
    ) -> Result<Vec<HandId>, SubstrateError> {
        let threshold_text = format_time(threshold);
        self.run_read(move |connection| {
            let mut statement = connection
                .prepare(
                    "SELECT id FROM hands
                     WHERE last_seen IS NULL OR last_seen < ?1
                     ORDER BY id",
                )
                .map_err(sql_error)?;
            let rows = statement
                .query_map(params![threshold_text], |row| row.get::<_, String>(0))
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?;
            rows.into_iter().map(HandId::new).collect()
        })
        .await
    }

    /// Tickets currently `InFlight` owned by the given hand.
    pub(crate) async fn list_inflight_tickets_owned_by(
        &self,
        hand: &HandId,
    ) -> Result<Vec<TicketId>, SubstrateError> {
        let hand = hand.clone();
        self.run_read(move |connection| {
            let mut statement = connection
                .prepare(
                    "SELECT id FROM tickets WHERE state = 'in_flight' AND owner = ?1
                     ORDER BY created_at, id",
                )
                .map_err(sql_error)?;
            let rows = statement
                .query_map(params![hand.as_str()], |row| row.get::<_, String>(0))
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?;
            rows.into_iter().map(TicketId::new).collect()
        })
        .await
    }

    /// `InReview` tickets whose `updated_at` predates `threshold`.
    pub(crate) async fn list_stale_inreview_tickets(
        &self,
        threshold: DateTime<Utc>,
    ) -> Result<Vec<TicketId>, SubstrateError> {
        let threshold_text = format_time(threshold);
        self.run_read(move |connection| {
            let mut statement = connection
                .prepare(
                    "SELECT id FROM tickets
                     WHERE state = 'in_review' AND updated_at < ?1
                     ORDER BY updated_at, id",
                )
                .map_err(sql_error)?;
            let rows = statement
                .query_map(params![threshold_text], |row| row.get::<_, String>(0))
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?;
            rows.into_iter().map(TicketId::new).collect()
        })
        .await
    }

    /// All ticket ids currently in `InReview`.
    pub(crate) async fn list_inreview_ticket_ids(&self) -> Result<Vec<TicketId>, SubstrateError> {
        self.run_read(|connection| {
            let mut statement = connection
                .prepare(
                    "SELECT id FROM tickets WHERE state = 'in_review'
                     ORDER BY updated_at, id",
                )
                .map_err(sql_error)?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?;
            rows.into_iter().map(TicketId::new).collect()
        })
        .await
    }

    /// Count tickets currently `InFlight`.
    pub(crate) async fn count_inflight_tickets(&self) -> Result<u64, SubstrateError> {
        self.run_read(|connection| {
            let count: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM tickets WHERE state = 'in_flight'",
                    [],
                    |row| row.get(0),
                )
                .map_err(sql_error)?;
            Ok(u64::try_from(count).unwrap_or(0))
        })
        .await
    }

    /// Ready tickets ordered by ordinal (NULL last), then created_at, then id.
    pub(crate) async fn list_ready_tickets_ordered(&self) -> Result<Vec<Ticket>, SubstrateError> {
        self.run_read(|connection| {
            let mut statement = connection
                .prepare(
                    "SELECT id FROM tickets WHERE state = 'ready'
                     ORDER BY ordinal IS NULL, ordinal, created_at, id",
                )
                .map_err(sql_error)?;
            let ids = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?;
            ids.into_iter()
                .map(|id| TicketId::new(id).and_then(|id| select_ticket(connection, &id)))
                .collect()
        })
        .await
    }

    /// Blocked tickets whose `block_reason` discriminator is `dependency`.
    pub(crate) async fn list_dependency_blocked_ticket_ids(
        &self,
    ) -> Result<Vec<TicketId>, SubstrateError> {
        self.run_read(|connection| {
            let mut statement = connection
                .prepare(
                    "SELECT id FROM tickets
                     WHERE state = 'blocked' AND block_reason = 'dependency'
                     ORDER BY id",
                )
                .map_err(sql_error)?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?;
            rows.into_iter().map(TicketId::new).collect()
        })
        .await
    }

    /// Ready tickets that have at least one prior
    /// `TicketTransitionedToInReview` event in their history.
    pub(crate) async fn list_ready_tickets_with_inreview_history(
        &self,
    ) -> Result<Vec<Ticket>, SubstrateError> {
        self.run_read(|connection| {
            let mut statement = connection
                .prepare(
                    "SELECT DISTINCT t.id
                     FROM tickets t
                     INNER JOIN events e ON e.ticket = t.id
                     WHERE t.state = 'ready'
                       AND e.kind = 'ticket_transitioned_to_in_review'
                     ORDER BY t.id",
                )
                .map_err(sql_error)?;
            let ids = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?;
            ids.into_iter()
                .map(|id| TicketId::new(id).and_then(|id| select_ticket(connection, &id)))
                .collect()
        })
        .await
    }

    /// Most-recent `TicketTransitionedToInReview` metadata for `id`, if any.
    pub async fn most_recent_in_review_metadata(
        &self,
        id: &TicketId,
    ) -> Result<Option<InReviewMetadata>, SubstrateError> {
        let id = id.clone();
        self.run_read(move |connection| {
            let row: Option<String> = connection
                .query_row(
                    "SELECT body FROM events
                     WHERE ticket = ?1
                       AND kind = 'ticket_transitioned_to_in_review'
                     ORDER BY rowid DESC LIMIT 1",
                    params![id.as_str()],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sql_error)?;
            let Some(body) = row else { return Ok(None) };
            let kind: EventKind = serde_json::from_str(&body).map_err(json_error)?;
            match kind {
                EventKind::TicketTransitionedToInReview {
                    branch,
                    pr_url,
                    pr_number,
                    head_sha,
                } => Ok(Some(InReviewMetadata {
                    branch,
                    pr_url,
                    pr_number,
                    head_sha,
                })),
                _ => Ok(None),
            }
        })
        .await
    }

    /// `blocks`-link predecessors for `id` (tickets `id` is blocked by).
    pub async fn blocks_predecessors(
        &self,
        id: &TicketId,
    ) -> Result<Vec<TicketId>, SubstrateError> {
        let id = id.clone();
        self.run_read(move |connection| select_outgoing_blocks_predecessors(connection, &id))
            .await
    }

    /// Returns ticket IDs that depend on `id` (reverse of
    /// [`Self::blocks_predecessors`]). Used by the foreman's restack pass
    /// to find dependents of a freshly-merged ticket.
    pub(crate) async fn blocks_dependents(
        &self,
        id: &TicketId,
    ) -> Result<Vec<TicketId>, SubstrateError> {
        let id = id.clone();
        self.run_read(move |connection| select_incoming_blocks_dependents(connection, &id))
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
                     (id, batch, ordinal, title, body, state, owner, merge_sha,
                      block_reason, block_reason_detail, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, 'ready', NULL, NULL, NULL, NULL, ?6, ?6)",
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
            insert_typed_event_raw(
                &transaction,
                &EventScope::Ticket(ticket.id.clone()),
                &EventKind::TicketCreated {
                    initial_state: TicketState::Ready,
                },
            )?;
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
        _reason: Option<String>,
    ) -> Result<Ticket, SubstrateError> {
        let id = id.clone();
        self.run_read(move |connection| {
            let current = select_ticket(connection, &id)?;
            if current.state == state {
                return Ok(current);
            }
            Err(set_ticket_state_redirect(state))
        })
        .await
    }

    async fn assign_to_hand(&self, id: &TicketId, hand: &HandId) -> Result<Ticket, SubstrateError> {
        let id = id.clone();
        let hand = hand.clone();
        self.run_write(move |connection| {
            let transaction = connection.transaction().map_err(sql_error)?;
            let current = select_ticket(&transaction, &id)?;
            if current.state != TicketState::Ready {
                return Err(SubstrateError::Invalid {
                    field: "state".to_owned(),
                    message: format!("assign_to_hand requires Ready; ticket is {}", current.state),
                });
            }
            ensure_hand_exists(&transaction, &hand)?;
            transaction
                .execute(
                    "UPDATE tickets SET state = 'in_flight', owner = ?1, updated_at = ?2
                     WHERE id = ?3",
                    params![hand.as_str(), now_text(), id.as_str()],
                )
                .map_err(conflict_or_sql)?;
            insert_typed_event_raw(
                &transaction,
                &EventScope::Ticket(id.clone()),
                &EventKind::TicketStateChanged {
                    from: TicketState::Ready,
                    to: TicketState::InFlight,
                    reason: None,
                },
            )?;
            insert_typed_event_raw(
                &transaction,
                &EventScope::Ticket(id.clone()),
                &EventKind::TicketAssigned { hand: hand.clone() },
            )?;
            let ticket = select_ticket(&transaction, &id)?;
            transaction.commit().map_err(sql_error)?;
            Ok(ticket)
        })
        .await
    }

    async fn release_from_hand(
        &self,
        id: &TicketId,
        reason: String,
    ) -> Result<Ticket, SubstrateError> {
        let id = id.clone();
        self.run_write(move |connection| {
            let transaction = connection.transaction().map_err(sql_error)?;
            let current = select_ticket(&transaction, &id)?;
            if current.state.is_terminal() {
                return Err(SubstrateError::Invalid {
                    field: "state".to_owned(),
                    message: format!(
                        "release_from_hand refused: ticket is terminal ({})",
                        current.state
                    ),
                });
            }
            transaction
                .execute(
                    "UPDATE tickets SET state = 'ready', owner = NULL, updated_at = ?1
                     WHERE id = ?2",
                    params![now_text(), id.as_str()],
                )
                .map_err(sql_error)?;
            insert_typed_event_raw(
                &transaction,
                &EventScope::Ticket(id.clone()),
                &EventKind::TicketStateChanged {
                    from: current.state,
                    to: TicketState::Ready,
                    reason: Some(reason.clone()),
                },
            )?;
            insert_typed_event_raw(
                &transaction,
                &EventScope::Ticket(id.clone()),
                &EventKind::TicketUnassigned {
                    reason: reason.clone(),
                },
            )?;
            let ticket = select_ticket(&transaction, &id)?;
            transaction.commit().map_err(sql_error)?;
            Ok(ticket)
        })
        .await
    }

    async fn transition_to_in_review(
        &self,
        id: &TicketId,
        review: InReviewMetadata,
    ) -> Result<Ticket, SubstrateError> {
        let id = id.clone();
        self.run_write(move |connection| {
            let transaction = connection.transaction().map_err(sql_error)?;
            let current = select_ticket(&transaction, &id)?;
            if current.state != TicketState::InFlight {
                return Err(SubstrateError::Invalid {
                    field: "state".to_owned(),
                    message: format!(
                        "transition_to_in_review requires InFlight; ticket is {}",
                        current.state
                    ),
                });
            }
            transaction
                .execute(
                    "UPDATE tickets SET state = 'in_review', updated_at = ?1 WHERE id = ?2",
                    params![now_text(), id.as_str()],
                )
                .map_err(sql_error)?;
            insert_typed_event_raw(
                &transaction,
                &EventScope::Ticket(id.clone()),
                &EventKind::TicketTransitionedToInReview {
                    branch: review.branch.clone(),
                    pr_url: review.pr_url.clone(),
                    pr_number: review.pr_number,
                    head_sha: review.head_sha.clone(),
                },
            )?;
            insert_typed_event_raw(
                &transaction,
                &EventScope::Ticket(id.clone()),
                &EventKind::TicketStateChanged {
                    from: TicketState::InFlight,
                    to: TicketState::InReview,
                    reason: None,
                },
            )?;
            let ticket = select_ticket(&transaction, &id)?;
            transaction.commit().map_err(sql_error)?;
            Ok(ticket)
        })
        .await
    }

    async fn verify_ticket_merged(
        &self,
        id: &TicketId,
        head_sha: String,
        merge_sha: String,
    ) -> Result<Ticket, SubstrateError> {
        let id = id.clone();
        self.run_write(move |connection| {
            let transaction = connection.transaction().map_err(sql_error)?;
            let current = select_ticket(&transaction, &id)?;
            if current.state != TicketState::InReview {
                return Err(SubstrateError::Invalid {
                    field: "state".to_owned(),
                    message: format!(
                        "verify_ticket_merged requires InReview; ticket is {}",
                        current.state
                    ),
                });
            }
            transaction
                .execute(
                    "UPDATE tickets SET state = 'done', merge_sha = ?1, updated_at = ?2
                     WHERE id = ?3",
                    params![merge_sha, now_text(), id.as_str()],
                )
                .map_err(sql_error)?;
            insert_typed_event_raw(
                &transaction,
                &EventScope::Ticket(id.clone()),
                &EventKind::TicketVerifiedMerged {
                    head_sha: head_sha.clone(),
                    merge_sha: merge_sha.clone(),
                },
            )?;
            insert_typed_event_raw(
                &transaction,
                &EventScope::Ticket(id.clone()),
                &EventKind::TicketStateChanged {
                    from: TicketState::InReview,
                    to: TicketState::Done,
                    reason: None,
                },
            )?;
            if let Some(batch) = current.batch.as_ref() {
                maybe_auto_close_batch(&transaction, batch.as_str())?;
            }
            let ticket = select_ticket(&transaction, &id)?;
            transaction.commit().map_err(sql_error)?;
            Ok(ticket)
        })
        .await
    }

    async fn verify_ticket_unmerged(
        &self,
        id: &TicketId,
        branch: String,
        pr_url: Option<String>,
    ) -> Result<Ticket, SubstrateError> {
        let id = id.clone();
        self.run_write(move |connection| {
            let transaction = connection.transaction().map_err(sql_error)?;
            let current = select_ticket(&transaction, &id)?;
            if current.state != TicketState::InReview {
                return Err(SubstrateError::Invalid {
                    field: "state".to_owned(),
                    message: format!(
                        "verify_ticket_unmerged requires InReview; ticket is {}",
                        current.state
                    ),
                });
            }
            let reason = BlockReason::PrClosedUnmerged {
                branch: branch.clone(),
                pr_url: pr_url.clone(),
            };
            let detail = serde_json::to_string(&reason).map_err(json_error)?;
            transaction
                .execute(
                    "UPDATE tickets SET state = 'blocked', block_reason = 'pr_closed_unmerged',
                     block_reason_detail = ?1, updated_at = ?2 WHERE id = ?3",
                    params![detail, now_text(), id.as_str()],
                )
                .map_err(sql_error)?;
            insert_typed_event_raw(
                &transaction,
                &EventScope::Ticket(id.clone()),
                &EventKind::TicketVerifiedUnmerged {
                    reason: format!("pr closed unmerged: {branch}"),
                },
            )?;
            insert_typed_event_raw(
                &transaction,
                &EventScope::Ticket(id.clone()),
                &EventKind::TicketStateChanged {
                    from: TicketState::InReview,
                    to: TicketState::Blocked,
                    reason: Some("pr_closed_unmerged".to_owned()),
                },
            )?;
            let ticket = select_ticket(&transaction, &id)?;
            transaction.commit().map_err(sql_error)?;
            Ok(ticket)
        })
        .await
    }

    async fn block_ticket(
        &self,
        id: &TicketId,
        reason: BlockReason,
    ) -> Result<Ticket, SubstrateError> {
        let id = id.clone();
        self.run_write(move |connection| {
            let transaction = connection.transaction().map_err(sql_error)?;
            let current = select_ticket(&transaction, &id)?;
            if current.state.is_terminal() {
                return Err(SubstrateError::Invalid {
                    field: "state".to_owned(),
                    message: format!(
                        "block_ticket refused: ticket is terminal ({})",
                        current.state
                    ),
                });
            }
            let discriminator = block_reason_discriminator(&reason);
            let detail = serde_json::to_string(&reason).map_err(json_error)?;
            transaction
                .execute(
                    "UPDATE tickets SET state = 'blocked', block_reason = ?1,
                     block_reason_detail = ?2, updated_at = ?3 WHERE id = ?4",
                    params![discriminator, detail, now_text(), id.as_str()],
                )
                .map_err(sql_error)?;
            insert_typed_event_raw(
                &transaction,
                &EventScope::Ticket(id.clone()),
                &EventKind::TicketStateChanged {
                    from: current.state,
                    to: TicketState::Blocked,
                    reason: Some(discriminator.to_owned()),
                },
            )?;
            let ticket = select_ticket(&transaction, &id)?;
            transaction.commit().map_err(sql_error)?;
            Ok(ticket)
        })
        .await
    }

    async fn unblock_ticket(&self, id: &TicketId) -> Result<Ticket, SubstrateError> {
        let id = id.clone();
        self.run_write(move |connection| {
            let transaction = connection.transaction().map_err(sql_error)?;
            let current = select_ticket(&transaction, &id)?;
            if current.state != TicketState::Blocked {
                return Err(SubstrateError::Invalid {
                    field: "state".to_owned(),
                    message: format!(
                        "unblock_ticket requires Blocked; ticket is {}",
                        current.state
                    ),
                });
            }
            let reason = current
                .block_reason
                .as_ref()
                .ok_or_else(|| SubstrateError::Invalid {
                    field: "block_reason".to_owned(),
                    message: "Blocked ticket missing block_reason".to_owned(),
                })?;
            if !matches!(reason, BlockReason::Dependency { .. }) {
                return Err(SubstrateError::Invalid {
                    field: "block_reason".to_owned(),
                    message: "unblock_ticket only valid for Dependency; use human_reopen_blocked"
                        .to_owned(),
                });
            }
            // Re-verify predecessors inside the same transaction.
            let predecessors = select_outgoing_blocks_predecessors(&transaction, &id)?;
            for predecessor in &predecessors {
                let pred = select_ticket(&transaction, predecessor)?;
                if !pred.state.is_terminal() {
                    return Err(SubstrateError::Invalid {
                        field: "predecessor".to_owned(),
                        message: format!(
                            "predecessor {} is not terminal (state {})",
                            predecessor, pred.state
                        ),
                    });
                }
            }
            transaction
                .execute(
                    "UPDATE tickets SET state = 'ready', block_reason = NULL,
                     block_reason_detail = NULL, updated_at = ?1 WHERE id = ?2",
                    params![now_text(), id.as_str()],
                )
                .map_err(sql_error)?;
            insert_typed_event_raw(
                &transaction,
                &EventScope::Ticket(id.clone()),
                &EventKind::TicketStateChanged {
                    from: TicketState::Blocked,
                    to: TicketState::Ready,
                    reason: Some("dependency cleared".to_owned()),
                },
            )?;
            let ticket = select_ticket(&transaction, &id)?;
            transaction.commit().map_err(sql_error)?;
            Ok(ticket)
        })
        .await
    }

    async fn human_reopen_blocked(
        &self,
        id: &TicketId,
        note: String,
    ) -> Result<Ticket, SubstrateError> {
        let id = id.clone();
        self.run_write(move |connection| {
            let transaction = connection.transaction().map_err(sql_error)?;
            let current = select_ticket(&transaction, &id)?;
            if current.state != TicketState::Blocked {
                return Err(SubstrateError::Invalid {
                    field: "state".to_owned(),
                    message: format!(
                        "human_reopen_blocked requires Blocked; ticket is {}",
                        current.state
                    ),
                });
            }
            transaction
                .execute(
                    "UPDATE tickets SET state = 'ready', block_reason = NULL,
                     block_reason_detail = NULL, updated_at = ?1 WHERE id = ?2",
                    params![now_text(), id.as_str()],
                )
                .map_err(sql_error)?;
            insert_typed_event_raw(
                &transaction,
                &EventScope::Ticket(id.clone()),
                &EventKind::TicketStateChanged {
                    from: TicketState::Blocked,
                    to: TicketState::Ready,
                    reason: Some(format!("human reopened: {note}")),
                },
            )?;
            let ticket = select_ticket(&transaction, &id)?;
            transaction.commit().map_err(sql_error)?;
            Ok(ticket)
        })
        .await
    }

    async fn reconcile_ticket_done_from_git(
        &self,
        id: &TicketId,
        head_sha: String,
        merge_sha: String,
    ) -> Result<Ticket, SubstrateError> {
        let id = id.clone();
        self.run_write(move |connection| {
            let transaction = connection.transaction().map_err(sql_error)?;
            let current = select_ticket(&transaction, &id)?;
            if current.state != TicketState::Ready {
                return Err(SubstrateError::Invalid {
                    field: "state".to_owned(),
                    message: format!(
                        "reconcile_ticket_done_from_git requires Ready; ticket is {}",
                        current.state
                    ),
                });
            }
            // Verify at least one prior TicketTransitionedToInReview event
            // exists for this ticket (D33).
            let has_history = ticket_has_in_review_event(&transaction, &id)?;
            if !has_history {
                return Err(SubstrateError::Invalid {
                    field: "history".to_owned(),
                    message:
                        "reconcile_ticket_done_from_git requires a prior TicketTransitionedToInReview event (D33)"
                            .to_owned(),
                });
            }
            transaction
                .execute(
                    "UPDATE tickets SET state = 'done', merge_sha = ?1, updated_at = ?2
                     WHERE id = ?3",
                    params![merge_sha, now_text(), id.as_str()],
                )
                .map_err(sql_error)?;
            insert_typed_event_raw(
                &transaction,
                &EventScope::Ticket(id.clone()),
                &EventKind::TicketVerifiedMerged {
                    head_sha: head_sha.clone(),
                    merge_sha: merge_sha.clone(),
                },
            )?;
            insert_typed_event_raw(
                &transaction,
                &EventScope::Ticket(id.clone()),
                &EventKind::TicketStateChanged {
                    from: TicketState::Ready,
                    to: TicketState::Done,
                    reason: Some("reconciled from git".to_owned()),
                },
            )?;
            if let Some(batch) = current.batch.as_ref() {
                maybe_auto_close_batch(&transaction, batch.as_str())?;
            }
            let ticket = select_ticket(&transaction, &id)?;
            transaction.commit().map_err(sql_error)?;
            Ok(ticket)
        })
        .await
    }

    async fn mark_ticket_done_manually(
        &self,
        id: &TicketId,
        attestation: ManualDoneAttestation,
    ) -> Result<Ticket, SubstrateError> {
        let id = id.clone();
        self.run_write(move |connection| {
            let transaction = connection.transaction().map_err(sql_error)?;
            let current = select_ticket(&transaction, &id)?;
            if current.state.is_terminal() {
                return Err(SubstrateError::Invalid {
                    field: "state".to_owned(),
                    message: format!(
                        "mark_ticket_done_manually refused: ticket already terminal ({})",
                        current.state
                    ),
                });
            }
            transaction
                .execute(
                    "UPDATE tickets SET state = 'done', updated_at = ?1 WHERE id = ?2",
                    params![now_text(), id.as_str()],
                )
                .map_err(sql_error)?;
            insert_typed_event_raw(
                &transaction,
                &EventScope::Ticket(id.clone()),
                &EventKind::TicketMarkedDoneManually {
                    claimant: attestation.claimant.clone(),
                    note: attestation.note.clone(),
                },
            )?;
            insert_typed_event_raw(
                &transaction,
                &EventScope::Ticket(id.clone()),
                &EventKind::TicketStateChanged {
                    from: current.state,
                    to: TicketState::Done,
                    reason: Some(format!("manual: {}", attestation.claimant)),
                },
            )?;
            if let Some(batch) = current.batch.as_ref() {
                maybe_auto_close_batch(&transaction, batch.as_str())?;
            }
            let ticket = select_ticket(&transaction, &id)?;
            transaction.commit().map_err(sql_error)?;
            Ok(ticket)
        })
        .await
    }

    async fn reject_ticket(&self, id: &TicketId, reason: String) -> Result<Ticket, SubstrateError> {
        let id = id.clone();
        self.run_write(move |connection| {
            let transaction = connection.transaction().map_err(sql_error)?;
            let current = select_ticket(&transaction, &id)?;
            if current.state.is_terminal() {
                return Err(SubstrateError::Invalid {
                    field: "state".to_owned(),
                    message: format!(
                        "reject_ticket refused: ticket already terminal ({})",
                        current.state
                    ),
                });
            }
            transaction
                .execute(
                    "UPDATE tickets SET state = 'rejected', updated_at = ?1 WHERE id = ?2",
                    params![now_text(), id.as_str()],
                )
                .map_err(sql_error)?;
            insert_typed_event_raw(
                &transaction,
                &EventScope::Ticket(id.clone()),
                &EventKind::TicketStateChanged {
                    from: current.state,
                    to: TicketState::Rejected,
                    reason: Some(reason),
                },
            )?;
            if let Some(batch) = current.batch.as_ref() {
                maybe_auto_close_batch(&transaction, batch.as_str())?;
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
            match &owner {
                Some(hand) => insert_typed_event_raw(
                    &transaction,
                    &EventScope::Ticket(id.clone()),
                    &EventKind::TicketAssigned { hand: hand.clone() },
                )?,
                None => insert_typed_event_raw(
                    &transaction,
                    &EventScope::Ticket(id.clone()),
                    &EventKind::TicketUnassigned {
                        reason: "cleared".to_owned(),
                    },
                )?,
            };
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
            insert_typed_event_raw(
                connection,
                &EventScope::Ticket(id.clone()),
                &EventKind::Note {
                    body: format!("LabelAdded {label}"),
                },
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
            insert_typed_event_raw(
                connection,
                &EventScope::Ticket(id.clone()),
                &EventKind::Note {
                    body: format!("LabelRemoved {label}"),
                },
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
            insert_typed_event_raw(
                connection,
                &EventScope::Ticket(from.clone()),
                &EventKind::Note {
                    body: format!("LinkCreated {} {}", kind, to.as_str()),
                },
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
            insert_typed_event_raw(
                connection,
                &EventScope::Ticket(from.clone()),
                &EventKind::Note {
                    body: format!("LinkRemoved {} {}", kind, to.as_str()),
                },
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
            insert_typed_event_raw(
                connection,
                &EventScope::Batch(name.clone()),
                &EventKind::BatchCreated,
            )?;
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
            let open_ticket_ids = open_ids
                .into_iter()
                .map(TicketId::new)
                .collect::<Result<Vec<_>, _>>()?;
            insert_typed_event_raw(
                &transaction,
                &EventScope::Batch(name.clone()),
                &EventKind::BatchClosed { open_ticket_ids },
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
            insert_typed_event_raw(
                connection,
                &EventScope::Hand(hand.id.clone()),
                &EventKind::HandRegistered,
            )?;
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
            insert_typed_event_raw(
                connection,
                &EventScope::Hand(id.clone()),
                &EventKind::HandHeartbeat,
            )?;
            Ok(())
        })
        .await
    }

    async fn hand_heartbeat(&self, id: &HandId) -> Result<(), SubstrateError> {
        Substrate::heartbeat(self, id).await
    }

    async fn record_event(&self, event: NewEvent) -> Result<Event, SubstrateError> {
        self.run_write(move |connection| {
            let scope = match event.ticket.as_ref() {
                Some(t) => EventScope::Ticket(t.clone()),
                None => EventScope::Site,
            };
            // Legacy path: write as a Note holding the body verbatim and use
            // the supplied kind discriminator.
            let kind = EventKind::Note {
                body: event.body.clone(),
            };
            let _rowid = insert_typed_event_with_kind(connection, &scope, &kind, &event.kind)?;
            // Re-fetch as legacy Event shape.
            let row: (Vec<u8>, String, String, Option<String>, String) = connection
                .query_row(
                    "SELECT id, at, kind, ticket, body FROM events
                     ORDER BY rowid DESC LIMIT 1",
                    [],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                        ))
                    },
                )
                .map_err(sql_error)?;
            let id = Uuid::from_slice(&row.0).map_err(|e| SubstrateError::Backend(Box::new(e)))?;
            let at = parse_required_time(&row.1)?;
            let kind = decode_event_body(&row.2, &row.4)?;
            let ticket = match row.3 {
                Some(t) => Some(TicketId::new(t)?),
                None => None,
            };
            Ok(Event {
                id,
                at,
                kind,
                ticket,
                body: row.4,
            })
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
                     ORDER BY at DESC, rowid DESC
                     LIMIT ?2",
                )
                .map_err(sql_error)?;
            let events = statement
                .query_map(params![since_text, capped_limit], legacy_event_from_row)
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?;
            Ok(events)
        })
        .await
    }

    async fn record_typed_event(
        &self,
        scope: EventScope,
        kind: EventKind,
    ) -> Result<EventId, SubstrateError> {
        self.run_write(move |connection| {
            let rowid = insert_typed_event_raw(connection, &scope, &kind)?;
            Ok(EventId(rowid))
        })
        .await
    }

    async fn tail_typed_events(
        &self,
        since: Option<DateTime<Utc>>,
        limit: usize,
    ) -> Result<Vec<TypedEvent>, SubstrateError> {
        self.run_read(move |connection| {
            let capped_limit = i64::try_from(limit).map_err(|error| SubstrateError::Invalid {
                field: "limit".to_owned(),
                message: error.to_string(),
            })?;
            let since_text = since.map(format_time);
            let mut statement = connection
                .prepare(
                    "SELECT rowid, at, kind, ticket, body, scope_kind, scope_batch,
                            scope_hand, scope_run_id
                     FROM events
                     WHERE (?1 IS NULL OR at > ?1)
                     ORDER BY rowid ASC
                     LIMIT ?2",
                )
                .map_err(sql_error)?;
            let events = statement
                .query_map(params![since_text, capped_limit], |row| {
                    typed_event_from_row(row)
                })
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?;
            // Validate decoding outside SQLite (rusqlite can't propagate
            // serde_json errors cleanly).
            events.into_iter().map(decode_typed_event_raw).collect()
        })
        .await
    }

    async fn ticket_events(
        &self,
        id: &TicketId,
        limit: usize,
    ) -> Result<Vec<TypedEvent>, SubstrateError> {
        let id = id.clone();
        self.run_read(move |connection| {
            let capped_limit = i64::try_from(limit).map_err(|error| SubstrateError::Invalid {
                field: "limit".to_owned(),
                message: error.to_string(),
            })?;
            let mut statement = connection
                .prepare(
                    "SELECT rowid, at, kind, ticket, body, scope_kind, scope_batch,
                            scope_hand, scope_run_id
                     FROM events
                     WHERE ticket = ?1
                     ORDER BY rowid DESC
                     LIMIT ?2",
                )
                .map_err(sql_error)?;
            let rows = statement
                .query_map(params![id.as_str(), capped_limit], |row| {
                    typed_event_from_row(row)
                })
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?;
            rows.into_iter().map(decode_typed_event_raw).collect()
        })
        .await
    }

    async fn foreman_status(&self) -> Result<ForemanStatus, SubstrateError> {
        self.run_read(move |connection| {
            let row: Option<(Option<i64>, Option<String>, String)> = connection
                .query_row(
                    "SELECT pid, started_at, mode FROM foreman LIMIT 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()
                .map_err(sql_error)?;
            let Some((pid, started_at, mode_text)) = row else {
                return Ok(stopped_foreman());
            };
            let mode = ForemanMode::from_str(&mode_text)?;
            let pid = match pid {
                Some(p) => Some(u32::try_from(p).map_err(|error| SubstrateError::Invalid {
                    field: "foreman.pid".to_owned(),
                    message: error.to_string(),
                })?),
                None => None,
            };
            Ok(ForemanStatus {
                pid,
                started_at: parse_optional_time(started_at)?,
                mode,
            })
        })
        .await
    }

    async fn record_foreman_start(&self, pid: u32) -> Result<(), SubstrateError> {
        Substrate::record_foreman_detached(self, pid).await
    }

    async fn record_foreman_attached(&self, pid: u32) -> Result<(), SubstrateError> {
        self.record_foreman_started(pid, ForemanMode::Attached)
            .await
    }

    async fn record_foreman_detached(&self, pid: u32) -> Result<(), SubstrateError> {
        self.record_foreman_started(pid, ForemanMode::Detached)
            .await
    }

    async fn record_foreman_stop(&self) -> Result<(), SubstrateError> {
        Substrate::record_foreman_stopped(self).await
    }

    async fn record_foreman_stopped(&self) -> Result<(), SubstrateError> {
        let site_name = self.site.name().to_owned();
        self.run_write(move |connection| {
            connection
                .execute(
                    "INSERT INTO foreman (site, pid, started_at, mode) VALUES (?1, NULL, NULL, 'stopped')
                     ON CONFLICT(site) DO UPDATE SET pid = NULL, started_at = NULL, mode = 'stopped'",
                    params![site_name],
                )
                .map_err(sql_error)?;
            insert_typed_event_raw(connection, &EventScope::Site, &EventKind::ForemanStopped)?;
            Ok(())
        })
        .await
    }

    async fn reserve_worktree(
        &self,
        run_id: &str,
        branch: &str,
    ) -> Result<PathBuf, SubstrateError> {
        NativeSubstrate::reserve_worktree(self, run_id, branch).await
    }

    async fn close_worktree(&self, run_id: &str) -> Result<(), SubstrateError> {
        NativeSubstrate::close_worktree(self, run_id).await
    }
}

impl NativeSubstrate {
    async fn record_foreman_started(
        &self,
        pid: u32,
        mode: ForemanMode,
    ) -> Result<(), SubstrateError> {
        let site_name = self.site.name().to_owned();
        let mode_str = mode.to_string();
        self.run_write(move |connection| {
            connection
                .execute(
                    "INSERT INTO foreman (site, pid, started_at, mode) VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(site) DO UPDATE SET pid = excluded.pid,
                       started_at = excluded.started_at, mode = excluded.mode",
                    params![site_name, i64::from(pid), now_text(), mode_str],
                )
                .map_err(sql_error)?;
            insert_typed_event_raw(
                connection,
                &EventScope::Site,
                &EventKind::ForemanStarted { mode, pid },
            )?;
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
    let version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(sql_error)?;
    if version < 2 {
        run_migration_0002(connection)?;
    }
    Ok(())
}

fn run_migration_0002(connection: &mut Connection) -> Result<(), SubstrateError> {
    // PRAGMA foreign_keys = OFF must run OUTSIDE any transaction. SQLite
    // silently ignores the toggle inside a txn (it's read at BEGIN).
    connection
        .execute_batch("PRAGMA foreign_keys = OFF;")
        .map_err(sql_error)?;

    let migration_result = (|| -> Result<(), SubstrateError> {
        let transaction = connection.transaction().map_err(sql_error)?;
        transaction
            .execute_batch(MIGRATION_0002)
            .map_err(sql_error)?;
        let mut statement = transaction
            .prepare("PRAGMA foreign_key_check;")
            .map_err(sql_error)?;
        let violations: Vec<(String, i64, String, i64)> = statement
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?;
        drop(statement);
        if !violations.is_empty() {
            return Err(SubstrateError::Invalid {
                field: "migration_0002".to_owned(),
                message: format!("foreign_key_check violations: {violations:?}"),
            });
        }
        transaction.commit().map_err(sql_error)?;
        Ok(())
    })();

    // Re-enable FKs regardless of success — leaving them off would silently
    // disable downstream integrity checks.
    let restore = connection.execute_batch("PRAGMA foreign_keys = ON;");
    migration_result?;
    restore.map_err(sql_error)?;
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
            let batch = BatchName::new(batch_name)?;
            insert_typed_event_raw(
                transaction,
                &EventScope::Batch(batch),
                &EventKind::BatchClosed {
                    open_ticket_ids: vec![],
                },
            )?;
        }
    }
    Ok(())
}

fn set_ticket_state_redirect(target: TicketState) -> SubstrateError {
    let pointer = match target {
        TicketState::InFlight => "use assign_to_hand",
        TicketState::InReview => "use transition_to_in_review",
        TicketState::Blocked => "use block_ticket",
        TicketState::Done => "use verify_ticket_merged or mark_ticket_done_manually",
        TicketState::Rejected => "use reject_ticket",
        TicketState::Ready => "use release_from_hand or unblock_ticket",
        _ => "see DESIGN.md §8.6 D31 for the typed state machine",
    };
    SubstrateError::Invalid {
        field: "ticket_state".to_owned(),
        message: format!("set_ticket_state refused for target {target} (D31): {pointer}"),
    }
}

fn block_reason_discriminator(reason: &BlockReason) -> &'static str {
    match reason {
        BlockReason::Dependency { .. } => "dependency",
        BlockReason::PrClosedUnmerged { .. } => "pr_closed_unmerged",
        BlockReason::RestackConflict { .. } => "restack_conflict",
        BlockReason::Human { .. } => "human",
        _ => "human",
    }
}

fn select_incoming_blocks_dependents(
    connection: &Connection,
    id: &TicketId,
) -> Result<Vec<TicketId>, SubstrateError> {
    let mut statement = connection
        .prepare("SELECT from_ticket FROM links WHERE to_ticket = ?1 AND kind = 'blocks'")
        .map_err(sql_error)?;
    let rows = statement
        .query_map(params![id.as_str()], |row| row.get::<_, String>(0))
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)?;
    rows.into_iter().map(TicketId::new).collect()
}

fn select_outgoing_blocks_predecessors(
    connection: &Connection,
    id: &TicketId,
) -> Result<Vec<TicketId>, SubstrateError> {
    let mut statement = connection
        .prepare("SELECT to_ticket FROM links WHERE from_ticket = ?1 AND kind = 'blocks'")
        .map_err(sql_error)?;
    let rows = statement
        .query_map(params![id.as_str()], |row| row.get::<_, String>(0))
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)?;
    rows.into_iter().map(TicketId::new).collect()
}

fn ticket_has_in_review_event(
    connection: &Connection,
    id: &TicketId,
) -> Result<bool, SubstrateError> {
    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM events
             WHERE ticket = ?1 AND kind = 'ticket_transitioned_to_in_review'",
            params![id.as_str()],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    Ok(count > 0)
}

fn ensure_hand_exists(connection: &Connection, id: &HandId) -> Result<(), SubstrateError> {
    let exists: Option<i64> = connection
        .query_row(
            "SELECT 1 FROM hands WHERE id = ?1",
            params![id.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    if exists.is_some() {
        Ok(())
    } else {
        Err(SubstrateError::NotFound {
            kind: "hand",
            id: id.to_string(),
        })
    }
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
            "SELECT id, batch, ordinal, title, body, state, owner, merge_sha,
                    block_reason_detail, created_at, updated_at
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

fn insert_typed_event_raw(
    connection: &Connection,
    scope: &EventScope,
    kind: &EventKind,
) -> Result<i64, SubstrateError> {
    insert_typed_event_with_kind(connection, scope, kind, kind.discriminator())
}

fn insert_typed_event_with_kind(
    connection: &Connection,
    scope: &EventScope,
    kind: &EventKind,
    kind_discriminator: &str,
) -> Result<i64, SubstrateError> {
    let id = Uuid::new_v4();
    let body = serde_json::to_string(kind).map_err(json_error)?;
    let (scope_kind, ticket, scope_batch, scope_hand, scope_run_id) = match scope {
        EventScope::Ticket(t) => ("ticket", Some(t.as_str().to_owned()), None, None, None),
        EventScope::Batch(b) => ("batch", None, Some(b.as_str().to_owned()), None, None),
        EventScope::Hand(h) => ("hand", None, None, Some(h.as_str().to_owned()), None),
        EventScope::Worktree { run_id } => ("worktree", None, None, None, Some(run_id.clone())),
        EventScope::Site => ("site", None, None, None, None),
        _ => ("site", None, None, None, None),
    };
    connection
        .execute(
            "INSERT INTO events
             (id, at, kind, ticket, body, scope_kind, scope_batch, scope_hand, scope_run_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                id.as_bytes().as_slice(),
                now_text(),
                kind_discriminator,
                ticket,
                body,
                scope_kind,
                scope_batch,
                scope_hand,
                scope_run_id,
            ],
        )
        .map_err(sql_error)?;
    Ok(connection.last_insert_rowid())
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
    let merge_sha: Option<String> = row.get(7)?;
    let block_reason_detail: Option<String> = row.get(8)?;
    let created_at: String = row.get(9)?;
    let updated_at: String = row.get(10)?;

    let block_reason = match block_reason_detail {
        Some(detail) => Some(parse_in_row(detail, |s| serde_json::from_str(&s))?),
        None => None,
    };

    Ok(Ticket {
        id: parse_in_row(id, TicketId::new)?,
        batch: parse_optional_in_row(batch, BatchName::new)?,
        ordinal: ordinal.and_then(|value| u32::try_from(value).ok()),
        title: row.get(3)?,
        body: row.get(4)?,
        state: parse_in_row(state, |value| TicketState::from_str(&value))?,
        labels: Vec::new(),
        owner: parse_optional_in_row(owner, HandId::new)?,
        merge_sha,
        block_reason,
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

/// Raw event row, decoded into Rust types but with `kind`/`body` still
/// stringly typed so we can decode via `decode_event_body` outside the
/// rusqlite mapping closure.
struct RawTypedEvent {
    rowid: i64,
    at: DateTime<Utc>,
    kind: String,
    body: String,
    scope_kind: String,
    ticket: Option<String>,
    scope_batch: Option<String>,
    scope_hand: Option<String>,
    scope_run_id: Option<String>,
}

fn typed_event_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawTypedEvent> {
    let rowid: i64 = row.get(0)?;
    let at: String = row.get(1)?;
    let kind: String = row.get(2)?;
    let ticket: Option<String> = row.get(3)?;
    let body: String = row.get(4)?;
    let scope_kind: String = row.get(5)?;
    let scope_batch: Option<String> = row.get(6)?;
    let scope_hand: Option<String> = row.get(7)?;
    let scope_run_id: Option<String> = row.get(8)?;
    Ok(RawTypedEvent {
        rowid,
        at: parse_time_in_row(at)?,
        kind,
        body,
        scope_kind,
        ticket,
        scope_batch,
        scope_hand,
        scope_run_id,
    })
}

fn decode_typed_event_raw(raw: RawTypedEvent) -> Result<TypedEvent, SubstrateError> {
    let kind = decode_event_body(&raw.kind, &raw.body)?;
    let scope = match raw.scope_kind.as_str() {
        "ticket" => {
            let t = raw.ticket.ok_or_else(|| SubstrateError::Invalid {
                field: "scope".to_owned(),
                message: "ticket-scope event missing ticket column".to_owned(),
            })?;
            EventScope::Ticket(TicketId::new(t)?)
        }
        "batch" => {
            let b = raw.scope_batch.ok_or_else(|| SubstrateError::Invalid {
                field: "scope".to_owned(),
                message: "batch-scope event missing scope_batch column".to_owned(),
            })?;
            EventScope::Batch(BatchName::new(b)?)
        }
        "hand" => {
            let h = raw.scope_hand.ok_or_else(|| SubstrateError::Invalid {
                field: "scope".to_owned(),
                message: "hand-scope event missing scope_hand column".to_owned(),
            })?;
            EventScope::Hand(HandId::new(h)?)
        }
        "worktree" => {
            let r = raw.scope_run_id.ok_or_else(|| SubstrateError::Invalid {
                field: "scope".to_owned(),
                message: "worktree-scope event missing scope_run_id column".to_owned(),
            })?;
            EventScope::Worktree { run_id: r }
        }
        "site" => EventScope::Site,
        other => {
            return Err(SubstrateError::Invalid {
                field: "scope_kind".to_owned(),
                message: format!("unknown scope_kind {other}"),
            });
        }
    };
    Ok(TypedEvent {
        id: EventId(raw.rowid),
        scope,
        kind,
        at: raw.at,
    })
}

/// Legacy reader that decodes a row of the events table into the deprecated
/// `Event` shape. Used by `tail_events` / `record_event` for the deprecated
/// API surface.
fn legacy_event_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Event> {
    let id_bytes: Vec<u8> = row.get(0)?;
    let at: String = row.get(1)?;
    let kind: String = row.get(2)?;
    let ticket: Option<String> = row.get(3)?;
    let body: String = row.get(4)?;
    let id = Uuid::from_slice(&id_bytes).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Blob, Box::new(error))
    })?;
    let decoded = decode_event_body(&kind, &body).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let ticket = match ticket {
        Some(t) => Some(parse_in_row(t, TicketId::new)?),
        None => None,
    };
    Ok(Event {
        id,
        at: parse_time_in_row(at)?,
        kind: decoded,
        ticket,
        body,
    })
}

/// Decode an event body using its kind discriminator. Tries the new tagged
/// JSON shape first; falls back to per-kind legacy reconstruction for rows
/// written by T007 before the typed-event migration.
fn decode_event_body(kind: &str, body: &str) -> Result<EventKind, SubstrateError> {
    // Path 1: new-format tagged JSON round-trips via #[serde(tag = "kind")].
    if let Ok(parsed) = serde_json::from_str::<EventKind>(body) {
        return Ok(parsed);
    }
    // Path 2: legacy T007 rows. Per-kind reconstruction.
    match kind {
        "note" => Ok(EventKind::Note {
            body: body.to_owned(),
        }),
        "ticket_state_changed" => {
            let value: serde_json::Value = serde_json::from_str(body).map_err(json_error)?;
            let from: TicketState =
                serde_json::from_value(value.get("from").cloned().unwrap_or_default())
                    .map_err(json_error)?;
            let to: TicketState =
                serde_json::from_value(value.get("to").cloned().unwrap_or_default())
                    .map_err(json_error)?;
            let reason = value
                .get("reason")
                .and_then(|r| r.as_str())
                .map(String::from);
            Ok(EventKind::TicketStateChanged { from, to, reason })
        }
        "ticket_created" => Ok(EventKind::TicketCreated {
            initial_state: TicketState::Ready,
        }),
        "ticket_assigned" => {
            let hand = HandId::new(body.trim()).map_err(|e| SubstrateError::Invalid {
                field: "legacy_ticket_assigned".to_owned(),
                message: e.to_string(),
            })?;
            Ok(EventKind::TicketAssigned { hand })
        }
        "ticket_unassigned" => Ok(EventKind::TicketUnassigned {
            reason: body.to_owned(),
        }),
        "batch_created" => Ok(EventKind::BatchCreated),
        "batch_closed" => Ok(EventKind::BatchClosed {
            open_ticket_ids: vec![],
        }),
        "foreman_started" => {
            let pid = body.trim().parse::<u32>().unwrap_or(0);
            Ok(EventKind::ForemanStarted {
                mode: ForemanMode::Detached,
                pid,
            })
        }
        "foreman_stopped" => Ok(EventKind::ForemanStopped),
        "hand_registered" => Ok(EventKind::HandRegistered),
        "hand_heartbeat" => Ok(EventKind::HandHeartbeat),
        other => Err(SubstrateError::Invalid {
            field: "event_kind".to_owned(),
            message: format!("unknown legacy event kind: {other}"),
        }),
    }
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

fn parse_required_time(value: &str) -> Result<DateTime<Utc>, SubstrateError> {
    DateTime::parse_from_rfc3339(value)
        .map(|time| time.with_timezone(&Utc))
        .map_err(|error| SubstrateError::Invalid {
            field: "timestamp".to_owned(),
            message: error.to_string(),
        })
}

fn parse_optional_time(value: Option<String>) -> Result<Option<DateTime<Utc>>, SubstrateError> {
    value.map(|value| parse_required_time(&value)).transpose()
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

pub mod foreman;

#[cfg(test)]
mod tests;
