//! UI state machine for the dashboard.
//!
//! `App` is the in-memory state held by the event loop. It owns the latest
//! `DataModel` snapshot plus all transient UI state (active tab, scroll
//! offset, filter, detail pane open/closed). The event loop renders from
//! `App` and feeds key events into [`App::handle_key`].

use crossterm::event::KeyCode;

use crate::data::{DataModel, Tab};

// ---------------------------------------------------------------------------
// Ticket sort
// ---------------------------------------------------------------------------

/// Sort order for the Tickets tab table. Cycles with the `s` key.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TicketSort {
    /// Most-recently-updated first. Default — mirrors the substrate query
    /// order so the view is stable without any extra sorting overhead.
    #[default]
    Updated,
    /// Lifecycle state in operational priority order:
    /// in-flight → in-review → ready → blocked → done → rejected.
    State,
    /// Lexicographic ticket id with numeric-suffix awareness
    /// (`tst-2` < `tst-10`).
    Id,
    /// Case-insensitive title.
    Title,
}

impl TicketSort {
    /// Returns the next sort in the cycle.
    pub fn cycle(self) -> Self {
        match self {
            Self::Updated => Self::State,
            Self::State => Self::Id,
            Self::Id => Self::Title,
            Self::Title => Self::Updated,
        }
    }

    /// Short label shown in the Tickets block title.
    pub fn label(self) -> &'static str {
        match self {
            Self::Updated => "updated",
            Self::State => "state",
            Self::Id => "id",
            Self::Title => "title",
        }
    }
}

/// Search filter state. `Inactive` means the filter bar is not focused; the
/// stored string is the live query buffer when `Active`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum FilterState {
    /// Filter bar is hidden / inactive.
    #[default]
    Inactive,
    /// Filter bar is focused and accepting input.
    Active(String),
}

impl FilterState {
    /// Returns the current query string, regardless of activation state.
    pub fn query(&self) -> &str {
        match self {
            Self::Inactive => "",
            Self::Active(q) => q.as_str(),
        }
    }

    /// `true` when the filter bar is focused.
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Active(_))
    }
}

/// In-memory UI state. Cloned cheaply for tests, but held by reference in
/// the event loop.
#[derive(Clone, Debug)]
pub struct App {
    /// Currently displayed tab.
    pub active_tab: Tab,
    /// Top-of-pane scroll offset for tabular tabs.
    pub scroll_offset: usize,
    /// Selected row index inside the active tab.
    pub selected_row: usize,
    /// Search filter state for the active tab.
    pub filter: FilterState,
    /// Active sort order for the Tickets tab. Cycled with `s`.
    pub ticket_sort: TicketSort,
    /// `true` when a detail/preview pane is open over the active tab.
    pub detail_open: bool,
    /// Latest data snapshot pulled from the substrate.
    pub data: DataModel,
    /// Set by the `q` keypress to signal the event loop to exit.
    pub quit: bool,
    /// `true` when the help overlay is showing.
    pub show_help: bool,
    /// Set by the `r` keypress to request a refresh on the next tick.
    pub refresh_requested: bool,
    /// `true` when the user has scrolled away from the bottom of Activity;
    /// the auto-scroll behaviour pauses until the user returns to bottom.
    pub activity_auto_scroll: bool,
    /// PR URL to open in the system browser on the next event loop iteration.
    pub pending_open_url: Option<String>,
    /// Memory slug to append to the prune queue on the next event loop iteration.
    pub pending_prune_slug: Option<String>,
    /// Monotonic animation frame counter incremented by the ~100 ms animation
    /// tick (D78). Used only by the Factory tab for per-worker motion; the
    /// substrate is still polled at 1 Hz / on `notify` fs events — this counter
    /// drives purely local animation state.
    pub animation_frame: u64,
}

impl App {
    /// Construct an `App` rooted at `initial_tab` with the given data.
    pub fn new(initial_tab: Tab, data: DataModel) -> Self {
        Self {
            active_tab: initial_tab,
            scroll_offset: 0,
            selected_row: 0,
            filter: FilterState::Inactive,
            ticket_sort: TicketSort::default(),
            detail_open: false,
            data,
            quit: false,
            show_help: false,
            refresh_requested: false,
            activity_auto_scroll: true,
            pending_open_url: None,
            pending_prune_slug: None,
            animation_frame: 0,
        }
    }

    /// Replace the data snapshot. Resets the activity scroll to bottom when
    /// the user is currently at the bottom (auto-scroll mode).
    pub fn set_data(&mut self, data: DataModel) {
        self.data = data;
    }

    /// Dispatch a key event to the right handler. Returns nothing; the
    /// effects are visible on `self`.
    pub fn handle_key(&mut self, key: KeyCode) {
        if self.filter.is_active() {
            self.handle_filter_key(key);
            return;
        }

        if self.show_help {
            // Any key dismisses the help overlay.
            self.show_help = false;
            return;
        }

        match key {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('r') => self.refresh_requested = true,
            KeyCode::Char('?') => self.show_help = !self.show_help,
            KeyCode::Char('/') => self.filter = FilterState::Active(String::new()),
            KeyCode::Esc => {
                if self.detail_open {
                    self.detail_open = false;
                } else if !matches!(self.filter, FilterState::Inactive) {
                    self.filter = FilterState::Inactive;
                }
            }
            KeyCode::Enter => match self.active_tab {
                Tab::Stack => {
                    if let Some(node) = self.data.stack_nodes.get(self.selected_row) {
                        if let Some(url) = &node.pr_url {
                            self.pending_open_url = Some(url.clone());
                        }
                    }
                }
                _ => {
                    self.detail_open = !self.detail_open;
                }
            },
            KeyCode::Up => {
                if self.selected_row > 0 {
                    self.selected_row -= 1;
                }
                if self.active_tab == Tab::Activity {
                    self.activity_auto_scroll = false;
                }
            }
            KeyCode::Down => {
                self.selected_row = self.selected_row.saturating_add(1);
                if self.active_tab == Tab::Activity {
                    // Returning to the bottom re-enables auto-scroll. The
                    // renderer is responsible for clamping the index.
                    self.activity_auto_scroll = true;
                }
            }
            KeyCode::Char('d') if self.active_tab == Tab::Memory => {
                if let Some(entry) = self.data.memory_entries.get(self.selected_row) {
                    self.pending_prune_slug = Some(entry.slug.clone());
                }
            }
            // `s` cycles the sort order on the Tickets tab; resets the
            // selected-row index so it doesn't point into a stale position.
            KeyCode::Char('s') if self.active_tab == Tab::Tickets => {
                self.ticket_sort = self.ticket_sort.cycle();
                self.selected_row = 0;
            }
            KeyCode::Char(c @ '1'..='8') => {
                let idx = (c as u8 - b'1') as usize;
                if let Some(tab) = Tab::from_index(idx) {
                    self.active_tab = tab;
                    self.selected_row = 0;
                    self.scroll_offset = 0;
                    self.detail_open = false;
                }
            }
            _ => {}
        }
    }

    fn handle_filter_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Esc => self.filter = FilterState::Inactive,
            KeyCode::Enter => {
                // Commit: deactivate but keep the query string applied. We
                // store the committed query back in the inactive variant by
                // emptying the active buffer; the renderer reads `query()`.
                if let FilterState::Active(q) = &self.filter {
                    let committed = q.clone();
                    self.filter = if committed.is_empty() {
                        FilterState::Inactive
                    } else {
                        FilterState::Active(committed)
                    };
                }
            }
            KeyCode::Backspace => {
                if let FilterState::Active(q) = &mut self.filter {
                    q.pop();
                }
            }
            KeyCode::Char(c) => {
                if let FilterState::Active(q) = &mut self.filter {
                    q.push(c);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{DataModel, MemoryEntry, StackNode};

    fn app() -> App {
        App::new(Tab::Overview, DataModel::empty())
    }

    #[test]
    fn key_q_sets_quit() {
        let mut a = app();
        a.handle_key(KeyCode::Char('q'));
        assert!(a.quit);
    }

    #[test]
    fn key_r_requests_refresh() {
        let mut a = app();
        a.handle_key(KeyCode::Char('r'));
        assert!(a.refresh_requested);
    }

    #[test]
    fn keys_1_through_6_switch_tabs() {
        let mut a = app();
        a.handle_key(KeyCode::Char('2'));
        assert_eq!(a.active_tab, Tab::Tickets);
        a.handle_key(KeyCode::Char('4'));
        assert_eq!(a.active_tab, Tab::Activity);
        a.handle_key(KeyCode::Char('6'));
        assert_eq!(a.active_tab, Tab::Memory);
        a.handle_key(KeyCode::Char('1'));
        assert_eq!(a.active_tab, Tab::Overview);
    }

    #[test]
    fn slash_enters_filter_mode() {
        let mut a = app();
        a.handle_key(KeyCode::Char('/'));
        assert!(a.filter.is_active());
        a.handle_key(KeyCode::Char('r'));
        a.handle_key(KeyCode::Char('e'));
        a.handle_key(KeyCode::Char('a'));
        a.handle_key(KeyCode::Char('d'));
        a.handle_key(KeyCode::Char('y'));
        assert_eq!(a.filter.query(), "ready");
    }

    #[test]
    fn esc_closes_detail_then_filter() {
        let mut a = app();
        a.detail_open = true;
        a.handle_key(KeyCode::Esc);
        assert!(!a.detail_open);
        a.filter = FilterState::Active("foo".into());
        a.handle_key(KeyCode::Esc);
        assert_eq!(a.filter, FilterState::Inactive);
    }

    #[test]
    fn key_enter_on_stack_sets_pending_open_url() {
        let mut data = DataModel::empty();
        data.stack_nodes.push(StackNode {
            ticket_id: "T1".into(),
            branch: "feature/x".into(),
            pr_url: Some("https://example.com/pr/1".into()),
            pr_number: Some(1),
            state: "open".into(),
            parent_branch: None,
        });
        let mut a = App::new(Tab::Stack, data);
        a.handle_key(KeyCode::Enter);
        assert_eq!(
            a.pending_open_url.as_deref(),
            Some("https://example.com/pr/1")
        );
    }

    #[test]
    fn key_d_on_memory_sets_pending_prune_slug() {
        let mut data = DataModel::empty();
        data.memory_entries.push(MemoryEntry {
            slug: "my-entry".into(),
            path: std::path::PathBuf::from("/tmp/my-entry.md"),
            preview: String::new(),
        });
        let mut a = App::new(Tab::Memory, data);
        a.handle_key(KeyCode::Char('d'));
        assert_eq!(a.pending_prune_slug.as_deref(), Some("my-entry"));
    }

    #[test]
    fn key_enter_on_overview_toggles_detail() {
        let mut a = app();
        a.handle_key(KeyCode::Enter);
        assert!(a.detail_open);
    }

    #[test]
    fn help_toggles_then_dismisses_on_next_key() {
        let mut a = app();
        a.handle_key(KeyCode::Char('?'));
        assert!(a.show_help);
        a.handle_key(KeyCode::Char('q'));
        // Any key dismisses help — but doesn't quit on that same press.
        assert!(!a.show_help);
        assert!(!a.quit);
    }

    // ------------------------------------------------------------------
    // TicketSort tests
    // ------------------------------------------------------------------

    #[test]
    fn ticket_sort_cycles_through_all_variants_and_wraps() {
        let mut s = TicketSort::default();
        assert_eq!(s, TicketSort::Updated);
        s = s.cycle();
        assert_eq!(s, TicketSort::State);
        s = s.cycle();
        assert_eq!(s, TicketSort::Id);
        s = s.cycle();
        assert_eq!(s, TicketSort::Title);
        s = s.cycle();
        assert_eq!(s, TicketSort::Updated, "should wrap back to Updated");
    }

    #[test]
    fn key_s_on_tickets_tab_cycles_sort_and_resets_row() {
        let mut a = app();
        a.active_tab = Tab::Tickets;
        a.selected_row = 5;
        assert_eq!(a.ticket_sort, TicketSort::Updated);
        a.handle_key(KeyCode::Char('s'));
        assert_eq!(a.ticket_sort, TicketSort::State);
        assert_eq!(a.selected_row, 0, "sort change must reset selected row");
        a.handle_key(KeyCode::Char('s'));
        assert_eq!(a.ticket_sort, TicketSort::Id);
        a.handle_key(KeyCode::Char('s'));
        assert_eq!(a.ticket_sort, TicketSort::Title);
        a.handle_key(KeyCode::Char('s'));
        assert_eq!(a.ticket_sort, TicketSort::Updated);
    }

    #[test]
    fn key_s_outside_tickets_tab_is_ignored() {
        let mut a = app();
        // active_tab is Overview (default)
        a.handle_key(KeyCode::Char('s'));
        assert_eq!(
            a.ticket_sort,
            TicketSort::Updated,
            "s outside Tickets tab must not change sort"
        );
    }
}
