use clap::Command;
use clap_complete::generate;

use crate::commands::CompletionsArgs;

pub(crate) fn execute(args: CompletionsArgs, command: &mut Command) {
    let shell: clap_complete::Shell = args.shell.into();
    generate(shell, command, "derrick", &mut std::io::stdout());
}
