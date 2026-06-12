// In-kernel ARM GICv3, created through Apple's hv_gic_* API.
//
// Lifted from libkrun's devices/legacy/hvfgicv3.rs, but the hv_gic_* functions
// are called directly (declared as externs in `bindings`, resolved via the
// Hypervisor framework that hvf/build.rs links) instead of via libloading — we
// target macOS 26+, so the dlopen-for-backward-compat path is unnecessary.

use crate::Error;
use crate::bindings::{
    HV_SUCCESS, hv_gic_config_create, hv_gic_config_set_distributor_base,
    hv_gic_config_set_redistributor_base, hv_gic_create, hv_gic_get_distributor_size,
    hv_gic_get_redistributor_size, hv_gic_set_spi,
};

/// The maintenance interrupt PPI for GICv3 (matches FC/libkrun).
pub const MAINT_IRQ: u32 = 9;

/// The in-kernel GICv3 created through Apple's hv_gic_* API.
pub struct HvfGicV3 {
    dist_base: u64,
    dist_size: u64,
    redist_base: u64,
    redist_size: u64, // total redistributor region: per-cpu size * vcpu_count
}

impl HvfGicV3 {
    /// Create the in-kernel GICv3.
    ///
    /// MUST be called after `hv_vm_create` and BEFORE any vCPU is created.
    /// Places the distributor and redistributors immediately below `gic_top`
    /// (the address just above the GIC region — in practice the guest RAM base),
    /// distributor lowest.
    pub fn new(vcpu_count: u64, gic_top: u64) -> Result<Self, Error> {
        let mut dist_size: usize = 0;
        let ret = unsafe { hv_gic_get_distributor_size(&mut dist_size) };
        if ret != HV_SUCCESS {
            return Err(Error::GicCreate);
        }
        let dist_size = dist_size as u64;

        let mut redist_each: usize = 0;
        let ret = unsafe { hv_gic_get_redistributor_size(&mut redist_each) };
        if ret != HV_SUCCESS {
            return Err(Error::GicCreate);
        }
        let redist_size = redist_each as u64 * vcpu_count;

        // Place dist+redist just below `gic_top`; guard against a `gic_top` too
        // small to hold them rather than underflow-panicking.
        let redist_base = gic_top
            .checked_sub(redist_size)
            .ok_or(Error::GicCreate)?;
        let dist_base = redist_base
            .checked_sub(dist_size)
            .ok_or(Error::GicCreate)?;

        // Retained OS object; Apple says os_release when done. We intentionally
        // leak it (process-lifetime, single GIC) — matches hv_vm_config_create
        // in lib.rs. TODO: a Drop wrapper calling os_release if GICs become
        // dynamic.
        let config = unsafe { hv_gic_config_create() };
        let ret = unsafe { hv_gic_config_set_distributor_base(config, dist_base) };
        if ret != HV_SUCCESS {
            return Err(Error::GicCreate);
        }
        let ret = unsafe { hv_gic_config_set_redistributor_base(config, redist_base) };
        if ret != HV_SUCCESS {
            return Err(Error::GicCreate);
        }
        let ret = unsafe { hv_gic_create(config) };
        if ret != HV_SUCCESS {
            return Err(Error::GicCreate);
        }

        Ok(Self { dist_base, dist_size, redist_base, redist_size })
    }

    /// The FDT interrupt-controller description implied by this GIC's placement.
    pub fn fdt_info(&self) -> arch::aarch64::fdt::GicInfo {
        arch::aarch64::fdt::GicInfo {
            dist_base: self.dist_base,
            dist_size: self.dist_size,
            redist_base: self.redist_base,
            redist_size: self.redist_size,
            maint_irq: MAINT_IRQ,
        }
    }

    /// Assert (`level=true`) or deassert a shared peripheral interrupt.
    /// `intid` is the absolute GIC INTID (an SPI is `32 + spi_index`).
    pub fn set_spi(&self, intid: u32, level: bool) -> Result<(), Error> {
        let ret = unsafe { hv_gic_set_spi(intid, level) };
        if ret != HV_SUCCESS {
            Err(Error::GicSetSpi)
        } else {
            Ok(())
        }
    }
}
