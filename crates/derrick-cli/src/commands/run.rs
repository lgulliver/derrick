use crate::commands::{RunArgs, RunCommand};
use crate::exit_code::CliExitCode;
use crate::{current_repo_root, native_paths, read_config};

use std::sync::Arc;

use derrick_config::SubstrateBackendKind;
use derrick_flow::{PipelineInput, ProgressReporter, Runner};
use derrick_substrate_native::NativeSubstrate;
use derrick_tools::HostRegistry;

use crate::progress::CliReporter;

/// Executes the `derrick run` subcommand (drill or resume).
pub(crate) async fn execute(args: RunArgs) -> Result<CliExitCode, crate::CliError> {
    // A `--spec <path>` override (drill only) forces the `import` provider for
    // this run at the highest precedence, without editing derrick.yaml. It is
    // applied to the in-memory config before the runner is built.
    //
    // It cannot be combined with resuming: a resume reuses the prior run's
    // artifacts (and its config hash must match), so a new `--spec` source would
    // never be imported and later phases would run from stale artifacts. Reject
    // it up front with a clear error rather than silently ignoring the source.
    let spec_override = match &args.command {
        RunCommand::Drill(drill) => {
            if drill.spec.is_some() && (drill.resume_from.is_some() || drill.auto_resume) {
                return Err(crate::message(
                    "`--spec` cannot be combined with resuming an existing run \
                     (--resume-from or an auto-resumed prompt); start a fresh drill \
                     run instead (e.g. with --force)",
                ));
            }
            drill.spec.clone()
        }
        RunCommand::Resume(_) => None,
    };
    // A `--profile <name>` override (drill only) applies the named profile's
    // stage bindings to the in-memory config for this run. When absent on a
    // fresh drill, the config's `default_profile` (if any) is applied.
    // Resume paths must never apply a profile — they reuse the prior run's
    // pinned config and altering bindings mid-run violates the same contract
    // as `--spec` + resume. Reject it up front with a clear error.
    let profile_override = match &args.command {
        RunCommand::Drill(drill) => {
            if drill.profile.is_some() && (drill.resume_from.is_some() || drill.auto_resume) {
                return Err(crate::message(
                    "`--profile` cannot be combined with resuming an existing run \
                     (--resume-from or an auto-resumed prompt); resume the run as-is \
                     or start a fresh drill run instead",
                ));
            }
            drill.profile.clone()
        }
        RunCommand::Resume(_) => None,
    };
    let is_resume = match &args.command {
        RunCommand::Drill(drill) => drill.auto_resume || drill.resume_from.is_some(),
        RunCommand::Resume(_) => true,
    };
    let (_repo_root, _config, _substrate, runner) =
        build_runner(spec_override, profile_override, !is_resume).await?;

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
                let input = pipeline_input(drill)?;
                let outcome = runner
                    .run_pipeline_as_restart("drill", input, prior_run_id)
                    .await
                    .map_err(|error| crate::message(error.to_string()))?;
                return Ok(status_code(outcome.status));
            }

            let input = pipeline_input(drill)?;
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

/// Builds the substrate, runner, and resolved config for a run or resume.
async fn build_runner(
    spec_override: Option<String>,
    profile_override: Option<String>,
    allow_default_profile: bool,
) -> Result<
    (
        std::path::PathBuf,
        derrick_config::Config,
        derrick_substrate_native::NativeSubstrate,
        Runner,
    ),
    crate::CliError,
> {
    let repo_root = current_repo_root()?;
    let mut config = read_config(&repo_root)?;
    // Apply the requested profile (or the configured default profile) before any
    // other override: `--profile <name>` takes precedence over `default_profile`.
    // On resume paths `allow_default_profile` is false to preserve the original
    // run's role bindings.
    config = if let Some(profile_name) = &profile_override {
        config
            .with_profile(profile_name)
            .map_err(|e| crate::message(e.to_string()))?
    } else if allow_default_profile {
        if let Some(default) = config.default_profile().map(str::to_owned) {
            config
                .with_profile(&default)
                .map_err(|e| crate::message(e.to_string()))?
        } else {
            config
        }
    } else {
        config
    };
    // Highest-precedence run override: `--spec <path>` forces the import provider.
    if let Some(source) = spec_override {
        config.force_import_spec(source);
    }
    // D15/D65: surface model/host issues early without blocking the run.
    crate::commands::models::emit_soft_warnings(&config);
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

/// Constructs a [`PipelineInput`] from the resolved drill arguments.
fn pipeline_input(args: crate::commands::DrillRunArgs) -> Result<PipelineInput, crate::CliError> {
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

    // On the direct `run drill` path the prompt may arrive via
    // `--prompt-file` or stdin; resolve it here.  When `drill.rs` is the caller it
    // has already resolved the prompt and cleared `prompt_file`, so this is a
    // no-op for that path.
    let prompt =
        crate::commands::prompt_input::resolve_prompt_from_env(args.prompt, args.prompt_file)?;

    Ok(PipelineInput {
        prompt,
        skip,
        unskip: args.unskip.into_iter().collect(),
        dry_run: args.dry_run,
        run_id: args.run_id,
        no_github_issues: args.no_github_issues,
    })
}

/// Maps a pipeline run status to the CLI exit code.
fn status_code(status: derrick_flow::RunStatus) -> CliExitCode {
    match status {
        derrick_flow::RunStatus::Success => CliExitCode::Success,
        derrick_flow::RunStatus::Failed | derrick_flow::RunStatus::Halted => CliExitCode::Failure,
    }
}
