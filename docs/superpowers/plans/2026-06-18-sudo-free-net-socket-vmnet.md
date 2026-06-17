# Sudo-free networking via socket_vmnet — Implementation Plan (phase 1)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `boot --net` work without sudo by talking to the existing socket_vmnet daemon: a client-only `SocketVmnetBackend` (speaking socket_vmnet's 4-byte-big-endian framing) replaces the in-process vmnet backend by default; `--net-direct` keeps the old sudo path.

**Architecture:** A new `SocketVmnetBackend` implements the existing `NetBackend` trait — pure unix-socket client, no `vmnet.framework` link. We generate the guest MAC (random locally-administered unicast) per boot/restore. Backend selection at boot is type-erased via `Box<dyn NetBackend>`. `VirtioNet`, the RX feeder, and snapshot machinery are unchanged.

**Tech Stack:** Rust (std unix sockets, `std::sync::mpsc`), socket_vmnet (lima-vm, Homebrew), virtio-net.

**Spec:** `docs/superpowers/specs/2026-06-18-sudo-free-net-socket-vmnet-design.md`

**Reminders:** plain commit messages (no co-author/generated trailer); re-sign `target/debug/boot` with `./scripts/sign.sh target/debug/boot` after any cargo build before a live run. Rust builds + unit tests run locally with no sudo/daemon. The live test (Task 6) needs socket_vmnet installed + HVF.

**Key facts (verified against socket_vmnet/main.c + README):**
- Socket: `${HOMEBREW_PREFIX}/var/run/socket_vmnet` → default `/opt/homebrew/var/run/socket_vmnet` (Apple Silicon).
- Framing: `SOCK_STREAM`; each frame = 4-byte **big-endian** length (`htonl`/`ntohl`) + raw ethernet frame, both directions. No handshake.
- Client picks its own MAC.
- Install: `brew install socket_vmnet` + `sudo brew services start socket_vmnet` (plist `homebrew.mxcl.socket_vmnet`).

---

## File Structure

- `crates/vmnet/src/socket_vmnet.rs` — **new.** `SocketVmnetBackend` (impl `NetBackend`) + `generate_mac()` + tests.
- `crates/vmnet/src/lib.rs` — **modify.** Export the new items; fix the "Needs sudo" module doc.
- `crates/vmnet/Cargo.toml` — **modify.** Add `log` dep + `tempfile` dev-dep.
- `crates/devices/src/virtio/net.rs` — **modify.** `impl NetBackend for Box<dyn NetBackend>` + test.
- `spike/src/bin/boot.rs` — **modify.** `--net-direct`/`--net-socket` flags, `DeviceContext` fields, backend selection, `default_socket_path()`.
- `scripts/install-socket-vmnet.sh` — **new.**
- `docs/src/features/devices.md`, `ROADMAP.md` — **modify.** Docs.

---

### Task 1: `SocketVmnetBackend` + MAC generator

**Files:**
- Create: `crates/vmnet/src/socket_vmnet.rs`
- Modify: `crates/vmnet/src/lib.rs`, `crates/vmnet/Cargo.toml`

- [ ] **Step 1: Add deps**

In `crates/vmnet/Cargo.toml`, under `[dependencies]` add `log = "0.4"`, and add a dev-deps section:
```toml
[dependencies]
ignition_devices = { package = "ignition-devices", path = "../devices" }
log = "0.4"

[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 2: Write the failing tests**

Create `crates/vmnet/src/socket_vmnet.rs` with the test module first (implementation added next step, above it):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn mac_is_unicast_laa_and_random() {
        let a = generate_mac();
        let b = generate_mac();
        // bit0 (0x01) = 0 -> unicast; bit1 (0x02) = 1 -> locally administered.
        assert_eq!(a[0] & 0x03, 0x02);
        assert_ne!(a, b);
    }

    #[test]
    fn framing_roundtrip_against_fake_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sv.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            // read one client-sent frame: 4-byte BE length + payload
            let mut lenb = [0u8; 4];
            s.read_exact(&mut lenb).unwrap();
            let n = u32::from_be_bytes(lenb) as usize;
            let mut buf = vec![0u8; n];
            s.read_exact(&mut buf).unwrap();
            assert_eq!(buf, b"hello");
            // send one frame to the client
            let f = b"world!";
            s.write_all(&(f.len() as u32).to_be_bytes()).unwrap();
            s.write_all(f).unwrap();
            thread::sleep(Duration::from_millis(100)); // keep open so client reads before EOF
        });

        let (backend, rx) = SocketVmnetBackend::start(&path).unwrap();
        backend.write_frame(b"hello").unwrap();
        let got = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(got, b"world!");
        server.join().unwrap();
    }
}
```

- [ ] **Step 3: Run, verify fail**

Run: `cargo test -p ignition-vmnet socket`
Expected: FAIL to compile (`SocketVmnetBackend`/`generate_mac` undefined).

- [ ] **Step 4: Implement the module (above the tests)**

```rust
//! socket_vmnet client backend: talks to the socket_vmnet daemon over a unix
//! stream, so guest networking needs no sudo (the daemon holds the privilege).
//! Frame protocol (socket_vmnet): 4-byte big-endian length prefix + ethernet
//! frame, both directions. We generate the guest MAC; socket_vmnet learns it.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Mutex;

use ignition_devices::virtio::net::NetBackend;

/// Reject an implausible frame-length header (matches virtio-net MAX_FRAME).
const MAX_FRAME: usize = 65_536;

/// A random locally-administered unicast MAC (`02:..`). Fresh per call, so every
/// boot and restore gets a distinct MAC -> distinct DHCP lease.
pub fn generate_mac() -> [u8; 6] {
    let mut b = [0u8; 6];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut b);
    }
    b[0] = (b[0] & 0xFE) | 0x02; // clear multicast bit, set locally-administered
    b
}

pub struct SocketVmnetBackend {
    write: Mutex<UnixStream>,
    mac: [u8; 6],
}

impl SocketVmnetBackend {
    /// Connect to the socket_vmnet daemon. Returns the backend + the RX frame
    /// receiver (wired to the existing RX feeder exactly like VmnetBackend).
    pub fn start(socket_path: &Path) -> std::io::Result<(SocketVmnetBackend, Receiver<Vec<u8>>)> {
        let stream = UnixStream::connect(socket_path).map_err(|e| {
            std::io::Error::other(format!(
                "--net needs socket_vmnet at {} ({e}). Run scripts/install-socket-vmnet.sh, \
                 or pass --net-direct for the in-process sudo path.",
                socket_path.display()
            ))
        })?;
        let reader = stream.try_clone()?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || reader_loop(reader, tx));
        Ok((SocketVmnetBackend { write: Mutex::new(stream), mac: generate_mac() }, rx))
    }
}

fn reader_loop(mut s: UnixStream, tx: Sender<Vec<u8>>) {
    loop {
        let mut lenb = [0u8; 4];
        if s.read_exact(&mut lenb).is_err() {
            break; // EOF or error: daemon gone / link down
        }
        let n = u32::from_be_bytes(lenb) as usize;
        if n == 0 || n > MAX_FRAME {
            log::warn!("socket_vmnet: bad frame length {n}; closing RX");
            break;
        }
        let mut buf = vec![0u8; n];
        if s.read_exact(&mut buf).is_err() {
            break;
        }
        if tx.send(buf).is_err() {
            break; // receiver dropped
        }
    }
}

impl NetBackend for SocketVmnetBackend {
    fn write_frame(&self, frame: &[u8]) -> std::io::Result<()> {
        let mut s = self.write.lock().unwrap();
        // Serialized by the Mutex, so the two writes are not interleaved with
        // another writer's frame.
        s.write_all(&(frame.len() as u32).to_be_bytes())?;
        s.write_all(frame)?;
        Ok(())
    }
    fn mac(&self) -> [u8; 6] {
        self.mac
    }
}
```

- [ ] **Step 5: Export from `lib.rs`**

In `crates/vmnet/src/lib.rs`: change the top doc line `//! vmnet.framework shared/NAT backend (via the C shim). Needs sudo.` to:
```rust
//! Guest networking backends. `VmnetBackend` calls vmnet.framework in-process
//! (needs sudo); `SocketVmnetBackend` talks to the socket_vmnet daemon (no sudo).
```
Add after the existing `use` lines (top of file):
```rust
mod socket_vmnet;
pub use socket_vmnet::{generate_mac, SocketVmnetBackend};
```

- [ ] **Step 6: Run tests, verify pass**

Run: `cargo test -p ignition-vmnet socket`
Expected: 2 passed. Also `cargo build -p ignition-vmnet` clean.

- [ ] **Step 7: Commit**

```bash
git add crates/vmnet/src/socket_vmnet.rs crates/vmnet/src/lib.rs crates/vmnet/Cargo.toml
git commit -m "net: socket_vmnet client backend (no-sudo) + MAC generator"
```

---

### Task 2: `Box<dyn NetBackend>` impl for runtime backend selection

**Files:**
- Modify: `crates/devices/src/virtio/net.rs`

- [ ] **Step 1: Write the failing test**

In `crates/devices/src/virtio/net.rs`, inside the existing `#[cfg(test)] mod tests` block, add:

```rust
#[test]
fn box_dyn_netbackend_delegates() {
    struct Dummy;
    impl NetBackend for Dummy {
        fn write_frame(&self, _frame: &[u8]) -> std::io::Result<()> { Ok(()) }
        fn mac(&self) -> [u8; 6] { [0x02, 1, 2, 3, 4, 5] }
    }
    let b: Box<dyn NetBackend> = Box::new(Dummy);
    assert_eq!(b.mac(), [0x02, 1, 2, 3, 4, 5]);
    assert!(b.write_frame(b"x").is_ok());
    // Box<dyn NetBackend> itself satisfies NetBackend (used by VirtioNet<B>).
    fn takes_backend<B: NetBackend>(_: B) {}
    takes_backend(b);
}
```

- [ ] **Step 2: Run, verify fail**

Run: `cargo test -p ignition-devices box_dyn`
Expected: FAIL — `Box<dyn NetBackend>: NetBackend` not satisfied (the `takes_backend(b)` line).

- [ ] **Step 3: Implement**

In `crates/devices/src/virtio/net.rs`, immediately after the `NetBackend` trait definition (after the `pub trait NetBackend: Send { ... }` block), add:

```rust
/// Lets the VMM pick a backend at runtime (`Box<dyn NetBackend>`) while keeping
/// `VirtioNet<B: NetBackend>` generic.
impl NetBackend for Box<dyn NetBackend> {
    fn write_frame(&self, frame: &[u8]) -> std::io::Result<()> {
        (**self).write_frame(frame)
    }
    fn mac(&self) -> [u8; 6] {
        (**self).mac()
    }
}
```

- [ ] **Step 4: Run, verify pass**

Run: `cargo test -p ignition-devices box_dyn`
Expected: 1 passed. Also `cargo test -p ignition-devices` (the existing net tests still pass).

- [ ] **Step 5: Commit**

```bash
git add crates/devices/src/virtio/net.rs
git commit -m "net: impl NetBackend for Box<dyn NetBackend> (runtime backend choice)"
```

---

### Task 3: Boot wiring — default to socket_vmnet, `--net-direct` fallback

**Files:**
- Modify: `spike/src/bin/boot.rs`

- [ ] **Step 1: Add flag defaults + parse arms**

Near the other flag defaults (find `let mut no_reseed = false;` / the `let mut vsock_uds` area, ~boot.rs:793-797), add:
```rust
    let mut net_direct = false;
    let mut net_socket: Option<PathBuf> = None;
```
In the argument match (near the `"--no-reseed"`/`"--vsock-uds"` arms, ~boot.rs:877-889), add:
```rust
            "--net-direct" => {
                net_direct = true;
            }
            "--net-socket" => {
                net_socket = Some(PathBuf::from(it.next().expect("--net-socket needs a path")));
            }
```
Update the main usage string (~boot.rs:957) to include `[--net-direct] [--net-socket <path>]` next to `[--net]`.

- [ ] **Step 2: Add a default-socket helper**

Add this free function near the other small boot helpers (e.g. just above `fn setup_devices` or near `fn run_restore`):
```rust
/// The socket_vmnet daemon socket: `--net-socket`, else $IGN_VMNET_SOCKET, else the
/// Homebrew default. Resolved at the net-backend creation site.
fn default_socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("IGN_VMNET_SOCKET") {
        return PathBuf::from(p);
    }
    PathBuf::from("/opt/homebrew/var/run/socket_vmnet")
}
```

- [ ] **Step 3: Thread the flags into `DeviceContext`**

Find the context struct that holds the existing `net: bool` field (grep `net:` near the `vsock_uds:` field; it is the `DeviceContext`/`ctx` struct). Add two fields next to `net`:
```rust
    net_direct: bool,
    net_socket: Option<PathBuf>,
```
Set them at BOTH context-construction sites that already set `net`/`vsock_uds` — the boot path (~boot.rs:1035, where `vsock_uds: vsock_uds.clone()` is set) and the restore path (~boot.rs:1935). At each, add:
```rust
        net_direct,
        net_socket: net_socket.clone(),
```
(`net_socket.clone()` because it is used at two sites.)

- [ ] **Step 4: Swap the backend selection in `setup_devices`**

Replace the `if want_net { ... }` body's first lines (boot.rs:634-638, from `let (backend, rx) = ignition_vmnet::VmnetBackend::start()...` through `let net_dev = VirtioNet::new(backend);`) with:

```rust
    if want_net {
        let mem = ctx.guest_ram();
        let (backend, rx): (Box<dyn ignition_devices::virtio::net::NetBackend>, std::sync::mpsc::Receiver<Vec<u8>>) =
            if ctx.net_direct {
                let (b, rx) = ignition_vmnet::VmnetBackend::start().map_err(|e| {
                    io::Error::other(format!("vmnet direct start failed (need sudo for --net-direct): {e}"))
                })?;
                (Box::new(b), rx)
            } else {
                let path = ctx.net_socket.clone().unwrap_or_else(default_socket_path);
                // start() already returns an io::Error carrying the install hint.
                let (b, rx) = ignition_vmnet::SocketVmnetBackend::start(&path)?;
                (Box::new(b), rx)
            };
        let net_dev = VirtioNet::new(backend);
        if let Some(h) = place(mgr, &mode, "virtio-net", layout::MMIO_WINDOW,
            move |irq| VirtioMmio::new("virtio-net", Box::new(net_dev), mem, irq))? {
            // ... existing RX feeder + ctx.rx_stop + ctx.net_mmio block UNCHANGED ...
```
Leave the RX feeder thread, `ctx.rx_stop`, and `ctx.net_mmio` lines (boot.rs:641-657) exactly as they are. Only the backend creation + `net_dev` binding above them changes. `SocketVmnetBackend::start` already returns an `io::Error` carrying the install hint, so the socket branch uses `?` directly; only the direct-vmnet branch wraps its error to add the sudo context.

- [ ] **Step 5: Build + tests + sign**

Run: `cargo build -p ignition-spike --bin boot`
Expected: builds clean. Fix the exact `DeviceContext` field names / construction sites if the grep located them slightly differently than the line hints.
Run: `cargo test -p ignition-spike --bin boot`
Expected: existing tests pass (22).
Run: `./scripts/sign.sh target/debug/boot`
Expected: signed.

- [ ] **Step 6: Commit**

```bash
git add spike/src/bin/boot.rs
git commit -m "net: default --net to socket_vmnet (no sudo); --net-direct keeps in-process vmnet"
```

---

### Task 4: Install helper script

**Files:**
- Create: `scripts/install-socket-vmnet.sh`

- [ ] **Step 1: Write the script**

```bash
#!/usr/bin/env bash
# One-time setup for sudo-free guest networking: install the socket_vmnet daemon
# (lima-vm/socket_vmnet) via Homebrew and start its root LaunchDaemon. After this,
# `boot --net` connects to the daemon socket and needs no sudo.
set -euo pipefail

if ! command -v brew >/dev/null 2>&1; then
  echo "Homebrew not found. Install it from https://brew.sh first." >&2
  exit 1
fi

brew install socket_vmnet
# The daemon must run as root (vmnet shared mode); brew services + sudo installs
# the LaunchDaemon homebrew.mxcl.socket_vmnet.
sudo "$(brew --prefix)/bin/brew" services start socket_vmnet

sock="$(brew --prefix)/var/run/socket_vmnet"
echo "socket_vmnet socket path: $sock"
if [ -S "$sock" ]; then
  echo "ready: boot --net will use it (override with --net-socket or IGN_VMNET_SOCKET)."
else
  echo "socket not present yet; give it a moment, then check: sudo brew services list" >&2
fi
```

- [ ] **Step 2: Make it executable + syntax check**

Run: `chmod +x scripts/install-socket-vmnet.sh && bash -n scripts/install-socket-vmnet.sh`
Expected: no output (valid).

- [ ] **Step 3: Commit**

```bash
git add scripts/install-socket-vmnet.sh
git commit -m "net: install-socket-vmnet.sh — one-time socket_vmnet daemon setup"
```

---

### Task 5: Docs + ROADMAP

**Files:**
- Modify: `docs/src/features/devices.md`, `ROADMAP.md`

- [ ] **Step 1: Update the networking section of `docs/src/features/devices.md`**

Find the virtio-net / networking discussion (it documents `--net` needing sudo). Add/adjust a paragraph: `--net` now defaults to the **socket_vmnet** daemon and needs **no sudo** once installed via `scripts/install-socket-vmnet.sh` (the daemon holds the privileged vmnet interface; the VMM is an unprivileged unix-socket client speaking socket_vmnet's 4-byte-big-endian frame protocol). The guest MAC is generated by the VMM (random locally-administered unicast) per boot/restore, so clones still get distinct MAC + DHCP lease. `--net-direct` keeps the original in-process vmnet path (still needs sudo) for debugging or when the daemon is absent. Socket path defaults to `/opt/homebrew/var/run/socket_vmnet`, overridable with `--net-socket <path>` or `$IGN_VMNET_SOCKET`. Match the file's existing prose style; no marketing language.

- [ ] **Step 2: Update `ROADMAP.md`**

In the "Deferred / out of scope" section, the item `[-] **Userspace net backend (gvproxy/passt)** ...` notes vmnet stays sudo-bound. Add a new shipped/near-term line (or amend) recording that sudo-free networking now exists via socket_vmnet:
```
- [x] **Sudo-free networking via socket_vmnet** — `--net` defaults to the
  socket_vmnet daemon (Homebrew, root LaunchDaemon); the VMM is an unprivileged
  client (4-byte-BE frame protocol, VMM-generated MAC). `--net-direct` keeps the
  in-process sudo path. `scripts/install-socket-vmnet.sh`,
  `docs/superpowers/specs/2026-06-18-sudo-free-net-socket-vmnet-design.md`. Phase 2
  (in-process shim hardening) still planned.
```
Place it under the Snapshot/restore or a networking area as fits; bump `_Last updated:_` to 2026-06-18 if present.

- [ ] **Step 3: Verify links + commit**

If `mdbook` is available: `cd docs && mdbook build` (linkcheck). Otherwise confirm any relative links resolve. Commit:
```bash
git add docs/src/features/devices.md ROADMAP.md
git commit -m "docs: sudo-free networking via socket_vmnet (devices + roadmap)"
```

---

### Task 6: Live verification (HUMAN / live HVF)

Needs socket_vmnet installed + a signed `boot` + kernel/rootfs. Run by the human (or the controller if it has the Mac + HVF). Stop and hand back after Task 5.

- [ ] **Step 1: Install the daemon**

Run: `bash scripts/install-socket-vmnet.sh`
Expected: prints `ready` and the socket path exists.

- [ ] **Step 2: Boot with networking, no sudo**

Run (NO sudo): `./target/debug/boot --net --mem 512 kimage/out/Image kimage/out/rootfs.ext4`
Expected: the guest boots; on the serial console, `ip addr`/`ifconfig eth0` shows a DHCP-assigned address (from the vmnet subnet, e.g. 192.168.105.x); `ping -c1 1.1.1.1` succeeds. No sudo prompt.

- [ ] **Step 3: Fan-out distinct IPs**

Snapshot a base, then restore 2 clones (each a fresh `SocketVmnetBackend` → fresh MAC); confirm each clone gets a **distinct** IP and both reach the internet. (Reuse the existing snapshot/restore flow; the carrier-watch rebind handles the new MAC.)

- [ ] **Step 4: `--net-direct` still works (optional)**

Run: `sudo ./target/debug/boot --net --net-direct --mem 512 kimage/out/Image kimage/out/rootfs.ext4`
Expected: the in-process vmnet path still brings up networking under sudo (regression check).

---

## Notes for the executor

- **No sandbox change.** The unix-socket `connect` happens in `setup_devices`, before the Seatbelt profile is applied (apply is right before the vCPU run loop), so the fd is open pre-sandbox and the reader/writer use the already-open fd. No `SandboxPaths` edit needed.
- **Snapshot/restore is unchanged** — the restore path constructs the backend the same way (Task 3 covers both ctx sites), yielding a fresh MAC + new socket connection per restore; the guest's carrier-watch re-DHCP already handles it.
- **Rust unit tests (Tasks 1–2) need no daemon/sudo** (fake unix socket). Only Task 6 needs socket_vmnet + HVF.
- If `cargo build` flags `MAX_FRAME` as duplicated, note the one in `socket_vmnet.rs` is intentionally local (the `net.rs` one is private to that module).
