# BowEcho — Products Guide

How to read every product, its units, palette, and the SOTA reference behind it.
All algorithms are unit-tested and verified on real derecho data.

## Base & dual-pol moments — now each has a purpose-built palette

| Product | Reads | Palette | Notes |
|---|---|---|---|
| **REF** Reflectivity (dBZ) | precip intensity | "Analyst Reflectivity HD" — smooth NWS ramp; magenta reserved for ≥65 dBZ hail | default; GR2/NWS presets still available |
| **VEL** Velocity (m/s) | radial wind; tick **Unfold VEL** | "Analyst Velocity HD" (vivid) + **"Balance VEL (CVD-safe)"** (colorblind-safe, Thyng 2016/Kovesi 2015) | dealiased via region-based unfolder |
| **SRV / DSRV** Storm-relative velocity | rotation w/ storm motion removed | velocity family | set storm motion in the sidebar |
| **CC** Correlation coefficient (ρhv) | met vs non-met; **TDS** = cool hole in warm precip | "Analyst CC" (+ "CC Debris") | resolution packed into 0.80–1.00 |
| **ZDR** Differential reflectivity (dB) | drop shape; ZDR columns/arcs | "Analyst ZDR" diverging about 0 | |
| **PHI** Differential phase (°) | along-beam phase | "Analyst PHI" ramp | was a generic ramp before |
| **KDP** Specific differential phase (°/km) | rain rate / liquid water | "Analyst KDP" diverging | warm = heavy rain |
| **SW** Spectrum width | turbulence / shear | spectrum-width family | |

## Velocity dealiasing (the spokes fix)

Region-based unfolder (Jing & Wiener 1993; Feldmann et al. 2020 *R2D2*
JTECH-D-20-0054.1; Helmus & Collis 2016 Py-ART) replacing the old radial
gate-by-gate walk that produced radial spokes. On the 05:51 UTC derecho it cut
residual fold-boundaries 21,458 → 190 (−99%). Inspector now also reports
**beam height** (4/3-Earth; Doviak & Zrnić 1993 eq. 2.28b).

## Derived products (selectable from the product picker when their source moment is present)

| Product | Formula / method | Units | Reference | How to read |
|---|---|---|---|---|
| **CREF** Composite reflectivity | column-max Z over all tilts | dBZ | NWS NCR | fullest picture of cores incl. elevated |
| **ET** Echo Tops | highest tilt with Z ≥ 18.3 dBZ, 4/3-Earth beam height | m (palette labels kft) | NWS ET | storm-top height; cone of silence reads low over radar |
| **VIL** Vert. Integ. Liquid | Σ 3.44e-6·Z̄^(4/7)·Δh, 56 dBZ hail cap, surface-extended | kg/m² | Greene & Clark 1972; Witt et al. 1998 | traces the bow / convective cores |
| **VILD** VIL Density | VIL ÷ echo-top depth | g/m³ | Amburn & Wolf 1997 | ≳3.5 g/m³ → large hail; more selective than VIL |
| **AzShr** Azimuthal shear | LLSD ∂Vr across the radial, on dealiased VEL | ×10⁻³/s | Smith & Elmore 2004; Mahalik 2019 | mesocyclone/TVS rotation; warm=cyclonic |
| **Div** Radial divergence | LLSD ∂Vr along the radial, on dealiased VEL | ×10⁻³/s | Smith & Elmore 2004 | gust front = convergence (cool); outflow = divergence (warm) |

Volume products (CREF/ET/VIL/VILD) render on the lowest reflectivity tilt and
are tilt-independent (cached across tilts). The shear/divergence products are
per-cut on the selected tilt. The volume column-walk is rayon-parallel
(~50 ms/product on a dense VCP-212 volume).

## Vertical cross-sections (engines; interactive GUI pending)

`reflectivity_cross_section` and `velocity_cross_section` reconstruct a
height×distance RHI from the volume (4/3-Earth beam geometry, height
interpolation between tilts) — for BWER/vault, overhang, descending cores, and
RIJ descent. The compute is done and gallery-verified
(`gallery_CrossSection.png`); the **draw-line gesture + on-screen section panel
is deferred** (see below).

## UX added this session

Dark dense GR2-style theme · on-canvas **colorbar** for the active product ·
**LIVE / ARCHIVE / STALE** mode chip (never mistake a stale frame for live) ·
persistent settings (startup-site memory + favorites, JSON under the platform
config dir).

## Verify without launching the app

- **Gallery PNGs:** regenerate with
  `cargo run --release -p render2d --example product_gallery -- <level2-file> <out-dir>`
  — renders every product through the real `ViewportMomentCache` path to
  `<out-dir>/gallery_*.png`.
- **Tests:** `cargo test` (≈195 across the workspace), `cargo clippy --workspace`
  (clean). Diagnostics: `velocity_diag`, `volumetric_probe`, `cross_section_probe`,
  `shear_probe`, `product_gallery`, `velocity_bench` examples.
- **App:** run `bowecho` (or grab a release build).

## Deliberately deferred (need a user-present verify loop — can't confirm GUI headlessly)

1. **Cross-section draw-gesture + section panel** (engine is done).
2. **Multi-pane grid + sync** (SP2/SP3 — the largest P0; needs a shared render budget).
3. Projection upgrade (naive equirectangular → azimuthal-equidistant) before pane sync.
4. Smoothing/bilinear sampler; dual-pol CC/SNR mask + ZDR offset; placefiles.

These are the prioritized next steps. Engines that feed them (beam geometry,
column-walk, cross-section, derived products) are in place and tested.
