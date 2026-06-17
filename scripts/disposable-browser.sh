#!/usr/bin/env bash
# Launch disposable Firefox-kiosk microVMs restored from a warm-base snapshot.
# Each clone is an independent `boot --gui --net --mem 2048
# --restore <base>` process: its own window, its own CoW instance dir (keyed by
# pid), its own MAC/IP. Ctrl+Alt+R does a COLD reset — the clone exits with code
# 42 and this script re-restores it from the snapshot (a fresh window at the warm
# homepage). Ctrl+Alt+X closes the window (exit 0) and ends that clone. The base
# snapshot is never mutated.
#
# usage: disposable-browser.sh [-n N] [snapshot-name] [extra boot args...]
#   e.g. disposable-browser.sh                       # 1 clone of browser-base
#        disposable-browser.sh -n 3                  # 3 clones
#        sudo disposable-browser.sh -n 3 browser-base   # 3 networked clones
# --net needs sudo (vmnet shared/NAT). --net is added by default; pass extra
# boot args after the snapshot name to override store etc.
set -euo pipefail

N=1
if [ "${1:-}" = "-n" ]; then
  N="${2:-}"
  case "$N" in '' | *[!0-9]*) echo "usage: $0 [-n N] [snapshot-name] [extra boot args...]" >&2; exit 2 ;; esac
  [ "$N" -ge 1 ] || { echo "N must be >= 1" >&2; exit 2; }
  shift 2
fi

BASE="${1:-browser-base}"
[ $# -ge 1 ] && shift || true

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BOOT="$ROOT/target/debug/boot"
[ -x "$BOOT" ] || {
  echo "boot binary not found or not built: $BOOT" >&2
  echo "build + sign it first: cargo build -p ignition-spike --bin boot && ./scripts/sign.sh target/debug/boot" >&2
  exit 1
}

RESET_EXIT=42   # boot's exit code for Ctrl+Alt+R (cold reset -> relaunch); see display_sink.rs

pids=()
cleanup() {
  for p in "${pids[@]:-}"; do
    kill "$p" 2>/dev/null || true
  done
}
trap cleanup EXIT INT TERM

# One supervisor subshell per clone: run boot, and on the cold-reset exit code
# re-restore it (fresh window at the warm homepage); any other exit ends the clone.
# The subshell traps TERM/INT to kill its current boot child so Ctrl-C on the
# launcher tears everything down (boot is the subshell's child, not a direct one).
clone_loop() {
  local n="$1"; shift
  local bpid=""
  trap 'kill "$bpid" 2>/dev/null || true; exit 0' TERM INT
  while :; do
    # Per-clone vsock control socket: lets the VMM push a fresh CRNG seed to
    # this clone after restore (vmid), so sibling clones do not share RNG state.
    "$BOOT" --gui --net --mem 2048 --vsock-uds "/tmp/ign-vmid-$n-$$" --restore "$BASE" "$@" &
    bpid=$!
    # `wait` returns boot's exit code; capture it WITHOUT tripping `set -e` (a bare
    # non-zero `wait` would abort the subshell before we can relaunch).
    local rc=0
    wait "$bpid" || rc=$?
    if [ "$rc" -eq "$RESET_EXIT" ]; then
      echo "  browser $n: cold reset -> relaunching"
      continue
    fi
    break
  done
}

echo "launching $N disposable browser(s) from base '$BASE'"
for i in $(seq 1 "$N"); do
  clone_loop "$i" "$@" &
  pid=$!
  pids+=("$pid")
  echo "  browser $i: pid $pid"
done
echo "all launched; Ctrl+Alt+R = cold reset (relaunch), Ctrl+Alt+X = close; Ctrl-C tears all down"
wait
