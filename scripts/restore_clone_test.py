#!/usr/bin/env python3
# Verify restore is interactive (login + run a command) AND clone-capable
# (restore the same snapshot N times into independent guests).
import os, pty, sys, time, select, subprocess, signal

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BOOT = os.path.join(ROOT, "target/debug/boot")
SNAP = os.path.join(ROOT, "snapshot2")  # reuse snapshot from restore_test.py

def spawn(args):
    pid, fd = pty.fork()
    if pid == 0:
        os.execv(BOOT, [BOOT] + args)
        os._exit(127)
    return pid, fd

def drain(fd, seconds, until=None):
    out = b""; end = time.time() + seconds
    while time.time() < end:
        r,_,_ = select.select([fd], [], [], 0.2)
        if r:
            try: d = os.read(fd, 4096)
            except OSError: break
            if not d: break
            out += d
            if until and until in out: break
    return out

def restore_session(idx):
    pid, fd = spawn(["--restore", SNAP])
    drain(fd, 2)
    os.write(fd, b"\r"); time.sleep(0.3)
    pre = drain(fd, 2, until=b"login:")
    # try to log in as root (rootfs may or may not require a password)
    os.write(fd, b"root\r"); time.sleep(0.5)
    drain(fd, 2)
    # run a command that exercises timekeeping + userspace
    os.write(fd, b"uname -sr; echo CLONE-%d-OK\r" % idx); time.sleep(0.5)
    out = drain(fd, 3)
    cpu = subprocess.check_output(["ps","-o","%cpu=","-p",str(pid)]).decode().strip()
    os.kill(pid, signal.SIGKILL); os.waitpid(pid, 0); os.close(fd)
    marker = (b"CLONE-%d-OK" % idx) in out
    text = (pre + out).decode(errors="replace")
    return marker, cpu, text

if not os.path.exists(os.path.join(SNAP, "memory.bin")):
    print("no snapshot2 — run restore_test.py first"); sys.exit(1)

for i in range(2):  # two independent restores from the same snapshot = clones
    ok, cpu, text = restore_session(i)
    tail = text.strip().splitlines()[-4:]
    print(f"=== restore/clone #{i}: marker={ok} cpu={cpu}% ===")
    for line in tail: print("   ", line)
print("\nDONE")
