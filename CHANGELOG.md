# Changelog

All notable changes to derrick are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
aims to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0-alpha.5] — 2026-07-01

### Added
- **Native spec provider is now the default for new sites (D87).** Fresh
  `derrick init` configs write `tools.specify.provider: native` and bare
  `specify` / `plan` / `tasks` steps, so the standard `/drill` path no longer
  requires speckit. Speckit remains an explicit compatibility provider for
  host-delegated `/speckit.*` flows.
- **Role bindings now support repo-local agent files (D88).** `roles:` accepts
  both the existing short form (`reviewer: codex-gpt5`) and an expanded form
  with `model` plus `agent`, such as
  `reviewer: { model: codex-gpt5, agent: .codex/agents/integrations-engineer.md }`.
  Host-backed role steps prepend that repo-local instruction file as explicit
  role context.

### Fixed
- **Speckit compatibility init now pins existing `analyze` steps correctly.**
  When a repo already had an `analyze` step, the speckit provider init path now
  normalises that step into the pinned `/speckit.analyze` form instead of
  leaving it half-converted.
- **Survey hub integration tests no longer race on startup.** Hub-routing tests
  now serialise startup around the shared transport/resource path that was
  causing intermittent failures.
- **Role-agent review follow-ups.** Host-native role agent files are now kept
  off the non-host model-completion path, `roles.<role>.agent` is constrained to
  paths inside the working tree, and profile/stage overrides preserve existing
  agent bindings on synthesised assay reviewer roles.

## [0.1.0-alpha.4] — 2026-06-24

### Added
- **Centralised multi-repo survey hub — `derrick survey hub` (D80).** A new
  `derrick-survey-hub` crate wraps the repo-agnostic `derrick-survey` engine
  (tree-sitter extraction, SQLite+FTS5, search/context/impact/status) to serve
  **N** indexed repositories from one long-lived process over rmcp's streamable
  HTTP transport, configured by a `hub.yaml` workspace registry. The per-repo
  stdio server (D57) is untouched — the hub is purely additive. Each of the four
  survey tools takes a `workspace` argument selecting which repo's index to
  query.
- **Hub freshness — poll-on-query TTL + explicit refresh (D81).** Each workspace
  carries a `last_checked` timestamp; a query past the configured
  `freshness_ttl_secs` triggers a cheap staleness probe and, if files changed, an
  incremental rebuild before answering — a self-healing floor — with a
  single-flight guard against duplicate concurrent rebuilds. A workspace-scoped
  `derrick_survey_refresh` tool forces an immediate rebuild for CI/git-hook
  push-style proactivity.
- **Hub workspace sourcing — Local vs Pushed (D82).** A `WorkspaceSource`
  abstraction lets each workspace be either `Local { root }` (the hub holds a
  working tree and builds the index itself, D81 freshness applies) or
  `Pushed { db_path }` (an operator or CI places a prebuilt `.db` on disk; the
  hub opens, serves, and **atomically hot-swaps** it when the file changes).
  Modes may be mixed within one `hub.yaml`. Cross-version safety reuses the
  existing `PRAGMA user_version` / `SchemaTooNew` guard.
- **Hub authentication — scoped bearer tokens (D83).** An optional `auth` section
  in `hub.yaml` lists bearer tokens, each granting a set of workspace ids (or
  `*`) and capabilities (`read`, `refresh`; `upload` reserved). Clients present
  `Authorization: Bearer <token>`; the hub authenticates in constant time and
  authorizes per-workspace per-capability. A non-loopback bind is rejected unless
  `auth` is configured; TLS is terminated by a reverse proxy.
- **Hub routing — discovery tool + path-prefix mounts (D84).** A new
  `derrick_survey_list_workspaces` tool enumerates the workspace ids a caller's
  token may reach (auth-scoped). In addition to the default root endpoint (where
  every tool takes an explicit `workspace`), the hub serves each workspace at a
  pinned path `/w/<id>` where the `workspace` argument is optional — clean
  per-site URLs a reverse proxy can route without wildcard DNS. Workspace ids are
  validated as single URL-safe path segments.
- **Factory view — 8th tab in `derrick observe` (D78).** An animated ASCII
  factory floor of workers, driven by the new structured hand telemetry
  (D75/D76/D77). Each registered hand is a workstation with a unicode avatar
  per `HandKind` (🤖 claude / 🐙 copilot / 🧑‍💻 codex / 🦀 opencode / 🦜 aider /
  🧑 human). Running workers animate a braille spinner from `HandProgress`
  events; `HandExited` resolves them to ✓/✗. A 🏭💨 smokestack puffs when the
  foreman is attached/detached and idles when stopped; a ready-ticket conveyor
  feeds the shipping dock (done-ticket count). Hotkey `8` or
  `derrick observe --tab factory`. Read-only — substrate still polled at 1 Hz /
  on `notify`; only the animation frame ticks at ~100 ms.
- **Structured hand telemetry (D76).** Crew dispatchers (`derrick-hand`
  HostCliHandDispatcher, `derrick-copilot` local, `derrick-claude`) now emit
  typed `EventKind` variants — `HandStarted { pid, ticket }`, `HandProgress {
  snippet }`, `HandExited { code, stats }` — replacing the free-text `Note`
  bodies they previously wrote. `HandProgress` is throttled to at most one
  event per hand per 2 seconds and only on meaningful-change (snippet capped at
  ~80 display columns), bounding event-log growth. The TUI's `build_hand_rows`
  consumes the structured events directly and no longer string-matches
  `"exited successfully"` / `"hand stats:"` bodies. `Note` events remain for
  genuinely free-form operator messages.
- **Hand pid for process liveness (D75).** The `hands` table gains a `pid`
  column (migration 0005, additive NULL default — migration-safe). Crew
  dispatchers record the spawned agent child pid via a new `PidSink` on
  `HostRequest` (fired live at spawn by `derrick-tools::process::run_host`, so
  the pid is known while the agent runs, not after it exits). The foreman
  cleanup pass uses `kill(pid, 0)` liveness as a second signal alongside the
  30-minute heartbeat TTL: a dead pid abandons the hand immediately even with a
  fresh heartbeat; a live pid suppresses TTL abandonment when the heartbeat is
  merely stale (busy agent). The `Substrate` trait gains
  `register_hand_with_pid` (default delegates to `register_hand` so test mocks
  keep working). Unix only — Windows falls back to heartbeat-TTL (pid probing
  is part of the v1.1 Windows track).
- **`PipelineStepStarted` event (D77).** The flow runner emits a
  `PipelineStepStarted { step_id, index, total }` event scoped to
  `EventScope::Worktree { run_id }` at step entry, mirroring the existing
  `PipelineStepCompleted` at step exit. This bridges the live
  `ProgressReporter` plane (D60/D61, seen only by the launching terminal) into
  the persisted event log so `derrick observe` sees mid-step liveness without
  polling the launching process. Live per-line agent output is not duplicated
  into the event log (volume); the factory view tails per-step `.log` files
  for that.

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
