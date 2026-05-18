# T012 — foreman loop with D31/D32/D33 state machine integrity

**Specialist owner**: `substrate-engineer` (opus) — D31/D32/D33
specifically extend its stop conditions; trait extension and
schema migration are core substrate work.
**Crate**: `crates/derrick-substrate-native` (foreman impl) +
small extension to `crates/derrick-substrate` (trait)
**Depends on**: `derrick-config`, `derrick-substrate`, `derrick-substrate-native`, `derrick-tools` (for `gh` invocations via a thin adapter)
**Priority**: P0 — the state-machine pillar of crew mode. Without this, derrick is solo-only and the §8.6 integrity contract is unenforced.

## Why

DESIGN.md §8.6 + D31/D32/D33 are unimplemented until this
lands. Today T007 ships substrate CRUD with a foreman *table*
but no foreman *loop*. T010's pipeline orchestrator stops at
`tasks.md`. To run crew mode safely — i.e. dispatch tickets
to hands and rely on the result — the foreman must:

1. Walk `ready` tickets, dispatch to a hand (D31 ticket
   lifecycle: `Ready → InFlight → InReview → Done`).
2. **Verify** that `InReview` tickets actually merged before
   moving to `Done` — never trust hand self-report (the
   gastown anti-pattern).
3. Clean up abandoned worktrees, stale InReview tickets,
   stale InFlight tickets — append-only events, never
   silent state drift.
4. Cross-reference its own substrate state against `git log`
   on the target branch (D33).

This ticket implements the loop + the trait extension + the
schema migration in one go.

## Scope (v1, crew mode)

### Trait extension (`crates/derrick-substrate`)

Additive only (the trait crate is already published with
`#[non_exhaustive]` on every enum, so adding variants and
methods is non-breaking):

```rust
/// Lifecycle state for a ticket. New variant: InReview.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TicketState {
    Ready,
    InFlight,
    InReview,  // NEW (D31)
    Blocked,
    Done,
    Rejected,
}

impl TicketState {
    /// True if the state is terminal (`Done` or `Rejected`).
    /// `InReview` is **not** terminal.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Rejected)
    }
}

/// Ticket now carries `merge_sha`, set only when state
/// transitioned to `Done` via the verifier path.
pub struct Ticket {
    // ... existing fields ...
    pub merge_sha: Option<String>,
}

/// Foreman mode, now writable. T007 derived it from pid;
/// T012 distinguishes attached/detached.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ForemanMode { Stopped, Detached, Attached }

#[async_trait::async_trait]
pub trait Substrate: Send + Sync {
    // ... existing methods unchanged ...

    /// Transition a ticket from InReview to Done, recording
    /// the observed merge SHA. Rejects if the ticket is not
    /// currently InReview. **The only path to Done that this
    /// trait exposes** — there is no longer a way to set
    /// state == Done without a merge_sha. This is the D31
    /// teeth.
    async fn verify_ticket_merged(
        &self,
        id: &TicketId,
        merge_sha: String,
    ) -> Result<Ticket, SubstrateError>;

    /// Transition a ticket from InReview to Rejected with
    /// the reason recorded as an event body. Rejects if not
    /// InReview.
    async fn verify_ticket_unmerged(
        &self,
        id: &TicketId,
        reason: String,
    ) -> Result<Ticket, SubstrateError>;

    /// Record the foreman starting in attached mode (vs
    /// detached, which uses `record_foreman_start` from T002).
    /// T012 may extend later; for now both methods coexist.
    async fn record_foreman_attached(&self, pid: u32)
        -> Result<(), SubstrateError>;

    /// Heartbeat from a hand. Updates `hands.last_seen`.
    /// The cleanup pass uses this for D32's hand-abandonment
    /// detection.
    async fn hand_heartbeat(&self, id: &HandId)
        -> Result<(), SubstrateError>;
}
```

**`set_ticket_state` keeps working for non-terminal
transitions** (Ready ↔ InFlight ↔ Blocked, InFlight → InReview)
but **refuses** to set state to `Done` directly with
`SubstrateError::Invalid { field: "state", message:
"transitions to Done go through verify_ticket_merged
per D31; set_ticket_state does not accept Done as a
target" }`. Same for Rejected → use `verify_ticket_unmerged`.

This is the trait change that turns D31 from a principle
into a teeth-bearing rule.

### Schema migration (`crates/derrick-substrate-native`)

New file: `migrations/0002_state_machine_integrity.sql`.

```sql
-- 0002: D31/D32 state machine integrity columns
-- Idempotent if rerun (T007 migration runner checks
-- user_version before applying).

-- Add InReview to ticket state CHECK constraint by
-- rebuilding the table (SQLite doesn't support ALTER
-- TABLE … DROP CONSTRAINT). Same-shape table; data
-- preserved via INSERT INTO.
PRAGMA foreign_keys = OFF;

CREATE TABLE tickets_new (
    id TEXT PRIMARY KEY,
    batch TEXT NULL REFERENCES batches(name),
    ordinal INTEGER NULL,
    title TEXT NOT NULL,
    body TEXT NOT NULL DEFAULT '',
    state TEXT NOT NULL,
    owner TEXT NULL REFERENCES hands(id),
    merge_sha TEXT NULL,            -- NEW (D31)
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    CHECK (state IN ('ready', 'in_flight', 'in_review',
                     'blocked', 'done', 'rejected')),
    CHECK (ordinal IS NULL OR batch IS NOT NULL),
    CHECK (state != 'done' OR merge_sha IS NOT NULL)
                                     -- D31: Done requires evidence
);

INSERT INTO tickets_new
  (id, batch, ordinal, title, body, state, owner,
   merge_sha, created_at, updated_at)
SELECT
  id, batch, ordinal, title, body, state, owner,
  NULL, created_at, updated_at
FROM tickets;

-- Rebuild the indexes ON the new table (SQLite drops
-- them with the old table).
DROP TABLE tickets;
ALTER TABLE tickets_new RENAME TO tickets;

CREATE INDEX idx_tickets_state ON tickets(state);
CREATE INDEX idx_tickets_batch_ordinal ON tickets(batch, ordinal);
CREATE INDEX idx_tickets_owner ON tickets(owner);

-- Foreman gets mode column (D31/D33).
ALTER TABLE foreman ADD COLUMN mode TEXT NOT NULL DEFAULT 'stopped'
  CHECK (mode IN ('stopped', 'detached', 'attached'));

PRAGMA foreign_keys = ON;
PRAGMA user_version = 2;
```

The migration handles existing T007 v1 DBs: all existing
tickets retain their state; none of them are `Done` so the
new CHECK constraint passes trivially. The `merge_sha`
column starts NULL for legacy rows; the foreman's verifier
path will populate it for new transitions but won't
backfill historical rows.

The native substrate's `open()` checks `user_version`:
- `0` → run 0001 then 0002.
- `1` → run 0002 only.
- `2` → no-op.
- `>2` → refuse with "DB is from a newer derrick".

### Foreman loop (`crates/derrick-substrate-native/src/foreman.rs`)

```rust
//! Foreman loop. See DESIGN.md §8.6.

use crate::NativeSubstrate;
use derrick_substrate::*;

pub struct Foreman {
    substrate: std::sync::Arc<NativeSubstrate>,
    config: derrick_config::Config,
    pr_status: Box<dyn PullRequestStatus>,
    repo_root: std::path::PathBuf,
    poll_interval: std::time::Duration,
    in_review_ttl: chrono::Duration,
    hand_ttl: chrono::Duration,
}

impl Foreman {
    pub fn new(
        substrate: std::sync::Arc<NativeSubstrate>,
        config: derrick_config::Config,
        pr_status: Box<dyn PullRequestStatus>,
        repo_root: std::path::PathBuf,
    ) -> Self;

    /// Run a single loop iteration. Public so tests can drive
    /// it deterministically without spawning the background
    /// task. Returns a structured TickReport describing what
    /// changed.
    pub async fn tick(&self) -> Result<TickReport, ForemanError>;

    /// Run the loop in foreground until shutdown signal.
    /// Returns on signal or when no work remains and the
    /// `exit_when_idle` config flag is set.
    pub async fn run_attached(&self) -> Result<(), ForemanError>;

    /// Spawn the loop as a detached tokio task. Returns the
    /// task handle; the caller persists the pid via
    /// substrate.record_foreman_start(pid) before this
    /// returns. The task runs until shutdown.
    pub async fn run_detached(self) -> Result<tokio::task::JoinHandle<()>, ForemanError>;
}

#[derive(Clone, Debug, Default)]
pub struct TickReport {
    pub cleanup_actions: Vec<CleanupAction>,
    pub verifier_actions: Vec<VerifierAction>,
    pub unblocked: Vec<TicketId>,
    pub dispatched: Vec<TicketId>,
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum CleanupAction {
    PrunedAbandonedWorktree { run_id: String },
    RequeuedAbandonedHand { ticket: TicketId, hand: HandId },
    TriggeredStaleInReviewCheck { ticket: TicketId },
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum VerifierAction {
    /// Ticket transitioned from InReview to Done.
    Merged { ticket: TicketId, merge_sha: String },
    /// Ticket transitioned from InReview to Rejected.
    Unmerged { ticket: TicketId, reason: String },
    /// Ticket still in flight; verifier emitted an
    /// escalation event but no state change.
    StuckEscalated { ticket: TicketId },
}

#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum ForemanError {
    #[error("substrate error: {0}")]
    Substrate(#[from] SubstrateError),
    #[error("pr status check failed: {0}")]
    PrStatus(Box<dyn std::error::Error + Send + Sync>),
    #[error("io error at {path}: {source}")]
    Io { path: std::path::PathBuf, source: std::io::Error },
}
```

### `PullRequestStatus` trait (in `derrick-substrate-native`)

```rust
/// Abstracts the git+gh callouts so the foreman is
/// testable without a real github. The native impl shells
/// to `gh pr view --json` via `derrick-tools`-style
/// subprocess; tests provide a mock that returns canned
/// states.
#[async_trait::async_trait]
pub trait PullRequestStatus: Send + Sync {
    /// Check whether the PR for this branch has been merged.
    /// Returns the merge commit SHA if so. None if still
    /// open. Err if the PR is closed unmerged or doesn't
    /// exist (caller decides what to do — typically map
    /// closed-unmerged to Rejected).
    async fn check_merged(&self, branch: &str)
        -> Result<PrCheckResult, Box<dyn std::error::Error + Send + Sync>>;
}

#[derive(Clone, Debug)]
pub enum PrCheckResult {
    Merged { sha: String },
    Open,
    ClosedUnmerged,
    NotFound,
}

/// Production impl shells to `gh pr view <branch>
/// --json state,mergeCommit --jq ...`.
pub struct GhPullRequestStatus { /* opaque */ }
impl GhPullRequestStatus { pub fn new() -> Self; }
```

### Loop iteration (concrete; mirrors §8.6 step-for-step)

`tick()` body, sequentially:

1. **Cleanup pass** (D32):
   - Walk `worktrees` rows where `closed_at IS NULL` and a
     finalize event is missing AND `created_at` older than
     `cleanup.worktree_ttl` (default 24h). Prune the
     worktree directory via `git worktree remove --force`,
     delete the row, emit `WorktreeAbandoned` event.
   - Walk hands with `last_seen` older than `hand_ttl`
     (default 30 minutes). For each hand owning an
     `InFlight` ticket, transition the ticket back to
     `Ready` via `set_ticket_state`, emit `HandAbandoned`
     event with the hand id and prior ticket assignment.
   - List tickets in `InReview` with `updated_at` older than
     `in_review_ttl` (default 24h). Add each to the verifier
     pass's eager queue (so they're rechecked immediately
     instead of waiting another poll cycle).

2. **Verifier pass** (D31, the teeth):
   - For each `InReview` ticket (including the eager queue
     from step 1):
     - Compute the expected branch name (derrick-stack
       convention `derrick/<batch>/<ticket_id>`).
     - Call `pr_status.check_merged(branch)`.
     - `Merged { sha }` → `verify_ticket_merged(id, sha)`.
       Records `VerifierAction::Merged`.
     - `ClosedUnmerged` → `verify_ticket_unmerged(id,
       "pr closed unmerged")`. Records `Unmerged`.
     - `NotFound` → if the ticket is past TTL, emit
       `EscalationStuckInReview` event with body
       *"no PR found for branch <name>; human triage
       required"* and record `StuckEscalated`. Otherwise
       leave it alone (PR might not be opened yet).
     - `Open` → leave it; future tick rechecks.

3. **Reconcile Blocked**:
   - For each `Blocked` ticket, re-check all `blocks`-link
     predecessors. If every predecessor is now terminal,
     transition to `Ready`; record `unblocked`.

4. **Dispatch ready tickets**:
   - Up to `parallelism.batch_max` concurrent in-flight
     hands. Query ready tickets, sort by ordinal (within
     batch) then created_at, dispatch the first N where
     N + currently-in-flight ≤ batch_max.
   - Dispatch goes through `HandDispatcher::dispatch` —
     a trait this crate defines but doesn't fully
     implement. `claude` and `human` implementations
     are inline; `copilot` is **stub-only** in T012 and
     fully implemented in T013 `derrick-copilot`. A
     stub copilot dispatcher returns
     `ForemanError::PrStatus(...)` with "copilot dispatch
     not implemented; see T013" so the user gets a clear
     error rather than silent skipping.

5. **Sleep** `poll_interval` (default 10s; configurable via
   `tools.foreman.poll_interval`).

### Configuration additions to `derrick.yaml`

```yaml
tools:
  foreman:
    poll_interval: "10s"
    in_review_ttl: "24h"
    hand_ttl: "30m"
    worktree_ttl: "24h"
    exit_when_idle: false  # for batch-style invocations
```

These slot into the existing `tools:` block; the parser is
already extensible per T001's `#[serde(default)]` pattern.

### CLI wiring

T012 adds two subcommands to `derrick-cli`:

- `derrick foreman start [--attached | --detached]` — starts
  the loop. Default detached; writes pid to
  `.derrick/foreman.pid`. Returns immediately for detached;
  blocks for attached.
- `derrick foreman stop` — sends shutdown signal to the
  pid, awaits exit, removes pid file.
- `derrick foreman tick` — runs a single iteration in
  foreground. Useful for tests and `derrick status` cron
  setups.

`derrick run add-feature` in `mode: crew` now optionally
starts the foreman after `tasks` if `--start-foreman` is
passed; default false. The flow doesn't dispatch hands
itself — that's the foreman's job once started.

### Dependencies

```toml
[dependencies]
derrick-config = { path = "../derrick-config" }
derrick-substrate = { path = "../derrick-substrate" }
derrick-tools = { path = "../derrick-tools" }   # for gh shell via HostAdapter pattern
async-trait = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
thiserror = { workspace = true }
tokio = { workspace = true }
tracing = { workspace = true }
rusqlite = { workspace = true }
chrono = { workspace = true }
uuid = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
```

No new top-level workspace deps. (`gh` is invoked at runtime
via a subprocess; we don't add a Rust gh client crate.)

### Tests

Real SQLite via tempfile. Mocked `PullRequestStatus` and
mock `HandDispatcher` impls inline.

**Trait extension:**

- `set_ticket_state_done_refused_with_d31_message` —
  attempt `set_ticket_state(id, Done, _)` directly →
  `Invalid { field: "state", .. }` with the D31 message.
- `verify_ticket_merged_transitions_in_review_to_done`.
- `verify_ticket_merged_refuses_when_not_in_review`.
- `verify_ticket_unmerged_transitions_in_review_to_rejected`.
- `done_ticket_requires_merge_sha_at_schema_level` —
  raw rusqlite INSERT with state=done AND merge_sha=NULL →
  CHECK constraint fires.

**Migration:**

- `migration_0002_upgrades_v1_db_in_place` — populate a
  T007 v1 DB, open it, assert PRAGMA user_version == 2
  afterwards and all data preserved.
- `migration_0002_idempotent_on_v2_db`.
- `migration_refuses_v3_db` — refuses with the clear error.

**Verifier:**

- `verifier_marks_merged_tickets_done`.
- `verifier_marks_unmerged_tickets_rejected`.
- `verifier_escalates_stuck_in_review_past_ttl`.
- `verifier_leaves_pr_open_tickets_alone`.

**Cleanup (D32):**

- `cleanup_prunes_abandoned_worktrees_past_ttl`.
- `cleanup_requeues_inflight_with_dead_hand`.
- `cleanup_triggers_eager_verifier_on_stale_in_review`.

**Dispatch:**

- `dispatch_respects_batch_max_parallelism`.
- `dispatch_orders_by_ordinal_then_created_at`.
- `dispatch_copilot_stub_surfaces_t013_pointer`.
- `unblocked_tickets_become_ready`.

**Tick determinism:**

- `tick_against_canned_substrate_produces_canned_report` —
  reusable fixture that drives a full tick and asserts the
  `TickReport` byte-for-byte.

**Concurrency:**

- `parallel_ticks_serialise_through_writer_mutex` — two
  tasks calling tick() concurrently against the same
  substrate; assert no race conditions on state transitions.

**Coverage target**: 85% (lots of error branches in cleanup
+ verifier paths; 85% is the realistic gate).

## Out of scope

- Real `derrick-copilot` hand impl. T013.
- Real `derrick-stack` integration (creating PRs, restacking
  on merge). T014.
- The `code-review-before-pr-open` feature (§11 "Later"
  adversarial-code-review note from the prior turn).
- `derrick foreman logs` tail command — useful, follow-up.
- Multi-site federation. Out of scope for v1 entirely.

## D31/D32/D33 compliance checklist (must all be true)

- [ ] Done state transitions ONLY via `verify_ticket_merged`.
- [ ] DB-level CHECK enforces `state=done → merge_sha NOT NULL`.
- [ ] Verifier consults `gh pr view` (via the trait), not
      substrate state alone (D33).
- [ ] Cleanup pass walks abandoned worktrees + dead hands +
      stale InReview on every tick (D32).
- [ ] Append-only events for every state transition — no row
      overwrites that lose the previous state (D31).
- [ ] `set_ticket_state(_, Done, _)` is rejected; no path to
      Done bypasses the verifier.

## Acceptance

- [ ] `cargo check -p derrick-substrate-native` passes.
- [ ] `cargo check -p derrick-substrate` passes (trait
      extension is non-breaking).
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] `cargo test -p derrick-substrate-native` passes; 3×
      stress green.
- [ ] `cargo test -p derrick-substrate` still passes — the
      additive trait change doesn't break existing tests.
- [ ] `cargo llvm-cov -p derrick-substrate-native --fail-under-lines 85`.
- [ ] Workspace `cargo llvm-cov --fail-under-lines 80` passes.
- [ ] No `unwrap`/`expect`/`panic` in non-test code.
- [ ] No gastown vocabulary.
- [ ] D31/D32/D33 compliance checklist (above) all boxes
      checked, demonstrated by named tests.

## Reviewer notes (Codex)

Pre-implementation review. Focus on:
- Is the additive trait change actually non-breaking for
  existing derrick-flow consumers? (T010 uses
  `set_ticket_state` for some transitions.)
- Is the migration safe on a populated v1 DB? Specifically
  the table-rebuild approach SQLite forces for adding a
  CHECK constraint.
- Does the verifier sequencing leave any state gap where a
  PR could merge between the InReview transition and the
  next tick? If so, is the eventual-consistency window
  bounded and observable per D31?
- Is the `HandDispatcher` trait shape correct given T013
  hasn't drafted yet? Anything that'd force T013 to break
  the trait?

## Implementer notes (Copilot)

Trait additions go in `derrick-substrate` (small file
edit). Migration SQL in
`crates/derrick-substrate-native/migrations/0002_*.sql`.
Foreman loop body in
`crates/derrick-substrate-native/src/foreman.rs` (new
module). CLI subcommands added to
`derrick-cli/src/commands/foreman.rs` (new file).

Mock the `PullRequestStatus` and `HandDispatcher` in tests
via small inline impls; no need for `mockall` or similar.

D32's `.derrick/.adopt-stage-*` cleanup pass is owed to
T012 per T011's TODO. Extend the cleanup step 1 to also
walk those dirs with the same TTL behavior.
