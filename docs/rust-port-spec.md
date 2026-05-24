# shdeps Compatibility Specification

This document defines the public behavior contract for `shdeps`. It is the
normative target for the Rust port and for future implementations. The current
Bash implementation is the reference implementation until the Rust
implementation satisfies this specification and the parity test suite.

This is a compatibility specification, not an implementation plan. The
implementation roadmap lives in [rust-port-plan.md](rust-port-plan.md).

## Status

Status: draft.

The spec should be treated as binding for new Rust code once a section has
tests. If this document and the current Bash implementation disagree before the
Rust port is complete, treat the Bash implementation and existing tests as the
source of truth, then update this spec or the tests intentionally.

## Implementation Workflow Contract

This spec defines behavior, not a release branch policy, but the Rust port is
large enough that implementation workflow is part of compatibility risk
control. The port MUST be implemented through granular, progressive local
commits that each preserve a coherent repository state and carry their relevant
tests.

Local commit expectations:

- Commit each logical slice independently: skeleton, parser/domain types,
  state/manifest, package cache, one install method at a time, hook runner,
  Bash wrapper, installer migration, and release packaging.
- Keep behavior changes close to their tests. A commit that changes public CLI,
  Bash API, state format, hook behavior, installer behavior, or performance
  contract MUST include or update the matching test coverage in the same local
  commit.
- Prefer local amend/fixup/rebase for unpushed corrections so the final history
  remains readable.
- Do not push Rust port implementation commits to the remote GitHub repository
  unless the user explicitly requests a push. Local commits are allowed and
  expected; remote publication is a separate decision.

## Terminology

- **Dependency**: one configured tool or asset line in a `*.conf` file.
- **Dependency name**: the first field in a config entry. It is also the primary
  manifest key and hook path key.
- **Method**: install method field such as `pkg`, `github:repo`, or `custom`.
- **Command**: executable name used to check whether a dependency is installed.
- **Install root**: directory owned or selected by shdeps for a dependency.
- **State dir**: `$SHDEPS_STATE_DIR`.
- **Config dir**: `$SHDEPS_CONF_DIR`.
- **Hooks dir**: `$SHDEPS_HOOKS_DIR`.
- **Cheap command**: command that must avoid expensive setup. This includes
  `help`, `version`, `dep-root`, `dep-path`, and `dep-file`.
- **Manifest-backed dep**: a dep whose installed executable path is resolved from
  the manifest, including `github:release`, `cargo`, `go`, `uv`, and `npm`.

Normative words:

- **MUST** means required for compatibility.
- **SHOULD** means required unless there is a documented reason not to.
- **MAY** means permitted behavior.

## Compatibility Levels

shdeps has four API levels:

1. **Stable user API**: CLI commands, config format, environment variables,
   state files, hook ABI, Bash `shdeps_*` functions, and release artifacts.
2. **Stable Rust API**: public Rust crate types and functions documented by
   rustdoc. These names are not prefixed with `shdeps_`.
3. **Compatibility bridge API**: hidden CLI commands under `shdeps __api`.
   These are stable enough for `shdeps.sh` and hook preludes, but are not user
   CLI.
4. **Private implementation**: internal Rust modules, private Bash helper names,
   private globals, worker coordination files, and test-only hooks. These are
   not compatibility contract unless promoted explicitly.

The Rust port MUST preserve Stable user API. It SHOULD keep Compatibility
bridge API minimal and documented near its callers.

## CLI Contract

### Global Syntax

```text
shdeps [options] <command> [args]
```

Global options:

- `-c, --config <path>`: config directory or config file. If a file is provided,
  its parent directory is used.
- `-f, --force`: set force mode and bypass TTL caches.
- `-R, --reinstall`: set force and reinstall mode.
- `-q, --quiet`: suppress interactive prompts and normal logs.
- `-v, --verbose`: set verbose log level.
- `-h, --help`: print help and exit successfully.

Option precedence:

1. Explicit CLI flags override inherited environment where applicable.
2. If `-c/--config` is absent and `SHDEPS_CONF_DIR` is unset, the CLI defaults
   to `${XDG_CONFIG_HOME:-$HOME/.config}/shdeps`.
3. Source/library mode defaults differ; see [Bash API](#bash-api).

Current Bash decides whether `-c/--config <path>` is a directory by checking
whether the path exists as a directory at parse time. A nonexistent path is
therefore treated like a config file path and its parent directory is used. The
Rust port SHOULD preserve this for compatibility, even though it is slightly
surprising.

Unknown global options MUST print an error to stderr and exit with code `2`.
Unknown commands MUST print an error to stderr and exit with code `2`.

### Exit Codes

General exit codes:

- `0`: success.
- `1`: runtime error, not installed, failed install, unknown dependency, or
  intentionally aborted guarded operation.
- `2`: usage error.

Commands MAY return narrower meanings documented below, but MUST preserve these
classes.

### `help`

`shdeps help` and `shdeps --help` MUST print usage text to stdout and exit `0`.

`help` is a cheap command. It MUST NOT load config, detect package managers,
source hooks, or touch the network.

### `version`

`shdeps version` MUST print:

```text
shdeps commit <short-git-commit>
```

It MUST NOT print `unknown`. If a concrete commit cannot be resolved at build or
runtime, build or startup MUST fail clearly before producing a misleading
version.

`version` is a cheap command. It MUST NOT load config, detect package managers,
source hooks, or touch the network.

### `update`

`shdeps update` installs or upgrades configured dependencies.

Behavior:

- MUST require `git` and return `1` when `git` is unavailable.
- MUST ensure `$SHDEPS_BIN_DIR` is present in `PATH` during the update process.
- MUST load all active config entries.
- MUST detect the package manager.
- MUST prepare method transitions before installing new method artifacts.
- MUST process package-manager deps before non-package deps.
- MUST run non-package deps with bounded concurrency when `SHDEPS_JOBS > 1`.
- When the parallel job runner is active, MUST run `custom` deps sequentially
  after parallel non-custom Phase B deps. With `SHDEPS_JOBS=1`, current Bash
  behavior processes all non-`pkg` deps sequentially in loaded dependency order,
  so a `custom` dep can run before a later-sorting non-custom dep.
- MUST run `post(name)` hooks for changed deps after install phases.
- MUST return `1` if any required dep install fails, while still attempting
  later independent deps.
- Package-manager deps that are unavailable in the active package repositories
  currently warn and skip without making the whole update fail. This skip
  behavior is part of compatibility unless a future spec revision deliberately
  tightens package policy.
- MUST detect and report orphans after successful install phases.
- MUST NOT prune orphaned deps during `update`.

Output:

- Normal output is human-oriented.
- Important error/warning lines MUST go to stderr or a warning path that remains
  visible in quiet/captured modes.
- Verbose output SHOULD explain cache, filter, hook, install-root, and GitHub
  asset decisions.

### `self-update`

`shdeps self-update` updates the shdeps installation.

Current source-checkout behavior:

- If the install dir contains `.git`, it MUST skip dirty trees.
- If the tree is clean, it MUST use `git pull --ff-only`.
- If the pull fails, it MUST warn and leave the current install usable.
- It MUST re-link shdeps extras after the update attempt.

Rust/release behavior:

- If install metadata says the install came from a release archive,
  `self-update` MUST download the matching platform archive, verify checksum,
  smoke-test or validate staged files, and atomically replace archive-owned
  files.
- If install metadata is missing or unknown and no `.git` checkout exists,
  `self-update` MUST fail clearly and suggest rerunning `install.sh`.
- Failed `self-update` MUST leave the previous binary and wrapper usable.

Release-selection rules for `self-update` (release-archive mode):

- MUST skip prereleases by default (releases marked `prerelease: true` in
  the GitHub API). An explicit opt-in flag (TBD; not initially exposed) MAY
  be added later for testing pre-release artifacts.
- MUST skip drafts (releases marked `draft: true`).
- MUST refuse downgrade: if the latest selectable release's tag sorts
  earlier than the currently-installed tag recorded in metadata,
  `self-update` exits cleanly with a "no update available" message rather
  than re-installing the older version.
- MUST treat checksum failure, smoke-test failure, or extraction failure on
  the candidate archive as a bad-artifact condition: keep the previous
  install in place, log the failure with the rejected tag, and exit
  non-zero. The bad tag SHOULD be remembered (e.g., a small skip-list in
  state) so a subsequent automatic `self-update` does not loop on the same
  broken artifact.
- MUST handle the no-supported-artifact case (current platform has no
  matching asset label) with a clear error; do not fall back to a nearby
  platform.

### `list`

`shdeps list` lists configured dependencies with status.

Behavior:

- MUST load config, detect package manager, and read manifest.
- If no deps are configured, MUST print `No dependencies configured.` and exit
  `0`.
- MUST print columns for `NAME`, `METHOD`, `STATUS`, and `DETAILS`.
- MUST report platform-filtered deps as `skipped (platform)`.
- MUST report host-filtered deps as `skipped (host)`.
- MUST report package-manager `NONE` deps as `skipped (pkg manager)`.
- MUST report installed deps as `installed`.
- MUST report missing deps as `missing`.

Status detection:

- `pkg`: command or package-manager query.
- `github:repo`: install dir under `$SHDEPS_INSTALL_DIR/<name>`.
- `github:release`, `cargo`, `go`, `uv`, `npm`: executable path from manifest.
- `custom`: hook `exists(name)` returns success.

### `check <name>`

`shdeps check <name>` checks one configured dependency.

Exit codes:

- `0`: dependency is installed or skipped by filter/package-manager `NONE`.
- `1`: dependency is configured but not installed, or dependency is unknown.
- `2`: missing required `<name>` argument.

Output examples:

```text
name: installed
name: installed (1.2.3)
name: skipped (platform mismatch)
name: skipped (host mismatch)
name: skipped (pkg manager)
name: not installed
```

Unknown dependency errors MUST go to stderr.

Performance rule:

- `check` MUST load config and classify the target dependency before doing any
  expensive install-method setup.
- Manifest-backed methods (`github:release`, `cargo`, `go`, `uv`, and `npm`)
  SHOULD check manifest/path state directly and MUST NOT detect the package
  manager, scan package databases, source hooks, query GitHub, or touch the
  network.
- `pkg` deps MAY detect the package manager because manager aliases and `NONE`
  are part of their status contract.
- `custom` deps MAY source only the target hook because `exists(name)` is the
  custom status contract; unrelated hooks must not be sourced.

### `dep-root <name>`

`shdeps dep-root <name>` prints the root directory for a configured dependency
when shdeps can identify one.

Exit codes:

- `0`: root found and printed.
- `1`: configured dependency has no shdeps-owned root, is inactive by filter,
  is missing, or is unknown.
- `2`: missing or invalid name.

Root rules:

- `github:repo`: prefer `$SHDEPS_GIT_DEV_DIR/<short-name>` when it exists, then
  `$SHDEPS_INSTALL_DIR/<owner>/<repo>`.
- `github:release`, `cargo`, `go`, `uv`, `npm`: use
  `$SHDEPS_INSTALL_DIR/<name>` when it exists.
- `pkg` and `custom`: no built-in root.

`dep-root` is a cheap command. It MUST NOT detect package managers, run hooks,
query GitHub, or touch the network.

### `dep-path <name> <rel>`

`shdeps dep-path <name> <rel>` prints `<root>/<rel>` for a configured dependency.
The final path need not exist.

Exit codes:

- `0`: path printed.
- `1`: no root or inactive/missing/unknown dependency.
- `2`: missing arguments, invalid name, or invalid relative path.

`rel` MUST be non-empty, relative, and MUST NOT contain parent traversal.

Relative path validation is lexical. Current behavior prevents explicit
absolute paths and `..` traversal in the requested `rel`, but it does not
promise to sandbox symlink traversal inside a dependency root. Callers should
only request assets from dependencies they trust.

`dep-path` is a cheap command with the same performance restrictions as
`dep-root`.

### `dep-file <name> <rel>`

`shdeps dep-file <name> <rel>` prints a dependency asset path only when the path
is a readable regular file.

Exit codes:

- `0`: readable regular file found and printed.
- `1`: no root, inactive/missing/unknown dependency, missing file, unreadable
  file, or path is not a regular file.
- `2`: missing arguments, invalid name, or invalid relative path.

`dep-file` is a cheap command with the same performance restrictions as
`dep-root`.

### `prune`

`shdeps prune` removes orphaned dependencies: manifest entries whose names are
not present in the current config set.

Options:

- `-y`: skip confirmation prompt.
- `--dry-run`: list what would be removed but do not remove anything.

Exit codes:

- `0`: success, no orphans, dry run success, user aborted prompt, or quiet mode
  skipped prompt/action.
- `1`: guarded all-orphans condition or cleanup runtime error.
- `2`: unknown prune option.

Safety:

- If config has zero deps and manifest has entries, prune MUST require `-y`
  before treating every manifest entry as orphaned.
- `--dry-run` MUST NOT remove files or change manifest.
- Quiet mode without `-y` MUST skip prompt and action.
- `pkg` deps MUST NOT uninstall system packages.

### `migrate` (removed from user-facing CLI)

The historical `shdeps migrate [--dry-run]` command is removed from the
user-facing CLI in the Rust port. Current parsing canonicalizes GitHub names
at load/parse time, so the historical migration is moot for new fleet
machines. See Δ15.

If any latent state-rewrite need emerges, expose it under
`shdeps __api migrate` rather than re-adding to the public surface. The
command MUST remain harmless and idempotent if kept under `__api`.

`shdeps help` MUST NOT mention `migrate`. `shdeps migrate` invoked directly
on the user-facing CLI MUST return a usage error (exit code 2) with a
message pointing at the removal.

## Environment Contract

Runtime environment variables:

| Variable | Default | Contract |
| --- | --- | --- |
| `SHDEPS_CONF_DIR` | CLI: `${XDG_CONFIG_HOME:-$HOME/.config}/shdeps`; sourced: `./shdeps` | Config directory. The CLI `-c/--config` flag accepts a file and exports its parent directory, but the environment variable itself is interpreted as a directory by the Bash library. |
| `SHDEPS_HOOKS_DIR` | `<conf_dir>/hooks.d` | Hook root |
| `SHDEPS_STATE_DIR` | `${XDG_STATE_HOME:-$HOME/.local/state}/shdeps` | State root |
| `SHDEPS_FORCE` | `0` | Bypass remote and package caches |
| `SHDEPS_REINSTALL` | `0` | Force reinstall behavior |
| `SHDEPS_QUIET` | `0` | Suppress normal logs and prompts |
| `SHDEPS_REMOTE_TTL` | `3600` | Remote cache TTL seconds |
| `SHDEPS_GIT_DEV_DIR` | `$HOME/git` | Local dev clone root |
| `SHDEPS_INSTALL_DIR` | `$HOME/.local/share` | Dependency install root |
| `SHDEPS_BIN_DIR` | `$HOME/.local/bin` | Public command symlink dir |
| `SHDEPS_LOG_LEVEL` | `1` | `0` quiet, `1` normal, `2` verbose |
| `SHDEPS_JOBS` | auto, max `8` | Max concurrent non-custom Phase B jobs |
| `SHDEPS_AUTO_EPEL` | `0` unless caller sets | dnf CRB/EPEL automation gate |
| `GH_TOKEN` | unset | Preferred GitHub auth token for runtime GitHub API and asset calls |
| `GITHUB_TOKEN` | unset | GitHub auth token fallback for runtime GitHub API and asset calls |

Dynamic repo override:

- `SHDEPS_<NAME>_REPO` MAY override a `github:repo` clone URL.
- Current Bash derives `<NAME>` from the dependency short name, uppercases it,
  and replaces `-` with `_`. For example, `cgraf78/my-tool` reads
  `SHDEPS_MY_TOOL_REPO`.
- Current Bash does not derive the override from `owner/repo`, and repo short
  names containing characters that are invalid in shell variable names are an
  edge case. The Rust port MUST preserve the working cases above and SHOULD
  handle invalid computed variable names by ignoring the override rather than
  failing during expansion.

Installer/bootstrap environment variables:

| Variable | Default | Contract |
| --- | --- | --- |
| `SHDEPS_DIR` | `$HOME/.local/share/shdeps` | shdeps install dir |
| `SHDEPS_REPO` | `https://github.com/cgraf78/shdeps.git` currently; dotfiles may prefer SSH | source repo URL |
| `SHDEPS_BIN` | `$HOME/.local/bin/shdeps` | CLI symlink path |
| `SHDEPS_LIB` | unset | direct `shdeps.sh` path for bootstrap |
| `SHDEPS_GIT_DEV_DIR` | `$HOME/git` | dev clone discovery |
| `GH_TOKEN` | unset | release-asset auth candidate for the Rust-era installer |
| `GITHUB_TOKEN` | unset | release-asset auth fallback for the Rust-era installer |

Internal `_SHDEPS_*` variables are private. Tests SHOULD avoid depending on
private globals once public Rust/API parity exists.

Shell runtime requirement (two tiers):

- **Tier 1 — `install.sh` and its sourceable bootstrap MUST work on Bash
  `3.2`.** This is required because `install.sh` runs BEFORE anything is
  installed, and stock macOS ships Bash 3.2. An installer that depends on
  Bash 4.3 cannot bootstrap a fresh macOS machine. `install.sh` therefore
  MUST NOT use associative arrays, `${var,,}` lowercasing, `mapfile`/
  `readarray`, `${BASH_REMATCH[*]}` with global modifiers, or other Bash
  4.0+ features. CI MUST include a Bash 3.2 smoke test for `install.sh`
  (Docker fixture or `/bin/bash --posix` style).
- **Tier 2 — `shdeps.sh` wrapper, hook files, and Rust hook preludes MUST
  support Bash `4.3` or newer.** On source, the wrapper MUST detect
  `BASH_VERSINFO[0]*100+BASH_VERSINFO[1] < 403`, print a clear remediation
  message (`shdeps requires Bash 4.3+. macOS ships Bash 3.2; install with
  brew install bash and re-source.`), and return non-zero without
  registering any function. Silent failures deep inside `declare -A` are
  not acceptable user experience.
- The Rust binary itself does not require Bash for non-hook CLI commands, but
  the distributed compatibility wrapper and sourceable bootstrap do. Release
  archives therefore MUST keep a Bash-compatible wrapper until the Bash API is
  intentionally retired.
- CI MUST include a wrapper/prelude smoke test under the minimum supported
  Bash version, because macOS and fleet bootstrap paths are where old Bash
  assumptions usually surface.

## Config File Grammar

Config files are loaded from `$SHDEPS_CONF_DIR`.

Rules:

- If the config dir is missing, config load succeeds with zero deps and emits a
  warning in update/prune contexts.
- Only files matching `*.conf` directly under the config dir are loaded.
- Config files are sorted with bytewise/C-locale order.
- Lines beginning with optional whitespace then `#` are comments.
- Blank lines are ignored.
- Non-comment lines are split by shell-like whitespace today. The Rust port MUST
  preserve observable whitespace-separated field behavior.
- At least `name` and `method` are required for a valid, actionable entry.
  Current Bash can retain malformed lines with fewer fields as inert entries
  that do not match any install method. Rust compatibility tests should cover
  the valid grammar; stricter diagnostics for malformed entries must not break
  valid existing configs.
- Loaded entries are sorted by dependency name case-insensitively using current
  Bash behavior as the reference.

Fields:

```text
name method [cmd] [aliases] [filter]
```

Field meanings:

- `name`: dependency identity, manifest key, hook path.
- `method`: install method.
- `cmd`: command name, optional. `-` means default.
- `aliases`: package alias overrides, optional. `-` means none.
- `filter`: `os:` and `host:` filter tokens, optional. `-` means none.

Valid install methods are:

- `pkg`
- `github:repo`
- `github:release`
- `cargo`
- `go`
- `uv`
- `npm`
- `custom`

Unknown methods are not part of the stable config contract. Current Bash often
skips unknown methods incidentally because no install branch matches. The Rust
port SHOULD diagnose unknown methods clearly, but must not change behavior for
valid existing configs.

Default command:

- If `cmd` is absent or `-`, command defaults to the dependency short name.
- Short name is the suffix after the final `/`.

GitHub canonicalization:

- For `github:repo` and `github:release`, names shaped as `owner/repo.git`
  MUST canonicalize to `owner/repo`.
- Non-GitHub methods MUST NOT strip `.git`.

Invalid dependency names for path APIs:

- empty
- absolute paths
- `..` or paths containing parent traversal
- names containing whitespace
- names containing `|`

## Platform And Host Matching

Platform names:

- Darwin normalizes to `macos`.
- WSL normalizes to `wsl`.
- Other `uname -s` values lowercase, with Linux normally `linux`.

Platform specs:

- Empty spec matches.
- Include-only list matches only listed platform.
- Exclude-only list matches unless current platform is excluded.
- Mixed include/exclude list checks excludes first, then includes.

Host specs:

- Empty spec matches.
- Host matching is case-insensitive.
- Include/exclude/mixed behavior mirrors platform matching.

Filter field:

- Comma-separated tokens.
- `os:<spec>` contributes to platform spec.
- `host:<spec>` contributes to host spec.
- Unknown token prefixes are ignored by current behavior and SHOULD remain
  ignored unless a future spec revision defines an error.
- Return/status classes:
  - match
  - platform mismatch
  - host mismatch

## State Directory Layout

All state lives under `$SHDEPS_STATE_DIR` unless otherwise specified.

State paths for dependency names containing `/` MUST preserve nested path
behavior. For example, `cgraf78/ds` uses files under
`$SHDEPS_STATE_DIR/cgraf78/`.

State writes SHOULD be transactional:

- write to a temp file in the same directory
- flush as appropriate for important metadata
- rename atomically
- never intentionally leave partially written state as valid

The Rust implementation MUST use a per-state-dir advisory lock before becoming
the default implementation. The Bash reference does not have a broad lock, but
the Rust port is taking ownership of concurrent install/update/prune safety, and
state corruption is a worse compatibility failure than a small amount of lock
coordination.

Locking rules:

- Hold the lock around state read-modify-write windows for manifest, cache,
  stamp, `.links`, `.binlinks`, install metadata, prune, and method-transition
  cleanup.
- Do not hold the broad state lock across slow network downloads, package
  manager installs, language tool installs, or arbitrary hook execution.
- Re-read and revalidate the affected state after reacquiring the lock and
  before committing a mutation that depended on earlier observations.
- Hook subprocesses MUST NOT inherit a locked parent critical section. Side
  effects from hooks are applied after Rust validates coordination records and
  enters its own short state mutation window.
- Lock acquisition failures MUST surface as structured state errors with action
  and path context. They must not silently fall back to unlocked mutation.

## Manifest Format

Path:

```text
$SHDEPS_STATE_DIR/manifest
```

Line format:

```text
name|method|cmd|install_path
```

Rules:

- Encoding is UTF-8-compatible plain text.
- One entry per line.
- Empty lines are ignored.
- `name` is the manifest key.
- Later duplicate names currently override earlier names in memory, but upsert
  MUST normalize duplicates back to one row.
- `install_path` MAY be empty.
- Legacy relative install paths are interpreted relative to `$HOME` during some
  cleanup paths and MUST remain supported until a migration removes them.

Compatibility:

- Rust MUST read Bash-written manifests.
- During transition, Bash MUST read Rust-written manifests.
- No migration should be required for existing fleet state.

## Package Check Cache Format

Path:

```text
$SHDEPS_STATE_DIR/pkg-check-cache-v3
```

Purpose: avoid expensive package-manager checks on warm no-op updates.

The cache MUST NOT be a blind timestamp. It must prove that the previous clean
package pass is still valid.

Validity inputs:

- cache version
- active package-manager identity
- current platform
- current host
- config dir fingerprint
- config file content fingerprints
- manifest fingerprint
- package database fingerprints
- command path fingerprints
- package hook content fingerprints
- force/reinstall/log-level gates
- dynamic env override fingerprint: hash of all currently-set
  `SHDEPS_<NAME>_REPO` variables and `SHDEPS_AUTO_EPEL`. Changing a
  per-dep repo override or the EPEL gate MUST invalidate the cache so the
  affected deps re-evaluate.

If any required proof field is missing or stale, the cache MUST miss.

Validation cost rules:

- The cache validator MUST stat tracked paths first and only content-hash on
  mtime+size mismatch. Hashing every fingerprint on every warm `update` would
  make the cache its own cost and put the 500 ms warm-update budget at risk.
- On stat match: cache valid; skip hashing.
- On stat mismatch: content-hash the path; if hash matches, refresh the cached
  mtime/size and continue (avoids cache miss on touch-only changes such as
  `git pull` rewriting unchanged files).
- On hash mismatch: cache invalid; record which input invalidated for verbose
  diagnostics.

## Stamp And Link State

Remote TTL stamps:

```text
$SHDEPS_STATE_DIR/<name>.*.stamp
```

Revision stamps:

```text
$SHDEPS_STATE_DIR/<name>.rev
```

Extras links:

```text
$SHDEPS_STATE_DIR/<name>.links
```

Bin links:

```text
$SHDEPS_STATE_DIR/<name>.binlinks
```

Rules:

- `.links` and `.binlinks` track symlinks created by shdeps.
- Relinking MUST remove stale tracked symlinks before writing new link state.
- Prune and method-transition cleanup MUST remove tracked links for owned deps.
- Missing link-state files are treated as empty state.

## Dependency Identity And Install Roots

Short name:

- The short name is the path suffix after the final `/`.
- Examples: `cgraf78/ds` -> `ds`, `ripgrep` -> `ripgrep`.

Install roots:

| Method | Root |
| --- | --- |
| `pkg` | none |
| `github:repo` | `$SHDEPS_INSTALL_DIR/<owner>/<repo>` or symlink to `$SHDEPS_GIT_DEV_DIR/<repo>` |
| `github:release` | `$SHDEPS_INSTALL_DIR/<owner>/<repo>` for extracted assets; manifest install path may be public binary |
| `cargo` | `$SHDEPS_INSTALL_DIR/<crate>` |
| `go` | `$SHDEPS_INSTALL_DIR/<module>` |
| `uv` | `$SHDEPS_INSTALL_DIR/<package>` |
| `npm` | `$SHDEPS_INSTALL_DIR/<package>` |
| `custom` | none by default |

`github:repo` root discovery MUST prefer local dev clones in
`$SHDEPS_GIT_DEV_DIR/<short-name>` for path APIs.

## Install Method Contracts

### `pkg`

Package manager detection order:

1. macOS with `brew`: `brew`
2. `apt-get`: `apt`
3. `dnf`: `dnf`
4. `pacman`: `pacman`
5. `zypper`: `zypper`
6. `apk`: `apk`
7. none

Behavior:

- Aliases are comma-separated `mgr:name` pairs.
- Matching alias for active manager wins.
- Alias value `NONE` skips the dependency for that manager.
- Installed package existence is checked by command lookup first, then package
  manager query when needed.
- Package installs are batched where possible.
- Batch failure falls back to individual installs.
- Package deps write manifest rows with empty install path.
- Prune MUST NOT uninstall package-manager packages.

### `github:repo`

Behavior:

- Dependency name is GitHub `owner/repo`.
- `.git` suffix spelling is canonicalized.
- Local dev clone `$SHDEPS_GIT_DEV_DIR/<repo>` is preferred and symlinked.
- Existing clones are pulled/updated according to current behavior.
- Fresh clones use shallow clone behavior.
- Private repo clone failures over HTTPS should fall back to normal GitHub SSH
  clone where possible.
- Every executable directly under the repo `bin/` dir is linked into
  `$SHDEPS_BIN_DIR`.
- Existing regular files in `$SHDEPS_BIN_DIR` MUST be preserved.
- Bin links are tracked.
- Extras are discovered and linked.

### `github:release`

Behavior:

- Dependency name is GitHub `owner/repo`.
- `.git` suffix spelling is canonicalized.
- Latest release JSON is fetched from GitHub API unless cached/prefetched.
- Runtime GitHub credential precedence is `GH_TOKEN`, then `GITHUB_TOKEN`, then
  `gh auth token`.
- `GH_TOKEN` support is a compatibility-safe expansion over the Bash reference.
  It lets CI and fleet bootstrap provide an explicit token without depending on
  the host's `gh` login state, while preserving public unauthenticated release
  behavior when no token is available.
- Current Bash sends the auth token to GitHub release API JSON requests, but
  direct asset downloads and fallback `HEAD` probes do not currently attach
  that token. Authenticated asset download support is a compatibility-safe
  improvement for private repos and rate-limit handling, but tests must keep
  public unauthenticated asset behavior intact.
- Asset matching is multi-pass and MUST preserve current preference behavior:
  standalone binary, then tar archives, then zip archives.
- Matching considers OS, architecture, libc on Linux, command name, and common
  naming conventions.
- Metadata assets such as checksums/signatures are skipped as install assets.
- Archives supported by current behavior include tar gzip/xz/bzip2/zstd, tgz,
  tzst, zip, compressed singles, and raw binaries.
- Current Bash delegates archive extraction to `tar`/`unzip` and does not
  explicitly reject unsafe archive paths before extraction. The Rust port MUST
  add safe extraction as an intentional security hardening, while preserving
  behavior for normal safe archives.
- Current Bash writes or symlinks the selected release binary directly to the
  requested public `bin_path`, so a pre-existing non-symlink file at that exact
  path can be replaced. This differs from `_shdeps_link_bin` used by other
  methods, which preserves regular files. Any Rust hardening here must be
  deliberate and covered by migration/compatibility tests.
- Extras are discovered and linked from extracted installs.

### `cargo`

Behavior:

- Runs `cargo install --locked --root "$SHDEPS_INSTALL_DIR/<name>" <name>`.
- `--reinstall` passes force behavior to cargo.
- Public binary path is `$SHDEPS_INSTALL_DIR/<name>/bin/<cmd>` symlinked into
  `$SHDEPS_BIN_DIR/<cmd>`.
- Missing `cargo` warns and skips affected deps without aborting unrelated deps.

### `go`

Behavior:

- Runs `GOBIN="$SHDEPS_INSTALL_DIR/<name>/bin" go install <name>@latest`.
- `cmd` defaults to basename of module path.
- Missing `go` warns and skips affected deps without aborting unrelated deps.

### `uv`

Behavior:

- Runs `uv tool install <name>` with:
  - `UV_TOOL_DIR=$SHDEPS_INSTALL_DIR/<name>/tools`
  - `UV_TOOL_BIN_DIR=$SHDEPS_INSTALL_DIR/<name>/bin`
- `--reinstall` passes force behavior to `uv`.
- Missing `uv` warns and skips affected deps without aborting unrelated deps.

### `npm`

Behavior:

- Runs `npm install -g --prefix "$SHDEPS_INSTALL_DIR/<name>" <name>`.
- `--reinstall` passes force behavior where current helper supports it.
- Missing `npm` warns and skips affected deps without aborting unrelated deps.

### `custom`

Behavior:

- No built-in install root.
- Hook file is required for useful behavior.
- Missing hook file skips silently during update.
- Missing `exists()` warns and skips.
- If `exists(name)` succeeds and reinstall is not active, install is skipped and
  manifest is updated.
- If `exists(name)` fails or reinstall is active, `install(name)` runs when
  defined.
- If `install(name)` is absent after `exists(name)` reports missing, current
  Bash skips without writing a manifest entry and returns success for that dep.
- Successful `install(name)` marks dep changed and writes manifest.
- Failed `install(name)` warns and causes update to return failure after other
  independent deps are attempted.

## Ownership Policy

shdeps MUST centralize ownership decisions. This table is normative:

| Artifact | Owned by shdeps? | Removal rule |
| --- | --- | --- |
| manifest row | yes | remove on prune/method transition |
| TTL/rev stamps | yes | remove on prune/method transition |
| symlink recorded in `.binlinks` | yes | remove on relink/prune/transition |
| symlink recorded in `.links` | yes | remove on relink/prune/transition |
| regular file in `$SHDEPS_BIN_DIR` not created as symlink | no for link helpers; current `github:release` is an exception | preserve for `_shdeps_link_bin`/repo/external method linking; current `github:release` may replace the requested `bin_path` |
| `github:repo` install symlink to local dev clone | symlink yes, target no | remove symlink only |
| local dev clone under `$SHDEPS_GIT_DEV_DIR` | no | never remove |
| install dir under `$SHDEPS_INSTALL_DIR/<name>` | yes for non-local managed methods | remove on prune/transition |
| package-manager package | no | never uninstall |
| hook-created artifact | unknown | hook `uninstall()` owns cleanup |
| shdeps release-installed binary/wrapper/extras | yes | self-update/uninstall may replace/remove |

When ownership is ambiguous, shdeps SHOULD preserve files and warn rather than
delete. The current `github:release` public binary write path is the main known
exception, and any Rust-era behavior change there needs an explicit
compatibility test because a caller may currently rely on reinstall replacing a
stale binary at `$SHDEPS_BIN_DIR/<cmd>`.

## Method Transitions

If a configured dependency's method differs from its manifest method:

- It is not an orphan.
- shdeps MUST stage the new method install before mutating manifest or
  bin-links: download, extract, run external installers, and verify the new
  artifact is present and runnable in a staging location.
- Once staging succeeds, shdeps MUST atomically swap manifest row and bin
  symlink to the new method.
- After the swap, shdeps MUST clean old non-`pkg` managed artifacts (old
  install root, old `.links`/`.binlinks` entries, old TTL/rev stamps).
- If cleanup fails after a successful swap, the system MUST remain usable:
  the new method works, and the leftover artifacts are reported as
  orphaned-leftover via the verbose log. A subsequent `shdeps update` or
  `shdeps prune` MUST re-attempt cleanup. Cleanup failure MUST NOT roll back
  the swap.
- `pkg` transition MUST clear tracking but MUST NOT uninstall system packages.
- Hook `uninstall(name)` MAY run as part of post-swap cleanup when present.
  It runs in the same isolated-subprocess regime as other hook phases.

This order avoids the "remove-then-install" failure mode where a failed new
install leaves the dependency entirely absent. Add a fixture test that simulates
cleanup failure mid-transition (e.g., chmod the old install root read-only after
swap) and asserts: (1) `shdeps check <name>` reports installed against the new
method, (2) `shdeps update` re-attempts cleanup on next run, (3) no manifest
corruption.

## Orphans And Prune

A manifest entry is orphaned when its dependency name is absent from the current
config set, regardless of platform or host filters.

Platform-filtered configured deps are not orphans.

Prune MUST list orphans before removal unless quiet behavior skips action.
Prune MUST remove manifest rows after cleanup attempts.

## Hook ABI

Hook path:

```text
$SHDEPS_HOOKS_DIR/<name>.sh
```

For dependency names containing `/`, hook paths are nested. Examples:

```text
hooks.d/cgraf78/ds.sh
hooks.d/github.com/junegunn/fzf.sh
```

Hook functions:

| Function | Required? | Meaning |
| --- | --- | --- |
| `exists(name)` | required for `custom` | returns success when custom dep is present |
| `version(name)` | optional | prints version string |
| `install(name)` | required for installable `custom` deps | performs custom install |
| `post(name)` | optional | runs after changed dep installs |
| `uninstall(name)` | optional | reverses hook-created artifacts during prune/transition |

Execution environment:

- Hooks receive the dependency name as `$1`.
- Hooks inherit the invoking environment, including `SHDEPS_CONF_DIR`,
  `SHDEPS_HOOKS_DIR`, `SHDEPS_STATE_DIR`, `SHDEPS_INSTALL_DIR`,
  `SHDEPS_BIN_DIR`, force/reinstall flags, and `PATH`.
- Current Bash sources hooks in the current shell and current working
  directory. Rust hook subprocesses SHOULD preserve the caller's working
  directory and environment for compatibility.
- Hooks may rely on filesystem/process side effects and public `shdeps_*`
  helper calls. They SHOULD NOT rely on accidental shell variable leakage
  between separate hook invocations; Rust subprocess isolation will not preserve
  that as shared mutable process state.

Isolation:

- Hook functions MUST NOT leak between dependencies.
- The implementation MUST unset or isolate `exists`, `version`, `install`,
  `post`, and `uninstall` between hook executions.
- Hook source failures MUST preserve current command-specific behavior:
  `update`, `post`, `prune`, and method-transition cleanup warn and continue;
  `list` and `check` currently suppress hook source errors and treat the custom
  dep as missing unless `exists(name)` is successfully loaded and returns
  success.
- `post(name)` failures do not currently fail update; this behavior MUST remain
  unless a future spec revision changes it.

Bash compatibility:

- Hook code MUST have access to the public Bash `shdeps_*` helpers.
- In Rust, hooks run in Bash subprocesses with a compatibility prelude.
- Hook coordination records read by Rust MUST be treated as untrusted input.

## Bash API

`shdeps.sh` is sourceable and MUST define a public API section. Every
top-level function named `shdeps_*` in that section is public Bash API. Helpers
with `_shdeps_*` names and `_SHDEPS_*` globals are private implementation
details unless explicitly promoted in a future spec revision.

The stable Bash API is:

| Function | Stdout | Return contract |
| --- | --- | --- |
| `shdeps_version` | `shdeps commit <hash>` | `0` when version resolved, non-zero if no concrete commit is available |
| `shdeps_update [args...]` | human logs | `0` on successful update, `1` on runtime/install failure |
| `shdeps_self_update [dir]` | human logs/warnings | non-zero when the target is not self-updatable; current Bash returns success for dirty-tree skips and non-destructive pull failures |
| `shdeps_load` | configured dependency count | `0`; populates Bash reference internals but callers should prefer the count/output contract |
| `shdeps_prune [-y] [--dry-run]` | human logs | same behavior as `shdeps prune` |
| `shdeps_platform_match <spec>` | none | predicate: `0` match, `1` mismatch |
| `shdeps_host_match <spec>` | none | predicate: `0` match, `1` mismatch |
| `shdeps_filter_match <spec>` | none | predicate: `0` match, `1` platform mismatch, `2` host mismatch |
| `shdeps_platform` | normalized platform string | `0` |
| `shdeps_force` | none | predicate: `0` when force mode active, `1` otherwise |
| `shdeps_reinstall` | none | predicate: `0` when reinstall mode active, `1` otherwise |
| `shdeps_pkg_mgr` | current detected package manager or empty string | `0`; MUST read the already-detected manager (Bash reference: `${_SHDEPS_PKG_MGR:-}`). MUST NOT trigger detection. Detection is owned by `update`/`list`/`check` paths; on-demand detection from this helper would fork the package-manager probe chain for every hook call, break perf budgets, and risk inconsistent answers if env mutates mid-update. Returns empty string if detection has not happened yet in the current process or its parent `__api` runtime context. |
| `shdeps_pkg_install <package>` | human logs | `0` only when the package install succeeds |
| `shdeps_pkg_install_for_mgr <mgr:package>...` | human logs | `0` when a spec for the active manager installs successfully |
| `shdeps_require_sudo` | sudo prompt/output as needed | `0` if root or sudo was obtained |
| `shdeps_install_dir` | install root path | `0` |
| `shdeps_git_dev_dir` | local dev clone root path | `0` |
| `shdeps_bin_dir` | public bin dir path | `0` |
| `shdeps_dep_root <name>` | dependency root path | same behavior as `shdeps dep-root` |
| `shdeps_dep_path <name> <rel>` | dependency-relative path | same behavior as `shdeps dep-path` |
| `shdeps_dep_file <name> <rel>` | readable regular file path | same behavior as `shdeps dep-file` |
| `shdeps_dep_source <name> <rel>` | sourced file output, if any | sources into the current Bash process; returns the sourced file status |
| `shdeps_link_extras <name> <dir>` | normally none | `0`; discovers and tracks extras symlinks |
| `shdeps_unlink_extras <name>` | normally none | `0`; removes tracked extras symlinks |
| `shdeps_github_release_install <name> <cmd> [owner/repo] [bin_path]` | human logs | `0` only when release install succeeds or is already current |
| `shdeps_log <message...>` | log line when enabled | `0` |
| `shdeps_warn <message...>` | warning line to stderr when logging is enabled | `0` |
| `shdeps_log_warn <message...>` | warning line to stderr when logging is enabled | `0` |
| `shdeps_log_ok <message...>` | success log line | `0` |
| `shdeps_log_dim <message...>` | low-importance log line when enabled | `0` |
| `shdeps_log_header <message...>` | header log line when enabled | `0` |
| `shdeps_mark_changed <name>` | none | `0`; records that `post(name)` should run in the current update context |

Source mode default config dir:

- When sourced directly as a library, config defaults to `./shdeps` unless env
  overrides it.
- The CLI wrapper defaults to XDG config dir.

API compatibility requirements:

- Predicate helpers MUST preserve shell return status semantics.
- Path helper stdout MUST remain clean enough for command substitution.
- `shdeps_dep_source` MUST source into the current shell. It cannot be replaced
  by a subprocess-only implementation.
- Hook authors may call this API. Rust hook preludes MUST provide equivalent
  `shdeps_*` functions for hooks that run in Bash subprocesses.
- Public Bash functions may delegate to the Rust binary or bridge API, but their
  stdout, stderr, return status, and source-time side effects must match the
  Bash reference for supported behavior.

### Bash API Bridge Mapping

The Rust port MUST keep one authoritative mapping from each public Bash helper
to its Rust/library implementation and command bridge. This table prevents the
compatibility wrapper from becoming a second implementation with drift-prone
logic.

| Bash API | Rust API or module owner | CLI or bridge surface |
| --- | --- | --- |
| `shdeps_version` | `version()` | `shdeps version` |
| `shdeps_update` | `update()` | `shdeps update` |
| `shdeps_self_update` | `self_update()` | `shdeps self-update` |
| `shdeps_load` | `load()` | `shdeps __api load-count` |
| `shdeps_prune` | `prune()` | `shdeps prune` |
| `shdeps_platform_match` | `platform_match()` | `shdeps __api platform-match <spec>` |
| `shdeps_host_match` | `host_match()` | `shdeps __api host-match <spec>` |
| `shdeps_filter_match` | `filter_match()` | `shdeps __api filter-match <spec>` |
| `shdeps_platform` | `platform()` | `shdeps __api platform` |
| `shdeps_force` | `RuntimeEnv::force` | `shdeps __api force` |
| `shdeps_reinstall` | `RuntimeEnv::reinstall` | `shdeps __api reinstall` |
| `shdeps_pkg_mgr` | cached `RuntimeEnv::pkg_mgr` (read-only) | `shdeps __api pkg-mgr` (reads cached value; does NOT trigger detection) |
| `shdeps_pkg_install` | `pkg_install()` | `shdeps __api pkg-install <package>` |
| `shdeps_pkg_install_for_mgr` | `pkg_install_for_mgr()` | `shdeps __api pkg-install-for-mgr <mgr:package>...` |
| `shdeps_require_sudo` | `pkg::require_sudo()` | `shdeps __api require-sudo` |
| `shdeps_install_dir` | `RuntimeEnv::install_dir` | `shdeps __api install-dir` |
| `shdeps_git_dev_dir` | `RuntimeEnv::git_dev_dir` | `shdeps __api git-dev-dir` |
| `shdeps_bin_dir` | `RuntimeEnv::bin_dir` | `shdeps __api bin-dir` |
| `shdeps_dep_root` | `dep_root()` | `shdeps dep-root` and `shdeps __api dep-root <name>` |
| `shdeps_dep_path` | `dep_path()` | `shdeps dep-path` and `shdeps __api dep-path <name> <rel>` |
| `shdeps_dep_file` | `dep_file()` | `shdeps dep-file` and `shdeps __api dep-file <name> <rel>` |
| `shdeps_dep_source` | wrapper-owned current-shell sourcing | local Bash wrapper only |
| `shdeps_link_extras` | `link_extras()` | `shdeps __api link-extras <name> <dir>` |
| `shdeps_unlink_extras` | `unlink_extras()` | `shdeps __api unlink-extras <name>` |
| `shdeps_github_release_install` | `github_release_install()` | `shdeps __api github-release-install <name> <cmd> [owner/repo] [bin_path]` |
| `shdeps_log` | `logging` | local Bash/prelude diagnostic helper or `shdeps __api log` |
| `shdeps_warn` | `logging` | local Bash/prelude diagnostic helper or `shdeps __api warn` |
| `shdeps_log_warn` | `logging` | local Bash/prelude diagnostic helper or `shdeps __api warn` |
| `shdeps_log_ok` | `logging` | local Bash/prelude diagnostic helper or `shdeps __api log-ok` |
| `shdeps_log_dim` | `logging` | local Bash/prelude diagnostic helper or `shdeps __api log-dim` |
| `shdeps_log_header` | `logging` | local Bash/prelude diagnostic helper or `shdeps __api log-header` |
| `shdeps_mark_changed` | `hooks` coordination state | hook prelude coordination record |

Bridge mapping rules:

- A Bash helper that returns a predicate status MUST delegate to a bridge
  command that exits with the predicate status directly. The wrapper must not
  parse stdout to recover boolean meaning.
- `shdeps_dep_source` is intentionally wrapper-owned because it must source the
  resolved file into the current shell. Rust may resolve the file path, but the
  final `.` operation must happen in the caller's Bash process.
- Logging helpers MAY stay local in the wrapper/prelude when they only format
  output from already-available env state. If Rust-owned log policy becomes more
  complex, add explicit bridge commands instead of duplicating formatting rules.
- `shdeps_mark_changed` in hooks MUST write a validated hook coordination
  record. It cannot be a normal subprocess command that tries to mutate parent
  Rust memory.
- Any bridge command added for this table is compatibility surface for the
  wrapper and hook prelude, even if it remains hidden from user help.

## Native Rust API

The native Rust API is the stable API for Rust callers of the `shdeps` crate.
It mirrors the same operations as the CLI and Bash API, but uses Rust domain
types and structured errors rather than shell strings and process statuses.

Native Rust API names SHOULD be idiomatic and unprefixed:

```rust
pub fn version() -> Result<Version>;

pub fn load(config: &Config) -> Result<Vec<Dependency>>;
pub fn update(config: &Config, opts: UpdateOptions) -> Result<UpdateSummary>;
pub fn self_update(config: &Config, dir: Option<&Path>) -> Result<SelfUpdateSummary>;
pub fn list(config: &Config) -> Result<Vec<DependencyStatus>>;
pub fn check(config: &Config, name: &DependencyName) -> Result<CheckStatus>;
pub fn prune(config: &Config, opts: PruneOptions) -> Result<PruneSummary>;

pub fn platform(env: &RuntimeEnv) -> Platform;
pub fn platform_match(spec: &PlatformSpec, platform: &Platform) -> bool;
pub fn host_match(spec: &HostSpec, host: &HostName) -> bool;
pub fn filter_match(filter: &Filter, env: &RuntimeEnv) -> FilterResult;

pub fn dep_root(config: &Config, name: &DependencyName) -> Result<PathBuf>;
pub fn dep_path(
    config: &Config,
    name: &DependencyName,
    rel: &RelativeAssetPath,
) -> Result<PathBuf>;
pub fn dep_file(
    config: &Config,
    name: &DependencyName,
    rel: &RelativeAssetPath,
) -> Result<PathBuf>;

pub fn pkg_install(config: &Config, package: &PackageName) -> Result<()>;
pub fn pkg_install_for_mgr(config: &Config, specs: &[PackageSpec]) -> Result<()>;
pub fn link_extras(config: &Config, name: &DependencyName, dir: &Path) -> Result<LinkSummary>;
pub fn unlink_extras(config: &Config, name: &DependencyName) -> Result<UnlinkSummary>;
pub fn github_release_install(
    config: &Config,
    name: &DependencyName,
    cmd: &CommandName,
    repo: Option<&GitHubRepo>,
    bin_path: Option<&Path>,
) -> Result<InstallSummary>;
```

The crate/module path provides namespacing. Do not use `shdeps_` prefixes in
native Rust APIs unless a name would otherwise be unclear.

Rust public APIs MUST use rustdoc and structured domain types where practical.
At minimum, public types should cover:

- `Config`, `RuntimeEnv`, `UpdateOptions`, `PruneOptions`
- `Dependency`, `DependencyName`, `InstallMethod`, `CommandName`
- `Platform`, `PlatformSpec`, `HostName`, `HostSpec`, `Filter`, `FilterResult`
- `PackageManager`, `PackageName`, `PackageSpec`
- `GitHubRepo`, `RelativeAssetPath`, `ManifestEntry`
- summary/status types for update, self-update, list/check, install, linking,
  and prune
- a structured error type that preserves enough context to format current CLI
  and Bash-wrapper errors

The Rust API MUST NOT expose private Bash globals, worker coordination files, or
implementation-specific cache structs as stable types.

## Hidden Bridge API

Hidden commands under `shdeps __api` exist to support `shdeps.sh` and hook
preludes.

They are not user-facing CLI, but they are compatibility bridge surface.

The bridge command registry MUST be defined before the Bash wrapper cutover.
The initial registry SHOULD include only commands needed by public Bash API
functions and hook preludes:

- `version` — prints the wrapper-binary ABI version (e.g. `abi:1`). The
  wrapper invokes this once on source to negotiate compatibility (see
  Δ6). MUST be cheap; MUST NOT load config, detect package managers,
  source hooks, or touch the network. MUST be backwards-compatible: a
  wrapper from an older shdeps version MUST get a parseable response from
  any future binary.
- `env-snapshot` — prints all wrapper-cacheable RuntimeEnv values
  (`install_dir`, `bin_dir`, `git_dev_dir`, `platform`, `pkg_mgr`,
  `force`, `reinstall`, `abi`) in a single subprocess call to amortize
  fork cost. Output is machine-clean `key=value` lines, one per line.
  The wrapper caches the result in shell vars and exports them so child
  shells skip the fetch. See Δ12. MUST NOT trigger package-manager
  detection (returns empty `pkg_mgr=` if not already detected, matching
  the `__api pkg-mgr` contract; see Δ13).
- `platform-match <spec>`
- `host-match <spec>`
- `filter-match <spec>`
- `platform`
- `force`
- `reinstall`
- `load-count`
- `pkg-mgr`
- `pkg-install <package>`
- `pkg-install-for-mgr <mgr:package>...`
- `require-sudo`
- `install-dir`
- `git-dev-dir`
- `bin-dir`
- `dep-root <name>`
- `dep-path <name> <rel>`
- `dep-file <name> <rel>`
- `link-extras <name> <dir>`
- `unlink-extras <name>`
- `github-release-install <name> <cmd> [owner/repo] [bin_path]`

Any bridge command that emits data for shell command substitution MUST keep
stdout machine-clean and send diagnostics to stderr.

Rules:

- Hidden commands MUST be documented near their Bash/prelude callers.
- Hidden commands SHOULD avoid output not required by their caller.
- Hidden commands SHOULD be tested through the wrapper behavior they support.
- Hidden commands MAY change only when the wrapper/prelude changes in the same
  release.

## Hook Coordination Protocol

Rust hook subprocesses cannot mutate the parent Rust process's in-memory
state directly, but they CAN mutate on-disk state through the same `__api`
bridge commands the wrapper uses. There is intentionally no versioned IPC
schema between hook and parent.

Design (deliberately simple):

- Every hook-callable `shdeps_*` helper in the prelude is a one-line shim
  that invokes `command shdeps __api <name> "$@"`. Anything that mutates
  state (`shdeps_link_extras`, `shdeps_unlink_extras`,
  `shdeps_github_release_install`, `shdeps_pkg_install`,
  `shdeps_pkg_install_for_mgr`) runs as a fresh `__api` subprocess that
  acquires the state lock for its own short read-modify-write window and
  releases before returning. The parent re-reads state after the hook
  subprocess exits; no record-apply step is needed.
- `shdeps_mark_changed <name>` is the only helper whose effect must surface
  back into the parent's in-process update transaction. It writes a sentinel
  file: `$SHDEPS_STATE_DIR/.changed-markers/<txn_id>/<name>`. The parent
  enumerates and unlinks markers in this directory after each hook exits,
  feeding the names into its post-hook scheduling. `<txn_id>` is a unique
  identifier the parent generates per `shdeps update` and exports to the
  hook subprocess via `SHDEPS_UPDATE_TXN_ID`.
- Logging helpers (`shdeps_log`, `shdeps_warn`, `shdeps_log_*`) write
  directly to stdout/stderr from the subprocess; the parent does not
  reformat. The wrapper one-liner discipline (see Code Quality requirements)
  keeps these consistent with parent output.

Hook update-transaction context propagation:

The parent MUST export the following environment variables to every hook
subprocess so the prelude and `__api` calls share context:

- `SHDEPS_UPDATE_TXN_ID`: opaque per-update identifier, used for the
  changed-markers directory.
- `SHDEPS_CURRENT_DEP`: the dependency name being processed (`$1` for the
  hook function is the same value; the env var lets `__api` calls pick it
  up without re-passing).
- `SHDEPS_HOOK_PHASE`: one of `exists`, `version`, `install`, `post`,
  `uninstall`. Used by `__api` for diagnostic context only; does not gate
  behavior.

State-lock invariant (critical):

The parent MUST NOT hold the state lock when forking a hook subprocess.
Hook subprocesses' `__api` calls acquire the lock fresh in their own short
windows. Holding the parent lock across a hook fork deadlocks the first
`__api` call. A regression test MUST cover this: a custom hook whose
`install()` calls `shdeps_link_extras` during `shdeps update` completes
without deadlock. The lock acquisition path MUST have a timeout (e.g., 30s)
that fails with a structured error rather than hanging indefinitely.

Why no versioned protocol:

A versioned JSON record protocol was considered and rejected in eng review.
The simpler delegation model removes the entire untrusted-input attack
surface (no record parser to harden, no schema-version negotiation, no
ad-hoc validation). It costs ~one extra fork per mutating helper call, which
is acceptable on hot paths because hooks already run in a subprocess.

## C ABI

No stable C ABI or `libshdeps.a` static archive is required for the initial Rust
port. The required reusable API is the Rust crate API.

A stable C ABI is not required until a real non-Rust consumer exists. If added,
it MUST define:

- prefixed exported symbols such as `shdeps_update`
- C header file
- ownership/freeing rules for returned memory
- error/status representation
- string encoding assumptions
- versioning policy

## Installer Contract

`install.sh` remains the stable install/bootstrap entry point.

### Invocation Mode Detection

`install.sh` MUST detect its invocation context and behave accordingly. This
is required because `install.sh` ships in three contexts: inside the release
archive, inside a git checkout, and piped from curl. Without detection, the
archive-extracted `install.sh` would helpfully re-download the same archive
from GitHub. See Δ-adjacent installer test fixtures.

Detection order:

1. **Bundled-archive mode.** If `$0`'s directory contains both a `shdeps`
   binary and a `.shdeps-install.json` file, treat the surrounding directory
   as a freshly-extracted release archive. Install from the local files; do
   NOT download.
2. **Source-checkout mode.** If `$0`'s directory contains a `.git`
   directory, treat it as a developer source checkout. Preserve current
   git-pull / dirty-tree-skip behavior; do NOT download a release archive.
3. **Curl-pipe mode.** Otherwise (typical `curl … | bash` path), download
   the latest release archive for the host platform, verify checksum,
   stage, swap, write metadata.

Each mode MUST have a dedicated fixture test in CI.

### SSH-clone fallback for private repos (preserved)

For machines that have SSH git auth but no `GH_TOKEN`/`GITHUB_TOKEN`/`gh`,
`install.sh` MUST preserve the existing SSH-clone-then-source-build fallback
path. Concretely, when release-archive download fails due to missing
credentials AND `git` + a working Rust toolchain are available AND the
configured `SHDEPS_REPO` URL is reachable over SSH, the installer SHOULD
clone via SSH and build from source rather than failing the bootstrap.
Fleet machines that previously bootstrapped via SSH clone MUST NOT regress.

### Reference Behavior

Bash reference executed install mode currently MUST:

- require `git`
- clone `$SHDEPS_REPO` into `$SHDEPS_DIR` when missing
- pull an existing clean git checkout with `git pull --ff-only --quiet`
- skip dirty git checkouts without failing
- fail when `$SHDEPS_DIR` exists but is not a git repo
- source `shdeps.sh` from the installed checkout
- symlink `$SHDEPS_BIN` to the checkout's CLI wrapper
- link shdeps man page and completions when the library helper is available
- print PATH hint when the bin dir is not on `PATH`

Transparent legacy migration is required. Existing consumers, including
dotfiles flows that already source `install.sh --bootstrap`, must migrate from
the Bash implementation to the Rust implementation without changing their
config, hook files, environment variables, or bootstrap call site.

Migration requirements:

- Preserve `SHDEPS_DIR`, `SHDEPS_BIN`, `SHDEPS_LIB`, `SHDEPS_CONF_DIR`,
  `SHDEPS_HOOKS_DIR`, `SHDEPS_STATE_DIR`, `SHDEPS_INSTALL_DIR`,
  `SHDEPS_BIN_DIR`, and `SHDEPS_GIT_DEV_DIR` semantics.
- Preserve the sourceable `shdeps.sh` public Bash API. After migration, sourcing
  `shdeps.sh` must still define every public `shdeps_*` function.
- Preserve the existing `$SHDEPS_BIN` command path. Existing shells and scripts
  should keep invoking `shdeps` from the same path.
- Preserve the current `$SHDEPS_BIN` symlink contract: migration may update the
  symlink target or replace the target it already points at, but callers must
  not need to change `PATH` or command names.
- Preserve existing manifests, stamps, link state, config files, and hooks with
  no manual migration command.
- Skip automatic conversion for dirty git checkouts and leave the Bash
  implementation usable, because a dirty checkout is treated as active
  development.
- Treat explicit source/development overrides conservatively. When bootstrap is
  using `SHDEPS_LIB` or `$SHDEPS_GIT_DEV_DIR/shdeps`, automatic release
  conversion SHOULD NOT rewrite that checkout unless the user explicitly asks
  for it. Those paths are developer-controlled source checkouts, not fleet
  installs.
- Stage the Rust binary/wrapper and metadata before switching the public
  symlink or wrapper behavior.
- Leave the previous Bash implementation usable if binary download, checksum
  verification, extraction, smoke test, metadata write, or symlink replacement
  fails.
- Make the migration idempotent: rerunning `install.sh`, sourcing
  `install.sh --bootstrap`, or running `shdeps self-update` after a partial
  attempt must either complete migration or continue using the prior working
  install.
- Do not strand a currently running shell. A shell that already sourced the
  Bash implementation may keep those functions until the next bootstrap or
  shell session, but the on-disk install must remain internally consistent and
  the next bootstrap must see either the old working Bash implementation or the
  fully staged Rust implementation.

The migration cannot rely on an old Bash installer re-executing newly pulled
installer code after `git pull`. The project therefore MUST ship either:

- a bridge release where the Bash implementation remains functional but the
  installer/self-update path learns how to stage and activate the Rust binary,
  followed by the Rust-default release after fleet machines have had a chance to
  pick up the bridge; or
- a self-contained compatibility shim that remains functional when an old
  checkout pulls Rust-era files and the Rust binary is not installed yet.

Any chosen migration path MUST be covered by tests that start from a Bash-era
install and end with the Rust binary active through the same `SHDEPS_BIN` and
sourceable `shdeps.sh` entry points.

Converted installs MUST write install metadata describing the new install
method and enough previous-install context to debug or roll back conversion
failures. The metadata SHOULD identify that the install was converted from a
Bash-era git checkout when applicable.

Rust-era release install mode MUST:

- detect OS and architecture
- choose a supported artifact label
- download a matching release archive when possible
- verify SHA-256 when available
- stage files before replacing current install
- install `shdeps`, `shdeps.sh`, docs, man page, and completions
- symlink `$SHDEPS_BIN` to the installed binary
- write install metadata
- preserve the previous install if any step fails
- print PATH hint when the bin dir is not on `PATH`

Release install and self-update rollback MUST be implemented as a first-class
transactional workflow:

- discover current install ownership from metadata and filesystem state
- download and verify into a staging directory outside the live install
- smoke-test or validate staged files before changing live files
- replace live files with rename/swap operations where practical
- keep enough previous-install metadata to restore the old binary/wrapper if a
  later step fails
- clean staging directories after success or failure

Rollback logic should live in one installer/self-update module rather than being
open-coded across download, extraction, symlink, and metadata paths.

Sourceable bootstrap mode MUST:

- avoid leaking `set -e` or other strict-mode options into the caller
- find existing shdeps via `SHDEPS_LIB`, dev clone, installed dir, or fresh
  install
- source `shdeps.sh` so Bash API functions become available
- set up CLI/extras links
- run method-aware `self-update` when safe
- return success/failure to caller instead of exiting the caller shell

Uninstall mode MUST:

- unlink shdeps-owned extras before removing install dir
- remove `$SHDEPS_BIN` when it is the shdeps symlink
- remove `$SHDEPS_DIR`
- be idempotent

## Install Metadata

Release/source installs SHOULD write:

```text
$SHDEPS_DIR/.shdeps-install.json
```

Schema draft:

```json
{
  "schema": 1,
  "method": "release",
  "artifact_platform": "linux-x86_64-musl",
  "tag": "v2026.05.23",
  "commit": "abc1234",
  "repo": "cgraf78/shdeps",
  "converted_from": {
    "method": "git",
    "path": "/home/chris/.local/share/shdeps",
    "commit": "def5678"
  },
  "installed_at": "2026-05-23T00:00:00Z"
}
```

Allowed `method` values:

- `git`
- `release`
- `source-build`
- `manual`

Unknown schema or method MUST make release-style `self-update` fail clearly
without replacing files.

## Release Artifact Contract

Supported artifact labels:

- `linux-x86_64-musl`
- `linux-aarch64-musl`
- `macos-x86_64`
- `macos-aarch64`

WSL uses Linux musl artifacts.

Archive naming:

```text
shdeps-${TAG}-${ASSET_PLATFORM}.tar.gz
shdeps-${TAG}-${ASSET_PLATFORM}.tar.gz.sha256
```

Checksum files SHOULD use the standard `sha256sum`/`shasum -a 256` text format
used by the hive-memory release workflow:

```text
<hex-sha256>  shdeps-${TAG}-${ASSET_PLATFORM}.tar.gz
```

Installers and `self-update` MUST verify the archive bytes against the checksum
before replacing an existing release install. If the current platform has no
supported artifact label, the release installer MUST fail clearly or fall back
to a documented source/git install path; it must not guess a nearby platform.

Archives MUST contain:

- `shdeps` executable
- `shdeps.sh`
- `install.sh`
- `README.md`
- `LICENSE`
- `man/man1/shdeps.1`
- shell completions

Linux release artifacts SHOULD use musl targets to avoid glibc compatibility
failures on older machines.

## Diagnostics And Logging

Log levels:

- `0`: quiet
- `1`: normal
- `2`: verbose

Quiet mode:

- suppresses normal logs
- suppresses interactive prune prompts unless `-y` is supplied
- MUST still allow important warnings/errors to be visible where current
  behavior does

Verbose diagnostics SHOULD explain:

- config files loaded
- platform/host filter decisions
- selected install roots
- package cache hit/miss and invalidation reason
- selected hook path
- GitHub credential source, redacted
- GitHub asset selected and why
- network request count
- subprocess count by command family
- phase timings

Secrets MUST NOT be printed.

## Performance Contract

shdeps should feel light and snappy.

Cheap commands:

- `help`
- `version`
- `dep-root`
- `dep-path`
- `dep-file`

Cheap commands MUST avoid:

- package-manager detection
- package database scans
- hook sourcing
- GitHub API calls
- network access
- install method initialization

Initial budgets:

| Path | Local warm target | CI target |
| --- | ---: | ---: |
| `shdeps dep-file <installed> <asset>` | <= 50 ms | <= 200 ms |
| `shdeps dep-root <installed>` | <= 50 ms | <= 200 ms |
| `shdeps check <installed>` for manifest-backed deps | <= 100 ms | <= 300 ms |
| no-op `shdeps update` with package cache hit | <= 500 ms | <= 2 s |
| no-op update with only manifest-backed non-pkg deps | <= 250 ms | <= 1 s |

The Rust port MAY recalibrate budgets with measured data, but any material
regression in warm-path performance needs an explicit documented reason.

Performance tests SHOULD be added before broad install-method rewrites land.
They should measure enough phase detail to explain regressions rather than only
failing on a single wall-clock number:

- process startup and CLI parsing
- config load
- manifest/state read
- package-cache validation
- GitHub release prefetch or skipped network work
- hook runner startup where relevant

Warm cheap-command tests MUST run with network-denying fixtures or mocks so a
regression that accidentally touches the network fails deterministically.

## Security And Trust Boundaries

Trust boundaries:

- Config files are trusted local policy, but parse defensively.
- Hook files are trusted user code, but run in isolated Bash subprocesses in
  Rust.
- Hook coordination records are untrusted input to Rust.
- GitHub API JSON is untrusted input.
- Downloaded archives are untrusted until safely extracted.
- Package-manager commands are privileged boundaries when sudo is involved.

Rules:

- Never print secrets.
- Prefer explicit GitHub auth headers and avoid accidental ambient credential
  behavior unless intentionally supported.
- Reject unsafe archive paths during extraction. Enumerated MUST-reject
  vectors (each guarded by a discrete fixture test; see Δ1):
  - Δ1a: tar entry whose path contains `..` parent-traversal components.
  - Δ1b: tar entry whose path is absolute (starts with `/`).
  - Δ1c: tar entry that is a symlink whose target resolves outside the
    extraction root.
  - Δ1d: tar entry that is a hardlink whose target resolves outside the
    extraction root.
  - Δ1e: zip entry whose path uses `\\` as separator on Linux (CVE-class
    Windows-path normalization bug).
- Verify release checksums when available.
- Preserve previous install on failed update.

## Error Message Contract

User-facing errors SHOULD include:

- action attempted
- dependency name when relevant
- path or command when relevant
- lower-level failure context
- next step when one exists

Errors that are known scripting surface SHOULD have golden tests.

Rust errors MUST be structured before formatting. The public Rust API should
return a typed error that carries:

- error kind
- action
- optional dependency name
- optional path, command, URL, or package manager
- lower-level source error when available
- intended shell exit class (`0`, `1`, or `2`) when surfaced through CLI/Bash

The CLI and Bash wrapper MAY format those errors differently from Rust callers,
but they MUST preserve the current exit-code classes and compatibility-sensitive
message content covered by golden tests.

At minimum, the Rust error taxonomy SHOULD distinguish:

- usage/config parse errors
- missing dependency or missing dependency asset
- unsupported platform/artifact
- package-manager unavailable or package install failure
- external tool unavailable or install failure
- Git/GitHub network or auth failure
- archive download/checksum/extraction failure
- unsafe archive contents
- state read/write/lock failure
- hook source/execution/protocol failure
- self-update/install rollback failure

## Deterministic Ordering

The following MUST be deterministic:

- config file load order
- loaded dependency order
- `list` output order
- hook execution order
- manifest upsert behavior for a single dependency

The following are not fully deterministic in the Bash reference and MUST NOT be
treated as scripting contracts without new tests and an explicit migration note:

- orphan/prune listing order, because Bash reads manifest rows into an
  associative array before listing them
- normal-mode output order from parallel Phase B jobs, because lines are emitted
  as workers finish
- manifest full-file row order after mixed remove/upsert operations, beyond the
  guarantee that each dependency has at most one current row after upsert

Rust may stabilize these outputs where doing so does not break callers, but
final state must remain equivalent to current Bash behavior.

## Backward Compatibility Rules

- Existing config files must continue to parse.
- Existing hooks must continue to run.
- Existing manifests and state files must continue to work.
- Existing dotfiles bootstrap flow must continue to work.
- Existing recent repos using `shdeps dep-file` must continue to work.
- Breaking changes require a spec revision, migration path, and tests.

## Test Coverage Strategy

The existing Bash test suite is not disposable. It is the reference parity
suite for observable behavior and should continue to run during the Rust port.

The Rust port MUST use three complementary test layers:

1. **Reference/parity tests**: the existing `test/shdeps-test` shell suite,
   adapted so public behavior can run against either the Bash reference or the
   Rust implementation. This suite protects CLI behavior, shell return codes,
   hook semantics, state compatibility, mocked install methods, and historical
   edge cases discovered while shdeps was Bash-only.
2. **Rust-native tests**: Rust unit and integration tests for parser logic,
   platform matching, ownership policy, state formats, GitHub asset matching,
   archive safety, installer metadata, error types, and performance-sensitive
   pure logic. These tests should live close to the Rust code and should not
   require shelling out when a direct library call is clearer.
3. **Compatibility smoke tests**: end-to-end tests against real packaging and
   downstream usage surfaces: `install.sh --bootstrap`, release archives,
   dotfiles integration, and recent repositories that call `shdeps dep-file`.

Golden tests MUST cover compatibility-sensitive CLI and Bash-wrapper output:

- `shdeps version`
- `shdeps help`
- `shdeps check` installed, skipped, missing, and unknown cases
- `shdeps dep-root`, `dep-path`, and `dep-file` success and failure cases
- unknown option/command usage errors
- common install-method warnings that downstream scripts may notice
- `shdeps.sh` public API helper stdout for path and predicate wrappers

The existing Bash tests SHOULD be split as the port progresses:

- public behavior tests that both implementations must pass
- legacy-internal tests that may remain Bash-only until the internal Bash
  helper they inspect is replaced by a Rust API, hidden bridge command, or
  public behavior assertion

Do not mechanically port every shell assertion into Rust. Prefer Rust-native
tests for Rust-owned pure logic and keep the shell suite for compatibility
surfaces where shell behavior itself is part of the contract.

The Rust implementation is not complete until:

- the public-behavior portion of the existing shell suite passes against Rust
- Rust unit/integration tests cover the same core behavior at the library level
- Bash compatibility wrapper tests prove existing `source shdeps.sh` callers and
  hooks still work
- release/install smoke tests pass on every supported platform artifact

## Spec-To-Test Mapping

| Spec area | Required coverage |
| --- | --- |
| CLI contract | golden CLI tests and parity tests |
| Config grammar | Rust unit tests and Bash parity tests |
| Platform/host filters | Rust unit tests and existing shell tests |
| Manifest schema | Rust unit tests, transition tests, Bash parity |
| Package cache | cache hit/miss integration tests |
| GitHub credentials | `GH_TOKEN`, `GITHUB_TOKEN`, `gh auth token`, and unauthenticated fallback tests |
| Install methods | mocked toolchain integration tests |
| Ownership policy | prune and method-transition tests |
| Hook ABI | Bash compatibility and Rust hook-runner tests |
| Bash API | source `shdeps.sh` tests |
| Bash runtime floor | wrapper/bootstrap/hook-prelude smoke tests under Bash `4.3` |
| Rust API | rustdoc and unit/integration tests |
| Installer | archive smoke tests and bootstrap tests |
| Self-update | git checkout and release install tests |
| Legacy migration | Bash-era checkout to Rust install conversion tests |
| Release artifacts | matrix smoke tests |
| Diagnostics | verbose output tests |
| Performance | warm-path budget tests |
| Security | archive traversal and secret-redaction tests |

## Compatibility Deltas

These are intentional behavior changes the Rust port introduces relative to
the Bash reference. Each delta is compatibility-safe in the sense that valid
existing configs, hooks, and state continue to work, but each one IS a change
and deserves its own regression test guarding the prior behavior where
relevant. This ledger is the source of truth for the test plan's
delta-regression tests.

| ID | Delta | Why compatibility-safe | Regression test |
| --- | --- | --- | --- |
| Δ1 | Archive extraction rejects unsafe paths (`..`, absolute, symlink/hardlink traversal, backslash on Linux) | Normal safe archives still extract; only malformed ones are rejected | Five discrete fixture tests, one per attack vector |
| Δ2 | Runtime GitHub credential precedence `GH_TOKEN` → `GITHUB_TOKEN` → `gh auth token` | Public unauthenticated path still works when no token is available | Public-asset download succeeds with no token; public-asset download succeeds with `GH_TOKEN` set (same asset selected) |
| Δ3 | Authenticated asset download for `github:release` | Optional, only triggers when token present | Same as Δ2 |
| Δ4 | Per-state-dir advisory lock for state mutations | Bash had no lock; Rust adds for safety | Concurrent `shdeps update` runs preserve manifest/.links/.binlinks integrity |
| Δ5 | Wrapper refuses Bash <4.3 with remediation message | Clearer than cryptic `declare -A` failures | Source wrapper under Bash 3.2: clean refusal, non-zero exit, no functions registered |
| Δ6 | ABI version negotiation between wrapper and binary | Wrapper that pre-dates a renamed `__api` command refuses cleanly | Source wrapper from older shdeps; place newer binary on `PATH` with renamed bridge; wrapper refuses, does not silently return wrong predicate exit codes |
| Δ7 | `install.sh` requires Bash 3.2 only; wrapper requires Bash 4.3+ | Installer can bootstrap a fresh macOS; wrapper can use modern Bash | Install.sh runs under Bash 3.2 fixture |
| Δ8 | Method transition: stage→swap→cleanup ordering | Failed cleanup leaves new method usable; failed install leaves old method intact | Cleanup-failure fixture: chmod old install root read-only after swap; assert new method works and manifest intact |
| Δ9 | Package check cache invalidates on `SHDEPS_<NAME>_REPO` and `SHDEPS_AUTO_EPEL` change | Bash cache did not track these; new cache is stricter | Change repo override env, assert cache miss on next run |
| Δ10 | Package check cache validation uses stat-mtime fast path | Hash work only on mtime mismatch | Warm `shdeps update` measured under 500 ms with 50-dep config |
| Δ11 | Cheap commands (`dep-file`, `dep-root`, `dep-path`) use partial/lazy config load | Full load is not required to resolve a single dep | `dep-file` with 100-dep config under 50 ms local |
| Δ12 | Wrapper caches ABI check and common RuntimeEnv values in shell vars | Reduces fork overhead on dotfiles bootstrap from ~250 ms to ~30 ms | Sourced wrapper followed by 5 helper calls measured under 50 ms |
| Δ13 | `shdeps_pkg_mgr` MUST NOT trigger detection (cached-read only) | Matches current Bash semantics; the naive Rust impl would change it | Test sourcing wrapper and calling `shdeps_pkg_mgr` BEFORE `shdeps_update` returns empty, not a freshly-detected value |
| Δ14 | Cross-state-dir sharing of install/bin/extras dirs unsupported | Single-user single-state-dir is the documented case | Warning logged if `SHDEPS_INSTALL_DIR` is overridden without matching `SHDEPS_STATE_DIR` |
| Δ15 | `migrate` removed from user-facing CLI | Canonicalization happens at parse-time; user command is moot | `shdeps help` does not mention `migrate`; `shdeps migrate` returns usage error |

Any future intentional behavior change MUST add a row here before landing.

## Open Spec Questions

These are intentionally unresolved in this draft:

- Whether a C ABI or `staticlib` artifact is needed for a future non-Rust
  consumer.
- Exact release tag scheme for distribution archives.
