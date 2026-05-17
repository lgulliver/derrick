# T007 — `derrick-substrate-native` SQLite-backed substrate (CRUD)

**Specialist owner**: `substrate-engineer` (opus)
**Crate**: `crates/derrick-substrate-native`
**Depends on**: `derrick-substrate` (the trait), `derrick-config` (for `Site`)
**Priority**: P0 — dogfooding-critical (substrate trait needs an actual storage backend to be useful).

## Why

DESIGN.md §8.2 commits to a SQLite-backed implementation of
`Substrate`: `.derrick/derrick.db`, WAL mode, single writer,
many readers. This ticket implements the storage and the
trait CRUD. **Foreman loop is T008**; this ticket stops at
the substrate API. Worktree integration (per §9.C.5) is also
this ticket — the substrate is what creates and tracks
worktrees for a run.

## Scope

### Schema (v1)

SQLite file at the configured `state.dir/derrick.db` (default
`.derrick/derrick.db`). Created via `init` migration at first
open. Single migration file: `migrations/0001_initial.sql`.

```sql
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

-- Single-row site identity.
CREATE TABLE site (
    name TEXT NOT NULL,
    prefix TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (name)
);

CREATE TABLE batches (
    name TEXT PRIMARY KEY,
    created_at TEXT NOT NULL,
    closed_at TEXT NULL
);

CREATE TABLE tickets (
    id TEXT PRIMARY KEY,
    batch TEXT NULL REFERENCES batches(name),
    ordinal INTEGER NULL,
    title TEXT NOT NULL,
    body TEXT NOT NULL DEFAULT '',
    state TEXT NOT NULL,
    owner TEXT NULL REFERENCES hands(id),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    CHECK (state IN ('ready', 'in_flight', 'blocked', 'done', 'rejected')),
    CHECK (ordinal IS NULL OR batch IS NOT NULL)
);

-- Labels as a side table (flexible without dragging in JSON).
CREATE TABLE ticket_labels (
    ticket_id TEXT NOT NULL REFERENCES tickets(id) ON DELETE CASCADE,
    label TEXT NOT NULL,
    PRIMARY KEY (ticket_id, label)
);

CREATE TABLE links (
    from_ticket TEXT NOT NULL REFERENCES tickets(id) ON DELETE CASCADE,
    to_ticket   TEXT NOT NULL REFERENCES tickets(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    PRIMARY KEY (from_ticket, to_ticket, kind),
    CHECK (kind IN ('blocks', 'related'))
);

CREATE TABLE hands (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    last_seen TEXT NULL,
    CHECK (kind IN ('claude', 'copilot', 'human'))
);

CREATE TABLE events (
    id BLOB PRIMARY KEY,              -- uuid v4 bytes
    at TEXT NOT NULL,
    kind TEXT NOT NULL,
    ticket TEXT NULL REFERENCES tickets(id) ON DELETE SET NULL,
    body TEXT NOT NULL DEFAULT ''
);

-- Foreman state is a single row keyed by site name.
-- v1 stores pid + started_at only. The attached/detached
-- distinction (T002 ForemanMode::Attached vs Detached) does
-- not yet have a write path on the Substrate trait, so v1
-- derives mode at read time: pid IS NULL → Stopped,
-- pid IS NOT NULL → Detached (the only path that exists
-- today is `derrick run` detaching the foreman). T008 adds
-- a trait method to distinguish and a migration to persist
-- the mode column.
CREATE TABLE foreman (
    site TEXT PRIMARY KEY REFERENCES site(name) ON DELETE CASCADE,
    pid INTEGER NULL,
    started_at TEXT NULL
);

-- Worktrees created by /add-feature runs.
CREATE TABLE worktrees (
    run_id TEXT PRIMARY KEY,
    branch TEXT NOT NULL,
    path TEXT NOT NULL,
    created_at TEXT NOT NULL,
    closed_at TEXT NULL
);

CREATE INDEX idx_tickets_state ON tickets(state);
CREATE INDEX idx_tickets_batch_ordinal ON tickets(batch, ordinal);
CREATE INDEX idx_tickets_owner ON tickets(owner);
CREATE INDEX idx_ticket_labels_label ON ticket_labels(label, ticket_id);
CREATE INDEX idx_links_to ON links(to_ticket);
CREATE INDEX idx_events_at ON events(at);
CREATE INDEX idx_events_ticket ON events(ticket);
CREATE INDEX idx_worktrees_branch ON worktrees(branch);
```

### Per-connection PRAGMAs

`journal_mode = WAL` is a database-wide setting and survives
across opens, but `foreign_keys` is **per-connection**. Both
the writer connection and every reader pool connection must
issue, immediately after open:

```sql
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;   -- 5s, generous enough for WAL contention
PRAGMA synchronous = NORMAL;  -- WAL-safe; faster than FULL
```

Reader connections additionally execute:

```sql
PRAGMA query_only = ON;       -- enforce read-only at the driver
```

so that even a bug in the read path cannot mutate. The
`open()` function is responsible for configuring these on
every connection it hands out.

### Site singleton check

The `site` table is a singleton (zero or one row). `open()`
enforces this by:

1. `SELECT COUNT(*) FROM site` — must be 0 or 1; on `> 1`
   return `SubstrateError::Invalid { field: "site", message:
   "DB has multiple site rows; corrupted state, refusing to
   open" }`.
2. If `COUNT == 0`: INSERT the supplied `Site` as the only
   row.
3. If `COUNT == 1`: SELECT the existing `(name, prefix)` and
   compare to the supplied `Site`. On mismatch return
   `SubstrateError::Invalid { field: "site", message: "DB
   site <X> does not match config site <Y>; refusing to
   open" }`. On match proceed.

This sequence prevents both misrouting an existing site's DB
onto a different config and the schema-allows-many problem
that a `PRIMARY KEY (name)` alone wouldn't catch.

(The schema retains `PRIMARY KEY (name)` for the join-target
property; a single-row table without a PRIMARY KEY can't be
referenced by FK constraints from `foreman`. We're not
adding a `CHECK ((SELECT COUNT(*) FROM site) <= 1)` trigger
because rusqlite's lack of `CREATE TRIGGER` ergonomics
combined with SQLite's eager trigger evaluation make the
runtime check cleaner.)

`schema_version` lives in `PRAGMA user_version` (= 1 for now).
Migrations run on `open()`: read `user_version`, apply
`0001_initial.sql` if 0, panic-safe-but-error-returning if
the on-disk version is newer than this binary knows.

### Public API

```rust
//! SQLite-backed `Substrate` implementation. See DESIGN.md §8.2.

use derrick_config::Site;
use derrick_substrate::*;

/// Configuration for opening the native substrate.
pub struct NativeConfig {
    /// Path to the SQLite file. The parent directory must
    /// exist; the substrate creates the file if absent.
    pub db_path: PathBuf,
    /// Worktree root (default `.derrick/worktrees/`).
    pub worktree_root: PathBuf,
}

pub struct NativeSubstrate { /* opaque */ }

impl NativeSubstrate {
    /// Open or create the substrate. Runs migrations to current
    /// schema; refuses to open a DB whose schema version exceeds
    /// this binary's known version.
    pub async fn open(config: NativeConfig, site: Site)
        -> Result<Self, SubstrateError>;

    /// Close the underlying connection pool. Idempotent.
    pub async fn close(self) -> Result<(), SubstrateError>;

    /// --- Worktree integration (§9.C.5) ---
    ///
    /// Two-phase lifecycle (reserve → finalize | rollback).
    /// The substrate owns the bookkeeping; the caller owns
    /// the `git worktree add` invocation.

    /// Reserve a worktree slot. Returns the planned absolute
    /// path. Creates a row in `worktrees` with `closed_at`
    /// NULL (state implied by absence of a finalize event).
    /// Returns `SubstrateError::Conflict` if the `run_id` is
    /// already reserved (including a reserved-but-unfinalized
    /// stale row from a crashed prior run — the caller
    /// resolves via `rollback_worktree` before re-reserving).
    /// Branch collisions with an existing live worktree row
    /// also return `Conflict`.
    pub async fn reserve_worktree(
        &self,
        run_id: &str,
        branch: &str,
    ) -> Result<PathBuf, SubstrateError>;

    /// Commit the reservation after a successful
    /// `git worktree add`. No-op semantically (the row already
    /// exists) but records a `WorktreeFinalized` event so the
    /// activity log distinguishes reserved-and-finalized from
    /// reserved-and-still-pending.
    pub async fn finalize_worktree(&self, run_id: &str)
        -> Result<(), SubstrateError>;

    /// Reverse a reservation when `git worktree add` fails.
    /// Deletes the row. Returns `Ok(())` if the row was
    /// missing (idempotent on already-rolled-back).
    pub async fn rollback_worktree(&self, run_id: &str)
        -> Result<(), SubstrateError>;

    /// Mark a worktree closed (post-merge or post-abandonment).
    /// Does not delete the directory; callers (or
    /// `derrick worktrees prune`) handle removal.
    pub async fn close_worktree(&self, run_id: &str)
        -> Result<(), SubstrateError>;

    pub async fn list_worktrees(&self, include_closed: bool)
        -> Result<Vec<WorktreeRecord>, SubstrateError>;
}

#[async_trait::async_trait]
impl Substrate for NativeSubstrate {
    /* every trait method implemented */
}

#[derive(Clone, Debug)]
pub struct WorktreeRecord {
    pub run_id: String,
    pub branch: String,
    pub path: PathBuf,
    pub created_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
}
```

### Behaviour the implementer must get right

- **Single writer**: all `Substrate` write methods serialise
  through a single mutex held while a SQL transaction is open.
  Read methods use a separate read-only connection pool
  (rusqlite + r2d2, or hand-rolled — implementer's choice).
- **Auto-close batch (per T002 contract)**: `set_ticket_state`
  performs the state update **and** the auto-close check
  **inside a single SQL transaction**, in this order:
  1. UPDATE the ticket's state and updated_at.
  2. INSERT the ticket-state-changed event.
  3. If the new state is terminal and the ticket belongs to a
     batch, SELECT COUNT(*) of tickets in that batch whose
     state is non-terminal.
  4. If that count is 0, UPDATE batches SET closed_at = now()
     WHERE name = ? AND closed_at IS NULL — the `IS NULL`
     guard makes the close idempotent. If the UPDATE affected
     1 row, INSERT a `BatchClosed` event. If it affected 0
     rows, the batch was already closed and we emit nothing
     (exactly-once event semantics).
  5. COMMIT.

  This transactional shape rules out racing BatchClosed
  events from concurrent state changes and is the canonical
  pattern. No SQL triggers needed (and avoided — triggers
  are harder to test).
- **Force-close batch (T002 contract)**: `close_batch` is
  idempotent on already-closed batches (returns the existing
  row); on a batch with non-terminal tickets, the
  `BatchClosed` event body lists those tickets' ids.
- **Ordinal preservation**: `tickets_in_batch` orders by
  `(ordinal NULLS LAST, created_at)`. SQLite's ORDER BY ASC
  puts NULLs first by default; remember `NULLS LAST` semantics
  via `ORDER BY ordinal IS NULL, ordinal, created_at`.
- **Creation into closed batch**: `create_ticket` checks the
  target batch's `closed_at` and returns
  `SubstrateError::Conflict` if it's set.
- **Activity events on every mutation**: every state change,
  link add/remove, batch close, etc. writes an `events` row.
  The substrate's `tail_events` API returns these.
- **Foreman state**: the `foreman` table tracks pid +
  started_at only in v1 (see schema note above). T002's
  `foreman_status()` returns `ForemanStatus { pid, started_at,
  mode }`. The native impl derives `mode`:
  - `pid IS NULL` → `ForemanMode::Stopped`
  - otherwise → `ForemanMode::Detached`

  `Attached` is unreachable through v1's write API and is
  reserved for T008. Document this clearly in the impl
  doc-comment.
- **All timestamps are UTC** (`DateTime<Utc>`); stored as
  ISO-8601 strings in TEXT columns.

### Dependencies

```toml
[dependencies]
derrick-substrate = { path = "../derrick-substrate" }
derrick-config = { path = "../derrick-config" }
async-trait = { workspace = true }
rusqlite = { workspace = true }
serde_json = { workspace = true }
thiserror = { workspace = true }
tokio = { workspace = true }
chrono = { workspace = true }
uuid = { workspace = true }
tracing = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
```

### Tests

Real SQLite via `tempfile::tempdir()`. No mocks. Each
`Substrate` trait method gets at least one happy-path test
and one error-path test:

- `site_initialises_from_config`.
- `create_ticket_persists` / `get_ticket_returns_some` /
  `get_ticket_missing_returns_none`.
- `create_ticket_into_closed_batch_returns_conflict`.
- `create_ticket_duplicate_id_returns_conflict`.
- `list_tickets_respects_filter` — for each filter field.
- `list_tickets_respects_limit` and `unlimited`.
- `set_ticket_state_writes_event`.
- `set_ticket_state_terminal_triggers_batch_autoclose`
  when last non-terminal ticket flips.
- `set_ticket_state_does_not_autoclose_if_others_open`.
- `assign_ticket_writes_event`.
- `add_label` / `remove_label` round-trip.
- `add_label_idempotent_on_duplicate`.
- `link_creates_row` / `unlink_removes_row`.
- `outgoing_links` / `incoming_links` return correct rows.
- `create_batch_persists` / `get_batch_returns_some`.
- `close_batch_is_idempotent_on_already_closed`.
- `force_close_batch_lists_open_ticket_ids_in_event_body`.
- `tickets_in_batch_orders_by_ordinal_then_created_at`.
- `tickets_in_batch_handles_null_ordinals_last`.
- `register_hand` / `list_hands` round-trip.
- `heartbeat_updates_last_seen`.
- `record_event` / `tail_events` (incl. `since` and `limit`).
- `tail_events_orders_by_at_descending`.
- `record_foreman_start` / `record_foreman_stop`.
- `foreman_status_reports_correctly`.
- `reserve_worktree_creates_row_and_returns_unique_path`.
- `reserve_worktree_duplicate_run_id_returns_conflict`.
- `reserve_worktree_branch_collision_returns_conflict`.
- `finalize_worktree_records_event`.
- `rollback_worktree_deletes_row`.
- `rollback_worktree_idempotent_on_missing`.
- `close_worktree_marks_closed`.
- `list_worktrees_respects_include_closed`.
- `open_rejects_mismatched_site` — populate a DB with site
  X, then call `open()` with site Y; expect `Invalid`.
- `open_rejects_multiple_site_rows` — inject two rows via
  raw rusqlite (simulating corruption), call `open()`,
  expect `Invalid` with "multiple site rows" in the message.
- `open_accepts_first_call_persists_site` — first call with
  empty DB writes the site row.
- `pragmas_set_on_every_connection` — open, query
  `PRAGMA foreign_keys` from a reader and from the writer;
  both return 1.
- `reader_connection_is_query_only` — try to INSERT through
  a reader connection; expect a SQL error.
- `batch_close_event_emitted_exactly_once_under_concurrent_terminal_writes` —
  two tickets in a batch; spawn two tasks that flip each to
  `Done` concurrently; assert exactly one `BatchClosed`
  event in the log.
- `concurrent_writes_serialise` — spawn 10 tokio tasks that
  each create a ticket; all 10 succeed with distinct ids
  and the events log has 10 entries.
- `concurrent_reads_dont_block` — while a write is in flight
  (hold a transaction open in one task), a read from another
  task returns within 100ms.
- `migration_runs_on_fresh_db`.
- `migration_skips_on_already_initialised_db`.
- `open_refuses_newer_schema_version`.

**Coverage target**: 88% (a few error paths are SQLite-side
and hard to inject; the bulk is covered).

## Out of scope

- The foreman loop body (T008).
- Hand types beyond bookkeeping (claude/copilot/human
  dispatch). T008/T012.
- PR stacking (T011 `derrick-stack`).
- The CLI surface (`derrick ticket new` etc.) — that's T010
  `derrick-cli`.

## Acceptance

- [ ] `cargo check -p derrick-substrate-native` passes.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] `cargo test -p derrick-substrate-native` passes; 3×
      stress green at default `--test-threads`.
- [ ] `cargo llvm-cov -p derrick-substrate-native --fail-under-lines 88`.
- [ ] Workspace `cargo llvm-cov --fail-under-lines 80` still passes.
- [ ] No `unwrap`/`expect`/`panic` in non-test code.
- [ ] All public types/methods documented.
- [ ] No gastown vocabulary.
- [ ] Concurrent read/write tests verify single-writer-many-
      readers under tokio.

## Reviewer notes (Codex)

Pre-implementation review. Crate stub. Focus on:
- Is the schema enough for the T002 trait surface? Anything
  missing?
- Are the auto-close + idempotent-close semantics
  implementable from the SQL alone or do they require trait-
  level logic?
- Is the worktree integration scoped correctly (substrate
  tracks the row; caller does the git ops)?
- Is the migration story sane for v1 (single file, no
  versioning beyond `user_version`)?

## Implementer notes (Copilot)

Stay in `crates/derrick-substrate-native/`. The `rusqlite`
workspace dep is `features = ["bundled"]` already. Use
`tokio::sync::Mutex` for the writer lock; spawn-blocking
around rusqlite calls (`tokio::task::spawn_blocking`).
