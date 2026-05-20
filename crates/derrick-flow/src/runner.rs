use std::collections::BTreeSet;
use std::io::{IsTerminal, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use derrick_config::{Config, PipelineStep, Runner as StepRunner};
use derrick_substrate::Substrate;
use derrick_tools::HostRegistry;
use owo_colors::OwoColorize;
use tokio::process::Command;
use tokio::sync::Semaphore;

use crate::assay::ExecutionState;
use crate::io::{
    config_hash, create_dir_all, default_run_id, prior_feature_dir, read_dir_names,
    read_feature_dir,
};
use crate::manifest::{FlagsManifest, ManifestStep, RunManifest};
use crate::names::runner_name;
use crate::steps;
use crate::template::{validate_rounds_template, validate_template};
use crate::types::{PipelineInput, RunError, RunOutcome, RunStatus, StepRecord, StepStatus};

const ADD_FEATURE_PIPELINE: &str = "add-feature";

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
}

impl Runner {
    /// Builds a runner from already-loaded configuration and process adapters.
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
        }
    }

    /// Execute the named pipeline.
    pub async fn run_pipeline(
        &self,
        pipeline_id: &str,
        input: PipelineInput,
    ) -> Result<RunOutcome, RunError> {
        self.run_pipeline_from(pipeline_id, input, None).await
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
        self.validate_pipeline_id(ADD_FEATURE_PIPELINE)?;
        self.validate_config()?;

        let run_id = match run_id {
            Some(run_id) => run_id.to_owned(),
            None => self.latest_run_id()?,
        };
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
        };
        if input.prompt.as_deref().is_some_and(str::is_empty) {
            input.prompt = None;
        }
        let prior = manifest.steps.into_iter().take(from_index).collect();
        self.run_pipeline_from(ADD_FEATURE_PIPELINE, input, Some(prior))
            .await
    }

    async fn run_pipeline_from(
        &self,
        pipeline_id: &str,
        input: PipelineInput,
        prior_steps: Option<Vec<ManifestStep>>,
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
        eprintln!(
            "{} {} ({})",
            "pipeline:".bold(),
            pipeline_id.cyan(),
            format!("run {run_id}").bright_black()
        );
        let tail = &self.config.pipeline()[start_index..];
        let mut idx = 0usize;
        'outer: while idx < tail.len() {
            let step = &tail[idx];
            match step.parallel_group() {
                None => {
                    if self.should_skip(step, &input) {
                        let record = self.skipped_record(step);
                        eprintln!(
                            "  {} {} {}",
                            step.id().cyan(),
                            "\u{23ed}".bright_cyan(),
                            "skipped".bright_black()
                        );
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

                    let record = {
                        if is_interactive_step(step) {
                            eprintln!("  {}...", step.id().cyan());
                            steps::execute_step(
                                &self.config,
                                self.substrate.as_ref(),
                                self.hosts.clone(),
                                &self.repo_root,
                                step,
                                &mut state,
                                &run_id,
                                &self.manifest_path(&run_id),
                            )
                            .await?
                        } else {
                            let step_id = step.id().to_owned();
                            let frames = crate::spinner::scanner_frames();
                            let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
                            let r2 = running.clone();
                            let spinner = tokio::task::spawn(async move {
                                use std::io::Write as _;
                                use std::sync::atomic::Ordering;
                                use std::time::Duration;
                                let mut i = 0usize;
                                while r2.load(Ordering::Relaxed) {
                                    eprint!("\r  {} {}...", step_id.cyan(), frames[i]);
                                    let _ = std::io::stderr().flush();
                                    tokio::time::sleep(Duration::from_millis(80)).await;
                                    i = (i + 1) % frames.len();
                                }
                            });

                            let result = steps::execute_step(
                                &self.config,
                                self.substrate.as_ref(),
                                self.hosts.clone(),
                                &self.repo_root,
                                step,
                                &mut state,
                                &run_id,
                                &self.manifest_path(&run_id),
                            )
                            .await;
                            running.store(false, std::sync::atomic::Ordering::Relaxed);
                            let _ = spinner.await;
                            eprint!("\r                                            \r");
                            result?
                        }
                    };
                    match record.status {
                        StepStatus::Success => eprintln!(
                            "  {} {} {}",
                            step.id().cyan(),
                            "\u{2713}".green(),
                            "done".green()
                        ),
                        StepStatus::Skipped => eprintln!(
                            "  {} {} {}",
                            step.id().cyan(),
                            "\u{23ed}".bright_cyan(),
                            "skipped".bright_black()
                        ),
                        StepStatus::Halted => eprintln!(
                            "  {} {} {}",
                            step.id().cyan(),
                            "\u{26a0}".yellow(),
                            "HALTED".yellow()
                        ),
                        StepStatus::Failed => eprintln!(
                            "  {} {} {}",
                            step.id().cyan(),
                            "\u{2717}".red(),
                            "FAILED".red()
                        ),
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
                            } else if step.id() == "specify" {
                                let p = wd.join(feature_dir).join("spec.md");
                                if p.exists() {
                                    eprintln!(
                                        "  {} Specification generated: {}",
                                        "\u{2713}".green(),
                                        p.display().to_string().cyan()
                                    );
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
                            eprintln!(
                                "    {} {} {}",
                                step.id().cyan(),
                                "\u{23ed}".bright_cyan(),
                                "skipped".bright_black()
                            );
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
                        eprintln!("    {}...", step.id().cyan());
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
                            let record = steps::execute_step(
                                &runner.config,
                                runner.substrate.as_ref(),
                                runner.hosts.clone(),
                                &runner.repo_root,
                                &step,
                                &mut st,
                                &run_id,
                                &runner.manifest_path(&run_id),
                            )
                            .await?;
                            match record.status {
                                StepStatus::Success => eprintln!(
                                    "    {} {} {}",
                                    step.id().cyan(),
                                    "\u{2713}".green(),
                                    "done".green()
                                ),
                                StepStatus::Skipped => eprintln!(
                                    "    {} {} {}",
                                    step.id().cyan(),
                                    "\u{23ed}".bright_cyan(),
                                    "skipped".bright_black()
                                ),
                                StepStatus::Halted => {
                                    eprintln!(
                                        "    {} {} {}",
                                        step.id().cyan(),
                                        "\u{26a0}".yellow(),
                                        "HALTED".yellow()
                                    )
                                }
                                StepStatus::Failed => {
                                    eprintln!(
                                        "    {} {} {}",
                                        step.id().cyan(),
                                        "\u{2717}".red(),
                                        "FAILED".red()
                                    )
                                }
                            }
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
        if pipeline_id == ADD_FEATURE_PIPELINE {
            Ok(())
        } else {
            Err(RunError::UnknownPipeline(pipeline_id.to_owned()))
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
