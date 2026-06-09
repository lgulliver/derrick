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
    let hints = "q quit  r refresh  ↑↓ scroll  ⏎ detail  / filter  ? help  Esc back  1-7 tabs";
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
            Paragraph::new("(no step data — run `derrick add` to generate token records)").block(
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
            Constraint::Length(12),
            Constraint::Length(16),
            Constraint::Length(8),
            Constraint::Min(10),
        ],
    )
    .header(
        Row::new(vec!["", "hand", "ticket", "action", "age", "detail"])
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

fn render_help_overlay(frame: &mut Frame, area: Rect) {
    let body = "Keys:\n\
        q     quit\n\
        r     refresh\n\
        1-7   switch tab\n\
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

#[cfg(test)]
mod tests {
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
}
