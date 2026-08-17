mod app;
mod diff;
mod lock;
mod nix_edit;
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
use crossterm::terminal::{Clear, ClearType};
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
    let mut app = App::new(groups, roots, parse_errors, cfg);
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

    // Refs of one repo that merging could combine. These are never divergent
    // groups on their own, so without this they are invisible in the report —
    // and they are the reason the merge feature exists.
    let mut by_repo: std::collections::BTreeMap<lock::MergeKey, Vec<&lock::Group>> =
        std::collections::BTreeMap::new();
    for g in &app.groups {
        by_repo.entry(g.identity.merge_key()).or_default().push(g);
    }
    // A ref with no actionable pins in the current scope is not something the
    // user can merge, so listing it would advertise an action that does nothing.
    let mergeable: Vec<Vec<&lock::Group>> = by_repo
        .values()
        .map(|v| {
            v.iter()
                .copied()
                .filter(|g| app.actionable_pins(g).next().is_some())
                .collect::<Vec<_>>()
        })
        .filter(|v| v.len() > 1)
        .collect();
    if !mergeable.is_empty() {
        println!("\n{} repo(s) declared under more than one ref:", mergeable.len());
        for groups in &mergeable {
            let total: usize = groups.iter().map(|g| app.actionable_pins(g).count()).sum();
            println!("  {} refs, {total} pins:", groups.len());
            for g in groups.iter() {
                println!(
                    "    {:>2} revs across {:>2} pins  {}",
                    app.distinct_versions(g),
                    app.actionable_pins(g).count(),
                    g.identity
                );
            }
        }
        println!("  (merge them in the TUI with m/M — this rewrites flake.nix)");
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
                KeyCode::Char('m') => app.toggle_merge_mark(),
                KeyCode::Char('M') => app.commit_merge(),
                KeyCode::Char('e') => open_in_editor(term, app)?,
                KeyCode::PageDown => app.scroll_details(1),
                KeyCode::PageUp => app.scroll_details(-1),
                _ => {}
            },
            Mode::Confirm => match key.code {
                KeyCode::Enter => execute_plan(app),
                KeyCode::Esc => app.cancel(),
                KeyCode::Up | KeyCode::Char('k') => app.scroll_details(-1),
                KeyCode::Down | KeyCode::Char('j') => app.scroll_details(1),
                // The plan lists projects this tool will not rewrite; `e` opens
                // the first of them so they can be fixed by hand.
                KeyCode::Char('e') => open_in_editor(term, app)?,
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

/// Which `flake.nix` the `e` key should open.
///
/// In the confirm view the interesting file is one the tool refused to rewrite —
/// that is the case needing hands. Otherwise it is the highlighted project.
fn editor_target(app: &App) -> Option<std::path::PathBuf> {
    if app.mode == Mode::Confirm {
        if let Some(p) = app.unwritable().first() {
            return Some(p.project.join("flake.nix"));
        }
    }
    app.selected_version_row()
        .and_then(|r| r.pins.first().map(|p| p.project.join("flake.nix")))
}

/// Hand the terminal to the configured editor, then take it back.
///
/// The TUI owns the alternate screen and raw mode, so both are released before
/// the child starts and restored after it exits — otherwise the editor draws
/// into a screen it does not control and leaves the terminal unusable.
fn open_in_editor(term: &mut Term, app: &mut App) -> Result<()> {
    let Some(path) = editor_target(app) else {
        app.status = "nothing to open".into();
        return Ok(());
    };
    let cmd = app.config.editor_command();
    let Some((program, args)) = cmd.split_first() else {
        app.status = "no editor configured".into();
        return Ok(());
    };

    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;

    let status = std::process::Command::new(program)
        .args(args)
        .arg(&path)
        .status();

    enable_raw_mode()?;
    execute!(term.backend_mut(), EnterAlternateScreen, Clear(ClearType::All))?;
    term.clear()?;

    app.status = match status {
        Ok(_) => format!("edited {} — rescan (q, then rerun) to pick up changes", path.display()),
        Err(e) => format!("could not launch {program}: {e}"),
    };
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

    // Take the plan out so `app` can be borrowed immutably for the previews.
    let pending = std::mem::take(&mut app.pending);

    for action in &pending {
        // A ref change edits flake.nix first: the lockfile is resolved against
        // whatever the file declares, so it has to be right before nix runs.
        if action.changes_ref() {
            let Some(edit) = app.edit_for(&action.project, &action.input_name) else {
                report.push(format!(
                    "fail  {} [{}]: flake.nix could not be rewritten, skipped",
                    action.project.display(),
                    action.input_name
                ));
                failed.insert(action.project.clone());
                continue;
            };
            match nix_edit::apply_edit(edit) {
                Ok(backup) => {
                    report.push(format!(
                        "ok    {} [{}] url {} → {}  (backup: {})",
                        edit.path.display(),
                        action.input_name,
                        edit.from_url,
                        edit.to_url,
                        backup.display()
                    ));
                    // Show the edit exactly as it landed, not as it was planned.
                    let applied = diff::unified(&edit.path, &edit.before, &edit.after);
                    for line in diff::render(&applied, &app.config.differ).lines() {
                        report.push(format!("      {line}"));
                    }
                }
                Err(e) => {
                    report.push(format!("fail  {}: {e}", edit.path.display()));
                    failed.insert(action.project.clone());
                    continue;
                }
            }
        }

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
    // The backups are the undo path for every flake.nix touched here, so say
    // they exist rather than leaving them to be discovered.
    let edited: Vec<String> = pending
        .iter()
        .filter(|a| a.changes_ref())
        .filter_map(|a| app.edit_for(&a.project, &a.input_name))
        .map(|e| nix_edit::backup_path(&e.path).display().to_string())
        .collect();
    if !edited.is_empty() {
        report.push(format!(
            "{} flake.nix backup(s) kept alongside the originals (.nix.bak).",
            edited.len()
        ));
    }

    app.report = report;
    app.pending_diffs.clear();
    app.mode = Mode::Report;
}
