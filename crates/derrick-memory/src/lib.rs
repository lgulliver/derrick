//! Derrick memory layers. See DESIGN.md §9.A.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use derrick_config::Site;
use derrick_substrate::ticket_id_pattern;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

const MEMORY_INDEX: &str = "MEMORY.md";
const PROJECT_LAYER: &str = "project";
const REFERENCE_LAYER: &str = "reference";
const FEEDBACK_LAYER: &str = "feedback";
const RUNS_DIR: &str = "runs";
const STATE_FILE: &str = "state.json";
const LESSONS_FILE: &str = "lessons.md";
const FEATURES_KEY: &str = "features";

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Handles for both memory domains for a given site.
#[derive(Clone, Debug)]
pub struct MemoryStore {
    paths: MemoryPaths,
    site_name: String,
}

/// Filesystem roots used by [`MemoryStore`].
#[derive(Clone, Debug)]
pub struct MemoryPaths {
    /// Host auto-memory root. The store appends `derrick/<site_name>/`.
    pub host_memory_root: Option<PathBuf>,
    /// Repo's `.derrick/` state directory.
    pub repo_state: PathBuf,
}

/// Init-time memory seed facts.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Seeds {
    /// Project facts: one file per fact.
    pub project: Vec<(String, String)>,
    /// Reference facts: one file per fact.
    pub reference: Vec<(String, String)>,
    /// Feedback facts: one file per fact.
    pub feedback: Vec<(String, String)>,
}

/// A cross-feature lesson entry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Lesson {
    /// Timestamp when the lesson was captured.
    pub at: DateTime<Utc>,
    /// Batch slug, when extracted from a batch closure.
    pub batch: Option<String>,
    /// Gate-checked lesson body.
    pub body: String,
    /// Ticket IDs and section anchors extracted from `body` by the quality gate.
    /// Populated by [`MemoryStore::append_lesson`]; absent in legacy entries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// A memory entry discovered on disk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryEntry {
    /// Logical memory layer.
    pub layer: MemoryLayer,
    /// Entry path.
    pub path: PathBuf,
    /// Entry size in bytes.
    pub size_bytes: u64,
    /// Last modification timestamp.
    pub modified: DateTime<Utc>,
}

/// Memory layer names.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum MemoryLayer {
    /// Init-time project facts.
    Project,
    /// Init-time reference facts.
    Reference,
    /// Init-time feedback facts.
    Feedback,
    /// Per-run digest files.
    RunDigest,
    /// Per-feature state file.
    FeatureState,
    /// Cross-feature lesson file.
    Lessons,
}

/// Errors returned by memory storage operations.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum MemoryError {
    /// A filesystem operation failed.
    #[error("io error at {path}: {source}")]
    Io {
        /// Path involved in the failing operation.
        path: PathBuf,
        /// Source I/O error.
        source: std::io::Error,
    },

    /// A lesson failed the quality gate.
    #[error("lesson rejected by quality gate: {reason}")]
    Rejected {
        /// Human-readable rejection reason.
        reason: String,
    },

    /// Caller input was invalid.
    #[error("invalid input: {field}: {message}")]
    Invalid {
        /// Field that failed validation.
        field: String,
        /// Human-readable validation detail.
        message: String,
    },
}

impl MemoryStore {
    /// Construct a memory store with explicit paths.
    pub fn open(paths: MemoryPaths, site: &Site) -> Result<Self, MemoryError> {
        validate_namespace_component("site.name", site.name())?;
        create_dir_all(&paths.repo_state)?;
        Ok(Self {
            paths,
            site_name: site.name().to_owned(),
        })
    }

    /// Write the project, reference, feedback, and index seed files.
    pub fn seed(&self, seeds: &Seeds) -> Result<Vec<PathBuf>, MemoryError> {
        let Some(site_dir) = self.host_site_dir() else {
            return Ok(Vec::new());
        };

        create_dir_all(&site_dir)?;
        let mut written = Vec::new();
        let mut index_entries = Vec::new();

        self.write_seed_layer(&site_dir, PROJECT_LAYER, &seeds.project, &mut written)?;
        self.write_seed_layer(&site_dir, REFERENCE_LAYER, &seeds.reference, &mut written)?;
        self.write_seed_layer(&site_dir, FEEDBACK_LAYER, &seeds.feedback, &mut written)?;

        collect_index_entries(PROJECT_LAYER, &seeds.project, &mut index_entries)?;
        collect_index_entries(REFERENCE_LAYER, &seeds.reference, &mut index_entries)?;
        collect_index_entries(FEEDBACK_LAYER, &seeds.feedback, &mut index_entries)?;
        index_entries.sort();

        let index_body = index_entries
            .into_iter()
            .map(|entry| format!("- {entry}\n"))
            .collect::<String>();
        let index_path = site_dir.join(MEMORY_INDEX);
        write_if_changed(&index_path, index_body.as_bytes(), &mut written)?;

        written.sort();
        Ok(written)
    }

    /// Remove every file under this site's host-memory namespace.
    pub fn unmemoize(&self) -> Result<(), MemoryError> {
        let Some(site_dir) = self.host_site_dir() else {
            return Ok(());
        };
        match fs::remove_dir_all(&site_dir) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(io_error(site_dir, source)),
        }
    }

    /// List all memory entries for this site.
    pub fn list(&self) -> Result<Vec<MemoryEntry>, MemoryError> {
        let mut entries = Vec::new();
        if let Some(site_dir) = self.host_site_dir() {
            collect_layer_entries(
                &site_dir.join(PROJECT_LAYER),
                MemoryLayer::Project,
                &mut entries,
            )?;
            collect_layer_entries(
                &site_dir.join(REFERENCE_LAYER),
                MemoryLayer::Reference,
                &mut entries,
            )?;
            collect_layer_entries(
                &site_dir.join(FEEDBACK_LAYER),
                MemoryLayer::Feedback,
                &mut entries,
            )?;
        }

        collect_run_digest_entries(&self.paths.repo_state.join(RUNS_DIR), &mut entries)?;
        collect_file_entry(
            &self.paths.repo_state.join(STATE_FILE),
            MemoryLayer::FeatureState,
            &mut entries,
        )?;
        collect_file_entry(
            &self.paths.repo_state.join(LESSONS_FILE),
            MemoryLayer::Lessons,
            &mut entries,
        )?;

        entries.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(entries)
    }

    /// Append a one-line digest to a run's `memory.md`.
    pub fn append_run_digest(&self, run_id: &str, line: &str) -> Result<(), MemoryError> {
        validate_path_component("run_id", run_id)?;
        let run_dir = self.paths.repo_state.join(RUNS_DIR).join(run_id);
        create_dir_all(&run_dir)?;
        append_line(&run_dir.join("memory.md"), line)
    }

    /// Read per-feature state from `state.json`.
    pub fn get_feature_state<T: for<'de> Deserialize<'de>>(
        &self,
        feature_slug: &str,
    ) -> Result<Option<T>, MemoryError> {
        validate_path_component("feature_slug", feature_slug)?;
        let Some(root) = self.read_state_root()? else {
            return Ok(None);
        };
        let Some(features) = root.get(FEATURES_KEY).and_then(Value::as_object) else {
            return Ok(None);
        };
        let Some(value) = features.get(feature_slug) else {
            return Ok(None);
        };
        serde_json::from_value(value.clone())
            .map(Some)
            .map_err(|source| invalid_error("state.json", source.to_string()))
    }

    /// Write per-feature state to `state.json`.
    pub fn set_feature_state<T: Serialize>(
        &self,
        feature_slug: &str,
        state: &T,
    ) -> Result<(), MemoryError> {
        validate_path_component("feature_slug", feature_slug)?;
        let mut root = self.read_state_root()?.unwrap_or_default();
        let state_value = serde_json::to_value(state)
            .map_err(|source| invalid_error("state", source.to_string()))?;
        let features = root
            .entry(FEATURES_KEY.to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
        let Some(features) = features.as_object_mut() else {
            return Err(invalid_error(
                "state.json",
                "features must be a JSON object",
            ));
        };
        features.insert(feature_slug.to_owned(), state_value);
        self.write_state_root(&root)
    }

    /// Remove per-feature state when a batch closes.
    pub fn prune_feature_state(&self, feature_slug: &str) -> Result<(), MemoryError> {
        validate_path_component("feature_slug", feature_slug)?;
        let Some(mut root) = self.read_state_root()? else {
            return Ok(());
        };
        if let Some(features) = root.get_mut(FEATURES_KEY).and_then(Value::as_object_mut) {
            features.remove(feature_slug);
        }
        self.write_state_root(&root)
    }

    /// Append a lesson after applying the D9 quality gate.
    ///
    /// Tags (ticket IDs and section anchors) are extracted from the body and
    /// stored alongside it so [`load_lesson_index`] can build an exact-match
    /// index without re-parsing every entry on each load.
    ///
    /// [`load_lesson_index`]: MemoryStore::load_lesson_index
    pub fn append_lesson(&self, lesson: &Lesson) -> Result<(), MemoryError> {
        validate_lesson(lesson)?;
        let tags = extract_tags(&lesson.body)?;
        let with_tags = Lesson {
            tags,
            ..lesson.clone()
        };
        let line = serde_json::to_string(&with_tags)
            .map_err(|source| invalid_error("lesson", source.to_string()))?;
        append_line(&self.paths.repo_state.join(LESSONS_FILE), &line)
    }

    /// Load all lessons and build an in-memory [`LessonIndex`] for retrieval.
    ///
    /// The index is built once and held for the lifetime of a pipeline run.
    /// Legacy lessons whose `tags` field is absent are back-filled by
    /// re-extracting from their bodies.
    pub fn load_lesson_index(&self) -> Result<LessonIndex, MemoryError> {
        Ok(LessonIndex::build(self.lessons(None)?))
    }

    /// List lessons newer than `since`, or all lessons when `since` is `None`.
    pub fn lessons(&self, since: Option<DateTime<Utc>>) -> Result<Vec<Lesson>, MemoryError> {
        let lessons = read_lessons(&self.paths.repo_state.join(LESSONS_FILE))?;
        Ok(lessons
            .into_iter()
            .filter(|lesson| since.map_or(true, |since| lesson.at > since))
            .collect())
    }

    /// Remove lessons with `at <= older_than`, or all lessons when `older_than` is `None`.
    pub fn prune_lessons(&self, older_than: Option<DateTime<Utc>>) -> Result<usize, MemoryError> {
        let path = self.paths.repo_state.join(LESSONS_FILE);
        let lessons = read_lessons(&path)?;
        let original_count = lessons.len();
        let kept = lessons
            .into_iter()
            .filter(|lesson| older_than.is_some_and(|older_than| lesson.at > older_than))
            .collect::<Vec<_>>();
        let pruned = original_count.saturating_sub(kept.len());
        let body = lessons_to_lines(&kept)?;
        atomic_write(&path, body.as_bytes())?;
        Ok(pruned)
    }

    fn host_site_dir(&self) -> Option<PathBuf> {
        self.paths
            .host_memory_root
            .as_ref()
            .map(|root| root.join("derrick").join(&self.site_name))
    }

    fn write_seed_layer(
        &self,
        site_dir: &Path,
        layer: &str,
        facts: &[(String, String)],
        written: &mut Vec<PathBuf>,
    ) -> Result<(), MemoryError> {
        let layer_dir = site_dir.join(layer);
        create_dir_all(&layer_dir)?;
        for (name, body) in facts {
            let file_name = fact_file_name(name)?;
            write_if_changed(&layer_dir.join(file_name), body.as_bytes(), written)?;
        }
        Ok(())
    }

    fn read_state_root(&self) -> Result<Option<Map<String, Value>>, MemoryError> {
        let path = self.paths.repo_state.join(STATE_FILE);
        match fs::read(&path) {
            Ok(bytes) => {
                let value = serde_json::from_slice::<Value>(&bytes)
                    .map_err(|source| invalid_error("state.json", source.to_string()))?;
                let Value::Object(root) = value else {
                    return Err(invalid_error("state.json", "root must be a JSON object"));
                };
                Ok(Some(root))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(io_error(path, source)),
        }
    }

    fn write_state_root(&self, root: &Map<String, Value>) -> Result<(), MemoryError> {
        let path = self.paths.repo_state.join(STATE_FILE);
        let body = serde_json::to_vec_pretty(root)
            .map_err(|source| invalid_error("state.json", source.to_string()))?;
        atomic_write(&path, &body)
    }
}

fn collect_index_entries(
    layer: &str,
    facts: &[(String, String)],
    entries: &mut Vec<String>,
) -> Result<(), MemoryError> {
    for (name, _) in facts {
        entries.push(format!("{layer}/{}", fact_file_name(name)?));
    }
    Ok(())
}

fn fact_file_name(name: &str) -> Result<String, MemoryError> {
    validate_fact_name(name)?;
    Ok(format!("{name}.md"))
}

fn validate_fact_name(name: &str) -> Result<(), MemoryError> {
    if !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        Ok(())
    } else {
        Err(invalid_error(
            "seed_name",
            "must contain only ASCII letters, digits, '-' or '_'",
        ))
    }
}

fn validate_path_component(field: &str, value: &str) -> Result<(), MemoryError> {
    if !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        Ok(())
    } else {
        Err(invalid_error(
            field,
            "must be a safe relative path component",
        ))
    }
}

fn validate_namespace_component(field: &str, value: &str) -> Result<(), MemoryError> {
    if !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains('/')
        && !value.contains('\\')
    {
        Ok(())
    } else {
        Err(invalid_error(field, "must be a single path component"))
    }
}

fn validate_lesson(lesson: &Lesson) -> Result<(), MemoryError> {
    let ticket_regex = ticket_id_lesson_regex()?;
    let section_regex = section_anchor_regex()?;
    if ticket_regex.is_match(&lesson.body) || section_regex.is_match(&lesson.body) {
        Ok(())
    } else {
        Err(MemoryError::Rejected {
            reason: format!(
                "body {:?} must contain a ticket id matching {} or a section anchor matching {}",
                lesson.body,
                ticket_regex.as_str(),
                section_regex.as_str()
            ),
        })
    }
}

fn ticket_id_lesson_regex() -> Result<Regex, MemoryError> {
    let anchored = ticket_id_pattern();
    let inner = anchored.trim_start_matches('^').trim_end_matches('$');
    regex::RegexBuilder::new(&format!(r"\b{inner}\b"))
        .unicode(false)
        .build()
        .map_err(|source| invalid_error("ticket_id_pattern", source.to_string()))
}

fn section_anchor_regex() -> Result<Regex, MemoryError> {
    regex::RegexBuilder::new(r"#[A-Za-z0-9.-]+\b")
        .case_insensitive(true)
        .build()
        .map_err(|source| invalid_error("section_anchor_pattern", source.to_string()))
}

/// Extract all ticket IDs and section anchors from `text`.
///
/// Returns a sorted, deduplicated list. Used both at write time (to populate
/// [`Lesson::tags`]) and at query time (to turn a task description into lookup
/// keys for [`LessonIndex::relevant`]).
fn extract_tags(text: &str) -> Result<Vec<String>, MemoryError> {
    let ticket_re = ticket_id_lesson_regex()?;
    let section_re = section_anchor_regex()?;
    let mut tags: Vec<String> = ticket_re
        .find_iter(text)
        .chain(section_re.find_iter(text))
        .map(|m| m.as_str().to_owned())
        .collect();
    tags.sort();
    tags.dedup();
    Ok(tags)
}

/// Extract ticket IDs and section anchors from a query string for use with
/// [`LessonIndex::relevant`].
///
/// Infallible — returns an empty vec if the internal regex fails to compile
/// (which should never happen in practice).
pub fn extract_query_tags(text: &str) -> Vec<String> {
    extract_tags(text).unwrap_or_default()
}

fn create_dir_all(path: &Path) -> Result<(), MemoryError> {
    fs::create_dir_all(path).map_err(|source| io_error(path, source))
}

fn write_if_changed(
    path: &Path,
    body: &[u8],
    written: &mut Vec<PathBuf>,
) -> Result<(), MemoryError> {
    match fs::read(path) {
        Ok(existing) if existing == body => return Ok(()),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => return Err(io_error(path, source)),
    }
    atomic_write(path, body)?;
    written.push(path.to_path_buf());
    Ok(())
}

fn atomic_write(path: &Path, body: &[u8]) -> Result<(), MemoryError> {
    let Some(parent) = path.parent() else {
        return Err(invalid_error("path", "must have a parent directory"));
    };
    create_dir_all(parent)?;
    let tmp = temp_path(path)?;
    {
        let mut file = File::create(&tmp).map_err(|source| io_error(&tmp, source))?;
        file.write_all(body)
            .map_err(|source| io_error(&tmp, source))?;
        file.sync_all().map_err(|source| io_error(&tmp, source))?;
    }
    maybe_inject_pre_rename_panic();
    fs::rename(&tmp, path).map_err(|source| io_error(path, source))?;
    Ok(())
}

fn temp_path(path: &Path) -> Result<PathBuf, MemoryError> {
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return Err(invalid_error("path", "must end in a valid UTF-8 file name"));
    };
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    Ok(path.with_file_name(format!(
        ".{file_name}.tmp.{}.{}",
        std::process::id(),
        counter
    )))
}

fn append_line(path: &Path, line: &str) -> Result<(), MemoryError> {
    let Some(parent) = path.parent() else {
        return Err(invalid_error("path", "must have a parent directory"));
    };
    create_dir_all(parent)?;

    #[cfg(windows)]
    {
        // TODO(windows): replace POSIX O_APPEND reliance with a small file lock.
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|source| io_error(path, source))?;
    let mut bytes = line.as_bytes().to_vec();
    bytes.push(b'\n');
    file.write_all(&bytes)
        .map_err(|source| io_error(path, source))
}

fn collect_layer_entries(
    dir: &Path,
    layer: MemoryLayer,
    entries: &mut Vec<MemoryEntry>,
) -> Result<(), MemoryError> {
    let read_dir = match fs::read_dir(dir) {
        Ok(read_dir) => read_dir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(io_error(dir, source)),
    };

    for entry in read_dir {
        let entry = entry.map_err(|source| io_error(dir, source))?;
        collect_file_entry(&entry.path(), layer, entries)?;
    }
    Ok(())
}

fn collect_run_digest_entries(
    dir: &Path,
    entries: &mut Vec<MemoryEntry>,
) -> Result<(), MemoryError> {
    let read_dir = match fs::read_dir(dir) {
        Ok(read_dir) => read_dir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(io_error(dir, source)),
    };

    for entry in read_dir {
        let entry = entry.map_err(|source| io_error(dir, source))?;
        collect_file_entry(
            &entry.path().join("memory.md"),
            MemoryLayer::RunDigest,
            entries,
        )?;
    }
    Ok(())
}

fn collect_file_entry(
    path: &Path,
    layer: MemoryLayer,
    entries: &mut Vec<MemoryEntry>,
) -> Result<(), MemoryError> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(io_error(path, source)),
    };
    if metadata.is_file() {
        let modified = metadata
            .modified()
            .map_err(|source| io_error(path, source))?;
        entries.push(MemoryEntry {
            layer,
            path: path.to_path_buf(),
            size_bytes: metadata.len(),
            modified: DateTime::<Utc>::from(modified),
        });
    }
    Ok(())
}

fn read_lessons(path: &Path) -> Result<Vec<Lesson>, MemoryError> {
    let mut body = String::new();
    match File::open(path) {
        Ok(mut file) => {
            file.read_to_string(&mut body)
                .map_err(|source| io_error(path, source))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(io_error(path, source)),
    }

    body.lines()
        .enumerate()
        .map(|(index, line)| {
            serde_json::from_str::<Lesson>(line).map_err(|source| {
                invalid_error("lessons.md", format!("line {}: {source}", index + 1))
            })
        })
        .collect()
}

fn lessons_to_lines(lessons: &[Lesson]) -> Result<String, MemoryError> {
    let mut lines = String::new();
    for lesson in lessons {
        let line = serde_json::to_string(lesson)
            .map_err(|source| invalid_error("lesson", source.to_string()))?;
        lines.push_str(&line);
        lines.push('\n');
    }
    Ok(lines)
}

fn io_error(path: impl Into<PathBuf>, source: std::io::Error) -> MemoryError {
    MemoryError::Io {
        path: path.into(),
        source,
    }
}

fn invalid_error(field: impl Into<String>, message: impl Into<String>) -> MemoryError {
    MemoryError::Invalid {
        field: field.into(),
        message: message.into(),
    }
}

// ---------------------------------------------------------------------------
// LessonIndex
// ---------------------------------------------------------------------------

/// In-memory index of lessons, built once per pipeline run from the JSONL file.
///
/// Provides two retrieval modes:
///
/// - [`relevant`] — exact tag lookup ranked by match count, then recency.
///   Falls back to [`recent`] when the query yields no tag matches.
/// - [`recent`] — the N most recently appended lessons.
///
/// [`relevant`]: LessonIndex::relevant
/// [`recent`]: LessonIndex::recent
pub struct LessonIndex {
    lessons: Vec<Lesson>,
    /// tag (ticket id or section anchor) → indices into `lessons`
    by_tag: HashMap<String, Vec<usize>>,
}

impl LessonIndex {
    /// Build an index from a loaded lesson list.
    ///
    /// Legacy lessons with empty `tags` have their tags back-filled by
    /// re-extracting from their bodies, so the index works without requiring a
    /// migration step.
    pub fn build(lessons: Vec<Lesson>) -> Self {
        let mut by_tag: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, lesson) in lessons.iter().enumerate() {
            let tags = if lesson.tags.is_empty() {
                extract_tags(&lesson.body).unwrap_or_default()
            } else {
                lesson.tags.clone()
            };
            for tag in tags {
                by_tag.entry(tag).or_default().push(i);
            }
        }
        Self { lessons, by_tag }
    }

    /// Return up to `limit` lessons most relevant to `query_tags`.
    ///
    /// Each lesson is scored by how many of the query tags it contains.
    /// Ties are broken by recency (later-appended lessons rank higher).
    /// Falls back to [`recent`] when no lessons share any query tag.
    ///
    /// Obtain `query_tags` by calling [`extract_query_tags`] on the current
    /// task description.
    ///
    /// [`recent`]: LessonIndex::recent
    pub fn relevant(&self, query_tags: &[&str], limit: usize) -> Vec<&Lesson> {
        if limit == 0 {
            return Vec::new();
        }
        if query_tags.is_empty() {
            return self.recent(limit);
        }
        let mut scores: HashMap<usize, usize> = HashMap::new();
        for &tag in query_tags {
            if let Some(indices) = self.by_tag.get(tag) {
                for &idx in indices {
                    *scores.entry(idx).or_default() += 1;
                }
            }
        }
        if scores.is_empty() {
            return self.recent(limit);
        }
        // score desc, then index desc (recency tiebreak — lessons are appended in order)
        let mut ranked: Vec<(usize, usize)> = scores.into_iter().collect();
        ranked.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(b.0.cmp(&a.0)));
        ranked
            .into_iter()
            .take(limit)
            .map(|(idx, _)| &self.lessons[idx])
            .collect()
    }

    /// Return up to `limit` most recently appended lessons.
    pub fn recent(&self, limit: usize) -> Vec<&Lesson> {
        self.lessons.iter().rev().take(limit).collect()
    }

    /// Number of lessons in the index.
    pub fn len(&self) -> usize {
        self.lessons.len()
    }

    /// Returns `true` if the index contains no lessons.
    pub fn is_empty(&self) -> bool {
        self.lessons.is_empty()
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
fn maybe_inject_pre_rename_panic() {
    PRE_RENAME_PANIC.with(|enabled| {
        assert!(
            !enabled.replace(false),
            "injected pre-rename panic for atomic write test"
        );
    });
}

#[cfg(not(test))]
fn maybe_inject_pre_rename_panic() {}

#[cfg(test)]
thread_local! {
    static PRE_RENAME_PANIC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::panic;
    use std::sync::Arc;
    use std::thread;

    use chrono::TimeZone;
    use derrick_config::Config;
    use serde_json::json;
    use tempfile::{tempdir, TempDir};

    use super::*;

    fn temp_dir() -> TempDir {
        tempdir().unwrap_or_else(|error| panic!("tempdir should be created: {error}"))
    }

    fn site_from_yaml(name: &str, prefix: &str) -> Site {
        let dir = temp_dir();
        let path = dir.path().join("derrick.yaml");
        fs::write(
            &path,
            format!(
                r#"
version: 1
site:
  name: {name}
  prefix: {prefix}
models:
  claude-sonnet:
    provider: anthropic
    model: claude-sonnet-4-6
  codex-gpt5:
    provider: openai-cli
    model: gpt-5
roles:
  drafter: claude-sonnet
  reviewer: codex-gpt5
tools:
  speckit:
    enabled: true
    version: ">=0.4.0"
  assay:
    enabled: false
    role: reviewer
    reviewers: [reviewer]
  substrate:
    backend: native
    mode: solo
  copilot:
    agent_identity: derrick-hand
pipeline: []
guardrails:
  constitution_path: .specify/memory/constitution.md
parallelism:
  batch_max: 8
  step_max: 4
  assay_max: 2
state:
  dir: .derrick
  log_runs: true
  worktree_root: .derrick/worktrees
"#
            ),
        )
        .unwrap_or_else(|error| panic!("site fixture should be written: {error}"));
        Config::load_from_path(&path)
            .unwrap_or_else(|error| panic!("site fixture should load: {error}"))
            .site()
            .clone()
    }

    fn default_site() -> Site {
        Config::defaults().site().clone()
    }

    fn store_with_host() -> (TempDir, MemoryStore) {
        let dir = temp_dir();
        let paths = MemoryPaths {
            host_memory_root: Some(dir.path().join("host-memory")),
            repo_state: dir.path().join(".derrick"),
        };
        let store = MemoryStore::open(paths, &default_site())
            .unwrap_or_else(|error| panic!("memory store should open: {error}"));
        (dir, store)
    }

    fn seeds() -> Seeds {
        Seeds {
            project: vec![
                ("site".to_owned(), "derrick".to_owned()),
                ("prefix".to_owned(), "drk".to_owned()),
            ],
            reference: vec![("tasks".to_owned(), "tickets live under tickets/".to_owned())],
            feedback: vec![(
                "guardrails".to_owned(),
                "assay verdicts are binding".to_owned(),
            )],
        }
    }

    fn read_to_string(path: &Path) -> String {
        fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("{} should be readable: {error}", path.display()))
    }

    fn lesson(at: DateTime<Utc>, body: &str) -> Lesson {
        Lesson {
            at,
            batch: Some("batch-1".to_owned()),
            body: body.to_owned(),
            tags: extract_tags(body).unwrap_or_default(),
        }
    }

    fn utc(year: i32, month: u32, day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, 0, 0, 0)
            .single()
            .unwrap_or_else(|| panic!("test timestamp should be valid"))
    }

    #[test]
    fn seed_writes_all_layers_idempotently() {
        let (_dir, store) = store_with_host();
        let first = store
            .seed(&seeds())
            .unwrap_or_else(|error| panic!("seed should write: {error}"));
        let second = store
            .seed(&seeds())
            .unwrap_or_else(|error| panic!("seed should be idempotent: {error}"));

        assert_eq!(first.len(), 5);
        assert!(second.is_empty());
        let changed = store
            .seed(&Seeds {
                project: vec![
                    ("site".to_owned(), "derrick".to_owned()),
                    ("prefix".to_owned(), "mem".to_owned()),
                ],
                reference: vec![("tasks".to_owned(), "tickets live under tickets/".to_owned())],
                feedback: vec![(
                    "guardrails".to_owned(),
                    "assay verdicts are binding".to_owned(),
                )],
            })
            .unwrap_or_else(|error| panic!("changed seed should write: {error}"));
        assert_eq!(changed.len(), 1);
        let site_dir = store
            .host_site_dir()
            .unwrap_or_else(|| panic!("host dir exists"));
        assert_eq!(
            read_to_string(&site_dir.join(MEMORY_INDEX)),
            "- feedback/guardrails.md\n- project/prefix.md\n- project/site.md\n- reference/tasks.md\n"
        );
    }

    #[test]
    fn unmemoize_removes_only_derrick_namespace() {
        let (dir, store) = store_with_host();
        store
            .seed(&seeds())
            .unwrap_or_else(|error| panic!("seed should write: {error}"));
        let outside = dir.path().join("host-memory").join("outside.md");
        fs::write(&outside, "keep")
            .unwrap_or_else(|error| panic!("outside fixture should write: {error}"));

        store
            .unmemoize()
            .unwrap_or_else(|error| panic!("unmemoize should succeed: {error}"));

        assert!(outside.exists());
        assert!(!store
            .host_site_dir()
            .unwrap_or_else(|| panic!("host dir exists"))
            .exists());
    }

    #[test]
    fn list_returns_all_layer_entries() {
        let (_dir, store) = store_with_host();
        store
            .seed(&seeds())
            .unwrap_or_else(|error| panic!("seed should write: {error}"));
        store
            .append_run_digest("20260518", "plan accepted")
            .unwrap_or_else(|error| panic!("digest should append: {error}"));
        store
            .set_feature_state("feature-1", &json!({"status": "open"}))
            .unwrap_or_else(|error| panic!("state should write: {error}"));
        store
            .append_lesson(&lesson(utc(2026, 5, 18), "drk-7 clarified #9.A"))
            .unwrap_or_else(|error| panic!("lesson should append: {error}"));

        let layers = store
            .list()
            .unwrap_or_else(|error| panic!("list should work: {error}"))
            .into_iter()
            .map(|entry| entry.layer)
            .collect::<BTreeSet<_>>();

        assert_eq!(
            layers,
            BTreeSet::from([
                MemoryLayer::Project,
                MemoryLayer::Reference,
                MemoryLayer::Feedback,
                MemoryLayer::RunDigest,
                MemoryLayer::FeatureState,
                MemoryLayer::Lessons,
            ])
        );
    }

    #[test]
    fn run_digest_appends_atomically() {
        let (_dir, store) = store_with_host();
        let store = Arc::new(store);
        let mut handles = Vec::new();
        for index in 0..32 {
            let store = Arc::clone(&store);
            handles.push(thread::spawn(move || {
                store
                    .append_run_digest("20260518", &format!("digest-{index}"))
                    .unwrap_or_else(|error| panic!("digest should append: {error}"));
            }));
        }
        for handle in handles {
            handle
                .join()
                .unwrap_or_else(|error| panic!("thread should join: {error:?}"));
        }

        let body = read_to_string(&store.paths.repo_state.join("runs/20260518/memory.md"));
        let lines = body.lines().collect::<BTreeSet<_>>();
        assert_eq!(lines.len(), 32);
        assert!(lines.contains("digest-0"));
        assert!(lines.contains("digest-31"));
    }

    #[test]
    fn feature_state_round_trip() {
        let (_dir, store) = store_with_host();
        store
            .set_feature_state("feature-1", &json!({"ticket": "drk-1"}))
            .unwrap_or_else(|error| panic!("state should write: {error}"));

        let state = store
            .get_feature_state::<Value>("feature-1")
            .unwrap_or_else(|error| panic!("state should read: {error}"));

        assert_eq!(state, Some(json!({"ticket": "drk-1"})));
    }

    #[test]
    fn prune_feature_state_removes_only_that_feature() {
        let (_dir, store) = store_with_host();
        store
            .set_feature_state("feature-1", &json!({"ticket": "drk-1"}))
            .unwrap_or_else(|error| panic!("state should write: {error}"));
        store
            .set_feature_state("feature-2", &json!({"ticket": "drk-2"}))
            .unwrap_or_else(|error| panic!("state should write: {error}"));

        store
            .prune_feature_state("feature-1")
            .unwrap_or_else(|error| panic!("state should prune: {error}"));

        assert_eq!(
            store
                .get_feature_state::<Value>("feature-1")
                .unwrap_or_else(|error| panic!("state should read: {error}")),
            None
        );
        assert_eq!(
            store
                .get_feature_state::<Value>("feature-2")
                .unwrap_or_else(|error| panic!("state should read: {error}")),
            Some(json!({"ticket": "drk-2"}))
        );
    }

    #[test]
    fn lesson_with_ticket_id_passes() {
        let (_dir, store) = store_with_host();
        store
            .append_lesson(&lesson(utc(2026, 5, 18), "the mp-47 retry bug was fixed"))
            .unwrap_or_else(|error| panic!("lesson should pass: {error}"));

        let lessons = store
            .lessons(None)
            .unwrap_or_else(|error| panic!("lessons should read: {error}"));
        assert_eq!(lessons.len(), 1);
    }

    #[test]
    fn lesson_with_section_anchor_passes() {
        let (_dir, store) = store_with_host();
        store
            .append_lesson(&lesson(
                utc(2026, 5, 18),
                "per #9.B.7 use transcript telemetry",
            ))
            .unwrap_or_else(|error| panic!("lesson should pass: {error}"));

        let lessons = store
            .lessons(None)
            .unwrap_or_else(|error| panic!("lessons should read: {error}"));
        assert_eq!(lessons.len(), 1);
    }

    #[test]
    fn lesson_without_either_is_rejected() {
        let (_dir, store) = store_with_host();
        let result = store.append_lesson(&lesson(utc(2026, 5, 18), "be careful with concurrency"));

        assert!(matches!(result, Err(MemoryError::Rejected { .. })));
        assert!(!store.paths.repo_state.join(LESSONS_FILE).exists());
    }

    #[test]
    fn lesson_rejected_message_includes_offending_body() {
        let (_dir, store) = store_with_host();
        let result = store.append_lesson(&lesson(utc(2026, 5, 18), "be careful with concurrency"));

        assert!(matches!(result, Err(MemoryError::Rejected { .. })));
        assert!(format!("{result:?}").contains("be careful with concurrency"));
    }

    #[test]
    fn prune_lessons_removes_only_old_ones() {
        let (_dir, store) = store_with_host();
        let old = utc(2026, 1, 1);
        let boundary = utc(2026, 2, 1);
        let new = utc(2026, 3, 1);
        for lesson in [
            lesson(old, "drk-1 old lesson"),
            lesson(boundary, "drk-2 boundary lesson"),
            lesson(new, "drk-3 new lesson"),
        ] {
            store
                .append_lesson(&lesson)
                .unwrap_or_else(|error| panic!("lesson should append: {error}"));
        }

        let pruned = store
            .prune_lessons(Some(boundary))
            .unwrap_or_else(|error| panic!("lessons should prune: {error}"));
        let remaining = store
            .lessons(None)
            .unwrap_or_else(|error| panic!("lessons should read: {error}"));

        assert_eq!(pruned, 2);
        assert_eq!(remaining, vec![lesson(new, "drk-3 new lesson")]);
    }

    #[test]
    fn multiple_sites_dont_collide_in_host_memory() {
        let dir = temp_dir();
        let root = dir.path().join("host-memory");
        let repo_state = dir.path().join(".derrick");
        let first = MemoryStore::open(
            MemoryPaths {
                host_memory_root: Some(root.clone()),
                repo_state: repo_state.clone(),
            },
            &site_from_yaml("alpha", "alp"),
        )
        .unwrap_or_else(|error| panic!("first store should open: {error}"));
        let second = MemoryStore::open(
            MemoryPaths {
                host_memory_root: Some(root.clone()),
                repo_state,
            },
            &site_from_yaml("beta", "bet"),
        )
        .unwrap_or_else(|error| panic!("second store should open: {error}"));

        first
            .seed(&Seeds {
                project: vec![("site".to_owned(), "alpha".to_owned())],
                ..Seeds::default()
            })
            .unwrap_or_else(|error| panic!("first seed should write: {error}"));
        second
            .seed(&Seeds {
                project: vec![("site".to_owned(), "beta".to_owned())],
                ..Seeds::default()
            })
            .unwrap_or_else(|error| panic!("second seed should write: {error}"));

        assert_eq!(
            read_to_string(&root.join("derrick/alpha/project/site.md")),
            "alpha"
        );
        assert_eq!(
            read_to_string(&root.join("derrick/beta/project/site.md")),
            "beta"
        );
    }

    #[test]
    fn atomic_write_survives_kill_mid_save() {
        let (_dir, store) = store_with_host();
        store
            .set_feature_state("feature-1", &json!({"version": "old"}))
            .unwrap_or_else(|error| panic!("initial state should write: {error}"));
        let before = read_to_string(&store.paths.repo_state.join(STATE_FILE));

        let result = panic::catch_unwind(|| {
            PRE_RENAME_PANIC.with(|enabled| enabled.set(true));
            let _ = store.set_feature_state("feature-1", &json!({"version": "new"}));
        });

        assert!(result.is_err());
        assert_eq!(
            read_to_string(&store.paths.repo_state.join(STATE_FILE)),
            before
        );
        assert_eq!(
            store
                .get_feature_state::<Value>("feature-1")
                .unwrap_or_else(|error| panic!("state should read: {error}")),
            Some(json!({"version": "old"}))
        );
    }

    #[test]
    fn ticket_id_regex_matches_substrate_source_of_truth() {
        assert_eq!(ticket_id_pattern(), "^[a-z]{1,6}-\\d+$");
        let gate = ticket_id_lesson_regex()
            .unwrap_or_else(|error| panic!("ticket regex should compile: {error}"));

        for (body, should_match) in [
            ("drk-1 changed the retry path", true),
            ("prefix drk-001 suffix", true),
            ("DRK-1 should not match", false),
            ("drk-x should not match", false),
            ("drk-١ should not match", false),
        ] {
            assert_eq!(gate.is_match(body), should_match, "{body:?}");
        }
    }

    #[test]
    fn memory_layer_serde_names_are_stable() {
        assert_eq!(
            serde_json::to_value(MemoryLayer::Project).ok(),
            Some(serde_json::json!("project"))
        );
        assert_eq!(
            serde_json::to_value(MemoryLayer::Reference).ok(),
            Some(serde_json::json!("reference"))
        );
        assert_eq!(
            serde_json::to_value(MemoryLayer::Feedback).ok(),
            Some(serde_json::json!("feedback"))
        );
        assert_eq!(
            serde_json::to_value(MemoryLayer::RunDigest).ok(),
            Some(serde_json::json!("rundigest"))
        );
        assert_eq!(
            serde_json::to_value(MemoryLayer::FeatureState).ok(),
            Some(serde_json::json!("featurestate"))
        );
        assert_eq!(
            serde_json::to_value(MemoryLayer::Lessons).ok(),
            Some(serde_json::json!("lessons"))
        );
        assert!(matches!(
            serde_json::from_str::<MemoryLayer>("\"project\""),
            Ok(MemoryLayer::Project)
        ));
        assert!(matches!(
            serde_json::from_str::<MemoryLayer>("\"reference\""),
            Ok(MemoryLayer::Reference)
        ));
        assert!(matches!(
            serde_json::from_str::<MemoryLayer>("\"feedback\""),
            Ok(MemoryLayer::Feedback)
        ));
        assert!(matches!(
            serde_json::from_str::<MemoryLayer>("\"rundigest\""),
            Ok(MemoryLayer::RunDigest)
        ));
        assert!(matches!(
            serde_json::from_str::<MemoryLayer>("\"featurestate\""),
            Ok(MemoryLayer::FeatureState)
        ));
        assert!(matches!(
            serde_json::from_str::<MemoryLayer>("\"lessons\""),
            Ok(MemoryLayer::Lessons)
        ));
        assert!(serde_json::from_str::<MemoryLayer>("\"unknown\"").is_err());
        assert!(MemoryLayer::Project < MemoryLayer::Reference);
        assert!(MemoryLayer::Reference < MemoryLayer::Feedback);
        assert!(MemoryLayer::Feedback < MemoryLayer::RunDigest);
        assert!(MemoryLayer::RunDigest < MemoryLayer::FeatureState);
        assert!(MemoryLayer::FeatureState < MemoryLayer::Lessons);
        assert_eq!(format!("{:?}", MemoryLayer::Project), "Project");
        assert_eq!(format!("{:?}", MemoryLayer::Reference), "Reference");
        assert_eq!(format!("{:?}", MemoryLayer::Feedback), "Feedback");
        assert_eq!(format!("{:?}", MemoryLayer::RunDigest), "RunDigest");
        assert_eq!(format!("{:?}", MemoryLayer::FeatureState), "FeatureState");
        assert_eq!(format!("{:?}", MemoryLayer::Lessons), "Lessons");
        assert_eq!(MemoryLayer::Project, MemoryLayer::Project);
    }

    #[test]
    fn host_memory_none_seed_and_unmemoize_are_noops() {
        let dir = temp_dir();
        let store = MemoryStore::open(
            MemoryPaths {
                host_memory_root: None,
                repo_state: dir.path().join(".derrick"),
            },
            &default_site(),
        )
        .unwrap_or_else(|error| panic!("store should open: {error}"));

        assert!(store
            .seed(&seeds())
            .unwrap_or_else(|error| panic!("seed should no-op: {error}"))
            .is_empty());
        store
            .unmemoize()
            .unwrap_or_else(|error| panic!("unmemoize should no-op: {error}"));
        assert!(store
            .list()
            .unwrap_or_else(|error| panic!("list should work: {error}"))
            .is_empty());
    }

    #[test]
    fn corrupt_state_is_not_clobbered() {
        let (_dir, store) = store_with_host();
        let state_path = store.paths.repo_state.join(STATE_FILE);
        fs::write(&state_path, "{not-json")
            .unwrap_or_else(|error| panic!("corrupt state fixture should write: {error}"));

        let result = store.set_feature_state("feature-1", &json!({"status": "open"}));

        assert!(matches!(result, Err(MemoryError::Invalid { .. })));
        assert_eq!(read_to_string(&state_path), "{not-json");
    }

    #[test]
    fn missing_and_invalid_inputs_take_explicit_paths() {
        let (dir, store) = store_with_host();

        assert!(store
            .list()
            .unwrap_or_else(|error| panic!("empty store should list: {error}"))
            .is_empty());
        fs::create_dir_all(store.paths.repo_state.join("runs/empty"))
            .unwrap_or_else(|error| panic!("empty run dir fixture should write: {error}"));
        assert!(store
            .list()
            .unwrap_or_else(|error| panic!("store with empty run should list: {error}"))
            .is_empty());
        let mut entries = Vec::new();
        collect_file_entry(
            &store.paths.repo_state.join(RUNS_DIR),
            MemoryLayer::RunDigest,
            &mut entries,
        )
        .unwrap_or_else(|error| panic!("directory entry should be ignored: {error}"));
        assert!(entries.is_empty());
        store
            .unmemoize()
            .unwrap_or_else(|error| panic!("missing namespace should be removable: {error}"));
        store
            .prune_feature_state("feature-1")
            .unwrap_or_else(|error| panic!("missing state prune should succeed: {error}"));
        assert_eq!(
            store
                .get_feature_state::<Value>("feature-1")
                .unwrap_or_else(|error| panic!("missing state should read: {error}")),
            None
        );

        let state_path = store.paths.repo_state.join(STATE_FILE);
        fs::write(&state_path, "{}")
            .unwrap_or_else(|error| panic!("empty state fixture should write: {error}"));
        assert_eq!(
            store
                .get_feature_state::<Value>("feature-1")
                .unwrap_or_else(|error| panic!("missing feature should read: {error}")),
            None
        );
        fs::write(&state_path, r#"{"features":{"feature-1":"text"}}"#)
            .unwrap_or_else(|error| panic!("typed state fixture should write: {error}"));
        assert!(matches!(
            store.get_feature_state::<BTreeSet<String>>("feature-1"),
            Err(MemoryError::Invalid { .. })
        ));
        fs::write(&state_path, r#"{"features":"not-object"}"#)
            .unwrap_or_else(|error| panic!("bad features fixture should write: {error}"));
        assert!(matches!(
            store.set_feature_state("feature-1", &json!({"status": "open"})),
            Err(MemoryError::Invalid { .. })
        ));
        fs::write(&state_path, "[]")
            .unwrap_or_else(|error| panic!("bad root fixture should write: {error}"));
        assert!(matches!(
            store.get_feature_state::<Value>("feature-1"),
            Err(MemoryError::Invalid { .. })
        ));

        assert!(matches!(
            store.append_run_digest("../bad", "bad"),
            Err(MemoryError::Invalid { .. })
        ));
        assert!(matches!(
            store.seed(&Seeds {
                project: vec![("bad/name".to_owned(), "body".to_owned())],
                ..Seeds::default()
            }),
            Err(MemoryError::Invalid { .. })
        ));
        assert!(matches!(
            MemoryStore::open(
                MemoryPaths {
                    host_memory_root: None,
                    repo_state: dir.path().join("other-state"),
                },
                &site_from_yaml("../bad", "bad")
            ),
            Err(MemoryError::Invalid { .. })
        ));
    }

    #[test]
    fn filesystem_errors_are_reported() {
        let dir = temp_dir();
        let repo_state_file = dir.path().join("state-file");
        fs::write(&repo_state_file, "not a directory")
            .unwrap_or_else(|error| panic!("state file fixture should write: {error}"));
        assert!(matches!(
            MemoryStore::open(
                MemoryPaths {
                    host_memory_root: None,
                    repo_state: repo_state_file,
                },
                &default_site()
            ),
            Err(MemoryError::Io { .. })
        ));

        let (_dir, store) = store_with_host();
        let runs_path = store.paths.repo_state.join(RUNS_DIR);
        fs::write(&runs_path, "not a directory")
            .unwrap_or_else(|error| panic!("runs file fixture should write: {error}"));
        assert!(matches!(
            store.append_run_digest("20260518", "digest"),
            Err(MemoryError::Io { .. })
        ));
        assert!(matches!(store.list(), Err(MemoryError::Io { .. })));

        assert!(matches!(
            atomic_write(Path::new(""), b"{}"),
            Err(MemoryError::Invalid { .. })
        ));
        assert!(matches!(
            append_line(Path::new(""), "{}"),
            Err(MemoryError::Invalid { .. })
        ));
    }

    // -----------------------------------------------------------------------
    // Tag extraction
    // -----------------------------------------------------------------------

    #[test]
    fn append_lesson_writes_extracted_tags() {
        let (_dir, store) = store_with_host();
        store
            .append_lesson(&lesson(utc(2026, 5, 18), "drk-7 clarified #9.A"))
            .unwrap_or_else(|e| panic!("lesson should append: {e}"));

        let lessons = store.lessons(None).unwrap();
        assert_eq!(lessons[0].tags, vec!["#9.A", "drk-7"]);
    }

    #[test]
    fn tags_are_sorted_and_deduplicated() {
        let (_dir, store) = store_with_host();
        store
            .append_lesson(&lesson(utc(2026, 5, 18), "drk-7 and drk-7 again and #9.A"))
            .unwrap_or_else(|e| panic!("lesson should append: {e}"));

        let lessons = store.lessons(None).unwrap();
        assert_eq!(lessons[0].tags, vec!["#9.A", "drk-7"]);
    }

    #[test]
    fn extract_query_tags_extracts_ticket_ids_and_anchors() {
        let tags = extract_query_tags("implement drk-42 per spec in #9.C.5");
        assert_eq!(tags, vec!["#9.C.5", "drk-42"]);
    }

    #[test]
    fn extract_query_tags_returns_empty_for_plain_text() {
        let tags = extract_query_tags("implement the feature");
        assert!(tags.is_empty());
    }

    // -----------------------------------------------------------------------
    // LessonIndex
    // -----------------------------------------------------------------------

    #[test]
    fn index_relevant_returns_exact_tag_matches() {
        let (_dir, store) = store_with_host();
        store
            .append_lesson(&lesson(utc(2026, 5, 1), "drk-1 the first fix"))
            .unwrap();
        store
            .append_lesson(&lesson(utc(2026, 5, 2), "drk-2 the second fix"))
            .unwrap();
        store
            .append_lesson(&lesson(utc(2026, 5, 3), "drk-1 revisited after #9.A"))
            .unwrap();

        let index = store.load_lesson_index().unwrap();
        let hits = index.relevant(&["drk-1"], 10);
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|l| l.tags.contains(&"drk-1".to_owned())));
    }

    #[test]
    fn index_relevant_ranks_multi_match_first() {
        let (_dir, store) = store_with_host();
        store
            .append_lesson(&lesson(utc(2026, 5, 1), "drk-1 touches #9.B.2"))
            .unwrap();
        store
            .append_lesson(&lesson(utc(2026, 5, 2), "drk-1 only"))
            .unwrap();

        let index = store.load_lesson_index().unwrap();
        let hits = index.relevant(&["drk-1", "#9.B.2"], 10);
        // first hit must match both tags (score 2)
        assert_eq!(hits[0].tags, vec!["#9.B.2", "drk-1"]);
    }

    #[test]
    fn index_relevant_falls_back_to_recent_on_no_match() {
        let (_dir, store) = store_with_host();
        store
            .append_lesson(&lesson(utc(2026, 5, 1), "drk-1 something"))
            .unwrap();

        let index = store.load_lesson_index().unwrap();
        let hits = index.relevant(&["drk-99"], 5);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].tags.contains(&"drk-1".to_owned()));
    }

    #[test]
    fn index_relevant_falls_back_to_recent_on_empty_query() {
        let (_dir, store) = store_with_host();
        store
            .append_lesson(&lesson(utc(2026, 5, 1), "drk-1 something"))
            .unwrap();

        let index = store.load_lesson_index().unwrap();
        let hits = index.relevant(&[], 5);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn index_recent_returns_newest_first() {
        let (_dir, store) = store_with_host();
        let old = utc(2026, 3, 1);
        let new = utc(2026, 5, 1);
        store.append_lesson(&lesson(old, "drk-1 old")).unwrap();
        store.append_lesson(&lesson(new, "drk-2 new")).unwrap();

        let index = store.load_lesson_index().unwrap();
        let hits = index.recent(1);
        assert_eq!(hits[0].at, new);
    }

    #[test]
    fn index_migrates_legacy_lessons_without_tags() {
        let dir = tempdir().unwrap();
        let lessons_path = dir.path().join(".derrick").join(LESSONS_FILE);
        fs::create_dir_all(lessons_path.parent().unwrap()).unwrap();
        // write a legacy lesson with no tags field
        fs::write(
            &lessons_path,
            r#"{"at":"2026-05-01T00:00:00Z","batch":"b1","body":"drk-5 fixed the thing"}"#
                .to_owned()
                + "\n",
        )
        .unwrap();

        let store = MemoryStore::open(
            MemoryPaths {
                host_memory_root: None,
                repo_state: dir.path().join(".derrick"),
            },
            &default_site(),
        )
        .unwrap();

        let index = store.load_lesson_index().unwrap();
        let hits = index.relevant(&["drk-5"], 5);
        assert_eq!(hits.len(), 1, "legacy lesson should be found by tag");
    }

    #[test]
    fn index_limit_zero_returns_empty() {
        let (_dir, store) = store_with_host();
        store
            .append_lesson(&lesson(utc(2026, 5, 1), "drk-1 something"))
            .unwrap();
        let index = store.load_lesson_index().unwrap();
        assert!(index.relevant(&["drk-1"], 0).is_empty());
        assert!(index.recent(0).is_empty());
    }

    #[test]
    fn empty_index_is_empty() {
        let (_dir, store) = store_with_host();
        let index = store.load_lesson_index().unwrap();
        assert!(index.is_empty());
        assert_eq!(index.len(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn unix_filesystem_edge_errors_are_reported() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::fs::symlink;

        let (dir, store) = store_with_host();
        let site_dir = store
            .host_site_dir()
            .unwrap_or_else(|| panic!("host dir exists"));
        fs::create_dir_all(
            site_dir
                .parent()
                .unwrap_or_else(|| panic!("site has parent")),
        )
        .unwrap_or_else(|error| panic!("host parent should be created: {error}"));
        fs::write(&site_dir, "not a directory")
            .unwrap_or_else(|error| panic!("site file fixture should write: {error}"));
        assert!(matches!(store.unmemoize(), Err(MemoryError::Io { .. })));
        fs::remove_file(&site_dir)
            .unwrap_or_else(|error| panic!("site file fixture should remove: {error}"));

        store
            .seed(&seeds())
            .unwrap_or_else(|error| panic!("seed should write: {error}"));
        let fact_path = site_dir.join("project/site.md");
        fs::remove_file(&fact_path)
            .unwrap_or_else(|error| panic!("fact file should remove: {error}"));
        fs::create_dir(&fact_path)
            .unwrap_or_else(|error| panic!("fact dir fixture should write: {error}"));
        assert!(matches!(store.seed(&seeds()), Err(MemoryError::Io { .. })));
        fs::remove_dir(&fact_path)
            .unwrap_or_else(|error| panic!("fact dir fixture should remove: {error}"));

        fs::remove_dir_all(site_dir.join(PROJECT_LAYER))
            .unwrap_or_else(|error| panic!("project dir should remove: {error}"));
        fs::write(site_dir.join(PROJECT_LAYER), "not a directory")
            .unwrap_or_else(|error| panic!("project file fixture should write: {error}"));
        assert!(matches!(store.list(), Err(MemoryError::Io { .. })));
        fs::remove_file(site_dir.join(PROJECT_LAYER))
            .unwrap_or_else(|error| panic!("project file fixture should remove: {error}"));
        fs::create_dir(site_dir.join(PROJECT_LAYER))
            .unwrap_or_else(|error| panic!("project dir should restore: {error}"));
        fs::remove_dir_all(site_dir.join(REFERENCE_LAYER))
            .unwrap_or_else(|error| panic!("reference dir should remove: {error}"));
        fs::write(site_dir.join(REFERENCE_LAYER), "not a directory")
            .unwrap_or_else(|error| panic!("reference file fixture should write: {error}"));
        assert!(matches!(store.list(), Err(MemoryError::Io { .. })));
        fs::remove_file(site_dir.join(REFERENCE_LAYER))
            .unwrap_or_else(|error| panic!("reference file fixture should remove: {error}"));
        fs::create_dir(site_dir.join(REFERENCE_LAYER))
            .unwrap_or_else(|error| panic!("reference dir should restore: {error}"));
        fs::remove_dir_all(site_dir.join(FEEDBACK_LAYER))
            .unwrap_or_else(|error| panic!("feedback dir should remove: {error}"));
        fs::write(site_dir.join(FEEDBACK_LAYER), "not a directory")
            .unwrap_or_else(|error| panic!("feedback file fixture should write: {error}"));
        assert!(matches!(store.list(), Err(MemoryError::Io { .. })));

        let loop_path = dir.path().join("loop");
        symlink(&loop_path, &loop_path)
            .unwrap_or_else(|error| panic!("symlink loop should be created: {error}"));
        assert!(matches!(
            collect_file_entry(&loop_path, MemoryLayer::Lessons, &mut Vec::new()),
            Err(MemoryError::Io { .. })
        ));

        let bad_name = PathBuf::from(OsString::from_vec(vec![0xff]));
        assert!(matches!(
            temp_path(&bad_name),
            Err(MemoryError::Invalid { .. })
        ));

        let lessons_loop = store.paths.repo_state.join(LESSONS_FILE);
        let _ = fs::remove_file(&lessons_loop);
        symlink(&lessons_loop, &lessons_loop)
            .unwrap_or_else(|error| panic!("lessons symlink loop should be created: {error}"));
        assert!(matches!(store.lessons(None), Err(MemoryError::Io { .. })));

        let clean_paths = MemoryPaths {
            host_memory_root: None,
            repo_state: dir.path().join("clean-state"),
        };
        let clean_store = MemoryStore::open(clean_paths, &default_site())
            .unwrap_or_else(|error| panic!("clean store should open: {error}"));
        let run_memory_loop = clean_store.paths.repo_state.join("runs/bad/memory.md");
        fs::create_dir_all(
            run_memory_loop
                .parent()
                .unwrap_or_else(|| panic!("run has parent")),
        )
        .unwrap_or_else(|error| panic!("run dir should be created: {error}"));
        symlink(&run_memory_loop, &run_memory_loop)
            .unwrap_or_else(|error| panic!("run memory symlink loop should be created: {error}"));
        assert!(matches!(clean_store.list(), Err(MemoryError::Io { .. })));
        fs::remove_dir_all(clean_store.paths.repo_state.join(RUNS_DIR))
            .unwrap_or_else(|error| panic!("run dir should remove: {error}"));
        let state_loop = clean_store.paths.repo_state.join(STATE_FILE);
        symlink(&state_loop, &state_loop)
            .unwrap_or_else(|error| panic!("state symlink loop should be created: {error}"));
        assert!(matches!(
            clean_store.get_feature_state::<Value>("feature-1"),
            Err(MemoryError::Io { .. })
        ));
        assert!(matches!(clean_store.list(), Err(MemoryError::Io { .. })));
        fs::remove_file(&state_loop)
            .unwrap_or_else(|error| panic!("state symlink loop should remove: {error}"));
        let clean_lessons_loop = clean_store.paths.repo_state.join(LESSONS_FILE);
        assert!(symlink(&clean_lessons_loop, &clean_lessons_loop).is_ok());
        assert!(matches!(clean_store.list(), Err(MemoryError::Io { .. })));
    }

    #[test]
    fn lesson_read_and_prune_edges_are_explicit() {
        let (_dir, store) = store_with_host();
        assert_eq!(
            store
                .lessons(Some(utc(2026, 1, 1)))
                .unwrap_or_else(|error| panic!("missing lessons should read: {error}")),
            Vec::<Lesson>::new()
        );

        store
            .append_lesson(&lesson(utc(2026, 5, 18), "drk-8 kept for now"))
            .unwrap_or_else(|error| panic!("lesson should append: {error}"));
        assert_eq!(
            store
                .lessons(Some(utc(2026, 5, 18)))
                .unwrap_or_else(|error| panic!("lessons should filter: {error}")),
            Vec::<Lesson>::new()
        );
        assert_eq!(
            store
                .prune_lessons(None)
                .unwrap_or_else(|error| panic!("lessons should prune all: {error}")),
            1
        );
        assert_eq!(
            store
                .lessons(None)
                .unwrap_or_else(|error| panic!("lessons should be empty: {error}")),
            Vec::<Lesson>::new()
        );

        fs::write(store.paths.repo_state.join(LESSONS_FILE), "{bad-json}\n")
            .unwrap_or_else(|error| panic!("bad lessons fixture should write: {error}"));
        assert!(matches!(
            store.lessons(None),
            Err(MemoryError::Invalid { .. })
        ));
    }
}
