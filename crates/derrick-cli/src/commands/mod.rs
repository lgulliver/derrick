use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::output::OutputFormat;

pub(crate) mod completions;
pub(crate) mod doctor;
pub(crate) mod init;
pub(crate) mod run;
pub(crate) mod status;

#[derive(Debug, Parser)]
#[command(name = "derrick", version, about = "Derrick orchestration CLI")]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    Init(InitArgs),
    Status(StatusArgs),
    Doctor(DoctorArgs),
    Run(RunArgs),
    Completions(CompletionsArgs),
}

#[derive(Debug, Args)]
pub(crate) struct InitArgs {
    #[arg(long)]
    pub(crate) greenfield: bool,
    #[arg(long, value_enum, default_value_t = InitMode::Solo)]
    pub(crate) mode: InitMode,
    #[arg(long)]
    pub(crate) site: Option<String>,
    #[arg(long)]
    pub(crate) prefix: Option<String>,
    #[arg(long)]
    pub(crate) force: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum InitMode {
    Solo,
    Copilot,
    Crew,
}

impl InitMode {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Solo => "solo",
            Self::Copilot => "copilot",
            Self::Crew => "crew",
        }
    }
}

#[derive(Debug, Args)]
pub(crate) struct StatusArgs {
    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    pub(crate) format: OutputFormat,
    #[arg(long)]
    pub(crate) watch: bool,
}

#[derive(Debug, Args)]
pub(crate) struct DoctorArgs {
    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    pub(crate) format: OutputFormat,
}

#[derive(Debug, Args)]
pub(crate) struct RunArgs {
    #[command(subcommand)]
    pub(crate) command: RunCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum RunCommand {
    AddFeature(AddFeatureArgs),
}

#[derive(Debug, Args)]
pub(crate) struct AddFeatureArgs {
    #[arg(long)]
    pub(crate) prompt: Option<String>,
    #[arg(long)]
    pub(crate) resume_from: Option<String>,
    #[arg(long)]
    pub(crate) no_clarify: bool,
    #[arg(long)]
    pub(crate) no_checkpoint: bool,
    #[arg(long)]
    pub(crate) no_assay: bool,
}

#[derive(Debug, Args)]
pub(crate) struct CompletionsArgs {
    #[arg(value_enum)]
    pub(crate) shell: CompletionShell,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum CompletionShell {
    Bash,
    Zsh,
    Fish,
    Elvish,
    Powershell,
}

impl From<CompletionShell> for clap_complete::Shell {
    fn from(shell: CompletionShell) -> Self {
        match shell {
            CompletionShell::Bash => Self::Bash,
            CompletionShell::Zsh => Self::Zsh,
            CompletionShell::Fish => Self::Fish,
            CompletionShell::Elvish => Self::Elvish,
            CompletionShell::Powershell => Self::PowerShell,
        }
    }
}
