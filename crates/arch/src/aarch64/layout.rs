// Memory map for the ignition aarch64 microVM. Device MMIO is bump-allocated
// from MMIO_BASE; guest RAM starts at RAM_BASE; the FDT occupies the top
// FDT_MAX_SIZE of RAM.

/// Guest RAM base (1 GiB). The GIC sits just below this (gic_top = RAM_BASE).
pub const RAM_BASE: u64 = 0x4000_0000;
/// Device MMIO region; serial + virtio windows are bump-allocated here.
pub const MMIO_BASE: u64 = 0x0900_0000;
pub const MMIO_LEN: u64 = 0x0020_0000; // 2 MiB of device space
/// Fixed MMIO address of the boot-timer pseudo device. Placed at the TOP of the
/// device MMIO region so it never collides with the DeviceManager's bump allocator
/// (which grows up from MMIO_BASE). The guest writes the magic byte here via devmem;
/// there is no FDT node (the address is an out-of-band contract, like Firecracker).
pub const BOOT_TIMER_ADDR: u64 = MMIO_BASE + MMIO_LEN - 0x1000; // 0x091F_F000
/// Per-device window size; 16550 and virtio-mmio both fit in 0x1000.
pub const MMIO_WINDOW: u64 = 0x1000;
/// SPI allocation range (FDT interrupt index; GIC INTID = index + 32).
pub const SPI_BASE: u32 = 0;
pub const SPI_COUNT: u32 = 32;
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

/// Default kernel command line. The earlycon address matches the first device
/// window in the bump-allocated region (serial is always allocated first at
/// MMIO_BASE).
pub fn default_cmdline() -> String {
    format!("console=ttyS0 earlycon=uart8250,mmio,{MMIO_BASE:#x} root=/dev/vda rw rootwait reboot=k panic=1")
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
    fn default_cmdline_references_mmio_base() {
        let cmdline = default_cmdline();
        assert!(cmdline.contains(&format!("{MMIO_BASE:#x}")), "cmdline: {cmdline}");
        assert!(cmdline.contains("earlycon"), "cmdline: {cmdline}");
    }

    #[test]
    fn mmio_region_is_below_ram() {
        const { assert!(MMIO_BASE + MMIO_LEN <= RAM_BASE) };
    }

    #[test]
    fn mmio_window_fits_in_region() {
        const { assert!(MMIO_WINDOW <= MMIO_LEN) };
    }
}
