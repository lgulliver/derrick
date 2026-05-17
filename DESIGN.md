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

| Component | Lang | Lives in | Purpose |
|---|---|---|---|
| `derrick` CLI | Go | `cmd/derrick` | Install, init, run, doctor, config |
| Orchestrator | Go | `internal/flow` | Phase state machine, tool runner, logging |
| Tool adapters | Go | `internal/tools` | Thin wrappers around `claude`, `codex`, `gh copilot`, `gt`, `bd`, `specify` |
| Assay | Go | `internal/assay` | Derrick-native adversarial plan review (Codex-driven, courtroom-pattern) |
| Memory | Go | `internal/memory` | Seeds Claude memory files on init + per-step context budgets |
| Scrubber | Go | `internal/scrub` | Derrick-native subprocess output filter (RTK-equivalent) |
| Caveman | Go | `internal/caveman` | Derrick-native text compressor for inter-step handoff |
| Copilot adapter | Go | `internal/copilot` | Dispatches steps or beads to Copilot agents (CLI + Workspace API) |
| Substrate iface | Go | `internal/substrate` | One interface (beads, links, convoys, workers); two backends |
| Native substrate | Go | `internal/substrate/native` | SQLite-backed default execution substrate + in-process mayor |
| Gastown shim | Go | `internal/substrate/gastown` | Backend that proxies to `gt`/`bd` for users who already run gastown |
| Observe | Go | `internal/observe` | Aggregated read-only view (talks to substrate iface, not backends) |
| Config | Go | `internal/config` | Load + validate `derrick.yaml` |
| Repo templates | files | `templates/` | What `derrick init` copies in |
| Plugin | md+sh | `templates/.claude/` | `/add-feature` command + skill |
| Install script | bash | `scripts/install.sh` | Curlable bootstrap |

Why Go? Same toolchain as gastown/beads, single static binary for the
install script, trivial to ship via Homebrew or GitHub release.

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
  role: foreman        # default role when the foreman is started

# Underlying tool versions / opt-outs
tools:
  speckit:   { enabled: true,  version: ">=0.4.0" }
  assay:     { enabled: true,  reviewer: codex, model: "gpt-5", rounds: 1, strict: false }
  substrate:
    backend: native           # native (default) | gastown | none
    mode: crew                # solo | copilot | crew     (see §8)
  copilot:
    enabled: true
    cli: "gh copilot"           # gh copilot CLI; Workspace API later
    model: "gpt-5-codex"        # whichever Copilot model the user has
    agent_identity: derrick-hand     # for crew mode handoff
  # No external rtk/caveman dependency — derrick ships its own (see §9).

# /add-feature pipeline. Steps run in order; any can be skipped via flag.
pipeline:
  - id: specify
    runner: claude
    model: sonnet
    command: "/speckit.specify {{prompt}}"
  - id: clarify
    runner: claude
    model: sonnet
    command: "/speckit.clarify"
    skippable: true
    default_skip: false
  - id: plan
    runner: claude
    model: opus
    command: "/speckit.plan"
  - id: checkpoint
    runner: human
    skippable: true
    default_skip: false
    prompt: "Review plan.md at {{feature_dir}}/plan.md — continue? [y/N]"
  - id: assay
    runner: derrick               # in-process; calls codex directly (see §7)
    inputs: [{{feature_dir}}/spec.md, {{feature_dir}}/plan.md]
    rounds: "{{tools.assay.rounds}}"
    on_reject: halt               # halt | warn — fail closed by default
  - id: analyze
    runner: claude
    model: opus
    command: "/speckit.analyze"
  - id: tasks
    runner: claude
    model: sonnet
    command: "/speckit.tasks"
  - id: bridge
    runner: derrick                # creates tickets in the substrate (native or gastown shim)
    inputs: [{{tasks_md}}]
    batch: "{{batch}}"
  - id: foreman
    runner: derrick                # starts the foreman loop (in-proc native, or gastown's mayor via shim)
    role: "{{site.role}}"

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
  lock_ttl: 1h         # multi-feature lock TTL (§9.C.5)
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
$ curl -fsSL https://derrick.dev/install | bash
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

Interactive, but answers can be passed via flags / a config file for CI.
Steps:

1. Detect or ask for: project name, rig prefix, primary language(s),
   default model preference.
2. Refuse to clobber existing `.specify/`, `.claude/commands/add-feature.md`,
   or `derrick.yaml` unless `--force`.
3. Bootstrap:
   - `derrick.yaml` from template.
   - `.specify/` skeleton (constitution stub, memory, scripts) by shelling
     out to `specify init --here` then patching in derrick's extensions.
   - `.specify/extensions/derrick/scripts/tasks-to-tickets.sh`
     (derived from the blacksmith bridge but speaking derrick's
     vocabulary; emits to whichever substrate backend is selected).
   - `.claude/commands/add-feature.md` (the slash command).
   - `.claude/agents/` placeholders for the standard roles
     (foreman, assay-reviewer, hand-default). User can edit/extend.
   - `CLAUDE.md` block appended (or created) pointing at derrick's docs.
4. Register the rig with gastown (`gt rig add` or equivalent), unless
   `--no-gastown`.
5. `derrick doctor` on the freshly initialised repo.

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

All commands talk to the substrate interface. Output uses
derrick's vocabulary; if the gastown backend is selected, the
shim translates inbound (`bead → ticket`, `convoy → batch`,
`polecat → hand`, `mayor → foreman`, `rig → site`).

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
| `derrick runs` | Last N derrick pipeline runs, exit status per step | local `.derrick/runs/` |
| `derrick run <id>` | Replay the manifest of one specific run | local |

(Gastown backend only: `derrick mail` exposes `gt mail`. Native
backend doesn't ship mail in v1.)

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

### Why Codex specifically

It's a different family (OpenAI), it ships a stable `codex exec`
non-interactive mode, and the user already has it installed for
courtroom. `tools.assay.reviewer` accepts `codex | gemini | bedrock`
later — the adapter lives in `internal/assay/reviewers/<name>.go`.

### Boundaries with the *other* underlying tools

We do **not** fork speckit. We *optionally* shell to gastown (§8.3).
Derrick's contract:

- **speckit**: invoked via `claude /speckit.*`. Derrick assumes speckit
  writes `.specify/feature.json` and a per-feature directory; that's the
  same assumption flight.sh already makes.
- **gastown (when selected as backend)**: invoked via `gt` and `bd`
  CLIs only. No DB poking, no direct dolt access. See §8.3 / §8.6.
- **Default backend is derrick's own** (§8.2), which has no external
  tool dependency beyond SQLite.

If any underlying tool changes its CLI shape, derrick updates its
adapter in `internal/tools/<tool>.go` or `internal/substrate/<name>/`.
That's the only blast radius.

---

## 8. Execution substrate — derrick-native by default, gastown optional

The pipeline produces a `tasks.md`. *Something* then has to track
those tasks as work units, sequence them, dispatch them to hands,
and report state. That something is the **execution substrate**.

### 8.-1 Glossary (derrick's vocabulary)

We deliberately do **not** reuse gastown's nouns. When both systems
are in play this makes them distinguishable; when only derrick is in
play the words stand on their own.

| Derrick | Role | Gastown equivalent (when `backend: gastown`) |
|---|---|---|
| **site** | Workspace registered with the substrate | rig |
| **ticket** | One unit of work | bead |
| **batch** | Ordered named group of tickets for one feature | convoy |
| **hand** | An executor (claude / copilot / human) | polecat |
| **foreman** | Orchestrator loop that walks ready tickets and dispatches | mayor |
| **dispatch** | The verb for assigning a ticket to a hand | sling |
| **activity** | Recent event timeline | trail |
| **link / blocks** | Typed edges between tickets | (same) |
| **prefix** | Short site code, e.g. `ti` → `ti-47` | (same) |

The gastown shim translates inbound (`bead → ticket`, etc.) so
derrick's CLI output is consistent regardless of backend.

### 8.0 The decision: own it

Originally derrick depended on gastown for this. Gastown is excellent
but it's a large surface (50+ `gt` commands, a fragile dolt data
plane, multi-agent federation features most users don't need) and
depending on it gives derrick the same install friction users
already have today. Following the same logic that led us to own
assay, scrubber, and caveman, we ship a **derrick-native execution
substrate** as the default.

- **Default**: derrick's own minimal substrate, SQLite-backed,
  single-binary, no server, no dolt.
- **Opt-in**: full gastown, for users who already run it (blacksmith
  and similar) or who need its federation/mail/multi-rig features.

Selected via `tools.execution_substrate`:

```yaml
tools:
  execution_substrate: native   # native (default) | gastown | none
```

`none` is solo-mode shorthand (pipeline ends at `tasks.md`).

### 8.1 The model (one shape, two backends)

We define **one** logical model with **derrick's own vocabulary**
and implement it twice (once natively, once as a thin shim over
gastown). The rest of derrick — observability surface, runners,
memory layers — talks to the model, not the backend.

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
  v1 runs in-process inside derrick; out-of-process daemon later.

Deliberate vocabulary split from gastown: site/ticket/batch/hand/
foreman/dispatch are derrick's terms, used in the CLI, the docs,
and all user-facing output regardless of which backend is in play.
When `backend: gastown` is selected, gastown's own terms
(rig/bead/convoy/polecat/mayor) apply *behind* the shim — they
never leak through.

Things gastown has that the native substrate **does not** ship in
v1: agent mail, multi-site federation, watchdog services, merge
queue, persistent agent identity beyond per-ticket ownership,
epic staging. Users who need these flip to `backend: gastown`.

### 8.2 The native substrate

**Storage**: SQLite at `.derrick/derrick.db`. Schema is small —
`tickets`, `links`, `batches`, `hands`, `events`. WAL mode, single
writer (the in-process foreman), many readers (observability
surface). File-based means no server, trivial backup, trivial
gitignore.

**Foreman loop**: a goroutine in the derrick process that polls
ready tickets, dispatches, and watches for completion via hand
hooks. When `derrick run add-feature` returns, the foreman either:
- exits cleanly if all tickets are `done`, or
- detaches into `.derrick/foreman.pid` and continues in the
  background. `derrick foreman stop` ends it; `derrick foreman
  logs` tails it.

**Hands**:
- `copilot` hand shells to `gh copilot agent run --task <body>
  --label "derrick/ticket=<id>"` and watches the resulting PR
  for the ticket-id label to detect completion.
- `human` hand just marks the ticket `in_flight` and waits for
  the user to flip it `done` via `derrick ticket done <id>`.
- `claude` hand writes a `.derrick/queue/<ticket-id>.md` file
  and prints a hint — the user picks it up in their Claude
  Code session.

**Concurrency**: §9.C `parallelism.batch_max` caps how many hands
run at once. The foreman honours `blocks` links strictly.

**Mutation API**: in-process Go, plus a small subset of CLI
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

### 8.3 The gastown backend (opt-in)

When `tools.substrate.backend: gastown`, derrick:

- Skips creating `.derrick/derrick.db`.
- The `bridge` step shells to `bd create` / `bd link` (gastown
  creates beads; derrick's CLI still calls them tickets to the user).
- The `foreman` step shells to `gt prime --rig <name> --role mayor`
  (gastown spins up its mayor; derrick still calls it the foreman).
- The observability surface (§5.5) reads from gastown CLIs
  instead of SQLite and translates terms inbound:
  `bead → ticket`, `convoy → batch`, `polecat → hand`,
  `mayor → foreman`, `rig → site`.
- The mutation commands above proxy to `bd` / `gt`.

The shim lives in `internal/substrate/gastown/`. The native
implementation lives in `internal/substrate/native/`. Both
satisfy the same Go interface (`internal/substrate/Substrate`).
Adding a third backend later (e.g. GitHub Projects directly) is a
new package, not a rewrite.

### 8.4 Modes revisited

`tools.gastown.mode` is now misnamed — it's really
`tools.substrate.mode`. Three values, semantics unchanged:

- **`solo`** — `execution_substrate: none`. Pipeline ends at
  `tasks.md`. The user works from the markdown.
- **`copilot`** — substrate present (native by default), but no
  foreman loop: tickets are dispatched directly to Copilot agents
  and derrick polls completions inline.
- **`crew`** — substrate present, foreman running, hands fanning
  out. This is the dark-factory mode.

### 8.5 Copilot as a first-class runner

(Unchanged from previous design — Copilot is still both a
pipeline-step runner and the batch executor in `mode: copilot`.
The Copilot adapter `internal/copilot/` now sits *behind* the
substrate interface in crew mode, called by the foreman.)

### 8.6 Dolt awareness (gastown backend only)

When using the gastown backend, derrick:

- Surfaces `gt dolt status` in `derrick doctor` (warning if
  degraded).
- **Never** runs `gt dolt stop`, never `rm -rf ~/.dolt-data`.
- On a step that fails with the classic Dolt symptoms (timeouts,
  connection refused), prints the exact diagnostic recipe from
  `gt`'s own runbook before exiting.

Native backend has no dolt and no daemon — `derrick doctor` checks
SQLite file accessibility and that's it.

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

**9.A.5 Lifecycle.** `derrick memory list | show | prune | unmemoize`.
Unmemoize removes everything under `derrick/<rig>/` for clean
uninstall.

### 9.B — Tokens

Every byte across a model boundary earns its place. Seven knobs:

**9.B.1 Model tiering** (per step, overridable in `derrick.yaml`):

| Step | Model | Why |
|---|---|---|
| `specify`, `clarify`, `tasks` | sonnet | Mechanical, structured |
| `plan`, `analyze` | opus | Genuinely hard reasoning |
| `assay` | codex (gpt-5) | Adversarial, different family |
| `bridge`, `mayor` | n/a | Subprocess |
| `dispatch-copilot`, `runner: copilot` | Copilot model | Mechanical at Copilot rates |

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
saved, actual usage. This is the feedback loop that keeps the
other six knobs honest.

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
list. If two are configured (e.g. `[codex, gemini]`), they run in
parallel against the same brief. Derrick reconciles: unanimous
accept → accept; any reject → reject; split → user-decides or
re-plan, per `on_split:`. Cost is one extra reviewer call;
benefit is meaningfully harder-to-game adversarial review.

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

**9.C.5 Multi-feature parallelism.** Two `/add-feature`
invocations against the same repo, at the same time, must not
clobber each other's `.specify/feature.json`. Each derrick run
gets a private feature_dir lock (`.derrick/locks/<run-id>`) and
passes `SPECIFY_FEATURE_DIRECTORY` to every sub-claude call,
mirroring the flight.sh fix. The lock auto-expires on run
completion or after `state.lock_ttl`.

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
  `derrick config`.
- Observability surface (§5.5): `derrick status`, `tickets`,
  `ticket`, `batch`, `foreman`, `activity`, `hands`, `orphans`,
  `runs`.
- Token tooling: `derrick scrub`, `derrick caveman`, `derrick gain`.
- `/add-feature`, `/derrick-doctor`, `/derrick-resume`,
  `/derrick-status` slash commands.
- Templates for `.specify/`, `.claude/`, `derrick.yaml`, constitution
  stub, tasks-to-tickets bridge.
- macOS + Linux install script. Binary published as GitHub release.

**Later:**

- Homebrew formula and a Windows build.
- `derrick run <custom-pipeline>` for repos that want flows beyond
  add-feature (e.g. "hotfix", "spike", "refactor").
- `derrick observe` — full TUI built on top of the §5.5 read APIs.
- Copilot Workspace HTTP API backend (replaces `gh copilot` CLI in
  `mode: copilot`).
- Optional Slack feedback hook so hand completions ping a channel.

---

## 12. Open questions to resolve before coding

1. **Plugin distribution.** Claude Code plugins today come from a
   marketplace JSON. Stand up our own (`derrick.dev/marketplace.json`)
   so install is a single curl.

2. **Speckit init under the hood.** `specify init --here` writes a lot
   of opinionated content. Derrick can either (a) shell out and patch
   the result, or (b) ship its own minimal `.specify/` skeleton. (a) is
   less code; (b) is more robust to speckit changes. Leaning: (a) with a
   pinned speckit version range in `tools.speckit.version`.

3. **Constitution defaults.** What ships in the constitution stub? A
   blank file is unhelpful; blacksmith's is too prescriptive. Probably
   a short template with placeholders and pointers to the speckit docs.

4. **Assay reviewer default.** `codex` is the obvious v1 choice
   (already installed for courtroom, stable non-interactive mode).
   But should we ship a `gemini` adapter in v1 as well so users
   without Codex auth can still get adversarial review? Leaning:
   codex-only v1, gemini in v1.1.

5. **Caveman in-process vs. via skill.** §9.3 describes inter-step
   summarisation. Cleanest is to invoke the existing `caveman` skill
   via `claude` for the summary. Cheapest is to apply the caveman
   shaping rules in Go directly. Leaning: in-process for speed and to
   avoid recursive Claude invocations, fall back to the skill if our
   ruleset misses an artifact type.

6. **Memory namespacing.** §9.4 writes into the user's global memory
   dir. We need a stable prefix (`derrick/<rig-name>/...`) so multiple
   derrick-managed repos on the same machine don't collide, and so
   `derrick init --unmemoize` can clean up without touching unrelated
   memories.

7. **Scrubber / caveman compatibility with the originals.** We're
   shipping our own implementations (§9.2, §9.3). Should we treat
   byte-for-byte compatibility with RTK and the caveman skill as a
   contract (so users can swap freely), or just call ours
   "RTK-inspired" / "caveman-inspired" and diverge as needed?
   Leaning: caveman byte-identical at matched intensities (it's a
   pure shaping function); scrubber drift-tolerant (CLI output shapes
   change upstream too often to chase).

8. **Copilot Workspace API timing.** §8.3 says v1 = `gh copilot` CLI,
   v1.1 = Workspace HTTP API. The API isn't fully GA at design time.
   If it slips, the `mode: copilot` experience is degraded (CLI is
   single-task, no good parallelism). Do we hold `mode: copilot` for
   v1.1, or ship a CLI-only v1 with documented limits? Leaning: ship
   CLI-only v1 with `--serialise` default = true so it's predictable,
   document the upgrade path.

9. **Multi-reviewer assay split-verdict policy** (§9.C.2). If two
   reviewers disagree (codex: accept, gemini: reject), the safe
   default is fail-closed (=reject). But that gives veto power to
   the more pessimistic model. Alternative: `on_split: human`
   prompts the user. Leaning: `on_split: reject` default,
   `on_split: human` opt-in for solo mode.

10. **Cross-feature lessons quality control** (§9.A.4). The lesson
    extractor is itself an LLM call after each batch closes. If
    its output is noisy, it pollutes future plans. Need a quality
    gate — probably "lesson must reference at least one specific
    ticket id or constitution section, else discard." Worth
    piloting before turning on by default.

11. **Lock contention in multi-feature mode** (§9.C.5). The
    `SPECIFY_FEATURE_DIRECTORY` env var solves serial conflicts
    but the underlying speckit may still write shared state we
    haven't audited. v1 should ship with a loud warning if two
    `derrick run`s overlap, and only relax to silent parallelism
    once we've verified speckit is fully feature-dir-scoped.

12. **Native substrate scope creep** (§8.2). We've explicitly
    excluded mail, federation, refinery, witness/deacon, persistent
    agent identity, mountain-eater. Users moving from blacksmith
    will notice the absences. Risk: feature-by-feature, the native
    substrate grows back into gastown. Mitigation: every additional
    substrate feature needs explicit sign-off; if we're building
    three of them in a quarter, the answer is probably "switch the
    user to `backend: gastown`," not "extend the native one."

13. **Migration path between backends.** A user starts on native,
    hits a scale wall, wants gastown. Or vice-versa. v1 ships a
    `derrick migrate-backend --to gastown` (and `--to native`) that
    exports tickets/batches/links from one and imports into the
    other. Open question: do we attempt to preserve ticket IDs
    across the move, or accept ID rewrite with a `legacy_id`
    label? Leaning: ID rewrite, document it loudly.

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
