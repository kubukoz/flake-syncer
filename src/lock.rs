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
use std::collections::BTreeMap;
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

/// What must match for two identities to be *mergeable*: the same upstream
/// repository, ignoring which ref each project tracks.
///
/// Grouping deliberately keeps `nixpkgs-unstable` and `nixos-unstable` apart,
/// because converging them silently would change which branch a project
/// follows. Merging is the user saying "these are the same thing to me, move
/// them onto one ref" — so it needs a coarser key than [`Identity`], and it
/// stays opt-in.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MergeKey {
    pub kind: String,
    pub owner: String,
    pub repo: String,
}

impl Identity {
    pub fn merge_key(&self) -> MergeKey {
        MergeKey {
            kind: self.kind.clone(),
            owner: self.owner.clone(),
            repo: self.repo.clone(),
        }
    }

    /// The flakeref a project would declare to track this identity's ref.
    ///
    /// This is what gets written into `flake.nix` when merging: a branch-level
    /// url, not a pinned revision. The revision is the lockfile's business, and
    /// baking one into `flake.nix` would freeze the input permanently.
    pub fn url(&self) -> String {
        let base = if self.owner.is_empty() {
            self.repo.clone()
        } else {
            format!("{}:{}/{}", self.kind, self.owner, self.repo)
        };
        if self.git_ref.is_empty() {
            base
        } else if self.owner.is_empty() {
            // Non-github forges carry the ref as a query parameter.
            let sep = if base.contains('?') { '&' } else { '?' };
            format!("{base}{sep}ref={}", self.git_ref)
        } else {
            format!("{base}/{}", self.git_ref)
        }
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
    /// The input's node name within that project's lockfile.
    ///
    /// Not necessarily what the flake calls it: when two nodes would collide,
    /// Nix suffixes them (`nixpkgs_2`), so a flake declaring `nixpkgs` can have
    /// its own input stored under `nixpkgs_2`. See [`Pin::declared_name`].
    pub input_name: String,
    /// The attribute name the project's own `flake.nix` declares, when this is
    /// a direct input.
    ///
    /// This is the name to use against `flake.nix` and `--override-input`;
    /// `input_name` is a lockfile-internal identifier and may differ.
    pub declared_name: Option<String>,
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

impl Pin {
    /// The name to use when addressing this input from outside the lockfile:
    /// what `flake.nix` calls it, falling back to the node name.
    pub fn declared_name(&self) -> &str {
        self.declared_name.as_deref().unwrap_or(&self.input_name)
    }
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
        // every other node reached the lockfile through a dependency. The map
        // is attribute-name → node-name, and the two differ whenever Nix had to
        // disambiguate a collision (`nixpkgs` declared, `nixpkgs_2` stored), so
        // it is inverted here to recover what `flake.nix` actually calls each.
        let declared: BTreeMap<&str, &str> = lock
            .nodes
            .get(&lock.root)
            .map(|r| {
                r.inputs
                    .iter()
                    .filter_map(|(attr, e)| e.node_name().map(|n| (n, attr.as_str())))
                    .collect()
            })
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

            let declared_name = declared.get(name.as_str()).map(|s| s.to_string());
            map.entry(identity).or_default().push(Pin {
                project: project.clone(),
                input_name: name.clone(),
                version: version.to_string(),
                last_modified: locked.last_modified,
                direct: declared_name.is_some(),
                declared_name,
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
                Pin { project: "/a".into(), input_name: "nixpkgs".into(), version: "same".into(), last_modified: Some(1), direct: true , declared_name: Some("nixpkgs".into()) },
                Pin { project: "/b".into(), input_name: "nixpkgs_2".into(), version: "other".into(), last_modified: Some(2), direct: false , declared_name: None },
            ],
        };
        assert_eq!(g.transitive_count(), 1);
    }

    #[test]
    fn a_renamed_node_reports_the_name_the_flake_declares() {
        // Nix suffixes colliding node names, so a flake that declares `nixpkgs`
        // can have its own direct input stored as `nixpkgs_2`. Addressing
        // flake.nix or --override-input by the node name would miss it, and the
        // node name is what the lockfile hands us.
        let json = r#"{
          "root": "root",
          "nodes": {
            "root": { "inputs": { "nixpkgs": "nixpkgs_2", "deploy-rs": "deploy-rs" } },
            "nixpkgs": {
              "original": { "type": "github", "owner": "nixos", "repo": "nixpkgs" },
              "locked": { "rev": "aaa", "lastModified": 1 }
            },
            "nixpkgs_2": {
              "original": { "type": "github", "owner": "nixos", "repo": "nixpkgs", "ref": "nixos-unstable" },
              "locked": { "rev": "bbb", "lastModified": 2 }
            },
            "deploy-rs": {
              "original": { "type": "github", "owner": "serokell", "repo": "deploy-rs" },
              "locked": { "rev": "ccc", "lastModified": 3 }
            }
          }
        }"#;
        let dir = std::env::temp_dir().join("flake-syncer-renamed-node-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("flake.lock"), json).unwrap();

        let (groups, errors) = group(&[dir.join("flake.lock")]);
        assert!(errors.is_empty());

        let renamed = groups
            .iter()
            .flat_map(|g| &g.pins)
            .find(|p| p.input_name == "nixpkgs_2")
            .expect("nixpkgs_2 should be recorded");
        assert!(renamed.direct, "it is the flake's own input, despite the suffix");
        assert_eq!(
            renamed.declared_name(),
            "nixpkgs",
            "flake.nix and --override-input must be addressed by the declared name"
        );

        // The unreferenced `nixpkgs` node came in through a dependency.
        let transitive = groups
            .iter()
            .flat_map(|g| &g.pins)
            .find(|p| p.input_name == "nixpkgs")
            .expect("nixpkgs should be recorded");
        assert!(!transitive.direct);
        assert_eq!(transitive.declared_name(), "nixpkgs");

        std::fs::remove_dir_all(&dir).ok();
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
        let direct: std::collections::BTreeSet<&str> = root
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
