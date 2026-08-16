//! Applying a chosen version to the projects that diverge from it.
//!
//! Lockfiles are never edited by hand. Rewriting `rev` in place would leave the
//! `narHash` and `lastModified` inconsistent with it, producing a lockfile Nix
//! rejects or, worse, silently mistrusts. Instead we drive
//! `nix flake update --override-input`, which re-resolves the input properly.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::lock::{Group, Identity};

/// A single project-level edit: point `input_name` at `target_version`.
#[derive(Debug, Clone)]
pub struct Action {
    pub project: PathBuf,
    pub input_name: String,
    pub identity: Identity,
    /// Retained for diagnostics and future undo support.
    #[allow(dead_code)]
    pub from_version: String,
    pub target_version: String,
}

/// Build the actions needed to bring every pin in `group` to `target_version`.
pub fn plan(group: &Group, target_version: &str) -> Vec<Action> {
    group
        .pins
        .iter()
        .filter(|p| p.version != target_version)
        .map(|p| Action {
            project: p.project.clone(),
            input_name: p.input_name.clone(),
            identity: group.identity.clone(),
            from_version: p.version.clone(),
            target_version: target_version.to_string(),
        })
        .collect()
}

/// The flakeref pinning `identity` at an exact revision.
fn pinned_flakeref(identity: &Identity, version: &str) -> String {
    if identity.owner.is_empty() {
        format!("{}?rev={}", identity.repo, version)
    } else {
        format!("{}:{}/{}/{}", identity.kind, identity.owner, identity.repo, version)
    }
}

/// The command an action would run, for display in the TUI's dry-run view.
pub fn command_line(action: &Action) -> String {
    format!(
        "nix flake update {} --override-input {} {} (in {})",
        action.input_name,
        action.input_name,
        pinned_flakeref(&action.identity, &action.target_version),
        action.project.display()
    )
}

/// Execute one action.
///
/// Only inputs named directly in the lockfile's root set can be overridden this
/// way; transitive inputs (`nixpkgs_2` and friends) belong to a dependency's own
/// lockfile and are reported as skipped rather than silently ignored.
pub fn apply(action: &Action) -> Result<()> {
    let flake_nix = action.project.join("flake.nix");
    if !flake_nix.exists() {
        bail!("no flake.nix in {}", action.project.display());
    }

    let output = Command::new("nix")
        .arg("flake")
        .arg("update")
        .arg(&action.input_name)
        .arg("--override-input")
        .arg(&action.input_name)
        .arg(pinned_flakeref(&action.identity, &action.target_version))
        .current_dir(&action.project)
        .output()
        .with_context(|| format!("running nix flake update in {}", action.project.display()))?;

    if !output.status.success() {
        bail!(
            "nix flake update failed in {}: {}",
            action.project.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Remove a project's stale direnv flake-input roots.
///
/// This is what actually frees disk space. It only unlinks the GC root symlink;
/// the store path itself is removed by the next `nix store gc`. We never touch
/// `/nix/store` directly.
pub fn drop_direnv_roots(project: &Path) -> Result<usize> {
    let dir = project.join(".direnv").join("flake-inputs");
    if !dir.exists() {
        return Ok(0);
    }
    let mut removed = 0;
    for entry in std::fs::read_dir(&dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok())
    {
        if std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}
