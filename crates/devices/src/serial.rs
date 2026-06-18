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
    irq: Arc<dyn IrqLine>,
}

impl<W: Write + Send> Serial<W> {
    /// A serial with no interrupt line (output only; smoke tests).
    pub fn new(out: W) -> Self {
        let irq: Arc<dyn IrqLine> = Arc::new(crate::virtio::NoopIrq);
        Self { inner: vm_superio::Serial::new(SerialIrq::Noop, out), irq }
    }

    /// A serial whose interrupt pulses the given GIC line — required for the
    /// kernel's interrupt-driven tty (it blocks on the TX-empty interrupt once
    /// the 16-byte FIFO fills).
    pub fn with_irq(out: W, irq: Arc<dyn IrqLine>) -> Self {
        Self {
            inner: vm_superio::Serial::new(SerialIrq::Gic(irq.clone()), out),
            irq,
        }
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
    pub fn save_state(&self) -> SerialSnapshot {
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

    /// Build a serial from a snapshot (the restore path builds devices fresh).
    pub fn from_snapshot(out: W, irq: Arc<dyn IrqLine>, snap: &SerialSnapshot) -> Self {
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
        let inner =
            vm_superio::Serial::from_state(&vs_state, SerialIrq::Gic(irq.clone()), NoEvents, out)
                .expect("from_state with empty in_buffer cannot fail");
        Self { inner, irq }
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

use crate::device::{DeviceMgrError, FdtKind, MmioDevice};

impl<W: Write + Send + Default> MmioDevice for Serial<W> {
    fn fdt_kind(&self) -> FdtKind { FdtKind::Ns16550a }
    fn snapshot_id(&self) -> &str { "serial" }
    fn save(&self) -> serde_json::Value {
        serde_json::to_value(self.save_state()).expect("SerialSnapshot serializes")
    }
    fn restore(&mut self, v: &serde_json::Value) -> Result<(), DeviceMgrError> {
        let snap: SerialSnapshot = serde_json::from_value(v.clone())
            .map_err(|e| DeviceMgrError::StateInvalid { id: "serial".into(), reason: e.to_string() })?;
        // vm_superio applies serial state only at construction, so rebuild in place
        // from the stored irq + a fresh writer (W: Default).
        *self = Serial::from_snapshot(W::default(), self.irq.clone(), &snap);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use crate::virtio::NoopIrq;

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

    #[derive(Clone, Default)]
    struct SinkWriter;
    impl Write for SinkWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> { Ok(buf.len()) }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }

    #[test]
    fn serial_state_round_trips() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut s = Serial::new(SharedSink(buf.clone()));
        s.write(0, 1, &[0x0f]); // IER = 0x0f (offset 1)
        let st = s.save_state();
        let s2 = Serial::from_snapshot(SharedSink(buf), Arc::new(NoopIrq), &st);
        assert_eq!(s2.save_state(), st);
    }

    #[test]
    fn serial_mmio_device_roundtrips() {
        use crate::device::{FdtKind, MmioDevice};
        let irq = Arc::new(NoopIrq);
        let mut s = Serial::with_irq(SinkWriter, irq);
        s.write(0, 1, &[0xab]); // IER register — dirty some state
        let saved = MmioDevice::save(&s);
        assert_eq!(s.fdt_kind(), FdtKind::Ns16550a);
        assert_eq!(s.snapshot_id(), "serial");
        let mut s2 = Serial::with_irq(SinkWriter, Arc::new(NoopIrq));
        MmioDevice::restore(&mut s2, &saved).unwrap();
        assert_eq!(MmioDevice::save(&s2), saved);
    }
}
