//! `ClaudeHandDispatcher` — Claude Code implementation of `HandDispatcher`.
//!
//! Each dispatch writes a self-contained queue file under `queue_dir`. When
//! `auto_dispatch` is on, a background task spawns `claude --print` with the
//! queue file piped to stdin, heartbeats the hand, and releases the ticket
//! back to `Ready` if the process times out. When `auto_dispatch` is off the
//! dispatcher just records a note pointing operators at the queue file.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use derrick_memory::LessonIndex;

use async_trait::async_trait;
use chrono::Utc;
use derrick_substrate::{
    Complexity, EventKind, EventScope, Hand, HandId, HandKind, Substrate, SubstrateError, Ticket,
    TicketId,
};
use derrick_substrate_native::NativeSubstrate;
use derrick_substrate_native::foreman::{
    DispatchContext, DispatchError, DispatchResult, HandDispatcher,
};
use derrick_tools::{ModelChoice, Tier, select_model};
use tokio::process::Command;
use tracing::{error, info, instrument, warn};

use crate::prompt::render_queue_file;

/// Runtime configuration for [`ClaudeHandDispatcher`]. Sourced from
/// `tools.claude` in `derrick.yaml`.
#[derive(Clone, Debug)]
pub struct ClaudeHandDispatcherConfig {
    /// When true, spawn `claude --print` automatically; otherwise just write
    /// the queue file and leave invocation to an operator.
    pub auto_dispatch: bool,
    /// Heartbeat interval while the background task waits on `claude`.
    pub poll_interval: Duration,
    /// Maximum wall-clock duration the background task waits before
    /// releasing the hand back to Ready.
    pub poll_timeout: Duration,
    /// Stable identity prefix used when minting hand ids. The dispatcher
    /// appends a short suffix so multiple dispatches do not collide.
    pub agent_identity: String,
    /// Prefix applied to dispatch branch names. Combined with the ticket
    /// batch and id to form `<prefix>/<batch>/<ticket-id>`.
    pub branch_prefix: String,
    /// Directory where queue files are written. Absolute path; the
    /// dispatcher creates it on demand.
    pub queue_dir: PathBuf,
    /// Default base branch when the foreman does not supply a parent (kept
    /// for symmetry with the Copilot dispatcher; unused in dispatch).
    pub base_branch: String,
    /// Whether to prepend Roughneck instructions to the queue file prompt.
    pub roughneck_enabled: bool,
    /// Roughneck level: "lite", "full", or "ultra".
    pub roughneck_level: String,
    /// Pre-loaded lesson index for retrieval injection (§9.A.4). When
    /// present, up to [`derrick_memory::LESSON_RETRIEVAL_LIMIT`] relevant
    /// lessons are appended to the queue file prompt. `None` skips injection
    /// and adds zero tokens.
    pub lesson_index: Option<Arc<LessonIndex>>,
}

impl Default for ClaudeHandDispatcherConfig {
    fn default() -> Self {
        Self {
            auto_dispatch: false,
            poll_interval: Duration::from_secs(60),
            poll_timeout: Duration::from_secs(60 * 60),
            agent_identity: "derrick-claude-hand".to_owned(),
            branch_prefix: "derrick".to_owned(),
            queue_dir: PathBuf::from(".derrick/queue"),
            base_branch: "main".to_owned(),
            roughneck_enabled: true,
            roughneck_level: "full".to_owned(),
            lesson_index: None,
        }
    }
}

/// `HandDispatcher` for Claude Code. See module docs.
pub struct ClaudeHandDispatcher {
    substrate: Arc<NativeSubstrate>,
    config: ClaudeHandDispatcherConfig,
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

impl ClaudeHandDispatcher {
    /// Construct a dispatcher from its substrate and config. The model choice
    /// defaults to foreman-selected [`ModelChoice::Auto`]; override it with
    /// [`Self::with_model_choice`].
    pub fn new(substrate: Arc<NativeSubstrate>, config: ClaudeHandDispatcherConfig) -> Self {
        Self {
            substrate,
            config,
            model_choice: ModelChoice::Auto { bias: None },
        }
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
            .map(derrick_substrate::BatchName::as_str)
            .unwrap_or("ad-hoc");
        format!("{}/{}/{}", self.config.branch_prefix, batch, ticket.id)
    }

    fn mint_hand_id(&self) -> Result<HandId, SubstrateError> {
        // HandId pattern: ^[a-z][a-z0-9-]{0,63}$. Sanitize the configured
        // identity, then append a short suffix derived from SystemTime.
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
    // 6 hex chars of entropy is enough for collision resistance within
    // a single dispatcher process.
    format!("{:06x}", (nanos as u64) & 0x00ff_ffff)
}

#[async_trait]
impl HandDispatcher for ClaudeHandDispatcher {
    fn kind(&self) -> &'static str {
        "claude"
    }

    #[instrument(skip(self, ctx), fields(ticket_id = %ctx.ticket.id))]
    async fn dispatch(&self, ctx: &DispatchContext<'_>) -> Result<DispatchResult, DispatchError> {
        let ticket = ctx.ticket;
        let branch = self.target_branch(ticket);

        // Resolve the per-ticket model from the executor's ModelChoice and the
        // ticket's complexity (D67). `None` lets claude pick its own default.
        let model = select_model("claude", &self.model_choice, tier_for(ticket.complexity));

        // 1. Mint a hand id and register it in the substrate.
        let hand_id = self.mint_hand_id()?;
        let hand = Hand {
            id: hand_id.clone(),
            kind: HandKind::Claude,
            last_seen: Some(Utc::now()),
        };
        self.substrate.register_hand(hand).await?;

        // 2. Ensure the queue dir exists and render the queue file.
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
            &branch,
            &ctx.parent_branch,
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

        // 3. Atomic Ready -> InFlight + owner = hand.
        self.substrate.assign_to_hand(&ticket.id, &hand_id).await?;

        // 4. Surface dispatch in the activity log.
        let queue_path_display = queue_file.display().to_string();
        let note = format!(
            "claude hand: queue file written to {queue_path_display}; run \
             'claude --print < {queue_path_display}' to dispatch"
        );
        self.substrate
            .record_typed_event(
                EventScope::Ticket(ticket.id.clone()),
                EventKind::Note { body: note },
            )
            .await?;
        info!(
            ticket = %ticket.id,
            branch = branch.as_str(),
            queue_file = %queue_path_display,
            auto_dispatch = self.config.auto_dispatch,
            "claude queue file written"
        );

        // 5. Optionally spawn the background poll task.
        if self.config.auto_dispatch {
            let task = PollTask {
                substrate: Arc::clone(&self.substrate),
                ticket_id: ticket.id.clone(),
                hand_id: hand_id.clone(),
                queue_file: queue_file.clone(),
                model: model.clone(),
                poll_interval: self.config.poll_interval,
                poll_timeout: self.config.poll_timeout,
                roughneck_enabled: self.config.roughneck_enabled,
                roughneck_level: self.config.roughneck_level.clone(),
            };
            tokio::spawn(task.run());
        }

        Ok(DispatchResult {
            hand: hand_id,
            completed_synchronously: false,
        })
    }
}

/// Background task that supervises a single `claude --print` invocation.
struct PollTask {
    substrate: Arc<NativeSubstrate>,
    ticket_id: TicketId,
    hand_id: HandId,
    queue_file: PathBuf,
    /// Resolved per-ticket model id (D67). `None` omits `--model`.
    model: Option<String>,
    poll_interval: Duration,
    poll_timeout: Duration,
    roughneck_enabled: bool,
    roughneck_level: String,
}

impl PollTask {
    async fn run(self) {
        // Open the queue file for stdin redirection.
        let stdin_handle = match std::fs::File::open(&self.queue_file) {
            Ok(file) => file,
            Err(error) => {
                error!(
                    ?error,
                    queue_file = %self.queue_file.display(),
                    "failed to open queue file for claude --print"
                );
                self.release(format!(
                    "failed to open queue file {}: {error}",
                    self.queue_file.display()
                ))
                .await;
                return;
            }
        };

        let mut cmd = Command::new("claude");
        cmd.arg("--print").arg("--output-format").arg("json");
        if let Some(model) = &self.model {
            cmd.arg("--model").arg(model);
        }
        cmd.stdin(Stdio::from(stdin_handle))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Fail closed: if the supervising foreman crashes or this task is
            // cancelled, the `claude --print` child must not survive as an
            // orphan. `kill_on_drop` ties the child's lifetime to `child`.
            .kill_on_drop(true);
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(error) => {
                error!(?error, "failed to spawn `claude --print`");
                self.release(format!("failed to spawn claude: {error}"))
                    .await;
                return;
            }
        };

        // Drain stdout concurrently so the pipe never blocks the child.
        let stdout_handle = child.stdout.take().map(|mut stdout| {
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut buf = Vec::new();
                let _ = stdout.read_to_end(&mut buf).await;
                buf
            })
        });

        // Heartbeat loop interleaved with process wait.
        let deadline = tokio::time::Instant::now() + self.poll_timeout;
        loop {
            // Heartbeat each tick.
            if let Err(error) = self.substrate.hand_heartbeat(&self.hand_id).await {
                warn!(
                    ?error,
                    hand = %self.hand_id,
                    "hand_heartbeat failed during claude poll loop"
                );
            }

            let now = tokio::time::Instant::now();
            if now >= deadline {
                warn!(
                    ticket = %self.ticket_id,
                    timeout_seconds = self.poll_timeout.as_secs(),
                    "claude dispatch timed out; killing process and releasing hand"
                );
                let _ = child.kill().await;
                self.release(format!(
                    "claude dispatch timed out after {}s",
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
                                "claude --print exited successfully"
                            );
                            let stdout_bytes = match stdout_handle {
                                Some(handle) => handle.await.unwrap_or_default(),
                                None => Vec::new(),
                            };
                            self.record_hand_stats(&stdout_bytes).await;
                            self.check_terminal_state().await;
                            return;
                        }
                        Ok(status) => {
                            warn!(
                                ticket = %self.ticket_id,
                                code = ?status.code(),
                                "claude --print exited non-zero"
                            );
                            self.release(format!(
                                "claude exited non-zero: {:?}",
                                status.code()
                            ))
                            .await;
                            return;
                        }
                        Err(error) => {
                            error!(?error, "failed to wait on claude --print");
                            self.release(format!("failed to wait on claude: {error}"))
                                .await;
                            return;
                        }
                    }
                }
                () = tokio::time::sleep(sleep) => {
                    // Loop back around for another heartbeat and timeout check.
                }
            }
        }
    }

    async fn record_hand_stats(&self, stdout_bytes: &[u8]) {
        let bytes_raw = stdout_bytes.len() as u32;
        let scrubber = derrick_scrub::Scrubber::with_defaults();
        let (_scrubbed, scrub_stats) = scrubber.scrub("claude", stdout_bytes);
        let bytes_saved = scrub_stats
            .bytes_in
            .saturating_sub(scrub_stats.bytes_out)
            .min(u64::from(u32::MAX)) as u32;

        let (tokens_in, tokens_out) =
            match serde_json::from_slice::<serde_json::Value>(stdout_bytes) {
                Ok(value) => {
                    let usage = value.get("usage");
                    let tokens_in = usage
                        .and_then(|u| u.get("input_tokens"))
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0) as u32;
                    let tokens_out = usage
                        .and_then(|u| u.get("output_tokens"))
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0) as u32;
                    (tokens_in, tokens_out)
                }
                Err(_) => (0, 0),
            };

        let roughneck_saved = if self.roughneck_enabled {
            // Use the text-based compliance measurement (replaces the
            // deprecated estimate_tokens_saved which assumed full compliance).
            let text = std::str::from_utf8(stdout_bytes).unwrap_or("");
            derrick_roughneck::estimate_savings(text, &self.roughneck_level).tokens_saved
        } else {
            0
        };

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
                "release_from_hand failed during claude cleanup"
            );
        }
        let _ = self
            .substrate
            .record_typed_event(
                EventScope::Ticket(self.ticket_id.clone()),
                EventKind::Note {
                    body: format!("claude hand released: {reason}"),
                },
            )
            .await;
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
                        "claude ticket reached terminal hand state"
                    );
                } else {
                    warn!(
                        ticket = %self.ticket_id,
                        state = %ticket.state,
                        "claude --print exited 0 but ticket is not InReview; \
                         operator may need to run `derrick ticket review`"
                    );
                }
            }
            Ok(None) => {
                warn!(ticket = %self.ticket_id, "ticket not found after claude exit");
            }
            Err(error) => {
                warn!(?error, ticket = %self.ticket_id, "failed to read ticket after claude exit");
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

    fn dispatcher_config(queue_dir: PathBuf) -> ClaudeHandDispatcherConfig {
        ClaudeHandDispatcherConfig {
            auto_dispatch: false,
            poll_interval: Duration::from_millis(20),
            poll_timeout: Duration::from_millis(100),
            agent_identity: "claude-test".to_owned(),
            branch_prefix: "derrick".to_owned(),
            queue_dir,
            base_branch: "main".to_owned(),
            roughneck_enabled: false,
            roughneck_level: "full".to_owned(),
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

    #[test]
    fn queue_file_render_contains_branch_and_review_command() {
        let body = render_queue_file(
            "drk-100",
            None,
            "title",
            "body",
            "derrick/ad-hoc/drk-100",
            "main",
            false,
            "full",
        );
        assert!(body.contains("derrick/ad-hoc/drk-100"));
        assert!(body.contains("main"));
        assert!(body.contains("derrick ticket review drk-100"));
    }

    #[tokio::test]
    async fn dispatch_interactive_writes_queue_file() {
        let tempdir = tempfile::tempdir()
            .map_err(|error| format!("tempdir: {error}"))
            .unwrap_or_else(|message| panic!("{message}"));
        let substrate = open_substrate(&tempdir).await;
        let ticket = make_ticket(&substrate, "drk-101").await;
        let queue_dir = tempdir.path().join("queue");
        let dispatcher =
            ClaudeHandDispatcher::new(Arc::clone(&substrate), dispatcher_config(queue_dir.clone()));

        let result = dispatcher
            .dispatch(&ctx(&ticket, tempdir.path()))
            .await
            .map_err(|error| format!("dispatch: {error}"))
            .unwrap_or_else(|message| panic!("{message}"));
        assert!(!result.completed_synchronously);

        // Queue file exists with expected content.
        let path = queue_dir.join("drk-101.md");
        let body = tokio::fs::read_to_string(&path)
            .await
            .map_err(|error| format!("read queue: {error}"))
            .unwrap_or_else(|message| panic!("{message}"));
        assert!(body.contains("derrick/ad-hoc/drk-101"));
        assert!(body.contains("derrick ticket review drk-101"));

        // Ticket is now InFlight, owned by the registered hand.
        let refreshed = substrate
            .get_ticket(&ticket.id)
            .await
            .map_err(|error| format!("get: {error}"))
            .unwrap_or_else(|message| panic!("{message}"))
            .unwrap_or_else(|| panic!("ticket present"));
        assert_eq!(refreshed.state, TicketState::InFlight);
        assert_eq!(refreshed.owner.as_ref(), Some(&result.hand));

        // Hand exists and is of kind Claude.
        let hands = substrate
            .list_hands()
            .await
            .map_err(|error| format!("list hands: {error}"))
            .unwrap_or_else(|message| panic!("{message}"));
        let registered = hands
            .iter()
            .find(|h| h.id == result.hand)
            .unwrap_or_else(|| panic!("hand present"));
        assert_eq!(registered.kind, HandKind::Claude);
    }

    #[tokio::test]
    async fn roughneck_injection_enabled() {
        let tempdir = tempfile::tempdir()
            .map_err(|error| format!("tempdir: {error}"))
            .unwrap_or_else(|message| panic!("{message}"));
        let substrate = open_substrate(&tempdir).await;
        let ticket = make_ticket(&substrate, "drk-103").await;
        let queue_dir = tempdir.path().join("queue");
        let mut cfg = dispatcher_config(queue_dir.clone());
        cfg.roughneck_enabled = true;
        cfg.roughneck_level = "full".to_owned();
        let dispatcher = ClaudeHandDispatcher::new(Arc::clone(&substrate), cfg);

        dispatcher
            .dispatch(&ctx(&ticket, tempdir.path()))
            .await
            .map_err(|error| format!("dispatch: {error}"))
            .unwrap_or_else(|message| panic!("{message}"));

        let body = tokio::fs::read_to_string(queue_dir.join("drk-103.md"))
            .await
            .map_err(|error| format!("read queue: {error}"))
            .unwrap_or_else(|message| panic!("{message}"));
        assert!(
            body.starts_with("[ROUGHNECK:FULL]"),
            "expected queue body to begin with ROUGHNECK header, got: {}",
            &body[..body.len().min(80)]
        );
    }

    #[tokio::test]
    async fn roughneck_injection_disabled() {
        let tempdir = tempfile::tempdir()
            .map_err(|error| format!("tempdir: {error}"))
            .unwrap_or_else(|message| panic!("{message}"));
        let substrate = open_substrate(&tempdir).await;
        let ticket = make_ticket(&substrate, "drk-104").await;
        let queue_dir = tempdir.path().join("queue");
        let mut cfg = dispatcher_config(queue_dir.clone());
        cfg.roughneck_enabled = false;
        let dispatcher = ClaudeHandDispatcher::new(Arc::clone(&substrate), cfg);

        dispatcher
            .dispatch(&ctx(&ticket, tempdir.path()))
            .await
            .map_err(|error| format!("dispatch: {error}"))
            .unwrap_or_else(|message| panic!("{message}"));

        let body = tokio::fs::read_to_string(queue_dir.join("drk-104.md"))
            .await
            .map_err(|error| format!("read queue: {error}"))
            .unwrap_or_else(|message| panic!("{message}"));
        assert!(!body.starts_with("[ROUGHNECK:"));
    }

    #[tokio::test]
    async fn auto_dispatch_with_missing_binary_releases_hand() {
        // We override $PATH so `claude` is not found; the poll task should
        // record a release event and return the ticket to Ready.
        let tempdir = tempfile::tempdir()
            .map_err(|error| format!("tempdir: {error}"))
            .unwrap_or_else(|message| panic!("{message}"));
        let substrate = open_substrate(&tempdir).await;
        let ticket = make_ticket(&substrate, "drk-102").await;
        let queue_dir = tempdir.path().join("queue");
        let mut cfg = dispatcher_config(queue_dir);
        cfg.auto_dispatch = true;
        cfg.poll_timeout = Duration::from_millis(200);
        let dispatcher = ClaudeHandDispatcher::new(Arc::clone(&substrate), cfg);

        // Empty PATH for the duration of this test so spawn fails fast.
        // SAFETY: tests run single-threaded by default for this scope.
        let prev_path = std::env::var_os("PATH");
        // SAFETY: set_var/remove_var require care in multi-threaded contexts;
        // tokio::test uses a current-thread runtime by default which is fine.
        unsafe {
            std::env::set_var("PATH", "");
        }
        let _result = dispatcher
            .dispatch(&ctx(&ticket, tempdir.path()))
            .await
            .map_err(|error| format!("dispatch: {error}"))
            .unwrap_or_else(|message| panic!("{message}"));

        // Wait for the background task to release the hand.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut released = false;
        while std::time::Instant::now() < deadline {
            let refreshed = substrate
                .get_ticket(&ticket.id)
                .await
                .map_err(|error| format!("get: {error}"))
                .unwrap_or_else(|message| panic!("{message}"))
                .unwrap_or_else(|| panic!("ticket present"));
            if refreshed.state == TicketState::Ready && refreshed.owner.is_none() {
                released = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }

        // Restore PATH before any assertion that could panic.
        unsafe {
            match prev_path {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
        }
        assert!(released, "expected claude poll task to release the hand");
    }

    /// Regression guard for orphaned subprocesses: a `tokio::process::Command`
    /// configured the way `PollTask::run` configures the `claude --print`
    /// invocation (with `.kill_on_drop(true)`) must terminate its child when
    /// the `Child` handle is dropped, rather than leaving it running.
    #[tokio::test]
    async fn kill_on_drop_terminates_orphaned_child() {
        // A long-running child that would otherwise outlive the handle.
        let mut cmd = Command::new("sleep");
        cmd.arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let child = cmd.spawn().expect("spawn sleep");
        let pid = child.id().expect("child has a pid");

        // Sanity: the process is alive immediately after spawn.
        assert!(
            process_is_alive(pid),
            "child should be running right after spawn"
        );

        // Dropping the handle must reap/kill the child because of kill_on_drop.
        drop(child);

        // Give the runtime a moment to deliver the kill and reap the zombie.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut gone = false;
        while std::time::Instant::now() < deadline {
            if !process_is_alive(pid) {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        assert!(
            gone,
            "child {pid} should be terminated after dropping the handle"
        );
    }

    /// Returns true while `pid` refers to a live, non-reaped process.
    /// Uses `kill(pid, 0)` semantics via `/proc` to avoid extra crates.
    fn process_is_alive(pid: u32) -> bool {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }
}
