#!/usr/bin/env python3
# Drive boot through a pty: boot -> snapshot (Ctrl-A s) -> restore, measuring
# restore CPU% and console responsiveness. Not a unit test (needs the hypervisor
# entitlement + a real kernel/rootfs); a live integration driver.
import os, pty, sys, time, select, subprocess, signal, re
import hashlib

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BOOT = os.path.join(ROOT, "target/debug/boot")
KERNEL = os.path.join(ROOT, "kimage/out/Image")
ROOTFS = os.path.join(ROOT, "kimage/out/rootfs.ext4")
STORE = os.path.join(ROOT, "vmstore-test")
NAME = "test-snap"
SNAP = os.path.join(STORE, "snapshots", NAME)  # base dir, for the immutability checks

def md5(path):
    h = hashlib.md5()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()

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
os.system(f"rm -rf {STORE}")
pidA, fdA = spawn(["--store", STORE, "--name", NAME, KERNEL, ROOTFS])
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

# ---- Phase B: restore, measure CPU% + responsiveness + latency + immutability ----
time.sleep(1)
# capture base hashes before restore so we can prove the base is never mutated
base_mem = os.path.join(SNAP, "memory.bin")
base_disk = os.path.join(SNAP, "disk.img")
base_mem_md5 = md5(base_mem)
base_disk_md5 = md5(base_disk) if os.path.exists(base_disk) and os.path.getsize(base_disk) > 0 else None

print("=== restore phase ===", flush=True)
t_restore = time.time()
pidB, fdB = spawn(["--store", STORE, "--restore", NAME])
# A restored guest resumes PAST login and does NOT reprint `login:`, so we
# measure "time to interactive": prompt the resumed shell with a CR and time
# the wall-clock until the first non-empty console output comes back.
os.write(fdB, b"\r")
time.sleep(0.3)
os.write(fdB, b"\r")
resp = b""
full = b""  # accumulate ALL restore-phase bytes (Restore-time/Restore-breakdown
            # log lines fall outside the truncated `resp` tail we print below)
end = time.time() + 6.0
restore_latency_ms = 6000.0
while time.time() < end:
    r, _, _ = select.select([fdB], [], [], 0.2)
    if r:
        try:
            d = os.read(fdB, 4096)
        except OSError:
            break
        if not d:
            break
        resp += d
        full += d
        if resp.strip():
            restore_latency_ms = (time.time() - t_restore) * 1000.0
            break
# drain a little more so the Restore-breakdown/Restore-time log lines land in `full`
full += drain(fdB, 0.8)
responsive = len(resp.strip()) > 0
print(f"[restore -> first output latency: {restore_latency_ms:.0f} ms]", flush=True)
print(f"[responsive: {responsive}  bytes={len(resp)}]", flush=True)
if resp:
    print("---- restore console after Enter ----")
    sys.stdout.buffer.write(resp[-400:]); print("\n----")
import re as _re
_bd = _re.search(rb"Restore-breakdown = chain:(\d+)us .* total:(\d+)us", full)
print(f"[Restore-breakdown present: {_bd is not None}]", flush=True)
assert _bd is not None, "Restore-breakdown line missing from restore output"
# sample CPU% (should idle low)
samples = [cpu_pct(pidB) for _ in range(5) if not time.sleep(0.5)]
avg_cpu = sum(s for s in samples if s >= 0) / max(1, len([s for s in samples if s >= 0]))
print(f"[restore CPU% samples: {samples}  avg={avg_cpu:.1f}]", flush=True)
os.kill(pidB, signal.SIGKILL); os.waitpid(pidB, 0)
os.close(fdB)

# ---- immutability: the CoW clone must not have touched the base ----
mem_unchanged = (md5(base_mem) == base_mem_md5)
disk_unchanged = (base_disk_md5 is None) or (md5(base_disk) == base_disk_md5)
print(f"[base memory.bin unchanged: {mem_unchanged}]", flush=True)
print(f"[base disk.img unchanged: {disk_unchanged}]", flush=True)
print(
    f"\nRESULT: snapshot={ok_snap} restore_cpu={avg_cpu:.1f}% "
    f"responsive={responsive} latency_ms={restore_latency_ms:.0f} "
    f"immutable_mem={mem_unchanged} immutable_disk={disk_unchanged}"
)
if not (mem_unchanged and disk_unchanged):
    sys.exit(1)
