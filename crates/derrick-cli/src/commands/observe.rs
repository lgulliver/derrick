//! `derrick observe` subcommand — see T016.

use derrick_tui::Tab;

use crate::commands::ObserveArgs;
use crate::exit_code::CliExitCode;
use crate::{CliError, message};

/// Executes the `derrick observe` subcommand (launches the TUI dashboard).
pub(crate) async fn execute(args: ObserveArgs) -> Result<CliExitCode, CliError> {
    let initial_tab = match args.tab.as_deref() {
        Some(name) => name
            .parse::<Tab>()
            .map_err(|e| message(format!("unknown --tab value: {e}")))?,
        None => Tab::default(),
    };

    // `--read-only` is accepted in v1 with no behavioural difference; the
    // TUI never writes to the substrate today.
    let _ = args.read_only;

    derrick_observe::observe(initial_tab, args.site.clone())
        .await
        .map_err(|e| message(format!("observe: {e}")))?;
    Ok(CliExitCode::Success)
}
