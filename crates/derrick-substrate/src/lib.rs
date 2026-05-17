//! Contract types and trait for derrick's execution substrate.
//!
//! This crate defines the storage boundary used by downstream crates. It does
//! not provide a native backend, open SQLite, spawn the foreman, or perform any
//! I/O; those responsibilities live in `derrick-substrate-native`.

mod types;

use chrono::{DateTime, Utc};

pub use derrick_config::Site;
pub use types::{
    ticket_id_pattern, Batch, BatchName, Event, EventKind, ForemanMode, ForemanStatus, Hand,
    HandId, HandKind, Link, LinkKind, NewEvent, NewTicket, SubstrateError, Ticket, TicketFilter,
    TicketId, TicketState,
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

    /// Sets a ticket state and records `reason` in activity when supplied.
    ///
    /// Implementations auto-close a ticket's batch when this transition leaves
    /// all member tickets in terminal states (`Done` or `Rejected`).
    async fn set_ticket_state(
        &self,
        id: &TicketId,
        state: TicketState,
        reason: Option<String>,
    ) -> Result<Ticket, SubstrateError>;

    /// Assigns or clears the hand that owns a ticket.
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

    /// Lists registered hands.
    async fn list_hands(&self) -> Result<Vec<Hand>, SubstrateError>;

    /// Records that a hand is still alive.
    async fn heartbeat(&self, id: &HandId) -> Result<(), SubstrateError>;

    /// Appends an activity event and returns the persisted event.
    async fn record_event(&self, event: NewEvent) -> Result<Event, SubstrateError>;

    /// Returns recent activity after `since`, capped at `limit`.
    async fn tail_events(
        &self,
        since: Option<DateTime<Utc>>,
        limit: usize,
    ) -> Result<Vec<Event>, SubstrateError>;

    /// Returns the current foreman process status.
    async fn foreman_status(&self) -> Result<ForemanStatus, SubstrateError>;

    /// Records that a foreman started with process id `pid`.
    async fn record_foreman_start(&self, pid: u32) -> Result<(), SubstrateError>;

    /// Records that the foreman stopped.
    async fn record_foreman_stop(&self) -> Result<(), SubstrateError>;
}
