//! Shared pipeline run/step types — see DESIGN.md §5.3 and §10.

use chrono::{DateTime, Utc};
use derrick_models::ModelError;
use derrick_substrate::SubstrateError;
use derrick_tools::HostError;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;

/// Internal result of executing a single pipeline step.
/// Used internally by step implementations before conversion to `StepRecord`.
pub struct StepExecution {
    pub status: StepStatus,
    pub artifacts: Vec<PathBuf>,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub message: String,
}

impl StepExecution {
    pub fn success(artifacts: Vec<PathBuf>) -> Self {
        Self {
            status: StepStatus::Success,
            artifacts,
            tokens_in: 0,
            tokens_out: 0,
            message: String::new(),
        }
    }

    pub fn skipped() -> Self {
        Self {
            status: StepStatus::Skipped,
            artifacts: Vec::new(),
            tokens_in: 0,
            tokens_out: 0,
            message: String::new(),
        }
    }

    pub fn halted(artifacts: Vec<PathBuf>, message: impl Into<String>) -> Self {
        Self {
            status: StepStatus::Halted,
            artifacts,
            tokens_in: 0,
            tokens_out: 0,
            message: message.into(),
        }
    }

    pub fn with_tokens(mut self, tokens_in: u32, tokens_out: u32) -> Self {
        self.tokens_in = tokens_in;
        self.tokens_out = tokens_out;
        self
    }
}

/// Input values and flags for a pipeline run.
#[derive(Clone, Debug, Default)]
pub struct PipelineInput {
    /// The `/add-feature` prompt.
    pub prompt: Option<String>,
    /// Step IDs explicitly skipped for this run.
    pub skip: std::collections::BTreeSet<String>,
    /// Step IDs explicitly re-enabled despite `default_skip: true`.
    pub unskip: std::collections::BTreeSet<String>,
    /// Halt after the `tasks` step.
    pub dry_run: bool,
    /// Override run id.
    pub run_id: Option<String>,
    /// Skip the GitHub Issues creation offer even if `gh` is available.
    pub no_github_issues: bool,
}

/// Result returned after a pipeline run.
#[derive(Clone, Debug)]
pub struct RunOutcome {
    /// Run identifier.
    pub run_id: String,
    /// Final run status.
    pub status: RunStatus,
    /// Feature directory after `specify` completes.
    pub feature_dir: Option<PathBuf>,
    /// Per-step records.
    pub steps: Vec<StepRecord>,
    /// Total input tokens consumed by model calls in this run.
    pub tokens_in: u64,
    /// Total output tokens produced by model calls in this run.
    pub tokens_out: u64,
}

impl RunOutcome {
    /// Estimate USD cost for model-backed steps, using the built-in pricing table.
    /// Returns `None` if the model name is unknown.
    pub fn cost_estimate_usd(&self, model_name: &str) -> Option<f64> {
        derrick_models::builtin_cost_hint(model_name)
            .map(|hint| hint.estimate_usd(self.tokens_in, self.tokens_out))
    }
}

/// Final run status.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// All required steps completed.
    Success,
    /// A step failed.
    Failed,
    /// The run intentionally halted.
    Halted,
}

/// One step's execution record.
#[derive(Clone, Debug)]
pub struct StepRecord {
    /// Step identifier.
    pub id: String,
    /// Final step status.
    pub status: StepStatus,
    /// Start timestamp.
    pub started_at: DateTime<Utc>,
    /// Finish timestamp.
    pub finished_at: DateTime<Utc>,
    /// Step log path.
    pub log_path: PathBuf,
    /// Artifacts observed after this step.
    pub artifacts: Vec<PathBuf>,
    /// Input tokens consumed by model calls in this step (0 for non-model steps).
    pub tokens_in: u32,
    /// Output tokens produced by model calls in this step (0 for non-model steps).
    pub tokens_out: u32,
}

/// Per-step status.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    /// Step was skipped.
    Skipped,
    /// Step completed successfully.
    Success,
    /// Step failed.
    Failed,
    /// Step intentionally halted the run.
    Halted,
}

/// Errors returned by the runner.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum RunError {
    /// Pipeline id is unknown.
    #[error("unknown pipeline: {0}")]
    UnknownPipeline(String),
    /// Required prompt is absent.
    #[error("missing prompt for pipeline {0}")]
    MissingPrompt(String),
    /// A step failed.
    #[error("step {id} failed: {message}")]
    StepFailed {
        /// Step identifier.
        id: String,
        /// Failure message.
        message: String,
    },
    /// Substrate operation failed.
    #[error("substrate error: {0}")]
    Substrate(#[from] SubstrateError),
    /// Host adapter failed.
    #[error("host error: {0}")]
    Host(#[from] HostError),
    /// Model provider failed.
    #[error("model error: {0}")]
    Model(#[from] ModelError),
    /// Filesystem operation failed.
    #[error("io error at {path}: {source}")]
    Io {
        /// Path involved in the operation.
        path: PathBuf,
        /// Underlying source.
        source: std::io::Error,
    },
    /// JSON operation failed.
    #[error("json error at {path}: {source}")]
    Json {
        /// Path involved in the operation.
        path: PathBuf,
        /// Underlying source.
        source: serde_json::Error,
    },
    /// Configuration is unsupported.
    #[error("config error: {0}")]
    Config(String),
}
