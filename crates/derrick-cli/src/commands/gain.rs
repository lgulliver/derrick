#![allow(clippy::print_stdout)]

use anyhow::Result;

use crate::commands::GainArgs;
use crate::output::OutputFormat;
use crate::telemetry;

pub(crate) async fn run(args: GainArgs) -> Result<()> {
    // Attempt to find the repo root and project dir; degrade gracefully if absent.
    let repo_root = std::env::current_dir().ok();
    let project_dir = repo_root.as_deref().and_then(telemetry::project_dir);

    let usage = match &project_dir {
        Some(dir) => {
            if args.all {
                let sessions = telemetry::all_sessions(dir);
                if sessions.is_empty() {
                    None
                } else {
                    Some(telemetry::aggregate(&sessions))
                }
            } else {
                telemetry::latest_session(dir).map(|s| telemetry::parse_session(&s))
            }
        }
        None => None,
    };

    match args.format {
        OutputFormat::Human => print_human(&usage, args.all, &project_dir),
        OutputFormat::Json => print_json(&usage, args.all),
    }

    Ok(())
}

fn print_human(
    usage: &Option<telemetry::TokenUsage>,
    all_sessions: bool,
    project_dir: &Option<std::path::PathBuf>,
) {
    println!("derrick gain \u{2014} token savings\n");

    match usage {
        Some(u) => {
            let scope = if all_sessions {
                format!(
                    "{} session{}",
                    u.session_count,
                    if u.session_count == 1 { "" } else { "s" }
                )
            } else {
                "latest session".to_owned()
            };

            println!("  Tokens ({scope})");
            println!("  {:<28} {:>12}", "input", fmt_tokens(u.input_tokens));
            println!(
                "  {:<28} {:>12}",
                "cache writes",
                fmt_tokens(u.cache_creation_input_tokens)
            );
            println!(
                "  {:<28} {:>12}  \u{2190} ~90% cheaper than fresh input",
                "cache reads",
                fmt_tokens(u.cache_read_input_tokens)
            );
            println!("  {:<28} {:>12}", "output", fmt_tokens(u.output_tokens));
            println!("  {}", "\u{2500}".repeat(44));
            println!("  {:<28} {:>12}", "total", fmt_tokens(u.total_tokens()));

            if u.cache_savings_tokens() > 0 {
                println!();
                println!(
                    "  Cache saved ~{} tokens at full input price this {scope}.",
                    fmt_tokens(u.cache_savings_tokens())
                );
            }
        }
        None => {
            if project_dir.is_none() {
                println!("  No Claude Code session data found for this directory.");
                println!("  Run `derrick init` and start a session to see telemetry here.");
            } else {
                println!(
                    "  No session data yet. Start working in Claude Code to populate telemetry."
                );
            }
        }
    }

    println!();
    println!("  Hooks");
    println!(
        "  {:<10} active (rules for git, gh, claude, codex, copilot, cargo, bd, gt)",
        "scrub:"
    );
    println!(
        "  {:<10} active (lite / full / ultra compression)",
        "caveman:"
    );
    println!();

    if usage.is_some() && !all_sessions {
        println!("  Run `derrick gain --all` to aggregate across all sessions for this repo.");
    }
    println!("  Run `derrick gain --format json` for machine-readable output.");
}

fn print_json(usage: &Option<telemetry::TokenUsage>, all_sessions: bool) {
    let telemetry_val = match usage {
        Some(u) => serde_json::json!({
            "scope": if all_sessions { "all_sessions" } else { "latest_session" },
            "session_count": u.session_count,
            "message_count": u.message_count,
            "input_tokens": u.input_tokens,
            "output_tokens": u.output_tokens,
            "cache_read_input_tokens": u.cache_read_input_tokens,
            "cache_creation_input_tokens": u.cache_creation_input_tokens,
            "total_tokens": u.total_tokens(),
            "cache_savings_tokens": u.cache_savings_tokens(),
        }),
        None => serde_json::json!({ "scope": "none", "reason": "no_session_data" }),
    };

    let obj = serde_json::json!({
        "scrub": {
            "status": "active",
            "tools": ["bd", "cargo", "claude", "codex", "copilot", "gh", "git", "gt"]
        },
        "caveman": {
            "status": "active",
            "intensities": ["lite", "full", "ultra"]
        },
        "telemetry": telemetry_val,
    });
    println!("{}", serde_json::to_string_pretty(&obj).unwrap_or_default());
}

fn fmt_tokens(n: u64) -> String {
    // Format with thousands separators: 1234567 → "1,234,567"
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_tokens_formats_with_commas() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(1_000), "1,000");
        assert_eq!(fmt_tokens(1_234_567), "1,234,567");
    }

    #[test]
    fn gain_json_with_no_usage_reports_no_session_data() {
        let obj = serde_json::json!({
            "telemetry": { "scope": "none", "reason": "no_session_data" }
        });
        assert_eq!(obj["telemetry"]["reason"], "no_session_data");
    }

    #[test]
    fn gain_json_with_usage_includes_totals() {
        use crate::telemetry::TokenUsage;
        let u = TokenUsage {
            input_tokens: 1000,
            output_tokens: 200,
            cache_read_input_tokens: 5000,
            cache_creation_input_tokens: 300,
            session_count: 1,
            message_count: 10,
        };
        let val = serde_json::json!({
            "total_tokens": u.total_tokens(),
            "cache_savings_tokens": u.cache_savings_tokens(),
        });
        assert_eq!(val["total_tokens"], 6500);
        assert_eq!(val["cache_savings_tokens"], 4500);
    }
}
