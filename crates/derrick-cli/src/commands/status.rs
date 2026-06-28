use std::io::{Write, stdout};
use std::path::Path;
use std::time::Duration;

use crossterm::{cursor, execute, terminal};
use derrick_config::{Config, SubstrateBackendKind};
use derrick_substrate::{Substrate, TicketFilter, TicketState};
use derrick_substrate_native::NativeSubstrate;
use serde_json::json;

use crate::commands::StatusArgs;
use crate::exit_code::CliExitCode;
use crate::output::OutputFormat;
use crate::{current_repo_root, message, native_paths, read_config};

/// Executes the `derrick status` subcommand.
pub(crate) async fn execute(args: StatusArgs) -> Result<CliExitCode, crate::CliError> {
    if args.watch {
        loop {
            let status = load_status().await?;
            execute!(
                stdout(),
                terminal::Clear(terminal::ClearType::All),
                cursor::MoveTo(0, 0)
            )
            .map_err(|source| crate::CliError::Io {
                path: ".".into(),
                source,
            })?;
            print_status(&status, args.format)?;
            stdout().flush().map_err(|source| crate::CliError::Io {
                path: ".".into(),
                source,
            })?;
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    let status = load_status().await?;
    print_status(&status, args.format)?;
    Ok(CliExitCode::Success)
}

struct StatusSnapshot {
    site: String,
    mode: String,
    backend: String,
    db: String,
    batch_count: usize,
    ticket_count: usize,
    ready: usize,
    in_flight: usize,
    done: usize,
    foreman: String,
    active_profile: Option<String>,
}

async fn load_status() -> Result<StatusSnapshot, crate::CliError> {
    let repo_root = current_repo_root()?;
    let config = read_config(&repo_root)?;
    match config.tools().substrate().backend() {
        SubstrateBackendKind::Native => native_status(&repo_root, &config).await,
        SubstrateBackendKind::None => Ok(StatusSnapshot {
            site: config.site().name().to_owned(),
            mode: mode_name(config.tools().substrate().mode()).to_owned(),
            backend: "none".to_owned(),
            db: "-".to_owned(),
            batch_count: 0,
            ticket_count: 0,
            ready: 0,
            in_flight: 0,
            done: 0,
            foreman: "disabled".to_owned(),
            active_profile: config.default_profile().map(str::to_owned),
        }),
    }
}

async fn native_status(
    repo_root: &Path,
    config: &Config,
) -> Result<StatusSnapshot, crate::CliError> {
    let native_config = native_paths(repo_root, config);
    if !native_config.db_path.exists() {
        return Err(message(format!(
            "{} does not exist; run `derrick init --greenfield` first",
            native_config.db_path.display()
        )));
    }

    let substrate = NativeSubstrate::open(native_config.clone(), config.site().clone()).await?;
    let tickets = substrate.list_tickets(TicketFilter::default()).await?;
    let batches = substrate.list_batches(false).await?;
    let foreman = substrate.foreman_status().await?;
    substrate.close().await?;

    let ready = tickets
        .iter()
        .filter(|ticket| ticket.state == TicketState::Ready)
        .count();
    let in_flight = tickets
        .iter()
        .filter(|ticket| ticket.state == TicketState::InFlight)
        .count();
    let done = tickets
        .iter()
        .filter(|ticket| ticket.state == TicketState::Done)
        .count();

    Ok(StatusSnapshot {
        site: config.site().name().to_owned(),
        mode: mode_name(config.tools().substrate().mode()).to_owned(),
        backend: "native".to_owned(),
        db: native_config.db_path.display().to_string(),
        batch_count: batches.len(),
        ticket_count: tickets.len(),
        ready,
        in_flight,
        done,
        foreman: match foreman.pid {
            Some(pid) => format!("detached (pid {pid})"),
            None => "stopped".to_owned(),
        },
        active_profile: config.default_profile().map(str::to_owned),
    })
}

fn print_status(status: &StatusSnapshot, format: OutputFormat) -> Result<(), crate::CliError> {
    match format {
        OutputFormat::Human => {
            println!(
                "site         {}                            mode: {}",
                status.site, status.mode
            );
            println!(
                "backend      {}                                 db: {}",
                status.backend, status.db
            );
            println!(
                "batch        {} active batches                  {} tickets",
                status.batch_count, status.ticket_count
            );
            println!(
                "tickets      {} done | {} in-flight | {} ready",
                status.done, status.in_flight, status.ready
            );
            println!("foreman      {}", status.foreman);
            if let Some(profile) = &status.active_profile {
                println!("profile      {profile}");
            }
        }
        OutputFormat::Json => {
            let body = json!({
                "site": status.site,
                "mode": status.mode,
                "backend": status.backend,
                "db": status.db,
                "batch_count": status.batch_count,
                "ticket_count": status.ticket_count,
                "tickets": {
                    "ready": status.ready,
                    "in_flight": status.in_flight,
                    "done": status.done
                },
                "foreman": status.foreman,
                "profile": status.active_profile
            });
            println!("{}", serde_json::to_string(&body)?);
        }
    }
    Ok(())
}

fn mode_name(mode: derrick_config::SubstrateMode) -> &'static str {
    match mode {
        derrick_config::SubstrateMode::Solo => "solo",
        derrick_config::SubstrateMode::Copilot => "copilot",
        derrick_config::SubstrateMode::Crew => "crew",
    }
}
