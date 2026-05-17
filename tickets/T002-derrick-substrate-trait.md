# T002 — `derrick-substrate` trait surface

**Specialist owner**: `substrate-engineer` (opus, per AGENTS.md routing)
**Crate**: `crates/derrick-substrate` (trait crate; native impl is T003+)
**Depends on**: `derrick-config` (T001 — re-exports `Site`). *Do not* depend on `derrick-substrate-native` (it'll depend on this).
**Priority**: P0 — every downstream crate programs against this trait.

## Why

DESIGN.md §8.1 describes the substrate model — *Site*, *Ticket*,
*Batch*, *Link*, *Hand*, *Foreman* — and §8 commits to one trait
with multiple potential backends (native SQLite for v1, room for
others later). This ticket defines the trait surface plus the
supporting types and error enum. **The trait crate is contract
only — no I/O, no SQLite, no async runtime spin-up.** The native
implementation, foreman loop, and worktree integration ship in
T003 (`derrick-substrate-native`).

## Scope

### Public types

All in `crates/derrick-substrate/src/types.rs` (or split into
`types/` if it gets unwieldy):

- `TicketId(String)` — newtype, `Display`/`FromStr`. Validation
  in the type's constructor: matches `^[a-z]{1,6}-\d+$` (prefix
  then number; the prefix half mirrors `Site.prefix` from
  derrick-config).
- `BatchName(String)` — newtype, `Display`/`FromStr`. Validation:
  matches `^[a-z0-9][a-z0-9-]{0,63}$` (kebab-case, ≤64 chars).
- `Site` — **re-exported from `derrick-config`**
  (`pub use derrick_config::Site;`). The substrate crate depends
  on `derrick-config` for this one type; we do not duplicate it.
  Adds a hard dependency edge `derrick-substrate -> derrick-config`
  which is fine layering-wise (config is foundational; substrate is
  one layer up). This sidesteps any "keep in sync" risk.
- `Ticket` — id, batch (`Option<BatchName>`), **ordinal**
  (`Option<u32>` — only meaningful when batch is `Some`; the
  substrate stores tickets in a batch sorted by this), title
  (≤200 chars enforced at `NewTicket` construction), body
  (string), state (enum), labels (`Vec<String>`), owner
  (`Option<HandId>`), created_at + updated_at
  (`chrono::DateTime<chrono::Utc>`).
- `TicketState` — enum: `Ready | InFlight | Blocked | Done | Rejected`.
  `Display`, `FromStr`, `serde` impls.
- `Batch` — name (BatchName), created_at, closed_at (Option).
  *Does not* embed tickets; tickets reference their batch by
  name. Listing tickets in a batch is a `Substrate` method
  (returns tickets ordered by their `ordinal`, ascending; ties
  broken by `created_at`).

#### Batch close semantics

Auto-close is the primary path:

- When `set_ticket_state` transitions a ticket so that *all*
  member tickets of a batch are in terminal states (`Done` or
  `Rejected`), the substrate auto-closes the batch: sets
  `closed_at`, emits a `BatchClosed` event, and returns the
  affected batch in the event body.
- `close_batch(name)` is a force-close escape hatch (e.g. for
  the user closing a batch with one stubbornly `Blocked`
  ticket). The trait method must explicitly document this. The
  native impl emits the `BatchClosed` event with a `body`
  field naming "force-close" so the activity log distinguishes
  the two paths.
- A closed batch is **final** in v1. `create_ticket` with a
  batch that is closed returns `SubstrateError::Conflict`.
  Reopening a closed batch is out of scope; if a future
  feature needs it, file a `design-question` issue.
- `close_batch(name)` on an *already-closed* batch is
  **idempotent**: returns `Ok(batch)` with the existing
  `closed_at` unchanged. No second `BatchClosed` event is
  emitted.
- Force-closing a batch that still has non-terminal tickets
  does **not** change those tickets' state. They remain in
  whatever state they were (`Ready` / `InFlight` / `Blocked`);
  no implicit transition to `Rejected`. The activity log
  records a `BatchClosed` event whose body field lists the
  ids of the still-open tickets so the user can triage them.
- `Link` — `{ from: TicketId, to: TicketId, kind: LinkKind }`.
- `LinkKind` — enum: `Blocks | Related`. Serde + Display + FromStr.
- `HandId(String)` — newtype, matches `^[a-z][a-z0-9-]{0,63}$`
  (must start with a lowercase letter, then kebab letters/digits,
  ≤64 chars total). `Display`/`FromStr`/serde.
- `HandKind` — enum: `Claude | Copilot | Human`. Serde + Display
  + FromStr.
- `Hand` — `{ id: HandId, kind: HandKind, last_seen: Option<DateTime<Utc>> }`.
- `Event` — `{ id: uuid::Uuid, at: DateTime<Utc>, kind: EventKind,
  ticket: Option<TicketId>, body: String }`.
- `EventKind` — enum at minimum: `TicketCreated | TicketStateChanged
  | TicketAssigned | BatchCreated | BatchClosed | ForemanStarted |
  ForemanStopped | RestackConflict | Note`. Plus `Display`/serde.
- `ForemanStatus` — `{ pid: Option<u32>, started_at:
  Option<DateTime<Utc>>, mode: ForemanMode }`.
- `ForemanMode` — enum: `Detached | Attached | Stopped`.

All types are `#[non_exhaustive]` on their public enum forms so
future variants don't break downstream crates.

### The `Substrate` trait

In `crates/derrick-substrate/src/lib.rs`:

```rust
#[async_trait::async_trait]
pub trait Substrate: Send + Sync {
    // --- Site ---
    async fn site(&self) -> Result<Site, SubstrateError>;

    // --- Tickets ---
    async fn create_ticket(&self, ticket: NewTicket) -> Result<Ticket, SubstrateError>;
    async fn get_ticket(&self, id: &TicketId) -> Result<Option<Ticket>, SubstrateError>;
    async fn list_tickets(&self, filter: TicketFilter) -> Result<Vec<Ticket>, SubstrateError>;
    async fn set_ticket_state(
        &self,
        id: &TicketId,
        state: TicketState,
        reason: Option<String>,
    ) -> Result<Ticket, SubstrateError>;
    async fn assign_ticket(
        &self,
        id: &TicketId,
        owner: Option<HandId>,
    ) -> Result<Ticket, SubstrateError>;
    async fn add_label(&self, id: &TicketId, label: &str) -> Result<(), SubstrateError>;
    async fn remove_label(&self, id: &TicketId, label: &str) -> Result<(), SubstrateError>;

    // --- Links ---
    async fn link(
        &self,
        from: &TicketId,
        to: &TicketId,
        kind: LinkKind,
    ) -> Result<(), SubstrateError>;
    async fn unlink(
        &self,
        from: &TicketId,
        to: &TicketId,
        kind: LinkKind,
    ) -> Result<(), SubstrateError>;
    async fn outgoing_links(&self, id: &TicketId) -> Result<Vec<Link>, SubstrateError>;
    async fn incoming_links(&self, id: &TicketId) -> Result<Vec<Link>, SubstrateError>;

    // --- Batches ---
    async fn create_batch(&self, name: BatchName) -> Result<Batch, SubstrateError>;
    async fn get_batch(&self, name: &BatchName) -> Result<Option<Batch>, SubstrateError>;
    async fn list_batches(&self, include_closed: bool) -> Result<Vec<Batch>, SubstrateError>;
    /// Force-close a batch (escape hatch — e.g. closing past a stubbornly
    /// `Blocked` ticket). Native impls also auto-close batches when all
    /// member tickets reach a terminal state (see "Batch close semantics"
    /// above); that path does not go through this method.
    async fn close_batch(&self, name: &BatchName) -> Result<Batch, SubstrateError>;
    /// List tickets in a batch, ordered by `ordinal` ascending, ties
    /// broken by `created_at` ascending.
    async fn tickets_in_batch(&self, name: &BatchName) -> Result<Vec<Ticket>, SubstrateError>;

    // --- Hands ---
    async fn register_hand(&self, hand: Hand) -> Result<(), SubstrateError>;
    async fn list_hands(&self) -> Result<Vec<Hand>, SubstrateError>;
    async fn heartbeat(&self, id: &HandId) -> Result<(), SubstrateError>;

    // --- Events / activity ---
    async fn record_event(&self, event: NewEvent) -> Result<Event, SubstrateError>;
    async fn tail_events(&self, since: Option<DateTime<Utc>>, limit: usize)
        -> Result<Vec<Event>, SubstrateError>;

    // --- Foreman ---
    async fn foreman_status(&self) -> Result<ForemanStatus, SubstrateError>;
    async fn record_foreman_start(&self, pid: u32) -> Result<(), SubstrateError>;
    async fn record_foreman_stop(&self) -> Result<(), SubstrateError>;
}
```

Plus the supporting input types:

```rust
pub struct NewTicket {
    pub id: TicketId,            // caller-supplied; impl enforces uniqueness
    pub batch: Option<BatchName>,
    pub ordinal: Option<u32>,    // batch ordering; None == "end of batch"
    pub title: String,           // <=200 chars; constructor returns Invalid otherwise
    pub body: String,
    pub labels: Vec<String>,
}

impl NewTicket {
    /// Constructor. Validates `title.chars().count() <= 200` and that
    /// `ordinal` is only `Some` when `batch` is also `Some`. Returns
    /// `SubstrateError::Invalid` on violation.
    pub fn new(/* ... */) -> Result<Self, SubstrateError> { /* ... */ }
}

pub struct NewEvent {
    pub kind: EventKind,
    pub ticket: Option<TicketId>,
    pub body: String,
}

pub struct TicketFilter {
    pub state: Option<TicketState>,
    pub batch: Option<BatchName>,
    pub owner: Option<HandId>,
    pub label: Option<String>,
    pub limit: Option<std::num::NonZeroUsize>,  // None == unlimited
}
```

`TicketFilter::default()` returns "no filters, limit
`NonZeroUsize::new(100)`". Using `Option<NonZeroUsize>` makes
the "no limit" vs "limit N" distinction unambiguous and removes
the `0` edge case entirely.

### Error type

```rust
#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum SubstrateError {
    #[error("not found: {kind} {id}")]
    NotFound { kind: &'static str, id: String },

    #[error("conflict: {message}")]
    Conflict { message: String },

    #[error("invalid input: {field}: {message}")]
    Invalid { field: String, message: String },

    #[error("backend error: {0}")]
    Backend(Box<dyn std::error::Error + Send + Sync>),
}
```

Backends wrap their native errors (rusqlite, io, etc.) in
`SubstrateError::Backend`. The trait crate does not depend on
`rusqlite`.

### Dependencies (workspace.dependencies only)

```toml
[dependencies]
derrick-config = { path = "../derrick-config" }
async-trait = { workspace = true }    # adds to workspace.dependencies
serde = { workspace = true }
thiserror = { workspace = true }
chrono = { workspace = true }
uuid = { workspace = true }

[dev-dependencies]
tokio = { workspace = true, features = ["macros", "rt"] }
serde_json = { workspace = true }
```

Add `async-trait = "0.1"` to workspace.dependencies in the root
Cargo.toml.

### Tests (all in this crate, all unit tests on the types)

Per AGENTS.md house rule 5, the substrate proper uses real SQLite —
but *this* crate has no SQLite. Tests exercise types and validators:

- `ticket_id_accepts_valid_form` — `mp-1`, `xyz-42`, `a-1` all parse.
- `ticket_id_rejects_invalid_form` — `mp1`, `MP-1`, `mp-`, `mp-x`,
  empty all fail.
- `batch_name_accepts_valid` — `001-webhook`, `a`.
- `batch_name_rejects_invalid` — leading `-`, uppercase, too long.
- `hand_id_accepts_valid` — `bramble`, `polecat-1`, `sumac-7`.
- `hand_id_rejects_invalid` — `1-leading-digit`, `-leading-dash`,
  uppercase, empty, too long, contains `_`.
- `new_ticket_rejects_title_over_200_chars` — exactly 201
  characters fails; 200 succeeds; uses `chars().count()` so a
  multi-byte char counts as 1.
- `new_ticket_rejects_ordinal_without_batch` — `ordinal: Some,
  batch: None` returns `SubstrateError::Invalid`.
- `new_ticket_accepts_ordinal_with_batch` — happy path.
- Round-trip serde for every enum (`TicketState`, `LinkKind`,
  `HandKind`, `EventKind`, `ForemanMode`).
- Display/FromStr round-trip for every type with both impls.
- `ticket_filter_default_has_limit_100` — limit is
  `Some(NonZeroUsize::new(100).unwrap())`; no other field is set.
- `ticket_filter_unlimited` — when limit is `None`, the filter
  encodes "no cap"; verify via destructuring, not by running it.
- `site_reexport_is_derrick_config_site` — compile-time
  assertion via `let _: derrick_config::Site = some_substrate::Site::new(...)?;`
  (the substrate's `Site` is literally the config's `Site`).
- `non_exhaustive_compiles_at_match_site` — a trivial `match`
  statement with a catch-all on each enum (compile-time assertion).

**Coverage target**: 80% line coverage on this crate. It's mostly
types and a few validators; that's achievable without contortion.

## Out of scope

- Any SQLite, any rusqlite. That's T003 (`derrick-substrate-native`).
- The foreman loop body. T003.
- Worktree integration. T003.
- Conformance tests against an in-memory impl. The trait is
  exercised against the native impl in T003's tests.
- Migrations. The schema lives in T003.

## Acceptance

- [ ] `cargo check -p derrick-substrate` passes.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes.
- [ ] `cargo test -p derrick-substrate` passes; all tests above present.
- [ ] Workspace `cargo llvm-cov --fail-under-lines 80` passes
      (the new crate's tests must keep workspace ≥80%; current
      workspace is 92.88% so headroom is comfortable).
- [ ] Every public type and method is documented (workspace lint
      `missing_docs = warn`).
- [ ] No `unwrap`/`expect`/`panic` in non-test code.
- [ ] Stress-run tests 3× consecutively at default `--test-threads`;
      all green (per test-engineer working agreement after T001).

## Reviewer notes (Codex)

This is a **pre-implementation** ticket review. The crate body
currently contains only `//! crate docstring`. Do not nitpick the
absence of implementation; review the spec for whether it's
implementable, internally consistent, and faithful to DESIGN.md
§8.1.

Cite DESIGN.md sections where contradictions exist. Verdict in
the usual format: top risks / contradictions / suggested
revisions / `accept | revise | reject`.

## Implementer notes (Copilot)

Stay in `crates/derrick-substrate/`. Add `async-trait` to
`workspace.dependencies` in the root `Cargo.toml` and reference
it via `workspace = true` in the crate's `Cargo.toml`. No other
top-level dep additions.
