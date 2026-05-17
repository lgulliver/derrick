# Derrick — Design

> **derrick** *(n.)* The load-bearing tower over an oil well. The structure
> that lifts every length of pipe in and out of the hole. Without it, the
> rig is a hole in the ground.

Derrick is a unified layer over **speckit**, **courtroom**, and **gastown**.
One install, one config, one command (`/add-feature`). It works in any repo,
for any user, without them needing to know how the underlying tools talk to
each other.

---

## 1. Problem

Today, getting the speckit → courtroom → gastown flow running in a new repo
means:

1. Install `claude`, `codex`, `gt`, `bd`, `specify` CLIs separately.
2. Install the `courtroom` Claude Code plugin.
3. Run `specify init` and tune `.specify/` for the project.
4. Author a `rigs.json` entry and bootstrap the rig with `gt`.
5. Write a per-repo `flight.sh` (see `blacksmith/.specify/extensions/blacksmith/commands/flight.sh`)
   that hardcodes feature paths, agent identities, and checkpoint policy.
6. Wire `tasks-to-beads.sh` as a SpecKit post-tasks hook.
7. Author a CLAUDE.md and AGENTS.md that explain the flow.
8. Document the runbook, the dolt-server caveats, the mayor session etc.

This is bespoke per repo. `flight.sh` is the proven shape but it is **glued
to blacksmith** — its phase labels, its five-service rule, its
constitution path, its `gt prime --rig blacksmith --role mayor`.

We want: **any user, any repo, single command, `/add-feature` UX.**

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
  rig with gastown.
- **One primary command**: `/add-feature <prompt>` runs the full
  dark factory pipeline — spec → assay → plan → tasks → batch →
  foreman / Copilot agents.
- **One front door for observability**: `derrick status` is the
  answer to "what's going on?" — never `gt status` + `bd query` +
  `gt mail` separately.
- **Reusable**: nothing in derrick assumes blacksmith. Project-
  specific rules live in the repo's constitution + `derrick.yaml`,
  not in derrick.
- **Transparent**: every underlying tool call is logged and exit
  codes propagate. Nothing is magic. Power users can still call
  `gt`, `bd`, `claude /speckit.specify` directly.

### Non-goals (v1)

- Re-implementing speckit, courtroom, or gastown. Derrick **orchestrates**;
  it does not replace.
- A GUI. CLI + slash command only.
- Self-hosted dolt management. Users who use gastown inherit gastown's
  dolt server contract; derrick will surface its health but not run it.
- Cross-language polyglot dispatching beyond what gastown already does.

---

## 3. Architecture

```
┌──────────────────────────────────────────────────────────────────┐
│                      User (in any repo)                          │
│   $ derrick init        $ /add-feature "build the X service"     │
└─────────────────┬───────────────────────────┬────────────────────┘
                  │                           │
                  ▼                           ▼
        ┌──────────────────┐        ┌───────────────────────┐
        │   derrick CLI    │        │  /add-feature command │
        │     (Go bin)     │        │  (Claude Code plugin) │
        └────────┬─────────┘        └───────────┬───────────┘
                 │                              │
                 │  reads/writes                │ shells out via
                 │  derrick.yaml                │ derrick run …
                 ▼                              ▼
        ┌─────────────────────────────────────────────────┐
        │              Derrick Orchestrator               │
        │                                                 │
        │   Phase pipeline  ←→  derrick.yaml (per repo)   │
        │   Tool detection  ←→  ~/.derrick/state.json     │
        │   Logging         ←→  .derrick/runs/<ts>.log    │
        └──────────┬────────┬────────┬────────┬───────────┘
                   │        │        │        │
                   ▼        ▼        ▼        ▼
                ┌────┐  ┌──────┐  ┌────────┐  ┌────┐
                │spec│  │court │  │gastown │  │mayor│
                │kit │  │room  │  │bd/sling│  │ gt │
                └────┘  └──────┘  └────────┘  └────┘
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
| Scrubber | `crates/derrick-scrub` | Subprocess output filter (RTK-equivalent), pure functions, zero-copy where possible |
| Caveman | `crates/derrick-caveman` | Text compressor for inter-step handoff, pure functions |
| Copilot | `crates/derrick-copilot` | Dispatches steps or tickets to Copilot agents (CLI + Workspace API) |
| Models | `crates/derrick-models` | Provider trait; adapters for API providers, local runtimes, CLI shells (see §6.5) |
| Adopt | `crates/derrick-adopt` | Brownfield detection of AGENTS.md, CLAUDE.md, agents/, skills/, docs (see §5.6) |
| Substrate trait | `crates/derrick-substrate` | One async trait (`Substrate`); a native impl, future impls slot in behind it |
| Native substrate | `crates/derrick-substrate-native` | SQLite-backed execution substrate + in-process foreman |
| Stack | `crates/derrick-stack` | PR stacking: trait + native / graphite / git-spice backends (see §8.5) |
| TUI | `crates/derrick-tui` | `derrick observe` — ratatui-based interactive dashboard (see §5.7) |
| Observe | `crates/derrick-observe` | Aggregated read-only view (talks to substrate trait) |
| Config | `crates/derrick-config` | Load + validate `derrick.yaml` (serde) |
| Repo templates | `templates/` | What `derrick init` copies in |
| Plugin | `templates/.claude/` | `/add-feature` command + skill |
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

Trade-off accepted: build times are slower than Go's, and the gastown shim
(if we ever add one) means shelling to a Go CLI, not vendoring its types.
That cost is bounded.

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
models:
  claude-opus:    { provider: anthropic, model: "claude-opus-4-7" }
  claude-sonnet:  { provider: anthropic, model: "claude-sonnet-4-6" }
  codex-gpt5:     { provider: openai-cli, cli: "codex exec", model: "gpt-5" }
  copilot:        { provider: copilot-cli, cli: "copilot",  model: "gpt-5-codex" }
  # examples of BYOM (none enabled by default):
  # gemini-pro:   { provider: google,  model: "gemini-2.5-pro" }
  # local-llama:  { provider: ollama,  base_url: "http://localhost:11434", model: "llama3.3:70b" }
  # bedrock-claude: { provider: bedrock, region: "eu-west-2", model: "anthropic.claude-opus-4-7-v1" }
  # azure-gpt5:   { provider: azure-openai, endpoint: "...", deployment: "gpt-5" }

# Role bindings — pipeline steps name a role; the role names a model.
# Changing one model changes the whole class of step that uses it.
roles:
  proposer:  claude-opus       # plan + analyze (heavy reasoning)
  drafter:   claude-sonnet     # specify + clarify + tasks (mechanical)
  reviewer:  codex-gpt5        # assay (adversarial, different family)
  executor:  copilot           # ticket dispatch in crew/copilot mode
  summariser: claude-sonnet    # inter-step caveman-augmented summary, if used

# Underlying tool versions / opt-outs
tools:
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

# /add-feature pipeline. Steps run in order; any can be skipped via flag.
# Each step names a role (resolved via `roles:` above) or runner: derrick / human.
pipeline:
  - id: specify
    role: drafter
    host: claude                  # which host CLI loads the prompt (see §6.5)
    command: "/speckit.specify {{prompt}}"
  - id: clarify
    role: drafter
    host: claude
    command: "/speckit.clarify"
    skippable: true
  - id: plan
    role: proposer
    host: claude
    command: "/speckit.plan"
  - id: checkpoint
    runner: human
    prompt: "Review plan.md at {{feature_dir}}/plan.md — continue? [y/N]"
    skippable: true
  - id: assay
    runner: derrick               # in-process; uses the reviewer role(s) (§7)
    inputs: [{{feature_dir}}/spec.md, {{feature_dir}}/plan.md]
    rounds: "{{tools.assay.rounds}}"
    on_reject: halt               # halt | warn — fail closed by default
  - id: analyze
    role: proposer
    host: claude
    command: "/speckit.analyze"
  - id: tasks
    role: drafter
    host: claude
    command: "/speckit.tasks"
  - id: bridge
    runner: derrick               # creates tickets in the substrate
    inputs: [{{tasks_md}}]
    batch: "{{batch}}"
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
  (preferred model, courtroom rounds, etc).
- Falls back to a baked-in default shipped with the binary.

Templates use Go `text/template` with a small context (`prompt`, `rig`,
`feature_dir`, `tasks_md`, `batch`, env). No general expression language.

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
   - Installs the `derrick` Claude Code plugin (`/add-feature`,
     `/derrick-doctor`) into `~/.claude/plugins/`.
   - Writes `~/.derrick/config.yaml` with sensible defaults.
3. Prints next-step: `cd your/repo && derrick init`.

No repo touched at this stage.

### 5.2 Init (one-time, per repo)

```
$ cd ~/repos/my-project
$ derrick init
```

Brownfield-first by default. Interactive but flag/file-driven for CI.
See §5.6 for the full brownfield adoption contract; the short version:

1. **Adoption pass.** Walk the repo for existing AGENTS.md,
   CLAUDE.md, `.claude/`, constitution-like docs, trackers. Classify
   each as *adopt as-is*, *reference*, or *augment*.
2. **Identify.** Detect or ask for: project name, ticket prefix,
   primary language(s), preferred provider/host for each role.
3. **Propose.** Print the proposed `derrick.yaml` and the list of
   files derrick would create or append to. Nothing is moved or
   rewritten unless the user opts in.
4. **Constitution comes from speckit, not from derrick.** Derrick
   does *not* ship a constitution template. If no constitution
   exists, init runs speckit's own constitution flow
   (`/speckit.constitution` via the detected host CLI, or
   `specify init --here` if a host CLI isn't available — see §5.2.1
   for the detect-then-defer logic). Brownfield repos with an
   existing constitution-like doc are referenced via
   `guardrails.constitution_path` instead. Either way the
   constitution belongs to speckit; derrick only points at it.
5. **Bootstrap (only after confirm)**:
   - `derrick.yaml` from template, pointing at *existing* paths
     wherever they were found.
   - `.specify/` only if speckit isn't already present (handled
     by the detect-then-defer logic above).
   - `.specify/extensions/derrick/scripts/tasks-to-tickets.sh`.
   - `.claude/commands/add-feature.md` — refuses to overwrite an
     existing command of the same name without `--force`.
   - `.claude/agents/` *additions*, never replacements. Names that
     collide with the user's existing agents are skipped.
   - `CLAUDE.md` block appended only with `--append-agents-md` or
     the equivalent confirm.
6. Register the site with the substrate (native SQLite), unless
   `--no-substrate`.
7. `derrick doctor` on the freshly initialised repo.

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

### 5.3 `/add-feature` (the primary UX)

What the user types in Claude Code:

```
/add-feature build a webhook ingest endpoint with idempotent dedupe
```

What happens (this is the load-bearing flow):

1. The slash command body resolves to `derrick run add-feature --prompt "..."`.
2. Derrick walks the `pipeline:` from `derrick.yaml`.
3. Each step:
   - Logs to `.derrick/runs/<utc-ts>/step-<id>.log`.
   - On `runner: claude`, shells out to `claude --model X "<command>"`.
   - On `runner: gt`/`bash`, shells out and streams output.
   - On `runner: human`, prompts on stdout and reads stdin (or auto-skips
     when `--no-checkpoint`).
4. After `specify`, derrick reads `.specify/feature.json` to pin
   `feature_dir` for subsequent steps (mirrors flight.sh — solves the
   "stale feature.json" bug it already fixed).
5. Failure of any step halts the pipeline with a numbered error and the
   exact resume command (`derrick run add-feature --resume-from plan`).

Variants exposed as slash commands or flags:

| Flag | Behaviour |
|---|---|
| `--no-clarify` | Skip the clarify step |
| `--no-checkpoint` | Skip the human plan review |
| `--no-assay` | Skip cross-model adversarial review |
| `--dry-run` | Run through tasks; do not create beads or start mayor |
| `--phase <label>` | Apply a phase label to every bead |
| `--resume-from <step>` | Restart from a given pipeline step |

### 5.4 `derrick doctor`

Inspects the local install + the current repo and prints a coloured
checklist:

- Binaries: `claude`, `codex`, `gh`, `gt`, `bd`, `git` (versions + paths).
- Claude Code plugin presence and version.
- Repo: `derrick.yaml` valid, `.specify/memory/constitution.md` exists
  and non-empty, gastown rig registered, dolt server reachable if
  gastown enabled.
- Exit code is the count of failing checks (handy for CI).

### 5.5 Observability — derrick as the front door for "what's going on?"

Once a feature is in flight, the user shouldn't need to remember
which underlying tool answers which question. Derrick exposes a
flat, predictable surface that aggregates gastown/bd/Copilot reads
into one view. Everything here is **read-only** — these commands
never mutate state.

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
| `derrick hands` | Hands registered to this site, current task, last heartbeat | `Hand.List` |
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
  into `/add-feature` resume contexts.
- **Mode-aware.** In `mode: solo` most of these collapse — `derrick
  status` shows the current spec dir and tasks.md progress, no
  tickets. In `mode: copilot` it shows Copilot agent dispatch
  state, no foreman. In `mode: crew` it shows the lot.
- **No mutation.** If the user wants to claim/close/comment, they
  use `bd` directly. Derrick is deliberately not a wrapper around
  every write path; that surface is gastown's by design and
  derrick doesn't want to keep up.
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

Concrete behaviours:

| Existing | Derrick's default behaviour |
|---|---|
| `AGENTS.md` | Reference it from `guardrails.agents_md`. Do not overwrite. Append a short derrick block at the bottom (opt-in via `--append-agents-md`). |
| `CLAUDE.md` | Same — referenced, optionally appended. |
| `.claude/agents/<name>.md` | Treat as authoritative. Do not create `hand-default.md`, `foreman.md` etc. if names overlap. |
| `.claude/commands/` | Add `/add-feature` and friends *alongside*. Refuse to overwrite an existing command of the same name without `--force`. |
| `.claude/skills/` | Untouched. Derrick's skills are added separately. |
| Existing constitution-like file | Reference it as `guardrails.constitution_path`. Do not write a new one. |
| Existing `.specify/` | Reuse. Patch only `.specify/extensions/derrick/`. |
| No constitution at all | Offer to generate a *minimal stub* (`derrick init --constitution-stub`) or run a one-shot LLM pass that drafts one from existing docs (`--constitution-from-docs`). Both opt-in. |
| Existing tracker (Linear, Jira, GitHub Projects) | Skip native substrate ticket creation; offer a future adapter (out of scope for v1, recorded as a constraint). |
| Existing CI / pre-commit / git hooks | Untouched. |

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
│  [1] Overview   [2] Tickets   [3] Stack   [4] Activity   [5] Tokens   [6] Memory │
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
   §9.B's seven knobs.
6. **Memory** — current site's memory entries (project /
   reference / feedback / lessons). Lets the user spot stale or
   wrong entries; `d` flags one for deletion (writes to a queue,
   not applied until the user runs `derrick memory prune`).

#### Live updates

Two mechanisms, both running:

- **File watcher** (notify crate) on `.derrick/derrick.db`,
  `.derrick/runs/`, `.derrick/foreman.pid`. Fires the moment
  anything changes.
- **Tick timer** (1s) as a fallback for things the watcher
  doesn't surface (e.g. age counters in the in-flight pane,
  remote PR state freshness).

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
  add-feature.md            # the primary UX
  derrick-status.md         # wraps `derrick status` (caveman-formatted)
  derrick-doctor.md         # wraps `derrick doctor`
  derrick-resume.md         # wraps `derrick run --resume-from`
skills/
  add-feature/SKILL.md      # full phase-by-phase instructions
README.md
```

`commands/add-feature.md` is intentionally thin: it parses arguments,
verifies derrick is installed, and then defers to the skill for the
actual workflow narrative — same pattern the Anthropic-shipped skills
use (a one-page command, a fat skill).

### 6.5 BYOM (Bring Your Own Model) — hosts, providers, roles

Derrick separates three concerns most tools conflate:

- **Provider** — *who serves the inference*. Anthropic API,
  OpenAI API, Google Gemini, Bedrock, Azure OpenAI, Ollama,
  llama.cpp, or a CLI shell (`codex exec`, `copilot`,
  `claude --print`). Adapters live in
  `internal/models/providers/<name>.go`.
- **Host** — *who the user is conversing with*. `claude` (the
  Claude Code CLI), `codex` (the Codex CLI), `copilot`, raw
  HTTP, or none. The host loads its own context: AGENTS.md,
  sub-agents, skills, plugins. Hosts are configured per pipeline
  step (`host: claude`) or implied by the provider.
- **Role** — *what the step needs done*. `proposer`,
  `drafter`, `reviewer`, `executor`, `summariser`. Pipeline
  steps name roles, never models directly.

The binding is `step → role → model → provider`, with `host`
selected per step. Changing your reviewer model from Codex to
Gemini is one line in `models:`; no pipeline edits.

#### Supported providers (v1)

| Provider | Type | Notes |
|---|---|---|
| `anthropic` | API | First-class. Prompt caching used by §9.B.4. |
| `openai` | API | Used for non-Claude reasoning roles. |
| `openai-cli` | CLI | `codex exec` etc — for users who prefer the local CLI. |
| `google` | API | Gemini. |
| `bedrock` | API | AWS Bedrock. Region + model-id. |
| `azure-openai` | API | Endpoint + deployment. |
| `copilot-cli` | CLI | Standalone `copilot` CLI (`@github/copilot`) for ticket dispatch. v1 default. |
| `copilot-sdk` | Lib | GitHub Copilot SDK in-process. v1.1 research target. |
| `ollama` | Local | `base_url` + model tag. Sensible for `summariser` role to keep tokens off the network entirely. |
| `llamacpp` | Local | Same idea. |
| `shell` | Generic | Any command that takes a prompt on stdin and emits a response on stdout. Escape hatch. |

#### Respecting the host's own rules

When derrick invokes a step on a **host** CLI (claude / codex /
copilot), it deliberately does **not** inject a system prompt,
override the host's context, or bypass the host's rule loading.
The contract:

- Derrick passes the working directory (the user's repo) and the
  step command. That's it.
- The host loads its **own** files: `CLAUDE.md`, `AGENTS.md`,
  sub-agents under `.claude/agents/`, skills under
  `.claude/skills/`, plugins, hooks, `.codex/`, `~/.codex/`,
  `.github/copilot-instructions.md`, etc. Derrick does not touch
  any of this.
- Sub-agent spawn within a step (e.g. `Agent({subagent_type:
  "Explore"})`) is the host's decision; derrick doesn't see it
  and doesn't intercede.
- Skills triggered in-session (caveman, find-skills, the user's
  custom skills) run inside the host. Derrick's *own* caveman
  implementation is for inter-step compression, not in-session.
- Derrick records *what command it sent* and *what artifact came
  back*. It does not influence the host's internal context,
  prompt expansion, or subagent behaviour. It *does* read the
  host's transcript file after the step for token telemetry only
  (§9.B.7) — read-only, post-hoc, never used to alter the host's
  next call.

This means: a brownfield repo with a carefully tuned AGENTS.md
and twenty agents gets exactly the same Claude Code behaviour
inside a derrick step as it would in a normal session. Derrick
is the conductor, not the orchestra.

When a step uses an API provider directly (no host CLI),
derrick *does* assemble the prompt — but only from declared
inputs (the artifact files in `inputs:` and the constitution),
never from arbitrary repo state. This is the only path where
derrick acts as the prompter; we keep it narrow.

#### Cost and latency knobs per model

`models.<name>` accepts:

```yaml
models:
  bedrock-claude:
    provider: bedrock
    region: eu-west-2
    model: anthropic.claude-opus-4-7-v1
    max_tokens: 4096
    temperature: 0.2
    cache: true              # prompt caching where supported
    timeout: 120s
    rate_limit: { rpm: 20, tpm: 80000 }
    cost_hint:  { in_per_mtok: 15, out_per_mtok: 75 }   # for `derrick gain`
```

Cost hints are optional but power the §9.B.7 telemetry — without
them, `derrick gain` reports token counts only, not dollars.

#### Validation: `derrick models check`

Not every role / host / provider combination is sensible
(`host: claude` + `provider: ollama` is nonsense — Claude Code
calls Anthropic, not Ollama). v1 ships:

- `derrick models check` — explicit subcommand; verifies every
  binding in `derrick.yaml` resolves to a real provider, that
  required env vars / credentials exist, and that the host/
  provider pairing is supported. Exit code is the count of
  failing checks.
- Warnings (not errors) emitted at `derrick init` and `derrick
  run` if validation finds anything off, so issues surface early
  without blocking experiments.

#### Auth

Credentials are sensitive and live outside the repo:

- **Env vars first** (CI-friendly): `ANTHROPIC_API_KEY`,
  `OPENAI_API_KEY`, `GOOGLE_API_KEY`, `AWS_*` for Bedrock, etc.
  Derrick documents the env var per provider.
- **Optional** `~/.derrick/credentials.yaml` for desktop
  convenience: one file, one set of keys, never repo-local. Mode
  0600. `derrick auth set <provider>` and `derrick auth list` for
  ergonomics.
- **Never** committed to a repo. `.gitignore` for `derrick.yaml`
  is *not* defaulted (we want it in version control), so secrets
  go through env vars or `~/.derrick/credentials.yaml` only.
- **Host-delegated providers** (claude, codex, copilot) inherit
  auth from the host CLI's own mechanism — derrick doesn't see
  those keys.

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
   to `plan.md`. Bounded by `tools.assay.rounds`.
5. **Verdict** — written to `{{feature_dir}}/assay/verdict.md` with
   the model name, rounds used, and the final accept/revise/reject.
6. **Gate** — on `reject` derrick halts and prints the verdict path.
   On `revise` past `rounds`, halts the same way. `on_reject: warn`
   downgrades to a printed warning for solo-mode repos.

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
  writes `.specify/feature.json` and a per-feature directory; that's the
  same assumption flight.sh already makes.
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
talks to the `Substrate` trait, not to SQLite directly. Adding a
second backend later means a new crate that implements the trait;
no other code changes.

- **Site** — a workspace registered with the substrate. One per
  repo. Has a name and a ticket prefix.
- **Ticket** — a single unit of work with state
  (`ready | in_flight | blocked | done | rejected`), labels, body,
  links to other tickets, owner.
- **Link** — typed edge between tickets. v1 supports `blocks`
  (sequencing) and `related` (informational).
- **Batch** — an ordered named group of tickets representing one
  feature. Closes when all member tickets close.
- **Hand** — anything that can execute a ticket. v1 hand types:
  `claude` (interactive human-driven), `copilot` (agent dispatch),
  `human` (just claimed by a person).
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
hooks. When `derrick run add-feature` returns, the foreman either:
- exits cleanly if all tickets are `done`, or
- detaches into `.derrick/foreman.pid` and continues in the
  background. `derrick foreman stop` ends it; `derrick foreman
  logs` tails it.

**Hands**:
- `copilot` hand shells to the standalone `copilot` CLI
  (`copilot agent run --task <body> --label
  "derrick/ticket=<id>"`) and watches the resulting PR for the
  ticket-id label to detect completion.
- `human` hand just marks the ticket `in_flight` and waits for
  the user to flip it `done` via `derrick ticket done <id>`.
- `claude` hand writes a `.derrick/queue/<ticket-id>.md` file
  and prints a hint — the user picks it up in their Claude
  Code session.

**Concurrency**: §9.C `parallelism.batch_max` caps how many hands
run at once. The foreman honours `blocks` links strictly. Hand
dispatch is `tokio::spawn`; completion is a `mpsc` channel back to
the foreman.

**Mutation API**: in-process Rust, plus a small subset of CLI
write commands derrick *does* expose (it can't be entirely
read-only against its own substrate):

| Command | Purpose |
|---|---|
| `derrick ticket new` | Create a ticket (used internally by `bridge`) |
| `derrick ticket done <id>` | Mark complete |
| `derrick ticket block <id> --on <id>` | Add a `blocks` link |
| `derrick ticket reopen <id>` | Re-ready a done/rejected ticket |
| `derrick batch close <name>` | Force-close a batch |

Reads (status, tickets, ticket, batch, etc.) are §5.5 already.

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
- **`native`** — derrick manages the stack itself using plain
  `git` + `gh pr create`. v1 default when stacking is enabled.
  Branches are named `derrick/<batch>/<ticket-id>`; the foreman
  sets parents based on `blocks` links; when a parent PR lands,
  derrick rebases and force-pushes all dependents.
- **`graphite`** — shell out to Graphite (`gt`). Adapter
  records `gt branch create --parent <branch>` and lets
  Graphite handle restacks. Detected automatically if
  Graphite is installed and the repo is initialised
  (`.graphite_user_config` present).
- **`git-spice`** — shell out to `gs` (git-spice). Same shape
  as the graphite adapter.

The trait `StackBackend` lives in `crates/derrick-stack`;
adapters in `crates/derrick-stack/src/backends/`.

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
   asks the backend to restack them (`git rebase --onto` for
   native; `gt restack` for graphite). Force-push uses
   `--force-with-lease`.
5. If a restack fails (merge conflict), derrick bails immediately
   (D19): the ticket moves to `blocked` with a `restack-conflict`
   label, and the activity log records the exact `git rebase
   --onto <parent> <old-base> <branch>` recipe so the human can
   resolve and resume. No auto three-way-merge attempts —
   force-pushed half-resolved branches are worse than blocked
   ones.

#### `derrick stack` subcommand

- `derrick stack` — show the current stack for the active batch:
  parent → child PRs, merge status, restack health.
- `derrick stack restack [--batch <name>]` — manual restack of
  a batch's children after a parent lands (in case the foreman
  isn't running).
- `derrick stack submit [--batch <name>]` — open PRs for every
  ticket whose hand has pushed a branch but not yet opened a
  PR (catches up after a foreman crash).

#### Configuration

```yaml
tools:
  git:
    stacking:
      backend: native            # none | native | graphite | git-spice
      branch_pattern: "derrick/{{batch}}/{{ticket_id}}"
      auto_restack_on_merge: true
      force_push: with-lease     # with-lease | off
      auto_pr: false             # D22: default off. `--auto-pr` flag overrides per run.
      draft: false               # open as draft PRs when auto_pr fires
```

#### Brownfield detection

`derrick init` detects Graphite via `.graphite_user_config` or
`~/.graphite/`. If found, it proposes `backend: graphite` in
the generated `derrick.yaml` so the user keeps their existing
stack tooling. Same idea for git-spice. Otherwise it proposes
`backend: native`.

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

The `Substrate` trait is the only contract. A future
`crates/derrick-substrate-gastown` (or `-linear`, `-github-projects`,
`-jira`) would implement the same trait — observability,
runners, and the foreman keep working unchanged. We are not
designing for this in v1; we are leaving the door open.

`derrick doctor` in v1 checks: SQLite file accessibility,
foreman PID liveness if attached, ticket schema version. No
external service, no daemon, no port.

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
- *feedback memory*: derrick's own guardrails ("never `gt dolt stop`",
  "batches never re-ordered after creation", "assay verdict is
  binding unless `--no-assay`").

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
| `specify`, `clarify`, `tasks` | `drafter` | claude-sonnet | Mechanical, structured |
| `plan`, `analyze` | `proposer` | claude-opus | Hard reasoning |
| `assay` | `reviewer` | codex-gpt5 | Adversarial, different family |
| `bridge`, `foreman` | — | n/a | Subprocess / in-process |
| `runner: copilot` steps | `executor` | copilot | Mechanical at Copilot rates |
| Inter-step summary | `summariser` | claude-sonnet (or `ollama` local) | Hot path; local is free |

Re-binding a role re-routes every step that uses it. BYOM means
you can bind `proposer` to a Bedrock-hosted Claude, or `reviewer`
to Gemini, without touching the pipeline.

**9.B.2 Scrubber (derrick-native, §3.1).** Per-tool output filters
strip CLI noise before the next step sees it. `derrick scrub <cmd>`
for ad-hoc use; auto-applied to every subprocess. Target 60–90%
reduction on subprocess noise. Rules per tool in
`internal/scrub/rules/<tool>.go`. `--raw` opts out.

**9.B.3 Caveman (derrick-native, §3.1).** Pure-Go text compressor
with three intensity levels (`lite | full | ultra`). Auto-applied
to inter-step handoff: full log to disk, compressed summary into
the next prompt. Identifiers, paths, error messages preserved
verbatim. Byte-identical to the caveman skill at matched intensities.

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
raw estimate, what scrubber/caveman/tiering/caching/memory each
saved, actual usage.

Sub-agents and skills invoked *inside* a host step (Claude
spawning Explore via Agent, a skill triggering caveman, etc.)
are invisible to derrick — but **not** to the host. Claude Code
persists session transcripts under `~/.claude/projects/<repo>/
*.jsonl`. After every step `crates/derrick-observe` reads the
transcript file matching the run's session id, sums token usage
across all turns (including sub-agent ones), and writes the real
number into the run manifest. Falls back to estimate when no
transcript is available (codex, copilot, raw API).

### 9.C — Parallelism

The pipeline has a sequential spine (`specify → plan → assay →
tasks`), but everything *around* and *after* it is parallel by
default. Derrick treats serial work as a justified exception.

**9.C.1 Batch fan-out.** Independent tickets in a batch run
concurrently. The substrate's foreman (native in-process loop,
or gastown's `gt prime` behind the shim) walks ready tickets and
dispatches them to hands, serialising only across explicit
`blocks` dependencies. Default concurrency is
`min(8, len(ready_tickets))`; configurable in `derrick.yaml`:

```yaml
parallelism:
  batch_max: 8         # max hands / copilot agents in flight
  step_max:  4         # max parallel sub-tasks within one step
  assay_max: 2         # max concurrent reviewers in multi-reviewer assay
```

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
aggregates from `gt status`, `bd query`, `gt dolt status`, and the
local manifest — all fired in parallel. The whole dashboard
returns in the slowest read, not the sum.

**9.C.4 Parallel pipeline steps.** Steps with no data dependency
on each other can be marked `parallel_group: <name>` in the yaml
and derrick will fan them out. v1 ships this for `analyze` and any
side-channel checks the user adds (lint, type-check, schema
validation). v1 does **not** parallelise `specify → plan → assay
→ tasks` — that chain stays sequential because each consumes the
previous.

**9.C.5 Multi-feature parallelism — git worktrees.** Two
`/add-feature` invocations against the same repo, at the same
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
trick from flight.sh. Worktrees give us *real* isolation, not
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

Re-running `/add-feature` with the same prompt does **not** dedupe — the
user is in charge. `--resume-from` reads `manifest.json` from the most
recent run unless `--run <id>` is passed.

`.derrick/runs/` is gitignored by default (init writes a `.gitignore`
entry). `state.json` is gitignored too. The yaml is committed.

---

## 11. What v1 ships vs. later

**v1 (this design):**

- `derrick init`, `derrick run add-feature`, `derrick doctor`,
  `derrick config`, `derrick uninstall` (reverses init cleanly).
- Observability surface (§5.5): `derrick status`, `tickets`,
  `ticket`, `batch`, `foreman`, `activity`, `hands`, `orphans`,
  `runs`.
- Token tooling: `derrick scrub`, `derrick caveman`, `derrick gain`.
- BYOM tooling: `derrick models check`, `derrick auth set/list`.
- PR stacking: `derrick stack` / `derrick stack restack` /
  `derrick stack submit`; native backend default, graphite and
  git-spice adapters detected and offered at init time.
- TUI dashboard: `derrick observe` (ratatui), six tabs covering
  Overview / Tickets / Stack / Activity / Tokens / Memory.
  Live-updating via filesystem watcher + 1s tick.
- `/add-feature`, `/derrick-doctor`, `/derrick-resume`,
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
  add-feature (e.g. "hotfix", "spike", "refactor").
- `derrick observe` mutation features (claim/close from inside
  the TUI; v1 is read-only).
- Evaluate `@github/copilot-sdk` for in-process Copilot dispatch
  (potential replacement for the `copilot` CLI backend).
- Optional Slack feedback hook so hand completions ping a channel.

---

## 12. Decisions (resolved) and remaining open questions

### Decisions taken

These were open during design and have now been resolved. Each
links back to the section where it lives.

| # | Decision | Locus |
|---|---|---|
| D1 | **Plugin distribution**: own marketplace at `derrick.dev/marketplace.json` (primary) + GitHub release artefacts (fallback). | §11 |
| D2 | **Speckit init**: detect-then-defer — use speckit if installed; fall back to a minimal `.specify/` skeleton derrick ships, with a banner requiring the user to author the constitution via `/speckit.constitution` before any pipeline runs. | §5.2 / §5.2.1 |
| D3 | **Constitution stub**: derrick does not ship a constitution template at all — speckit owns that file. Brownfield init points at the existing constitution-like doc; greenfield init forces the speckit constitution flow. | §5.2 |
| D4 | **Brownfield `--constitution-from-docs` drafts**: marked with a banner; `plan` step refuses to run until the user removes the banner. | §5.6 |
| D5 | **Assay reviewers in v1**: codex only. Other providers slot in via the model abstraction later — no extra v1 work. | §7 |
| D6 | **Split-verdict policy**: configurable per repo via `on_split:` (`reject` default fail-closed, `human`, `majority`). | §9.C.2 |
| D7 | **Scrubber/caveman compatibility**: caveman is byte-identical to the original skill at matched intensities; scrubber is drift-tolerant (CLI output evolves upstream). | §9.B.2 / §9.B.3 |
| D8 | **Caveman invocation**: in-process Rust default; falls back to invoking the caveman skill via the host when an unknown artifact type is encountered. | §9.B.3 |
| D9 | **Cross-feature lessons**: shipped in v1 with a mechanical quality gate — each lesson must reference a specific ticket id or constitution section anchor, else discarded. | §9.A.4 |
| D10 | **Multi-feature parallelism**: git worktrees per run (`.derrick/worktrees/<run-id>/`), not file locks. Substrate DB stays in the main checkout and is shared. | §9.C.5 |
| D11 | **Native substrate scope discipline**: additions require explicit sign-off and a DESIGN.md note; an OSS-facing policy in `CONTRIBUTING.md` keeps the rule visible to external contributors. | §8.1 |
| D12 | **Provider auth**: env vars first; optional `~/.derrick/credentials.yaml` (mode 0600) for desktop convenience; never repo-local; host-delegated providers inherit auth from the host CLI. | §6.5 |
| D13 | **Copilot backend in v1**: standalone `copilot` CLI (`@github/copilot`). `gh copilot` is the older extension, not what we use. Backend trait allows an SDK-based path later. `@github/copilot-sdk` recorded as a v1.1 research target. | §8.4 |
| D14 | **Sub-agent / skill telemetry**: derrick parses Claude Code's session transcript files (`~/.claude/projects/<repo>/*.jsonl`) post-step for accurate token counts; falls back to estimates for codex / copilot / raw API. | §9.B.7 |
| D15 | **Role/host validation**: `derrick models check` subcommand for explicit verification; warnings (not errors) emitted at `derrick init` and `derrick run` so issues surface early. | §6.5 |
| D16 | **v1 install surface (beyond CLI + plugin)**: shell completions (clap_complete: bash/zsh/fish), VS Code + JetBrains editor configs in templates (opt-in), `.codex/instructions.md` wrapper config written during init so codex sees the constitution, `derrick uninstall` to cleanly reverse init. | §11 |
| D17 | **PR stacking ships in v1** as a first-class concern. Default backend `native` (plain git + `gh pr create`). Graphite and git-spice adapters auto-detected at init. Foreman restacks dependents on merge using `--force-with-lease`. | §8.5 |
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
| D28 | **Supersedes D1 and D24 — GitHub-only distribution.** The `derrick.dev` domain was unavailable, so all derrick artefacts (install script, marketplace JSON, release binaries) live under `github.com/lgulliver/derrick`. The Claude Code marketplace JSON is fetched from `https://raw.githubusercontent.com/lgulliver/derrick/main/marketplace.json`. There is no longer a separate marketplace host to health-check, so D24's fallback logic collapses to a single GitHub-releases path; transient GitHub unavailability surfaces as a normal network error to the user with the documented recovery (`gh release download` or manual binary install). | §11 |

### Remaining open questions

None blocking. All v1 design questions resolved (D1–D26).

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
