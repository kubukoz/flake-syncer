# flake-syncer

A TUI that finds flake inputs pinned to different revisions across the user's
projects and converges them. See README.md for what it does and why.

## Never execute for real

This tool **rewrites `flake.lock` and `flake.nix` in the user's own repositories**
under `~/projects`, `~/dev` and `~/.nixpkgs`. Working on it is not permission to
run it against them.

Do not trigger the confirm/execute path — `s` then `Enter` in the TUI, or
anything that calls `sync::apply`, `nix_edit::apply_edit` or
`sync::drop_direnv_roots` — unless the user explicitly asked for that run.
This includes scripted keystrokes: an `expect` script that sends `A s \r` is a
real execution.

Verify instead by:

- unit-testing the state machine in `src/app.rs`;
- rendering screens from a hand-built `App` through ratatui's `TestBackend`
  (see the render tests at the bottom of `src/ui.rs`);
- running read-only invocations (`--report`), which never write;
- staging a plan and reading the confirm screen, which computes every edit in
  memory and writes nothing.

If an end-to-end run is genuinely required, build throwaway flakes in the
scratchpad, confirm from `--report` that only scratch paths are in scope, and
ask first.

**Config cannot be redirected with environment variables.** `scan::config_path`
uses `directories::ProjectDirs`, so on macOS it always reads
`~/Library/Application Support/flake-syncer/config.toml`. Setting
`XDG_CONFIG_HOME` silently has no effect — an override that appears to work but
does not is how a scratch test becomes a live one.

## Testing

`cargo test` — everything is unit tests in-module; there is no integration
suite and none should shell out to `nix` against real projects.
