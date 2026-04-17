# AGENTS.md

This file provides context for AI agents when working in this repo.

## About

**shdeps** is a standalone shell dependency manager. It reads declarative
config files (`*.conf`) from a config directory and installs/updates tools
via system package managers, GitHub repos, or GitHub release binaries.
Post-install hooks let callers run arbitrary setup after each dependency
changes.

## Architecture

- **`shdeps.sh`** — the core library. Sourceable by any bash script.
  Caller does: `source shdeps.sh; shdeps_update`
- **`bin/shdeps`** — CLI wrapper. Parses args, sources `shdeps.sh`, dispatches
  to subcommands (`update`, `self-update`, `list`, `check`, `prune`, `version`).
- **`install.sh`** — curl-pipeable installer and bootstrap script. Clones the
  repo, symlinks the CLI into `~/.local/bin`. Idempotent (re-run updates).
  Supports `--uninstall` and `--bootstrap` (sourceable mode for client
  integration — finds shdeps, sources it, symlinks CLI, runs self-update).
- **`test/shdeps-test`** — test runner. Run with: `./test/shdeps-test`

## Code Organization

- **Public API section** at the top of `shdeps.sh` contains every `shdeps_` function — the complete public contract in one place. Most are thin wrappers that delegate to internal `_shdeps_` implementations.
- **Internal sections** below are organized by concern (config parsing, platform matching, package management, etc.) and use `_shdeps_` prefixes.
- **Internal global variables** use `_SHDEPS_` prefix.
- When adding new public functions, define the implementation as `_shdeps_` in the appropriate internal section, then add a `shdeps_` wrapper to the public API section.

## Configuration

All behavior is controlled via environment variables (no hardcoded paths):

| Variable | Default | Description |
|---|---|---|
| `SHDEPS_CONF_DIR` | `~/.config/shdeps/` (CLI) or `./shdeps/` (library) | Config directory (all `*.conf` files loaded) |
| `SHDEPS_HOOKS_DIR` | `<conf_dir>/hooks.d` | Post-install hooks |
| `SHDEPS_STATE_DIR` | `${XDG_STATE_HOME:-$HOME/.local/state}/shdeps` | Cache/state dir |
| `SHDEPS_FORCE` | `0` | Bypass TTL cache |
| `SHDEPS_REINSTALL` | `0` | Force reinstall all deps |
| `SHDEPS_QUIET` | `0` | Suppress interactive prompts |
| `SHDEPS_REMOTE_TTL` | `3600` | Cache TTL in seconds |
| `SHDEPS_GIT_DEV_DIR` | `~/git` | Dev clone directory for the `github:repo` method |
| `SHDEPS_INSTALL_DIR` | `~/.local/share` | Base directory for `github:*`, `cargo`, `go`, and `uv` installs (each dep lives in `<dir>/<name>/`) |
| `SHDEPS_BIN_DIR` | `~/.local/bin` | Directory for binary symlinks |
| `SHDEPS_LOG_LEVEL` | `1` | Logging: 0=quiet, 1=normal, 2=verbose |

## Config File Format

```
# name              method           [cmd]            [aliases]                [filter]
jq                  pkg
bat                 pkg              apt:batcat
fd                  pkg              apt:fdfind       apt:fd-find,dnf:fd-find
cgraf78/ds          github:repo
neovim/neovim       github:release   nvim
ripgrep             cargo            rg
github.com/junegunn/fzf              go
ruff                uv
nerd-fonts          custom
openai/codex        github:release   -                -                        host:nas
dust                pkg              -                -                        os:macos
```

Methods: `pkg` (system package manager), `github:repo` (GitHub clone), `github:release` (GitHub release binary), `cargo` (Rust crate), `go` (Go module), `uv` (Python CLI tool), `custom` (hook-only).
Fields are ordered most-used to least-used. For `github:repo`/`github:release`, the `owner/repo` is the `name` field. For `go`, the full module path (e.g. `github.com/junegunn/fzf`) is the `name`. `cmd` supports `mgr:name` qualifiers (e.g., `apt:batcat`). `aliases` holds per-manager package name overrides for `pkg` deps. `filter` uses `os:` and `host:` prefixes (e.g., `os:linux`, `host:nas`, `os:!wsl`).

## State Tracking

shdeps tracks installed deps in a manifest file at
`$SHDEPS_STATE_DIR/manifest`. Each line is pipe-delimited:
`name|method|cmd|install_path`. Written automatically during `shdeps update`.

When a dep is removed from config but still in the manifest, `shdeps update`
prints an orphan notice. Run `shdeps prune` to remove orphaned artifacts.

## Extras Linking

shdeps auto-discovers man pages and shell completions from `github`
installs and symlinks them to XDG user-local directories. `cargo`, `go`,
and `uv` installs produce single binaries only — users should generate
extras from the tool itself in a `post()` hook:

| Type | Target | Auto-discovered? |
|------|--------|-----------------|
| Man pages | `~/.local/share/man/man<N>/` | No — needs `MANPATH` |
| Bash completions | `~/.local/share/bash-completion/completions/` | Yes |
| Zsh completions | `~/.local/share/zsh/site-functions/` | No — needs `fpath` |
| Fish completions | `~/.local/share/fish/vendor_completions.d/` | Yes |

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
