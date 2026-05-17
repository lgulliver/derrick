//! Scrub CLI noise from subprocess output before it crosses a model boundary.
//!
//! The scrubber is byte-preserving by default: unknown tools and malformed
//! UTF-8 lines pass through unchanged.

use regex::{Captures, Regex};
use std::cmp::min;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::Arc;

pub mod rules;

/// A scrubber instance configured with a registry of per-tool rule sets.
#[derive(Clone, Default)]
pub struct Scrubber {
    rules: Arc<HashMap<String, RuleSet>>,
}

impl Scrubber {
    /// Construct with the default rule set.
    pub fn with_defaults() -> Self {
        let mut scrubber = Self::empty();
        scrubber.register("gt", rules::gt::rules());
        scrubber.register("bd", rules::bd::rules());
        scrubber.register("git", rules::git::rules());
        scrubber.register("gh", rules::gh::rules());
        scrubber.register("claude", rules::claude::rules());
        scrubber.register("codex", rules::codex::rules());
        scrubber.register("copilot", rules::copilot::rules());
        scrubber.register("cargo", rules::cargo::rules());
        scrubber
    }

    /// Construct without any rules.
    pub fn empty() -> Self {
        Self {
            rules: Arc::new(HashMap::new()),
        }
    }

    /// Register or replace rules for a tool.
    pub fn register(&mut self, tool: &str, rules: RuleSet) {
        Arc::make_mut(&mut self.rules).insert(tool.to_owned(), rules);
    }

    /// Scrub bytes claimed to originate from `tool`.
    pub fn scrub(&self, tool: &str, input: &[u8]) -> (Vec<u8>, ScrubStats) {
        scrub_with_rules(self.rules.get(tool), input)
    }

    /// Build a streaming scrub adapter.
    pub fn scrub_stream<R: Read>(&self, tool: &str, reader: R) -> ScrubReader<R> {
        ScrubReader {
            reader,
            scrubber: self.clone(),
            tool: tool.to_owned(),
            output: Vec::new(),
            offset: 0,
            stats: ScrubStats::default(),
            loaded: false,
        }
    }
}

/// Streaming-scrub adapter.
pub struct ScrubReader<R> {
    reader: R,
    scrubber: Scrubber,
    tool: String,
    output: Vec<u8>,
    offset: usize,
    stats: ScrubStats,
    loaded: bool,
}

impl<R: Read> Read for ScrubReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if !self.loaded {
            let mut input = Vec::new();
            self.reader.read_to_end(&mut input)?;
            let (output, stats) = self.scrubber.scrub(&self.tool, &input);
            self.output = output;
            self.stats = stats;
            self.loaded = true;
        }

        if self.offset >= self.output.len() {
            return Ok(0);
        }

        let len = min(buf.len(), self.output.len() - self.offset);
        buf[..len].copy_from_slice(&self.output[self.offset..self.offset + len]);
        self.offset += len;
        Ok(len)
    }
}

impl<R> ScrubReader<R> {
    /// Consume the reader and return its stats.
    pub fn into_stats(mut self) -> ScrubStats {
        if !self.loaded {
            self.stats.eof = false;
        }
        self.stats
    }

    /// Return a stats snapshot without consuming the reader.
    pub fn snapshot_stats(&self) -> ScrubStats {
        if self.loaded {
            self.stats.clone()
        } else {
            let mut stats = self.stats.clone();
            stats.eof = false;
            stats
        }
    }
}

/// A set of rules for one tool.
#[derive(Clone, Default)]
pub struct RuleSet {
    rules: Vec<Rule>,
}

impl RuleSet {
    /// Construct an empty rule set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a rule to the end of the set.
    pub fn add(&mut self, rule: Rule) {
        self.rules.push(rule);
    }

    /// Extend the set with multiple rules.
    pub fn extend<I: IntoIterator<Item = Rule>>(&mut self, rules: I) {
        self.rules.extend(rules);
    }

    /// Return the number of rules.
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// Return whether the set contains no rules.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

/// One scrub rule: a regex pattern over lines and an action.
#[derive(Clone)]
pub struct Rule {
    /// Regex pattern matched against one line body, excluding its terminator.
    pub pattern: Regex,
    /// Action applied when the pattern matches.
    pub action: Action,
    /// Human-readable name for telemetry.
    pub name: &'static str,
}

/// A replacement template string.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Replacement(pub String);

/// A rule action.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Action {
    /// Drop the entire line.
    Drop,
    /// Replace the matched portion with the rendered replacement.
    Replace(Replacement),
    /// Keep the first matching line in a consecutive run.
    KeepFirstDropRest {
        /// Optional key extractor. `None` falls back to raw line equality.
        key: Option<Replacement>,
    },
    /// Collapse a consecutive matching run into one rendered line.
    Collapse {
        /// Output template for the collapsed run.
        render: Replacement,
        /// Optional grouping key.
        key: Option<Replacement>,
    },
}

/// Per-call scrub statistics.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ScrubStats {
    /// Number of bytes read.
    pub bytes_in: u64,
    /// Number of bytes emitted.
    pub bytes_out: u64,
    /// Number of input lines, including a trailing unterminated line.
    pub lines_in: u64,
    /// Number of emitted logical lines.
    pub lines_out: u64,
    /// Count of rule applications by rule name.
    pub rules_fired: HashMap<String, u64>,
    /// Whether all input has reached EOF and buffered state has been emitted.
    pub eof: bool,
}

impl ScrubStats {
    /// Return byte savings as a percentage of input bytes.
    pub fn savings_pct(&self) -> f64 {
        if self.bytes_in == 0 || self.bytes_out >= self.bytes_in {
            0.0
        } else {
            let saved = self.bytes_in - self.bytes_out;
            (saved as f64 / self.bytes_in as f64) * 100.0
        }
    }
}

/// Scrub from `input` into `output`, returning final stats.
pub fn scrub_io(
    scrubber: &Scrubber,
    tool: &str,
    input: &mut dyn Read,
    output: &mut dyn Write,
) -> std::io::Result<ScrubStats> {
    let mut reader = scrubber.scrub_stream(tool, input);
    let mut buf = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buf)?;
        if read == 0 {
            break;
        }
        output.write_all(&buf[..read])?;
    }
    Ok(reader.into_stats())
}

fn scrub_with_rules(rules: Option<&RuleSet>, input: &[u8]) -> (Vec<u8>, ScrubStats) {
    let mut engine = Engine::new(rules);
    for line in split_lines(input) {
        engine.process_line(&line);
    }
    engine.finish(input.len())
}

#[derive(Clone, Debug)]
struct Line<'a> {
    body: &'a [u8],
    terminator: &'a [u8],
}

fn split_lines(input: &[u8]) -> Vec<Line<'_>> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (idx, byte) in input.iter().enumerate() {
        if *byte == b'\n' {
            let (body_end, term_start) = if idx > start && input[idx - 1] == b'\r' {
                (idx - 1, idx - 1)
            } else {
                (idx, idx)
            };
            lines.push(Line {
                body: &input[start..body_end],
                terminator: &input[term_start..=idx],
            });
            start = idx + 1;
        }
    }
    if start < input.len() {
        lines.push(Line {
            body: &input[start..],
            terminator: &[],
        });
    }
    lines
}

struct Engine<'a> {
    rules: Option<&'a RuleSet>,
    output: Vec<u8>,
    stats: ScrubStats,
    keep_state: Option<(String, String)>,
    collapse: Option<PendingCollapse>,
}

impl<'a> Engine<'a> {
    fn new(rules: Option<&'a RuleSet>) -> Self {
        Self {
            rules,
            output: Vec::new(),
            stats: ScrubStats::default(),
            keep_state: None,
            collapse: None,
        }
    }

    fn process_line(&mut self, line: &Line<'_>) {
        self.stats.lines_in += 1;
        if self.rules.map_or(true, RuleSet::is_empty) {
            self.emit_raw(line);
            return;
        }

        let Ok(mut text) = std::str::from_utf8(line.body).map(str::to_owned) else {
            self.flush_collapse();
            self.keep_state = None;
            self.emit_raw(line);
            return;
        };

        let rules = match self.rules {
            Some(rules) => rules,
            None => {
                self.emit_raw(line);
                return;
            }
        };

        for rule in &rules.rules {
            if let Some(captures) = rule.pattern.captures(&text) {
                match &rule.action {
                    Action::Replace(replacement) => {
                        self.flush_collapse();
                        self.keep_state = None;
                        let replaced = rule.pattern.replace_all(&text, |caps: &Captures<'_>| {
                            render_replacement(replacement, caps, None)
                        });
                        text = replaced.into_owned();
                        self.fire(rule.name);
                    }
                    Action::Drop => {
                        self.flush_collapse();
                        self.keep_state = None;
                        self.fire(rule.name);
                        return;
                    }
                    Action::KeepFirstDropRest { key } => {
                        self.flush_collapse();
                        let key = key_for(key.as_ref(), &captures, &text);
                        let state = (rule.name.to_owned(), key.clone());
                        let drop_line = self.keep_state.as_ref() == Some(&state);
                        self.keep_state = Some(state);
                        self.fire(rule.name);
                        if drop_line {
                            return;
                        }
                        self.emit_text(&text, line.terminator);
                        return;
                    }
                    Action::Collapse { render, key } => {
                        self.keep_state = None;
                        let key = key.as_ref().map_or_else(String::new, |replacement| {
                            render_replacement(replacement, &captures, None)
                        });
                        if !self.extend_collapse(rule.name, &key, line.terminator) {
                            self.flush_collapse();
                            self.start_collapse(rule.name, key, render, &captures, line.terminator);
                        }
                        self.fire(rule.name);
                        return;
                    }
                }
            }
        }

        self.flush_collapse();
        self.keep_state = None;
        self.emit_text(&text, line.terminator);
    }

    fn emit_raw(&mut self, line: &Line<'_>) {
        self.output.extend_from_slice(line.body);
        self.output.extend_from_slice(line.terminator);
        self.stats.lines_out += 1;
    }

    fn emit_text(&mut self, text: &str, terminator: &[u8]) {
        self.output.extend_from_slice(text.as_bytes());
        self.output.extend_from_slice(terminator);
        self.stats.lines_out += 1;
    }

    fn start_collapse(
        &mut self,
        rule_name: &str,
        key: String,
        render: &Replacement,
        captures: &Captures<'_>,
        terminator: &[u8],
    ) {
        self.collapse = Some(PendingCollapse {
            rule_name: rule_name.to_owned(),
            key,
            render: render.clone(),
            captures: CaptureValues::from(captures),
            count: 1,
            terminator: terminator.to_vec(),
        });
    }

    fn extend_collapse(&mut self, rule_name: &str, key: &str, terminator: &[u8]) -> bool {
        if let Some(pending) = &mut self.collapse {
            if pending.rule_name == rule_name && pending.key == key {
                pending.count += 1;
                pending.terminator.clear();
                pending.terminator.extend_from_slice(terminator);
                return true;
            }
        }
        false
    }

    fn flush_collapse(&mut self) {
        if let Some(pending) = self.collapse.take() {
            let text = pending
                .render
                .render_from_values(&pending.captures, Some(pending.count));
            self.output.extend_from_slice(text.as_bytes());
            self.output.extend_from_slice(&pending.terminator);
            self.stats.lines_out += 1;
        }
    }

    fn fire(&mut self, name: &str) {
        *self.stats.rules_fired.entry(name.to_owned()).or_insert(0) += 1;
    }

    fn finish(mut self, bytes_in: usize) -> (Vec<u8>, ScrubStats) {
        self.flush_collapse();
        self.stats.bytes_in = bytes_in as u64;
        self.stats.bytes_out = self.output.len() as u64;
        self.stats.eof = true;
        (self.output, self.stats)
    }
}

struct PendingCollapse {
    rule_name: String,
    key: String,
    render: Replacement,
    captures: CaptureValues,
    count: usize,
    terminator: Vec<u8>,
}

#[derive(Debug)]
struct CaptureValues {
    values: Vec<Option<String>>,
}

impl CaptureValues {
    fn get(&self, index: usize) -> Option<&str> {
        self.values
            .get(index)
            .and_then(std::option::Option::as_deref)
    }
}

impl From<&Captures<'_>> for CaptureValues {
    fn from(captures: &Captures<'_>) -> Self {
        Self {
            values: captures
                .iter()
                .map(|value| value.map(|matched| matched.as_str().to_owned()))
                .collect(),
        }
    }
}

impl Replacement {
    fn render_from_values(&self, captures: &CaptureValues, count: Option<usize>) -> String {
        render_template(&self.0, |index| captures.get(index), count)
    }
}

fn key_for(replacement: Option<&Replacement>, captures: &Captures<'_>, line: &str) -> String {
    replacement.map_or_else(
        || line.to_owned(),
        |replacement| render_replacement(replacement, captures, None),
    )
}

fn render_replacement(
    replacement: &Replacement,
    captures: &Captures<'_>,
    count: Option<usize>,
) -> String {
    render_template(
        &replacement.0,
        |index| captures.get(index).map(|matched| matched.as_str()),
        count,
    )
}

fn render_template<'a>(
    template: &str,
    capture: impl Fn(usize) -> Option<&'a str>,
    count: Option<usize>,
) -> String {
    let mut output = String::new();
    let chars: Vec<char> = template.chars().collect();
    let mut idx = 0;
    while idx < chars.len() {
        if chars[idx] != '$' {
            output.push(chars[idx]);
            idx += 1;
            continue;
        }

        if idx + 1 >= chars.len() {
            output.push('$');
            idx += 1;
            continue;
        }

        let next = chars[idx + 1];
        if next == '$' {
            output.push('$');
            idx += 2;
        } else if next.is_ascii_digit() {
            let mut end = idx + 1;
            while end < chars.len() && chars[end].is_ascii_digit() {
                end += 1;
            }
            let number = chars[idx + 1..end].iter().collect::<String>();
            if let Ok(index) = number.parse::<usize>() {
                if let Some(value) = capture(index) {
                    output.push_str(value);
                }
            }
            idx = end;
        } else if chars[idx + 1..].starts_with(&['c', 'o', 'u', 'n', 't']) {
            if let Some(count) = count {
                output.push_str(&count.to_string());
            } else {
                output.push_str("$count");
            }
            idx += 6;
        } else {
            output.push('$');
            idx += 1;
        }
    }
    output
}

fn add_regex_rule(rules: &mut RuleSet, name: &'static str, pattern: &str, action: Action) {
    if let Ok(pattern) = Regex::new(pattern) {
        rules.add(Rule {
            pattern,
            action,
            name,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn single_rule(pattern: &str, action: Action, name: &'static str) -> RuleSet {
        let mut rules = RuleSet::new();
        add_regex_rule(&mut rules, name, pattern, action);
        rules
    }

    #[test]
    fn unknown_tool_passes_through_unchanged() {
        let scrubber = Scrubber::with_defaults();
        let input = b"alpha\nbeta\r\n";
        let (output, stats) = scrubber.scrub("unknown", input);
        assert_eq!(output, input);
        assert_eq!(stats.lines_in, 2);
        assert_eq!(stats.lines_out, 2);
        assert!(stats.eof);
    }

    #[test]
    fn empty_input_produces_empty_output() {
        let scrubber = Scrubber::with_defaults();
        let (output, stats) = scrubber.scrub("git", b"");
        assert!(output.is_empty());
        assert_eq!(stats.bytes_in, 0);
        assert_eq!(stats.bytes_out, 0);
        assert_eq!(stats.lines_in, 0);
        assert_eq!(stats.lines_out, 0);
        assert!(stats.eof);
    }

    #[test]
    fn non_utf8_passes_through_without_panic() {
        let scrubber = Scrubber::with_defaults();
        let input = b"ok\nbad \xFF line\n";
        let (output, stats) = scrubber.scrub("git", input);
        assert_eq!(output, input);
        assert_eq!(stats.lines_in, 2);
    }

    #[test]
    fn stats_savings_pct_rounds_correctly() {
        let stats = ScrubStats {
            bytes_in: 10,
            bytes_out: 7,
            ..ScrubStats::default()
        };
        assert!((stats.savings_pct() - 30.0).abs() < f64::EPSILON);
        assert_eq!(ScrubStats::default().savings_pct(), 0.0);
    }

    #[test]
    fn stream_handles_crlf_newlines_unchanged() -> std::io::Result<()> {
        let scrubber = Scrubber::with_defaults();
        let input = b"keep\r\nremote: Counting objects: 1\r\n";
        let mut reader = scrubber.scrub_stream("git", &input[..]);
        let mut output = Vec::new();
        reader.read_to_end(&mut output)?;
        assert_eq!(output, b"keep\r\n");
        assert!(reader.snapshot_stats().eof);
        Ok(())
    }

    #[test]
    fn stream_handles_missing_trailing_newline() -> std::io::Result<()> {
        let scrubber = Scrubber::with_defaults();
        let input = b"remote: Counting objects: 1\nfinal";
        let mut reader = scrubber.scrub_stream("git", &input[..]);
        let mut output = Vec::new();
        reader.read_to_end(&mut output)?;
        assert_eq!(output, b"final");
        Ok(())
    }

    #[test]
    fn stream_handles_malformed_utf8_across_chunk_boundary() -> std::io::Result<()> {
        let scrubber = Scrubber::with_defaults();
        let input = b"remote: Counting objects: 1\nbad \xE2\x82 line\n";
        let mut reader = scrubber.scrub_stream("git", &input[..]);
        let mut output = Vec::new();
        reader.read_to_end(&mut output)?;
        assert_eq!(output, b"bad \xE2\x82 line\n");
        Ok(())
    }

    #[test]
    fn register_replaces_existing_rules_for_same_tool() {
        let mut scrubber = Scrubber::empty();
        scrubber.register("tool", single_rule("^drop$", Action::Drop, "drop"));
        scrubber.register("tool", RuleSet::new());
        let (output, _) = scrubber.scrub("tool", b"drop\n");
        assert_eq!(output, b"drop\n");
    }

    #[test]
    fn keep_first_drop_rest_resets_on_non_match() {
        let mut scrubber = Scrubber::empty();
        scrubber.register(
            "tool",
            single_rule(
                "^same$",
                Action::KeepFirstDropRest { key: None },
                "keep same",
            ),
        );
        let (output, _) = scrubber.scrub("tool", b"same\nsame\nother\nsame\n");
        assert_eq!(output, b"same\nother\nsame\n");
    }

    #[test]
    fn collapse_count_capture_renders_correctly() {
        let mut scrubber = Scrubber::empty();
        scrubber.register(
            "tool",
            single_rule(
                "^warn: (.+)$",
                Action::Collapse {
                    render: Replacement("warn: $1 ($count times)".to_owned()),
                    key: Some(Replacement("$1".to_owned())),
                },
                "collapse warn",
            ),
        );
        let (output, _) = scrubber.scrub("tool", b"warn: retry\nwarn: retry\n");
        assert_eq!(output, b"warn: retry (2 times)\n");
    }

    #[test]
    fn replace_rules_do_not_short_circuit() {
        let mut rules = RuleSet::new();
        add_regex_rule(
            &mut rules,
            "strip brackets",
            "\\[(.+)\\]",
            Action::Replace(Replacement("$1".to_owned())),
        );
        add_regex_rule(
            &mut rules,
            "rename",
            "hello",
            Action::Replace(Replacement("hi".to_owned())),
        );
        let mut scrubber = Scrubber::empty();
        scrubber.register("tool", rules);
        let (output, _) = scrubber.scrub("tool", b"[hello]\n");
        assert_eq!(output, b"hi\n");
    }

    #[test]
    fn scrub_io_writes_output_and_returns_stats() -> std::io::Result<()> {
        let scrubber = Scrubber::with_defaults();
        let mut input: &[u8] = b"remote: Counting objects: 1\nok\n";
        let mut output = Vec::new();
        let stats = scrub_io(&scrubber, "git", &mut input, &mut output)?;
        assert_eq!(output, b"ok\n");
        assert_eq!(stats.bytes_out, 3);
        Ok(())
    }

    #[test]
    fn default_rule_sets_have_expected_counts() {
        let scrubber = Scrubber::with_defaults();
        for tool in [
            "gt", "bd", "git", "gh", "claude", "codex", "copilot", "cargo",
        ] {
            let rules = scrubber.rules.get(tool);
            assert!(rules.is_some_and(|rules| rules.len() >= 3));
        }
    }
}
