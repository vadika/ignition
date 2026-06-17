#!/usr/bin/env python3
"""Live HVF proof for vmid (per-clone CRNG reseed on restore).

No --net (no sudo): plain rootfs + vsock only. Steps:
  1. Cold-boot the plain rootfs WITH --vsock-uds (so the guest has a virtio-vsock
     device and the local.d `socat VSOCK-LISTEN:9000 EXEC:/usr/bin/vmid-reseed`
     listener comes up), log in, snapshot as a base, quit.
  2. Restore clones and read /dev/urandom, comparing reseeded vs --no-reseed.

PASS criteria (what is verifiable on this platform):
  - the host prints "vmid: pushed fresh CRNG seed" on each reseeded clone, AND
  - the two reseeded clones produce DIFFERENT /dev/urandom (distinct pushed seeds).

The no-reseed clones are read for information only. NOTE (verified 2026-06-17 on
M-series HVF, guest kernel 6.1, aarch64): the "two no-reseed clones emit IDENTICAL
bytes" bug does NOT reproduce here, even with virtio-rng disabled (IGN_NO_RNG=1).
The guest CPU exposes no arch RNG (no `rng` in /proc/cpuinfo Features, so no RNDR),
and `random: crng init done` fires at t=0 from the fixed FDT rng-seed -- so the
CRNG state IS identical across clones at the instant of resume. But the kernel
mixes interrupt-timing entropy (add_interrupt_randomness) and reseeds within the
first scheduling quantum after resume, before a serial-shell read can run, so
siblings diverge regardless. The shared-CRNG window is real but sub-millisecond
here; vmid closes it deterministically (and matters more on configs that generate
randomness in early userspace before interrupts flow, or for deterministic replay).
"""
import os
import re
import select
import shutil
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BOOT = os.path.join(ROOT, "target/debug/boot")
KERNEL = os.path.join(ROOT, "kimage/out/Image")
ROOTFS = os.path.join(ROOT, "kimage/out/rootfs.ext4")
STORE = os.path.join(ROOT, "vmid-store")
NAME = "vmidbase"
CTRL_A = b"\x01"


def spawn(args):
    return subprocess.Popen(
        [BOOT, *args],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,  # merge: host "vmid:" lines + guest serial in one stream
        bufsize=0,
    )


def expect(proc, pattern, timeout, label="", echo=True):
    """Read merged output until `pattern` (regex) is seen. Returns the full text
    read. Raises TimeoutError on timeout."""
    rx = re.compile(pattern)
    buf = []
    deadline = time.time() + timeout
    while time.time() < deadline:
        r, _, _ = select.select([proc.stdout], [], [], 0.25)
        if not r:
            if proc.poll() is not None:
                break
            continue
        chunk = os.read(proc.stdout.fileno(), 4096)
        if not chunk:
            break
        text = chunk.decode("utf-8", "replace")
        if echo:
            sys.stdout.write(text)
            sys.stdout.flush()
        buf.append(text)
        if rx.search("".join(buf)):
            return "".join(buf)
    raise TimeoutError(f"[{label}] never matched /{pattern}/ within {timeout}s")


def send(proc, data):
    proc.stdin.write(data)
    proc.stdin.flush()


def send_slow(proc, data, delay=0.008):
    """Feed input one byte at a time so the guest's small 16550 RX FIFO (16 bytes)
    can drain between bytes. A whole command line sent at once overflows it."""
    for b in data:
        proc.stdin.write(bytes([b]))
        proc.stdin.flush()
        time.sleep(delay)


def login(proc):
    expect(proc, r"login:", 60, "login-prompt")
    send_slow(proc, b"root\n")
    expect(proc, r"~#", 20, "shell-prompt")  # busybox prompt "(none):~#"
    # Confirm the shell actually executes: the OUTPUT "MARK_42" (the echoed command
    # line shows the literal "MARK_$((6*7))", so only real execution matches).
    send_slow(proc, b"echo MARK_$((6*7))\n")
    expect(proc, r"MARK_42", 20, "shell-ready")


def make_base():
    os.makedirs(STORE, exist_ok=True)
    print(f"\n=== Phase 1: cold-boot + snapshot base '{NAME}' ===")
    proc = spawn(["--mem", "512", "--vsock-uds", "/tmp/vmidbase.sock", "--force",
                  "--store", STORE, "--name", NAME, KERNEL, ROOTFS])
    try:
        login(proc)
        print(">> shell ready; snapshotting")
        send(proc, CTRL_A + b"s")
        expect(proc, r"\[snapshot\].*written", 30, "snapshot-written")
        time.sleep(1)
        send(proc, CTRL_A + b"x")
        try:
            proc.wait(timeout=15)
        except subprocess.TimeoutExpired:
            proc.kill()
    finally:
        if proc.poll() is None:
            proc.kill()


def read_urandom(reseed, tag):
    """Restore a clone, optionally reseeded, read /dev/urandom promptly. Returns
    (hex_value, pushed_bool)."""
    args = ["--restore", NAME, "--store", STORE,
            "--vsock-uds", f"/tmp/vmid-{tag}.sock", KERNEL, ROOTFS]
    if not reseed:
        args = ["--no-reseed"] + args
    print(f"\n--- restore clone '{tag}' (reseed={reseed}) ---")
    proc = spawn(args)
    pushed = False
    try:
        if reseed:
            try:
                expect(proc, r"vmid: pushed fresh CRNG seed", 8, "reseed-push")
                pushed = True
            except TimeoutError:
                print(">> WARN: no reseed-push line seen")
        else:
            time.sleep(1.0)  # let the shell resume; do NOT wait for a push
        # Resumed at the post-snapshot shell. Read /dev/urandom, marked for parsing.
        send_slow(proc, b"printf RAND=; head -c 16 /dev/urandom | od -A n -t x1 | tr -d ' \\n'; printf '\\n'\n")
        out = expect(proc, r"RAND=[0-9a-f]{32}", 20, f"urandom-{tag}")
        val = re.search(r"RAND=([0-9a-f]{32})", out).group(1)
        send(proc, CTRL_A + b"x")
        try:
            proc.wait(timeout=15)
        except subprocess.TimeoutExpired:
            proc.kill()
        return val, pushed
    finally:
        if proc.poll() is None:
            proc.kill()


def main():
    for p in (BOOT, KERNEL, ROOTFS):
        if not os.path.exists(p):
            print(f"missing: {p}", file=sys.stderr)
            return 2
    make_base()

    print("\n=== Phase 2: restore clones, compare /dev/urandom ===")
    n1, _ = read_urandom(reseed=False, tag="noreseed1")
    n2, _ = read_urandom(reseed=False, tag="noreseed2")
    v1, p1 = read_urandom(reseed=True, tag="reseed1")
    v2, p2 = read_urandom(reseed=True, tag="reseed2")

    print("\n=== RESULTS ===")
    print(f"no-reseed clone 1 : {n1}")
    print(f"no-reseed clone 2 : {n2}")
    print(f"reseed    clone 1 : {v1}   (push seen: {p1})")
    print(f"reseed    clone 2 : {v2}   (push seen: {p2})")

    fixed = (v1 != v2)
    pushed = p1 and p2
    print(f"\nreseed push observed on both reseed clones : {pushed}")
    print(f"reseed clones divergent (vmid works)       : {fixed}")
    print(f"[info] no-reseed clones identical          : {n1 == n2}  "
          "(not expected to reproduce here; see module docstring)")

    ok = fixed and pushed
    print("\nPASS — vmid mechanism verified live" if ok else "\nFAIL")

    # Leave no untracked scratch behind (the snapshot store is regenerated each run).
    shutil.rmtree(STORE, ignore_errors=True)
    for tag in ("base", "noreseed1", "noreseed2", "reseed1", "reseed2"):
        try:
            os.unlink(f"/tmp/vmid-{tag}.sock")
        except FileNotFoundError:
            pass
    try:
        os.unlink("/tmp/vmidbase.sock")
    except FileNotFoundError:
        pass
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
