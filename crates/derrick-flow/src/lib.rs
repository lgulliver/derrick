//! Pipeline orchestrator. See DESIGN.md §5.3 and §10.

mod clarify;
mod code_review;
mod manifest;
mod runner;
mod spinner;
mod steps;

pub use code_review::{run_code_review, CodeReviewOutcome};
pub use derrick_assay::types::{
    PipelineInput, RunError, RunOutcome, RunStatus, StepRecord, StepStatus,
};
pub use runner::Runner;

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
        parse_clarify_questions, render_clarify_markdown, select_clarify_answer, ClarifyQuestion,
    };
    use crate::runner::Runner;
    use derrick_assay::io::FEATURE_JSON;
    use derrick_assay::types::{PipelineInput, RunError, RunStatus, StepStatus};
    use derrick_assay::ExecutionState;
    use derrick_config::Config;
    use derrick_substrate_native::{NativeConfig, NativeSubstrate};
    use derrick_tools::{HostAdapter, HostError, HostRegistry, HostRequest, HostResponse};
    use std::error::Error;
    use std::path::PathBuf;
    use std::time::Duration;
    use tempfile::{tempdir, TempDir};

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    const ADD_FEATURE_PIPELINE: &str = "add-feature";

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

    fn add_feature_pipeline_with_dispatch() -> String {
        format!(
            "{}\n  - id: bridge\n    runner: derrick\n    inputs: [\"{{{{feature_dir}}}}/tasks.md\"]\n    batch: \"br-{{{{run_id}}}}\"\n  - id: foreman\n    runner: derrick\n",
            add_feature_pipeline()
        )
    }

    #[tokio::test]
    async fn bridge_creates_tickets_from_tasks() -> TestResult {
        let reviewer = reviewer_script("#!/bin/sh\nprintf '## Verdict\\naccept\\n'")?;
        let (dir, runner) = runner(&yaml_crew(
            &add_feature_pipeline_with_dispatch(),
            &reviewer.path().join("reviewer"),
        ))
        .await?;
        let outcome = runner
            .run_pipeline(
                ADD_FEATURE_PIPELINE,
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
        let drafter = reviewer_script("#!/bin/sh\ncat > /dev/null\nprintf 'ok'")?;
        let reviewer = reviewer_script("#!/bin/sh\nexit 9")?;
        let (_dir, runner) = runner(&yaml_with_drafter(
            add_feature_pipeline(),
            &drafter.path().join("reviewer"),
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
    async fn specify_step_pre_creates_specify_features_dir() -> TestResult {
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
        let (dir, runner) = runner(&yaml(
            add_feature_pipeline(),
            &reviewer.path().join("reviewer"),
        ))
        .await?;
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
