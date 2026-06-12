# Phase 1 Milestone 2c: kernel loader + layout Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Provide the microVM memory layout and a hand-rolled arm64 `Image` loader (header parse + copy into guest RAM, returning the entry address), so 2d can place a real kernel + DTB and boot.

**Architecture:** Two pure modules in the `arch` crate — `layout` (memory-map constants + `fdt_addr`/`default_cmdline` helpers) and `kernel` (`parse_arm64_header` + `load_kernel`). No HVF, no real kernel, no new dependencies; everything is verified with `cargo test` using synthetic headers and a `Vec`-as-RAM. `load_kernel` operates on `&mut [u8]` so 2d passes the HVF mmap slice unchanged.

**Tech Stack:** Rust edition 2024, std only.

**Commit convention for this project:** plain commit messages, NO `Co-Authored-By` / "Generated with Claude" trailer.

---

## File Structure

- `crates/arch/src/aarch64/mod.rs` — add `pub mod layout;` and `pub mod kernel;`
- `crates/arch/src/aarch64/layout.rs` — **create**: memory-map constants + `fdt_addr`/`default_cmdline` + tests
- `crates/arch/src/aarch64/kernel.rs` — **create**: `KernelError`, `Arm64Header`, `parse_arm64_header`, `load_kernel` + tests

Both are self-contained pure modules; impl + tests land together per module (the "red" for a new pure module is a compile failure of the test module — there is no separate runtime red phase). No HVF, so plain `cargo test`.

---

## Task 1: `layout` module

**Files:**
- Modify: `crates/arch/src/aarch64/mod.rs`
- Create: `crates/arch/src/aarch64/layout.rs`

- [ ] **Step 1: Declare the module**

In `crates/arch/src/aarch64/mod.rs`, add at the end (after `pub mod fdt;`):

```rust
pub mod layout;
```

- [ ] **Step 2: Create `crates/arch/src/aarch64/layout.rs`**

```rust
// Memory map for the ignition aarch64 microVM. Regions are non-overlapping:
// serial MMIO at SERIAL_BASE, the GIC just below RAM_BASE (placed by HvfGicV3
// with gic_top = RAM_BASE), guest RAM at RAM_BASE, and the FDT in RAM's top
// FDT_MAX_SIZE.

/// Guest RAM base (1 GiB). The GIC sits just below this (gic_top = RAM_BASE).
pub const RAM_BASE: u64 = 0x4000_0000;
/// 16550 serial MMIO window.
pub const SERIAL_BASE: u64 = 0x0900_0000;
pub const SERIAL_SIZE: u64 = 0x1000;
/// Serial interrupt as the bare GIC SPI index written into the FDT
/// (absolute INTID = 32 + this; index 0 -> INTID 32, confirmed by gic-smoke).
pub const SERIAL_SPI: u32 = 0;
/// Reserved size for the flattened device tree.
pub const FDT_MAX_SIZE: u64 = 0x20_0000; // 2 MiB

/// Address where the DTB is placed: the top `FDT_MAX_SIZE` of RAM, rounded down
/// to an 8-byte boundary. Within `[RAM_BASE, RAM_BASE + ram_size)` and clear of
/// a kernel at `RAM_BASE` while the kernel stays smaller than
/// `ram_size - FDT_MAX_SIZE`. `ram_size` must be >= `FDT_MAX_SIZE`.
pub fn fdt_addr(ram_size: u64) -> u64 {
    (RAM_BASE + ram_size - FDT_MAX_SIZE) & !0x7
}

/// Default kernel command line, with the earlycon MMIO address kept in sync with
/// `SERIAL_BASE`.
pub fn default_cmdline() -> String {
    format!("console=ttyS0 earlycon=uart8250,mmio,{SERIAL_BASE:#x} reboot=k panic=1")
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
    fn default_cmdline_references_serial_base() {
        let cmdline = default_cmdline();
        assert!(cmdline.contains(&format!("{SERIAL_BASE:#x}")), "cmdline: {cmdline}");
        assert!(cmdline.contains("earlycon"), "cmdline: {cmdline}");
    }

    #[test]
    fn serial_window_is_below_ram() {
        // serial sits well below the GIC, which sits just below RAM.
        assert!(SERIAL_BASE + SERIAL_SIZE <= RAM_BASE);
    }
}
```

- [ ] **Step 3: Run the tests, verify they pass**

Run: `cargo test -p ignition-arch layout 2>&1 | tail -15`
Expected: `test result: ok. 3 passed`.

- [ ] **Step 4: Commit**

```bash
git add crates/arch/src/aarch64/mod.rs crates/arch/src/aarch64/layout.rs
git commit -m "feat(arch): microVM memory layout module

RAM/serial/GIC/FDT placement constants, fdt_addr (top of RAM, 8-byte
aligned) and default_cmdline (earlycon synced to SERIAL_BASE). Unit-tested
non-overlap + alignment."
```

---

## Task 2: `kernel` module (arm64 Image loader)

**Files:**
- Modify: `crates/arch/src/aarch64/mod.rs`
- Create: `crates/arch/src/aarch64/kernel.rs`

- [ ] **Step 1: Declare the module**

In `crates/arch/src/aarch64/mod.rs`, add at the end (after `pub mod layout;`):

```rust
pub mod kernel;
```

- [ ] **Step 2: Create `crates/arch/src/aarch64/kernel.rs`**

```rust
// Minimal arm64 Linux `Image` loader. Parses the 64-byte image header and copies
// the image into guest RAM at the address the boot protocol requires.
//
// Header layout (Linux Documentation/arm64/booting.rst):
//   offset 8  : text_offset (LE u64)  load offset from the 2 MiB-aligned base
//   offset 16 : image_size  (LE u64)  effective image size (0 on old kernels)
//   offset 56 : magic       (LE u32)  = 0x644D5241 ("ARM\x64")

use std::fmt::{self, Display, Formatter};

const ARM64_IMAGE_MAGIC: u32 = 0x644D_5241;
const ARM64_HEADER_LEN: usize = 64;
/// Load offset assumed by kernels that report image_size == 0 (pre-3.17).
const LEGACY_TEXT_OFFSET: u64 = 0x8_0000;

/// Errors loading an arm64 Image.
#[derive(Debug, PartialEq, Eq)]
pub enum KernelError {
    /// Image shorter than the 64-byte arm64 header.
    TooShort,
    /// Header magic was not 0x644D5241 ("ARM\x64").
    BadMagic,
    /// Image does not fit in the supplied guest RAM at its load offset.
    DoesNotFit,
}

impl Display for KernelError {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match self {
            KernelError::TooShort => write!(f, "kernel image shorter than the arm64 header"),
            KernelError::BadMagic => write!(f, "kernel image has a bad arm64 magic number"),
            KernelError::DoesNotFit => write!(f, "kernel image does not fit in guest RAM"),
        }
    }
}

/// The fields we use from the 64-byte arm64 Image header.
#[derive(Debug, PartialEq, Eq)]
pub struct Arm64Header {
    pub text_offset: u64,
    pub image_size: u64,
}

/// Parse the arm64 Image header from the start of `image`.
pub fn parse_arm64_header(image: &[u8]) -> Result<Arm64Header, KernelError> {
    if image.len() < ARM64_HEADER_LEN {
        return Err(KernelError::TooShort);
    }
    let magic = u32::from_le_bytes(image[56..60].try_into().unwrap());
    if magic != ARM64_IMAGE_MAGIC {
        return Err(KernelError::BadMagic);
    }
    let text_offset = u64::from_le_bytes(image[8..16].try_into().unwrap());
    let image_size = u64::from_le_bytes(image[16..24].try_into().unwrap());
    Ok(Arm64Header { text_offset, image_size })
}

/// Copy `image` into guest RAM (`ram` is the host mapping of the region based at
/// `ram_base`) at `ram_base + effective_offset`, where `effective_offset` is the
/// header's `text_offset`, or `LEGACY_TEXT_OFFSET` when `image_size == 0`.
/// Returns the guest entry address.
pub fn load_kernel(ram: &mut [u8], ram_base: u64, image: &[u8]) -> Result<u64, KernelError> {
    let header = parse_arm64_header(image)?;
    let offset = if header.image_size == 0 {
        LEGACY_TEXT_OFFSET
    } else {
        header.text_offset
    } as usize;

    let end = offset.checked_add(image.len()).ok_or(KernelError::DoesNotFit)?;
    if end > ram.len() {
        return Err(KernelError::DoesNotFit);
    }
    ram[offset..end].copy_from_slice(image);
    Ok(ram_base + offset as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic 64-byte arm64 header with the given fields.
    fn header(text_offset: u64, image_size: u64) -> Vec<u8> {
        let mut h = vec![0u8; ARM64_HEADER_LEN];
        h[8..16].copy_from_slice(&text_offset.to_le_bytes());
        h[16..24].copy_from_slice(&image_size.to_le_bytes());
        h[56..60].copy_from_slice(&ARM64_IMAGE_MAGIC.to_le_bytes());
        h
    }

    #[test]
    fn parse_reads_text_offset_and_image_size() {
        let h = header(0, 0x1000);
        assert_eq!(
            parse_arm64_header(&h),
            Ok(Arm64Header { text_offset: 0, image_size: 0x1000 })
        );
    }

    #[test]
    fn parse_rejects_short_image() {
        assert_eq!(parse_arm64_header(&[0u8; 40]), Err(KernelError::TooShort));
    }

    #[test]
    fn parse_rejects_bad_magic() {
        let mut h = header(0, 0x1000);
        h[56] ^= 0xff; // corrupt the magic
        assert_eq!(parse_arm64_header(&h), Err(KernelError::BadMagic));
    }

    #[test]
    fn load_modern_kernel_at_base() {
        let mut image = header(0, 0x2000);
        image.extend_from_slice(&[0xaa, 0xbb, 0xcc, 0xdd]);
        let mut ram = vec![0u8; 0x10_0000];
        let entry = load_kernel(&mut ram, 0x4000_0000, &image).unwrap();
        assert_eq!(entry, 0x4000_0000);
        assert_eq!(&ram[..image.len()], image.as_slice());
    }

    #[test]
    fn load_legacy_kernel_at_0x80000() {
        let image = header(0, 0); // image_size == 0 -> legacy offset
        let mut ram = vec![0u8; 0x10_0000];
        let entry = load_kernel(&mut ram, 0x4000_0000, &image).unwrap();
        assert_eq!(entry, 0x4000_0000 + 0x8_0000);
        assert_eq!(&ram[0x8_0000..0x8_0000 + image.len()], image.as_slice());
    }

    #[test]
    fn load_rejects_oversized_image() {
        let mut image = header(0, 0x100); // image_size != 0 -> offset 0
        image.resize(ARM64_HEADER_LEN + 64, 0); // 128 bytes total
        let mut ram = vec![0u8; 64]; // smaller than the image
        assert_eq!(
            load_kernel(&mut ram, 0x4000_0000, &image),
            Err(KernelError::DoesNotFit)
        );
        assert!(ram.iter().all(|&b| b == 0), "ram must be unchanged on failure");
    }

    #[test]
    fn load_propagates_bad_magic() {
        let mut image = header(0, 0x100);
        image[56] ^= 0xff;
        let mut ram = vec![0u8; 0x1000];
        assert_eq!(
            load_kernel(&mut ram, 0x4000_0000, &image),
            Err(KernelError::BadMagic)
        );
    }
}
```

- [ ] **Step 3: Run the tests, verify they pass**

Run: `cargo test -p ignition-arch kernel 2>&1 | tail -20`
Expected: `test result: ok. 7 passed`.

- [ ] **Step 4: Confirm the whole crate is clippy-clean**

Run: `cargo clippy -p ignition-arch 2>&1 | tail -5`
Expected: `Finished`, no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/arch/src/aarch64/mod.rs crates/arch/src/aarch64/kernel.rs
git commit -m "feat(arch): arm64 Image loader

parse_arm64_header (magic/text_offset/image_size) and load_kernel (copy the
Image into guest RAM at ram_base + text_offset, or the legacy 0x80000 when
image_size==0; returns the entry address). Unit-tested modern/legacy/oversized."
```

---

## Self-Review

**Spec coverage:**
- `layout` constants (RAM_BASE, SERIAL_BASE/SIZE/SPI, FDT_MAX_SIZE) → Task 1 ✓
- `fdt_addr(ram_size)` (top of RAM, 8-byte aligned) → Task 1 ✓
- `default_cmdline()` (earlycon synced to SERIAL_BASE) → Task 1 ✓
- `KernelError { TooShort, BadMagic, DoesNotFit }` + Display → Task 2 ✓
- `Arm64Header { text_offset, image_size }` → Task 2 ✓
- `parse_arm64_header` (len check, magic @56, fields @8/@16) → Task 2 ✓
- `load_kernel` (legacy 0x80000 when image_size==0, fit check, copy, return entry) → Task 2 ✓
- Tests: parse (ok/short/bad-magic), load (modern/legacy/oversized/bad-magic), layout (aligned/within-RAM/non-overlap, cmdline) → Tasks 1,2 ✓
- Out-of-scope items (real kernel fetch/boot, HVF wiring, DTB write, initrd) → not implemented ✓

**Placeholder scan:** No TBD/TODO-as-work. All code complete, all tests have real assertions.

**Type consistency:** `KernelError`/`Arm64Header` names + variants consistent between definition, `parse_arm64_header`, `load_kernel`, and the tests. `parse_arm64_header(&[u8]) -> Result<Arm64Header, KernelError>` and `load_kernel(&mut [u8], u64, &[u8]) -> Result<u64, KernelError>` signatures match all call sites. Constants `ARM64_IMAGE_MAGIC`/`ARM64_HEADER_LEN`/`LEGACY_TEXT_OFFSET` used consistently in impl and the test `header()` helper. `layout` consts (`RAM_BASE`, `SERIAL_BASE`, `SERIAL_SIZE`, `FDT_MAX_SIZE`) used consistently in `fdt_addr`/`default_cmdline`/tests. The `0x8_0000` legacy offset matches between `load_kernel` and its test.

No issues found.
