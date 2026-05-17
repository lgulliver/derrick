# T005 — `derrick-memory` memory seeding and lessons

**Specialist owner**: `token-economist` (sonnet)
**Crate**: `crates/derrick-memory`
**Depends on**: `derrick-config` (for `Site` namespacing) + `derrick-substrate` (for the ticket-id regex source of truth)
**Priority**: P2 — required for §9.A pillar (memory) but not blocking dogfooding bar.

## Why

DESIGN.md §9.A defines five layers of memory derrick curates.
**Two storage domains**, per the design text:

- **Host auto-memory dir** (`~/.claude/projects/<repo>/memory/`
  namespaced under `derrick/<site>/`) — *init-time seeds only*
  (§9.A.1).
- **Repo `.derrick/`** — per-run digests (§9.A.2 →
  `.derrick/runs/<ts>/memory.md`), per-feature state (§9.A.3 →
  `.derrick/state.json` under `features.<slug>`), and cross-
  feature lessons (§9.A.4 → `.derrick/lessons.md`).

This ticket implements the filesystem primitives for both
domains plus the quality gate from D9. The two-domain split is
load-bearing — get it wrong and downstream `derrick-flow`
semantics break.

## Scope

### Public API

```rust
//! Derrick memory layers. See DESIGN.md §9.A.

use derrick_config::Site;

/// Handles for both memory domains for a given site:
/// the host auto-memory dir (init seeds only) and the repo's
/// `.derrick/` (everything else).
pub struct MemoryStore { /* opaque */ }

pub struct MemoryPaths {
    /// Host auto-memory **root** (the parent the crate appends
    /// `derrick/<site_name>/` to). For Claude Code that's
    /// typically `~/.claude/projects/<repo>/memory/`. None when
    /// the host doesn't expose a memory dir (codex, copilot).
    pub host_memory_root: Option<PathBuf>,
    /// Repo's `.derrick/` directory. **Not** site-namespaced:
    /// per §9.A.2–§9.A.4, runs/state/lessons live at fixed
    /// paths under `.derrick/` shared across all sites that
    /// share a repo. In practice repos have exactly one site,
    /// but the model is "repo-scoped, not site-scoped" for
    /// this domain.
    pub repo_state: PathBuf,
}

impl MemoryStore {
    /// Construct with explicit paths. Callers (typically
    /// `derrick-flow` during init) resolve `host_memory` based
    /// on which host is active and `repo_state` from
    /// `derrick.yaml`'s `state.dir` (default `.derrick/`).
    pub fn open(paths: MemoryPaths, site: &Site)
        -> Result<Self, MemoryError>;

    // --- Init-time seeding (§9.A.1) ---

    /// Write the project / reference / feedback seed files.
    /// Idempotent: re-running with the same inputs is a no-op.
    /// Returns the list of paths written or updated.
    pub fn seed(&self, seeds: &Seeds) -> Result<Vec<PathBuf>, MemoryError>;

    /// Remove every file under `derrick/<site>/`. Used by
    /// `derrick init --unmemoize`.
    pub fn unmemoize(&self) -> Result<(), MemoryError>;

    /// List all memory entries for this site.
    pub fn list(&self) -> Result<Vec<MemoryEntry>, MemoryError>;

    // --- Per-run digest (§9.A.2) ---

    /// Append a one-line digest to the run's memory.md.
    pub fn append_run_digest(
        &self,
        run_id: &str,
        line: &str,
    ) -> Result<(), MemoryError>;

    // --- Per-feature state (§9.A.3) ---

    /// Read/write per-feature state as JSON. Caller picks the
    /// schema; this crate just enforces the path layout.
    pub fn get_feature_state<T: for<'a> serde::Deserialize<'a>>(
        &self,
        feature_slug: &str,
    ) -> Result<Option<T>, MemoryError>;

    pub fn set_feature_state<T: serde::Serialize>(
        &self,
        feature_slug: &str,
        state: &T,
    ) -> Result<(), MemoryError>;

    /// Remove per-feature state when a batch closes.
    pub fn prune_feature_state(&self, feature_slug: &str)
        -> Result<(), MemoryError>;

    // --- Cross-feature lessons (§9.A.4) ---

    /// Append a lesson. Subject to the quality gate (D9): the
    /// lesson body **must** contain at least one ticket-id-shaped
    /// token or a constitution-section anchor like `#section-3.2`.
    /// Otherwise returns `MemoryError::Rejected` and does not
    /// write.
    pub fn append_lesson(&self, lesson: &Lesson)
        -> Result<(), MemoryError>;

    /// List lessons newer than `since` (or all if `None`).
    pub fn lessons(&self, since: Option<DateTime<Utc>>)
        -> Result<Vec<Lesson>, MemoryError>;

    /// Remove lessons older than `older_than` (or all if `None`).
    /// Used by `derrick memory prune --older-than 90d`.
    pub fn prune_lessons(&self, older_than: Option<DateTime<Utc>>)
        -> Result<usize, MemoryError>;
}
```

### Types

```rust
pub struct Seeds {
    /// Project facts: site name, ticket prefix, mode,
    /// primary languages, constitution path. One file per fact.
    pub project: Vec<(String /*name*/, String /*body*/)>,
    /// Reference facts: where artifacts live.
    pub reference: Vec<(String, String)>,
    /// Feedback facts: derrick's own guardrails.
    pub feedback: Vec<(String, String)>,
}

pub struct Lesson {
    pub at: DateTime<Utc>,
    pub batch: Option<String>,   // batch slug if extracted from a batch closure
    pub body: String,            // gate-checked: must reference a ticket id or section
}

pub struct MemoryEntry {
    pub layer: MemoryLayer,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub modified: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum MemoryLayer {
    Project,
    Reference,
    Feedback,
    RunDigest,
    FeatureState,
    Lessons,
}

#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum MemoryError {
    #[error("io error at {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },

    #[error("lesson rejected by quality gate: {reason}")]
    Rejected { reason: String },

    #[error("invalid input: {field}: {message}")]
    Invalid { field: String, message: String },
}
```

### Layout on disk

Two domains, matching DESIGN.md §9.A exactly:

```
# Host auto-memory (init seeds only)
# host_memory_root + "derrick" + site_name, e.g.
# ~/.claude/projects/<repo>/memory/derrick/<site_name>/
<host_memory_root>/derrick/<site_name>/
├── project/
│   ├── site.md        # one fact per file, ≤150 chars body
│   ├── prefix.md
│   ├── mode.md
│   ├── languages.md
│   └── constitution.md     # path pointer, not the body
├── reference/
│   ├── tasks.md
│   ├── verdicts.md
│   └── status.md
├── feedback/
│   └── guardrails.md       # one entry per line
└── MEMORY.md          # one-line entries pointing at the per-file facts

# Repo state dir (`.derrick/` by default)
<repo_state>/
├── runs/
│   └── <utc-ts>/
│       └── memory.md       # appended one-line digests
├── state.json              # features.<slug> per-feature state
└── lessons.md              # newline-delimited JSON lessons
```

`MEMORY.md` is required by the Claude Code memory system per
the global CLAUDE.md spec: it indexes the per-file facts (one
line each, ≤150 chars) so the auto-memory system loads only the
index until a fact is referenced. Per-file fact bodies are
larger but loaded on demand.

`lessons.md` format: each entry is a single line of JSON
(`{"at": "...", "batch": "...", "body": "..."}`) so the file
remains greppable but also machine-readable. **No vocabulary
that conflicts with AGENTS.md house rule 1** — never name a
fact file `rig.md` or similar.

### Atomic write semantics

All mutating filesystem operations use temp-file-and-rename so
a crashed process never leaves a partial file:

- `seed()` — each fact file written to `<path>.tmp` then renamed.
- `set_feature_state()` — `state.json.tmp` → rename. Reads of
  `state.json` during a concurrent write see the prior version
  or the new one, never a partial one.
- `append_run_digest()` — open-for-append with `O_APPEND` so
  short writes interleave atomically at line boundaries on
  POSIX. Windows: fall back to a small flock.
- `append_lesson()` — same as run digest.
- `unmemoize()` — recursive remove of `<host_memory>/derrick/<site>/`
  only; the rest of the user's memory dir is untouched.

### Quality gate (D9)

A lesson body passes the gate if **either**:

- It **contains a substring** matching the ticket-id regex
  `\b[a-z]{1,6}-\d+\b` (unanchored, word-boundary-bracketed).
  The regex itself **must be defined in** `derrick-substrate`
  and re-exported (e.g. `derrick_substrate::ticket_id_regex()`)
  so this crate does not duplicate the source of truth. If
  re-export is not feasible without refactoring T002, vendor
  the regex with a `// keep in sync with
  derrick-substrate::TicketId` comment and a regression test
  that constructs both regexes and compares their patterns
  byte-for-byte. Prefer the re-export.
- It **contains a substring** matching the constitution
  section-anchor regex `#[A-Za-z0-9.-]+\b` (e.g. `#9.B.7`,
  `#substrate-design`, `#D29`). Case-insensitive on purpose so
  both `#9.B.7` and `#9.b.7` validate.

Both checked via `Regex::is_match` against the body. If
neither matches, `append_lesson` returns
`MemoryError::Rejected { reason }` where `reason` cites the
two patterns expected. The lesson is **not** written to disk.

### Brownfield empty-lessons handling (D23)

D23 says: when a constitution doesn't exist yet, lessons file
stays empty rather than relaxing the gate. **This crate does
not implement that decision** — the gate is unconditional.
The decision lives in `derrick-flow`'s post-batch lesson-
extraction step: if `guardrails.constitution_path` doesn't
exist on disk, `derrick-flow` skips lesson extraction entirely
and never calls `append_lesson` on this store. No marker
files, no constitution awareness in `MemoryStore`.

### Dependencies

```toml
[dependencies]
derrick-config = { path = "../derrick-config" }
derrick-substrate = { path = "../derrick-substrate" }   # re-uses TicketId regex
serde = { workspace = true }
serde_json = { workspace = true }
thiserror = { workspace = true }
chrono = { workspace = true }
regex = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
```

The implementer must expose the ticket-id pattern from
`derrick-substrate` (e.g. add `pub fn ticket_id_pattern() ->
&'static str { "^[a-z]{1,6}-\\d+$" }` to that crate) and re-use
it here. If exposing requires non-trivial substrate refactors,
vendor with a regression test that asserts pattern equality
(per the gate spec above).

### Tests

- `seed_writes_all_layers_idempotently` — running `seed()` twice
  produces no diff.
- `unmemoize_removes_only_derrick_namespace` — files outside
  `derrick/<site>/` are untouched.
- `list_returns_all_layer_entries`.
- `run_digest_appends_atomically` — concurrent appends from
  spawned threads end up in a coherent file.
- `feature_state_round_trip` — set then get returns same value.
- `prune_feature_state_removes_only_that_feature`.
- `lesson_with_ticket_id_passes` — `"the mp-47 retry bug ..."`.
- `lesson_with_section_anchor_passes` — `"per #9.B.7 ..."`.
- `lesson_without_either_is_rejected` — `"be careful with concurrency"`
  → `MemoryError::Rejected`.
- `lesson_rejected_message_includes_offending_body` — the
  caller can show the user why.
- `prune_lessons_removes_only_old_ones` — verify timestamp
  boundary inclusively.
- `multiple_sites_dont_collide_in_host_memory` — open two
  stores with the *same* `host_memory_root` but different
  `Site.name`, write seeds to each, confirm
  `derrick/<site>/` namespace separation. (Repo-state domain
  is repo-scoped not site-scoped per §9.A.2–§9.A.4, so no
  isolation test there.)
- `atomic_write_survives_kill_mid_save` — spawn a writer task
  that calls `set_feature_state`, kill it mid-write via a
  panic injection, confirm the on-disk file is either fully
  the old version or fully the new — never partial.
- `ticket_id_regex_matches_substrate_source_of_truth` — a
  compile-time or runtime assertion that this crate's
  ticket-id regex (whether re-exported or vendored) matches
  `derrick-substrate::TicketId`'s validator on a corpus of
  inputs.

**Coverage target**: 90%.

## Out of scope

- Lesson extraction (the LLM call that produces lesson bodies).
  That's `derrick-flow`'s post-batch hook; this crate only
  enforces the gate and the layout.
- Telemetry / `derrick gain` aggregation across memory layers.
  Separate concern.
- The `derrick memory list/show/prune/unmemoize` CLI
  surface — `derrick-cli` wraps this crate later.

## Acceptance

- [ ] `cargo check -p derrick-memory` passes.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] `cargo test -p derrick-memory` passes; stress 3× green.
- [ ] `cargo llvm-cov -p derrick-memory --fail-under-lines 90`.
- [ ] Workspace `cargo llvm-cov --fail-under-lines 80` still passes.
- [ ] Every public type/method documented.
- [ ] No `unwrap`/`expect`/`panic` in non-test code.
- [ ] No gastown vocabulary anywhere in the crate.

## Reviewer notes (Codex)

Pre-implementation review. Focus on:
- Is the layer split sensible? Anything missing or
  inappropriately bundled?
- Is the quality-gate regex pair (ticket id OR section
  anchor) sufficient? Easy to bypass with a fake token?
- Are the file-system operations atomic where they need to
  be (run digest appends in particular)?
- Does this play right with §9.A and D9 + D23?

## Implementer notes (Copilot)

Stay in `crates/derrick-memory/`. The `regex` workspace
dependency was added in T003. The `derrick-config` path dep
follows the same pattern as `derrick-substrate`. Tests use
`tempfile::tempdir()` for the memory root — no environment
variable mutation needed (tests pass an explicit root via
`MemoryStore::open_at`).
