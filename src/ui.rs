//! Rendering. Pure: reads `App`, writes to a frame.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use crate::app::{fit_names, short, App, Mode, Pane};
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
            format!("{} selected", app.targets.len()),
            Style::new().fg(Color::Magenta),
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
    // The details pane only earns its space when there is room left for a
    // usable list above it.
    let show_details = area.height >= 14;
    let panes = if show_details {
        Layout::vertical([Constraint::Min(6), Constraint::Length(8)]).split(area)
    } else {
        Layout::vertical([Constraint::Min(1)]).split(area)
    };

    let cols = Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(panes[0]);

    // --- groups ---
    let visible = app.visible();
    let items: Vec<ListItem> = visible
        .iter()
        .map(|&i| {
            let g = &app.groups[i];
            let n = app.distinct_versions(g);
            let marked = app.target_for(i).is_some();
            let style = if app.is_divergent(g) {
                Style::new().fg(Color::Yellow)
            } else {
                Style::new().fg(Color::DarkGray)
            };
            ListItem::new(Line::from(vec![
                Span::raw(if marked { "● " } else { "  " }),
                Span::styled(format!("{:<40}", g.identity.to_string()), style),
                Span::raw(format!("{n} revs / {} pins", app.actionable_pins(g).count())),
            ]))
        })
        .collect();

    let title = if app.show_all {
        " inputs (all) "
    } else {
        " inputs (divergent) "
    };
    let groups_block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(pane_style(app, Pane::Groups));
    let groups_inner = groups_block.inner(cols[0]);
    f.render_widget(groups_block, cols[0]);

    let (groups_head, groups_body) = split_header(groups_inner);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("  {:<40}{}", "input", "revs / pins"),
            header_style(),
        ))),
        groups_head,
    );

    let mut state = ListState::default().with_selected(Some(app.group_idx));
    f.render_stateful_widget(
        List::new(items).highlight_style(Style::new().reversed()),
        groups_body,
        &mut state,
    );

    // --- versions of selection ---
    let gi = visible.get(app.group_idx).copied();
    let target = gi.and_then(|i| app.target_for(i)).unwrap_or("");
    // With nothing selected by default, the recommendation still has to be
    // visible — otherwise every group looks equally arbitrary.
    let suggested = gi.and_then(|i| app.suggested_target(i)).unwrap_or("");

    let versions_block = Block::default()
        .borders(Borders::ALL)
        .title(" versions ")
        .border_style(pane_style(app, Pane::Versions));
    let versions_inner = versions_block.inner(cols[1]);
    f.render_widget(versions_block, cols[1]);

    let (versions_head, versions_body) = split_header(versions_inner);

    // Fixed columns: marker(2) + rev(14) + date(12) + count(5). Whatever is
    // left over is what the project names get to use.
    const FIXED: usize = 2 + 14 + 12 + 5;
    let names_width = (versions_inner.width as usize).saturating_sub(FIXED);

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(
                "  {:<14}{:<12}{:>4} {}",
                "revision", "date", "n", "projects"
            ),
            header_style(),
        ))),
        versions_head,
    );

    let rows: Vec<ListItem> = app
        .version_rows()
        .into_iter()
        .map(|row| {
            let is_target = row.version == target;
            let is_suggested = !is_target && row.version == suggested;
            let when = row
                .last_modified
                .map(format_date)
                .unwrap_or_else(|| "?".into());
            let (marker, marker_style) = if is_target {
                ("→ ", Style::new().fg(Color::Green).bold())
            } else if is_suggested {
                ("· ", Style::new().fg(Color::DarkGray))
            } else {
                ("  ", Style::new())
            };
            ListItem::new(Line::from(vec![
                Span::styled(marker, marker_style),
                Span::styled(
                    format!("{:<14}", short(&row.version)),
                    Style::new().fg(Color::White),
                ),
                Span::styled(format!("{when:<12}"), Style::new().fg(Color::DarkGray)),
                Span::styled(format!("{:>4} ", row.count()), Style::new().fg(Color::Cyan)),
                Span::raw(fit_names(&row.project_names(), names_width)),
            ]))
        })
        .collect();

    let mut vstate = ListState::default().with_selected(Some(app.version_idx));
    f.render_stateful_widget(
        List::new(rows).highlight_style(Style::new().reversed()),
        versions_body,
        &mut vstate,
    );

    if show_details {
        draw_details(f, panes[1], app);
    }
}

/// Split a pane's interior into a one-row heading and the body below it.
fn split_header(inner: Rect) -> (Rect, Rect) {
    let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(inner);
    (parts[0], parts[1])
}

fn header_style() -> Style {
    Style::new().fg(Color::DarkGray).bold()
}

/// Details for the highlighted version: the full project list, unabbreviated.
fn draw_details(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" details ")
        .border_style(pane_style(app, Pane::Details));

    let (Some(group), Some(row)) = (app.selected_group(), app.selected_version_row()) else {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "no selection",
                Style::new().fg(Color::DarkGray),
            )))
            .block(block),
            area,
        );
        return;
    };

    let when = row
        .last_modified
        .map(format_date)
        .unwrap_or_else(|| "unknown".into());

    let mut lines = vec![
        Line::from(vec![
            Span::styled(group.identity.to_string(), Style::new().bold().fg(Color::Yellow)),
            Span::raw("  "),
            Span::styled(row.version.clone(), Style::new().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("locked ", Style::new().fg(Color::DarkGray)),
            Span::raw(when),
            Span::styled("   pinned by ", Style::new().fg(Color::DarkGray)),
            Span::styled(
                format!("{} input(s)", row.count()),
                Style::new().fg(Color::Cyan),
            ),
            Span::styled(
                format!(" in {} project(s)", row.project_names().len()),
                Style::new().fg(Color::Cyan),
            ),
        ]),
        Line::from(""),
    ];

    // Full paths, and the input name, since one project can pin an identity
    // twice under different attribute names.
    for pin in &row.pins {
        lines.push(Line::from(vec![
            Span::styled("  • ", Style::new().fg(Color::DarkGray)),
            Span::raw(pin.project.display().to_string()),
            Span::styled(
                format!("  [{}]", pin.input_name),
                Style::new().fg(Color::DarkGray),
            ),
        ]));
    }

    // Transitive pins are excluded from the plan; say so rather than letting
    // the pin count look inexplicably low next to the lockfiles on disk.
    let transitive = group.transitive_count();
    if transitive > 0 {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!(
                "+{transitive} transitive pin(s) of this input, not shown — \
                 they belong to a dependency's lockfile and cannot be overridden"
            ),
            Style::new().fg(Color::DarkGray).italic(),
        )));
    }

    let unrooted = app.unrooted_count(group);
    if app.rooted_only && unrooted > 0 {
        lines.push(Line::from(Span::styled(
            format!(
                "+{unrooted} pin(s) in projects with no GC root — \
                 converging them frees nothing (g to include)"
            ),
            Style::new().fg(Color::DarkGray).italic(),
        )));
    }

    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((app.details_scroll, 0))
            .block(block),
        area,
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
            "↑↓ move  Tab pane  Enter pick  x exclude  A/N all/none  a divergent  g rooted-only  s stage  q quit"
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
