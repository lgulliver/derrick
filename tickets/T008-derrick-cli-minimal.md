# T008 — `derrick-cli` minimal binary

**Specialist owner**: `flow-engineer` + `rust-architect` (opus)
**Crate**: `crates/derrick-cli`
**Depends on**: `derrick-config`, `derrick-substrate`, `derrick-substrate-native`, `derrick-memory`
**Priority**: P0 — half of the dogfooding bar (AGENTS.md). T009 (derrick-flow) is the other half.

## Why

The user types `derrick init`, `derrick status`, `derrick
doctor`. The binary needs to exist and route to the right
internals. This ticket builds the **CLI structure only** —
`derrick run` is wired as a stub that delegates to
`derrick-flow`, which lands in T009.

## Scope (MVP for dogfooding)

### Subcommands shipped in T008

| Command | Behaviour |
|---|---|
| `derrick init --greenfield [--mode solo\|copilot\|crew] [--site <name>] [--prefix <prefix>] [--force]` | **Greenfield init only in T008.** Writes `derrick.yaml` from the workspace template, creates `.derrick/`, opens `NativeSubstrate` for the new site (runs migrations). Refuses if `derrick.yaml` already exists without `--force`. Matches DESIGN.md §5.2's `--greenfield` opt-in. |
| `derrick init` (bare, no `--greenfield`) | **Refuses with a T011 pointer.** Per DESIGN.md §5.2, bare `derrick init` is brownfield-first: it runs an adoption pass, proposes the writes, and only bootstraps after the user confirms. That logic lives in `derrick-adopt` (T011, not yet built). T008 prints: *"Brownfield init (the default) is provided by `derrick-adopt` (T011), which is not yet implemented. For a fresh repo use `derrick init --greenfield`. Existing repos with AGENTS.md / CLAUDE.md / .specify/ / existing trackers should wait for T011 to land before being initialised."* and exits 1. This keeps the bare-`init` contract reserved for the brownfield-first semantics; T008 doesn't redefine it. |
| `derrick status [--format human\|json] [--watch]` | Read-only dashboard. Reads from `NativeSubstrate`: site, active batch summary, tickets by state, foreman status, last assay verdict (from `.derrick/runs/`). `--watch` polls every 1s. |
| `derrick doctor [--format human\|json]` | Health checks driven by the user's `derrick.yaml`: only required binaries (derived from `tools.copilot.enabled`, `tools.assay.enabled`, and the providers referenced by configured roles) are checked. See "Doctor check derivation" below. Exit code = number of failures. |
| `derrick run add-feature [--prompt "..."] [--resume-from <step>] [--no-clarify] [--no-checkpoint] [--no-assay]` | **Stub:** prints *"`derrick run add-feature` is implemented in T009. Until then, see tickets/T009-derrick-flow-minimal.md."* and exits 1. The flag surface is parsed (so T009 can drop in without breaking users' muscle memory) but no side effects occur. |
| `derrick --version` | Prints `derrick X.Y.Z` from `env!("CARGO_PKG_VERSION")`. |
| `derrick completions <shell>` | Emits a completion script for `bash | zsh | fish | elvish | powershell`. Uses `clap_complete`. |

`derrick run` (without `add-feature`) and other `run` shapes
mentioned in DESIGN.md (`derrick run <id>` replay,
`derrick run <custom-pipeline>` future) are explicitly NOT
exposed in T008. `add-feature` is the only `run` shape the
clap surface knows about. This keeps the door open for T009+
to extend `derrick run` with additional subcommands without
breaking the positional shape.

### Out of scope for T008 (later tickets)

- `derrick run add-feature` actual pipeline execution → T009.
- `derrick scrub` / `derrick caveman` CLI subcommands → T012 wires them.
- `derrick ticket new/done/block/reopen`, `derrick batch close` → T013.
- `derrick gain`, `derrick gain --pillars` → T014.
- `derrick mayor` foreman lifecycle → T015 (T010 is the foreman loop in derrick-substrate-native).
- `derrick observe` TUI → T016.
- `derrick stack` → T017.
- `derrick auth set / list` → T018.
- `derrick models check` → T019.
- `derrick uninstall` → T020.

The CLI surface is split deliberately so each crate's ticket
adds its own subcommand. T008 leaves clean extension points
(via `clap`'s `Subcommand` derive on a `Cli` struct) for the
follow-ups to plug into.

### Structure

```
crates/derrick-cli/
├── Cargo.toml
└── src/
    ├── main.rs              # entry point; clap parse + dispatch
    ├── lib.rs               # `pub fn run(args) -> ExitCode` so the
    │                        # binary stays a thin shell over an
    │                        # testable function
    ├── commands/
    │   ├── mod.rs           # `pub enum Command { Init(InitArgs), ... }`
    │   ├── init.rs
    │   ├── status.rs
    │   ├── doctor.rs
    │   ├── run.rs           # stub
    │   └── completions.rs
    ├── output.rs            # human vs json rendering helpers
    └── exit_code.rs         # typed exit codes (Success=0, Doctor=N>0, etc.)
```

### `derrick init --greenfield` details

(Bare `derrick init` exits 1 with the T011 pointer — see the
table above. The flow below applies only when `--greenfield`
is passed.)

1. Resolve cwd to repo root (walk up to find `.git`; if none,
   error: "derrick init must be run inside a git repo").
2. Check for existing `derrick.yaml`. If present and no
   `--force`, exit with the path and a hint.
3. Interactive prompts (skipped if all `--site` / `--prefix` /
   `--mode` provided):
   - Site name (default: repo dir basename).
   - Ticket prefix (default: first 3 chars of site name,
     lowercased, alphabetic only; validate `^[a-z]{1,6}$`).
   - Mode (default: `solo`).
4. Write `derrick.yaml` from the workspace template at
   `templates/derrick.yaml.in` (workspace root, **not**
   crate-local — matches DESIGN.md §3.1 component table:
   templates live at the workspace root, owned at the
   schema-shape level by `derrick-config` and embedded by
   any crate that needs them via `include_str!`). The
   template includes the minimum required fields plus a
   pipeline that matches the spec → clarify → plan → assay
   → analyze → tasks default. A small template-render helper
   lives in `derrick-config` so future template consumers
   (T011 adopt, downstream tooling) don't re-invent the
   substitution logic.
5. Create `.derrick/` directory + `.gitignore` entry for
   `.derrick/runs/`, `.derrick/state.json`, etc. (per repo
   `.gitignore` already covers these from the workspace
   init; the per-crate-init step just ensures the dir exists).
6. Open `NativeSubstrate` (migrations run on first open).
7. Print a one-screen summary: site, mode, prefix, pipeline
   id list, next step ("now run `derrick doctor` to verify
   the install").

**Brownfield**: if the cwd already has `AGENTS.md`,
`CLAUDE.md`, `.claude/agents/`, etc., this ticket **does not
touch them**. T011 (`derrick-adopt`) is the brownfield
detection ticket that adopts existing files. T008 simply
refuses to overwrite anything outside `derrick.yaml` and
`.derrick/`.

### `derrick status` details

Reads from `NativeSubstrate`. Output for `mode: solo` (no
batch, no foreman):

```
$ derrick status
site         taxi-ingest                            mode: solo
backend      native                                 db: .derrick/derrick.db
last run     20260518T012714Z                       (no active batch)
```

Output for `mode: crew` mid-flight:

```
site         taxi-ingest                            mode: crew
batch        001-webhook-ingest      11 tickets     3 done • 2 in-flight • 6 ready
foreman      detached (pid 28411, 14m)
backend      native
last assay   2026-05-17 09:18  →  accept (round 2)

in flight:
  ti-50  ▸  hand:bramble       storage layer …                   12m
  ti-51  ▸  hand:sumac         replay-safe migration               4m
ready next:
  ti-52     handler wiring                  blocked by: ti-50
  ti-53     contract test for /ingest      blocked by: ti-50, ti-51
```

`--format json` emits a single JSON object with all of the
above as structured fields.

`--watch` redraws every 1s using `crossterm` (we already
have it as a workspace dep from T013 ratatui pulling it in;
if not, add as a workspace dep here — `crossterm = "0.28"`).

### `derrick doctor` details

Checks return `Pass | Warn | Fail` with a message. Exit code
= count of `Fail`s. The required check set is **derived from
the user's `derrick.yaml`**, not hard-coded by mode (D15 —
config-driven, not policy-driven):

**Always run** (no derrick.yaml needed for these):

- `which git` → required.
- `derrick.yaml` exists and parses → required.
- `Config::validate()` passes → required.

**Run if `derrick.yaml` parses**, deriving from config:

- For each model in `models:`, check the binary or env-var
  required by that provider:
  - `provider: shell` → `which <argv[0]>`.
  - `provider: openai-cli` → `which codex`.
  - `provider: copilot-cli` → `which copilot`.
  - `provider: anthropic | openai | google | bedrock |
    azure-openai | ollama | llamacpp` → check the
    documented env var (e.g. `ANTHROPIC_API_KEY`). Use
    `AuthStore::missing_required()` from T006.
  - Host-delegated providers (`host_delegated_auth() ==
    true` — claude/codex/copilot) check binary presence, NOT
    env vars.
- For each pipeline step's `host:`, `which <host>` →
  required.
- `.derrick/` accessible → required.
- `NativeSubstrate::open()` succeeds with the configured
  site → required (catches site-mismatch corruption).
- D29 host hooks installed → **warn-only** in T008 (T011
  installs them; T008 just reports presence).
- D21 squash-merge stance: if `tools.git.stacking.backend !=
  "none"`, query `gh api repos/{owner}/{name}` for
  allow_squash_merge / merge_commit / rebase_merge defaults
  and warn if squash is the only option.

`--format json` emits a list of `{check, status, message,
remediation}` objects so machines can read it.

**Severity matrix is config-driven**: a binary like `codex`
is `Fail` only when a configured model or pipeline step
actually requires it; if no role binds to it, its absence is
not a problem.

### Dependencies

```toml
[dependencies]
derrick-config = { path = "../derrick-config" }
derrick-substrate = { path = "../derrick-substrate" }
derrick-substrate-native = { path = "../derrick-substrate-native" }
derrick-memory = { path = "../derrick-memory" }
clap = { workspace = true }
clap_complete = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
serde_yaml = { workspace = true }
tokio = { workspace = true }
anyhow = { workspace = true }
thiserror = { workspace = true }
tracing = { workspace = true }
tracing-subscriber = { workspace = true }
chrono = { workspace = true }
which = { workspace = true }
crossterm = { workspace = true }
```

No new top-level workspace deps.

### Tests

`assert_cmd`-based integration tests against the binary,
using `tempfile::tempdir()` for an isolated repo:

- `bare_init_refuses_with_t011_pointer` — `derrick init`
  without `--greenfield` exits 1 with a message naming T011.
- `greenfield_init_in_empty_repo_creates_files` — bare repo
  + `derrick init --greenfield --site test --prefix tst
  --mode solo` produces `derrick.yaml` and
  `.derrick/derrick.db`.
- `greenfield_init_refuses_existing_yaml_without_force`.
- `greenfield_init_overwrites_with_force`.
- `init_refuses_outside_git_repo` (applies to both shapes).
- `greenfield_init_validates_prefix`.
- `status_shows_site_after_init`.
- `status_json_round_trips` — parses output back into the
  same structure.
- `doctor_passes_after_successful_init` — with all binaries
  mocked via PATH manipulation.
- `doctor_fails_when_yaml_invalid`.
- `doctor_fails_when_substrate_corrupt` — write nonsense to
  derrick.db, verify failure.
- `doctor_exit_code_equals_fail_count`.
- `run_stub_prints_t009_hint_and_exits_1`.
- `completions_emit_for_each_shell`.
- `version_matches_cargo_pkg_version`.

Unit tests inside crate for output formatting helpers.

**Coverage target**: 80%. CLI integration tests carry most
of the weight; some clap-derived code is hard to exercise
fully without breaking encapsulation. The 80% workspace
floor is the gate.

## Acceptance

- [ ] `cargo check -p derrick-cli` passes.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] `cargo test -p derrick-cli` passes; 3× stress green.
- [ ] `cargo llvm-cov -p derrick-cli --fail-under-lines 80`.
- [ ] Workspace `cargo llvm-cov --fail-under-lines 80` still passes.
- [ ] Built binary runs: `cargo run -p derrick-cli -- --version`
      emits a version string.
- [ ] `cargo run -p derrick-cli -- init` inside a temp git
      repo exits 1 and prints the T011 pointer (bare init is
      reserved for the brownfield-first contract).
- [ ] `cargo run -p derrick-cli -- init --greenfield --site
      test --prefix tst --mode solo` inside a temp git repo
      produces a valid `derrick.yaml` + working substrate.
- [ ] No `unwrap`/`expect`/`panic` in non-test code.
- [ ] No gastown vocabulary.

## Reviewer notes (Codex)

Pre-implementation review. Focus on:
- Is `derrick run` as a stub honest, or does the user see
  partial state mid-run?
- Are the subcommand boundaries clean enough for T009–T020
  to slot in without rewrites?
- Is `derrick doctor` exit-code semantics clear?
- Is the `derrick.yaml.in` template scope correct or should
  the template live in `derrick-config`?

## Implementer notes (Copilot)

Stay in `crates/derrick-cli/`. clap is the lib; route by
`#[derive(Subcommand)]`. Test scripts manipulate `PATH` via
`Command::env_clear().env("PATH", ...)` to mock external
binaries. macOS + Linux only; Windows is a clear TODO.
