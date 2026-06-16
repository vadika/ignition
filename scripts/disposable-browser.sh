#!/usr/bin/env bash
# Launch disposable Firefox-kiosk microVMs restored from a warm-base snapshot.
# Each clone is an independent `boot --gui --net --mem 1024 --track-dirty
# --restore <base>` process: its own window, its own CoW instance dir (keyed by
# pid), its own MAC/IP. Ctrl-A r inside a clone snaps it back to the warm
# homepage; closing the window tears that clone down. The base snapshot is never
# mutated.
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

pids=()
cleanup() {
  for p in "${pids[@]:-}"; do
    kill "$p" 2>/dev/null || true
  done
}
trap cleanup EXIT INT TERM

echo "launching $N disposable browser(s) from base '$BASE'"
for i in $(seq 1 "$N"); do
  "$BOOT" --gui --net --mem 1024 --track-dirty --restore "$BASE" "$@" &
  pid=$!
  pids+=("$pid")
  echo "  browser $i: pid $pid"
done
echo "all launched; Ctrl-A r resets a browser in place; Ctrl-C tears them all down"
wait
