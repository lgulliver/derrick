//! Integration test: `DataModel::refresh` counts match a seeded substrate.

use derrick_substrate::{NewTicket, TicketId, TicketStore};
use derrick_substrate_native::{NativeConfig, NativeSubstrate};
use derrick_tui::{DataModel, StackLoadResult};
use tempfile::TempDir;

fn site() -> derrick_config::Site {
    derrick_config::Config::defaults().site().clone()
}

fn cfg(tmp: &TempDir) -> NativeConfig {
    NativeConfig {
        db_path: tmp.path().join("derrick.db"),
        worktree_root: tmp.path().join("worktrees"),
    }
}

fn ticket_id(s: &str) -> TicketId {
    match TicketId::new(s) {
        Ok(id) => id,
        Err(e) => unreachable!("invalid fixture id {s}: {e}"),
    }
}

async fn seed_ticket(substrate: &NativeSubstrate, id: &str) {
    let new = match NewTicket::new(
        ticket_id(id),
        None,
        None,
        "title",
        "body content for ticket",
        Vec::new(),
    ) {
        Ok(n) => n,
        Err(e) => unreachable!("invalid new ticket: {e}"),
    };
    if let Err(e) = substrate.create_ticket(new).await {
        unreachable!("create_ticket: {e}");
    }
}

#[tokio::test]
async fn data_model_counts_reflect_substrate() {
    let tmp = match TempDir::new() {
        Ok(t) => t,
        Err(e) => unreachable!("tempdir: {e}"),
    };
    let substrate = match NativeSubstrate::open(cfg(&tmp), site()).await {
        Ok(s) => s,
        Err(e) => unreachable!("open: {e}"),
    };

    // Seed 3 tickets — all start in Ready state.
    for id in ["mp-1", "mp-2", "mp-3"] {
        seed_ticket(&substrate, id).await;
    }

    let data = match DataModel::refresh(&substrate, &[], StackLoadResult::Loaded, &[], None).await {
        Ok(d) => d,
        Err(e) => unreachable!("refresh: {e}"),
    };
    assert_eq!(data.overview.tickets_total, 3);
    assert_eq!(data.overview.tickets_ready, 3);
    assert_eq!(data.overview.tickets_inflight, 0);
    assert_eq!(data.overview.tickets_done, 0);
    assert_eq!(data.tickets.len(), 3);
}
