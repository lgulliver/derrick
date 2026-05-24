//! CLI integration tests for the T012 `ticket` and `foreman` subcommands.

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use assert_cmd::Command;
use derrick_config::{Config, Site};
use derrick_substrate::{
    BatchName, BlockReason, LinkKind, NewTicket, Substrate, TicketId, TicketState,
};
use derrick_substrate_native::{NativeConfig, NativeSubstrate};
use tempfile::TempDir;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn derrick() -> TestResult<Command> {
    Ok(Command::cargo_bin("derrick")?)
}

fn make_repo(mode: &str) -> TestResult<TempDir> {
    let dir = tempfile::tempdir()?;
    fs::create_dir(dir.path().join(".git"))?;
    derrick()?
        .current_dir(dir.path())
        .env("DERRICK_SKIP_PREREQS", "1")
        .args([
            "init",
            "--greenfield",
            "--site",
            "test",
            "--prefix",
            "tst",
            "--mode",
            mode,
            "--yes",
        ])
        .assert()
        .success();
    Ok(dir)
}

fn config_for(dir: &Path) -> TestResult<Config> {
    Ok(Config::load_from_path(&dir.join("derrick.yaml"))?)
}

fn native_paths(dir: &Path, config: &Config) -> NativeConfig {
    NativeConfig {
        db_path: dir.join(config.state().dir()).join("derrick.db"),
        worktree_root: dir.join(config.state().worktree_root()),
    }
}

async fn open_substrate(dir: &Path) -> TestResult<NativeSubstrate> {
    let config = config_for(dir)?;
    let native_config = native_paths(dir, &config);
    let site: Site = config.site().clone();
    Ok(NativeSubstrate::open(native_config, site).await?)
}

async fn seed_ticket(substrate: &NativeSubstrate, id: &str) -> TestResult<TicketId> {
    let ticket_id = TicketId::new(id)?;
    let batch_name = BatchName::new("batch-1")?;
    if substrate.get_batch(&batch_name).await?.is_none() {
        substrate.create_batch(batch_name.clone()).await?;
    }
    substrate
        .create_ticket(NewTicket::new(
            ticket_id.clone(),
            Some(batch_name),
            None,
            format!("title for {id}"),
            "",
            Vec::new(),
        )?)
        .await?;
    Ok(ticket_id)
}

// --- Tests ---------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn cli_ticket_done_refuses_in_crew_mode() -> TestResult {
    let dir = make_repo("crew")?;
    let substrate = open_substrate(dir.path()).await?;
    seed_ticket(&substrate, "tst-1").await?;
    drop(substrate); // release SQLite handle before invoking the CLI

    let output = derrick()?
        .current_dir(dir.path())
        .args(["ticket", "done", "tst-1", "--note", "n"])
        .assert()
        .failure()
        .get_output()
        .clone();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("\u{a7}8.6") || stderr.contains("D31"),
        "expected D31 pointer in stderr; got: {stderr}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_ticket_done_succeeds_in_solo_mode() -> TestResult {
    let dir = make_repo("solo")?;
    let substrate = open_substrate(dir.path()).await?;
    let ticket_id = seed_ticket(&substrate, "tst-1").await?;
    drop(substrate);

    derrick()?
        .current_dir(dir.path())
        .args(["ticket", "done", "tst-1", "--note", "manual"])
        .assert()
        .success();

    let substrate = open_substrate(dir.path()).await?;
    let ticket = substrate
        .get_ticket(&ticket_id)
        .await?
        .ok_or("ticket missing")?;
    assert_eq!(ticket.state, TicketState::Done);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_ticket_block_writes_link_and_blocks_when_predecessor_open() -> TestResult {
    let dir = make_repo("solo")?;
    let substrate = open_substrate(dir.path()).await?;
    let a = seed_ticket(&substrate, "tst-1").await?;
    let b = seed_ticket(&substrate, "tst-2").await?;
    drop(substrate);

    derrick()?
        .current_dir(dir.path())
        .args(["ticket", "block", "tst-2", "--on", "tst-1"])
        .assert()
        .success();

    let substrate = open_substrate(dir.path()).await?;
    let ticket_b = substrate.get_ticket(&b).await?.ok_or("b missing")?;
    assert_eq!(ticket_b.state, TicketState::Blocked);
    match &ticket_b.block_reason {
        Some(BlockReason::Dependency { predecessor }) => assert_eq!(predecessor, &a),
        other => panic!("expected dependency reason, got {other:?}"),
    }
    // Link present.
    let outgoing = substrate.outgoing_links(&b).await?;
    assert!(outgoing
        .iter()
        .any(|link| link.to == a && link.kind == LinkKind::Blocks));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_ticket_block_writes_link_only_when_predecessor_terminal() -> TestResult {
    let dir = make_repo("solo")?;
    let substrate = open_substrate(dir.path()).await?;
    let a = seed_ticket(&substrate, "tst-1").await?;
    let b = seed_ticket(&substrate, "tst-2").await?;
    // Mark a Done via the solo path so its state is terminal.
    substrate
        .mark_ticket_done_manually(
            &a,
            derrick_substrate::ManualDoneAttestation {
                claimant: "tester".to_owned(),
                note: "pre-terminal".to_owned(),
            },
        )
        .await?;
    drop(substrate);

    derrick()?
        .current_dir(dir.path())
        .args(["ticket", "block", "tst-2", "--on", "tst-1"])
        .assert()
        .success();

    let substrate = open_substrate(dir.path()).await?;
    let ticket_b = substrate.get_ticket(&b).await?.ok_or("b missing")?;
    assert_eq!(
        ticket_b.state,
        TicketState::Ready,
        "should not have changed state"
    );
    let outgoing = substrate.outgoing_links(&b).await?;
    assert!(outgoing
        .iter()
        .any(|link| link.to == a && link.kind == LinkKind::Blocks));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_ticket_reopen_transitions_pr_closed_unmerged_to_ready() -> TestResult {
    let dir = make_repo("solo")?;
    let substrate = open_substrate(dir.path()).await?;
    let id = seed_ticket(&substrate, "tst-1").await?;
    substrate
        .block_ticket(
            &id,
            BlockReason::PrClosedUnmerged {
                branch: "feature/x".to_owned(),
                pr_url: Some("https://example.test/pr/1".to_owned()),
            },
        )
        .await?;
    drop(substrate);

    derrick()?
        .current_dir(dir.path())
        .args(["ticket", "reopen", "tst-1", "--note", "retrying"])
        .assert()
        .success();

    let substrate = open_substrate(dir.path()).await?;
    let ticket = substrate.get_ticket(&id).await?.ok_or("missing")?;
    assert_eq!(ticket.state, TicketState::Ready);
    assert!(ticket.block_reason.is_none());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_foreman_tick_runs_once_and_exits() -> TestResult {
    let dir = make_repo("crew")?;
    // Seed nothing — verifier and dispatch passes should be no-ops.
    derrick()?
        .current_dir(dir.path())
        .args(["foreman", "tick"])
        .assert()
        .success();
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_foreman_start_detached_writes_pid_and_exits() -> TestResult {
    let dir = make_repo("crew")?;
    let pid_path: PathBuf = dir.path().join(".derrick/foreman.pid");

    derrick()?
        .current_dir(dir.path())
        .args(["foreman", "start", "--detached"])
        .timeout(Duration::from_secs(60))
        .assert()
        .success();

    // Wait briefly for the pid file to appear (generous for coverage builds).
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline && !pid_path.exists() {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(pid_path.exists(), "pid file should exist at {pid_path:?}");

    let pid: i32 = fs::read_to_string(&pid_path)?.trim().parse()?;
    assert!(pid > 0);
    // Kernel liveness check.
    assert_eq!(unsafe { libc::kill(pid, 0) }, 0, "child should be alive");

    // Clean up via stop.
    derrick()?
        .current_dir(dir.path())
        .args(["foreman", "stop"])
        .timeout(Duration::from_secs(60))
        .assert()
        .success();
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_foreman_stop_signals_and_cleans_pid() -> TestResult {
    let dir = make_repo("crew")?;
    let pid_path: PathBuf = dir.path().join(".derrick/foreman.pid");

    derrick()?
        .current_dir(dir.path())
        .args(["foreman", "start", "--detached"])
        .timeout(Duration::from_secs(60))
        .assert()
        .success();

    // Wait for pid file (generous for coverage builds).
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline && !pid_path.exists() {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(pid_path.exists());
    let pid: i32 = fs::read_to_string(&pid_path)?.trim().parse()?;

    derrick()?
        .current_dir(dir.path())
        .args(["foreman", "stop"])
        .timeout(Duration::from_secs(60))
        .assert()
        .success();

    assert!(!pid_path.exists(), "pid file should be removed");

    // Process should be gone within a moment.
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline && unsafe { libc::kill(pid, 0) } == 0 {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "child should be gone");
    Ok(())
}
