# Phase 1 Milestone 2c: kernel loader + boot protocol + layout

Status: approved design, pre-implementation.
Date: 2026-06-12.
Project: ignition (Firecracker → macOS/HVF fork). Follows FDT (2a, merged
3e55306) and in-kernel GIC (2b, merged aee2a2c).

## Goal

Provide the pieces needed to place a guest kernel and its device tree in memory:
the microVM memory layout, an arm64 `Image` header parser, and a `load_kernel`
that copies the Image to the correct guest address and reports the entry point.
This is pure logic in the `arch` crate — no HVF, no real kernel, no signed
binary — fully unit-tested. 2d wires the resulting entry/FDT addresses into a
live VM and boots.

Kernel-boot decomposition: 2a FDT ✓ → 2b GIC ✓ → **2c loader (this)** → 2d
integration boot (fetch+load a real kernel, wire entry/fdt, replace NoIrqVcpus,
earlycon to our 16550) → 2e initramfs/shell.

## Approach

Hand-roll a minimal arm64 `Image` loader rather than pull in `linux-loader`
(which FC uses) + `vm-memory`. Our stack maps guest RAM as a raw host slice, so
the loader operates on `&mut [u8]`; the parser is a pure function over the
64-byte header. This keeps `arch` dependency-free and the whole milestone
unit-testable.

## Location

Two new modules in the `arch` crate, declared from
`crates/arch/src/aarch64/mod.rs`:
- `pub mod layout;` — `crates/arch/src/aarch64/layout.rs`
- `pub mod kernel;` — `crates/arch/src/aarch64/kernel.rs`

No new dependencies.

## Module: `layout`

The microVM memory map. All regions are non-overlapping:
serial MMIO `0x0900_0000` · GIC just below `RAM_BASE` (placed by `HvfGicV3` with
`gic_top = RAM_BASE`) · RAM at `RAM_BASE` · FDT in RAM's top 2 MiB.

```rust
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

/// Where the DTB is placed: the top `FDT_MAX_SIZE` of RAM, rounded DOWN to an
/// 8-byte boundary. Stays within `[RAM_BASE, RAM_BASE + ram_size)` and clear of
/// a kernel loaded at `RAM_BASE` as long as the kernel is smaller than
/// `ram_size - FDT_MAX_SIZE`. `ram_size` MUST be >= `FDT_MAX_SIZE`.
pub fn fdt_addr(ram_size: u64) -> u64 {
    (RAM_BASE + ram_size - FDT_MAX_SIZE) & !0x7
}

/// Default kernel command line. Embeds `SERIAL_BASE` in the earlycon clause so
/// the cmdline's console address always matches the serial device.
pub fn default_cmdline() -> String {
    format!("console=ttyS0 earlycon=uart8250,mmio,{SERIAL_BASE:#x} reboot=k panic=1")
}
```

Note: the DTB must lie within the first 512 MiB of RAM (the kernel maps it early
in boot). With `RAM_BASE = 0x4000_0000` and the small `ram_size` values used
here, the top-of-RAM placement satisfies this. A guard or assertion for larger
RAM is a 2d concern (documented, not enforced here).

## Module: `kernel`

```rust
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
// + a Display impl (no external error crate).

/// The fields we use from the 64-byte arm64 Image header.
#[derive(Debug, PartialEq, Eq)]
pub struct Arm64Header {
    pub text_offset: u64,
    pub image_size: u64,
}

/// Parse the arm64 Image header. Layout (Linux Documentation/arm64/booting.rst):
///   offset 8  : text_offset (LE u64) — load offset from the 2 MiB-aligned base
///   offset 16 : image_size  (LE u64) — effective image size (0 on old kernels)
///   offset 56 : magic       (LE u32) = 0x644D5241
pub fn parse_arm64_header(image: &[u8]) -> Result<Arm64Header, KernelError>;

/// Copy the kernel `image` into guest RAM (`ram` is the host-side mapping of the
/// region based at `ram_base`) at `ram_base + effective_offset`, where
/// `effective_offset` = `text_offset`, or the legacy `0x8_0000` when
/// `image_size == 0`. Returns the guest entry address (`ram_base +
/// effective_offset`). Errors `DoesNotFit` if the image would exceed `ram`.
pub fn load_kernel(ram: &mut [u8], ram_base: u64, image: &[u8]) -> Result<u64, KernelError>;
```

### `parse_arm64_header` algorithm
1. `image.len() < 64` → `TooShort`.
2. `magic = u32::from_le_bytes(image[56..60])`; `!= 0x644D5241` → `BadMagic`.
3. `text_offset = u64::from_le_bytes(image[8..16])`;
   `image_size = u64::from_le_bytes(image[16..24])`.
4. `Ok(Arm64Header { text_offset, image_size })`.

### `load_kernel` algorithm
1. `let h = parse_arm64_header(image)?;`
2. `let effective_offset = if h.image_size == 0 { 0x8_0000 } else { h.text_offset };`
3. `let end = effective_offset as usize + image.len();`
   `if end > ram.len() { return Err(DoesNotFit); }`
4. `ram[effective_offset as usize .. end].copy_from_slice(image);`
5. `Ok(ram_base + effective_offset)`.

(`effective_offset` from a well-formed header is small — `0` or `0x8_0000`; no
overflow concern with the bounds check in step 3.)

## Boot protocol wiring (context, implemented in 2d)

`load_kernel` returns the entry address; `layout::fdt_addr(ram_size)` gives the
DTB address. The existing `HvfVcpu::set_initial_state(entry_addr, fdt_addr)`
(from the UART-echo milestone) already sets `PC = entry`, `X0 = fdt_addr`,
`CPSR = PSTATE_EL1_FAULT_BITS_64` — the arm64 boot-protocol register state. So
2c emits the two addresses and 2d feeds them to `set_initial_state` and writes
the DTB bytes into guest RAM at `fdt_addr`. 2c changes no HVF code.

## Testing (pure `cargo test`, no HVF)

`parse_arm64_header`:
- A synthetic 64-byte header (helper builds it: magic at [56..60], text_offset at
  [8..16], image_size at [16..24]) with `text_offset = 0`, `image_size = 0x1000`
  → `Ok(Arm64Header { 0, 0x1000 })`.
- A 40-byte slice → `TooShort`.
- A 64-byte header with wrong magic → `BadMagic`.

`load_kernel`:
- Modern: header(`text_offset = 0`, `image_size = 0x2000`) followed by a few
  payload bytes, into `vec![0u8; 0x10_0000]`, `ram_base = RAM_BASE` →
  `Ok(RAM_BASE)`, and `ram[0..image.len()] == image`.
- Legacy: header(`image_size = 0`) → `Ok(RAM_BASE + 0x8_0000)`, bytes land at
  offset `0x8_0000`.
- Oversized: image longer than `ram` → `DoesNotFit`, `ram` unchanged.
- Bad image (wrong magic) propagates `BadMagic`.

`layout`:
- `fdt_addr(ram_size)` is 8-byte aligned, `>= RAM_BASE`, `< RAM_BASE + ram_size`,
  and `>= RAM_BASE + kernel_size` for a representative `kernel_size <
  ram_size - FDT_MAX_SIZE` (non-overlap with a kernel at base).
- `default_cmdline()` contains the `SERIAL_BASE` hex string and `earlycon`.

## Out of scope (→ 2d)

Fetching and booting a real kernel; wiring `entry`/`fdt_addr` into
`set_initial_state`; writing the DTB into a live VM's RAM; initrd loading; the
512 MiB-DTB-range guard for large RAM; SMP. `load_kernel`'s `&mut [u8]` signature
lets 2d pass the HVF mmap slice unchanged.

## References to lift from

- Linux `Documentation/arm64/booting.rst` — Image header + boot register contract
- `firecracker/src/vmm/src/arch/aarch64/mod.rs` — `load_kernel` / `get_fdt_addr`
  shape (we hand-roll the equivalent without `linux-loader`)
- `crates/hvf/src/lib.rs` `set_initial_state` — the boot-register state already built
