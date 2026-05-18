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

**This is a breaking change to the `Substrate` trait** (adds
required methods, narrows `set_ticket_state` semantics).
Acceptable because the only impl today is
`derrick-substrate-native` which lands the corresponding
changes in this same ticket. External impls in the future
will see a clear semver bump on the trait crate.

Enum variant additions remain non-breaking thanks to
`#[non_exhaustive]`.

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
    /// True if the state is terminal — `Done` or `Rejected`
    /// only. `InReview` and **`Blocked`** are NOT terminal:
    /// `Blocked` awaits a human decision (re-open or reject)
    /// and a batch must not auto-close while it contains
    /// Blocked tickets.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Rejected)
    }
}

/// Ticket now carries `merge_sha` (set only via the
/// verifier path) and `block_reason` (set only while state
/// == Blocked, cleared on transition out).
pub struct Ticket {
    // ... existing fields ...
    pub merge_sha: Option<String>,
    pub block_reason: Option<BlockReason>,
}

/// Why a ticket is Blocked. Determines whether the cleanup
/// pass may auto-unblock it.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum BlockReason {
    /// Blocked on a `blocks`-link predecessor that wasn't
    /// terminal at dispatch time. Auto-unblocks when the
    /// predecessor reaches Done/Rejected.
    Dependency { predecessor: TicketId },
    /// Verifier saw the PR closed without a merge.
    /// Requires human action.
    PrClosedUnmerged { branch: String, pr_url: Option<String> },
    /// Stacking restack conflict (D19). Requires human
    /// action to rebase.
    RestackConflict { recipe: String },
    /// User explicitly blocked via `derrick ticket block`.
    Human { note: String },
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
    /// both the head SHA the verifier observed on the
    /// InReview branch and the actual merge commit on the
    /// target branch. For fast-forward merges these are
    /// equal; for squash/rebase merges they differ.
    /// `merge_sha` is what gets stored on the ticket row.
    /// Rejects if the ticket is not currently InReview.
    /// **The canonical path to Done.**
    async fn verify_ticket_merged(
        &self,
        id: &TicketId,
        head_sha: String,
        merge_sha: String,
    ) -> Result<Ticket, SubstrateError>;

    /// Transition a ticket from InReview to **Blocked** with
    /// `block_reason = BlockReason::PrClosedUnmerged{..}`
    /// (D32: a closed-unmerged PR blocks the ticket pending
    /// human decision; it is not auto-rejected). Atomic:
    /// state + block_reason + `TicketVerifiedUnmerged` event
    /// + `TicketStateChanged { to: Blocked }` event in one
    /// transaction. Refuses if not currently InReview.
    async fn verify_ticket_unmerged(
        &self,
        id: &TicketId,
        branch: String,
        pr_url: Option<String>,
    ) -> Result<Ticket, SubstrateError>;

    /// Transition any non-terminal state → Blocked with a
    /// structured `BlockReason`. Atomic: sets `state =
    /// blocked`, populates `block_reason` and
    /// `block_reason_detail` (serialised JSON of the
    /// `BlockReason` value), emits `TicketStateChanged {
    /// to: Blocked, reason: Some(<discriminator>) }` event.
    /// This is the **only** path to Blocked from the typed
    /// API (`set_ticket_state(_, Blocked, _)` is refused).
    /// Used by:
    /// - Dispatch step when a `blocks`-predecessor isn't
    ///   terminal (`BlockReason::Dependency`).
    /// - Stacking restack failure (`BlockReason::RestackConflict`).
    /// - User-facing `derrick ticket block <id> --on <p>`
    ///   (`BlockReason::Human` or `Dependency`).
    /// - Verifier closed-unmerged path calls
    ///   `verify_ticket_unmerged` instead (so it can refuse
    ///   non-InReview tickets at the same time).
    async fn block_ticket(
        &self,
        id: &TicketId,
        reason: BlockReason,
    ) -> Result<Ticket, SubstrateError>;

    /// Auto-unblock path used by the cleanup pass (D32
    /// step 4). Clears `block_reason` and transitions
    /// Blocked → Ready. Only callable when `block_reason`
    /// is the `Dependency` flavour AND all
    /// `blocks`-predecessors are now terminal; refuses
    /// otherwise. The cleanup pass calls this after
    /// verifying the predecessor states; the substrate
    /// re-verifies inside the same transaction to close
    /// the TOCTOU window.
    async fn unblock_ticket(
        &self,
        id: &TicketId,
    ) -> Result<Ticket, SubstrateError>;

    /// Human recovery path: transition Blocked → Ready
    /// regardless of `block_reason` flavour, capturing
    /// the human's note in the resulting state-change
    /// event. Called by `derrick ticket reopen <id>
    /// --note <text>`. Atomic state + `block_reason` clear
    /// + event emit. Refuses if the ticket is not Blocked.
    async fn human_reopen_blocked(
        &self,
        id: &TicketId,
        note: String,
    ) -> Result<Ticket, SubstrateError>;

    /// D33 pre-dispatch reconciliation. The ticket is
    /// currently `Ready` (re-queued from InReview by the
    /// cleanup pass) but the foreman's git check shows the
    /// recorded `head_sha` from a prior
    /// `TicketTransitionedToInReview` event is now on
    /// target. Transitions Ready → Done directly with the
    /// observed SHAs. Implementations MUST verify that the
    /// ticket has at least one prior
    /// `TicketTransitionedToInReview` event in its history;
    /// without that evidence, the call is refused. This
    /// distinguishes the legitimate reconciliation case
    /// from a speculative "any Ready ticket might be done"
    /// scan, which D33 explicitly rules out.
    async fn reconcile_ticket_done_from_git(
        &self,
        id: &TicketId,
        head_sha: String,
        merge_sha: String,
    ) -> Result<Ticket, SubstrateError>;

    /// Atomic dispatch transition: Ready → InFlight + set
    /// `owner = hand` in a single write. Refuses if the
    /// ticket is not currently Ready or if the hand row
    /// does not exist. Emits a
    /// `TicketStateChanged { from: Ready, to: InFlight }`
    /// event followed by `TicketAssigned { hand }` (both
    /// in the same transaction; readers see both or
    /// neither).
    async fn assign_to_hand(
        &self,
        id: &TicketId,
        hand: &HandId,
    ) -> Result<Ticket, SubstrateError>;

    /// Atomic abandonment transition: any non-terminal
    /// state → Ready + clear `owner` in one write. Used by
    /// the cleanup pass when a hand goes silent past its
    /// TTL. Emits `TicketStateChanged { from: X, to: Ready,
    /// reason: Some(_) }` followed by `TicketUnassigned`,
    /// both in the same transaction. Refuses if the ticket
    /// is already terminal.
    async fn release_from_hand(
        &self,
        id: &TicketId,
        reason: String,
    ) -> Result<Ticket, SubstrateError>;

    /// Mark a ticket Done in **`mode: solo` only**. DESIGN.md
    /// §8.2 exposes `derrick ticket done <id>` for the human
    /// hand path where no PR is ever opened. Crew/copilot
    /// modes never reach Done via this path — they go through
    /// `verify_ticket_merged` after the foreman observes the
    /// merge SHA on target. The attestation is recorded as
    /// the event payload so the audit trail records *who*
    /// said done and *why*.
    ///
    /// The mode guard lives **at the CLI layer**, not in the
    /// substrate trait: `derrick ticket done <id>` reads
    /// `derrick.yaml.mode` and refuses with a D31 pointer if
    /// it's not `solo`. The substrate trait method itself
    /// trusts its caller (consistent with the rest of the
    /// trait — substrate doesn't know about modes). This
    /// keeps `NativeSubstrate`'s open shape unchanged.
    ///
    /// Substrate-level refusal: when the ticket is currently
    /// Done or Rejected (idempotency-via-error, not silent
    /// re-write).
    async fn mark_ticket_done_manually(
        &self,
        id: &TicketId,
        attestation: ManualDoneAttestation,
    ) -> Result<Ticket, SubstrateError>;

    /// Record the foreman starting in attached mode. Writes
    /// **both** the `foreman` row's `mode` column (set to
    /// `attached`, pid recorded) **and** an
    /// `EventKind::ForemanStarted { mode: ForemanMode::Attached, pid }`
    /// event. The row reflects current state for cheap reads;
    /// the event preserves history.
    async fn record_foreman_attached(&self, pid: u32)
        -> Result<(), SubstrateError>;

    /// Record the foreman starting in detached mode. Symmetric
    /// with the attached path: writes mode = `detached` to the
    /// row and an `EventKind::ForemanStarted { mode:
    /// ForemanMode::Detached, pid }` event. **Replaces T002's
    /// `record_foreman_start`** — the rename is a small
    /// breaking change inside the substrate trait, fixed up in
    /// the same ticket. Migration shim: `record_foreman_start`
    /// stays as a `#[deprecated]` `async fn` for one release
    /// that forwards to `record_foreman_detached`.
    async fn record_foreman_detached(&self, pid: u32)
        -> Result<(), SubstrateError>;

    /// Record the foreman stopping cleanly. Writes mode =
    /// `stopped`, clears `pid`, emits
    /// `EventKind::ForemanStopped`.
    async fn record_foreman_stopped(&self)
        -> Result<(), SubstrateError>;

    /// Heartbeat from a hand. Updates `hands.last_seen`.
    /// The cleanup pass uses this for D32's hand-abandonment
    /// detection.
    async fn hand_heartbeat(&self, id: &HandId)
        -> Result<(), SubstrateError>;

    /// Record the hand's claim that work is ready for review.
    /// Transitions `InFlight` → `InReview`. Carries the
    /// metadata D33 needs to verify against git later: the
    /// branch the hand pushed to, the PR url+number if the
    /// hand opened one, and the head SHA at the moment of
    /// claim. `verify_ticket_merged` later compares against
    /// these recorded values.
    async fn transition_to_in_review(
        &self,
        id: &TicketId,
        review: InReviewMetadata,
    ) -> Result<Ticket, SubstrateError>;
}

#[derive(Clone, Debug)]
pub struct ManualDoneAttestation {
    /// Human identity claiming completion. Required so the
    /// event log records who said done.
    pub claimant: String,
    /// Reason or note. Free-form; recorded in the event body.
    pub note: String,
}

#[derive(Clone, Debug)]
pub struct InReviewMetadata {
    /// Branch the hand pushed (must match
    /// derrick-stack's `derrick/<batch>/<ticket_id>`
    /// convention when stacking is on).
    pub branch: String,
    /// PR URL if opened. None means the hand is mid-push.
    pub pr_url: Option<String>,
    /// PR number (extracted from URL for convenience).
    pub pr_number: Option<u64>,
    /// Head commit SHA at the moment of transition. The
    /// verifier reconciles this against git log on the
    /// target branch (D33).
    pub head_sha: String,
}
```

**`set_ticket_state` is narrowed to the no-op idempotency
path only** (the full refusal table is in the "Legacy
mutation surface" section below). Every real transition
has a dedicated typed method:
- `Done` ← `verify_ticket_merged` (D31) or
  `mark_ticket_done_manually` (`mode: solo` only — refused
  otherwise).
- `Blocked` ← `verify_ticket_unmerged` (per D32: a
  closed-unmerged PR is *blocked*, not *rejected*; a human
  decides whether to re-open with a new branch).
- `Rejected` is reserved for explicit user rejection via
  the §8.2 mutation API (future ticket; not implemented in
  T012).
- `InReview` ← `transition_to_in_review` (carries the D33
  metadata).

Each refusal returns `SubstrateError::Invalid` with a
pointer to the correct method. This is the trait change
that turns D31 from a principle into a teeth-bearing rule.

**Legacy mutation surface — explicit narrowing.** To prevent
the new state-integrity contract from being bypassed via
T007's methods, T012 narrows the old public API in the
same trait change:

- `assign_ticket(id, owner)` is **removed from the public
  trait** (the `derrick-substrate` crate no longer
  re-exports it). The atomic path is `assign_to_hand` /
  `release_from_hand`. An internal helper with the same
  signature lives in `derrick-substrate-native` and is
  used only inside the new typed methods.
- `set_ticket_state(id, state, reason)` keeps its public
  surface but is reduced to a **single legal transition**:
  `Ready ↔ InFlight` is **not** done here (use
  `assign_to_hand`/`release_from_hand`); the only
  remaining valid use is the no-op idempotency path
  (current state == target state, returns Ok without a
  write). Every other transition is refused with
  `SubstrateError::Invalid` carrying a pointer to the
  correct typed method:
  - `→ InFlight` → `assign_to_hand`.
  - `→ InReview` → `transition_to_in_review`.
  - `→ Blocked` → `block_ticket`.
  - `→ Done` → `verify_ticket_merged` /
    `mark_ticket_done_manually`.
  - `→ Rejected` → future `reject_ticket` API.
  - `→ Ready` from non-Blocked → `release_from_hand`.
  - `→ Ready` from Blocked → `unblock_ticket`.
  The narrow no-op kept-allowed case exists only so
  callers that don't know the current state can call
  set_ticket_state defensively without a special case.
- `record_event(NewEvent)` is **removed from the public
  trait**. The replacement is `record_typed_event`. The
  legacy method moves to a `pub(crate)` helper inside
  `derrick-substrate-native` used by the new typed
  methods. No deprecation window — there is exactly one
  existing external caller (T010's bridge step), updated
  in this same ticket.

This narrowing is part of the breaking trait change called
out in the Acceptance section. It is the load-bearing fix
that ensures D31 cannot be circumvented in new code.

**Batch auto-close behavior is preserved**, with the
corrected `is_terminal` definition (Done / Rejected only;
**Blocked is not terminal** — it awaits a human decision).
When `verify_ticket_merged` or `mark_ticket_done_manually`
land a ticket in `Done`, or a future `reject_ticket` lands
it in `Rejected`, the implementation checks the ticket's
batch for any non-terminal siblings (including Blocked) and,
if none remain, transitions the batch to `Closed`.
`verify_ticket_unmerged` transitions a ticket to `Blocked`
and therefore does **not** trigger auto-close — the batch
stays open until a human resolves the Blocked ticket.
Batch-close event: `BatchClosed { open_ticket_ids: [] }`.

### Structured event kinds (D31 append-only audit)

`EventKind` gains explicit transition variants so the
current state is reconstructable from the event log
without parsing freeform body text:

```rust
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum EventKind {
    // Lifecycle transitions — each carries the source and
    // destination state explicitly, so the current state
    // is `events.iter().rev().find_map(state_after)` and
    // never needs to consult the tickets row.
    TicketCreated { initial_state: TicketState },
    TicketStateChanged { from: TicketState, to: TicketState, reason: Option<String> },
    TicketTransitionedToInReview {
        // Captures the full InReviewMetadata so the verifier
        // can reconstruct branch/head/PR context from the
        // event log alone (D31's "current state is a
        // projection of events").
        branch: String,
        pr_url: Option<String>,
        pr_number: Option<u64>,
        head_sha: String,
    },                                                  // → InReview
    TicketVerifiedMerged {
        // The head SHA that was on the InReview branch when
        // the verifier observed it.
        head_sha: String,
        // The actual merge commit on the target branch.
        // For fast-forward merges these are equal; for
        // squash/rebase merges merge_sha is distinct (gh's
        // reported merge commit, or the commit found by
        // walking target ancestry).
        merge_sha: String,
    },                                                  // → Done
    TicketVerifiedUnmerged { reason: String },          // → Blocked (D32)
    TicketMarkedDoneManually { claimant: String, note: String }, // → Done (solo mode only)
    TicketAssigned { hand: HandId },
    TicketUnassigned { reason: String },

    // Batch lifecycle (unchanged from T007).
    BatchCreated,
    BatchClosed { open_ticket_ids: Vec<TicketId> },

    // Foreman + hand lifecycle.
    /// Foreman started. `mode` discriminates attached vs
    /// detached; `pid` is the OS pid of the process running
    /// the loop (the daemon child for detached, the
    /// current process for attached).
    ForemanStarted { mode: ForemanMode, pid: u32 },
    /// Foreman stopped cleanly. Written by
    /// `record_foreman_stopped`. Does not carry a pid —
    /// the most recent `ForemanStarted` provides it for
    /// audit reconstruction.
    ForemanStopped,
    HandRegistered,
    HandHeartbeat,
    HandAbandoned { previous_owner_of: TicketId },

    // Worktree lifecycle (D32).
    WorktreeReserved { run_id: String, branch: String },
    WorktreeFinalized { run_id: String },
    WorktreeAbandoned { run_id: String, reason: String },

    // Escalations.
    EscalationStuckInReview { ticket: TicketId, branch: String },
    RestackConflict { ticket: TicketId, recipe: String },

    // Catch-all freeform note for ad-hoc human commentary.
    Note { body: String },
}
```

This replaces T002's looser `EventKind` enum. The change is
**non-breaking at the type level** (`#[non_exhaustive]`) but
existing event-reading code that match'd specific variants
must add arms for the new ones — fine because the only
existing consumer is `derrick-observe` (T015, not yet
shipped).

**Persistence rule (concrete schema + API contract):**

The existing `events(kind TEXT, body TEXT)` columns are
reused. `kind` stores the snake_case discriminator (e.g.
`"ticket_transitioned_to_in_review"`) for indexed queries;
`body` stores the full `EventKind` value as `serde_json`
(payload round-trips via `#[serde(tag = "kind")]`).

**The freeform `record_event(NewEvent { kind, body })` API
is deprecated and made private.** T012 replaces it with a
typed surface on the `Substrate` trait:

```rust
#[async_trait::async_trait]
pub trait Substrate: Send + Sync {
    // ... existing methods ...

    /// The only public path to writing an event. Implementations
    /// serialize `kind` to the discriminator column and the
    /// full `EventKind` to `body` as JSON, guaranteeing
    /// kind/body never diverge.
    async fn record_typed_event(
        &self,
        scope: EventScope,
        kind: EventKind,
    ) -> Result<EventId, SubstrateError>;

    /// Typed event read. Replaces T002's `tail_events`
    /// which returned `Event { kind: String, body:
    /// String }`. Readers get `TypedEvent` with the
    /// deserialised `EventKind` directly.
    async fn tail_typed_events(
        &self,
        since: Option<chrono::DateTime<chrono::Utc>>,
        limit: usize,
    ) -> Result<Vec<TypedEvent>, SubstrateError>;

    /// Per-ticket event history, ordered newest-first.
    /// Used by the verifier to find the most recent
    /// `TicketTransitionedToInReview` event.
    async fn ticket_events(
        &self,
        id: &TicketId,
        limit: usize,
    ) -> Result<Vec<TypedEvent>, SubstrateError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize)]
pub struct EventId(pub i64);

#[derive(Clone, Debug)]
pub enum EventScope {
    Ticket(TicketId),
    Batch(BatchName),
    Hand(HandId),
    Worktree { run_id: String },
    Site,
}

#[derive(Clone, Debug)]
pub struct TypedEvent {
    pub id: EventId,
    pub scope: EventScope,
    pub kind: EventKind,
    pub at: chrono::DateTime<chrono::Utc>,
}
```

The legacy `tail_events(...) -> Vec<Event>` remains as
`#[deprecated]` returning the string-bodied shape for one
release; new code uses `tail_typed_events`. The
`derrick-observe` (T015) and `derrick-cli status`
consumers migrate to `tail_typed_events` in this ticket.

`NewEvent` and `record_event` remain inside the
`derrick-substrate-native` crate as private
implementation details. T007's only external consumer
(the bridge step in `derrick-flow`) migrates to
`record_typed_event` in T012 — small touchup.

Readers deserialise `body` into `EventKind` directly and
ignore the `kind` column except for filter predicates.

**Scope persistence** — the T007 `events` table only has
a single `ticket TEXT NULL` column, which cannot represent
`EventScope::Batch | Hand | Worktree | Site` losslessly.
Migration 0002 adds the columns needed for the full scope
taxonomy:

```sql
-- (Inside the 0002 migration, after the tickets rebuild)
ALTER TABLE events ADD COLUMN scope_kind TEXT NOT NULL
    DEFAULT 'site'
    CHECK (scope_kind IN ('ticket','batch','hand','worktree','site'));
ALTER TABLE events ADD COLUMN scope_batch TEXT NULL
    REFERENCES batches(name);
ALTER TABLE events ADD COLUMN scope_hand TEXT NULL
    REFERENCES hands(id);
ALTER TABLE events ADD COLUMN scope_run_id TEXT NULL;

-- Backfill: existing T007 rows have a non-NULL ticket
-- column when they were ticket-scoped; set scope_kind
-- accordingly. Rows with NULL ticket become site-scope.
UPDATE events SET scope_kind = 'ticket' WHERE ticket IS NOT NULL;

CREATE INDEX idx_events_scope_kind ON events(scope_kind);
CREATE INDEX idx_events_scope_batch ON events(scope_batch);
CREATE INDEX idx_events_scope_hand  ON events(scope_hand);
```

The existing `ticket` column is retained (not renamed) so
T007 readers keep working during the transition. On the
write side, `record_typed_event` populates exactly one of
`{ticket, scope_batch, scope_hand, scope_run_id}` based on
the supplied `EventScope` and sets `scope_kind` to the
discriminator. On the read side, `tail_typed_events`
reconstructs `EventScope` from `(scope_kind, ticket,
scope_batch, scope_hand, scope_run_id)`.

**Legacy compatibility reader.** T007 wrote event `body`
values with per-kind shapes — `ticket_state_changed` as
JSON `{from, to, reason}`, `note` as freeform text, others
typically empty or scalar. The new typed reader handles
this entirely in Rust, not via SQL backfill:

```rust
fn decode_event_body(kind: &str, body: &str) -> Result<EventKind, DecodeError> {
    // Path 1: new-format rows are tagged JSON; the
    //         #[serde(tag = "kind")] discriminator round-
    //         trips cleanly.
    if let Ok(parsed) = serde_json::from_str::<EventKind>(body) {
        return Ok(parsed);
    }
    // Path 2: legacy T007 rows. Per-kind reconstruction.
    match kind {
        "note" => Ok(EventKind::Note { body: body.to_string() }),
        "ticket_state_changed" => {
            // T007 wrote {from, to, reason} as JSON directly,
            // not tagged. Re-shape into the tagged variant.
            let v: serde_json::Value = serde_json::from_str(body)?;
            Ok(EventKind::TicketStateChanged {
                from: serde_json::from_value(v["from"].clone())?,
                to:   serde_json::from_value(v["to"].clone())?,
                reason: v.get("reason").and_then(|r| r.as_str()).map(String::from),
            })
        }
        "ticket_created"   => Ok(EventKind::TicketCreated {
            initial_state: TicketState::Ready,  // T007 only created Ready tickets
        }),
        "ticket_assigned"  => Ok(EventKind::TicketAssigned {
            hand: HandId::new(body)?,  // T007 stored hand id directly in body
        }),
        "ticket_unassigned" => Ok(EventKind::TicketUnassigned {
            reason: body.to_string(),
        }),
        "batch_created"    => Ok(EventKind::BatchCreated),
        "batch_closed"     => Ok(EventKind::BatchClosed { open_ticket_ids: vec![] }),
        "foreman_started"  => Ok(EventKind::ForemanStarted {
            mode: ForemanMode::Detached,  // T007 only had record_foreman_start
            pid: body.parse().unwrap_or(0),
        }),
        "foreman_stopped"  => Ok(EventKind::ForemanStopped),
        "hand_registered"  => Ok(EventKind::HandRegistered),
        "hand_heartbeat"   => Ok(EventKind::HandHeartbeat),
        other => Err(DecodeError::UnknownLegacyKind(other.to_string())),
    }
}
```

Round-trip is one-way: legacy rows decode into typed
variants on read; they're not rewritten on write. New
events serialise to tagged JSON and decode trivially via
Path 1. The compatibility reader's per-variant logic
ships with explicit unit tests (one per legacy kind) so
the migration is testable on a real T007-written DB.

### Schema migration (`crates/derrick-substrate-native`)

New file: `migrations/0002_state_machine_integrity.sql`.

**D31 enforcement is split across two layers in v2**:

- **API layer (immediate)**: the new trait methods are the
  only way to reach `Done` from new code, and
  `set_ticket_state` refuses `Done` as a target.
- **DB-level `state=done → merge_sha IS NOT NULL` CHECK is
  deferred** to migration 0003 (a future ticket) after we
  ship a backfill pass that audits any pre-existing `Done`
  rows from v1. Adding the CHECK now would either reject a
  legitimate v1 DB at migration time (v1 allowed direct
  Done transitions) or force us to invent a fake merge_sha
  for legacy rows.

**Critical FK ordering note.** SQLite's
`PRAGMA foreign_keys = OFF` is silently a no-op *inside* a
transaction (the setting is read at transaction begin), so
the migration runner must toggle FKs **before** opening
the transaction. The migration is split into two phases:

```rust
// Pseudocode in the Rust migration runner:
conn.execute_batch("PRAGMA foreign_keys = OFF;")?;          // outside any txn
let tx = conn.transaction()?;                                // BEGIN IMMEDIATE
tx.execute_batch(include_str!("0002_state_machine_integrity.sql"))?;

// foreign_key_check returns one row per FK violation; it
// does NOT raise an error just because rows exist. We must
// query explicitly and abort if non-empty.
let violations: Vec<(String, i64, String, i64)> = tx
    .prepare("PRAGMA foreign_key_check;")?
    .query_map([], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
    })?
    .collect::<Result<_, _>>()?;
if !violations.is_empty() {
    return Err(SubstrateError::MigrationFkViolations(violations));
}

tx.commit()?;                                                // COMMIT
conn.execute_batch("PRAGMA foreign_keys = ON;")?;           // outside txn again
```

The `.sql` file therefore contains **only** the schema
mutations, no PRAGMA toggles:

```sql
-- 0002: D31/D32 state machine integrity columns.
-- Must be run by a migration runner that has already
-- disabled foreign_keys outside the transaction.

CREATE TABLE tickets_new (
    id TEXT PRIMARY KEY,
    batch TEXT NULL REFERENCES batches(name),
    ordinal INTEGER NULL,
    title TEXT NOT NULL,
    body TEXT NOT NULL DEFAULT '',
    state TEXT NOT NULL,
    owner TEXT NULL REFERENCES hands(id),
    merge_sha TEXT NULL,
    block_reason TEXT NULL,
    block_reason_detail TEXT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    CHECK (state IN ('ready', 'in_flight', 'in_review',
                     'blocked', 'done', 'rejected')),
    CHECK (block_reason IS NULL OR
           block_reason IN ('dependency', 'pr_closed_unmerged',
                            'restack_conflict', 'human')),
    -- block_reason must be set iff state = 'blocked'
    CHECK ((state = 'blocked') = (block_reason IS NOT NULL)),
    CHECK (ordinal IS NULL OR batch IS NOT NULL)
);

INSERT INTO tickets_new
  (id, batch, ordinal, title, body, state, owner,
   merge_sha, block_reason, block_reason_detail,
   created_at, updated_at)
SELECT
  id, batch, ordinal, title, body, state, owner,
  NULL,
  CASE WHEN state = 'blocked' THEN 'human' ELSE NULL END,
  CASE WHEN state = 'blocked'
       THEN json_object('kind','human','note','migrated from v1')
       ELSE NULL END,
  created_at, updated_at
FROM tickets;

DROP TABLE tickets;
ALTER TABLE tickets_new RENAME TO tickets;

CREATE INDEX idx_tickets_state ON tickets(state);
CREATE INDEX idx_tickets_batch_ordinal ON tickets(batch, ordinal);
CREATE INDEX idx_tickets_owner ON tickets(owner);

-- Foreman mode column.
ALTER TABLE foreman ADD COLUMN mode TEXT NOT NULL DEFAULT 'stopped'
  CHECK (mode IN ('stopped', 'detached', 'attached'));

-- Events table: scope taxonomy columns (see "Scope
-- persistence" above).
ALTER TABLE events ADD COLUMN scope_kind TEXT NOT NULL
    DEFAULT 'site'
    CHECK (scope_kind IN ('ticket','batch','hand','worktree','site'));
ALTER TABLE events ADD COLUMN scope_batch TEXT NULL
    REFERENCES batches(name);
ALTER TABLE events ADD COLUMN scope_hand TEXT NULL
    REFERENCES hands(id);
ALTER TABLE events ADD COLUMN scope_run_id TEXT NULL;

UPDATE events SET scope_kind = 'ticket' WHERE ticket IS NOT NULL;

CREATE INDEX idx_events_scope_kind ON events(scope_kind);
CREATE INDEX idx_events_scope_batch ON events(scope_batch);
CREATE INDEX idx_events_scope_hand  ON events(scope_hand);

-- NO event body backfill in SQL. Legacy T007 rows have
-- per-kind body shapes that aren't worth round-tripping
-- through SQLite JSON functions; the new typed reader
-- (see "Legacy compatibility reader" below) handles them
-- in Rust where the per-variant logic is easy to test.

PRAGMA user_version = 2;
```

Because FKs are disabled *before* the transaction starts,
the `DROP TABLE tickets` succeeds even though
`ticket_labels`, `links`, `events.scope_ticket`, and
`owner` columns reference it; rows in those tables are
preserved and re-bind by name when the new `tickets`
table is renamed into place (SQLite's table-rebuild
recipe — see the [official guide](https://sqlite.org/lang_altertable.html#otheralter),
section "Making Other Kinds Of Table Schema Changes",
steps 4–7). The `foreign_key_check` inside the
transaction confirms no FK violation survived the
rebuild; any violation aborts the migration via the
transaction's rollback. FKs are re-enabled outside the
transaction once it commits cleanly.

The native substrate's `open()` checks `user_version`:
- `0` → run 0001 then 0002.
- `1` → run 0002 only.
- `2` → no-op.
- `>2` → refuse with "DB is from a newer derrick".

T012 also adds the migration runner's transactional
wrapper if T007 didn't already provide one. Per the
T007-shipped code, table-rebuild migrations were not
transactionally wrapped; this ticket fixes that as a
prerequisite for shipping 0002 safely.

### Foreman loop (`crates/derrick-substrate-native/src/foreman.rs`)

```rust
//! Foreman loop. See DESIGN.md §8.6.

use crate::NativeSubstrate;
use derrick_substrate::*;

pub struct Foreman {
    substrate: std::sync::Arc<NativeSubstrate>,
    config: derrick_config::Config,
    repo_state: Box<dyn RepoState>,
    repo_root: std::path::PathBuf,
    poll_interval: std::time::Duration,
    in_review_ttl: chrono::Duration,
    hand_ttl: chrono::Duration,
}

impl Foreman {
    pub fn new(
        substrate: std::sync::Arc<NativeSubstrate>,
        config: derrick_config::Config,
        repo_state: Box<dyn RepoState>,
        repo_root: std::path::PathBuf,
    ) -> Self;

    /// Run a single loop iteration. Public so tests can drive
    /// it deterministically without spawning the background
    /// task. Returns a structured TickReport describing what
    /// changed.
    pub async fn tick(&self) -> Result<TickReport, ForemanError>;

    /// Run the loop in foreground until shutdown signal
    /// (SIGTERM/SIGINT) or when no work remains and the
    /// `exit_when_idle` config flag is set. This is the
    /// only execution surface; **the Foreman struct does
    /// not own daemonisation**.
    pub async fn run_attached(&self) -> Result<(), ForemanError>;
}

/// Daemonisation lives at the CLI layer, not in the
/// Foreman struct. The CLI's `derrick foreman start
/// --detached` flow — **the parent owns the foreman row
/// writes; the child only runs the loop**:
///
/// 1. Parent forks (`fork()` on unix; `CreateProcess` with
///    `DETACHED_PROCESS` on windows) a child that re-execs
///    `derrick foreman start --__internal-daemon-child`.
///    The child flag is internal-only and tells the child
///    process to skip the row write in step 3.
/// 2. Parent writes the child's pid to `.derrick/foreman.pid`
///    and calls `substrate.record_foreman_detached(pid)`,
///    which writes both `foreman.mode = 'detached'` and a
///    `ForemanStarted { mode: Detached, pid: child_pid }`
///    event. Parent exits 0.
/// 3. Child redirects stdio to `.derrick/foreman.log`
///    (append) and calls `Foreman::run_attached`, which is
///    the pure loop runtime — it does **not** write to the
///    `foreman` row and does **not** emit a second
///    `ForemanStarted` event. The row stays `detached` for
///    the lifetime of the child process.
/// 4. `derrick foreman stop` reads the pid file, sends
///    SIGTERM, waits up to 5s, then SIGKILL if still alive.
///    The signal handler in the child calls
///    `substrate.record_foreman_stopped()` (writes mode =
///    `stopped`, clears pid column, emits
///    `ForemanStopped`) before exiting. The stop command
///    then removes `.derrick/foreman.pid`.
/// 5. `derrick foreman start --attached` is the
///    foreground-equivalent: same process writes mode =
///    `attached` via `record_foreman_attached(getpid())`,
///    runs the loop, and writes `record_foreman_stopped`
///    on SIGTERM/SIGINT.
///
/// Net rule: **exactly one `ForemanStarted` event per
/// process lifetime**, written by whichever process owns
/// the row (parent for detached, self for attached).
///
/// This separation keeps the Foreman crate free of
/// platform-specific daemonisation code and makes the CLI's
/// process model independently testable.

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
    /// Ticket transitioned from InReview to Blocked (D32 —
    /// closed-unmerged PR).
    Unmerged { ticket: TicketId, reason: String },
    /// Ticket still in flight; verifier emitted an
    /// escalation event but no state change.
    StuckEscalated { ticket: TicketId },
    /// A ticket the substrate said was Ready had a prior
    /// `TransitionedToInReview` event whose `head_sha` is now
    /// on the target branch (e.g. it was re-queued by the
    /// cleanup pass and has since merged externally). D33's
    /// idempotent correction path.
    ReconciledFromGit { ticket: TicketId, merge_sha: String },
}

#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum ForemanError {
    #[error("substrate error: {0}")]
    Substrate(#[from] SubstrateError),
    #[error("repo state check failed: {0}")]
    RepoState(Box<dyn std::error::Error + Send + Sync>),
    #[error("io error at {path}: {source}")]
    Io { path: std::path::PathBuf, source: std::io::Error },
}
```

### `HandDispatcher` trait (in `derrick-substrate-native`)

The dispatch seam T013 will implement against. Frozen in
T012 so T013 doesn't need to reshape it.

```rust
#[async_trait::async_trait]
pub trait HandDispatcher: Send + Sync {
    /// Identifier for telemetry; matches `derrick.yaml`
    /// hand kind (`claude` | `copilot` | `human`).
    fn kind(&self) -> &'static str;

    /// Reserve a hand for this ticket and start the work.
    /// Implementations:
    /// - Register a fresh hand row via
    ///   `substrate.register_hand` and return its id.
    /// - Atomically transition the ticket Ready -> InFlight
    ///   and set owner via
    ///   `substrate.assign_to_hand(ticket, hand)`. Must be
    ///   this typed call, not a separate state-set + assign
    ///   pair, so the dispatch is observable in a single
    ///   transaction.
    /// - Kick off the actual work (spawn a Copilot agent,
    ///   write a TODO for a human, etc.).
    /// The implementation is responsible for the
    /// **eventual** `transition_to_in_review` call when
    /// the hand finishes; the foreman does not poll the
    /// hand's progress directly (T013 wires that up).
    async fn dispatch(
        &self,
        ticket: &Ticket,
        worktree_root: &std::path::Path,
    ) -> Result<DispatchResult, DispatchError>;
}

#[derive(Clone, Debug)]
pub struct DispatchResult {
    pub hand: HandId,
    /// True if the dispatcher synchronously moved the
    /// ticket to InReview (rare; mostly for `human`
    /// hands that complete the work in-process). False
    /// for async dispatchers that hand off and exit.
    pub completed_synchronously: bool,
}

#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum DispatchError {
    #[error("dispatcher kind {kind} not implemented in v1; see T013")]
    NotImplemented { kind: &'static str },
    #[error("substrate error: {0}")]
    Substrate(#[from] SubstrateError),
    #[error("dispatch io: {0}")]
    Io(std::io::Error),
}
```

The `transition_to_in_review` step is the hand's
responsibility, not the foreman's — see the loop
algorithm above. T013's `derrick-copilot` will implement
this trait against the Copilot CLI; T012 ships a
`HumanHandDispatcher` (writes a TODO event, leaves the
ticket InFlight) and a `CopilotStubDispatcher` (returns
`DispatchError::NotImplemented`).

### `RepoState` trait (git + gh, in `derrick-substrate-native`)

Per D33: derrick trusts git, not just PR metadata. The trait
exposes **both** git-log checks and PR-state lookups so the
verifier can confirm a merge against the target branch's
actual commit history rather than relying on `gh pr view`
alone (a PR can report `MERGED` while a force-push or
revert leaves the merge commit no longer on target).

```rust
#[async_trait::async_trait]
pub trait RepoState: Send + Sync {
    /// Is `head_sha` present on the target branch's
    /// ancestry as of now? This is the canonical
    /// "did it merge" check.
    async fn target_contains_sha(
        &self,
        target_branch: &str,
        head_sha: &str,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>>;

    /// PR metadata for cross-reference. Used to detect
    /// closed-unmerged (where target_contains_sha is also
    /// false, but PR state distinguishes "still open" from
    /// "actively rejected").
    async fn pr_status(&self, branch: &str)
        -> Result<PrStatus, Box<dyn std::error::Error + Send + Sync>>;

    /// Merge SHA the PR reports merging with. Only meaningful
    /// when PR is merged according to gh; the verifier still
    /// confirms via target_contains_sha.
    async fn pr_merge_sha(&self, branch: &str)
        -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PrStatus { Open, Merged, ClosedUnmerged, NotFound }

/// Production impl shells to `git log` and `gh pr view`
/// via derrick-tools subprocess pattern.
pub struct GhRepoState { /* opaque */ }
impl GhRepoState { pub fn new() -> Self; }
```

### Loop iteration (concrete; mirrors §8.6 step-for-step)

`tick()` body, sequentially:

1. **Cleanup pass** (D32):
   - Walk `worktrees` rows where `closed_at IS NULL` and a
     finalize event is missing AND `created_at` older than
     `cleanup.worktree_ttl` (default 24h). Prune the
     worktree directory via `git worktree remove --force`,
     delete the row, emit `WorktreeAbandoned` event.
   - Walk `.derrick/.adopt-stage-*` directories older than
     `adopt_stage_ttl` (default 24h). Remove and emit a
     `Note` event (this is T011's TODO that T012 picks up).
   - Walk hands with `last_seen` older than `hand_ttl`
     (default 30 minutes). For each hand owning an
     `InFlight` ticket, call `release_from_hand(ticket,
     "hand abandoned: last seen <ts>")`. The substrate
     does the atomic state-reset + owner-clear in one
     transaction and emits the
     `TicketStateChanged`+`TicketUnassigned` event pair.
     The cleanup pass additionally emits a
     `HandAbandoned { previous_owner_of: ticket }` event
     against the hand scope for telemetry.
   - List tickets in `InReview` with `updated_at` older than
     `in_review_ttl` (default 24h). Add each to the verifier
     pass's eager queue (rechecked this tick rather than
     waiting another poll cycle).

2. **Verifier pass** (D31 + D33, the teeth):
   - For each `InReview` ticket (including eager queue from
     step 1):
     - Read the recorded `InReviewMetadata` (branch, pr_url,
       head_sha) from the most recent
       `TicketTransitionedToInReview` event in the ticket's
       event log.
     - Call `repo_state.target_contains_sha(target_branch,
       head_sha)`. This is the canonical D33 check — git
       history is authoritative for fast-forward / rebase
       merges where the head SHA itself lands on target.
     - If true: resolve the actual merge commit:
       (a) if `pr_url` is set, prefer
       `repo_state.pr_merge_sha(branch)` (gh's recorded
       merge commit); (b) otherwise fall back to `head_sha`
       (fast-forward case). Call
       `verify_ticket_merged(id, head_sha, merge_sha)`.
       Records `VerifierAction::Merged`.
     - If `target_contains_sha` returned false but
       `pr_status` reports `Merged` (squash-merge case where
       the head SHA itself doesn't land on target but a
       merge commit does), fetch `pr_merge_sha(branch)` and
       call `target_contains_sha(target_branch, merge_sha)`
       to confirm. If confirmed, call
       `verify_ticket_merged(id, head_sha, merge_sha)`.
       This covers D21's squash-default repos.
     - If false: call `repo_state.pr_status(branch)`.
       - `ClosedUnmerged` → `verify_ticket_unmerged(id,
         branch, pr_url)`, which atomically sets
         `state = Blocked` and `block_reason =
         BlockReason::PrClosedUnmerged { branch, pr_url }`
         per D32. Records `Unmerged`.
       - `Open` → leave it; recheck next tick.
       - `Merged` reported by gh but `target_contains_sha`
         returned false → log a warning event
         (`EscalationStuckInReview` with the recipe
         *"gh reports PR merged but head SHA not on target
         branch; possible force-push or revert; manual
         triage"*) and leave the ticket in `InReview`. D33
         prefers loud over silent.
       - `NotFound` and past TTL → emit
         `EscalationStuckInReview`, record
         `StuckEscalated`. Otherwise leave alone.

3. **D33 pre-dispatch reconciliation pass**:
   - Scope is **narrow and evidence-bound**: only tickets
     currently `Ready` that *also* have a prior
     `TransitionedToInReview` event in their history. This
     is the re-queue case: a ticket went InReview, the
     cleanup pass or a hand error rolled it back to Ready,
     and the PR has since merged externally.
   - For each such ticket, read the most recent
     `TicketTransitionedToInReview` event's `head_sha`,
     `branch`, and `pr_url`. Run the same two-path merge
     resolution as step 2:
     - **Fast-forward / rebase path:**
       `target_contains_sha(target_branch, head_sha)` true
       → resolve merge_sha (via `pr_merge_sha` if a PR URL
       is on record, else `head_sha`) and call
       `reconcile_ticket_done_from_git(id, head_sha,
       merge_sha)`.
     - **Squash-merge path:** head_sha not on target, but
       `pr_status(branch) == Merged` and
       `pr_merge_sha(branch)` returns Some(M), and
       `target_contains_sha(target_branch, M)` true →
       reconcile with merge_sha = M.
     `reconcile_ticket_done_from_git` is distinct from
     `verify_ticket_merged` (which requires InReview).
     Records `VerifierAction::ReconciledFromGit`. This
     mirrors step 2 exactly so squash-default repos
     behave consistently across both paths (D21 / D33).
   - Tickets with no prior InReview history are **not**
     speculatively reconciled — there is no `expected_head`
     to verify against, and "any commit on target" is too
     weak to justify Done. Externally-completed work that
     never went through the foreman remains the user's
     responsibility to mark via `derrick ticket done` (solo)
     or by re-running the bridge step (crew).

4. **Reconcile Blocked** (dependency path only):
   - Blocked provenance is recorded in a **dedicated
     column** on the ticket row, `block_reason TEXT NULL`,
     populated by every path that transitions to Blocked
     and cleared when the ticket leaves Blocked. The column
     stores a typed `BlockReason` value (see below) as its
     snake_case discriminator — `dependency`,
     `pr_closed_unmerged`, `restack_conflict`, or `human` —
     plus a JSON detail blob in a sibling
     `block_reason_detail TEXT NULL` column.
   - For each ticket with `state = blocked AND block_reason
     = 'dependency'`, re-check its `blocks`-link
     predecessors. If every predecessor is terminal
     (Done/Rejected; Blocked does NOT count), transition to
     `Ready` via the typed `unblock_ticket(id)` substrate
     method (atomic state-set + block_reason clear +
     `TicketStateChanged` event). Records `unblocked`.
   - Tickets with `block_reason IN
     ('pr_closed_unmerged','restack_conflict','human')`
     **never auto-unblock**; the user acts via
     `derrick ticket reopen <id> --note <text>`
     (implemented in T012) or `derrick ticket reject <id>`
     (T012-stub — rejection workflow lands in a follow-up
     ticket).

5. **Dispatch ready tickets**:
   - Up to `parallelism.batch_max` concurrent in-flight
     hands. Sort by ordinal (within batch) then created_at.
   - Dispatch goes through `HandDispatcher::dispatch`,
     returning a `DispatchResult` containing the assigned
     hand id. The hand is responsible for opening the PR
     and calling `transition_to_in_review` with full
     `InReviewMetadata` when work is ready — the foreman
     does **not** transition InFlight → InReview itself.
   - T012 ships `HumanHandDispatcher` (writes a TODO event,
     leaves the ticket InFlight until the user runs
     `derrick ticket review`) and `CopilotStubDispatcher`
     (returns `DispatchError::NotImplemented { kind:
     "copilot" }`). A `ClaudeHandDispatcher` is **not**
     shipped in T012 — it lands in a follow-up alongside
     the Claude Code SDK adapter. Dispatch for ticket
     `kind: claude` errors with the same NotImplemented
     pointer until then.

6. **Sleep** `poll_interval` (default 10s; configurable via
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

This requires a real config-surface change to T001's
`ToolsLayer`: add a `foreman: Option<ToolsForemanLayer>`
field with the four duration fields above, each
`#[serde(with = "humantime_serde")]` and defaulted via the
existing layer-merge pattern. T012 extends T001's config
crate accordingly — no breaking change to existing
`tools:` consumers because the block is optional.

### CLI wiring

T012 adds two subcommand groups to `derrick-cli`.

**`derrick ticket` subcommands** (new top-level group; the
substrate-facing mutation API for §8.2):

- `derrick ticket done <id>` — calls
  `mark_ticket_done_manually` after reading
  `derrick.yaml.mode`. If mode is not `solo`, exits 2 with
  a D31 pointer ("Done is reached via the foreman's
  verifier in crew/copilot modes; see DESIGN.md §8.6.
  Use `derrick ticket review` to mark work ready for the
  verifier."). In solo mode, prompts for `--note <text>`
  if not supplied and uses the local user's git config
  name as the claimant.
- `derrick ticket review <id> --branch <name>
   [--pr-url <url>] --head-sha <sha>` — the crew-mode
  equivalent of `done`: the human hand declares "PR is
  open (or branch is pushed), here is the metadata the
  foreman needs to verify." Calls
  `transition_to_in_review` with the supplied
  `InReviewMetadata`. If `--pr-url` is omitted but a
  matching open PR exists for the branch on the remote,
  the CLI queries `gh pr view <branch>` and fills it in.
  This is the documented path for human hands in crew
  mode; without it the foreman has no metadata to verify
  against.
- `derrick ticket list` and `derrick ticket show <id>` —
  read-only convenience that wraps T007's substrate
  reads. Included here so the new `ticket` command group
  is self-consistent; not load-bearing for D31.
- `derrick ticket reject <id> --reason <text>` —
  **stub only in T012**: the command shell exists, parses
  args, exits 2 with "rejection workflow is implemented
  in a follow-up ticket; see TODO". No substrate
  `reject_ticket` method is added in T012. Reserved so
  the namespace is set and tests can assert the shell
  exists.
- `derrick ticket reopen <id> --note <text>` — human
  recovery path for Blocked tickets, **fully implemented
  in T012**. Calls the new `human_reopen_blocked(id,
  note)` substrate method (below), which works for any
  `BlockReason` flavour: clears `block_reason`,
  transitions Blocked → Ready atomically, emits a
  `TicketStateChanged { from: Blocked, to: Ready, reason:
  Some("human reopened: <note>") }` event. Refuses if the
  ticket is not currently Blocked.
- `derrick ticket block <id> [--on <predecessor>] [--note <text>]` —
  human blocking command (DESIGN.md §8.2). Two-phase per
  the link/state split:
  - If `--on <p>` is supplied:
    1. Write a `blocks` link `(this) blocked_by (p)` via
       T007's `add_link` API (idempotent on duplicate
       links).
    2. **Only if** the predecessor is currently
       non-terminal, call `block_ticket(id,
       BlockReason::Dependency { predecessor: p })`. If
       the predecessor is already Done/Rejected, the link
       is recorded but the ticket stays in its current
       state (the dependency is a no-op).
  - If `--note <text>` is supplied (without `--on`):
    call `block_ticket(id, BlockReason::Human { note })`.
  - At least one of `--on` or `--note` is required.
  - Refuses if the ticket is already terminal. If the
    ticket is already Blocked with a different
    `block_reason`, refuses with a pointer to `derrick
    ticket reopen` (block-reason changes go through
    reopen-then-block).

**`derrick foreman` subcommands** (the loop runtime):

- `derrick foreman start [--attached | --detached]` — starts
  the loop. Default detached; writes pid to
  `.derrick/foreman.pid`. Returns immediately for detached;
  blocks for attached.
- `derrick foreman stop` — sends shutdown signal to the
  pid, awaits exit, removes pid file.
- `derrick foreman tick` — runs a single iteration in
  foreground. Useful for tests and `derrick status` cron
  setups.

`derrick run add-feature` in `mode: crew` now **automatically
starts the foreman after the `bridge` step** per DESIGN.md
§8.2 / D25 (detached by default, attached if `--attach` is
passed). `--no-foreman` opts out for the rare debugging
case. The flow doesn't dispatch hands itself — the started
foreman picks up the freshly bridged tickets on its first
tick.

In `mode: solo` the foreman is not started; the user works
from `tasks.md` directly.

In **`mode: copilot`** the foreman is **not** started per
DESIGN.md §8.3 — `derrick-flow`'s pipeline ends after the
bridge step writes tickets, and dispatch happens inline
inside `derrick-flow` via the Copilot adapter (T013).
Polling for completions also runs inline. The state-
integrity guarantees of D31/D32/D33 still apply: the
inline dispatcher MUST use the same typed substrate API
(`assign_to_hand`, `transition_to_in_review`,
`verify_ticket_merged`) as the foreman would. T012 does
not add a copilot-mode pipeline runner — that's T013's
ticket. Until T013 lands, `mode: copilot` runs error out
in `derrick-flow` with a clear T013 pointer (the existing
T009-shipped behavior).

Net: T012 owns the foreman loop (crew mode only). Copilot
mode never reaches T012's code.

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

Real SQLite via tempfile. Mock `RepoState` and mock
`HandDispatcher` impls inline.

**Trait extension:**

- `set_ticket_state_done_refused_with_d31_message`.
- `set_ticket_state_rejected_refused_with_d31_message`.
- `set_ticket_state_in_review_refused_pointing_at_transition_to_in_review`.
- `verify_ticket_merged_transitions_in_review_to_done`.
- `verify_ticket_merged_refuses_when_not_in_review`.
- `cli_ticket_done_refuses_in_crew_mode` — CLI-layer mode
  guard: with `derrick.yaml.mode: crew`, the `derrick
  ticket done <id>` command exits non-zero with a D31
  pointer; substrate is not consulted.
- `cli_ticket_done_succeeds_in_solo_mode` — with
  `mode: solo`, the command writes the
  `TicketMarkedDoneManually` event and transitions to Done.
- `mark_ticket_done_manually_records_attestation_in_event`.
- `mark_ticket_done_manually_refuses_when_already_terminal`.
- `transition_to_in_review_records_metadata_in_event` —
  branch + pr_url + pr_number + head_sha all preserved in
  the event payload (round-trips through `body` JSON) so
  the verifier can read them back.
- `event_log_reconstructs_current_state_from_kinds` —
  walk the event stream backwards, assert the last
  state-change kind matches the row's current state.
- `events_body_round_trips_through_serde_json` — write
  one of each new `EventKind` variant; read back; assert
  byte-equality after re-serialisation.
- `verify_ticket_merged_stores_distinct_head_and_merge_shas` —
  pass differing `head_sha` and `merge_sha` (squash-merge
  case); assert ticket row carries `merge_sha`, event
  carries both.
- `assign_to_hand_is_atomic_state_and_owner` — Ready→
  InFlight + owner set in one transaction; assert both
  events present and ordered.
- `assign_to_hand_refuses_when_not_ready` — refuses if
  ticket is already InFlight, InReview, etc.
- `release_from_hand_is_atomic_state_and_owner` — any
  non-terminal → Ready + owner cleared in one txn.
- `release_from_hand_refuses_on_terminal` — Done/Rejected
  refused.
- `reconcile_ticket_done_from_git_requires_prior_inreview_event` —
  ticket is Ready with no prior InReview history → refuses
  with D33 pointer.
- `reconcile_ticket_done_from_git_accepts_ready_with_history` —
  ticket re-queued from InReview, prior event exists →
  transitions to Done.
- `tail_typed_events_returns_deserialised_kinds` — write
  one of each variant; read back; assert structural
  equality.
- `ticket_events_returns_history_newest_first`.

**Migration:**

- `migration_0002_upgrades_v1_db_in_place` — populate a
  T007 v1 DB with mixed ticket states, open it, assert
  PRAGMA user_version == 2 afterwards and all data
  preserved including state values.
- `migration_0002_preserves_legacy_done_tickets` —
  populate v1 DB with a `Done` ticket (no merge_sha — v1
  allowed this); migration succeeds; the row survives
  with `merge_sha IS NULL`. Documents the deferred
  CHECK that migration 0003 will introduce after a
  backfill.
- `migration_0002_idempotent_on_v2_db`.
- `migration_refuses_v3_db` — refuses with the clear error.
- `migration_0002_rolls_back_on_mid_rebuild_crash` —
  inject a panic between the INSERT and the DROP; assert
  the DB is still openable as v1 and the data is intact.

**Verifier (D31 + D33):**

- `verifier_marks_merged_via_target_contains_sha` —
  mock `RepoState::target_contains_sha` returns true,
  `pr_merge_sha` returns Some(M); assert transition to
  Done with `merge_sha = M` and event carrying both
  head_sha and merge_sha.
- `verifier_handles_squash_merge` — `target_contains_sha`
  on head returns false, `pr_status` returns `Merged`,
  `pr_merge_sha` returns Some(M), `target_contains_sha`
  on M returns true; assert transition to Done with
  merge_sha = M.
- `verifier_marks_blocked_when_pr_closed_unmerged` —
  asserts D32 behavior: closed-unmerged → Blocked with
  `block_reason = PrClosedUnmerged`, not Rejected.
- `cleanup_does_not_unblock_pr_closed_unmerged_ticket` —
  ticket sits in Blocked with `block_reason =
  PrClosedUnmerged` and has no `blocks`-link
  predecessors; assert the cleanup pass leaves it
  Blocked (the vacuous-predecessor trap).
- `cleanup_unblocks_only_dependency_blocked_tickets` —
  one ticket Blocked with `Dependency` (predecessor now
  Done), one Blocked with `PrClosedUnmerged`; assert
  only the first transitions to Ready.
- `block_reason_check_constraint_enforced` — direct
  SQL probe asserts the migration's `(state = 'blocked')
  = (block_reason IS NOT NULL)` CHECK rejects an
  inconsistent insert.
- `human_reopen_blocked_works_for_pr_closed_unmerged` —
  reopen path resolves a verifier-blocked ticket back to
  Ready with a captured note.
- `human_reopen_blocked_refuses_when_not_blocked`.
- `cli_ticket_block_writes_link_and_blocks_when_predecessor_open` —
  end-to-end CLI test.
- `cli_ticket_block_writes_link_only_when_predecessor_terminal` —
  asserts the link is recorded but no state change happens
  if predecessor is already Done.
- `cli_ticket_reopen_transitions_pr_closed_unmerged_to_ready`.
- `verifier_escalates_when_gh_merged_but_target_lacks_sha` —
  the D33 force-push/revert case; assert escalation event
  + ticket stays InReview.
- `verifier_escalates_stuck_in_review_past_ttl`.
- `verifier_leaves_pr_open_tickets_alone`.
- `pre_dispatch_reconciliation_done_for_requeued_ready_ticket` —
  D33's narrow case: ticket has a prior
  `TransitionedToInReview` event with `head_sha = X`; mock
  `target_contains_sha` returns true for X; assert
  transition to Done with merge_sha = X, no dispatch.
- `pre_dispatch_reconciliation_skips_ready_ticket_with_no_inreview_history` —
  asserts the narrow scope: a Ready ticket that has never
  been InReview is dispatched normally; no speculative
  reconciliation.
- `verify_ticket_unmerged_transitions_in_review_to_blocked` —
  trait test asserting the D32 destination state.

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

**CLI daemonisation (process model):**

- `cli_foreman_start_detached_writes_pid_and_exits` —
  invoke `derrick foreman start --detached`; assert the
  parent process exits 0 quickly, `.derrick/foreman.pid`
  exists, and the recorded pid points to a live process.
- `cli_foreman_stop_signals_and_cleans_pid` — start
  detached, then `stop`; assert SIGTERM is delivered, the
  process exits within 5s, and the pid file is removed.
- `cli_foreman_start_attached_runs_in_foreground` — assert
  the command blocks until SIGINT/SIGTERM.
- Daemonisation tests gate on `cfg(unix)` for the
  fork-based path; windows path is left as a follow-up.

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

## D31/D32/D33 compliance checklist

**T012 guarantees (must all be true to ship):**

- [ ] Done state from new code transitions ONLY via
      `verify_ticket_merged` (D31) or
      `mark_ticket_done_manually` (solo mode only).
- [ ] `set_ticket_state(_, Done | Rejected | InReview, _)` is
      rejected at the API; no path to those states bypasses
      the typed methods.
- [ ] Verifier consults `target_contains_sha` (git log) as
      the authoritative D33 signal, with `pr_status` only as
      a cross-reference for the closed-unmerged case.
- [ ] Cleanup pass walks abandoned worktrees +
      `.derrick/.adopt-stage-*` + dead hands + stale
      InReview on every tick (D32).
- [ ] Append-only events for every state transition via
      structured `EventKind` variants — current state is
      reconstructable from the event log alone (D31).
- [ ] Closed-unmerged PR transitions ticket to **Blocked**
      (D32), not Rejected.

**Deferred to future tickets (documented, not blocking
T012):**

- [ ] DB-level `state=done → merge_sha IS NOT NULL` CHECK
      ships in migration 0003 after a backfill pass audits
      legacy v1 Done rows. T012 enforces this at the API
      layer only.

## Acceptance

- [ ] `cargo check -p derrick-substrate-native` passes.
- [ ] `cargo check -p derrick-substrate` passes (trait
      extension is **breaking**: adds required methods and
      narrows `set_ticket_state` semantics — only impl today
      is `derrick-substrate-native`, updated in the same
      ticket).
- [ ] `cargo check -p derrick-config` passes (adds
      `tools.foreman` layer).
- [ ] `cargo check -p derrick-flow` passes (migrates bridge
      step from `record_event` → `record_typed_event` and
      from `set_ticket_state(_, InFlight | InReview, _)` →
      typed methods).
- [ ] `cargo check -p derrick-cli` passes (new `ticket`
      command group + `foreman` subcommands).
- [ ] `cargo test -p derrick-cli` passes (CLI mode-guard
      tests for `ticket done` solo vs crew).
- [ ] `cargo test -p derrick-flow` passes (bridge step
      regression after typed-API migration).
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] `cargo test -p derrick-substrate-native` passes; 3×
      stress green.
- [ ] `cargo test -p derrick-substrate` passes (existing
      tests updated for the breaking trait change).
- [ ] `cargo llvm-cov -p derrick-substrate-native --fail-under-lines 85`.
- [ ] Workspace `cargo llvm-cov --fail-under-lines 80` passes.
- [ ] No `unwrap`/`expect`/`panic` in non-test code.
- [ ] No gastown vocabulary.
- [ ] D31/D32/D33 compliance checklist (above) all boxes
      checked, demonstrated by named tests.

## Reviewer notes (Codex)

Pre-implementation review. Focus on:
- Is the breaking trait change actionable for existing
  consumers? (T010's derrick-flow uses `set_ticket_state`
  for some transitions; calls to set state to Done/Rejected/
  InReview must migrate to the typed methods in this same
  ticket.)
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

Mock the `RepoState` and `HandDispatcher` in tests via
small inline impls; no need for `mockall` or similar.

D32's `.derrick/.adopt-stage-*` cleanup pass is owed to
T012 per T011's TODO. Extend the cleanup step 1 to also
walk those dirs with the same TTL behavior.
