/* target_png.c — real-target fuzz body for the M3 benchmark: decode the window
 * bytes as a PNG via libpng's simplified read API. Built with SanCov
 * (-fsanitize-coverage=trace-pc) but WITHOUT AddressSanitizer (the M3 throughput
 * build is coverage-only per spec §12; the ASan build uses the synthetic
 * target.c). Same `target_parse` signature the harness calls. No planted bug —
 * this measures the snapshot machinery against a real decoder. */
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <png.h>

void target_parse(const uint8_t *d, unsigned long n) {
    png_image image;
    memset(&image, 0, sizeof image);
    image.version = PNG_IMAGE_VERSION;

    if (!png_image_begin_read_from_memory(&image, d, (size_t)n)) {
        return;  /* not a PNG / header rejected */
    }
    image.format = PNG_FORMAT_RGBA;

    /* Bound the allocation so a malformed huge dimension does not OOM the guest;
     * 64 MiB ceiling keeps us inside the 96-128 MiB guest RAM. */
    png_alloc_size_t sz = PNG_IMAGE_SIZE(image);
    if (sz == 0 || sz > (64u << 20)) {
        png_image_free(&image);
        return;
    }
    void *buf = malloc((size_t)sz);
    if (!buf) {
        png_image_free(&image);
        return;
    }
    png_image_finish_read(&image, NULL /*background*/, buf, 0 /*row_stride*/, NULL /*colormap*/);
    free(buf);
}
