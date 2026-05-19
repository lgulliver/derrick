//! `DataModel` snapshot and tab enum used by the TUI.
//!
//! `DataModel` is the read-side projection populated from substrate queries
//! plus filesystem reads (memory entries) and a stack-adapter shell-out
//! (stack nodes). The renderer modules consume `&DataModel` and never call
//! the substrate directly.

use std::path::PathBuf;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use derrick_substrate::{
    EventKind, EventScope, ForemanMode, ForemanStatus, Substrate, SubstrateError, Ticket,
    TicketFilter, TicketState, TypedEvent,
};

/// One of the six tabs in the dashboard. The discriminant ordering matches
/// the numeric `1`-`6` hotkeys.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Tab {
    /// Overview: standup view of the active batch.
    #[default]
    Overview,
    /// Tickets: filterable table of all tickets.
    Tickets,
    /// Stack: PR graph tree.
    Stack,
    /// Activity: live tail of substrate events.
    Activity,
    /// Tokens: token-spend summary.
    Tokens,
    /// Memory: per-site memory entries.
    Memory,
}

impl Tab {
    /// Human-readable tab title used in the tabs bar.
    pub fn title(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Tickets => "Tickets",
            Self::Stack => "Stack",
            Self::Activity => "Activity",
            Self::Tokens => "Tokens",
            Self::Memory => "Memory",
        }
    }

    /// Zero-based index in the tabs bar (0..=5).
    pub fn index(self) -> usize {
        match self {
            Self::Overview => 0,
            Self::Tickets => 1,
            Self::Stack => 2,
            Self::Activity => 3,
            Self::Tokens => 4,
            Self::Memory => 5,
        }
    }

    /// Inverse of [`Tab::index`]; returns `None` when out of range.
    pub fn from_index(i: usize) -> Option<Self> {
        match i {
            0 => Some(Self::Overview),
            1 => Some(Self::Tickets),
            2 => Some(Self::Stack),
            3 => Some(Self::Activity),
            4 => Some(Self::Tokens),
            5 => Some(Self::Memory),
            _ => None,
        }
    }

    /// All tabs in display order.
    pub fn all() -> [Self; 6] {
        [
            Self::Overview,
            Self::Tickets,
            Self::Stack,
            Self::Activity,
            Self::Tokens,
            Self::Memory,
        ]
    }
}

/// Error returned when parsing a tab name from the `--tab` CLI flag.
#[derive(Debug, thiserror::Error)]
#[error("unknown tab: {0}")]
pub struct ParseTabError(pub String);

impl FromStr for Tab {
    type Err = ParseTabError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "overview" => Ok(Self::Overview),
            "tickets" => Ok(Self::Tickets),
            "stack" => Ok(Self::Stack),
            "activity" => Ok(Self::Activity),
            "tokens" => Ok(Self::Tokens),
            "memory" => Ok(Self::Memory),
            other => Err(ParseTabError(other.to_owned())),
        }
    }
}

/// Aggregate counts and the active batch for the Overview tab header.
#[derive(Clone, Debug, Default)]
pub struct OverviewData {
    /// Name of the active batch (most-recently-touched non-closed batch).
    pub batch_name: Option<String>,
    /// Number of tickets in terminal `Done` state.
    pub tickets_done: u32,
    /// Total tickets visible to the dashboard.
    pub tickets_total: u32,
    /// Tickets currently `InFlight` or `InReview`.
    pub tickets_inflight: u32,
    /// Tickets currently `Ready`.
    pub tickets_ready: u32,
    /// Tickets currently `Blocked`.
    pub tickets_blocked: u32,
    /// Foreman mode and timestamps.
    pub foreman_status: Option<ForemanStatusSnapshot>,
}

/// Plain-data snapshot of the foreman row so renderers do not need to import
/// the substrate types.
#[derive(Clone, Debug)]
pub struct ForemanStatusSnapshot {
    /// Foreman mode (attached/detached/stopped).
    pub mode: ForemanMode,
    /// OS pid when running.
    pub pid: Option<u32>,
    /// Start timestamp when running.
    pub started_at: Option<DateTime<Utc>>,
}

impl From<ForemanStatus> for ForemanStatusSnapshot {
    fn from(value: ForemanStatus) -> Self {
        Self {
            mode: value.mode,
            pid: value.pid,
            started_at: value.started_at,
        }
    }
}

/// One row in the tickets table.
#[derive(Clone, Debug, Default)]
pub struct TicketRow {
    /// Ticket id.
    pub id: String,
    /// Ticket title.
    pub title: String,
    /// `TicketState` rendered as a string (`ready`, `in_flight`, ...).
    pub state: String,
    /// Batch name, if any.
    pub batch: Option<String>,
    /// Hand id, if assigned.
    pub owner: Option<String>,
    /// Last-update timestamp.
    pub updated_at: Option<DateTime<Utc>>,
}

impl From<&Ticket> for TicketRow {
    fn from(t: &Ticket) -> Self {
        Self {
            id: t.id.to_string(),
            title: t.title.clone(),
            state: t.state.to_string(),
            batch: t.batch.as_ref().map(ToString::to_string),
            owner: t.owner.as_ref().map(ToString::to_string),
            updated_at: Some(t.updated_at),
        }
    }
}

/// One node in the PR stack tab.
#[derive(Clone, Debug, Default)]
pub struct StackNode {
    /// Ticket id this node represents.
    pub ticket_id: String,
    /// Branch name.
    pub branch: String,
    /// PR URL, when one was opened.
    pub pr_url: Option<String>,
    /// PR number, parsed from the URL.
    pub pr_number: Option<u64>,
    /// PR/branch state rendered as a short string.
    pub state: String,
    /// Parent branch this branch is stacked on.
    pub parent_branch: Option<String>,
}

/// One row in the Activity tab.
#[derive(Clone, Debug)]
pub struct EventRow {
    /// Event timestamp.
    pub at: DateTime<Utc>,
    /// Snake-case event kind discriminator.
    pub kind: String,
    /// Ticket id when the event is ticket-scoped.
    pub ticket: Option<String>,
    /// One-line rendered body.
    pub body: String,
}

impl From<&TypedEvent> for EventRow {
    fn from(ev: &TypedEvent) -> Self {
        let ticket = match &ev.scope {
            EventScope::Ticket(id) => Some(id.to_string()),
            _ => None,
        };
        Self {
            at: ev.at,
            kind: ev.kind.discriminator().to_owned(),
            ticket,
            body: summarise_event(&ev.kind),
        }
    }
}

fn summarise_event(kind: &EventKind) -> String {
    match kind {
        EventKind::TicketStateChanged { from, to, reason } => {
            let reason = reason
                .as_deref()
                .map(|r| format!(" ({r})"))
                .unwrap_or_default();
            format!("{from} -> {to}{reason}")
        }
        EventKind::TicketAssigned { hand } => format!("assigned to {hand}"),
        EventKind::TicketUnassigned { reason } => format!("unassigned: {reason}"),
        EventKind::Note { body } => body.clone(),
        EventKind::PipelineStepCompleted { step_id, status } => {
            format!("step {step_id}: {status}")
        }
        other => other.discriminator().to_owned(),
    }
}

/// Aggregate token spend summary (placeholder — v1 has no per-event token
/// metadata in the substrate).
#[derive(Clone, Debug, Default)]
pub struct TokenSummary {
    /// Total prompt tokens.
    pub total_in: u64,
    /// Total completion tokens.
    pub total_out: u64,
}

/// One row in the Memory tab.
#[derive(Clone, Debug, Default)]
pub struct MemoryEntry {
    /// Filename-derived slug (e.g. `feedback_testing`).
    pub slug: String,
    /// Absolute path to the entry file.
    pub path: PathBuf,
    /// First ~200 chars of the file body.
    pub preview: String,
}

/// Snapshot of every tab's data, produced by [`DataModel::refresh`].
#[derive(Clone, Debug, Default)]
pub struct DataModel {
    /// Overview tab aggregates.
    pub overview: OverviewData,
    /// All tickets, newest-updated first.
    pub tickets: Vec<TicketRow>,
    /// PR stack nodes from the stack adapter.
    pub stack_nodes: Vec<StackNode>,
    /// Activity tail, newest first.
    pub events: Vec<EventRow>,
    /// Token spend summary.
    pub token_summary: TokenSummary,
    /// Site memory entries.
    pub memory_entries: Vec<MemoryEntry>,
    /// Timestamp of the most recent refresh.
    pub last_refresh: Option<DateTime<Utc>>,
    /// Site name pulled from the substrate.
    pub site_name: String,
}

impl DataModel {
    /// Returns an empty model. Used in tests and for the initial state before
    /// the first refresh.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Pulls fresh data from `substrate` and merges the injected stack and
    /// memory state.
    pub async fn refresh(
        substrate: &dyn Substrate,
        stack_nodes: &[StackNode],
        memory_entries: &[MemoryEntry],
    ) -> Result<Self, SubstrateError> {
        let site = substrate.site().await?;
        let tickets = substrate.list_tickets(TicketFilter::default()).await?;
        let foreman = substrate.foreman_status().await?;
        let events = substrate.tail_typed_events(None, 100).await?;

        let mut overview = OverviewData {
            foreman_status: Some(foreman.into()),
            tickets_total: u32::try_from(tickets.len()).unwrap_or(u32::MAX),
            ..OverviewData::default()
        };
        for t in &tickets {
            match t.state {
                TicketState::Ready => overview.tickets_ready += 1,
                TicketState::InFlight | TicketState::InReview => overview.tickets_inflight += 1,
                TicketState::Blocked => overview.tickets_blocked += 1,
                TicketState::Done => overview.tickets_done += 1,
                TicketState::Rejected => {}
                _ => {}
            }
        }
        overview.batch_name = tickets
            .iter()
            .filter_map(|t| t.batch.as_ref().map(ToString::to_string))
            .next();

        let ticket_rows: Vec<TicketRow> = tickets.iter().map(TicketRow::from).collect();
        let event_rows: Vec<EventRow> = events.iter().map(EventRow::from).collect();

        Ok(Self {
            overview,
            tickets: ticket_rows,
            stack_nodes: stack_nodes.to_vec(),
            events: event_rows,
            token_summary: TokenSummary::default(),
            memory_entries: memory_entries.to_vec(),
            last_refresh: Some(Utc::now()),
            site_name: site.name().to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_from_str_accepts_known_names() {
        assert_eq!("overview".parse::<Tab>().ok(), Some(Tab::Overview));
        assert_eq!("tickets".parse::<Tab>().ok(), Some(Tab::Tickets));
        assert_eq!("STACK".parse::<Tab>().ok(), Some(Tab::Stack));
        assert_eq!("Activity".parse::<Tab>().ok(), Some(Tab::Activity));
        assert_eq!("tokens".parse::<Tab>().ok(), Some(Tab::Tokens));
        assert_eq!("memory".parse::<Tab>().ok(), Some(Tab::Memory));
    }

    #[test]
    fn tab_from_str_rejects_unknown() {
        assert!("bogus".parse::<Tab>().is_err());
        assert!("".parse::<Tab>().is_err());
    }

    #[test]
    fn tab_index_roundtrip() {
        for tab in Tab::all() {
            assert_eq!(Tab::from_index(tab.index()), Some(tab));
        }
        assert_eq!(Tab::from_index(6), None);
    }
}
