# scripts/test_fanout_demo.py
import os, socket, struct, threading, tempfile, unittest
import fanout_demo as fd


def _fake_guest(uds_path, ready, request_seen):
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(uds_path)
    srv.listen(1)
    ready.set()
    conn, _ = srv.accept()
    # read "CONNECT 7000\n"
    line = b""
    while not line.endswith(b"\n"):
        line += conn.recv(1)
    assert line == b"CONNECT 7000\n", line
    conn.sendall(b"OK 1024\n")
    hdr = conn.recv(4)
    n = struct.unpack("<I", hdr)[0]
    body = b""
    while len(body) < n:
        body += conn.recv(n - len(body))
    request_seen.append(body)
    resp = b'{"exit":0,"stdout":"hi\\n","stderr":"","timed_out":false}'
    conn.sendall(struct.pack("<I", len(resp)) + resp)
    conn.close()
    srv.close()


class TestVsockExec(unittest.TestCase):
    def test_exec_roundtrip(self):
        d = tempfile.mkdtemp()
        uds = os.path.join(d, "s.sock")
        ready, seen = threading.Event(), []
        t = threading.Thread(target=_fake_guest, args=(uds, ready, seen))
        t.start()
        ready.wait(5)
        resp = fd.vsock_exec(uds, "echo hi", timeout=5.0, deadline=5.0)
        t.join(5)
        self.assertEqual(resp["exit"], 0)
        self.assertEqual(resp["stdout"], "hi\n")
        self.assertFalse(resp["timed_out"])
        import json
        self.assertEqual(json.loads(seen[0])["cmd"], "echo hi")


if __name__ == "__main__":
    unittest.main()
