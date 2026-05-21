//! Display names for host adapters and step runners.

use derrick_config::{Host, Runner as StepRunner};

pub fn host_name(host: Host) -> &'static str {
    match host {
        Host::Claude => "claude",
        Host::Codex => "codex",
        Host::Copilot => "copilot",
    }
}

pub fn runner_name(runner: StepRunner) -> &'static str {
    match runner {
        StepRunner::Derrick => "derrick",
        StepRunner::Human => "human",
        StepRunner::Bash => "bash",
        StepRunner::Claude => "claude",
        StepRunner::Codex => "codex",
        StepRunner::Copilot => "copilot",
    }
}
