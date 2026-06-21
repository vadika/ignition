#!/usr/bin/env bash
# Build a disposable-browser aarch64 rootfs: base (busybox+openrc, getty, net,
# boot-timer) PLUS a cage Wayland kiosk running Firefox ESR. The root is an
# overlay (tmpfs upper over the RO ext4 lower) set up by /sbin/overlay-init at
# cold boot, so every guest write lands in RAM and the disk never diverges.
# Rendering uses Mesa llvmpipe (software GL). Output: ~/kbuild/out/rootfs-browser.ext4.
set -euo pipefail

OUT="$HOME/kbuild/out"
STAGE="$HOME/kbuild"
mkdir -p "$OUT"
TAR="$STAGE/rootfs-browser.tar"

HOMEPAGE="${HOMEPAGE:-https://vadika.github.io/ignition-browser/}"

# 1. Provision the browser rootfs inside an arm64 alpine container.
docker rm -f fcroot_browser_build >/dev/null 2>&1 || true
docker run --platform linux/arm64 --name fcroot_browser_build \
  -e HOMEPAGE="$HOMEPAGE" \
  -v "$(cd "$(dirname "$0")" && pwd)/devmem.c:/devmem.c:ro" \
  -v "$(cd "$(dirname "$0")" && pwd)/vmid-reseed.c:/vmid-reseed.c:ro" \
  alpine:3.21 sh -euxc '
  # --- base provisioning (kept in sync with build-rootfs.sh) ---
  apk add --no-cache openrc util-linux ifupdown-ng socat

  apk add --no-cache --virtual .build gcc musl-dev linux-headers
  gcc -O2 -static /devmem.c -o /usr/bin/devmem
  gcc -O2 -static /vmid-reseed.c -o /usr/bin/vmid-reseed
  apk del .build

  ln -sf agetty /etc/init.d/agetty.ttyS0
  echo ttyS0 > /etc/securetty
  rc-update add agetty.ttyS0 default
  # NOTE: no agetty on tty1 in the GUI rootfs — cage owns the framebuffer VT, and a
  # getty there competes with cage for the keyboard (events go to the VT, not
  # libinput). Serial (ttyS0) stays as the debug login. Alpine ALSO spawns gettys on
  # tty1..tty6 from busybox /etc/inittab; strip those VT gettys so none grabs the
  # keyboard from cage (ttyS0 line, if present, is kept — [0-9] does not match "S").
  sed -i "/^tty[0-9].*getty/d" /etc/inittab
  rc-update add devfs boot
  rc-update add procfs boot
  rc-update add sysfs boot

  passwd -d root || true

  # NOTE: the base rootfs symlinks /dev/tty -> /dev/ttyS0 for serial-console programs.
  # The GUI rootfs MUST NOT: foot runs its app on a pty and the app opens /dev/tty as
  # its controlling terminal; if /dev/tty is a symlink to the serial port the app gets
  # the wrong device (the cannot-access-tty error, no echo). Leave /dev/tty as the real
  # kernel ctty node (c 5 0) from devtmpfs so it resolves to the apps pty.

  mkdir -p /etc/network /etc/local.d
  printf "auto lo\niface lo inet loopback\n\nauto eth0\niface eth0 inet dhcp\n" > /etc/network/interfaces
  printf "#!/bin/sh\nifup -a\n" > /etc/local.d/network.start
  chmod +x /etc/local.d/network.start
  printf "#!/bin/sh\ndevmem 0x091FF000 8 123\n" > /etc/local.d/boottime.start
  chmod +x /etc/local.d/boottime.start
  # on-restore: runs once per snapshot restore (the host pushes the CRNG seed over
  # vsock 9000 on every restore). Reseed the CRNG, THEN force a network re-init. A
  # restore attaches a FRESH gvproxy with a new MAC/lease, so the restored guest must
  # rebind virtio_net and re-DHCP to get a working address+route. The carrier-flap
  # poller (netwatch) is best-effort and was not firing in the app launcher; this hook
  # fires reliably on every restore. Net state is echoed to ttyS0 (session log).
  cat > /usr/bin/on-restore <<'"'"'ONREOF'"'"'
#!/bin/sh
# Runs once per restore (host pushes the CRNG seed over vsock 9000). Reseed, then bring the
# network back STATICALLY instead of re-DHCP. A re-DHCP (ifdown/ifup -> udhcpc DISCOVER/
# OFFER/REQUEST/ACK) took ~5s after restore and gated the first page load. Each session has
# its own fresh, single-client gvproxy on a deterministic 192.168.127.0/24 (gateway+DNS
# .1, client .3 -- the address DHCP always handed out), so we can just assert that config
# and skip the handshake. ip addr/route replace is idempotent (safe if netwatch also runs).
# Do NOT unbind/bind virtio_net (that destroys eth0 here). $? values are ignored.
/usr/bin/vmid-reseed
ip link set eth0 up 2>/dev/null
ip addr replace 192.168.127.3/24 dev eth0 2>/dev/null
ip route replace default via 192.168.127.1 dev eth0 2>/dev/null
echo "nameserver 192.168.127.1" > /etc/resolv.conf
echo "[on-restore] net static-config done" > /dev/ttyS0 2>&1
ip -o addr show eth0 > /dev/ttyS0 2>&1
ONREOF
  chmod +x /usr/bin/on-restore
  # vmid: host-pushed CRNG reseed + net re-init on snapshot restore.
  printf "#!/bin/sh\nsocat VSOCK-LISTEN:9000,fork EXEC:/usr/bin/on-restore &\n" > /etc/local.d/vmid.start
  chmod +x /etc/local.d/vmid.start

  # URL injection: the host sends one validated URL line over vsock port 7777;
  # open-url hands it to the already-running kiosk Firefox by argv (NEVER sh -c the
  # URL). Same socat VSOCK-LISTEN pattern as the vmid listener above.
  # open-url just records the validated URL (argv-safe; never sh -c). The
  # kiosk-loop (cage command) picks it up and hands it to the running Firefox via its
  # DBus remote (cage now runs under dbus-run-session, so the second `firefox-esr <url>`
  # finds the running instance instead of hitting the profile lock). Navigation is a
  # remote handoff to the warm instance — near-instant, no kill/relaunch.
  cat > /usr/bin/open-url <<'"'"'URLEOF'"'"'
#!/bin/sh
IFS= read -r url
echo "[open-url] got: $url" > /dev/ttyS0 2>&1
case "$url" in
  http://*|https://*) ;;
  *) echo "[open-url] rejected: $url" > /dev/ttyS0 2>&1; exit 1 ;;
esac
echo "$url" > /run/openurl.target
echo "[open-url] wrote target: $url" > /dev/ttyS0 2>&1
URLEOF
  chmod +x /usr/bin/open-url
  printf "#!/bin/sh\nsocat VSOCK-LISTEN:7777,fork EXEC:/usr/bin/open-url &\n" > /etc/local.d/openurl.start
  chmod +x /etc/local.d/openurl.start

  # kiosk-loop: cage runs this (under dbus-run-session, not firefox directly). Launches
  # ONE firefox-esr (no --no-remote, so it owns the DBus remote) and keeps it running;
  # when open-url writes a new URL to /run/openurl.target, hands the URL to that running
  # instance via a transient `firefox-esr <url>` remote call (no kill/relaunch). $1 = initial URL.
  cat > /usr/bin/kiosk-loop <<'"'"'KIOSKEOF'"'"'
#!/bin/sh
# The MAIN firefox runs in a subshell that captures its REAL exit code into $DONE. That
# lets us tell:
#   exit 0   = the user closed the window  -> end the disposable session (poweroff)
#   non-zero = crash, Ctrl+Shift+R, or a snapshot-restore that killed firefox GL/Wayland
#              -> relaunch and recover (a restored firefox can die on resume; this is how
#              a New-Browser session comes back instead of powering off instantly).
# Navigation (open-url writes a new URL) is a REMOTE handoff to the running instance via
# a separate, short-lived `firefox-esr <url>` (DBus remote) — no kill, no cold start. That
# remote call exit code is ignored (it is NOT the $DONE-tracked main process).
TARGET=/run/openurl.target
DONE="${XDG_RUNTIME_DIR:-/tmp}/ff-done"   # kiosk-writable; /run itself is root-owned
PROFILES="$HOME/.mozilla/firefox"
url="${1:-about:blank}"
: > "$TARGET"
last=""
start_ff() {
  rm -f "$PROFILES"/*/lock "$PROFILES"/*/.parentlock "$DONE" 2>/dev/null
  echo "[kiosk-loop] launching firefox: $url" > /dev/ttyS0 2>&1
  # Foreground firefox inside the subshell so $? is its real exit code; publish it.
  # No --no-remote: this instance registers on the session bus so later `firefox-esr <url>`
  # calls route to it instead of starting a second (profile-locked) instance.
  ( /usr/bin/firefox-esr "$url" >/dev/null 2>&1; echo $? > "$DONE" ) &
}
navigate() {
  # Hand the URL to the already-running firefox over its DBus remote. Returns fast; its
  # exit code is intentionally discarded (only the main firefox drives $DONE).
  echo "[kiosk-loop] navigating (remote) to: $url" > /dev/ttyS0 2>&1
  /usr/bin/firefox-esr "$url" >/dev/null 2>&1 || true
}
start_ff
while :; do
  cur=$(cat "$TARGET" 2>/dev/null)
  if [ -n "$cur" ] && [ "$cur" != "$last" ]; then
    last="$cur"; url="$cur"
    if pgrep -f /usr/lib/firefox >/dev/null 2>&1; then
      navigate                       # warm instance alive -> remote handoff (fast)
    else
      start_ff                       # no live instance (edge) -> launch on the URL
    fi
  elif [ -f "$DONE" ]; then
    code=$(cat "$DONE" 2>/dev/null); rm -f "$DONE"
    if [ "$code" = 0 ]; then
      echo "[kiosk-loop] firefox closed (exit 0); powering off session" > /dev/ttyS0 2>&1
      doas poweroff -f   # kiosk-loop runs as the kiosk user; reboot(2) needs root
      exit 0
    fi
    echo "[kiosk-loop] firefox exited (code $code); relaunching" > /dev/ttyS0 2>&1
    sleep 1
    start_ff                         # crash/restore recovery -> reopen current $url
  fi
  sleep 0.5
done
KIOSKEOF
  chmod +x /usr/bin/kiosk-loop

  # Net re-init fallback on snapshot restore: a restore bounces the virtio-net link
  # down->up. on-restore (vsock 9000) is the primary path and fires reliably from the app;
  # this carrier poller is a backup for paths that do not push the seed. It now re-asserts
  # the SAME static config as on-restore (no DHCP), so a flap costs ~0 instead of a ~5s
  # re-DHCP. ip addr/route replace is idempotent. Do NOT unbind/bind virtio_net (destroys eth0).
  cat > /etc/local.d/netwatch.start <<'"'"'NETEOF'"'"'
#!/bin/sh
( prev=1
  while :; do
    cur=$(cat /sys/class/net/eth0/carrier 2>/dev/null || echo 0)
    if [ "$prev" = 0 ] && [ "$cur" = 1 ]; then
      # Carrier came back (restore bounces the link). Re-assert the static config.
      ip link set eth0 up 2>/dev/null
      ip addr replace 192.168.127.3/24 dev eth0 2>/dev/null
      ip route replace default via 192.168.127.1 dev eth0 2>/dev/null
      echo "nameserver 192.168.127.1" > /etc/resolv.conf
      sleep 5          # cooldown: ignore the config-induced flap
      prev=1
      continue
    fi
    prev=$cur
    sleep 1
  done ) &
NETEOF
  chmod +x /etc/local.d/netwatch.start
  rc-update add local boot

  # --- GUI layer: cage + foot + seatd over the virtio-gpu/input devices ---
  # cage/foot/seatd live in the alpine community repo; enable it. alpine 3.21 ships
  # cage 0.2.0: on the DRM backend it survives destroying the sole output, so a
  # virtio-gpu connector-cycle re-modesets the kiosk on host window resize. cage
  # 0.1.5 (alpine 3.19) terminates instead, so do not downgrade below 3.21.
  # NB: no apostrophes in this comment (the whole block is inside sh -euxc PROVISION).
  echo "https://dl-cdn.alpinelinux.org/alpine/v3.21/community" >> /etc/apk/repositories
  apk update
  # pixman software path: no mesa/GL. cage pulls wlroots/libinput/wayland/pixman/libdrm.
  # libinput-tools: libinput list-devices / debug-events for bring-up diagnosis.
  # xkeyboard-config: XKB layout data — without it libxkbcommon compiles an empty
  # keymap, so cage focuses the window but keystrokes map to nothing (verified: key
  # events reach the guest, but no characters appear until this is installed).
  # dbus: provides a per-session message bus (via dbus-run-session) so a second
  # `firefox-esr <url>` hands the URL to the ALREADY-RUNNING firefox over its DBus
  # remote instead of cold-launching (~15s). Makes navigation near-instant on the
  # warm snapshot.
  apk add --no-cache cage foot seatd font-terminus libinput-tools xkeyboard-config wev dbus
  apk add --no-cache firefox-esr mesa-dri-gallium mesa-gl ca-certificates font-dejavu

  # Run the GUI stack (cage + firefox) as an unprivileged user, not root: firefox
  # refuses some operations as root and it is poor hygiene. cage opens DRM/input via
  # seatd (libseat), so the kiosk user only needs the seat/video/input/tty groups,
  # not root. First adduser -D gets uid 1000 on alpine (matches XDG_RUNTIME_DIR below).
  adduser -D -h /home/kiosk kiosk
  for g in video input seat tty; do addgroup kiosk "$g" 2>/dev/null || true; done
  # kiosk-loop (running as kiosk) ends the disposable session by powering off, but
  # reboot(2) needs root. Allow kiosk to run ONLY poweroff via doas (Alpine sudo).
  apk add --no-cache doas
  echo "permit nopass kiosk as root cmd poweroff" > /etc/doas.conf

  # Firefox kiosk policy: no first-run, no telemetry, no update checks, set homepage.
  mkdir -p /usr/lib/firefox/distribution
  cat > /usr/lib/firefox/distribution/policies.json <<'"'"'POLEOF'"'"'
{
  "policies": {
    "DisableTelemetry": true,
    "DisableFirefoxStudies": true,
    "DisableAppUpdate": true,
    "DontCheckDefaultBrowser": true,
    "OverrideFirstRunPage": "",
    "OverridePostUpdatePage": "",
    "Homepage": { "URL": "__HOMEPAGE__", "StartPage": "homepage" }
  }
}
POLEOF
  sed -i "s|__HOMEPAGE__|$HOMEPAGE|" /usr/lib/firefox/distribution/policies.json

  # Suppress the one-time "Firefox automatically sends some data to Mozilla" infobar
  # (DisableTelemetry alone does not hide it). autoconfig locks the bypass pref for
  # every profile, so a fresh disposable session never shows it.
  mkdir -p /usr/lib/firefox/defaults/pref
  cat > /usr/lib/firefox/defaults/pref/autoconfig.js <<'"'"'ACEOF'"'"'
pref("general.config.filename", "firefox.cfg");
pref("general.config.obscure_value", 0);
ACEOF
  cat > /usr/lib/firefox/firefox.cfg <<'"'"'CFGEOF'"'"'
// firefox.cfg: the first line is ignored by the autoconfig parser, keep it a comment.
lockPref("datareporting.policy.dataSubmissionPolicyBypassNotification", true);
lockPref("datareporting.healthreport.uploadEnabled", false);
CFGEOF

  # udev (eudev): wlroots libinput discovers /dev/input/event* via udev. Without it
  # cage aborts ("libinput initialization failed, no input devices"). Run at sysinit
  # so input + DRM nodes are enumerated before cage starts.
  apk add --no-cache eudev
  rc-update add udev sysinit
  rc-update add udev-trigger sysinit
  rc-update add udev-settle sysinit

  # seat daemon for cage to open DRM + input devices.
  rc-update add seatd default

  # cage-kiosk service: launch cage(firefox) once a virtio-gpu scanout exists. Runs as
  # root, software renderer, logs to /var/log/cage.log (read via the serial console
  # for debugging). No-ops cleanly when booted without --gui (no /dev/dri/card0).
  cat > /etc/init.d/cage-kiosk <<'"'"'CAGEEOF'"'"'
#!/sbin/openrc-run
description="cage kiosk (firefox) on the virtio-gpu framebuffer"

export HOME=/home/kiosk
export XDG_RUNTIME_DIR=/run/user/1000
export WLR_RENDERER=pixman
export WLR_RENDERER_ALLOW_SOFTWARE=1
# Layout order must match the LAYOUTS array in spike/src/bin/display_sink.rs (group index = position)
export XKB_DEFAULT_LAYOUT="us,ru,de,fr,es,it,ua,pl"
export XKB_DEFAULT_OPTIONS="grp:sclk_toggle"
export LIBGL_ALWAYS_SOFTWARE=1
export GALLIUM_DRIVER=llvmpipe
export MOZ_ENABLE_WAYLAND=1
# NOTE: Firefox on llvmpipe may also need MOZ_WEBRENDER=1 or MOZ_ACCELERATED=0 to
# render reliably; to be confirmed at bring-up.
# NOTE: deliberately NOT setting WLR_LIBINPUT_NO_DEVICES — that flag makes wlroots
# skip enumerating the already-present (cold-boot) input devices and only listen for
# new hotplug uevents, which never fire for devices present before cage starts, so
# cage ends up with no keyboard (no wl_keyboard -> the app never gets focus). Instead
# start_pre waits until libinput can actually see the keyboard, then cage enumerates
# it normally at startup.

# No --kiosk: kiosk mode hides ALL chrome (no address bar/tabs). cage already
# fullscreens the single window, so plain firefox-esr gives a maximized browser
# WITH its normal toolbar and address bar. Disposability comes from the overlay
# root + Ctrl+Alt+R reset, not from kiosk mode.
command="/usr/bin/cage"
# dbus-run-session gives the kiosk session a private message bus so the firefox DBus
# remote works (a later `firefox-esr <url>` reaches the running instance). Without it
# there is no session bus under cage and the remote falls back to a new, profile-locked
# instance.
command_args="-- dbus-run-session -- /usr/bin/kiosk-loop __HOMEPAGE__"
command_background=true
command_user="kiosk:kiosk"
pidfile="/run/cage-kiosk.pid"
output_log="/var/log/cage.log"
error_log="/var/log/cage.log"

depend() {
    need seatd
    after udev-settle udev-trigger udev devfs
}

start_pre() {
    if [ ! -e /dev/dri/card0 ]; then
        ewarn "no /dev/dri/card0 (booted without --gui); not starting cage"
        return 1
    fi
    mkdir -p "$XDG_RUNTIME_DIR"
    chmod 0700 "$XDG_RUNTIME_DIR"
    chown kiosk:kiosk "$XDG_RUNTIME_DIR"
    # start-stop-daemon opens output_log AS the kiosk user, but /var/log is root-owned;
    # pre-create it owned by kiosk or the cage start aborts with Permission denied.
    : > /var/log/cage.log
    chown kiosk:kiosk /var/log/cage.log
    # kiosk-loop (running as kiosk) truncates this at startup; open-url (root) writes
    # it later. Pre-create it owned by kiosk so the unprivileged loop can touch it.
    : > /run/openurl.target
    chown kiosk:kiosk /run/openurl.target
    # Wait until libinput can enumerate the keyboard (udev has finished tagging
    # /dev/input/event*). cage enumerates input once at startup; if it starts before
    # tagging, it gets no keyboard and the app never receives focus.
    i=0
    while [ "$i" -lt 50 ]; do
        if libinput list-devices 2>/dev/null | grep -qi keyboard; then
            break
        fi
        sleep 0.2
        i=$((i + 1))
    done
}
CAGEEOF
  chmod +x /etc/init.d/cage-kiosk
  rc-update add cage-kiosk default
  sed -i "s|__HOMEPAGE__|$HOMEPAGE|" /etc/init.d/cage-kiosk

  # overlay-root setup: mount tmpfs upper, overlay onto the RO ext4 lower, then
  # switch_root into openrc. Every guest write lands in RAM so the disk never
  # diverges (required for Ctrl-A r reset). Passed via init= on the kernel cmdline
  # at cold boot only; restore inherits the mounted overlay from the RAM image.
  # The lower MUST be read-only: overlayfs requires a stable lower, and a
  # read-write lower yields inconsistent reads (EBADMSG loading shared libs).
  # The kernel cmdline also passes ro so the root mount itself is read-only; the
  # remount below is belt-and-suspenders.
  cat > /sbin/overlay-init <<'"'"'OVLEOF'"'"'
#!/bin/sh
mount -t proc proc /proc 2>/dev/null
mount -t sysfs sys /sys 2>/dev/null
mount -t tmpfs tmpfs /mnt
mkdir -p /mnt/up /mnt/work /mnt/root /mnt/lower
mount --bind / /mnt/lower
mount -o remount,ro /mnt/lower 2>/dev/null || true
mount -t overlay overlay -o lowerdir=/mnt/lower,upperdir=/mnt/up,workdir=/mnt/work /mnt/root
exec switch_root /mnt/root /sbin/init
OVLEOF
  chmod +x /sbin/overlay-init

  # Readiness sentinel for make-browser-base.sh: once the firefox process is up
  # and settled, print a marker on the serial console so the host can snapshot.
  cat > /etc/local.d/browser-ready.start <<'"'"'RDYEOF'"'"'
#!/bin/sh
# Match the real firefox binary path, not the cage command line (which contains
# the string firefox-esr) so the marker does not fire before firefox is running.
# The path may need adjusting at bring-up; pgrep -f /usr/lib/firefox is broad
# enough to catch the launched process without matching cage.
( i=0
  while [ "$i" -lt 120 ]; do
    if pgrep -f /usr/lib/firefox >/dev/null 2>&1; then
      sleep 6
      echo BROWSER_READY > /dev/ttyS0
      exit 0
    fi
    sleep 1
    i=$((i + 1))
  done
  echo BROWSER_TIMEOUT > /dev/ttyS0 ) &
RDYEOF
  chmod +x /etc/local.d/browser-ready.start
'

# Export the container filesystem to a tarball (host-user writable path).
docker export fcroot_browser_build -o "$TAR"
docker rm fcroot_browser_build >/dev/null

# 2. Pack the tree into a 1536 MiB ext4 (browser tree is far larger than the GUI tree).
docker run --rm -v "$STAGE:/work" ubuntu:22.04 bash -euxc '
  export DEBIAN_FRONTEND=noninteractive
  apt-get update -qq
  apt-get install -y -qq --no-install-recommends e2fsprogs >/dev/null

  rm -rf /tmp/rootfs && mkdir -p /tmp/rootfs
  tar xf /work/rootfs-browser.tar -C /tmp/rootfs
  rm -f /tmp/rootfs/.dockerenv
  # Drop the build-only C sources left at / by the ro bind mounts (compiled to
  # /usr/bin/{devmem,vmid-reseed} already; the sources are not needed at runtime).
  rm -f /tmp/rootfs/devmem.c /tmp/rootfs/vmid-reseed.c
  for d in dev proc run sys tmp mnt; do mkdir -p /tmp/rootfs/$d; done

  rm -f /work/out/rootfs-browser.ext4
  mke2fs -q -t ext4 -d /tmp/rootfs -L rootfs-browser /work/out/rootfs-browser.ext4 1536M
  ls -la /work/out/rootfs-browser.ext4
'

rm -f "$TAR"
