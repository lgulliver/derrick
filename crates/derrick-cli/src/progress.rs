//! Live CLI rendering of pipeline progress (run-feedback Layer 1).
//!
//! Implements [`derrick_flow::ProgressReporter`] with `indicatif`: an animated
//! spinner per running step on a TTY — showing an `i/total` counter and live
//! elapsed time — with each step resolving to a `✓ / ⏭ / ⚠ / ✗` line carrying
//! its duration and token cost. When stderr is not a terminal (CI logs, pipes)
//! or `NO_COLOR` is set, it degrades to plain status lines with no animation.
//!
//! The orchestrator (`derrick-flow`) stays UI-free; all terminal handling lives
//! here.

use std::collections::HashMap;
use std::io::IsTerminal;
use std::sync::Mutex;
use std::time::Duration;

use derrick_flow::{ProgressReporter, RunProgress, RunStatus, StepProgress, StepStatus};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use owo_colors::OwoColorize;

const TICK_STRINGS: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", "⠿"];

/// Renders pipeline progress to stderr.
pub(crate) struct CliReporter {
    multi: MultiProgress,
    bars: Mutex<HashMap<String, ProgressBar>>,
    styled: bool,
}

impl CliReporter {
    /// Creates a new reporter, enabling styled output when stderr is a TTY.
    pub(crate) fn new() -> Self {
        let styled = std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();
        Self {
            multi: MultiProgress::new(),
            bars: Mutex::new(HashMap::new()),
            styled,
        }
    }

    /// Print a finished line, routing above any active spinners on a TTY.
    fn emit(&self, line: String) {
        if self.styled {
            // `println` draws above the live bars without clobbering them.
            let _ = self.multi.println(line);
        } else {
            eprintln!("{line}");
        }
    }
}

impl ProgressReporter for CliReporter {
    fn pipeline_started(&self, pipeline_id: &str, run_id: &str, _total_steps: usize) {
        if self.styled {
            self.emit(format!(
                "{} {}  {}",
                "▸".cyan(),
                pipeline_id.bold(),
                format!("run {run_id}").bright_black()
            ));
        } else {
            self.emit(format!("pipeline {pipeline_id} (run {run_id})"));
        }
    }

    fn step_started(&self, step_id: &str, index: usize, total: usize, interactive: bool) {
        let counter = if total > 0 {
            format!("[{index}/{total}] ")
        } else {
            String::new()
        };

        // Interactive steps read stdin; a steady-tick spinner would clobber the
        // prompt, so we just print a static line and let the step own the
        // terminal. Non-styled output never animates either.
        if !self.styled || interactive {
            self.emit(format!("  {} {counter}{step_id}", "▸".cyan()));
            return;
        }

        let pb = self.multi.add(ProgressBar::new_spinner());
        pb.set_style(
            ProgressStyle::with_template("  {spinner:.cyan} {prefix}{msg} {elapsed:.dimmed}")
                .expect("static spinner template is valid")
                .tick_strings(TICK_STRINGS),
        );
        // Static label in the prefix; the live output tail goes in the message.
        pb.set_prefix(format!("{counter}{step_id}"));
        pb.enable_steady_tick(Duration::from_millis(90));
        self.bars.lock().unwrap().insert(step_id.to_owned(), pb);
    }

    fn step_output(&self, step_id: &str, line: &str) {
        let Some(snippet) = tail_snippet(line) else {
            return;
        };
        if let Some(bar) = self.bars.lock().unwrap().get(step_id) {
            bar.set_message(format!("  {}", snippet.bright_black()));
        }
    }

    fn step_finished(&self, progress: StepProgress<'_>) {
        if let Some(bar) = self.bars.lock().unwrap().remove(progress.step_id) {
            bar.finish_and_clear();
        }
        self.emit(self.format_outcome(&progress));
    }

    fn pipeline_finished(&self, progress: RunProgress<'_>) {
        self.emit(self.format_summary(&progress));
    }
}

impl CliReporter {
    /// Formats a completed step as a resolved status line with elapsed time and token cost.
    fn format_outcome(&self, p: &StepProgress<'_>) -> String {
        let (glyph, word) = outcome_glyph(p.status);
        let mut suffix = String::new();
        if let Some(elapsed) = format_elapsed(p.elapsed) {
            suffix.push_str(&format!("  {elapsed}"));
        }
        if let Some(tokens) = format_tokens(p.tokens_in, p.tokens_out) {
            suffix.push_str(&format!("  {tokens}"));
        }
        if self.styled {
            let glyph = match p.status {
                StepStatus::Success => glyph.green().to_string(),
                StepStatus::Skipped => glyph.bright_cyan().to_string(),
                StepStatus::Halted => glyph.yellow().to_string(),
                StepStatus::Failed => glyph.red().to_string(),
            };
            format!(
                "  {} {} {}{}",
                p.step_id.cyan(),
                glyph,
                word.bright_black(),
                suffix.bright_black()
            )
        } else {
            format!("  {} {glyph} {word}{suffix}", p.step_id)
        }
    }

    /// Formats the overall pipeline result as a summary line.
    fn format_summary(&self, p: &RunProgress<'_>) -> String {
        let (glyph, word) = run_glyph(p.status);
        let mut parts = vec![format!("run {}", p.run_id), word.to_owned()];
        if let Some(tokens) = format_tokens_u64(p.tokens_in, p.tokens_out) {
            parts.push(tokens);
        }
        if let Some(elapsed) = format_elapsed(p.elapsed) {
            parts.push(elapsed);
        }
        let body = parts.join(" · ");
        if self.styled {
            let glyph = match p.status {
                RunStatus::Success => glyph.green().to_string(),
                RunStatus::Halted => glyph.yellow().to_string(),
                RunStatus::Failed => glyph.red().to_string(),
            };
            format!("{glyph} {}", body.bold())
        } else {
            format!("{glyph} {body}")
        }
    }
}

/// Returns the glyph and word for a completed step status.
fn outcome_glyph(status: StepStatus) -> (&'static str, &'static str) {
    match status {
        StepStatus::Success => ("✓", "done"),
        StepStatus::Skipped => ("⏭", "skipped"),
        StepStatus::Halted => ("⚠", "halted"),
        StepStatus::Failed => ("✗", "failed"),
    }
}

/// Returns the glyph and word for an overall run status.
fn run_glyph(status: RunStatus) -> (&'static str, &'static str) {
    match status {
        RunStatus::Success => ("✓", "success"),
        RunStatus::Halted => ("⚠", "halted"),
        RunStatus::Failed => ("✗", "failed"),
    }
}

/// Format a token delta like `↑1.2k ↓800`, or `None` when both are zero.
fn format_tokens(tokens_in: u32, tokens_out: u32) -> Option<String> {
    format_tokens_u64(u64::from(tokens_in), u64::from(tokens_out))
}

/// Format a u64 token delta, or `None` when both are zero.
fn format_tokens_u64(tokens_in: u64, tokens_out: u64) -> Option<String> {
    if tokens_in == 0 && tokens_out == 0 {
        return None;
    }
    Some(format!(
        "↑{} ↓{}",
        humanize_count(tokens_in),
        humanize_count(tokens_out)
    ))
}

/// Compact count: `800`, `1.2k`, `3.4M`.
fn humanize_count(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

/// Condense a raw output line into a short single-line heartbeat for the
/// spinner: collapse internal whitespace, drop control characters, and truncate
/// with an ellipsis. Returns `None` for blank lines (don't disturb the spinner).
fn tail_snippet(line: &str) -> Option<String> {
    const MAX: usize = 72;
    let collapsed: String = line
        .split_whitespace()
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let cleaned: String = collapsed.chars().filter(|c| !c.is_control()).collect();
    if cleaned.is_empty() {
        return None;
    }
    if cleaned.chars().count() > MAX {
        let truncated: String = cleaned.chars().take(MAX - 1).collect();
        Some(format!("{truncated}…"))
    } else {
        Some(cleaned)
    }
}

/// Format a duration like `3.2s` or `1m04s`, or `None` when zero (skipped).
fn format_elapsed(elapsed: Duration) -> Option<String> {
    if elapsed.is_zero() {
        return None;
    }
    let secs = elapsed.as_secs();
    if secs < 60 {
        let frac = elapsed.as_millis() as f64 / 1000.0;
        Some(format!("{frac:.1}s"))
    } else {
        Some(format!("{}m{:02}s", secs / 60, secs % 60))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyphs_cover_every_status() {
        assert_eq!(outcome_glyph(StepStatus::Success).1, "done");
        assert_eq!(outcome_glyph(StepStatus::Skipped).1, "skipped");
        assert_eq!(outcome_glyph(StepStatus::Halted).1, "halted");
        assert_eq!(outcome_glyph(StepStatus::Failed).1, "failed");
        assert_eq!(run_glyph(RunStatus::Success).1, "success");
        assert_eq!(run_glyph(RunStatus::Halted).1, "halted");
        assert_eq!(run_glyph(RunStatus::Failed).1, "failed");
    }

    #[test]
    fn tokens_hidden_when_zero() {
        assert_eq!(format_tokens(0, 0), None);
        assert_eq!(format_tokens(800, 0), Some("↑800 ↓0".to_owned()));
    }

    #[test]
    fn token_counts_humanize() {
        assert_eq!(humanize_count(800), "800");
        assert_eq!(humanize_count(1_200), "1.2k");
        assert_eq!(humanize_count(3_400_000), "3.4M");
    }

    #[test]
    fn elapsed_hidden_when_zero_and_formats_minutes() {
        assert_eq!(format_elapsed(Duration::ZERO), None);
        assert_eq!(
            format_elapsed(Duration::from_millis(3210)),
            Some("3.2s".to_owned())
        );
        assert_eq!(
            format_elapsed(Duration::from_secs(64)),
            Some("1m04s".to_owned())
        );
    }

    #[test]
    fn tail_snippet_blanks_and_truncates() {
        assert_eq!(tail_snippet("   "), None);
        assert_eq!(tail_snippet("\t\n"), None);
        assert_eq!(
            tail_snippet("  reading   src/main.rs  "),
            Some("reading src/main.rs".to_owned())
        );
        let long = "x".repeat(100);
        let snippet = tail_snippet(&long).unwrap();
        assert_eq!(snippet.chars().count(), 72);
        assert!(snippet.ends_with('…'));
    }

    #[test]
    fn plain_outcome_line_has_no_ansi() {
        let reporter = CliReporter {
            multi: MultiProgress::new(),
            bars: Mutex::new(HashMap::new()),
            styled: false,
        };
        let line = reporter.format_outcome(&StepProgress {
            step_id: "plan",
            status: StepStatus::Success,
            tokens_in: 1200,
            tokens_out: 800,
            elapsed: Duration::from_millis(2500),
        });
        assert_eq!(line, "  plan ✓ done  2.5s  ↑1.2k ↓800");
        assert!(
            !line.contains('\u{1b}'),
            "plain line must not contain ANSI escapes"
        );
    }

    #[test]
    fn plain_summary_line_reads_cleanly() {
        let reporter = CliReporter {
            multi: MultiProgress::new(),
            bars: Mutex::new(HashMap::new()),
            styled: false,
        };
        let line = reporter.format_summary(&RunProgress {
            run_id: "run-1",
            status: RunStatus::Success,
            tokens_in: 12_300,
            tokens_out: 4_500,
            elapsed: Duration::from_secs(134),
        });
        assert_eq!(line, "✓ run run-1 · success · ↑12.3k ↓4.5k · 2m14s");
    }
}
