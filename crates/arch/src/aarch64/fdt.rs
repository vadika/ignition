// Flattened Device Tree (DTB) generation for the ignition aarch64 microVM.
//
// Built with the `vm-fdt` writer crate; node construction is a stripped lift of
// Firecracker's src/vmm/src/arch/aarch64/fdt.rs (cache nodes, virtio, vmgenid,
// PCI, RTC removed — none have backing devices yet).

use vm_fdt::FdtWriter;

// Uniquely identifies the interrupt-controller node; the root and devices point
// at it via `interrupt-parent` / `phandle`.
const GIC_PHANDLE: u32 = 1;
// Uniquely identifies the fixed clock the serial node references.
const CLOCK_PHANDLE: u32 = 2;
// On ARMv8 64-bit, root address/size cells are 2.
const ADDRESS_CELLS: u32 = 2;
const SIZE_CELLS: u32 = 2;

// GIC DT interrupt encoding (Linux arm,gic binding).
const IRQ_TYPE_SPI: u32 = 0;
const IRQ_TYPE_PPI: u32 = 1;
const IRQ_TYPE_EDGE_RISING: u32 = 1;
const IRQ_TYPE_LEVEL_HI: u32 = 4;

/// An MMIO device's placement and its SPI interrupt number.
pub struct MmioDev {
    pub addr: u64,
    pub size: u64,
    /// Bare GIC SPI index (the DT cell value; the kernel adds the 32 offset).
    pub irq: u32,
}

/// GICv3 placement, supplied by the GIC milestone. Parameterized so FDT
/// generation stays pure.
pub struct GicInfo {
    pub dist_base: u64,
    pub dist_size: u64,
    pub redist_base: u64,
    pub redist_size: u64,
    /// Maintenance interrupt PPI number (typically 9).
    pub maint_irq: u32,
}

/// Everything needed to describe the machine to the guest kernel.
pub struct FdtConfig {
    pub mem_base: u64,
    pub mem_size: u64,
    /// One entry per vCPU, in boot order.
    pub cpu_mpidrs: Vec<u64>,
    /// Kernel command line -> /chosen bootargs.
    pub cmdline: String,
    pub serial: MmioDev,
    pub gic: GicInfo,
    /// (guest addr, size) when an initramfs is loaded.
    pub initrd: Option<(u64, u64)>,
}

/// Build the DTB blob. All errors originate in `vm-fdt` (e.g. an interior NUL in
/// `cmdline` -> `Error::InvalidString`).
pub fn generate(cfg: &FdtConfig) -> Result<Vec<u8>, vm_fdt::Error> {
    let mut fdt = FdtWriter::new()?;

    let root = fdt.begin_node("")?;
    fdt.property_string("compatible", "linux,dummy-virt")?;
    fdt.property_u32("#address-cells", ADDRESS_CELLS)?;
    fdt.property_u32("#size-cells", SIZE_CELLS)?;
    fdt.property_u32("interrupt-parent", GIC_PHANDLE)?;

    create_memory_node(&mut fdt, cfg.mem_base, cfg.mem_size)?;

    fdt.end_node(root)?;
    fdt.finish()
}

fn create_memory_node(fdt: &mut FdtWriter, base: u64, size: u64) -> Result<(), vm_fdt::Error> {
    let mem = fdt.begin_node("memory@ram")?;
    fdt.property_string("device_type", "memory")?;
    fdt.property_array_u64("reg", &[base, size])?;
    fdt.end_node(mem)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fdt::Fdt;

    // ---- raw big-endian property decoders (minimal reader API surface) ----
    fn be_u32s(bytes: &[u8]) -> Vec<u32> {
        bytes.chunks_exact(4).map(|c| u32::from_be_bytes(c.try_into().unwrap())).collect()
    }
    fn be_u64s(bytes: &[u8]) -> Vec<u64> {
        bytes.chunks_exact(8).map(|c| u64::from_be_bytes(c.try_into().unwrap())).collect()
    }
    /// Decode a DT string property (NUL-terminated) to &str.
    fn dt_str(bytes: &[u8]) -> &str {
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        std::str::from_utf8(&bytes[..end]).unwrap()
    }

    fn sample() -> FdtConfig {
        FdtConfig {
            mem_base: 0x4000_0000,
            mem_size: 0x2000_0000,
            cpu_mpidrs: vec![0x0, 0x1],
            cmdline: "console=ttyS0 earlycon=uart8250,mmio,0x9000000".to_string(),
            serial: MmioDev { addr: 0x0900_0000, size: 0x1000, irq: 33 },
            gic: GicInfo {
                dist_base: 0x0800_0000,
                dist_size: 0x1_0000,
                redist_base: 0x080A_0000,
                redist_size: 0xC_0000,
                maint_irq: 9,
            },
            initrd: None,
        }
    }

    #[test]
    fn blob_parses_with_root_and_memory() {
        let blob = generate(&sample()).unwrap();
        let dt = Fdt::new(&blob).expect("fdt reader must parse vm-fdt output");

        let root = dt.find_node("/").unwrap();
        assert_eq!(dt_str(root.property("compatible").unwrap().value), "linux,dummy-virt");
        assert_eq!(be_u32s(root.property("#address-cells").unwrap().value), vec![2]);

        let mem = dt.find_node("/memory@ram").unwrap();
        assert_eq!(dt_str(mem.property("device_type").unwrap().value), "memory");
        assert_eq!(be_u64s(mem.property("reg").unwrap().value), vec![0x4000_0000, 0x2000_0000]);
    }
}
