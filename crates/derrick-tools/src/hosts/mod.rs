//! Concrete host CLI adapters.

mod claude;
mod codex;
mod copilot;

pub use claude::ClaudeHost;
pub use codex::CodexHost;
pub use copilot::CopilotHost;
