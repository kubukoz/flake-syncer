# flake-syncer

A TUI for finding Nix flake inputs that are declared identically across your
projects but pinned to different revisions — and converging them.

```
cargo run --release                     # interactive TUI
cargo run --release -- --report         # non-interactive summary
cargo run --release -- --roots          # what holds each rooted store path
cargo run --release -- --all-projects   # include projects with no GC root
```

## Where the disk space actually is

This is the thing worth knowing before trusting any number the tool prints.

Editing a `flake.lock` frees **nothing** on its own. The bytes are held by GC
roots — in practice `direnv`'s `.direnv/flake-inputs/<store-path>` symlinks,
which pin every input of every project so the garbage collector cannot touch
them. A source path is only reclaimable once *every* root pointing at it is
gone.

The root set comes from `nix-store --gc --print-roots`, which is authoritative:
it includes profiles, `result` symlinks and paths held open by running
processes (reported as `{lsof}`), none of which live under
`/nix/var/nix/gcroots/auto`. A path pinned by any non-direnv root is excluded
from the savings figure entirely — dropping direnv's link cannot free it.

Note that `--print-roots` is not a pure read: scanning roots makes Nix prune
`gcroots/auto` symlinks that already point at deleted paths, and it logs each
one. Only dangling pointers go, and no repository file is touched — but the
scan every startup performs does write to GC state.

Two consequences follow, and both are deliberate defaults:

- A project holding **no GC root at all** is skipped. Converging its lockfile
  would free nothing, so it is churn on a file you may not want touched. Press
  `g` in the TUI, or pass `--all-projects`, to include them.
- Only **direct** inputs are considered — see below.

So the tool claims savings for a store path only when the projects retaining it
are all projects you are migrating away from that version. If some project you
are not touching also roots that path, the path stays, and counting it would be
a lie. `store::reclaimable` implements exactly that subset check, and the tests
in `src/store.rs` pin the behaviour down.

Two caveats on the figure, both deliberate:

- It is an **upper bound**. Sizes are a plain byte sum; Nix hardlinks identical
  files between store paths when store optimisation is on, so some of those
  bytes are shared and would not come back.
- Nothing is freed until you run `nix store gc`. The tool only removes the GC
  root symlinks; it never touches `/nix/store` itself.

## The roots view

`nix-store --gc --print-roots` lists roots and leaves you to work out what each
one holds. The roots view inverts that: press `r` in the TUI, or run
`--roots`, and every rooted store path is listed largest first with the roots
holding it — split into direnv projects (droppable by this tool), durable
external roots (profiles, `result` symlinks, channels), and running processes.

`·` marks a path droppable by dropping direnv roots alone; `×` marks one
something else pins.

**Process roots are hidden by default.** A path a process merely has open says
nothing about whether it is durably pinned, and on a working machine `{lsof}`
entries swamp the list — on the author's store they were most of 1048 rows.
Press `p`, or pass `--with-process-roots`, to see them. Paths *only* a process
holds disappear entirely with the filter on, since with their sole retainer
hidden they would list with nothing explaining why.

This is presentation only. `store::reclaimable` still treats a process root as
blocking, so the savings figure never counts space a live process is holding —
hiding a root must never turn into claiming its bytes.

## Identity grouping

Inputs group by their `original` flakeref, normalized:

- GitHub owner/repo fold to lowercase, so `NixOS/nixpkgs` and `nixos/nixpkgs`
  are one identity (GitHub treats them as the same repo).
- Distinct `ref`s stay separate — `nixpkgs-unstable` and `nixos-unstable` are
  genuinely different branches, and converging them silently would change which
  branch a project follows. Merging them is possible, but opt-in: see below.
- `path` and `indirect` inputs are skipped; they are local or registry-relative
  and not shared artifacts.

Node names are not attribute names. When two nodes would collide, Nix suffixes
one (`nixpkgs_2`), so a flake that declares `nixpkgs` can have its own direct
input stored under `nixpkgs_2`. The declared name is recovered by inverting the
root node's `inputs` map, and it — not the node name — is what gets written to
`flake.nix` and passed to `--override-input`.

## Merging refs

One repo is often declared under several refs across projects: `nixos/nixpkgs`,
`@nixos-unstable`, `@nixpkgs-unstable`, `@nixos-24.11`. Each is its own identity,
so none of them looks divergent, and no amount of revision syncing will bring
them together — they are pinned to different branches by declaration.

`m` marks a group, `M` merges every marked group onto the highlighted one. The
merged group then behaves as a single input: one version list, one target, one
plan. `m` on a merged group unmerges it. Nothing is merged by default and a merge
is never persisted — it lasts for the session, and unmerging restores exactly
what was there.

Unlike revision syncing, this **edits `flake.nix`**, because that is where the
ref is declared. For each project whose declared ref is not the canonical one:

```
edit  flake.nix: inputs.<name>.url = "github:owner/repo/<canonical-ref>"
then  nix flake update <name> --override-input <name> github:owner/repo/<rev>
```

A pin on a non-canonical ref is included in the plan even when its revision
already matches the target — the declaration is still wrong, and leaving it would
drift straight back on the next `nix flake update`.

### How flake.nix is edited

Textually, and only the URL string literal — every other byte is left alone,
including comments and formatting. Four layouts are recognized:

```nix
inputs = { nixpkgs.url = "..."; };        # attr inside an inputs block
inputs.nixpkgs.url = "...";               # top-level dotted path
inputs = { nixpkgs = { url = "..."; }; }; # nested attrset
inputs = { nixpkgs = {                    # nested attrset, multi-line
  url = "...";
  inputs.foo.follows = "bar";
}; };
```

Anything else is **refused, not guessed at**. The confirm screen lists the
projects it will not rewrite before you approve the plan, and `e` opens one in
your editor. Cases that are correctly refused include an input declared only as
`nixpkgs.follows = "typelevel-nix/nixpkgs"` (there is no url to change) and a url
built by interpolation (substring replacement cannot reason about it).

Before writing, a `.nix.bak` is saved next to the original. After writing, the
result is checked with `nix flake metadata`; if it no longer evaluates the
original is restored byte for byte and the failure is reported. Backups are kept
rather than cleaned up, so there is always something to diff against.

## Seeing what happens

Every `flake.nix` edit is shown as a real diff — computed from the actual file
contents, not described — on the confirm screen *before* anything is written, and
again in the results afterwards showing what actually landed. Diffs are piped
through a configurable command (`delta` by default); if it is missing or fails,
plain unified output is used, so a missing pretty-printer never means an edit
goes unreviewed.

## Sync mechanism

Lockfiles are never hand-edited. Rewriting `rev` in place would leave `narHash`
and `lastModified` inconsistent with it, producing a lockfile Nix rejects or
silently mistrusts. Instead each action shells out to:

```
nix flake update <input> --override-input <input> github:owner/repo/<rev>
```

Only inputs named directly in a project's own lockfile can be overridden this
way. Transitive inputs (`nixpkgs_2` and friends) belong to a dependency's
lockfile, so they are excluded from divergence counts and from the plan
altogether: a divergence only they exhibit is not one this tool can resolve, and
counting it would promise a fix `--override-input` cannot deliver. Directness is
read from the lockfile's root node `inputs` map, which names exactly the inputs
the flake declares. The details pane reports how many were held back.

After a project's updates all succeed, its `.direnv/flake-inputs` roots are
dropped. A project with any failed update keeps its roots — it still needs the
inputs it has.

## Keys

| key | action |
| --- | --- |
| `↑`/`↓`, `k`/`j` | move, or scroll when the details pane has focus |
| `Tab` | cycle pane (inputs → versions → details) |
| `Enter` | set highlighted version as this group's target |
| `x` | exclude group from the plan |
| `A` / `N` | select all groups at their suggested version / clear all |
| `m` | mark group for merging, or unmerge a merged group |
| `M` | merge marked groups onto the highlighted one |
| `e` | open a `flake.nix` in your editor |
| `a` | toggle divergent-only / all |
| `g` | toggle rooted-only / include projects with no GC root |
| `r` | open the roots view (read-only) |
| `s` | stage plan (shows every command and every diff) |
| `Enter` (staged) | execute |
| `Esc` | cancel |
| `q` | quit |

In the roots view: `↑`/`↓` move, `f` filters to droppable paths only, `p`
toggles process-held paths, `Esc` returns. Nothing there stages or applies
anything.

**Nothing is selected on startup.** Each group's suggested version — the
**newest revision already in use**, marked `·` — needs no network and is usually
already in the store, so converging on it tends to free space rather than
download more. `Enter` accepts it for one group, `A` for all of them. Staging
every divergent group by default would make a destructive plan the path of least
resistance, so it is opt-in.

The details pane at the bottom shows the highlighted version's full project
paths, the input name each was pinned under, and counts of anything held back
(transitive pins, unrooted projects).

## Config

Written on first run to
`~/Library/Application Support/flake-syncer/config.toml` (macOS):

```toml
roots = ["/Users/you/dev", "/Users/you/projects", "/Users/you/.nixpkgs"]
max_depth = 4

# Projects to skip entirely. Absolute entries match the path or any ancestor;
# relative entries match the tail, on component boundaries — so `talks` ignores
# `talks/` and everything under it, but not `talks-archive`.
ignore = ["talks", "/Users/you/projects/scratch"]

# Renders diffs of flake.nix edits; receives a unified diff on stdin.
# Anything that reads a diff from stdin works. Set to [] for plain output.
differ = ["delta", "--paging=never", "--color-only"]

# Opens a flake.nix for manual editing, with the path appended.
# Left at the default, $VISUAL / $EDITOR take precedence if set.
editor = ["nvim"]
```

`.git`, `.direnv` and `result` are skipped while scanning — `.direnv` in
particular contains materialized copies of inputs whose lockfiles are not
projects you maintain and would skew the counts.
