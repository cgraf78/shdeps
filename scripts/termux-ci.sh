#!/data/data/com.termux/files/usr/bin/bash
set -euo pipefail

# The standard matrix owns the full test suite. This job executes the
# NDK-built Android binary inside Termux and verifies Android package policy.
binary=.termux-ci/shdeps
pidfd_signal=.termux-ci/pidfd-signal
[[ -x "$pidfd_signal" ]]

fixture=$(mktemp -d)
shdeps_pid=
child_pid=
descendant_pid=
shdeps_identity=
child_identity=
descendant_identity=
helper_probe_identity=

# Return PID:starttime from one procfs stat record. The start time pins a PID to
# the process generation observed by this test, so cleanup never signals a
# later unrelated process that reused the same number.
process_identity() {
  local proc_root=$1 pid=$2 stat tail start
  [[ "$pid" =~ ^[1-9][0-9]*$ ]] || return 1
  IFS= read -r stat <"$proc_root/$pid/stat" || return 1
  tail=${stat##*) }
  local -a fields
  read -r -a fields <<<"$tail"
  ((${#fields[@]} >= 20)) || return 1
  start=${fields[19]}
  [[ "$start" =~ ^[0-9]+$ ]] || return 1
  printf '%s:%s\n' "$pid" "$start"
}

process_identity_matches() {
  local proc_root=$1 identity=$2 pid
  [[ "$identity" == *:* ]] || return 1
  pid=${identity%%:*}
  [[ "$(process_identity "$proc_root" "$pid" 2>/dev/null || true)" == "$identity" ]]
}

signal_owned_process() {
  local signal=$1 identity=$2
  "$pidfd_signal" "$identity" "$signal"
}

cleanup() {
  local identity
  for identity in \
    "$shdeps_identity" "$child_identity" "$descendant_identity" "$helper_probe_identity"; do
    if [[ -n "$identity" ]]; then
      signal_owned_process 9 "$identity" 2>/dev/null || true
    fi
  done
  if [[ -n "$shdeps_pid" ]]; then
    wait "$shdeps_pid" 2>/dev/null || true
  fi
  rm -rf "$fixture"
}
trap cleanup EXIT
mkdir -p "$fixture/conf" "$fixture/state"

# Exercise the exact delivery seam before the cancellation scenario. Opening a
# pidfd before checking the recorded start time makes a stale identity harmless
# even if the numeric PID was reused just before delivery.
sleep 30 &
helper_probe_pid=$!
helper_probe_identity=$(process_identity /proc "$helper_probe_pid")
fixture_identity=${helper_probe_identity%:*}:$((${helper_probe_identity#*:} + 1))
if "$pidfd_signal" "$fixture_identity" 0; then
  printf 'pidfd helper accepted a stale process identity\n' >&2
  exit 1
fi
process_identity_matches /proc "$helper_probe_identity"
signal_owned_process 9 "$helper_probe_identity"
wait "$helper_probe_pid" 2>/dev/null || true
helper_probe_identity=
printf '%s\n' \
  'termux-runtime pkg android:bash,apt:shdeps-ci-missing android:bash,apt:shdeps-ci-missing os:android' \
  >"$fixture/conf/runtime.conf"

output=$(
  SHDEPS_CONF_DIR="$fixture/conf" \
    SHDEPS_STATE_DIR="$fixture/state" \
    "$binary" list
)
printf '%s\n' "$output"
printf '%s\n' "$output" |
  grep -Eq '^termux-runtime[[:space:]]+pkg[[:space:]]+installed'

# Exercise the Android process APIs, signal latch, descendant attribution, and
# conventional status in the real Termux runtime. Unit tests cannot validate
# Bionic's procfs/process behavior or the transported release binary.
mkdir -p "$fixture/cancel-conf/hooks.d" "$fixture/cancel-state"
printf '%s\n' 'cancel-probe custom' >"$fixture/cancel-conf/deps.conf"
cat >"$fixture/cancel-conf/hooks.d/cancel-probe.sh" <<'HOOK'
exists() { return 1; }
install() {
  trap '' HUP INT QUIT TERM
  sh -c '
    trap "" HUP INT QUIT TERM
    printf "%s\n" "$$" >"$SHDEPS_STATE_DIR/descendant.pid"
    while :; do
      printf x >>"$SHDEPS_STATE_DIR/mutations"
      sleep 0.02
    done
  ' &
  printf '%s\n' "$$" >"$SHDEPS_STATE_DIR/child.pid"
  wait
}
HOOK

SHDEPS_CONF_DIR="$fixture/cancel-conf" \
  SHDEPS_STATE_DIR="$fixture/cancel-state" \
  SHDEPS_INSTALL_DIR="$fixture/cancel-install" \
  SHDEPS_BIN_DIR="$fixture/cancel-bin" \
  SHDEPS_HOOK_TIMEOUT_SECS=30 \
  "$binary" update >"$fixture/cancel.stdout" 2>"$fixture/cancel.stderr" &
shdeps_pid=$!
deadline=$((SECONDS + 10))
while [[ -z "$shdeps_identity" ]]; do
  shdeps_identity=$(process_identity /proc "$shdeps_pid" 2>/dev/null || true)
  [[ $SECONDS -lt $deadline ]]
  sleep 0.02
done

deadline=$((SECONDS + 10))
while :; do
  child_pid=$(sed -n '1p' "$fixture/cancel-state/child.pid" 2>/dev/null || true)
  descendant_pid=$(sed -n '1p' "$fixture/cancel-state/descendant.pid" 2>/dev/null || true)
  if [[ "$child_pid" =~ ^[1-9][0-9]*$ && "$descendant_pid" =~ ^[1-9][0-9]*$ ]]; then
    child_identity=$(process_identity /proc "$child_pid" 2>/dev/null || true)
    descendant_identity=$(process_identity /proc "$descendant_pid" 2>/dev/null || true)
  fi
  if [[ -n "$child_identity" && -n "$descendant_identity" ]]; then
    break
  fi
  [[ $SECONDS -lt $deadline ]]
  sleep 0.02
done

signal_owned_process 15 "$shdeps_identity"
set +e
wait "$shdeps_pid"
cancel_status=$?
set -e
shdeps_pid=
shdeps_identity=
[[ "$cancel_status" == 143 ]]

deadline=$((SECONDS + 5))
while process_identity_matches /proc "$child_identity" || process_identity_matches /proc "$descendant_identity"; do
  [[ $SECONDS -lt $deadline ]]
  sleep 0.02
done
if process_identity_matches /proc "$child_identity"; then
  exit 1
fi
if process_identity_matches /proc "$descendant_identity"; then
  exit 1
fi

mutation_size=$(wc -c <"$fixture/cancel-state/mutations")
sleep 0.2
[[ "$(wc -c <"$fixture/cancel-state/mutations")" == "$mutation_size" ]]
child_pid=
descendant_pid=
child_identity=
descendant_identity=
