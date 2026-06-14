// target.c — a tiny "chunk format" parser with a PLANTED, realistic bug.
// Format:  "FUZ" magic | version(1 byte) | chunks...
//   chunk = type(1) | len(2, little-endian) | data[len]
//   type 'C' (copy) copies `len` bytes into a 16-byte heap buffer.
//   BUG: it trusts `len` without bounds-checking -> heap-buffer-overflow.
// This is the archetypal length-field CVE shape (cf. many image/font parsers).
// Compiled WITH -fsanitize=address -fsanitize-coverage=trace-pc (instrumented).
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

volatile uint8_t g_sink;  // observable side-effect so the copy isn't optimized away

void parse(const uint8_t *d, unsigned long n) {
    if (n < 4) return;
    if (d[0] != 'F') return;          // each correct magic byte unlocks a new edge,
    if (d[1] != 'U') return;          // so coverage-guided fuzzing ratchets through
    if (d[2] != 'Z') return;          // the magic far faster than blind random.
    if (d[3] != 1)   return;          // version must be 1
    unsigned long i = 4;
    while (i + 3 <= n) {
        uint8_t  type = d[i];
        uint16_t len  = d[i+1] | (d[i+2] << 8);
        const uint8_t *data = d + i + 3;
        if (i + 3 + len > n) return;  // chunk truncated
        if (type == 'C') {
            uint8_t *buf = malloc(16);
            memcpy(buf, data, len);   // <-- BUG: no check that len <= 16
            for (uint16_t k=0;k<len;k++) g_sink ^= buf[k];  // read buf -> copy is live
            free(buf);
        }
        i += 3 + len;
    }
}
