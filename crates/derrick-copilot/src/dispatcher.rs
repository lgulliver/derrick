//! `CopilotHandDispatcher` — the real implementation of `HandDispatcher`
//! for the GitHub Copilot coding agent.
//!
//! The dispatcher takes ownership of the branch creation, issue creation,
//! Copilot assignment, substrate hand registration, and asynchronous PR
//! polling. See `T013` for the full design.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use derrick_substrate::{
    EventKind, EventScope, Hand, HandId, HandKind, InReviewMetadata, Substrate, Ticket,
};
use derrick_substrate_native::NativeSubstrate;
use derrick_substrate_native::foreman::{
    DispatchContext, DispatchError, DispatchResult, HandDispatcher,
};
use tokio::time::Instant;
use tracing::{error, info, instrument, warn};

use crate::branch::{BranchCreator, BranchError, branch_name};
use crate::client::{CopilotDispatchClient, CopilotDispatchError, PrInfo};

/// Runtime configuration for [`CopilotHandDispatcher`]. Sourced from
/// `tools.copilot` in `derrick.yaml`.
#[derive(Clone, Debug)]
pub struct CopilotHandDispatcherConfig {
    /// Interval between successive PR polls. Default 30s.
    pub poll_interval: Duration,
    /// Maximum wall-clock duration the poll loop will wait for a PR before
    /// giving up. Default 10 minutes.
    pub poll_timeout: Duration,
    /// Branch name used as the base for new dispatch branches. Default
    /// `main`.
    pub base_branch: String,
    /// Stable identity prefix used when minting hand ids. The dispatcher
    /// appends a short random suffix so multiple dispatches do not collide.
    pub agent_identity: String,
    /// Prefix applied to dispatch branch names. Combined with the ticket
    /// batch and id to form `<prefix>/<batch>/<ticket-id>`. Sourced from
    /// `tools.git.branch_prefix`; defaults to `"derrick"`.
    pub branch_prefix: String,
    /// Whether to prepend Roughneck instructions to the issue body submitted
    /// to Copilot.
    pub roughneck_enabled: bool,
    /// Roughneck level: "lite", "full", or "ultra".
    pub roughneck_level: String,
}

impl Default for CopilotHandDispatcherConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(30),
            poll_timeout: Duration::from_secs(60 * 10),
            base_branch: "main".to_owned(),
            agent_identity: "derrick-hand".to_owned(),
            branch_prefix: "derrick".to_owned(),
            roughneck_enabled: true,
            roughneck_level: "full".to_owned(),
        }
    }
}

/// `HandDispatcher` for the GitHub Copilot coding agent. See module docs.
pub struct CopilotHandDispatcher {
    substrate: Arc<NativeSubstrate>,
    branch_creator: Arc<dyn BranchCreator>,
    client: Arc<dyn CopilotDispatchClient>,
    config: CopilotHandDispatcherConfig,
}

impl CopilotHandDispatcher {
    /// Construct a dispatcher from its three collaborators and config.
    pub fn new(
        substrate: Arc<NativeSubstrate>,
        branch_creator: Arc<dyn BranchCreator>,
        client: Arc<dyn CopilotDispatchClient>,
        config: CopilotHandDispatcherConfig,
    ) -> Self {
        Self {
            substrate,
            branch_creator,
            client,
            config,
        }
    }

    fn mint_hand_id(&self, _ticket: &Ticket) -> Result<HandId, derrick_substrate::SubstrateError> {
        // Substrate validator: ^[a-z][a-z0-9-]{0,63}$. We sanitize
        // `agent_identity` to keep just ASCII letters/digits/hyphens (lowered)
        // and ensure the first character is a letter. Then append the first
        // 8 hex chars of a fresh v4 uuid for entropy.
        let suffix: String = uuid::Uuid::new_v4()
            .as_simple()
            .to_string()
            .chars()
            .take(8)
            .collect();
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
        let mut raw = format!("{cleaned}-{suffix}");
        raw.truncate(64);
        HandId::new(raw)
    }

    fn target_branch(&self, ticket: &Ticket) -> String {
        let batch = ticket
            .batch
            .as_ref()
            .map(derrick_substrate::BatchName::as_str);
        branch_name(&self.config.branch_prefix, batch, ticket.id.as_str())
    }
}

#[async_trait]
impl HandDispatcher for CopilotHandDispatcher {
    fn kind(&self) -> &'static str {
        "copilot"
    }

    #[instrument(skip(self, ctx), fields(ticket_id = %ctx.ticket.id))]
    async fn dispatch(&self, ctx: &DispatchContext<'_>) -> Result<DispatchResult, DispatchError> {
        let ticket = ctx.ticket;
        let branch = self.target_branch(ticket);

        // 1. Ensure the branch exists on the remote so Copilot can target
        //    it. Use the foreman-supplied parent branch so stacked tickets
        //    are based on their predecessor rather than the global target.
        self.branch_creator
            .ensure_branch(&branch, &ctx.parent_branch)
            .await
            .map_err(branch_error_to_dispatch)?;

        // 2. Mint a hand id and register it in the substrate.
        let hand_id = self.mint_hand_id(ticket)?;
        let hand = Hand {
            id: hand_id.clone(),
            kind: HandKind::Copilot,
            last_seen: Some(Utc::now()),
            pid: None,
        };
        self.substrate.register_hand(hand).await?;

        // 3. File the Copilot task. If this fails the hand exists but no
        //    work has been dispatched — record a note and surface the
        //    error so the foreman can re-queue.
        let issue_body = if self.config.roughneck_enabled {
            derrick_roughneck::inject_prompt(&ticket.body, &self.config.roughneck_level)
        } else {
            ticket.body.clone()
        };
        let task = match self
            .client
            .create_task(&branch, &ticket.title, &issue_body)
            .await
        {
            Ok(task) => task,
            Err(error) => {
                error!(?error, "copilot create_task failed; leaving ticket Ready");
                let body = format!("copilot dispatch failed: {error}");
                self.substrate
                    .record_typed_event(
                        EventScope::Ticket(ticket.id.clone()),
                        EventKind::Note { body },
                    )
                    .await?;
                return Err(copilot_error_to_dispatch(error));
            }
        };
        info!(
            issue_number = task.issue_number,
            branch = branch.as_str(),
            "copilot task created"
        );

        // 4. Atomic Ready -> InFlight + owner = hand.
        self.substrate.assign_to_hand(&ticket.id, &hand_id).await?;
        // Surface dispatch in the activity log so operators can correlate
        // the issue with the ticket.
        self.substrate
            .record_typed_event(
                EventScope::Ticket(ticket.id.clone()),
                EventKind::Note {
                    body: format!(
                        "copilot dispatched: issue={} branch={} task_url={}",
                        task.issue_number,
                        branch,
                        task.issue_url.as_deref().unwrap_or("(none)")
                    ),
                },
            )
            .await?;

        // 5. Spawn the poll task. The task heartbeats the hand each
        //    iteration so the foreman's cleanup pass does not declare it
        //    abandoned; on success it transitions the ticket to InReview.
        let poll = PollTask {
            substrate: Arc::clone(&self.substrate),
            client: Arc::clone(&self.client),
            ticket_id: ticket.id.clone(),
            hand: hand_id.clone(),
            branch,
            poll_interval: self.config.poll_interval,
            poll_timeout: self.config.poll_timeout,
        };
        tokio::spawn(poll.run());

        Ok(DispatchResult {
            hand: hand_id,
            completed_synchronously: false,
        })
    }
}

fn branch_error_to_dispatch(error: BranchError) -> DispatchError {
    match error {
        BranchError::Io { source, .. } => DispatchError::Io(source),
        BranchError::NonZeroExit { .. } => {
            DispatchError::Io(std::io::Error::other(error.to_string()))
        }
    }
}

fn copilot_error_to_dispatch(error: CopilotDispatchError) -> DispatchError {
    match error {
        CopilotDispatchError::Io { source, .. } => DispatchError::Io(source),
        _ => DispatchError::Io(std::io::Error::other(error.to_string())),
    }
}

/// Background task that polls for a PR matching `branch` and transitions the
/// ticket to InReview when one is found.
pub(crate) struct PollTask {
    substrate: Arc<NativeSubstrate>,
    client: Arc<dyn CopilotDispatchClient>,
    ticket_id: derrick_substrate::TicketId,
    hand: HandId,
    branch: String,
    poll_interval: Duration,
    poll_timeout: Duration,
}

impl PollTask {
    /// Run the poll loop to completion. Visible to the crate so dispatcher
    /// tests can drive it directly without spawning.
    pub(crate) async fn run(self) {
        let deadline = Instant::now() + self.poll_timeout;
        loop {
            // Heartbeat before each attempt so a stuck network call doesn't
            // make the hand look abandoned.
            if let Err(error) = self.substrate.hand_heartbeat(&self.hand).await {
                warn!(
                    ?error,
                    hand = %self.hand,
                    "hand_heartbeat failed during poll loop"
                );
            }
            match self.client.poll_pr(&self.branch).await {
                Ok(Some(pr)) => {
                    if let Err(error) = self.transition_to_in_review(pr).await {
                        error!(
                            ?error,
                            ticket = %self.ticket_id,
                            "failed to transition ticket to InReview"
                        );
                    }
                    return;
                }
                Ok(None) => {}
                Err(error) => {
                    warn!(?error, branch = %self.branch, "poll_pr failed; will retry");
                }
            }
            if Instant::now() + self.poll_interval > deadline {
                warn!(
                    ticket = %self.ticket_id,
                    branch = %self.branch,
                    timeout_seconds = self.poll_timeout.as_secs(),
                    "copilot poll timed out without a PR; leaving ticket InFlight for the foreman to clean up"
                );
                let _ = self
                    .substrate
                    .record_typed_event(
                        EventScope::Ticket(self.ticket_id.clone()),
                        EventKind::Note {
                            body: format!(
                                "copilot poll timed out after {}s without a PR on branch {}",
                                self.poll_timeout.as_secs(),
                                self.branch
                            ),
                        },
                    )
                    .await;
                return;
            }
            tokio::time::sleep(self.poll_interval).await;
        }
    }

    async fn transition_to_in_review(
        &self,
        pr: PrInfo,
    ) -> Result<(), derrick_substrate::SubstrateError> {
        let metadata = InReviewMetadata {
            branch: self.branch.clone(),
            pr_url: Some(pr.url.clone()),
            pr_number: Some(pr.number),
            head_sha: pr.head_sha.clone(),
        };
        self.substrate
            .transition_to_in_review(&self.ticket_id, metadata)
            .await?;
        info!(
            ticket = %self.ticket_id,
            pr_number = pr.number,
            head_sha = %pr.head_sha,
            "copilot PR detected; ticket transitioned to InReview"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::tests::FakeGhClient;
    use derrick_substrate::{NewTicket, Ticket, TicketId, TicketState};
    use derrick_substrate_native::{NativeConfig, NativeSubstrate};
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;
    use tempfile::TempDir;
    use tokio::sync::Mutex;

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
        Arc::new(
            NativeSubstrate::open(native_config(tempdir), site_fixture())
                .await
                .expect("open"),
        )
    }

    fn ctx<'a>(ticket: &'a Ticket, worktree_root: &'a std::path::Path) -> DispatchContext<'a> {
        DispatchContext {
            ticket,
            worktree_root,
            parent_branch: "main".to_owned(),
        }
    }

    async fn make_ticket(substrate: &NativeSubstrate, id: &str) -> Ticket {
        let new = NewTicket::new(
            TicketId::new(id).expect("ticket id"),
            None,
            None,
            "title",
            "body",
            Vec::new(),
        )
        .expect("new ticket");
        substrate.create_ticket(new).await.expect("create")
    }

    /// In-memory branch creator that records calls and never fails.
    #[derive(Default)]
    struct RecordingBranch {
        calls: StdMutex<Vec<(String, String)>>,
    }

    #[async_trait]
    impl BranchCreator for RecordingBranch {
        async fn ensure_branch(&self, branch: &str, base_branch: &str) -> Result<(), BranchError> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push((branch.to_owned(), base_branch.to_owned()));
            }
            Ok(())
        }
    }

    fn fast_config() -> CopilotHandDispatcherConfig {
        CopilotHandDispatcherConfig {
            poll_interval: Duration::from_millis(10),
            poll_timeout: Duration::from_millis(200),
            base_branch: "main".to_owned(),
            agent_identity: "copilot-test".to_owned(),
            branch_prefix: "derrick".to_owned(),
            roughneck_enabled: false,
            roughneck_level: "full".to_owned(),
        }
    }

    #[tokio::test]
    async fn dispatch_creates_branch_and_registers_hand() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let substrate = open_substrate(&tempdir).await;
        let ticket = make_ticket(&substrate, "drk-001").await;
        let branch = RecordingBranch::default();
        let branch = Arc::new(branch);
        let client = Arc::new(FakeGhClient::new());
        let dispatcher = CopilotHandDispatcher::new(
            Arc::clone(&substrate),
            Arc::clone(&branch) as Arc<dyn BranchCreator>,
            Arc::clone(&client) as Arc<dyn CopilotDispatchClient>,
            fast_config(),
        );

        let result = dispatcher
            .dispatch(&ctx(&ticket, tempdir.path()))
            .await
            .expect("dispatch ok");
        assert!(!result.completed_synchronously);

        // Branch was requested with the derrick/<batch>/<ticket-id> pattern.
        let calls = branch.calls.lock().expect("lock").clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "derrick/ad-hoc/drk-001");
        assert_eq!(calls[0].1, "main");

        // Issue was created with the ticket's title/body.
        let inner = client.handle();
        let inner = inner.lock().await;
        assert_eq!(inner.create_calls.len(), 1);
        assert_eq!(inner.create_calls[0].0, "derrick/ad-hoc/drk-001");
        assert_eq!(inner.create_calls[0].1, "title");

        // Ticket is now InFlight, owned by the registered hand.
        let refreshed = substrate
            .get_ticket(&ticket.id)
            .await
            .expect("get")
            .expect("ticket exists");
        assert_eq!(refreshed.state, TicketState::InFlight);
        assert_eq!(refreshed.owner.as_ref(), Some(&result.hand));

        // The hand row is in the substrate.
        let hands = substrate.list_hands().await.expect("list hands");
        assert!(hands.iter().any(|h| h.id == result.hand));
    }

    #[tokio::test]
    async fn poll_task_transitions_to_in_review_when_pr_found() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let substrate = open_substrate(&tempdir).await;
        let ticket = make_ticket(&substrate, "drk-002").await;
        let branch = Arc::new(RecordingBranch::default());
        let client = Arc::new(FakeGhClient::new());

        // Queue: first poll returns None, second returns a PR.
        client
            .queue_poll_response("derrick/ad-hoc/drk-002", None)
            .await;
        let pr = PrInfo {
            number: 42,
            url: "https://example.test/pr/42".to_owned(),
            head_sha: "deadbeef".to_owned(),
        };
        client
            .queue_poll_response("derrick/ad-hoc/drk-002", Some(pr.clone()))
            .await;

        let dispatcher = CopilotHandDispatcher::new(
            Arc::clone(&substrate),
            Arc::clone(&branch) as Arc<dyn BranchCreator>,
            Arc::clone(&client) as Arc<dyn CopilotDispatchClient>,
            fast_config(),
        );
        let result = dispatcher
            .dispatch(&ctx(&ticket, tempdir.path()))
            .await
            .expect("dispatch");

        // Wait for the background poll task to transition the ticket.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let refreshed = substrate
                .get_ticket(&ticket.id)
                .await
                .expect("get")
                .expect("present");
            if refreshed.state == TicketState::InReview {
                // Verify metadata round-trips via the typed event log.
                let events = substrate
                    .ticket_events(&ticket.id, 50)
                    .await
                    .expect("events read");
                let found = events.iter().find_map(|event| match &event.kind {
                    EventKind::TicketTransitionedToInReview {
                        branch,
                        pr_url,
                        pr_number,
                        head_sha,
                    } => Some((branch.clone(), pr_url.clone(), *pr_number, head_sha.clone())),
                    _ => None,
                });
                let (branch, pr_url, pr_number, head_sha) =
                    found.expect("in-review metadata event present");
                assert_eq!(branch, "derrick/ad-hoc/drk-002");
                assert_eq!(pr_url.as_deref(), Some("https://example.test/pr/42"));
                assert_eq!(pr_number, Some(42));
                assert_eq!(head_sha, "deadbeef");
                // Heartbeat ran at least once.
                let inner = client.handle();
                let inner = inner.lock().await;
                assert!(inner.poll_call_count >= 1);
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("poll task did not transition ticket within deadline");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // The hand stayed the one we got back from dispatch.
        assert!(result.hand.as_str().starts_with("copilot-test-"));
    }

    #[tokio::test]
    async fn poll_task_respects_timeout() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let substrate = open_substrate(&tempdir).await;
        let ticket = make_ticket(&substrate, "drk-003").await;
        let branch = Arc::new(RecordingBranch::default());
        let client = Arc::new(FakeGhClient::new());

        // Queue several Nones and never a PR; rely on the timeout.
        for _ in 0..50 {
            client
                .queue_poll_response("derrick/ad-hoc/drk-003", None)
                .await;
        }

        let cfg = CopilotHandDispatcherConfig {
            poll_interval: Duration::from_millis(20),
            poll_timeout: Duration::from_millis(80),
            base_branch: "main".to_owned(),
            agent_identity: "copilot-test".to_owned(),
            branch_prefix: "derrick".to_owned(),
            roughneck_enabled: false,
            roughneck_level: "full".to_owned(),
        };
        let dispatcher = CopilotHandDispatcher::new(
            Arc::clone(&substrate),
            Arc::clone(&branch) as Arc<dyn BranchCreator>,
            Arc::clone(&client) as Arc<dyn CopilotDispatchClient>,
            cfg,
        );
        dispatcher
            .dispatch(&ctx(&ticket, tempdir.path()))
            .await
            .expect("dispatch");

        // Wait long enough for timeout, then assert ticket is still
        // InFlight (the poll task gave up but didn't transition).
        tokio::time::sleep(Duration::from_millis(300)).await;
        let refreshed = substrate
            .get_ticket(&ticket.id)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(refreshed.state, TicketState::InFlight);

        // An event was recorded about the timeout.
        let events = substrate
            .ticket_events(&ticket.id, 50)
            .await
            .expect("events");
        assert!(events.iter().any(|event| {
            matches!(&event.kind, EventKind::Note { body } if body.contains("copilot poll timed out"))
        }));
    }

    #[tokio::test]
    async fn dispatch_is_idempotent_on_existing_branch() {
        // The branch creator is called twice but both times reports
        // success; the dispatcher should treat that as fine and dispatch
        // both tickets independently.
        let tempdir = tempfile::tempdir().expect("tempdir");
        let substrate = open_substrate(&tempdir).await;
        let ticket1 = make_ticket(&substrate, "drk-010").await;
        let ticket2 = make_ticket(&substrate, "drk-011").await;
        let branch = Arc::new(RecordingBranch::default());
        let client = Arc::new(FakeGhClient::new());

        let dispatcher = CopilotHandDispatcher::new(
            Arc::clone(&substrate),
            Arc::clone(&branch) as Arc<dyn BranchCreator>,
            Arc::clone(&client) as Arc<dyn CopilotDispatchClient>,
            fast_config(),
        );

        dispatcher
            .dispatch(&ctx(&ticket1, tempdir.path()))
            .await
            .expect("dispatch 1");
        dispatcher
            .dispatch(&ctx(&ticket2, tempdir.path()))
            .await
            .expect("dispatch 2");

        let calls = branch.calls.lock().expect("lock").clone();
        assert_eq!(calls.len(), 2);
        // Both tickets are InFlight.
        let t1 = substrate
            .get_ticket(&ticket1.id)
            .await
            .expect("get")
            .expect("present");
        let t2 = substrate
            .get_ticket(&ticket2.id)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(t1.state, TicketState::InFlight);
        assert_eq!(t2.state, TicketState::InFlight);
        assert_ne!(t1.owner, t2.owner, "hands should be unique per dispatch");

        // Suppress unused warnings on test-only helpers.
        let _ = VecDeque::<i32>::new();
        let _ = Mutex::new(0);
    }
}
