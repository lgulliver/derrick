//! Read-side queries: full-text search, context, impact, and status. All take
//! a borrowed read-only [`Connection`] and run inside `spawn_blocking`.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use rusqlite::{params, Connection, Row};

use crate::model::{ImpactSet, IndexStatus, PendingFile, SymbolContext, SymbolHit, SymbolKind};
use crate::walk;
use crate::SurveyError;

/// Columns selected for a [`SymbolHit`], plus the symbol id as column 0.
const HIT_COLUMNS: &str = "s.id, s.name, s.kind, f.path, s.start_line, s.end_line, s.signature";

fn row_to_hit(row: &Row) -> rusqlite::Result<(i64, SymbolHit)> {
    let id: i64 = row.get(0)?;
    let kind: String = row.get(2)?;
    Ok((
        id,
        SymbolHit {
            name: row.get(1)?,
            kind: SymbolKind::from_wire(&kind),
            path: row.get(3)?,
            start_line: row.get(4)?,
            end_line: row.get(5)?,
            signature: row.get(6)?,
        },
    ))
}

/// Build an FTS5 MATCH expression: each whitespace token becomes a quoted
/// prefix term, AND-ed together. Quoting neutralises FTS5 syntax in user input.
fn fts_query(raw: &str) -> String {
    raw.split_whitespace()
        .map(|t| format!("\"{}\"*", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Full-text search returning symbol ids paired with hits, ranked by FTS5.
fn search_rows(
    conn: &Connection,
    query: &str,
    limit: usize,
) -> Result<Vec<(i64, SymbolHit)>, SurveyError> {
    let match_expr = fts_query(query);
    if match_expr.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT {HIT_COLUMNS}
         FROM symbols_fts fts
         JOIN symbols s ON s.id = fts.rowid
         JOIN files f ON f.id = s.file_id
         WHERE symbols_fts MATCH ?1
         ORDER BY rank
         LIMIT ?2"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![match_expr, limit as i64], row_to_hit)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Public search: hits only.
pub(crate) fn search(
    conn: &Connection,
    query: &str,
    limit: usize,
) -> Result<Vec<SymbolHit>, SurveyError> {
    Ok(search_rows(conn, query, limit)?
        .into_iter()
        .map(|(_, hit)| hit)
        .collect())
}

/// Context: entry-point symbols plus the resolved symbols they reference.
pub(crate) fn context(
    conn: &Connection,
    query: &str,
    limit: usize,
) -> Result<SymbolContext, SurveyError> {
    let entries = search_rows(conn, query, limit)?;
    let mut related = Vec::new();
    let mut seen: HashSet<i64> = entries.iter().map(|(id, _)| *id).collect();

    let sql = format!(
        "SELECT DISTINCT {HIT_COLUMNS}
         FROM refs r
         JOIN symbols s ON s.id = r.dst_symbol_id
         JOIN files f ON f.id = s.file_id
         WHERE r.src_symbol_id = ?1 AND r.dst_symbol_id IS NOT NULL"
    );
    let mut stmt = conn.prepare(&sql)?;
    for (id, _) in &entries {
        let rows = stmt.query_map(params![id], row_to_hit)?;
        for row in rows {
            let (rid, hit) = row?;
            if seen.insert(rid) {
                related.push(hit);
            }
        }
    }

    Ok(SymbolContext {
        entry_points: entries.into_iter().map(|(_, hit)| hit).collect(),
        related,
    })
}

/// Impact: the resolved symbol with its direct callers and callees.
pub(crate) fn impact(conn: &Connection, symbol: &str) -> Result<Option<ImpactSet>, SurveyError> {
    let select = format!(
        "SELECT {HIT_COLUMNS} FROM symbols s JOIN files f ON f.id = s.file_id
         WHERE s.name = ?1 LIMIT 1"
    );
    let mut stmt = conn.prepare(&select)?;
    let Some((symbol_id, symbol_hit)) = stmt.query_row(params![symbol], row_to_hit).ok() else {
        return Ok(None);
    };

    // Callers: any edge whose textual target is this symbol's name.
    let callers_sql = format!(
        "SELECT DISTINCT {HIT_COLUMNS}
         FROM refs r
         JOIN symbols s ON s.id = r.src_symbol_id
         JOIN files f ON f.id = s.file_id
         WHERE r.dst_name = ?1"
    );
    let mut stmt = conn.prepare(&callers_sql)?;
    let callers = collect_hits(stmt.query_map(params![symbol_hit.name], row_to_hit)?)?;

    // Callees: resolved outgoing edges from this symbol.
    let callees_sql = format!(
        "SELECT DISTINCT {HIT_COLUMNS}
         FROM refs r
         JOIN symbols s ON s.id = r.dst_symbol_id
         JOIN files f ON f.id = s.file_id
         WHERE r.src_symbol_id = ?1 AND r.dst_symbol_id IS NOT NULL"
    );
    let mut stmt = conn.prepare(&callees_sql)?;
    let callees = collect_hits(stmt.query_map(params![symbol_id], row_to_hit)?)?;

    Ok(Some(ImpactSet {
        symbol: symbol_hit,
        callers,
        callees,
    }))
}

fn collect_hits(
    rows: impl Iterator<Item = rusqlite::Result<(i64, SymbolHit)>>,
) -> Result<Vec<SymbolHit>, SurveyError> {
    let mut out = Vec::new();
    for row in rows {
        out.push(row?.1);
    }
    Ok(out)
}

/// File mtime as whole seconds since the Unix epoch, matching the value the
/// build pipeline records. Returns 0 when unavailable so it stays comparable.
fn file_mtime(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64)
}

/// Status: counts plus the set of files that differ from the working tree.
pub(crate) fn status(conn: &Connection, repo_root: &Path) -> Result<IndexStatus, SurveyError> {
    let files: i64 = conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?;
    let symbols: i64 = conn.query_row("SELECT COUNT(*) FROM symbols", [], |r| r.get(0))?;
    let refs: i64 = conn.query_row("SELECT COUNT(*) FROM refs", [], |r| r.get(0))?;
    let schema_version: u32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;

    // Index state: path -> (size, mtime, content_hash). Size and mtime let us
    // skip hashing files whose cheap stat metadata is unchanged.
    let mut stmt = conn.prepare("SELECT path, size, mtime, content_hash FROM files")?;
    let mut indexed: std::collections::HashMap<String, (i64, i64, String)> =
        std::collections::HashMap::new();
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    for row in rows {
        let (path, size, mtime, hash) = row?;
        indexed.insert(path, (size, mtime, hash));
    }

    let mut pending = Vec::new();
    let mut on_disk = HashSet::new();
    for file in walk::discover(repo_root) {
        on_disk.insert(file.rel_path.clone());
        let Some((prev_size, prev_mtime, prev_hash)) = indexed.get(&file.rel_path) else {
            pending.push(PendingFile {
                path: file.rel_path,
                reason: "new".to_owned(),
            });
            continue;
        };
        // Cheap path: unchanged size+mtime means unchanged content.
        let stat = fs::metadata(&file.abs_path).ok();
        let (disk_size, disk_mtime) = stat
            .as_ref()
            .map(|m| (m.len() as i64, file_mtime(m)))
            .unwrap_or((-1, -1));
        if disk_size == *prev_size && disk_mtime == *prev_mtime {
            continue;
        }
        // Metadata differs — confirm with a content hash before reporting it.
        let changed = fs::read(&file.abs_path)
            .ok()
            .map(|bytes| blake3::hash(&bytes).to_hex().to_string())
            .is_none_or(|now| now != *prev_hash);
        if changed {
            pending.push(PendingFile {
                path: file.rel_path,
                reason: "modified".to_owned(),
            });
        }
    }
    for path in indexed.keys() {
        if !on_disk.contains(path) {
            pending.push(PendingFile {
                path: path.clone(),
                reason: "deleted".to_owned(),
            });
        }
    }

    Ok(IndexStatus {
        files: files as u64,
        symbols: symbols as u64,
        refs: refs as u64,
        schema_version,
        pending,
    })
}
