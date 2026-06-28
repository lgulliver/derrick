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
//! Phase 1 wires only the [`SpecProviderKind::Speckit`] arm, which delegates to
//! the existing host path so the speckit behaviour (pre-scaffold, spec
//! verification, artifact detection) is identical to the explicit-step path.
//! The `Native` and `Import` arms are config-accepted but return a clear "not
//! yet available" error until Phases 2/3.
//!
//! Implementor: [`run_spec_phase`]. Tested in `crates/derrick-flow/src/lib.rs`
//! (`spec_provider_seam` tests) against a stub [`HostRegistry`].

use std::path::Path;

use derrick_config::{PipelineStep, SpecProviderKind};
use derrick_tools::{HostRegistry, OutputSink};

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
/// * [`SpecProviderKind::Native`] / [`SpecProviderKind::Import`] are stubs that
///   return a "not yet available" [`RunError::Config`] until Phases 2/3.
pub async fn run_spec_phase(
    provider: SpecProviderKind,
    phase: SpecPhase,
    ctx: SpecPhaseCtx<'_>,
) -> Result<StepExecution, RunError> {
    match provider {
        SpecProviderKind::Speckit => {
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
        SpecProviderKind::Native => Err(RunError::Config(format!(
            "spec provider 'native' is not yet available (Phase 2) — \
             phase '{}' cannot be produced natively yet; \
             use an explicit speckit step or set tools.specify.provider: speckit",
            phase.label()
        ))),
        SpecProviderKind::Import => Err(RunError::Config(format!(
            "spec provider 'import' is not yet available (Phase 3) — \
             phase '{}' cannot be imported yet; \
             use an explicit speckit step or set tools.specify.provider: speckit",
            phase.label()
        ))),
    }
}
