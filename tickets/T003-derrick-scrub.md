# T003 — `derrick-scrub` subprocess output filter

**Specialist owner**: `token-economist` (sonnet, per AGENTS.md routing)
**Crate**: `crates/derrick-scrub`
**Depends on**: nothing in our workspace (pure-function utility)
**Priority**: P1 — unblocks downstream crates that wrap subprocesses, and is one of the two pillars of D29 (scrub fires at every model boundary).

## Why

DESIGN.md §9.B.2 + D29 commit to a derrick-native scrubber that
strips CLI noise before it crosses any model boundary
(subprocess output → next pipeline step, host tool calls →
model context). RTK-equivalent in behaviour but in-process and
contractually drift-tolerant (D7: scrubber rules may evolve as
CLI output shapes change upstream).

"Strip CLI noise" in §9.B.2 is the headline; in practice that
includes mild structural rewrites (folding `bd list` rows to
`id: title`, collapsing repeated progress markers). This ticket
treats both as in-scope under the same crate — the rule shape
(below) is expressive enough for either.

**Guardrail**: scrub rules may compact or normalise, **but
never paraphrase**. The signal in the line — identifiers,
ticket ids, file paths, error codes, exact error messages —
must survive byte-for-byte. If a rule needs to drop semantic
content, it belongs in `derrick-caveman` (prose compression),
not here.

## Scope

### Public API

```rust
//! Scrub CLI noise from subprocess output before it crosses a
//! model boundary. See DESIGN.md §9.B.2 and D29.

/// A scrubber instance configured with a registry of per-tool
/// rule sets. Cheap to clone (rules are reference-counted).
#[derive(Clone)]
pub struct Scrubber { /* opaque */ }

impl Scrubber {
    /// Construct with the default rule set (every tool registered
    /// in `crates/derrick-scrub/src/rules/`).
    pub fn with_defaults() -> Self;

    /// Construct without any rules. Useful for tests that want
    /// to verify rules in isolation.
    pub fn empty() -> Self;

    /// Register or replace rules for a tool.
    pub fn register(&mut self, tool: &str, rules: RuleSet);

    /// Scrub bytes claimed to originate from `tool` (e.g. "gt",
    /// "bd", "git", "gh", "claude", "codex", "copilot").
    /// Returns the scrubbed bytes paired with per-call stats.
    /// Bytes not matching any rule pass through untouched.
    /// UTF-8 invalid sequences pass through as-is; the scrubber
    /// never panics on input shape.
    pub fn scrub(&self, tool: &str, input: &[u8]) -> (Vec<u8>, ScrubStats);

    /// Streaming variant. The returned `ScrubReader` buffers
    /// internally line-by-line. The invariant is:
    /// `read_to_end(scrub_stream(tool, &input[..])) == scrub(tool, input).0`
    /// for any byte slice `input`. CRLF and bare-LF newlines are
    /// preserved verbatim. A trailing partial line (no terminating
    /// newline) is scrubbed and emitted without inventing one.
    /// Malformed UTF-8 split across chunk boundaries is rejoined
    /// before rule matching, never observed by user code.
    pub fn scrub_stream<R: std::io::Read>(
        &self,
        tool: &str,
        reader: R,
    ) -> ScrubReader<R>;
}

/// Streaming-scrub adapter. Implements `Read`; after the inner
/// reader returns EOF, call `into_stats()` to retrieve the
/// per-call stats (the byte counts are only final once EOF is
/// observed). `into_stats()` before EOF returns the running
/// counters with `eof: false`.
pub struct ScrubReader<R> { /* opaque */ }

impl<R: std::io::Read> std::io::Read for ScrubReader<R> { /* ... */ }

impl<R> ScrubReader<R> {
    pub fn into_stats(self) -> ScrubStats;
    /// Snapshot without consuming the reader. `stats.eof` is
    /// `false` if more bytes might still be read.
    pub fn snapshot_stats(&self) -> ScrubStats;
}

// Stats are per-call (returned from `scrub()` and from
// `ScrubReader::into_stats()`). Aggregation across calls is the
// caller's job (typically `derrick gain`'s).

/// A set of rules for one tool. Internally a Vec of Rule; each
/// rule is a pattern + action.
///
/// **Rule application order**: rules are evaluated against each
/// input line in insertion order. The first rule whose `pattern`
/// matches *and whose action consumes the line entirely* —
/// `Drop`, `KeepFirstDropRest`, or `Collapse` — wins; remaining
/// rules are not evaluated for that line. `Replace` actions are
/// applied in-place and **do not short-circuit**: a line may pass
/// through multiple `Replace` rules before being emitted. This
/// makes "strip ANSI escapes then fold the remaining text"
/// expressible as two rules, in that order.
#[derive(Clone, Default)]
pub struct RuleSet { /* opaque */ }

impl RuleSet {
    pub fn new() -> Self;
    pub fn add(&mut self, rule: Rule);
    pub fn extend<I: IntoIterator<Item = Rule>>(&mut self, rules: I);
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
}

/// One scrub rule: a regex pattern over lines and an action.
#[derive(Clone)]
pub struct Rule {
    pub pattern: regex::Regex,
    pub action: Action,
    /// Human-readable name for telemetry: "strip gh spinner".
    pub name: &'static str,
}

/// A replacement template string. `$1`/`$2`/... refer to the
/// regex's capture groups; `$0` is the whole match; `$$` is a
/// literal dollar sign. Plain strings without `$` are emitted
/// verbatim.
#[derive(Clone, Debug)]
pub struct Replacement(pub String);

#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Action {
    /// Drop the entire line.
    Drop,
    /// Replace the matched portion (per `Regex::replace_all`)
    /// with the rendered Replacement.
    Replace(Replacement),
    /// Keep the first line of a run that matches this rule
    /// (against the rule's `key`, or raw line equality if
    /// `key == None`); drop subsequent consecutive matches.
    /// Resets when a non-matching line appears.
    KeepFirstDropRest {
        /// Optional key extractor — a `Replacement` evaluated
        /// against the matched line; consecutive lines whose
        /// key is byte-equal collapse. `None` falls back to raw
        /// line equality.
        key: Option<Replacement>,
    },
    /// Collapse a run of consecutive matching lines into one
    /// rendered output line.
    ///
    /// **Newline policy**: the collapsed output line carries the
    /// terminator (`\r\n`, `\n`, or none) of the *last* line in
    /// the run, so mixed-newline input doesn't produce surprising
    /// emissions. Implementations must preserve this exactly.
    Collapse {
        /// What the collapsed run renders to. `$N` refers to
        /// captures on the first matching line; **`$count` is a
        /// Collapse-only expansion** — it is reserved for this
        /// action and resolves to the number of collapsed lines.
        /// In any other `Replacement` context `$count` is a
        /// literal and emits as `$count`.
        render: Replacement,
        /// Optional key for grouping; lines with the same
        /// key collapse together. `None` collapses any
        /// consecutive match.
        key: Option<Replacement>,
    },
}

#[derive(Clone, Debug, Default)]
pub struct ScrubStats {
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub lines_in: u64,
    pub lines_out: u64,
    pub rules_fired: std::collections::HashMap<String, u64>,
    /// True when the input stream has hit EOF and all buffered
    /// state has been emitted. `scrub()` always sets this to
    /// `true` (it consumes the whole slice). `ScrubReader::
    /// snapshot_stats()` may return `false` if reads are still
    /// pending; `into_stats()` returns `true` because the reader
    /// has been consumed.
    pub eof: bool,
}

impl ScrubStats {
    pub fn savings_pct(&self) -> f64;
}
```

### Built-in rule sets (one file per tool)

`crates/derrick-scrub/src/rules/`:

- `gt.rs` — strip gastown `gt` output noise: ANSI clear/reset
  sequences, progress spinners, repeated header banners.
- `bd.rs` — fold `bd list` rows to `id: title` format.
- `git.rs` — strip git progress (`remote: Counting objects: ...`),
  collapse consecutive identical "warning: ..." lines.
- `gh.rs` — strip GitHub CLI spinners (`⠋ Fetching ...`), drop the
  "✓" prefix lines after completion.
- `claude.rs` — strip Claude Code's `[INFO]`/`[DEBUG]` noise if
  any leaks through, fold "Tool use" decorations.
- `codex.rs` — strip codex's "tokens used" preambles, drop the
  "exec" recap lines, collapse "succeeded in Xms" footers.
- `copilot.rs` — strip the "● Read file.rs" decorations, collapse
  "Premium request" telemetry footers.
- `cargo.rs` — strip "Compiling /Checking" progress lines after
  the first; preserve final result lines (test pass/fail counts).

Each rule file exposes a `pub fn rules() -> RuleSet` that
`Scrubber::with_defaults()` calls to populate the registry.

### CLI

Exposed via `derrick-cli` later (T-future), but the *library*
provides:

```rust
/// Convenience for the `derrick scrub <tool> [-]` subcommand:
/// reads from `input` (stdin if `-`), scrubs through `tool`'s
/// rules, writes to `output`. Returns `ScrubStats`.
pub fn scrub_io(
    scrubber: &Scrubber,
    tool: &str,
    input: &mut dyn std::io::Read,
    output: &mut dyn std::io::Write,
) -> std::io::Result<ScrubStats>;
```

### Dependencies (workspace.dependencies only)

```toml
[dependencies]
regex = { workspace = true }      # add to workspace deps
serde = { workspace = true }       # ScrubStats serializes for `derrick gain`
thiserror = { workspace = true }

[dev-dependencies]
# none — pure unit tests on bytes-in / bytes-out
```

Add `regex = "1"` to workspace.dependencies in the root Cargo.toml.

### Tests

Pure I/O-free tests. For each tool's rules file, a paired
corpus under `crates/derrick-scrub/tests/corpus/<tool>/`:

- `<tool>/<case>.in` — raw bytes.
- `<tool>/<case>.out` — expected scrubbed bytes.

Walked by a parameterized test:

```rust
#[test]
fn corpus_round_trips() {
    let scrubber = Scrubber::with_defaults();
    for case in walk("tests/corpus/") {
        let input = read(&case.input);
        let expected = read(&case.expected);
        let got = scrubber.scrub(case.tool, &input);
        assert_eq!(got, expected, "case {:?} mismatch", case.path);
    }
}
```

Minimum corpus coverage per tool: at least 3 cases each
(typical, edge, degenerate).

Per-rule regression tests: **every rule in every built-in
ruleset has at least one named unit test** asserting the exact
noise it strips and the exact signal it preserves. Use a naming
convention `<tool>_<rule_slug>_<scenario>` (e.g.
`gh_spinner_dropped_mid_progress`). Coverage at the rule level
is what keeps D7 honest as CLIs drift upstream.

Plus framework-level tests:

- `unknown_tool_passes_through_unchanged`.
- `empty_input_produces_empty_output`.
- `non_utf8_passes_through_without_panic` — a malformed byte
  sequence in the middle of a line doesn't crash.
- `stats_savings_pct_rounds_correctly`.
- `stream_variant_matches_buffered_for_each_corpus_case` —
  asserts the invariant `read_to_end(scrub_stream(input)) ==
  scrub(input).0` over every corpus case.
- `stream_handles_crlf_newlines_unchanged`.
- `stream_handles_missing_trailing_newline`.
- `stream_handles_malformed_utf8_across_chunk_boundary` — a
  multi-byte sequence split by an internal buffer boundary
  doesn't change the output.
- `register_replaces_existing_rules_for_same_tool`.
- `keep_first_drop_rest_resets_on_non_match`.
- `collapse_count_capture_renders_correctly`.

**Coverage target**: 90%+ on this crate (it's small,
pattern-matching, and very testable).

## Out of scope

- Token telemetry beyond `ScrubStats`. That's a downstream
  concern (`derrick gain`).
- Per-line text shaping (drop articles, collapse prose). That's
  `derrick-caveman` (T004).
- The Claude Code `PreToolUse`/`PostToolUse` hook integration
  (D29). That's `derrick-adopt`'s job — it writes the hook
  configs that *invoke* `derrick scrub`.

## Acceptance

- [ ] `cargo check -p derrick-scrub` passes.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes.
- [ ] `cargo test -p derrick-scrub` passes.
- [ ] `cargo llvm-cov -p derrick-scrub --fail-under-lines 90` passes.
- [ ] Workspace `cargo llvm-cov --fail-under-lines 80` still passes.
- [ ] At least 3 corpus cases per built-in tool.
- [ ] No `unwrap`/`expect`/`panic` in non-test code.
- [ ] Stress-test 3× at default `--test-threads`, all green.
- [ ] Every public type and method documented.

## Reviewer notes (Codex)

Pre-implementation review. Crate is a stub by design. Focus on:
- Is the rule-set abstraction expressive enough for the
  rules listed?
- Does the streaming API hold up (line-buffered scrubbing
  is the practical mode; do we need anything else)?
- Are the per-tool rule scopes correct? Anything missing?
- Are the tests sufficient for byte-determinism?

## Implementer notes (Copilot)

Stay in `crates/derrick-scrub/`. Add `regex = "1"` to root
`Cargo.toml` `[workspace.dependencies]`. Real corpus files
under `crates/derrick-scrub/tests/corpus/`; small, hand-written
sample outputs from each tool.
