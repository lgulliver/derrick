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

- **One-line install**: `curl -fsSL <url> | bash` puts the `derrick` binary
  and Claude Code plugin on the user's machine and verifies the deps.
- **One-line init**: `derrick init` in a repo writes the config, the
  templates, the hooks, the constitution skeleton, and registers the rig
  with gastown.
- **One primary command**: `/add-feature <prompt>` runs the full dark
  factory pipeline — spec → courtroom → plan → tasks → convoy → mayor.
- **Reusable**: nothing in derrick assumes blacksmith. Project-specific
  rules live in the repo's constitution + `derrick.yaml`, not in derrick.
- **Transparent**: every underlying tool call is logged and exit codes
  propagate. Nothing is magic. Power users can still call `gt`, `bd`,
  `claude /speckit.specify` directly.

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
| Tool adapters | Go | `internal/tools` | Thin wrappers around `claude`, `codex`, `gt`, `bd`, `specify`, `rtk` |
| Assay | Go | `internal/assay` | Derrick-native adversarial plan review (Codex-driven, courtroom-pattern) |
| Memory | Go | `internal/memory` | Seeds Claude memory files on init + per-step context budgets |
| Token policy | Go | `internal/token` | RTK wrapping, caveman summaries, output digesting between steps |
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

# Identity for gastown
rig:
  name: my-project
  prefix: mp           # bead prefix (mp-1, mp-2 …)
  role: mayor          # default role when `gt prime` is called

# Underlying tool versions / opt-outs
tools:
  speckit:   { enabled: true,  version: ">=0.4.0" }
  assay:     { enabled: true,  reviewer: codex, model: "gpt-5", rounds: 1, strict: false }
  gastown:   { enabled: true,  mode: crew }   # solo | crew  (see §8)
  rtk:       { enabled: true,  required: false }   # wrap subprocesses if present

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
    runner: bash
    command: "${DERRICK_HOME}/scripts/tasks-to-beads.sh {{tasks_md}} --convoy={{convoy}}"
  - id: mayor
    runner: gt
    command: "prime --rig {{rig.name}} --role mayor"

# Project-specific guardrails surfaced into prompts and checkpoints
guardrails:
  constitution_path: .specify/memory/constitution.md
  forbid_paths: []         # paths that may not be touched by a feature
  required_labels: []      # labels every bead must carry

# Where derrick writes its own state inside the repo
state:
  dir: .derrick
  log_runs: true
```

Resolution rules:

- Repo `derrick.yaml` wins.
- Falls back to `~/.derrick/config.yaml` for user defaults
  (preferred model, courtroom rounds, etc).
- Falls back to a baked-in default shipped with the binary.

Templates use Go `text/template` with a small context (`prompt`, `rig`,
`feature_dir`, `tasks_md`, `convoy`, env). No general expression language.

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
   - `.specify/extensions/derrick/scripts/tasks-to-beads.sh` (copy of the
     blacksmith bridge, deblacksmith-ified).
   - `.claude/commands/add-feature.md` (the slash command).
   - `.claude/agents/` placeholders for the standard roles (mayor,
     assay-reviewer, polecat-default). User can edit/extend.
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

- Binaries: `claude`, `codex`, `gt`, `bd`, `git` (versions + paths).
- Claude Code plugin presence and version.
- Repo: `derrick.yaml` valid, `.specify/memory/constitution.md` exists
  and non-empty, gastown rig registered, dolt server reachable if
  gastown enabled.
- Exit code is the count of failing checks (handy for CI).

---

## 6. The Claude Code plugin

Shipped alongside the binary, installed by the install script into
`~/.claude/plugins/derrick/derrick/1.0.0/`.

Contents:

```
.claude-plugin/plugin.json
commands/
  add-feature.md            # the primary UX
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

We do **not** fork speckit or gastown. Derrick's contract:

- **speckit**: invoked via `claude /speckit.*`. Derrick assumes speckit
  writes `.specify/feature.json` and a per-feature directory; that's the
  same assumption flight.sh already makes.
- **gastown**: invoked via `gt` and `bd` CLIs only. No DB poking, no
  direct dolt access. Derrick will *surface* `gt dolt status` warnings
  but never run `gt dolt stop`. See §8 for the lifecycle.

If any underlying tool changes its CLI shape, derrick updates its
adapter in `internal/tools/<tool>.go`. That's the only blast radius.

---

## 8. Gastown — rigs, beads, convoys, mayor

Gastown is not a terminal step; it's the **execution substrate** for
everything after `tasks`. Derrick is the front door; gastown is the
factory floor. This section is the contract.

### 8.1 The model (short version, derrick's view)

- **Rig** — a workspace registered with gastown. One per repo. Has a
  name and a bead prefix (`mp-123`). Created during `derrick init`.
- **Bead** — a single unit of work (an issue). The `bridge` step
  converts each `tasks.md` row into one bead via `bd create`, applying
  routing labels (`runtime/copilot`, `phase-N`, etc).
- **Convoy** — an ordered group of beads representing one feature.
  Named after the speckit feature slug. Bead ordering is preserved via
  `bd link --type=blocks` so polecats can't pull work out of sequence.
- **Polecat** — a Copilot agent with a persistent identity but
  ephemeral sessions. The Mayor `sling`s a bead to a polecat; the
  polecat works it; the Refinery merges it.
- **Mayor** — the Chief of Staff. Drives `bd ready`, runs assay or
  re-assay per bead if configured, dispatches to polecats, escalates
  to the human via `gt mail --human`.
- **Refinery** — merge-queue processor. Out of derrick's scope but
  derrick prints the right `gt status --rig <name>` command so the
  user can watch it.
- **Witness / Deacon** — observers and watchdogs. Same: out of scope,
  but `derrick doctor` checks they exist when `mode: crew`.

### 8.2 Modes

`tools.gastown.mode` is the load-bearing knob:

- **`solo`** (default for new repos) — pipeline ends at `tasks`. No
  beads, no convoy, no mayor. The user opens `tasks.md` and works it
  themselves. Derrick is still useful — speckit + assay are doing
  real work.
- **`crew`** — full flow. `bridge` runs, the convoy is created, mayor
  is started. Requires `gt` and `bd` on PATH and the rig to be
  registered.

`derrick init --mode crew` does the gastown registration. `derrick
init --mode solo` skips it cleanly. Switching modes is a one-line
edit to `derrick.yaml` plus a `derrick init --migrate`.

### 8.3 What derrick adds to the gastown story

- **Convoy naming** — derived from the speckit feature slug, so the
  convoy and the spec dir share an obvious identity.
- **Phase labelling** — `--phase <label>` is passed through to the
  bridge so every bead carries it. Useful for "phase-7" rollups.
- **Mayor handoff** — derrick stops at "Mayor session running. Watch
  with `gt status --rig <name>`." It does **not** stay attached. The
  pipeline run is done.
- **Resume** — `derrick run add-feature --resume-from bridge` is the
  canonical recovery path when speckit succeeded but gastown wobbled.

### 8.4 Dolt awareness

Gastown's data plane is Dolt. Derrick:

- Surfaces `gt dolt status` in `derrick doctor` (warning if degraded).
- **Never** runs `gt dolt stop`, never `rm -rf ~/.dolt-data`.
- On a step that fails with the classic Dolt symptoms (timeouts,
  connection refused), prints the exact diagnostic recipe from
  `gt`'s own runbook before exiting.

---

## 9. Token efficiency by design

Derrick will be used many times per repo per week. Per-run token cost
matters. Four levers, all on by default:

### 9.1 Model tiering

The pipeline assigns models per step (already shown in §4):

| Step | Model | Why |
|---|---|---|
| `specify`, `clarify`, `tasks` | sonnet | Mechanical, structured |
| `plan`, `analyze` | opus | Genuinely hard reasoning |
| `assay` | codex (gpt-5) | Adversarial, different family |
| `bridge`, `mayor` | n/a | Subprocess |

Anyone can override in `derrick.yaml`. Default biases toward the
cheapest model that still does the job.

### 9.2 RTK wrapping (subprocess token saver)

If `rtk` is on PATH, derrick prefixes every CLI subprocess
(`gt`, `bd`, `git`, `claude`, `codex`) with `rtk`. RTK is a transparent
proxy that filters noisy CLI output before it ever reaches Claude's
context. Documented in the user's `~/.claude/RTK.md` — derrick just
opts in.

- `tools.rtk.enabled: true` and `required: false` (default) — best
  effort. If `which rtk` is empty, fall back to direct invocation.
- `required: true` — `derrick doctor` fails if rtk is missing.

Average savings (per RTK's published numbers): 60-90% on dev
operations. We get this free.

### 9.3 Caveman summaries between steps

Derrick captures full subprocess output to disk (`.derrick/runs/...`)
but only surfaces a compressed summary to the next step. The
summariser uses the `caveman` skill pattern (ultra-compressed,
~75% token reduction) for inter-step handoff text:

- Step N writes its log to `.derrick/runs/<ts>/step-N.log` (full).
- Derrick reads the last K lines, calls the caveman skill (or applies
  the same shaping rules in-process), and writes
  `.derrick/runs/<ts>/step-N.summary.md` (tight).
- Step N+1's prompt context references the summary, not the log.

When something fails the user sees the full log. When everything is
fine, nobody reads it. Caveman intensity defaults to `lite` so
verdict files remain human-grep-able.

### 9.4 Agent memory seeding

Claude's auto-memory system (`~/.claude/projects/.../memory/`) is the
right place for facts that would otherwise eat context every turn.
`derrick init` seeds:

- **project memory**: rig name, bead prefix, constitution path,
  gastown mode, primary language(s). One file each, ≤150 chars in
  `MEMORY.md` pointing at them.
- **reference memory**: where `tasks.md` lives, where assay verdicts
  land, where `gt status` is run.
- **feedback memory**: derrick's own guardrails ("don't run `gt dolt
  stop`", "use `bd q` not `bd create` when prefix is unknown").

These are written to the *user's* memory directory, namespaced so
they can be cleanly removed via `derrick init --unmemoize`. They
shave the per-turn prompt size for the whole `/add-feature` flow.

### 9.5 Context discipline (assay specifically)

Assay's brief is *deliberately* small: artifact files, not chat
transcripts. The codex prompt template lives in
`internal/assay/prompts/cross_examine.tmpl` and is budget-capped at
2k tokens of context excluding the artifact bodies. If artifacts
exceed budget, derrick chunk-summarises them with caveman first and
notes that in the verdict.

### 9.6 Observability

`derrick run add-feature --tokens` prints a per-step token estimate
after the run (input + output, by model). `derrick gain` (clone of
RTK's analytics command shape) shows aggregate savings over time.
This is the feedback loop that keeps the four levers honest.

---

## 10. State and idempotency

Per-repo derrick state lives in `.derrick/`:

```
.derrick/
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

`.derrick/runs/` is gitignored by default (init writes a `.gitignore`
entry). `state.json` is gitignored too. The yaml is committed.

---

## 11. What v1 ships vs. later

**v1 (this design):**

- `derrick init`, `derrick run add-feature`, `derrick doctor`,
  `derrick config`.
- `/add-feature`, `/derrick-doctor`, `/derrick-resume` slash commands.
- Templates for `.specify/`, `.claude/`, `derrick.yaml`, constitution
  stub, tasks-to-beads bridge.
- macOS + Linux install script. Binary published as GitHub release.

**Later:**

- Homebrew formula and a Windows build.
- `derrick run <custom-pipeline>` for repos that want flows beyond
  add-feature (e.g. "hotfix", "spike", "refactor").
- A `derrick observe` TUI for watching a running mayor session live
  (currently you `gt status --rig <name>` yourself).
- Optional Slack feedback hook so polecat completions ping a channel.

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
