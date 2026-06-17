#!/usr/bin/env bash
# Create the warm-base snapshot for the MCP server: cold-boot the tools rootfs
# (overlay root), wait for TOOLS_READY on the serial console, snapshot as
# "tools-base" via Ctrl-A s, then quit with Ctrl-A x. One-time step.
# usage: make-tools-base.sh [snapshot-name] [kernel] [rootfs] [store]
set -euo pipefail

NAME="${1:-tools-base}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
KERNEL="${2:-$ROOT/kimage/out/Image}"
ROOTFS="${3:-$ROOT/kimage/out/rootfs-tools.ext4}"
STORE="${4:-$ROOT/mcp-store}"
BOOT="$ROOT/target/debug/boot"

[ -x "$BOOT" ] || { echo "boot not built/signed: $BOOT" >&2; exit 1; }
[ -f "$KERNEL" ] || { echo "kernel not found: $KERNEL" >&2; exit 1; }
[ -f "$ROOTFS" ] || { echo "rootfs not found: $ROOTFS" >&2; exit 1; }

fifo="$(mktemp -u)"; mkfifo "$fifo"
cleanup() { rm -f "$fifo"; [ -n "${boot_pid:-}" ] && kill "$boot_pid" 2>/dev/null || true; }
trap cleanup EXIT INT TERM
exec 3<>"$fifo"

echo "cold-booting tools rootfs to create snapshot '$NAME' ..."
"$BOOT" --mem 1024 --vsock-uds /tmp/ign-toolsbase.sock --store "$STORE" \
  --name "$NAME" --force --append "ro init=/sbin/overlay-init" \
  "$KERNEL" "$ROOTFS" <"$fifo" 2>&1 | (
    while IFS= read -r line; do
      echo "$line"
      case "$line" in
        *TOOLS_TIMEOUT*)
          echo ">> guest never reported ready; aborting" >&2
          printf '\001x' >&3; exit 1 ;;
        *TOOLS_READY*)
          echo ">> guest ready; snapshotting as '$NAME'"
          printf '\001s' >&3 ;;
        *"[snapshot]"*written*)
          sleep 1; printf '\001x' >&3 ;;
      esac
    done
  ) &
boot_pid=$!
wait "$boot_pid"
echo "done. snapshot '$NAME' written to $STORE."
