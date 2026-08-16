//! Discovery of flake projects on disk, and the config that drives it.

use crate::progress::Progress;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use walkdir::WalkDir;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Directories to scan for `flake.lock` files.
    pub roots: Vec<PathBuf>,
    /// How deep below each root to descend.
    #[serde(default = "default_depth")]
    pub max_depth: usize,
    /// Paths to skip. An entry matches a project when it is equal to, or a
    /// parent of, the project directory — so listing a directory ignores
    /// everything beneath it.
    ///
    /// Relative entries are matched against the tail of the project path, which
    /// makes `talks/nix-scala-folks` work without spelling out `$HOME`.
    #[serde(default)]
    pub ignore: Vec<PathBuf>,
}

fn default_depth() -> usize {
    4
}

/// Whether `project` is excluded by any `ignore` entry.
pub fn is_ignored(project: &std::path::Path, ignore: &[PathBuf]) -> bool {
    ignore.iter().any(|pat| {
        if pat.is_absolute() {
            project.starts_with(pat)
        } else {
            // Match on a path-component boundary, so `talks` does not ignore
            // `talks-archive`, and the pattern may name any ancestor.
            project
                .ancestors()
                .any(|a| a.ends_with(pat))
        }
    })
}

impl Default for Config {
    fn default() -> Self {
        let home = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf());
        let roots = home
            .map(|h| vec![h.join("dev"), h.join("projects"), h.join(".nixpkgs")])
            .unwrap_or_default();
        Config { roots, max_depth: default_depth(), ignore: Vec::new() }
    }
}

pub fn config_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "flake-syncer")
        .map(|d| d.config_dir().join("config.toml"))
}

/// Load config, writing out the default on first run so it is discoverable.
pub fn load_config() -> Result<Config> {
    let Some(path) = config_path() else {
        return Ok(Config::default());
    };
    if path.exists() {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        return toml::from_str(&text).with_context(|| format!("parsing {}", path.display()));
    }

    let cfg = Config::default();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    if let Ok(text) = toml::to_string_pretty(&cfg) {
        std::fs::write(&path, text).ok();
    }
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pats(v: &[&str]) -> Vec<PathBuf> {
        v.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn absolute_ignore_covers_descendants() {
        let ig = pats(&["/home/k/projects/talks"]);
        assert!(is_ignored(std::path::Path::new("/home/k/projects/talks"), &ig));
        assert!(is_ignored(
            std::path::Path::new("/home/k/projects/talks/nix-scala-folks"),
            &ig
        ));
        assert!(!is_ignored(std::path::Path::new("/home/k/projects/other"), &ig));
    }

    #[test]
    fn relative_ignore_matches_the_path_tail() {
        let ig = pats(&["talks/nix-scala-folks"]);
        assert!(is_ignored(
            std::path::Path::new("/home/k/projects/talks/nix-scala-folks"),
            &ig
        ));
        assert!(!is_ignored(
            std::path::Path::new("/home/k/projects/talks/other"),
            &ig
        ));
    }

    #[test]
    fn ignore_respects_component_boundaries() {
        // A prefix match would wrongly ignore `talks-archive`.
        let ig = pats(&["talks"]);
        assert!(!is_ignored(
            std::path::Path::new("/home/k/projects/talks-archive"),
            &ig
        ));
        assert!(is_ignored(
            std::path::Path::new("/home/k/projects/talks/deep/nested"),
            &ig
        ));
    }

    #[test]
    fn empty_ignore_matches_nothing() {
        assert!(!is_ignored(std::path::Path::new("/anything"), &[]));
    }
}

/// Find `flake.lock` files under the configured roots.
///
/// `.git` and `.direnv` are skipped: the latter contains materialized copies of
/// flake inputs, whose own lockfiles are not projects the user maintains and
/// would badly skew the divergence counts.
///
/// The walk cannot know how many directories it will visit before visiting
/// them, so progress here is a running count of what has been found rather than
/// a percentage.
pub fn find_lockfiles(cfg: &Config) -> Vec<PathBuf> {
    let progress = Progress::new("discovering", None);
    let mut out = Vec::new();
    for root in &cfg.roots {
        if !root.exists() {
            continue;
        }
        let walker = WalkDir::new(root)
            .max_depth(cfg.max_depth)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                let name = e.file_name().to_string_lossy();
                !(name == ".git" || name == ".direnv" || name == "result")
            });

        for entry in walker.filter_map(|e| e.ok()) {
            if entry.file_type().is_file() && entry.file_name() == "flake.lock" {
                let path = entry.into_path();
                let project = path.parent().unwrap_or(&path);
                if is_ignored(project, &cfg.ignore) {
                    continue;
                }
                out.push(path);
                progress.advance(1);
            }
        }
    }
    out.sort();
    out.dedup();
    progress.set(out.len());
    progress.finish();
    out
}
