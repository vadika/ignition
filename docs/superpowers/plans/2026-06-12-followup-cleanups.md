# Follow-up Cleanups Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the remaining actionable items in `docs/phase1-followups.md`: the unknown-PSCI-fn panic, missing `std::error::Error` impls (`hvf::Error`, `KernelError`), the over-broad `set_spi` error variant, and the `NoIrqVcpus` duplication.

**Architecture:** Four small, independent hardening/hygiene changes grouped into three tasks: (1) PSCI returns NOT_SUPPORTED instead of panicking; (2) error-type hygiene (`std::error::Error` impls + a dedicated `GicSetSpi` variant); (3) hoist `NoIrqVcpus` into the `hvf` crate so both runners share one definition.

**Tech Stack:** Rust, Apple Hypervisor.framework via the `hvf` crate.

---

## Background the engineer needs

- **PSCI panic:** `crates/hvf/src/lib.rs` `handle_psci_request` ends with
  `val => panic!("Unexpected val={val}")`. A guest issuing an unmodeled PSCI/HVC
  call (e.g. `CPU_OFF`, `AFFINITY_INFO`) hard-panics the vCPU thread (and the
  process via panic-on-join). PSCI's NOT_SUPPORTED return value is `-1`
  (`0xffff_ffff_ffff_ffff`), written to X0.
- **`hvf::Error`** (`crates/hvf/src/lib.rs:105`) is a `pub enum` with a `Display`
  impl (ends ~line 154) but no `impl std::error::Error`. Variants include
  `GicCreate`. `set_spi` in `crates/hvf/src/gic.rs:91` returns `Err(Error::GicCreate)`
  on `hv_gic_set_spi` failure — misleading ("creating GIC" for a runtime injection
  failure); `set_spi` is now on the hot IRQ path (serial + virtio).
- **`KernelError`** (`crates/arch/src/aarch64/kernel.rs:18`) is a `pub enum` with a
  `Display` impl but no `impl std::error::Error`.
- **`NoIrqVcpus`** is defined twice — `crates/vmm/src/vstate/hvf_vcpu.rs` (private
  struct) and `crates/vmm/src/vstate/vcpu_manager.rs` (a verbatim copy with a
  "Copied from hvf_vcpu.rs" comment). Both impl `hvf::Vcpus`. The `hvf` crate owns
  the `Vcpus` trait, so the stub belongs there. `hvf_vcpu.rs` does
  `pub use hvf::{HvfVcpu, InterruptType, VcpuExit, Vcpus};`.
- **Build/test:** `cargo test -p ignition-hvf`, `cargo test -p ignition-arch`,
  `cargo build --workspace`, `cargo clippy --workspace`. Re-sign after a build:
  `./scripts/sign.sh target/debug/boot`.
- **Commit policy:** plain messages, NO `Co-Authored-By` / "Generated with Claude".

## File structure

- **Modify `crates/hvf/src/lib.rs`** — PSCI NOT_SUPPORTED (Task 1); `GicSetSpi`
  variant + `impl std::error::Error` + hoisted `NoIrqVcpus` (Tasks 2, 3).
- **Modify `crates/hvf/src/gic.rs`** — `set_spi` uses `GicSetSpi` (Task 2).
- **Modify `crates/arch/src/aarch64/kernel.rs`** — `impl std::error::Error` (Task 2).
- **Modify `crates/vmm/src/vstate/hvf_vcpu.rs` and
  `crates/vmm/src/vstate/vcpu_manager.rs`** — drop the local `NoIrqVcpus`, use
  `hvf::NoIrqVcpus` (Task 3).

---

## Task 1: PSCI unknown-fn returns NOT_SUPPORTED (no panic)

**Files:**
- Modify: `crates/hvf/src/lib.rs`

No unit test (the PSCI path reads/writes vCPU registers via HVF FFI). Verified by
build + an SMP boot smoke.

- [ ] **Step 1: Add the NOT_SUPPORTED constant**

In `crates/hvf/src/lib.rs`, near the other PSCI-related consts (or just above
`handle_psci_request`), add:

```rust
/// PSCI return value for an unrecognized function id (SMCCC: -1 in X0/W0).
const PSCI_NOT_SUPPORTED: u64 = -1_i64 as u64;
```

- [ ] **Step 2: Replace the panic arm**

In `handle_psci_request`, replace:

```rust
            val => panic!("Unexpected val={val}")
```

with:

```rust
            val => {
                // Unknown PSCI/HVC function: return NOT_SUPPORTED instead of
                // panicking, so a guest probing CPU_OFF/AFFINITY_INFO/etc. gets a
                // clean error rather than taking down the vCPU thread.
                log::debug!("unhandled PSCI/HVC fn {val:#x} -> NOT_SUPPORTED");
                self.write_reg(hv_reg_t_HV_REG_X0, PSCI_NOT_SUPPORTED)?;
                Ok(VcpuExit::PsciHandled)
            }
```

- [ ] **Step 3: Build**

Run: `cargo build -p ignition-hvf 2>&1 | tail -3` and
`cargo clippy -p ignition-hvf 2>&1 | grep -c 'warning:'`
Expected: builds, 0 clippy warnings.

- [ ] **Step 4: SMP boot smoke (no regression)**

```bash
cargo build -p hvf-spike --bin boot 2>&1 | tail -1
./scripts/sign.sh target/debug/boot
pkill -9 -f 'target/debug/boot' 2>/dev/null; sleep 1
( sleep 35; printf 'root\n'; sleep 2; printf 'nproc\n'; sleep 2; printf 'poweroff\n'; sleep 6 ) \
  | target/debug/boot --smp 4 kimage/out/Image kimage/out/rootfs.ext4 >/tmp/psci.out 2>/dev/null
grep -i 'SMP: Total of 4' /tmp/psci.out && echo "smp4 OK"
```
Expected: `SMP: Total of 4 processors activated`, `smp4 OK` (still boots; the change
only affects previously-panicking unknown calls).

- [ ] **Step 5: Commit**

```bash
git add crates/hvf/src/lib.rs
git commit -m "fix(hvf): return PSCI NOT_SUPPORTED for unknown fn instead of panicking"
```

---

## Task 2: Error-type hygiene (`std::error::Error` impls + `GicSetSpi`)

**Files:**
- Modify: `crates/hvf/src/lib.rs`
- Modify: `crates/hvf/src/gic.rs`
- Modify: `crates/arch/src/aarch64/kernel.rs`

- [ ] **Step 1: Write the failing tests**

In `crates/hvf/src/lib.rs`, add a test module (or extend the existing `mmio_tests`
— prefer a new `mod error_tests`):

```rust
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
    }
}
```

In `crates/arch/src/aarch64/kernel.rs`, add to its `#[cfg(test)] mod tests` (or a new
one):

```rust
    #[test]
    fn kernel_error_is_std_error() {
        fn assert_std_error<E: std::error::Error>() {}
        assert_std_error::<KernelError>();
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ignition-hvf error_tests 2>&1 | tail -15`
Expected: FAIL — `Error: std::error::Error` not satisfied; `Error::GicSetSpi` not
found.
Run: `cargo test -p ignition-arch kernel_error_is_std_error 2>&1 | tail -10`
Expected: FAIL — `KernelError: std::error::Error` not satisfied.

- [ ] **Step 3: Add the `GicSetSpi` variant + Display arm + Error impls (hvf)**

In `crates/hvf/src/lib.rs`:
- Add a `GicSetSpi` variant to `pub enum Error` (next to `GicCreate`):

```rust
    GicSetSpi,
```

- Add its Display arm in the `impl fmt::Display for Error` match (next to the
  `GicCreate => ...` arm):

```rust
            GicSetSpi => write!(f, "Error setting HVF GIC SPI level"),
```

- Add the std error impl after the Display impl:

```rust
impl std::error::Error for Error {}
```

- [ ] **Step 4: Use `GicSetSpi` in `set_spi`**

In `crates/hvf/src/gic.rs`, in `set_spi` (line 91-ish), change the failure return
from `Err(Error::GicCreate)` to `Err(Error::GicSetSpi)`. (Do NOT change the other
`Error::GicCreate` returns in `HvfGicV3::new` — those are genuinely GIC-create
failures.)

- [ ] **Step 5: Add the `KernelError` std error impl**

In `crates/arch/src/aarch64/kernel.rs`, after the `impl Display for KernelError`
block, add:

```rust
impl std::error::Error for KernelError {}
```

- [ ] **Step 6: Run tests to verify they pass**

Run:
```bash
cargo test -p ignition-hvf 2>&1 | grep 'test result'
cargo test -p ignition-arch 2>&1 | grep 'test result'
cargo build --workspace 2>&1 | tail -1
cargo clippy --workspace 2>&1 | grep -c 'warning:'
```
Expected: hvf tests pass (incl. the 2 new), arch tests pass (incl. the new one),
workspace builds, 0 clippy warnings.

- [ ] **Step 7: Commit**

```bash
git add crates/hvf/src/lib.rs crates/hvf/src/gic.rs crates/arch/src/aarch64/kernel.rs
git commit -m "feat(hvf,arch): impl std::error::Error; split GicSetSpi from GicCreate"
```

---

## Task 3: Hoist `NoIrqVcpus` into the `hvf` crate

**Files:**
- Modify: `crates/hvf/src/lib.rs`
- Modify: `crates/vmm/src/vstate/hvf_vcpu.rs`
- Modify: `crates/vmm/src/vstate/vcpu_manager.rs`

No new test (it's a move + dedup). Verified by build + the existing vmm tests + the
SMP smoke.

- [ ] **Step 1: Define `NoIrqVcpus` in the `hvf` crate**

In `crates/hvf/src/lib.rs`, after the `pub trait Vcpus { ... }` definition, add:

```rust
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
```

- [ ] **Step 2: Use it in `hvf_vcpu.rs`**

In `crates/vmm/src/vstate/hvf_vcpu.rs`:
- Delete the local `struct NoIrqVcpus;` and its `impl Vcpus for NoIrqVcpus { ... }`
  (the block with the doc comment about "Interrupt source with no GIC yet").
- Add `NoIrqVcpus` to the existing re-export/import: change
  `pub use hvf::{HvfVcpu, InterruptType, VcpuExit, Vcpus};` to
  `pub use hvf::{HvfVcpu, InterruptType, NoIrqVcpus, VcpuExit, Vcpus};`.
  (The run loop constructs `Arc::new(NoIrqVcpus)` — it now resolves to the
  re-exported `hvf::NoIrqVcpus`.)

- [ ] **Step 3: Use it in `vcpu_manager.rs`**

In `crates/vmm/src/vstate/vcpu_manager.rs`:
- Delete the local `struct NoIrqVcpus;` + its `impl Vcpus` (the "Copied from
  hvf_vcpu.rs" block).
- Add `NoIrqVcpus` to the hvf import: change
  `use hvf::{HvfVcpu, VcpuExit, Vcpus};` to
  `use hvf::{HvfVcpu, NoIrqVcpus, VcpuExit, Vcpus};`.

- [ ] **Step 4: Build, test, clippy**

Run:
```bash
cargo build --workspace 2>&1 | tail -2
cargo test -p ignition-vmm 2>&1 | grep 'test result'
cargo test -p ignition-hvf 2>&1 | grep 'test result'
cargo clippy --workspace 2>&1 | grep -c 'warning:'
```
Expected: builds, vmm tests pass (3), hvf tests pass, 0 clippy warnings. No more
`NoIrqVcpus` definition outside `hvf` (grep `struct NoIrqVcpus` across `crates/` —
should be exactly one hit, in `crates/hvf/src/lib.rs`).

- [ ] **Step 5: SMP smoke (no regression)**

```bash
cargo build -p hvf-spike --bin boot 2>&1 | tail -1
./scripts/sign.sh target/debug/boot
pkill -9 -f 'target/debug/boot' 2>/dev/null; sleep 1
( sleep 35; printf 'root\n'; sleep 2; printf 'nproc\n'; sleep 2; printf 'poweroff\n'; sleep 6 ) \
  | target/debug/boot --smp 2 kimage/out/Image kimage/out/rootfs.ext4 >/tmp/noirq.out 2>/dev/null
grep -i 'SMP: Total of 2' /tmp/noirq.out && echo "OK"
```
Expected: `SMP: Total of 2 processors activated`, `OK`.

- [ ] **Step 6: Commit**

```bash
git add crates/hvf/src/lib.rs crates/vmm/src/vstate/hvf_vcpu.rs crates/vmm/src/vstate/vcpu_manager.rs
git commit -m "refactor(hvf): hoist NoIrqVcpus into the hvf crate (dedup)"
```

---

## Self-review notes (resolved)

- **Spec coverage:** PSCI NOT_SUPPORTED (Task 1); `std::error::Error` for
  `hvf::Error` + `KernelError` and the `GicSetSpi` split (Task 2); `NoIrqVcpus`
  dedup (Task 3). The remaining `phase1-followups.md` items are deliberately left:
  `hv_gic_config_t` leak ("fine at process scope"), `text_offset` warning ("could
  warn"), CPU hotplug (out of scope), and the intentional `Serial` 1-byte /
  `NoIrqVcpus`-stub constraints.
- **Type consistency:** `PSCI_NOT_SUPPORTED`, `Error::GicSetSpi`, `NoIrqVcpus`
  (now `pub` in `hvf`) used consistently; `set_spi` is the ONLY `GicCreate`->
  `GicSetSpi` change (the create-path returns stay `GicCreate`).
- **No unit test for Tasks 1/3** is intentional (FFI / pure move); covered by build
  + SMP smoke. Task 2's impls are compile-time-checked by the `assert_std_error`
  tests.
- After all tasks, update `docs/phase1-followups.md` to mark these items DONE
  (controller does this in finishing).
```
