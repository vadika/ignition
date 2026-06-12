#!/usr/bin/env python3
# Timing comparison: fresh boot vs restore-from-snapshot, wall-clock to a usable
# login prompt. Live (needs the hypervisor entitlement + real kernel/rootfs).
import os, pty, sys, time, select, signal

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BOOT = os.path.join(ROOT, "target/debug/boot")
KERNEL = os.path.join(ROOT, "kimage/out/Image")
ROOTFS = os.path.join(ROOT, "kimage/out/rootfs.ext4")
SNAP = os.path.join(ROOT, "snapshot2")
N = int(sys.argv[1]) if len(sys.argv) > 1 else 5

def spawn(args):
    pid, fd = pty.fork()
    if pid == 0:
        os.execv(BOOT, [BOOT] + args)
        os._exit(127)
    return pid, fd

def wait_for(fd, needle, timeout):
    """Return (seconds_to_needle | None, first_byte_seconds | None)."""
    out = b""; t0 = time.time(); first = None
    while time.time() - t0 < timeout:
        r,_,_ = select.select([fd], [], [], 0.05)
        if r:
            try: d = os.read(fd, 4096)
            except OSError: break
            if not d: break
            if first is None and d.strip(): first = time.time() - t0
            out += d
            if needle in out:
                return time.time() - t0, first
    return None, first

def kill(pid, fd):
    try: os.kill(pid, signal.SIGKILL); os.waitpid(pid, 0)
    except OSError: pass
    try: os.close(fd)
    except OSError: pass
    time.sleep(0.4)  # let HVF release the VM before the next launch

def make_snapshot():
    os.system(f"rm -rf {SNAP}")
    pid, fd = spawn(["--snap-dir", SNAP, KERNEL, ROOTFS])
    t, _ = wait_for(fd, b"login:", 30)
    time.sleep(1)
    os.write(fd, b"\x01s")          # Ctrl-A s
    time.sleep(4)
    ok = os.path.exists(os.path.join(SNAP, "vmstate.json"))
    kill(pid, fd)
    return ok, t

def time_fresh():
    pid, fd = spawn([KERNEL, ROOTFS])
    t, first = wait_for(fd, b"login:", 30)
    kill(pid, fd)
    return t, first

def time_restore():
    """Return (overhead_s, interactive_s): overhead = launch -> 'guest console'
    banner (mmap + 512 MiB memory.bin load + GIC/vCPU state restore, before the
    guest runs); interactive = launch -> login prompt responds to Enter."""
    pid, fd = spawn(["--restore", SNAP])
    t0 = time.time(); out = b""; banner = None; last_nudge = 0.0
    while time.time() - t0 < 15:
        now = time.time() - t0
        # one gentle Enter every 0.5s (flooding overflows the 16550 RX FIFO)
        if now - last_nudge >= 0.5:
            os.write(fd, b"\r"); last_nudge = now
        r,_,_ = select.select([fd], [], [], 0.02)
        if r:
            try: d = os.read(fd, 4096)
            except OSError: break
            out += d
            if banner is None and b"guest console" in out:
                banner = time.time() - t0
            if b"login:" in out:
                kill(pid, fd); return banner, time.time() - t0
    sys.stderr.write(f"[restore miss] banner={banner} tail={out[-120:]!r}\n")
    kill(pid, fd)
    return banner, None

def stats(xs):
    xs = [x for x in xs if x is not None]
    if not xs: return None
    return sum(xs)/len(xs), min(xs), max(xs)

print(f"=== building snapshot (once) ===", flush=True)
ok, bt = make_snapshot()
if not ok:
    print("snapshot failed"); sys.exit(1)
print(f"[snapshot baseline fresh-boot-to-login: {bt:.2f}s]\n", flush=True)

fresh, r_over, r_inter = [], [], []
for i in range(N):
    f, _ = time_fresh();          fresh.append(f)
    over, inter = time_restore(); r_over.append(over); r_inter.append(inter)
    istr = f"{inter:.2f}s" if inter is not None else "n/a"
    print(f"run {i+1}: fresh_boot={f:.2f}s  restore_overhead={over:.2f}s  restore_interactive={istr}", flush=True)

fs, os_, is_ = stats(fresh), stats(r_over), stats(r_inter)
print("\n=== seconds, n={} ===".format(N))
print(f"fresh boot (launch -> login)            : mean={fs[0]:.2f}  min={fs[1]:.2f}  max={fs[2]:.2f}")
print(f"restore overhead (launch -> guest runs) : mean={os_[0]:.2f}  min={os_[1]:.2f}  max={os_[2]:.2f}")
print(f"restore interactive (launch -> prompt)  : mean={is_[0]:.2f}  min={is_[1]:.2f}  max={is_[2]:.2f}")
print(f"\nrestore overhead is {fs[0]/os_[0]:.1f}x faster than a fresh boot "
      f"({fs[0]-os_[0]:.2f}s saved); the rest is getty re-prompting on Enter.")
