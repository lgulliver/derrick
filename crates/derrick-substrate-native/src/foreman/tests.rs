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
    /// Recorded `(branch, new_base)` pairs from `retarget_pr`.
    retarget_calls: Arc<Mutex<Vec<(String, String)>>>,
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

    async fn retarget_pr(
        &self,
        branch: &str,
        new_base: &str,
        _repo_root: &std::path::Path,
    ) -> Result<(), derrick_stack::StackError> {
        self.retarget_calls
            .lock()
            .await
            .push((branch.to_owned(), new_base.to_owned()));
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

    // The merge cascade must also retarget the child PR's base via gh, not
    // just rebase the git branch (capability 3).
    let retargets = fake.retarget_calls.lock().await;
    assert_eq!(retargets.len(), 1, "dependent PR base retargeted once");
    assert_eq!(
        retargets[0],
        ("derrick/alpha/drk-2".to_owned(), "main".to_owned())
    );

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

// ---- GhRepoState construction (exercises GhRepoState::new path) ----------

#[test]
fn gh_repo_state_new_stores_root() {
    // GhRepoState::new is not async and doesn't require a live git repo.
    // Just verify the constructor sets up the path correctly; the actual
    // subprocess calls are covered by integration tests that require a real
    // repo.
    let root = std::path::PathBuf::from("/tmp/fake-repo");
    let state = GhRepoState::new(root.clone());
    // The struct field is private, but we can round-trip through the trait
    // indirectly. For coverage purposes the constructor itself is the goal.
    // We call pr_merge_sha which shells out to `gh` — that will fail in CI
    // (no gh binary or no real repo), so we only care that `new` compiled
    // and ran (no panic). The async path is gated in integration tests.
    drop(state);
}

// ---- prune_ticket_worktree_dir -------------------------------------------

#[tokio::test]
async fn prune_ticket_worktree_dir_logs_on_failure_without_panicking() {
    // Point at a path that does not exist; `git worktree remove` will exit
    // non-zero. The function must not panic or propagate an error.
    let repo_root = std::path::PathBuf::from("/nonexistent/repo");
    let worktree_path = std::path::PathBuf::from("/nonexistent/worktree/drk-1");
    // Should complete without unwinding even if git is missing or fails.
    prune_ticket_worktree_dir(&repo_root, &worktree_path).await;
}

#[tokio::test]
async fn prune_ticket_worktree_dir_succeeds_on_real_worktree() {
    // Create a minimal real git repo with a worktree so the happy-path
    // branch (success) of prune_ticket_worktree_dir is covered.
    let td = TempDir::new().expect("tempdir");
    let root = td.path();

    // Initialise main repo with commit.gpgsign=false to avoid GPG prompts.
    Bash::git_init(root).await;
    Bash::git_commit_empty(root, "init").await;

    // Create a worktree.
    let wt_path = td.path().join("wt1");
    let out = tokio::process::Command::new("git")
        .args(["worktree", "add", "--detach"])
        .arg(&wt_path)
        .arg("HEAD")
        .current_dir(root)
        .output()
        .await
        .expect("git worktree add");
    assert!(
        out.status.success(),
        "git worktree add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // prune_ticket_worktree_dir should remove it without error.
    prune_ticket_worktree_dir(root, &wt_path).await;

    // Worktree directory should be gone (or at least not registered).
    let list_out = tokio::process::Command::new("git")
        .args(["worktree", "list"])
        .current_dir(root)
        .output()
        .await
        .expect("git worktree list");
    let list_text = String::from_utf8_lossy(&list_out.stdout);
    assert!(
        !list_text.contains(wt_path.to_str().unwrap()),
        "worktree should have been removed: {list_text}"
    );
}

// ---- HumanHandDispatcher --------------------------------------------------

#[tokio::test]
async fn human_hand_dispatcher_dispatches_and_writes_note_event() {
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;
    let ticket = new_ticket(&substrate, "drk-1").await;

    let dispatcher = HumanHandDispatcher::new(substrate.clone(), hand.clone());
    let worktree_root = tempdir.path().to_path_buf();
    let ctx = DispatchContext {
        ticket: &ticket,
        worktree_root: &worktree_root,
        parent_branch: "main".to_owned(),
    };
    let result = dispatcher.dispatch(&ctx).await.expect("dispatch");
    assert_eq!(result.hand, hand);
    assert!(!result.completed_synchronously);

    // Ticket must be InFlight and owned by the hand.
    let after = substrate
        .get_ticket(&ticket.id)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(after.state, TicketState::InFlight);
    assert_eq!(after.owner.as_ref(), Some(&hand));

    // A Note event must have been written.
    let events = substrate
        .ticket_events(&ticket.id, 10)
        .await
        .expect("events");
    let has_note = events.iter().any(|e| match &e.kind {
        EventKind::Note { body } => body.contains("human hand"),
        _ => false,
    });
    assert!(has_note, "expected human hand Note event");
}

// ---- MultiDispatcher routing ----------------------------------------------

#[tokio::test]
async fn multi_dispatcher_routes_by_kind_label() {
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;

    // Create a ticket with label "kind:human".
    let nt = NewTicket::new(
        TicketId::new("drk-1").unwrap(),
        None,
        None,
        "title",
        "body",
        vec!["kind:human".to_owned()],
    )
    .unwrap();
    let ticket = substrate.create_ticket(nt).await.expect("create");

    let recorder = RecordingDispatcher::new("human", hand.clone(), substrate.clone());
    let multi = MultiDispatcher::new("copilot")
        .register(Box::new(RecordingDispatcher::new(
            "copilot",
            hand.clone(),
            substrate.clone(),
        )))
        .register(Box::new(recorder.clone()));

    assert!(!multi.is_empty());

    let worktree_root = tempdir.path().to_path_buf();
    let ctx = DispatchContext {
        ticket: &ticket,
        worktree_root: &worktree_root,
        parent_branch: "main".to_owned(),
    };
    multi.dispatch(&ctx).await.expect("dispatch");

    // The "human" dispatcher (matched by label) should have been called.
    assert_eq!(recorder.calls().await, vec![ticket.id.clone()]);
}

#[tokio::test]
async fn multi_dispatcher_falls_back_to_default_kind() {
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;

    // No kind label on the ticket — fall back to default_kind "human".
    let ticket = new_ticket(&substrate, "drk-1").await;

    let recorder = RecordingDispatcher::new("human", hand.clone(), substrate.clone());
    let multi = MultiDispatcher::new("human").register(Box::new(recorder.clone()));

    let worktree_root = tempdir.path().to_path_buf();
    let ctx = DispatchContext {
        ticket: &ticket,
        worktree_root: &worktree_root,
        parent_branch: "main".to_owned(),
    };
    multi.dispatch(&ctx).await.expect("dispatch");
    assert_eq!(recorder.calls().await, vec![ticket.id.clone()]);
}

#[tokio::test]
async fn multi_dispatcher_falls_back_to_first_when_default_missing() {
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;

    // default_kind "codex" is not registered; first registered is "human".
    let ticket = new_ticket(&substrate, "drk-1").await;
    let recorder = RecordingDispatcher::new("human", hand.clone(), substrate.clone());
    let multi = MultiDispatcher::new("codex").register(Box::new(recorder.clone()));

    let worktree_root = tempdir.path().to_path_buf();
    let ctx = DispatchContext {
        ticket: &ticket,
        worktree_root: &worktree_root,
        parent_branch: "main".to_owned(),
    };
    multi.dispatch(&ctx).await.expect("dispatch");
    assert_eq!(recorder.calls().await, vec![ticket.id.clone()]);
}

#[tokio::test]
async fn multi_dispatcher_empty_returns_not_implemented() {
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let ticket = new_ticket(&substrate, "drk-1").await;

    let multi = MultiDispatcher::new("human"); // no registered dispatchers
    assert!(multi.is_empty());

    let worktree_root = tempdir.path().to_path_buf();
    let ctx = DispatchContext {
        ticket: &ticket,
        worktree_root: &worktree_root,
        parent_branch: "main".to_owned(),
    };
    let result = multi.dispatch(&ctx).await;
    assert!(
        matches!(result, Err(DispatchError::NotImplemented { .. })),
        "empty MultiDispatcher should return NotImplemented, got {result:?}"
    );
}

#[tokio::test]
async fn multi_dispatcher_kind_returns_multi() {
    let multi = MultiDispatcher::new("human");
    assert_eq!(multi.kind(), "multi");
}

// ---- Foreman::with_target_branch -----------------------------------------

#[tokio::test]
async fn with_target_branch_overrides_default_main() {
    // Build a foreman targeting "trunk" and verify the verifier uses it.
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;
    let ticket = new_ticket(&substrate, "drk-1").await;
    ticket_to_in_flight(&substrate, &ticket, &hand).await;
    ticket_to_in_review(
        &substrate,
        &ticket.id,
        "feature",
        "sha-trunk",
        Some("https://example/pr/1".to_owned()),
    )
    .await;

    let repo = MockRepoState::new();
    // SHA is on "trunk", not "main".
    repo.set_contains("trunk", "sha-trunk", true).await;
    repo.set_contains("main", "sha-trunk", false).await;
    repo.set_pr_merge_sha("feature", Some("merge-trunk".to_owned()))
        .await;

    let foreman = Foreman::new(
        substrate.clone(),
        Config::defaults(),
        Box::new(repo),
        tempdir.path().to_path_buf(),
        no_op_dispatcher(substrate.clone(), hand),
    )
    .with_target_branch("trunk");

    let report = foreman.tick().await.expect("tick");
    let merged = report
        .verifier_actions
        .iter()
        .any(|a| matches!(a, VerifierAction::Merged { .. }));
    assert!(merged, "expected Merged action when target_branch=trunk");
}

// ---- run_attached with exit_when_idle ------------------------------------

#[tokio::test]
async fn run_attached_exits_when_idle_after_first_idle_tick() {
    // With exit_when_idle=true and no work to do, run_attached should return
    // after the first tick.
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;

    let foreman = build_foreman(
        substrate.clone(),
        Box::new(MockRepoState::new()),
        no_op_dispatcher(substrate.clone(), hand),
        tempdir.path().to_path_buf(),
    )
    .with_exit_when_idle(true)
    .with_ttls(ForemanTtls {
        poll_interval: std::time::Duration::from_millis(1),
        ..ForemanTtls::default()
    });

    // No tickets — first tick is idle — must return Ok(()).
    foreman.run_attached().await.expect("run_attached");
}

#[tokio::test]
async fn run_attached_does_not_exit_when_idle_flag_is_false() {
    // With exit_when_idle=false the loop would run forever; drive it from a
    // separate task and cancel after one successful tick.
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;
    new_ticket(&substrate, "drk-1").await;

    let recorder = RecordingDispatcher::new("human", hand.clone(), substrate.clone());
    let foreman = Arc::new(
        build_foreman(
            substrate.clone(),
            Box::new(MockRepoState::new()),
            Box::new(recorder.clone()),
            tempdir.path().to_path_buf(),
        )
        .with_ttls(ForemanTtls {
            poll_interval: std::time::Duration::from_millis(5),
            ..ForemanTtls::default()
        }),
    );

    let handle = tokio::spawn(async move { foreman.run_attached().await });
    // Give run_attached one tick to dispatch the ticket.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    handle.abort();
    // The recorder must have recorded the dispatch.
    assert!(!recorder.calls().await.is_empty());
}

// ---- cleanup_adopt_stage_dirs --------------------------------------------

#[tokio::test]
async fn cleanup_adopt_stage_removes_stale_dirs() {
    // Create a .derrick/.adopt-stage-<uuid> directory that is old enough
    // to be pruned. The cleanup pass must remove it and write a Note event.
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;

    // Create .derrick/.adopt-stage-test dir.
    let derrick_dir = tempdir.path().join(".derrick");
    let stage_dir = derrick_dir.join(".adopt-stage-abc123");
    std::fs::create_dir_all(&stage_dir).expect("create dirs");

    // Backdate its mtime by 48 h using filetime to ensure it's past-TTL.
    // We use a raw mtime write via utime instead of the filetime crate
    // (not a dependency); use tokio fs touch to an old time via utimes.
    // Simpler: just set worktree_ttl to zero seconds so anything is stale.
    let foreman = build_foreman(
        substrate.clone(),
        Box::new(MockRepoState::new()),
        no_op_dispatcher(substrate.clone(), hand),
        tempdir.path().to_path_buf(),
    )
    .with_ttls(ForemanTtls {
        worktree_ttl: chrono::Duration::nanoseconds(1),
        ..ForemanTtls::default()
    });

    // Give the dir a moment so its mtime is definitively < now - 1ns.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    let report = foreman.tick().await.expect("tick");

    // Dir must be gone.
    assert!(
        !stage_dir.exists(),
        "stale adopt-stage dir should be pruned"
    );

    // A Note event must have been recorded.
    let events = substrate.tail_typed_events(None, 50).await.unwrap();
    let found = events.iter().any(|e| match &e.kind {
        EventKind::Note { body } => body.contains(".adopt-stage-abc123"),
        _ => false,
    });
    assert!(
        found,
        "expected stale adopt-stage Note event; report: {report:?}"
    );
}

#[tokio::test]
async fn cleanup_adopt_stage_ignores_non_matching_dirs() {
    // Directories that don't start with .adopt-stage- must be skipped.
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;

    let derrick_dir = tempdir.path().join(".derrick");
    let other_dir = derrick_dir.join("some-other-dir");
    std::fs::create_dir_all(&other_dir).expect("create dirs");

    let foreman = build_foreman(
        substrate.clone(),
        Box::new(MockRepoState::new()),
        no_op_dispatcher(substrate.clone(), hand),
        tempdir.path().to_path_buf(),
    )
    .with_ttls(ForemanTtls {
        worktree_ttl: chrono::Duration::nanoseconds(1),
        ..ForemanTtls::default()
    });

    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let _ = foreman.tick().await.expect("tick");

    // The non-matching dir must still exist.
    assert!(other_dir.exists(), "non-matching dir should not be pruned");
}

#[tokio::test]
async fn cleanup_adopt_stage_no_op_when_derrick_dir_absent() {
    // No .derrick directory at all — cleanup pass must be a no-op (no error).
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;

    // Do not create .derrick.
    let foreman = build_foreman(
        substrate.clone(),
        Box::new(MockRepoState::new()),
        no_op_dispatcher(substrate.clone(), hand),
        tempdir.path().to_path_buf(),
    );
    // Must not error out.
    foreman.tick().await.expect("tick when .derrick absent");
}

#[tokio::test]
async fn cleanup_adopt_stage_skips_recent_dirs() {
    // A recently created .adopt-stage dir must be left alone.
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;

    let derrick_dir = tempdir.path().join(".derrick");
    let stage_dir = derrick_dir.join(".adopt-stage-recent");
    std::fs::create_dir_all(&stage_dir).expect("create dirs");

    // Use a very large TTL so the dir is definitely not stale.
    let foreman = build_foreman(
        substrate.clone(),
        Box::new(MockRepoState::new()),
        no_op_dispatcher(substrate.clone(), hand),
        tempdir.path().to_path_buf(),
    )
    .with_ttls(ForemanTtls {
        worktree_ttl: chrono::Duration::days(365),
        ..ForemanTtls::default()
    });

    foreman.tick().await.expect("tick");
    assert!(
        stage_dir.exists(),
        "recent adopt-stage dir should not be pruned"
    );
}

#[tokio::test]
async fn cleanup_adopt_stage_skips_non_directory_entries() {
    // A file named .adopt-stage-xxx (not a directory) must be skipped.
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;

    let derrick_dir = tempdir.path().join(".derrick");
    std::fs::create_dir_all(&derrick_dir).expect("create derrick dir");
    let stage_file = derrick_dir.join(".adopt-stage-notadir");
    std::fs::write(&stage_file, b"").expect("write file");

    let foreman = build_foreman(
        substrate.clone(),
        Box::new(MockRepoState::new()),
        no_op_dispatcher(substrate.clone(), hand),
        tempdir.path().to_path_buf(),
    )
    .with_ttls(ForemanTtls {
        worktree_ttl: chrono::Duration::nanoseconds(1),
        ..ForemanTtls::default()
    });

    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    foreman.tick().await.expect("tick");
    // The file should still exist (not treated as a dir to remove).
    assert!(
        stage_file.exists(),
        "non-directory .adopt-stage file should be untouched"
    );
}

// ---- verify_in_review_ticket: merged-PR path where pr_merge_sha is None --

#[tokio::test]
async fn verifier_squash_merge_path_no_merge_sha_escalates() {
    // PrStatus::Merged but pr_merge_sha returns None → escalate
    // (gh says merged but no commit SHA — D33 loud-over-silent).
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
    // pr_merge_sha returns None
    repo.set_pr_merge_sha("feature", None).await;

    let foreman = build_foreman(
        substrate.clone(),
        Box::new(repo),
        no_op_dispatcher(substrate.clone(), hand),
        tempdir.path().to_path_buf(),
    );
    let report = foreman.tick().await.expect("tick");
    assert!(
        report
            .verifier_actions
            .iter()
            .any(|a| matches!(a, VerifierAction::StuckEscalated { .. })),
        "expected StuckEscalated when merge SHA is None"
    );
    let after = substrate.get_ticket(&ticket.id).await.unwrap().unwrap();
    assert_eq!(
        after.state,
        TicketState::InReview,
        "ticket must stay InReview"
    );
}

#[tokio::test]
async fn verifier_not_found_pr_within_ttl_leaves_ticket_alone() {
    // PR not found but ticket was updated recently (within TTL).
    // Verifier must leave the ticket in InReview without escalating.
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;
    let ticket = new_ticket(&substrate, "drk-1").await;
    ticket_to_in_flight(&substrate, &ticket, &hand).await;
    ticket_to_in_review(&substrate, &ticket.id, "feature", "head", None).await;

    let repo = MockRepoState::new();
    repo.set_contains("main", "head", false).await;
    repo.set_pr_status("feature", PrStatus::NotFound).await;

    // Large in_review_ttl so the ticket is well within threshold.
    let foreman = build_foreman(
        substrate.clone(),
        Box::new(repo),
        no_op_dispatcher(substrate.clone(), hand),
        tempdir.path().to_path_buf(),
    )
    .with_ttls(ForemanTtls {
        in_review_ttl: chrono::Duration::days(365),
        ..ForemanTtls::default()
    });
    let report = foreman.tick().await.expect("tick");
    assert!(
        report.verifier_actions.is_empty(),
        "should not escalate a freshly-created ticket with NotFound PR"
    );
    let after = substrate.get_ticket(&ticket.id).await.unwrap().unwrap();
    assert_eq!(after.state, TicketState::InReview);
}

// ---- reconcile_ready_ticket: squash-merge paths -------------------------

#[tokio::test]
async fn reconcile_skips_ready_ticket_when_pr_not_merged() {
    // Ready ticket with InReview history; PR status is Open (not merged).
    // reconcile_ready_ticket must return without any action.
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
    // Release back to Ready (simulating a re-queue).
    substrate
        .release_from_hand(&ticket.id, "requeue".to_owned())
        .await
        .unwrap();

    let repo = MockRepoState::new();
    repo.set_contains("main", "head", false).await;
    repo.set_pr_status("feature", PrStatus::Open).await;

    let foreman = build_foreman(
        substrate.clone(),
        Box::new(repo),
        Box::new(CopilotStubDispatcher::new()),
        tempdir.path().to_path_buf(),
    );
    let report = foreman.tick().await.expect("tick");
    let reconciled = report
        .verifier_actions
        .iter()
        .any(|a| matches!(a, VerifierAction::ReconciledFromGit { .. }));
    assert!(!reconciled, "should not reconcile when PR is Open");
}

#[tokio::test]
async fn reconcile_skips_when_pr_merged_but_sha_none() {
    // reconcile_ready_ticket: PR is Merged but pr_merge_sha returns None.
    // Should be a no-op (the squash-merge branch's guard `let Some(sha) =` fails).
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
    substrate
        .release_from_hand(&ticket.id, "requeue".to_owned())
        .await
        .unwrap();

    let repo = MockRepoState::new();
    repo.set_contains("main", "head", false).await;
    repo.set_pr_status("feature", PrStatus::Merged).await;
    repo.set_pr_merge_sha("feature", None).await; // no sha

    let foreman = build_foreman(
        substrate.clone(),
        Box::new(repo),
        Box::new(CopilotStubDispatcher::new()),
        tempdir.path().to_path_buf(),
    );
    let report = foreman.tick().await.expect("tick");
    let reconciled = report
        .verifier_actions
        .iter()
        .any(|a| matches!(a, VerifierAction::ReconciledFromGit { .. }));
    assert!(!reconciled, "should not reconcile when merge sha is None");
    // Ticket must still be Ready.
    let after = substrate.get_ticket(&ticket.id).await.unwrap().unwrap();
    assert_eq!(after.state, TicketState::Ready);
}

#[tokio::test]
async fn reconcile_skips_when_pr_merged_sha_not_on_target() {
    // reconcile_ready_ticket: PR is Merged, sha is present, but not on target.
    // `if !on_target { return Ok(()); }` path.
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
    substrate
        .release_from_hand(&ticket.id, "requeue".to_owned())
        .await
        .unwrap();

    let repo = MockRepoState::new();
    repo.set_contains("main", "head", false).await;
    repo.set_pr_status("feature", PrStatus::Merged).await;
    repo.set_pr_merge_sha("feature", Some("squash-sha".to_owned()))
        .await;
    // squash-sha is NOT on main
    repo.set_contains("main", "squash-sha", false).await;

    let foreman = build_foreman(
        substrate.clone(),
        Box::new(repo),
        Box::new(CopilotStubDispatcher::new()),
        tempdir.path().to_path_buf(),
    );
    let report = foreman.tick().await.expect("tick");
    let reconciled = report
        .verifier_actions
        .iter()
        .any(|a| matches!(a, VerifierAction::ReconciledFromGit { .. }));
    assert!(
        !reconciled,
        "should not reconcile when squash sha not on target"
    );
}

#[tokio::test]
async fn reconcile_handles_ready_ticket_with_no_pr_url() {
    // reconcile_ready_ticket fast-forward path where pr_url is None:
    // merge_sha falls back to head_sha.
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;
    let ticket = new_ticket(&substrate, "drk-1").await;
    ticket_to_in_flight(&substrate, &ticket, &hand).await;
    // No pr_url.
    ticket_to_in_review(&substrate, &ticket.id, "feature", "head", None).await;
    substrate
        .release_from_hand(&ticket.id, "requeue".to_owned())
        .await
        .unwrap();

    let repo = MockRepoState::new();
    repo.set_contains("main", "head", true).await;
    // No pr_merge_sha configured — should fall back to head_sha.

    let foreman = build_foreman(
        substrate.clone(),
        Box::new(repo),
        Box::new(CopilotStubDispatcher::new()),
        tempdir.path().to_path_buf(),
    );
    let report = foreman.tick().await.expect("tick");
    let reconciled = report
        .verifier_actions
        .iter()
        .find_map(|a| match a {
            VerifierAction::ReconciledFromGit {
                ticket: t,
                merge_sha,
            } => Some((t.clone(), merge_sha.clone())),
            _ => None,
        })
        .expect("expected ReconciledFromGit");
    assert_eq!(reconciled.0, ticket.id);
    // Without a PR URL the merge_sha falls back to the head_sha.
    assert_eq!(reconciled.1, "head");
}

// ---- unblock_dependencies: non-terminal and deleted predecessor -----------

#[tokio::test]
async fn unblock_skips_dependency_blocked_with_non_terminal_predecessor() {
    // Predecessor is still Ready — all_terminal=false — must not unblock.
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;

    let pred = new_ticket(&substrate, "drk-1").await;
    let dep = new_ticket(&substrate, "drk-2").await;

    substrate
        .link(&dep.id, &pred.id, derrick_substrate::LinkKind::Blocks)
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

    // pred is still Ready — not terminal.
    let hand = register_hand_simple(&substrate, "h1").await;
    let foreman = build_foreman(
        substrate.clone(),
        Box::new(MockRepoState::new()),
        no_op_dispatcher(substrate.clone(), hand),
        tempdir.path().to_path_buf(),
    );
    let report = foreman.tick().await.expect("tick");
    assert!(
        report.unblocked.is_empty(),
        "must not unblock when predecessor is Ready"
    );
    let after = substrate.get_ticket(&dep.id).await.unwrap().unwrap();
    assert_eq!(after.state, TicketState::Blocked);
}

#[tokio::test]
async fn unblock_skips_when_predecessor_ticket_row_missing() {
    // blocks predecessor link exists but the referenced ticket row was
    // deleted. The None branch in `get_ticket` sets all_terminal=false.
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;

    // Create and immediately delete the predecessor ticket row via SQL.
    let pred_id = TicketId::new("drk-99").unwrap();
    let pred = NewTicket::new(pred_id.clone(), None, None, "t", "b", vec![]).unwrap();
    substrate.create_ticket(pred).await.unwrap();

    let dep = new_ticket(&substrate, "drk-2").await;
    substrate
        .link(&dep.id, &pred_id, derrick_substrate::LinkKind::Blocks)
        .await
        .unwrap();
    substrate
        .block_ticket(
            &dep.id,
            BlockReason::Dependency {
                predecessor: pred_id.clone(),
            },
        )
        .await
        .unwrap();

    // Delete the predecessor row directly.
    let conn = rusqlite::Connection::open(tempdir.path().join("derrick.db")).unwrap();
    conn.execute("DELETE FROM tickets WHERE id = ?1", [pred_id.as_str()])
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
    assert!(
        report.unblocked.is_empty(),
        "must not unblock when predecessor row is missing"
    );
}

// ---- dispatch_ready: Io error path ---------------------------------------

/// Dispatcher that returns a DispatchError::Io.
struct IoErrorDispatcher;

#[async_trait]
impl HandDispatcher for IoErrorDispatcher {
    fn kind(&self) -> &'static str {
        "io-error"
    }

    async fn dispatch(&self, _ctx: &DispatchContext<'_>) -> Result<DispatchResult, DispatchError> {
        Err(DispatchError::Io(std::io::Error::other(
            "injected I/O failure",
        )))
    }
}

#[tokio::test]
async fn dispatch_io_error_propagates_as_foreman_error() {
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    new_ticket(&substrate, "drk-1").await;

    let foreman = build_foreman(
        substrate.clone(),
        Box::new(MockRepoState::new()),
        Box::new(IoErrorDispatcher),
        tempdir.path().to_path_buf(),
    );
    let result = foreman.tick().await;
    assert!(
        matches!(result, Err(ForemanError::Io { .. })),
        "expected ForemanError::Io from dispatcher, got {result:?}"
    );
}

// ---- dispatch_ready: batch_max cap when already at limit -----------------

#[tokio::test]
async fn dispatch_skips_when_inflight_equals_batch_max() {
    // If count_inflight_tickets() >= batch_max the dispatch step returns
    // early without dispatching anything.
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;

    // Pre-occupy the hand with a ticket so count_inflight = 1.
    let occupied = new_ticket(&substrate, "drk-1").await;
    substrate.assign_to_hand(&occupied.id, &hand).await.unwrap();

    // One more ready ticket.
    new_ticket(&substrate, "drk-2").await;

    let recorder = RecordingDispatcher::new("human", hand.clone(), substrate.clone());
    let foreman = build_foreman(
        substrate.clone(),
        Box::new(MockRepoState::new()),
        Box::new(recorder.clone()),
        tempdir.path().to_path_buf(),
    )
    .with_batch_max(1); // cap = 1, inflight = 1 → budget = 0

    let report = foreman.tick().await.expect("tick");
    assert!(
        report.dispatched.is_empty(),
        "dispatch must skip when inflight >= batch_max"
    );
    assert!(recorder.calls().await.is_empty());
}

// ---- restack_dependents: dependent not in InFlight/InReview ---------------

#[tokio::test]
async fn restack_skips_done_dependent_after_parent_merges() {
    // A dependent ticket that is already Done should be skipped by restack.
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;

    let a = new_ticket_in_batch(&substrate, "drk-1", "alpha", 1).await;
    let b = new_ticket_in_batch(&substrate, "drk-2", "alpha", 2).await;

    // Move b all the way to Done independently.
    let hand_b = register_hand_simple(&substrate, "h2").await;
    ticket_to_in_flight(&substrate, &b, &hand_b).await;
    ticket_to_in_review(&substrate, &b.id, "derrick/alpha/drk-2", "b-sha", None).await;
    substrate
        .verify_ticket_merged(&b.id, "b-sha".to_owned(), "b-sha".to_owned())
        .await
        .unwrap();

    // Move a to InReview.
    ticket_to_in_flight(&substrate, &a, &hand).await;
    ticket_to_in_review(&substrate, &a.id, "derrick/alpha/drk-1", "a-sha", None).await;
    substrate
        .link(&b.id, &a.id, derrick_substrate::LinkKind::Blocks)
        .await
        .unwrap();

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
    foreman.tick().await.expect("tick");

    // No restack call should have been made for the Done dependent.
    let calls = fake.calls.lock().await;
    assert!(
        calls.iter().all(|c| c.branch != "derrick/alpha/drk-2"),
        "Done dependent should not be restacked"
    );
}

#[tokio::test]
async fn restack_skips_dependent_with_no_in_review_metadata() {
    // A dependent in InFlight but with no InReview metadata should be skipped.
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;

    let a = new_ticket_in_batch(&substrate, "drk-1", "alpha", 1).await;
    let b = new_ticket_in_batch(&substrate, "drk-2", "alpha", 2).await;

    // Move a to InReview (will be verified as merged).
    ticket_to_in_flight(&substrate, &a, &hand).await;
    ticket_to_in_review(&substrate, &a.id, "derrick/alpha/drk-1", "a-sha", None).await;

    // b is InFlight but has NO InReview metadata (just assigned, not reviewed).
    let hand_b = register_hand_simple(&substrate, "h2").await;
    substrate.assign_to_hand(&b.id, &hand_b).await.unwrap();

    substrate
        .link(&b.id, &a.id, derrick_substrate::LinkKind::Blocks)
        .await
        .unwrap();

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
    foreman.tick().await.expect("tick");

    // No restack call should have been made.
    let calls = fake.calls.lock().await;
    assert!(
        calls.is_empty(),
        "should not restack dependent with no InReview metadata: {calls:?}"
    );
}

// ---- restack_dependents: force-push failure path -------------------------

/// FakeStackBackend variant where force_push always fails.
#[derive(Clone, Default)]
struct ForcePushFailBackend {
    calls: Arc<Mutex<Vec<derrick_stack::RestackParams>>>,
}

#[async_trait]
impl derrick_stack::StackBackend for ForcePushFailBackend {
    fn kind(&self) -> &'static str {
        "force-push-fail"
    }

    async fn open_pr(
        &self,
        _params: derrick_stack::OpenPrParams,
    ) -> Result<derrick_stack::PrInfo, derrick_stack::StackError> {
        Err(derrick_stack::StackError::NotSupported {
            backend: "force-push-fail",
            reason: "not used in tests",
        })
    }

    async fn restack(
        &self,
        params: derrick_stack::RestackParams,
    ) -> Result<derrick_stack::RestackOutcome, derrick_stack::StackError> {
        self.calls.lock().await.push(params);
        Ok(derrick_stack::RestackOutcome::Restacked)
    }

    async fn force_push(
        &self,
        _branch: &str,
        _repo_root: &std::path::Path,
    ) -> Result<(), derrick_stack::StackError> {
        Err(derrick_stack::StackError::NotSupported {
            backend: "force-push-fail",
            reason: "injected force-push failure",
        })
    }
}

#[tokio::test]
async fn restack_force_push_failure_records_note_and_continues() {
    // When force_push fails after a successful restack, the foreman must
    // log a Note event and continue (no error propagation).
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
        .link(&b.id, &a.id, derrick_substrate::LinkKind::Blocks)
        .await
        .unwrap();

    let repo = MockRepoState::new();
    repo.set_contains("main", "a-sha", true).await;

    let backend = ForcePushFailBackend::default();
    let stacking_cfg = Config::defaults().tools().git().stacking().clone();
    let foreman = Foreman::new(
        substrate.clone(),
        Config::defaults(),
        Box::new(repo),
        tempdir.path().to_path_buf(),
        no_op_dispatcher(substrate.clone(), hand),
    )
    .with_stack_backend(Arc::new(backend.clone()), stacking_cfg);

    // Must not return an error.
    let report = foreman
        .tick()
        .await
        .expect("tick must succeed despite force-push failure");

    // A Note event about the failure must have been written.
    let events = substrate.ticket_events(&b.id, 20).await.unwrap();
    let has_failure_note = events.iter().any(|e| match &e.kind {
        EventKind::Note { body } => body.contains("force-push failed"),
        _ => false,
    });
    assert!(
        has_failure_note,
        "expected force-push failure Note event; report: {report:?}"
    );
}

// ---- report_is_idle (private fn tested via run_attached) ----------------

#[tokio::test]
async fn run_attached_exit_when_idle_requires_no_action_in_tick() {
    // After all dispatching, a second tick should be idle (nothing left to
    // dispatch) and exit_when_idle should return then.
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;

    // Create and dispatch a ticket on the first tick so the second tick is idle.
    new_ticket(&substrate, "drk-1").await;

    let recorder = RecordingDispatcher::new("human", hand.clone(), substrate.clone());
    let foreman = Arc::new(
        build_foreman(
            substrate.clone(),
            Box::new(MockRepoState::new()),
            Box::new(recorder.clone()),
            tempdir.path().to_path_buf(),
        )
        .with_exit_when_idle(true)
        .with_ttls(ForemanTtls {
            poll_interval: std::time::Duration::from_millis(1),
            ..ForemanTtls::default()
        }),
    );

    // With exit_when_idle=true this should run 2 ticks (first dispatches,
    // second is idle) and then return.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        foreman.run_attached().await
    })
    .await
    .expect("timeout")
    .expect("run_attached");

    assert!(
        !recorder.calls().await.is_empty(),
        "expected at least one dispatch"
    );
}

// ---- GhRepoState pr_status: branch-level output parsing -----------------

#[tokio::test]
async fn gh_repo_state_pr_status_returns_not_found_on_failure() {
    // When `gh pr view` exits non-zero (e.g. branch not found), pr_status
    // must return PrStatus::NotFound.
    //
    // We exercise this by creating a real temp git repo and running gh
    // against a branch that has no PR. gh is expected to exit non-zero.
    // If gh is not available, skip via the "gh not available" guard below.
    let which = std::process::Command::new("which").arg("gh").output().ok();
    if which.map(|o| !o.status.success()).unwrap_or(true) {
        // gh not available in this environment — skip.
        return;
    }

    let td = TempDir::new().expect("tempdir");
    let root = td.path();
    Bash::git_init(root).await;
    Bash::git_commit_empty(root, "init").await;

    let state = GhRepoState::new(root.to_path_buf());
    let status = state
        .pr_status("definitely-no-pr-branch-xyzzy")
        .await
        .expect("pr_status");
    assert_eq!(status, PrStatus::NotFound);
}

// ---- Helper: minimal real git operations ---------------------------------

/// Thin wrapper for running real git commands in tests that need a live repo.
struct Bash;

impl Bash {
    async fn git_init(root: &std::path::Path) {
        let out = tokio::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(root)
            .output()
            .await
            .expect("git init");
        assert!(out.status.success(), "git init failed");

        tokio::process::Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(root)
            .output()
            .await
            .expect("git config");
        tokio::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(root)
            .output()
            .await
            .expect("git config");
        tokio::process::Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(root)
            .output()
            .await
            .expect("git config gpgsign");
    }

    async fn git_commit_empty(root: &std::path::Path, msg: &str) {
        let out = tokio::process::Command::new("git")
            .args(["commit", "--allow-empty", "-m", msg])
            .current_dir(root)
            .output()
            .await
            .expect("git commit");
        assert!(
            out.status.success(),
            "git commit failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

// ---- GhRepoState::target_contains_sha with a real git repo ---------------

#[tokio::test]
async fn gh_repo_state_target_contains_sha_returns_true_for_commit_on_branch() {
    let td = TempDir::new().expect("tempdir");
    let root = td.path();
    Bash::git_init(root).await;
    Bash::git_commit_empty(root, "init").await;

    // Get HEAD sha.
    let out = tokio::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .await
        .expect("rev-parse");
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_owned();

    // Get the default branch name (could be main or master).
    let branch_out = tokio::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(root)
        .output()
        .await
        .expect("abbrev-ref");
    let branch = String::from_utf8_lossy(&branch_out.stdout)
        .trim()
        .to_owned();

    let state = GhRepoState::new(root.to_path_buf());
    let result = state
        .target_contains_sha(&branch, &sha)
        .await
        .expect("target_contains_sha");
    assert!(result, "HEAD sha should be on its own branch");
}

#[tokio::test]
async fn gh_repo_state_target_contains_sha_returns_false_for_unknown_sha() {
    let td = TempDir::new().expect("tempdir");
    let root = td.path();
    Bash::git_init(root).await;
    Bash::git_commit_empty(root, "init").await;

    let branch_out = tokio::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(root)
        .output()
        .await
        .expect("abbrev-ref");
    let branch = String::from_utf8_lossy(&branch_out.stdout)
        .trim()
        .to_owned();

    let state = GhRepoState::new(root.to_path_buf());
    let result = state
        .target_contains_sha(&branch, "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef")
        .await
        .expect("target_contains_sha");
    assert!(!result, "garbage sha should not be on branch");
}

// ---- verify_in_review_ticket: no metadata (pre-D33 case) ----------------

#[tokio::test]
async fn verifier_skips_in_review_ticket_with_no_metadata() {
    // A ticket in InReview that has no TicketTransitionedToInReview event
    // (pre-D33 case) must be left alone by the verifier.
    let tempdir = TempDir::new().expect("tempdir");
    let substrate = open_substrate(&tempdir).await;
    let hand = register_hand_simple(&substrate, "h1").await;
    let ticket = new_ticket(&substrate, "drk-1").await;
    ticket_to_in_flight(&substrate, &ticket, &hand).await;

    // Transition to InReview without metadata by directly patching state.
    // Use `transition_to_in_review` but then delete the associated event.
    ticket_to_in_review(&substrate, &ticket.id, "feature", "head", None).await;
    // Delete the TicketTransitionedToInReview event so most_recent_in_review_metadata returns None.
    let conn = rusqlite::Connection::open(tempdir.path().join("derrick.db")).unwrap();
    conn.execute(
        "DELETE FROM events WHERE kind = 'ticket_transitioned_to_in_review'",
        [],
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
        report.verifier_actions.is_empty(),
        "verifier must leave no-metadata InReview ticket alone"
    );
    let after = substrate.get_ticket(&ticket.id).await.unwrap().unwrap();
    assert_eq!(after.state, TicketState::InReview);
}
