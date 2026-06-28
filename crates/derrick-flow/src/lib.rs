//! Pipeline orchestrator. See DESIGN.md §5.3 and §10.

mod clarify;
mod code_review;
mod manifest;
mod progress;
mod runner;
pub mod spec_provider;
mod steps;

pub use code_review::{CodeReviewOutcome, run_code_review};
pub use derrick_assay::types::{
    PipelineInput, RunError, RunOutcome, RunStatus, StepRecord, StepStatus,
};
pub use manifest::compute_prompt_key;
pub use progress::{NoopReporter, ProgressReporter, RunProgress, StepProgress};
pub use runner::Runner;
pub use spec_provider::{SpecPhase, SpecPhaseCtx, run_spec_phase};
pub use steps::hand_kind_for_executor;

/// Re-export of the shared run/step types crate. Existing call sites that
/// reach for `derrick_flow::types::*` keep working.
pub use derrick_assay::types;

#[cfg(test)]
mod code_review_tests {
    use crate::code_review::extract_verdict_from_review;

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

#[cfg(test)]
mod tests {
    use crate::clarify::{
        ClarifyQuestion, parse_clarify_questions, render_clarify_markdown, select_clarify_answer,
    };
    use crate::runner::Runner;
    use derrick_assay::ExecutionState;
    use derrick_assay::io::FEATURE_JSON;
    use derrick_assay::types::{PipelineInput, RunError, RunStatus, StepStatus};
    use derrick_config::Config;
    use derrick_substrate_native::{NativeConfig, NativeSubstrate};
    use derrick_tools::{HostAdapter, HostError, HostRegistry, HostRequest, HostResponse};
    use std::error::Error;
    use std::path::PathBuf;
    use std::time::Duration;
    use tempfile::{TempDir, tempdir};

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    const DRILL_PIPELINE: &str = "drill";

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
                    stdout: String::new(),
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
                std::fs::write(
                    feature.join("tasks.md"),
                    r#"## Task one
Description of task one.

## Task two
Description of task two.

## Task three
Description of task three.
"#,
                )
                .map_err(|source| HostError::Io {
                    host: self.name.to_owned(),
                    source,
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
                tokens_in: 0,
                tokens_out: 0,
                pid: None,
            })
        }
    }

    /// Host adapter that records the `HostRequest.model` it was handed so a
    /// test can assert the pipeline forwarded the role's configured model
    /// (D66, Part A). Does not normalise — that is the adapter's job (D65).
    struct ModelCapturingHost {
        name: &'static str,
        captured_model: std::sync::Arc<std::sync::Mutex<Option<Option<String>>>>,
    }

    #[async_trait::async_trait]
    impl HostAdapter for ModelCapturingHost {
        fn name(&self) -> &str {
            self.name
        }

        fn is_available(&self) -> bool {
            true
        }

        async fn run(&self, request: HostRequest) -> Result<HostResponse, HostError> {
            *self.captured_model.lock().expect("lock") = Some(request.model.clone());
            Ok(HostResponse {
                stdout: "ok\n".to_owned(),
                stderr: String::new(),
                exit_code: 0,
                elapsed: Duration::from_millis(1),
                tokens_in: 0,
                tokens_out: 0,
                pid: None,
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

    const YAML_MID_CREW: &str = r#"
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
    mode: crew
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

    fn yaml_crew(pipeline: &str, reviewer_cli: &Path) -> String {
        format!(
            "version: 1\nsite:\n  name: test\n  prefix: tst\nmodels:\n  shell-reviewer:\n    provider: shell\n    cli: \"{}\"\n    model: shell-reviewer\nroles:\n  drafter: shell-reviewer\n  proposer: shell-reviewer\n  reviewer: shell-reviewer\n  executor: shell-reviewer{YAML_MID_CREW}{pipeline}{YAML_TAIL}",
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

    fn drill_pipeline() -> &'static str {
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

    use std::collections::BTreeSet;
    use std::path::Path;
    use std::sync::Arc;

    #[tokio::test]
    async fn host_step_forwards_role_model_to_request() -> TestResult {
        // D66 Part A: a `host:` step bound to a role must populate
        // `HostRequest.model` with the RAW configured model id so the
        // adapter can normalise + pass `--model`.
        let dir = tempdir()?;
        let yaml = format!(
            "version: 1\nsite:\n  name: test\n  prefix: tst\nmodels:\n  opus:\n    provider: claude\n    model: claude-opus-4-8\nroles:\n  proposer: opus\n  reviewer: opus{YAML_MID}  - id: probe\n    role: proposer\n    host: claude\n    command: \"hello {{{{prompt}}}}\"\n{YAML_TAIL}",
        );
        std::fs::write(dir.path().join("derrick.yaml"), &yaml)?;
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

        let captured = Arc::new(std::sync::Mutex::new(None));
        let mut hosts = HostRegistry::empty();
        hosts.register(
            "claude",
            Box::new(ModelCapturingHost {
                name: "claude",
                captured_model: captured.clone(),
            }),
        );

        let run_dir = dir.path().join(".derrick/runs/run-1");
        std::fs::create_dir_all(&run_dir)?;
        let manifest_path = run_dir.join("manifest.json");
        let mut state = ExecutionState::new("do the thing".to_owned(), "run-1".to_owned(), run_dir);

        let step = config
            .pipeline()
            .iter()
            .find(|s| s.id() == "probe")
            .expect("probe step")
            .clone();

        crate::steps::execute_step(
            &config,
            &substrate,
            Arc::new(hosts),
            dir.path(),
            &step,
            &mut state,
            "run-1",
            &manifest_path,
            None,
        )
        .await?;

        let model = captured.lock().expect("lock").clone();
        assert_eq!(
            model,
            Some(Some("claude-opus-4-8".to_owned())),
            "host step must forward the role's configured model id, raw"
        );
        Ok(())
    }

    fn drill_pipeline_with_dispatch() -> String {
        format!(
            "{}\n  - id: bridge\n    runner: derrick\n    inputs: [\"{{{{feature_dir}}}}/tasks.md\"]\n    batch: \"br-{{{{run_id}}}}\"\n  - id: foreman\n    runner: derrick\n",
            drill_pipeline()
        )
    }

    #[tokio::test]
    async fn bridge_creates_tickets_from_tasks() -> TestResult {
        let reviewer = reviewer_script("#!/bin/sh\nprintf '## Verdict\\naccept\\n'")?;
        let (dir, runner) = runner(&yaml_crew(
            &drill_pipeline_with_dispatch(),
            &reviewer.path().join("reviewer"),
        ))
        .await?;
        let outcome = runner
            .run_pipeline(
                DRILL_PIPELINE,
                PipelineInput {
                    prompt: Some("test".to_owned()),
                    run_id: Some("bridge-test".to_owned()),
                    ..PipelineInput::default()
                },
            )
            .await?;

        assert_eq!(outcome.status, RunStatus::Success);
        let db_path = dir.path().join(".derrick/derrick.db");
        assert!(
            db_path.exists(),
            "substrate database should exist after bridge"
        );

        // Check tasks.md was written by the mock
        let tasks_md_path = dir.path().join("specs/001-test/tasks.md");
        assert!(
            tasks_md_path.exists(),
            "tasks.md should exist at {tasks_md_path:?}"
        );

        // Debug: search for tasks.md elsewhere
        if !tasks_md_path.exists() {
            let worktree_base = dir.path().join(".derrick/worktrees");
            if worktree_base.exists() {
                for entry in std::fs::read_dir(&worktree_base)? {
                    let entry = entry?;
                    let wt_tasks = entry.path().join("specs/001-test/tasks.md");
                    if wt_tasks.exists() {
                        panic!("tasks.md found in worktree instead: {:?}", wt_tasks);
                    }
                }
            }
        }

        // Verify tickets were created in the database
        let conn = rusqlite::Connection::open(&db_path)?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM tickets WHERE batch = 'br-bridge-test'",
            [],
            |row| row.get(0),
        )?;
        assert!(
            count >= 3,
            "expected at least 3 tickets from tasks.md, got {count}"
        );

        // Verify at least one ticket was dispatched by foreman
        let dispatched: i64 = conn.query_row(
            "SELECT COUNT(*) FROM events WHERE kind = 'ticket_assigned'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(dispatched, 3, "expected 3 tickets to be dispatched");
        Ok(())
    }

    // ---- bridge auto-remediation tests ----

    /// Pipeline with bridge only (no foreman) — tickets stay in `ready` state
    /// after bridge, making it easy to move them to terminal states for tests.
    fn drill_pipeline_bridge_only() -> String {
        format!(
            "{}\n  - id: bridge\n    runner: derrick\n    inputs: [\"{{{{feature_dir}}}}/tasks.md\"]\n    batch: \"br-{{{{run_id}}}}\"\n",
            drill_pipeline()
        )
    }

    #[tokio::test]
    async fn bridge_recreates_tickets_that_are_terminal() -> TestResult {
        let reviewer = reviewer_script("#!/bin/sh\nprintf '## Verdict\\naccept\\n'")?;
        // Use a pipeline WITHOUT foreman so tickets stay in `ready` (not in_flight)
        // after run 1, allowing us to mark them done via the substrate API.
        let (dir, runner) = runner(&yaml_crew(
            &drill_pipeline_bridge_only(),
            &reviewer.path().join("reviewer"),
        ))
        .await?;

        // Run 1: creates tst-0 … tst-2 in `ready` state.
        let outcome1 = runner
            .run_pipeline(
                DRILL_PIPELINE,
                PipelineInput {
                    prompt: Some("test".to_owned()),
                    run_id: Some("remediate-run-1".to_owned()),
                    ..PipelineInput::default()
                },
            )
            .await?;
        assert_eq!(outcome1.status, RunStatus::Success);

        // Mark tickets as `done` via the substrate API (mark_ticket_done_manually
        // works on any non-terminal ticket; they are currently `ready`).
        use derrick_substrate::{ManualDoneAttestation, Substrate, TicketId};
        let db_path = dir.path().join(".derrick/derrick.db");
        {
            let config = Config::load_from_path(&dir.path().join("derrick.yaml"))?;
            let substrate_for_setup = NativeSubstrate::open(
                NativeConfig {
                    db_path: db_path.clone(),
                    worktree_root: dir.path().join(".derrick/worktrees"),
                },
                config.site().clone(),
            )
            .await?;
            for id_str in &["tst-0", "tst-1", "tst-2"] {
                let id = TicketId::new(*id_str)?;
                substrate_for_setup
                    .mark_ticket_done_manually(
                        &id,
                        ManualDoneAttestation {
                            claimant: "test-automation".to_owned(),
                            note: "terminal for re-dispatch test".to_owned(),
                        },
                    )
                    .await?;
            }
            let conn = rusqlite::Connection::open(&db_path)?;
            let done_count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM tickets WHERE state = 'done'",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(done_count, 3, "pre-condition: all 3 tickets should be done");
        }

        // Run 2: bridge should detect terminal tickets, delete and recreate them.
        let outcome2 = runner
            .run_pipeline(
                DRILL_PIPELINE,
                PipelineInput {
                    prompt: Some("test".to_owned()),
                    run_id: Some("remediate-run-2".to_owned()),
                    ..PipelineInput::default()
                },
            )
            .await?;
        assert_eq!(outcome2.status, RunStatus::Success);

        // All 3 tickets should be back in `ready` state.
        let conn = rusqlite::Connection::open(&db_path)?;
        let ready_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM tickets WHERE state = 'ready'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(
            ready_count, 3,
            "tickets from run-2 should be recreated as ready; got {ready_count}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn bridge_skips_tickets_that_are_active() -> TestResult {
        let reviewer = reviewer_script("#!/bin/sh\nprintf '## Verdict\\naccept\\n'")?;
        let (dir, runner) = runner(&yaml_crew(
            &drill_pipeline_with_dispatch(),
            &reviewer.path().join("reviewer"),
        ))
        .await?;

        // Run 1: creates tst-0 … tst-2 in `ready` state, then foreman dispatches
        // them → `in_flight`.
        let outcome1 = runner
            .run_pipeline(
                DRILL_PIPELINE,
                PipelineInput {
                    prompt: Some("test".to_owned()),
                    run_id: Some("skip-run-1".to_owned()),
                    ..PipelineInput::default()
                },
            )
            .await?;
        assert_eq!(outcome1.status, RunStatus::Success);

        // Run 2: tickets are now `in_flight` (active). Bridge should skip them,
        // not error, and the pipeline should succeed.
        let outcome2 = runner
            .run_pipeline(
                DRILL_PIPELINE,
                PipelineInput {
                    prompt: Some("test".to_owned()),
                    run_id: Some("skip-run-2".to_owned()),
                    ..PipelineInput::default()
                },
            )
            .await?;
        assert_eq!(outcome2.status, RunStatus::Success);

        // Total distinct ticket IDs should still be exactly 3 — no duplicates.
        let db_path = dir.path().join(".derrick/derrick.db");
        let conn = rusqlite::Connection::open(&db_path)?;
        let ticket_count: i64 =
            conn.query_row("SELECT COUNT(DISTINCT id) FROM tickets", [], |row| {
                row.get(0)
            })?;
        assert_eq!(ticket_count, 3, "no duplicate tickets should be created");
        Ok(())
    }

    #[tokio::test]
    async fn drill_happy_path_writes_all_artifacts() -> TestResult {
        let reviewer = reviewer_script("#!/bin/sh\nprintf '## Verdict\\naccept\\n'")?;
        let (dir, runner) =
            runner(&yaml(drill_pipeline(), &reviewer.path().join("reviewer"))).await?;
        let outcome = runner
            .run_pipeline(
                DRILL_PIPELINE,
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
        assert!(
            dir.path()
                .join(".derrick/runs/run-1/manifest.json")
                .exists()
        );
        Ok(())
    }

    /// Fresh runs persist `pipeline_id: "drill"`, and a pre-rename manifest
    /// carrying the legacy `pipeline_id: "add-feature"` still resolves and
    /// resumes through the runner's dual-accept matcher.
    #[tokio::test]
    async fn legacy_add_feature_manifest_still_resumes() -> TestResult {
        let reviewer = reviewer_script("#!/bin/sh\nprintf '## Verdict\\naccept\\n'")?;
        let (dir, runner) =
            runner(&yaml(drill_pipeline(), &reviewer.path().join("reviewer"))).await?;

        // A fresh run writes the new pipeline id.
        let outcome = runner
            .run_pipeline(
                DRILL_PIPELINE,
                PipelineInput {
                    prompt: Some("test".to_owned()),
                    run_id: Some("legacy-run".to_owned()),
                    ..PipelineInput::default()
                },
            )
            .await?;
        assert_eq!(outcome.status, RunStatus::Success);

        let manifest_path = dir.path().join(".derrick/runs/legacy-run/manifest.json");
        let mut manifest = crate::manifest::read_manifest(&manifest_path)?;
        assert_eq!(
            manifest.pipeline_id, "drill",
            "fresh runs must persist pipeline_id: drill"
        );

        // Rewrite the manifest to look like a pre-rename run, then resume it.
        manifest.pipeline_id = "add-feature".to_owned();
        crate::manifest::write_manifest(&manifest_path, &manifest)?;

        let resumed = runner.resume(Some("legacy-run"), None).await?;
        assert_eq!(
            resumed.status,
            RunStatus::Success,
            "a legacy add-feature manifest should still resolve and resume"
        );
        Ok(())
    }

    #[tokio::test]
    async fn run_pipeline_drives_the_progress_reporter() -> TestResult {
        use crate::{ProgressReporter, RunProgress, StepProgress};
        use std::sync::Mutex as StdMutex;
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Default)]
        struct CountingReporter {
            started: AtomicUsize,
            steps_started: AtomicUsize,
            steps_finished: AtomicUsize,
            final_status: StdMutex<Option<RunStatus>>,
        }
        impl ProgressReporter for CountingReporter {
            fn pipeline_started(&self, _pid: &str, _run: &str, _total: usize) {
                self.started.fetch_add(1, Ordering::Relaxed);
            }
            fn step_started(&self, _id: &str, _i: usize, _t: usize, _interactive: bool) {
                self.steps_started.fetch_add(1, Ordering::Relaxed);
            }
            fn step_finished(&self, _p: StepProgress<'_>) {
                self.steps_finished.fetch_add(1, Ordering::Relaxed);
            }
            fn pipeline_finished(&self, p: RunProgress<'_>) {
                *self.final_status.lock().unwrap() = Some(p.status);
            }
        }

        let reviewer = reviewer_script("#!/bin/sh\nprintf '## Verdict\\naccept\\n'")?;
        let (_dir, runner) =
            runner(&yaml(drill_pipeline(), &reviewer.path().join("reviewer"))).await?;
        let reporter = std::sync::Arc::new(CountingReporter::default());
        let runner = runner.with_progress(reporter.clone());
        let outcome = runner
            .run_pipeline(
                DRILL_PIPELINE,
                PipelineInput {
                    prompt: Some("test".to_owned()),
                    run_id: Some("run-progress".to_owned()),
                    ..PipelineInput::default()
                },
            )
            .await?;

        assert_eq!(outcome.status, RunStatus::Success);
        assert_eq!(
            reporter.started.load(Ordering::Relaxed),
            1,
            "pipeline_started should fire exactly once"
        );
        // Every attempted step reports a finish (started fires for non-skipped).
        let finished = reporter.steps_finished.load(Ordering::Relaxed);
        assert_eq!(
            finished,
            outcome.steps.len(),
            "every step should report a finish"
        );
        assert!(
            reporter.steps_started.load(Ordering::Relaxed) >= 1,
            "at least one step should report a start"
        );
        assert_eq!(
            *reporter.final_status.lock().unwrap(),
            Some(RunStatus::Success),
            "pipeline_finished should carry the final status"
        );
        Ok(())
    }

    #[tokio::test]
    async fn no_assay_marks_assay_skipped() -> TestResult {
        let drafter = reviewer_script("#!/bin/sh\ncat > /dev/null\nprintf 'ok'")?;
        let reviewer = reviewer_script("#!/bin/sh\nexit 9")?;
        let (_dir, runner) = runner(&yaml_with_drafter(
            drill_pipeline(),
            &drafter.path().join("reviewer"),
            &reviewer.path().join("reviewer"),
        ))
        .await?;
        let mut skip = BTreeSet::new();
        skip.insert("assay".to_owned());
        let outcome = runner
            .run_pipeline(
                DRILL_PIPELINE,
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
                DRILL_PIPELINE,
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
                DRILL_PIPELINE,
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
        let (_dir, runner) =
            runner(&yaml(drill_pipeline(), &reviewer.path().join("reviewer"))).await?;
        let outcome = runner
            .run_pipeline(
                DRILL_PIPELINE,
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
        let (_dir, runner) =
            runner(&yaml(drill_pipeline(), &reviewer.path().join("reviewer"))).await?;

        let error = runner
            .run_pipeline("missing", PipelineInput::default())
            .await
            .err()
            .ok_or("unknown pipeline should error")?;

        assert!(matches!(error, RunError::UnknownPipeline(_)));
        Ok(())
    }

    #[tokio::test]
    async fn missing_prompt_for_drill_errors() -> TestResult {
        let reviewer = reviewer_script("#!/bin/sh\nprintf '## Verdict\\naccept\\n'")?;
        let (_dir, runner) =
            runner(&yaml(drill_pipeline(), &reviewer.path().join("reviewer"))).await?;

        let error = runner
            .run_pipeline(DRILL_PIPELINE, PipelineInput::default())
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
                DRILL_PIPELINE,
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
                DRILL_PIPELINE,
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
        let outcome = runner
            .run_pipeline(
                DRILL_PIPELINE,
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
        let ids: Vec<_> = outcome.steps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["specify", "writeA", "writeB"]);
        Ok(())
    }

    #[tokio::test]
    async fn skip_id_on_nonskippable_step_errors() -> TestResult {
        let reviewer = reviewer_script("#!/bin/sh\nprintf '## Verdict\\naccept\\n'")?;
        let (_dir, runner) =
            runner(&yaml(drill_pipeline(), &reviewer.path().join("reviewer"))).await?;
        let mut skip = BTreeSet::new();
        skip.insert("specify".to_owned());
        let error = runner
            .run_pipeline(
                DRILL_PIPELINE,
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
        let (_dir, runner) =
            runner(&yaml(drill_pipeline(), &reviewer.path().join("reviewer"))).await?;
        let outcome = runner
            .run_pipeline(
                DRILL_PIPELINE,
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
        let rounds = drill_pipeline().replace("rounds: \"{{tools.assay.rounds}}\"", "rounds: 2");
        let (dir, runner) = runner(&yaml_with_drafter(
            &rounds,
            &drafter.path().join("reviewer"),
            &reviewer.path().join("reviewer"),
        ))
        .await?;

        let outcome = runner
            .run_pipeline(
                DRILL_PIPELINE,
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
    async fn assay_revise_past_rounds_continues_pipeline() -> TestResult {
        // When the reviewer consistently returns `revise` and rounds are
        // exhausted, the pipeline should treat it as accept_with_conditions
        // and continue — no halt, no interactive prompt.
        let reviewer = reviewer_script(
            "#!/bin/sh\nprintf '## Suggested revisions\\nonly objection\\n## Verdict\\nrevise\\n'",
        )?;
        let (_dir, runner) =
            runner(&yaml(drill_pipeline(), &reviewer.path().join("reviewer"))).await?;
        let outcome = runner
            .run_pipeline(
                DRILL_PIPELINE,
                PipelineInput {
                    prompt: Some("test".to_owned()),
                    run_id: Some("revise-accept-conditions".to_owned()),
                    ..PipelineInput::default()
                },
            )
            .await?;

        assert_eq!(outcome.status, RunStatus::Success);
        Ok(())
    }

    #[tokio::test]
    async fn assay_unparsable_verdict_surfaces_step_failed() -> TestResult {
        let reviewer = reviewer_script("printf 'no verdict\\n'")?;
        let (_dir, runner) =
            runner(&yaml(drill_pipeline(), &reviewer.path().join("reviewer"))).await?;
        let error = runner
            .run_pipeline(
                DRILL_PIPELINE,
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
                DRILL_PIPELINE,
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
                DRILL_PIPELINE,
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
                DRILL_PIPELINE,
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
                DRILL_PIPELINE,
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
        let (_dir, runner) =
            runner(&yaml(drill_pipeline(), &reviewer.path().join("reviewer"))).await?;
        runner
            .run_pipeline(
                DRILL_PIPELINE,
                PipelineInput {
                    prompt: Some("test".to_owned()),
                    skip: BTreeSet::from(["assay".to_owned()]),
                    run_id: Some("resume-ok".to_owned()),
                    ..PipelineInput::default()
                },
            )
            .await?;
        let outcome = runner.resume(Some("resume-ok"), Some("tasks")).await?;

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
                    DRILL_PIPELINE,
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
        let (dir, runner) =
            runner(&yaml(drill_pipeline(), &reviewer.path().join("reviewer"))).await?;
        runner
            .run_pipeline(
                DRILL_PIPELINE,
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
            .resume(Some("resume-run"), Some("tasks"))
            .await
            .err()
            .ok_or("resume should refuse drift")?;

        assert!(error.to_string().contains("config has changed"));
        Ok(())
    }

    #[test]
    fn resume_step_index_retries_failed_step() {
        use crate::manifest::{ManifestStep, RunManifest};
        use chrono::Utc;
        use derrick_assay::types::StepStatus;

        let mut manifest = RunManifest::new(
            "test".into(),
            "pipeline".into(),
            "prompt".into(),
            crate::manifest::FlagsManifest {
                skip: vec![],
                unskip: vec![],
                dry_run: false,
            },
            "hash".into(),
            Utc::now(),
        );
        manifest.steps.push(ManifestStep {
            id: "specify".into(),
            status: StepStatus::Success,
            started_at: Utc::now(),
            finished_at: Utc::now(),
            log_path: std::path::PathBuf::new(),
            artifacts: vec![],
            tokens_in: 0,
            tokens_out: 0,
            bytes_raw: 0,
            bytes_saved: 0,
            roughneck_tokens_saved: 0,
        });
        manifest.steps.push(ManifestStep {
            id: "failme".into(),
            status: StepStatus::Failed,
            started_at: Utc::now(),
            finished_at: Utc::now(),
            log_path: std::path::PathBuf::new(),
            artifacts: vec![],
            tokens_in: 0,
            tokens_out: 0,
            bytes_raw: 0,
            bytes_saved: 0,
            roughneck_tokens_saved: 0,
        });
        assert_eq!(manifest.resume_step_index(), 1); // retry from "failme"
    }

    #[test]
    fn resume_step_index_skips_to_next_on_success() {
        use crate::manifest::{ManifestStep, RunManifest};
        use chrono::Utc;
        use derrick_assay::types::StepStatus;

        let mut manifest = RunManifest::new(
            "test".into(),
            "pipeline".into(),
            "prompt".into(),
            crate::manifest::FlagsManifest {
                skip: vec![],
                unskip: vec![],
                dry_run: false,
            },
            "hash".into(),
            Utc::now(),
        );
        manifest.steps.push(ManifestStep {
            id: "specify".into(),
            status: StepStatus::Success,
            started_at: Utc::now(),
            finished_at: Utc::now(),
            log_path: std::path::PathBuf::new(),
            artifacts: vec![],
            tokens_in: 0,
            tokens_out: 0,
            bytes_raw: 0,
            bytes_saved: 0,
            roughneck_tokens_saved: 0,
        });
        assert_eq!(manifest.resume_step_index(), 1); // next step is index 1
    }

    #[test]
    fn resume_step_index_retries_halted_step() {
        use crate::manifest::{ManifestStep, RunManifest};
        use chrono::Utc;
        use derrick_assay::types::StepStatus;

        let mut manifest = RunManifest::new(
            "test".into(),
            "pipeline".into(),
            "prompt".into(),
            crate::manifest::FlagsManifest {
                skip: vec![],
                unskip: vec![],
                dry_run: false,
            },
            "hash".into(),
            Utc::now(),
        );
        manifest.steps.push(ManifestStep {
            id: "assay".into(),
            status: StepStatus::Halted,
            started_at: Utc::now(),
            finished_at: Utc::now(),
            log_path: std::path::PathBuf::new(),
            artifacts: vec![],
            tokens_in: 0,
            tokens_out: 0,
            bytes_raw: 0,
            bytes_saved: 0,
            roughneck_tokens_saved: 0,
        });
        assert_eq!(manifest.resume_step_index(), 0); // retry from "assay"
    }

    #[test]
    fn resume_step_index_empty_manifest_returns_zero() {
        use crate::manifest::RunManifest;
        use chrono::Utc;

        let manifest = RunManifest::new(
            "test".into(),
            "pipeline".into(),
            "prompt".into(),
            crate::manifest::FlagsManifest {
                skip: vec![],
                unskip: vec![],
                dry_run: false,
            },
            "hash".into(),
            Utc::now(),
        );
        assert_eq!(manifest.resume_step_index(), 0);
    }

    #[tokio::test]
    async fn specify_step_prescaffolds_and_writes_feature_json() -> TestResult {
        use derrick_substrate::{EventKind, EventScope, Substrate};
        // Verify that derrick pre-scaffolds the feature directory and writes
        // feature.json before invoking the host, then the host overwrites the
        // stub spec.md with real content.
        struct MinimalSpecifyHost;

        #[async_trait::async_trait]
        impl HostAdapter for MinimalSpecifyHost {
            fn name(&self) -> &str {
                "claude"
            }
            fn is_available(&self) -> bool {
                true
            }
            async fn run(&self, request: HostRequest) -> Result<HostResponse, HostError> {
                // The pre-scaffold step has already created specs/001-test/.
                // A well-behaved host overwrites the stub spec.md.
                let feature = request.cwd.join("specs/001-test");
                std::fs::write(feature.join("spec.md"), "# Real spec\n\nFull content.\n").map_err(
                    |source| HostError::Io {
                        host: "claude".to_owned(),
                        source,
                    },
                )?;
                Ok(HostResponse {
                    stdout: "spec written to specs/001-test/spec.md\n".to_owned(),
                    stderr: String::new(),
                    exit_code: 0,
                    elapsed: Duration::from_millis(1),
                    tokens_in: 0,
                    tokens_out: 0,
                    pid: None,
                })
            }
        }

        let dir = tempdir()?;
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
        let substrate: std::sync::Arc<dyn Substrate> = std::sync::Arc::new(
            NativeSubstrate::open(
                NativeConfig {
                    db_path: dir.path().join(".derrick/derrick.db"),
                    worktree_root: dir.path().join(".derrick/worktrees"),
                },
                config.site().clone(),
            )
            .await?,
        );
        let mut hosts = HostRegistry::empty();
        hosts.register("claude", Box::new(MinimalSpecifyHost));
        let runner = Runner::new(
            config,
            Arc::clone(&substrate),
            hosts,
            dir.path().to_path_buf(),
        );
        runner
            .run_pipeline(
                DRILL_PIPELINE,
                PipelineInput {
                    prompt: Some("test".to_owned()),
                    run_id: Some("specify-detect".to_owned()),
                    ..PipelineInput::default()
                },
            )
            .await?;

        // D77: a PipelineStepStarted event must be persisted for the specify
        // step, scoped to the run's worktree, so `derrick observe` sees mid-step
        // liveness without polling the launching process.
        let events = substrate.tail_typed_events(None, 100).await?;
        assert!(
            events.iter().any(|e| matches!(
                (&e.scope, &e.kind),
                (EventScope::Worktree { run_id }, EventKind::PipelineStepStarted { step_id, .. })
                    if run_id == "specify-detect" && step_id == "specify"
            )),
            "expected PipelineStepStarted for specify, got: {:?}",
            events.iter().map(|e| &e.kind).collect::<Vec<_>>()
        );

        let feature_json =
            std::fs::read_to_string(dir.path().join(FEATURE_JSON)).expect("feature.json missing");
        assert!(
            feature_json.contains("specs/001-test"),
            "feature.json should point to specs/001-test, got: {feature_json}"
        );
        assert!(dir.path().join("specs/001-test/spec.md").exists());
        let spec = std::fs::read_to_string(dir.path().join("specs/001-test/spec.md"))?;
        assert!(
            !spec.contains(derrick_assay::io::SPEC_STUB_MARKER),
            "host should have overwritten the stub, got: {spec}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn assay_falls_back_to_claude_when_codex_and_no_tty() -> TestResult {
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
                    tokens_in: 0,
                    tokens_out: 0,
                    pid: None,
                })
            }
        }

        let dir = tempdir()?;
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
                DRILL_PIPELINE,
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
        let verdict = std::fs::read_to_string(dir.path().join("specs/001-test/assay/verdict.md"))?;
        assert!(
            verdict.contains("model: claude"),
            "verdict.md should record the claude fallback, got: {verdict}"
        );
        Ok(())
    }

    #[test]
    fn suggested_revisions_extracts_only_block() -> TestResult {
        use derrick_assay::suggested_revisions;
        let text = "## Risks\nfull prompt\n## Suggested revisions\nonly this\n## Verdict\nrevise\n";
        assert_eq!(suggested_revisions(text), Some("only this"));
        Ok(())
    }

    #[test]
    fn parse_clarify_response_and_render_answers() {
        let model_output =
            "Q: Which API style should we use?\nOptions: REST, GraphQL\nRecommendation: REST\n";
        let questions = parse_clarify_questions(model_output);
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].question, "Which API style should we use?");
        assert_eq!(questions[0].options, vec!["REST", "GraphQL"]);
        assert_eq!(questions[0].recommendation.as_deref(), Some("REST"));

        let selected = select_clarify_answer(&questions[0], "1");
        assert_eq!(selected, "REST");
        let markdown = render_clarify_markdown(&questions, &[selected]);
        assert!(markdown.contains("Answer: REST"));
    }

    #[test]
    fn select_clarify_answer_uses_recommendation_on_empty_input() {
        let question = ClarifyQuestion {
            question: "Q".to_owned(),
            options: vec!["A".to_owned(), "B".to_owned()],
            recommendation: Some("B".to_owned()),
        };
        assert_eq!(select_clarify_answer(&question, ""), "B");
    }

    #[tokio::test]
    async fn plan_prompt_includes_clarify_answers_when_present() -> TestResult {
        let reviewer = reviewer_script("#!/bin/sh\nprintf '## Verdict\\naccept\\n'")?;
        let (dir, runner) =
            runner(&yaml(drill_pipeline(), &reviewer.path().join("reviewer"))).await?;
        let feature_dir = dir.path().join("specs/001-test");
        std::fs::create_dir_all(&feature_dir)?;
        std::fs::write(
            feature_dir.join("clarify.md"),
            "# Clarification Q&A\n\nAnswer: GraphQL\n",
        )?;
        let mut state = ExecutionState::new(
            "test".to_owned(),
            "run-clarify".to_owned(),
            dir.path().join(".derrick/runs/run-clarify"),
        );
        state.feature_dir = Some(PathBuf::from("specs/001-test"));

        let prompt =
            runner.inject_clarify_answers_for_plan("plan", &state, "/speckit.plan".to_owned())?;
        assert!(prompt.contains("Apply these accepted clarifications"));
        assert!(prompt.contains("Answer: GraphQL"));
        Ok(())
    }

    fn multi_reviewer_yaml(reviewer_clis: &[(&str, &Path)], on_split: &str) -> String {
        let mut models = String::new();
        let mut roles = String::from(
            "  drafter: shell-drafter\n  proposer: shell-drafter\n  reviewer: shell-drafter\n",
        );
        let mut reviewer_list = String::new();
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
                DRILL_PIPELINE,
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
                DRILL_PIPELINE,
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
    async fn role_step_records_bytes_raw_for_host_subprocess() -> TestResult {
        // The role step's host CLI is a subprocess too. Its stdout/stderr
        // must be counted toward bytes_raw via the scrub plumbing.
        let reviewer = reviewer_script("#!/bin/sh\nprintf '## Verdict\\naccept\\n'")?;
        let (_dir, runner) =
            runner(&yaml(drill_pipeline(), &reviewer.path().join("reviewer"))).await?;
        let outcome = runner
            .run_pipeline(
                DRILL_PIPELINE,
                PipelineInput {
                    prompt: Some("test".to_owned()),
                    run_id: Some("role-bytes-raw".to_owned()),
                    ..PipelineInput::default()
                },
            )
            .await?;
        let specify = outcome
            .steps
            .iter()
            .find(|s| s.id == "specify")
            .ok_or("specify step should exist")?;
        assert_eq!(specify.status, StepStatus::Success);
        assert!(
            specify.bytes_raw > 0,
            "role step bytes_raw should be > 0, got {}",
            specify.bytes_raw
        );
        Ok(())
    }

    #[tokio::test]
    async fn bash_step_records_bytes_raw_when_compression_enabled() -> TestResult {
        let reviewer = reviewer_script("#!/bin/sh\nprintf '## Verdict\\naccept\\n'")?;
        // A bash step that produces non-empty stdout. Using `echo` rather
        // than a known scrubber-tool ensures we only assert bytes_raw, not
        // bytes_saved (which requires a matching rule set).
        let pipe = r#"  - id: specify
    role: drafter
    host: claude
    command: "/speckit.specify {{prompt}}"
  - id: noisy
    runner: bash
    command: "printf 'hello world from bash step\\n'"
"#;
        let (_dir, runner) = runner(&yaml(pipe, &reviewer.path().join("reviewer"))).await?;
        let outcome = runner
            .run_pipeline(
                DRILL_PIPELINE,
                PipelineInput {
                    prompt: Some("test".to_owned()),
                    run_id: Some("bytes-raw".to_owned()),
                    ..PipelineInput::default()
                },
            )
            .await?;
        let noisy = outcome
            .steps
            .iter()
            .find(|s| s.id == "noisy")
            .ok_or("noisy step should exist")?;
        assert_eq!(noisy.status, StepStatus::Success);
        assert!(
            noisy.bytes_raw > 0,
            "bytes_raw should be non-zero for bash step with stdout, got {}",
            noisy.bytes_raw
        );
        Ok(())
    }

    #[tokio::test]
    async fn bash_step_records_bytes_saved_when_tool_has_scrub_rules() -> TestResult {
        let reviewer = reviewer_script("#!/bin/sh\nprintf '## Verdict\\naccept\\n'")?;
        // `git` has a scrub rule that drops `remote: Counting objects` lines.
        // Use bash to fake git stderr noise that the scrubber will collapse.
        let pipe = r#"  - id: specify
    role: drafter
    host: claude
    command: "/speckit.specify {{prompt}}"
  - id: fake_git
    runner: bash
    command: "git --version >/dev/null; printf 'remote: Counting objects: 100\\nremote: Counting objects: 100\\nkeep\\n'"
"#;
        let (_dir, runner) = runner(&yaml(pipe, &reviewer.path().join("reviewer"))).await?;
        let outcome = runner
            .run_pipeline(
                DRILL_PIPELINE,
                PipelineInput {
                    prompt: Some("test".to_owned()),
                    run_id: Some("bytes-saved".to_owned()),
                    ..PipelineInput::default()
                },
            )
            .await?;
        let step = outcome
            .steps
            .iter()
            .find(|s| s.id == "fake_git")
            .ok_or("fake_git step should exist")?;
        assert_eq!(step.status, StepStatus::Success);
        assert!(step.bytes_raw > 0, "bytes_raw should be > 0");
        assert!(
            step.bytes_saved > 0,
            "bytes_saved should be > 0 once git scrub rules fire, got {}",
            step.bytes_saved
        );
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
                DRILL_PIPELINE,
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

/// Spec-provider seam (DESIGN.md §5.3) dispatch tests. A *bare*
/// `specify`/`plan`/`tasks` step routes through `run_spec_phase`; explicit
/// `host:`+`command:` steps bypass the seam entirely.
#[cfg(test)]
mod spec_provider_seam {
    use crate::spec_provider::SpecPhase;
    use crate::steps::execute_step;
    use derrick_assay::ExecutionState;
    use derrick_assay::io::FEATURE_JSON;
    use derrick_assay::types::StepStatus;
    use derrick_config::Config;
    use derrick_substrate_native::{NativeConfig, NativeSubstrate};
    use derrick_tools::{HostAdapter, HostError, HostRegistry, HostRequest, HostResponse};
    use std::error::Error;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;
    use tempfile::tempdir;

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    /// A bare drill pipeline: spec steps carry only `id`, no host/command/runner.
    fn bare_drill_pipeline() -> &'static str {
        r#"  - id: specify
  - id: plan
  - id: tasks
"#
    }

    /// Records every prompt the host receives, so a test can prove the seam
    /// produced the canonical speckit command (or that an explicit step ran).
    struct RecordingHost {
        prompts: Arc<StdMutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl HostAdapter for RecordingHost {
        fn name(&self) -> &str {
            "claude"
        }
        fn is_available(&self) -> bool {
            true
        }
        async fn run(&self, request: HostRequest) -> Result<HostResponse, HostError> {
            self.prompts
                .lock()
                .expect("lock")
                .push(request.prompt.clone());
            let feature = request.cwd.join("specs/001-test");
            std::fs::create_dir_all(&feature).map_err(|source| HostError::Io {
                host: "claude".to_owned(),
                source,
            })?;
            std::fs::create_dir_all(request.cwd.join(".specify")).map_err(|source| {
                HostError::Io {
                    host: "claude".to_owned(),
                    source,
                }
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
                std::fs::write(feature.join("spec.md"), "# Real spec\nfull content\n").map_err(
                    |source| HostError::Io {
                        host: "claude".to_owned(),
                        source,
                    },
                )?;
            } else if request.prompt.contains("speckit.plan") {
                std::fs::write(feature.join("plan.md"), "plan").map_err(|source| {
                    HostError::Io {
                        host: "claude".to_owned(),
                        source,
                    }
                })?;
            } else if request.prompt.contains("speckit.tasks") {
                std::fs::write(feature.join("tasks.md"), "## Task one\nbody\n").map_err(
                    |source| HostError::Io {
                        host: "claude".to_owned(),
                        source,
                    },
                )?;
            }
            Ok(HostResponse {
                stdout: "ok\n".to_owned(),
                stderr: String::new(),
                exit_code: 0,
                elapsed: Duration::from_millis(1),
                tokens_in: 0,
                tokens_out: 0,
                pid: None,
            })
        }
    }

    /// Builds a config + substrate + recording host for a given pipeline YAML
    /// body, optionally injecting a `tools.specify` block. Returns the loaded
    /// pieces plus the shared prompt log.
    async fn harness(
        pipeline: &str,
        specify_block: &str,
    ) -> TestResult<(
        tempfile::TempDir,
        Config,
        Arc<NativeSubstrate>,
        Arc<HostRegistry>,
        Arc<StdMutex<Vec<String>>>,
    )> {
        let dir = tempdir()?;
        // Self-contained YAML: a shell-reviewer model + drafter/proposer/reviewer
        // roles, the speckit block, the optional tools.specify block, then the
        // supplied pipeline body. No host CLI is actually shelled — the spec
        // steps run through the registered claude RecordingHost.
        let yaml = format!(
            r#"version: 1
site:
  name: test
  prefix: tst
models:
  shell-reviewer:
    provider: shell
    cli: "/nonexistent-reviewer"
    model: shell-reviewer
roles:
  drafter: shell-reviewer
  proposer: shell-reviewer
  reviewer: shell-reviewer
tools:
  speckit:
    enabled: true
    version: ">=0.4.0"
{specify_block}  assay:
    enabled: false
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
{pipeline}guardrails:
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
        std::fs::write(dir.path().join("derrick.yaml"), &yaml)?;
        std::fs::create_dir_all(dir.path().join(".specify/memory"))?;
        std::fs::create_dir_all(dir.path().join(".derrick"))?;
        std::fs::write(
            dir.path().join(".specify/memory/constitution.md"),
            "constitution",
        )?;
        let config = Config::load_from_path(&dir.path().join("derrick.yaml"))?;
        let substrate = Arc::new(
            NativeSubstrate::open(
                NativeConfig {
                    db_path: dir.path().join(".derrick/derrick.db"),
                    worktree_root: dir.path().join(".derrick/worktrees"),
                },
                config.site().clone(),
            )
            .await?,
        );
        let prompts = Arc::new(StdMutex::new(Vec::new()));
        let mut hosts = HostRegistry::empty();
        hosts.register(
            "claude",
            Box::new(RecordingHost {
                prompts: prompts.clone(),
            }),
        );
        Ok((dir, config, substrate, Arc::new(hosts), prompts))
    }

    /// Runs a single named step through `execute_step` and returns its status.
    async fn run_step(
        config: &Config,
        substrate: &NativeSubstrate,
        hosts: Arc<HostRegistry>,
        repo_root: &Path,
        state: &mut ExecutionState,
        step_id: &str,
    ) -> TestResult<StepStatus> {
        let step = config
            .pipeline()
            .iter()
            .find(|s| s.id() == step_id)
            .unwrap_or_else(|| panic!("step {step_id} missing"))
            .clone();
        let manifest_path = state.run_dir.join("manifest.json");
        let record = execute_step(
            config,
            substrate,
            hosts,
            repo_root,
            &step,
            state,
            &state.run_id.clone(),
            &manifest_path,
            None,
        )
        .await?;
        Ok(record.status)
    }

    #[test]
    fn step_id_maps_to_phase() {
        assert_eq!(SpecPhase::from_step_id("specify"), Some(SpecPhase::Specify));
        assert_eq!(SpecPhase::from_step_id("plan"), Some(SpecPhase::Plan));
        assert_eq!(SpecPhase::from_step_id("tasks"), Some(SpecPhase::Tasks));
        assert_eq!(SpecPhase::from_step_id("assay"), None);
        assert_eq!(SpecPhase::from_step_id("bridge"), None);
    }

    #[tokio::test]
    async fn bare_spec_steps_route_through_speckit_arm() -> TestResult {
        // Default provider (no tools.specify block) → speckit. The bare
        // specify/plan/tasks steps must produce the same artifacts and
        // feature.json as the explicit speckit steps, via the canonical
        // speckit commands.
        let (dir, config, substrate, hosts, prompts) = harness(bare_drill_pipeline(), "").await?;
        let run_dir = dir.path().join(".derrick/runs/bare-run");
        std::fs::create_dir_all(&run_dir)?;
        let mut state = ExecutionState::new("test".to_owned(), "bare-run".to_owned(), run_dir);

        for (id, status) in [
            ("specify", StepStatus::Success),
            ("plan", StepStatus::Success),
            ("tasks", StepStatus::Success),
        ] {
            let got = run_step(
                &config,
                &substrate,
                hosts.clone(),
                dir.path(),
                &mut state,
                id,
            )
            .await?;
            assert_eq!(got, status, "step {id} should succeed via the seam");
        }

        // Same artifacts as the explicit speckit path.
        assert!(
            dir.path().join(FEATURE_JSON).exists(),
            "feature.json written"
        );
        assert!(dir.path().join("specs/001-test/spec.md").exists());
        assert!(dir.path().join("specs/001-test/plan.md").exists());
        assert!(dir.path().join("specs/001-test/tasks.md").exists());

        // The seam handed the canonical speckit commands to the host.
        let prompts = prompts.lock().expect("lock");
        assert!(
            prompts.iter().any(|p| p.contains("/speckit.specify")),
            "specify phase must use the speckit command, got: {prompts:?}"
        );
        assert!(prompts.iter().any(|p| p.contains("/speckit.plan")));
        assert!(prompts.iter().any(|p| p.contains("/speckit.tasks")));
        Ok(())
    }

    #[tokio::test]
    async fn explicit_speckit_step_bypasses_the_seam() -> TestResult {
        // An explicit host+command specify step must NOT consult the provider:
        // even with `tools.specify.provider: native` (whose seam arm errors),
        // the explicit step runs through execute_role_step and succeeds.
        let explicit = r#"  - id: specify
    role: drafter
    host: claude
    command: "/speckit.specify {{prompt}}"
"#;
        let (dir, config, substrate, hosts, prompts) =
            harness(explicit, "  specify:\n    provider: native\n").await?;
        let run_dir = dir.path().join(".derrick/runs/explicit-run");
        std::fs::create_dir_all(&run_dir)?;
        let mut state = ExecutionState::new("test".to_owned(), "explicit-run".to_owned(), run_dir);

        let status = run_step(
            &config,
            &substrate,
            hosts.clone(),
            dir.path(),
            &mut state,
            "specify",
        )
        .await?;
        assert_eq!(
            status,
            StepStatus::Success,
            "explicit speckit step must bypass the native seam and succeed"
        );
        assert!(dir.path().join("specs/001-test/spec.md").exists());
        let prompts = prompts.lock().expect("lock");
        assert!(
            prompts.iter().any(|p| p.contains("/speckit.specify")),
            "explicit step still drives the host, got: {prompts:?}"
        );
        Ok(())
    }

    /// A claude host that emits canned native-schema artifacts per phase, and
    /// records every prompt so the test can assert clarify-first ordering. This
    /// is the real `HostRegistry` boundary — the native provider calls
    /// `hosts.get("claude")` exactly as in production.
    struct NativeStubHost {
        prompts: Arc<StdMutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl HostAdapter for NativeStubHost {
        fn name(&self) -> &str {
            "claude"
        }
        fn is_available(&self) -> bool {
            true
        }
        async fn run(&self, request: HostRequest) -> Result<HostResponse, HostError> {
            let prompt = request.prompt.clone();
            self.prompts.lock().expect("lock").push(prompt.clone());
            let stdout = if prompt.contains("clarify a feature request BEFORE") {
                "Q: Which format?\nOptions: JSON, YAML\nRecommendation: JSON\n".to_owned()
            } else if prompt.contains("Write a specification") {
                "---\nschema: derrick.spec/v1\nslug: thing\nintent: do the thing\nrequirements:\n  - id: R1\n    must: it works\nacceptance:\n  - id: A1\n    check: verified\nnon_goals: []\nopen_questions: []\n---\n# Thing\n\n## Context\nx\n\n## Requirements\nR1\n\n## Acceptance Criteria\nA1\n\n## Out of Scope\nnone\n".to_owned()
            } else if prompt.contains("implementation plan") {
                "---\nschema: derrick.plan/v1\ncovers: [R1]\ntouches: []\n---\n# Plan\nsteps\n"
                    .to_owned()
            } else if prompt.contains("Break this plan into tickets") {
                "## Ticket one\nImplements R1.\n".to_owned()
            } else {
                String::new()
            };
            Ok(HostResponse {
                stdout,
                stderr: String::new(),
                exit_code: 0,
                elapsed: Duration::from_millis(1),
                tokens_in: 5,
                tokens_out: 9,
                pid: None,
            })
        }
    }

    #[tokio::test]
    async fn bare_step_with_native_provider_runs_end_to_end() -> TestResult {
        // Build the config + substrate via the shared harness, then swap in a
        // native-aware host registry (the harness's RecordingHost only knows the
        // speckit commands).
        let (dir, config, substrate, _hosts, _prompts) =
            harness(bare_drill_pipeline(), "  specify:\n    provider: native\n").await?;
        let prompts = Arc::new(StdMutex::new(Vec::new()));
        let mut native_hosts = HostRegistry::empty();
        native_hosts.register(
            "claude",
            Box::new(NativeStubHost {
                prompts: prompts.clone(),
            }),
        );
        let hosts = Arc::new(native_hosts);

        let run_dir = dir.path().join(".derrick/runs/native-run");
        std::fs::create_dir_all(&run_dir)?;
        let mut state =
            ExecutionState::new("do the thing".to_owned(), "native-run".to_owned(), run_dir);

        for id in ["specify", "plan", "tasks"] {
            let status = run_step(
                &config,
                substrate.as_ref(),
                hosts.clone(),
                dir.path(),
                &mut state,
                id,
            )
            .await?;
            assert_eq!(status, StepStatus::Success, "native {id} should succeed");
        }

        // Clarify-first: the clarify prompt was issued before the spec draft.
        let issued = prompts.lock().expect("lock").clone();
        let clarify_at = issued
            .iter()
            .position(|p| p.contains("clarify a feature request BEFORE"));
        let spec_at = issued
            .iter()
            .position(|p| p.contains("Write a specification"));
        assert!(
            matches!((clarify_at, spec_at), (Some(c), Some(s)) if c < s),
            "clarify must precede the spec draft"
        );

        // The three canonical artifacts exist and validate.
        let feature_dir = state
            .feature_dir
            .clone()
            .expect("feature_dir set by specify");
        let spec_md = std::fs::read_to_string(dir.path().join(&feature_dir).join("spec.md"))?;
        let plan_md = std::fs::read_to_string(dir.path().join(&feature_dir).join("plan.md"))?;
        let tasks_md = std::fs::read_to_string(dir.path().join(&feature_dir).join("tasks.md"))?;
        assert!(!derrick_specify::schema::has_reject(
            &derrick_specify::schema::validate_spec(&spec_md)
        ));
        assert!(!derrick_specify::schema::has_reject(
            &derrick_specify::schema::validate_plan(&plan_md, &["R1".to_owned()], &[],)
        ));
        assert!(!derrick_specify::schema::has_reject(
            &derrick_specify::schema::validate_tasks(&tasks_md)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn bare_step_with_import_provider_returns_not_yet_available() -> TestResult {
        let (dir, config, substrate, hosts, _prompts) =
            harness(bare_drill_pipeline(), "  specify:\n    provider: import\n").await?;
        let run_dir = dir.path().join(".derrick/runs/import-run");
        std::fs::create_dir_all(&run_dir)?;
        let mut state =
            ExecutionState::new("do the thing".to_owned(), "import-run".to_owned(), run_dir);

        let step = config
            .pipeline()
            .iter()
            .find(|s| s.id() == "specify")
            .expect("specify step")
            .clone();
        let manifest_path = state.run_dir.join("manifest.json");
        let error = execute_step(
            &config,
            substrate.as_ref(),
            hosts.clone(),
            dir.path(),
            &step,
            &mut state,
            "import-run",
            &manifest_path,
            None,
        )
        .await
        .expect_err("import provider should error in Phase 1");
        assert!(
            error.to_string().contains("not yet available") && error.to_string().contains("import"),
            "expected a clear not-yet-available error, got: {error}"
        );
        Ok(())
    }

    #[test]
    fn detect_artifacts_returns_expected_spec_paths() {
        // Unchanged behaviour: the artifact resolver maps spec phases to their
        // canonical files under the feature dir + .specify/feature.json.
        let tmp = tempdir().expect("tempdir");
        let feature = tmp.path().join("specs/001-test");
        std::fs::create_dir_all(&feature).expect("feature dir");
        std::fs::create_dir_all(tmp.path().join(".specify")).expect(".specify");
        std::fs::write(tmp.path().join(FEATURE_JSON), "{}").expect("feature.json");
        std::fs::write(feature.join("spec.md"), "spec").expect("spec");
        std::fs::write(feature.join("plan.md"), "plan").expect("plan");
        std::fs::write(feature.join("tasks.md"), "tasks").expect("tasks");

        let mut state = ExecutionState::new(
            "p".to_owned(),
            "r".to_owned(),
            tmp.path().join(".derrick/runs/r"),
        );
        state.feature_dir = Some(std::path::PathBuf::from("specs/001-test"));

        let specify = crate::steps::detect_artifacts("specify", &state, tmp.path());
        assert!(specify.contains(&std::path::PathBuf::from(FEATURE_JSON)));
        assert!(specify.contains(&std::path::PathBuf::from("specs/001-test/spec.md")));

        let plan = crate::steps::detect_artifacts("plan", &state, tmp.path());
        assert_eq!(
            plan,
            vec![std::path::PathBuf::from("specs/001-test/plan.md")]
        );

        let tasks = crate::steps::detect_artifacts("tasks", &state, tmp.path());
        assert_eq!(
            tasks,
            vec![std::path::PathBuf::from("specs/001-test/tasks.md")]
        );
    }
}
