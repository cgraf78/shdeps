#!/usr/bin/env bash
# Record the shdeps demo GIF.
#
# Requirements: asciinema, agg (brew install asciinema agg)
# Usage: ./demo/record.sh
#
# Must be run inside a real TTY (not a subshell or pipe).
# The resulting demo.gif is written to the repo root.

set -euo pipefail

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT

# -- Check dependencies --
for cmd in asciinema agg shdeps; do
  command -v "$cmd" >/dev/null || { echo "error: $cmd not found" >&2; exit 1; }
done

# -- Write demo config --
mkdir -p "$WORK_DIR"/{conf,state,install,bin}
cat > "$WORK_DIR/conf/deps.conf" << 'CONF'
# name          method    cmd       cmd_alt   source
jq              pkg
fzf             pkg
bat             pkg       bat       batcat
ripgrep         pkg       rg
neovim          binary    nvim      -         neovim/neovim
lazygit         binary    -         -         jesseduffield/lazygit
shfmt           binary    -         -         mvdan/sh
tpm             git       -         -         tmux-plugins/tpm
CONF

# -- Write the demo script --
cat > "$WORK_DIR/demo.sh" << 'DEMO'
#!/usr/bin/env bash
set -e

type_cmd() {
  local cmd="$1"
  for ((i = 0; i < ${#cmd}; i++)); do
    printf '%s' "${cmd:$i:1}"
    sleep 0.04
  done
  sleep 0.4
  echo
}

prompt() { printf '\033[1;34m$\033[0m '; }
pause()  { sleep "${1:-1.5}"; }

clear
pause 0.3

# -- Show config --
prompt
type_cmd "cat ~/.config/shdeps/deps.conf"
printf '\033[2m# name          method    cmd       source\033[0m\n'
echo "jq              pkg"
echo "fzf             pkg"
echo "bat             pkg       bat       batcat"
echo "ripgrep         pkg       rg"
echo "neovim          binary    nvim      neovim/neovim"
echo "lazygit         binary              jesseduffield/lazygit"
echo "shfmt           binary              mvdan/sh"
echo "tpm             git                 tmux-plugins/tpm"
pause 3

# -- Update (fresh install) --
echo
prompt
type_cmd "shdeps update"
shdeps update 2>&1
pause 5

# -- List deps --
prompt
type_cmd "shdeps list"
shdeps list 2>&1
pause 4

prompt
pause 1
DEMO
chmod +x "$WORK_DIR/demo.sh"

# -- Record --
export SHDEPS_CONF_DIR="$WORK_DIR/conf"
export SHDEPS_STATE_DIR="$WORK_DIR/state"
export SHDEPS_INSTALL_DIR="$WORK_DIR/install"
export SHDEPS_BIN_DIR="$WORK_DIR/bin"
export SHDEPS_GIT_DEV_DIR="$WORK_DIR/nodev"

CAST="$WORK_DIR/demo.cast"
GIF="$REPO_DIR/demo/demo.gif"

echo "==> Recording demo..."
stty cols 80 rows 40 2>/dev/null || true
asciinema rec "$CAST" --command "$WORK_DIR/demo.sh" --overwrite

echo "==> Converting to GIF..."
agg "$CAST" "$GIF" --font-size 16 --theme monokai

echo "==> Done: $GIF ($(du -h "$GIF" | cut -f1))"
