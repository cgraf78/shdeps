# Hook and sudo contract

Implementation contract for hook execution, the hook helper toolkit, and the
`shdeps_require_sudo` prompt/retry path. The user-facing summary lives in the
README's [Hooks](../README.md#hooks) and [Public API](../README.md#public-api)
sections; this file records the invariants maintainers must preserve.

## Hook functions

Hook files in `hooks.d/$name.sh` may define these functions (for `github:*`
and `go` deps, hooks go in a nested path mirroring the `name` — e.g.
`hooks.d/owner/repo.sh` or `hooks.d/github.com/owner/repo.sh`):

- `exists(name)` — **required for `custom`**. Returns 0 if the dep is installed.
- `install(name)` — **required for `custom`**. Called when `exists` returns 1.
- `version(name)` — return version string.
- `post(name)` — post-install setup. Runs after any change; a failed post
  stays pending and is retried on the next update.
- `uninstall(name)` — **optional**. Called by `shdeps prune` when removing
  an orphaned dep (any method). For custom deps, this is the only cleanup.
  For other methods, runs before the built-in cleanup — use it to reverse
  what `post()` created (symlinks, config files). A failed or unsourceable
  hook keeps the manifest row and built-in payload for the next prune to
  retry, and prune exits 1; a hook that succeeded is recorded and not rerun.
  A hook that needs sudo when prune has no terminal is deferred instead: no
  `sudo` runs, the row is kept, one warning is printed (without a terminal,
  only when the deferred set changes; `prune --dry-run` always shows it), and
  prune exits 0.

## Hook helper toolkit

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

## Sudo prompt and retry

Initial mutating hook subprocesses are deliberately detached with closed stdin
so cancellation and timeouts can kill their complete session.
`shdeps_require_sudo` must not prompt from that detached child: after a failed
`sudo -n` probe it requests authentication from the attached parent, which
pauses progress, runs the prompt, and retries the hook once. Without a
controlling terminal `update` runs no sudo for the request and does not retry:
it defers a post (kept pending, one warning, not a failure) or fails an
install. Quiet hooks still get `1` from `shdeps_require_sudo` (fallbacks keep
working) but leave a note, so a quiet post or uninstall that then fails without
a terminal is deferred the same way. The authenticated
retry must keep closed stdin and its own killable process group but remain in the parent's session so
terminal-scoped sudo timestamps and subsequent direct `sudo` commands work.
Keep quiet mode noninteractive, preserve the direct helper's normal prompt
behavior outside hooks, and never prompt for an `install()` that was skipped
because `exists()` already succeeded.
JSONL progress adapters that render on the controlling terminal can set
`SHDEPS_PROGRESS_PROMPT_ACK` to a private FIFO. Shdeps opens its nonblocking
read end before it flushes the `prompt` event, then waits up to five seconds for
an exact `ready\n` token before writing the visible prompt status to `/dev/tty`
and invoking sudo. The consumer suspends its display, opens the FIFO
write-only/nonblocking, and writes the token; the exact token alone completes
the acknowledgement, so the writer may stay open and closing is optional.
`ENXIO` means there is no live acknowledgement reader. Without the variable, JSONL progress remains
standalone-compatible and does not wait.
Hooks are trusted same-user code, but the request file still requires a private,
parent-created regular file and a no-follow open on Unix; do not weaken that
defense or treat an exit status alone as a prompt request.
The retry restarts the hook function, so hooks that need sudo must call
`shdeps_require_sudo` before any side effect. If an install attempt changes its
`exists()` result before requesting sudo, the retry fails closed instead of
silently accepting the partial installation or running it twice.
