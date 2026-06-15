use crate::commands::{RunArgs, RunCommand};
use crate::exit_code::CliExitCode;
use crate::{current_repo_root, native_paths, read_config};

use std::sync::Arc;

use derrick_config::SubstrateBackendKind;
use derrick_flow::{PipelineInput, ProgressReporter, Runner};
use derrick_substrate_native::NativeSubstrate;
use derrick_tools::HostRegistry;

use crate::progress::CliReporter;

pub(crate) async fn execute(args: RunArgs) -> Result<CliExitCode, crate::CliError> {
    let (_repo_root, _config, _substrate, runner) = build_runner().await?;

    match args.command {
        RunCommand::Drill(drill) => {
            if let Some(from_step) = drill.resume_from {
                // Explicit step-level resume (--resume-from <step>).
                let outcome = runner
                    .resume(drill.run_id.as_deref(), Some(&from_step))
                    .await
                    .map_err(|error| crate::message(error.to_string()))?;
                return Ok(status_code(outcome.status));
            }

            if drill.auto_resume {
                // Prompt-key auto-resume: detected by `drill.rs`, run_id already
                // pinned to the incomplete run.
                let outcome = runner
                    .resume(drill.run_id.as_deref(), None)
                    .await
                    .map_err(|error| crate::message(error.to_string()))?;
                return Ok(status_code(outcome.status));
            }

            if let Some(prior_run_id) = drill.force_prior_run_id.clone() {
                // Force-restart: start a brand-new run but record lineage.
                let input = pipeline_input(drill);
                let outcome = runner
                    .run_pipeline_as_restart("drill", input, prior_run_id)
                    .await
                    .map_err(|error| crate::message(error.to_string()))?;
                return Ok(status_code(outcome.status));
            }

            let input = pipeline_input(drill);
            let outcome = runner
                .run_pipeline("drill", input)
                .await
                .map_err(|error| crate::message(error.to_string()))?;
            Ok(status_code(outcome.status))
        }
        RunCommand::Resume(args) => {
            let outcome = runner
                .resume(args.run_id.as_deref(), None)
                .await
                .map_err(|error| crate::message(error.to_string()))?;
            Ok(status_code(outcome.status))
        }
    }
}

async fn build_runner() -> Result<
    (
        std::path::PathBuf,
        derrick_config::Config,
        derrick_substrate_native::NativeSubstrate,
        Runner,
    ),
    crate::CliError,
> {
    let repo_root = current_repo_root()?;
    let config = read_config(&repo_root)?;
    if config.tools().substrate().backend() != SubstrateBackendKind::Native {
        return Err(crate::message(
            "derrick run drill requires tools.substrate.backend: native",
        ));
    }
    let substrate =
        NativeSubstrate::open(native_paths(&repo_root, &config), config.site().clone()).await?;
    let reporter: Arc<dyn ProgressReporter> = Arc::new(CliReporter::new());
    let runner = Runner::new(
        config.clone(),
        std::sync::Arc::new(substrate.clone()),
        HostRegistry::with_defaults(),
        repo_root.clone(),
    )
    .with_progress(reporter);
    Ok((repo_root, config, substrate, runner))
}

fn pipeline_input(args: crate::commands::DrillRunArgs) -> PipelineInput {
    let mut skip = args
        .skip
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    if args.no_clarify {
        skip.insert("clarify".to_owned());
    }
    if args.no_assay {
        skip.insert("assay".to_owned());
    }

    PipelineInput {
        prompt: args.prompt,
        skip,
        unskip: args.unskip.into_iter().collect(),
        dry_run: args.dry_run,
        run_id: args.run_id,
        no_github_issues: args.no_github_issues,
    }
}

fn status_code(status: derrick_flow::RunStatus) -> CliExitCode {
    match status {
        derrick_flow::RunStatus::Success => CliExitCode::Success,
        derrick_flow::RunStatus::Failed | derrick_flow::RunStatus::Halted => CliExitCode::Failure,
    }
}
