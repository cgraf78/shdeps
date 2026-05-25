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
- **`shdeps-legacy.sh`** — preserved Bash reference implementation. Keep it
  available for parity tests and emergency comparison, but do not add new
  behavior here unless a test explicitly needs to pin legacy behavior.
- **`bin/shdeps-legacy`** — temporary legacy Bash CLI wrapper that sources
  `shdeps-legacy.sh`. Release archives and normal installs should use the
  Rust `shdeps` binary instead, and `bin/` must not contain a `shdeps`
  executable that can shadow the Rust CLI when `.envrc` adds `./bin` to `PATH`.
- **`install.sh`** — curl-pipeable installer and bootstrap script. It installs
  release archives when available, can build explicit source checkouts, symlinks
  the CLI into `~/.local/bin`, and supports `--uninstall` plus sourceable
  `--bootstrap` for client integration.
- **`test/shdeps-test`** — parity test runner. Run Bash reference coverage with
  `./test/shdeps-test` and Rust CLI parity with
  `SHDEPS_IMPL=rust ./test/shdeps-test`.
- **`test/shdeps-wrapper-test`** — focused tests for the sourceable Rust-era
  wrapper API.

## Code Organization

- `shdeps.sh` must stay small: public `shdeps_*` functions should be one-line
  delegating shims unless the helper genuinely must affect the caller's current
  shell, such as `shdeps_dep_source` or changed-marker coordination.
- Hidden `shdeps __api ...` commands are the contract between the Bash wrapper
  and Rust. Keep that bridge narrow, documented, and covered by wrapper tests.
- Rust modules should own real behavior. Prefer adding Rust APIs and CLI/API
  tests over growing Bash wrapper logic.
- Legacy Bash internals still use `_shdeps_` and `_SHDEPS_` names, but those are
  reference implementation details, not new public API.

## Configuration

All behavior is controlled via environment variables (no hardcoded paths):

| Variable             | Default                                            | Description                                                                                                |
| -------------------- | -------------------------------------------------- | ---------------------------------------------------------------------------------------------------------- |
| `SHDEPS_CONF_DIR`    | `~/.config/shdeps/` (CLI) or `./shdeps/` (library) | Config directory (all `*.conf` files loaded)                                                               |
| `SHDEPS_HOOKS_DIR`   | `<conf_dir>/hooks.d`                               | Post-install hooks                                                                                         |
| `SHDEPS_STATE_DIR`   | `${XDG_STATE_HOME:-$HOME/.local/state}/shdeps`     | Cache/state dir                                                                                            |
| `SHDEPS_FORCE`       | `0`                                                | Bypass TTL cache                                                                                           |
| `SHDEPS_REINSTALL`   | `0`                                                | Force reinstall all deps                                                                                   |
| `SHDEPS_QUIET`       | `0`                                                | Suppress interactive prompts                                                                               |
| `SHDEPS_REMOTE_TTL`  | `3600`                                             | Cache TTL in seconds                                                                                       |
| `SHDEPS_GIT_DEV_DIR` | `~/git`                                            | Dev clone directory for the `github:repo` method                                                           |
| `SHDEPS_INSTALL_DIR` | `~/.local/share`                                   | Base directory for `github:*`, `cargo`, `go`, `uv`, and `npm` installs (each dep lives in `<dir>/<name>/`) |
| `SHDEPS_BIN_DIR`     | `~/.local/bin`                                     | Directory for binary symlinks                                                                              |
| `SHDEPS_LOG_LEVEL`   | `1`                                                | Logging: 0=quiet, 1=normal, 2=verbose                                                                      |
| `SHDEPS_JOBS`        | auto (`nproc`)                                     | Max concurrent read-only probes. Explicit values win; `1` = sequential.                                    |

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
```

Methods: `pkg` (system package manager), `github` (auto-resolve to release or repo), `github:repo` (GitHub clone), `github:release` (GitHub release binary), `cargo` (Rust crate), `go` (Go module), `uv` (Python CLI tool), `npm` (Node.js package), `custom` (hook-only).
Fields are ordered most-used to least-used. For `github`/`github:repo`/`github:release`, the `owner/repo` is the `name` field. For `go`, the full module path (e.g. `github.com/junegunn/fzf`) is the `name`. `cmd` supports `mgr:name` qualifiers (e.g., `apt:batcat`). `aliases` holds per-manager package name overrides for `pkg` deps. `filter` uses `os:` and `host:` prefixes (e.g., `os:linux`, `host:nas`, `os:!wsl`).

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

Discovery uses four pattern arrays (`_SHDEPS_MAN_PATTERNS`, `_SHDEPS_BASH_COMP_PATTERNS`,
`_SHDEPS_ZSH_COMP_PATTERNS`, `_SHDEPS_FISH_COMP_PATTERNS`) defined near the top of
`shdeps.sh`. Adding a new convention = appending one glob to the appropriate array.

State tracking: each dep's linked symlinks are recorded in
`$SHDEPS_STATE_DIR/<name>.links`. On re-link (update), stale symlinks are
cleaned before new ones are created. On prune, `_shdeps_unlink_extras` removes
all tracked symlinks.

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

## Code Quality

- shellcheck must pass on all `.sh` files
- All variables quoted, edge cases handled, return codes checked
- Comments explain WHY, not WHAT
- Every function has a brief comment explaining purpose and params

## Testing

Run the test suite:

```bash
./test/shdeps-test
```
