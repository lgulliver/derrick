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

use async_trait::async_trait;
use chrono::Utc;
use derrick_substrate::{
    EventKind, EventScope, Hand, HandId, HandKind, Substrate, SubstrateError, Ticket, TicketId,
};
use derrick_substrate_native::foreman::{
    DispatchContext, DispatchError, DispatchResult, HandDispatcher,
};
use derrick_substrate_native::NativeSubstrate;
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
        }
    }
}

/// `HandDispatcher` for Claude Code. See module docs.
pub struct ClaudeHandDispatcher {
    substrate: Arc<NativeSubstrate>,
    config: ClaudeHandDispatcherConfig,
}

impl ClaudeHandDispatcher {
    /// Construct a dispatcher from its substrate and config.
    pub fn new(substrate: Arc<NativeSubstrate>, config: ClaudeHandDispatcherConfig) -> Self {
        Self { substrate, config }
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
        let queue_body = render_queue_file(
            ticket.id.as_str(),
            batch,
            &ticket.title,
            &ticket.body,
            &branch,
            &ctx.parent_branch,
        );
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
                poll_interval: self.config.poll_interval,
                poll_timeout: self.config.poll_timeout,
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
    poll_interval: Duration,
    poll_timeout: Duration,
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

        let mut child = match Command::new("claude")
            .arg("--print")
            .stdin(Stdio::from(stdin_handle))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                error!(?error, "failed to spawn `claude --print`");
                self.release(format!("failed to spawn claude: {error}"))
                    .await;
                return;
            }
        };

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
}
