# bowecho-dealias

Region-based Doppler velocity dealiasing (unfolding) for weather radar, in
pure Rust with **zero dependencies**. This is the engine shipped in
[BowEcho](https://github.com/FahrenheitResearch/bowecho), extracted so anyone
can drop it behind their own Level II decoder (NEXRAD, ODIM, IRIS, …).

Doppler radars measure radial velocity modulo the Nyquist co-interval: true
velocities outside ±v<sub>N</sub> come back *aliased* (folded) by a multiple
of 2·v<sub>N</sub>, which renders as nonsense couplets exactly where the
weather matters most. This crate restores the true field.

## Why region-based?

The classic gate-by-gate radial-walk correction propagates a single bad fold
decision down an entire ray, producing the radial "spokes" that plague
high-shear convection (derechos, mesocyclones). The region-based approach
decides whole coherent regions at once and lets genuine discontinuities
remain at region boundaries, so an error cannot run down a radial:

1. **Label regions** — flood-fill connected regions whose neighbouring gates
   differ by less than half a Nyquist (union-find over the polar grid,
   including the 360° azimuth wrap), so no fold occurs *within* a region.
2. **Vote on boundaries** — a region-adjacency graph whose edges carry the
   integer Nyquist fold between two regions: the consensus of
   `round((v_a − v_b) / 2·v_N)` over every shared boundary gate-pair.
3. **Resolve folds** — merge regions strongest-boundary-first through a
   union-find with per-node integer fold offsets (a weighted/"potential"
   DSU), keeping all relative fold relations consistent.
4. **Anchor** — shift each connected group so its largest region has fold
   zero.
5. **Apply & despeckle** — add 2·v<sub>N</sub>·fold per gate, then snap
   isolated single-gate outliers onto the Nyquist multiple nearest their
   local median.

On the 9 June 2026 KEAX derecho sweep this cut residual fold boundaries from
21,458 (radial-walk) to under 200. The output is deterministic: identical
input yields the identical field, every run.

## Use

```toml
[dependencies]
bowecho-dealias = "0.6"
```

```rust
use bowecho_dealias::{Sweep, dealias};

// Row-major rows × gates, one row per radial. NaN = no data / range-folded.
let result = dealias(&Sweep {
    velocity: &velocity_mps,   // &[f32], rows * gates
    gates,                     // usize
    nyquist: &nyquist_mps,     // &[f32], one per radial (NaN ok — median fallback)
    azimuths_deg: &azimuths,   // &[f32], one per radial
});
// result.velocity : corrected field, same layout, NaN preserved
// result.folds    : integer Nyquist co-intervals added per gate (QC field)
```

There is no radar-format coupling: decode your moment to `f32` m/s, hand it
over, write the corrected slice back into whatever structure you render from.

### Whole volumes: the tilt cascade

Boundary votes only lock folds *relative* to each other; each connected
group's absolute branch is under-determined from one sweep, and on sweeps
with widespread aliasing a same-sweep VAD reference is circular (the wrapped
gates poison the fit). `dealias_cascade` breaks the circularity vertically —
higher tilts carry higher Nyquist velocities and little aliasing, so the
volume is dealiased top-down, each tilt's Browning–Wexler wind fit serving as
the external reference for the tilt below:

```rust
use bowecho_dealias::{Tilt, dealias_cascade};

let tilts: Vec<Tilt> = volume_tilts.iter()
    .map(|t| Tilt { sweep: t.as_sweep(), elevation_deg: t.elevation })
    .collect();
let result = dealias_cascade(&tilts, target_index);
```

SAILS/MRLE elevation revisits are de-duplicated automatically.

### External references (model winds)

`RangeBandReference` has public fields, so the branch evidence can come from
anywhere — e.g. NWP model winds at the radar site. For a wind blowing toward
azimuth φ at speed s over some range band: `a = s·cos(φ)`, `b = s·sin(φ)`,
then call `dealias_with_reference`. `fit_range_band_reference` builds the
same structure from any already-trusted velocity field (previous volume,
upper tilt). For the failure regime that motivates this seam — deep strong
flow aliasing every usable tilt — see
[`docs/dealias-fold-branch-analysis.md`](../../docs/dealias-fold-branch-analysis.md).

### Building blocks

- `compute_folds` — fold counts only (steps 1–4), if you keep your own
  storage or want folds as a QC field.
- `despeckle` — the standalone post-pass.
- `fit_range_band_reference` / `RangeBandReference::evaluate` — the wind
  reference on its own.

Run the demo: `cargo run -p bowecho-dealias --example unfold_wind`

## References

- Jing, Z., and G. Wiener (1993): Two-Dimensional Dealiasing of Doppler
  Velocities. *J. Atmos. Oceanic Technol.* **10**, 798–808,
  doi:[10.1175/1520-0426(1993)010<0798:TDDODV>2.0.CO;2](https://doi.org/10.1175/1520-0426(1993)010<0798:TDDODV>2.0.CO;2).
- Helmus, J. J., and S. M. Collis (2016): The Python ARM Radar Toolkit
  (Py-ART). *J. Open Res. Softw.* **4**(1), e25,
  doi:[10.5334/jors.119](https://doi.org/10.5334/jors.119) — the
  `dealias_region_based` lineage.
- Feldmann, M., et al. (2020): R2D2 — A Region-Based Recursive Doppler
  Dealiasing Algorithm for Operational Weather Radar. *J. Atmos. Oceanic
  Technol.* **37**,
  doi:[10.1175/JTECH-D-20-0054.1](https://doi.org/10.1175/JTECH-D-20-0054.1).
- Louf, V., et al. (2020): UNRAVEL — A Robust Modular Velocity Dealiasing
  Technique for Doppler Radar. *J. Atmos. Oceanic Technol.* **37**(5),
  741–758,
  doi:[10.1175/JTECH-D-19-0020.1](https://doi.org/10.1175/JTECH-D-19-0020.1)
  — the reference-check design.
- Browning, K. A., and R. Wexler (1968): The Determination of Kinematic
  Properties of a Wind Field Using Doppler Radar. *J. Appl. Meteor.* **7**,
  105–113 — the zeroth-harmonic wind reference.
- Holleman, I., and H. Beekhuis (2003): Analysis and Correction of Dual PRF
  Velocity Data. *J. Atmos. Oceanic Technol.* **20**, 443–453.
- Altube, P., et al. (2017): Correction of Dual-PRF Doppler Velocity Outliers
  in the Presence of Aliasing. *J. Atmos. Oceanic Technol.* **34**(7),
  1529–1543,
  doi:[10.1175/JTECH-D-16-0065.1](https://doi.org/10.1175/JTECH-D-16-0065.1)
  — the despeckle pass.

## License

MIT OR Apache-2.0, same as BowEcho.
