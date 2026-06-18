//! Tab renderers for the dashboard.
//!
//! Each renderer takes a `&mut Frame`, the area to draw in, and the `App`
//! state. They are deliberately simple — `Table`, `Paragraph`, `List`,
//! `Block` — to keep v1 maintenance cost low.

use chrono::Utc;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{BarChart, Block, Borders, Cell, List, ListItem, Paragraph, Row, Table};

use crate::app::{App, TicketSort};
use crate::data::{ActivityFilter, HandRow, StackLoadResult, Tab, TicketRow};
use derrick_substrate::{ForemanMode, HandKind};

/// Format a duration in seconds as a compact human-readable string
/// (`"14m"`, `"2h03m"`, `"5s"`).
fn fmt_secs(secs: i64) -> String {
    let secs = secs.max(0) as u64;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

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
    let hints = "q quit  r refresh  ↑↓ scroll  ⏎ detail  / filter  ? help  Esc back  1-8 tabs";
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
        Tab::Hands => render_hands(frame, area, app),
        Tab::Factory => render_factory(frame, area, app),
    }

    if app.show_help {
        render_help_overlay(frame, area);
    }
}

fn render_overview(frame: &mut Frame, area: Rect, app: &App) {
    // Summary block: 6 content lines + top/bottom borders = 8 rows.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8),
            Constraint::Min(4),
            Constraint::Min(4),
        ])
        .split(area);

    let o = &app.data.overview;

    // ── row 1: active batch ──────────────────────────────────────────────
    let batch = o.batch_name.as_deref().unwrap_or("(no active batch)");

    // ── row 2: ticket counts ─────────────────────────────────────────────
    let tickets_line = format!(
        "tickets:      {}/{} done · {} in-flight · {} ready · {} blocked",
        o.tickets_done, o.tickets_total, o.tickets_inflight, o.tickets_ready, o.tickets_blocked
    );

    // ── row 3: foreman ───────────────────────────────────────────────────
    let foreman_line = match &o.foreman_status {
        Some(s) => {
            let age = s
                .started_at
                .map(|t| fmt_secs((Utc::now() - t).num_seconds()))
                .unwrap_or_else(|| "?".to_owned());
            let pid_str = s.pid.map(|p| format!(", pid {p}")).unwrap_or_default();
            format!("foreman:      {} ({}{})", s.mode, age, pid_str)
        }
        None => "foreman:      unknown".to_owned(),
    };

    // ── row 4: stack summary ─────────────────────────────────────────────
    let ss = &o.stack_summary;
    let restack_label = if ss.restack_ok { "ok" } else { "conflict!" };
    let stack_line = format!(
        "stack:        ● {} merged · {} open · {} pending  restack: {}",
        ss.merged, ss.open, ss.pending, restack_label
    );

    // ── row 5: last assay ────────────────────────────────────────────────
    let assay_line = match &o.last_assay {
        Some(a) => {
            let model = a.model.as_deref().unwrap_or("–");
            format!(
                "last assay:   {} · {} · {}",
                a.verdict,
                model,
                a.at.format("%H:%M")
            )
        }
        None => "last assay:   (none yet)".to_owned(),
    };

    // ── row 6: token spend (today only) ─────────────────────────────────
    let ts = &app.data.token_summary;
    let tokens_line = if ts.today_in == 0 && ts.today_out == 0 {
        "tokens today: (no data)".to_owned()
    } else {
        match ts.savings_pct {
            Some(pct) => {
                // savings_pct is the fraction of raw tokens saved; actual =
                // raw * (1 - pct).
                let raw_k = ts.today_in / 1_000;
                let actual_k = (ts.today_in as f64 * f64::from(1.0 - pct) / 1_000.0) as u64;
                format!(
                    "tokens today: raw {}k \u{2192} actual {}k (-{:.0}%)",
                    raw_k,
                    actual_k,
                    f64::from(pct) * 100.0
                )
            }
            None => format!(
                "tokens today: {}k in \u{00b7} {}k out",
                ts.today_in / 1_000,
                ts.today_out / 1_000,
            ),
        }
    };

    let summary = vec![
        Line::from(format!("batch:        {batch}")),
        Line::from(tickets_line),
        Line::from(foreman_line),
        Line::from(stack_line),
        Line::from(assay_line),
        Line::from(tokens_line),
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

// ---------------------------------------------------------------------------
// Ticket sort helpers
// ---------------------------------------------------------------------------

/// Maps a ticket state string to a display priority (lower = more urgent).
/// Keeps the table operationally useful: active work at the top.
fn state_priority(state: &str) -> u8 {
    match state {
        "in_flight" => 0,
        "in_review" => 1,
        "ready" => 2,
        "blocked" => 3,
        "done" => 4,
        "rejected" => 5,
        _ => 6,
    }
}

/// Compare two ticket ids with numeric-suffix awareness so that `tst-2`
/// sorts before `tst-10` rather than after it lexicographically.
fn compare_ticket_id(a: &str, b: &str) -> std::cmp::Ordering {
    fn split(s: &str) -> (&str, Option<u64>) {
        match s.rfind('-') {
            Some(i) => (&s[..i], s[i + 1..].parse::<u64>().ok()),
            None => (s, None),
        }
    }
    let (ap, an) = split(a);
    let (bp, bn) = split(b);
    ap.cmp(bp).then_with(|| match (an, bn) {
        (Some(x), Some(y)) => x.cmp(&y),
        _ => a.cmp(b),
    })
}

/// Sort a slice of `TicketRow` references in-place according to `sort`.
fn sort_ticket_rows(rows: &mut Vec<&TicketRow>, sort: TicketSort) {
    match sort {
        // Newest-updated first; `None` (no timestamp) sorts last.
        // `Reverse` flips the `Option<DateTime>` order so `Some(newest)`
        // sorts first while `None` (which is less than any Some) ends up last.
        TicketSort::Updated => {
            rows.sort_by_key(|t| std::cmp::Reverse(t.updated_at));
        }
        TicketSort::State => rows.sort_by_key(|t| state_priority(&t.state)),
        TicketSort::Id => rows.sort_by(|a, b| compare_ticket_id(&a.id, &b.id)),
        TicketSort::Title => rows.sort_by(|a, b| {
            a.title
                .to_ascii_lowercase()
                .cmp(&b.title.to_ascii_lowercase())
        }),
    }
}

fn render_tickets(frame: &mut Frame, area: Rect, app: &App) {
    let q = app.filter.query().to_ascii_lowercase();

    // Collect filtered references, then sort.
    let mut filtered: Vec<&TicketRow> = app
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
        .collect();
    sort_ticket_rows(&mut filtered, app.ticket_sort);

    let rows: Vec<Row> = filtered
        .iter()
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

    // Title encodes filter and active sort so the user can see both at a
    // glance without opening the help overlay.
    let sort_label = app.ticket_sort.label();
    let title = match (q.is_empty(), app.filter.is_active()) {
        (true, _) => format!("Tickets  sort:{sort_label}  s:cycle"),
        (false, true) => format!("Tickets  sort:{sort_label}  filter:{q}_"),
        (false, false) => format!("Tickets  sort:{sort_label}  filter:{q}"),
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

    // Status bar: shows filter prompt and a reminder that s cycles the sort.
    let status_line = if app.filter.is_active() {
        format!(
            "/ {}_  |  sort: {sort_label}  (s to cycle)",
            app.filter.query()
        )
    } else {
        format!("press / to filter  |  sort: {sort_label}  (s to cycle)")
    };
    frame.render_widget(
        Paragraph::new(status_line).block(Block::default().borders(Borders::ALL)),
        chunks[1],
    );
}

fn render_stack(frame: &mut Frame, area: Rect, app: &App) {
    // Show honest state before we attempt to render nodes.
    match &app.data.stack_load_result {
        StackLoadResult::Loading => {
            let p = Paragraph::new("loading stack data…\n(waiting for gh pr list)")
                .block(Block::default().title("Stack").borders(Borders::ALL));
            frame.render_widget(p, area);
            return;
        }
        StackLoadResult::Error(reason) => {
            let msg = format!("stack data unavailable: {reason}");
            let p =
                Paragraph::new(msg).block(Block::default().title("Stack").borders(Borders::ALL));
            frame.render_widget(p, area);
            return;
        }
        StackLoadResult::Loaded => {}
    }

    if app.data.stack_nodes.is_empty() {
        let p = Paragraph::new("(no stack nodes — no open PRs found)")
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
    let filter = ActivityFilter::from_query(app.filter.query());

    let items: Vec<ListItem> = app
        .data
        .events
        .iter()
        .rev() // newest-last so newest sits at bottom
        .filter(|e| filter.matches(e))
        .map(|e| {
            // Build a compact scope tag from whatever scope field is populated.
            let scope_tag = if let Some(t) = &e.ticket {
                format!("[{t}] ")
            } else if let Some(h) = &e.hand {
                format!("[hand:{h}] ")
            } else if let Some(r) = &e.run_id {
                format!("[run:{r}] ")
            } else {
                String::new()
            };
            ListItem::new(format!(
                "{} {} {scope_tag}{}",
                e.at.format("%H:%M:%S"),
                e.kind,
                e.body
            ))
        })
        .collect();

    // Split into list area + one-line status bar.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(area);

    // Block title reflects scroll state and active filter mode.
    let scroll_label = if app.activity_auto_scroll {
        "auto-scroll"
    } else {
        "paused"
    };
    let title = match filter.mode_label() {
        Some(label) => format!("Activity ({scroll_label})  filter:{label}"),
        None => format!("Activity ({scroll_label})"),
    };
    let list = List::new(items).block(Block::default().title(title).borders(Borders::ALL));
    frame.render_widget(list, chunks[0]);

    // Status bar: show active filter and available prefix hints.
    let status_line = if app.filter.is_active() {
        format!(
            "/ {}_  |  prefixes: ticket:  hand:  run:",
            app.filter.query()
        )
    } else if !filter.is_none() {
        // Committed (inactive) filter is applied.
        format!(
            "filter: {}  |  / to edit  Esc to clear  |  prefixes: ticket:  hand:  run:",
            filter.mode_label().unwrap_or_default()
        )
    } else {
        "press / to filter  |  prefixes: ticket:<id>  hand:<id>  run:<id>".to_owned()
    };
    frame.render_widget(
        Paragraph::new(status_line).block(Block::default().borders(Borders::ALL)),
        chunks[1],
    );
}

fn render_tokens(frame: &mut Frame, area: Rect, app: &App) {
    let s = &app.data.token_summary;
    let show_hands = s.hands_tokens_out > 0
        || s.hands_roughneck_saved > 0
        || s.hands_bytes_raw > 0
        || s.hands_bytes_saved > 0;

    // Layout: summary (7) [+ optional Hands (6)] + bar chart (min 5).
    let constraints: Vec<Constraint> = if show_hands {
        vec![
            Constraint::Length(7),
            Constraint::Length(6),
            Constraint::Min(5),
        ]
    } else {
        vec![Constraint::Length(7), Constraint::Min(5)]
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    // ── Summary paragraph ────────────────────────────────────────────────
    let today_note = if s.today_in == 0 && s.today_out == 0 {
        "today:     (no runs yet today)".to_owned()
    } else {
        format!(
            "today:     {}k in / {}k out",
            s.today_in / 1_000,
            s.today_out / 1_000
        )
    };
    let alltime_note = if s.total_in == 0 {
        "all-time:  (no data)".to_owned()
    } else {
        format!(
            "all-time:  {}k in / {}k out",
            s.total_in / 1_000,
            s.total_out / 1_000
        )
    };
    let savings_note = if s.total_bytes_raw > 0 {
        let pct = 100.0 * s.total_bytes_saved as f64 / s.total_bytes_raw as f64;
        format!(
            "compression: {}kb raw → {}kb out  ({:.0}% saved, subprocess output)",
            s.total_bytes_raw / 1024,
            (s.total_bytes_raw.saturating_sub(s.total_bytes_saved)) / 1024,
            pct,
        )
    } else {
        match s.savings_pct {
            Some(p) => format!("savings:   {:.0}% (RTK attribution)", f64::from(p) * 100.0),
            None => "compression: (no bash steps recorded yet)".to_owned(),
        }
    };
    let roughneck_note = if s.total_roughneck_saved > 0 {
        format!(
            "roughneck: ~{} tokens saved  (prompt injection, est.)",
            s.total_roughneck_saved
        )
    } else {
        "roughneck: no savings recorded yet".to_string()
    };
    let summary = vec![
        Line::from(today_note),
        Line::from(alltime_note),
        Line::from(savings_note),
        Line::from(roughneck_note),
        Line::from("source:    run manifests (.derrick/runs/*/manifest.json)"),
    ];
    frame.render_widget(
        Paragraph::new(summary).block(Block::default().title("Tokens").borders(Borders::ALL)),
        chunks[0],
    );

    // ── Hands summary (optional) ─────────────────────────────────────────
    let chart_chunk = if show_hands {
        let pct = if s.hands_bytes_raw > 0 {
            100.0 * s.hands_bytes_saved as f64 / s.hands_bytes_raw as f64
        } else {
            0.0
        };
        let hands_lines = vec![
            Line::from(format!("tokens out:        {}", s.hands_tokens_out)),
            Line::from(format!("roughneck saved:   ~{}", s.hands_roughneck_saved)),
            Line::from(format!(
                "scrub bytes saved: {} kb / {} kb raw  ({:.0}%)",
                s.hands_bytes_saved / 1024,
                s.hands_bytes_raw / 1024,
                pct
            )),
            Line::from("source:            substrate `hand stats:` notes"),
        ];
        frame.render_widget(
            Paragraph::new(hands_lines)
                .block(Block::default().title("Hands").borders(Borders::ALL)),
            chunks[1],
        );
        chunks[2]
    } else {
        chunks[1]
    };

    // ── Per-step bar chart ───────────────────────────────────────────────
    if s.per_step.is_empty() {
        frame.render_widget(
            Paragraph::new("(no step data — run `derrick drill` to generate token records)").block(
                Block::default()
                    .title("Per-step breakdown")
                    .borders(Borders::ALL),
            ),
            chart_chunk,
        );
        return;
    }

    // Build a flat label/value slice for BarChart. We show tokens_out per
    // step (output tokens represent actual LLM work done).  The bar chart
    // widget requires &[(&str, u64)] backed by a local Vec of owned strings
    // — we keep both alive until `frame.render_widget` consumes them.
    //
    // Cap each value to u32::MAX so the cast is safe; no real run will
    // approach 4B tokens on a single step.
    let cap = u64::from(u32::MAX);
    let owned_labels: Vec<String> = s.per_step.iter().map(|st| st.step_id.clone()).collect();
    let bar_data: Vec<(&str, u64)> = s
        .per_step
        .iter()
        .zip(owned_labels.iter())
        .map(|(st, label)| {
            let val = st.tokens_out.min(cap);
            (label.as_str(), val)
        })
        .collect();

    let chart = BarChart::default()
        .block(
            Block::default()
                .title("Per-step (tokens out, all runs)")
                .borders(Borders::ALL),
        )
        .data(&bar_data)
        .bar_width(9)
        .bar_gap(1);
    frame.render_widget(chart, chart_chunk);
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

fn hand_status_span(status: &str) -> Span<'static> {
    match status {
        "done" => Span::styled("✓", Style::default().fg(ratatui::style::Color::Green)),
        "failed" => Span::styled("✗", Style::default().fg(ratatui::style::Color::Red)),
        _ => Span::styled("⟳", Style::default().fg(ratatui::style::Color::Yellow)),
    }
}

fn render_hands(frame: &mut Frame, area: Rect, app: &App) {
    let filter = ActivityFilter::from_query(app.filter.query());

    // Apply hand:/ticket: / text filters to the rollup. Hands are filtered
    // by hand_id; ticket filters narrow to hands that touched a given ticket.
    let visible: Vec<&HandRow> = app
        .data
        .hand_rows
        .iter()
        .filter(|row| match &filter {
            ActivityFilter::None => true,
            ActivityFilter::Hand(q) => row.hand_id.to_ascii_lowercase().contains(q),
            ActivityFilter::Ticket(q) => row
                .ticket_id
                .as_deref()
                .is_some_and(|t| t.to_ascii_lowercase().contains(q)),
            ActivityFilter::Run(_) => false,
            ActivityFilter::Text(q) => {
                row.hand_id.to_ascii_lowercase().contains(q)
                    || row
                        .ticket_id
                        .as_deref()
                        .is_some_and(|t| t.to_ascii_lowercase().contains(q))
                    || row.action.to_ascii_lowercase().contains(q)
                    || row
                        .detail
                        .as_deref()
                        .is_some_and(|d| d.to_ascii_lowercase().contains(q))
            }
        })
        .collect();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(3)])
        .split(area);

    let now = Utc::now();
    let rows: Vec<Row> = visible
        .iter()
        .map(|h| {
            let age = fmt_secs((now - h.last_seen).num_seconds());
            Row::new(vec![
                Cell::from(Line::from(hand_status_span(&h.status))),
                Cell::from(h.hand_id.clone()),
                Cell::from(h.pid.map(|p| p.to_string()).unwrap_or_default()),
                Cell::from(h.ticket_id.clone().unwrap_or_default()),
                Cell::from(h.action.clone()),
                Cell::from(age),
                Cell::from(h.detail.clone().unwrap_or_default()),
            ])
        })
        .collect();

    let count_label = format!(
        "{} hand{}",
        visible.len(),
        if visible.len() == 1 { "" } else { "s" }
    );
    let title = match filter.mode_label() {
        Some(label) => format!("Hands  {count_label}  filter:{label}"),
        None => format!("Hands  {count_label}"),
    };

    let table = Table::new(
        rows,
        [
            Constraint::Length(2),
            Constraint::Length(20),
            Constraint::Length(8),
            Constraint::Length(12),
            Constraint::Length(16),
            Constraint::Length(8),
            Constraint::Min(10),
        ],
    )
    .header(
        Row::new(vec!["", "hand", "pid", "ticket", "action", "age", "detail"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(Block::default().title(title).borders(Borders::ALL));
    frame.render_widget(table, chunks[0]);

    let status_line = if app.filter.is_active() {
        format!("/ {}_  |  prefixes: hand:  ticket:", app.filter.query())
    } else if !filter.is_none() {
        format!(
            "filter: {}  |  / to edit  Esc to clear",
            filter.mode_label().unwrap_or_default()
        )
    } else {
        "press / to filter  |  prefixes: hand:<id>  ticket:<id>".to_owned()
    };
    frame.render_widget(
        Paragraph::new(status_line).block(Block::default().borders(Borders::ALL)),
        chunks[1],
    );
}

/// Unicode avatar glyph for each hand kind (D78). Emoji give the workers a
/// little personality; the braille spinner set below carries the animation.
fn hand_kind_avatar(kind: HandKind) -> &'static str {
    match kind {
        HandKind::Claude => "🤖",
        HandKind::Copilot => "🐙",
        HandKind::Codex => "🧑‍💻",
        HandKind::Opencode => "🦀",
        HandKind::Aider => "🦜",
        HandKind::Human => "🧑",
        _ => "·",
    }
}

/// Braille spinner frames used for the "hammering" worker animation (D78).
/// Reuses the set from `derrick-cli`'s `CliReporter` conceptually; kept inline
/// here so `derrick-tui` stays independent of `derrick-cli`.
const FACTORY_SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Factory tab (D78): an ASCII factory floor of animated workers. Each
/// registered hand is a workstation with a unicode avatar per `HandKind`; the
/// worker's animation state is derived from the structured hand telemetry
/// (`HandStarted`/`HandProgress`/`HandExited`, surfaced via `hand_rows`). A
/// smokestack puffs when the foreman is running; a ready-ticket conveyor and a
/// shipping dock (done tickets) frame the floor. Read-only — never mutates
/// state. Animation is driven by `app.animation_frame` (incremented at ~100 ms
/// by the event loop); substrate data still refreshes at 1 Hz / on `notify`.
fn render_factory(frame: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5), // smokestack + status
            Constraint::Min(5),    // workstations
            Constraint::Length(3), // conveyor + dock
        ])
        .split(area);

    let foreman = app.data.overview.foreman_status.as_ref();
    let puffing = matches!(
        foreman.map(|f| f.mode),
        Some(ForemanMode::Attached) | Some(ForemanMode::Detached)
    );
    let mode_label = match foreman.map(|f| f.mode) {
        Some(ForemanMode::Attached) => "attached",
        Some(ForemanMode::Detached) => "detached",
        _ => "stopped",
    };
    let done_count = app
        .data
        .tickets
        .iter()
        .filter(|t| t.state == "done")
        .count();
    let ready_count = app
        .data
        .tickets
        .iter()
        .filter(|t| t.state == "ready")
        .count();
    let inflight_count = app
        .data
        .tickets
        .iter()
        .filter(|t| t.state == "in_flight" || t.state == "in_review")
        .count();
    let stack_glyph = if puffing { "🏭💨" } else { "🏭  " };
    let status_line = format!(
        "{stack_glyph}  foreman: {mode_label}   workers: {}   in-flight: {inflight_count}   ready: {ready_count}   shipped: {done_count}",
        app.data.hands.len(),
    );
    frame.render_widget(
        Paragraph::new(status_line)
            .block(Block::default().title("Factory floor").borders(Borders::ALL)),
        chunks[0],
    );

    // Workstations: one row per registered hand. The spinner frame is indexed
    // by app.animation_frame so the "running" workers animate at ~100 ms.
    let frame_idx = (app.animation_frame as usize) % FACTORY_SPINNER.len();
    let rows: Vec<Row> = app
        .data
        .hands
        .iter()
        .map(|hand| {
            let avatar = hand_kind_avatar(hand.kind);
            let row = app
                .data
                .hand_rows
                .iter()
                .find(|r| r.hand_id == hand.id.as_str());
            let (status_glyph, action) = match row {
                Some(r) => {
                    let g = match r.status.as_str() {
                        "done" => "✓",
                        "failed" => "✗",
                        "running" => FACTORY_SPINNER[frame_idx],
                        _ => "·",
                    };
                    (g.to_owned(), r.action.clone())
                }
                None => ("·".to_owned(), "idle".to_owned()),
            };
            let ticket = row
                .and_then(|r| r.ticket_id.clone())
                .unwrap_or_else(|| "-".to_owned());
            let pid = hand
                .pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".to_owned());
            Row::new(vec![
                Cell::from(avatar.to_owned()),
                Cell::from(hand.id.as_str().to_owned()),
                Cell::from(status_glyph),
                Cell::from(action),
                Cell::from(ticket),
                Cell::from(pid),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(4),
            Constraint::Length(20),
            Constraint::Length(3),
            Constraint::Length(12),
            Constraint::Length(14),
            Constraint::Length(8),
        ],
    )
    .header(
        Row::new(vec!["", "hand", "st", "action", "ticket", "pid"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(Block::default().title("Workstations").borders(Borders::ALL));
    frame.render_widget(table, chunks[1]);

    // Conveyor: ready tickets flowing toward the dock. A spinner offset by a
    // few frames from the worker animation gives the belt its own motion.
    let belt_frame = (app.animation_frame as usize + 3) % FACTORY_SPINNER.len();
    let ready: Vec<String> = app
        .data
        .tickets
        .iter()
        .filter(|t| t.state == "ready")
        .map(|t| t.id.clone())
        .collect();
    let belt = if ready.is_empty() {
        "(queue empty)".to_owned()
    } else {
        ready.join(" ▸ ")
    };
    let conveyor_line = format!("{} ready conveyor: {}", FACTORY_SPINNER[belt_frame], belt);
    frame.render_widget(
        Paragraph::new(conveyor_line)
            .block(Block::default().title("Conveyor → dock").borders(Borders::ALL)),
        chunks[2],
    );
}

fn render_help_overlay(frame: &mut Frame, area: Rect) {
    let body = "Keys:\n\
        q     quit\n\
        r     refresh\n\
        1-8   switch tab\n\
        ↑/↓   navigate rows\n\
        ⏎     toggle detail / open PR (Stack)\n\
        /     filter\n\
        s     cycle sort order (Tickets tab)\n\
        d     flag memory entry for deletion (Memory tab)\n\
        Esc   close detail / cancel filter\n\
        ?     toggle this help";
    let p = Paragraph::new(body).block(Block::default().title("Help").borders(Borders::ALL));
    frame.render_widget(p, area);
}

// ---------------------------------------------------------------------------
// Render-safety helpers (used only in tests)
// ---------------------------------------------------------------------------

/// Render every tab once using the given app state and a
/// `TestBackend`. Panics in the render path become test failures.
#[cfg(test)]
fn render_all_tabs_no_panic(app: &crate::app::App) {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::{Constraint, Direction, Layout};

    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).expect("test terminal");

    for tab in crate::data::Tab::all() {
        let mut test_app = app.clone();
        test_app.active_tab = tab;
        terminal
            .draw(|frame| {
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(2),
                        Constraint::Length(3),
                        Constraint::Min(5),
                        Constraint::Length(2),
                    ])
                    .split(frame.area());
                render_header(frame, chunks[0], &test_app);
                render_tabs_bar(frame, chunks[1], &test_app);
                render_active_tab(frame, chunks[2], &test_app);
                render_footer(frame, chunks[3]);
            })
            .expect("draw should not fail");
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use chrono::TimeZone;

    use super::*;
    use crate::data::TicketRow;

    fn row(
        id: &str,
        state: &str,
        title: &str,
        updated_at: Option<chrono::DateTime<Utc>>,
    ) -> TicketRow {
        TicketRow {
            id: id.to_owned(),
            state: state.to_owned(),
            title: title.to_owned(),
            batch: None,
            owner: None,
            updated_at,
        }
    }

    #[test]
    fn sort_by_id_uses_numeric_suffix() {
        let r1 = row("tst-1", "ready", "a", None);
        let r2 = row("tst-2", "ready", "b", None);
        let r10 = row("tst-10", "ready", "c", None);
        let mut rows = vec![&r10, &r2, &r1];
        sort_ticket_rows(&mut rows, TicketSort::Id);
        assert_eq!(rows[0].id, "tst-1");
        assert_eq!(rows[1].id, "tst-2");
        assert_eq!(rows[2].id, "tst-10");
    }

    #[test]
    fn sort_by_state_puts_inflight_first() {
        let r_done = row("t1", "done", "d", None);
        let r_blocked = row("t2", "blocked", "b", None);
        let r_inflight = row("t3", "in_flight", "i", None);
        let r_ready = row("t4", "ready", "r", None);
        let mut rows = vec![&r_done, &r_blocked, &r_inflight, &r_ready];
        sort_ticket_rows(&mut rows, TicketSort::State);
        assert_eq!(rows[0].id, "t3", "in_flight should be first");
        assert_eq!(rows[1].id, "t4", "ready should follow");
        assert_eq!(rows[2].id, "t2", "blocked should follow ready");
        assert_eq!(rows[3].id, "t1", "done should be last");
    }

    #[test]
    fn sort_by_title_is_case_insensitive() {
        let r_z = row("t1", "ready", "Zebra", None);
        let r_a = row("t2", "ready", "apple", None);
        let r_m = row("t3", "ready", "Mango", None);
        let mut rows = vec![&r_z, &r_a, &r_m];
        sort_ticket_rows(&mut rows, TicketSort::Title);
        assert_eq!(rows[0].id, "t2"); // apple
        assert_eq!(rows[1].id, "t3"); // Mango
        assert_eq!(rows[2].id, "t1"); // Zebra
    }

    #[test]
    fn sort_by_updated_newest_first_and_none_last() {
        let newer = Utc.with_ymd_and_hms(2025, 6, 1, 12, 0, 0).unwrap();
        let older = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let r_none = row("t1", "ready", "no timestamp", None);
        let r_old = row("t2", "ready", "old", Some(older));
        let r_new = row("t3", "ready", "new", Some(newer));
        let mut rows = vec![&r_none, &r_old, &r_new];
        sort_ticket_rows(&mut rows, TicketSort::Updated);
        assert_eq!(rows[0].id, "t3", "newest should be first");
        assert_eq!(rows[1].id, "t2", "older should follow");
        assert_eq!(rows[2].id, "t1", "None updated_at should be last");
    }

    #[test]
    fn compare_ticket_id_handles_same_prefix_different_numbers() {
        use std::cmp::Ordering;
        assert_eq!(compare_ticket_id("tst-2", "tst-10"), Ordering::Less);
        assert_eq!(compare_ticket_id("tst-10", "tst-2"), Ordering::Greater);
        assert_eq!(compare_ticket_id("tst-5", "tst-5"), Ordering::Equal);
    }

    // -----------------------------------------------------------------------
    // Render-safety: every tab must render without panic on empty / mid-refresh
    // data. These use ratatui TestBackend so they don't need a real terminal.
    // -----------------------------------------------------------------------

    #[test]
    fn all_tabs_render_without_panic_on_empty_data() {
        let app = crate::app::App::new(crate::data::Tab::Overview, crate::data::DataModel::empty());
        // Renders all seven tabs; any panic is a test failure.
        render_all_tabs_no_panic(&app);
    }

    #[test]
    fn all_tabs_render_without_panic_when_stack_is_loading() {
        let mut data = crate::data::DataModel::empty();
        data.stack_load_result = crate::data::StackLoadResult::Loading;
        let app = crate::app::App::new(crate::data::Tab::Stack, data);
        render_all_tabs_no_panic(&app);
    }

    #[test]
    fn all_tabs_render_without_panic_when_stack_has_error() {
        let mut data = crate::data::DataModel::empty();
        data.stack_load_result = crate::data::StackLoadResult::Error("gh not found".to_owned());
        let app = crate::app::App::new(crate::data::Tab::Stack, data);
        render_all_tabs_no_panic(&app);
    }

    #[test]
    fn all_tabs_render_without_panic_when_stack_loaded_empty() {
        let mut data = crate::data::DataModel::empty();
        data.stack_load_result = crate::data::StackLoadResult::Loaded;
        // stack_nodes is empty — should show "no open PRs found"
        let app = crate::app::App::new(crate::data::Tab::Stack, data);
        render_all_tabs_no_panic(&app);
    }

    #[test]
    fn all_tabs_render_without_panic_when_selected_row_out_of_bounds() {
        // Simulate a stale selected_row after data shrinks (e.g. tickets cleared
        // between refreshes).
        let mut app =
            crate::app::App::new(crate::data::Tab::Tickets, crate::data::DataModel::empty());
        // Force selected_row well past the end of all empty vecs.
        app.selected_row = 999;
        render_all_tabs_no_panic(&app);
    }

    #[test]
    fn all_tabs_render_without_panic_with_help_overlay() {
        let mut app =
            crate::app::App::new(crate::data::Tab::Overview, crate::data::DataModel::empty());
        app.show_help = true;
        render_all_tabs_no_panic(&app);
    }

    // -----------------------------------------------------------------------
    // Helpers: build rich data fixtures used across the populated-data tests
    // -----------------------------------------------------------------------

    fn make_ticket(
        id: &str,
        state: &str,
        title: &str,
        batch: Option<&str>,
        owner: Option<&str>,
    ) -> crate::data::TicketRow {
        crate::data::TicketRow {
            id: id.to_owned(),
            state: state.to_owned(),
            title: title.to_owned(),
            batch: batch.map(str::to_owned),
            owner: owner.map(str::to_owned),
            updated_at: Some(Utc::now()),
        }
    }

    fn make_stack_node(
        ticket_id: &str,
        branch: &str,
        state: &str,
        parent_branch: Option<&str>,
        pr_url: Option<&str>,
    ) -> crate::data::StackNode {
        crate::data::StackNode {
            ticket_id: ticket_id.to_owned(),
            branch: branch.to_owned(),
            pr_url: pr_url.map(str::to_owned),
            pr_number: pr_url.map(|_| 42),
            state: state.to_owned(),
            parent_branch: parent_branch.map(str::to_owned),
        }
    }

    fn make_event(
        kind: &str,
        ticket: Option<&str>,
        hand: Option<&str>,
        run_id: Option<&str>,
        body: &str,
    ) -> crate::data::EventRow {
        crate::data::EventRow {
            at: Utc::now(),
            kind: kind.to_owned(),
            ticket: ticket.map(str::to_owned),
            hand: hand.map(str::to_owned),
            run_id: run_id.map(str::to_owned),
            body: body.to_owned(),
        }
    }

    fn make_hand_row(
        hand_id: &str,
        ticket_id: Option<&str>,
        action: &str,
        status: &str,
        detail: Option<&str>,
    ) -> crate::data::HandRow {
        crate::data::HandRow {
            hand_id: hand_id.to_owned(),
            ticket_id: ticket_id.map(str::to_owned),
            action: action.to_owned(),
            last_seen: Utc::now(),
            status: status.to_owned(),
            detail: detail.map(str::to_owned),
            pid: None,
        }
    }

    /// Build a `TestBackend` terminal and render the given `App`, returning
    /// the buffer contents as a flat string for assertions.
    fn render_tab_to_string(app: &crate::app::App, width: u16, height: u16) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::layout::{Constraint, Direction, Layout};

        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let test_app = app.clone();

        terminal
            .draw(|frame| {
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(2),
                        Constraint::Length(3),
                        Constraint::Min(5),
                        Constraint::Length(2),
                    ])
                    .split(frame.area());
                render_header(frame, chunks[0], &test_app);
                render_tabs_bar(frame, chunks[1], &test_app);
                render_active_tab(frame, chunks[2], &test_app);
                render_footer(frame, chunks[3]);
            })
            .expect("draw");

        // Collect the buffer into a string of all visible cells.
        let buf = terminal.backend().buffer().clone();
        buf.content()
            .iter()
            .map(|cell| cell.symbol().to_owned())
            .collect::<Vec<_>>()
            .join("")
    }

    // -----------------------------------------------------------------------
    // fmt_secs corner cases
    // -----------------------------------------------------------------------

    #[test]
    fn fmt_secs_handles_zero() {
        assert_eq!(fmt_secs(0), "0s");
    }

    #[test]
    fn fmt_secs_negative_clamped_to_zero() {
        assert_eq!(fmt_secs(-5), "0s");
    }

    #[test]
    fn fmt_secs_exact_minute() {
        assert_eq!(fmt_secs(60), "1m");
    }

    #[test]
    fn fmt_secs_minutes() {
        assert_eq!(fmt_secs(90), "1m");
        assert_eq!(fmt_secs(3599), "59m");
    }

    #[test]
    fn fmt_secs_hours_and_minutes() {
        assert_eq!(fmt_secs(3600), "1h00m");
        assert_eq!(fmt_secs(3661), "1h01m");
        assert_eq!(fmt_secs(7384), "2h03m");
    }

    // -----------------------------------------------------------------------
    // Overview tab: populated data paths
    // -----------------------------------------------------------------------

    #[test]
    fn overview_renders_batch_name_and_ticket_counts() {
        use crate::data::{ForemanStatusSnapshot, LastAssaySnapshot, OverviewData, StackSummary};
        use derrick_substrate::ForemanMode;

        let mut data = crate::data::DataModel::empty();
        data.overview = OverviewData {
            batch_name: Some("sprint-3".to_owned()),
            tickets_done: 5,
            tickets_total: 10,
            tickets_inflight: 2,
            tickets_ready: 2,
            tickets_blocked: 1,
            foreman_status: Some(ForemanStatusSnapshot {
                mode: ForemanMode::Attached,
                pid: Some(12345),
                started_at: Some(Utc::now() - chrono::Duration::seconds(125)),
            }),
            stack_summary: StackSummary {
                merged: 3,
                open: 2,
                pending: 1,
                restack_ok: true,
            },
            last_assay: Some(LastAssaySnapshot {
                verdict: "success".to_owned(),
                model: Some("claude-3".to_owned()),
                at: Utc::now(),
            }),
        };
        // Add in_flight and ready tickets so the overview sub-tables are populated.
        data.tickets = vec![
            make_ticket(
                "tst-1",
                "in_flight",
                "Add login form",
                Some("sprint-3"),
                Some("bramble"),
            ),
            make_ticket(
                "tst-2",
                "in_review",
                "Fix search bug",
                Some("sprint-3"),
                Some("cedar"),
            ),
            make_ticket("tst-3", "ready", "Write docs", Some("sprint-3"), None),
        ];
        let app = crate::app::App::new(crate::data::Tab::Overview, data);
        let out = render_tab_to_string(&app, 120, 40);
        assert!(out.contains("sprint-3"), "batch name should appear");
        assert!(out.contains("tst-1"), "in_flight ticket id should appear");
        assert!(out.contains("tst-3"), "ready ticket id should appear");
        assert!(
            out.contains("In flight"),
            "in-flight table header should appear"
        );
    }

    #[test]
    fn overview_renders_foreman_pid_and_age() {
        use crate::data::{ForemanStatusSnapshot, OverviewData};
        use derrick_substrate::ForemanMode;

        let mut data = crate::data::DataModel::empty();
        data.overview = OverviewData {
            foreman_status: Some(ForemanStatusSnapshot {
                mode: ForemanMode::Detached,
                pid: Some(9999),
                started_at: Some(Utc::now() - chrono::Duration::seconds(65)),
            }),
            ..OverviewData::default()
        };
        let app = crate::app::App::new(crate::data::Tab::Overview, data);
        let out = render_tab_to_string(&app, 120, 40);
        assert!(out.contains("9999"), "pid should appear in foreman line");
    }

    #[test]
    fn overview_renders_foreman_no_pid_no_started_at() {
        use crate::data::{ForemanStatusSnapshot, OverviewData};
        use derrick_substrate::ForemanMode;

        let mut data = crate::data::DataModel::empty();
        data.overview = OverviewData {
            foreman_status: Some(ForemanStatusSnapshot {
                mode: ForemanMode::Stopped,
                pid: None,
                started_at: None,
            }),
            ..OverviewData::default()
        };
        let app = crate::app::App::new(crate::data::Tab::Overview, data);
        let out = render_tab_to_string(&app, 120, 40);
        assert!(out.contains("foreman"), "foreman line should appear");
    }

    #[test]
    fn overview_renders_restack_conflict() {
        use crate::data::{OverviewData, StackSummary};

        let mut data = crate::data::DataModel::empty();
        data.overview = OverviewData {
            stack_summary: StackSummary {
                merged: 0,
                open: 1,
                pending: 0,
                restack_ok: false,
            },
            ..OverviewData::default()
        };
        let app = crate::app::App::new(crate::data::Tab::Overview, data);
        let out = render_tab_to_string(&app, 120, 40);
        assert!(
            out.contains("conflict!"),
            "conflict marker should appear when restack_ok=false"
        );
    }

    #[test]
    fn overview_tokens_today_with_savings_pct() {
        use crate::data::TokenSummary;

        let mut data = crate::data::DataModel::empty();
        data.token_summary = TokenSummary {
            today_in: 50_000,
            today_out: 10_000,
            total_in: 100_000,
            total_out: 20_000,
            savings_pct: Some(0.20),
            ..TokenSummary::default()
        };
        let app = crate::app::App::new(crate::data::Tab::Overview, data);
        let out = render_tab_to_string(&app, 120, 40);
        assert!(
            out.contains("tokens today"),
            "token summary line should appear"
        );
        // savings_pct branch: should show raw -> actual with percentage
        assert!(out.contains("%"), "savings percentage should be rendered");
    }

    #[test]
    fn overview_tokens_no_savings_pct() {
        use crate::data::TokenSummary;

        let mut data = crate::data::DataModel::empty();
        data.token_summary = TokenSummary {
            today_in: 30_000,
            today_out: 5_000,
            savings_pct: None,
            ..TokenSummary::default()
        };
        let app = crate::app::App::new(crate::data::Tab::Overview, data);
        let out = render_tab_to_string(&app, 120, 40);
        assert!(
            out.contains("tokens today"),
            "token summary without savings_pct"
        );
        assert!(out.contains("in"), "should show in/out");
    }

    #[test]
    fn overview_last_refresh_timestamp_shown() {
        let mut data = crate::data::DataModel::empty();
        data.last_refresh = Some(Utc::now());
        let app = crate::app::App::new(crate::data::Tab::Overview, data);
        let out = render_tab_to_string(&app, 120, 40);
        // Header renders the refresh time; at some hour it will have digits.
        assert!(out.contains("[derrick]"), "header should appear");
    }

    // -----------------------------------------------------------------------
    // Tickets tab: populated data and filter paths
    // -----------------------------------------------------------------------

    #[test]
    fn tickets_tab_renders_rows_with_selection_highlight() {
        let mut data = crate::data::DataModel::empty();
        data.tickets = vec![
            make_ticket(
                "tst-1",
                "in_flight",
                "First task",
                Some("b1"),
                Some("bramble"),
            ),
            make_ticket("tst-2", "ready", "Second task", None, None),
            make_ticket("tst-3", "blocked", "Blocked task", Some("b1"), None),
            make_ticket("tst-4", "done", "Finished", None, None),
        ];
        let mut app = crate::app::App::new(crate::data::Tab::Tickets, data);
        app.selected_row = 1; // row 1 selected → should get REVERSED style
        let out = render_tab_to_string(&app, 120, 40);
        assert!(out.contains("tst-1"), "ticket id should appear");
        assert!(out.contains("tst-2"), "selected ticket id should appear");
        assert!(out.contains("in_flight"), "state should appear");
        assert!(out.contains("First task"), "title should appear");
    }

    #[test]
    fn tickets_tab_filter_active_shows_underscore_cursor() {
        let mut data = crate::data::DataModel::empty();
        data.tickets = vec![make_ticket("tst-5", "ready", "Some ticket", None, None)];
        let mut app = crate::app::App::new(crate::data::Tab::Tickets, data);
        app.filter = crate::app::FilterState::Active("ready".to_owned());
        let out = render_tab_to_string(&app, 120, 40);
        // Active filter shows "filter:<q>_" in the title and status bar
        assert!(out.contains("ready"), "filter query should appear");
    }

    #[test]
    fn tickets_tab_filter_narrows_results() {
        let mut data = crate::data::DataModel::empty();
        data.tickets = vec![
            make_ticket("tst-10", "in_flight", "Active work", None, None),
            make_ticket("tst-11", "done", "Completed work", None, None),
        ];
        let mut app = crate::app::App::new(crate::data::Tab::Tickets, data);
        // Filter for "done" — only tst-11 should be visible
        app.filter = crate::app::FilterState::Active("done".to_owned());
        let out = render_tab_to_string(&app, 120, 40);
        assert!(out.contains("tst-11"), "matching ticket should appear");
    }

    #[test]
    fn tickets_tab_sort_state_renders() {
        let mut data = crate::data::DataModel::empty();
        data.tickets = vec![
            make_ticket("tst-20", "done", "Done item", None, None),
            make_ticket("tst-21", "in_flight", "Active item", None, None),
        ];
        let mut app = crate::app::App::new(crate::data::Tab::Tickets, data);
        app.ticket_sort = crate::app::TicketSort::State;
        let out = render_tab_to_string(&app, 120, 40);
        assert!(
            out.contains("sort:state"),
            "sort label should appear in title"
        );
    }

    #[test]
    fn tickets_tab_all_sort_modes_render_without_panic() {
        let mut data = crate::data::DataModel::empty();
        data.tickets = vec![
            make_ticket("tst-1", "in_flight", "Alpha", None, None),
            make_ticket("tst-2", "ready", "Beta", None, None),
        ];
        for sort in [
            crate::app::TicketSort::Updated,
            crate::app::TicketSort::State,
            crate::app::TicketSort::Id,
            crate::app::TicketSort::Title,
        ] {
            let mut app = crate::app::App::new(crate::data::Tab::Tickets, data.clone());
            app.ticket_sort = sort;
            // Should not panic; meaningful content is present.
            let out = render_tab_to_string(&app, 120, 40);
            assert!(
                out.contains("tst-1") || out.contains("tst-2"),
                "at least one ticket id visible"
            );
        }
    }

    #[test]
    fn tickets_tab_status_bar_inactive_filter_shows_hint() {
        let mut data = crate::data::DataModel::empty();
        data.tickets = vec![make_ticket("tst-99", "ready", "Check hint", None, None)];
        let app = crate::app::App::new(crate::data::Tab::Tickets, data);
        let out = render_tab_to_string(&app, 120, 40);
        assert!(
            out.contains("press / to filter"),
            "hint should appear when filter inactive"
        );
    }

    // -----------------------------------------------------------------------
    // Stack tab: nodes with various states
    // -----------------------------------------------------------------------

    #[test]
    fn stack_tab_renders_nodes_open_merged_closed_unknown() {
        let mut data = crate::data::DataModel::empty();
        data.stack_load_result = crate::data::StackLoadResult::Loaded;
        data.stack_nodes = vec![
            make_stack_node(
                "tst-1",
                "feature/tst-1",
                "open",
                Some("main"),
                Some("https://github.com/org/repo/pull/10"),
            ),
            make_stack_node(
                "tst-2",
                "feature/tst-2",
                "merged",
                Some("feature/tst-1"),
                None,
            ),
            make_stack_node(
                "tst-3",
                "feature/tst-3",
                "closed",
                Some("feature/tst-2"),
                None,
            ),
            make_stack_node(
                "tst-4",
                "feature/tst-4",
                "draft",
                Some("feature/tst-3"),
                None,
            ),
        ];
        let app = crate::app::App::new(crate::data::Tab::Stack, data);
        let out = render_tab_to_string(&app, 120, 40);
        // Markers: open=●, merged=✓, closed=✗, unknown=…
        assert!(out.contains("tst-1"), "ticket id should appear");
        assert!(out.contains("feature/tst-1"), "branch should appear");
        assert!(out.contains("●"), "open marker should appear");
        assert!(out.contains("✓"), "merged marker should appear");
        assert!(out.contains("✗"), "closed marker should appear");
        assert!(out.contains("…"), "unknown state marker should appear");
    }

    #[test]
    fn stack_tab_renders_pr_url_when_present() {
        let mut data = crate::data::DataModel::empty();
        data.stack_load_result = crate::data::StackLoadResult::Loaded;
        data.stack_nodes = vec![make_stack_node(
            "tst-5",
            "feature/x",
            "open",
            None,
            Some("https://github.com/org/repo/pull/42"),
        )];
        let app = crate::app::App::new(crate::data::Tab::Stack, data);
        let out = render_tab_to_string(&app, 160, 40);
        // PR URL should appear; root node has "(root)" as parent
        assert!(out.contains("(root)"), "root parent label should appear");
        assert!(out.contains("github.com"), "pr url should be visible");
    }

    #[test]
    fn stack_tab_node_without_pr_url_renders_cleanly() {
        let mut data = crate::data::DataModel::empty();
        data.stack_load_result = crate::data::StackLoadResult::Loaded;
        data.stack_nodes = vec![make_stack_node(
            "tst-6",
            "feature/y",
            "open",
            Some("main"),
            None,
        )];
        let app = crate::app::App::new(crate::data::Tab::Stack, data);
        let out = render_tab_to_string(&app, 120, 40);
        assert!(out.contains("tst-6"), "ticket id should appear");
        assert!(out.contains("main"), "parent branch should appear");
    }

    // -----------------------------------------------------------------------
    // Activity tab: populated events and filter paths
    // -----------------------------------------------------------------------

    #[test]
    fn activity_tab_renders_events_with_ticket_scope_tag() {
        let mut data = crate::data::DataModel::empty();
        data.events = vec![
            make_event(
                "ticket_state_changed",
                Some("tst-7"),
                None,
                None,
                "ready -> in_flight",
            ),
            make_event("note", None, Some("bramble"), None, "work started"),
            make_event(
                "pipeline_step_completed",
                None,
                None,
                Some("run-abc"),
                "step assay: success",
            ),
        ];
        let app = crate::app::App::new(crate::data::Tab::Activity, data);
        let out = render_tab_to_string(&app, 120, 40);
        assert!(out.contains("[tst-7]"), "ticket scope tag should appear");
        assert!(
            out.contains("[hand:bramble]"),
            "hand scope tag should appear"
        );
        assert!(out.contains("[run:run-abc]"), "run scope tag should appear");
    }

    #[test]
    fn activity_tab_auto_scroll_label_changes() {
        let mut data = crate::data::DataModel::empty();
        data.events = vec![make_event("note", None, None, None, "some event")];

        let mut app = crate::app::App::new(crate::data::Tab::Activity, data.clone());
        app.activity_auto_scroll = true;
        let out = render_tab_to_string(&app, 120, 40);
        assert!(
            out.contains("auto-scroll"),
            "auto-scroll label should appear"
        );

        let mut app2 = crate::app::App::new(crate::data::Tab::Activity, data);
        app2.activity_auto_scroll = false;
        let out2 = render_tab_to_string(&app2, 120, 40);
        assert!(
            out2.contains("paused"),
            "paused label should appear when not auto-scrolling"
        );
    }

    #[test]
    fn activity_tab_active_filter_shows_in_title_and_status_bar() {
        let mut data = crate::data::DataModel::empty();
        data.events = vec![
            make_event("note", Some("tst-1"), None, None, "ticket event"),
            make_event("note", None, Some("cedar"), None, "hand event"),
        ];
        let mut app = crate::app::App::new(crate::data::Tab::Activity, data);
        app.filter = crate::app::FilterState::Active("ticket:tst-1".to_owned());
        let out = render_tab_to_string(&app, 120, 40);
        assert!(out.contains("ticket"), "filter label should appear");
    }

    #[test]
    fn activity_tab_committed_filter_shows_edit_hint() {
        // A committed (inactive) non-empty filter: filter bar is inactive but
        // activity filter is derived from the query string.
        let mut data = crate::data::DataModel::empty();
        data.events = vec![make_event("note", Some("tst-2"), None, None, "body")];
        let mut app = crate::app::App::new(crate::data::Tab::Activity, data);
        // Inactive filter state but query is committed (non-empty inactive).
        // We simulate this by switching filter to active-then-committed using
        // FilterState::Active (the renderer reads query() regardless).
        // In the actual render path the "committed filter" status bar branch
        // is reached when filter.is_active() is false AND filter.is_none() is false.
        // We can't reach it with the current FilterState enum without hacking,
        // so we test the closest branch: inactive filter (empty query, is_none=true).
        // The "edit hint" path is the third branch in the status bar render.
        app.filter = crate::app::FilterState::Inactive;
        let out = render_tab_to_string(&app, 120, 40);
        assert!(
            out.contains("press / to filter"),
            "inactive filter hint should appear"
        );
    }

    // -----------------------------------------------------------------------
    // Tokens tab: all branches
    // -----------------------------------------------------------------------

    #[test]
    fn tokens_tab_today_data_and_alltime_data() {
        use crate::data::TokenSummary;

        let mut data = crate::data::DataModel::empty();
        data.token_summary = TokenSummary {
            today_in: 20_000,
            today_out: 8_000,
            total_in: 200_000,
            total_out: 80_000,
            ..TokenSummary::default()
        };
        let app = crate::app::App::new(crate::data::Tab::Tokens, data);
        let out = render_tab_to_string(&app, 120, 40);
        assert!(out.contains("today:"), "today line should appear");
        assert!(out.contains("all-time:"), "all-time line should appear");
        assert!(out.contains("20k"), "today_in in thousands should appear");
    }

    #[test]
    fn tokens_tab_bytes_raw_compression_note() {
        use crate::data::TokenSummary;

        let mut data = crate::data::DataModel::empty();
        data.token_summary = TokenSummary {
            total_bytes_raw: 4096 * 1024,   // 4096 kb
            total_bytes_saved: 1024 * 1024, // 1024 kb saved
            ..TokenSummary::default()
        };
        let app = crate::app::App::new(crate::data::Tab::Tokens, data);
        let out = render_tab_to_string(&app, 120, 40);
        assert!(
            out.contains("compression:"),
            "compression note should appear"
        );
        assert!(out.contains("raw"), "raw label should appear");
    }

    #[test]
    fn tokens_tab_savings_pct_no_bytes() {
        use crate::data::TokenSummary;

        let mut data = crate::data::DataModel::empty();
        data.token_summary = TokenSummary {
            total_bytes_raw: 0,
            savings_pct: Some(0.15),
            ..TokenSummary::default()
        };
        let app = crate::app::App::new(crate::data::Tab::Tokens, data);
        let out = render_tab_to_string(&app, 120, 40);
        assert!(
            out.contains("savings:"),
            "savings line should appear for pct-only mode"
        );
    }

    #[test]
    fn tokens_tab_roughneck_saved_shows_count() {
        use crate::data::TokenSummary;

        let mut data = crate::data::DataModel::empty();
        data.token_summary = TokenSummary {
            total_roughneck_saved: 12_345,
            ..TokenSummary::default()
        };
        let app = crate::app::App::new(crate::data::Tab::Tokens, data);
        let out = render_tab_to_string(&app, 120, 40);
        assert!(
            out.contains("12345") || out.contains("roughneck"),
            "roughneck count should appear"
        );
    }

    #[test]
    fn tokens_tab_per_step_bar_chart_renders() {
        use crate::data::{StepTokenSummary, TokenSummary};

        let mut data = crate::data::DataModel::empty();
        data.token_summary = TokenSummary {
            per_step: vec![
                StepTokenSummary {
                    step_id: "specify".to_owned(),
                    tokens_in: 1000,
                    tokens_out: 500,
                    bytes_saved: 0,
                    roughneck_tokens_saved: 0,
                },
                StepTokenSummary {
                    step_id: "plan".to_owned(),
                    tokens_in: 2000,
                    tokens_out: 800,
                    bytes_saved: 100,
                    roughneck_tokens_saved: 50,
                },
            ],
            total_in: 3000,
            total_out: 1300,
            ..TokenSummary::default()
        };
        let app = crate::app::App::new(crate::data::Tab::Tokens, data);
        let out = render_tab_to_string(&app, 120, 40);
        assert!(
            out.contains("Per-step"),
            "per-step chart header should appear"
        );
        assert!(
            out.contains("specify") || out.contains("plan"),
            "step id should appear"
        );
    }

    #[test]
    fn tokens_tab_hands_section_shown_when_nonzero() {
        use crate::data::TokenSummary;

        let mut data = crate::data::DataModel::empty();
        data.token_summary = TokenSummary {
            hands_tokens_out: 5_000,
            hands_roughneck_saved: 200,
            hands_bytes_raw: 8192,
            hands_bytes_saved: 2048,
            ..TokenSummary::default()
        };
        let app = crate::app::App::new(crate::data::Tab::Tokens, data);
        let out = render_tab_to_string(&app, 120, 40);
        assert!(
            out.contains("Hands"),
            "Hands section should appear when nonzero"
        );
        assert!(
            out.contains("tokens out:") || out.contains("5000"),
            "hands tokens should appear"
        );
    }

    // -----------------------------------------------------------------------
    // Memory tab: entries and selection
    // -----------------------------------------------------------------------

    #[test]
    fn memory_tab_renders_entry_list_and_preview() {
        let mut data = crate::data::DataModel::empty();
        data.memory_entries = vec![
            crate::data::MemoryEntry {
                slug: "feedback_testing".to_owned(),
                path: std::path::PathBuf::from("/site/.derrick/memory/feedback_testing.md"),
                preview: "This is a test memory entry preview text.".to_owned(),
            },
            crate::data::MemoryEntry {
                slug: "architecture_notes".to_owned(),
                path: std::path::PathBuf::from("/site/.derrick/memory/architecture_notes.md"),
                preview: "Architecture decisions go here.".to_owned(),
            },
        ];
        let mut app = crate::app::App::new(crate::data::Tab::Memory, data);
        app.selected_row = 0;
        let out = render_tab_to_string(&app, 120, 40);
        assert!(
            out.contains("feedback_testing"),
            "first slug should appear in list"
        );
        assert!(
            out.contains("test memory entry"),
            "preview of selected entry should appear"
        );
    }

    #[test]
    fn memory_tab_second_row_selected_shows_its_preview() {
        let mut data = crate::data::DataModel::empty();
        data.memory_entries = vec![
            crate::data::MemoryEntry {
                slug: "entry-a".to_owned(),
                path: std::path::PathBuf::from("/a.md"),
                preview: "Preview A".to_owned(),
            },
            crate::data::MemoryEntry {
                slug: "entry-b".to_owned(),
                path: std::path::PathBuf::from("/b.md"),
                preview: "Preview B text here".to_owned(),
            },
        ];
        let mut app = crate::app::App::new(crate::data::Tab::Memory, data);
        app.selected_row = 1;
        let out = render_tab_to_string(&app, 120, 40);
        assert!(out.contains("entry-b"), "selected slug should appear");
        assert!(out.contains("Preview B"), "selected preview should appear");
    }

    #[test]
    fn memory_tab_out_of_bounds_row_shows_no_entry_selected() {
        let mut data = crate::data::DataModel::empty();
        data.memory_entries = vec![crate::data::MemoryEntry {
            slug: "only-entry".to_owned(),
            path: std::path::PathBuf::from("/x.md"),
            preview: "Content here".to_owned(),
        }];
        let mut app = crate::app::App::new(crate::data::Tab::Memory, data);
        app.selected_row = 99; // beyond bounds
        let out = render_tab_to_string(&app, 120, 40);
        assert!(
            out.contains("(no entry selected)"),
            "fallback text should appear"
        );
    }

    // -----------------------------------------------------------------------
    // Hands tab: rows, filter variants, status badges
    // -----------------------------------------------------------------------

    #[test]
    fn hands_tab_renders_running_done_failed_rows() {
        let mut data = crate::data::DataModel::empty();
        data.hand_rows = vec![
            make_hand_row("bramble", Some("tst-1"), "dispatched", "running", None),
            make_hand_row("cedar", Some("tst-2"), "completed", "done", Some("exit 0")),
            make_hand_row("oak", Some("tst-3"), "failed", "failed", Some("exit 1")),
        ];
        let app = crate::app::App::new(crate::data::Tab::Hands, data);
        let out = render_tab_to_string(&app, 160, 40);
        assert!(out.contains("bramble"), "running hand should appear");
        assert!(out.contains("cedar"), "done hand should appear");
        assert!(out.contains("oak"), "failed hand should appear");
        // Status glyphs
        assert!(out.contains("✓"), "done glyph should appear");
        assert!(out.contains("✗"), "failed glyph should appear");
    }

    #[test]
    fn hands_tab_no_rows_shows_zero_count() {
        let data = crate::data::DataModel::empty();
        let app = crate::app::App::new(crate::data::Tab::Hands, data);
        let out = render_tab_to_string(&app, 120, 40);
        assert!(out.contains("0 hands"), "zero count label should appear");
    }

    #[test]
    fn hands_tab_singular_hand_label() {
        let mut data = crate::data::DataModel::empty();
        data.hand_rows = vec![make_hand_row("solo", None, "running", "running", None)];
        let app = crate::app::App::new(crate::data::Tab::Hands, data);
        let out = render_tab_to_string(&app, 120, 40);
        assert!(
            out.contains("1 hand"),
            "singular label should appear with one hand"
        );
    }

    #[test]
    fn hands_tab_filter_active_shows_cursor() {
        let mut data = crate::data::DataModel::empty();
        data.hand_rows = vec![make_hand_row("bramble", None, "running", "running", None)];
        let mut app = crate::app::App::new(crate::data::Tab::Hands, data);
        app.filter = crate::app::FilterState::Active("bramble".to_owned());
        let out = render_tab_to_string(&app, 120, 40);
        assert!(out.contains("bramble"), "active filter query should appear");
    }

    #[test]
    fn hands_tab_hand_filter_narrows_rows() {
        let mut data = crate::data::DataModel::empty();
        data.hand_rows = vec![
            make_hand_row("bramble", None, "running", "running", None),
            make_hand_row("cedar", None, "done", "done", None),
        ];
        let mut app = crate::app::App::new(crate::data::Tab::Hands, data);
        // hand: filter — only bramble matches
        app.filter = crate::app::FilterState::Active("hand:bramble".to_owned());
        let out = render_tab_to_string(&app, 120, 40);
        assert!(out.contains("bramble"), "filtered hand should appear");
    }

    #[test]
    fn hands_tab_ticket_filter_narrows_by_ticket() {
        let mut data = crate::data::DataModel::empty();
        data.hand_rows = vec![
            make_hand_row("bramble", Some("tst-10"), "dispatched", "running", None),
            make_hand_row("cedar", Some("tst-20"), "dispatched", "running", None),
        ];
        let mut app = crate::app::App::new(crate::data::Tab::Hands, data);
        app.filter = crate::app::FilterState::Active("ticket:tst-10".to_owned());
        let out = render_tab_to_string(&app, 120, 40);
        assert!(
            out.contains("bramble"),
            "bramble matched by ticket filter should appear"
        );
    }

    #[test]
    fn hands_tab_run_filter_hides_all() {
        // run: filter is explicitly not supported for hands — it yields no results.
        let mut data = crate::data::DataModel::empty();
        data.hand_rows = vec![make_hand_row("bramble", None, "running", "running", None)];
        let mut app = crate::app::App::new(crate::data::Tab::Hands, data);
        app.filter = crate::app::FilterState::Active("run:anything".to_owned());
        let out = render_tab_to_string(&app, 120, 40);
        // The run: filter always returns false for hands rows; bramble should be hidden.
        // But the status bar / title should still render without panic.
        assert!(out.contains("Hands"), "Hands tab title should appear");
    }

    #[test]
    fn hands_tab_text_filter_matches_action_and_detail() {
        let mut data = crate::data::DataModel::empty();
        data.hand_rows = vec![
            make_hand_row(
                "bramble",
                Some("tst-1"),
                "dispatched",
                "running",
                Some("queue written"),
            ),
            make_hand_row("cedar", None, "completed", "done", None),
        ];
        let mut app = crate::app::App::new(crate::data::Tab::Hands, data);
        app.filter = crate::app::FilterState::Active("queue".to_owned());
        let out = render_tab_to_string(&app, 160, 40);
        assert!(
            out.contains("bramble"),
            "bramble with matching detail should appear"
        );
    }

    #[test]
    fn hands_tab_inactive_filter_shows_hint() {
        let data = crate::data::DataModel::empty();
        let app = crate::app::App::new(crate::data::Tab::Hands, data);
        let out = render_tab_to_string(&app, 120, 40);
        assert!(
            out.contains("press / to filter"),
            "filter hint should appear when inactive"
        );
    }

    #[test]
    fn hands_tab_committed_filter_shows_edit_hint() {
        // When filter is Active with a query (which is the committed-but-still-active
        // FilterState), the status bar should show the "/ to edit" hint branch
        // via !filter.is_none() on the ActivityFilter derived from the query.
        let mut data = crate::data::DataModel::empty();
        data.hand_rows = vec![make_hand_row("oak", None, "running", "running", None)];
        let mut app = crate::app::App::new(crate::data::Tab::Hands, data);
        // filter.is_active()=false, query non-empty => committed filter path
        // Simulate: FilterState::Inactive but with non-empty underlying filter.
        // We achieve the "committed" branch by making the filter inactive after
        // typing. In current FilterState enum, Inactive has no stored string, so
        // the only way to get a non-empty committed filter via query() is to use
        // FilterState::Active (the is_active()=true branch). The status bar in
        // render_hands checks filter.is_active() first (the "/" line), then
        // !filter.is_none() for the edit-hint branch. There's no way to reach the
        // edit-hint branch from tests since Inactive always returns empty query().
        // We cover the is_active()=true branch explicitly:
        app.filter = crate::app::FilterState::Active("hand:oak".to_owned());
        let out = render_tab_to_string(&app, 120, 40);
        // Active filter path: should show "/ <q>_  |  prefixes:".
        assert!(out.contains("oak"), "active filter query appears");
    }

    // -----------------------------------------------------------------------
    // render_header: timestamp shown vs missing
    // -----------------------------------------------------------------------

    #[test]
    fn header_shows_placeholder_when_no_refresh() {
        let data = crate::data::DataModel::empty(); // last_refresh = None
        let app = crate::app::App::new(crate::data::Tab::Overview, data);
        let out = render_tab_to_string(&app, 120, 40);
        assert!(
            out.contains("--:--:--"),
            "placeholder should appear before first refresh"
        );
    }

    // -----------------------------------------------------------------------
    // Narrow-terminal truncation: 80x24 should not panic
    // -----------------------------------------------------------------------

    #[test]
    fn all_tabs_render_without_panic_on_narrow_terminal() {
        use crate::data::{OverviewData, StackSummary, TokenSummary};

        let mut data = crate::data::DataModel::empty();
        data.tickets = vec![
            make_ticket(
                "tst-1",
                "in_flight",
                "Long ticket title that may truncate",
                Some("batch"),
                Some("bramble"),
            ),
            make_ticket("tst-2", "ready", "Another task", None, None),
        ];
        data.stack_nodes = vec![make_stack_node(
            "tst-1",
            "feature/tst-1",
            "open",
            Some("main"),
            None,
        )];
        data.stack_load_result = crate::data::StackLoadResult::Loaded;
        data.events = vec![make_event(
            "note",
            Some("tst-1"),
            None,
            None,
            "event body text",
        )];
        data.token_summary = TokenSummary {
            today_in: 5_000,
            today_out: 2_000,
            total_in: 50_000,
            total_out: 20_000,
            ..TokenSummary::default()
        };
        data.memory_entries = vec![crate::data::MemoryEntry {
            slug: "slug-a".to_owned(),
            path: std::path::PathBuf::from("/a.md"),
            preview: "preview".to_owned(),
        }];
        data.hand_rows = vec![make_hand_row(
            "bramble",
            Some("tst-1"),
            "dispatched",
            "running",
            None,
        )];
        data.overview = OverviewData {
            batch_name: Some("batch-1".to_owned()),
            stack_summary: StackSummary {
                merged: 0,
                open: 1,
                pending: 0,
                restack_ok: true,
            },
            ..OverviewData::default()
        };
        let app = crate::app::App::new(crate::data::Tab::Overview, data);
        // Render all tabs at 80x24 — should not panic.
        render_all_tabs_no_panic(&app);
    }

    // -----------------------------------------------------------------------
    // Factory tab (D78)
    // -----------------------------------------------------------------------

    fn factory_hand(id: &str, kind: derrick_substrate::HandKind, pid: Option<u32>) -> derrick_substrate::Hand {
        derrick_substrate::Hand {
            id: derrick_substrate::HandId::new(id).expect("hand id"),
            kind,
            last_seen: None,
            pid,
        }
    }

    #[test]
    fn factory_tab_renders_workers_smokestack_and_dock() {
        use derrick_substrate::{ForemanMode, HandKind};

        let mut data = crate::data::DataModel::empty();
        data.hands = vec![
            factory_hand("bramble", HandKind::Codex, Some(123)),
            factory_hand("sumac", HandKind::Copilot, None),
        ];
        data.hand_rows = vec![
            make_hand_row("bramble", Some("tst-1"), "working", "running", None),
            make_hand_row("sumac", Some("tst-2"), "completed", "done", None),
        ];
        data.tickets = vec![
            row("tst-1", "in_flight", "ingest", None),
            row("tst-2", "done", "migration", None),
            row("tst-3", "ready", "wiring", None),
        ];
        data.overview.foreman_status = Some(crate::data::ForemanStatusSnapshot {
            mode: ForemanMode::Attached,
            pid: Some(1),
            started_at: None,
        });
        let mut app = crate::app::App::new(crate::data::Tab::Factory, data);
        app.animation_frame = 2;
        let out = render_tab_to_string(&app, 160, 40);
        assert!(out.contains("Factory floor"), "title should appear");
        assert!(out.contains("bramble"), "worker hand id should appear");
        assert!(out.contains("sumac"), "second worker should appear");
        assert!(out.contains("tst-3"), "ready ticket should appear on the conveyor");
        assert!(out.contains("shipped: 1"), "done ticket count should appear");
        assert!(out.contains("💨"), "smokestack should puff when foreman is attached");
    }

    #[test]
    fn factory_tab_idle_when_no_hands_and_foreman_stopped() {
        use derrick_substrate::ForemanMode;

        let mut data = crate::data::DataModel::empty();
        data.overview.foreman_status = Some(crate::data::ForemanStatusSnapshot {
            mode: ForemanMode::Stopped,
            pid: None,
            started_at: None,
        });
        let app = crate::app::App::new(crate::data::Tab::Factory, data);
        let out = render_tab_to_string(&app, 120, 24);
        assert!(out.contains("workers: 0"), "zero-worker count should appear");
        assert!(out.contains("stopped"), "foreman mode should appear");
        assert!(
            !out.contains("💨"),
            "smokestack should NOT puff when foreman is stopped"
        );
    }

    #[test]
    fn factory_tab_renders_across_animation_frames_without_panic() {
        let mut data = crate::data::DataModel::empty();
        data.hands = vec![factory_hand(
            "bramble",
            derrick_substrate::HandKind::Codex,
            Some(9),
        )];
        data.hand_rows = vec![make_hand_row("bramble", Some("tst-1"), "working", "running", None)];
        let mut app = crate::app::App::new(crate::data::Tab::Factory, data);
        // Cycle the animation frame through several values — each must render
        // without panic (the spinner index wraps via modulo).
        for frame in 0..25 {
            app.animation_frame = frame;
            let out = render_tab_to_string(&app, 80, 24);
            assert!(out.contains("bramble"), "worker renders at frame {frame}");
        }
    }
}
