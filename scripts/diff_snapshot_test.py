#!/usr/bin/env python3
# Headless end-to-end driver for the diff/incremental-snapshot feature.
#
# Proves, on real HVF hardware:
#   Phase A  - boot fresh --track-dirty, log in, write a sentinel, Ctrl-A s ->
#              Full root.  Record root memory.bin size (~= mem MiB).
#   Phase B  - restore root --track-dirty, verify the sentinel survived, mutate a
#              BOUNDED known region (an 8 MiB blob), sync, Ctrl-A s -> Diff layer
#              (parent=root, only dirty pages packed).  The single-process "two
#              Ctrl-A s" path cannot make a differently-named diff (one write_name
#              per process + the same-name-as-parent guard), so the DESIGNED diff
#              path is restore-then-resnapshot, per the plan doc.
#   Size     - assert Diff memory.bin physical/logical size << full root.
#   Phase C  - restore the LEAF (the diff); assert it reaches a usable prompt, the
#              post-diff blob AND the pre-root marker both read back, idle CPU low.
#   Immut.   - md5 of every stored layer's artifacts unchanged before/after restore.
#
# Not a unit test (needs the hypervisor entitlement + a real kernel/rootfs); a live
# integration driver.  Modeled on scripts/restore_test.py (pty spawn, drain, md5,
# Ctrl-A escape) and scripts/restore_clone_test.py (login + run-a-command).
import os, pty, sys, time, select, subprocess, signal, json, hashlib, glob

ROOT   = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BOOT   = os.path.join(ROOT, "target/debug/boot")
KERNEL = os.path.join(ROOT, "kimage/out/Image")
ROOTFS = os.path.join(ROOT, "kimage/out/rootfs.ext4")
STORE  = os.path.join(ROOT, "vmstore-difftest")
SNAPS  = os.path.join(STORE, "snapshots")

MEM_MIB   = 512
FULL_MB   = float(MEM_MIB)
BLOB_MB   = 8                       # bounded mutation region
DIFF_MAX_MB = FULL_MB * 0.25        # diff must be < 25% of full RAM

def md5(path):
    h = hashlib.md5()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()

def phys_bytes(path):
    """Physically-allocated bytes (st_blocks*512); for a sparse/packed file this
    is the real on-disk cost."""
    st = os.stat(path)
    return st.st_blocks * 512

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

def send_slow(fd, data, chunk=8, pause=0.06):
    """Feed host input to the guest in <=chunk-byte bursts with a pause between
    them.  The guest UART RX FIFO is only 16 bytes; a full command line written
    at once overflows it ("serial RX dropped byte: No space left in FIFO") and
    the guest sees a corrupted line.  Pacing lets the kernel drain the FIFO."""
    if isinstance(data, str):
        data = data.encode()
    for i in range(0, len(data), chunk):
        os.write(fd, data[i:i + chunk])
        time.sleep(pause)

def cpu_pct(pid):
    try:
        o = subprocess.check_output(["ps", "-o", "%cpu=", "-p", str(pid)]).decode().strip()
        return float(o)
    except Exception:
        return -1.0

def kill(pid, fd):
    try: os.kill(pid, signal.SIGKILL)
    except ProcessLookupError: pass
    try: os.waitpid(pid, 0)
    except ChildProcessError: pass
    try: os.close(fd)
    except OSError: pass

def login_root(fd):
    """Wait for / wake a login prompt and log in as root.  The rootfs root account
    has no password (busybox login accepts `root\\n`).  Returns the console text."""
    txt = drain(fd, 6, until=b"login:")
    if b"login:" not in txt:
        send_slow(fd, b"\r"); time.sleep(0.4)
        txt += drain(fd, 4, until=b"login:")
    send_slow(fd, b"root\r"); time.sleep(0.8)
    txt += drain(fd, 3)
    # If a password prompt appears, send empty password.
    if b"assword" in txt.lower():
        send_slow(fd, b"\r"); time.sleep(0.5)
        txt += drain(fd, 2)
    # Confirm we have a shell by running a unique echo.
    send_slow(fd, b"echo SHELL_$((1+1))_READY\r"); time.sleep(0.6)
    txt += drain(fd, 3)
    return txt

def run_cmd(fd, cmd, wait=1.2, drain_s=3):
    send_slow(fd, cmd.encode() + b"\r"); time.sleep(wait)
    return drain(fd, drain_s)

def fail(msg):
    print("\nFAILURE:", msg)
    sys.exit(1)

# ----------------------------------------------------------------------------
os.system(f"rm -rf {STORE}")
print("=== Phase A: fresh boot --track-dirty -> Full root ===", flush=True)
pidA, fdA = spawn(["--store", STORE, "--name", "root", "--track-dirty",
                   "--mem", str(MEM_MIB), KERNEL, ROOTFS])
t = login_root(fdA)
got_shell = b"SHELL_2_READY" in t
print(f"[A login -> shell ready: {got_shell}]", flush=True)
if not got_shell:
    sys.stdout.buffer.write(t[-600:]); print()
    kill(pidA, fdA); fail("could not reach a shell in Phase A boot")

# Write a KNOWN sentinel into RAM-backed tmpfs, sync, then snapshot.
run_cmd(fdA, "echo SENTINEL_BEFORE > /root/marker && sync && cat /root/marker", wait=1.0)
os.write(fdA, b"\x01s")                       # Ctrl-A s -> Full root
print("[A sent Ctrl-A s, waiting for Full root write]", flush=True)
root_dir = os.path.join(SNAPS, "root")
root_mf  = os.path.join(root_dir, "manifest.json")
deadline = time.time() + 12
while time.time() < deadline and not os.path.exists(root_mf):
    drain(fdA, 0.5)
ok_root = os.path.exists(root_mf)
print(f"[A root manifest.json present: {ok_root}]", flush=True)
if not ok_root:
    kill(pidA, fdA); fail("Full root snapshot was not written")
time.sleep(0.5)
root_manifest = json.load(open(root_mf))
print(f"[A root manifest: type={root_manifest['snapshot_type']} parent={root_manifest['parent']}]", flush=True)
if root_manifest["snapshot_type"] != "Full":
    kill(pidA, fdA); fail(f"root is not Full: {root_manifest}")
root_mem = os.path.join(root_dir, "memory.bin")
root_mem_logical = os.path.getsize(root_mem)
root_mem_phys    = phys_bytes(root_mem)
print(f"[A root memory.bin: logical={root_mem_logical/1e6:.1f}MB phys={root_mem_phys/1e6:.1f}MB]", flush=True)
kill(pidA, fdA)

# ----------------------------------------------------------------------------
print("\n=== Phase B: restore root --track-dirty -> mutate -> Diff layer ===", flush=True)
# Names present before the diff write (so we can detect the auto-generated one).
before_names = set(os.listdir(SNAPS))
pidB, fdB = spawn(["--store", STORE, "--restore", "root", "--track-dirty",
                   "--mem", str(MEM_MIB)])
# A restored guest resumes PAST login; wake the shell with a CR.
time.sleep(1.0)
send_slow(fdB, b"\r"); time.sleep(0.5)
t = drain(fdB, 4)
# Verify the pre-snapshot marker survived the restore.
mt = run_cmd(fdB, "cat /root/marker", wait=1.0)
marker_survived_restore = b"SENTINEL_BEFORE" in mt
print(f"[B marker survived restore: {marker_survived_restore}]", flush=True)
# Mutate a BOUNDED known region: 8 MiB written with /dev/zero, then sync. Writing
# to /root (disk-backed ext4) dirties guest RAM page-cache pages -> Diff layer.
run_cmd(fdB, f"dd if=/dev/zero of=/root/blob bs=1M count={BLOB_MB} 2>&1 | tail -1", wait=2.5, drain_s=5)
bt = run_cmd(fdB, "sync; wc -c /root/blob; md5sum /root/blob 2>/dev/null || true", wait=2.0, drain_s=5)
print("[B blob written, guest reports:]")
for line in bt.decode(errors="replace").strip().splitlines()[-4:]:
    print("   ", line)
# Capture the guest's md5 of the blob (best-effort) for the leaf-restore check.
guest_blob_md5 = None
for tok in bt.decode(errors="replace").split():
    if len(tok) == 32 and all(c in "0123456789abcdef" for c in tok):
        guest_blob_md5 = tok; break

os.write(fdB, b"\x01s")                       # Ctrl-A s -> Diff (parent=root)
print("[B sent Ctrl-A s, waiting for Diff layer write]", flush=True)
diff_name = None
deadline = time.time() + 12
while time.time() < deadline:
    drain(fdB, 0.5)
    now = set(os.listdir(SNAPS))
    new = [n for n in (now - before_names)
           if os.path.exists(os.path.join(SNAPS, n, "manifest.json"))]
    if new:
        diff_name = new[0]; break
print(f"[B diff layer name: {diff_name}]", flush=True)
if not diff_name:
    kill(pidB, fdB); fail("Diff layer was not written (no new snapshot dir)")
time.sleep(0.5)
kill(pidB, fdB)

# ---- Assertions on the Diff layer ----
diff_dir = os.path.join(SNAPS, diff_name)
diff_manifest = json.load(open(os.path.join(diff_dir, "manifest.json")))
print(f"[diff manifest: type={diff_manifest['snapshot_type']} parent={diff_manifest['parent']}]", flush=True)
if diff_manifest["snapshot_type"] != "Diff":
    fail(f"diff layer is not Diff type: {diff_manifest}")
if diff_manifest["parent"] != "root":
    fail(f"diff parent != root: {diff_manifest['parent']}")
diff_idx = os.path.join(diff_dir, "dirty.idx")
if not os.path.exists(diff_idx):
    fail("diff layer missing dirty.idx")
diff_mem = os.path.join(diff_dir, "memory.bin")
diff_mem_logical = os.path.getsize(diff_mem)
diff_mem_phys    = phys_bytes(diff_mem)
n_dirty_idx = os.path.getsize(diff_idx) // 8   # u64 page indices
print(f"[diff memory.bin: logical={diff_mem_logical/1e6:.2f}MB phys={diff_mem_phys/1e6:.2f}MB "
      f"dirty.idx pages={n_dirty_idx}]", flush=True)
# A packed diff memory.bin is NOT sparse: logical == n_dirty_pages * 16384.
PAGE = 16384
expected_logical = n_dirty_idx * PAGE
if diff_mem_logical != expected_logical:
    fail(f"diff memory.bin logical {diff_mem_logical} != n_dirty*{PAGE} {expected_logical}")
diff_mb = diff_mem_logical / 1e6
diff_smaller = diff_mem_logical < (DIFF_MAX_MB * 1e6)
print(f"[diff_smaller (<{DIFF_MAX_MB:.0f}MB): {diff_smaller}  diff_mb={diff_mb:.2f}  full_mb={FULL_MB:.1f}]", flush=True)
if not diff_smaller:
    fail(f"diff memory.bin {diff_mb:.1f}MB not << full {FULL_MB:.0f}MB")

# ---- Record md5 of EVERY layer's artifacts BEFORE leaf restore ----
ARTIFACTS = ["memory.bin", "disk.img", "gic.bin", "vmstate.json"]
def layer_hashes(name):
    d = os.path.join(SNAPS, name)
    h = {}
    for a in ARTIFACTS:
        p = os.path.join(d, a)
        if os.path.exists(p) and os.path.getsize(p) > 0:
            h[a] = md5(p)
    return h
before_hashes = {n: layer_hashes(n) for n in ("root", diff_name)}

# ----------------------------------------------------------------------------
print("\n=== Phase C: restore the LEAF (diff) -> verify mutation + marker ===", flush=True)
pidC, fdC = spawn(["--store", STORE, "--restore", diff_name, "--mem", str(MEM_MIB)])
time.sleep(1.0)
send_slow(fdC, b"\r"); time.sleep(0.5)
resp = drain(fdC, 5)
send_slow(fdC, b"\r"); time.sleep(0.4)
resp += drain(fdC, 3)
leaf_responsive = len(resp.strip()) > 0
print(f"[C leaf responsive: {leaf_responsive}  bytes={len(resp)}]", flush=True)
# Read back the post-diff blob and the pre-root marker.
ck = run_cmd(fdC, "wc -c /root/blob; md5sum /root/blob 2>/dev/null; cat /root/marker", wait=1.5, drain_s=4)
ck_txt = ck.decode(errors="replace")
print("[C leaf guest readback:]")
for line in ck_txt.strip().splitlines()[-5:]:
    print("   ", line)
expected_bytes = BLOB_MB * 1024 * 1024
blob_present = (str(expected_bytes) in ck_txt) or ("/root/blob" in ck_txt and "No such" not in ck_txt and str(expected_bytes) in ck_txt)
# Stronger: byte count line must show the exact size.
blob_present = str(expected_bytes) in ck_txt and "No such file" not in ck_txt
blob_md5_match = (guest_blob_md5 is not None) and (guest_blob_md5 in ck_txt)
marker_present = b"SENTINEL_BEFORE" in ck
mutation_present = blob_present and marker_present
print(f"[C blob_present={blob_present} blob_md5_match={blob_md5_match} marker_present={marker_present}]", flush=True)
# CPU after settle (should idle low).
time.sleep(1.0)
samples = [cpu_pct(pidC) for _ in range(5) if not time.sleep(0.5)]
valid = [s for s in samples if s >= 0]
restore_cpu = (sum(valid) / len(valid)) if valid else -1.0
print(f"[C restore CPU% samples: {samples}  avg={restore_cpu:.1f}]", flush=True)
kill(pidC, fdC)

# ---- Immutability: every stored layer's artifacts unchanged ----
after_hashes = {n: layer_hashes(n) for n in ("root", diff_name)}
immutable = (before_hashes == after_hashes)
print(f"[immutable layers (root + {diff_name}): {immutable}]", flush=True)
if not immutable:
    for n in before_hashes:
        if before_hashes[n] != after_hashes.get(n):
            print(f"   MUTATED layer {n}: before={before_hashes[n]} after={after_hashes.get(n)}")

# ----------------------------------------------------------------------------
# Cleanup the test store (avoid disk bloat; also gitignored so never committed).
os.system(f"rm -rf {STORE}")

all_ok = (diff_smaller and leaf_responsive and mutation_present and immutable
          and marker_survived_restore)
print("\n" + "=" * 70)
print(f"diff_smaller={diff_smaller} root_mb={FULL_MB:.1f} diff_mb={diff_mb:.2f} "
      f"leaf_responsive={leaf_responsive} mutation_present={mutation_present} "
      f"immutable={immutable} restore_cpu={restore_cpu:.1f}%")
print("=" * 70)
sys.exit(0 if all_ok else 1)
