//! Foreman loop. See DESIGN.md §8.6 and ticket T012.
//!
//! The foreman owns the periodic state-machine maintenance pass for crew
//! mode: it cleans up abandoned worktrees and hands, verifies `InReview`
//! tickets against git (D31), reconciles `Ready` tickets whose work merged
//! externally (D33), unblocks dependency-blocked tickets, and dispatches
//! `Ready` tickets to a `HandDispatcher`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use derrick_config::{Config, Stacking};
use derrick_stack::{NoneStackBackend, RestackOutcome, RestackParams, StackBackend};
use derrick_substrate::{
    BlockReason, EventKind, EventLog, EventScope, Hand, HandId, HandRegistry, InReviewMetadata,
    SubstrateError, Ticket, TicketId, TicketState, TicketStore,
};
use futures::future::join_all;
use thiserror::Error;
use tokio::process::Command;
use tracing::{info, warn};

use crate::{NativeSubstrate, WorktreeRecord};

/// Returns `true` when a process with `pid` is currently alive (D75). Used by
/// the cleanup pass to decide whether a stale-heartbeat hand is still actually
/// running its agent child.
///
/// Unix uses `kill(pid, 0)`: it returns `0` if the process exists and is
/// signalable by us, `-1` otherwise. `ESRCH` (no such process) and `EPERM`
/// (exists, not ours) both produce `-1`; for our own children `EPERM` is
/// impossible, and a reused pid owned by another user means our child is gone
/// — so `== 0` is the only "alive" signal. This avoids needing libc's errno
/// constants or adding a `libc`/`nix` dependency (house rule #2).
///
/// Windows falls back to `false` (process probing is part of the v1.1 Windows
/// support track) so the heartbeat TTL remains authoritative there.
#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe { kill(pid as i32, 0) == 0 }
}

#[cfg(not(unix))]
fn process_alive(_pid: u32) -> bool {
    false
}

/// Whether a hand should be treated as still running. This is the exact
/// inverse of the abandonment predicate the cleanup pass applies in step 1c:
/// a dead pid is authoritative (never live), otherwise the hand is live if its
/// pid is alive or its heartbeat is fresh relative to `hand_threshold`. Kept as
/// a shared helper so the worktree-prune guard (step 1a) and the hand-release
/// pass (step 1c) can never drift apart.
fn hand_is_live(hand: &Hand, hand_threshold: chrono::DateTime<Utc>) -> bool {
    let pid_dead = hand.pid.is_some_and(|pid| !process_alive(pid));
    if pid_dead {
        return false;
    }
    let live_pid = hand.pid.is_some_and(process_alive);
    let fresh_heartbeat = hand.last_seen.is_some_and(|seen| seen >= hand_threshold);
    live_pid || fresh_heartbeat
}

/// Lightweight cross-reference against git + GitHub PR state.
///
/// Per D33 the verifier trusts git history over PR metadata. The trait
/// exposes both so the verifier can reconcile the two when they disagree
/// (e.g. squash-merge or force-push).
#[async_trait]
pub trait RepoState: Send + Sync {
    /// Is `head_sha` present on `target_branch`'s ancestry as of now?
    async fn target_contains_sha(
        &self,
        target_branch: &str,
        head_sha: &str,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>>;

    /// PR state for `branch`. Used to distinguish "still open" from
    /// "actively rejected" when `target_contains_sha` is false.
    async fn pr_status(
        &self,
        branch: &str,
    ) -> Result<PrStatus, Box<dyn std::error::Error + Send + Sync>>;

    /// Merge SHA the PR reports, when gh reports merged. The verifier
    /// still confirms via `target_contains_sha`.
    async fn pr_merge_sha(
        &self,
        branch: &str,
    ) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>>;
}

/// PR lifecycle state as reported by gh.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PrStatus {
    /// The PR is open.
    Open,
    /// The PR is merged.
    Merged,
    /// The PR was closed without merging.
    ClosedUnmerged,
    /// gh reports no PR for this branch.
    NotFound,
}

/// Production `RepoState` impl that shells out to `git` and `gh`.
///
/// Each call spawns a subprocess; the implementation is deliberately
/// stateless so callers can construct fresh instances per tick.
pub struct GhRepoState {
    repo_root: PathBuf,
}

impl GhRepoState {
    /// Construct a `GhRepoState` rooted at `repo_root` (the directory
    /// containing the `.git` folder).
    pub fn new(repo_root: PathBuf) -> Self {
        Self { repo_root }
    }
}

#[async_trait]
impl RepoState for GhRepoState {
    async fn target_contains_sha(
        &self,
        target_branch: &str,
        head_sha: &str,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        // `git merge-base --is-ancestor <sha> <ref>` exits 0 if ancestor.
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.repo_root)
            .arg("merge-base")
            .arg("--is-ancestor")
            .arg(head_sha)
            .arg(target_branch)
            .output()
            .await
            .map_err(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>)?;
        Ok(output.status.success())
    }

    async fn pr_status(
        &self,
        branch: &str,
    ) -> Result<PrStatus, Box<dyn std::error::Error + Send + Sync>> {
        let output = Command::new("gh")
            .arg("pr")
            .arg("view")
            .arg(branch)
            .arg("--json")
            .arg("state")
            .arg("-q")
            .arg(".state")
            .current_dir(&self.repo_root)
            .output()
            .await
            .map_err(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>)?;
        if !output.status.success() {
            return Ok(PrStatus::NotFound);
        }
        let state = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        match state.as_str() {
            "OPEN" => Ok(PrStatus::Open),
            "MERGED" => Ok(PrStatus::Merged),
            "CLOSED" => Ok(PrStatus::ClosedUnmerged),
            _ => Ok(PrStatus::NotFound),
        }
    }

    async fn pr_merge_sha(
        &self,
        branch: &str,
    ) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> {
        let output = Command::new("gh")
            .arg("pr")
            .arg("view")
            .arg(branch)
            .arg("--json")
            .arg("mergeCommit")
            .arg("-q")
            .arg(".mergeCommit.oid")
            .current_dir(&self.repo_root)
            .output()
            .await
            .map_err(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>)?;
        if !output.status.success() {
            return Ok(None);
        }
        let sha = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if sha.is_empty() {
            Ok(None)
        } else {
            Ok(Some(sha))
        }
    }
}

/// Result of a successful `HandDispatcher::dispatch` call.
#[derive(Clone, Debug)]
pub struct DispatchResult {
    /// The hand the dispatcher reserved for the ticket.
    pub hand: HandId,
    /// `true` when the dispatcher synchronously moved the ticket to
    /// `InReview` (rare; used by human hands that complete work in-process).
    pub completed_synchronously: bool,
}

/// Errors returned by `HandDispatcher::dispatch`.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum DispatchError {
    /// The dispatcher kind has no implementation in v1; T013 wires up the
    /// real Copilot adapter.
    #[error("dispatcher kind {kind} not implemented in v1; see T013")]
    NotImplemented {
        /// Dispatcher kind identifier.
        kind: &'static str,
    },
    /// Substrate-side error during dispatch.
    #[error("substrate error: {0}")]
    Substrate(#[from] SubstrateError),
    /// I/O error during dispatch (e.g. spawning a child process).
    #[error("dispatch io: {0}")]
    Io(std::io::Error),
}

/// Context passed to [`HandDispatcher::dispatch`]. Replaces the previous
/// two-argument (ticket, worktree_root) call pattern so the foreman can
/// thread stacking information through without breaking the trait boundary.
pub struct DispatchContext<'a> {
    /// Ticket being dispatched.
    pub ticket: &'a Ticket,
    /// Worktree root the dispatcher should operate inside.
    pub worktree_root: &'a Path,
    /// Parent branch for this ticket's stack position. Equals the foreman's
    /// `target_branch` (default `"main"`) for roots; otherwise the computed
    /// branch name of the highest-`ordinal` predecessor.
    pub parent_branch: String,
}

/// Trait T013 will implement against. T012 ships two stubs:
/// [`HumanHandDispatcher`] and [`CopilotStubDispatcher`].
#[async_trait]
pub trait HandDispatcher: Send + Sync {
    /// Identifier for telemetry; matches `derrick.yaml` hand kind
    /// (`claude` | `copilot` | `human`).
    fn kind(&self) -> &'static str;

    /// Reserve a hand for `ctx.ticket` and start the work. See trait docs in
    /// T012 for the contract.
    async fn dispatch(&self, ctx: &DispatchContext<'_>) -> Result<DispatchResult, DispatchError>;
}

/// Best-effort removal of a per-ticket hand worktree directory via
/// `git worktree remove --force`, rooted at `repo_root`. Shared by the local
/// hand dispatchers so they stay behaviourally identical; the row is forgotten
/// separately via [`NativeSubstrate::forget_ticket_worktree`]. Logs on failure
/// (the foreman TTL cleanup pass is the backstop) and never propagates.
pub async fn prune_ticket_worktree_dir(repo_root: &Path, worktree_path: &Path) {
    let result = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["worktree", "remove", "--force"])
        .arg(worktree_path)
        .kill_on_drop(true)
        .output()
        .await;
    match result {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            warn!(
                worktree = %worktree_path.display(),
                %stderr,
                "git worktree remove failed during hand worktree cleanup"
            );
        }
        Err(error) => {
            warn!(
                ?error,
                worktree = %worktree_path.display(),
                "failed to spawn git worktree remove during hand worktree cleanup"
            );
        }
    }
}

/// Human-hand dispatcher: emits a `Note` event marking the ticket ready
/// for human work and leaves it `InFlight` until the user runs
/// `derrick ticket review`.
pub struct HumanHandDispatcher {
    substrate: Arc<NativeSubstrate>,
    hand: HandId,
}

impl HumanHandDispatcher {
    /// Construct a human dispatcher pinned to `hand`.
    pub fn new(substrate: Arc<NativeSubstrate>, hand: HandId) -> Self {
        Self { substrate, hand }
    }
}

#[async_trait]
impl HandDispatcher for HumanHandDispatcher {
    fn kind(&self) -> &'static str {
        "human"
    }

    async fn dispatch(&self, ctx: &DispatchContext<'_>) -> Result<DispatchResult, DispatchError> {
        let ticket = ctx.ticket;
        // Atomically Ready -> InFlight + owner = self.hand.
        self.substrate
            .assign_to_hand(&ticket.id, &self.hand)
            .await?;
        // Surface the human task in the activity log.
        self.substrate
            .record_typed_event(
                EventScope::Ticket(ticket.id.clone()),
                EventKind::Note {
                    body: format!("human hand: ticket {} is ready for work", ticket.id),
                },
            )
            .await?;
        Ok(DispatchResult {
            hand: self.hand.clone(),
            completed_synchronously: false,
        })
    }
}

/// Copilot dispatcher placeholder. Returns `DispatchError::NotImplemented`
/// pointing at T013.
///
/// Superseded in T013 by `derrick_copilot::CopilotHandDispatcher`. Retained
/// for one release so downstream code that still references the stub keeps
/// compiling.
#[deprecated(since = "0.1.0", note = "Use derrick_copilot::CopilotHandDispatcher")]
pub struct CopilotStubDispatcher;

#[allow(deprecated)]
impl CopilotStubDispatcher {
    /// Construct the stub. No state needed.
    pub fn new() -> Self {
        Self
    }
}

#[allow(deprecated)]
impl Default for CopilotStubDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
#[allow(deprecated)]
impl HandDispatcher for CopilotStubDispatcher {
    fn kind(&self) -> &'static str {
        "copilot"
    }

    async fn dispatch(&self, _ctx: &DispatchContext<'_>) -> Result<DispatchResult, DispatchError> {
        Err(DispatchError::NotImplemented { kind: "copilot" })
    }
}

/// Dispatcher façade that routes each ticket to one of several registered
/// dispatchers based on a `kind:<name>` label. Used by crew mode when both
/// Copilot and Claude hands are enabled (T015).
///
/// Selection rule: if a ticket carries a label `kind:<name>` where `<name>`
/// matches a registered dispatcher's [`HandDispatcher::kind`], that
/// dispatcher is used. Otherwise the dispatcher registered with the
/// constructor's `default_kind` is used. If no dispatcher matches the
/// default kind the first registered dispatcher is used.
pub struct MultiDispatcher {
    dispatchers: Vec<Box<dyn HandDispatcher>>,
    default_kind: &'static str,
}

impl MultiDispatcher {
    /// Construct a new `MultiDispatcher` whose fallback dispatcher is
    /// identified by `default_kind`.
    pub fn new(default_kind: &'static str) -> Self {
        Self {
            dispatchers: Vec::new(),
            default_kind,
        }
    }

    /// Register a dispatcher. Order is not meaningful except as the
    /// last-resort fallback when no other selector matches.
    #[must_use]
    pub fn register(mut self, dispatcher: Box<dyn HandDispatcher>) -> Self {
        self.dispatchers.push(dispatcher);
        self
    }

    /// Returns `true` when no dispatchers have been registered.
    pub fn is_empty(&self) -> bool {
        self.dispatchers.is_empty()
    }

    fn select(&self, ticket: &Ticket) -> Option<&dyn HandDispatcher> {
        for label in &ticket.labels {
            if let Some(name) = label.strip_prefix("kind:") {
                if let Some(d) = self.dispatchers.iter().find(|d| d.kind() == name) {
                    return Some(d.as_ref());
                }
            }
        }
        if let Some(d) = self
            .dispatchers
            .iter()
            .find(|d| d.kind() == self.default_kind)
        {
            return Some(d.as_ref());
        }
        self.dispatchers.first().map(std::convert::AsRef::as_ref)
    }
}

#[async_trait]
impl HandDispatcher for MultiDispatcher {
    fn kind(&self) -> &'static str {
        "multi"
    }

    async fn dispatch(&self, ctx: &DispatchContext<'_>) -> Result<DispatchResult, DispatchError> {
        match self.select(ctx.ticket) {
            Some(dispatcher) => dispatcher.dispatch(ctx).await,
            None => Err(DispatchError::NotImplemented { kind: "multi" }),
        }
    }
}

/// Errors returned by `Foreman::tick`.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum ForemanError {
    /// Substrate-side error during tick.
    #[error("substrate error: {0}")]
    Substrate(#[from] SubstrateError),
    /// `RepoState` returned an error during a verifier check.
    #[error("repo state check failed: {0}")]
    RepoState(Box<dyn std::error::Error + Send + Sync>),
    /// I/O error during cleanup (e.g. `git worktree remove`).
    #[error("io error at {path}: {source}")]
    Io {
        /// Path the error occurred on.
        path: PathBuf,
        /// Source I/O error.
        source: std::io::Error,
    },
}

/// Structured report of what a single `tick` performed. Returned so callers
/// (tests, CLI `foreman tick`, observability) can audit the pass.
#[derive(Clone, Debug, Default)]
pub struct TickReport {
    /// Cleanup actions taken in step 1.
    pub cleanup_actions: Vec<CleanupAction>,
    /// Verifier outcomes from step 2 and 3.
    pub verifier_actions: Vec<VerifierAction>,
    /// Tickets unblocked in step 4.
    pub unblocked: Vec<TicketId>,
    /// Tickets dispatched in step 5.
    pub dispatched: Vec<TicketId>,
}

/// One cleanup action performed during step 1.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum CleanupAction {
    /// An abandoned worktree row was deleted.
    PrunedAbandonedWorktree {
        /// Run id of the pruned worktree.
        run_id: String,
    },
    /// A ticket owned by a stale hand was released to `Ready`.
    RequeuedAbandonedHand {
        /// Ticket released.
        ticket: TicketId,
        /// Hand that had been owning it.
        hand: HandId,
    },
    /// A stale `InReview` ticket was added to the eager verifier queue.
    TriggeredStaleInReviewCheck {
        /// Ticket added.
        ticket: TicketId,
    },
}

/// One verifier outcome from step 2 or step 3.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum VerifierAction {
    /// `InReview` ticket merged: transitioned to `Done`.
    Merged {
        /// Ticket transitioned.
        ticket: TicketId,
        /// Merge SHA recorded.
        merge_sha: String,
    },
    /// `InReview` ticket landed `Blocked` (D32 — PR closed unmerged).
    Unmerged {
        /// Ticket transitioned.
        ticket: TicketId,
        /// Human-readable reason.
        reason: String,
    },
    /// Ticket left in place; verifier emitted an escalation event.
    StuckEscalated {
        /// Ticket escalated.
        ticket: TicketId,
    },
    /// Re-queued `Ready` ticket reconciled to `Done` from git (D33).
    ReconciledFromGit {
        /// Ticket reconciled.
        ticket: TicketId,
        /// Merge SHA recorded.
        merge_sha: String,
    },
    /// Dependent ticket restacked after its parent merged.
    Restacked {
        /// Dependent ticket whose branch moved.
        ticket: TicketId,
        /// Dependent's branch.
        branch: String,
    },
    /// Restack conflict: dependent ticket was blocked with a recipe for
    /// human resolution.
    RestackConflict {
        /// Dependent ticket transitioned to `Blocked`.
        ticket: TicketId,
        /// Human-runnable `git rebase --onto` recipe.
        recipe: String,
    },
}

/// Per-tick configuration knobs sourced from `tools.foreman` in
/// `derrick.yaml`.
#[derive(Clone, Debug)]
pub struct ForemanTtls {
    /// Time between `tick()` iterations when running attached.
    pub poll_interval: Duration,
    /// Maximum age of an `InReview` ticket before the verifier eagerly
    /// re-checks it.
    pub in_review_ttl: chrono::Duration,
    /// Maximum gap since a hand's last heartbeat before the cleanup pass
    /// releases its tickets.
    pub hand_ttl: chrono::Duration,
    /// Maximum age of an open worktree row before the cleanup pass prunes
    /// it.
    pub worktree_ttl: chrono::Duration,
}

impl Default for ForemanTtls {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(10),
            in_review_ttl: chrono::Duration::hours(24),
            hand_ttl: chrono::Duration::minutes(30),
            worktree_ttl: chrono::Duration::hours(24),
        }
    }
}

/// The foreman loop. Construct one per process and either call `tick()`
/// directly (tests, CLI `foreman tick`) or `run_attached()` to enter the
/// poll loop.
pub struct Foreman {
    substrate: Arc<NativeSubstrate>,
    target_branch: String,
    repo_state: Box<dyn RepoState>,
    repo_root: PathBuf,
    dispatcher: Box<dyn HandDispatcher>,
    batch_max: u32,
    ttls: ForemanTtls,
    exit_when_idle: bool,
    stack_backend: Arc<dyn StackBackend>,
    stacking_config: Stacking,
}

impl Foreman {
    /// Construct a `Foreman` from a substrate, config, repo-state adapter,
    /// repo root, and dispatcher. TTLs default to the values in
    /// `ForemanTtls::default`.
    pub fn new(
        substrate: Arc<NativeSubstrate>,
        config: Config,
        repo_state: Box<dyn RepoState>,
        repo_root: PathBuf,
        dispatcher: Box<dyn HandDispatcher>,
    ) -> Self {
        let batch_max = config.parallelism().batch_max();
        let stacking_config = config.tools().git().stacking().clone();
        Self {
            substrate,
            target_branch: "main".to_owned(),
            repo_state,
            repo_root,
            dispatcher,
            batch_max,
            ttls: ForemanTtls::default(),
            exit_when_idle: false,
            stack_backend: Arc::new(NoneStackBackend),
            stacking_config,
        }
    }

    /// Override the stack backend and stacking config. Callers that want
    /// auto-restack must opt in by installing a backend that supports it.
    pub fn with_stack_backend(mut self, backend: Arc<dyn StackBackend>, config: Stacking) -> Self {
        self.stack_backend = backend;
        self.stacking_config = config;
        self
    }

    /// Override TTL configuration.
    pub fn with_ttls(mut self, ttls: ForemanTtls) -> Self {
        self.ttls = ttls;
        self
    }

    /// Override the target branch (defaults to `"main"`).
    pub fn with_target_branch(mut self, branch: impl Into<String>) -> Self {
        self.target_branch = branch.into();
        self
    }

    /// Set the `exit_when_idle` flag. When `true`, `run_attached` returns
    /// after the first tick that produced no actions.
    pub fn with_exit_when_idle(mut self, exit: bool) -> Self {
        self.exit_when_idle = exit;
        self
    }

    /// Override the dispatch parallelism cap. Defaults to
    /// `config.parallelism().batch_max()`.
    pub fn with_batch_max(mut self, batch_max: u32) -> Self {
        self.batch_max = batch_max;
        self
    }

    /// Run a single tick. Public so tests and the CLI can drive it
    /// deterministically.
    pub async fn tick(&self) -> Result<TickReport, ForemanError> {
        let mut report = TickReport::default();

        // Step 1: cleanup pass.
        self.cleanup_pass(&mut report).await?;

        // Step 2: verifier pass over all InReview tickets.
        let inreview = self.substrate.list_inreview_ticket_ids().await?;
        for ticket_id in inreview {
            self.verify_in_review_ticket(&ticket_id, &mut report)
                .await?;
        }

        // Step 3: D33 pre-dispatch reconciliation.
        let ready_with_history = self
            .substrate
            .list_ready_tickets_with_inreview_history()
            .await?;
        for ticket in ready_with_history {
            self.reconcile_ready_ticket(&ticket, &mut report).await?;
        }

        // Step 4: unblock dependency-blocked tickets whose predecessors
        // are all terminal.
        self.unblock_dependencies(&mut report).await?;

        // Step 5: dispatch up to batch_max - inflight tickets.
        self.dispatch_ready(&mut report).await?;

        Ok(report)
    }

    /// Run `tick()` in a foreground loop until shutdown signal or
    /// `exit_when_idle` becomes true and a tick produced no actions.
    pub async fn run_attached(&self) -> Result<(), ForemanError> {
        let mut sigterm = signal_stream_or_err(tokio::signal::unix::SignalKind::terminate())?;
        let mut sigint = signal_stream_or_err(tokio::signal::unix::SignalKind::interrupt())?;
        loop {
            let report = self.tick().await?;
            if self.exit_when_idle && report_is_idle(&report) {
                return Ok(());
            }
            tokio::select! {
                _ = tokio::time::sleep(self.ttls.poll_interval) => {}
                _ = sigterm.recv() => return Ok(()),
                _ = sigint.recv() => return Ok(()),
            }
        }
    }

    async fn cleanup_pass(&self, report: &mut TickReport) -> Result<(), ForemanError> {
        let now = Utc::now();

        // 1a: prune abandoned worktrees past TTL.
        let worktree_threshold = now - self.ttls.worktree_ttl;
        let hand_threshold = now - self.ttls.hand_ttl;
        let stale_worktrees = self
            .substrate
            .list_stale_open_worktrees(worktree_threshold)
            .await?;
        // Fetch hands once so the per-worktree liveness check below is a plain
        // in-memory lookup rather than a query per row.
        let hands_for_worktrees = self.substrate.list_hands().await?;
        for record in stale_worktrees {
            // D32: TTL cleanup reconciles *crashed* runs — it must never evict a
            // live one. A heavy `ticket:`-keyed checkout whose owning ticket is
            // still non-terminal and whose owning hand is still alive (live pid
            // or fresh heartbeat) is working past the worktree TTL, not
            // abandoned. Leave it in place; it gets reclaimed when the ticket
            // reaches a terminal state or the hand dies.
            if self
                .worktree_still_live(&record, &hands_for_worktrees, hand_threshold)
                .await?
            {
                info!(
                    run_id = %record.run_id,
                    "skipping worktree prune: owning ticket non-terminal and hand alive"
                );
                continue;
            }
            // Try to remove the on-disk worktree; swallow not-found errors
            // but propagate genuine I/O failures.
            let _ = Command::new("git")
                .arg("-C")
                .arg(&self.repo_root)
                .arg("worktree")
                .arg("remove")
                .arg("--force")
                .arg(&record.path)
                .output()
                .await
                .map_err(|source| ForemanError::Io {
                    path: record.path.clone(),
                    source,
                })?;
            self.substrate.delete_worktree_row(&record.run_id).await?;
            self.substrate
                .record_typed_event(
                    EventScope::Worktree {
                        run_id: record.run_id.clone(),
                    },
                    EventKind::WorktreeAbandoned {
                        run_id: record.run_id.clone(),
                        reason: "cleanup pass: worktree past TTL".to_owned(),
                    },
                )
                .await?;
            report
                .cleanup_actions
                .push(CleanupAction::PrunedAbandonedWorktree {
                    run_id: record.run_id,
                });
        }

        // 1b: walk .derrick/.adopt-stage-* directories past TTL.
        self.cleanup_adopt_stage_dirs(report, worktree_threshold)
            .await?;

        // 1c: release tickets owned by hands whose agent process is gone or
        //     whose heartbeat is stale (D75/D32). A dead pid is authoritative
        //     — abandon immediately even if the heartbeat is fresh. A live pid
        //     suppresses TTL abandonment when the heartbeat is merely stale
        //     (the agent is still running, just busy). Hands with no pid keep
        //     the existing heartbeat-TTL behaviour.
        // Reuse the single `hand_threshold` computed for step 1a rather than
        // recomputing it, so the two passes cannot drift.
        let all_hands = self.substrate.list_hands().await?;
        for hand in all_hands {
            // Abandon a hand iff it is not live. Routed through the shared
            // `hand_is_live` helper (the exact inverse) so this pass and the
            // step 1a worktree-prune guard can never drift apart. `pid_dead` is
            // still needed below to phrase the abandonment reason.
            let pid_dead = hand.pid.is_some_and(|pid| !process_alive(pid));
            if hand_is_live(&hand, hand_threshold) {
                continue;
            }
            let inflight = self
                .substrate
                .list_inflight_tickets_owned_by(&hand.id)
                .await?;
            for ticket_id in inflight {
                let reason = match (pid_dead, hand.pid) {
                    (true, Some(pid)) => {
                        format!("hand abandoned: child process {pid} is no longer alive")
                    }
                    _ => format!(
                        "hand abandoned: last seen before {}",
                        hand_threshold.to_rfc3339()
                    ),
                };
                self.substrate.release_from_hand(&ticket_id, reason).await?;
                self.substrate
                    .record_typed_event(
                        EventScope::Hand(hand.id.clone()),
                        EventKind::HandAbandoned {
                            previous_owner_of: ticket_id.clone(),
                        },
                    )
                    .await?;
                report
                    .cleanup_actions
                    .push(CleanupAction::RequeuedAbandonedHand {
                        ticket: ticket_id,
                        hand: hand.id.clone(),
                    });
            }
        }

        // 1d: list stale InReview tickets (so the verifier pass can pick
        // them up this tick rather than waiting another poll cycle).
        let inreview_threshold = now - self.ttls.in_review_ttl;
        let stale_inreview = self
            .substrate
            .list_stale_inreview_tickets(inreview_threshold)
            .await?;
        for ticket_id in stale_inreview {
            report
                .cleanup_actions
                .push(CleanupAction::TriggeredStaleInReviewCheck { ticket: ticket_id });
        }

        Ok(())
    }

    async fn cleanup_adopt_stage_dirs(
        &self,
        _report: &mut TickReport,
        threshold: chrono::DateTime<Utc>,
    ) -> Result<(), ForemanError> {
        let derrick_dir = self.repo_root.join(".derrick");
        let entries = match std::fs::read_dir(&derrick_dir) {
            Ok(iter) => iter,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(source) => {
                return Err(ForemanError::Io {
                    path: derrick_dir,
                    source,
                });
            }
        };
        for entry in entries {
            let entry = entry.map_err(|source| ForemanError::Io {
                path: derrick_dir.clone(),
                source,
            })?;
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if !name.starts_with(".adopt-stage-") {
                continue;
            }
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(source) => {
                    return Err(ForemanError::Io {
                        path: path.clone(),
                        source,
                    });
                }
            };
            if !metadata.is_dir() {
                continue;
            }
            let modified = match metadata.modified() {
                Ok(time) => time,
                Err(source) => {
                    return Err(ForemanError::Io {
                        path: path.clone(),
                        source,
                    });
                }
            };
            let modified: chrono::DateTime<Utc> = modified.into();
            if modified >= threshold {
                continue;
            }
            if let Err(source) = std::fs::remove_dir_all(&path) {
                return Err(ForemanError::Io {
                    path: path.clone(),
                    source,
                });
            }
            self.substrate
                .record_typed_event(
                    EventScope::Site,
                    EventKind::Note {
                        body: format!("removed stale adopt stage dir: {}", path.display()),
                    },
                )
                .await?;
        }
        Ok(())
    }

    async fn verify_in_review_ticket(
        &self,
        id: &TicketId,
        report: &mut TickReport,
    ) -> Result<(), ForemanError> {
        let Some(metadata) = self.substrate.most_recent_in_review_metadata(id).await? else {
            // No InReview event recorded — leave it alone; this is the
            // pre-D33 case where a ticket landed in InReview without
            // metadata. The eager-verifier escalation below covers TTL.
            return Ok(());
        };
        let InReviewMetadata {
            branch,
            pr_url,
            head_sha,
            ..
        } = metadata;

        // Fast-forward path: head SHA on target.
        if self
            .repo_state
            .target_contains_sha(&self.target_branch, &head_sha)
            .await
            .map_err(ForemanError::RepoState)?
        {
            let merge_sha = if pr_url.is_some() {
                self.repo_state
                    .pr_merge_sha(&branch)
                    .await
                    .map_err(ForemanError::RepoState)?
                    .unwrap_or_else(|| head_sha.clone())
            } else {
                head_sha.clone()
            };
            self.substrate
                .verify_ticket_merged(id, head_sha, merge_sha.clone())
                .await?;
            report.verifier_actions.push(VerifierAction::Merged {
                ticket: id.clone(),
                merge_sha: merge_sha.clone(),
            });
            self.restack_dependents(id, &branch, &merge_sha, report)
                .await?;
            return Ok(());
        }

        // PR-driven paths: consult gh.
        let status = self
            .repo_state
            .pr_status(&branch)
            .await
            .map_err(ForemanError::RepoState)?;
        match status {
            PrStatus::Merged => {
                // Squash/rebase: head_sha not on target, but PR reports
                // merged. Confirm the merge commit lives on target.
                let merge_sha = self
                    .repo_state
                    .pr_merge_sha(&branch)
                    .await
                    .map_err(ForemanError::RepoState)?;
                if let Some(sha) = merge_sha {
                    let on_target = self
                        .repo_state
                        .target_contains_sha(&self.target_branch, &sha)
                        .await
                        .map_err(ForemanError::RepoState)?;
                    if on_target {
                        self.substrate
                            .verify_ticket_merged(id, head_sha, sha.clone())
                            .await?;
                        report.verifier_actions.push(VerifierAction::Merged {
                            ticket: id.clone(),
                            merge_sha: sha.clone(),
                        });
                        self.restack_dependents(id, &branch, &sha, report).await?;
                        return Ok(());
                    }
                }
                // gh says merged but target doesn't have the SHA. Escalate
                // and leave the ticket InReview (D33 prefers loud over
                // silent).
                self.substrate
                    .record_typed_event(
                        EventScope::Ticket(id.clone()),
                        EventKind::EscalationStuckInReview {
                            ticket: id.clone(),
                            branch: branch.clone(),
                        },
                    )
                    .await?;
                report
                    .verifier_actions
                    .push(VerifierAction::StuckEscalated { ticket: id.clone() });
            }
            PrStatus::ClosedUnmerged => {
                self.substrate
                    .verify_ticket_unmerged(id, branch.clone(), pr_url.clone())
                    .await?;
                report.verifier_actions.push(VerifierAction::Unmerged {
                    ticket: id.clone(),
                    reason: format!("pr closed unmerged: {branch}"),
                });
            }
            PrStatus::NotFound => {
                // If past the TTL, escalate.
                let ticket = self.substrate.get_ticket(id).await?.ok_or_else(|| {
                    SubstrateError::NotFound {
                        kind: "ticket",
                        id: id.to_string(),
                    }
                })?;
                let threshold = Utc::now() - self.ttls.in_review_ttl;
                if ticket.updated_at < threshold {
                    self.substrate
                        .record_typed_event(
                            EventScope::Ticket(id.clone()),
                            EventKind::EscalationStuckInReview {
                                ticket: id.clone(),
                                branch: branch.clone(),
                            },
                        )
                        .await?;
                    report
                        .verifier_actions
                        .push(VerifierAction::StuckEscalated { ticket: id.clone() });
                }
            }
            PrStatus::Open => {
                // Leave alone; rechecked next tick.
            }
        }
        Ok(())
    }

    async fn reconcile_ready_ticket(
        &self,
        ticket: &Ticket,
        report: &mut TickReport,
    ) -> Result<(), ForemanError> {
        let Some(metadata) = self
            .substrate
            .most_recent_in_review_metadata(&ticket.id)
            .await?
        else {
            return Ok(());
        };
        let InReviewMetadata {
            branch,
            pr_url,
            head_sha,
            ..
        } = metadata;

        // Fast-forward path.
        if self
            .repo_state
            .target_contains_sha(&self.target_branch, &head_sha)
            .await
            .map_err(ForemanError::RepoState)?
        {
            let merge_sha = if pr_url.is_some() {
                self.repo_state
                    .pr_merge_sha(&branch)
                    .await
                    .map_err(ForemanError::RepoState)?
                    .unwrap_or_else(|| head_sha.clone())
            } else {
                head_sha.clone()
            };
            self.substrate
                .reconcile_ticket_done_from_git(&ticket.id, head_sha, merge_sha.clone())
                .await?;
            report
                .verifier_actions
                .push(VerifierAction::ReconciledFromGit {
                    ticket: ticket.id.clone(),
                    merge_sha: merge_sha.clone(),
                });
            self.restack_dependents(&ticket.id, &branch, &merge_sha, report)
                .await?;
            return Ok(());
        }

        // Squash-merge path.
        let status = self
            .repo_state
            .pr_status(&branch)
            .await
            .map_err(ForemanError::RepoState)?;
        if status != PrStatus::Merged {
            return Ok(());
        }
        let Some(sha) = self
            .repo_state
            .pr_merge_sha(&branch)
            .await
            .map_err(ForemanError::RepoState)?
        else {
            return Ok(());
        };
        let on_target = self
            .repo_state
            .target_contains_sha(&self.target_branch, &sha)
            .await
            .map_err(ForemanError::RepoState)?;
        if !on_target {
            return Ok(());
        }
        self.substrate
            .reconcile_ticket_done_from_git(&ticket.id, head_sha, sha.clone())
            .await?;
        report
            .verifier_actions
            .push(VerifierAction::ReconciledFromGit {
                ticket: ticket.id.clone(),
                merge_sha: sha.clone(),
            });
        self.restack_dependents(&ticket.id, &branch, &sha, report)
            .await?;
        Ok(())
    }

    async fn unblock_dependencies(&self, report: &mut TickReport) -> Result<(), ForemanError> {
        let candidates = self.substrate.list_dependency_blocked_ticket_ids().await?;
        for ticket_id in candidates {
            let predecessors = self.substrate.blocks_predecessors(&ticket_id).await?;
            let mut all_terminal = true;
            for predecessor in &predecessors {
                let pred = self.substrate.get_ticket(predecessor).await?;
                let Some(pred) = pred else {
                    all_terminal = false;
                    break;
                };
                if !pred.state.is_terminal() {
                    all_terminal = false;
                    break;
                }
            }
            if !all_terminal {
                continue;
            }
            // Skip the vacuous-predecessor case: if there are no recorded
            // `blocks` predecessors, the dependency block is malformed
            // — leave it for human triage instead of silently unblocking.
            if predecessors.is_empty() {
                continue;
            }
            self.substrate.unblock_ticket(&ticket_id).await?;
            report.unblocked.push(ticket_id);
        }
        Ok(())
    }

    async fn compute_parent_branch(&self, ticket: &Ticket) -> Result<String, ForemanError> {
        let predecessors = self.substrate.blocks_predecessors(&ticket.id).await?;
        if predecessors.is_empty() {
            return Ok(self.target_branch.clone());
        }
        let mut pred_tickets = Vec::with_capacity(predecessors.len());
        for pred_id in &predecessors {
            if let Some(pred) = self.substrate.get_ticket(pred_id).await? {
                pred_tickets.push(pred);
            }
        }
        Ok(derrick_stack::parent_branch_for(
            &predecessors,
            &pred_tickets,
            &self.target_branch,
            self.stacking_config.branch_pattern(),
        ))
    }

    /// After a ticket merges, restack any dependents from the merged
    /// branch onto `target_branch`. Honours the
    /// `tools.git.stacking.auto_restack_on_merge` flag and the configured
    /// stack backend.
    async fn restack_dependents(
        &self,
        merged_ticket_id: &TicketId,
        merged_branch: &str,
        merge_sha: &str,
        report: &mut TickReport,
    ) -> Result<(), ForemanError> {
        if !self.stacking_config.auto_restack_on_merge() {
            return Ok(());
        }
        let dependents = self.substrate.blocks_dependents(merged_ticket_id).await?;
        for dependent_id in dependents {
            let Some(dependent) = self.substrate.get_ticket(&dependent_id).await? else {
                continue;
            };
            // Only restack dependents whose work is still on a feature
            // branch (InFlight while a hand is open, InReview while a PR is
            // live). Terminal or Blocked dependents are left alone.
            if !matches!(
                dependent.state,
                derrick_substrate::TicketState::InFlight | derrick_substrate::TicketState::InReview
            ) {
                continue;
            }
            let Some(metadata) = self
                .substrate
                .most_recent_in_review_metadata(&dependent_id)
                .await?
            else {
                continue;
            };
            let dependent_branch = metadata.branch.clone();
            let params = RestackParams {
                branch: dependent_branch.clone(),
                old_parent: merged_branch.to_owned(),
                new_parent: self.target_branch.clone(),
                repo_root: self.repo_root.clone(),
            };
            let outcome = self.stack_backend.restack(params).await.map_err(|error| {
                ForemanError::RepoState(Box::new(std::io::Error::other(error.to_string()))
                    as Box<dyn std::error::Error + Send + Sync>)
            })?;
            match outcome {
                RestackOutcome::Restacked => {
                    if let Err(error) = self
                        .stack_backend
                        .force_push(&dependent_branch, &self.repo_root)
                        .await
                    {
                        warn!(
                            ticket = %dependent_id,
                            branch = %dependent_branch,
                            error = %error,
                            "force-push after restack failed",
                        );
                        self.substrate
                            .record_typed_event(
                                EventScope::Ticket(dependent_id.clone()),
                                EventKind::Note {
                                    body: format!(
                                        "restack succeeded but force-push failed: {error}"
                                    ),
                                },
                            )
                            .await?;
                        continue;
                    }
                    // Move the child PR's GitHub base onto the new target.
                    // A git rebase alone leaves the open PR comparing against
                    // the merged (now-deleted) parent branch; the base must be
                    // retargeted via gh. Backends that don't support it (e.g.
                    // `none`) report NotSupported — warn and continue, like the
                    // force-push gate above, rather than failing the whole pass.
                    if let Err(error) = self
                        .stack_backend
                        .retarget_pr(&dependent_branch, &self.target_branch, &self.repo_root)
                        .await
                    {
                        warn!(
                            ticket = %dependent_id,
                            branch = %dependent_branch,
                            error = %error,
                            "retarget pr base after restack failed",
                        );
                        self.substrate
                            .record_typed_event(
                                EventScope::Ticket(dependent_id.clone()),
                                EventKind::Note {
                                    body: format!(
                                        "restacked {dependent_branch} but retarget of PR base failed: {error}"
                                    ),
                                },
                            )
                            .await?;
                    }
                    info!(
                        ticket = %dependent_id,
                        branch = %dependent_branch,
                        parent_merge_sha = %merge_sha,
                        "restacked dependent onto target",
                    );
                    self.substrate
                        .record_typed_event(
                            EventScope::Ticket(dependent_id.clone()),
                            EventKind::Note {
                                body: format!(
                                    "restacked {dependent_branch} from {merged_branch} onto {target} after merge {merge_sha}",
                                    target = self.target_branch,
                                ),
                            },
                        )
                        .await?;
                    report.verifier_actions.push(VerifierAction::Restacked {
                        ticket: dependent_id.clone(),
                        branch: dependent_branch,
                    });
                }
                RestackOutcome::Conflict { recipe } => {
                    warn!(
                        ticket = %dependent_id,
                        branch = %dependent_branch,
                        recipe = %recipe,
                        "restack conflict; blocking dependent",
                    );
                    self.substrate
                        .block_ticket(
                            &dependent_id,
                            BlockReason::RestackConflict {
                                recipe: recipe.clone(),
                            },
                        )
                        .await?;
                    self.substrate
                        .record_typed_event(
                            EventScope::Ticket(dependent_id.clone()),
                            EventKind::Note {
                                body: format!("restack conflict; resolve manually with: {recipe}"),
                            },
                        )
                        .await?;
                    report
                        .verifier_actions
                        .push(VerifierAction::RestackConflict {
                            ticket: dependent_id.clone(),
                            recipe,
                        });
                }
                // RestackOutcome is `#[non_exhaustive]`; treat any future
                // variants as a no-op until they get explicit handling.
                _ => {}
            }
        }
        Ok(())
    }

    /// Re-read a ticket at the substrate boundary and report whether it has
    /// actually left `Ready`. Used as the post-dispatch backstop so the
    /// no-double-dispatch guarantee is owned by an observed state transition
    /// rather than by dispatcher convention. A ticket that has vanished (was
    /// deleted mid-tick) counts as "left Ready" — there is nothing to
    /// re-dispatch.
    async fn ticket_left_ready(&self, id: &TicketId) -> Result<bool, ForemanError> {
        Ok(match self.substrate.get_ticket(id).await? {
            Some(ticket) => ticket.state != TicketState::Ready,
            None => true,
        })
    }

    /// Whether a stale worktree row belongs to a still-live run and so must be
    /// preserved rather than pruned by the TTL sweep (FIX 2 / D32).
    ///
    /// Only `ticket:`-keyed rows (per-ticket hand checkouts) can be live: a row
    /// is live iff its owning ticket is non-terminal AND its owning hand is
    /// still alive (live pid or fresh heartbeat). Run-keyed rows, orphaned rows
    /// (ticket gone / unowned / owner no longer registered), terminal tickets,
    /// and dead/absent hands all return `false` so the existing prune behaviour
    /// applies.
    async fn worktree_still_live(
        &self,
        record: &WorktreeRecord,
        hands: &[Hand],
        hand_threshold: chrono::DateTime<Utc>,
    ) -> Result<bool, ForemanError> {
        let Some(ticket_key) = record.run_id.strip_prefix("ticket:") else {
            return Ok(false);
        };
        let Ok(ticket_id) = TicketId::new(ticket_key) else {
            // Malformed key we would never have written; treat as prunable.
            return Ok(false);
        };
        let Some(ticket) = self.substrate.get_ticket(&ticket_id).await? else {
            return Ok(false);
        };
        if ticket.state.is_terminal() {
            return Ok(false);
        }
        let Some(owner) = ticket.owner else {
            return Ok(false);
        };
        let Some(hand) = hands.iter().find(|hand| hand.id == owner) else {
            return Ok(false);
        };
        Ok(hand_is_live(hand, hand_threshold))
    }

    async fn dispatch_ready(&self, report: &mut TickReport) -> Result<(), ForemanError> {
        // D92: `batch_max` bounds ACTIVE HANDS, not total resource footprint.
        // `count_inflight_tickets` counts only `InFlight` tickets (hands
        // actually running); tickets in `InReview` still hold their worktree
        // and open PR by design but are deliberately NOT counted against the
        // cap. The budget below therefore limits concurrent active workers,
        // not the number of non-terminal tickets holding resources. See
        // DESIGN.md §9.C.
        let inflight = self.substrate.count_inflight_tickets().await?;
        let cap = u64::from(self.batch_max);
        if inflight >= cap {
            return Ok(());
        }
        let budget = (cap - inflight) as usize;
        let ready = self.substrate.list_ready_tickets_ordered().await?;

        // Phase 1 (sequential): resolve each ticket's parent branch. These are
        // fast substrate reads and must happen before dispatch so the dispatcher
        // receives accurate stack context.
        let mut candidates: Vec<(Ticket, String)> = Vec::with_capacity(budget);
        for ticket in ready.into_iter().take(budget) {
            let parent_branch = self.compute_parent_branch(&ticket).await?;
            candidates.push((ticket, parent_branch));
        }

        // Phase 2 (concurrent): fan-out all dispatch futures. Build the
        // `DispatchContext` values first (before constructing futures) so
        // each future can borrow a live `DispatchContext` from `contexts`
        // for its entire poll lifetime. `HandDispatcher` is `Send + Sync`,
        // so concurrent borrows of `self.dispatcher` are safe. `join_all`
        // does not require `'static` — it polls all futures on the current
        // task, which is sufficient for I/O-bound dispatchers.
        let contexts: Vec<DispatchContext<'_>> = candidates
            .iter()
            .map(|(ticket, parent_branch)| DispatchContext {
                ticket,
                worktree_root: &self.repo_root,
                parent_branch: parent_branch.clone(),
            })
            .collect();
        let results = join_all(contexts.iter().map(|ctx| self.dispatcher.dispatch(ctx))).await;

        // Phase 3: process results and update report. Errors on individual
        // tickets propagate using the same policy as the sequential version.
        for ((ticket, _), result) in candidates.iter().zip(results) {
            match result {
                Ok(_) => {
                    // Backstop for the no-double-dispatch guarantee (D31/D32
                    // spirit). A dispatcher returning `Ok` is NOT sufficient
                    // evidence the ticket was claimed: the guarantee must not
                    // rest on dispatcher convention alone. Verify at the
                    // substrate boundary that the ticket actually left `Ready`.
                    // If it is still `Ready`, a misbehaving (or crashed-mid-
                    // dispatch) hand reported success without calling
                    // `assign_to_hand`; counting it as dispatched would let the
                    // next tick re-dispatch it, spawning duplicate
                    // hands/worktrees and colliding branches.
                    if self.ticket_left_ready(&ticket.id).await? {
                        report.dispatched.push(ticket.id.clone());
                    } else {
                        warn!(
                            ticket = %ticket.id,
                            "dispatcher reported success but ticket is still Ready; \
                             not counting as claimed (no assign_to_hand observed)"
                        );
                        self.substrate
                            .record_typed_event(
                                EventScope::Ticket(ticket.id.clone()),
                                EventKind::Note {
                                    body: format!(
                                        "dispatch reported success but ticket {} never left \
                                         Ready; not claimed (dispatcher failed to assign it to a \
                                         hand)",
                                        ticket.id
                                    ),
                                },
                            )
                            .await?;
                    }
                }
                Err(DispatchError::NotImplemented { kind }) => {
                    self.substrate
                        .record_typed_event(
                            EventScope::Ticket(ticket.id.clone()),
                            EventKind::Note {
                                body: format!(
                                    "dispatcher kind {kind} not implemented in v1; see T013"
                                ),
                            },
                        )
                        .await?;
                }
                Err(DispatchError::Substrate(error)) => return Err(error.into()),
                Err(DispatchError::Io(source)) => {
                    return Err(ForemanError::Io {
                        path: self.repo_root.clone(),
                        source,
                    });
                }
            }
        }
        Ok(())
    }
}

fn report_is_idle(report: &TickReport) -> bool {
    report.cleanup_actions.is_empty()
        && report.verifier_actions.is_empty()
        && report.unblocked.is_empty()
        && report.dispatched.is_empty()
}

fn signal_stream_or_err(
    kind: tokio::signal::unix::SignalKind,
) -> Result<tokio::signal::unix::Signal, ForemanError> {
    tokio::signal::unix::signal(kind).map_err(|source| ForemanError::Io {
        path: PathBuf::from("<signal>"),
        source,
    })
}

#[cfg(test)]
mod tests;
