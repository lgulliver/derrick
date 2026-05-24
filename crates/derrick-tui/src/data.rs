//! `DataModel` snapshot and tab enum used by the TUI.
//!
//! `DataModel` is the read-side projection populated from substrate queries
//! plus filesystem reads (memory entries) and a stack-adapter shell-out
//! (stack nodes). The renderer modules consume `&DataModel` and never call
//! the substrate directly.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use chrono::{DateTime, Timelike, Utc};
use derrick_substrate::{
    EventKind, EventScope, ForemanMode, ForemanStatus, Substrate, SubstrateError, Ticket,
    TicketFilter, TicketState, TypedEvent,
};
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Stack summary
// ---------------------------------------------------------------------------

/// Aggregate counts derived from the current `stack_nodes` for the Overview
/// row. Computed locally — no extra substrate round-trip required.
#[derive(Clone, Debug, Default)]
pub struct StackSummary {
    /// PRs whose `state` is `"merged"`.
    pub merged: u32,
    /// PRs whose `state` is `"open"`.
    pub open: u32,
    /// Nodes not yet in a PR (state is neither merged nor open).
    pub pending: u32,
    /// `true` when no node carries a conflict state (state contains
    /// `"conflict"`).
    pub restack_ok: bool,
}

impl StackSummary {
    fn from_nodes(nodes: &[StackNode]) -> Self {
        let mut s = Self {
            restack_ok: true,
            ..Default::default()
        };
        for n in nodes {
            match n.state.as_str() {
                "merged" => s.merged += 1,
                "open" => s.open += 1,
                _ => s.pending += 1,
            }
            if n.state.contains("conflict") {
                s.restack_ok = false;
            }
        }
        s
    }
}

// ---------------------------------------------------------------------------
// Last assay snapshot
// ---------------------------------------------------------------------------

/// Snapshot of the most recent `assay` pipeline step event, derived from the
/// substrate event tail.
#[derive(Clone, Debug)]
pub struct LastAssaySnapshot {
    /// Terminal status reported by the step: `"success"`, `"skipped"`,
    /// `"halted"`, or `"failed"`.
    pub verdict: String,
    /// Model identifier, if known (requires run-manifest plumbing — `None`
    /// until that is wired).
    pub model: Option<String>,
    /// When the step completed.
    pub at: DateTime<Utc>,
}

/// One of the seven tabs in the dashboard. The discriminant ordering matches
/// the numeric `1`-`7` hotkeys.
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
    /// Hands: per-hand activity rollup.
    Hands,
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
            Self::Hands => "Hands",
        }
    }

    /// Zero-based index in the tabs bar (0..=6).
    pub fn index(self) -> usize {
        match self {
            Self::Overview => 0,
            Self::Tickets => 1,
            Self::Stack => 2,
            Self::Activity => 3,
            Self::Tokens => 4,
            Self::Memory => 5,
            Self::Hands => 6,
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
            6 => Some(Self::Hands),
            _ => None,
        }
    }

    /// All tabs in display order.
    pub fn all() -> [Self; 7] {
        [
            Self::Overview,
            Self::Tickets,
            Self::Stack,
            Self::Activity,
            Self::Tokens,
            Self::Memory,
            Self::Hands,
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
            "hands" => Ok(Self::Hands),
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
    /// Merged / open / pending counts derived from the stack nodes.
    pub stack_summary: StackSummary,
    /// Most recent `assay` pipeline step event, if any.
    pub last_assay: Option<LastAssaySnapshot>,
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
    /// Hand id when the event is hand-scoped.
    pub hand: Option<String>,
    /// Run id when the event is worktree/pipeline-scoped.
    pub run_id: Option<String>,
    /// One-line rendered body.
    pub body: String,
}

impl From<&TypedEvent> for EventRow {
    fn from(ev: &TypedEvent) -> Self {
        let (ticket, hand, run_id) = match &ev.scope {
            EventScope::Ticket(id) => (Some(id.to_string()), None, None),
            EventScope::Hand(h) => (None, Some(h.to_string()), None),
            EventScope::Worktree { run_id } => (None, None, Some(run_id.clone())),
            _ => (None, None, None),
        };
        Self {
            at: ev.at,
            kind: ev.kind.discriminator().to_owned(),
            ticket,
            hand,
            run_id,
            body: summarise_event(&ev.kind),
        }
    }
}

// ---------------------------------------------------------------------------
// Activity filter
// ---------------------------------------------------------------------------

/// Parsed filter for the Activity tab. Derived from the shared
/// [`FilterState`](crate::app::FilterState) query string at render time.
///
/// Plain text (no prefix) performs a substring match across all fields.
/// Prefixed queries narrow to a single dimension:
///
/// ```text
/// ticket:tst-42     — only events scoped to ticket tst-42
/// hand:bramble      — only events scoped to hand bramble
/// run:abc123        — only events scoped to run abc123
/// ```
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum ActivityFilter {
    /// No filter — every event is shown.
    #[default]
    None,
    /// Substring match across ticket, hand, run_id, kind, and body.
    Text(String),
    /// Exact prefix match on the ticket id field.
    Ticket(String),
    /// Exact prefix match on the hand id field.
    Hand(String),
    /// Exact prefix match on the run id field.
    Run(String),
}

impl ActivityFilter {
    /// Parse a raw query string (from the filter input) into an
    /// `ActivityFilter`. The empty string produces `None`.
    pub fn from_query(q: &str) -> Self {
        let q = q.trim();
        if q.is_empty() {
            return Self::None;
        }
        if let Some(rest) = q.strip_prefix("ticket:") {
            return Self::Ticket(rest.to_ascii_lowercase());
        }
        if let Some(rest) = q.strip_prefix("hand:") {
            return Self::Hand(rest.to_ascii_lowercase());
        }
        if let Some(rest) = q.strip_prefix("run:") {
            return Self::Run(rest.to_ascii_lowercase());
        }
        Self::Text(q.to_ascii_lowercase())
    }

    /// Short label for display in the UI (e.g. `"ticket:tst-1"`).
    /// Returns `None` when the filter is inactive.
    pub fn mode_label(&self) -> Option<String> {
        match self {
            Self::None => None,
            Self::Text(q) => Some(format!("text:{q}")),
            Self::Ticket(q) => Some(format!("ticket:{q}")),
            Self::Hand(q) => Some(format!("hand:{q}")),
            Self::Run(q) => Some(format!("run:{q}")),
        }
    }

    /// Whether this filter is inactive.
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    /// Returns `true` when `row` passes this filter.
    pub fn matches(&self, row: &EventRow) -> bool {
        match self {
            Self::None => true,
            Self::Text(q) => {
                row.ticket
                    .as_deref()
                    .is_some_and(|t| t.to_ascii_lowercase().contains(q.as_str()))
                    || row
                        .hand
                        .as_deref()
                        .is_some_and(|h| h.to_ascii_lowercase().contains(q.as_str()))
                    || row
                        .run_id
                        .as_deref()
                        .is_some_and(|r| r.to_ascii_lowercase().contains(q.as_str()))
                    || row.kind.contains(q.as_str())
                    || row.body.to_ascii_lowercase().contains(q.as_str())
            }
            Self::Ticket(q) => row
                .ticket
                .as_deref()
                .is_some_and(|t| t.to_ascii_lowercase().contains(q.as_str())),
            Self::Hand(q) => row
                .hand
                .as_deref()
                .is_some_and(|h| h.to_ascii_lowercase().contains(q.as_str())),
            Self::Run(q) => row
                .run_id
                .as_deref()
                .is_some_and(|r| r.to_ascii_lowercase().contains(q.as_str())),
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

/// Per-step aggregate token spend (across all runs).
#[derive(Clone, Debug, Default)]
pub struct StepTokenSummary {
    /// Pipeline step identifier (e.g. `"specify"`, `"plan"`, `"assay"`).
    pub step_id: String,
    /// Total input tokens consumed by this step across all recorded runs.
    pub tokens_in: u64,
    /// Total output tokens produced by this step across all recorded runs.
    pub tokens_out: u64,
    /// Total subprocess output bytes saved by compression in this step.
    pub bytes_saved: u64,
    /// Total output tokens saved by roughneck prompt injection in this step.
    pub roughneck_tokens_saved: u64,
}

/// Aggregate token spend summary derived from run manifests.
#[derive(Clone, Debug, Default)]
pub struct TokenSummary {
    /// All-time total input tokens.
    pub total_in: u64,
    /// All-time total output tokens.
    pub total_out: u64,
    /// Input tokens consumed by runs that started today (UTC day).
    pub today_in: u64,
    /// Output tokens produced by runs that started today (UTC day).
    pub today_out: u64,
    /// Per-step breakdown aggregated across all runs, sorted by step_id.
    pub per_step: Vec<StepTokenSummary>,
    /// Savings fraction in `[0.0, 1.0]`. `None` until a savings source is
    /// wired (RTK or otherwise).
    pub savings_pct: Option<f32>,
    /// Total raw subprocess output bytes across all recorded runs.
    pub total_bytes_raw: u64,
    /// Total bytes saved by output compression across all recorded runs.
    pub total_bytes_saved: u64,
    /// Total output tokens saved by roughneck prompt injection across all
    /// recorded runs.
    pub total_roughneck_saved: u64,
    /// Total output tokens from hand LLM calls (aggregated from substrate
    /// `hand stats:` note events).
    pub hands_tokens_out: u64,
    /// Total roughneck savings from hand calls.
    pub hands_roughneck_saved: u64,
    /// Total raw bytes from hand subprocess output.
    pub hands_bytes_raw: u64,
    /// Total bytes saved by scrub in hand subprocesses.
    pub hands_bytes_saved: u64,
}

/// Parse a `"hand stats: tokens_in=N tokens_out=N roughneck_saved=N bytes_raw=N bytes_saved=N"`
/// note body into `(tokens_in, tokens_out, roughneck_saved, bytes_raw, bytes_saved)`.
///
/// Returns `None` when the body does not start with `"hand stats:"` or when
/// any of the five required keys is missing or cannot be parsed as a `u64`.
/// Extra keys are ignored.
pub fn parse_hand_stats_note(body: &str) -> Option<(u64, u64, u64, u64, u64)> {
    let rest = body.strip_prefix("hand stats:")?.trim();
    let mut tokens_in: Option<u64> = None;
    let mut tokens_out: Option<u64> = None;
    let mut roughneck_saved: Option<u64> = None;
    let mut bytes_raw: Option<u64> = None;
    let mut bytes_saved: Option<u64> = None;
    for pair in rest.split_whitespace() {
        let (k, v) = pair.split_once('=')?;
        let v: u64 = v.parse().ok()?;
        match k {
            "tokens_in" => tokens_in = Some(v),
            "tokens_out" => tokens_out = Some(v),
            "roughneck_saved" => roughneck_saved = Some(v),
            "bytes_raw" => bytes_raw = Some(v),
            "bytes_saved" => bytes_saved = Some(v),
            _ => {}
        }
    }
    Some((
        tokens_in?,
        tokens_out?,
        roughneck_saved?,
        bytes_raw?,
        bytes_saved?,
    ))
}

/// One row in the Hands tab — a single hand's activity summary.
#[derive(Clone, Debug)]
pub struct HandRow {
    /// Hand identifier.
    pub hand_id: String,
    /// Ticket this hand is/was working on.
    pub ticket_id: Option<String>,
    /// Latest action observed (e.g. "dispatched", "completed", "failed").
    pub action: String,
    /// Timestamp of the most recent event for this hand.
    pub last_seen: DateTime<Utc>,
    /// Status badge: "running", "done", "failed", "unknown".
    pub status: String,
    /// One-line summary of notable output (commit hash, PR URL, error, etc.).
    pub detail: Option<String>,
}

/// Aggregate `TypedEvent`s into one `HandRow` per hand id, sorted newest-first
/// by `last_seen`.
///
/// `events` is expected newest-first (as returned by `tail_typed_events`).
/// Because we only set a `HandRow` field if it is empty, the most recent
/// value for each field wins.
fn build_hand_rows(events: &[TypedEvent]) -> Vec<HandRow> {
    let mut rows: HashMap<String, HandRow> = HashMap::new();
    // We also track whether any "failed" or "done" terminal event has been
    // seen for each hand, since these dominate the status badge regardless of
    // ordering.
    let mut terminal: HashMap<String, &'static str> = HashMap::new();

    for ev in events {
        // Determine the hand id this event belongs to, plus the action/detail
        // we should record.
        let (hand_id, action, ticket_id, detail, status_hint): (
            Option<String>,
            &'static str,
            Option<String>,
            Option<String>,
            Option<&'static str>,
        ) = match (&ev.scope, &ev.kind) {
            // Hand-scoped events
            (EventScope::Hand(h), EventKind::HandRegistered) => {
                (Some(h.to_string()), "registered", None, None, None)
            }
            (EventScope::Hand(h), EventKind::HandHeartbeat) => {
                (Some(h.to_string()), "heartbeat", None, None, None)
            }
            (EventScope::Hand(h), EventKind::HandAbandoned { previous_owner_of }) => (
                Some(h.to_string()),
                "abandoned",
                Some(previous_owner_of.to_string()),
                None,
                Some("failed"),
            ),
            // Ticket-scoped events that name a hand
            (EventScope::Ticket(tid), EventKind::TicketAssigned { hand }) => (
                Some(hand.to_string()),
                "dispatched",
                Some(tid.to_string()),
                None,
                None,
            ),
            (EventScope::Ticket(tid), EventKind::TicketUnassigned { reason }) => {
                // We don't know which hand released the ticket here, so this
                // event cannot update a HandRow.  We still record nothing.
                let _ = (tid, reason);
                (None, "", None, None, None)
            }
            // Hand-scoped notes ("claude hand: ...", "exited successfully",
            // "exited non-zero", "hand stats: ...", etc.)
            (EventScope::Hand(h), EventKind::Note { body }) => {
                if let Some(rest) = body.strip_prefix("hand stats:") {
                    (
                        Some(h.to_string()),
                        "stats recorded",
                        None,
                        Some(rest.trim().to_owned()),
                        None,
                    )
                } else if body.contains("exited successfully") {
                    (
                        Some(h.to_string()),
                        "completed",
                        None,
                        Some(body.clone()),
                        Some("done"),
                    )
                } else if body.contains("exited non-zero") || body.contains("failed") {
                    (
                        Some(h.to_string()),
                        "failed",
                        None,
                        Some(body.clone()),
                        Some("failed"),
                    )
                } else if let Some(rest) = body.strip_prefix("claude hand:") {
                    (
                        Some(h.to_string()),
                        "queue written",
                        None,
                        Some(rest.trim().to_owned()),
                        None,
                    )
                } else {
                    (Some(h.to_string()), "note", None, Some(body.clone()), None)
                }
            }
            _ => (None, "", None, None, None),
        };

        let Some(hand_id) = hand_id else { continue };
        if action.is_empty() {
            continue;
        }

        if let Some(s) = status_hint {
            // Failure is sticky: once we see a "failed" hint, it dominates
            // any later "done" we encounter (events are newest-first, so any
            // "failed" anywhere in the trail should override).
            let entry = terminal.entry(hand_id.clone()).or_insert(s);
            if s == "failed" {
                *entry = "failed";
            }
        }

        let row = rows.entry(hand_id.clone()).or_insert_with(|| HandRow {
            hand_id: hand_id.clone(),
            ticket_id: None,
            action: action.to_owned(),
            last_seen: ev.at,
            status: "running".to_owned(),
            detail: None,
        });
        // Newest-first: only fill empty fields.
        if row.ticket_id.is_none() {
            row.ticket_id = ticket_id;
        }
        if row.detail.is_none() {
            row.detail = detail;
        }
        // last_seen is the most recent event timestamp; first-seen wins
        // because events are newest-first.
        if ev.at > row.last_seen {
            row.last_seen = ev.at;
        }
    }

    // Apply terminal status overrides
    for (id, status) in &terminal {
        if let Some(row) = rows.get_mut(id) {
            row.status = (*status).to_owned();
        }
    }

    let mut out: Vec<HandRow> = rows.into_values().collect();
    out.sort_by_key(|r| std::cmp::Reverse(r.last_seen));
    out
}

// ---------------------------------------------------------------------------
// Minimal manifest deserialization — only the token fields we need.
// Lives here so derrick-tui does not depend on derrick-flow.
// ---------------------------------------------------------------------------

/// Deserializes only the token-relevant fields of a run manifest JSON file.
#[derive(Deserialize)]
struct ManifestTokens {
    tokens_in: u64,
    tokens_out: u64,
    started_at: DateTime<Utc>,
    #[serde(default)]
    steps: Vec<ManifestStepTokens>,
}

#[derive(Deserialize)]
struct ManifestStepTokens {
    id: String,
    #[serde(default)]
    tokens_in: u32,
    #[serde(default)]
    tokens_out: u32,
    #[serde(default)]
    bytes_raw: u32,
    #[serde(default)]
    bytes_saved: u32,
    #[serde(default)]
    roughneck_tokens_saved: u32,
}

/// Scan `runs_dir` for `manifest.json` files and aggregate token counts.
///
/// Returns a zeroed `TokenSummary` when the directory is absent or unreadable.
fn load_token_summary(runs_dir: &Path) -> TokenSummary {
    let today_start = Utc::now()
        .with_hour(0)
        .and_then(|t| t.with_minute(0))
        .and_then(|t| t.with_second(0))
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or_else(Utc::now);

    let mut total_in: u64 = 0;
    let mut total_out: u64 = 0;
    let mut today_in: u64 = 0;
    let mut today_out: u64 = 0;
    let mut total_bytes_raw: u64 = 0;
    let mut total_bytes_saved: u64 = 0;
    let mut total_roughneck_saved: u64 = 0;
    let mut per_step: HashMap<String, (u64, u64, u64, u64)> = HashMap::new();

    let Ok(dir_entries) = std::fs::read_dir(runs_dir) else {
        return TokenSummary::default();
    };

    for entry in dir_entries.flatten() {
        let manifest_path = entry.path().join("manifest.json");
        let Ok(content) = std::fs::read_to_string(&manifest_path) else {
            continue;
        };
        let Ok(manifest) = serde_json::from_str::<ManifestTokens>(&content) else {
            continue;
        };

        total_in = total_in.saturating_add(manifest.tokens_in);
        total_out = total_out.saturating_add(manifest.tokens_out);

        if manifest.started_at >= today_start {
            today_in = today_in.saturating_add(manifest.tokens_in);
            today_out = today_out.saturating_add(manifest.tokens_out);
        }

        for step in &manifest.steps {
            total_bytes_raw = total_bytes_raw.saturating_add(u64::from(step.bytes_raw));
            total_bytes_saved = total_bytes_saved.saturating_add(u64::from(step.bytes_saved));
            total_roughneck_saved =
                total_roughneck_saved.saturating_add(u64::from(step.roughneck_tokens_saved));
            let (si, so, bs, rn) = per_step.entry(step.id.clone()).or_default();
            *si = si.saturating_add(u64::from(step.tokens_in));
            *so = so.saturating_add(u64::from(step.tokens_out));
            *bs = bs.saturating_add(u64::from(step.bytes_saved));
            *rn = rn.saturating_add(u64::from(step.roughneck_tokens_saved));
        }
    }

    let mut per_step_vec: Vec<StepTokenSummary> = per_step
        .into_iter()
        .map(
            |(step_id, (tokens_in, tokens_out, bytes_saved, roughneck_tokens_saved))| {
                StepTokenSummary {
                    step_id,
                    tokens_in,
                    tokens_out,
                    bytes_saved,
                    roughneck_tokens_saved,
                }
            },
        )
        .collect();
    per_step_vec.sort_by(|a, b| a.step_id.cmp(&b.step_id));

    let savings_pct = if total_bytes_raw > 0 {
        Some(total_bytes_saved as f32 / total_bytes_raw as f32)
    } else {
        None
    };

    TokenSummary {
        total_in,
        total_out,
        today_in,
        today_out,
        per_step: per_step_vec,
        savings_pct,
        total_bytes_raw,
        total_bytes_saved,
        total_roughneck_saved,
        hands_tokens_out: 0,
        hands_roughneck_saved: 0,
        hands_bytes_raw: 0,
        hands_bytes_saved: 0,
    }
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
    /// Per-hand activity rollup for the Hands tab.
    pub hand_rows: Vec<HandRow>,
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

    /// Pulls fresh data from `substrate` and merges the injected stack,
    /// memory, and run-manifest state.
    ///
    /// `runs_dir` should point to the `.derrick/runs/` directory so that
    /// token counts can be aggregated from the per-run manifests. Pass
    /// `None` in tests or contexts where manifests are unavailable.
    pub async fn refresh(
        substrate: &dyn Substrate,
        stack_nodes: &[StackNode],
        memory_entries: &[MemoryEntry],
        runs_dir: Option<&Path>,
    ) -> Result<Self, SubstrateError> {
        let site = substrate.site().await?;
        let tickets = substrate.list_tickets(TicketFilter::default()).await?;
        let foreman = substrate.foreman_status().await?;
        let events = substrate.tail_typed_events(None, 100).await?;

        // Derive stack summary from the already-fetched stack nodes (no
        // extra I/O needed).
        let stack_summary = StackSummary::from_nodes(stack_nodes);

        // Scan the event tail for the most recent assay pipeline step.
        // `tail_typed_events` returns events newest-first, so the first match
        // is the most recent.
        let last_assay = events.iter().find_map(|ev| {
            if let EventKind::PipelineStepCompleted { step_id, status } = &ev.kind {
                if step_id == "assay" {
                    return Some(LastAssaySnapshot {
                        verdict: status.clone(),
                        model: None, // run-manifest plumbing deferred
                        at: ev.at,
                    });
                }
            }
            None
        });

        let mut overview = OverviewData {
            foreman_status: Some(foreman.into()),
            tickets_total: u32::try_from(tickets.len()).unwrap_or(u32::MAX),
            stack_summary,
            last_assay,
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

        let mut token_summary = runs_dir.map(load_token_summary).unwrap_or_default();
        // Aggregate hand stats recorded as substrate Note events.
        for ev in &events {
            if let EventKind::Note { body } = &ev.kind {
                if let Some((_ti, to, rn, br, bs)) = parse_hand_stats_note(body) {
                    token_summary.hands_tokens_out =
                        token_summary.hands_tokens_out.saturating_add(to);
                    token_summary.hands_roughneck_saved =
                        token_summary.hands_roughneck_saved.saturating_add(rn);
                    token_summary.hands_bytes_raw =
                        token_summary.hands_bytes_raw.saturating_add(br);
                    token_summary.hands_bytes_saved =
                        token_summary.hands_bytes_saved.saturating_add(bs);
                    token_summary.total_roughneck_saved =
                        token_summary.total_roughneck_saved.saturating_add(rn);
                    token_summary.total_out = token_summary.total_out.saturating_add(to);
                }
            }
        }

        let hand_rows = build_hand_rows(&events);

        Ok(Self {
            overview,
            tickets: ticket_rows,
            stack_nodes: stack_nodes.to_vec(),
            events: event_rows,
            token_summary,
            memory_entries: memory_entries.to_vec(),
            hand_rows,
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
        assert_eq!(Tab::from_index(7), None);
    }

    #[test]
    fn tab_title_covers_all_variants() {
        assert_eq!(Tab::Overview.title(), "Overview");
        assert_eq!(Tab::Tickets.title(), "Tickets");
        assert_eq!(Tab::Stack.title(), "Stack");
        assert_eq!(Tab::Activity.title(), "Activity");
        assert_eq!(Tab::Tokens.title(), "Tokens");
        assert_eq!(Tab::Memory.title(), "Memory");
        assert_eq!(Tab::Hands.title(), "Hands");
    }

    #[test]
    fn tab_all_has_seven_entries() {
        assert_eq!(Tab::all().len(), 7);
    }

    #[test]
    fn tab_hands_has_correct_index() {
        assert_eq!(Tab::Hands.index(), 6);
        assert_eq!(Tab::from_index(6), Some(Tab::Hands));
        assert_eq!(Tab::from_index(7), None);
    }

    #[test]
    fn tab_from_str_accepts_hands() {
        assert_eq!("hands".parse::<Tab>().ok(), Some(Tab::Hands));
        assert_eq!("HANDS".parse::<Tab>().ok(), Some(Tab::Hands));
    }

    // -----------------------------------------------------------------------
    // parse_hand_stats_note
    // -----------------------------------------------------------------------

    #[test]
    fn parse_hand_stats_note_valid() {
        let result = parse_hand_stats_note(
            "hand stats: tokens_in=100 tokens_out=200 roughneck_saved=300 bytes_raw=4096 bytes_saved=1024",
        );
        assert_eq!(result, Some((100, 200, 300, 4096, 1024)));
    }

    #[test]
    fn parse_hand_stats_note_invalid() {
        assert_eq!(parse_hand_stats_note("something else"), None);
        assert_eq!(parse_hand_stats_note("hand stats: broken"), None);
        // Missing required key
        assert_eq!(
            parse_hand_stats_note(
                "hand stats: tokens_in=1 tokens_out=2 roughneck_saved=3 bytes_raw=4"
            ),
            None
        );
    }

    // -----------------------------------------------------------------------
    // build_hand_rows
    // -----------------------------------------------------------------------

    #[test]
    fn build_hand_rows_from_events() {
        use derrick_substrate::{EventId, HandId, TicketId};

        let Ok(hand) = HandId::new("bramble") else {
            return;
        };
        let Ok(ticket) = TicketId::new("tst-7") else {
            return;
        };
        let now = chrono::Utc::now();

        let events = vec![
            // Newest first: ticket assignment to this hand.
            TypedEvent {
                id: EventId(2),
                scope: EventScope::Ticket(ticket.clone()),
                kind: EventKind::TicketAssigned { hand: hand.clone() },
                at: now,
            },
            // Hand was registered earlier.
            TypedEvent {
                id: EventId(1),
                scope: EventScope::Hand(hand.clone()),
                kind: EventKind::HandRegistered,
                at: now - chrono::Duration::seconds(60),
            },
        ];

        let rows = build_hand_rows(&events);
        assert_eq!(rows.len(), 1, "should aggregate to one hand row");
        let row = &rows[0];
        assert_eq!(row.hand_id, "bramble");
        assert_eq!(row.ticket_id.as_deref(), Some("tst-7"));
        assert_eq!(row.status, "running");
    }

    #[test]
    fn build_hand_rows_marks_failed() {
        use derrick_substrate::{EventId, HandId, TicketId};

        let Ok(hand) = HandId::new("cedar") else {
            return;
        };
        let Ok(ticket) = TicketId::new("tst-9") else {
            return;
        };
        let now = chrono::Utc::now();

        let events = vec![TypedEvent {
            id: EventId(1),
            scope: EventScope::Hand(hand.clone()),
            kind: EventKind::HandAbandoned {
                previous_owner_of: ticket,
            },
            at: now,
        }];

        let rows = build_hand_rows(&events);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "failed");
        assert_eq!(rows[0].action, "abandoned");
    }

    // -----------------------------------------------------------------------
    // StackSummary tests
    // -----------------------------------------------------------------------

    fn make_node(state: &str) -> StackNode {
        StackNode {
            ticket_id: "tst-1".to_owned(),
            branch: "feature/x".to_owned(),
            pr_url: None,
            pr_number: None,
            state: state.to_owned(),
            parent_branch: None,
        }
    }

    #[test]
    fn stack_summary_counts_merged_open_pending() {
        let nodes = vec![
            make_node("merged"),
            make_node("merged"),
            make_node("open"),
            make_node("draft"),
        ];
        let s = StackSummary::from_nodes(&nodes);
        assert_eq!(s.merged, 2);
        assert_eq!(s.open, 1);
        assert_eq!(s.pending, 1);
        assert!(s.restack_ok);
    }

    #[test]
    fn stack_summary_restack_false_on_conflict() {
        let nodes = vec![make_node("open"), make_node("conflict-rebase")];
        let s = StackSummary::from_nodes(&nodes);
        assert!(!s.restack_ok);
    }

    #[test]
    fn stack_summary_empty_nodes() {
        let s = StackSummary::from_nodes(&[]);
        assert_eq!(s.merged, 0);
        assert_eq!(s.open, 0);
        assert_eq!(s.pending, 0);
        assert!(s.restack_ok);
    }

    // -----------------------------------------------------------------------
    // EventRow::from scope extraction
    // -----------------------------------------------------------------------

    fn make_typed_event(scope: EventScope) -> TypedEvent {
        TypedEvent {
            id: derrick_substrate::EventId(1),
            scope,
            kind: EventKind::Note {
                body: "test".to_owned(),
            },
            at: chrono::Utc::now(),
        }
    }

    #[test]
    fn event_row_from_worktree_scope_sets_run_id() {
        let ev = make_typed_event(EventScope::Worktree {
            run_id: "run-abc".to_owned(),
        });
        let row = EventRow::from(&ev);
        assert_eq!(row.run_id.as_deref(), Some("run-abc"));
        assert!(row.ticket.is_none());
        assert!(row.hand.is_none());
    }

    #[test]
    fn event_row_from_hand_scope_sets_hand() {
        use derrick_substrate::HandId;
        let Ok(hand_id) = HandId::new("bramble") else {
            return; // skip if hand id validation changes
        };
        let ev = make_typed_event(EventScope::Hand(hand_id));
        let row = EventRow::from(&ev);
        assert!(row.ticket.is_none());
        assert_eq!(row.hand.as_deref(), Some("bramble"));
        assert!(row.run_id.is_none());
    }

    #[test]
    fn event_row_from_site_scope_all_none() {
        let ev = make_typed_event(EventScope::Site);
        let row = EventRow::from(&ev);
        assert!(row.ticket.is_none());
        assert!(row.hand.is_none());
        assert!(row.run_id.is_none());
    }

    // -----------------------------------------------------------------------
    // ActivityFilter mode_label for Ticket / Hand / Run
    // -----------------------------------------------------------------------

    #[test]
    fn mode_label_ticket() {
        assert_eq!(
            ActivityFilter::Ticket("tst-1".to_owned()).mode_label(),
            Some("ticket:tst-1".to_owned())
        );
    }

    #[test]
    fn mode_label_hand() {
        assert_eq!(
            ActivityFilter::Hand("bramble".to_owned()).mode_label(),
            Some("hand:bramble".to_owned())
        );
    }

    #[test]
    fn mode_label_run() {
        assert_eq!(
            ActivityFilter::Run("abc123".to_owned()).mode_label(),
            Some("run:abc123".to_owned())
        );
    }

    #[test]
    fn is_none_false_for_active_filter() {
        assert!(!ActivityFilter::Ticket("x".to_owned()).is_none());
        assert!(!ActivityFilter::Hand("y".to_owned()).is_none());
        assert!(!ActivityFilter::Run("z".to_owned()).is_none());
        assert!(!ActivityFilter::Text("q".to_owned()).is_none());
    }

    // -----------------------------------------------------------------------
    // load_token_summary
    // -----------------------------------------------------------------------

    #[test]
    fn load_token_summary_returns_default_for_missing_dir() {
        let summary = load_token_summary(std::path::Path::new("/nonexistent/path/xyzzy"));
        assert_eq!(summary.total_in, 0);
        assert_eq!(summary.total_out, 0);
    }

    #[test]
    fn load_token_summary_reads_manifest_files() {
        let tmp = std::env::temp_dir().join(format!("derrick-test-{}", std::process::id()));
        let run_dir = tmp.join("run-001");
        let _ = std::fs::create_dir_all(&run_dir);

        let manifest = serde_json::json!({
            "tokens_in": 1000,
            "tokens_out": 500,
            "started_at": chrono::Utc::now().to_rfc3339(),
            "steps": [
                {"id": "specify", "tokens_in": 400, "tokens_out": 200},
                {"id": "plan",    "tokens_in": 600, "tokens_out": 300}
            ]
        });
        let _ = std::fs::write(run_dir.join("manifest.json"), manifest.to_string());

        let summary = load_token_summary(&tmp);
        let _ = std::fs::remove_dir_all(&tmp);

        assert_eq!(summary.total_in, 1000);
        assert_eq!(summary.total_out, 500);
        // Both steps started today so today_in / today_out should be set.
        assert_eq!(summary.today_in, 1000);
        assert_eq!(summary.today_out, 500);
        assert_eq!(summary.per_step.len(), 2);
        let specify = summary.per_step.iter().find(|s| s.step_id == "specify");
        assert!(specify.is_some_and(|s| s.tokens_in == 400 && s.tokens_out == 200));
    }

    #[test]
    fn load_token_summary_skips_malformed_manifests() {
        let tmp = std::env::temp_dir().join(format!("derrick-test-mal-{}", std::process::id()));
        let run_dir = tmp.join("run-bad");
        let _ = std::fs::create_dir_all(&run_dir);
        let _ = std::fs::write(run_dir.join("manifest.json"), "not json at all");

        let summary = load_token_summary(&tmp);
        let _ = std::fs::remove_dir_all(&tmp);

        assert_eq!(summary.total_in, 0);
    }

    #[test]
    fn load_token_summary_aggregates_bytes_saved() {
        let tmp = std::env::temp_dir().join(format!("derrick-test-bytes-{}", std::process::id()));
        let run_dir = tmp.join("run-001");
        let _ = std::fs::create_dir_all(&run_dir);

        let manifest = serde_json::json!({
            "tokens_in": 100,
            "tokens_out": 50,
            "started_at": chrono::Utc::now().to_rfc3339(),
            "steps": [
                {
                    "id": "analyze",
                    "tokens_in": 0,
                    "tokens_out": 0,
                    "bytes_raw": 8192,
                    "bytes_saved": 2048
                },
                {
                    "id": "specify",
                    "tokens_in": 100,
                    "tokens_out": 50,
                    "bytes_raw": 0,
                    "bytes_saved": 0
                }
            ]
        });
        let _ = std::fs::write(run_dir.join("manifest.json"), manifest.to_string());

        let summary = load_token_summary(&tmp);
        let _ = std::fs::remove_dir_all(&tmp);

        assert_eq!(summary.total_bytes_raw, 8192);
        assert_eq!(summary.total_bytes_saved, 2048);
        // savings_pct should be set: 2048/8192 = 0.25
        assert!(summary.savings_pct.is_some_and(|p| (p - 0.25).abs() < 1e-4));
        // Per-step bytes_saved should be wired
        let analyze = summary.per_step.iter().find(|s| s.step_id == "analyze");
        assert!(analyze.is_some_and(|s| s.bytes_saved == 2048));
    }

    #[test]
    fn load_token_summary_aggregates_roughneck_saved() {
        let tmp = std::env::temp_dir().join(format!("derrick-test-rn-{}", std::process::id()));
        let run_dir = tmp.join("run-001");
        let _ = std::fs::create_dir_all(&run_dir);

        let manifest = serde_json::json!({
            "tokens_in": 100,
            "tokens_out": 50,
            "started_at": chrono::Utc::now().to_rfc3339(),
            "steps": [
                {
                    "id": "plan",
                    "tokens_in": 50,
                    "tokens_out": 25,
                    "roughneck_tokens_saved": 300
                },
                {
                    "id": "specify",
                    "tokens_in": 50,
                    "tokens_out": 25,
                    "roughneck_tokens_saved": 200
                }
            ]
        });
        let _ = std::fs::write(run_dir.join("manifest.json"), manifest.to_string());

        let summary = load_token_summary(&tmp);
        let _ = std::fs::remove_dir_all(&tmp);

        assert_eq!(summary.total_roughneck_saved, 500);
        let plan = summary.per_step.iter().find(|s| s.step_id == "plan");
        assert!(plan.is_some_and(|s| s.roughneck_tokens_saved == 300));
    }

    #[test]
    fn load_token_summary_savings_pct_none_when_no_bash_steps() {
        let tmp = std::env::temp_dir().join(format!("derrick-test-nopct-{}", std::process::id()));
        let run_dir = tmp.join("run-001");
        let _ = std::fs::create_dir_all(&run_dir);

        // Only token steps, no bytes_raw
        let manifest = serde_json::json!({
            "tokens_in": 100,
            "tokens_out": 50,
            "started_at": chrono::Utc::now().to_rfc3339(),
            "steps": [
                {"id": "specify", "tokens_in": 100, "tokens_out": 50}
            ]
        });
        let _ = std::fs::write(run_dir.join("manifest.json"), manifest.to_string());

        let summary = load_token_summary(&tmp);
        let _ = std::fs::remove_dir_all(&tmp);

        assert_eq!(summary.total_bytes_raw, 0);
        assert!(summary.savings_pct.is_none());
    }

    // -----------------------------------------------------------------------
    // ActivityFilter tests
    // -----------------------------------------------------------------------

    fn make_event_row(
        ticket: Option<&str>,
        hand: Option<&str>,
        run_id: Option<&str>,
        kind: &str,
        body: &str,
    ) -> EventRow {
        EventRow {
            at: chrono::Utc::now(),
            kind: kind.to_owned(),
            ticket: ticket.map(str::to_owned),
            hand: hand.map(str::to_owned),
            run_id: run_id.map(str::to_owned),
            body: body.to_owned(),
        }
    }

    #[test]
    fn from_query_empty_gives_none() {
        assert_eq!(ActivityFilter::from_query(""), ActivityFilter::None);
        assert_eq!(ActivityFilter::from_query("   "), ActivityFilter::None);
    }

    #[test]
    fn from_query_ticket_prefix() {
        assert_eq!(
            ActivityFilter::from_query("ticket:TST-1"),
            ActivityFilter::Ticket("tst-1".to_owned())
        );
    }

    #[test]
    fn from_query_hand_prefix() {
        assert_eq!(
            ActivityFilter::from_query("hand:Bramble"),
            ActivityFilter::Hand("bramble".to_owned())
        );
    }

    #[test]
    fn from_query_run_prefix() {
        assert_eq!(
            ActivityFilter::from_query("run:abc123"),
            ActivityFilter::Run("abc123".to_owned())
        );
    }

    #[test]
    fn from_query_plain_text() {
        assert_eq!(
            ActivityFilter::from_query("hello"),
            ActivityFilter::Text("hello".to_owned())
        );
    }

    #[test]
    fn filter_none_matches_everything() {
        let row = make_event_row(Some("tst-1"), None, None, "ticket_state_changed", "ready");
        assert!(ActivityFilter::None.matches(&row));
    }

    #[test]
    fn filter_ticket_matches_ticket_field() {
        let row = make_event_row(Some("tst-42"), None, None, "some_event", "body");
        assert!(ActivityFilter::Ticket("tst-42".to_owned()).matches(&row));
        // Substring match: "tst" should also find "tst-42"
        assert!(ActivityFilter::Ticket("tst".to_owned()).matches(&row));
        // Non-matching ticket
        assert!(!ActivityFilter::Ticket("tst-99".to_owned()).matches(&row));
        // hand-scoped row should not match a ticket filter
        let hand_row = make_event_row(None, Some("bramble"), None, "some_event", "body");
        assert!(!ActivityFilter::Ticket("tst".to_owned()).matches(&hand_row));
    }

    #[test]
    fn filter_hand_matches_hand_field() {
        let row = make_event_row(None, Some("bramble"), None, "ticket_assigned", "body");
        assert!(ActivityFilter::Hand("bramble".to_owned()).matches(&row));
        assert!(ActivityFilter::Hand("bram".to_owned()).matches(&row));
        assert!(!ActivityFilter::Hand("cedar".to_owned()).matches(&row));
    }

    #[test]
    fn filter_run_matches_run_id_field() {
        let row = make_event_row(
            None,
            None,
            Some("run-abc123"),
            "pipeline_step_completed",
            "ok",
        );
        assert!(ActivityFilter::Run("abc123".to_owned()).matches(&row));
        assert!(!ActivityFilter::Run("xyz".to_owned()).matches(&row));
    }

    #[test]
    fn filter_text_matches_across_all_fields() {
        // Matches ticket id
        let r1 = make_event_row(Some("tst-1"), None, None, "state_changed", "moved");
        assert!(ActivityFilter::Text("tst".to_owned()).matches(&r1));
        // Matches kind
        let r2 = make_event_row(None, None, None, "pipeline_step_completed", "done");
        assert!(ActivityFilter::Text("pipeline".to_owned()).matches(&r2));
        // Matches body
        let r3 = make_event_row(None, None, None, "event", "moved to review");
        assert!(ActivityFilter::Text("review".to_owned()).matches(&r3));
        // Matches hand
        let r4 = make_event_row(None, Some("cedar"), None, "event", "body");
        assert!(ActivityFilter::Text("cedar".to_owned()).matches(&r4));
        // No match
        let r5 = make_event_row(Some("tst-1"), None, None, "event", "nothing here");
        assert!(!ActivityFilter::Text("zebra".to_owned()).matches(&r5));
    }

    #[test]
    fn mode_label_none_is_none() {
        assert_eq!(ActivityFilter::None.mode_label(), None);
    }

    #[test]
    fn mode_label_text_prefix() {
        assert_eq!(
            ActivityFilter::Text("hello".to_owned()).mode_label(),
            Some("text:hello".to_owned())
        );
    }
}
