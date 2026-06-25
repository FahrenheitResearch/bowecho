# BowEcho KDP + derived-products patch — agent handoff

## Target and intent

This patch was authored against public BowEcho commit
`55c6976a73a874eb21fe1d1c4c1cc97219ca36b0` (workspace version `0.23.3`).
The requesting tree is described as BowEcho `v0.26`, so apply with three-way
merge and adapt only the final ingest hook if that private tree moved files.

The primary behavior change is:

> When a completed elevation cut contains PHI/DPHI but no native KDP, derive a
> quality-controlled KDP `MomentGrid` and insert it as
> `MomentType::SpecificDifferentialPhase`. Never overwrite native KDP unless
> explicitly configured.

The patch also turns `product_engine` into an actual derivation crate. It adds
30 sweep-local products, generic CAPPI/column products, and reusable temporal
operations. Existing BowEcho volume products in `render2d` remain the preferred
implementations for CREF, echo tops, VIL/VILD, SHI, MESH, POSH/POH, shear,
divergence, MARC, gust proxy, cross-sections, and dealiasing.

## Files added/changed

- `crates/product_engine/src/lib.rs` — registry and public API
- `crates/product_engine/src/sweep.rs` — KDP and 29 additional sweep products
- `crates/product_engine/src/volume.rs` — CAPPI, column extrema/mean,
  low-level composite, echo base/depth, and height of max reflectivity
- `crates/product_engine/src/temporal.rs` — difference, trend, swaths,
  accumulation, duration, and exceedance probability

No new third-party dependencies are required.

## Apply

```bash
git apply --3way bowecho-derived-products.patch
```

Then run the validation commands near the end of this document.

## Required ingest wiring

Do **not** derive before split products/sweeps are merged. International feeds
can deliver REF, RHO, ZDR, and PHI in separate files; KDP should see the final
merged cut so it can use all available quality-control fields.

Add `product_engine` as a path dependency to the crate that owns the final
`RadarVolume` handoff:

```toml
product_engine = { path = "../product_engine" }
```

Immediately after decode/assembly/merge and before the volume enters the cache,
UI, product picker, or renderer:

```rust
let kdp_report = product_engine::derive_volume_in_place(
    &mut volume,
    &product_engine::DerivationConfig::kdp_only(),
);
```

This call is idempotent under the default configuration. Native KDP wins. A
PHI-only frame receives derived KDP. A frame with neither remains unchanged.

Search likely integration points with:

```bash
rg -n "merge_radar_volumes|decode_supported_volume|RadarVolume" crates/app_ui crates/data_source crates/nexrad_io
```

Wire the call at the last common point, not separately into every format
decoder. If BowEcho has a partial-preview path that is rendered before normal
finalization, call the same function after that preview cut is internally
complete; native-preserving idempotence makes a second final call safe.

### Radar band

`kdp_only()` defaults to S band, which is appropriate for NEXRAD WSR-88D.
For C- or X-band feeds:

```rust
let mut config = product_engine::DerivationConfig::kdp_only();
config.set_band(product_engine::RadarBand::C); // or X
product_engine::derive_volume_in_place(&mut volume, &config);
```

The band changes KDP physical bounds, R(KDP) coefficients, and attenuation
coefficients.

## KDP retrieval implemented here

The retrieval is intentionally more conservative than a raw finite difference:

1. Sample PHIDP as physical values through `MomentGrid::scaled_value`.
2. Align optional RHO and REF by radial index **and physical range**, even when
   their gate spacing differs from PHI.
3. Reject low-quality gates using configurable RHO/REF thresholds.
4. Unwrap PHIDP across its configurable phase period.
5. Interpolate only short internal gaps (default: two gates) for fitting.
6. Suppress isolated phase spikes with a Hampel filter.
7. Estimate local phase slope with a centered, Huber-robust linear regression
   over a physical-range window (default: 3 km).
8. Compute `KDP = 0.5 * d(PHIDP)/dr` in degrees/km.
9. Reject estimates outside band-specific physical bounds.
10. Emit an uncertainty grid from the local slope standard error.

By default, interpolated PHI gates are useful to stabilize neighboring fits but
KDP is not painted into the missing gate itself.

## Eager versus lazy products

Only `kdp_only()` should run automatically on every decoded frame. Computing
all products eagerly wastes CPU and memory, especially textures and volume
columns.

Use:

```rust
// A small operational set.
let config = product_engine::DerivationConfig::analyst_defaults();

// Full sweep-local gallery; suitable for an explicit background/on-demand job.
let config = product_engine::DerivationConfig::all_supported();
```

For one selected product without mutating the source cut:

```rust
let grid = product_engine::derive_product(
    cut,
    product_engine::DerivedSweepProduct::CircularDepolarizationRatio,
    &config,
);
```

## Sweep-local products

| ID | Product | Principal input(s) |
|---|---|---|
| KDP | Specific differential phase | PHI; optional RHO/REF QC |
| PHIF | Filtered/unwrapped differential phase | PHI |
| KDP_SD | Local KDP slope uncertainty | PHI |
| AH, PIA, REFC | Specific/path attenuation and corrected REF | KDP/PHI + REF |
| ADP, PIDA, ZDRC | Differential attenuation and corrected ZDR | KDP/PHI + ZDR |
| RATE_Z | Reflectivity rain rate | REF |
| RATE_KDP | KDP rain rate | KDP |
| RATE | Hybrid Z/KDP rain rate | REF + KDP; optional RHO |
| LWC | Radar liquid-water-content proxy | REF |
| HKE | Hail kinetic-energy flux | REF |
| CDR | Circular depolarization ratio | ZDR + RHO |
| L_RHO | Logarithmic correlation ratio | RHO |
| REF_TEX, VEL_TEX, SW_TEX | Moment textures | corresponding base moment |
| ZDR_TEX, RHO_TEX, PHI_TEX, KDP_TEX | Dual-pol textures | corresponding moment |
| REF_GRAD_R, VEL_GRAD_R | Range gradients | REF or VEL |
| MET_QI, MET_MASK | Meteorological quality/index mask | RHO and/or REF |
| TDS_SCORE | Debris-signature diagnostic score | REF + RHO; optional ZDR |
| HAIL_SCORE | Dual-pol hail-signature diagnostic | REF; optional RHO/ZDR |
| TURB | Doppler turbulence proxy | SW and/or velocity texture |

`TDS_SCORE` and `HAIL_SCORE` are explicitly diagnostics, not warnings,
classifications, or declarations of a tornado/hail report. Keep those labels in
the UI.

## Volume and temporal products

The patch adds generic functions rather than stuffing these into
`ElevationCut::moments`:

- `cappi_grid`
- `column_max_grid`, `column_min_grid`, `column_mean_grid`
- `low_level_composite_reflectivity_grid`
- `echo_base_grid`, `echo_top_height_grid`, `echo_depth_grid`
- `height_of_max_reflectivity_grid`
- `difference_grid`, `trend_grid`
- `maximum_swath_grid`, `minimum_swath_grid`, `mean_grid`
- `accumulate_rate_grids`
- `exceedance_duration_grid`, `exceedance_probability_grid`

Keep BowEcho's existing optimized `render2d::volumetric` algorithms for CREF,
ET, VIL/VILD, SHI/MESH/POSH/POH. The registry lists them so the product catalog
can be unified without duplicating their implementation.

Temporal helpers require identical polar geometry. Regrid/motion-compensate
before calling them when radials or gate geometry differ.

## Picker, units, and color tables

KDP is inserted as the existing known enum variant, so the current KDP picker,
readout, 2-D renderer, 3-D volume path, and KDP color family should work once
the final ingest hook runs.

Other new sweep products use `MomentType::Unknown(stable_id)` to avoid breaking
exhaustive matches across the private v0.26 tree. If the picker already lists
all keys in `cut.moments`, they will appear by ID. For polished labels and units,
read `product_engine::derived_products()` and
`DerivedSweepProduct::{display_name, units}`. Add palette mappings for at least:

- RATE/RATE_Z/RATE_KDP: precipitation-rate palette
- PHIF: phase palette
- KDP_SD and texture products: sequential low-to-high palette
- AH/PIA/ADP/PIDA: attenuation palette
- REFC/ZDRC: corresponding base-moment palette
- CDR: negative dB palette
- MET_QI/MET_MASK/TDS_SCORE/HAIL_SCORE: bounded 0-1 or 0-100 palette
- height products: metres or km AGL palette

Promoting stable IDs to dedicated `MomentType` variants can be done later, but
is not required for rendering an F32 `MomentGrid`.

## Scientific defaults and traceability

- KDP uses the defining relation `KDP = 0.5 d(PHIDP)/dr` and a range window,
  consistent with operational/research retrieval families such as the
  Vulpiani and other filtered-PHIDP methods exposed by ARM-DOE Py-ART.
- R(KDP) coefficients are S `(50.70, 0.85)`, C `(29.70, 0.85)`, and X
  `(15.81, 0.7992)`, matching Py-ART's published coefficient table.
- PHIDP-linear attenuation coefficients are S `(0.04, 0.004)`, C
  `(0.08, 0.03)`, and X `(0.28, 0.04)` for horizontal and differential
  correction, matching Py-ART's band table.
- CDR follows Matrosov (2004); `L_RHO = -log10(1-RHOHV)` follows Ryzhkov
  (2001).
- HKE uses the reflectivity weighting used by the WSR-88D hail algorithm family.

These are configurable defaults, not a calibration claim. QPE and attenuation
coefficients vary with radar calibration, wavelength, drop-size distribution,
season, and climate regime.

## Products deliberately not fabricated

The patch does not pretend that all meteorological products can be recovered
from the seven base moments. The following need additional state or dedicated
algorithms and should remain separate work items:

- hydrometeor classification with temperature/freezing-level profiles
- melting-layer height/bright-band correction
- snow/ice precipitation rate calibrated to local climatology
- beam-blockage correction and static clutter maps
- multi-radar mosaics/MRMS-style blending
- storm-motion advection and motion-compensated accumulations
- gauge-adjusted QPE

BowEcho already contains several velocity, cell, hail, and volume algorithms;
do not replace them with weaker duplicates.

## Validation

Run in the real v0.26 workspace:

```bash
cargo fmt --all -- --check
cargo test -p product_engine
cargo test -p nexrad_io
cargo test -p render2d
cargo clippy --workspace --all-targets -- -D warnings
```

Then use representative Level-II files:

1. A volume with PHI but no native KDP: KDP must appear on each PHI-bearing cut.
2. A volume with native KDP: values must remain byte-for-byte/logically unchanged.
3. A split-product international volume: derive only after merge and verify RHO
   and REF QC align at physical range.
4. Compare derived KDP against a trusted Py-ART retrieval and native KDP where
   available. Inspect bias, RMSE, edge loss, negative-KDP behavior, and strong
   backscatter-phase regions.
5. Benchmark `kdp_only()` separately from `all_supported()`.

The included `validation/kdp_numerical_validation.py` independently exercises
wrapped linear phase, noise/spikes/gaps, and flat phase. Its generated JSON is
not a Rust compile result; it validates the numerical equations only.
