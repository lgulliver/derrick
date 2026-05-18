//! Binary entry point for the derrick command.

#![allow(clippy::print_stdout)]
#![allow(clippy::print_stderr)]

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    derrick_cli::run(std::env::args_os()).await
}
