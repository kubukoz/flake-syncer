//! Application state, independent of rendering.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::lock::{Group, Pin};
use crate::store::{self, RootedPath};
use crate::sync::{self, Action};

/// One distinct version of the selected group, with every pin holding it.
#[derive(Debug, Clone)]
pub struct VersionRow {
    pub version: String,
    pub last_modified: Option<i64>,
    pub pins: Vec<Pin>,
}

impl VersionRow {
    pub fn count(&self) -> usize {
        self.pins.len()
    }

    /// Project basenames, in pin order, deduplicated.
    ///
    /// A single project can pin one identity twice (`nixpkgs` and `nixpkgs_2`),
    /// so this is shorter than `count()` when transitive inputs are involved.
    pub fn project_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for p in &self.pins {
            let name = project_name(&p.project);
            if !names.contains(&name) {
                names.push(name);
            }
        }
        names
    }
}

/// Last path component, which is how a project is recognizable at a glance.
pub fn project_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Join names to fit `width` columns, eliding the tail as `+N more`.
///
/// Always emits at least one name (truncated if it alone overflows) so a row
/// never degrades to a bare count, which is what this display replaced.
pub fn fit_names(names: &[String], width: usize) -> String {
    if names.is_empty() {
        return String::new();
    }

    // Try the longest prefix of names that leaves room for its own suffix.
    for take in (1..=names.len()).rev() {
        let shown = names[..take].join(", ");
        let hidden = names.len() - take;
        let text = if hidden == 0 {
            shown
        } else {
            format!("{shown}, +{hidden} more")
        };
        if text.chars().count() <= width {
            return text;
        }
    }

    let first = &names[0];
    let hidden = names.len() - 1;
    let suffix = if hidden == 0 {
        String::new()
    } else {
        format!(", +{hidden} more")
    };
    let room = width.saturating_sub(suffix.chars().count() + 1);
    let clipped: String = first.chars().take(room).collect();
    format!("{clipped}…{suffix}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Groups,
    Versions,
    /// The details pane, focusable so its project list can be scrolled.
    Details,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Browse,
    /// Showing the commands that would run, awaiting confirm.
    Confirm,
    /// Actions have run; showing the outcome.
    Report,
}

pub struct App {
    pub groups: Vec<Group>,
    pub roots: BTreeMap<PathBuf, RootedPath>,
    pub parse_errors: Vec<(PathBuf, String)>,

    pub group_idx: usize,
    pub version_idx: usize,
    pub pane: Pane,
    pub mode: Mode,
    pub show_all: bool,
    /// Restrict to projects that actually hold a GC root.
    ///
    /// On by default: converging a project that roots nothing frees no disk
    /// space, so touching its lockfile is churn for no benefit.
    pub rooted_only: bool,
    /// Projects holding at least one GC root, derived from `roots`.
    rooted: BTreeSet<PathBuf>,

    /// Per-group chosen target version, keyed by index into `groups`.
    pub targets: BTreeMap<usize, String>,
    pub pending: Vec<Action>,
    pub report: Vec<String>,
    pub status: String,
    /// Scroll offset of the details pane, for groups with many projects.
    pub details_scroll: u16,
}

impl App {
    pub fn new(
        groups: Vec<Group>,
        roots: BTreeMap<PathBuf, RootedPath>,
        parse_errors: Vec<(PathBuf, String)>,
    ) -> Self {
        let rooted = store::rooted_projects(&roots);
        let mut app = App {
            groups,
            roots,
            parse_errors,
            group_idx: 0,
            version_idx: 0,
            pane: Pane::Groups,
            mode: Mode::Browse,
            show_all: false,
            rooted_only: true,
            rooted,
            targets: BTreeMap::new(),
            pending: Vec::new(),
            report: Vec::new(),
            status: String::new(),
            details_scroll: 0,
        };
        app.status = "nothing selected — Enter picks a version, A selects all".into();
        app
    }

    /// The version a group would converge on if selected: its newest in use.
    ///
    /// Offered as a suggestion rather than applied up front — staging every
    /// divergent group by default makes a destructive plan the path of least
    /// resistance.
    pub fn suggested_target(&self, group_index: usize) -> Option<&str> {
        let g = self.groups.get(group_index)?;
        if !self.is_divergent(g) {
            return None;
        }
        self.actionable_pins(g)
            .max_by_key(|p| p.last_modified.unwrap_or(i64::MIN))
            .map(|p| p.version.as_str())
    }

    /// Opt into the suggested target for every divergent group at once.
    pub fn select_all_suggested(&mut self) {
        let picks: Vec<(usize, String)> = (0..self.groups.len())
            .filter_map(|i| self.suggested_target(i).map(|v| (i, v.to_string())))
            .collect();
        let n = picks.len();
        self.targets.extend(picks);
        self.status = format!("selected {n} group(s) at newest in use");
    }

    /// Drop every selection, returning to an empty plan.
    pub fn select_none(&mut self) {
        self.targets.clear();
        self.status = "selection cleared".into();
    }

    /// Whether a pin is one this tool can and should act on.
    ///
    /// Two independent reasons to exclude one: it is transitive (no
    /// `--override-input` can reach it), or its project holds no GC root (so
    /// converging it frees nothing).
    pub fn is_actionable(&self, pin: &Pin) -> bool {
        pin.direct && (!self.rooted_only || self.rooted.contains(&pin.project))
    }

    pub fn actionable_pins<'a>(&'a self, group: &'a Group) -> impl Iterator<Item = &'a Pin> {
        group.pins.iter().filter(|p| self.is_actionable(p))
    }

    /// Distinct versions among a group's actionable pins.
    pub fn distinct_versions(&self, group: &Group) -> usize {
        let mut v: Vec<&str> = self.actionable_pins(group).map(|p| p.version.as_str()).collect();
        v.sort_unstable();
        v.dedup();
        v.len()
    }

    pub fn is_divergent(&self, group: &Group) -> bool {
        self.distinct_versions(group) > 1
    }

    /// Pins excluded only because their project roots nothing.
    pub fn unrooted_count(&self, group: &Group) -> usize {
        group
            .pins
            .iter()
            .filter(|p| p.direct && !self.rooted.contains(&p.project))
            .count()
    }

    /// Projects currently in scope, and how many the rooted-only filter hides.
    pub fn project_scope(&self) -> (usize, usize) {
        let all: BTreeSet<&PathBuf> = self
            .groups
            .iter()
            .flat_map(|g| g.pins.iter().filter(|p| p.direct).map(|p| &p.project))
            .collect();
        let rooted = all.iter().filter(|p| self.rooted.contains(**p)).count();
        if self.rooted_only {
            (rooted, all.len() - rooted)
        } else {
            (all.len(), 0)
        }
    }

    pub fn toggle_rooted_only(&mut self) {
        self.rooted_only = !self.rooted_only;
        // Selections may now refer to groups that are no longer divergent.
        let stale: Vec<usize> = self
            .targets
            .keys()
            .copied()
            .filter(|&gi| !self.is_divergent(&self.groups[gi]))
            .collect();
        for gi in stale {
            self.targets.remove(&gi);
        }
        self.group_idx = 0;
        self.version_idx = 0;
        self.details_scroll = 0;
        self.status = if self.rooted_only {
            "showing only projects with GC roots".into()
        } else {
            "showing all projects, including unrooted".into()
        };
    }

    /// Indices of the groups currently listed, honouring the divergent filter.
    pub fn visible(&self) -> Vec<usize> {
        self.groups
            .iter()
            .enumerate()
            .filter(|(_, g)| self.show_all || self.is_divergent(g))
            .map(|(i, _)| i)
            .collect()
    }

    pub fn selected_group(&self) -> Option<&Group> {
        self.visible().get(self.group_idx).map(|&i| &self.groups[i])
    }

    fn selected_index(&self) -> Option<usize> {
        self.visible().get(self.group_idx).copied()
    }

    pub fn target_for(&self, group_index: usize) -> Option<&str> {
        self.targets.get(&group_index).map(|s| s.as_str())
    }

    /// Versions of the selected group, newest first, with the pins holding each.
    pub fn version_rows(&self) -> Vec<VersionRow> {
        let Some(g) = self.selected_group() else {
            return Vec::new();
        };
        let mut by_version: BTreeMap<&str, VersionRow> = BTreeMap::new();
        for p in self.actionable_pins(g) {
            let e = by_version
                .entry(p.version.as_str())
                .or_insert_with(|| VersionRow {
                    version: p.version.clone(),
                    last_modified: p.last_modified,
                    pins: Vec::new(),
                });
            e.pins.push(p.clone());
            if p.last_modified > e.last_modified {
                e.last_modified = p.last_modified;
            }
        }
        let mut rows: Vec<VersionRow> = by_version.into_values().collect();
        // Pins within a row read best in a stable, path-sorted order.
        for row in &mut rows {
            row.pins.sort_by(|a, b| {
                (&a.project, &a.input_name).cmp(&(&b.project, &b.input_name))
            });
        }
        rows.sort_by_key(|r| std::cmp::Reverse(r.last_modified));
        rows
    }

    /// The version row currently highlighted in the versions pane.
    pub fn selected_version_row(&self) -> Option<VersionRow> {
        self.version_rows().get(self.version_idx).cloned()
    }

    /// Bytes freed if the current selections are applied and the affected
    /// projects' direnv roots are dropped.
    pub fn estimated_savings(&self) -> (u64, usize) {
        let mut projects: BTreeSet<PathBuf> = BTreeSet::new();
        for (&gi, target) in &self.targets {
            for p in self.actionable_pins(&self.groups[gi]) {
                if p.version != *target {
                    projects.insert(p.project.clone());
                }
            }
        }
        store::reclaimable(&self.roots, &projects)
    }

    pub fn divergent_count(&self) -> usize {
        self.groups.iter().filter(|g| self.is_divergent(g)).count()
    }

    pub fn on_up(&mut self) {
        match self.pane {
            Pane::Groups => {
                self.group_idx = self.group_idx.saturating_sub(1);
                self.reset_scroll();
            }
            Pane::Versions => {
                self.version_idx = self.version_idx.saturating_sub(1);
                self.reset_scroll();
            }
            Pane::Details => self.scroll_details(-1),
        }
    }

    pub fn on_down(&mut self) {
        match self.pane {
            Pane::Groups => {
                let n = self.visible().len();
                if n > 0 && self.group_idx + 1 < n {
                    self.group_idx += 1;
                    self.version_idx = 0;
                }
                self.reset_scroll();
            }
            Pane::Versions => {
                let n = self.version_rows().len();
                if n > 0 && self.version_idx + 1 < n {
                    self.version_idx += 1;
                }
                self.reset_scroll();
            }
            Pane::Details => self.scroll_details(1),
        }
    }

    fn reset_scroll(&mut self) {
        self.details_scroll = 0;
    }

    /// Scroll the details pane, for versions pinned by more projects than fit.
    ///
    /// Clamped to the line count so scrolling cannot run off into blank space.
    pub fn scroll_details(&mut self, delta: i16) {
        let max = self
            .selected_version_row()
            .map(|r| r.pins.len().saturating_add(4) as u16)
            .unwrap_or(0);
        self.details_scroll = self
            .details_scroll
            .saturating_add_signed(delta)
            .min(max.saturating_sub(1));
    }

    pub fn toggle_pane(&mut self) {
        self.pane = match self.pane {
            Pane::Groups => Pane::Versions,
            Pane::Versions => Pane::Details,
            Pane::Details => Pane::Groups,
        };
    }

    /// Choose the highlighted version as this group's sync target.
    pub fn choose_version(&mut self) {
        let (Some(gi), Some(row)) = (self.selected_index(), self.selected_version_row()) else {
            return;
        };
        self.status = format!("target set to {}", short(&row.version));
        self.targets.insert(gi, row.version);
    }

    /// Drop the selected group from the plan entirely.
    pub fn clear_target(&mut self) {
        if let Some(gi) = self.selected_index() {
            self.targets.remove(&gi);
            self.status = "group excluded from plan".into();
        }
    }

    /// Build the action list and move to confirmation.
    pub fn stage(&mut self) {
        let mut actions = Vec::new();
        for (&gi, target) in &self.targets {
            let g = &self.groups[gi];
            actions.extend(
                self.actionable_pins(g)
                    .filter(|p| p.version != *target)
                    .map(|p| sync::action_for(g, p, target)),
            );
        }
        if actions.is_empty() {
            self.status = "nothing to do".into();
            return;
        }
        self.pending = actions;
        self.mode = Mode::Confirm;
    }

    pub fn cancel(&mut self) {
        self.pending.clear();
        self.mode = Mode::Browse;
        self.status = "cancelled".into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::Identity;
    use crate::store::RootedPath;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn pin(project: &str, name: &str, version: &str, direct: bool) -> Pin {
        Pin {
            project: project.into(),
            input_name: name.into(),
            version: version.into(),
            last_modified: Some(1),
            direct,
        }
    }

    /// An App over one group, with `rooted` projects holding a GC root.
    fn app_with(pins: Vec<Pin>, rooted: &[&str]) -> App {
        let group = Group {
            identity: Identity {
                kind: "github".into(),
                owner: "nixos".into(),
                repo: "nixpkgs".into(),
                git_ref: String::new(),
            },
            pins,
        };
        let roots: BTreeMap<PathBuf, RootedPath> = rooted
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let sp = PathBuf::from(format!("/store/{i}"));
                (
                    sp.clone(),
                    RootedPath {
                        store_path: sp,
                        retained_by: [PathBuf::from(*p)].into_iter().collect(),
                        pinned_elsewhere: false,
                        size_bytes: 1,
                    },
                )
            })
            .collect();
        App::new(vec![group], roots, Vec::new())
    }

    #[test]
    fn nothing_is_selected_on_startup() {
        // Staging every divergent group by default would make a destructive
        // plan the path of least resistance.
        let app = app_with(
            vec![pin("/a", "nixpkgs", "one", true), pin("/b", "nixpkgs", "two", true)],
            &["/a", "/b"],
        );
        assert!(app.targets.is_empty());
        assert_eq!(app.divergent_count(), 1);
    }

    #[test]
    fn select_all_opts_into_the_suggestion() {
        let mut app = app_with(
            vec![pin("/a", "nixpkgs", "one", true), pin("/b", "nixpkgs", "two", true)],
            &["/a", "/b"],
        );
        app.select_all_suggested();
        assert_eq!(app.targets.len(), 1);
        app.select_none();
        assert!(app.targets.is_empty());
    }

    #[test]
    fn transitive_pins_do_not_create_divergence() {
        // Only a dependency's lockfile disagrees; --override-input cannot fix
        // that, so it must not be reported as actionable.
        let app = app_with(
            vec![
                pin("/a", "nixpkgs", "same", true),
                pin("/b", "nixpkgs", "same", true),
                pin("/b", "nixpkgs_2", "other", false),
            ],
            &["/a", "/b"],
        );
        let g = &app.groups[0];
        assert!(!app.is_divergent(g));
        assert_eq!(app.actionable_pins(g).count(), 2);
        assert_eq!(g.transitive_count(), 1);
    }

    #[test]
    fn unrooted_projects_are_excluded_by_default() {
        // /b holds no GC root, so converging it frees nothing and the group is
        // not worth acting on.
        let mut app = app_with(
            vec![pin("/a", "nixpkgs", "one", true), pin("/b", "nixpkgs", "two", true)],
            &["/a"],
        );
        assert!(app.rooted_only);
        let g = &app.groups[0];
        assert!(!app.is_divergent(g), "unrooted /b should not count as divergence");
        assert_eq!(app.unrooted_count(g), 1);

        // Toggling brings it back.
        app.toggle_rooted_only();
        assert!(app.is_divergent(&app.groups[0]));
        assert_eq!(app.actionable_pins(&app.groups[0]).count(), 2);
    }

    #[test]
    fn project_scope_reports_what_the_filter_hides() {
        let mut app = app_with(
            vec![
                pin("/a", "nixpkgs", "one", true),
                pin("/b", "nixpkgs", "two", true),
                pin("/c", "nixpkgs", "three", true),
            ],
            &["/a"],
        );
        // Rooted-only: one in scope, two hidden.
        assert_eq!(app.project_scope(), (1, 2));

        app.toggle_rooted_only();
        assert_eq!(app.project_scope(), (3, 0));
    }

    #[test]
    fn suggestion_ignores_unrooted_and_transitive_pins() {
        let mut newest = pin("/c", "nixpkgs_2", "transitive", false);
        newest.last_modified = Some(9_000);
        let mut unrooted = pin("/b", "nixpkgs", "unrooted", true);
        unrooted.last_modified = Some(8_000);
        let app = app_with(
            vec![pin("/a", "nixpkgs", "rooted", true), unrooted, newest],
            &["/a"],
        );
        // Only /a is both direct and rooted, so it is the only valid target.
        assert_eq!(app.suggested_target(0), None, "not divergent among actionable pins");
        assert_eq!(app.actionable_pins(&app.groups[0]).count(), 1);
    }

    #[test]
    fn all_names_shown_when_they_fit() {
        let n = names(&["alpha", "beta"]);
        assert_eq!(fit_names(&n, 40), "alpha, beta");
    }

    #[test]
    fn overflow_is_elided_with_a_remainder_count() {
        let n = names(&["alpha", "beta", "gamma", "delta"]);
        let out = fit_names(&n, 20);
        assert!(out.chars().count() <= 20, "{out:?} exceeds width");
        assert!(out.starts_with("alpha"), "{out:?} should lead with the first");
        assert!(out.contains("more"), "{out:?} should say how many are hidden");
    }

    #[test]
    fn a_single_overlong_name_is_truncated_not_dropped() {
        // The point of this column is to name projects; degrading to an empty
        // string would be worse than the count it replaced.
        let n = names(&["a-very-long-project-name-indeed", "other"]);
        let out = fit_names(&n, 16);
        assert!(out.chars().count() <= 16, "{out:?} exceeds width");
        assert!(out.starts_with('a'), "{out:?}");
        assert!(!out.trim().is_empty());
    }

    #[test]
    fn empty_input_is_empty_output() {
        assert_eq!(fit_names(&[], 10), "");
    }

    #[test]
    fn project_names_dedupe_repeated_projects() {
        // One project pinning an identity twice is one project, two pins.
        let row = VersionRow {
            version: "abc".into(),
            last_modified: None,
            pins: vec![
                Pin { project: "/x/foo".into(), input_name: "nixpkgs".into(), version: "abc".into(), last_modified: None, direct: true },
                Pin { project: "/x/foo".into(), input_name: "nixpkgs_2".into(), version: "abc".into(), last_modified: None, direct: true },
                Pin { project: "/x/bar".into(), input_name: "nixpkgs".into(), version: "abc".into(), last_modified: None, direct: true },
            ],
        };
        assert_eq!(row.count(), 3);
        assert_eq!(row.project_names(), vec!["foo".to_string(), "bar".to_string()]);
    }
}

/// Abbreviate a git revision for display.
pub fn short(version: &str) -> String {
    if version.len() > 12 && !version.starts_with("sha256-") {
        version[..12].to_string()
    } else if let Some(rest) = version.strip_prefix("sha256-") {
        format!("nar:{}", &rest[..rest.len().min(8)])
    } else {
        version.to_string()
    }
}
