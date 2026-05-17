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
| `derrick init [--mode solo\|copilot\|crew] [--site <name>] [--prefix <prefix>] [--force]` | Brownfield-safe init. Writes `derrick.yaml` from template, creates `.derrick/`, opens `NativeSubstrate` for the new site (runs migrations). Refuses if `derrick.yaml` already exists without `--force`. |
| `derrick status [--format human\|json] [--watch]` | Read-only dashboard. Reads from `NativeSubstrate`: site, active batch summary, tickets by state, foreman status, last assay verdict (from `.derrick/runs/`). `--watch` polls every 1s. |
| `derrick doctor [--format human\|json]` | Health checks: binaries on PATH (`claude`, `codex`, `copilot`, `git`), `derrick.yaml` valid, `.derrick/` accessible, substrate openable, host hooks present (if `mode != solo`). Exit code = number of failures. |
| `derrick run <pipeline> [--prompt "..."] [--resume-from <step>]` | Stub: prints `derrick run is implemented in T009; the pipeline pipeline is defined in derrick.yaml. For now, see tickets/T009-...md` and exits 1. The plumbing for argparsing and config loading is in place so T009 can drop the runner in. |
| `derrick --version` | Prints `derrick X.Y.Z` from `env!("CARGO_PKG_VERSION")`. |
| `derrick completions <shell>` | Emits a completion script for `bash | zsh | fish | elvish | powershell`. Uses `clap_complete`. |

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

### `derrick init` details

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
4. Write `derrick.yaml` from the template
   (`templates/derrick.yaml.in` in this crate). The template
   includes the minimum required fields plus a pipeline that
   matches the spec → clarify → plan → assay → analyze →
   tasks default.
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

Each check returns `Pass | Warn | Fail` with a message.
Exit code = count of Fails.

| Check | Pass | Warn | Fail |
|---|---|---|---|
| `which claude` | binary found | n/a | not on PATH |
| `which codex` | found | n/a | not found |
| `which copilot` | found | warn if absent and mode is `copilot`/`crew` | fail if mode `copilot` and absent |
| `which git` | found | n/a | not found |
| `derrick.yaml` exists and parses | valid | n/a | missing or invalid |
| `Config::validate()` | passes | n/a | fails |
| `.derrick/` accessible | yes | n/a | permission denied / missing |
| Substrate opens | yes | n/a | site mismatch / corruption |
| Hooks installed (D29) if `mode != solo` | present | not present (T011 hasn't run init-hooks) | n/a yet — warn only in T008 |
| Repo merge strategy (D21) | merge-commit or rebase available | squash-only & stacking enabled | n/a — warn only |

`--format json` emits structured findings.

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

- `init_in_empty_repo_creates_files` — bare repo + `derrick
  init --site test --prefix tst --mode solo` produces
  `derrick.yaml` and `.derrick/derrick.db`.
- `init_refuses_existing_yaml_without_force`.
- `init_overwrites_with_force`.
- `init_refuses_outside_git_repo`.
- `init_validates_prefix`.
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
      repo produces valid derrick.yaml + working substrate.
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
