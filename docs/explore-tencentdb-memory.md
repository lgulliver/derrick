# Exploration: TencentDB-Agent-Memory for Derrick

**Branch:** `claude/explore-tencentdb-memory-OXd0y`
**Date:** 2026-05-18
**Repo:** https://github.com/Tencent/TencentDB-Agent-Memory

---

## 1. What TencentDB-Agent-Memory Is

A TypeScript framework for persistent agent memory built around two complementary ideas:

**Layered long-term memory (L0–L3)**

| Layer | Content | Storage |
|-------|---------|---------|
| L0 | Raw conversation turns and tool logs | SQLite + `refs/*.md` files |
| L1 | Atomic facts extracted every ~5 turns | SQLite + vector embeddings |
| L2 | Scenario blocks (grouped patterns) | Markdown files |
| L3 | User persona (long-term preferences) | `persona.md` |

Each layer links back to the one below via deterministic `node_id` references — nothing is discarded, only abstracted.

**Symbolic short-term compression**

Verbose tool logs are offloaded to external files; current task state is encoded as a Mermaid diagram with `node_id` anchors. The agent can retrieve full detail on demand via grep. Reported results: −61% tokens on WideSearch, +51% success rate.

**Hybrid retrieval**

Lessons and atoms are recalled via BM25 (full-text) + embedding vector search, fused with RRF. The agent gets the most relevant context, not all context.

---

## 2. What Derrick Currently Has

`crates/derrick-memory` implements six filesystem-based layers:

| Layer | Storage | Purpose |
|-------|---------|---------|
| `Project` | `~/.claude/.../memory/derrick/<site>/project/` | Site name, prefix, mode, languages (one file each) |
| `Reference` | `~/.claude/.../memory/derrick/<site>/reference/` | Where specs/tasks/verdicts live |
| `Feedback` | `~/.claude/.../memory/derrick/<site>/feedback/` | Guardrails and anti-patterns |
| `RunDigest` | `.derrick/runs/<ts>/memory.md` | Per-step one-liner appended after each pipeline step |
| `FeatureState` | `.derrick/state.json` | Per-feature JSON blob; pruned when batch closes |
| `Lessons` | `.derrick/lessons.md` | Cross-feature JSONL; quality-gated (D9) |

**Key properties:**
- All writes are atomic (temp-file-and-rename or O_APPEND)
- Lessons gate: each lesson must cite a ticket ID or `#section-anchor` (D9); vague maxims are rejected
- No semantic retrieval: all lessons are prepended as low-priority context at plan/assay time
- Lessons are pruned by age (`derrick memory prune --older-than 90d`)
- Zero runtime dependencies beyond chrono/serde/regex/fs

---

## 3. Gap Analysis

The three init-time seed layers (Project, Reference, Feedback) are working well — they are stable, small, and cached in the prompt via §9.B.4. No change needed there.

The two live layers have different scaling profiles:

**FeatureState** — JSON K/V per feature. Bounded by active batch count. No problem.

**Lessons** — Grows over time (bounded only by the 90-day TTL). The current approach injects *all* lessons as low-priority context. This creates two failure modes as the file grows:

1. **Token pressure.** At 50+ lessons the slice of context devoted to lessons grows proportionally, pushing out more immediately useful content or triggering caveman compression on the lessons themselves.
2. **Signal dilution.** A `plan` step for a database-heavy ticket is equally exposed to lessons about CI configuration, PR review patterns, and scaffolding bugs. Relevant and irrelevant lessons carry the same weight.

TencentDB's answer to this is relevance-ranked retrieval: inject the top-K lessons most similar to the current task rather than all of them. This is the core insight worth adopting.

**What TencentDB has that derrick doesn't need:**

| TencentDB feature | Why we skip it |
|-------------------|----------------|
| Vector embeddings (L1) | Requires an embedding model (ONNX/candle) or an external API. Breaks local-first design. BM25 over quality-gated lessons achieves 80% of the benefit at zero dependency cost. |
| Scenario/persona layers (L2/L3) | Derrick is repo-scoped, not user-scoped. Lessons are already structured (quality gate), so a persona abstraction adds overhead without benefit. |
| Mermaid symbolic compression | Caveman already handles token compression. Derrick doesn't need a separate task graph representation. |
| Full L0 conversation capture | Derrick uses Claude Code transcript telemetry (D14) for this. Duplicating it in the memory crate would create two sources of truth. |
| TypeScript codebase | Not applicable — we need Rust. |

---

## 4. Proposed Derrick-Native Adaptation

**Core change: SQLite-backed lessons with FTS5 retrieval**

Move `lessons.md` storage from append-only JSONL to the substrate SQLite database. Use SQLite's built-in FTS5 extension for full-text search. This gives:

- `relevant_lessons(query, limit)` — keyword-ranked retrieval for plan/assay steps
- `lessons(since)` — existing time-filtered API, unchanged (scans the table)
- `append_lesson` — unchanged externally; writes to DB instead of file
- `prune_lessons` — unchanged; deletes old rows

No embedding model. No new process. No new binary dependencies. The substrate DB is already open at every pipeline step.

**Proposed schema addition to `derrick-substrate`:**

```sql
CREATE TABLE IF NOT EXISTS memory_lessons (
    id       INTEGER PRIMARY KEY,
    site     TEXT    NOT NULL,
    at       TEXT    NOT NULL,  -- ISO-8601 UTC
    batch    TEXT,
    body     TEXT    NOT NULL
);

CREATE VIRTUAL TABLE IF NOT EXISTS memory_lessons_fts
    USING fts5(body, content='memory_lessons', content_rowid='id');

-- triggers to keep FTS in sync
CREATE TRIGGER memory_lessons_ai AFTER INSERT ON memory_lessons BEGIN
    INSERT INTO memory_lessons_fts(rowid, body) VALUES (new.id, new.body);
END;
CREATE TRIGGER memory_lessons_ad AFTER DELETE ON memory_lessons BEGIN
    INSERT INTO memory_lessons_fts(memory_lessons_fts, rowid, body) VALUES('delete', old.id, old.body);
END;
```

**New `MemoryStore` method:**

```rust
/// Return up to `limit` lessons most relevant to `query`, ranked by FTS5 BM25.
pub fn relevant_lessons(
    &self,
    query: &str,
    limit: usize,
) -> Result<Vec<Lesson>, MemoryError>
```

**Migration path:**

On first `MemoryStore::open` after the upgrade, if `lessons.md` exists and the DB table is empty, migrate all existing JSONL lines to the table and rename the file to `lessons.md.migrated`. The JSONL file becomes a backup, not the live store.

**Backward compatibility:**

The existing `append_lesson`, `lessons`, and `prune_lessons` API signatures stay unchanged. Only the storage backend changes. The quality gate (D9) stays exactly as-is.

---

## 5. What the Plan/Assay Steps Get

Currently:
```
// inject all lessons as low-priority context
let context = store.lessons(None)?;  // could be 80+ entries
```

After:
```
// inject only relevant lessons for this specific task
let context = store.relevant_lessons(&task_description, 8)?;
```

This mirrors TencentDB's L1 recall behaviour — but using FTS5 BM25 rather than vector similarity. Given derrick's quality gate (lessons are specific, citable, non-vague), keyword match is a strong signal. A lesson about `drk-42` will rank higher for a similar ticket than a lesson about CI infrastructure.

---

## 6. What This Does Not Change

- The three seed layers (Project/Reference/Feedback) stay filesystem-based. They are static, small, and integrate with Claude Code's host memory system as designed.
- D9 quality gate stays. BM25 retrieval rewards specificity; high-quality, well-cited lessons rank better.
- D23 stays. Brownfield behaviour (no constitution → no lessons) is unchanged.
- `derrick memory list | show | prune | unmemoize` CLI surface stays. The TUI Memory tab (§5.7 / DESIGN.md line 606) displays the same layers.
- Atom extraction and scenario synthesis (L1/L2 from TencentDB) are not ruled out as a future enhancement, but are out of scope until `relevant_lessons` is shipping and we have data on whether BM25 is sufficient.

---

## 7. Open Questions / Decisions Needed

These are candidates for new D-entries before implementation starts:

| # | Question |
|---|---------|
| Q1 | **Storage location.** Should the lessons table live in the substrate DB (`derrick-substrate-native`) or in a separate `memory.db` under `.derrick/`? Using the substrate DB is simpler and avoids a second SQLite file, but couples `derrick-memory` more tightly to `derrick-substrate`. A `memory.db` keeps the crate boundary clean. |
| Q2 | **Dual-write period.** Should we dual-write to both JSONL and DB during a transition window so existing installations can roll back? Or migrate-on-open with the `.migrated` backup sufficient? |
| Q3 | **FTS5 availability.** SQLite ships with FTS5 disabled on some platforms (particularly older distros). Should we compile our own sqlite (`bundled` feature in rusqlite) or gate FTS5 support with a fallback to full-scan? |
| Q4 | **Relevance tuning.** FTS5 BM25 is the default ranking. Should we expose a configurable `memory.search_depth` in `derrick.yaml` (default 8 lessons per recall), or hard-code it? |
| Q5 | **Run digest retrieval.** Run digests are currently appended as sequential one-liners and read wholesale by subsequent steps. Should the same FTS5 approach apply to run digests, or are they always small enough to inject wholesale? |
| Q6 | **Vector path.** If BM25 proves insufficient (e.g., tickets using domain jargon not present in lesson text), should we plan for sqlite-vec from the start (reserve the schema extension point), or treat it as a complete re-design decision? |

---

## 8. Verdict

**Borrow the idea, not the code.** TencentDB's full L0–L3 pipeline is sized for a general-purpose conversational agent accumulating years of user interaction. Derrick's memory problem is narrower and more structured: bounded, quality-gated lessons that need relevance-ranked injection at plan and assay time.

The single most valuable idea from TencentDB is **retrieval over injection** — don't dump all lessons into context, search for the relevant ones. SQLite FTS5 is the right Rust-native implementation of that idea given derrick's existing infrastructure.

A full replacement of `derrick-memory` with a TencentDB-style system is not recommended. A targeted enhancement to add `relevant_lessons` backed by SQLite FTS5 captures ~80% of the benefit at ~10% of the implementation cost and zero new runtime dependencies.

**Recommended next step:** Record this as a new ticket (T016-ish) targeting `derrick-substrate` (schema) and `derrick-memory` (new method + migration), and file a design-question issue for Q1 and Q3 before implementation starts.
