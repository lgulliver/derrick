# T010 — `derrick-flow` minimal pipeline orchestrator

**Specialist owner**: `flow-engineer` (opus)
**Crate**: `crates/derrick-flow`
**Depends on**: `derrick-config`, `derrick-substrate`, `derrick-substrate-native`, `derrick-tools` (T009), `derrick-cli` (to wire `run add-feature`)
**Priority**: P0 — final dogfooding-bar item. After T010 lands and produces real artifacts, dogfooding switch fires (per morning decision question 2).

## Why

The whole stack so far — config, substrate, tools, models —
only matters if the orchestrator can actually walk a pipeline
end-to-end. T010 is that orchestrator for **solo mode in v1**:
read `derrick.yaml`, walk the configured pipeline, invoke
hosts via `derrick-tools`, capture per-step logs, write a run
manifest. Output of a successful run: real `spec.md`,
`plan.md`, `assay/verdict.md`, `tasks.md` artifacts on disk.

## Scope (v1, solo mode)

### Public API

```rust
//! Pipeline orchestrator. See DESIGN.md §5.3 and §10.

use derrick_config::Config;
use derrick_substrate::Substrate;
use derrick_tools::HostRegistry;

pub struct Runner {
    config: Config,
    substrate: std::sync::Arc<dyn Substrate>,
    hosts: HostRegistry,
    repo_root: PathBuf,
}

impl Runner {
    pub fn new(
        config: Config,
        substrate: std::sync::Arc<dyn Substrate>,
        hosts: HostRegistry,
        repo_root: PathBuf,
    ) -> Self;

    /// Execute the named pipeline. v1 supports `pipeline_id ==
    /// "add-feature"`. Other ids return RunError::UnknownPipeline.
    pub async fn run_pipeline(
        &self,
        pipeline_id: &str,
        input: PipelineInput,
    ) -> Result<RunOutcome, RunError>;

    /// Resume the most recent run (or a specific run id) from
    /// the named step. Steps already in the manifest as
    /// success are skipped; failed/missing ones re-execute.
    pub async fn resume(
        &self,
        run_id: Option<&str>,
        from_step: &str,
    ) -> Result<RunOutcome, RunError>;
}

#[derive(Clone, Debug, Default)]
pub struct PipelineInput {
    /// The /add-feature prompt. Required for add-feature.
    pub prompt: Option<String>,
    /// Step IDs the user explicitly skipped on this run.
    /// Only effective for steps with `skippable: true` in
    /// the yaml; otherwise rejected at config-time. Bespoke
    /// CLI aliases (`--no-clarify` etc.) populate this set
    /// in T008/T010's argparse code.
    pub skip: std::collections::BTreeSet<String>,
    /// Step IDs the user explicitly *re-enabled* despite
    /// the yaml's `default_skip: true`.
    pub unskip: std::collections::BTreeSet<String>,
    /// Halt after `tasks` step instead of proceeding to
    /// `bridge`/`foreman`. (In solo mode the latter are
    /// no-ops anyway; the flag matters once T011 wires the
    /// foreman.)
    pub dry_run: bool,
    /// Override run id (for tests). None = generate UTC
    /// timestamp.
    pub run_id: Option<String>,
}

#[derive(Clone, Debug)]
pub struct RunOutcome {
    pub run_id: String,
    pub status: RunStatus,
    /// Set after the `specify` step lands and records the
    /// feature_dir from `.specify/feature.json`.
    pub feature_dir: Option<PathBuf>,
    pub steps: Vec<StepRecord>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunStatus {
    Success,
    Failed,
    Halted,           // checkpoint declined, assay rejected, etc.
}

#[derive(Clone, Debug)]
pub struct StepRecord {
    pub id: String,
    pub status: StepStatus,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: chrono::DateTime<chrono::Utc>,
    pub log_path: PathBuf,
    pub artifacts: Vec<PathBuf>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StepStatus {
    Skipped,
    Success,
    Failed,
    Halted,
}

#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum RunError {
    #[error("unknown pipeline: {0}")]
    UnknownPipeline(String),
    #[error("missing prompt for pipeline {0}")]
    MissingPrompt(String),
    #[error("step {id} failed: {message}")]
    StepFailed { id: String, message: String },
    #[error("substrate error: {0}")]
    Substrate(#[from] derrick_substrate::SubstrateError),
    #[error("host error: {0}")]
    Host(#[from] derrick_tools::HostError),
    #[error("io error at {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("config error: {0}")]
    Config(String),
}
```

### Step runner taxonomy

D27 says **`role` XOR `runner`** are mutually exclusive on a
step. A `role`-bound step may *also* carry a `host:` to
specify which host CLI handles the invocation. Pure
`runner:` steps don't carry `host:`.

`derrick-config` already accepts these shapes; T010's runner
must execute every shape derrick.yaml can declare or reject
the config with a config-time error before running anything.

| Step shape | Dispatch in T010 |
|---|---|
| `role: <r>` + `host: claude` + `command:` | Resolve role → model (just for logging/telemetry; the host uses its own default per D30). `HostRegistry.get("claude").run(prompt=command_with_templates_resolved, cwd=repo_root, ...)`. Artifacts detected from `.specify/feature.json` updates. |
| `role: <r>` + `host: codex` + `command:` | Same shape, codex adapter. |
| `role: <r>` + `host: copilot` + `command:` | Same shape, copilot adapter with `CopilotToolPermission::AllowAll`. |
| `role: <r>` without `host:` | Treated as a **model invocation via `derrick-models`**: resolve role → model, build a `CompletionRequest`, call `Model::complete`. This is the path the assay reviewer takes (see "Assay step" below). |
| `runner: derrick` + `id: assay` | In-process assay orchestrator (composes the model-invocation path above for the reviewer call, plus the file IO around it). |
| `runner: derrick` + `id: bridge` | **No-op in T010** (solo mode). Logs "bridge skipped in solo mode" and records `Skipped`. T011 foreman implementation does the real bridge step. |
| `runner: derrick` + `id: foreman` | Same — no-op in T010. |
| `runner: human` + `prompt:` | Print the prompt to stdout, read y/n from stdin. `n` → step Halted, pipeline halts. |
| `runner: bash` + `command:` | Execute the (templated) shell command via `tokio::process` in the repo root; capture stdout/stderr to the step log. Non-zero exit → `StepFailed`. |
| `runner: claude \| codex \| copilot` | **Rejected at config-time** with `RunError::Config { message: "runner: <name> is not supported; use `host: <name>` with a role binding instead (see DESIGN.md §4 and D30)" }`. The schema permits these for forward-compat, but T010's contract is `host:` for CLI invocations and `role:`-only for model-trait invocations. |
| `runner: gt \| derrick-internal future runner` | Rejected with the same shape: *"unsupported runner: ..."*. |
| `parallel_group: <name>` on any step | Rejected at config-time: *"parallel_group is not supported in T010; sequential execution only. Remove the field or wait for T015 (§9.C.4)."* |
| `on_failure:` on `runner: copilot` steps | Deferred to T013 (copilot dispatch); reject at config-time. |
| `poll_interval:` | Same: T013. Reject at config-time. |

#### Skippable + default_skip semantics

`derrick-config` exposes generic `skippable: bool` and
`default_skip: bool` on every step. T010 honors them
uniformly:

- `skippable: false` (default) — the step is mandatory. The
  CLI rejects any attempt to skip it with
  *"step `<id>` is not skippable"*.
- `skippable: true` — the step can be omitted from a run.
  Triggers:
  - `default_skip: false` (default) — the step runs unless
    the user passes `--skip <id>` on `derrick run`.
  - `default_skip: true` — the step is omitted unless the
    user passes `--unskip <id>`.

The CLI's bespoke `--no-clarify`, `--no-checkpoint`,
`--no-assay` flags (already specified in T008) are
**convenience aliases** for `--skip clarify`,
`--skip checkpoint`, `--skip assay` respectively. They work
only if the matching step has `skippable: true`. T008 added
them as flags; T010 maps them through the generic skip
mechanism so future skippable steps can use the generic
path without bespoke flag work.

Skipped steps record `StepStatus::Skipped` in the manifest
with no log file written.

A config-time error reports unsupported shapes before any
side effects.

### Template variables

The runner templates `{{var}}` in each step's `command:`,
`inputs:`, etc. before dispatch. The canonical set per
DESIGN.md §4 (post-D27 cleanup) is:

- `{{prompt}}` — `input.prompt` (the /add-feature argument).
- `{{site_name}}` — `config.site.name`.
- `{{site_prefix}}` — `config.site.prefix`.
- `{{feature_dir}}` — set after `specify` lands; reads
  `.specify/feature.json` and uses the `feature_directory`
  field.
- `{{tasks_md}}` — `{{feature_dir}}/tasks.md`.
- `{{batch}}` — basename of `feature_dir`.
- `{{run_id}}` — the run id string.

Unknown placeholders are an error
(`RunError::Config { message: "unknown template var: {{...}}" }`).
Empty `{{feature_dir}}` (used before specify completes) is
also an error if a step references it.

The older `{{rig}}` variable mentioned in pre-D27 design
drafts is **not** supported — use `{{site_name}}` instead.
This is the canonical name per DESIGN.md §4 (just updated).

### Run manifest

Written to `.derrick/runs/<run_id>/manifest.json`. Format:

```json
{
  "run_id": "20260518T010101Z",
  "pipeline_id": "add-feature",
  "prompt": "build the webhook ingest endpoint",
  "flags": {
    "skip": ["clarify"],
    "unskip": [],
    "dry_run": false
  },
  "config_hash": "sha256:7a3b…",
  "started_at": "2026-05-18T01:01:01Z",
  "finished_at": "2026-05-18T01:08:42Z",
  "status": "success",
  "feature_dir": "specs/001-webhook-ingest",
  "steps": [
    {
      "id": "specify",
      "status": "success",
      "started_at": "...",
      "finished_at": "...",
      "log_path": "step-specify.log",
      "artifacts": ["specs/001-webhook-ingest/spec.md", ".specify/feature.json"]
    },
    ...
  ]
}
```

`flags.skip` is the set of step ids the user opted *out* of
on this run (whether via bespoke aliases or generic
`--skip <id>`). `flags.unskip` is the inverse set (step ids
re-enabled despite `default_skip: true`). `dry_run` is the
boolean flag. Together they fully describe the
run-time configuration and play back deterministically on
resume.

`config_hash` is a SHA-256 of the canonicalised
`derrick.yaml` bytes at run start; resume uses it to
detect drift (see below).

Per-step logs at `.derrick/runs/<run_id>/step-<id>.log`
contain the full captured stdout+stderr. Tests rely on the
manifest, not the log contents, for assertions.

### Assay step (in-process)

`assay` is `runner: derrick`. **Reviewer invocation goes
through `derrick-models`, not the host adapter** (per D30:
structured `CompletionRequest` shape is for providers; host
adapters get opaque prompt-as-argv only).

The runner:

1. Reads `spec.md`, `plan.md`, `constitution.md` from
   `feature_dir` and `.specify/memory/`.
2. Builds a `CompletionRequest`:
   - `system`: the assay prompt template asking for top-N
     risks, constitution contradictions, and a verdict
     (`accept | revise | reject`).
   - `cached_prefix`: the constitution + spec (stable
     across rounds).
   - `prompt`: the plan body + a one-line task statement.
3. Resolves `tools.assay.role` → model name → `Model`
   instance via `derrick_models::resolve_role`. Calls
   `Model::complete(request)`.
4. Parses the response for the verdict (a final line like
   `## Verdict\n<accept|revise|reject>`). On parse failure
   surfaces `RunError::StepFailed { id: "assay", message:
   "could not parse verdict from reviewer response" }`.
5. Writes `verdict.md` at `{{feature_dir}}/assay/verdict.md`
   with the model name, round number, and the full
   reviewer response.
6. On `revise`: build a **rebuttal** request that includes
   *only the reviewer's objections* (extracted as text
   between `## Suggested revisions` and the next H2), and
   sends it back to the plan step's host CLI with a
   prompt like *"The reviewer raised the following
   objections. Produce a delta to plan.md that addresses
   each. Do not rewrite the plan from scratch."*. Capture
   the new plan body. Then re-run assay. Bounded by
   `tools.assay.rounds`.
7. On `reject` (or `revise` past rounds): step Halted, the
   run halts with `RunStatus::Halted`. The verdict is
   preserved.

This mirrors DESIGN.md §7 exactly: the rebuttal is scoped
to the reviewer's objections only, asks for a delta, and is
bounded by rounds.

For T010 v1, **multi-reviewer assay is not implemented**.
`tools.assay.reviewers` is treated as the first entry only.
T015+ adds multi-reviewer reconciliation per §9.C.2.

### Resume semantics

`.derrick/runs/<run_id>/manifest.json` is the source of
truth. `resume(run_id, from_step)`:

1. Load manifest. Find `from_step`.
2. Compute the current `config_hash` and compare to the
   manifest's recorded value. **Mismatch** ⇒ refuse with
   `RunError::Config { message: "config has changed since
   this run started (manifest hash X, current Y);
   start a fresh run instead" }`. This prevents
   resume-after-yaml-edit from changing semantics
   silently.
3. Mark every step before `from_step` as already-completed;
   their artifacts are read off disk; their logs are not
   re-streamed.
4. Mark `from_step` and everything after as pending.
5. Re-run from `from_step` onward.

If `run_id` is None, resume the most recent run dir
(highest UTC-timestamp directory under `.derrick/runs/`).

The same `run_id` resumes into the same directory; manifest
is rewritten in place at the end of each step (so a
crash mid-step leaves the most recent successful step
recorded).

### Wiring into `derrick run add-feature`

T010 adds `derrick-flow` as a dep of `derrick-cli` and
replaces T008's `run` stub with a real call to
`Runner::run_pipeline("add-feature", ...)`. The argparse
needs these **additions** beyond what T008 shipped:

- `--skip <id>` (repeatable) — populates `PipelineInput.skip`.
- `--unskip <id>` (repeatable) — populates
  `PipelineInput.unskip`.
- `--dry-run` — sets `PipelineInput.dry_run`.
- `--run <id>` — target a specific run dir on
  `--resume-from`; default "most recent".

The existing T008 aliases (`--no-clarify`, `--no-checkpoint`,
`--no-assay`) stay as convenience: argparse expands each
into `--skip <id>` before constructing `PipelineInput`.
The bespoke flags remain visible in `--help` but document
themselves as "alias for `--skip <id>`".

The dispatch body is the substantive new work.

### Dependencies

```toml
[dependencies]
derrick-config = { path = "../derrick-config" }
derrick-substrate = { path = "../derrick-substrate" }
derrick-substrate-native = { path = "../derrick-substrate-native" }
derrick-tools = { path = "../derrick-tools" }
async-trait = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
serde_yaml = { workspace = true }
thiserror = { workspace = true }
tokio = { workspace = true }
tracing = { workspace = true }
chrono = { workspace = true }
uuid = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
tokio = { workspace = true, features = ["macros", "rt", "rt-multi-thread"] }
```

No new top-level workspace deps.

### Tests

All tempfile-based with mocked host CLIs (PATH-prefixed
shell scripts), no real claude/codex calls. Build a Runner
against an `Arc<NativeSubstrate>` opened on a tempfile.

**Happy path:**

- `add_feature_happy_path_writes_all_artifacts` — mock
  claude that writes a fake `spec.md`, `plan.md`, etc. when
  invoked; mock codex that returns an `accept` verdict.
  Assert the manifest records all 7 steps as success, the
  feature_dir is set, and every declared artifact exists
  on disk.
- `add_feature_writes_manifest_at_correct_path`.
- `add_feature_writes_per_step_logs`.

**Skip flags:**

- `no_clarify_marks_clarify_skipped`.
- `no_checkpoint_marks_checkpoint_skipped`.
- `no_assay_marks_assay_skipped`.
- `dry_run_halts_after_tasks_step`.

**Failure paths:**

- `host_failure_halts_pipeline_with_step_failed_status`.
- `unknown_pipeline_id_errors`.
- `missing_prompt_for_add_feature_errors`.
- `unknown_template_var_in_step_command_errors`.
- `feature_dir_template_before_specify_errors`.
- `parallel_group_in_yaml_rejected_at_config_time`.
- `on_failure_in_yaml_rejected_at_config_time`.
- `poll_interval_in_yaml_rejected_at_config_time`.
- `runner_claude_codex_copilot_rejected_at_config_time` —
  asserts the message points at `host:` as the right path.
- `rig_template_var_rejected` — guards against the
  vestigial vocabulary creeping back.

**Skippable mechanics:**

- `skip_id_on_nonskippable_step_errors`.
- `skip_id_on_skippable_step_marks_skipped`.
- `default_skip_true_omits_step_by_default`.
- `unskip_overrides_default_skip`.
- `no_clarify_flag_aliases_skip_clarify`.

**Assay paths:**

- `assay_accept_first_round_succeeds`.
- `assay_revise_then_accept_succeeds_after_replan` — also
  asserts the rebuttal request to the plan host carried
  only the objections-block, not the full original prompt.
- `assay_reject_halts_pipeline`.
- `assay_revise_past_rounds_halts_pipeline`.
- `assay_writes_verdict_md`.
- `assay_uses_derrick_models_path_not_host_adapter` —
  asserts (via mock provider counters) that the reviewer
  was invoked through `derrick-models::Model::complete`,
  not through the host adapter registry.
- `assay_unparsable_verdict_surfaces_step_failed`.

**Bash runner:**

- `bash_runner_executes_and_captures_output`.
- `bash_runner_nonzero_exit_fails_step`.
- `bash_runner_respects_templated_command`.

**Checkpoint paths:**

- `checkpoint_yes_continues`.
- `checkpoint_no_halts_pipeline`.

**Resume:**

- `resume_from_step_skips_earlier_steps`.
- `resume_default_uses_most_recent_run`.
- `resume_with_run_id_targets_specific_run`.
- `resume_preserves_existing_artifacts`.
- `resume_refuses_when_config_hash_mismatches` — edit
  derrick.yaml between start and resume; expect
  `RunError::Config` with "config has changed" message.

**Bridge / foreman in solo mode:**

- `bridge_step_is_no_op_in_solo_mode`.
- `foreman_step_is_no_op_in_solo_mode`.

**Coverage target**: 80% (this is a coordination crate with
lots of error branches that are intrinsically hard to
exercise without injecting host failures; 80% with the
named tests above is reasonable; workspace gate is still 80%).

## Out of scope

- Worktrees per §9.C.5 — T012 adds. T010 runs in the repo
  root.
- Foreman loop, ticket dispatch to hands. T011 + T013.
- Scrub/caveman wiring between steps. T015+.
- Memory hooks (init-time seeding, per-run digest, per-
  feature state, lessons). T016+.
- Multi-reviewer assay (§9.C.2). Single-reviewer only.
- Parallel pipeline steps (§9.C.4). Sequential only.
- Token telemetry / `derrick gain`. T017.
- crew/copilot mode dispatch. T011 + T013.

## Acceptance

- [ ] `cargo check -p derrick-flow` passes.
- [ ] `cargo check -p derrick-cli` passes (wiring still works).
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] `cargo test -p derrick-flow` passes; 3× stress green.
- [ ] `cargo llvm-cov -p derrick-flow --fail-under-lines 80`.
- [ ] Workspace `cargo llvm-cov --fail-under-lines 80` still passes.
- [ ] No `unwrap`/`expect`/`panic` in non-test code.
- [ ] Vocabulary clean.
- [ ] **End-to-end smoke** documented in the commit body:
      build the binary, run `derrick init --greenfield ...`
      in a tempdir, then run `derrick run add-feature
      --prompt "..."` against mock hosts; assert real files
      land on disk and the manifest is sane.

## Reviewer notes (Codex)

Pre-implementation review. Focus on:
- Is the step taxonomy complete? Anything in DESIGN.md §4
  the pipeline yaml supports that the runner doesn't?
- Are template variables enough? Should `{{site.name}}` or
  `{{site.prefix}}` be there too?
- Is the assay step's "reopen plan once" loop the right
  shape, or should it be a separate step type?
- Is the resume semantics deterministic given the
  manifest's incremental-write strategy?

## Implementer notes (Copilot)

Stay in `crates/derrick-flow/` plus the small edit to
`crates/derrick-cli/src/commands/run.rs` to swap the stub
for a real call. Mock hosts in tests use the same pattern
as T009: write shell scripts to a tempdir, prepend to PATH,
construct a `HostRegistry` that finds them. Keep step
execution sequential — no parallel; T010 is the boring
straight-line orchestrator.
