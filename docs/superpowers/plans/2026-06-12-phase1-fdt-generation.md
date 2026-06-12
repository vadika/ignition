# Phase 1 Milestone 2a: FDT Generation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Generate a boot-minimal aarch64 Flattened Device Tree (DTB) blob from a config struct, so a later milestone can hand it to a guest kernel in X0.

**Architecture:** A pure function `arch::aarch64::fdt::generate(&FdtConfig) -> Result<Vec<u8>, vm_fdt::Error>` builds the DTB with the `vm-fdt` writer crate (same crate Firecracker uses), lifting FC's node construction stripped to the boot-minimal set (root, cpus, memory, chosen, GICv3 intc, apb-pclk, timer, psci, ns16550a uart). No HVF — fully unit-tested by parsing the blob back with the `fdt` reader crate.

**Tech Stack:** Rust edition 2024, `vm-fdt = "0.3"` (writer), `fdt = "0.1"` (reader, dev-only).

**Commit convention for this project:** plain commit messages, NO `Co-Authored-By` / "Generated with Claude" trailer.

---

## File Structure

- `crates/arch/Cargo.toml` — add `vm-fdt` dep + `fdt` dev-dep
- `crates/arch/src/aarch64/mod.rs` — add `pub mod fdt;`
- `crates/arch/src/aarch64/fdt.rs` — **create**: config structs, `generate`, node helpers, unit tests

The whole milestone is one focused module (~180 lines incl. tests). It is built in
three TDD increments: (1) scaffolding + root/memory + the parser smoke test that
de-risks the writer→reader pipeline; (2) cpus + chosen; (3) GIC + clock + timer +
psci + serial.

---

## Task 1: Scaffolding, root + memory nodes, parser smoke test

This task proves the riskiest assumption first: that the `fdt` reader parses
`vm-fdt`'s output.

**Files:**
- Modify: `crates/arch/Cargo.toml`
- Modify: `crates/arch/src/aarch64/mod.rs`
- Create: `crates/arch/src/aarch64/fdt.rs`

- [ ] **Step 1: Add dependencies**

In `crates/arch/Cargo.toml`, add a `[dependencies]` section with `vm-fdt` and a
`[dev-dependencies]` section with `fdt`. The file currently has `[package]` and
`[lib]` only (no dependencies). Result should contain:

```toml
[dependencies]
vm-fdt = "0.3"

[dev-dependencies]
fdt = "0.1"
```

- [ ] **Step 2: Declare the module**

In `crates/arch/src/aarch64/mod.rs`, add at the end (after the existing
`pub mod sysreg;`):

```rust
pub mod fdt;
```

- [ ] **Step 3: Write `fdt.rs` with config types, a root+memory `generate`, and the smoke test**

Create `crates/arch/src/aarch64/fdt.rs`:

```rust
// Flattened Device Tree (DTB) generation for the ignition aarch64 microVM.
//
// Built with the `vm-fdt` writer crate; node construction is a stripped lift of
// Firecracker's src/vmm/src/arch/aarch64/fdt.rs (cache nodes, virtio, vmgenid,
// PCI, RTC removed — none have backing devices yet).

use vm_fdt::FdtWriter;

// Uniquely identifies the interrupt-controller node; the root and devices point
// at it via `interrupt-parent` / `phandle`.
const GIC_PHANDLE: u32 = 1;
// Uniquely identifies the fixed clock the serial node references.
const CLOCK_PHANDLE: u32 = 2;
// On ARMv8 64-bit, root address/size cells are 2.
const ADDRESS_CELLS: u32 = 2;
const SIZE_CELLS: u32 = 2;

// GIC DT interrupt encoding (Linux arm,gic binding).
const IRQ_TYPE_SPI: u32 = 0;
const IRQ_TYPE_PPI: u32 = 1;
const IRQ_TYPE_EDGE_RISING: u32 = 1;
const IRQ_TYPE_LEVEL_HI: u32 = 4;

/// An MMIO device's placement and its SPI interrupt number.
pub struct MmioDev {
    pub addr: u64,
    pub size: u64,
    /// Bare GIC SPI index (the DT cell value; the kernel adds the 32 offset).
    pub irq: u32,
}

/// GICv3 placement, supplied by the GIC milestone. Parameterized so FDT
/// generation stays pure.
pub struct GicInfo {
    pub dist_base: u64,
    pub dist_size: u64,
    pub redist_base: u64,
    pub redist_size: u64,
    /// Maintenance interrupt PPI number (typically 9).
    pub maint_irq: u32,
}

/// Everything needed to describe the machine to the guest kernel.
pub struct FdtConfig {
    pub mem_base: u64,
    pub mem_size: u64,
    /// One entry per vCPU, in boot order.
    pub cpu_mpidrs: Vec<u64>,
    /// Kernel command line -> /chosen bootargs.
    pub cmdline: String,
    pub serial: MmioDev,
    pub gic: GicInfo,
    /// (guest addr, size) when an initramfs is loaded.
    pub initrd: Option<(u64, u64)>,
}

/// Build the DTB blob. All errors originate in `vm-fdt` (e.g. an interior NUL in
/// `cmdline` -> `Error::InvalidString`).
pub fn generate(cfg: &FdtConfig) -> Result<Vec<u8>, vm_fdt::Error> {
    let mut fdt = FdtWriter::new()?;

    let root = fdt.begin_node("")?;
    fdt.property_string("compatible", "linux,dummy-virt")?;
    fdt.property_u32("#address-cells", ADDRESS_CELLS)?;
    fdt.property_u32("#size-cells", SIZE_CELLS)?;
    fdt.property_u32("interrupt-parent", GIC_PHANDLE)?;

    create_memory_node(&mut fdt, cfg.mem_base, cfg.mem_size)?;

    fdt.end_node(root)?;
    fdt.finish()
}

fn create_memory_node(fdt: &mut FdtWriter, base: u64, size: u64) -> Result<(), vm_fdt::Error> {
    let mem = fdt.begin_node("memory@ram")?;
    fdt.property_string("device_type", "memory")?;
    fdt.property_array_u64("reg", &[base, size])?;
    fdt.end_node(mem)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fdt::Fdt;

    // ---- raw big-endian property decoders (minimal reader API surface) ----
    fn be_u32s(bytes: &[u8]) -> Vec<u32> {
        bytes.chunks_exact(4).map(|c| u32::from_be_bytes(c.try_into().unwrap())).collect()
    }
    fn be_u64s(bytes: &[u8]) -> Vec<u64> {
        bytes.chunks_exact(8).map(|c| u64::from_be_bytes(c.try_into().unwrap())).collect()
    }
    /// Decode a DT string property (NUL-terminated) to &str.
    fn dt_str(bytes: &[u8]) -> &str {
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        std::str::from_utf8(&bytes[..end]).unwrap()
    }

    fn sample() -> FdtConfig {
        FdtConfig {
            mem_base: 0x4000_0000,
            mem_size: 0x2000_0000,
            cpu_mpidrs: vec![0x0, 0x1],
            cmdline: "console=ttyS0 earlycon=uart8250,mmio,0x9000000".to_string(),
            serial: MmioDev { addr: 0x0900_0000, size: 0x1000, irq: 33 },
            gic: GicInfo {
                dist_base: 0x0800_0000,
                dist_size: 0x1_0000,
                redist_base: 0x080A_0000,
                redist_size: 0xC_0000,
                maint_irq: 9,
            },
            initrd: None,
        }
    }

    #[test]
    fn blob_parses_with_root_and_memory() {
        let blob = generate(&sample()).unwrap();
        let dt = Fdt::new(&blob).expect("fdt reader must parse vm-fdt output");

        let root = dt.find_node("/").unwrap();
        assert_eq!(dt_str(root.property("compatible").unwrap().value), "linux,dummy-virt");
        assert_eq!(be_u32s(root.property("#address-cells").unwrap().value), vec![2]);

        let mem = dt.find_node("/memory@ram").unwrap();
        assert_eq!(dt_str(mem.property("device_type").unwrap().value), "memory");
        assert_eq!(be_u64s(mem.property("reg").unwrap().value), vec![0x4000_0000, 0x2000_0000]);
    }
}
```

**Reader API caveat:** the test uses `fdt::Fdt::new(&blob)`, `dt.find_node(path) -> Option<FdtNode>`, and `node.property(name) -> Option<NodeProperty>` with a public `.value: &[u8]`. These are the repnop `fdt` 0.1.x APIs. If the resolved version differs (method renamed, `value` is a method, etc.), adjust ONLY the test accessors to read the property's raw bytes — keep the big-endian decoders and assertions identical. If `Fdt::new` cannot parse the blob at all (writer/reader incompatibility), STOP and report BLOCKED with the exact error — do not paper over it; the whole milestone depends on this pipeline.

- [ ] **Step 4: Run the test, verify it passes**

Run: `cargo test -p ignition-arch fdt 2>&1 | tail -20`
Expected: `test result: ok. 1 passed`. (Cargo fetches `vm-fdt` and `fdt` on first build.)

- [ ] **Step 5: Commit**

```bash
git add crates/arch/Cargo.toml crates/arch/src/aarch64/mod.rs crates/arch/src/aarch64/fdt.rs Cargo.lock
git commit -m "feat(arch): FDT generation scaffolding with root + memory nodes

vm-fdt writer; fdt reader (dev-dep) for tests. Smoke test confirms the
writer->reader pipeline parses, plus root compatible/cells and memory reg."
```

---

## Task 2: cpus + chosen nodes

**Files:**
- Modify: `crates/arch/src/aarch64/fdt.rs`

- [ ] **Step 1: Write the failing tests**

Add these tests inside the existing `mod tests` block in
`crates/arch/src/aarch64/fdt.rs` (after `blob_parses_with_root_and_memory`):

```rust
    #[test]
    fn cpu_nodes_match_mpidrs() {
        let blob = generate(&sample()).unwrap();
        let dt = Fdt::new(&blob).unwrap();
        let cpus = dt.find_node("/cpus").unwrap();
        let cpu_children: Vec<_> =
            cpus.children().filter(|c| c.name.starts_with("cpu@")).collect();
        assert_eq!(cpu_children.len(), 2);

        let cpu0 = dt.find_node("/cpus/cpu@0").unwrap();
        assert_eq!(dt_str(cpu0.property("device_type").unwrap().value), "cpu");
        assert_eq!(dt_str(cpu0.property("enable-method").unwrap().value), "psci");
        assert_eq!(be_u64s(cpu0.property("reg").unwrap().value), vec![0x0]);

        let cpu1 = dt.find_node("/cpus/cpu@1").unwrap();
        assert_eq!(be_u64s(cpu1.property("reg").unwrap().value), vec![0x1]);
    }

    #[test]
    fn chosen_has_bootargs_and_no_initrd_by_default() {
        let blob = generate(&sample()).unwrap();
        let dt = Fdt::new(&blob).unwrap();
        let chosen = dt.find_node("/chosen").unwrap();
        assert_eq!(
            dt_str(chosen.property("bootargs").unwrap().value),
            "console=ttyS0 earlycon=uart8250,mmio,0x9000000"
        );
        assert!(chosen.property("linux,initrd-start").is_none());
        assert!(chosen.property("linux,initrd-end").is_none());
    }

    #[test]
    fn chosen_includes_initrd_when_set() {
        let mut cfg = sample();
        cfg.initrd = Some((0x4800_0000, 0x10_0000));
        let blob = generate(&cfg).unwrap();
        let dt = Fdt::new(&blob).unwrap();
        let chosen = dt.find_node("/chosen").unwrap();
        assert_eq!(be_u64s(chosen.property("linux,initrd-start").unwrap().value), vec![0x4800_0000]);
        assert_eq!(be_u64s(chosen.property("linux,initrd-end").unwrap().value), vec![0x4810_0000]);
    }
```

- [ ] **Step 2: Run tests, verify they fail**

Run: `cargo test -p ignition-arch fdt 2>&1 | tail -20`
Expected: the three new tests FAIL (`find_node("/cpus")` / `/chosen` return `None` -> `unwrap` panics), because `generate` does not emit those nodes yet.

- [ ] **Step 3: Add the cpu and chosen node helpers and wire them in**

In `crates/arch/src/aarch64/fdt.rs`, add the two helper functions (place them
after `create_memory_node`):

```rust
fn create_cpu_nodes(fdt: &mut FdtWriter, mpidrs: &[u64]) -> Result<(), vm_fdt::Error> {
    let cpus = fdt.begin_node("cpus")?;
    fdt.property_u32("#address-cells", 0x02)?;
    fdt.property_u32("#size-cells", 0x0)?;
    for (i, mpidr) in mpidrs.iter().enumerate() {
        let cpu = fdt.begin_node(&format!("cpu@{i:x}"))?;
        fdt.property_string("device_type", "cpu")?;
        fdt.property_string("compatible", "arm,arm-v8")?;
        fdt.property_string("enable-method", "psci")?;
        // First 24 bits of MPIDR (affinity).
        fdt.property_u64("reg", mpidr & 0x7F_FFFF)?;
        fdt.end_node(cpu)?;
    }
    fdt.end_node(cpus)?;
    Ok(())
}

fn create_chosen_node(
    fdt: &mut FdtWriter,
    cmdline: &str,
    initrd: Option<(u64, u64)>,
) -> Result<(), vm_fdt::Error> {
    let chosen = fdt.begin_node("chosen")?;
    fdt.property_string("bootargs", cmdline)?;
    if let Some((addr, size)) = initrd {
        fdt.property_u64("linux,initrd-start", addr)?;
        fdt.property_u64("linux,initrd-end", addr + size)?;
    }
    fdt.end_node(chosen)?;
    Ok(())
}
```

Then in `generate`, insert the calls so the body reads (cpus before memory,
chosen after memory):

```rust
    create_cpu_nodes(&mut fdt, &cfg.cpu_mpidrs)?;
    create_memory_node(&mut fdt, cfg.mem_base, cfg.mem_size)?;
    create_chosen_node(&mut fdt, &cfg.cmdline, cfg.initrd)?;
```

(Replace the lone `create_memory_node(...)` call with these three lines.)

- [ ] **Step 4: Run tests, verify all pass**

Run: `cargo test -p ignition-arch fdt 2>&1 | tail -20`
Expected: `test result: ok. 4 passed`.

- [ ] **Step 5: Commit**

```bash
git add crates/arch/src/aarch64/fdt.rs
git commit -m "feat(arch): FDT cpus + chosen nodes

Per-vCPU cpu@N nodes (psci enable-method, mpidr reg) and /chosen bootargs
with optional initrd start/end."
```

---

## Task 3: GIC + clock + timer + psci + serial nodes

**Files:**
- Modify: `crates/arch/src/aarch64/fdt.rs`

- [ ] **Step 1: Write the failing tests**

Add these tests inside `mod tests` (after `chosen_includes_initrd_when_set`):

```rust
    #[test]
    fn gic_node_is_gicv3_with_reg_and_cells() {
        let blob = generate(&sample()).unwrap();
        let dt = Fdt::new(&blob).unwrap();
        let intc = dt.find_node("/intc").unwrap();
        assert_eq!(dt_str(intc.property("compatible").unwrap().value), "arm,gic-v3");
        assert_eq!(be_u32s(intc.property("#interrupt-cells").unwrap().value), vec![3]);
        assert!(intc.property("interrupt-controller").is_some());
        assert_eq!(
            be_u64s(intc.property("reg").unwrap().value),
            vec![0x0800_0000, 0x1_0000, 0x080A_0000, 0xC_0000]
        );
        assert_eq!(be_u32s(intc.property("phandle").unwrap().value), vec![1]);
    }

    #[test]
    fn psci_uses_hvc() {
        let blob = generate(&sample()).unwrap();
        let dt = Fdt::new(&blob).unwrap();
        let psci = dt.find_node("/psci").unwrap();
        assert_eq!(dt_str(psci.property("compatible").unwrap().value), "arm,psci-0.2");
        assert_eq!(dt_str(psci.property("method").unwrap().value), "hvc");
    }

    #[test]
    fn timer_is_armv8_timer() {
        let blob = generate(&sample()).unwrap();
        let dt = Fdt::new(&blob).unwrap();
        let timer = dt.find_node("/timer").unwrap();
        assert_eq!(dt_str(timer.property("compatible").unwrap().value), "arm,armv8-timer");
        // four PPIs, each 3 cells: [PPI, n, LEVEL_HI]
        assert_eq!(
            be_u32s(timer.property("interrupts").unwrap().value),
            vec![1, 13, 4, 1, 14, 4, 1, 11, 4, 1, 10, 4]
        );
    }

    #[test]
    fn serial_node_is_ns16550a() {
        let blob = generate(&sample()).unwrap();
        let dt = Fdt::new(&blob).unwrap();
        let uart = dt.find_node("/uart@9000000").unwrap();
        assert_eq!(dt_str(uart.property("compatible").unwrap().value), "ns16550a");
        assert_eq!(be_u64s(uart.property("reg").unwrap().value), vec![0x0900_0000, 0x1000]);
        assert_eq!(be_u32s(uart.property("clocks").unwrap().value), vec![2]);
        // [SPI, irq, EDGE_RISING]
        assert_eq!(be_u32s(uart.property("interrupts").unwrap().value), vec![0, 33, 1]);
    }
```

- [ ] **Step 2: Run tests, verify they fail**

Run: `cargo test -p ignition-arch fdt 2>&1 | tail -20`
Expected: the four new tests FAIL (`find_node("/intc")`, `/psci`, `/timer`,
`/uart@9000000` return `None`), because `generate` does not emit them yet.

- [ ] **Step 3: Add the remaining node helpers and wire them in**

In `crates/arch/src/aarch64/fdt.rs`, add these helpers (after
`create_chosen_node`):

```rust
fn create_gic_node(fdt: &mut FdtWriter, gic: &GicInfo) -> Result<(), vm_fdt::Error> {
    let intc = fdt.begin_node("intc")?;
    fdt.property_string("compatible", "arm,gic-v3")?;
    fdt.property_null("interrupt-controller")?;
    fdt.property_u32("#interrupt-cells", 3)?;
    fdt.property_array_u64(
        "reg",
        &[gic.dist_base, gic.dist_size, gic.redist_base, gic.redist_size],
    )?;
    fdt.property_u32("phandle", GIC_PHANDLE)?;
    fdt.property_u32("#address-cells", 2)?;
    fdt.property_u32("#size-cells", 2)?;
    fdt.property_null("ranges")?;
    fdt.property_array_u32("interrupts", &[IRQ_TYPE_PPI, gic.maint_irq, IRQ_TYPE_LEVEL_HI])?;
    fdt.end_node(intc)?;
    Ok(())
}

fn create_clock_node(fdt: &mut FdtWriter) -> Result<(), vm_fdt::Error> {
    let clock = fdt.begin_node("apb-pclk")?;
    fdt.property_string("compatible", "fixed-clock")?;
    fdt.property_u32("#clock-cells", 0x0)?;
    fdt.property_u32("clock-frequency", 24_000_000)?;
    fdt.property_string("clock-output-names", "clk24mhz")?;
    fdt.property_u32("phandle", CLOCK_PHANDLE)?;
    fdt.end_node(clock)?;
    Ok(())
}

fn create_timer_node(fdt: &mut FdtWriter) -> Result<(), vm_fdt::Error> {
    // Fixed PPIs for the arm,armv8-timer (secure/non-secure/virtual/hyp).
    let irqs = [13u32, 14, 11, 10];
    let mut cells = Vec::with_capacity(irqs.len() * 3);
    for &irq in irqs.iter() {
        cells.push(IRQ_TYPE_PPI);
        cells.push(irq);
        cells.push(IRQ_TYPE_LEVEL_HI);
    }
    let timer = fdt.begin_node("timer")?;
    fdt.property_string("compatible", "arm,armv8-timer")?;
    fdt.property_null("always-on")?;
    fdt.property_array_u32("interrupts", &cells)?;
    fdt.end_node(timer)?;
    Ok(())
}

fn create_psci_node(fdt: &mut FdtWriter) -> Result<(), vm_fdt::Error> {
    let psci = fdt.begin_node("psci")?;
    fdt.property_string("compatible", "arm,psci-0.2")?;
    // PSCI calls use HVC (we are the hypervisor firmware).
    fdt.property_string("method", "hvc")?;
    fdt.end_node(psci)?;
    Ok(())
}

fn create_serial_node(fdt: &mut FdtWriter, serial: &MmioDev) -> Result<(), vm_fdt::Error> {
    let uart = fdt.begin_node(&format!("uart@{:x}", serial.addr))?;
    fdt.property_string("compatible", "ns16550a")?;
    fdt.property_array_u64("reg", &[serial.addr, serial.size])?;
    fdt.property_u32("clocks", CLOCK_PHANDLE)?;
    fdt.property_string("clock-names", "apb_pclk")?;
    fdt.property_array_u32(
        "interrupts",
        &[IRQ_TYPE_SPI, serial.irq, IRQ_TYPE_EDGE_RISING],
    )?;
    fdt.end_node(uart)?;
    Ok(())
}
```

Then in `generate`, insert the calls after the `create_chosen_node` line, so the
body's node sequence is cpus, memory, chosen, gic, clock, timer, psci, serial:

```rust
    create_gic_node(&mut fdt, &cfg.gic)?;
    create_clock_node(&mut fdt)?;
    create_timer_node(&mut fdt)?;
    create_psci_node(&mut fdt)?;
    create_serial_node(&mut fdt, &cfg.serial)?;
```

- [ ] **Step 4: Run tests, verify all pass**

Run: `cargo test -p ignition-arch fdt 2>&1 | tail -20`
Expected: `test result: ok. 8 passed`.

- [ ] **Step 5: Confirm the whole workspace still builds and is clippy-clean**

Run: `cargo build --workspace 2>&1 | tail -3 && cargo clippy -p ignition-arch 2>&1 | tail -5`
Expected: `Finished`, no errors or warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/arch/src/aarch64/fdt.rs
git commit -m "feat(arch): FDT gic + clock + timer + psci + serial nodes

GICv3 intc (dist/redist reg, 3 interrupt-cells, maintenance irq), 24MHz
apb-pclk, arm,armv8-timer PPIs, psci(hvc), ns16550a uart with SPI interrupt.
Completes the boot-minimal DTB."
```

---

## Self-Review

**Spec coverage:**
- `generate(&FdtConfig) -> Result<Vec<u8>, vm_fdt::Error>` + `MmioDev`/`GicInfo`/`FdtConfig` → Task 1 ✓
- Dependencies (`vm-fdt`, `fdt` dev-dep), module decl → Task 1 ✓
- root node (compatible, cells, interrupt-parent) → Task 1 ✓
- memory@ram → Task 1 ✓
- cpus/cpu@N (psci, mpidr reg, no cache nodes) → Task 2 ✓
- chosen (bootargs, optional initrd) → Task 2 ✓
- intc GICv3 (compatible, reg, #interrupt-cells, phandle, ranges, maint irq) → Task 3 ✓
- apb-pclk, timer (PPIs), psci(hvc), ns16550a uart (reg, clocks, SPI interrupt) → Task 3 ✓
- Testing: parse blob, assert each node/prop, initrd both ways, parser-reads-blob smoke first → Tasks 1-3 ✓
- Dropped nodes (virtio/vmgenid/vmclock/PCI/RTC/rng-seed) → not emitted; nothing to test ✓

**Placeholder scan:** No TBD/TODO-as-work. All code complete. The reader-API caveat in Task 1 is a bounded, instructed adjustment (mirrors how the vm_superio caveat was handled in the prior milestone), not a gap.

**Type consistency:** `FdtConfig`/`MmioDev`/`GicInfo` field names identical across Tasks 1-3 and the `sample()` helper. Helper fns `create_*` signatures match their `generate` call sites. `dt_str`/`be_u32s`/`be_u64s` defined once in Task 1, reused in Tasks 2-3. Node paths in tests (`/cpus/cpu@0`, `/intc`, `/uart@9000000`) match the names emitted by the helpers (`cpu@{i:x}`, `intc`, `uart@{addr:x}` where `0x900_0000` formats as `9000000`). Test interrupt vectors match the constants (`IRQ_TYPE_PPI=1`, `LEVEL_HI=4`, `SPI=0`, `EDGE_RISING=1`).

No issues found.
