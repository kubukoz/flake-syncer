mod app;
mod lock;
mod progress;
mod scan;
mod store;
mod sync;
mod ui;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::execute;
use ratatui::prelude::*;
use std::collections::BTreeSet;
use std::io::stdout;
use std::time::Duration;

use app::{App, Mode};
use store::human_bytes;

fn main() -> Result<()> {
    let cfg = scan::load_config()?;
    let locks = scan::find_lockfiles(&cfg);

    if locks.is_empty() {
        eprintln!(
            "No flake.lock files found under: {}",
            cfg.roots
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        if let Some(p) = scan::config_path() {
            eprintln!("Edit {} to change the scan roots.", p.display());
        }
        return Ok(());
    }

    let (groups, errors) = lock::group(&locks);
    let parse_errors = errors
        .into_iter()
        .map(|(p, e)| (p, e.to_string()))
        .collect();

    // `--report` prints the analysis and exits, for scripting and for sanity
    // checks without entering the alternate screen.
    let report_only = std::env::args().any(|a| a == "--report");
    // `--all-projects` drops the GC-root requirement in both modes.
    let rooted_only = !std::env::args().any(|a| a == "--all-projects");

    let roots = store::scan_roots();
    let mut app = App::new(groups, roots, parse_errors);
    app.rooted_only = rooted_only;

    if report_only {
        return print_report(&app, locks.len());
    }
    run_tui(app)
}

/// The analysis, printed and exited: for scripting, and for sanity checks
/// without entering the alternate screen.
fn print_report(app: &App, lock_count: usize) -> Result<()> {
    println!("{lock_count} lockfiles, {} input identities", app.groups.len());
    if app.rooted_only {
        println!("(only projects holding a GC root; --all-projects to include the rest)");
    }

    let mut divergent: Vec<&lock::Group> =
        app.groups.iter().filter(|g| app.is_divergent(g)).collect();
    divergent.sort_by_key(|g| std::cmp::Reverse(app.distinct_versions(g)));

    println!("\n{} divergent:", divergent.len());
    for g in &divergent {
        println!(
            "  {:>2} revs across {:>2} pins  {}",
            app.distinct_versions(g),
            app.actionable_pins(g).count(),
            g.identity
        );
    }

    let all: BTreeSet<_> = app
        .groups
        .iter()
        .filter(|g| app.is_divergent(g))
        .flat_map(|g| app.actionable_pins(g).map(|p| p.project.clone()))
        .collect();
    let (bytes, paths) = store::reclaimable(&app.roots, &all);
    println!(
        "\nUpper bound if every divergent group is unified and its direnv roots dropped:\n  {} across {paths} store paths",
        human_bytes(bytes)
    );
    Ok(())
}

fn run_tui(mut app: App) -> Result<()> {
    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen)?;
    let mut term = Terminal::new(CrosstermBackend::new(out))?;

    let result = event_loop(&mut term, &mut app);

    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;
    result
}

type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

fn event_loop(term: &mut Term, app: &mut App) -> Result<()> {
    loop {
        term.draw(|f| ui::draw(f, app))?;

        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        match app.mode {
            Mode::Browse => match key.code {
                KeyCode::Char('q') => break,
                KeyCode::Up | KeyCode::Char('k') => app.on_up(),
                KeyCode::Down | KeyCode::Char('j') => app.on_down(),
                KeyCode::Tab => app.toggle_pane(),
                KeyCode::Enter => app.choose_version(),
                KeyCode::Char('x') => app.clear_target(),
                KeyCode::Char('a') => {
                    app.show_all = !app.show_all;
                    app.group_idx = 0;
                    app.version_idx = 0;
                }
                KeyCode::Char('s') => app.stage(),
                KeyCode::Char('A') => app.select_all_suggested(),
                KeyCode::Char('N') => app.select_none(),
                KeyCode::Char('g') => app.toggle_rooted_only(),
                KeyCode::PageDown => app.scroll_details(1),
                KeyCode::PageUp => app.scroll_details(-1),
                _ => {}
            },
            Mode::Confirm => match key.code {
                KeyCode::Enter => execute_plan(app),
                KeyCode::Esc => app.cancel(),
                KeyCode::Char('q') => break,
                _ => {}
            },
            Mode::Report => {
                if matches!(key.code, KeyCode::Char('q') | KeyCode::Enter | KeyCode::Esc) {
                    break;
                }
            }
        }
    }
    Ok(())
}

/// Run the staged actions, then drop direnv roots for projects we changed.
///
/// Roots are only dropped for projects where every action succeeded; a project
/// whose update failed still needs its current inputs.
fn execute_plan(app: &mut App) {
    let mut report = Vec::new();
    let mut touched: BTreeSet<std::path::PathBuf> = BTreeSet::new();
    let mut failed: BTreeSet<std::path::PathBuf> = BTreeSet::new();

    for action in &app.pending {
        match sync::apply(action) {
            Ok(()) => {
                report.push(format!(
                    "ok    {} {} → {}",
                    action.project.display(),
                    action.input_name,
                    app::short(&action.target_version)
                ));
                touched.insert(action.project.clone());
            }
            Err(e) => {
                report.push(format!("fail  {}: {e}", action.project.display()));
                failed.insert(action.project.clone());
            }
        }
    }

    let mut dropped = 0;
    for project in touched.difference(&failed) {
        match sync::drop_direnv_roots(project) {
            Ok(n) => dropped += n,
            Err(e) => report.push(format!("fail  dropping roots in {}: {e}", project.display())),
        }
    }

    report.push(String::new());
    report.push(format!(
        "Dropped {dropped} direnv GC root(s). Run `nix store gc` to reclaim the space."
    ));

    app.report = report;
    app.pending.clear();
    app.mode = Mode::Report;
}
