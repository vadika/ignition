# Phase 1 Milestone 2f: interrupt delivery → shell prompt

Status: approved design, pre-implementation.
Date: 2026-06-12.
Project: ignition (Firecracker → macOS/HVF fork). Follows 2e (virtio-blk rootfs
mounts, init runs, then OpenRC stalls at 0% CPU parked in WFI; see
`docs/2e-virtio-result.md`).

## Goal

Deliver the virtual-timer interrupt to the guest so userspace `sleep`/service
timeouts work, and replace the earlycon-grade bounded-sleep WFI handling with
proper channel-based parking. Result: OpenRC finishes, getty spawns, and a
login/shell prompt appears on host stdout. Output only — interactive keyboard
input (serial RX) is the next milestone.

## Nature: experiment-led

Unlike prior milestones, the core — vtimer PPI delivery with the in-kernel
`hv_gic` — is unproven and must be determined by live boot iteration. The
controller runs the experiment and the boot gates in the main session (needs the
hypervisor entitlement + the real kernel/rootfs). The mechanical parking code is
the only cleanly pre-specifiable part.

## Root cause (established)

`hvf::lib.rs::hvf_sync_vtimer` is a verbatim lift of libkrun's **userspace-GIC**
logic. On `VtimerActivated` it masks the vtimer, then only unmasks once the timer
condition clears — which assumes a userspace GIC queued the PPI for the guest to
acknowledge. With our **in-kernel GIC** and `NoIrqVcpus::set_vtimer_irq` a no-op,
the PPI is never delivered, the condition never clears, and the vtimer stays
masked forever. The kernel boots (busy-wait) but every userspace timed wait
hangs. The in-kernel GIC does expose `HV_GIC_INT_EL1_VIRTUAL_TIMER = 27`, so the
delivery primitive exists; the question is which call path delivers it.

## Component 1: vtimer delivery (`hvf` crate) — experiment-driven

Determine the correct in-kernel-GIC vtimer delivery by live experiment, trying in
order and keeping the first that makes userspace timed waits progress:

1. **Unmask-and-let-GIC-deliver:** on `VtimerActivated`, unmask the vtimer
   (`hv_vcpu_set_vtimer_mask(false)`) unconditionally and let the in-kernel GIC
   raise PPI 27 itself. (The current code gates the unmask on the timer condition
   clearing — likely wrong for in-kernel GIC.)
2. **Raw line assert:** `hv_vcpu_set_pending_interrupt(vcpuid, IRQ, true)` to
   assert the vCPU IRQ line, relying on the in-kernel GIC to present the vtimer
   INTID at `ICC_IAR1`.
3. **Combination / masking-window tweak:** keep the vtimer briefly masked to
   avoid an exit storm but unmask on the next entry regardless of condition.

The winning change lands in `hvf_sync_vtimer` (and possibly `run`). Because the
`hvf` crate is otherwise a verbatim libkrun lift, this is a **deliberate,
documented divergence** for the in-kernel-GIC path — note it in the code and in
`docs/phase1-followups.md`.

Acceptance for this component: with the change, the live boot progresses past
`OpenRC 0.52.1` (service start messages appear), proving the timer tick fires.

## Component 2: `GicVcpus` (`vmm` crate) replacing `NoIrqVcpus`

A `Vcpus` impl backed by the in-kernel GIC semantics and a per-vCPU wake channel.

```rust
// crates/vmm/src/vstate/gic_vcpus.rs (new), used by hvf_vcpu.rs
pub struct GicVcpus { /* per-vcpu: wfe_sender: Mutex<Option<Sender<()>>> */ }
impl GicVcpus { pub fn new(vcpu_count: u64) -> Self; pub fn register(&self, vcpuid, Sender<()>); }
impl hvf::Vcpus for GicVcpus {
    fn set_vtimer_irq(&self, vcpuid)  // the discovered mechanism + wake the parked vcpu
    fn should_wait(&self, vcpuid) -> bool // true (park) — no userspace pending queue
    fn has_pending_irq(&self, vcpuid) -> bool // false — in-kernel GIC drives SPIs
    fn get_pending_irq(&self, vcpuid) -> u32 // unused (in-kernel GIC); return spurious 1023
    fn handle_sysreg_read/write          // Some(0) / true, as today
}
```

Whether `set_vtimer_irq` needs to do anything beyond waking depends on Component
1's result (if unmask-on-entry handles delivery, this is just a wake).

## Component 3: channel-based WFI parking (`vmm` run loop)

Per-vCPU `(Sender<()>, Receiver<()>)` (crossbeam unbounded). The `Receiver` lives
in the spawned vCPU thread's run loop; the `Sender` is registered into `GicVcpus`
(and is the hook serial RX will use later).

Replace the `MAX_PARK` bounded-sleep arms:
- `WaitForEvent` → `receiver.recv()` (block until woken; a no-deadline WFI waits
  for an external interrupt).
- `WaitForEventTimeout(d)` → `receiver.recv_timeout(d)` (wake at the CNTV deadline
  → re-enter → vtimer fires).
- `WaitForEventExpired` / `VtimerActivated` → continue (unchanged).

Drain any stale wake tokens after a park so a buffered token doesn't short-circuit
the next genuine wait. A waker (`Sender::send`) from another thread, plus
`hvf::vcpu_request_exit` if the vCPU is mid-`run`, wakes a parked vCPU. For 2f the
only cross-thread waker is none yet (timer self-wakes via timeout; virtio is
same-thread); the channel is wired so 2f's serial-RX successor only adds a sender.

## Verification

- **Primary (live, controller):** `target/debug/boot kimage/out/Image
  kimage/out/rootfs.ext4` (after codesign) reaches a `login:` or shell prompt on
  stdout. Run + iterated in the main session.
- **Regression:** `uart-echo`, `gic-smoke`, `hvf-spike` still build, sign, and
  pass; the workspace builds; existing unit tests stay green.
- **Unit:** a small test on the parking primitive (returns promptly on a sent
  token; returns after the timeout when none sent). The vtimer/GIC path itself is
  only provable by the live boot.

## Decomposition (plan tasks)

A. **vtimer experiment (controller, in-session):** iterate the Component-1
   candidates against the live boot; capture the winning `hvf` change. Gate:
   boot progresses past the OpenRC banner.
B. **Land the vtimer fix** in `hvf` with the documented divergence + a note.
C. **`GicVcpus` + channel parking** in `vmm`: new `GicVcpus`, rework
   `hvf_vcpu.rs` run loop to channel parking, register the sender per vCPU,
   replace `NoIrqVcpus`. Build-checked + the parking unit test.
D. **Boot run → shell prompt** (controller, in-session): the integration gate;
   iterate if it stalls elsewhere (e.g. a second missing piece surfaces).

Because A and D are exploratory and need the live boot, they run in-session;
B and C are mechanical once A's result is known.

## Out of scope (→ next milestone)

Serial RX / interactive typing (the channel is built to accept it), SMP
(`PSCI CPU_ON` secondaries), snapshot/restore, and any timer-accuracy tuning
beyond "userspace timed waits progress".

## Risks

- The vtimer may resist all three candidates → deeper HVF investigation (read
  Apple's `hv_gic`/vtimer headers, try `hv_gic`-specific calls). Time-box and
  report.
- Changing `hvf` diverges from the verbatim libkrun lift — documented.
- Wake race: an IRQ raised just before `recv_timeout` — bounded by the timeout so
  it cannot hang, only delay one quantum.
- A new blocker may surface past the timer (e.g. another device the guest waits
  on); 2d/2e showed each layer can reveal the next — handle iteratively.

## References

- `crates/hvf/src/lib.rs` — `hvf_sync_vtimer`, `run`, `vcpu_set_pending_irq`
- `crates/hvf/src/bindings.rs` — `hv_vcpu_set_vtimer_mask`,
  `hv_vcpu_set_pending_interrupt`, `hv_gic_intid_t_HV_GIC_INT_EL1_VIRTUAL_TIMER`
- `libkrun/src/vmm/src/macos/vstate.rs` + `devices/src/legacy/vcpu.rs` — the
  channel-parking + VcpuList pattern (userspace-GIC; we adapt for in-kernel)
- `docs/2e-virtio-result.md` — the stall this milestone resolves
