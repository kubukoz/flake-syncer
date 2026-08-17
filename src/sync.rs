//! Applying a chosen version to the projects that diverge from it.
//!
//! Lockfiles are never edited by hand. Rewriting `rev` in place would leave the
//! `narHash` and `lastModified` inconsistent with it, producing a lockfile Nix
//! rejects or, worse, silently mistrusts. Instead we drive
//! `nix flake update --override-input`, which re-resolves the input properly.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::lock::{Group, Identity, Pin};

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
    /// The declared ref this input is being moved *off*, when merging.
    ///
    /// `Some` means the project's `flake.nix` currently declares a different
    /// flakeref than `identity`, and must be rewritten before the lockfile can
    /// be resolved against the merged ref. `None` is a plain revision sync,
    /// which never touches `flake.nix`.
    pub from_identity: Option<Identity>,
}

impl Action {
    /// Whether this action rewrites `flake.nix` as well as the lockfile.
    pub fn changes_ref(&self) -> bool {
        self.from_identity.is_some()
    }

    /// The url this action would write into `flake.nix`, if any.
    pub fn target_url(&self) -> Option<String> {
        self.changes_ref().then(|| self.identity.url())
    }
}

/// The action bringing one pin to `target_version`.
///
/// Callers decide which pins are eligible: only direct inputs can be reached by
/// `--override-input`, and only rooted projects free anything by moving.
pub fn action_for(group: &Group, pin: &Pin, target_version: &str) -> Action {
    Action {
        project: pin.project.clone(),
        input_name: pin.declared_name().to_string(),
        identity: group.identity.clone(),
        from_version: pin.version.clone(),
        target_version: target_version.to_string(),
        from_identity: None,
    }
}

/// The action moving one pin onto a merged identity's ref *and* revision.
pub fn merge_action_for(
    canonical: &Identity,
    from: &Identity,
    pin: &Pin,
    target_version: &str,
) -> Action {
    Action {
        project: pin.project.clone(),
        input_name: pin.declared_name().to_string(),
        identity: canonical.clone(),
        from_version: pin.version.clone(),
        target_version: target_version.to_string(),
        from_identity: Some(from.clone()),
    }
}

/// The flakeref pinning `identity` at an exact revision.
fn pinned_flakeref(identity: &Identity, version: &str) -> String {
    if identity.owner.is_empty() {
        format!("{}?rev={}", identity.repo, version)
    } else {
        format!("{}:{}/{}/{}", identity.kind, identity.owner, identity.repo, version)
    }
}

/// The steps an action would perform, for display in the TUI's dry-run view.
///
/// A merge is two steps, and showing only the second would understate what is
/// about to happen: the `flake.nix` rewrite is the part that edits a file the
/// user maintains by hand.
pub fn command_lines(action: &Action) -> Vec<String> {
    let mut steps = Vec::new();
    if let Some(url) = action.target_url() {
        steps.push(format!(
            "edit {}: inputs.{}.url = \"{}\"",
            action.project.join("flake.nix").display(),
            action.input_name,
            url
        ));
    }
    steps.push(format!(
        "nix flake update {} --override-input {} {} (in {})",
        action.input_name,
        action.input_name,
        pinned_flakeref(&action.identity, &action.target_version),
        action.project.display()
    ));
    steps
}

/// Execute one action.
///
/// Actions only ever name direct inputs — transitive ones are filtered out
/// before planning, since `--override-input` cannot reach them. A failure here
/// is a genuine Nix error and is surfaced verbatim.
///
/// When the action changes the declared ref, `edit` must already have been
/// applied by the caller: the lockfile is resolved against whatever `flake.nix`
/// says, so the file has to be correct before this runs.
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
