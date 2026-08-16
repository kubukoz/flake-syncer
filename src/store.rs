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

const AUTO_ROOTS: &str = "/nix/var/nix/gcroots/auto";

/// A store path retained by direnv, and the projects whose roots retain it.
#[derive(Debug, Clone)]
pub struct RootedPath {
    /// Retained for diagnostics; the map key carries the same value.
    #[allow(dead_code)]
    pub store_path: PathBuf,
    pub retained_by: BTreeSet<PathBuf>,
    pub size_bytes: u64,
}

/// Scan `/nix/var/nix/gcroots/auto` for direnv-managed flake input roots.
///
/// Dangling links are skipped: Nix reports those as stale and they hold nothing.
pub fn scan_roots() -> BTreeMap<PathBuf, RootedPath> {
    use rayon::prelude::*;

    let mut retainers: BTreeMap<PathBuf, BTreeSet<PathBuf>> = BTreeMap::new();

    let Ok(entries) = std::fs::read_dir(AUTO_ROOTS) else {
        return BTreeMap::new();
    };

    let links = Progress::new("gc roots", None);
    for entry in entries.filter_map(|e| e.ok()) {
        let Ok(target) = std::fs::read_link(entry.path()) else {
            continue;
        };
        let target_str = target.to_string_lossy().to_string();

        // Only direnv's per-project flake input roots are attributable to a
        // project. Profile links and manual roots are left alone.
        let Some((project, _)) = target_str.split_once("/.direnv/") else {
            continue;
        };
        if !target_str.contains("/flake-inputs/") {
            continue;
        }
        // A stale link points at a path that no longer exists.
        let Ok(real) = std::fs::canonicalize(&target) else {
            continue;
        };

        retainers
            .entry(real)
            .or_default()
            .insert(PathBuf::from(project));
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
            (
                path.clone(),
                RootedPath { store_path: path, retained_by, size_bytes },
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

/// How many bytes would actually be freed by migrating `projects` off the
/// versions they currently pin.
///
/// A rooted path counts only when every project retaining it is in `projects`;
/// otherwise some other project keeps it alive and nothing is freed.
pub fn reclaimable(
    roots: &BTreeMap<PathBuf, RootedPath>,
    projects: &BTreeSet<PathBuf>,
) -> (u64, usize) {
    let mut bytes = 0;
    let mut count = 0;
    for rooted in roots.values() {
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
    fn human_bytes_scales() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MiB");
    }
}
