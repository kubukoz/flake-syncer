//! Rewriting the `url` of one input in a `flake.nix`.
//!
//! Merging two identities that differ only by `ref` means changing what a
//! project *declares*, not just what it resolved to — so unlike revision
//! syncing, this cannot be done with `--override-input` alone. The declaration
//! lives in `flake.nix`, and that file is hand-written and hand-formatted, so
//! the edit is textual and surgical: find the one string literal that is this
//! input's URL, replace it, leave every other byte alone.
//!
//! There is no Nix parser here on purpose. A parser would have to round-trip
//! comments and formatting perfectly to be an improvement, and the shapes that
//! actually occur are few. What matters is that unrecognized shapes are
//! *refused* rather than guessed at: [`find_url`] returns `None` and the caller
//! reports the project as skipped, which is recoverable. A bad guess would
//! silently corrupt a file the user maintains by hand, which is not.
//!
//! The four shapes below are the ones real flakes use:
//!
//! ```nix
//! inputs = { nixpkgs.url = "github:nixos/nixpkgs"; };      # 1
//! inputs.nixpkgs.url = "github:nixos/nixpkgs";             # 2
//! inputs = { nixpkgs = { url = "..."; }; };                # 3
//! inputs = { nixpkgs = {                                   # 4
//!   url = "...";
//!   inputs.foo.follows = "bar";
//! }; };
//! ```

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// The located URL literal: the byte range of its *contents*, without quotes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlSpan {
    pub start: usize,
    pub end: usize,
}

impl UrlSpan {
    pub fn text<'a>(&self, source: &'a str) -> &'a str {
        &source[self.start..self.end]
    }
}

/// Whether `c` can appear in a Nix attribute path (identifier chars plus the
/// separators that make up a dotted path like `inputs.nixpkgs.url`).
fn is_attr_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '\'' | '.')
}

/// Whether the attribute-path token ending at `end` is exactly `name`,
/// as a whole token rather than a suffix of a longer one.
///
/// Without this, looking for `nixpkgs` would match the `nixpkgs` inside
/// `nixpkgs-unstable`, and rewrite the wrong input — a mistake that is easy to
/// make and hard to notice, since both are plausible nixpkgs URLs.
fn token_ends_at(source: &str, end: usize, name: &str) -> bool {
    let Some(start) = end.checked_sub(name.len()) else {
        return false;
    };
    if !source.is_char_boundary(start) || &source[start..end] != name {
        return false;
    }
    // The char before must not extend the token — but `.` does not count, since
    // `inputs.nixpkgs` legitimately ends with the `nixpkgs` token.
    match source[..start].chars().next_back() {
        Some(c) if is_attr_char(c) && c != '.' => false,
        _ => true,
    }
}

/// Find the string literal holding `input_name`'s URL.
///
/// Scans for `url` assignments and decides, for each, whether it belongs to
/// this input — either because the same statement names it (`nixpkgs.url`,
/// `inputs.nixpkgs.url`) or because it sits inside a `name = { … }` block
/// opened for it. Returns `None` when nothing matches, which the caller must
/// treat as "cannot edit this file", not "no change needed".
pub fn find_url(source: &str, input_name: &str) -> Option<UrlSpan> {
    // Track the innermost `name = {` block we are inside, so shape 3 and 4 can
    // attribute a bare `url = ...` to the right input.
    let mut block_stack: Vec<Option<String>> = Vec::new();
    let bytes = source.as_bytes();
    let mut i = 0;

    while i < source.len() {
        if !source.is_char_boundary(i) {
            i += 1;
            continue;
        }
        let rest = &source[i..];

        // Skip over comments and strings so their contents never look like code.
        if rest.starts_with('#') {
            i += rest.find('\n').unwrap_or(rest.len());
            continue;
        }
        if rest.starts_with("/*") {
            i += rest[2..].find("*/").map(|n| n + 4).unwrap_or(rest.len());
            continue;
        }
        if rest.starts_with("''") {
            i += rest[2..].find("''").map(|n| n + 4).unwrap_or(rest.len());
            continue;
        }
        if bytes[i] == b'"' {
            i += string_len(rest);
            continue;
        }

        if bytes[i] == b'{' {
            // Attribute the block to whatever `name =` precedes it, if any.
            block_stack.push(preceding_binding(source, i).map(str::to_string));
            i += 1;
            continue;
        }
        if bytes[i] == b'}' {
            block_stack.pop();
            i += 1;
            continue;
        }

        // A `url` token, as a whole word.
        if rest.starts_with("url") && token_ends_at(source, i + 3, "url") {
            let after = i + 3;
            // Must be an assignment: `url = "..."`.
            let Some(eq) = skip_ws_to(source, after, '=') else {
                i += 3;
                continue;
            };
            let value = skip_ws(source, eq + 1);
            if value >= source.len() || bytes[value] != b'"' {
                i = after;
                continue;
            }

            // Whose url is it? Either the attr path names the input, or the
            // enclosing `name = { ... }` block does.
            let owned_by_path = dotted_prefix_exists(source, i)
                && dotted_prefix_names(source, i, input_name);
            let owned_by_block = !dotted_prefix_exists(source, i)
                && block_stack
                    .iter()
                    .rev()
                    .find_map(|b| b.as_deref())
                    .is_some_and(|n| n == input_name);

            if owned_by_path || owned_by_block {
                let literal = &source[value..];
                let len = string_len(literal);
                // Interpolated or escaped URLs are not safely rewritable by
                // substring replacement, so refuse them.
                let inner = &literal[1..len - 1];
                if inner.contains('\\') || inner.contains("${") {
                    return None;
                }
                return Some(UrlSpan { start: value + 1, end: value + len - 1 });
            }
            i = after;
            continue;
        }

        i += 1;
    }

    None
}

/// Length in bytes of the double-quoted string starting at `s[0]`, quotes included.
fn string_len(s: &str) -> usize {
    let b = s.as_bytes();
    let mut i = 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    s.len()
}

fn skip_ws(source: &str, mut i: usize) -> usize {
    let b = source.as_bytes();
    while i < b.len() && (b[i] as char).is_whitespace() {
        i += 1;
    }
    i
}

/// Position of `c` after optional whitespace from `i`, if that is what follows.
fn skip_ws_to(source: &str, i: usize, c: char) -> Option<usize> {
    let j = skip_ws(source, i);
    (source.as_bytes().get(j)? == &(c as u8)).then_some(j)
}

/// Whether a dotted attr path precedes the `url` token at `i`.
fn dotted_prefix_exists(source: &str, i: usize) -> bool {
    i > 0 && source.as_bytes()[i - 1] == b'.'
}

/// Whether the attr path ending just before `url` at `i` names `input_name`
/// as its last component — matching both `nixpkgs.url` and `inputs.nixpkgs.url`.
fn dotted_prefix_names(source: &str, i: usize, input_name: &str) -> bool {
    // Walk back over the whole dotted path, then compare its final component.
    let mut start = i - 1; // the '.'
    while start > 0 {
        let c = source[..start].chars().next_back().unwrap();
        if is_attr_char(c) {
            start -= c.len_utf8();
        } else {
            break;
        }
    }
    let path = source[start..i - 1].trim();
    path.rsplit('.').next().is_some_and(|last| last == input_name)
}

/// The attribute name bound by `name = {` immediately before the brace at `i`.
fn preceding_binding(source: &str, i: usize) -> Option<&str> {
    let head = source[..i].trim_end();
    let head = head.strip_suffix('=')?.trim_end();
    let start = head
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_attr_char(*c))
        .last()
        .map(|(idx, _)| idx)?;
    let name = &head[start..];
    (!name.is_empty()).then_some(name)
}

/// A `flake.nix` edit that has been computed but not performed.
///
/// Separating this from applying it is what lets the plan be shown as a real
/// diff of real file contents before anything is written: the preview is not a
/// description of an intended edit, it *is* the edit, held in memory.
#[derive(Debug, Clone)]
pub struct PendingEdit {
    pub path: PathBuf,
    pub before: String,
    pub after: String,
    pub from_url: String,
    pub to_url: String,
}

impl PendingEdit {
    /// Whether this would change anything on disk.
    pub fn is_noop(&self) -> bool {
        self.before == self.after
    }
}

/// Compute the edit that points `input_name`'s url at `new_url`, without
/// touching disk.
///
/// Errors when the input's url cannot be located: an inputs block shaped in a
/// way this tool will not edit blindly is reported to the user (who can open it
/// in their editor), never guessed at.
pub fn plan_edit(project: &Path, input_name: &str, new_url: &str) -> Result<PendingEdit> {
    let path = project.join("flake.nix");
    let before = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;

    let Some(span) = find_url(&before, input_name) else {
        bail!(
            "could not locate `{input_name}`'s url in {} — its inputs block has a shape this tool will not edit blindly",
            path.display()
        );
    };

    let from_url = span.text(&before).to_string();
    let mut after = String::with_capacity(before.len());
    after.push_str(&before[..span.start]);
    after.push_str(new_url);
    after.push_str(&before[span.end..]);

    Ok(PendingEdit {
        path,
        before,
        after,
        from_url,
        to_url: new_url.to_string(),
    })
}

/// Where the pre-edit copy of `flake.nix` is kept.
pub fn backup_path(path: &Path) -> PathBuf {
    path.with_extension("nix.bak")
}

/// Write a planned edit, verifying the result still evaluates.
///
/// A `.bak` is written before anything is modified, and a file that no longer
/// parses is restored from it — an unusable `flake.nix` is far worse than an
/// un-merged input. The backup is left in place afterwards so the user always
/// has the original to diff against or restore by hand.
pub fn apply_edit(edit: &PendingEdit) -> Result<PathBuf> {
    let backup = backup_path(&edit.path);
    if edit.is_noop() {
        return Ok(backup);
    }

    std::fs::write(&backup, &edit.before)
        .with_context(|| format!("writing backup {}", backup.display()))?;
    std::fs::write(&edit.path, &edit.after)
        .with_context(|| format!("writing {}", edit.path.display()))?;

    let project = edit.path.parent().unwrap_or(&edit.path);
    if let Err(e) = verify(project) {
        std::fs::write(&edit.path, &edit.before).ok();
        return Err(e);
    }

    Ok(backup)
}

/// Confirm the edited flake still evaluates.
///
/// `--no-update-lock-file` keeps this a read-only check: the lockfile is the
/// next step's job, and a metadata call that rewrote it would obscure what the
/// sync actually did.
fn verify(project: &Path) -> Result<()> {
    let out = std::process::Command::new("nix")
        .args(["flake", "metadata", "--no-update-lock-file", "--no-write-lock-file"])
        .current_dir(project)
        .output()
        .with_context(|| format!("running nix flake metadata in {}", project.display()))?;
    if !out.status.success() {
        bail!(
            "edited flake.nix no longer evaluates in {} (restored from backup): {}",
            project.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(source: &str, name: &str) -> Option<String> {
        find_url(source, name).map(|s| s.text(source).to_string())
    }

    #[test]
    fn shape_1_attr_in_inputs_block() {
        let src = r#"{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };
}"#;
        assert_eq!(found(src, "nixpkgs").as_deref(), Some("github:nixos/nixpkgs/nixpkgs-unstable"));
        assert_eq!(found(src, "flake-utils").as_deref(), Some("github:numtide/flake-utils"));
    }

    #[test]
    fn shape_2_top_level_dotted_path() {
        let src = r#"{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  inputs.flake-parts.url = "github:hercules-ci/flake-parts";
}"#;
        assert_eq!(found(src, "nixpkgs").as_deref(), Some("github:NixOS/nixpkgs/nixpkgs-unstable"));
        assert_eq!(found(src, "flake-parts").as_deref(), Some("github:hercules-ci/flake-parts"));
    }

    #[test]
    fn shape_3_nested_attrset_one_line() {
        let src = r#"{
  inputs = {
    flake-parts = { url = "github:hercules-ci/flake-parts"; };
  };
}"#;
        assert_eq!(found(src, "flake-parts").as_deref(), Some("github:hercules-ci/flake-parts"));
    }

    #[test]
    fn shape_4_nested_attrset_multiline() {
        let src = r#"{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    treefmt-nix = {
      url = "github:numtide/treefmt-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };
}"#;
        assert_eq!(found(src, "treefmt-nix").as_deref(), Some("github:numtide/treefmt-nix"));
        assert_eq!(found(src, "nixpkgs").as_deref(), Some("github:NixOS/nixpkgs/nixpkgs-unstable"));
    }

    #[test]
    fn a_prefix_of_another_input_is_not_matched() {
        // The exact hazard in the author's own config: `nixpkgs` and
        // `nixpkgs-unstable` coexist, and a substring match rewrites the wrong
        // one — producing a plausible-looking but incorrect edit.
        let src = r#"{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixpkgs-unstable";
    nixpkgs-unstable.url = "github:nixos/nixpkgs";
  };
}"#;
        assert_eq!(found(src, "nixpkgs").as_deref(), Some("github:nixos/nixpkgs/nixpkgs-unstable"));
        assert_eq!(found(src, "nixpkgs-unstable").as_deref(), Some("github:nixos/nixpkgs"));
    }

    #[test]
    fn follows_lines_are_not_mistaken_for_urls() {
        // `darwin.inputs.nixpkgs.follows` mentions nixpkgs but assigns no url.
        let src = r#"{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixpkgs-unstable";
    darwin.url = "github:nix-darwin/nix-darwin/master";
    darwin.inputs.nixpkgs.follows = "nixpkgs";
  };
}"#;
        assert_eq!(found(src, "darwin").as_deref(), Some("github:nix-darwin/nix-darwin/master"));
        assert_eq!(found(src, "nixpkgs").as_deref(), Some("github:nixos/nixpkgs/nixpkgs-unstable"));
    }

    #[test]
    fn a_nested_inputs_url_belongs_to_its_own_block() {
        // The inner `nixpkgs.url` overrides a *dependency's* input, and must not
        // be mistaken for this flake's own nixpkgs declaration.
        let src = r#"{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/a";
    dep = {
      url = "github:x/dep";
      inputs.nixpkgs.url = "github:nixos/nixpkgs/b";
    };
  };
}"#;
        // The outer declaration is the one found first and the one we mean.
        assert_eq!(found(src, "nixpkgs").as_deref(), Some("github:nixos/nixpkgs/a"));
        assert_eq!(found(src, "dep").as_deref(), Some("github:x/dep"));
    }

    #[test]
    fn comments_are_not_scanned() {
        let src = r#"{
  inputs = {
    # nixpkgs.url = "github:nixos/nixpkgs/commented-out";
    nixpkgs.url = "github:nixos/nixpkgs/real";
  };
}"#;
        assert_eq!(found(src, "nixpkgs").as_deref(), Some("github:nixos/nixpkgs/real"));
    }

    #[test]
    fn missing_input_is_refused_not_guessed() {
        let src = r#"{ inputs = { nixpkgs.url = "github:nixos/nixpkgs"; }; }"#;
        assert_eq!(find_url(src, "flake-utils"), None);
    }

    #[test]
    fn an_input_that_only_follows_has_no_url_to_rewrite() {
        // Real shape, from typelevel-nix consumers: `nixpkgs` is an alias into
        // another input rather than a declaration of its own. There is no url
        // here, so there is nothing this tool can merge — refusing is correct,
        // and inventing a `nixpkgs.url` would change the flake's meaning.
        let src = r#"{
  inputs = {
    typelevel-nix.url = "github:typelevel/typelevel-nix";
    nixpkgs.follows = "typelevel-nix/nixpkgs";
  };
}"#;
        assert_eq!(find_url(src, "nixpkgs"), None);
        assert_eq!(
            found(src, "typelevel-nix").as_deref(),
            Some("github:typelevel/typelevel-nix")
        );
    }

    #[test]
    fn interpolated_urls_are_refused() {
        // Substring replacement cannot reason about what the interpolation
        // evaluates to, so editing it would be a guess.
        let src = r#"{ inputs = { nixpkgs.url = "github:nixos/${branch}"; }; }"#;
        assert_eq!(find_url(src, "nixpkgs"), None);
    }

    #[test]
    fn planning_an_edit_touches_only_the_target_line() {
        // Modelled on a real system config where `nixpkgs` and
        // `nixpkgs-unstable` sit on adjacent lines and several `follows`
        // entries mention `nixpkgs` by name.
        let dir = std::env::temp_dir().join("flake-syncer-plan-edit-test");
        std::fs::create_dir_all(&dir).unwrap();
        let src = r#"{
  description = "config";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixpkgs-unstable";
    nixpkgs-unstable.url = "github:nixos/nixpkgs";
    darwin.url = "github:nix-darwin/nix-darwin/master";
    darwin.inputs.nixpkgs.follows = "nixpkgs";
  };
}
"#;
        std::fs::write(dir.join("flake.nix"), src).unwrap();

        let edit = plan_edit(&dir, "nixpkgs", "github:nixos/nixpkgs/nixos-unstable").unwrap();
        assert_eq!(edit.from_url, "github:nixos/nixpkgs/nixpkgs-unstable");

        // Exactly one line differs, and the neighbouring input is untouched.
        let differing = edit
            .before
            .lines()
            .zip(edit.after.lines())
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(differing, 1, "only nixpkgs' own line may change");
        assert!(edit.after.contains(r#"nixpkgs-unstable.url = "github:nixos/nixpkgs";"#));
        assert!(edit.after.contains(r#"darwin.inputs.nixpkgs.follows = "nixpkgs";"#));
        assert!(edit.after.contains(r#"nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";"#));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_failed_verify_restores_the_original_and_keeps_the_backup() {
        // The safety property that makes automatic rewriting acceptable: if the
        // edited file does not evaluate, the user's file comes back byte for
        // byte. Forced here by pointing at a directory `nix` cannot evaluate.
        let dir = std::env::temp_dir().join("flake-syncer-restore-test");
        std::fs::create_dir_all(&dir).unwrap();
        // Deliberately not a valid flake: no outputs, so evaluation fails.
        let src = "{\n  inputs.nixpkgs.url = \"github:nixos/nixpkgs\";\n}\n";
        let path = dir.join("flake.nix");
        std::fs::write(&path, src).unwrap();

        let edit = plan_edit(&dir, "nixpkgs", "github:nixos/nixpkgs/nixos-unstable").unwrap();
        let result = apply_edit(&edit);

        // Whether nix is installed decides which branch runs; both must leave
        // the file in a state the user can live with.
        match result {
            Err(_) => {
                assert_eq!(
                    std::fs::read_to_string(&path).unwrap(),
                    src,
                    "a rejected edit must restore the original exactly"
                );
                assert_eq!(
                    std::fs::read_to_string(backup_path(&path)).unwrap(),
                    src,
                    "the backup must hold the pre-edit content"
                );
            }
            Ok(backup) => {
                assert_eq!(std::fs::read_to_string(&backup).unwrap(), src);
                assert!(std::fs::read_to_string(&path).unwrap().contains("nixos-unstable"));
            }
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_edit_to_the_current_value_is_a_noop() {
        let dir = std::env::temp_dir().join("flake-syncer-noop-edit-test");
        std::fs::create_dir_all(&dir).unwrap();
        let src = "{\n  inputs.nixpkgs.url = \"github:nixos/nixpkgs\";\n}\n";
        std::fs::write(dir.join("flake.nix"), src).unwrap();

        let edit = plan_edit(&dir, "nixpkgs", "github:nixos/nixpkgs").unwrap();
        assert!(edit.is_noop(), "identical url must not rewrite the file");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn replacement_preserves_every_other_byte() {
        let src = "{\n  inputs = {\n    nixpkgs.url = \"github:nixos/nixpkgs/old\";  # keep me\n  };\n}\n";
        let span = find_url(src, "nixpkgs").unwrap();
        let out = format!("{}{}{}", &src[..span.start], "github:nixos/nixpkgs/new", &src[span.end..]);
        assert_eq!(
            out,
            "{\n  inputs = {\n    nixpkgs.url = \"github:nixos/nixpkgs/new\";  # keep me\n  };\n}\n"
        );
    }
}

