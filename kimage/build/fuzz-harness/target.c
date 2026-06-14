/* target.c — a chunk-format parser with a PLANTED length-field heap overflow.
 * Format: "FUZ" magic | version(1) | chunks; chunk = type(1) | len(2 LE) | data[len].
 * Type 'C' copies `len` bytes into a 16-byte heap buffer with NO bound check
 * (the archetypal image/font CVE shape). Built with AddressSanitizer (M1 Task 3)
 * so the overflow is caught deterministically. */
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

volatile uint8_t g_sink;  /* observable side-effect so the copy isn't elided */

void target_parse(const uint8_t *d, unsigned long n) {
    if (n < 4) return;
    if (d[0] != 'F' || d[1] != 'U' || d[2] != 'Z' || d[3] != 1) return;
    unsigned long i = 4;
    while (i + 3 <= n) {
        uint8_t  type = d[i];
        uint16_t len  = (uint16_t)(d[i + 1] | (d[i + 2] << 8));
        const uint8_t *data = d + i + 3;
        if (i + 3 + (unsigned long)len > n) return;     /* chunk truncated */
        if (type == 'C') {
            uint8_t *buf = malloc(16);
            memcpy(buf, data, len);                     /* BUG: len may exceed 16 */
            for (uint16_t k = 0; k < len; k++) g_sink ^= buf[k];  /* read -> live */
            free(buf);
        }
        i += 3 + (unsigned long)len;
    }
}
