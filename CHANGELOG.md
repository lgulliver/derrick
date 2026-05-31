# Changelog

All notable changes to derrick are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
aims to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0-alpha.3] — 2026-05-31

### Added
- **`derrick survey setup`** — standalone MCP wiring for any git repo. Creates
  `.derrick/` + `.gitignore` and merges the `derrick-survey` server into
  `.mcp.json` without requiring `derrick init`, a `derrick.yaml`, or a
  substrate database. Useful for Cursor, Windsurf, or any other MCP-capable
  host that does not need the full derrick pipeline. `docs/survey.md` updated
  with a two-path setup guide.

### Fixed
- **`derrick init` wizard missing `opencode` and `aider`** — both hosts are
  first-class crew executors (D66) but were absent from
  `available_model_choices()`, making them unselectable at every role-binding
  prompt in the wizard.

## [0.1.0-alpha.2] — 2026-05-31

### Fixed
- **Splash logo alignment** — all 12 box lines are now exactly 65 display
  columns wide; ASCII-art rows were 1–2 chars short and the subtitle was 1 char
  too wide, causing a ragged right border on narrow terminals.
- **Adoption plan `writes` formatting** — file paths now print one per line with
  13-char indent continuation instead of a single comma-joined line that wrapped
  mid-terminal.
- **`derrick add` false "No constitution found" warning** — `ensure_constitution`
  was checking the per-run git worktree path (created from HEAD) rather than the
  main working tree; files not yet committed appeared missing.

### Added
- **Codex PreToolUse / PostToolUse hooks (D69, resolves D34)** — `derrick init`
  now writes `.codex/settings.toml` with scrub (`derrick scrub --tool bash`) and
  caveman (`derrick caveman --intensity lite`) hooks on matchers
  `Bash|Read|Write|Edit|Glob|Grep`, matching the Claude Code D29 path.
  `CodexHost::run()` passes `--dangerously-bypass-hook-trust` so hooks fire
  automatically in non-interactive automation. The "Codex tool I/O not scrubbed"
  warning on `derrick init` is removed.

## [0.1.0-alpha.1] — 2026-05-30

First public pre-release. derrick is a unified layer over speckit, courtroom,
and gastown — one binary, one config, one command (`/add-feature`) that runs
the full spec → plan → tasks → build pipeline in any repo. This alpha is
feature-broad but rough at the edges (see **Known limitations**); the
architecture and 68 recorded decisions live in [DESIGN.md](./DESIGN.md).

### Pipeline & flow
- `derrick add "<prompt>"` (positional shorthand) and `derrick run add-feature`
  (scripting) drive the full pipeline: specify → clarify → plan → tasks →
  analyze → assay → bridge → foreman.
- Feature prompt accepted from an argument, `--prompt-file <path>`, or stdin
  (`-`) (D64).
- Multi-reviewer **assay** with `parallel_group` steps and true parallel
  fan-out; headless/CI-safe (only a `reject` verdict blocks the pipeline).
- `derrick ticket code-review` — adversarial pre-PR review with an
  auto-remediation loop.
- Run **resume**: `prompt_key`-based idempotent retry, `--force` for a fresh
  start, `resume_of` lineage recorded in run manifests.
- Live run progress via a UI-free `ProgressReporter` with per-step spinners,
  elapsed time, token/cost deltas, and line-by-line agent output streaming
  (D60, D61).

### Substrate, foreman & crew
- SQLite-backed substrate (`rusqlite`, bundled) with a ticket state machine
  (ready → in-flight → in-review → done / blocked / rejected) and schema
  migrations (v1 → v4).
- Foreman dispatch loop (attached and detached daemon) with batch/step
  parallelism budgets, stale-hand requeue, dependency unblocking, and a
  merge-observing verifier.
- Per-ticket isolated git worktrees for parallel safety, with TTL cleanup.
- `derrick switch` — solo → crew (or copilot) upgrade command.

### Models & hosts (no BYOK)
- **All model inference routes through one of five host CLIs** — `claude`,
  `codex`, `copilot`, `opencode`, `aider`. derrick holds no API keys; each host
  manages its own auth (D65; supersedes the earlier BYOM/API-key design).
- Curated, current per-host model catalogue with per-host `--model`
  normalisation; `derrick models check` validates bindings (host installed →
  fail; unknown model id → warn-and-pass-through) and surfaces soft warnings at
  `init`/`run` (D65, completes D15).
- `opencode`/`codex`/`aider` are first-class crew executor hands, executed
  through the host adapters in per-ticket worktrees (D66).
- **Foreman-driven adaptive model selection** (D67): you pick the *tool* (host)
  per role; the foreman picks the *model per ticket* by estimated complexity
  within that host's light/standard/heavy tiers. Ticket complexity is produced
  upstream by the `tasks` step (an HTML-comment marker) and stored on the
  ticket (migration 0004). An `auto` / `auto:light` / `auto:heavy` sentinel in
  `derrick.yaml` opts into selection; an explicit model pin always wins.
- Per-ticket hand-worktree lifecycle: tracked worktree rows with prune-on-
  terminal/release and a TTL backstop (D68).

### Tokens
- `derrick scrub` — strips CLI noise (progress bars, spinners, ANSI) before
  tool output reaches a model; 80%+ reduction on git/cargo output, with
  `bytes_raw`/`bytes_saved` recorded per step.
- `derrick-roughneck` — LLM output compression via prompt injection
  (lite/full/ultra).
- `derrick caveman` — inter-step prose compression (60%+ at full intensity).
- `derrick gain --run <id>` — per-step token + cost breakdown from run
  manifests.

### Memory & survey
- Tiered memory with a tag index and lesson retrieval, seeded by `derrick init`.
- `derrick survey` — native code-graph index (SQLite + FTS5) over Rust, TS/JS,
  Python, Go, C#, Java, Kotlin; an MCP server (`survey serve --mcp`) lets agents
  query symbols/callers/impact instead of fanning out across file reads, with a
  debounced watcher to keep it fresh (D54–D59).

### CLI & UX
- `derrick init` — brownfield-safe setup wizard (arrow-key prompts via
  `inquire`), constitution seeding, VS Code + JetBrains opt-in, Claude Code
  hooks + survey MCP wiring, `.codex/instructions.md`, and an initial commit
  when the repo has no HEAD.
- `derrick observe` — live ratatui dashboard.
- `derrick status` — single front door for run/ticket state.
- `derrick doctor` — environment + host/model checks, including a live
  squash-merge policy check via the GitHub API.
- `derrick upgrade` — binary self-update from GitHub releases (`--check`,
  `--force`; atomic replacement preserving permissions).
- PR stacking: `stack show / restack / submit` (native / Graphite / git-spice).
- Shell completions (bash / zsh / fish / elvish / powershell).
- A shared CLI theme module for consistent styled output (D63).

### Release & install
- `scripts/install.sh` — curl-able, platform-detecting (linux-x86_64,
  macos-arm64, macos-x86_64).
- GitHub release workflow builds binaries + checksums on a `v*` tag push and
  stamps the workspace version from the tag so the binary's reported version
  matches the release.
- `marketplace.json` for Claude Code plugin discovery.

### Known limitations
- **Platforms:** Linux and macOS only. Windows is not supported yet (process
  probing, file-locking, and shell semantics are stubbed).
- **aider** is supported but **experimental** — its CLI flags were implemented
  from documentation and have not been validated against an installed binary.
- The model catalogue is **pinned to May 2026 ids**; newer models pass through
  with a warning but the curated defaults/tiers will drift until updated.
- The GitHub-cloud Copilot dispatcher is a stub; the crew uses the **local**
  `copilot` CLI.
- Homebrew tap is planned for a later release.

[Unreleased]: https://github.com/lgulliver/derrick/compare/v0.1.0-alpha.3...HEAD
[0.1.0-alpha.3]: https://github.com/lgulliver/derrick/compare/v0.1.0-alpha.2...v0.1.0-alpha.3
[0.1.0-alpha.2]: https://github.com/lgulliver/derrick/compare/v0.1.0-alpha.1...v0.1.0-alpha.2
[0.1.0-alpha.1]: https://github.com/lgulliver/derrick/releases/tag/v0.1.0-alpha.1
