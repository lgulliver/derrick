//! Concrete host CLI adapters.

mod aider;
mod claude;
mod codex;
mod copilot;
mod opencode;

pub use aider::AiderHost;
pub use claude::ClaudeHost;
pub use codex::CodexHost;
pub use copilot::CopilotHost;
pub use opencode::OpencodeHost;
