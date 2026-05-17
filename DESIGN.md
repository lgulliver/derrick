# Dark Factory — Design

> **dark factory** *(n.)* A production line that runs lights-out — no
> humans on the floor, work flowing from intake to shipment by itself.
> The kit, not the foreman.

Dark Factory is a unified layer over **speckit**, **courtroom**, and
**gastown**. One install, one config, one command (`/add-feature`). It
works in any repo, for any user, without them needing to know how the
underlying tools talk to each other.

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

- **One-line install**: `curl -fsSL <url> | bash` puts the `df` binary
  and Claude Code plugin on the user's machine and verifies the deps.
- **One-line init**: `df init` in a repo writes the config, the
  templates, the hooks, the constitution skeleton, and registers the rig
  with gastown.
- **One primary command**: `/add-feature <prompt>` runs the full dark
  factory pipeline — spec → courtroom → plan → tasks → convoy → mayor.
- **Reusable**: nothing in dark-factory assumes blacksmith. Project-specific
  rules live in the repo's constitution + `dark-factory.yaml`, not in
  dark-factory.
- **Transparent**: every underlying tool call is logged and exit codes
  propagate. Nothing is magic. Power users can still call `gt`, `bd`,
  `claude /speckit.specify` directly.

### Non-goals (v1)

- Re-implementing speckit, courtroom, or gastown. Dark Factory
  **orchestrates**; it does not replace.
- A GUI. CLI + slash command only.
- Self-hosted dolt management. Users who use gastown inherit gastown's
  dolt server contract; dark-factory will surface its health but not run it.
- Cross-language polyglot dispatching beyond what gastown already does.

---

## 3. Architecture

```
┌──────────────────────────────────────────────────────────────────┐
│                      User (in any repo)                          │
│   $ df init             $ /add-feature "build the X service"     │
└─────────────────┬───────────────────────────┬────────────────────┘
                  │                           │
                  ▼                           ▼
        ┌──────────────────┐        ┌───────────────────────┐
        │      df CLI      │        │  /add-feature command │
        │     (Go bin)     │        │  (Claude Code plugin) │
        └────────┬─────────┘        └───────────┬───────────┘
                 │                              │
                 │  reads/writes                │ shells out via
                 │  dark-factory.yaml           │ df run …
                 ▼                              ▼
        ┌─────────────────────────────────────────────────┐
        │            Dark Factory Orchestrator            │
        │                                                 │
        │   Phase pipeline  ←→  dark-factory.yaml (repo)  │
        │   Tool detection  ←→  ~/.dark-factory/state.json│
        │   Logging         ←→  .dark-factory/runs/<ts>/  │
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
| `df` CLI | Go | `cmd/df` | Install, init, run, doctor, config |
| Orchestrator | Go | `internal/flow` | Phase state machine, tool runner, logging |
| Tool adapters | Go | `internal/tools` | Thin wrappers around `claude`, `codex`, `gt`, `bd`, `specify` |
| Config | Go | `internal/config` | Load + validate `dark-factory.yaml` |
| Repo templates | files | `templates/` | What `df init` copies in |
| Plugin | md+sh | `templates/.claude/` | `/add-feature` command + skill |
| Install script | bash | `scripts/install.sh` | Curlable bootstrap |

Why Go? Same toolchain as gastown/beads, single static binary for the
install script, trivial to ship via Homebrew or GitHub release.

### 3.2 Naming

- Project / repo: **dark-factory**
- Binary: **`df`** — short, two letters, common in muscle memory
  already; we collide with the Unix `df(1)` so the install script will
  warn and offer `dark-factory` as the long form if `df` is already on
  PATH. (See §10 open question.)
- Claude Code plugin: **`dark-factory`** with commands `/add-feature`,
  `/df-doctor`, `/df-resume`.

---

## 4. `dark-factory.yaml` — the per-repo contract

This is the only file dark-factory truly *owns* in the user's repo.
Everything else (`.specify/`, `.claude/`, constitution) is content
dark-factory writes once during `init` and the user then owns.

```yaml
# dark-factory.yaml — single source of truth for this repo's pipeline
version: 1

# Identity for gastown
rig:
  name: my-project
  prefix: mp           # bead prefix (mp-1, mp-2 …)
  role: mayor          # default role when `gt prime` is called

# Underlying tool versions / opt-outs
tools:
  speckit:   { enabled: true,  version: ">=0.4.0" }
  courtroom: { enabled: true,  rounds: 1, strict: false }
  gastown:   { enabled: true }

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
  - id: courtroom
    runner: claude
    command: "/courtroom --task \"implement plan at {{feature_dir}}/plan.md\" --rounds {{tools.courtroom.rounds}}"
  - id: analyze
    runner: claude
    model: opus
    command: "/speckit.analyze"
  - id: tasks
    runner: claude
    model: sonnet
    command: "/speckit.tasks"
  - id: bridge
    runner: bash
    command: "${DF_HOME}/scripts/tasks-to-beads.sh {{tasks_md}} --convoy={{convoy}}"
  - id: mayor
    runner: gt
    command: "prime --rig {{rig.name}} --role mayor"

# Project-specific guardrails surfaced into prompts and checkpoints
guardrails:
  constitution_path: .specify/memory/constitution.md
  forbid_paths: []         # paths that may not be touched by a feature
  required_labels: []      # labels every bead must carry

# Where dark-factory writes its own state inside the repo
state:
  dir: .dark-factory
  log_runs: true
```

Resolution rules:

- Repo `dark-factory.yaml` wins.
- Falls back to `~/.dark-factory/config.yaml` for user defaults
  (preferred model, courtroom rounds, etc).
- Falls back to a baked-in default shipped with the binary.

Templates use Go `text/template` with a small context (`prompt`, `rig`,
`feature_dir`, `tasks_md`, `convoy`, env). No general expression language.

---

## 5. The flows

### 5.1 Install (one-time, per machine)

```
$ curl -fsSL https://dark-factory.dev/install | bash
```

Script does, in order:

1. Detect OS/arch, fetch the right `df` binary into `~/.local/bin`.
2. Run `df doctor --install` which:
   - Verifies `claude`, `codex`, `gt`, `bd`, `git` are present.
   - If a tool is missing, prints the canonical install command (does
     **not** install it silently — these are auth-bearing tools).
   - Installs the `dark-factory` Claude Code plugin
     (`/add-feature`, `/df-doctor`) into `~/.claude/plugins/`.
   - Writes `~/.dark-factory/config.yaml` with sensible defaults.
3. Prints next-step: `cd your/repo && df init`.

No repo touched at this stage.

### 5.2 Init (one-time, per repo)

```
$ cd ~/repos/my-project
$ df init
```

Interactive, but answers can be passed via flags / a config file for CI.
Steps:

1. Detect or ask for: project name, rig prefix, primary language(s),
   default model preference.
2. Refuse to clobber existing `.specify/`, `.claude/commands/add-feature.md`,
   or `dark-factory.yaml` unless `--force`.
3. Bootstrap:
   - `dark-factory.yaml` from template.
   - `.specify/` skeleton (constitution stub, memory, scripts) by shelling
     out to `specify init --here` then patching in dark-factory's
     extensions.
   - `.specify/extensions/dark-factory/scripts/tasks-to-beads.sh` (copy
     of the blacksmith bridge, deblacksmith-ified).
   - `.claude/commands/add-feature.md` (the slash command).
   - `.claude/agents/` placeholders for the standard roles (mayor,
     courtroom, polecat-default). User can edit/extend.
   - `CLAUDE.md` block appended (or created) pointing at dark-factory's docs.
4. Register the rig with gastown (`gt rig add` or equivalent), unless
   `--no-gastown`.
5. `df doctor` on the freshly initialised repo.

### 5.3 `/add-feature` (the primary UX)

What the user types in Claude Code:

```
/add-feature build a webhook ingest endpoint with idempotent dedupe
```

What happens (this is the load-bearing flow):

1. The slash command body resolves to `df run add-feature --prompt "..."`.
2. `df` walks the `pipeline:` from `dark-factory.yaml`.
3. Each step:
   - Logs to `.dark-factory/runs/<utc-ts>/step-<id>.log`.
   - On `runner: claude`, shells out to `claude --model X "<command>"`.
   - On `runner: gt`/`bash`, shells out and streams output.
   - On `runner: human`, prompts on stdout and reads stdin (or auto-skips
     when `--no-checkpoint`).
4. After `specify`, `df` reads `.specify/feature.json` to pin
   `feature_dir` for subsequent steps (mirrors flight.sh — solves the
   "stale feature.json" bug it already fixed).
5. Failure of any step halts the pipeline with a numbered error and the
   exact resume command (`df run add-feature --resume-from plan`).

Variants exposed as slash commands or flags:

| Flag | Behaviour |
|---|---|
| `--no-clarify` | Skip the clarify step |
| `--no-checkpoint` | Skip the human plan review |
| `--no-courtroom` | Skip cross-model deliberation |
| `--dry-run` | Run through tasks; do not create beads or start mayor |
| `--phase <label>` | Apply a phase label to every bead |
| `--resume-from <step>` | Restart from a given pipeline step |

### 5.4 `df doctor`

Inspects the local install + the current repo and prints a coloured
checklist:

- Binaries: `claude`, `codex`, `gt`, `bd`, `git` (versions + paths).
- Claude Code plugin presence and version.
- Repo: `dark-factory.yaml` valid, `.specify/memory/constitution.md`
  exists and non-empty, gastown rig registered, dolt server reachable if
  gastown enabled.
- Exit code is the count of failing checks (handy for CI).

---

## 6. The Claude Code plugin

Shipped alongside the binary, installed by the install script into
`~/.claude/plugins/dark-factory/dark-factory/1.0.0/`.

Contents:

```
.claude-plugin/plugin.json
commands/
  add-feature.md            # the primary UX
  df-doctor.md              # wraps `df doctor`
  df-resume.md              # wraps `df run --resume-from`
skills/
  add-feature/SKILL.md      # full phase-by-phase instructions
README.md
```

`commands/add-feature.md` is intentionally thin: it parses arguments,
verifies dark-factory is installed, and then defers to the skill for the
actual workflow narrative — same pattern courtroom uses today.

---

## 7. Boundaries with the underlying tools

We do **not** fork speckit, courtroom, or gastown. Dark Factory's contract:

- **speckit**: invoked via `claude /speckit.*`. Dark Factory assumes
  speckit writes `.specify/feature.json` and a per-feature directory;
  that's the same assumption flight.sh already makes.
- **courtroom**: invoked via `claude /courtroom`. Dark Factory passes
  the plan.md path through `--files` and reads the verdict file from
  `docs/courtroom/`.
- **gastown**: invoked via `gt` and `bd` CLIs only. No DB poking, no
  direct dolt access. Dark Factory will *surface* `gt dolt status`
  warnings but never run `gt dolt stop`.

If any of those tools change their CLI shape, dark-factory updates its
adapter in `internal/tools/<tool>.go`. That's the only blast radius.

---

## 8. State and idempotency

Per-repo dark-factory state lives in `.dark-factory/`:

```
.dark-factory/
  state.json            # last run id, last feature_dir, last convoy
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

`.dark-factory/runs/` is gitignored by default (init writes a
`.gitignore` entry). `state.json` is gitignored too. The yaml is committed.

---

## 9. What v1 ships vs. later

**v1 (this design):**

- `df init`, `df run add-feature`, `df doctor`, `df config`.
- `/add-feature`, `/df-doctor`, `/df-resume` slash commands.
- Templates for `.specify/`, `.claude/`, `dark-factory.yaml`,
  constitution stub, tasks-to-beads bridge.
- macOS + Linux install script. Binary published as GitHub release.

**Later:**

- Homebrew formula and a Windows build.
- `df run <custom-pipeline>` for repos that want flows beyond
  add-feature (e.g. "hotfix", "spike", "refactor").
- A `df observe` TUI for watching a running mayor session live
  (currently you `gt status --rig <name>` yourself).
- Optional Slack feedback hook so polecat completions ping a channel.

---

## 10. Open questions to resolve before coding

1. **Plugin distribution.** Claude Code plugins today come from a
   marketplace JSON. Do we publish to the same marketplace courtroom
   uses, or stand up our own? Leaning: stand up our own
   (`dark-factory.dev/marketplace.json`) so install is a single curl.

2. **Speckit init under the hood.** `specify init --here` writes a lot
   of opinionated content. Dark Factory can either (a) shell out and
   patch the result, or (b) ship its own minimal `.specify/` skeleton.
   (a) is less code; (b) is more robust to speckit changes. Leaning:
   (a) with a pinned speckit version range in `tools.speckit.version`.

3. **What does "mayor" mean for a non-blacksmith repo?** Blacksmith has a
   five-agent topology (mayor / polecats / refinery / witness / deacon).
   A small repo may not want polecats at all. The pipeline should allow
   ending at `tasks` (no mayor, no convoy) for simple single-author
   repos. Need a `mode: solo | crew` switch — default solo, opt into
   crew when gastown is wired up.

4. **Binary name collision.** `df(1)` is a standard Unix utility for
   disk-free. Shipping `df` on PATH will shadow it for anyone who
   sources `~/.local/bin` ahead of `/usr/bin`. Options:
   - Ship `dark-factory` as the canonical binary and a `df` symlink
     that the install script declines to create if `which df` already
     resolves to a non-empty path.
   - Ship only `dark-factory` and let users alias.
   Leaning: long name canonical, short symlink optional and
   collision-aware.

5. **Constitution defaults.** What ships in the constitution stub? A
   blank file is unhelpful; blacksmith's is too prescriptive. Probably
   a short template with placeholders and pointers to the speckit docs.
