#!/usr/bin/env python3
# Drive boot through a pty: boot -> snapshot (Ctrl-A s) -> restore, measuring
# restore CPU% and console responsiveness. Not a unit test (needs the hypervisor
# entitlement + a real kernel/rootfs); a live integration driver.
import os, pty, sys, time, select, subprocess, signal, re

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BOOT = os.path.join(ROOT, "target/debug/boot")
KERNEL = os.path.join(ROOT, "kimage/out/Image")
ROOTFS = os.path.join(ROOT, "kimage/out/rootfs.ext4")
SNAP = os.path.join(ROOT, "snapshot2")

def spawn(args):
    pid, fd = pty.fork()
    if pid == 0:
        os.execv(BOOT, [BOOT] + args)
        os._exit(127)
    return pid, fd

def drain(fd, seconds, echo=False, until=None):
    out = b""
    end = time.time() + seconds
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.2)
        if r:
            try:
                d = os.read(fd, 4096)
            except OSError:
                break
            if not d:
                break
            out += d
            if echo:
                sys.stdout.buffer.write(d); sys.stdout.flush()
            if until and until in out:
                break
    return out

def cpu_pct(pid):
    try:
        o = subprocess.check_output(["ps", "-o", "%cpu=", "-p", str(pid)]).decode().strip()
        return float(o)
    except Exception:
        return -1.0

# ---- Phase A: boot to login, snapshot via Ctrl-A s ----
os.system(f"rm -rf {SNAP}")
pidA, fdA = spawn(["--snap-dir", SNAP, KERNEL, ROOTFS])
print("=== boot phase: waiting for login prompt ===", flush=True)
buf = drain(fdA, 25, echo=False, until=b"login:")
got_login = b"login:" in buf
print(f"[boot reached login: {got_login}]", flush=True)
time.sleep(1)
# trigger snapshot
os.write(fdA, b"\x01s")
print("[sent Ctrl-A s, waiting for snapshot write]", flush=True)
drain(fdA, 6, echo=False)
ok_snap = os.path.exists(os.path.join(SNAP, "memory.bin")) and os.path.exists(os.path.join(SNAP, "vmstate.json"))
print(f"[snapshot written: {ok_snap}]", flush=True)
os.kill(pidA, signal.SIGKILL); os.waitpid(pidA, 0)
os.close(fdA)
if not ok_snap:
    print("RESULT: snapshot FAILED, abort"); sys.exit(1)

# ---- Phase B: restore, measure CPU% + responsiveness ----
time.sleep(1)
pidB, fdB = spawn(["--restore", SNAP])
print("=== restore phase ===", flush=True)
drain(fdB, 3, echo=False)            # let it settle
samples = [cpu_pct(pidB) for _ in range(5) if not time.sleep(0.5)]
avg_cpu = sum(s for s in samples if s >= 0) / max(1, len([s for s in samples if s >= 0]))
print(f"[restore CPU% samples: {samples}  avg={avg_cpu:.1f}]", flush=True)
# responsiveness: send a newline, expect the shell/login to echo something
os.write(fdB, b"\r")
time.sleep(0.5)
os.write(fdB, b"\r")
resp = drain(fdB, 4, echo=False)
responsive = len(resp.strip()) > 0
print(f"[responsive: {responsive}  bytes={len(resp)}]", flush=True)
if resp:
    print("---- restore console after Enter ----")
    sys.stdout.buffer.write(resp[-400:]); print("\n----")
os.kill(pidB, signal.SIGKILL); os.waitpid(pidB, 0)
os.close(fdB)

print(f"\nRESULT: snapshot={ok_snap} restore_cpu={avg_cpu:.1f}% responsive={responsive}")
