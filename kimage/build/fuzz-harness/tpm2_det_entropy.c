/* Deterministic entropy source for reproducible TPM fuzzing. Replaces
 * ms-tpm-20-ref's Platform/src/Entropy.c (_plat__GetEntropy), which reads the
 * host RNG -- nondeterministic, and /dev/urandom may not exist in the fuzz
 * initramfs. A fixed counter makes same-snapshot + same-input runs reproducible
 * (required for crash replay). Swapped into libplatform.a via `ar`. */
#include <stdint.h>

int32_t _plat__GetEntropy(unsigned char *entropy, uint32_t amount) {
    static uint8_t counter = 0;
    for (uint32_t i = 0; i < amount; i++)
        entropy[i] = counter++;
    return (int32_t)amount;
}
