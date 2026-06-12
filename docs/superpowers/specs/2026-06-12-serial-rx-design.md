# Serial RX (interactive console) — design

Date: 2026-06-12. Milestone: serial receive path, completing the bidirectional
16550 console so a real interactive login works on ignition/HVF.

## Goal

Type at the Alpine `(none) login:` prompt, log in as `root` (passwordless), run a
shell command and see its output, then quit the harness cleanly with the host
terminal restored. TX already works (milestone 2f); this adds RX.

## Context

- TX path is done: `crates/devices/src/serial.rs` wraps `vm_superio::Serial` with a
  `SerialIrq` Trigger that edge-pulses the GIC serial SPI (INTID 32). The kernel's
  8250 tty drives a login prompt; output reaches host stdout via `FlushWriter`.
- The rootfs already supports login: `passwd -d root` (passwordless), agetty on
  `ttyS0`, `ttyS0` in `/etc/securetty`. No rootfs change needed.
- The vCPU runs on its own thread (`crates/vmm/src/vstate/hvf_vcpu.rs`); the bus
  dispatches MMIO. The boot harness (`spike/src/bin/boot.rs`) owns device wiring.

## Architecture & data flow

```
host stdin (raw) -> reader thread -> [Ctrl-A x state machine]
   -> Serial::enqueue -> vm_superio RX FIFO + RX IRQ (GIC SPI 32)
   -> guest 8250 reads RBR (MMIO exit -> bus -> serial.read offset 0)
   -> getty/login/shell; guest echoes via TX -> FlushWriter -> host stdout
```

The host terminal stays fully raw (no echo, no ISIG): the **guest** tty does echo
and line editing. Control keys (Ctrl-C, etc.) pass straight to the guest. The RX
interrupt is automatic — `vm_superio::enqueue_raw_bytes` raises it through the same
`SerialIrq::Gic` line TX already uses, so no new IRQ wiring is added.

### Approach chosen

A dedicated host **stdin reader thread** (recommended over polling stdin inside the
vCPU run loop, which adds input lag and couples host IO to the vcpu thread, and
over an event-driven selector, which is overkill for one fd).

## Components & files

### `crates/devices/src/serial.rs` — add one method

```rust
/// Feed host input into the RX FIFO; raises the RX interrupt (via the wired
/// Trigger) if the guest enabled it. Returns the number of bytes accepted.
pub fn enqueue(&mut self, bytes: &[u8]) -> io::Result<usize> {
    self.inner.enqueue_raw_bytes(bytes)
}
```

`enqueue_raw_bytes` buffers into the RX FIFO and pulses the Trigger when the guest
has the RX-data-available interrupt enabled (IER bit 0), which getty/login set on
opening `ttyS0`.

### `spike/src/bin/boot.rs` — harness changes

- Build the serial as `Arc<Mutex<Serial<FlushWriter>>>`. Clone one handle (coerced
  to `Arc<Mutex<dyn BusDevice>>`) into the bus; keep the typed handle for the
  reader thread. (`Arc<Mutex<Serial<W>>>` unsize-coerces to `Arc<Mutex<dyn
  BusDevice>>`.)
- **`TermiosGuard` (RAII):** on construct, if stdin is a TTY (`isatty(0)`), save the
  original termios and apply raw mode (clear `ICANON|ECHO|ISIG|IEXTEN` in lflag,
  `IXON` in iflag; `VMIN=1`, `VTIME=0`); restore the saved termios on `Drop`. If
  stdin is not a TTY, no-op (piped/CI runs unaffected).
- **Reader thread:** blocks on `libc::read(0, …)`, runs the escape state machine,
  `enqueue`s forwarded bytes into the typed serial handle. `read` returning 0 (EOF)
  or error ends the thread quietly; the vCPU keeps running.
- **Exit:** Ctrl-A x -> restore termios -> `process::exit(0)`. Guest-initiated exit
  (vcpu thread returns) -> `main` drops the `TermiosGuard` -> restore. Both paths
  restore the terminal; `process::exit` runs only after restore.

### Escape state machine — pure function, unit-tested

```rust
enum EscState { Normal, SawCtrlA }
enum Action<'a> { Forward(&'a [u8]), Quit, Pending }
fn step(state: &mut EscState, byte: u8) -> Action
```

- `Normal` + Ctrl-A (`0x01`) -> `SawCtrlA`, `Pending`
- `SawCtrlA` + `'x'` (`0x78`) -> `Quit`
- `SawCtrlA` + other -> `Forward([0x01, byte])` (literal Ctrl-A), back to `Normal`
- `Normal` + other -> `Forward([byte])`

(Implementation returns owned bytes or a small buffer rather than a borrowed slice;
shape shown for intent.)

## Error handling

- `enqueue` maps the vm_superio error to `io::Error`; the reader thread logs a
  warning and drops the byte if the FIFO rejects it (full). A 64-byte FIFO never
  fills under interactive typing.
- Non-TTY stdin: `TermiosGuard` no-ops; the reader thread still runs and ends on
  EOF. Output-only runs keep working unchanged.
- `process::exit(0)` runs only after termios restore — the terminal is never left
  in raw mode.

## Testing

- **Unit (escape state machine, pure):** Ctrl-A x -> `Quit`; Ctrl-A a ->
  `Forward([0x01, 0x61])`; plain byte -> `Forward`; Ctrl-A alone -> `Pending`.
- **Unit (`Serial::enqueue`, devices crate):** enqueue `b"hi"`, assert LSR
  data-ready bit set, `read(RBR)` returns `'h'` then `'i'`.
- **Manual integration (the bar):** run boot, type `root`↵ at the login prompt, get
  a `#` shell, run `ls /`, see output, press Ctrl-A x — harness exits and the host
  terminal is restored (`stty` sane afterward).

## Out of scope

- Serial flow control (RTS/CTS), break signaling, modem-status lines — not used by
  the Linux 8250 console.
- Multiple serial ports. Single `ttyS0` only.
- Re-signing automation for the hypervisor entitlement (a separate harness concern;
  `cargo build --workspace` strips it — re-run `scripts/sign.sh` after a build).
