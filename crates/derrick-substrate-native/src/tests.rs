//! Test module for the native substrate. Real SQLite via `tempfile`.

use std::num::NonZeroUsize;
use std::sync::Arc;

use chrono::Utc;
use derrick_config::Config;
use derrick_substrate::{
    BlockReason, EventKind, EventScope, ForemanMode, Hand, HandKind, InReviewMetadata,
    ManualDoneAttestation, NewEvent, NewTicket, Substrate, SubstrateError, TicketFilter,
    TicketState,
};
use rusqlite::params;
use tempfile::TempDir;
use uuid::Uuid;

use super::*;

fn site_fixture() -> Site {
    Config::defaults().site().clone()
}

fn native_config_fixture(tempdir: &TempDir) -> NativeConfig {
    NativeConfig {
        db_path: tempdir.path().join("derrick.db"),
        worktree_root: tempdir.path().join("worktrees"),
    }
}

async fn open_substrate(tempdir: &TempDir) -> Result<NativeSubstrate, SubstrateError> {
    NativeSubstrate::open(native_config_fixture(tempdir), site_fixture()).await
}

fn ticket_id(value: &str) -> Result<TicketId, SubstrateError> {
    TicketId::new(value)
}

fn batch_name(value: &str) -> Result<BatchName, SubstrateError> {
    BatchName::new(value)
}

fn hand_id(value: &str) -> Result<HandId, SubstrateError> {
    HandId::new(value)
}

fn new_ticket(value: &str) -> Result<NewTicket, SubstrateError> {
    NewTicket::new(ticket_id(value)?, None, None, "title", "body", Vec::new())
}

fn new_batched_ticket(
    value: &str,
    batch: &str,
    ordinal: Option<u32>,
) -> Result<NewTicket, SubstrateError> {
    NewTicket::new(
        ticket_id(value)?,
        Some(batch_name(batch)?),
        ordinal,
        "title",
        "body",
        Vec::new(),
    )
}

fn io_error(error: std::io::Error) -> SubstrateError {
    SubstrateError::Backend(Box::new(error))
}

async fn register_hand_fixture(
    substrate: &NativeSubstrate,
    id: &str,
) -> Result<HandId, SubstrateError> {
    let h = hand_id(id)?;
    substrate
        .register_hand(Hand {
            id: h.clone(),
            kind: HandKind::Human,
            last_seen: None,
        })
        .await?;
    Ok(h)
}

/// Move a ticket to InReview via the typed methods.
async fn move_to_in_review(
    substrate: &NativeSubstrate,
    ticket: &TicketId,
    hand: &HandId,
    head_sha: &str,
) -> Result<(), SubstrateError> {
    substrate.assign_to_hand(ticket, hand).await?;
    substrate
        .transition_to_in_review(
            ticket,
            InReviewMetadata {
                branch: format!("derrick/{}", ticket.as_str()),
                pr_url: Some("https://example/pr/1".to_owned()),
                pr_number: Some(1),
                head_sha: head_sha.to_owned(),
            },
        )
        .await?;
    Ok(())
}

// ---------------------- set_ticket_state narrowing ----------------------

#[tokio::test]
async fn set_ticket_state_done_refused_with_d31_message() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    substrate.create_ticket(new_ticket("drk-1")?).await?;
    let result = substrate
        .set_ticket_state(&ticket_id("drk-1")?, TicketState::Done, None)
        .await;
    match result {
        Err(SubstrateError::Invalid { message, .. }) => {
            assert!(message.contains("verify_ticket_merged"));
        }
        other => panic!("expected Invalid, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn set_ticket_state_rejected_refused_with_d31_message() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    substrate.create_ticket(new_ticket("drk-1")?).await?;
    let result = substrate
        .set_ticket_state(&ticket_id("drk-1")?, TicketState::Rejected, None)
        .await;
    match result {
        Err(SubstrateError::Invalid { message, .. }) => {
            assert!(message.contains("reject_ticket"));
        }
        other => panic!("expected Invalid, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn set_ticket_state_in_review_refused_pointing_at_transition_to_in_review(
) -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    substrate.create_ticket(new_ticket("drk-1")?).await?;
    let result = substrate
        .set_ticket_state(&ticket_id("drk-1")?, TicketState::InReview, None)
        .await;
    match result {
        Err(SubstrateError::Invalid { message, .. }) => {
            assert!(message.contains("transition_to_in_review"));
        }
        other => panic!("expected Invalid, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn set_ticket_state_no_op_returns_ok() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    substrate.create_ticket(new_ticket("drk-1")?).await?;
    let ticket = substrate
        .set_ticket_state(&ticket_id("drk-1")?, TicketState::Ready, None)
        .await?;
    assert_eq!(ticket.state, TicketState::Ready);
    Ok(())
}

// ---------------------- verify_ticket_merged ----------------------

#[tokio::test]
async fn verify_ticket_merged_transitions_in_review_to_done() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    let h = register_hand_fixture(&substrate, "h1").await?;
    substrate.create_ticket(new_ticket("drk-1")?).await?;
    move_to_in_review(&substrate, &ticket_id("drk-1")?, &h, "sha-head").await?;

    let ticket = substrate
        .verify_ticket_merged(
            &ticket_id("drk-1")?,
            "sha-head".to_owned(),
            "sha-merge".to_owned(),
        )
        .await?;
    assert_eq!(ticket.state, TicketState::Done);
    assert_eq!(ticket.merge_sha.as_deref(), Some("sha-merge"));
    Ok(())
}

#[tokio::test]
async fn verify_ticket_merged_refuses_when_not_in_review() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    substrate.create_ticket(new_ticket("drk-1")?).await?;
    let result = substrate
        .verify_ticket_merged(
            &ticket_id("drk-1")?,
            "sha-head".to_owned(),
            "sha-merge".to_owned(),
        )
        .await;
    assert!(matches!(result, Err(SubstrateError::Invalid { .. })));
    Ok(())
}

#[tokio::test]
async fn verify_ticket_merged_stores_distinct_head_and_merge_shas() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    let h = register_hand_fixture(&substrate, "h1").await?;
    substrate.create_ticket(new_ticket("drk-1")?).await?;
    move_to_in_review(&substrate, &ticket_id("drk-1")?, &h, "sha-head").await?;

    substrate
        .verify_ticket_merged(
            &ticket_id("drk-1")?,
            "sha-head".to_owned(),
            "sha-merge-squash".to_owned(),
        )
        .await?;
    let events = substrate.ticket_events(&ticket_id("drk-1")?, 50).await?;
    let found = events.iter().any(|e| match &e.kind {
        EventKind::TicketVerifiedMerged {
            head_sha,
            merge_sha,
        } => head_sha == "sha-head" && merge_sha == "sha-merge-squash",
        _ => false,
    });
    assert!(found, "expected TicketVerifiedMerged event with both shas");
    let ticket = substrate
        .get_ticket(&ticket_id("drk-1")?)
        .await?
        .expect("ticket present");
    assert_eq!(ticket.merge_sha.as_deref(), Some("sha-merge-squash"));
    Ok(())
}

// ---------------------- mark_ticket_done_manually ----------------------

#[tokio::test]
async fn mark_ticket_done_manually_records_attestation_in_event() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    substrate.create_ticket(new_ticket("drk-1")?).await?;

    substrate
        .mark_ticket_done_manually(
            &ticket_id("drk-1")?,
            ManualDoneAttestation {
                claimant: "alice".to_owned(),
                note: "done locally".to_owned(),
            },
        )
        .await?;
    let events = substrate.ticket_events(&ticket_id("drk-1")?, 50).await?;
    let found = events.iter().any(|e| match &e.kind {
        EventKind::TicketMarkedDoneManually { claimant, note } => {
            claimant == "alice" && note == "done locally"
        }
        _ => false,
    });
    assert!(found);
    Ok(())
}

#[tokio::test]
async fn mark_ticket_done_manually_refuses_when_already_terminal() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    substrate.create_ticket(new_ticket("drk-1")?).await?;
    substrate
        .mark_ticket_done_manually(
            &ticket_id("drk-1")?,
            ManualDoneAttestation {
                claimant: "alice".to_owned(),
                note: "first".to_owned(),
            },
        )
        .await?;
    let result = substrate
        .mark_ticket_done_manually(
            &ticket_id("drk-1")?,
            ManualDoneAttestation {
                claimant: "alice".to_owned(),
                note: "second".to_owned(),
            },
        )
        .await;
    assert!(matches!(result, Err(SubstrateError::Invalid { .. })));
    Ok(())
}

// ---------------------- transition_to_in_review ----------------------

#[tokio::test]
async fn transition_to_in_review_records_metadata_in_event() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    let h = register_hand_fixture(&substrate, "h1").await?;
    substrate.create_ticket(new_ticket("drk-1")?).await?;
    substrate.assign_to_hand(&ticket_id("drk-1")?, &h).await?;
    substrate
        .transition_to_in_review(
            &ticket_id("drk-1")?,
            InReviewMetadata {
                branch: "derrick/feature".to_owned(),
                pr_url: Some("https://example/pr/42".to_owned()),
                pr_number: Some(42),
                head_sha: "headsha".to_owned(),
            },
        )
        .await?;
    let events = substrate.ticket_events(&ticket_id("drk-1")?, 50).await?;
    let found = events.iter().any(|e| match &e.kind {
        EventKind::TicketTransitionedToInReview {
            branch,
            pr_url,
            pr_number,
            head_sha,
        } => {
            branch == "derrick/feature"
                && pr_url.as_deref() == Some("https://example/pr/42")
                && *pr_number == Some(42)
                && head_sha == "headsha"
        }
        _ => false,
    });
    assert!(found);
    Ok(())
}

#[tokio::test]
async fn stack_submit_idempotent_re_records_metadata_for_in_review_ticket(
) -> Result<(), SubstrateError> {
    // Reproduces the `derrick stack submit` idempotent path: a ticket
    // is already InReview without a PR URL; submit publishes the PR
    // and re-records `TicketTransitionedToInReview` with the fresh
    // metadata. Calling `transition_to_in_review` here would fail
    // (state must be InFlight), so submit emits the event directly
    // via `record_typed_event`. `most_recent_in_review_metadata` must
    // observe the newer payload.
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    let h = register_hand_fixture(&substrate, "h1").await?;
    substrate.create_ticket(new_ticket("drk-1")?).await?;
    substrate.assign_to_hand(&ticket_id("drk-1")?, &h).await?;
    substrate
        .transition_to_in_review(
            &ticket_id("drk-1")?,
            InReviewMetadata {
                branch: "derrick/feature".to_owned(),
                pr_url: None,
                pr_number: None,
                head_sha: "headsha".to_owned(),
            },
        )
        .await?;
    // Re-recording while the ticket is already InReview — the path
    // exercised by `stack_submit` after `open_pr` returns.
    substrate
        .record_typed_event(
            EventScope::Ticket(ticket_id("drk-1")?),
            EventKind::TicketTransitionedToInReview {
                branch: "derrick/feature".to_owned(),
                pr_url: Some("https://example/pr/7".to_owned()),
                pr_number: Some(7),
                head_sha: "headsha".to_owned(),
            },
        )
        .await?;
    let metadata = substrate
        .most_recent_in_review_metadata(&ticket_id("drk-1")?)
        .await?
        .expect("metadata present");
    assert_eq!(metadata.pr_url.as_deref(), Some("https://example/pr/7"));
    assert_eq!(metadata.pr_number, Some(7));
    let ticket = substrate
        .get_ticket(&ticket_id("drk-1")?)
        .await?
        .expect("ticket present");
    assert_eq!(ticket.state, TicketState::InReview);
    Ok(())
}

// ---------------------- event log integrity ----------------------

#[tokio::test]
async fn event_log_reconstructs_current_state_from_kinds() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    let h = register_hand_fixture(&substrate, "h1").await?;
    substrate.create_ticket(new_ticket("drk-1")?).await?;
    substrate.assign_to_hand(&ticket_id("drk-1")?, &h).await?;
    substrate
        .transition_to_in_review(
            &ticket_id("drk-1")?,
            InReviewMetadata {
                branch: "b".to_owned(),
                pr_url: None,
                pr_number: None,
                head_sha: "s".to_owned(),
            },
        )
        .await?;
    substrate
        .verify_ticket_merged(&ticket_id("drk-1")?, "s".to_owned(), "m".to_owned())
        .await?;
    let events = substrate.ticket_events(&ticket_id("drk-1")?, 50).await?;
    // ticket_events orders newest-first; find the first state-change.
    let last_state = events.iter().find_map(|e| match &e.kind {
        EventKind::TicketStateChanged { to, .. } => Some(*to),
        EventKind::TicketCreated { initial_state } => Some(*initial_state),
        _ => None,
    });
    let ticket = substrate
        .get_ticket(&ticket_id("drk-1")?)
        .await?
        .expect("ticket present");
    assert_eq!(last_state, Some(ticket.state));
    Ok(())
}

#[tokio::test]
async fn events_body_round_trips_through_serde_json() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    let kinds = vec![
        EventKind::Note {
            body: "hello".to_owned(),
        },
        EventKind::BatchCreated,
        EventKind::ForemanStopped,
        EventKind::TicketStateChanged {
            from: TicketState::Ready,
            to: TicketState::InFlight,
            reason: Some("dispatch".to_owned()),
        },
    ];
    for kind in &kinds {
        substrate
            .record_typed_event(EventScope::Site, kind.clone())
            .await?;
    }
    let read = substrate.tail_typed_events(None, 50).await?;
    for kind in &kinds {
        let serialised = serde_json::to_string(kind).unwrap();
        let matched = read
            .iter()
            .any(|e| serde_json::to_string(&e.kind).unwrap() == serialised);
        assert!(matched, "missing round-trip for {kind:?}");
    }
    Ok(())
}

// ---------------------- assign_to_hand / release_from_hand ----------------------

#[tokio::test]
async fn assign_to_hand_is_atomic_state_and_owner() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    let h = register_hand_fixture(&substrate, "h1").await?;
    substrate.create_ticket(new_ticket("drk-1")?).await?;
    let ticket = substrate.assign_to_hand(&ticket_id("drk-1")?, &h).await?;
    assert_eq!(ticket.state, TicketState::InFlight);
    assert_eq!(ticket.owner.as_ref(), Some(&h));

    let events = substrate.ticket_events(&ticket_id("drk-1")?, 50).await?;
    // events come newest-first; iter must contain TicketAssigned before
    // TicketStateChanged (newer first), since assigned is appended last.
    let mut iter = events.iter();
    let first = iter.next().expect("at least one event");
    let second = iter.next().expect("at least two events");
    assert!(matches!(first.kind, EventKind::TicketAssigned { .. }));
    assert!(matches!(second.kind, EventKind::TicketStateChanged { .. }));
    Ok(())
}

#[tokio::test]
async fn assign_to_hand_refuses_when_not_ready() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    let h = register_hand_fixture(&substrate, "h1").await?;
    substrate.create_ticket(new_ticket("drk-1")?).await?;
    substrate.assign_to_hand(&ticket_id("drk-1")?, &h).await?;
    let result = substrate.assign_to_hand(&ticket_id("drk-1")?, &h).await;
    assert!(matches!(result, Err(SubstrateError::Invalid { .. })));
    Ok(())
}

#[tokio::test]
async fn release_from_hand_is_atomic_state_and_owner() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    let h = register_hand_fixture(&substrate, "h1").await?;
    substrate.create_ticket(new_ticket("drk-1")?).await?;
    substrate.assign_to_hand(&ticket_id("drk-1")?, &h).await?;
    let ticket = substrate
        .release_from_hand(&ticket_id("drk-1")?, "abandoned".to_owned())
        .await?;
    assert_eq!(ticket.state, TicketState::Ready);
    assert!(ticket.owner.is_none());
    Ok(())
}

#[tokio::test]
async fn release_from_hand_refuses_on_terminal() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    substrate.create_ticket(new_ticket("drk-1")?).await?;
    substrate
        .mark_ticket_done_manually(
            &ticket_id("drk-1")?,
            ManualDoneAttestation {
                claimant: "a".to_owned(),
                note: "n".to_owned(),
            },
        )
        .await?;
    let result = substrate
        .release_from_hand(&ticket_id("drk-1")?, "x".to_owned())
        .await;
    assert!(matches!(result, Err(SubstrateError::Invalid { .. })));
    Ok(())
}

// ---------------------- reconcile_ticket_done_from_git ----------------------

#[tokio::test]
async fn reconcile_ticket_done_from_git_requires_prior_inreview_event() -> Result<(), SubstrateError>
{
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    substrate.create_ticket(new_ticket("drk-1")?).await?;
    let result = substrate
        .reconcile_ticket_done_from_git(&ticket_id("drk-1")?, "h".to_owned(), "m".to_owned())
        .await;
    match result {
        Err(SubstrateError::Invalid { message, .. }) => {
            assert!(message.contains("D33"));
        }
        other => panic!("expected Invalid with D33 message, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn reconcile_ticket_done_from_git_accepts_ready_with_history() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    let h = register_hand_fixture(&substrate, "h1").await?;
    substrate.create_ticket(new_ticket("drk-1")?).await?;
    move_to_in_review(&substrate, &ticket_id("drk-1")?, &h, "sha").await?;
    // Re-queue: release back to Ready.
    substrate
        .release_from_hand(&ticket_id("drk-1")?, "requeued".to_owned())
        .await?;

    let ticket = substrate
        .reconcile_ticket_done_from_git(&ticket_id("drk-1")?, "sha".to_owned(), "sha".to_owned())
        .await?;
    assert_eq!(ticket.state, TicketState::Done);
    assert_eq!(ticket.merge_sha.as_deref(), Some("sha"));
    Ok(())
}

// ---------------------- typed event reads ----------------------

#[tokio::test]
async fn tail_typed_events_returns_deserialised_kinds() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    substrate
        .record_typed_event(
            EventScope::Site,
            EventKind::Note {
                body: "ping".to_owned(),
            },
        )
        .await?;
    let events = substrate.tail_typed_events(None, 50).await?;
    assert!(events
        .iter()
        .any(|e| matches!(&e.kind, EventKind::Note { body } if body == "ping")));
    Ok(())
}

#[tokio::test]
async fn ticket_events_returns_history_newest_first() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    let h = register_hand_fixture(&substrate, "h1").await?;
    substrate.create_ticket(new_ticket("drk-1")?).await?;
    substrate.assign_to_hand(&ticket_id("drk-1")?, &h).await?;
    let events = substrate.ticket_events(&ticket_id("drk-1")?, 50).await?;
    let ids: Vec<i64> = events.iter().map(|e| e.id.0).collect();
    let mut sorted = ids.clone();
    sorted.sort_by(|a, b| b.cmp(a));
    assert_eq!(ids, sorted, "ticket_events must be newest-first");
    Ok(())
}

// ---------------------- migration tests ----------------------

/// Populate a temporary DB so its `user_version` is 1 and the schema matches
/// the T007 / migration 0001 shape. Useful for testing 0002 in place.
fn write_v1_db(db_path: &Path) -> Result<(), SubstrateError> {
    let connection = open_writer_connection(db_path)?;
    connection
        .execute_batch(MIGRATION_0001)
        .map_err(sql_error)?;
    // The 0001 migration sets user_version = 1.
    let v: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(sql_error)?;
    assert_eq!(v, 1);
    // Seed a site row (migrations don't do this for us).
    connection
        .execute(
            "INSERT INTO site (name, prefix, created_at) VALUES (?1, ?2, ?3)",
            params!["test", "tst", now_text()],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn seed_v1_ticket(
    db_path: &Path,
    id: &str,
    state: &str,
    batch: Option<&str>,
) -> Result<(), SubstrateError> {
    let connection = open_writer_connection(db_path)?;
    if let Some(b) = batch {
        connection
            .execute(
                "INSERT OR IGNORE INTO batches (name, created_at, closed_at) VALUES (?1, ?2, NULL)",
                params![b, now_text()],
            )
            .map_err(sql_error)?;
    }
    connection
        .execute(
            "INSERT INTO tickets (id, batch, ordinal, title, body, state, owner,
                                  created_at, updated_at)
             VALUES (?1, ?2, NULL, 'title', 'body', ?3, NULL, ?4, ?4)",
            params![id, batch, state, now_text()],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn site_for_v1() -> Site {
    // Open with default site; we will overwrite the site row after.
    Config::defaults().site().clone()
}

#[tokio::test]
async fn migration_0002_upgrades_v1_db_in_place() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let config = native_config_fixture(&tempdir);
    write_v1_db(&config.db_path)?;
    seed_v1_ticket(&config.db_path, "drk-1", "ready", None)?;
    seed_v1_ticket(&config.db_path, "drk-2", "blocked", None)?;

    // Re-write site row to match the default Config site so open() doesn't refuse.
    {
        let connection = open_writer_connection(&config.db_path)?;
        connection
            .execute("DELETE FROM site", [])
            .map_err(sql_error)?;
        let s = site_for_v1();
        connection
            .execute(
                "INSERT INTO site (name, prefix, created_at) VALUES (?1, ?2, ?3)",
                params![s.name(), s.prefix(), now_text()],
            )
            .map_err(sql_error)?;
    }

    let substrate = NativeSubstrate::open(config.clone(), site_for_v1()).await?;
    let t1 = substrate
        .get_ticket(&ticket_id("drk-1")?)
        .await?
        .expect("drk-1 preserved");
    assert_eq!(t1.state, TicketState::Ready);
    let t2 = substrate
        .get_ticket(&ticket_id("drk-2")?)
        .await?
        .expect("drk-2 preserved");
    assert_eq!(t2.state, TicketState::Blocked);
    // Legacy blocked ticket gets the human block_reason.
    match t2.block_reason {
        Some(BlockReason::Human { ref note }) => {
            assert!(note.contains("migrated"));
        }
        other => panic!("expected Human block reason, got {other:?}"),
    }

    let connection = open_writer_connection(&config.db_path)?;
    let v: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(sql_error)?;
    assert_eq!(v, 2);
    Ok(())
}

#[tokio::test]
async fn migration_0002_preserves_legacy_done_tickets() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let config = native_config_fixture(&tempdir);
    write_v1_db(&config.db_path)?;
    seed_v1_ticket(&config.db_path, "drk-1", "done", None)?;
    {
        let connection = open_writer_connection(&config.db_path)?;
        connection
            .execute("DELETE FROM site", [])
            .map_err(sql_error)?;
        let s = site_for_v1();
        connection
            .execute(
                "INSERT INTO site (name, prefix, created_at) VALUES (?1, ?2, ?3)",
                params![s.name(), s.prefix(), now_text()],
            )
            .map_err(sql_error)?;
    }
    let substrate = NativeSubstrate::open(config, site_for_v1()).await?;
    let t = substrate
        .get_ticket(&ticket_id("drk-1")?)
        .await?
        .expect("drk-1 preserved");
    assert_eq!(t.state, TicketState::Done);
    assert!(t.merge_sha.is_none());
    Ok(())
}

#[tokio::test]
async fn migration_0002_idempotent_on_v2_db() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let s1 = open_substrate(&tempdir).await?;
    s1.create_ticket(new_ticket("drk-1")?).await?;
    drop(s1);
    let s2 = open_substrate(&tempdir).await?;
    let t = s2
        .get_ticket(&ticket_id("drk-1")?)
        .await?
        .expect("ticket preserved");
    assert_eq!(t.state, TicketState::Ready);
    Ok(())
}

#[tokio::test]
async fn migration_refuses_v3_db() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let config = native_config_fixture(&tempdir);
    let s = NativeSubstrate::open(config.clone(), site_fixture()).await?;
    drop(s);
    let connection = open_writer_connection(&config.db_path)?;
    connection
        .pragma_update(None, "user_version", 3u32)
        .map_err(sql_error)?;
    drop(connection);
    let result = NativeSubstrate::open(config, site_fixture()).await;
    assert!(matches!(result, Err(SubstrateError::Invalid { .. })));
    Ok(())
}

#[tokio::test]
async fn migration_0002_rolls_back_on_mid_rebuild_crash() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let config = native_config_fixture(&tempdir);
    write_v1_db(&config.db_path)?;
    seed_v1_ticket(&config.db_path, "drk-1", "ready", None)?;
    {
        let connection = open_writer_connection(&config.db_path)?;
        connection
            .execute("DELETE FROM site", [])
            .map_err(sql_error)?;
        let s = site_for_v1();
        connection
            .execute(
                "INSERT INTO site (name, prefix, created_at) VALUES (?1, ?2, ?3)",
                params![s.name(), s.prefix(), now_text()],
            )
            .map_err(sql_error)?;
        // Pre-create a `tickets_new` table so the CREATE TABLE in migration 0002
        // fails partway through the rebuild.
        connection
            .execute("CREATE TABLE tickets_new (boom TEXT)", [])
            .map_err(sql_error)?;
    }
    let result = NativeSubstrate::open(config.clone(), site_for_v1()).await;
    assert!(result.is_err(), "migration should fail");

    // DB still openable as v1: drop the booby-trap and re-open.
    {
        let connection = open_writer_connection(&config.db_path)?;
        connection
            .execute("DROP TABLE tickets_new", [])
            .map_err(sql_error)?;
        let v: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(sql_error)?;
        assert_eq!(v, 1, "user_version must remain at 1 after failed migration");
        // Data must be intact.
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM tickets WHERE id = 'drk-1'", [], |r| {
                r.get(0)
            })
            .map_err(sql_error)?;
        assert_eq!(count, 1);
    }

    // After fixing, migration can succeed.
    let substrate = NativeSubstrate::open(config, site_for_v1()).await?;
    let t = substrate
        .get_ticket(&ticket_id("drk-1")?)
        .await?
        .expect("preserved");
    assert_eq!(t.state, TicketState::Ready);
    Ok(())
}

// ---------------------- D32 blocking semantics ----------------------

#[tokio::test]
async fn verify_ticket_unmerged_transitions_in_review_to_blocked() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    let h = register_hand_fixture(&substrate, "h1").await?;
    substrate.create_ticket(new_ticket("drk-1")?).await?;
    move_to_in_review(&substrate, &ticket_id("drk-1")?, &h, "s").await?;
    let ticket = substrate
        .verify_ticket_unmerged(
            &ticket_id("drk-1")?,
            "feature".to_owned(),
            Some("https://example/pr/1".to_owned()),
        )
        .await?;
    assert_eq!(ticket.state, TicketState::Blocked);
    match ticket.block_reason {
        Some(BlockReason::PrClosedUnmerged { ref branch, .. }) => {
            assert_eq!(branch, "feature");
        }
        other => panic!("expected PrClosedUnmerged, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn block_ticket_sets_block_reason_column() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    substrate.create_ticket(new_ticket("drk-1")?).await?;
    let ticket = substrate
        .block_ticket(
            &ticket_id("drk-1")?,
            BlockReason::Human {
                note: "user wants it blocked".to_owned(),
            },
        )
        .await?;
    assert_eq!(ticket.state, TicketState::Blocked);
    match ticket.block_reason {
        Some(BlockReason::Human { ref note }) => assert_eq!(note, "user wants it blocked"),
        other => panic!("expected Human, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn unblock_ticket_refuses_for_non_dependency_reason() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    substrate.create_ticket(new_ticket("drk-1")?).await?;
    substrate
        .block_ticket(
            &ticket_id("drk-1")?,
            BlockReason::Human {
                note: "n".to_owned(),
            },
        )
        .await?;
    let result = substrate.unblock_ticket(&ticket_id("drk-1")?).await;
    assert!(matches!(result, Err(SubstrateError::Invalid { .. })));
    Ok(())
}

#[tokio::test]
async fn human_reopen_blocked_works_for_pr_closed_unmerged() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    let h = register_hand_fixture(&substrate, "h1").await?;
    substrate.create_ticket(new_ticket("drk-1")?).await?;
    move_to_in_review(&substrate, &ticket_id("drk-1")?, &h, "s").await?;
    substrate
        .verify_ticket_unmerged(&ticket_id("drk-1")?, "b".to_owned(), None)
        .await?;
    let ticket = substrate
        .human_reopen_blocked(&ticket_id("drk-1")?, "retry".to_owned())
        .await?;
    assert_eq!(ticket.state, TicketState::Ready);
    assert!(ticket.block_reason.is_none());
    Ok(())
}

#[tokio::test]
async fn human_reopen_blocked_refuses_when_not_blocked() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    substrate.create_ticket(new_ticket("drk-1")?).await?;
    let result = substrate
        .human_reopen_blocked(&ticket_id("drk-1")?, "n".to_owned())
        .await;
    assert!(matches!(result, Err(SubstrateError::Invalid { .. })));
    Ok(())
}

#[tokio::test]
async fn block_reason_check_constraint_enforced() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    substrate.create_ticket(new_ticket("drk-1")?).await?;
    // Direct probe at the SQLite layer: try to set state = blocked without
    // a block_reason; the CHECK should refuse.
    let connection = open_writer_connection(&native_config_fixture(&tempdir).db_path)?;
    let result = connection.execute(
        "UPDATE tickets SET state = 'blocked' WHERE id = 'drk-1'",
        [],
    );
    assert!(result.is_err(), "CHECK constraint must reject");
    Ok(())
}

// ---------------------- foreman row writes ----------------------

#[tokio::test]
async fn record_foreman_attached_writes_row_and_event() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    substrate.record_foreman_attached(42).await?;
    let status = substrate.foreman_status().await?;
    assert_eq!(status.mode, ForemanMode::Attached);
    assert_eq!(status.pid, Some(42));
    let events = substrate.tail_typed_events(None, 50).await?;
    assert!(events.iter().any(|e| matches!(
        &e.kind,
        EventKind::ForemanStarted {
            mode: ForemanMode::Attached,
            pid: 42
        }
    )));
    Ok(())
}

#[tokio::test]
async fn record_foreman_detached_writes_row_and_event() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    substrate.record_foreman_detached(99).await?;
    let status = substrate.foreman_status().await?;
    assert_eq!(status.mode, ForemanMode::Detached);
    assert_eq!(status.pid, Some(99));
    let events = substrate.tail_typed_events(None, 50).await?;
    assert!(events.iter().any(|e| matches!(
        &e.kind,
        EventKind::ForemanStarted {
            mode: ForemanMode::Detached,
            pid: 99
        }
    )));
    Ok(())
}

#[tokio::test]
async fn record_foreman_stopped_clears_mode() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    substrate.record_foreman_attached(1).await?;
    substrate.record_foreman_stopped().await?;
    let status = substrate.foreman_status().await?;
    assert_eq!(status.mode, ForemanMode::Stopped);
    assert!(status.pid.is_none());
    Ok(())
}

// ---------------------- decode_event_body legacy compat ----------------------

#[test]
fn decode_event_body_note_legacy() -> Result<(), SubstrateError> {
    let kind = decode_event_body("note", "hello world")?;
    assert!(matches!(kind, EventKind::Note { body } if body == "hello world"));
    Ok(())
}

#[test]
fn decode_event_body_ticket_state_changed_legacy() -> Result<(), SubstrateError> {
    let body = r#"{"from":"ready","to":"in_flight","reason":"dispatch"}"#;
    let kind = decode_event_body("ticket_state_changed", body)?;
    match kind {
        EventKind::TicketStateChanged { from, to, reason } => {
            assert_eq!(from, TicketState::Ready);
            assert_eq!(to, TicketState::InFlight);
            assert_eq!(reason.as_deref(), Some("dispatch"));
        }
        other => panic!("expected TicketStateChanged, got {other:?}"),
    }
    Ok(())
}

#[test]
fn decode_event_body_ticket_created_legacy() -> Result<(), SubstrateError> {
    let kind = decode_event_body("ticket_created", "")?;
    match kind {
        EventKind::TicketCreated { initial_state } => {
            assert_eq!(initial_state, TicketState::Ready);
        }
        other => panic!("expected TicketCreated, got {other:?}"),
    }
    Ok(())
}

#[test]
fn decode_event_body_ticket_assigned_legacy() -> Result<(), SubstrateError> {
    let kind = decode_event_body("ticket_assigned", "h1")?;
    assert!(matches!(kind, EventKind::TicketAssigned { hand } if hand.as_str() == "h1"));
    Ok(())
}

#[test]
fn decode_event_body_ticket_unassigned_legacy() -> Result<(), SubstrateError> {
    let kind = decode_event_body("ticket_unassigned", "rebalance")?;
    assert!(matches!(kind, EventKind::TicketUnassigned { reason } if reason == "rebalance"));
    Ok(())
}

#[test]
fn decode_event_body_batch_created_legacy() -> Result<(), SubstrateError> {
    let kind = decode_event_body("batch_created", "")?;
    assert!(matches!(kind, EventKind::BatchCreated));
    Ok(())
}

#[test]
fn decode_event_body_batch_closed_legacy() -> Result<(), SubstrateError> {
    let kind = decode_event_body("batch_closed", "")?;
    assert!(
        matches!(kind, EventKind::BatchClosed { open_ticket_ids } if open_ticket_ids.is_empty())
    );
    Ok(())
}

#[test]
fn decode_event_body_foreman_started_legacy() -> Result<(), SubstrateError> {
    let kind = decode_event_body("foreman_started", "1234")?;
    assert!(matches!(
        kind,
        EventKind::ForemanStarted {
            mode: ForemanMode::Detached,
            pid: 1234
        }
    ));
    Ok(())
}

#[test]
fn decode_event_body_foreman_stopped_legacy() -> Result<(), SubstrateError> {
    let kind = decode_event_body("foreman_stopped", "")?;
    assert!(matches!(kind, EventKind::ForemanStopped));
    Ok(())
}

#[test]
fn decode_event_body_hand_registered_legacy() -> Result<(), SubstrateError> {
    let kind = decode_event_body("hand_registered", "")?;
    assert!(matches!(kind, EventKind::HandRegistered));
    Ok(())
}

#[test]
fn decode_event_body_hand_heartbeat_legacy() -> Result<(), SubstrateError> {
    let kind = decode_event_body("hand_heartbeat", "")?;
    assert!(matches!(kind, EventKind::HandHeartbeat));
    Ok(())
}

#[test]
fn decode_event_body_unknown_legacy_kind_errors() {
    let result = decode_event_body("bogus_kind", "");
    assert!(matches!(result, Err(SubstrateError::Invalid { .. })));
}

#[test]
fn decode_event_body_new_format_round_trips() -> Result<(), SubstrateError> {
    let original = EventKind::Note {
        body: "hi".to_owned(),
    };
    let body = serde_json::to_string(&original).unwrap();
    let decoded = decode_event_body("note", &body)?;
    assert!(matches!(decoded, EventKind::Note { body } if body == "hi"));
    Ok(())
}

// ---------------------- existing baseline tests (T007 regression) ----------------------

#[tokio::test]
async fn site_initialises_from_config() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    assert_eq!(substrate.site().await?, site_fixture());
    Ok(())
}

#[tokio::test]
async fn create_ticket_persists_and_missing_returns_none() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    let ticket = substrate.create_ticket(new_ticket("drk-1")?).await?;
    assert_eq!(ticket.id, ticket_id("drk-1")?);
    assert!(substrate.get_ticket(&ticket_id("drk-1")?).await?.is_some());
    assert!(substrate.get_ticket(&ticket_id("drk-2")?).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn create_ticket_into_closed_batch_returns_conflict() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    substrate.create_batch(batch_name("batch-1")?).await?;
    substrate.close_batch(&batch_name("batch-1")?).await?;
    let result = substrate
        .create_ticket(new_batched_ticket("drk-1", "batch-1", Some(1))?)
        .await;
    assert!(matches!(result, Err(SubstrateError::Conflict { .. })));
    Ok(())
}

#[tokio::test]
async fn create_ticket_duplicate_id_returns_conflict() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    substrate.create_ticket(new_ticket("drk-1")?).await?;
    let result = substrate.create_ticket(new_ticket("drk-1")?).await;
    assert!(matches!(result, Err(SubstrateError::Conflict { .. })));
    Ok(())
}

#[tokio::test]
async fn verify_ticket_merged_auto_closes_batch() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    let h = register_hand_fixture(&substrate, "h1").await?;
    substrate.create_batch(batch_name("batch-1")?).await?;
    substrate
        .create_ticket(new_batched_ticket("drk-1", "batch-1", Some(1))?)
        .await?;
    move_to_in_review(&substrate, &ticket_id("drk-1")?, &h, "s").await?;
    substrate
        .verify_ticket_merged(&ticket_id("drk-1")?, "s".to_owned(), "m".to_owned())
        .await?;
    let b = substrate.get_batch(&batch_name("batch-1")?).await?;
    assert!(b.and_then(|b| b.closed_at).is_some());
    Ok(())
}

#[tokio::test]
async fn list_tickets_respects_filters() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    substrate.create_ticket(new_ticket("drk-1")?).await?;
    substrate.create_ticket(new_ticket("drk-2")?).await?;
    let tickets = substrate
        .list_tickets(TicketFilter {
            limit: NonZeroUsize::new(5),
            ..TicketFilter::default()
        })
        .await?;
    assert_eq!(tickets.len(), 2);
    Ok(())
}

#[tokio::test]
async fn link_and_unlink_round_trip() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    substrate.create_ticket(new_ticket("drk-1")?).await?;
    substrate.create_ticket(new_ticket("drk-2")?).await?;
    substrate
        .link(&ticket_id("drk-1")?, &ticket_id("drk-2")?, LinkKind::Blocks)
        .await?;
    assert_eq!(
        substrate.outgoing_links(&ticket_id("drk-1")?).await?.len(),
        1
    );
    substrate
        .unlink(&ticket_id("drk-1")?, &ticket_id("drk-2")?, LinkKind::Blocks)
        .await?;
    assert!(substrate
        .outgoing_links(&ticket_id("drk-1")?)
        .await?
        .is_empty());
    Ok(())
}

#[tokio::test]
async fn batch_lifecycle_and_ordering() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    substrate.create_batch(batch_name("batch-1")?).await?;
    substrate
        .create_ticket(new_batched_ticket("drk-2", "batch-1", None)?)
        .await?;
    substrate
        .create_ticket(new_batched_ticket("drk-1", "batch-1", Some(1))?)
        .await?;
    let tickets = substrate.tickets_in_batch(&batch_name("batch-1")?).await?;
    assert_eq!(tickets[0].id, ticket_id("drk-1")?);
    assert_eq!(tickets[1].id, ticket_id("drk-2")?);
    Ok(())
}

#[tokio::test]
async fn hands_and_heartbeat_round_trip() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    substrate
        .register_hand(Hand {
            id: hand_id("claude-1")?,
            kind: HandKind::Claude,
            last_seen: None,
        })
        .await?;
    substrate.heartbeat(&hand_id("claude-1")?).await?;
    let hands = substrate.list_hands().await?;
    assert_eq!(hands.len(), 1);
    assert!(hands[0].last_seen.is_some());
    Ok(())
}

#[tokio::test]
async fn worktree_lifecycle_round_trip() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    let path = substrate
        .reserve_worktree("run-1", "derrick/feature-run-1")
        .await?;
    assert_eq!(path, tempdir.path().join("worktrees").join("run-1"));
    substrate.finalize_worktree("run-1").await?;
    substrate.close_worktree("run-1").await?;
    assert!(substrate.list_worktrees(false).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn pragmas_set_on_every_connection() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    assert!(substrate.writer_foreign_keys_enabled_for_test().await?);
    assert!(substrate.reader_foreign_keys_enabled_for_test().await?);
    assert!(matches!(
        substrate.reader_insert_fails_for_test().await,
        Err(SubstrateError::Backend(_))
    ));
    Ok(())
}

#[tokio::test]
async fn concurrent_terminal_writes_emit_one_batch_closed_event() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = Arc::new(open_substrate(&tempdir).await?);
    substrate.create_batch(batch_name("batch-1")?).await?;
    substrate
        .create_ticket(new_batched_ticket("drk-1", "batch-1", Some(1))?)
        .await?;
    substrate
        .create_ticket(new_batched_ticket("drk-2", "batch-1", Some(2))?)
        .await?;

    let first = Arc::clone(&substrate);
    let second = Arc::clone(&substrate);
    let first_task = tokio::spawn(async move {
        first
            .mark_ticket_done_manually(
                &ticket_id("drk-1")?,
                ManualDoneAttestation {
                    claimant: "a".to_owned(),
                    note: "".to_owned(),
                },
            )
            .await
    });
    let second_task = tokio::spawn(async move {
        second
            .mark_ticket_done_manually(
                &ticket_id("drk-2")?,
                ManualDoneAttestation {
                    claimant: "a".to_owned(),
                    note: "".to_owned(),
                },
            )
            .await
    });
    first_task.await.map_err(join_error)??;
    second_task.await.map_err(join_error)??;
    let events = substrate.tail_typed_events(None, 200).await?;
    let batch_closed = events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::BatchClosed { .. }))
        .count();
    assert_eq!(batch_closed, 1);
    Ok(())
}

#[tokio::test]
async fn record_typed_event_and_tail_round_trip() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    let id = substrate
        .record_typed_event(
            EventScope::Site,
            EventKind::Note {
                body: "hello".to_owned(),
            },
        )
        .await?;
    let events = substrate.tail_typed_events(None, 10).await?;
    assert!(events.iter().any(|e| e.id == id));
    Ok(())
}

#[tokio::test]
async fn legacy_record_event_still_works() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;
    #[allow(deprecated)]
    let event = substrate
        .record_event(NewEvent {
            kind: "note".to_owned(),
            ticket: None,
            body: "legacy".to_owned(),
        })
        .await?;
    let _ = event.id;
    let _: chrono::DateTime<chrono::Utc> = Utc::now();
    let _ = Uuid::new_v4();
    Ok(())
}

// ---------------------- delete_ticket ----------------------

#[tokio::test]
async fn delete_ticket_removes_ticket_and_is_idempotent() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;

    substrate.create_ticket(new_ticket("drk-1")?).await?;
    assert!(
        substrate.get_ticket(&ticket_id("drk-1")?).await?.is_some(),
        "ticket should exist before delete"
    );

    // First delete removes the ticket.
    substrate.delete_ticket(&ticket_id("drk-1")?).await?;
    assert!(
        substrate.get_ticket(&ticket_id("drk-1")?).await?.is_none(),
        "ticket should be gone after delete"
    );

    // Second delete on missing ticket is a no-op (idempotent).
    substrate.delete_ticket(&ticket_id("drk-1")?).await?;

    Ok(())
}

#[tokio::test]
async fn delete_ticket_removes_associated_events() -> Result<(), SubstrateError> {
    let tempdir = tempfile::tempdir().map_err(io_error)?;
    let substrate = open_substrate(&tempdir).await?;

    substrate.create_ticket(new_ticket("drk-1")?).await?;

    // The create_ticket call should have written a TicketCreated typed event.
    let events_before = substrate.ticket_events(&ticket_id("drk-1")?, 100).await?;
    assert!(!events_before.is_empty(), "expected events before delete");

    substrate.delete_ticket(&ticket_id("drk-1")?).await?;

    // After delete the ticket row is gone; ticket_events for a missing ticket
    // should return an empty list (not an error).
    let events_after = substrate.ticket_events(&ticket_id("drk-1")?, 100).await?;
    assert!(events_after.is_empty(), "expected no events after delete");

    Ok(())
}
