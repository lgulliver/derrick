//! Minimal derrick command-line interface for T008.

#![allow(clippy::print_stdout)]
#![allow(clippy::print_stderr)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{CommandFactory, Parser};
use commands::{Cli, Command};
use thiserror::Error;

mod commands;
mod exit_code;
mod output;
mod telemetry;

/// Runs the CLI with an argument iterator and returns the process exit code.
pub async fn run<I, T>(args: I) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => {
            let code = error.exit_code();
            if let Err(print_error) = error.print() {
                eprintln!("failed to print clap error: {print_error}");
            }
            return exit_code::from_i32(code);
        }
    };

    match dispatch(cli).await {
        Ok(code) => code.into(),
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(1)
        }
    }
}

async fn dispatch(cli: Cli) -> Result<exit_code::CliExitCode, CliError> {
    match cli.command {
        Command::Add(args) => commands::add::execute(args).await,
        Command::Init(args) => commands::init::execute(args).await,
        Command::Status(args) => commands::status::execute(args).await,
        Command::Doctor(args) => commands::doctor::execute(args).await,
        Command::Run(args) => commands::run::execute(args).await,
        Command::Completions(args) => {
            commands::completions::execute(args, &mut Cli::command());
            Ok(exit_code::CliExitCode::Success)
        }
        Command::Ticket(args) => commands::ticket::execute(args).await,
        Command::Foreman(args) => commands::foreman::execute(args).await,
        Command::Stack(args) => commands::stack::execute(args).await,
        Command::Observe(args) => commands::observe::execute(args).await,
        Command::Uninstall(args) => commands::uninstall::execute(args).await,
        Command::Upgrade(args) => commands::upgrade::execute(args).await,
        Command::Scrub(args) => commands::scrub::run(args)
            .await
            .map(|()| exit_code::CliExitCode::Success)
            .map_err(|error| message(error.to_string())),
        Command::Caveman(args) => commands::caveman::run(args)
            .await
            .map(|()| exit_code::CliExitCode::Success)
            .map_err(|error| message(error.to_string())),
        Command::Gain(args) => commands::gain::run(args)
            .await
            .map(|()| exit_code::CliExitCode::Success)
            .map_err(|error| message(error.to_string())),
        Command::Switch(args) => commands::switch::execute(args).await,
    }
}

#[derive(Debug, Error)]
enum CliError {
    #[error("{0}")]
    Message(String),
    #[error("IO error at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{0}")]
    Config(#[from] derrick_config::ConfigError),
    #[error("{0}")]
    Substrate(#[from] derrick_substrate::SubstrateError),
    #[error("{0}")]
    Adopt(#[from] derrick_adopt::AdoptError),
    #[error("{0}")]
    Json(#[from] serde_json::Error),
}

fn message(text: impl Into<String>) -> CliError {
    CliError::Message(text.into())
}

fn find_repo_root(start: &Path) -> Result<PathBuf, CliError> {
    for candidate in start.ancestors() {
        if candidate.join(".git").exists() {
            return Ok(candidate.to_path_buf());
        }
    }

    Err(message("derrick init must be run inside a git repo"))
}

fn current_repo_root() -> Result<PathBuf, CliError> {
    let cwd = std::env::current_dir().map_err(|source| CliError::Io {
        path: PathBuf::from("."),
        source,
    })?;
    find_repo_root(&cwd)
}

fn read_config(repo_root: &Path) -> Result<derrick_config::Config, CliError> {
    derrick_config::Config::load_from_path(&repo_root.join("derrick.yaml")).map_err(Into::into)
}

fn native_paths(
    repo_root: &Path,
    config: &derrick_config::Config,
) -> derrick_substrate_native::NativeConfig {
    derrick_substrate_native::NativeConfig {
        db_path: repo_root.join(config.state().dir()).join("derrick.db"),
        worktree_root: repo_root.join(config.state().worktree_root()),
    }
}

fn write_file(path: &Path, contents: &str) -> Result<(), CliError> {
    std::fs::write(path, contents).map_err(|source| CliError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn create_dir_all(path: &Path) -> Result<(), CliError> {
    std::fs::create_dir_all(path).map_err(|source| CliError::Io {
        path: path.to_path_buf(),
        source,
    })
}
