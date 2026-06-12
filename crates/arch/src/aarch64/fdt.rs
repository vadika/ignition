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
    /// GIC SPI index as written into the DT interrupts cell (zero-based within
    /// the SPI bank). Stored verbatim; Linux computes hwirq = this + 32.
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

    create_cpu_nodes(&mut fdt, &cfg.cpu_mpidrs)?;
    create_memory_node(&mut fdt, cfg.mem_base, cfg.mem_size)?;
    create_chosen_node(&mut fdt, &cfg.cmdline, cfg.initrd)?;

    fdt.end_node(root)?;
    fdt.finish()
}

fn create_cpu_nodes(fdt: &mut FdtWriter, mpidrs: &[u64]) -> Result<(), vm_fdt::Error> {
    let cpus = fdt.begin_node("cpus")?;
    fdt.property_u32("#address-cells", 2)?;
    fdt.property_u32("#size-cells", 0)?; // cpu nodes have no size/ranges
    for (i, mpidr) in mpidrs.iter().enumerate() {
        let cpu = fdt.begin_node(&format!("cpu@{i:x}"))?;
        fdt.property_string("device_type", "cpu")?;
        fdt.property_string("compatible", "arm,arm-v8")?;
        fdt.property_string("enable-method", "psci")?;
        // Low 23 bits of MPIDR (matches FC/libkrun; covers Aff0-Aff2 for the
        // small, linear MPIDRs we assign — Aff2 bit 23 is always 0 here).
        fdt.property_u64("reg", mpidr & 0x7F_FFFF)?;
        fdt.end_node(cpu)?;
    }
    fdt.end_node(cpus)?;
    Ok(())
}

fn create_chosen_node(
    fdt: &mut FdtWriter,
    cmdline: &str,
    initrd: Option<(u64, u64)>,
) -> Result<(), vm_fdt::Error> {
    let chosen = fdt.begin_node("chosen")?;
    fdt.property_string("bootargs", cmdline)?;
    if let Some((addr, size)) = initrd {
        fdt.property_u64("linux,initrd-start", addr)?;
        fdt.property_u64("linux,initrd-end", addr + size)?;
    }
    fdt.end_node(chosen)?;
    Ok(())
}

fn create_memory_node(fdt: &mut FdtWriter, base: u64, size: u64) -> Result<(), vm_fdt::Error> {
    // Unit-address is the literal "ram" (QEMU virt convention, as FC uses), not
    // the numeric base; the kernel keys off device_type="memory", not the name.
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
        assert_eq!(be_u32s(root.property("interrupt-parent").unwrap().value), vec![1]);

        let mem = dt.find_node("/memory@ram").unwrap();
        assert_eq!(dt_str(mem.property("device_type").unwrap().value), "memory");
        assert_eq!(be_u64s(mem.property("reg").unwrap().value), vec![0x4000_0000, 0x2000_0000]);
    }

    #[test]
    fn cpu_nodes_match_mpidrs() {
        let blob = generate(&sample()).unwrap();
        let dt = Fdt::new(&blob).unwrap();
        let cpus = dt.find_node("/cpus").unwrap();
        let cpu_children: Vec<_> =
            cpus.children().filter(|c| c.name.starts_with("cpu@")).collect();
        assert_eq!(cpu_children.len(), 2);

        let cpu0 = dt.find_node("/cpus/cpu@0").unwrap();
        assert_eq!(dt_str(cpu0.property("device_type").unwrap().value), "cpu");
        assert_eq!(dt_str(cpu0.property("enable-method").unwrap().value), "psci");
        assert_eq!(be_u64s(cpu0.property("reg").unwrap().value), vec![0x0]);

        let cpu1 = dt.find_node("/cpus/cpu@1").unwrap();
        assert_eq!(dt_str(cpu1.property("enable-method").unwrap().value), "psci");
        assert_eq!(be_u64s(cpu1.property("reg").unwrap().value), vec![0x1]);
    }

    #[test]
    fn chosen_has_bootargs_and_no_initrd_by_default() {
        let blob = generate(&sample()).unwrap();
        let dt = Fdt::new(&blob).unwrap();
        let chosen = dt.find_node("/chosen").unwrap();
        assert_eq!(
            dt_str(chosen.property("bootargs").unwrap().value),
            "console=ttyS0 earlycon=uart8250,mmio,0x9000000"
        );
        assert!(chosen.property("linux,initrd-start").is_none());
        assert!(chosen.property("linux,initrd-end").is_none());
    }

    #[test]
    fn chosen_includes_initrd_when_set() {
        let mut cfg = sample();
        cfg.initrd = Some((0x4800_0000, 0x10_0000));
        let blob = generate(&cfg).unwrap();
        let dt = Fdt::new(&blob).unwrap();
        let chosen = dt.find_node("/chosen").unwrap();
        assert_eq!(be_u64s(chosen.property("linux,initrd-start").unwrap().value), vec![0x4800_0000]);
        assert_eq!(be_u64s(chosen.property("linux,initrd-end").unwrap().value), vec![0x4810_0000]);
    }
}
