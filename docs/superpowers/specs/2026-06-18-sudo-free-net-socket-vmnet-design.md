# Sudo-free networking via socket_vmnet — design (phase 1)

_Design spec. Status: approved, awaiting implementation plan. Phase 1 of the
"proper vmnet" work; phase 2 (in-process shim hardening) is a separate later spec._

## Goal

Make `boot --net` work **without sudo**, with no loss of guest-visible behavior
(shared/NAT, per-clone MAC + IP, snapshot link-bounce). Achieve it by talking to
the existing **socket_vmnet** daemon (lima-vm/socket_vmnet) — a Homebrew-installable
root LaunchDaemon that owns the privileged `vmnet` interface — instead of calling
`vmnet_start_interface` in-process (which requires root). We write client-side code
only; no privileged component is built or shipped.

This unblocks net-by-default for the MCP agent sandboxes, the disposable browser,
and fan-out, none of which want a sudo prompt per clone.

## Decisions (settled in brainstorming)

- **Reuse socket_vmnet** (existing daemon), not a bespoke helper. No daemon code,
  no privileged binary to sign or secure — that is socket_vmnet's job.
- **Client-only change**: a new `SocketVmnetBackend` implementing the existing
  `NetBackend` trait. `VirtioNet`, the RX feeder thread, and the snapshot machinery
  are untouched — only the backend behind the trait changes.
- **We generate the guest MAC.** socket_vmnet runs one shared `vmnet` interface and
  software-bridges all clients; the client picks its own MAC. We generate a random
  locally-administered unicast MAC per boot and per restore, preserving "fresh MAC
  per clone/restore → fresh DHCP lease."
- **Wire protocol** = socket_vmnet's QEMU-compatible framing (verified against the
  source): each ethernet frame is prefixed by a 4-byte **big-endian** length over a
  `SOCK_STREAM` unix socket. No handshake.
- **Keep the in-process path** behind `--net-direct` (still sudo) for debugging and
  as a fallback if the daemon is absent.

## Architecture

```
socket_vmnet  (brew, root LaunchDaemon homebrew.mxcl.socket_vmnet)
  one shared vmnet SHARED interface + software switch (learns client MACs)
  listens on  ${HOMEBREW_PREFIX}/var/run/socket_vmnet   (default /opt/homebrew/var/run/socket_vmnet)
        ▲  SOCK_STREAM, frames = [u32 BE length | ethernet frame]
        │
boot / clones  (unprivileged, NO sudo)
  --net  -> SocketVmnetBackend
    generate MAC (02:xx:xx:xx:xx:xx)            -> virtio-net config (mac())
    connect the socket
    reader thread: read [len|frame] -> mpsc -> existing RX feeder -> VirtioNet::inject_rx
    write_frame(frame): writev([htonl(len), frame]) -> socket
  --net-direct  -> existing in-process VmnetBackend (sudo; unchanged)
```

The RX feeder thread, `stop_rx` snapshot quiescing, link-bounce on restore, and
`VirtioNet` TX/RX are all reused verbatim — they sit above the `NetBackend` trait.

## Components

### a. `SocketVmnetBackend` (new) — `crates/vmnet/src/socket_vmnet.rs`

Implements the existing `NetBackend` trait (`devices::virtio::net::NetBackend`):
`write_frame(&[u8]) -> io::Result<()>` and `mac() -> [u8; 6]`. Pure unix-socket
client — does **not** link or call `vmnet.framework`.

- `SocketVmnetBackend::start(socket_path: &Path) -> io::Result<(Self, Receiver<Vec<u8>>)>`:
  1. Generate the guest MAC (see below); store it.
  2. `UnixStream::connect(socket_path)`; on failure return a clear error (see Errors).
  3. Spawn a **reader thread**: loop `read_exact(4)` → `u32::from_be_bytes` → bounds-check
     (`<= MAX_FRAME`, 65536) → `read_exact(len)` → send the frame into an mpsc `Sender`.
     On EOF/error, drop the sender (RX stops) and log once.
  4. Return `(backend, frame_receiver)`. The caller wires the receiver to the existing
     RX feeder exactly as it does for `VmnetBackend` today.
- `write_frame`: under a `Mutex<UnixStream>` (write half), `writev` (or two `write_all`s)
  of `(len as u32).to_be_bytes()` then the frame. Errors logged + returned; do not panic.
- `mac()`: returns the generated MAC.

### b. MAC generator

`fn generate_mac() -> [u8; 6]`: 6 random bytes, then force `buf[0] = (buf[0] & 0xFE) | 0x02`
(clear the multicast bit, set the locally-administered bit → `02`-style unicast LAA).
Randomness from `getentropy` (libc) with a `/dev/urandom` fallback. Called once per
`start()`, so every boot and every restore gets a fresh MAC.

### c. Boot wiring — `spike/src/bin/boot.rs`

- Add a `--net-direct` flag (bool). `--net` stays the user-facing "I want networking"
  flag; its **backend** is now socket_vmnet by default.
- In `setup_devices` where `VmnetBackend::start()` is called today (~boot.rs:635):
  - if `net_direct`: keep the current `VmnetBackend::start()` (in-process vmnet, sudo).
  - else (default): `SocketVmnetBackend::start(&socket_path)`.
  - Both return a `NetBackend` impl + a `Receiver<Vec<u8>>`; the rest (wrap in
    `VirtioNet`, `place()`, spawn the RX feeder, store `net_mmio`/`stop_rx`) is identical.
    Note: `VirtioNet<B: NetBackend>` is generic, so the two concrete backends need either
    `VirtioNet<Box<dyn NetBackend>>` (make `NetBackend` object-safe + `impl NetBackend for
    Box<dyn NetBackend>`) or a small backend enum, so the downstream wiring is written once
    rather than duplicated per branch. The plan picks one.
- Socket path: default `${HOMEBREW_PREFIX}/var/run/socket_vmnet` resolved as
  `/opt/homebrew/var/run/socket_vmnet`, overridable via `IGN_VMNET_SOCKET` env or a
  `--net-socket <path>` flag (Intel Homebrew uses `/usr/local/...`).
- The restore path (~boot.rs:628) constructs the backend the same way → a fresh MAC +
  a new socket connection per restore.

### d. Install helper — `scripts/install-socket-vmnet.sh`

Documents/automates the one-time privileged setup:
```sh
brew install socket_vmnet
sudo "$(brew --prefix)/bin/brew" services start socket_vmnet
```
Prints the resolved socket path and a check that it exists. No ignition-owned
privileged code; socket_vmnet ships its own signed daemon + plist
(`/Library/LaunchDaemons/homebrew.mxcl.socket_vmnet.plist`).

## Protocol (verified against socket_vmnet/main.c)

`SOCK_STREAM` unix socket. Each frame, both directions:
```
4 bytes: length, BIG-ENDIAN (htonl / ntohl)   |   <length> bytes: raw ethernet frame
```
socket_vmnet send path uses `writev([htonl(vm_pkt_size), frame])`; receive path reads
4 bytes, `ntohl`, then reads that many. There is **no** initial handshake — the client
connects and immediately reads/writes frames. The guest's frames carry our generated
src MAC; socket_vmnet's switch learns it and vmnet's DHCP leases an IP to that MAC.

## Snapshot / restore

Unchanged semantics. A restore builds a fresh `SocketVmnetBackend` → fresh MAC → new
socket connection; the guest's carrier-watch service sees the link-down→up bounce,
rebinds virtio_net, and re-DHCPs onto the new MAC. RX feeder + `stop_rx` quiescing and
the link-bounce pulse are identical to the current `--net` path. Distinct clones get
distinct MACs (independent random draws) → distinct IPs on the shared subnet.

## Error handling

- Socket connect fails (daemon not installed/running): return an error whose message
  names the fix: `"--net needs socket_vmnet: run scripts/install-socket-vmnet.sh (or
  pass --net-direct for the in-process sudo path)"`. Non-fatal to the binary only in
  that `--net` was explicitly requested → it is fatal for that run (as today's sudo
  failure is).
- Reader thread hits EOF/error: log once, drop the sender; the guest sees the link go
  quiet (RX stops). The VMM stays up.
- `write_frame` error: logged + returned; the TX path already tolerates per-frame
  errors (continues).
- Oversized length header (> `MAX_FRAME`): treat as a protocol error, close the
  connection, log — do not allocate attacker-sized buffers.

## Security

socket_vmnet owns the privilege boundary and its socket's group/permissions (its
documented install restricts access). ignition adds no privileged code. Note: any
local process that can open the socket gets a NAT'd interface — that is socket_vmnet's
posture, acceptable on a single-user dev Mac, and unchanged by us. The VMM keeps only
the hypervisor entitlement and runs as the normal user.

## Testing

- **Framing unit** (`socket_vmnet.rs` tests): stand up a fake unix-socket server in a
  thread that speaks the 4-byte-BE framing; assert `write_frame` emits
  `[htonl(len)|frame]` and that a server-sent `[len|frame]` arrives on the receiver
  intact. Mirrors the `vsock_client` test pattern. No daemon/sudo needed.
- **MAC generator unit**: `generate_mac()` always returns a unicast LAA address
  (`b[0] & 0x03 == 0x02`) and two draws differ.
- **Live (sudo-free, after `install-socket-vmnet.sh`)**: `boot --net` → guest gets a
  DHCP lease and reaches the internet; 2-clone fan-out → distinct IPs, both reach out.
  This is the existing manual net check, now without sudo. Human-run on HVF.

## Files

- Create: `crates/vmnet/src/socket_vmnet.rs` (`SocketVmnetBackend` + `generate_mac`).
- Modify: `crates/vmnet/src/lib.rs` (export `SocketVmnetBackend`, `generate_mac`).
- Modify: `spike/src/bin/boot.rs` (`--net-direct` + `--net-socket` flags, backend
  selection, shared downstream wiring, restore path).
- Create: `scripts/install-socket-vmnet.sh`.
- Modify: `docs/src/features/devices.md` (networking section: socket_vmnet, no sudo,
  `--net-direct` fallback) and `ROADMAP.md`.

## Out of scope (phase 2 / later)

- In-process shim hardening (teardown, error reporting, `max_packet` enforcement,
  non-blocking start, bridged/host modes, lock-free RX) — phase 2 spec; the
  `--net-direct` path inherits today's behavior until then.
- Bridged mode (socket_vmnet supports `--vmnet-mode bridged` at the daemon; exposing
  it is a follow-up, not phase 1).
- The newer `vmnet-helper` daemon as an alternative backend — deferred; socket_vmnet
  is the established choice.
- Multi-vCPU + `--net` snapshot (still single-vCPU as today).
