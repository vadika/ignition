// 16550 MMIO UART, backed by rust-vmm's `vm_superio::Serial` — the same device
// Firecracker uses on aarch64 (FDT `compatible = "ns16550a"`).

use std::io::{self, Write};
use std::sync::Arc;

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
}
