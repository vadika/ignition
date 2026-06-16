#!/usr/bin/env bash
# Create the warm-base snapshot for the disposable browser: cold-boot the browser
# rootfs (overlay root via init=/sbin/overlay-init), wait for the guest to print
# BROWSER_READY on the serial console, then snapshot it as "browser-base" by
# sending Ctrl-A s to boot, and quit with Ctrl-A x.
#
# This is a ONE-TIME step. After it, use scripts/disposable-browser.sh.
#
# MANUAL EQUIVALENT (if you prefer to eyeball readiness yourself):
#   sudo target/debug/boot --gui --net --mem 2048 --track-dirty \
#        --append "ro init=/sbin/overlay-init" kimage/out/Image kimage/out/rootfs-browser.ext4
#   ...watch the window paint the homepage, then press Ctrl-A s, name it browser-base,
#   then Ctrl-A x.
#
# usage: sudo make-browser-base.sh [snapshot-name] [kernel] [rootfs]
set -euo pipefail

# vCPU count baked into the warm-base. Accept a leading `--smp N` flag (survives
# sudo, unlike an env var) or the SMP env; default 2.
SMP="${SMP:-2}"
if [ "${1:-}" = "--smp" ]; then SMP="${2:?--smp needs a number}"; shift 2; fi

NAME="${1:-browser-base}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
KERNEL="${2:-$ROOT/kimage/out/Image}"
ROOTFS="${3:-$ROOT/kimage/out/rootfs-browser.ext4}"
BOOT="$ROOT/target/debug/boot"

[ -x "$BOOT" ] || { echo "boot not built/signed: $BOOT" >&2; exit 1; }
[ -f "$KERNEL" ] || { echo "kernel not found: $KERNEL" >&2; exit 1; }
[ -f "$ROOTFS" ] || { echo "rootfs not found: $ROOTFS" >&2; exit 1; }

# CTRL-A is 0x01. Drive boot's stdin through a FIFO so we can inject the snapshot
# and quit keystrokes after we see BROWSER_READY on its output.
fifo="$(mktemp -u)"; mkfifo "$fifo"
cleanup() { rm -f "$fifo"; [ -n "${boot_pid:-}" ] && kill "$boot_pid" 2>/dev/null || true; }
trap cleanup EXIT INT TERM

# Hold the FIFO open on fd 3 so boot does not see EOF. Open read-WRITE (3<>):
# a plain write-open (3>) blocks until a reader appears, but our reader (boot,
# launched below) opens it only after this line, so 3> would deadlock at start.
# 3<> returns immediately on a FIFO and keeps the write side held open.
exec 3<>"$fifo"

echo "cold-booting browser rootfs to create snapshot '$NAME' ..."
# Boot reads stdin from the FIFO; its serial output goes through a reader that
# watches for BROWSER_READY (snapshot trigger) or BROWSER_TIMEOUT (abort).
"$BOOT" --gui --net --smp "$SMP" --mem 2048 --track-dirty \
  --append "ro init=/sbin/overlay-init" --name "$NAME" \
  "$KERNEL" "$ROOTFS" <"$fifo" 2>&1 | (
    while IFS= read -r line; do
      echo "$line"
      case "$line" in
        *BROWSER_TIMEOUT*)
          echo ">> guest never reported the browser ready; aborting" >&2
          printf '\001x' >&3
          exit 1
          ;;
        *BROWSER_READY*)
          echo ">> guest ready; snapshotting as '$NAME'"
          printf '\001s' >&3
          ;;
        *"[snapshot]"*written*)
          # Snapshot WRITE completed (e.g. "[snapshot] full 'NAME' written to ...").
          # Distinct from "[snapshot requested]", which prints immediately on Ctrl-A s
          # before the write finishes — do not quit on that.
          sleep 1
          printf '\001x' >&3
          ;;
      esac
    done
  ) &
boot_pid=$!
wait "$boot_pid"
echo "done. snapshot '$NAME' written. Run: scripts/disposable-browser.sh $NAME"
