//! Contract types and trait for derrick's execution substrate.
//!
//! This crate defines the storage boundary used by downstream crates. It does
//! not provide a native backend, open SQLite, spawn the foreman, or perform any
//! I/O; those responsibilities live in `derrick-substrate-native`.

mod types;

use chrono::{DateTime, Utc};

pub use derrick_config::Site;
pub use types::{
    Batch, BatchName, BlockReason, Complexity, Event, EventId, EventKind, EventScope, ForemanMode,
    ForemanStatus, Hand, HandExitStats, HandId, HandKind, InReviewMetadata, Link, LinkKind,
    ManualDoneAttestation, NewEvent, NewTicket, SubstrateError, Ticket, TicketFilter, TicketId,
    TicketState, TypedEvent, ticket_id_pattern,
};

/// Storage contract implemented by derrick substrate backends.
#[async_trait::async_trait]
pub trait Substrate: Send + Sync {
    /// Returns the site registered in this substrate.
    async fn site(&self) -> Result<Site, SubstrateError>;

    /// Creates a ticket, enforcing backend uniqueness and batch constraints.
    async fn create_ticket(&self, ticket: NewTicket) -> Result<Ticket, SubstrateError>;

    /// Returns one ticket by id, or `None` when it does not exist.
    async fn get_ticket(&self, id: &TicketId) -> Result<Option<Ticket>, SubstrateError>;

    /// Lists tickets matching `filter`.
    async fn list_tickets(&self, filter: TicketFilter) -> Result<Vec<Ticket>, SubstrateError>;

    /// Permanently removes a ticket and its associated labels and events.
    ///
    /// Bridge uses this when re-dispatching a feature whose prior run left
    /// terminal tickets in the substrate. Only terminal tickets (`done` /
    /// `rejected`) may be deleted; active tickets must be handled via the
    /// typed state transitions. Returns `Ok(())` when the ticket did not
    /// exist (idempotent).
    async fn delete_ticket(&self, id: &TicketId) -> Result<(), SubstrateError>;

    /// **Narrowed in T012**: this method is reduced to the no-op idempotency
    /// path only (current state == target state, returns `Ok` without a
    /// write). Every other transition has a dedicated typed method:
    ///
    /// - `→ InFlight` → [`Substrate::assign_to_hand`]
    /// - `→ InReview` → [`Substrate::transition_to_in_review`]
    /// - `→ Blocked` → [`Substrate::block_ticket`]
    /// - `→ Done` → [`Substrate::verify_ticket_merged`] /
    ///   [`Substrate::mark_ticket_done_manually`]
    /// - `→ Rejected` → [`Substrate::reject_ticket`]
    /// - `→ Ready` from non-`Blocked` → [`Substrate::release_from_hand`]
    /// - `→ Ready` from `Blocked` → [`Substrate::unblock_ticket`]
    ///
    /// Implementations MUST refuse every non-no-op call with
    /// `SubstrateError::Invalid` carrying a pointer to the correct typed
    /// method.
    async fn set_ticket_state(
        &self,
        id: &TicketId,
        state: TicketState,
        reason: Option<String>,
    ) -> Result<Ticket, SubstrateError>;

    /// Atomic dispatch transition: `Ready → InFlight` plus set
    /// `owner = hand` in one write. Refuses if the ticket is not currently
    /// `Ready` or if the hand row does not exist.
    async fn assign_to_hand(&self, id: &TicketId, hand: &HandId) -> Result<Ticket, SubstrateError>;

    /// Atomic abandonment transition: any non-terminal state → `Ready` and
    /// clear `owner`. Used by the cleanup pass when a hand goes silent past
    /// its TTL. Refuses terminal tickets.
    async fn release_from_hand(
        &self,
        id: &TicketId,
        reason: String,
    ) -> Result<Ticket, SubstrateError>;

    /// Hand-driven transition `InFlight → InReview`. Records the metadata
    /// the verifier needs (branch, PR, head SHA) so the current state is a
    /// projection of the event log (D31).
    async fn transition_to_in_review(
        &self,
        id: &TicketId,
        review: InReviewMetadata,
    ) -> Result<Ticket, SubstrateError>;

    /// Canonical path to `Done`: verifier observed a merge for an `InReview`
    /// ticket. Stores `merge_sha` on the ticket row. Refuses if the ticket
    /// is not currently `InReview`.
    async fn verify_ticket_merged(
        &self,
        id: &TicketId,
        head_sha: String,
        merge_sha: String,
    ) -> Result<Ticket, SubstrateError>;

    /// Verifier observed the PR closed without a merge (D32): transitions
    /// `InReview → Blocked` with `BlockReason::PrClosedUnmerged`. Refuses if
    /// not currently `InReview`.
    async fn verify_ticket_unmerged(
        &self,
        id: &TicketId,
        branch: String,
        pr_url: Option<String>,
    ) -> Result<Ticket, SubstrateError>;

    /// Transition any non-terminal state → `Blocked` with a structured
    /// reason. The only path to `Blocked` from the typed API.
    async fn block_ticket(
        &self,
        id: &TicketId,
        reason: BlockReason,
    ) -> Result<Ticket, SubstrateError>;

    /// Auto-unblock (cleanup pass): clears `block_reason` and transitions
    /// `Blocked → Ready` only when the block is `BlockReason::Dependency`
    /// AND all `blocks`-predecessors are now terminal. Re-verifies inside
    /// the transaction to close the TOCTOU window.
    async fn unblock_ticket(&self, id: &TicketId) -> Result<Ticket, SubstrateError>;

    /// Human recovery: transition `Blocked → Ready` regardless of
    /// `block_reason` flavour, recording the human note.
    async fn human_reopen_blocked(
        &self,
        id: &TicketId,
        note: String,
    ) -> Result<Ticket, SubstrateError>;

    /// D33 pre-dispatch reconciliation. Transitions a re-queued `Ready`
    /// ticket directly to `Done` after the foreman's git check confirms the
    /// recorded `head_sha` is on target. Implementations MUST verify that
    /// the ticket has at least one prior `TicketTransitionedToInReview`
    /// event; without that evidence the call is refused.
    async fn reconcile_ticket_done_from_git(
        &self,
        id: &TicketId,
        head_sha: String,
        merge_sha: String,
    ) -> Result<Ticket, SubstrateError>;

    /// `mode: solo` manual completion. CLI layer enforces the mode guard;
    /// substrate refuses only if the ticket is already terminal.
    async fn mark_ticket_done_manually(
        &self,
        id: &TicketId,
        attestation: ManualDoneAttestation,
    ) -> Result<Ticket, SubstrateError>;

    /// Human rejection path: transition any non-terminal state → `Rejected`,
    /// recording the human reason. Refuses if the ticket is already terminal
    /// (`Done` or `Rejected`). This is the typed counterpart to `block_ticket`
    /// for the irreversible rejection branch of the state machine
    /// (DESIGN.md §8.6).
    async fn reject_ticket(&self, id: &TicketId, reason: String) -> Result<Ticket, SubstrateError>;

    /// Assigns or clears the hand that owns a ticket.
    ///
    /// Retained for non-atomic owner updates (e.g. clearing the owner on a
    /// terminal ticket). The atomic dispatch path is `assign_to_hand` /
    /// `release_from_hand`.
    async fn assign_ticket(
        &self,
        id: &TicketId,
        owner: Option<HandId>,
    ) -> Result<Ticket, SubstrateError>;

    /// Adds `label` to a ticket.
    async fn add_label(&self, id: &TicketId, label: &str) -> Result<(), SubstrateError>;

    /// Removes `label` from a ticket.
    async fn remove_label(&self, id: &TicketId, label: &str) -> Result<(), SubstrateError>;

    /// Creates a typed edge from one ticket to another.
    async fn link(
        &self,
        from: &TicketId,
        to: &TicketId,
        kind: LinkKind,
    ) -> Result<(), SubstrateError>;

    /// Removes a typed edge from one ticket to another.
    async fn unlink(
        &self,
        from: &TicketId,
        to: &TicketId,
        kind: LinkKind,
    ) -> Result<(), SubstrateError>;

    /// Lists links whose `from` endpoint is `id`.
    async fn outgoing_links(&self, id: &TicketId) -> Result<Vec<Link>, SubstrateError>;

    /// Lists links whose `to` endpoint is `id`.
    async fn incoming_links(&self, id: &TicketId) -> Result<Vec<Link>, SubstrateError>;

    /// Creates a named batch.
    async fn create_batch(&self, name: BatchName) -> Result<Batch, SubstrateError>;

    /// Returns a batch by name, or `None` when it does not exist.
    async fn get_batch(&self, name: &BatchName) -> Result<Option<Batch>, SubstrateError>;

    /// Lists batches, optionally including closed batches.
    async fn list_batches(&self, include_closed: bool) -> Result<Vec<Batch>, SubstrateError>;

    /// Force-closes a batch.
    ///
    /// This is an escape hatch for closing past non-terminal tickets. Native
    /// implementations also auto-close batches when all member tickets reach a
    /// terminal state; that path does not go through this method. Calling this
    /// for an already closed batch is idempotent and returns the existing batch.
    async fn close_batch(&self, name: &BatchName) -> Result<Batch, SubstrateError>;

    /// Lists tickets in a batch ordered by `ordinal`, then by creation time.
    async fn tickets_in_batch(&self, name: &BatchName) -> Result<Vec<Ticket>, SubstrateError>;

    /// Registers a hand that can own or execute tickets.
    async fn register_hand(&self, hand: Hand) -> Result<(), SubstrateError>;

    /// Registers a crew hand with its spawned child pid for process liveness
    /// (D75). Dispatchers call this when they spawn an agent process so the
    /// foreman cleanup pass can check `kill(pid, 0)` alongside the heartbeat
    /// TTL. The default implementation delegates to [`Substrate::register_hand`]
    /// (pid ignored) so backends without pid tracking and test mocks keep
    /// working; the native backend overrides it to persist `pid` on the hand
    /// row. The hand's existing `pid` field, if set, is overwritten with the
    /// supplied pid.
    async fn register_hand_with_pid(
        &self,
        mut hand: Hand,
        pid: u32,
    ) -> Result<(), SubstrateError> {
        hand.pid = Some(pid);
        self.register_hand(hand).await
    }

    /// Lists registered hands.
    async fn list_hands(&self) -> Result<Vec<Hand>, SubstrateError>;

    /// Records that a hand is still alive.
    async fn heartbeat(&self, id: &HandId) -> Result<(), SubstrateError>;

    /// T012 alias for [`Substrate::heartbeat`]. Provided so the cleanup
    /// pass and dispatcher can call a method with the contract-aligned name.
    async fn hand_heartbeat(&self, id: &HandId) -> Result<(), SubstrateError>;

    /// **Deprecated.** Use [`Substrate::record_typed_event`] which preserves
    /// the structured payload as JSON in `body` and the discriminator in
    /// `kind`. Retained for one release for the T010 bridge step.
    #[deprecated(
        note = "use record_typed_event; the legacy string-bodied API will be removed after T012"
    )]
    async fn record_event(&self, event: NewEvent) -> Result<Event, SubstrateError>;

    /// **Deprecated.** Use [`Substrate::tail_typed_events`] which returns
    /// `TypedEvent` with the deserialised `EventKind` payload.
    #[deprecated(
        note = "use tail_typed_events; the legacy string-bodied API will be removed after T012"
    )]
    async fn tail_events(
        &self,
        since: Option<DateTime<Utc>>,
        limit: usize,
    ) -> Result<Vec<Event>, SubstrateError>;

    /// The only public path to writing an event. Implementations serialise
    /// `kind` to the snake_case discriminator column and the full payload to
    /// `body` as tagged JSON, guaranteeing the two never diverge.
    async fn record_typed_event(
        &self,
        scope: EventScope,
        kind: EventKind,
    ) -> Result<EventId, SubstrateError>;

    /// Typed event read. Returns events newest-first after `since`, capped
    /// at `limit`.
    async fn tail_typed_events(
        &self,
        since: Option<DateTime<Utc>>,
        limit: usize,
    ) -> Result<Vec<TypedEvent>, SubstrateError>;

    /// Per-ticket event history ordered newest-first. Used by the verifier
    /// to find the most recent `TicketTransitionedToInReview` event.
    async fn ticket_events(
        &self,
        id: &TicketId,
        limit: usize,
    ) -> Result<Vec<TypedEvent>, SubstrateError>;

    /// Returns the current foreman process status.
    async fn foreman_status(&self) -> Result<ForemanStatus, SubstrateError>;

    /// **Deprecated.** Use [`Substrate::record_foreman_detached`]. The new
    /// method writes `mode = detached` to the foreman row and emits a
    /// `ForemanStarted { mode: Detached, pid }` event.
    #[deprecated(note = "use record_foreman_detached")]
    async fn record_foreman_start(&self, pid: u32) -> Result<(), SubstrateError>;

    /// Records that the foreman started in attached mode (running in the
    /// caller's process).
    async fn record_foreman_attached(&self, pid: u32) -> Result<(), SubstrateError>;

    /// Records that the foreman started in detached (daemon) mode.
    async fn record_foreman_detached(&self, pid: u32) -> Result<(), SubstrateError>;

    /// **Deprecated.** Use [`Substrate::record_foreman_stopped`].
    #[deprecated(note = "use record_foreman_stopped")]
    async fn record_foreman_stop(&self) -> Result<(), SubstrateError>;

    /// Records that the foreman stopped cleanly. Writes `mode = stopped`,
    /// clears `pid`, emits `EventKind::ForemanStopped`.
    async fn record_foreman_stopped(&self) -> Result<(), SubstrateError>;

    /// Reserve a worktree slot for a pipeline run and return its planned path.
    async fn reserve_worktree(
        &self,
        run_id: &str,
        branch: &str,
    ) -> Result<std::path::PathBuf, SubstrateError>;

    /// Mark a worktree closed after the run completed (success or failure).
    async fn close_worktree(&self, run_id: &str) -> Result<(), SubstrateError>;
}
