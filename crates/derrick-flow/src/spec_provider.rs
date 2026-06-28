//! Spec-provider seam (DESIGN.md §5.3).
//!
//! The `specify`, `plan`, and `tasks` pipeline steps can be declared two ways:
//!
//!   * **Explicit** — `host: claude` + `command: "/speckit.specify {{prompt}}"`.
//!     These steps name their host and command and are dispatched directly by
//!     [`crate::steps::execute_step`]; they never reach this module.
//!   * **Bare** — `id: specify` with no `host`/`command`/`runner`. These steps
//!     route here, and the provider selected by `tools.specify.provider`
//!     decides how the artifact is produced.
//!
//! All three arms are wired:
//!
//!   * [`SpecProviderKind::Speckit`] delegates to the existing host path so the
//!     speckit behaviour (pre-scaffold, spec verification, artifact detection) is
//!     identical to the explicit-step path.
//!   * [`SpecProviderKind::Native`] calls [`derrick_specify::NativeSpecProvider`]
//!     for each phase ([`run_native_phase`]).
//!   * [`SpecProviderKind::Import`] normalizes an external spec via
//!     [`crate::import::import_specify`] for the Specify phase, then routes
//!     `plan`/`tasks` per `tools.specify.import.{plan,tasks}`
//!     ([`run_import_phase`]) to the native, speckit, or import path. Import
//!     never silently skips plan/tasks.
//!
//! Implementor: [`run_spec_phase`]. Tested in `crates/derrick-flow/src/lib.rs`
//! (`spec_provider_seam` tests) against a stub [`HostRegistry`].

use std::path::Path;

use derrick_config::{DownstreamMode, PipelineStep, SpecProviderKind};
use derrick_specify::{NativeOutcome, NativeRequest, NativeSpecProvider};
use derrick_tools::{HostRegistry, OutputSink};

use crate::import::{ImportSpecifyRequest, import_specify};

use derrick_assay::ExecutionState;
use derrick_assay::types::{RunError, StepExecution};

/// The three spec-authoring phases dispatched through the seam.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SpecPhase {
    /// Produce `spec.md` (+ `.specify/feature.json`).
    Specify,
    /// Produce `plan.md`.
    Plan,
    /// Produce `tasks.md`.
    Tasks,
}

impl SpecPhase {
    /// Maps a bare step id to its phase. Returns `None` for any other id.
    pub fn from_step_id(id: &str) -> Option<Self> {
        match id {
            "specify" => Some(Self::Specify),
            "plan" => Some(Self::Plan),
            "tasks" => Some(Self::Tasks),
            _ => None,
        }
    }

    /// The canonical speckit host command for this phase, matching the explicit
    /// drill steps byte-for-byte. Only `specify` interpolates `{{prompt}}`.
    fn speckit_command(self) -> &'static str {
        match self {
            Self::Specify => "/speckit.specify {{prompt}}",
            Self::Plan => "/speckit.plan",
            Self::Tasks => "/speckit.tasks",
        }
    }

    /// The provider-agnostic name used in error messages.
    fn label(self) -> &'static str {
        match self {
            Self::Specify => "specify",
            Self::Plan => "plan",
            Self::Tasks => "tasks",
        }
    }
}

/// Everything [`run_spec_phase`] needs to execute one spec phase. Mirrors the
/// parameters [`crate::steps::execute_role_step`] already takes, so the speckit
/// arm can hand straight through to it.
pub struct SpecPhaseCtx<'a> {
    /// The effective configuration.
    pub config: &'a derrick_config::Config,
    /// The registered host adapters.
    pub hosts: &'a HostRegistry,
    /// The repository root.
    pub repo_root: &'a Path,
    /// Mutable execution state (carries `feature_dir`, prompt, run id).
    pub state: &'a mut ExecutionState,
    /// The original bare pipeline step (its `id` selects the phase).
    pub step: &'a PipelineStep,
    /// Where this step's log is written.
    pub log_path: &'a Path,
    /// Optional sink for streaming host output.
    pub output_sink: Option<OutputSink>,
}

/// Dispatches one spec phase through the selected provider.
///
/// * [`SpecProviderKind::Speckit`] delegates to
///   [`crate::steps::execute_role_step`] with the canonical speckit host
///   (`claude`) and command, so the produced artifacts and `feature.json` are
///   identical to the explicit-step path.
/// * [`SpecProviderKind::Native`] calls [`NativeSpecProvider`] for the phase.
/// * [`SpecProviderKind::Import`] imports/normalizes the spec, then routes
///   `plan`/`tasks` per `tools.specify.import.{plan,tasks}`.
pub async fn run_spec_phase(
    provider: SpecProviderKind,
    phase: SpecPhase,
    ctx: SpecPhaseCtx<'_>,
) -> Result<StepExecution, RunError> {
    match provider {
        SpecProviderKind::Speckit => speckit_phase(phase, ctx).await,
        SpecProviderKind::Native => run_native_phase(phase, ctx).await,
        SpecProviderKind::Import => run_import_phase(phase, ctx).await,
    }
}

/// The speckit host path for one phase (shared by the speckit provider and the
/// import provider's `plan`/`tasks` `speckit` downstream mode).
async fn speckit_phase(phase: SpecPhase, ctx: SpecPhaseCtx<'_>) -> Result<StepExecution, RunError> {
    crate::steps::execute_role_step(
        ctx.config,
        ctx.hosts,
        ctx.repo_root,
        ctx.step,
        ctx.state,
        ctx.log_path,
        ctx.output_sink,
        Some("claude"),
        Some(phase.speckit_command()),
    )
    .await
}

/// Dispatches one import-provider phase.
///
/// * `Specify` → [`import_specify`]: resolve source, scaffold, passthrough or
///   normalize, verify.
/// * `Plan` / `Tasks` → routed per `tools.specify.import.{plan,tasks}`:
///   - [`DownstreamMode::Native`] → the native planner/tasker against the
///     imported spec (identical to the native path).
///   - [`DownstreamMode::Speckit`] → the host-delegated `/speckit.{plan,tasks}`.
///   - [`DownstreamMode::Import`] → read the artifact from the source too (the
///     source must then be a directory/multi-doc; a single-file source is a
///     clear error for v1). Import never silently skips plan/tasks.
async fn run_import_phase(
    phase: SpecPhase,
    ctx: SpecPhaseCtx<'_>,
) -> Result<StepExecution, RunError> {
    match phase {
        SpecPhase::Specify => run_import_specify_phase(ctx).await,
        SpecPhase::Plan => {
            let mode = ctx.config.tools().specify().import().plan();
            run_import_downstream(phase, mode, ctx).await
        }
        SpecPhase::Tasks => {
            let mode = ctx.config.tools().specify().import().tasks();
            run_import_downstream(phase, mode, ctx).await
        }
    }
}

/// The import Specify phase: import/normalize the source into `spec.md`, set
/// `feature_dir`, and account for it as a [`StepExecution`] matching the role
/// path (artifacts re-detected from canonical paths).
async fn run_import_specify_phase(ctx: SpecPhaseCtx<'_>) -> Result<StepExecution, RunError> {
    let source = ctx
        .config
        .tools()
        .specify()
        .import()
        .source()
        .ok_or_else(|| {
            RunError::Config(
                "spec provider 'import' requires a source: set tools.specify.import.source \
                 or pass --spec <path>"
                    .to_owned(),
            )
        })?
        .to_owned();

    let wd = working_dir(ctx.state, ctx.repo_root).to_path_buf();
    let raw_prompt = ctx.state.prompt.clone();
    let request = ImportSpecifyRequest {
        config: ctx.config,
        hosts: ctx.hosts,
        repo_root: ctx.repo_root,
        working_dir: &wd,
        raw_prompt: &raw_prompt,
        source: &source,
    };
    let imported = import_specify(&request).await?;
    ctx.state.feature_dir = Some(imported.feature_dir.clone());

    derrick_assay::io::write_log(
        ctx.log_path,
        &format!(
            "import specify produced {} artifact(s) from {source} (passthrough={}); \
             tokens_in={} tokens_out={}\n",
            imported.outcome.artifacts.len(),
            imported.passthrough,
            imported.outcome.tokens_in,
            imported.outcome.tokens_out
        ),
        "",
    )?;

    let artifacts = crate::steps::detect_artifacts("specify", ctx.state, ctx.repo_root);
    Ok(StepExecution::success(artifacts)
        .with_tokens(imported.outcome.tokens_in, imported.outcome.tokens_out)
        .with_compression(imported.outcome.bytes_raw, imported.outcome.bytes_saved)
        .with_roughneck(imported.outcome.roughneck_tokens_saved))
}

/// Routes the import provider's `plan`/`tasks` phase per its [`DownstreamMode`].
async fn run_import_downstream(
    phase: SpecPhase,
    mode: DownstreamMode,
    ctx: SpecPhaseCtx<'_>,
) -> Result<StepExecution, RunError> {
    match mode {
        DownstreamMode::Native => run_native_phase(phase, ctx).await,
        DownstreamMode::Speckit => speckit_phase(phase, ctx).await,
        DownstreamMode::Import => run_import_downstream_from_source(phase, ctx).await,
    }
}

/// `import`-mode downstream `plan`/`tasks`: read the artifact from the source.
///
/// For v1 the import source is a single file (the spec), so there is no separate
/// plan/tasks document to import. Selecting `import` mode for `plan`/`tasks` with
/// a single-file source is a clear configuration error rather than a silent skip
/// — multi-doc/directory sources are a documented, deferred capability.
async fn run_import_downstream_from_source(
    phase: SpecPhase,
    ctx: SpecPhaseCtx<'_>,
) -> Result<StepExecution, RunError> {
    let source = ctx
        .config
        .tools()
        .specify()
        .import()
        .source()
        .unwrap_or("<unset>");
    let wd = working_dir(ctx.state, ctx.repo_root);
    let is_dir = ctx
        .config
        .tools()
        .specify()
        .import()
        .source()
        .map(|s| wd.join(s).is_dir())
        .unwrap_or(false);
    if !is_dir {
        return Err(RunError::Config(format!(
            "tools.specify.import.{phase} is set to 'import', but the import source {source:?} \
             is a single file, not a directory/multi-doc source — there is no separate \
             {phase} document to import. Use 'native' or 'speckit' for the {phase} phase, \
             or point the source at a directory containing {phase}.md. (Multi-doc import \
             sources are a documented, deferred capability for v1.)",
            phase = phase.label(),
        )));
    }
    // A directory source: import the named artifact verbatim (parallel to the
    // Specify passthrough). Deferred for v1 — surfaced as a clear error rather
    // than a silent skip.
    Err(RunError::Config(format!(
        "importing {phase}.md from a directory source is not implemented in v1; \
         use 'native' or 'speckit' for the {phase} phase",
        phase = phase.label(),
    )))
}

/// Resolves the working directory the same way `crate::steps` does: the
/// worktree path when present, otherwise the repo root.
fn working_dir<'a>(state: &'a ExecutionState, repo_root: &'a Path) -> &'a Path {
    state.worktree_path.as_deref().unwrap_or(repo_root)
}

/// Runs one native spec phase via [`NativeSpecProvider`] and maps its
/// [`NativeOutcome`] onto a [`StepExecution`], matching the role-path's
/// accounting (artifacts via `detect_artifacts`, tokens/bytes/roughneck).
async fn run_native_phase(
    phase: SpecPhase,
    ctx: SpecPhaseCtx<'_>,
) -> Result<StepExecution, RunError> {
    let provider = NativeSpecProvider::new();
    // Pipeline steps run headless (no TTY to answer clarify questions), so the
    // native clarify loop auto-accepts each recommendation and still writes
    // `clarify.md`. The interactive clarify path is the dedicated `clarify`
    // step (`crate::clarify::execute_clarify`).
    let interactive = false;

    // The specify phase pre-scaffolds the feature dir (reusing the same
    // assay io used by the speckit path) and records it on the state, so
    // detect_artifacts and the later plan/tasks phases see it.
    if phase == SpecPhase::Specify {
        let wd = working_dir(ctx.state, ctx.repo_root);
        let feature_dir = derrick_assay::io::prescaffold_feature_dir(wd, &ctx.state.prompt)?;
        ctx.state.feature_dir = Some(feature_dir);
    }
    let feature_dir = ctx.state.feature_dir.clone().ok_or_else(|| {
        RunError::Config(format!(
            "native '{}' requires feature_dir from a prior specify phase",
            phase.label()
        ))
    })?;

    // Borrow split: capture the working dir as an owned path so `ctx.state`
    // is free for the (non-specify) clarify read below.
    let wd = working_dir(ctx.state, ctx.repo_root).to_path_buf();
    let raw_prompt = ctx.state.prompt.clone();

    let request = NativeRequest {
        raw_prompt: &raw_prompt,
        repo_root: ctx.repo_root,
        working_dir: &wd,
        hosts: ctx.hosts,
        config: ctx.config,
        interactive,
        feature_dir: &feature_dir,
    };

    let outcome: NativeOutcome = match phase {
        SpecPhase::Specify => provider
            .specify(&request)
            .await
            .map_err(map_specify_err("specify"))?,
        SpecPhase::Plan => {
            // Thread accepted clarifications into the native planner, mirroring
            // `inject_clarify_answers_for_plan` on the role path. A missing
            // clarify.md is fine (None); any other IO error is surfaced rather
            // than silently swallowed.
            let clarify_path = wd.join(&feature_dir).join("clarify.md");
            let clarifications = match std::fs::read_to_string(&clarify_path) {
                Ok(text) => Some(text),
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => None,
                Err(source) => {
                    return Err(RunError::Io {
                        path: clarify_path,
                        source,
                    });
                }
            };
            provider
                .plan(&request, clarifications.as_deref())
                .await
                .map_err(map_specify_err("plan"))?
        }
        SpecPhase::Tasks => provider
            .tasks(&request)
            .await
            .map_err(map_specify_err("tasks"))?,
    };

    // Write the step log for parity with the role path.
    derrick_assay::io::write_log(
        ctx.log_path,
        &format!(
            "native {} produced {} artifact(s); tokens_in={} tokens_out={} repaired={}\n",
            phase.label(),
            outcome.artifacts.len(),
            outcome.tokens_in,
            outcome.tokens_out,
            outcome.repaired
        ),
        "",
    )?;

    // Artifacts: re-detect from canonical paths so downstream keys off the same
    // files the speckit path produces.
    let artifacts = crate::steps::detect_artifacts(
        match phase {
            SpecPhase::Specify => "specify",
            SpecPhase::Plan => "plan",
            SpecPhase::Tasks => "tasks",
        },
        ctx.state,
        ctx.repo_root,
    );

    Ok(StepExecution::success(artifacts)
        .with_tokens(outcome.tokens_in, outcome.tokens_out)
        .with_compression(outcome.bytes_raw, outcome.bytes_saved)
        .with_roughneck(outcome.roughneck_tokens_saved))
}

/// Maps a [`derrick_specify::SpecifyError`] into a [`RunError::StepFailed`],
/// matching `verify_spec_written` failure semantics.
fn map_specify_err(phase: &'static str) -> impl Fn(derrick_specify::SpecifyError) -> RunError {
    move |error| RunError::StepFailed {
        id: phase.to_owned(),
        message: error.to_string(),
    }
}
