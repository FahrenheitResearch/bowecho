# weather-contours native engine

This directory contains BowEcho's vendored native Rust contour engine. It is
derived from the `weather-contours` 0.2.0 source archive supplied to the
project on 2026-08-14 (archive SHA-256
`E9B7F22646BCB2C4938A10B1CE98D33705C41BFB75A134DF28C4E8CE9730E453`).
The pristine 99,948-byte archive is retained at
`upstream/autumnplot-weather-contours-sota.zip`; [PATCHES.md](PATCHES.md)
records the exact import scope and BowEcho changes.

The vendored foundation intentionally includes only the dependency-free native
Rust isoline and isoband implementation. The archive's browser shaders,
TypeScript integration, worker, raw WebAssembly ABI, demos, and benchmarks are
not part of this crate.

BowEcho-specific hardening includes stable degenerate-saddle topology,
canonical removal of exact zero-length output segments, configurable checked
resource budgets, fallible large allocations, stricter rectilinear-coordinate
validation, and regression tests for those contracts.

## BowEcho integration benchmark

The production adapter was measured on three native 1799x1059 HRRR fields
(Windows 11, Ryzen 9 9950X3D, Rust 1.94, fat LTO, 5 warmups and 25 measured
iterations). Times include packed-path extraction and conversion back to
BowEcho's existing `ContourSegment` renderer input.

| Field | This engine p50 / p95 | Removed BowEcho loop p50 / p95 | p50 speedup |
| --- | ---: | ---: | ---: |
| 500-hPa height | 33.813 / 34.465 ms | 795.449 / 857.918 ms | 23.52x |
| 2-m temperature | 79.981 / 85.963 ms | 1,005.590 / 1,097.749 ms | 12.57x |
| Composite reflectivity | 46.760 / 48.031 ms | 592.171 / 601.096 ms | 12.66x |

All three adapted outputs contained zero collapsed and zero non-finite
segments. BowEcho currently uses this crate for connected upper-air isolines;
its existing filled-contour and direct-raster paths remain unchanged because
COBRM is not a universal end-to-end speedup or a drop-in renderer replacement.

See [LICENSE](LICENSE), [PATCHES.md](PATCHES.md), and BowEcho's
`THIRD-PARTY-NOTICES.md`.
