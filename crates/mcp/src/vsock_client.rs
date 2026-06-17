//! Host-side vsock client: drive the E2 host->guest control handshake and run a
//! framed exec request against the in-guest ign-exec agent.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The guest vsock port the ign-exec socat listener binds.
pub const EXEC_PORT: u32 = 7000;

#[derive(Debug, Serialize)]
pub struct ExecRequest {
    pub cmd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<f64>,
}

#[derive(Debug, Deserialize, PartialEq)]
pub struct ExecResponse {
    pub exit: i32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

fn io_err(msg: impl Into<String>) -> std::io::Error {
    std::io::Error::other(msg.into())
}

/// Connect to a session control UDS, perform `CONNECT <EXEC_PORT>` / `OK`, send a
/// length-prefixed JSON request, and read the length-prefixed JSON response.
pub fn exec(uds: &Path, req: &ExecRequest, op_timeout: Duration) -> std::io::Result<ExecResponse> {
    let mut s = UnixStream::connect(uds)?;
    s.set_read_timeout(Some(op_timeout))?;
    s.set_write_timeout(Some(op_timeout))?;

    s.write_all(format!("CONNECT {EXEC_PORT}\n").as_bytes())?;

    // Read the "OK <host_port>\n" ack one byte at a time (it precedes the binary frame).
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    loop {
        let n = s.read(&mut b)?;
        if n == 0 {
            return Err(io_err("vsock: connection closed before OK"));
        }
        line.push(b[0]);
        if b[0] == b'\n' {
            break;
        }
        if line.len() > 128 {
            return Err(io_err("vsock: oversized ack line"));
        }
    }
    if !line.starts_with(b"OK ") {
        return Err(io_err(format!("vsock: expected OK, got {:?}", String::from_utf8_lossy(&line))));
    }

    let body = serde_json::to_vec(req)?;
    s.write_all(&(body.len() as u32).to_le_bytes())?;
    s.write_all(&body)?;

    let mut lenb = [0u8; 4];
    s.read_exact(&mut lenb)?;
    let n = u32::from_le_bytes(lenb) as usize;
    if n > 64 * 1024 * 1024 {
        return Err(io_err("vsock: response frame too large"));
    }
    let mut buf = vec![0u8; n];
    s.read_exact(&mut buf)?;
    let resp: ExecResponse = serde_json::from_slice(&buf)?;
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::thread;

    // A fake guest: accept the UDS connection, read the "CONNECT 7000\n" line,
    // reply "OK 1024\n", read the framed request, reply a framed response.
    #[test]
    fn exec_roundtrip_against_fake_guest() {
        let dir = tempfile::tempdir().unwrap();
        let uds = dir.path().join("s.sock");
        let listener = UnixListener::bind(&uds).unwrap();
        let h = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut line = Vec::new();
            let mut b = [0u8; 1];
            while s.read(&mut b).unwrap() == 1 {
                line.push(b[0]);
                if b[0] == b'\n' { break; }
            }
            assert_eq!(line, b"CONNECT 7000\n");
            s.write_all(b"OK 1024\n").unwrap();
            let mut lenb = [0u8; 4];
            s.read_exact(&mut lenb).unwrap();
            let n = u32::from_le_bytes(lenb) as usize;
            let mut buf = vec![0u8; n];
            s.read_exact(&mut buf).unwrap();
            let req: serde_json::Value = serde_json::from_slice(&buf).unwrap();
            assert_eq!(req["cmd"], "echo hi");
            let resp = br#"{"exit":0,"stdout":"hi\n","stderr":"","timed_out":false}"#;
            s.write_all(&(resp.len() as u32).to_le_bytes()).unwrap();
            s.write_all(resp).unwrap();
        });
        let req = ExecRequest { cmd: "echo hi".into(), stdin: None, cwd: None, timeout: Some(5.0) };
        let resp = exec(&uds, &req, std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(resp.exit, 0);
        assert_eq!(resp.stdout, "hi\n");
        assert!(!resp.timed_out);
        h.join().unwrap();
    }
}
