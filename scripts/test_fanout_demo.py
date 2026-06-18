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


def _fake_guest_close(uds_path, ready):
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(uds_path)
    srv.listen(1)
    ready.set()
    conn, _ = srv.accept()
    conn.close()  # accept then drop, no OK
    srv.close()


class TestVsockExec(unittest.TestCase):
    def test_exec_roundtrip(self):
        d = tempfile.mkdtemp()
        uds = os.path.join(d, "s.sock")
        ready, seen = threading.Event(), []
        t = threading.Thread(target=_fake_guest, args=(uds, ready, seen))
        t.start()
        assert ready.wait(5), "guest thread not ready"
        resp = fd.vsock_exec(uds, "echo hi", timeout=5.0, deadline=5.0)
        t.join(5)
        self.assertEqual(resp["exit"], 0)
        self.assertEqual(resp["stdout"], "hi\n")
        self.assertFalse(resp["timed_out"])
        import json
        self.assertEqual(json.loads(seen[0])["cmd"], "echo hi")

    def test_exec_closed_before_ok(self):
        d = tempfile.mkdtemp()
        uds = os.path.join(d, "s.sock")
        ready = threading.Event()
        t = threading.Thread(target=_fake_guest_close, args=(uds, ready))
        t.start()
        assert ready.wait(5), "guest thread not ready"
        with self.assertRaises((IOError, OSError, TimeoutError)):
            fd.vsock_exec(uds, "echo hi", timeout=2.0, deadline=2.0)
        t.join(5)


class TestParseVerdict(unittest.TestCase):
    def test_parse_workload(self):
        out = "BOOTID=3f9a\nRAND=a17c4e\nFILE=/tmp/fork-marker:a17c4e\n"
        rec = fd.parse_workload(out)
        self.assertEqual(rec, {"bootid": "3f9a", "rand": "a17c4e",
                               "file_path": "/tmp/fork-marker",
                               "file_readback": "a17c4e"})

    def test_verdict_pass(self):
        forks = [
            {"bootid": "x", "rand": "a", "file_readback": "a", "exit": 0, "error": None},
            {"bootid": "y", "rand": "b", "file_readback": "b", "exit": 0, "error": None},
        ]
        v = fd.verdict(forks)
        self.assertTrue(v["randoms_distinct"])
        self.assertTrue(v["cow_isolated"])
        self.assertTrue(v["identities_distinct"])
        self.assertTrue(v["ok"])

    def test_identities_distinct_is_informational(self):
        # Identical bootids no longer fail the verdict; identities_distinct is
        # just supplementary evidence and is False here.
        forks = [
            {"bootid": "x", "rand": "a", "file_readback": "a", "exit": 0, "error": None},
            {"bootid": "x", "rand": "b", "file_readback": "b", "exit": 0, "error": None},
        ]
        v = fd.verdict(forks)
        self.assertTrue(v["ok"])
        self.assertFalse(v["identities_distinct"])

    def test_verdict_fails_on_dup_random(self):
        forks = [
            {"bootid": "x", "rand": "a", "file_readback": "a", "exit": 0, "error": None},
            {"bootid": "x", "rand": "a", "file_readback": "a", "exit": 0, "error": None},
        ]
        v = fd.verdict(forks)
        self.assertFalse(v["randoms_distinct"])
        self.assertFalse(v["ok"])

    def test_verdict_fails_on_error(self):
        forks = [
            {"bootid": "x", "rand": "a", "file_readback": "a", "exit": 0, "error": None},
            {"bootid": None, "rand": None, "file_readback": None, "exit": None, "error": "timeout"},
        ]
        self.assertFalse(fd.verdict(forks)["ok"])


if __name__ == "__main__":
    unittest.main()
