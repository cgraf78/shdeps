# shdeps

![Tests](https://github.com/cgraf78/shdeps/actions/workflows/test.yml/badge.svg?branch=main)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Bash Version](https://img.shields.io/badge/bash-%3E%3D4.0-blue.svg)](https://www.gnu.org/software/bash/)
[![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20macOS%20%7C%20WSL-lightgrey.svg)](#)

A standalone shell dependency manager. Declare your tools in a config file, and shdeps installs them via system package managers, GitHub git repos, or GitHub release binaries.

## Features

- **Declarative config** — one line per dependency in `deps.conf`
- **Multiple install methods** — system packages (brew/apt/dnf/pacman), git clones, GitHub release binaries, or fully custom hooks
- **Cross-platform** — Linux, macOS, WSL with platform and hostname filtering per dep
- **Package manager abstraction** — batched installs with individual retry fallback
- **Smart binary matching** — multi-pass asset selection by OS, arch, and libc
- **TTL-based caching** — avoids redundant network calls
- **Post-install hooks** — run arbitrary setup when a dependency changes
- **Local overrides** — `deps.local.conf` for machine-specific deps
- **Usable as CLI or library** — `bin/shdeps` CLI or `source shdeps.sh`

## Quick Start

```bash
git clone https://github.com/cgraf78/shdeps.git ~/.local/share/shdeps

# Create a config file
cat > deps.conf << 'EOF'
jq    pkg
fzf   pkg
EOF

# Run it
~/.local/share/shdeps/bin/shdeps -c deps.conf update
```

Or symlink the CLI into your PATH:

```bash
ln -sf ~/.local/share/shdeps/bin/shdeps ~/.local/bin/shdeps
shdeps update
```

The CLI automatically finds `~/.config/shdeps/deps.conf` when no `-c` flag or `SHDEPS_CONF` env var is set. The library (`source shdeps.sh`) defaults to `./deps.conf`.

## Configuration

### deps.conf Format

```
# name    method    [cmd]  [cmd_alt]  [pkg_overrides]  [repo]  [dir]  [platforms]  [hosts]
```

| Field | Required | Description |
|---|---|---|
| `name` | yes | Dependency name (used for hooks, logging, tracking) |
| `method` | yes | Install method: `pkg`, `git`, `binary`, or `custom` |
| `cmd` | no | Binary to check for existence (defaults to name) |
| `cmd_alt` | no | Alternate binary name (e.g., `batcat` for `bat`) |
| `pkg_overrides` | no | Per-manager package names: `apt:fd-find,dnf:fd-find` |
| `repo` | no | GitHub `owner/repo` (for `git` and `binary` methods) |
| `dir` | no | Install directory relative to `$HOME` (for `git` method) |
| `platforms` | no | Platform filter: `linux,darwin`, `!wsl`, etc. |
| `hosts` | no | Hostname filter: `nas,taylor`, `!workstation`, etc. |

Use `-` for fields you want to skip. See [examples/deps.conf](examples/deps.conf) for a full example.

### Environment Variables

| Variable | Default | Description |
|---|---|---|
| `SHDEPS_CONF` | `~/.config/shdeps/deps.conf` (CLI) or `./deps.conf` (library) | Main config file |
| `SHDEPS_CONF_LOCAL` | `<conf_dir>/deps.local.conf` | Local overrides (same dir as conf) |
| `SHDEPS_HOOKS_DIR` | `<conf_dir>/hooks.d` | Post-install hooks directory |
| `SHDEPS_STATE_DIR` | `$XDG_STATE_HOME/shdeps` | Cache/state directory |
| `SHDEPS_FORCE` | `0` | Force reinstall all deps |
| `SHDEPS_QUIET` | `0` | Suppress interactive prompts |
| `SHDEPS_REMOTE_TTL` | `3600` | Cache TTL in seconds |
| `SHDEPS_LOG_LEVEL` | `1` | 0=quiet, 1=normal, 2=verbose |

## Install Methods

### `pkg` — System Packages

Installs via the detected package manager (brew, apt, dnf, or pacman). Packages are batched into a single install command for speed.

```
jq        pkg
bat       pkg    bat    batcat
fd        pkg    fd     fdfind    apt:fd-find,dnf:fd-find
dust      pkg    -      -         -              -    -    darwin
htop      pkg    -      -         -              -    -    -        nas
```

Use `pkg_overrides` to map names across package managers. Use `NONE` to skip a dep on a specific manager (e.g., `brew:NONE`). Use `hosts` to limit a dep to specific machines.

### `git` — GitHub Git Repos

Clones a GitHub repo into `$HOME/<dir>`. Prefers local clones in `~/git/<name>` (symlinked for live development). Falls back to release tarballs, then shallow clones.

```
ds    git    -    -    -    cgraf78/ds.git    .local/share/ds
```

Override the repo URL with `SHDEPS_<NAME>_REPO` env vars.

### `binary` — GitHub Release Binaries

Downloads the latest release binary from GitHub, matching the current OS and architecture. Handles tarballs, zips, compressed singles (.gz, .bz2, .zst), and raw binaries.

```
neovim    binary    nvim    -    -    neovim/neovim
shfmt     binary    -       -    -    mvdan/sh
```

### `custom` — Hook-Only

No built-in install logic. Entirely managed by a post-install hook file.

```
nerd-fonts    custom
```

## Hooks

Place hook files in `<hooks_dir>/<name>.sh`. Each file can define:

- **`post()`** — runs after a dep is installed/updated/changed. Return 0 to mark as complete.
- **`status()`** — runs every time for read-only status reporting.

See [examples/hooks.d/example-hook.sh](examples/hooks.d/example-hook.sh).

## CLI Usage

```
Usage: shdeps [options] <command> [args]

Commands:
  update          Install/update all dependencies
  list            List all configured dependencies with status
  check <name>    Check if a specific dependency is installed
  version         Print shdeps version
  help            Show this help message

Options:
  -c, --config <path>   Path to deps.conf (default: ~/.config/shdeps/deps.conf)
  -f, --force           Force reinstall all dependencies
  -q, --quiet           Suppress interactive prompts
  -v, --verbose         Verbose output
```

## As a Library

```bash
export SHDEPS_CONF="$HOME/.config/myapp/deps.conf"
source /path/to/shdeps.sh
shdeps_update
```

Available public functions:

- `shdeps_update` — install/update all dependencies
- `shdeps_load` — parse config and return dep count
- `shdeps_version` — print version string

## Testing

```bash
./test/shdeps-test
```

Requires bash 4.0+ (for associative arrays).

## License

[MIT](LICENSE)
