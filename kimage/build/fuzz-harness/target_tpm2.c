/* TPM 2.0 fuzz target (ms-tpm-20-ref, OpenSSL backend). target_init runs the
 * one-time manufacture/power-on/startup BEFORE the snapshot doorbell, so every
 * fuzz iteration starts from an identical, fully-started TPM. target_parse runs
 * one TPM command through ExecuteCommand against that snapshot; the TPM global
 * state a command mutates (NV image, sessions, objects) is rolled back by the
 * VMM's per-iteration dirty-page reset. Built with ASan (catches the planted
 * handler bug) + SanCov trace-pc (coverage over the command path). */
#include <stdint.h>

/* ms-tpm-20-ref entry points (extern-declared to avoid pulling the full Tpm.h
 * include tree into the harness build). */
extern int  _plat__NVEnable(void *platParameter);
extern void _plat__SetNvAvail(void);
extern void _plat__Signal_PowerOn(void);
extern int  TPM_Manufacture(int firstTime);
extern void _TPM_Init(void);
extern void ExecuteCommand(uint32_t requestSize, unsigned char *request,
                           uint32_t *responseSize, unsigned char **response);

static void send(unsigned char *cmd, uint32_t len) {
    unsigned char rsp[4096];
    unsigned char *rp = rsp;
    uint32_t rlen = sizeof rsp;
    ExecuteCommand(len, cmd, &rlen, &rp);
}

void target_init(void) {
    _plat__NVEnable(0);
    TPM_Manufacture(1);
    _plat__SetNvAvail();
    _plat__Signal_PowerOn();
    _TPM_Init();
    /* TPM2_Startup(TPM_SU_CLEAR): tag=TPM_ST_NO_SESSIONS(0x8001), size=12,
     * cc=TPM2_CC_Startup(0x0144), startupType=TPM_SU_CLEAR(0x0000). */
    unsigned char startup[] = {0x80,0x01, 0,0,0,12, 0,0,0x01,0x44, 0,0};
    send(startup, sizeof startup);
}

void target_parse(const uint8_t *data, unsigned long len) {
    /* PLANTED BUG (fuzz demo gate, see spec). A length-field OOB in the classic
     * CVE shape, standing in for a vulnerable TPM command handler: when the input
     * is a TPM2_NV_Write command (commandCode 0x00000137 at the header's cc slot),
     * copy its size-prefixed payload into a fixed 32-byte scratch with no bound
     * check. ASan traps the stack overflow and the harness rings CRASH; the M1-
     * style gate rediscovers it by mutating the size field past 32. It lives in
     * the wrapper (not a patched upstream handler) so the build stays a clean
     * upstream clone; real handler bugs are the real-CVE stretch. */
    if (len >= 14 &&
        data[6] == 0x00 && data[7] == 0x00 && data[8] == 0x01 && data[9] == 0x37) {
        uint16_t sz = (uint16_t)((data[12] << 8) | data[13]);  /* payload size field */
        volatile unsigned char scratch[32];
        for (uint16_t k = 0; k < sz && (14u + k) <= len; k++)
            scratch[k] = data[14 + k];   /* BUG: writes past scratch when sz > 32 */
    }
    unsigned char rsp[4096];
    unsigned char *rp = rsp;
    uint32_t rlen = sizeof rsp;
    ExecuteCommand((uint32_t)len, (unsigned char *)data, &rlen, &rp);
}
