use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use derrick_assay::types::{RunError, RunStatus, StepRecord, StepStatus};

#[derive(Deserialize, Serialize)]
pub struct RunManifest {
    pub run_id: String,
    pub pipeline_id: String,
    pub prompt: String,
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
        Self {
            run_id,
            pipeline_id,
            prompt,
            flags,
            config_hash,
            started_at,
            finished_at: None,
            status: RunStatus::Success,
            feature_dir: None,
            steps: Vec::new(),
            tokens_in: 0,
            tokens_out: 0,
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

pub fn write_manifest(path: &Path, manifest: &RunManifest) -> Result<(), RunError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| RunError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(
        path,
        &serde_json::to_string_pretty(manifest).map_err(|source| RunError::Json {
            path: path.to_path_buf(),
            source,
        })?,
    )
    .map_err(|source| RunError::Io {
        path: path.to_path_buf(),
        source,
    })
}
