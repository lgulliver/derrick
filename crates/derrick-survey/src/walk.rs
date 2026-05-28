//! Repository file discovery for the indexer.
//!
//! Walks the working tree, prunes heavy/generated directories by name, and
//! keeps only files whose extension maps to a supported [`Lang`]. Paths are
//! reported relative to the repo root using forward slashes.

use std::path::{Path, PathBuf};

use walkdir::{DirEntry, WalkDir};

use crate::model::Lang;

/// Directory names never worth indexing.
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".derrick",
    "target",
    "node_modules",
    "dist",
    "build",
    "vendor",
    "__pycache__",
    ".venv",
    "venv",
    ".mypy_cache",
    ".pytest_cache",
    ".next",
    ".turbo",
];

/// A source file found in the working tree.
pub(crate) struct Discovered {
    /// Absolute path on disk.
    pub abs_path: PathBuf,
    /// Path relative to the repo root, forward-slashed.
    pub rel_path: String,
    /// Detected language.
    pub lang: Lang,
}

fn is_skipped_dir(entry: &DirEntry) -> bool {
    entry.file_type().is_dir()
        && entry
            .file_name()
            .to_str()
            .is_some_and(|name| SKIP_DIRS.contains(&name))
}

/// Whether a path lies under any pruned directory. Used by the watcher to
/// ignore filesystem events from the index's own DB writes (`.derrick/`) and
/// other generated trees, which would otherwise re-arm an endless rebuild.
pub(crate) fn is_under_skipped_dir(path: &Path) -> bool {
    path.components().any(|c| {
        c.as_os_str()
            .to_str()
            .is_some_and(|name| SKIP_DIRS.contains(&name))
    })
}

/// Discover all supported source files under `repo_root`.
pub(crate) fn discover(repo_root: &Path) -> Vec<Discovered> {
    WalkDir::new(repo_root)
        .into_iter()
        .filter_entry(|e| !is_skipped_dir(e))
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| {
            let lang = Lang::from_path(e.path())?;
            let rel = e
                .path()
                .strip_prefix(repo_root)
                .ok()?
                .to_string_lossy()
                .replace('\\', "/");
            Some(Discovered {
                abs_path: e.path().to_owned(),
                rel_path: rel,
                lang,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skipped_dir_detection_covers_index_writes() {
        // The index's own DB writes must be classed as ignorable, or the
        // watcher rebuild loops forever on its own output.
        assert!(is_under_skipped_dir(Path::new(
            "/repo/.derrick/index.db-wal"
        )));
        assert!(is_under_skipped_dir(Path::new("/repo/.derrick/index.db")));
        assert!(is_under_skipped_dir(Path::new("/repo/node_modules/x/y.js")));
        assert!(is_under_skipped_dir(Path::new("/repo/target/debug/foo")));
    }

    #[test]
    fn real_source_changes_are_not_skipped() {
        assert!(!is_under_skipped_dir(Path::new("/repo/src/lib.rs")));
        assert!(!is_under_skipped_dir(Path::new("/repo/app.py")));
    }
}
