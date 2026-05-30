//! UI-free progress reporting for pipeline runs.
//!
//! The [`Runner`](crate::Runner) calls a [`ProgressReporter`] at each step
//! boundary so a front-end (the CLI) can render live feedback — a spinner,
//! elapsed time, per-step outcomes — without the orchestrator owning any
//! terminal I/O. Non-interactive callers and tests use [`NoopReporter`], the
//! default, which renders nothing.
//!
//! The reporter trait is deliberately free of any UI dependency: it carries
//! plain data (step ids, statuses, token counts, durations) and leaves all
//! rendering to the implementor. This keeps `derrick-flow` independent of
//! `indicatif`/`crossterm` and keeps the pipeline orchestrator testable.

use std::time::Duration;

use crate::{RunStatus, StepStatus};

/// Outcome of a single step, handed to [`ProgressReporter::step_finished`].
#[derive(Clone, Copy, Debug)]
pub struct StepProgress<'a> {
    /// Step identifier (`specify`, `plan`, …).
    pub step_id: &'a str,
    /// Terminal status of the step.
    pub status: StepStatus,
    /// Input tokens attributed to the step.
    pub tokens_in: u32,
    /// Output tokens attributed to the step.
    pub tokens_out: u32,
    /// Wall-clock time the step took. Zero for skipped steps.
    pub elapsed: Duration,
}

/// Final run summary, handed to [`ProgressReporter::pipeline_finished`].
#[derive(Clone, Copy, Debug)]
pub struct RunProgress<'a> {
    /// The run identifier.
    pub run_id: &'a str,
    /// Final status of the run.
    pub status: RunStatus,
    /// Total input tokens across all steps.
    pub tokens_in: u64,
    /// Total output tokens across all steps.
    pub tokens_out: u64,
    /// Total wall-clock time for the run.
    pub elapsed: Duration,
}

/// Live progress callbacks emitted as a pipeline executes.
///
/// All methods default to no-ops; implementors override only what they render.
/// Methods may be invoked from concurrent tasks (parallel step groups), so
/// implementations must be `Send + Sync` and tolerate interleaved calls.
pub trait ProgressReporter: Send + Sync {
    /// The run is starting. `total_steps` is the number of steps that will be
    /// attempted after any resumed prefix.
    fn pipeline_started(&self, pipeline_id: &str, run_id: &str, total_steps: usize) {
        let _ = (pipeline_id, run_id, total_steps);
    }

    /// A step is about to run. `index` is 1-based within the attempted tail
    /// (`0` when the position is not meaningful, e.g. inside a parallel group).
    /// `interactive` steps read from stdin, so reporters must not animate a
    /// spinner over them.
    fn step_started(&self, step_id: &str, index: usize, total: usize, interactive: bool) {
        let _ = (step_id, index, total, interactive);
    }

    /// A line of live output from the running step's agent subprocess
    /// (run-feedback Layer 2). Called once per complete stdout/stderr line while
    /// the step runs; high-frequency, so implementations must be cheap and must
    /// not block. Default is a no-op.
    fn step_output(&self, step_id: &str, line: &str) {
        let _ = (step_id, line);
    }

    /// A step finished — success, skip, halt, or failure.
    fn step_finished(&self, progress: StepProgress<'_>) {
        let _ = progress;
    }

    /// The run finished.
    fn pipeline_finished(&self, progress: RunProgress<'_>) {
        let _ = progress;
    }
}

/// A reporter that renders nothing — the default for non-interactive callers
/// and tests.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopReporter;

impl ProgressReporter for NoopReporter {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A reporter that records the lifecycle calls it receives, for assertions.
    #[derive(Default)]
    struct RecordingReporter {
        events: Mutex<Vec<String>>,
    }

    impl ProgressReporter for RecordingReporter {
        fn pipeline_started(&self, pipeline_id: &str, _run_id: &str, total_steps: usize) {
            self.events
                .lock()
                .unwrap()
                .push(format!("start:{pipeline_id}:{total_steps}"));
        }
        fn step_started(&self, step_id: &str, index: usize, total: usize, _interactive: bool) {
            self.events
                .lock()
                .unwrap()
                .push(format!("step_started:{step_id}:{index}/{total}"));
        }
        fn step_finished(&self, progress: StepProgress<'_>) {
            self.events
                .lock()
                .unwrap()
                .push(format!("step_finished:{}", progress.step_id));
        }
        fn pipeline_finished(&self, progress: RunProgress<'_>) {
            self.events
                .lock()
                .unwrap()
                .push(format!("finished:{:?}", progress.status));
        }
    }

    #[test]
    fn noop_reporter_is_silent() {
        let reporter = NoopReporter;
        // None of these should panic or do anything observable.
        reporter.pipeline_started("add-feature", "run-1", 3);
        reporter.step_started("specify", 1, 3, false);
        reporter.step_finished(StepProgress {
            step_id: "specify",
            status: StepStatus::Success,
            tokens_in: 10,
            tokens_out: 5,
            elapsed: Duration::from_secs(1),
        });
        reporter.pipeline_finished(RunProgress {
            run_id: "run-1",
            status: RunStatus::Success,
            tokens_in: 10,
            tokens_out: 5,
            elapsed: Duration::from_secs(2),
        });
    }

    #[test]
    fn recording_reporter_captures_lifecycle() {
        let reporter = RecordingReporter::default();
        reporter.pipeline_started("add-feature", "run-1", 2);
        reporter.step_started("specify", 1, 2, false);
        reporter.step_finished(StepProgress {
            step_id: "specify",
            status: StepStatus::Success,
            tokens_in: 0,
            tokens_out: 0,
            elapsed: Duration::ZERO,
        });
        reporter.pipeline_finished(RunProgress {
            run_id: "run-1",
            status: RunStatus::Success,
            tokens_in: 0,
            tokens_out: 0,
            elapsed: Duration::ZERO,
        });
        let events = reporter.events.lock().unwrap();
        assert_eq!(
            *events,
            vec![
                "start:add-feature:2",
                "step_started:specify:1/2",
                "step_finished:specify",
                "finished:Success",
            ]
        );
    }
}
