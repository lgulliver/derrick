#![allow(clippy::print_stdout)]
#![allow(clippy::print_stderr)]

use std::io::{self, Read, Write};

use anyhow::Result;
use derrick_scrub::Scrubber;

use crate::commands::ScrubArgs;

/// Runs the `derrick scrub` subcommand (redacts sensitive data from a transcript on stdin).
pub(crate) async fn run(args: ScrubArgs) -> Result<()> {
    let mut input = Vec::new();
    io::stdin().lock().read_to_end(&mut input)?;

    let scrubber = Scrubber::with_defaults();
    let (output, stats) = scrubber.scrub(&args.tool, &input);

    io::stdout().lock().write_all(&output)?;

    if args.stats {
        let rules: u64 = stats.rules_fired.values().sum();
        eprintln!(
            "scrub [{}]: {}B \u{2192} {}B ({:.1}% saved, {} rules fired)",
            args.tool,
            stats.bytes_in,
            stats.bytes_out,
            stats.savings_pct(),
            rules,
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use derrick_scrub::Scrubber;

    #[test]
    fn scrub_git_drops_counting_line() {
        let scrubber = Scrubber::with_defaults();
        let (output, _) = scrubber.scrub("git", b"remote: Counting objects: 1\nok\n");
        assert_eq!(output, b"ok\n");
    }

    #[test]
    fn scrub_unknown_tool_passthrough() {
        let scrubber = Scrubber::with_defaults();
        let input = b"hello\n";
        let (output, stats) = scrubber.scrub("unknown_tool_xyz", input);
        assert_eq!(output, input);
        assert_eq!(stats.bytes_in, stats.bytes_out);
    }
}
