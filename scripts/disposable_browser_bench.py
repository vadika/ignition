#!/usr/bin/env python3
# Disposable-browser latency: cold boot vs cold restore vs hot restore.
# Live — needs the hypervisor entitlement + the browser rootfs + an existing
# `browser-base` snapshot (create it first: sudo scripts/make-browser-base.sh).
# vmnet (--net) needs root, so run under sudo:
#
#   sudo scripts/disposable_browser_bench.py [N]
#
# Three scenarios (n runs each, default 3):
#   cold boot    — full kernel boot + overlay switch_root + Firefox launch.
#                  Timed launch -> BROWSER_READY (the guest's readiness sentinel),
#                  plus the VMM-logged Guest-boot-time (kernel + early init).
#   cold restore — a FRESH `boot --restore browser-base` process: bring the
#                  snapshot on disk back to a running browser. Measured by
#                  Restore-time (host-side: clonefile + mmap(MAP_SHARED) +
#                  GIC/device/vCPU state restore, before the guest runs).
#   hot restore  — an IN-PLACE reset of an already-running instance (Ctrl-A r /
#                  GUI Ctrl+Alt+R): dirty-only rollback to the in-memory reset
#                  point. Measured by Reset-time (the synchronous snap-back; the
#                  net reconnect afterwards is async ~2s and excluded). We trigger
#                  it over the serial console with Ctrl-A r.
import os, pty, sys, time, select, signal, re, statistics

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BOOT = os.path.join(ROOT, "target/debug/boot")
KERNEL = os.path.join(ROOT, "kimage/out/Image")
ROOTFS = os.path.join(ROOT, "kimage/out/rootfs-browser.ext4")
STORE = os.path.join(ROOT, "vmstore")
BASE = "browser-base"
N = int(sys.argv[1]) if len(sys.argv) > 1 else 3

COLD_BOOT_ARGS = [
    "--gui", "--net", "--smp", "2", "--mem", "1024", "--track-dirty",
    "--append", "ro init=/sbin/overlay-init", KERNEL, ROOTFS,
]
RESTORE_ARGS = [
    "--gui", "--net", "--mem", "1024", "--track-dirty",
    "--store", STORE, "--restore", BASE,
]

RE_GUEST_BOOT = re.compile(rb"Guest-boot-time = (\d+) ms")
RE_RESTORE = re.compile(rb"Restore-time = (\d+) ms")
RE_TAIL = re.compile(rb"Restore-tail = (.+)")
RE_RESET = re.compile(rb"Reset-time = (\d+) us")
CTRL_A = b"\x01"


def spawn(args):
    pid, fd = pty.fork()
    if pid == 0:
        os.execv(BOOT, [BOOT] + args)
        os._exit(127)
    return pid, fd


def kill(pid, fd):
    try:
        os.kill(pid, signal.SIGKILL)
        os.waitpid(pid, 0)
    except OSError:
        pass
    try:
        os.close(fd)
    except OSError:
        pass
    time.sleep(0.6)  # let HVF release the VM + the window close before the next launch


def read_until(fd, needle, timeout, t0=None):
    """Read the pty stream until `needle` (bytes) appears or timeout. Return
    (seconds_since_t0_to_needle | None, captured_bytes). t0 defaults to now."""
    if t0 is None:
        t0 = time.time()
    out = b""
    while time.time() - t0 < timeout:
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try:
                d = os.read(fd, 4096)
            except OSError:
                break
            if not d:
                break
            out += d
            if needle in out:
                # small drain so the line (and any trailing log) is fully captured
                deadline = time.time() + 0.3
                while time.time() < deadline:
                    rr, _, _ = select.select([fd], [], [], 0.05)
                    if not rr:
                        break
                    try:
                        out += os.read(fd, 4096)
                    except OSError:
                        break
                return time.time() - t0, out
    return None, out


def first_int(rx, buf):
    m = rx.search(buf)
    return int(m.group(1)) if m else None


def summ(name, xs, unit):
    xs = [x for x in xs if x is not None]
    if not xs:
        return f"{name:<36}: (no samples)"
    return (f"{name:<36}: mean={statistics.mean(xs):9.1f}  "
            f"min={min(xs):9.1f}  max={max(xs):9.1f} {unit}  (n={len(xs)})")


def cold_boot_phase():
    ready_ms, gboot_ms = [], []
    print("--- cold boot (launch -> BROWSER_READY) ---", flush=True)
    for i in range(N):
        pid, fd = spawn(COLD_BOOT_ARGS)
        t, out = read_until(fd, b"BROWSER_READY", timeout=120)
        gb = first_int(RE_GUEST_BOOT, out)
        kill(pid, fd)
        ready_ms.append(t * 1000 if t else None)
        gboot_ms.append(gb)
        print(f"  run {i+1}: BROWSER_READY={(f'{t:.2f}s' if t else 'TIMEOUT')}  Guest-boot-time={gb} ms", flush=True)
    return ready_ms, gboot_ms


def cold_restore_phase():
    rt_ms, tail = [], None
    print("\n--- cold restore (fresh --restore process, Restore-time) ---", flush=True)
    for i in range(N):
        pid, fd = spawn(RESTORE_ARGS)
        _, out = read_until(fd, b"Restore-time", timeout=30)
        rt = first_int(RE_RESTORE, out)
        if tail is None:
            m = RE_TAIL.search(out)
            tail = m.group(1).decode(errors="replace") if m else None
        # Restore-time is logged BEFORE the GUI event loop opens the window; linger
        # so the window actually paints (visual confirmation). Does not affect rt.
        time.sleep(3.0)
        kill(pid, fd)
        rt_ms.append(rt)
        print(f"  run {i+1}: Restore-time={rt} ms", flush=True)
    return rt_ms, tail


SITES = [
    "https://en.wikipedia.org/wiki/MicroVM",
    "https://www.mozilla.org",
    "https://en.wikipedia.org/wiki/Hypervisor",
    "https://example.org",
]


def serial_login(fd):
    """Best-effort root login on the serial console (root has no password). Harmless
    if a shell is already active (`root` just becomes an unknown command)."""
    os.write(fd, b"\r")
    time.sleep(0.4)
    os.write(fd, b"root\r")
    time.sleep(1.2)


def navigate(fd, url):
    """Tell the running (cage) Firefox to load `url` via its remote: a second
    firefox-esr invocation with the same profile loads into the existing instance.
    Needs the cage Wayland session env. Best-effort; logged to /tmp/ffnav.log in-guest."""
    cmd = (b"env XDG_RUNTIME_DIR=/run/user/0 WAYLAND_DISPLAY=wayland-1 "
           b"MOZ_ENABLE_WAYLAND=1 firefox-esr --new-tab '" + url.encode() +
           b"' >/tmp/ffnav.log 2>&1 &\r")
    os.write(fd, cmd)


def hot_restore_phase():
    """One restored instance; before each in-place reset, navigate the live Firefox
    to a real page (dirtying a realistic working set), then trigger Ctrl-A r and read
    Reset-time. Returns reset latencies in MILLISECONDS."""
    reset_ms = []
    print("\n--- hot restore (navigate to a real page, then in-place Ctrl-A r reset) ---", flush=True)
    pid, fd = spawn(RESTORE_ARGS)
    done, _ = read_until(fd, b"Restore-time", timeout=30)
    if done is None:
        print("  restore did not settle; skipping hot-restore phase", flush=True)
        kill(pid, fd)
        return reset_ms
    time.sleep(5.0)  # let the restore's net carrier-bounce reconnect + the desktop settle
    for i in range(N):
        url = SITES[i % len(SITES)]
        serial_login(fd)
        navigate(fd, url)
        print(f"  reset {i+1}: navigated to {url}; loading...", flush=True)
        time.sleep(8.0)  # page load + render -> dirty a realistic working set
        os.write(fd, CTRL_A + b"r")          # Ctrl-A r -> Action::Reset (in-place)
        _, out = read_until(fd, b"Reset-time", timeout=15)
        us = first_int(RE_RESET, out)
        ms = us / 1000.0 if us is not None else None
        reset_ms.append(ms)
        print(f"  reset {i+1}: Reset-time={ms:.3f} ms" if ms is not None else f"  reset {i+1}: Reset-time=MISS", flush=True)
        time.sleep(5.0)  # post-reset net reconnect + settle before the next navigation
    kill(pid, fd)
    return reset_ms


def main():
    if os.geteuid() != 0:
        print("run under sudo (needs --net vmnet)", file=sys.stderr)
        sys.exit(2)
    for p in (BOOT, KERNEL, ROOTFS):
        if not os.path.exists(p):
            print(f"missing: {p}", file=sys.stderr)
            sys.exit(1)
    if not os.path.isdir(os.path.join(STORE, "snapshots", BASE)):
        print(f"warning: snapshot '{BASE}' not found under {STORE}/snapshots — "
              f"create it: sudo scripts/make-browser-base.sh", file=sys.stderr)

    print(f"=== disposable-browser latency, n={N} ===\n", flush=True)
    ready, gboot = cold_boot_phase()
    crestore, tail = cold_restore_phase()
    hreset = hot_restore_phase()

    print("\n=== summary ===")
    print(summ("cold boot -> BROWSER_READY (wall)", ready, "ms"))
    print(summ("cold boot Guest-boot-time", gboot, "ms"))
    print(summ("cold restore Restore-time", crestore, "ms"))
    print(summ("hot restore Reset-time (snap-back)", hreset, "ms"))
    if tail:
        print(f"\nRestore-tail (one cold-restore sample): {tail}")
    print("\nNotes (all figures in ms): cold restore Restore-time is the host-side up-front\n"
          "cost (clonefile + lazy mmap + state restore); lazy page-in afterward is amortized.\n"
          "Hot restore Reset-time is the synchronous in-place snap-back AFTER navigating the\n"
          "live Firefox to a real page (rolling back that working set) — the net reconnect\n"
          "that follows a reset is async (~2s) and not included.")


if __name__ == "__main__":
    main()
