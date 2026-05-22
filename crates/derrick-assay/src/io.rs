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
