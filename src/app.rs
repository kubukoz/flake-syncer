//! Application state, independent of rendering.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::diff;
use crate::lock::{Group, Identity, Pin};
use crate::nix_edit::{self, PendingEdit};
use crate::scan::Config;
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
    /// Actions are running; showing live progress.
    ///
    /// A plan can be dozens of `nix flake update` invocations, each taking
    /// seconds, so without this the TUI would sit frozen on the confirm screen
    /// for minutes and look hung.
    Running,
    /// Actions have run; showing the outcome.
    Report,
}

/// What an action is doing right now, for the running display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Rewriting `flake.nix` — fast, local, no network.
    Editing,
    /// `nix flake update` — the slow part, and usually network-bound.
    Updating,
    /// Removing a project's stale direnv GC roots.
    DroppingRoots,
}

impl Step {
    pub fn label(self) -> &'static str {
        match self {
            Step::Editing => "editing flake.nix",
            Step::Updating => "nix flake update",
            Step::DroppingRoots => "dropping GC roots",
        }
    }
}

/// Live state of an executing plan.
///
/// Held on `App` so rendering stays a pure function of state: the execute loop
/// updates this and asks for a redraw, and `ui` reads it like anything else.
#[derive(Debug, Clone)]
pub struct Running {
    /// Total units of work: one per action, plus one per project whose roots
    /// are dropped at the end.
    pub total: usize,
    pub done: usize,
    /// What is happening right now, if anything.
    pub current: Option<(Step, PathBuf, String)>,
    pub started: std::time::Instant,
    /// Outcomes so far, newest last. Mirrors what ends up in the report.
    pub log: Vec<Outcome>,
}

/// One completed step's result, kept structured so the UI can style it.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub ok: bool,
    pub text: String,
}

impl Running {
    pub fn new(total: usize) -> Self {
        Running {
            total,
            done: 0,
            current: None,
            started: std::time::Instant::now(),
            log: Vec::new(),
        }
    }

    pub fn fraction(&self) -> f64 {
        if self.total == 0 {
            1.0
        } else {
            (self.done as f64 / self.total as f64).clamp(0.0, 1.0)
        }
    }

    pub fn failures(&self) -> usize {
        self.log.iter().filter(|o| !o.ok).count()
    }

    /// Remaining time, extrapolated from the average rate so far.
    ///
    /// Blank until something has finished: `nix flake update` timings vary by
    /// an order of magnitude depending on what is already in the store, so an
    /// estimate from zero samples would be actively misleading.
    pub fn eta(&self) -> Option<std::time::Duration> {
        if self.done == 0 || self.done >= self.total {
            return None;
        }
        let per = self.started.elapsed().as_secs_f64() / self.done as f64;
        Some(std::time::Duration::from_secs_f64(
            per * (self.total - self.done) as f64,
        ))
    }
}

/// A `flake.nix` edit, rendered for review.
///
/// Holds the computed edit alongside its rendered diff so the confirm screen can
/// show exactly what will be written, and the same edit can then be applied
/// without recomputing it from a file that may have changed underneath.
#[derive(Debug, Clone)]
pub struct EditPreview {
    pub project: PathBuf,
    pub input_name: String,
    /// `Ok` holds the planned edit; `Err` explains why this project cannot be
    /// rewritten automatically, so the user can open it themselves.
    pub edit: Result<PendingEdit, String>,
    pub rendered: String,
}

impl EditPreview {
    pub fn is_error(&self) -> bool {
        self.edit.is_err()
    }
}

/// A user-declared merge: several identities of one repo, converged onto one ref.
///
/// Merging is opt-in and reversible — it is recorded here rather than applied to
/// `App::groups`, so unmerging is just dropping this entry and nothing has to be
/// re-derived from disk.
#[derive(Debug, Clone)]
pub struct Merge {
    /// The ref every member will be rewritten to declare.
    pub canonical: Identity,
    /// Indices into `App::groups` that this merge covers, canonical included.
    pub members: Vec<usize>,
}

pub struct App {
    pub groups: Vec<Group>,
    pub roots: BTreeMap<PathBuf, RootedPath>,
    pub parse_errors: Vec<(PathBuf, String)>,
    /// Active merges, keyed by the index of the canonical group.
    pub merges: BTreeMap<usize, Merge>,
    /// Groups marked for merging but not yet merged, awaiting a canonical pick.
    pub merge_marks: BTreeSet<usize>,

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
    /// Rendered diffs of the `flake.nix` edits in `pending`, shown before they
    /// are applied so nothing changes on disk unseen.
    pub pending_diffs: Vec<EditPreview>,
    pub report: Vec<String>,
    /// Live progress while a plan executes; `None` outside `Mode::Running`.
    pub running: Option<Running>,
    pub status: String,
    /// Scan configuration, carried for the configurable differ and editor.
    pub config: Config,
    /// Scroll offset of the details pane, for groups with many projects.
    pub details_scroll: u16,
}

impl App {
    pub fn new(
        groups: Vec<Group>,
        roots: BTreeMap<PathBuf, RootedPath>,
        parse_errors: Vec<(PathBuf, String)>,
        config: Config,
    ) -> Self {
        let rooted = store::rooted_projects(&roots);
        let mut app = App {
            groups,
            roots,
            parse_errors,
            merges: BTreeMap::new(),
            merge_marks: BTreeSet::new(),
            group_idx: 0,
            version_idx: 0,
            pane: Pane::Groups,
            mode: Mode::Browse,
            show_all: false,
            rooted_only: true,
            rooted,
            targets: BTreeMap::new(),
            pending: Vec::new(),
            pending_diffs: Vec::new(),
            report: Vec::new(),
            running: None,
            status: String::new(),
            config,
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
        if !self.is_divergent_at(group_index) {
            return None;
        }
        self.actionable_at(group_index)
            .into_iter()
            .max_by_key(|p| p.last_modified.unwrap_or(i64::MIN))
            .map(|p| p.version.as_str())
    }

    /// Opt into the suggested target for every divergent group at once.
    pub fn select_all_suggested(&mut self) {
        // Only listed groups: an absorbed member is represented by its merge,
        // and selecting it separately would plan the same pins twice.
        let picks: Vec<(usize, String)> = self
            .visible()
            .into_iter()
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

    /// The merge that has absorbed `group_index`, if any.
    fn merge_of(&self, group_index: usize) -> Option<&Merge> {
        self.merges
            .values()
            .find(|m| m.members.contains(&group_index))
    }

    /// Whether this group has been folded into another group's merge, and so
    /// should not be listed in its own right.
    pub fn is_absorbed(&self, group_index: usize) -> bool {
        !self.merges.contains_key(&group_index)
            && self
                .merge_of(group_index)
                .is_some_and(|m| m.members.first().is_none_or(|&f| f != group_index))
    }

    /// Every pin belonging to a group, following a merge into its members.
    ///
    /// This is what makes a merged group behave as one input everywhere else:
    /// divergence, version rows, suggestion and planning all read pins through
    /// here, so none of them need to know merging exists.
    pub fn pins_of(&self, group_index: usize) -> Vec<&Pin> {
        match self.merges.get(&group_index) {
            Some(m) => m
                .members
                .iter()
                .filter_map(|&i| self.groups.get(i))
                .flat_map(|g| g.pins.iter())
                .collect(),
            None => self
                .groups
                .get(group_index)
                .map(|g| g.pins.iter().collect())
                .unwrap_or_default(),
        }
    }

    /// The identity a group presents: its merge's canonical ref, or its own.
    pub fn identity_of(&self, group_index: usize) -> Option<&Identity> {
        match self.merges.get(&group_index) {
            Some(m) => Some(&m.canonical),
            None => self.groups.get(group_index).map(|g| &g.identity),
        }
    }

    /// Groups that could join a merge with `group_index`: same repo, different ref.
    pub fn merge_candidates(&self, group_index: usize) -> Vec<usize> {
        let Some(key) = self
            .groups
            .get(group_index)
            .map(|g| g.identity.merge_key())
        else {
            return Vec::new();
        };
        (0..self.groups.len())
            .filter(|&i| i != group_index)
            .filter(|&i| !self.is_absorbed(i) && !self.merges.contains_key(&i))
            .filter(|&i| self.groups[i].identity.merge_key() == key)
            // A ref with nothing actionable in the current scope cannot
            // contribute to a merge, so offering it would be a dead end.
            .filter(|&i| !self.actionable_at(i).is_empty())
            .collect()
    }

    /// Mark or unmark the highlighted group as a merge participant.
    ///
    /// Marking is separate from merging so several refs can be chosen before
    /// committing, and so the canonical one is picked explicitly rather than
    /// inferred from cursor position.
    pub fn toggle_merge_mark(&mut self) {
        let Some(gi) = self.selected_index() else { return };
        if self.merges.contains_key(&gi) {
            self.unmerge(gi);
            return;
        }
        if self.merge_marks.remove(&gi) {
            self.status = format!("{} unmarked", self.groups[gi].identity);
            return;
        }

        // Only same-repo groups can merge; marking across repos is a mistake
        // worth refusing loudly rather than discovering at commit time.
        let key = self.groups[gi].identity.merge_key();
        if let Some(&other) = self
            .merge_marks
            .iter()
            .find(|&&o| self.groups[o].identity.merge_key() != key)
        {
            self.status = format!(
                "cannot merge {} with {} — different repos",
                self.groups[gi].identity, self.groups[other].identity
            );
            return;
        }
        self.merge_marks.insert(gi);
        let n = self.merge_marks.len();
        self.status = if n < 2 {
            format!("{} marked — mark another ref, then M to merge", self.groups[gi].identity)
        } else {
            format!("{n} refs marked — M merges them onto the highlighted one")
        };
    }

    /// Merge the marked groups, adopting the highlighted group's ref as canonical.
    pub fn commit_merge(&mut self) {
        let Some(gi) = self.selected_index() else { return };
        if self.merge_marks.len() < 2 {
            self.status = "mark at least two refs with m before merging".into();
            return;
        }
        if !self.merge_marks.contains(&gi) {
            self.status = "highlight one of the marked refs — it becomes the canonical one".into();
            return;
        }

        // Canonical first, so the merged group is listed at its position.
        let mut members = vec![gi];
        members.extend(self.merge_marks.iter().copied().filter(|&i| i != gi));

        let canonical = self.groups[gi].identity.clone();
        let n = members.len();
        self.merges.insert(gi, Merge { canonical: canonical.clone(), members });
        self.merge_marks.clear();
        // Selections keyed by absorbed groups no longer denote anything listed.
        self.drop_stale_targets();
        self.group_idx = 0;
        self.version_idx = 0;
        self.status = format!("merged {n} refs onto {canonical}");
    }

    /// Undo a merge, restoring its members as independent groups.
    pub fn unmerge(&mut self, group_index: usize) {
        if let Some(m) = self.merges.remove(&group_index) {
            self.targets.remove(&group_index);
            self.status = format!("unmerged {} ({} refs)", m.canonical, m.members.len());
            self.group_idx = 0;
            self.version_idx = 0;
        }
    }

    /// Drop selections that no longer name a listed, divergent group.
    fn drop_stale_targets(&mut self) {
        let stale: Vec<usize> = self
            .targets
            .keys()
            .copied()
            .filter(|&gi| self.is_absorbed(gi) || !self.is_divergent_at(gi))
            .collect();
        for gi in stale {
            self.targets.remove(&gi);
        }
    }

    /// Projects whose `flake.nix` must change for a merge: those declaring a ref
    /// other than the canonical one.
    ///
    /// Pins already on the canonical ref need no edit — only a revision sync,
    /// which the existing machinery handles.
    pub fn ref_changes(&self, group_index: usize) -> Vec<(&Pin, &Identity)> {
        let Some(m) = self.merges.get(&group_index) else {
            return Vec::new();
        };
        m.members
            .iter()
            .filter_map(|&i| self.groups.get(i))
            .filter(|g| g.identity != m.canonical)
            .flat_map(|g| {
                g.pins
                    .iter()
                    .filter(|p| self.is_actionable(p))
                    .map(move |p| (p, &g.identity))
            })
            .collect()
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

    /// Actionable pins of a group *by index*, so merged members are included.
    pub fn actionable_at(&self, group_index: usize) -> Vec<&Pin> {
        self.pins_of(group_index)
            .into_iter()
            .filter(|p| self.is_actionable(p))
            .collect()
    }

    /// Distinct versions among a group's actionable pins.
    pub fn distinct_versions(&self, group: &Group) -> usize {
        let mut v: Vec<&str> = self.actionable_pins(group).map(|p| p.version.as_str()).collect();
        v.sort_unstable();
        v.dedup();
        v.len()
    }

    /// Distinct versions of a group by index, across a merge's members.
    pub fn distinct_versions_at(&self, group_index: usize) -> usize {
        let mut v: Vec<&str> = self
            .actionable_at(group_index)
            .into_iter()
            .map(|p| p.version.as_str())
            .collect();
        v.sort_unstable();
        v.dedup();
        v.len()
    }

    pub fn is_divergent(&self, group: &Group) -> bool {
        self.distinct_versions(group) > 1
    }

    /// Whether a group diverges, counting a merge's members together.
    ///
    /// A merge can create divergence where neither member had any: two projects
    /// each internally consistent but on different refs and revisions.
    pub fn is_divergent_at(&self, group_index: usize) -> bool {
        self.distinct_versions_at(group_index) > 1
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
    ///
    /// Groups absorbed into another group's merge are not listed on their own —
    /// the merged group stands for all of them.
    pub fn visible(&self) -> Vec<usize> {
        (0..self.groups.len())
            .filter(|&i| !self.is_absorbed(i))
            .filter(|&i| self.show_all || self.is_divergent_at(i))
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
        let Some(gi) = self.selected_index() else {
            return Vec::new();
        };
        let mut by_version: BTreeMap<&str, VersionRow> = BTreeMap::new();
        for p in self.actionable_at(gi) {
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
            for p in self.actionable_at(gi) {
                if p.version != *target {
                    projects.insert(p.project.clone());
                }
            }
        }
        store::reclaimable(&self.roots, &projects)
    }

    pub fn divergent_count(&self) -> usize {
        self.visible()
            .into_iter()
            .filter(|&i| self.is_divergent_at(i))
            .count()
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
    ///
    /// A merged group yields two kinds of action: pins already on the canonical
    /// ref need only a revision sync, while pins on a different ref also need
    /// their `flake.nix` rewritten. A pin on a non-canonical ref is included
    /// even when its revision already matches the target — the declaration is
    /// still wrong, and leaving it would silently un-merge on the next update.
    pub fn stage(&mut self) {
        let mut actions = Vec::new();
        for (&gi, target) in &self.targets {
            match self.merges.get(&gi) {
                Some(m) => {
                    for &mi in &m.members {
                        let g = &self.groups[mi];
                        let same_ref = g.identity == m.canonical;
                        for p in self.actionable_pins(g) {
                            if same_ref {
                                if p.version != *target {
                                    actions.push(sync::action_for(g, p, target));
                                }
                            } else {
                                actions.push(sync::merge_action_for(
                                    &m.canonical,
                                    &g.identity,
                                    p,
                                    target,
                                ));
                            }
                        }
                    }
                }
                None => {
                    let g = &self.groups[gi];
                    actions.extend(
                        self.actionable_pins(g)
                            .filter(|p| p.version != *target)
                            .map(|p| sync::action_for(g, p, target)),
                    );
                }
            }
        }
        if actions.is_empty() {
            self.status = "nothing to do".into();
            return;
        }
        // flake.nix rewrites first: they are the reviewable part of the plan.
        actions.sort_by_key(|a| (!a.changes_ref(), a.project.clone()));
        self.pending = actions;
        self.pending_diffs = self.compute_diffs();
        self.mode = Mode::Confirm;
    }

    /// Compute and render the `flake.nix` diff for every ref-changing action.
    ///
    /// Done at stage time rather than at execute time so the confirm screen
    /// shows the real before/after of each file — including the projects that
    /// cannot be rewritten, which the user needs to know about *before*
    /// approving a plan, not after it half-runs.
    fn compute_diffs(&self) -> Vec<EditPreview> {
        self.pending
            .iter()
            .filter_map(|a| {
                let url = a.target_url()?;
                let preview = match nix_edit::plan_edit(&a.project, &a.input_name, &url) {
                    Ok(edit) => {
                        let d = diff::unified(&edit.path, &edit.before, &edit.after);
                        EditPreview {
                            project: a.project.clone(),
                            input_name: a.input_name.clone(),
                            rendered: diff::render(&d, &self.config.differ),
                            edit: Ok(edit),
                        }
                    }
                    Err(e) => EditPreview {
                        project: a.project.clone(),
                        input_name: a.input_name.clone(),
                        rendered: String::new(),
                        edit: Err(e.to_string()),
                    },
                };
                Some(preview)
            })
            .collect()
    }

    /// The planned edit for a project, if it was computed and is applicable.
    pub fn edit_for(&self, project: &std::path::Path, input_name: &str) -> Option<&PendingEdit> {
        self.pending_diffs
            .iter()
            .find(|p| p.project == project && p.input_name == input_name)
            .and_then(|p| p.edit.as_ref().ok())
    }

    /// Projects in the plan whose `flake.nix` could not be rewritten.
    pub fn unwritable(&self) -> Vec<&EditPreview> {
        self.pending_diffs.iter().filter(|p| p.is_error()).collect()
    }

    /// Enter `Mode::Running` with a work total to count against.
    ///
    /// `extra` covers the post-update root dropping, which is real work the
    /// user waits on and so must be part of the denominator — a bar that hits
    /// 100% and then keeps going is worse than no bar.
    pub fn begin_running(&mut self, extra: usize) {
        self.running = Some(Running::new(self.pending.len() + extra));
        self.mode = Mode::Running;
    }

    /// Note what is starting, so the display can name it while it blocks.
    pub fn step_started(&mut self, step: Step, project: &std::path::Path, input: &str) {
        if let Some(r) = &mut self.running {
            r.current = Some((step, project.to_path_buf(), input.to_string()));
        }
    }

    /// Record a finished step. `counts` is false for sub-steps that share an
    /// action's slot in the total, such as the edit that precedes an update.
    pub fn step_finished(&mut self, ok: bool, text: String, counts: bool) {
        if let Some(r) = &mut self.running {
            r.log.push(Outcome { ok, text });
            if counts {
                r.done += 1;
            }
            r.current = None;
        }
    }

    pub fn cancel(&mut self) {
        self.pending.clear();
        self.pending_diffs.clear();
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
            declared_name: direct.then(|| name.to_string()),
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
        App::new(vec![group], roots, Vec::new(), Config::default())
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

    /// An App over several groups of the same repo on different refs — the
    /// exact situation merging exists for.
    fn app_with_refs(specs: &[(&str, Vec<Pin>)], rooted: &[&str]) -> App {
        let groups = specs
            .iter()
            .map(|(git_ref, pins)| Group {
                identity: Identity {
                    kind: "github".into(),
                    owner: "nixos".into(),
                    repo: "nixpkgs".into(),
                    git_ref: (*git_ref).into(),
                },
                pins: pins.clone(),
            })
            .collect();
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
        App::new(groups, roots, Vec::new(), Config::default())
    }

    /// Mark `indices` and merge them onto `canonical`.
    fn merge(app: &mut App, canonical: usize, indices: &[usize]) {
        for &i in indices {
            app.merge_marks.insert(i);
        }
        let visible = app.visible();
        app.group_idx = visible.iter().position(|&v| v == canonical).unwrap();
        app.commit_merge();
    }

    #[test]
    fn merging_is_opt_in() {
        // Two refs of the same repo stay separate until the user says otherwise:
        // converging them silently would change which branch a project follows.
        let app = app_with_refs(
            &[
                ("nixpkgs-unstable", vec![pin("/a", "nixpkgs", "one", true)]),
                ("nixos-unstable", vec![pin("/b", "nixpkgs", "two", true)]),
            ],
            &["/a", "/b"],
        );
        assert!(app.merges.is_empty());
        assert_eq!(app.visible().len(), 0, "neither group diverges on its own");
        assert_eq!(app.divergent_count(), 0);
    }

    #[test]
    fn merging_two_refs_creates_one_divergent_group() {
        let mut app = app_with_refs(
            &[
                ("nixpkgs-unstable", vec![pin("/a", "nixpkgs", "one", true)]),
                ("nixos-unstable", vec![pin("/b", "nixpkgs", "two", true)]),
            ],
            &["/a", "/b"],
        );
        app.show_all = true;
        merge(&mut app, 0, &[0, 1]);

        // One listing, standing for both, and now divergent.
        assert_eq!(app.visible(), vec![0]);
        assert!(app.is_absorbed(1));
        assert!(app.is_divergent_at(0));
        assert_eq!(app.actionable_at(0).len(), 2);
        assert_eq!(app.identity_of(0).unwrap().git_ref, "nixpkgs-unstable");
    }

    #[test]
    fn unmerging_restores_both_groups() {
        let mut app = app_with_refs(
            &[
                ("nixpkgs-unstable", vec![pin("/a", "nixpkgs", "one", true)]),
                ("nixos-unstable", vec![pin("/b", "nixpkgs", "two", true)]),
            ],
            &["/a", "/b"],
        );
        app.show_all = true;
        merge(&mut app, 0, &[0, 1]);
        app.unmerge(0);

        assert!(app.merges.is_empty());
        assert_eq!(app.visible(), vec![0, 1]);
        assert!(!app.is_absorbed(1));
    }

    #[test]
    fn only_off_ref_pins_need_a_flake_nix_edit() {
        // /a already declares the canonical ref, so it needs a revision sync
        // only; /b declares a different one and must be rewritten.
        let mut app = app_with_refs(
            &[
                ("nixpkgs-unstable", vec![pin("/a", "nixpkgs", "one", true)]),
                ("nixos-unstable", vec![pin("/b", "nixpkgs", "two", true)]),
            ],
            &["/a", "/b"],
        );
        app.show_all = true;
        merge(&mut app, 0, &[0, 1]);

        let changes = app.ref_changes(0);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].0.project, PathBuf::from("/b"));
        assert_eq!(changes[0].1.git_ref, "nixos-unstable");
    }

    #[test]
    fn staging_a_merge_emits_ref_edits_and_rev_syncs() {
        let mut app = app_with_refs(
            &[
                ("nixpkgs-unstable", vec![pin("/a", "nixpkgs", "one", true)]),
                ("nixos-unstable", vec![pin("/b", "nixpkgs", "two", true)]),
            ],
            &["/a", "/b"],
        );
        app.show_all = true;
        merge(&mut app, 0, &[0, 1]);
        app.targets.insert(0, "one".into());
        app.stage();

        assert_eq!(app.mode, Mode::Confirm);
        // /b changes ref; /a is already on target and needs nothing.
        assert_eq!(app.pending.len(), 1);
        let a = &app.pending[0];
        assert!(a.changes_ref());
        assert_eq!(a.project, PathBuf::from("/b"));
        assert_eq!(
            a.target_url().as_deref(),
            Some("github:nixos/nixpkgs/nixpkgs-unstable")
        );
    }

    #[test]
    fn an_off_ref_pin_is_staged_even_when_its_revision_matches() {
        // Same rev, wrong declaration: skipping it would leave flake.nix saying
        // something the merge was meant to change, and the next `nix flake
        // update` would drift straight back.
        let mut app = app_with_refs(
            &[
                ("nixpkgs-unstable", vec![pin("/a", "nixpkgs", "same", true)]),
                ("nixos-unstable", vec![pin("/b", "nixpkgs", "same", true)]),
            ],
            &["/a", "/b"],
        );
        app.show_all = true;
        merge(&mut app, 0, &[0, 1]);
        app.targets.insert(0, "same".into());
        app.stage();

        assert_eq!(app.pending.len(), 1);
        assert!(app.pending[0].changes_ref());
        assert_eq!(app.pending[0].project, PathBuf::from("/b"));
    }

    #[test]
    fn merging_across_different_repos_is_refused() {
        let mut app = app_with(vec![pin("/a", "nixpkgs", "one", true)], &["/a"]);
        app.groups.push(Group {
            identity: Identity {
                kind: "github".into(),
                owner: "numtide".into(),
                repo: "flake-utils".into(),
                git_ref: String::new(),
            },
            pins: vec![pin("/b", "flake-utils", "two", true)],
        });
        app.show_all = true;
        app.merge_marks.insert(0);
        app.group_idx = 1;
        app.toggle_merge_mark();

        assert_eq!(app.merge_marks.len(), 1, "the second mark must be refused");
        assert!(app.status.contains("different repos"), "{}", app.status);
    }

    #[test]
    fn merge_candidates_are_same_repo_other_refs() {
        let app = app_with_refs(
            &[
                ("nixpkgs-unstable", vec![pin("/a", "nixpkgs", "one", true)]),
                ("nixos-unstable", vec![pin("/b", "nixpkgs", "two", true)]),
                ("", vec![pin("/c", "nixpkgs", "three", true)]),
            ],
            &["/a", "/b", "/c"],
        );
        assert_eq!(app.merge_candidates(0), vec![1, 2]);
    }

    #[test]
    fn a_ref_with_nothing_actionable_is_not_offered_as_a_candidate() {
        // /c holds no GC root, so under the default filter that ref contributes
        // nothing — offering it would advertise a merge that changes nothing.
        let app = app_with_refs(
            &[
                ("nixpkgs-unstable", vec![pin("/a", "nixpkgs", "one", true)]),
                ("nixos-unstable", vec![pin("/b", "nixpkgs", "two", true)]),
                ("nixos-24.11", vec![pin("/c", "nixpkgs", "three", true)]),
            ],
            &["/a", "/b"],
        );
        assert!(app.rooted_only);
        assert_eq!(app.merge_candidates(0), vec![1], "the unrooted ref is not offered");

        // Including unrooted projects brings it back.
        let mut app = app;
        app.toggle_rooted_only();
        assert_eq!(app.merge_candidates(0), vec![1, 2]);
    }

    #[test]
    fn a_merge_needs_two_marks() {
        let mut app = app_with_refs(
            &[
                ("nixpkgs-unstable", vec![pin("/a", "nixpkgs", "one", true)]),
                ("nixos-unstable", vec![pin("/b", "nixpkgs", "two", true)]),
            ],
            &["/a", "/b"],
        );
        app.show_all = true;
        app.merge_marks.insert(0);
        app.group_idx = 0;
        app.commit_merge();
        assert!(app.merges.is_empty(), "one mark is not a merge");
    }

    #[test]
    fn the_bar_accounts_for_root_dropping_too() {
        // Root dropping is real work the user waits on. If it were outside the
        // total, the bar would reach 100% and then keep going.
        let mut app = app_with(vec![pin("/a", "nixpkgs", "one", true)], &["/a"]);
        app.pending = vec![sync::action_for(
            &app.groups[0],
            &app.groups[0].pins[0],
            "two",
        )];
        app.begin_running(1);

        let r = app.running.as_ref().unwrap();
        assert_eq!(r.total, 2, "one action plus one root drop");
        assert_eq!(r.done, 0);
        assert_eq!(app.mode, Mode::Running);
    }

    #[test]
    fn an_edit_shares_its_actions_slot_with_the_update() {
        // The flake.nix edit and the update that follows are one action; if the
        // edit advanced the counter the bar would overshoot its total.
        let mut app = app_with(vec![pin("/a", "nixpkgs", "one", true)], &["/a"]);
        app.pending = vec![sync::action_for(
            &app.groups[0],
            &app.groups[0].pins[0],
            "two",
        )];
        app.begin_running(0);

        app.step_finished(true, "edited".into(), false);
        assert_eq!(app.running.as_ref().unwrap().done, 0, "the edit does not count");
        app.step_finished(true, "updated".into(), true);
        assert_eq!(app.running.as_ref().unwrap().done, 1);
        assert_eq!(app.running.as_ref().unwrap().fraction(), 1.0);
    }

    #[test]
    fn failures_are_counted_and_the_run_continues() {
        // One project failing must not stop the rest: the log records it and
        // the count still advances, so the bar reflects work attempted.
        let mut app = app_with(vec![pin("/a", "nixpkgs", "one", true)], &["/a"]);
        app.pending.clear();
        app.begin_running(3);

        app.step_finished(true, "ok a".into(), true);
        app.step_finished(false, "fail b".into(), true);
        app.step_finished(true, "ok c".into(), true);

        let r = app.running.as_ref().unwrap();
        assert_eq!(r.done, 3);
        assert_eq!(r.failures(), 1);
        assert_eq!(r.log.len(), 3);
    }

    #[test]
    fn the_current_step_is_cleared_when_it_finishes() {
        // A stale "▶ updating X" left on screen after X completed would name
        // the wrong project during the next one's wait.
        let mut app = app_with(vec![pin("/a", "nixpkgs", "one", true)], &["/a"]);
        app.pending.clear();
        app.begin_running(1);

        app.step_started(Step::Updating, std::path::Path::new("/a"), "nixpkgs");
        let cur = app.running.as_ref().unwrap().current.clone();
        assert_eq!(cur.unwrap().0, Step::Updating);

        app.step_finished(true, "done".into(), true);
        assert!(app.running.as_ref().unwrap().current.is_none());
    }

    #[test]
    fn eta_waits_for_a_sample() {
        // nix update times vary by an order of magnitude depending on what is
        // already in the store, so an estimate from zero samples would mislead.
        let mut r = Running::new(4);
        assert_eq!(r.eta(), None, "no ETA before anything finishes");
        r.done = 4;
        assert_eq!(r.eta(), None, "no ETA once complete");
        r.done = 2;
        assert!(r.eta().is_some(), "an ETA once there is a rate to use");
    }

    #[test]
    fn an_empty_plan_is_complete_not_divided_by_zero() {
        let r = Running::new(0);
        assert_eq!(r.fraction(), 1.0);
    }

    #[test]
    fn identity_url_round_trips_the_declared_ref() {
        let with_ref = Identity {
            kind: "github".into(),
            owner: "nixos".into(),
            repo: "nixpkgs".into(),
            git_ref: "nixos-unstable".into(),
        };
        assert_eq!(with_ref.url(), "github:nixos/nixpkgs/nixos-unstable");

        let bare = Identity { git_ref: String::new(), ..with_ref };
        assert_eq!(bare.url(), "github:nixos/nixpkgs");
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
                Pin { project: "/x/foo".into(), input_name: "nixpkgs".into(), version: "abc".into(), last_modified: None, direct: true , declared_name: Some("nixpkgs".into()) },
                Pin { project: "/x/foo".into(), input_name: "nixpkgs_2".into(), version: "abc".into(), last_modified: None, direct: true , declared_name: Some("nixpkgs_2".into()) },
                Pin { project: "/x/bar".into(), input_name: "nixpkgs".into(), version: "abc".into(), last_modified: None, direct: true , declared_name: Some("nixpkgs".into()) },
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



/// Constructors used by `ui`'s render tests, which need a populated `App`.
#[cfg(test)]
pub mod test_support {
    use super::*;
    use crate::lock::{Group, Identity};

    pub fn app_for_render() -> App {
        let identity = Identity {
            kind: "github".into(),
            owner: "nixos".into(),
            repo: "nixpkgs".into(),
            git_ref: "nixpkgs-unstable".into(),
        };
        let group = Group {
            identity,
            pins: vec![Pin {
                project: "/Users/k/projects/myapp".into(),
                input_name: "nixpkgs".into(),
                version: "abc123".into(),
                last_modified: Some(1),
                direct: true,
                declared_name: Some("nixpkgs".into()),
            }],
        };
        App::new(vec![group], BTreeMap::new(), Vec::new(), Config::default())
    }
}
