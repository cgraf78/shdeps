# Dep Removal & Pruning

## Summary

Add a manifest to track what shdeps has installed, surface orphaned deps
(installed but no longer in config), and provide `shdeps prune` to clean
them up. The config remains the source of truth for what *should* be
installed; the manifest tracks what *is* installed.

## Manifest

**File:** `$SHDEPS_STATE_DIR/manifest`

**Format:** one line per installed dep, pipe-delimited:

```
name|method|cmd|install_path
```

Fields:
- `name` — dep name from config
- `method` — `pkg`, `git`, `binary`, `custom`
- `cmd` — the command name (for binary/pkg lookup)
- `install_path` — where artifacts live, relative to `$HOME`
  - `git`: the `_dir` value (e.g. `.local/share/ds`)
  - `binary`: `.local/bin/$cmd` (plus `.local/share/$name` if archive)
  - `pkg`: empty (managed by system package manager)
  - `custom`: empty (managed by hook)

**No new dependencies.** Read/write with bash builtins — `grep`, `sed`,
`printf`. Same pipe-delimited convention used throughout shdeps.

**Lifecycle:**
- Written/updated by `_shdeps_install_dep` after successful install
- Read by `_shdeps_update` to detect orphans (manifest entries not in config)
- Lines removed by `shdeps prune` after successful cleanup

## Orphan Detection

At the end of `shdeps update`, after all installs and hooks:

1. Load manifest entries
2. Compare against loaded config dep names
3. If orphans exist, print a notice:

```
==> 2 orphaned deps (removed from config but still installed):
  neovim (binary), codex (binary)
  Run: shdeps prune
```

No automatic removal — informational only.

## `shdeps prune`

**Behavior:** remove artifacts for orphaned deps.

**Interactive by default:**
```
The following deps are no longer in config:
  neovim (binary) — ~/.local/bin/nvim, ~/.local/share/neovim/
  codex (binary) — ~/.local/bin/codex
  nerd-fonts (custom)
Remove? [y/N]
```

**Flags:**
- `-y` — skip confirmation
- `--dry-run` — show what would be removed without removing

**Removal logic per method:**

| Method | Action |
|--------|--------|
| `pkg` | Warn only: "pkg dep 'jq' should be removed manually via brew/apt/dnf" |
| `git` | Remove `$HOME/$install_path`. If it's a symlink (dev clone), remove only the symlink. Remove `~/.local/bin/$name` symlink if present. Remove state stamps. |
| `binary` | Remove `~/.local/bin/$cmd`. Remove `~/.local/share/$name/` if it exists. Remove state stamps. |
| `custom` | Source `$hooks_dir/$name.sh`, call `uninstall()` if defined. If hook file missing or `uninstall()` not defined, warn. Remove state stamps. |

After successful removal, delete the manifest line.

## Hook Contract Update

Hook files (`hooks.d/$name.sh`) gain one optional function:

```bash
# Called by `shdeps prune` when dep is orphaned.
# Should reverse what install() did.
uninstall() {
  local name="$1"
  rm -rf "$HOME/.local/share/fonts/NerdFonts"
  fc-cache -f 2>/dev/null || true
}
```

If `uninstall()` is not defined, shdeps warns:
"custom dep '$name' has no uninstall() hook — manual cleanup may be needed"

## Public API Additions

```bash
shdeps_prune()    { _shdeps_prune "$@"; }
```

## CLI Additions

```
Commands:
  prune           Remove orphaned deps no longer in config

Options (prune):
  -y              Skip confirmation prompt
  --dry-run       Show what would be removed without removing
```

## Implementation Steps

1. **Manifest read/write helpers** in `shdeps.sh`
   - `_shdeps_manifest_path` — returns manifest file path
   - `_shdeps_manifest_read` — load manifest into associative array
   - `_shdeps_manifest_upsert` — add/update a manifest entry
   - `_shdeps_manifest_remove` — remove a manifest entry

2. **Write manifest during install** — call `_shdeps_manifest_upsert`
   at the end of each successful install in `_shdeps_install_dep`

3. **Orphan detection in `_shdeps_update`** — after post-hooks, diff
   manifest against config, print notice if orphans exist

4. **`_shdeps_prune` implementation** — removal logic per method

5. **CLI wiring** — add `prune` subcommand to `bin/shdeps`, parse
   `-y` and `--dry-run` flags

6. **Tests** — manifest CRUD, orphan detection, prune per method,
   hook uninstall contract, dry-run, confirmation flow

7. **Update AGENTS.md** — document manifest, prune command, hook contract
