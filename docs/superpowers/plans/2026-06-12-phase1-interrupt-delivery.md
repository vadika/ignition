# Phase 1 Milestone 2f: interrupt delivery → shell Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans (this milestone is experiment-led; Tasks A and D are live-boot experiments run by the controller in the main session, not subagent-able). Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver the virtual-timer interrupt to the guest and replace the bounded-sleep WFI with channel-based parking, so OpenRC finishes and a login/shell prompt appears on stdout.

**Architecture:** Discover the in-kernel-GIC vtimer delivery path by live experiment (Task A), land it in `hvf` (Task B), then replace `NoIrqVcpus`/`MAX_PARK` with a `GicVcpus` interrupt source + per-vCPU `std::sync::mpsc` channel parking (Task C). The boot run is the gate (Task D).

**Tech Stack:** Rust edition 2024, Apple Hypervisor.framework (`hv_vcpu_set_vtimer_mask`, `hv_vcpu_set_pending_interrupt`, in-kernel `hv_gic`), `std::sync::mpsc`.

**Commit convention:** plain commit messages, NO co-author/"Generated with Claude" trailer.

**Execution note:** Tasks A and D require the live boot (HVF entitlement + the real kernel/rootfs) and are inherently iterative — the controller runs them in the main session. Tasks B and C are mechanical once A's result is known.

---

## File Structure

- `crates/hvf/src/lib.rs` — the discovered vtimer-delivery change in `hvf_sync_vtimer`/`run` (Task B)
- `crates/vmm/src/vstate/gic_vcpus.rs` — **create**: `GicVcpus` interrupt source (Task C)
- `crates/vmm/src/vstate/mod.rs` — declare the module (Task C)
- `crates/vmm/src/vstate/hvf_vcpu.rs` — channel parking + use `GicVcpus` (Task C)
- `docs/phase1-followups.md` / `docs/2f-*.md` — document the libkrun divergence + result

---

## Task A: vtimer delivery experiment (controller, in-session)

Goal: find the minimal `hvf` change that makes the in-kernel GIC deliver the EL1
virtual-timer PPI, so userspace timed waits progress. Current `hvf_sync_vtimer`
(lib.rs) only unmasks when the timer condition has cleared — wrong for in-kernel
GIC where nothing else delivers the PPI.

- [ ] **Step 1: Baseline** — confirm the stall. `cargo build -p hvf-spike --bin boot && scripts/sign.sh target/debug/boot && (target/debug/boot kimage/out/Image kimage/out/rootfs.ext4 >/tmp/a.out 2>/dev/null & p=$!; sleep 30; kill $p)`. Expected: `tail /tmp/a.out` ends at `OpenRC 0.52.1` (no service messages).

- [ ] **Step 2: Candidate 1 — unmask unconditionally.** In `crates/hvf/src/lib.rs` `hvf_sync_vtimer`, change the unmask to be unconditional (remove the `if !irq_state` gate):

```rust
    fn hvf_sync_vtimer(&mut self, vcpu_list: Arc<dyn Vcpus>) {
        if !self.vtimer_masked {
            return;
        }
        vcpu_list.set_vtimer_irq(self.vcpuid);
        // In-kernel GIC: unmask so the GIC delivers PPI 27 (EL1 virtual timer).
        // (libkrun's userspace-GIC path gates this on the timer condition; that
        // path never delivers under an in-kernel GIC.)
        vcpu_set_vtimer_mask(self.vcpuid, false).unwrap();
        self.vtimer_masked = false;
    }
```

Rebuild, sign, run as Step 1. Success: `/tmp/a.out` shows OpenRC service-start
lines (e.g. `* Mounting …`, `* Starting …`) past the banner → the timer tick
fires. If it boots further or to a prompt, record and proceed to Task B with this
change.

- [ ] **Step 3: Candidate 2 (only if Step 2 fails) — raw line assert.** Revert Step 2. In `lib.rs`, after the vtimer exit handling, also assert the IRQ line so the GIC presents the vtimer INTID: in `hvf_sync_vtimer` call
`vcpu_set_pending_irq(self.vcpuid, InterruptType::Irq, true)?;` (make the fn return Result or `let _ =`), in addition to unmasking. Rebuild/run as Step 1; check for service-start lines.

- [ ] **Step 4 (only if Steps 2–3 fail) — masking-window tweak.** Keep masked for the current exit but unmask on the NEXT entry regardless of condition (one-shot delay), to avoid an exit storm while still delivering. Iterate. If none work, STOP and report — deeper `hv_gic` header investigation is needed (read `hv_gic.h`/`hv_vcpu.h` in the SDK).

- [ ] **Step 5: Record the winning change** verbatim (the exact `hvf_sync_vtimer` body and any `set_vtimer_irq` requirement) for Task B/C. Leave the change in place (Task B finalizes + commits it).

---

## Task B: land the vtimer fix in `hvf`

**Files:** Modify `crates/hvf/src/lib.rs`; Modify `docs/phase1-followups.md`.

- [ ] **Step 1:** Ensure `crates/hvf/src/lib.rs` contains exactly the winning change from Task A (the `hvf_sync_vtimer` body), with a comment marking it as a deliberate divergence from the verbatim libkrun lift for the in-kernel-GIC path.

- [ ] **Step 2:** Add a note to `docs/phase1-followups.md` under a "2f" heading: which candidate won, why libkrun's logic doesn't apply (userspace vs in-kernel GIC), and that `hvf` now diverges here.

- [ ] **Step 3: Build-check** `cargo build -p ignition-hvf 2>&1 | tail -3` → `Finished`. Confirm `uart-echo` + `gic-smoke` still build: `cargo build -p hvf-spike 2>&1 | tail -3`.

- [ ] **Step 4: Commit (plain message, no trailer):**
```bash
git add crates/hvf/src/lib.rs docs/phase1-followups.md
git commit -m "fix(hvf): deliver the vtimer PPI under the in-kernel GIC

hvf_sync_vtimer no longer gates the vtimer unmask on the timer condition
clearing (libkrun's userspace-GIC assumption); the in-kernel GIC delivers
PPI 27 once unmasked. Documented divergence from the verbatim libkrun lift."
```

---

## Task C: GicVcpus + channel-based WFI parking

**Files:**
- Create: `crates/vmm/src/vstate/gic_vcpus.rs`
- Modify: `crates/vmm/src/vstate/mod.rs`
- Modify: `crates/vmm/src/vstate/hvf_vcpu.rs`

- [ ] **Step 1: declare the module.** In `crates/vmm/src/vstate/mod.rs`, add `pub mod gic_vcpus;`.

- [ ] **Step 2: create `crates/vmm/src/vstate/gic_vcpus.rs`:**

```rust
//! Interrupt source for the in-kernel-GIC vCPU run loop.
//!
//! Replaces the earlier `NoIrqVcpus`. The in-kernel GIC drives SPI lines and
//! answers the ICC system registers itself, so this impl is mostly inert; its
//! job is (a) the vtimer handoff and (b) holding the per-vCPU wake channel the
//! run loop parks on, so an interrupt source can wake a parked vCPU.

use std::sync::Mutex;
use std::sync::mpsc::Sender;

use hvf::Vcpus;

/// Spurious INTID returned when the guest reads a pending IRQ we don't track
/// (the in-kernel GIC owns real INTIDs).
const GIC_INTID_SPURIOUS: u32 = 1023;

#[derive(Default)]
struct PerCpu {
    waker: Option<Sender<()>>,
}

pub struct GicVcpus {
    cpus: Vec<Mutex<PerCpu>>,
}

impl GicVcpus {
    pub fn new(vcpu_count: u64) -> Self {
        let mut cpus = Vec::with_capacity(vcpu_count as usize);
        for _ in 0..vcpu_count {
            cpus.push(Mutex::new(PerCpu::default()));
        }
        Self { cpus }
    }

    /// Register the run loop's wake channel for `vcpuid`.
    pub fn register(&self, vcpuid: u64, waker: Sender<()>) {
        self.cpus[vcpuid as usize].lock().unwrap().waker = Some(waker);
    }

    fn wake(&self, vcpuid: u64) {
        if let Some(tx) = &self.cpus[vcpuid as usize].lock().unwrap().waker {
            let _ = tx.send(());
        }
    }
}

impl Vcpus for GicVcpus {
    fn set_vtimer_irq(&self, vcpuid: u64) {
        // The vtimer is delivered by the in-kernel GIC once hvf unmasks it
        // (see hvf::hvf_sync_vtimer). Here we just wake a parked vCPU so it
        // re-enters and takes the interrupt.
        self.wake(vcpuid);
    }
    fn should_wait(&self, _vcpuid: u64) -> bool {
        true
    }
    fn has_pending_irq(&self, _vcpuid: u64) -> bool {
        // The in-kernel GIC drives the IRQ line for SPIs; do not double-inject.
        false
    }
    fn get_pending_irq(&self, _vcpuid: u64) -> u32 {
        GIC_INTID_SPURIOUS
    }
    fn handle_sysreg_read(&self, _vcpuid: u64, _reg: u32) -> Option<u64> {
        Some(0)
    }
    fn handle_sysreg_write(&self, _vcpuid: u64, _reg: u32, _val: u64) -> bool {
        true
    }
}
```

(If Task A determined `set_vtimer_irq` must do more than wake, encode that here
per the recorded result.)

- [ ] **Step 3: rework the run loop** in `crates/vmm/src/vstate/hvf_vcpu.rs`. Replace the imports + the `NoIrqVcpus` struct/impl + the `MAX_PARK` const with channel parking using `GicVcpus`. Specifically:

Change the imports block to:
```rust
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use devices::bus::Bus;

pub use hvf::{HvfVcpu, InterruptType, VcpuExit, Vcpus};

use super::gic_vcpus::GicVcpus;
```
Delete the `MAX_PARK` const and the entire `NoIrqVcpus` struct + its `impl Vcpus`.

Replace the body of `fn run(self)` with:
```rust
    fn run(self) -> Result<(), hvf::Error> {
        // Per-vCPU wake channel: the run loop parks on `rx`; the interrupt
        // source (GicVcpus) holds `tx` and sends to wake a parked vCPU.
        let (tx, rx) = mpsc::channel::<()>();
        let gic_vcpus = Arc::new(GicVcpus::new(1));
        gic_vcpus.register(0, tx);
        let vcpus: Arc<dyn Vcpus> = gic_vcpus;

        // Thread-affine: create the vCPU here, not in `new`.
        let mut vcpu = HvfVcpu::new(self.mpidr, false)?;
        vcpu.set_initial_state(self.entry, self.fdt_addr)?;

        loop {
            let exit = vcpu.run(vcpus.clone())?;
            match exit {
                VcpuExit::MmioWrite(addr, data) => self.bus.write(addr, data),
                VcpuExit::MmioRead(addr, data) => self.bus.read(addr, data),
                VcpuExit::Shutdown => {
                    log::info!("guest requested shutdown (PSCI SYSTEM_OFF)");
                    return Ok(());
                }
                VcpuExit::Canceled => return Ok(()),
                // WFI with a timer deadline: park until it elapses, then re-enter
                // (the vtimer fires on re-entry). A wake token short-circuits.
                VcpuExit::WaitForEventTimeout(d) => {
                    drain(&rx);
                    let _ = rx.recv_timeout(d);
                }
                // WFI with no deadline: block until an interrupt source wakes us.
                VcpuExit::WaitForEvent => {
                    drain(&rx);
                    let _ = rx.recv();
                }
                VcpuExit::WaitForEventExpired | VcpuExit::VtimerActivated => {}
                other => log::debug!("unhandled vCPU exit: {other:?}"),
            }
        }
    }
```
And add this free function above `impl Vcpu` (or below the struct):
```rust
/// Discard any buffered wake tokens so a stale token doesn't short-circuit the
/// next genuine park.
fn drain(rx: &mpsc::Receiver<()>) {
    while rx.try_recv().is_ok() {}
}
```

- [ ] **Step 4: parking unit test.** Add to `crates/vmm/src/vstate/hvf_vcpu.rs` a `#[cfg(test)] mod tests` exercising the `drain` + park-timeout primitive without HVF:
```rust
#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::drain;

    #[test]
    fn recv_timeout_returns_after_timeout_when_no_token() {
        let (_tx, rx) = mpsc::channel::<()>();
        let start = std::time::Instant::now();
        assert!(rx.recv_timeout(Duration::from_millis(20)).is_err());
        assert!(start.elapsed() >= Duration::from_millis(20));
    }

    #[test]
    fn drain_clears_buffered_tokens_then_token_wakes() {
        let (tx, rx) = mpsc::channel::<()>();
        tx.send(()).unwrap();
        tx.send(()).unwrap();
        drain(&rx); // clears the two stale tokens
        assert!(rx.recv_timeout(Duration::from_millis(5)).is_err());
        tx.send(()).unwrap(); // a fresh token wakes immediately
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_ok());
    }
}
```

- [ ] **Step 5: build + test.** `cargo build --workspace 2>&1 | tail -3 && cargo test -p ignition-vmm 2>&1 | grep 'test result' && cargo clippy -p ignition-vmm 2>&1 | tail -5`. Expected: Finished, `2 passed`, no clippy warnings.

- [ ] **Step 6: commit (plain message, no trailer):**
```bash
git add crates/vmm/src/vstate/gic_vcpus.rs crates/vmm/src/vstate/mod.rs crates/vmm/src/vstate/hvf_vcpu.rs
git commit -m "feat(vmm): GicVcpus + channel-based WFI parking

Replace NoIrqVcpus/MAX_PARK with a GicVcpus interrupt source holding a
per-vCPU mpsc wake channel; the run loop parks on recv()/recv_timeout()
(wakes at the CNTV deadline or on an injected IRQ) instead of busy-sleeping."
```

---

## Task D: boot run → shell prompt (controller, in-session)

- [ ] **Step 1:** `cargo build -p hvf-spike --bin boot && scripts/sign.sh target/debug/boot`.
- [ ] **Step 2:** `(target/debug/boot kimage/out/Image kimage/out/rootfs.ext4 >/tmp/shell.out 2>/dev/null & p=$!; sleep 60; kill $p)`.
- [ ] **Step 3:** `grep -iE 'Starting|login:|/ #|Welcome|localhost' /tmp/shell.out; tail -20 /tmp/shell.out`. Expected: OpenRC service-start messages and a `login:` or shell prompt. If it stalls at a NEW point (a second missing piece), diagnose iteratively (CPU%, MMIO trace) as in 2e.
- [ ] **Step 4:** Write `docs/2f-result.md` recording the outcome (prompt reached, or the next blocker) and commit it.

---

## Self-Review

**Spec coverage:**
- vtimer delivery (in-kernel GIC), experiment-driven, candidates ordered → Task A ✓
- Land the fix in hvf + documented divergence → Task B ✓
- GicVcpus replacing NoIrqVcpus → Task C ✓
- channel-based WFI parking (recv/recv_timeout, drain stale) → Task C ✓
- parking unit test → Task C ✓
- boot-run gate to shell prompt → Task D ✓
- regression (uart-echo/gic-smoke build) → Task B Step 3 ✓
- out-of-scope (serial RX, SMP) → not implemented ✓

**Placeholder scan:** Task A is an experiment with concrete candidate edits and a
measurable success signal (service-start lines), not a placeholder. The
`set_vtimer_irq`-may-need-more note is conditional on A's empirical result, which
is the milestone's nature. All other code is complete.

**Type consistency:** `GicVcpus::{new,register}` + `Sender<()>` match the run
loop's `mpsc::channel::<()>()` + `register(0, tx)`. `drain(&mpsc::Receiver<()>)`
matches its call sites and the test. `Vcpus` trait methods match `hvf::Vcpus`.
`GicVcpus` replaces `NoIrqVcpus` at the single `Arc::new(...)` site.
