//! `derrick ticket ...` subcommands. See DESIGN.md §8.2 and T012.

use std::io::Write;
use std::path::Path;
use std::process::Command;

use derrick_config::{Config, SubstrateBackendKind, SubstrateMode};
use derrick_substrate::{
    BlockReason, InReviewMetadata, LinkKind, ManualDoneAttestation, Substrate, TicketFilter,
    TicketId, TicketState,
};
use derrick_substrate_native::NativeSubstrate;

use crate::commands::{
    TicketArgs, TicketBlockArgs, TicketCodeReviewArgs, TicketCommand, TicketDoneArgs,
    TicketRejectArgs, TicketReopenArgs, TicketReviewArgs, TicketShowArgs,
};
use crate::exit_code::CliExitCode;
use crate::{create_dir_all, current_repo_root, message, native_paths, read_config, write_file};

/// Executes the `derrick ticket` subcommand.
pub(crate) async fn execute(args: TicketArgs) -> Result<CliExitCode, crate::CliError> {
    let repo_root = current_repo_root()?;
    let config = read_config(&repo_root)?;
    if config.tools().substrate().backend() != SubstrateBackendKind::Native {
        return Err(message(
            "derrick ticket subcommands require tools.substrate.backend: native",
        ));
    }
    let substrate =
        NativeSubstrate::open(native_paths(&repo_root, &config), config.site().clone()).await?;
    let result = dispatch(args.command, &config, &repo_root, &substrate).await;
    substrate.close().await?;
    result
}

async fn dispatch(
    command: TicketCommand,
    config: &Config,
    repo_root: &Path,
    substrate: &NativeSubstrate,
) -> Result<CliExitCode, crate::CliError> {
    match command {
        TicketCommand::Done(done) => ticket_done(done, config, substrate).await,
        TicketCommand::Review(review) => ticket_review(review, repo_root, substrate).await,
        TicketCommand::List => ticket_list(substrate).await,
        TicketCommand::Show(show) => ticket_show(show, substrate).await,
        TicketCommand::Reject(reject) => ticket_reject(reject, substrate).await,
        TicketCommand::Reopen(reopen) => ticket_reopen(reopen, substrate).await,
        TicketCommand::Block(block) => ticket_block(block, substrate).await,
        TicketCommand::CodeReview(args) => {
            ticket_code_review(args, config, repo_root, substrate).await
        }
    }
}

async fn ticket_done(
    args: TicketDoneArgs,
    config: &Config,
    substrate: &NativeSubstrate,
) -> Result<CliExitCode, crate::CliError> {
    if config.tools().substrate().mode() != SubstrateMode::Solo {
        eprintln!(
            "Done is reached via the foreman's verifier in crew/copilot modes. \
             Use `derrick ticket review` to mark work ready for the verifier. \
             (See DESIGN.md \u{a7}8.6, D31.)"
        );
        return Ok(CliExitCode::Refused);
    }

    let ticket_id = parse_ticket_id(&args.id)?;
    let note = match args.note {
        Some(note) => note,
        None => prompt_for_note("Note: ")?,
    };
    let claimant = git_user_name().unwrap_or_else(|| "unknown".to_owned());
    let attestation = ManualDoneAttestation { claimant, note };
    let ticket = substrate
        .mark_ticket_done_manually(&ticket_id, attestation)
        .await?;
    println!("ticket {} -> {}", ticket.id, ticket.state);
    Ok(CliExitCode::Success)
}

async fn ticket_review(
    args: TicketReviewArgs,
    repo_root: &Path,
    substrate: &NativeSubstrate,
) -> Result<CliExitCode, crate::CliError> {
    let ticket_id = parse_ticket_id(&args.id)?;
    let pr_url = match args.pr_url {
        Some(url) => Some(url),
        None => lookup_pr_url(&args.branch, repo_root),
    };
    let pr_number = pr_url.as_deref().and_then(parse_pr_number_from_url);
    let metadata = InReviewMetadata {
        branch: args.branch,
        pr_url,
        pr_number,
        head_sha: args.head_sha,
    };
    let ticket = substrate
        .transition_to_in_review(&ticket_id, metadata)
        .await?;
    println!("ticket {} -> {}", ticket.id, ticket.state);
    Ok(CliExitCode::Success)
}

async fn ticket_list(substrate: &NativeSubstrate) -> Result<CliExitCode, crate::CliError> {
    let tickets = substrate.list_tickets(TicketFilter::default()).await?;
    let header_id = "id";
    let header_state = "state";
    let header_owner = "owner";
    let header_title = "title";
    println!("{header_id:<16} {header_state:<12} {header_owner:<12} {header_title}");
    for ticket in tickets {
        let owner = ticket
            .owner
            .as_ref()
            .map_or("-", |hand| hand.as_str())
            .to_owned();
        println!(
            "{:<16} {:<12} {:<12} {}",
            ticket.id, ticket.state, owner, ticket.title
        );
    }
    Ok(CliExitCode::Success)
}

async fn ticket_show(
    args: TicketShowArgs,
    substrate: &NativeSubstrate,
) -> Result<CliExitCode, crate::CliError> {
    let ticket_id = parse_ticket_id(&args.id)?;
    let ticket = substrate
        .get_ticket(&ticket_id)
        .await?
        .ok_or_else(|| message(format!("ticket {} not found", ticket_id)))?;
    println!("id:           {}", ticket.id);
    println!("title:        {}", ticket.title);
    println!("state:        {}", ticket.state);
    if let Some(batch) = &ticket.batch {
        println!("batch:        {batch}");
    }
    if let Some(ordinal) = ticket.ordinal {
        println!("ordinal:      {ordinal}");
    }
    if let Some(owner) = &ticket.owner {
        println!("owner:        {owner}");
    }
    if let Some(merge_sha) = &ticket.merge_sha {
        println!("merge_sha:    {merge_sha}");
    }
    if let Some(reason) = &ticket.block_reason {
        println!(
            "block_reason: {}",
            serde_json::to_string(reason).unwrap_or_else(|_| "<unserialisable>".to_owned())
        );
    }
    if !ticket.labels.is_empty() {
        println!("labels:       {}", ticket.labels.join(", "));
    }
    println!("created_at:   {}", ticket.created_at);
    println!("updated_at:   {}", ticket.updated_at);
    if !ticket.body.is_empty() {
        println!("\n{}", ticket.body);
    }
    Ok(CliExitCode::Success)
}

async fn ticket_reopen(
    args: TicketReopenArgs,
    substrate: &NativeSubstrate,
) -> Result<CliExitCode, crate::CliError> {
    let ticket_id = parse_ticket_id(&args.id)?;
    let ticket = substrate
        .human_reopen_blocked(&ticket_id, args.note)
        .await?;
    println!("ticket {} -> {}", ticket.id, ticket.state);
    Ok(CliExitCode::Success)
}

async fn ticket_reject(
    args: TicketRejectArgs,
    substrate: &NativeSubstrate,
) -> Result<CliExitCode, crate::CliError> {
    let ticket_id = parse_ticket_id(&args.id)?;
    let existing = substrate
        .get_ticket(&ticket_id)
        .await?
        .ok_or_else(|| message(format!("ticket {} not found", ticket_id)))?;

    // Done and already-Rejected are terminal — refuse so the user gets a
    // clear, non-zero exit without a substrate error.
    match existing.state {
        TicketState::Done => {
            eprintln!(
                "ticket {} is already Done and cannot be rejected",
                ticket_id
            );
            return Ok(CliExitCode::Refused);
        }
        TicketState::Rejected => {
            eprintln!("ticket {} is already Rejected", ticket_id);
            return Ok(CliExitCode::Refused);
        }
        _ => {}
    }

    let reason = match args.reason {
        Some(r) => r,
        None => prompt_for_note("Reason: ")?,
    };

    let ticket = substrate.reject_ticket(&ticket_id, reason.clone()).await?;
    println!("ticket {} -> {} ({})", ticket.id, ticket.state, reason);
    Ok(CliExitCode::Success)
}

async fn ticket_block(
    args: TicketBlockArgs,
    substrate: &NativeSubstrate,
) -> Result<CliExitCode, crate::CliError> {
    if args.on.is_none() && args.note.is_none() {
        return Err(message(
            "derrick ticket block requires at least one of --on or --note",
        ));
    }
    let ticket_id = parse_ticket_id(&args.id)?;
    let existing = substrate
        .get_ticket(&ticket_id)
        .await?
        .ok_or_else(|| message(format!("ticket {} not found", ticket_id)))?;
    if existing.state.is_terminal() {
        return Err(message(format!(
            "ticket {} is already terminal ({}); cannot block",
            existing.id, existing.state
        )));
    }

    if let Some(pred_raw) = args.on.as_deref() {
        let predecessor = parse_ticket_id(pred_raw)?;
        // Write the blocks link first; idempotent on duplicate.
        substrate
            .link(&ticket_id, &predecessor, LinkKind::Blocks)
            .await?;
        let pred_ticket = substrate
            .get_ticket(&predecessor)
            .await?
            .ok_or_else(|| message(format!("predecessor ticket {} not found", predecessor)))?;
        if pred_ticket.state.is_terminal() {
            println!(
                "ticket {} blocks link to {} recorded; predecessor already {} so no state change",
                ticket_id, predecessor, pred_ticket.state
            );
            return Ok(CliExitCode::Success);
        }
        if existing.state == TicketState::Blocked
            && !matches!(
                existing.block_reason.as_ref(),
                Some(BlockReason::Dependency { predecessor: p }) if p == &predecessor
            )
        {
            return Err(message(format!(
                "ticket {ticket_id} is already Blocked with a different reason; use \
                 `derrick ticket reopen` first"
            )));
        }
        let ticket = substrate
            .block_ticket(
                &ticket_id,
                BlockReason::Dependency {
                    predecessor: predecessor.clone(),
                },
            )
            .await?;
        println!(
            "ticket {} -> {} (dependency on {})",
            ticket.id, ticket.state, predecessor
        );
        return Ok(CliExitCode::Success);
    }

    // --note path (no predecessor): human block.
    let note = args.note.unwrap_or_default();
    if existing.state == TicketState::Blocked {
        return Err(message(format!(
            "ticket {} is already Blocked; use `derrick ticket reopen` first",
            ticket_id
        )));
    }
    let ticket = substrate
        .block_ticket(&ticket_id, BlockReason::Human { note })
        .await?;
    println!("ticket {} -> {}", ticket.id, ticket.state);
    Ok(CliExitCode::Success)
}

async fn ticket_code_review(
    args: TicketCodeReviewArgs,
    config: &Config,
    repo_root: &Path,
    substrate: &NativeSubstrate,
) -> Result<CliExitCode, crate::CliError> {
    let cr = config.tools().code_review();

    if !cr.enabled() {
        println!("code_review is disabled in derrick.yaml (tools.code_review.enabled: false)");
        println!("To enable: set tools.code_review.enabled: true and configure a reviewer role.");
        return Ok(CliExitCode::Success);
    }

    let ticket_id = parse_ticket_id(&args.id)?;
    let ticket = substrate
        .get_ticket(&ticket_id)
        .await?
        .ok_or_else(|| message(format!("ticket {} not found", ticket_id)))?;

    let base = cr.base_branch();
    let diff = git_diff_branch(repo_root, base, &args.branch)?;

    if diff.trim().is_empty() {
        println!(
            "No diff between {base} and {}. Code review passed.",
            args.branch
        );
        return Ok(CliExitCode::Success);
    }

    let outcome = derrick_flow::run_code_review(
        &diff,
        &ticket.title,
        &ticket.body,
        cr.role(),
        config,
        repo_root,
    )
    .await
    .map_err(|e| message(e.to_string()))?;

    let review_dir = repo_root
        .join(".derrick")
        .join("reviews")
        .join(ticket_id.as_str());
    create_dir_all(&review_dir)?;
    let review_path = review_dir.join(format!("round-{}.md", args.round));
    write_file(&review_path, &outcome.review_text)?;

    match outcome.verdict.as_str() {
        "pass" => {
            println!(
                "Code review passed (round {}). Review written to: {}",
                args.round,
                review_path.display()
            );
            Ok(CliExitCode::Success)
        }
        _ => {
            eprintln!(
                "Code review found issues (round {}):\n\n{}",
                args.round, outcome.review_text
            );
            eprintln!(
                "\nReview written to: {}\n\
                 Fix the issues above, then run: derrick ticket code-review {} --branch {} --round {}",
                review_path.display(),
                args.id,
                args.branch,
                args.round + 1
            );
            Ok(CliExitCode::ReviewIssues)
        }
    }
}

fn git_diff_branch(repo_root: &Path, base: &str, branch: &str) -> Result<String, crate::CliError> {
    let output = Command::new("git")
        .args(["diff", &format!("origin/{base}...{branch}"), "--"])
        .current_dir(repo_root)
        .output()
        .map_err(|source| crate::CliError::Io {
            path: repo_root.to_path_buf(),
            source,
        })?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn parse_ticket_id(raw: &str) -> Result<TicketId, crate::CliError> {
    TicketId::new(raw).map_err(|error| message(format!("invalid ticket id {raw:?}: {error}")))
}

fn prompt_for_note(prompt: &str) -> Result<String, crate::CliError> {
    let mut stdout = std::io::stdout();
    stdout
        .write_all(prompt.as_bytes())
        .and_then(|()| stdout.flush())
        .map_err(|source| crate::CliError::Io {
            path: "<stdout>".into(),
            source,
        })?;
    let mut buffer = String::new();
    std::io::stdin()
        .read_line(&mut buffer)
        .map_err(|source| crate::CliError::Io {
            path: "<stdin>".into(),
            source,
        })?;
    let trimmed = buffer.trim().to_owned();
    if trimmed.is_empty() {
        Ok("(no note)".to_owned())
    } else {
        Ok(trimmed)
    }
}

fn git_user_name() -> Option<String> {
    let output = Command::new("git")
        .args(["config", "user.name"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let name = String::from_utf8(output.stdout).ok()?;
    let trimmed = name.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

fn lookup_pr_url(branch: &str, repo_root: &Path) -> Option<String> {
    let output = Command::new("gh")
        .args(["pr", "view", branch, "--json", "url", "-q", ".url"])
        .current_dir(repo_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8(output.stdout).ok()?;
    let trimmed = url.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

fn parse_pr_number_from_url(url: &str) -> Option<u64> {
    url.rsplit('/').next().and_then(|tail| tail.parse().ok())
}
