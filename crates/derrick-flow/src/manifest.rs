use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use derrick_assay::types::{RunError, RunStatus, StepRecord, StepStatus};

#[derive(Deserialize, Serialize)]
pub struct RunManifest {
    pub run_id: String,
    pub pipeline_id: String,
    pub prompt: String,
    /// Normalised SHA256 prefix of the prompt — used to match incomplete runs
    /// for the same feature so `derrick add` can auto-resume instead of
    /// starting fresh and hitting ticket-ID collisions.
    #[serde(default)]
    pub prompt_key: String,
    pub flags: FlagsManifest,
    pub config_hash: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub status: RunStatus,
    pub feature_dir: Option<std::path::PathBuf>,
    pub steps: Vec<ManifestStep>,
    #[serde(default)]
    pub tokens_in: u64,
    #[serde(default)]
    pub tokens_out: u64,
    /// When set, this run is a continuation or forced restart of the named
    /// prior run (same or different run_id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_of: Option<String>,
}

/// Normalise `prompt` and return a 12-hex-char SHA256 prefix that
/// acts as a stable identity key across re-invocations of the same feature.
///
/// Normalisation: trim, lowercase, collapse runs of whitespace to a single
/// space. That makes "  Build a webhook " and "build a webhook" the same key.
pub fn compute_prompt_key(prompt: &str) -> String {
    let normalised: String = prompt
        .trim()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let digest = Sha256::digest(normalised.as_bytes());
    format!("{digest:x}")[..12].to_owned()
}

impl RunManifest {
    /// Returns the pipeline step index to resume from.
    ///
    /// - If the last step is Failed or Halted, resume from that step (retry).
    /// - If the last step is Success or Skipped, resume from the next step.
    /// - If no steps completed, resume from step 0.
    pub fn resume_step_index(&self) -> usize {
        let last = match self.steps.last() {
            Some(s) => s,
            None => return 0,
        };
        match last.status {
            StepStatus::Failed | StepStatus::Halted => self.steps.len() - 1,
            StepStatus::Success | StepStatus::Skipped => self.steps.len(),
        }
    }
}

impl RunManifest {
    pub fn new(
        run_id: String,
        pipeline_id: String,
        prompt: String,
        flags: FlagsManifest,
        config_hash: String,
        started_at: DateTime<Utc>,
    ) -> Self {
        let prompt_key = compute_prompt_key(&prompt);
        Self {
            run_id,
            pipeline_id,
            prompt,
            prompt_key,
            flags,
            config_hash,
            started_at,
            finished_at: None,
            status: RunStatus::Success,
            feature_dir: None,
            steps: Vec::new(),
            tokens_in: 0,
            tokens_out: 0,
            resume_of: None,
        }
    }
}

#[derive(Deserialize, Serialize)]
pub struct FlagsManifest {
    pub skip: Vec<String>,
    pub unskip: Vec<String>,
    pub dry_run: bool,
}

impl FlagsManifest {
    pub fn from_input(input: &derrick_assay::types::PipelineInput) -> Self {
        Self {
            skip: input.skip.iter().cloned().collect(),
            unskip: input.unskip.iter().cloned().collect(),
            dry_run: input.dry_run,
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub struct ManifestStep {
    pub id: String,
    pub status: StepStatus,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub log_path: std::path::PathBuf,
    pub artifacts: Vec<std::path::PathBuf>,
    #[serde(default)]
    pub tokens_in: u32,
    #[serde(default)]
    pub tokens_out: u32,
    /// Raw subprocess output bytes before compression (0 for non-bash steps).
    #[serde(default)]
    pub bytes_raw: u32,
    /// Bytes removed by output compression (0 when off or N/A).
    #[serde(default)]
    pub bytes_saved: u32,
    /// Output tokens saved by roughneck prompt injection (0 when off or N/A).
    #[serde(default)]
    pub roughneck_tokens_saved: u32,
}

impl ManifestStep {
    pub fn from_record(record: &StepRecord) -> Self {
        Self {
            id: record.id.clone(),
            status: record.status,
            started_at: record.started_at,
            finished_at: record.finished_at,
            log_path: record.log_path.clone(),
            artifacts: record.artifacts.clone(),
            tokens_in: record.tokens_in,
            tokens_out: record.tokens_out,
            bytes_raw: record.bytes_raw,
            bytes_saved: record.bytes_saved,
            roughneck_tokens_saved: record.roughneck_tokens_saved,
        }
    }
}

impl From<ManifestStep> for StepRecord {
    fn from(step: ManifestStep) -> Self {
        Self {
            id: step.id,
            status: step.status,
            started_at: step.started_at,
            finished_at: step.finished_at,
            log_path: step.log_path,
            artifacts: step.artifacts,
            tokens_in: step.tokens_in,
            tokens_out: step.tokens_out,
            bytes_raw: step.bytes_raw,
            bytes_saved: step.bytes_saved,
            roughneck_tokens_saved: step.roughneck_tokens_saved,
        }
    }
}

pub fn read_manifest(path: &Path) -> Result<RunManifest, RunError> {
    let contents = std::fs::read_to_string(path).map_err(|source| RunError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_str(&contents).map_err(|source| RunError::Json {
        path: path.to_path_buf(),
        source,
    })
}

pub fn prior_feature_dir(steps: &[ManifestStep]) -> Option<std::path::PathBuf> {
    steps
        .iter()
        .flat_map(|step| step.artifacts.iter())
        .find_map(|artifact| {
            if artifact.ends_with("spec.md") {
                artifact.parent().map(std::path::Path::to_path_buf)
            } else {
                None
            }
        })
}

/// Write the run manifest atomically.
///
/// The manifest is rewritten between every step, so a crash mid-write must
/// never leave a truncated/corrupt JSON file that would poison resume. We
/// write to a temp file in the same directory, fsync its contents, then
/// rename it over the target (rename is atomic on the same filesystem). On
/// any failure the temp file is removed so it never lingers.
pub fn write_manifest(path: &Path, manifest: &RunManifest) -> Result<(), RunError> {
    use std::io::Write as _;

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|source| RunError::Io {
        path: parent.to_path_buf(),
        source,
    })?;

    let body = serde_json::to_string_pretty(manifest).map_err(|source| RunError::Json {
        path: path.to_path_buf(),
        source,
    })?;

    // Temp file lives in the same directory so the rename stays on one
    // filesystem (cross-device renames are not atomic and would fail).
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("manifest.json");
    let tmp_path = parent.join(format!(
        ".{file_name}.tmp.{}.{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));

    // Closure so we can clean up the temp file on any early error.
    let write_and_sync = || -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp_path)?;
        file.write_all(body.as_bytes())?;
        file.flush()?;
        file.sync_all()?;
        Ok(())
    };

    if let Err(source) = write_and_sync() {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(RunError::Io {
            path: tmp_path,
            source,
        });
    }

    if let Err(source) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(RunError::Io {
            path: path.to_path_buf(),
            source,
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_prompt_key_is_deterministic() {
        assert_eq!(
            compute_prompt_key("Build a webhook endpoint"),
            compute_prompt_key("build a webhook endpoint"),
            "case should not change the key"
        );
        assert_eq!(
            compute_prompt_key("build a webhook endpoint"),
            compute_prompt_key("  build  a  webhook  endpoint  "),
            "whitespace normalisation should not change the key"
        );
    }

    #[test]
    fn compute_prompt_key_is_12_hex_chars() {
        let key = compute_prompt_key("some feature");
        assert_eq!(key.len(), 12);
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn different_prompts_produce_different_keys() {
        let a = compute_prompt_key("add rate limiting");
        let b = compute_prompt_key("build a webhook endpoint");
        assert_ne!(a, b);
    }

    #[test]
    fn manifest_new_stores_prompt_key() {
        use chrono::Utc;
        let m = RunManifest::new(
            "run-1".to_owned(),
            "add-feature".to_owned(),
            "Build a webhook endpoint".to_owned(),
            FlagsManifest {
                skip: vec![],
                unskip: vec![],
                dry_run: false,
            },
            "sha256:abc".to_owned(),
            Utc::now(),
        );
        let expected = compute_prompt_key("Build a webhook endpoint");
        assert_eq!(m.prompt_key, expected);
        assert!(m.resume_of.is_none());
    }

    #[test]
    fn manifest_new_stores_resume_of_when_set() {
        use chrono::Utc;
        let mut m = RunManifest::new(
            "run-2".to_owned(),
            "add-feature".to_owned(),
            "prompt".to_owned(),
            FlagsManifest {
                skip: vec![],
                unskip: vec![],
                dry_run: false,
            },
            "sha256:abc".to_owned(),
            Utc::now(),
        );
        m.resume_of = Some("run-1".to_owned());
        assert_eq!(m.resume_of.as_deref(), Some("run-1"));
    }

    #[test]
    fn write_manifest_round_trips_and_leaves_no_temp_file() {
        use chrono::Utc;

        let tmp = tempfile::tempdir().expect("tempdir");
        let run_dir = tmp.path().join(".derrick/runs/20250101T000000Z");
        let path = run_dir.join("manifest.json");

        let manifest = RunManifest::new(
            "20250101T000000Z".to_owned(),
            "add-feature".to_owned(),
            "Build a thing".to_owned(),
            FlagsManifest {
                skip: vec![],
                unskip: vec![],
                dry_run: false,
            },
            "sha256:abc".to_owned(),
            Utc::now(),
        );

        // Write twice to exercise the rewrite-between-steps path.
        write_manifest(&path, &manifest).expect("write 1");
        write_manifest(&path, &manifest).expect("write 2");

        let read_back = read_manifest(&path).expect("read back");
        assert_eq!(read_back.run_id, manifest.run_id);
        assert_eq!(read_back.prompt, manifest.prompt);

        // No `.manifest.json.tmp.*` sidecar should remain in the run dir.
        let lingering: Vec<_> = std::fs::read_dir(&run_dir)
            .expect("read run dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp."))
            .collect();
        assert!(lingering.is_empty(), "temp files lingered: {lingering:?}");

        let entries: Vec<_> = std::fs::read_dir(&run_dir)
            .expect("read run dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["manifest.json".to_owned()]);
    }

    #[test]
    fn manifest_step_bytes_round_trip() {
        use chrono::Utc;
        use derrick_assay::types::StepStatus;
        use std::path::PathBuf;

        let step = ManifestStep {
            id: "analyze".to_owned(),
            status: StepStatus::Success,
            started_at: Utc::now(),
            finished_at: Utc::now(),
            log_path: PathBuf::from(".derrick/runs/r1/step-analyze.log"),
            artifacts: vec![],
            tokens_in: 10,
            tokens_out: 20,
            bytes_raw: 4096,
            bytes_saved: 1024,
            roughneck_tokens_saved: 0,
        };
        let json = serde_json::to_string(&step).expect("serialize");
        let decoded: ManifestStep = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded.bytes_raw, 4096);
        assert_eq!(decoded.bytes_saved, 1024);
    }

    #[test]
    fn manifest_step_roughneck_round_trip() {
        use chrono::Utc;
        use derrick_assay::types::StepStatus;
        use std::path::PathBuf;

        let step = ManifestStep {
            id: "plan".to_owned(),
            status: StepStatus::Success,
            started_at: Utc::now(),
            finished_at: Utc::now(),
            log_path: PathBuf::from(".derrick/runs/r1/step-plan.log"),
            artifacts: vec![],
            tokens_in: 100,
            tokens_out: 200,
            bytes_raw: 0,
            bytes_saved: 0,
            roughneck_tokens_saved: 500,
        };
        let json = serde_json::to_string(&step).expect("serialize");
        let decoded: ManifestStep = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded.roughneck_tokens_saved, 500);
    }

    #[test]
    fn manifest_step_roughneck_defaults_to_zero() {
        let json = r#"{
            "id": "specify",
            "status": "success",
            "started_at": "2026-05-24T09:00:00Z",
            "finished_at": "2026-05-24T09:01:00Z",
            "log_path": ".derrick/runs/r1/step-specify.log",
            "artifacts": [],
            "tokens_in": 5,
            "tokens_out": 10
        }"#;
        let step: ManifestStep = serde_json::from_str(json).expect("deserialize");
        assert_eq!(step.roughneck_tokens_saved, 0);
    }

    #[test]
    fn manifest_step_bytes_default_to_zero_when_absent() {
        // Old manifests without bytes_raw / bytes_saved should still parse.
        let json = r#"{
            "id": "specify",
            "status": "success",
            "started_at": "2026-05-24T09:00:00Z",
            "finished_at": "2026-05-24T09:01:00Z",
            "log_path": ".derrick/runs/r1/step-specify.log",
            "artifacts": [],
            "tokens_in": 5,
            "tokens_out": 10
        }"#;
        let step: ManifestStep = serde_json::from_str(json).expect("deserialize");
        assert_eq!(step.bytes_raw, 0);
        assert_eq!(step.bytes_saved, 0);
    }
}
