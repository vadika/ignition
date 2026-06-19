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
    unsigned char rsp[4096];
    unsigned char *rp = rsp;
    uint32_t rlen = sizeof rsp;
    ExecuteCommand((uint32_t)len, (unsigned char *)data, &rlen, &rp);
}
