# Rust Port Plan

This document defines the plan to port `shdeps` from Bash to Rust while
preserving today's behavior exactly. The Rust `shdeps` binary must be a
drop-in replacement for the current CLI, and the Rust crate API must expose the
same operations for Rust callers.

The current Bash implementation is the reference implementation until the Rust
port passes the full parity suite. The goal is not a redesign. It is a careful
implementation-language migration with compatible state, hooks, config, output,
exit codes, and install behavior.

## Goals

- Preserve the current `shdeps` CLI interface exactly.
- Preserve the current config format and environment-variable contract.
- Preserve the current manifest, stamp, binlink, and extras-link state formats.
- Preserve existing Bash hook files and their public `shdeps_*` helper API.
- Define the public API clearly in code, docs, and tests so future changes do
  not blur public compatibility with private implementation details.
- Provide a first-class Rust library crate that owns the implementation and is
  reused by the CLI and Bash compatibility wrapper.
- Provide prebuilt binaries for every supported platform.
- Keep the current test coverage, then extend it with Rust unit,
  integration, parity, compatibility, and release smoke tests.
- Keep `shdeps version` readable and tied to a concrete git commit suffix. It
  must never degrade to `unknown`.

## Non-Goals

- Do not change the config language during the port.
- Do not change dependency names, install roots, state paths, or hook lookup
  rules.
- Do not require users to migrate existing manifests or state files.
- Do not require a Rust toolchain on machines that only want to install and run
  `shdeps`.
- Do not define or commit to a stable C ABI until a real non-Rust consumer
  exists.
- Do not remove the sourceable Bash API until all current users have been
  migrated away from it. The audit below shows they have not.
- Do not preserve private Bash globals, internal function names, or incidental
  call structure as Rust public API. Preserve behavior; make public API
  boundaries intentional.

## Reference Inputs

The Rust port is based on these existing sources:

- `shdeps.sh`: current implementation and public Bash API.
- `bin/shdeps-legacy`: legacy CLI argument parsing, command names, help text,
  and output conventions.
- `install.sh`: current install, uninstall, and sourceable bootstrap behavior.
- `test/shdeps-test`: current behavior suite and the primary parity oracle.
- `README.md` and `AGENTS.md`: documented user and agent-facing contracts.
- `docs/rust-port-spec.md`: normative compatibility contract for CLI, config,
  state, hooks, installer, performance, and release behavior.
- `hive-memory`: release workflow and artifact packaging reference.

## Downstream Usage Audit

Recent `cgraf78` repos mostly use the CLI path APIs:

- `agentguard`, `sley`, `termnav`, `cmdblocks`, and `checkrun` document or use
  `shdeps dep-file ...`.
- `sley` resolves schema files at runtime via `shdeps dep-file`.
- Dotfiles uses the CLI for `dep-root`, `dep-file`, `version`, and scheduled
  `shdeps prune -y`.

Dotfiles also uses the sourceable Bash API directly:

- `.local/lib/dot/core/init.sh` sources `install.sh --bootstrap`.
- `.local/lib/dot/core/core.sh` calls `shdeps_update`.
- Dot merge/filter code calls `shdeps_platform_match` and
  `shdeps_host_match`.
- `~/.config/shdeps/hooks.d/*` calls public helpers such as
  `shdeps_install_dir`, `shdeps_bin_dir`, `shdeps_pkg_mgr`,
  `shdeps_reinstall`, `shdeps_pkg_install_for_mgr`, `shdeps_warn`,
  `shdeps_log`, `shdeps_require_sudo`, `shdeps_platform`, and
  `shdeps_github_release_install`.

Conclusion: the Bash compatibility API is required. A Rust crate API and Rust
CLI are not enough by themselves because existing hooks and dotfiles currently
depend on shell functions being available after sourcing.

## Compatibility Contract

### CLI

The Rust binary must support the same commands:

- `update`
- `self-update`
- `list`
- `check <name>`
- `dep-root <name>`
- `dep-path <name> <rel>`
- `dep-file <name> <rel>`
- `prune`
- `version`
- `help`

`migrate` is removed from the user-facing CLI in the Rust port. Current
parsing canonicalizes GitHub names at load/parse time, so the historical
migration is moot for new fleet machines. If any latent state-rewrite need
emerges, expose it under `shdeps __api migrate` rather than re-adding to
the public surface. See spec Δ15.

The same options must work with the same precedence and exit codes:

- `-c, --config <path>`
- `-f, --force`
- `-R, --reinstall`
- `-q, --quiet`
- `-v, --verbose`
- `-h, --help`
- prune-specific `-y`
- prune-specific `--dry-run`

Error wording, help text, table headings, status words, and exit codes should be
treated as compatibility-sensitive. The test suite should pin the lines that
matter for scripting and user recognition.

### Environment Variables

The Rust implementation must preserve the current runtime environment contract:

- `SHDEPS_CONF_DIR`
- `SHDEPS_HOOKS_DIR`
- `SHDEPS_STATE_DIR`
- `SHDEPS_FORCE`
- `SHDEPS_REINSTALL`
- `SHDEPS_QUIET`
- `SHDEPS_REMOTE_TTL`
- `SHDEPS_GIT_DEV_DIR`
- `SHDEPS_INSTALL_DIR`
- `SHDEPS_BIN_DIR`
- `SHDEPS_LOG_LEVEL`
- `SHDEPS_JOBS`
- `SHDEPS_AUTO_EPEL`
- `SHDEPS_<NAME>_REPO` dynamic repo URL overrides for GitHub repo deps
- `GH_TOKEN`
- `GITHUB_TOKEN`

Runtime GitHub credential precedence should be `GH_TOKEN`, then
`GITHUB_TOKEN`, then `gh auth token`. `GH_TOKEN` is an intentional
compatibility-safe expansion over the Bash reference so CI and fleet bootstrap
can provide explicit credentials without depending on a host-level `gh` login.
For explicit private forks or private release assets, the installer needs the
same credential plan; see [Public Release Bootstrap](#public-release-bootstrap).

The installer and sourceable bootstrap have their own compatibility variables:

- `SHDEPS_DIR`
- `SHDEPS_REPO`
- `SHDEPS_BIN`
- `SHDEPS_LIB`
- `SHDEPS_GIT_DEV_DIR`

The Rust-era wrapper, sourceable bootstrap, hooks, and hook preludes must keep
supporting Bash `4.3` or newer unless a future compatibility revision changes
the public Bash API contract. The standalone Rust binary can run cheap
non-hook commands without Bash, but the distributed shell compatibility layer
still has a Bash runtime floor.

Internal variables such as `_SHDEPS_*` arrays, display state, and test-only
`SHDEPS_TEST_*` variables are not public API. Tests may keep using test-only
variables while the Bash reference exists, but the Rust port should expose
behavior through explicit test fixtures or hidden `__api` commands instead of
accidentally freezing private implementation globals.

### Config Format

The config format must remain unchanged:

```text
# name              method           [cmd]            [aliases]                [filter]
jq                  pkg
cgraf78/ds          github:repo
neovim/neovim       github:release   nvim
ripgrep             cargo            rg
github.com/junegunn/fzf              go
ruff                uv
prettier            npm
nerd-fonts          custom
```

The Rust parser must preserve:

- sorted loading of `*.conf` files
- ignoring non-`.conf` files
- comments and blank lines
- whitespace-separated fields
- `-` as an explicit empty field marker
- manager-qualified command names such as `apt:batcat`
- package aliases such as `apt:fd-find,dnf:fd-find`
- `NONE` alias semantics
- `os:` and `host:` filters
- negated filters such as `os:!wsl`
- `.git` suffix canonicalization for `github:repo` and `github:release`

### State Files

The Rust implementation must read and write the same state files:

- manifest: `$SHDEPS_STATE_DIR/manifest`
- manifest line format: `name|method|cmd|install_path`
- package check cache: `$SHDEPS_STATE_DIR/pkg-check-cache-v3`
- TTL stamps: `$SHDEPS_STATE_DIR/<name>.*.stamp`
- revision stamps: `$SHDEPS_STATE_DIR/<name>.rev`
- extras links: `$SHDEPS_STATE_DIR/<name>.links`
- bin links: `$SHDEPS_STATE_DIR/<name>.binlinks`

For names containing slashes, state paths must stay nested exactly as they are
today. For example, `cgraf78/ds` must continue to use paths under
`$SHDEPS_STATE_DIR/cgraf78/`.

The method-transition cleanup behavior is part of the contract, and the Rust
port refines the ordering to be staged-then-swap-then-cleanup. If a dependency
changes install method:

1. Stage the new method's install (download, extract, run external installer)
   to a staging path; verify the artifact is present and runnable.
2. Atomically swap the manifest row and bin symlink to the new method.
3. Clean up old non-`pkg` managed artifacts (old install root, old
   `.links`/`.binlinks` entries, old TTL/rev stamps).
4. If step 3 fails after a successful swap, the new method still works;
   leftover artifacts are reported as orphaned-leftover via verbose log and
   re-attempted on the next `update`/`prune`. Cleanup failure MUST NOT roll
   back the swap.

The deliberate exception is `pkg`: `shdeps` must stop tracking the old
package-manager entry, but it must not uninstall the system package.

This ordering replaces the earlier "remove old before installing new"
behavior because the old order leaves the dependency entirely absent if the
new install fails. See spec Method Transitions section and Δ8 for the
required regression test (cleanup-failure fixture).

The package check cache is also compatibility-sensitive because it prevents warm
`dot update` runs from paying repeated package-manager probes. Preserve the
proof-obligation idea from the Bash cache: config content, manifest fingerprint,
package-manager identity, platform, host, command paths, package database
fingerprints, and hook content must all be part of cache validity. Do not
replace it with a bare timestamp cache.

### Install Methods

All current methods must be preserved:

- `pkg`
- `github:repo`
- `github:release`
- `cargo`
- `go`
- `uv`
- `npm`
- `custom`

The Rust port must preserve:

- package-manager detection order and skip behavior
- package batching and retry fallback
- local dev clone preference for `github:repo`
- owner/repo install roots for GitHub methods
- SSH fallback for private GitHub repo clones
- `SHDEPS_<NAME>_REPO` overrides
- GitHub release API token behavior
- GitHub release asset matching preferences
- archive extraction behavior
- binary symlink behavior
- preserving existing regular files in `SHDEPS_BIN_DIR`
- extras discovery and symlink tracking
- no-network testability through mocked commands and local fixtures

### Hooks

Existing hook files must continue to be Bash scripts and must continue to define
the same optional functions:

- `exists(name)`
- `version(name)`
- `install(name)`
- `post(name)`
- `uninstall(name)`

Hooks must continue to have access to the public `shdeps_*` Bash helpers.

The most important hook compatibility case is `shdeps_github_release_install`.
Dotfiles uses it to install `neovim/neovim` into a hook-owned target path so a
launcher can own the public `nvim` command. The Rust port must preserve this
behavior, including manifest and prune semantics.

## Target Architecture

The port should have four layers.

Architecture must follow the global design principles used for this repo:

- Keep shared knowledge single-sourced. Config parsing, dependency identity,
  state path calculation, platform matching, and install-root resolution should
  each have one authoritative module and one clean API.
- Expose clean interfaces. Callers should ask for `dep_file` or
  `install_dep`; they should not recreate manifest parsing, path validation, or
  method-specific ownership checks.
- Compose from single-purpose parts. Keep parsing, state, process execution,
  hook execution, package-manager logic, GitHub release selection, archive
  extraction, logging, and CLI formatting separate enough to test directly.
- Keep source files reasonably sized. A file that starts owning multiple
  concerns should be split by responsibility rather than becoming a broad
  catch-all module. Large files are acceptable only when the domain itself is
  cohesive and heavily commented, such as a table-driven release asset matcher.
- Consolidate after the second use. Do not pre-abstract every helper up front,
  but do not allow three copies of the same path, identity, or platform logic.
- Guard async and subprocess boundaries. Parallel installs, hook subprocesses,
  background downloads, and deferred cleanup must revalidate paths/state before
  mutating anything.
- Prevent re-entrancy in polled/display loops. If the Rust port keeps a live
  progress display, it must not allow overlapping refresh/render operations.
- Prefer separation over crippling. Hooks should run as normal Bash in a
  controlled subprocess with a compatibility prelude, not in a partially
  reimplemented shell.
- Make compatibility observable. When behavior differs by config, cache,
  platform, host, selected install root, hook path, GitHub asset, or credential
  source, diagnostics should be able to explain the decision without exposing
  secrets.
- Design for rollback. `shdeps` installs infrastructure used to install other
  infrastructure, so failed installer and self-update paths must leave the last
  known-good binary and wrapper usable.
- Make state changes transactional. State files should be written through temp
  files and atomic renames, and broad state mutations should have one module
  that owns locking, cleanup, and recovery rules.
- Make broad Rust state mutation lock-protected before Rust becomes the default.
  The lock should cover short read-modify-write windows for manifest, cache,
  stamp, link-state, metadata, prune, and method-transition changes, but it
  should not be held across network downloads, package-manager installs,
  language tool installs, or arbitrary hook execution. Re-read state after
  reacquiring the lock so stale observations do not overwrite another process.
- Separate policy from mechanism. Decisions such as "which asset wins" or
  "`pkg` is not uninstalled during prune" should live in testable policy code,
  separate from download, extraction, symlink, and file-removal mechanics.
- Define trust boundaries explicitly. Local config is trusted policy but still
  parsed defensively; hooks are trusted user code but run in isolated Bash
  subprocesses; GitHub API data and archives are untrusted input; sudo/package
  manager calls are privileged boundaries.
- Make ownership rules explicit. One documented policy should define which
  files shdeps owns and may remove, including bin symlinks, extras links,
  install roots, local-dev clone symlinks, package-manager deps, and hook-owned
  artifacts.
- Prefer deterministic ordering. Config loading, dependency display, manifest
  rewrites, prune actions, hook execution, and parallel-install log output
  should be stable enough for tests and human comparison.
- Treat user-facing errors as API. Errors should include the dependency name and
  failed action where relevant, add context around lower-level failures, and
  include the next step when one exists.
- Set explicit performance expectations for warm no-op paths. `dot update`
  calls `shdeps`, so no-op runs should avoid network calls, avoid unnecessary
  language package-manager spawns, and keep `dep-file` startup cost low enough
  for editor/runtime use.
- Treat performance as a user-facing feature. `shdeps` should feel light and
  snappy in shell, editor, cron, and dotfiles workflows, especially when no
  dependencies changed.
- Keep state human-readable. Manifests, install metadata, and important caches
  should remain inspectable enough to debug a broken machine over SSH.
- Design for partial availability. Fresh systems may lack `gh`, `curl`, `sudo`,
  `tar`, `unzip`, `zstd`, a supported package manager, network access, or a
  writable bin dir. Fail hard only when the requested operation cannot make
  meaningful progress.
- Keep escape hatches explicit. Environment overrides such as repo URLs, config
  dirs, state dirs, install dirs, force/reinstall modes, and future release-tag
  overrides are public contract, not incidental implementation checks.

### Rust Library Crate

The Rust library crate owns all real behavior. The binary, Bash compatibility
wrapper, hook prelude bridge, and tests should call into this crate instead of
duplicating implementation logic. It should be structured around small modules:

```text
src/
  lib.rs
  cli.rs
  config.rs
  env.rs
  errors.rs
  fs.rs
  hooks.rs
  install/
    cargo.rs
    custom.rs
    github_release.rs
    github_repo.rs
    go.rs
    npm.rs
    pkg.rs
    uv.rs
  logging.rs
  manifest.rs
  platform.rs
  process.rs
  state.rs
  version.rs
```

`Cargo.toml` should define a normal Rust library crate plus the `shdeps`
binary:

```toml
[lib]
name = "shdeps"

[[bin]]
name = "shdeps"
path = "src/main.rs"
```

Cargo will build the Rust library artifacts needed by Rust callers and by the
CLI. Do not add `crate-type = ["staticlib"]` or design exported C symbols until
there is a real non-Rust consumer that needs a stable ABI.

Every module should document the invariant it owns. For example, `config` owns
the config file grammar and canonical dependency identity, `state` owns
manifest/stamp/cache file formats, and `hooks` owns the Bash compatibility
boundary. This keeps callers from depending on ad hoc details in another layer.

### Rust CLI

The `shdeps` binary should be a thin CLI layer over the Rust library crate. It
should use the same command names and options as the current Bash CLI.

Hidden `__api` commands should exist to support the Bash compatibility wrapper
and hook prelude. These are internal compatibility commands, not user-facing
commands.

Initial registry:

- `shdeps __api platform-match <spec>`
- `shdeps __api host-match <spec>`
- `shdeps __api filter-match <spec>`
- `shdeps __api platform`
- `shdeps __api force`
- `shdeps __api reinstall`
- `shdeps __api load-count`
- `shdeps __api install-dir`
- `shdeps __api git-dev-dir`
- `shdeps __api bin-dir`
- `shdeps __api pkg-mgr`
- `shdeps __api pkg-install <package>`
- `shdeps __api pkg-install-for-mgr <mgr:package>...`
- `shdeps __api require-sudo`
- `shdeps __api dep-root <name>`
- `shdeps __api dep-path <name> <rel>`
- `shdeps __api dep-file <name> <rel>`
- `shdeps __api link-extras <name> <dir>`
- `shdeps __api unlink-extras <name>`
- `shdeps __api github-release-install <name> <cmd> [repo] [bin_path]`

These commands make the Bash wrapper small and auditable.

The compatibility spec owns the complete table mapping every public Bash
function to a Rust API owner and either a public CLI command, hidden bridge
command, hook coordination record, or local shell-only implementation. Keep that
table current before adding or changing a public helper. This prevents
accidental wrapper behavior from becoming a second implementation.

### Bash Compatibility Wrapper

`shdeps.sh` should remain sourceable. It should define every current public
`shdeps_*` function and delegate to the Rust binary.

This wrapper is required because shell callers need functions, return statuses,
and source-time behavior. A static library cannot replace shell sourcing.

Most wrapper functions should be simple:

```bash
shdeps_update() { command shdeps update "$@"; }
shdeps_platform_match() { command shdeps __api platform-match "$@"; }
shdeps_install_dir() { command shdeps __api install-dir; }
```

The wrapper must preserve predicate return codes for functions like
`shdeps_force`, `shdeps_reinstall`, `shdeps_platform_match`,
`shdeps_host_match`, and `shdeps_filter_match`.

The wrapper must prefer the `shdeps` binary beside the sourced wrapper before
falling back to `command shdeps` on `PATH`. This avoids a fleet machine sourcing
one checkout's `shdeps.sh` while accidentally delegating to an older binary
elsewhere on `PATH`.

### Hook Runner

Rust should execute hooks by spawning Bash with a generated prelude. The prelude
defines the public `shdeps_*` helper functions as one-line shims that delegate
to the Rust binary via `__api` subprocess calls. There is intentionally no
versioned IPC protocol between hook and parent.

Why this design (revised in eng review):

A versioned JSON record protocol was considered and rejected. The simpler
delegation model removes the entire untrusted-input attack surface — no record
parser to harden, no schema-version negotiation, no validation layer. Cost is
one extra fork per mutating helper call, which is acceptable because hooks
already run in a subprocess.

The hook runner should:

1. Generate a unique `txn_id` and create
   `$SHDEPS_STATE_DIR/.changed-markers/<txn_id>/` for the current update run.
2. Build a generated Bash prelude defining every public `shdeps_*` function
   as `command shdeps __api <name> "$@"` (one-line shims). The lone exception
   is `shdeps_mark_changed`, which `touch`es a sentinel file under the markers
   directory.
3. Release the parent's state lock if held (see lock invariant below).
4. Fork a Bash subprocess with environment:
   `SHDEPS_UPDATE_TXN_ID=<txn_id>`, `SHDEPS_CURRENT_DEP=<name>`,
   `SHDEPS_HOOK_PHASE=<phase>`, plus all existing SHDEPS_* env.
5. In the subprocess: source the prelude, source the hook file, invoke the
   requested hook function with `<name>` as `$1`.
6. After the subprocess exits: re-read state files that may have changed
   (manifest, .links, .binlinks), enumerate and unlink sentinel files in
   `.changed-markers/<txn_id>/`, and feed those names into post-hook
   scheduling.

State-lock invariant (critical):

The parent MUST release the state lock before forking the hook subprocess.
Each `__api` call from inside the hook acquires the lock fresh in its own
short read-modify-write window. Holding the parent lock across the fork
deadlocks the first `__api` call. A regression test is required (see Tests
below): a custom hook whose `install()` calls `shdeps_link_extras` during
`shdeps update` completes without deadlock.

Mutating helpers (all go through `__api`):

- `shdeps_link_extras` → `shdeps __api link-extras`
- `shdeps_unlink_extras` → `shdeps __api unlink-extras`
- `shdeps_github_release_install` → `shdeps __api github-release-install`
- `shdeps_pkg_install` → `shdeps __api pkg-install`
- `shdeps_pkg_install_for_mgr` → `shdeps __api pkg-install-for-mgr`
- `shdeps_require_sudo` → `shdeps __api require-sudo`

Non-mutating helpers that read RuntimeEnv (`shdeps_install_dir`,
`shdeps_bin_dir`, `shdeps_git_dev_dir`, `shdeps_pkg_mgr`, `shdeps_force`,
`shdeps_reinstall`, `shdeps_platform`) MAY be cached in shell vars on first
call to amortize fork cost — see wrapper performance design below.

`shdeps_mark_changed` is the only special case: it does not invoke a Rust
subprocess at all. It `touch`es `$SHDEPS_STATE_DIR/.changed-markers/<txn_id>/<name>`
and the parent enumerates after the hook exits.

`shdeps_dep_source` remains wrapper-owned (it must source into the hook's
current shell). The prelude implements it by calling `shdeps __api dep-file`
to resolve the path, then `.` (source) that path in the subprocess. A
regression test verifies this works from inside a hook.

`shdeps_pkg_mgr` semantics (read-only):

The prelude/wrapper version of `shdeps_pkg_mgr` MUST NOT trigger detection.
It returns the already-detected manager (empty string if detection has not
run in the current process or parent runtime context). Current Bash
behavior reads `${_SHDEPS_PKG_MGR:-}`. A naive Rust `__api pkg-mgr` that
ran detection on every call would fork the package-manager probe chain per
hook call, break perf budgets, and produce inconsistent answers if env
mutates mid-update. Detection is owned by `update`/`list`/`check` paths only.

## Rust API Shape

The public Rust API should expose the same operations as the CLI and Bash API,
but in idiomatic Rust types.

Native Rust function names should not be prefixed with `shdeps_`; the crate and
module path already provide namespacing. For example, Rust callers should use
`shdeps::update(...)` and `shdeps::dep_file(...)`, while Bash keeps
`shdeps_update` and `shdeps_dep_file`. If a C ABI is added later, exported C
symbols should use a `shdeps_` prefix because C/linker symbols are flat.

Initial API sketch:

```rust
pub fn version() -> Result<String>;
pub fn update(config: Config) -> Result<UpdateSummary>;
pub fn self_update(config: Config, dir: Option<PathBuf>) -> Result<()>;
pub fn list(config: Config) -> Result<Vec<DependencyStatus>>;
pub fn check(config: Config, name: &str) -> Result<CheckStatus>;
pub fn prune(config: Config, opts: PruneOptions) -> Result<PruneSummary>;

pub fn platform(config: &Config) -> Result<Platform>;
pub fn platform_match(spec: &str, platform: Platform) -> bool;
pub fn host_match(spec: &str, host: &str) -> bool;
pub fn filter_match(spec: &str, env: &RuntimeEnv) -> FilterResult;

pub fn dep_root(config: &Config, name: &str) -> Result<PathBuf>;
pub fn dep_path(config: &Config, name: &str, rel: &str) -> Result<PathBuf>;
pub fn dep_file(config: &Config, name: &str, rel: &str) -> Result<PathBuf>;

pub fn pkg_install(config: &Config, package: &str) -> Result<()>;
pub fn pkg_install_for_mgr(config: &Config, specs: &[String]) -> Result<()>;
pub fn link_extras(config: &Config, name: &str, dir: &Path) -> Result<()>;
pub fn unlink_extras(config: &Config, name: &str) -> Result<()>;
pub fn github_release_install(
    config: &Config,
    name: &str,
    cmd: &str,
    repo: Option<&str>,
    bin_path: Option<&Path>,
) -> Result<()>;
```

Do not add a large FFI surface speculatively. The immediate requirement is a
Rust crate API that mirrors the current operations and is reused by the CLI and
Bash compatibility layer.

If the intent later becomes "usable from C or other non-Rust languages," the
project must add an explicit C ABI, a header file, ownership rules for returned
strings/arrays, symbol-versioning expectations, and probably a `staticlib` or
`cdylib` crate type. Until that decision is made, the stable API is the Rust
crate API plus the CLI/Bash compatibility layers.

## Public API And Comment Standard

The public API and compatibility boundaries must be clear in code before the
Rust implementation becomes the default. Walls of uncommented code are not
acceptable for this port because the hard parts are compatibility invariants,
not clever algorithms.

Rust requirements:

- Every public Rust type, enum, trait, function, error variant, and option type
  must have rustdoc.
- Rustdoc should explain why the API exists, what compatibility contract it
  preserves, and which state or side effects it may touch.
- Public docs should distinguish stable contract from implementation detail.
  For example, manifest format is stable; an internal cache struct is not.
- Public APIs should use domain types instead of loose strings when that avoids
  repeated parsing or invalid states. Examples: `DependencyName`,
  `InstallMethod`, `PackageManager`, `Platform`, `Filter`, `ManifestEntry`, and
  `InstallRoot`.
- Constructors and parsers should validate invariants at the boundary so later
  code does not re-check the same rules in every module.
- Return types should make expected failures explicit. Use `Result` with
  structured errors for user-visible failures and avoid sentinel strings.
- Public examples should include the main library use cases: update, prune,
  path lookup, platform/filter matching, and hook-support operations.

Comment requirements:

- Add comments where they explain why, not what the next line does.
- Comment compatibility boundaries generously: state-file formats, hook prelude
  behavior, install-root ownership, package-cache invalidation, release asset
  selection, archive extraction safety, and installer rollback.
- Comment cross-system assumptions, especially GitHub auth behavior, WSL using
  Linux artifacts, musl release rationale, package-manager quirks, and
  dotfiles/bootstrap compatibility.
- For dense logic blocks, include a short orienting comment before the block so
  future maintainers do not have to rediscover the invariant from tests.
- Avoid stale narration, commented-out code, or restating obvious assignments.

Source organization requirements:

- Keep modules focused enough that a reviewer can understand the file's job
  without reading half the crate.
- Split files when they accumulate unrelated responsibilities, for example when
  a module starts mixing config parsing with filesystem mutation or CLI
  formatting with install-method logic.
- Prefer submodules for large cohesive domains: `install/github_release.rs` can
  split into `asset_match.rs`, `download.rs`, and `extract.rs` once each part
  has enough behavior to test independently.
- Do not hide complexity in giant utility modules. Shared helpers should live
  near the domain they support unless they are truly cross-cutting.

Behavior design requirements:

- Use small domain types to carry validated meaning across modules:
  `DependencyName`, `GitHubRepo`, `CommandName`, `RelativeAssetPath`,
  `InstallRoot`, `ManifestEntry`, `ArtifactLabel`, and `PackageOverride`.
- Make invalid states unrepresentable where practical. For example,
  `RelativeAssetPath` should reject absolute paths and parent traversal at
  construction, and `PackageOverride::Skip` should represent `NONE` instead of
  passing that sentinel string around.
- Centralize shdeps ownership decisions. Prune, method transitions,
  self-update, and uninstall should call the same ownership policy instead of
  reimplementing "is this safe to remove?" locally.
- Preserve human-readable state unless there is a measured reason not to.
  Binary cache formats are a last resort for this tool.
- Add observability hooks for compatibility decisions. A verbose run should be
  able to explain why a dependency was skipped, why a cache was invalidated, why
  a particular install root was chosen, and why a GitHub asset matched.
- Keep escape hatches named, documented, and tested. Avoid hidden environment
  toggles that only appear in one module.

Bash wrapper requirements:

- Keep a single public API section near the top of `shdeps.sh`, mirroring the
  current Bash layout.
- Each public `shdeps_*` function should have a short comment documenting its
  compatibility role, arguments, stdout/stderr behavior, return status, and
  whether it delegates to the Rust binary.
- Comments should explain non-obvious compatibility choices, especially why the
  wrapper prefers its sibling binary, why predicate functions preserve shell
  statuses, and why `shdeps_dep_source` cannot be replaced by a subprocess-only
  command.
- Private wrapper helpers should remain clearly prefixed and separated from the
  public API section.

CLI requirements:

- User-facing commands and hidden `__api` commands must be documented in
  separate code sections so internal bridge commands are not mistaken for
  supported user CLI.
- Hidden `__api` commands should have comments explaining which Bash wrapper or
  hook-prelude function depends on them.
- Golden tests should assert the public CLI help text, but hidden commands
  should be tested through the wrapper/prelude behavior they support.

Documentation requirements:

- README should document the user-facing CLI and Bash compatibility API.
- Generated or hand-written Rust docs should document the native Rust API.
- A separate compatibility table should map:
  Rust API name -> Bash API name -> CLI or hidden bridge command.
- The table should explicitly note that Rust names are unprefixed because the
  crate/module path provides namespacing.

## Versioning

`shdeps version` must keep returning:

```text
shdeps YYYYMMDD-HHMMSS-<8hex>
```

The Rust build should resolve this through `build.rs` and the shared
`scripts/release-version.sh` formatter.

Resolution order:

1. `SHDEPS_BUILD_VERSION`, when release packaging pins the exact public
   version.
2. A pushed release tag with the same `YYYYMMDD-HHMMSS-<8hex>` shape.
3. `SHDEPS_BUILD_COMMIT`/`GITHUB_SHA` plus `SHDEPS_BUILD_TIMESTAMP`.
4. `SHDEPS_BUILD_COMMIT`/`GITHUB_SHA` plus the current UTC timestamp.
5. Git checkout commit plus the current UTC timestamp.
6. fail the build.

Do not fall back to `unknown`.

`Cargo.toml` may need a package version because Cargo requires one, and release
tags use the same generated identifier as the runtime binary. The Cargo package
version must not become the runtime shdeps version.

## Release And Binary Distribution

Use `hive-memory` as the release reference.

### Supported Platforms

Build these archive labels:

- `linux-x86_64-musl`
- `linux-aarch64-musl`
- `macos-x86_64`
- `macos-aarch64`

WSL should consume the Linux musl artifacts. There should not be a separate WSL
binary unless a future WSL-specific behavior actually requires one.

### Rust Targets

Map artifact labels to Rust targets:

| Artifact label | Rust target | Runner |
| --- | --- | --- |
| `linux-x86_64-musl` | `x86_64-unknown-linux-musl` | `ubuntu-24.04` |
| `linux-aarch64-musl` | `aarch64-unknown-linux-musl` | `ubuntu-24.04-arm` |
| `macos-x86_64` | `x86_64-apple-darwin` | `macos-15-intel` |
| `macos-aarch64` | `aarch64-apple-darwin` | `macos-latest` |

The Linux builds should be musl because shdeps is an installer/bootstrap tool.
It should not depend on the target machine having a new enough glibc.

### Artifact Names

Derive the archive shape from hive-memory, but use the `shdeps` binary name:

```text
shdeps-${TAG}-${ASSET_PLATFORM}.tar.gz
shdeps-${TAG}-${ASSET_PLATFORM}.tar.gz.sha256
```

Example:

```text
shdeps-20260523-184512-abc12345-linux-x86_64-musl.tar.gz
shdeps-20260523-184512-abc12345-linux-x86_64-musl.tar.gz.sha256
```

The archive should contain:

- `shdeps` executable
- `shdeps.sh` compatibility wrapper
- `install.sh`
- `README.md`
- `LICENSE`
- `man/man1/shdeps.1`
- shell completions

The archive should not require a source checkout or Rust toolchain.

### Release Workflow

Model the workflow on `hive-memory/.github/workflows/release.yml`:

1. Create or reuse a draft release for the pushed tag.
2. Build one archive per platform.
3. Smoke-test each archive on its runner.
4. Upload `.tar.gz` and `.sha256` assets.
5. Publish the draft release only after all matrix builds pass.

Release jobs should keep default permissions read-only and grant
`contents: write` only to jobs that create or upload release assets.

Use pinned Actions revisions, as hive-memory does.

Do not copy hive-memory's Cargo-version tag validation verbatim unless shdeps
chooses semver package releases. `shdeps` runtime versioning is generated from
build timestamp plus commit suffix, so release tag validation should validate
that scheme and ensure the suffix matches the tagged commit.

### Packaging Script

Add `scripts/package-release.sh`, derived from hive-memory's script.

Responsibilities:

- build `cargo build --release --locked --target <target>`
- stage the executable and compatibility files
- create the tarball
- create a SHA-256 file with native `sha256sum` or `shasum -a 256`
- print the archive path

The packaging script must be runnable locally so release behavior is
reproducible outside GitHub Actions.

## Public Release Bootstrap

This is the biggest release/distribution edge case.

`shdeps` is public, so default bootstrap should prefer the public release and
HTTPS paths. SSH clone credentials are not a substitute for release assets: they
can be useful for explicit forks or custom source installs, but fleet bootstrap
should not require GitHub SSH auth to install `cgraf78/shdeps`.

The installer should therefore support this order:

1. Use an existing dev clone when present.
2. Use an existing installed binary when present.
3. Download a matching public release artifact.
4. Build from source over the configured repo URL when `cargo` is available,
   falling back to SSH only when the configured HTTPS repo cannot be cloned.
5. Use the legacy Bash implementation only during the transition window.
6. Fail clearly with remediation text when none of the above can work.

Release artifact credentials should be detected in this order:

1. `GH_TOKEN`
2. `GITHUB_TOKEN`
3. `gh auth token`

Do not assume SSH auth can download release assets, and do not require SSH auth
for the default public `cgraf78/shdeps` bootstrap path.

## Self-Update Plan

The current `self-update` command updates a clean git checkout with
`git pull --ff-only` and skips dirty trees. A Rust binary installed from a
release archive will not necessarily live in a git checkout, so `self-update`
needs method-aware behavior.

Supported self-update modes:

1. **Dev/source checkout:** if the install directory contains `.git`, preserve
   current behavior: skip dirty trees and use `git pull --ff-only`.
2. **Release archive install:** if install metadata says the binary came from a
   release archive, query the latest compatible release, download the matching
   platform archive, verify checksum, and atomically replace the binary,
   wrapper, man page, and completions.
3. **Unknown/manual install:** report that the install method is unknown and
   suggest rerunning `install.sh`.

The installer should write a small metadata file, for example
`$SHDEPS_DIR/.shdeps-install.json`, recording:

- install method: `git`, `release`, `source-build`, or `manual`
- platform artifact label
- installed tag, when known
- build commit
- source repo/release URL
- install timestamp

This metadata lets `self-update` avoid guessing from filesystem shape alone and
gives future bug reports enough information to diagnose bootstrap failures.

## Installer Plan

`install.sh` remains the stable curl/source entry point.

Executed mode:

- detect platform and architecture
- select the matching artifact label
- download the latest or requested release archive
- verify SHA-256 when the checksum is available
- install the `shdeps` binary under `SHDEPS_DIR`
- install the `shdeps.sh` compatibility wrapper under `SHDEPS_DIR`
- symlink `$SHDEPS_BIN` to the binary
- link man pages and completions
- print the existing PATH hint when needed
- write install metadata for future `self-update`

Sourceable `--bootstrap` mode:

- preserve current no-`set -e` leakage behavior
- expose public `shdeps_*` functions by sourcing `shdeps.sh`
- ensure the binary exists, installing it if needed
- run method-aware `shdeps self-update` unless the install is dirty or
  unavailable
- keep dotfiles-compatible `SHDEPS_CONF_DIR` and `SHDEPS_HOOKS_DIR` behavior
- transparently migrate an existing clean Bash-era install to the Rust binary
  without requiring dotfiles or other sourced consumers to change their
  bootstrap call site

Uninstall mode:

- unlink man pages and completions before removing files
- remove `$SHDEPS_BIN`
- remove `$SHDEPS_DIR`
- keep current idempotent behavior and wording

During the transition, `install.sh` may still source the legacy Bash
implementation. After cutover, it should source the compatibility wrapper.

### Transparent Bash-To-Rust Migration

Existing fleet machines may already have a Bash-era `~/.local/share/shdeps`
git checkout and dotfiles may source that checkout's `install.sh --bootstrap`.
That path must migrate in place.

The subtle failure mode is that the old Bash installer can `git pull` newer
files, but it does not automatically re-exec the newly pulled installer logic.
If a release simply replaces `shdeps.sh` with a Rust-delegating wrapper and the
Rust binary is not present yet, that first post-pull bootstrap can strand the
machine with a wrapper that has nothing valid to delegate to.

Use a two-stage migration:

1. **Bridge release**: keep the Bash implementation functional, but update
   `install.sh`, `shdeps_self_update`, and any bootstrap helpers so they can
   detect the Rust artifact for the host platform, download it, verify the
   checksum, stage it, smoke-test it, write install metadata, and switch the
   public command path. Dirty git checkouts skip conversion and remain usable.
2. **Rust-default release**: after the bridge has landed and CI/dotfiles smoke
   tests prove the conversion path, make the Rust binary the default
   implementation while keeping `shdeps.sh` as the source-compatible Bash API
   wrapper.

The migration must preserve:

- `SHDEPS_DIR`, `SHDEPS_BIN`, `SHDEPS_LIB`, `SHDEPS_CONF_DIR`,
  `SHDEPS_HOOKS_DIR`, `SHDEPS_STATE_DIR`, `SHDEPS_INSTALL_DIR`,
  `SHDEPS_BIN_DIR`, and `SHDEPS_GIT_DEV_DIR` behavior
- the existing `$SHDEPS_BIN` command path
- the current symlink contract for `$SHDEPS_BIN`: callers should keep invoking
  the same path even if migration changes the symlink target or replaces the
  target file
- `source shdeps.sh` and `install.sh --bootstrap` behavior
- existing config files, hooks, manifests, stamps, `.links`, and `.binlinks`
- rollback to the prior Bash implementation if any migration step fails
- developer-controlled source checkouts selected through `SHDEPS_LIB` or
  `$SHDEPS_GIT_DEV_DIR/shdeps`; skip automatic release conversion for those
  unless explicitly requested

Add a fixture that starts from a Bash-era installed checkout, runs the same
bootstrap path dotfiles uses, and verifies that:

- the call returns success without manual intervention
- `shdeps version` resolves to the Rust binary's generated timestamp-plus-commit
  version
- `source shdeps.sh; shdeps_update` still works
- public Bash helpers delegate successfully
- a failed artifact download/checksum/extraction leaves the Bash install usable
- a dev checkout selected through `$SHDEPS_GIT_DEV_DIR/shdeps` is not silently
  converted into a release install
- install metadata records that a release install was converted from a Bash-era
  git checkout

Installer safety requirements:

- Stage new files in a temporary directory and atomically swap or rename only
  after download, checksum, and smoke checks pass.
- Preserve the previous working install if an update fails.
- Use credential headers explicitly for GitHub release assets and avoid ambient
  `.netrc` behavior unless intentionally supported.
- Detect unsupported OS/architecture combinations with a clear error.
- Keep uninstall scoped to `SHDEPS_DIR`, `SHDEPS_BIN`, and shdeps-owned extras.

## Test Strategy

The existing Bash suite is the parity oracle. The Rust port should not be
considered complete until the current suite can run against Rust with equivalent
coverage.

### Reference Harness

Add a test switch:

```bash
SHDEPS_IMPL=bash ./test/shdeps-test
SHDEPS_IMPL=rust ./test/shdeps-test
```

The harness should make the same assertions against both implementations where
possible. Tests that inspect private Bash internals should be split into:

- public behavior tests that Rust must pass
- legacy-internal tests that remain only for the Bash reference until removed

### Rust Unit Tests

Rust unit tests should cover pure logic:

- config parsing
- GitHub name canonicalization
- platform and host matching
- filter matching
- package alias resolution
- dependency path validation
- release asset matching
- version extraction
- manifest parsing and rewriting
- stamp freshness
- path calculation
- ownership policy decisions
- deterministic ordering guarantees
- typed constructors rejecting invalid states

### Rust Integration Tests

Integration tests should use `assert_cmd`, `tempfile`, and mock command
directories to cover:

- CLI help/version/list/check output
- update with custom deps
- package manager detection and batching
- cargo/go/uv/npm via mocked tools
- GitHub repo installs via local fixture repos
- GitHub release installs via local fixture assets
- prune behavior
- method transition cleanup
- extras linking and unlinking
- hidden `__api` commands used by the Bash wrapper
- package-check cache hits and invalidations
- concurrent update/prune safety around shared state files
- verbose diagnostics explaining compatibility decisions
- warm no-op update behavior and `dep-file` startup behavior
- `check` fast-path behavior for manifest-backed deps, including an assertion
  that package-manager detection is not invoked for those methods

### Bash Compatibility Tests

Add tests that source `shdeps.sh` and verify:

- every public `shdeps_*` function exists
- predicate functions return the expected shell status
- `shdeps_update` delegates correctly
- `shdeps_dep_source` sources into the current shell
- no `set -e` leakage
- hook prelude functions behave like normal sourced functions

### Downstream Compatibility Tests

Run or reproduce the relevant dotfiles tests against Rust shdeps:

- bootstrap tests
- core update/finalize tests
- shdeps hook tests
- runtime asset helpers
- cron filter helpers

Also smoke-test recent repos that use `shdeps dep-file`:

- `agentguard`
- `sley`
- `termnav`
- `cmdblocks`
- `checkrun`

### Release Smoke Tests

Each release matrix job should:

- extract the archive
- run `./shdeps version`
- run `./shdeps help`
- run a minimal `custom` dependency update in an isolated HOME
- source `./shdeps.sh` and call `shdeps_version`
- verify `dep-file` against an archive-contained fixture or staged temp dep

These release smoke tests catch packaging mistakes that unit tests will miss.

### Failure And Safety Tests

Add tests for cases that are easy to miss in a straight port:

- interrupted or failed install leaves the previous binary usable
- corrupted or mismatched `.sha256` files abort installation
- unknown install metadata makes `self-update` fail clearly
- release-installed `self-update` replaces all archive-owned files together
- concurrent runs do not corrupt manifest, cache, `.links`, or `.binlinks`
- hook subprocesses cannot write malformed coordination records that panic the
  Rust parent
- partial tool availability produces precise warnings instead of broad failure
  when progress is still possible
- rollback leaves the previous binary usable after failed release download,
  failed checksum verification, failed extraction, or failed smoke test
- wrapper and hook-prelude behavior works under Bash `4.3`, the current minimum
  shell version required by the Bash implementation

### Performance Expectations

The port should add lightweight performance checks for the paths that users hit
often. These are product requirements, not optional tuning. `shdeps` is on the
interactive shell/dotfiles path, so it should feel light and snappy.

- warm no-op `shdeps update` should not touch the network
- warm no-op package checks should use the package-check cache
- warm no-op external installer deps should not spawn `cargo`, `go`, `uv`, or
  `npm`
- `shdeps dep-file` should have low startup cost because editor and shell
  integrations may call it interactively
- `shdeps check` should classify the target before expensive setup. For
  manifest-backed deps, it should read manifest/path state directly and avoid
  package-manager detection, package database scans, hook sourcing, GitHub
  calls, and network access. Only `pkg` and the target `custom` hook should pay
  for their method-specific status checks.

Treat these as budgets to keep honest rather than microbenchmarks. The goal is
to prevent obvious regressions during the port.

Initial performance budgets:

| Path | Local warm target | CI target |
| --- | ---: | ---: |
| `shdeps dep-file <installed> <asset>` | <= 50 ms | <= 200 ms |
| `shdeps dep-root <installed>` | <= 50 ms | <= 200 ms |
| `shdeps check <installed>` for manifest-backed deps | <= 100 ms | <= 300 ms |
| no-op `shdeps update` with package cache hit | <= 500 ms | <= 2 s |
| no-op update with only manifest-backed non-pkg deps | <= 250 ms | <= 1 s |

These budgets should be calibrated once the Rust skeleton exists, but any
change that materially slows a warm path needs an explicit reason. CI runners
are noisier than local machines, so CI budgets may use a multiplier like
hive-memory's performance tests do.

Performance design rules:

- Keep CLI startup lean. Avoid heavy global initialization before dispatching
  cheap commands like `dep-file`, `dep-root`, `version`, and `help`.
- Parse only what the command needs. Path lookup commands should not detect the
  package manager, check GitHub, load hooks, or scan package databases.
- Keep network work behind explicit stale-cache or force decisions.
- Cache expensive package-manager checks with complete invalidation proof, not
  with a blind TTL.
- Prefer direct filesystem checks and manifest reads over spawning subprocesses
  on hot paths.
- Preserve deterministic parallelism for slow install/update work without
  making warm no-op paths pay for worker setup.
- Add lightweight timing instrumentation under verbose or debug output so slow
  phases can be diagnosed in real user environments without a profiler.
- Keep release builds optimized for small startup overhead and predictable
  static-ish deployment. Avoid dependencies that add large runtime startup cost
  unless they replace enough complexity to justify it.

Performance tests should record enough phase detail to explain regressions:

- config load time
- manifest/cache read time
- package cache validation time
- package-manager probe count
- `check` method classification time and whether package-manager detection was
  avoided for manifest-backed deps
- subprocess count by command family
- network request count
- hook invocation count
- total elapsed time

## CI Plan

During the port, CI should include:

- existing shell test matrix
- `cargo fmt --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test`
- Rust integration tests
- Bash compatibility tests
- dotfiles compatibility smoke tests where practical
- release packaging dry run on at least Linux x86_64 for non-tag pushes
- installer/self-update smoke tests for both git-checkout and release-archive
  install modes

The existing OS matrix should continue:

- macOS
- Ubuntu
- Debian
- Arch
- CentOS Stream
- Fedora
- Alpine
- WSL simulation

Use hive-memory's checkout/auth pattern:

- pinned `actions/checkout`
- `persist-credentials: false`
- minimal job permissions
- explicit credentials only where release upload requires them

## Implementation Workflow

Implement the Rust port as a series of granular, progressive local commits.
Each commit should leave the repository in a coherent state and should include
the focused tests for the behavior it introduces or changes.

Commit rules for the port:

- Commit after each logical slice, such as the Rust skeleton, pure parser
  logic, manifest state, one install method, hook runner mechanics, wrapper
  cutover, installer migration, and release packaging.
- Keep structural and behavioral changes separate when practical. For example,
  add the module skeleton and public types before filling in install-method
  behavior that depends on them.
- Run the relevant local checks before each commit. At minimum, run the current
  shell parity suite for Bash-facing behavior and the Rust fmt/clippy/test
  checks once the Rust crate exists.
- Use the repository commit-message style from the agent rules, with `Summary`
  and `Testing` sections, so each commit can stand alone in review.
- Do not push this work to the remote GitHub repository during implementation
  unless the user explicitly asks for a push. Local commits are expected; remote
  publication is a separate user decision.
- If a later phase reveals a previous local commit needs adjustment and that
  commit has not been pushed, amend or use a local fixup/rebase workflow rather
  than adding noisy corrective commits.

## Implementation Phases

### Phase 0: Freeze The Reference

- Keep the current Bash implementation available as the reference.
- Keep `docs/rust-port-spec.md` current as the normative compatibility contract.
- Add the dual-implementation test harness.
- Add golden outputs for compatibility-sensitive CLI commands.
- Add downstream audit tests for sourced API usage.
- Audit `SHDEPS_TEST_*` and `_SHDEPS_TEST_*` variables in `test/shdeps-test`
  and document each in the spec as either: (a) drop after Bash retire,
  (b) promote to a hidden `shdeps __api test-*` command for fixture use, or
  (c) re-spec as a `RuntimeEnv` override. Without this audit, private impl
  details bleed into the Rust port's surface.
- Audit `~/.config/shdeps/hooks.d/` (and dotfiles hook files) for actual
  public-API usage. Grep every `shdeps_*` invocation, every reliance on
  current working directory, every assumption about function leakage, every
  read of private `_SHDEPS_*` globals. Record findings in the spec's Bash
  API table or as test cases. The plan's repo-level audit (lines 60-83) is
  not enough; arbitrary user hooks may depend on quirks not yet documented.

Acceptance:

- Current Bash implementation still passes the full test suite.
- CI proves the reference suite is stable before Rust behavior changes land.
- `SHDEPS_TEST_*` enumeration is committed to the spec.
- `hooks.d/` audit findings are reflected in the Bash API stability table.

### Phase 1: Rust Skeleton

- Add `Cargo.toml`.
- Add `src/lib.rs` and `src/main.rs`.
- Add `build.rs` generated-version enforcement.
- Add top-level module docs that define the crate's public API boundaries.
- Add initial CI for fmt, clippy, test, and doc checks.

Acceptance:

- `cargo test` passes.
- `RUSTDOCFLAGS='-D missing-docs' cargo doc --no-deps` passes.
- `cargo build --release` produces the `shdeps` binary.
- `shdeps version` prints `shdeps YYYYMMDD-HHMMSS-<8hex>`.

### Phase 2: Pure Logic

- Port config parsing.
- Port platform/host/filter matching.
- Port package alias resolution.
- Port path validation.
- Port release asset matching.
- Port version extraction.

Acceptance:

- Rust unit tests match Bash reference cases.
- No filesystem mutation is needed for these tests.

### Phase 3: State And Filesystem

- Port manifest read/upsert/remove.
- Port package-check cache validation and writes.
- Port stamp freshness and revision stamps.
- Port orphan detection.
- Port method transition cleanup.
- Port binlink and extras-link state.
- Port dep-root/dep-path/dep-file.
- Port prune cleanup.
- Add the per-state-dir advisory lock used by Rust state mutations. Keep lock
  windows short, release before slow external work or hooks, and revalidate
  state before committing mutations after a lock gap.

Acceptance:

- Rust reads Bash-written state.
- Bash reads Rust-written state during the transition.
- No state migration is required.
- Concurrent update/prune tests prove manifest, cache, `.links`, and
  `.binlinks` are not corrupted.

### Phase 4: CLI Parity

- Implement all public commands.
- Implement hidden `__api` commands.
- Match output and exit codes.
- Add the compatibility table mapping Rust, Bash, CLI, and hidden bridge APIs.
- Add golden output tests.
- Add the Rust error taxonomy and CLI/Bash error formatting layer before broad
  command implementations depend on ad hoc string errors.
- Add performance gates for cheap commands before later install-method work can
  add startup cost unnoticed.

Acceptance:

- Existing CLI behavior tests pass against Rust.
- Recent repos using `shdeps dep-file` work unchanged.
- Golden tests cover `version`, `help`, `check`, path helpers, and usage errors.
- Cheap-command benchmarks run with network-denying fixtures.

### Phase 5: Hook Runner

- Implement Bash hook subprocess runner.
- Generate hook prelude.
- Define the versioned hook coordination protocol.
- Coordinate side effects back to Rust through validated protocol records.
- Preserve hook function isolation.

Acceptance:

- Existing hook lifecycle tests pass.
- Dotfiles hook tests pass unchanged.
- `shdeps_github_release_install` works from hooks.
- Malformed hook coordination records fail safely.

### Phase 6: Install Methods

Port install methods in this order:

1. `custom`
2. `pkg`
3. `cargo`
4. `go`
5. `uv`
6. `npm`
7. `github:repo`
8. `github:release`

This order validates hooks first, then simple external command wrappers, then
the GitHub methods with the most state and network behavior.

Acceptance:

- Each method passes unit and integration tests.
- No method test requires live network access.
- GitHub release behavior matches current asset-selection and extraction rules.

### Phase 7: Installer And Bridge Release

Reordered ahead of wrapper cutover in eng review: the wrapper cannot land
before the bridge migration machinery exists to install/find the Rust binary
it delegates to. Without this ordering, Bash-era fleet checkouts get stranded.

- Ship the bridge migration release. The Bash implementation remains the
  authoritative `shdeps` while installer/self-update gains the ability to
  detect the host platform, download the Rust binary, verify checksum, stage
  it, smoke-test it, write install metadata, and prepare the public
  `$SHDEPS_BIN` symlink to flip to Rust.
- Add packaging script (`scripts/package-release.sh`).
- Add release workflow.
- Add installer artifact download (multi-platform).
- Add source-build fallback.
- Add private release credential handling (`GH_TOKEN`/`GITHUB_TOKEN`/
  `gh auth token`).
- Preserve the existing SSH-clone-then-source-build fallback path for private
  repos; do not regress fleet machines that bootstrap via SSH git clone.
- Add install metadata (`$SHDEPS_DIR/.shdeps-install.json`).
- Add method-aware `self-update` (git checkout / release / unknown).
- Spec release-selection rules: skip prereleases by default; skip drafts;
  refuse downgrade; rollback to prior install on bad-artifact detection.
- Implement rollback as one transactional installer/self-update module rather
  than open-coding recovery in download, extraction, symlink, and metadata
  paths.
- `install.sh` MUST remain Bash 3.2-compatible (it runs before anything is
  installed; stock macOS ships Bash 3.2). Add a Bash 3.2 CI smoke test.
- `install.sh` MUST detect its invocation context: (a) bundled-in-release-
  archive mode (binary sibling + `.shdeps-install.json` present → skip
  download), (b) source-checkout mode (sibling `.git` directory → preserve
  git pull behavior), (c) curl-pipe mode (else → download release).

Acceptance:

- `install.sh` installs the prebuilt binary on Linux, macOS, and WSL.
- `shdeps self-update` works for git checkouts and release-archive installs.
- A Bash-era installed checkout migrates through `install.sh --bootstrap`
  without changing dotfiles.
- Release archives pass smoke tests.
- Checksums are generated and verified.
- Failed download, checksum, extraction, smoke-test, symlink, and metadata
  writes leave the previous install usable.
- `install.sh` runs successfully under Bash 3.2 (Docker fixture or
  macOS-default-shell CI job).
- Three install.sh mode-detection fixtures pass (bundled / source / curl).
- SSH-clone fallback still bootstraps private-repo fleet machines.

### Phase 8: Bash Wrapper Cutover

Lands AFTER Phase 7 so the Rust binary is available everywhere before the
wrapper starts delegating to it.

- Replace `shdeps.sh` implementation with the compatibility wrapper.
- Keep every public `shdeps_*` function as a one-line shim (lint-enforced;
  see Code Quality section).
- Keep the public API section clear and fully commented.
- Keep `install.sh --bootstrap` source behavior.
- Wrapper detects Bash version on source and refuses Bash <4.3 with a clear
  remediation message (do not let users see cryptic `declare -A` failures).
- Wrapper performs ABI version negotiation: calls `shdeps __api version` once
  on source, caches the result in `_SHDEPS_ABI_CHECKED`, refuses to define
  functions if the binary reports an incompatible ABI version.
- Wrapper caches common RuntimeEnv values (`install_dir`, `bin_dir`,
  `git_dev_dir`, `platform`, `pkg_mgr`) in shell vars on first call to
  amortize fork overhead. Cached values survive into child shells via export.

Acceptance:

- `source shdeps.sh; shdeps_update` works.
- Dotfiles can source bootstrap and call `shdeps_update`.
- Predicate helpers return correct shell statuses.
- Public wrapper comments describe arguments, output, return statuses, and
  compatibility rationale.
- No `set -e` leakage.
- Sourcing wrapper under Bash 3.2 refuses cleanly with remediation message.
- Sourcing wrapper against an ABI-incompatible binary refuses cleanly.
- Wrapper one-liner lint test passes (grep `shdeps.sh`; assert each public
  `shdeps_*` function body is one line of `command shdeps ...` with named
  exceptions for `shdeps_dep_source` and the predicate-translating helpers).
- Sourced wrapper followed by 5 helper calls completes under 50 ms (env
  caching working).

### Phase 9: Default To Rust

- Make the Rust binary the default installed `shdeps`.
- Keep Bash wrapper as the public sourceable API.
- Update README badges and docs from Bash implementation to Rust binary plus
  Bash compatibility wrapper.
- Keep the legacy Bash reference only as long as needed for confidence.

Acceptance:

- Full CI is green.
- Dotfiles and recent repo smoke tests are green.
- Existing fleet state does not need manual migration.
- Existing dotfiles bootstrap paths transparently activate the Rust
  implementation or keep the prior Bash implementation usable on failure.

## Edge Cases To Pin In Tests

- Empty and missing config dirs.
- Multiple config files sorted deterministically.
- `.git` suffix canonicalization for GitHub names.
- Owner/repo names in manifest and state paths.
- Existing regular files in `SHDEPS_BIN_DIR` are preserved.
- Broken symlinks are handled correctly during cleanup.
- Method transitions clean stale non-`pkg` artifacts.
- Method transitions from `pkg` do not uninstall packages.
- `github:repo` prefers local dev clones.
- `github:repo` falls back from HTTPS to SSH.
- Existing GitHub repo clones with HTTPS origins get SSH push URL handling.
- `github:release` handles tar, zip, gzip, bzip2, zstd, and raw binary assets.
- Linux asset matching prefers the correct libc variant.
- Multi-binary release assets prefer the configured command name.
- Archive extraction rejects unsafe paths.
- Archive extraction rejects symlink/hardlink path traversal and absolute paths.
- Package-manager aliases and `NONE` behave correctly.
- Ownership policy prevents shdeps from removing user-owned commands, local dev
  clone targets, system packages, and hook-owned artifacts without hook
  cooperation.
- Verbose diagnostics explain config loading, filter decisions, install root
  selection, cache invalidation, hook selection, asset matching, and credential
  source without leaking secrets.
- User-facing errors include action and dependency context where relevant.
- Ordering is deterministic for config loads, manifests, list output, prune
  output, hook execution, and parallel job summaries.
- Fresh or partial systems degrade precisely when optional tools are missing.
- Package-check cache invalidates on config, manifest, package DB, command path,
  platform, host, and hook-content changes.
- Missing external installers warn once and skip only affected deps.
- `SHDEPS_REINSTALL` implies force behavior.
- `cargo` installs keep using `--locked`.
- `uv`, `npm`, and `go` installs preserve current root/bin-dir environment
  behavior.
- TTL-fresh runs self-heal missing public symlinks without reinstalling.
- Hooks do not leak functions into each other.
- Hook failures do not prevent later deps from running, but update exits
  non-zero when required.
- `custom` deps without required `exists()` warn and skip.
- `custom` uninstall without `uninstall()` warns but removes shdeps stamps.
- `pkg` prune warns and removes tracking only.
- `dep-path` rejects absolute and parent-traversal paths.
- `dep-file` requires a readable regular file.
- `dep-source` sources into the current shell from the Bash wrapper.
- Cheap commands such as `version`, `help`, `dep-root`, and `dep-file` do not
  perform expensive package-manager, hook, GitHub, or network initialization.
- `self-update` skips dirty trees.
- `self-update` updates release archive installs without a git checkout.
- `self-update` preserves ANSI codes when stdout is redirected.
- `install.sh --bootstrap` is idempotent.
- `install.sh --bootstrap` does not leak shell options.
- `install.sh --uninstall` is idempotent.
- Failed installer and self-update attempts leave the previous install usable.
- Extras directories keep compaudit-safe permissions under permissive umasks.
- Parallel jobs clean up temp dirs and child processes on `INT`/`TERM`.
- `shdeps version` never prints `unknown`.

## Open Decisions

These should be decided before the release workflow lands:

- Release tag format. The runtime version, release tag, release archive names,
  and packaged metadata all use `YYYYMMDD-HHMMSS-<8hex>`. This keeps versions
  readable and traceable without reintroducing a hand-maintained `VERSION`
  file.
- Private release asset access. If the repo or release assets remain private,
  fleet installs need `GH_TOKEN`, `GITHUB_TOKEN`, or `gh auth token`; SSH clone
  auth is not enough.
- C ABI. Do not build or promise `libshdeps.a` immediately. Add a C ABI or
  `staticlib` output only when there is a real non-Rust consumer and the
  project can define headers, exported symbols, ownership rules, and
  error/status representation.

## Completion Criteria

The Rust port is complete when:

- Rust `shdeps` is the default installed binary.
- Public API docs clearly separate stable API from private implementation.
- Code comments explain why at compatibility boundaries, hook boundaries,
  release/install safety boundaries, and state-format boundaries.
- Source files remain reasonably scoped and are split when they collect
  unrelated responsibilities.
- Compatibility decisions are observable in verbose/debug output.
- State changes are transactional and preserve human-readable formats.
- Ownership policy is centralized and tested.
- Warm no-op and cheap-command performance expectations are tested or otherwise
  enforced in CI.
- The native Rust API is idiomatic and unprefixed; Bash compatibility keeps
  `shdeps_*`; any future C ABI uses prefixed symbols.
- `shdeps.sh` remains source-compatible with existing dotfiles and hooks.
- The current shdeps test suite passes against Rust.
- Rust unit and integration tests cover the same behavior as the Bash suite.
- Dotfiles compatibility tests pass.
- Release archives are produced for all supported platforms.
- Installer smoke tests pass on Linux, macOS, and WSL.
- `self-update` works for both source checkout and release archive installs.
- Existing manifests and state files work without migration.
- CI is green across the full matrix.

## Review Decisions Appendix

This appendix consolidates decisions made during eng review (2026-05-23). Each
ID below is referenced from prose changes throughout this plan and the spec.
Prose has been edited inline for the most structural decisions (R1, D5, D12,
D21); the rest live here as the authoritative list for implementers.

### Architecture (D2–D7)

- **D2 — ABI version negotiation.** Wrapper records its built-against `__api`
  ABI version. On source it calls `shdeps __api version` (once, cached in
  `_SHDEPS_ABI_CHECKED`), refuses to define functions if the binary reports
  an incompatible ABI. Adds one bridge command. Prevents silent wrong-exit
  bugs when fleet machines have a mixed wrapper/binary on PATH.
- **D3 — Cache key extensions.** Package check cache validity inputs now
  include the dynamic env override fingerprint: hash of all
  `SHDEPS_<NAME>_REPO` vars + `SHDEPS_AUTO_EPEL`. See spec lines on Package
  Check Cache Format.
- **D4 — No-lock-across-fork invariant.** The parent MUST release the state
  lock before forking a hook subprocess. Lock acquisition has a timeout
  (~30 s) that surfaces a structured error rather than hanging. Regression
  test: custom hook whose `install()` calls `shdeps_link_extras` during
  `shdeps update` completes without deadlock.
- **D5 — Method-transition stage→swap→cleanup ordering.** Prose updated
  inline in Reference Inputs / Compatibility Contract → State Files. Failed
  cleanup leaves new method usable; failed install leaves old method intact.
  Fixture test simulates cleanup failure (chmod old install root read-only
  after swap) and asserts new method works, manifest intact.
- **D6 — `install.sh` mode detection.** Three modes: bundled-in-archive
  (binary sibling + `.shdeps-install.json` → skip download); source-checkout
  (sibling `.git` → preserve git pull); curl-pipe (else → download release).
  Fixture per mode in Phase 7 acceptance.
- **D7 — Intel macOS runner.** Use the current repo-accepted Intel macOS
  runner label for x86_64 release and installer smoke jobs. Add a TODO to drop
  `x86_64-apple-darwin` entirely once GitHub retires Intel runners.

### Code Quality (D8–D13)

- **D8 — `InstallMethod` trait.** All eight install methods share a
  lifecycle (classify → install → record manifest → link extras → mark
  changed). Define `trait InstallMethod { fn classify(…); fn install(…);
  fn artifacts(…); }` so the dispatcher iterates without method-specific
  branching. Prevents 8 copies of "did this change anything? then mark
  changed" glue.
- **D9 — `manifest` vs `state` boundary.** `manifest` owns schema
  (`ManifestEntry` parse/serialize/upsert) as pure functions, no IO.
  `state` owns the per-state-dir advisory lock, atomic temp-file-then-
  rename writes, and orchestrates all state-file operations (manifest,
  .links, .binlinks, stamps, install metadata). Single source for on-disk
  state mutation.
- **D10 — `SHDEPS_TEST_*` audit in Phase 0.** Enumerate and classify before
  the Rust port locks them in. Already added to Phase 0 acceptance above.
- **D11 — Wrapper one-liner lint.** Bash test that greps `shdeps.sh` and
  asserts every public `shdeps_*` body is one line of `command shdeps ...`
  (named exceptions: `shdeps_dep_source`, predicate-translating helpers).
  CI failure on wrapper bloat.
- **D12 — `migrate` removed from user-facing CLI.** Prose updated inline.
  See Δ15.
- **D13 — `#![deny(missing_docs)]` at crate level.** Add to `src/lib.rs` in
  Phase 1. Local fail-fast for missing docs rather than CI-only.

### Tests (D14–D16)

- **D14 — Bash 3.2 wrapper refusal.** Wrapper detects `BASH_VERSINFO[0]*100
  - BASH_VERSINFO[1] < 403` on source, prints remediation, returns non-zero
  without registering functions. CI smoke test under Bash 3.2. See Δ5.
- **D15 — Five discrete archive-safety tests.** One fixture per attack
  vector: tar `../etc/passwd`, tar absolute path, tar symlink traversal,
  tar hardlink traversal, zip backslash on Linux. See Δ1.
- **D16 — `GH_TOKEN` + public-asset regression test.** Mock GitHub API +
  asset endpoint, `GH_TOKEN` exported, public asset URL: verify same asset
  selected, same file written, same behavior as no-token path. See Δ2.

### Performance (D17–D19)

- **D17 — Lazy/partial config for cheap commands.** `dep-file`, `dep-root`,
  `dep-path` MUST resolve config lazily — enumerate config files, locate
  the entry matching `name`, parse only that entry. Full load remains for
  update/list/check. Perf gate: `dep-file` with 100-dep config fixture
  under 50 ms local. See Δ11.
- **D18 — Cache validation stat-mtime fast path.** Already specced in
  Package Check Cache Format → Validation cost rules. See Δ10.
- **D19 — Wrapper env caching.** On first `shdeps_*` call (or at source
  time), wrapper invokes one `shdeps __api env-snapshot` returning
  install_dir, bin_dir, git_dev_dir, platform, pkg_mgr, ABI version. Cached
  in shell vars and exported so child shells skip the fetch. Reduces
  dotfiles bootstrap hang from ~250 ms to ~30 ms. See Δ12.

### Codex Outside-Voice Decisions (D21–D26)

- **D21 — Phase reorder.** Bridge installer becomes Phase 7; wrapper
  cutover becomes Phase 8. Prose updated inline in Implementation Phases.
- **D22 — `shdeps_pkg_mgr` cached-read semantics.** Spec updated; the
  bridge command reads the cached value, never triggers detection. See Δ13.
- **D23 — `install.sh` Bash 3.2 tier.** Two-tier rule pinned: `install.sh`
  must work on Bash 3.2; wrapper requires Bash 4.3+. See Δ7.
- **D24 — Cross-state-dir sharing declared unsupported.** Spec MUST: all
  writers sharing install_dir/bin_dir/extras MUST share state_dir. Plan
  adds a startup warning if `SHDEPS_INSTALL_DIR` is overridden without
  matching `SHDEPS_STATE_DIR` (or vice versa). See Δ14.
- **D25 — Compatibility Deltas section.** Added to spec as a top-level
  ledger of intentional behavior changes (Δ1–Δ15). Source of truth for
  delta-regression tests.
- **D26 — Keep all-at-once port.** Reject the cheap-commands-only minimum
  slice. Acknowledge Codex's framing as a Risk: if Phase 6 (install
  methods) stalls, fall back to cheap-commands-Rust + permanent Bash for
  `update`. See Risk Register below.

### Residual Codex Findings (folded inline)

- **C#7 hook update-context.** Parent exports `SHDEPS_UPDATE_TXN_ID`,
  `SHDEPS_CURRENT_DEP`, `SHDEPS_HOOK_PHASE` to every hook subprocess. See
  Hook Runner section above.
- **C#9/C#18 hooks.d audit.** Added to Phase 0 acceptance above.
- **C#11 SSH-clone fallback.** Phase 7 explicitly preserves SSH-clone-then-
  source-build for private-repo fleet machines.
- **C#13 release selection rules.** Phase 7 specs: skip prereleases by
  default; skip drafts; refuse downgrade; rollback to prior install on
  bad-artifact detection.
- **C#16 project-level rollback.** See Risk Register below.
- **C#19 single output-formatting owner.** Add an `output` module (or
  consolidate inside `cli`) that owns all CLI text formatting, wrapper
  formatting helpers, and golden-fixture generation. Single source for
  user-visible strings; drift becomes a single-file review.

## Risk Register

- **Port stalls mid-Phase-6.** If install-method porting reaches 70% and
  becomes a maintenance burden, fall back: keep cheap commands + state +
  config in Rust, leave `update` and install methods on Bash long-term.
  Bash impl remains the default until Rust beats it on reliability and
  bootstrap simplicity. Define a quarterly checkpoint to re-evaluate.
- **Fleet machines offline during bridge window.** A machine that misses the
  bridge release and later receives a Rust-default release must still
  migrate cleanly. Rust-default installer MUST detect "no bridge ever ran"
  state and execute the bridge logic in-place rather than assuming bridge
  already happened.
- **Checksum trust scope.** SHA-256 fetched from the same channel as the
  archive defends against transfer corruption, not channel compromise.
  Document this threat model honestly; do not imply supply-chain protection
  the implementation does not provide. Signature/provenance verification
  is out of scope for the initial port.

## "What Already Exists" Section

What's already partially or fully solved by current Bash:

- **CLI argument parsing, command dispatch, help text.** `bin/shdeps-legacy` (573
  LOC) is the reference. Rust CLI just re-implements with same syntax/exit
  codes/text. Reuse: behavior contract via golden tests; do not port code
  line-for-line.
- **Config parsing (whitespace-separated fields, comments, aliases, filters,
  `.git` canonicalization).** `_shdeps_load_config` and related helpers in
  `shdeps.sh`. Rust `config` module rebuilds with same observable behavior
  pinned by Bash parity tests.
- **Platform/host filter matching.** Solid Bash impl, well-tested. Port
  to pure-logic Rust unit tests in Phase 2.
- **GitHub asset selection (multi-pass OS/arch/libc/cmd-name).** Complex,
  well-loved Bash code. Port carefully with golden fixtures of historical
  asset names that selected correctly.
- **Package-manager detection order + alias resolution.** `_shdeps_pkg_*`
  helpers. Spec already documents the contract; Rust just re-implements.
- **Method-transition cleanup.** Recently added Bash logic (`0b5ae8f`).
  Reuse the test cases; refine ordering per D5.
- **Test suite (5009 LOC `test/shdeps-test`).** This IS the parity oracle.
  Do not discard or mechanically port; split into public-behavior tests
  (run against both impls) and legacy-internal tests (Bash-only until
  replaced).
- **`install.sh` bootstrap.** Reference behavior for SHDEPS_DIR/SHDEPS_BIN/
  etc. Rust-era installer preserves all of it.
- **Hook lifecycle (`exists`, `install`, `post`, `uninstall`, `version`).**
  Mature contract; preserve as-is. The R1 simplification only changes
  HOW helpers communicate with the parent, not the hook ABI itself.

What does NOT exist yet and the Rust port must build:

- Rust build system (Cargo.toml, build.rs, target matrix).
- Per-state-dir advisory lock.
- ABI version negotiation.
- Install metadata schema (`.shdeps-install.json`).
- Release workflow + packaging script.
- Method-aware self-update.
- Compatibility Deltas test suite (Δ1–Δ15).
- Three-mode `install.sh` detection.
- Wrapper env caching layer.

## "NOT In Scope" Section

Explicitly deferred:

- **C ABI / `libshdeps.a` static archive.** Defer until a real non-Rust
  consumer exists. Adding it speculatively freezes symbol names, ownership
  rules, and error representations before requirements are real.
- **Signature/provenance verification of release artifacts.** SHA-256 from
  same channel only protects against transfer corruption. Supply-chain
  hardening (sigstore, signed manifests) is out of scope; documented as
  intentional limitation.
- **Public Rust API stability promise pre-cutover.** During Phase 0–6 the
  Rust API is internal-only. Promotion to "stable" happens after Phase 9
  when the port has survived real fleet use. Avoids freezing design before
  parity proves the model.
- **WSL-specific binary.** WSL consumes Linux musl artifacts. Revisit only
  if a WSL-specific behavior actually requires it.
- **Migrating existing manifests/state.** No migration command required;
  Rust reads Bash-written state and vice versa during transition.
- **Removing the Bash compatibility wrapper.** Stays indefinitely. The
  wrapper IS the public sourceable API; removing it would break dotfiles
  and hooks.
- **Rewriting the test runner.** `test/shdeps-test` extends with a Rust
  switch; not rewritten as a Rust harness.
- **Cheap-commands-only port slice.** Codex's minimum-risk alternative
  (D26). Rejected as the primary plan; kept as the Phase-6-stall fallback.
- **Per-resource locking beyond state_dir.** Cross-state-dir install_dir
  sharing is documented as unsupported (D24) rather than enforced via
  additional locks.
- **C#12 supply-chain signature verification.** Listed above as defer.

## Parallelization Strategy

The phased plan is mostly sequential by dependency. Within phases, some work
can parallelize across worktrees:

| Phase | Modules touched | Depends on |
| --- | --- | --- |
| 0 — Freeze | `test/`, `docs/` | — |
| 1 — Skeleton | `Cargo.toml`, `src/`, `build.rs`, CI | 0 |
| 2 — Pure logic | `src/config`, `src/platform`, `src/version` | 1 |
| 3 — State/FS | `src/state`, `src/manifest`, `src/fs` | 1 |
| 4 — CLI parity | `src/cli`, `src/output`, golden tests | 2, 3 |
| 5 — Hook runner | `src/hooks`, hook prelude generator | 3, 4 |
| 6 — Install methods | `src/install/*` | 3, 4, 5 |
| 7 — Installer/Bridge | `install.sh`, `scripts/`, `.github/workflows/` | 4 |
| 8 — Wrapper cutover | `shdeps.sh` | 5, 7 |
| 9 — Default to Rust | docs/README, release flip | 8 |

Parallel lanes after Phase 1 lands:

- **Lane A:** Phase 2 (pure logic) — `src/config`, `src/platform`,
  `src/version`. Independent of state code.
- **Lane B:** Phase 3 (state/FS) — `src/state`, `src/manifest`, `src/fs`.
  Independent of pure-logic modules.
- **Lane C:** Phase 7 prep (installer/release packaging script) — works
  against the Phase 1 skeleton; can land bridge installer behavior in
  parallel with Phase 4 CLI work.

Lanes A + B + C launch in parallel after Phase 1. Phase 4 merges A + B
results. Phase 5 depends on 3+4. Phase 6 sequential per method but the
trait skeleton (D8) lands first and individual methods can be parallel
worktrees from then.

Conflict flag: Phase 4 (CLI) and Phase 7 (installer) both touch
`src/output` if the Codex C#19 single-formatter recommendation is adopted.
Coordinate or sequence those edits.

## Failure Modes (one per new codepath)

| Codepath | Realistic failure | Test? | Error handling? | Silent? |
| --- | --- | --- | --- | --- |
| `build.rs` version resolution | Neither env nor git available at build | Required (D7-adjacent gap G7) | Build fails hard | No |
| Advisory lock acquisition | Lock held by orphaned process | D4 timeout test | Structured error after timeout | No |
| `__api` ABI mismatch | Wrapper sourced from older shdeps | D2 negotiation test | Wrapper refuses on source | No |
| Hook `__api link-extras` | Hook called during update; parent holds lock | D4 deadlock regression | Lock-fork invariant + timeout | **Was silent; now caught** |
| Method transition cleanup | chmod read-only after swap | D5 cleanup-failure fixture | Warn, leave usable, re-attempt | No |
| Archive extraction | Malicious `../etc/passwd` entry | D15 (×5 vectors) | Refuse, abort, no files written | No |
| `install.sh` curl-pipe | Network drops mid-download | Existing rollback path | Staging dir cleaned, prior install kept | No |
| Bash 3.2 wrapper source | macOS default shell | D14 refusal test | Clear remediation message | No |
| Pkg cache `SHDEPS_FOO_REPO` change | User changes env, expects refetch | D3 invalidation test | Cache miss, refetches | **Was silent; now caught** |
| GH_TOKEN + public asset | Token set, public asset URL | D16 regression test | Same asset selected as no-token | **Was silent; now caught** |

Critical-gap escalations (no test AND no handling AND silent): zero after
D1–D26 are applied. Every previously-silent failure now has a regression
test obligation.

## Implementation Tasks

Synthesized from this review's findings. Each task derives from a specific
finding above. Run with Claude Code or Codex; checkbox as you ship.

- [ ] **T1 (P1, human: ~4h / CC: ~30min)** — hooks/prelude — Replace versioned
  IPC protocol with `__api` shim prelude + sentinel-file mark-changed
  - Surfaced by: R1 / D26 — Hook Runner section
  - Files: spec Hook Coordination Protocol; plan Hook Runner; new
    `src/hooks/prelude.rs`, `src/hooks/runner.rs`
  - Verify: integration test — custom hook calls `shdeps_link_extras` during
    update, no deadlock, link is recorded
- [ ] **T2 (P1, human: ~2h / CC: ~20min)** — wrapper/__api — ABI version
  negotiation
  - Surfaced by: D2 — wrapper requires `shdeps __api version` on source
  - Files: `shdeps.sh` (cache `_SHDEPS_ABI_CHECKED`), `src/cli/api.rs`
    (`__api version`)
  - Verify: source older wrapper against newer binary with renamed bridge;
    wrapper refuses with remediation
- [ ] **T3 (P1, human: ~1h / CC: ~10min)** — cache — Add env-override
  fingerprint to package check cache key
  - Surfaced by: D3 / Δ9
  - Files: `src/state/pkg_cache.rs`
  - Verify: change `SHDEPS_FOO_REPO`, assert cache miss on next run
- [ ] **T4 (P1, human: ~3h / CC: ~20min)** — locking — Per-state-dir lock
  with no-lock-across-fork invariant + 30 s timeout + structured error
  - Surfaced by: D4
  - Files: `src/state/lock.rs`
  - Verify: hook that calls `shdeps_link_extras` during update completes
    without deadlock
- [ ] **T5 (P1, human: ~4h / CC: ~30min)** — install — Method-transition
  stage→swap→cleanup ordering + cleanup-failure fixture
  - Surfaced by: D5 / Δ8
  - Files: `src/install/transition.rs`, `src/state/manifest.rs`
  - Verify: chmod old install root read-only after swap; new method works,
    manifest intact, re-run cleans up
- [ ] **T6 (P1, human: ~2h / CC: ~15min)** — installer — Three-mode
  `install.sh` detection (bundled / source / curl)
  - Surfaced by: D6
  - Files: `install.sh`
  - Verify: one fixture per mode passes
- [ ] **T7 (P2, human: ~30min / CC: ~5min)** — CI — Keep macOS x86_64
  jobs on a repo-accepted Intel runner; add TODO to drop Intel target
  - Surfaced by: D7
  - Files: `.github/workflows/release.yml`
- [ ] **T8 (P1, human: ~3h / CC: ~25min)** — install — Define
  `trait InstallMethod`; dispatcher iterates trait objects
  - Surfaced by: D8
  - Files: `src/install/mod.rs`, all `src/install/*.rs`
  - Verify: dispatcher has no per-method match arms
- [ ] **T9 (P1, human: ~1h / CC: ~10min)** — modules — Pin `manifest` (schema
  only, no IO) vs `state` (lock + transactional IO) boundary in code
  comments
  - Surfaced by: D9
  - Files: `src/manifest.rs`, `src/state.rs`
- [ ] **T10 (P1, human: ~2h / CC: ~20min)** — phase-0 — Audit `SHDEPS_TEST_*`
  and `~/.config/shdeps/hooks.d/` for actual API/quirk usage
  - Surfaced by: D10 / C#9 / C#18
  - Files: spec API table, `test/` documentation
- [ ] **T11 (P2, human: ~30min / CC: ~5min)** — wrapper — One-liner lint test
  for `shdeps.sh`
  - Surfaced by: D11
  - Files: `test/wrapper-lint.sh`
  - Verify: every `shdeps_*` body is one line of `command shdeps ...`
- [ ] **T12 (P1, human: ~30min / CC: ~5min)** — CLI — Remove `migrate` from
  user help text and command dispatch
  - Surfaced by: D12 / Δ15
  - Files: `src/cli/dispatch.rs`, help text golden
- [ ] **T13 (P2, human: ~5min)** — crate — `#![deny(missing_docs)]` in
  `src/lib.rs`
  - Surfaced by: D13
  - Files: `src/lib.rs`
- [ ] **T14 (P1, human: ~1h / CC: ~10min)** — wrapper — Bash 3.2 detection +
  refusal with remediation
  - Surfaced by: D14 / Δ5
  - Files: `shdeps.sh`, CI Bash 3.2 fixture
  - Verify: source under Bash 3.2 → non-zero exit, clear message, no functions
- [ ] **T15 (P1, human: ~3h / CC: ~25min)** — archive — Five discrete
  extraction-safety fixtures
  - Surfaced by: D15 / Δ1
  - Files: `tests/fixtures/unsafe-archives/`, `src/install/github_release.rs`
- [ ] **T16 (P1, human: ~1h / CC: ~10min)** — github — `GH_TOKEN` +
  public-asset regression test
  - Surfaced by: D16 / Δ2
  - Files: `tests/integration/github_release_auth.rs`
- [ ] **T17 (P1, human: ~3h / CC: ~25min)** — CLI — Lazy/partial config load
  for `dep-file`/`dep-root`/`dep-path`
  - Surfaced by: D17 / Δ11
  - Files: `src/config/lazy.rs`, `src/cli/dep_path.rs`
  - Verify: 100-dep config, `dep-file` under 50 ms local
- [ ] **T18 (P1, human: ~2h / CC: ~15min)** — cache — Stat-mtime fast path
  in package cache validator
  - Surfaced by: D18 / Δ10
  - Files: `src/state/pkg_cache.rs`
- [ ] **T19 (P1, human: ~2h / CC: ~15min)** — wrapper — Env caching layer
  (one `__api env-snapshot` call, cached in shell vars)
  - Surfaced by: D19 / Δ12
  - Files: `shdeps.sh`, `src/cli/api.rs`
  - Verify: source wrapper + 5 helper calls under 50 ms
- [ ] **T20 (P1, human: ~30min)** — plan — Reorder Phase 7/8 in plan doc
  - Surfaced by: D21
  - Files: `docs/rust-port-plan.md` (this file — already done above)
- [ ] **T21 (P1, human: ~1h / CC: ~10min)** — bridge — `shdeps_pkg_mgr`
  cached-read implementation + test
  - Surfaced by: D22 / Δ13
  - Files: `src/cli/api.rs` (`__api pkg-mgr`), wrapper
  - Verify: source wrapper, call `shdeps_pkg_mgr` before update → empty string
- [ ] **T22 (P1, human: ~2h / CC: ~15min)** — installer — Bash 3.2 audit of
  `install.sh`; CI smoke test under Bash 3.2
  - Surfaced by: D23 / Δ7
  - Files: `install.sh`, CI matrix
- [ ] **T23 (P2, human: ~30min)** — env — Startup warning when
  `SHDEPS_INSTALL_DIR` overridden without `SHDEPS_STATE_DIR` (or vice versa)
  - Surfaced by: D24 / Δ14
  - Files: `src/env.rs`
- [ ] **T24 (P1, human: ~2h)** — spec — Compatibility Deltas section
  - Surfaced by: D25
  - Files: `docs/rust-port-spec.md` (already done above)
- [ ] **T25 (P2, human: ~1h)** — risk — Document quarterly checkpoint to
  re-evaluate port progress against Bash-baseline rollback option
  - Surfaced by: D26 / C#16
  - Files: this doc (Risk Register, already added)
- [ ] **T26 (P1, human: ~2h / CC: ~15min)** — hooks — Export
  `SHDEPS_UPDATE_TXN_ID`, `SHDEPS_CURRENT_DEP`, `SHDEPS_HOOK_PHASE` to
  hook subprocesses
  - Surfaced by: C#7
  - Files: `src/hooks/runner.rs`
- [ ] **T27 (P2, human: ~1h / CC: ~10min)** — output — Consolidate CLI text
      + wrapper formatting + golden generation in one `output` module
  - Surfaced by: C#19
  - Files: `src/output.rs`
- [ ] **T28 (P1, human: ~1h / CC: ~10min)** — release — Spec release
  selection (skip prereleases/drafts, refuse downgrade, rollback on bad
  artifact)
  - Surfaced by: C#13
  - Files: `src/install/self_update.rs`
- [ ] **T29 (P1, human: ~1h / CC: ~5min)** — installer — Preserve SSH-clone-
  then-source-build fallback for private repos
  - Surfaced by: C#11
  - Files: `install.sh`

## GSTACK REVIEW REPORT

| Review | Trigger | Why | Runs | Status | Findings |
| --- | --- | --- | --- | --- | --- |
| CEO Review | `/plan-ceo-review` | Scope & strategy | 0 | — | — |
| Codex Review | `/codex review` | Independent 2nd opinion | 0 | — | — |
| Eng Review | `/plan-eng-review` | Architecture & tests (required) | 1 | CLEAR (PLAN) | 26 decisions (D1–D26), 0 unresolved, 0 critical gaps |
| Design Review | `/plan-design-review` | UI/UX gaps | 0 | — | — |
| DX Review | `/plan-devex-review` | Developer experience gaps | 0 | — | — |

- **CODEX:** Outside voice returned 20 findings; 17 substantive, 6 raised as cross-model tensions (D21–D26), 5 folded inline (C#7, C#9/#18, C#11, C#13, C#16, C#19), 2 noted as already-addressed (C#1, C#6, C#14), 3 deferred (C#15 too-broad-test-plan absorbed into per-section gates, C#12 supply-chain noted as out-of-scope, C#17 offline-machine handled in Risk Register).
- **CROSS-MODEL:** Codex agreed with all eng review recommendations on D5 (transition order needed pinning), D14 (Bash 3.2 stance — extended to D23 install.sh tier), D4 (lock invariant — extended to D24 lock scope). Disagreed: Codex pushed cheap-commands-only slice (D26); user kept all-at-once.
- **UNRESOLVED:** 0
- **VERDICT:** ENG CLEARED — 26 decisions captured, plan + spec updated inline for structural changes, full task list T1–T29 ready to implement.
