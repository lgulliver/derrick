//! `derrick stack ...` subcommands. See DESIGN.md §8.5 and T014.

use std::path::Path;
use std::sync::Arc;

use derrick_config::{Config, StackBackendKind, SubstrateBackendKind};
use derrick_stack::{
    NativeStackBackend, NoneStackBackend, OpenPrParams, RestackOutcome, RestackParams, StackBackend,
};
use derrick_substrate::{
    BatchName, BlockReason, EventKind, EventScope, Substrate, TicketFilter, TicketState,
};
use derrick_substrate_native::NativeSubstrate;

use crate::commands::{StackArgs, StackCommand, StackRestackArgs, StackSubmitArgs};
use crate::exit_code::CliExitCode;
use crate::{current_repo_root, message, native_paths, read_config};

pub(crate) async fn execute(args: StackArgs) -> Result<CliExitCode, crate::CliError> {
    let repo_root = current_repo_root()?;
    let config = read_config(&repo_root)?;
    if config.tools().substrate().backend() != SubstrateBackendKind::Native {
        return Err(message(
            "derrick stack requires tools.substrate.backend: native",
        ));
    }
    let substrate =
        NativeSubstrate::open(native_paths(&repo_root, &config), config.site().clone()).await?;
    let result = match args.command {
        StackCommand::Show => stack_show(&substrate).await,
        StackCommand::Restack(restack) => {
            stack_restack(restack, &config, &repo_root, &substrate).await
        }
        StackCommand::Submit(submit) => stack_submit(submit, &config, &repo_root, &substrate).await,
    };
    substrate.close().await?;
    result
}

fn build_backend(
    repo_root: &Path,
    config: &Config,
) -> Result<Arc<dyn StackBackend>, crate::CliError> {
    let stack_cfg = config.tools().git().stacking();
    let backend: Arc<dyn StackBackend> = match stack_cfg.backend() {
        StackBackendKind::Native => Arc::new(NativeStackBackend::new(
            repo_root.to_path_buf(),
            stack_cfg.force_push(),
        )),
        StackBackendKind::None => Arc::new(NoneStackBackend),
    };
    Ok(backend)
}

async fn stack_show(substrate: &NativeSubstrate) -> Result<CliExitCode, crate::CliError> {
    let mut tickets = substrate
        .list_tickets(TicketFilter::default())
        .await
        .map_err(|error| message(format!("list tickets: {error}")))?;
    tickets.sort_by(|a, b| {
        let a_key = (
            a.batch
                .as_ref()
                .map(BatchName::as_str)
                .unwrap_or("")
                .to_owned(),
            a.ordinal.unwrap_or(u32::MAX),
            a.id.as_str().to_owned(),
        );
        let b_key = (
            b.batch
                .as_ref()
                .map(BatchName::as_str)
                .unwrap_or("")
                .to_owned(),
            b.ordinal.unwrap_or(u32::MAX),
            b.id.as_str().to_owned(),
        );
        a_key.cmp(&b_key)
    });
    println!(
        "{:<14} {:<16} {:<10} {:<40} {:<6} HEALTH",
        "TICKET", "BATCH", "STATE", "BRANCH", "PR"
    );
    for ticket in &tickets {
        let metadata = substrate
            .most_recent_in_review_metadata(&ticket.id)
            .await
            .map_err(|error| message(format!("read metadata for {}: {error}", ticket.id)))?;
        let branch = metadata.as_ref().map(|m| m.branch.as_str()).unwrap_or("-");
        let pr = metadata
            .as_ref()
            .and_then(|m| m.pr_url.as_deref())
            .unwrap_or("-");
        let health = match &ticket.block_reason {
            Some(BlockReason::RestackConflict { .. }) => "restack-conflict",
            _ => "ok",
        };
        println!(
            "{:<14} {:<16} {:<10} {:<40} {:<6} {}",
            ticket.id.as_str(),
            ticket.batch.as_ref().map(BatchName::as_str).unwrap_or("-"),
            ticket.state.to_string(),
            branch,
            pr,
            health,
        );
    }
    Ok(CliExitCode::Success)
}

async fn stack_restack(
    args: StackRestackArgs,
    config: &Config,
    repo_root: &Path,
    substrate: &NativeSubstrate,
) -> Result<CliExitCode, crate::CliError> {
    let backend = build_backend(repo_root, config)?;
    let target_branch = "main".to_owned();
    let tickets = substrate
        .list_tickets(TicketFilter::default())
        .await
        .map_err(|error| message(format!("list tickets: {error}")))?;
    let mut restacked = 0_usize;
    let mut conflicts = 0_usize;
    for ticket in tickets {
        if let Some(filter) = args.batch.as_deref() {
            let batch_ok = ticket
                .batch
                .as_ref()
                .map(|b| b.as_str() == filter)
                .unwrap_or(false);
            if !batch_ok {
                continue;
            }
        }
        if !matches!(ticket.state, TicketState::InFlight | TicketState::InReview) {
            continue;
        }
        let metadata = substrate
            .most_recent_in_review_metadata(&ticket.id)
            .await
            .map_err(|error| message(format!("read metadata: {error}")))?;
        let Some(metadata) = metadata else {
            continue;
        };
        let predecessors = substrate
            .blocks_predecessors(&ticket.id)
            .await
            .map_err(|error| message(format!("read predecessors: {error}")))?;
        if predecessors.is_empty() {
            continue;
        }
        let mut pred_tickets = Vec::new();
        for pred_id in &predecessors {
            if let Some(pred) = substrate
                .get_ticket(pred_id)
                .await
                .map_err(|error| message(format!("get predecessor: {error}")))?
            {
                pred_tickets.push(pred);
            }
        }
        // If any predecessor is non-terminal, restack onto its branch;
        // otherwise restack onto target_branch.
        let any_non_terminal = pred_tickets.iter().any(|t| !t.state.is_terminal());
        let new_parent = if any_non_terminal {
            derrick_stack::parent_branch_for(
                &predecessors,
                &pred_tickets,
                &target_branch,
                config.tools().git().stacking().branch_pattern(),
            )
        } else {
            target_branch.clone()
        };
        let outcome = backend
            .restack(RestackParams {
                branch: metadata.branch.clone(),
                old_parent: target_branch.clone(),
                new_parent: new_parent.clone(),
                repo_root: repo_root.to_path_buf(),
            })
            .await
            .map_err(|error| message(format!("restack {}: {error}", metadata.branch)))?;
        match outcome {
            RestackOutcome::Restacked => {
                // A local rebase alone leaves the remote stale; publish the
                // rewritten branch with --force-with-lease so the open PR
                // reflects the new parent. Mirrors the foreman's restack path
                // (§8.5 step 4). When the force_push policy is off the native
                // backend reports NotSupported; like the foreman, warn and
                // continue rather than failing the whole restack run.
                if let Err(error) = backend.force_push(&metadata.branch, repo_root).await {
                    println!(
                        "restacked {} onto {} but force-push failed: {error}",
                        metadata.branch, new_parent
                    );
                } else {
                    println!("restacked {} onto {}", metadata.branch, new_parent);
                }
                restacked += 1;
            }
            RestackOutcome::Conflict { recipe } => {
                println!("conflict on {}: resolve with: {}", metadata.branch, recipe);
                conflicts += 1;
            }
            _ => {}
        }
    }
    println!("restack summary: ok={restacked} conflicts={conflicts}");
    Ok(CliExitCode::Success)
}

async fn stack_submit(
    args: StackSubmitArgs,
    config: &Config,
    repo_root: &Path,
    substrate: &NativeSubstrate,
) -> Result<CliExitCode, crate::CliError> {
    let backend = build_backend(repo_root, config)?;
    let target_branch = "main".to_owned();
    let filter = TicketFilter {
        state: Some(TicketState::InReview),
        ..TicketFilter::default()
    };
    let tickets = substrate
        .list_tickets(filter)
        .await
        .map_err(|error| message(format!("list tickets: {error}")))?;
    let mut opened = 0_usize;
    for ticket in tickets {
        if let Some(b) = args.batch.as_deref() {
            let batch_ok = ticket
                .batch
                .as_ref()
                .map(|x| x.as_str() == b)
                .unwrap_or(false);
            if !batch_ok {
                continue;
            }
        }
        let Some(metadata) = substrate
            .most_recent_in_review_metadata(&ticket.id)
            .await
            .map_err(|error| message(format!("read metadata: {error}")))?
        else {
            continue;
        };
        if metadata.pr_url.is_some() {
            continue;
        }
        // Compute parent branch.
        let predecessors = substrate
            .blocks_predecessors(&ticket.id)
            .await
            .map_err(|error| message(format!("read predecessors: {error}")))?;
        let mut pred_tickets = Vec::new();
        for pred_id in &predecessors {
            if let Some(pred) = substrate
                .get_ticket(pred_id)
                .await
                .map_err(|error| message(format!("get predecessor: {error}")))?
            {
                pred_tickets.push(pred);
            }
        }
        let parent_branch = derrick_stack::parent_branch_for(
            &predecessors,
            &pred_tickets,
            &target_branch,
            config.tools().git().stacking().branch_pattern(),
        );
        let info = backend
            .open_pr(OpenPrParams {
                branch: metadata.branch.clone(),
                parent_branch,
                title: ticket.title.clone(),
                body: ticket.body.clone(),
                draft: config.tools().git().stacking().draft(),
                repo_root: repo_root.to_path_buf(),
            })
            .await
            .map_err(|error| message(format!("open_pr: {error}")))?;
        // Re-record InReview metadata with the new pr_url / pr_number /
        // head_sha so future ticks see the published PR. The ticket is
        // already InReview (we filtered on it), so we emit a fresh
        // `TicketTransitionedToInReview` event directly rather than
        // calling `transition_to_in_review`, which would reject because
        // the substrate requires the current state to be InFlight.
        substrate
            .record_typed_event(
                EventScope::Ticket(ticket.id.clone()),
                EventKind::TicketTransitionedToInReview {
                    branch: metadata.branch.clone(),
                    pr_url: Some(info.url.clone()),
                    pr_number: Some(info.number),
                    head_sha: info.head_sha,
                },
            )
            .await
            .map_err(|error| message(format!("update metadata: {error}")))?;
        println!("opened {} for {} → {}", info.url, ticket.id, info.number);
        opened += 1;
    }
    println!("submit summary: opened={opened}");
    Ok(CliExitCode::Success)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;

    fn config_with_backend(backend: &str) -> Config {
        let yaml = format!(
            r#"
version: 1
site:
  name: test-site
  prefix: tst
models:
  claude-sonnet:
    provider: anthropic
    model: claude-sonnet-4-6
roles:
  drafter: claude-sonnet
  proposer: claude-sonnet
  reviewer: claude-sonnet
tools:
  speckit:
    enabled: true
    version: ">=0.4.0"
  assay:
    enabled: true
    role: reviewer
    reviewers: [reviewer]
    rounds: 1
  substrate:
    backend: native
    mode: solo
  copilot:
    enabled: false
    agent_identity: derrick-hand
  git:
    stacking:
      backend: {backend}
pipeline: []
guardrails:
  constitution_path: .specify/memory/constitution.md
  forbid_paths: []
  required_labels: []
parallelism:
  batch_max: 8
  step_max: 4
  assay_max: 2
state:
  dir: .derrick
  log_runs: true
  worktree_root: .derrick/worktrees
"#
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("derrick.yaml");
        fs::write(&path, yaml).expect("write yaml");
        Config::load_from_path(&path).expect("load config")
    }

    /// The native backend is derrick's only real stacking engine (D72) and
    /// builds without any third-party binary on PATH.
    #[test]
    fn native_config_builds_native_backend() {
        let config = config_with_backend("native");
        let backend = build_backend(Path::new("."), &config).expect("build native backend");
        assert_eq!(backend.kind(), "native");
    }

    /// The `none` backend disables stacking and builds unconditionally.
    #[test]
    fn none_config_builds_none_backend() {
        let config = config_with_backend("none");
        let backend = build_backend(Path::new("."), &config).expect("build none backend");
        assert_eq!(backend.kind(), "none");
    }

    /// A removed third-party backend name must fail config load with the
    /// actionable D72 error rather than silently building anything.
    #[test]
    fn graphite_backend_name_is_rejected_at_config_load() {
        let yaml = removed_backend_yaml("graphite");
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("derrick.yaml");
        fs::write(&path, yaml).expect("write yaml");
        let err = Config::load_from_path(&path).expect_err("graphite must be rejected");
        let text = err.to_string();
        assert!(text.contains("graphite"), "got: {text}");
        assert!(text.contains("native"), "got: {text}");
    }

    fn removed_backend_yaml(backend: &str) -> String {
        format!(
            r#"
version: 1
site:
  name: test-site
  prefix: tst
models:
  claude-sonnet:
    provider: anthropic
    model: claude-sonnet-4-6
roles:
  drafter: claude-sonnet
  proposer: claude-sonnet
  reviewer: claude-sonnet
tools:
  speckit:
    enabled: true
    version: ">=0.4.0"
  assay:
    enabled: true
    role: reviewer
    reviewers: [reviewer]
    rounds: 1
  substrate:
    backend: native
    mode: solo
  copilot:
    enabled: false
    agent_identity: derrick-hand
  git:
    stacking:
      backend: {backend}
pipeline: []
guardrails:
  constitution_path: .specify/memory/constitution.md
  forbid_paths: []
  required_labels: []
parallelism:
  batch_max: 8
  step_max: 4
  assay_max: 2
state:
  dir: .derrick
  log_runs: true
  worktree_root: .derrick/worktrees
"#
        )
    }
}
