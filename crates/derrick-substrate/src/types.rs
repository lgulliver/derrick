use std::fmt;
use std::num::NonZeroUsize;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use uuid::Uuid;

const DEFAULT_TICKET_LIMIT: usize = 100;
const MAX_BATCH_NAME_LEN: usize = 64;
const MAX_HAND_ID_LEN: usize = 64;
const MAX_TICKET_TITLE_CHARS: usize = 200;

/// Returns the canonical regular expression pattern for ticket identifiers.
pub fn ticket_id_pattern() -> &'static str {
    "^[a-z]{1,6}-\\d+$"
}

/// Error type returned by substrate contracts and implementations.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum SubstrateError {
    /// A requested substrate entity was not found.
    #[error("not found: {kind} {id}")]
    NotFound {
        /// The entity kind, such as `ticket` or `batch`.
        kind: &'static str,
        /// The entity identifier.
        id: String,
    },

    /// A write conflicts with current substrate state.
    #[error("conflict: {message}")]
    Conflict {
        /// Human-readable conflict details.
        message: String,
    },

    /// Caller input failed validation.
    #[error("invalid input: {field}: {message}")]
    Invalid {
        /// Field or type that failed validation.
        field: String,
        /// Human-readable validation details.
        message: String,
    },

    /// Backend-specific error wrapped at the substrate boundary.
    #[error("backend error: {0}")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl SubstrateError {
    fn invalid(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Invalid {
            field: field.into(),
            message: message.into(),
        }
    }
}

/// Identifier for a ticket.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TicketId(String);

impl TicketId {
    /// Creates a ticket id matching `^[a-z]{1,6}-\d+$`.
    pub fn new(value: impl Into<String>) -> Result<Self, SubstrateError> {
        let value = value.into();
        if is_valid_ticket_id(&value) {
            Ok(Self(value))
        } else {
            Err(SubstrateError::invalid(
                "ticket_id",
                "must match ^[a-z]{1,6}-\\d+$",
            ))
        }
    }

    /// Returns the ticket id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TicketId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for TicketId {
    type Err = SubstrateError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for TicketId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for TicketId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Name of an ordered batch of tickets.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BatchName(String);

impl BatchName {
    /// Creates a batch name matching `^[a-z0-9][a-z0-9-]{0,63}$`.
    pub fn new(value: impl Into<String>) -> Result<Self, SubstrateError> {
        let value = value.into();
        if is_valid_batch_name(&value) {
            Ok(Self(value))
        } else {
            Err(SubstrateError::invalid(
                "batch_name",
                "must match ^[a-z0-9][a-z0-9-]{0,63}$",
            ))
        }
    }

    /// Returns the batch name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BatchName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for BatchName {
    type Err = SubstrateError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for BatchName {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for BatchName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Identifier for a hand.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HandId(String);

impl HandId {
    /// Creates a hand id matching `^[a-z][a-z0-9-]{0,63}$`.
    pub fn new(value: impl Into<String>) -> Result<Self, SubstrateError> {
        let value = value.into();
        if is_valid_hand_id(&value) {
            Ok(Self(value))
        } else {
            Err(SubstrateError::invalid(
                "hand_id",
                "must match ^[a-z][a-z0-9-]{0,63}$",
            ))
        }
    }

    /// Returns the hand id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for HandId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for HandId {
    type Err = SubstrateError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for HandId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for HandId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Lifecycle state for a ticket.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TicketState {
    /// Ready to be picked up by a hand.
    Ready,
    /// Currently owned and being worked.
    InFlight,
    /// Work claimed complete by the hand, awaiting verifier confirmation (D31).
    InReview,
    /// Waiting on another ticket or external condition.
    Blocked,
    /// Completed successfully.
    Done,
    /// Closed without being accepted.
    Rejected,
}

impl TicketState {
    /// Returns `true` when the state closes a ticket.
    ///
    /// Only `Done` and `Rejected` are terminal. `InReview` and `Blocked`
    /// are non-terminal: `Blocked` awaits a human decision (re-open or
    /// reject) and must not auto-close a batch.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Rejected)
    }
}

impl fmt::Display for TicketState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Ready => "ready",
            Self::InFlight => "in_flight",
            Self::InReview => "in_review",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Rejected => "rejected",
        })
    }
}

impl FromStr for TicketState {
    type Err = SubstrateError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "ready" => Ok(Self::Ready),
            "in_flight" => Ok(Self::InFlight),
            "in_review" => Ok(Self::InReview),
            "blocked" => Ok(Self::Blocked),
            "done" => Ok(Self::Done),
            "rejected" => Ok(Self::Rejected),
            _ => Err(SubstrateError::invalid("ticket_state", "unknown state")),
        }
    }
}

/// Reason a ticket is `Blocked`. Determines whether the cleanup pass may
/// auto-unblock it.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum BlockReason {
    /// Blocked on a `blocks`-link predecessor that wasn't terminal at
    /// dispatch time. Auto-unblocks when the predecessor reaches
    /// `Done` / `Rejected`.
    Dependency {
        /// Predecessor ticket whose terminal state will release this one.
        predecessor: TicketId,
    },
    /// Verifier saw the PR closed without a merge. Requires human action.
    PrClosedUnmerged {
        /// Branch the hand pushed.
        branch: String,
        /// PR URL, when one was opened.
        pr_url: Option<String>,
    },
    /// Stacking restack conflict (D19). Requires human action to rebase.
    RestackConflict {
        /// Recipe identifier that failed to restack.
        recipe: String,
    },
    /// User explicitly blocked via `derrick ticket block`.
    Human {
        /// Free-form human note.
        note: String,
    },
}

/// Typed edge between two tickets.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LinkKind {
    /// The `from` ticket blocks the `to` ticket.
    Blocks,
    /// Informational relationship between tickets.
    Related,
}

impl fmt::Display for LinkKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Blocks => "blocks",
            Self::Related => "related",
        })
    }
}

impl FromStr for LinkKind {
    type Err = SubstrateError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "blocks" => Ok(Self::Blocks),
            "related" => Ok(Self::Related),
            _ => Err(SubstrateError::invalid("link_kind", "unknown link kind")),
        }
    }
}

/// Kind of hand that can own or execute tickets.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum HandKind {
    /// Interactive Claude-driven hand.
    Claude,
    /// Copilot agent dispatch hand.
    Copilot,
    /// Codex host-CLI executor hand.
    Codex,
    /// Opencode host-CLI executor hand.
    Opencode,
    /// Aider host-CLI executor hand.
    Aider,
    /// Human-owned hand.
    Human,
}

impl fmt::Display for HandKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Claude => "claude",
            Self::Copilot => "copilot",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
            Self::Aider => "aider",
            Self::Human => "human",
        })
    }
}

impl FromStr for HandKind {
    type Err = SubstrateError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "claude" => Ok(Self::Claude),
            "copilot" => Ok(Self::Copilot),
            "codex" => Ok(Self::Codex),
            "opencode" => Ok(Self::Opencode),
            "aider" => Ok(Self::Aider),
            "human" => Ok(Self::Human),
            _ => Err(SubstrateError::invalid("hand_kind", "unknown hand kind")),
        }
    }
}

/// Kind of event in the activity log. Carries the full payload so the
/// current ticket state is a projection of the event log (D31).
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum EventKind {
    /// A ticket was created.
    TicketCreated {
        /// Initial lifecycle state.
        initial_state: TicketState,
    },
    /// A ticket changed state.
    TicketStateChanged {
        /// Previous state.
        from: TicketState,
        /// New state.
        to: TicketState,
        /// Optional human-readable reason.
        reason: Option<String>,
    },
    /// A ticket entered `InReview` with the metadata the verifier needs to
    /// reconcile against git later.
    TicketTransitionedToInReview {
        /// Branch the hand pushed.
        branch: String,
        /// PR URL if opened.
        pr_url: Option<String>,
        /// PR number extracted from the URL.
        pr_number: Option<u64>,
        /// Head commit SHA at transition.
        head_sha: String,
    },
    /// The verifier observed a merge for an `InReview` ticket.
    TicketVerifiedMerged {
        /// Head SHA on the `InReview` branch at observation.
        head_sha: String,
        /// Merge commit on the target branch.
        merge_sha: String,
    },
    /// The verifier observed the PR closed without a merge (D32 — blocks).
    TicketVerifiedUnmerged {
        /// Human-readable reason from the verifier.
        reason: String,
    },
    /// A human attested completion via `derrick ticket done` (`solo` mode).
    TicketMarkedDoneManually {
        /// Human identity claiming completion.
        claimant: String,
        /// Human note recorded alongside the attestation.
        note: String,
    },
    /// A ticket owner was set.
    TicketAssigned {
        /// Hand now owning the ticket.
        hand: HandId,
    },
    /// A ticket owner was cleared.
    TicketUnassigned {
        /// Reason the hand was released.
        reason: String,
    },
    /// A batch was created.
    BatchCreated,
    /// A batch was closed.
    BatchClosed {
        /// Tickets that were still open at force-close (empty on auto-close).
        open_ticket_ids: Vec<TicketId>,
    },
    /// The foreman started.
    ForemanStarted {
        /// Mode the foreman started in.
        mode: ForemanMode,
        /// OS pid of the loop process.
        pid: u32,
    },
    /// The foreman stopped cleanly.
    ForemanStopped,
    /// A hand was registered.
    HandRegistered,
    /// A hand reported a heartbeat.
    HandHeartbeat,
    /// A hand was abandoned by the cleanup pass after its TTL expired.
    HandAbandoned {
        /// Ticket the hand was previously owning, if any.
        previous_owner_of: TicketId,
    },
    /// A worktree was reserved for a dispatch run.
    WorktreeReserved {
        /// Run identifier.
        run_id: String,
        /// Branch the worktree was reserved for.
        branch: String,
    },
    /// A worktree was finalised cleanly after the run completed.
    WorktreeFinalized {
        /// Run identifier.
        run_id: String,
    },
    /// A worktree was abandoned by the cleanup pass.
    WorktreeAbandoned {
        /// Run identifier.
        run_id: String,
        /// Reason the worktree was abandoned.
        reason: String,
    },
    /// Operator escalation: ticket stuck in `InReview` past the threshold.
    EscalationStuckInReview {
        /// Ticket that is stuck.
        ticket: TicketId,
        /// Branch the ticket is on.
        branch: String,
    },
    /// A restack conflict was detected.
    RestackConflict {
        /// Ticket whose restack failed.
        ticket: TicketId,
        /// Recipe identifier that failed.
        recipe: String,
    },
    /// Human-readable note.
    Note {
        /// Free-form body.
        body: String,
    },
    /// A pipeline step finished and recorded a terminal status.
    PipelineStepCompleted {
        /// Step identifier (e.g. "specify", "plan", "assay").
        step_id: String,
        /// Terminal status: "success", "skipped", "halted", or "failed".
        status: String,
    },
}

/// Snake-case discriminator strings for `EventKind` variants. Used for the
/// `kind` column in the events table and for human-readable filters.
impl EventKind {
    /// Returns the snake_case discriminator for this event kind.
    pub fn discriminator(&self) -> &'static str {
        match self {
            Self::TicketCreated { .. } => "ticket_created",
            Self::TicketStateChanged { .. } => "ticket_state_changed",
            Self::TicketTransitionedToInReview { .. } => "ticket_transitioned_to_in_review",
            Self::TicketVerifiedMerged { .. } => "ticket_verified_merged",
            Self::TicketVerifiedUnmerged { .. } => "ticket_verified_unmerged",
            Self::TicketMarkedDoneManually { .. } => "ticket_marked_done_manually",
            Self::TicketAssigned { .. } => "ticket_assigned",
            Self::TicketUnassigned { .. } => "ticket_unassigned",
            Self::BatchCreated => "batch_created",
            Self::BatchClosed { .. } => "batch_closed",
            Self::ForemanStarted { .. } => "foreman_started",
            Self::ForemanStopped => "foreman_stopped",
            Self::HandRegistered => "hand_registered",
            Self::HandHeartbeat => "hand_heartbeat",
            Self::HandAbandoned { .. } => "hand_abandoned",
            Self::WorktreeReserved { .. } => "worktree_reserved",
            Self::WorktreeFinalized { .. } => "worktree_finalized",
            Self::WorktreeAbandoned { .. } => "worktree_abandoned",
            Self::EscalationStuckInReview { .. } => "escalation_stuck_in_review",
            Self::RestackConflict { .. } => "restack_conflict",
            Self::Note { .. } => "note",
            Self::PipelineStepCompleted { .. } => "pipeline_step_completed",
        }
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.discriminator())
    }
}

/// Current foreman execution mode.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ForemanMode {
    /// Foreman is running in the background.
    Detached,
    /// Foreman is running in the current process.
    Attached,
    /// Foreman is not running.
    Stopped,
}

impl fmt::Display for ForemanMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Detached => "detached",
            Self::Attached => "attached",
            Self::Stopped => "stopped",
        })
    }
}

impl FromStr for ForemanMode {
    type Err = SubstrateError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "detached" => Ok(Self::Detached),
            "attached" => Ok(Self::Attached),
            "stopped" => Ok(Self::Stopped),
            _ => Err(SubstrateError::invalid(
                "foreman_mode",
                "unknown foreman mode",
            )),
        }
    }
}

/// Ticket persisted by a substrate backend.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Ticket {
    /// Caller-supplied ticket identifier.
    pub id: TicketId,
    /// Batch containing this ticket, when any.
    pub batch: Option<BatchName>,
    /// Ordering within the batch.
    pub ordinal: Option<u32>,
    /// Short human-readable title.
    pub title: String,
    /// Full ticket body.
    pub body: String,
    /// Current lifecycle state.
    pub state: TicketState,
    /// Labels attached to this ticket.
    pub labels: Vec<String>,
    /// Hand that owns this ticket.
    pub owner: Option<HandId>,
    /// Merge commit SHA on the target branch (set only via the verifier path).
    pub merge_sha: Option<String>,
    /// Structured reason this ticket is `Blocked`. Set only while
    /// `state == Blocked`; cleared on transition out.
    pub block_reason: Option<BlockReason>,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Last update timestamp.
    pub updated_at: DateTime<Utc>,
}

/// Ordered named group of tickets.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Batch {
    /// Batch name.
    pub name: BatchName,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Closure timestamp, when closed.
    pub closed_at: Option<DateTime<Utc>>,
}

/// Typed edge between two tickets.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Link {
    /// Source ticket.
    pub from: TicketId,
    /// Destination ticket.
    pub to: TicketId,
    /// Edge kind.
    pub kind: LinkKind,
}

/// Registered actor that can own or execute a ticket.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Hand {
    /// Stable hand identifier.
    pub id: HandId,
    /// Hand kind.
    pub kind: HandKind,
    /// Last heartbeat timestamp.
    pub last_seen: Option<DateTime<Utc>>,
}

/// Persisted activity event.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Event {
    /// Event id.
    pub id: Uuid,
    /// Event timestamp.
    pub at: DateTime<Utc>,
    /// Event kind.
    pub kind: EventKind,
    /// Ticket associated with the event, when any.
    pub ticket: Option<TicketId>,
    /// Event body.
    pub body: String,
}

/// Current foreman process status.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ForemanStatus {
    /// Foreman process id, when known.
    pub pid: Option<u32>,
    /// Foreman start timestamp, when running.
    pub started_at: Option<DateTime<Utc>>,
    /// Foreman mode.
    pub mode: ForemanMode,
}

/// Input for creating a ticket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewTicket {
    /// Caller-supplied ticket identifier.
    pub id: TicketId,
    /// Batch containing the ticket, when any.
    pub batch: Option<BatchName>,
    /// Desired ordering within the batch; `None` means end of batch.
    pub ordinal: Option<u32>,
    /// Short human-readable title, limited to 200 Unicode scalar values.
    pub title: String,
    /// Full ticket body.
    pub body: String,
    /// Initial labels.
    pub labels: Vec<String>,
}

impl NewTicket {
    /// Creates a ticket input after validating title length and ordinal scope.
    pub fn new(
        id: TicketId,
        batch: Option<BatchName>,
        ordinal: Option<u32>,
        title: impl Into<String>,
        body: impl Into<String>,
        labels: Vec<String>,
    ) -> Result<Self, SubstrateError> {
        let title = title.into();
        if title.chars().count() > MAX_TICKET_TITLE_CHARS {
            return Err(SubstrateError::invalid(
                "title",
                "must be 200 characters or fewer",
            ));
        }

        if ordinal.is_some() && batch.is_none() {
            return Err(SubstrateError::invalid(
                "ordinal",
                "requires a ticket batch",
            ));
        }

        Ok(Self {
            id,
            batch,
            ordinal,
            title,
            body: body.into(),
            labels,
        })
    }
}

/// Input for recording an activity event (legacy string-bodied surface;
/// retained for the deprecated `record_event` API).
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct NewEvent {
    /// Event kind discriminator string.
    pub kind: String,
    /// Ticket associated with the event, when any.
    pub ticket: Option<TicketId>,
    /// Event body.
    pub body: String,
}

/// Identifier for a persisted typed event.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct EventId(pub i64);

impl fmt::Display for EventId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// Scope of an event in the activity log. Exactly one variant identifies
/// the entity the event pertains to.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
#[non_exhaustive]
pub enum EventScope {
    /// Scoped to a specific ticket.
    Ticket(TicketId),
    /// Scoped to a batch.
    Batch(BatchName),
    /// Scoped to a hand.
    Hand(HandId),
    /// Scoped to a worktree dispatch run.
    Worktree {
        /// Run identifier.
        run_id: String,
    },
    /// Scoped to the site as a whole.
    Site,
}

/// A typed event read from the activity log. The `kind` payload carries
/// the full structured event content (D31).
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct TypedEvent {
    /// Persistent event id.
    pub id: EventId,
    /// Event scope.
    pub scope: EventScope,
    /// Structured event payload.
    pub kind: EventKind,
    /// Event timestamp.
    pub at: DateTime<Utc>,
}

/// Attestation captured when a human marks a ticket `Done` manually via
/// `derrick ticket done` (`solo` mode).
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ManualDoneAttestation {
    /// Human identity claiming completion.
    pub claimant: String,
    /// Free-form note recorded in the event body.
    pub note: String,
}

/// Metadata captured when a hand transitions a ticket to `InReview`.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct InReviewMetadata {
    /// Branch the hand pushed (must follow
    /// `derrick/<batch>/<ticket_id>` when stacking is on).
    pub branch: String,
    /// PR URL, when one was opened. `None` means the hand is mid-push.
    pub pr_url: Option<String>,
    /// PR number extracted from the URL.
    pub pr_number: Option<u64>,
    /// Head commit SHA at the moment of transition.
    pub head_sha: String,
}

/// Filters used when listing tickets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TicketFilter {
    /// Match only this ticket state.
    pub state: Option<TicketState>,
    /// Match only tickets in this batch.
    pub batch: Option<BatchName>,
    /// Match only tickets owned by this hand.
    pub owner: Option<HandId>,
    /// Match only tickets with this label.
    pub label: Option<String>,
    /// Maximum number of tickets to return; `None` means unlimited.
    pub limit: Option<NonZeroUsize>,
}

impl Default for TicketFilter {
    fn default() -> Self {
        Self {
            state: None,
            batch: None,
            owner: None,
            label: None,
            limit: NonZeroUsize::new(DEFAULT_TICKET_LIMIT),
        }
    }
}

fn is_valid_ticket_id(value: &str) -> bool {
    let Some((prefix, number)) = value.split_once('-') else {
        return false;
    };

    !number.is_empty()
        && (1..=6).contains(&prefix.len())
        && prefix.bytes().all(|byte| byte.is_ascii_lowercase())
        && number.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_valid_batch_name(value: &str) -> bool {
    let bytes = value.as_bytes();
    let Some((&first, rest)) = bytes.split_first() else {
        return false;
    };

    bytes.len() <= MAX_BATCH_NAME_LEN
        && is_ascii_lowercase_or_digit(first)
        && rest
            .iter()
            .copied()
            .all(|byte| is_ascii_lowercase_or_digit(byte) || byte == b'-')
}

fn is_valid_hand_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    let Some((&first, rest)) = bytes.split_first() else {
        return false;
    };

    bytes.len() <= MAX_HAND_ID_LEN
        && first.is_ascii_lowercase()
        && rest
            .iter()
            .copied()
            .all(|byte| is_ascii_lowercase_or_digit(byte) || byte == b'-')
}

fn is_ascii_lowercase_or_digit(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit()
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::str::FromStr;

    use derrick_config::Config;

    use super::*;
    use crate::Site;

    fn assert_invalid<T>(result: Result<T, SubstrateError>) {
        assert!(matches!(result, Err(SubstrateError::Invalid { .. })));
    }

    fn ticket_id(value: &str) -> TicketId {
        match TicketId::new(value) {
            Ok(id) => id,
            Err(error) => unreachable!("test fixture ticket id should be valid: {error}"),
        }
    }

    fn batch_name(value: &str) -> BatchName {
        match BatchName::new(value) {
            Ok(name) => name,
            Err(error) => unreachable!("test fixture batch name should be valid: {error}"),
        }
    }

    fn hand_id(value: &str) -> HandId {
        match HandId::new(value) {
            Ok(id) => id,
            Err(error) => unreachable!("test fixture hand id should be valid: {error}"),
        }
    }

    fn nonzero(value: usize) -> NonZeroUsize {
        match NonZeroUsize::new(value) {
            Some(value) => value,
            None => unreachable!("test fixture non-zero usize should be valid"),
        }
    }

    #[test]
    fn ticket_id_accepts_valid_form() {
        for value in ["mp-1", "xyz-42", "a-1"] {
            let parsed = TicketId::new(value);
            assert!(parsed.is_ok(), "{value} should parse");
        }
    }

    #[test]
    fn ticket_id_rejects_invalid_form() {
        for value in ["mp1", "MP-1", "mp-", "mp-x", "", "mp-١"] {
            assert_invalid(TicketId::new(value));
        }
    }

    #[test]
    fn ticket_id_pattern_matches_ticket_id_validator() {
        let regex = match regex::RegexBuilder::new(ticket_id_pattern())
            .unicode(false)
            .build()
        {
            Ok(regex) => regex,
            Err(error) => unreachable!("ticket id pattern should compile: {error}"),
        };

        for value in [
            "a-1",
            "abcdef-123",
            "abc-000",
            "abcdefg-1",
            "ABC-1",
            "ab-",
            "ab-x",
            "ab-١",
            " ab-1",
            "ab-1 ",
            "ab-1-extra",
            "",
        ] {
            assert_eq!(
                regex.is_match(value),
                TicketId::new(value).is_ok(),
                "{value:?} should have matching regex and validator results",
            );
        }
    }

    #[test]
    fn batch_name_accepts_valid() {
        for value in ["001-webhook", "a"] {
            let parsed = BatchName::new(value);
            assert!(parsed.is_ok(), "{value} should parse");
        }
    }

    #[test]
    fn batch_name_rejects_invalid() {
        let too_long = "a".repeat(MAX_BATCH_NAME_LEN + 1);
        for value in ["-leading", "Upper", too_long.as_str()] {
            assert_invalid(BatchName::new(value));
        }
    }

    #[test]
    fn hand_id_accepts_valid() {
        for value in ["bramble", "hand-1", "sumac-7"] {
            let parsed = HandId::new(value);
            assert!(parsed.is_ok(), "{value} should parse");
        }
    }

    #[test]
    fn hand_id_rejects_invalid() {
        let too_long = "a".repeat(MAX_HAND_ID_LEN + 1);
        for value in [
            "1-leading-digit",
            "-leading-dash",
            "Upper",
            "",
            too_long.as_str(),
            "contains_underscore",
        ] {
            assert_invalid(HandId::new(value));
        }
    }

    #[test]
    fn new_ticket_rejects_title_over_200_chars() {
        let valid_title = "ø".repeat(MAX_TICKET_TITLE_CHARS);
        let too_long_title = "ø".repeat(MAX_TICKET_TITLE_CHARS + 1);

        assert!(NewTicket::new(ticket_id("mp-1"), None, None, valid_title, "", Vec::new()).is_ok());
        assert_invalid(NewTicket::new(
            ticket_id("mp-2"),
            None,
            None,
            too_long_title,
            "",
            Vec::new(),
        ));
    }

    #[test]
    fn new_ticket_rejects_ordinal_without_batch() {
        assert_invalid(NewTicket::new(
            ticket_id("mp-1"),
            None,
            Some(1),
            "title",
            "",
            Vec::new(),
        ));
    }

    #[test]
    fn new_ticket_accepts_ordinal_with_batch() {
        let ticket = NewTicket::new(
            ticket_id("mp-1"),
            Some(batch_name("batch-1")),
            Some(1),
            "title",
            "body",
            vec!["phase-1".to_owned()],
        );

        assert!(ticket.is_ok());
    }

    #[test]
    fn serde_round_trip_for_every_enum() {
        assert_enum_serde(TicketState::Ready, "ready");
        assert_enum_serde(TicketState::InFlight, "in_flight");
        assert_enum_serde(TicketState::InReview, "in_review");
        assert_enum_serde(TicketState::Blocked, "blocked");
        assert_enum_serde(TicketState::Done, "done");
        assert_enum_serde(TicketState::Rejected, "rejected");
        assert_enum_serde(LinkKind::Blocks, "blocks");
        assert_enum_serde(LinkKind::Related, "related");
        assert_enum_serde(HandKind::Claude, "claude");
        assert_enum_serde(HandKind::Copilot, "copilot");
        assert_enum_serde(HandKind::Codex, "codex");
        assert_enum_serde(HandKind::Opencode, "opencode");
        assert_enum_serde(HandKind::Aider, "aider");
        assert_enum_serde(HandKind::Human, "human");
        assert_enum_serde(ForemanMode::Detached, "detached");
        assert_enum_serde(ForemanMode::Attached, "attached");
        assert_enum_serde(ForemanMode::Stopped, "stopped");
    }

    #[test]
    fn event_kind_serde_round_trips_via_tag() {
        let kind = EventKind::TicketStateChanged {
            from: TicketState::Ready,
            to: TicketState::InFlight,
            reason: Some("dispatch".to_owned()),
        };
        let json = serde_json::to_string(&kind).expect("serialize");
        assert!(json.contains("\"kind\":\"ticket_state_changed\""));
        let back: EventKind = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, kind);

        let pipeline = EventKind::PipelineStepCompleted {
            step_id: "clarify".to_owned(),
            status: "success".to_owned(),
        };
        let json = serde_json::to_string(&pipeline).expect("serialize");
        assert!(json.contains("\"kind\":\"pipeline_step_completed\""));
        let back: EventKind = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, pipeline);
    }

    #[test]
    fn event_kind_discriminator_matches_snake_case() {
        assert_eq!(EventKind::BatchCreated.discriminator(), "batch_created");
        assert_eq!(
            EventKind::Note {
                body: "x".to_owned()
            }
            .discriminator(),
            "note"
        );
        assert_eq!(
            EventKind::PipelineStepCompleted {
                step_id: "clarify".to_owned(),
                status: "success".to_owned(),
            }
            .discriminator(),
            "pipeline_step_completed"
        );
    }

    fn assert_enum_serde<T>(value: T, expected: &str)
    where
        T: Copy + fmt::Debug + PartialEq + Serialize + for<'de> Deserialize<'de>,
    {
        let serialized = match serde_json::to_string(&value) {
            Ok(serialized) => serialized,
            Err(error) => unreachable!("enum serialization should succeed: {error}"),
        };
        assert_eq!(serialized, format!("\"{expected}\""));

        let deserialized: T = match serde_json::from_str(&serialized) {
            Ok(deserialized) => deserialized,
            Err(error) => unreachable!("enum deserialization should succeed: {error}"),
        };
        assert_eq!(deserialized, value);
    }

    #[test]
    fn display_from_str_round_trip_for_every_supported_type() {
        assert_display_from_str(ticket_id("mp-1"));
        assert_display_from_str(batch_name("batch-1"));
        assert_display_from_str(hand_id("copilot-1"));
        assert_display_from_str(TicketState::InFlight);
        assert_display_from_str(TicketState::InReview);
        assert_display_from_str(LinkKind::Blocks);
        assert_display_from_str(HandKind::Copilot);
        assert_display_from_str(HandKind::Codex);
        assert_display_from_str(HandKind::Opencode);
        assert_display_from_str(HandKind::Aider);
        assert_display_from_str(ForemanMode::Detached);
    }

    fn assert_display_from_str<T>(value: T)
    where
        T: fmt::Debug + PartialEq + ToString + FromStr,
        <T as FromStr>::Err: fmt::Display,
    {
        let rendered = value.to_string();
        let parsed = match rendered.parse::<T>() {
            Ok(parsed) => parsed,
            Err(error) => unreachable!("display output should parse: {error}"),
        };
        assert_eq!(parsed, value);
    }

    #[test]
    fn ticket_filter_default_has_limit_100() {
        let filter = TicketFilter::default();

        assert_eq!(filter.limit, Some(nonzero(DEFAULT_TICKET_LIMIT)));
        assert!(filter.state.is_none());
        assert!(filter.batch.is_none());
        assert!(filter.owner.is_none());
        assert!(filter.label.is_none());
    }

    #[test]
    fn ticket_filter_unlimited() {
        let filter = TicketFilter {
            state: None,
            batch: None,
            owner: None,
            label: None,
            limit: None,
        };

        let TicketFilter { limit, .. } = filter;
        assert!(limit.is_none());
    }

    #[test]
    fn site_reexport_is_derrick_config_site() {
        let site: Site = Config::defaults().site().clone();
        let _: derrick_config::Site = site;
    }

    #[test]
    fn non_exhaustive_compiles_at_match_site() {
        let state = TicketState::Ready;
        let link = LinkKind::Related;
        let hand = HandKind::Human;
        let event = EventKind::Note {
            body: "hi".to_owned(),
        };
        let foreman = ForemanMode::Stopped;

        assert_eq!(
            match state {
                TicketState::Ready => "ready",
                _ => "other",
            },
            "ready"
        );
        assert_eq!(
            match link {
                LinkKind::Related => "related",
                _ => "other",
            },
            "related"
        );
        assert_eq!(
            match hand {
                HandKind::Human => "human",
                _ => "other",
            },
            "human"
        );
        assert_eq!(
            match event {
                EventKind::Note { .. } => "note",
                _ => "other",
            },
            "note"
        );
        assert_eq!(
            match foreman {
                ForemanMode::Stopped => "stopped",
                _ => "other",
            },
            "stopped"
        );
    }

    #[test]
    fn newtype_serde_rejects_invalid_values() {
        let result: Result<TicketId, _> = serde_json::from_str("\"MP-1\"");
        assert!(result.is_err());

        let result: Result<BatchName, _> = serde_json::from_str("\"-bad\"");
        assert!(result.is_err());

        let result: Result<HandId, _> = serde_json::from_str("\"1-bad\"");
        assert!(result.is_err());
    }
}
