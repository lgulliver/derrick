//! Claude Code session transcript parser for token telemetry (D14).
//!
//! Reads `~/.claude/projects/<repo-key>/<session>.jsonl` files and
//! aggregates token usage. Message deduplication is applied by `message.id`
//! since sidechain branching causes ~50% of lines to be duplicates.
//!
//! Also counts survey MCP tool-use entries (`mcp__derrick-survey__*`) per D55
//! and estimates tokens saved vs equivalent grep/Read fan-out.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Deserialize;

// ── Survey heuristic (D55 / §9.B.8) ─────────────────────────────────────────
//
// A survey query replaces a fan-out of grep/glob/Read calls. We attribute a
// flat, deliberately conservative 300 input tokens saved per query — roughly
// one avoided Read of a function-sized span (~200 lines at ~4 bytes/token
// minus overhead). It is a labelled estimate, not a measurement. The figure
// reconciles with `derrick gain` because it counts only avoided *input*
// tokens (file bytes that would otherwise enter the prompt), never output.
pub(crate) const SURVEY_TOKENS_SAVED_PER_QUERY: u64 = 300;

/// Survey MCP tool name prefix as it appears in Claude Code transcripts.
const SURVEY_TOOL_PREFIX: &str = "mcp__derrick-survey__";

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
    /// Number of `mcp__derrick-survey__*` tool-use calls across all sessions.
    pub survey_queries: u64,
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

    /// Estimated tokens saved by survey queries (conservative per-query heuristic).
    pub fn survey_tokens_saved(&self) -> u64 {
        self.survey_queries
            .saturating_mul(SURVEY_TOKENS_SAVED_PER_QUERY)
    }
}

/// Estimate USD cost for a session using claude-sonnet-4 pricing.
/// This is a rough estimate since session-level data doesn't have model breakdown.
pub(crate) fn estimate_session_cost_usd(usage: &TokenUsage) -> f64 {
    derrick_models::CostHint {
        in_per_mtok: 3.0,
        out_per_mtok: 15.0,
    }
    .estimate_usd(usage.input_tokens, usage.output_tokens)
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
    /// Content blocks — may contain `tool_use` entries (D55 survey counting).
    #[serde(default)]
    content: Vec<RawContentBlock>,
}

#[derive(Deserialize)]
struct RawUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
}

/// Minimal content block — we only need `type` and `name`.
#[derive(Deserialize)]
struct RawContentBlock {
    #[serde(rename = "type")]
    block_type: Option<String>,
    name: Option<String>,
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
    if path.is_dir() { Some(path) } else { None }
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
///
/// Also counts `mcp__derrick-survey__*` tool-use calls in message content
/// blocks (D55 / §9.B.8), deduplicated by message id the same way usage
/// fields are, so sidechain replays of a message don't double-count a query.
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

        // Count survey tool-use calls, skipping sidechain duplicates.
        // Survey queries are physical MCP round-trips; sidechain forks replay
        // the same message id, so we apply the same deduplication as for usage.
        let not_yet_seen = msg.id.as_deref().is_none_or(|id| !seen.contains(id));

        if not_yet_seen {
            let survey_calls = msg
                .content
                .iter()
                .filter(|block| {
                    block.block_type.as_deref().is_some_and(|t| t == "tool_use")
                        && block
                            .name
                            .as_deref()
                            .is_some_and(|n| n.starts_with(SURVEY_TOOL_PREFIX))
                })
                .count() as u64;
            usage.survey_queries = usage.survey_queries.saturating_add(survey_calls);
        }

        // Deduplicate sidechain copies of usage fields by message ID.
        if let Some(id) = msg.id {
            if !seen.insert(id) {
                continue;
            }
        }

        let Some(u) = msg.usage else { continue };

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
        total.survey_queries = total.survey_queries.saturating_add(s.survey_queries);
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

    // ── survey query counting (D55 / §9.B.8) ─────────────────────────────

    #[test]
    fn parse_session_counts_survey_tool_use_calls() {
        // Message with two survey tool-use blocks and token usage.
        let f = write_jsonl(&[
            r#"{"message":{"id":"msg_1","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"tool_use","name":"mcp__derrick-survey__derrick_survey_search","id":"t1","input":{}},{"type":"tool_use","name":"mcp__derrick-survey__derrick_survey_context","id":"t2","input":{}}]}}"#,
            r#"{"message":{"id":"msg_2","usage":{"input_tokens":20,"output_tokens":10,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[]}}"#,
        ]);
        let u = parse_session(f.path());
        assert_eq!(u.survey_queries, 2);
        assert_eq!(u.input_tokens, 120);
    }

    #[test]
    fn parse_session_ignores_non_survey_tool_use() {
        let f = write_jsonl(&[
            r#"{"message":{"id":"msg_1","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"tool_use","name":"Bash","id":"t1","input":{}},{"type":"tool_use","name":"Read","id":"t2","input":{}}]}}"#,
        ]);
        let u = parse_session(f.path());
        assert_eq!(u.survey_queries, 0);
    }

    #[test]
    fn parse_session_deduplicates_survey_calls_by_message_id() {
        // Sidechain duplicate of msg_1 — survey calls must not be double-counted.
        let f = write_jsonl(&[
            r#"{"message":{"id":"msg_1","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"tool_use","name":"mcp__derrick-survey__derrick_survey_search","id":"t1","input":{}}]}}"#,
            r#"{"message":{"id":"msg_1","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"tool_use","name":"mcp__derrick-survey__derrick_survey_search","id":"t1","input":{}}]}}"#,
        ]);
        let u = parse_session(f.path());
        assert_eq!(u.survey_queries, 1);
        assert_eq!(u.input_tokens, 10);
    }

    #[test]
    fn survey_tokens_saved_uses_per_query_constant() {
        let u = TokenUsage {
            survey_queries: 3,
            ..Default::default()
        };
        assert_eq!(u.survey_tokens_saved(), 3 * SURVEY_TOKENS_SAVED_PER_QUERY);
    }

    #[test]
    fn aggregate_sums_survey_queries_across_sessions() {
        use std::io::Write;
        let mut f1 = tempfile::NamedTempFile::new().unwrap();
        writeln!(f1, r#"{{"message":{{"id":"m1","usage":{{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}},"content":[{{"type":"tool_use","name":"mcp__derrick-survey__derrick_survey_search","id":"t1","input":{{}}}}]}}}}"#).unwrap();
        let mut f2 = tempfile::NamedTempFile::new().unwrap();
        writeln!(f2, r#"{{"message":{{"id":"m2","usage":{{"input_tokens":20,"output_tokens":8,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}},"content":[{{"type":"tool_use","name":"mcp__derrick-survey__derrick_survey_impact","id":"t2","input":{{}}}}]}}}}"#).unwrap();
        let sessions = vec![f1.path().to_path_buf(), f2.path().to_path_buf()];
        let total = aggregate(&sessions);
        assert_eq!(total.survey_queries, 2);
        assert_eq!(total.input_tokens, 30);
    }
}
