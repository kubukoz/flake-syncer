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
    /// True when something outside direnv also holds this path: a profile, a
    /// `result` symlink, a running process. Such a path cannot be reclaimed by
    /// editing lockfiles, whatever we do to the direnv roots.
    pub pinned_elsewhere: bool,
    pub size_bytes: u64,
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

/// Ask the Nix daemon for the authoritative GC root set.
///
/// This must come from `nix-store --gc --print-roots` rather than a walk of
/// `/nix/var/nix/gcroots/auto`: profiles, `result` symlinks and running
/// processes also root store paths, and a scan blind to them reports paths as
/// reclaimable when something else is still holding them.
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
    let mut external: BTreeSet<PathBuf> = BTreeSet::new();

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
                external.insert(real);
            }
        }
        links.advance(1);
    }
    links.set(retainers.len());
    links.finish();

    // Sizing dominates the scan (it walks every file of every rooted source),
    // so each unique store path is measured once, in parallel. This is also the
    // one phase slow enough to need an ETA, and the count is known up front, so
    // the estimate is a real one.
    let sizing = Progress::new("sizing paths", Some(retainers.len()));
    let sizes: Vec<(PathBuf, u64)> = retainers
        .keys()
        .cloned()
        .collect::<Vec<_>>()
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
            let pinned_elsewhere = external.contains(&path);
            (
                path.clone(),
                RootedPath { store_path: path, retained_by, pinned_elsewhere, size_bytes },
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
        if rooted.pinned_elsewhere {
            continue;
        }
        if !rooted.retained_by.is_empty() && rooted.retained_by.is_subset(projects) {
            bytes += rooted.size_bytes;
            count += 1;
        }
    }
    (bytes, count)
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
                pinned_elsewhere: false,
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
        r.pinned_elsewhere = true;
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
    fn human_bytes_scales() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MiB");
    }
}
