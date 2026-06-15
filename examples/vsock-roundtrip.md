# Use case: vsock round-trip, both directions

**Scenario.** You want a control channel between the host and a guest without giving
the guest a network: a host tool driving an in-guest agent, or a guest process
streaming results back out. virtio-vsock gives you stream sockets in both directions
over the virtio transport — no IP, no `sudo`, no vmnet.

vsock addresses are `(CID, port)`. ignition uses the standard CIDs:

| CID | Meaning |
|-----|---------|
| `2` | the host (`VMADDR_CID_HOST`) |
| `3` | this guest (`config.guest_cid`) |

Both directions are served by one `--vsock-uds <path>` flag. The base path is the
host→guest **control socket**; per-port paths `{path}_{port}` are the guest→host
**listener sockets**.

```
            host                                   guest
   ┌───────────────────────┐            ┌───────────────────────┐
   │ {uds}_5000  (listen)   │◀── E1 ─────│ connect VSOCK 2:5000   │  guest→host
   │ {uds}       (control)  │─── E2 ────▶│ listen  VSOCK *:6000   │  host→guest
   └───────────────────────┘            └───────────────────────┘
```

`socat` is used on both ends below; any vsock-aware program works the same way.

## Boot a guest with vsock wired

```sh
cargo build -p ignition-spike --bin boot && scripts/sign.sh target/debug/boot

target/debug/boot --vsock-uds /tmp/ign-vsock \
    kimage/out/Image kimage/out/rootfs.ext4
```

The guest kernel needs `CONFIG_VIRTIO_VSOCKETS` (enabled in the shipped kernel
config); confirm the driver bound with `dmesg | grep -i vsock` inside the guest.

---

## E1 — guest → host

A guest process connects out to a host port; the host sees it as a connection on the
matching Unix socket. **The host listener must exist before the guest connects** — on
the guest's connect, ignition dials `{uds}_{port}`, so nothing is listening means the
guest gets a reset.

**1. Host: listen on the per-port socket (port 5000 → `{uds}_5000`).**

```sh
# echo server on the host side
socat UNIX-LISTEN:/tmp/ign-vsock_5000,fork EXEC:cat
```

**2. Guest: connect to the host (CID 2) on that port and send a line.**

```sh
# inside the guest
echo 'hello from guest' | socat - VSOCK-CONNECT:2:5000
# -> prints: hello from guest   (echoed back by the host server)
```

The bytes flow guest → host → (echo) → guest. Credit flow-control keeps either side
from overrunning the other.

---

## E2 — host → guest

A host process opens a connection *into* a listening guest, using Firecracker's hybrid
control protocol over the base `{uds}` socket. **The guest listener must be up first.**

**1. Guest: listen on a vsock port (6000).**

```sh
# inside the guest — echo server on the guest side
socat VSOCK-LISTEN:6000,fork EXEC:cat
```

**2. Host: open the control socket, ask for the guest port, then stream bytes.**

The protocol is a single text line, then raw bytes on the same connection:

```
host → ignition:  CONNECT 6000\n
ignition → host:  OK <host_port>\n      # connection established; bytes follow
```

With `socat`, send the `CONNECT` line and the payload together:

```sh
# on the host
printf 'CONNECT 6000\nhello from host\n' | socat - UNIX-CONNECT:/tmp/ign-vsock
# -> OK 1024
#    hello from host                    (echoed back by the guest server)
```

ignition allocates an ephemeral host port (from 1024 up), signals the guest
(`REQUEST`), the guest's listener accepts (`RESPONSE`), and ignition replies
`OK <host_port>`. If no guest process is listening on 6000, the connection is closed
with no `OK`.

An automated version of this round trip is `scripts/vsock_e2_test.py` (boots a guest,
connects, asserts the `OK` and the echo).

---

## Both at once

E1 and E2 coexist on the same `--vsock-uds` base. A guest can run an outbound client
(E1) and an inbound listener (E2) simultaneously; the per-port `{uds}_{port}` sockets
(guest→host) and the base `{uds}` control socket (host→guest) never collide.

## Notes

- **No snapshot of live connections.** A snapshot taken with open vsock connections
  restores with the device present but the connections reset — reconnect after restore.
  See [Snapshot & restore](../docs/src/features/snapshot-restore.md).
- **Datagrams are out of scope** — `SOCK_STREAM` only.
- Design detail: [vsock E1](../docs/superpowers/specs/2026-06-13-virtio-vsock-e1-design.md),
  [vsock E2](../docs/superpowers/specs/2026-06-15-virtio-vsock-e2-design.md).
