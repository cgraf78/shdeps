# shdeps

![Tests](https://github.com/cgraf78/shdeps/actions/workflows/test.yml/badge.svg?branch=main)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Bash Version](https://img.shields.io/badge/bash-%3E%3D4.0-blue.svg)](https://www.gnu.org/software/bash/)
[![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20macOS%20%7C%20WSL-lightgrey.svg)](#)

A standalone shell dependency manager. Declare your tools in config files, and shdeps installs them via system package managers, GitHub git repos, or GitHub release binaries.

## Features

- **Declarative config** — one line per dependency in `*.conf` files
- **Multiple install methods** — system packages (brew/apt/dnf/pacman), git clones, GitHub release binaries, or fully custom hooks
- **Cross-platform** — Linux, macOS, WSL with platform (`linux`, `macos`, `wsl`) and hostname filtering per dep
- **Package manager abstraction** — batched installs with individual retry fallback
- **Smart binary matching** — multi-pass asset selection by OS, arch, and libc
- **TTL-based caching** — avoids redundant network calls
- **Post-install hooks** — run arbitrary setup when a dependency changes
- **Config composition** — split deps across multiple `*.conf` files in a config directory
- **Usable as CLI or library** — `bin/shdeps` CLI or `source shdeps.sh`

## Quick Start

```bash
curl -fsSL https://raw.githubusercontent.com/cgraf78/shdeps/main/install.sh | bash
```

This clones shdeps to `~/.local/share/shdeps` and symlinks the CLI into `~/.local/bin/shdeps`. Re-running the installer updates to the latest version.

Then create a config and run:

```bash
mkdir -p ~/.config/shdeps
cat > ~/.config/shdeps/deps.conf << 'EOF'
jq    pkg
fzf   pkg
EOF

shdeps update
```

The CLI loads all `*.conf` files from `~/.config/shdeps/` (sorted alphabetically). Split deps across multiple files for organization (e.g., `00-core.conf`, `50-tools.conf`, `99-local.conf`). The library (`source shdeps.sh`) defaults to `./shdeps/`.

### Updating shdeps

```bash
shdeps self-update
```

Uses TTL-based caching to avoid redundant pulls. Skips updates if the working tree has uncommitted changes (active development). Use `--force` to bypass the TTL cache.

### Uninstalling

```bash
curl -fsSL https://raw.githubusercontent.com/cgraf78/shdeps/main/install.sh | bash -s -- --uninstall
```

Or manually: `rm -rf ~/.local/share/shdeps ~/.local/bin/shdeps`.

## Configuration

### Config File Format

```
# name    method    [cmd]  [cmd_alt]  [pkg_overrides]  [repo]  [platforms]  [hosts]
```

| Field | Required | Description |
|---|---|---|
| `name` | yes | Dependency name (used for hooks, logging, tracking) |
| `method` | yes | Install method: `pkg`, `git`, `binary`, or `custom` |
| `cmd` | no | Binary to check for existence (defaults to name) |
| `cmd_alt` | no | Alternate binary name (e.g., `batcat` for `bat`) |
| `pkg_overrides` | no | Per-manager package names: `apt:fd-find,dnf:fd-find` |
| `repo` | no | GitHub `owner/repo` (for `git` and `binary` methods) |
| `platforms` | no | Platform filter. Values: `linux`, `macos`, `wsl`. Prefix `!` to exclude. |
| `hosts` | no | Hostname filter (matches `hostname -s`, case-insensitive). Prefix `!` to exclude. |

Use `-` for fields you want to skip. See [examples/deps.conf](examples/deps.conf) for a full example.

### Environment Variables

| Variable | Default | Description |
|---|---|---|
| `SHDEPS_CONF_DIR` | `~/.config/shdeps/` (CLI) or `./shdeps/` (library) | Config directory (all `*.conf` files loaded) |
| `SHDEPS_HOOKS_DIR` | `<conf_dir>/hooks.d` | Post-install hooks directory |
| `SHDEPS_STATE_DIR` | `$XDG_STATE_HOME/shdeps` | Cache/state directory |
| `SHDEPS_FORCE` | `0` | Bypass TTL cache (check for updates now) |
| `SHDEPS_REINSTALL` | `0` | Force reinstall all deps |
| `SHDEPS_QUIET` | `0` | Suppress interactive prompts |
| `SHDEPS_REMOTE_TTL` | `3600` | Cache TTL in seconds |
| `SHDEPS_GIT_DEV_DIR` | `~/git` | Dev clone directory for the `git` method |
| `SHDEPS_INSTALL_DIR` | `~/.local/share` | Base directory for `git` and `binary` installs |
| `SHDEPS_BIN_DIR` | `~/.local/bin` | Directory for binary symlinks |
| `SHDEPS_LOG_LEVEL` | `1` | 0=quiet, 1=normal, 2=verbose |

## Install Methods

### `pkg` — System Packages

Installs via the detected package manager (brew, apt, dnf, or pacman). Packages are batched into a single install command for speed.

```
jq        pkg
bat       pkg    bat    batcat
fd        pkg    fd     fdfind    apt:fd-find,dnf:fd-find
dust      pkg    -      -         -              -    macos
htop      pkg    -      -         -              -    -    nas
```

Use `pkg_overrides` to map names across package managers. Use `NONE` to skip a dep on a specific manager (e.g., `brew:NONE`). Use `hosts` to limit a dep to specific machines.

### `git` — GitHub Git Repos

Clones a GitHub repo into `$SHDEPS_INSTALL_DIR/<name>` (default `~/.local/share/<name>`). Prefers local dev clones in `$SHDEPS_GIT_DEV_DIR/<name>` (default `~/git/<name>`, symlinked for live development). Falls back to release tarballs, then shallow clones.

```
ds    git    -    -    -    cgraf78/ds.git
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

Place hook files in `<hooks_dir>/<name>.sh`. For `custom` deps, hooks define the full install lifecycle. For other methods, hooks provide optional post-install setup.

**Custom dep hooks:**

- **`exists(name)`** — return 0 if installed (or not applicable), 1 if missing. Required for `custom` deps.
- **`version(name)`** — print version string to stdout. Optional.
- **`install(name)`** — perform the install unconditionally. shdeps only calls this when `exists()` returns 1 or `--reinstall` is used.
- **`uninstall(name)`** — reverse what `install()` or `post()` created. Optional. Called by `shdeps prune` when removing an orphaned dep (any method). For custom deps, this is the only cleanup. For other methods, runs before the built-in cleanup.
- **`post(name)`** — optional post-install setup.

**Non-custom dep hooks** (`pkg`, `git`, `binary`):

- **`post(name)`** — runs after shdeps installs/updates the dep (symlinking, config, etc.).

All [public API functions](#public-api) are available to hook authors. See [examples/hooks.d/example-hook.sh](examples/hooks.d/example-hook.sh).

## CLI Usage

```
Usage: shdeps [options] <command> [args]

Commands:
  update          Install/update all dependencies
  self-update     Update shdeps itself (git pull with TTL caching)
  list            List all configured dependencies with status
  check <name>    Check if a specific dependency is installed
  prune           Remove orphaned deps no longer in config
  version         Print shdeps version
  help            Show this help message

Options:
  -c, --config <path>   Config directory or file (default: ~/.config/shdeps/)
  -f, --force           Bypass TTL cache (check for updates now)
  -R, --reinstall       Force reinstall all dependencies (implies --force)
  -q, --quiet           Suppress interactive prompts
  -v, --verbose         Verbose output

Prune options:
  -y                    Skip confirmation prompt
  --dry-run             Show what would be removed without removing
```

### Removing Dependencies

When you remove a dep from your config, `shdeps update` will notify you that it's orphaned. Run `shdeps prune` to clean up the artifacts:

```bash
# Remove a dep from config, then update
shdeps update
# ==> 1 orphaned dep(s) (removed from config but still installed):
#   neovim (binary)
#   Run: shdeps prune

shdeps prune           # interactive confirmation
shdeps prune -y        # skip confirmation
shdeps prune --dry-run # preview without removing
```

For `pkg` deps, prune warns that manual removal is needed (system packages may be shared). For `custom` deps, prune calls the optional `uninstall()` hook function.

## As a Library

```bash
export SHDEPS_CONF_DIR="$HOME/.config/myapp"
source /path/to/shdeps.sh
shdeps_update
```

## Bootstrapping (Client Integration)

For projects that embed shdeps (e.g., dotfiles managers), `install.sh --bootstrap` provides a single sourceable entry point that handles discovery, sourcing, CLI symlink, and self-update:

```bash
# Set your project's config before bootstrapping
export SHDEPS_CONF_DIR="$HOME/.config/myapp"
export SHDEPS_HOOKS_DIR="$HOME/.config/myapp/hooks.d"

# Source shdeps — finds it automatically, installs if missing
. ~/git/shdeps/install.sh --bootstrap ||
  . ~/.local/share/shdeps/install.sh --bootstrap ||
  { echo "shdeps not found"; return 1; }

# shdeps_update is now available
shdeps_update
```

The `--bootstrap` flag:

- **Finds shdeps.sh** via `$SHDEPS_LIB` → `$SHDEPS_GIT_DEV_DIR/shdeps/` → `$SHDEPS_DIR/` → fresh install
- **Sources it** into the caller (all `shdeps_*` functions become available)
- **Symlinks the CLI** into `$SHDEPS_BIN` (default `~/.local/bin/shdeps`)
- **Runs `self-update`** with TTL caching (skips dirty working trees)
- **Is idempotent** — safe to call multiple times
- **Does not leak `set -e`** into the caller's shell

## Public API

All `shdeps_` functions are defined in a single section at the top of `shdeps.sh`. This is the complete public contract — available to callers, library users, and hook authors.

| Function | Description |
|---|---|
| `shdeps_update` | Install/update all dependencies |
| `shdeps_self_update [dir]` | Update shdeps itself (TTL-cached git pull, skips dirty trees) |
| `shdeps_prune [-y] [--dry-run]` | Remove orphaned deps no longer in config |
| `shdeps_load` | Parse config and return dep count |
| `shdeps_version` | Print version string |
| `shdeps_platform_match <spec>` | Check if current platform matches a spec (e.g., `linux,macos`, `!wsl`) |
| `shdeps_host_match <spec>` | Check if current hostname matches a spec (e.g., `nas,taylor`, `!workstation`) |
| `shdeps_platform` | Print normalized platform name (`linux`, `macos`, `wsl`) |
| `shdeps_force` | Return 0 if force mode is active (TTL bypass) |
| `shdeps_reinstall` | Return 0 if reinstall mode is active |
| `shdeps_pkg_mgr` | Print detected package manager (`brew`, `apt`, `dnf`, `pacman`, or empty) |
| `shdeps_install_dir` | Print base install directory (`$SHDEPS_INSTALL_DIR`, default `~/.local/share`) |
| `shdeps_git_dev_dir` | Print git dev clone directory (`$SHDEPS_GIT_DEV_DIR`, default `~/git`) |
| `shdeps_bin_dir` | Print binary symlink directory (`$SHDEPS_BIN_DIR`, default `~/.local/bin`) |
| `shdeps_require_sudo` | Acquire sudo; returns 0 if root or sudo obtained |
| `shdeps_log` | Normal log line |
| `shdeps_warn` | Warning (always shown unless quiet) |
| `shdeps_log_ok` | Success highlight |
| `shdeps_log_dim` | Dimmed / low-importance line |
| `shdeps_log_header` | Section header |

## Testing

```bash
./test/shdeps-test
```

Requires bash 4.0+ (for associative arrays).

## License

[MIT](LICENSE)
