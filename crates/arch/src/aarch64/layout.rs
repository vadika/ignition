// Memory map for the ignition aarch64 microVM. Regions are non-overlapping:
// serial MMIO at SERIAL_BASE, the GIC just below RAM_BASE (placed by HvfGicV3
// with gic_top = RAM_BASE), guest RAM at RAM_BASE, and the FDT in RAM's top
// FDT_MAX_SIZE.

/// Guest RAM base (1 GiB). The GIC sits just below this (gic_top = RAM_BASE).
pub const RAM_BASE: u64 = 0x4000_0000;
/// 16550 serial MMIO window.
pub const SERIAL_BASE: u64 = 0x0900_0000;
pub const SERIAL_SIZE: u64 = 0x1000;
/// Serial interrupt as the bare GIC SPI index written into the FDT (absolute
/// INTID = 32 + this; index 0 -> INTID 32, confirmed by the spike/src/bin/
/// gic-smoke.rs run on macOS 26).
pub const SERIAL_SPI: u32 = 0;
/// virtio-mmio device window (one block device). Above the serial, below GIC/RAM.
pub const VIRTIO_BASE: u64 = 0x0a00_0000;
/// virtio-mmio register frame size (512 bytes, per virtio 1.0 §4.2.2).
pub const VIRTIO_SIZE: u64 = 0x200;
/// virtio block IRQ as the bare GIC SPI index (absolute INTID = 32 + this = 33).
pub const VIRTIO_SPI: u32 = 1;
/// Second virtio-mmio window (the NIC). Above the block device, below GIC/RAM.
pub const NET_BASE: u64 = 0x0a00_0200;
pub const NET_SIZE: u64 = 0x200;
/// virtio-net IRQ as the bare GIC SPI index (absolute INTID = 32 + this = 34).
pub const NET_SPI: u32 = 2;
/// Reserved size for the flattened device tree.
pub const FDT_MAX_SIZE: u64 = 0x20_0000; // 2 MiB
/// The DTB must sit within the part of RAM the kernel maps early in boot (before
/// the full linear map exists). 512 MiB is a conservative upper bound for arm64.
pub const DTB_EARLY_MAP_LIMIT: u64 = 0x2000_0000; // 512 MiB

/// Address where the DTB is placed: the top `FDT_MAX_SIZE` of `min(ram_size,
/// 512 MiB)` of RAM, rounded down to an 8-byte boundary. The 512 MiB cap keeps
/// the DTB within the kernel's early-mapped window even for large RAM configs.
/// Within `[RAM_BASE, RAM_BASE + ram_size)` and clear of a kernel at `RAM_BASE`.
/// A kernel at `RAM_BASE` must fit in `DTB_EARLY_MAP_LIMIT - FDT_MAX_SIZE`
/// (~510 MiB) to clear the DTB, regardless of total `ram_size`.
/// `ram_size` must be >= `FDT_MAX_SIZE`.
pub fn fdt_addr(ram_size: u64) -> u64 {
    debug_assert!(ram_size >= FDT_MAX_SIZE, "ram_size must be >= FDT_MAX_SIZE");
    // Place the DTB at the top of usable low RAM: the top of RAM, but never above
    // the kernel's early-map window, so for ram_size > 512 MiB it sits just below
    // that limit instead of beyond it.
    let window = ram_size.min(DTB_EARLY_MAP_LIMIT);
    (RAM_BASE + window - FDT_MAX_SIZE) & !0x7
}

/// Default kernel command line, with the earlycon MMIO address kept in sync with
/// `SERIAL_BASE`.
pub fn default_cmdline() -> String {
    format!("console=ttyS0 earlycon=uart8250,mmio,{SERIAL_BASE:#x} root=/dev/vda rw rootwait reboot=k panic=1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fdt_addr_is_aligned_within_ram_and_clear_of_kernel() {
        let ram_size = 0x2000_0000; // 512 MiB
        let addr = fdt_addr(ram_size);
        assert_eq!(addr & 0x7, 0, "fdt addr must be 8-byte aligned");
        assert!(addr >= RAM_BASE, "fdt addr must be within RAM");
        assert!(addr < RAM_BASE + ram_size, "fdt addr must be within RAM");
        // A 16 MiB kernel loaded at RAM_BASE must not reach the FDT.
        let kernel_size = 0x100_0000;
        assert!(addr >= RAM_BASE + kernel_size, "fdt must clear a kernel at base");
    }

    #[test]
    fn fdt_addr_at_minimum_ram_size() {
        // The smallest valid ram_size puts the FDT exactly at RAM_BASE.
        let addr = fdt_addr(FDT_MAX_SIZE);
        assert_eq!(addr, RAM_BASE);
    }

    #[test]
    #[should_panic(expected = "ram_size must be >= FDT_MAX_SIZE")]
    fn fdt_addr_panics_below_minimum() {
        // Guards against passing e.g. bytes where MiB were intended.
        let _ = fdt_addr(FDT_MAX_SIZE - 1);
    }

    #[test]
    fn fdt_addr_large_ram_stays_within_early_map() {
        // For RAM larger than the 512 MiB early-map window, the DTB must sit
        // within the first 512 MiB, not at the top of RAM.
        let ram_size = 0x8000_0000; // 2 GiB
        let addr = fdt_addr(ram_size);
        assert_eq!(addr & 0x7, 0, "fdt addr must be 8-byte aligned");
        assert!(addr >= RAM_BASE, "fdt addr must be within RAM");
        assert!(
            addr < RAM_BASE + DTB_EARLY_MAP_LIMIT,
            "DTB must stay within the kernel's early-mapped first 512 MiB"
        );
    }

    #[test]
    fn default_cmdline_references_serial_base() {
        let cmdline = default_cmdline();
        assert!(cmdline.contains(&format!("{SERIAL_BASE:#x}")), "cmdline: {cmdline}");
        assert!(cmdline.contains("earlycon"), "cmdline: {cmdline}");
    }

    #[test]
    fn serial_window_is_below_ram() {
        // serial sits well below the GIC, which sits just below RAM.
        assert!(SERIAL_BASE + SERIAL_SIZE <= RAM_BASE);
    }

    #[test]
    fn net_window_is_adjacent_to_virtio_and_below_ram() {
        // NET_BASE must immediately follow the block device window (no gap, no overlap).
        assert_eq!(NET_BASE, VIRTIO_BASE + VIRTIO_SIZE, "net window must be adjacent to virtio");
        // The net window must fit entirely below guest RAM.
        assert!(NET_BASE + NET_SIZE <= RAM_BASE, "net window must not overlap RAM");
    }
}
