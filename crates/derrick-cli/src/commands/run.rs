use crate::commands::{RunArgs, RunCommand};
use crate::exit_code::CliExitCode;

pub(crate) async fn execute(args: RunArgs) -> Result<CliExitCode, crate::CliError> {
    match args.command {
        RunCommand::AddFeature(add_feature) => {
            let _ = (
                add_feature.prompt,
                add_feature.resume_from,
                add_feature.no_clarify,
                add_feature.no_checkpoint,
                add_feature.no_assay,
            );
            eprintln!(
                "`derrick run add-feature` is implemented in T009. Until then, see tickets/T009-derrick-flow-minimal.md."
            );
            Ok(CliExitCode::Failure)
        }
    }
}
