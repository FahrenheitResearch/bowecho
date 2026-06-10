# AGENTS.md — bowecho-dealias

Guidance for AI coding agents integrating, modifying, or reviewing this
crate. Human-oriented docs are in [README.md](README.md); algorithm rustdoc
is on `src/lib.rs`.

## What this is

Region-based Doppler velocity dealiasing (unfolding) for weather radar.
Pure Rust, **zero dependencies**, no radar-format types — everything works
on plain `f32` slices. Extracted from (and still used by) the BowEcho
NEXRAD viewer's `render2d` crate.

## Integrating it into another codebase

The whole integration is: decode your velocity moment to `f32` m/s, call
one function, write the corrected slice back.

```rust
use bowecho_dealias::{Sweep, dealias};

let result = dealias(&Sweep {
    velocity: &velocity_mps, // &[f32], row-major rows × gates
    gates,                   // usize, gates per radial
    nyquist: &nyquist_mps,   // &[f32], one per radial
    azimuths_deg: &azimuths, // &[f32], one per radial
});
// result.velocity: corrected field, same layout
// result.folds:    i32 Nyquist co-intervals added per gate (QC field)
```

Data contract (violations of the length rules panic — caller bug):

| Field | Contract |
|---|---|
| `velocity` | m/s, **decoded physical values**, not raw codes. `NaN` = no data, below threshold, censored, or **range-folded**. Length must equal `rows * gates`. |
| `gates` | gates per radial; short radials must be padded with `NaN`. |
| `nyquist` | m/s per radial (`rows` = `nyquist.len()`). Non-finite/zero entries fall back to the sweep median; if none are valid the sweep passes through unchanged. |
| `azimuths_deg` | degrees per radial, same length as `nyquist`. Used to detect the 360° wrap (last radial adjacent to first) and to evaluate wind references. Any consistent convention works; values are taken mod 360. |

Common integration mistakes to check for:

1. **Passing raw data-words instead of decoded m/s** (e.g. NEXRAD codes
   0/1 are below-threshold/range-folded, not velocities — map them to
   `NaN` before calling).
2. **Column-major or gate-major layout.** The grid is row-major with one
   row per radial: index = `row * gates + gate`.
3. **One Nyquist for the whole volume.** Nyquist varies per tilt (and can
   vary per radial); always pass the per-radial values from the decoder.
4. **Calling once per volume.** `dealias` is per sweep/tilt (PPI). For a
   volume, prefer `dealias_cascade` (see below).
5. **Dropping `NaN` gates instead of keeping them.** Keep the grid dense;
   `NaN` holes are part of the contract and are preserved in the output.

## Choosing an entry point

- `dealias(&sweep)` — single sweep, no outside information. Correct
  *relative* unfolding; each connected group is anchored on its largest
  region, so under widespread aliasing an entire group can sit one
  2·Nyquist branch off.
- `dealias_cascade(&tilts, target)` — whole volume available. Dealiases
  top tilt (highest Nyquist, least aliasing) downward, branch-checking
  each tilt against the Browning–Wexler wind fit from the tilt above.
  Use this when you have the volume; it degrades gracefully to the plain
  engine for single-tilt input. SAILS/MRLE revisits are de-duplicated
  internally (0.1° buckets) — pass every velocity tilt, in any order.
- `dealias_with_reference(&sweep, Some(&reference))` — you have
  independent wind evidence (model winds, sounding, previous volume).
  Build a `RangeBandReference` yourself: for a wind blowing **toward**
  azimuth φ at speed s, `a = s·cos(φ)`, `b = s·sin(φ)` per range band.
- `compute_folds` / `despeckle` — building blocks if you keep your own
  storage and want to apply `2·v_N·fold` yourself.

Known limitation (documented, by design): when deep strong flow aliases
*every* usable tilt, no self-derived reference can fix the absolute
branch — that regime needs external (NWP) winds via `RangeBandReference`.
The failure analysis lives in
`docs/dealias-fold-branch-analysis.md` in the BowEcho repo.

## Verifying an integration

- Run the included demo: `cargo run -p bowecho-dealias --example
  unfold_wind` (synthetic 38 m/s wind under Nyquist 22 → 0 residual fold
  boundaries, 0.00 m/s error).
- On real data, count residual fold boundaries (adjacent finite gates
  differing by more than one Nyquist) before and after; expect roughly a
  99% reduction on heavily aliased sweeps. On the KEAX 2026-06-09 05:51 UTC
  derecho sweep the engine goes 21,458 → 196.
- Invariant you can assert anywhere: where input was finite and Nyquist
  known, `output == input + 2.0 * nyquist[row] * folds[idx] as f32`
  (despeckling included — it only ever adds whole fold multiples).
- The engine is deterministic: identical input ⇒ identical output, every
  run. If you observe nondeterminism, the bug is in your plumbing.

## Working on the crate itself

Layout (one concern per module):

```
src/lib.rs        Sweep/Dealiased types, dealias()/compute_folds(), Nyquist fallback, crate docs
src/region.rs     core fold solver: region labelling, boundary votes, weighted union-find, anchoring
src/despeckle.rs  post-pass: snap isolated outliers onto local consensus
src/reference.rs  RangeBandReference + Browning–Wexler per-band fit
src/cascade.rs    Tilt + dealias_cascade (top-down volume engine)
```

Gates (same as repo CI): `cargo fmt --all --check`,
`cargo test -p bowecho-dealias`, `cargo clippy -p bowecho-dealias
--all-targets -- -D warnings`. Keep the crate **zero-dependency**.

Determinism is a tested guarantee (`is_deterministic_across_runs`). The
`HashMap` uses in `region.rs` are safe only because of explicit
tie-breaking: vote ties prefer the smaller |fold|, resolved edges sort by
(support, region pair). Do not remove those tie-breakers or iterate a
`HashMap` into anything order-sensitive.

Numeric constants (`REGION_JOIN_FRAC = 0.5`, `REGION_MAX_FOLD = 5`, fit
gates in `reference.rs`) are field-validated on real NEXRAD events — don't
tune them without re-running the validation set in the BowEcho repo
(`docs/dealias-fold-branch-analysis.md` lists it: the KEAX derecho sweep,
two KMBX QLCS volumes, plus the unit suite).

This crate is also consumed by BowEcho's `render2d` (thin grid adapters
around these functions); if you change public signatures, fix `render2d`
in the same change and run the full workspace tests.

## Method citations

Region engine: Jing & Wiener 1993 (JTECH 10, 798–808); Helmus & Collis
2016 (J. Open Res. Softw. 4(1) e25, Py-ART `dealias_region_based`);
Feldmann et al. 2020 R2D2 (doi:10.1175/JTECH-D-20-0054.1). Reference
checks: Louf et al. 2020 UNRAVEL (doi:10.1175/JTECH-D-19-0020.1). Wind
fit: Browning & Wexler 1968 (JAM 7, 105–113). Despeckle: Holleman &
Beekhuis 2003 (JTECH 20, 443–453); Altube et al. 2017
(doi:10.1175/JTECH-D-16-0065.1). Keep citations in code comments when
extending — that is a project convention.

## License

MIT OR Apache-2.0. Attribution appreciated: "velocity dealiasing from
BowEcho (bowecho-dealias)".
