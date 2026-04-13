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
  to subcommands (`update`, `self-update`, `list`, `check`, `version`).
- **`install.sh`** — curl-pipeable installer. Clones the repo, symlinks the
  CLI into `~/.local/bin`. Idempotent (re-run updates). Supports `--uninstall`.
- **`test/shdeps-test`** — test runner. Run with: `./test/shdeps-test`

## Naming Conventions

- Public API functions: `shdeps_` prefix (e.g., `shdeps_update`, `shdeps_load`, `shdeps_platform_match`, `shdeps_host_match`, `shdeps_log`, `shdeps_pkg_mgr`)
- Internal functions: `_shdeps_` prefix (e.g., `_shdeps_parse`, `_shdeps_pkg_detect`)
- Internal global variables: `_SHDEPS_` prefix

## Configuration

All behavior is controlled via environment variables (no hardcoded paths):

| Variable | Default | Description |
|---|---|---|
| `SHDEPS_CONF_DIR` | `~/.config/shdeps/` (CLI) or `./shdeps/` (library) | Config directory (all `*.conf` files loaded) |
| `SHDEPS_HOOKS_DIR` | `<conf_dir>/hooks.d` | Post-install hooks |
| `SHDEPS_STATE_DIR` | `${XDG_STATE_HOME:-$HOME/.local/state}/shdeps` | Cache/state dir |
| `SHDEPS_FORCE` | `0` | Force reinstall all deps |
| `SHDEPS_QUIET` | `0` | Suppress interactive prompts |
| `SHDEPS_REMOTE_TTL` | `3600` | Cache TTL in seconds |
| `SHDEPS_LOG_LEVEL` | `1` | Logging: 0=quiet, 1=normal, 2=verbose |

## Config File Format

```
# name    method    [cmd]  [cmd_alt]  [pkg_overrides]  [repo]  [dir]  [platforms]  [hosts]
jq        pkg
bat       pkg       bat    batcat
fd        pkg       fd     fdfind     apt:fd-find,dnf:fd-find
ds        git       -      -          -                cgraf78/ds.git   .local/share/ds
neovim    binary    nvim   -          -                neovim/neovim
nerd-fonts custom
codex     binary    -      -          -                openai/codex     -            -       nas
```

Methods: `pkg` (system package manager), `git` (GitHub clone), `binary` (GitHub release), `custom` (hook-only).

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
