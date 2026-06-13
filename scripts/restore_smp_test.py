#!/usr/bin/env python3
# Drive boot through a pty with --smp 4: boot -> snapshot (Ctrl-A s) -> restore,
# asserting the restored guest is responsive and sees all 4 cores (nproc == 4).
# Not a unit test (needs the hypervisor entitlement + a real kernel/rootfs).
import os, pty, sys, time, select, subprocess, signal

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BOOT = os.path.join(ROOT, "target/debug/boot")
KERNEL = os.path.join(ROOT, "kimage/out/Image")
ROOTFS = os.path.join(ROOT, "kimage/out/rootfs.ext4")
SNAP = os.path.join(ROOT, "snapshot_smp")
SMP = "4"

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

# ---- Phase A: boot --smp 4 to login, snapshot via Ctrl-A s ----
os.system(f"rm -rf {SNAP}")
pidA, fdA = spawn(["--smp", SMP, "--snap-dir", SNAP, KERNEL, ROOTFS])
print(f"=== boot phase: --smp {SMP}, waiting for login prompt ===", flush=True)
buf = drain(fdA, 30, echo=False, until=b"login:")
print(f"[boot reached login: {b'login:' in buf}]", flush=True)
time.sleep(1)
os.write(fdA, b"\x01s")
print("[sent Ctrl-A s, waiting for snapshot write]", flush=True)
drain(fdA, 8, echo=False)
ok_snap = os.path.exists(os.path.join(SNAP, "memory.bin")) and os.path.exists(os.path.join(SNAP, "vmstate.json"))
print(f"[snapshot written: {ok_snap}]", flush=True)
os.kill(pidA, signal.SIGKILL); os.waitpid(pidA, 0)
os.close(fdA)
if not ok_snap:
    print("RESULT: snapshot FAILED, abort"); sys.exit(1)

# ---- Phase B: restore, check responsiveness + core count ----
time.sleep(1)
pidB, fdB = spawn(["--restore", SNAP])
print("=== restore phase ===", flush=True)
drain(fdB, 3, echo=False)
samples = [cpu_pct(pidB) for _ in range(5) if not time.sleep(0.5)]
ok = [s for s in samples if s >= 0]
avg_cpu = sum(ok) / max(1, len(ok))
print(f"[restore CPU% samples: {samples}  avg={avg_cpu:.1f}]", flush=True)

# Log in (root, no password) and ask the guest how many cores it sees.
os.write(fdB, b"\r"); time.sleep(0.5)
drain(fdB, 2, echo=False)
os.write(fdB, b"root\r"); time.sleep(0.8)
drain(fdB, 2, echo=False)
os.write(fdB, b"nproc\r"); time.sleep(0.8)
resp = drain(fdB, 3, echo=False)
responsive = len(resp.strip()) > 0
nproc4 = b"4" in resp
print(f"[responsive: {responsive}  nproc==4: {nproc4}]", flush=True)
if resp:
    print("---- restore console after nproc ----")
    sys.stdout.buffer.write(resp[-400:]); print("\n----")
os.kill(pidB, signal.SIGKILL); os.waitpid(pidB, 0)
os.close(fdB)

print(f"\nRESULT: snapshot={ok_snap} restore_cpu={avg_cpu:.1f}% responsive={responsive} nproc4={nproc4}")
sys.exit(0 if (ok_snap and responsive and nproc4) else 1)
