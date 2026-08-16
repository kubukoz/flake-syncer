# flake-syncer

A TUI for finding Nix flake inputs that are declared identically across your
projects but pinned to different revisions — and converging them.

```
cargo run --release                     # interactive TUI
cargo run --release -- --report         # non-interactive summary
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

## Identity grouping

Inputs group by their `original` flakeref, normalized:

- GitHub owner/repo fold to lowercase, so `NixOS/nixpkgs` and `nixos/nixpkgs`
  are one identity (GitHub treats them as the same repo).
- Distinct `ref`s stay separate — `nixpkgs-unstable` and `nixos-unstable` are
  genuinely different branches and merging them would be wrong.
- `path` and `indirect` inputs are skipped; they are local or registry-relative
  and not shared artifacts.

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
| `a` | toggle divergent-only / all |
| `g` | toggle rooted-only / include projects with no GC root |
| `s` | stage plan (shows dry-run commands) |
| `Enter` (staged) | execute |
| `Esc` | cancel |
| `q` | quit |

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
```

`.git`, `.direnv` and `result` are skipped while scanning — `.direnv` in
particular contains materialized copies of inputs whose lockfiles are not
projects you maintain and would skew the counts.
