/* M0 guest fuzz harness: PID 1 in an initramfs. Maps the ignition-fuzz device,
 * parks at the parse site, and drives the reset->inject->run->observe loop via
 * the doorbell. The "target" is a stub parser that overflows on a magic byte so
 * the M0 gate can plant a deterministic crash. */
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>
#include "ignition_fuzz.h"

static volatile uint8_t *g_ctrl;   /* control registers (16 KiB) */
static volatile uint8_t *g_win;    /* shared window (input bytes) */

static inline void reg_write(unsigned off, uint32_t v) {
    *(volatile uint32_t *)(g_ctrl + off) = v;
}
static inline uint32_t reg_read(unsigned off) {
    return *(volatile uint32_t *)(g_ctrl + off);
}
static inline void doorbell(uint32_t cmd) { reg_write(REG_DOORBELL, cmd); }

/* On any fatal signal: report a CRASH and spin. The VMM resets PC/regs/RAM on
 * the CRASH doorbell, so this frame is discarded — we never actually return. */
static void crash_handler(int sig) {
    reg_write(REG_CRASH_CODE, (uint32_t)sig);
    doorbell(CMD_CRASH);
    for (;;) { /* VMM resets us out of this loop */ }
}

/* The M0 stub target. A real target (libpng) replaces this in M1. */
static void target_parse(const uint8_t *data, uint32_t len) {
    char buf[16];
    if (len > 0 && data[0] == 0xFF) {
        /* Deterministic fatal fault -> SIGSEGV -> crash_handler -> CMD_CRASH.
         * NB: a plain `char buf[N]; memset(buf, .., len+64)` is dead-store-
         * eliminated at -O2 (buf is never read), so the planted bug vanishes and
         * the guest never faults. Force a real, un-removable wild write instead:
         * spray 0xAA past `buf` through a volatile pointer so the compiler must
         * emit the stores; with a large `len` this walks off the stack into an
         * unmapped page and faults. The volatile sink also defeats DSE. */
        volatile char *p = buf;
        size_t n = (size_t)len + 64;
        for (size_t i = 0; i < n; i++) p[i] = (char)0xAA;
        /* Fallback guarantee: a NULL deref always faults even if the loop above
         * somehow stayed in mapped memory. Unreachable on a real fault. */
        *(volatile char *)0 = p[n - 1];
    } else {
        /* touch the input so the read is real work */
        volatile uint8_t acc = 0;
        for (uint32_t i = 0; i < len && i < sizeof(buf); i++) acc ^= data[i];
        (void)acc;
    }
}

int main(void) {
    int fd = open("/dev/mem", O_RDWR | O_SYNC);
    if (fd < 0) return 1;
    g_ctrl = mmap(0, IGNITION_FUZZ_CTRL_SIZE, PROT_READ | PROT_WRITE, MAP_SHARED, fd, IGNITION_FUZZ_CTRL_GPA);
    g_win  = mmap(0, IGNITION_FUZZ_WIN_SIZE,  PROT_READ | PROT_WRITE, MAP_SHARED, fd, IGNITION_FUZZ_WIN_GPA);
    if (g_ctrl == MAP_FAILED || g_win == MAP_FAILED) return 2;

    signal(SIGSEGV, crash_handler);
    signal(SIGABRT, crash_handler);
    signal(SIGBUS,  crash_handler);

    /* One-time setup is complete; park at the parse site. */
    doorbell(CMD_SNAPSHOT_ME);   /* <-- snapshot/reset PC lands just after here */

    for (;;) {
        uint32_t len = reg_read(REG_INPUT_LEN);
        if (len > IGNITION_FUZZ_WIN_SIZE) len = IGNITION_FUZZ_WIN_SIZE;
        target_parse((const uint8_t *)g_win, len);
        doorbell(CMD_DONE);
    }
    return 0;
}
