#!/usr/bin/env bash
# Fan out N GUI clones from one warm-base snapshot. Each clone is an independent
# `boot --gui --restore <base>` process: its own macOS window, its own CoW
# instance dir (keyed by pid), its own MAC/IP when --net is passed. The immutable
# base snapshot is never mutated.
#
# usage: fanout-gui.sh <N> <snapshot-name> [extra boot args...]
#   e.g. fanout-gui.sh 3 warm-base
#        fanout-gui.sh 4 warm-base --store ./vmstore
set -euo pipefail

usage() {
  echo "usage: $0 <N> <snapshot-name> [extra boot args...]" >&2
  exit 2
}

[ $# -ge 2 ] || usage
N="$1"
BASE="$2"
shift 2
case "$N" in
  '' | *[!0-9]*) usage ;;
esac
[ "$N" -ge 1 ] || usage

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

echo "fanning out $N GUI clone(s) from base '$BASE'"
for i in $(seq 1 "$N"); do
  "$BOOT" --gui --restore "$BASE" "$@" &
  pid=$!
  pids+=("$pid")
  echo "  clone $i: pid $pid"
done
echo "all clones launched; Ctrl-C tears them all down"
wait
