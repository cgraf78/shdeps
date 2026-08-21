# AGENTS.md

This file provides context for AI agents when working in this repo.

## About

**shdeps** is a standalone shell dependency manager. It reads declarative
config files (`*.conf`) from a config directory and installs/updates tools
via system package managers, GitHub repos, or GitHub release binaries.
Post-install hooks let callers run arbitrary setup after each dependency
changes.

## Architecture

- **Rust crate and binary (`src/`)** — the behavior owner for the CLI,
  install/update logic, state handling, hook execution, and hidden `__api`
  bridge used by shell callers.
- **`shdeps.sh`** — sourceable Bash compatibility wrapper for the Rust
  implementation. Existing callers still do:
  `source shdeps.sh; shdeps_update`.
- **`tests/shell/`** — shell-owned test suites for the sourceable wrapper,
  installer, and release scripts.
- **`install.sh`** — curl-pipeable installer and bootstrap script. It installs
  release archives when available, can build explicit source checkouts, symlinks
  the CLI into `~/.local/bin`, and supports `--uninstall` plus sourceable
  `--bootstrap` for client integration.
- **`tests/shell/shdeps-wrapper-test`** — focused tests for the sourceable
  Rust-era wrapper API.
- **Files listed in `scripts/.release-scripts.manifest`** — generated verbatim
  from the `cgraf78/actions` commit in `.github/cgraf78-actions.lock`. Do not
  edit them here: the same consumer sync that updates workflow pins owns these
  files so executable code cannot drift from the reviewed action commit.
  `scripts/release.conf` and `scripts/release-smoke-hook.sh` are intentionally
  repo-owned; they define shdeps-specific inputs and runtime smoke assertions.

## Code Organization

- `shdeps.sh` must stay small: public `shdeps_*` functions should be one-line
  delegating shims unless the helper genuinely must affect the caller's current
  shell, such as `shdeps_dep_source` or changed-marker coordination.
- Hidden `shdeps __api ...` commands are the contract between the Bash wrapper
  and Rust. Keep that bridge narrow, documented, and covered by wrapper tests.
- Rust modules should own real behavior. Prefer adding Rust APIs and CLI/API
  tests over growing Bash wrapper logic.

## Configuration

All behavior is controlled via environment variables (no hardcoded paths):

| Variable             | Default                                            | Description                                                                                                |
| -------------------- | -------------------------------------------------- | ---------------------------------------------------------------------------------------------------------- |
| `SHDEPS_CONF_DIR`    | `~/.config/shdeps/` (CLI) or `./shdeps/` (library) | Config directory (all `*.conf` files loaded)                                                               |
| `SHDEPS_HOOKS_DIR`   | `<conf_dir>/hooks.d`                               | Post-install hooks                                                                                         |
| `SHDEPS_STATE_DIR`   | `${XDG_STATE_HOME:-$HOME/.local/state}/shdeps`     | Cache/state dir                                                                                            |
| `SHDEPS_FORCE`       | `0`                                                | Bypass TTL cache                                                                                           |
| `SHDEPS_BOOTSTRAP_FORCE` | `0`                                          | Force provider freshness during bootstrap without forcing dependency updates                              |
| `SHDEPS_REINSTALL`   | `0`                                                | Force reinstall all deps                                                                                   |
| `SHDEPS_QUIET`       | `0`                                                | Suppress non-result output and interactive prompts                                                             |
| `SHDEPS_REMOTE_TTL`  | `3600`                                             | Cache TTL in seconds                                                                                       |
| `SHDEPS_SELF_UPDATE_TTL` | `3600`                                         | Self-update attempt TTL for `shdeps update`                                                                |
| `SHDEPS_GIT_DEV_DIR` | `~/git`                                            | Dev clone directory for the `github:repo` method                                                           |
| `SHDEPS_INSTALL_DIR` | `~/.local/share`                                   | Base directory for `github:*`, `cargo`, `go`, `uv`, and `npm` installs (each dep lives in `<dir>/<name>/`) |
| `SHDEPS_BIN_DIR`     | `~/.local/bin`                                     | Directory for binary symlinks                                                                              |
| `SHDEPS_LUA_DIR`     | `~/.local/lib/shdeps`                              | Installer-owned stable link to the active Shdeps Lua API tree                                              |
| `SHDEPS_LOG_LEVEL`   | `1`                                                | Logging: 0=quiet, 1=normal, 2=verbose                                                                      |
| `SHDEPS_JOBS`        | auto (`nproc`)                                     | Max concurrent read-only probes. Explicit values win; `1` = sequential.                                    |
| `SHDEPS_STATE_LOCK_TIMEOUT_SECS` | `1800`                                | Max seconds a mutating command waits for another live `update`/`prune` holder before failing with metadata. |
| `SHDEPS_CHECKOUT_LOCK_TIMEOUT_SECS` | `1800`                             | Max seconds a `github:repo` mutation waits for the shared installer/Shdeps checkout lock; strict nonnegative decimal, at most 9 digits. |

## Config File Format

```text
# name              method           [cmd]            [aliases]                [filter]
jq                  pkg
bat                 pkg              apt:batcat
fd                  pkg              apt:fdfind       apt:fd-find,dnf:fd-find
cgraf78/ds          github
cgraf78/ds          github:repo
neovim/neovim       github:release   nvim
ripgrep             cargo            rg
github.com/junegunn/fzf              go
ruff                uv
nerd-fonts          custom
openai/codex        github:release   -                -                        host:nas
dust                pkg              -                -                        os:macos
ast-grep            pkg              ast-grep         brew:ast-grep,pacman:ast-grep mgr:brew,mgr:pacman
ast-grep            cargo            -                -                        mgr:!brew,mgr:!pacman
```

Methods: `pkg` (system package manager), `github` (auto-resolve to release or repo), `github:repo` (GitHub clone), `github:release` (GitHub release binary), `cargo` (Rust crate), `go` (Go module), `uv` (Python CLI tool), `npm` (Node.js package), `custom` (hook-only).
Fields are ordered most-used to least-used. For `github`/`github:repo`/`github:release`, the `owner/repo` is the `name` field. For `go`, the full module path (e.g. `github.com/junegunn/fzf`) is the `name`. `cmd` supports `mgr:name` qualifiers (e.g., `apt:batcat`). `aliases` holds per-manager package name overrides for `pkg` deps. `filter` uses `os:`, `host:`, and `mgr:` prefixes (e.g., `os:linux`, `host:nas`, `mgr:brew`, `mgr:!pacman`). Termux exposes `mgr:android`.

## State Tracking

shdeps tracks installed deps in a manifest file at
`$SHDEPS_STATE_DIR/manifest`. Each line is pipe-delimited:
`name|method|cmd|install_path`. Written automatically during `shdeps update`.

When a dep is removed from config but still in the manifest, `shdeps update`
prints an orphan notice. Run `shdeps prune` to remove orphaned artifacts.

## Extras Linking

shdeps auto-discovers man pages and shell completions from `github`
installs and symlinks them to XDG user-local directories. `cargo`, `go`,
`uv`, and `npm` installs produce single binaries only — users should generate
extras from the tool itself in a `post()` hook:

| Type             | Target                                        | Auto-discovered?     |
| ---------------- | --------------------------------------------- | -------------------- |
| Man pages        | `~/.local/share/man/man<N>/`                  | No — needs `MANPATH` |
| Bash completions | `~/.local/share/bash-completion/completions/` | Yes                  |
| Zsh completions  | `~/.local/share/zsh/site-functions/`          | No — needs `fpath`   |
| Fish completions | `~/.local/share/fish/vendor_completions.d/`   | Yes                  |

Discovery uses pattern constants in `src/extras.rs`. Adding a new convention =
appending one glob to the appropriate list and covering it with an extras unit
test.

State tracking: each dep's linked symlinks are recorded in
`$SHDEPS_STATE_DIR/<name>.links`. On re-link (update), stale symlinks are
cleaned before new ones are created. On prune, the hidden
`shdeps __api unlink-extras` bridge removes all tracked symlinks.

## Hook Contract

Hook files in `hooks.d/$name.sh` may define these functions (for `github:*`
and `go` deps, hooks go in a nested path mirroring the `name` — e.g.
`hooks.d/owner/repo.sh` or `hooks.d/github.com/owner/repo.sh`):

- `exists(name)` — **required for `custom`**. Returns 0 if the dep is installed.
- `install(name)` — **required for `custom`**. Called when `exists` returns 1.
- `version(name)` — return version string.
- `post(name)` — post-install setup. Runs after any change.
- `uninstall(name)` — **optional**. Called by `shdeps prune` when removing
  an orphaned dep (any method). For custom deps, this is the only cleanup.
  For other methods, runs before the built-in cleanup — use it to reverse
  what `post()` created (symlinks, config files).

### Hook helper toolkit

Fallback-install hooks may call these public helpers instead of re-implementing
the plumbing. Like every `shdeps_*` function they are thin one-line shims; the
behavior lives in Rust (`src/hook_toolkit.rs`, bridged through `__api`).

- `shdeps_skip <dep> [reason]` — record a `.skipped` marker (with an optional
  reason) under the dependency's install dir, e.g. when no runtime is available.
- `shdeps_skipped <dep>` — predicate: `0` if the dep is marked skipped.
- `shdeps_skip_reason <dep>` — print the recorded reason; `1` if not skipped.
- `shdeps_unskip <dep>` — remove the marker.
- `shdeps_find_runtime [--path DIR]... [--reject SUBSTR] [--verify] <name>...` —
  print the first executable found in the `--path` dirs then `$PATH`. `--reject`
  drops a candidate whose `--version` output contains `SUBSTR`; `--verify`
  requires a successful `--version`. Exit `1` when none qualifies.
- `shdeps_write_wrapper [--env VAR=value]... <name> <interp> [args...] -- <payload>`
  — write an executable `<bin_dir>/<name>` that execs
  `<interp> <args...> <payload> "$@"`. `--env` lines are emitted before the exec
  (e.g. a gem `PATH=...:$PATH`). Prints the wrapper path.

Initial mutating hook subprocesses are deliberately detached with closed stdin
so timeouts can kill their complete process group. `shdeps_require_sudo` must
not prompt from that detached child: after a failed `sudo -n` probe it requests
authentication from the attached parent, which pauses progress, runs the
prompt, and retries the hook once. The authenticated retry must keep closed
stdin and its own killable process group but remain in the parent's session so
terminal-scoped sudo timestamps and subsequent direct `sudo` commands work.
Keep quiet mode noninteractive, preserve the direct helper's normal prompt
behavior outside hooks, and never prompt for an `install()` that was skipped
because `exists()` already succeeded.
Hooks are trusted same-user code, but the request file still requires a private,
parent-created regular file and a no-follow open on Unix; do not weaken that
defense or treat an exit status alone as a prompt request.
The retry restarts the hook function, so hooks that need sudo must call
`shdeps_require_sudo` before any side effect. If an install attempt changes its
`exists()` result before requesting sudo, the retry fails closed instead of
silently accepting the partial installation or running it twice.

## Code Quality

- ShellCheck must pass on every program in `.github/shellcheck-files.txt`;
  the shared CI inventory gate rejects newly discovered shell programs until
  they are reviewed and classified.
- All variables quoted, edge cases handled, return codes checked
- Comments explain WHY, not WHAT
- Every function has a brief comment explaining purpose and params

## Testing

Run the test suite:

```bash
cargo test --locked
tests/shell/install-sh-test
tests/shell/install-interruption-test
tests/shell/installer-flow-test
tests/shell/release-scripts-test
SHDEPS_RUST_CLI=target/debug/shdeps tests/shell/shdeps-wrapper-test
```
