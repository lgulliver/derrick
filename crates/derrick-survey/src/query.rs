//! Read-side queries: full-text search, context, impact, and status. All take
//! a borrowed read-only [`Connection`] and run inside `spawn_blocking`.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use rusqlite::{Connection, Row, params};

use crate::SurveyError;
use crate::model::{ImpactSet, IndexStatus, PendingFile, SymbolContext, SymbolHit, SymbolKind};
use crate::walk;

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

/// The summary counts every status query reads, independent of the working
/// tree: file/symbol/ref totals, the on-disk schema version, and the last-build
/// timestamp from the `meta` table.
struct IndexCounts {
    files: i64,
    symbols: i64,
    refs: i64,
    schema_version: u32,
    last_build_ts: Option<i64>,
}

/// Read the tree-independent index counts shared by [`status`] and [`stats`].
///
/// Kept separate from the working-tree diff so a pushed index — which has no
/// tree to diff against — can report real counts without walking `repo_root`.
fn read_counts(conn: &Connection) -> Result<IndexCounts, SurveyError> {
    let files: i64 = conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?;
    let symbols: i64 = conn.query_row("SELECT COUNT(*) FROM symbols", [], |r| r.get(0))?;
    let refs: i64 = conn.query_row("SELECT COUNT(*) FROM refs", [], |r| r.get(0))?;
    let schema_version: u32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;

    // Read the last-build timestamp from the meta table (may not exist on v1 DBs
    // that have not yet been migrated, or before the first build completes).
    let last_build_ts: Option<i64> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'last_build_ts'",
            [],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|s| s.parse::<i64>().ok());

    Ok(IndexCounts {
        files,
        symbols,
        refs,
        schema_version,
        last_build_ts,
    })
}

/// Stats: the same summary as [`status`] but WITHOUT diffing the working tree.
///
/// A pushed index serves a prebuilt `.db` and has no working tree to compare
/// against (its `repo_root` is just the DB's parent dir), so a tree diff would
/// spuriously report every indexed file as `deleted`. The `pending` list is
/// genuinely empty — a pushed index *is* exactly what was built, with no drift
/// concept — so freshness comes out as the normal "fresh"/timestamped label.
pub(crate) fn stats(conn: &Connection) -> Result<IndexStatus, SurveyError> {
    let counts = read_counts(conn)?;
    let pending = Vec::new();
    let freshness = compute_freshness(&pending, counts.last_build_ts, false);
    Ok(IndexStatus {
        files: counts.files as u64,
        symbols: counts.symbols as u64,
        refs: counts.refs as u64,
        schema_version: counts.schema_version,
        pending,
        last_build_ts: counts.last_build_ts,
        freshness,
    })
}

/// Status: counts plus the set of files that differ from the working tree.
///
/// `rebuilding` should be `true` when the background watcher has detected
/// changes and a rebuild is in progress (used to compute the `freshness` label).
pub(crate) fn status(
    conn: &Connection,
    repo_root: &Path,
    rebuilding: bool,
) -> Result<IndexStatus, SurveyError> {
    let IndexCounts {
        files,
        symbols,
        refs,
        schema_version,
        last_build_ts,
    } = read_counts(conn)?;

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

    let freshness = compute_freshness(&pending, last_build_ts, rebuilding);

    Ok(IndexStatus {
        files: files as u64,
        symbols: symbols as u64,
        refs: refs as u64,
        schema_version,
        pending,
        last_build_ts,
        freshness,
    })
}

/// Build the human-readable freshness label.
fn compute_freshness(
    pending: &[PendingFile],
    last_build_ts: Option<i64>,
    rebuilding: bool,
) -> String {
    if rebuilding {
        return "rebuilding".to_owned();
    }
    if pending.is_empty() {
        return "fresh".to_owned();
    }
    // Stale: include last-build time when available.
    match last_build_ts {
        Some(ts) => {
            // Format as a simple ISO-8601 UTC timestamp.
            let iso = unix_ts_to_iso8601(ts);
            format!("stale since {iso}")
        }
        None => "stale".to_owned(),
    }
}

/// Convert a Unix timestamp (seconds) to a minimal ISO-8601 UTC string,
/// e.g. `"2024-01-02T09:00:00Z"`. Falls back to the raw number on overflow.
fn unix_ts_to_iso8601(ts: i64) -> String {
    // Manual computation avoids pulling in a date-time crate.
    // We only need second-granularity and UTC.
    if ts < 0 {
        return ts.to_string();
    }
    let secs = ts as u64;
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86400;
    // Compute year/month/day from `days` (days since 1970-01-01).
    let (year, month, day) = days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Convert days-since-epoch to (year, month, day) via the proleptic Gregorian
/// calendar. Only valid for dates after 1970-01-01.
fn days_to_ymd(days: u64) -> (u32, u32, u32) {
    // Shift epoch to 1 March 0000 (a convenient astronomical calendar).
    let days = days + 719468;
    let era = days / 146097;
    let doe = days % 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as u32, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freshness_fresh_when_no_pending() {
        let label = compute_freshness(&[], Some(1_700_000_000), false);
        assert_eq!(label, "fresh");
    }

    #[test]
    fn freshness_rebuilding_overrides_pending() {
        let pending = vec![PendingFile {
            path: "src/lib.rs".to_owned(),
            reason: "modified".to_owned(),
        }];
        let label = compute_freshness(&pending, Some(1_700_000_000), true);
        assert_eq!(label, "rebuilding");
    }

    #[test]
    fn freshness_stale_with_timestamp() {
        let pending = vec![PendingFile {
            path: "src/lib.rs".to_owned(),
            reason: "modified".to_owned(),
        }];
        // 2024-01-02T09:00:00Z = 1704186000
        let label = compute_freshness(&pending, Some(1_704_186_000), false);
        assert!(
            label.starts_with("stale since "),
            "expected 'stale since ...', got: {label}"
        );
        assert!(label.contains("2024"), "should contain year: {label}");
    }

    #[test]
    fn freshness_stale_without_timestamp() {
        let pending = vec![PendingFile {
            path: "x.rs".to_owned(),
            reason: "new".to_owned(),
        }];
        let label = compute_freshness(&pending, None, false);
        assert_eq!(label, "stale");
    }

    #[test]
    fn unix_ts_to_iso8601_known_date() {
        // 2024-01-02T09:00:00Z
        assert_eq!(unix_ts_to_iso8601(1_704_186_000), "2024-01-02T09:00:00Z");
    }

    #[test]
    fn unix_ts_to_iso8601_epoch() {
        assert_eq!(unix_ts_to_iso8601(0), "1970-01-01T00:00:00Z");
    }
}
