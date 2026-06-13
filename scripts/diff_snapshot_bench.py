#!/usr/bin/env python3
# Timing harness for the diff-snapshot feature, run on REAL HVF hardware
# (Apple Silicon / macOS). Produces medians + spread over repeated runs for:
#
#   1. Dirty-tracking runtime overhead
#        a. Guest boot time WITHOUT vs WITH --track-dirty.
#           - "boot device" timer: the boot_timer device's internal
#             `Guest-boot-time = N ms` (kernel start -> userspace magic byte).
#           - "wall" timer: host spawn() -> `login:` seen on console.
#        b. In-guest write throughput (dd 64 MiB to /dev/shm, a RAM-backed
#           tmpfs) tracked vs untracked. Captures dd's reported MB/s. Writing to
#           tmpfs dirties guest RAM pages directly (the rootfs ext4 is 100% full,
#           so a disk-backed write is impossible anyway). The write-protect fault
#           tax shows up as lower MB/s under --track-dirty (every first-write
#           page-faults out of write-protect).
#
#   2. Snapshot WRITE time (Ctrl-A s sent -> handler prints the
#      `[snapshot] full|diff '<name>' ... written` line). Full root (whole RAM)
#      vs Diff layers at a couple of dirtied sizes (8 MiB, 64 MiB).
#
#   3. Restore LATENCY (host spawn() -> first non-empty console output after we
#      poke Enter; restored guests do not reprint the login banner). Also parses
#      the in-process `Restore-time = N ms` log line for the internal cost.
#      Compared across chain depth: Full-only (1 layer), golden+1 diff,
#      golden+3 diffs. Each extra diff layer adds read_diff_pages + apply_diff
#      (a memcpy of that layer's dirty pages) before vCPUs run.
#
#   4. Disk FOOTPRINT: full memory.bin vs diff memory.bin (logical st_size,
#      since packed, and physical st_blocks*512). N-fork chain vs N full snaps.
#
# WHAT EACH TIMER BRACKETS is stated explicitly in the printed output and the
# generated report. Uses time.monotonic(). Debug build by default; pass
# --release to point at target/release/boot for a release data point.
#
# Console driving (pty + Ctrl-A escapes + paced keystrokes for the 16-byte UART
# RX FIFO) is reused from scripts/restore_test.py and scripts/diff_snapshot_test.py.
import os, pty, sys, time, select, signal, json, re, argparse, statistics

ROOT   = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
KERNEL = os.path.join(ROOT, "kimage/out/Image")
ROOTFS = os.path.join(ROOT, "kimage/out/rootfs.ext4")
STORE  = os.path.join(ROOT, "vmstore-bench")
SNAPS  = os.path.join(STORE, "snapshots")
PAGE   = 16384

BOOT = None  # set in main from --release

# ---- low-level console driving (from restore_test.py / diff_snapshot_test.py) ----
def spawn(args):
    pid, fd = pty.fork()
    if pid == 0:
        os.execv(BOOT, [BOOT] + args)
        os._exit(127)
    return pid, fd

def drain(fd, seconds, until=None, sink=None):
    out = b""
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        r, _, _ = select.select([fd], [], [], 0.1)
        if r:
            try:
                d = os.read(fd, 4096)
            except OSError:
                break
            if not d:
                break
            out += d
            if sink is not None:
                sink.append(d)
            if until and until in out:
                break
    return out

def send_slow(fd, data, chunk=8, pause=0.06):
    """Pace host->guest input in <=chunk-byte bursts: the guest UART RX FIFO is
    only 16 bytes; a full line at once overflows it and corrupts the command."""
    if isinstance(data, str):
        data = data.encode()
    for i in range(0, len(data), chunk):
        os.write(fd, data[i:i + chunk])
        time.sleep(pause)

def kill(pid, fd):
    try: os.kill(pid, signal.SIGKILL)
    except ProcessLookupError: pass
    try: os.waitpid(pid, 0)
    except ChildProcessError: pass
    try: os.close(fd)
    except OSError: pass

def login_root(fd, boot_timeout=30):
    """Wait for login prompt, log in as root (no password), confirm a shell."""
    txt = drain(fd, boot_timeout, until=b"login:")
    if b"login:" not in txt:
        send_slow(fd, b"\r"); txt += drain(fd, 4, until=b"login:")
    send_slow(fd, b"root\r"); time.sleep(0.8)
    txt += drain(fd, 3)
    if b"assword" in txt.lower():
        send_slow(fd, b"\r"); txt += drain(fd, 2)
    send_slow(fd, b"echo SHELL_$((1+1))_READY\r"); time.sleep(0.6)
    txt += drain(fd, 3)
    return txt

def run_cmd(fd, cmd, wait=1.2, drain_s=3):
    send_slow(fd, cmd.encode() + b"\r"); time.sleep(wait)
    return drain(fd, drain_s)

# env_logger colorizes; match the number after "Guest-boot-time = ".
RE_BOOT    = re.compile(rb"Guest-boot-time = (\d+) ms")
RE_RESTORE = re.compile(rb"Restore-time = (\d+) ms")
RE_SNAPDONE = re.compile(rb"\[snapshot\] (full|diff) '([^']+)'")
RE_DD_MBS  = re.compile(rb"([\d.]+)\s*(MB|MiB|GB|kB)/s")

def parse_boot_ms(buf):
    m = RE_BOOT.search(buf)
    return int(m.group(1)) if m else None

def parse_restore_ms(buf):
    m = RE_RESTORE.search(buf)
    return int(m.group(1)) if m else None

def parse_dd_mbs(buf):
    # dd prints e.g. "67108864 bytes (64.0MB) copied, 1.23 s, 54.5MB/s"
    m = None
    for m in RE_DD_MBS.finditer(buf):
        pass
    if not m:
        return None
    val = float(m.group(1)); unit = m.group(2)
    # normalize to MB/s (decimal) for reporting; dd on busybox uses MB.
    mult = {b"kB": 1e-3, b"MB": 1.0, b"MiB": 1.048576, b"GB": 1e3}.get(unit, 1.0)
    return val * mult

def phys_bytes(path):
    st = os.stat(path)
    return st.st_blocks * 512

def med_spread(xs):
    xs = [x for x in xs if x is not None]
    if not xs:
        return (None, None, None, 0)
    return (statistics.median(xs), min(xs), max(xs), len(xs))

# ---------------------------------------------------------------------------
def measure_boot(track, samples, mem):
    """Boot N times; return lists of (boot_device_ms, wall_login_ms)."""
    dev, wall = [], []
    for i in range(samples):
        os.system(f"rm -rf {STORE}")
        args = ["--store", STORE, "--name", f"bootx{i}", "--mem", str(mem)]
        if track:
            args.append("--track-dirty")
        args += [KERNEL, ROOTFS]
        t0 = time.monotonic()
        pid, fd = spawn(args)
        buf = drain(fd, 40, until=b"login:")
        wall_ms = (time.monotonic() - t0) * 1000.0
        b = parse_boot_ms(buf)
        kill(pid, fd)
        if b"login:" in buf:
            dev.append(b); wall.append(wall_ms)
            print(f"   [boot track={track} #{i}] dev={b}ms wall={wall_ms:.0f}ms", flush=True)
        else:
            print(f"   [boot track={track} #{i}] FAILED to reach login", flush=True)
        time.sleep(0.5)
    return dev, wall

def measure_dd(track, samples, mem, count_mb=64):
    """Boot, log in, run a bounded dd, capture MB/s. Returns list of MB/s."""
    res = []
    for i in range(samples):
        os.system(f"rm -rf {STORE}")
        args = ["--store", STORE, "--name", f"ddx{i}", "--mem", str(mem)]
        if track:
            args.append("--track-dirty")
        args += [KERNEL, ROOTFS]
        pid, fd = spawn(args)
        t = login_root(fd)
        if b"SHELL_2_READY" not in t:
            print(f"   [dd track={track} #{i}] no shell", flush=True)
            kill(pid, fd); time.sleep(0.5); continue
        # Write to /dev/shm (RAM-backed tmpfs): dirties guest RAM directly. The
        # rootfs ext4 is 100% full so disk-backed writes fail; tmpfs is also the
        # right target for the dirty-tracking fault-tax measurement.
        out = run_cmd(fd,
            f"dd if=/dev/zero of=/dev/shm/blob bs=1M count={count_mb} 2>&1 | tail -1",
            wait=3.0, drain_s=12)
        mbs = parse_dd_mbs(out)
        res.append(mbs)
        print(f"   [dd track={track} #{i}] {mbs} MB/s", flush=True)
        kill(pid, fd); time.sleep(0.5)
    return res

def boot_snapshot(name, track, mem, dirty_mb=0, login_first=True):
    """Fresh boot -> (optionally dirty `dirty_mb`) -> Ctrl-A s -> measure the
    write time (spawn-of-snapshot-request -> `[snapshot] ... written` line).
    Returns (write_ms, snap_type, store_dir kept). Leaves the process killed."""
    args = ["--store", STORE, "--name", name, "--mem", str(mem)]
    if track:
        args.append("--track-dirty")
    args += [KERNEL, ROOTFS]
    pid, fd = spawn(args)
    login_root(fd)
    if dirty_mb:
        run_cmd(fd, f"dd if=/dev/zero of=/dev/shm/blob bs=1M count={dirty_mb} 2>&1 | tail -1",
                wait=2.5, drain_s=8)
        run_cmd(fd, "sync", wait=1.0)
    sink = []
    t0 = time.monotonic()
    os.write(fd, b"\x01s")
    write_ms = None
    end = time.monotonic() + 30
    while time.monotonic() < end:
        drain(fd, 0.3, sink=sink)
        joined = b"".join(sink)
        m = RE_SNAPDONE.search(joined)
        if m:
            write_ms = (time.monotonic() - t0) * 1000.0
            snap_type = m.group(1).decode()
            break
    else:
        snap_type = None
    time.sleep(0.4)
    kill(pid, fd)
    return write_ms, snap_type

def diff_layer_from(parent, new_name, mem, dirty_mb):
    """Restore `parent` with --track-dirty --name new_name, dirty `dirty_mb`,
    Ctrl-A s -> Diff. Returns (write_ms, n_dirty_pages, snap_type)."""
    args = ["--store", STORE, "--restore", parent, "--track-dirty",
            "--name", new_name, "--mem", str(mem)]
    pid, fd = spawn(args)
    time.sleep(1.0)
    send_slow(fd, b"\r"); time.sleep(0.5); drain(fd, 3)
    # restored guest is already logged in; make sure we have a shell
    send_slow(fd, b"\r"); drain(fd, 1)
    run_cmd(fd, f"dd if=/dev/zero of=/dev/shm/blob_{new_name} bs=1M count={dirty_mb} 2>&1 | tail -1",
            wait=2.5, drain_s=8)
    run_cmd(fd, "sync", wait=1.0)
    sink = []
    t0 = time.monotonic()
    os.write(fd, b"\x01s")
    write_ms = None; snap_type = None
    end = time.monotonic() + 30
    while time.monotonic() < end:
        drain(fd, 0.3, sink=sink)
        m = RE_SNAPDONE.search(b"".join(sink))
        if m:
            write_ms = (time.monotonic() - t0) * 1000.0
            snap_type = m.group(1).decode()
            break
    time.sleep(0.4)
    kill(pid, fd)
    idx = os.path.join(SNAPS, new_name, "dirty.idx")
    n_dirty = os.path.getsize(idx) // 8 if os.path.exists(idx) else None
    return write_ms, n_dirty, snap_type

def measure_restore(name, samples):
    """Restore `name` repeatedly; return lists of (wall_ms spawn->first output,
    internal Restore-time ms)."""
    wall, internal = [], []
    for i in range(samples):
        sink = []
        t0 = time.monotonic()
        pid, fd = spawn(["--store", STORE, "--restore", name, "--mem", str(MEM)])
        # poke Enter a couple times; time to first non-empty output.
        os.write(fd, b"\r"); time.sleep(0.25); os.write(fd, b"\r")
        first_ms = None
        end = time.monotonic() + 12
        while time.monotonic() < end:
            r, _, _ = select.select([fd], [], [], 0.1)
            if r:
                try: d = os.read(fd, 4096)
                except OSError: break
                if not d: break
                sink.append(d)
                if b"".join(sink).strip():
                    first_ms = (time.monotonic() - t0) * 1000.0
                    break
        # give Restore-time line a moment (it's logged during setup, usually already there)
        drain(fd, 0.6, sink=sink)
        internal_ms = parse_restore_ms(b"".join(sink))
        kill(pid, fd)
        wall.append(first_ms); internal.append(internal_ms)
        print(f"   [restore {name} #{i}] wall={first_ms}ms internal={internal_ms}ms", flush=True)
        time.sleep(0.4)
    return wall, internal

# ---------------------------------------------------------------------------
def main():
    global BOOT, MEM
    ap = argparse.ArgumentParser()
    ap.add_argument("--release", action="store_true", help="use target/release/boot")
    ap.add_argument("--mem", type=int, default=512)
    ap.add_argument("--boot-samples", type=int, default=3)
    ap.add_argument("--dd-samples", type=int, default=3)
    ap.add_argument("--snap-samples", type=int, default=3)
    ap.add_argument("--restore-samples", type=int, default=3)
    ap.add_argument("--out", default=os.path.join(ROOT, "docs/diff-snapshot-benchmarks.md"))
    ap.add_argument("--json", default=None, help="dump raw results json")
    args = ap.parse_args()
    MEM = args.mem
    BOOT = os.path.join(ROOT, "target/release/boot" if args.release else "target/debug/boot")
    build = "release" if args.release else "debug"
    print(f"=== diff-snapshot bench (build={build}, mem={MEM} MiB, boot={BOOT}) ===", flush=True)

    R = {"build": build, "mem": MEM}
    os.system(f"rm -rf {STORE}")

    # --- 1a. boot time tracked vs untracked ---
    print("\n--- 1a. boot time: untracked ---", flush=True)
    dev_u, wall_u = measure_boot(False, args.boot_samples, MEM)
    print("--- 1a. boot time: --track-dirty ---", flush=True)
    dev_t, wall_t = measure_boot(True, args.boot_samples, MEM)
    R["boot_dev_untracked"] = med_spread(dev_u)
    R["boot_dev_tracked"]   = med_spread(dev_t)
    R["boot_wall_untracked"] = med_spread(wall_u)
    R["boot_wall_tracked"]   = med_spread(wall_t)

    # --- 1b. dd throughput tracked vs untracked ---
    print("\n--- 1b. dd 64MiB throughput: untracked ---", flush=True)
    dd_u = measure_dd(False, args.dd_samples, MEM)
    print("--- 1b. dd 64MiB throughput: --track-dirty ---", flush=True)
    dd_t = measure_dd(True, args.dd_samples, MEM)
    R["dd_untracked"] = med_spread(dd_u)
    R["dd_tracked"]   = med_spread(dd_t)

    # --- 2. snapshot write time: Full vs Diff (8MiB, 64MiB) ---
    # Full root write time: fresh boot --track-dirty, no dirtying, Ctrl-A s.
    print("\n--- 2. Full snapshot write time ---", flush=True)
    full_ms = []
    for i in range(args.snap_samples):
        os.system(f"rm -rf {STORE}")
        ms, st = boot_snapshot(f"full{i}", track=True, mem=MEM, dirty_mb=0)
        print(f"   [full #{i}] {ms}ms type={st}", flush=True)
        full_ms.append(ms)
    R["snap_full"] = med_spread(full_ms)

    # Build ONE golden root to diff against (kept for restore tests too).
    print("\n--- build golden root (kept) ---", flush=True)
    os.system(f"rm -rf {STORE}")
    g_ms, g_st = boot_snapshot("golden", track=True, mem=MEM, dirty_mb=0)
    print(f"   [golden] {g_ms}ms type={g_st}", flush=True)

    # Diff write time at 8 MiB and 64 MiB dirtied (each from golden, discard after).
    diff_results = {}
    for dirty_mb in (8, 64):
        print(f"\n--- 2. Diff snapshot write time ({dirty_mb} MiB dirtied) ---", flush=True)
        ms_list, pg_list = [], []
        for i in range(args.snap_samples):
            name = f"diff_{dirty_mb}m_{i}"
            ms, npg, st = diff_layer_from("golden", name, MEM, dirty_mb)
            print(f"   [diff {dirty_mb}MiB #{i}] {ms}ms pages={npg} type={st}", flush=True)
            ms_list.append(ms); pg_list.append(npg)
            # remove the throwaway diff so the chain stays clean
            os.system(f"rm -rf {os.path.join(SNAPS, name)}")
        diff_results[dirty_mb] = (med_spread(ms_list), med_spread(pg_list))
    R["snap_diff"] = {k: (v[0], v[1]) for k, v in diff_results.items()}

    # --- 4. footprint: golden full vs one diff layer ---
    golden_mem = os.path.join(SNAPS, "golden", "memory.bin")
    R["full_mem_logical"] = os.path.getsize(golden_mem)
    R["full_mem_phys"]    = phys_bytes(golden_mem)

    # --- 3. restore latency by chain depth ---
    # (a) Full-only: restore golden (1 layer).
    print("\n--- 3a. restore Full-only (golden, 1 layer) ---", flush=True)
    w0, i0 = measure_restore("golden", args.restore_samples)
    R["restore_full"] = (med_spread(w0), med_spread(i0))

    # (b) golden + 1 diff. Build d1 off golden (8 MiB dirtied), keep it.
    print("\n--- build chain: golden -> d1 -> d2 -> d3 (8 MiB each, kept) ---", flush=True)
    chain = ["golden"]
    diff_footprint = []
    for depth, parent in [(1, "golden"), (2, "d1"), (3, "d2")]:
        nm = f"d{depth}"
        ms, npg, st = diff_layer_from(parent, nm, MEM, 8)
        memp = os.path.join(SNAPS, nm, "memory.bin")
        log = os.path.getsize(memp); phys = phys_bytes(memp)
        diff_footprint.append((nm, npg, log, phys))
        print(f"   [chain {nm}] write={ms}ms pages={npg} mem_logical={log/1e6:.2f}MB phys={phys/1e6:.2f}MB type={st}", flush=True)
        chain.append(nm)
    R["diff_footprint"] = diff_footprint

    print("\n--- 3b. restore golden+1 diff (d1) ---", flush=True)
    w1, i1 = measure_restore("d1", args.restore_samples)
    R["restore_diff1"] = (med_spread(w1), med_spread(i1))

    print("\n--- 3c. restore golden+3 diffs (d3) ---", flush=True)
    w3, i3 = measure_restore("d3", args.restore_samples)
    R["restore_diff3"] = (med_spread(w3), med_spread(i3))

    # total store size for the chain (golden + d1..d3) — du-style
    def dir_size(p):
        tot = 0
        for dp, _, fs in os.walk(p):
            for f in fs:
                fp = os.path.join(dp, f)
                try: tot += os.stat(fp).st_blocks * 512
                except OSError: pass
        return tot
    R["chain_store_phys"] = dir_size(SNAPS)

    if args.json:
        with open(args.json, "w") as f:
            json.dump(R, f, indent=2, default=str)

    os.system(f"rm -rf {STORE}")
    write_report(args.out, R)
    print("\n=== DONE; report:", args.out, "===", flush=True)
    print(json.dumps(R, indent=2, default=str))

def fmt(ms):
    med, lo, hi, n = ms
    if med is None:
        return "n/a"
    if isinstance(med, float):
        return f"{med:.0f} (min {lo:.0f}, max {hi:.0f}, n={n})"
    return f"{med} (min {lo}, max {hi}, n={n})"

def write_report(path, R):
    # The actual prose/table is authored separately (docs file is hand-curated);
    # here we only dump a machine-readable block the author can paste. Keeping the
    # harness from clobbering curated prose: if the file exists, append a fenced
    # RAW RESULTS block update instead of overwriting.
    raw = "```json\n" + json.dumps(R, indent=2, default=str) + "\n```\n"
    sys.stdout.write("\n----- RAW RESULTS (for report) -----\n")
    sys.stdout.write(raw)

if __name__ == "__main__":
    main()
