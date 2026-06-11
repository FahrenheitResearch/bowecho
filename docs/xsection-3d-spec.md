# Cross-Section + Volumetric 3D Implementation Spec (verified)
## INTERPOLATION / CROSS-SECTIONS
# Radar Volume Vertical Interpolation & Cross-Section Reconstruction — Implementation Spec

**Scope:** real-time single-radar NEXRAD Level II viewer (Rust) with full volume scans (all VCP tilts, polar gates), arbitrary-line vertical cross-sections, CAPPIs. All formulas below were verified against the primary papers (PDFs fetched and read), not from memory. Direct quotes are marked.

---

## 0. TL;DR recommendations

1. **Geometry:** 4/3-effective-earth-radius model, Doviak & Zrnić (1993) Eq. 2.28 form (verified via Gao, Brewster & Xue, *Adv. Atmos. Sci.* 2006, Eqs. 3–4, and Zhang, Howard & Gourley 2005, Eqs. 2–4). Use the exact closed-form **inverse** (§1.3) for per-pixel slice sampling — no iteration needed.
2. **Vertical interpolation:** the MRMS/NMQ scheme — **nearest-neighbor in range and azimuth, linear interpolation in elevation angle** between the two bracketing tilts (Zhang, Howard & Gourley 2005, *JTECH* 22, 30–42, Eqs. 5–7). Interpolate in **elevation angle, not height**.
3. **Above top tilt / below lowest tilt:** extend the nearest tilt's value only within **half a beamwidth**; blank beyond. Exact MRMS rule, Zhang et al. 2011 (*BAMS* 92): *"No extrapolation was applied at the top and bottom of the radar volume scan beyond half a beam width."* Cone of silence stays blank (single radar).
4. **Gaps:** always interpolate between bracketing tilts regardless of gap size (MRMS does, even across VCP 21's ~5° upper gaps). When a bright band is present **and adjacent tilts are >1° apart**, blend in Zhang's horizontal interpolation (VHI, Eqs. 8–10) or at minimum expect/flag ring artifacts.
5. **Velocity:** slice the **dealiased single-radar field in native radial geometry**; nearest-neighbor azimuth; linear-in-elevation only with a shear/alias guard (fall back to nearest gate when the bracketing samples differ by more than the Nyquist velocity). Never average velocity across radars or across RF gates.
6. **Dual-pol:** ρhv (CC) is non-monotone with a melting-layer minimum (ρhv 0.90–0.97 band, Giangrande, Krause & Ryzhkov 2008) — use **nearest-gate in elevation when either bracketing sample is < 0.97**, linear otherwise. Z: interpolate in dBZ for display (MRMS practice); linear-Z is more accurate in convective cores (Warren & Protat 2019) — make it an option. ZDR: linear in dB.
7. **Render raw gates as beam-width-tall arcs** (±bw/2 about beam center), not points: at 230 km a 0.95° beam is ~3.8 km tall; a point/thin-line rendering misrepresents the resolution volume and fabricates gaps that the beam actually fills.

---

## 1. Beam geometry (Doviak & Zrnić 4/3 model)

Primary source: Doviak, R. J., and D. S. Zrnić, 1993: *Doppler Radar and Weather Observations*, 2nd ed., Academic Press, §2.2.3 (Eq. 2.28). Formula text verified against Gao, Brewster & Xue (2006, Eqs. 1, 3, 4, 6) and Zhang et al. (2005, Eqs. 2–4), both of which reproduce it verbatim.

### 1.1 Forward equations (gate → height/ground distance)

Let `a = 6371.0 km` (mean Earth radius), effective radius

```
k_e = 1 / (1 + a·(dn/dh))      // dn/dh ≈ −1/(4a) standard atmosphere ⇒ k_e = 4/3
a_e = k_e · a = (4/3)·6371.0 = 8494.67 km
```

For slant range `r` (to gate center) and elevation angle `θ_e` (radians, from horizontal):

```
h(r, θ_e) = sqrt(r² + a_e² + 2·r·a_e·sin θ_e) − a_e          // height above the FEEDHORN
s(r, θ_e) = a_e · asin( r·cos θ_e / (a_e + h) )              // great-circle ground distance
z_MSL     = h + z_station + z_tower                          // add antenna height from Vol. header
```

Numerically: `h` is a small difference of two ~8500 km numbers — **compute in f64** (Gao et al. 2006 flag exactly this; their Taylor-expanded alternative `h ≈ r sinθ_e + r²cos²θ_e/(2a_e)` is f32-safe and within meters of exact, fine as a fast path).

Sanity values (computed from the formula): 0.5° tilt → h ≈ 1.46 km at r = 100 km; ≈ 5.12 km at r = 230 km (matches published WSR-88D beam-height charts).

**Validity caveats** (Gao et al. 2006): assumes refractivity decreasing linearly with height (≈ −39 N/km); under strong inversions observed errors reached ~400 m at 1.5 km height; ducting (dN/dh ≤ −300 N/km) breaks the model entirely (AP). Accept this for a viewer; do not pretend sub-100 m vertical accuracy at long range.

### 1.2 Beam width growth — why arcs, not points

- WSR-88D half-power (−3 dB) beamwidth `bw ≈ 0.95°` (NWS WSR-88D training spec; 0.925° is the commonly used antenna figure — make it a constant, default `0.95°`).
- **Effective azimuthal beamwidth is wider** because the antenna rotates while integrating pulses: 1.02° at 0.5° (super-res) sampling, 1.29° at 1.0° sampling (Wood & Brown 1997, cited in the super-res literature).
- Physical beam diameter `D(r) = 2·r·tan(bw/2) ≈ r·bw_rad`: **0.3 km at 25 km, ~2.5 km at 150 km, ~3.8 km at 230 km** (the 25/150 km figures are Zhang et al. 2005's own numbers, p. 35).
- The returned moment is a **power-weighted average over the whole resolution volume** — two-way pattern `f⁴(α)`, with one-way `f²(α) = {8 J₂[(πD sinα)/λ] / [(πD sinα)/λ]²}²` (Bessel function of 2nd order; Zhang et al. 2005 Eq. 1, after D&Z 1993). The sample does not live at a point.
- Zhang et al. 2005 render exactly this way in their reference figures ("radar bin volume mapping": shaded beam areas on a 50 m × 10 m RHI grid). **Spec:** in raw mode, draw each gate as a filled quad/arc spanning `θ_e ± bw/2` in elevation (heights via §1.1 at the gate's near/far range) so the user sees true sampling, including the unsampled wedges between high tilts.

### 1.3 Exact inverse (pixel → radar coordinates) — the core of per-pixel slicing

For a slice pixel at ground distance `s` (along the great circle from the radar through the pixel's map location) and height `h` above the feedhorn, with `σ = s / a_e`:

```
r   = sqrt( a_e² + (a_e + h)² − 2·a_e·(a_e + h)·cos σ )        // law of cosines on effective sphere
θ_e = asin( ((a_e + h)² − a_e² − r²) / (2·a_e·r) )             // exact inversion of h(r,θ_e)
```

This is algebraically exact under the same model (derived directly from Eq. 1.1; round-trips to machine precision — make that a unit test). Azimuth `φ` comes from the geodesic bearing radar→pixel (your map projection already provides it).

Local beam-tangent angle at the gate (needed only for velocity-projection niceties, §5):

```
θ_e' = θ_e + atan( r·cos θ_e / (a_e + r·sin θ_e) ) = θ_e + s/a_e     // Gao et al. 2006, Eq. 6
```

---

## 2. Vertical interpolation between tilts — operational approaches

### 2.a MRMS / NMQ (Zhang, Howard & Gourley 2005 — RECOMMENDED)

*JTECH* **22**, 30–42, doi:10.1175/JTECH-1689.1. Verified from the full paper text.

Scheme "VI": **nearest neighbor in range and azimuth; linear interpolation in elevation angle** between the two adjacent tilts bracketing the grid point, *at the same range and azimuth as the grid point*. Their Eqs. (5)–(7), verbatim structure:

```
f_a = (w1·f1 + w2·f2) / (w1 + w2)                  // (5)
w2  = (θ_i − θ1) / (θ2 − θ1)                       // (6)  weight of the UPPER tilt
w1  = (θ2 − θ_i) / (θ2 − θ1)                       // (7)  weight of the LOWER tilt
```

where `θ_i` is the grid point's elevation angle (from §1.3), `θ1, θ2` the bracketing tilt elevations, `f1, f2` the nearest gates (in range and azimuth) on those tilts. Interpolation is performed on the **dBZ values** as stored (MRMS reflectivity grids are dBZ; see §6 for the Z-vs-dBZ evidence).

- **Above the highest tilt / below the lowest tilt / cone of silence:** Zhang 2005: *"All the spherical-to-Cartesian remapping schemes discussed … do not extrapolate reflectivity values into the data void regions."* Operational refinement, Zhang et al. 2011 (*BAMS* 92, 1321–1338, doi:10.1175/2011BAMS-D-11-00047.1): *"The analysis scheme includes a nearest-neighbor mapping on the range–azimuth plane and an exponential interpolation in the elevation direction (Zhang et al. 2005; Lakshmanan et al. 2006). **No extrapolation was applied at the top and bottom of the radar volume scan beyond half a beam width.**"* I.e. fill `[θ_lowest − bw/2, θ_highest + bw/2]` with the edge tilt's value; blank outside. The cone of silence (above 19.5° for precip VCPs) is filled only by *other* radars in the multi-radar mosaic, or by VPR extrapolation for QPE — for a single-radar viewer, leave it blank/hatched.
- MRMS's severe-weather 3D grid uses the sibling w2merger formulation (Lakshmanan, Smith, Hondl, Stumpf & Witt 2006, *Wea. Forecasting* 21, 802–823, doi:10.1175/WAF942.1): same nearest-neighbor range/azimuth, but an **exponential elevation weight** `δ_e = exp[α³·ln(0.005)]`, `α = (e − θ_i)/max(|θ_i±1 − θ_i|, b_i)` — equals 1 on the beam axis, ~0.5 at half the gap to the next tilt, 0.005 at the full gap. It interpolates *tangentially in spherical space*; the authors themselves note it "could fail in the presence of strong vertical gradients such as bright bands." Linear-in-θ (Zhang) and this are visually near-identical for slices; linear is simpler — use linear.

### 2.b Askelson, Aubagnac & Straka 2000 — adaptive Barnes

*Mon. Wea. Rev.* **128**, 3050–3082, doi:10.1175/1520-0493(2000)128<3050:AAOTBF>2.0.CO;2. Verified from the full paper.

Insight: radar data spacing is **anisotropic** (radial spacing fixed at Δr; azimuthal/elevational spacing = r·Δangle, growing linearly with range) so an isotropic fixed-length weight either oversmooths near the radar or undersmooths far away. Their A-B weight (their Eq. 3) splits the Gaussian by spherical direction:

```
w = exp( −r_ik²/κ_r  −  φ_ik²/κ_φ  −  θ_ik²/κ_θ )
```

with `r_ik` the radial separation and `φ_ik, θ_ik` **angular** separations, smoothing parameters chosen per-direction from the per-direction data spacing. Because the azimuth/elevation terms are angular, the *physical* smoothing length automatically scales with range — that is the "distance-dependent smoothing" insight. Relevance to you: it justifies doing your interpolation **in (r, φ, θ) radar space** rather than in Cartesian xyz; you get its benefit for free with scheme 2.a (also stated by Lakshmanan et al. 2006: interpolating in spherical coordinates is "neither vertical nor horizontal, but tangential to the direction of the beam").

### 2.c Trapp & Doswell 2000 — what NOT to do

*JTECH* **17**, 105–120, doi:10.1175/1520-0426(2000)017<0105:RDOA>2.0.CO;2. Verified from the full paper; their numbered conclusions:

1. Fixed isotropic Cressman/Barnes filters damp fixed *dimensional* wavelengths everywhere — consistent but throws away near-radar detail.
2. **Range-varying smoothing parameters** make the analysis amplitude vary with range; the artifact "may be misinterpreted as some physical change in the analyzed field" — evolution of the storm gets convolved with the weight-function variation. (This is the caution against being too clever with adaptive schemes for *quantitative* use.)
3. **Cressman weights produce negative spectral sidelobes** (phase-shifted short-wavelength garbage). Avoid Cressman.
4. Anisotropic weights distort shapes depending on feature orientation.
5. **"Bilinear and 'nearest-neighbor' interpolation schemes generate wavelengths not present in the initial data"** — aliasing-like artifacts sensitive to feature orientation vs. data spacing.

Their recommendation (for quantitative analysis, e.g. derivatives): isotropic Barnes, *constant* smoothing parameter sized to the **coarsest** data spacing in the domain. Note Zhang et al. 2005 explicitly rejected that for display/storm-scale use because it discards the high resolution near the radar — for a *viewer*, Zhang's VI wins; for any future quantitative product (gradients, divergence), remember Trapp & Doswell.

### 2.d SPRINT / REORDER — the classical baseline

- SPRINT: Mohr, C. G., and R. L. Vaughan, 1979: "An economical procedure for Cartesian interpolation and display of reflectivity factor data in three-dimensional space," *J. Appl. Meteor.* **18**, 661–670 — **bilinear interpolation in radar space** to a Cartesian grid (confirmed in James et al. 2000, *Wea. Forecasting* 15: "the data are bilinearly interpolated to a Cartesian grid using NCAR's SPRINT software"; Trapp & Doswell classify it as "bilinear interpolation"). Mohr & Vaughan's own honest warning, quoted by Trapp & Doswell: *"More elaborate interpolation schemes were examined and as the degree of sophistication increased the artifacts became smoother, but they never disappeared."*
- REORDER (Oye, Mueller & Smith 1995, 27th Conf. Radar Meteor.): Cressman/exponential radius-of-influence resampling — superseded; carries the Cressman sidelobe problem (2.c #3).
- Verdict: SPRINT-style bilinear-in-(range, elevation) is the "simple correct baseline" — your per-pixel scheme below is exactly this baseline upgraded with MRMS's edge rules (and you can optionally add linear-in-range blending between the two nearest gates instead of nearest-gate; visually it just antialiases range bin edges).

### Recommendation and the "elevation angle vs height" question

**Interpolate in elevation angle at constant (range, azimuth)** — scheme 2.a — because:
- Tilts are surfaces of constant θ_e: bracketing is a binary search over ~9–15 sorted angles; exact and O(log T) per pixel (or O(1) walking up a pixel column, since θ_e is monotone in h at fixed s).
- "Interpolation in height" at constant ground distance s would connect gates from *different ranges* on the two tilts (the lower tilt reaches ground distance s at longer slant range) — it silently mixes range resolutions and is what produces Zhang's ring artifacts in the bright band *worse*, not better. The vertical line through a slice pixel is sampled by the two tilts at (very nearly) the same range — elevation-angle interpolation respects that.
- It is the published operational standard (MRMS QPE grid: Zhang et al. 2005/2011; MRMS severe grid: Lakshmanan et al. 2006 — both interpolate in the elevation direction in spherical space).

**Caveats (state them in code comments):** linear-in-θ assumes the field varies linearly across the tilt gap. It (a) smears/dilutes thin layers (bright band) that lie between tilts, producing **low-reflectivity rings** on CAPPIs and soft bands on slices in VCP-21-style gaps (documented, Zhang 2005 Fig. 9); (b) makes C¹-discontinuous fields — fine for display, do not differentiate it (Zhang 2005 summary explicitly warns derivatives need smoothing first); (c) cannot create maxima between tilts — true layer maxima between tilts are *underestimated*.

---

## 3. Gap and bright-band handling

- **Tilt-gap reality:** VCP 21 elevations 0.5, 1.45, 2.4, 3.35, 4.3, 6.0, 9.9, 14.6, 19.5° (Zhang 2005 Table 1) — upper gaps up to 5.3°. Modern VCP 12/212/215: 14–15 tilts, dense below 6.4°; VCP 31/32/35 top out at 4.5–6.4°, leaving everything above unsampled.
- **What operational systems do:** they **interpolate across all internal gaps** (MRMS VI has no maximum-gap cutoff — verified; the only blanking rule is the half-beamwidth rule at the volume top/bottom). w2merger likewise lets the two straddling tilts fill the void with weights down to 0.005. Nothing operational blanks interior gaps.
- **The 1° threshold:** Zhang 2005 VHI activates **"horizontal interpolation between adjacent tilts that are more than 1° apart"** and only where a bright band was detected (their automated BB detector, Gourley & Calvert 2003, *Wea. Forecasting* 18, 585–599). VHI formula (Eqs. 8–10): add two horizontally adjacent observations `f3, f4` taken **at the same height as the grid cell** on the tilts below/above, weighted by ground distance: `w4 = (s3 − s_i)/(s3 − s4)`, `w3 = (s_i − s4)/(s3 − s4)`, combined `f_a = (w1f1 + w2f2 + w3f3 + w4f4)/(Σw)`. Cost/benefit: recovers the BB layer and kills the rings in stratiform rain, but **creates artifacts at the edges of upright convective cores** (Zhang 2005 Fig. 13) — hence it's conditional on BB detection.
- **Viewer spec:** default = pure VI everywhere inside coverage. Optional "stratiform mode" (auto if you wire up a BB detector, e.g. from dual-pol §6): VHI in gaps > 1°. Optionally render interior gaps with subtle uncertainty shading (alpha ramp on `min(w1,w2)`), and always blank (or hatch) outside the half-beamwidth envelope — honesty beats invented data above the top tilt.

## 4. CAPPI construction

Same machinery with `h` fixed: for each map pixel, `s, φ` from geodesic to radar, then §1.3 inverse → (r, θ_e), then §2.a. (Equivalently: a CAPPI is one horizontal row of the cross-section sampler swept in 2D.)

Standard altitude grids (verified):
- **MRMS (operational CONUS): 33 levels, 0–20 km MSL; 250 m spacing from 0–3 km, 500 m from 3–9 km, 1000 m from 9–20 km; horizontal 0.01° (~1 km)** (Smith et al. 2016, *BAMS* 97, 1617–1630, doi:10.1175/BAMS-D-14-00173.1 — quoted text).
- Predecessor NMQ: 31 levels, 500 m–18 km MSL (Zhang et al. 2011); earlier 21 levels 1–17 km, 500 m below 5 km / 1000 m above (Zhang et al. 2005 conference companion).
- Spec: offer the MRMS 250/500/1000 ladder as the default CAPPI picker; heights are MSL (convert with antenna elevation). Distinguish true CAPPI (blank outside coverage) from "pseudo-CAPPI" (fall back to nearest tilt outside the envelope — label it if you do it).

## 5. Velocity on cross-sections

Radial velocity is a **per-radial projection** of the wind onto the beam: `v_r = u·cosθ_e'·sinφ + v·cosθ_e'·cosφ + (w − w_t)·sinθ_e'` (D&Z 1993; Gao et al. 2006 Eq. 8). Implications, plus operational practice (Lakshmanan et al. 2006 §3b — they explicitly do **not** objectively analyze velocity like a scalar; they either retrieve winds or merge derived scalars like azimuthal shear):

1. **Single radar only.** Never blend velocities from different radars or compare magnitudes across radars on one slice — different beams measure different projections.
2. **Per-pixel geometry is fine for arbitrary lines:** each slice pixel maps to its own (r, φ, θ_e); sample the velocity field of *that* radial. An arbitrary line crossing many azimuths is legitimate — but the sign ("toward/away") is relative to the radar, so draw the radar's position marker on the slice axis and keep a toward/away legend; when the line passes near the zero isodop the slice legitimately shows a sign change.
3. **Use the Doppler cut in split cuts:** legacy low tilts are scanned twice (CS surveillance → Z; CD Doppler → V/SW with higher PRF and 0.25 km gates). Pair fields by cut type, not by tilt index. **Dedupe SAILS/MESO-SAILS re-visits of 0.5°**: for a static volume slice keep the re-visit closest to the volume midpoint time (or the latest, for "most recent" semantics) — never let two 0.5° cuts both enter the bracket search.
4. **Dealias before slicing** (you already have region-based dealiasing). Vertical interpolation between tilts is then *usually* safe because both samples share the same azimuth and near-equal range, but guard it: `if |v1 − v2| > V_nyquist_min(tilt1,tilt2) → use nearest tilt sample` (a residual fold or true TVS shear should render as a sharp discontinuity, not be averaged into a plausible-looking lie). Range-folded (RF) gates are missing, never 0: if one bracket is RF, use the other (nearest), if both RF, blank.
5. Projection purity: between tilts the measured component direction differs slightly (θ_e' differs by the tilt gap; cosθ' ≥ 0.94 for θ' ≤ 20°). Operational displays ignore this; do the same. (Optional "quasi-horizontal" toggle dividing by cosθ_e' is defensible but nonstandard — off by default.)
6. Spectrum width: scalar, interpolate like reflectivity (linear).

## 6. Dual-pol on slices

**Melting-layer signature** (the thing your slice must not destroy) — Giangrande, Krause & Ryzhkov 2008, *J. Appl. Meteor. Climatol.* **47**, 1354–1364, doi:10.1175/2007JAMC1634.1 (verified):
- ML gates: **ρhv between 0.90 and 0.97**, confirmed by **Z maximum 30–47 dBZ** and **ZDR maximum 0.8–2.5 dB** within a 500 m window above the ρhv dip; climatological cap 6 km.
- Detection runs on **native radials at elevations 4°–10°** — below 4° "ML signatures are smeared due to beam broadening"; ML top = height below which 80% of ML points sit, bottom = 20% (per 21° azimuth sector). Lesson: dual-pol layer analysis is done in radar space at steep tilts, **never on the interpolated Cartesian grid** — if you add an ML overlay, compute it Giangrande-style from radials, then draw the top/bottom heights as lines on the slice.

**Interpolation rules per field:**
- **Z (reflectivity):** Warren & Protat 2019 (*JTECH* 36, 1143–1156, doi:10.1175/JTECH-D-18-0183.1, verified): linear interpolation **in Z (mm⁶ m⁻³) is more accurate on average — especially >30 dBZ / convective cores — while dBZ is better in low-reflectivity monotone regions**; their recommendation: interpolate in Z *"except when identifying echo-top height or other low-reflectivity boundaries."* MRMS interpolates dBZ. Spec: default **dBZ** (matches operational practice and keeps echo tops/colormap behavior tame), expose `interp_units: DbZ | LinearZ` as an engine option, cite both in the comment.
- **ZDR:** a dB-ratio field, smooth-ish in stratiform rain; linear interpolation in dB between bracketing tilts. Never convert to linear units for interpolation (no physical basis; ZDR columns/ML maxima get distorted either way — linear-in-dB is the convention).
- **ρhv (CC) — the non-monotone trap:** bounded (≲1.0) with a sharp ML/non-met minimum. Linear interpolation is a convex combination so it can't overshoot, but it (a) dilutes the ML dip into a thick mushy band and (b) at far range, where the tilt gap (km) exceeds ML thickness (~0.2–0.7 km), can erase the dip entirely between tilts. **Rule: if min(cc1, cc2) < 0.97 (the Giangrande wet-snow/rain discrimination threshold), assign nearest-tilt-in-θ (α<0.5 → lower) instead of blending; else linear.** This preserves the layer's observed magnitude and is consistent with published practice of analyzing ML on native gates. (Same rule serves non-met/clutter CC.)
- **KDP:** already a range-derivative product; interpolate linearly, but never recompute derivatives from interpolated ΦDP on the slice.
- Honest fallback: a "raw mode" (beam-width arcs, §1.2, no interpolation) is the most truthful dual-pol display — Zhang's RBVM — and should stay one keypress away.

---

## 7. Per-pixel slice sampling algorithm (build spec)

### 7.1 Precompute per volume (per radar, per field)
```rust
struct TiltIndex {
    elev_deg: f32,            // mean reported elevation of the cut
    sin_elev: f64, cos_elev: f64,
    az_sorted: Vec<f32>,      // radial azimuths (sorted, wrap-aware) -> radial idx
    gate0_km: f32, gate_dr_km: f32, n_gates: u32,
    nyquist_ms: f32,          // for velocity guard
}
// Volume: tilts sorted ascending by elev_deg, SAILS duplicates collapsed
// (keep cut nearest volume mid-time; pair CS/CD split cuts by field).
const A_E_KM: f64 = 6371.0 * 4.0 / 3.0;      // 8494.667 — Doviak & Zrnic (1993) 4/3 model
const BEAMWIDTH_DEG: f32 = 0.95;             // WSR-88D half-power beamwidth
```

### 7.2 Per pixel column (slice x = ground distance `s`, then ascend in h)
```text
σ  = s / A_E
for each pixel height h (bottom → top):                     // θ_e monotone in h: bracket only walks up
  r  = sqrt(A_E² + (A_E+h)² − 2·A_E·(A_E+h)·cos σ)          // §1.3 exact inverse
  θe = asin(((A_E+h)² − A_E² − r²) / (2·A_E·r))             // radians → degrees
  if r > r_max(field) → blank
  // ---- bracket in elevation
  if θe < θ_lowest:
       if θe ≥ θ_lowest − bw/2 → sample(lowest tilt)        // MRMS half-beamwidth rule
       else → blank                                          // below-beam void
  else if θe > θ_highest:
       if θe ≤ θ_highest + bw/2 → sample(highest tilt)       // cone of silence edge
       else → blank
  else:
       (lo, hi) = bracketing tilts                           // walk-up pointer per column
       α  = (θe − θ_lo) / (θ_hi − θ_lo)                      // Zhang 2005 Eqs. 6–7
       v_lo = sample(lo); v_hi = sample(hi)                  // nearest azimuth + nearest range gate
       combine per field rules (7.3)

sample(tilt): radial = nearest azimuth (binary search az_sorted, wrap),
              gate   = round((r − gate0)/dr); BOTH nearest-neighbor (Zhang 2005 VI).
```

### 7.3 Combine rules per field
```text
REF:  both valid                    → lerp(v_lo, v_hi, α)        // in dBZ (option: linear Z)
      one  valid, other NO-ECHO     → lerp with floor (see 7.4)
      one  valid, other NOT-SCANNED → nearest (the valid one)
VEL:  RF/missing either side       → nearest valid, else blank
      |v_hi − v_lo| > min(Nyq_lo, Nyq_hi) → nearest in θ (α<0.5 ? lo : hi)
      else                          → lerp
SW :  like REF (linear)
ZDR:  lerp in dB
CC :  min(v_lo, v_hi) < 0.97        → nearest in θ               // preserve ML dip / non-met
      else                          → lerp
KDP:  lerp
```

### 7.4 Missing-data semantics (the artifact you must not create)
Distinguish three states end-to-end: **(a)** below SNR threshold but scanned = "no echo" → treat as a numeric floor for REF (e.g. −32 dBZ) so storm tops taper instead of hard-clipping against `missing`; **(b)** not scanned / outside coverage / RF → `missing`, excluded from interpolation (nearest-valid fallback or blank as above); **(c)** outside the half-beamwidth envelope → blank, optionally hatched ("not sampled by VCP"). Never lerp a real dBZ against `missing-as-0` — that's the classic fake-erosion bug at echo top.

### 7.5 Optional layers
- **VHI stratiform branch (§3):** if BB detected and `θ_hi − θ_lo > 1°`, add the two same-height horizontal neighbors with ground-distance weights (Zhang Eqs. 8–10).
- **Raw/RBVM mode:** draw gates as beam arcs `θ_e ± bw/2` (§1.2) with no interpolation.
- **Gap-confidence alpha:** scale saturation by `1 − |2α − 1|`·f(gap) if you want subtle honesty shading.

### 7.6 Performance notes
- Everything in 7.2 before the field sampling is field-independent: compute (r, θe, az, bracket, α) once per pixel and reuse across REF/VEL/CC/ZDR layers of the same slice.
- Per column, σ is constant; `cos σ` hoists out; θe is monotone in h so the bracket pointer only advances — the whole slice is O(W·H) with no per-pixel searches except one azimuth lookup per (tilt, column).
- f64 for the height/range/asin chain only; everything after the bracket can be f32.
- CAPPI = same kernel, h fixed, iterate map pixels; reuse for point soundings (column at one s).

### 7.7 Unit tests
1. Forward/inverse round trip: random (r, θe) → (s, h) → (r, θe) to 1e-9 relative.
2. Beam height anchors: 0.5° @ 100 km → 1.459 km; 0.5° @ 230 km → 5.12 km (+antenna).
3. Bracket weights sum to 1; α at a tilt center == 0/1 reproduces the raw gate exactly.
4. Half-beamwidth envelope: pixel at θ_lowest − 0.6° blank for bw = 0.95°; at −0.4° equals lowest-tilt value.
5. Velocity guard: synthetic fold (v1 = +25, v2 = −25, Nyq 26) → nearest, not 0.
6. CC ML: cc_lo = 0.99 (rain), cc_hi = 0.93 (ML) → 0.93 or 0.99 by α-side, never 0.96 blend.

---

## Sources (primary, all verified against fetched full texts)
- Doviak, R. J., and D. S. Zrnić, 1993: *Doppler Radar and Weather Observations*, 2nd ed., Academic Press (beam-height Eq. 2.28; 4/3 model) — formulas verified via Gao, J., K. Brewster, and M. Xue, 2006: A comparison of the radar ray path equations and approximations for use in radar data assimilation. *Adv. Atmos. Sci.*, **23**, 190–198 (Eqs. 1, 3–4, 6, 14).
- Zhang, J., K. Howard, and J. J. Gourley, 2005: Constructing three-dimensional multiple-radar reflectivity mosaics. *J. Atmos. Oceanic Technol.*, **22**, 30–42, doi:10.1175/JTECH-1689.1 (VI/VHI Eqs. 5–10; >1° VHI trigger; no-extrapolation rule; beam-size numbers; VCP table).
- Zhang, J., et al., 2011: National Mosaic and Multi-Sensor QPE (NMQ) system. *Bull. Amer. Meteor. Soc.*, **92**, 1321–1338, doi:10.1175/2011BAMS-D-11-00047.1 (half-beamwidth extrapolation limit; 31-level grid; VPR gap-filling).
- Smith, T. M., et al., 2016: Multi-Radar Multi-Sensor (MRMS) severe weather and aviation products. *Bull. Amer. Meteor. Soc.*, **97**, 1617–1630, doi:10.1175/BAMS-D-14-00173.1 (33-level 0–20 km grid, 250/500/1000 m spacing).
- Lakshmanan, V., T. Smith, K. Hondl, G. J. Stumpf, and A. Witt, 2006: A real-time, three-dimensional, rapidly updating, heterogeneous radar merger technique. *Wea. Forecasting*, **21**, 802–823, doi:10.1175/WAF942.1 (exponential elevation weight Eq. 6; velocity-merging practice; bright-band caveat).
- Askelson, M. A., J.-P. Aubagnac, and J. M. Straka, 2000: An adaptation of the Barnes filter applied to the objective analysis of radar data. *Mon. Wea. Rev.*, **128**, 3050–3082 (A-B weight Eq. 3; anisotropic-spacing insight).
- Trapp, R. J., and C. A. Doswell III, 2000: Radar data objective analysis. *J. Atmos. Oceanic Technol.*, **17**, 105–120 (five conclusions; Cressman sidelobes; constant-κ Barnes recommendation).
- Mohr, C. G., and R. L. Vaughan, 1979: An economical procedure for Cartesian interpolation and display of reflectivity factor data. *J. Appl. Meteor.*, **18**, 661–670 (SPRINT bilinear baseline; via James et al. 2000, *Wea. Forecasting* **15**, 327–338, and Trapp & Doswell 2000).
- Giangrande, S. E., J. M. Krause, and A. V. Ryzhkov, 2008: Automatic designation of the melting layer with a polarimetric prototype of the WSR-88D radar. *J. Appl. Meteor. Climatol.*, **47**, 1354–1364, doi:10.1175/2007JAMC1634.1 (ρhv 0.90–0.97, Z 30–47 dBZ, ZDR 0.8–2.5 dB, 4°–10° tilts, 80/20% boundaries).
- Warren, R. A., and A. Protat, 2019: Should interpolation of radar reflectivity be performed in Z or dBZ? *J. Atmos. Oceanic Technol.*, **36**, 1143–1156, doi:10.1175/JTECH-D-18-0183.1.
- WSR-88D beamwidth ≈0.95°: NWS NEXRAD training (training.weather.gov); effective beamwidths 1.02°/1.29°: Wood & Brown 1997 (cited secondhand).
- Oye, R., C. Mueller, and S. Smith, 1995: Software for radar translation, visualization, editing, and interpolation (REORDER). 27th Conf. Radar Meteor. (cited secondhand — classical reference, not fetched).

## INTERP PAPERS
- Doviak, R. J., and D. S. Zrnić, 1993: Doppler Radar and Weather Observations, 2nd ed., Academic Press — 4/3 effective earth radius beam height/ground range equations (Eq. 2.28)
- Gao, J., K. Brewster, and M. Xue, 2006: A comparison of the radar ray path equations and approximations for use in radar data assimilation. Adv. Atmos. Sci., 23, 190–198 — verification of D&Z formulas, simplified height equation, refraction error analysis, local beam angle Eq. 6
- Zhang, J., K. Howard, and J. J. Gourley, 2005: Constructing three-dimensional multiple-radar reflectivity mosaics: examples of convective storms and stratiform rain echoes. J. Atmos. Oceanic Technol., 22, 30–42, doi:10.1175/JTECH-1689.1 — VI/VHI interpolation Eqs. 5–10, 1° gap trigger, no-extrapolation rule, mosaic weighting
- Zhang, J., et al., 2011: National Mosaic and Multi-Sensor QPE (NMQ) System: description, results, and future plans. Bull. Amer. Meteor. Soc., 92, 1321–1338, doi:10.1175/2011BAMS-D-11-00047.1 — half-beamwidth top/bottom extrapolation limit, 31-level grid, VPR gap filling
- Smith, T. M., et al., 2016: Multi-Radar Multi-Sensor (MRMS) severe weather and aviation products: initial operating capabilities. Bull. Amer. Meteor. Soc., 97, 1617–1630, doi:10.1175/BAMS-D-14-00173.1 — 33-level 0–20 km MSL grid: 250 m (0–3 km), 500 m (3–9 km), 1000 m (9–20 km)
- Lakshmanan, V., T. Smith, K. Hondl, G. J. Stumpf, and A. Witt, 2006: A real-time, three-dimensional, rapidly updating, heterogeneous radar merger technique for reflectivity, velocity, and derived products. Wea. Forecasting, 21, 802–823, doi:10.1175/WAF942.1 — w2merger exponential elevation weight δe = exp[α³ ln 0.005], velocity merging practice
- Askelson, M. A., J.-P. Aubagnac, and J. M. Straka, 2000: An adaptation of the Barnes filter applied to the objective analysis of radar data. Mon. Wea. Rev., 128, 3050–3082, doi:10.1175/1520-0493(2000)128<3050:AAOTBF>2.0.CO;2 — direction-split adaptive Barnes weight, range-dependent smoothing insight
- Trapp, R. J., and C. A. Doswell III, 2000: Radar data objective analysis. J. Atmos. Oceanic Technol., 17, 105–120, doi:10.1175/1520-0426(2000)017<0105:RDOA>2.0.CO;2 — Cressman sidelobe and range-varying-smoothing pitfalls; constant-parameter isotropic Barnes recommendation
- Mohr, C. G., and R. L. Vaughan, 1979: An economical procedure for Cartesian interpolation and display of reflectivity factor data in three-dimensional space. J. Appl. Meteor., 18, 661–670 — SPRINT bilinear-in-radar-space baseline
- Giangrande, S. E., J. M. Krause, and A. V. Ryzhkov, 2008: Automatic designation of the melting layer with a polarimetric prototype of the WSR-88D radar. J. Appl. Meteor. Climatol., 47, 1354–1364, doi:10.1175/2007JAMC1634.1 — MLDA: ρhv 0.90–0.97, Z 30–47 dBZ, ZDR 0.8–2.5 dB, elevations 4°–10°, 80/20 percentile ML boundaries
- Warren, R. A., and A. Protat, 2019: Should interpolation of radar reflectivity be performed in Z or dBZ? J. Atmos. Oceanic Technol., 36, 1143–1156, doi:10.1175/JTECH-D-18-0183.1 — Z better >30 dBZ/convective cores, dBZ better for low-Z/echo-top boundaries
- Gourley, J. J., and C. M. Calvert, 2003: Automated detection of the bright band using WSR-88D data. Wea. Forecasting, 18, 585–599 — bright-band detector used to gate Zhang's VHI scheme
- James, C. N., S. R. Brodzik, H. Edmon, R. A. Houze Jr., and S. E. Yuter, 2000: Radar data processing and visualization over complex terrain. Wea. Forecasting, 15, 327–338 — confirms SPRINT bilinear interpolation usage
- Oye, R., C. Mueller, and S. Smith, 1995: Software for radar translation, visualization, editing, and interpolation. 27th Conf. on Radar Meteorology — REORDER (Cressman/exponential ROI), cited secondhand
- Wood, V. T., and R. A. Brown, 1997: Effects of radar sampling on single-Doppler velocity signatures of mesocyclones and tornadoes. Wea. Forecasting — effective beamwidths 1.02° (0.5° sampling) / 1.29° (1.0° sampling), cited secondhand

## VOLUMETRIC 3D VISUALIZATION
# Volumetric 3D Radar Visualization for BowEcho — Research Report + v1 Implementation Spec

Scope: desktop NEXRAD viewer, Rust + egui 0.34/eframe (both `egui_glow` and `egui-wgpu` are already in rusty-weather's Cargo.lock, so an egui paint-callback in either backend is available). Benchmark: GR2Analyst's Volume Explorer. All load-bearing claims below were checked against primary sources (GRLevelX's own documentation pages, the GR2Analyst manual, AMS journal papers); inferences are flagged as such.

---

## 1. What GR2Analyst's Volume Explorer actually renders

**Verified from GRLevelX's own docs** ([Volume Renderer in GR2Analyst 3](http://www.grlevelx.com/gr2analyst_3/volume_renderer.htm), [Using the Volume Renderer](https://www.grlevelx.com/gr2analyst_3/using_volume_renderer.htm), [Volume Toolbar manual page](http://www.grlevelx.com/manuals/gr2analyst/volume_toolbar.htm), [Volume Alpha window](http://www.grlevelx.com/manuals/gr2analyst/window_volume_alpha.htm), [Volume Light window](http://www.grlevelx.com/manuals/gr2analyst/window_volume_light.htm), and the [Iowa State GR2Analyst manual PDF](https://cumulus.geol.iastate.edu/mteor411/GR_Manual.pdf), pp. 19–21):

- **It is NOT marching cubes.** GRLevelX states verbatim: *"GR2Analyst uses the latest hardware-based direct volume rendering techniques to produce high quality radar volume displays in real time."* It is GPU **direct volume rendering (DVR)** of a resampled sub-volume, with two view modes:
  - **Lit Volume** — translucent DVR driven by a user-editable **alpha (transfer-function) table**: the user click-drags an opacity curve over the product color table (0–100%). Diffuse/ambient lighting with a movable light (world-space or eye-space origin, azimuth + tilt sliders, "Type" slider from Diffuse=max shadows to Ambient=none).
  - **Isosurface** — a single isovalue, set by dragging a pointer along the color table in the Volume Alpha window. In DVR terms this is a first-hit/sharp-transfer-function render of the same sampled volume, not a mesh extraction (inference from it being a mode of the same renderer; the docs never mention geometry extraction).
- **Resampling grid (the key number):** the Volume Settings dialog sets *"the number of samples along the x/y and z axes. The default values are 92 and 64"* — i.e., a **92×92×64 sub-volume**, raisable to *"128 or higher"* on fast GPUs, with the warning that *"at 128x128x64 each viewing axis uses 12MB of system and video memory."* The phrase "each viewing axis" is the classic fingerprint of **three axis-aligned 2D-texture slice stacks** (the pre-3D-texture DVR technique of Cabral et al. 1994) — flagged as inference, but the memory math matches. The takeaway either way: **the gold standard renders a ~92³-class volume, not a 460-km Cartesian cube.** Sub-volume resampling at fixed sample count *is* GR2A's LOD strategy.
- **Default thresholds and pro alpha tables** (verbatim from "Using the Volume Renderer"):
  - Default reflectivity alpha table is "a simple ramp from zero to 100%".
  - **Tornado table:** everything below **50 dBZ transparent**, ≥50 dBZ opaque ("Basically, we have created an isosurface at 50 dbz"), plus a thin **25%-alpha "halo" at 43–44 dBZ** to show storm shape; suggested isosurface-mode isovalue **~52 dBZ**, viewed from the east looking down at ~60°.
  - **Hail table:** **60+ dBZ fully opaque** with a semi-transparent **50 dBZ halo** — the operative rule being *"look for 60 dbz regions above the freezing level."* For extreme hail: **70+ dBZ opaque with a 60 dBZ halo**.
  - Pro reads from these displays: the 50+ dBZ overhang → descending reflectivity core (DRC) "finger" → debris-blob "tube" sequence for tornadoes; hail cores reaching the ground in "two or three volume scans (10–15 minutes)".
- **Interactions pros actually use:**
  - **Volume Mouse mode:** drag a box over a cell on the 2D map → GR2A samples that area → Volume Explorer window pops up. (Storm-scale box selection, not whole-radar.)
  - **Click-drag rotate, scroll-wheel zoom** (Iowa State manual p. 21).
  - **Isovalue slider drag** and live alpha-curve editing; up to **16 saved alpha tables** (right-click → Save Alpha Table).
  - **VCR animation buffer** (oldest/prev/next/latest volume) + Refresh — pros animate hail cores through successive volume scans.
  - **Storm-motion advection correction:** *"GR2Analyst automatically corrects for this when sampling the radar volume by using the storm motion vector to slide the upper tilts backwards along the direction of motion"* — this is the "tilt with storm" feature; without it a 25-kt cell smears ~3 km over a 4–5-min VCP and the volume looks sheared.
  - **Reference furniture:** two height walls with a bar every **10,000 ft**, a **yellow 0°C line and red −20°C line** on the walls, cardinal direction markers (Iowa State manual). The freezing-level lines are what make the "60 dBZ above the freezing level" hail read possible at a glance.
  - **Products:** base reflectivity, base velocity, storm-relative velocity, spectrum width (+ a "rotation" product per the GR2AE manual). Volume SRV in isosurface mode is the documented tornado-shear read.

## 2. 3D gridding for visualization: published practice

**Cartesian gridding (the standard):**
- **MRMS** is the operational reference: ~0.01° (~1 km) horizontal, **33 vertical levels spanning 0–20 km MSL** (stretched spacing, finer at low levels), 2-min updates over CONUS (3500×7000 columns) — [Smith et al. 2016, BAMS 97(9), "Multi-Radar Multi-Sensor (MRMS) Severe Weather and Aviation Products: Initial Operating Capabilities"](https://journals.ametsoc.org/view/journals/bams/97/9/bams-d-14-00173.1.xml), DOI 10.1175/BAMS-D-14-00173.1; system overview in Zhang et al. 2016, BAMS 97(4), DOI 10.1175/BAMS-D-14-00174.1; [NSSL MRMS](https://www.nssl.noaa.gov/projects/mrms/). Note 33 levels ≈ 0.6 km mean spacing but NOT uniform 0.5 km.
- **Single-radar remap methodology:** [Zhang, Howard & Gourley 2005, JTECH 22, 30–42, "Constructing Three-Dimensional Multiple-Radar Reflectivity Mosaics"](https://journals.ametsoc.org/view/journals/atot/22/1/jtech-1689_1.xml), DOI 10.1175/JTECH-1689.1 — compares nearest-neighbor, vertical interpolation, and Barnes-type schemes for polar→Cartesian remap at ≤1 km / ≤5 min; this is the canonical "how to fill between tilts" paper and documents the artifacts (Section 3 below).
- **Objective-analysis tradeoffs:** [Trapp & Doswell 2000, JTECH 17, 105–120, "Radar Data Objective Analysis"](https://journals.ametsoc.org/jtech/article/17/2/105/104346/Radar-Data-Objective-Analysis) — radar data spacing varies wildly with range/direction; an isotropic Gaussian (Barnes) weight with **constant** smoothing parameter behaves most predictably; Cressman/nearest produce phase/amplitude artifacts.
- **Interpolation domain:** [Warren & Protat 2019, JTECH 36(6), 1143–1156, "Should Interpolation of Radar Reflectivity be Performed in Z or dBZ?"](https://journals.ametsoc.org/view/journals/atot/36/6/jtech-d-18-0183.1.xml), DOI 10.1175/JTECH-D-18-0183.1 — **interpolate in linear Z for severe convection** (better preserves >30 dBZ cores); dBZ only better for weak echo. For a hail/BWER viewer: interpolate in Z.
- **Open-source precedent:** Py-ART's [`grid_from_radars` / `map_to_grid`](https://arm-doe.github.io/pyart/API/generated/pyart.map.grid_from_radars.html) (Barnes2/Cressman/Nearest with radius-of-influence control; [Helmus & Collis 2016, J. Open Research Software 4, DOI 10.5334/jors.119](https://openresearchsoftware.metajnl.com/articles/10.5334/jors.119)); LROSE Radx2Grid does the same offline. **Unidata IDV** is the closest open precedent to GR2A: its [Level II isosurface display](https://www.unidata.ucar.edu/software/idv/2_7u2/docs/workshop/datadisplays/level2/LevelIIIsosurfaceDisplays.html) auto-generates a 3D Cartesian grid from the radial volume using an **8-point weighted interpolation**, then renders isosurfaces/volume via VisAD ([IDV radar docs](https://docs.unidata.ucar.edu/idv/current/userguide/examples/Radar.html)).

**Skipping Cartesian gridding (direct polar rendering):**
- [Liang, Gong, Li & Nasser 2014, Computers & Geosciences 68, 81–91, "Visualizing 3D atmospheric data with spherical volume texture on virtual globes"](https://www.sciencedirect.com/science/article/abs/pii/S0098300414000752), DOI 10.1016/j.cageo.2014.03.015 (C&G 2014 Best Paper) — GPU ray casting that samples a **"spherical volume texture"** directly in lon/lat/alt, explicitly to avoid over/undersampling from Cartesian reprojection. Same idea transfers to radar (r, az, el).
- [spherical-volume-rendering/svr-algorithm (GitHub)](https://github.com/spherical-volume-rendering/svr-algorithm) — open-source ray casting through a spherical voxel grid, an Amanatides & Woo (1987 Eurographics, "A Fast Voxel Traversal Algorithm for Ray Tracing") traversal generalized to spherical shells.
- Ultrasound rendering literature has long rendered native polar volumes without scan conversion (e.g., [US Patent 6,723,050, polar-coordinate volume rendering](https://image-ppubs.uspto.gov/dirsearch-public/print/downloadPdf/6723050)); cylindrical-grid DVR for radar also published ([Cylindrical Volume Rendering on Radar Information, ResearchGate](https://www.researchgate.net/publication/251912999_Cylindrical_Volume_Rendering_on_Radar_Information_in_Complicated_Environment)).
- Practical fast path for a viewer: you don't ray-march polar space *or* build a big Cartesian grid — you do what GR2A does: **resample a storm-scale box into a small regular grid by inverse-mapping each voxel to (gate, azimuth, elevation) and interpolating the two bracketing sweeps.** That's O(voxels), embarrassingly parallel, and exact w.r.t. the 4/3-earth-radius beam model. Aron Ernvik's master's thesis ([Ernvik 2002, Linköping University, "3D Visualization of Weather Radar Data"](https://www.semanticscholar.org/paper/3D-Visualization-of-Weather-Radar-Data-Ernvik/8d6daf781e11de1a4db3af3c1acf9a06bf1e768d)) implemented exactly this trio (slices, isosurface extraction, DVR) for Swedish radar and remains the best end-to-end writeup.
- **Competitive note:** a GitHub issue search of dpaulat/supercell-wx for "3d volume" returns no feature work — no open-source desktop NEXRAD viewer currently ships a GR2A-class volume display. This is open differentiation territory for BowEcho.

## 3. Marching cubes on radar grids: artifacts and mitigations

Marching cubes itself: Lorensen & Cline 1987, SIGGRAPH (Computer Graphics 21(4), 163–169), DOI 10.1145/37402.37422. Radar-specific failure modes, all rooted in WSR-88D scan geometry (14 elevations, 0.5°–19.5° in VCP 12/212; ~4–6 min per volume):

1. **Inter-tilt gaps / vertical stretching.** Elevation gaps reach ~2.5–4° aloft; at 100 km range a 2.5° gap is ~4.4 km of unsampled depth. Naive nearest-tilt assignment produces stair-stepped "wedding cake" isosurfaces; linear interpolation across the gap produces vertically smeared, cone-shaped sheets (the smooth "GR2A look" is exactly linear-in-elevation-angle interpolation). Zhang et al. 2005 documents both artifact families and shows vertical interpolation choices change echo-top/core shape materially. Mitigation: interpolate **linearly in elevation angle between bracketing sweeps along the same (r, az) ray** (not linearly in z between stacked CAPPIs), in **linear Z** units (Warren & Protat 2019), and render honestly — don't over-smooth to hide the cones; pros know what beam spacing looks like.
2. **Cone of silence.** No data above 19.5° elevation: near the radar, storm tops are unsampled. If you grid-fill with 0 dBZ, marching cubes fabricates a flat "lid" on every nearby storm. Mitigation: keep a **validity mask** (voxel above top beam, below lowest beam, or beyond range = NaN), run MC only on cells with 8 valid corners, and let surfaces terminate open. GR2A shows the same open-topped cores. Optionally render the cone boundary as faint furniture so the user knows why a top is missing.
3. **Echo-top quantization.** The apparent top of an isosurface is biased to the highest tilt that still exceeds threshold; true tops sit between tilts. [Lakshmanan, Hondl, Potvin & Preignitz 2013, WAF 28(2), 459–465, "An Improved Method for Estimating Radar Echo-Top Height"](https://journals.ametsoc.org/view/journals/wefo/28/2/waf-d-12-00084_1.xml), DOI 10.1175/WAF-D-12-00084.1 — interpolate between the bracketing tilts assuming a linear vertical reflectivity profile near top. The elevation-angle interpolation in (1) gives you this for free inside the volume; use the Lakshmanan formula explicitly for any 2.5D echo-top *surface* product.
4. **Storm motion shear.** Upper tilts are scanned minutes after the lowest; advect each sweep by `-(t_sweep - t_ref) * V_storm` before sampling (GR2A does this, quoted above).
5. **Range-dependent resolution.** Super-res gates are 250 m × 0.5°; at 150 km a gate is ~1.3 km wide. A fixed-resolution storm box (Section 6) sidesteps the worst of Trapp & Doswell's variable-spacing problem because the box spans a narrow range interval; use a small Gaussian (σ ≈ one gate/beam width) only if aliasing is visible.

## 4. Performance envelope

The decisive observation: **GR2Analyst ran its volume display in real time on 2005-era GPUs at 92×92×64 samples (12 MB/axis at 128×128×64).** A 2026 consumer GPU has four orders of magnitude more headroom. Concretely:

- A 96×96×64 f32 grid is **2.25 MB**; 128³ is 8 MB. A 3D-texture raymarch of 128³ at 1080p runs far beyond 60 fps on integrated graphics (browser-grade WebGL demos do it: [Usher, "Volume Rendering with WebGL"](https://www.willusher.io/webgl/2019/01/13/volume-rendering-with-webgl/)).
- GPU marching cubes hit >100 fps on 256³ volumes in **2008**: [Dyken, Ziegler, Theobalt & Seidel 2008, Computer Graphics Forum 27, 2028–2039, "High-speed Marching Cubes using HistoPyramids"](https://onlinelibrary.wiley.com/doi/10.1111/j.1467-8659.2008.01182.x), DOI 10.1111/j.1467-8659.2008.01182.x. You don't even need the GPU: CPU MC over 96×96×64 (~578k cells, typically <5% active at 50 dBZ) is single-digit milliseconds in Rust with rayon.
- **LOD practice in operational viewers = fixed-budget resampling, not mesh decimation.** GR2A resamples whatever box you select into the same 92×92×64 texture; IDV grids the radial volume once at fixed resolution. Nobody runs MC over a CONUS MRMS cube for interactive display; storm-scale boxes are the unit of interaction. For a whole-radar "overview isosurface" later, a 0.5–1 km grid out to 230 km × 18 km depth (460×460×36 at 1 km) is 7.6M voxels ≈ 30 MB f32 — still raymarchable at 60 fps on a mid-range discrete GPU, with sampling (not rendering) as the slow step (~100–300 ms multithreaded, do it once per volume scan).

Budget conclusion: at storm-box scale every stage fits inside one frame; at full-radar scale only the resample needs to be async.

## 5. Severe-weather payoff features: display-only vs. new algorithms

| Feature | What pros do with it | Display-only or algorithm? |
|---|---|---|
| **Hail core isosurface (50/60/70 dBZ)** | "60 dBZ above the freezing level" rule; watch core descend over 2–3 volume scans to time hail onset (GRLevelX docs) | **Display-only** — isosurface + the 0°C/−20°C reference lines. BowEcho already computes freezing-level inputs for SHI/MESH; reuse them. |
| **BWER/vault** | Look up into the low-reflectivity vault surrounded by the 50 dBZ overhang; strength of updraft ∝ dBZ surrounding the vault ([COMET BWER module](https://www.faculty.luther.edu/~bernatzr/Courses/Sci123/comet/radar/severe_signatures/print_bwer.htm); [Wikipedia BWER](https://en.wikipedia.org/wiki/Bounded_weak_echo_region)) | **Display-only** to *see* it (translucent shell or cutaway/underside view). Automated *detection* is an algorithm: [Lakshmanan 2000, J. Appl. Meteor. 39(2), 222–230](https://journals.ametsoc.org/view/journals/apme/39/2/1520-0450_2000_039_0222_uagatt_2.0.co_2.xml); fuzzy-rule version Pal et al. 2006, JAMC 45, 1304. Not needed for v1. |
| **DRC / tornado "tube"** | Overhang → descending finger → ground contact sequence (GRLevelX tornado page); SRV isosurface for shear couplet extent | **Display-only** (reflectivity + SRV volumes). |
| **Echo-top surface (18/30/50 dBZ)** | Storm-top trends, collapse detection ([WDTD xx dBZ Echo Top](https://vlab.noaa.gov/web/wdtd/-/xx-dbz-echo-top-et-)) | **Small algorithm**: per-column topmost-exceedance with Lakshmanan et al. 2013 inter-tilt interpolation; render as a 2.5D height-colored sheet. |
| **ZDR column in 3D** | Updraft proxy: ZDR ≥ 1 dB extending above the 0°C level; depth correlates with updraft intensity and precedes hail/intensification ([Snyder, Ryzhkov, Kumjian, Khain & Picca 2015, WAF 30(6), 1819–1844](https://journals.ametsoc.org/view/journals/wefo/30/6/waf-d-15-0068_1.xml), DOI 10.1175/WAF-D-15-0068.1) | **Display is free** once the volume pipeline samples ZDR (isosurface at +1 dB, masked below 0°C height). Snyder's *detection/depth product* is an algorithm — phase 3. |

The pattern: **one good sampled volume + isosurface/DVR renderer unlocks four of five pro features with zero new science.** That is why the volume pane is high-leverage.

## 6. Recommended v1 for BowEcho: "Storm Volume" pane

**Smallest feature, biggest pro payoff:** a box-select isosurface viewer replicating GR2A's documented workflow — Volume-Mouse-style drag on the map → pane opens with a lit 50 dBZ isosurface you can rotate, re-threshold, and animate, with 10-kft height walls and freezing-level lines. Marching cubes (not DVR) for v1: it is exactly the "tornado table = isosurface at 50" read pros describe, it needs no transfer-function UI, meshes cache per frame for VCR animation, and a triangle mesh is the easiest thing to draw correctly from an egui paint callback on both glow and wgpu backends. DVR "Lit Volume" mode is phase 2.

### 6.1 Pipeline

```
sweeps (r, az, el) ──advect──> box resample (96×96×64 f32 + mask) ──MC──> mesh ──paint callback──> pane
                     per-sweep Δt        rayon, linear-Z              rayon      arcball + slider
```

1. **Box select:** drag on map (reuse the Ctrl-click/selection plumbing). Default box **60×60 km**, clamp 20–120 km; vertical 0–18 km AGL fixed. Grid **96×96×64** (Δxy = 0.625 km at 60 km box, Δz = 0.28 km — finer than MRMS everywhere; cf. Smith et al. 2016).
2. **Resample** (per volume scan, once; cite in code: Zhang et al. 2005; Warren & Protat 2019; Doviak & Zrnić 4/3-earth beam model):
   - Per column (x,y): great-circle range `s`, azimuth `az` from radar; per z: invert the 4/3-earth-radius beam-height equation to elevation angle `el(z, s)`.
   - Find bracketing sweeps `el_lo ≤ el ≤ el_hi`; bilinear sample each sweep in (az, gate) **in linear Z (mm⁶ m⁻³) for values ≥ ~30 dBZ** (Warren & Protat 2019), then lerp in elevation angle. Convert back to dBZ.
   - **Advection correction (GR2A parity):** before sampling sweep k, shift the query point by `+(t_k − t_lowest) · V_storm` using BowEcho's existing storm-motion estimate; expose as a toggle.
   - **Mask:** el above top tilt + ½ beamwidth, below lowest tilt − ½ beamwidth, or s > max range ⇒ NaN. MC skips cells with any NaN corner (open surfaces at the cone of silence — honest, like GR2A).
   - Budget: 589k voxels × (2 bilinear + 1 lerp) ≈ **2–6 ms** with rayon on 8 cores; 2.25 MB/grid. Keep a ring of the last 12 grids (27 MB) for VCR animation.
3. **Marching cubes** (Lorensen & Cline 1987): plain 256-case table MC with vertex interpolation in dBZ; normals from central-difference gradient of the grid (smooth shading). At 50 dBZ on a supercell expect ~20–60k active cells → **40–150k triangles**, ~1–4 ms with rayon. Re-run on every slider tick — it's cheap enough for 60 Hz threshold scrubbing. (Rust: hand-roll or use the `isosurface` crate; GPU HistoPyramids per Dyken et al. 2008 is overkill at this size.)
4. **Render** via `egui::PaintCallback`:
   - glow path (matches eframe default): one VAO, position+normal interleaved (24 B/vertex), single directional+ambient shader, depth test on. wgpu path identical via `egui-wgpu` CallbackTrait. Mesh upload worst case ~10 MB — trivial.
   - **Two-surface hail mode** (GR2A hail-table parity): inner opaque mesh at 60 dBZ + outer mesh at 50 dBZ drawn translucent (α≈0.3, depth-write off, draw after opaque, backface-then-frontface for correct-enough compositing of a single shell).
   - **Furniture (do not skip — it's what makes it a tool, not a demo):** corner height walls with ticks every 10 kft; **0°C (yellow) and −20°C (red) lines** sourced from BowEcho's existing model-layer/sounding data (same inputs as SHI/MESH); N/E/S/W markers; ground plane with the box's base-tilt reflectivity image textured on it for orientation.
   - **Interactions:** left-drag arcball rotate, wheel zoom, right-drag pan; isovalue slider 5–75 dBZ defaulting to **50 dBZ** with preset chips 40/50/60/65 + "Hail (60 in 50 shell)"; VCR buttons bound to the rolling store (meshes cached per frame per threshold); product dropdown REF → then SRV/velocity/SW (same pipeline, no interp-domain change needed for velocity beyond staying in m/s — note velocity volumes need the dealiased field from the region-based dealiaser).
5. **Threading:** resample + MC on a worker (same pattern as IngestWorker); UI thread only uploads buffers. Target **< 50 ms box-select-to-first-surface**, < 5 ms re-mesh on slider drag, 60 fps rotation always (it's a static VBO).

### 6.2 Explicit non-goals for v1 (phase 2/3 seeds)
- Lit Volume DVR mode (3D-texture raymarch + alpha-table editor widget — GR2A parity feature, wgpu backend preferred; technique per Liang et al. 2014 if ever done in geo-coordinates).
- Whole-radar isosurface overview (async 460×460×36 resample per scan).
- ZDR column isosurface (+1 dB above 0°C; display-only once dual-pol fields flow through the same sampler; detection per Snyder et al. 2015 later).
- Echo-top 2.5D sheet (Lakshmanan et al. 2013 interpolation).
- BWER auto-detection (Lakshmanan 2000) — v1 gives the *visual* vault via the hail two-surface mode and underside camera preset.

### 6.3 Acceptance tests
- KTLX 1999-05-03 23:51 UTC (the Moore case GR2A's own docs use, file ships with GR2A): 50 dBZ isosurface must show the documented overhang + descending tube with storm motion 236°/25 kt correction on.
- KEAX 2026-06-09 derecho scans (already the local dealias validation set): hail mode must show 60-in-50 cores; rotation at 60 fps; re-mesh under 5 ms.
- Cone-of-silence honesty: a cell within 30 km of the radar must render an open top, not a lid.

---

## References

**Primary GR2Analyst sources:** [GR2A 3 Volume Renderer](http://www.grlevelx.com/gr2analyst_3/volume_renderer.htm) · [Using the Volume Renderer](https://www.grlevelx.com/gr2analyst_3/using_volume_renderer.htm) · [Volume Toolbar](http://www.grlevelx.com/manuals/gr2analyst/volume_toolbar.htm) · [Volume Alpha Window](http://www.grlevelx.com/manuals/gr2analyst/window_volume_alpha.htm) · [Volume Light Window](http://www.grlevelx.com/manuals/gr2analyst/window_volume_light.htm) · [Iowa State GR2Analyst manual (PDF, pp. 19–21)](https://cumulus.geol.iastate.edu/mteor411/GR_Manual.pdf) · [GR2AE Volume Explorer intro (AllisonHouse)](https://support.allisonhouse.com/hc/en-us/articles/206870303--GR2AE-Introduction-to-the-Volume-Explorer)

**Papers:** see the structured `papers` list accompanying this report (author, year, venue, DOI). Key supporting docs: [Py-ART grid_from_radars](https://arm-doe.github.io/pyart/API/generated/pyart.map.grid_from_radars.html) · [IDV Level II isosurface workshop](https://www.unidata.ucar.edu/software/idv/2_7u2/docs/workshop/datadisplays/level2/LevelIIIsosurfaceDisplays.html) · [IDV radar displays](https://docs.unidata.ucar.edu/idv/current/userguide/examples/Radar.html) · [NSSL MRMS](https://www.nssl.noaa.gov/projects/mrms/) · [WDTD xx dBZ Echo Top](https://vlab.noaa.gov/web/wdtd/-/xx-dbz-echo-top-et-) · [COMET BWER signature](https://www.faculty.luther.edu/~bernatzr/Courses/Sci123/comet/radar/severe_signatures/print_bwer.htm) · [svr-algorithm spherical voxel ray casting](https://github.com/spherical-volume-rendering/svr-algorithm) · [Ernvik 2002 thesis (Semantic Scholar)](https://www.semanticscholar.org/paper/3D-Visualization-of-Weather-Radar-Data-Ernvik/8d6daf781e11de1a4db3af3c1acf9a06bf1e768d) · [supercell-wx GitHub](https://github.com/dpaulat/supercell-wx) (no 3D volume feature work found in issue search)

**Confidence notes:** GR2A rendering technique ("direct volume rendering"), 92×92×64 default, alpha-table thresholds, advection correction, and all interactions are verbatim from GRLevelX/manual sources (high confidence). The "three axis-aligned slice stacks" implementation detail is an inference from the per-viewing-axis memory statement (medium confidence; does not affect the spec). MRMS 33-level vertical spacing is stretched, not uniform 0.5 km (verified against Smith et al. 2016 search corroboration). AllisonHouse article returned HTTP 403 and was corroborated only via search excerpts.

## VIZ PAPERS
- Lorensen, W. E., and H. E. Cline, 1987: Marching Cubes: A high resolution 3D surface construction algorithm. Computer Graphics (SIGGRAPH '87), 21(4), 163-169, DOI 10.1145/37402.37422
- Zhang, J., K. Howard, and J. J. Gourley, 2005: Constructing Three-Dimensional Multiple-Radar Reflectivity Mosaics: Examples of Convective Storms and Stratiform Rain Echoes. J. Atmos. Oceanic Technol., 22(1), 30-42, DOI 10.1175/JTECH-1689.1
- Smith, T. M., et al., 2016: Multi-Radar Multi-Sensor (MRMS) Severe Weather and Aviation Products: Initial Operating Capabilities. Bull. Amer. Meteor. Soc., 97(9), 1617-1630, DOI 10.1175/BAMS-D-14-00173.1
- Zhang, J., et al., 2016: Multi-Radar Multi-Sensor (MRMS) Quantitative Precipitation Estimation: Initial Operating Capabilities. Bull. Amer. Meteor. Soc., 97(4), 621-638, DOI 10.1175/BAMS-D-14-00174.1
- Trapp, R. J., and C. A. Doswell III, 2000: Radar Data Objective Analysis. J. Atmos. Oceanic Technol., 17(2), 105-120, DOI 10.1175/1520-0426(2000)017<0105:RDOA>2.0.CO;2
- Warren, R. A., and A. Protat, 2019: Should Interpolation of Radar Reflectivity be Performed in Z or dBZ? J. Atmos. Oceanic Technol., 36(6), 1143-1156, DOI 10.1175/JTECH-D-18-0183.1
- Lakshmanan, V., K. Hondl, C. K. Potvin, and D. Preignitz, 2013: An Improved Method for Estimating Radar Echo-Top Height. Wea. Forecasting, 28(2), 459-465, DOI 10.1175/WAF-D-12-00084.1
- Snyder, J. C., A. V. Ryzhkov, M. R. Kumjian, A. P. Khain, and J. Picca, 2015: A ZDR Column Detection Algorithm to Examine Convective Storm Updrafts. Wea. Forecasting, 30(6), 1819-1844, DOI 10.1175/WAF-D-15-0068.1
- Lakshmanan, V., 2000: Using a Genetic Algorithm to Tune a Bounded Weak Echo Region Detection Algorithm. J. Appl. Meteor., 39(2), 222-230, DOI 10.1175/1520-0450(2000)039<0222:UAGATT>2.0.CO;2
- Pal, N. R., et al., 2006: Fuzzy Rule-Based Approach for Detection of Bounded Weak-Echo Regions in Radar Images. J. Appl. Meteor. Climatol., 45(9), 1304-1312
- Dyken, C., G. Ziegler, C. Theobalt, and H.-P. Seidel, 2008: High-speed Marching Cubes using HistoPyramids. Computer Graphics Forum, 27(8), 2028-2039, DOI 10.1111/j.1467-8659.2008.01182.x
- Liang, J., J. Gong, W. Li, and I. A. Nasser, 2014: Visualizing 3D atmospheric data with spherical volume texture on virtual globes. Computers & Geosciences, 68, 81-91, DOI 10.1016/j.cageo.2014.03.015 (C&G 2014 Best Paper Award)
- Helmus, J. J., and S. M. Collis, 2016: The Python ARM Radar Toolkit (Py-ART), a Library for Working with Weather Radar Data in the Python Programming Language. J. Open Research Software, 4(1), e25, DOI 10.5334/jors.119
- Amanatides, J., and A. Woo, 1987: A Fast Voxel Traversal Algorithm for Ray Tracing. Proc. Eurographics '87, 3-10
- Cabral, B., N. Cam, and J. Foran, 1994: Accelerated volume rendering and tomographic reconstruction using texture mapping hardware. Proc. 1994 Symposium on Volume Visualization, 91-98, DOI 10.1145/197938.197972
- Ernvik, A., 2002: 3D Visualization of Weather Radar Data. M.S. thesis, Linkoping University (LiTH-ISY-EX-3252-2002)

## VERIFICATION: INTERP
# Adversarial Verification Report — Radar Volume Interpolation Spec

**Method:** I re-fetched and text-extracted the actual papers (Zhang et al. 2005 JTECH; Zhang et al. 2011 BAMS; Lakshmanan et al. 2006 WAF; Askelson et al. 2000 MWR; Trapp & Doswell 2000 JTECH; Giangrande et al. 2008 JAMC; Warren & Protat 2019 JTECH; Smith et al. 2016 BAMS; Gao, Brewster & Xue 2006 AAS + their CAPS JTECH draft; Zhang et al. 2005 AMS conference companion 81781; James et al. 2000 WAF) and checked every quoted equation/number against the extracted text. I also re-derived/numerically tested the geometry in f64. Extracted texts live at `C:\Users\drew\AppData\Local\Temp\{zhang2005,zhang2011,lak2006,askelson2000,trapp2000,giangrande2008,warren2019,smith2016mrms,gao2006,nmq2005conf,james2000}.txt` (PDFs alongside).

**Bottom line:** the spec is remarkably faithful — every verbatim quote I checked matches character-for-character, all three Zhang/Lakshmanan weight formulas are exact, the Zhang-2011 half-beamwidth sentence is exact, Giangrande's thresholds are exact, and Smith 2016's grid description is exact. I found **2 outright numeric errors, 3 imprecise/incorrectly-attributed claims, 1 framing problem, and 4 claims that are inference rather than verification**. Details below.

---

## A. WRONG — fix before implementation

### A1. Unit-test beam-height anchor (§7.7 test 2): 1.459 km is wrong → **1.461 km**
Exact evaluation of the spec's own Eq. §1.1 with a = 6371.0 km, a_e = 8494.6667 km:
- h(100 km, 0.5°) = **1.4611 km** above the feedhorn (not 1.459; an exact implementation fails this test by ~2 m). The spec's §1.1 prose ("≈ 1.46 km") is consistent with 1.4611, so this is also an internal inconsistency.
- h(230 km, 0.5°) = 5.1193 km — the "5.12" anchor is fine.

### A2. Round-trip unit-test tolerance (§7.7 test 1): "1e-9 relative" is unachievable in f64
Measured over 300k random (r ∈ [0.25, 460] km, θ ∈ [−0.5°, 20°]) in f64: max relative error in recovered r is **3.0×10⁻⁷** (worst at short range — catastrophic cancellation in the law-of-cosines inverse, exactly the small-difference-of-large-numbers issue Gao et al. flag for the forward equation), max absolute θ error **~5×10⁻⁶°**. The inverse *is* algebraically exact (verified symbolically: the asin argument reduces identically to sin θ_e, and law-of-sines on the effective sphere gives the spec's s), but the test must use realistic tolerances: e.g., |Δr| < 1 cm absolute and |Δθ| < 1×10⁻⁴°, or 1e-6 relative on r.

### A3. "VCP 21 … upper gaps up to 5.3°" (§3) is wrong → **4.9°**
From the verified Table 1 (0.5, 1.45, 2.4, 3.35, 4.3, 6.0, 9.9, 14.6, 19.5): gaps are 1.7°, 3.9°, 4.7°, **4.9°** (14.6→19.5). Nothing reaches 5.3°. (TL;DR's "~5°" is fine.)

### A4. Ducting threshold "(dN/dh ≤ −300 N/km)" (§1.1 caveats) is imprecise → onset is **−157 N/km**
Gao et al. 2006 (verified verbatim): "In an extreme condition (a sharp refractivity gradient of −300 N km⁻¹ below 100 m in height), a ray sent at a positive elevation angle may actually decrease in height with range and eventually strike the earth (Doviak and Zrnić, 1993)." −300 N/km is D&Z's *extreme example*, not the ducting criterion. Trapping/ducting onset is dN/dh < −157 N km⁻¹ (where dM/dh < 0 and the effective-radius model's k_e → ∞ at dn/dh = −1/a). Suggested wording: "trapping begins near dN/dh ≈ −157 N/km; D&Z's −300 N/km example bends rays into the ground."

### A5. Taylor approximation mis-attributed to Gao (§1.1)
The spec quotes Gao's fast path as `h ≈ r sinθ_e + r² cos²θ_e/(2a_e)`. Gao's actual Eq. (16) (verified verbatim) is **h ≈ r sinθ_e + r²/(2 k_e a)** — no cos² factor — derived from their Eq. (14)–(15) first-order expansion, with their measured max error vs. the exact Eq. (4) of only **~1.5 m at 230 km, 0.5°** (supporting the spec's "within meters"). The cos² variant is the *second*-order expansion (I re-derived it: the −r²sin²θ/(2a_e) term combines to give cos²) and appears elsewhere in the literature — it's *more* accurate than Gao's Eq. 16, so the code is fine, but the attribution/citation ("Eqs. 1, 3–4, 6, 14") should either quote Gao's exact form or note the cos² form is the standard second-order variant, not Gao's equation.

---

## B. FRAMING ISSUE — "linear-in-θ = the MRMS scheme" (TL;DR #2, §2 recommendation)
- Zhang et al. **2005** Eqs. (5)–(7) (verified verbatim) are linear VI, and the 2005 NMQ conference paper (ams.confex 81781, verified) says the then-operational NMQ used "linear interpolations between the elevation angles."
- But Zhang et al. **2011** (the operational NMQ description, quoted verbatim in the spec itself) says "**an exponential interpolation in the elevation direction**," and Smith et al. **2016** (verified) says the MRMS severe-weather 3D grid "interpolates between elevation scans using a spline whose weights are given by a power density function" — i.e., the Lakshmanan-2006 w2merger weight.
- So: linear-in-θ is *Zhang 2005's published recommendation and early NMQ practice*; the literature describes operational NMQ/MRMS as exponential/power-density in elevation. The spec's design choice (linear) remains well-supported and the spec partially acknowledges this in §2.a bullet 2 — but TL;DR #2 should be softened to "the Zhang et al. (2005) VI scheme that NMQ/MRMS gridding is built on (operationally MRMS uses an exponential elevation weight; visually near-identical)."

---

## C. UNVERIFIABLE / INFERENCE presented as fact
1. **"Interpolation is performed on the dBZ values as stored (MRMS reflectivity grids are dBZ)"** (§2.a): not explicitly stated in Zhang 2005/2011 or Smith 2016. Indirect support only (all fields, thresholds, and figures are dBZ; Warren & Protat/Lakshmanan 2012 note image processing is generally done in dBZ; MRMS GRIB2 outputs are dBZ). Keep, but cite as inference.
2. **"MRMS VI has no maximum-gap cutoff — verified"** (§3): absence-of-evidence. No cutoff appears in any cited paper and Zhang 2005 Fig. 9 (verified: "low reflectivity rings appear in these gaps") demonstrably interpolates across VCP-21's largest gaps — but the papers can't verify current operational code. Say "no gap cutoff appears in the literature; Zhang 2005 Fig. 9 shows interpolation across the largest VCP-21 gaps."
3. **SPRINT "bilinear-in-(range, elevation)"** (§2.d): both secondary sources verified say only "bilinear interpolation" (Trapp & Doswell classify Mohr & Vaughan as bilinear; James et al. 2000 verbatim: "the data are bilinearly interpolated to a Cartesian grid using NCAR's SPRINT software"). Neither specifies *which two directions*; the Mohr & Vaughan 1979 PDF is a non-OCR scan. Historical SPRINT docs describe range–azimuth interpolation within sweeps plus vertical interpolation between sweeps. Drop the "(range, elevation)" qualifier or mark it unconfirmed. (Also minor: the spec's NN-range/azimuth + linear-θ kernel is not literally "exactly this baseline.")
4. **"ZDR: linear in dB … is the convention"** (§6): no primary citation; reasonable default, but unverified — label as design choice.

Minor nuance, not an error: Gao's ~400 m beam-height error case (verified verbatim, Lake Charles at ~1.5 km, "about 17%" of beamwidth) was caused by a strong inversion **plus sharp moisture gradient**, and Gao's own conclusion was that the 4/3 model remained *sufficiently accurate* even there.

---

## D. VERIFIED EXACT (the items you asked me to attack — all check out)

**Beam-height equation & constants.** Zhang 2005 Eqs. (2)–(4) verbatim match §1.1: a_e = (4/3)a; h = (r² + a_e² + 2 r a_e sinθ_e)^½ − a_e; s = a_e asin(r cosθ_e/(a_e + h)). Gao 2006 Eq. (1) verbatim: a_e = k_e a, k_e = 1/(1 + a·dn/dh), = 4/3 for standard atmosphere; dn/dh ≈ −1/(4a) ↔ −39.2 N/km (computed; matches spec's "≈ −39 N/km"). Gao verbatim confirms the f64 warning: "double precision is usually required because the right hand side of Eq. (4) is a small difference between two large terms." Doviak & Zrnić attribution: Py-ART's `antenna_to_cartesian` documents the identical equations as **D&Z 1993, 2nd ed., p. 21, Eqs. 2.28(b) and 2.28(c)** — the spec's "Eq. 2.28" is right (pedantically: 2.28b = height, 2.28c = arc distance). Gao Eq. (6) verbatim: θ′_e = θ_e + tan⁻¹[r cosθ_e/(k_e a + r sinθ_e)]; the spec's "= θ_e + s/a_e" equality is the spec's own derivation, not in Gao — I verified it algebraically (exact in-model) and numerically. Gao Eq. (8) radial-velocity projection verbatim matches §5.

**Zhang interpolation weights & gap thresholds.** Eqs. (5)–(7) verbatim: f_a = (w1 f1 + w2 f2)/(w1+w2); **w2 = (θ_i − θ1)/(θ2 − θ1) is the upper-tilt weight, w1 = (θ2 − θ_i)/(θ2 − θ1) the lower** — exactly as the spec has them. VHI trigger verbatim: "the VI scheme plus a horizontal interpolation between adjacent tilts that are **more than 1° apart**." Eqs. (8)–(10) verbatim incl. w4 = (s3 − s_i)/(s3 − s4), w3 = (s_i − s4)/(s3 − s4). BB-conditionality supported twice: Zhang 2005 ("Objective brightband identification schemes such as the one developed by Gourley and Calvert (2003) can be used to … determine a proper objective analysis approach in real time") and operationally in the 2005 NMQ conference paper ("If no bright-band is detected, then a vertical interpolation … If a bright-band is identified, then an additional horizontal interpolation is performed"). Fig. 13 convective-core artifact claim verbatim ("the VHI approach produces unwanted artifacts near the edges of the upright convective cores"). Eq. (1) antenna pattern verbatim (8 J₂ form). Beam sizes verbatim ("approximate diameter of 0.3 km at a range of 25 km to 2.5 km at a range of 150 km", p. 35) — note 0.3 km is Zhang's own *approximate* figure (a 0.95° beam at 25 km is geometrically 0.41 km); attribution is correct. Table 1 VCP-21 list exact. Summary derivative warning verbatim. "Radar bin volume mapping (RBVM)" naming and the 50 m × 50 m × 10 m fine-grid rendering confirmed.

**MRMS above/below tilts.** Zhang 2011 quote **character-for-character exact**: "The analysis scheme includes a nearest-neighbor mapping on the range–azimuth plane and an exponential interpolation in the elevation direction (Zhang et al. 2005; Lakshmanan et al. 2006). No extrapolation was applied at the top and bottom of the radar volume scan beyond half a beam width." Zhang 2005 no-extrapolation sentence exact (full version: "…discussed in section 4 do not extrapolate reflectivity values into the data void regions below the beam coverage shown in Fig. 2"). Cone-of-silence fill via other radars / VPR confirmed in both papers. The spec's half-beamwidth envelope reading (fill within ±bw/2 of edge tilts, blank beyond) is the natural reading; unit test #4 arithmetic is self-consistent.

**Lakshmanan 2006.** Eq. (6) verbatim: δ_e = exp[α³ ln(0.005)], α = (e − θ_i)/(|θ_{i±1} − θ_i| ∨ b_i), ∨ = max — exactly the spec's formula. Paper's gloss: 1 at beam center, 0.5 at half-beamwidth, "below 0.01 at the beamwidth (beyond which the influence of the range gate can be disregarded)" — spec's gap-denominated reading is consistent (denominator is max(gap, beamwidth)). "Neither vertical nor horizontal, but tangential to the direction of the beam" — verbatim. "Could fail in the presence of strong vertical gradients such as bright bands, stratiform rain, or convective anvils" — verbatim. Velocity: verified they do **not** objectively analyze velocity as a scalar — three options verbatim: inverse VAD, approximate multi-Doppler, or "forgoing the wind field retrieval altogether, but merging shear, a scalar field derived from the velocity data"; "their collaborative technique is not an objective analysis one."

**Askelson 2000.** Eq. (3) verbatim: w = exp(−r²ik/κr − φ²ik/κφ − θ²ik/κθ) with angular azimuth/elevation separations; spacing-anisotropy motivation verbatim.

**Trapp & Doswell 2000.** All five numbered conclusions verified near-verbatim, including both spec quotes ("may be misinterpreted as some physical change in the analyzed field"; "Bilinear and 'nearest-neighbor' interpolation schemes generate wavelengths not present in the initial data"), Cressman negative sidelobes, and the recommendation (isotropic Barnes, smoothing parameter from "the maximum datapoint spacing affecting the analysis domain"). Zhang 2005's pushback also verified ("significantly less high-resolution information"). Mohr & Vaughan honest-warning quote verbatim via T&D.

**Grids.** Smith 2016 verbatim: 0.01° × 0.01°, "33 vertical levels from 0 to 20 km MSL", "250 m from 0 to 3 km MSL, 500 m from 3 to 9 km MSL, and 1000 m from 9 to 20 km MSL"; MRMS blending via Lakshmanan et al. 2006b confirmed. Zhang 2011: 31 levels, 500 m–18 km MSL, 0.01° verbatim. NMQ 2005 conference: 21 levels, 1–17 km MSL, 500 m below 5 km / 1000 m above — verbatim.

**Dual-pol.** Giangrande 2008 all verbatim: ρhv 0.90–0.97; Z max 30–47 dBZ and ZDR max 0.8–2.5 dB in a 500-m window *above* the ρhv gate; elevations 4°–10° ("less than 4°, ML signatures are smeared due to beam broadening"); **6-km climatological cap confirmed** ("not identified above 6 km (adaptable threshold)"); running 21° (±10°) azimuth sector; 80%/20% top/bottom. Warren & Protat 2019 abstract+conclusion verbatim: Z more accurate on average, especially high-Z/strong-gradient convective cores; dBZ better for low, monotone regions (crossover ~30 dBZ for monotone triads); recommendation quote exact ("…except when identifying echo-top height or other low-reflectivity boundaries").

**Beamwidths.** NWS training site: 0.95° half-power confirmed. Wood & Brown 1997 effective beamwidths **1.29° (1.0° sampling) and 1.02° (0.5° sampling) confirmed** via NSSL (Wood's own page) — I initially suspected 1.39° (that figure belongs to Brown et al. 2002's 0.89°-beam simulations); the spec's numbers and attribution are correct. James et al. 2000 (WAF 15, 327–338) SPRINT quote verbatim.

**Math spot-checks (computed):** a_e = 8494.6667 km ✓; beam diameter r·bw: 0.41/2.49/3.81 km at 25/150/230 km (spec's 25-km "0.3 km" correctly attributed to Zhang, but geometrically it's 0.41 km for 0.95° — consider a footnote); cos 20° = 0.9397 ✓ ("≥0.94"); inverse formulas round-trip exactly up to f64 cancellation (see A2).

---

## E. Suggested edits (minimal diff)
1. §7.7 test 2: 1.459 → **1.4611 km** (100 km); keep 5.12 km (exact 5.1193).
2. §7.7 test 1: replace "1e-9 relative" with "|Δr| < 1 cm, |Δθ| < 1e-4°" (or 1e-6 relative on r) and note short-range cancellation.
3. §3: "up to 5.3°" → "up to 4.9°".
4. §1.1: ducting parenthetical → "trapping onset dN/dh < −157 N/km; D&Z's −300 N/km example drives rays into the ground"; credit moisture gradients alongside inversions for the 400-m case.
5. §1.1: fast path → either Gao's actual Eq. (16) `h ≈ r sinθ_e + r²/(2a_e)` (max err ~1.5 m at 230 km per Gao) or keep the cos² form but cite it as the standard second-order expansion, not Gao's equation.
6. TL;DR #2 / §2 recommendation: soften "the MRMS/NMQ scheme is linear" → "Zhang et al. (2005) VI, the basis of NMQ/MRMS gridding; operational MRMS uses an exponential/power-density elevation weight (Zhang et al. 2011; Smith et al. 2016) that is visually near-identical".
7. §2.a/§3/§6: downgrade "verified" → "consistent with the literature (no contrary statement found)" for: dBZ-units interpolation, no-gap-cutoff, ZDR-in-dB convention; drop SPRINT's "(range, elevation)" qualifier.

## Sources (all fetched and text-verified this session)
- Zhang, Howard & Gourley 2005, JTECH 22, 30–42, [doi:10.1175/JTECH-1689.1](https://doi.org/10.1175/JTECH-1689.1)
- Zhang et al. 2011, BAMS 92, 1321–1338, [doi:10.1175/2011BAMS-D-11-00047.1](https://doi.org/10.1175/2011BAMS-D-11-00047.1)
- Lakshmanan et al. 2006, WAF 21, 802–823, [doi:10.1175/WAF942.1](https://doi.org/10.1175/WAF942.1)
- Smith et al. 2016, BAMS 97, 1617–1630, [doi:10.1175/BAMS-D-14-00173.1](https://doi.org/10.1175/BAMS-D-14-00173.1)
- Askelson, Aubagnac & Straka 2000, MWR 128, 3050–3082
- Trapp & Doswell 2000, JTECH 17, 105–120
- Giangrande, Krause & Ryzhkov 2008, JAMC 47, 1354–1364, [doi:10.1175/2007JAMC1634.1](https://doi.org/10.1175/2007JAMC1634.1)
- Warren & Protat 2019, JTECH 36, 1143–1156, [doi:10.1175/JTECH-D-18-0183.1](https://doi.org/10.1175/JTECH-D-18-0183.1) ([Monash OA PDF](https://researchmgt.monash.edu/ws/files/291896794/278666657_oa.pdf))
- Gao, Brewster & Xue 2006, Adv. Atmos. Sci. 23, 190–198, [doi:10.1007/s00376-006-0190-3](https://doi.org/10.1007/s00376-006-0190-3) ([IAP full text](https://www.iapjournals.ac.cn/aas/en/article/doi/10.1007/s00376-006-0190-3); [CAPS draft](https://twister.caps.ou.edu/papers/GaoEtal_JTech2005RayPath.pdf))
- Zhang et al. 2005 conference companion, [AMS 81781](https://ams.confex.com/ams/pdfpapers/81781.pdf)
- James et al. 2000, WAF 15, 327–338 ([UW PDF](https://atmos.uw.edu/MG/PDFs/WF00_jame_radar.pdf))
- [Py-ART antenna_to_cartesian docs](https://arm-doe.github.io/pyart/API/generated/pyart.core.antenna_to_cartesian.html) (D&Z 1993 2nd ed., p. 21, Eqs. 2.28b/2.28c)
- [NSSL Wood & Brown conference page](https://www.nssl.noaa.gov/users/wood/public_html/PUBL/CONF/SLS_19/conf_paper.html) (effective beamwidths 1.29°/1.02°)
- [NWS NEXRAD training, RADAR Basics](https://training.weather.gov/nwstc/NEXRAD/RADAR/Section1-2.html) (0.95° beamwidth)

## VERIFICATION: VIZ
# Adversarial Verification Report — "Volumetric 3D Radar Visualization for BowEcho" Spec

Verification method: direct fetch of all GRLevelX pages (curl, browser UA — site 403s default fetchers), full-text extraction of the Iowa State GR2AE manual PDF and the Dyken et al. 2008 draft PDF, Crossref + Semantic Scholar API metadata for every DOI, GitHub API/`gh` for repo claims, and targeted web searches. Verdict summary: **the GR2Analyst sourcing is excellent (every verbatim quote checked out), but the spec contains one materially false competitive claim, one false performance claim, one wrong page citation, one wrong comparison to MRMS, and one graphics-literature misattribution.** Details below.

---

## A. WRONG — must fix before this spec ships

### A1. "No open-source desktop NEXRAD viewer currently ships a GR2A-class volume display" — FALSE
**OpenStorm** ([github.com/JordanSchlick/OpenStorm](https://github.com/JordanSchlick/OpenStorm), GPL-2.0, created 2022, releases since ≥2023 (latest tagged 1.4.0), actively pushed through May 2026, ~133 stars) is exactly that: *"a free and open source 3d radar viewer"* using Unreal Engine 5 with *"a custom volumetric ray marching shader, entire radar volumes can be displayed"* — full 3D NEXRAD Level 2, base + derived products (**de-aliased velocity, SRV, rotation**), interpolation in space **and time**, real-time polling, VR support, Windows + Linux. This is arguably *beyond* GR2A-class (whole-volume DVR, not a storm-box).

The supporting sub-claim is also wrong as stated: a supercell-wx issue search does NOT come back empty. **dpaulat/supercell-wx issue #164 "OpenStorm Integration"** (open, enhancement, active as of 2026-05-31) requests bounding-box hand-off to OpenStorm for 3D visualization, and #403 requests vertical cross-sections.

**Corrected competitive framing:** supercell-wx itself ships no 3D volume display and its community is asking for one (issue #164 validates demand). OpenStorm ships whole-volume DVR but as a heavyweight standalone UE5 app with no integrated 2D-analysis workflow, no GR2A-style alpha-table/isosurface workflow benchmark, and no storm-box + furniture (height walls, freezing-level lines) analyst affordances. BowEcho's differentiation is "GR2A's documented analyst workflow, natively integrated in a lightweight Rust viewer" — not "first open-source 3D NEXRAD volume rendering," which OpenStorm already claimed.

### A2. "GPU marching cubes hit >100 fps on 256³ volumes in 2008" — FALSE as stated
Verified against the Dyken et al. performance table (Table 1, hpmarcher draft, matching CGF 27(8), 2028–2039): on a GeForce 8800GTX, the fastest variant (GLHP-VS) achieved **33 fps (Bunny), 34 fps (Bonsai), 55 fps (Aneurism), 68 fps (Cayley) on 255³ volumes** — i.e., 33–68 fps at 256³ class. **>100 fps was reached one size down, at 127³ (122–261 fps)**, and at 63³ (378–695 fps). Throughput peaked "over 1000 million MC cells processed per second" only on the sparsest dataset.

This correction *helps* the spec's argument: the v1 grid (96×96×64 ≈ 0.57M MC cells) is ~29× smaller than 255³ (16.6M cells), so 2008 hardware was already far past 60 fps at the spec's working size. State it that way instead. Also add the issue number to the citation: CGF **27(8)**, 2028–2039.

### A3. Lakshmanan et al. 2013 page numbers — WRONG
Crossref-verified: *An Improved Method for Estimating Radar Echo-Top Height*, **WAF 28(2), 481–488**, DOI 10.1175/WAF-D-12-00084.1. The spec's "459–465" is incorrect. (Method characterization — interpolate between bracketing tilts toward an improved echo top — is consistent with the paper as described in secondary sources; the abstract is publisher-elided so the exact formula wording was not independently re-verified.)

### A4. "Δz = 0.28 km — finer than MRMS everywhere" — FALSE in the vertical below 3 km
MRMS vertical spacing (corroborated against the Smith et al. 2016 BAMS description and NOAA docs): **250 m up to 3 km MSL**, 500 m from 3–9 km, 1000 m above — i.e., MRMS is *finer* than the proposed 281 m below 3 km, which is exactly the layer that matters for base-scan/low-level structure. Correct to: "finer than MRMS horizontally everywhere (0.625 vs ~1 km) and vertically above 3 km; comparable below 3 km (281 vs 250 m)." Note also the spec's "0–20 km MSL" matches the published BAMS-level description, but the operational level list runs 0.5–19 km MSL (33 levels) — the spec's own confidence note already gestures at this; make it exact.

### A5. "the pre-3D-texture DVR technique of Cabral et al. 1994" — MISATTRIBUTED
Cabral, Cam & Foran 1994 (Proc. IEEE/ACM Symp. Volume Visualization, 91–98, DOI 10.1145/197938.197972) is the seminal *texture-mapped DVR* paper, but it used **3D texture hardware** (SGI), not the pre-3D-texture technique. The **three axis-aligned 2D-texture slice stacks** method — the one whose "per viewing axis" memory fingerprint the spec is matching — is canonically **Rezk-Salama, Engel, Bauer, Greiner & Ertl 2000**, *Interactive volume rendering on standard PC graphics hardware using multi-textures and multi-stage rasterization*, SIGGRAPH/Eurographics Workshop on Graphics Hardware, 109–118, DOI 10.1145/346876.348238. Cite both correctly (Cabral for texture-slicing DVR generally; Rezk-Salama for the 2D-stack consumer-GPU variant GR2A plausibly used in 2005).

---

## B. OVERSTATED / NOT FROM THE CITED SOURCE — downgrade or reword

### B1. "the memory math matches" (12 MB per viewing axis ⇒ three 2D slice stacks)
The manual quote is verbatim-verified: *"at 128x128x64 each viewing axis uses 12MB of system and video memory."* But the math does **not** cleanly match plain RGBA8 slice stacks: 128×128×64 = 1,048,576 voxels × 4 B = **4 MB per stack**, not 12. Getting to 12 MB/axis requires assuming ~12 B/voxel/axis (e.g., color+gradient/normal textures for the lit mode plus a system-RAM copy: 3 × 4 MB) — plausible, but assumption-laden. The *directional* inference (per-axis storage phrasing ⇒ axis-aligned stacks) remains reasonable. Downgrade "the memory math matches" to "a per-axis cost is consistent with axis-aligned stacks under plausible texture layouts (≈12 B/voxel/axis)"; confidence medium-low. As the spec notes, nothing downstream depends on it.

### B2. Warren & Protat "interpolate in Z **for values ≥ ~30 dBZ**" — the threshold is not in the paper
Abstract verified verbatim: interpolation in Z *"is more accurate on average, especially in regions of high reflectivity and strong reflectivity gradient (i.e., convective cores)"*; dBZ is better *"in regions of low and monotonically increasing/decreasing reflectivity"*; recommendation: *"reflectivities be converted from dBZ to Z prior to interpolation **except when identifying echo-top height or other low-reflectivity boundaries**."* A full-text search of the open-access PDF finds no ~30 dBZ switch-over. The spec's value-conditional domain is an invention — and a mildly bad one (a per-value domain switch introduces a small derivative discontinuity at the threshold and complicates the inner loop). **Correction for §6.1:** interpolate the whole volume in linear Z (paper-faithful, simpler); use dBZ-domain logic only in the future echo-top 2.5D product, which is precisely the paper's carve-out.

### B3. Usher WebGL blog cited for "far beyond 60 fps on integrated graphics" at 128³/1080p
The post exists and demonstrates WebGL2 3D-texture raymarching ("an elegant and fast volume renderer... entirely in the browser") but **publishes no fps numbers, no 1080p/128³ benchmark, and no integrated-graphics claim**. Keep the engineering estimate (it is almost certainly true) but present it as an estimate, or cite the demo as existence-proof only.

### B4. "Cylindrical-grid DVR for radar also published" (IEEE 'Cylindrical Volume Rendering on Radar Information in Complicated Environment')
The paper exists (IEEE Xplore 5365752) but it renders **radar electromagnetic coverage/beam volumes from a propagation model (battlefield situational awareness)** — not weather-echo volumes. Either drop it or relabel as generic non-Cartesian-grid DVR precedent.

### B5. Zhang et al. 2016 BAMS-D-14-00174.1 labeled "system overview"
Crossref-verified: that DOI is *MRMS **Quantitative Precipitation Estimation**: Initial Operating Capabilities*, BAMS 97(4), 621–638. It does contain system/3D-grid description, but label it as the QPE IOC companion paper.

### B6. Numeric nit: "~578k cells"
96×96×64 has 95×95×63 = **568,575 (~569k)** MC cells; the 589,824 (~590k) voxel count elsewhere is right. Trivial, but it's a load-bearing budget number — fix it.

---

## C. VERIFIED — claims that survived adversarial checking (spot-check highlights)

**GRLevelX primary sources (all fetched live; quotes match verbatim):**
- *"GR2Analyst uses the latest hardware-based direct volume rendering techniques to produce high quality radar volume displays in real time."* — exact.
- Volume Settings: *"the number of samples along the x/y and z axes. The default values are 92 and 64"* and the 128×128×64 / 12 MB warning — exact (Volume Toolbar manual page). The 92×92×64 reading is sound.
- Default reflectivity alpha table "a simple ramp from zero to 100%" — exact.
- Tornado table: <50 dBZ transparent, 100% above 50 (*"Basically, we have created an isosurface at 50 dbz"*), 25% halo drawn at **43–44 dBZ**, isovalue *"around 52 dbz"*, *"viewing from the east and looking down at a 60° angle"* — all exact.
- Hail table: 60+ opaque with 50 dBZ halo; *"look for 60 dbz regions above the freezing level"*; extreme events 70+ opaque with purple 60 halo (May 14 2003 KTLX example); *"it takes two or three volume scans (10-15 minutes) for a hail core to reach the ground"* — all exact.
- DRC sequence: *"A large overhang of 50+ dbz develops, a 'finger' or descending reflectivity core (DRC) comes out of it, a debris blob appears at the ground level, the DRC connects to the debris blob to form a complete tube."* — exact; SRV-isosurface tornado-shear read shown for Ft Worth 2000 / Paducah 2003 — confirmed.
- Advection: *"GR2Analyst automatically corrects for this when sampling the radar volume by using the storm motion vector to slide the upper tilts backwards along the direction of motion"*; *"four or five minutes to scan"*; example storm motion **"from Azimuth of 236° and a Speed of 25 kts"** — all exact.
- Volume Mouse drag-box → sample → Volume Explorer pops up — exact. *"You can save up to 16 custom alpha tables"* — exact. VCR buffer (oldest/prev/next/latest) + Refresh — exact. Volume Light (world/eye space, azimuth+tilt sliders, Type slider Diffuse=max shadows → Ambient=none) — exact. Lit Volume / Isosurface as the two modes, isovalue-pointer drag — exact (Volume Alpha + Volume Display pages).
- **Acceptance-test premise verified:** *"We load the **KTLX19990503_235123.Z** file"* ... *"It's included in the installation directory"* — the 23:51 UTC Moore file and its bundling are both confirmed.
- Iowa State GR2AE manual (PDF extracted, pp. 19–21): two height walls *"with a bar every 10,000 feet"*, *"yellow line is the 0°C height; the red line is the −20°C height"*, *"cardinal direction markers"*, click-drag rotate + scroll-wheel zoom, products = base reflectivity, base velocity, SRV, spectrum width, **and rotation** — all confirmed.
- GRLevelX suite went on the market **March 2005**, and the v1 GR2Analyst pages already document the Volume Explorer with 2005 case data — "real time on 2005-era GPUs" stands.

**Papers/metadata (Crossref/Semantic Scholar):** Zhang et al. 2005 (JTECH 22(1), 30–42) ✓; Trapp & Doswell 2000 (JTECH 17(2), 105–120) ✓; Warren & Protat 2019 (JTECH 36(6), 1143–1156) ✓ incl. conclusion direction; Smith et al. 2016 (BAMS 97(9), 1617–1630; 0.01°, 3500×7000, 33 levels, 2-min) ✓; Lorensen & Cline 1987 (CG 21(4), 163–169) ✓; Snyder et al. 2015 (WAF 30(6), 1819–1844; ZDR ≥1 dB above 0°C) ✓; Liang et al. 2014 (C&G 68, 81–91) ✓ **including the 2014 C&G Best Paper Award** (IAMG recipients list); Lakshmanan 2000 BWER GA (JAM 39(2), 222–230) ✓; Pal et al. 2006 (JAMC 45(9), 1304–1312, DOI 10.1175/JAM2408.1) ✓; Helmus & Collis 2016 (JORS 4, article 25) ✓; Amanatides & Woo 1987 ✓; Ernvik 2002 Linköping thesis (slices + surface extraction + volume rendering trio) ✓; svr-algorithm repo (spherical-voxel ray casting, explicitly Amanatides-Woo-based, yt-project extension) ✓; US Patent 6,723,050 ("Volume rendered three dimensional ultrasonic images with polar coordinates", rendering directly on polar data without scan conversion) ✓; IDV **8-point weighted interpolation** radial→3D-Cartesian (confirmed via Unidata IDV docs/release notes; note the spec's specific 2_7u2 workshop URL 404s — swap to docs.unidata.ucar.edu) ✓; COMET BWER (*"The higher the average dBZ echo outlining the BWER, the stronger the associated updraft"*) ✓; VCP 12/212 = 14 elevations, 0.5°–19.5°, ~4.2 min (NOAA/ROC; "4–6 min" is fine with SAILS) ✓.

**Environment claim:** rusty-weather Cargo.lock (local clone) pins egui/eframe/egui_glow/egui-wgpu **0.34.3** — paint-callback availability on both backends confirmed.

**Math audit (all recomputed):** 2.5° gap at 100 km ≈ 4.37 km ✓; 25 kt × 4–5 min ≈ 3.1–3.5 km ("~3 km" ✓); 0.5° azimuth at 150 km ≈ 1.31 km ✓; 96×96×64 f32 = 2.25 MiB ✓; 128³ = 8 MiB ✓; 12-frame ring = 27 MiB ✓; 460×460×36 = 7.62M voxels ≈ 30 MB ✓; Δxy 60/96 = 0.625 km ✓; Δz 18/64 = 0.281 km ✓; 150k tris × 3 × 24 B ≈ 10.8 MB ("~10 MB" ✓); 24 B/vertex ✓. The 2–6 ms resample and 1–4 ms MC figures are uncited engineering estimates — plausible (≈50–100 ns/voxel single-thread for 2 bilinear + lerp, /8 cores), but mark them as estimates pending the first benchmark; per-column precomputation of (s, az, el(z)) is what makes them achievable.

## D. Unverifiable (correctly disclosed in the spec; leave flagged)
- AllisonHouse GR2AE article: still 403 (re-confirmed). The "rotation" product claim it backed is now independently verified via the Iowa State GR2AE manual, so the AllisonHouse cite can be dropped entirely.
- GR2A Isosurface mode as first-hit DVR rather than mesh extraction: remains an inference; nothing in the fetched manual pages mentions geometry extraction, consistent with the spec's framing.
- Zhang et al. 2005 artifact-family details: canonical and consistent with the literature, but the full text was not independently re-read (AMS blocks fetchers); confidence unchanged.

## E. Net impact on the v1 design
No architectural change required — storm-box resample → marching cubes → egui paint callback survives review, and the corrected Dyken numbers make the perf budget *more* comfortable at v1 scale, not less. Required edits: (1) rewrite the §2 competitive note around OpenStorm + supercell-wx #164; (2) fix the Dyken fps claim and Lakshmanan 2013 pages; (3) simplify §6.1 to interpolate everything in linear Z (drop the ≥30 dBZ conditional, cite Warren & Protat's actual recommendation, keep dBZ only for future echo-top products); (4) fix the MRMS vertical-spacing comparison; (5) swap Cabral→Rezk-Salama for the 2D-stack technique and soften "memory math matches"; (6) 578k→569k cells; (7) replace the dead IDV workshop URL and drop the AllisonHouse cite.