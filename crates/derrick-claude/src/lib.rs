//! `derrick-claude` — Claude Code hand dispatcher. See DESIGN.md §8.2 and
//! ticket T015.
//!
//! The dispatcher renders a self-contained queue file for each ticket and
//! either waits for an operator to run `claude --print < <file>` (the default
//! interactive flow) or spawns the process itself when `auto_dispatch` is on.
//! The queue file's final instruction tells Claude to call
//! `derrick ticket review`, which transitions the ticket to `InReview` and
//! hands it back to the foreman's verifier.

#![deny(clippy::print_stdout, clippy::print_stderr)]

mod dispatcher;
mod prompt;

pub use dispatcher::{ClaudeHandDispatcher, ClaudeHandDispatcherConfig};
pub use prompt::render_queue_file;
