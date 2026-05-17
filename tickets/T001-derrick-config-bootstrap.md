# T001 — Bootstrap `derrick-config`

**Specialist owner**: `flow-engineer` (per `AGENTS.md` routing table)
**Crate**: `crates/derrick-config`
**Depends on**: nothing (this is the foundation; everything else depends on it)
**Priority**: P0 — every other crate consumes `derrick.yaml`

## Why

`derrick.yaml` is the single source of truth for a repo's
pipeline, models, roles, substrate backend, and parallelism
budgets (DESIGN.md §4). No other crate can be built without a
typed, validated representation of it.

## Scope

Implement the `derrick-config` crate. **Schema only and
validation only** — no I/O against speckit, no host CLI calls,
no substrate work. Everything else is out of scope.

### Public API

```rust
//! Load + validate `derrick.yaml`.

pub struct Config { /* opaque */ }

impl Config {
    /// Load a derrick.yaml from the given path.
    /// Returns a typed Config on success, or a ConfigError that
    /// names the offending line and suggests a fix when possible.
    pub fn load_from_path(path: &std::path::Path) -> Result<Self, ConfigError>;

    /// Load defaults baked into the binary. Used as the lowest layer
    /// in the merge chain (user → repo wins; user → ~/.derrick/config.yaml wins;
    /// fallback → built-in defaults).
    pub fn defaults() -> Self;

    /// Layered load: built-in defaults → ~/.derrick/config.yaml (if present)
    /// → repo derrick.yaml (if present). Each layer overrides the previous.
    pub fn load_layered(repo_root: &std::path::Path) -> Result<Self, ConfigError>;

    /// Validate without loading from disk. Called automatically by
    /// `load_from_path` and `load_layered`.
    pub fn validate(&self) -> Result<(), ConfigError>;

    /// Typed accessors for downstream crates.
    pub fn site(&self) -> &Site;
    pub fn models(&self) -> &ModelRegistry;
    pub fn roles(&self) -> &RoleBindings;
    pub fn tools(&self) -> &Tools;
    pub fn pipeline(&self) -> &[PipelineStep];
    pub fn guardrails(&self) -> &Guardrails;
    pub fn parallelism(&self) -> &Parallelism;
    pub fn state(&self) -> &StateConfig;
}

#[derive(thiserror::Error, Debug)]
pub enum ConfigError {
    #[error("IO error reading {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },

    #[error("YAML syntax error in {path} at line {line}: {message}")]
    Syntax { path: PathBuf, line: usize, message: String },

    #[error("Validation failed: {0}")]
    Validation(String),
}
```

### Schema

Mirror DESIGN.md §4 exactly. Fields:

- `version: u32` (currently `1`)
- `site: { name, prefix }`  (per D27, no `role:` field)
- `models: HashMap<String, ModelDef>` — provider, model, optional
  cli, optional endpoint/region/deployment/base_url, optional
  max_tokens / temperature / cache / timeout / rate_limit /
  cost_hint
- `roles: HashMap<String, String>` — role name → model name
- `tools:`
  - `speckit: { enabled, version }`
  - `assay: { enabled: bool, role: String, reviewers:
    Vec<String>, rounds: String (templated, opaque to this crate),
    strict: bool, on_split: "reject" | "human" | "majority" }`
  - `substrate: { backend: "native" | "none", mode: "solo" |
    "copilot" | "crew" }`
  - `copilot: { enabled, agent_identity }`
  - `git: { stacking: { backend: "none" | "native" |
    "graphite" | "git-spice", branch_pattern, auto_restack_on_merge,
    force_push: "with-lease" | "off", auto_pr, draft } }`
- `pipeline: Vec<PipelineStep>` where each step has:
  - `id: String`
  - `role: Option<String>` XOR `runner: Option<Runner>`
  - `host: Option<String>` (`claude` | `codex` | `copilot` if present)
  - `command: Option<String>` (template; resolved later, not here)
  - `inputs: Option<Vec<String>>`
  - `skippable: Option<bool>`
  - `default_skip: Option<bool>`
  - `prompt: Option<String>` (used by `runner: human` steps; §4)
  - `rounds: Option<String>` (used by `assay` step; templated, opaque here)
  - `on_reject: Option<OnReject>` (`halt` | `warn`; §7)
  - `on_failure: Option<OnFailure>` (`pause` | `retry` | `abort`; §8.3 dispatch-copilot)
  - `poll_interval: Option<String>` (e.g. `"30s"`; opaque string here)
  - `batch: Option<String>` (template var name; bridge step)
  - `executor_role: Option<String>` (foreman step)
  - `parallel_group: Option<String>` (§9.C.4)

  Per D27, pipeline steps with `runner: derrick` do **not** also
  carry a `role:` binding. The XOR rule (validation rule 3 below)
  is therefore unambiguous.
- `guardrails: { constitution_path, forbid_paths, required_labels }`
- `parallelism: { batch_max, step_max, assay_max }`
- `state: { dir, log_runs, worktree_root }`

Use `serde(deny_unknown_fields)` on every struct so typos in
yaml are caught at parse time rather than silently ignored.
Unknown-field errors must carry the offending field path and
the source line (use `serde_yaml::Error::location()` plus the
serde path) — the same UX bar as `ConfigError::Syntax`.

### Optional fields and defaults

Several DESIGN.md §4 fields are optional in yaml and take
defaults when omitted. Mark these as `Option<_>` in the struct
**and** define their `Default` so omission round-trips cleanly:

| Field | Optional | Default when omitted |
|---|---|---|
| `tools.git` | yes | `{ stacking: { backend: "none", ... defaults below ... } }` |
| `tools.git.stacking.backend` | yes | `"none"` |
| `tools.git.stacking.branch_pattern` | yes | `"derrick/{{batch}}/{{ticket_id}}"` |
| `tools.git.stacking.auto_restack_on_merge` | yes | `true` |
| `tools.git.stacking.force_push` | yes | `"with-lease"` |
| `tools.git.stacking.auto_pr` | yes | `false` (D22) |
| `tools.git.stacking.draft` | yes | `false` |
| `tools.assay.on_split` | yes | `"reject"` (D6 / §9.C.2 default fail-closed) |
| `tools.assay.strict` | yes | `false` |
| `tools.assay.rounds` | yes | `"1"` (templated string) |
| `tools.copilot.enabled` | yes | `false` (only relevant when substrate.mode != "solo") |
| `pipeline[].inputs` | yes | `[]` |
| `pipeline[].skippable` | yes | `false` |
| `pipeline[].default_skip` | yes | `false` |
| `pipeline[].parallel_group` | yes | `None` |
| `guardrails.forbid_paths` | yes | `[]` |
| `guardrails.required_labels` | yes | `[]` |

All other fields in DESIGN.md §4 are required (e.g.
`version`, `site.name`, `site.prefix`, `models`, `roles`,
`tools.substrate.backend`, `tools.substrate.mode`,
`parallelism.*`, `state.dir`).

### Layered merge semantics

`Config::load_layered(repo_root)` builds the effective config
from three layers, lowest precedence first:

1. `Config::defaults()` (built into the binary).
2. `~/.derrick/config.yaml` if present and readable.
3. `<repo_root>/derrick.yaml` if present.

Merge rules, applied per-field:

- **Maps keyed by name** (`models`, `roles`): merge by key.
  Higher layer overrides on key collision; absent keys
  inherit from the lower layer.
- **Sequences** (`pipeline`, `tools.assay.reviewers`,
  `guardrails.forbid_paths`, `guardrails.required_labels`):
  higher layer **replaces wholesale** if present. Empty
  sequences in a higher layer count as "set" — they replace.
  Absent sequences in a higher layer inherit.
- **Nested structs** (`tools`, `tools.assay`, `tools.git`, etc.):
  field-by-field. `Option<_>` fields use the higher layer's
  value if `Some`, else fall through.
- **Scalars**: higher layer wins if present.
- After merge, run `validate()` on the result, not on each
  layer individually.
- **YAML `null` is treated as absent** (same as omitting the
  key). Higher layers cannot use `null` to forcibly clear an
  inherited value; if a downstream feature needs that, file a
  follow-up design question.

Document this precisely in the crate-level rustdoc so
downstream crates can rely on it.

### Scope of validation

`derrick-config` performs **structural** validation only:
schema shape, enum values, intra-config references (a role
named in `pipeline.role` exists in `roles:`; a model named in
`roles:` exists in `models:`; etc.). Host/provider/auth
compatibility (e.g. *"host claude can't use provider ollama"*)
is **out of scope** here — that's `derrick models check`'s job
per D15 / DESIGN.md §6.5, surfaced as warnings at `derrick
init` and `derrick run` rather than as config-load failures.

### Validation rules

`Config::validate()` must catch and report (with the offending
section name in the error message):

1. Every role in `roles:` points to a model present in `models:`.
2. Every pipeline step that uses `role:` references a role
   defined in `roles:`.
3. Pipeline steps use `role:` XOR `runner:`, never both, never
   neither. Steps with `runner: human` must have a `prompt:`;
   steps with `runner: derrick` may omit `command:`.
4. `tools.assay.role` (when assay enabled) references a role
   defined in `roles:`.
5. Every entry in `tools.assay.reviewers` references a role
   defined in `roles:`.
6. `tools.substrate.backend` is one of `native` or `none` (v1).
   Reject `gastown` with a clear message that gastown backend
   ships in a future version.
7. `tools.assay.reviewers` is non-empty when `tools.assay.enabled`.
8. `tools.assay.on_split` is one of `reject | human | majority`.
    When `majority`, `reviewers` length must be odd; otherwise
    raise a validation error suggesting `reject` as the safe
    fallback (§9.C.2).
9. `tools.git.stacking.backend` is one of `none | native |
   graphite | git-spice`.
10. `site.prefix` matches `^[a-z]{1,6}$`.
11. `parallelism.batch_max` and `step_max` are `>= 1` and `<= 64`.
12. `state.dir` is a relative path (not absolute).
13. `executor_role` on a foreman step references a role
    defined in `roles:`.
14. `version` equals `1`. Any other value is a validation
    error with a migration-oriented message
    (*"unsupported config version N; this binary speaks v1
    only. See DESIGN.md §4."*).

Validation errors should include the path-style location of the
offending field (e.g. `tools.substrate.backend: "gastown" is not
allowed in v1; future backends slot in behind the Substrate
trait — see DESIGN.md §8.5`).

### Defaults

`Config::defaults()` returns a `Config` that:

- Has empty `pipeline:` (caller fills this in).
- Defines `roles: { proposer: claude-opus, drafter:
  claude-sonnet, reviewer: codex-gpt5, executor: copilot,
  summariser: claude-sonnet }`.
- Defines those five models with sensible provider entries.
- Sets substrate `backend: native`, `mode: solo`.
- Disables assay, copilot, git.stacking by default.
- `parallelism: { batch_max: 8, step_max: 4, assay_max: 2 }`.
- `state: { dir: ".derrick", log_runs: true, worktree_root:
  ".derrick/worktrees" }`.

### Tests

Real files via `tempfile::tempdir()`. No mocks. Each validation
rule above (1–14) gets a dedicated unit test that builds an
invalid config and asserts the right `ConfigError::Validation`
message. Plus:

- `parses_minimal_valid_yaml` — smallest legal `derrick.yaml`
  round-trips.
- `parses_full_design_md_example` — the yaml literal from
  DESIGN.md §4 parses cleanly.
- `defaults_validate` — `Config::defaults()` passes
  `validate()` unchanged.
- `layered_load_overrides_correctly` — built-in defaults <
  ~/.derrick/config.yaml < repo derrick.yaml, with a
  three-layer fixture proving each override path.
- `unknown_field_is_rejected` — `serde(deny_unknown_fields)`
  catches a typo at the right field.
- `yaml_syntax_error_reports_line` — a malformed file yields
  `ConfigError::Syntax { line, .. }` with the correct
  1-indexed line number for the offending token.
- `missing_optional_sections_default_correctly` — a yaml that
  omits `tools.git` and `tools.assay.on_split` parses cleanly
  and the resulting `Config` exposes the defaults documented
  above (e.g. `cfg.tools().git().stacking().backend() ==
  StackBackendKind::None`, `cfg.tools().assay().on_split() ==
  OnSplit::Reject`).
- `unsupported_version_is_rejected` — `version: 2` round-trips
  through the parser then trips rule 14.

**Coverage target**: 90%+ on this crate (it's small and
mechanical; can't hide behind "edge cases").

## Dependencies to add (workspace.dependencies are already declared)

```toml
[dependencies]
serde = { workspace = true }
serde_yaml = { workspace = true }
thiserror = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
```

## Out of scope (don't do these in this ticket)

- Loading any host CLI's context.
- Resolving template strings in `command:`, `rounds:`,
  `inputs:`, etc. Schema parses them as opaque strings;
  resolution happens in `derrick-flow`.
- Host/provider/auth compatibility validation (D15 / §6.5 — that's
  `derrick models check`).
- Writing a `derrick.yaml` (only reading + validating).
- Migration of older config versions (we're at v1).

## Acceptance

- [ ] `cargo check -p derrick-config` passes.
- [ ] `cargo clippy -p derrick-config -- -D warnings` passes.
- [ ] `cargo test -p derrick-config` passes, all tests above
      present.
- [ ] `cargo llvm-cov -p derrick-config --fail-under-lines 90`
      passes.
- [ ] Public types are documented (`missing_docs` warn).
- [ ] No `unwrap()` / `expect()` / `panic!` in non-test code
      (workspace lints forbid these).

## Reviewer notes (Codex)

Verdict format per DESIGN.md §7:
- Top three risks.
- Any contradiction with DESIGN.md §4 or §6.5 (models /
  roles / BYOM).
- Verdict: `accept | revise | reject`.

## Implementer notes (Copilot)

Stay in `crates/derrick-config/`. Do not touch any other crate.
If schema feels wrong somewhere, leave a `// TODO(T001):
escalate to flow-engineer` and surface it in the PR description
rather than improvising.
