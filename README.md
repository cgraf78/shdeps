# shdeps

![Tests](https://github.com/cgraf78/shdeps/actions/workflows/test.yml/badge.svg?branch=main)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Rust](https://img.shields.io/badge/rust-%3E%3D1.74-orange.svg)](https://www.rust-lang.org/)
[![Bash API](https://img.shields.io/badge/bash%20API-%3E%3D4.3-blue.svg)](https://www.gnu.org/software/bash/)
[![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20macOS%20%7C%20WSL-lightgrey.svg)](#)

Declare your shell tools in one config file. shdeps installs and updates them everywhere — brew, apt, dnf, pacman, zypper, apk, GitHub repos, or GitHub release binaries. One manifest, any machine.

<p align="center">
  <img src="demo/demo.gif" alt="shdeps demo">
</p>

## Features

- **Declarative config** — one line per dependency in `*.conf` files
- **Multiple install methods** — system packages (brew/apt/dnf/pacman/zypper/apk), automatic GitHub release/repo selection, explicit GitHub repos, explicit GitHub release binaries, Rust crates (`cargo install`), Go modules (`go install`), Python CLI tools (`uv tool install`), Node.js CLI tools (`npm install -g`), or fully custom hooks
- **Cross-platform** — Linux, macOS, WSL with `os:` and `host:` filtering per dep
- **Package manager abstraction** — batched installs with individual retry fallback
- **Smart binary matching** — multi-pass asset selection by OS, arch, and libc
- **TTL-based caching** — avoids redundant network calls
- **Post-install hooks** — run arbitrary setup when a dependency changes
- **Config composition** — split deps across multiple `*.conf` files in a config directory
- **Usable from CLI, Bash, or Rust** — Rust `shdeps` CLI, stable Bash functions via `source shdeps.sh`, and a documented Rust crate API

## Quick Start

```bash
curl -fsSL https://raw.githubusercontent.com/cgraf78/shdeps/main/install.sh | bash
```

This installs the latest release archive to `~/.local/share/shdeps` and
symlinks the Rust CLI into `~/.local/bin/shdeps`. If a release archive is not
available, the installer falls back to a source checkout build so first-time
installs keep working while release assets are being bootstrapped. Re-running
the installer is idempotent. Developer/source-checkout installs are still
supported when you run `install.sh` from a git checkout or set `SHDEPS_REPO`
explicitly.

Then create a config and run:

```bash
mkdir -p ~/.config/shdeps
cat > ~/.config/shdeps/deps.conf << 'EOF'
jq    pkg
fzf   pkg
EOF

shdeps update
```

The CLI loads all `*.conf` files from `~/.config/shdeps/` (sorted alphabetically). Split deps across multiple files for organization (e.g., `00-core.conf`, `50-tools.conf`, `99-local.conf`). The Bash API (`source shdeps.sh`) defaults to `./shdeps/`.

### Updating shdeps

```bash
shdeps self-update
```

Uses TTL-based caching to avoid redundant work. Release installs update from
the latest matching archive, while source checkouts use `git pull --ff-only`
and skip updates when the working tree has uncommitted changes (active
development). Use `--force` to bypass the TTL cache.

### Uninstalling

```bash
curl -fsSL https://raw.githubusercontent.com/cgraf78/shdeps/main/install.sh | bash -s -- --uninstall
```

Or manually: `rm -rf ~/.local/share/shdeps ~/.local/bin/shdeps`.

## Configuration

### Config File Format

```text
# name    method    [cmd]  [aliases]  [filter]
```

| Field     | Required | Description                                                                                                                                 |
| --------- | -------- | ------------------------------------------------------------------------------------------------------------------------------------------- |
| `name`    | yes      | Dependency name (used for hooks, logging, tracking). For `github`, `github:repo`, and `github:release`: GitHub `owner/repo`. For `go`: full module path. |
| `method`  | yes      | Install method: `pkg`, `github`, `github:repo`, `github:release`, `cargo`, `go`, `uv`, `npm`, or `custom`                                             |
| `cmd`     | no       | Command to check for existence (defaults to name). Supports `mgr:name` qualifiers for platform-specific command names (e.g., `apt:batcat`). |
| `aliases` | no       | For `pkg`: per-manager package name overrides (`apt:fd-find,dnf:fd-find`). Use `NONE` to skip a specific manager (e.g., `brew:NONE`).       |
| `filter`  | no       | Platform and hostname filter. Use `os:` and `host:` prefixes (e.g., `os:linux`, `host:nas`, `os:linux,host:nas`, `os:!wsl`).                |

Use `-` for fields you want to skip. See [examples/deps.conf](examples/deps.conf) for a full example.

### Environment Variables

| Variable             | Default                                                 | Description                                                                                                                                                                           |
| -------------------- | ------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `SHDEPS_CONF_DIR`    | `~/.config/shdeps/` (CLI) or `./shdeps/` (Bash API)     | Config directory (all `*.conf` files loaded)                                                                                                                                          |
| `SHDEPS_HOOKS_DIR`   | `<conf_dir>/hooks.d`                                    | Post-install hooks directory                                                                                                                                                          |
| `SHDEPS_STATE_DIR`   | `${XDG_STATE_HOME:-$HOME/.local/state}/shdeps`          | Cache/state directory                                                                                                                                                                 |
| `SHDEPS_FORCE`       | `0`                                                     | Bypass TTL cache (check for updates now)                                                                                                                                              |
| `SHDEPS_REINSTALL`   | `0`                                                     | Force reinstall all deps                                                                                                                                                              |
| `SHDEPS_QUIET`       | `0`                                                     | Suppress interactive prompts                                                                                                                                                          |
| `SHDEPS_REMOTE_TTL`  | `3600`                                                  | Cache TTL in seconds                                                                                                                                                                  |
| `SHDEPS_GIT_DEV_DIR` | `~/git`                                                 | Dev clone directory used only by `github:repo` deps (prefers `<dir>/<repo>` over a managed clone)                                                                                     |
| `SHDEPS_INSTALL_DIR` | `~/.local/share`                                        | Base directory for shdeps-owned install roots (`github:repo`, archive-style `github:release`, `cargo`, `go`, `uv`, `npm`). Raw release binaries install into `SHDEPS_BIN_DIR` instead. |
| `SHDEPS_BIN_DIR`     | `~/.local/bin`                                          | Directory for binary symlinks and raw `github:release` binaries                                                                                                                       |
| `SHDEPS_LOG_LEVEL`   | `1`                                                     | 0=quiet, 1=normal, 2=verbose                                                                                                                                                          |
| `SHDEPS_JOBS`        | auto (`nproc`)                                          | Max concurrent read-only probes. Explicit values win; `1` = sequential.                                                                                                               |

## Install Methods

### `pkg` — System Packages

Installs via the detected package manager (brew, apt, dnf, pacman, zypper, or apk). Packages are batched into a single install command for speed.

```text
jq        pkg
bat       pkg    apt:batcat
fd        pkg    apt:fdfind       apt:fd-find,dnf:fd-find
dust      pkg    -                -                        os:macos
htop      pkg    -                -                        host:nas
```

Use `aliases` to map names across package managers. Use `NONE` to skip a dep on a specific manager (e.g., `brew:NONE`). Use `filter` with `os:` and `host:` prefixes to limit deps to specific platforms or machines.

### `github` — Automatic GitHub Install

Resolves to an existing concrete GitHub method before install, status, prune,
and method-transition logic runs. shdeps prefers `github:release` when the
latest stable release has a compatible asset for the current host and requested
command. If no compatible release asset is available, it falls back to
`github:repo`.

```text
cgraf78/ds    github
```

The manifest still records only the resolved concrete method
(`github:release` or `github:repo`), and `shdeps list` shows the same resolved
method. This keeps installed-state cleanup compatible with explicit methods
and avoids turning bare `github` into a third artifact ownership model.

Local development clones in `$SHDEPS_GIT_DEV_DIR/<repo>` are considered only
when `github` resolves to `github:repo`. A compatible release asset wins over a
local clone; use explicit `github:repo` when live-checkout behavior is required.

### `github:repo` — GitHub Repos

Clones a GitHub repo into `$SHDEPS_INSTALL_DIR/<owner>/<repo>` (default `~/.local/share/<owner>/<repo>`). Prefers local dev clones in `$SHDEPS_GIT_DEV_DIR/<repo>` (default `~/git/<repo>`, symlinked for live development). Falls back to a shallow clone for fresh installs. Every executable directly under the repo's `bin/` directory is symlinked into `$SHDEPS_BIN_DIR`; existing regular files in `$SHDEPS_BIN_DIR` are preserved so shdeps never overwrites a user-owned command.

Local clone discovery is intentionally limited to `github:repo`. Other methods,
including `github:release`, ignore `$SHDEPS_GIT_DEV_DIR` so changing a dep's
method cleanly changes ownership and install behavior instead of accidentally
continuing to use a live checkout.

```text
cgraf78/ds    github:repo
```

The `owner/repo` is the `name` field. Override the repo URL with `SHDEPS_<NAME>_REPO` env vars.

### `github:release` — GitHub Release Binaries

Downloads the latest release binary from GitHub, matching the current OS and architecture. Handles tarballs, zips, compressed singles (.gz, .bz2, .zst), and raw binaries. Archive-style releases install into a shdeps-owned root under `$SHDEPS_INSTALL_DIR/<owner>/<repo>` so bundled assets can be linked. Raw and compressed single-binary releases install directly to `$SHDEPS_BIN_DIR/<cmd>`, preserving the historical Bash behavior for this method.

```text
neovim/neovim    github:release    nvim
mvdan/sh         github:release
```

The `owner/repo` is the `name` field.

### `cargo` — Rust Crates

Installs crates from crates.io via `cargo install --root "$SHDEPS_INSTALL_DIR/<name>" <name>`. Binaries land in a per-dep install directory and are symlinked into `$SHDEPS_BIN_DIR`.

```text
ripgrep   cargo    rg
tokei     cargo
```

The `name` field is the crate name. Override `cmd` when the crate installs a binary with a different name (e.g., the `ripgrep` crate installs `rg`). `--reinstall` passes `--force` to cargo.

Requires `cargo` on `$PATH`. If absent, shdeps warns once at startup and skips all cargo deps.

### `go` — Go Modules

Installs Go binaries via `GOBIN=... go install <module>@latest`. Supports any module host (github.com, golang.org, k8s.io, private registries).

```text
github.com/junegunn/fzf             go
golang.org/x/tools/cmd/goimports    go
github.com/charmbracelet/gum        go    gum
```

The `name` field is the full Go module path (including any `cmd/...` subpath). `cmd` defaults to the basename of the module path.

Requires `go` on `$PATH`. If absent, shdeps warns once at startup and skips all go deps.

### `uv` — Python CLI Tools

Installs Python CLI tools from PyPI via `uv tool install`. Each dep gets its own isolated venv under `$SHDEPS_INSTALL_DIR/<name>/tools/` with the binary at `$SHDEPS_INSTALL_DIR/<name>/bin/<cmd>`, symlinked into `$SHDEPS_BIN_DIR`.

```text
ruff      uv
black     uv
mypy      uv
poetry    uv
```

The `name` field is the PyPI package name. Override `cmd` when the package's executable name differs from the package name. `--reinstall` passes `--force` to `uv tool install`.

Requires `uv` on `$PATH` (install via `pipx install uv`, `brew install uv`, or [Astral's installer](https://docs.astral.sh/uv/getting-started/installation/)). If absent, shdeps warns once at startup and skips all uv deps.

### `custom` — Hook-Only

No built-in install logic. Entirely managed by a post-install hook file.

```text
nerd-fonts    custom
```

## Hooks

Place hook files in `<hooks_dir>/<name>.sh`. For methods whose `name` contains path segments (`github:*`, `go`), hooks nest accordingly — e.g., `hooks.d/owner/repo.sh` or `hooks.d/github.com/owner/repo.sh`. For `custom` deps, hooks define the full install lifecycle. For other methods, hooks provide optional post-install setup.

**Custom dep hooks:**

- **`exists(name)`** — return 0 if installed (or not applicable), 1 if missing. Required for `custom` deps.
- **`version(name)`** — print version string to stdout. Optional.
- **`install(name)`** — perform the install unconditionally. shdeps only calls this when `exists()` returns 1 or `--reinstall` is used.
- **`uninstall(name)`** — reverse what `install()` or `post()` created. Optional. Called by `shdeps prune` when removing an orphaned dep (any method). For custom deps, this is the only cleanup. For other methods, runs before the built-in cleanup.
- **`post(name)`** — optional post-install setup.

**Non-custom dep hooks** (`pkg`, `github`, `github:repo`, `github:release`, `cargo`, `go`, `uv`, `npm`):

- **`post(name)`** — runs after shdeps installs/updates the dep (symlinking, config, etc.).

All [public API functions](#public-api) are available to hook authors. See [examples/hooks.d/example-hook.sh](examples/hooks.d/example-hook.sh).

## Man Pages & Completions

shdeps automatically discovers man pages and shell completions bundled inside
`github:repo` installs and archive-style `github:release` installs, then
symlinks them into standard XDG user-local directories. Tools like neovim, gum,
ripgrep, fd, bat, and hyperfine ship these files but they're not discoverable
without this linking. Raw single-binary releases and `cargo`, `go`, `uv`, and
`npm` deps do not provide bundled extras through shdeps — generate completions
from the tool itself in a `post()` hook (see
[examples/hooks.d/example-hook.sh](examples/hooks.d/example-hook.sh)).

**What gets linked:**

| Type             | Target directory                              | Auto-discovered by shell? |
| ---------------- | --------------------------------------------- | ------------------------- |
| Man pages        | `~/.local/share/man/man<N>/`                  | No                        |
| Bash completions | `~/.local/share/bash-completion/completions/` | Yes                       |
| Zsh completions  | `~/.local/share/zsh/site-functions/`          | No                        |
| Fish completions | `~/.local/share/fish/vendor_completions.d/`   | Yes                       |

**Required shell config** (bash and fish need nothing):

```bash
# Man pages — add to your shell env
export MANPATH="$HOME/.local/share/man:$MANPATH"

# Zsh completions — add before compinit
fpath=("$HOME/.local/share/zsh/site-functions" $fpath)
```

Symlinks are tracked per-dep in `$SHDEPS_STATE_DIR/<name>.links`. Running `shdeps prune` removes symlinks along with the dep. Updates clean stale symlinks before re-linking.

## CLI Usage

```text
Usage: shdeps [options] <command> [args]

Commands:
  update                 Install/update all dependencies
  self-update            Update shdeps itself (git pull, skips dirty trees)
  list                   List all configured dependencies with status
  check <name>           Check if a specific dependency is installed
  dep-root <name>        Print a configured dependency root directory
  dep-path <name> <rel>  Print a path below a configured dependency root
  dep-file <name> <rel>  Print a readable regular file below a dependency root
  prune                  Remove orphaned deps no longer in config
  version                Print shdeps version
  help                   Show this help message

Options:
  -c, --config <path>   Config directory or file (default: ~/.config/shdeps/)
  -f, --force           Bypass TTL cache (check for updates now)
  -R, --reinstall       Force reinstall all dependencies (implies --force)
  -q, --quiet           Suppress interactive prompts
  -v, --verbose         Verbose output (log level 2)

Prune options:
  -y                    Skip confirmation prompt
  --dry-run             Show what would be removed without removing

Examples:
  shdeps update
  shdeps -c ~/.config/myapp/ update
  shdeps --force update
  shdeps list
  shdeps check jq
  shdeps prune --dry-run
  shdeps prune -y

Exit codes:
  0  Success
  1  Error
  2  Usage error
```

### Removing Dependencies

When you remove a dep from your config, `shdeps update` will notify you that it's orphaned. Run `shdeps prune` to clean up the artifacts:

```bash
# Remove a dep from config, then update
shdeps update
# ==> 1 orphaned dep(s) (removed from config but still installed):
#   neovim/neovim (github:release)
#   Run: shdeps prune

shdeps prune           # interactive confirmation
shdeps prune -y        # skip confirmation
shdeps prune --dry-run # preview without removing
```

For `pkg` deps, prune warns that manual removal is needed (system packages may be shared). For `custom` deps, prune calls the optional `uninstall()` hook function.

## Bash API

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
- **Runs `self-update`** (skips dirty working trees)
- **Is idempotent** — safe to call multiple times
- **Does not leak `set -e`** into the caller's shell

## Public API

The sourceable Bash API defines the public `shdeps_` functions and delegates
behavior to the Rust binary through a small hidden bridge. This is the complete
shell contract available to callers and hook authors, and it is intended to
remain stable because Bash hook code depends on it. A Rust crate API also
exists; this Bash section documents the shell-facing contract specifically.

| Function                          | Description                                                                                 |
| --------------------------------- | ------------------------------------------------------------------------------------------- |
| `shdeps_update`                   | Install/update all dependencies                                                             |
| `shdeps_self_update [dir]`        | Update shdeps itself (git pull, skips dirty trees)                                          |
| `shdeps_prune [-y] [--dry-run]`   | Remove orphaned deps no longer in config                                                    |
| `shdeps_load`                     | Parse config and return dep count                                                           |
| `shdeps_version`                  | Print version string                                                                        |
| `shdeps_filter_match <spec>`      | Check if current platform/host matches a filter spec (e.g., `os:linux,host:nas`, `os:!wsl`) |
| `shdeps_platform_match <spec>`    | Check if current platform matches a spec (e.g., `linux,macos`, `!wsl`)                      |
| `shdeps_host_match <spec>`        | Check if current hostname matches a spec (e.g., `nas,taylor`, `!workstation`)               |
| `shdeps_platform`                 | Print normalized platform name (`linux`, `macos`, `wsl`)                                    |
| `shdeps_force`                    | Return 0 if force mode is active (TTL bypass)                                               |
| `shdeps_reinstall`                | Return 0 if reinstall mode is active                                                        |
| `shdeps_pkg_mgr`                  | Print detected package manager (`brew`, `apt`, `dnf`, `pacman`, `zypper`, `apk`, or empty)  |
| `shdeps_pkg_install <package>`    | Install one package immediately via the detected package manager                            |
| `shdeps_pkg_install_for_mgr <mgr:package>...` | Install the package spec matching the detected package manager                    |
| `shdeps_install_dir`              | Print base install directory (`$SHDEPS_INSTALL_DIR`, default `~/.local/share`)              |
| `shdeps_git_dev_dir`              | Print git dev clone directory (`$SHDEPS_GIT_DEV_DIR`, default `~/git`)                      |
| `shdeps_bin_dir`                  | Print binary symlink directory (`$SHDEPS_BIN_DIR`, default `~/.local/bin`)                  |
| `shdeps_dep_root <name>`          | Print a configured dependency root when shdeps owns one                                     |
| `shdeps_dep_path <name> <rel>`    | Print a path below a dependency root; rejects absolute and parent-traversal paths           |
| `shdeps_dep_file <name> <rel>`    | Print a dependency asset path only when it is a readable regular file                       |
| `shdeps_dep_source <name> <rel>`  | Source an existing dependency asset into the current Bash process                           |
| `shdeps_link_extras <name> <dir>` | Discover and symlink man pages and completions from an install dir                          |
| `shdeps_unlink_extras <name>`     | Remove all extras symlinks tracked for a dep                                                |
| `shdeps_require_sudo`             | Acquire sudo; returns 0 if root or sudo obtained                                            |
| `shdeps_log`                      | Normal log line                                                                             |
| `shdeps_warn`                     | Warning (always shown unless quiet)                                                         |
| `shdeps_log_ok`                   | Success highlight                                                                           |
| `shdeps_log_dim`                  | Dimmed / low-importance line                                                                |
| `shdeps_log_header`               | Section header                                                                              |

`shdeps_dep_root` follows the same ownership rules as installation. For
`github:repo`, it prefers `$SHDEPS_GIT_DEV_DIR/<repo>` when a local development
clone exists, then falls back to `$SHDEPS_INSTALL_DIR/<owner>/<repo>`. For
methods with per-dependency install directories (`cargo`, `go`, `uv`, `npm`,
and archive-style `github:release` installs), it returns
`$SHDEPS_INSTALL_DIR/<name>` when present. Raw single-binary `github:release`
installs and package-manager deps do not have shdeps-owned roots.

## Rust API

shdeps builds a normal Rust library crate (`shdeps`) alongside the CLI. The
binary, tests, and future Rust consumers call into documented modules such as
`shdeps::config`, `shdeps::runtime`, `shdeps::status`, `shdeps::update`, and
`shdeps::prune`.

The Rust API is intentionally documented in code because it owns the real
implementation boundaries. Its stability promise is narrower than the CLI and
Bash API while the Rust port is still landing: downstream Rust callers can use
the crate, but module boundaries may still tighten before the first public crate
release. shdeps does not currently promise a stable C ABI or `libshdeps.a`
static archive.

## Testing

```bash
./test/shdeps-test
SHDEPS_IMPL=rust ./test/shdeps-test
cargo test --locked
./test/shdeps-wrapper-test
```

The standalone CLI is a Rust binary. The sourceable Bash API and hook prelude
require Bash 4.3+; `install.sh` itself is kept compatible with the stock macOS
Bash 3.2 installer path.

## License

[MIT](LICENSE)
