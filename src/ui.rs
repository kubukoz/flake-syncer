//! Rendering. Pure: reads `App`, writes to a frame.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use crate::app::{fit_names, short, App, Mode, Pane};
use crate::progress::human_duration;
use crate::store::human_bytes;
use crate::sync;

/// Width of the execution progress bar, matching the startup scan's.
const BAR_WIDTH: usize = 24;

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
        Mode::Running => draw_running(f, chunks[1], app),
        Mode::Report => draw_report(f, chunks[1], app),
        Mode::Roots => draw_roots(f, chunks[1], app),
    }

    draw_footer(f, chunks[2], app);
}

/// A byte figure, marked with `+` while sizing is still running.
///
/// Every unmeasured path can only add to the total, so a figure shown mid-scan
/// is a lower bound. The marker is what keeps it from reading as final — this
/// tool exists to not overstate savings, and an understated number presented as
/// settled is the same class of error.
fn provisional_bytes(app: &App, bytes: u64) -> String {
    if app.sizes_provisional() {
        format!("{}+", human_bytes(bytes))
    } else {
        human_bytes(bytes)
    }
}

/// One row's size, or a placeholder while that path is still being walked.
///
/// An unmeasured path holds 0 bytes as far as the struct is concerned, and
/// rendering that as `0 B` would claim an empty store path — indistinguishable
/// from a real one, and wrong. The placeholder says which it is.
fn row_size(app: &App, row: &crate::store::RootRow) -> String {
    if app.is_measured(&row.store_path) {
        human_bytes(row.size_bytes)
    } else {
        "—".to_string()
    }
}

/// The sizing count, as a span to place next to the figures it qualifies.
///
/// Spelled out so the `+` marker has a visible explanation rather than being a
/// symbol the user has to guess at — which means it has to be positioned where a
/// narrow terminal will not truncate it before the thing it explains.
fn sizing_span(app: &App) -> Option<Span<'static>> {
    let s = app.sizing?;
    Some(Span::styled(
        format!("sizing {}/{}", s.done, s.total),
        Style::new().fg(Color::DarkGray),
    ))
}

/// Append the sizing count to a header line that has nothing outranking it.
///
/// Where a header does carry lower-priority trailing spans, the count is placed
/// inline instead — a narrow terminal must not truncate the explanation of `+`
/// while keeping the `+` itself.
fn with_sizing_note<'a>(app: &App, line: Line<'a>) -> Line<'a> {
    let Some(note) = sizing_span(app) else {
        return line;
    };
    let mut spans = line.spans;
    spans.push(Span::raw("  "));
    spans.push(note);
    Line::from(spans)
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    // Mid-run the browse-mode counts describe a state the user has already left,
    // and `estimated_savings` is about a plan that is currently being applied.
    // The header becomes what the run is, instead.
    if let (Mode::Running, Some(r)) = (app.mode, &app.running) {
        let line = Line::from(vec![
            Span::styled("flake-syncer", Style::new().bold().fg(Color::Cyan)),
            Span::raw("  "),
            Span::styled("applying plan", Style::new().fg(Color::Yellow).bold()),
            Span::raw("  "),
            Span::raw(format!("{}/{} step(s)", r.done, r.total)),
            Span::raw("  "),
            Span::styled(
                human_duration(r.started.elapsed()),
                Style::new().fg(Color::DarkGray),
            ),
        ]);
        f.render_widget(
            Paragraph::new(line).block(Block::default().borders(Borders::ALL)),
            area,
        );
        return;
    }

    // The roots view is about store paths, not input groups, so the divergent
    // and selected counts would describe a screen the user is not looking at.
    if app.mode == Mode::Roots {
        let rows = app.root_rows();
        let freeable = rows.iter().filter(|r| r.freeable()).count();
        let line = Line::from(vec![
            Span::styled("flake-syncer", Style::new().bold().fg(Color::Cyan)),
            Span::raw("  "),
            Span::styled("gc roots", Style::new().fg(Color::Yellow).bold()),
            Span::raw("  "),
            Span::raw(format!("{} path(s)", rows.len())),
            Span::raw("  "),
            Span::styled(
                format!("{} held", provisional_bytes(app, app.roots_total_bytes())),
                Style::new().fg(Color::DarkGray),
            ),
            Span::raw("  "),
            Span::styled(
                format!("{freeable} droppable"),
                Style::new().fg(Color::Green),
            ),
        ]);
        // Nothing here outranks the sizing count, so it goes on the end.
        let line = with_sizing_note(app, line);
        f.render_widget(
            Paragraph::new(line).block(Block::default().borders(Borders::ALL)),
            area,
        );
        return;
    }

    let (bytes, paths) = app.estimated_savings();
    let mut spans = vec![
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
            format!("~{} reclaimable ({} paths)", provisional_bytes(app, bytes), paths),
            Style::new().fg(Color::Green),
        ),
        Span::raw("  "),
    ];
    // Directly after the figure it qualifies, so a narrow header truncates the
    // scope span (which explains itself) rather than the text explaining `+`.
    if let Some(note) = sizing_span(app) {
        spans.push(note);
        spans.push(Span::raw("  "));
    }
    // The rooted-only filter hides whole projects. Saying so here is what
    // makes `g` discoverable — otherwise the counts look unexplained.
    spans.push(scope_span(app));
    let line = Line::from(spans);
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

/// The current project scope, and what it is excluding.
fn scope_span(app: &App) -> Span<'static> {
    let (shown, hidden) = app.project_scope();
    if app.rooted_only {
        Span::styled(
            format!("{shown} rooted projects (g: +{hidden} unrooted)"),
            Style::new().fg(Color::Blue),
        )
    } else {
        Span::styled(
            format!("all {shown} projects (g: rooted only)"),
            Style::new().fg(Color::Blue).bold(),
        )
    }
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
            let n = app.distinct_versions_at(i);
            let selected = app.target_for(i).is_some();
            let style = if app.is_divergent_at(i) {
                Style::new().fg(Color::Yellow)
            } else {
                Style::new().fg(Color::DarkGray)
            };

            // Merge state shares the marker column with selection: a group is
            // either being assembled into a merge or ready to be planned.
            let (marker, marker_style) = if app.merge_marks.contains(&i) {
                ("◆ ", Style::new().fg(Color::Magenta).bold())
            } else if selected {
                ("● ", Style::new())
            } else {
                ("  ", Style::new())
            };

            let label = app
                .identity_of(i)
                .map(|id| id.to_string())
                .unwrap_or_default();
            // A merged group stands for several refs; say how many, so the pin
            // count does not look inexplicably high next to its siblings.
            let label = match app.merges.get(&i) {
                Some(m) => format!("{label}  +{} merged", m.members.len() - 1),
                None => label,
            };

            ListItem::new(Line::from(vec![
                Span::styled(marker, marker_style),
                Span::styled(format!("{label:<40}"), style),
                Span::raw(format!("{n} revs / {} pins", app.actionable_at(i).len())),
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

    let gi = app.visible().get(app.group_idx).copied();
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

    let heading = gi
        .and_then(|i| app.identity_of(i))
        .map(|id| id.to_string())
        .unwrap_or_else(|| group.identity.to_string());

    let mut lines = vec![
        Line::from(vec![
            Span::styled(heading, Style::new().bold().fg(Color::Yellow)),
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

    if let Some(i) = gi {
        // A merge changes what gets written to disk, so it is spelled out: which
        // refs were folded in, and which projects will have flake.nix rewritten.
        if let Some(m) = app.merges.get(&i) {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("merged onto {} — declared by:", m.canonical),
                Style::new().fg(Color::Magenta).bold(),
            )));
            for &mi in &m.members {
                let id = &app.groups[mi].identity;
                let mark = if *id == m.canonical { "canonical" } else { "will be rewritten" };
                lines.push(Line::from(Span::styled(
                    format!("  • {id}  ({mark})"),
                    Style::new().fg(Color::DarkGray),
                )));
            }
            let changes = app.ref_changes(i);
            if !changes.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!(
                        "  {} project(s) will have flake.nix rewritten to {}",
                        changes.len(),
                        m.canonical.url()
                    ),
                    Style::new().fg(Color::Magenta),
                )));
            }
        } else {
            // Merging is opt-in, so the opportunity has to be visible to exist.
            let candidates = app.merge_candidates(i);
            if !candidates.is_empty() {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    format!(
                        "{} other ref(s) of this repo — m marks, M merges:",
                        candidates.len()
                    ),
                    Style::new().fg(Color::Magenta),
                )));
                for c in candidates {
                    lines.push(Line::from(Span::styled(
                        format!("  • {}", app.groups[c].identity),
                        Style::new().fg(Color::DarkGray),
                    )));
                }
            }
        }
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

/// The plan, in full: every command, and every `flake.nix` edit as a real diff.
///
/// Editing a hand-maintained file is the part of a merge that most warrants
/// review, so the diffs are shown here rather than summarized — nothing is
/// written that the user has not had the chance to read first.
fn draw_confirm(f: &mut Frame, area: Rect, app: &App) {
    let edits = app.pending_diffs.len();
    let refs = app.pending.iter().filter(|a| a.changes_ref()).count();

    let mut lines = vec![Line::from(vec![
        Span::styled(
            format!("{} action(s)", app.pending.len()),
            Style::new().bold(),
        ),
        Span::raw("  "),
        Span::styled(
            format!("{refs} flake.nix rewrite(s)"),
            Style::new().fg(if refs > 0 { Color::Magenta } else { Color::DarkGray }),
        ),
    ])];

    // Projects that cannot be rewritten come first: they change what the plan
    // will actually accomplish, and the user should see them before approving.
    let blocked = app.unwritable();
    if !blocked.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("{} project(s) cannot be rewritten automatically:", blocked.len()),
            Style::new().fg(Color::Red).bold(),
        )));
        for p in &blocked {
            lines.push(Line::from(Span::styled(
                format!("  ! {}", p.project.display()),
                Style::new().fg(Color::Red),
            )));
            if let Err(e) = &p.edit {
                lines.push(Line::from(Span::styled(
                    format!("      {e}"),
                    Style::new().fg(Color::DarkGray),
                )));
            }
        }
        lines.push(Line::from(Span::styled(
            "  these are skipped; press e to open the first one in your editor",
            Style::new().fg(Color::DarkGray).italic(),
        )));
    }

    if edits > blocked.len() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "flake.nix changes:",
            Style::new().bold(),
        )));
        for p in app.pending_diffs.iter().filter(|p| !p.is_error()) {
            lines.push(Line::from(Span::styled(
                format!("  {} [{}]", p.project.display(), p.input_name),
                Style::new().fg(Color::Cyan),
            )));
            for raw in p.rendered.lines() {
                lines.push(diff_line(raw));
            }
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("commands:", Style::new().bold())));
    for a in &app.pending {
        for step in sync::command_lines(a) {
            lines.push(Line::from(Span::styled(
                format!("  {step}"),
                Style::new().fg(Color::Gray),
            )));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Enter = run   Esc = cancel   ↑↓ = scroll   e = edit a skipped flake.nix",
        Style::new().fg(Color::Yellow),
    )));

    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((app.details_scroll, 0))
            .block(Block::default().borders(Borders::ALL).title(" plan ")),
        area,
    );
}

/// Colour a diff line by its marker, stripping any ANSI the differ emitted.
///
/// A configurable differ may or may not colour its output, and ratatui renders
/// escape sequences literally, so they are removed and the colour reapplied
/// from the marker — which keeps every differ looking native in the TUI.
fn diff_line(raw: &str) -> Line<'static> {
    let text = strip_ansi(raw);
    let style = match text.trim_start().chars().next() {
        _ if text.starts_with("+++") || text.starts_with("---") => {
            Style::new().fg(Color::DarkGray)
        }
        Some('@') => Style::new().fg(Color::Cyan),
        Some('+') => Style::new().fg(Color::Green),
        Some('-') => Style::new().fg(Color::Red),
        _ => Style::new().fg(Color::Gray),
    };
    Line::from(Span::styled(format!("    {text}"), style))
}

/// Remove ANSI escape sequences, which ratatui would otherwise draw literally.
///
/// The configured differ decides whether to colour its output, and `delta` does
/// so heavily; stripping here and re-colouring from the diff marker means every
/// differ renders the same way inside the TUI, including one that emits none.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // Skip the introducer (`[` for CSI) before scanning for the final
            // byte — `[` is itself inside the @..~ range, so testing it first
            // would end the sequence immediately and leak its parameters.
            let mut peeked = chars.next();
            if peeked == Some('[') {
                peeked = chars.next();
            }
            // CSI parameters run until a final byte in @..~.
            while let Some(c) = peeked {
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    break;
                }
                peeked = chars.next();
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_colouring_is_stripped_to_plain_text() {
        // A real `delta --color-only` line: 24-bit colour, an intra-line
        // highlight, and a trailing erase-to-end-of-line.
        let raw = "\u{1b}[48;2;63;0;1m-    nixpkgs.url = \"github:nixos/nixpkgs/\u{1b}[48;2;144;16;17mnixpkgs\u{1b}[48;2;63;0;1m-unstable\";\u{1b}[0m\u{1b}[0K\u{1b}[0m";
        assert_eq!(
            strip_ansi(raw),
            "-    nixpkgs.url = \"github:nixos/nixpkgs/nixpkgs-unstable\";"
        );
    }

    #[test]
    fn plain_diff_text_survives_stripping_unchanged() {
        // A differ that emits no colour must render identically.
        let raw = "+    nixpkgs.url = \"github:nixos/nixpkgs/nixos-unstable\";";
        assert_eq!(strip_ansi(raw), raw);
    }

    #[test]
    fn diff_markers_drive_the_colour() {
        // Coloring is derived from the marker, not from what the differ sent,
        // so removed and added lines stay distinguishable for any differ.
        let removed = diff_line("-old line");
        let added = diff_line("+new line");
        assert_eq!(removed.spans[0].style.fg, Some(Color::Red));
        assert_eq!(added.spans[0].style.fg, Some(Color::Green));
        // File headers are chrome, not changes, despite starting with -/+.
        assert_eq!(diff_line("--- a/flake.nix").spans[0].style.fg, Some(Color::DarkGray));
        assert_eq!(diff_line("+++ b/flake.nix").spans[0].style.fg, Some(Color::DarkGray));
        assert_eq!(diff_line("@@ -4,3 +4,3 @@").spans[0].style.fg, Some(Color::Cyan));
    }
}

/// Live progress while the plan runs.
///
/// Each `nix flake update` blocks for seconds, so this screen has to answer
/// three questions at a glance: how far along, what is happening right now, and
/// whether anything has failed. The step in flight is named explicitly —
/// a bar that sits still for ten seconds is indistinguishable from a hang
/// unless the screen says what it is waiting on.
fn draw_running(f: &mut Frame, area: Rect, app: &App) {
    let Some(r) = &app.running else {
        return;
    };

    let filled = (r.fraction() * BAR_WIDTH as f64).round() as usize;
    let bar: String = "█".repeat(filled) + &"░".repeat(BAR_WIDTH.saturating_sub(filled));
    let failures = r.failures();

    let mut lines = vec![
        Line::from(vec![
            Span::styled(bar, Style::new().fg(Color::Green)),
            Span::raw("  "),
            Span::styled(
                format!("{:>3}%", (r.fraction() * 100.0) as usize),
                Style::new().bold(),
            ),
            Span::raw("  "),
            Span::raw(format!("{}/{}", r.done, r.total)),
            Span::raw("  "),
            Span::styled(
                human_duration(r.started.elapsed()),
                Style::new().fg(Color::DarkGray),
            ),
            Span::raw("  "),
            Span::styled(
                r.eta()
                    .map(|d| format!("ETA {}", human_duration(d)))
                    .unwrap_or_default(),
                Style::new().fg(Color::DarkGray),
            ),
            Span::raw("  "),
            Span::styled(
                if failures > 0 {
                    format!("{failures} failed")
                } else {
                    String::new()
                },
                Style::new().fg(Color::Red).bold(),
            ),
        ]),
        Line::from(""),
    ];

    // What is blocking right now, named so a long pause is legible.
    match &r.current {
        Some((step, project, input)) => {
            let mut spans = vec![
                Span::styled("▶ ", Style::new().fg(Color::Cyan).bold()),
                Span::styled(step.label(), Style::new().fg(Color::Cyan)),
                Span::raw("  "),
                Span::raw(project.display().to_string()),
            ];
            if !input.is_empty() {
                spans.push(Span::styled(
                    format!("  [{input}]"),
                    Style::new().fg(Color::DarkGray),
                ));
            }
            lines.push(Line::from(spans));
        }
        None => lines.push(Line::from(Span::styled(
            "  working…",
            Style::new().fg(Color::DarkGray),
        ))),
    }
    lines.push(Line::from(""));

    // Completed steps, most recent last, trimmed to what fits so the newest
    // stays on screen without scrolling.
    let room = (area.height as usize).saturating_sub(lines.len() + 3);
    let shown = r.log.iter().filter(|o| !o.text.is_empty()).rev().take(room);
    let mut recent: Vec<&crate::app::Outcome> = shown.collect();
    recent.reverse();
    for o in recent {
        lines.push(Line::from(Span::styled(
            format!("  {}", o.text),
            if o.ok {
                Style::new().fg(Color::Green)
            } else {
                Style::new().fg(Color::Red)
            },
        )));
    }

    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title(" running ")),
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

/// The reverse of `nix-store --gc --print-roots`: rooted store paths on the
/// left, and for the highlighted one, every root holding it on the right.
///
/// Split rather than a flat list because a path can be held by a dozen roots;
/// inlining them would push the size column off screen, and the sizes are how
/// the list is ranked.
fn draw_roots(f: &mut Frame, area: Rect, app: &App) {
    let cols = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(area);

    let rows = app.root_rows();
    let title = format!(
        " rooted store paths — {} total{}{} ",
        human_bytes(app.roots_total_bytes()),
        if app.roots_freeable_only { ", droppable only" } else { "" },
        if app.roots_hide_process { "" } else { ", +process" },
    );

    if rows.is_empty() {
        f.render_widget(
            // Distinguish "nix told us nothing" from "the filters hid it all",
            // which need opposite actions from the user.
            Paragraph::new(if app.roots.is_empty() {
                "No GC roots found.\n\nThis needs `nix-store` on PATH."
            } else if app.roots_freeable_only {
                "Nothing here is droppable by direnv roots alone.\n\nPress f to show every rooted path."
            } else {
                "Nothing rooted outside running processes.\n\nPress p to include process-held paths."
            })
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title(title)),
            cols[0],
        );
        f.render_widget(
            Block::default().borders(Borders::ALL).title(" retained by "),
            cols[1],
        );
        return;
    }

    // Keep the cursor on screen for stores with thousands of rooted paths.
    let height = cols[0].height.saturating_sub(2) as usize;
    let offset = app.roots_idx.saturating_sub(height.saturating_sub(1));

    let items: Vec<Line> = rows
        .iter()
        .enumerate()
        .skip(offset)
        .take(height)
        .map(|(i, r)| {
            let selected = i == app.roots_idx;
            // A path nothing outside direnv holds is one this tool could
            // actually free, which is the distinction the view is for.
            let marker = if r.freeable() { "·" } else { "×" };
            let name = r
                .store_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| r.store_path.display().to_string());
            let style = if selected {
                Style::new().fg(Color::Black).bg(Color::Cyan)
            } else if r.freeable() {
                Style::new()
            } else {
                Style::new().fg(Color::DarkGray)
            };
            Line::from(Span::styled(
                format!(
                    "{marker} {:>9}  {:>2}  {name}",
                    row_size(app, r),
                    r.retainer_count(),
                ),
                style,
            ))
        })
        .collect();

    f.render_widget(
        Paragraph::new(items).block(Block::default().borders(Borders::ALL).title(title)),
        cols[0],
    );

    draw_root_detail(f, cols[1], app);
}

/// Everything holding the highlighted store path.
fn draw_root_detail(f: &mut Frame, area: Rect, app: &App) {
    let Some(row) = app.selected_root() else {
        f.render_widget(
            Block::default().borders(Borders::ALL).title(" retained by "),
            area,
        );
        return;
    };

    let mut lines = vec![
        Line::from(Span::styled(
            row.store_path.display().to_string(),
            Style::new().fg(Color::Cyan),
        )),
        Line::from(format!(
            "{} across {} root(s)",
            row_size(app, &row),
            row.retainer_count()
        )),
        Line::from(""),
    ];

    if !row.projects.is_empty() {
        lines.push(Line::from(Span::styled(
            "direnv (droppable by this tool)",
            Style::new().fg(Color::Green),
        )));
        for p in &row.projects {
            lines.push(Line::from(format!("  {}", p.display())));
        }
        lines.push(Line::from(""));
    }

    if !row.external.is_empty() {
        lines.push(Line::from(Span::styled(
            "external (must be released elsewhere)",
            Style::new().fg(Color::Yellow),
        )));
        for e in &row.external {
            lines.push(Line::from(format!("  {e}")));
        }
        lines.push(Line::from(""));
    }

    if !row.process.is_empty() {
        lines.push(Line::from(Span::styled(
            "running processes (transient)",
            Style::new().fg(Color::DarkGray),
        )));
        for p in &row.process {
            lines.push(Line::from(format!("  {p}")));
        }
        lines.push(Line::from(""));
    }

    // Say plainly whether the space is actually recoverable — the whole point
    // of separating the two lists above.
    lines.push(Line::from(if row.freeable() {
        Span::styled(
            "Converging these projects and dropping their roots would free this.",
            Style::new().fg(Color::Green),
        )
    } else if row.blocked_only_by_process() {
        Span::styled(
            "Only a running process holds this — it frees once that exits.",
            Style::new().fg(Color::DarkGray),
        )
    } else if row.projects.is_empty() {
        Span::styled(
            "No direnv root holds this; this tool cannot free it.",
            Style::new().fg(Color::DarkGray),
        )
    } else {
        Span::styled(
            "Held outside direnv too — dropping direnv roots frees nothing here.",
            Style::new().fg(Color::Yellow),
        )
    }));

    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title(" retained by ")),
        area,
    );
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    // `g` and `a` are toggles, so the legend names what the key would switch
    // *to* rather than showing a state-independent label.
    let keys = match app.mode {
        Mode::Browse => format!(
            "↑↓ move  Tab pane  Enter pick  x exclude  A/N all/none  m mark  M merge  \
             e edit  r roots  a {}  g {}  s stage  q quit",
            if app.show_all { "divergent only" } else { "show all" },
            if app.rooted_only { "+unrooted" } else { "rooted only" },
        ),
        Mode::Confirm => "Enter run  Esc cancel  ↑↓ scroll  e edit  q quit".to_string(),
        // No keys: the run blocks until it finishes, and offering an abort we
        // cannot honour mid-`nix`-invocation would be a lie.
        Mode::Running => "running — please wait".to_string(),
        Mode::Report => "r roots  q quit".to_string(),
        Mode::Roots => format!(
            "↑↓ move  f {}  p {}  Esc back  q quit",
            if app.roots_freeable_only { "show all" } else { "droppable only" },
            if app.roots_hide_process { "+process" } else { "hide process" },
        ),
    };
    let mut spans = vec![Span::styled(keys, Style::new().fg(Color::DarkGray))];
    // The status line carries browse-mode feedback ("target set to…"), which is
    // stale once a run starts and would sit next to the progress bar as if it
    // described it.
    if !app.status.is_empty() && app.mode != Mode::Running {
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

#[cfg(test)]
mod render_tests {
    use super::*;
    use crate::app::{Running, Step};
    use ratatui::backend::TestBackend;

    /// Render the running screen into a test terminal and return it as text.
    fn render(app: &App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn running_app() -> App {
        let mut app = crate::app::test_support::app_for_render();
        let mut r = Running::new(4);
        r.done = 1;
        r.current = Some((
            Step::Updating,
            std::path::PathBuf::from("/Users/k/projects/myapp"),
            "nixpkgs".into(),
        ));
        r.log.push(crate::app::Outcome { ok: true, text: "ok    /Users/k/projects/a nixpkgs".into() });
        r.log.push(crate::app::Outcome { ok: false, text: "fail  /Users/k/projects/b: boom".into() });
        app.running = Some(r);
        app.mode = Mode::Running;
        app
    }

    /// The roots view, mid-sizing: one path measured, two still pending.
    fn part_sized_roots_app() -> App {
        let mut app = crate::app::test_support::app_with_roots();
        for r in app.roots.values_mut() {
            r.size_bytes = 0;
        }
        app.begin_sizing(3);
        app.apply_size(
            &std::path::PathBuf::from("/nix/store/bbb-utils-source"),
            2_000_000,
        );
        app.open_roots();
        app
    }

    #[test]
    fn a_growing_total_is_marked_as_still_sizing() {
        let out = render(&part_sized_roots_app(), 110, 24);
        // The bytes so far, flagged as a lower bound...
        assert!(out.contains("1.9 MiB+ held"), "{out}");
        // ...and the count that explains the flag.
        assert!(out.contains("sizing 1/3"), "{out}");
    }

    #[test]
    fn an_unmeasured_path_renders_as_pending_not_as_empty() {
        let out = render(&part_sized_roots_app(), 110, 24);
        // The measured row shows its size; the unmeasured ones must not claim
        // to be empty store paths.
        assert!(out.contains("1.9 MiB"), "{out}");
        assert!(!out.contains("0 B"), "{out}");
        assert!(out.contains("—"), "{out}");
    }

    #[test]
    fn a_finished_scan_leaves_no_provisional_marker() {
        let app = roots_app();
        let out = render(&app, 110, 24);
        assert!(!out.contains("sizing"), "{out}");
        assert!(!out.contains("MiB+"), "{out}");
        assert!(out.contains("11.0 MiB held"), "{out}");
    }

    #[test]
    fn the_browse_header_marks_a_provisional_savings_figure() {
        let mut app = crate::app::test_support::app_for_render();
        app.begin_sizing(4);
        let out = render(&app, 110, 24);
        assert!(out.contains("~0 B+ reclaimable"), "{out}");
        // The count sits inside the width the scope span used to push it past:
        // a `+` whose explanation is truncated away is worse than no marker.
        assert!(out.contains("sizing 0/4"), "{out}");
    }

    #[test]
    fn the_running_screen_names_what_is_blocking() {
        let out = render(&running_app(), 100, 24);
        assert!(out.contains("running"), "{out}");
        // The step in flight, named — a still bar must be distinguishable
        // from a hang.
        assert!(out.contains("nix flake update"), "{out}");
        assert!(out.contains("myapp"), "{out}");
        assert!(out.contains("[nixpkgs]"), "{out}");
    }

    #[test]
    fn the_running_screen_shows_counts_and_failures() {
        let out = render(&running_app(), 100, 24);
        assert!(out.contains("1/4"), "{out}");
        assert!(out.contains("25%"), "{out}");
        assert!(out.contains("1 failed"), "{out}");
        // Completed steps stay visible while the next one runs.
        assert!(out.contains("boom"), "{out}");
    }

    #[test]
    fn a_narrow_terminal_still_renders() {
        // Must not panic on a small area; the log simply gets less room.
        let out = render(&running_app(), 40, 10);
        assert!(out.contains("running"), "{out}");
    }

    fn roots_app() -> App {
        let mut app = crate::app::test_support::app_with_roots();
        app.open_roots();
        app
    }

    #[test]
    fn the_roots_screen_ranks_paths_by_size() {
        let out = render(&roots_app(), 110, 24);
        let big = out
            .find("aaa-nixpkgs-source")
            .unwrap_or_else(|| panic!("largest path missing:\n{out}"));
        let small = out
            .find("ccc-jdk")
            .unwrap_or_else(|| panic!("smallest path missing:\n{out}"));
        assert!(big < small, "largest path should sort first:\n{out}");
    }

    #[test]
    fn the_roots_screen_names_what_holds_the_selected_path() {
        // Cursor starts on the largest path, held only by one project.
        let out = render(&roots_app(), 110, 24);
        assert!(out.contains("/Users/k/projects/myapp"), "{out}");
        assert!(out.contains("direnv"), "{out}");
    }

    #[test]
    fn an_externally_pinned_path_says_it_cannot_be_freed() {
        let mut app = roots_app();
        // Move to the jdk, which a profile and a process both hold.
        app.roots_down();
        app.roots_down();
        let out = render(&app, 110, 24);
        assert!(out.contains("/run/current-system"), "{out}");
        assert!(out.contains("frees nothing"), "{out}");
        // Process roots are hidden by default, even on a row that stays listed
        // because something durable also holds it.
        assert!(!out.contains("{lsof}"), "{out}");
    }

    #[test]
    fn process_roots_appear_once_asked_for() {
        let mut app = roots_app();
        app.toggle_roots_process();
        app.roots_down();
        app.roots_down();
        let out = render(&app, 110, 24);
        assert!(out.contains("{lsof}"), "{out}");
        assert!(out.contains("running processes"), "{out}");
    }

    #[test]
    fn a_path_only_a_process_holds_is_hidden_by_default() {
        let mut app = crate::app::test_support::app_with_roots();
        // Pin the second path with a process and nothing else.
        let key = std::path::PathBuf::from("/nix/store/bbb-utils-source");
        app.roots
            .get_mut(&key)
            .unwrap()
            .external_roots
            .insert("{temp:4242}".into());
        app.open_roots();

        let hidden = render(&app, 110, 24);
        assert!(!hidden.contains("bbb-utils-source"), "{hidden}");

        app.toggle_roots_process();
        let shown = render(&app, 110, 24);
        assert!(shown.contains("bbb-utils-source"), "{shown}");
    }

    #[test]
    fn a_path_no_direnv_root_holds_is_hidden_with_process_roots() {
        // Real case: a JDK held only by {lsof}. With process roots hidden it
        // would otherwise list with no retainers at all.
        let mut app = crate::app::test_support::app_with_roots();
        let p = std::path::PathBuf::from("/nix/store/ddd-jdk-only-lsof");
        app.roots.insert(
            p.clone(),
            crate::store::RootedPath {
                store_path: p,
                retained_by: Default::default(),
                external_roots: ["{lsof}".to_string()].into_iter().collect(),
                size_bytes: 400_000_000,
            },
        );
        app.open_roots();

        let out = render(&app, 110, 24);
        assert!(!out.contains("ddd-jdk-only-lsof"), "{out}");

        app.toggle_roots_process();
        let shown = render(&app, 110, 24);
        assert!(shown.contains("ddd-jdk-only-lsof"), "{shown}");
    }

    #[test]
    fn hiding_process_roots_never_marks_a_pinned_path_droppable() {
        // The filter must not make `freeable` true for a path a process pins;
        // that would advertise space the user cannot actually get back.
        let mut app = crate::app::test_support::app_with_roots();
        let key = std::path::PathBuf::from("/nix/store/bbb-utils-source");
        app.roots
            .get_mut(&key)
            .unwrap()
            .external_roots
            .insert("{lsof}".into());
        app.open_roots();
        app.roots_freeable_only = true;

        assert!(
            !app.root_rows()
                .iter()
                .any(|r| r.store_path.ends_with("bbb-utils-source")),
            "process-pinned path leaked into the droppable list"
        );
    }

    #[test]
    fn the_freeable_filter_hides_externally_pinned_paths() {
        let mut app = roots_app();
        app.toggle_roots_filter();
        let out = render(&app, 110, 24);
        assert!(out.contains("aaa-nixpkgs-source"), "{out}");
        // Held by {lsof} and a profile, so dropping direnv roots frees nothing.
        assert!(!out.contains("ccc-jdk"), "{out}");
    }

    #[test]
    fn the_roots_screen_survives_an_empty_root_set() {
        // No nix, or a store with nothing rooted.
        let mut app = crate::app::test_support::app_for_render();
        app.open_roots();
        let out = render(&app, 110, 24);
        assert!(out.contains("No GC roots found"), "{out}");
    }

    #[test]
    fn a_narrow_terminal_still_renders_the_roots_screen() {
        let out = render(&roots_app(), 40, 10);
        assert!(out.contains("gc roots"), "{out}");
    }
}

