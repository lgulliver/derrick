use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::output::OutputFormat;

pub(crate) mod caveman;
pub(crate) mod completions;
pub(crate) mod doctor;
pub(crate) mod drill;
pub(crate) mod foreman;
pub(crate) mod gain;
pub(crate) mod init;
pub(crate) mod init_wizard;
pub(crate) mod models;
pub(crate) mod observe;
pub(crate) mod prompt_input;
pub(crate) mod reset;
pub(crate) mod run;
pub(crate) mod scrub;
pub(crate) mod stack;
pub(crate) mod status;
pub(crate) mod survey;
pub(crate) mod switch;
pub(crate) mod ticket;
pub(crate) mod undo;
pub(crate) mod uninstall;
pub(crate) mod upgrade;

#[derive(Debug, Parser)]
#[command(name = "derrick", version, about = "Derrick orchestration CLI")]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Shorthand for `run drill` — prompt is a positional argument.
    #[command(alias = "add")]
    Drill(DrillArgs),
    Init(InitArgs),
    Status(StatusArgs),
    Doctor(DoctorArgs),
    /// Inspect and validate model/role/host configuration.
    Models(ModelsArgs),
    Run(RunArgs),
    Completions(CompletionsArgs),
    Ticket(TicketArgs),
    Foreman(ForemanArgs),
    Stack(StackArgs),
    Observe(ObserveArgs),
    Uninstall(UninstallArgs),
    /// Binary self-update from the latest GitHub release.
    Upgrade(UpgradeArgs),
    Scrub(ScrubArgs),
    Caveman(CavemanArgs),
    Gain(GainArgs),
    /// Switch the repo's substrate mode in-place (e.g. solo → crew).
    Switch(SwitchArgs),
    /// Query the native code-graph index (symbols, references, impact).
    Survey(SurveyArgs),
    /// Re-scaffold .claude/ skills and hooks from the current derrick.yaml (preserves config and DB).
    Reset(ResetArgs),
    /// Revert the last hand's git commits.
    Undo(UndoArgs),
}

#[derive(Debug, Args)]
pub(crate) struct ResetArgs {
    /// Skip confirmation prompts.
    #[arg(long, short = 'y')]
    pub(crate) yes: bool,
    /// Preview changes without writing.
    #[arg(long)]
    pub(crate) dry_run: bool,
}

#[derive(Debug, Args)]
pub(crate) struct UndoArgs {
    /// Skip confirmation prompt.
    #[arg(long, short = 'y')]
    pub(crate) yes: bool,
    /// Preview what would be reverted without making changes.
    #[arg(long)]
    pub(crate) dry_run: bool,
}

/// Arguments for `derrick drill` — positional prompt shorthand for `run drill`.
#[derive(Debug, Args)]
pub(crate) struct DrillArgs {
    /// Feature description. Equivalent to `run drill --prompt "..."`.
    pub(crate) prompt: Option<String>,
    #[arg(
        long = "prompt-file",
        value_name = "PATH",
        help = "Read the feature prompt from a file (use - for stdin)"
    )]
    pub(crate) prompt_file: Option<String>,
    #[arg(long = "resume-from")]
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
    #[arg(long, help = "Alias for `--skip assay`")]
    pub(crate) no_assay: bool,
    #[arg(long, help = "Skip the GitHub Issues creation offer")]
    pub(crate) no_github_issues: bool,
    /// Wipe prior run state and start fresh instead of auto-resuming.
    #[arg(long, help = "Discard any prior incomplete run and start from scratch")]
    pub(crate) force: bool,
}

#[derive(Debug, Args)]
pub(crate) struct ScrubArgs {
    /// The tool name to apply rules for (e.g. git, gh, claude, cargo).
    #[arg(long = "tool", value_name = "TOOL")]
    pub(crate) tool: String,
    /// Print scrub statistics to stderr after processing.
    #[arg(long)]
    pub(crate) stats: bool,
}

#[derive(Debug, Args)]
pub(crate) struct CavemanArgs {
    /// Compression intensity.
    #[arg(long, value_enum, default_value_t = CavemanIntensity::Lite)]
    pub(crate) intensity: CavemanIntensity,
    /// Print compression statistics to stderr after processing.
    #[arg(long)]
    pub(crate) stats: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum CavemanIntensity {
    Lite,
    Full,
    Ultra,
}

#[derive(Debug, Args)]
pub(crate) struct GainArgs {
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    pub(crate) format: OutputFormat,
    /// Aggregate all sessions for this repo, not just the most recent.
    #[arg(long)]
    pub(crate) all: bool,
    /// Show token breakdown for a specific pipeline run by run-id.
    #[arg(long = "run")]
    pub(crate) run: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct UninstallArgs {
    /// Skip the confirmation prompt and proceed immediately.
    #[arg(long)]
    pub(crate) yes: bool,
    /// Remove all files without asking, including the state database.
    #[arg(long = "purge")]
    pub(crate) purge: bool,
}

#[derive(Debug, Args)]
pub(crate) struct UpgradeArgs {
    /// Check whether an upgrade is available without installing it.
    #[arg(long)]
    pub(crate) check: bool,
    /// Run the upgrade flow even if the current version appears up to date.
    #[arg(long)]
    pub(crate) force: bool,
}

#[derive(Debug, Args)]
pub(crate) struct ObserveArgs {
    /// Initial tab: overview, tickets, stack, activity, tokens, memory.
    #[arg(long)]
    pub(crate) tab: Option<String>,
    /// Optional site name selector (reserved for multi-site v1.1).
    #[arg(long)]
    pub(crate) site: Option<String>,
    /// Accepted as a no-op in v1; the TUI is already read-only.
    #[arg(long = "read-only")]
    pub(crate) read_only: bool,
}

#[derive(Debug, Args)]
pub(crate) struct InitArgs {
    #[arg(long)]
    pub(crate) greenfield: bool,
    #[arg(long, value_enum, default_value_t = InitMode::Solo)]
    pub(crate) mode: InitMode,
    #[arg(
        long = "site",
        alias = "project",
        help = "Derrick project name written to site.name"
    )]
    pub(crate) site: Option<String>,
    #[arg(long)]
    pub(crate) prefix: Option<String>,
    #[arg(long)]
    pub(crate) force: bool,
    #[arg(long)]
    pub(crate) yes: bool,
    #[arg(long, conflicts_with_all = ["no_wizard", "yes"])]
    pub(crate) wizard: bool,
    #[arg(long, conflicts_with = "wizard")]
    pub(crate) no_wizard: bool,
    #[arg(long, conflicts_with = "wizard")]
    pub(crate) dry_run: bool,
    #[arg(long)]
    pub(crate) no_hooks: bool,
    #[arg(long)]
    pub(crate) append_agents_md: bool,
    #[arg(long, conflicts_with = "constitution_from_docs")]
    pub(crate) constitution_stub: bool,
    #[arg(long, conflicts_with = "constitution_stub")]
    pub(crate) constitution_from_docs: bool,
    /// Write VS Code task definitions to `.vscode/tasks.json` (opt-in).
    #[arg(long)]
    pub(crate) vscode: bool,
    /// Write JetBrains run configurations to `.idea/runConfigurations/` (opt-in).
    #[arg(long)]
    pub(crate) jetbrains: bool,
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
pub(crate) struct ModelsArgs {
    #[command(subcommand)]
    pub(crate) command: ModelsCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ModelsCommand {
    /// Validate configured models and role bindings against the host catalogue.
    Check(ModelsCheckArgs),
}

#[derive(Debug, Args)]
pub(crate) struct ModelsCheckArgs {
    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    pub(crate) format: OutputFormat,
    /// Probe API/local runtime endpoints for reachability (network access).
    #[arg(long, default_value_t = false)]
    pub(crate) probe: bool,
}

#[derive(Debug, Args)]
pub(crate) struct RunArgs {
    #[command(subcommand)]
    pub(crate) command: RunCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum RunCommand {
    Drill(DrillRunArgs),
    /// Resume the latest incomplete or failed run.
    Resume(ResumeArgs),
}

#[derive(Debug, Args)]
pub(crate) struct DrillRunArgs {
    #[arg(long)]
    pub(crate) prompt: Option<String>,
    #[arg(
        long = "prompt-file",
        value_name = "PATH",
        help = "Read the feature prompt from a file (use - for stdin)"
    )]
    pub(crate) prompt_file: Option<String>,
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
    #[arg(long, help = "Alias for `--skip assay`")]
    pub(crate) no_assay: bool,
    #[arg(long, help = "Skip the GitHub Issues creation offer")]
    pub(crate) no_github_issues: bool,
    /// Internal routing flag set by `drill.rs` when it detects an incomplete run
    /// with a matching prompt key. Not exposed as a CLI flag.
    #[arg(skip)]
    pub(crate) auto_resume: bool,
    /// When `auto_resume` is true and the prior run was abandoned via
    /// `--force`, this carries the old run_id for `resume_of` lineage.
    #[arg(skip)]
    pub(crate) force_prior_run_id: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct ResumeArgs {
    /// Specific run to resume; defaults to the latest incomplete or failed run.
    #[arg(long = "run")]
    pub(crate) run_id: Option<String>,
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
    /// Reject a ticket.
    Reject(TicketRejectArgs),
    /// Reopen a Blocked ticket back to Ready.
    Reopen(TicketReopenArgs),
    /// Block a ticket on a predecessor and/or with a human note.
    Block(TicketBlockArgs),
    /// Adversarial pre-PR code review.
    CodeReview(TicketCodeReviewArgs),
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
pub(crate) struct TicketCodeReviewArgs {
    /// Ticket identifier.
    pub(crate) id: String,
    /// Branch to review (the hand's work branch).
    #[arg(long)]
    pub(crate) branch: String,
    /// Remediation round number (0-indexed; reported in output and filenames).
    #[arg(long, default_value = "0")]
    pub(crate) round: u32,
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

// ---------- Stack subcommand group (T014) --------------------------------

#[derive(Debug, Args)]
pub(crate) struct StackArgs {
    #[command(subcommand)]
    pub(crate) command: StackCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum StackCommand {
    /// Show the current stack: tickets, branches, PRs, restack health.
    Show,
    /// Restack open dependent branches.
    Restack(StackRestackArgs),
    /// Open PRs for InReview tickets that have no PR yet.
    Submit(StackSubmitArgs),
}

#[derive(Debug, Args)]
pub(crate) struct StackRestackArgs {
    /// Only restack tickets in this batch.
    #[arg(long)]
    pub(crate) batch: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct StackSubmitArgs {
    /// Only submit tickets in this batch.
    #[arg(long)]
    pub(crate) batch: Option<String>,
}

/// Arguments for `derrick switch`.
#[derive(Debug, Args)]
pub(crate) struct SwitchArgs {
    /// Target mode to switch to (solo, copilot, crew).
    #[arg(long, value_enum)]
    pub(crate) mode: InitMode,
    /// Override the in-flight run guard (dangerous).
    #[arg(long)]
    pub(crate) force: bool,
    /// Preview changes without writing derrick.yaml.
    #[arg(long)]
    pub(crate) dry_run: bool,
}

// ---------- Survey subcommand group (D54/D55) ----------------------------

#[derive(Debug, Args)]
pub(crate) struct SurveyArgs {
    #[command(subcommand)]
    pub(crate) command: SurveyCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum SurveyCommand {
    /// (Re)build the index from the working tree.
    Build(SurveyBuildArgs),
    /// Full-text search over symbol names and signatures.
    Search(SurveyQueryArgs),
    /// Show entry-point symbols for a query plus what they reference.
    Context(SurveyQueryArgs),
    /// Show direct callers and callees of a symbol.
    Impact(SurveyImpactArgs),
    /// Index freshness and size summary.
    Status(SurveyStatusArgs),
    /// Run the MCP server over stdio (what coding-agent hosts launch).
    Serve(SurveyServeArgs),
    /// Wire the survey MCP server into this repo without running `derrick init`.
    ///
    /// Creates `.derrick/` (with a `.gitignore` for the index DB) and merges
    /// the `derrick-survey` stdio server into `.mcp.json`. Safe to run on any
    /// git repo — does not require a `derrick.yaml` or a substrate database.
    Setup(SurveySetupArgs),
}

#[derive(Debug, Args)]
pub(crate) struct SurveyBuildArgs {
    /// Reparse every file, ignoring unchanged content hashes.
    #[arg(long)]
    pub(crate) full: bool,
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    pub(crate) format: OutputFormat,
}

#[derive(Debug, Args)]
pub(crate) struct SurveyQueryArgs {
    /// Search terms.
    pub(crate) query: String,
    /// Maximum number of entry-point hits.
    #[arg(long, default_value_t = 20)]
    pub(crate) limit: usize,
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    pub(crate) format: OutputFormat,
}

#[derive(Debug, Args)]
pub(crate) struct SurveyImpactArgs {
    /// Symbol name to resolve.
    pub(crate) symbol: String,
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    pub(crate) format: OutputFormat,
}

#[derive(Debug, Args)]
pub(crate) struct SurveyStatusArgs {
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    pub(crate) format: OutputFormat,
}

#[derive(Debug, Args)]
pub(crate) struct SurveyServeArgs {
    /// Serve over the Model Context Protocol on stdio. Accepted for forward
    /// compatibility; stdio MCP is currently the only transport.
    #[arg(long, default_value_t = true)]
    pub(crate) mcp: bool,
}

#[derive(Debug, Args)]
pub(crate) struct SurveySetupArgs {}

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
