# Serial RX milestone — DONE

Date: 2026-06-12. Status: **complete, verified end-to-end.** ignition/HVF now has a
fully bidirectional 16550 console: a real interactive root login works.

## Verified session

Driven through the boot harness (`target/debug/boot kimage/out/Image
kimage/out/rootfs.ext4`):

```
(none) login: root
Welcome to Alpine!
(none):~# ls /
bin  dev  etc  home  lib  ...  sbin  sys  tmp  usr  var
(none):~# id
uid=0(root) gid=0(root) groups=0(root),...
(none):~#
```

Typed `root` (passwordless) → root shell → `ls /` and `id` returned correct output
→ Ctrl-A x → `[console detached]`, process exit 0, terminal restored.

## What landed

- `crates/devices/src/serial.rs`: `Serial::enqueue(&[u8]) -> io::Result<usize>`
  delegates to `vm_superio::enqueue_raw_bytes`, which buffers into the RX FIFO,
  sets the LSR data-ready bit, and raises the RX interrupt through the same
  `SerialIrq::Gic` line TX already uses (GIC serial SPI, INTID 32). `FullFifo`
  maps to `WouldBlock`, trigger failures to `Other`.
- `spike/src/bin/boot.rs`:
  - **Reader thread** (`spawn_stdin_reader`): blocks on `libc::read` (outside the
    serial lock), runs the escape FSM, `enqueue`s forwarded bytes. Retries on
    EINTR (so `SIGWINCH`/resize doesn't kill input); exits on EOF/error.
  - **Escape FSM** (`step`/`EscState`/`Action`): Ctrl-A x quits; Ctrl-A + other
    forwards a literal Ctrl-A then the byte; Ctrl-A Ctrl-A forwards one literal.
    Pure, unit-tested (4 cases).
  - **`TermiosGuard`**: raw mode on a TTY (clear ICANON/ECHO/ISIG/IEXTEN/IXON/ICRNL,
    VMIN=1/VTIME=0, TCSAFLUSH entry); no-op for piped stdin. Restores via `Drop`
    (guest-initiated exit) and explicitly before `process::exit` (Ctrl-A x).
  - Serial rewired to a typed `Arc<Mutex<Serial<FlushWriter>>>` shared between the
    reader thread and the bus (coerced clone).

## Concurrency note (from final review)

The serial `Mutex` is the only lock either thread takes, held briefly per
read/write/enqueue. The RX IRQ pulse inside the lock only calls the stateless
`hv_gic_set_spi` FFI — no re-entry into the bus or serial — so TX (vCPU thread) and
RX (reader thread) cannot deadlock. The host terminal stays fully raw; the guest
tty does echo/line-editing.

## Tests

20 device unit tests (incl. `enqueue_sets_data_ready_and_reads_back`), 4 boot FSM
tests, workspace builds, 0 clippy warnings.

## Gotcha (unchanged)

`cargo build --workspace` strips the hypervisor entitlement from `boot`; re-run
`scripts/sign.sh target/debug/boot` before running or `hv_vm_create` fails
`VmCreate`.

## Remaining

The console is complete. Earlier carried-forward items (halfword-MMIO panic, `Vm`
owning memory regions, `Bus::register` overlap validation) remain open — see
`docs/phase1-followups.md`.
