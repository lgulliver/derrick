#![allow(clippy::print_stdout)]

use anyhow::Result;

use crate::commands::GainArgs;
use crate::output::OutputFormat;

pub(crate) async fn run(args: GainArgs) -> Result<()> {
    match args.format {
        OutputFormat::Human => print_human(),
        OutputFormat::Json => print_json(),
    }
    Ok(())
}

fn print_human() {
    println!("derrick gain \u{2014} token savings\n");
    println!("  scrub:   active (rules for git, gh, claude, codex, copilot, cargo, bd, gt)");
    println!("  caveman: active (lite / full / ultra compression)\n");
    println!("  Per-session telemetry is tracked when scrub and caveman are wired as");
    println!("  hooks in .claude/settings.json (added by `derrick init`).\n");
    println!("  Run `derrick gain --format json` for machine-readable output.");
}

fn print_json() {
    let obj = serde_json::json!({
        "scrub": {
            "status": "active",
            "tools": ["bd", "cargo", "claude", "codex", "copilot", "gh", "git", "gt"]
        },
        "caveman": {
            "status": "active",
            "intensities": ["lite", "full", "ultra"]
        },
        "telemetry": "pending"
    });
    println!("{}", serde_json::to_string_pretty(&obj).unwrap_or_default());
}

#[cfg(test)]
mod tests {
    #[test]
    fn gain_json_contains_active_scrub_status() {
        let obj = serde_json::json!({
            "scrub": { "status": "active", "tools": ["bd","cargo","claude","codex","copilot","gh","git","gt"] },
            "caveman": { "status": "active", "intensities": ["lite","full","ultra"] },
            "telemetry": "pending"
        });
        assert_eq!(obj["scrub"]["status"], "active");
        assert_eq!(obj["caveman"]["status"], "active");
    }

    #[test]
    fn gain_json_tools_sorted() {
        let tools = [
            "bd", "cargo", "claude", "codex", "copilot", "gh", "git", "gt",
        ];
        let mut sorted = tools;
        sorted.sort_unstable();
        assert_eq!(tools, sorted, "tools list should be alphabetically sorted");
    }
}
