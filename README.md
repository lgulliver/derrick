<div align="center">
  <img src="assets/logo.png" alt="Derrick" width="800" />
</div>

---

> *The load-bearing tower over an oil well — the structure that lifts every length of pipe in and out of the hole.*

[![CI](https://github.com/lgulliver/derrick/actions/workflows/ci.yml/badge.svg)](https://github.com/lgulliver/derrick/actions/workflows/ci.yml)
[![Rust 1.85+](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](./LICENSE)

**derrick** is a Rust CLI that turns a single command into a full dark-factory feature pipeline — spec, adversarial review, tickets, dispatch, PR stacking — without asking you to wire each underlying tool by hand.

```bash
derrick add "build a webhook ingest endpoint with idempotent dedupe"
```

That one line walks the entire pipeline, remembers what it learns about your codebase, compresses everything that crosses a model boundary, and runs independent work in parallel. One binary, SQLite, no daemon required.

---

## How it works

```
derrick add "description"
      │
      ▼
  clarify ──► plan ──► checkpoint ──► assay (adversarial review)
                                          │
                                          ▼
                               analyze ──► tasks ──► bridge
                                                         │
                                                         ▼
                                                  foreman (dispatch)
                                                         │
                                           ┌─────────────┴─────────────┐
                                           ▼                           ▼
                                       hand A                      hand B
                                    (git worktree)              (git worktree)
```

Every step is configured in your repo's `derrick.yaml`. Skip any step at invocation time (`--no-clarify`, `--no-assay`, `--dry-run`) or remove it from the pipeline entirely.

---

## The three pillars

### 🧠 Memory
derrick seeds and curates persistent agent memory so the assistant builds on what it already knows about your codebase. Tiered retrieval surfaces the right context at the right time — no relearning the rig on every turn.

### ⚡ Tokens
Every byte across a model boundary earns its place.

| What | How | Typical saving |
|---|---|---|
| **Survey** (`derrick-survey`) | Pre-indexes symbols + call-graph (SQLite + FTS5); agents query the graph instead of fanning out across Read/grep calls. Savings tracked in `derrick gain`. | **~300 input tokens** per avoided Read fan-out |
| **Scrub** (`derrick-scrub`) | Strips CLI noise (progress bars, spinners, ANSI codes) before tool output reaches the model. Records `bytes_raw`/`bytes_saved` per step. | **88% on `git fetch`**, 94% on `cargo build` |
| **Roughneck** (`derrick-roughneck`) | LLM output compression via prompt injection — model emits a compressed form of its own output before handoff. Three levels: lite (~30%), full (~65%, default), ultra (~75%). | **~65% at Full** on typical model output |
| **Caveman** | Compresses verbose prose in inter-step handoffs (lite / full / ultra) | **62% at Full** on typical AI-generated text |
| **Model tiering** | The foreman picks the model per ticket (light/standard/heavy) by estimated complexity, within the host you chose; pin a model to opt out | Per-ticket adaptive selection (`auto`) |
| **Prompt caching** | Anthropic cache headers on repeated context | Up to 90% on repeated prefixes |

Survey is wired automatically via MCP by `derrick init` — agents query it instead of fanning out across reads without any extra setup. Scrub and caveman fire automatically at every model boundary via Claude Code / Codex hooks written by `derrick init`. Roughneck fires at every model step via prompt injection; configure via `tools.roughneck` in `derrick.yaml`.

### 🔀 Parallelism
Independent work runs concurrently. Each `/add-feature` run gets an isolated git worktree. The foreman dispatches multiple hands (agents) in parallel via `join_all`. Multi-reviewer assay fans reviewers out concurrently, bounded by `parallelism.assay_max` (§9.C.5).

---

## Installation

### Nightly (recommended until v1.0)

Built weekly (Sunday 06:00 UTC) from `main`. This is the working install path today.

```bash
curl -fsSL https://raw.githubusercontent.com/lgulliver/derrick/main/scripts/install.sh | bash -s -- --nightly
```

### Stable (not yet available)

Stable `v*` releases will be cut starting at v1.0. The default (no flag) install will work then. Until then, use `--nightly` or install from source below.

```bash
curl -fsSL https://raw.githubusercontent.com/lgulliver/derrick/main/scripts/install.sh | bash
```

### Pin a specific version

```bash
DERRICK_VERSION=v0.1.0 curl -fsSL https://raw.githubusercontent.com/lgulliver/derrick/main/scripts/install.sh | bash
```

### Custom install directory

Defaults to `/usr/local/bin`. Override with `DERRICK_INSTALL_DIR`:

```bash
DERRICK_INSTALL_DIR=~/.local/bin curl -fsSL https://raw.githubusercontent.com/lgulliver/derrick/main/scripts/install.sh | bash
```

### From source (Rust toolchain required)

```bash
cargo install --git https://github.com/lgulliver/derrick derrick-cli
```

### Platform support

| Platform | Architecture | Status |
|---|---|---|
| Linux | x86_64 | Supported |
| macOS | Apple Silicon (arm64) | Supported |
| macOS | Intel (x86_64) | Supported |
| Windows | — | Planned for v1.1 |

Homebrew tap also planned for v1.1.

---

## Getting started

Then adopt a repo:

```bash
cd ~/repos/my-project
derrick init                     # brownfield-safe: won't clobber your AGENTS.md
derrick survey build             # index symbols + call-graph for agent queries
derrick doctor                   # checks toolchain, hooks, squash-merge policy
derrick foreman start --attached # start the dispatch loop
```

Or launch the guided setup:

```bash
derrick init --wizard
```

The wizard uses **Project name** language in prompts (internally this still maps to `site.name` in `derrick.yaml`) and lets you choose:
- init type (existing repo vs fresh project)
- operating mode (`solo`, `copilot`, `crew`)
- AI tool bindings (recommended defaults, one tool for all stages, or per-stage)
- optional editor integrations
- final preview + confirmation before writes

Non-interactive usage is unchanged (`--yes`, `--dry-run`, non-TTY, and scripted flags skip prompts).

Start a feature:

```bash
derrick add "build a webhook ingest endpoint with idempotent dedupe"

# Skip steps you don't need right now
derrick add "fix the auth token refresh race" --no-clarify --no-assay

# Dry run to see the plan without executing
derrick add "refactor the rate limiter" --dry-run
```

Or trigger from inside Claude Code with `/add-feature` (maps to the same pipeline).

---

## CLI reference

```
derrick <COMMAND>

PIPELINE
  add          Run the full pipeline — prompt is a positional argument
  init         Adopt a repo (brownfield-safe, VS Code / JetBrains opt-in)
  switch       Upgrade a solo-mode repo to crew (or copilot) mode
  upgrade      Binary self-update from the latest GitHub release (--check, --force)
  run          add-feature / resume — canonical forms for scripts / CI
  foreman      start / stop / tick the dispatch loop

VISIBILITY
  status       Current batch, in-flight tickets, foreman state
  observe      Live ratatui dashboard (6 tabs: overview, tickets, stack,
               activity, tokens, memory)
  doctor       Toolchain and config health check

TICKET MANAGEMENT
  ticket       done / review / code-review / list / show / reject / reopen / block

STACKING
  stack        show / restack / submit — PR stack management

SURVEY (CODE-GRAPH INDEX)
  survey       build / search / context / impact / status / serve
               build    — (re)index the repo
               search   — FTS symbol search
               context  — entry points + related symbols + snippets
               impact   — callers / callees / impact radius
               status   — freshness report
               serve    — run MCP server (--mcp for stdio transport)

TOKEN TOOLS
  scrub        Filter CLI noise from stdin (git, cargo, claude, gh, ...)
  caveman      Compress verbose prose from stdin (lite / full / ultra)
  gain         Show scrub, caveman, and survey token savings

SHELL
  completions  Generate shell completions (bash / zsh / fish / elvish / powershell)
  uninstall    Remove derrick from a repo
```

### Token tools in action

```bash
# Build (or rebuild) the code-graph index
derrick survey build

# Query the index — agents do this via MCP; you can also use the CLI
derrick survey search "parse_session"
derrick survey impact "TokenUsage"
derrick survey context "foreman loop"

# Strip git fetch noise before feeding output to a model
git fetch 2>&1 | derrick scrub --tool git

# Compress an inter-step summary
echo "I would like to let you know that in order to..." | derrick caveman --intensity full

# Show what's active — scrub, caveman, and survey savings
derrick gain
```

---

## Architecture

19 crates, one binary:

| Crate | Role |
|---|---|
| `derrick-cli` | Binary, all subcommands |
| `derrick-survey` | Native code-graph index: SQLite + FTS5 symbol/reference/call-graph at `.derrick/index.db`, queried by agents over MCP; CLI `survey build/search/context/impact/status` |
| `derrick-flow` | Pipeline executor, state machine |
| `derrick-assay` | Multi-reviewer adversarial assay + shared pipeline types (RunError, StepExecution, io helpers) |
| `derrick-config` | Typed schema, layered loader, 14 validation rules |
| `derrick-scrub` | CLI noise filter — rules for git, gh, claude, codex, copilot, cargo; records bytes_raw/bytes_saved per step |
| `derrick-roughneck` | LLM output compressor via prompt injection — lite / full / ultra; records roughneck_tokens_saved per step |
| `derrick-caveman` | Prose compressor — lite / full / ultra intensities |
| `derrick-memory` | Tiered retrieval, tag index, lesson curation |
| `derrick-tui` | ratatui dashboard (6 tabs) |
| `derrick-observe` | TUI wiring, stack refresh, event loop |
| `derrick-stack` | PR stacking (native / Graphite / git-spice) |
| `derrick-models` | Model trait + host-delegated providers (one per host CLI) + shell escape hatch |
| `derrick-adopt` | Brownfield adoption — detects AGENTS.md, writes hooks + survey MCP wiring |
| `derrick-substrate` | Substrate trait + ticket/batch/hand state types |
| `derrick-substrate-native` | SQLite-backed substrate + foreman loop |
| `derrick-claude` | Claude substrate |
| `derrick-copilot` | Copilot substrate |
| `derrick-tools` | Host CLI adapters (claude, codex, copilot, opencode, aider) + model catalogue |

---

## Hosts and models

Derrick routes **all** model inference through one of five host CLIs. It holds no API keys of its own — each host manages its own auth (no BYOK):

| Host | CLI | Models |
|---|---|---|
| `claude` | Claude Code (`claude`) | Anthropic (`claude-opus-4-8`, `claude-sonnet-4-6`, `claude-haiku-4-5`) |
| `codex` | Codex (`codex`) | OpenAI (`gpt-5.5`, `gpt-5.4`, …) |
| `copilot` | GitHub Copilot (`copilot`) | multi-model |
| `opencode` | OpenCode (`opencode`) | multi-provider (`provider/model` ids) |
| `aider` | Aider (`aider`) | multi-provider (`provider/model` ids) |

A `shell` escape hatch remains for an arbitrary subprocess model. Model ids are validated against a curated, current catalogue by **`derrick models check`** — an unknown id warns but still passes through to the CLI; a missing host CLI fails.

You pick the **tool** (host) per role; the **foreman picks the model per ticket** by size/complexity within that host, unless you pin one:

```yaml
models:
  claude-opus:   { provider: claude,  model: "claude-opus-4-8" }
  claude-sonnet: { provider: claude,  model: "claude-sonnet-4-6" }
  claude-haiku:  { provider: claude,  model: "claude-haiku-4-5" }
  codex-gpt5:    { provider: codex,   model: "gpt-5.5" }
  copilot:       { provider: copilot, model: "auto" }   # foreman selects per ticket
```

`auto` (or `auto:light` / `auto:heavy`) lets the foreman pick the model by the ticket's estimated complexity within the host's tier list (light/standard/heavy); a concrete id always wins. Legacy configs naming the old `anthropic` / `openai-cli` / `copilot-cli` providers still load (aliased to their host).

### Crew mode role bindings

`crew` mode is role-aware and uses differentiated bindings by default:

```yaml
tools:
  substrate:
    mode: crew

roles:
  proposer: claude-opus
  drafter: claude-sonnet
  reviewer: codex-gpt5
  executor: copilot      # its model is `auto` — foreman picks per ticket
  summariser: claude-haiku
```

You can override any role binding in `roles`, and pin a role's model to a concrete id (instead of `auto`) to take selection out of the foreman's hands.

---

## Status

**Active development.** Architecture and 67 decisions in [DESIGN.md](./DESIGN.md).

What's landed and tested:

- ✅ `derrick add` — positional-prompt shorthand; `run add-feature` for scripts
- ✅ Full pipeline executor with multi-reviewer assay and `parallel_group` steps
- ✅ Foreman dispatch loop (attached and detached daemon)
- ✅ Ticket state machine (ready → in-flight → in-review → done / blocked / rejected)
- ✅ `derrick ticket code-review` — adversarial pre-PR code review with auto-remediation loop
- ✅ Per-run isolated git worktrees (`.derrick/worktrees/<run-id>/`) for parallel safety
- ✅ Token tracking per pipeline step + cost estimates — `derrick gain --run <id>` for per-step breakdown
- ✅ `derrick scrub` with 80%+ reduction on git and cargo output; bytes_raw/bytes_saved in run manifests
- ✅ `derrick-roughneck` — LLM output compression via prompt injection (lite/full/ultra); roughneck_tokens_saved in manifests
- ✅ `derrick caveman` with 60%+ reduction at Full intensity on verbose prose
- ✅ Run resume — `prompt_key`-based idempotent retry; `--force` for fresh start; `resume_of` lineage in manifests
- ✅ Bridge auto-remediation — terminal ticket delete+recreate, active ticket skip
- ✅ Assay headless mode — CI-safe; only `reject` blocks the pipeline
- ✅ `derrick switch` — solo → crew upgrade command
- ✅ `derrick upgrade` — binary self-update from GitHub releases (`--check`, `--force`, atomic replacement with permission preservation)
- ✅ Constitution seeding in `derrick init` wizard
- ✅ `derrick init` initial commit fix — creates HEAD before first `derrick add`
- ✅ Pipeline step order fix — `tasks` before `analyze`
- ✅ `derrick observe` — live ratatui dashboard
- ✅ Tiered memory with tag index and lesson retrieval
- ✅ `derrick survey` — native code-graph index (SQLite + FTS5) over Rust/TS/JS/Python/Go/C#/Java/Kotlin; MCP server (`survey serve --mcp`) so agents query symbols/callers/impact instead of fanning out across reads; debounced watcher keeps it fresh
- ✅ `derrick init` — brownfield-safe, VS Code + JetBrains opt-in, Codex instructions
- ✅ `derrick doctor` — live squash-merge policy check via GitHub API
- ✅ PR stacking: `stack show / restack / submit`
- ✅ Shell completions (bash / zsh / fish / elvish / powershell)
- ✅ `scripts/install.sh` — curl-able, platform-detecting (linux-x86\_64, macos-arm64, macos-x86\_64)
- ✅ GitHub release workflow — builds on `v*` tag push, attaches binaries + checksums
- ✅ `marketplace.json` — Claude Code plugin discovery
- ✅ True parallel fan-out for multi-reviewer assay and `parallel_group` steps
- 🔜 Homebrew tap (v1.1)

650 tests passing across 19 crates.

## Coverage

CI enforces a workspace line-coverage floor of 80%.

Run locally:

```bash
cargo llvm-cov --workspace --all-features --fail-under-lines 80
```

---

## Read next

- [DESIGN.md](./DESIGN.md) — full architecture, pipeline schema, and all 67 decisions
- [AGENTS.md](./AGENTS.md) — operational contract for agents building derrick
- [CONTRIBUTING.md](./CONTRIBUTING.md) — engineering standards and PR workflow
- [docs/survey.md](./docs/survey.md) — derrick survey deep-dive: how it works, setup, CLI reference, MCP tools, token accounting

---

## License

MIT — see [LICENSE](./LICENSE).
