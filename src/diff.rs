//! Unified diffs of pending and completed `flake.nix` edits.
//!
//! Every edit this tool makes to a hand-maintained file is shown as a diff
//! *before* it is applied and again *after*, so nothing changes on disk that the
//! user has not seen in full. That requires computing the diff from two strings
//! in memory — the preview must exist before the write does, so shelling out to
//! `diff(1)` against a file that does not yet exist is not an option.
//!
//! Rendering is delegated to a configurable command (`delta` by default). If it
//! is missing or fails, the plain unified text is used: a missing pretty-printer
//! must never be the reason an edit goes unreviewed.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// A unified diff of `before` → `after`, labelled with `path`.
///
/// Empty when the two are identical, which callers use to skip no-op edits.
pub fn unified(path: &Path, before: &str, after: &str) -> String {
    if before == after {
        return String::new();
    }
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();

    let mut out = format!("--- a/{}\n+++ b/{}\n", path.display(), path.display());
    for hunk in hunks(&a, &b, 3) {
        out.push_str(&hunk);
    }
    out
}

/// Group changed lines into unified-diff hunks with `context` lines around each.
///
/// The edits here are single-line url replacements, so a full LCS diff would be
/// machinery without a purpose: line-by-line comparison plus a trailing-suffix
/// match covers replacement, insertion and deletion at one site, which is what
/// [`crate::nix_edit`] produces.
fn hunks(a: &[&str], b: &[&str], context: usize) -> Vec<String> {
    let changed = changed_range(a, b);
    let Some((start, a_end, b_end)) = changed else {
        return Vec::new();
    };

    let from = start.saturating_sub(context);
    let a_to = (a_end + context).min(a.len());
    let b_to = (b_end + context).min(b.len());

    let mut hunk = format!(
        "@@ -{},{} +{},{} @@\n",
        from + 1,
        a_to - from,
        from + 1,
        b_to - from
    );
    for line in &a[from..start] {
        hunk.push_str(&format!(" {line}\n"));
    }
    for line in &a[start..a_end] {
        hunk.push_str(&format!("-{line}\n"));
    }
    for line in &b[start..b_end] {
        hunk.push_str(&format!("+{line}\n"));
    }
    for line in &a[a_end..a_to] {
        hunk.push_str(&format!(" {line}\n"));
    }
    vec![hunk]
}

/// The span of lines that differ: common prefix, then common suffix.
fn changed_range(a: &[&str], b: &[&str]) -> Option<(usize, usize, usize)> {
    let mut start = 0;
    while start < a.len() && start < b.len() && a[start] == b[start] {
        start += 1;
    }
    if start == a.len() && start == b.len() {
        return None;
    }
    let mut back = 0;
    while back < a.len() - start && back < b.len() - start && a[a.len() - 1 - back] == b[b.len() - 1 - back] {
        back += 1;
    }
    Some((start, a.len() - back, b.len() - back))
}

/// Render a diff through the configured command, falling back to plain text.
///
/// Returns the rendered string rather than writing to the terminal directly:
/// the TUI owns the screen, so output has to be captured and drawn as part of a
/// frame.
pub fn render(diff: &str, differ: &[String]) -> String {
    if diff.is_empty() {
        return String::new();
    }
    let Some((program, args)) = differ.split_first() else {
        return diff.to_string();
    };

    let spawned = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();

    let Ok(mut child) = spawned else {
        return diff.to_string();
    };
    if let Some(mut stdin) = child.stdin.take() {
        // A differ that exits before reading everything (or is not there at all)
        // must not take the whole run down with a broken pipe.
        let _ = stdin.write_all(diff.as_bytes());
    }
    match child.wait_with_output() {
        Ok(out) if out.status.success() && !out.stdout.is_empty() => {
            String::from_utf8_lossy(&out.stdout).into_owned()
        }
        _ => diff.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p() -> &'static Path {
        Path::new("flake.nix")
    }

    #[test]
    fn identical_input_produces_no_diff() {
        // Callers rely on emptiness to mean "no edit needed".
        assert_eq!(unified(p(), "a\nb\n", "a\nb\n"), "");
    }

    #[test]
    fn a_single_line_change_is_shown_with_context() {
        let before = "{\n  inputs = {\n    nixpkgs.url = \"github:nixos/nixpkgs/old\";\n  };\n}\n";
        let after = "{\n  inputs = {\n    nixpkgs.url = \"github:nixos/nixpkgs/new\";\n  };\n}\n";
        let d = unified(p(), before, after);
        assert!(d.contains("--- a/flake.nix"), "{d}");
        assert!(d.contains("-    nixpkgs.url = \"github:nixos/nixpkgs/old\";"), "{d}");
        assert!(d.contains("+    nixpkgs.url = \"github:nixos/nixpkgs/new\";"), "{d}");
        // Surrounding lines are context, not changes.
        assert!(d.contains(" {\n"), "{d}");
        assert!(!d.contains("-  };"), "{d}");
    }

    #[test]
    fn only_the_changed_line_is_marked() {
        let before = "a\nb\nc\n";
        let after = "a\nB\nc\n";
        let d = unified(p(), before, after);
        // Count body lines only — the `---`/`+++` header is not a change.
        let body: Vec<&str> = d.lines().skip(2).collect();
        assert_eq!(body.iter().filter(|l| l.starts_with('-')).count(), 1, "{d}");
        assert_eq!(body.iter().filter(|l| l.starts_with('+')).count(), 1, "{d}");
    }

    #[test]
    fn a_missing_differ_falls_back_to_plain_text() {
        // Not having `delta` installed must never mean an edit goes unreviewed.
        let diff = "--- a/x\n+++ b/x\n@@ -1,1 +1,1 @@\n-a\n+b\n";
        let out = render(diff, &["definitely-not-a-real-command-xyz".to_string()]);
        assert_eq!(out, diff);
    }

    #[test]
    fn an_empty_differ_config_uses_plain_text() {
        let diff = "--- a/x\n+++ b/x\n";
        assert_eq!(render(diff, &[]), diff);
    }

    #[test]
    fn nothing_to_render_stays_empty() {
        assert_eq!(render("", &["delta".to_string()]), "");
    }
}
