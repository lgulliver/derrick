// derrick-cli — the binary entry point.
// See DESIGN.md and AGENTS.md for the contract this implements.

#![allow(clippy::print_stdout)]
#![allow(clippy::print_stderr)]
// ^ The CLI legitimately prints to stdout/stderr. Other crates do not.

fn main() {
    println!("derrick: scaffold only — see DESIGN.md");
}
