use crate::commands::{RunArgs, RunCommand};
use crate::exit_code::CliExitCode;
use crate::{current_repo_root, native_paths, read_config};

use derrick_config::SubstrateBackendKind;
use derrick_flow::{PipelineInput, Runner};
use derrick_substrate_native::NativeSubstrate;
use derrick_tools::HostRegistry;

pub(crate) async fn execute(args: RunArgs) -> Result<CliExitCode, crate::CliError> {
    match args.command {
        RunCommand::AddFeature(add_feature) => {
            let repo_root = current_repo_root()?;
            let config = read_config(&repo_root)?;
            if config.tools().substrate().backend() != SubstrateBackendKind::Native {
                return Err(crate::message(
                    "derrick run add-feature requires tools.substrate.backend: native",
                ));
            }
            let substrate =
                NativeSubstrate::open(native_paths(&repo_root, &config), config.site().clone())
                    .await?;
            let runner = Runner::new(
                config,
                std::sync::Arc::new(substrate),
                HostRegistry::with_defaults(),
                repo_root,
            );

            if let Some(from_step) = add_feature.resume_from {
                let outcome = runner
                    .resume(add_feature.run_id.as_deref(), &from_step)
                    .await
                    .map_err(|error| crate::message(error.to_string()))?;
                println!("resumed run {}: {:?}", outcome.run_id, outcome.status);
                return Ok(status_code(outcome.status));
            }

            let input = pipeline_input(add_feature);
            let outcome = runner
                .run_pipeline("add-feature", input)
                .await
                .map_err(|error| crate::message(error.to_string()))?;
            println!("run {}: {:?}", outcome.run_id, outcome.status);
            Ok(status_code(outcome.status))
        }
    }
}

fn pipeline_input(args: crate::commands::AddFeatureArgs) -> PipelineInput {
    let mut skip = args
        .skip
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    if args.no_clarify {
        skip.insert("clarify".to_owned());
    }
    if args.no_checkpoint {
        skip.insert("checkpoint".to_owned());
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
    }
}

fn status_code(status: derrick_flow::RunStatus) -> CliExitCode {
    match status {
        derrick_flow::RunStatus::Success => CliExitCode::Success,
        derrick_flow::RunStatus::Failed | derrick_flow::RunStatus::Halted => CliExitCode::Failure,
    }
}
