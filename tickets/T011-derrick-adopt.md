# T011 — `derrick-adopt` brownfield init + D29 host hooks

**Specialist owner**: `flow-engineer` (opus) — owns adopt + init flow per AGENTS.md routing
**Crate**: `crates/derrick-adopt`
**Depends on**: `derrick-config`, `derrick-substrate`, `derrick-substrate-native`, `derrick-tools` (for speckit/claude/codex detection), `derrick-cli` (to wire bare `derrick init`)
**Priority**: P0 — unblocks self-dogfooding on the derrick repo itself (morning decision question 5) and OSS adoption for any non-empty repo.

## Why

DESIGN.md §5.6 specifies brownfield adoption: walk the repo
for `AGENTS.md` / `CLAUDE.md` / `.claude/` / constitution-like
docs / `.specify/` / `.github/`; classify each as
*adopt as-is*, *reference*, or *augment*; propose the writes;
write only after confirm; install D29 hooks. T008's
`derrick init` ships only the greenfield path and refuses
bare `init` with a T011 pointer. **This ticket is what bare
`derrick init` calls into.**

## Scope (v1)

### Public API

```rust
//! Brownfield-safe init for existing repos.
//! See DESIGN.md §5.2, §5.6, and D29.

use derrick_config::Site;
use std::path::{Path, PathBuf};

pub struct Adopter {
    repo_root: PathBuf,
}

impl Adopter {
    pub fn new(repo_root: impl Into<PathBuf>) -> Self;

    /// Walk the repo, return what's there. No writes, no
    /// network, fast.
    pub fn detect(&self) -> Result<DetectionReport, AdoptError>;

    /// Produce a plan describing what `apply` would write,
    /// what it would reference, and what it would warn about.
    /// Pure function of the detection report + the user's
    /// options.
    pub fn propose(
        &self,
        detection: &DetectionReport,
        opts: &AdoptOptions,
    ) -> Result<AdoptionPlan, AdoptError>;

    /// Apply the plan. Writes only files in `plan.writes`;
    /// touches no others. Returns the outcome with paths
    /// actually written.
    pub async fn apply(&self, plan: &AdoptionPlan)
        -> Result<AdoptionOutcome, AdoptError>;
}
```

### `DetectionReport`

```rust
#[derive(Clone, Debug)]
pub struct DetectionReport {
    pub git_repo: bool,                          // .git/ at root

    // Existing agent + meta files
    pub agents_md: Option<PathBuf>,              // AGENTS.md
    pub claude_md: Option<PathBuf>,              // CLAUDE.md
    pub claude_dir: Option<PathBuf>,             // .claude/
    pub claude_settings: Option<PathBuf>,        // .claude/settings.json
    pub claude_agents: Vec<PathBuf>,             // .claude/agents/*.md
    pub claude_commands: Vec<PathBuf>,           // .claude/commands/*.md
    pub claude_skills: Vec<PathBuf>,             // .claude/skills/*/SKILL.md
    pub codex_instructions: Option<PathBuf>,     // .codex/instructions.md
    pub github_copilot_instructions: Option<PathBuf>,
                                                 // .github/copilot-instructions.md

    // Speckit footprint
    pub specify_dir: Option<PathBuf>,            // .specify/
    pub constitution: Option<PathBuf>,           // .specify/memory/constitution.md or similar

    // Existing derrick state (refuse if --force not set)
    pub existing_derrick_yaml: Option<PathBuf>,
    pub existing_derrick_dir: Option<PathBuf>,

    // Tool availability (for speckit detect-then-defer per D2)
    pub speckit_cli_available: bool,             // `which specify`
    pub claude_cli_available: bool,
    pub codex_cli_available: bool,

    // Tracker / docs we'd reference as constitution sources
    pub readme: Option<PathBuf>,
    pub contributing: Option<PathBuf>,
    pub adrs_dir: Option<PathBuf>,               // docs/adrs/ or similar

    // Repo-level git metadata (for D21 squash warning)
    pub default_branch: Option<String>,
}
```

Detection is **pure read** — no writes, no network, no
subprocess calls beyond `which`-checks. Fast enough to run
on every `derrick doctor` invocation too.

### `AdoptOptions`

```rust
#[derive(Clone, Debug)]
pub struct AdoptOptions {
    pub site_name: String,                       // required
    pub site_prefix: String,                     // validated against ^[a-z]{1,6}$
    pub mode: SubstrateMode,                     // solo | copilot | crew

    /// Overwrite `derrick.yaml` / `.derrick/` if present.
    pub force: bool,

    /// Skip D29 host-hook installation entirely.
    pub no_hooks: bool,

    /// Append a derrick block to existing AGENTS.md and
    /// CLAUDE.md instead of just referencing them.
    pub append_agents_md: bool,

    /// Run an LLM pass over existing docs to draft a
    /// constitution.md stub (D4). The draft lands with a
    /// banner; `plan` step refuses until banner is removed.
    pub constitution_from_docs: bool,
}
```

### `AdoptionPlan`

```rust
#[derive(Clone, Debug)]
pub struct AdoptionPlan {
    /// Files derrick will write. Each entry includes
    /// the destination path, the content (rendered from
    /// templates with site / prefix / mode substituted),
    /// and the rationale (so `derrick init --dry-run` can
    /// explain the choices).
    pub writes: Vec<PlannedWrite>,

    /// Files derrick will reference from `derrick.yaml`
    /// guardrails / agents config but NOT touch.
    pub references: Vec<PlannedReference>,

    /// Non-fatal observations the user should see
    /// (e.g. "your CLAUDE.md is shorter than 5 lines,
    /// consider expanding it before relying on it").
    pub warnings: Vec<String>,

    /// Fatal preconditions. If non-empty, `apply` refuses
    /// without `force`.
    pub blockers: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct PlannedWrite {
    pub path: PathBuf,
    pub content: String,
    pub mode: WriteMode,           // Create | Append | MergeJson
    pub rationale: String,
}

#[derive(Clone, Debug)]
pub struct PlannedReference {
    pub path: PathBuf,
    pub as_field: String,          // e.g. "guardrails.constitution_path"
    pub rationale: String,
}
```

### Detection → plan rules

Given a `DetectionReport`, `propose` builds the plan by
applying these rules in order (first match wins per file):

| Detected | Plan |
|---|---|
| `existing_derrick_yaml` and `!force` | Add to `blockers`: *"derrick.yaml already exists at <path>; pass --force to overwrite or use `derrick adopt --merge` (future ticket)"*. |
| `git_repo` is false | Add to `blockers`: *"derrick init must run inside a git repo"*. |
| `agents_md` present | Add to `references`: `guardrails.agents_md = <path>`. If `opts.append_agents_md` also adds a planned append to that file with a `<!-- derrick:start -->` / `<!-- derrick:end -->` block referencing derrick. |
| `claude_md` present | Same: reference + optional append, controlled by `append_agents_md`. |
| `constitution` present | Add to `references`: `guardrails.constitution_path = <path>`. |
| `constitution` absent and `opts.mode != solo` | Add to `writes`: a minimal `.specify/memory/constitution.md` stub with a banner *"This is a derrick-init stub. Run `/speckit.constitution` to author."*. Plan also adds a `BannerCheck` to `tools.speckit.pre_plan_hooks` so the `plan` step refuses until the banner is gone (D4 enforcement). |
| `claude_settings` present and `!opts.no_hooks` | Add to `writes` with `WriteMode::MergeJson`: D29 `PreToolUse` + `PostToolUse` entries inserted **before** existing ones, comment-marker `// derrick:scrub` for later identification. If existing entries already cover the same tool, add to `warnings` and require `--force` to override per the §5.6 brownfield table. |
| `claude_settings` absent and `!opts.no_hooks` | Add to `writes` with `WriteMode::Create`: minimal `.claude/settings.json` with derrick's hooks only. |
| Equivalent paths for `.codex/instructions.md` and `.codex/settings.json` | Same pattern. Skip if codex CLI not available unless mode=copilot/crew. |
| `claude_agents` contains agent names that collide with derrick's `foreman` / `assay-reviewer` / `hand-default` | Skip those — derrick doesn't overwrite existing agents. Add to `warnings`. Other derrick agent names ship cleanly. |
| `claude_commands` contains `add-feature.md` etc. | Refuse with `blockers` unless `--force`. |
| `opts.constitution_from_docs == true` | Inject an extra `writes` step (constitution.md.draft with banner per D4) and add a warning about the unreviewed prose. **Network-touching:** invokes the configured `proposer` role to draft from `readme + contributing + adrs_dir` content. Bounded by 30s timeout. |
| `default_branch` repo allows squash-merge only | Add a `warnings` entry per D21 (squash-merge stance) — doesn't block, just informs. |

### Apply semantics

`apply` performs writes in a fixed order:

1. Verify blockers list is empty (or `force` is set).
2. Pre-flight: for each `PlannedWrite`, check that the
   target file matches expectations (e.g. for `MergeJson`,
   that the existing JSON is parseable). On mismatch, abort
   without writing anything.
3. Materialise writes one by one, each via temp-file-and-
   rename. If any single write fails, the rest are
   attempted (best-effort recovery) and the outcome
   records `failed: Vec<(PathBuf, AdoptError)>`. We don't
   roll back successful writes — they're real files; the
   user can `git status` to see what changed.
4. Open the substrate (`NativeSubstrate::open`) with the
   chosen site — this runs migrations on a fresh DB or
   verifies the existing one (per T007).
5. Write a small adoption record to
   `.derrick/state.json#adoption`: timestamp, opts used,
   files written, files referenced. Used by future
   `derrick adopt --reverse` (out of scope here).

### CLI wiring (replaces T008's bare-init refusal)

`derrick init` (bare, no `--greenfield`) is the **primary
brownfield path**. T008's stub message is removed; the
command now:

1. Calls `Adopter::detect`.
2. Prompts interactively (or accepts flags) for the missing
   options: `--site`, `--prefix`, `--mode`, `--append-agents-md`,
   `--constitution-from-docs`, `--no-hooks`.
3. Calls `Adopter::propose`.
4. Prints the plan (writes + references + warnings + blockers).
   Asks the user: *"continue? [y/N]"* unless `--yes`.
5. On confirm, calls `Adopter::apply` and prints the outcome.

`derrick init --greenfield` continues to work exactly as T008
shipped — no detection, just writes the canonical template.

### Hook content (D29)

`.claude/settings.json` `PreToolUse` entry derrick installs:

```json
{
  "matcher": "Bash",
  "hooks": [
    {
      "type": "command",
      "command": "derrick scrub --tool bash <(cat)",
      "comment": "derrick:scrub"
    }
  ]
}
```

`PostToolUse` is the same shape but with `derrick caveman
--intensity lite <(cat)`. Both stream stdin → stdout (the
hooks are pipe-style per Claude Code's hook spec). The
marker `derrick:scrub` lets `derrick adopt --reverse`
identify derrick-installed entries for cleanup.

For tools the user doesn't run via Bash (Read, Glob, Grep,
etc.) the hooks fire on the matcher pattern Claude Code
documents. We start with `Bash` only and extend in a
follow-up; v1 is "scrub the noisy ones".

### Dependencies

```toml
[dependencies]
derrick-config = { path = "../derrick-config" }
derrick-substrate = { path = "../derrick-substrate" }
derrick-substrate-native = { path = "../derrick-substrate-native" }
derrick-tools = { path = "../derrick-tools" }    # for speckit/host detection only
async-trait = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
serde_yaml = { workspace = true }
thiserror = { workspace = true }
tokio = { workspace = true }
tracing = { workspace = true }
which = { workspace = true }
chrono = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
```

No new top-level workspace deps.

### Tests

All tempfile-based with synthetic brownfield layouts. No
network in unit tests; the `--constitution-from-docs` LLM
path is exercised via a mocked `Model` from `derrick-models`
(same pattern T010 uses).

**Detection:**

- `detect_in_empty_repo_finds_nothing`.
- `detect_finds_agents_md_at_root`.
- `detect_finds_claude_md_at_root`.
- `detect_finds_claude_agents_and_skills`.
- `detect_finds_specify_dir_and_constitution`.
- `detect_finds_codex_instructions`.
- `detect_finds_github_copilot_instructions`.
- `detect_finds_readme_contributing_adrs`.
- `detect_reports_existing_derrick_yaml`.
- `detect_runs_in_under_50ms_on_a_realistic_repo` —
  perf guardrail, snapshot a synthetic ~200-file repo.

**Proposal:**

- `propose_on_clean_repo_writes_full_skeleton`.
- `propose_with_existing_agents_md_references_it`.
- `propose_appends_to_agents_md_when_flag_set`.
- `propose_skips_colliding_agents`.
- `propose_blocks_on_existing_yaml_without_force`.
- `propose_allows_existing_yaml_with_force`.
- `propose_emits_squash_only_warning_when_repo_default_is_squash`.
- `propose_skips_hooks_when_no_hooks_flag`.
- `propose_inserts_derrick_hooks_before_existing_ones`.
- `propose_blocks_on_command_name_collision_without_force`.
- `propose_emits_banner_check_when_constitution_drafted`.

**Apply:**

- `apply_writes_only_planned_files` — assert no other
  files are touched.
- `apply_uses_atomic_write` — kill mid-write via injected
  panic; on-disk file is either pristine or fully new.
- `apply_preserves_existing_unrelated_files`.
- `apply_records_adoption_to_state_json`.
- `apply_opens_substrate_after_writes` — migrations run.

**Constitution-from-docs (D4):**

- `constitution_draft_includes_banner`.
- `plan_step_refuses_against_unreviewed_banner` — exercised
  by feeding the drafted constitution into a mocked
  `derrick-flow` pipeline run; assert it halts with the
  banner message.

**End-to-end smoke (in tests/integration):**

- `bare_derrick_init_against_derrick_repo` — uses the
  derrick repo's own brownfield layout as the fixture (the
  test repo is a temp clone) and verifies a full bare-init
  produces a working derrick.yaml + .derrick/derrick.db
  with the existing AGENTS.md and CLAUDE.md referenced.

**Coverage target**: 80% (mixed-shape crate with file-system
operations; 80% is the gate).

## Out of scope

- `derrick adopt --reverse` to undo what adopt wrote.
  Future ticket; the adoption record in state.json captures
  what's needed.
- `derrick adopt --merge` for re-running adoption on an
  already-initialised repo. Future.
- Hook matchers beyond `Bash` — extend in a follow-up once
  we have telemetry on which matchers benefit.
- A `--reverse` for the constitution draft (re-roll). User
  edits the banner away when they're satisfied.
- Codex/Copilot hook installation beyond writing the
  `.codex/instructions.md` reference file. Real Codex hooks
  in Codex CLI land in a later ticket if Codex grows hook
  support.

## D31/D32/D33 compliance

This ticket does not change any ticket state and writes no
substrate rows beyond opening the DB. It's pre-substrate-
lifecycle. So §8.6's state-integrity rules don't apply
directly — but the adoption record at `.derrick/state.json
#adoption` follows the same append-only principle: every
re-run of adopt appends a new record rather than overwriting,
so the user can `derrick adopt --history` later.

## Acceptance

- [ ] `cargo check -p derrick-adopt` passes.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] `cargo test -p derrick-adopt` passes; 3× stress green.
- [ ] `cargo llvm-cov -p derrick-adopt --fail-under-lines 80`.
- [ ] Workspace `cargo llvm-cov --fail-under-lines 80` still passes.
- [ ] No `unwrap`/`expect`/`panic` in non-test code.
- [ ] No gastown vocabulary.
- [ ] **End-to-end smoke documented in the commit body**:
      `derrick init` (bare) against a temp clone of the
      derrick repo produces a valid `derrick.yaml` +
      `.derrick/derrick.db` with existing AGENTS.md /
      CLAUDE.md referenced, no existing files touched.

## Reviewer notes (Codex)

Pre-implementation review. Focus on:
- Is the detection set complete vs DESIGN.md §5.6 brownfield
  table? Anything missing?
- Is the rule ordering in `propose` deterministic enough?
- Is the apply step's atomic-write story sufficient given
  multi-file writes (vs single-file in derrick-memory)?
- The `--constitution-from-docs` LLM call is the only
  network-touching path. Is the bounded-timeout +
  banner-on-output approach the right shape?
- D31/D32/D33 don't directly apply, but is anything in
  adopt's lifecycle implicitly violating them?

## Implementer notes (Copilot)

Stay in `crates/derrick-adopt/` plus a small edit to
`crates/derrick-cli/src/commands/init.rs` to swap the bare-
init refusal for a real `Adopter` call. Templates for the
hook JSON and the constitution stub go in
`templates/hooks/` and `templates/constitution.md.in` at
workspace root, alongside the existing
`templates/derrick.yaml.in`.

The interactive prompt path needs to be testable —
extract the I/O behind a trait so tests can drive it with
canned inputs.
