//! `derrick undo` — revert the last hand's git commits.
use std::path::Path;
use std::process::Stdio;

use derrick_substrate::{EventKind, Substrate};
use derrick_substrate_native::NativeSubstrate;
use owo_colors::OwoColorize;
use tokio::process::Command;

use crate::commands::UndoArgs;
use crate::exit_code::CliExitCode;
use crate::{CliError, current_repo_root, message, native_paths, read_config};

/// Executes the `derrick undo` subcommand (reverts the last completed run).
pub(crate) async fn execute(args: UndoArgs) -> Result<CliExitCode, CliError> {
    let repo_root = current_repo_root()?;
    let config = read_config(&repo_root)?;
    let native_config = native_paths(&repo_root, &config);

    if !native_config.db_path.exists() {
        return Err(message(
            "no derrick database found — run `derrick init` first",
        ));
    }

    let substrate = NativeSubstrate::open(native_config, config.site().clone()).await?;
    let events = substrate.tail_typed_events(None, 100).await?;
    substrate.close().await?;

    // Find the most recent InReview transition — it carries branch + head_sha.
    let (branch, head_sha) = events
        .iter()
        .find_map(|e| {
            if let EventKind::TicketTransitionedToInReview {
                branch, head_sha, ..
            } = &e.kind
            {
                Some((branch.clone(), head_sha.clone()))
            } else {
                None
            }
        })
        .ok_or_else(|| message("no completed hand found in the event log"))?;

    // Find commits on the feature branch that are not on the current HEAD.
    let merge_base = git_output(&repo_root, &["merge-base", "HEAD", &head_sha])
        .await
        .unwrap_or_else(|_| head_sha.clone());

    let log = git_output(
        &repo_root,
        &["log", "--oneline", &format!("{}..{}", merge_base, head_sha)],
    )
    .await?;

    let commits: Vec<String> = log
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();

    if commits.is_empty() {
        eprintln!(
            "  {}  No commits to revert for branch {branch}.",
            "·".yellow()
        );
        return Ok(CliExitCode::Success);
    }

    eprintln!(
        "  {}  Last hand: branch {}",
        "·".cyan(),
        branch.bright_black()
    );
    eprintln!("  {}  Commits to revert:", "·".cyan());
    for c in &commits {
        eprintln!("       {c}");
    }

    if args.dry_run {
        return Ok(CliExitCode::Success);
    }

    if !args.yes {
        use std::io::Write as _;
        eprint!("  Revert {} commit(s)? [y/N] ", commits.len());
        std::io::stderr().flush().ok();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).ok();
        if !answer.trim().eq_ignore_ascii_case("y") && !answer.trim().eq_ignore_ascii_case("yes") {
            eprintln!("  Cancelled.");
            return Ok(CliExitCode::Success);
        }
    }

    // Collect SHAs (log is newest-first) and revert in that order so the
    // working tree ends up clean after each no-commit revert.
    let shas: Vec<String> = git_output(
        &repo_root,
        &[
            "log",
            "--format=%H",
            &format!("{}..{}", merge_base, head_sha),
        ],
    )
    .await?
    .lines()
    .filter(|l| !l.is_empty())
    .map(str::to_owned)
    .collect();

    for sha in &shas {
        let status = Command::new("git")
            .args(["revert", "--no-commit", sha])
            .current_dir(&repo_root)
            .status()
            .await
            .map_err(|source| CliError::Io {
                path: repo_root.clone(),
                source,
            })?;
        if !status.success() {
            return Err(message(format!("git revert --no-commit {sha} failed")));
        }
    }

    let msg = format!("revert: undo last hand ({branch})");
    let status = Command::new("git")
        .args(["commit", "-m", &msg])
        .current_dir(&repo_root)
        .status()
        .await
        .map_err(|source| CliError::Io {
            path: repo_root.clone(),
            source,
        })?;
    if !status.success() {
        return Err(message("git commit failed — working tree may need cleanup"));
    }

    eprintln!(
        "  {}  Reverted {} commit(s) from {branch}.",
        "✓".green(),
        shas.len()
    );
    Ok(CliExitCode::Success)
}

async fn git_output(repo_root: &Path, args: &[&str]) -> Result<String, CliError> {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|source| CliError::Io {
            path: repo_root.to_path_buf(),
            source,
        })?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    } else {
        Err(message(format!(
            "git {} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}
