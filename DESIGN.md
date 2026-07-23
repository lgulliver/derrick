# Derrick — Design

> **derrick** *(n.)* The load-bearing tower over an oil well. The structure
> that lifts every length of pipe in and out of the hole. Without it, the
> rig is a hole in the ground.

**Derrick** is a Rust CLI that turns a single command into a full dark-factory
feature pipeline — spec, adversarial review, tickets, dispatch, PR stacking —
without asking you to wire each underlying tool by hand. One install, one
config (`derrick.yaml`), one primary command (`/drill`).

> *Lineage note: derrick's pipeline pattern descends from the
> speckit → courtroom → gastown toolchain it replaces. courtroom is the
> historical inspiration for the assay; gastown is the historical inspiration
> for the execution substrate. Neither is a runtime dependency.*

---

## 1. Problem

Getting a coherent spec → adversarial-review → task → dispatch → PR-stack
flow running in a new repo previously meant:

1. Install `claude`, `codex`, and one or more stacking CLIs separately.
2. Install a courtroom-style adversarial-review plugin.
3. Wire a speckit constitution and per-project config.
4. Author bespoke CLAUDE.md / AGENTS.md that explain the glue.
5. Build your own execution substrate (ticket tracking, foreman, worktrees).
6. Document all the caveats and runbook steps.

This is bespoke per repo. Every part is **glued to a specific toolchain** —
its phase labels, its rules, its config path.

We want: **any user, any repo, single command, `/drill` UX.**

---

## 2. Goals & non-goals

### Goals

Three architectural pillars (see §9):

- **Memory** — derrick seeds and curates persistent memory so the
  assistant doesn't relearn the rig every turn.
- **Tokens** — every byte across a model boundary earns its place,
  via tiering, scrubbing, caveman compression, prompt caching, and
  lazy artifact loading.
- **Parallelism** — independent work runs concurrently by default;
  sequential work is a justified exception.

Plus the product surface:

- **One-line install**: `curl -fsSL <url> | bash` puts the `derrick`
  binary and Claude Code plugin on the user's machine and verifies
  the deps.
- **One-line init**: `derrick init` in a repo writes the config, the
  templates, the hooks, the constitution skeleton, and registers the
  site with the native substrate.
- **One primary command**: `/drill <prompt>` runs the full
  dark factory pipeline — spec → assay → plan → tasks → batch →
  foreman / Copilot agents.
- **One front door for observability**: `derrick status` is the
  answer to "what's going on?" — one command, not one per underlying tool.
- **Reusable**: nothing in derrick assumes a specific toolchain. Project-
  specific rules live in the repo's constitution + `derrick.yaml`,
  not in derrick.
- **Transparent**: every underlying tool call is logged and exit
  codes propagate. Nothing is magic. Power users can still call
  `claude /speckit.specify` or any host CLI directly.

### Non-goals (v1)

- Re-implementing speckit. Derrick defers to speckit when it is
  installed (detect-then-defer, D2); it does not replace speckit.
- A GUI. CLI + slash command only.
- Cross-language polyglot dispatching beyond what the host CLIs already do.

---

## 3. Architecture

```
┌──────────────────────────────────────────────────────────────────┐
│                      User (in any repo)                          │
│   $ derrick init        $ /drill "build the X service"           │
└─────────────────┬───────────────────────────┬────────────────────┘
                  │                           │
                  ▼                           ▼
        ┌──────────────────┐        ┌───────────────────────┐
        │   derrick CLI    │        │  /drill command       │
        │   (Rust binary)  │        │  (Claude Code plugin) │
        └────────┬─────────┘        └───────────┬───────────┘
                 │                              │
                 │  reads/writes                │ shells out via
                 │  derrick.yaml                │ derrick run …
                 ▼                              ▼
        ┌─────────────────────────────────────────────────┐
        │              Derrick Orchestrator               │
        │                                                 │
        │   Phase pipeline  ←→  derrick.yaml (per repo)   │
        │   Native substrate ←→  .derrick/derrick.db      │
        │   Logging         ←→  .derrick/runs/<ts>.log    │
        └──────────┬───────────────────┬──────────────────┘
                   │                   │
       speckit     │  (optional,       │  all model inference
       detect-     │   D2)             │  routes through one of
       then-defer  ▼                   ▼  five host CLIs:
                ┌──────┐   ┌───────────────────────────────┐
                │spec  │   │  claude │ codex │ copilot      │
                │kit   │   │  opencode │ aider              │
                └──────┘   └───────────────────────────────┘
```

### 3.1 Components

Rust workspace. One binary (`derrick`), many crates so individual
modules can be tested, profiled, and (later) ported in isolation.

| Component | Crate | Purpose |
|---|---|---|
| `derrick` CLI | `crates/derrick-cli` | clap-based binary; subcommands route to other crates |
| Orchestrator | `crates/derrick-flow` | Pipeline state machine, step runner, structured logging (`tracing`) |
| Tool adapters | `crates/derrick-tools` | Thin wrappers around `claude`, `codex`, `copilot`, `specify` (tokio::process) |
| Assay | `crates/derrick-assay` | Adversarial plan review; calls the reviewer role(s) directly (see §7) |
| Memory | `crates/derrick-memory` | Seeds host memory files on init + per-step context budgets |
| Scrubber | `crates/derrick-scrub` | Subprocess output filter; rules-based sanitizer for cargo/git/gh/claude/opencode output before writing to step logs. Reduces context bytes fed to the LLM in subsequent steps. Per-tool rule sets in `src/rules/<tool>.rs`. Pure functions, zero-copy where possible. |
| Roughneck | `crates/derrick-roughneck` | LLM output compression via prompt injection. Three levels: `lite` (~30% savings), `full` (~65%, default), `ultra` (~75%). Fires after every model boundary to compress the response before it enters the next step's context. |
| Caveman | `crates/derrick-caveman` | Text compressor for inter-step handoff, pure functions |
| Copilot | `crates/derrick-copilot` | Dispatches steps or tickets to Copilot agents (CLI + Workspace API) |
| Models | `crates/derrick-models` | Provider trait; adapters for API providers, local runtimes, CLI shells (see §6.5) |
| Adopt | `crates/derrick-adopt` | Brownfield detection of AGENTS.md, CLAUDE.md, agents/, skills/, docs (§5.6); writes Claude Code host hook configs (`.claude/settings.json` PreToolUse/PostToolUse) so scrub+caveman fire at host boundaries (D29). Codex hook installation deferred (D34); T011 writes `.codex/instructions.md` only. |
| Substrate trait | `crates/derrick-substrate` | A family of focused async role traits (`TicketStore`, `EventLog`, `HandRegistry`, `ForemanState`, `WorktreeReservations` — D89, splitting the prior monolithic `Substrate` trait); a native impl implements all of them, future impls slot in behind whichever slice they need |
| Native substrate | `crates/derrick-substrate-native` | SQLite-backed execution substrate + in-process foreman |
| Stack | `crates/derrick-stack` | PR stacking: native engine (plain git + `gh`); `StackBackend` trait as extension seam (see §8.5) |
| Survey | `crates/derrick-survey` | Native code-graph index: SQLite + FTS5 symbol/reference/call-graph index at `.derrick/index.db`; MCP server surface for agent queries; CLI subcommands `build|search|context|impact|status` for ad-hoc/Bash parity (see §9.B.8) |
| TUI | `crates/derrick-tui` | `derrick observe` — ratatui-based interactive dashboard (see §5.7) |
| Observe | `crates/derrick-observe` | Aggregated read-only view (talks to substrate trait) |
| Config | `crates/derrick-config` | Load + validate `derrick.yaml` (serde) |
| Repo templates | `templates/` | What `derrick init` copies in |
| Plugin | `templates/.claude/` | `/drill` command + skill |
| Install script | `scripts/install.sh` | Curlable bootstrap |

Why Rust?

- **Lightweight at runtime.** `derrick scrub` wraps every subprocess; cold-start
  cost matters. Rust gives us near-zero overhead.
- **Pure-function hot paths.** Scrubber and caveman are string-manipulation
  hot paths — Rust's iterators and zero-copy slices fit naturally.
- **Async fan-out.** Multi-reviewer assay, parallel observability reads, and
  the foreman loop are textbook tokio workloads.
- **Single static binary.** `cargo build --release` → one file, easy to ship
  via GitHub releases or Homebrew.
- **SQLite via `rusqlite`** (bundled) or `sqlx` — both first-class.

Trade-off accepted: build times are slower than Go's. That cost is bounded.

---

## 4. `derrick.yaml` — the per-repo contract

This is the only file derrick truly *owns* in the user's repo. Everything
else (`.specify/`, `.claude/`, constitution) is content derrick writes
once during `init` and the user then owns.

```yaml
# derrick.yaml — single source of truth for this repo's pipeline
version: 1

# Identity for the substrate
site:
  name: my-project
  prefix: mp           # ticket prefix (mp-1, mp-2 …)

# Model registry — define providers once, name them in roles
# provider names match host CLIs: claude | codex | copilot | opencode | aider
models:
  claude-opus:    { provider: claude,   model: "claude-opus-4-8" }
  claude-sonnet:  { provider: claude,   model: "claude-sonnet-4-6" }
  claude-haiku:   { provider: claude,   model: "claude-haiku-4-5" }
  codex-gpt5:     { provider: codex,    model: "gpt-5.5" }
  copilot:        { provider: copilot,  model: "gpt-5.4" }
  # opencode and aider use provider/model strings:
  # opencode-claude: { provider: opencode, model: "anthropic/claude-opus-4-8" }
  # aider-gpt5:    { provider: aider,    model: "openai/gpt-5.5" }
  # shell escape hatch — any CLI that speaks the structured prompt envelope:
  # my-tool:      { provider: shell, command: "my-tool --prompt-envelope" }

# Role bindings — pipeline steps name a role; the role names a model.
# Changing one model changes the whole class of step that uses it. Use the
# expanded form when the role should also carry a host-native agent file.
roles:
  proposer:  claude-opus       # plan (heavy reasoning)
  drafter:   claude-sonnet     # specify + tasks (mechanical)
  reviewer:
    model: codex-gpt5          # assay (adversarial, different family)
    agent: .codex/agents/integrations-engineer.md
  executor:  copilot           # ticket dispatch in crew/copilot mode
  summariser: claude-sonnet    # inter-step caveman-augmented summary, if used

# Underlying tool versions / opt-outs
tools:
  specify: { provider: native } # native | speckit | import
  speckit: { enabled: true, version: ">=0.4.0" }
  assay:
    enabled: true
    role: reviewer              # which role runs cross-examination
    reviewers: [reviewer]       # list → multi-reviewer assay (§9.C.2)
    rounds: 1
    strict: false
  substrate:
    backend: native             # v1: native | none. Trait allows more backends later.
    mode: crew                  # solo | copilot | crew (see §8)
  copilot:
    enabled: true
    agent_identity: derrick-hand
  # No external rtk/caveman dependency — derrick ships its own (see §9).
  roughneck:
    enabled: true
    level: full                  # lite | full | ultra (see §9.B.2a)
    compress_memory: true        # also compress per-run memory digests
  output_compression:
    enabled: true                # derrick-scrub subprocess filters (see §9.B.2)

# /drill pipeline. Steps run in order; any can be skipped via flag.
# Each step names a role (resolved via `roles:` above) or runner: derrick / human.
pipeline:
  - id: specify
  - id: clarify
    runner: derrick
    skippable: true
  - id: plan
  - id: assay
    runner: derrick               # in-process; uses the reviewer role(s) (§7)
    inputs: [{{feature_dir}}/spec.md, {{feature_dir}}/plan.md]
    rounds: "{{tools.assay.rounds}}"
    on_reject: halt               # halt | warn — fail closed by default
    # Headless mode: when stdin is not a TTY, assay runs without interactive
    # prompts. Only a `reject` verdict blocks the pipeline; `revise` and
    # `accept` are both treated as pass. Allows CI/automated runs.
  - id: tasks
  - id: bridge
    runner: derrick               # creates tickets in the substrate
    inputs: [{{tasks_md}}]
    batch: "{{batch}}"
    # Bridge auto-remediation: if a ticket for this feature already exists
    # in a terminal state (Done/Cancelled), bridge deletes and recreates it.
    # If an active (non-terminal) ticket already exists for the feature,
    # bridge skips creation to avoid duplicates.
  - id: foreman
    runner: derrick               # starts the foreman loop
    executor_role: executor       # which role hands run as

# Project-specific guardrails surfaced into prompts and checkpoints
guardrails:
  constitution_path: .specify/memory/constitution.md
  forbid_paths: []         # paths that may not be touched by a feature
  required_labels: []      # labels every ticket must carry

# Parallelism budgets (see §9.C)
parallelism:
  batch_max: 8         # max hands / copilot agents in flight at once
  step_max:   4        # max parallel sub-tasks within one pipeline step
  assay_max:  2        # max concurrent reviewers in multi-reviewer assay

# Where derrick writes its own state inside the repo
state:
  dir: .derrick
  log_runs: true
  worktree_root: .derrick/worktrees   # per-run isolation (§9.C.5)
```

Resolution rules:

- Repo `derrick.yaml` wins.
- Falls back to `~/.derrick/config.yaml` for user defaults
  (preferred model, assay rounds, etc).
- Falls back to a baked-in default shipped with the binary.

Templates use a simple `{{var}}` substitution (no general
expression language) with a small fixed context: `prompt`,
`site_name`, `site_prefix`, `feature_dir`, `tasks_md`,
`batch`, `run_id`. (Older drafts referenced a `rig` variable;
that's vestigial gastown vocabulary — site_name / site_prefix
are the canonical names per D27.)

---

## 5. The flows

### 5.1 Install (one-time, per machine)

```
$ curl -fsSL https://raw.githubusercontent.com/lgulliver/derrick/main/scripts/install.sh | bash
```

Script does, in order:

1. Detect OS/arch, fetch the right `derrick` binary into `~/.local/bin`.
2. Run `derrick doctor --install` which:
   - Verifies `claude`, `codex`, `gt`, `bd`, `git` are present.
   - If a tool is missing, prints the canonical install command (does
     **not** install it silently — these are auth-bearing tools).
   - Installs the `derrick` Claude Code plugin (`/drill`,
     `/derrick-doctor`) into `~/.claude/plugins/`.
   - Writes `~/.derrick/config.yaml` with sensible defaults.
3. Prints next-step: `cd your/repo && derrick init`.

No repo touched at this stage.

### 5.2 Init — the setup wizard (one-time, per repo)

```
$ cd ~/repos/my-project
$ derrick init
```

`derrick init` is a **full interactive wizard**. On first run it shows the
derrick splash screen (the logo, version, tagline), then walks the user
through seven steps. UX target: the same polish and terminal aesthetic as the
speckit install experience — styled prompts, colour, spinner states, progress
markers. Users should feel like they're using a well-made product (Railway CLI
/ Vercel CLI quality bar).

The wizard never writes anything until the user confirms at the end of each
phase. Every step that would mutate state shows a dry-run preview first.
`--dry-run` globally opts out of writes for scripting.

See §5.6 for the full brownfield adoption contract; the wizard honours it in
full.

#### Step 1 — Prerequisite check

Derrick checks for every required tool before doing anything else:

- `git` — required.
- `gh` — required for GitHub features (Issues, PRs, squash-merge check).
- `speckit` / `specify` CLI — required for the pipeline.
- Any AI provider CLI the user intends to use (`claude`, `codex`, `copilot`,
  `opencode`, etc.) — checked against what the user selects in step 3.

**If anything is missing:** print exactly what is missing, the canonical
install command for each, and exit. Do not continue. The user must install the
missing tools and re-run `derrick init`. There is no partial-init mode that
skips a failing prerequisite.

#### Step 2 — Git repo check

If the current directory is not inside a git repository:

> *"This directory is not a git repository. Would you like Derrick to
> initialise one here?"*

- **Yes** → run `git init`, then run the correct `speckit init` command for a
  fresh repo (per speckit's own documented init flow — derrick does not
  invent speckit commands). Continue to step 3.
- **No** → print a polite cancellation message and exit. Derrick never
  operates outside a git repo.

#### Step 3 — Provider configuration

Show which AI providers are currently installed / available (detected from
PATH + known API key env vars). For each pipeline role (`proposer`, `drafter`,
`reviewer`, `executor`, `summariser`) show:

- Which providers are available.
- A recommended default per role with a one-line rationale (e.g. "Opus for
  proposer — this is the planning role; heavy reasoning matters here").
- A terminal radio-button or multi-select UI for the user to confirm or
  override the recommendation for each role.

Write the resolved role → model → provider bindings into `derrick.yaml` once
the user confirms.

#### Step 4 — Commit and branching conventions

Ask two questions with sensible defaults:

1. *"Use conventional commits?"* (yes / no, default: yes)
2. *"Branch naming prefix?"* (text input, default: `feat/`; accepts e.g.
   `fix/`, `chore/`, empty for no prefix)

Use the answers to configure speckit accordingly (write into
`.specify/` config — do not invent speckit config keys; use the ones speckit
actually supports).

#### Step 5 — Constitution creation

Guide the user through creating the speckit constitution **without leaving
derrick**. Derrick drives the speckit constitution flow in-process or via the
detected host CLI — the user never needs to know which underlying command is
running.

- If `speckit` is installed and a host CLI is available: run
  `/speckit.constitution` via the host. Stream output back to the user's
  terminal. Wait for completion before continuing.
- If `speckit` is installed but no host CLI is detected: run
  `specify constitution --here` (or the equivalent documented speckit command)
  directly.
- If neither is available: this step was caught in step 1; it cannot be
  reached.

Brownfield repos with an existing constitution-like file (`PRINCIPLES.md`,
`STYLE.md`, `CONTRIBUTING.md`, an existing `.specify/memory/constitution.md`):
skip this step; reference the existing file via `guardrails.constitution_path`
and tell the user which file was adopted.

**Constitution seeding**: when no existing constitution is found and speckit
is not available for interactive authoring, the wizard prompts the user
directly for their team constitution content (coding standards, review
expectations, commit conventions, out-of-scope guardrails). The entered text
is written verbatim to `.specify/constitution.md`. The banner stub is **not**
used — a seeded constitution is real content and the pipeline accepts it
immediately. `derrick init --constitution-stub` still writes the banner stub
for users who prefer to author it separately.

#### Step 6 — Bootstrap (write everything)

Only after the user confirms the full plan:

- `derrick.yaml` from template, pointing at existing paths wherever found.
- `.specify/extensions/derrick/scripts/tasks-to-tickets.sh`.
- `.claude/commands/drill.md` — refuses to overwrite without `--force`.
- `.claude/agents/` additions only — names that collide with existing agents
  are skipped and reported.
- `CLAUDE.md` block appended only with `--append-agents-md` or explicit
  confirm in the wizard.
- Register the site with the native SQLite substrate (`--no-substrate` opts
  out).
- Write host hooks for scrub + caveman at every model boundary (D29):
  `.claude/settings.json` PreToolUse / PostToolUse. Codex hooks deferred
  (D34); T011 writes `.codex/instructions.md` only. Brownfield: refuse to
  overwrite existing hook entries, surface a merge plan instead. `--no-hooks`
  opts out entirely.

#### Step 7 — Completion

Print a success screen showing what was written, then show the next step:

```
  derrick drill "your first feature"
```

**Initial commit**: after writing all files, if the repo has no commits yet
(no `HEAD` ref), the wizard runs `git add -A && git commit -m "chore: derrick
init"` automatically. This ensures the repo has a valid `HEAD` before the
first `derrick drill` run, which requires a commit to create a worktree. The
commit message follows the conventional-commits setting chosen in step 4.

Run `derrick doctor` automatically to confirm the environment is healthy before
the user leaves init.

### 5.2.1 Speckit detect-then-defer

- If `specify` CLI is on PATH and the user's host CLI accepts the
  `/speckit.*` slash commands, derrick prefers them: it runs
  `specify init --here` for the skeleton and `/speckit.constitution`
  via the host for the actual constitution authoring.
- If neither is available, derrick ships a minimal `.specify/`
  skeleton (templates, scripts, empty constitution file with a
  banner: *"Run `/speckit.constitution` to author this."*) and
  refuses to run the pipeline until the constitution file has had
  the banner removed.

`derrick init --greenfield` is the opt-in for an empty repo where
derrick may write authoritatively.

### 5.2.2 `derrick switch` — upgrade solo mode to crew

```
$ derrick switch
```

Upgrades a repo that was initialised in `mode: solo` to `mode: crew`
without wiping the existing config. It:

1. Verifies the current mode is `solo` (errors if already `crew` or
   `copilot`).
2. Adds a `peers:` stanza to `derrick.yaml` prompting the user to
   list collaborating sites/machines (optional but common in crew mode).
3. Patches `tools.substrate.mode` from `solo` to `crew` in
   `derrick.yaml`.
4. Registers the site with the native substrate if it was not
   registered (no-op if already present — idempotent).
5. Writes (or updates) the foreman configuration block in
   `derrick.yaml` with crew-appropriate defaults
   (`tools.foreman.poll_interval`, `hand_ttl`, `in_review_ttl`).
6. Prints a summary of what changed and the next step
   (`derrick foreman start`).

`--dry-run` previews the yaml diff without writing it.
`--mode copilot` switches to `copilot` mode instead of `crew`.

### 5.3 `derrick drill` — the feature pipeline

```
derrick drill "build a webhook ingest endpoint with idempotent dedupe"
```

Or via Claude Code:

```
/drill build a webhook ingest endpoint with idempotent dedupe
```

The slash command resolves to `derrick run drill --prompt "..."`. The
user never sees the underlying tools. All output and questions are in
**derrick's voice**.

The feature prompt may also be supplied from a file (`--prompt-file <path>`)
or from stdin (`-` sentinel, or piped) on both `derrick drill` and
`derrick run drill`, so a large multi-line `/speckit.specify`-style brief
can be passed without shell-escaping (D64). The three sources fold into the one
prompt string that feeds the `specify` step; supplying more than one explicit
source is a usage error.

The pipeline runs eight stages in sequence. Each stage that surfaces output
to the user does so in a clean, styled terminal UI — same quality bar as
§5.2's wizard. Stages that block on user input wait indefinitely; stages that
run autonomously show a spinner and then a summary.

Every step logs to `.derrick/runs/<utc-ts>/step-<id>.log`. On host-backed
steps, the host adapter in `derrick-tools` shells to the host CLI with the
step's command and current working directory (D30). The host loads its own
context. Failure of any step halts the pipeline with a numbered error and the
exact resume command (`derrick run drill --resume-from <step>`).

Each step emits two substrate events scoped to `EventScope::Worktree { run_id }`:
`PipelineStepStarted { step_id, index, total }` at step entry and
`PipelineStepCompleted { step_id, status }` at step exit (D77). The
`PipelineStepStarted` event bridges the live `ProgressReporter` plane (D60/D61,
seen only by the launching terminal) into the persisted event log, so
`derrick observe` can show mid-step liveness without polling the launching
process. Live per-line agent output is not duplicated into the event log
(volume); the TUI tails the per-step `.log` file for that (D78).

The `add` subcommand is a hidden, deprecated alias for `drill`; the runner
also accepts `"add-feature"` as a deprecated `pipeline_id` so existing run
manifests stay resumable. See D64.

#### Stage 1 — Specify

Run `speckit specify` internally. When complete, show the user a clean summary
of what derrick understood from the prompt:

> *"Here is what I understood. Is this correct?"*

Wait for an explicit **yes** or **no** before continuing. If no: accept
free-text correction and re-run specify with the correction appended. If yes:
proceed. Derrick reads `.specify/feature.json` after this stage to pin
`feature_dir` for subsequent steps (solves the stale feature.json bug).

#### Stage 2 — Plan

Run `speckit plan` internally. Show the user a brief human-readable summary of
the plan (not the raw speckit output). No confirmation required at this stage —
the assay in stage 5 is the formal gate.

#### Stage 3 — Clarify

Run `speckit clarify` interactively. Stream the questions to the user's
terminal and capture their answers in-session. The user types answers directly;
no special UI required — plain readline input is fine.

#### Stage 4 — Apply clarifications

Apply the user's clarification answers to the plan. This is an in-process
derrick step (no host CLI invocation). Show the user a diff-style summary of
what changed in the plan as a result of their answers.

#### Stage 5 — Assay

Run cross-model adversarial review (courtroom internally). Show the user a live
view of:

1. **Current state** — the plan being reviewed.
2. **Objections raised** — what the reviewer found, and why.
3. **Rebuttals** — how the proposer responded.
4. **Verdict** — accept / reject, with a one-paragraph rationale.

If the assay rejects after the configured number of rounds: halt and surface the
final report to the user. Do not proceed automatically on a rejection.

#### Stage 6 — Summary and confirmation

Show a clean, human-readable summary of the final accepted plan. Ask:

> *"Shall I proceed and begin implementation?"*

Wait for an explicit **yes** or **no**. This is the last human checkpoint
before autonomous work begins. If no: exit cleanly with the plan saved so the
user can resume later.

#### Stage 7 — Task and agent generation

On yes:

1. **Generate tasks** — run `speckit tasks` internally. Show a numbered list
   of the generated tasks.

2. **Generate sub-agent files** — write appropriate host-specific agent `.md`
   files for the providers and hosts selected during `derrick init`. This
   means creating or updating `.claude/agents/`, `.codex/agents/`,
   `.opencode/agents/`, `.github/agents/` entries for any hosts the user has
   configured. File content is derived from the ticket context and the role
   definitions in `derrick.yaml`.

3. **GitHub Issues (optional)** — if a GitHub remote is detected and `gh` is
   available, ask:

   > *"Would you like these tasks created as GitHub Issues?"*

   If yes: create one issue per task via `gh issue create`. Link each issue
   back to the batch in the substrate. If no: skip silently.

#### Stage 8 — Begin work (detachable)

Start the foreman and dispatch hands to begin executing tickets:

- Show a live progress feed: which tickets are in-flight, which hands are
  running, what just completed.
- **Always commit as tickets complete.** Each completed ticket results in a
  commit on the feature branch before the hand transitions to the next ticket.
  Never leave completed work uncommitted.
- **The process is detachable.** The user can `Ctrl-C` or close the terminal
  at any point without killing the foreman or the hands in flight. Work
  continues in the background. The user can re-attach at any time with
  `derrick observe` or check status with `derrick status`.

Detachment is explicit: when the user hits `Ctrl-C`, derrick prints:

> *"Work is continuing in the background. Run `derrick observe` to re-attach
> or `derrick status` for a snapshot."*

It does not ask "are you sure?" — detach is always safe because the foreman
owns the process.

#### Flags

| Flag | Behaviour |
|---|---|
| `--no-clarify` | Skip stage 3 (clarify) and stage 4 (apply) |
| `--no-assay` | Skip stage 5 (adversarial review) |
| `--dry-run` | Run through task generation (stages 1–7); do not start foreman or create tickets |
| `--phase <label>` | Apply a phase label to every ticket in the batch |
| `--resume-from <step>` | Restart from a given pipeline stage |
| `--no-github-issues` | Skip the GitHub Issues offer in stage 7 |
| `--detach` | Skip the live progress feed in stage 8; go straight to background |

### 5.4 `derrick doctor`

Inspects the local install + the current repo and prints a coloured
checklist:

- Binaries: `claude`, `codex`, `copilot`, `opencode`, `gh`, `git`, `speckit`/`specify` (versions + paths). Any provider CLIs configured in `derrick.yaml` are also checked.
- Claude Code plugin presence and version.
- Repo: `derrick.yaml` valid, constitution file exists and is non-empty, substrate healthy, foreman state consistent.
- Exit code is the count of failing checks (handy for CI).

### 5.5 Observability — derrick as the front door for "what's going on?"

Once a feature is in flight, the user shouldn't need to remember
which underlying tool answers which question. Derrick exposes a
flat, predictable surface that aggregates substrate, git, and
host-CLI reads into one view. Everything here is **read-only** —
these commands never mutate state.

All commands talk to the `Substrate` trait. Output uses derrick's
vocabulary regardless of backend (v1 ships only the native one).

| Command | What it shows | Substrate calls |
|---|---|---|
| `derrick status` | Dashboard: site health, active batch, tickets by state, foreman session, last assay verdict | `Site.Health`, `Batch.Current`, `Ticket.List` |
| `derrick status --watch` | Same, live-refreshing every N seconds | tick loop |
| `derrick tickets [filter]` | List tickets with state, owner, labels, age. Filters: `ready`, `in-flight`, `blocked`, `done`, `mine`, `batch=<name>`, `phase=<label>` | `Ticket.List` |
| `derrick ticket <id>` | Full detail on one ticket — body, comments, blockers, history, hand assignment, PR link | `Ticket.Get` |
| `derrick batch [name]` | Batch state: order, blockers, who's working what, ETA estimate | `Batch.Get` |
| `derrick foreman` | Foreman session status, current focus, recent escalations | `Foreman.Status` |
| `derrick activity [--site \| --batch]` | Recent agent activity timeline | `Event.Tail` |
| `derrick hands` | Hands registered to this site, current task, last heartbeat, pid (D75) | `Hand.List` |
| `derrick orphans` | Lost work (tickets with no live owner) | `Ticket.Orphans` |
| `derrick stack [--batch <name>]` | Current PR stack: parent→child PRs, merge state, restack health | `Stack.Show` |
| `derrick runs` | Last N derrick pipeline runs, exit status per step | local `.derrick/runs/` |
| `derrick run <id>` | Replay the manifest of one specific run | local |

(v1 has no `derrick mail` — agent mail is out of scope. A future
backend that wraps a system with mail can add it behind the trait.)

Design rules for the observability surface:

- **Scrubbed by default.** Output goes through `internal/scrub`
  (§9.2) so a copy-paste into a Claude prompt is already token-tight.
  `--raw` opts out.
- **Caveman-aware.** `derrick status --caveman` produces a
  one-screen summary good for pasting into stand-ups or feeding back
  into `/drill` resume contexts.
- **Mode-aware.** In `mode: solo` most of these collapse — `derrick
  status` shows the current spec dir and tasks.md progress, no
  tickets. In `mode: copilot` it shows Copilot agent dispatch
  state, no foreman. In `mode: crew` it shows the lot.
- **No mutation.** The observability commands never mutate state.
  Write paths are surfaced through the explicit mutation API
  (`derrick ticket done/review/block/reopen`, §8.2) — not
  through the status/observe surface.
- **JSON when piped.** `--format json` (or auto-detected from
  non-TTY) emits structured output for scripting and for the future
  `derrick observe` TUI.

`derrick status` example output (crew mode, mid-flight):

```
$ derrick status
site         taxi-ingest                            mode: crew
batch        001-webhook-ingest      11 tickets     3 done • 2 in-flight • 6 ready
foreman      running (pid 28411, 14m)               last escalation: none
backend      native                                 orphans: 0
stack        native      3 PRs merged • 2 open • 6 pending     no restack conflicts
last assay   2026-05-17 09:18  →  accept (round 2)  by codex/gpt-5

in flight:
  ti-50  ▸  hand:bramble       storage layer with idempotent dedupe   12m
  ti-51  ▸  hand:sumac         replay-safe migration                   4m
ready next:
  ti-52     handler wiring                  blocked by: ti-50
  ti-53     contract test for /ingest      blocked by: ti-50, ti-51
  …
```

This is what users actually want at 09:30 standup. One command.

### 5.6 Brownfield adoption — `derrick init` on a real repo

Many target repos already have an `AGENTS.md`, a `CLAUDE.md`, an
existing `.claude/` directory, written conventions, an issue
tracker, and house style. `derrick init` must **adopt, not
overwrite**. It runs an adoption pass *before* writing anything:

1. **Detect.** Walk the repo for: `AGENTS.md`, `CLAUDE.md`,
   `CODEOWNERS`, `.claude/`, `.github/copilot-instructions.md`,
   `docs/adrs/`, existing `.specify/`, existing constitution-like
   files (`PRINCIPLES.md`, `STYLE.md`, `CONTRIBUTING.md`), and
   linked tracker prefixes (e.g. `LIN-`, `JIRA-`, `BD-`).
2. **Classify.** For each artifact, decide: *adopt as-is*,
   *reference from derrick.yaml*, or *augment*. Nothing is moved
   or rewritten unless the user opts in.
3. **Propose.** Print the proposed `derrick.yaml` and the *list
   of files derrick would create or append to* (nothing else). The
   user reviews, accepts or edits, then derrick writes.

**Known gap (OQ6):** merges of `hooks`/`.mcp.json`/
`.claude/settings.json` are computed against the snapshot captured
at step 1 (Detect) and are not re-read from disk before step 3's
write (Promote). An external edit to any of those files between
detect and apply is silently lost. A detect→merge revalidation
pass — re-read on-disk state immediately before promotion and
refuse or re-merge if it changed — is the leaning fix; not yet
built. See §12 OQ6.

Concrete behaviours:

| Existing | Derrick's default behaviour |
|---|---|
| `AGENTS.md` | Reference it from `guardrails.agents_md`. Do not overwrite. Append a short derrick block at the bottom (opt-in via `--append-agents-md`). |
| `CLAUDE.md` | Same — referenced, optionally appended. |
| `.claude/agents/<name>.md` | Treat as authoritative. Do not create `hand-default.md`, `foreman.md` etc. if names overlap. |
| `.claude/commands/` | Add `/drill` and friends *alongside*. Refuse to overwrite an existing command of the same name without `--force`. |
| `.claude/skills/` | Untouched. Derrick's skills are added separately. |
| Existing constitution-like file | Reference it as `guardrails.constitution_path`. Do not write a new one. |
| Existing `.specify/` | Reuse. Patch only `.specify/extensions/derrick/`. |
| No constitution at all | Offer to generate a *minimal stub* (`derrick init --constitution-stub`) or run a one-shot LLM pass that drafts one from existing docs (`--constitution-from-docs`). Both opt-in. |
| Existing tracker (Linear, Jira, GitHub Projects) | Skip native substrate ticket creation; offer a future adapter (out of scope for v1, recorded as a constraint). |
| Existing CI / pre-commit / git hooks | Untouched. |
| Existing `.claude/settings.json` hooks | Adopt-additively. Derrick adds its `PreToolUse`/`PostToolUse` entries (D29) **before** existing ones in the array, marked with a `"description"` field (`"description": "derrick:scrub"` for PreToolUse, `"description": "derrick:caveman"` for PostToolUse — JSON has no line comments, so the marker is a real field that Claude Code preserves on unknown keys). Refuses to overwrite; refuses without `--force` if the user has conflicting entries on the same tool. |
| Existing `.codex/instructions.md` | Adopt-additively: append a derrick block; the user's own content is preserved. No `.codex/` hook installation per D34. |

Switches:

```
$ derrick init                          # interactive, brownfield-safe default
$ derrick init --greenfield             # current behaviour: assumes empty slate
$ derrick init --dry-run                # print plan, write nothing
$ derrick init --adopt-only             # write derrick.yaml + .derrick/ only;
                                        # don't touch .claude/, AGENTS.md, etc.
$ derrick init --constitution-from-docs # one-shot LLM draft from existing docs
$ derrick init --import-tasks <file>    # seed the substrate with existing tasks
```

The brownfield path is the default because most real repos are
brownfield. Greenfield is the explicit opt-in.

### 5.7 `derrick observe` — the TUI dashboard

`derrick status` is a one-shot snapshot (`--watch` refreshes the
whole screen). `derrick observe` is an interactive, persistent
TUI you leave open in a tmux pane while work runs. Same data,
much higher information density, and you can drill in without
typing another command.

Built on **ratatui** + **crossterm**. Lives in `crates/derrick-tui`.
Reads exclusively through the `Substrate` trait and the local run
manifests — no mutations from the TUI in v1, so a runaway TUI
can't damage state.

#### Layout

```
┌── derrick · site: taxi-ingest · mode: crew · backend: native ────────── 09:47 ─┐
│  [1] Overview  [2] Tickets  [3] Stack  [4] Activity  [5] Tokens  [6] Memory      │
│  [7] Hands     [8] Factory                                                        │
├──────────────────────────────────────────────────────────────────────────────────┤
│  Active batch  001-webhook-ingest                                                │
│  ▰▰▰▰▰▱▱▱▱▱▱   3 / 11 done · 2 in-flight · 6 ready · 0 blocked                  │
│  Foreman       running (pid 28411, 14m)         escalations: 0                  │
│  Stack         native  ● 3 merged · 2 open · 6 pending     restack: ok           │
│  Last assay    accept (round 2) · codex/gpt-5 · 09:18                            │
│  Tokens today  raw 312k → actual 41k  (-87%)                                     │
│                                                                                  │
│  In flight:                                                                      │
│    ti-50  ▸ hand:bramble    storage layer with idempotent dedupe        12m04s  │
│    ti-51  ▸ hand:sumac      replay-safe migration                        4m12s  │
│  Ready next:                                                                     │
│    ti-52    handler wiring                              blocked by ti-50         │
│    ti-53    contract test for /ingest                   blocked by ti-50,51      │
├──────────────────────────────────────────────────────────────────────────────────┤
│  q quit   r refresh   ↑↓ nav   ⏎ open   / search   ? help                        │
└──────────────────────────────────────────────────────────────────────────────────┘
```

#### Tabs

1. **Overview** — the screen above. The 09:30-standup view.
2. **Tickets** — a sortable, filterable table. Filters mirror the
   CLI (`ready | in-flight | blocked | done | mine`). `⏎`
   opens a ticket detail pane (body, blockers, hand history, PR
   link, recent comments).
3. **Stack** — current PR graph as an ASCII tree. Shows merge
   state per PR; flags `restack-conflict` tickets in red.
   `⏎` on a node opens the PR URL in the user's browser.
4. **Activity** — live tail of the event log
   (substrate `events` table). Filter by ticket, hand, run id.
5. **Tokens** — `derrick gain --pillars` rendered live. Per-step
   cost, model-tier breakdown, savings attribution to each of
   §9.B's knobs. Roughneck savings (`roughneck_tokens_saved`) and
   scrub savings (`bytes_saved`) are shown per step alongside the
   standard caveman/tiering/caching breakdown.
6. **Memory** — current site's memory entries (project /
   reference / feedback / lessons). Lets the user spot stale or
   wrong entries; `d` flags one for deletion (writes to a queue,
   not applied until the user runs `derrick memory prune`).
7. **Hands** — per-hand rollup: hand id, kind, current ticket,
   last heartbeat, pid (D75), and a status glyph
   (`✓ done / ✗ failed / ⟳ running`) derived from the structured
   telemetry events (D76) rather than free-text `Note` matching.
8. **Factory** — an ASCII factory floor of animated workers
   (D78). Workstations = active worktree rows; each worker avatar
   is a unicode glyph chosen by `HandKind`
   (claude/copilot/codex/opencode/aider/human). Worker animation
   states are driven by recent events on `EventScope::Hand`:
   `HandStarted` → worker arrives; throttled `HandProgress` →
   worker "hammering" (braille frame cycle);
   `TicketTransitionedToInReview` → box placed on conveyor;
   `HandExited` → worker leaves; `TicketVerifiedMerged` → box
   ships to dock. The conveyor is rendered from the `links`
   dependency graph (`Blocks` edges); the shipping dock counts
   merged PRs; the smokestack puffs when the foreman is
   Attached/Detached and idles when Stopped. For per-line agent
   output the tab tails `.derrick/runs/<id>/step-<id>.log` rather
   than expanding the event log. Read-only — never mutates state.

#### Live updates

Three mechanisms, all running:

- **File watcher** (notify crate) on `.derrick/derrick.db`,
   `.derrick/runs/`, `.derrick/foreman.pid`. Fires the moment
   anything changes.
- **Tick timer** (1s) as a fallback for things the watcher
   doesn't surface (e.g. age counters in the in-flight pane,
   remote PR state freshness, substrate event tail).
- **Animation tick** (~100ms) for the Factory tab only (D78).
   Drives per-worker frame counters and conveyor motion; substrate
   polling still happens at the 1 Hz cadence above, so the
   animation tick is pure local state (no substrate reads).
   ratatui's diff-based rendering keeps the 10× redraw cheap.

Refresh is incremental — only the affected pane redraws, not the
whole screen. Important for long-running tmux sessions.

#### Invocation

```
$ derrick observe                       # default: opens on Overview tab
$ derrick observe --tab stack           # jump straight to a tab
$ derrick observe --site <name>         # in case the user runs from outside the repo
$ derrick observe --read-only           # belt-and-braces; refuses any future write features
```

The TUI is the *only* path that ever shows multi-tab live data
in one place. `derrick status --watch` remains the headless
equivalent for tmux purists and CI logs.

---

## 6. The Claude Code plugin

Shipped alongside the binary, installed by the install script into
`~/.claude/plugins/derrick/derrick/1.0.0/`.

Contents:

```
.claude-plugin/plugin.json
commands/
  drill.md                  # the primary UX
  derrick-status.md         # wraps `derrick status` (caveman-formatted)
  derrick-doctor.md         # wraps `derrick doctor`
  derrick-resume.md         # wraps `derrick run --resume-from`
skills/
  drill/SKILL.md            # full phase-by-phase instructions
README.md
```

`commands/drill.md` is intentionally thin: it parses arguments,
verifies derrick is installed, and then defers to the skill for the
actual workflow narrative — same pattern the Anthropic-shipped skills
use (a one-page command, a fat skill).

### 6.5 Hosts, models, roles (D65)

Derrick separates three concerns most tools conflate:

- **Host** — *who executes the work*. One of exactly five host CLIs:
  `claude` (Claude Code), `codex`, `copilot` (GitHub Copilot CLI),
  `opencode`, `aider`. The host loads its own context (AGENTS.md,
  sub-agents, skills, plugins) and manages its own auth. Derrick
  holds no API keys. Hosts are configured per pipeline step
  (`host: claude`) or implied by the provider name.
- **Provider** — in `derrick-models`, a provider is a named
  **host-delegated wrapper** that maps a `ModelDef` to one of the
  five hosts, builds a `HostRequest`, and calls the `derrick-tools`
  host adapter. Provider names match host names (`claude`, `codex`,
  `copilot`, `opencode`, `aider`). The `shell` provider survives as
  a bespoke-envelope escape hatch for arbitrary command-line tools
  that speak the structured prompt envelope protocol.
- **Role** — *what the step needs done*. `proposer`, `drafter`,
  `reviewer`, `executor`, `summariser`. Pipeline steps name roles,
  never models directly.

The binding is `step → role → model → provider (= host)`.
Changing the reviewer from codex to a copilot-backed model is one
line in `models:`; no pipeline edits.

#### Five hosts

All model inference runs through exactly one of these:

| Host | Auth | Default model | Notes |
|---|---|---|---|
| `claude` | Claude Code's own auth | `claude-opus-4-8` | Anthropic models. Strips a leading `anthropic/` prefix from model ids before passing `--model`. |
| `codex` | Codex CLI's own auth | `gpt-5.5` | OpenAI models. Strips a leading `openai/` prefix from model ids. |
| `copilot` | GitHub Copilot CLI's own auth | `gpt-5.4` | Multi-model. Strips any `provider/` prefix; keeps dotted ids (e.g. `claude-sonnet-4.6`) — no dot↔dash translation. |
| `opencode` | opencode's own auth | — | Multi-model; expects `provider/model` strings. Passes id verbatim. |
| `aider` | aider's own auth | — | Multi-model; expects `provider/model` strings. Passes id verbatim. |

Provider names `anthropic`, `openai-cli`, `copilot-cli` are
migration aliases only — they are mapped at config load time and
a deprecation warning is emitted. Use the host names above in new
configs.

The `shell` provider is not a host. It accepts any command that
reads a structured prompt envelope on stdin and writes a
sentinel-delimited response on stdout. Use it for bespoke tooling
not covered by the five hosts.

#### Role taxonomy

| Role | Typical host | What it does |
|---|---|---|
| `proposer` | `claude` | Generates the initial plan / spec |
| `drafter` | `claude` | Writes or revises artifacts |
| `reviewer` | `codex` | Cross-examines plans in assay; different family from proposer by default (D5) |
| `executor` | `copilot` / `aider` / `opencode` | Implements tickets as a hand |
| `summariser` | `claude` (haiku tier) | Compresses step output for memory / telemetry |

These are defaults. Any role can bind to any host; the only
constraint `derrick doctor` enforces is that `proposer` and
`reviewer` do not share the same provider (§7).

#### Model catalogue and normalisation (D65)

A curated per-host catalogue lives in
`derrick-tools/src/catalogue.rs`. It drives:

- **Defaults** used by `Config::defaults()` and `derrick init`.
- **`derrick models check`** — the implemented validation command
  (D15). Checks: (1) host binary installed → FAIL if missing;
  (2) model id in catalogue → WARN-only if not; (3) provider maps
  to one of the five hosts → FAIL if not; (4) opencode/aider model
  string contains `/` → WARN if not. Exit code equals the count of
  FAILs. WARN conditions never block the pipeline.
- **Per-host normalisation** applied inside each host adapter
  before the `--model` argument is pushed (see Five hosts table).

Current catalogue (May 2026):

| Host | Known ids |
|---|---|
| `claude` | `claude-opus-4-8`, `claude-sonnet-4-6`, `claude-haiku-4-5` |
| `codex` | `gpt-5.5`, `gpt-5.4`, `gpt-5.4-mini`, `gpt-5.2-codex` |
| `copilot` | `gpt-5.4`, `gpt-5.3-codex`, `claude-sonnet-4.6`, `claude-haiku-4.5`, `gpt-5.4-mini` |
| `opencode` | A small curated set of `provider/model` strings; unknown ids WARN |
| `aider` | A small curated set of `provider/model` strings; unknown ids WARN |

Unknown model ids produce a warning and still pass through to the
host CLI. Derrick never hard-fails solely because a model id is
not in the catalogue — the host's own error is the source of truth.

Soft warnings are also emitted at `derrick init` and `derrick run`
so issues surface early without blocking experiments.

#### Respecting the host's own rules

When derrick invokes a step on a host CLI it deliberately does
**not** inject a system prompt, override the host's context, or
bypass the host's rule loading. The contract:

- Derrick passes the working directory (the user's repo) and the
  step command. If `roles.<role>.agent` is configured, derrick
  reads that repo-local agent file and prepends it to the step as
  explicit user-configured role context.
- The host loads its **own** files: `CLAUDE.md`, `AGENTS.md`,
  sub-agents under `.claude/agents/`, skills under
  `.claude/skills/`, plugins, hooks, `.codex/`, `~/.codex/`,
  `.github/copilot-instructions.md`, `.opencode/agents/`, etc.
  Derrick does not touch any of this.
- Sub-agent spawn within a step is the host's decision; derrick
  doesn't see it and doesn't intercede.
- Skills triggered in-session (caveman, find-skills, the user's
  custom skills) run inside the host. Derrick's own caveman
  implementation is for inter-step compression, not in-session.
- Derrick records what command it sent and what artifact came
  back. It does not influence the host's internal context,
  prompt expansion, or sub-agent behaviour. It reads the host's
  transcript file after the step for token telemetry only
  (§9.B.7) — read-only, post-hoc, never used to alter the host's
  next call.

A brownfield repo with a carefully tuned AGENTS.md and twenty
agents gets exactly the same Claude Code behaviour inside a
derrick step as it would in a normal session. Derrick is the
conductor, not the orchestra.

#### Per-role providers and agent files

The short role form remains the default:

```yaml
roles:
  reviewer: codex-gpt5
```

Use the expanded form to keep provider/model selection and the
role's agent file together:

```yaml
roles:
  reviewer:
    model: codex-gpt5
    agent: .codex/agents/integrations-engineer.md
  executor:
    model: copilot
    agent: .github/agents/flow-engineer.md
```

`model` is the existing model alias, so switching providers is still
one edit under `models:` or `roles:`. `agent` is a path relative to
the step working tree unless absolute. Derrick reads the file and
includes it as role context for that step; host CLIs still load their
own standard files (`AGENTS.md`, `.codex/instructions.md`,
`.github/copilot-instructions.md`, etc.) normally.

**One narrowing (D66/D67)**: model selection is the single dimension
derrick now asserts on the run path. The user picks the HOST (the
executor role's `ModelDef.provider`); the foreman picks the best
MODEL within that host per ticket by tier. A role's model id may be
a concrete PIN (always used as-is) or the `auto` sentinel
(`auto:light`, `auto:standard`, `auto:heavy`, or plain `auto`).
Plain `auto` maps to the tier that matches the ticket's
`Ticket.complexity` (Low→light, Standard→standard, Heavy→heavy;
missing → standard). `auto:*` is a hard tier override. The per-host
tier mapping lives in `derrick-tools/src/catalogue.rs` (D65). An
`auto*` value is never forwarded to the CLI — it is resolved to a
concrete model id before `HostRequest` is built. All five crew hands
are local CLIs and all participate in tier selection; the cloud
Copilot issue-dispatcher is not wired as a crew hand. When no model
is configured the host keeps its own default, preserving the
no-configuration-needed path. Everything else — context, agents,
hooks, sub-agent spawn — remains the host's domain. See D66 and D67
for the full scope.

Codex host hooks are deferred (D34). aider's headless flags
(`--yes-always --no-auto-commits --no-stream --no-pretty
--no-show-release-notes`) are always on for pipeline runs;
opencode and aider hook instrumentation is a documented gap with
the same posture as D34.

#### Cost and latency knobs per model

`models.<name>` accepts:

```yaml
models:
  claude-opus:
    provider: claude
    model: "claude-opus-4-8"
    max_tokens: 4096
    temperature: 0.2
    timeout: 120s
    rate_limit: { rpm: 20, tpm: 80000 }
    cost_hint:  { in_per_mtok: 15, out_per_mtok: 75 }   # for `derrick gain`
```

Fields `endpoint`, `region`, `deployment`, and `base_url` are
parsed-and-ignored (with a one-line deprecation warning) so
existing `derrick.yaml` files continue to load after D65. The
`cli` field is deprecated for the host providers
(`claude`/`codex`/`copilot`/`opencode`/`aider`) and ignored
there, but remains in use by the `shell` escape-hatch provider,
which still spawns the configured command. No `CONFIG_VERSION`
bump.

Cost hints are optional but power the §9.B.7 telemetry — without
them, `derrick gain` reports token counts only, not dollars.

#### Auth (D65 — no BYOK)

Every host CLI manages its own auth. Derrick holds no API keys.
`AuthStore` is env-passthrough only: env vars (e.g. `GH_TOKEN`,
proxy vars) in `HostRequest.env` are forwarded to the child
process. There is no `~/.derrick/credentials.yaml`, no
`derrick auth` subcommand, and no `MissingCredential` error.

If a host CLI is not authenticated, it will fail on its own terms
and that failure surfaces as a step error in the pipeline log.

### 6.5.1 Runtime-based AI architecture (D79)

D79 generalises §6.5's fixed five-host model into an open runtime
registry without changing the default CLI path.

#### Runtime ≠ Provider ≠ Model

Three concepts that must not be conflated:

- **Runtime** — *how* derrick invokes the model. One of:
  `claude-cli`, `codex-cli`, `copilot-cli`, `opencode-cli`,
  `aider-cli` (the five CLI runtimes; default path, unchanged);
  `anthropic-api`, `openai-api` (direct API; opt-in);
  `openai-compatible` (any OpenAI-protocol endpoint; opt-in);
  `ollama` (local; opt-in); `shell` (bespoke envelope; unchanged).
- **Provider** — *who* serves the model. Examples: `anthropic`,
  `openai`, `openrouter`, `ollama`. Provider is metadata — it
  informs auth strategy and cost hints but does not determine
  invocation path. Invocation path is the runtime's job.
- **Model** — the model identifier, forwarded to the runtime
  untouched. Derrick holds no alias tables. A new model id never
  requires a derrick code change; the host's own error is the
  source of truth.

The binding is `stage → model-alias → {runtime, model, …}`.

#### Runtime registry

Each runtime is a struct implementing a `Runtime` trait that owns
invocation, auth resolution, model-id forwarding, error handling,
streaming, and telemetry:

| Runtime | Invocation path |
|---|---|
| `ClaudeCliRuntime` | `claude --print …` (D65 behaviour, unchanged) |
| `CodexCliRuntime` | `codex …` (D65 behaviour, unchanged) |
| `CopilotCliRuntime` | `copilot …` (D65 behaviour, unchanged) |
| `OpenCodeRuntime` | `opencode run …` (D65/D41 behaviour, unchanged) |
| `AiderRuntime` | `aider …` (D65/D41 behaviour, unchanged) |
| `AnthropicApiRuntime` | Anthropic REST API (opt-in; auth via `auth_env`) |
| `OpenAiApiRuntime` | OpenAI REST API (opt-in; auth via `auth_env`) |
| `OpenAiCompatibleRuntime` | Any OpenAI-protocol `base_url` (opt-in) |
| `OllamaRuntime` | Local Ollama endpoint (opt-in) |
| `ShellRuntime` | Bespoke command via structured envelope (unchanged) |

Adding a new runtime or provider is a registry entry — not an
architectural change. A runtime is selected by the `runtime:` key
in the model-alias block; legacy `provider:` names map to their
`*-cli` counterpart at config load time (see Backward compatibility
below).

#### Config surface

**Structured form** (preferred):

```yaml
models:
  fast:
    runtime: claude-cli
    model: claude-sonnet-4-6
  local:
    runtime: ollama
    provider: ollama
    base_url: http://localhost:11434
    model: llama3.2
  remote:
    runtime: anthropic-api
    model: claude-opus-4-8
    auth_env: ANTHROPIC_API_KEY
    auth_mode: bearer
    params:
      temperature: 0.2
    capabilities:
      prompt_cache: true
      context_window: 200000
```

**Short syntax** (expands to structured form internally):

```yaml
models:
  fast: claude-cli:claude-sonnet-4-6
  local: ollama:llama3.2
```

**Preset** — `ai.preset: cli-defaults` (or `claude-only`,
`codex-only`, `local-only`) generates normal structured config that
the user then edits directly. No hidden behaviour; the preset runs
once at init time.

**Stage bindings** — `stages:` may bind pipeline stages to model
aliases; a stage may declare capability requirements:

```yaml
stages:
  assay:
    model: fast
    requires: [tools, streaming]
```

#### ModelCapabilities

```
ModelCapabilities {
  streaming: bool,
  tools: bool,
  json_mode: bool,
  vision: bool,
  prompt_cache: bool,
  context_window: Option<u32>,
  max_output_tokens: Option<u32>,
}
```

Capabilities are declared in the model-alias block (optional) and
checked at `derrick models check` time. Validation fails only when
a stage's explicit `requires:` list names a capability that is
explicitly declared `false` in the alias block. An undeclared
capability produces a WARN, not a FAIL.

#### `derrick models check` output (D79 revision)

| Condition | Verdict |
|---|---|
| Runtime binary missing | FAIL |
| Required `auth_env` var unset | FAIL |
| Explicit stage `requires:` unmet | FAIL |
| Model id unknown to the catalogue | WARN (pass-through) |
| Capability undeclared | WARN |
| Everything else | PASS |

Exit code equals the count of FAILs. WARN conditions never block
the pipeline.

#### Backward compatibility

Existing configs work unchanged. No `CONFIG_VERSION` bump.

- `provider: claude` → `runtime: claude-cli` (mapped at load time; deprecation warning emitted)
- `provider: codex` → `runtime: codex-cli`
- `provider: copilot` → `runtime: copilot-cli`
- `provider: opencode` → `runtime: opencode-cli`
- `provider: aider` → `runtime: aider-cli`
- `roles:` and pipeline `role:` bindings keep working unchanged.
- `endpoint` / `base_url` fields previously parsed-and-ignored
  (see §6.5 "Cost and latency knobs") become meaningful for
  `openai-compatible`, `ollama`, and API runtimes. For CLI
  runtimes these fields remain ignored (with the existing warning).

#### RuntimeError

All runtimes surface errors through a normalised struct:

```
RuntimeError {
  runtime:   String,   // e.g. "claude-cli"
  provider:  Option<String>,
  retryable: bool,
  message:   String,
  stdout:    Option<String>,
  stderr:    Option<String>,
}
```

`retryable` lets the foreman decide whether to retry the step or
escalate immediately. CLI runtimes set `retryable: false` on auth
failures and `retryable: true` on transient subprocess errors. API
runtimes set `retryable: true` on HTTP 429/5xx.

---

## 7. Assay — derrick-native adversarial review

We considered depending on the existing `courtroom` Claude Code plugin
(it already runs the Claude-proposes / Codex-cross-examines dance). We
chose to build our own equivalent inside derrick instead. Reasons:

- **No third-party plugin contract.** Courtroom is great but its CLI
  shape, output format, and verdict file path can shift without our
  input. Derrick depends on its artifacts every step; we want that
  surface stable.
- **Token-tuned prompts.** Generic courtroom prompts are long and
  thorough. Ours can be terse and structured: "here is the spec, here
  is the plan, name the three biggest risks and the contradiction with
  the constitution if any." Driven by §9 token policy.
- **Pipeline-aware.** Assay sees the exact `feature_dir`, the
  constitution, and the prior step manifest. It knows what we're
  reviewing because it lives in the pipeline.

### Pattern

Same shape courtroom popularised, compressed:

1. **Brief** — derrick assembles a small bundle: `spec.md`, `plan.md`,
   `constitution.md`, and a one-line task statement. No transcripts, no
   prior chat. (Token discipline.)
2. **Prosecution** — Claude has already produced the plan (the `plan`
   step). That artifact *is* the prosecution case. We do not re-run it.
3. **Cross-examination** — derrick invokes `codex exec` (or any
   configured second model) with the brief and a structured prompt
   asking for: top-N risks, missing edge cases, constitution
   violations, and a verdict (`accept | revise | reject`).
4. **Rebuttal** — if verdict is `revise`, derrick reopens Claude
   *once*, scoped to the codex objections only, and asks for a delta
   to `plan.md`. Bounded by `tools.assay.rounds` (default 10).
5. **Deliberation** — after rebuttal, the cross-examiner reviews the
   updated plan in the next loop iteration, continuing the debate
   until accept/reject or round exhaustion.
6. **Verdict** — written to `{{feature_dir}}/assay/verdict.md` with
   the model name, rounds used, and the final accept/revise/reject.
7. **Gate** — on `reject` derrick halts and prints the verdict path.
   On `revise` past `rounds`, derrick prompts the user to extend or
   halt. Constitution violations are parsed from the review and
   enforced as non-negotiable (human override required).
   `on_reject: warn` downgrades to a printed warning for solo-mode repos.

Progress is streamed in real-time to stderr showing round number,
verdict per round, and replying status — no spinner.

### Headless mode

When derrick detects `!isatty(stdin)` (CI, background subprocess,
automated `derrick drill`), assay runs without blocking for interactive
input. Behaviour changes:

- Only a `reject` verdict halts the pipeline. `revise` and `accept`
  are both treated as pass.
- Round exhaustion without a `reject` logs a warning and continues.
- The user is never prompted to extend rounds or override — the
  configured `tools.assay.rounds` limit is hard.

This allows fully unattended `derrick drill` runs in CI pipelines. For
interactive sessions the behaviour is unchanged — `revise` and
`reject` both surface to the user.

### Why a second-family reviewer

Assay's value is *different-family scrutiny*. The default
`reviewer` role binds to `codex-gpt5` because Codex ships a stable
`codex exec` non-interactive mode and is widely installed, but
**any** model in §6.5 can fill the role. The only constraint we
care about: don't bind both `proposer` and `reviewer` to the same
provider — that defeats the point. `derrick doctor` warns if you
do.

Multi-reviewer assay (§9.C.2) accepts a list:
`tools.assay.reviewers: [reviewer, reviewer-gemini, reviewer-local]`.
Reconciliation policy in §9.C.2.

### Boundaries with the *other* underlying tools

We do **not** fork speckit. We own the execution substrate. Derrick's
contract:

- **speckit**: invoked via `claude /speckit.*`. Derrick assumes speckit
   writes `.specify/feature.json` and a per-feature directory.
- **Substrate is ours** (§8) — SQLite-backed, no external service. The
  trait allows additional backends later but v1 ships only the native one.

If any underlying tool changes its CLI shape, derrick updates its
adapter in `crates/derrick-tools/`. That's the only blast radius.

---

## 8. Execution substrate

The pipeline produces a `tasks.md`. *Something* then has to track
those tasks as work units, sequence them, dispatch them to hands,
and report state. That something is the **execution substrate**.
Derrick ships its own. v1 has exactly one backend — native — behind
a trait so additional backends can slot in later without touching
the rest of the codebase.

### 8.-1 Glossary (derrick's vocabulary)

| Derrick | Role |
|---|---|
| **site** | Workspace registered with the substrate |
| **ticket** | One unit of work |
| **batch** | Ordered named group of tickets for one feature |
| **hand** | An executor (claude / copilot / human) |
| **foreman** | Orchestrator loop that walks ready tickets and dispatches |
| **dispatch** | The verb for assigning a ticket to a hand |
| **activity** | Recent event timeline |
| **link / blocks** | Typed edges between tickets |
| **prefix** | Short site code, e.g. `ti` → `ti-47` |

**Verb note — `drill`:** the verb for kicking off the feature pipeline (`derrick drill`, `/drill`). It is not a substrate noun, but it is reserved vocabulary: `drill` is not a gastown word and should not be confused for one. New contributors: use `drill` for this action, not `add`. (D64)

Chosen deliberately distinct from gastown's vocabulary
(rig/bead/convoy/polecat/mayor) so the two are unambiguous when
both are present on the same machine.

### 8.0 The decision: own it

The substrate is the most strategically important piece of derrick
because everything after `tasks` runs on it. Originally we sketched
a gastown shim as an alternative backend. Decision for v1: **drop
the shim, focus on the native implementation, ship it well.** A
gastown backend can be added later as a separate crate behind the
same trait if the demand is there.

Selected via:

```yaml
tools:
  substrate:
    backend: native             # v1 ships only this; trait allows future backends
    mode: crew                  # solo | copilot | crew
```

`none` is solo-mode shorthand (pipeline ends at `tasks.md`).

### 8.1 The model

One logical model, one native implementation in v1. Everything
in derrick — observability surface, runners, memory layers —
talks to the substrate through a family of focused role traits
(`TicketStore`, `EventLog`, `HandRegistry`, `ForemanState`,
`WorktreeReservations`), not to SQLite directly and not through
one all-encompassing trait (D89 split the original 47-method
`Substrate` god-trait once it was clear callers only ever held
the concrete `Arc<NativeSubstrate>` and the trait bought no real
decoupling). Adding a second backend later means a new crate that
implements the role traits it needs; no other code changes.

- **Site** — a workspace registered with the substrate. One per
  repo. Has a name and a ticket prefix.
- **Ticket** — a single unit of work with state
  (`ready | in_flight | in_review | blocked | done | rejected`),
  labels, body, optional `merge_sha` (set only when verified-Done per §8.6),
  links to other tickets, owner.
- **Link** — typed edge between tickets. v1 supports `blocks`
  (sequencing) and `related` (informational).
- **Batch** — an ordered named group of tickets representing one
  feature. Closes when all member tickets close.
- **Hand** — anything that can execute a ticket. v1 hand types:
  `claude` (interactive human-driven), `copilot` (cloud agent
  dispatch via GitHub API), `human` (just claimed by a person).
  Extended by D66: `codex`, `opencode`, `aider` (host-CLI executor
  hands dispatched by the generic `derrick-hand` dispatcher).
- **Foreman** — the orchestrator that walks ready tickets, applies
  routing rules, dispatches to a hand, polls completion, reports.
  v1 runs as a tokio task inside the derrick process; can later
  detach to an out-of-process daemon.

Things explicitly **out of scope for v1**: agent mail, multi-site
federation, watchdog services, merge queue, persistent agent
identity beyond per-ticket ownership, epic staging. These are
features other systems (gastown, GitHub Projects, Linear) already
ship — we'll add them only if our own substrate genuinely needs
them, not by default.

### 8.2 The native substrate

**Storage**: SQLite at `.derrick/derrick.db` via `rusqlite`
(bundled, no external dependency). Schema is small — `tickets`,
`links`, `batches`, `hands`, `events`. WAL mode, single writer
(the foreman task), many readers (observability surface).
File-based: no server, trivial backup, trivial gitignore.

**Foreman loop**: a tokio task in the derrick process. It polls
ready tickets, dispatches, and watches for completion via hand
hooks. When `derrick run drill` returns, the foreman either:
- exits cleanly if all tickets are `done`, or
- detaches into `.derrick/foreman.pid` and continues in the
  background. `derrick foreman stop` ends it; `derrick foreman
  logs` tails it.

**Hands**:
- `copilot` hand shells to the standalone `copilot` CLI
  (`copilot agent run --task <body> --label
  "derrick/ticket=<id>"`) and watches the resulting PR for the
  ticket-id label to detect completion.
- `human` hand marks the ticket `in_flight` and waits for the
  user to declare completion. The completion path depends on
  `mode` (D35 / T012):
  - `mode: solo` — `derrick ticket done <id>` (transitions
    directly to `Done` with an attestation event; no PR
    expected).
  - `mode: crew` or `copilot` — `derrick ticket review <id>
    --branch <b> [--pr-url <u>] --head-sha <s>` (transitions
    to `InReview`; the foreman's verifier moves it to `Done`
    after observing the merge SHA on the target branch).
    `derrick ticket done` is refused outside solo mode to
    keep D31's "Done requires observable evidence" rule
    enforceable.
- `claude` hand writes a `.derrick/queue/<ticket-id>.md` file
  and prints a hint — the user picks it up in their Claude
  Code session.

**Concurrency**: §9.C `parallelism.batch_max` caps how many hands
run at once. The foreman honours `blocks` links strictly.
`dispatch_ready` pre-computes per-ticket branch context sequentially
(so branch-name derivation can't race), then fans the dispatches
out concurrently via `futures::future::join_all`. Each
`dispatcher.dispatch(ctx)` future awaits independently; results
are collected in-place and folded into the tick report.

**Mutation API**: in-process Rust, plus a small subset of CLI
write commands derrick *does* expose (it can't be entirely
read-only against its own substrate):

| Command | Purpose |
|---|---|
| `derrick ticket new` | Create a ticket (used internally by `bridge`) |
| `derrick ticket done <id>` | Mark complete (solo mode only) |
| `derrick ticket review <id> --branch <b> --head-sha <s>` | Declare InReview (crew mode human path) |
| `derrick ticket block <id> --on <id>` | Add a `blocks` link |
| `derrick ticket reopen <id> --note <text>` | Re-ready a Blocked ticket (T012); Done/Rejected reopen deferred to a follow-up |
| `derrick batch close <name>` | Force-close a batch |

Reads (status, tickets, ticket, batch, etc.) are §5.5 already.

### 8.2.1 Bridge auto-remediation

The `bridge` step creates substrate tickets from `tasks.md`. Two
idempotency rules that fire before any ticket is created:

1. **Terminal ticket delete+recreate**: if a ticket for the same
   feature already exists in a terminal state (`Done` or `Cancelled`),
   bridge deletes it and recreates it so the new run starts fresh.
   This handles the common case where a feature was completed in a
   prior run and is now being reopened or revised.

2. **Active ticket skip**: if a ticket for the same feature already
   exists in a non-terminal state (`Ready`, `InFlight`, `InReview`,
   `Blocked`), bridge skips creation entirely and reuses the existing
   ticket. This prevents duplicate tickets when `derrick drill` is
   re-run for a prompt that was already in progress.

Both rules fire per-ticket (not per-batch) so a partial batch
(some tickets completed, some not) is handled correctly.

### 8.3 Modes

`tools.substrate.mode`:

- **`solo`** — `backend: none`. Pipeline ends at `tasks.md`.
  The user works from the markdown.
- **`copilot`** — substrate present, foreman not started:
  tickets are dispatched directly to Copilot agents and derrick
  polls completions inline.
- **`crew`** — substrate present, foreman running, hands fanning
  out. This is the dark-factory mode.

### 8.4 Copilot as a first-class runner

Copilot remains both a pipeline-step runner (`runner: copilot`
on any step) and the batch executor in `mode: copilot`. The
Copilot adapter (`crates/derrick-copilot`) sits behind the
substrate trait when the foreman is running.

**Backend choice in v1.** GitHub ships two surfaces we can use:

- The standalone **`copilot` CLI** (`@github/copilot` / `npm i -g
  @github/copilot`) — purpose-built for agent dispatch.
- **`gh copilot`** as a `gh` extension — older, narrower (chat /
  suggestions, less agent-oriented).
- The **GitHub Copilot SDK** (`@github/copilot-sdk`) for embedded
  use — to be researched; could be the right path for native fan-out
  without shelling out per task.

v1 default: the standalone `copilot` CLI. The adapter abstracts
behind a `CopilotBackend` trait so we can add an SDK-based backend
later without changing the rest of derrick. (Recorded as a v1.1
research task: "evaluate `@github/copilot-sdk` for parallel fan-out
and in-process dispatch.")

### 8.5 PR stacking — tickets become a stack, not a pile

A batch is an ordered group of tickets with explicit `blocks`
links. That dependency chain maps directly onto a **stacked PR
graph**: each ticket becomes one PR; its parent branch is the
PR of the ticket it blocks on (or `main` for roots); reviewers
see small, focused changes; tickets can land independently as
their dependencies clear.

Without stacking, derrick produces N PRs all rooted on `main`
that conflict with each other on every shared file. With
stacking, the same N PRs form a clean DAG.

#### Backends

`tools.git.stacking.backend`:

- **`none`** (default) — derrick doesn't manage PRs. Hands
  push branches and open PRs however they want. Sensible
  for solo mode or single-ticket batches.
- **`native`** — derrick's own stacking engine using plain
  `git` + `gh pr create`. The sole implemented backend (D72).
  Branches are named `derrick/<batch>/<ticket-id>`; the foreman
  sets parents based on `blocks` links; when a parent PR lands,
  derrick rebases and force-pushes all dependents (`--force-with-lease`,
  D19 conflict-bail, D20 branch ownership). Stacked PRs are
  opened via `gh pr create`; `derrick doctor` checks that
  `git` and `gh` are present.

Legacy configs that name `graphite` or `git-spice` fail at
load time with an actionable error directing the user to set
`backend: native`.

The trait `StackBackend` lives in `crates/derrick-stack` as
the §8.6 extension seam for future backends.

#### What the foreman does when stacking is on

1. Before dispatching a ticket to a hand, the foreman computes
   its parent branch from the substrate (the most-recent
   `blocks` predecessor's branch, or `main` if root).
2. For `native` and `copilot` hands, derrick creates the branch
   off the parent itself and tells the hand to push to it (D20).
   For `human` hands, the hand creates its own branch and
   derrick rebases it after if needed.
3. When the hand opens a PR, derrick adds the PR URL to the
   ticket via the substrate.
4. When a PR merges, derrick walks the dependent tickets and
   restacks them via `git rebase --onto`. Force-push uses
   `--force-with-lease`. After rebase+force-push the foreman
   also retargets the child PR's base via `gh pr edit --base`
   so GitHub shows the correct diff; `NotSupported` is tolerated
   (warn + Note), same posture as the force-push gate.
5. If a restack fails (merge conflict), derrick bails immediately
   (D19): the ticket moves to `blocked` with a `restack-conflict`
   label, and the activity log records the exact `git rebase
   --onto <parent> <old-base> <branch>` recipe so the human can
   resolve and resume. No auto three-way-merge attempts —
   force-pushed half-resolved branches are worse than blocked
   ones.

#### `derrick stack` subcommand

- `derrick stack show` — renders the current stack for the active
  batch: a PARENT column with tree indentation by DAG depth in
  topological order, merge status, and restack health.
- `derrick stack restack [--batch <name>]` — topological cascade
  restack: processes the `blocks` DAG in deterministic topological
  order (tie-break: ordinal then ticket id). A D19 conflict blocks
  only the conflicting ticket and poisons its transitive descendants;
  independent subtrees continue unaffected. Manual fallback when
  the foreman isn't running.
- `derrick stack submit [--batch <name>]` — whole-stack submit:
  walks the batch in stack order, opens missing PRs with the correct
  base (parent branch for non-roots, `main` for roots), and retargets
  existing PRs whose base is stale via `gh pr edit --base`. Also
  maintains an idempotent stack navigation table (marked section) in
  each PR body listing the stack with the current PR highlighted.

#### Configuration

```yaml
tools:
  git:
    stacking:
      backend: native            # none | native
      branch_pattern: "derrick/{{batch}}/{{ticket_id}}"
      auto_restack_on_merge: true
      force_push: with-lease     # with-lease | off
      auto_pr: false             # D22: default off. `--auto-pr` flag overrides per run.
      draft: false               # open as draft PRs when auto_pr fires
```

#### Brownfield detection

`derrick init` always proposes `backend: native`. Repos that
previously used Graphite or git-spice will need to migrate to
the native engine; the init wizard notes this if it finds
`.graphite_user_config` or a `gs`-managed repo.

#### Squash-merge warning (D21)

Stacked PRs break under squash-merge: the squash rewrites the
parent SHA, so children no longer rebase cleanly. `derrick doctor`
detects the repo's default merge strategy (via `gh api
repos/{owner}/{name}` → `allow_squash_merge` /
`allow_merge_commit` / `allow_rebase_merge`) and emits a warning
if squash is the only or default option while stacking is enabled.
We do **not** refuse to run — that would lock derrick out of too
many repos — but we make the trade-off visible. The
recommendation is to enable merge-commit or rebase-merge for
derrick-managed PRs.

### 8.6 Extension point: adding more backends later

The substrate role traits (`TicketStore`, `EventLog`,
`HandRegistry`, `ForemanState`, `WorktreeReservations` — D89) are
the only contract. A future `crates/derrick-substrate-gastown` (or
`-linear`, `-github-projects`, `-jira`) would implement the traits
it needs — observability, runners, and the foreman keep working
unchanged against whichever slice a given caller depends on. We
are not designing for this in v1; we are leaving the door open,
and the split makes that door cheaper to open than one
monolithic trait would.

`derrick doctor` in v1 checks: SQLite file accessibility,
foreman PID liveness if attached, ticket schema version. No
external service, no daemon, no port.

### 8.6 State machine integrity (D31 / D32 / D33)

This subsection bakes in real-world lessons from running
gastown at scale (refinery optimistic-close incident,
gt-pvx WISP leak, mayor-trusts-bead-state pattern). The
dark-factory case requires **append-only, observable,
verifiable** state transitions; gastown's interactive-first
shape doesn't guarantee any of those autonomously.

#### Ticket lifecycle

```
                ┌──────────────────────────────────┐
                │ Ready  (created, not yet picked) │
                └─────────────────┬────────────────┘
                                  │ foreman dispatches → hand
                                  ▼
                ┌──────────────────────────────────┐
                │ InFlight  (hand working)         │
                └──────────────┬───────────────┬───┘
                               │               │ hand opens PR
                hand reports   │               ▼
                blocked        │   ┌──────────────────────────────┐
                               │   │ InReview  (PR open, awaiting │
                               ▼   │  merge — NOT a terminal state│
                ┌──────────────────┤  per D31)                    │
                │ Blocked          └────────┬─────────────────────┘
                └──────┬───────────┘        │ foreman observes merge
                       │                    │ SHA on target branch
                       │ unblocked          │ (or PR-merged event)
                       │                    ▼
                       │            ┌──────────────────┐
                       │            │ Done             │
                       │            │ (merge_sha set)  │
                       │            └──────────────────┘
                       │
                       │ closed unmerged
                       ▼
                ┌──────────────────────────────────┐
                │ Blocked                          │
                │ (human re-opens or rejects)      │
                └──────────────────────────────────┘
```

`InReview` is the state that closes the optimistic-close
hole. A hand finishing work and opening a PR transitions the
ticket to `InReview`, *not* `Done`. Only the foreman's
verifier path — observing the merge SHA on the target branch
— moves it forward. If the PR is closed unmerged, the verifier
moves it to `Blocked` (a human re-opens with a new branch or
explicitly rejects — D32). If the PR has been pending past
`tools.foreman.in_review_ttl` (default 24h), the foreman
re-queries `gh` and either rolls forward or flags
`escalation: stuck-in-review`.

#### Append-only events

Every state transition writes an `events` row. Reverting a
state moves *forward* to a different state — e.g.
`Blocked` → `Ready` via `derrick ticket reopen` (T012), or
`Done` → a future "Reopened" variant when a merge gets
reverted (deferred to a follow-up ticket; T012 does not
implement the Done-revert path). State changes never erase
prior events. The activity log is the durable record; the
current state on the ticket row is a projection of the
latest event.

#### Verifier loop

The foreman's loop iteration (T012):

1. `bd ready`-equivalent: tickets with state `Ready` and all
   `blocks` dependencies satisfied.
2. **Verifier pass**: for each ticket in `InReview`, query the
   target branch's git log for the recorded `pr_head_sha`'s
   merge commit. If found → transition to `Done` with
   `merge_sha` set. If `gh pr view` says closed-unmerged →
   transition to `Blocked` (D32; not `Rejected`). If neither
   resolves and the ticket
   has been `InReview` longer than the TTL → emit an
   escalation event but don't change state automatically.
3. Reconcile `Blocked` tickets: re-check whether their `blocks`
   predecessors are now `Done`; un-block if so.
4. Dispatch ready tickets to hands.
5. Sleep `tools.foreman.poll_interval` (default 10s).

Step 2 is the load-bearing piece. **Without it, derrick
inherits gastown's bug.**

#### Cleanup loop (D32)

On every `derrick run` startup, before dispatching any work:

1. Walk worktree rows whose `finalize_worktree` event is
   missing and whose `created_at` is older than 24h. Prune
   the worktree directory and remove the row. Emit a
   `WorktreeAbandoned` event.
2. Walk tickets in `InReview` older than the TTL. Trigger
   the verifier pass immediately (don't wait for the next
   loop tick).
3. Walk `claimed_at`-stale tickets in `InFlight` whose hands
   haven't heartbeat'd in `tools.foreman.hand_ttl` (default
   30 minutes). Re-queue as `Ready`, emit a
   `HandAbandoned` event. **Hand pid liveness (D75)**: when a
   hand row carries a `pid` (crew hands spawned by a dispatcher),
   `kill(pid, 0)` is a second authoritative signal — a dead pid
   abandons the hand immediately even before the heartbeat TTL
   elapses, and a live pid suppresses abandonment when heartbeats
   are merely stale (e.g. a busy agent that hasn't heartbeat'd but
   is still running). Heartbeat TTL remains the backstop for hands
   with no pid (human hands, externally-spawned hands).

The cleanup loop runs sequentially with the main loop — never
concurrently, never as background — so cleanups can't race
the foreman's dispatch.

#### Structured hand telemetry (D76)

Crew dispatchers (`derrick-hand` HostCliHandDispatcher,
`derrick-claude`, `derrick-copilot` local,
`derrick-substrate-native` HumanHandDispatcher) report hand
progress via three typed `EventKind` variants rather than
free-text `Note` bodies:

- `HandStarted { hand, pid, ticket }` — emitted on spawn; the pid
  feeds D75 liveness.
- `HandProgress { hand, snippet }` — throttled to at most one
  event per hand per 2 seconds and only on meaningful-change
  (latest stdout line differs from the last emitted snippet);
  the snippet is capped at ~80 display columns. This bounds
  event-log growth even when an agent streams hundreds of lines.
- `HandExited { hand, code, stats }` — emitted on completion with
  exit code and token stats.

`Note` events remain for genuinely free-form operator messages.
The TUI's `build_hand_rows` is the first consumer and no longer
string-matches `"exited successfully"` / `"hand stats:"` bodies.

#### Why this is in §8 and not in §9 parallelism

Parallelism (§9.C) is about *throughput*. State integrity
(§8.6) is about *correctness*. They interact (cleanup runs
before dispatch; the verifier blocks new dispatch for the
ticket being verified) but they're different concerns.
The integrity rules trump throughput — if reconciliation
takes 200ms before a dispatch, dispatch waits.

---

## 9. The three pillars: memory, tokens, parallelism

Derrick is architected around three load-bearing properties. Every
design decision is checked against them. If a feature can't be made
memory-aware, token-efficient, and parallel-safe, we don't ship it.

> **Memory** — the assistant doesn't relearn the rig every turn.
> **Tokens** — every byte that crosses a model boundary earned its place.
> **Parallelism** — independent work runs concurrently, by default.

### 9.A — Memory

Memory turns repeated facts into context derrick doesn't have to
re-send. Three layers, all written by derrick and namespaced
`derrick/<rig-name>/...` so multi-repo machines stay tidy.

**9.A.1 Init-time seeding.** `derrick init` writes into the user's
auto-memory dir (`~/.claude/projects/.../memory/derrick/<rig>/`):

- *project memory*: site name, ticket prefix, mode, primary
  language(s), constitution path. One file each, one-line entries
  in `MEMORY.md`.
- *reference memory*: where specs/tasks/verdicts/logs live.
- *feedback memory*: derrick's own guardrails ("batches never
  re-ordered after creation", "assay verdict is binding unless
  `--no-assay`", "don't mutate the substrate DB directly").

**9.A.2 Per-run memory** (`.derrick/runs/<ts>/memory.md`). After
every pipeline step derrick appends a one-line digest:
*"plan: 11 tasks, opus, 41.8s; revised once after assay → accept."*
The next step reads this instead of replaying transcripts.

**9.A.3 Per-feature memory** (`.derrick/state.json` →
`features.<slug>`). Persists across runs of the same feature: the
spec dir, the batch id, the last assay verdict, open hand
assignments. `--resume-from` reads this. When a feature ships and
its batch closes, derrick auto-prunes the entry.

**9.A.4 Cross-feature lessons.** Once a batch closes, derrick
extracts non-obvious lessons (constitution amendments touched,
assay rejections by reason, orphan ticket count) into
`.derrick/lessons.md`. Future `plan` and `assay` steps get this
appended as low-priority context. It's the only memory layer that
grows over time, and it's pruned via `derrick memory prune
--older-than 90d`.

**Quality gate**: every extracted lesson must reference at least
one specific ticket id *or* a constitution section anchor; if not,
it's discarded. This keeps the lessons file specific and citable,
and prevents the LLM extractor from polluting it with vague
maxims ("be careful with concurrency"). The gate is mechanical
(regex check), runs before the lesson is written.

**9.A.5 Lifecycle.** `derrick memory list | show | prune | unmemoize`.
Unmemoize removes everything under `derrick/<rig>/` for clean
uninstall.

### 9.B — Tokens

Every byte across a model boundary earns its place. Seven knobs:

**9.B.1 Model tiering** (per role, overridable in `derrick.yaml`):

| Step | Role | Default model | Why |
|---|---|---|---|
| `specify`, `tasks` | `drafter` | claude-sonnet | Mechanical, structured |
| `plan` | `proposer` | claude-opus | Hard reasoning |
| `assay` | `reviewer` | codex-gpt5 | Adversarial, different family |
| `bridge`, `foreman` | — | n/a | Subprocess / in-process |
| `runner: copilot` steps | `executor` | copilot | Mechanical at Copilot rates |
| Inter-step summary | `summariser` | claude-sonnet (or `ollama` local) | Hot path; local is free |

The native default sequential spine is
`specify → clarify → plan → assay → tasks → bridge → foreman`. Speckit opt-in
configs may still pin the historical `/speckit.analyze` step after `tasks`.

Re-binding a role re-routes every step that uses it. BYOM means
you can bind `proposer` to a Bedrock-hosted Claude, or `reviewer`
to Gemini, without touching the pipeline.

**9.B.2 Scrubber (`derrick-scrub`, §3.1).** Per-tool output filters
strip CLI noise before the next step sees it. Target 60–90%
reduction on subprocess noise. Rules per tool in
`crates/derrick-scrub/src/rules/<tool>.rs`. `--raw` opts out.

Config:

```yaml
tools:
  output_compression:
    enabled: true              # default true; false disables all scrub filters
```

Per-step manifest fields: `bytes_raw` (bytes before scrubbing),
`bytes_saved` (bytes removed). `derrick gain` uses these to
report context reduction per step.

**9.B.2a Roughneck (`derrick-roughneck`, §3.1).** LLM output
compression via prompt injection. After every model step, derrick
appends a short instruction to the model's context asking it to
produce a compressed version of its own output before handing off
to the next step. Three levels:

| Level | Technique | Typical saving |
|---|---|---|
| `lite` | Drop filler phrases, redundant headers, verbose preamble | ~30% |
| `full` | Fragment-style output ok, abbreviate known patterns | ~65% (default) |
| `ultra` | Telegraphic; structured data only; no prose | ~75% |

Config:

```yaml
tools:
  roughneck:
    enabled: true
    level: full                # lite | full | ultra
    compress_memory: true      # also compress per-run memory digests
```

Per-step manifest field: `roughneck_tokens_saved` (estimated tokens
saved vs. uncompressed output). The TUI Tokens tab surfaces roughneck
savings alongside scrub savings.

**Where scrub fires (D29):** at every boundary where CLI-shaped
output crosses to a model context, not only derrick's own
pipeline seams. Three classes:

| Boundary | Mechanism |
|---|---|
| Derrick-internal (foreman dispatch, step handoff, assay brief) | Inline in `derrick-flow` / `derrick-substrate-native`. |
| Host tool calls (Claude Code `Bash`/`Read` → host context) | Hooks. `derrick init` writes `PreToolUse`+`PostToolUse` entries in `.claude/settings.json` that pipe tool I/O through `derrick scrub`. Codex equivalent is deferred per D34 (no stable hook surface today). |
| Copilot dispatch input/output | Inline in `derrick-copilot`'s adapter (Copilot's hook surface is too thin to plug today). |

Direction: scrub fires on both **input** (before embedding tool
output into the next prompt — this is where prompt caching saves
compound) and **output** (when an agent quotes CLI output back).

`derrick scrub <cmd>` remains the ad-hoc CLI for users.

**9.B.3 Caveman (derrick-native, §3.1).** Pure-Rust text compressor
with three intensity levels (`lite | full | ultra`). Identifiers,
paths, error messages preserved verbatim. Byte-identical to the
caveman skill at matched intensities.

**Where caveman fires (D29):** every model boundary that carries
prose. Same three classes as scrub:

| Boundary | Mechanism |
|---|---|
| Derrick-internal | Inline; full log to disk, compressed summary into the next prompt. |
| Host tool calls | Hooks in `.claude/settings.json` route prose-shaped tool output through `derrick caveman --intensity lite`. Code spans and file paths preserved verbatim. Codex equivalent deferred per D34. |
| Copilot dispatch | Inline in `derrick-copilot`. |

`derrick caveman --intensity lite path/file.md` for ad-hoc use.

**9.B.4 Prompt caching.** Constitution + `derrick.yaml` + memory
seeds are stable across every step in a run. They go into the
cached portion of every Claude prompt so we pay the input cost once
per session, not per step. (Anthropic API: `cache_control:
ephemeral` on the system block.) Estimated save: 30–50% of input
tokens on multi-step runs.

**9.B.5 Lazy artifact loading.** A step only sees the artifacts it
declares it needs (`inputs:` in the pipeline yaml). `analyze`
doesn't get the full `contracts/` dir if it didn't ask for it.
Default `inputs:` per step are minimal and grow only on demand.

**9.B.6 Assay context discipline.** The codex brief is capped at 2k
tokens of context excluding artifact bodies. If artifacts overflow,
caveman pre-compresses and the verdict notes the compression.

**9.B.7 Telemetry.** `derrick run --tokens` prints per-step token
estimate after the run. `derrick gain` shows aggregate savings:
raw estimate, what scrubber/caveman/roughneck/tiering/caching/memory
each saved, actual usage.

Per-step manifest fields:

| Field | Source |
|---|---|
| `tokens_in` | See below for correction logic |
| `tokens_out` | Model-reported output tokens |
| `roughneck_tokens_saved` | Roughneck estimated saving vs. uncompressed output |
| `bytes_raw` | Raw subprocess bytes before scrub |
| `bytes_saved` | Bytes removed by scrub |

**`tokens_in` correction**: `claude --output-format json` under-reports
input tokens because it counts only the direct message, not the full
session context (cached prefixes, sub-agent turns, memory). Fix: derrick
stores the raw reported value *and* computes `prompt_len / 4` from the
serialised prompt and takes `max(cli_reported, prompt_len / 4)` as the
canonical `tokens_in`. The estimate is conservative but prevents
systematically under-counting cached-prefix sessions.

Sub-agents and skills invoked *inside* a host step (Claude
spawning Explore via Agent, a skill triggering caveman, etc.)
are invisible to derrick — but **not** to the host. Claude Code
persists session transcripts under `~/.claude/projects/<repo>/
*.jsonl`. After every step `crates/derrick-observe` reads the
transcript file matching the run's session id, sums token usage
across all turns (including sub-agent ones), and writes the real
number into the run manifest. Falls back to estimate when no
transcript is available (codex, copilot, raw API).

**9.B.8 Survey index (`derrick-survey`).** A pre-built SQLite + FTS5 index of repository symbols, call relationships, and cross-file references that AI agents query directly instead of fanning out across `grep`/`glob`/`Read` calls. This replaces expensive multi-file reads with cheap structured queries and feeds savings through the existing scrub/caveman/`derrick gain` machinery. Inspired by CodeGraph's data model (symbols + `references` edges + FTS5); the runtime is native Rust, not a Node wrapper. The index lives at `.derrick/index.db` (distinct from the substrate DB at `.derrick/derrick.db`). Gitignored via `.derrick/index.db*`.

The agent-facing surface is an MCP server (`derrick survey serve --mcp`). `derrick-adopt` wires Claude Code MCP support across two files: the **server declaration** goes into `.mcp.json` at the repo root (project-scoped, checked into VCS) as `{ "mcpServers": { "derrick-survey": { "type": "stdio", "command": "derrick", "args": ["survey","serve","--mcp"] } } }` — Claude Code does not honour a `mcpServers` key in `.claude/settings.json` for project scope; the **permissions** (`mcp__derrick-survey__*` tool allow-list) go into `.claude/settings.json` under `permissions.allow` so per-call trust prompts are suppressed for known tools. A known gap: Claude Code still requires a one-time interactive project-trust prompt on first load of `.mcp.json`; there is no settings key to auto-skip it (same "document the gap" posture as D34). Opencode/codex/copilot MCP support is documented and handled where each host supports it, with gaps noted (same posture as D34's Codex-hook deferral). CLI subcommands (`derrick survey build|search|context|impact|status`) ship for ad-hoc and Bash parity. This is the first MCP surface in derrick — today the tool boundary is host CLI subprocess (D30) + hooks (D29); survey adds MCP as a third seam. See D57 for the formal correction to D54 clause (b).

Config:

```yaml
tools:
  survey:
    enabled: true
    languages: [rust, typescript, javascript, python, go, csharp, java, kotlin]  # D55, extended by D58/D59
    db: .derrick/index.db    # rebuildable cache; always gitignored
    reader_pool: 4           # shared read-only across worktree runs per D38
```

Per-step manifest fields: `survey_queries` (count of MCP queries issued by agents), `survey_tokens_saved` (estimated tokens saved vs. equivalent Read/grep calls, owner: token-economist role so numbers reconcile with `derrick gain`). `derrick gain --pillars` surfaces survey savings on the Tokens line. `derrick-memory` seeds the index location as a reference memory entry on `derrick init` (Memory pillar). Parse fan-out is per-file and parallel at index-build time (Parallelism pillar; the index DB is shared read-only across worktree runs per D38, behind a reader pool sized by `reader_pool`).

**9.B.8a Survey hub (`derrick-survey-hub`).** The hub is an optional centralised mode that holds multiple codebases in one long-lived process and serves them over a network transport (rmcp streamable HTTP/SSE) rather than the per-session stdio server. It is a new in-workspace layer — a `derrick-survey-hub` crate and/or a `derrick survey hub` subcommand — not a fork of the engine and not a separate repository. The valuable IP (tree-sitter extraction, the SQLite+FTS5 schema, and the four query tools: search/context/impact/status) stays in `derrick-survey` as a repo-agnostic engine that the hub calls directly. A separate repository would require either publishing the engine as a public crate or duplicating the tree-sitter pipeline; maintaining two extraction pipelines is the one outcome the hub design explicitly avoids. The existing per-repo stdio server and its `.mcp.json` wiring (D57) are untouched; the hub is purely additive and backward-compatible. The single-binary constraint (D11/D54) is preserved. See D80.

**Workspace index sourcing (D82, resolves OQ5).** Each workspace is sourced via a `WorkspaceSource` enum: `Local { root }` — the hub holds a working tree on disk and builds/refreshes the index itself (unchanged behaviour, D81 poll-TTL applies) — or `Pushed { db_path }` — an operator or CI places a prebuilt `.db` on disk (rsync / shared volume / scp) and the hub opens, serves, and hot-swaps it atomically when the file changes, gated by the freshness TTL. Modes may be mixed within one `hub.yaml`. Schema portability is already handled by `PRAGMA user_version` / `SchemaTooNew`. The authenticated HTTP upload path for `Pushed` is deferred (D83: the `upload` capability's consumer); auth is scoped bearer tokens with proxy-terminated TLS — see D83.

The hub adds three capabilities over the per-repo stdio server: (1) **Workspace registry** — N repos are registered, each with its own `index.db` (or namespaced rows), keyed by a workspace id. (2) **Network transport** — the hub runs as a long-lived process using rmcp's streamable HTTP/SSE transport instead of (in addition to) stdio, so agents on different machines or sessions can share the same index. (3) **Tool routing** — the four survey tools gain a `workspace`/`repo` selector argument; a per-workspace tool-namespace scheme is a noted alternative but the selector-argument approach is the recommended starting point (see OQ4). **Freshness model (D81, resolves OQ2)**: hub freshness uses a hybrid of poll-on-query TTL and an explicit refresh tool. Each workspace carries a `last_checked` timestamp; on a query past the configured TTL the hub performs a cheap staleness probe (`status().pending`, size/mtime based) and, if files changed, runs an incremental rebuild before answering — this is the self-healing floor. A single-flight guard prevents concurrent queries from triggering duplicate rebuilds of the same workspace. The existing `dirty` flag arms the staleness banner during a rebuild. CI pipelines and git hooks may call `derrick_survey_refresh` (workspace-scoped) at any time to force an immediate proactive rebuild. The `notify`-watcher approach (used by the per-repo stdio server) is unsuitable as the primary mechanism because hosted repos are typically not the operator's live working tree. Push-only rebuild was rejected as the sole mechanism: a producer that forgets to notify leaves a workspace silently stale. TTL is configurable in `hub.yaml` via `freshness_ttl_secs` with a sensible default. Auth/multi-tenancy is resolved by D83 (scoped bearer tokens, proxy-terminated TLS). Routing scheme is resolved by D84: explicit `workspace` argument (default, D80-compatible) + `derrick_survey_list_workspaces` discovery tool + optional path-prefix routing (e.g. `/w/<id>`) for reverse-proxy-friendly per-site URLs; subdomain/host-based routing rejected. No hub open questions remain — OQ2–OQ5 are all resolved (D81, D83, D82, D84 respectively).

### 9.C — Parallelism

The pipeline has a sequential spine (`specify → clarify → plan → assay →
tasks`), but everything *around* and *after* it is
parallel by default. Derrick treats serial work as a justified
exception.

**9.C.1 Batch fan-out.** Independent tickets in a batch run
concurrently. The substrate's foreman (native in-process loop)
walks ready tickets and dispatches them to hands, serialising only
across explicit `blocks` dependencies. Default concurrency is
`min(8, len(ready_tickets))`; configurable in `derrick.yaml`:

```yaml
parallelism:
  batch_max: 8         # max hands / copilot agents in flight
  step_max:  4         # max parallel sub-tasks within one step
  assay_max: 2         # max concurrent reviewers in multi-reviewer assay
```

**`batch_max` bounds active hands, not total footprint (D92).**
`batch_max` caps tickets in `InFlight` only — it does not count
tickets sitting in `InReview` (hand has exited, worktree still
held, PR open, awaiting merge/verify). This is intentional: the
cap governs how many hands (concurrent workers) the foreman will
run at once, not the total number of open worktrees or PRs the
batch may accumulate. A batch can therefore hold more open
worktrees than `batch_max` at any given moment if several tickets
have reached `InReview` while new `InFlight` slots are dispatched
underneath the cap. Do not read `batch_max` as a resource-footprint
limit — it isn't one.

**9.C.2 Multi-reviewer assay.** `tools.assay.reviewers` accepts a
list. v1 ships with `[reviewer]` (codex only) as the default;
adding `gemini` or a local model is a config edit. When multiple
reviewers are configured they run in parallel against the same
brief. Derrick reconciles by `on_split:`:

```yaml
tools:
  assay:
    on_split: reject     # reject (default, fail-closed) | human | majority
```

- `reject` (default) — any reviewer's reject is binding. Conservative.
- `human` — prompt the user; surfaces both verdicts.
- `majority` — needs an odd reviewer count; otherwise treated as
  `reject`.

**9.C.3 Concurrent observability reads.** `derrick status`
aggregates substrate reads, git/gh queries, and the local manifest
— all fired in parallel. The whole dashboard returns in the
slowest read, not the sum.

**9.C.4 Parallel pipeline steps.** Steps with no data dependency
on each other can be marked `parallel_group: <name>` in the yaml
and derrick will fan them out. v1 ships this for any side-channel
checks the user adds (lint, type-check, schema validation). v1
does **not** parallelise `specify → clarify → plan → assay → tasks`
— that chain stays sequential because each step consumes the
previous step's output.

**9.C.5 Multi-feature parallelism — git worktrees.** Two
`/drill` invocations against the same repo, at the same
time, must not clobber each other's `.specify/feature.json`
(or anything else under `specs/`, `.derrick/`, working tree
state). The clean answer is git worktrees:

- Each run creates `.derrick/worktrees/<run-id>/` as a fresh
  worktree of the repo at the current HEAD, on a branch named
  `derrick/<feature-slug>-<run-id>`.
- The entire pipeline executes inside that worktree. Speckit,
  assay, and the foreman all see an isolated working tree and
  an isolated `.specify/feature.json`.
- The substrate DB at `.derrick/derrick.db` lives in the
  *main* checkout, not the worktree, and is shared. Tickets
  from concurrent runs coexist; their batches are distinct.
- On success the worktree's branch is left for the user to
  inspect, push, or PR. On failure the worktree is preserved
  with the partial state so `--resume-from` can pick up.
- `derrick run --cleanup` (and `derrick worktrees prune`)
  remove orphaned worktrees once their branches are merged or
  abandoned.

This obsoletes the file-lock / `SPECIFY_FEATURE_DIRECTORY` env
trick. Worktrees give us *real* isolation, not
just polite cooperation between sub-processes.

**9.C.6 What isn't parallel** (by design): the sequential spine
above; assay rounds within a single reviewer (round N reads round
N-1's rebuttal); `bridge → foreman` handoff (foreman depends on
tickets existing). Documented so users don't expect a free win there.

**9.C.7 Failure isolation.** When one parallel branch fails,
others complete cleanly. Derrick reports per-branch exit codes in
the run manifest. A reject from one reviewer doesn't kill the
other reviewer's report — both end up in
`<feature_dir>/assay/`.

---

The three pillars sit behind a single dashboard:

```
$ derrick gain --pillars
memory       seeded 14 entries  •  per-turn save ~3.2k tokens
tokens       this week: 412k raw → 54k actual  (-87%)
parallelism  avg 4.1 hands in flight, peak 7  •  zero lock conflicts
```

---

## 10. State and idempotency

Per-repo derrick state lives in `.derrick/`:

```
.derrick/
  state.json            # last run id, last feature_dir, last batch
  runs/
    20260517T091500Z/
      manifest.json     # pipeline, prompt, flags, exit codes per step
      step-specify.log
      step-plan.log
      …
```

`.derrick/runs/` is gitignored by default (init writes a `.gitignore`
entry). `state.json` is gitignored too. The yaml is committed.

### 10.1 Run manifest fields

`manifest.json` includes all pipeline metadata. Relevant fields added
since the initial design:

| Field | Type | Description |
|---|---|---|
| `prompt_key` | `String` | 12-hex SHA-256 prefix of the normalised prompt (whitespace-collapsed, lowercased). Used for auto-resume. |
| `resume_of` | `Option<RunId>` | Set when this run was auto-resumed from an earlier incomplete run for the same `prompt_key`. Tracks lineage. |
| Per step: `tokens_in` | `u64` | Corrected input token count (see §9.B.7). |
| Per step: `tokens_out` | `u64` | Model-reported output tokens. |
| Per step: `roughneck_tokens_saved` | `u64` | Estimated tokens saved by roughneck compression. |
| Per step: `bytes_raw` | `u64` | Raw subprocess output bytes before scrub. |
| Per step: `bytes_saved` | `u64` | Bytes removed by scrub filters. |

### 10.2 Run resume — idempotent retry

When `derrick drill "<prompt>"` is invoked, the runner computes the
`prompt_key` and checks `.derrick/runs/` for the most recent run
with the same key that exited in a non-terminal state (i.e. failed
or was interrupted mid-pipeline). If found, derrick **auto-resumes**
from the last successful step rather than starting fresh. The resumed
run sets `resume_of` to the original run's id.

```
# Same prompt, pipeline incomplete from yesterday → auto-resumes
derrick drill "build a webhook ingest endpoint"

# Force a fresh run even if an incomplete one exists
derrick drill "build a webhook ingest endpoint" --force
```

`--force` discards any incomplete prior run for the same key and
always starts from `specify`. Completed runs (all steps `ok`) are
never resumed — `--force` is a no-op against them; derrick starts
fresh as before.

The `resume_of` lineage chain can be traced via `derrick run <id>`
to reconstruct the full history of a prompt across retries.

---

## 11. What v1 ships vs. later

**v1 (this design):**

- `derrick init`, `derrick drill` / `derrick run drill`, `derrick doctor`,
  `derrick config`, `derrick uninstall` (reverses init cleanly).
- `derrick switch` — upgrades a solo-mode repo to crew mode; patches
  `tools.substrate.mode`, adds `peers:` stanza, writes foreman defaults
  (see §5.2.2).
- `derrick upgrade` — **name reserved** for binary self-update (checks
  GitHub releases, downloads, and replaces the running binary). Not yet
  implemented; the subcommand exists and prints "upgrade not yet available,
  re-run the install script" so users get a clear signal rather than a
  "command not found" error.
- Observability surface (§5.5): `derrick status`, `tickets`,
  `ticket`, `batch`, `foreman`, `activity`, `hands`, `orphans`,
  `runs`.
- Token tooling: `derrick scrub`, `derrick caveman`, `derrick gain`.
- BYOM tooling: `derrick models check`, `derrick auth set/list`.
- PR stacking: `derrick stack` / `derrick stack restack` /
  `derrick stack submit`; native engine only (D72) — plain
  git + gh, parent computation from `blocks` links, rebase
  --onto restack with --force-with-lease.
- TUI dashboard: `derrick observe` (ratatui), six tabs covering
  Overview / Tickets / Stack / Activity / Tokens / Memory.
  Live-updating via filesystem watcher + 1s tick.
- `/drill`, `/derrick-doctor`, `/derrick-resume`,
  `/derrick-status` slash commands.
- Codex CLI wrapper config: derrick writes a `.codex/instructions.md`
  (or whatever Codex's equivalent is) during init so the assay
  reviewer sees the project constitution.
- Shell completions: bash, zsh, fish, generated via `clap_complete`.
- Editor integrations: VS Code task definitions and JetBrains run
  configs in `templates/.vscode/` and `templates/.idea/`, opt-in.
- Templates for `.specify/`, `.claude/`, `derrick.yaml`,
  tasks-to-tickets bridge.
- Marketplace JSON published at
  `https://raw.githubusercontent.com/lgulliver/derrick/main/marketplace.json`
  for one-line plugin install (D28; supersedes D1 / D24 which
  assumed a custom domain). GitHub is the sole host — install
  script, marketplace JSON, and release artefacts all under
  `github.com/lgulliver/derrick`.
- Install paths (D26): `curl | bash` primary, `cargo install derrick`
  for Rust-native users, Homebrew tap for macOS. All three resolve
  to the same GitHub release artefact.
- macOS + Linux supported in v1; Windows in v1.1.

**Later:**

- Homebrew formula and a Windows build.
- `derrick run <custom-pipeline>` for repos that want flows beyond
  drill (e.g. "hotfix", "spike", "refactor").
- `derrick observe` mutation features (claim/close from inside
  the TUI; v1 is read-only).
- Evaluate `@github/copilot-sdk` for in-process Copilot dispatch
  (potential replacement for the `copilot` CLI backend).
- Optional Slack feedback hook so hand completions ping a channel.
- **Adversarial code review before PR open** — extend the assay
  pattern from plans (§7) to code. After a hand finishes writing
  code in its worktree and before the PR opens, a different-family
  reviewer (codex / another configured `reviewer` role) reads the
  diff + the spec + the constitution and produces a structured
  verdict (`accept | revise | reject`). On `revise` the hand is
  reopened with the objections; on `reject` the ticket transitions
  to `Blocked` with the verdict attached. Likely lands as a new
  step type in `derrick-stack` between "code ready" and "PR open",
  or as part of T012 foreman's verifier loop. Inherits the
  multi-reviewer reconciliation logic from §9.C.2 (`on_split`).
  Fits the D31 verifiability pillar — observing the diff before
  it crosses the model/PR boundary is the same "earn its place"
  principle scrub/caveman use for tokens.

---

## 12. Decisions (resolved) and remaining open questions

### Decisions taken

These were open during design and have now been resolved. Each
links back to the section where it lives.

| # | Decision | Locus |
|---|---|---|
| D1 | **Plugin distribution**: own marketplace at `derrick.dev/marketplace.json` (primary) + GitHub release artefacts (fallback). | §11 |
| D2 | **Speckit init**: detect-then-defer — use speckit if installed; fall back to a minimal `.specify/` skeleton derrick ships, with a banner requiring the user to author the constitution via `/speckit.constitution` before any pipeline runs. **Refined by D85**: speckit became one of three selectable providers (`tools.specify.provider`). **Superseded in part by D87**: `native` is now the default provider for new sites; speckit remains explicit opt-in/back-compatible. | §5.2 / §5.2.1 |
| D3 | **Constitution stub**: derrick prefers speckit as the constitution owner. The detect-then-defer logic in §5.2.1 enforces this: if `specify` is on PATH, init runs `/speckit.constitution` (or `specify init --here`); derrick does **not** write a constitution. **Refined by T011 / D34 era**: when speckit is *not* available and the user explicitly opts in (`--constitution-stub` or `--constitution-from-docs`), `derrick-adopt` may write a minimal banner stub or LLM-drafted constitution as a fallback. Both opt-in modes refuse if speckit is available — derrick still defers. Greenfield init forces the speckit constitution flow. **Refined by D85 and D87**: the spec-provider seam does not alter constitution artifact paths, but new sites can use derrick-native spec generation without installing speckit. | §5.2 / §5.6 / T011 |
| D4 | **Brownfield `--constitution-from-docs` drafts**: marked with a banner; `plan` step refuses to run until the user removes the banner. | §5.6 |
| D5 | **Assay reviewers in v1**: codex only. Other providers slot in via the model abstraction later — no extra v1 work. | §7 |
| D6 | **Split-verdict policy**: configurable per repo via `on_split:` (`reject` default fail-closed, `human`, `majority`). | §9.C.2 |
| D7 | **Scrubber/caveman compatibility**: caveman is byte-identical to the original skill at matched intensities; scrubber is drift-tolerant (CLI output evolves upstream). | §9.B.2 / §9.B.3 |
| D8 | **Caveman invocation**: in-process Rust default; falls back to invoking the caveman skill via the host when an unknown artifact type is encountered. | §9.B.3 |
| D9 | **Cross-feature lessons**: shipped in v1 with a mechanical quality gate — each lesson must reference a specific ticket id or constitution section anchor, else discarded. | §9.A.4 |
| D10 | **Multi-feature parallelism**: git worktrees per run (`.derrick/worktrees/<run-id>/`), not file locks. Substrate DB stays in the main checkout and is shared. | §9.C.5 |
| D11 | **Native substrate scope discipline**: additions require explicit sign-off and a DESIGN.md note; an OSS-facing policy in `CONTRIBUTING.md` keeps the rule visible to external contributors. | §8.1 |
| D12 | **Provider auth**: env vars first; optional `~/.derrick/credentials.yaml` (mode 0600) for desktop convenience; never repo-local; host-delegated providers inherit auth from the host CLI. *Superseded by D65*, which removes direct-API providers entirely. Auth is now env-passthrough only; `credentials.yaml` and `derrick auth` are removed. *Superseded-in-part by D79*, which reintroduces optional direct-API and local runtimes (API runtimes, Ollama, OpenAI-compatible, etc.) alongside the default CLI runtimes; for those opt-in runtimes, API keys via `auth_env`/`auth_mode` are again a supported path. CLI runtimes still delegate auth to the host. | §6.5 |
| D13 | **Copilot backend in v1**: standalone `copilot` CLI (`@github/copilot`). `gh copilot` is the older extension, not what we use. Backend trait allows an SDK-based path later. `@github/copilot-sdk` recorded as a v1.1 research target. | §8.4 |
| D14 | **Sub-agent / skill telemetry**: derrick parses Claude Code's session transcript files (`~/.claude/projects/<repo>/*.jsonl`) post-step for accurate token counts; falls back to estimates for codex / copilot / raw API. | §9.B.7 |
| D15 | **Role/host validation**: `derrick models check` subcommand for explicit verification; warnings (not errors) emitted at `derrick init` and `derrick run` so issues surface early. *Implemented by D65*: the check runs against the curated host catalogue in `derrick-tools/src/catalogue.rs`; unknown model ids produce WARN, not FAIL — the hybrid validation rule is now the authoritative posture. | §6.5 / D65 |
| D16 | **v1 install surface (beyond CLI + plugin)**: shell completions (clap_complete: bash/zsh/fish), VS Code + JetBrains editor configs in templates (opt-in), `.codex/instructions.md` wrapper config written during init so codex sees the constitution, `derrick uninstall` to cleanly reverse init. | §11 |
| D17 | **PR stacking ships in v1** as a first-class concern. Default backend `native` (plain git + `gh pr create`). Graphite and git-spice adapters auto-detected at init. Foreman restacks dependents on merge using `--force-with-lease`. *Superseded by D72* (adapter clauses only — graphite/git-spice adapters removed; native is the sole backend). | §8.5 |
| D18 | **TUI dashboard ships in v1** as `derrick observe`. ratatui + crossterm, six tabs, read-only, live-updates via filesystem watcher + 1s tick. Mutation features are explicitly out of scope for v1. | §5.7 |
| D19 | **Restack conflict policy**: bail immediately, surface the exact `git rebase --onto` recipe to the activity log, mark the ticket `blocked` with `restack-conflict`. No auto three-way-merge attempts (they produce subtly-wrong force-pushed history). | §8.5 |
| D20 | **Branch ownership when stacking**: derrick creates the branch off the computed parent for `native` and `copilot` hands. Human hands create their own branches; derrick rebases them after if needed. | §8.5 |
| D21 | **Squash-merge stance**: derrick *does not* refuse to run against squash-default repos, but `derrick doctor` warns and recommends switching repo merge strategy to merge-commit or rebase-merge for derrick-managed stacks. | §8.5 |
| D22 | **Auto-PR on run completion**: ship `derrick run --auto-pr` (and `auto_pr: true` in derrick.yaml), default off. Opt-in respects existing review workflows. | §8.5 |
| D23 | **Brownfield lessons gap**: when a constitution doesn't yet exist (D2/D3 flow), the lessons file stays empty rather than relaxing the quality gate. Users notice the gap and are nudged to author a constitution via speckit. | §9.A.4 |
| D24 | **Marketplace install fallback**: install script health-checks `derrick.dev/marketplace.json` with a 2s timeout and falls through silently to GitHub release artefacts. User sees a successful install either way. | §11 |
| D25 | **Foreman exit mode**: `derrick run` detaches the foreman to `.derrick/foreman.pid` and returns; a watch hint is printed (`derrick observe` or `derrick status --watch`). `--attach` for foreground for users who want it. | §8.2 |
| D26 | **Install paths**: ship three. `curl | bash` (primary, one-line install), `cargo install derrick` (Rust-native), and a Homebrew tap (macOS native). All three resolve to the same release artefact. | §11 |
| D27 | **Drop `site.role` and `pipeline[].role` for `runner: derrick` steps**: `site.role` was vestigial gastown vocabulary; the derrick substrate has one orchestrator (the foreman), no multi-role agent system. Pipeline steps with `runner: derrick` carry their own runner-specific fields (`executor_role`, `batch`, `inputs`) and do not also need a `role:` binding. Steps that need a model role still use `role:` (mutually exclusive with `runner:` in that case). | §4 |
| D31 | **State machine integrity for tickets and batches: append-only, observable, verifiable.** Lessons banked from gastown's Refinery optimistic-close incident at scale. Three rules: (a) **`Done` requires observable evidence** — the foreman never transitions a ticket to `Done` based on a hand's self-report or PR-open event; it observes the merge SHA on the target branch (`git log origin/<base>`) or equivalent end-state for the workflow. A new `InReview` ticket state covers "hand finished, PR open, awaiting merge". (b) **State changes are append-only at the event log** — every state transition writes an immutable `events` row. Reverting a state moves *forward* to a different state (e.g. `Done → Reopened`), never erases history. (c) **The foreman trusts git, not just substrate state** — when polling ready tickets and reconciling batch closure, it cross-references against the actual repository (git log, gh PR status) rather than blindly trusting its own row values. Adds `InReview` to `TicketState`, a `merge_sha: Option<String>` field on the ticket, and a verifier loop step. | §8.1 / §8.2 / future T012 foreman |
| D32 | **Worktree and ticket cleanup is continuous and self-healing.** Lessons banked from gastown's gt-pvx WISP-branch leak. Periodic cleanup runs (a) on every `derrick run` startup before doing anything else and (b) optionally as a launchd/systemd plist for long-lived setups. It walks worktree rows whose runs have crashed (no `finalize_worktree` event after a configurable TTL, default 24h), and either prunes them or marks them `Abandoned`. Same pattern for tickets stuck in `InReview` past a TTL: the foreman re-checks the PR and either transitions to `Done` (if observably merged), `Blocked` (if the PR was closed unmerged), or surfaces an escalation event. **There is no "trust eventually consistent state" path** — every long-lived state has an explicit reconciliation pass that can fail loud. | §8.2 / §9.C.5 / future T012 |
| D33 | **The foreman never has authoritative state independent of the substrate and git.** Where gastown's Mayor reads `gt convoy status` and trusts it, derrick's foreman treats its own poll as a hint and the substrate + git as the truth. Concretely: on every loop iteration the foreman (a) reads `bd ready`-equivalent tickets from the substrate, (b) for any in `InReview`, queries `git log` and `gh pr view` for the PR's actual state, (c) reconciles before dispatching new work. The dispatch is idempotent against state drift — if a ticket the substrate says is `Ready` is actually merged on main, the foreman corrects to `Done` and continues. | future T012 |
| D30 | **`derrick-tools` owns host CLI subprocess invocations; `derrick-models` owns the `Model` trait and providers.** Hosts (claude / codex / copilot) are invoked when a pipeline step sets `host:`. They receive an opaque prompt-as-argv (typically a slash command), they load their own context per the host rules, and derrick captures stdout. Providers are invoked when derrick needs a model completion via a structured `CompletionRequest` (assay reviewers, future direct-API calls). The split is **invocation-shape-driven**, not binary-driven. *Amended by D65*: for the inference path, the two paths now converge — `derrick-models` providers are host-delegated wrappers that call through to `derrick-tools` host adapters. The only remaining distinct path is `shell` (bespoke-envelope escape hatch, not a host). The invocation-shape distinction stated in D30 remains accurate for `shell` and all non-inference subprocess calls. | §3.1 / §6.5 / D65 |
| D35 | **§8.6 alignment with D32: a closed-unmerged PR transitions its ticket to `Blocked`, not `Rejected`.** Earlier §8.6 prose used `Rejected` for both closed-unmerged and explicit user rejection. D32 distinguishes them: closed-unmerged is recoverable (a human may re-open with a new branch or explicitly reject), so the verifier moves to `Blocked` and waits for a human decision. `Rejected` is reserved for explicit user rejection via the §8.2 mutation API. The §8.6 diagram and verifier prose are updated to match; the T012 trait method `verify_ticket_unmerged` transitions to `Blocked`. | §8.6 / T012 |
| D34 | **D29 refinement — Codex host hooks are best-effort/deferred.** Codex's CLI today does not expose a stable `PreToolUse`/`PostToolUse`-equivalent hook surface that derrick can rely on. T011 writes `.codex/instructions.md` (constitution + derrick.yaml reference) but does **not** install Codex tool-boundary hooks. When Codex grows a stable hook mechanism a follow-up ticket extends `derrick-adopt`. Claude Code hooks (D29 path b) remain mandatory; Copilot inline path (D29 path c) is unchanged. Users in `mode: copilot`/`crew` with codex hosts see a documented warning at init that Codex tool I/O is not scrubbed in v1. *Note from D65*: opencode and aider hook instrumentation inherits the same "documented gap, same posture as D34" stance — neither CLI offers a stable tool-boundary hook surface at this time. *Superseded by D69.* | §9.B.2 / §9.B.3 / T011 / D65 |
| D29 | **Scrub and caveman fire at every model boundary, not just derrick's pipeline seams.** Three boundary classes: (a) derrick-internal — inline in `derrick-flow` and `derrick-substrate-native`; (b) host tool calls — `derrick init` writes `PreToolUse`+`PostToolUse` hooks in `.claude/settings.json` for Claude Code; Codex's equivalent is **deferred** (see D34); (c) Copilot dispatch — inline in `derrick-copilot` until Copilot's hook surface lands. Both directions matter: input (before embedding tool output into the next prompt) saves the most because of prompt caching; output (when an agent quotes tool output back) catches the second-order leakage. | §9.B.2 / §9.B.3 |
| D28 | **Supersedes D1 and D24 — GitHub-only distribution.** The `derrick.dev` domain was unavailable, so all derrick artefacts (install script, marketplace JSON, release binaries) live under `github.com/lgulliver/derrick`. The Claude Code marketplace JSON is fetched from `https://raw.githubusercontent.com/lgulliver/derrick/main/marketplace.json`. There is no longer a separate marketplace host to health-check, so D24's fallback logic collapses to a single GitHub-releases path; transient GitHub unavailability surfaces as a normal network error to the user with the documented recovery (`gh release download` or manual binary install). | §11 |
| D36 | **Headless subprocess Write permissions: pre-create feature dirs.** `claude --print` prompts for Write tool permission when creating files in directories that don't yet exist, even with a `permissions.allow` block in `.claude/settings.json` (the block format did not suppress the prompt in practice). Resolved: `derrick init` and the pipeline runner pre-create `.specify/features/` before invoking any host step, so the directory exists and the Write prompt is not triggered. Long-term: `derrick-flow` creates the feature dir as the first act of the `specify` step, before the host invocation. | §5.3 / T013 |
| D37 | **Codex requires an interactive TTY; assay skips in headless mode.** `codex` exits immediately with "stdin is not a terminal" when invoked from a background subprocess, making the assay step unusable in unattended `derrick run` invocations. Resolution: `derrick-flow` detects `!isatty(stdin)` and, when the assay step's reviewer is a codex-family host, falls back to `claude` as the reviewer (with the assay role model). The fallback is logged as an `EscalationNote` event so the audit trail records that the interactive reviewer was substituted. Until this is implemented, `--skip assay` is the documented workaround. | §7 / D5 |
| D38 | **Each pipeline run gets an isolated git worktree (§9.C.5).** `run_pipeline_from` calls `git worktree add -b derrick/<run-id> .derrick/worktrees/<run-id> HEAD` before the first step and `git worktree remove --force` on completion. The substrate's `reserve_worktree` / `close_worktree` methods are added to the `Substrate` trait so the `Runner` can track the lifecycle via `Arc<dyn Substrate>`. All host-request CWDs and bash `current_dir` use the worktree path; `relative_to_root` and manifest paths continue to use `repo_root`. Degradation: if `git worktree add` fails (no binary, dirty index), setup logs a warning and the run continues in `repo_root` — no crash. | §9.C.5 |
| D40 | **Token counts and cost estimates are tracked per pipeline step and per run.** `CompletionResponse.tokens_in/out` are threaded through `StepExecution` → `StepRecord` → `ManifestStep` and accumulated into `RunManifest.tokens_in/out`. `RunOutcome.cost_estimate_usd(model_name)` uses a built-in pricing table (`builtin_cost_hint`) seeded with current list prices for Claude Opus/Sonnet/Haiku, GPT-4o/mini, and Gemini 2.5. Host-subprocess steps (claude CLI, copilot CLI) report zero at this layer; their token counts appear separately in `derrick gain` via Claude Code JSONL. `derrick gain --run <id>` shows a per-step breakdown from the manifest; session-level `gain` shows estimated dollar cost alongside token totals. | §9.B / `derrick-models` |
| D39 | **Adversarial code review fires before every PR, not after.** `derrick ticket code-review <id> --branch <branch> --round N` diffs `origin/<base>...<branch>` (three-dot), passes the diff + ticket requirements to a configured reviewer role, and exits 0 (pass) or 3 (issues found). Hands must call this and get a pass before calling `derrick ticket review`. Auto-remediation is hand-driven: the hand reads `.derrick/reviews/<id>/round-N.md`, fixes, and retries up to `tools.code_review.rounds` times. Beyond that, the hand surfaces the report to the human. Exit code 3 (not 1) lets hands distinguish "fix needed" from infrastructure errors. Disabled by default (`tools.code_review.enabled: false`). | §8.6 / AGENTS.md hand protocol |
| D41 | **OpenCode is a first-class host.** `derrick-tools` gains an `OpencodeHost` adapter that invokes `opencode run "<prompt>" --dir <cwd> [--dangerously-skip-permissions]`. `derrick-scrub` gains an `opencode` rule set that strips the startup banner, tool-use progress lines, spinner frames, thinking markers, and cost footers. Specialist sub-agents are published under `.opencode/agents/` with opencode frontmatter (`mode: agent`). The `HostRegistry` default set now includes `opencode` alongside `claude`, `codex`, and `copilot`. *Extended by D65*: `aider` is also added as a first-class host (fifth host), giving the full five-host set. | §6.5 / `derrick-tools` / `derrick-scrub` / D65 |
| D42 | **Full courtroom pattern: adversarial cross-model deliberation with auto-revise loop.** Assay implements the structured Claude-prosecutes / Codex-cross-examines / Claude-rebuts / Codex-deliberates cycle from the courtroom pattern. Default rounds: 10. Constitution violations are parsed and enforced as non-negotiable gates (override requires human approval). When the revise loop exhausts configured rounds, derrick prompts the user to continue or halt. Progress is streamed in real-time (round N/M, verdict, phase name) instead of a spinner. The loop switches from a `for` to a `while` to allow dynamic round extension at user request. | §7 / `derrick-flow/src/assay.rs` |
| D43 | **Roughneck: LLM output compression via prompt injection.** A new crate `derrick-roughneck` appends a compression instruction to every model request, asking the model to emit a compressed form of its output before handoff. Three levels: `lite` (~30%), `full` (~65%, default), `ultra` (~75%). Config: `tools.roughneck.{enabled,level,compress_memory}`. Per-step manifest field `roughneck_tokens_saved` records estimated savings. TUI Tokens tab surfaces roughneck savings alongside scrub. | §3.1 / §9.B.2a / §10.1 |
| D44 | **`derrick-scrub` records bytes_raw and bytes_saved per step in the manifest.** The scrub crate already existed; this decision adds structured telemetry so `derrick gain` and the TUI can show per-step context reduction, not just a binary "scrub on/off". Config: `tools.output_compression.enabled`. | §9.B.2 / §10.1 |
| D45 | **`tokens_in` correction: `max(cli_reported, prompt_len/4)`.** `claude --output-format json` under-reports input tokens (direct message only, not full session context). Fix: derrick records the CLI value and a character-count estimate and stores the larger. Conservative but prevents systematic under-counting on cached-prefix sessions. | §9.B.7 / §10.1 |
| D46 | **Run resume via `prompt_key`: idempotent retry for incomplete runs.** Each run computes a 12-hex SHA-256 prefix of the normalised prompt (`prompt_key`). `derrick drill` auto-resumes the most recent incomplete run with the same key instead of starting fresh. `--force` overrides. `resume_of` in the manifest tracks lineage. Completed runs are never auto-resumed. *(Command renamed from `derrick add` to `derrick drill` by D64; behaviour unchanged.)* | §10.2 |
| D47 | **Bridge auto-remediation: terminal ticket delete+recreate; active ticket skip.** When creating tickets, bridge checks for existing tickets with the same feature identity. Terminal-state tickets (Done/Cancelled) are deleted and recreated. Non-terminal tickets are reused (skipped). Both rules fire per-ticket so partial batches are handled correctly. | §8.2.1 |
| D48 | **Assay headless mode: only `reject` blocks the pipeline.** When `!isatty(stdin)`, assay runs without interactive prompts. `revise` and `accept` are both treated as pass; round exhaustion without `reject` logs a warning and continues. The hard rounds limit applies. Enables fully unattended CI runs. | §7 headless / §4 assay step |
| D49 | **Constitution seeding in `derrick init` wizard.** When no existing constitution is found and speckit is unavailable for interactive authoring, the wizard prompts the user to enter constitution content directly. The text is written to `.specify/constitution.md` as real content (no banner stub). `--constitution-stub` still writes the banner for users who prefer to author separately. | §5.2 Step 5 |
| D50 | **`derrick init` creates an initial commit when the repo has no HEAD.** After writing all init files, if the repo has no commits yet, the wizard runs `git add -A && git commit -m "chore: derrick init"`. Required because `derrick drill` creates git worktrees, which require at least one commit. *(Command renamed from `derrick add` to `derrick drill` by D64; behaviour unchanged.)* | §5.2 Step 7 |
| D51 | **Pipeline step order fix: `tasks` runs before `analyze`.** Task generation depends on the accepted plan but not on codebase analysis. `analyze` then has the full task list available as context. Canonical order at the time: `specify → clarify → plan → tasks → analyze → assay → bridge → foreman`. **Superseded in part by D87**: the native default drops `analyze`; speckit opt-in configs may still pin it. | §4 pipeline yaml / §9.B.1 / §9.C |
| D52 | **`derrick switch`: solo → crew upgrade command.** New subcommand upgrades a repo from `mode: solo` to `mode: crew` (or `copilot` via `--mode`). Patches `tools.substrate.mode`, adds `peers:` stanza, writes foreman defaults. Idempotent. `--dry-run` previews the yaml diff. | §5.2.2 / §11 |
| D53 | **`derrick upgrade` name reserved for binary self-update.** The subcommand is registered in the CLI but not yet implemented. It prints a clear "not yet available" message rather than a "command not found" error, preserving the name for the future self-update feature (check GitHub releases, download, replace running binary). | §11 |
| D54 | **Native code-graph index (`derrick-survey`): native Rust, own SQLite, MCP agent surface.** A pre-built symbol/reference/call-graph index that AI agents query via MCP instead of fanning out across file reads. Three locked choices: (a) **Native Rust, not a Node wrapper** — preserves the single-static-binary, no-external-runtime ethos and D11 substrate/scope discipline; CodeGraph's data model (symbols + `references` edges + FTS5) is the reference, not its runtime. (b) **MCP server as the agent-facing surface** — `derrick survey serve --mcp`; `derrick-adopt` wires the `mcpServers` stanza into Claude Code settings and documents the gap for other hosts (same posture as D34). This is the first MCP seam in derrick; today the boundary is host CLI subprocess (D30) + hooks (D29). CLI subcommands (`build|search|context|impact|status`) ship for Bash/ad-hoc parity. *(Superseded by D57 on MCP host-wiring split.)* (c) **Separate SQLite DB at `.derrick/index.db`, not the substrate DB** — different schema, rebuildable-cache lifecycle (gitignored), and read-heavy concurrency profile incompatible with the substrate's single-writer contract. | §3.1 / §9.B.8 |
| D55 | **Survey v1 language scope and pillar wiring.** Language scope: Rust, TypeScript/JavaScript, Python, Go (extended to C# by D58; to Java and Kotlin by D59). Symbol index + FTS5 full-text search + caller/callee/impact. Framework-aware routing and iOS/RN cross-language bridging deferred. Pillar wiring: Tokens — a `derrick gain` line for survey (`survey_tokens_saved`), owned by the token-economist role so numbers reconcile; Memory — `derrick init` seeds the index location as a reference memory entry; Parallelism — per-file parse fan-out at build time; index DB shared read-only across worktree runs per D38, behind a reader pool (`tools.survey.reader_pool`). Config knob at §9.B.8. | §9.B.8 / §9.A.1 / §9.C |
| D56 | **Workspace MSRV bump to unlock `derrick-survey` dependencies.** The declared `rust-version = "1.75"` floor (root `Cargo.toml`) blocked the official MCP SDK (`rmcp`, edition 2024 / Rust ≥ 1.85) and current `tree-sitter` (≥ 1.76). The floor was declared but never enforced — CI and release build on `@stable` (1.95 at decision time), with no `rust-toolchain.toml` and no MSRV gate. Resolution: raise `rust-version` to a modern floor (≥ 1.85, the edition-2024 minimum) so survey can depend on `rmcp` and the latest `tree-sitter` grammar line rather than hand-rolling an MCP transport and pinning stale grammars. Supersedes rust-architect's hold-1.75 fallback (which assumed the floor was load-bearing). Other crates' dependencies are upgraded as required by the bump. No MSRV CI gate is added — the floor remains advisory, matching prior practice. | §3.1 / D54 / root `Cargo.toml` |
| D57 | **MCP host-wiring split: `.mcp.json` for the server stanza, `settings.json` for permissions.** Corrects D54 clause (b), which stated "`derrick-adopt` wires the `mcpServers` stanza into Claude Code's `settings.json`". Claude Code does not honour `mcpServers` in `.claude/settings.json` for project-scoped servers. The correct split: (i) the server declaration is written to `.mcp.json` at the repo root (project-scoped, checked into VCS), shaped as `{ "mcpServers": { "derrick-survey": { "type": "stdio", "command": "derrick", "args": ["survey","serve","--mcp"] } } }`; (ii) per-tool permissions are written to `.claude/settings.json` under `permissions.allow`, using the `mcp__derrick-survey__<tool>` naming convention, to suppress per-call trust prompts for known tools. Known gap: Claude Code still requires a one-time interactive project-trust prompt on first load of `.mcp.json`; no settings key eliminates it (same "document the gap" posture as D34). All other host-wiring statements in D54 and §9.B.8 remain valid. Supersedes D54 clause (b) only. | §9.B.8 / D54 |
| D58 | **Survey language scope extended to C#/.NET; `tree-sitter` runtime bumped to 0.26.** Extends D55's v1 language scope (Rust, TS/JS, Python, Go) to include C# (`.cs`), via the `tree-sitter-c-sharp` 0.23.5 grammar. Symbol extraction covers classes/structs/records (type), interfaces (interface), enums (enum), methods/constructors/local functions/properties (function), delegates (type), and namespaces incl. file-scoped (module); reference extraction covers invocations, member-access calls, and object creation (`new T()`). The 0.23.5 C# grammar emits tree-sitter ABI 15, which the workspace's pinned `tree-sitter` 0.24 runtime (max ABI 14) rejected; resolution: bump the workspace `tree-sitter` runtime to 0.26 (supports ABI 13–15), which keeps the existing 0.23-line Rust/Python/Go/JS/TS grammars working unchanged (no grammar-crate or survey API changes required). Pinning C# back to the ABI-14 0.23.1 grammar was considered and rejected in favour of the current grammar plus a runtime bump. Per-language extraction is mechanical and contained to `derrick-survey` (`model.rs`, `parse/`). No change to the index schema, MCP surface (D57), or token accounting. | §9.B.8 / D55 / D56 / root `Cargo.toml` |
| D59 | **Survey language scope extended to Java and Kotlin.** Extends the scope (D55, D58) to Java (`.java`, via `tree-sitter-java` 0.23.5) and Kotlin (`.kt`/`.kts`, via `tree-sitter-kotlin-ng` 1.1.0 — the maintained successor to the stale `tree-sitter-kotlin`). Both grammars are on the 0.23/ABI-14 line the 0.26 runtime already supports (D58), so no runtime change was needed. Java symbols: classes/records (type), interfaces/annotation types (interface), enums (enum), methods/constructors (function), enum constants (constant), packages (module); refs: method invocations and `new T()`. Kotlin symbols: classes/objects (type), functions (function), properties/enum entries (constant), package headers (module); refs: call expressions incl. navigation (`a.b()`). Known limitation: Kotlin's grammar models interfaces as `class_declaration` (no distinct node), so Kotlin interfaces are indexed as `type` rather than `interface` — the query layer cannot discriminate without modifier predicates, which the extractor does not evaluate. Contained to `derrick-survey` (`model.rs`, `parse/`); no schema, MCP (D57), or token-accounting change. | §9.B.8 / D55 / D58 / root `Cargo.toml` |
| D60 | **Live run progress via a UI-free `ProgressReporter` (run-feedback Layer 1).** `derrick run`/`derrick drill` (previously `derrick add`) previously executed the whole pipeline behind a single await and surfaced only a hand-rolled carriage-return spinner with no elapsed time plus a debug-formatted `run <id>: Success` line on stdout — the "black box" UX. Resolution: `derrick-flow` defines a dependency-free `ProgressReporter` trait (`pipeline_started` / `step_started` / `step_finished` / `pipeline_finished`, carrying step id, status, token deltas, and durations) that the `Runner` calls at each step boundary; the orchestrator owns no terminal I/O. The `Runner` gains an `Arc<dyn ProgressReporter>` defaulting to `NoopReporter` (so tests and library callers stay silent) plus a `with_progress` builder. `derrick-cli` implements it with `indicatif` (new workspace dep): an animated per-step spinner with `i/total` counter and live elapsed time on a TTY, each step resolving to a `✓/⏭/⚠/✗` line with duration and token cost, and a clean final summary; degrades to plain status lines when stderr is not a terminal or `NO_COLOR` is set. All run status output is on **stderr** (stdout reserved for machine-readable output); the obsolete `crate::spinner` module and the stdout summary line are removed. **Layer 2 (true line-by-line streaming of agent subprocess output, currently buffered in `derrick-tools` via `wait_with_output`) is deferred** to a follow-up. Interactive prompt redesign (arrow-key menus via `inquire`) and a shared CLI theme module are tracked separately. | §5.3 / §10 / `derrick-flow` / `derrick-cli` / root `Cargo.toml` |
| D61 | **Live agent-output streaming (run-feedback Layer 2).** Completes the black-box fix begun in D60. The host process layer (`derrick-tools/process.rs`) previously buffered agent output via `child.wait_with_output()`, so a step ran silently for minutes. Resolution: `run_host` now drains stdout and stderr concurrently on separate tasks (avoiding pipe-buffer deadlock), forwarding each complete line to an optional, UI-free `OutputSink` (a `Arc<dyn Fn(StreamSource, &str)>` newtype on `HostRequest`, default `None`) as it arrives, while still accumulating the raw bytes so the captured `HostResponse` is byte-identical to before. `derrick-flow` adds `ProgressReporter::step_output` and threads a per-step sink (closing over the step id + reporter) through `execute_step`/`execute_role_step`; the runner builds it from `self.reporter`, skipping interactive steps (which own stdin). `derrick-cli`'s `indicatif` reporter renders the latest output line as a condensed, truncated heartbeat in the running step's spinner. The sink stays `None` for non-TTY/`NO_COLOR` and interactive steps, so capture-only behaviour is unchanged there. No schema or token-accounting change. Remaining UX work (init-wizard redesign via `inquire`, shared CLI theme) is still open. | §6.5 / §5.3 / D60 / `derrick-tools` / `derrick-flow` / `derrick-cli` |
| D62 | **Init wizard redesigned on `inquire` (arrow-key prompts).** Closes the init-wizard item deferred by D60. The wizard previously used hand-rolled numbered-list selects (type a number) and a long chain of separate yes/no prompts read via raw stdin — visually weak and tedious. Resolution: adopt `inquire` (new workspace dep) for all wizard prompts — arrow-key `Select`/`MultiSelect`, `Text` with inline validators (the ticket-prefix re-ask loop becomes a validator), and `Confirm`. The trailing yes/no toggles (conventional commits, append AGENTS.md, hooks, VS Code, JetBrains, force) collapse into a single `MultiSelect` with sensible defaults pre-checked, cutting ~6 prompts to one screen. `Esc`/`Ctrl-C` on any prompt cancels cleanly (→ `WizardSelection::Cancelled`); `prompt_constitution` falls back to default seeds. Safe because `should_run_wizard` already gates on a real TTY (`stdin`+`stdout`), so non-interactive/`--yes`/`--no-wizard`/piped/test paths never reach `inquire` and are unchanged; the `WizardInput`/`WizardOutput` contract and pure helpers/tests are preserved. Contained to `derrick-cli` (`init_wizard.rs`); no cross-crate or schema impact. Shared CLI theme module remains the one open UX item. | `derrick-cli` / D60 / root `Cargo.toml` |
| D63 | **Shared CLI theme module (`ui`).** Closes the last UX item from D60/D62. Styling was scattered: three duplicated `is_styled()` definitions (`init.rs`, `init_wizard.rs`, `switch.rs`) and ~30 hand-rolled `\x1b[…m` escape literals across commands, with no single authority. Resolution: a new `crate::ui` module is the one place that decides whether output is styled (stdout TTY + `NO_COLOR` unset) and exposes colour/weight primitives (`bold`/`dim`/`cyan`/`green`/`red`/`yellow`, built on `owo_colors`), coloured glyphs (`tick`/`cross`/`warn_glyph`/`arrow`), and semantic line builders (`ready`/`written`/`skipped`/`done`/`hint`/`warn`/`rule`/`section`). The three `is_styled()` copies collapse to `ui::styled()`; `init_wizard`'s `bold`/`dim`/`section_rule` delegate to `ui`; the clean semantic lines in `init.rs`/`switch.rs` migrate to `ui` helpers. `owo_colors` emits the same escape codes as the prior literals and helpers degrade to plain text when unstyled, so output is byte-equivalent — verified by the existing init integration tests (which assert on plain text) plus new `ui` unit tests. Mixed lines with inline code spans keep their `ui::styled()`-gated branches (preserving backtick markers in plain mode) rather than forcing a lossy collapse. Contained to `derrick-cli`. | `derrick-cli` / D60 / D62 |
| D64 | **Feature prompt accepted from file or stdin.** The feature brief previously had to be a shell-positional/`--prompt` string, making a large multi-line `/speckit.specify`-style brief (newlines, quotes, `$`) painful to pass. Resolution: add `--prompt-file <path>` to both `derrick add` and `derrick run add-feature`, and read stdin when the prompt is the `-` sentinel, `--prompt-file -` is given, or input is piped with no other source. A new `derrick-cli` `prompt_input` resolver folds the three sources into the single `Option<String>` that feeds `state.prompt`; the "is stdin a terminal" check and reader are injected so the rules are unit-tested without a TTY. More than one explicit source is a usage error; a missing file or empty-after-trim prompt is rejected; one trailing newline is trimmed and interior newlines preserved; terminal-with-nothing returns `None`, preserving the interactive no-prompt fallback. Resolution happens once before the prompt-key auto-resume scan so the key matches. Contained to `derrick-cli`; no `derrick-flow`/schema/token-accounting change. | §5.3 / `derrick-cli` |
| D65 | **Host-CLI-only model routing — no BYOK.** Derrick routes ALL model inference through exactly five host CLIs: `claude` (Claude Code), `codex`, `copilot` (GitHub Copilot CLI), `opencode`, `aider`. Derrick holds no API keys. Each host CLI manages its own auth; derrick's `AuthStore` shrinks to env-passthrough only (forwarding vars such as `GH_TOKEN` and proxy vars to child processes). Supersedes D12. The `derrick-models` `Model`-trait providers collapse into a single **host-delegated provider** that maps a `ModelDef` to a host, builds a `HostRequest`, calls the `derrick-tools` host adapter, and wraps the `HostResponse` into a one-shot completion stream. `derrick-models` gains a dependency on `derrick-tools` (no cycle — tools is a leaf). The direct-API `anthropic` provider and the API-key mode of `openai-cli` are deleted; `shell` survives as a bespoke-envelope escape hatch. Host–model mapping: anthropic models route via `claude`; OpenAI models via `codex`; `copilot`, `opencode`, and `aider` are multi-model front-ends that authenticate themselves. opencode and aider are first-class pipeline hosts (D41 extended): the `Host` enum gains `Opencode` and `Aider`. Per-host model normalisation: `claude` strips a leading `anthropic/`; `codex` strips `openai/`; `copilot` strips any `provider/` prefix but keeps its own dotted ids (e.g. `claude-sonnet-4.6`) without dot↔dash translation; `opencode` and `aider` pass `provider/model` verbatim. A curated, current per-host catalogue (owned by `derrick-tools/src/catalogue.rs`) drives defaults and `derrick models check`; unknown model ids WARN and still pass through to the CLI — hard-fail on model id is explicitly prohibited. Current catalogue (May 2026): claude → `claude-opus-4-8` / `claude-sonnet-4-6` / `claude-haiku-4-5`; codex → `gpt-5.5` / `gpt-5.4` / `gpt-5.4-mini` / `gpt-5.2-codex`; copilot → `gpt-5.4` / `gpt-5.3-codex` / `claude-sonnet-4.6` / `claude-haiku-4.5` / `gpt-5.4-mini`; opencode and aider use `provider/model` strings (a few curated, else WARN). `derrick models check` is now implemented with the warn-not-fail rule (completes D15). `MissingCredential` and `AuthStore::require()` are removed. `reqwest` drops out of `derrick-models`. D30's host-vs-provider split stands for invocation shape: the `shell` provider and all non-inference subprocess calls are unchanged; inference-path providers are host-delegated wrappers. *Generalised by D79*: the fixed five-host list becomes five CLI *runtimes* inside an open runtime registry; optional API runtimes (`anthropic-api`, `openai-api`) and local/self-hosted runtimes (`ollama`, `openai-compatible`) are added alongside them. CLI runtimes remain the default path and their behaviour is unchanged. | §6.5 / D12 / D15 / D30 / D34 / D41 |
| D66 | **Model forwarding on the run path; opencode/codex/aider as crew executor hands.** Refines D65/§6.5. Four clauses. (1) **Model forwarding**: derrick now forwards the role-bound `ModelDef.model()` as a normalised `--model` to the host CLI on the RUN path when a model is configured; when unset the host keeps its own default. Two surfaces: pipeline `host:` steps (`derrick-flow` `execute_role_step`) resolve the step's role → model and set `HostRequest.model`; crew executor hands (see clause 2) pass the resolved executor model as `--model`. The host still loads all its own context (CLAUDE.md/AGENTS.md, agents, skills, hooks) — only model *selection* becomes derrick-driven when configured. This narrows, not revokes, the §6.5 "conductor not orchestra" principle: model choice is the one thing derrick now asserts. (2) **opencode/codex/aider as crew executor hands**: D65 made them first-class *pipeline* hosts; this decision makes them first-class *crew executor hands* — new `HandKind` variants `Codex`, `Opencode`, `Aider` executed by a generic host-CLI hand dispatcher in new crate `derrick-hand`, which runs an assigned ticket through the `derrick-tools` host adapter (so the D65 `--model` normalisation and headless flags apply uniformly). The existing `claude` and `copilot` dispatchers are left unchanged for now; folding them onto the generic path is a deferred follow-up. (3) **Copilot crew hand is excluded from `--model`**: the crew `copilot` dispatcher targets GitHub's *cloud* Copilot agent via the API (creates an issue and polls the PR); there is no local CLI and the model is server-side. Model forwarding does NOT apply to the cloud copilot hand. The pipeline `host: copilot` step is a separate local-CLI path and does forward `--model`. (4) **Schema migration**: the `hands.kind` SQLite column currently has `CHECK (kind IN ('claude','copilot','human'))`. Adding the new kinds requires migration 0003, which recreates the `hands` table with the expanded CHECK set (or drops the CHECK and relies on the Rust `HandKind` `FromStr` as the sole source of truth), preserving `owner`/events foreign keys. No config-version bump. **Deferred**: per-host `tools.{codex,opencode,aider}` config blocks (MVP uses hardcoded poll defaults); refactoring the Claude/Copilot dispatchers onto the generic host-CLI path. | §6.5 / §8.1 / §8.2 / D30 / D41 / D65 |
| D67 | **Foreman-driven adaptive model selection.** Refines D65/D66/§6.5. Five clauses. (1) **`auto` sentinel**: a role's model id may be a concrete PIN (always used as-is) or one of the sentinels `auto` / `auto:light` / `auto:standard` / `auto:heavy`. `auto:*` is a hard tier override that ignores the ticket's complexity; plain `auto` maps to the tier that matches the ticket's complexity. The executor role defaults to `auto`. `auto` is never passed to a host CLI — it is resolved to a concrete model id (or omitted to let the host default) before `HostRequest` is built. `derrick models check` treats only the exact sentinels (`auto` / `auto:light` / `auto:standard` / `auto:heavy`) as PASS once the host CLI is present; lookalikes such as `auto-foo` are validated as ordinary pinned model ids. (2) **Per-host model tiers** live in the D65 catalogue (`derrick-tools/src/catalogue.rs`): each host gains an ordered light / standard / heavy tier mapping — claude: haiku-4-5 / sonnet-4-6 / opus-4-8; codex: gpt-5.4-mini / gpt-5.4 / gpt-5.5; copilot: claude-haiku-4.5 / gpt-5.4 / gpt-5.3-codex; opencode and aider: the `provider/model` string from their curated catalogue entry at the matching tier. Complexity→tier: Low→light, Standard→standard, Heavy→heavy; missing complexity → standard. (3) **`Ticket.complexity`**: a new `Option<Complexity{Low,Standard,Heavy}>` field on the `Ticket` struct, persisted via migration 0004 (a clean `ADD COLUMN complexity TEXT` on the `tickets` table). Complexity is produced by the `tasks` generation step: each task heading carries an HTML-comment marker `<!-- complexity: low|standard|heavy -->`; the tasks→tickets bridge parses and stores it. Missing or garbled values are treated as Standard. Complexity is advisory — it never blocks dispatch. (4) **All crew hands are local CLIs and all participate**: clarifies D66 clause 3, which incorrectly characterised the copilot crew hand as a cloud API path and excluded it from `--model`. The crew wires the LOCAL copilot CLI (`LocalCopilotHandDispatcher`); the cloud GitHub-issue dispatcher is intentionally not wired as a crew hand. The local copilot CLI, like codex / opencode / aider / claude, receives `--model` and participates in tier selection. Model selection for each ticket is resolved inside the three crew dispatchers (`derrick-hand` generic, `LocalCopilotHandDispatcher`, `ClaudeHandDispatcher`) via a shared selector reading `ctx.ticket.complexity`. (5) **Host selection vs model selection**: D66 narrowed §6.5 so derrick asserts model *identity* when configured; D67 refines that to: the user picks the HOST (the executor role's `ModelDef.provider`); the foreman picks the best MODEL within that host per ticket, by tier. An explicit model PIN in `derrick.yaml` always wins over tier selection. | §6.5 / D30 / D41 / D65 / D66 |
| D68 | **Per-ticket hand worktree lifecycle (hybrid).** Supersedes the D66 deferral that left success-path hand-worktree cleanup out of scope. Both local hand dispatchers (`LocalCopilotHandDispatcher`, `HostCliHandDispatcher`) create a per-ticket `git worktree add` checkout but previously only removed it on a failure path, leaking `.derrick/{copilot,host}-worktrees/<id>` dirs the foreman TTL pass never reclaimed (untracked by any `worktrees` row). Resolution is hybrid: **(a)** each per-ticket worktree is tracked as a ticket-keyed `worktrees` row — `run_id` namespaced `ticket:<id>`, storing the dispatcher's explicit caller-chosen path (distinct from run-keyed rows whose path `reserve_worktree` derives from `worktree_root`) — via two new inherent `pub` methods on `NativeSubstrate`, `register_ticket_worktree(ticket_id, branch, path)` / `forget_ticket_worktree(ticket_id)`; this reuses the existing table (no migration, `SCHEMA_VERSION` unchanged) and needs no `Substrate` trait change (dispatchers hold `Arc<NativeSubstrate>`), and the cleanup pass reclaims abandoned ticket rows unchanged since it keys on `path`+`run_id`. **(b)** A shared helper `foreman::prune_ticket_worktree_dir(repo_root, path)` plus `forget_ticket_worktree` removes the checkout the moment a ticket reaches a terminal hand state (`InReview`/`Done`) or its hand is released/fails — applied identically to both dispatchers (the copilot `PollTask` previously leaked on every non-success path too). Policy: register on create; prune dir + forget row on terminal-success or release/failure; KEEP both (TTL backstop) when left for an operator (`auto_dispatch` off) or when the CLI exited without reaching `InReview`. Safe because the verify/merge flow (`verify_in_review_ticket`, copilot `open_stacked_pr`) observes merges via `gh`/SHAs on `repo_root` and never touches the per-ticket checkout. Accepted caveat: a hand running past `worktree_ttl` (24h) could have its tracked worktree pruned mid-run — identical to the existing run-worktree risk and far beyond the 1h default `poll_timeout`. | §8.2 / §8.6 / D66 |
| D69 | **Codex PreToolUse/PostToolUse hooks implemented; D34 deferred stance resolved.** `derrick init` now writes `.codex/settings.toml` with scrub and caveman hooks mirroring the Claude Code D29 path. Hook format is the same JSON-equivalent structure: `PreToolUse` (derrick:scrub, `derrick scrub --tool bash`) and `PostToolUse` (derrick:caveman, `derrick caveman --intensity lite`) both on matcher `Bash|Read|Write|Edit|Glob|Grep`. `CodexHost::run()` passes `--dangerously-bypass-hook-trust` so hooks fire in non-interactive automation. The "Codex tool I/O not scrubbed" warning on `derrick init` is removed. *Supersedes D34.* | §9.B.2 / §9.B.3 / T011 |
| D70 | **Assay reviewer-instruction envelope.** The assay prepends reviewer instructions (`assay_system_prompt`) to the prompt it sends through the host CLI. This is a deliberate, narrow exception to §6.5 "hosts own their own context" — the same narrowing pattern as D66's `--model` clause. Derrick asserts the reviewer's task framing because the assay IS derrick's own feature, not a user pipeline step; the host's broader AGENTS.md / skills are still respected. The verdict contract is now a strict final `**Verdict:** accept\|revise\|reject` line, parsed fail-closed: a response that lacks this exact line is treated as `reject`. Full reviewer quorum is required in multi-reviewer mode — a reviewer that fails to emit the verdict line counts as a `reject` against the quorum. | §7 / §9.C.2 |
| D71 | **Stacking backends implemented.** The graphite (`gt`) and git-spice (`gs`) backends in `derrick-stack` are real, tested implementations — not v1 stubs. Restack-conflict policy (D19) is unchanged. `derrick doctor` checks the configured backend's binary (`gt` for graphite, `gs` for git-spice, `git`+`gh` for native) and fails the doctor check if the configured backend binary is absent. Supersedes any prose implying these backends were future work. *Superseded by D72* (graphite and git-spice backends deliberately removed). | §8.5 / D19 |
| D72 | **Native-only stacking: derrick owns its stacking engine.** The graphite (`gt`) and git-spice (`gs`) third-party backend adapters are removed from `derrick-stack`; the native backend (plain git + `gh`) is the sole `StackBackend` implementation. Legacy configs naming `graphite` or `git-spice` fail with an actionable error pointing the user at `native`. Owning the stacking engine beats adapting to third-party CLIs whose semantics derrick cannot guarantee: restack correctness (D19 conflict-bail, D20 branch ownership) depends on derrick observing and controlling the exact git operations. D19, D20, D21, and D22 are unchanged — they govern the native engine. The `StackBackend` trait remains as the §8.6 extension seam for future backends. Supersedes the adapter clauses of D17 and D71. | §8.5 / D17 / D71 |
| D73 | **Native stacking engine v2: topological cascade restack, whole-stack submit, merge-cascade PR retarget, stack navigation table.** Owning the engine (D72) obligates feature parity with the removed third-party tools where derrick's pipeline needs it; D19/D20/D21/D22 are unchanged. Four capabilities added: (1) `derrick stack restack` processes the `blocks` DAG in deterministic topological order (tie-break: ordinal then ticket id); a D19 conflict blocks only the conflicting ticket and poisons its transitive descendants — independent subtrees continue. (2) `derrick stack submit` walks the batch in stack order, opens missing PRs with the correct base (parent branch for non-roots, `main` for roots), and retargets existing PRs whose base is stale via `gh pr edit --base`. Submit retargets unconditionally rather than reading the current base first — `gh` treats a no-op base change idempotently, and one extra gh call beats a read-then-write race. (3) The foreman's merge-cascade (`restack_dependents`) also retargets the child PR's base after rebase+force-push; `NotSupported` is tolerated (warn + Note), same posture as the force-push gate. (4) `derrick stack submit` maintains an idempotent marked section (`<!-- derrick-stack-nav … -->`) in each stacked PR body listing the stack with the current PR highlighted; replace-if-present, append-if-absent. `derrick stack show` renders a PARENT column with tree indentation by DAG depth in topological order. `StackBackend` gains three additive default methods (`retarget_pr`, `set_pr_body`, `pr_body`) that return `NotSupported`; the native backend overrides all three; `NoneStackBackend` inherits the defaults. §8.6 seam preserved. | §8.5 / D72 |
| D74 | **Rename \`add\` → \`drill\` across the full user surface. Supersedes the command naming in D46 and D50.** \`derrick add\` drove the entire dark-factory pipeline (spec → clarify → assay → plan → tasks → batch → foreman dispatch). \`add\` is the verb of passive list-appending (\`git add\`, \`npm add\`) — it framed derrick as a backlog/queue tool rather than the build/execution engine it is. \`drill\` is the verb the oil-derrick metaphor implies and fits the existing vocabulary (site / foreman / hand / dispatch). Applied consistently: (a) **CLI** — \`derrick drill "<prompt>"\` is the canonical command; \`add\` is retained as a hidden, deprecated alias. (b) **Run subcommand** — \`derrick run drill\`; the runner also accepts the legacy \`pipeline_id\` \`"add-feature"\` as a deprecated alias so existing run manifests remain resumable. (c) **Slash command and skill** — \`/drill\`; the plugin ships \`commands/drill.md\` and \`skills/drill/SKILL.md\`. (d) **\`pipeline_id\` string** — canonical value is \`"drill"\`; \`"add-feature"\` is a deprecated alias accepted at runtime. (e) The D64 \`--prompt-file\`/stdin surface moves with the rename — \`derrick drill --prompt-file\` and \`derrick run drill --prompt-file\`. English prose uses of "add"/"adds" for general actions are unchanged — only command-name references are renamed. | §5.3 / §6 / §10.2 / §11 / D46 / D50 / D64 |
| D75 | **Hand pid for process liveness.** The `hands` table gains a `pid INTEGER NULL` column (migration 0005, additive NULL default — migration-safe). `register_hand` records the dispatcher's spawned child pid; clean release sets it back to NULL. The foreman cleanup pass uses `kill(pid, 0)` liveness as a second signal alongside the existing 30-minute `last_seen` heartbeat TTL before abandoning a hand and requeuing its tickets — a dead pid is authoritative for "the agent process is gone" even before the heartbeat TTL elapses, and a live pid suppresses premature abandonment when heartbeats are stale. The `Substrate` trait gains `register_hand_with_pid`; the existing `register_hand` is retained for human/external hands. | §8.6 / foreman |
| D76 | **Structured hand telemetry events.** The free-text `Note` bodies that crew dispatchers (`derrick-hand` HostCliHandDispatcher, `derrick-claude`, `derrick-copilot` local, `derrick-substrate-native` HumanHandDispatcher) currently emit to report hand progress are replaced by three typed `EventKind` variants: `HandStarted { hand, pid, ticket }`, `HandProgress { hand, snippet }`, `HandExited { hand, code, stats }`. `HandProgress` is throttled to at most one event per hand per 2 seconds and only on meaningful-change (latest stdout line differs from the last emitted snippet), bounding event-log growth; the snippet is capped at ~80 display columns. The TUI's `build_hand_rows` (`derrick-tui/src/data.rs`) is the first consumer and no longer string-matches `"exited successfully"` / `"hand stats:"` bodies. `Note` events remain for genuinely free-form operator messages. Append-only; no supersession of D66's hand-kind work. | §8.6 / §5.5 / `derrick-tui` / foreman |
| D77 | **`PipelineStepStarted` event: bridge live run telemetry into the persisted plane.** D60/D61 gave the launching terminal live step progress via the in-process `ProgressReporter` trait (`step_started`/`step_output`/`step_finished`), but that plane is not persisted and `derrick observe` only sees step *completion* (`PipelineStepCompleted`). The flow runner now emits a `PipelineStepStarted { step_id, index, total }` event scoped to `EventScope::Worktree { run_id }` at the same call sites that later emit `PipelineStepCompleted` (`runner.rs` serial + parallel-group branches), giving the dashboard mid-step liveness without polling the launching process. Live per-line agent output is not duplicated into the event log (volume); the TUI continues to tail `.derrick/runs/<id>/step-<id>.log` for that (D78). The `ProgressReporter` trait is unchanged; this is a substrate-side mirror of its `step_started` callback. | §5.3 / §5.7 / D60 / D61 |
| D78 | **Factory view — 8th tab in `derrick observe`.** A new `Factory` tab (hotkey `8`) in the ratatui dashboard renders an ASCII factory floor driven by the D75/D76/D77 telemetry. Workstations = active worktree rows; each worker avatar is a unicode glyph chosen by `HandKind` (claude/copilot/codex/opencode/aider/human). Worker animation states are driven by recent events on `EventScope::Hand`: `HandStarted` → worker arrives; throttled `HandProgress` → worker "hammering" (braille frame cycle); `TicketTransitionedToInReview` → box placed on conveyor; `HandExited` → worker leaves; `TicketVerifiedMerged` → box ships to dock. The conveyor is rendered from the `links` dependency graph (`Blocks` edges); the shipping dock counts merged PRs; the smokestack puffs when `ForemanStatus.mode` is Attached/Detached and idles when Stopped. A sub-second animation tick (~100 ms `tokio::time::interval`) is layered on the existing 1 Hz data refresh — animation state (per-worker frame counter) is local to the TUI; substrate polling stays at 1 Hz / on `notify` fs event, and ratatui's diff rendering keeps the 10× redraw cheap. For per-line agent output the tab tails `.derrick/runs/<id>/step-<id>.log` (already written per D60/D61) rather than expanding the event log. Read-only: the tab never mutates state, consistent with §5.7. | §5.7 / D75 / D76 / D77 |
| D79 | **Runtime-based AI architecture and simple configuration.** Supersedes-in-part D65 (fixed five-host list) and D12 (no direct-API auth). Derrick separates three concepts: **runtime** (*how* derrick invokes the model — `claude-cli`, `codex-cli`, `copilot-cli`, `opencode-cli`, `aider-cli`, `anthropic-api`, `openai-api`, `openai-compatible`, `ollama`, `shell`), **provider** (*who* serves the model — `anthropic`, `openai`, `openrouter`, `ollama`, etc.), and **model** (the identifier, passed through untouched; no alias tables; releasing a new model id never requires a derrick code change). The binding is `stage → model-alias → {runtime, model, …}`. CLI runtimes remain the default path and are behaviourally unchanged. API runtimes and local/self-hosted runtimes (Ollama, LM Studio, LiteLLM, vLLM, OpenAI-compatible) are opt-in. A runtime registry (`ClaudeCliRuntime`, `CodexCliRuntime`, `CopilotCliRuntime`, `OpenCodeRuntime`, `AiderRuntime`, `AnthropicApiRuntime`, `OpenAiApiRuntime`, `OpenAiCompatibleRuntime`, `OllamaRuntime`, `ShellRuntime`) owns invocation, auth, model forwarding, error handling, streaming, and telemetry; adding a runtime is a registry entry, not an architectural change. Config accepts a structured `models:` block (`runtime` + `model` + optional `provider`, `endpoint`, `base_url`, `auth`, `auth_env`, `auth_mode`, `params`, `capabilities`) and an optional short syntax (`fast: claude-cli:claude-sonnet-4-6`) that expands internally. `ai.preset` generates editable starter config. `ModelCapabilities` carries streaming, tools, json_mode, vision, prompt_cache, context_window, max_output_tokens; validation fails only when an explicit stage `requires:` is unmet. `derrick models check` emits PASS / WARN (unknown model id, recoverable) / FAIL (missing runtime binary, unmet explicit capability, missing required auth env). Existing configs are fully backward-compatible: legacy `provider: claude|codex|copilot|opencode|aider` maps to the corresponding `-cli` runtime; `roles:` / pipeline `role:` bindings keep working; `endpoint`/`base_url` fields that were previously parsed-and-ignored become meaningful for API/compatible/local runtimes. No `CONFIG_VERSION` bump. | §6.5 / §6.5.1 / D12 / D65 |
| D80 | **Centralised multi-repo survey hub: engine stays in `derrick-survey`, hub is an in-workspace layer.** Extends D54–D57; does not supersede them. The `derrick-survey` engine (tree-sitter extraction, SQLite+FTS5 schema, search/context/impact/status query logic) is repo-agnostic and shared as-is; `derrick-survey-hub` (new crate, `derrick survey hub` subcommand) wraps it to serve N indexed repos from one long-lived process over rmcp's streamable HTTP/SSE transport. A separate repository is explicitly ruled out — it would force either publishing the engine or duplicating the extraction pipeline, and two extraction pipelines is the one outcome to avoid. The per-repo stdio server and its D57 `.mcp.json` wiring are untouched; the hub is purely additive. Single-binary constraint (D11/D54) is preserved. Hub adds: workspace registry (N repos, each its own `index.db` or namespaced rows, keyed by workspace id), network transport (HTTP/SSE via rmcp), and per-tool `workspace`/`repo` selector argument for routing. Three open questions remain (OQ2–OQ4): freshness strategy for non-live-tree checkouts, auth/multi-tenancy, and routing scheme. | §9.B.8a / D54 / D55 / D56 / D57 |
| D81 | **Hub freshness model: poll-on-query TTL (self-healing floor) + explicit refresh tool (proactive path). Extends D80; does not supersede it. Resolves OQ2.** The phase-1 hub built an index at connect time and never refreshed it. A centralised hub cannot rely on a live local `notify` watcher (the mechanism used by the per-repo stdio server) because the hosted repos are usually not the operator's live working tree — the watcher approach is rejected as the primary mechanism. Push-only (CI/webhook triggers a rebuild) was considered and rejected as the *sole* mechanism: it has no self-healing — a producer that forgets to notify leaves a workspace silently stale. Chosen hybrid: **poll-on-query TTL** guarantees eventual correctness with no external wiring, and an **explicit refresh tool** (`derrick_survey_refresh`, workspace-scoped) gives CI/git hooks a proactive, low-latency path to force a rebuild when they know something changed. Mechanism: each workspace carries a `last_checked` timestamp; on a query past the configured TTL the hub first runs a cheap staleness probe (`status().pending`, size/mtime based) and only triggers an incremental rebuild if files actually changed, then answers the query. A single-flight guard prevents concurrent queries from triggering duplicate rebuilds of the same workspace. The existing `dirty` flag (wired in phase 1) arms the staleness banner during a rebuild. TTL is configurable in `hub.yaml` via a `freshness_ttl_secs` field with a sensible default. | §9.B.8a / D80 |
| D82 | **`WorkspaceSource` abstraction: `Local { root }` vs `Pushed { db_path }`. Extends D80/D81; does not supersede them. Resolves OQ5.** The hub's query layer depends only on `index.db`, not on how it was produced. This enables two permanent sourcing modes, selected per workspace in `hub.yaml`. **`Local { root }`**: the hub holds a working tree on disk, builds the index itself, and refreshes it via the D81 poll-TTL + `derrick_survey_refresh` mechanism — unchanged from the current implementation. **`Pushed { db_path }`**: no source on the hub; an operator or CI places a prebuilt `.db` (rsync / shared volume / scp) at a configured path; the hub opens and serves it, performing an **atomic hot-swap** (open-new → swap `Arc<Survey>` → drop-old) when the file changes, gated by the freshness TTL and forceable by `derrick_survey_refresh`. Schema portability is already solved: `PRAGMA user_version` (currently 2) + the `SchemaTooNew` hard-error in `migrate()` accept or reject a pushed DB cleanly — no new stamping work required. The authenticated HTTP upload endpoint is explicitly deferred to OQ3 (auth), since that path needs bearer-token gating. Rationale: the index DB is the natural seam (the serving tools `search`/`context`/`impact`/`status` are untouched regardless of source); registry mode keeps source code off shared infrastructure and makes freshness push-based; the atomic swap is the only genuinely new mechanism. Modes may be mixed within one `hub.yaml`. Default the hosted product to `Pushed`; keep `Local` as the zero-setup on-ramp. Sequenced before OQ3 (auth) and OQ4 (routing). | §9.B.8a / D80 / D81 |
| D83 | **Hub auth: scoped bearer tokens, TLS via reverse proxy. Extends D80; supersedes D80's loopback-only rationale for the no-auth case. Resolves OQ3.** The hub config gains an optional `auth` section listing bearer tokens; each token grants a set of workspace ids (or `*` for all) and a set of capabilities (`read`, `refresh`; `upload` reserved for the deferred Pushed upload endpoint that D82 requires). Clients present `Authorization: Bearer <token>`; the hub authenticates (constant-time comparison) and authorizes the requested workspace per the token's scope — one mechanism covers authn, per-workspace authz, and read/write capability separation. **Bind policy** (supersedes D80's phase-1 loopback-only rationale): with no `auth` configured, the hub stays loopback-only — a non-loopback bind without auth is rejected. With `auth` configured, a non-loopback bind is permitted; the operator has opted into authentication. **TLS** is terminated by a reverse proxy (nginx / Caddy / cloud LB); the hub speaks plain HTTP behind it. Built-in hub TLS is out of scope. First slice: token authn + per-workspace authz on the existing read/refresh tools + gated non-loopback bind. The authenticated Pushed upload endpoint (D82's deferred write path, `upload` capability) is deferred to a follow-up and is not a new open question. Rationale: simplest ops for derrick's self-hosted stage (no CA/IdP), forward-compatible (mTLS / OIDC can layer on as additional auth methods), and the token→workspace→capability mapping directly delivers the multi-tenancy half of OQ3. Sequenced before OQ4 (routing). | §9.B.8a / D80 / D82 |
| D84 | **Hub routing scheme: explicit `workspace` argument (default) + `derrick_survey_list_workspaces` discovery tool + optional path-prefix routing. Subdomain/host-based routing rejected. Extends D80; does not supersede it. Resolves OQ4. Completes the D80–D84 hub arc (D80 hub, D81 freshness, D82 sourcing, D83 auth, D84 routing).** **Default scheme (D80, unchanged):** clients connect to the hub's root MCP endpoint and pass a `workspace` argument on each tool call — backward-compatible with any client already targeting a single workspace. **Discovery:** a new `derrick_survey_list_workspaces` tool returns the workspace ids the caller may reach, auth-scoped (a `*` token sees all configured ids; an `Ids`-scoped token sees only its subset), so clients enumerate rather than hardcode workspace ids. Works in both addressing modes. **Path-prefix routing (additive):** the hub also serves a per-workspace MCP endpoint at a per-workspace path (e.g. `/w/<id>`), where the workspace is fixed by the path so the per-call `workspace` argument is optional; this gives clean per-site URLs that a reverse proxy can route and authorize on, without wildcard DNS. Authorization (D83) binds to the resolved workspace — whether it came from the `workspace` argument or the path pin — so a bearer token must still be scoped to that workspace regardless of addressing mode. **Subdomain/host-based routing rejected:** wildcard DNS and wildcard TLS are required and the operational cost only earns its place at many-tenant scale; revisit if that scale is reached. | §9.B.8a / D80 / D83 |
| D86 | **AI Profiles, Budgeting & Intelligent Model Selection.** Builds on D79 (runtime-based AI architecture) by introducing three layered additions. **(1) Profiles** — a named set of stage overrides that temporarily replaces role bindings without touching `derrick.yaml`. `profiles:` in config maps a profile name to a `stages:` binding exactly like the top-level `stages:` section; `--profile speed` on `derrick drill` or `derrick run drill` calls `Config::with_profile`, applies the overrides in-memory, and the resulting config is used for that run only. Six built-in profiles ship as compiled defaults: `speed` (fast alias everywhere, minimum reviewers), `balanced` (no overrides — labels the baseline config), `quality` (strong models, multi-reviewer assay), `cheap` (cheap alias everywhere), `local` (local alias everywhere, fails if no local runtime configured), `ci` (same bindings as speed, `ci: true` flag suppresses interactive prompts). Built-in profiles reference the conventional aliases `fast`, `strong`, `cheap`, `local`; missing aliases are warned-and-skipped (never a hard error). User-defined profiles in `derrick.yaml` take precedence over built-ins by name and can reference any alias in the model registry. `derrick profile list` prints all profiles (built-in + user); `derrick profile show <name>` shows one profile's bindings. `default_profile:` in config sets the baseline profile applied when no `--profile` flag is given; wizard exposes this as a single-select during `derrick init --wizard`. **(2) Model estimate metadata** — an optional `estimated:` block on a model alias (`latency: low\|medium\|high`, `cost: very_low\|low\|medium\|high\|very_high`, `quality: low\|medium\|high\|very_high`) informs intelligent selection and the cost report. Unknown values are ignored (forward-compatible). **(3) Budget system** — optional `budgets:` in config adds per-ticket, daily, and monthly cost caps (`max_cost: <f64>` USD). Before execution the foreman estimates cost from `estimated.cost` tiers; if the estimate exceeds the active budget it warns and prompts (y/N) or auto-fails in CI mode. `derrick cost` reports estimated spend by tier, CLI usage, API usage, and local usage. Active profile is visible in `derrick status`, `derrick observe` Overview tab, and structured logs. Architecture constraint: built-in profiles are treated exactly like user-defined profiles at the application layer — no hardcoded names in the selection or dispatch logic beyond the initial fallback lookup. `CONFIG_VERSION` unchanged (all new top-level fields and model-def fields are additive optional). | §6.5 / §6.5.1 / D79 |
| D85 | **Pluggable spec-provider seam: `tools.specify.provider` selects `speckit` \| `native` \| `import` across the spec→plan→tasks surface. Refines D2/D3; default clause superseded by D87.** The `specify`/`plan`/`tasks` pipeline steps generalise from a single host-delegated speckit invocation into a selectable provider — all three produce the **same on-disk artifacts** (`specs/<NNN>-<slug>/{spec,plan,tasks}.md` + `.specify/feature.json`) so downstream `clarify`/`assay`/`bridge` are unchanged. **(a) `speckit`** — the host-delegated path (D30), preserved as an explicit compatibility provider. **(b) `native`** — a derrick-owned in-process generator (new `derrick-specify` crate) using host-CLI completions: **survey-grounded** (derrick writes the `grounding:` front-matter from the real index, so the model never invents symbol/path names; degrades gracefully with no index), **clarify-first** (the clarify Q&A runs *before* drafting), and **schema-validated** (YAML front-matter + required headings; `validate_spec/plan/tasks` return Reject/Warn findings with one bounded repair pass), wired through roughneck/caveman/prompt-caching for token efficiency. **(c) `import`** — bring-your-own spec/PRD from a local file (v1): passed through verbatim if it already matches the schema, else normalised by one model call; `import.{plan,tasks}` each select `native`\|`speckit`\|`import` downstream. Remote sources (GitHub issue / Notion / Confluence) are a documented deferred limitation — derrick's Rust cannot call agent-side MCP tools; export to a local file. **Seam shape:** a closed `SpecProviderKind` enum + a `run_spec_phase` resolver in `derrick-flow` (not a `dyn` trait), matching the `StackBackendKind`/`SubstrateBackendKind` selection precedent rather than the open `Substrate`/`StackBackend` trait seams. **Back-compat:** the provider is consulted only for a *bare* `specify`/`plan`/`tasks` step (no `role`/`host`/`command`/`runner`); a step pinning `host:`+`command:` runs verbatim through the existing role path. The absent `role` is load-bearing, not incidental: it is part of what classifies a step as bare (`is_bare` in `derrick-flow::steps`, `is_bare_spec_step` in `derrick-config`), so a step that carries a `role:` deliberately does *not* route to the seam. The native generator never reads the step's `role:` — it resolves its own `drafter`/`proposer` tiers from `roles:` internally, and `derrick doctor` validates those two tiers (not the step's role) resolve to a model. `CONFIG_VERSION` unchanged (additive optional fields, per D66/D67). `derrick init` has a provider prompt; `derrick doctor` reports the active provider and scopes the speckit-on-PATH check to the `speckit` provider or steps that pin `/speckit.*`. New MCP-backed import sources touching company systems require IT approval. | §4 / §5.2 / §5.3 / D2 / D3 / D30 |
| D87 | **Native spec provider is the default for new sites.** Supersedes D85's default-provider clause while preserving its seam and artifact contract. New `derrick init` configs write `tools.specify.provider: native` and bare `specify`/`plan`/`tasks` steps, so a standard `/drill` path no longer requires speckit. Speckit remains an explicit compatibility provider: selecting it pins `/speckit.specify`, `/speckit.plan`, `/speckit.tasks`, and `/speckit.analyze` as host commands, and existing configs with explicit `/speckit.*` steps continue to run verbatim. The default native pipeline drops the optional `/speckit.analyze` step; schema validation in `derrick-specify` plus the assay step are the native quality gates. `derrick doctor` checks for speckit only when provider `speckit` is selected or a pipeline step explicitly pins `/speckit.*`. `CONFIG_VERSION` remains unchanged because existing config semantics are preserved and the change affects generated defaults. | §4 / §5.2 / §5.3 / D85 |
| D88 | **Roles may bind both model/provider and an agent file.** The `roles:` map keeps its existing short form (`reviewer: codex-gpt5`) and gains an expanded form (`reviewer: { model: codex-gpt5, agent: .codex/agents/integrations-engineer.md }`). `model` is the existing model alias, so provider switching remains a one-line role/model edit; `agent` is an optional repo-local path to the host-native agent instruction file for that role. During role-step execution, derrick reads the configured file from the step working tree and prepends it as explicit user-configured role context before the step prompt. This is not a hidden derrick system prompt and does not replace host-native rule loading: Claude/Codex/Copilot/opencode/aider still load `AGENTS.md`, `.codex/instructions.md`, `.github/copilot-instructions.md`, sub-agent files, skills, hooks, and plugins normally. Profiles and `stages:` overrides update only the role's model binding and preserve any configured agent path. `CONFIG_VERSION` remains unchanged because the short form is still accepted and the expanded form is additive. | §6.5 / §5.2 / D65 / D66 |
| D89 | **Split `Substrate` into focused role traits.** The 47-method `Substrate` god-trait (`derrick-substrate/src/lib.rs`) costs every implementor and mock but buys little decoupling in practice — callers hold concrete `Arc<NativeSubstrate>` in 43 places against only 6 `dyn Substrate` uses. Decision: split it into focused role traits (proposed: `TicketStore`, `EventLog`, `HandRegistry`, `ForemanState`, `WorktreeReservations`) so each caller depends only on the slice it actually uses, per the narrow-traits house rule (Interface Segregation) — a future alternate backend or `dyn` boundary becomes cheap for the slice that needs it instead of all-or-nothing on one god-trait. `NativeSubstrate` implements all of them; this is a cross-crate change and the technical shape is routed through rust-architect. Refines D11's scope-discipline principle — sign-off for substrate additions now applies per role trait rather than to a single monolithic one. | §3.1 / §8.1 / §8.6 / D11 |
| D90 | **Caveman ultra conforms to the skill: no arrows.** Caveman ultra converted causal conjunctions (`because`/`therefore`, previously also `so`) into `->` arrows, but the installed caveman `SKILL.md` strips the conjunction outright and explicitly forbids arrows ("NO arrows — measured zero token saving under tokenizer") — a direct violation of D7's byte-identical-to-the-skill rule. Decision: caveman ultra now strips the causal conjunction and emits no arrow, matching the skill exactly. Corpus tests that locked the old arrow behaviour are updated to match the corrected output. | §9.B.3 / D7 |
| D91 | **Automated skill-parity harness enforcing D7.** D7 ("caveman byte-identical to the skill at matched intensities") had no automated enforcement — `skill_parity` was a 9-line ignored placeholder test — which is exactly how the D90 drift (arrows vs. no arrows) shipped unnoticed. Decision: build a real parity harness that runs the installed caveman skill and the crate's `compress()` over the shared corpus and diffs the two outputs, wired into CI so any future D7 drift fails the build instead of silently diverging. Depends on D90 landing first — building the harness against the old arrow behaviour would lock in the wrong output. | §9.B.3 / D7 / D90 |
| D92 | **`batch_max` bounds active hands, not total in-flight footprint.** Confirmed intent: the foreman's `parallelism.batch_max` caps only tickets in `InFlight` — tickets in `InReview` (worktree still held, PR open, awaiting merge) are not counted against it. This is deliberate, not an oversight: the cap bounds concurrent *active workers* (hands actually running), not the total resource footprint held across every non-terminal ticket state. No behaviour change from this decision; §9.C now documents the semantics explicitly so the cap isn't mistaken for a footprint limit. | §9.C |

### Remaining open questions

| # | Question | Leaning | Related |
|---|---|---|---|
| OQ1 | **Survey index location as reference memory.** Should `derrick init` write a flat root `~/.claude/projects/<repo-key>/memory/MEMORY.md` summary pointing at `.derrick/index.db` (bypassing the `derrick/<site>/` namespace that Claude Code does not recursively load), or defer until Claude Code supports recursive memory-dir discovery? For v1 the survey index is discoverable via the `.mcp.json` MCP registration (D57), so the memory seed is deferred — not load-bearing. | Defer; revisit if Claude Code adds recursive memory-dir support. | D55 / D57 |
| OQ2 | **Hub freshness strategy.** ~~The existing `notify`-watcher + `(size, mtime)` incremental-reindex model assumes a live local working tree. A hub holding bare mirrors or remote checkouts breaks that assumption. Candidates: push-based reindex triggered by a CI/webhook hook on each push, or polling on a configurable interval. This is the biggest design departure from the per-repo stdio model and is unresolved.~~ **Resolved by D81**: hybrid poll-on-query TTL (self-healing floor) + explicit `derrick_survey_refresh` tool (proactive path). | Resolved — see D81. | D80 / D81 / §9.B.8a |
| OQ3 | **Hub auth and multi-tenancy.** ~~A network-exposed server holding multiple teams' source requires at minimum bearer-token gating and likely per-workspace access scoping. The stdio model never had this surface; it should be treated as first-class design, not a bolt-on. Exact mechanism (static tokens, OAuth, mTLS, per-workspace ACL table) is unresolved.~~ **Resolved by D83**: scoped bearer tokens in hub config; each token grants workspace ids and capabilities (`read`/`refresh`; `upload` deferred); TLS terminated by reverse proxy; non-loopback bind gated on auth presence. | Resolved — see D83. | D80 / D83 / §9.B.8a |
| OQ4 | **Hub routing scheme.** ~~`repo`/`workspace` selector argument on each tool (recommended starting point per D80) vs. per-workspace tool namespacing at the MCP layer. Start with the selector-arg approach; revisit if client ergonomics demand otherwise.~~ **Resolved by D84**: explicit `workspace` argument remains the default (D80, backward-compatible); a `derrick_survey_list_workspaces` discovery tool enumerates auth-scoped workspace ids; optional path-prefix routing (e.g. `/w/<id>`) gives clean per-site URLs for reverse-proxy routing without per-call `workspace` argument; subdomain/host-based routing rejected (requires wildcard DNS + TLS, not justified at current scale). All OQ2–OQ5 are now resolved. | Resolved — see D84. | D80 / D84 / §9.B.8a |
| OQ5 | **Where does indexing happen? Hub-as-indexer vs hub-as-index-registry (the `WorkspaceSource` fork).** ~~The hub's query layer depends only on `index.db`, not on how it was produced, which enables two sourcing modes. **Local (indexer mode, current):** `root` points at a working tree on the hub's disk; the hub builds and refreshes it (D81 poll-TTL + `derrick_survey_refresh`). Implies source code lives on the hub. **Pushed (registry mode):** no `root`; a producer (CI/dev machine) builds `index.db` where the code already lives and uploads it; the hub opens and serves it, never seeing source. A `WorkspaceSource` enum (`Local { root }` \| `Pushed { … }`) is the natural seam: `WorkspaceConfig` becomes a tagged enum, `WorkspaceEntry.root` moves into a `source` field, only the build/refresh path branches — the serving tools (`search`/`context`/`impact`/`status`) are untouched. Modes can be mixed within one `hub.yaml`, giving a migration path. Two new costs in registry mode only: (1) **Schema/version portability** — a pushed DB from a different `derrick-survey` binary version must be accepted or rejected cleanly. **This is already implemented**: `db.rs` uses `PRAGMA user_version` (currently 2, set by `migrations/0002_meta_table.sql`), and `migrate()` enforces a `SchemaTooNew` hard-error when the stored version exceeds `SCHEMA_VERSION`. Cross-machine portability is therefore largely solved; no new stamping work is required. (2) **Atomic swap** — replacing a pushed DB while queries are in flight requires open-new, atomically swap the `Arc<Survey>`, drop-old; the current entry only rebuilds in place. The registry write path also implies an authenticated upload endpoint, coupling to OQ3 (auth). Registry mode makes D81's poll-TTL moot (freshness becomes push-based) and reshapes the OQ4 routing discussion (a registry hub stores `.db` files only; no source on shared infra).~~ **Resolved by D82**: `WorkspaceSource` abstraction (`Local { root }` \| `Pushed { db_path }`) is adopted as permanent; `Local` is unchanged; `Pushed` serves a prebuilt DB via atomic hot-swap; authenticated HTTP upload deferred to OQ3. | Resolved — see D82. | D80 / D81 / D82 / §9.B.8a |
| OQ6 | **`derrick-adopt` detect→apply TOCTOU.** The adoption pass (§5.6) merges `hooks`/`.mcp.json`/`.claude/settings.json` against a snapshot captured at *detect* time and never re-reads disk before *promotion* (the final write) — an external edit to any of those files between detect and apply is silently lost. | Build a detect→merge revalidation pass: re-read the on-disk state immediately before promotion, re-diff against the detect-time snapshot, and refuse (or re-merge) if it changed. | §5.6 |

New questions raised during implementation will be tracked as
GitHub issues with the `design-question` label, and locked-in
answers folded back into this file as further `D` entries with
a section back-reference.

---

## 13. Naming

We're calling it **derrick**. Reasons:

- The load-bearing structure over an oil well — fits the gastown /
  petroleum metaphor lineage.
- Single point that lifts every length of pipe in and out — matches the
  "single point of entry" framing.
- Short, lowercase, doesn't collide with an existing common CLI on PATH
  (verified locally — there's no `derrick` in homebrew core formulas at
  the time of writing; will re-check before publishing).

Binary: `derrick`. Repo: `derrick`. Plugin: `derrick`.

**Crate vocabulary — petroleum metaphor lineage:**

| Name | Petroleum meaning | Derrick meaning |
|---|---|---|
| `derrick` | Load-bearing rig tower | The whole system |
| `survey` | Seismic survey: map subsurface structure before drilling | Code-graph index: map code structure before working it |

`survey` was chosen over `core` (too overloaded in computing: CPU cores, core dumps) and retains the petroleum-metaphor lineage: a seismic survey is exactly what you do before putting pipe in the ground. `derrick survey …` reads naturally as a command. It does not collide with the reserved vocabulary (site / ticket / batch / hand / foreman / dispatch / activity) or any existing subcommand.
