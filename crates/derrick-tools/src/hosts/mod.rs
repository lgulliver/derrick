//! Concrete host CLI adapters.

mod claude;
mod codex;
mod copilot;
mod opencode;

pub use claude::ClaudeHost;
pub use codex::CodexHost;
pub use copilot::CopilotHost;
pub use opencode::OpencodeHost;
