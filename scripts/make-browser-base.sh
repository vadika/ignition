#!/usr/bin/env bash
# Create the warm-base snapshot for the disposable browser: cold-boot the browser
# rootfs (overlay root via init=/sbin/overlay-init), wait for the guest to print
# BROWSER_READY on the serial console, then snapshot it as "browser-base" by
# sending Ctrl-A s to boot, and quit with Ctrl-A x.
#
# This is a ONE-TIME step. After it, use scripts/disposable-browser.sh.
#
# MANUAL EQUIVALENT (if you prefer to eyeball readiness yourself):
#   sudo target/debug/boot --gui --net --mem 2048 \
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
# Snapshot store. Default ./vmstore (CWD-relative, matches boot). Override with a
# user-writable path (the disposable-browser app passes its own store) to avoid a
# root-owned ./vmstore left by earlier sudo (socket_vmnet) runs.
STORE="${STORE:-./vmstore}"

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
# NET=1 (default) attaches socket_vmnet so the homepage loads during warm-up.
# NET=0 builds a net-free base: required for the disposable-browser app (zero-setup,
# no socket_vmnet daemon) and more correct for the quiescent-snapshot rule (no live
# TLS at snapshot) -- pair it with a rootfs built HOMEPAGE=about:blank (see
# build-rootfs-browser.sh) so Firefox idles with no pending connections; each cloned
# child then gets gvproxy NAT + an injected URL.
# Net mode for the warm base. The base MUST include the virtio-net DEVICE (so a
# restored clone can attach a fresh backend + re-DHCP); a net-free base has no
# eth0 to restore. Quiescence (no live TLS) comes from sitting on about:blank, NOT
# from omitting the device.
#   NET_SOCKET=<path> : --net over a gvproxy qemu socket (no daemon/sudo) -- what
#                       the disposable-browser app uses; device present, DHCP lease.
#   NET=1 (default)   : --net over socket_vmnet (needs the daemon).
#   NET=0             : no net device (only for non-networked bases).
NET="${NET:-1}"
NET_SOCKET="${NET_SOCKET:-}"
if [ -n "$NET_SOCKET" ]; then
  NET_FLAG="--net --net-socket $NET_SOCKET"
elif [ "$NET" = 0 ]; then
  NET_FLAG=""
else
  NET_FLAG="--net"
fi
# Boot WITH a vsock device (--vsock-uds) so the guest vsock listeners (vmid reseed
# on 9000, URL injection on 7777) bind during warm-up and are captured in the
# snapshot; otherwise the restored clone has no vsock device and the host cannot
# reach those ports. The base-build UDS itself is throwaway.
VSOCK_UDS="${TMPDIR:-/tmp}/browser-base-build.vsock"
rm -f "$VSOCK_UDS"
# Boot reads stdin from the FIFO; its serial output goes through a reader that
# watches for BROWSER_READY (snapshot trigger) or BROWSER_TIMEOUT (abort).
"$BOOT" --gui $NET_FLAG --smp "$SMP" --mem 2048 \
  --append "ro init=/sbin/overlay-init" --store "$STORE" --name "$NAME" \
  --vsock-uds "$VSOCK_UDS" \
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
