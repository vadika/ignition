// 16550 MMIO UART, backed by rust-vmm's `vm_superio::Serial` — the same device
// Firecracker uses on aarch64 (FDT `compatible = "ns16550a"`).

use std::io::{self, Write};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use vm_superio::Trigger;
use vm_superio::serial::NoEvents;

use crate::bus::BusDevice;
use crate::virtio::IrqLine;

/// The 16550's interrupt line. Either discarded (no GIC — used by smoke tests)
/// or pulsed on the GIC's serial SPI. `vm_superio` calls `trigger()` when the
/// UART raises an interrupt (e.g. TX FIFO empty / RX data available); the kernel's
/// interrupt-driven 8250 tty path blocks on the TX-empty interrupt, so this must
/// be wired for anything beyond the FIFO's worth of output.
#[derive(Clone)]
pub enum SerialIrq {
    /// No interrupt controller — the interrupt is dropped.
    Noop,
    /// Pulse the (edge-triggered) serial SPI on the in-kernel GIC.
    Gic(Arc<dyn IrqLine>),
}

impl Trigger for SerialIrq {
    type E = io::Error;

    fn trigger(&self) -> io::Result<()> {
        if let SerialIrq::Gic(irq) = self {
            // Edge-rising SPI: assert then deassert; the GIC latches the edge.
            irq.set_spi(true);
            irq.set_spi(false);
        }
        Ok(())
    }
}

/// Back-compat alias for the smoke tests / output-only harnesses.
pub type NoopTrigger = SerialIrq;

/// Serializable snapshot of the 16550 register state (mirrors `vm_superio::serial::SerialState`
/// minus the RX FIFO buffer — a few buffered bytes lost on restore is acceptable).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerialSnapshot {
    pub baud_divisor_low: u8,
    pub baud_divisor_high: u8,
    pub interrupt_enable: u8,
    pub interrupt_identification: u8,
    pub line_control: u8,
    pub line_status: u8,
    pub modem_control: u8,
    pub modem_status: u8,
    pub scratch: u8,
}

/// MMIO 16550 UART writing to sink `W` (e.g. `io::Stdout`, or a captured
/// buffer in tests).
pub struct Serial<W: Write + Send> {
    inner: vm_superio::Serial<SerialIrq, NoEvents, W>,
}

impl<W: Write + Send> Serial<W> {
    /// A serial with no interrupt line (output only; smoke tests).
    pub fn new(out: W) -> Self {
        Self { inner: vm_superio::Serial::new(SerialIrq::Noop, out) }
    }

    /// A serial whose interrupt pulses the given GIC line — required for the
    /// kernel's interrupt-driven tty (it blocks on the TX-empty interrupt once
    /// the 16-byte FIFO fills).
    pub fn with_irq(out: W, irq: Arc<dyn IrqLine>) -> Self {
        Self { inner: vm_superio::Serial::new(SerialIrq::Gic(irq), out) }
    }

    /// Feed host input into the RX FIFO. Sets the LSR data-ready bit and raises
    /// the RX interrupt (via the wired Trigger) if the guest enabled it. Returns
    /// the number of bytes accepted.
    pub fn enqueue(&mut self, bytes: &[u8]) -> io::Result<usize> {
        use vm_superio::serial::Error as SerialError;
        self.inner.enqueue_raw_bytes(bytes).map_err(|e| {
            let kind = match e {
                SerialError::FullFifo => io::ErrorKind::WouldBlock,
                _ => io::ErrorKind::Other,
            };
            io::Error::new(kind, format!("serial enqueue: {e}"))
        })
    }

    /// Capture the 16550 register state for snapshot.
    pub fn save(&self) -> SerialSnapshot {
        let s = self.inner.state();
        SerialSnapshot {
            baud_divisor_low: s.baud_divisor_low,
            baud_divisor_high: s.baud_divisor_high,
            interrupt_enable: s.interrupt_enable,
            interrupt_identification: s.interrupt_identification,
            line_control: s.line_control,
            line_status: s.line_status,
            modem_control: s.modem_control,
            modem_status: s.modem_status,
            scratch: s.scratch,
        }
    }

    /// Restore register state from a snapshot in-place. Rebuilds `self.inner` from
    /// the snapshot registers, preserving the existing trigger and writer.
    pub fn restore(&mut self, snap: &SerialSnapshot) {
        use std::mem::ManuallyDrop;
        use vm_superio::serial::SerialState;

        let vs_state = SerialState {
            baud_divisor_low: snap.baud_divisor_low,
            baud_divisor_high: snap.baud_divisor_high,
            interrupt_enable: snap.interrupt_enable,
            interrupt_identification: snap.interrupt_identification,
            line_control: snap.line_control,
            line_status: snap.line_status,
            modem_control: snap.modem_control,
            modem_status: snap.modem_status,
            scratch: snap.scratch,
            in_buffer: Vec::new(),
        };

        // SAFETY: We move `self.inner` out via ptr::read (leaving the memory
        // uninitialized), immediately reconstruct it, and write it back via
        // ptr::write — so `self` is always valid at any observable point (the
        // raw pointer operations are not observable between each other and no
        // panic can occur between the read and write because from_state with an
        // empty in_buffer and no pending interrupts cannot fail).
        let new_inner = unsafe {
            let inner_ptr =
                std::ptr::addr_of_mut!(self.inner);
            // Move out without dropping.
            let old: ManuallyDrop<vm_superio::Serial<SerialIrq, NoEvents, W>> =
                ManuallyDrop::new(std::ptr::read(inner_ptr));
            // Extract trigger (cloned) and writer (moved out of old).
            let trigger = old.interrupt_evt().clone();
            let writer = ManuallyDrop::into_inner(old).into_writer();
            // Rebuild from state. from_state with empty in_buffer + no THR/RDA
            // interrupt pending (we just set the registers; any interrupt enable
            // mismatch will re-trigger, which is acceptable) will not return Err
            // unless in_buffer.len() > FIFO_SIZE, which is 0 here.
            vm_superio::Serial::from_state(&vs_state, trigger, NoEvents, writer)
                .expect("serial from_state: in_buffer empty, cannot fail")
        };
        // SAFETY: We already moved `self.inner` out above; write the rebuilt value.
        unsafe {
            std::ptr::write(std::ptr::addr_of_mut!(self.inner), new_inner);
        }
    }
}

impl<W: Write + Send> BusDevice for Serial<W> {
    fn read(&mut self, _base: u64, offset: u64, data: &mut [u8]) {
        if let (Ok(off), 1) = (u8::try_from(offset), data.len()) {
            data[0] = self.inner.read(off);
        } else {
            log::warn!("serial: ignored read (offset={offset:#x}, len={})", data.len());
        }
    }

    fn write(&mut self, _base: u64, offset: u64, data: &[u8]) {
        if let (Ok(off), 1) = (u8::try_from(offset), data.len()) {
            if let Err(e) = self.inner.write(off, data[0]) {
                log::warn!("serial: write error at offset {off:#x}: {e}");
            }
        } else {
            log::warn!("serial: ignored write (offset={offset:#x}, len={})", data.len());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// `Write` sink capturing into a shared buffer for assertions.
    #[derive(Clone)]
    struct SharedSink(Arc<Mutex<Vec<u8>>>);
    impl Write for SharedSink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn thr_writes_reach_the_sink() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut serial = Serial::new(SharedSink(buf.clone()));
        for b in b"IGNITION\n" {
            // offset 0 == THR (transmit holding register)
            serial.write(0, 0, &[*b]);
        }
        assert_eq!(buf.lock().unwrap().as_slice(), b"IGNITION\n");
    }

    #[test]
    fn noop_trigger_never_errors() {
        assert!(SerialIrq::Noop.trigger().is_ok());
    }

    #[test]
    fn enqueue_sets_data_ready_and_reads_back() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut serial = Serial::new(SharedSink(buf.clone()));
        let n = serial.enqueue(b"hi").unwrap();
        assert_eq!(n, 2);

        // LSR is register offset 5; data-ready is bit 0x01.
        let mut lsr = [0u8; 1];
        serial.read(0, 5, &mut lsr);
        assert_ne!(lsr[0] & 0x01, 0, "data-ready bit should be set after enqueue");

        // RBR is register offset 0; bytes come out in order.
        let mut b = [0u8; 1];
        serial.read(0, 0, &mut b);
        assert_eq!(b[0], b'h');
        serial.read(0, 0, &mut b);
        assert_eq!(b[0], b'i');

        assert!(buf.lock().unwrap().is_empty(), "enqueue must not write to the TX sink");
    }

    #[test]
    fn serial_state_round_trips() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut s = Serial::new(SharedSink(buf.clone()));
        s.write(0, 1, &[0x0f]); // IER = 0x0f (offset 1)
        let st = s.save();
        let mut s2 = Serial::new(SharedSink(buf));
        s2.restore(&st);
        assert_eq!(s2.save(), st);
    }
}
