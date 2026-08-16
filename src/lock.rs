//! Parsing of `flake.lock` files and grouping of inputs into identities.
//!
//! A lockfile is a graph of nodes. Each non-root node carries an `original`
//! (the flakeref as written, e.g. `github:nixos/nixpkgs/nixpkgs-unstable`) and
//! a `locked` (what it resolved to, including `rev`). Deduplication works on
//! the `original`: two flakes "declare the same input" when their `original`
//! matches, and they diverge when the corresponding `locked.rev` differs.

use crate::progress::Progress;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
pub struct LockFile {
    #[serde(default)]
    pub nodes: BTreeMap<String, Node>,
    #[serde(default)]
    pub root: String,
}

#[derive(Debug, Deserialize)]
pub struct Node {
    pub original: Option<Ref>,
    pub locked: Option<Locked>,
    /// Edges to other nodes. On the root node this names the flake's own
    /// direct inputs; values are either a node name or a follows-path.
    #[serde(default)]
    pub inputs: BTreeMap<String, InputEdge>,
}

/// An entry in a node's `inputs` map: either `"node"` or `["parent", "child"]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum InputEdge {
    Node(String),
    Follows(Vec<String>),
}

impl InputEdge {
    /// The node this edge resolves to, for the direct-input check.
    fn node_name(&self) -> Option<&str> {
        match self {
            InputEdge::Node(n) => Some(n.as_str()),
            InputEdge::Follows(path) => path.first().map(|s| s.as_str()),
        }
    }
}

/// The flakeref as written by the user, before resolution.
#[derive(Debug, Clone, Deserialize)]
pub struct Ref {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub owner: Option<String>,
    pub repo: Option<String>,
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
    pub url: Option<String>,
    /// Present on `path` inputs; parsed so they can be recognized and skipped.
    #[allow(dead_code)]
    pub path: Option<String>,
    /// Present on `indirect` (registry) inputs; likewise recognized and skipped.
    #[allow(dead_code)]
    pub id: Option<String>,
}

/// The resolved pin.
#[derive(Debug, Clone, Deserialize)]
pub struct Locked {
    pub rev: Option<String>,
    #[serde(rename = "narHash")]
    pub nar_hash: Option<String>,
    #[serde(rename = "lastModified")]
    pub last_modified: Option<i64>,
}

impl Locked {
    /// Stable identifier for "which version is this". Prefer `rev`; fall back to
    /// `narHash` for inputs that have no revision (tarballs, paths).
    pub fn version_id(&self) -> Option<&str> {
        self.rev.as_deref().or(self.nar_hash.as_deref())
    }
}

/// A normalized input identity: the thing we group divergent pins under.
///
/// GitHub owner/repo names are case-insensitive upstream, so `NixOS/nixpkgs`
/// and `nixos/nixpkgs` fold together here. Distinct `ref`s (`nixpkgs-unstable`
/// vs `nixos-unstable`) stay separate: they are genuinely different branches.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Identity {
    pub kind: String,
    pub owner: String,
    pub repo: String,
    pub git_ref: String,
}

impl Identity {
    fn from_ref(r: &Ref) -> Option<Self> {
        let kind = r.kind.clone().unwrap_or_default();

        // `path` and `indirect` inputs are local or registry-relative. They are
        // not shared artifacts, so deduplicating them across projects is
        // meaningless and would produce bogus groups.
        if kind == "path" || kind == "indirect" || kind.is_empty() {
            return None;
        }

        let (owner, repo) = match (r.owner.as_deref(), r.repo.as_deref()) {
            (Some(o), Some(p)) => (o.to_ascii_lowercase(), p.to_ascii_lowercase()),
            // Non-github forges identify by URL instead of owner/repo.
            _ => match r.url.as_deref() {
                Some(u) => (String::new(), u.trim_end_matches(".git").to_string()),
                None => return None,
            },
        };

        Some(Identity {
            kind,
            owner,
            repo,
            git_ref: r.git_ref.clone().unwrap_or_default(),
        })
    }
}

impl fmt::Display for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.owner.is_empty() {
            write!(f, "{}", self.repo)?;
        } else {
            write!(f, "{}/{}", self.owner, self.repo)?;
        }
        if !self.git_ref.is_empty() {
            write!(f, "@{}", self.git_ref)?;
        }
        Ok(())
    }
}

/// One project's pin of one identity.
#[derive(Debug, Clone)]
pub struct Pin {
    /// Path to the project directory (the dir containing `flake.lock`).
    pub project: PathBuf,
    /// The input's attribute name within that project's lockfile.
    pub input_name: String,
    pub version: String,
    pub last_modified: Option<i64>,
    /// Whether the project's own `flake.nix` declares this input.
    ///
    /// Only direct inputs can be retargeted with `--override-input`; transitive
    /// ones belong to a dependency's lockfile, so an action against them would
    /// fail. They are recorded but excluded from the default view and the plan.
    pub direct: bool,
}

/// All pins of a single identity, across every scanned project.
#[derive(Debug, Clone)]
pub struct Group {
    pub identity: Identity,
    pub pins: Vec<Pin>,
}

impl Group {
    /// Transitive pins, shown for context but never planned against: they live
    /// in a dependency's lockfile, where `--override-input` cannot reach them.
    pub fn transitive_count(&self) -> usize {
        self.pins.iter().filter(|p| !p.direct).count()
    }
}

pub fn parse(path: &Path) -> Result<LockFile> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Build identity groups from a set of lockfile paths.
///
/// Each lockfile contributes at most one pin per identity. A lockfile can
/// contain the same identity twice (e.g. `nixpkgs` and `nixpkgs_2`, from
/// transitive inputs that were not followed); both are recorded, since each is
/// a separately-materialized store path.
pub fn group(lock_paths: &[PathBuf]) -> (Vec<Group>, Vec<(PathBuf, anyhow::Error)>) {
    let mut map: BTreeMap<Identity, Vec<Pin>> = BTreeMap::new();
    let mut errors = Vec::new();
    let progress = Progress::new("parsing", Some(lock_paths.len()));

    for lock_path in lock_paths {
        progress.advance(1);
        let lock = match parse(lock_path) {
            Ok(l) => l,
            Err(e) => {
                errors.push((lock_path.clone(), e));
                continue;
            }
        };
        let project = lock_path.parent().unwrap_or(lock_path).to_path_buf();

        // The root node's inputs are exactly the flake's own declared inputs;
        // every other node reached the lockfile through a dependency.
        let direct_nodes: BTreeSet<&str> = lock
            .nodes
            .get(&lock.root)
            .map(|r| r.inputs.values().filter_map(|e| e.node_name()).collect())
            .unwrap_or_default();

        for (name, node) in &lock.nodes {
            if *name == lock.root {
                continue;
            }
            let (Some(orig), Some(locked)) = (&node.original, &node.locked) else {
                continue;
            };
            let (Some(identity), Some(version)) = (Identity::from_ref(orig), locked.version_id())
            else {
                continue;
            };

            map.entry(identity).or_default().push(Pin {
                project: project.clone(),
                input_name: name.clone(),
                version: version.to_string(),
                last_modified: locked.last_modified,
                direct: direct_nodes.contains(name.as_str()),
            });
        }
    }

    progress.finish();

    let groups = map
        .into_iter()
        .map(|(identity, pins)| Group { identity, pins })
        .collect();

    (groups, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gh(owner: &str, repo: &str, git_ref: Option<&str>) -> Ref {
        Ref {
            kind: Some("github".into()),
            owner: Some(owner.into()),
            repo: Some(repo.into()),
            git_ref: git_ref.map(String::from),
            url: None,
            path: None,
            id: None,
        }
    }

    #[test]
    fn github_owner_case_is_normalized() {
        // Real lockfiles in the wild spell this both ways for the same repo.
        let a = Identity::from_ref(&gh("NixOS", "nixpkgs", Some("nixpkgs-unstable"))).unwrap();
        let b = Identity::from_ref(&gh("nixos", "nixpkgs", Some("nixpkgs-unstable"))).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn distinct_refs_stay_separate() {
        // nixpkgs-unstable and nixos-unstable are genuinely different branches.
        let a = Identity::from_ref(&gh("nixos", "nixpkgs", Some("nixpkgs-unstable"))).unwrap();
        let b = Identity::from_ref(&gh("nixos", "nixpkgs", Some("nixos-unstable"))).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn local_and_registry_inputs_are_not_grouped() {
        for kind in ["path", "indirect"] {
            let r = Ref {
                kind: Some(kind.into()),
                owner: None,
                repo: None,
                git_ref: None,
                url: None,
                path: Some("/tmp/x".into()),
                id: Some("nixpkgs".into()),
            };
            assert!(Identity::from_ref(&r).is_none(), "{kind} should be skipped");
        }
    }

    #[test]
    fn version_falls_back_to_nar_hash() {
        let l = Locked { rev: None, nar_hash: Some("sha256-abc".into()), last_modified: None };
        assert_eq!(l.version_id(), Some("sha256-abc"));
    }

    #[test]
    fn transitive_pins_are_counted_separately() {
        let g = Group {
            identity: Identity::from_ref(&gh("nixos", "nixpkgs", None)).unwrap(),
            pins: vec![
                Pin { project: "/a".into(), input_name: "nixpkgs".into(), version: "same".into(), last_modified: Some(1), direct: true },
                Pin { project: "/b".into(), input_name: "nixpkgs_2".into(), version: "other".into(), last_modified: Some(2), direct: false },
            ],
        };
        assert_eq!(g.transitive_count(), 1);
    }

    #[test]
    fn direct_inputs_are_read_from_the_root_node() {
        // `follows` entries are arrays; plain deps are strings. Both are direct.
        let json = r#"{
          "root": "root",
          "nodes": {
            "root": { "inputs": { "nixpkgs": "nixpkgs", "utils": ["flake-utils"] } },
            "nixpkgs": {
              "original": { "type": "github", "owner": "nixos", "repo": "nixpkgs" },
              "locked": { "rev": "aaa", "lastModified": 1 }
            },
            "flake-utils": {
              "original": { "type": "github", "owner": "numtide", "repo": "flake-utils" },
              "locked": { "rev": "bbb", "lastModified": 2 }
            },
            "systems": {
              "original": { "type": "github", "owner": "nix-systems", "repo": "default" },
              "locked": { "rev": "ccc", "lastModified": 3 }
            }
          }
        }"#;
        let lock: LockFile = serde_json::from_str(json).unwrap();
        let root = lock.nodes.get(&lock.root).unwrap();
        let direct: BTreeSet<&str> = root
            .inputs
            .values()
            .filter_map(|e| e.node_name())
            .collect();
        assert!(direct.contains("nixpkgs"));
        assert!(direct.contains("flake-utils"));
        // Pulled in by flake-utils, not declared by this flake.
        assert!(!direct.contains("systems"));
    }

}
