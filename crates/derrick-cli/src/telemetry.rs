//! Claude Code session transcript parser for token telemetry (D14).
//!
//! Reads `~/.claude/projects/<repo-key>/<session>.jsonl` files and
//! aggregates token usage. Message deduplication is applied by `message.id`
//! since sidechain branching causes ~50% of lines to be duplicates.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Aggregated token usage from one or more sessions.
#[derive(Debug, Default, Clone)]
pub(crate) struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    /// Number of sessions contributing to this total.
    pub session_count: usize,
    /// Number of unique messages counted (after deduplication).
    pub message_count: usize,
}

impl TokenUsage {
    /// Sum of all token dimensions.
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_read_input_tokens)
            .saturating_add(self.cache_creation_input_tokens)
    }

    /// Tokens avoided at full input price due to cache hits.
    /// Anthropic cache reads cost ~10% of fresh input tokens, so 90% is saved.
    pub fn cache_savings_tokens(&self) -> u64 {
        (self.cache_read_input_tokens as f64 * 0.9) as u64
    }
}

// ── serde types ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct TranscriptLine {
    message: Option<TranscriptMessage>,
}

#[derive(Deserialize)]
struct TranscriptMessage {
    id: Option<String>,
    usage: Option<RawUsage>,
}

#[derive(Deserialize)]
struct RawUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
}

// ── public helpers ────────────────────────────────────────────────────────────

/// Return the Claude Code project directory for `repo_root`, if it exists.
///
/// The directory key is the repo path with every `/` replaced by `-`.
/// Example: `/Users/alice/repos/derrick` → `-Users-alice-repos-derrick`
pub(crate) fn project_dir(repo_root: &Path) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let key = repo_root.to_str()?.replace('/', "-");
    let path = PathBuf::from(home)
        .join(".claude")
        .join("projects")
        .join(key);
    if path.is_dir() {
        Some(path)
    } else {
        None
    }
}

/// Return the path to the most-recently-modified session file, or `None`.
pub(crate) fn latest_session(project_dir: &Path) -> Option<PathBuf> {
    let mut entries = jsonl_entries(project_dir);
    entries.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    entries.into_iter().next_back()
}

/// Return all session files sorted oldest → newest.
pub(crate) fn all_sessions(project_dir: &Path) -> Vec<PathBuf> {
    let mut entries = jsonl_entries(project_dir);
    entries.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    entries
}

/// Parse a single session file and return deduplicated token totals.
pub(crate) fn parse_session(path: &Path) -> TokenUsage {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return TokenUsage::default(),
    };

    let mut seen: HashSet<String> = HashSet::new();
    let mut usage = TokenUsage {
        session_count: 1,
        ..Default::default()
    };

    for line in content.lines() {
        let entry = match serde_json::from_str::<TranscriptLine>(line) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let Some(msg) = entry.message else { continue };
        let Some(u) = msg.usage else { continue };

        // Deduplicate sidechain copies by message ID.
        if let Some(id) = msg.id {
            if !seen.insert(id) {
                continue;
            }
        }

        usage.input_tokens = usage
            .input_tokens
            .saturating_add(u.input_tokens.unwrap_or(0));
        usage.output_tokens = usage
            .output_tokens
            .saturating_add(u.output_tokens.unwrap_or(0));
        usage.cache_read_input_tokens = usage
            .cache_read_input_tokens
            .saturating_add(u.cache_read_input_tokens.unwrap_or(0));
        usage.cache_creation_input_tokens = usage
            .cache_creation_input_tokens
            .saturating_add(u.cache_creation_input_tokens.unwrap_or(0));
        usage.message_count += 1;
    }

    usage
}

/// Aggregate usage across multiple session files.
pub(crate) fn aggregate(sessions: &[PathBuf]) -> TokenUsage {
    let mut total = TokenUsage::default();
    for path in sessions {
        let s = parse_session(path);
        total.input_tokens = total.input_tokens.saturating_add(s.input_tokens);
        total.output_tokens = total.output_tokens.saturating_add(s.output_tokens);
        total.cache_read_input_tokens = total
            .cache_read_input_tokens
            .saturating_add(s.cache_read_input_tokens);
        total.cache_creation_input_tokens = total
            .cache_creation_input_tokens
            .saturating_add(s.cache_creation_input_tokens);
        total.session_count += 1;
        total.message_count += s.message_count;
    }
    total
}

// ── private helpers ───────────────────────────────────────────────────────────

fn jsonl_entries(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "jsonl"))
        .collect()
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_jsonl(lines: &[&str]) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
        f
    }

    #[test]
    fn parse_session_sums_unique_messages() {
        let f = write_jsonl(&[
            r#"{"message":{"id":"msg_1","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":200}}}"#,
            r#"{"message":{"id":"msg_2","usage":{"input_tokens":30,"output_tokens":20,"cache_read_input_tokens":500,"cache_creation_input_tokens":0}}}"#,
        ]);
        let u = parse_session(f.path());
        assert_eq!(u.input_tokens, 130);
        assert_eq!(u.output_tokens, 70);
        assert_eq!(u.cache_read_input_tokens, 500);
        assert_eq!(u.cache_creation_input_tokens, 200);
        assert_eq!(u.message_count, 2);
    }

    #[test]
    fn parse_session_deduplicates_by_id() {
        let f = write_jsonl(&[
            r#"{"message":{"id":"msg_1","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#,
            // exact duplicate — same id, same usage
            r#"{"message":{"id":"msg_1","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#,
        ]);
        let u = parse_session(f.path());
        assert_eq!(u.input_tokens, 100, "duplicate should be skipped");
        assert_eq!(u.message_count, 1);
    }

    #[test]
    fn parse_session_skips_lines_without_usage() {
        let f = write_jsonl(&[
            r#"{"message":{"id":"msg_1","role":"user","content":"hello"}}"#,
            r#"{"type":"system"}"#,
            r#"not json at all"#,
            r#"{"message":{"id":"msg_2","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#,
        ]);
        let u = parse_session(f.path());
        assert_eq!(u.input_tokens, 10);
        assert_eq!(u.message_count, 1);
    }

    #[test]
    fn cache_savings_tokens_is_90_pct_of_reads() {
        let u = TokenUsage {
            cache_read_input_tokens: 1000,
            ..Default::default()
        };
        assert_eq!(u.cache_savings_tokens(), 900);
    }

    #[test]
    fn total_tokens_sums_all_dimensions() {
        let u = TokenUsage {
            input_tokens: 10,
            output_tokens: 20,
            cache_read_input_tokens: 30,
            cache_creation_input_tokens: 40,
            ..Default::default()
        };
        assert_eq!(u.total_tokens(), 100);
    }

    #[test]
    fn project_dir_returns_none_for_nonexistent_path() {
        let fake = std::path::Path::new("/nonexistent/repo/path/that/does/not/exist");
        assert!(project_dir(fake).is_none());
    }
}
