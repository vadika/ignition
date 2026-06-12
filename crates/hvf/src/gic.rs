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
    hv_gic_get_redistributor_size, hv_gic_set_spi, hv_gic_set_state, hv_gic_state_create,
    hv_gic_state_get_data, hv_gic_state_get_size, os_release,
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

    /// Capture the in-kernel GIC state as an opaque blob (for snapshot).
    pub fn save_state(&self) -> Result<Vec<u8>, Error> {
        // SAFETY: state is created here, queried, copied out, and released via
        // os_release before return on every path (success and both error paths).
        unsafe {
            let state = hv_gic_state_create();
            if state.is_null() {
                return Err(Error::GicSaveState);
            }
            let result = (|| {
                let mut size: usize = 0;
                if hv_gic_state_get_size(state, &mut size) != HV_SUCCESS {
                    return Err(Error::GicSaveState);
                }
                let mut buf = vec![0u8; size];
                if hv_gic_state_get_data(state, buf.as_mut_ptr() as *mut _) != HV_SUCCESS {
                    return Err(Error::GicSaveState);
                }
                Ok(buf)
            })();
            os_release(state as *mut std::os::raw::c_void);
            result
        }
    }

    /// Restore the in-kernel GIC from a snapshot blob and return a usable
    /// `HvfGicV3` handle (for `set_spi`/`fdt_info` on the restore path).
    ///
    /// # Restore mechanism
    ///
    /// `hv_gic_set_state` restores state INTO an existing in-kernel GIC — it does
    /// not create one. So this creates the GIC with the same config/placement as
    /// `new` (recomputed from `vcpu_count`/`gic_top`), then applies the snapshot
    /// blob. The placement fields let `set_spi`/`fdt_info` work after restore.
    /// (Verified at runtime: `hv_gic_set_state` without a preceding `hv_gic_create`
    /// returns an error.)
    ///
    /// MUST be called after `hv_vm_create` and before any vCPU is created.
    pub fn from_state(blob: &[u8], vcpu_count: u64, gic_top: u64) -> Result<Self, Error> {
        // Recompute placement using the same arithmetic as `new`.
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

        let redist_base = gic_top.checked_sub(redist_size).ok_or(Error::GicCreate)?;
        let dist_base = redist_base.checked_sub(dist_size).ok_or(Error::GicCreate)?;

        // The GIC must be created (same config/placement as `new`) BEFORE its state
        // can be restored: `hv_gic_set_state` restores into an existing in-kernel
        // GIC, it does not create one. (Verified at runtime — set_state alone fails.)
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

        // Now restore the captured distributor/redistributor state into it.
        let ret = unsafe { hv_gic_set_state(blob.as_ptr() as *const _, blob.len()) };
        if ret != HV_SUCCESS {
            return Err(Error::GicRestore);
        }

        Ok(Self { dist_base, dist_size, redist_base, redist_size })
    }
}

/// Restore the in-kernel GIC from a snapshot blob. Call after `hv_vm_create` and
/// before any vCPU is created (replaces `HvfGicV3::new`'s create path).
///
/// Prefer `HvfGicV3::from_state` for restore paths that also need `set_spi` or
/// `fdt_info` — this free function is provided for callers that only need to
/// restore GIC hardware state and manage the `HvfGicV3` handle separately.
pub fn gic_restore(blob: &[u8]) -> Result<(), Error> {
    let ret = unsafe { hv_gic_set_state(blob.as_ptr() as *const _, blob.len()) };
    if ret != HV_SUCCESS {
        Err(Error::GicRestore)
    } else {
        Ok(())
    }
}
