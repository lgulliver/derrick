//! Foreman tests. Real SQLite via tempfile; inline `RepoState` and
//! `HandDispatcher` mocks (no external mocking crates per AGENTS.md).

// `CopilotStubDispatcher` is deprecated in favour of
// `derrick_copilot::CopilotHandDispatcher` (T013), but these tests
// intentionally exercise the stub's NotImplemented path. Keep the
// deprecation visible at compile time elsewhere.
#![allow(deprecated)]

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use derrick_config::Config;
use derrick_substrate::{
    BlockReason, EventKind, Hand, HandId, HandKind, InReviewMetadata, LinkKind, NewTicket,
    Substrate, SubstrateError, Ticket, TicketId, TicketState,
};
use tempfile::TempDir;
use tokio::sync::Mutex;

use super::*;
use crate::{NativeConfig, NativeSubstrate};

// ---- Fixtures -------------------------------------------------------------

fn site_fixture() -> derrick_config::Site {
    Config::defaults().site().clone()
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
            .expect("open substrate"),
    )
}

async fn new_ticket(substrate: &NativeSubstrate, id: &str) -> Ticket {
    let ticket = NewTicket::new(
        TicketId::new(id).expect("ticket id"),
        None,
        None,
        "title",
        "body",
        Vec::new(),
    )
    .expect("new ticket");
    substrate
        .create_ticket(ticket)
        .await
        .expect("create ticket")
}

async fn ticket_to_in_flight(substrate: &NativeSubstrate, ticket: &Ticket, hand: &HandId) {
    substrate
        .assign_to_hand(&ticket.id, hand)
        .await
        .expect("assign");
}

async fn ticket_to_in_review(
    substrate: &NativeSubstrate,
    id: &TicketId,
    branch: &str,
    head_sha: &str,
    pr_url: Option<String>,
) {
    substrate
        .transition_to_in_review(
            id,
            InReviewMetadata {
                branch: branch.to_owned(),
                pr_url,
                pr_number: None,
                head_sha: head_sha.to_owned(),
            },
        )
        .await
        .expect("transition to in review");
}

// ---- Mock RepoState -------------------------------------------------------

#[derive(Clone, Default)]
struct MockRepoState {
    inner: Arc<Mutex<MockRepoStateInner>>,
}

#[derive(Default)]
struct MockRepoStateInner {
    /// Map of (target_branch, sha) -> bool.
    contains: HashMap<(String, String), bool>,
    /// Map of branch -> PrStatus.
    pr_status: HashMap<String, PrStatus>,
    /// Map of branch -> Option<merge_sha>.
    pr_merge_sha: HashMap<String, Option<String>>,
}

impl MockRepoState {
    fn new() -> Self {
        Self::default()
    }

    async fn set_contains(&self, target: &str, sha: &str, value: bool) {
        let mut inner = self.inner.lock().await;
        inner
            .contains
            .insert((target.to_owned(), sha.to_owned()), value);
    }

    async fn set_pr_status(&self, branch: &str, status: PrStatus) {
        let mut inner = self.inner.lock().await;
        inner.pr_status.insert(branch.to_owned(), status);
    }

    async fn set_pr_merge_sha(&self, branch: &str, sha: Option<String>) {
        let mut inner = self.inner.lock().await;
        inner.pr_merge_sha.insert(branch.to_owned(), sha);
    }
}

#[async_trait]
impl RepoState for MockRepoState {
    async fn target_contains_sha(
        &self,
        target_branch: &str,
        head_sha: &str,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let inner = self.inner.lock().await;
        Ok(inner
            .contains
            .get(&(target_branch.to_owned(), head_sha.to_owned()))
            .copied()
            .unwrap_or(false))
    }

    async fn pr_status(
        &self,
        branch: &str,
    ) -> Result<PrStatus, Box<dyn std::error::Error + Send + Sync>> {
        let inner = self.inner.lock().await;
        Ok(inner
            .pr_status
            .get(branch)
            .copied()
            .unwrap_or(PrStatus::NotFound))
    }

    async fn pr_merge_sha(
        &self,
        branch: &str,
    ) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> {
        let inner = self.inner.lock().await;
        Ok(inner.pr_merge_sha.get(branch).cloned().unwrap_or(None))
    }
}

// ---- Mock HandDispatcher --------------------------------------------------

#[derive(Clone)]
struct RecordingDispatcher {
    kind: &'static str,
    hand: HandId,
    substrate: Arc<NativeSubstrate>,
    calls: Arc<Mutex<Vec<TicketId>>>,
}

impl RecordingDispatcher {
    fn new(kind: &'static str, hand: HandId, substrate: Arc<NativeSubstrate>) -> Self {
        Self {
            kind,
            hand,
            substrate,
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    async fn calls(&self) -> Vec<TicketId> {
        self.calls.lock().await.clone()
    }
}

#[async_trait]
impl HandDispatcher for RecordingDispatcher {
    fn kind(&self) -> &'static str {
        self.kind
    }

    async fn dispatch(&self, ctx: &DispatchContext<'_>) -> Result<DispatchResult, DispatchError> {
        let ticket = ctx.ticket;
        // Transition the ticket to InFlight so subsequent ticks see it as
        // owned (matches the real human dispatcher contract). Tolerate the
        // race where a concurrent tick already moved it out of Ready —
        // surface as Ok with `completed_synchronously: false` so the
        // foreman doesn't propagate the error.
        match self.substrate.assign_to_hand(&ticket.id, &self.hand).await {
            Ok(_) => {
                self.calls.lock().await.push(ticket.id.clone());
                Ok(DispatchResult {
                    hand: self.hand.clone(),
                    completed_synchronously: false,
                })
            }
            Err(SubstrateError::Invalid { field, .. }) if field == "state" => {
                // Lost the race — another tick already dispatched this
                // ticket. Return success without recording the call.
                Ok(DispatchResult {
                    hand: self.hand.clone(),
                    completed_synchronously: false,
                })
            }
            Err(error) => Err(error.into()),
        }
    }
}

// ---- Helpers --------------------------------------------------------------

fn build_foreman(
    substrate: Arc<NativeSubstrate>,
    repo_state: Box<dyn RepoState>,
    dispatcher: Box<dyn HandDispatcher>,
    repo_root: std::path::PathBuf,
) -> Foreman {
    Foreman::new(
        substrate,
        Config::defaults(),
        repo_state,
        repo_root,
        dispatcher,
    )
}

fn no_op_dispatcher(substrate: Arc<NativeSubstrate>, hand: HandId) -> Box<dyn HandDispatcher> {
    Box::new(RecordingDispatcher::new("human", hand, substrate))
}

// ---- Verifier tests -------------------------------------------------------

#[tokio::test]
async fn verifier_marks_merged_via_target_contains_sha() {
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;
    let ticket = new_ticket(&substrate, "drk-1").await;
    ticket_to_in_flight(&substrate, &ticket, &hand).await;
    ticket_to_in_review(
        &substrate,
        &ticket.id,
        "feature",
        "abc123",
        Some("https://example/pr/1".to_owned()),
    )
    .await;

    let repo = MockRepoState::new();
    repo.set_contains("main", "abc123", true).await;
    repo.set_pr_merge_sha("feature", Some("merge-xyz".to_owned()))
        .await;

    let foreman = build_foreman(
        substrate.clone(),
        Box::new(repo.clone()),
        no_op_dispatcher(substrate.clone(), hand.clone()),
        tempdir.path().to_path_buf(),
    );
    let report = foreman.tick().await.expect("tick");

    let merged = report
        .verifier_actions
        .iter()
        .find_map(|action| match action {
            VerifierAction::Merged { ticket, merge_sha } => {
                Some((ticket.clone(), merge_sha.clone()))
            }
            _ => None,
        })
        .expect("merged action");
    assert_eq!(merged.0, ticket.id);
    assert_eq!(merged.1, "merge-xyz");

    let after = substrate
        .get_ticket(&ticket.id)
        .await
        .expect("get")
        .unwrap();
    assert_eq!(after.state, TicketState::Done);
    assert_eq!(after.merge_sha.as_deref(), Some("merge-xyz"));
}

#[tokio::test]
async fn verifier_handles_squash_merge() {
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;
    let ticket = new_ticket(&substrate, "drk-1").await;
    ticket_to_in_flight(&substrate, &ticket, &hand).await;
    ticket_to_in_review(
        &substrate,
        &ticket.id,
        "feature",
        "head-sha",
        Some("https://example/pr/1".to_owned()),
    )
    .await;

    let repo = MockRepoState::new();
    // Head SHA is not on target.
    repo.set_contains("main", "head-sha", false).await;
    // But the PR reports merged with a different merge commit.
    repo.set_pr_status("feature", PrStatus::Merged).await;
    repo.set_pr_merge_sha("feature", Some("squash-merge".to_owned()))
        .await;
    repo.set_contains("main", "squash-merge", true).await;

    let foreman = build_foreman(
        substrate.clone(),
        Box::new(repo),
        no_op_dispatcher(substrate.clone(), hand),
        tempdir.path().to_path_buf(),
    );
    let report = foreman.tick().await.expect("tick");

    let merge_sha = report
        .verifier_actions
        .iter()
        .find_map(|a| match a {
            VerifierAction::Merged { merge_sha, .. } => Some(merge_sha.clone()),
            _ => None,
        })
        .expect("merged");
    assert_eq!(merge_sha, "squash-merge");
    let after = substrate.get_ticket(&ticket.id).await.unwrap().unwrap();
    assert_eq!(after.state, TicketState::Done);
    assert_eq!(after.merge_sha.as_deref(), Some("squash-merge"));
}

#[tokio::test]
async fn verifier_marks_blocked_when_pr_closed_unmerged() {
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;
    let ticket = new_ticket(&substrate, "drk-1").await;
    ticket_to_in_flight(&substrate, &ticket, &hand).await;
    ticket_to_in_review(
        &substrate,
        &ticket.id,
        "feature",
        "abc",
        Some("https://example/pr/1".to_owned()),
    )
    .await;

    let repo = MockRepoState::new();
    repo.set_contains("main", "abc", false).await;
    repo.set_pr_status("feature", PrStatus::ClosedUnmerged)
        .await;

    let foreman = build_foreman(
        substrate.clone(),
        Box::new(repo),
        no_op_dispatcher(substrate.clone(), hand),
        tempdir.path().to_path_buf(),
    );
    let report = foreman.tick().await.expect("tick");

    assert!(matches!(
        report.verifier_actions.first(),
        Some(VerifierAction::Unmerged { .. })
    ));
    let after = substrate.get_ticket(&ticket.id).await.unwrap().unwrap();
    assert_eq!(after.state, TicketState::Blocked);
    assert!(matches!(
        after.block_reason,
        Some(BlockReason::PrClosedUnmerged { .. })
    ));
}

#[tokio::test]
async fn verifier_escalates_when_gh_merged_but_target_lacks_sha() {
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;
    let ticket = new_ticket(&substrate, "drk-1").await;
    ticket_to_in_flight(&substrate, &ticket, &hand).await;
    ticket_to_in_review(
        &substrate,
        &ticket.id,
        "feature",
        "head",
        Some("https://example/pr/1".to_owned()),
    )
    .await;

    let repo = MockRepoState::new();
    repo.set_contains("main", "head", false).await;
    repo.set_pr_status("feature", PrStatus::Merged).await;
    // gh has a merge commit but it's not on target (e.g. force-push).
    repo.set_pr_merge_sha("feature", Some("merge".to_owned()))
        .await;
    repo.set_contains("main", "merge", false).await;

    let foreman = build_foreman(
        substrate.clone(),
        Box::new(repo),
        no_op_dispatcher(substrate.clone(), hand),
        tempdir.path().to_path_buf(),
    );
    let report = foreman.tick().await.expect("tick");

    assert!(matches!(
        report.verifier_actions.first(),
        Some(VerifierAction::StuckEscalated { .. })
    ));
    let after = substrate.get_ticket(&ticket.id).await.unwrap().unwrap();
    assert_eq!(after.state, TicketState::InReview);
}

#[tokio::test]
async fn verifier_escalates_stuck_in_review_past_ttl() {
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;
    let ticket = new_ticket(&substrate, "drk-1").await;
    ticket_to_in_flight(&substrate, &ticket, &hand).await;
    ticket_to_in_review(&substrate, &ticket.id, "feature", "head", None).await;

    let repo = MockRepoState::new();
    repo.set_contains("main", "head", false).await;
    repo.set_pr_status("feature", PrStatus::NotFound).await;

    // Tiny TTL: anything older than 0 seconds is past-TTL.
    let foreman = build_foreman(
        substrate.clone(),
        Box::new(repo),
        no_op_dispatcher(substrate.clone(), hand),
        tempdir.path().to_path_buf(),
    )
    .with_ttls(ForemanTtls {
        in_review_ttl: chrono::Duration::nanoseconds(1),
        ..ForemanTtls::default()
    });
    // Sleep briefly so the ticket's updated_at is past the threshold.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let report = foreman.tick().await.expect("tick");
    assert!(
        report
            .verifier_actions
            .iter()
            .any(|a| matches!(a, VerifierAction::StuckEscalated { .. }))
    );
}

#[tokio::test]
async fn verifier_leaves_pr_open_tickets_alone() {
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;
    let ticket = new_ticket(&substrate, "drk-1").await;
    ticket_to_in_flight(&substrate, &ticket, &hand).await;
    ticket_to_in_review(&substrate, &ticket.id, "feature", "head", None).await;

    let repo = MockRepoState::new();
    repo.set_contains("main", "head", false).await;
    repo.set_pr_status("feature", PrStatus::Open).await;

    let foreman = build_foreman(
        substrate.clone(),
        Box::new(repo),
        no_op_dispatcher(substrate.clone(), hand),
        tempdir.path().to_path_buf(),
    );
    let report = foreman.tick().await.expect("tick");
    assert!(report.verifier_actions.is_empty());
    let after = substrate.get_ticket(&ticket.id).await.unwrap().unwrap();
    assert_eq!(after.state, TicketState::InReview);
}

// ---- Cleanup tests --------------------------------------------------------

#[tokio::test]
async fn cleanup_prunes_abandoned_worktrees_past_ttl() {
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;

    // Reserve a worktree, then make its created_at look old via direct
    // SQL update — done by writing a stale timestamp via raw connection.
    let path = substrate
        .reserve_worktree("run-1", "feature")
        .await
        .expect("reserve");
    // Backdate the row by directly opening the DB. Use NativeSubstrate's
    // db_path — accessible via a fresh rusqlite connection here.
    let stale_text = (Utc::now() - chrono::Duration::hours(48)).to_rfc3339();
    let db_path = tempdir.path().join("derrick.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "UPDATE worktrees SET created_at = ?1 WHERE run_id = ?2",
        rusqlite::params![stale_text, "run-1"],
    )
    .unwrap();
    drop(conn);

    let hand = register_hand_simple(&substrate, "h1").await;
    let foreman = build_foreman(
        substrate.clone(),
        Box::new(MockRepoState::new()),
        no_op_dispatcher(substrate.clone(), hand),
        tempdir.path().to_path_buf(),
    );
    let report = foreman.tick().await.expect("tick");
    let pruned: Vec<_> = report
        .cleanup_actions
        .iter()
        .filter_map(|a| match a {
            CleanupAction::PrunedAbandonedWorktree { run_id } => Some(run_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(pruned, vec!["run-1".to_owned()]);
    // Worktree row should be gone.
    let remaining = substrate.list_worktrees(true).await.unwrap();
    assert!(remaining.iter().all(|w| w.run_id != "run-1"));
    // Silence unused variable lint.
    let _ = path;
}

#[tokio::test]
async fn cleanup_prunes_abandoned_ticket_worktree_past_ttl() {
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;

    // A ticket-keyed hand worktree row (the backstop case: dispatcher crashed
    // before its deterministic terminal-state removal could forget the row).
    let wt_path = tempdir.path().join("host-worktrees").join("drk-1");
    substrate
        .register_ticket_worktree("drk-1", "derrick/ad-hoc/drk-1", &wt_path)
        .await
        .expect("register ticket worktree");

    let stale_text = (Utc::now() - chrono::Duration::hours(48)).to_rfc3339();
    let conn = rusqlite::Connection::open(tempdir.path().join("derrick.db")).unwrap();
    conn.execute(
        "UPDATE worktrees SET created_at = ?1 WHERE run_id = ?2",
        rusqlite::params![stale_text, "ticket:drk-1"],
    )
    .unwrap();
    drop(conn);

    let hand = register_hand_simple(&substrate, "h1").await;
    let foreman = build_foreman(
        substrate.clone(),
        Box::new(MockRepoState::new()),
        no_op_dispatcher(substrate.clone(), hand),
        tempdir.path().to_path_buf(),
    );
    let report = foreman.tick().await.expect("tick");
    let pruned: Vec<_> = report
        .cleanup_actions
        .iter()
        .filter_map(|a| match a {
            CleanupAction::PrunedAbandonedWorktree { run_id } => Some(run_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(pruned, vec!["ticket:drk-1".to_owned()]);
    let remaining = substrate.list_worktrees(true).await.unwrap();
    assert!(remaining.iter().all(|w| w.run_id != "ticket:drk-1"));
}

#[tokio::test]
async fn cleanup_requeues_inflight_with_dead_hand() {
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;
    let ticket = new_ticket(&substrate, "drk-1").await;
    ticket_to_in_flight(&substrate, &ticket, &hand).await;

    // Backdate hand's last_seen.
    let stale = (Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    let db_path = tempdir.path().join("derrick.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "UPDATE hands SET last_seen = ?1 WHERE id = ?2",
        rusqlite::params![stale, hand.as_str()],
    )
    .unwrap();
    drop(conn);

    let foreman = build_foreman(
        substrate.clone(),
        Box::new(MockRepoState::new()),
        no_op_dispatcher(substrate.clone(), hand.clone()),
        tempdir.path().to_path_buf(),
    );
    let report = foreman.tick().await.expect("tick");
    let requeued: Vec<_> = report
        .cleanup_actions
        .iter()
        .filter_map(|a| match a {
            CleanupAction::RequeuedAbandonedHand { ticket, hand: h } => {
                Some((ticket.clone(), h.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(requeued.len(), 1);
    assert_eq!(requeued[0].0, ticket.id);
    assert_eq!(requeued[0].1, hand);
    let after = substrate.get_ticket(&ticket.id).await.unwrap().unwrap();
    // Ticket was released back to Ready, then re-dispatched within the same
    // tick by the recording dispatcher (which moves it to InFlight again).
    assert_eq!(after.state, TicketState::InFlight);
}

#[tokio::test]
async fn cleanup_triggers_eager_verifier_on_stale_in_review() {
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;
    let ticket = new_ticket(&substrate, "drk-1").await;
    ticket_to_in_flight(&substrate, &ticket, &hand).await;
    ticket_to_in_review(&substrate, &ticket.id, "feature", "abc", None).await;

    // Backdate the ticket's updated_at.
    let stale = (Utc::now() - chrono::Duration::hours(48)).to_rfc3339();
    let db_path = tempdir.path().join("derrick.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "UPDATE tickets SET updated_at = ?1 WHERE id = ?2",
        rusqlite::params![stale, ticket.id.as_str()],
    )
    .unwrap();
    drop(conn);

    let foreman = build_foreman(
        substrate.clone(),
        Box::new(MockRepoState::new()),
        no_op_dispatcher(substrate.clone(), hand),
        tempdir.path().to_path_buf(),
    );
    let report = foreman.tick().await.expect("tick");
    assert!(
        report
            .cleanup_actions
            .iter()
            .any(|a| matches!(a, CleanupAction::TriggeredStaleInReviewCheck { .. }))
    );
}

// ---- Unblock tests --------------------------------------------------------

#[tokio::test]
async fn cleanup_does_not_unblock_pr_closed_unmerged_ticket() {
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;
    let ticket = new_ticket(&substrate, "drk-1").await;
    ticket_to_in_flight(&substrate, &ticket, &hand).await;
    ticket_to_in_review(&substrate, &ticket.id, "feature", "head", None).await;
    substrate
        .verify_ticket_unmerged(&ticket.id, "feature".to_owned(), None)
        .await
        .unwrap();

    let foreman = build_foreman(
        substrate.clone(),
        Box::new(MockRepoState::new()),
        no_op_dispatcher(substrate.clone(), hand),
        tempdir.path().to_path_buf(),
    );
    let report = foreman.tick().await.expect("tick");
    assert!(report.unblocked.is_empty());
    let after = substrate.get_ticket(&ticket.id).await.unwrap().unwrap();
    assert_eq!(after.state, TicketState::Blocked);
}

#[tokio::test]
async fn cleanup_unblocks_only_dependency_blocked_tickets() {
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;

    // Predecessor ticket: Done.
    let pred = new_ticket(&substrate, "drk-1").await;
    ticket_to_in_flight(&substrate, &pred, &hand).await;
    ticket_to_in_review(
        &substrate,
        &pred.id,
        "feature-a",
        "shaA",
        Some("u".to_owned()),
    )
    .await;
    substrate
        .verify_ticket_merged(&pred.id, "shaA".to_owned(), "shaA".to_owned())
        .await
        .unwrap();

    // Dependency-blocked ticket pointing at the predecessor.
    let dep = new_ticket(&substrate, "drk-2").await;
    substrate
        .link(&dep.id, &pred.id, LinkKind::Blocks)
        .await
        .unwrap();
    substrate
        .block_ticket(
            &dep.id,
            BlockReason::Dependency {
                predecessor: pred.id.clone(),
            },
        )
        .await
        .unwrap();

    // PR-closed-unmerged ticket. Must NOT auto-unblock.
    let pcu = new_ticket(&substrate, "drk-3").await;
    ticket_to_in_flight(&substrate, &pcu, &hand).await;
    ticket_to_in_review(&substrate, &pcu.id, "feature-c", "shaC", None).await;
    substrate
        .verify_ticket_unmerged(&pcu.id, "feature-c".to_owned(), None)
        .await
        .unwrap();

    let foreman = build_foreman(
        substrate.clone(),
        Box::new(MockRepoState::new()),
        no_op_dispatcher(substrate.clone(), hand),
        tempdir.path().to_path_buf(),
    );
    let report = foreman.tick().await.expect("tick");
    assert_eq!(report.unblocked, vec![dep.id.clone()]);
    let pcu_after = substrate.get_ticket(&pcu.id).await.unwrap().unwrap();
    assert_eq!(pcu_after.state, TicketState::Blocked);
}

#[tokio::test]
async fn unblocked_tickets_become_ready() {
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;
    let pred = new_ticket(&substrate, "drk-1").await;
    ticket_to_in_flight(&substrate, &pred, &hand).await;
    ticket_to_in_review(
        &substrate,
        &pred.id,
        "feature-a",
        "shaA",
        Some("u".to_owned()),
    )
    .await;
    substrate
        .verify_ticket_merged(&pred.id, "shaA".to_owned(), "shaA".to_owned())
        .await
        .unwrap();

    let dep = new_ticket(&substrate, "drk-2").await;
    substrate
        .link(&dep.id, &pred.id, LinkKind::Blocks)
        .await
        .unwrap();
    substrate
        .block_ticket(
            &dep.id,
            BlockReason::Dependency {
                predecessor: pred.id.clone(),
            },
        )
        .await
        .unwrap();

    let foreman = build_foreman(
        substrate.clone(),
        Box::new(MockRepoState::new()),
        no_op_dispatcher(substrate.clone(), hand),
        tempdir.path().to_path_buf(),
    );
    foreman.tick().await.expect("tick");
    let after = substrate.get_ticket(&dep.id).await.unwrap().unwrap();
    // Either Ready (if dispatch didn't fire), or InFlight (re-dispatched).
    assert!(matches!(
        after.state,
        TicketState::Ready | TicketState::InFlight
    ));
}

// ---- Dispatch tests -------------------------------------------------------

#[tokio::test]
async fn dispatch_respects_batch_max_parallelism() {
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;
    // Create 5 ready tickets.
    for n in 1..=5 {
        new_ticket(&substrate, &format!("drk-{n}")).await;
    }

    let recorder = RecordingDispatcher::new("human", hand, substrate.clone());
    let dispatcher: Box<dyn HandDispatcher> = Box::new(recorder.clone());
    let foreman = build_foreman(
        substrate.clone(),
        Box::new(MockRepoState::new()),
        dispatcher,
        tempdir.path().to_path_buf(),
    )
    .with_batch_max(3);
    let report = foreman.tick().await.expect("tick");
    assert_eq!(report.dispatched.len(), 3);
    assert_eq!(recorder.calls().await.len(), 3);
}

#[tokio::test]
async fn dispatch_orders_by_ordinal_then_created_at() {
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;

    // Create a batch with three tickets, ordinals 3, 1, 2.
    let batch = derrick_substrate::BatchName::new("b1").unwrap();
    substrate.create_batch(batch.clone()).await.unwrap();
    for (id, ord) in [("drk-1", 3u32), ("drk-2", 1), ("drk-3", 2)] {
        let nt = NewTicket::new(
            TicketId::new(id).unwrap(),
            Some(batch.clone()),
            Some(ord),
            "title",
            "body",
            Vec::new(),
        )
        .unwrap();
        substrate.create_ticket(nt).await.unwrap();
    }

    let recorder = RecordingDispatcher::new("human", hand, substrate.clone());
    let dispatcher: Box<dyn HandDispatcher> = Box::new(recorder.clone());
    let foreman = build_foreman(
        substrate.clone(),
        Box::new(MockRepoState::new()),
        dispatcher,
        tempdir.path().to_path_buf(),
    );
    foreman.tick().await.expect("tick");
    let calls = recorder.calls().await;
    let ids: Vec<&str> = calls.iter().map(|t| t.as_str()).collect();
    assert_eq!(ids, vec!["drk-2", "drk-3", "drk-1"]);
}

#[tokio::test]
async fn dispatch_copilot_stub_surfaces_t013_pointer() {
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let _hand = register_hand_simple(&substrate, "h1").await;
    new_ticket(&substrate, "drk-1").await;

    let foreman = build_foreman(
        substrate.clone(),
        Box::new(MockRepoState::new()),
        Box::new(CopilotStubDispatcher::new()),
        tempdir.path().to_path_buf(),
    );
    let report = foreman.tick().await.expect("tick");
    // The stub fails dispatch but the tick still completes; nothing was
    // dispatched, and a Note event recording the T013 pointer should exist.
    assert!(report.dispatched.is_empty());
    let events = substrate
        .ticket_events(&TicketId::new("drk-1").unwrap(), 10)
        .await
        .unwrap();
    let found_pointer = events.iter().any(|event| match &event.kind {
        EventKind::Note { body } => body.contains("not implemented in v1") && body.contains("T013"),
        _ => false,
    });
    assert!(found_pointer, "expected T013 pointer in events: {events:?}");
}

// ---- D33 reconciliation tests --------------------------------------------

#[tokio::test]
async fn pre_dispatch_reconciliation_done_for_requeued_ready_ticket() {
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;
    let ticket = new_ticket(&substrate, "drk-1").await;
    ticket_to_in_flight(&substrate, &ticket, &hand).await;
    ticket_to_in_review(
        &substrate,
        &ticket.id,
        "feature",
        "head",
        Some("https://example/pr/1".to_owned()),
    )
    .await;
    // Cleanup-style requeue: release back to Ready.
    substrate
        .release_from_hand(&ticket.id, "test requeue".to_owned())
        .await
        .unwrap();

    let repo = MockRepoState::new();
    repo.set_contains("main", "head", true).await;
    repo.set_pr_merge_sha("feature", Some("merge-x".to_owned()))
        .await;

    // Use the stub dispatcher to make sure reconciliation runs before
    // dispatch.
    let foreman = build_foreman(
        substrate.clone(),
        Box::new(repo),
        Box::new(CopilotStubDispatcher::new()),
        tempdir.path().to_path_buf(),
    );
    let report = foreman.tick().await.expect("tick");
    let found = report.verifier_actions.iter().any(|a| {
        matches!(a, VerifierAction::ReconciledFromGit { ticket: t, merge_sha }
            if t == &ticket.id && merge_sha == "merge-x")
    });
    assert!(
        found,
        "expected reconciled action: {:?}",
        report.verifier_actions
    );
    let after = substrate.get_ticket(&ticket.id).await.unwrap().unwrap();
    assert_eq!(after.state, TicketState::Done);
    assert_eq!(after.merge_sha.as_deref(), Some("merge-x"));
}

#[tokio::test]
async fn pre_dispatch_reconciliation_skips_ready_ticket_with_no_inreview_history() {
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;
    let ticket = new_ticket(&substrate, "drk-1").await;

    let recorder = RecordingDispatcher::new("human", hand, substrate.clone());
    let dispatcher: Box<dyn HandDispatcher> = Box::new(recorder.clone());
    let foreman = build_foreman(
        substrate.clone(),
        Box::new(MockRepoState::new()),
        dispatcher,
        tempdir.path().to_path_buf(),
    );
    let report = foreman.tick().await.expect("tick");
    assert!(report.verifier_actions.is_empty());
    // Dispatched normally.
    assert_eq!(recorder.calls().await, vec![ticket.id]);
}

// ---- Tick determinism + concurrency --------------------------------------

#[tokio::test]
async fn tick_against_canned_substrate_produces_canned_report() {
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;
    new_ticket(&substrate, "drk-1").await;
    new_ticket(&substrate, "drk-2").await;

    let recorder = RecordingDispatcher::new("human", hand, substrate.clone());
    let dispatcher: Box<dyn HandDispatcher> = Box::new(recorder.clone());
    let foreman = build_foreman(
        substrate.clone(),
        Box::new(MockRepoState::new()),
        dispatcher,
        tempdir.path().to_path_buf(),
    );
    let report = foreman.tick().await.expect("tick");
    assert!(report.cleanup_actions.is_empty());
    assert!(report.verifier_actions.is_empty());
    assert!(report.unblocked.is_empty());
    assert_eq!(report.dispatched.len(), 2);
}

#[tokio::test]
async fn parallel_ticks_serialise_through_writer_mutex() {
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;
    new_ticket(&substrate, "drk-1").await;
    new_ticket(&substrate, "drk-2").await;
    new_ticket(&substrate, "drk-3").await;

    let recorder = RecordingDispatcher::new("human", hand, substrate.clone());
    let foreman_a = Arc::new(build_foreman(
        substrate.clone(),
        Box::new(MockRepoState::new()),
        Box::new(recorder.clone()),
        tempdir.path().to_path_buf(),
    ));
    let recorder2 =
        RecordingDispatcher::new("human", HandId::new("h1").unwrap(), substrate.clone());
    let foreman_b = Arc::new(build_foreman(
        substrate.clone(),
        Box::new(MockRepoState::new()),
        Box::new(recorder2.clone()),
        tempdir.path().to_path_buf(),
    ));

    let h1 = tokio::spawn({
        let foreman = foreman_a.clone();
        async move { foreman.tick().await }
    });
    let h2 = tokio::spawn({
        let foreman = foreman_b.clone();
        async move { foreman.tick().await }
    });
    let (r1, r2) = (h1.await.unwrap(), h2.await.unwrap());
    r1.expect("tick a");
    r2.expect("tick b");
    // Every ticket landed in InFlight exactly once (the writer mutex
    // serialises assign_to_hand transitions; the dispatcher tolerates
    // the lost-race surface).
    for id in ["drk-1", "drk-2", "drk-3"] {
        let t = substrate
            .get_ticket(&TicketId::new(id).unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(t.state, TicketState::InFlight);
    }
    let mut all: Vec<TicketId> = recorder.calls().await;
    all.extend(recorder2.calls().await);
    all.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    all.dedup();
    assert_eq!(all.len(), 3);
}

// ---- Small helper: simpler register without unreachable_default ----------

async fn register_hand_simple(substrate: &NativeSubstrate, id: &str) -> HandId {
    let hand_id = HandId::new(id).expect("hand id");
    substrate
        .register_hand(Hand {
            id: hand_id.clone(),
            kind: HandKind::Human,
            last_seen: Some(Utc::now()),
        })
        .await
        .expect("register hand");
    hand_id
}

// ---- Stack / restack tests (T014) ----------------------------------------

/// Recording dispatcher that captures the parent_branch from
/// `DispatchContext` so tests can assert the foreman computed it correctly.
#[derive(Clone, Default)]
struct ParentBranchRecorder {
    last_parent: Arc<Mutex<Option<String>>>,
}

#[async_trait]
impl HandDispatcher for ParentBranchRecorder {
    fn kind(&self) -> &'static str {
        "test"
    }

    async fn dispatch(&self, ctx: &DispatchContext<'_>) -> Result<DispatchResult, DispatchError> {
        *self.last_parent.lock().await = Some(ctx.parent_branch.clone());
        // Don't actually transition; return NotImplemented so the foreman
        // surfaces a Note event but the ticket stays in Ready. This lets us
        // inspect parent_branch in isolation.
        Err(DispatchError::NotImplemented { kind: "test" })
    }
}

/// Fake stack backend that records restack calls and can be programmed to
/// return `Conflict`.
#[derive(Clone, Default)]
struct FakeStackBackend {
    calls: Arc<Mutex<Vec<derrick_stack::RestackParams>>>,
    force_conflict: Arc<Mutex<bool>>,
}

#[async_trait]
impl derrick_stack::StackBackend for FakeStackBackend {
    fn kind(&self) -> &'static str {
        "fake"
    }

    async fn open_pr(
        &self,
        _params: derrick_stack::OpenPrParams,
    ) -> Result<derrick_stack::PrInfo, derrick_stack::StackError> {
        Err(derrick_stack::StackError::NotSupported {
            backend: "fake",
            reason: "not used in tests",
        })
    }

    async fn restack(
        &self,
        params: derrick_stack::RestackParams,
    ) -> Result<derrick_stack::RestackOutcome, derrick_stack::StackError> {
        let conflict = *self.force_conflict.lock().await;
        self.calls.lock().await.push(params.clone());
        if conflict {
            Ok(derrick_stack::RestackOutcome::Conflict {
                recipe: format!(
                    "git rebase --onto {} {} {}",
                    params.new_parent, params.old_parent, params.branch
                ),
            })
        } else {
            Ok(derrick_stack::RestackOutcome::Restacked)
        }
    }

    async fn force_push(
        &self,
        _branch: &str,
        _repo_root: &std::path::Path,
    ) -> Result<(), derrick_stack::StackError> {
        Ok(())
    }
}

async fn new_ticket_in_batch(
    substrate: &NativeSubstrate,
    id: &str,
    batch: &str,
    ordinal: u32,
) -> Ticket {
    use derrick_substrate::{BatchName, NewTicket};
    let batch_name = BatchName::new(batch).expect("batch");
    // Best-effort batch creation; ignore "already exists" so multiple
    // tickets can share a batch.
    let _ = substrate.create_batch(batch_name.clone()).await;
    let ticket = NewTicket::new(
        TicketId::new(id).expect("ticket id"),
        Some(batch_name),
        Some(ordinal),
        "title",
        "body",
        Vec::new(),
    )
    .expect("new ticket");
    substrate
        .create_ticket(ticket)
        .await
        .expect("create ticket")
}

#[tokio::test]
async fn dispatch_uses_parent_branch_from_predecessor() {
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;

    // A is the predecessor, B depends on A.
    let a = new_ticket_in_batch(&substrate, "drk-1", "alpha", 1).await;
    let b = new_ticket_in_batch(&substrate, "drk-2", "alpha", 2).await;
    // Mark A as Done by sending it through the full path.
    ticket_to_in_flight(&substrate, &a, &hand).await;
    ticket_to_in_review(&substrate, &a.id, "derrick/alpha/drk-1", "a-sha", None).await;
    substrate
        .verify_ticket_merged(&a.id, "a-sha".to_owned(), "a-sha".to_owned())
        .await
        .expect("merge a");
    // Link B -> A via `blocks`.
    substrate
        .link(&b.id, &a.id, LinkKind::Blocks)
        .await
        .expect("link");

    let recorder = ParentBranchRecorder::default();
    let foreman = Foreman::new(
        substrate.clone(),
        Config::defaults(),
        Box::new(MockRepoState::new()),
        tempdir.path().to_path_buf(),
        Box::new(recorder.clone()),
    );
    let _ = foreman.tick().await.expect("tick");

    let parent = recorder
        .last_parent
        .lock()
        .await
        .clone()
        .expect("parent branch recorded");
    assert_eq!(parent, "derrick/alpha/drk-1");
}

#[tokio::test]
async fn restack_dependents_called_after_merge() {
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;

    let a = new_ticket_in_batch(&substrate, "drk-1", "alpha", 1).await;
    let b = new_ticket_in_batch(&substrate, "drk-2", "alpha", 2).await;

    // B depends on A. B is already InReview with its own branch.
    ticket_to_in_flight(&substrate, &a, &hand).await;
    ticket_to_in_review(&substrate, &a.id, "derrick/alpha/drk-1", "a-sha", None).await;
    let hand_b = register_hand_simple(&substrate, "h2").await;
    ticket_to_in_flight(&substrate, &b, &hand_b).await;
    ticket_to_in_review(&substrate, &b.id, "derrick/alpha/drk-2", "b-sha", None).await;
    substrate
        .link(&b.id, &a.id, LinkKind::Blocks)
        .await
        .expect("link");

    // A is currently InReview pending merge.
    let repo = MockRepoState::new();
    repo.set_contains("main", "a-sha", true).await;

    let fake = FakeStackBackend::default();
    let stacking_cfg = Config::defaults().tools().git().stacking().clone();
    let foreman = Foreman::new(
        substrate.clone(),
        Config::defaults(),
        Box::new(repo),
        tempdir.path().to_path_buf(),
        no_op_dispatcher(substrate.clone(), hand),
    )
    .with_stack_backend(Arc::new(fake.clone()), stacking_cfg);
    let report = foreman.tick().await.expect("tick");

    let calls = fake.calls.lock().await;
    assert_eq!(calls.len(), 1, "exactly one dependent restacked");
    assert_eq!(calls[0].branch, "derrick/alpha/drk-2");
    assert_eq!(calls[0].old_parent, "derrick/alpha/drk-1");
    assert_eq!(calls[0].new_parent, "main");

    assert!(
        report
            .verifier_actions
            .iter()
            .any(|action| matches!(action, VerifierAction::Restacked { .. }))
    );
}

#[tokio::test]
async fn restack_conflict_blocks_ticket() {
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;

    let a = new_ticket_in_batch(&substrate, "drk-1", "alpha", 1).await;
    let b = new_ticket_in_batch(&substrate, "drk-2", "alpha", 2).await;
    ticket_to_in_flight(&substrate, &a, &hand).await;
    ticket_to_in_review(&substrate, &a.id, "derrick/alpha/drk-1", "a-sha", None).await;
    let hand_b = register_hand_simple(&substrate, "h2").await;
    ticket_to_in_flight(&substrate, &b, &hand_b).await;
    ticket_to_in_review(&substrate, &b.id, "derrick/alpha/drk-2", "b-sha", None).await;
    substrate
        .link(&b.id, &a.id, LinkKind::Blocks)
        .await
        .expect("link");

    let repo = MockRepoState::new();
    repo.set_contains("main", "a-sha", true).await;

    let fake = FakeStackBackend::default();
    *fake.force_conflict.lock().await = true;
    let stacking_cfg = Config::defaults().tools().git().stacking().clone();
    let foreman = Foreman::new(
        substrate.clone(),
        Config::defaults(),
        Box::new(repo),
        tempdir.path().to_path_buf(),
        no_op_dispatcher(substrate.clone(), hand),
    )
    .with_stack_backend(Arc::new(fake.clone()), stacking_cfg);
    let report = foreman.tick().await.expect("tick");

    let after = substrate
        .get_ticket(&b.id)
        .await
        .expect("get")
        .expect("b present");
    assert_eq!(after.state, TicketState::Blocked);
    match after.block_reason {
        Some(BlockReason::RestackConflict { recipe }) => {
            assert!(recipe.contains("git rebase --onto"));
        }
        other => panic!("expected RestackConflict, got {other:?}"),
    }
    assert!(
        report
            .verifier_actions
            .iter()
            .any(|action| matches!(action, VerifierAction::RestackConflict { .. }))
    );
}

// ---- Fan-out concurrency proof -------------------------------------------

/// Dispatcher that sleeps for `delay` before dispatching. Used by the
/// wall-time concurrency test to prove that dispatches run in parallel.
#[derive(Clone)]
struct SleepingDispatcher {
    delay: std::time::Duration,
    hand: HandId,
    substrate: Arc<NativeSubstrate>,
}

impl SleepingDispatcher {
    fn new(delay: std::time::Duration, hand: HandId, substrate: Arc<NativeSubstrate>) -> Self {
        Self {
            delay,
            hand,
            substrate,
        }
    }
}

#[async_trait]
impl HandDispatcher for SleepingDispatcher {
    fn kind(&self) -> &'static str {
        "sleeping"
    }

    async fn dispatch(&self, ctx: &DispatchContext<'_>) -> Result<DispatchResult, DispatchError> {
        tokio::time::sleep(self.delay).await;
        match self
            .substrate
            .assign_to_hand(&ctx.ticket.id, &self.hand)
            .await
        {
            Ok(_) => Ok(DispatchResult {
                hand: self.hand.clone(),
                completed_synchronously: false,
            }),
            // Tolerate lost races (another concurrent task won the assignment).
            Err(SubstrateError::Invalid { field, .. }) if field == "state" => Ok(DispatchResult {
                hand: self.hand.clone(),
                completed_synchronously: false,
            }),
            Err(error) => Err(error.into()),
        }
    }
}

/// Proves that `dispatch_ready` fans out N ticket dispatches concurrently:
/// total wall time must be less than N × per-ticket delay.
///
/// With 4 tickets each sleeping 50 ms, sequential dispatch would take ≥ 200 ms.
/// Concurrent dispatch finishes in ~50 ms. We use 180 ms as the ceiling to
/// give the CI runner plenty of headroom while still catching any regression
/// back to sequential dispatch.
#[tokio::test]
async fn dispatch_ready_fans_out_concurrently() {
    let per_ticket_delay = std::time::Duration::from_millis(50);
    let ticket_count: u32 = 4;
    // Sequential floor: N × delay (200 ms). Ceiling with headroom: 180 ms.
    // A passing run should finish around 50–60 ms; we allow up to 180 ms.
    let sequential_floor = per_ticket_delay * ticket_count;
    let ceiling = sequential_floor * 9 / 10; // 90% of sequential = 180 ms

    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;

    for n in 1..=ticket_count {
        new_ticket(&substrate, &format!("drk-{n}")).await;
    }

    let foreman = build_foreman(
        substrate.clone(),
        Box::new(MockRepoState::new()),
        Box::new(SleepingDispatcher::new(
            per_ticket_delay,
            hand,
            substrate.clone(),
        )),
        tempdir.path().to_path_buf(),
    )
    .with_batch_max(ticket_count);

    let start = std::time::Instant::now();
    let report = foreman.tick().await.expect("tick");
    let elapsed = start.elapsed();

    assert_eq!(
        report.dispatched.len(),
        ticket_count as usize,
        "all {ticket_count} tickets should be dispatched"
    );
    assert!(
        elapsed < ceiling,
        "wall time {elapsed:?} >= ceiling {ceiling:?}; dispatches appear sequential"
    );
}
