//! derrick-copilot — `HandDispatcher` implementation for the GitHub Copilot
//! coding agent.
//!
//! This crate fulfils ticket T013. It supersedes `CopilotStubDispatcher` from
//! `derrick-substrate-native` with [`CopilotHandDispatcher`], which:
//!
//! 1. Creates and pushes a `derrick/<batch>/<ticket-id>` branch.
//! 2. Files a GitHub issue tagged for Copilot via `gh issue create` and
//!    assigns Copilot to it via `gh issue assign ... --assignee @copilot`
//!    (the stable surface that avoided the 401 errors we saw on the Copilot
//!    pull-request MCP endpoint).
//! 3. Registers a fresh hand and atomically transitions the ticket
//!    `Ready → InFlight` via [`Substrate::assign_to_hand`].
//! 4. Spawns a background tokio task that polls `gh pr list --head <branch>`
//!    on the configured interval; the task heartbeats each iteration and
//!    transitions the ticket to `InReview` via
//!    [`Substrate::transition_to_in_review`] when a PR appears.
//!
//! Dispatch is asynchronous: `dispatch()` returns `completed_synchronously:
//! false` once the hand is registered. If the host process exits before the
//! poll task completes, the hand row's `last_seen` stops being updated and
//! the foreman's cleanup pass releases the ticket on its next tick.

#![allow(clippy::module_name_repetitions)]

mod branch;
mod client;
mod dispatcher;
mod local_dispatcher;

pub use branch::{BranchCreator, BranchError, GitBranchCreator};
pub use client::{CopilotDispatchClient, CopilotDispatchError, GhCopilotClient, PrInfo, TaskId};
pub use dispatcher::{CopilotHandDispatcher, CopilotHandDispatcherConfig};
pub use local_dispatcher::{LocalCopilotHandDispatcher, LocalCopilotHandDispatcherConfig};
