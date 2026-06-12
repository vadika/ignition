# Snapshot / restore (single-vCPU, clone) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Snapshot a running single-vCPU guest to a directory (Ctrl-A s) and restore it in a fresh process (`boot --restore <dir>`) — repeatably, so one snapshot clones into N independent microVMs.

**Architecture:** Each stateful component (`HvfVcpu`, `HvfGicV3`, the virtio devices, `Serial`) gains save/restore of a small serde struct. A `vmm::snapshot` module writes/reads the snapshot directory (`vmstate.json` + raw `memory.bin`/`gic.bin`/`disk.img`). Snapshot runs on the vCPU thread at a `Canceled` exit (HVF thread-affinity); restore rebuilds the VM from the artifacts and resumes at the saved PC — no kernel load, no FDT generation.

**Tech Stack:** Rust, Apple Hypervisor.framework (`hv_vcpu_get/set_*`, `hv_gic_state_*`), `serde` + `serde_json`.

---

## Background the engineer needs

- **HVF state APIs (verified):** `hv_vcpu_get/set_reg`, `hv_vcpu_get/set_sys_reg`,
  `hv_vcpu_get/set_vtimer_mask`, `hv_vcpu_get/set_vtimer_offset` (all thread-affine).
  GIC: `hv_gic_state_create() -> hv_gic_state_t`, `hv_gic_state_get_size(state, *mut
  usize)`, `hv_gic_state_get_data(state, *mut c_void)`, `hv_gic_set_state(*const
  c_void, usize)`.
- **`HvfVcpu`** (`crates/hvf/src/lib.rs`) has private `read_reg`/`write_reg`
  (GP regs via `hv_reg_t`) and `read_sys_reg`/`write_sys_reg` (sysregs via
  `hv_sys_reg_t`). `set_initial_state`/`set_secondary_state` show the reg-set idiom.
  GP reg enum: `hv_reg_t_HV_REG_X0..X30`, `HV_REG_FP`, `HV_REG_LR`, `HV_REG_SP`,
  `HV_REG_PC`, `HV_REG_CPSR`. Sysreg enum: `hv_sys_reg_t_HV_SYS_REG_*` (all curated
  names verified present).
- **`HvfGicV3`** (`crates/hvf/src/gic.rs`): `new(vcpu_count, gic_top)`, `fdt_info()`,
  `set_spi`. Stores `dist_base`/`redist_base`/sizes.
- **`Virtqueue`** (`crates/devices/src/virtio/queue.rs`): private `last_avail_idx`,
  `used_idx`; `new(size, desc, driver, device)`. Needs index accessors.
- **`VirtioMmio`** (`crates/devices/src/virtio/mmio.rs`): `QueueState { num, ready,
  desc_lo/hi, driver_lo/hi, device_lo/hi, vq }`, plus `status`, `queue_sel`,
  `interrupt_status`. `set_queue_ready` builds the `Virtqueue` from the addr shadows.
- **`Serial`** (`crates/devices/src/serial.rs`): wraps `vm_superio::Serial`.
  `vm_superio::Serial::state() -> SerialState` (9 register bytes: baud_divisor_low/
  high, interrupt_enable, interrupt_identification, line_control, line_status,
  modem_control, modem_status, scratch — NO FIFO) and `Serial::from_state(trigger,
  &SerialState, out)` (verify the exact `from_state` arg order against
  `~/.cargo/.../vm-superio-0.8.1/src/serial.rs:346`). `SerialState` is not serde —
  mirror it.
- **`VmnetBackend`**: when `--net` is active, snapshot is refused.
- **boot harness** (`spike/src/bin/boot.rs`): arg parser (`--smp`, `--net`,
  unknown-flag guard), the escape FSM (`step`/`EscState`/`Action`, Ctrl-A x quit),
  `spawn_stdin_reader`, `VcpuManager::new(smp, bus).run(entry, fdt_addr)`, the
  `host`/`RAM_SIZE` mmap, `HvfGicV3::new`, FDT generation.
- **`VcpuManager`** (`crates/vmm/src/vstate/vcpu_manager.rs`): `run_loop` matches
  `Canceled => return Ok(())`. The single-vCPU snapshot hooks the `Canceled` arm.
- **Build/test:** `cargo test -p ignition-hvf -p ignition-devices -p ignition-vmm`,
  `cargo build --workspace`, `cargo clippy --workspace`. Re-sign after a build:
  `./scripts/sign.sh target/debug/boot`. The integration needs the entitlement + a
  TTY but NO sudo (non-net), so it is drivable via piped input.
- **Commit policy:** plain messages, NO `Co-Authored-By` / "Generated with Claude".

## File structure

- `crates/hvf/Cargo.toml` + `lib.rs` + `gic.rs` — serde dep; `VcpuState`,
  `HvfVcpu::save_state/restore_state`; `HvfGicV3::save_state` + free `gic_restore`
  (Task 1).
- `crates/devices/Cargo.toml` + `queue.rs`/`mmio.rs`/`serial.rs` — serde dep;
  `Virtqueue` index accessors; `VirtioMmioState`/save/restore; `SerialSnapshot`/
  save/restore (Task 2).
- `crates/vmm/Cargo.toml` + new `crates/vmm/src/snapshot.rs` + `vstate/mod.rs` —
  serde/serde_json; `VmSnapshot`/`write_snapshot`/`read_snapshot` (Task 3).
- `crates/vmm/src/vstate/vcpu_manager.rs` + `spike/src/bin/boot.rs` — Ctrl-A s
  trigger, `Canceled`→snapshot, `--restore`/`--snap-dir`, the restore path,
  integration (Task 4).

---

## Task 1: vCPU + GIC state save/restore (hvf)

**Files:**
- Modify: `crates/hvf/Cargo.toml`, `crates/hvf/src/lib.rs`, `crates/hvf/src/gic.rs`

- [ ] **Step 1: Add serde + the `VcpuState` struct + the sysreg list (write the failing test)**

`crates/hvf/Cargo.toml`: add `serde = { version = "1", features = ["derive"] }`.

In `crates/hvf/src/lib.rs`, add:
```rust
use serde::{Deserialize, Serialize};

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
];

/// GP registers captured: X0..X30, FP, LR, SP, PC, CPSR (33 entries, in this order).
const SAVED_GP: &[hv_reg_t] = &[
    hv_reg_t_HV_REG_X0, hv_reg_t_HV_REG_X1, hv_reg_t_HV_REG_X2, hv_reg_t_HV_REG_X3,
    hv_reg_t_HV_REG_X4, hv_reg_t_HV_REG_X5, hv_reg_t_HV_REG_X6, hv_reg_t_HV_REG_X7,
    hv_reg_t_HV_REG_X8, hv_reg_t_HV_REG_X9, hv_reg_t_HV_REG_X10, hv_reg_t_HV_REG_X11,
    hv_reg_t_HV_REG_X12, hv_reg_t_HV_REG_X13, hv_reg_t_HV_REG_X14, hv_reg_t_HV_REG_X15,
    hv_reg_t_HV_REG_X16, hv_reg_t_HV_REG_X17, hv_reg_t_HV_REG_X18, hv_reg_t_HV_REG_X19,
    hv_reg_t_HV_REG_X20, hv_reg_t_HV_REG_X21, hv_reg_t_HV_REG_X22, hv_reg_t_HV_REG_X23,
    hv_reg_t_HV_REG_X24, hv_reg_t_HV_REG_X25, hv_reg_t_HV_REG_X26, hv_reg_t_HV_REG_X27,
    hv_reg_t_HV_REG_X28, hv_reg_t_HV_REG_X29, hv_reg_t_HV_REG_X30,
    hv_reg_t_HV_REG_PC, hv_reg_t_HV_REG_CPSR,
];

/// Serializable vCPU state for snapshot/restore.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VcpuState {
    /// One value per `SAVED_GP` entry, in order.
    pub gp: Vec<u64>,
    /// (hv_sys_reg_t as u32, value) per `SAVED_SYSREGS` entry.
    pub sysregs: Vec<(u32, u64)>,
    pub vtimer_mask: bool,
    pub vtimer_offset: u64,
}
```
(Confirm the `hv_reg_t_HV_REG_FP`/`LR`/`SP` names: arm64 FP=X29, LR=X30 — HVF may
not expose separate `HV_REG_FP`/`LR`; X29/X30 cover them, and `HV_REG_SP` is the
EL-current SP. The list above uses X0..X30 + PC + CPSR; SP_EL0/SP_EL1 are captured
as sysregs, which is the correct EL1 SP state. If `hv_reg_t_HV_REG_SP` exists and is
needed, add it — but SP_EL0/EL1 sysregs are the authoritative EL1 stack pointers.)

Add a serde round-trip test:
```rust
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
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: VcpuState = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }
}
```
(Add `serde_json` as a `[dev-dependencies]` in `crates/hvf/Cargo.toml` for the test.)

- [ ] **Step 2: Run the test to verify it fails, then passes after the struct compiles**

Run: `cargo test -p ignition-hvf vcpu_state_round_trips 2>&1 | tail -15`. With the
struct + serde added this should compile and pass. (The "failing" state is the
pre-serde compile error.)

- [ ] **Step 3: `HvfVcpu::save_state` / `restore_state`**

In `crates/hvf/src/lib.rs`, add to `impl HvfVcpu` (use the existing private
`read_reg`/`write_reg`/`read_sys_reg`/`write_sys_reg`; if `read_sys_reg`/`write_sys_reg`
aren't both present, add them mirroring `read_reg`/`write_reg`):
```rust
    /// Capture all snapshot state. MUST be called on the vCPU's own thread.
    pub fn save_state(&self) -> Result<VcpuState, Error> {
        let gp = SAVED_GP.iter().map(|&r| self.read_reg(r)).collect::<Result<Vec<_>, _>>()?;
        let sysregs = SAVED_SYSREGS
            .iter()
            .map(|&r| Ok((r, self.read_sys_reg(r as u16)?)))
            .collect::<Result<Vec<(_, _)>, Error>>()?
            .into_iter()
            .map(|(r, v)| (r as u32, v))
            .collect();
        let mut vtimer_mask = false;
        let ret = unsafe { hv_vcpu_get_vtimer_mask(self.vcpuid, &mut vtimer_mask) };
        if ret != HV_SUCCESS { return Err(Error::VcpuReadRegister); }
        let mut vtimer_offset = 0u64;
        let ret = unsafe { hv_vcpu_get_vtimer_offset(self.vcpuid, &mut vtimer_offset) };
        if ret != HV_SUCCESS { return Err(Error::VcpuReadRegister); }
        Ok(VcpuState { gp, sysregs, vtimer_mask, vtimer_offset })
    }

    /// Restore all snapshot state onto a freshly-created vCPU. MUST run on the
    /// vCPU's own thread, before the first `run()`.
    pub fn restore_state(&self, s: &VcpuState) -> Result<(), Error> {
        for (r, v) in SAVED_GP.iter().zip(&s.gp) {
            self.write_reg(*r, *v)?;
        }
        for (r, v) in &s.sysregs {
            self.write_sys_reg(*r as u16, *v)?;
        }
        let ret = unsafe { hv_vcpu_set_vtimer_offset(self.vcpuid, s.vtimer_offset) };
        if ret != HV_SUCCESS { return Err(Error::VcpuSetRegister); }
        let ret = unsafe { hv_vcpu_set_vtimer_mask(self.vcpuid, s.vtimer_mask) };
        if ret != HV_SUCCESS { return Err(Error::VcpuSetVtimerMask); }
        Ok(())
    }
```
(`read_sys_reg`/`write_sys_reg` take a `u16` reg id in this codebase — match the
existing signature. `self.vcpuid` is the field used elsewhere. If a particular
sysreg `set` returns an error on this hardware, it must abort restore — the `?`
does that.)

- [ ] **Step 4: `HvfGicV3` save/restore** (`crates/hvf/src/gic.rs`)

```rust
    /// Capture the in-kernel GIC state as an opaque blob (for snapshot).
    pub fn save_state(&self) -> Result<Vec<u8>, Error> {
        // SAFETY: the state object is created, queried for size, copied out, and
        // dropped within this call.
        unsafe {
            let state = hv_gic_state_create();
            if state.is_null() { return Err(Error::GicCreate); }
            let mut size: usize = 0;
            if hv_gic_state_get_size(state, &mut size) != HV_SUCCESS {
                return Err(Error::GicCreate);
            }
            let mut buf = vec![0u8; size];
            if hv_gic_state_get_data(state, buf.as_mut_ptr() as *mut _) != HV_SUCCESS {
                return Err(Error::GicCreate);
            }
            Ok(buf)
        }
    }
```
And a free function (restore happens before `HvfGicV3::new`/vCPU create — order per
HVF: `hv_vm_create` → `hv_gic_set_state` → vCPU create; confirm against the create
order in `HvfGicV3::new`, which currently does `hv_gic_create`; for restore we use
`hv_gic_set_state` INSTEAD of building config + `hv_gic_create`):
```rust
/// Restore the in-kernel GIC from a snapshot blob. Call after `hv_vm_create` and
/// before any vCPU is created (replaces `HvfGicV3::new`'s create path).
pub fn gic_restore(blob: &[u8]) -> Result<(), Error> {
    let ret = unsafe { hv_gic_set_state(blob.as_ptr() as *const _, blob.len()) };
    if ret != HV_SUCCESS { Err(Error::GicCreate) } else { Ok(()) }
}
```
NOTE for the plan executor: verify whether `hv_gic_set_state` alone reconstructs the
GIC (placement included in the blob) or whether a `HvfGicV3` wrapper is still needed
for `fdt_info()`/`set_spi` after restore. On restore we DON'T regenerate the FDT
(it's in RAM), but the device IRQ lines still call `gic.set_spi`. So restore likely
needs BOTH `gic_restore(blob)` AND a `HvfGicV3` handle for `set_spi`. If
`hv_gic_set_state` cannot coexist with a fresh `hv_gic_create`, the restore path
creates the `HvfGicV3` (which calls `hv_gic_create`) and the design must choose one;
TEST which works. If `set_state` replaces create, expose a `HvfGicV3::from_state(blob,
vcpu_count, gic_top)` that calls `set_state` and fills the placement fields from the
same computation as `new` (so `set_spi`/`fdt_info` still work). Resolve this during
implementation and document the finding.

- [ ] **Step 5: Build + test + clippy**

```bash
cargo test -p ignition-hvf 2>&1 | grep 'test result'
cargo build -p ignition-hvf 2>&1 | tail -1
cargo clippy -p ignition-hvf 2>&1 | grep -c 'warning:'
```
Expected: the round-trip test passes, builds, 0 clippy.

- [ ] **Step 6: Commit**

```bash
git add crates/hvf/Cargo.toml crates/hvf/src/lib.rs crates/hvf/src/gic.rs
git commit -m "feat(hvf): vCPU + GIC state save/restore for snapshots"
```

---

## Task 2: Device state save/restore (devices)

**Files:**
- Modify: `crates/devices/Cargo.toml`, `crates/devices/src/virtio/queue.rs`,
  `crates/devices/src/virtio/mmio.rs`, `crates/devices/src/serial.rs`

- [ ] **Step 1: Write the failing tests**

`crates/devices/Cargo.toml`: add `serde = { version = "1", features = ["derive"] }`.

Add to `crates/devices/src/virtio/mmio.rs` tests:
```rust
    #[test]
    fn virtio_mmio_state_round_trips() {
        let mut backing = vec![0u8; 0x6000];
        let irq = Arc::new(FakeIrq::default());
        let mut d = dev(&mut backing, irq);
        // Program a queue and advance its indices via a real notify (reuse the
        // existing notify test's setup), then snapshot + restore into a fresh dev.
        // Minimal version: set queue 0 ready with addresses, save, restore, compare.
        wr(&mut d, 0x080, 0x1000); wr(&mut d, 0x038, 8); wr(&mut d, 0x044, 1);
        let st = d.save();
        let mut backing2 = vec![0u8; 0x6000];
        let mut d2 = dev(&mut backing2, Arc::new(FakeIrq::default()));
        d2.restore(&st);
        assert_eq!(d2.save(), st); // round-trips
    }
```
Add to `crates/devices/src/serial.rs` tests:
```rust
    #[test]
    fn serial_state_round_trips() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut s = Serial::new(SharedSink(buf.clone()));
        s.write(0, 1, &[0x0f]); // IER = 0x0f (offset 1)
        let st = s.save();
        let mut s2 = Serial::new(SharedSink(buf));
        s2.restore(&st);
        assert_eq!(s2.save(), st);
    }
```

- [ ] **Step 2: Run tests, verify they fail**

Run: `cargo test -p ignition-devices state_round_trips 2>&1 | tail -15`
Expected: FAIL — `save`/`restore`/`VirtioMmioState`/`SerialSnapshot` not found.

- [ ] **Step 3: `Virtqueue` index accessors** (`queue.rs`)

```rust
    /// (last_avail_idx, used_idx) — the consumer/producer positions, for snapshots.
    pub fn indices(&self) -> (u16, u16) {
        (self.last_avail_idx, self.used_idx)
    }
    /// Restore the positions onto a queue rebuilt from the same ring addresses.
    pub fn set_indices(&mut self, last_avail: u16, used: u16) {
        self.last_avail_idx = last_avail;
        self.used_idx = used;
    }
```

- [ ] **Step 4: `VirtioMmioState` + save/restore** (`mmio.rs`)

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueSnapshot {
    pub num: u16, pub ready: u32,
    pub desc_lo: u32, pub desc_hi: u32, pub driver_lo: u32, pub driver_hi: u32,
    pub device_lo: u32, pub device_hi: u32,
    pub last_avail: u16, pub used: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtioMmioState {
    pub status: u32, pub queue_sel: u32, pub device_features_sel: u32,
    pub interrupt_status: u32,
    pub queues: Vec<QueueSnapshot>,
}

impl VirtioMmio {
    pub fn save(&self) -> VirtioMmioState {
        let queues = self.queues.iter().map(|q| {
            let (la, u) = q.vq.as_ref().map_or((0, 0), |vq| vq.indices());
            QueueSnapshot {
                num: q.num, ready: q.ready,
                desc_lo: q.desc_lo, desc_hi: q.desc_hi,
                driver_lo: q.driver_lo, driver_hi: q.driver_hi,
                device_lo: q.device_lo, device_hi: q.device_hi,
                last_avail: la, used: u,
            }
        }).collect();
        VirtioMmioState {
            status: self.status, queue_sel: self.queue_sel,
            device_features_sel: self.device_features_sel,
            interrupt_status: self.interrupt_status, queues,
        }
    }

    pub fn restore(&mut self, s: &VirtioMmioState) {
        self.status = s.status;
        self.queue_sel = s.queue_sel;
        self.device_features_sel = s.device_features_sel;
        self.interrupt_status = s.interrupt_status;
        for (q, snap) in self.queues.iter_mut().zip(&s.queues) {
            q.num = snap.num; q.ready = snap.ready;
            q.desc_lo = snap.desc_lo; q.desc_hi = snap.desc_hi;
            q.driver_lo = snap.driver_lo; q.driver_hi = snap.driver_hi;
            q.device_lo = snap.device_lo; q.device_hi = snap.device_hi;
            if snap.ready != 0 {
                let desc = (u64::from(snap.desc_hi) << 32) | u64::from(snap.desc_lo);
                let driver = (u64::from(snap.driver_hi) << 32) | u64::from(snap.driver_lo);
                let device = (u64::from(snap.device_hi) << 32) | u64::from(snap.device_lo);
                let mut vq = Virtqueue::new(snap.num, desc, driver, device);
                vq.set_indices(snap.last_avail, snap.used);
                q.vq = Some(vq);
            }
        }
    }
}
```
(If `QueueState`'s fields are private to the module, these methods are in the same
module so they have access. The `FakeIrq` test device's IRQ line is rebuilt on
restore by the harness, not serialized.)

- [ ] **Step 5: `Serial` save/restore** (`serial.rs`)

```rust
use serde::{Deserialize, Serialize};

/// Mirror of `vm_superio::serial::SerialState` (registers only; the FIFO is not
/// captured — a few buffered console bytes lost on restore is acceptable).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerialSnapshot {
    pub baud_divisor_low: u8, pub baud_divisor_high: u8,
    pub interrupt_enable: u8, pub interrupt_identification: u8,
    pub line_control: u8, pub line_status: u8,
    pub modem_control: u8, pub modem_status: u8, pub scratch: u8,
}

impl<W: Write + Send> Serial<W> {
    pub fn save(&self) -> SerialSnapshot {
        let s = self.inner.state();
        SerialSnapshot {
            baud_divisor_low: s.baud_divisor_low, baud_divisor_high: s.baud_divisor_high,
            interrupt_enable: s.interrupt_enable,
            interrupt_identification: s.interrupt_identification,
            line_control: s.line_control, line_status: s.line_status,
            modem_control: s.modem_control, modem_status: s.modem_status,
            scratch: s.scratch,
        }
    }
}
```
For restore, the simplest correct path is to REBUILD the inner serial from a
`vm_superio::serial::SerialState` + the trigger + the writer. Add a constructor:
```rust
    pub fn restore(&mut self, snap: &SerialSnapshot) {
        use vm_superio::serial::SerialState as VsState;
        let st = VsState {
            baud_divisor_low: snap.baud_divisor_low,
            baud_divisor_high: snap.baud_divisor_high,
            interrupt_enable: snap.interrupt_enable,
            interrupt_identification: snap.interrupt_identification,
            line_control: snap.line_control,
            line_status: snap.line_status,
            modem_control: snap.modem_control,
            modem_status: snap.modem_status,
            scratch: snap.scratch,
            ..Default::default() // in case SerialState has private/extra fields
        };
        // Rebuild inner with the same trigger + a fresh writer is not possible
        // (we don't own the writer here); instead use vm_superio's setter if it
        // exposes one. If `vm_superio::Serial` has no in-place state setter, store
        // the SerialSnapshot and apply it by reconstructing inner in `with_irq`/`new`
        // at restore-build time (the harness builds the Serial fresh on restore and
        // passes the snapshot to a `with_irq_and_state` constructor).
        let _ = st; // see note
    }
```
RESOLUTION (do this): `vm_superio::Serial` is built fresh on restore anyway (the
harness constructs all devices). So instead of an in-place `restore`, add a
**constructor** `Serial::from_snapshot(out, irq, &SerialSnapshot)` that builds the
inner via `vm_superio::Serial::from_state(trigger, &vs_state, out)` (check
`from_state`'s exact signature at `vm-superio-0.8.1/src/serial.rs:346`). Keep the
`save()` getter as above. Update the test to construct via `from_snapshot` rather
than `restore` if an in-place setter isn't available. Pick whichever vm_superio
supports and adjust the test accordingly.

- [ ] **Step 6: Run tests + clippy**

```bash
cargo test -p ignition-devices 2>&1 | grep 'test result'
cargo clippy -p ignition-devices 2>&1 | grep -c 'warning:'
```
Expected: device tests pass (incl. the 2 new round-trips, adapted to the actual
serial constructor), 0 clippy.

- [ ] **Step 7: Commit**

```bash
git add crates/devices/Cargo.toml crates/devices/src/virtio/queue.rs crates/devices/src/virtio/mmio.rs crates/devices/src/serial.rs
git commit -m "feat(devices): virtio-mmio + serial state save/restore"
```

---

## Task 3: Snapshot module — write/read the directory (vmm)

**Files:**
- Modify: `crates/vmm/Cargo.toml`, `crates/vmm/src/vstate/mod.rs` (or `lib.rs`)
- Create: `crates/vmm/src/snapshot.rs`

- [ ] **Step 1: Deps + module + the failing test**

`crates/vmm/Cargo.toml`: add `serde = { version = "1", features = ["derive"] }`,
`serde_json = "1"`. Register the module: in `crates/vmm/src/lib.rs` add `pub mod
snapshot;`.

- [ ] **Step 2: Implement `crates/vmm/src/snapshot.rs`**

```rust
//! Snapshot directory I/O: a JSON state file plus raw memory/gic/disk artifacts.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use devices::serial::SerialSnapshot;
use devices::virtio::mmio::VirtioMmioState;
use hvf::VcpuState;

pub const SNAP_MAGIC: &str = "ignition-snapshot-v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MmioWindow { pub base: u64, pub size: u64, pub spi: u32 }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmConfig {
    pub mem_size: u64,
    pub vcpu_count: u64,
    pub serial: MmioWindow,
    pub blk: MmioWindow,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceState {
    pub blk: VirtioMmioState,
    pub serial: SerialSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmSnapshot {
    pub magic: String,
    pub config: VmConfig,
    pub vcpu: VcpuState,
    pub devices: DeviceState,
}

impl VmSnapshot {
    pub fn new(config: VmConfig, vcpu: VcpuState, devices: DeviceState) -> Self {
        Self { magic: SNAP_MAGIC.to_string(), config, vcpu, devices }
    }
}

pub struct Paths { pub memory: PathBuf, pub gic: PathBuf, pub disk: PathBuf, pub state: PathBuf }

pub fn paths(dir: &Path) -> Paths {
    Paths {
        memory: dir.join("memory.bin"),
        gic: dir.join("gic.bin"),
        disk: dir.join("disk.img"),
        state: dir.join("vmstate.json"),
    }
}

/// Write the full snapshot. `ram` is the guest RAM slice; `gic_blob` the GIC state;
/// `disk_src` the live rootfs path (copied into the snapshot).
pub fn write_snapshot(
    dir: &Path,
    snap: &VmSnapshot,
    ram: &[u8],
    gic_blob: &[u8],
    disk_src: &Path,
) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let p = paths(dir);
    fs::File::create(&p.memory)?.write_all(ram)?;
    fs::File::create(&p.gic)?.write_all(gic_blob)?;
    fs::copy(disk_src, &p.disk)?;
    let json = serde_json::to_vec_pretty(snap).map_err(io::Error::other)?;
    fs::write(&p.state, json)?;
    Ok(())
}

/// Read + validate a snapshot's metadata (the raw artifacts are loaded by the
/// caller, which owns the mmap/disk lifetimes).
pub fn read_snapshot(dir: &Path) -> io::Result<(VmSnapshot, Vec<u8>, Paths)> {
    let p = paths(dir);
    let snap: VmSnapshot =
        serde_json::from_slice(&fs::read(&p.state)?).map_err(io::Error::other)?;
    if snap.magic != SNAP_MAGIC {
        return Err(io::Error::other(format!("bad snapshot magic: {}", snap.magic)));
    }
    let gic = fs::read(&p.gic)?;
    Ok((snap, gic, p))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> VmSnapshot {
        VmSnapshot::new(
            VmConfig {
                mem_size: 0x2000_0000, vcpu_count: 1,
                serial: MmioWindow { base: 0x0900_0000, size: 0x1000, spi: 0 },
                blk: MmioWindow { base: 0x0a00_0000, size: 0x200, spi: 1 },
            },
            VcpuState { gp: (0..33).collect(), sysregs: vec![(1, 2)], vtimer_mask: false, vtimer_offset: 0 },
            DeviceState {
                blk: VirtioMmioState { status: 0xf, queue_sel: 0, device_features_sel: 0, interrupt_status: 0, queues: vec![] },
                serial: SerialSnapshot { baud_divisor_low: 1, baud_divisor_high: 0, interrupt_enable: 0xf, interrupt_identification: 1, line_control: 3, line_status: 0x60, modem_control: 0, modem_status: 0, scratch: 0 },
            },
        )
    }

    #[test]
    fn snapshot_json_round_trips() {
        let s = sample();
        let json = serde_json::to_string(&s).unwrap();
        let back: VmSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn write_then_read_validates_magic() {
        let dir = tempfile::tempdir().unwrap();
        let disk = dir.path().join("src.img");
        std::fs::write(&disk, b"DISK").unwrap();
        write_snapshot(dir.path(), &sample(), &[0u8; 16], &[1u8, 2, 3], &disk).unwrap();
        let (snap, gic, p) = read_snapshot(dir.path()).unwrap();
        assert_eq!(snap, sample());
        assert_eq!(gic, vec![1, 2, 3]);
        assert_eq!(std::fs::read(&p.memory).unwrap(), vec![0u8; 16]);
        assert_eq!(std::fs::read(&p.disk).unwrap(), b"DISK");
    }
}
```
(`tempfile` is already a dev-dependency in the workspace — add it to `crates/vmm`'s
`[dev-dependencies]` if not present.)

- [ ] **Step 3: Run tests + clippy**

```bash
cargo test -p ignition-vmm snapshot 2>&1 | grep 'test result'
cargo test -p ignition-vmm 2>&1 | grep 'test result'
cargo clippy -p ignition-vmm 2>&1 | grep -c 'warning:'
```
Expected: the 2 snapshot tests pass, all vmm tests pass, 0 clippy.

- [ ] **Step 4: Commit**

```bash
git add crates/vmm/Cargo.toml crates/vmm/src/lib.rs crates/vmm/src/snapshot.rs
git commit -m "feat(vmm): snapshot directory model + write/read"
```

---

## Task 4: Orchestration — Ctrl-A s trigger + `--restore` + integration

**Files:**
- Modify: `spike/src/bin/boot.rs`
- Modify: `crates/vmm/src/vstate/vcpu_manager.rs` (expose the single-vCPU snapshot hook)

This task wires the live snapshot trigger and the restore path. The vCPU thread is
the only place state can be captured, so the single-vCPU `VcpuManager` gets a
snapshot callback invoked on the `Canceled` exit.

- [ ] **Step 1: Snapshot hook in `VcpuManager` (single-vCPU)**

Add a snapshot request flag + a callback to `VcpuManager`. The simplest, lowest-risk
shape for v1: store an `Option<Box<dyn Fn(&HvfVcpu) -> bool + Send + Sync>>` snapshot
handler and an `AtomicBool snapshot_req`. In `run_loop`, change the `Canceled` arm:
```rust
                VcpuExit::Canceled => {
                    if self.snapshot_req.swap(false, Ordering::AcqRel) {
                        if let Some(h) = &self.snapshot_handler {
                            // Handler reads vcpu state + writes the snapshot dir.
                            h(&vcpu);
                        }
                        continue; // resume the guest after snapshotting
                    }
                    return Ok(());
                }
```
Add `pub fn request_snapshot(&self)` (sets the flag) and a constructor/setter to
install the handler. The harness installs a handler that calls `vcpu.save_state()`,
`gic.save_state()`, gathers device state (via `Arc<Mutex<…>>` clones of the serial +
blk devices it already holds), and calls `snapshot::write_snapshot`. The reader
thread calls `manager.request_snapshot()` + `vcpu_request_exit(primary_vcpuid)`.

Because the handler needs the GIC + device Arcs + the RAM slice + the disk path,
build it as a closure in `boot.rs` capturing those, and pass it to the manager
before `run`. Keep the manager generic over `Fn(&HvfVcpu)`; do not pull
device/snapshot types into the vmm manager beyond the `hvf::HvfVcpu` it already uses
— the closure (defined in boot.rs) does the snapshot composition.

If threading a closure through `VcpuManager` is awkward, an acceptable alternative
for v1: the manager exposes the `Canceled`+`snapshot_req` hook by RETURNING a new
`VcpuExit`-like signal to the harness — but the harness doesn't own the run loop.
Prefer the closure. Resolve the exact wiring during implementation; keep the
SMP/shutdown behavior unchanged (snapshot is single-vCPU only — assert vcpu_count==1
when a handler is installed).

- [ ] **Step 2: Ctrl-A s in the escape FSM** (`boot.rs`)

Extend the `step` FSM: `SawCtrlA + 's'` → a new `Action::Snapshot`. Update the
reader thread to call `manager.request_snapshot()` on `Action::Snapshot`. Add a unit
test `ctrl_a_then_s_snapshots` (mirrors `ctrl_a_then_x_quits`). The reader thread
needs a handle to the manager (or a shared `snapshot_req` flag + the primary
vcpuid); pass an `Arc` or a flag the way `saved_termios` is passed.

- [ ] **Step 3: `--snap-dir` + `--restore` args** (`boot.rs`)

Extend the parser:
- `--snap-dir <path>` (default `./snapshot`) — where Ctrl-A s writes.
- `--restore <dir>` — restore mode: skip kernel load / FDT generation; instead load
  the snapshot.

- [ ] **Step 4: The restore path** (`boot.rs`)

A `fn run_restore(dir)`:
```text
let (snap, gic_blob, paths) = snapshot::read_snapshot(dir)?;
mmap RAM_SIZE; read paths.memory into the RAM slice (assert len == snap.config.mem_size);
let vm = Vm::new(false)?;
hvf::gic_restore(&gic_blob)  (OR HvfGicV3::from_state — per Task 1 Step 4 finding);
build a HvfGicV3 handle for set_spi/IRQ lines (placement from the same constants);
vm.map_memory(host, RAM_BASE, RAM_SIZE)?;
copy paths.disk -> a private instance disk path (e.g. <dir>/instance-<pid>.img or a temp);
build the bus: Serial::from_snapshot(...) at snap.config.serial, VirtioMmio for blk
  (VirtioBlk over the private disk copy) then virtio_mmio.restore(&snap.devices.blk)
  at snap.config.blk;
set up the serial reader thread + TermiosGuard (interactive console);
create the vCPU: HvfVcpu::new(0) then vcpu.restore_state(&snap.vcpu) (NOT
  set_initial_state — PC/regs come from the snapshot);
spawn the vCPU thread to run the loop from the restored PC (reuse VcpuManager or a
  direct run loop; for restore the guest is already booted, so just run the loop).
```
KEY: restore does NOT call `load_kernel`, `fdt::generate`, or
`set_initial_state` — the kernel, DTB, and all CPU state are already captured. The
GIC IRQ lines (serial SPI 32, blk SPI 33) must point at the restored GIC so device
interrupts still deliver.

- [ ] **Step 5: Build + sign + unit tests**

```bash
cargo build -p hvf-spike --bin boot 2>&1 | tail -3
cargo test -p hvf-spike --bin boot 2>&1 | grep 'test result'
cargo clippy --workspace 2>&1 | grep -c 'warning:'
./scripts/sign.sh target/debug/boot
```
Expected: builds, the boot FSM tests pass (incl. `ctrl_a_then_s_snapshots`), 0
clippy, signed.

- [ ] **Step 6: Integration (the bar) — drivable via piped input, NO sudo**

```bash
rm -rf ./snapshot
pkill -9 -f 'target/debug/boot' 2>/dev/null; sleep 1
# Boot, log in, write a marker, then Ctrl-A s (0x01 0x73) to snapshot, then keep running a moment, then Ctrl-A x (0x01 0x78) to quit.
( sleep 35; printf 'root\n'; sleep 3; printf 'echo CLONE_ME > /tmp/marker; sync\n'; sleep 3; printf '\001s'; sleep 5; printf '\001x' ) \
  | target/debug/boot kimage/out/Image kimage/out/rootfs.ext4 >/tmp/snap.out 2>/tmp/snap.err
echo "snapshot dir written: $(ls -la ./snapshot 2>/dev/null | grep -c vmstate.json)"
ls -la ./snapshot
# Restore in a fresh process and check the marker survived.
./scripts/sign.sh target/debug/boot
( sleep 8; printf 'cat /tmp/marker\n'; sleep 3; printf 'uname -a\n'; sleep 2; printf '\001x' ) \
  | target/debug/boot --restore ./snapshot >/tmp/restore.out 2>/tmp/restore.err
echo "=== restore resumed + marker survived? ==="; grep -c 'CLONE_ME' /tmp/restore.out
# Clone: a second independent restore.
( sleep 8; printf 'cat /tmp/marker; echo CLONE2 >> /tmp/marker; cat /tmp/marker\n'; sleep 3; printf '\001x' ) \
  | target/debug/boot --restore ./snapshot >/tmp/restore2.out 2>/dev/null
echo "=== clone independent? (restore2 has CLONE_ME; original snapshot unchanged) ==="; grep -c 'CLONE_ME' /tmp/restore2.out
```
Expected: `./snapshot/vmstate.json` (+ memory.bin/gic.bin/disk.img) written; the
restore boot resumes WITHOUT re-booting (no kernel banner — straight to a responsive
shell) and `cat /tmp/marker` prints `CLONE_ME`; the second restore is independent.
NOTE: the restore appears as a near-instant resume (the guest was mid-shell), so the
`sleep 8` before typing is just settling time, not a full boot. If restore shows a
fresh kernel boot instead of a resume, the restore path wrongly re-initialized the
vCPU — investigate `restore_state` vs `set_initial_state`. If the restored guest
hangs, suspect the GIC restore (Task 1 Step 4 finding) or a missing sysreg.

- [ ] **Step 7: Commit**

```bash
git add spike/src/bin/boot.rs crates/vmm/src/vstate/vcpu_manager.rs
git commit -m "feat(boot): Ctrl-A s snapshot + --restore (resume & clone)"
```

---

## Self-review notes (resolved)

- **Spec coverage:** vCPU+GIC state (Task 1), device state (Task 2), snapshot dir
  I/O (Task 3), trigger + restore + clone integration (Task 4). Single-vCPU,
  no-net, full-RAM-dump per the spec.
- **Type consistency:** `VcpuState` (hvf), `VirtioMmioState`/`QueueSnapshot`/
  `SerialSnapshot` (devices), `VmSnapshot`/`VmConfig`/`DeviceState`/`MmioWindow`
  (vmm) — referenced consistently across crates; `save()`/`restore()`/`save_state()`/
  `restore_state()`/`gic_restore` naming consistent.
- **Two open implementation findings to resolve + document:** (1) the GIC restore
  shape (`hv_gic_set_state` alone vs. needing a `HvfGicV3` handle for `set_spi`) —
  TEST which works (Task 1 Step 4). (2) the serial restore constructor
  (`from_state` arg order; in-place setter vs. `from_snapshot` constructor) — pick
  what `vm_superio` supports (Task 2 Step 5). Both are flagged in-task.
- **No unit tests for the FFI state get/set or the live snapshot/restore** is
  intentional (HVF + a running guest); covered by the integration bar. The serde
  round-trips + device save/restore + snapshot dir I/O ARE unit-tested.
- After all tasks, write `docs/snapshot-restore-result.md` (controller, in finishing).
```
