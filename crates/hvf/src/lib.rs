// Copyright 2021 Red Hat, Inc.
// SPDX-License-Identifier: Apache-2.0

#[allow(non_camel_case_types)]
#[allow(improper_ctypes)]
#[allow(dead_code)]
#[allow(non_snake_case)]
#[allow(non_upper_case_globals)]
#[allow(deref_nullptr)]
pub mod bindings;
pub mod gic;

#[macro_use]
extern crate log;

use bindings::*;
use serde::{Deserialize, Serialize};

#[cfg(target_arch = "aarch64")]
use std::arch::asm;

use std::convert::TryInto;
use std::fmt::{Display, Formatter};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

/// The sysregs captured for snapshot/restore (EL1 guest-resume set + the EL2 regs
/// set at boot + the generic timer). MPIDR_EL1 is set at vCPU create, not here.
const SAVED_SYSREGS: &[hv_sys_reg_t] = &[
    hv_sys_reg_t_HV_SYS_REG_SCTLR_EL1, hv_sys_reg_t_HV_SYS_REG_TTBR0_EL1,
    hv_sys_reg_t_HV_SYS_REG_TTBR1_EL1, hv_sys_reg_t_HV_SYS_REG_TCR_EL1,
    hv_sys_reg_t_HV_SYS_REG_MAIR_EL1, hv_sys_reg_t_HV_SYS_REG_AMAIR_EL1,
    hv_sys_reg_t_HV_SYS_REG_VBAR_EL1, hv_sys_reg_t_HV_SYS_REG_SP_EL0,
    hv_sys_reg_t_HV_SYS_REG_SP_EL1, hv_sys_reg_t_HV_SYS_REG_ELR_EL1,
    hv_sys_reg_t_HV_SYS_REG_SPSR_EL1, hv_sys_reg_t_HV_SYS_REG_ESR_EL1,
    hv_sys_reg_t_HV_SYS_REG_FAR_EL1, hv_sys_reg_t_HV_SYS_REG_CONTEXTIDR_EL1,
    hv_sys_reg_t_HV_SYS_REG_TPIDR_EL0, hv_sys_reg_t_HV_SYS_REG_TPIDR_EL1,
    hv_sys_reg_t_HV_SYS_REG_TPIDRRO_EL0, hv_sys_reg_t_HV_SYS_REG_CPACR_EL1,
    hv_sys_reg_t_HV_SYS_REG_CSSELR_EL1, hv_sys_reg_t_HV_SYS_REG_AFSR0_EL1,
    hv_sys_reg_t_HV_SYS_REG_AFSR1_EL1, hv_sys_reg_t_HV_SYS_REG_PAR_EL1,
    hv_sys_reg_t_HV_SYS_REG_MDSCR_EL1,
    hv_sys_reg_t_HV_SYS_REG_CNTV_CTL_EL0, hv_sys_reg_t_HV_SYS_REG_CNTV_CVAL_EL0,
    hv_sys_reg_t_HV_SYS_REG_CNTKCTL_EL1, hv_sys_reg_t_HV_SYS_REG_CNTP_CTL_EL0,
    hv_sys_reg_t_HV_SYS_REG_CNTP_CVAL_EL0,
    // Pointer-authentication keys: the kernel signs return addresses with these
    // (pac*/aut* instructions). A restored vCPU with different keys fails `autiasp`
    // and crashes — these MUST be captured.
    hv_sys_reg_t_HV_SYS_REG_APIAKEYLO_EL1, hv_sys_reg_t_HV_SYS_REG_APIAKEYHI_EL1,
    hv_sys_reg_t_HV_SYS_REG_APIBKEYLO_EL1, hv_sys_reg_t_HV_SYS_REG_APIBKEYHI_EL1,
    hv_sys_reg_t_HV_SYS_REG_APDAKEYLO_EL1, hv_sys_reg_t_HV_SYS_REG_APDAKEYHI_EL1,
    hv_sys_reg_t_HV_SYS_REG_APDBKEYLO_EL1, hv_sys_reg_t_HV_SYS_REG_APDBKEYHI_EL1,
    hv_sys_reg_t_HV_SYS_REG_APGAKEYLO_EL1, hv_sys_reg_t_HV_SYS_REG_APGAKEYHI_EL1,
];

/// GP registers captured: X0..X30, PC, CPSR (33 entries, in this order).
/// SP_EL0/EL1 are captured as sysregs. X29=FP, X30=LR.
const SAVED_GP: &[hv_reg_t] = &[
    hv_reg_t_HV_REG_X0,  hv_reg_t_HV_REG_X1,  hv_reg_t_HV_REG_X2,  hv_reg_t_HV_REG_X3,
    hv_reg_t_HV_REG_X4,  hv_reg_t_HV_REG_X5,  hv_reg_t_HV_REG_X6,  hv_reg_t_HV_REG_X7,
    hv_reg_t_HV_REG_X8,  hv_reg_t_HV_REG_X9,  hv_reg_t_HV_REG_X10, hv_reg_t_HV_REG_X11,
    hv_reg_t_HV_REG_X12, hv_reg_t_HV_REG_X13, hv_reg_t_HV_REG_X14, hv_reg_t_HV_REG_X15,
    hv_reg_t_HV_REG_X16, hv_reg_t_HV_REG_X17, hv_reg_t_HV_REG_X18, hv_reg_t_HV_REG_X19,
    hv_reg_t_HV_REG_X20, hv_reg_t_HV_REG_X21, hv_reg_t_HV_REG_X22, hv_reg_t_HV_REG_X23,
    hv_reg_t_HV_REG_X24, hv_reg_t_HV_REG_X25, hv_reg_t_HV_REG_X26, hv_reg_t_HV_REG_X27,
    hv_reg_t_HV_REG_X28, hv_reg_t_HV_REG_X29, hv_reg_t_HV_REG_X30,
    hv_reg_t_HV_REG_PC,  hv_reg_t_HV_REG_CPSR,
];

/// Per-vCPU GIC CPU-interface (ICC) registers captured for snapshot/restore. These
/// are NOT in the `hv_gic_state` blob (which is the distributor/redistributor global
/// state) — they live in the vCPU's CPU interface and control whether it can take
/// interrupts (group enable, priority mask). Without them a restored vCPU has the
/// interface at reset (interrupts masked) and the guest hangs. RPR (read-only) and
/// the EL2 SRE (nested-only) are excluded.
const SAVED_ICC: &[hv_gic_icc_reg_t] = &[
    hv_gic_icc_reg_t_HV_GIC_ICC_REG_SRE_EL1,
    hv_gic_icc_reg_t_HV_GIC_ICC_REG_CTLR_EL1,
    hv_gic_icc_reg_t_HV_GIC_ICC_REG_PMR_EL1,
    hv_gic_icc_reg_t_HV_GIC_ICC_REG_BPR0_EL1,
    hv_gic_icc_reg_t_HV_GIC_ICC_REG_BPR1_EL1,
    hv_gic_icc_reg_t_HV_GIC_ICC_REG_IGRPEN0_EL1,
    hv_gic_icc_reg_t_HV_GIC_ICC_REG_IGRPEN1_EL1,
    hv_gic_icc_reg_t_HV_GIC_ICC_REG_AP0R0_EL1,
    hv_gic_icc_reg_t_HV_GIC_ICC_REG_AP1R0_EL1,
];

/// Serializable vCPU state for snapshot/restore.
///
/// Captures EL1 guest state for the **non-nested** configuration (the only one
/// the boot harness uses). Nested-mode EL2 control registers (HCR_EL2,
/// CNTHCTL_EL2) are not captured here because non-nested boot does not program
/// them through the snapshot path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VcpuState {
    /// One value per `SAVED_GP` entry, in order.
    pub gp: Vec<u64>,
    /// (hv_sys_reg_t as u32, value) per `SAVED_SYSREGS` entry.
    pub sysregs: Vec<(u32, u64)>,
    pub vtimer_mask: bool,
    pub vtimer_offset: u64,
    /// NEON/FP Q0..Q31 registers, stored as 128-bit little-endian values.
    /// 32 entries in order Q0..Q31.
    pub simd: Vec<u128>,
    /// Floating-point Control Register (FPCR).
    pub fpcr: u64,
    /// Floating-point Status Register (FPSR).
    pub fpsr: u64,
    /// (hv_gic_icc_reg_t as u32, value) per `SAVED_ICC` entry — the per-vCPU GIC
    /// CPU-interface state (interrupt enable/mask). Restored after the GIC + vCPU
    /// exist.
    pub icc: Vec<(u32, u64)>,
    /// The host physical counter (`mach_absolute_time`) at snapshot time. On
    /// restore the vtimer offset is adjusted by the elapsed host counter so the
    /// guest's virtual time (CNTVCT) continues from here instead of jumping to the
    /// host's much-larger uptime — which would otherwise bury the guest in expired
    /// timer interrupts and spin it forever.
    pub host_counter: u64,
}

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
use ignition_arch::aarch64::sysreg::{SYSREG_MASK, sys_reg_name};
use log::debug;

unsafe extern "C" {
    pub fn mach_absolute_time() -> u64;
}

const HV_EXIT_REASON_CANCELED: hv_exit_reason_t = 0;
const HV_EXIT_REASON_EXCEPTION: hv_exit_reason_t = 1;
const HV_EXIT_REASON_VTIMER_ACTIVATED: hv_exit_reason_t = 2;

const TMR_CTL_ENABLE: u64 = 1 << 0;
const TMR_CTL_IMASK: u64 = 1 << 1;
const TMR_CTL_ISTATUS: u64 = 1 << 2;

const PSR_MODE_EL1H: u64 = 0x0000_0005;
const PSR_MODE_EL2H: u64 = 0x0000_0009;
const PSR_F_BIT: u64 = 0x0000_0040;
const PSR_I_BIT: u64 = 0x0000_0080;
const PSR_A_BIT: u64 = 0x0000_0100;
const PSR_D_BIT: u64 = 0x0000_0200;
const PSTATE_EL1_FAULT_BITS_64: u64 = PSR_MODE_EL1H | PSR_A_BIT | PSR_F_BIT | PSR_I_BIT | PSR_D_BIT;
const PSTATE_EL2_FAULT_BITS_64: u64 = PSR_MODE_EL2H | PSR_A_BIT | PSR_F_BIT | PSR_I_BIT | PSR_D_BIT;

const HCR_TLOR: u64 = 1 << 35;
const HCR_RW: u64 = 1 << 31;
const HCR_TSW: u64 = 1 << 22;
const HCR_TACR: u64 = 1 << 21;
const HCR_TIDCP: u64 = 1 << 20;
const HCR_TSC: u64 = 1 << 19;
const HCR_TID3: u64 = 1 << 18;
const HCR_TWE: u64 = 1 << 14;
const HCR_TWI: u64 = 1 << 13;
const HCR_BSU_IS: u64 = 1 << 10;
const HCR_FB: u64 = 1 << 9;
const HCR_AMO: u64 = 1 << 5;
const HCR_IMO: u64 = 1 << 4;
const HCR_FMO: u64 = 1 << 3;
const HCR_PTW: u64 = 1 << 2;
const HCR_SWIO: u64 = 1 << 1;
const HCR_VM: u64 = 1 << 0;
// Use the same bits as KVM uses in vcpu reset.
const HCR_EL2_BITS: u64 = HCR_TSC
    | HCR_TSW
    | HCR_TWE
    | HCR_TWI
    | HCR_VM
    | HCR_BSU_IS
    | HCR_FB
    | HCR_TACR
    | HCR_AMO
    | HCR_SWIO
    | HCR_TIDCP
    | HCR_RW
    | HCR_TLOR
    | HCR_FMO
    | HCR_IMO
    | HCR_PTW
    | HCR_TID3;

const CNTHCTL_EL0VCTEN: u64 = 1 << 1;
const CNTHCTL_EL0PCTEN: u64 = 1 << 0;
// Trap accesses to both virtual and physical counter registers.
const CNTHCTL_EL2_BITS: u64 = CNTHCTL_EL0VCTEN | CNTHCTL_EL0PCTEN;

const AA64PFR0_EL1_EL2EN: u64 = 1 << 8;
const AA64PFR0_EL1_GIC3EN: u64 = 1 << 24;
const AA64PFR1_EL1_SMEMASK: u64 = 3 << 24;

const EC_WFX_TRAP: u64 = 0x1;
const EC_AA64_HVC: u64 = 0x16;
const EC_AA64_SMC: u64 = 0x17;
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
const EC_SYSTEMREGISTERTRAP: u64 = 0x18;
const EC_DATAABORT: u64 = 0x24;
const EC_AA64_BKPT: u64 = 0x3c;

/// PSCI return value for an unrecognized function id (SMCCC: -1 in X0/W0).
const PSCI_NOT_SUPPORTED: u64 = -1_i64 as u64;

#[derive(Debug)]
pub enum Error {
    EnableEL2,
    FindSymbol(libloading::Error),
    MemoryMap,
    MemoryProtect,
    MemoryUnmap,
    NestedCheck,
    VcpuCreate,
    VcpuInitialRegisters,
    VcpuReadRegister,
    VcpuReadSystemRegister,
    VcpuRequestExit,
    VcpuRun,
    VcpuSetPendingIrq,
    VcpuSetRegister,
    VcpuSetSystemRegister(u16, u64),
    VcpuSetVtimerMask,
    VmCreate,
    GicCreate,
    GicSaveState,
    GicRestore,
    GicSetSpi,
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        use self::Error::*;

        match self {
            EnableEL2 => write!(f, "Error enabling EL2 mode in HVF"),
            FindSymbol(err) => write!(f, "Couldn't find symbol in HVF library: {err}"),
            MemoryMap => write!(f, "Error registering memory region in HVF"),
            MemoryProtect => write!(f, "Error re-protecting memory region in HVF"),
            MemoryUnmap => write!(f, "Error unregistering memory region in HVF"),
            NestedCheck => write!(
                f,
                "Nested virtualization was requested but it's not support in this system"
            ),
            VcpuCreate => write!(f, "Error creating HVF vCPU instance"),
            VcpuInitialRegisters => write!(f, "Error setting up initial HVF vCPU registers"),
            VcpuReadRegister => write!(f, "Error reading HVF vCPU register"),
            VcpuReadSystemRegister => write!(f, "Error reading HVF vCPU system register"),
            VcpuRequestExit => write!(f, "Error requesting HVF vCPU exit"),
            VcpuRun => write!(f, "Error running HVF vCPU"),
            VcpuSetPendingIrq => write!(f, "Error setting HVF vCPU pending irq"),
            VcpuSetRegister => write!(f, "Error setting HVF vCPU register"),
            VcpuSetSystemRegister(reg, val) => write!(
                f,
                "Error setting HVF vCPU system register 0x{reg:#x} to 0x{val:#x}"
            ),
            VcpuSetVtimerMask => write!(f, "Error setting HVF vCPU vtimer mask"),
            VmCreate => write!(f, "Error creating HVF VM instance"),
            GicCreate => write!(f, "Error creating in-kernel HVF GIC"),
            GicSaveState => write!(f, "Error saving HVF GIC state"),
            GicRestore => write!(f, "Error restoring HVF GIC state"),
            GicSetSpi => write!(f, "Error setting HVF GIC SPI level"),
        }
    }
}

impl std::error::Error for Error {}

pub enum InterruptType {
    Irq,
    Fiq,
}

pub trait Vcpus {
    fn set_vtimer_irq(&self, vcpuid: u64);
    fn should_wait(&self, vcpuid: u64) -> bool;
    fn has_pending_irq(&self, vcpuid: u64) -> bool;
    fn get_pending_irq(&self, vcpuid: u64) -> u32;
    fn handle_sysreg_read(&self, vcpuid: u64, reg: u32) -> Option<u64>;
    fn handle_sysreg_write(&self, vcpuid: u64, reg: u32, val: u64) -> bool;
}

/// A `Vcpus` impl for the in-kernel GIC: the userspace IRQ/sysreg path is stubbed
/// because `hv_gic` delivers interrupts and per-cpu timers in-kernel. Used by every
/// vCPU runner.
pub struct NoIrqVcpus;

impl Vcpus for NoIrqVcpus {
    fn set_vtimer_irq(&self, _vcpuid: u64) {}
    fn should_wait(&self, _vcpuid: u64) -> bool { false }
    fn has_pending_irq(&self, _vcpuid: u64) -> bool { false }
    fn get_pending_irq(&self, _vcpuid: u64) -> u32 { 0 }
    fn handle_sysreg_read(&self, _vcpuid: u64, _reg: u32) -> Option<u64> { Some(0) }
    fn handle_sysreg_write(&self, _vcpuid: u64, _reg: u32, _val: u64) -> bool { true }
}

/// Re-protect an already-mapped guest range, process-globally. HVF's VM is a
/// per-process singleton (`hv_vm_create` takes no handle), so `hv_vm_protect`
/// needs no `Vm` reference — the dirty-fault run loop, which has no `Vm` handle,
/// calls this to re-grant WRITE on a faulting page. `flags` is a bitwise-or of
/// `HV_MEMORY_READ`/`HV_MEMORY_WRITE`/`HV_MEMORY_EXEC`; `guest_addr` must be
/// page-aligned and `size` a page multiple.
/// Serializes `hv_vm_protect`. The HVF VM is a per-process singleton and
/// `hv_vm_protect` mutates its stage-2 page tables VM-wide; Apple does not
/// document it as safe for concurrent calls. With multiple vCPU threads under
/// `--track-dirty`, each first-write-per-page fault re-grants WRITE on its own
/// page concurrently, so we hold this lock across the call. It is taken only on
/// first-write faults (~5µs each), so contention is negligible.
static PROTECT_LOCK: Mutex<()> = Mutex::new(());

pub fn vm_protect_memory(guest_addr: u64, size: u64, flags: u64) -> Result<(), Error> {
    let _guard = PROTECT_LOCK.lock().unwrap();
    let ret = unsafe { hv_vm_protect(guest_addr, size.try_into().unwrap(), flags as hv_memory_flags_t) };
    if ret != HV_SUCCESS {
        Err(Error::MemoryProtect)
    } else {
        Ok(())
    }
}

pub fn vcpu_request_exit(vcpuid: u64) -> Result<(), Error> {
    let mut vcpu: u64 = vcpuid;
    let ret = unsafe { hv_vcpus_exit(&mut vcpu, 1) };

    if ret != HV_SUCCESS {
        Err(Error::VcpuRequestExit)
    } else {
        Ok(())
    }
}

pub fn vcpu_set_pending_irq(
    vcpuid: u64,
    irq_type: InterruptType,
    pending: bool,
) -> Result<(), Error> {
    let _type = match irq_type {
        InterruptType::Irq => hv_interrupt_type_t_HV_INTERRUPT_TYPE_IRQ,
        InterruptType::Fiq => hv_interrupt_type_t_HV_INTERRUPT_TYPE_FIQ,
    };

    let ret = unsafe { hv_vcpu_set_pending_interrupt(vcpuid, _type, pending) };

    if ret != HV_SUCCESS {
        Err(Error::VcpuSetPendingIrq)
    } else {
        Ok(())
    }
}

pub fn vcpu_set_vtimer_mask(vcpuid: u64, masked: bool) -> Result<(), Error> {
    let ret = unsafe { hv_vcpu_set_vtimer_mask(vcpuid, masked) };

    if ret != HV_SUCCESS {
        Err(Error::VcpuSetVtimerMask)
    } else {
        Ok(())
    }
}

/// Checks if Nested Virtualization is supported on the current system. Only
/// M3 or newer chips on macOS 15+ will satisfy the requirements.
pub fn check_nested_virt() -> Result<bool, Error> {
    type GetEL2Supported =
        libloading::Symbol<'static, unsafe extern "C" fn(*mut bool) -> hv_return_t>;

    let get_el2_supported: Result<GetEL2Supported, libloading::Error> =
        unsafe { HVF.get(b"hv_vm_config_get_el2_supported") };
    if get_el2_supported.is_err() {
        info!("cannot find hv_vm_config_get_el2_supported symbol");
        return Ok(false);
    }

    let mut el2_supported: bool = false;
    let ret = unsafe { (get_el2_supported.unwrap())(&mut el2_supported) };
    if ret != HV_SUCCESS {
        error!("hv_vm_config_get_el2_supported failed: {ret:?}");
        return Err(Error::NestedCheck);
    }

    Ok(el2_supported)
}

pub struct HvfVm {}

static HVF: LazyLock<libloading::Library> = LazyLock::new(|| unsafe {
    libloading::Library::new(
        "/System/Library/Frameworks/Hypervisor.framework/Versions/A/Hypervisor",
    )
    .unwrap()
});

impl HvfVm {
    pub fn new(nested_enabled: bool) -> Result<Self, Error> {
        let config = unsafe { hv_vm_config_create() };
        if nested_enabled {
            let set_el2_enabled: libloading::Symbol<
                'static,
                unsafe extern "C" fn(hv_vm_config_t, bool) -> hv_return_t,
            > = unsafe {
                HVF.get(b"hv_vm_config_set_el2_enabled")
                    .map_err(Error::FindSymbol)?
            };

            let ret = unsafe { (set_el2_enabled)(config, true) };
            if ret != HV_SUCCESS {
                return Err(Error::EnableEL2);
            }
        }

        let ret = unsafe { hv_vm_create(config) };

        if ret != HV_SUCCESS {
            Err(Error::VmCreate)
        } else {
            Ok(Self {})
        }
    }

    pub fn map_memory(
        &self,
        host_start_addr: u64,
        guest_start_addr: u64,
        size: u64,
    ) -> Result<(), Error> {
        let ret = unsafe {
            hv_vm_map(
                host_start_addr as *mut core::ffi::c_void,
                guest_start_addr,
                size.try_into().unwrap(),
                (HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC).into(),
            )
        };
        if ret != HV_SUCCESS {
            Err(Error::MemoryMap)
        } else {
            Ok(())
        }
    }

    pub fn unmap_memory(&self, guest_start_addr: u64, size: u64) -> Result<(), Error> {
        let ret = unsafe { hv_vm_unmap(guest_start_addr, size.try_into().unwrap()) };
        if ret != HV_SUCCESS {
            Err(Error::MemoryUnmap)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug)]
pub enum VcpuExit<'a> {
    Breakpoint,
    Canceled,
    CpuOn(u64, u64, u64),
    DirtyFault(u64),
    HypervisorCall,
    MmioRead(u64, &'a mut [u8]),
    MmioWrite(u64, &'a [u8]),
    PsciHandled,
    SecureMonitorCall,
    Shutdown,
    SystemRegister,
    VtimerActivated,
    WaitForEvent,
    WaitForEventExpired,
    WaitForEventTimeout(Duration),
}

struct MmioRead {
    len: usize,
    srt: u32,
}

pub struct HvfVcpu<'a> {
    vcpuid: hv_vcpu_t,
    vcpu_exit: &'a hv_vcpu_exit_t,
    cntfrq: u64,
    mmio_buf: [u8; 8],
    pending_mmio_read: Option<MmioRead>,
    pending_advance_pc: bool,
    vtimer_masked: bool,
    nested_enabled: bool,
    dirty_tracking: bool,
    ram_base: u64,
    ram_size: u64,
}

/// Write the low `len` little-endian bytes of `val` into `buf` (the MMIO data
/// buffer). `len` is `1 << sas` from the data-abort syndrome, so it is always one
/// of 1, 2, 4, 8.
fn encode_mmio_le(buf: &mut [u8], val: u64, len: usize) {
    debug_assert!(matches!(len, 1 | 2 | 4 | 8), "mmio len must be 1/2/4/8, got {len}");
    let bytes = val.to_le_bytes();
    buf[..len].copy_from_slice(&bytes[..len]);
}

/// Read `len` little-endian bytes from `buf` as a zero-extended `u64`. `len` is
/// `1 << sas`, always one of 1, 2, 4, 8.
fn decode_mmio_le(buf: &[u8], len: usize) -> u64 {
    debug_assert!(matches!(len, 1 | 2 | 4 | 8), "mmio len must be 1/2/4/8, got {len}");
    let mut bytes = [0u8; 8];
    bytes[..len].copy_from_slice(&buf[..len]);
    u64::from_le_bytes(bytes)
}

impl HvfVcpu<'_> {
    pub fn new(mpidr: u64, nested_enabled: bool) -> Result<Self, Error> {
        let mut vcpuid: hv_vcpu_t = 0;
        let vcpu_exit_ptr: *mut hv_vcpu_exit_t = std::ptr::null_mut();

        #[cfg(target_arch = "aarch64")]
        let cntfrq = {
            let cntfrq: u64;
            unsafe { asm!("mrs {}, cntfrq_el0", out(reg) cntfrq) };
            cntfrq
        };
        #[cfg(target_arch = "x86_64")]
        let cntfrq = 0u64;
        #[cfg(target_arch = "riscv64")]
        let cntfrq = 0u64;

        let ret = unsafe {
            hv_vcpu_create(
                &mut vcpuid,
                &vcpu_exit_ptr as *const _ as *mut *mut _,
                std::ptr::null_mut(),
            )
        };
        if ret != HV_SUCCESS {
            return Err(Error::VcpuCreate);
        }

        // Set MPIDR_EL1 to the caller's linear MPIDR (Aff0 = cpu index, via
        // VcpuManager::mpidr_for). HVF's in-kernel GICv3 matches this affinity to
        // the per-cpu redistributor; verified across --smp 2/4 with no CPU_ON
        // mismatch. (libkrun shifted the id into Aff1; Aff0 works here.)
        let ret = unsafe { hv_vcpu_set_sys_reg(vcpuid, hv_sys_reg_t_HV_SYS_REG_MPIDR_EL1, mpidr) };
        if ret != HV_SUCCESS {
            return Err(Error::VcpuCreate);
        }

        let vcpu_exit: &hv_vcpu_exit_t = unsafe { vcpu_exit_ptr.as_mut().unwrap() };

        Ok(Self {
            vcpuid,
            vcpu_exit,
            cntfrq,
            mmio_buf: [0; 8],
            pending_mmio_read: None,
            pending_advance_pc: false,
            vtimer_masked: false,
            nested_enabled,
            dirty_tracking: false,
            ram_base: 0,
            ram_size: 0,
        })
    }

    /// Enable dirty-page tracking and set the guest-RAM window used to
    /// disambiguate write-protect dirty faults from MMIO data aborts.
    pub fn set_dirty_window(&mut self, base: u64, size: u64) {
        self.ram_base = base;
        self.ram_size = size;
        self.dirty_tracking = true;
    }

    /// Full initial register/system-register setup shared by the primary
    /// (`set_initial_state`, X0 = FDT address) and secondaries
    /// (`set_secondary_state`, X0 = PSCI context id). Sets EL2/GICv3/SME/CPSR,
    /// `PC = entry_addr`, and `X0 = x0`.
    fn setup_registers(&self, entry_addr: u64, x0: u64) -> Result<(), Error> {
        if self.nested_enabled {
            let ret = unsafe {
                hv_vcpu_set_reg(self.vcpuid, hv_reg_t_HV_REG_CPSR, PSTATE_EL2_FAULT_BITS_64)
            };
            if ret != HV_SUCCESS {
                return Err(Error::VcpuInitialRegisters);
            }

            let ret = unsafe {
                hv_vcpu_set_sys_reg(self.vcpuid, hv_sys_reg_t_HV_SYS_REG_HCR_EL2, HCR_EL2_BITS)
            };
            if ret != HV_SUCCESS {
                return Err(Error::VcpuInitialRegisters);
            }

            let ret = unsafe {
                hv_vcpu_set_sys_reg(
                    self.vcpuid,
                    hv_sys_reg_t_HV_SYS_REG_CNTHCTL_EL2,
                    CNTHCTL_EL2_BITS,
                )
            };
            if ret != HV_SUCCESS {
                return Err(Error::VcpuInitialRegisters);
            }

            // Enable EL2 and GICv3 in ID_AA64PFR0_EL1
            let val: u64 = 0;
            let ret = unsafe {
                hv_vcpu_get_sys_reg(
                    self.vcpuid,
                    hv_sys_reg_t_HV_SYS_REG_ID_AA64PFR0_EL1,
                    &val as *const _ as *mut _,
                )
            };
            if ret != HV_SUCCESS {
                return Err(Error::VcpuInitialRegisters);
            }
            let ret = unsafe {
                hv_vcpu_set_sys_reg(
                    self.vcpuid,
                    hv_sys_reg_t_HV_SYS_REG_ID_AA64PFR0_EL1,
                    val | AA64PFR0_EL1_EL2EN | AA64PFR0_EL1_GIC3EN,
                )
            };
            if ret != HV_SUCCESS {
                return Err(Error::VcpuInitialRegisters);
            }

            // If SME is enabled in ID_AA64PFR1_EL1 in the VM, the guest will
            // break after enabling the MMU. Mask it out.
            let val: u64 = 0;
            let ret = unsafe {
                hv_vcpu_get_sys_reg(
                    self.vcpuid,
                    hv_sys_reg_t_HV_SYS_REG_ID_AA64PFR1_EL1,
                    &val as *const _ as *mut _,
                )
            };
            if ret != HV_SUCCESS {
                return Err(Error::VcpuInitialRegisters);
            }
            let ret = unsafe {
                hv_vcpu_set_sys_reg(
                    self.vcpuid,
                    hv_sys_reg_t_HV_SYS_REG_ID_AA64PFR1_EL1,
                    val & !AA64PFR1_EL1_SMEMASK,
                )
            };
            if ret != HV_SUCCESS {
                return Err(Error::VcpuInitialRegisters);
            }
        } else {
            let ret = unsafe {
                hv_vcpu_set_reg(self.vcpuid, hv_reg_t_HV_REG_CPSR, PSTATE_EL1_FAULT_BITS_64)
            };
            if ret != HV_SUCCESS {
                return Err(Error::VcpuInitialRegisters);
            }
        }

        let ret = unsafe { hv_vcpu_set_reg(self.vcpuid, hv_reg_t_HV_REG_PC, entry_addr) };
        if ret != HV_SUCCESS {
            return Err(Error::VcpuInitialRegisters);
        }

        let ret = unsafe { hv_vcpu_set_reg(self.vcpuid, hv_reg_t_HV_REG_X0, x0) };
        if ret != HV_SUCCESS {
            return Err(Error::VcpuInitialRegisters);
        }

        Ok(())
    }

    /// Primary vCPU initial state: `PC = entry_addr`, `X0 = fdt_addr`.
    pub fn set_initial_state(&self, entry_addr: u64, fdt_addr: u64) -> Result<(), Error> {
        self.setup_registers(entry_addr, fdt_addr)
    }

    /// Secondary vCPU initial state on PSCI CPU_ON: `PC = entry_addr`,
    /// `X0 = context_id` (the value the guest passed in X3, returned in X0 to
    /// `__secondary_switched`).
    pub fn set_secondary_state(&self, entry_addr: u64, context_id: u64) -> Result<(), Error> {
        self.setup_registers(entry_addr, context_id)
    }

    pub fn id(&self) -> u64 {
        self.vcpuid
    }

    /// The current program counter (debug/sampling aid).
    fn read_reg(&self, reg: u32) -> Result<u64, Error> {
        let val: u64 = 0;
        let ret = unsafe { hv_vcpu_get_reg(self.vcpuid, reg, &val as *const _ as *mut _) };
        if ret != HV_SUCCESS {
            Err(Error::VcpuReadRegister)
        } else {
            Ok(val)
        }
    }

    pub fn write_reg(&self, rt: u32, val: u64) -> Result<(), Error> {
        let ret = unsafe { hv_vcpu_set_reg(self.vcpuid, rt, val) };
        if ret != HV_SUCCESS {
            Err(Error::VcpuSetRegister)
        } else {
            Ok(())
        }
    }

    fn read_sys_reg(&self, reg: u16) -> Result<u64, Error> {
        let val: u64 = 0;
        let ret = unsafe { hv_vcpu_get_sys_reg(self.vcpuid, reg, &val as *const _ as *mut _) };
        if ret != HV_SUCCESS {
            Err(Error::VcpuReadSystemRegister)
        } else {
            Ok(val)
        }
    }

    fn write_sys_reg(&self, reg: u16, val: u64) -> Result<(), Error> {
        let ret = unsafe { hv_vcpu_set_sys_reg(self.vcpuid, reg, val) };
        if ret != HV_SUCCESS {
            Err(Error::VcpuSetSystemRegister(reg, val))
        } else {
            Ok(())
        }
    }

    /// Capture all snapshot state. MUST be called on the vCPU's own thread.
    pub fn save_state(&self) -> Result<VcpuState, Error> {
        let gp = SAVED_GP
            .iter()
            .map(|&r| self.read_reg(r))
            .collect::<Result<Vec<_>, _>>()?;
        let sysregs = SAVED_SYSREGS
            .iter()
            .map(|&r| Ok((r as u32, self.read_sys_reg(r)?)))
            .collect::<Result<Vec<_>, Error>>()?;
        let mut vtimer_mask = false;
        let ret = unsafe { hv_vcpu_get_vtimer_mask(self.vcpuid, &mut vtimer_mask) };
        if ret != HV_SUCCESS {
            return Err(Error::VcpuReadRegister);
        }
        let mut vtimer_offset = 0u64;
        let ret = unsafe { hv_vcpu_get_vtimer_offset(self.vcpuid, &mut vtimer_offset) };
        if ret != HV_SUCCESS {
            return Err(Error::VcpuReadRegister);
        }
        let fpcr = self.read_reg(hv_reg_t_HV_REG_FPCR)?;
        let fpsr = self.read_reg(hv_reg_t_HV_REG_FPSR)?;
        let simd = (0u32..32)
            .map(|q| -> Result<u128, Error> {
                let mut val: hv_simd_fp_uchar16_t = 0;
                let ret = unsafe {
                    hv_vcpu_get_simd_fp_reg(self.vcpuid, q, &mut val)
                };
                if ret != HV_SUCCESS {
                    Err(Error::VcpuReadRegister)
                } else {
                    Ok(val)
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        let icc = SAVED_ICC
            .iter()
            .map(|&r| -> Result<(u32, u64), Error> {
                let mut val: u64 = 0;
                let ret = unsafe { hv_gic_get_icc_reg(self.vcpuid, r, &mut val) };
                if ret != HV_SUCCESS {
                    Err(Error::GicSaveState)
                } else {
                    Ok((r as u32, val))
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        let host_counter = unsafe { mach_absolute_time() };
        Ok(VcpuState {
            gp, sysregs, vtimer_mask, vtimer_offset, simd, fpcr, fpsr, icc, host_counter,
        })
    }

    /// Restore all snapshot state onto a freshly-created vCPU. MUST run on the
    /// vCPU's own thread, before the first `run()`.
    pub fn restore_state(&self, s: &VcpuState) -> Result<(), Error> {
        for (r, v) in SAVED_GP.iter().zip(&s.gp) {
            self.write_reg(*r, *v)?;
        }
        for &(r, v) in &s.sysregs {
            self.write_sys_reg(r as u16, v)?;
        }
        // Make the guest's virtual counter CONTINUOUS across the snapshot.
        //
        // At snapshot time vtimer_offset was 0, so CNTVCT == CNTPCT ==
        // mach_absolute_time() == the captured `host_counter`. By restore time the
        // host physical counter (mach) has advanced by the wall-clock gap. If we
        // leave offset 0, CNTVCT jumps forward by that whole gap in one step, so
        // every clock-event deadline the guest had armed expires at once and the
        // kernel re-fires the virtual timer continuously trying to catch up -> a
        // timer storm that pins the vCPU at 100% with no idle WFI ever parking.
        //
        // Setting offset = mach_now - host_counter makes CNTVCT resume at the
        // captured value (CNTVCT = mach - offset = host_counter), so no time appears
        // to pass: the guest's armed CNTV_CVAL is still a near-future deadline, the
        // first idle WFI parks, the timer fires once, and the guest re-arms normally.
        // The WFI exit handler in run() is offset-aware (reads hv_vcpu_get_vtimer_offset
        // and compares CNTV_CVAL against mach - offset), so the host parks correctly
        // in this shifted domain instead of busy-looping on WaitForEventExpired.
        let mach_now = unsafe { mach_absolute_time() };
        let offset = mach_now.saturating_sub(s.host_counter);
        let ret = unsafe { hv_vcpu_set_vtimer_offset(self.vcpuid, offset) };
        if ret != HV_SUCCESS {
            return Err(Error::VcpuSetRegister);
        }
        let ret = unsafe { hv_vcpu_set_vtimer_mask(self.vcpuid, s.vtimer_mask) };
        if ret != HV_SUCCESS {
            return Err(Error::VcpuSetVtimerMask);
        }
        self.write_reg(hv_reg_t_HV_REG_FPCR, s.fpcr)?;
        self.write_reg(hv_reg_t_HV_REG_FPSR, s.fpsr)?;
        for (q, &val) in s.simd.iter().enumerate() {
            let ret = unsafe { hv_vcpu_set_simd_fp_reg(self.vcpuid, q as u32, val) };
            if ret != HV_SUCCESS {
                return Err(Error::VcpuSetRegister);
            }
        }
        // Restore the per-vCPU GIC CPU-interface state last (the GIC + vCPU exist by
        // now). Without this the interface is at reset and the guest takes no IRQs.
        for &(r, v) in &s.icc {
            let ret = unsafe { hv_gic_set_icc_reg(self.vcpuid, r as hv_gic_icc_reg_t, v) };
            if ret != HV_SUCCESS {
                return Err(Error::GicRestore);
            }
        }
        Ok(())
    }

    fn hvf_sync_vtimer(&mut self, vcpu_list: Arc<dyn Vcpus>) {
        if !self.vtimer_masked {
            return;
        }

        let ctl = self
            .read_sys_reg(hv_sys_reg_t_HV_SYS_REG_CNTV_CTL_EL0)
            .unwrap();
        let irq_state = (ctl & (TMR_CTL_ENABLE | TMR_CTL_IMASK | TMR_CTL_ISTATUS))
            == (TMR_CTL_ENABLE | TMR_CTL_ISTATUS);
        vcpu_list.set_vtimer_irq(self.vcpuid);
        if !irq_state {
            vcpu_set_vtimer_mask(self.vcpuid, false).unwrap();
            self.vtimer_masked = false;
        }
    }

    fn handle_psci_request(&self) -> Result<VcpuExit<'_>, Error> {
        match self.read_reg(hv_reg_t_HV_REG_X0)? {
            0x8400_0000 /* QEMU_PSCI_0_2_FN_PSCI_VERSION */ => {
                self.write_reg(hv_reg_t_HV_REG_X0, 2)?;
                Ok(VcpuExit::PsciHandled)
            },
            0x8400_0006 /* QEMU_PSCI_0_2_FN_MIGRATE_INFO_TYPE */ => {
                self.write_reg(hv_reg_t_HV_REG_X0, 2)?;
                Ok(VcpuExit::PsciHandled)
            },
            0x8400_0008 /* QEMU_PSCI_0_2_FN_SYSTEM_OFF */ => {
                Ok(VcpuExit::Shutdown)
            },
            0x8400_0009 /* QEMU_PSCI_0_2_FN_SYSTEM_RESET */ => {
                Ok(VcpuExit::Shutdown)
            },
            0xc400_0003 /* QEMU_PSCI_0_2_FN64_CPU_ON */ => {
                let mpidr = self.read_reg(hv_reg_t_HV_REG_X1)?;
                let entry = self.read_reg(hv_reg_t_HV_REG_X2)?;
                let context_id = self.read_reg(hv_reg_t_HV_REG_X3)?;
                self.write_reg(hv_reg_t_HV_REG_X0, 0)?;
                Ok(VcpuExit::CpuOn(mpidr, entry, context_id))
            }
            val => {
                // Unknown PSCI/HVC function: return NOT_SUPPORTED instead of
                // panicking, so a guest probing CPU_OFF/AFFINITY_INFO/etc. gets a
                // clean error rather than taking down the vCPU thread.
                log::debug!("unhandled PSCI/HVC fn {val:#x} -> NOT_SUPPORTED");
                self.write_reg(hv_reg_t_HV_REG_X0, PSCI_NOT_SUPPORTED)?;
                Ok(VcpuExit::PsciHandled)
            }
        }
    }

    pub fn run(&mut self, vcpu_list: Arc<dyn Vcpus>) -> Result<VcpuExit<'_>, Error> {
        let pending_irq = vcpu_list.has_pending_irq(self.vcpuid);

        if let Some(mmio_read) = self.pending_mmio_read.take()
            && mmio_read.srt < 31
        {
            let val = decode_mmio_le(&self.mmio_buf, mmio_read.len);

            self.write_reg(mmio_read.srt, val)?;
        }

        if self.pending_advance_pc {
            let pc = self.read_reg(hv_reg_t_HV_REG_PC)?;
            self.write_reg(hv_reg_t_HV_REG_PC, pc + 4)?;
            self.pending_advance_pc = false;
        }

        if pending_irq {
            vcpu_set_pending_irq(self.vcpuid, InterruptType::Irq, true)?;
        }

        let ret = unsafe { hv_vcpu_run(self.vcpuid) };
        if ret != HV_SUCCESS {
            return Err(Error::VcpuRun);
        }

        match self.vcpu_exit.reason {
            HV_EXIT_REASON_EXCEPTION => { /* This is the main one, handle below. */ }
            HV_EXIT_REASON_VTIMER_ACTIVATED => {
                self.vtimer_masked = true;
                return Ok(VcpuExit::VtimerActivated);
            }
            HV_EXIT_REASON_CANCELED => return Ok(VcpuExit::Canceled),
            _ => {
                let pc = self.read_reg(hv_reg_t_HV_REG_PC)?;
                panic!(
                    "unexpected exit reason: vcpuid={} 0x{:x} at pc=0x{:x}",
                    self.id(),
                    self.vcpu_exit.reason,
                    pc
                );
            }
        }

        self.hvf_sync_vtimer(vcpu_list.clone());

        let syndrome = self.vcpu_exit.exception.syndrome;
        let ec = (syndrome >> 26) & 0x3f;
        match ec {
            EC_AA64_BKPT => {
                debug!("vcpu[{}]: BRK exit", self.vcpuid);
                Ok(VcpuExit::Breakpoint)
            }
            EC_DATAABORT => {
                let isv: bool = (syndrome & (1 << 24)) != 0;
                let iswrite: bool = ((syndrome >> 6) & 1) != 0;
                let s1ptw: bool = ((syndrome >> 7) & 1) != 0;
                let sas: u32 = ((syndrome >> 22) & 3) as u32;
                let len: usize = (1 << sas) as usize;
                let srt: u32 = ((syndrome >> 16) & 0x1f) as u32;
                let cm: u32 = ((syndrome >> 8) & 0x1) as u32;

                debug!(
                    "EC_DATAABORT {} {} {} {} {} {} {} {}",
                    syndrome, isv as u8, iswrite as u8, s1ptw as u8, sas, len, srt, cm
                );

                let pa = self.vcpu_exit.exception.physical_address;

                // Write-protect dirty fault: a guest store into the tracked RAM
                // window. HVF reports this as a *translation* fault (DFSC 0x07/0x0f),
                // NOT a permission fault, so the only reliable discriminator is a
                // write data abort whose physical address falls inside the RAM
                // region. Re-grant of write permission happens in the caller; we
                // must NOT advance PC here so the trapping store re-executes.
                if self.dirty_tracking
                    && iswrite
                    && pa >= self.ram_base
                    && pa < self.ram_base + self.ram_size
                {
                    return Ok(VcpuExit::DirtyFault(pa));
                }

                self.pending_advance_pc = true;

                if iswrite {
                    let val = if srt < 31 {
                        self.read_reg(hv_reg_t_HV_REG_X0 + srt)?
                    } else {
                        0
                    };

                    encode_mmio_le(&mut self.mmio_buf, val, len);

                    Ok(VcpuExit::MmioWrite(pa, &self.mmio_buf[0..len]))
                } else {
                    self.pending_mmio_read = Some(MmioRead { srt, len });
                    Ok(VcpuExit::MmioRead(pa, &mut self.mmio_buf[0..len]))
                }
            }
            #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
            EC_SYSTEMREGISTERTRAP => {
                let isread: bool = (syndrome & 1) != 0;
                let rt: u32 = ((syndrome >> 5) & 0x1f) as u32;
                let reg: u32 = syndrome as u32 & SYSREG_MASK;
                debug!(
                    "EC_SYSTEMREGISTERTRAP isread={}, syndrome={}, rt={}, reg={}, reg_name={}",
                    isread as u32,
                    syndrome,
                    rt,
                    reg,
                    sys_reg_name(reg).unwrap_or("unknown sysreg")
                );

                self.pending_advance_pc = true;

                if isread {
                    assert!(rt < 32);

                    // See https://developer.arm.com/documentation/dui0801/l/Overview-of-AArch64-state/Registers-in-AArch64-state
                    if rt == 31 {
                        return Ok(VcpuExit::SystemRegister);
                    }

                    match vcpu_list.handle_sysreg_read(self.vcpuid, reg) {
                        Some(val) => {
                            self.write_reg(rt, val)?;
                            Ok(VcpuExit::SystemRegister)
                        }
                        None => panic!(
                            "UNKNOWN rt={}, reg={} name={}",
                            rt,
                            reg,
                            sys_reg_name(reg).unwrap_or("unknown sysreg")
                        ),
                    }
                } else {
                    assert!(rt < 32);

                    // See https://developer.arm.com/documentation/dui0801/l/Overview-of-AArch64-state/Registers-in-AArch64-state
                    let val = if rt == 31 { 0u64 } else { self.read_reg(rt)? };

                    if vcpu_list.handle_sysreg_write(self.vcpuid, reg, val) {
                        Ok(VcpuExit::SystemRegister)
                    } else {
                        panic!(
                            "unexpected write: {} name={}",
                            reg,
                            sys_reg_name(reg).unwrap_or("unknown sysreg")
                        );
                    }
                }
            }
            EC_WFX_TRAP => {
                let ctl = self.read_sys_reg(hv_sys_reg_t_HV_SYS_REG_CNTV_CTL_EL0)?;

                self.pending_advance_pc = true;
                if ((ctl & 1) == 0) || (ctl & 2) != 0 {
                    return Ok(VcpuExit::WaitForEvent);
                }

                // Compare the comparator against the guest's virtual counter, not raw
                // mach time. CNTVCT = CNTPCT - vtimer_offset, and on Apple Silicon
                // CNTPCT == mach_absolute_time(). After a restore the offset is nonzero
                // (set so CNTVCT stays continuous across the snapshot), so using raw
                // mach here would read the comparator as perpetually expired and the
                // host would busy-loop on WaitForEventExpired. On a fresh boot the
                // offset is 0 and this reduces to the original raw-mach comparison.
                let cval = self.read_sys_reg(hv_sys_reg_t_HV_SYS_REG_CNTV_CVAL_EL0)?;
                let mut offset: u64 = 0;
                unsafe { hv_vcpu_get_vtimer_offset(self.vcpuid, &mut offset) };
                let cntvct = unsafe { mach_absolute_time() }.wrapping_sub(offset);
                if cntvct >= cval {
                    return Ok(VcpuExit::WaitForEventExpired);
                }

                let timeout =
                    Duration::from_nanos((cval - cntvct) * (1_000_000_000 / self.cntfrq));
                Ok(VcpuExit::WaitForEventTimeout(timeout))
            }
            EC_AA64_HVC => self.handle_psci_request(),
            EC_AA64_SMC => {
                self.pending_advance_pc = true;
                self.handle_psci_request()
            }
            _ => panic!("unexpected exception: 0x{ec:x}"),
        }
    }
}

#[cfg(test)]
mod mmio_tests {
    use super::{decode_mmio_le, encode_mmio_le};

    #[test]
    fn encode_roundtrips_all_access_sizes() {
        for &len in &[1usize, 2, 4, 8] {
            let mut buf = [0u8; 8];
            let val: u64 = 0x1122_3344_5566_7788;
            encode_mmio_le(&mut buf, val, len);
            let expected = &val.to_le_bytes()[..len];
            assert_eq!(&buf[..len], expected, "encode len={len}");
            let mask = if len == 8 { u64::MAX } else { (1u64 << (len * 8)) - 1 };
            assert_eq!(decode_mmio_le(&buf, len), val & mask, "decode len={len}");
        }
    }

    #[test]
    fn halfword_write_does_not_panic() {
        let mut buf = [0u8; 8];
        encode_mmio_le(&mut buf, 0xBEEF, 2);
        assert_eq!(&buf[..2], &[0xEF, 0xBE]);
        assert_eq!(decode_mmio_le(&buf, 2), 0xBEEF);
    }
}

#[cfg(test)]
mod error_tests {
    use super::Error;

    #[test]
    fn error_is_std_error() {
        fn assert_std_error<E: std::error::Error>() {}
        assert_std_error::<Error>();
    }

    #[test]
    fn gic_set_spi_has_distinct_message() {
        assert_ne!(
            Error::GicSetSpi.to_string(),
            Error::GicCreate.to_string(),
            "GicSetSpi must not reuse the GicCreate message"
        );
        assert_ne!(
            Error::GicSaveState.to_string(),
            Error::GicCreate.to_string(),
            "GicSaveState must not reuse the GicCreate message"
        );
        assert_ne!(
            Error::GicRestore.to_string(),
            Error::GicCreate.to_string(),
            "GicRestore must not reuse the GicCreate message"
        );
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::VcpuState;

    #[test]
    fn vcpu_state_round_trips() {
        let s = VcpuState {
            gp: (0..33).collect(),
            sysregs: vec![(1, 0xaaaa), (2, 0xbbbb)],
            vtimer_mask: true,
            vtimer_offset: 0x1234,
            simd: (0u128..32).map(|i| i * 0x0101_0101_0101_0101_0101_0101_0101_0101).collect(),
            fpcr: 0x00_00_00_00,
            fpsr: 0x0000_0000,
            icc: vec![(1, 0x11), (2, 0x22)],
            host_counter: 0,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: VcpuState = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
        assert_eq!(back.simd.len(), 32);
        assert_eq!(back.fpcr, s.fpcr);
        assert_eq!(back.fpsr, s.fpsr);
    }
}
