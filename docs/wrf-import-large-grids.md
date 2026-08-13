# WRF full-diagnostics import on large grids — crash forensics + memory fix

**Scope:** the "Process WRF" full-diagnostics import (`crates/app_ui/src/wrf_process.rs`
→ `read_wrf_products` → ~80 `wrf-core` `getvar` diagnostics), FABLE_BACKLOG issue #9.
Investigated 2026-07-06 on the real Enderlin 250 m wrfouts
(`wrfout_d03_*`, 800×800×79 ≈ 50.5 M cells, ~2 GB each).
The LIGHT import path got the same treatment on 2026-07-06 — see the
"Light import" section at the end.

## Symptom as reported

The default full-diagnostics pass on the 2 GB Enderlin file "aborts `0xffffffff`
at ~5.9 GB working set" on a 64 GB machine; the live release app also died
mid-import.

## What the investigation established (real data, release builds)

1. **The import completes.** An instrumented release run
   (`wrf_process::tests::optional_real_fixture_default_import_instrumented`,
   env-gated on `RW_WRF_CRASH_FIXTURE`, per-diagnostic progress to stderr) of the
   exact dock-button path on `wrfout_d03_2025-06-21_02_15_00` finished in
   **274.9 s** on a quiet machine, wrote **117 variables** (all core fields +
   `*_iso` sounding volumes + the full severe-diagnostic suite), exit 0. The
   2.50 GB `05_15_00` derecho file (the family the live app was loading)
   also completes.
2. **`0xffffffff` is not a Rust failure code.** Probed on this exact
   toolchain (MSVC): `std::process::abort` and allocation failure exit
   `0xC0000409`, stack overflow exits `0xC00000FD`, a plain panic exits `101`,
   and a panic on a non-main thread doesn't kill the process at all. The ONLY
   mechanism that produces `0xffffffff` is `std::process::exit(-1)` /
   `TerminateProcess(h, -1)` — i.e. an **external kill**. No `process::exit`
   is reachable from the import path in this workspace, wrf-core, ecape-rs, or
   the rusty-weather crates (grep-verified). The observed abort matches an
   agent-tool timeout killing a **debug-profile** run (debug is 20–40× slower:
   ~2–3 h for this pass; at a 10-minute kill a debug run sits mid-CAPE-suite at
   ~5.5–6.2 GB — exactly the reported "~5.9 GB").
3. **The real defect is the peak working set**, which is what melted the
   machine and set up the RAM-contention app crash (backlog #2): peak
   **8.87 GB for a 2 GB file** (measured 1 s sampling). Root cause:
   `wrf-core`'s `WrfFile` memoizes every 3-D **f64** intermediate (~400 MB
   each at 50.5 M cells — full pressure, theta, temperature, geopotential,
   height MSL/AGL, QVAPOR, destaggered U/V, …) and only evicts when the
   *timestep changes* (`prepare_cache_for_time`, `compute.rs:137`). Its
   `clear_cache()` (`file.rs:407-415`) claims it is "called automatically
   after each getvar call" — **nothing calls it, anywhere**. The ~4.5 GB cache
   is still resident during the end-of-hour store write, where the global peak
   landed.

## Fixes in this app (wrf_process.rs, wrf_volumes.rs)

- `read_wrf_products` now calls `file.clear_cache()` after the last `getvar`
  of the hour (post iso-volumes, pre store-write), releasing the ~5 GB of
  3-D intermediates before `write_hour_from_fields_with_derived` runs.
  Measured on the Enderlin file: working set falls 8.8 GB → ~1 GB at that
  point instead of riding through the write; zero recompute cost.
- `build_iso_volumes` no longer duplicates its biggest arrays: the `uvmet`
  U/V halves are split zero-copy (`split_off`) instead of `to_vec`-ing both
  halves while the 800 MB source was alive, and the dewpoint °C→K conversion
  is done in place instead of allocating a second ~400 MB array. Measured
  global peak: **8.87 GB → 8.24 GB**.
- **Anti-fix, measured and rejected:** clearing the cache *before* the
  volume build more than DOUBLED the peak (18.3 GB) — every volume read then
  recomputes its whole dependency chain (staggered reads, destaggering,
  theta→T…) with multi-hundred-MB transients stacking on the re-growing
  cache. The cache must stay warm through the volume build.
- Every diagnostic (and the iso-volume builder) is wrapped in
  `isolate_panics` (`catch_unwind`): a panic inside wrf-core/ecape-rs on a
  pathological grid or profile now degrades to a per-field
  "`panicked computing <name>: …`" note instead of unwinding the
  `rw-ui-wrf-process` worker and losing the whole import.
- The remaining ~8 GB peak is cache-dominated (mid-diagnostics ~7.9 GB floor)
  and can only be lowered upstream — see below.

## Upstream patch wrf-rust should take (FahrenheitResearch/wrf-rust @ f05758d)

`crates/wrf-core/src/file.rs:407-415` — `WrfFile::clear_cache`'s doc comment
says it is "Called automatically after each `getvar` call so that 3-D
intermediates (full_pressure, temperature, etc.) do not persist beyond the
computation that needed them". That call does not exist. Either:

- **(a) preferred:** bound the cache — track per-entry byte size in
  `cached_or_compute` (`file.rs:419-436`) and evict oldest entries beyond a
  budget (a few fields' worth, e.g. `3 * nz * ny * nx * 8` bytes), so a
  50-diagnostic pass on a 50 M-cell grid stays a few hundred MB above its
  transient floor instead of accumulating every intermediate; or
- **(b) minimal:** make `compute::getvar` honour the documented contract by
  releasing non-reusable intermediates, or fix the doc comment and document
  that long multi-`getvar` sessions must call `clear_cache()` between hours
  (what this app now does).

Also worth noting upstream: `VarOutput.data` is `Vec<f64>` — every 2-D
diagnostic hands back 8-byte floats that all consumers here immediately
narrow to f32; an f32 output mode would halve both cache and transient sizes.

## Operational guidance

- Debug builds of this path are ~20–40× slower (hours, all cores pinned) —
  never run the 250 m import under `cargo test` without `--release`/
  `--profile release-fast`; tool-driven runs will hit their timeout and get
  killed with exit `0xffffffff`, which looks like (but is not) a crash.
- The repro/regression harness lives in `wrf_process.rs`
  (`optional_real_fixture_default_import_instrumented`); point
  `RW_WRF_CRASH_FIXTURE` at any wrfout and run with `--nocapture`.

## Light import (`local_import.rs`) — same class of defect, fixed 2026-07-06

The "📄 WRF/NetCDF file…" / "📥 WRF/NetCDF folder…" LIGHT import (2D surface
fields + `*_iso` sounding volumes through the same `wrf-core` getvar family)
never got the fixes above, and its worker sent exactly ONE terminal message —
the dock showed an anonymous spinner for the whole run.

Measured on a 24-core Linux build host (release, `/usr/bin/time -v`), single
Enderlin `wrfout_d03_2025-06-21_02_15_00` (2.05 GB, 800×800×79):

| | wall | peak RSS |
|---|---|---|
| before (`d0af0b1`) | 1241 s (20:41) | 7.20 GB |
| after (`9c95975`) | 1235 s (20:35) | 6.81 GB |
| fast reads (`2242750`, 2026-07-07) | **91 s (1:31)** | 6.74 GB |

Findings and fixes:

- **The import completes — it is not a hang** (exit 0, 120 store variables
  including the `*_iso` volumes). The "doesn't import" report is the
  spinner: zero feedback for twenty minutes. The worker now streams
  `LocalImportMessage::Progress` per stage (per-getvar, per-raw-plane,
  interpolation percent, store write), rendered by the same dock code as the
  heavy path, and the light path gets the heavy path's confirm-first size
  gate (same 10 M-cell / 1 GiB thresholds) with the "🌩 Simulated radar from
  WRF (fast)" tip.
- **`clear_cache` placement:** `build_iso_volumes` now clears wrf-core's
  memoized 3-D f64 intermediates right after the hour's LAST getvar (the
  uvmet/ua/va read), before the interpolation loop — every interpolator
  input is owned by then, so the clear costs zero recompute, and the ~4 GB
  cache no longer rides through interpolation + store write. This is still
  AFTER the volume build's reads — the 18.3 GB clearing-early anti-fix above
  remains excluded. Both import paths inherit the clear; the heavy path's
  end-of-hour clear stays as the backstop for `core_fields`-off runs. The
  remaining 6.81 GB peak is the instant of the last getvar itself (warm
  cache + the five owned f64 field copies) and is only reducible by the
  upstream cache bound; the fix's real win is that the post-getvar phases
  (interpolation + store write) now run ~4 GB lighter, which is what the
  RAM-contention app crash (backlog #2) fed on.
- **Panic isolation:** the volume build is wrapped in the heavy path's
  `isolate_panics`, so a wrf-core panic degrades to a per-file
  "isobaric sounding volumes unavailable" note instead of unwinding the
  `rw-ui-local-import` worker ("Import worker stopped unexpectedly").
- **Where the time actually goes (upstream, measured by the new stage
  timestamps):** ~95% of the wall is the 2-D plane reads through netcrust's
  `hdf5-reader` — ~10.3 s and ~8 M minor page faults PER 800×800 f32 plane
  (105 raw extras + ~16 canonical planes ≈ 1165 s), burning 1087 s of SYSTEM
  time against 150 s user (832 M minor faults ≈ 3.3 TB of transient pages —
  allocation churn in the kernel, not honest compute). For contrast,
  wrf-core's own pure reader pulls all five FULL 79-level 3-D sounding
  fields in 10 s, interpolation takes 1.3 s, the store write 0.4 s. The
  churn is in the pinned `hdf5-reader`/netcrust plane-read path and cannot
  be fixed app-side; it belongs in the same upstream conversation as the
  `clear_cache` contract above. An app-side follow-up worth measuring:
  route the 2-D plane reads through wrf-core's reader for files it can
  open (est. ~10× wall reduction on compressed 250 m wrfouts), keeping
  netcrust for plain NetCDF.
- The harness is `local_import::tests::optional_wrf_fixture_imports_to_store`
  (env `RW_LOCAL_IMPORT_FIXTURE`, `--nocapture`): timestamps every progress
  line and prints peak RSS (`VmHWM`) at the end. Release only on large grids.

### Fast plane reads (2026-07-07, `f624ef2` + `2242750`) — 1241 s → 91 s

The reroute proposed above is in: for raw wrfout, `local_import` now decodes
every `[Time, …, ny, nx]` 2-D plane through **wrf-core's single-timestep
reader** (`WrfFile::read_var`), while netcrust remains the sole metadata
source (variable list, dim names, shapes, units, global attrs) and the sole
data path for plain NetCDF, the post-processed climate reader, non-record
layouts, and any plane wrf-core fails on (per-file progress line reports the
count and each fallback's reason). The per-file loop also opens netcrust
ONCE — `netcrust::open` eagerly indexes NetCDF-4 metadata twice over
(NcFile + Hdf5File, both through the churny `hdf5-reader`; ~57 s per open on
this file) and the old code paid it in both `try_postprocessed_wrf` and
`read_wrf_2d_fields`.

Enderlin `02_15_00` after-breakdown (Linux release benchmark, `/usr/bin/time -v`):
**91 s wall, 6.74 GB peak RSS** (18 s user / 75 s system, 58 M minor faults),
120 store variables — identical field set. Stages: 57.8 s `netcrust::open`
(the remaining churn — upstream), 21.5 s all ~120 2-D planes (≈1 s for the
118 via wrf-core + ~20.5 s for IVGTYP + ISLTYP, which fall back to netcrust
because wrf-core's HDF5 parser reports "No layout message in dataset object
header" for those two int datasets — a second upstream item for wrf-rust),
9.9 s sounding volumes, 1.3 s interpolation, 0.4 s store write. Peak RSS is
unchanged by design — it is the wrf-core getvar-cache peak documented above,
not the read path.

Value identity is guarded by
`local_import::tests::optional_wrf_fixture_fast_and_netcrust_2d_reads_match`
(env `RW_LOCAL_IMPORT_FIXTURE`): every record-layout plane bit-identical
between `read_first_2d_netcrust` and the wrf-core path (with fast-path
engagement asserted, so a silently-disabled reroute fails the test), plus
name/unit/bit identity of the full canonical + raw + grid field set.

Remaining upstream asks, in order of payoff on this file: (1) netcrust /
`hdf5-reader` open-time metadata indexing (57.8 s of the 91 s); (2) wrf-core
HDF5 object-header layout parsing for `IVGTYP`/`ISLTYP`-style int datasets
(~20.5 s); (3) the getvar cache bound for the ~6.7 GB peak, unchanged from
the section above. The full-diagnostics path (`wrf_process.rs`) never used
netcrust for plane data (all reads go through wrf-core `getvar`), but it DOES
pay one `netcrust::open` per file inside its `try_postprocessed_wrf` gate —
~57 s of its ~275 s on this file; worth the same shared-handle treatment if
the heavy path is revisited.
