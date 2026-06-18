#!/usr/bin/env python3
"""Live FC-sequence proof for ignition-fc-api against a REAL boot child.

Drives the Firecracker route subset end to end on a real microVM, then
clones-from-snapshot in a second server:

  server A: machine-config -> boot-source -> drive -> InstanceStart
            -> poll GET / until Running
            -> PATCH /vm Paused -> snapshot/create -> PATCH /vm Resumed
  server B (same --store): boot-source + drive -> snapshot/load resume_vm
            -> poll GET / until Running

Requirements (this is a by-hand proof, NOT a CI test — no HVF in CI):
  - an Apple-Silicon Mac with Hypervisor.framework
  - a signed boot binary:
        cargo build -p ignition-spike --bin boot
        scripts/sign.sh target/debug/boot
  - the tools-base guest assets: kimage/out/Image, kimage/out/rootfs-tools.ext4
        (built per the MCP server / sandbox-bench instructions)

Stdlib only; reuses the UDS HTTP client pattern from fc_api_mock_test.py.
"""
import http.client, json, os, socket, subprocess, sys, tempfile, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
API_BIN = os.path.join(ROOT, "target/debug/ignition-fc-api")
BOOT_BIN = os.path.join(ROOT, "target/debug/boot")
KERNEL = os.path.join(ROOT, "kimage/out/Image")
ROOTFS = os.path.join(ROOT, "kimage/out/rootfs-tools.ext4")
# Cold-boot cmdline for the overlay-root tools rootfs (mirrors scripts/sandbox_bench.py
# COLD_APPEND and scripts/make-tools-base.sh). The restore carries this in the snapshot.
BOOT_ARGS = "ro init=/sbin/overlay-init"


class UDSConn(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost")
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(self.path)


def req(api, method, route, body=None):
    c = UDSConn(api)
    c.request(method, route, json.dumps(body) if body is not None else None,
              {"Content-Type": "application/json"})
    r = c.getresponse()
    data = r.read()
    c.close()
    return r.status, data


def wait_socket(path, timeout=10.0):
    """Wait for the api-sock to appear and accept a connection."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if os.path.exists(path):
            try:
                s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                s.connect(path)
                s.close()
                return True
            except OSError:
                pass
        time.sleep(0.05)
    return False


def poll_state(api, want, timeout=60.0):
    """Poll GET / until state == want. Returns the last observed state."""
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        status, data = req(api, "GET", "/")
        if status == 200:
            last = json.loads(data).get("state")
            if last == want:
                return last
        time.sleep(0.1)
    return last


def start_server(api, store):
    return subprocess.Popen(
        [API_BIN, "--api-sock", api, "--store", store,
         "--boot", BOOT_BIN, "--kernel", KERNEL],
        stderr=subprocess.PIPE)


def put_config(api):
    """PUT the machine-config + boot-source + root drive shared by both phases."""
    assert req(api, "PUT", "/machine-config",
               {"vcpu_count": 1, "mem_size_mib": 512, "track_dirty_pages": True})[0] == 204
    assert req(api, "PUT", "/boot-source",
               {"kernel_image_path": KERNEL, "boot_args": BOOT_ARGS})[0] == 204
    assert req(api, "PUT", "/drives/rootfs",
               {"drive_id": "rootfs", "path_on_host": ROOTFS, "is_root_device": True})[0] == 204


def stop(proc):
    if proc and proc.poll() is None:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()


def dump_stderr(proc, label):
    try:
        err = proc.stderr.read().decode(errors="replace")
        if err:
            sys.stderr.write(f"--- {label} stderr ---\n{err}\n")
    except Exception:
        pass


def main():
    for label, path in (("ignition-fc-api", API_BIN), ("boot", BOOT_BIN),
                         ("kernel", KERNEL), ("rootfs", ROOTFS)):
        if not os.path.exists(path):
            sys.stderr.write(f"missing {label}: {path}\n")
            return 2

    d = tempfile.mkdtemp(prefix="fcapi-live-")
    store = os.path.join(d, "store")
    snap_path = os.path.join(d, "snap.state")  # opaque handle (basename -> store name)
    api_a = os.path.join(d, "api_a.sock")
    api_b = os.path.join(d, "api_b.sock")
    srv_a = srv_b = None
    ok = False
    try:
        # --- Phase A: boot a real VM, snapshot it while paused ---
        srv_a = start_server(api_a, store)
        assert wait_socket(api_a), "server A api-sock never came up"
        put_config(api_a)
        assert req(api_a, "PUT", "/actions", {"action_type": "InstanceStart"})[0] == 204
        st = poll_state(api_a, "Running")
        assert st == "Running", f"VM A never reached Running (last={st!r})"

        assert req(api_a, "PATCH", "/vm", {"state": "Paused"})[0] == 204
        assert req(api_a, "PUT", "/snapshot/create", {"snapshot_path": snap_path})[0] == 204
        assert req(api_a, "PATCH", "/vm", {"state": "Resumed"})[0] == 204
        st = poll_state(api_a, "Running")
        assert st == "Running", f"VM A not Running after resume (last={st!r})"

        # Stop A before cloning so only one VM touches the store at a time.
        stop(srv_a)
        srv_a = None

        # --- Phase B: clone from the snapshot in a fresh server (same store) ---
        srv_b = start_server(api_b, store)
        assert wait_socket(api_b), "server B api-sock never came up"
        # load-only still needs the boot-source + root drive positionals.
        put_config(api_b)
        assert req(api_b, "PUT", "/snapshot/load",
                   {"snapshot_path": snap_path, "resume_vm": True})[0] == 204
        st = poll_state(api_b, "Running")
        assert st == "Running", f"cloned VM B never reached Running (last={st!r})"

        print("fc_api_live_test PASS")
        ok = True
    finally:
        if not ok:
            if srv_a:
                dump_stderr(srv_a, "server A")
            if srv_b:
                dump_stderr(srv_b, "server B")
        stop(srv_a)
        stop(srv_b)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
