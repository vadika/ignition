/* Guest fuzz harness: PID 1 in an initramfs. Maps the ignition-fuzz device,
 * parks at the parse site, and drives the reset->inject->run->observe loop via
 * the doorbell. The target is the chunk parser in target.c (a planted length-
 * field heap overflow), built with AddressSanitizer; ASan's death callback rings
 * the CRASH doorbell, with the signal handlers below as a backstop. */
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <unistd.h>
#include "ignition_fuzz.h"

/* The fuzz target lives in target.c (instrumented with AddressSanitizer). */
void target_parse(const uint8_t *data, unsigned long len);

/* Optional one-time per-target setup, run BEFORE the snapshot doorbell so its
 * effects are baked into the snapshot and identical every iteration. Targets
 * that need no setup (synthetic, libpng) omit it and get this weak no-op. */
__attribute__((weak)) void target_init(void) {}

static volatile uint8_t *g_ctrl;   /* control registers (16 KiB) */
static volatile uint8_t *g_win;    /* shared window (input bytes) */
static volatile uint8_t *g_cov;    /* 8-bit SanCov edge counters (host reads) */

static inline void reg_write(unsigned off, uint32_t v) {
    *(volatile uint32_t *)(g_ctrl + off) = v;
}
static inline uint32_t reg_read(unsigned off) {
    return *(volatile uint32_t *)(g_ctrl + off);
}
static inline void doorbell(uint32_t cmd) { reg_write(REG_DOORBELL, cmd); }

/* SanCov edge callback. target.c is built with -fsanitize-coverage=trace-pc, so
 * this fires once per edge with the return address identifying the edge. We hash
 * it into the shared coverage map (8-bit counters) the host reads after DONE.
 * harness.c is NOT coverage-instrumented, or this would recurse into itself.
 * The null-guard matters: the callback can fire during libc/global init, before
 * main() maps g_cov. */
void __sanitizer_cov_trace_pc(void) {
    if (!g_cov) return;
    uintptr_t pc = (uintptr_t)__builtin_return_address(0);
    g_cov[(pc >> 4) & (IGNITION_FUZZ_COV_SIZE - 1)]++;
}

/* On any fatal signal: report a CRASH and spin. The VMM resets PC/regs/RAM on
 * the CRASH doorbell, so this frame is discarded — we never actually return. */
static void crash_handler(int sig) {
    reg_write(REG_CRASH_CODE, (uint32_t)sig);
    doorbell(CMD_CRASH);
    for (;;) { /* VMM resets us out of this loop */ }
}

/* ASan calls this just before aborting on a finding. We ring the CRASH doorbell
 * (the VMM records the input + resets us) instead of letting ASan exit. The
 * signal handlers remain a backstop for faults ASan does not intercept. */
extern void __asan_set_death_callback(void (*cb)(void));

#define CRASH_CODE_ASAN 0x5a  /* nonzero ASan-class marker for CRASH_CODE */

static void asan_on_death(void) {
    reg_write(REG_CRASH_CODE, CRASH_CODE_ASAN);
    doorbell(CMD_CRASH);
    for (;;) { }
}

/* Force ASan to abort (so the death callback fires) and keep it quiet/fast. */
const char *__asan_default_options(void) {
    return "abort_on_error=1:halt_on_error=1:detect_leaks=0";
}

int main(void) {
    mount("proc", "/proc", "proc", 0, 0);   /* for ASan symbolization; ignore errors */

    int fd = open("/dev/mem", O_RDWR | O_SYNC);
    if (fd < 0) return 1;
    g_ctrl = mmap(0, IGNITION_FUZZ_CTRL_SIZE, PROT_READ | PROT_WRITE, MAP_SHARED, fd, IGNITION_FUZZ_CTRL_GPA);
    g_win  = mmap(0, IGNITION_FUZZ_WIN_SIZE,  PROT_READ | PROT_WRITE, MAP_SHARED, fd, IGNITION_FUZZ_WIN_GPA);
    if (g_ctrl == MAP_FAILED || g_win == MAP_FAILED) return 2;
    g_cov = mmap(0, IGNITION_FUZZ_COV_SIZE, PROT_READ | PROT_WRITE, MAP_SHARED, fd, IGNITION_FUZZ_COV_GPA);
    if (g_cov == MAP_FAILED) return 3;

    signal(SIGSEGV, crash_handler);
    signal(SIGABRT, crash_handler);
    signal(SIGBUS,  crash_handler);

    __asan_set_death_callback(asan_on_death);

    target_init();               /* one-time, pre-snapshot (weak no-op by default) */
    /* One-time setup is complete; park at the parse site. */
    doorbell(CMD_SNAPSHOT_ME);   /* <-- snapshot/reset PC lands just after here */

    for (;;) {
        uint32_t len = reg_read(REG_INPUT_LEN);
        if (len > IGNITION_FUZZ_WIN_SIZE) len = IGNITION_FUZZ_WIN_SIZE;
        target_parse((const uint8_t *)g_win, (unsigned long)len);
        doorbell(CMD_DONE);
    }
    return 0;
}
