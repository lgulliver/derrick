//! `derrick add` — positional-prompt shorthand for `derrick run add-feature`.
//!
//! All flags are identical; the only difference is that the feature description
//! is a positional argument rather than `--prompt "..."`, making one-liners
//! feel natural:
//!
//! ```text
//! derrick add "build a webhook ingest endpoint with idempotent dedupe"
//! ```
//!
//! The implementation simply converts [`AddArgs`] into [`AddFeatureArgs`] and
//! delegates to [`super::run::execute`] so the two paths stay in sync.

use crate::commands::{AddArgs, AddFeatureArgs, RunArgs, RunCommand};
use crate::exit_code::CliExitCode;
use crate::CliError;

pub(crate) async fn execute(args: AddArgs) -> Result<CliExitCode, CliError> {
    let add_feature = AddFeatureArgs {
        prompt: args.prompt,
        resume_from: args.resume_from,
        run_id: args.run_id,
        skip: args.skip,
        unskip: args.unskip,
        dry_run: args.dry_run,
        no_clarify: args.no_clarify,
        no_assay: args.no_assay,
    };
    super::run::execute(RunArgs {
        command: RunCommand::AddFeature(add_feature),
    })
    .await
}

#[cfg(test)]
mod tests {
    use crate::commands::AddArgs;

    fn default_args() -> AddArgs {
        AddArgs {
            prompt: None,
            resume_from: None,
            run_id: None,
            skip: vec![],
            unskip: vec![],
            dry_run: false,
            no_clarify: false,
            no_assay: false,
        }
    }

    #[test]
    fn add_args_converts_prompt() {
        let args = AddArgs {
            prompt: Some("build a webhook endpoint".to_owned()),
            ..default_args()
        };
        assert_eq!(args.prompt.as_deref(), Some("build a webhook endpoint"));
    }

    #[test]
    fn add_args_skip_flags_independent() {
        let args = AddArgs {
            no_clarify: true,
            no_assay: true,
            ..default_args()
        };
        assert!(args.no_clarify);
        assert!(args.no_assay);
    }
}
