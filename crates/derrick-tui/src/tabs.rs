//! Tab renderers for the dashboard.
//!
//! Each renderer takes a `&mut Frame`, the area to draw in, and the `App`
//! state. They are deliberately simple — `Table`, `Paragraph`, `List`,
//! `Block` — to keep v1 maintenance cost low.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{BarChart, Block, Borders, Cell, List, ListItem, Paragraph, Row, Table};
use ratatui::Frame;

use crate::app::App;
use crate::data::Tab;

/// Header line shown above all tabs.
pub fn render_header(frame: &mut Frame, area: Rect, app: &App) {
    let ts = app
        .data
        .last_refresh
        .map(|t| t.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "--:--:--".to_owned());
    let line = Line::from(vec![
        Span::styled("[derrick] ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(format!("site: {} ", app.data.site_name)),
        Span::raw("| "),
        Span::raw(format!("tab: {} ", app.active_tab.title())),
        Span::raw("| "),
        Span::raw(ts),
    ]);
    let p = Paragraph::new(line).block(Block::default().borders(Borders::BOTTOM));
    frame.render_widget(p, area);
}

/// Footer key-hint line shown below all tabs.
pub fn render_footer(frame: &mut Frame, area: Rect) {
    let hints = "q quit  r refresh  ↑↓ scroll  ⏎ detail  / filter  ? help  Esc back  1-6 tabs";
    let p = Paragraph::new(hints).block(Block::default().borders(Borders::TOP));
    frame.render_widget(p, area);
}

/// Tab bar across the top showing the active tab.
pub fn render_tabs_bar(frame: &mut Frame, area: Rect, app: &App) {
    let titles: Vec<Line> = Tab::all()
        .iter()
        .enumerate()
        .map(|(i, t)| Line::from(Span::raw(format!("{}:{}", i + 1, t.title()))))
        .collect();
    let tabs = ratatui::widgets::Tabs::new(titles)
        .select(app.active_tab.index())
        .block(Block::default().borders(Borders::BOTTOM))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_widget(tabs, area);
}

/// Render the active tab's body inside `area`.
pub fn render_active_tab(frame: &mut Frame, area: Rect, app: &App) {
    match app.active_tab {
        Tab::Overview => render_overview(frame, area, app),
        Tab::Tickets => render_tickets(frame, area, app),
        Tab::Stack => render_stack(frame, area, app),
        Tab::Activity => render_activity(frame, area, app),
        Tab::Tokens => render_tokens(frame, area, app),
        Tab::Memory => render_memory(frame, area, app),
    }

    if app.show_help {
        render_help_overlay(frame, area);
    }
}

fn render_overview(frame: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(5),
            Constraint::Min(5),
        ])
        .split(area);

    let o = &app.data.overview;
    let batch = o.batch_name.as_deref().unwrap_or("(no active batch)");
    let foreman = match &o.foreman_status {
        Some(s) => format!("foreman: {} pid={:?}", s.mode, s.pid),
        None => "foreman: unknown".to_owned(),
    };
    let summary = vec![
        Line::from(format!("batch: {batch}")),
        Line::from(format!(
            "{}/{} done · {} in-flight · {} ready · {} blocked",
            o.tickets_done, o.tickets_total, o.tickets_inflight, o.tickets_ready, o.tickets_blocked
        )),
        Line::from(foreman),
    ];
    frame.render_widget(
        Paragraph::new(summary).block(Block::default().title("Summary").borders(Borders::ALL)),
        chunks[0],
    );

    let inflight_rows: Vec<Row> = app
        .data
        .tickets
        .iter()
        .filter(|t| t.state == "in_flight" || t.state == "in_review")
        .map(|t| {
            Row::new(vec![
                Cell::from(t.id.clone()),
                Cell::from(t.title.clone()),
                Cell::from(t.owner.clone().unwrap_or_default()),
            ])
        })
        .collect();
    let inflight_table = Table::new(
        inflight_rows,
        [
            Constraint::Length(12),
            Constraint::Min(20),
            Constraint::Length(20),
        ],
    )
    .header(
        Row::new(vec!["id", "title", "owner"]).style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(Block::default().title("In flight").borders(Borders::ALL));
    frame.render_widget(inflight_table, chunks[1]);

    let ready_rows: Vec<Row> = app
        .data
        .tickets
        .iter()
        .filter(|t| t.state == "ready")
        .map(|t| {
            Row::new(vec![
                Cell::from(t.id.clone()),
                Cell::from(t.title.clone()),
                Cell::from(t.batch.clone().unwrap_or_default()),
            ])
        })
        .collect();
    let ready_table = Table::new(
        ready_rows,
        [
            Constraint::Length(12),
            Constraint::Min(20),
            Constraint::Length(20),
        ],
    )
    .header(
        Row::new(vec!["id", "title", "batch"]).style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(Block::default().title("Ready next").borders(Borders::ALL));
    frame.render_widget(ready_table, chunks[2]);
}

fn render_tickets(frame: &mut Frame, area: Rect, app: &App) {
    let q = app.filter.query().to_ascii_lowercase();
    let rows: Vec<Row> = app
        .data
        .tickets
        .iter()
        .filter(|t| {
            if q.is_empty() {
                return true;
            }
            t.state.to_ascii_lowercase().contains(&q)
                || t.title.to_ascii_lowercase().contains(&q)
                || t.id.to_ascii_lowercase().contains(&q)
        })
        .enumerate()
        .map(|(i, t)| {
            let style = if i == app.selected_row {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            Row::new(vec![
                Cell::from(t.id.clone()),
                Cell::from(t.state.clone()),
                Cell::from(t.title.clone()),
                Cell::from(t.batch.clone().unwrap_or_default()),
                Cell::from(t.owner.clone().unwrap_or_default()),
            ])
            .style(style)
        })
        .collect();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(3)])
        .split(area);

    let title = if q.is_empty() {
        "Tickets".to_owned()
    } else {
        format!("Tickets (filter: {q})")
    };
    let table = Table::new(
        rows,
        [
            Constraint::Length(12),
            Constraint::Length(10),
            Constraint::Min(20),
            Constraint::Length(14),
            Constraint::Length(14),
        ],
    )
    .header(
        Row::new(vec!["id", "state", "title", "batch", "owner"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(Block::default().title(title).borders(Borders::ALL));
    frame.render_widget(table, chunks[0]);

    let filter_line = if app.filter.is_active() {
        format!("/ {}_", app.filter.query())
    } else {
        "press / to filter".to_owned()
    };
    frame.render_widget(
        Paragraph::new(filter_line).block(Block::default().borders(Borders::ALL)),
        chunks[1],
    );
}

fn render_stack(frame: &mut Frame, area: Rect, app: &App) {
    if app.data.stack_nodes.is_empty() {
        let p = Paragraph::new("loading stack…\n(no stack data yet)")
            .block(Block::default().title("Stack").borders(Borders::ALL));
        frame.render_widget(p, area);
        return;
    }

    let items: Vec<ListItem> = app
        .data
        .stack_nodes
        .iter()
        .map(|n| {
            let mark = match n.state.as_str() {
                "open" => "●",
                "merged" => "✓",
                "closed" => "✗",
                _ => "…",
            };
            let parent = n.parent_branch.as_deref().unwrap_or("(root)");
            let pr = n
                .pr_url
                .as_deref()
                .map(|u| format!(" {u}"))
                .unwrap_or_default();
            ListItem::new(format!(
                "{mark} {parent} -> {} [{}]{pr}",
                n.branch, n.ticket_id
            ))
        })
        .collect();

    let list = List::new(items).block(Block::default().title("Stack").borders(Borders::ALL));
    frame.render_widget(list, area);
}

fn render_activity(frame: &mut Frame, area: Rect, app: &App) {
    let items: Vec<ListItem> = app
        .data
        .events
        .iter()
        .rev() // newest-last so newest sits at bottom
        .map(|e| {
            let ticket = e
                .ticket
                .as_deref()
                .map(|t| format!("[{t}] "))
                .unwrap_or_default();
            ListItem::new(format!(
                "{} {} {ticket}{}",
                e.at.format("%H:%M:%S"),
                e.kind,
                e.body
            ))
        })
        .collect();

    let title = if app.activity_auto_scroll {
        "Activity (auto-scroll)"
    } else {
        "Activity (paused)"
    };
    let list = List::new(items).block(Block::default().title(title).borders(Borders::ALL));
    frame.render_widget(list, area);
}

fn render_tokens(frame: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Min(5)])
        .split(area);

    let s = &app.data.token_summary;
    let summary = vec![
        Line::from(format!("total in:  {}", s.total_in)),
        Line::from(format!("total out: {}", s.total_out)),
        Line::from("(full savings attribution deferred — see plan §Tokens)"),
    ];
    frame.render_widget(
        Paragraph::new(summary).block(Block::default().title("Tokens").borders(Borders::ALL)),
        chunks[0],
    );

    let data = [
        ("in", u64::min(s.total_in, u64::from(u32::MAX))),
        ("out", u64::min(s.total_out, u64::from(u32::MAX))),
    ];
    let chart = BarChart::default()
        .block(Block::default().title("Breakdown").borders(Borders::ALL))
        .data(&data)
        .bar_width(5);
    frame.render_widget(chart, chunks[1]);
}

fn render_memory(frame: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(30), Constraint::Min(20)])
        .split(area);

    let items: Vec<ListItem> = app
        .data
        .memory_entries
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let style = if i == app.selected_row {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            ListItem::new(m.slug.clone()).style(style)
        })
        .collect();
    let list = List::new(items).block(Block::default().title("Memory").borders(Borders::ALL));
    frame.render_widget(list, chunks[0]);

    let preview = app
        .data
        .memory_entries
        .get(app.selected_row)
        .map(|m| m.preview.clone())
        .unwrap_or_else(|| "(no entry selected)".to_owned());
    frame.render_widget(
        Paragraph::new(preview).block(Block::default().title("Preview").borders(Borders::ALL)),
        chunks[1],
    );
}

fn render_help_overlay(frame: &mut Frame, area: Rect) {
    let body = "Keys:\n\
        q     quit\n\
        r     refresh\n\
        1-6   switch tab\n\
        ↑/↓   navigate rows\n\
        ⏎     toggle detail\n\
        /     filter\n\
        Esc   close detail / cancel filter\n\
        ?     toggle this help";
    let p = Paragraph::new(body).block(Block::default().title("Help").borders(Borders::ALL));
    frame.render_widget(p, area);
}
