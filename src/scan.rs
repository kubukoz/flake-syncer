//! Discovery of flake projects on disk, and the config that drives it.

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
}

fn default_depth() -> usize {
    4
}

impl Default for Config {
    fn default() -> Self {
        let home = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf());
        let roots = home
            .map(|h| vec![h.join("dev"), h.join("projects"), h.join(".nixpkgs")])
            .unwrap_or_default();
        Config { roots, max_depth: default_depth() }
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

/// Find `flake.lock` files under the configured roots.
///
/// `.git` and `.direnv` are skipped: the latter contains materialized copies of
/// flake inputs, whose own lockfiles are not projects the user maintains and
/// would badly skew the divergence counts.
pub fn find_lockfiles(cfg: &Config) -> Vec<PathBuf> {
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
                out.push(entry.into_path());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}
