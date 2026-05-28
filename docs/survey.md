# derrick survey — code-graph index

> Design decisions: D54, D55, D56, D57 (DESIGN.md §12). Architecture: §9.B.8.

derrick survey is a native Rust code-graph index that lets AI agents query your repository's symbol structure — functions, types, call relationships, cross-file references — with a single MCP tool call instead of fanning out across dozens of `grep`/`glob`/`Read` operations.

---

## Why it exists

When an agent needs to understand a piece of code it doesn't already know about, it typically issues a cascade of tool calls: grep for the function name, read the file, grep for callers, read those files, and so on. Each read brings raw file content across the model boundary, consuming input tokens proportional to the file size regardless of how little of it is relevant.

derrick survey breaks that pattern. It pre-indexes the entire repository into a SQLite + FTS5 database and exposes it over an MCP server. An agent can answer "where is `parse_session` defined and who calls it?" with one `derrick_survey_impact` call instead of five to ten reads. The savings compound: the index is built once (and kept fresh automatically), but queried on every turn.

The design is inspired by [CodeGraph](https://github.com/colbymchenry/codegraph) (MIT), which benchmarked a ~35% cost reduction and ~71% fewer tool calls across seven representative repos. derrick's implementation is native Rust — no Node.js runtime, no separate process, one static binary — matching the project's no-external-runtime-dependencies constraint (D11, D54).

---

## How it works

### Build phase

`derrick survey build` walks the repository, skipping `.git`, `.derrick`, `target`, `node_modules`, and other noise directories. For each source file it:

1. Detects the language by extension.
2. Parses the file with [tree-sitter](https://tree-sitter.github.io/tree-sitter/) using pre-compiled, statically-linked grammars.
3. Extracts **symbols** (functions, types, interfaces, enums, constants, modules) with their name, kind, line span, and a one-line signature.
4. Extracts **outgoing references** (calls and references) and attributes each to its innermost enclosing symbol.

All parsing is parallelised per-file with `rayon`. Results are written to SQLite in a single transaction (one writer task). A full rebuild of a medium-sized Rust workspace takes a few seconds.

### Storage

The index lives at **`.derrick/index.db`** — separate from the substrate database at `.derrick/derrick.db`. The two databases have different schemas, lifecycles, and concurrency profiles:

| | `index.db` | `derrick.db` |
|---|---|---|
| Purpose | Rebuildable read cache | Authoritative pipeline state |
| Journal mode | WAL (many concurrent readers) | WAL |
| Lifecycle | Gitignored, rebuilt on demand | Persistent, never dropped |
| Writer | Single (build pipeline) | Single (foreman) |

Schema:

```
files(id, path, lang, size, mtime, content_hash)
symbols(id, file_id, name, kind, start_line, end_line, signature)
refs(id, src_symbol_id, dst_symbol_id, dst_name, kind)
symbols_fts  — FTS5 virtual table over name + signature
```

References that cannot be resolved to a known symbol are stored with a `NULL` `dst_symbol_id` and the raw target name in `dst_name`. Resolution runs as a post-write pass after each build.

### MCP server

`derrick survey serve --mcp` launches an MCP server over stdio using the official [`rmcp`](https://crates.io/crates/rmcp) Rust SDK. `derrick init` wires it automatically — you do not start it manually.

The server exposes four tools:

| Tool | Input | What it returns |
|---|---|---|
| `derrick_survey_search` | `query: string` | FTS5 symbol search — names and signatures matching the query, ranked by relevance, with file and line. |
| `derrick_survey_context` | `query: string` | Entry-point symbols matching the query plus the symbols they directly reference — the "one call answers the architecture question" tool. |
| `derrick_survey_impact` | `symbol: string` | The matching symbol(s) with their direct callers and callees. Matching is by name; results may include unrelated symbols that share the name. |
| `derrick_survey_status` | — | Index freshness: file counts, last build time, and the list of files that have changed since the last build. |

Each response includes a **freshness banner** if there are pending files — files that have changed since the last build. The agent sees the banner and can decide whether to `Read` the affected file directly or trigger a rebuild.

### Watcher

When the MCP server is running, a `notify`-based file watcher debounces filesystem events (500 ms window) and triggers an incremental rebuild. The rebuild only touches files whose `(size, mtime)` pair has changed; unchanged files are skipped. Events from `.derrick/index.db` itself are filtered out to prevent the watcher from looping on its own writes.

---

## Setup

`derrick init` handles all wiring automatically. After running it:

- **`.mcp.json`** (repo root, checked into VCS) declares the MCP server:
  ```json
  {
    "mcpServers": {
      "derrick-survey": {
        "type": "stdio",
        "command": "derrick",
        "args": ["survey", "serve", "--mcp"]
      }
    }
  }
  ```
- **`.claude/settings.json`** receives permission allow-list entries so Claude Code does not prompt on each tool call:
  ```json
  {
    "permissions": {
      "allow": [
        "mcp__derrick-survey__derrick_survey_search",
        "mcp__derrick-survey__derrick_survey_context",
        "mcp__derrick-survey__derrick_survey_impact",
        "mcp__derrick-survey__derrick_survey_status"
      ]
    }
  }
  ```

**One-time trust prompt:** Claude Code requires an interactive project-trust confirmation the first time it loads `.mcp.json`. This prompt appears in the Claude Code UI and cannot be bypassed programmatically. Accept it once; subsequent sessions start without prompting.

For existing repos that were adopted before survey was added, run `derrick init` again — it is brownfield-safe and will add the missing files without touching what is already there.

After setup, build the initial index:

```bash
derrick survey build
```

Subsequent builds are incremental. The watcher keeps the index fresh while the MCP server is running.

---

## CLI reference

All survey subcommands are under `derrick survey`.

### `derrick survey build`

```
derrick survey build [--repo <path>]
```

(Re)indexes the repository. Walks all source files, skipping noise directories. On an incremental run (index already exists), only changed files are re-parsed. Prints a build report: files indexed, symbols extracted, references resolved, elapsed time.

### `derrick survey search <query>`

```
derrick survey search "parse session tokens"
```

FTS5 full-text search over symbol names and signatures. Returns a ranked list of matching symbols with file, line, kind, and signature. Terms are AND-ed; each term is treated as a prefix match, so `parse` matches `parse_session`, `parser`, `parsed_file`, etc.

### `derrick survey context <query>`

```
derrick survey context "foreman dispatch loop"
```

Returns entry-point symbols matching the query plus the symbols they directly reference. Use this when you want to understand how a subsystem is structured — one call gives you the shape of the module without reading individual files.

### `derrick survey impact <symbol>`

```
derrick survey impact "TokenUsage"
```

Returns the matching symbol(s) with their direct callers and callees. Use this to understand the blast radius of a change or to find every call site before a refactor.

### `derrick survey status`

```
derrick survey status
```

Prints index freshness: total files indexed, total symbols, last build time, and the list of files that have been modified since the last build (pending files).

### `derrick survey serve --mcp`

```
derrick survey serve --mcp
```

Runs the MCP server over stdio. This is what `derrick init` registers in `.mcp.json`; you do not normally run it manually. Launched by Claude Code on connection and terminated when the session ends.

---

## Supported languages

| Language | Extensions |
|---|---|
| Rust | `.rs` |
| TypeScript | `.ts`, `.tsx` |
| JavaScript | `.js`, `.jsx`, `.mjs`, `.cjs` |
| Python | `.py` |
| Go | `.go` |

Symbol extraction covers: functions/methods, types/structs/classes, interfaces, enums, constants, and modules. Reference extraction covers: function calls and identifier references.

Languages not in this list are ignored at index time. The index still answers queries against the languages it does know about.

---

## Configuration

Survey options live under `tools.survey` in `derrick.yaml`:

```yaml
tools:
  survey:
    enabled: true          # set false to disable survey entirely
    reader_pool: 4         # SQLite reader connections (default: 4)
    debounce_ms: 500       # watcher debounce window in milliseconds
    skip_dirs:             # additional directories to skip (appended to built-in list)
      - my-generated-dir
```

Built-in skip list: `.git`, `.derrick`, `target`, `node_modules`, `dist`, `build`, `vendor`, `__pycache__`, `.venv`, `venv`, `.mypy_cache`, `.pytest_cache`, `.next`, `.turbo`.

---

## Token savings accounting

`derrick gain` includes a survey line showing how many MCP tool calls agents issued against the index and the estimated tokens saved. The savings figure is conservative: each query is credited with **300 input tokens** — roughly the cost of one avoided `Read` of a function-sized span (~200 lines at ~4 bytes/token, minus overhead). It counts only avoided *input* tokens (file bytes that would otherwise enter the prompt), never output.

```
$ derrick gain

Scrub     ──  bytes_raw: 1.2 MB   bytes_saved: 1.0 MB  (84%)
Caveman   ──  chars_in:  48 000   chars_out:   17 000  (65% at Full)
Survey    ──  queries: 12          tokens_saved: 3 600  (est. 300/query)
```

The query count is read from Claude Code session transcripts (`~/.claude/projects/<repo-key>/*.jsonl`), deduplicated by message ID to avoid counting sidechain replays. `derrick gain --run <id>` shows per-step breakdown for a pipeline run.

---

## Architecture notes

**Single writer.** The build pipeline holds an exclusive write lock for the duration of a build. Agents reading over MCP use a reader pool (default: 4 connections) that operates concurrently with an in-progress build via WAL. A build does not block reads; reads do not block a build.

**Cross-worktree reads.** The index DB is at the repo root, not inside a worktree. All parallel hands (agents running in isolated git worktrees under `.derrick/worktrees/`) share one read-only view of the index (D38). This is correct: the index reflects `HEAD` at build time, which is the common ancestor all worktrees branch from.

**Incremental correctness.** A file is re-indexed when its `(size, mtime)` pair changes. This is a freshness hint, not a guarantee — a same-size same-second edit will not be detected until the watcher fires on the next filesystem event. The authoritative correctness check is `content_hash` (SHA-256), used during `build` to partition changed vs. unchanged files. The status command uses the lighter `(size, mtime)` check so it remains fast.

**FTS5 external content.** The `symbols_fts` table is an FTS5 external-content table over `symbols`. This means FTS does not store its own copy of the text; all reads go through the `symbols` table, saving space. The FTS index is rebuilt after each incremental build.

**`.mcp.json` vs. `settings.json` split (D57).** Claude Code does not honour `mcpServers` keys in `.claude/settings.json` for project-scoped servers. The server stanza goes in `.mcp.json` (checked into VCS, project-scoped); per-tool permissions go in `.claude/settings.json` (local, gitignored for the `settings.local.json` variant). Both files are written by `derrick init`.

---

## Troubleshooting

**Index is stale — agents see outdated results.**
Run `derrick survey build` to force a full rebuild. If the watcher is running (MCP server active), it will pick up file changes automatically; a full rebuild is only needed if many files changed while the server was stopped.

**`derrick survey status` shows many pending files.**
The index was built before recent edits. Run `derrick survey build` or wait for the watcher to debounce and rebuild.

**Claude Code asks for project trust on startup.**
This is the one-time interactive `.mcp.json` trust prompt. Accept it in the Claude Code UI. It will not appear again for this project.

**Survey queries return no results.**
Check that `derrick survey build` has been run at least once (`derrick survey status` will report `files: 0` if not). Confirm the files you expect are in a supported language and not in a skipped directory.

**`derrick survey serve --mcp` fails to start.**
Ensure `derrick` is on `PATH` (the MCP server is launched by Claude Code using the binary name, not a full path). Run `which derrick` and `derrick --version` to confirm.
