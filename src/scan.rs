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

    /// Command that renders a unified diff, receiving it on stdin.
    ///
    /// Every `flake.nix` edit is shown as a diff before and after it is applied,
    /// so nothing changes on disk that the user has not seen. `delta` is the
    /// default; anything reading a diff from stdin works, and an empty list
    /// falls back to plain built-in output.
    #[serde(default = "default_differ")]
    pub differ: Vec<String>,

    /// Command used to open a file for manual editing, with the path appended.
    ///
    /// Used for the flakes this tool refuses to rewrite automatically: rather
    /// than guess at an unfamiliar `inputs` layout, it hands the file over.
    #[serde(default = "default_editor")]
    pub editor: Vec<String>,
}

fn default_depth() -> usize {
    4
}

/// `delta`, configured for capture rather than for a terminal it owns.
///
/// `--paging=never` stops it blocking on a pager, and `--color-only` keeps it
/// from drawing file headers and box-drawing rules that assume a full-width
/// terminal — the TUI supplies its own framing and re-colours each line from its
/// diff marker, so what is wanted here is the diff text, not delta's chrome.
fn default_differ() -> Vec<String> {
    vec![
        "delta".into(),
        "--paging=never".into(),
        "--color-only".into(),
    ]
}

fn default_editor() -> Vec<String> {
    vec!["nvim".into()]
}

impl Config {
    /// The editor to use, preferring `$VISUAL`/`$EDITOR` when the config still
    /// holds the default — an explicitly configured editor always wins.
    pub fn editor_command(&self) -> Vec<String> {
        if self.editor != default_editor() {
            return self.editor.clone();
        }
        std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.split_whitespace().map(String::from).collect())
            .unwrap_or_else(default_editor)
    }
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
        Config {
            roots,
            max_depth: default_depth(),
            ignore: Vec::new(),
            differ: default_differ(),
            editor: default_editor(),
        }
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

    #[test]
    fn an_explicit_editor_beats_the_environment() {
        // Someone who wrote an editor into the config meant it, whatever their
        // shell exports.
        let cfg = Config { editor: vec!["emacs".into()], ..Config::default() };
        assert_eq!(cfg.editor_command(), vec!["emacs".to_string()]);
    }

    #[test]
    fn a_multi_word_env_editor_is_split_into_arguments() {
        // `EDITOR="code --wait"` is common, and passing it as one program name
        // would fail to launch.
        let cfg = Config::default();
        // SAFETY: single-threaded test; no other thread reads the environment.
        unsafe { std::env::set_var("VISUAL", "code --wait") };
        assert_eq!(
            cfg.editor_command(),
            vec!["code".to_string(), "--wait".to_string()]
        );
        unsafe { std::env::remove_var("VISUAL") };
    }

    #[test]
    fn the_default_differ_never_blocks_on_a_pager() {
        // Output is captured rather than handed a terminal, so a pager would
        // hang the TUI rather than show anything.
        assert!(default_differ().contains(&"--paging=never".to_string()));
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
