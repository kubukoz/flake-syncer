//! Rendering. Pure: reads `App`, writes to a frame.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use crate::app::{short, App, Mode, Pane};
use crate::store::human_bytes;
use crate::sync;

pub fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(5),
        Constraint::Length(3),
    ])
    .split(f.area());

    draw_header(f, chunks[0], app);

    match app.mode {
        Mode::Browse => draw_browse(f, chunks[1], app),
        Mode::Confirm => draw_confirm(f, chunks[1], app),
        Mode::Report => draw_report(f, chunks[1], app),
    }

    draw_footer(f, chunks[2], app);
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let (bytes, paths) = app.estimated_savings();
    let line = Line::from(vec![
        Span::styled("flake-syncer", Style::new().bold().fg(Color::Cyan)),
        Span::raw("  "),
        Span::raw(format!("{} groups", app.groups.len())),
        Span::raw("  "),
        Span::styled(
            format!("{} divergent", app.divergent_count()),
            Style::new().fg(Color::Yellow),
        ),
        Span::raw("  "),
        Span::styled(
            format!("~{} reclaimable ({} paths)", human_bytes(bytes), paths),
            Style::new().fg(Color::Green),
        ),
    ]);
    // Unreadable lockfiles are surfaced rather than silently dropped: they make
    // the divergence counts an undercount.
    let line = if app.parse_errors.is_empty() {
        line
    } else {
        let mut spans = line.spans;
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("{} unreadable", app.parse_errors.len()),
            Style::new().fg(Color::Red),
        ));
        Line::from(spans)
    };
    f.render_widget(
        Paragraph::new(line).block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn draw_browse(f: &mut Frame, area: Rect, app: &App) {
    let cols =
        Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).split(area);

    // --- groups ---
    let visible = app.visible();
    let items: Vec<ListItem> = visible
        .iter()
        .map(|&i| {
            let g = &app.groups[i];
            let n = g.distinct_versions().len();
            let marked = app.target_for(i).is_some();
            let style = if g.is_divergent() {
                Style::new().fg(Color::Yellow)
            } else {
                Style::new().fg(Color::DarkGray)
            };
            ListItem::new(Line::from(vec![
                Span::raw(if marked { "● " } else { "  " }),
                Span::styled(format!("{:<40}", g.identity.to_string()), style),
                Span::raw(format!("{n} revs / {} pins", g.pins.len())),
            ]))
        })
        .collect();

    let title = if app.show_all {
        " inputs (all) "
    } else {
        " inputs (divergent) "
    };
    let mut state = ListState::default().with_selected(Some(app.group_idx));
    f.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .border_style(pane_style(app, Pane::Groups)),
            )
            .highlight_style(Style::new().reversed()),
        cols[0],
        &mut state,
    );

    // --- versions of selection ---
    let gi = visible.get(app.group_idx).copied();
    let target = gi.and_then(|i| app.target_for(i)).unwrap_or("");
    let rows: Vec<ListItem> = app
        .version_rows()
        .into_iter()
        .map(|(v, count, lm)| {
            let is_target = v == target;
            let when = lm
                .map(|t| format_date(t))
                .unwrap_or_else(|| "?".into());
            ListItem::new(Line::from(vec![
                Span::styled(
                    if is_target { "→ " } else { "  " },
                    Style::new().fg(Color::Green).bold(),
                ),
                Span::styled(format!("{:<14}", short(&v)), Style::new().fg(Color::White)),
                Span::styled(format!("{when:<12}"), Style::new().fg(Color::DarkGray)),
                Span::raw(format!("{count} project(s)")),
            ]))
        })
        .collect();

    let mut vstate = ListState::default().with_selected(Some(app.version_idx));
    f.render_stateful_widget(
        List::new(rows)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" versions ")
                    .border_style(pane_style(app, Pane::Versions)),
            )
            .highlight_style(Style::new().reversed()),
        cols[1],
        &mut vstate,
    );
}

fn draw_confirm(f: &mut Frame, area: Rect, app: &App) {
    let mut lines = vec![Line::from(Span::styled(
        format!("{} action(s) will run:", app.pending.len()),
        Style::new().bold(),
    ))];
    for a in app.pending.iter().take(area.height.saturating_sub(4) as usize) {
        lines.push(Line::from(Span::styled(
            format!("  {}", sync::command_line(a)),
            Style::new().fg(Color::Gray),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Enter = run   Esc = cancel",
        Style::new().fg(Color::Yellow),
    )));

    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title(" dry run ")),
        area,
    );
}

fn draw_report(f: &mut Frame, area: Rect, app: &App) {
    let lines: Vec<Line> = app
        .report
        .iter()
        .map(|l| {
            let style = if l.starts_with("ok") {
                Style::new().fg(Color::Green)
            } else if l.starts_with("fail") {
                Style::new().fg(Color::Red)
            } else {
                Style::new()
            };
            Line::from(Span::styled(l.clone(), style))
        })
        .collect();
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title(" results ")),
        area,
    );
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    let keys = match app.mode {
        Mode::Browse => {
            "↑↓ move  Tab pane  Enter pick version  x exclude  a all/divergent  s stage  q quit"
        }
        Mode::Confirm => "Enter run  Esc cancel  q quit",
        Mode::Report => "q quit",
    };
    let mut spans = vec![Span::styled(keys, Style::new().fg(Color::DarkGray))];
    if !app.status.is_empty() {
        spans.push(Span::raw("   "));
        spans.push(Span::styled(app.status.clone(), Style::new().fg(Color::Cyan)));
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)).block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn pane_style(app: &App, pane: Pane) -> Style {
    if app.pane == pane && app.mode == Mode::Browse {
        Style::new().fg(Color::Cyan)
    } else {
        Style::new().fg(Color::DarkGray)
    }
}

/// Render a unix timestamp as YYYY-MM-DD without pulling in a date crate.
fn format_date(ts: i64) -> String {
    let days = ts / 86_400;
    let (mut y, mut d) = (1970, days);
    loop {
        let len = if leap(y) { 366 } else { 365 };
        if d < len {
            break;
        }
        d -= len;
        y += 1;
    }
    let months = [
        31,
        if leap(y) { 29 } else { 28 },
        31, 30, 31, 30, 31, 31, 30, 31, 30, 31,
    ];
    let mut m = 0;
    while m < 12 && d >= months[m] {
        d -= months[m];
        m += 1;
    }
    format!("{y:04}-{:02}-{:02}", m + 1, d + 1)
}

fn leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}
