#!/usr/bin/env python3
"""Live virtio-vsock E2 (host->guest) round-trip.

Boots a guest whose init starts a vsock listener on a known port, then from the
host connects to the control socket {uds}, issues CONNECT <port>, expects
`OK <host_port>`, and echoes a string into the guest, reading it back.

Requires the hypervisor entitlement + a kernel/rootfs whose init runs e.g.
`socat VSOCK-LISTEN:5000,fork EXEC:cat` (echo server). Adjust PORT/UDS/paths
to the local setup. Exit 0 on a successful round trip.
"""
import os
import socket
import subprocess
import sys
import time

UDS = "/tmp/ignition-vsock-e2"
PORT = 5000
KERNEL = os.environ.get("IGN_KERNEL", "kimage/out/Image")
ROOTFS = os.environ.get("IGN_ROOTFS", "kimage/out/rootfs.ext4")
BOOT = os.environ.get("IGN_BOOT", "target/debug/boot")


def main() -> int:
    for p in (UDS, f"{UDS}_{PORT}"):
        try:
            os.unlink(p)
        except FileNotFoundError:
            pass

    proc = subprocess.Popen(
        [BOOT, "--vsock-uds", UDS, KERNEL, ROOTFS],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.STDOUT,
    )
    try:
        # Wait for the control socket to appear (guest boot + listener bind).
        deadline = time.time() + 60
        while not os.path.exists(UDS):
            if time.time() > deadline:
                print("FAIL: control socket never appeared", file=sys.stderr)
                return 1
            time.sleep(0.5)
        time.sleep(2)  # let the in-guest listener come up

        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.settimeout(10)
        s.connect(UDS)
        s.sendall(f"CONNECT {PORT}\n".encode())
        ack = s.recv(64).decode()
        if not ack.startswith("OK "):
            print(f"FAIL: expected OK, got {ack!r}", file=sys.stderr)
            return 1

        s.sendall(b"ping-e2\n")
        echo = s.recv(64)
        if b"ping-e2" not in echo:
            print(f"FAIL: no echo, got {echo!r}", file=sys.stderr)
            return 1

        print("PASS: host->guest vsock round trip OK")
        return 0
    finally:
        try:
            s.close()
        except NameError:
            pass
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()


if __name__ == "__main__":
    sys.exit(main())
