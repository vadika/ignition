# FDT Follow-ups Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the two actionable FDT follow-ups: a `fdt_addr` placement guard so the DTB stays in the kernel's early-map window for RAM larger than 512 MiB, and a typed device list in `FdtConfig` replacing the special-cased `serial`/`virtio` fields. Document the other two FDT items (`GicInfo` multi-redist, mpidr mask) as deferred with rationale.

**Architecture:** (1) `layout::fdt_addr` clamps DTB placement to `min(ram_size, 512 MiB)` from RAM base. (2) `FdtConfig` gains `devices: Vec<FdtDevice>` (an enum over the node kinds) and `generate` dispatches per kind, replacing the `serial: MmioDev` + `virtio: Option<MmioDev>` fields.

**Tech Stack:** Rust, `vm-fdt` writer crate.

---

## Background the engineer needs

- **`crates/arch/src/aarch64/layout.rs`** `fdt_addr(ram_size) -> u64` currently
  returns `(RAM_BASE + ram_size - FDT_MAX_SIZE) & !0x7` with a `TODO(larger-ram)`:
  top-of-RAM placement leaves the kernel's early-mapped first 512 MiB once
  `ram_size > 512 MiB`. `RAM_BASE = 0x4000_0000`, `FDT_MAX_SIZE = 0x20_0000` (2 MiB).
  Existing tests in that file: `fdt_addr_is_aligned_within_ram_and_clear_of_kernel`
  (ram_size = 512 MiB), `fdt_addr_at_minimum_ram_size`, `fdt_addr_panics_below_minimum`.
- **`crates/arch/src/aarch64/fdt.rs`**: `FdtConfig` has `serial: MmioDev` and
  `virtio: Option<MmioDev>`. `generate` calls `create_serial_node(&mut fdt,
  &cfg.serial)` then `if let Some(virtio) = &cfg.virtio { create_virtio_node(...) }`.
  `create_serial_node(&mut FdtWriter, &MmioDev)` and `create_virtio_node(&mut
  FdtWriter, &MmioDev)` already exist and stay as-is (each emits a different node).
  `MmioDev { addr, size, irq }` is public.
- **`FdtConfig` is constructed in 3 places:** `fdt.rs` test helper `sample()`
  (~line 241), `spike/src/bin/boot.rs:252`, `spike/src/bin/gic-smoke.rs:47`. The
  test `virtio_node_present_only_when_set` (~line 388) mutates `cfg.virtio`.
  `spike/src/bin/uart-echo.rs` does NOT build an FDT.
- **Build/test:** `cargo test -p ignition-arch`, `cargo build --workspace`,
  `cargo clippy --workspace`. Crate name for the arch crate is `ignition-arch`.
- **Commit policy:** plain messages, NO `Co-Authored-By` / "Generated with Claude".

## File structure

- **Modify `crates/arch/src/aarch64/layout.rs`** — clamp `fdt_addr`; add a test (Task 1).
- **Modify `crates/arch/src/aarch64/fdt.rs`** — `FdtDevice` enum, `devices` field,
  `generate` dispatch, migrate `sample()` + `virtio_node_present_only_when_set` (Task 2).
- **Modify `spike/src/bin/boot.rs` and `spike/src/bin/gic-smoke.rs`** — build the
  `devices` vec (Task 2).

---

## Task 1: `fdt_addr` large-RAM guard

**Files:**
- Modify: `crates/arch/src/aarch64/layout.rs`

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `crates/arch/src/aarch64/layout.rs`:

```rust
    #[test]
    fn fdt_addr_large_ram_stays_within_early_map() {
        // For RAM larger than the 512 MiB early-map window, the DTB must sit
        // within the first 512 MiB, not at the top of RAM.
        let ram_size = 0x8000_0000; // 2 GiB
        let addr = fdt_addr(ram_size);
        assert_eq!(addr & 0x7, 0, "fdt addr must be 8-byte aligned");
        assert!(addr >= RAM_BASE, "fdt addr must be within RAM");
        assert!(
            addr < RAM_BASE + 0x2000_0000,
            "DTB must stay within the kernel's early-mapped first 512 MiB"
        );
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ignition-arch fdt_addr_large_ram 2>&1 | tail -15`
Expected: FAIL — the current top-of-RAM placement returns `RAM_BASE + 2GiB - 2MiB`,
which is `>= RAM_BASE + 512 MiB`, so the assertion trips.

- [ ] **Step 3: Clamp the placement**

In `crates/arch/src/aarch64/layout.rs`, add a constant near `FDT_MAX_SIZE`:

```rust
/// The kernel maps the first 512 MiB of RAM early in boot, so the DTB must live
/// within that window (it is read before the full linear map exists).
pub const DTB_EARLY_MAP_LIMIT: u64 = 0x2000_0000; // 512 MiB
```

Replace the body of `fdt_addr` (keep the `debug_assert!`) with:

```rust
pub fn fdt_addr(ram_size: u64) -> u64 {
    debug_assert!(ram_size >= FDT_MAX_SIZE, "ram_size must be >= FDT_MAX_SIZE");
    // Place the DTB at the top of usable low RAM: the top of RAM, but never above
    // the kernel's early-map window, so for ram_size > 512 MiB it sits just below
    // that limit instead of beyond it.
    let window = ram_size.min(DTB_EARLY_MAP_LIMIT);
    (RAM_BASE + window - FDT_MAX_SIZE) & !0x7
}
```

Also update the `fdt_addr` doc comment: remove the `TODO(larger-ram)` line and note
the DTB is placed within `min(ram_size, 512 MiB)`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ignition-arch 2>&1 | grep 'test result'`
Expected: PASS — the new test plus all existing layout tests. (The 512 MiB and
minimum-ram cases are unchanged: `min(ram_size, 512 MiB) == ram_size` for both.)

- [ ] **Step 5: Commit**

```bash
git add crates/arch/src/aarch64/layout.rs
git commit -m "fix(arch): keep DTB within the kernel early-map window for large RAM"
```

---

## Task 2: Typed device list in `FdtConfig`

**Files:**
- Modify: `crates/arch/src/aarch64/fdt.rs`
- Modify: `spike/src/bin/boot.rs`
- Modify: `spike/src/bin/gic-smoke.rs`

- [ ] **Step 1: Migrate the test helper + virtio test to the new shape (write the failing tests)**

In `crates/arch/src/aarch64/fdt.rs`, change the `sample()` helper (~line 241): replace
the `serial: MmioDev { ... },` and `virtio: None,` fields with a `devices` field:

```rust
            devices: vec![FdtDevice::Serial(MmioDev { addr: 0x0900_0000, size: 0x1000, irq: 33 })],
```

(Remove the old `serial:` and `virtio:` lines from `sample()`.)

And change `virtio_node_present_only_when_set` (~line 394-396): replace

```rust
        let mut cfg = sample();
        cfg.virtio = Some(MmioDev { addr: 0x0a00_0000, size: 0x200, irq: 1 });
```

with

```rust
        let mut cfg = sample();
        cfg.devices.push(FdtDevice::VirtioBlk(MmioDev { addr: 0x0a00_0000, size: 0x200, irq: 1 }));
```

- [ ] **Step 2: Run tests to verify they fail (compile error)**

Run: `cargo test -p ignition-arch 2>&1 | tail -15`
Expected: FAIL to compile — `FdtDevice` not found, and `FdtConfig` has no `devices`
field (still has `serial`/`virtio`).

- [ ] **Step 3: Add `FdtDevice`, swap the `FdtConfig` fields, dispatch in `generate`**

In `crates/arch/src/aarch64/fdt.rs`, add the enum just below the `MmioDev` struct
(~after line 31):

```rust
/// An MMIO device to emit as an FDT node. Each variant maps to a node builder,
/// so adding a device kind (RTC, more virtio) is a new variant + match arm rather
/// than a new `FdtConfig` field.
pub enum FdtDevice {
    /// 16550-compatible serial -> `ns16550a` node.
    Serial(MmioDev),
    /// virtio-mmio block device -> `virtio,mmio` node.
    VirtioBlk(MmioDev),
}
```

In `FdtConfig`, remove the `pub serial: MmioDev,` and `pub virtio: Option<MmioDev>,`
fields and add (e.g. where `serial` was):

```rust
    /// MMIO devices to emit, in node order.
    pub devices: Vec<FdtDevice>,
```

In `generate`, replace the serial + virtio block (currently):

```rust
    create_serial_node(&mut fdt, &cfg.serial)?;
    if let Some(virtio) = &cfg.virtio {
        create_virtio_node(&mut fdt, virtio)?;
    }
```

with:

```rust
    for dev in &cfg.devices {
        match dev {
            FdtDevice::Serial(m) => create_serial_node(&mut fdt, m)?,
            FdtDevice::VirtioBlk(m) => create_virtio_node(&mut fdt, m)?,
        }
    }
```

Leave `create_serial_node` and `create_virtio_node` (both take `&MmioDev`) unchanged.

- [ ] **Step 4: Run the arch tests**

Run: `cargo test -p ignition-arch 2>&1 | grep 'test result'`
Expected: PASS — `virtio_node_present_only_when_set` (absent in `sample()`, present
after pushing `VirtioBlk`), `serial_node_is_ns16550a`, and all others.

- [ ] **Step 5: Migrate `spike/src/bin/boot.rs`**

In `spike/src/bin/boot.rs` find the `FdtConfig { ... }` (~line 252). It currently has:

```rust
        serial: MmioDev {
            addr: layout::SERIAL_BASE,
            size: layout::SERIAL_SIZE,
            irq: layout::SERIAL_SPI,
        },
        gic: gic.fdt_info(),
        initrd: None,
        virtio: disk_path
            .as_ref()
            .map(|_| MmioDev { addr: layout::VIRTIO_BASE, size: layout::VIRTIO_SIZE, irq: layout::VIRTIO_SPI }),
```

Build the device vec BEFORE the `FdtConfig { ... }` literal:

```rust
    let mut fdt_devices = vec![fdt::FdtDevice::Serial(MmioDev {
        addr: layout::SERIAL_BASE,
        size: layout::SERIAL_SIZE,
        irq: layout::SERIAL_SPI,
    })];
    if disk_path.is_some() {
        fdt_devices.push(fdt::FdtDevice::VirtioBlk(MmioDev {
            addr: layout::VIRTIO_BASE,
            size: layout::VIRTIO_SIZE,
            irq: layout::VIRTIO_SPI,
        }));
    }
```

Then in the `FdtConfig { ... }` literal, remove the `serial:` and `virtio:` fields and
add `devices: fdt_devices,` (keep `gic:`, `initrd:`, and the others). Ensure `FdtDevice`
is reachable — it is via the existing `use arch::aarch64::fdt::{self, FdtConfig, MmioDev}`
(reference it as `fdt::FdtDevice` as shown, or add `FdtDevice` to that `use`). Check the
existing `use` line at the top of boot.rs and pick whichever keeps it consistent.

- [ ] **Step 6: Migrate `spike/src/bin/gic-smoke.rs`**

In `spike/src/bin/gic-smoke.rs` (~line 47), the `FdtConfig { ... }` has
`serial: MmioDev { addr: 0x0900_0000, size: 0x1000, irq: 33 },` and `virtio: None,`.
Remove both and add:

```rust
        devices: vec![arch::aarch64::fdt::FdtDevice::Serial(MmioDev { addr: 0x0900_0000, size: 0x1000, irq: 33 })],
```

Match however `MmioDev` / the fdt module are already imported in that file (read the
`use` lines first; reference `FdtDevice` via the same path as `MmioDev`).

- [ ] **Step 7: Build + clippy the workspace**

Run:
```bash
cargo build --workspace 2>&1 | tail -3
cargo clippy --workspace 2>&1 | grep -c 'warning:'
```
Expected: builds, 0 clippy warnings. If clippy flags the `vec!` + push pattern (it
will not for a conditional push), address only genuinely-new warnings in your code.

- [ ] **Step 8: Re-sign and smoke-test boot still reaches login**

```bash
cargo build -p hvf-spike --bin boot 2>&1 | tail -1
./scripts/sign.sh target/debug/boot
pkill -9 -f 'target/debug/boot' 2>/dev/null; sleep 1
( target/debug/boot kimage/out/Image kimage/out/rootfs.ext4 >/tmp/fdt.out 2>/dev/null & p=$!; sleep 35; kill -9 $p 2>/dev/null; wait $p 2>/dev/null )
echo "login: $(grep -c 'login:' /tmp/fdt.out)"
```
Expected: `login: 1` — the guest still boots with the serial + virtio nodes emitted
from the new device list. If 0, inspect `/tmp/fdt.out` (the FDT may be missing a
node) and report.

- [ ] **Step 9: Commit**

```bash
git add crates/arch/src/aarch64/fdt.rs spike/src/bin/boot.rs spike/src/bin/gic-smoke.rs
git commit -m "refactor(arch): FdtConfig takes a typed device list (serial/virtio)"
```

---

## Self-review notes (resolved)

- **Spec coverage:** `fdt_addr` large-RAM guard (Task 1); `serial`/`virtio` ->
  typed `Vec<FdtDevice>` (Task 2). The other two FDT items (GicInfo multi-redist,
  mpidr mask) are intentionally NOT code changes — documented as deferred (controller
  updates `docs/phase1-followups.md` after these tasks; GicInfo multi-region is moot
  for HVF's single contiguous redistributor region, mpidr re-validation is gated on
  the SMP vCPU milestone wiring real MPIDRs).
- **Type consistency:** `FdtDevice::{Serial,VirtioBlk}(MmioDev)`, `FdtConfig.devices:
  Vec<FdtDevice>`, `DTB_EARLY_MAP_LIMIT`, `fdt_addr` clamp used consistently across
  fdt.rs, boot.rs, gic-smoke.rs, and the tests. All 3 `FdtConfig` constructions
  migrated; `create_serial_node`/`create_virtio_node` signatures unchanged.
- **uart-echo.rs is NOT touched** (it builds no FDT) — confirmed by grep.
```
