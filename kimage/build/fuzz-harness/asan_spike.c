/* Throwaway: prove a sanitizer-instrumented binary runs in the guest and
 * detects a heap overflow. Writes a result byte to the boot-timer-style console
 * via stdout (the kernel console) so we can see it on the serial log. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

int main(void) {
    printf("ASAN_SPIKE: start\n");
    fflush(stdout);
    char *buf = malloc(16);
    /* 17-byte write into a 16-byte buffer: ASan (or a guard page) must catch it. */
    memset(buf, 0xAA, 17);
    printf("ASAN_SPIKE: no-detection (BAD) sink=%d\n", buf[16]);
    fflush(stdout);
    free(buf);
    return 0;
}
