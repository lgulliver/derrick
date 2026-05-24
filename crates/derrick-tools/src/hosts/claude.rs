use std::ffi::OsString;
use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;

use crate::process::{is_available, run_host, CommandSpec};
use crate::{HostAdapter, HostError, HostRequest, HostResponse};

const NAME: &str = "claude";

/// Host adapter for the Claude Code CLI.
#[derive(Clone, Debug)]
pub struct ClaudeHost {
    binary: PathBuf,
}

impl ClaudeHost {
    /// Creates an adapter that resolves `claude` on `PATH`.
    pub fn new() -> Self {
        Self {
            binary: PathBuf::from(NAME),
        }
    }

    /// Creates an adapter using an explicit binary path.
    pub fn with_binary(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
        }
    }
}

impl Default for ClaudeHost {
    fn default() -> Self {
        Self::new()
    }
}

/// Subset of the JSON object emitted by `claude --print --output-format json`.
///
/// Claude Code CLI (≥1.x) writes a single JSON object to stdout. We only
/// parse the fields we need; everything else is ignored.
#[derive(Deserialize)]
struct ClaudeJsonResult {
    /// The assistant's text response.
    #[serde(default)]
    result: String,
    /// Token usage reported by the API.
    #[serde(default)]
    usage: ClaudeUsage,
}

#[derive(Default, Deserialize)]
struct ClaudeUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
}

/// Parse `claude --output-format json` output.
///
/// Returns `(text, tokens_in, tokens_out)`. Falls back to
/// `(raw_stdout, 0, 0)` if the JSON is missing or malformed, so older
/// versions of the CLI that don't support JSON output keep working.
fn parse_claude_json(raw: &str) -> (String, u32, u32) {
    match serde_json::from_str::<ClaudeJsonResult>(raw.trim()) {
        Ok(parsed) => (
            parsed.result,
            parsed.usage.input_tokens,
            parsed.usage.output_tokens,
        ),
        Err(_) => (raw.to_owned(), 0, 0),
    }
}

#[async_trait]
impl HostAdapter for ClaudeHost {
    fn name(&self) -> &str {
        NAME
    }

    fn is_available(&self) -> bool {
        is_available(&self.binary)
    }

    async fn run(&self, request: HostRequest) -> Result<HostResponse, HostError> {
        let mut args = vec![
            OsString::from("--print"),
            OsString::from("--output-format"),
            OsString::from("json"),
        ];
        if let Some(ref model) = request.model {
            args.push(OsString::from("--model"));
            args.push(OsString::from(model.as_str()));
        }
        if request.headless {
            // Suppress interactive permission prompts when running without a
            // terminal. Pipeline steps always set `HostRequest::headless = true`.
            args.push(OsString::from("--dangerously-skip-permissions"));
        }
        args.push(OsString::from(&request.prompt));
        let spec = CommandSpec {
            binary: self.binary.clone(),
            args,
        };
        let mut response = run_host(NAME, spec, request).await?;
        // Parse JSON to extract the text and token counts; fall back
        // gracefully for CLI versions that don't support --output-format json.
        let (text, tokens_in, tokens_out) = parse_claude_json(&response.stdout);
        response.stdout = text;
        response.tokens_in = tokens_in;
        response.tokens_out = tokens_out;
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::parse_claude_json;

    #[test]
    fn parse_claude_json_extracts_result_and_tokens() {
        let raw = r#"{
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": "Hello, world!",
            "session_id": "abc",
            "num_turns": 1,
            "total_cost_usd": 0.001,
            "usage": {
                "input_tokens": 42,
                "output_tokens": 7,
                "cache_read_input_tokens": 0,
                "cache_creation_input_tokens": 0
            }
        }"#;
        let (text, tokens_in, tokens_out) = parse_claude_json(raw);
        assert_eq!(text, "Hello, world!");
        assert_eq!(tokens_in, 42);
        assert_eq!(tokens_out, 7);
    }

    #[test]
    fn parse_claude_json_falls_back_on_plain_text() {
        let raw = "This is a plain text response";
        let (text, tokens_in, tokens_out) = parse_claude_json(raw);
        assert_eq!(text, raw);
        assert_eq!(tokens_in, 0);
        assert_eq!(tokens_out, 0);
    }

    #[test]
    fn parse_claude_json_handles_missing_usage() {
        let raw = r#"{"result": "hi", "type": "result"}"#;
        let (text, tokens_in, tokens_out) = parse_claude_json(raw);
        assert_eq!(text, "hi");
        assert_eq!(tokens_in, 0);
        assert_eq!(tokens_out, 0);
    }
}
