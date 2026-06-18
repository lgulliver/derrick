use std::collections::BTreeSet;
use std::io::{IsTerminal, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use derrick_config::{Config, PipelineStep, Runner as StepRunner};
use derrick_memory::{Lesson, MemoryPaths, MemoryStore};
use derrick_substrate::Substrate;
use derrick_tools::HostRegistry;
use owo_colors::OwoColorize;
use tokio::process::Command;
use tokio::sync::Semaphore;

use crate::manifest::{FlagsManifest, ManifestStep, RunManifest, prior_feature_dir};
use crate::progress::{NoopReporter, ProgressReporter, RunProgress, StepProgress};
use crate::steps;
use derrick_assay::ExecutionState;
use derrick_assay::io::{
    config_hash, create_dir_all, default_run_id, read_dir_names, read_feature_dir,
};
use derrick_assay::names::runner_name;
use derrick_assay::template::{validate_rounds_template, validate_template};
use derrick_assay::types::{
    PipelineInput, RunError, RunOutcome, RunStatus, StepRecord, StepStatus,
};

const DRILL_PIPELINE: &str = "drill";
/// Pipeline id written by derrick before the `add`→`drill` rename. Accepted on
/// resume so pre-rename `.derrick/runs/*/manifest.json` still load.
const LEGACY_DRILL_PIPELINE: &str = "add-feature";

/// Maximum accepted length of a run id. Generated ids are 16 chars
/// (`%Y%m%dT%H%M%SZ`); this leaves generous room for any future format
/// while bounding attacker-supplied input.
const MAX_RUN_ID_LEN: usize = 128;

/// Validate that a run id is a single safe path component before it is used
/// to build any filesystem path (run dir, manifest, worktree, resume scan).
///
/// A run id must match `[A-Za-z0-9][A-Za-z0-9._-]*`: it begins with an
/// alphanumeric (so no leading `.` or `-`) and otherwise contains only
/// alphanumerics, `.`, `_`, and `-`. This rejects path separators (`/`,
/// `\`), `..` traversal, absolute paths, and empty ids. Generated ids of the
/// form `20250101T000000Z` pass.
fn validate_run_id(run_id: &str) -> Result<(), RunError> {
    let invalid = |reason: &str| {
        RunError::Config(format!(
            "invalid run id `{run_id}`: {reason}; run ids must match [A-Za-z0-9][A-Za-z0-9._-]* (a single path component, e.g. 20250101T000000Z)"
        ))
    };

    if run_id.is_empty() {
        return Err(invalid("run id is empty"));
    }
    if run_id.len() > MAX_RUN_ID_LEN {
        return Err(invalid("run id is too long"));
    }
    let mut chars = run_id.chars();
    let first = chars.next().expect("non-empty checked above");
    if !first.is_ascii_alphanumeric() {
        return Err(invalid("must start with a letter or digit"));
    }
    for ch in run_id.chars() {
        if !(ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' || ch == '-') {
            return Err(invalid(
                "contains a disallowed character (path separators are not allowed)",
            ));
        }
    }
    Ok(())
}

/// Executes derrick pipelines against a repository.
///
/// Clone is O(1) — all shared state is behind `Arc`. Clones share the
/// same config, substrate connection, and host registry.
#[derive(Clone)]
pub struct Runner {
    config: Arc<Config>,
    substrate: Arc<dyn Substrate>,
    hosts: Arc<HostRegistry>,
    repo_root: PathBuf,
    reporter: Arc<dyn ProgressReporter>,
}

impl Runner {
    /// Builds a runner from already-loaded configuration and process adapters.
    ///
    /// Progress reporting defaults to [`NoopReporter`]; call
    /// [`Runner::with_progress`] to attach a live front-end.
    pub fn new(
        config: Config,
        substrate: Arc<dyn Substrate>,
        hosts: HostRegistry,
        repo_root: PathBuf,
    ) -> Self {
        Self {
            config: Arc::new(config),
            substrate,
            hosts: Arc::new(hosts),
            repo_root,
            reporter: Arc::new(NoopReporter),
        }
    }

    /// Attaches a progress reporter that receives live step-lifecycle callbacks.
    /// The CLI uses this to render a spinner and per-step outcomes; tests and
    /// non-interactive callers leave the default [`NoopReporter`] in place.
    #[must_use]
    pub fn with_progress(mut self, reporter: Arc<dyn ProgressReporter>) -> Self {
        self.reporter = reporter;
        self
    }

    /// Builds a per-line output sink that forwards a step's live agent output to
    /// the progress reporter (run-feedback Layer 2). The closure captures the
    /// step id so the reporter can route lines to the right spinner.
    fn output_sink_for(&self, step_id: &str) -> Option<derrick_tools::OutputSink> {
        let reporter = Arc::clone(&self.reporter);
        let step_id = step_id.to_owned();
        Some(derrick_tools::OutputSink::new(move |_source, line| {
            reporter.step_output(&step_id, line);
        }))
    }

    /// Execute the named pipeline.
    pub async fn run_pipeline(
        &self,
        pipeline_id: &str,
        input: PipelineInput,
    ) -> Result<RunOutcome, RunError> {
        self.run_pipeline_from(pipeline_id, input, None, None).await
    }

    /// Like [`run_pipeline`] but records `resume_of` in the manifest so the
    /// run is traceable back to a prior incomplete run that was force-restarted.
    pub async fn run_pipeline_as_restart(
        &self,
        pipeline_id: &str,
        input: PipelineInput,
        resume_of: String,
    ) -> Result<RunOutcome, RunError> {
        self.run_pipeline_from(pipeline_id, input, None, Some(resume_of))
            .await
    }

    /// Resume a previous run from the given step, or auto-detect if `from_step` is `None`.
    ///
    /// When `from_step` is `None`, the resume step is determined from the manifest:
    /// - If the last completed step Failed or Halted, retry that step.
    /// - If the last completed step was Success or Skipped, resume from the next step.
    pub async fn resume(
        &self,
        run_id: Option<&str>,
        from_step: Option<&str>,
    ) -> Result<RunOutcome, RunError> {
        self.validate_pipeline_id(DRILL_PIPELINE)?;
        self.validate_config()?;

        let run_id = match run_id {
            Some(run_id) => run_id.to_owned(),
            None => self.latest_run_id()?,
        };
        // Reject path-traversal / separator-laden ids before any filesystem
        // path is built from them.
        validate_run_id(&run_id)?;
        let manifest_path = self.manifest_path(&run_id);
        let manifest = crate::manifest::read_manifest(&manifest_path)?;
        let current_hash = self.config_hash()?;
        if manifest.config_hash != current_hash {
            return Err(RunError::Config(format!(
                "config has changed since this run started (manifest hash {}, current {}); start a fresh run instead",
                manifest.config_hash, current_hash
            )));
        }
        let from_index = match from_step {
            Some(step) => self.step_index(step)?,
            None => manifest.resume_step_index(),
        };
        let mut input = PipelineInput {
            prompt: Some(manifest.prompt),
            skip: manifest.flags.skip.into_iter().collect(),
            unskip: manifest.flags.unskip.into_iter().collect(),
            dry_run: manifest.flags.dry_run,
            run_id: Some(run_id.clone()),
            no_github_issues: false,
        };
        if input.prompt.as_deref().is_some_and(str::is_empty) {
            input.prompt = None;
        }
        let prior = manifest.steps.into_iter().take(from_index).collect();
        self.run_pipeline_from(DRILL_PIPELINE, input, Some(prior), Some(run_id.clone()))
            .await
    }

    async fn run_pipeline_from(
        &self,
        pipeline_id: &str,
        input: PipelineInput,
        prior_steps: Option<Vec<ManifestStep>>,
        resume_of: Option<String>,
    ) -> Result<RunOutcome, RunError> {
        self.validate_pipeline_id(pipeline_id)?;
        let prompt = input
            .prompt
            .clone()
            .ok_or_else(|| RunError::MissingPrompt(pipeline_id.to_owned()))?;
        self.validate_config()?;
        self.validate_skip_flags(&input)?;
        let _site = self.substrate.site().await?;

        let run_id = input.run_id.clone().unwrap_or_else(default_run_id);
        // Universal chokepoint: every run (fresh or resumed) flows through
        // here before run_dir / manifest_path / worktree_path build any
        // filesystem path. Generated ids always pass; CLI-supplied ones are
        // validated against traversal.
        validate_run_id(&run_id)?;
        let run_dir = self.run_dir(&run_id);
        let config_hash = self.config_hash()?;
        let started_at = Utc::now();
        let mut state = ExecutionState::new(prompt, run_id.clone(), run_dir.clone());
        let mut manifest = RunManifest::new(
            run_id.clone(),
            pipeline_id.to_owned(),
            state.prompt.clone(),
            FlagsManifest::from_input(&input),
            config_hash,
            started_at,
        );

        if let Some(prior_steps) = prior_steps {
            state.feature_dir =
                prior_feature_dir(&prior_steps).or_else(|| read_feature_dir(&self.repo_root).ok());
            manifest.feature_dir = state.feature_dir.clone();
            for prior in &prior_steps {
                manifest.tokens_in = manifest
                    .tokens_in
                    .saturating_add(u64::from(prior.tokens_in));
                manifest.tokens_out = manifest
                    .tokens_out
                    .saturating_add(u64::from(prior.tokens_out));
            }
            manifest.steps = prior_steps;
        }
        manifest.resume_of = resume_of;

        create_dir_all(&run_dir)?;
        crate::manifest::write_manifest(&self.manifest_path(&run_id), &manifest)?;

        let worktree_branch = format!("derrick/{run_id}");
        if let Err(err) = self
            .setup_worktree(&run_id, &worktree_branch, &mut state)
            .await
        {
            tracing::warn!(
                run_id = %run_id,
                error = %err,
                "worktree setup failed; continuing in repo root"
            );
        }

        let start_index = manifest.steps.len();
        let mut outcome_status = RunStatus::Success;
        let run_timer = std::time::Instant::now();
        // Constitution is a repo-wide guardrail; always check (and write) it
        // against the main working tree, not a per-run worktree.
        steps::ensure_constitution(&self.config, &self.repo_root, self.hosts.clone()).await?;

        let tail = &self.config.pipeline()[start_index..];
        let total_steps = tail.len();
        self.reporter
            .pipeline_started(pipeline_id, &run_id, total_steps);
        let mut idx = 0usize;
        'outer: while idx < tail.len() {
            let step = &tail[idx];
            match step.parallel_group() {
                None => {
                    if self.should_skip(step, &input) {
                        let record = self.skipped_record(step);
                        self.reporter.step_finished(StepProgress {
                            step_id: step.id(),
                            status: record.status,
                            tokens_in: record.tokens_in,
                            tokens_out: record.tokens_out,
                            elapsed: std::time::Duration::ZERO,
                        });
                        manifest.tokens_in = manifest
                            .tokens_in
                            .saturating_add(u64::from(record.tokens_in));
                        manifest.tokens_out = manifest
                            .tokens_out
                            .saturating_add(u64::from(record.tokens_out));
                        manifest.steps.push(ManifestStep::from_record(&record));
                        crate::manifest::write_manifest(&self.manifest_path(&run_id), &manifest)?;
                        let _ = self
                            .substrate
                            .record_typed_event(
                                derrick_substrate::EventScope::Worktree {
                                    run_id: run_id.clone(),
                                },
                                derrick_substrate::EventKind::PipelineStepCompleted {
                                    step_id: step.id().to_owned(),
                                    status: "skipped".to_owned(),
                                },
                            )
                            .await;
                        idx += 1;
                        continue;
                    }

                    let interactive = is_interactive_step(step);
                    self.reporter
                        .step_started(step.id(), idx + 1, total_steps, interactive);
                    // D77: mirror the live step_started reporter callback into
                    // the persisted event log so `derrick observe` sees mid-step
                    // liveness without polling the launching process. Scoped to
                    // the run's worktree, mirroring PipelineStepCompleted below.
                    let _ = self
                        .substrate
                        .record_typed_event(
                            derrick_substrate::EventScope::Worktree {
                                run_id: run_id.clone(),
                            },
                            derrick_substrate::EventKind::PipelineStepStarted {
                                step_id: step.id().to_owned(),
                                index: u32::try_from(idx).unwrap_or(u32::MAX),
                                total: u32::try_from(total_steps).unwrap_or(u32::MAX),
                            },
                        )
                        .await;
                    // Interactive steps own the terminal (stdin prompts), so do
                    // not stream their output over the prompt.
                    let sink = if interactive {
                        None
                    } else {
                        self.output_sink_for(step.id())
                    };
                    let step_timer = std::time::Instant::now();
                    let record = steps::execute_step(
                        &self.config,
                        self.substrate.as_ref(),
                        self.hosts.clone(),
                        &self.repo_root,
                        step,
                        &mut state,
                        &run_id,
                        &self.manifest_path(&run_id),
                        sink,
                    )
                    .await?;
                    self.reporter.step_finished(StepProgress {
                        step_id: step.id(),
                        status: record.status,
                        tokens_in: record.tokens_in,
                        tokens_out: record.tokens_out,
                        elapsed: step_timer.elapsed(),
                    });

                    // Halted assay: surface the verdict detail beneath the outcome.
                    if record.status == StepStatus::Halted && step.id() == "assay" {
                        if let Some(feature_dir) = &state.feature_dir {
                            let wd = self.working_dir(&state);
                            let verdict_path =
                                wd.join(feature_dir).join("assay").join("verdict.md");
                            if let Ok(content) = std::fs::read_to_string(&verdict_path) {
                                let verdict = content
                                    .lines()
                                    .find_map(|l| l.strip_prefix("verdict: "))
                                    .unwrap_or("unknown");
                                let lines: Vec<&str> = content
                                    .lines()
                                    .skip_while(|l| !l.starts_with("## "))
                                    .filter(|l| !l.is_empty())
                                    .collect();
                                eprintln!(
                                    "  {} {} {}",
                                    "\u{2502}".bright_cyan(),
                                    "Verdict:".cyan(),
                                    verdict.yellow()
                                );
                                let preview: Vec<&str> = lines
                                    .iter()
                                    .take_while(|l| !l.starts_with("## Verdict"))
                                    .flat_map(|l| l.strip_prefix("**"))
                                    .filter(|l| l.len() > 5)
                                    .take(3)
                                    .collect();
                                for item in &preview {
                                    let cleaned = item.trim_end_matches("**").trim();
                                    if !cleaned.is_empty() {
                                        eprintln!(
                                            "  {} {} {}",
                                            "\u{2502}".bright_cyan(),
                                            "\u{2022}".yellow(),
                                            cleaned.yellow()
                                        );
                                    }
                                }
                                eprintln!(
                                    "  {} {} {}",
                                    "\u{2514}".bright_cyan(),
                                    "Review:".cyan(),
                                    verdict_path.display().to_string().cyan()
                                );
                            }
                        }
                    }
                    manifest.feature_dir = state.feature_dir.clone();
                    manifest.tokens_in = manifest
                        .tokens_in
                        .saturating_add(u64::from(record.tokens_in));
                    manifest.tokens_out = manifest
                        .tokens_out
                        .saturating_add(u64::from(record.tokens_out));
                    manifest.steps.push(ManifestStep::from_record(&record));
                    crate::manifest::write_manifest(&self.manifest_path(&run_id), &manifest)?;

                    if record.status == StepStatus::Success {
                        if let Some(feature_dir) = &state.feature_dir {
                            let wd = self.working_dir(&state);
                            if step.id() == "plan" {
                                let p = wd.join(feature_dir).join("plan.md");
                                if p.exists() {
                                    if let Ok(c) = std::fs::read_to_string(&p) {
                                        let line_count = c.lines().count();
                                        let lines: Vec<&str> = c
                                            .lines()
                                            .filter(|l| !l.is_empty() && !l.starts_with('#'))
                                            .collect();
                                        eprintln!();
                                        eprintln!(
                                            "  {} {}",
                                            "\u{250c}".bright_cyan(),
                                            "Plan Summary".bold()
                                        );
                                        if let Some(first) = lines.first() {
                                            let preview = summarize_line(first, 80);
                                            eprintln!(
                                                "  {} {}",
                                                "\u{2502}".bright_cyan(),
                                                preview.bright_white()
                                            );
                                        }
                                        eprintln!(
                                            "  {} {} {} {}",
                                            "\u{2514}".bright_cyan(),
                                            format!("{line_count} lines").cyan(),
                                            "\u{2192}".cyan(),
                                            p.display().to_string().cyan()
                                        );
                                        eprintln!();
                                    }
                                }
                            } else if step.id() == "analyze" {
                                let p = wd.join(feature_dir).join("analyze.md");
                                if p.exists() {
                                    if let Ok(c) = std::fs::read_to_string(&p) {
                                        let line_count = c.lines().count();
                                        let lines: Vec<&str> = c
                                            .lines()
                                            .filter(|l| !l.is_empty() && !l.starts_with('#'))
                                            .collect();
                                        eprintln!();
                                        eprintln!(
                                            "  {} {}",
                                            "\u{250c}".bright_cyan(),
                                            "Analysis".bold()
                                        );
                                        if let Some(first) = lines.first() {
                                            let preview = summarize_line(first, 80);
                                            eprintln!(
                                                "  {} {}",
                                                "\u{2502}".bright_cyan(),
                                                preview.bright_white()
                                            );
                                        }
                                        eprintln!(
                                            "  {} {} {} {}",
                                            "\u{2514}".bright_cyan(),
                                            format!("{line_count} lines").cyan(),
                                            "\u{2192}".cyan(),
                                            p.display().to_string().cyan()
                                        );
                                        eprintln!();
                                    }
                                }
                            } else if step.id() == "tasks" {
                                let p = wd.join(feature_dir).join("tasks.md");
                                if p.exists() {
                                    if let Ok(c) = std::fs::read_to_string(&p) {
                                        let line_count = c.lines().count();
                                        let tasks: Vec<String> = c
                                            .lines()
                                            .filter(|l| l.trim().starts_with("## "))
                                            .map(|l| l.trim_start_matches('#').trim().to_owned())
                                            .collect();
                                        let task_count = if tasks.is_empty() {
                                            c.lines()
                                                .filter(|l| l.trim().starts_with("- ["))
                                                .count()
                                        } else {
                                            tasks.len()
                                        };
                                        eprintln!();
                                        eprintln!(
                                            "  {} {}",
                                            "\u{250c}".bright_cyan(),
                                            "Task Plan".bold()
                                        );
                                        eprintln!(
                                            "  {} {} {} {}",
                                            "\u{2502}".bright_cyan(),
                                            format!("{task_count} tasks").cyan(),
                                            format!("({line_count} lines)").bright_black(),
                                            "\u{2192}".cyan(),
                                        );
                                        eprintln!(
                                            "  {} {}",
                                            "\u{2514}".bright_cyan(),
                                            p.display().to_string().cyan()
                                        );
                                        eprintln!();

                                        // GitHub Issues offer
                                        if !input.no_github_issues
                                            && std::io::stdin().is_terminal()
                                            && !tasks.is_empty()
                                            && which::which("gh").is_ok()
                                            && has_github_remote(&self.repo_root)
                                        {
                                            eprintln!(
                                                "  {} Would you like these tasks created as GitHub Issues? {}",
                                                "\u{276f}".cyan(),
                                                format!("({task_count} issues)").bright_black()
                                            );
                                            eprint!("  {} [Y/n] ", "\u{276f}".cyan());
                                            std::io::stderr().flush().ok();
                                            let mut answer = String::new();
                                            if std::io::stdin().read_line(&mut answer).is_ok() {
                                                let trimmed = answer.trim();
                                                if trimmed.is_empty()
                                                    || trimmed.eq_ignore_ascii_case("y")
                                                    || trimmed.eq_ignore_ascii_case("yes")
                                                {
                                                    create_github_issues(&tasks, &self.repo_root)
                                                        .await;
                                                } else {
                                                    eprintln!(
                                                        "  {} {}",
                                                        "\u{2502}".bright_cyan(),
                                                        "Skipping GitHub Issues — tasks saved to tasks.md."
                                                            .bright_black()
                                                    );
                                                }
                                                eprintln!();
                                            }
                                        }
                                    }
                                }
                            } else if step.id() == "specify" {
                                let p = wd.join(feature_dir).join("spec.md");
                                if p.exists() {
                                    eprintln!(
                                        "  {} Specification generated: {}",
                                        "\u{2713}".green(),
                                        p.display().to_string().cyan()
                                    );
                                    // Confirmation gate — only in interactive mode
                                    if std::io::stdin().is_terminal() {
                                        if let Ok(content) = std::fs::read_to_string(&p) {
                                            let preview_lines: Vec<&str> = content
                                                .lines()
                                                .filter(|l| !l.trim().is_empty())
                                                .take(6)
                                                .collect();
                                            eprintln!();
                                            eprintln!(
                                                "  {} {}",
                                                "\u{250c}".bright_cyan(),
                                                "Here is what I understood:".bold()
                                            );
                                            for line in &preview_lines {
                                                eprintln!(
                                                    "  {} {}",
                                                    "\u{2502}".bright_cyan(),
                                                    line.bright_white()
                                                );
                                            }
                                            if content
                                                .lines()
                                                .filter(|l| !l.trim().is_empty())
                                                .count()
                                                > 6
                                            {
                                                eprintln!(
                                                    "  {} {}",
                                                    "\u{2502}".bright_cyan(),
                                                    "  … (see spec.md for full detail)"
                                                        .bright_black()
                                                );
                                            }
                                            eprintln!(
                                                "  {} {}",
                                                "\u{2514}".bright_cyan(),
                                                "Is this correct?".bold()
                                            );
                                            eprint!("  {} Continue? [Y/n] ", "\u{276f}".cyan());
                                            std::io::stderr().flush().ok();
                                            let mut answer = String::new();
                                            std::io::stdin().read_line(&mut answer).map_err(
                                                |source| RunError::Io {
                                                    path: PathBuf::from("<stdin>"),
                                                    source,
                                                },
                                            )?;
                                            let trimmed = answer.trim();
                                            if trimmed.eq_ignore_ascii_case("n")
                                                || trimmed.eq_ignore_ascii_case("no")
                                            {
                                                eprint!(
                                                    "  {} What should I correct? ",
                                                    "\u{276f}".cyan()
                                                );
                                                std::io::stderr().flush().ok();
                                                let mut correction = String::new();
                                                std::io::stdin()
                                                    .read_line(&mut correction)
                                                    .map_err(|source| RunError::Io {
                                                        path: PathBuf::from("<stdin>"),
                                                        source,
                                                    })?;
                                                let correction = correction.trim().to_owned();
                                                eprintln!();
                                                if correction.is_empty() {
                                                    eprintln!(
                                                        "  {} {}",
                                                        "\u{26a0}".yellow(),
                                                        "Pipeline halted at specify gate.".yellow()
                                                    );
                                                    eprintln!(
                                                        "  {} Re-run with a refined prompt to try again.",
                                                        "\u{2502}".bright_cyan()
                                                    );
                                                } else {
                                                    eprintln!(
                                                        "  {} {}",
                                                        "\u{26a0}".yellow(),
                                                        "Pipeline halted at specify gate.".yellow()
                                                    );
                                                    eprintln!(
                                                        "  {} Re-run with:",
                                                        "\u{2502}".bright_cyan()
                                                    );
                                                    eprintln!(
                                                        "  {}   derrick drill \"{}. {}\"",
                                                        "\u{2514}".bright_cyan(),
                                                        state.prompt.replace('"', "\\\""),
                                                        correction.replace('"', "\\\"")
                                                    );
                                                }
                                                outcome_status = RunStatus::Halted;
                                                break 'outer;
                                            }
                                            eprintln!();
                                        }
                                    }
                                }
                            } else if step.id() == "assay"
                                && !self.config.tools().assay().auto_execute()
                                && std::io::stdin().is_terminal()
                            {
                                let assay_dir = wd.join(feature_dir).join("assay");
                                let verdict_path = assay_dir.join("verdict.md");
                                let rounds_used = std::fs::read_to_string(&verdict_path)
                                    .ok()
                                    .as_deref()
                                    .and_then(parse_rounds_used)
                                    .unwrap_or(0);
                                let transcript_paths = assay_transcript_paths(&assay_dir);
                                eprintln!();
                                eprintln!(
                                    "  {} {}",
                                    "\u{250c}".bright_cyan(),
                                    "Verdict Gate".bold()
                                );
                                eprintln!(
                                    "  {} {} {} {}",
                                    "\u{2502}".bright_cyan(),
                                    "Plan approved after".cyan(),
                                    format!("{rounds_used} rounds").cyan(),
                                    "by cross-model deliberation.".cyan()
                                );
                                eprintln!(
                                    "  {} {}",
                                    "\u{2502}".bright_cyan(),
                                    if transcript_paths.len() == 1 {
                                        "Review transcript:".cyan()
                                    } else {
                                        "Review transcripts:".cyan()
                                    }
                                );
                                if transcript_paths.is_empty() {
                                    eprintln!(
                                        "  {} {}",
                                        "\u{2502}".bright_cyan(),
                                        assay_dir.join("debate.md").display().to_string().cyan()
                                    );
                                } else {
                                    for transcript_path in transcript_paths {
                                        eprintln!(
                                            "  {} {}",
                                            "\u{2502}".bright_cyan(),
                                            transcript_path.display().to_string().cyan()
                                        );
                                    }
                                }
                                eprintln!(
                                    "  {} {}",
                                    "\u{2514}".bright_cyan(),
                                    "Proceed to implementation?".bold()
                                );
                                eprint!("  {} Continue pipeline? [Y/n] ", "\u{276f}".cyan());
                                std::io::stderr().flush().ok();
                                let mut answer = String::new();
                                std::io::stdin().read_line(&mut answer).map_err(|source| {
                                    RunError::Io {
                                        path: PathBuf::from("<stdin>"),
                                        source,
                                    }
                                })?;
                                let trimmed = answer.trim();
                                if trimmed.eq_ignore_ascii_case("n")
                                    || trimmed.eq_ignore_ascii_case("no")
                                {
                                    eprintln!(
                                        "  {} {}",
                                        "\u{26a0}".yellow(),
                                        "Pipeline halted at verdict gate.".yellow()
                                    );
                                    outcome_status = RunStatus::Halted;
                                    break 'outer;
                                }
                                eprintln!();
                            } else if step.id() == "assay" {
                                eprintln!(
                                    "  {} {}",
                                    "\u{2502}".bright_cyan(),
                                    "Assay complete.".cyan()
                                );
                            }
                        }
                    }

                    match record.status {
                        StepStatus::Success | StepStatus::Skipped => {}
                        StepStatus::Halted => {
                            outcome_status = RunStatus::Halted;
                            break 'outer;
                        }
                        StepStatus::Failed => {
                            outcome_status = RunStatus::Failed;
                            break 'outer;
                        }
                    }

                    if input.dry_run && step.id() == "tasks" {
                        outcome_status = RunStatus::Halted;
                        break 'outer;
                    }
                    // Foreman detach hint — printed after the foreman step completes
                    if step.id() == "foreman" && std::io::stdin().is_terminal() {
                        eprintln!();
                        eprintln!(
                            "  {} {}",
                            "\u{276f}".cyan(),
                            "Work is continuing in the background.".bold()
                        );
                        eprintln!(
                            "  {} Run {} to re-attach or {} for a snapshot.",
                            "\u{2502}".bright_cyan(),
                            "derrick observe".cyan(),
                            "derrick status".cyan()
                        );
                        eprintln!();
                    }
                    idx += 1;
                }
                Some(group_name) => {
                    let group_name = group_name.to_owned();
                    let mut group_end = idx;
                    while group_end < tail.len()
                        && tail[group_end].parallel_group() == Some(group_name.as_str())
                    {
                        group_end += 1;
                    }

                    let group_steps: Vec<PipelineStep> = tail[idx..group_end].to_vec();
                    let step_max = self.config.parallelism().step_max().max(1) as usize;
                    let semaphore = Arc::new(Semaphore::new(step_max));
                    let mut handles: Vec<tokio::task::JoinHandle<Result<StepRecord, RunError>>> =
                        Vec::new();
                    let mut skipped: Vec<StepRecord> = Vec::new();

                    eprintln!(
                        "  {} {} ({} steps)...",
                        "parallel group".bright_cyan(),
                        group_name.cyan(),
                        group_steps.len()
                    );
                    for step in &group_steps {
                        if self.should_skip(step, &input) {
                            let record = self.skipped_record(step);
                            self.reporter.step_finished(StepProgress {
                                step_id: step.id(),
                                status: record.status,
                                tokens_in: record.tokens_in,
                                tokens_out: record.tokens_out,
                                elapsed: std::time::Duration::ZERO,
                            });
                            let _ = self
                                .substrate
                                .record_typed_event(
                                    derrick_substrate::EventScope::Worktree {
                                        run_id: run_id.clone(),
                                    },
                                    derrick_substrate::EventKind::PipelineStepCompleted {
                                        step_id: step.id().to_owned(),
                                        status: "skipped".to_owned(),
                                    },
                                )
                                .await;
                            skipped.push(record);
                            continue;
                        }
                        // Index/total are not meaningful within a parallel group.
                        self.reporter.step_started(step.id(), 0, 0, false);
                        // D77: mirror step_started into the persisted event log
                        // (0/0 index, matching the reporter — the step's
                        // absolute position is not meaningful inside a group).
                        let _ = self
                            .substrate
                            .record_typed_event(
                                derrick_substrate::EventScope::Worktree {
                                    run_id: run_id.clone(),
                                },
                                derrick_substrate::EventKind::PipelineStepStarted {
                                    step_id: step.id().to_owned(),
                                    index: 0,
                                    total: 0,
                                },
                            )
                        .await;
                        let sem = semaphore.clone();
                        let runner = self.clone();
                        let step = step.clone();
                        let state_clone = state.clone();
                        let run_id = run_id.clone();
                        handles.push(tokio::task::spawn(async move {
                            let _permit = sem.acquire_owned().await.map_err(|_| {
                                RunError::Config("semaphore closed unexpectedly".to_owned())
                            })?;
                            let mut st = state_clone;
                            let sink = runner.output_sink_for(step.id());
                            let step_timer = std::time::Instant::now();
                            let record = steps::execute_step(
                                &runner.config,
                                runner.substrate.as_ref(),
                                runner.hosts.clone(),
                                &runner.repo_root,
                                &step,
                                &mut st,
                                &run_id,
                                &runner.manifest_path(&run_id),
                                sink,
                            )
                            .await?;
                            runner.reporter.step_finished(StepProgress {
                                step_id: step.id(),
                                status: record.status,
                                tokens_in: record.tokens_in,
                                tokens_out: record.tokens_out,
                                elapsed: step_timer.elapsed(),
                            });
                            Ok(record)
                        }));
                    }

                    for record in &skipped {
                        manifest.tokens_in = manifest
                            .tokens_in
                            .saturating_add(u64::from(record.tokens_in));
                        manifest.tokens_out = manifest
                            .tokens_out
                            .saturating_add(u64::from(record.tokens_out));
                        manifest.steps.push(ManifestStep::from_record(record));
                        crate::manifest::write_manifest(&self.manifest_path(&run_id), &manifest)?;
                    }

                    let mut halted = false;
                    let mut failed = false;
                    for handle in handles {
                        let record = handle
                            .await
                            .map_err(|e| RunError::Config(format!("parallel task join: {e}")))?
                            .map_err(|e| {
                                tracing::error!(
                                    run_id = %run_id,
                                    group = %group_name,
                                    error = %e,
                                    "step in parallel group failed"
                                );
                                e
                            })?;
                        manifest.feature_dir = state.feature_dir.clone();
                        manifest.tokens_in = manifest
                            .tokens_in
                            .saturating_add(u64::from(record.tokens_in));
                        manifest.tokens_out = manifest
                            .tokens_out
                            .saturating_add(u64::from(record.tokens_out));
                        manifest.steps.push(ManifestStep::from_record(&record));
                        crate::manifest::write_manifest(&self.manifest_path(&run_id), &manifest)?;
                        match record.status {
                            StepStatus::Success | StepStatus::Skipped => {}
                            StepStatus::Halted => halted = true,
                            StepStatus::Failed => failed = true,
                        }
                    }
                    if halted {
                        outcome_status = RunStatus::Halted;
                        break 'outer;
                    }
                    if failed {
                        outcome_status = RunStatus::Failed;
                        break 'outer;
                    }
                    idx = group_end;
                }
            }
        }

        manifest.status = outcome_status;
        manifest.finished_at = Some(Utc::now());
        manifest.feature_dir = state.feature_dir.clone();
        crate::manifest::write_manifest(&self.manifest_path(&run_id), &manifest)?;

        match outcome_status {
            RunStatus::Success => {
                if let Some(path) = state.worktree_path.clone() {
                    self.teardown_worktree(&run_id, &path).await;
                }
                // Curation (§9.A.4 / D9): record a cross-feature lesson now
                // that the run has a known outcome. Best-effort — failures are
                // logged at WARN, not propagated.
                self.record_run_lesson(&run_id, &manifest).await;
            }
            RunStatus::Failed | RunStatus::Halted => {
                if state.worktree_path.is_some() {
                    tracing::info!(
                        run_id = %run_id,
                        status = ?outcome_status,
                        "preserving worktree for resume"
                    );
                }
            }
        }

        let tokens_in_total = manifest.tokens_in;
        let tokens_out_total = manifest.tokens_out;
        self.reporter.pipeline_finished(RunProgress {
            run_id: &run_id,
            status: outcome_status,
            tokens_in: tokens_in_total,
            tokens_out: tokens_out_total,
            elapsed: run_timer.elapsed(),
        });
        Ok(RunOutcome {
            run_id,
            status: outcome_status,
            feature_dir: state.feature_dir.map(|path| self.repo_root.join(path)),
            steps: manifest.steps.into_iter().map(StepRecord::from).collect(),
            tokens_in: tokens_in_total,
            tokens_out: tokens_out_total,
        })
    }

    fn working_dir<'a>(&'a self, state: &'a ExecutionState) -> &'a Path {
        state
            .worktree_path
            .as_deref()
            .unwrap_or(self.repo_root.as_path())
    }

    pub fn inject_clarify_answers_for_plan(
        &self,
        step_id: &str,
        state: &ExecutionState,
        prompt: String,
    ) -> Result<String, RunError> {
        steps::inject_clarify_answers_for_plan(step_id, state, &self.repo_root, prompt)
    }

    async fn setup_worktree(
        &self,
        run_id: &str,
        branch: &str,
        state: &mut ExecutionState,
    ) -> Result<(), RunError> {
        let path = self.worktree_path(run_id);

        if self.is_valid_worktree(&path) {
            tracing::info!(run_id = %run_id, "reusing existing worktree");
            state.worktree_path = Some(path);
            return Ok(());
        }

        let path = self
            .substrate
            .reserve_worktree(run_id, branch)
            .await
            .map_err(RunError::Substrate)?;

        let result = Command::new("git")
            .args(["worktree", "add", "-B", branch])
            .arg(&path)
            .arg("HEAD")
            .current_dir(&self.repo_root)
            .kill_on_drop(true)
            .output()
            .await;

        match result {
            Ok(output) if output.status.success() => {
                state.worktree_path = Some(path);
                Ok(())
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let reason = stderr.trim().to_owned();
                let _ = self
                    .substrate
                    .record_typed_event(
                        derrick_substrate::EventScope::Worktree {
                            run_id: run_id.to_owned(),
                        },
                        derrick_substrate::EventKind::WorktreeAbandoned {
                            run_id: run_id.to_owned(),
                            reason: reason.clone(),
                        },
                    )
                    .await;
                Err(RunError::Config(format!(
                    "git worktree add failed: {reason}"
                )))
            }
            Err(source) => Err(RunError::Io {
                path: path.clone(),
                source,
            }),
        }
    }

    fn is_valid_worktree(&self, path: &Path) -> bool {
        path.join(".git").exists()
    }

    fn worktree_path(&self, run_id: &str) -> PathBuf {
        self.repo_root
            .join(self.config.state().worktree_root())
            .join(run_id)
    }

    async fn teardown_worktree(&self, run_id: &str, path: &Path) {
        let _ = self.substrate.close_worktree(run_id).await;
        let _ = Command::new("git")
            .args(["worktree", "remove", "--force"])
            .arg(path)
            .current_dir(&self.repo_root)
            .kill_on_drop(true)
            .output()
            .await;
    }

    fn validate_pipeline_id(&self, pipeline_id: &str) -> Result<(), RunError> {
        match pipeline_id {
            DRILL_PIPELINE => Ok(()),
            // deprecated alias: pre-rename manifests
            LEGACY_DRILL_PIPELINE => Ok(()),
            _ => Err(RunError::UnknownPipeline(pipeline_id.to_owned())),
        }
    }

    fn validate_config(&self) -> Result<(), RunError> {
        let mut seen = BTreeSet::new();
        let mut feature_available = false;
        for step in self.config.pipeline() {
            if !seen.insert(step.id().to_owned()) {
                return Err(RunError::Config(format!(
                    "pipeline.{}: duplicate step id",
                    step.id()
                )));
            }
            if step.on_failure().is_some() {
                return Err(RunError::Config(format!(
                    "pipeline.{}: on_failure is not supported in T010; copilot dispatch failure policy is deferred to T013",
                    step.id()
                )));
            }
            if step.poll_interval().is_some() {
                return Err(RunError::Config(format!(
                    "pipeline.{}: poll_interval is not supported in T010; polling is deferred to T013",
                    step.id()
                )));
            }
            if let Some(runner) = step.runner() {
                match runner {
                    StepRunner::Claude | StepRunner::Codex | StepRunner::Copilot => {
                        return Err(RunError::Config(format!(
                            "runner: {} is not supported; use `host: {}` with a role binding instead (see DESIGN.md §4 and D30)",
                            runner_name(runner),
                            runner_name(runner)
                        )));
                    }
                    StepRunner::Derrick | StepRunner::Human | StepRunner::Bash => {}
                }
            }
            self.validate_template_field(step, step.command(), "command", feature_available)?;
            self.validate_template_field(step, step.prompt(), "prompt", feature_available)?;
            self.validate_template_field(step, step.batch(), "batch", feature_available)?;
            for input in step.inputs() {
                validate_template(input, feature_available).map_err(|error| {
                    RunError::Config(format!("pipeline.{}.inputs: {error}", step.id()))
                })?;
            }
            if let Some(rounds) = step.rounds() {
                validate_rounds_template(rounds, feature_available)?;
            }
            if step.id() == "specify" {
                feature_available = true;
            }
        }
        Ok(())
    }

    fn validate_template_field(
        &self,
        step: &PipelineStep,
        value: Option<&str>,
        field: &str,
        feature_available: bool,
    ) -> Result<(), RunError> {
        if let Some(value) = value {
            validate_template(value, feature_available).map_err(|error| {
                RunError::Config(format!("pipeline.{}.{}: {error}", step.id(), field))
            })?;
        }
        Ok(())
    }

    fn validate_skip_flags(&self, input: &PipelineInput) -> Result<(), RunError> {
        for id in input.skip.iter().chain(input.unskip.iter()) {
            let step = self
                .config
                .pipeline()
                .iter()
                .find(|step| step.id() == id)
                .ok_or_else(|| RunError::Config(format!("unknown step `{id}`")))?;
            if !step.skippable() {
                return Err(RunError::Config(format!("step `{id}` is not skippable")));
            }
        }
        Ok(())
    }

    fn should_skip(&self, step: &PipelineStep, input: &PipelineInput) -> bool {
        (step.default_skip() && !input.unskip.contains(step.id())) || input.skip.contains(step.id())
    }

    fn skipped_record(&self, step: &PipelineStep) -> StepRecord {
        let now = Utc::now();
        StepRecord {
            id: step.id().to_owned(),
            status: StepStatus::Skipped,
            started_at: now,
            finished_at: now,
            log_path: PathBuf::new(),
            artifacts: Vec::new(),
            tokens_in: 0,
            tokens_out: 0,
            bytes_raw: 0,
            bytes_saved: 0,
            roughneck_tokens_saved: 0,
        }
    }

    fn step_index(&self, step_id: &str) -> Result<usize, RunError> {
        self.config
            .pipeline()
            .iter()
            .position(|step| step.id() == step_id)
            .ok_or_else(|| RunError::Config(format!("unknown step `{step_id}`")))
    }

    fn latest_run_id(&self) -> Result<String, RunError> {
        let runs_dir = self.repo_root.join(self.config.state().dir()).join("runs");
        let mut entries = read_dir_names(&runs_dir)?;
        entries.sort();
        entries
            .pop()
            .ok_or_else(|| RunError::Config("no previous runs found".to_owned()))
    }

    fn config_hash(&self) -> Result<String, RunError> {
        config_hash(&self.repo_root.join("derrick.yaml"))
    }

    fn run_dir(&self, run_id: &str) -> PathBuf {
        self.repo_root
            .join(self.config.state().dir())
            .join("runs")
            .join(run_id)
    }

    fn manifest_path(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("manifest.json")
    }

    /// Record a cross-feature lesson after a successful run (§9.A.4 / D9).
    ///
    /// The lesson body always contains the section anchor `#9.A.4` so it passes
    /// the D9 quality gate unconditionally. Substrate ticket IDs are appended
    /// when they can be retrieved — the lesson is still written if they cannot.
    ///
    /// All errors are logged at WARN and not propagated; this is best-effort.
    async fn record_run_lesson(&self, run_id: &str, manifest: &RunManifest) {
        // Fetch ticket IDs from the most-recently-closed batch. We look at open
        // batches first (a batch may still be open when the pipeline finishes),
        // then closed ones.  Failures are non-fatal.
        let ticket_ids = match self.substrate.list_batches(true).await {
            Err(err) => {
                tracing::debug!(?err, "could not list batches for lesson curation");
                Vec::new()
            }
            Ok(batches) => {
                // Find the batch that was most recently closed (or most recently
                // created if none are closed yet). Use the batch closest to the
                // run's start time.
                let best = batches.into_iter().min_by_key(|b| {
                    let ts = b.closed_at.unwrap_or(b.created_at);
                    let delta = ts.signed_duration_since(manifest.started_at);
                    delta.num_seconds().unsigned_abs()
                });
                match best {
                    None => Vec::new(),
                    Some(batch) => match self.substrate.tickets_in_batch(&batch.name).await {
                        Err(err) => {
                            tracing::debug!(?err, "could not fetch tickets for lesson curation");
                            Vec::new()
                        }
                        Ok(tickets) => tickets
                            .into_iter()
                            .map(|t| t.id.to_string())
                            .collect::<Vec<_>>(),
                    },
                }
            }
        };

        // Compose a deterministic body. The section anchor #9.A.4 is always
        // present so the D9 gate passes even if ticket_ids is empty.
        let step_summary = manifest
            .steps
            .iter()
            .map(|s| format!("{}={:?}", s.id, s.status))
            .collect::<Vec<_>>()
            .join(", ");

        let tickets_part = if ticket_ids.is_empty() {
            String::new()
        } else {
            format!("; tickets: {}", ticket_ids.join(", "))
        };

        let body = format!(
            "Run {run_id} ({pipeline}) completed: {status:?}{tickets_part}; steps: [{step_summary}]. \
             See §#9.A.4 for curation policy.",
            run_id = run_id,
            pipeline = manifest.pipeline_id,
            status = manifest.status,
            tickets_part = tickets_part,
            step_summary = step_summary,
        );

        let lesson = Lesson {
            at: manifest.finished_at.unwrap_or_else(Utc::now),
            batch: None,
            body,
            tags: Vec::new(), // populated by append_lesson
        };

        let paths = MemoryPaths {
            host_memory_root: None,
            repo_state: self.repo_root.join(self.config.state().dir()),
        };

        match MemoryStore::open(paths, self.config.site()) {
            Err(err) => {
                tracing::warn!(
                    ?err,
                    run_id,
                    "failed to open memory store for lesson curation"
                );
            }
            Ok(store) => {
                if let Err(err) = store.append_lesson(&lesson) {
                    tracing::warn!(?err, run_id, "failed to record run lesson");
                } else {
                    tracing::debug!(run_id, "run lesson recorded");
                }
            }
        }
    }
}

fn is_interactive_step(step: &PipelineStep) -> bool {
    matches!(step.runner(), Some(StepRunner::Human))
        || (matches!(step.runner(), Some(StepRunner::Derrick))
            && (step.id() == "clarify" || step.id() == "assay"))
}

fn summarize_line(line: &str, max_chars: usize) -> String {
    let total = line.chars().count();
    if total <= max_chars {
        return line.to_owned();
    }
    let keep = max_chars.saturating_sub(3);
    let mut preview = line.chars().take(keep).collect::<String>();
    preview.push_str("...");
    preview
}

fn parse_rounds_used(verdict_text: &str) -> Option<usize> {
    verdict_text.lines().find_map(|line| {
        line.strip_prefix("round: ")
            .or_else(|| line.strip_prefix("rounds_used: "))
            .and_then(|value| value.parse::<usize>().ok())
    })
}

fn assay_transcript_paths(assay_dir: &Path) -> Vec<PathBuf> {
    let canonical = assay_dir.join("debate.md");
    if canonical.is_file() {
        return vec![canonical];
    }
    let Ok(mut reviewers) = read_dir_names(assay_dir) else {
        return Vec::new();
    };
    reviewers.sort();
    reviewers
        .into_iter()
        .map(|reviewer| assay_dir.join(reviewer).join("debate.md"))
        .filter(|path| path.is_file())
        .collect()
}

/// Check whether the repository has a GitHub remote configured.
fn has_github_remote(repo_root: &Path) -> bool {
    std::process::Command::new("git")
        .args(["remote", "-v"])
        .current_dir(repo_root)
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).contains("github.com"))
        .unwrap_or(false)
}

/// Create one GitHub Issue per task title via `gh issue create`.
async fn create_github_issues(tasks: &[String], repo_root: &Path) {
    eprintln!();
    for title in tasks {
        let result = tokio::process::Command::new("gh")
            .args([
                "issue",
                "create",
                "--title",
                title,
                "--body",
                "Created by derrick from tasks.md.",
                "--label",
                "derrick",
            ])
            .current_dir(repo_root)
            .output()
            .await;
        match result {
            Ok(out) if out.status.success() => {
                let url = String::from_utf8_lossy(&out.stdout).trim().to_owned();
                eprintln!(
                    "  {} {} {}",
                    "\u{2713}".green(),
                    "Issue created:".cyan(),
                    url.bright_white()
                );
            }
            Ok(out) => {
                let err = String::from_utf8_lossy(&out.stderr);
                eprintln!(
                    "  {} {} {}",
                    "\u{26a0}".yellow(),
                    format!("Failed to create issue for \"{title}\":").yellow(),
                    err.trim().bright_black()
                );
            }
            Err(err) => {
                eprintln!(
                    "  {} {}",
                    "\u{26a0}".yellow(),
                    format!("gh error: {err}").yellow()
                );
            }
        }
    }
    eprintln!();
}

#[cfg(test)]
mod tests {
    #[test]
    fn summarize_line_truncates_without_utf8_panics() {
        let line = "🚀🚀🚀🚀🚀";
        let preview = super::summarize_line(line, 4);
        assert_eq!(preview, "🚀...");
    }

    #[test]
    fn parse_rounds_used_supports_single_and_multi_reviewer_verdicts() {
        assert_eq!(super::parse_rounds_used("round: 3"), Some(3));
        assert_eq!(super::parse_rounds_used("rounds_used: 7"), Some(7));
    }

    #[test]
    fn validate_run_id_accepts_generated_id() {
        // Matches default_run_id()'s `%Y%m%dT%H%M%SZ` format.
        let id = derrick_assay::io::default_run_id();
        assert!(super::validate_run_id(&id).is_ok(), "generated id: {id}");
        assert!(super::validate_run_id("20250101T000000Z").is_ok());
        assert!(super::validate_run_id("run-1").is_ok());
        assert!(super::validate_run_id("a.b_c-1").is_ok());
    }

    #[test]
    fn validate_run_id_rejects_traversal_and_separators() {
        for bad in [
            "",
            "..",
            "../../x",
            "a/b",
            "a\\b",
            "/abs/path",
            ".hidden",
            "-leading-dash",
            "with space",
            "semi;colon",
        ] {
            assert!(
                super::validate_run_id(bad).is_err(),
                "expected rejection for {bad:?}"
            );
        }
    }

    #[test]
    fn validate_run_id_rejects_overlong() {
        let long = "a".repeat(super::MAX_RUN_ID_LEN + 1);
        assert!(super::validate_run_id(&long).is_err());
    }

    #[test]
    fn assay_transcript_paths_uses_canonical_when_present() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let assay_dir = tmp.path().join("assay");
        std::fs::create_dir_all(&assay_dir).expect("mkdir");
        std::fs::write(assay_dir.join("debate.md"), "log").expect("write");
        let paths = super::assay_transcript_paths(&assay_dir);
        assert_eq!(paths, vec![assay_dir.join("debate.md")]);
    }

    #[test]
    fn assay_transcript_paths_discovers_multi_reviewer_logs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let assay_dir = tmp.path().join("assay");
        std::fs::create_dir_all(assay_dir.join("claude")).expect("mkdir claude");
        std::fs::create_dir_all(assay_dir.join("codex")).expect("mkdir codex");
        std::fs::write(assay_dir.join("codex").join("debate.md"), "codex").expect("write codex");
        std::fs::write(assay_dir.join("claude").join("debate.md"), "claude").expect("write claude");

        let paths = super::assay_transcript_paths(&assay_dir);
        assert_eq!(
            paths,
            vec![
                assay_dir.join("claude").join("debate.md"),
                assay_dir.join("codex").join("debate.md")
            ]
        );
    }
}
