//! `derrick-hand` — a generic host-CLI [`HandDispatcher`].
//!
//! [`HostCliHandDispatcher`] drives any of derrick's host CLIs (codex,
//! opencode, aider, …) as a crew-mode executor hand. It mirrors the shape of
//! `derrick_copilot::LocalCopilotHandDispatcher`: per ticket it creates a git
//! worktree on the ticket's branch (rooted at the foreman-supplied parent
//! branch) and runs the host CLI there, so concurrently dispatched hands never
//! share a checkout or index. It routes through the shared [`HostRegistry`]
//! instead of spawning a process directly, so model forwarding and per-host
//! CLI normalisation (D65) happen inside the adapter.
//!
//! The model id is threaded RAW (`provider/model` or a bare id) into
//! [`HostRequest::model`]; the adapter calls `catalogue::normalize` per host.
//! Crucially, a ticket never reaches `Done` from a hand self-report: this
//! dispatcher only ever moves `Ready → InFlight` (via `assign_to_hand`) and
//! leaves the merge-observed transition to the foreman's verifier (D31/D32).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use derrick_memory::LessonIndex;

use async_trait::async_trait;
use chrono::Utc;
use derrick_substrate::{
    Complexity, EventKind, EventScope, Hand, HandExitStats, HandId, HandKind, Substrate,
    SubstrateError, Ticket,
};
use derrick_substrate_native::NativeSubstrate;
use derrick_substrate_native::foreman::{
    DispatchContext, DispatchError, DispatchResult, HandDispatcher, prune_ticket_worktree_dir,
};
use derrick_tools::{
    HostError, HostRegistry, HostRequest, ModelChoice, OutputSink, PidSink, Tier, select_model,
};
use tokio::process::Command;
use tracing::{info, instrument, warn};

/// Default heartbeat / poll interval while a host CLI runs.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(60);
/// Default wall-clock ceiling for a single host CLI invocation.
const DEFAULT_POLL_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// Throttle window for `HandProgress` events (D76): at most one progress
/// event per hand per 2 seconds, and only when the latest output line differs
/// from the last emitted snippet.
const HAND_PROGRESS_WINDOW: Duration = Duration::from_secs(2);
/// Maximum display width of a `HandProgress` snippet (D76). Longer lines are
/// truncated with an ellipsis.
const HAND_PROGRESS_SNIPPET_MAX: usize = 80;

/// Throttles `HandProgress` emission to at most one event per
/// [`HAND_PROGRESS_WINDOW`] and only on meaningful-change (D76). Pure logic —
/// unit-tested independently of the substrate.
struct ProgressThrottler {
    last_emit: Option<Instant>,
    last_snippet: String,
    window: Duration,
}

impl ProgressThrottler {
    fn new(window: Duration) -> Self {
        Self {
            last_emit: None,
            last_snippet: String::new(),
            window,
        }
    }

    /// Returns `Some(snippet)` when both (a) `>= window` has elapsed since the
    /// last emit and (b) `snippet` differs from the last emitted value. Returns
    /// `None` otherwise (suppress).
    fn consider(&mut self, now: Instant, snippet: &str) -> Option<String> {
        let changed = snippet != self.last_snippet;
        let elapsed = self
            .last_emit
            .is_none_or(|t| now.duration_since(t) >= self.window);
        if changed && elapsed {
            self.last_emit = Some(now);
            self.last_snippet = snippet.to_owned();
            Some(snippet.to_owned())
        } else {
            None
        }
    }
}

/// Truncates `line` to [`HAND_PROGRESS_SNIPPET_MAX`] display columns, appending
/// an ellipsis when truncated. Used for `HandProgress` snippets (D76).
fn truncate_snippet(line: &str) -> String {
    if line.chars().count() <= HAND_PROGRESS_SNIPPET_MAX {
        line.to_owned()
    } else {
        let truncated: String = line.chars().take(HAND_PROGRESS_SNIPPET_MAX.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}


/// Runtime configuration for [`HostCliHandDispatcher`].
#[derive(Clone, Debug)]
pub struct HostCliHandDispatcherConfig {
    /// When true, invoke the host CLI as part of dispatch; otherwise just
    /// record a note and leave the ticket `InFlight` for an operator.
    pub auto_dispatch: bool,
    /// Heartbeat interval (currently advisory; reserved for a future
    /// background poll loop).
    pub poll_interval: Duration,
    /// Wall-clock ceiling applied to the host CLI invocation.
    pub poll_timeout: Duration,
    /// Stable identity prefix used when minting hand ids.
    pub agent_identity: String,
    /// Prefix applied to dispatch branch names (`<prefix>/<batch>/<id>`).
    pub branch_prefix: String,
    /// Repository root used as the source for `git worktree add` (absolute).
    pub repo_root: PathBuf,
    /// Directory under which per-ticket worktrees are created (absolute).
    /// Each ticket gets its own checkout so concurrently dispatched host
    /// CLIs (codex/opencode/aider) never share an index or working tree.
    pub worktree_root: PathBuf,
    /// Whether to prepend Roughneck instructions to the prompt body.
    pub roughneck_enabled: bool,
    /// Roughneck level: "lite", "full", or "ultra".
    pub roughneck_level: String,
    /// Pre-loaded lesson index for retrieval injection (§9.A.4). When
    /// present, up to [`derrick_memory::LESSON_RETRIEVAL_LIMIT`] relevant
    /// lessons are appended to the ticket prompt. `None` skips injection
    /// and adds zero tokens.
    pub lesson_index: Option<Arc<LessonIndex>>,
}

impl Default for HostCliHandDispatcherConfig {
    fn default() -> Self {
        Self {
            auto_dispatch: true,
            poll_interval: DEFAULT_POLL_INTERVAL,
            poll_timeout: DEFAULT_POLL_TIMEOUT,
            agent_identity: "derrick-hand".to_owned(),
            branch_prefix: "derrick".to_owned(),
            repo_root: PathBuf::from("."),
            worktree_root: PathBuf::from(".derrick/host-worktrees"),
            roughneck_enabled: true,
            roughneck_level: "full".to_owned(),
            lesson_index: None,
        }
    }
}

/// A [`HandDispatcher`] that drives a single named host CLI as a crew
/// executor. See the crate docs for the lifecycle contract.
pub struct HostCliHandDispatcher {
    substrate: Arc<NativeSubstrate>,
    hosts: Arc<HostRegistry>,
    host_name: &'static str,
    hand_kind: HandKind,
    model_choice: ModelChoice,
    config: HostCliHandDispatcherConfig,
}

/// Map a ticket's optional [`Complexity`] to a [`Tier`] for adaptive model
/// selection (D67). `None` and `Standard` both resolve to `Standard`.
fn tier_for(complexity: Option<Complexity>) -> Tier {
    match complexity {
        Some(Complexity::Low) => Tier::Light,
        Some(Complexity::Heavy) => Tier::Heavy,
        // `None`, `Standard`, and any future variant default to Standard.
        _ => Tier::Standard,
    }
}

impl HostCliHandDispatcher {
    /// Construct a dispatcher for `host_name`, registering hands of
    /// `hand_kind`. The per-ticket model is resolved at dispatch time from
    /// `model_choice` and the ticket's complexity (D67).
    pub fn new(
        substrate: Arc<NativeSubstrate>,
        hosts: Arc<HostRegistry>,
        host_name: &'static str,
        hand_kind: HandKind,
        model_choice: ModelChoice,
        config: HostCliHandDispatcherConfig,
    ) -> Self {
        Self {
            substrate,
            hosts,
            host_name,
            hand_kind,
            model_choice,
            config,
        }
    }

    fn target_branch(&self, ticket: &Ticket) -> String {
        let batch = ticket
            .batch
            .as_ref()
            .map(derrick_substrate::BatchName::as_str)
            .unwrap_or("ad-hoc");
        format!("{}/{}/{}", self.config.branch_prefix, batch, ticket.id)
    }

    fn mint_hand_id(&self) -> Result<HandId, SubstrateError> {
        // HandId pattern: ^[a-z][a-z0-9-]{0,63}$. Sanitize the configured
        // identity, then append a short suffix derived from the wall clock.
        let mut cleaned: String = self
            .config
            .agent_identity
            .chars()
            .filter_map(|c| {
                if c.is_ascii_alphanumeric() {
                    Some(c.to_ascii_lowercase())
                } else if c == '-' {
                    Some('-')
                } else {
                    None
                }
            })
            .collect();
        let starts_with_letter = cleaned
            .chars()
            .next()
            .map(|c| c.is_ascii_lowercase())
            .unwrap_or(false);
        if !starts_with_letter {
            cleaned.insert_str(0, "h-");
        }
        let suffix = short_suffix();
        let mut raw = format!("{cleaned}-{suffix}");
        raw.truncate(64);
        HandId::new(raw)
    }

    /// Render the prompt body the host CLI receives. Inlined for MVP rather
    /// than depending on `derrick-claude`'s queue renderer. The branch is
    /// already checked out in `worktree`, so the agent works there directly
    /// rather than creating a branch itself.
    fn render_body(
        &self,
        ticket: &Ticket,
        branch: &str,
        parent_branch: &str,
        worktree: &std::path::Path,
    ) -> String {
        let batch_display = ticket
            .batch
            .as_ref()
            .map(derrick_substrate::BatchName::as_str)
            .unwrap_or("(none)");
        let worktree_display = worktree.display();
        let body = format!(
            "# Derrick ticket: {title}\n\n\
             You are implementing a ticket dispatched by derrick's crew-mode foreman.\n\
             Complete ALL steps below in order.\n\n\
             ## Ticket metadata\n\
             - **ID**: {id}\n\
             - **Batch**: {batch_display}\n\
             - **Branch**: `{branch}`\n\
             - **Base**: `{parent_branch}`\n\
             - **Worktree**: `{worktree_display}`\n\n\
             ## Specification\n\n{spec}\n\n\
             ## Required steps\n\n\
             1. Work inside the worktree `{worktree_display}`. The branch `{branch}` is\n   \
             already checked out there.\n\
             2. Implement the specification above and commit your changes.\n\
             3. Push the branch.\n\
             4. Run `derrick ticket review {id} --branch {branch} --head-sha <sha>`\n   \
             so the foreman's verifier can observe the merge. Do NOT mark the\n   \
             ticket done yourself.\n",
            title = ticket.title,
            id = ticket.id,
            spec = ticket.body,
        );
        // Inject relevant lessons (§9.A.4). Query uses the ticket title and id
        // to find tag matches; empty index → no block, zero tokens added.
        let query = format!("{} {}", ticket.id, ticket.title);
        let body = if let Some(index) = &self.config.lesson_index {
            derrick_memory::inject_lessons_into_prompt(&body, index, &query)
        } else {
            body
        };
        if self.config.roughneck_enabled {
            derrick_roughneck::inject_prompt(&body, &self.config.roughneck_level)
        } else {
            body
        }
    }

    /// Create the per-ticket worktree on `branch`, rooted at `parent_branch`,
    /// under `self.config.worktree_root`. Mirrors `LocalCopilotHandDispatcher`:
    /// each ticket gets its own checkout so concurrent host CLIs never share an
    /// index. Returns the worktree path on success.
    ///
    /// Lifecycle note: like `LocalCopilotHandDispatcher`, the worktree is created
    /// via raw `git worktree add` (not the substrate `reserve_worktree` row), so
    /// the foreman TTL pruning pass does not track it and the checkout persists
    /// after a *successful* dispatch. On failure it is removed (see `dispatch`).
    /// Adding success-path cleanup for both local hand dispatchers is a tracked
    /// follow-up; it is intentionally out of scope here so the two dispatchers
    /// stay behaviourally identical.
    async fn ensure_worktree(
        &self,
        ticket: &Ticket,
        branch: &str,
        parent_branch: &str,
    ) -> Result<PathBuf, DispatchError> {
        tokio::fs::create_dir_all(&self.config.worktree_root)
            .await
            .map_err(DispatchError::Io)?;
        let worktree_path = self.config.worktree_root.join(ticket.id.as_str());
        if !worktree_path.join(".git").exists() {
            let output = Command::new("git")
                .args(["worktree", "add", "-B", branch])
                .arg(&worktree_path)
                .arg(parent_branch)
                .current_dir(&self.config.repo_root)
                .kill_on_drop(true)
                .output()
                .await
                .map_err(DispatchError::Io)?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                return Err(DispatchError::Io(std::io::Error::other(format!(
                    "git worktree add failed: {stderr}"
                ))));
            }
        }
        Ok(worktree_path)
    }

    /// Remove a per-ticket worktree (on-disk directory + tracked substrate row)
    /// once it is no longer needed — a terminal ticket state or a released hand.
    /// Best-effort: logs but does not propagate failures, since the foreman TTL
    /// cleanup pass is the backstop for anything left behind.
    async fn cleanup_ticket_worktree(&self, ticket_id: &str, worktree_path: &std::path::Path) {
        prune_ticket_worktree_dir(&self.config.repo_root, worktree_path).await;
        if let Err(error) = self.substrate.forget_ticket_worktree(ticket_id).await {
            warn!(
                ?error,
                ticket = %ticket_id,
                "forget_ticket_worktree failed during host CLI cleanup"
            );
        }
    }
}

fn short_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:06x}", (nanos as u64) & 0x00ff_ffff)
}

#[async_trait]
impl HandDispatcher for HostCliHandDispatcher {
    fn kind(&self) -> &'static str {
        self.host_name
    }

    #[instrument(skip(self, ctx), fields(ticket_id = %ctx.ticket.id, host = self.host_name))]
    async fn dispatch(&self, ctx: &DispatchContext<'_>) -> Result<DispatchResult, DispatchError> {
        let ticket = ctx.ticket;
        let branch = self.target_branch(ticket);

        // 1. Mint and register the hand.
        let hand_id = self.mint_hand_id()?;
        self.substrate
            .register_hand(Hand {
                id: hand_id.clone(),
                kind: self.hand_kind,
                last_seen: Some(Utc::now()),
                pid: None,
            })
            .await?;

        // 2. Resolve the host adapter BEFORE assigning the ticket. A missing
        //    adapter must not leave the ticket stuck InFlight, so this check
        //    happens while the ticket is still Ready and unowned.
        let host = self.hosts.get(self.host_name).ok_or_else(|| {
            DispatchError::Substrate(SubstrateError::Invalid {
                field: "host".to_owned(),
                message: format!("host adapter {:?} is not registered", self.host_name),
            })
        })?;

        // 3. Create the per-ticket worktree on the target branch, rooted at the
        //    foreman-supplied parent branch — also before assigning, so a git
        //    failure never strands the ticket InFlight. Concurrent host CLIs
        //    each get their own checkout (the core fix vs. the shared root).
        let worktree_path = self
            .ensure_worktree(ticket, &branch, &ctx.parent_branch)
            .await?;

        // 3b. Track the worktree as a ticket-keyed substrate row so the foreman
        //     TTL cleanup pass can reclaim it if this process dies before the
        //     deterministic terminal-state removal below runs. Still pre-assign,
        //     so a failure leaves the ticket Ready.
        if let Err(error) = self
            .substrate
            .register_ticket_worktree(ticket.id.as_str(), &branch, &worktree_path)
            .await
        {
            prune_ticket_worktree_dir(&self.config.repo_root, &worktree_path).await;
            return Err(DispatchError::Substrate(error));
        }

        // 4. Atomic Ready -> InFlight + owner = hand. From here every failure
        //    path must release the ticket and tear down the worktree.
        if let Err(error) = self.substrate.assign_to_hand(&ticket.id, &hand_id).await {
            self.cleanup_ticket_worktree(ticket.id.as_str(), &worktree_path)
                .await;
            return Err(DispatchError::Substrate(error));
        }

        // 5. Run the rest of dispatch with cleanup-on-error: any error after
        //    assignment releases the hand back to Ready and removes the
        //    worktree before propagating.
        let outcome = self
            .dispatch_assigned(ctx, &hand_id, &branch, &worktree_path, host)
            .await;
        if let Err(error) = outcome {
            let reason = format!("{} dispatch failed: {error}", self.host_name);
            warn!(ticket = %ticket.id, host = self.host_name, %reason, "host CLI dispatch failed");
            if let Err(release_error) = self
                .substrate
                .release_from_hand(&ticket.id, reason.clone())
                .await
            {
                warn!(
                    ?release_error,
                    ticket = %ticket.id,
                    "release_from_hand failed after host CLI error"
                );
            }
            let _ = self
                .record_note(ctx, format!("{} hand released: {reason}", self.host_name))
                .await;
            self.cleanup_ticket_worktree(ticket.id.as_str(), &worktree_path)
                .await;
            // Surface the failure to the foreman so it is NOT recorded as
            // dispatched (foreman.rs Phase 3). A released ticket is re-queued
            // on the next tick.
            return Err(error);
        }

        Ok(DispatchResult {
            hand: hand_id,
            completed_synchronously: false,
        })
    }
}

impl HostCliHandDispatcher {
    /// Post-assignment body. Any `Err` returned here triggers the caller's
    /// cleanup (release the hand + remove the worktree). The host CLI runs
    /// with `cwd` set to the per-ticket worktree so concurrent dispatches do
    /// not share a checkout or index.
    async fn dispatch_assigned(
        &self,
        ctx: &DispatchContext<'_>,
        hand_id: &HandId,
        branch: &str,
        worktree_path: &std::path::Path,
        host: &dyn derrick_tools::HostAdapter,
    ) -> Result<(), DispatchError> {
        let ticket = ctx.ticket;

        // Build the host request. Model is RAW; the adapter normalises and
        // passes `--model` (D65). cwd is the per-ticket worktree.
        let body = self.render_body(ticket, branch, &ctx.parent_branch, worktree_path);
        let mut request = HostRequest::new(body, worktree_path);
        request.headless = true;
        request.model = select_model(
            self.host_name,
            &self.model_choice,
            tier_for(ticket.complexity),
        );
        request.timeout = self.config.poll_timeout;

        // If auto-dispatch is off, leave the ticket InFlight and surface a
        // note for the operator. The worktree is kept so the operator can run
        // the host CLI there.
        if !self.config.auto_dispatch {
            self.record_note(
                ctx,
                format!(
                    "{} hand: ticket {} assigned to a hand; worktree={}; \
                     auto-dispatch off, run the host CLI there manually",
                    self.host_name,
                    ticket.id,
                    worktree_path.display()
                ),
            )
            .await?;
            return Ok(());
        }

        // Wire the D75/D76 telemetry sinks before invoking the host. The
        // pid_sink fires the moment `run_host` spawns the child: it records
        // the pid for foreman liveness and emits `HandStarted`. The
        // output_sink throttles agent output lines into `HandProgress`
        // events (at most one per HAND_PROGRESS_WINDOW and only on change).
        let substrate_for_pid = Arc::clone(&self.substrate);
        let hand_id_for_pid = hand_id.clone();
        let ticket_id = ticket.id.clone();
        request.pid_sink = Some(PidSink::new(move |pid| {
            let substrate = Arc::clone(&substrate_for_pid);
            let hand_id = hand_id_for_pid.clone();
            let ticket_id = ticket_id.clone();
            tokio::spawn(async move {
                let _ = substrate.set_hand_pid(&hand_id, Some(pid)).await;
                let _ = substrate
                    .record_typed_event(
                        EventScope::Hand(hand_id),
                        EventKind::HandStarted {
                            pid: Some(pid),
                            ticket: ticket_id,
                        },
                    )
                    .await;
            });
        }));
        let substrate_for_progress = Arc::clone(&self.substrate);
        let hand_id_for_progress = hand_id.clone();
        let throttler = Arc::new(Mutex::new(ProgressThrottler::new(HAND_PROGRESS_WINDOW)));
        request.output_sink = Some(OutputSink::new(move |_source, line| {
            let snippet = truncate_snippet(line.trim());
            let emit = throttler
                .lock()
                .map(|mut t| t.consider(Instant::now(), &snippet))
                .ok()
                .flatten();
            if let Some(snippet) = emit {
                let substrate = Arc::clone(&substrate_for_progress);
                let hand_id = hand_id_for_progress.clone();
                tokio::spawn(async move {
                    let _ = substrate
                        .record_typed_event(
                            EventScope::Hand(hand_id),
                            EventKind::HandProgress { snippet },
                        )
                        .await;
                });
            }
        }));

        // Invoke the host CLI. A spawn error / timeout / nonzero exit surfaces
        // as `Err`, which the caller turns into a release + cleanup + Err so
        // the foreman does not record the ticket as dispatched.
        let response = host.run(request).await;

        // Emit HandExited (D76) on both success and failure paths. This
        // replaces the free-text "hand stats" Note — token stats now ride in
        // HandExitStats. The pid is intentionally left on the hand row: after
        // dispatch the ticket is InReview or released (never InFlight), so the
        // cleanup pass's dead-pid check finds no InFlight ticket to requeue.
        match response {
            Ok(response) => {
                let stats = HandExitStats {
                    tokens_in: Some(u64::from(response.tokens_in)),
                    tokens_out: Some(u64::from(response.tokens_out)),
                };
                self.record_hand_exited(hand_id, response.exit_code, Some(stats))
                    .await;
            }
            Err(error) => {
                let code = match &error {
                    HostError::NonZeroExit { exit_code, .. } => *exit_code,
                    _ => -1,
                };
                self.record_hand_exited(hand_id, code, None).await;
                return Err(DispatchError::Io(std::io::Error::other(error.to_string())));
            }
        }

        // If the hand drove the ticket to a terminal hand state (it ran
        // `derrick ticket review`, so the branch is pushed and the checkout is
        // dead weight), remove the worktree now. Otherwise keep it: an operator
        // may still need it, and the foreman TTL pass reclaims abandoned ones.
        if self.check_terminal_state(ctx).await {
            self.cleanup_ticket_worktree(ctx.ticket.id.as_str(), worktree_path)
                .await;
        }
        Ok(())
    }

    /// Records a `HandExited` event (D76). Best-effort: a recording failure is
    /// logged and swallowed so it never masks the dispatch outcome.
    async fn record_hand_exited(
        &self,
        hand_id: &HandId,
        code: i32,
        stats: Option<HandExitStats>,
    ) {
        if let Err(error) = self
            .substrate
            .record_typed_event(
                EventScope::Hand(hand_id.clone()),
                EventKind::HandExited { code, stats },
            )
            .await
        {
            warn!(?error, hand = %hand_id, "failed to record HandExited event");
        }
    }

    async fn record_note(
        &self,
        ctx: &DispatchContext<'_>,
        body: String,
    ) -> Result<(), DispatchError> {
        self.substrate
            .record_typed_event(
                EventScope::Ticket(ctx.ticket.id.clone()),
                EventKind::Note { body },
            )
            .await?;
        Ok(())
    }

    /// Inspect the ticket after the host CLI exits. Returns `true` when it
    /// reached a terminal hand state (`InReview`/`Done`) so the caller can tear
    /// down the worktree; `false` otherwise (unknown/in-flight — keep it).
    async fn check_terminal_state(&self, ctx: &DispatchContext<'_>) -> bool {
        match self.substrate.get_ticket(&ctx.ticket.id).await {
            Ok(Some(ticket)) => {
                let terminal = matches!(
                    ticket.state,
                    derrick_substrate::TicketState::InReview | derrick_substrate::TicketState::Done
                );
                if terminal {
                    info!(
                        ticket = %ticket.id,
                        state = %ticket.state,
                        host = self.host_name,
                        "host CLI ticket reached terminal hand state"
                    );
                } else {
                    warn!(
                        ticket = %ticket.id,
                        state = %ticket.state,
                        host = self.host_name,
                        "host CLI exited but ticket is not InReview; operator may \
                         need to run `derrick ticket review`"
                    );
                }
                terminal
            }
            Ok(None) => {
                warn!(ticket = %ctx.ticket.id, host = self.host_name, "ticket not found after host CLI exit");
                false
            }
            Err(error) => {
                warn!(?error, ticket = %ctx.ticket.id, host = self.host_name, "failed to read ticket after host CLI exit");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use derrick_substrate::{NewTicket, TicketId, TicketState};
    use derrick_substrate_native::{NativeConfig, NativeSubstrate};
    use derrick_tools::{HostAdapter, HostError, HostResponse};
    use std::sync::Mutex;
    use tempfile::TempDir;

    // ---- ProgressThrottler / snippet tests (D76) ----

    #[test]
    fn progress_throttler_emits_first_changed_line() {
        let mut t = ProgressThrottler::new(Duration::from_secs(2));
        let now = Instant::now();
        assert_eq!(
            t.consider(now, "editing src/lib.rs"),
            Some("editing src/lib.rs".to_owned())
        );
        // Same line again → suppressed (no change).
        assert_eq!(t.consider(now, "editing src/lib.rs"), None);
    }

    #[test]
    fn progress_throttler_suppresses_within_window() {
        let mut t = ProgressThrottler::new(Duration::from_secs(2));
        let base = Instant::now();
        assert_eq!(t.consider(base, "line A"), Some("line A".to_owned()));
        // Different line, but within the window → suppressed.
        assert_eq!(
            t.consider(base + Duration::from_millis(500), "line B"),
            None
        );
        // After the window, the changed line emits.
        assert_eq!(
            t.consider(base + Duration::from_secs(3), "line B"),
            Some("line B".to_owned())
        );
    }

    #[test]
    fn progress_throttler_requires_change_even_after_window() {
        let mut t = ProgressThrottler::new(Duration::from_secs(1));
        let base = Instant::now();
        assert_eq!(t.consider(base, "same"), Some("same".to_owned()));
        // After the window but the line is unchanged → suppressed.
        assert_eq!(t.consider(base + Duration::from_secs(2), "same"), None);
    }

    #[test]
    fn truncate_snippet_caps_long_lines_with_ellipsis() {
        let long = "x".repeat(120);
        let truncated = truncate_snippet(&long);
        assert!(truncated.chars().count() <= HAND_PROGRESS_SNIPPET_MAX);
        assert!(truncated.ends_with('…'));
        assert_eq!(truncate_snippet("short"), "short");
        assert_eq!(truncate_snippet(""), "");
    }

    // ---- HandExited emission (D76) ----

    #[tokio::test]
    async fn dispatch_emits_hand_exited_event() {
        let tempdir = TempDir::new().expect("tempdir");
        let repo = tempdir.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        init_repo(&repo);
        let worktree_root = tempdir.path().join("host-worktrees");

        let state_td = TempDir::new_in(tempdir.path()).expect("state td");
        let substrate = open_substrate(&state_td).await;
        let ticket = make_ticket(&substrate, "drk-300").await;

        let captured = Arc::new(Mutex::new(None));
        let mut registry = HostRegistry::empty();
        registry.register(
            "codex",
            Box::new(StubHost {
                name: "codex",
                captured: Arc::clone(&captured),
            }),
        );

        let dispatcher = HostCliHandDispatcher::new(
            Arc::clone(&substrate),
            Arc::new(registry),
            "codex",
            HandKind::Codex,
            ModelChoice::Pinned("gpt-5.5".to_owned()),
            test_config(repo.clone(), worktree_root.clone()),
        );

        let result = dispatcher
            .dispatch(&ctx(&ticket, tempdir.path()))
            .await
            .expect("dispatch");

        // HandExited must be recorded for the hand (D76). The StubHost does
        // not invoke pid_sink/output_sink, so HandStarted/HandProgress won't
        // fire here — only HandExited (emitted in dispatch_assigned after run).
        let events = substrate.tail_typed_events(None, 50).await.expect("tail");
        assert!(
            events.iter().any(|e| matches!(
                (&e.scope, &e.kind),
                (EventScope::Hand(h), EventKind::HandExited { code, stats })
                    if h == &result.hand && *code == 0
                        && stats.as_ref().and_then(|s| s.tokens_in) == Some(0u64)
            )),
            "expected HandExited for hand {}, got: {:?}",
            result.hand,
            events.iter().map(|e| &e.kind).collect::<Vec<_>>()
        );
    }

    fn site_fixture() -> derrick_config::Site {
        derrick_config::Config::defaults().site().clone()
    }

    async fn open_substrate(tempdir: &TempDir) -> Arc<NativeSubstrate> {
        let substrate = NativeSubstrate::open(
            NativeConfig {
                db_path: tempdir.path().join("derrick.db"),
                worktree_root: tempdir.path().join("worktrees"),
            },
            site_fixture(),
        )
        .await
        .map_err(|error| format!("open substrate: {error}"))
        .unwrap_or_else(|message| panic!("{message}"));
        Arc::new(substrate)
    }

    async fn make_ticket(substrate: &NativeSubstrate, id: &str) -> Ticket {
        let new = NewTicket::new(
            TicketId::new(id)
                .map_err(|error| format!("ticket id: {error}"))
                .unwrap_or_else(|message| panic!("{message}")),
            None,
            None,
            "title",
            "body content for ticket",
            Vec::new(),
        )
        .map_err(|error| format!("new ticket: {error}"))
        .unwrap_or_else(|message| panic!("{message}"));
        substrate
            .create_ticket(new)
            .await
            .map_err(|error| format!("create: {error}"))
            .unwrap_or_else(|message| panic!("{message}"))
    }

    async fn make_ticket_with_complexity(
        substrate: &NativeSubstrate,
        id: &str,
        complexity: Option<derrick_substrate::Complexity>,
    ) -> Ticket {
        let mut new = NewTicket::new(
            TicketId::new(id)
                .map_err(|error| format!("ticket id: {error}"))
                .unwrap_or_else(|message| panic!("{message}")),
            None,
            None,
            "title",
            "body content for ticket",
            Vec::new(),
        )
        .map_err(|error| format!("new ticket: {error}"))
        .unwrap_or_else(|message| panic!("{message}"));
        new.complexity = complexity;
        substrate
            .create_ticket(new)
            .await
            .map_err(|error| format!("create: {error}"))
            .unwrap_or_else(|message| panic!("{message}"))
    }

    /// Dispatch a ticket through a StubHost and return the model the request
    /// carried. Shared by the per-ticket selection tests below.
    async fn captured_model_for(
        complexity: Option<derrick_substrate::Complexity>,
        model_choice: ModelChoice,
        ticket_id: &str,
    ) -> Option<String> {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let repo = tempdir.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        init_repo(&repo);
        let worktree_root = tempdir.path().join("host-worktrees");

        let state_td = TempDir::new_in(tempdir.path()).expect("state td");
        let substrate = open_substrate(&state_td).await;
        let ticket = make_ticket_with_complexity(&substrate, ticket_id, complexity).await;

        let captured = Arc::new(Mutex::new(None));
        let mut registry = HostRegistry::empty();
        registry.register(
            "claude",
            Box::new(StubHost {
                name: "claude",
                captured: Arc::clone(&captured),
            }),
        );

        let dispatcher = HostCliHandDispatcher::new(
            Arc::clone(&substrate),
            Arc::new(registry),
            "claude",
            HandKind::Claude,
            model_choice,
            test_config(repo, worktree_root),
        );

        dispatcher
            .dispatch(&ctx(&ticket, tempdir.path()))
            .await
            .expect("dispatch");
        let request = captured
            .lock()
            .expect("lock")
            .clone()
            .expect("host run invoked");
        request.model
    }

    #[tokio::test]
    async fn auto_selects_heavy_model_for_heavy_ticket() {
        let model = captured_model_for(
            Some(derrick_substrate::Complexity::Heavy),
            ModelChoice::Auto { bias: None },
            "drk-300",
        )
        .await;
        assert_eq!(model.as_deref(), Some("claude-opus-4-8"));
    }

    #[tokio::test]
    async fn auto_selects_light_model_for_low_ticket() {
        let model = captured_model_for(
            Some(derrick_substrate::Complexity::Low),
            ModelChoice::Auto { bias: None },
            "drk-301",
        )
        .await;
        assert_eq!(model.as_deref(), Some("claude-haiku-4-5"));
    }

    #[tokio::test]
    async fn auto_selects_standard_model_for_unset_complexity() {
        let model = captured_model_for(None, ModelChoice::Auto { bias: None }, "drk-302").await;
        assert_eq!(model.as_deref(), Some("claude-sonnet-4-6"));
    }

    #[tokio::test]
    async fn explicit_pin_wins_over_complexity() {
        // A heavy ticket would auto-pick opus, but the pin must win.
        let model = captured_model_for(
            Some(derrick_substrate::Complexity::Heavy),
            ModelChoice::Pinned("claude-haiku-4-5".to_owned()),
            "drk-303",
        )
        .await;
        assert_eq!(model.as_deref(), Some("claude-haiku-4-5"));
    }

    /// Host adapter that captures the request it was handed and returns a
    /// canned success response.
    struct StubHost {
        name: &'static str,
        captured: Arc<Mutex<Option<HostRequest>>>,
    }

    #[async_trait]
    impl HostAdapter for StubHost {
        fn name(&self) -> &str {
            self.name
        }

        fn is_available(&self) -> bool {
            true
        }

        async fn run(&self, request: HostRequest) -> Result<HostResponse, HostError> {
            *self.captured.lock().expect("lock") = Some(request);
            Ok(HostResponse {
                stdout: "ok\n".to_owned(),
                stderr: String::new(),
                exit_code: 0,
                elapsed: Duration::from_millis(1),
                tokens_in: 0,
                tokens_out: 0,
                pid: None,
            })
        }
    }

    fn ctx<'a>(ticket: &'a Ticket, worktree_root: &'a std::path::Path) -> DispatchContext<'a> {
        DispatchContext {
            ticket,
            worktree_root,
            parent_branch: "main".to_owned(),
        }
    }

    /// Initialise a minimal git repository in `dir` so `git worktree add` has
    /// something to operate against.
    fn init_repo(dir: &std::path::Path) {
        use std::process::Command as StdCommand;
        let run = |args: &[&str]| {
            let status = StdCommand::new("git")
                .args(args)
                .current_dir(dir)
                .status()
                .expect("git command");
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "test@example.invalid"]);
        run(&["config", "user.name", "Test"]);
        // Host environments may enforce commit signing globally; tests must
        // not depend on a signing key being available.
        run(&["config", "commit.gpgsign", "false"]);
        run(&["commit", "--allow-empty", "-q", "-m", "init"]);
    }

    fn test_config(repo_root: PathBuf, worktree_root: PathBuf) -> HostCliHandDispatcherConfig {
        HostCliHandDispatcherConfig {
            auto_dispatch: true,
            poll_interval: Duration::from_millis(20),
            poll_timeout: Duration::from_millis(100),
            agent_identity: "codex-test".to_owned(),
            branch_prefix: "derrick".to_owned(),
            repo_root,
            worktree_root,
            roughneck_enabled: false,
            roughneck_level: "full".to_owned(),
            lesson_index: None,
        }
    }

    /// Host adapter whose `run` always fails, to exercise the failure path.
    struct FailingHost {
        name: &'static str,
    }

    #[async_trait]
    impl HostAdapter for FailingHost {
        fn name(&self) -> &str {
            self.name
        }

        fn is_available(&self) -> bool {
            true
        }

        async fn run(&self, _request: HostRequest) -> Result<HostResponse, HostError> {
            Err(HostError::NotFound {
                host: self.name.to_owned(),
            })
        }
    }

    #[tokio::test]
    async fn dispatch_forwards_model_and_runs_in_per_ticket_worktree() {
        let tempdir = tempfile::tempdir()
            .map_err(|error| format!("tempdir: {error}"))
            .unwrap_or_else(|message| panic!("{message}"));
        let repo = tempdir.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        init_repo(&repo);
        let worktree_root = tempdir.path().join("host-worktrees");

        let state_td = TempDir::new_in(tempdir.path()).expect("state td");
        let substrate = open_substrate(&state_td).await;
        let ticket = make_ticket(&substrate, "drk-200").await;

        let captured = Arc::new(Mutex::new(None));
        let mut registry = HostRegistry::empty();
        registry.register(
            "codex",
            Box::new(StubHost {
                name: "codex",
                captured: Arc::clone(&captured),
            }),
        );

        let dispatcher = HostCliHandDispatcher::new(
            Arc::clone(&substrate),
            Arc::new(registry),
            "codex",
            HandKind::Codex,
            ModelChoice::Pinned("openai/gpt-5.5".to_owned()),
            test_config(repo.clone(), worktree_root.clone()),
        );

        let result = dispatcher
            .dispatch(&ctx(&ticket, tempdir.path()))
            .await
            .map_err(|error| format!("dispatch: {error}"))
            .unwrap_or_else(|message| panic!("{message}"));
        assert!(!result.completed_synchronously);

        // (2) Captured request carries the per-host-normalised model (the
        // pinned `openai/gpt-5.5` has its `openai/` prefix stripped for the
        // bare-id codex host by `select_model`; the adapter re-normalising is a
        // no-op), headless = true, and the cwd is the per-ticket worktree (NOT
        // the shared worktree_root).
        let request = captured
            .lock()
            .expect("lock")
            .clone()
            .unwrap_or_else(|| panic!("host run was invoked"));
        assert_eq!(request.model, Some("gpt-5.5".to_owned()));
        assert!(request.headless);
        let expected_worktree = worktree_root.join("drk-200");
        assert_eq!(request.cwd, expected_worktree);
        assert_ne!(request.cwd, tempdir.path());

        // The worktree was actually created with the branch checked out.
        assert!(
            expected_worktree.join(".git").exists(),
            "worktree .git not found at {expected_worktree:?}"
        );

        // The StubHost did not run `derrick ticket review`, so the ticket is
        // still InFlight (non-terminal): the worktree is kept and tracked as a
        // ticket-keyed row for the foreman TTL backstop.
        let rows = substrate
            .list_worktrees(false)
            .await
            .expect("list worktrees");
        assert!(
            rows.iter().any(|w| w.run_id == "ticket:drk-200"),
            "expected a tracked ticket worktree row, got {rows:?}"
        );

        // (3) Ticket transitioned Ready -> InFlight, owned by the new hand.
        let refreshed = substrate
            .get_ticket(&ticket.id)
            .await
            .map_err(|error| format!("get: {error}"))
            .unwrap_or_else(|message| panic!("{message}"))
            .unwrap_or_else(|| panic!("ticket present"));
        assert_eq!(refreshed.state, TicketState::InFlight);
        assert_eq!(refreshed.owner.as_ref(), Some(&result.hand));

        // (1) The registered hand is of kind Codex.
        let hands = substrate
            .list_hands()
            .await
            .map_err(|error| format!("list hands: {error}"))
            .unwrap_or_else(|message| panic!("{message}"));
        let registered = hands
            .iter()
            .find(|h| h.id == result.hand)
            .unwrap_or_else(|| panic!("hand present"));
        assert_eq!(registered.kind, HandKind::Codex);
    }

    #[tokio::test]
    async fn missing_adapter_does_not_strand_ticket_inflight() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let repo = tempdir.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        init_repo(&repo);
        let worktree_root = tempdir.path().join("host-worktrees");

        let state_td = TempDir::new_in(tempdir.path()).expect("state td");
        let substrate = open_substrate(&state_td).await;
        let ticket = make_ticket(&substrate, "drk-201").await;

        // Registry has no "codex" adapter registered.
        let registry = HostRegistry::empty();
        let dispatcher = HostCliHandDispatcher::new(
            Arc::clone(&substrate),
            Arc::new(registry),
            "codex",
            HandKind::Codex,
            ModelChoice::Pinned("openai/gpt-5.5".to_owned()),
            test_config(repo, worktree_root.clone()),
        );

        let result = dispatcher.dispatch(&ctx(&ticket, tempdir.path())).await;
        assert!(result.is_err(), "missing adapter must surface an error");

        // Ticket stays Ready and unowned (resolved before assignment), and no
        // worktree was created.
        let refreshed = substrate
            .get_ticket(&ticket.id)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(refreshed.state, TicketState::Ready);
        assert!(refreshed.owner.is_none());
        assert!(!worktree_root.join("drk-201").join(".git").exists());
    }

    #[tokio::test]
    async fn host_failure_releases_ticket_and_returns_err() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let repo = tempdir.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        init_repo(&repo);
        let worktree_root = tempdir.path().join("host-worktrees");

        let state_td = TempDir::new_in(tempdir.path()).expect("state td");
        let substrate = open_substrate(&state_td).await;
        let ticket = make_ticket(&substrate, "drk-202").await;

        let mut registry = HostRegistry::empty();
        registry.register("codex", Box::new(FailingHost { name: "codex" }));
        let dispatcher = HostCliHandDispatcher::new(
            Arc::clone(&substrate),
            Arc::new(registry),
            "codex",
            HandKind::Codex,
            ModelChoice::Pinned("openai/gpt-5.5".to_owned()),
            test_config(repo, worktree_root.clone()),
        );

        // Host run fails: dispatch must surface Err (so the foreman does not
        // record the ticket as dispatched), release the ticket back to Ready,
        // and clean up the worktree.
        let result = dispatcher.dispatch(&ctx(&ticket, tempdir.path())).await;
        assert!(result.is_err(), "host CLI failure must return Err");

        let refreshed = substrate
            .get_ticket(&ticket.id)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(refreshed.state, TicketState::Ready);
        assert!(refreshed.owner.is_none());
        assert!(
            !worktree_root.join("drk-202").join(".git").exists(),
            "worktree should be removed after a failed dispatch"
        );
        // The tracked row is forgotten too, so the foreman does not later try to
        // reclaim an already-removed worktree.
        assert!(
            substrate
                .list_worktrees(true)
                .await
                .expect("list worktrees")
                .is_empty(),
            "ticket worktree row should be forgotten after a failed dispatch"
        );
    }

    /// Host adapter that simulates a hand running `derrick ticket review`:
    /// it transitions the (InFlight) ticket to InReview before returning
    /// success, so the dispatcher observes a terminal hand state.
    struct ReviewingHost {
        name: &'static str,
        substrate: Arc<NativeSubstrate>,
        ticket_id: TicketId,
        branch: String,
    }

    #[async_trait]
    impl HostAdapter for ReviewingHost {
        fn name(&self) -> &str {
            self.name
        }

        fn is_available(&self) -> bool {
            true
        }

        async fn run(&self, _request: HostRequest) -> Result<HostResponse, HostError> {
            self.substrate
                .transition_to_in_review(
                    &self.ticket_id,
                    derrick_substrate::InReviewMetadata {
                        branch: self.branch.clone(),
                        pr_url: None,
                        pr_number: None,
                        head_sha: "deadbeef".to_owned(),
                    },
                )
                .await
                .expect("transition to in_review");
            Ok(HostResponse {
                stdout: "ok\n".to_owned(),
                stderr: String::new(),
                exit_code: 0,
                elapsed: Duration::from_millis(1),
                tokens_in: 0,
                tokens_out: 0,
                pid: None,
            })
        }
    }

    #[tokio::test]
    async fn terminal_ticket_removes_worktree_and_forgets_row() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let repo = tempdir.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        init_repo(&repo);
        let worktree_root = tempdir.path().join("host-worktrees");

        let state_td = TempDir::new_in(tempdir.path()).expect("state td");
        let substrate = open_substrate(&state_td).await;
        let ticket = make_ticket(&substrate, "drk-203").await;

        let mut registry = HostRegistry::empty();
        registry.register(
            "codex",
            Box::new(ReviewingHost {
                name: "codex",
                substrate: Arc::clone(&substrate),
                ticket_id: ticket.id.clone(),
                branch: "derrick/ad-hoc/drk-203".to_owned(),
            }),
        );
        let dispatcher = HostCliHandDispatcher::new(
            Arc::clone(&substrate),
            Arc::new(registry),
            "codex",
            HandKind::Codex,
            ModelChoice::Auto { bias: None },
            test_config(repo, worktree_root.clone()),
        );

        dispatcher
            .dispatch(&ctx(&ticket, tempdir.path()))
            .await
            .expect("dispatch");

        // Hand drove the ticket to InReview, so the worktree dir is removed and
        // its tracked row is forgotten.
        assert!(
            !worktree_root.join("drk-203").join(".git").exists(),
            "worktree should be removed once the ticket reaches a terminal state"
        );
        assert!(
            substrate
                .list_worktrees(true)
                .await
                .expect("list worktrees")
                .is_empty(),
            "ticket worktree row should be forgotten on terminal state"
        );
    }
}
