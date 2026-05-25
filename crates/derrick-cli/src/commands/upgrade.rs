use crate::commands::UpgradeArgs;
use crate::exit_code::CliExitCode;

pub(crate) async fn execute(args: UpgradeArgs) -> Result<CliExitCode, crate::CliError> {
    let _ = (args.check, args.force);
    println!("upgrade not yet available, re-run the install script");
    Ok(CliExitCode::Success)
}
