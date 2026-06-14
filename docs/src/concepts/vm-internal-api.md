# VM internal API (MMIO)

Guest code talks to the VMM through fixed guest-physical MMIO regions. No virtio,
no syscall, no shared filesystem: the guest maps a device's region from `/dev/mem`
at a known GPA and reads/writes registers directly. The VMM either traps the access
(control registers) or hands the guest plain RAM it also maps host-side (data
windows).

Two devices expose this interface today: the **boot-timer** (a one-shot signal) and
the **fuzz device** (a full control protocol). Both regions sit below `RAM_BASE`
(`0x4000_0000`) so they are outside guest RAM and outside snapshot/reset tracking.

Access rules for guests:

- `mmap` the containing page of `/dev/mem` at the region's GPA. Offsets must be
  16 KiB-aligned (the guest page granule), which every GPA below already is.
- Use a single naturally-sized access at the register offset. The width matters for
  trap-MMIO registers, so `dd` is not a substitute. A `devmem`-style tool or a typed
  `volatile` store is correct.

## Boot-timer

A one-shot pseudo-device. The guest writes the magic byte **123** as an 8-bit store
to offset 0 once at the end of boot; the VMM records elapsed wall time since VM start
and logs `Guest-boot-time = N ms`. Repeat writes are ignored. No FDT node, no
interrupt, no snapshot state.

| Field | Value |
|-------|-------|
| GPA | `0x091F_F000` |
| Access | 8-bit write, offset 0 |
| Magic value | `123` |

The stock rootfs signals it from `/etc/local.d/boottime.start`:

```console
devmem 0x091FF000 8 123
```

The equivalent in C (the `devmem` tool's core: map the page, do one `uint8_t` store):

```c
#include <fcntl.h>
#include <stdint.h>
#include <sys/mman.h>
#include <unistd.h>

#define BOOT_TIMER_GPA 0x091FF000UL
#define BOOT_COMPLETE  123

int main(void) {
    int fd = open("/dev/mem", O_RDWR | O_SYNC);
    if (fd < 0) return 1;
    /* map the 16 KiB page containing the register */
    volatile uint8_t *reg = mmap(0, 0x4000, PROT_READ | PROT_WRITE,
                                 MAP_SHARED, fd, BOOT_TIMER_GPA);
    if (reg == MAP_FAILED) return 1;
    reg[0] = BOOT_COMPLETE;   /* single 8-bit store -> VMM logs the boot time */
    return 0;
}
```

## Fuzz device

The fuzz device carries the in-VMM fuzzing loop. It has three regions: a trapping
**control** region (registers), a RAM-backed **input window** (host writes the input,
guest reads it), and a RAM-backed **coverage** map (guest writes 8-bit SanCov edge
counters, host reads them). The canonical constants live in
`crates/devices/src/fuzz/protocol.rs`; the guest mirror is
`kimage/build/fuzz-harness/ignition_fuzz.h`.

### Memory map

| Region | GPA | Size | Backing |
|--------|-----|------|---------|
| Control registers | `0x0920_0000` | 16 KiB | trap-MMIO |
| Input window | `0x0920_4000` | 2 MiB (default) | shared RAM |
| Coverage map | `0x0940_4000` | 64 KiB | shared RAM |

### Control registers

| Offset | Name | Access | Meaning |
|--------|------|--------|---------|
| `0x00` | `DOORBELL` | W | guest writes a command code; the store traps to the VMM |
| `0x04` | `INPUT_LEN` | RW | length of the current input in the window (host writes, guest reads) |
| `0x08` | `CRASH_CODE` | W | abort/sanitizer reason class, written before a CRASH doorbell |
| `0x0c` | `STATUS` | R | VMM-to-guest handshake (optional) |

### Doorbell commands (guest → VMM)

| Code | Name | Meaning |
|------|------|---------|
| `0x1` | `SNAPSHOT_ME` | one-time setup complete, parked at the parse site; first receipt captures the snapshot |
| `0x2` | `DONE` | input processed cleanly |
| `0x3` | `CRASH` | target crashed (rung from the sanitizer/signal handler) |

### Guest harness (C)

The harness maps the three regions, then loops: read the input length, run the
target over the window, ring `DONE`. The VMM resets the guest to the snapshot after
each doorbell. Excerpt from `kimage/build/fuzz-harness/harness.c`:

```c
#include "ignition_fuzz.h"

static volatile uint8_t *g_ctrl;   /* control registers */
static volatile uint8_t *g_win;    /* input window      */
static volatile uint8_t *g_cov;    /* coverage counters */

static inline void reg_write(unsigned off, uint32_t v) {
    *(volatile uint32_t *)(g_ctrl + off) = v;
}
static inline uint32_t reg_read(unsigned off) {
    return *(volatile uint32_t *)(g_ctrl + off);
}
static inline void doorbell(uint32_t cmd) { reg_write(REG_DOORBELL, cmd); }

int main(void) {
    int fd = open("/dev/mem", O_RDWR | O_SYNC);
    g_ctrl = mmap(0, IGNITION_FUZZ_CTRL_SIZE, PROT_READ | PROT_WRITE, MAP_SHARED, fd, IGNITION_FUZZ_CTRL_GPA);
    g_win  = mmap(0, IGNITION_FUZZ_WIN_SIZE,  PROT_READ | PROT_WRITE, MAP_SHARED, fd, IGNITION_FUZZ_WIN_GPA);
    g_cov  = mmap(0, IGNITION_FUZZ_COV_SIZE,  PROT_READ | PROT_WRITE, MAP_SHARED, fd, IGNITION_FUZZ_COV_GPA);

    /* one-time setup is done; park here -- the snapshot/reset PC lands just after. */
    doorbell(CMD_SNAPSHOT_ME);

    for (;;) {
        uint32_t len = reg_read(REG_INPUT_LEN);
        if (len > IGNITION_FUZZ_WIN_SIZE) len = IGNITION_FUZZ_WIN_SIZE;
        target_parse((const uint8_t *)g_win, (unsigned long)len);  /* the code under test */
        doorbell(CMD_DONE);
    }
}
```

A crash is reported the same way, from a sanitizer death callback or a fatal-signal
handler, before the VMM rolls the guest back:

```c
static void on_crash(int reason) {
    reg_write(REG_CRASH_CODE, (uint32_t)reason);
    doorbell(CMD_CRASH);
    for (;;) { /* the VMM resets us out of this loop */ }
}
```

## Related

- [Device model](device-model.md) — how these devices register on the MMIO bus.
- [How snapshot fuzzing works](../fuzzing/overview.md) — the loop the fuzz device drives.
- [Running the fuzzer](../fuzzing/running.md) — building and driving the harness.
