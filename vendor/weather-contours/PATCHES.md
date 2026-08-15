# BowEcho vendor lineage

The pristine source supplied for this integration is preserved byte-for-byte
at `upstream/autumnplot-weather-contours-sota.zip`.

- Archive SHA-256: `E9B7F22646BCB2C4938A10B1CE98D33705C41BFB75A134DF28C4E8CE9730E453`
- Imported native crate: `rust/weather-contours` version 0.2.0
- Imported on: 2026-08-14

BowEcho intentionally excludes the archive's TypeScript, WebGL, worker,
WebAssembly ABI, demo, and integration scripts from the compiled dependency.
The unmodified archive remains here so the vendored Rust code and every local
change can be reconstructed without relying on a transient Downloads folder.

BowEcho's native fork adds:

- deterministic degenerate-saddle topology independent of requested-level
  order and cropped-grid origin;
- exact duplicate-vertex removal and rejection of non-drawable paths;
- half-open interior isoband ownership, with the final upper bound closed;
- finite and strictly monotonic rectilinear-axis validation;
- configurable resource limits and fallible large allocations;
- panic-free validation of public packed geometry;
- additional regression and bounded randomized-invariant tests; and
- native-only `rlib` packaging with no browser or WebAssembly build surface.

The original MIT license is retained in `LICENSE` and reproduced in BowEcho's
top-level `THIRD-PARTY-NOTICES.md`.
