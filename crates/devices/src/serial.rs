// 16550 MMIO UART, backed by rust-vmm's `vm_superio::Serial` — the same device
// Firecracker uses on aarch64 (FDT `compatible = "ns16550a"`).

use std::io::{self, Write};

use vm_superio::Trigger;
use vm_superio::serial::NoEvents;

use crate::bus::BusDevice;

/// No-op IRQ trigger. With no interrupt controller yet, the 16550 TX-ready
/// interrupt has nowhere to go. Replaced when a GIC lands.
#[derive(Debug, Default)]
pub struct NoopTrigger;

impl Trigger for NoopTrigger {
    type E = io::Error;

    fn trigger(&self) -> io::Result<()> {
        Ok(())
    }
}

/// MMIO 16550 UART writing to sink `W` (e.g. `io::Stdout`, or a captured
/// buffer in tests).
pub struct Serial<W: Write + Send> {
    inner: vm_superio::Serial<NoopTrigger, NoEvents, W>,
}

impl<W: Write + Send> Serial<W> {
    pub fn new(out: W) -> Self {
        Self { inner: vm_superio::Serial::new(NoopTrigger, out) }
    }
}

impl<W: Write + Send> BusDevice for Serial<W> {
    fn read(&mut self, _base: u64, offset: u64, data: &mut [u8]) {
        if let (Ok(off), 1) = (u8::try_from(offset), data.len()) {
            data[0] = self.inner.read(off);
        }
    }

    fn write(&mut self, _base: u64, offset: u64, data: &[u8]) {
        if let (Ok(off), 1) = (u8::try_from(offset), data.len()) {
            let _ = self.inner.write(off, data[0]);
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
        assert!(NoopTrigger.trigger().is_ok());
    }
}
