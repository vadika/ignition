#!/usr/bin/env python3
# Benchmark fresh boot vs snapshot restore using BOTH measurement methods:
#   - Guest-boot-time (boot_timer device): VM-start -> guest init readiness,
#     timestamped inside the VMM when the guest pokes the boot-timer MMIO byte.
#   - launch -> login: prompt (host wall-clock to an interactive shell).
#   - Restore-time (host-side): mmap + memory.bin load + state restore -> resume.
#   - launch -> restored prompt responsive.
# Live (needs the hypervisor entitlement + real kernel/rootfs). RUST_LOG=info so the
# device's info! lines (Guest-boot-time / Restore-time) are emitted.
import os, pty, sys, time, select, signal, re

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BOOT = os.path.join(ROOT, "target/debug/boot")
KERNEL = os.path.join(ROOT, "kimage/out/Image")
ROOTFS = os.path.join(ROOT, "kimage/out/rootfs.ext4")
SNAP = os.path.join(ROOT, "snapshot_bench")
N = int(sys.argv[1]) if len(sys.argv) > 1 else 5

GBT_RE = re.compile(rb"Guest-boot-time = (\d+) ms")
RT_RE = re.compile(rb"Restore-time = (\d+) ms")

def spawn(args):
    pid, fd = pty.fork()
    if pid == 0:
        os.environ["RUST_LOG"] = "info"
        os.execv(BOOT, [BOOT] + args)
        os._exit(127)
    return pid, fd

def kill(pid, fd):
    try: os.kill(pid, signal.SIGKILL); os.waitpid(pid, 0)
    except OSError: pass
    try: os.close(fd)
    except OSError: pass
    time.sleep(0.4)  # let HVF release the VM

def read_until(fd, deadline, want=None, accum=None):
    """Read until `want` (bytes) seen or deadline; return (found_time|None)."""
    while time.time() < deadline:
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try: d = os.read(fd, 4096)
            except OSError: break
            if not d: break
            if accum is not None: accum.append(d)
            if want and want in b"".join(accum or [d]):
                return time.time()
    return None

def bench_boot():
    """Returns (guest_boot_time_ms, launch_to_login_s)."""
    acc = []
    pid, fd = spawn([KERNEL, ROOTFS])
    t0 = time.time()
    login_t = read_until(fd, t0 + 20, b"login:", acc)
    # give the boot-timer poke (runs at the openrc `local` service, ~after login) time
    read_until(fd, time.time() + 2.0, b"\x00no-such", acc)  # just drain ~2s
    kill(pid, fd)
    blob = b"".join(acc)
    m = GBT_RE.search(blob)
    gbt = int(m.group(1)) if m else None
    login_s = (login_t - t0) if login_t else None
    return gbt, login_s

def make_snapshot():
    os.system(f"rm -rf {SNAP}")
    acc = []
    pid, fd = spawn(["--snap-dir", SNAP, KERNEL, ROOTFS])
    read_until(fd, time.time() + 25, b"login:", acc)
    time.sleep(1)
    os.write(fd, b"\x01s")
    time.sleep(4)
    ok = os.path.exists(os.path.join(SNAP, "vmstate.json"))
    kill(pid, fd)
    return ok

def bench_restore():
    """Returns (restore_time_ms, launch_to_prompt_s)."""
    acc = []
    pid, fd = spawn(["--restore", SNAP])
    t0 = time.time()
    # nudge gently for the getty prompt to redraw
    prompt_t = None
    last_nudge = 0.0
    while time.time() - t0 < 12:
        now = time.time() - t0
        if now - last_nudge >= 0.5:
            os.write(fd, b"\r"); last_nudge = now
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try: d = os.read(fd, 4096)
            except OSError: break
            acc.append(d)
            if b"login:" in b"".join(acc):
                prompt_t = time.time(); break
    kill(pid, fd)
    blob = b"".join(acc)
    m = RT_RE.search(blob)
    rt = int(m.group(1)) if m else None
    prompt_s = (prompt_t - t0) if prompt_t else None
    return rt, prompt_s

def stats(xs):
    xs = [x for x in xs if x is not None]
    if not xs: return (None, None, None)
    return (sum(xs)/len(xs), min(xs), max(xs))

print(f"=== ignition boot/restore benchmark (n={N}) ===", flush=True)
print("building snapshot...", flush=True)
if not make_snapshot():
    print("snapshot failed"); sys.exit(1)

gbt, login, rt, rprompt = [], [], [], []
for i in range(N):
    g, l = bench_boot()
    r, p = bench_restore()
    gbt.append(g); login.append(l); rt.append(r); rprompt.append(p)
    print(f"run {i+1}: Guest-boot-time={g}ms  ->login={l:.2f}s | Restore-time={r}ms  ->prompt={p:.2f}s", flush=True)

def fmt_ms(s): a = stats(s); return f"mean={a[0]:.0f}  min={a[1]:.0f}  max={a[2]:.0f}" if a[0] is not None else "n/a"
def fmt_s(s):  a = stats(s); return f"mean={a[0]:.2f}  min={a[1]:.2f}  max={a[2]:.2f}" if a[0] is not None else "n/a"

print("\n=== results ===")
print(f"FRESH BOOT")
print(f"  Guest-boot-time (boot_timer, VM-start->init ready, ms) : {fmt_ms(gbt)}")
print(f"  launch -> login: prompt (host wall, s)                 : {fmt_s(login)}")
print(f"RESTORE")
print(f"  Restore-time (host-side, RAM load+state restore, ms)   : {fmt_ms(rt)}")
print(f"  launch -> restored prompt (host wall, s)               : {fmt_s(rprompt)}")
os.system(f"rm -rf {SNAP}")
