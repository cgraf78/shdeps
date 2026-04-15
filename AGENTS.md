# AGENTS.md

This file provides context for AI agents when working in this repo.

## About

**shdeps** is a standalone shell dependency manager. It reads declarative
config files (`*.conf`) from a config directory and installs/updates tools
via system package managers, GitHub git repos, or GitHub release binaries.
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
| `SHDEPS_GIT_DEV_DIR` | `~/git` | Dev clone directory for the `git` method |
| `SHDEPS_INSTALL_DIR` | `~/.local/share` | Base directory for `git` and `binary` installs |
| `SHDEPS_BIN_DIR` | `~/.local/bin` | Directory for binary symlinks |
| `SHDEPS_LOG_LEVEL` | `1` | Logging: 0=quiet, 1=normal, 2=verbose |

## Config File Format

```
# name    method    [cmd]  [cmd_alt]  [pkg_overrides]  [repo]  [platforms]  [hosts]
jq        pkg
bat       pkg       bat    batcat
fd        pkg       fd     fdfind     apt:fd-find,dnf:fd-find
ds        git       -      -          -                cgraf78/ds.git
neovim    binary    nvim   -          -                neovim/neovim
nerd-fonts custom
codex     binary    -      -          -                openai/codex     -            nas
```

Methods: `pkg` (system package manager), `git` (GitHub clone), `binary` (GitHub release), `custom` (hook-only).

## State Tracking

shdeps tracks installed deps in a manifest file at
`$SHDEPS_STATE_DIR/manifest`. Each line is pipe-delimited:
`name|method|cmd|install_path`. Written automatically during `shdeps update`.

When a dep is removed from config but still in the manifest, `shdeps update`
prints an orphan notice. Run `shdeps prune` to remove orphaned artifacts.

## Hook Contract

Hook files in `hooks.d/$name.sh` may define these functions:

- `exists(name)` — **required**. Returns 0 if the dep is installed.
- `install(name)` — install the dep. Called when `exists` returns 1.
- `version(name)` — return version string.
- `post(name)` — post-install setup. Runs after any change.
- `uninstall(name)` — **optional**. Called by `shdeps prune` when removing
  an orphaned dep (any method). For custom deps, this is the only cleanup.
  For git/binary deps, this runs before the built-in cleanup — use it to
  reverse what `post()` created (symlinks, config files).

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
