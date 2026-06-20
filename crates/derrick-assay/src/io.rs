//! Filesystem and config-hash helpers shared by the pipeline runner and assay.

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use crate::types::RunError;

pub fn config_hash(path: &Path) -> Result<String, RunError> {
    let bytes = std::fs::read(path).map_err(|source| RunError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let yaml: serde_yaml::Value = serde_yaml::from_slice(&bytes).map_err(|source| {
        RunError::Config(format!(
            "failed to canonicalise {}: {source}",
            path.display()
        ))
    })?;
    let canonical = serde_json::to_vec(&canonical_json(yaml)).map_err(|source| RunError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    let digest = Sha256::digest(canonical);
    Ok(format!("sha256:{}", hex_lower(&digest)))
}

pub fn canonical_json(value: serde_yaml::Value) -> serde_json::Value {
    match value {
        serde_yaml::Value::Null => serde_json::Value::Null,
        serde_yaml::Value::Bool(value) => serde_json::Value::Bool(value),
        serde_yaml::Value::Number(number) => number
            .as_i64()
            .map(serde_json::Number::from)
            .or_else(|| number.as_u64().map(serde_json::Number::from))
            .or_else(|| number.as_f64().and_then(serde_json::Number::from_f64))
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        serde_yaml::Value::String(value) => serde_json::Value::String(value),
        serde_yaml::Value::Sequence(values) => {
            serde_json::Value::Array(values.into_iter().map(canonical_json).collect())
        }
        serde_yaml::Value::Mapping(mapping) => {
            let mut object = serde_json::Map::new();
            let mut entries = BTreeMap::new();
            for (key, value) in mapping {
                entries.insert(yaml_key(key), canonical_json(value));
            }
            for (key, value) in entries {
                object.insert(key, value);
            }
            serde_json::Value::Object(object)
        }
        serde_yaml::Value::Tagged(tagged) => canonical_json(tagged.value),
    }
}

fn yaml_key(value: serde_yaml::Value) -> String {
    match value {
        serde_yaml::Value::String(value) => value,
        other => serde_json::to_string(&canonical_json(other)).unwrap_or_default(),
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ignored = write!(&mut out, "{byte:02x}");
    }
    out
}

pub fn read_to_string(path: &Path) -> Result<String, RunError> {
    std::fs::read_to_string(path).map_err(|source| RunError::Io {
        path: path.to_path_buf(),
        source,
    })
}

pub fn write_file(path: &Path, contents: &str) -> Result<(), RunError> {
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
    }
    std::fs::write(path, contents).map_err(|source| RunError::Io {
        path: path.to_path_buf(),
        source,
    })
}

pub fn write_log(path: &Path, stdout: &str, stderr: &str) -> Result<(), RunError> {
    let mut contents = String::new();
    contents.push_str(stdout);
    contents.push_str(stderr);
    write_file(path, &contents)
}

pub fn append_log(path: &Path, text: &str) -> Result<(), RunError> {
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
    }
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|source| RunError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    file.write_all(text.as_bytes())
        .map_err(|source| RunError::Io {
            path: path.to_path_buf(),
            source,
        })
}

pub fn create_dir_all(path: &Path) -> Result<(), RunError> {
    std::fs::create_dir_all(path).map_err(|source| RunError::Io {
        path: path.to_path_buf(),
        source,
    })
}

pub fn read_dir_names(path: &Path) -> Result<Vec<String>, RunError> {
    let entries = std::fs::read_dir(path).map_err(|source| RunError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| RunError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if entry
            .file_type()
            .map_err(|source| RunError::Io {
                path: entry.path(),
                source,
            })?
            .is_dir()
        {
            if let Some(name) = entry.file_name().to_str() {
                names.push(name.to_owned());
            }
        }
    }
    Ok(names)
}

pub fn parent(path: &Path) -> Result<&Path, RunError> {
    path.parent()
        .ok_or_else(|| RunError::Config(format!("path has no parent: {}", path.display())))
}

pub fn relative_to_root(
    repo_root: &Path,
    path: std::path::PathBuf,
) -> Result<std::path::PathBuf, RunError> {
    path.strip_prefix(repo_root)
        .map(std::path::Path::to_path_buf)
        .map_err(|error| RunError::Config(error.to_string()))
}

pub fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

pub fn required_step_text<'a>(
    value: Option<&'a str>,
    step_id: &str,
    field: &str,
) -> Result<&'a str, RunError> {
    value.ok_or_else(|| {
        RunError::Config(format!(
            "pipeline.{step_id}.{field}: missing required field"
        ))
    })
}

pub fn default_run_id() -> String {
    use chrono::Utc;
    Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

pub const FEATURE_JSON: &str = ".specify/feature.json";

/// Roots derrick scans for feature directories created by spec-kit or shims.
///
/// Real spec-kit writes to `specs/`; the built-in shims write to `specs/` too
/// (after the shim update). `.specify/features/` is kept as a fallback so that
/// repos initialised with older shims continue to work.
pub const FEATURE_ROOTS: [&str; 2] = ["specs", ".specify/features"];

pub fn read_feature_dir(repo_root: &Path) -> Result<std::path::PathBuf, RunError> {
    use serde_json::Value;
    let path = repo_root.join(FEATURE_JSON);
    let value: serde_json::Value =
        serde_json::from_str(&read_to_string(&path)?).map_err(|source| RunError::Json {
            path: path.clone(),
            source,
        })?;
    let feature_dir = value
        .get("feature_directory")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            RunError::Config(".specify/feature.json missing feature_directory".to_owned())
        })?;
    Ok(std::path::PathBuf::from(feature_dir))
}

/// Writes `.specify/feature.json` pointing at `feature_dir` (relative to `repo_root`).
///
/// Called by the pipeline runner after the `specify` step completes, so that
/// resume and downstream steps can locate the feature directory without
/// depending on the AI having written the file itself.
pub fn write_feature_json(repo_root: &Path, feature_dir: &std::path::Path) -> Result<(), RunError> {
    let dir = repo_root.join(".specify");
    create_dir_all(&dir)?;
    let path = repo_root.join(FEATURE_JSON);
    let content = format!(
        "{{\n  \"feature_directory\": \"{}\"\n}}\n",
        feature_dir.display()
    );
    write_file(&path, &content)
}

/// Snapshots the immediate subdirectories of every [`FEATURE_ROOTS`] entry
/// under `wd`, returning their paths relative to `wd`.
pub fn snapshot_feature_dirs(wd: &Path) -> std::collections::BTreeSet<std::path::PathBuf> {
    let mut dirs = std::collections::BTreeSet::new();
    for root_rel in FEATURE_ROOTS {
        let root = wd.join(root_rel);
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    dirs.insert(std::path::PathBuf::from(root_rel).join(entry.file_name()));
                }
            }
        }
    }
    dirs
}

/// Given a before/after snapshot pair, returns the single new feature directory
/// (relative to `wd`) created during the specify step.
///
/// If the snapshot diff finds no new directory (e.g. the feature dir already
/// existed on a retry or resume), falls back to reading `feature.json` so
/// that pipelines can recover cleanly without re-creating the directory.
pub fn resolve_new_feature_dir(
    before: &std::collections::BTreeSet<std::path::PathBuf>,
    after: &std::collections::BTreeSet<std::path::PathBuf>,
    wd: &Path,
) -> Result<std::path::PathBuf, RunError> {
    let new: Vec<_> = after.difference(before).collect();
    match new.len() {
        0 => {
            // No new dir — fall back to feature.json for retry/resume cases.
            read_feature_dir(wd).map_err(|_| {
                RunError::Config(
                    "specify step completed but no new feature directory was found in \
                     `specs/` or `.specify/features/`, and `.specify/feature.json` is \
                     absent or unreadable; make sure the specify step creates a \
                     directory under `specs/`"
                        .to_owned(),
                )
            })
        }
        1 => Ok(new[0].clone()),
        _ => Err(RunError::Config(format!(
            "specify step created {} new directories — expected exactly one ({}); \
             check that the specify step creates only one feature directory per run",
            new.len(),
            new.iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// Derives a short kebab-case slug from a free-form feature prompt.
///
/// Lowercases, splits on non-alphanumerics, keeps at most `max_words`
/// words, joins with `-`, and truncates to `max_len` characters.
pub fn prompt_to_slug(prompt: &str, max_words: usize, max_len: usize) -> String {
    let joined: String = prompt
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .take(max_words)
        .collect::<Vec<_>>()
        .join("-");
    let slug: String = joined.chars().take(max_len).collect();
    // Avoid trailing hyphen if truncation lands on one.
    slug.trim_end_matches('-').to_owned()
}

/// Returns the next sequential 3-digit prefix for a new directory under
/// `specs_dir`. Returns 1 if the directory does not exist or is empty.
///
/// Scans existing entries, extracts the leading numeric prefix from each name
/// (e.g. `001-foo` → 1) and returns `max + 1`. Non-numeric names are ignored.
pub fn next_feature_prefix(specs_dir: &Path) -> u32 {
    if !specs_dir.exists() {
        return 1;
    }
    let max = std::fs::read_dir(specs_dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(std::result::Result::ok)
        .filter_map(|e| {
            let name = e.file_name();
            let s = name.to_string_lossy().into_owned();
            s.split('-').next().and_then(|n| n.parse::<u32>().ok())
        })
        .max()
        .unwrap_or(0);
    max + 1
}

/// Marker line written into the placeholder `spec.md` stub. Used by the
/// post-step check to detect that the LLM did not actually overwrite the file.
pub const SPEC_STUB_MARKER: &str = "<!-- derrick: specify pending -->";

/// Pre-scaffolds a feature directory before the LLM `specify` step runs.
///
/// Creates `specs/<NNN>-<slug>/spec.md` (with a stub marker) and writes
/// `.specify/feature.json` pointing at it. Returns the new feature directory
/// path relative to `wd`.
///
/// This eliminates the need for the LLM to invent a path and create the
/// directory itself — the model is then told exactly where to write.
/// Function words skipped when deriving a feature-directory slug so that
/// stopwords like "a" or "that" don't pad the branch name.
const SLUG_STOPWORDS: &[&str] = &[
    "a", "an", "the", "that", "this", "these", "those", "and", "or", "but", "for", "with", "from",
    "into", "to", "in", "on", "of", "at", "by", "as", "is", "are", "was", "were", "be", "been",
    "which", "who", "what",
];

pub fn prescaffold_feature_dir(wd: &Path, prompt: &str) -> Result<std::path::PathBuf, RunError> {
    let specs_root = wd.join("specs");
    create_dir_all(&specs_root)?;
    // Pre-filter the prompt so stopwords and single-character tokens (e.g. the
    // two halves of "D&D") don't fill the slug.  Rejoin as a plain string so
    // `prompt_to_slug` can apply its own word/char limits cleanly.
    let meaningful: String = prompt
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 1 && !SLUG_STOPWORDS.contains(w))
        .collect::<Vec<_>>()
        .join(" ");
    let slug = prompt_to_slug(&meaningful, 4, 25);
    let slug = if slug.is_empty() {
        "feature".to_owned()
    } else {
        slug
    };
    // If a directory already exists for the same slug (idempotent re-run /
    // resume), reuse it rather than allocating a new NNN prefix. This keeps
    // re-runs on the same prompt from polluting `specs/` with empty stubs.
    let existing = std::fs::read_dir(&specs_root)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(std::result::Result::ok)
        .filter_map(|e| {
            let name = e.file_name();
            let s = name.to_string_lossy().into_owned();
            // Match `NNN-<slug>` exactly (slug after first hyphen).
            let (_prefix, rest) = s.split_once('-')?;
            if rest == slug.as_str() { Some(s) } else { None }
        })
        .min(); // deterministic: earliest numeric prefix wins
    let dir_name = match existing {
        Some(name) => name,
        None => {
            let prefix = next_feature_prefix(&specs_root);
            format!("{prefix:03}-{slug}")
        }
    };
    let feature_rel = std::path::PathBuf::from("specs").join(&dir_name);
    let feature_abs = wd.join(&feature_rel);
    create_dir_all(&feature_abs)?;
    let spec_path = feature_abs.join("spec.md");
    if !spec_path.exists() {
        let stub = format!("# {slug}\n\n{SPEC_STUB_MARKER}\n");
        write_file(&spec_path, &stub)?;
    }
    write_feature_json(wd, &feature_rel)?;
    Ok(feature_rel)
}

/// Verifies that the LLM actually overwrote the pre-scaffolded spec stub with
/// real content. Returns `Ok(())` if the file exists, is non-empty, and no
/// longer contains the [`SPEC_STUB_MARKER`].
pub fn verify_spec_written(wd: &Path, feature_dir: &Path) -> Result<(), RunError> {
    let spec_path = wd.join(feature_dir).join("spec.md");
    let contents = read_to_string(&spec_path).map_err(|_| {
        RunError::Config(format!(
            "specify step did not produce {}; expected the LLM to overwrite \
             the pre-scaffolded stub",
            spec_path.display()
        ))
    })?;
    if contents.trim().is_empty() {
        return Err(RunError::Config(format!(
            "specify step left {} empty; expected the LLM to write the full spec",
            spec_path.display()
        )));
    }
    if contents.contains(SPEC_STUB_MARKER) {
        return Err(RunError::Config(format!(
            "specify step did not overwrite the pre-scaffolded stub at {}; \
             the LLM appears not to have written a real specification",
            spec_path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn prompt_to_slug_basic() {
        assert_eq!(
            prompt_to_slug("Add a greet command with flags", 6, 30),
            "add-a-greet-command-with-flags"
        );
    }

    #[test]
    fn prompt_to_slug_strips_punctuation_and_lowercases() {
        assert_eq!(
            prompt_to_slug("OAuth2: integrate, with API!", 6, 30),
            "oauth2-integrate-with-api"
        );
    }

    #[test]
    fn prompt_to_slug_respects_word_limit() {
        let s = prompt_to_slug("one two three four five six seven eight", 3, 50);
        assert_eq!(s, "one-two-three");
    }

    #[test]
    fn prompt_to_slug_respects_char_limit_and_trims_trailing_hyphen() {
        // Truncation at exactly a hyphen position should not leave a dangling -
        let s = prompt_to_slug("alpha beta gamma", 6, 10);
        // "alpha-beta-gamma" -> take 10 -> "alpha-beta"
        assert_eq!(s, "alpha-beta");
    }

    #[test]
    fn prompt_to_slug_empty_input() {
        assert_eq!(prompt_to_slug("!!! ??? ...", 6, 30), "");
    }

    #[test]
    fn next_feature_prefix_returns_one_for_missing_dir() {
        let tmp = tempdir().unwrap();
        let specs = tmp.path().join("does-not-exist");
        assert_eq!(next_feature_prefix(&specs), 1);
    }

    #[test]
    fn next_feature_prefix_returns_one_for_empty_dir() {
        let tmp = tempdir().unwrap();
        let specs = tmp.path().join("specs");
        std::fs::create_dir_all(&specs).unwrap();
        assert_eq!(next_feature_prefix(&specs), 1);
    }

    #[test]
    fn next_feature_prefix_finds_next_after_existing() {
        let tmp = tempdir().unwrap();
        let specs = tmp.path().join("specs");
        std::fs::create_dir_all(specs.join("001-alpha")).unwrap();
        std::fs::create_dir_all(specs.join("003-gamma")).unwrap();
        std::fs::create_dir_all(specs.join("not-numeric")).unwrap();
        assert_eq!(next_feature_prefix(&specs), 4);
    }

    #[test]
    fn prescaffold_creates_dir_stub_and_feature_json() {
        let tmp = tempdir().unwrap();
        let feature_dir =
            prescaffold_feature_dir(tmp.path(), "Add a greet command with flags").unwrap();
        assert_eq!(
            feature_dir,
            std::path::PathBuf::from("specs/001-add-greet-command-flags")
        );
        let spec = tmp.path().join(&feature_dir).join("spec.md");
        assert!(spec.exists(), "spec.md should be created");
        let content = std::fs::read_to_string(&spec).unwrap();
        assert!(content.contains(SPEC_STUB_MARKER));

        let json_path = tmp.path().join(FEATURE_JSON);
        assert!(json_path.exists(), "feature.json should be created");
        let json = std::fs::read_to_string(&json_path).unwrap();
        assert!(
            json.contains("specs/001-add-greet-command-flags"),
            "feature.json should point at the new dir, got: {json}"
        );
    }

    #[test]
    fn prescaffold_increments_when_existing_dirs_present() {
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("specs/001-existing")).unwrap();
        std::fs::create_dir_all(tmp.path().join("specs/002-another")).unwrap();
        let feature_dir = prescaffold_feature_dir(tmp.path(), "Something new").unwrap();
        assert!(
            feature_dir
                .to_string_lossy()
                .starts_with("specs/003-something-new"),
            "got {}",
            feature_dir.display()
        );
    }

    #[test]
    fn prescaffold_reuses_existing_dir_with_same_slug() {
        let tmp = tempdir().unwrap();
        // First run creates 001-test.
        let first = prescaffold_feature_dir(tmp.path(), "test").unwrap();
        assert_eq!(first, std::path::PathBuf::from("specs/001-test"));
        // Simulate the LLM overwriting the stub.
        std::fs::write(
            tmp.path().join(&first).join("spec.md"),
            "# Real\n\nContent.\n",
        )
        .unwrap();
        // Second run with same prompt should reuse the directory.
        let second = prescaffold_feature_dir(tmp.path(), "test").unwrap();
        assert_eq!(second, first);
        // Stub should not have been re-written over the real content.
        let content = std::fs::read_to_string(tmp.path().join(&second).join("spec.md")).unwrap();
        assert!(!content.contains(SPEC_STUB_MARKER));
        assert!(content.contains("Real"));
    }

    #[test]
    fn prescaffold_falls_back_to_feature_when_slug_empty() {
        let tmp = tempdir().unwrap();
        let feature_dir = prescaffold_feature_dir(tmp.path(), "!!! ???").unwrap();
        assert_eq!(feature_dir, std::path::PathBuf::from("specs/001-feature"));
    }

    #[test]
    fn verify_spec_written_rejects_untouched_stub() {
        let tmp = tempdir().unwrap();
        let feature_dir = prescaffold_feature_dir(tmp.path(), "Hello world").unwrap();
        let err = verify_spec_written(tmp.path(), &feature_dir).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("did not overwrite"),
            "expected stub-not-overwritten error, got: {msg}"
        );
    }

    #[test]
    fn verify_spec_written_accepts_real_content() {
        let tmp = tempdir().unwrap();
        let feature_dir = prescaffold_feature_dir(tmp.path(), "Hello world").unwrap();
        std::fs::write(
            tmp.path().join(&feature_dir).join("spec.md"),
            "# Hello World\n\n## Overview\n\nReal spec content here.\n",
        )
        .unwrap();
        verify_spec_written(tmp.path(), &feature_dir).unwrap();
    }

    #[test]
    fn verify_spec_written_rejects_missing_file() {
        let tmp = tempdir().unwrap();
        let err = verify_spec_written(tmp.path(), Path::new("specs/missing")).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("did not produce"),
            "expected missing-file error, got: {msg}"
        );
    }

    #[test]
    fn verify_spec_written_rejects_empty_file() {
        let tmp = tempdir().unwrap();
        let feature_dir = prescaffold_feature_dir(tmp.path(), "Empty test").unwrap();
        std::fs::write(tmp.path().join(&feature_dir).join("spec.md"), "   \n").unwrap();
        let err = verify_spec_written(tmp.path(), &feature_dir).unwrap_err();
        assert!(format!("{err}").contains("empty"));
    }
}
