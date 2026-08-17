//! Nix store accounting: what a pin actually costs on disk, and what
//! deduplicating it would really free.
//!
//! The key fact driving this module: aligning two lockfiles frees nothing by
//! itself. The bytes are held by GC roots — overwhelmingly `direnv`'s
//! `.direnv/flake-inputs/<store-path>` symlinks, which pin each project's
//! inputs so the garbage collector cannot touch them. A source path stops
//! costing anything only once *every* root referencing it is gone.
//!
//! So savings are only claimed for a store path that is rooted by exactly the
//! projects we are about to migrate away from it. A path shared with a project
//! we are not touching stays put, and reporting it as reclaimable would be a
//! lie.

use crate::progress::Progress;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// A store path retained by at least one GC root.
#[derive(Debug, Clone)]
pub struct RootedPath {
    /// Retained for diagnostics; the map key carries the same value.
    #[allow(dead_code)]
    pub store_path: PathBuf,
    /// Projects whose `.direnv/flake-inputs` roots retain this path. Dropping
    /// all of these is within the tool's power.
    pub retained_by: BTreeSet<PathBuf>,
    /// Roots outside direnv that hold this path: profiles, `result` symlinks,
    /// running processes (`{lsof}`). Such a path cannot be reclaimed by editing
    /// lockfiles, whatever we do to the direnv roots.
    ///
    /// Kept by name rather than as a flag so the roots view can answer *what*
    /// is pinning a path, which is the question a bare bool cannot.
    pub external_roots: BTreeSet<String>,
    pub size_bytes: u64,
}

impl RootedPath {
    /// Whether anything outside direnv holds this path.
    ///
    /// Process roots count here even though the view hides them by default: a
    /// path a process has open is genuinely not reclaimable right now, and
    /// letting the filter reach this would make the tool overstate savings.
    pub fn pinned_elsewhere(&self) -> bool {
        !self.external_roots.is_empty()
    }

    /// External roots that persist: profiles, `result` symlinks, channels.
    pub fn durable_roots(&self) -> impl Iterator<Item = &String> {
        self.external_roots.iter().filter(|r| !is_process_root(r))
    }

    /// External roots belonging to running processes.
    pub fn process_roots(&self) -> impl Iterator<Item = &String> {
        self.external_roots.iter().filter(|r| is_process_root(r))
    }
}

/// Parse one `nix-store --gc --print-roots` line: `"<root>" -> <store path>`.
///
/// Roots the daemon censors (`{lsof}`, `{censored}`) name no filesystem path
/// but still keep the target alive, so they count as external pins.
fn parse_root_line(line: &str) -> Option<(String, PathBuf)> {
    let (root, target) = line.rsplit_once(" -> ")?;
    let root = root.trim().trim_matches('"').to_string();
    let target = target.trim();
    if !target.starts_with("/nix/store/") {
        return None;
    }
    Some((root, PathBuf::from(target)))
}

/// The project a direnv flake-input root belongs to, if it is one.
fn direnv_project(root: &str) -> Option<PathBuf> {
    let (project, rest) = root.split_once("/.direnv/")?;
    rest.contains("flake-inputs/")
        .then(|| PathBuf::from(project))
}

/// Whether a root is a running process rather than a durable pin.
///
/// The daemon censors these to `{lsof}`, `{temp:<pid>}` and `{censored}`: they
/// name no filesystem path and vanish when the process exits, so they say
/// nothing about whether a path is durably held. Treated separately from
/// profiles and `result` symlinks, which persist across reboots and are the
/// external pins actually worth reading.
pub fn is_process_root(root: &str) -> bool {
    root.starts_with('{') && root.ends_with('}')
}

/// Ask the Nix daemon for the authoritative GC root set.
///
/// This must come from `nix-store --gc --print-roots` rather than a walk of
/// `/nix/var/nix/gcroots/auto`: profiles, `result` symlinks and running
/// processes also root store paths, and a scan blind to them reports paths as
/// reclaimable when something else is still holding them.
///
/// Not a pure read, despite being the scan phase: `--print-roots` makes Nix
/// prune `gcroots/auto` symlinks that already point at deleted paths, logging
/// each to stderr. Only dangling pointers go and no repository file is touched,
/// but this runs on every startup, so it is worth knowing that a "read-only"
/// invocation still writes GC state.
///
/// Returns an empty map if Nix is unavailable, which degrades to "no savings
/// claimed" rather than to an overstatement.
pub fn scan_roots() -> BTreeMap<PathBuf, RootedPath> {
    use rayon::prelude::*;

    let links = Progress::new("gc roots", None);

    let output = std::process::Command::new("nix-store")
        .args(["--gc", "--print-roots"])
        .output();
    let Ok(output) = output else {
        links.finish();
        return BTreeMap::new();
    };
    let stdout = String::from_utf8_lossy(&output.stdout);

    let mut retainers: BTreeMap<PathBuf, BTreeSet<PathBuf>> = BTreeMap::new();
    let mut external: BTreeMap<PathBuf, BTreeSet<String>> = BTreeMap::new();

    for line in stdout.lines() {
        let Some((root, target)) = parse_root_line(line) else {
            continue;
        };
        // A stale root points at a path that no longer exists.
        let Ok(real) = std::fs::canonicalize(&target) else {
            continue;
        };

        match direnv_project(&root) {
            Some(project) => {
                retainers.entry(real).or_default().insert(project);
            }
            None => {
                external.entry(real).or_default().insert(root);
            }
        }
        links.advance(1);
    }
    links.set(retainers.len());
    links.finish();

    // Every rooted path, however it is held. Paths rooted only from outside
    // direnv are included so the roots view can show what pins them; they still
    // contribute nothing to `reclaimable`, which filters them out on its own.
    let all_paths: Vec<PathBuf> = retainers
        .keys()
        .chain(external.keys())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    // Sizing dominates the scan (it walks every file of every rooted source),
    // so each unique store path is measured once, in parallel. This is also the
    // one phase slow enough to need an ETA, and the count is known up front, so
    // the estimate is a real one.
    let sizing = Progress::new("sizing paths", Some(all_paths.len()));
    let sizes: Vec<(PathBuf, u64)> = all_paths
        .into_par_iter()
        .map(|p| {
            let size = dir_size(&p);
            sizing.advance(1);
            (p, size)
        })
        .collect();
    sizing.finish();

    sizes
        .into_iter()
        .map(|(path, size_bytes)| {
            let retained_by = retainers.get(&path).cloned().unwrap_or_default();
            let external_roots = external.get(&path).cloned().unwrap_or_default();
            (
                path.clone(),
                RootedPath { store_path: path, retained_by, external_roots, size_bytes },
            )
        })
        .collect()
}

/// Apparent size of a store path, following no symlinks.
///
/// This is deliberately a plain byte sum rather than an allocated-blocks or
/// hardlink-aware figure: Nix hardlinks identical files across store paths when
/// optimisation is enabled, so exact accounting would require inode bookkeeping
/// the estimate does not need. Reported as an upper bound.
fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    for entry in walkdir::WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if let Ok(meta) = entry.metadata() {
            if meta.is_file() {
                total += meta.len();
            }
        }
    }
    total
}

/// Projects that retain at least one store path via a GC root.
///
/// A project absent from this set holds nothing: converging its lockfile frees
/// no disk space, which is the only reason this tool edits lockfiles at all.
pub fn rooted_projects(roots: &BTreeMap<PathBuf, RootedPath>) -> BTreeSet<PathBuf> {
    roots
        .values()
        .flat_map(|r| r.retained_by.iter().cloned())
        .collect()
}

/// How many bytes would actually be freed by migrating `projects` off the
/// versions they currently pin.
///
/// A rooted path counts only when every project retaining it is in `projects`
/// and nothing outside direnv pins it; otherwise something keeps it alive and
/// nothing is freed.
pub fn reclaimable(
    roots: &BTreeMap<PathBuf, RootedPath>,
    projects: &BTreeSet<PathBuf>,
) -> (u64, usize) {
    let mut bytes = 0;
    let mut count = 0;
    for rooted in roots.values() {
        if rooted.pinned_elsewhere() {
            continue;
        }
        if !rooted.retained_by.is_empty() && rooted.retained_by.is_subset(projects) {
            bytes += rooted.size_bytes;
            count += 1;
        }
    }
    (bytes, count)
}

/// One row of the roots view: a store path and everything keeping it alive.
///
/// This is the inverse of `nix-store --gc --print-roots`, which lists roots and
/// leaves you to work out what each one holds. Here the store path is the key
/// and its retainers are the value, so "why is this still on disk" is a lookup
/// rather than a grep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootRow {
    pub store_path: PathBuf,
    pub size_bytes: u64,
    /// Projects holding this via `.direnv/flake-inputs`.
    pub projects: Vec<PathBuf>,
    /// Durable external roots: profiles, `result` symlinks, channels.
    pub external: Vec<String>,
    /// Running-process roots (`{lsof}`, `{temp:N}`).
    ///
    /// Separated from `external` because they are noise for the question the
    /// view asks: a process holding a path says nothing about whether the path
    /// is durably pinned, and on a dev machine they swamp the list.
    pub process: Vec<String>,
}

impl RootRow {
    /// Total number of roots holding this path, process roots included.
    pub fn retainer_count(&self) -> usize {
        self.projects.len() + self.external.len() + self.process.len()
    }

    /// Whether dropping direnv roots alone could free this path.
    ///
    /// A process root blocks this as surely as a profile does — it just stops
    /// blocking when the process exits.
    pub fn freeable(&self) -> bool {
        self.external.is_empty() && self.process.is_empty() && !self.projects.is_empty()
    }

    /// Whether only a running process stands between this path and reclamation.
    ///
    /// Worth surfacing separately: unlike a profile, this resolves on its own.
    pub fn blocked_only_by_process(&self) -> bool {
        self.external.is_empty() && !self.process.is_empty() && !self.projects.is_empty()
    }

    /// Whether a running process is the *only* thing holding this path at all.
    ///
    /// Nothing in this tool's remit touches such a path, and with process roots
    /// hidden the row would render with no retainers listed — an unexplained
    /// entry, which is worse than an absent one.
    pub fn process_only(&self) -> bool {
        self.projects.is_empty() && self.external.is_empty() && !self.process.is_empty()
    }
}

/// Every rooted store path with its retainers, largest first.
///
/// Sorted by size because the view exists to answer "what is taking up space
/// and who is to blame", and that ordering puts the answer on the first screen.
pub fn root_rows(roots: &BTreeMap<PathBuf, RootedPath>) -> Vec<RootRow> {
    let mut rows: Vec<RootRow> = roots
        .values()
        .map(|r| RootRow {
            store_path: r.store_path.clone(),
            size_bytes: r.size_bytes,
            projects: r.retained_by.iter().cloned().collect(),
            external: r.durable_roots().cloned().collect(),
            process: r.process_roots().cloned().collect(),
        })
        .collect();
    // Ties broken by path so the ordering is stable across runs; an unstable
    // list is unusable to navigate with a cursor.
    rows.sort_by(|a, b| {
        b.size_bytes
            .cmp(&a.size_bytes)
            .then_with(|| a.store_path.cmp(&b.store_path))
    });
    rows
}

pub fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", b, UNITS[0])
    } else {
        format!("{:.1} {}", v, UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rooted(path: &str, size: u64, projects: &[&str]) -> (PathBuf, RootedPath) {
        let p = PathBuf::from(path);
        (
            p.clone(),
            RootedPath {
                store_path: p,
                retained_by: projects.iter().map(PathBuf::from).collect(),
                external_roots: BTreeSet::new(),
                size_bytes: size,
            },
        )
    }

    #[test]
    fn shared_paths_are_not_counted_as_savings() {
        // /store/a is also held by /proj/c, which we are not migrating, so
        // dropping b's root frees nothing. Counting it would overstate savings.
        let roots: BTreeMap<_, _> = [rooted("/store/a", 500, &["/proj/b", "/proj/c"])]
            .into_iter()
            .collect();
        let migrating: BTreeSet<PathBuf> = ["/proj/b"].iter().map(PathBuf::from).collect();

        assert_eq!(reclaimable(&roots, &migrating), (0, 0));
    }

    #[test]
    fn fully_covered_paths_are_counted() {
        let roots: BTreeMap<_, _> = [rooted("/store/a", 500, &["/proj/b", "/proj/c"])]
            .into_iter()
            .collect();
        let migrating: BTreeSet<PathBuf> =
            ["/proj/b", "/proj/c"].iter().map(PathBuf::from).collect();

        assert_eq!(reclaimable(&roots, &migrating), (500, 1));
    }

    #[test]
    fn unrooted_paths_are_ignored() {
        let roots: BTreeMap<_, _> = [rooted("/store/a", 500, &[])].into_iter().collect();
        let migrating: BTreeSet<PathBuf> = ["/proj/b"].iter().map(PathBuf::from).collect();

        assert_eq!(reclaimable(&roots, &migrating), (0, 0));
    }

    #[test]
    fn externally_pinned_paths_are_not_reclaimable() {
        // A `result` symlink or running process holds this path too, so
        // dropping the direnv root frees nothing.
        let (p, mut r) = rooted("/store/a", 500, &["/proj/b"]);
        r.external_roots.insert("/Users/k/projects/x/result".into());
        let roots: BTreeMap<_, _> = [(p, r)].into_iter().collect();
        let migrating: BTreeSet<PathBuf> = ["/proj/b"].iter().map(PathBuf::from).collect();

        assert_eq!(reclaimable(&roots, &migrating), (0, 0));
    }

    #[test]
    fn root_lines_are_parsed() {
        let (root, target) = parse_root_line(
            r#""/Users/k/dev/x/.direnv/flake-inputs/abc-source" -> /nix/store/abc-source"#,
        )
        .unwrap();
        assert_eq!(target, PathBuf::from("/nix/store/abc-source"));
        assert_eq!(direnv_project(&root), Some(PathBuf::from("/Users/k/dev/x")));
    }

    #[test]
    fn censored_process_roots_are_external_not_direnv() {
        // `{lsof}` names no path, but the target is still alive.
        let (root, target) =
            parse_root_line(r#""{lsof}" -> /nix/store/xyz-nodejs"#).unwrap();
        assert_eq!(target, PathBuf::from("/nix/store/xyz-nodejs"));
        assert_eq!(direnv_project(&root), None);
    }

    #[test]
    fn result_symlinks_are_not_attributed_to_a_project() {
        // Sits inside a project dir, but is not a direnv flake input.
        let (root, _) =
            parse_root_line(r#""/Users/k/projects/wedding/result" -> /nix/store/w-web"#).unwrap();
        assert_eq!(direnv_project(&root), None);
    }

    #[test]
    fn rooted_projects_lists_direnv_retainers() {
        let roots: BTreeMap<_, _> = [
            rooted("/store/a", 1, &["/proj/b"]),
            rooted("/store/c", 1, &["/proj/d", "/proj/b"]),
        ]
        .into_iter()
        .collect();
        let got = rooted_projects(&roots);
        assert!(got.contains(&PathBuf::from("/proj/b")));
        assert!(got.contains(&PathBuf::from("/proj/d")));
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn root_rows_are_ordered_by_size() {
        let roots: BTreeMap<_, _> = [
            rooted("/store/small", 10, &["/proj/a"]),
            rooted("/store/big", 9000, &["/proj/b"]),
            rooted("/store/mid", 500, &["/proj/c"]),
        ]
        .into_iter()
        .collect();

        let paths: Vec<_> = root_rows(&roots)
            .into_iter()
            .map(|r| r.store_path)
            .collect();
        assert_eq!(
            paths,
            [
                PathBuf::from("/store/big"),
                PathBuf::from("/store/mid"),
                PathBuf::from("/store/small"),
            ]
        );
    }

    #[test]
    fn root_rows_separate_direnv_from_external_retainers() {
        let (p, mut r) = rooted("/store/a", 100, &["/proj/b"]);
        r.external_roots.insert("{lsof}".into());
        r.external_roots.insert("/run/current-system".into());
        let roots: BTreeMap<_, _> = [(p, r)].into_iter().collect();

        let rows = root_rows(&roots);
        assert_eq!(rows[0].projects, [PathBuf::from("/proj/b")]);
        // The durable pin and the transient one are kept apart: only the former
        // is a lasting reason the path cannot go.
        assert_eq!(rows[0].external, ["/run/current-system"]);
        assert_eq!(rows[0].process, ["{lsof}"]);
        assert_eq!(rows[0].retainer_count(), 3);
        // Something outside direnv holds it, so lockfile edits cannot free it.
        assert!(!rows[0].freeable());
    }

    #[test]
    fn a_path_held_only_by_a_process_is_blocked_but_not_durably() {
        let (p, mut r) = rooted("/store/a", 100, &["/proj/b"]);
        r.external_roots.insert("{lsof}".into());
        let roots: BTreeMap<_, _> = [(p, r)].into_iter().collect();

        let rows = root_rows(&roots);
        assert!(!rows[0].freeable(), "a live process still pins it");
        assert!(rows[0].blocked_only_by_process());
    }

    #[test]
    fn temp_and_censored_roots_count_as_process_roots() {
        // The daemon censors these differently but they are all transient.
        assert!(is_process_root("{lsof}"));
        assert!(is_process_root("{temp:89776}"));
        assert!(is_process_root("{censored}"));
        // Real paths are not.
        assert!(!is_process_root("/run/current-system"));
        assert!(!is_process_root("/Users/k/projects/x/result"));
    }

    #[test]
    fn a_durably_pinned_path_is_not_merely_process_blocked() {
        let (p, mut r) = rooted("/store/a", 100, &["/proj/b"]);
        r.external_roots.insert("/run/current-system".into());
        r.external_roots.insert("{lsof}".into());
        let roots: BTreeMap<_, _> = [(p, r)].into_iter().collect();

        // The profile outlives the process, so hiding process roots must not
        // make this look reclaimable.
        assert!(!root_rows(&roots)[0].blocked_only_by_process());
    }

    #[test]
    fn a_purely_direnv_path_is_freeable() {
        let roots: BTreeMap<_, _> =
            [rooted("/store/a", 100, &["/proj/b", "/proj/c"])].into_iter().collect();
        assert!(root_rows(&roots)[0].freeable());
    }

    #[test]
    fn an_unrooted_path_is_not_freeable() {
        // Nothing holds it, so there is no root to drop and nothing to free.
        let roots: BTreeMap<_, _> = [rooted("/store/a", 100, &[])].into_iter().collect();
        assert!(!root_rows(&roots)[0].freeable());
    }

    #[test]
    fn human_bytes_scales() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MiB");
    }
}
