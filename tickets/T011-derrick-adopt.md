# T011 — `derrick-adopt` brownfield init + D29 host hooks

**Specialist owner**: `flow-engineer` (opus) — owns adopt + init flow per AGENTS.md routing
**Crate**: `crates/derrick-adopt`
**Depends on**: `derrick-config`, `derrick-substrate`, `derrick-substrate-native`, `derrick-tools` (for speckit/claude/codex detection), `derrick-models` (for the `--constitution-from-docs` LLM draft path via the `proposer` role), `derrick-cli` (to wire bare `derrick init`)
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

    /// (Only when `opts.constitution == ConstitutionMode::FromDocs`.)
    /// Run the LLM draft over readme + contributing + ADRs.
    /// 30s timeout. Returns the drafted constitution body
    /// **with the canonical banner prepended**, ready to be
    /// passed into `propose` as `drafted_constitution`. This
    /// is the **only** network-touching method on `Adopter`.
    pub async fn draft_constitution(
        &self,
        report: &DetectionReport,
        opts: &AdoptOptions,
    ) -> Result<String, AdoptError>;

    /// Produce a plan describing what `apply` would write,
    /// what it would reference, and what it would warn about.
    /// Pure function of the inputs — no I/O, no network.
    /// `drafted_constitution` is `Some(text)` only when the
    /// caller has already run `draft_constitution`; otherwise
    /// `None`. If `opts.constitution == FromDocs` but
    /// `drafted_constitution` is `None`, propose returns an
    /// error rather than silently skipping the planned write.
    pub fn propose(
        &self,
        detection: &DetectionReport,
        opts: &AdoptOptions,
        drafted_constitution: Option<&str>,
    ) -> Result<AdoptionPlan, AdoptError>;

    /// Apply the plan. Writes the files in `plan.writes`;
    /// in addition, owns and may touch a small set of
    /// derrick-bookkeeping paths regardless of the plan:
    /// - `.derrick/state.json` (adoption history append)
    /// - `.derrick/.adopt-stage-<uuid>/` (transient staging
    ///   dir, cleaned up by D32's reconciliation pass)
    /// - `.derrick/derrick.db` and friends (substrate open
    ///   side-effect; covered by T007)
    ///
    /// Files **outside** that set and not in `plan.writes`
    /// are guaranteed untouched. Returns the outcome with
    /// every path written (including derrick-bookkeeping
    /// paths) so callers can `git status`-cleanly review.
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
    pub codex_dir: Option<PathBuf>,              // .codex/
    pub codex_instructions: Option<PathBuf>,     // .codex/instructions.md
    pub codex_config: Option<PathBuf>,           // .codex/config.toml or settings.json (if present)
    pub github_copilot_instructions: Option<PathBuf>,
                                                 // .github/copilot-instructions.md
    pub codeowners: Option<PathBuf>,             // CODEOWNERS or .github/CODEOWNERS

    // Speckit footprint
    pub specify_dir: Option<PathBuf>,            // .specify/
    pub specify_extensions_derrick: Option<PathBuf>,
                                                 // .specify/extensions/derrick/ (existing reuse state)
    pub constitution: Option<PathBuf>,           // first match of the canon list below

    /// Files matched as constitution-like docs in priority
    /// order. `constitution` above is the first entry (if any).
    /// Canon search list (in this priority order):
    /// `.specify/memory/constitution.md`, `CONSTITUTION.md`,
    /// `PRINCIPLES.md`, `STYLE.md`, `RULES.md`,
    /// `CONTRIBUTING.md`, `docs/constitution.md`,
    /// `docs/principles.md`. `CONTRIBUTING.md` is included
    /// per DESIGN.md §5.6 (which lists it explicitly as a
    /// constitution-like doc derrick may reference).
    pub constitution_candidates: Vec<PathBuf>,

    // Existing derrick state (refuse if --force not set)
    pub existing_derrick_yaml: Option<PathBuf>,
    pub existing_derrick_dir: Option<PathBuf>,

    // Tool availability (for speckit detect-then-defer per D2)
    pub speckit_cli_available: bool,             // `which specify`
    pub claude_cli_available: bool,
    pub codex_cli_available: bool,
    pub gh_cli_available: bool,                  // for D21 squash check via gh

    // Docs we'd reference as constitution-drafting inputs
    pub readme: Option<PathBuf>,
    pub contributing: Option<PathBuf>,
    pub adrs_dir: Option<PathBuf>,               // docs/adrs/ or similar

    /// Tracker prefixes scraped from existing AGENTS.md /
    /// CLAUDE.md (e.g. "LIN-", "JIRA-", "BD-"). Detection
    /// only — adoption of external trackers is out of scope
    /// for v1 (DESIGN.md §5.6) but we record what we saw so
    /// future tickets can wire it.
    pub tracker_prefixes: Vec<String>,
}
```

Detection is **pure read** — no writes, no network, no
subprocess calls beyond `which`-checks. Fast enough to run
on every `derrick doctor` invocation too.

Notably **no** `default_branch` here: D21's squash-merge
warning requires `gh api repos/{owner}/{name}` per
DESIGN.md §8.5, which is a network call. That check belongs
to `derrick doctor` (already specified in T008) and not to
`derrick-adopt`. Removed.

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

    /// Constitution handling. Default is `Reference`:
    /// reference whatever constitution-like doc detection
    /// found; otherwise produce no constitution write.
    /// `Stub` opts in to writing a minimal banner stub.
    /// `FromDocs` opts in to the LLM draft path (D4).
    /// The three variants are **mutually exclusive**, and
    /// both `Stub` and `FromDocs` are no-ops when a
    /// constitution-like doc already exists — they refuse
    /// rather than silently double-writing.
    pub constitution: ConstitutionMode,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum ConstitutionMode {
    /// Default: reference existing if found; write nothing
    /// otherwise. Matches DESIGN.md §5.6 "no constitution"
    /// row: opt-in only.
    #[default]
    Reference,
    /// `--constitution-stub`: write the minimal banner stub
    /// at `.specify/memory/constitution.md`. Refused (with
    /// `blockers` entry) when a constitution-like doc was
    /// already detected.
    Stub,
    /// `--constitution-from-docs`: run an LLM draft pass
    /// over README + CONTRIBUTING + ADRs. Output lands with
    /// a banner; `plan` step refuses until banner is
    /// removed (D4). Refused (with `blockers` entry) when a
    /// constitution-like doc was already detected.
    FromDocs,
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

`propose` is **pure** — no I/O, no network, no LLM. It
takes a `DetectionReport` + `AdoptOptions` and returns an
`AdoptionPlan`. The `--constitution-from-docs` LLM draft
happens in a separate `draft_constitution` step called
before `propose` (see "Two-phase flow" below); its output
is fed back in as an additional input.

The plan builder runs these rules in fixed declared order
over sorted artifact lists. Every input has its sort key
documented so the output is byte-deterministic for a given
input.

#### Phase A — blockers (terminate plan generation if any fire and `!force`)

1. `!report.git_repo` → blocker: *"derrick init must run inside a git repo"*.
2. `report.existing_derrick_yaml.is_some()` → blocker: *"derrick.yaml already exists at <path>; pass --force or use `derrick adopt --merge` (future)"*.
3. `report.claude_commands` contains a name in `{add-feature.md, derrick-status.md, derrick-doctor.md, derrick-resume.md}` → blocker: *"existing Claude command <name> would be overwritten"*.
4. `opts.constitution != Reference` AND `report.constitution.is_some()` → blocker: *"a constitution-like doc already exists at <path>; constitution flags refuse to overwrite"*.

#### Phase B — references (read-only entries in derrick.yaml)

5. `report.agents_md.is_some()` → reference `guardrails.agents_md = <path>`.
6. `report.claude_md.is_some()` → reference `guardrails.claude_md = <path>`.
7. `report.constitution.is_some()` → reference `guardrails.constitution_path = <path>`.
8. `report.codeowners.is_some()` → reference `guardrails.codeowners = <path>`.

#### Phase C — writes (each in fixed precedence order)

9. Always: `derrick.yaml` (rendered from `templates/derrick.yaml.in` with site/prefix/mode substituted).
10. Always: `.derrick/.gitignore` (gitignores `runs/`, `state.json`, `derrick.db*`, `worktrees/`).
11. Optional append (`opts.append_agents_md`): a `<!-- derrick:start -->` / `<!-- derrick:end -->` block appended to existing `AGENTS.md` and `CLAUDE.md`. Idempotent: re-running with the same opts produces no change because the block is detected and replaced rather than re-appended.
12. `opts.constitution == ConstitutionMode::Stub` AND no existing constitution AND `!report.speckit_cli_available` → write `.specify/memory/constitution.md` with the canonical banner stub. Adds a runtime check: the `plan` pipeline step refuses to run until the banner is removed (per D4 enforcement). If speckit *is* available, `--constitution-stub` instead refuses with a `blockers` entry pointing at `/speckit.constitution` — derrick defers to speckit as the constitution owner per D2/D3 (detect-then-defer).
13. `opts.constitution == ConstitutionMode::FromDocs` AND no existing constitution AND `!report.speckit_cli_available` → write the prior `draft_constitution` output (passed in as a separate input) to `.specify/memory/constitution.md`, banner intact. With speckit available, `--constitution-from-docs` is similarly refused with a pointer to `/speckit.constitution`. (Detect-then-defer applies to both opt-in modes.)
14. `.specify/extensions/derrick/scripts/tasks-to-tickets.sh` (always; if `.specify/extensions/derrick/` already exists, **merge by file** rather than overwrite — only files derrick owns get rewritten).
15. `.claude/commands/add-feature.md`, `derrick-status.md`, `derrick-doctor.md`, `derrick-resume.md` (always; collision was blocked in Phase A).
16. `.claude/agents/<name>.md` for each derrick agent. Skip any name colliding with `report.claude_agents` — add a `warnings` entry naming the skipped agent.
17. `.codex/instructions.md` (always — not gated on codex CLI presence, since the user may install codex after init): a static reference to the constitution path + `derrick.yaml` so codex picks up project context when running in `host: codex` pipeline steps. **No `.codex/` hook installation** per D34. If `.codex/instructions.md` already exists, append a derrick block (matching the `--append-agents-md` pattern in rule 11) so the user's own content survives.
18. Hooks (skipped entirely if `opts.no_hooks`): see "Hook representation" below.

#### Phase D — warnings (non-fatal observations)

18. If `report.specify_extensions_derrick.is_some()` → warn: *"existing `.specify/extensions/derrick/` will be merged file-by-file; review the diff before committing."*
19. If `report.tracker_prefixes` is non-empty → warn: *"detected tracker prefixes <list>; v1 only ships the native substrate, no external-tracker adoption."*
20. If `opts.constitution == ConstitutionMode::FromDocs` → warn: *"the constitution draft is unreviewed LLM prose; `plan` will refuse to run until you remove the banner."*

**Squash-merge (D21) is NOT detected here** — that requires
`gh api` which is a network call and `derrick doctor`
already covers it.

### Two-phase flow

The `--constitution-from-docs` path requires a network call.
We separate it from `propose` to keep `propose` pure:

```text
detect (pure)
   ↓
[--constitution-from-docs only:] draft_constitution
   (LLM call, bounded 30s timeout, returns the drafted text)
   ↓
propose (pure; takes report + opts + optional drafted text)
   ↓
[interactive confirm]
   ↓
apply (I/O, idempotent staging + commit)
```

`Adopter::draft_constitution(report, opts) -> Result<String,
AdoptError>` is the LLM step. It invokes the configured
`proposer` role via `derrick-models`, passing the contents
of `readme`, `contributing`, and any `adrs_dir` files as a
structured prompt. Bounded by 30s timeout. Output is
prepended with the canonical banner *"DERRICK-DRAFT — review,
edit, remove this banner before running plan. Generated
from <docs> on <date> by <model>."*

### Apply semantics

`apply` is **stage-then-commit** with a best-effort
atomicity goal across multiple files:

1. **Pre-flight.** Verify `blockers` is empty or `force` is
   set. For each `PlannedWrite`, validate the target file
   matches expectations (`MergeJson` requires the existing
   JSON to be parseable). Substrate open is attempted in a
   dry-run mode (open + close immediately on a temp shadow
   of the planned `.derrick/derrick.db` path) to confirm
   it'd work. Any pre-flight failure → return error,
   nothing on disk has changed.

2. **Stage.** Render every output to a temp directory
   (`.derrick/.adopt-stage-<uuid>/`). All file contents
   exist as real bytes on disk before any production path
   is touched. Pre-flight is repeated against the staged
   `derrick.yaml` (parse + validate) so a misrender is
   caught before commit.

3. **Commit.** For each staged file in plan order, rename
   into the production path. Renames are atomic on the
   same filesystem. If any rename fails:
   - Stop committing remaining files.
   - Best-effort revert: any files already committed during
     this run are listed in the outcome's `partial_failure`
     field; the user is **explicitly told** which paths to
     `git checkout --` to revert. We don't attempt
     auto-revert because the user might have intentionally
     scheduled non-derrick changes alongside the init.
   - Write a `.derrick/state.json#last_partial_adoption`
     record with the staged dir path so a follow-up
     `derrick adopt --resume <id>` (future ticket) can
     pick up.

4. **Substrate open.** With all writes successful, open
   `NativeSubstrate` with the chosen site (real this time,
   not the dry-run shadow). Migrations run.

5. **Record.** Append an immutable entry to
   `.derrick/state.json#adoption_history` (note: append,
   not overwrite — every re-run of adopt adds a new
   record so `derrick adopt --history` works). Entry
   contains timestamp, opts, files written, files
   referenced.

Failure modes that leave on-disk state:

- Pre-flight failure → no state change. Safe to retry.
- Stage failure → temp dir leaked, no production change.
  Cleanup happens on next `derrick run` startup via D32's
  abandoned-stage prune (extends D32's worktree
  reconciliation to also clean `.adopt-stage-*`).
- Commit failure → `partial_failure` outcome + explicit
  paths to revert. The user must run `git checkout --` on
  the listed paths before retrying. Future
  `derrick adopt --resume` will automate.

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

Coverage in v1: **all standard tool boundaries** per D29.
That's the matcher set `Bash | Read | Write | Edit | Glob |
Grep`. Each gets a paired PreToolUse + PostToolUse entry.

JSON has no comments, so the derrick marker is a literal
field `"description"` (Claude Code preserves unknown
fields). One marker shape, one place. We pick
`"description": "derrick:scrub"` for PreToolUse and
`"description": "derrick:caveman"` for PostToolUse.

Example PreToolUse entry derrick installs:

```json
{
  "matcher": "Bash",
  "hooks": [
    {
      "type": "command",
      "command": "derrick scrub --tool bash",
      "description": "derrick:scrub"
    }
  ]
}
```

PostToolUse mirrors with `derrick caveman --intensity lite`
and `"description": "derrick:caveman"`.

Each derrick-installed hook command **reads from stdin
and writes to stdout** — that's the Claude Code hook
contract. `derrick scrub --tool <name>` already supports
this (T009: subprocess scrubbing was always streaming).
`derrick caveman --intensity lite` ditto.

`derrick adopt --reverse` (future ticket) identifies
derrick-installed entries by the `"description"` field
matching `"derrick:scrub" | "derrick:caveman"`, and
removes them while preserving the surrounding JSON
structure exactly.

#### Hook conflict semantics (D29 brownfield safety)

A "conflict on a matcher" means: for a given
`matcher` value (e.g. `"Bash"`) inside the `PreToolUse`
or `PostToolUse` array, the existing settings.json
already has at least one entry that derrick did not
install (i.e. its `"description"` field is **not** the
`derrick:scrub` / `derrick:caveman` marker, or the field
is absent).

**Detection happens in `propose`.** The plan builder reads
`.claude/settings.json` once, scans its
`PreToolUse`/`PostToolUse` arrays, and classifies each
existing entry per-(stage, matcher) pair:

| Existing entry | Plan behaviour |
|---|---|
| Marked derrick (`description == "derrick:scrub"` etc.) | **Idempotent replacement.** The planned write contains the current derrick template at that slot; if the existing entry is byte-equal, no change. If newer, the existing entry is overwritten in place (no array-position shuffle). This is what makes re-running adopt safe. |
| Unmarked, different matcher than derrick's set | **Coexists.** Derrick's new entries are prepended to the front of the array; unmarked entries follow. |
| Unmarked, same matcher as one derrick wants to install | **Conflict.** Without `--force`, the plan adds a `blockers` entry: *"`.claude/settings.json` PreToolUse already has an entry on matcher `<name>`; pass --force to prepend derrick's hook before it, or remove the conflicting entry first."* With `--force`, derrick's entry is prepended before the existing one (the existing one is not deleted or modified), and a `warnings` entry records that a force-merge happened. |
| Existing settings.json file is corrupt JSON | Blocker recorded by `propose` (during the same parse pass that does hook classification). Surfaces in `derrick init --dry-run` and the confirmation prompt before `apply` is ever called. `apply` re-validates as a defensive second pass, but the user's first signal is at proposal time. |

`--force` only relaxes the conflict refusal; it never deletes
unmarked entries. The user retains control over their existing
configuration.

`apply` then writes the rendered `settings.json` (full JSON
object) via the same stage-then-commit path as other files.
Atomic rename guarantees the file is never seen as half-written
by Claude Code (which reads it on session start).

**Codex hook installation is deferred to a follow-up
ticket.** Codex's hook surface is meaningfully thinner than
Claude Code's at the time of writing (no documented
PreToolUse / PostToolUse equivalents derrick can rely on),
and D29's Codex path is described as best-effort.

T011 writes `.codex/instructions.md` only — a static file
referencing the constitution + derrick.yaml. No
`.codex/settings.toml` mutation, no hook installation
through `.codex/`. When Codex grows a stable hook
mechanism, a follow-up ticket extends `derrick-adopt` to
install the equivalent there.

Add to the proposal `warnings` set: *"Codex host hook
installation is deferred; Codex tool I/O is not scrubbed in
v1. See <follow-up ticket id when filed>."* — so users
running `mode: copilot` or `mode: crew` with codex hosts
know the gap is real.

### Dependencies

```toml
[dependencies]
derrick-config = { path = "../derrick-config" }
derrick-substrate = { path = "../derrick-substrate" }
derrick-substrate-native = { path = "../derrick-substrate-native" }
derrick-tools = { path = "../derrick-tools" }    # for speckit/host detection only
derrick-models = { path = "../derrick-models" }  # for --constitution-from-docs draft
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

**Proposal (pure-function tests, deterministic byte
output for same inputs):**

- `propose_on_clean_repo_writes_full_skeleton`.
- `propose_with_existing_agents_md_references_it`.
- `propose_appends_to_agents_md_when_flag_set`.
- `propose_skips_colliding_agents`.
- `propose_blocks_on_existing_yaml_without_force`.
- `propose_allows_existing_yaml_with_force`.
- `propose_skips_hooks_when_no_hooks_flag`.
- `propose_inserts_derrick_hooks_before_existing_ones`.
- `propose_blocks_on_command_name_collision_without_force`.
- `propose_is_deterministic` — same inputs → byte-identical
  output across 10 runs.

**Constitution handling (four-way matrix):**

- `constitution_mode_reference_with_existing_doc_only_references`.
- `constitution_mode_reference_without_doc_writes_nothing`.
- `constitution_mode_stub_with_existing_doc_blocks` —
  blockers list includes the explicit message.
- `constitution_mode_stub_without_doc_writes_banner_stub`.
- `constitution_mode_fromdocs_with_existing_doc_blocks`.
- `constitution_mode_fromdocs_without_doc_writes_drafted_text` —
  uses a mocked `Model` from `derrick-models` to supply
  the draft content.
- `constitution_with_banner_makes_plan_step_refuse` —
  exercises the runtime check via a mocked
  `derrick-flow` pipeline.

**Hook coverage:**

- `hooks_installed_for_all_matchers` — Bash, Read, Write,
  Edit, Glob, Grep.
- `hooks_use_description_marker_for_identification` —
  asserts `"description": "derrick:scrub"` /
  `"derrick:caveman"`.
- `hooks_merge_into_existing_settings_json_preserves_unknown_fields`.
- `hooks_prepended_before_existing_entries`.

**Hook conflict semantics:**

- `hook_replacement_is_idempotent_for_derrick_marked_entries` —
  pre-existing settings has a derrick-marked entry with stale
  content; re-running adopt updates it in place with no
  array-position shuffle and produces an empty diff if
  already current.
- `hook_unmarked_different_matcher_coexists` — user has
  their own hook on `Read`; derrick installs `Bash`;
  result has both, derrick's first.
- `hook_unmarked_same_matcher_blocks_without_force` —
  blockers list contains the explicit message; no writes.
- `hook_unmarked_same_matcher_force_merges_with_warning` —
  with `--force`, derrick's entry is prepended, the
  unmarked entry survives unchanged, warnings list
  records the force-merge.
- `hook_corrupt_settings_json_blocks_during_proposal`.

**Apply (stage-then-commit):**

- `apply_writes_only_planned_files` — assert no other
  files are touched.
- `apply_stages_before_committing` — fail at the stage
  step via permission-denied on the stage dir; assert no
  production paths changed.
- `apply_partial_commit_surfaces_files_to_revert` — inject
  a rename failure on the third commit; assert the
  outcome's `partial_failure` lists the first two
  committed paths and the recovery message.
- `apply_pre_flight_dry_run_substrate_open_fails_aborts_cleanly` —
  inject a corrupt site DB scenario; pre-flight fails;
  nothing on disk changes.
- `apply_preserves_existing_unrelated_files`.
- `apply_appends_to_adoption_history_each_run` — run apply
  twice with `--force`; the history has two entries.
- `apply_opens_substrate_after_writes` — migrations run.
- `apply_resume_from_partial_failure_is_documented_followup`
  (placeholder test that exists but is `#[ignore]` until
  the resume path lands).

**Constitution-from-docs LLM path (D4):**

- `draft_constitution_returns_banner_prefixed_text` —
  mocked Model returns canned response; assert the
  banner is prepended.
- `draft_constitution_respects_30s_timeout` — mocked
  Model sleeps; assert timeout error.
- `draft_constitution_aggregates_readme_contributing_adrs` —
  inspect the passed-in prompt to confirm all three sources
  are included.

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

`propose` must be a pure function. Tests assert byte-
determinism for identical inputs (the
`propose_is_deterministic` test asserts this explicitly).
That means no `HashMap` iteration order leaks into the
plan output — use `BTreeMap` everywhere and sort
detection results in fixed order (alphabetic by path).

The interactive prompt path needs to be testable —
extract the I/O behind a trait so tests can drive it with
canned inputs.

D32 cleanup pass also needs to walk `.derrick/.adopt-
stage-*/` directories with `created_at` older than 24h.
This extends the cleanup logic specified in §8.6; add a
TODO comment in the cleanup code pointing here so the
T012 foreman implementer knows.
