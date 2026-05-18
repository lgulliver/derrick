use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::output::OutputFormat;

pub(crate) mod completions;
pub(crate) mod doctor;
pub(crate) mod foreman;
pub(crate) mod init;
pub(crate) mod run;
pub(crate) mod status;
pub(crate) mod ticket;

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
    Ticket(TicketArgs),
    Foreman(ForemanArgs),
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
    #[arg(long)]
    pub(crate) yes: bool,
    #[arg(long)]
    pub(crate) dry_run: bool,
    #[arg(long)]
    pub(crate) no_hooks: bool,
    #[arg(long)]
    pub(crate) append_agents_md: bool,
    #[arg(long, conflicts_with = "constitution_from_docs")]
    pub(crate) constitution_stub: bool,
    #[arg(long, conflicts_with = "constitution_stub")]
    pub(crate) constitution_from_docs: bool,
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
    #[arg(long = "run")]
    pub(crate) run_id: Option<String>,
    #[arg(long = "skip")]
    pub(crate) skip: Vec<String>,
    #[arg(long = "unskip")]
    pub(crate) unskip: Vec<String>,
    #[arg(long)]
    pub(crate) dry_run: bool,
    #[arg(long, help = "Alias for `--skip clarify`")]
    pub(crate) no_clarify: bool,
    #[arg(long, help = "Alias for `--skip checkpoint`")]
    pub(crate) no_checkpoint: bool,
    #[arg(long, help = "Alias for `--skip assay`")]
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

// ---------- Ticket subcommand group (T012) -------------------------------

#[derive(Debug, Args)]
pub(crate) struct TicketArgs {
    #[command(subcommand)]
    pub(crate) command: TicketCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum TicketCommand {
    /// Mark a ticket Done manually (`mode: solo` only).
    Done(TicketDoneArgs),
    /// Hand-driven transition InFlight -> InReview with verifier metadata.
    Review(TicketReviewArgs),
    /// List all tickets.
    List,
    /// Show one ticket's full details.
    Show(TicketShowArgs),
    /// Reject a ticket (stub — implemented in a follow-up).
    Reject(TicketRejectArgs),
    /// Reopen a Blocked ticket back to Ready.
    Reopen(TicketReopenArgs),
    /// Block a ticket on a predecessor and/or with a human note.
    Block(TicketBlockArgs),
}

#[derive(Debug, Args)]
pub(crate) struct TicketDoneArgs {
    pub(crate) id: String,
    #[arg(long)]
    pub(crate) note: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct TicketReviewArgs {
    pub(crate) id: String,
    #[arg(long)]
    pub(crate) branch: String,
    #[arg(long = "pr-url")]
    pub(crate) pr_url: Option<String>,
    #[arg(long = "head-sha")]
    pub(crate) head_sha: String,
}

#[derive(Debug, Args)]
pub(crate) struct TicketShowArgs {
    pub(crate) id: String,
}

#[derive(Debug, Args)]
pub(crate) struct TicketRejectArgs {
    pub(crate) id: String,
    #[arg(long)]
    pub(crate) reason: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct TicketReopenArgs {
    pub(crate) id: String,
    #[arg(long)]
    pub(crate) note: String,
}

#[derive(Debug, Args)]
pub(crate) struct TicketBlockArgs {
    pub(crate) id: String,
    #[arg(long)]
    pub(crate) on: Option<String>,
    #[arg(long)]
    pub(crate) note: Option<String>,
}

// ---------- Foreman subcommand group (T012) ------------------------------

#[derive(Debug, Args)]
pub(crate) struct ForemanArgs {
    #[command(subcommand)]
    pub(crate) command: ForemanCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ForemanCommand {
    /// Start the foreman loop.
    Start(ForemanStartArgs),
    /// Stop a detached foreman loop.
    Stop(ForemanStopArgs),
    /// Run a single tick in the foreground.
    Tick(ForemanTickArgs),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum ForemanStartMode {
    Attached,
    Detached,
}

#[derive(Debug, Args)]
pub(crate) struct ForemanStartArgs {
    /// `--attached` blocks; `--detached` forks a daemon (default).
    #[arg(long = "attached", group = "foreman_mode")]
    pub(crate) attached: bool,
    #[arg(long = "detached", group = "foreman_mode")]
    pub(crate) detached: bool,
    /// Internal: invoked by the parent on the daemon child to suppress the
    /// `record_foreman_*` row write (the parent already did it).
    #[arg(long = "__internal-daemon-child", hide = true)]
    pub(crate) internal_daemon_child: bool,
}

impl ForemanStartArgs {
    pub(crate) fn mode(&self) -> Option<ForemanStartMode> {
        if self.attached {
            Some(ForemanStartMode::Attached)
        } else if self.detached {
            Some(ForemanStartMode::Detached)
        } else {
            None
        }
    }
}

#[derive(Debug, Args)]
pub(crate) struct ForemanStopArgs {}

#[derive(Debug, Args)]
pub(crate) struct ForemanTickArgs {}

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
