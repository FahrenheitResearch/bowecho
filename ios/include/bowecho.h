#ifndef BOWECHO_H
#define BOWECHO_H

#include <stdint.h>
#include <stddef.h>

/* A rendered radar frame plus the georeferencing the map overlay needs.
 * The raster is a square image centered on the radar site; half_width_m /
 * half_height_m are the ground half-extents (meters) from center to edge.
 * Layout MUST match `BowEchoRender` in crates/bowecho_ffi/src/lib.rs. */
typedef struct BowEchoRender {
    uint8_t *rgba;
    size_t   len;
    uint32_t width;
    uint32_t height;
    double   center_lat;
    double   center_lon;
    double   half_width_m;
    double   half_height_m;
    int64_t  volume_time_unix;
} BowEchoRender;

/* Last error for the calling thread (or NULL). Copy immediately. */
const char *bowecho_last_error(void);

/* Fetch + decode + render the latest volume for `site` (e.g. "KTLX").
 * moment_code: 0=Reflectivity 1=Velocity 2=SpectrumWidth 3=ZDR 4=CC 5=PHI 6=KDP.
 * size_px: square raster dimension (clamped 256..=8192).
 * cache_dir: writable directory (the app's Caches dir).
 * Returns 0 on success (fills *out), negative on error. BLOCKS — call off-main. */
int bowecho_render_latest(const char *site,
                          int moment_code,
                          uint32_t size_px,
                          const char *cache_dir,
                          BowEchoRender *out);

/* Free the RGBA buffer owned by a BowEchoRender. */
void bowecho_render_free(BowEchoRender *out);

#endif /* BOWECHO_H */
