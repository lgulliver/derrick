# T004 — `derrick-caveman` text compressor

**Specialist owner**: `token-economist` (sonnet, per AGENTS.md routing)
**Crate**: `crates/derrick-caveman`
**Depends on**: nothing in our workspace (pure function utility)
**Priority**: P1 — second pillar of D29 (caveman fires at every model boundary), prose-side counterpart to `derrick-scrub`.

## Why

DESIGN.md §9.B.3 + D29 commit to a derrick-native caveman that
compresses prose passing into a model context. The contract
(D7): **byte-identical to the `caveman` skill at matched
intensities**. Identifiers, paths, error messages, file/line
refs preserved verbatim — only natural-language prose flattens.

## Scope

### Public API

```rust
//! Caveman text compressor. Pure-Rust shaping rules byte-identical
//! to the caveman skill at matched intensities. See DESIGN.md §9.B.3
//! and D29.

/// How aggressively to compress. Matches the skill's named modes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Intensity {
    /// Light compression — drop fillers, keep almost all structure.
    /// Safe default for inter-step handoffs where readability matters.
    #[default]
    Lite,
    /// Full compression — drop articles, collapse repetition,
    /// rewrite stock phrases. Default for `derrick gain`-tracked
    /// boundary crossings.
    Full,
    /// Maximum compression. Reserved for cases where the model
    /// reading the result is the one that produced it (round-trip
    /// safe by construction). Don't use for cross-model handoff.
    Ultra,
}

/// Compress text at the given intensity. Identifiers, code spans,
/// file paths, error messages, and URLs survive byte-for-byte
/// (they are protected via a tokeniser pre-pass; see "Preservation"
/// below).
pub fn compress(input: &str, intensity: Intensity) -> CompressOutput;

/// Streaming compressor for large inputs (the inter-step
/// summary path).
///
/// **Lexer state survives across `write_str` calls.** The
/// streaming compressor maintains protected-region state so a
/// fenced code block, multiline diagnostic, URL, or identifier
/// that starts in one chunk and ends in another is treated as
/// a single protected span. Paragraph boundary buffering alone
/// is not sufficient; the implementation must be a stateful
/// byte-stream lexer.
///
/// Memory bound: implementations cap the unbounded buffer at
/// 1 MiB. If a single in-progress protected region (e.g. a
/// gigantic code block) exceeds that, the compressor flushes
/// it as-is and emits an `OversizedProtectedSpan` event in
/// stats so the caller can decide whether to split upstream.
pub struct Compressor { /* opaque */ }

impl Compressor {
    pub fn new(intensity: Intensity) -> Self;
    /// Feed input. May return zero or more compressed regions
    /// depending on how many complete protected/prose boundaries
    /// have been observed. May return an empty `Vec` if all
    /// input is still buffered inside an open protected region.
    pub fn write_str(&mut self, input: &str) -> Vec<String>;
    /// Drain remaining buffered text. Implicitly closes any
    /// open protected region (treated as ended at EOF).
    pub fn finish(self) -> CompressOutput;
}

#[derive(Clone, Debug, Default)]
pub struct CompressOutput {
    pub text: String,
    pub stats: CompressStats,
}

#[derive(Clone, Debug, Default)]
pub struct CompressStats {
    pub chars_in: u64,
    pub chars_out: u64,
    pub words_in: u64,
    pub words_out: u64,
    pub paragraphs_processed: u64,
    /// Protected regions left untouched (counted, not stripped).
    pub preserved_spans: u64,
}

impl CompressStats {
    pub fn savings_pct(&self) -> f64;
}
```

### Preservation contract

Caveman compresses prose. It must not touch:

- Inline code spans (`` `...` `` and `` ```...``` `` blocks).
- File paths matching `^(\./|\.\./|/|[A-Za-z]:\\)[\w./\\\-]+$` or
  appearing inside the fenced code-block content.
- File:line[:col] refs: `<path>:<line>` and
  `<path>:<line>:<col>` (rustc's `--> file:line:col`
  diagnostic shape is the canonical form).
- Markdown links and autolinks: `[text](url)` and `<url>`
  forms — the text inside `[...]` is also protected to avoid
  garbling link labels.
- Multiline diagnostics: rustc-style indented spans starting
  with `^|`, `|`, or `-->` are protected as a contiguous
  block until the next blank line.
- CLI flags / options: tokens starting with `-` or `--`
  followed by word characters, optionally with `=value`.
- Error messages: anything between `error:` / `Error:` and the
  next sentence terminator (or end of paragraph).
- Identifiers in `snake_case`, `kebab-case`, `camelCase`,
  `PascalCase`, `ALL_CAPS_CONST`, or `module::path::form`.
- URLs (per a conservative URL regex).
- Numbers, including version strings (`1.2.3-rc4`), hex
  (`0xdead`), and decimals.
- Ticket-id-shaped tokens — see `derrick-substrate::TicketId`
  regex `^[a-z]{1,6}-\d+$` (e.g. `drk-1`, `mp-42`; note that
  `derrick-42` would *not* match because `derrick` is 7 chars).

Implementation strategy: pre-pass tokenises the input, marking
each span as `Protected` or `Prose`. Compression rules apply to
`Prose` spans only. After compression the output is reassembled
preserving the original protected-span byte sequences.

### Shaping rules

**The caveman skill is the source of truth (D7).** Do **not**
invent rules in this crate. The implementer ports the rule set
from the installed caveman skill at
`~/.agents/skills/caveman/SKILL.md` (or its equivalent location
on the implementer's machine) into Rust, preserving the
intensity-level semantics described there:

- `Lite` — light shaping; full sentences and articles
  preserved; whitespace and obvious filler stripped.
- `Full` — default; more aggressive prose flattening per the
  skill's specification.
- `Ultra` — maximum compression including the skill's
  abbreviation / arrow / conjunction-stripping conventions
  (e.g. `X -> Y`).

**The corpus is the parity gate.** Every corpus case in
`tests/corpus/<intensity>/` is a pinned input → output pair
known to match the skill at the corresponding intensity. The
`tests/skill_parity.rs` `#[ignore]` test (described below) is
the regenerator the implementer uses when porting rules and
when the skill ships a new version.

### Dependencies (workspace.dependencies only)

```toml
[dependencies]
serde = { workspace = true }
regex = { workspace = true }       # shared with derrick-scrub
thiserror = { workspace = true }

[dev-dependencies]
# none — pure pure-function tests
```

### Tests

The acceptance contract is **byte-identical to the caveman
skill**. We pin this with a corpus of paired inputs/outputs.

Corpus at `crates/derrick-caveman/tests/corpus/<intensity>/<case>.{in,out}`.

Preservation cases (all three intensities):

- `preserves_identifiers.in` / `.out`
- `preserves_paths.in` / `.out`
- `preserves_code_blocks.in` / `.out`
- `preserves_inline_code_spans.in` / `.out`
- `preserves_error_messages.in` / `.out`
- `preserves_urls.in` / `.out`
- `preserves_ticket_ids.in` / `.out`
- `preserves_numbers_and_versions.in` / `.out`
- `preserves_file_line_col_refs.in` / `.out`  (e.g. `src/lib.rs:42:7`)
- `preserves_markdown_links.in` / `.out`  (`[label](url)`, `<url>`)
- `preserves_cli_flags.in` / `.out`  (`--release`, `-p name`, `--fail-under-lines=80`)
- `preserves_rustc_diagnostics.in` / `.out`  (multi-line `--> file:line:col` block)
- `preserves_mixed_prose_protected.in` / `.out`  (one sentence with multiple span kinds)

Compression cases (varies by intensity, per the skill):

- `lite/light_shaping.in` / `.out`
- `full/article_drop.in` / `.out`
- `full/stock_phrase_substitution.in` / `.out`
- `full/whitespace_collapse.in` / `.out`
- `ultra/abbreviation_and_arrows.in` / `.out`
- `ultra/conjunction_stripping.in` / `.out`

Boundary / edge cases (all intensities):

- `chunk_split_fenced_code.in` / `.out` (streaming-specific —
  input gets chunked at byte offsets crossing a code-fence
  boundary)
- `chunk_split_url.in` / `.out`
- `chunk_split_identifier.in` / `.out`
- `crlf_input.in` / `.out`
- `no_trailing_newline.in` / `.out`
- `one_very_long_paragraph.in` / `.out`
- `ambiguous_uncompressed.in` / `.out`  (a case where dropping
  a word would change meaning; output equals input)

Plus unit tests:

- `intensity_serde_round_trip` — `Intensity` serialises as
  `"lite" | "full" | "ultra"`.
- `compress_empty_input` — empty string round-trips empty.
- `stats_chars_words_paragraphs` — counts are correct on a
  hand-crafted sample.
- `savings_pct_on_known_input` — verified savings for a
  documented corpus entry, used as the public-facing example
  in `derrick gain --pillars`.
- `streaming_matches_one_shot_for_corpus` — for every corpus
  case, feeding chunks (1 char, 16 chars, 1KB chunks) through
  `Compressor` yields output equivalent to `compress()`.
- `protected_span_count_is_accurate` — preserved-span stat
  matches actual protected-region count.

**Coverage target**: 90%+. Pure function plus shaping rules;
high coverage is achievable.

### Skill-parity verification

A test that runs at CI time (not gated on every test invocation,
because we don't want to invoke the host CLI in unit tests):

- `tests/skill_parity.rs` — `#[ignore]` by default. When run via
  `cargo test -- --ignored`, it invokes `claude` to run the
  `caveman` skill on each corpus input and diffs against our
  output. CI does **not** run this by default; we ship it as a
  manual diagnostic for the implementer to use when porting
  the rule set initially and when the skill ships a new
  version.

## Out of scope

- Per-tool routing (caveman is intensity-driven; tool-specific
  prose handling belongs at the call site).
- Scrubber-style structural rewrites — those are `derrick-scrub`
  (T003).
- Hook installation — `derrick-adopt`'s job (D29).
- **D8 fallback (invoking the caveman skill for unknown artifact
  types).** Fallback is a *routing* concern, not a per-function
  concern. This crate exposes `compress()` which always returns
  output; if the caller has classified an input as "unknown
  artifact" and decided to defer to the skill, it does so
  *without* calling this crate. The decision lives at the
  call site (`derrick-flow` for inter-step handoffs;
  `derrick-adopt`-installed hooks for host tool I/O).

## Acceptance

- [ ] `cargo check -p derrick-caveman` passes.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes.
- [ ] `cargo test -p derrick-caveman` passes; corpus + unit tests
      present.
- [ ] `cargo llvm-cov -p derrick-caveman --fail-under-lines 90` passes.
- [ ] Workspace `cargo llvm-cov --fail-under-lines 80` still passes.
- [ ] Every public type and method documented.
- [ ] No `unwrap`/`expect`/`panic` in non-test code.
- [ ] Stress-test 3× at default `--test-threads`, all green.
- [ ] No gastown vocabulary (`bead`/`convoy`/`polecat`/`mayor`/
      `sling`) anywhere in `crates/derrick-caveman/`.

## Reviewer notes (Codex)

Pre-implementation review. Focus on:
- Is the preservation contract complete? Anything obvious
  missing?
- Are the intensity rules coherent across the three levels?
- Are there cases the corpus should cover that aren't listed?
- Is the streaming compressor's paragraph-boundary buffering
  sufficient or does it need explicit handling for
  no-newline-EOF / very long paragraphs?

## Implementer notes (Copilot)

Stay in `crates/derrick-caveman/`. Reuse the workspace `regex`
dep added in T003 (`regex = "1"` in `[workspace.dependencies]`).
Hand-author the corpus files; do not generate them from the
skill in this commit (skill-parity verification is a separate
diagnostic per the spec).
