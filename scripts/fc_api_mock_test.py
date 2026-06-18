#!/usr/bin/env python3
"""Integration test for ignition-fc-api with a MOCK boot (no HVF).

Stub boot binds --control-sock, ACKs control lines, records actions. We drive the
FC sequence over the api-sock and assert status codes + that snapshot reached the
stub. Pause/resume are advisory (REST-state only) and never hit the control socket.
Stdlib only.
"""
import http.client, json, os, socket, stat, subprocess, sys, tempfile, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

STUB = r'''
import socket, sys, os, threading
ctl = sys.argv[sys.argv.index("--control-sock")+1]
rec = ctl + ".actions"
try: os.unlink(ctl)
except FileNotFoundError: pass
srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); srv.bind(ctl); srv.listen(8)
def serve(c):
    f = c.makefile("rwb")
    for line in f:
        open(rec, "a").write(line.decode())
        f.write(b'{"ok":true}\n'); f.flush()
while True:
    c,_ = srv.accept(); threading.Thread(target=serve, args=(c,), daemon=True).start()
'''

class UDSConn(http.client.HTTPConnection):
    def __init__(self, path): super().__init__("localhost"); self.path = path
    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); self.sock.connect(self.path)

def req(api, method, route, body=None):
    c = UDSConn(api)
    c.request(method, route, json.dumps(body) if body is not None else None,
              {"Content-Type": "application/json"})
    r = c.getresponse(); data = r.read(); c.close()
    return r.status, data

def main():
    d = tempfile.mkdtemp(prefix="fcapi-")
    api = os.path.join(d, "api.sock")
    stub = os.path.join(d, "stub_boot.py")
    open(stub, "w").write(STUB)
    # Wrapper executable used as --boot, so the server's `<boot> --control-sock ...` runs the stub.
    wrapper = os.path.join(d, "boot_wrapper")
    open(wrapper, "w").write('#!/bin/sh\nexec "%s" "%s" "$@"\n' % (sys.executable, stub))
    os.chmod(wrapper, os.stat(wrapper).st_mode | stat.S_IEXEC | stat.S_IXGRP | stat.S_IXOTH)
    store = os.path.join(d, "store")
    srv = subprocess.Popen(
        [os.path.join(ROOT, "target/debug/ignition-fc-api"),
         "--api-sock", api, "--store", store,
         "--boot", wrapper, "--kernel", "/k/Image"],
        stderr=subprocess.PIPE)
    ok = False
    try:
        for _ in range(100):
            if os.path.exists(api): break
            time.sleep(0.05)
        assert req(api, "PUT", "/machine-config", {"vcpu_count":1,"mem_size_mib":512})[0] == 204
        assert req(api, "PUT", "/boot-source", {"kernel_image_path":"/k/Image","boot_args":"ro"})[0] == 204
        assert req(api, "PUT", "/drives/rootfs", {"drive_id":"rootfs","path_on_host":"/r.ext4","is_root_device":True})[0] == 204
        assert req(api, "PUT", "/actions", {"action_type":"InstanceStart"})[0] == 204
        assert req(api, "PATCH", "/vm", {"state":"Paused"})[0] == 204
        assert req(api, "PUT", "/snapshot/create", {"snapshot_path":"/s/snap1"})[0] == 204
        assert req(api, "PATCH", "/vm", {"state":"Resumed"})[0] == 204
        actions = open(os.path.join(store, "control.sock.actions")).read()
        # Pause/resume are advisory (REST-state only) and never reach the control
        # socket; only the snapshot/create capture sends a control line.
        assert '"snapshot"' in actions, actions
        assert '"name":"snap1"' in actions, actions
        print("fc_api_mock_test PASS")
        ok = True
    finally:
        srv.terminate()
        if not ok:
            try:
                err = srv.stderr.read().decode(errors="replace")
                if err: sys.stderr.write("--- server stderr ---\n" + err + "\n")
            except Exception:
                pass
    if not ok:
        sys.exit(1)

if __name__ == "__main__":
    main()
