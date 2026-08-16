//! Application state, independent of rendering.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::lock::Group;
use crate::store::{self, RootedPath};
use crate::sync::{self, Action};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Groups,
    Versions,
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

    /// Per-group chosen target version, keyed by index into `groups`.
    pub targets: BTreeMap<usize, String>,
    pub pending: Vec<Action>,
    pub report: Vec<String>,
    pub status: String,
}

impl App {
    pub fn new(
        groups: Vec<Group>,
        roots: BTreeMap<PathBuf, RootedPath>,
        parse_errors: Vec<(PathBuf, String)>,
    ) -> Self {
        let mut app = App {
            groups,
            roots,
            parse_errors,
            group_idx: 0,
            version_idx: 0,
            pane: Pane::Groups,
            mode: Mode::Browse,
            show_all: false,
            targets: BTreeMap::new(),
            pending: Vec::new(),
            report: Vec::new(),
            status: String::new(),
        };
        app.seed_default_targets();
        app
    }

    /// Default every divergent group to its newest already-pinned version.
    fn seed_default_targets(&mut self) {
        for (i, g) in self.groups.iter().enumerate() {
            if g.is_divergent() {
                if let Some(p) = g.newest_in_use() {
                    self.targets.insert(i, p.version.clone());
                }
            }
        }
    }

    /// Indices of the groups currently listed, honouring the divergent filter.
    pub fn visible(&self) -> Vec<usize> {
        self.groups
            .iter()
            .enumerate()
            .filter(|(_, g)| self.show_all || g.is_divergent())
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

    /// Versions of the selected group, newest first, with pin counts.
    pub fn version_rows(&self) -> Vec<(String, usize, Option<i64>)> {
        let Some(g) = self.selected_group() else {
            return Vec::new();
        };
        let mut by_version: BTreeMap<&str, (usize, Option<i64>)> = BTreeMap::new();
        for p in &g.pins {
            let e = by_version.entry(p.version.as_str()).or_insert((0, p.last_modified));
            e.0 += 1;
            if p.last_modified > e.1 {
                e.1 = p.last_modified;
            }
        }
        let mut rows: Vec<(String, usize, Option<i64>)> = by_version
            .into_iter()
            .map(|(v, (n, lm))| (v.to_string(), n, lm))
            .collect();
        rows.sort_by(|a, b| b.2.cmp(&a.2));
        rows
    }

    /// Bytes freed if the current selections are applied and the affected
    /// projects' direnv roots are dropped.
    pub fn estimated_savings(&self) -> (u64, usize) {
        let mut projects: BTreeSet<PathBuf> = BTreeSet::new();
        for (&gi, target) in &self.targets {
            for p in &self.groups[gi].pins {
                if p.version != *target {
                    projects.insert(p.project.clone());
                }
            }
        }
        store::reclaimable(&self.roots, &projects)
    }

    pub fn divergent_count(&self) -> usize {
        self.groups.iter().filter(|g| g.is_divergent()).count()
    }

    pub fn on_up(&mut self) {
        match self.pane {
            Pane::Groups => self.group_idx = self.group_idx.saturating_sub(1),
            Pane::Versions => self.version_idx = self.version_idx.saturating_sub(1),
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
            }
            Pane::Versions => {
                let n = self.version_rows().len();
                if n > 0 && self.version_idx + 1 < n {
                    self.version_idx += 1;
                }
            }
        }
    }

    pub fn toggle_pane(&mut self) {
        self.pane = match self.pane {
            Pane::Groups => Pane::Versions,
            Pane::Versions => Pane::Groups,
        };
    }

    /// Choose the highlighted version as this group's sync target.
    pub fn choose_version(&mut self) {
        let (Some(gi), Some(row)) = (
            self.selected_index(),
            self.version_rows().get(self.version_idx).cloned(),
        ) else {
            return;
        };
        self.targets.insert(gi, row.0.clone());
        self.status = format!("target set to {}", short(&row.0));
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
            actions.extend(sync::plan(&self.groups[gi], target));
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
