//! The index build pipeline: discover → hash → parse (parallel) → write (one
//! transaction). The whole function is synchronous and is invoked from
//! [`crate::Survey::build`] inside `spawn_blocking`.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

use rayon::prelude::*;
use rusqlite::{Connection, params};

use crate::SurveyError;
use crate::model::{BuildOptions, BuildReport};
use crate::parse::{self, ParsedFile};
use crate::walk::{self, Discovered};

/// A discovered file with its content and freshness metadata.
struct FileMeta {
    rel_path: String,
    lang: crate::model::Lang,
    content: String,
    size: i64,
    mtime: i64,
    hash: String,
}

/// Read a file as UTF-8 and compute its content hash and freshness metadata.
/// Returns `None` for files that cannot be read as UTF-8 (binaries, etc.).
fn read_meta(file: &Discovered) -> Option<FileMeta> {
    let content = fs::read_to_string(&file.abs_path).ok()?;
    let meta = fs::metadata(&file.abs_path).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64);
    let hash = blake3::hash(content.as_bytes()).to_hex().to_string();
    Some(FileMeta {
        rel_path: file.rel_path.clone(),
        lang: file.lang,
        size: meta.len() as i64,
        mtime,
        hash,
        content,
    })
}

/// Run a full or incremental build against an open writer connection.
pub(crate) fn run(
    connection: &mut Connection,
    repo_root: &Path,
    options: BuildOptions,
) -> Result<BuildReport, SurveyError> {
    let discovered = walk::discover(repo_root);

    // Phase 1: read + hash every discovered file in parallel.
    let metas: Vec<FileMeta> = discovered.par_iter().filter_map(read_meta).collect();

    // Existing index state: path -> content_hash.
    let existing = load_existing_hashes(connection)?;

    // Phase 2: split into changed (reparse) vs unchanged.
    let (changed, unchanged): (Vec<&FileMeta>, Vec<&FileMeta>) = metas.iter().partition(|m| {
        options.full || existing.get(&m.rel_path).is_none_or(|prev| prev != &m.hash)
    });

    // Phase 3: parse changed files in parallel.
    let parsed: Vec<(&FileMeta, ParsedFile)> = changed
        .par_iter()
        .map(|m| parse::parse(m.lang, &m.content).map(|p| (*m, p)))
        .collect::<Result<_, _>>()?;

    // Phase 4: one transaction for all writes.
    let on_disk: std::collections::HashSet<&str> =
        metas.iter().map(|m| m.rel_path.as_str()).collect();
    let mut files_removed = 0u64;

    let tx = connection.transaction()?;
    {
        // Remove index entries for files no longer on disk.
        for path in existing.keys() {
            if !on_disk.contains(path.as_str()) {
                tx.execute("DELETE FROM files WHERE path = ?1", params![path])?;
                files_removed += 1;
            }
        }

        for (meta, file) in &parsed {
            write_file(&tx, meta, file)?;
        }

        // Both maintenance steps below are O(total symbols/refs), so skip them
        // entirely when nothing changed — otherwise every watcher poll pays a
        // full-table cost for a no-op build.
        if !parsed.is_empty() || files_removed > 0 {
            // Resolve reference targets to symbol ids by name (best-effort;
            // ambiguous names resolve to an arbitrary match). Unresolved refs
            // keep dst_name only.
            tx.execute(
                "UPDATE refs SET dst_symbol_id = (
                     SELECT s.id FROM symbols s WHERE s.name = refs.dst_name LIMIT 1
                 )",
                [],
            )?;

            // External-content FTS5 has no triggers; rebuild it from the
            // symbols table wholesale now that all symbol writes are done.
            tx.execute("INSERT INTO symbols_fts(symbols_fts) VALUES('rebuild')", [])?;
        }
    }
    tx.commit()?;

    // Record the build timestamp so freshness queries can surface it.
    let now = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    connection.execute(
        "INSERT INTO meta (key, value) VALUES ('last_build_ts', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![now],
    )?;

    let (symbols, refs) = counts(connection)?;
    Ok(BuildReport {
        files_indexed: parsed.len() as u64,
        files_removed,
        files_unchanged: unchanged.len() as u64,
        symbols,
        refs,
    })
}

/// Delete and re-insert one file's symbols and reference edges.
fn write_file(tx: &Connection, meta: &FileMeta, file: &ParsedFile) -> Result<(), SurveyError> {
    // Deleting the file row cascades to its symbols (and their outgoing refs).
    tx.execute("DELETE FROM files WHERE path = ?1", params![meta.rel_path])?;
    tx.execute(
        "INSERT INTO files (path, lang, size, mtime, content_hash)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            meta.rel_path,
            meta.lang.as_str(),
            meta.size,
            meta.mtime,
            meta.hash
        ],
    )?;
    let file_id = tx.last_insert_rowid();

    // Insert symbols, recording their row ids by source order.
    let mut symbol_ids = Vec::with_capacity(file.symbols.len());
    for sym in &file.symbols {
        tx.execute(
            "INSERT INTO symbols (file_id, name, kind, start_line, end_line, signature)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                file_id,
                sym.name,
                sym.kind.as_str(),
                sym.start_line,
                sym.end_line,
                sym.signature
            ],
        )?;
        symbol_ids.push(tx.last_insert_rowid());
    }

    // Attribute each reference to its enclosing symbol and insert the edge.
    for (src_idx, r) in parse::attribute_refs(&file.symbols, &file.refs) {
        tx.execute(
            "INSERT INTO refs (src_symbol_id, dst_symbol_id, dst_name, kind)
             VALUES (?1, NULL, ?2, ?3)",
            params![symbol_ids[src_idx], r.dst_name, r.kind.as_str()],
        )?;
    }
    Ok(())
}

fn load_existing_hashes(connection: &Connection) -> Result<HashMap<String, String>, SurveyError> {
    let mut stmt = connection.prepare("SELECT path, content_hash FROM files")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut map = HashMap::new();
    for row in rows {
        let (path, hash) = row?;
        map.insert(path, hash);
    }
    Ok(map)
}

fn counts(connection: &Connection) -> Result<(u64, u64), SurveyError> {
    let symbols: i64 = connection.query_row("SELECT COUNT(*) FROM symbols", [], |r| r.get(0))?;
    let refs: i64 = connection.query_row("SELECT COUNT(*) FROM refs", [], |r| r.get(0))?;
    Ok((symbols as u64, refs as u64))
}
