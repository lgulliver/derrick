//! Caveman text compressor.
//!
//! Pure-Rust shaping rules mirror the installed caveman skill's
//! `lite`, `full`, and `ultra` intensities while preserving technical
//! spans byte-for-byte.

use std::mem;
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

const STREAM_FLUSH_TARGET: usize = 8 * 1024;
const STREAM_TAIL_GUARD: usize = 256;
const MAX_PROTECTED_BUFFER: usize = 1024 * 1024;

/// How aggressively to compress.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Intensity {
    /// Light compression: drop filler and hedging while keeping
    /// articles and full sentence structure.
    #[default]
    Lite,
    /// Full compression: drop articles, flatten prose, and prefer
    /// short words.
    Full,
    /// Maximum compression: strip safe and causal conjunctions
    /// (`because`/`therefore`) without inserting an arrow (D90; the
    /// installed skill forbids `->`). Standard well-known acronyms
    /// (e.g. `DB`) still apply, but invented prose abbreviations like
    /// `req`/`res`/`fn`/`impl`/`auth` do not (D93; the installed skill
    /// bans them as zero-token-saving and clarity-costing).
    Ultra,
}

/// Result from a compression pass.
#[derive(Clone, Debug, Default)]
pub struct CompressOutput {
    /// Compressed text.
    pub text: String,
    /// Compression counters.
    pub stats: CompressStats,
}

/// Compression counters.
#[derive(Clone, Debug, Default)]
pub struct CompressStats {
    /// Input character count.
    pub chars_in: u64,
    /// Output character count.
    pub chars_out: u64,
    /// Input prose word count.
    pub words_in: u64,
    /// Output prose word count.
    pub words_out: u64,
    /// Non-empty paragraphs processed.
    pub paragraphs_processed: u64,
    /// Protected spans preserved unchanged.
    pub preserved_spans: u64,
}

impl CompressStats {
    /// Percentage of characters removed, clamped at zero when output
    /// is longer than input.
    pub fn savings_pct(&self) -> f64 {
        if self.chars_in == 0 {
            return 0.0;
        }

        let saved = self.chars_in.saturating_sub(self.chars_out);
        let saved_f64 = u32::try_from(saved).map_or(f64::from(u32::MAX), f64::from);
        let in_f64 = u32::try_from(self.chars_in).map_or(f64::from(u32::MAX), f64::from);
        (saved_f64 / in_f64) * 100.0
    }

    fn add(&mut self, other: &Self) {
        self.chars_in = self.chars_in.saturating_add(other.chars_in);
        self.chars_out = self.chars_out.saturating_add(other.chars_out);
        self.words_in = self.words_in.saturating_add(other.words_in);
        self.words_out = self.words_out.saturating_add(other.words_out);
        self.paragraphs_processed = self
            .paragraphs_processed
            .saturating_add(other.paragraphs_processed);
        self.preserved_spans = self.preserved_spans.saturating_add(other.preserved_spans);
    }
}

/// Streaming compressor for large inputs.
///
/// Lexer state survives across `write_str` calls by keeping
/// incomplete protected spans buffered until they close or reach the
/// 1 MiB protected-span cap.
#[derive(Debug)]
pub struct Compressor {
    intensity: Intensity,
    pending: String,
    stats: CompressStats,
}

impl Compressor {
    /// Create a streaming compressor.
    pub fn new(intensity: Intensity) -> Self {
        Self {
            intensity,
            pending: String::new(),
            stats: CompressStats::default(),
        }
    }

    /// Feed input and return any compressed regions that are safe to
    /// emit.
    pub fn write_str(&mut self, input: &str) -> Vec<String> {
        self.pending.push_str(input);

        let Some(safe_len) = safe_flush_len(&self.pending) else {
            if self.pending.len() > MAX_PROTECTED_BUFFER {
                let text = mem::take(&mut self.pending);
                let stats = stats_for_preserved(&text);
                self.stats.add(&stats);
                return vec![text];
            }
            return Vec::new();
        };

        if safe_len == 0 || self.pending.len() < STREAM_FLUSH_TARGET {
            return Vec::new();
        }

        let remainder = self.pending.split_off(safe_len);
        let chunk = mem::replace(&mut self.pending, remainder);
        let output = compress(&chunk, self.intensity);
        self.stats.add(&output.stats);
        vec![output.text]
    }

    /// Drain remaining buffered text. An open protected region is
    /// treated as closed at EOF.
    pub fn finish(mut self) -> CompressOutput {
        let output = compress(&self.pending, self.intensity);
        self.stats.add(&output.stats);
        CompressOutput {
            text: output.text,
            stats: self.stats,
        }
    }
}

/// Compress text at the given intensity.
pub fn compress(input: &str, intensity: Intensity) -> CompressOutput {
    if should_skip_for_clarity(input) {
        return CompressOutput {
            text: input.to_owned(),
            stats: stats_for_plain(input, input, 0),
        };
    }

    let mut lexer = Lexer::new(input);
    let mut text = String::new();
    let mut stats = CompressStats {
        chars_in: usize_to_u64(input.chars().count()),
        words_in: usize_to_u64(word_count(input)),
        paragraphs_processed: usize_to_u64(paragraph_count(input)),
        ..CompressStats::default()
    };

    while let Some(span) = lexer.next_span() {
        match span.kind {
            SpanKind::Protected => {
                stats.preserved_spans = stats.preserved_spans.saturating_add(1);
                text.push_str(span.text);
            }
            SpanKind::Prose => text.push_str(&compress_prose(span.text, intensity)),
        }
    }

    stats.chars_out = usize_to_u64(text.chars().count());
    stats.words_out = usize_to_u64(word_count(&text));

    CompressOutput { text, stats }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SpanKind {
    Prose,
    Protected,
}

#[derive(Clone, Copy, Debug)]
struct Span<'a> {
    kind: SpanKind,
    text: &'a str,
}

#[derive(Debug)]
struct Lexer<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> Lexer<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, pos: 0 }
    }

    fn next_span(&mut self) -> Option<Span<'a>> {
        if self.pos >= self.input.len() {
            return None;
        }

        if let Some(end) = protected_end_at(self.input, self.pos) {
            let start = self.pos;
            self.pos = end;
            return Some(Span {
                kind: SpanKind::Protected,
                text: &self.input[start..end],
            });
        }

        let start = self.pos;
        self.pos = next_char_boundary(self.input, self.pos);
        while self.pos < self.input.len() && protected_end_at(self.input, self.pos).is_none() {
            self.pos = next_char_boundary(self.input, self.pos);
        }
        Some(Span {
            kind: SpanKind::Prose,
            text: &self.input[start..self.pos],
        })
    }
}

fn protected_end_at(input: &str, pos: usize) -> Option<usize> {
    let rest = input.get(pos..)?;

    if rest.starts_with("```") {
        return Some(fenced_code_end(input, pos));
    }

    if rest.starts_with('`') {
        return Some(inline_code_end(input, pos));
    }

    if is_line_start(input, pos)
        && (rest.starts_with("-->") || rest.starts_with('|') || rest.starts_with("^|"))
    {
        return Some(diagnostic_end(input, pos));
    }

    if rest.starts_with("error[")
        || rest.starts_with("error:")
        || rest.starts_with("Error:")
        || (is_line_start(input, pos) && rest.starts_with("note:"))
        || (is_line_start(input, pos) && rest.starts_with("help:"))
        || (is_line_start(input, pos) && rest.starts_with("warning:"))
    {
        return Some(error_end(input, pos));
    }

    if rest.starts_with('[') {
        if let Some(end) = markdown_link_end(input, pos) {
            return Some(end);
        }
    }

    if rest.starts_with('<') {
        if let Some(end) = autolink_end(input, pos) {
            return Some(end);
        }
    }

    token_protected_end(input, pos)
}

fn fenced_code_end(input: &str, pos: usize) -> usize {
    let after_open = pos.saturating_add(3);
    input
        .get(after_open..)
        .and_then(|tail| tail.find("```").map(|offset| after_open + offset + 3))
        .unwrap_or(input.len())
}

fn inline_code_end(input: &str, pos: usize) -> usize {
    let after_open = pos.saturating_add(1);
    input
        .get(after_open..)
        .and_then(|tail| tail.find('`').map(|offset| after_open + offset + 1))
        .unwrap_or(input.len())
}

fn diagnostic_end(input: &str, pos: usize) -> usize {
    input
        .get(pos..)
        .and_then(|tail| tail.find("\n\n").map(|offset| pos + offset))
        .unwrap_or(input.len())
}

fn error_end(input: &str, pos: usize) -> usize {
    let rest = match input.get(pos..) {
        Some(rest) => rest,
        None => return input.len(),
    };

    // Detect whether this is the start of a compiler diagnostic block.
    //
    // Rustc format:  error[E0308]: ...
    //                  --> src/...
    //                   |
    //                nn | code
    //                   | ^^^^
    //                   = note: ...
    //                   = help: ...
    //
    // Generic runtime error (Node/Python):
    //   Error: Cannot find module './missing'
    //       at Function.Module._resolveFilename ...
    //
    // We only trigger full-block protection when the pattern is unambiguously
    // a structured diagnostic:
    //   1. `error[E...]` — rustc error with code (unambiguous)
    //   2. `error:` or `Error:` on a line whose NEXT line looks like a
    //      diagnostic continuation (starts with `  -->`, `    at `, etc.)
    //
    // Plain inline `error:` phrases like "error: failed to parse. The ..."
    // fall back to the original first-sentence heuristic so existing corpus
    // behaviour is preserved.

    if rest.starts_with("error[") {
        return diagnostic_block_end(input, pos);
    }

    if rest.starts_with("error:") || rest.starts_with("Error:") {
        // Peek at the next line to see if it looks like a diagnostic gutter.
        let next_line_is_diag = rest
            .find('\n')
            .and_then(|nl| rest.get(nl + 1..))
            .map(|next| {
                let t = next.trim_start();
                t.starts_with("-->")
                    || t.starts_with("at ")
                    || t.starts_with("File \"")
                    || t.starts_with("Traceback")
            })
            .unwrap_or(false);

        if next_line_is_diag {
            return diagnostic_block_end(input, pos);
        }
    }

    // Fall back to the original first-sentence / first-paragraph heuristic
    // for generic inline error phrases (e.g. "returns error: foo. More prose.").
    let paragraph = rest.find("\n\n").unwrap_or(rest.len());
    let sentence = rest
        .char_indices()
        .find(|(_, ch)| matches!(ch, '.' | '!' | '?'))
        .map(|(offset, ch)| offset + ch.len_utf8());
    pos + sentence
        .filter(|end| *end <= paragraph)
        .unwrap_or(paragraph)
}

/// Protect an entire compiler-diagnostic or stack-trace block starting at
/// `pos`. The block ends at the first line that is clearly non-diagnostic
/// after any blank separator lines.
///
/// Continuation patterns (lines that belong to the same diagnostic block):
/// - blank line (used as separator *within* a block — tolerated once)
/// - `  -->` / ` -->` — file/line pointer
/// - ` |` / `  |` — gutter line (any content after the bar)
/// - `  = note:` / `  = help:` — trailing annotations
/// - `  ...` — elision marker in rustc output
/// - `note:` / `help:` / `warning:` at line start — secondary messages
/// - Stack-trace: `    at ` / `   at ` / `at ` line prefix
/// - Python/Node: `  File "..."` / `Traceback`
fn diagnostic_block_end(input: &str, pos: usize) -> usize {
    let rest = match input.get(pos..) {
        Some(r) => r,
        None => return input.len(),
    };

    let mut end = pos;
    let mut blank_run = 0usize;

    for line in rest.lines() {
        let line_len = line.len();
        let trimmed = line.trim();

        if trimmed.is_empty() {
            blank_run += 1;
            // Allow up to one blank separator inside the block; two consecutive
            // blank lines always end the block.
            if blank_run >= 2 {
                break;
            }
            end += line_len + 1; // +1 for '\n'
            continue;
        }

        blank_run = 0;

        // Count leading spaces: lines indented ≥ 3 spaces inside a diagnostic
        // block are always continuations (rustc aligns multi-line note text,
        // stack traces indent with 4 spaces, etc.).
        let leading_spaces = line.len() - line.trim_start_matches(' ').len();

        let is_continuation = leading_spaces >= 3
            || trimmed.starts_with("-->")
            || trimmed.starts_with("| ")
            || trimmed == "|"
            || trimmed.starts_with("= note:")
            || trimmed.starts_with("= help:")
            || trimmed.starts_with("= warning:")
            || trimmed.starts_with("note:")
            || trimmed.starts_with("help:")
            || trimmed.starts_with("warning:")
            || trimmed.starts_with("error[")
            || trimmed.starts_with("error:")
            || trimmed.starts_with("Error:")
            || trimmed.starts_with("...")
            || trimmed.starts_with("at ")          // stack trace
            || trimmed.starts_with("File \"")      // Python traceback
            || trimmed.starts_with("Traceback")    // Python
            || trimmed.starts_with("caused by:")
            || trimmed.starts_with("^ ")           // some diagnostic pointers
            || is_gutter_line(trimmed); // "42 | ..." gutter

        if is_continuation {
            end += line_len + 1;
        } else {
            break;
        }
    }

    // Clamp to valid byte boundary.
    end.min(input.len())
}

/// True for rustc gutter lines of the form `  42 | ...` or `     | ...`.
fn is_gutter_line(trimmed: &str) -> bool {
    // Pattern: optional leading spaces + digits or spaces + " | "
    let s = trimmed;
    // Find the first ` | ` or end-of-line ` |`
    if let Some(bar) = s.find(" | ") {
        // Everything before the bar must be digits/spaces only.
        s[..bar].chars().all(|c| c.is_ascii_digit() || c == ' ')
    } else if let Some(stripped) = s.strip_suffix(" |") {
        stripped.chars().all(|c| c.is_ascii_digit() || c == ' ')
    } else {
        false
    }
}

fn markdown_link_end(input: &str, pos: usize) -> Option<usize> {
    let rest = input.get(pos..)?;
    let close_label = rest.find(']')?;
    let target_start = close_label.checked_add(1)?;
    if !rest.get(target_start..)?.starts_with('(') {
        return None;
    }
    let target = rest.get(target_start + 1..)?;
    let close_target = target.find(')')?;
    Some(pos + target_start + 1 + close_target + 1)
}

fn autolink_end(input: &str, pos: usize) -> Option<usize> {
    let rest = input.get(pos..)?;
    let close = rest.find('>')?;
    let inner = rest.get(1..close)?;
    if is_url(inner) {
        Some(pos + close + 1)
    } else {
        None
    }
}

fn token_protected_end(input: &str, pos: usize) -> Option<usize> {
    if !is_token_start(input, pos) {
        return None;
    }

    let end = token_end(input, pos);
    let token = input.get(pos..end)?;
    if is_protected_token(token) {
        Some(end)
    } else {
        None
    }
}

fn is_token_start(input: &str, pos: usize) -> bool {
    if pos > 0 {
        let prev = input
            .get(..pos)
            .and_then(|prefix| prefix.chars().next_back());
        if prev.is_some_and(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '-' | ':' | '/')) {
            return false;
        }
    }
    input
        .get(pos..)
        .and_then(|rest| rest.chars().next())
        .is_some_and(|ch| ch.is_alphanumeric() || matches!(ch, '.' | '/' | '-' | '_' | '<'))
}

fn token_end(input: &str, pos: usize) -> usize {
    let mut end = pos;
    let Some(rest) = input.get(pos..) else {
        return pos;
    };

    for (offset, ch) in rest.char_indices() {
        if ch.is_whitespace() || matches!(ch, ',' | ';' | ')' | ']' | '}') {
            break;
        }
        end = pos + offset + ch.len_utf8();
    }

    end
}

fn is_protected_token(token: &str) -> bool {
    is_url(token)
        || is_path(token)
        || is_file_line_ref(token)
        || is_cli_flag(token)
        || is_number_like(token)
        || is_ticket_id(token)
        || is_identifier(token)
}

fn is_url(token: &str) -> bool {
    url_regex().is_some_and(|regex| regex.is_match(token))
}

fn is_path(token: &str) -> bool {
    path_regex().is_some_and(|regex| regex.is_match(token))
}

fn is_file_line_ref(token: &str) -> bool {
    file_line_regex().is_some_and(|regex| regex.is_match(token))
}

fn is_cli_flag(token: &str) -> bool {
    cli_flag_regex().is_some_and(|regex| regex.is_match(token))
}

fn is_number_like(token: &str) -> bool {
    number_regex().is_some_and(|regex| regex.is_match(token))
}

fn is_ticket_id(token: &str) -> bool {
    ticket_regex().is_some_and(|regex| regex.is_match(token))
}

fn is_identifier(token: &str) -> bool {
    identifier_regex().is_some_and(|regex| regex.is_match(token))
}

fn compress_prose(input: &str, intensity: Intensity) -> String {
    if input.contains('\n') || input.contains('\r') {
        let mut out = String::new();
        let mut segment_start = 0;
        for (idx, ch) in input.char_indices() {
            if matches!(ch, '\n' | '\r') {
                if let Some(segment) = input.get(segment_start..idx) {
                    out.push_str(&compress_prose_flat(segment, intensity));
                }
                out.push(ch);
                segment_start = idx + ch.len_utf8();
            }
        }
        if let Some(segment) = input.get(segment_start..) {
            out.push_str(&compress_prose_flat(segment, intensity));
        }
        return out;
    }

    compress_prose_flat(input, intensity)
}

fn compress_prose_flat(input: &str, intensity: Intensity) -> String {
    let leading_space = input.starts_with(char::is_whitespace);
    let trailing_space = input.ends_with(char::is_whitespace);
    let normalised = collapse_horizontal_space(input);
    let without_phrases = remove_phrases(&normalised, intensity);
    let substituted = substitute_phrases(&without_phrases, intensity);
    let mut out = String::new();
    let mut pending_space = false;

    for token in prose_tokens(&substituted) {
        match token {
            ProseToken::Space => pending_space = true,
            ProseToken::Word(word) => {
                if should_drop_word(word, intensity) {
                    continue;
                }
                if pending_space && needs_space_before(&out) {
                    out.push(' ');
                }
                out.push_str(&rewrite_word(word, intensity));
                pending_space = false;
            }
            ProseToken::Punct(punct) => {
                trim_trailing_space(&mut out);
                out.push_str(punct);
                pending_space = false;
            }
            ProseToken::Other(other) => {
                if pending_space && needs_space_before(&out) {
                    out.push(' ');
                }
                out.push_str(other);
                pending_space = false;
            }
        }
    }

    if leading_space && !out.starts_with(char::is_whitespace) {
        out.insert(0, ' ');
    }
    if trailing_space && !out.ends_with(char::is_whitespace) {
        out.push(' ');
    }

    out
}

#[derive(Clone, Copy, Debug)]
enum ProseToken<'a> {
    Word(&'a str),
    Space,
    Punct(&'a str),
    Other(&'a str),
}

fn prose_tokens(input: &str) -> Vec<ProseToken<'_>> {
    let mut tokens = Vec::new();
    let mut pos = 0;

    while pos < input.len() {
        let next = next_char_boundary(input, pos);
        let ch = match input.get(pos..next) {
            Some(slice) => slice,
            None => break,
        };
        let first = match ch.chars().next() {
            Some(ch) => ch,
            None => break,
        };

        if first.is_whitespace() {
            let start = pos;
            pos = next;
            while pos < input.len() {
                let peek = next_char_boundary(input, pos);
                if input
                    .get(pos..peek)
                    .and_then(|slice| slice.chars().next())
                    .is_none_or(|c| !c.is_whitespace())
                {
                    break;
                }
                pos = peek;
            }
            let _ = start;
            tokens.push(ProseToken::Space);
        } else if first.is_ascii_alphabetic() || first == '\'' {
            let start = pos;
            pos = next;
            while pos < input.len() {
                let peek = next_char_boundary(input, pos);
                let cont = input
                    .get(pos..peek)
                    .and_then(|slice| slice.chars().next())
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '\'');
                if !cont {
                    break;
                }
                pos = peek;
            }
            if let Some(word) = input.get(start..pos) {
                tokens.push(ProseToken::Word(word));
            }
        } else if matches!(first, '.' | ',' | ':' | ';' | '!' | '?') {
            tokens.push(ProseToken::Punct(ch));
            pos = next;
        } else {
            tokens.push(ProseToken::Other(ch));
            pos = next;
        }
    }

    tokens
}

fn collapse_horizontal_space(input: &str) -> String {
    let mut out = String::new();
    let mut pending_space = false;

    for ch in input.chars() {
        if matches!(ch, ' ' | '\t') {
            pending_space = true;
        } else {
            if pending_space && !matches!(ch, '\n' | '\r') && !out.ends_with(['\n', '\r']) {
                out.push(' ');
            }
            out.push(ch);
            pending_space = false;
        }
    }

    out
}

fn remove_phrases(input: &str, _intensity: Intensity) -> String {
    phrase_regex().map_or_else(
        || input.to_owned(),
        |regex| {
            regex
                .replace_all(input, |caps: &regex::Captures<'_>| {
                    let matched = caps.get(0).map_or("", |m| m.as_str());
                    if matched.ends_with(' ') { " " } else { "" }
                })
                .into_owned()
        },
    )
}

fn substitute_phrases(input: &str, intensity: Intensity) -> String {
    let mut output = input.to_owned();
    if matches!(intensity, Intensity::Full | Intensity::Ultra) {
        if let Some(regex) = solution_phrase_regex() {
            output = regex.replace_all(&output, "fix").into_owned();
        }
        if let Some(regex) = extensive_regex() {
            output = regex.replace_all(&output, "big").into_owned();
        }
        if let Some(regex) = in_order_to_regex() {
            output = regex.replace_all(&output, "to ").into_owned();
        }
        if let Some(regex) = verbose_prefix_regex() {
            output = regex.replace_all(&output, "").into_owned();
        }
    }
    if intensity == Intensity::Ultra {
        if let Some(regex) = causal_regex() {
            // D90: the installed skill strips the causal conjunction and
            // forbids arrows outright ("NO arrows (X -> Y) -- measured
            // zero token saving under tokenizer, cost decode clarity").
            // Join the two clauses with a comma rather than the word —
            // mirrors the skill's own Ultra example, which joins clauses
            // with commas ("Inline obj prop, new ref, re-render.")
            // rather than inventing a connective.
            output = regex.replace_all(&output, ", ").into_owned();
        }
    }
    output
}

fn should_drop_word(word: &str, intensity: Intensity) -> bool {
    let lower = word.to_ascii_lowercase();
    let filler = matches!(
        lower.as_str(),
        "just"
            | "really"
            | "basically"
            | "actually"
            | "simply"
            | "maybe"
            | "probably"
            | "perhaps"
            | "likely"
            | "moreover"
            | "furthermore"
            | "additionally"
            | "essentially"
            | "obviously"
            | "clearly"
            | "indeed"
            | "typically"
            | "generally"
            | "necessarily"
            | "subsequently"
            | "consequently"
            | "accordingly"
            | "therefore"
            | "thus"
    );
    if filler {
        return true;
    }

    if matches!(intensity, Intensity::Full | Intensity::Ultra)
        && matches!(lower.as_str(), "a" | "an" | "the")
    {
        return true;
    }

    intensity == Intensity::Ultra && matches!(lower.as_str(), "and" | "or" | "but")
}

fn rewrite_word(word: &str, intensity: Intensity) -> String {
    if intensity == Intensity::Lite {
        return word.to_owned();
    }

    // D93: the installed skill bans *invented* prose abbreviations —
    // "never invent new abbreviations (cfg/impl/req/res/fn)" (Rules) and,
    // stronger still for Ultra, "NO prose abbreviations
    // (cfg/impl/req/res/fn/auth) ... measured zero token saving under
    // tokenizer, cost decode clarity" (Intensity table, Ultra row). This
    // match must never reintroduce `req`/`res`/`fn`/`impl`/`auth` (or any
    // other made-up truncation of the same kind) as a rewrite target —
    // those words pass through unabbreviated below. `DB` survives because
    // the same Rules line carves out an explicit exception: "Standard
    // well-known tech acronyms OK (DB/API/HTTP)". `config` survives for
    // the same reason the skill's banned list names `cfg` and not
    // `config`: `config` is the ordinary, undegraded word — it is not a
    // further invented truncation the way `cfg` is.
    match word.to_ascii_lowercase().as_str() {
        // Full and Ultra rewrites
        "however" => "but".to_owned(),
        "nevertheless" | "nonetheless" => "still".to_owned(),
        "additionally" => "also".to_owned(),
        // Ultra-only rewrites
        "database" | "databases" if intensity == Intensity::Ultra => "DB".to_owned(),
        "configuration" | "configure" if intensity == Intensity::Ultra => "config".to_owned(),
        _ => word.to_owned(),
    }
}

fn should_skip_for_clarity(input: &str) -> bool {
    let lower = input.to_ascii_lowercase();
    lower.contains("security warning")
        || lower.contains("cannot be undone")
        || lower.contains("permanently delete")
}

fn safe_flush_len(input: &str) -> Option<usize> {
    if has_open_protected_span(input) {
        return None;
    }

    if input.ends_with(char::is_whitespace) {
        return Some(input.len());
    }

    if input.len() <= STREAM_TAIL_GUARD {
        return Some(0);
    }

    let guard_start = input
        .char_indices()
        .rev()
        .find(|(idx, _)| input.len().saturating_sub(*idx) >= STREAM_TAIL_GUARD)
        .map_or(0, |(idx, _)| idx);

    input
        .get(..guard_start)
        .and_then(|prefix| prefix.rfind(char::is_whitespace))
        .map(|idx| idx + 1)
}

fn has_open_protected_span(input: &str) -> bool {
    let mut in_fence = false;
    let mut fence_count = 0_u64;
    let mut inline_count = 0_u64;

    let mut pos = 0;
    while let Some(offset) = input.get(pos..).and_then(|tail| tail.find('`')) {
        let tick = pos + offset;
        if input
            .get(tick..)
            .is_some_and(|tail| tail.starts_with("```"))
        {
            fence_count = fence_count.saturating_add(1);
            in_fence = !in_fence;
            pos = tick.saturating_add(3);
        } else if !in_fence {
            inline_count = inline_count.saturating_add(1);
            pos = tick.saturating_add(1);
        } else {
            pos = tick.saturating_add(1);
        }
    }

    fence_count % 2 == 1
        || inline_count % 2 == 1
        || has_unclosed_markdown_link(input)
        || has_unclosed_autolink(input)
        || has_open_error(input)
        || has_open_url_tail(input)
}

fn has_unclosed_markdown_link(input: &str) -> bool {
    let Some(open) = input.rfind('[') else {
        return false;
    };
    let tail = match input.get(open..) {
        Some(tail) => tail,
        None => return false,
    };
    tail.contains(']') && tail.contains('(') && !tail.contains(')')
}

fn has_unclosed_autolink(input: &str) -> bool {
    let Some(open) = input.rfind('<') else {
        return false;
    };
    let tail = match input.get(open..) {
        Some(tail) => tail,
        None => return false,
    };
    tail.starts_with("<http") && !tail.contains('>')
}

fn has_open_error(input: &str) -> bool {
    // Look for the last occurrence of an error/Error opener.
    let last_error = input
        .rfind("error[")
        .or_else(|| input.rfind("error:"))
        .or_else(|| input.rfind("Error:"));
    let Some(pos) = last_error else {
        return false;
    };
    let tail = match input.get(pos..) {
        Some(tail) => tail,
        None => return false,
    };
    // A compiler-diagnostic block is "open" (needs more input) if we haven't
    // yet seen a double-blank-line that would close it.
    !tail.contains("\n\n")
}

fn has_open_url_tail(input: &str) -> bool {
    let Some(pos) = input.rfind("http") else {
        return false;
    };
    let tail = match input.get(pos..) {
        Some(tail) => tail,
        None => return false,
    };
    (tail.starts_with("http://") || tail.starts_with("https://"))
        && !tail.ends_with(char::is_whitespace)
}

fn stats_for_preserved(input: &str) -> CompressStats {
    CompressStats {
        chars_in: usize_to_u64(input.chars().count()),
        chars_out: usize_to_u64(input.chars().count()),
        words_in: usize_to_u64(word_count(input)),
        words_out: usize_to_u64(word_count(input)),
        paragraphs_processed: usize_to_u64(paragraph_count(input)),
        preserved_spans: 1,
    }
}

fn stats_for_plain(input: &str, output: &str, preserved_spans: u64) -> CompressStats {
    CompressStats {
        chars_in: usize_to_u64(input.chars().count()),
        chars_out: usize_to_u64(output.chars().count()),
        words_in: usize_to_u64(word_count(input)),
        words_out: usize_to_u64(word_count(output)),
        paragraphs_processed: usize_to_u64(paragraph_count(input)),
        preserved_spans,
    }
}

fn paragraph_count(input: &str) -> usize {
    input
        .split("\n\n")
        .filter(|paragraph| !paragraph.trim().is_empty())
        .count()
}

fn word_count(input: &str) -> usize {
    word_regex().map_or(0, |regex| regex.find_iter(input).count())
}

fn trim_trailing_space(out: &mut String) {
    while out.ends_with(' ') {
        let _ = out.pop();
    }
}

fn needs_space_before(out: &str) -> bool {
    !out.is_empty() && !out.ends_with([' ', '\n', '\r', '(', '[', '{'])
}

fn is_line_start(input: &str, pos: usize) -> bool {
    pos == 0
        || input
            .get(..pos)
            .is_some_and(|prefix| prefix.ends_with('\n'))
}

fn next_char_boundary(input: &str, pos: usize) -> usize {
    input
        .get(pos..)
        .and_then(|rest| rest.chars().next())
        .map_or(input.len(), |ch| pos + ch.len_utf8())
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn url_regex() -> Option<&'static Regex> {
    static REGEX: OnceLock<Option<Regex>> = OnceLock::new();
    REGEX
        .get_or_init(|| Regex::new(r"(?i)^https?://[^\s<>()\]]+$").ok())
        .as_ref()
}

fn path_regex() -> Option<&'static Regex> {
    static REGEX: OnceLock<Option<Regex>> = OnceLock::new();
    REGEX
        .get_or_init(|| Regex::new(r"^(?:\./|\.\./|/|[A-Za-z]:\\)[\w./\\-]+$").ok())
        .as_ref()
}

fn file_line_regex() -> Option<&'static Regex> {
    static REGEX: OnceLock<Option<Regex>> = OnceLock::new();
    REGEX
        .get_or_init(|| Regex::new(r"^[\w./\\-]+:\d+(?::\d+)?$").ok())
        .as_ref()
}

fn cli_flag_regex() -> Option<&'static Regex> {
    static REGEX: OnceLock<Option<Regex>> = OnceLock::new();
    REGEX
        .get_or_init(|| Regex::new(r"^--?[\w-]+(?:=[^\s]+)?$").ok())
        .as_ref()
}

fn number_regex() -> Option<&'static Regex> {
    static REGEX: OnceLock<Option<Regex>> = OnceLock::new();
    REGEX
        .get_or_init(|| Regex::new(r"^(?:0x[0-9a-fA-F]+|\d+(?:\.\d+)*(?:-[A-Za-z0-9]+)?)$").ok())
        .as_ref()
}

fn ticket_regex() -> Option<&'static Regex> {
    static REGEX: OnceLock<Option<Regex>> = OnceLock::new();
    REGEX
        .get_or_init(|| Regex::new(r"^[a-z]{1,6}-\d+$").ok())
        .as_ref()
}

fn identifier_regex() -> Option<&'static Regex> {
    static REGEX: OnceLock<Option<Regex>> = OnceLock::new();
    REGEX
        .get_or_init(|| {
            Regex::new(
            r"^(?:[a-z]+_[a-z0-9_]+|[a-z]+-[a-z0-9-]+|[a-z]+[A-Z][A-Za-z0-9]*|[A-Z][a-z0-9]+[A-Z][A-Za-z0-9]*|[A-Z][A-Z0-9_]+|[A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)+)$",
        )
            .ok()
        })
        .as_ref()
}

fn word_regex() -> Option<&'static Regex> {
    static REGEX: OnceLock<Option<Regex>> = OnceLock::new();
    REGEX
        .get_or_init(|| Regex::new(r"[A-Za-z]+(?:'[A-Za-z]+)?").ok())
        .as_ref()
}

fn phrase_regex() -> Option<&'static Regex> {
    static REGEX: OnceLock<Option<Regex>> = OnceLock::new();
    REGEX
        .get_or_init(|| {
            Regex::new(
                r"(?i)\b(?:i would (?:like to|be (?:happy|glad|pleased) to)(?:\s+let you know that)?|i would like to|let you know that|it is (?:important|worth|necessary)\s+to\s+note\s+that|it should be noted that|please note that|as (?:mentioned|noted|discussed) (?:previously|above|earlier|before)|at this point in time|go ahead and|due to the fact that|in the event that|for the (?:purpose|purposes) of|with (?:regard|regards|respect) to|in (?:terms|light) of|in the context of|in close proximity to|make (?:sure|certain) (?:to|that)|it (?:is|was) worth noting that|as you can (?:see|tell)|needless to say|you (?:should|may|might|can) (?:need to|want to|consider|try to)|you (?:should|may|might) (?:also )?(?:just )?|the (?:best|right|correct) way to|sure|certainly|of course|happy to)\b[!,. ]*",
            )
            .ok()
        })
        .as_ref()
}

fn solution_phrase_regex() -> Option<&'static Regex> {
    static REGEX: OnceLock<Option<Regex>> = OnceLock::new();
    REGEX
        .get_or_init(|| Regex::new(r"(?i)\bimplement an? solution for\b").ok())
        .as_ref()
}

fn extensive_regex() -> Option<&'static Regex> {
    static REGEX: OnceLock<Option<Regex>> = OnceLock::new();
    REGEX
        .get_or_init(|| Regex::new(r"(?i)\bextensive\b").ok())
        .as_ref()
}

fn causal_regex() -> Option<&'static Regex> {
    static REGEX: OnceLock<Option<Regex>> = OnceLock::new();
    // `so` is deliberately excluded: unlike `because`/`therefore` it is
    // overwhelmingly used as a bare intensifier ("so effective", "so good",
    // "so far", "not so much") rather than a causal conjunction, and a
    // conjunction-only regex cannot tell the two apart without full parsing.
    // Converting intensifier `so` to an arrow inverts meaning (D-fix: caveman
    // ultra corrupted "so effective" into "-> effective"). Dropping the
    // alternative entirely is the conservative fix; see the
    // non_causal_so_* corpus cases in tests/corpus/ultra/.
    //
    // D90: `because`/`therefore` themselves no longer become an arrow either.
    // The installed skill's Ultra row strips the causal conjunction outright
    // and explicitly forbids arrows ("NO arrows (X -> Y) -- measured zero
    // token saving under tokenizer, cost decode clarity"). The matched
    // conjunction (with its surrounding whitespace) is replaced with a
    // comma-space join in `substitute_phrases` below — see the
    // ultra/*causal*/*conjunction* corpus cases for the resulting output.
    REGEX
        .get_or_init(|| Regex::new(r"(?i)\s+(?:because|therefore)\s+").ok())
        .as_ref()
}

fn in_order_to_regex() -> Option<&'static Regex> {
    static REGEX: OnceLock<Option<Regex>> = OnceLock::new();
    REGEX
        .get_or_init(|| Regex::new(r"(?i)\bin order to\b\s*").ok())
        .as_ref()
}

fn verbose_prefix_regex() -> Option<&'static Regex> {
    static REGEX: OnceLock<Option<Regex>> = OnceLock::new();
    REGEX
        .get_or_init(|| {
            Regex::new(
                r"(?i)\b(?:in addition(?:ally)?|as a result|it is clear that|it is obvious that|one (?:should|must|can) note that)\b[,.]?\s*",
            )
            .ok()
        })
        .as_ref()
}
