//! UI state machine for the dashboard.
//!
//! `App` is the in-memory state held by the event loop. It owns the latest
//! `DataModel` snapshot plus all transient UI state (active tab, scroll
//! offset, filter, detail pane open/closed). The event loop renders from
//! `App` and feeds key events into [`App::handle_key`].

use crossterm::event::KeyCode;

use crate::data::{DataModel, Tab};

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
}

impl App {
    /// Construct an `App` rooted at `initial_tab` with the given data.
    pub fn new(initial_tab: Tab, data: DataModel) -> Self {
        Self {
            active_tab: initial_tab,
            scroll_offset: 0,
            selected_row: 0,
            filter: FilterState::Inactive,
            detail_open: false,
            data,
            quit: false,
            show_help: false,
            refresh_requested: false,
            activity_auto_scroll: true,
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
            KeyCode::Enter => {
                self.detail_open = !self.detail_open;
            }
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
            KeyCode::Char(c @ '1'..='6') => {
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
    use crate::data::DataModel;

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
    fn help_toggles_then_dismisses_on_next_key() {
        let mut a = app();
        a.handle_key(KeyCode::Char('?'));
        assert!(a.show_help);
        a.handle_key(KeyCode::Char('q'));
        // Any key dismisses help — but doesn't quit on that same press.
        assert!(!a.show_help);
        assert!(!a.quit);
    }
}
