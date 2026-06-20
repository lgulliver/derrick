//! `derrick reset` — re-scaffold `.claude/` skills and hooks from the current
//! `derrick.yaml`, preserving the config file and the `.derrick/` database.
use crate::commands::ResetArgs;
use crate::exit_code::CliExitCode;
use crate::{CliError, current_repo_root, message};

pub(crate) async fn execute(args: ResetArgs) -> Result<CliExitCode, CliError> {
    let repo_root = current_repo_root()?;
    if !repo_root.join("derrick.yaml").exists() {
        return Err(message("derrick.yaml not found — run `derrick init` first"));
    }
    crate::commands::init::execute_reset(&repo_root, args.yes, args.dry_run).await
}
