# flake-syncer

A TUI for finding Nix flake inputs that are declared identically across your
projects but pinned to different revisions — and converging them.

```
cargo run --release            # interactive TUI
cargo run --release -- --report  # non-interactive summary
```

## Where the disk space actually is

This is the thing worth knowing before trusting any number the tool prints.

Editing a `flake.lock` frees **nothing** on its own. The bytes are held by GC
roots — in practice `direnv`'s `.direnv/flake-inputs/<store-path>` symlinks,
which pin every input of every project so the garbage collector cannot touch
them. A source path is only reclaimable once *every* root pointing at it is
gone.

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
lockfile; those actions fail loudly and are reported rather than skipped
silently.

After a project's updates all succeed, its `.direnv/flake-inputs` roots are
dropped. A project with any failed update keeps its roots — it still needs the
inputs it has.

## Keys

| key | action |
| --- | --- |
| `↑`/`↓`, `k`/`j` | move |
| `Tab` | switch pane (inputs ↔ versions) |
| `Enter` | set highlighted version as this group's target |
| `x` | exclude group from the plan |
| `a` | toggle divergent-only / all |
| `s` | stage plan (shows dry-run commands) |
| `Enter` (staged) | execute |
| `Esc` | cancel |
| `q` | quit |

Every divergent group defaults to the **newest revision already in use** — no
network, and the store path is usually already present, so converging tends to
free space rather than download more. Override per group in the TUI.

## Config

Written on first run to
`~/Library/Application Support/flake-syncer/config.toml` (macOS):

```toml
roots = ["/Users/you/dev", "/Users/you/projects", "/Users/you/.nixpkgs"]
max_depth = 4
```

`.git`, `.direnv` and `result` are skipped while scanning — `.direnv` in
particular contains materialized copies of inputs whose lockfiles are not
projects you maintain and would skew the counts.
