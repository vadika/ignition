import json, struct, subprocess, sys, os
HERE = os.path.dirname(os.path.abspath(__file__))
AGENT = os.path.join(HERE, "ign-exec.py")

def roundtrip(req: dict) -> dict:
    body = json.dumps(req).encode()
    frame = struct.pack("<I", len(body)) + body
    p = subprocess.run([sys.executable, AGENT], input=frame, capture_output=True)
    out = p.stdout
    n = struct.unpack("<I", out[:4])[0]
    return json.loads(out[4:4 + n])

def test_echo():
    r = roundtrip({"cmd": "echo hello"})
    assert r["exit"] == 0
    assert r["stdout"].strip() == "hello"
    assert r["timed_out"] is False

def test_exit_code_and_stderr():
    r = roundtrip({"cmd": "echo oops >&2; exit 3"})
    assert r["exit"] == 3
    assert r["stderr"].strip() == "oops"

def test_stdin_and_cwd():
    r = roundtrip({"cmd": "cat; pwd", "stdin": "piped\n", "cwd": "/tmp"})
    assert "piped" in r["stdout"]
    assert "/tmp" in r["stdout"]

def test_timeout():
    r = roundtrip({"cmd": "sleep 5", "timeout": 0.3})
    assert r["timed_out"] is True
    assert r["exit"] == 124

def test_bad_request():
    body = b"not json"
    frame = struct.pack("<I", len(body)) + body
    p = subprocess.run([sys.executable, AGENT], input=frame, capture_output=True)
    n = struct.unpack("<I", p.stdout[:4])[0]
    r = json.loads(p.stdout[4:4 + n])
    assert r["exit"] != 0
    assert "bad request" in r["stderr"]

def test_missing_cmd_key():
    r = roundtrip({"stdin": "x"})  # no "cmd"
    assert r["exit"] != 0
    assert "bad request" in r["stderr"]

def test_empty_string_stdin_is_empty_input():
    # cmd reads stdin; empty-string stdin must mean empty input (immediate EOF),
    # not "inherit the parent's stdin".
    r = roundtrip({"cmd": "cat; echo END", "stdin": ""})
    assert r["exit"] == 0
    assert r["stdout"].strip() == "END"

def test_truncated_frame_no_output():
    import struct, subprocess, sys
    # header claims 100 bytes, send only 4 -> agent must produce no response
    frame = struct.pack("<I", 100) + b"shrt"
    p = subprocess.run([sys.executable, AGENT], input=frame, capture_output=True)
    assert p.stdout == b""
