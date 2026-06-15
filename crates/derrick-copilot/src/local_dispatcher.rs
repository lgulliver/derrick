//! `LocalCopilotHandDispatcher` — local-CLI implementation of `HandDispatcher`
//! for the GitHub Copilot CLI (`copilot -p <prompt> --add-dir <cwd>`).
//!
//! Mirrors `derrick_claude::ClaudeHandDispatcher`: per ticket we create a
//! worktree on the ticket's branch (rooted at the foreman-supplied parent
//! branch), render a queue file containing the prompt, and spawn `copilot`
//! as a subprocess with the prompt passed via `-p`. The dispatched agent is
//! expected to commit, push, and call `derrick ticket review` to hand work
//! back to the foreman.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use derrick_memory::LessonIndex;

use async_trait::async_trait;
use chrono::Utc;
use derrick_stack::{OpenPrParams, StackBackend};
use derrick_substrate::{
    Complexity, EventKind, EventScope, Hand, HandId, HandKind, InReviewMetadata,
    ManualDoneAttestation, Substrate, SubstrateError, Ticket, TicketId, TicketState,
};
use derrick_substrate_native::NativeSubstrate;
use derrick_substrate_native::foreman::{
    DispatchContext, DispatchError, DispatchResult, HandDispatcher, prune_ticket_worktree_dir,
};
use derrick_tools::{ModelChoice, Tier, select_model};
use tokio::process::Command;
use tracing::{error, info, instrument, warn};

use crate::branch::branch_name;

/// Runtime configuration for [`LocalCopilotHandDispatcher`].
#[derive(Clone, Debug)]
pub struct LocalCopilotHandDispatcherConfig {
    /// When true, spawn `copilot` automatically; otherwise just write
    /// the queue file and leave invocation to an operator.
    pub auto_dispatch: bool,
    /// Heartbeat interval while the background task waits on `copilot`.
    pub poll_interval: Duration,
    /// Maximum wall-clock duration the background task waits before
    /// releasing the hand back to Ready.
    pub poll_timeout: Duration,
    /// Stable identity prefix used when minting hand ids.
    pub agent_identity: String,
    /// Prefix applied to dispatch branch names.
    pub branch_prefix: String,
    /// Directory where queue files are written (absolute path).
    pub queue_dir: PathBuf,
    /// Repository root used as the source for `git worktree add`.
    pub repo_root: PathBuf,
    /// Directory under which per-ticket worktrees are created (absolute).
    pub worktree_root: PathBuf,
    /// Path or name of the `copilot` binary. Defaults to "copilot".
    pub copilot_binary: PathBuf,
    /// Pass `--allow-all-tools` to the copilot CLI.
    pub allow_all_tools: bool,
    /// Whether to prepend Roughneck instructions to the queue file prompt.
    pub roughneck_enabled: bool,
    /// Roughneck level: "lite", "full", or "ultra".
    pub roughneck_level: String,
    /// Open the PR as a draft when the post-dispatch stack hook fires.
    pub stack_draft: bool,
    /// Pre-loaded lesson index for retrieval injection (§9.A.4). When
    /// present, up to [`derrick_memory::LESSON_RETRIEVAL_LIMIT`] relevant
    /// lessons are appended to the queue file prompt. `None` skips injection
    /// and adds zero tokens.
    pub lesson_index: Option<Arc<LessonIndex>>,
}

impl Default for LocalCopilotHandDispatcherConfig {
    fn default() -> Self {
        Self {
            auto_dispatch: false,
            poll_interval: Duration::from_secs(60),
            poll_timeout: Duration::from_secs(60 * 60),
            agent_identity: "derrick-copilot-hand".to_owned(),
            branch_prefix: "derrick".to_owned(),
            queue_dir: PathBuf::from(".derrick/queue"),
            repo_root: PathBuf::from("."),
            worktree_root: PathBuf::from(".derrick/copilot-worktrees"),
            copilot_binary: PathBuf::from("copilot"),
            allow_all_tools: true,
            roughneck_enabled: true,
            roughneck_level: "full".to_owned(),
            stack_draft: false,
            lesson_index: None,
        }
    }
}

/// `HandDispatcher` for the local GitHub Copilot CLI.
pub struct LocalCopilotHandDispatcher {
    substrate: Arc<NativeSubstrate>,
    config: LocalCopilotHandDispatcherConfig,
    stack_backend: Option<Arc<dyn StackBackend>>,
    model_choice: ModelChoice,
}

/// Map a ticket's optional [`Complexity`] to a [`Tier`] for adaptive model
/// selection (D67). `None` and `Standard` both resolve to `Standard`.
fn tier_for(complexity: Option<Complexity>) -> Tier {
    match complexity {
        Some(Complexity::Low) => Tier::Light,
        Some(Complexity::Heavy) => Tier::Heavy,
        _ => Tier::Standard,
    }
}

impl LocalCopilotHandDispatcher {
    /// Construct a dispatcher from its substrate and config. The model choice
    /// defaults to foreman-selected [`ModelChoice::Auto`]; override it with
    /// [`Self::with_model_choice`].
    pub fn new(substrate: Arc<NativeSubstrate>, config: LocalCopilotHandDispatcherConfig) -> Self {
        Self {
            substrate,
            config,
            stack_backend: None,
            model_choice: ModelChoice::Auto { bias: None },
        }
    }

    /// Attach a stack backend used to open a PR after a successful copilot run.
    pub fn with_stack_backend(mut self, backend: Arc<dyn StackBackend>) -> Self {
        self.stack_backend = Some(backend);
        self
    }

    /// Set the executor [`ModelChoice`] used to resolve the per-ticket model
    /// (D67).
    pub fn with_model_choice(mut self, model_choice: ModelChoice) -> Self {
        self.model_choice = model_choice;
        self
    }

    fn target_branch(&self, ticket: &Ticket) -> String {
        let batch = ticket
            .batch
            .as_ref()
            .map(derrick_substrate::BatchName::as_str);
        branch_name(&self.config.branch_prefix, batch, ticket.id.as_str())
    }

    fn mint_hand_id(&self) -> Result<HandId, SubstrateError> {
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
            cleaned.insert_str(0, "c-");
        }
        let suffix = short_suffix();
        let mut raw = format!("{cleaned}-{suffix}");
        raw.truncate(64);
        HandId::new(raw)
    }
}

fn short_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:06x}", (nanos as u64) & 0x00ff_ffff)
}

/// Render the queue file content for a local copilot dispatch.
#[allow(clippy::too_many_arguments)]
fn render_queue_file(
    ticket_id: &str,
    batch: Option<&str>,
    title: &str,
    body: &str,
    branch: &str,
    parent_branch: &str,
    worktree: &std::path::Path,
    roughneck_enabled: bool,
    roughneck_level: &str,
) -> String {
    let batch_display = batch.unwrap_or("(none)");
    let mut out = String::new();
    out.push_str("# Derrick ticket: ");
    out.push_str(title);
    out.push('\n');
    out.push('\n');
    out.push_str(
        "You are implementing a ticket dispatched by derrick's crew-mode foreman.\n\
         Complete ALL steps below in order. Do not stop until the final review step is done.\n",
    );
    out.push('\n');
    out.push_str("## Ticket metadata\n");
    out.push_str(&format!("- **ID**: {ticket_id}\n"));
    out.push_str(&format!("- **Batch**: {batch_display}\n"));
    out.push_str(&format!("- **Branch**: `{branch}`\n"));
    out.push_str(&format!("- **Base**: `{parent_branch}`\n"));
    out.push_str(&format!("- **Worktree**: `{}`\n", worktree.display()));
    out.push('\n');
    out.push_str("## Specification\n\n");
    out.push_str(body);
    if !body.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
    out.push_str("## Required steps\n\n");
    out.push_str(&format!(
        "1. Work inside the worktree `{}`. The branch `{branch}` is already checked out there.\n",
        worktree.display()
    ));
    out.push_str(
        "2. Implement the specification above. Commit all changes with conventional\n   \
         commit messages.\n",
    );
    out.push_str(&format!(
        "3. Push the branch: `git push -u origin {branch}`\n"
    ));
    out.push_str("4. Capture HEAD SHA: `git rev-parse HEAD`\n");
    out.push_str(&format!(
        "5. Hand back to the foreman:\n   `derrick ticket review {ticket_id} --branch {branch} --head-sha <HEAD_SHA>`\n",
    ));
    out.push('\n');
    out.push_str(
        "**Do not open a PR yourself** — the foreman handles PR creation when stacking\n\
         is configured.\n",
    );
    if roughneck_enabled {
        derrick_roughneck::inject_prompt(&out, roughneck_level)
    } else {
        out
    }
}

#[async_trait]
impl HandDispatcher for LocalCopilotHandDispatcher {
    fn kind(&self) -> &'static str {
        "copilot"
    }

    #[instrument(skip(self, ctx), fields(ticket_id = %ctx.ticket.id))]
    async fn dispatch(&self, ctx: &DispatchContext<'_>) -> Result<DispatchResult, DispatchError> {
        let ticket = ctx.ticket;
        let branch = self.target_branch(ticket);

        // Resolve the per-ticket model from the executor's ModelChoice and the
        // ticket's complexity (D67). `None` means: let copilot pick its default.
        let model = select_model("copilot", &self.model_choice, tier_for(ticket.complexity));

        // 1. Mint a hand id and register it.
        let hand_id = self.mint_hand_id()?;
        let hand = Hand {
            id: hand_id.clone(),
            kind: HandKind::Copilot,
            last_seen: Some(Utc::now()),
        };
        self.substrate.register_hand(hand).await?;

        // 2. Create the per-ticket worktree on the target branch, rooted at
        //    the foreman-supplied parent branch.
        tokio::fs::create_dir_all(&self.config.worktree_root)
            .await
            .map_err(DispatchError::Io)?;
        let worktree_path = self.config.worktree_root.join(ticket.id.as_str());
        if !worktree_path.join(".git").exists() {
            let output = Command::new("git")
                .args(["worktree", "add", "-B", branch.as_str()])
                .arg(&worktree_path)
                .arg(&ctx.parent_branch)
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

        // 2b. Track the worktree as a ticket-keyed substrate row so the foreman
        //     TTL cleanup pass can reclaim it if this process dies before the
        //     PollTask removes it. Still pre-assign, so a failure leaves the
        //     ticket Ready.
        if let Err(error) = self
            .substrate
            .register_ticket_worktree(ticket.id.as_str(), &branch, &worktree_path)
            .await
        {
            prune_ticket_worktree_dir(&self.config.repo_root, &worktree_path).await;
            return Err(DispatchError::Substrate(error));
        }

        // 3+. Everything past worktree registration is fallible and must not
        //     leak the tracked worktree (queue dir/file, assignment, dispatch
        //     note). Mirror `HostCliHandDispatcher`: on any error prune the
        //     worktree dir + forget the ticket-worktree row (and release the
        //     ticket if it was already assigned) before propagating. Without
        //     this a tracked worktree leaks until TTL and a redispatch can
        //     reuse a stale queue/worktree.
        let mut assigned = false;
        let outcome = self
            .dispatch_after_registration(
                ctx,
                &branch,
                &worktree_path,
                model,
                hand_id,
                &mut assigned,
            )
            .await;
        match outcome {
            Ok(result) => Ok(result),
            Err(error) => {
                if assigned {
                    let reason = format!("copilot dispatch failed after assignment: {error}");
                    if let Err(release_error) =
                        self.substrate.release_from_hand(&ticket.id, reason).await
                    {
                        warn!(
                            ?release_error,
                            ticket = %ticket.id,
                            "release_from_hand failed during copilot dispatch cleanup"
                        );
                    }
                }
                self.cleanup_ticket_worktree(ticket.id.as_str(), &worktree_path)
                    .await;
                Err(error)
            }
        }
    }
}

impl LocalCopilotHandDispatcher {
    /// Post-registration body of `dispatch`. Any `Err` returned here triggers
    /// the caller's cleanup (prune the worktree dir + forget the tracked row,
    /// and release the ticket when `*assigned` is true). `*assigned` is set
    /// once `assign_to_hand` succeeds so the caller knows whether a release is
    /// required.
    #[allow(clippy::too_many_arguments)]
    async fn dispatch_after_registration(
        &self,
        ctx: &DispatchContext<'_>,
        branch: &str,
        worktree_path: &std::path::Path,
        model: Option<String>,
        hand_id: HandId,
        assigned: &mut bool,
    ) -> Result<DispatchResult, DispatchError> {
        let ticket = ctx.ticket;

        // 3. Render the queue file and write it.
        tokio::fs::create_dir_all(&self.config.queue_dir)
            .await
            .map_err(DispatchError::Io)?;
        let batch = ticket
            .batch
            .as_ref()
            .map(derrick_substrate::BatchName::as_str);
        let raw_body = render_queue_file(
            ticket.id.as_str(),
            batch,
            &ticket.title,
            &ticket.body,
            branch,
            &ctx.parent_branch,
            worktree_path,
            self.config.roughneck_enabled,
            &self.config.roughneck_level,
        );
        // Inject relevant lessons (§9.A.4). Query uses the ticket id + title.
        let query = format!("{} {}", ticket.id, ticket.title);
        let queue_body = if let Some(index) = &self.config.lesson_index {
            derrick_memory::inject_lessons_into_prompt(&raw_body, index, &query)
        } else {
            raw_body
        };
        let queue_file = self
            .config
            .queue_dir
            .join(format!("{}.md", ticket.id.as_str()));
        tokio::fs::write(&queue_file, &queue_body)
            .await
            .map_err(DispatchError::Io)?;

        // 4. Atomic Ready -> InFlight + owner = hand.
        self.substrate.assign_to_hand(&ticket.id, &hand_id).await?;
        *assigned = true;

        // 5. Surface dispatch in the activity log.
        let queue_path_display = queue_file.display().to_string();
        let worktree_display = worktree_path.display().to_string();
        let note = format!(
            "copilot (local) hand: queue={queue_path_display} worktree={worktree_display}; \
             will spawn `copilot -p <prompt> --add-dir {worktree_display}`"
        );
        self.substrate
            .record_typed_event(
                EventScope::Ticket(ticket.id.clone()),
                EventKind::Note { body: note },
            )
            .await?;
        info!(
            ticket = %ticket.id,
            branch = branch,
            queue_file = %queue_path_display,
            worktree = %worktree_display,
            auto_dispatch = self.config.auto_dispatch,
            "local copilot queue file written"
        );

        // 6. Optionally spawn the background poll task.
        if self.config.auto_dispatch {
            let task = PollTask {
                substrate: Arc::clone(&self.substrate),
                ticket_id: ticket.id.clone(),
                hand_id: hand_id.clone(),
                prompt: queue_body,
                worktree: worktree_path.to_path_buf(),
                repo_root: self.config.repo_root.clone(),
                branch: branch.to_owned(),
                parent_branch: ctx.parent_branch.clone(),
                copilot_binary: self.config.copilot_binary.clone(),
                model: model.clone(),
                allow_all_tools: self.config.allow_all_tools,
                poll_interval: self.config.poll_interval,
                poll_timeout: self.config.poll_timeout,
                roughneck_enabled: self.config.roughneck_enabled,
                roughneck_level: self.config.roughneck_level.clone(),
                stack_backend: self.stack_backend.clone(),
                stack_draft: self.config.stack_draft,
                agent_identity: self.config.agent_identity.clone(),
            };
            tokio::spawn(task.run());
        }

        Ok(DispatchResult {
            hand: hand_id,
            completed_synchronously: false,
        })
    }

    /// Remove a per-ticket worktree (on-disk directory + tracked substrate row)
    /// after a failed dispatch. Best-effort: the foreman TTL cleanup pass is
    /// the backstop for anything left behind.
    async fn cleanup_ticket_worktree(&self, ticket_id: &str, worktree_path: &std::path::Path) {
        prune_ticket_worktree_dir(&self.config.repo_root, worktree_path).await;
        if let Err(error) = self.substrate.forget_ticket_worktree(ticket_id).await {
            warn!(
                ?error,
                ticket = %ticket_id,
                "forget_ticket_worktree failed during copilot dispatch cleanup"
            );
        }
    }
}

/// Background task supervising a single `copilot` invocation.
struct PollTask {
    substrate: Arc<NativeSubstrate>,
    ticket_id: TicketId,
    hand_id: HandId,
    prompt: String,
    worktree: PathBuf,
    repo_root: PathBuf,
    branch: String,
    parent_branch: String,
    copilot_binary: PathBuf,
    /// Resolved per-ticket model id (D67). `None` omits `--model`.
    model: Option<String>,
    allow_all_tools: bool,
    poll_interval: Duration,
    poll_timeout: Duration,
    roughneck_enabled: bool,
    roughneck_level: String,
    stack_backend: Option<Arc<dyn StackBackend>>,
    stack_draft: bool,
    agent_identity: String,
}

impl PollTask {
    async fn run(self) {
        let mut cmd = Command::new(&self.copilot_binary);
        cmd.arg("-p")
            .arg(&self.prompt)
            .arg("--add-dir")
            .arg(&self.worktree)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(model) = &self.model {
            cmd.arg("--model").arg(model);
        }
        if self.allow_all_tools {
            cmd.arg("--allow-all-tools");
        }

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(error) => {
                error!(?error, "failed to spawn `copilot`");
                self.release(format!("failed to spawn copilot: {error}"))
                    .await;
                return;
            }
        };

        let stdout_handle = child.stdout.take().map(|mut stdout| {
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut buf = Vec::new();
                let _ = stdout.read_to_end(&mut buf).await;
                buf
            })
        });

        let deadline = tokio::time::Instant::now() + self.poll_timeout;
        loop {
            if let Err(error) = self.substrate.hand_heartbeat(&self.hand_id).await {
                warn!(
                    ?error,
                    hand = %self.hand_id,
                    "hand_heartbeat failed during copilot poll loop"
                );
            }

            let now = tokio::time::Instant::now();
            if now >= deadline {
                warn!(
                    ticket = %self.ticket_id,
                    timeout_seconds = self.poll_timeout.as_secs(),
                    "copilot dispatch timed out; killing process and releasing hand"
                );
                let _ = child.kill().await;
                self.release(format!(
                    "copilot dispatch timed out after {}s",
                    self.poll_timeout.as_secs()
                ))
                .await;
                return;
            }

            let sleep = self
                .poll_interval
                .min(deadline.saturating_duration_since(now));
            tokio::select! {
                wait = child.wait() => {
                    match wait {
                        Ok(status) if status.success() => {
                            info!(
                                ticket = %self.ticket_id,
                                "copilot exited successfully"
                            );
                            let stdout_bytes = match stdout_handle {
                                Some(handle) => handle.await.unwrap_or_default(),
                                None => Vec::new(),
                            };
                            self.record_hand_stats(&stdout_bytes).await;
                            self.check_terminal_state().await;
                            self.open_stacked_pr().await;
                            // The branch is pushed (and a PR may be open), so the
                            // checkout is dead weight once the ticket is terminal.
                            self.prune_if_terminal().await;
                            return;
                        }
                        Ok(status) => {
                            warn!(
                                ticket = %self.ticket_id,
                                code = ?status.code(),
                                "copilot exited non-zero"
                            );
                            self.release(format!(
                                "copilot exited non-zero: {:?}",
                                status.code()
                            ))
                            .await;
                            return;
                        }
                        Err(error) => {
                            error!(?error, "failed to wait on copilot");
                            self.release(format!("failed to wait on copilot: {error}"))
                                .await;
                            return;
                        }
                    }
                }
                () = tokio::time::sleep(sleep) => {
                    // Loop back around for heartbeat + timeout check.
                }
            }
        }
    }

    async fn record_hand_stats(&self, stdout_bytes: &[u8]) {
        let bytes_raw = stdout_bytes.len() as u32;
        let scrubber = derrick_scrub::Scrubber::with_defaults();
        let (_scrubbed, scrub_stats) = scrubber.scrub("copilot", stdout_bytes);
        let bytes_saved = scrub_stats
            .bytes_in
            .saturating_sub(scrub_stats.bytes_out)
            .min(u64::from(u32::MAX)) as u32;

        // Copilot CLI does not currently emit structured token usage in
        // stdout, so input/output token counts are recorded as zero. The
        // scrub byte-savings + roughneck estimate remain meaningful.
        let tokens_in: u32 = 0;
        let tokens_out: u32 = 0;
        let roughneck_saved = if self.roughneck_enabled {
            let text = std::str::from_utf8(stdout_bytes).unwrap_or("");
            derrick_roughneck::estimate_savings(text, &self.roughneck_level).tokens_saved
        } else {
            0
        };
        let _ = tokens_out; // copilot stdout has no token count; suppress unused warning

        let body = format!(
            "hand stats: tokens_in={tokens_in} tokens_out={tokens_out} \
             roughneck_saved={roughneck_saved} bytes_raw={bytes_raw} bytes_saved={bytes_saved}"
        );
        if let Err(error) = self
            .substrate
            .record_typed_event(
                EventScope::Ticket(self.ticket_id.clone()),
                EventKind::Note { body },
            )
            .await
        {
            warn!(?error, ticket = %self.ticket_id, "failed to record hand stats note");
        }
    }

    async fn release(&self, reason: String) {
        if let Err(error) = self
            .substrate
            .release_from_hand(&self.ticket_id, reason.clone())
            .await
        {
            warn!(
                ?error,
                ticket = %self.ticket_id,
                reason = %reason,
                "release_from_hand failed during copilot cleanup"
            );
        }
        let _ = self
            .substrate
            .record_typed_event(
                EventScope::Ticket(self.ticket_id.clone()),
                EventKind::Note {
                    body: format!("copilot hand released: {reason}"),
                },
            )
            .await;
        // The work is abandoned; a re-dispatch recreates the checkout. Remove
        // the worktree dir + tracked row now rather than waiting for the TTL.
        self.prune_worktree().await;
    }

    /// Remove the per-ticket worktree directory and forget its tracked row.
    /// Best-effort: the foreman TTL pass is the backstop for anything left.
    async fn prune_worktree(&self) {
        prune_ticket_worktree_dir(&self.repo_root, &self.worktree).await;
        if let Err(error) = self
            .substrate
            .forget_ticket_worktree(self.ticket_id.as_str())
            .await
        {
            warn!(
                ?error,
                ticket = %self.ticket_id,
                "forget_ticket_worktree failed during copilot cleanup"
            );
        }
    }

    /// Remove the worktree only if the ticket reached a terminal hand state
    /// (`InReview`/`Done`). Otherwise leave it: the hand may not have handed
    /// back yet, and the foreman TTL pass reclaims genuinely abandoned ones.
    async fn prune_if_terminal(&self) {
        match self.substrate.get_ticket(&self.ticket_id).await {
            Ok(Some(ticket))
                if matches!(ticket.state, TicketState::InReview | TicketState::Done) =>
            {
                self.prune_worktree().await;
            }
            _ => {}
        }
    }

    async fn check_terminal_state(&self) {
        match self.substrate.get_ticket(&self.ticket_id).await {
            Ok(Some(ticket)) => {
                if matches!(
                    ticket.state,
                    derrick_substrate::TicketState::InReview | derrick_substrate::TicketState::Done
                ) {
                    info!(
                        ticket = %self.ticket_id,
                        state = %ticket.state,
                        "copilot ticket reached terminal hand state"
                    );
                } else {
                    warn!(
                        ticket = %self.ticket_id,
                        state = %ticket.state,
                        "copilot exited 0 but ticket is not InReview; \
                         operator may need to run `derrick ticket review`"
                    );
                }
            }
            Ok(None) => {
                warn!(ticket = %self.ticket_id, "ticket not found after copilot exit");
            }
            Err(error) => {
                warn!(?error, ticket = %self.ticket_id, "failed to read ticket after copilot exit");
            }
        }
    }

    /// Open a stacked PR for this ticket's branch using the configured
    /// [`StackBackend`], then transition the ticket to `Done`. If the PR
    /// fails to open (e.g. nothing to diff, branch already has a PR), log a
    /// warning and leave the ticket in `InReview`.
    async fn open_stacked_pr(&self) {
        let Some(backend) = self.stack_backend.as_ref() else {
            return;
        };

        let ticket = match self.substrate.get_ticket(&self.ticket_id).await {
            Ok(Some(t)) => t,
            Ok(None) => {
                warn!(ticket = %self.ticket_id, "ticket missing when opening stacked PR");
                return;
            }
            Err(error) => {
                warn!(?error, ticket = %self.ticket_id, "read ticket before open_pr failed");
                return;
            }
        };
        if ticket.state != TicketState::InReview {
            return;
        }

        let metadata = self
            .substrate
            .most_recent_in_review_metadata(&self.ticket_id)
            .await
            .ok()
            .flatten();
        if let Some(m) = metadata.as_ref() {
            if m.pr_url.is_some() {
                return;
            }
        }
        let branch = metadata
            .as_ref()
            .map(|m| m.branch.clone())
            .unwrap_or_else(|| self.branch.clone());
        let head_sha_existing = metadata.as_ref().map(|m| m.head_sha.clone());

        let body = format!("Closes {}\n\n{}", self.ticket_id, ticket.body);
        let params = OpenPrParams {
            branch: branch.clone(),
            parent_branch: self.parent_branch.clone(),
            title: ticket.title.clone(),
            body,
            draft: self.stack_draft,
            repo_root: self.repo_root.clone(),
        };

        let info = match backend.open_pr(params).await {
            Ok(info) => info,
            Err(error) => {
                warn!(
                    ?error,
                    ticket = %self.ticket_id,
                    branch = %branch,
                    "gh pr create failed; leaving ticket in InReview"
                );
                let _ = self
                    .substrate
                    .record_typed_event(
                        EventScope::Ticket(self.ticket_id.clone()),
                        EventKind::Note {
                            body: format!("open_pr failed: {error}"),
                        },
                    )
                    .await;
                return;
            }
        };

        let head_sha = if info.head_sha.is_empty() {
            head_sha_existing.unwrap_or_else(|| info.head_sha.clone())
        } else {
            info.head_sha.clone()
        };
        let new_metadata = InReviewMetadata {
            branch,
            pr_url: Some(info.url.clone()),
            pr_number: Some(info.number),
            head_sha,
        };
        if let Err(error) = self
            .substrate
            .transition_to_in_review(&self.ticket_id, new_metadata)
            .await
        {
            warn!(?error, ticket = %self.ticket_id, "failed to update InReview metadata with pr_url");
            return;
        }

        match self
            .substrate
            .mark_ticket_done_manually(
                &self.ticket_id,
                ManualDoneAttestation {
                    claimant: self.agent_identity.clone(),
                    note: format!("copilot hand opened {}", info.url),
                },
            )
            .await
        {
            Ok(_) => {
                info!(
                    ticket = %self.ticket_id,
                    pr = %info.url,
                    "copilot hand opened stacked PR and marked ticket Done"
                );
            }
            Err(error) => {
                warn!(
                    ?error,
                    ticket = %self.ticket_id,
                    "mark_ticket_done_manually failed after PR open"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use derrick_substrate::{NewTicket, TicketState};
    use derrick_substrate_native::{NativeConfig, NativeSubstrate};
    use tempfile::TempDir;

    fn site_fixture() -> derrick_config::Site {
        derrick_config::Config::defaults().site().clone()
    }

    fn native_config(tempdir: &TempDir) -> NativeConfig {
        NativeConfig {
            db_path: tempdir.path().join("derrick.db"),
            worktree_root: tempdir.path().join("worktrees"),
        }
    }

    async fn open_substrate(tempdir: &TempDir) -> Arc<NativeSubstrate> {
        let substrate = NativeSubstrate::open(native_config(tempdir), site_fixture())
            .await
            .expect("open substrate");
        Arc::new(substrate)
    }

    async fn make_ticket(substrate: &NativeSubstrate, id: &str) -> Ticket {
        let new = NewTicket::new(
            TicketId::new(id).expect("ticket id"),
            None,
            None,
            "title",
            "body content for ticket",
            Vec::new(),
        )
        .expect("new ticket");
        substrate.create_ticket(new).await.expect("create ticket")
    }

    /// Initialise a minimal git repository in `dir` so `git worktree add`
    /// has something to operate against.
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

    fn dispatcher_config(
        repo: PathBuf,
        worktree_root: PathBuf,
        queue_dir: PathBuf,
    ) -> LocalCopilotHandDispatcherConfig {
        LocalCopilotHandDispatcherConfig {
            auto_dispatch: false,
            poll_interval: Duration::from_millis(20),
            poll_timeout: Duration::from_millis(100),
            agent_identity: "copilot-test".to_owned(),
            branch_prefix: "derrick".to_owned(),
            queue_dir,
            repo_root: repo,
            worktree_root,
            copilot_binary: PathBuf::from("copilot"),
            allow_all_tools: true,
            roughneck_enabled: false,
            roughneck_level: "full".to_owned(),
            stack_draft: false,
            lesson_index: None,
        }
    }

    fn ctx<'a>(ticket: &'a Ticket, worktree_root: &'a std::path::Path) -> DispatchContext<'a> {
        DispatchContext {
            ticket,
            worktree_root,
            parent_branch: "main".to_owned(),
        }
    }

    #[tokio::test]
    async fn dispatch_creates_worktree_and_queue_file() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let repo = tempdir.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        init_repo(&repo);

        let substrate_dir = tempdir.path().join("state");
        std::fs::create_dir_all(&substrate_dir).expect("mkdir state");
        let state_td = TempDir::new_in(&substrate_dir).expect("state td");
        let substrate = open_substrate(&state_td).await;
        let ticket = make_ticket(&substrate, "drk-501").await;

        let worktree_root = tempdir.path().join("copilot-worktrees");
        let queue_dir = tempdir.path().join("queue");
        let dispatcher = LocalCopilotHandDispatcher::new(
            Arc::clone(&substrate),
            dispatcher_config(repo.clone(), worktree_root.clone(), queue_dir.clone()),
        );

        let result = dispatcher
            .dispatch(&ctx(&ticket, tempdir.path()))
            .await
            .expect("dispatch");
        assert!(!result.completed_synchronously);

        // Queue file exists and references branch + review command.
        let qf = queue_dir.join("drk-501.md");
        let body = tokio::fs::read_to_string(&qf).await.expect("read queue");
        assert!(body.contains("derrick/ad-hoc/drk-501"));
        assert!(body.contains("derrick ticket review drk-501"));

        // Worktree was created with the expected branch checked out.
        let wt = worktree_root.join("drk-501");
        assert!(
            wt.join(".git").exists(),
            "worktree .git not found at {wt:?}"
        );

        // auto_dispatch is off (no PollTask), so the worktree is kept for the
        // operator and tracked as a ticket-keyed row for the foreman backstop.
        let rows = substrate
            .list_worktrees(false)
            .await
            .expect("list worktrees");
        assert!(
            rows.iter().any(|w| w.run_id == "ticket:drk-501"),
            "expected a tracked ticket worktree row, got {rows:?}"
        );

        // Ticket is now InFlight, owned by the registered hand.
        let refreshed = substrate
            .get_ticket(&ticket.id)
            .await
            .expect("get")
            .expect("ticket present");
        assert_eq!(refreshed.state, TicketState::InFlight);
        assert_eq!(refreshed.owner.as_ref(), Some(&result.hand));

        // Hand exists and is of kind Copilot.
        let hands = substrate.list_hands().await.expect("list hands");
        let registered = hands.iter().find(|h| h.id == result.hand).expect("hand");
        assert_eq!(registered.kind, HandKind::Copilot);
    }

    #[tokio::test]
    async fn roughneck_injection_enabled() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let repo = tempdir.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        init_repo(&repo);

        let state_td = TempDir::new_in(tempdir.path()).expect("state td");
        let substrate = open_substrate(&state_td).await;
        let ticket = make_ticket(&substrate, "drk-502").await;
        let worktree_root = tempdir.path().join("copilot-worktrees");
        let queue_dir = tempdir.path().join("queue");
        let mut cfg = dispatcher_config(repo, worktree_root, queue_dir.clone());
        cfg.roughneck_enabled = true;
        cfg.roughneck_level = "full".to_owned();
        let dispatcher = LocalCopilotHandDispatcher::new(Arc::clone(&substrate), cfg);

        dispatcher
            .dispatch(&ctx(&ticket, tempdir.path()))
            .await
            .expect("dispatch");

        let body = tokio::fs::read_to_string(queue_dir.join("drk-502.md"))
            .await
            .expect("read queue");
        assert!(
            body.starts_with("[ROUGHNECK:FULL]"),
            "expected queue body to begin with ROUGHNECK header, got: {}",
            &body[..body.len().min(80)]
        );
    }

    #[tokio::test]
    async fn post_registration_failure_does_not_strand_worktree() {
        // Force a failure on a post-registration step (`assign_to_hand`) by
        // pre-assigning the ticket to another hand so it is no longer Ready.
        // The worktree row is registered before the assign, so this exercises
        // the cleanup-on-error path: no tracked worktree row may remain and the
        // worktree dir must be removed (mirrors derrick-hand's failure tests).
        let tempdir = tempfile::tempdir().expect("tempdir");
        let repo = tempdir.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        init_repo(&repo);

        let state_td = TempDir::new_in(tempdir.path()).expect("state td");
        let substrate = open_substrate(&state_td).await;
        let ticket = make_ticket(&substrate, "drk-504").await;

        // Pre-assign the ticket to a separate hand so the dispatcher's own
        // `assign_to_hand` fails (ticket is InFlight, not Ready).
        let blocker = Hand {
            id: HandId::new("blocker-hand").expect("hand id"),
            kind: HandKind::Copilot,
            last_seen: Some(Utc::now()),
        };
        substrate
            .register_hand(blocker.clone())
            .await
            .expect("register blocker");
        substrate
            .assign_to_hand(&ticket.id, &blocker.id)
            .await
            .expect("pre-assign");

        let worktree_root = tempdir.path().join("copilot-worktrees");
        let queue_dir = tempdir.path().join("queue");
        let dispatcher = LocalCopilotHandDispatcher::new(
            Arc::clone(&substrate),
            dispatcher_config(repo, worktree_root.clone(), queue_dir),
        );

        let result = dispatcher.dispatch(&ctx(&ticket, tempdir.path())).await;
        assert!(
            result.is_err(),
            "dispatch must surface Err when a post-registration step fails"
        );

        // The worktree dir is removed and its tracked row is forgotten, so a
        // redispatch does not reuse a stale checkout and the foreman TTL pass
        // has nothing to reclaim.
        assert!(
            !worktree_root.join("drk-504").join(".git").exists(),
            "worktree should be removed after a failed post-registration step"
        );
        assert!(
            substrate
                .list_worktrees(true)
                .await
                .expect("list worktrees")
                .is_empty(),
            "ticket worktree row should be forgotten after a failed dispatch"
        );
    }

    #[tokio::test]
    async fn auto_dispatch_with_missing_binary_releases_hand() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let repo = tempdir.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        init_repo(&repo);

        let state_td = TempDir::new_in(tempdir.path()).expect("state td");
        let substrate = open_substrate(&state_td).await;
        let ticket = make_ticket(&substrate, "drk-503").await;
        let worktree_root = tempdir.path().join("copilot-worktrees");
        let queue_dir = tempdir.path().join("queue");
        let mut cfg = dispatcher_config(repo, worktree_root.clone(), queue_dir);
        cfg.auto_dispatch = true;
        cfg.poll_timeout = Duration::from_millis(200);
        // Point at a binary that definitely doesn't exist so spawn fails.
        cfg.copilot_binary =
            PathBuf::from("/nonexistent/derrick/copilot-binary-that-does-not-exist");
        let dispatcher = LocalCopilotHandDispatcher::new(Arc::clone(&substrate), cfg);

        let _result = dispatcher
            .dispatch(&ctx(&ticket, tempdir.path()))
            .await
            .expect("dispatch");

        // Wait for the background task to release the hand AND prune the
        // worktree (release sets Ready first, then prunes, so poll on both).
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut released = false;
        while std::time::Instant::now() < deadline {
            let refreshed = substrate
                .get_ticket(&ticket.id)
                .await
                .expect("get")
                .expect("present");
            let rows_empty = substrate
                .list_worktrees(true)
                .await
                .expect("list worktrees")
                .is_empty();
            if refreshed.state == TicketState::Ready && refreshed.owner.is_none() && rows_empty {
                released = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        assert!(
            released,
            "expected copilot poll task to release the hand and prune the worktree"
        );
        // The on-disk worktree is removed on the release path too (previously it
        // leaked on every copilot non-success path).
        assert!(
            !worktree_root.join("drk-503").join(".git").exists(),
            "worktree should be removed after the hand is released"
        );
    }
}
