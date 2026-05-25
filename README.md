<div align="center">
  <img src="assets/logo.png" alt="Derrick" width="800" />
</div>

---

> *The load-bearing tower over an oil well — the structure that lifts every length of pipe in and out of the hole.*

[![CI](https://github.com/lgulliver/derrick/actions/workflows/ci.yml/badge.svg)](https://github.com/lgulliver/derrick/actions/workflows/ci.yml)
[![Rust 1.75+](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org)
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
| **Scrub** (`derrick-scrub`) | Strips CLI noise (progress bars, spinners, ANSI codes) before tool output reaches the model. Records `bytes_raw`/`bytes_saved` per step. | **88% on `git fetch`**, 94% on `cargo build` |
| **Roughneck** (`derrick-roughneck`) | LLM output compression via prompt injection — model emits a compressed form of its own output before handoff. Three levels: lite (~30%), full (~65%, default), ultra (~75%). | **~65% at Full** on typical model output |
| **Caveman** | Compresses verbose prose in inter-step handoffs (lite / full / ultra) | **62% at Full** on typical AI-generated text |
| **Model tiering** | Routes cheap steps to lighter models; expensive reasoning to frontier models | Configurable per pipeline step |
| **Prompt caching** | Anthropic cache headers on repeated context | Up to 90% on repeated prefixes |

Scrub and caveman fire automatically at every model boundary via Claude Code / Codex hooks written by `derrick init`. Roughneck fires at every model step via prompt injection; configure via `tools.roughneck` in `derrick.yaml`.

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
  upgrade      [reserved] Binary self-update (not yet implemented)
  run          add-feature — canonical form of `add` (scripts / CI)
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

TOKEN TOOLS
  scrub        Filter CLI noise from stdin (git, cargo, claude, gh, ...)
  caveman      Compress verbose prose from stdin (lite / full / ultra)
  gain         Show scrub and caveman status

SHELL
  completions  Generate shell completions (bash / zsh / fish / elvish / powershell)
  uninstall    Remove derrick from a repo
```

### Token tools in action

```bash
# Strip git fetch noise before feeding output to a model
git fetch 2>&1 | derrick scrub git

# Compress an inter-step summary
echo "I would like to let you know that in order to..." | derrick caveman --intensity full

# Show what's active
derrick gain
```

---

## Architecture

18 crates, one binary:

| Crate | Role |
|---|---|
| `derrick-cli` | Binary, all subcommands |
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
| `derrick-models` | Model trait + provider implementations (anthropic, openai-cli, opencode, shell) |
| `derrick-adopt` | Brownfield adoption — detects AGENTS.md, writes hooks |
| `derrick-substrate` | Substrate trait + ticket/batch/hand state types |
| `derrick-substrate-native` | SQLite-backed substrate + foreman loop |
| `derrick-claude` | Claude substrate |
| `derrick-copilot` | Copilot substrate |
| `derrick-tools` | Host CLI adapters (claude, codex, copilot, opencode) |

---

## Supported model providers

| Provider key | Backend | Auth |
|---|---|---|
| `anthropic` | Anthropic Messages API (streaming SSE) | `ANTHROPIC_API_KEY` env (or AuthStore override) |
| `openai-cli` | `codex exec` CLI (default) or OpenAI Chat API when `OPENAI_API_KEY` is set | CLI: host-delegated; API: `OPENAI_API_KEY` env (or `openai-cli` AuthStore override) |
| `opencode` | `opencode run` CLI | Host-delegated (opencode manages its own auth) |
| `shell` | Any shell command via `cli:` field in `derrick.yaml` | N/A (caller-managed) |

**Hosts** (for pipeline steps that invoke a CLI tool to run a slash command):
`claude` · `codex` · `copilot` · `opencode`

Configured per pipeline step in `derrick.yaml`. Bring your own model on any step.

```yaml
models:
  claude-opus:   { provider: anthropic,  model: "claude-opus-4-7" }
  codex-gpt5:    { provider: openai-cli, model: "gpt-5" }           # uses codex CLI by default
  opencode-sonnet: { provider: opencode, model: "claude-sonnet-4-5" }
  my-local:      { provider: shell,      cli: "my-model-wrapper --model foo", model: "foo" }
```

`openai-cli` falls back to the direct OpenAI API when `OPENAI_API_KEY` is present and no `cli:` override is set, so you get token-count telemetry without needing the codex binary installed.

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
  executor: copilot
  summariser: claude-sonnet
```

You can override any role binding in `roles`.

---

## Status

**Active development.** Architecture and 53 decisions in [DESIGN.md](./DESIGN.md).

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
- ✅ Constitution seeding in `derrick init` wizard
- ✅ `derrick init` initial commit fix — creates HEAD before first `derrick add`
- ✅ Pipeline step order fix — `tasks` before `analyze`
- ✅ `derrick observe` — live ratatui dashboard
- ✅ Tiered memory with tag index and lesson retrieval
- ✅ `derrick init` — brownfield-safe, VS Code + JetBrains opt-in, Codex instructions
- ✅ `derrick doctor` — live squash-merge policy check via GitHub API
- ✅ PR stacking: `stack show / restack / submit`
- ✅ Shell completions (bash / zsh / fish / elvish / powershell)
- ✅ `scripts/install.sh` — curl-able, platform-detecting (linux-x86\_64, macos-arm64, macos-x86\_64)
- ✅ GitHub release workflow — builds on `v*` tag push, attaches binaries + checksums
- ✅ `marketplace.json` — Claude Code plugin discovery
- 🔜 Homebrew tap (v1.1)
- ✅ Per-session token telemetry in `derrick gain` — `derrick gain --run <id>` for per-step breakdown
- ✅ True parallel fan-out for multi-reviewer assay and `parallel_group` steps

431 tests passing across 18 crates.

## Coverage

CI enforces a workspace line-coverage floor of 80%.

Run locally:

```bash
cargo llvm-cov --workspace --all-features --fail-under-lines 80
```

---

## Read next

- [DESIGN.md](./DESIGN.md) — full architecture, pipeline schema, and all 53 decisions
- [AGENTS.md](./AGENTS.md) — operational contract for agents building derrick
- [CONTRIBUTING.md](./CONTRIBUTING.md) — engineering standards and PR workflow

---

## License

MIT — see [LICENSE](./LICENSE).
