//! Pipeline orchestrator. See DESIGN.md §5.3 and §10.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;

use chrono::{DateTime, Utc};
use derrick_config::{Config, Host, OnSplit, PipelineStep, Runner as StepRunner};
use derrick_models::{resolve_role, AuthStore, CompletionRequest, ModelError};
use derrick_substrate::{Substrate, SubstrateError};
use derrick_tools::{CopilotToolPermission, HostError, HostRegistry, HostRequest};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::process::Command;

const ADD_FEATURE_PIPELINE: &str = "add-feature";
const FEATURE_JSON: &str = ".specify/feature.json";
const ASSAY_SYSTEM: &str = "Review the speckit plan. Identify the highest risks, missing edge cases, and constitution contradictions. End with an H2 `## Verdict` followed by exactly one of: accept, revise, reject.";

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

    /// Resume a previous run from the named step.
    pub async fn resume(
        &self,
        run_id: Option<&str>,
        from_step: &str,
    ) -> Result<RunOutcome, RunError> {
        self.validate_pipeline_id(ADD_FEATURE_PIPELINE)?;
        self.validate_config()?;

        let run_id = match run_id {
            Some(run_id) => run_id.to_owned(),
            None => self.latest_run_id()?,
        };
        let manifest_path = self.manifest_path(&run_id);
        let manifest = read_manifest(&manifest_path)?;
        let current_hash = self.config_hash()?;
        if manifest.config_hash != current_hash {
            return Err(RunError::Config(format!(
                "config has changed since this run started (manifest hash {}, current {}); start a fresh run instead",
                manifest.config_hash, current_hash
            )));
        }
        let from_index = self.step_index(from_step)?;
        let mut input = PipelineInput {
            prompt: Some(manifest.prompt),
            skip: manifest.flags.skip.into_iter().collect(),
            unskip: manifest.flags.unskip.into_iter().collect(),
            dry_run: manifest.flags.dry_run,
            run_id: Some(run_id),
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
        write_manifest(&self.manifest_path(&run_id), &manifest)?;

        // §9.C.5 — reserve + create a git worktree for this run. We degrade
        // gracefully: if `git worktree add` fails (e.g. dirty index, missing
        // git binary, no `.git` in repo_root for tests) we log and continue
        // using the repo root as the working directory.
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
        eprintln!("pipeline: {pipeline_id} (run {run_id})");
        // Walk the pipeline tail, batching consecutive steps that share a
        // `parallel_group`. Steps with no group run sequentially; grouped
        // steps fan out via tokio::spawn bounded by parallelism.step_max.
        let tail = &self.config.pipeline()[start_index..];
        let mut idx = 0usize;
        'outer: while idx < tail.len() {
            let step = &tail[idx];
            match step.parallel_group() {
                None => {
                    if self.should_skip(step, &input) {
                        let record = self.skipped_record(step);
                        eprintln!("  {} \u{23ed} skipped", step.id());
                        manifest.tokens_in = manifest
                            .tokens_in
                            .saturating_add(u64::from(record.tokens_in));
                        manifest.tokens_out = manifest
                            .tokens_out
                            .saturating_add(u64::from(record.tokens_out));
                        manifest.steps.push(ManifestStep::from_record(&record));
                        write_manifest(&self.manifest_path(&run_id), &manifest)?;
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
                        let step_id = step.id().to_owned();
                        let frames = scanner_frames();
                        let running = Arc::new(AtomicBool::new(true));
                        let r2 = running.clone();
                        let spinner = tokio::task::spawn(async move {
                            let mut i = 0usize;
                            while r2.load(Ordering::Relaxed) {
                                eprint!("\r  {} {}...", step_id, frames[i]);
                                let _ = std::io::stderr().flush();
                                tokio::time::sleep(Duration::from_millis(80)).await;
                                i = (i + 1) % frames.len();
                            }
                        });

                        let result = self.execute_step(step, &mut state).await;
                        running.store(false, Ordering::Relaxed);
                        let _ = spinner.await;
                        result?
                    };
                    eprint!("\r                                            \r");
                    match record.status {
                        StepStatus::Success => eprintln!("  {} \u{2713}", step.id()),
                        StepStatus::Skipped => eprintln!("  {} \u{23ed}", step.id()),
                        StepStatus::Halted => eprintln!("  {} \u{26a0} HALTED", step.id()),
                        StepStatus::Failed => eprintln!("  {} \u{2717} FAILED", step.id()),
                    }
                    manifest.feature_dir = state.feature_dir.clone();
                    manifest.tokens_in = manifest
                        .tokens_in
                        .saturating_add(u64::from(record.tokens_in));
                    manifest.tokens_out = manifest
                        .tokens_out
                        .saturating_add(u64::from(record.tokens_out));
                    manifest.steps.push(ManifestStep::from_record(&record));
                    write_manifest(&self.manifest_path(&run_id), &manifest)?;

                    if record.status == StepStatus::Success {
                        if let Some(feature_dir) = &state.feature_dir {
                            let wd = self.working_dir(&state);
                            if step.id() == "specify" {
                                let p = wd.join(feature_dir).join("spec.md");
                                if p.exists() {
                                    if let Ok(c) = std::fs::read_to_string(&p) {
                                        eprintln!("\n--- Specification ---\n{c}---\n");
                                    }
                                }
                            } else if step.id() == "plan" {
                                let p = wd.join(feature_dir).join("plan.md");
                                if p.exists() {
                                    if let Ok(c) = std::fs::read_to_string(&p) {
                                        eprintln!("\n--- Plan ---\n{c}---\n");
                                    }
                                }
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
                    // Collect this run of consecutive steps with the same group name.
                    let mut group_end = idx;
                    while group_end < tail.len()
                        && tail[group_end].parallel_group() == Some(group_name.as_str())
                    {
                        group_end += 1;
                    }

                    // §9.C.4 True parallel fan-out: run independent steps
                    // concurrently, bounded by parallelism.step_max.
                    let group_steps: Vec<PipelineStep> = tail[idx..group_end].to_vec();
                    let step_max = self.config.parallelism().step_max().max(1) as usize;
                    let semaphore = Arc::new(Semaphore::new(step_max));
                    let mut handles: Vec<tokio::task::JoinHandle<Result<StepRecord, RunError>>> =
                        Vec::new();
                    let mut skipped: Vec<StepRecord> = Vec::new();

                    eprintln!(
                        "  parallel group \"{}\" ({} steps)...",
                        group_name,
                        group_steps.len()
                    );
                    for step in &group_steps {
                        if self.should_skip(step, &input) {
                            let record = self.skipped_record(step);
                            eprintln!("    {} \u{23ed} skipped", step.id());
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
                        eprintln!("    {}...", step.id());
                        let sem = semaphore.clone();
                        let runner = self.clone();
                        let step = step.clone();
                        let state_clone = state.clone();
                        handles.push(tokio::task::spawn(async move {
                            let _permit =
                                sem.acquire_owned().await.expect("semaphore never closed");
                            let mut st = state_clone;
                            let record = runner.execute_step(&step, &mut st).await?;
                            match record.status {
                                StepStatus::Success => eprintln!("    {} \u{2713}", step.id()),
                                StepStatus::Skipped => eprintln!("    {} \u{23ed}", step.id()),
                                StepStatus::Halted => {
                                    eprintln!("    {} \u{26a0} HALTED", step.id())
                                }
                                StepStatus::Failed => {
                                    eprintln!("    {} \u{2717} FAILED", step.id())
                                }
                            }
                            Ok(record)
                        }));
                    }

                    // Flush skipped steps into the manifest first.
                    for record in &skipped {
                        manifest.tokens_in = manifest
                            .tokens_in
                            .saturating_add(u64::from(record.tokens_in));
                        manifest.tokens_out = manifest
                            .tokens_out
                            .saturating_add(u64::from(record.tokens_out));
                        manifest.steps.push(ManifestStep::from_record(record));
                        write_manifest(&self.manifest_path(&run_id), &manifest)?;
                    }

                    // §9.C.7 failure isolation — all steps in the group run
                    // concurrently; if any halts or fails we stop the outer
                    // pipeline loop after collecting all results.
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
                        write_manifest(&self.manifest_path(&run_id), &manifest)?;
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
        write_manifest(&self.manifest_path(&run_id), &manifest)?;

        // §9.C.5 — tear down the worktree regardless of outcome.
        if let Some(path) = state.worktree_path.clone() {
            self.teardown_worktree(&run_id, &path).await;
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

    async fn setup_worktree(
        &self,
        run_id: &str,
        branch: &str,
        state: &mut ExecutionState,
    ) -> Result<(), RunError> {
        let path = self
            .substrate
            .reserve_worktree(run_id, branch)
            .await
            .map_err(RunError::Substrate)?;

        let result = Command::new("git")
            .args(["worktree", "add", "-b", branch])
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

    async fn teardown_worktree(&self, run_id: &str, path: &Path) {
        // Close the DB row (emits WorktreeFinalized).
        let _ = self.substrate.close_worktree(run_id).await;
        // Remove the on-disk worktree.
        let _ = Command::new("git")
            .args(["worktree", "remove", "--force"])
            .arg(path)
            .current_dir(&self.repo_root)
            .kill_on_drop(true)
            .output()
            .await;
    }

    async fn execute_step(
        &self,
        step: &PipelineStep,
        state: &mut ExecutionState,
    ) -> Result<StepRecord, RunError> {
        let started_at = Utc::now();
        let log_path = state.run_dir.join(format!("step-{}.log", step.id()));
        let result = match (step.role(), step.runner()) {
            (Some(_), None) => self.execute_role_step(step, state, &log_path).await,
            (None, Some(StepRunner::Derrick)) => {
                self.execute_derrick_step(step, state, &log_path).await
            }
            (None, Some(StepRunner::Human)) => self.execute_human_step(step, state, &log_path),
            (None, Some(StepRunner::Bash)) => self.execute_bash_step(step, state, &log_path).await,
            _ => Err(RunError::Config(format!(
                "pipeline.{}: either supported role or runner is required",
                step.id()
            ))),
        };
        let finished_at = Utc::now();

        let run_id = state.run_id.clone();
        match result {
            Ok(StepExecution {
                status,
                artifacts,
                tokens_in,
                tokens_out,
            }) => {
                let status_str = match status {
                    StepStatus::Skipped => "skipped",
                    StepStatus::Success => "success",
                    StepStatus::Failed => "failed",
                    StepStatus::Halted => "halted",
                };
                let _ = self
                    .substrate
                    .record_typed_event(
                        derrick_substrate::EventScope::Worktree {
                            run_id: run_id.clone(),
                        },
                        derrick_substrate::EventKind::PipelineStepCompleted {
                            step_id: step.id().to_owned(),
                            status: status_str.to_owned(),
                        },
                    )
                    .await;
                Ok(StepRecord {
                    id: step.id().to_owned(),
                    status,
                    started_at,
                    finished_at,
                    log_path,
                    artifacts,
                    tokens_in,
                    tokens_out,
                })
            }
            Err(error) => {
                let _ignored = append_log(&log_path, &format!("{error}\n"));
                let record = StepRecord {
                    id: step.id().to_owned(),
                    status: StepStatus::Failed,
                    started_at,
                    finished_at,
                    log_path,
                    artifacts: Vec::new(),
                    tokens_in: 0,
                    tokens_out: 0,
                };
                let _ = self
                    .substrate
                    .record_typed_event(
                        derrick_substrate::EventScope::Worktree {
                            run_id: run_id.clone(),
                        },
                        derrick_substrate::EventKind::PipelineStepCompleted {
                            step_id: step.id().to_owned(),
                            status: "failed".to_owned(),
                        },
                    )
                    .await;
                let manifest_path = self.manifest_path(&state.run_id);
                if let Ok(mut manifest) = read_manifest(&manifest_path) {
                    manifest.status = RunStatus::Failed;
                    manifest.finished_at = Some(finished_at);
                    manifest.steps.push(ManifestStep::from_record(&record));
                    let _ignored = write_manifest(&manifest_path, &manifest);
                }
                Err(error)
            }
        }
    }

    async fn execute_role_step(
        &self,
        step: &PipelineStep,
        state: &mut ExecutionState,
        log_path: &Path,
    ) -> Result<StepExecution, RunError> {
        if let Some(host) = step.host() {
            let command = required_step_text(step.command(), step.id(), "command")?;
            let prompt = render_template(command, &self.template_context(state)?)?;
            let host_name = host_name(host);
            let host = self
                .hosts
                .get(host_name)
                .ok_or_else(|| RunError::Config(format!("host {host_name:?} is not registered")))?;
            // D36: speckit's /specify command writes into `.specify/features/`.
            // Headless host invocations cannot answer Write permission prompts
            // for a directory that does not yet exist, so create the path up
            // front. Safe to run on every invocation — `create_dir_all` is a
            // no-op when the directory already exists.
            if step.id() == "specify" {
                create_dir_all(&self.working_dir(state).join(".specify").join("features"))?;
            }
            let mut request = HostRequest::new(prompt, self.working_dir(state));
            // Pipeline steps run without a terminal — tell the host to suppress
            // interactive permission prompts. See D36 and HostRequest::headless.
            request.headless = true;
            if host_name == "copilot" {
                request.copilot_tools = CopilotToolPermission::AllowAll;
            }
            let response = host
                .run(request)
                .await
                .map_err(|source| RunError::StepFailed {
                    id: step.id().to_owned(),
                    message: source.to_string(),
                })?;
            write_log(log_path, &response.stdout, &response.stderr)?;
            if step.id() == "specify" {
                state.feature_dir = Some(read_feature_dir(self.working_dir(state))?);
            }
            Ok(StepExecution::success(
                self.detect_artifacts(step.id(), state),
            ))
        } else {
            let role = required_step_text(step.role(), step.id(), "role")?;
            let prompt = step
                .command()
                .map_or_else(|| state.prompt.clone(), ToOwned::to_owned);
            let rendered = render_template(&prompt, &self.template_context(state)?)?;
            let model = resolve_role(
                role,
                self.config.roles(),
                self.config.models(),
                &AuthStore::from_env(),
            )
            .await?;
            let response = model
                .complete(completion_request(rendered, None, None))
                .await?;
            write_log(log_path, &response.text, "")?;
            Ok(
                StepExecution::success(self.detect_artifacts(step.id(), state))
                    .with_tokens(response.tokens_in, response.tokens_out),
            )
        }
    }

    async fn execute_derrick_step(
        &self,
        step: &PipelineStep,
        state: &mut ExecutionState,
        log_path: &Path,
    ) -> Result<StepExecution, RunError> {
        match step.id() {
            "assay" => self.execute_assay(step, state, log_path).await,
            "clarify" => self.execute_clarify(state, log_path).await,
            "bridge" => {
                write_log(log_path, "bridge skipped in solo mode\n", "")?;
                Ok(StepExecution::skipped())
            }
            "foreman" => {
                write_log(log_path, "foreman skipped in solo mode\n", "")?;
                Ok(StepExecution::skipped())
            }
            other => Err(RunError::Config(format!(
                "runner derrick is not supported for step {other:?} in T010"
            ))),
        }
    }

    fn execute_human_step(
        &self,
        step: &PipelineStep,
        state: &ExecutionState,
        log_path: &Path,
    ) -> Result<StepExecution, RunError> {
        let prompt = required_step_text(step.prompt(), step.id(), "prompt")?;
        let prompt = render_template(prompt, &self.template_context(state)?)?;
        write_log(log_path, &prompt, "")?;
        let mut stdout = std::io::stdout();
        stdout
            .write_all(prompt.as_bytes())
            .and_then(|()| stdout.write_all(b"\n"))
            .and_then(|()| stdout.flush())
            .map_err(|source| RunError::Io {
                path: PathBuf::from("<stdout>"),
                source,
            })?;
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .map_err(|source| RunError::Io {
                path: PathBuf::from("<stdin>"),
                source,
            })?;
        if answer.trim().eq_ignore_ascii_case("y") || answer.trim().eq_ignore_ascii_case("yes") {
            Ok(StepExecution::success(Vec::new()))
        } else {
            Ok(StepExecution::halted(Vec::new(), "checkpoint declined"))
        }
    }

    async fn execute_bash_step(
        &self,
        step: &PipelineStep,
        state: &ExecutionState,
        log_path: &Path,
    ) -> Result<StepExecution, RunError> {
        let command = required_step_text(step.command(), step.id(), "command")?;
        let command = render_template(command, &self.template_context(state)?)?;
        let working_dir = self.working_dir(state).to_path_buf();
        let output = Command::new("bash")
            .arg("-lc")
            .arg(command)
            .current_dir(&working_dir)
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|source| RunError::Io {
                path: working_dir,
                source,
            })?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        write_log(log_path, &stdout, &stderr)?;
        if output.status.success() {
            Ok(StepExecution::success(Vec::new()))
        } else {
            Err(RunError::StepFailed {
                id: step.id().to_owned(),
                message: format!("bash exited with {}", output.status),
            })
        }
    }

    async fn execute_assay(
        &self,
        step: &PipelineStep,
        state: &mut ExecutionState,
        log_path: &Path,
    ) -> Result<StepExecution, RunError> {
        let feature_dir = state
            .feature_dir
            .clone()
            .ok_or_else(|| RunError::Config("assay requires feature_dir".to_owned()))?;
        let reviewers: Vec<String> = self.config.tools().assay().reviewers().to_vec();
        let on_split = self.config.tools().assay().on_split();
        let fallback_role = self.config.tools().assay().role().to_owned();

        if reviewers.len() <= 1 {
            let reviewer_role = reviewers
                .first()
                .cloned()
                .unwrap_or_else(|| fallback_role.clone());
            let reviewer_dir = self.working_dir(state).join(&feature_dir).join("assay");
            let outcome = match self
                .run_reviewer_rounds(step, state, log_path, &reviewer_role, &reviewer_dir)
                .await?
            {
                ReviewerRoundOutcome::Skipped => return Ok(StepExecution::skipped()),
                ReviewerRoundOutcome::Decided(outcome) => outcome,
            };
            let (tokens_in, tokens_out) = (outcome.tokens_in, outcome.tokens_out);
            return match outcome.verdict.as_str() {
                "accept" => Ok(StepExecution::success(vec![relative_to_root(
                    &self.repo_root,
                    outcome.verdict_path,
                )?])
                .with_tokens(tokens_in, tokens_out)),
                "reject" => Ok(StepExecution::halted(
                    vec![relative_to_root(&self.repo_root, outcome.verdict_path)?],
                    "assay rejected",
                )
                .with_tokens(tokens_in, tokens_out)),
                _ => Ok(StepExecution::halted(
                    vec![relative_to_root(&self.repo_root, outcome.verdict_path)?],
                    "assay requested revisions past configured rounds",
                )
                .with_tokens(tokens_in, tokens_out)),
            };
        }

        // §9.C.2 True parallel fan-out: run each reviewer concurrently bounded
        // by parallelism.assay_max. Each reviewer gets its own log path to avoid
        // concurrent append races on the step-level log file.
        let assay_max = self.config.parallelism().assay_max().max(1) as usize;
        let semaphore = Arc::new(Semaphore::new(assay_max));
        let mut handles: Vec<tokio::task::JoinHandle<Result<ReviewerRoundOutcome, RunError>>> =
            Vec::with_capacity(reviewers.len());
        let working_dir = self.working_dir(state).to_path_buf();

        for reviewer_role in &reviewers {
            let reviewer_dir = working_dir
                .join(&feature_dir)
                .join("assay")
                .join(reviewer_role);
            create_dir_all(&reviewer_dir)?;

            let sem = semaphore.clone();
            let runner = self.clone();
            let step = step.clone();
            let mut state_clone = state.clone();
            let role = reviewer_role.clone();
            let reviewer_log =
                state
                    .run_dir
                    .join(format!("step-{}-{}.log", step.id(), reviewer_role));

            handles.push(tokio::task::spawn(async move {
                let _permit = sem.acquire_owned().await.expect("semaphore never closed");
                runner
                    .run_reviewer_rounds(
                        &step,
                        &mut state_clone,
                        &reviewer_log,
                        &role,
                        &reviewer_dir,
                    )
                    .await
            }));
        }

        let mut outcomes: Vec<ReviewerOutcome> = Vec::with_capacity(reviewers.len());
        let mut any_skipped = false;
        for handle in handles {
            match handle
                .await
                .map_err(|e| RunError::Config(format!("reviewer task join: {e}")))?
            {
                Ok(ReviewerRoundOutcome::Decided(outcome)) => outcomes.push(outcome),
                Ok(ReviewerRoundOutcome::Skipped) => any_skipped = true,
                Err(e) => {
                    tracing::error!(
                        run_id = %state.run_id,
                        error = %e,
                        "reviewer in multi-reviewer assay failed"
                    );
                    return Err(e);
                }
            }
        }
        if any_skipped {
            return Ok(StepExecution::skipped());
        }

        let combined_path = self
            .working_dir(state)
            .join(&feature_dir)
            .join("assay")
            .join("verdict.md");
        reconcile_verdicts(&outcomes, on_split, &combined_path, &self.repo_root)
    }

    async fn execute_clarify(
        &self,
        state: &ExecutionState,
        log_path: &Path,
    ) -> Result<StepExecution, RunError> {
        let feature_dir = state.feature_dir.clone().ok_or_else(|| {
            RunError::Config("clarify requires feature_dir from specify step".to_owned())
        })?;
        let wd = self.working_dir(state);
        let spec_path = wd.join(&feature_dir).join("spec.md");
        let spec = std::fs::read_to_string(&spec_path).map_err(|source| RunError::Io {
            path: spec_path,
            source,
        })?;

        eprintln!("\n--- Specification (review before clarification) ---\n{spec}---\n");

        let prompt = format!(
            "You are helping refine a specification. Based on the specification below, generate \
             clarifying questions to ensure the requirements are well-understood. Focus on ambiguous \
             areas, trade-offs, and critical decisions that need human input.\n\n\
             For each question, provide:\n\
             - The question\n\
             - Multiple choice options (at least 2)\n\
             - Your recommendation\n\n\
             Format each question as:\n\
             Q: <question>\n\
             Options: <option1>, <option2>, ...\n\
             Recommendation: <recommended option>\n\n\
             Specification:\n{spec}"
        );

        let model = match derrick_models::resolve_role(
            "drafter",
            self.config.roles(),
            self.config.models(),
            &derrick_models::AuthStore::from_env(),
        )
        .await
        {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("clarify: cannot resolve drafter model, skipping: {e}");
                return Ok(StepExecution::success(Vec::new()));
            }
        };

        let response = match model.complete(completion_request(prompt, None, None)).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("clarify: model call failed, skipping: {e}");
                return Ok(StepExecution::success(Vec::new()));
            }
        };

        write_log(log_path, &response.text, "")?;

        let questions = parse_clarify_questions(&response.text);
        if questions.is_empty() {
            eprintln!("No clarifying questions needed. Proceeding.");
            return Ok(StepExecution::success(Vec::new()));
        }

        let mut answers: Vec<String> = Vec::new();
        for (i, q) in questions.iter().enumerate() {
            eprintln!("\n--- Question {} of {} ---", i + 1, questions.len());
            eprintln!("{}", q.question);
            if !q.options.is_empty() {
                for (j, opt) in q.options.iter().enumerate() {
                    let is_rec = q.recommendation.as_deref() == Some(opt.as_str());
                    if is_rec {
                        eprintln!("  {}. {} [recommended]", j + 1, opt);
                    } else {
                        eprintln!("  {}. {}", j + 1, opt);
                    }
                }
            }
            eprint!("Your answer (or press Enter to accept recommendation): ");
            std::io::stdout().flush().map_err(|source| RunError::Io {
                path: PathBuf::from("<stdout>"),
                source,
            })?;
            let mut answer = String::new();
            std::io::stdin()
                .read_line(&mut answer)
                .map_err(|source| RunError::Io {
                    path: PathBuf::from("<stdin>"),
                    source,
                })?;
            let trimmed = answer.trim().to_owned();
            if trimmed.is_empty() {
                answers.push(q.recommendation.clone().unwrap_or_default());
            } else {
                answers.push(trimmed);
            }
        }

        let clarify_path = wd.join(&feature_dir).join("clarify.md");
        let mut content = String::from("# Clarification Q&A\n\n");
        for (q, a) in questions.iter().zip(answers.iter()) {
            write!(
                content,
                "## Question\n{}\n\nOptions: {}\n\nRecommendation: {}\n\nAnswer: {}\n\n",
                q.question,
                q.options.join(", "),
                q.recommendation.as_deref().unwrap_or("none"),
                a
            )
            .unwrap();
        }
        std::fs::write(&clarify_path, &content).map_err(|source| RunError::Io {
            path: clarify_path.clone(),
            source,
        })?;

        eprintln!("\nClarification complete. Answers saved.");
        Ok(
            StepExecution::success(vec![relative_to_root(&self.repo_root, clarify_path)?])
                .with_tokens(response.tokens_in, response.tokens_out),
        )
    }

    async fn run_reviewer_rounds(
        &self,
        step: &PipelineStep,
        state: &mut ExecutionState,
        log_path: &Path,
        reviewer_role: &str,
        reviewer_dir: &Path,
    ) -> Result<ReviewerRoundOutcome, RunError> {
        let feature_dir = state
            .feature_dir
            .clone()
            .ok_or_else(|| RunError::Config("assay requires feature_dir".to_owned()))?;
        let rounds = self.assay_rounds(step, state)?;
        let spec = read_to_string(&self.working_dir(state).join(&feature_dir).join("spec.md"))?;
        let constitution = read_to_string(
            &self
                .working_dir(state)
                .join(self.config.guardrails().constitution_path()),
        )?;

        // D37: codex requires a TTY on stdin and aborts in headless contexts
        // (CI, background subprocesses). When the reviewer resolves to the
        // codex CLI and we have no TTY, fall back to the `claude` host. If
        // claude is not registered we emit a warning and skip rather than
        // erroring, mirroring `--skip assay` behaviour.
        let codex_fallback = self.detect_codex_fallback(reviewer_role).await?;
        if codex_fallback {
            if self.hosts.get("claude").is_none() {
                tracing::warn!(
                    step = "assay",
                    reviewer = reviewer_role,
                    reason = "codex requires TTY; claude host not registered, skipping assay"
                );
                return Ok(ReviewerRoundOutcome::Skipped);
            }
            tracing::warn!(
                step = "assay",
                reviewer = reviewer_role,
                reason = "codex requires TTY; falling back to claude reviewer"
            );
        }

        let verdict_path = reviewer_dir.join("verdict.md");
        create_dir_all(parent(&verdict_path)?)?;

        let mut tokens_in_total: u32 = 0;
        let mut tokens_out_total: u32 = 0;

        for round in 1..=rounds {
            let plan = read_to_string(&self.working_dir(state).join(&feature_dir).join("plan.md"))?;
            let prompt = format!("Task: {}\n\nPlan:\n{plan}", state.prompt);
            let cached = format!("Constitution:\n{constitution}\n\nSpec:\n{spec}");
            let (response_text, model_name, round_tokens_in, round_tokens_out) = if codex_fallback {
                let host = self.hosts.get("claude").ok_or_else(|| {
                    RunError::Config("host \"claude\" is not registered".to_owned())
                })?;
                let full_prompt = format!("{ASSAY_SYSTEM}\n\n{cached}\n\n{prompt}");
                let host_response = host
                    .run(HostRequest {
                        headless: true,
                        ..HostRequest::new(full_prompt, self.working_dir(state))
                    })
                    .await
                    .map_err(|source| RunError::StepFailed {
                        id: step.id().to_owned(),
                        message: source.to_string(),
                    })?;
                (host_response.stdout, "claude".to_owned(), 0u32, 0u32)
            } else {
                let model = resolve_role(
                    reviewer_role,
                    self.config.roles(),
                    self.config.models(),
                    &AuthStore::from_env(),
                )
                .await?;
                let name = model.name().to_owned();
                let response = model
                    .complete(completion_request(
                        prompt,
                        Some(cached),
                        Some(ASSAY_SYSTEM.to_owned()),
                    ))
                    .await?;
                (response.text, name, response.tokens_in, response.tokens_out)
            };
            tokens_in_total = tokens_in_total.saturating_add(round_tokens_in);
            tokens_out_total = tokens_out_total.saturating_add(round_tokens_out);
            append_log(log_path, &response_text)?;
            let verdict = parse_verdict(&response_text).ok_or_else(|| RunError::StepFailed {
                id: step.id().to_owned(),
                message: "could not parse verdict from reviewer response".to_owned(),
            })?;
            let verdict_body = format!(
                "model: {model_name}\nreviewer: {reviewer_role}\nround: {round}\nverdict: {verdict}\n\n{response_text}"
            );
            write_file(&verdict_path, &verdict_body)?;
            match verdict {
                "accept" | "reject" => {
                    return Ok(ReviewerRoundOutcome::Decided(ReviewerOutcome {
                        role: reviewer_role.to_owned(),
                        verdict: verdict.to_owned(),
                        verdict_path: verdict_path.clone(),
                        tokens_in: tokens_in_total,
                        tokens_out: tokens_out_total,
                    }));
                }
                "revise" if round < rounds => {
                    let objections = suggested_revisions(&response_text).ok_or_else(|| {
                        RunError::StepFailed {
                            id: step.id().to_owned(),
                            message: "could not parse suggested revisions from reviewer response"
                                .to_owned(),
                        }
                    })?;
                    self.replan_from_objections(state, objections).await?;
                }
                "revise" => {
                    return Ok(ReviewerRoundOutcome::Decided(ReviewerOutcome {
                        role: reviewer_role.to_owned(),
                        verdict: "revise".to_owned(),
                        verdict_path: verdict_path.clone(),
                        tokens_in: tokens_in_total,
                        tokens_out: tokens_out_total,
                    }));
                }
                _ => unreachable_verdict(step.id())?,
            }
        }

        Ok(ReviewerRoundOutcome::Decided(ReviewerOutcome {
            role: reviewer_role.to_owned(),
            verdict: "revise".to_owned(),
            verdict_path,
            tokens_in: tokens_in_total,
            tokens_out: tokens_out_total,
        }))
    }

    /// Returns true when stdin is not a TTY and the configured reviewer role
    /// resolves to a codex-family model. Used by `execute_assay` to fall back
    /// to the `claude` host (D37).
    async fn detect_codex_fallback(&self, reviewer_role: &str) -> Result<bool, RunError> {
        if std::io::stdin().is_terminal() {
            return Ok(false);
        }
        // Inspect the role binding without spawning the model: a fallback
        // decision should not depend on the model's cli being on PATH.
        let Some(model_name) = self.config.roles().get(reviewer_role) else {
            return Ok(false);
        };
        let Some(model_def) = self.config.models().get(model_name) else {
            return Ok(false);
        };
        let codex_family = model_name.eq_ignore_ascii_case("codex")
            || model_name.to_ascii_lowercase().starts_with("codex")
            || model_def
                .cli()
                .is_some_and(|cli| cli.split_whitespace().next() == Some("codex"));
        Ok(codex_family)
    }

    async fn replan_from_objections(
        &self,
        state: &ExecutionState,
        objections: &str,
    ) -> Result<(), RunError> {
        let plan_step = self
            .config
            .pipeline()
            .iter()
            .find(|step| step.id() == "plan")
            .ok_or_else(|| RunError::Config("assay revise requires a plan step".to_owned()))?;
        let host = plan_step
            .host()
            .ok_or_else(|| RunError::Config("assay revise requires plan step host".to_owned()))?;
        let host_name = host_name(host);
        let host = self
            .hosts
            .get(host_name)
            .ok_or_else(|| RunError::Config(format!("host {host_name:?} is not registered")))?;
        let prompt = format!(
            "The reviewer raised the following objections. Produce a delta to plan.md that addresses each. Do not rewrite the plan from scratch.\n\n{objections}"
        );
        let response = host
            .run(HostRequest::new(prompt, self.working_dir(state)))
            .await
            .map_err(|source| RunError::StepFailed {
                id: "plan".to_owned(),
                message: source.to_string(),
            })?;
        if !response.stdout.trim().is_empty() {
            let feature_dir = state
                .feature_dir
                .as_ref()
                .ok_or_else(|| RunError::Config("replan requires feature_dir".to_owned()))?;
            let plan_path = self.working_dir(state).join(feature_dir).join("plan.md");
            append_log(&plan_path, &response.stdout)?;
        }
        Ok(())
    }

    fn detect_artifacts(&self, step_id: &str, state: &ExecutionState) -> Vec<PathBuf> {
        let mut candidates = Vec::new();
        match step_id {
            "specify" => {
                candidates.push(PathBuf::from(FEATURE_JSON));
                if let Some(feature_dir) = &state.feature_dir {
                    candidates.push(feature_dir.join("spec.md"));
                }
            }
            "plan" => {
                if let Some(feature_dir) = &state.feature_dir {
                    candidates.push(feature_dir.join("plan.md"));
                }
            }
            "tasks" => {
                if let Some(feature_dir) = &state.feature_dir {
                    candidates.push(feature_dir.join("tasks.md"));
                }
            }
            "assay" => {
                if let Some(feature_dir) = &state.feature_dir {
                    candidates.push(feature_dir.join("assay/verdict.md"));
                }
            }
            _ => {}
        }
        candidates
            .into_iter()
            .filter(|path| self.working_dir(state).join(path).exists())
            .collect()
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

    fn assay_rounds(&self, step: &PipelineStep, state: &ExecutionState) -> Result<usize, RunError> {
        let raw = step
            .rounds()
            .unwrap_or_else(|| self.config.tools().assay().rounds());
        let rendered = if raw == "{{tools.assay.rounds}}" {
            self.config.tools().assay().rounds().to_owned()
        } else {
            render_template(raw, &self.template_context(state)?)?
        };
        rendered.parse::<usize>().map_err(|error| {
            RunError::Config(format!(
                "pipeline.{}.rounds: expected positive integer: {error}",
                step.id()
            ))
        })
    }

    fn template_context(&self, state: &ExecutionState) -> Result<TemplateContext, RunError> {
        Ok(TemplateContext {
            prompt: state.prompt.clone(),
            site_name: self.config.site().name().to_owned(),
            site_prefix: self.config.site().prefix().to_owned(),
            feature_dir: state.feature_dir.clone(),
            run_id: state.run_id.clone(),
        })
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

/// Input values and flags for a pipeline run.
#[derive(Clone, Debug, Default)]
pub struct PipelineInput {
    /// The `/add-feature` prompt.
    pub prompt: Option<String>,
    /// Step IDs explicitly skipped for this run.
    pub skip: BTreeSet<String>,
    /// Step IDs explicitly re-enabled despite `default_skip: true`.
    pub unskip: BTreeSet<String>,
    /// Halt after the `tasks` step.
    pub dry_run: bool,
    /// Override run id.
    pub run_id: Option<String>,
}

/// Result returned after a pipeline run.
#[derive(Clone, Debug)]
pub struct RunOutcome {
    /// Run identifier.
    pub run_id: String,
    /// Final run status.
    pub status: RunStatus,
    /// Feature directory after `specify` completes.
    pub feature_dir: Option<PathBuf>,
    /// Per-step records.
    pub steps: Vec<StepRecord>,
    /// Total input tokens consumed by model calls in this run.
    pub tokens_in: u64,
    /// Total output tokens produced by model calls in this run.
    pub tokens_out: u64,
}

impl RunOutcome {
    /// Estimate USD cost for model-backed steps, using the built-in pricing table.
    /// Returns `None` if the model name is unknown.
    pub fn cost_estimate_usd(&self, model_name: &str) -> Option<f64> {
        derrick_models::builtin_cost_hint(model_name)
            .map(|hint| hint.estimate_usd(self.tokens_in, self.tokens_out))
    }
}

/// Final run status.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// All required steps completed.
    Success,
    /// A step failed.
    Failed,
    /// The run intentionally halted.
    Halted,
}

/// One step's execution record.
#[derive(Clone, Debug)]
pub struct StepRecord {
    /// Step identifier.
    pub id: String,
    /// Final step status.
    pub status: StepStatus,
    /// Start timestamp.
    pub started_at: DateTime<Utc>,
    /// Finish timestamp.
    pub finished_at: DateTime<Utc>,
    /// Step log path.
    pub log_path: PathBuf,
    /// Artifacts observed after this step.
    pub artifacts: Vec<PathBuf>,
    /// Input tokens consumed by model calls in this step (0 for non-model steps).
    pub tokens_in: u32,
    /// Output tokens produced by model calls in this step (0 for non-model steps).
    pub tokens_out: u32,
}

impl From<ManifestStep> for StepRecord {
    fn from(step: ManifestStep) -> Self {
        Self {
            id: step.id,
            status: step.status,
            started_at: step.started_at,
            finished_at: step.finished_at,
            log_path: step.log_path,
            artifacts: step.artifacts,
            tokens_in: step.tokens_in,
            tokens_out: step.tokens_out,
        }
    }
}

/// Per-step status.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    /// Step was skipped.
    Skipped,
    /// Step completed successfully.
    Success,
    /// Step failed.
    Failed,
    /// Step intentionally halted the run.
    Halted,
}

/// Errors returned by the runner.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum RunError {
    /// Pipeline id is unknown.
    #[error("unknown pipeline: {0}")]
    UnknownPipeline(String),
    /// Required prompt is absent.
    #[error("missing prompt for pipeline {0}")]
    MissingPrompt(String),
    /// A step failed.
    #[error("step {id} failed: {message}")]
    StepFailed {
        /// Step identifier.
        id: String,
        /// Failure message.
        message: String,
    },
    /// Substrate operation failed.
    #[error("substrate error: {0}")]
    Substrate(#[from] SubstrateError),
    /// Host adapter failed.
    #[error("host error: {0}")]
    Host(#[from] HostError),
    /// Model provider failed.
    #[error("model error: {0}")]
    Model(#[from] ModelError),
    /// Filesystem operation failed.
    #[error("io error at {path}: {source}")]
    Io {
        /// Path involved in the operation.
        path: PathBuf,
        /// Underlying source.
        source: std::io::Error,
    },
    /// JSON operation failed.
    #[error("json error at {path}: {source}")]
    Json {
        /// Path involved in the operation.
        path: PathBuf,
        /// Underlying source.
        source: serde_json::Error,
    },
    /// Configuration is unsupported.
    #[error("config error: {0}")]
    Config(String),
}

#[derive(Clone)]
struct ExecutionState {
    prompt: String,
    run_id: String,
    run_dir: PathBuf,
    feature_dir: Option<PathBuf>,
    worktree_path: Option<PathBuf>,
}

impl ExecutionState {
    fn new(prompt: String, run_id: String, run_dir: PathBuf) -> Self {
        Self {
            prompt,
            run_id,
            run_dir,
            feature_dir: None,
            worktree_path: None,
        }
    }
}

#[derive(Debug)]
struct ReviewerOutcome {
    role: String,
    verdict: String,
    verdict_path: PathBuf,
    tokens_in: u32,
    tokens_out: u32,
}

#[derive(Debug)]
enum ReviewerRoundOutcome {
    Decided(ReviewerOutcome),
    Skipped,
}

struct StepExecution {
    status: StepStatus,
    artifacts: Vec<PathBuf>,
    tokens_in: u32,
    tokens_out: u32,
}

impl StepExecution {
    fn success(artifacts: Vec<PathBuf>) -> Self {
        Self {
            status: StepStatus::Success,
            artifacts,
            tokens_in: 0,
            tokens_out: 0,
        }
    }

    fn skipped() -> Self {
        Self {
            status: StepStatus::Skipped,
            artifacts: Vec::new(),
            tokens_in: 0,
            tokens_out: 0,
        }
    }

    fn halted(artifacts: Vec<PathBuf>, _message: impl Into<String>) -> Self {
        Self {
            status: StepStatus::Halted,
            artifacts,
            tokens_in: 0,
            tokens_out: 0,
        }
    }

    fn with_tokens(mut self, tokens_in: u32, tokens_out: u32) -> Self {
        self.tokens_in = tokens_in;
        self.tokens_out = tokens_out;
        self
    }
}

#[derive(Deserialize, Serialize)]
struct RunManifest {
    run_id: String,
    pipeline_id: String,
    prompt: String,
    flags: FlagsManifest,
    config_hash: String,
    started_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
    status: RunStatus,
    feature_dir: Option<PathBuf>,
    steps: Vec<ManifestStep>,
    #[serde(default)]
    tokens_in: u64,
    #[serde(default)]
    tokens_out: u64,
}

impl RunManifest {
    fn new(
        run_id: String,
        pipeline_id: String,
        prompt: String,
        flags: FlagsManifest,
        config_hash: String,
        started_at: DateTime<Utc>,
    ) -> Self {
        Self {
            run_id,
            pipeline_id,
            prompt,
            flags,
            config_hash,
            started_at,
            finished_at: None,
            status: RunStatus::Success,
            feature_dir: None,
            steps: Vec::new(),
            tokens_in: 0,
            tokens_out: 0,
        }
    }
}

#[derive(Deserialize, Serialize)]
struct FlagsManifest {
    skip: Vec<String>,
    unskip: Vec<String>,
    dry_run: bool,
}

impl FlagsManifest {
    fn from_input(input: &PipelineInput) -> Self {
        Self {
            skip: input.skip.iter().cloned().collect(),
            unskip: input.unskip.iter().cloned().collect(),
            dry_run: input.dry_run,
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
struct ManifestStep {
    id: String,
    status: StepStatus,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    log_path: PathBuf,
    artifacts: Vec<PathBuf>,
    #[serde(default)]
    tokens_in: u32,
    #[serde(default)]
    tokens_out: u32,
}

impl ManifestStep {
    fn from_record(record: &StepRecord) -> Self {
        Self {
            id: record.id.clone(),
            status: record.status,
            started_at: record.started_at,
            finished_at: record.finished_at,
            log_path: record.log_path.clone(),
            artifacts: record.artifacts.clone(),
            tokens_in: record.tokens_in,
            tokens_out: record.tokens_out,
        }
    }
}

struct TemplateContext {
    prompt: String,
    site_name: String,
    site_prefix: String,
    feature_dir: Option<PathBuf>,
    run_id: String,
}

fn render_template(template: &str, context: &TemplateContext) -> Result<String, RunError> {
    let mut rendered = String::new();
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let (prefix, after_prefix) = rest.split_at(start);
        rendered.push_str(prefix);
        let end = after_prefix
            .find("}}")
            .ok_or_else(|| RunError::Config("unterminated template var".to_owned()))?;
        let name = after_prefix[2..end].trim();
        rendered.push_str(&template_value(name, context)?);
        rest = &after_prefix[end + 2..];
    }
    rendered.push_str(rest);
    Ok(rendered)
}

fn template_value(name: &str, context: &TemplateContext) -> Result<String, RunError> {
    match name {
        "prompt" => Ok(context.prompt.clone()),
        "site_name" => Ok(context.site_name.clone()),
        "site_prefix" => Ok(context.site_prefix.clone()),
        "run_id" => Ok(context.run_id.clone()),
        "feature_dir" => context
            .feature_dir
            .as_ref()
            .map(|path| path_string(path))
            .ok_or_else(|| {
                RunError::Config(
                    "template var {{feature_dir}} is not available before specify completes"
                        .to_owned(),
                )
            }),
        "tasks_md" => context
            .feature_dir
            .as_ref()
            .map(|path| path_string(&path.join("tasks.md")))
            .ok_or_else(|| {
                RunError::Config(
                    "template var {{tasks_md}} is not available before specify completes"
                        .to_owned(),
                )
            }),
        "batch" => context
            .feature_dir
            .as_ref()
            .and_then(|path| path.file_name())
            .and_then(|name| name.to_str())
            .map(ToOwned::to_owned)
            .ok_or_else(|| {
                RunError::Config(
                    "template var {{batch}} is not available before specify completes".to_owned(),
                )
            }),
        "rig" => Err(RunError::Config(
            "unknown template var: {{rig}}; use {{site_name}}".to_owned(),
        )),
        other => Err(RunError::Config(format!(
            "unknown template var: {{{{{other}}}}}"
        ))),
    }
}

fn validate_template(template: &str, feature_available: bool) -> Result<(), String> {
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let after_prefix = &rest[start..];
        let end = after_prefix
            .find("}}")
            .ok_or_else(|| "unterminated template var".to_owned())?;
        let name = after_prefix[2..end].trim();
        match name {
            "prompt" | "site_name" | "site_prefix" | "run_id" => {}
            "feature_dir" | "tasks_md" | "batch" if feature_available => {}
            "feature_dir" | "tasks_md" | "batch" => {
                return Err(format!(
                    "template var {{{{{name}}}}} is not available before specify completes"
                ));
            }
            "rig" => return Err("unknown template var: {{rig}}; use {{site_name}}".to_owned()),
            other => return Err(format!("unknown template var: {{{{{other}}}}}")),
        }
        rest = &after_prefix[end + 2..];
    }
    Ok(())
}

fn validate_rounds_template(template: &str, feature_available: bool) -> Result<(), RunError> {
    if template == "{{tools.assay.rounds}}" {
        Ok(())
    } else {
        validate_template(template, feature_available).map_err(RunError::Config)
    }
}

/// Outcome of a single adversarial code review pass.
#[derive(Debug)]
pub struct CodeReviewOutcome {
    /// `"pass"` or `"fail"`.
    pub verdict: String,
    /// Full review text produced by the reviewer model.
    pub review_text: String,
    /// Input tokens consumed by the review call.
    pub tokens_in: u32,
    /// Output tokens produced by the review call.
    pub tokens_out: u32,
}

/// Run one adversarial code review pass.
///
/// Called by `derrick ticket code-review`. Uses the configured reviewer role
/// to assess the diff against the ticket requirements. Verdict is extracted
/// from a `## Verdict` section; if none is found the whole response is
/// returned as-is and treated as a failure.
pub async fn run_code_review(
    diff: &str,
    ticket_title: &str,
    ticket_body: &str,
    role: &str,
    config: &Config,
    _repo_root: &Path,
) -> Result<CodeReviewOutcome, RunError> {
    let prompt = format!(
        "You are an adversarial code reviewer. Review the following diff.\n\n\
         ## Ticket\n\n**{ticket_title}**\n\n{ticket_body}\n\n\
         ## Diff\n\n```diff\n{diff}\n```\n\n\
         Review for: security vulnerabilities, logic errors, missing edge cases, \
         inadequate test coverage, constitution violations, and style issues.\n\
         Be direct and specific — cite line numbers where relevant.\n\
         End your review with `## Verdict` followed by exactly one word on its own \
         line: `pass` or `fail`."
    );

    let model = resolve_role(
        role,
        config.roles(),
        config.models(),
        &AuthStore::from_env(),
    )
    .await?;
    let response = model
        .complete(completion_request(prompt, None, None))
        .await?;

    let verdict = extract_verdict_from_review(&response.text);

    Ok(CodeReviewOutcome {
        verdict,
        review_text: response.text,
        tokens_in: response.tokens_in,
        tokens_out: response.tokens_out,
    })
}

fn extract_verdict_from_review(text: &str) -> String {
    let mut after_verdict = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case("## verdict") {
            after_verdict = true;
            continue;
        }
        if after_verdict && !trimmed.is_empty() {
            let word = trimmed
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_lowercase();
            if word == "pass" || word == "fail" {
                return word;
            }
            return "fail".to_owned();
        }
    }
    "fail".to_owned()
}

#[cfg(test)]
mod code_review_tests {
    use super::extract_verdict_from_review;

    #[test]
    fn pass_verdict_extracted() {
        let text = "Looks good.\n\n## Verdict\npass\n";
        assert_eq!(extract_verdict_from_review(text), "pass");
    }

    #[test]
    fn fail_verdict_extracted() {
        let text = "Bug on line 3.\n\n## Verdict\nfail\n";
        assert_eq!(extract_verdict_from_review(text), "fail");
    }

    #[test]
    fn missing_verdict_treated_as_fail() {
        let text = "Some review text with no verdict heading.";
        assert_eq!(extract_verdict_from_review(text), "fail");
    }

    #[test]
    fn case_insensitive_heading() {
        let text = "## verdict\nPASS";
        assert_eq!(extract_verdict_from_review(text), "pass");
    }
}

fn scanner_frames() -> Vec<String> {
    // KITT-style scanning LED animation. Width is kept small so it works
    // on narrow terminals.
    const WIDTH: usize = 12;
    let mut frames = Vec::with_capacity(WIDTH * 2 - 2);
    for i in 0..WIDTH {
        let mut s = vec![' '; WIDTH];
        s[i] = '\u{2593}';
        frames.push(s.into_iter().collect());
    }
    for i in (1..WIDTH - 1).rev() {
        let mut s = vec![' '; WIDTH];
        s[i] = '\u{2593}';
        frames.push(s.into_iter().collect());
    }
    frames
}

struct ClarifyQuestion {
    question: String,
    options: Vec<String>,
    recommendation: Option<String>,
}

fn parse_clarify_questions(text: &str) -> Vec<ClarifyQuestion> {
    let mut questions: Vec<ClarifyQuestion> = Vec::new();
    let mut question: Option<String> = None;
    let mut options: Vec<String> = Vec::new();
    let mut recommendation: Option<String> = None;
    for line in text.lines() {
        let t = line.trim();
        if let Some(stripped) = t.strip_prefix("Q:") {
            if let Some(q) = question.take() {
                questions.push(ClarifyQuestion {
                    question: q,
                    options: std::mem::take(&mut options),
                    recommendation: recommendation.take(),
                });
            }
            question = Some(stripped.trim().to_owned());
        } else if let Some(stripped) = t.strip_prefix("Options:") {
            options = stripped
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect();
        } else if let Some(stripped) = t.strip_prefix("Recommendation:") {
            recommendation = Some(stripped.trim().to_owned());
        }
    }
    if let Some(q) = question {
        questions.push(ClarifyQuestion {
            question: q,
            options,
            recommendation,
        });
    }
    questions
}

fn completion_request(
    prompt: String,
    cached_prefix: Option<String>,
    system: Option<String>,
) -> CompletionRequest {
    CompletionRequest {
        cached_prefix,
        prompt,
        system,
        max_tokens: Some(4096),
        temperature: Some(0.2),
        timeout: Duration::from_secs(600),
    }
}

fn parse_verdict(text: &str) -> Option<&'static str> {
    let mut in_verdict = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case("## Verdict") {
            in_verdict = true;
            continue;
        }
        if in_verdict {
            match trimmed.to_ascii_lowercase().as_str() {
                "accept" => return Some("accept"),
                "revise" => return Some("revise"),
                "reject" => return Some("reject"),
                "" => {}
                _ if trimmed.starts_with("## ") => return None,
                _ => {}
            }
        }
    }
    None
}

fn suggested_revisions(text: &str) -> Option<&str> {
    let start_marker = "## Suggested revisions";
    let start = text.find(start_marker)? + start_marker.len();
    let rest = &text[start..];
    let end = rest.find("\n## ").unwrap_or(rest.len());
    Some(rest[..end].trim())
}

fn read_feature_dir(repo_root: &Path) -> Result<PathBuf, RunError> {
    let path = repo_root.join(FEATURE_JSON);
    let value: serde_json::Value =
        serde_json::from_str(&read_to_string(&path)?).map_err(|source| RunError::Json {
            path: path.clone(),
            source,
        })?;
    let feature_dir = value
        .get("feature_directory")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            RunError::Config(".specify/feature.json missing feature_directory".to_owned())
        })?;
    Ok(PathBuf::from(feature_dir))
}

fn config_hash(path: &Path) -> Result<String, RunError> {
    let bytes = std::fs::read(path).map_err(|source| RunError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let yaml: serde_yaml::Value = serde_yaml::from_slice(&bytes).map_err(|source| {
        RunError::Config(format!(
            "failed to canonicalise {}: {source}",
            path.display()
        ))
    })?;
    let canonical = serde_json::to_vec(&canonical_json(yaml)).map_err(|source| RunError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    let digest = Sha256::digest(canonical);
    Ok(format!("sha256:{}", hex_lower(&digest)))
}

fn canonical_json(value: serde_yaml::Value) -> serde_json::Value {
    match value {
        serde_yaml::Value::Null => serde_json::Value::Null,
        serde_yaml::Value::Bool(value) => serde_json::Value::Bool(value),
        serde_yaml::Value::Number(number) => number
            .as_i64()
            .map(serde_json::Number::from)
            .or_else(|| number.as_u64().map(serde_json::Number::from))
            .or_else(|| number.as_f64().and_then(serde_json::Number::from_f64))
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        serde_yaml::Value::String(value) => serde_json::Value::String(value),
        serde_yaml::Value::Sequence(values) => {
            serde_json::Value::Array(values.into_iter().map(canonical_json).collect())
        }
        serde_yaml::Value::Mapping(mapping) => {
            let mut object = serde_json::Map::new();
            let mut entries = BTreeMap::new();
            for (key, value) in mapping {
                entries.insert(yaml_key(key), canonical_json(value));
            }
            for (key, value) in entries {
                object.insert(key, value);
            }
            serde_json::Value::Object(object)
        }
        serde_yaml::Value::Tagged(tagged) => canonical_json(tagged.value),
    }
}

fn yaml_key(value: serde_yaml::Value) -> String {
    match value {
        serde_yaml::Value::String(value) => value,
        other => serde_json::to_string(&canonical_json(other)).unwrap_or_default(),
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ignored = write!(&mut out, "{byte:02x}");
    }
    out
}

fn read_manifest(path: &Path) -> Result<RunManifest, RunError> {
    let contents = read_to_string(path)?;
    serde_json::from_str(&contents).map_err(|source| RunError::Json {
        path: path.to_path_buf(),
        source,
    })
}

fn write_manifest(path: &Path, manifest: &RunManifest) -> Result<(), RunError> {
    write_file(
        path,
        &serde_json::to_string_pretty(manifest).map_err(|source| RunError::Json {
            path: path.to_path_buf(),
            source,
        })?,
    )
}

fn read_to_string(path: &Path) -> Result<String, RunError> {
    std::fs::read_to_string(path).map_err(|source| RunError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn write_file(path: &Path, contents: &str) -> Result<(), RunError> {
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
    }
    std::fs::write(path, contents).map_err(|source| RunError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn write_log(path: &Path, stdout: &str, stderr: &str) -> Result<(), RunError> {
    let mut contents = String::new();
    contents.push_str(stdout);
    contents.push_str(stderr);
    write_file(path, &contents)
}

fn append_log(path: &Path, text: &str) -> Result<(), RunError> {
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|source| RunError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    file.write_all(text.as_bytes())
        .map_err(|source| RunError::Io {
            path: path.to_path_buf(),
            source,
        })
}

fn create_dir_all(path: &Path) -> Result<(), RunError> {
    std::fs::create_dir_all(path).map_err(|source| RunError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn read_dir_names(path: &Path) -> Result<Vec<String>, RunError> {
    let entries = std::fs::read_dir(path).map_err(|source| RunError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| RunError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if entry
            .file_type()
            .map_err(|source| RunError::Io {
                path: entry.path(),
                source,
            })?
            .is_dir()
        {
            if let Some(name) = entry.file_name().to_str() {
                names.push(name.to_owned());
            }
        }
    }
    Ok(names)
}

fn parent(path: &Path) -> Result<&Path, RunError> {
    path.parent()
        .ok_or_else(|| RunError::Config(format!("path has no parent: {}", path.display())))
}

fn relative_to_root(repo_root: &Path, path: PathBuf) -> Result<PathBuf, RunError> {
    path.strip_prefix(repo_root)
        .map(Path::to_path_buf)
        .map_err(|error| RunError::Config(error.to_string()))
}

fn prior_feature_dir(steps: &[ManifestStep]) -> Option<PathBuf> {
    steps
        .iter()
        .flat_map(|step| step.artifacts.iter())
        .find_map(|artifact| {
            if artifact.ends_with("spec.md") {
                artifact.parent().map(Path::to_path_buf)
            } else {
                None
            }
        })
}

fn default_run_id() -> String {
    Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn required_step_text<'a>(
    value: Option<&'a str>,
    step_id: &str,
    field: &str,
) -> Result<&'a str, RunError> {
    value.ok_or_else(|| {
        RunError::Config(format!(
            "pipeline.{step_id}.{field}: missing required field"
        ))
    })
}

fn host_name(host: Host) -> &'static str {
    match host {
        Host::Claude => "claude",
        Host::Codex => "codex",
        Host::Copilot => "copilot",
    }
}

fn runner_name(runner: StepRunner) -> &'static str {
    match runner {
        StepRunner::Derrick => "derrick",
        StepRunner::Human => "human",
        StepRunner::Bash => "bash",
        StepRunner::Claude => "claude",
        StepRunner::Codex => "codex",
        StepRunner::Copilot => "copilot",
    }
}

fn reconcile_verdicts(
    outcomes: &[ReviewerOutcome],
    on_split: OnSplit,
    combined_path: &Path,
    repo_root: &Path,
) -> Result<StepExecution, RunError> {
    let all_accept = outcomes.iter().all(|o| o.verdict == "accept");
    let summary = outcomes
        .iter()
        .map(|o| format!("- {}: {}", o.role, o.verdict))
        .collect::<Vec<_>>()
        .join("\n");

    let final_verdict: &str = match on_split {
        OnSplit::Reject => {
            if all_accept {
                "accept"
            } else {
                "reject"
            }
        }
        OnSplit::Majority => {
            let accepts = outcomes.iter().filter(|o| o.verdict == "accept").count();
            let rejects = outcomes.iter().filter(|o| o.verdict != "accept").count();
            if accepts > rejects {
                "accept"
            } else {
                "reject"
            }
        }
        OnSplit::Human => {
            if all_accept {
                "accept"
            } else {
                let mut stdout = std::io::stdout();
                let _ = writeln!(stdout, "Reviewer split:");
                for o in outcomes {
                    let _ = writeln!(stdout, "  {}: {}", o.role, o.verdict);
                }
                let _ = write!(stdout, "Accept overall? [y/N] ");
                let _ = stdout.flush();
                let mut answer = String::new();
                std::io::stdin()
                    .read_line(&mut answer)
                    .map_err(|source| RunError::Io {
                        path: PathBuf::from("<stdin>"),
                        source,
                    })?;
                let trimmed = answer.trim();
                if trimmed.eq_ignore_ascii_case("y") || trimmed.eq_ignore_ascii_case("yes") {
                    "accept"
                } else {
                    "reject"
                }
            }
        }
    };

    let policy_name = match on_split {
        OnSplit::Reject => "reject",
        OnSplit::Human => "human",
        OnSplit::Majority => "majority",
    };
    let body = format!(
        "verdict: {final_verdict}\non_split: {policy_name}\nreviewers: {}\n\n{summary}\n",
        outcomes.len()
    );
    write_file(combined_path, &body)?;
    let combined_rel = relative_to_root(repo_root, combined_path.to_path_buf())?;
    let mut artifacts = vec![combined_rel];
    for o in outcomes {
        if let Ok(rel) = relative_to_root(repo_root, o.verdict_path.clone()) {
            artifacts.push(rel);
        }
    }

    let tokens_in: u32 = outcomes
        .iter()
        .map(|o| o.tokens_in)
        .fold(0u32, |a, b| a.saturating_add(b));
    let tokens_out: u32 = outcomes
        .iter()
        .map(|o| o.tokens_out)
        .fold(0u32, |a, b| a.saturating_add(b));

    match final_verdict {
        "accept" => Ok(StepExecution::success(artifacts).with_tokens(tokens_in, tokens_out)),
        _ => Ok(StepExecution::halted(
            artifacts,
            format!("assay rejected (on_split: {policy_name})"),
        )
        .with_tokens(tokens_in, tokens_out)),
    }
}

fn unreachable_verdict<T>(step_id: &str) -> Result<T, RunError> {
    Err(RunError::StepFailed {
        id: step_id.to_owned(),
        message: "unsupported verdict".to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use derrick_substrate_native::{NativeConfig, NativeSubstrate};
    use derrick_tools::{HostAdapter, HostResponse};
    use std::error::Error;
    use tempfile::{tempdir, TempDir};

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    struct StaticHost {
        name: &'static str,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl HostAdapter for StaticHost {
        fn name(&self) -> &str {
            self.name
        }

        fn is_available(&self) -> bool {
            true
        }

        async fn run(&self, request: HostRequest) -> Result<HostResponse, HostError> {
            if self.fail {
                return Err(HostError::NonZeroExit {
                    host: self.name.to_owned(),
                    exit_code: 7,
                    stderr: "failed".to_owned(),
                });
            }
            let feature = request.cwd.join("specs/001-test");
            std::fs::create_dir_all(feature.join("assay")).map_err(|source| HostError::Io {
                host: self.name.to_owned(),
                source,
            })?;
            std::fs::create_dir_all(request.cwd.join(".specify")).map_err(|source| {
                HostError::Io {
                    host: self.name.to_owned(),
                    source,
                }
            })?;
            if request.prompt.contains("speckit.specify") {
                std::fs::write(
                    request.cwd.join(FEATURE_JSON),
                    r#"{"feature_directory":"specs/001-test"}"#,
                )
                .map_err(|source| HostError::Io {
                    host: self.name.to_owned(),
                    source,
                })?;
                std::fs::write(feature.join("spec.md"), "spec").map_err(|source| {
                    HostError::Io {
                        host: self.name.to_owned(),
                        source,
                    }
                })?;
            } else if request.prompt.contains("speckit.plan") {
                std::fs::write(feature.join("plan.md"), "plan").map_err(|source| {
                    HostError::Io {
                        host: self.name.to_owned(),
                        source,
                    }
                })?;
            } else if request.prompt.contains("speckit.tasks") {
                std::fs::write(feature.join("tasks.md"), "tasks").map_err(|source| {
                    HostError::Io {
                        host: self.name.to_owned(),
                        source,
                    }
                })?;
            } else if request.prompt.contains("reviewer raised") {
                std::fs::write(feature.join("plan.md"), "\ndelta").map_err(|source| {
                    HostError::Io {
                        host: self.name.to_owned(),
                        source,
                    }
                })?;
            }
            Ok(HostResponse {
                stdout: "ok\n".to_owned(),
                stderr: String::new(),
                exit_code: 0,
                elapsed: Duration::from_millis(1),
            })
        }
    }

    async fn runner(yaml: &str) -> TestResult<(TempDir, Runner)> {
        let dir = tempdir()?;
        std::fs::write(dir.path().join("derrick.yaml"), yaml)?;
        std::fs::create_dir_all(dir.path().join(".specify/memory"))?;
        std::fs::create_dir_all(dir.path().join(".derrick"))?;
        std::fs::write(
            dir.path().join(".specify/memory/constitution.md"),
            "constitution",
        )?;
        let config = Config::load_from_path(&dir.path().join("derrick.yaml"))?;
        let substrate = NativeSubstrate::open(
            NativeConfig {
                db_path: dir.path().join(".derrick/derrick.db"),
                worktree_root: dir.path().join(".derrick/worktrees"),
            },
            config.site().clone(),
        )
        .await?;
        let mut hosts = HostRegistry::empty();
        hosts.register(
            "claude",
            Box::new(StaticHost {
                name: "claude",
                fail: false,
            }),
        );
        hosts.register(
            "copilot",
            Box::new(StaticHost {
                name: "copilot",
                fail: false,
            }),
        );
        let repo_root = dir.path().to_path_buf();
        Ok((
            dir,
            Runner::new(config, Arc::new(substrate), hosts, repo_root),
        ))
    }

    const YAML_MID: &str = r#"
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
pipeline:
"#;

    const YAML_TAIL: &str = r#"
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
"#;

    fn yaml(pipeline: &str, reviewer_cli: &Path) -> String {
        format!(
            "version: 1\nsite:\n  name: test\n  prefix: tst\nmodels:\n  shell-reviewer:\n    provider: shell\n    cli: \"{}\"\n    model: shell-reviewer\nroles:\n  drafter: shell-reviewer\n  proposer: shell-reviewer\n  reviewer: shell-reviewer{YAML_MID}{pipeline}{YAML_TAIL}",
            reviewer_cli.display()
        )
    }

    fn yaml_with_drafter(pipeline: &str, drafter_cli: &Path, reviewer_cli: &Path) -> String {
        format!(
            "version: 1\nsite:\n  name: test\n  prefix: tst\nmodels:\n  shell-drafter:\n    provider: shell\n    cli: \"{}\"\n    model: shell-drafter\n  shell-reviewer:\n    provider: shell\n    cli: \"{}\"\n    model: shell-reviewer\nroles:\n  drafter: shell-drafter\n  proposer: shell-drafter\n  reviewer: shell-reviewer{YAML_MID}{pipeline}{YAML_TAIL}",
            drafter_cli.display(),
            reviewer_cli.display()
        )
    }

    fn add_feature_pipeline() -> &'static str {
        r#"  - id: specify
    role: drafter
    host: claude
    command: "/speckit.specify {{prompt}}"
  - id: clarify
    runner: derrick
    skippable: true
  - id: plan
    role: proposer
    host: claude
    command: "/speckit.plan"
  - id: assay
    runner: derrick
    inputs: ["{{feature_dir}}/spec.md", "{{feature_dir}}/plan.md"]
    rounds: "{{tools.assay.rounds}}"
    skippable: true
  - id: tasks
    role: drafter
    host: claude
    command: "/speckit.tasks"
"#
    }

    fn reviewer_script(body: &str) -> TestResult<TempDir> {
        // All reviewer mocks drain stdin first so the shell provider's
        // stdin write doesn't SIGPIPE on Linux. Tests pass a body
        // assuming stdin has already been consumed.
        reviewer_script_raw(&format!(
            "#!/bin/sh\ncat > /dev/null\n{}",
            body.strip_prefix("#!/bin/sh\n").unwrap_or(body)
        ))
    }

    fn reviewer_script_raw(body: &str) -> TestResult<TempDir> {
        let dir = tempdir()?;
        let path = dir.path().join("reviewer");
        std::fs::write(&path, body)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(path, perms)?;
        }
        Ok(dir)
    }

    fn revise_then_accept_script() -> TestResult<TempDir> {
        let dir = tempdir()?;
        let path = dir.path().join("reviewer");
        let state = dir.path().join("round");
        std::fs::write(
            &path,
            format!(
                r#"#!/bin/sh
cat > /dev/null
state="{}"
if [ -f "$state" ]; then
  printf '## Verdict\naccept\n'
else
  printf seen > "$state"
  printf '## Suggested revisions\nonly objection\n## Verdict\nrevise\n'
fi
"#,
                state.display()
            ),
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(path, perms)?;
        }
        Ok(dir)
    }

    #[tokio::test]
    async fn add_feature_happy_path_writes_all_artifacts() -> TestResult {
        let reviewer = reviewer_script("#!/bin/sh\nprintf '## Verdict\\naccept\\n'")?;
        let (dir, runner) = runner(&yaml(
            add_feature_pipeline(),
            &reviewer.path().join("reviewer"),
        ))
        .await?;
        let outcome = runner
            .run_pipeline(
                ADD_FEATURE_PIPELINE,
                PipelineInput {
                    prompt: Some("test".to_owned()),
                    run_id: Some("run-1".to_owned()),
                    ..PipelineInput::default()
                },
            )
            .await?;

        assert_eq!(outcome.status, RunStatus::Success);
        assert!(dir.path().join("specs/001-test/spec.md").exists());
        assert!(dir.path().join("specs/001-test/plan.md").exists());
        assert!(dir.path().join("specs/001-test/tasks.md").exists());
        assert!(dir.path().join("specs/001-test/assay/verdict.md").exists());
        assert!(dir
            .path()
            .join(".derrick/runs/run-1/manifest.json")
            .exists());
        Ok(())
    }

    #[tokio::test]
    async fn no_assay_marks_assay_skipped() -> TestResult {
        let reviewer = reviewer_script("#!/bin/sh\nexit 9")?;
        let (_dir, runner) = runner(&yaml(
            add_feature_pipeline(),
            &reviewer.path().join("reviewer"),
        ))
        .await?;
        let mut skip = BTreeSet::new();
        skip.insert("assay".to_owned());
        let outcome = runner
            .run_pipeline(
                ADD_FEATURE_PIPELINE,
                PipelineInput {
                    prompt: Some("test".to_owned()),
                    skip,
                    run_id: Some("run-2".to_owned()),
                    ..PipelineInput::default()
                },
            )
            .await?;

        let assay = outcome
            .steps
            .iter()
            .find(|step| step.id == "assay")
            .ok_or("assay step should exist")?;
        assert_eq!(assay.status, StepStatus::Skipped);
        Ok(())
    }

    #[tokio::test]
    async fn default_skip_true_omits_step_by_default_and_unskip_runs() -> TestResult {
        let reviewer = reviewer_script("#!/bin/sh\nprintf '## Verdict\\naccept\\n'")?;
        let pipe = r#"  - id: specify
    role: drafter
    host: claude
    command: "/speckit.specify {{prompt}}"
  - id: optional
    runner: bash
    command: "printf ran > optional.txt"
    skippable: true
    default_skip: true
"#;
        let (dir, runner) = runner(&yaml(pipe, &reviewer.path().join("reviewer"))).await?;
        let first = runner
            .run_pipeline(
                ADD_FEATURE_PIPELINE,
                PipelineInput {
                    prompt: Some("test".to_owned()),
                    run_id: Some("default-skip".to_owned()),
                    ..PipelineInput::default()
                },
            )
            .await?;
        assert_eq!(first.steps[1].status, StepStatus::Skipped);
        assert!(!dir.path().join("optional.txt").exists());

        let second = runner
            .run_pipeline(
                ADD_FEATURE_PIPELINE,
                PipelineInput {
                    prompt: Some("test".to_owned()),
                    unskip: BTreeSet::from(["optional".to_owned()]),
                    run_id: Some("unskip".to_owned()),
                    ..PipelineInput::default()
                },
            )
            .await?;
        assert_eq!(second.steps[1].status, StepStatus::Success);
        assert!(dir.path().join("optional.txt").exists());
        Ok(())
    }

    #[tokio::test]
    async fn dry_run_halts_after_tasks_step() -> TestResult {
        let reviewer = reviewer_script("#!/bin/sh\nprintf '## Verdict\\naccept\\n'")?;
        let (_dir, runner) = runner(&yaml(
            add_feature_pipeline(),
            &reviewer.path().join("reviewer"),
        ))
        .await?;
        let outcome = runner
            .run_pipeline(
                ADD_FEATURE_PIPELINE,
                PipelineInput {
                    prompt: Some("test".to_owned()),
                    dry_run: true,
                    run_id: Some("run-3".to_owned()),
                    ..PipelineInput::default()
                },
            )
            .await?;

        assert_eq!(outcome.status, RunStatus::Halted);
        assert_eq!(
            outcome.steps.last().map(|step| step.id.as_str()),
            Some("tasks")
        );
        Ok(())
    }

    #[tokio::test]
    async fn unknown_pipeline_id_errors() -> TestResult {
        let reviewer = reviewer_script("#!/bin/sh\nprintf '## Verdict\\naccept\\n'")?;
        let (_dir, runner) = runner(&yaml(
            add_feature_pipeline(),
            &reviewer.path().join("reviewer"),
        ))
        .await?;

        let error = runner
            .run_pipeline("missing", PipelineInput::default())
            .await
            .err()
            .ok_or("unknown pipeline should error")?;

        assert!(matches!(error, RunError::UnknownPipeline(_)));
        Ok(())
    }

    #[tokio::test]
    async fn missing_prompt_for_add_feature_errors() -> TestResult {
        let reviewer = reviewer_script("#!/bin/sh\nprintf '## Verdict\\naccept\\n'")?;
        let (_dir, runner) = runner(&yaml(
            add_feature_pipeline(),
            &reviewer.path().join("reviewer"),
        ))
        .await?;

        let error = runner
            .run_pipeline(ADD_FEATURE_PIPELINE, PipelineInput::default())
            .await
            .err()
            .ok_or("missing prompt should error")?;

        assert!(matches!(error, RunError::MissingPrompt(_)));
        Ok(())
    }

    #[tokio::test]
    async fn rig_template_var_rejected() -> TestResult {
        let reviewer = reviewer_script("#!/bin/sh\nprintf '## Verdict\\naccept\\n'")?;
        let bad = r#"  - id: specify
    role: drafter
    host: claude
    command: "{{rig}}"
"#;
        let (_dir, runner) = runner(&yaml(bad, &reviewer.path().join("reviewer"))).await?;
        let error = runner
            .run_pipeline(
                ADD_FEATURE_PIPELINE,
                PipelineInput {
                    prompt: Some("test".to_owned()),
                    ..PipelineInput::default()
                },
            )
            .await
            .err()
            .ok_or("rig should error")?;

        assert!(error.to_string().contains("{{rig}}"));
        Ok(())
    }

    #[tokio::test]
    async fn runner_claude_codex_copilot_rejected_at_config_time() -> TestResult {
        let reviewer = reviewer_script("#!/bin/sh\nprintf '## Verdict\\naccept\\n'")?;
        let bad = r#"  - id: bad
    runner: claude
"#;
        let (_dir, runner) = runner(&yaml(bad, &reviewer.path().join("reviewer"))).await?;
        let error = runner
            .run_pipeline(
                ADD_FEATURE_PIPELINE,
                PipelineInput {
                    prompt: Some("test".to_owned()),
                    ..PipelineInput::default()
                },
            )
            .await
            .err()
            .ok_or("runner should error")?;

        assert!(error.to_string().contains("host: claude"));
        Ok(())
    }

    #[tokio::test]
    async fn parallel_group_accepted_in_config() -> TestResult {
        let reviewer = reviewer_script("#!/bin/sh\nprintf '## Verdict\\naccept\\n'")?;
        let pipe = r#"  - id: specify
    role: drafter
    host: claude
    command: "/speckit.specify {{prompt}}"
  - id: writeA
    runner: bash
    command: "printf a > a.txt"
    parallel_group: writing
  - id: writeB
    runner: bash
    command: "printf b > b.txt"
    parallel_group: writing
"#;
        let (dir, runner) = runner(&yaml(pipe, &reviewer.path().join("reviewer"))).await?;
        // Validation must accept parallel_group.
        let outcome = runner
            .run_pipeline(
                ADD_FEATURE_PIPELINE,
                PipelineInput {
                    prompt: Some("test".to_owned()),
                    run_id: Some("parallel-group".to_owned()),
                    ..PipelineInput::default()
                },
            )
            .await?;
        assert_eq!(outcome.status, RunStatus::Success);
        assert!(dir.path().join("a.txt").exists());
        assert!(dir.path().join("b.txt").exists());
        // Recognize that both grouped steps ran in the same group order.
        let ids: Vec<_> = outcome.steps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["specify", "writeA", "writeB"]);
        Ok(())
    }

    #[tokio::test]
    async fn skip_id_on_nonskippable_step_errors() -> TestResult {
        let reviewer = reviewer_script("#!/bin/sh\nprintf '## Verdict\\naccept\\n'")?;
        let (_dir, runner) = runner(&yaml(
            add_feature_pipeline(),
            &reviewer.path().join("reviewer"),
        ))
        .await?;
        let mut skip = BTreeSet::new();
        skip.insert("specify".to_owned());
        let error = runner
            .run_pipeline(
                ADD_FEATURE_PIPELINE,
                PipelineInput {
                    prompt: Some("test".to_owned()),
                    skip,
                    ..PipelineInput::default()
                },
            )
            .await
            .err()
            .ok_or("skip should error")?;

        assert!(error.to_string().contains("not skippable"));
        Ok(())
    }

    #[tokio::test]
    async fn assay_reject_halts_pipeline() -> TestResult {
        let reviewer = reviewer_script("#!/bin/sh\nprintf '## Verdict\\nreject\\n'")?;
        let (_dir, runner) = runner(&yaml(
            add_feature_pipeline(),
            &reviewer.path().join("reviewer"),
        ))
        .await?;
        let outcome = runner
            .run_pipeline(
                ADD_FEATURE_PIPELINE,
                PipelineInput {
                    prompt: Some("test".to_owned()),
                    run_id: Some("run-4".to_owned()),
                    ..PipelineInput::default()
                },
            )
            .await?;

        assert_eq!(outcome.status, RunStatus::Halted);
        assert_eq!(
            outcome.steps.last().map(|step| step.id.as_str()),
            Some("assay")
        );
        Ok(())
    }

    #[tokio::test]
    async fn assay_revise_then_accept_succeeds_after_replan() -> TestResult {
        let drafter = reviewer_script("#!/bin/sh\ncat > /dev/null\nprintf 'ok'")?;
        let reviewer = revise_then_accept_script()?;
        let rounds =
            add_feature_pipeline().replace("rounds: \"{{tools.assay.rounds}}\"", "rounds: 2");
        let (dir, runner) = runner(&yaml_with_drafter(
            &rounds,
            &drafter.path().join("reviewer"),
            &reviewer.path().join("reviewer"),
        ))
        .await?;

        let outcome = runner
            .run_pipeline(
                ADD_FEATURE_PIPELINE,
                PipelineInput {
                    prompt: Some("test".to_owned()),
                    run_id: Some("revise-accept".to_owned()),
                    ..PipelineInput::default()
                },
            )
            .await?;

        assert_eq!(outcome.status, RunStatus::Success);
        let plan = std::fs::read_to_string(dir.path().join("specs/001-test/plan.md"))?;
        assert!(plan.contains("delta"));
        Ok(())
    }

    #[tokio::test]
    async fn assay_revise_past_rounds_halts_pipeline() -> TestResult {
        let reviewer = reviewer_script(
            "#!/bin/sh\nprintf '## Suggested revisions\\nonly objection\\n## Verdict\\nrevise\\n'",
        )?;
        let (_dir, runner) = runner(&yaml(
            add_feature_pipeline(),
            &reviewer.path().join("reviewer"),
        ))
        .await?;
        let outcome = runner
            .run_pipeline(
                ADD_FEATURE_PIPELINE,
                PipelineInput {
                    prompt: Some("test".to_owned()),
                    run_id: Some("revise-halt".to_owned()),
                    ..PipelineInput::default()
                },
            )
            .await?;

        assert_eq!(outcome.status, RunStatus::Halted);
        Ok(())
    }

    #[tokio::test]
    async fn assay_unparsable_verdict_surfaces_step_failed() -> TestResult {
        // reviewer_script drains stdin via its #!/bin/sh wrapper.
        let reviewer = reviewer_script("printf 'no verdict\\n'")?;
        let (_dir, runner) = runner(&yaml(
            add_feature_pipeline(),
            &reviewer.path().join("reviewer"),
        ))
        .await?;
        let error = runner
            .run_pipeline(
                ADD_FEATURE_PIPELINE,
                PipelineInput {
                    prompt: Some("test".to_owned()),
                    ..PipelineInput::default()
                },
            )
            .await
            .err()
            .ok_or("bad verdict should error")?;

        assert!(error.to_string().contains("could not parse verdict"));
        Ok(())
    }

    #[tokio::test]
    async fn bash_runner_executes_and_captures_output() -> TestResult {
        let reviewer = reviewer_script("#!/bin/sh\nprintf '## Verdict\\naccept\\n'")?;
        let pipe = r#"  - id: shell
    runner: bash
    command: "printf '{{prompt}}'"
"#;
        let (dir, runner) = runner(&yaml(pipe, &reviewer.path().join("reviewer"))).await?;
        let outcome = runner
            .run_pipeline(
                ADD_FEATURE_PIPELINE,
                PipelineInput {
                    prompt: Some("hello".to_owned()),
                    run_id: Some("run-bash".to_owned()),
                    ..PipelineInput::default()
                },
            )
            .await?;

        assert_eq!(outcome.status, RunStatus::Success);
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".derrick/runs/run-bash/step-shell.log"))?,
            "hello"
        );
        Ok(())
    }

    #[tokio::test]
    async fn bash_runner_nonzero_exit_fails_step() -> TestResult {
        let reviewer = reviewer_script("#!/bin/sh\nprintf '## Verdict\\naccept\\n'")?;
        let pipe = r#"  - id: shell
    runner: bash
    command: "exit 7"
"#;
        let (_dir, runner) = runner(&yaml(pipe, &reviewer.path().join("reviewer"))).await?;
        let error = runner
            .run_pipeline(
                ADD_FEATURE_PIPELINE,
                PipelineInput {
                    prompt: Some("hello".to_owned()),
                    run_id: Some("bash-fail".to_owned()),
                    ..PipelineInput::default()
                },
            )
            .await
            .err()
            .ok_or("bash failure should error")?;

        assert!(error.to_string().contains("bash exited"));
        Ok(())
    }

    #[tokio::test]
    async fn bridge_and_foreman_are_no_ops_in_solo_mode() -> TestResult {
        let reviewer = reviewer_script("#!/bin/sh\nprintf '## Verdict\\naccept\\n'")?;
        let pipe = r#"  - id: bridge
    runner: derrick
  - id: foreman
    runner: derrick
"#;
        let (_dir, runner) = runner(&yaml(pipe, &reviewer.path().join("reviewer"))).await?;
        let outcome = runner
            .run_pipeline(
                ADD_FEATURE_PIPELINE,
                PipelineInput {
                    prompt: Some("hello".to_owned()),
                    run_id: Some("noop".to_owned()),
                    ..PipelineInput::default()
                },
            )
            .await?;

        assert_eq!(outcome.steps[0].status, StepStatus::Skipped);
        assert_eq!(outcome.steps[1].status, StepStatus::Skipped);
        Ok(())
    }

    #[tokio::test]
    async fn role_without_host_uses_model_completion() -> TestResult {
        let reviewer = reviewer_script("#!/bin/sh\ncat >/dev/null\nprintf 'model response'")?;
        let pipe = r#"  - id: model
    role: reviewer
    command: "hello {{site_name}}"
"#;
        let (dir, runner) = runner(&yaml(pipe, &reviewer.path().join("reviewer"))).await?;
        let outcome = runner
            .run_pipeline(
                ADD_FEATURE_PIPELINE,
                PipelineInput {
                    prompt: Some("hello".to_owned()),
                    run_id: Some("model".to_owned()),
                    ..PipelineInput::default()
                },
            )
            .await?;

        assert_eq!(outcome.status, RunStatus::Success);
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".derrick/runs/model/step-model.log"))?,
            "model response"
        );
        Ok(())
    }

    #[tokio::test]
    async fn resume_from_step_skips_earlier_steps() -> TestResult {
        let reviewer = reviewer_script("#!/bin/sh\nprintf '## Verdict\\naccept\\n'")?;
        let (_dir, runner) = runner(&yaml(
            add_feature_pipeline(),
            &reviewer.path().join("reviewer"),
        ))
        .await?;
        runner
            .run_pipeline(
                ADD_FEATURE_PIPELINE,
                PipelineInput {
                    prompt: Some("test".to_owned()),
                    skip: BTreeSet::from(["assay".to_owned()]),
                    run_id: Some("resume-ok".to_owned()),
                    ..PipelineInput::default()
                },
            )
            .await?;
        let outcome = runner.resume(Some("resume-ok"), "tasks").await?;

        assert_eq!(
            outcome.steps.last().map(|step| step.id.as_str()),
            Some("tasks")
        );
        assert_eq!(outcome.status, RunStatus::Success);
        Ok(())
    }

    #[tokio::test]
    async fn on_failure_poll_interval_and_input_template_errors_are_rejected() -> TestResult {
        let reviewer = reviewer_script("#!/bin/sh\nprintf '## Verdict\\naccept\\n'")?;
        for field in [
            "on_failure: retry",
            "poll_interval: 1s",
            "inputs: [\"{{unknown}}\"]",
        ] {
            let pipe = format!(
                r#"  - id: bad
    role: drafter
    host: claude
    command: ok
    {field}
"#
            );
            let (_dir, runner) = runner(&yaml(&pipe, &reviewer.path().join("reviewer"))).await?;
            let error = runner
                .run_pipeline(
                    ADD_FEATURE_PIPELINE,
                    PipelineInput {
                        prompt: Some("test".to_owned()),
                        ..PipelineInput::default()
                    },
                )
                .await
                .err()
                .ok_or("invalid field should error")?;
            assert!(matches!(error, RunError::Config(_)));
        }
        Ok(())
    }

    #[tokio::test]
    async fn resume_refuses_when_config_hash_mismatches() -> TestResult {
        let reviewer = reviewer_script("#!/bin/sh\nprintf '## Verdict\\naccept\\n'")?;
        let (dir, runner) = runner(&yaml(
            add_feature_pipeline(),
            &reviewer.path().join("reviewer"),
        ))
        .await?;
        runner
            .run_pipeline(
                ADD_FEATURE_PIPELINE,
                PipelineInput {
                    prompt: Some("test".to_owned()),
                    skip: BTreeSet::from(["assay".to_owned()]),
                    run_id: Some("resume-run".to_owned()),
                    ..PipelineInput::default()
                },
            )
            .await?;
        let path = dir.path().join("derrick.yaml");
        let contents = std::fs::read_to_string(&path)?;
        std::fs::write(path, contents.replacen("name: test", "name: drift", 1))?;

        let error = runner
            .resume(Some("resume-run"), "tasks")
            .await
            .err()
            .ok_or("resume should refuse drift")?;

        assert!(error.to_string().contains("config has changed"));
        Ok(())
    }

    #[tokio::test]
    async fn specify_step_pre_creates_specify_features_dir() -> TestResult {
        // D36: the runner must create `.specify/features/` before invoking
        // the specify host so the host's writes do not block on a permission
        // prompt the headless subprocess cannot answer.
        use std::sync::atomic::{AtomicBool, Ordering};

        struct ProbeHost {
            dir_existed_at_invocation: Arc<AtomicBool>,
        }

        #[async_trait::async_trait]
        impl HostAdapter for ProbeHost {
            fn name(&self) -> &str {
                "claude"
            }
            fn is_available(&self) -> bool {
                true
            }
            async fn run(&self, request: HostRequest) -> Result<HostResponse, HostError> {
                let exists = request.cwd.join(".specify/features").is_dir();
                self.dir_existed_at_invocation
                    .store(exists, Ordering::SeqCst);
                // Still emit the feature.json so downstream steps don't fail.
                let feature = request.cwd.join("specs/001-test");
                std::fs::create_dir_all(&feature).map_err(|source| HostError::Io {
                    host: "claude".to_owned(),
                    source,
                })?;
                std::fs::write(
                    request.cwd.join(FEATURE_JSON),
                    r#"{"feature_directory":"specs/001-test"}"#,
                )
                .map_err(|source| HostError::Io {
                    host: "claude".to_owned(),
                    source,
                })?;
                std::fs::write(feature.join("spec.md"), "spec").map_err(|source| {
                    HostError::Io {
                        host: "claude".to_owned(),
                        source,
                    }
                })?;
                Ok(HostResponse {
                    stdout: "ok\n".to_owned(),
                    stderr: String::new(),
                    exit_code: 0,
                    elapsed: Duration::from_millis(1),
                })
            }
        }

        let dir = tempdir()?;
        // Note: deliberately do NOT create `.specify/features` up front. The
        // surrounding `.specify/memory/constitution.md` fixture only creates
        // `.specify/memory/`, so the features subdirectory is absent until
        // the runner pre-creates it.
        std::fs::write(
            dir.path().join("derrick.yaml"),
            yaml(
                r#"  - id: specify
    role: drafter
    host: claude
    command: "/speckit.specify {{prompt}}"
"#,
                Path::new("/nonexistent-reviewer"),
            ),
        )?;
        std::fs::create_dir_all(dir.path().join(".specify/memory"))?;
        std::fs::create_dir_all(dir.path().join(".derrick"))?;
        std::fs::write(
            dir.path().join(".specify/memory/constitution.md"),
            "constitution",
        )?;
        let config = Config::load_from_path(&dir.path().join("derrick.yaml"))?;
        let substrate = NativeSubstrate::open(
            NativeConfig {
                db_path: dir.path().join(".derrick/derrick.db"),
                worktree_root: dir.path().join(".derrick/worktrees"),
            },
            config.site().clone(),
        )
        .await?;
        let flag = Arc::new(AtomicBool::new(false));
        let mut hosts = HostRegistry::empty();
        hosts.register(
            "claude",
            Box::new(ProbeHost {
                dir_existed_at_invocation: Arc::clone(&flag),
            }),
        );
        let runner = Runner::new(config, Arc::new(substrate), hosts, dir.path().to_path_buf());
        runner
            .run_pipeline(
                ADD_FEATURE_PIPELINE,
                PipelineInput {
                    prompt: Some("test".to_owned()),
                    run_id: Some("specify-precreate".to_owned()),
                    ..PipelineInput::default()
                },
            )
            .await?;

        assert!(
            flag.load(Ordering::SeqCst),
            ".specify/features must exist before the specify host runs"
        );
        assert!(dir.path().join(".specify/features").is_dir());
        Ok(())
    }

    #[tokio::test]
    async fn assay_falls_back_to_claude_when_codex_and_no_tty() -> TestResult {
        // D37: when stdin is not a TTY and the reviewer role resolves to a
        // codex-family model, the runner must call the claude host instead
        // of spawning codex (which would abort with "stdin is not a
        // terminal").
        //
        // Tests run under cargo without a TTY so `IsTerminal` already
        // returns false for stdin; we only need to point the reviewer role
        // at a codex-cli model and assert the claude host produced the
        // verdict.
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct AssayClaudeHost {
            calls: Arc<AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl HostAdapter for AssayClaudeHost {
            fn name(&self) -> &str {
                "claude"
            }
            fn is_available(&self) -> bool {
                true
            }
            async fn run(&self, request: HostRequest) -> Result<HostResponse, HostError> {
                // First call is the `/speckit.specify` write. Second is
                // /speckit.plan. Third is the assay fallback.
                let count = self.calls.fetch_add(1, Ordering::SeqCst);
                let feature = request.cwd.join("specs/001-test");
                std::fs::create_dir_all(feature.join("assay")).map_err(|source| HostError::Io {
                    host: "claude".to_owned(),
                    source,
                })?;
                if request.prompt.contains("speckit.specify") {
                    std::fs::write(
                        request.cwd.join(FEATURE_JSON),
                        r#"{"feature_directory":"specs/001-test"}"#,
                    )
                    .map_err(|source| HostError::Io {
                        host: "claude".to_owned(),
                        source,
                    })?;
                    std::fs::write(feature.join("spec.md"), "spec").map_err(|source| {
                        HostError::Io {
                            host: "claude".to_owned(),
                            source,
                        }
                    })?;
                } else if request.prompt.contains("speckit.plan") {
                    std::fs::write(feature.join("plan.md"), "plan").map_err(|source| {
                        HostError::Io {
                            host: "claude".to_owned(),
                            source,
                        }
                    })?;
                } else if request.prompt.contains("speckit.tasks") {
                    std::fs::write(feature.join("tasks.md"), "tasks").map_err(|source| {
                        HostError::Io {
                            host: "claude".to_owned(),
                            source,
                        }
                    })?;
                }
                let stdout =
                    if request.prompt.contains("Verdict") || request.prompt.contains("Plan:") {
                        // The assay system prompt includes "## Verdict" verbatim;
                        // recognize the assay call and emit an accept verdict.
                        "## Verdict\naccept\n".to_owned()
                    } else {
                        "ok\n".to_owned()
                    };
                let _ = count;
                Ok(HostResponse {
                    stdout,
                    stderr: String::new(),
                    exit_code: 0,
                    elapsed: Duration::from_millis(1),
                })
            }
        }

        let dir = tempdir()?;
        // The `codex` model definition points at a non-existent cli on
        // purpose: detect_codex_fallback inspects the config, not the
        // process, so the model is never spawned.
        let yaml = r#"
version: 1
site:
  name: test
  prefix: tst
models:
  codex:
    provider: shell
    cli: "codex exec"
    model: codex
roles:
  drafter: codex
  proposer: codex
  reviewer: codex
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
pipeline:
  - id: specify
    role: drafter
    host: claude
    command: "/speckit.specify {{prompt}}"
  - id: plan
    role: proposer
    host: claude
    command: "/speckit.plan"
  - id: assay
    runner: derrick
    rounds: "{{tools.assay.rounds}}"
    skippable: true
  - id: tasks
    role: drafter
    host: claude
    command: "/speckit.tasks"
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
"#;
        std::fs::write(dir.path().join("derrick.yaml"), yaml)?;
        std::fs::create_dir_all(dir.path().join(".specify/memory"))?;
        std::fs::create_dir_all(dir.path().join(".derrick"))?;
        std::fs::write(
            dir.path().join(".specify/memory/constitution.md"),
            "constitution",
        )?;
        let config = Config::load_from_path(&dir.path().join("derrick.yaml"))?;
        let substrate = NativeSubstrate::open(
            NativeConfig {
                db_path: dir.path().join(".derrick/derrick.db"),
                worktree_root: dir.path().join(".derrick/worktrees"),
            },
            config.site().clone(),
        )
        .await?;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut hosts = HostRegistry::empty();
        hosts.register(
            "claude",
            Box::new(AssayClaudeHost {
                calls: Arc::clone(&calls),
            }),
        );
        let runner = Runner::new(config, Arc::new(substrate), hosts, dir.path().to_path_buf());
        let outcome = runner
            .run_pipeline(
                ADD_FEATURE_PIPELINE,
                PipelineInput {
                    prompt: Some("test".to_owned()),
                    run_id: Some("codex-fallback".to_owned()),
                    ..PipelineInput::default()
                },
            )
            .await?;

        assert_eq!(outcome.status, RunStatus::Success);
        let assay = outcome
            .steps
            .iter()
            .find(|step| step.id == "assay")
            .ok_or("assay step should exist")?;
        assert_eq!(assay.status, StepStatus::Success);
        // Verdict file recorded the fallback model name.
        let verdict = std::fs::read_to_string(dir.path().join("specs/001-test/assay/verdict.md"))?;
        assert!(
            verdict.contains("model: claude"),
            "verdict.md should record the claude fallback, got: {verdict}"
        );
        Ok(())
    }

    #[test]
    fn suggested_revisions_extracts_only_block() -> TestResult {
        let text = "## Risks\nfull prompt\n## Suggested revisions\nonly this\n## Verdict\nrevise\n";
        assert_eq!(suggested_revisions(text), Some("only this"));
        Ok(())
    }

    fn multi_reviewer_yaml(reviewer_clis: &[(&str, &Path)], on_split: &str) -> String {
        // Build a config with N reviewer roles, each mapped to its own shell
        // model. The pipeline runs specify → plan → assay → tasks. The assay
        // step uses the configured reviewer list and on_split policy.
        let mut models = String::new();
        let mut roles = String::from(
            "  drafter: shell-drafter\n  proposer: shell-drafter\n  reviewer: shell-drafter\n",
        );
        let mut reviewer_list = String::new();
        // drafter model that always accepts (used by specify/plan/tasks bodies are
        // produced by StaticHost; the model is only used if claude host fallback
        // is hit, which it isn't in these tests).
        models.push_str(
            "  shell-drafter:\n    provider: shell\n    cli: \"/bin/echo\"\n    model: shell-drafter\n",
        );
        for (role, cli) in reviewer_clis {
            let model_name = format!("model-{role}");
            models.push_str(&format!(
                "  {model_name}:\n    provider: shell\n    cli: \"{}\"\n    model: {model_name}\n",
                cli.display()
            ));
            roles.push_str(&format!("  {role}: {model_name}\n"));
            if !reviewer_list.is_empty() {
                reviewer_list.push_str(", ");
            }
            reviewer_list.push_str(role);
        }
        format!(
            r#"
version: 1
site:
  name: test
  prefix: tst
models:
{models}roles:
{roles}tools:
  speckit:
    enabled: true
    version: ">=0.4.0"
  assay:
    enabled: true
    role: reviewer
    reviewers: [{reviewer_list}]
    rounds: 1
    on_split: {on_split}
  substrate:
    backend: native
    mode: solo
  copilot:
    enabled: false
    agent_identity: derrick-hand
pipeline:
  - id: specify
    role: drafter
    host: claude
    command: "/speckit.specify {{{{prompt}}}}"
  - id: plan
    role: proposer
    host: claude
    command: "/speckit.plan"
  - id: assay
    runner: derrick
    rounds: "1"
    skippable: true
  - id: tasks
    role: drafter
    host: claude
    command: "/speckit.tasks"
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

    async fn multi_reviewer_runner(yaml: &str) -> TestResult<(TempDir, Runner)> {
        let dir = tempdir()?;
        std::fs::write(dir.path().join("derrick.yaml"), yaml)?;
        std::fs::create_dir_all(dir.path().join(".specify/memory"))?;
        std::fs::create_dir_all(dir.path().join(".derrick"))?;
        std::fs::write(
            dir.path().join(".specify/memory/constitution.md"),
            "constitution",
        )?;
        let config = Config::load_from_path(&dir.path().join("derrick.yaml"))?;
        let substrate = NativeSubstrate::open(
            NativeConfig {
                db_path: dir.path().join(".derrick/derrick.db"),
                worktree_root: dir.path().join(".derrick/worktrees"),
            },
            config.site().clone(),
        )
        .await?;
        let mut hosts = HostRegistry::empty();
        hosts.register(
            "claude",
            Box::new(StaticHost {
                name: "claude",
                fail: false,
            }),
        );
        let repo_root = dir.path().to_path_buf();
        Ok((
            dir,
            Runner::new(config, Arc::new(substrate), hosts, repo_root),
        ))
    }

    #[tokio::test]
    async fn multi_reviewer_all_accept() -> TestResult {
        let accept_a = reviewer_script("#!/bin/sh\nprintf '## Verdict\\naccept\\n'")?;
        let accept_b = reviewer_script("#!/bin/sh\nprintf '## Verdict\\naccept\\n'")?;
        let yaml = multi_reviewer_yaml(
            &[
                ("reviewer_a", &accept_a.path().join("reviewer")),
                ("reviewer_b", &accept_b.path().join("reviewer")),
            ],
            "reject",
        );
        let (dir, runner) = multi_reviewer_runner(&yaml).await?;
        let outcome = runner
            .run_pipeline(
                ADD_FEATURE_PIPELINE,
                PipelineInput {
                    prompt: Some("test".to_owned()),
                    run_id: Some("multi-accept".to_owned()),
                    ..PipelineInput::default()
                },
            )
            .await?;
        assert_eq!(outcome.status, RunStatus::Success);
        let assay = outcome
            .steps
            .iter()
            .find(|s| s.id == "assay")
            .ok_or("assay step should exist")?;
        assert_eq!(assay.status, StepStatus::Success);
        let verdict = std::fs::read_to_string(dir.path().join("specs/001-test/assay/verdict.md"))?;
        assert!(verdict.contains("verdict: accept"), "got: {verdict}");
        assert!(verdict.contains("reviewer_a: accept"), "got: {verdict}");
        assert!(verdict.contains("reviewer_b: accept"), "got: {verdict}");
        Ok(())
    }

    #[tokio::test]
    async fn multi_reviewer_on_split_reject() -> TestResult {
        let accept = reviewer_script("#!/bin/sh\nprintf '## Verdict\\naccept\\n'")?;
        let reject = reviewer_script("#!/bin/sh\nprintf '## Verdict\\nreject\\n'")?;
        let yaml = multi_reviewer_yaml(
            &[
                ("reviewer_a", &accept.path().join("reviewer")),
                ("reviewer_b", &reject.path().join("reviewer")),
            ],
            "reject",
        );
        let (dir, runner) = multi_reviewer_runner(&yaml).await?;
        let outcome = runner
            .run_pipeline(
                ADD_FEATURE_PIPELINE,
                PipelineInput {
                    prompt: Some("test".to_owned()),
                    run_id: Some("multi-reject".to_owned()),
                    ..PipelineInput::default()
                },
            )
            .await?;
        assert_eq!(outcome.status, RunStatus::Halted);
        let assay = outcome
            .steps
            .iter()
            .find(|s| s.id == "assay")
            .ok_or("assay step should exist")?;
        assert_eq!(assay.status, StepStatus::Halted);
        let verdict = std::fs::read_to_string(dir.path().join("specs/001-test/assay/verdict.md"))?;
        assert!(verdict.contains("verdict: reject"), "got: {verdict}");
        assert!(verdict.contains("on_split: reject"), "got: {verdict}");
        Ok(())
    }

    #[tokio::test]
    async fn multi_reviewer_on_split_majority() -> TestResult {
        let a = reviewer_script("#!/bin/sh\nprintf '## Verdict\\naccept\\n'")?;
        let b = reviewer_script("#!/bin/sh\nprintf '## Verdict\\naccept\\n'")?;
        let c = reviewer_script("#!/bin/sh\nprintf '## Verdict\\nreject\\n'")?;
        let yaml = multi_reviewer_yaml(
            &[
                ("reviewer_a", &a.path().join("reviewer")),
                ("reviewer_b", &b.path().join("reviewer")),
                ("reviewer_c", &c.path().join("reviewer")),
            ],
            "majority",
        );
        let (dir, runner) = multi_reviewer_runner(&yaml).await?;
        let outcome = runner
            .run_pipeline(
                ADD_FEATURE_PIPELINE,
                PipelineInput {
                    prompt: Some("test".to_owned()),
                    run_id: Some("multi-majority".to_owned()),
                    ..PipelineInput::default()
                },
            )
            .await?;
        assert_eq!(outcome.status, RunStatus::Success);
        let verdict = std::fs::read_to_string(dir.path().join("specs/001-test/assay/verdict.md"))?;
        assert!(verdict.contains("verdict: accept"), "got: {verdict}");
        assert!(verdict.contains("on_split: majority"), "got: {verdict}");
        Ok(())
    }
}
