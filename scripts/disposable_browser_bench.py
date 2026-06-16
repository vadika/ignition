#!/usr/bin/env python3
# Disposable-browser latency: cold boot vs cold restore vs hot restore.
# Live — needs the hypervisor entitlement + the browser rootfs + an existing
# `browser-base` snapshot (create it first with: sudo scripts/make-browser-base.sh).
# vmnet (--net) AND `purge` (cold-cache) both need root, so run the whole thing
# under sudo:
#
#   sudo scripts/disposable_browser_bench.py [N]
#
# Three scenarios (n runs each, default 3):
#   cold boot    — full kernel boot + overlay switch_root + Firefox launch, timed
#                  launch -> BROWSER_READY (the guest's readiness sentinel), plus the
#                  VMM-logged Guest-boot-time (kernel + early init, guest time domain).
#   hot restore  — `--restore browser-base` with a WARM page cache (run repeatedly);
#                  Restore-time is the host-side up-front cost (clonefile + mmap +
#                  GIC/device/vCPU state restore, before the guest runs).
#   cold restore — same, but `purge` drops the page cache before each run so the
#                  base's memory.bin/disk.img start cold.
#
# Note: restore uses clonefile + mmap(MAP_SHARED), which is LAZY — Restore-time does
# no large reads, so cold vs hot differ mostly in post-restore lazy page-in (amortized
# as the guest runs), not in Restore-time itself. We report Restore-time for both and
# say so.
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


def run(args, needle, timeout):
    """Spawn boot, read the merged pty stream until `needle` (bytes) appears or
    timeout. Return (launch_to_needle_s | None, captured_bytes)."""
    pid, fd = spawn(args)
    t0 = time.time()
    out = b""
    hit = None
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
            if needle and hit is None and needle in out:
                hit = time.time() - t0
                # give it a beat to flush the timing logs, then stop
                deadline = time.time() + 0.5
                while time.time() < deadline:
                    rr, _, _ = select.select([fd], [], [], 0.05)
                    if rr:
                        try:
                            out += os.read(fd, 4096)
                        except OSError:
                            break
                break
    kill(pid, fd)
    return hit, out


def first_int(rx, buf):
    m = rx.search(buf)
    return int(m.group(1)) if m else None


def summary(name, xs):
    xs = [x for x in xs if x is not None]
    if not xs:
        return f"{name:<34}: (no samples)"
    mean = statistics.mean(xs)
    return f"{name:<34}: mean={mean:8.1f}  min={min(xs):8.1f}  max={max(xs):8.1f}  (n={len(xs)})"


def main():
    if os.geteuid() != 0:
        print("run under sudo (needs --net vmnet + purge for cold cache)", file=sys.stderr)
        sys.exit(2)
    for p in (BOOT, KERNEL, ROOTFS):
        if not os.path.exists(p):
            print(f"missing: {p}", file=sys.stderr)
            sys.exit(1)
    if not os.path.isdir(os.path.join(STORE, "snapshots", BASE)) and not os.path.isdir(os.path.join(STORE, BASE)):
        print(f"warning: snapshot '{BASE}' not found under {STORE} — "
              f"create it first: sudo scripts/make-browser-base.sh", file=sys.stderr)

    print(f"=== disposable-browser latency, n={N} (host: $(uname), warm unless noted) ===\n", flush=True)

    cold_ready, cold_gboot = [], []
    print("--- cold boot (launch -> BROWSER_READY) ---", flush=True)
    for i in range(N):
        ready, out = run(COLD_BOOT_ARGS, b"BROWSER_READY", timeout=120)
        gboot = first_int(RE_GUEST_BOOT, out)
        cold_ready.append(ready * 1000 if ready else None)
        cold_gboot.append(gboot)
        rstr = f"{ready:.2f}s" if ready else "TIMEOUT"
        print(f"  run {i+1}: BROWSER_READY={rstr}  Guest-boot-time={gboot} ms", flush=True)

    hot_restore, tail_sample = [], None
    print("\n--- hot restore (warm cache, Restore-time) ---", flush=True)
    for i in range(N):
        _, out = run(RESTORE_ARGS, b"Restore-time", timeout=30)
        rt = first_int(RE_RESTORE, out)
        hot_restore.append(rt)
        if tail_sample is None:
            m = RE_TAIL.search(out)
            tail_sample = m.group(1).decode(errors="replace") if m else None
        print(f"  run {i+1}: Restore-time={rt} ms", flush=True)

    cold_restore = []
    print("\n--- cold restore (purge cache before each, Restore-time) ---", flush=True)
    for i in range(N):
        os.system("purge")  # macOS: drop the unified page cache
        _, out = run(RESTORE_ARGS, b"Restore-time", timeout=30)
        rt = first_int(RE_RESTORE, out)
        cold_restore.append(rt)
        print(f"  run {i+1}: Restore-time={rt} ms (post-purge)", flush=True)

    print("\n=== summary (ms) ===")
    print(summary("cold boot -> BROWSER_READY (wall)", cold_ready))
    print(summary("cold boot Guest-boot-time", cold_gboot))
    print(summary("hot restore Restore-time", hot_restore))
    print(summary("cold restore Restore-time", cold_restore))
    if tail_sample:
        print(f"\nRestore-tail breakdown (one hot sample): {tail_sample}")
    print("\nNote: Restore-time is the up-front host cost (clonefile + lazy mmap + state\n"
          "restore); cold vs hot differ mainly in post-restore lazy page-in, amortized\n"
          "as the guest runs, not captured here.")


if __name__ == "__main__":
    main()
