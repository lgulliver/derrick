//! derrick-tui — `derrick observe` interactive dashboard. See DESIGN.md §5.7.
//!
//! This crate is the rendering and event-loop library. It depends only on
//! the `Substrate` trait and pulls all data through that contract; the
//! concrete substrate construction lives in `derrick-observe`.

#![deny(clippy::unwrap_used, clippy::expect_used)]

pub mod app;
pub mod data;
pub mod event_loop;
pub mod tabs;

pub use app::{App, FilterState};
pub use data::{
    DataModel, EventRow, ForemanStatusSnapshot, LastAssaySnapshot, MemoryEntry, OverviewData,
    ParseTabError, StackNode, StackSummary, Tab, TicketRow, TokenSummary,
};
pub use event_loop::{install_panic_hook, run_event_loop};
