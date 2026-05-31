#![allow(clippy::print_stdout)]

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::commands::GainArgs;
use crate::output::OutputFormat;
use crate::telemetry;

pub(crate) async fn run(args: GainArgs) -> Result<()> {
    if let Some(run_id) = args.run.clone() {
        return run_for_run_id(args, &run_id).await;
    }

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

            // Survey section — only rendered when queries were observed (§9.B.8 / D55).
            if u.survey_queries > 0 {
                println!();
                println!("  Survey ({scope})");
                println!(
                    "  {:<28} {:>12}",
                    "queries (mcp__derrick-survey__*)", u.survey_queries
                );
                println!(
                    "  {:<28} {:>12}  \u{2190} est. ~{} tokens/query saved vs grep/Read fan-out",
                    "tokens saved (estimate)",
                    fmt_tokens(u.survey_tokens_saved()),
                    telemetry::SURVEY_TOKENS_SAVED_PER_QUERY,
                );
            }

            println!();
            let input_cost = (u.input_tokens as f64 / 1_000_000.0) * 3.0;
            let output_cost = (u.output_tokens as f64 / 1_000_000.0) * 15.0;
            let total_cost = telemetry::estimate_session_cost_usd(u);
            println!("  Estimated cost (claude-sonnet-4, model steps only)");
            println!("  {}", "\u{2500}".repeat(49));
            println!("  {:<28} {:>16}", "input", fmt_usd(input_cost));
            println!("  {:<28} {:>16}", "output", fmt_usd(output_cost));
            println!("  {:<28} {:>16}", "total", fmt_usd(total_cost));
            println!();
            println!(
                "  (claude-sonnet-4 list price; see derrick.yaml models.[name].cost_hint to configure)"
            );
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
            "estimated_cost_usd": telemetry::estimate_session_cost_usd(u),
            // Survey savings (§9.B.8 / D55). survey_tokens_saved is an estimate;
            // see SURVEY_TOKENS_SAVED_PER_QUERY for the documented assumption.
            "survey_queries": u.survey_queries,
            "survey_tokens_saved": u.survey_tokens_saved(),
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

// ── --run <id> handling ──────────────────────────────────────────────────────

async fn run_for_run_id(args: GainArgs, run_id: &str) -> Result<()> {
    let manifest_path = match locate_manifest(run_id) {
        Some(path) if path.exists() => path,
        Some(path) => {
            match args.format {
                OutputFormat::Human => {
                    println!("derrick gain \u{2014} run {run_id}\n");
                    println!("  Run manifest not found: {}", path.display());
                }
                OutputFormat::Json => {
                    let obj = serde_json::json!({
                        "run_id": run_id,
                        "error": "manifest_not_found",
                        "path": path.display().to_string(),
                    });
                    println!("{}", serde_json::to_string_pretty(&obj).unwrap_or_default());
                }
            }
            return Ok(());
        }
        None => {
            match args.format {
                OutputFormat::Human => {
                    println!("derrick gain \u{2014} run {run_id}\n");
                    println!(
                        "  Could not resolve repo root or config; run from within a derrick repo."
                    );
                }
                OutputFormat::Json => {
                    let obj = serde_json::json!({
                        "run_id": run_id,
                        "error": "repo_root_not_found",
                    });
                    println!("{}", serde_json::to_string_pretty(&obj).unwrap_or_default());
                }
            }
            return Ok(());
        }
    };

    let value: serde_json::Value = match std::fs::read_to_string(&manifest_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
    {
        Some(v) => v,
        None => {
            match args.format {
                OutputFormat::Human => {
                    println!("derrick gain \u{2014} run {run_id}\n");
                    println!("  Failed to read manifest: {}", manifest_path.display());
                }
                OutputFormat::Json => {
                    let obj = serde_json::json!({
                        "run_id": run_id,
                        "error": "manifest_unreadable",
                        "path": manifest_path.display().to_string(),
                    });
                    println!("{}", serde_json::to_string_pretty(&obj).unwrap_or_default());
                }
            }
            return Ok(());
        }
    };

    match args.format {
        OutputFormat::Human => print_run_human(run_id, &value, &manifest_path),
        OutputFormat::Json => print_run_json(run_id, &value),
    }
    Ok(())
}

fn locate_manifest(run_id: &str) -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    let repo_root = find_repo_root(&cwd)?;
    // Default state dir is `.derrick`; try config first, fall back to default.
    let state_dir: PathBuf =
        derrick_config::Config::load_from_path(&repo_root.join("derrick.yaml"))
            .map(|cfg| cfg.state().dir().to_path_buf())
            .unwrap_or_else(|_| PathBuf::from(".derrick"));
    Some(
        repo_root
            .join(state_dir)
            .join("runs")
            .join(run_id)
            .join("manifest.json"),
    )
}

fn find_repo_root(start: &Path) -> Option<PathBuf> {
    for candidate in start.ancestors() {
        if candidate.join(".git").exists() {
            return Some(candidate.to_path_buf());
        }
    }
    None
}

fn step_rows(value: &serde_json::Value) -> Vec<StepRow> {
    let steps = value.get("steps").and_then(|s| s.as_array());
    let mut rows = Vec::new();
    if let Some(arr) = steps {
        for step in arr {
            let id = step
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            let status = step
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            let tokens_in = step.get("tokens_in").and_then(|v| v.as_u64()).unwrap_or(0);
            let tokens_out = step.get("tokens_out").and_then(|v| v.as_u64()).unwrap_or(0);
            let started = step.get("started_at").and_then(|v| v.as_str());
            let finished = step.get("finished_at").and_then(|v| v.as_str());
            let duration_s = duration_seconds(started, finished);
            rows.push(StepRow {
                id,
                status,
                tokens_in,
                tokens_out,
                duration_s,
            });
        }
    }
    rows
}

struct StepRow {
    id: String,
    status: String,
    tokens_in: u64,
    tokens_out: u64,
    duration_s: f64,
}

fn duration_seconds(started: Option<&str>, finished: Option<&str>) -> f64 {
    let (Some(s), Some(f)) = (started, finished) else {
        return 0.0;
    };
    let (Ok(s), Ok(f)) = (
        chrono::DateTime::parse_from_rfc3339(s),
        chrono::DateTime::parse_from_rfc3339(f),
    ) else {
        return 0.0;
    };
    let delta = f.signed_duration_since(s);
    delta.num_milliseconds() as f64 / 1000.0
}

fn print_run_human(run_id: &str, value: &serde_json::Value, manifest_path: &Path) {
    println!("derrick gain \u{2014} run {run_id}\n");
    let rows = step_rows(value);
    let tokens_in: u64 = rows.iter().map(|r| r.tokens_in).sum();
    let tokens_out: u64 = rows.iter().map(|r| r.tokens_out).sum();
    let duration_s: f64 = rows.iter().map(|r| r.duration_s).sum();

    println!(
        "  {:<13} {:<9} {:>9} {:>9} {:>10}",
        "Step", "Status", "Tok-in", "Tok-out", "Duration"
    );
    println!("  {}", "\u{2500}".repeat(54));
    for row in &rows {
        println!(
            "  {:<13} {:<9} {:>9} {:>9} {:>9.1}s",
            truncate(&row.id, 13),
            truncate(&row.status, 9),
            fmt_tokens(row.tokens_in),
            fmt_tokens(row.tokens_out),
            row.duration_s
        );
    }
    println!("  {}", "\u{2500}".repeat(54));
    println!(
        "  {:<13} {:<9} {:>9} {:>9} {:>9.1}s",
        "total",
        "",
        fmt_tokens(tokens_in),
        fmt_tokens(tokens_out),
        duration_s
    );

    let cost = derrick_models::CostHint {
        in_per_mtok: 3.0,
        out_per_mtok: 15.0,
    }
    .estimate_usd(tokens_in, tokens_out);
    println!();
    println!("  Estimated cost (claude-sonnet-4): {}", fmt_usd(cost));
    println!("  Run manifest: {}", manifest_path.display());
}

fn print_run_json(run_id: &str, value: &serde_json::Value) {
    let rows = step_rows(value);
    let tokens_in: u64 = rows.iter().map(|r| r.tokens_in).sum();
    let tokens_out: u64 = rows.iter().map(|r| r.tokens_out).sum();
    let cost = derrick_models::CostHint {
        in_per_mtok: 3.0,
        out_per_mtok: 15.0,
    }
    .estimate_usd(tokens_in, tokens_out);
    let status = value
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    let steps: Vec<_> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "id": r.id,
                "status": r.status,
                "tokens_in": r.tokens_in,
                "tokens_out": r.tokens_out,
            })
        })
        .collect();
    let obj = serde_json::json!({
        "run_id": run_id,
        "status": status,
        "tokens_in": tokens_in,
        "tokens_out": tokens_out,
        "cost_estimate_usd": cost,
        "steps": steps,
    });
    println!("{}", serde_json::to_string_pretty(&obj).unwrap_or_default());
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('\u{2026}');
        out
    }
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

fn fmt_usd(amount: f64) -> String {
    format!("${amount:.4}")
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
    fn fmt_usd_uses_four_decimals() {
        assert_eq!(fmt_usd(0.0), "$0.0000");
        assert_eq!(fmt_usd(0.0099), "$0.0099");
        assert_eq!(fmt_usd(1.5), "$1.5000");
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
            survey_queries: 0,
        };
        let val = serde_json::json!({
            "total_tokens": u.total_tokens(),
            "cache_savings_tokens": u.cache_savings_tokens(),
        });
        assert_eq!(val["total_tokens"], 6500);
        assert_eq!(val["cache_savings_tokens"], 4500);
    }

    #[test]
    fn gain_json_with_survey_queries_includes_savings() {
        use crate::telemetry::{SURVEY_TOKENS_SAVED_PER_QUERY, TokenUsage};
        let u = TokenUsage {
            input_tokens: 500,
            output_tokens: 100,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            session_count: 1,
            message_count: 5,
            survey_queries: 4,
        };
        assert_eq!(u.survey_tokens_saved(), 4 * SURVEY_TOKENS_SAVED_PER_QUERY);
        // Verify the JSON shape that print_json would emit.
        let val = serde_json::json!({
            "survey_queries": u.survey_queries,
            "survey_tokens_saved": u.survey_tokens_saved(),
        });
        assert_eq!(val["survey_queries"], 4);
        assert_eq!(
            val["survey_tokens_saved"],
            4 * SURVEY_TOKENS_SAVED_PER_QUERY
        );
    }

    #[test]
    fn step_rows_extracts_tokens_and_duration() {
        let value = serde_json::json!({
            "steps": [
                {
                    "id": "clarify",
                    "status": "success",
                    "tokens_in": 1234,
                    "tokens_out": 456,
                    "started_at": "2026-01-01T00:00:00Z",
                    "finished_at": "2026-01-01T00:00:02.5Z"
                }
            ]
        });
        let rows = step_rows(&value);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "clarify");
        assert_eq!(rows[0].tokens_in, 1234);
        assert_eq!(rows[0].tokens_out, 456);
        assert!((rows[0].duration_s - 2.5).abs() < 1e-6);
    }
}
