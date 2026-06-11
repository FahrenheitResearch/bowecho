# Hail + Wind Algorithm Specs (verified)
## HAIL
# Hail Detection + Sizing for radar-rs-analyst — Verified Implementation Spec

**Status of verification:** every constant below was checked against the primary literature (full text of Murillo & Homeyer 2019 via PMC, the 2021 AMS corrigendum, NOAA/WDTD official WSR-88D product documentation, Forcadell et al. 2024 restatement) and cross-checked against the reference implementation `pyhail` (github.com/joshua-wx/pyhail, `src/pyhail/mesh_grid.py`, `mesh_formulas.py`, `hsda.py`, `hsda_mf.py`). One **critical trap** was found and resolved (§2).

**Existing code this builds on:** `C:\Users\drew\radar-work\radar-rs-analyst\crates\render2d\src\volumetric.rs` — `mehs_grid()` (line 351) already implements Witt-MESH correctly on the shared `CutColumn`/`column_profile` ground-location column walk; `vil_grid()` (295), `echo_top_grid()` (254), 4/3-Earth beam height in `radar_core`. The work is: refactor `mehs_grid` into a hail-products walk that emits **SHI + MESH (3 calibrations) + POSH**, add **POH** (one-liner on existing echo tops), wire **H0/H−20 from HRRR soundings**, and (phase 2) HSDA.

---

## 1. WSR-88D Hail Detection Algorithm — Witt et al. (1998), verified constants

Citation: Witt, A., M. D. Eilts, G. J. Stumpf, J. T. Johnson, E. D. Mitchell, and K. W. Thomas, 1998: *An Enhanced Hail Detection Algorithm for the WSR-88D.* **Wea. Forecasting**, 13, 286–303, doi:10.1175/1520-0434(1998)013<0286:AEHDAF>2.0.CO;2.

### 1.1 Hail kinetic-energy flux (Ė, J m⁻² s⁻¹)

```
Ė(Z) = 5×10⁻⁶ · 10^(0.084·Z) · W(Z)
```
Z in dBZ. Verified in: M&H19 Eq. (4), pyhail (`hke = (5*10**-6) * 10**(0.084*dbz) * dbz_weights`), Forcadell et al. 2024.

### 1.2 Reflectivity weighting W(Z) — the rain/hail transition ramp

```
W(Z) = 0                      Z ≤ Z_L
     = (Z − Z_L)/(Z_U − Z_L)  Z_L < Z < Z_U
     = 1                      Z ≥ Z_U
Z_L = 40 dBZ,  Z_U = 50 dBZ
```
Verified: M&H19 Eq. (2), NOAA WDTD SHI page ("threshold values of 40 dBZ (lower) and 50 dBZ (upper)"), pyhail.

### 1.3 Temperature-based height weighting W_T(H)

```
W_T(H) = 0                          H ≤ H0
       = (H − H0)/(H_m20 − H0)      H0 < H < H_m20
       = 1                          H ≥ H_m20
```
H = beam-center height **above radar level (ARL)**; H0 = height of the 0 °C environmental level (melting level); H_m20 = height of the −20 °C level. Verified: M&H19 Eq. (3), pyhail, WDTD ("hail growth only occurs at temperatures < 0 °C, and most growth for severe hail occurs near −20 °C or colder").

### 1.4 Severe Hail Index (SHI, J m⁻¹ s⁻¹)

```
SHI = 0.1 · ∫_{H0}^{H_T}  W_T(H) · Ė(Z(H)) dH        (H in meters)
```
**Upper limit is storm top H_T, not H_m20.** (W_T = 1 above H_m20, so reflectivity all the way to storm top contributes at full weight. An earlier auto-extraction of M&H19 misread the limit as H_m20; the pyhail implementation sums the entire column, and the original Witt Eq. 5 integrates H0→H_T. Below H0 the weight is 0, so in practice you integrate the whole observed column and let W_T do the clipping.) The 0.1 is a fixed scaling constant from Witt. Verified: pyhail (`shi = 0.1 * np.sum(w_t * hke) * d_z`, heights in meters), Forcadell et al. 2024 (`SHI = 0.1 ∫_{H0}^{Ht}`).

### 1.5 POSH — Probability of Severe Hail (%)

```
WT_thresh = 57.5 · H0_km − 121          (H0_km = melting level in km ARL; result in J m⁻¹ s⁻¹)
WT_thresh = max(WT_thresh, 20)          (floor: if WT < 20, set WT = 20 — Witt 1998 / WDTD POSH page)
POSH = 29 · ln(SHI / WT_thresh) + 50
clamp to [0, 100]; operationally rounded to the nearest 10% for display
```
Property to unit-test: SHI = WT_thresh ⟹ POSH = 50% exactly. Verified: WDTD POSH page (floor + rounding + clamp), pyhail (`57.5*(meltlayer/1000) - 121`, `29*ln(shi/wt)+50`, clip 0–100), Forcadell 2024.

### 1.6 MESH — Maximum Estimated/Expected Size of Hail (mm)

```
MESH_witt = 2.54 · SHI^0.5      (mm; SHI in J m⁻¹ s⁻¹)
```
Empirically fit so that ~75% of observed hail sizes fall below MESH, using **147 hail reports** (count verified from M&H19's description of the original fit). Verified: M&H19 Eq. (6), pyhail, WDTD.

### 1.7 POH — Probability of Hail (any size) — cheap bonus product

Witt 1998's POH is the Waldvogel, Federer & Grimm (1979, *J. Appl. Meteor.*, 18, 1521–1525) hailpad-validated curve on ΔH₄₅ = (height of the 45 dBZ echo top) − H0:

| ΔH₄₅ (km) | 1.65 | 1.80 | 1.97 | 2.17 | 2.40 | 2.70 | 3.07 | 3.55 | 4.20 | 5.00 | 5.80 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| POH (%) | 0 | 10 | 20 | 30 | 40 | 50 | 60 | 70 | 80 | 90 | 100 |

(Table transcribed from an AMS conference restatement of Waldvogel's Table 1; linear-interpolate between rows.) Implementation: `echo_top_grid(volume, 45.0)` already exists — POH is `interp_table(et45_m_arl − h0_arl)`. One screen of code.

---

## 2. Murillo & Homeyer (2019) MESH recalibration — ⚠ CORRIGENDUM TRAP

Citation: Murillo, E. M., and C. R. Homeyer, 2019: *Severe Hail Fall and Hailstorm Detection Using Remote Sensing Observations.* **J. Appl. Meteor. Climatol.**, 58, 947–970, doi:10.1175/JAMC-D-18-0247.1. **Plus:** Corrigendum, **J. Appl. Meteor. Climatol.**, 60(3), 2021, doi:10.1175/JAMC-D-20-0271.1.

They refit MESH = a·SHI^b to the 75th and 95th percentiles of ~5,954 hail reports (vs Witt's 147). **The coefficients printed in the 2019 paper text (Eqs. 15–16) are WRONG.** The error was caught by Nathan Wendt (NOAA/SPC) and fixed in a published 2021 corrigendum; all analysis in the 2019 paper used the correct values. Verified against the corrigendum and pyhail (which matches the corrected values):

| Fit | 2019 paper as printed (DO NOT USE) | **Corrected (corrigendum 2021) — USE THESE** |
|---|---|---|
| MESH₇₅ | 16.566 · SHI^0.181 | **15.096 · SHI^0.206** |
| MESH₉₅ | 17.270 · SHI^0.272 | **22.157 · SHI^0.212** |

(mm; SHI in J m⁻¹ s⁻¹.) Sanity anchor: Witt and MESH₇₅ cross at SHI ≈ 429 J m⁻¹ s⁻¹ where both ≈ 52.6 mm (pyhail uses this as its blend pivot).

**Decision thresholds** (from the GridRad 23-yr climatology follow-up, Murillo et al. 2021, PMC8050942): peak skill for *severe* (≥1 in.) at MESH₇₅ ≥ 40 mm or MESH₉₅ ≥ 64 mm; for *significant* (≥2 in.) at MESH₇₅ ≥ 47 mm or MESH₉₅ ≥ 83 mm. For Witt-MESH the widely used climatology threshold is ≈29 mm for severe (Cintineo et al. 2012, *Wea. Forecasting*, 27, 1235–1248).

**Recommendation:** implement all three as an enum. Default display = **MESH₉₅** (M&H19 found it best bounds large hail; Witt-MESH systematically underestimates big hail). Keep **Witt1998** selectable for apples-to-apples comparison with MRMS/NWS (operational MRMS MESH still uses 2.54·√SHI — Smith et al. 2016, *BAMS*, 97, 1617–1630, doi:10.1175/BAMS-D-14-00173.1).

---

## 3. The column integral over per-tilt grids (Rust spec)

### 3.1 API (refactor of existing `mehs_grid` in `crates/render2d/src/volumetric.rs:351`)

```rust
/// Environmental heights ABOVE RADAR LEVEL (meters). Source: HRRR profile (§4).
pub struct HailEnv {
    pub h0_arl_m: f32,    // 0C (melting) level
    pub hm20_arl_m: f32,  // -20C level
}

pub enum MeshCalibration {
    /// Witt et al. 1998 (WAF 13, 286-303): MESH = 2.54*SHI^0.5. MRMS-compatible.
    Witt1998,
    /// Murillo & Homeyer 2019 (JAMC 58, 947-970) 75th-pct fit,
    /// COEFFICIENTS FROM THE 2021 CORRIGENDUM (doi:10.1175/JAMC-D-20-0271.1):
    /// MESH = 15.096*SHI^0.206  (paper text printed 16.566/0.181 -- wrong).
    MurilloHomeyer2019P75,
    /// Same papers, 95th-pct fit: MESH = 22.157*SHI^0.212 (corrigendum values).
    MurilloHomeyer2019P95,
}

pub struct HailGrids {
    pub shi: MomentGrid,       // J m^-1 s^-1
    pub mesh_mm: MomentGrid,   // mm
    pub posh_pct: MomentGrid,  // 0..100 (continuous; round to 10s at display time)
}

pub fn hail_grids(volume: &RadarVolume, env: HailEnv, cal: MeshCalibration) -> Option<HailGrids>;
```

One column walk produces all three (SHI is the expensive part; MESH/POSH are pointwise transforms). Keep `mehs_grid` as a thin wrapper for compatibility or delete it.

### 3.2 Per-column algorithm (replaces the body of the existing loop)

The existing structure is already right: resample every elevation cut onto the base tilt's (azimuth, ground-range) grid, build `prof: Vec<(height_m_arl, dbz)>` sorted by height via `column_profile`, then:

```rust
let h0   = env.h0_arl_m.max(0.0) as f64;
let hm20 = (env.hm20_arl_m as f64).max(h0 + 1.0);

let ke_flux = |dbz: f64| -> f64 {
    let w = ((dbz - 40.0) / 10.0).clamp(0.0, 1.0);   // W(Z), Witt eq. ramp 40-50 dBZ
    if w <= 0.0 { 0.0 } else { 5.0e-6 * 10f64.powf(0.084 * dbz) * w }
};
let wt = |h: f64| ((h - h0) / (hm20 - h0)).clamp(0.0, 1.0); // W_T(H)

let mut shi = 0.0f64;
for seg in prof.windows(2) {
    let (ha, za) = seg[0]; let (hb, zb) = seg[1];
    if hb <= h0 || hb <= ha { continue; }
    // Clip segment bottom at H0 (W_T = 0 below; clipping beats midpoint
    // weighting when tilt spacing is km-scale near the melting level).
    let lo = ha.max(h0);
    let t  = if hb > ha { (lo - ha) / (hb - ha) } else { 0.0 };
    let z_lo = za + t * (zb - za);              // linear in dBZ, like the discrete HDA
    let e_lo = ke_flux(z_lo); let e_hi = ke_flux(zb as f64);
    let w_mid = wt(0.5 * (lo + hb));
    shi += w_mid * 0.5 * (e_lo + e_hi) * (hb - lo);   // trapezoid, dH in meters
}
shi *= 0.1;                                            // Witt's fixed scaling

if shi > 1.0 {                                         // suppress noise floor
    mesh = match cal {
        Witt1998              => 2.54   * shi.powf(0.5),
        MurilloHomeyer2019P75 => 15.096 * shi.powf(0.206),
        MurilloHomeyer2019P95 => 22.157 * shi.powf(0.212),
    };
    let wt_thresh = (57.5 * (h0 / 1000.0) - 121.0).max(20.0);
    posh = (29.0 * (shi / wt_thresh).ln() + 50.0).clamp(0.0, 100.0);
}
```

Notes vs the current `mehs_grid`:
- Current code is numerically fine (midpoint W_T, skip-below-H0); the clip-at-H0 refinement above is exact for the piecewise-linear W_T and costs nothing. Optionally also split segments at H_m20 — second-order, skip unless you want bit-level agreement with a reference.
- **No surface-layer term** (unlike VIL): W_T = 0 below H0, so the lowest-beam-extends-to-ground convention is irrelevant for SHI.
- Treat missing/below-threshold gates as Ė = 0, not NaN-poison. A profile that never exceeds 40 dBZ yields SHI = 0 → leave grid cell NaN.
- Sanity-cap dBZ at e.g. 80 before `ke_flux` (pyhail clips at ±100; Ė is exponential in Z, so a corrupt 120 dBZ gate would dominate the whole grid).
- This is the **grid/column-based** formulation (as in MRMS and GridRad), not Witt's original per-cell formulation that ingested SCIT cell-centroid max-Z profiles. Grid-based is what every modern system runs (Smith et al. 2016 BAMS; M&H19 used GridRad columns at 0.5-km vertical resolution below 7 km MSL). No cell tracker needed.

### 3.3 Edge cases

1. **Cone of silence (near range).** Highest VCP tilt is 19.5°. Within ground range ≈ `storm_top / tan(19.5°)` (~30–35 km for a 11–12 km top) the upper column is truncated → SHI/MESH is a **lower bound**. Emit a quality flag where the top observed beam height < `hm20 + 1 km` (the full-weight region is unsampled); render with hatching or reduced alpha rather than suppressing.
2. **Tilt gaps aloft (far range).** VCP 12/212 elevation gaps give 2–5 km vertical spacing beyond ~120 km; the trapezoid bridges gaps exactly like the operational HDA's linear interpolation between beams — accept it, but it smooths real cores. SAILS/MRLE supplemental 0.5° cuts: **dedupe cuts by elevation angle** before building `CutColumn`s (keep the scan closest to volume mid-time); duplicate-elevation segments contribute dh≈0 (harmless) but waste work and can produce non-monotonic profiles.
3. **Range limits.** Witt validated to 230 km. Beyond that, the 0.5° beam center is >5 km ARL and beam broadening (~4 km wide at 230 km) smears the profile. Recommendation: compute to 300 km but flag/fade beyond 230 km. Inside ~10 km, the base-tilt grid geometry plus truncation makes values junk — existing `gate_for_ground_range` already refuses to smear high tilts into the cone of silence (good; keep).
4. **Cold seasons / low melting level.** If surface T ≤ 0 °C, H0_arl → 0: the W_T ramp starts at the radar and `WT_thresh` hits its 20 J m⁻¹ s⁻¹ floor. Known HDA failure mode (winter over-detection). Suppress the product (or badge it "low confidence") when H0_arl < 500 m.
5. **H_m20 ≤ H0** (corrupt/extrapolated sounding): clamp `hm20 = max(hm20, h0 + 1.0)` as the existing code does; if the HRRR profile doesn't reach −20 °C, fall back to climatology (H0 + 3.2 km) and flag.
6. **POSH units bug to avoid:** `WT_thresh` takes H0 in **km**; SHI integration takes H in **meters**. Mixing these is the classic implementation error.

---

## 4. Sourcing H0 / H−20 from HRRR

Precedent: GridRad/M&H19 take 0 °C and −20 °C heights from **hourly RAP analyses**; MRMS does the same from its hourly model environment fields. Using HRRR is strictly better resolution-wise.

From the HRRR profile already available to the app (`rw_ui::SoundingData` via the model store; sounding-at-point requests already exist in `crates/app_ui/src/model_data.rs`):

1. Sample the profile **at the radar site** (phase 1: single profile per volume; freezing level varies slowly except across strong fronts — if you later care, sample a 3×3 of points across the 230-km disk and bilinearly interpolate per column, which is exactly the MRMS approach).
2. Compute crossing heights from (T, z) pairs: scan from the **top down** and take the **first (highest) crossing** of 0 °C and −20 °C, linearly interpolating z between the bracketing levels: `z = z_a + (T_a − T_x)/(T_a − T_b) · (z_b − z_a)`. Top-down gives the top of any elevated warm layer — the level above which hail growth is possible — and is robust to shallow surface inversions. If the whole profile is subfreezing, H0 = surface.
3. HRRR heights are **geopotential meters MSL** → convert to ARL: `h_arl = z_msl − radar_antenna_elevation_msl` (site elevation + antenna height from the Archive II volume header / ICD site info you already use for beam height).
4. Refresh on each new HRRR cycle/valid hour; cache `(h0, hm20)` per (site, valid-hour). Use the analysis or shortest-lead forecast valid nearest the volume scan time.
5. For HSDA (phase 2) you additionally need **wet-bulb** 0 °C and −25 °C heights: compute Tw per level (Stull 2011 approximation is adequate: needs T and RH, both in the HRRR profile) and find crossings the same way.

---

## 5. Dual-pol, phase 1.5 (cheap win): HDR — Hail Differential Reflectivity

Before committing to HSDA, there is a two-screen dual-pol product with real pedigree: **HDR** (Aydin, Seliga & Balaji 1986, *J. Climate Appl. Meteor.*, 25, 1475–1484; performance + size relation: Depue, Kennedy & Rutledge 2007, *J. Appl. Meteor. Climatol.*, 46, 1290–1301, doi:10.1175/JAM2529.1). Verified from pyhail `hdr.py`:

```
HDR = Z_H − f(ZDR)                      (dB)
f(ZDR) = 27                ZDR ≤ 0
       = 19·ZDR + 27       0 < ZDR ≤ 1.74
       = 60                ZDR > 1.74
size:  D(mm) = 0.0284·HDR² − 0.366·HDR + 11.69   (D = 0 for HDR ≤ 0; Depue et al. 2007)
```
Run it on the lowest tilt (it's a rain/hail discriminator below the melting layer). HDR > ~30 dB correlated with structurally damaging hail in Depue et al. Needs ZDR to be reasonably calibrated. This is a single-tilt gate-wise map — no column walk — and gives you a dual-pol cross-check on MESH for free.

## 6. Dual-pol, phase 2: HSDA — Hail Size Discrimination Algorithm

Citations: Ryzhkov, A. V., M. R. Kumjian, S. M. Ganson, and P. Zhang, 2013: *Polarimetric Radar Characteristics of Melting Hail. Part II: Practical Implications.* **JAMC**, 52, 2871–2886, doi:10.1175/JAMC-D-13-074.1. Validation/refinement: Ortega, K. L., J. M. Krause, and A. V. Ryzhkov, 2016: *Part III: Validation of the Algorithm for Hail Size Discrimination.* **JAMC**, 55, 829–848, doi:10.1175/JAMC-D-15-0203.1.

### What it is
Gate-level fuzzy-logic classifier → 3 classes: **small (<25 mm), large (25–50 mm), giant (>50 mm)**, using Z_H, Z_DR, ρ_hv in **six height layers** relative to the **wet-bulb** 0 °C level (verified from pyhail `hsda.py`):

| Layer | Bounds |
|---|---|
| 0 | above the −25 °C (wet-bulb) level |
| 1 | wbt-0 °C … −25 °C |
| 2 | wbt-0 °C − 1000 m … wbt-0 °C |
| 3 | −2000 … −1000 m below wbt-0 °C |
| 4 | −3000 … −2000 m below wbt-0 °C |
| 5 | > 3000 m below wbt-0 °C |

Per layer × class, **trapezoidal membership functions** (x1,x2,x3,x4) on each of (Z_H, Z_DR, ρ_hv). Representative verified breakpoints (full tables in pyhail `hsda_mf.py`, which encodes Ryzhkov 2013 Tables w/ Ortega 2016 mods — lift verbatim at implementation time):

- Layers 0–1 (above melting): Small Z_H [45,50,60,65], Large [48,58,63,68], Giant [50,60,100,101]; ZDR for all classes ≈ [−0.5,−0.3,0.3,0.5] (giant: left edge open, [−8.75,−7.75,0.3,0.5]); ρ_hv Small [0.92,0.96,0.99,1.00], Giant [−1,0,0.99,1.00] (layer-1 variants slightly looser).
- Layers 3–5 (melting/below): ZDR breakpoints become **functions of Z_H** plus the ZDR calibration offset `dzdr`:
  `f1(Z) = −0.5 + 2.5e-3·Z + 7.5e-4·Z² + dzdr`, `f2(Z) = 0.1·(Z−50) + dzdr`, `f3(Z) = 0.1·(Z−60) + dzdr` (layer 3 uses steeper g1/g2/g3 variants). E.g. layer 4–5 Small ZDR = [f2−0.3, f2, f1, f1+0.3], Large = [f3−0.3, f3, f2, f2+0.3].

**Aggregation** (verified from pyhail): for each class, `A = Σᵢ wᵢ·qᵢ·MFᵢ / Σᵢ wᵢ·qᵢ` over i ∈ {Z_H, Z_DR, ρ_hv}, with per-layer weight vectors w (tabulated in Ryzhkov et al. 2013; encoded in `hsda_mf.build_mf` — transcribe from there) and confidence/quality vector q (default 1s). **Decision rules:** (1) if min(MF_ZH, MF_ZDR, MF_ρhv) < 0.2 → unclassified; (2) pick argmax class, but if max A < 0.6 → small; (3) if Z_DR ≥ 2 dB, downgrade any large/giant to small (big ZDR = rain/small melting hail). Output the class.

### What it takes in this codebase
- ZDR, CC per tilt: **have**. Wet-bulb 0 °C / −25 °C: from HRRR (§4.5). ZDR calibration offset `dzdr`: estimate from light-rain self-consistency or expose as a user setting (defaults 0).
- **HCA gating**: the real HSDA runs only on gates the WSR-88D HCA (Park et al. 2009, *Wea. Forecasting*, 24, 730–748) labels rain/hail. We have no HCA. Pragmatic substitute (flag as a deviation): gate on `Z_H ≥ 45 dBZ` (HSDA membership is ~0 below that anyway) and `ρ_hv ≥ 0.80`, and exclude obvious three-body-spike gates (low Z, low ρ_hv down-radial of a core).
- Render as a 3-color overlay (small/large/giant) on the lowest tilt or as a column-max class.

### Is it worth phase 2?
**Yes, but after MESH-family + POH + HDR, and with expectations set.** Verified skill (Ortega et al. 2016): the refined HSDA reached POD ≈ 0.594, FAR ≈ 0.136 against severe reports — useful, not magic; later literature consistently notes over-calling of giant hail and sensitivity to ZDR calibration and the melting-layer estimate. Its unique value over MESH: it is **gate-level and near-surface** (what's reaching the ground now, including melting effects) whereas MESH is a column climatological size proxy. The two disagree usefully. Effort estimate: the fuzzy machinery is small; the cost is transcribing the MF/weight tables (one afternoon against pyhail `hsda_mf.py` + Ryzhkov 2013 tables) and the wet-bulb profile code.

---

## 7. Build order (recommendation)

1. **Phase 1 — SHI/MESH/POSH grids** (1–2 days): refactor `mehs_grid` → `hail_grids` (§3.1–3.2); HRRR-fed `HailEnv` (§4); three MESH calibrations with **corrigendum constants** (§2); default display MESH₉₅; POSH rounded to 10s; quality flags for cone-of-silence/range (§3.3). Cite Witt 1998 + M&H 2019 + corrigendum 2021 in the constants' doc comments.
2. **Phase 1.5 — POH + HDR** (half day each): POH from `echo_top_grid(45.0)` + Waldvogel table (§1.7); HDR lowest-tilt overlay (§5).
3. **Phase 2 — HSDA** (§6), gated on a ZDR-calibration story.

## 8. Validation plan

- **Analytic unit test:** uniform 55 dBZ from 2–10 km ARL, H0 = 3 km, H_m20 = 6 km. Ė(55) = 5e-6·10^4.62·1 = 0.20845 J m⁻² s⁻¹; SHI = 0.1·Ė·[½·(6−3)·1000 + (10−6)·1000] = 0.1·0.20845·5500 ≈ **114.6 J m⁻¹ s⁻¹**; MESH_witt ≈ 27.2 mm; MESH₉₅ ≈ 60.6 mm; WT(3 km) = 51.5 → POSH = 29·ln(114.6/51.5)+50 ≈ 73% → display 70%.
- **Identity test:** SHI = WT_thresh ⟹ POSH = 50.
- **Corrigendum regression test:** assert coefficients (15.096, 0.206) / (22.157, 0.212) — guard against someone "fixing" them back to the paper's printed values.
- **Cross-check vs pyhail** on one exported volume (dump `column_profile` outputs, run pyhail's grid path, compare SHI within a few %).
- **Case validation:** the KEAX 2026-06-09 05:51 UTC derecho scans already in the repo's test rotation, vs archived MRMS MESH (Witt calibration) for the same times.

## 9. Reference implementations & docs used
- pyhail (J. Soderholm et al.): github.com/joshua-wx/pyhail — `src/pyhail/mesh_grid.py`, `mesh_formulas.py`, `hsda.py`, `hsda_mf.py`, `hdr.py` (constants cross-checked, including corrigendum values).
- NOAA WDTD product docs: vlab.noaa.gov/web/wdtd — SHI and POSH pages (40/50 dBZ ramp, WT floor 20, POSH rounding/clamp).
- M&H19 full text: pmc.ncbi.nlm.nih.gov/articles/PMC8050948/ (Eqs. 2–6, 15–16; GridRad/RAP environmental sourcing).
- Forcadell et al. 2024, AMT 17, 6707 (independent restatement of the Witt equation set).


## HAIL PAPERS
- Witt, A., M. D. Eilts, G. J. Stumpf, J. T. Johnson, E. D. Mitchell, and K. W. Thomas, 1998: An Enhanced Hail Detection Algorithm for the WSR-88D. Wea. Forecasting, 13, 286-303, doi:10.1175/1520-0434(1998)013<0286:AEHDAF>2.0.CO;2
- Murillo, E. M., and C. R. Homeyer, 2019: Severe Hail Fall and Hailstorm Detection Using Remote Sensing Observations. J. Appl. Meteor. Climatol., 58, 947-970, doi:10.1175/JAMC-D-18-0247.1
- Murillo, E. M., and C. R. Homeyer, 2021: Corrigendum (to M&H 2019 — corrected MESH75=15.096*SHI^0.206, MESH95=22.157*SHI^0.212). J. Appl. Meteor. Climatol., 60(3), doi:10.1175/JAMC-D-20-0271.1
- Waldvogel, A., B. Federer, and P. Grimm, 1979: Criteria for the Detection of Hail Cells. J. Appl. Meteor., 18, 1521-1525 (POH table)
- Ryzhkov, A. V., M. R. Kumjian, S. M. Ganson, and P. Zhang, 2013: Polarimetric Radar Characteristics of Melting Hail. Part II: Practical Implications. J. Appl. Meteor. Climatol., 52, 2871-2886, doi:10.1175/JAMC-D-13-074.1 (HSDA)
- Ortega, K. L., J. M. Krause, and A. V. Ryzhkov, 2016: Polarimetric Radar Characteristics of Melting Hail. Part III: Validation of the Algorithm for Hail Size Discrimination. J. Appl. Meteor. Climatol., 55, 829-848, doi:10.1175/JAMC-D-15-0203.1
- Aydin, K., T. A. Seliga, and V. Balaji, 1986: Remote Sensing of Hail with a Dual Linear Polarization Radar. J. Climate Appl. Meteor., 25, 1475-1484 (HDR)
- Depue, T. K., P. C. Kennedy, and S. A. Rutledge, 2007: Performance of the Hail Differential Reflectivity (HDR) Polarimetric Radar Hail Indicator. J. Appl. Meteor. Climatol., 46, 1290-1301, doi:10.1175/JAM2529.1
- Smith, T. M., and Coauthors, 2016: Multi-Radar Multi-Sensor (MRMS) Severe Weather and Aviation Products: Initial Operating Capabilities. Bull. Amer. Meteor. Soc., 97, 1617-1630, doi:10.1175/BAMS-D-14-00173.1 (grid-based MESH precedent)
- Park, H. S., A. V. Ryzhkov, D. S. Zrnic, and K.-E. Kim, 2009: The Hydrometeor Classification Algorithm for the Polarimetric WSR-88D. Wea. Forecasting, 24, 730-748, doi:10.1175/2008WAF2222205.1 (HCA gating for HSDA)
- Cintineo, J. L., T. M. Smith, V. Lakshmanan, H. E. Brooks, and K. L. Ortega, 2012: An Objective High-Resolution Hail Climatology of the Contiguous United States. Wea. Forecasting, 27, 1235-1248, doi:10.1175/WAF-D-11-00151.1 (MESH ~29 mm severe threshold)
- Murillo, E. M., C. R. Homeyer, et al., 2021: A 23-Year Severe Hail Climatology Using GridRad MESH Observations (PMC8050942; MESH75/MESH95 decision thresholds)

## WIND
# Damaging-Wind Algorithms for a Rust NEXRAD L2 Viewer — Research + Implementation Spec

All constants below were verified against primary sources (paper full texts or NOAA/NWS operational documentation) on 2026-06-10. Where a number is an engineering choice rather than a published constant, it is explicitly flagged **[ENG]**. Existing engine capabilities assumed: dealiased velocity per tilt, LLSD azimuthal shear + radial divergence (Smith & Elmore 2004), watershed cell ID + TITAN-style tracking, echo tops/VIL, beam-height computation, HRRR soundings.

---

## 1. Literature findings (verified)

### 1.1 MARC — Mid-Altitude Radial Convergence

**Primary source:** Schmocker, Przybylinski & Lin (1996), *Preprints, 15th Conf. on Weather Analysis and Forecasting* (Norfolk, VA), AMS, 306–311: "Forecasting the initial onset of damaging downburst winds associated with a mesoscale convective system (MCS) using the mid-altitude radial convergence (MARC) signature." Context review: Przybylinski (1995), *Wea. Forecasting*, **10**, 203–218 (bow-echo review; MARC as precursor to descending rear-inflow jet). Detailed operational case: Funk, DeWald & Lin, NWS Louisville COMET conference paper on the 14 May 1995 KY bow echo (weather.gov/lmk/paper-51495); current NWS operational guidance at weather.gov/lmk/squallbow.

Verified definition and constants:
- **MARC ΔV** = difference between the **maximum inbound and maximum outbound velocity within 6 km along a single radial** (storm-relative velocity preferred operationally, but the differential is insensitive to a constant storm-motion projection on one radial — see §2.2).
- **Threshold: ΔV ≥ 25 m/s (50 kt)** at an **altitude of 3–7 km** preceded the onset of damaging surface winds by **up to 20 min** (Schmocker et al. 1996). NWS LMK operational page: "strong (over 50 kts; 25 m/s), **persistent, deep-layered**" convergence at ~3–7 km; lead **15–20 min**.
- 14 May 1995 case (Funk et al.): significant convergence appeared **30–35 min** before damaging winds; max MARC ≈ **38 m/s at 4.5–5.5 km**; during strongest MARC the convergence spanned a **vertical depth of 6–8 km**; the convergent zone's radial **width was only 1–3 km** (well under the 6-km search window).
- Eilts et al. (1996, 18th SLS preprints; as summarized in Smith et al. 2004): the three best downburst precursors over 85 cases were (i) **rapidly descending reflectivity core**, (ii) **strong, deep convergence at mid-altitudes (2–6 km AGL)**, (iii) reflectivity core that **starts unusually high**.
- **Key caveat (verified quote):** "MARC is limited when velocities within mid-level convergence zones are oriented normal to the radar beam and thus greatly underestimated or masked." → viewing-angle QC is mandatory.

### 1.2 Low-level velocity → surface gust

**Finding: no peer-reviewed universal "80–85% of 0.5° velocity" constant exists.** NWS operational guidance (LMK squallbow page, AWOC/RAC) deliberately avoids a fixed percentage. The defensible quantitative anchors are:

1. **Smith, Elmore & Dulin (2004)**, *Wea. Forecasting* **19**, 240–250 (DDPDA): radar proxies accepted as *equivalent to* a severe (≥50 kt) surface gust: **radar-measured radial wind ≥ 25 m/s, or a divergent ΔV ≥ 40 m/s along a radial, observed within 1 km of the surface** (beam centerline < 1 km above radar pedestal). I.e., the NWS research practice maps low-beam velocity ≈ 1:1 to surface gust, not 0.8×.
2. **Hjelmfelt (1988)**, *J. Appl. Meteor.* **27**, 900–927: microburst outflow has a "nose" profile peaking at **~50–100 m AGL** (i.e., usually *below* the beam), and single-Doppler estimates of maximum velocity differentials **frequently underestimate the true differential by ≥ 50%** (as cited in Smith et al. 2004). → the lowest-tilt velocity is a **floor**, not an overestimate, for downburst outflows.
3. **Ibrahim, Kopp & Sills (2023)**, *J. Atmos. Oceanic Technol.* **40**(2), doi:10.1175/JTECH-D-22-0028.1: SOTA non-ML retrieval. Partitioned-VAD on lowest tilts vs >2,600 thunderstorm events at 19 radar–ASOS pairs (<10 km): MAE **1.5–4.5 m/s** depending on altitude, with an exponential height correction improving strong events. Validates that low-altitude radar velocity is a skillful surface-gust estimator at close range.
4. **Sherburn, Bunkers & Mose (2021)**, *Wea. Forecasting* **36**(4), doi:10.1175/WAF-D-20-0221.1: **Wind Gust Ratio (WGR) = measured peak gust / outflow-boundary speed**; n=943 CONUS cases: **median 1.44, mean 1.68, IQR 1.19–1.91**; WGR decreases as boundary speed increases and is *lower for linear (QLCS) modes*, higher for steep low-level lapse rates.
5. **Krupar et al. (2016)**, *Wea. Forecasting* **31**(4), doi:10.1175/WAF-D-15-0162.1 (tropical cyclones only): ASOS 10-m gust ≈ **0.67–0.86 ×** the VAD 0–500-m mean boundary-layer wind (site-dependent); best 10-m mean predictor was a linear regression on the VAD 0–200-m layer mean. This is the published basis for sub-1.0 reduction factors — **valid for TC/synoptic boundary layers, not convective downbursts**.

### 1.3 Legacy WSR-88D downburst algorithms — DDPDA (Smith, Elmore & Dulin 2004; full text verified)

- **Scope:** pulse/multicell downbursts in **weak shear (surface-to-500-mb shear 0–15 m/s)**, moderate-high CAPE; cells within **80 km** of the radar; two range bands **20–45 km** and **45–80 km** (no equations < 20 km — storm top unsampled).
- **Velocity preprocessing (identical to your LLSD module):** 3×3 median filter (3 gates × 3 azimuths), then 2-D linear least squares on a **5×5 template**: divergent shear `u_r = Σ(i·u_ij)/(50·Δr)`, rotational shear `u_s = Σ(j·u_ij)/(50·r₀·Δφ)`, where i,j ∈ {−2…2}, Δr = gate spacing (250 m legacy), Δφ = beamwidth in meters at range r₀, and 50 = Σi² over the template; result median-filtered again.
- **26 parameters** (Table 2; the ones that matter, with exact definitions):
  - `CONV006/004/002/001` — cross-sectional **area coverage of LLSD radial convergence exceeding 0.006/0.004/0.002/0.001 s⁻¹ in the 1–6-km-MSL layer** (within 5-km radius of cell centroid, all tilts).
  - `DPTHC` — **depth of convergence exceeding 0.004 s⁻¹**; `DPTHDV` — depth of convergent ΔV exceeding **10 m/s**.
  - `C16`/`DV16` — max LLSD convergence / max convergent ΔV in the **1–6 km MSL** layer; `CNVMELT`/`DVMELT` — same near the **environmental 0 °C height**; `CTHTE`/`DVTHTE` — same near the **height of minimum environmental θe**.
  - Reflectivity: `VIL`, `MASSHT` (height of center of mass), `ASP` (core aspect ratio = **cell depth / cell width**), `MAXDBZ`, `DBZHT` (height of max reflectivity), `DBZp7KM` (max Z above 7 km MSL), `ZTHTE`/`ZATHTE` (max Z near/above min-θe height), `VOL`, `SHI`; rotation: `MAXR17`, `MINR17` (max +/− LLSD rotation, 1–7 km MSL).
- **Most skillful predictors, 20–45 km:** VIL, SHI, MASSHT, ASP (reflectivity core elongating/descending) + **CONV006** as the dominant velocity parameter. At 45–80 km: SHI, VIL, ASP dominate (velocity field too noisy).
- **Detection sub-algorithm (non-predictive, lowest tilts only):** along-radial segments where LLSD divergence `u_r > 0.0008 s⁻¹`, **minimum segment length 1.5 km**, within **5 km** of a cell centroid, on any tilt with **beam centerline < 1 km** above the radar. For each segment compute max radial-velocity difference **ΔV** and absolute radial wind speed **ARWS**. Alerts: **SEVERE if ARWS ≥ 25 m/s or ΔV ≥ 40 m/s; MODERATE (aviation) if ARWS ≥ 18 m/s or ΔV ≥ 25 m/s**.
- **Skill:** median HSS **0.40** with median lead **5.5 min** (20–45 km); HSS **0.17**, lead 0 min (45–80 km). Typical first-echo→outflow time only ~15 min in weak shear.
- **Do not port the LDA coefficients** (Table 3): they are tuned to SCIT/HDA-specific parameter scalings and a 65/35 resampled dataset; the paper itself flags SCIT dependency as a weakness. Port the *parameters and detection thresholds*; surface trends to the user (their Fig. 8c time–height trend display is the model).
- Related: **Wilson et al. (1984)**, *J. Climate Appl. Meteor.* **23**, 898–915 — the canonical microburst definition: **divergent ΔV ≥ 10 m/s within 4 km horizontal distance** at the lowest tilt (also TDWR MBA heritage; also used by Kuster et al. 2021 as the downburst-onset clock). **Roberts & Wilson (1989)**, *J. Appl. Meteor.* **28**, 285–303 — precursors (descending reflectivity core, increasing in-cloud convergence at 3–8 km AGL/near cloud base, rotation, reflectivity notch) appear only **~2–6 min** before outflow onset in Colorado cells. Note "AMDA" in recent AMS abstracts is an ASR-9-derived *Automated Microburst Detection Algorithm* applied to NEXRAD out to 60–70 km — segment-based divergence detection, same Wilson ΔV heritage; no additional published constants worth importing.

### 1.4 Newer physically-based work

- **KDP cores (dual-pol): Kuster et al. (2021)**, *Wea. Forecasting* **36**(4), 1183–1198, doi:10.1175/WAF-D-21-0005.1. A **KDP core = region of KDP ≥ 1.0 °/km near or within 3 km below the environmental melting layer**. Over 81 downbursts (10 states): cores developed **≤ 31 min (mean 15 min) before downburst development** (downburst = ΔV ≥ 10 m/s divergent signature at lowest tilt); 75% showed a local maximum in core max-KDP before peak downburst intensity; **larger KDP and larger vertical KDP gradient below the melting layer favor strong (≥ 25.7 m/s / 50 kt or damage) vs weak (≤ 15.4 m/s) downbursts**. Implementable from L2 dual-pol (KDP from smoothed ΦDP) + HRRR 0 °C height.
- **Sounding-based indices (you have HRRR profiles):**
  - **WINDEX** (McCann 1994, *Wea. Forecasting* **9**, 532–541): `WI = 5·[H_M·R_Q·(Γ² − 30 + Q_L − 2·Q_M)]^0.5` in **knots**, where H_M = melting-level height AGL (km), Γ = surface→melting-level lapse rate (°C/km), Q_L = mean mixing ratio in lowest 1 km (g/kg), Q_M = mixing ratio at the melting level (g/kg), **R_Q = Q_L/12, capped at 1**; negative radicand ⇒ WI = 0. Estimates *maximum possible* microburst gust.
  - **Wet-microburst Δθe** (Atkins & Wakimoto 1991, *Wea. Forecasting* **6**, 470–482): θe(surface max) − θe(min aloft) **≥ 20 K ⇒ microburst day; ≤ 13 K ⇒ unlikely**. Operationalized as **MDPI = (max θe in lowest 150 mb − min θe aloft) / CT, CT = 30 K**, MDPI ≥ 1 favorable (Wheeler & Spratt 1996, NWS SR Tech. Memo 163; AMU verification at Cape Canaveral).
  - Parcel-theory bound `V_max ≈ √(2·DCAPE)` is a useful display adjunct (treat as an upper bound, not a forecast).
- ML products (ProbSevere wind, etc.) excluded per scope.

---

## 2. What to implement

Recommended: **(a) MARC detector**, **(b) Peak Low-Level Velocity / gust-floor product**, **(c) DDPDA-lite downburst precursor panel** (cheap — it reuses cells, VIL, echo tops, LLSD), with the **KDP-core precursor** as a fast-follow if you parse dual-pol moments. All four ride existing infrastructure.

### 2.1 Module A — MARC detector (`marc.rs`)

**Inputs:** dealiased velocity per tilt; beam height per (tilt, gate); cell objects + motion vectors (TITAN); volume timestamps.

**Constants** (all published unless [ENG]):
```rust
/// Schmocker, Przybylinski & Lin (1996), 15th Conf. WAF, 306-311;
/// NWS LMK operational guidance (weather.gov/lmk/squallbow).
pub const MARC_WINDOW_KM: f32        = 6.0;   // along-radial search window
pub const MARC_DELTAV_WARN: f32      = 25.0;  // m/s (50 kt) damaging-wind precursor
pub const MARC_DELTAV_EXTREME: f32   = 38.0;  // m/s observed in 14 May 1995 KY case (context tier)
pub const MARC_LAYER_BOTTOM_KM: f32  = 3.0;   // ARL; Eilts et al. 1996 used 2-6 km AGL
pub const MARC_LAYER_TOP_KM: f32     = 7.0;   // ARL
pub const MARC_MIN_DEPTH_KM: f32     = 1.5;   // [ENG] ">=2 tilts in layer" proxy for "deep-layered"
pub const MARC_PERSIST_VOLS: u32     = 2;     // [ENG] proxy for "persistent" (NWS: persistent, deep)
pub const MARC_VIEWANGLE_MAX_DEG: f32= 60.0;  // [ENG] flag when convergence axis >60° off-radial
pub const MARC_CELL_RADIUS_KM: f32   = 10.0;  // [ENG] association radius to cell centroid (QLCS cores are elongated)
```

**Per-tilt scan (only tilts whose beam-center height anywhere in 15–160 km falls in 3–7 km ARL):**
1. For each radial, restrict to gates where beam height ∈ [3, 7] km ARL and Z ≥ 30 dBZ within the cell's reflectivity envelope **[ENG: dBZ gate suppresses clear-air folds]**.
2. Sliding-window max convergent differential, O(n) with a monotonic deque:
   `ΔV_conv(g) = max over pairs a<b within 6 km of [ V(a) − V(b) ]` with V > 0 = outbound. (Near-gate outbound + far-gate inbound ⇒ convergence; this exactly implements "difference between maximum inbound and outbound velocity within 6 km along a radial." Note the convergent zone itself is only 1–3 km wide — do not shrink the window below 6 km, but report the actual min/max gate separation.)
3. Optional pre-filter: only evaluate windows containing LLSD radial divergence ≤ −0.002 s⁻¹ (reuses your existing field; DDPDA's CONV002 floor).
4. A tilt-level MARC sample = (ΔV_conv, height, range, azimuth, window centroid).

**Volume assembly + tiers:**
- Cluster samples within 5 km horizontal across tilts; **candidate** if ΔV ≥ 25 m/s on ≥ 1 tilt in-layer; **MARC (deep)** if samples span ≥ MARC_MIN_DEPTH_KM in height (Funk et al. observed 6–8 km depth in the severe case); **MARC (confirmed)** if deep for ≥ 2 consecutive volumes.
- **Viewing-angle QC:** estimate the convergence axis as the cell motion vector (QLCS rear-inflow is quasi-parallel to motion). If the angle between cell motion and the radial > 60°, tag `aspect_limited: true` and render hollow — the signature is *underestimated or masked*, never false-positive-prone, in this geometry.
- Storm-relative is unnecessary for ΔV (constant projection cancels on a radial pair at similar azimuth), but **use SRM for the companion display** so forecaster-style inspection matches NWS practice.
- **Lead-time semantics for UI:** "damaging surface winds possible within ~10–30 min downstream of MARC centroid along cell motion" (Schmocker: up to 20 min; Funk case: max damage 30–40 min after peak MARC).

### 2.2 Module B — Peak Low-Level Velocity + gust floor (`llv_gust.rs`)

**Product 1 — PLLV swath.** Per pixel: max |dealiased V| over the lowest tilt(s) whose **beam centerline < 1.0 km ARL** (DDPDA's sampling rule). Maintain a rolling **60-min max swath** (analog of MRMS rotation tracks) keyed to cell tracks. Per cell: `pllv`, its beam height, range, azimuth.

**Product 2 — divergent outflow detection** (DDPDA detection routine, verbatim constants):
```rust
/// Smith, Elmore & Dulin (2004), Wea. Forecasting 19, 240-250, sections 2d & 4.
pub const DIV_SEG_SHEAR: f32      = 0.0008; // s^-1 LLSD divergence to seed a segment
pub const DIV_SEG_MIN_LEN_KM: f32 = 1.5;
pub const DIV_CELL_RADIUS_KM: f32 = 5.0;
pub const DIV_BEAM_MAX_KM: f32    = 1.0;    // beam centerline above radar pedestal
pub const DET_SEVERE_ARWS: f32    = 25.0;   // m/s  -> "SEVERE downburst"
pub const DET_SEVERE_DV: f32      = 40.0;   // m/s along-radial divergent ΔV
pub const DET_MODERATE_ARWS: f32  = 18.0;   // m/s  -> "MODERATE downburst" (aviation)
pub const DET_MODERATE_DV: f32    = 25.0;   // m/s
/// Wilson, Roberts, Kessinger & McCarthy (1984), JCAM 23, 898-915.
pub const MICROBURST_DV: f32      = 10.0;   // m/s divergent ΔV ...
pub const MICROBURST_DIST_KM: f32 = 4.0;    // ... within 4 km  -> "microburst"
```

**Gust mapping — the honest spec.** Do **not** apply a fixed 0.80–0.85 reduction; it has no primary-source basis for convective wind and is wrong-signed for downbursts (outflow nose at 50–100 m AGL is below the beam; single-Doppler differentials underestimate by ≥ 50%, Hjelmfelt 1988). Instead:
- Report `surface_gust_floor = PLLV` with **"≥" semantics** when beam height ≤ 1.0 km and the pixel is convective (in/adjacent to a tracked cell): cite Smith et al. (2004) severe-equivalence (25 m/s radial wind ≈ 50-kt gust) and Ibrahim et al. (2023) skill (MAE 1.5–4.5 m/s near radar).
- Above 1.0 km beam height, label the value **"wind aloft"** — offer an *optional, clearly-labeled* mixing estimate only when the HRRR sounding supports momentum transfer: if the lapse rate from the surface to beam height is ≥ ~8 °C/km or DCAPE > 0 through the layer, show `gust_potential ≈ PLLV` (full transfer); otherwise suppress. **[ENG — physically motivated, no published constant]**
- **WGR estimate (independent, published):** when the tracker measures a gust-front/outflow-boundary speed `S` (fine-line motion or surge of the cell-edge), display `gust_est = 1.44·S` with uncertainty band `[1.19·S, 1.91·S]` (Sherburn et al. 2021 median/IQR); annotate "ratios run lower for linear modes & fast boundaries."
- **HRRR environmental chip per cell:** WINDEX (McCann 1994 formula above, knots), MDPI (CT = 30 K), Δθe (≥ 20 K wet-microburst flag, Atkins & Wakimoto 1991), √(2·DCAPE) upper bound. Cap any displayed gust estimate at max(WINDEX, √(2·DCAPE)) only for *labeling* ("near environmental ceiling"), never to reduce a measured velocity.

**Severity tiers for display:** ≥ 26 m/s (50 kt, NWS severe) red; 18–26 m/s amber (DDPDA moderate/aviation); microburst couplet glyph when Wilson criterion met.

### 2.3 Module C — DDPDA-lite precursor panel (`downburst_precursors.rs`)

Reuse cells/VIL/echo-tops/LLSD to compute, per cell per volume, the parameters DDPDA found most skillful, and render a **time–height trend panel** (their Fig. 8c pattern: max-Z height trace + convergence/divergence trace) rather than porting LDA weights:
- `CONV006`: area (km²) of LLSD radial convergence ≥ **0.006 s⁻¹** in the **1–6 km MSL** layer within 5 km of centroid (the single best velocity predictor at 20–45 km).
- `DPTHC`: depth of convergence ≥ **0.004 s⁻¹**; `DV16`: max convergent ΔV in 1–6 km MSL.
- `MASSHT`, `DBZHT`, `ASP` (= depth/width from watershed footprint + echo top/base), `VIL`, `MAXDBZ`.
- **Descending-core flag [ENG thresholds, published mechanism]:** `DBZHT` falls ≥ 1 km per volume while `MAXDBZ` ≥ 55 dBZ and VIL peaks-then-falls — the Roberts & Wilson (1989)/Eilts (1996) precursor. Expect only **~2–10 min** of lead for pulse storms; combine with MARC for QLCS (longer lead) and gate "pulse mode" on HRRR sfc–500-mb shear ≤ 15 m/s (DDPDA's validity envelope).
- **KDP core (fast-follow, dual-pol):** core = KDP ≥ **1.0 °/km** within [melting layer … melting layer − 3 km] (melting height from HRRR 0 °C); track core max/median/size; alert on development (mean lead 15 min, max 31) and on increasing vertical KDP gradient below the melting layer (strong-downburst discriminator). (Kuster et al. 2021.)

### 2.4 Display recommendations

- **MARC:** chevron/butterfly icon at the convergence centroid projected to the lowest tilt, colored by ΔV (25 amber / 30 red / 38 magenta m/s), hollow when `aspect_limited`, with a downstream lead-time wedge along cell motion (10–30 min). Clicking opens an along-radial V cross-section (SRM) for the flagged azimuth.
- **PLLV/gust:** swath layer (60-min rolling max) + per-cell badge: `≥ 27 m/s @ 0.4 km ARL, 32 km`. Always print beam height with the number — that is the operational discipline the literature demands.
- **Downburst:** "S"/"M" oval icons on lowest tilt (DDPDA's WDSS-II convention) + microburst couplet glyph; cell trend panel with max-Z height, VIL, CONV006 area, and (if dual-pol) KDP-core height/intensity traces.
- All thresholds in a `damaging_wind::constants` module with doc-comments citing the exact paper/page, per repo citation policy.

### 2.5 Caveats to encode

- MARC undetectable when convergence axis ⊥ beam (flag, don't silently miss); 3–7-km layer sampled only ~15–160 km from the radar (tilt geometry); QLCS apex regions can alias severely — your region-based dealiaser's fold-boundary count is a good per-cell quality gate; DDPDA-lite prediction tier is only validated for weak-shear pulse regimes (HSS 0.40 close-range, lead ~5.5 min — set expectations in UI); WGR assumes a measurable boundary speed; WINDEX/MDPI are environment-scale (per-sounding), not per-cell measurements.

### 2.6 Build order

1. **B first** (PLLV + DDPDA detection + Wilson microburst): pure lowest-tilt math on existing dealiased fields; immediate user value; constants fully published.
2. **A (MARC)**: the marquee QLCS/derecho precursor; one O(n)-per-radial kernel + clustering + persistence; validates beautifully on the KEAX 2026-06-09 derecho data already in the repo.
3. **C (trend panel)**, then **KDP cores** once dual-pol moments are surfaced.

---

## 3. Primary sources (verified this session)

- Schmocker, Przybylinski & Lin 1996, 15th Conf. WAF (AMS), 306–311 — MARC: ΔV ≥ 25 m/s within 6 km along-radial, 3–7 km, ≤ 20-min lead.
- Przybylinski 1995, *WAF* **10**, 203–218 — bow-echo signatures (RIN, MARC-precursor concept).
- Funk, DeWald & Lin (NWS LMK, weather.gov/lmk/paper-51495) — 14 May 1995 case: 38 m/s MARC at 4.5–5.5 km, 6–8-km depth, 1–3-km width, 30–35-min lead.
- Smith, Elmore & Dulin 2004, *WAF* **19**, 240–250, doi:10.1175/1520-0434(2004)019<0240:ADDPAD>2.0.CO;2 — DDPDA: full text verified (params, LLSD kernel, 0.0008 s⁻¹/1.5 km/5 km/1 km detection, 25/40 & 18/25 m/s tiers, HSS 0.40/0.17, leads 5.5/0 min).
- Wilson, Roberts, Kessinger & McCarthy 1984, *JCAM* **23**, 898–915 — microburst ΔV ≥ 10 m/s over ≤ 4 km.
- Roberts & Wilson 1989, *JAM* **28**, 285–303 — precursors, 2–6-min leads.
- Hjelmfelt 1988, *JAM* **27**, 900–927 — outflow nose 50–100 m AGL; ≥ 50% differential underestimate.
- Kuster et al. 2021, *WAF* **36**, 1183–1198, doi:10.1175/WAF-D-21-0005.1 — KDP cores ≥ 1.0 °/km, mean 15-min lead.
- Sherburn, Bunkers & Mose 2021, *WAF* **36**(4), doi:10.1175/WAF-D-20-0221.1 — WGR median 1.44 (IQR 1.19–1.91).
- Ibrahim, Kopp & Sills 2023, *JTECH* **40**(2), doi:10.1175/JTECH-D-22-0028.1 — radar→ASOS peak-wind retrieval, MAE 1.5–4.5 m/s.
- Krupar et al. 2016, *WAF* **31**(4), doi:10.1175/WAF-D-15-0162.1 — TC gust ratios 0.67–0.86 × 0–500-m mean (TC-only).
- McCann 1994, *WAF* **9**, 532–541 — WINDEX (formula + R_Q = Q_L/12 ≤ 1 verified against the paper PDF).
- Atkins & Wakimoto 1991, *WAF* **6**, 470–482 — Δθe ≥ 20 K / ≤ 13 K; Wheeler & Spratt 1996 (NWS SR Tech Memo 163) + AMU report — MDPI, CT = 30 K.
- NWS operational: weather.gov/lmk/squallbow (MARC > 50 kt, 3–7 km, 15–20-min lead); WDTB AWOC Severe Track FY10 QLCS lesson (mesovortex context: tornadic mesovortices mean Vr ≈ 12 m/s low-level, azshear > 10×10⁻³ s⁻¹, ~2-km diameter — optional companion product riding existing azShear).

Local artifacts from this session: DDPDA full text `C:\Users\drew\AppData\Local\Temp\ddpda.pdf` (+ extracted text at `C:\Users\drew\.claude\projects\C--Users-drew\2c09b16f-3f0d-478d-b6dc-78d5612dd641\tool-results\b4vb0zaeo.txt`), Kuster `kuster.pdf`, Sherburn `wgr.pdf`, Krupar `krupar.pdf` in the same Temp dir; AWOC QLCS + Wheeler/Spratt memo PDFs under `C:\Users\drew\.claude\projects\C--Users-drew\...\tool-results\`.

## WIND PAPERS
- Schmocker, G. K., R. W. Przybylinski, and Y.-J. Lin, 1996: Forecasting the initial onset of damaging downburst winds associated with a mesoscale convective system (MCS) using the mid-altitude radial convergence (MARC) signature. Preprints, 15th Conf. on Weather Analysis and Forecasting, Norfolk VA, AMS, 306-311
- Przybylinski, R. W., 1995: The bow echo: Observations, numerical simulations, and severe weather detection methods. Wea. Forecasting, 10, 203-218, doi:10.1175/1520-0434(1995)010<0203:TBEONS>2.0.CO;2
- Funk, T. W., V. L. DeWald, and Y.-J. Lin: A detailed WSR-88D Doppler radar evaluation of a damaging bow echo event on 14 May 1995 over north-central Kentucky. NWS Louisville COMET conference paper, https://www.weather.gov/lmk/paper-51495
- Smith, T. M., K. L. Elmore, and S. A. Dulin, 2004: A damaging downburst prediction and detection algorithm for the WSR-88D. Wea. Forecasting, 19, 240-250, doi:10.1175/1520-0434(2004)019<0240:ADDPAD>2.0.CO;2
- Wilson, J. W., R. D. Roberts, C. Kessinger, and J. McCarthy, 1984: Microburst wind structure and evaluation of Doppler radar for airport wind shear detection. J. Climate Appl. Meteor., 23, 898-915, doi:10.1175/1520-0450(1984)023<0898:MWSAEO>2.0.CO;2
- Roberts, R. D., and J. W. Wilson, 1989: A proposed microburst nowcasting procedure using single-Doppler radar. J. Appl. Meteor., 28, 285-303
- Hjelmfelt, M. R., 1988: Structure and life cycle of microburst outflows observed in Colorado. J. Appl. Meteor., 27, 900-927
- Kuster, C. M., B. R. Bowers, J. T. Carlin, T. J. Schuur, J. W. Brogden, R. Toomey, and A. Dean, 2021: Using KDP cores as a downburst precursor signature. Wea. Forecasting, 36, 1183-1198, doi:10.1175/WAF-D-21-0005.1
- Sherburn, K. D., M. J. Bunkers, and A. J. Mose, 2021: Radar-based comparison of thunderstorm outflow boundary speeds versus peak wind gusts from automated stations. Wea. Forecasting, 36(4), doi:10.1175/WAF-D-20-0221.1
- Ibrahim, I., G. A. Kopp, and D. M. L. Sills, 2023: Retrieval of peak thunderstorm wind velocities using WSR-88D weather radars. J. Atmos. Oceanic Technol., 40(2), doi:10.1175/JTECH-D-22-0028.1
- Krupar, R. J. III, J. L. Schroeder, D. A. Smith, S.-L. Kang, and S. Lorsolo, 2016: A comparison of ASOS near-surface winds and WSR-88D-derived wind speed profiles measured in landfalling tropical cyclones. Wea. Forecasting, 31(4), doi:10.1175/WAF-D-15-0162.1
- McCann, D. W., 1994: WINDEX - a new index for forecasting microburst potential. Wea. Forecasting, 9, 532-541, doi:10.1175/1520-0434(1994)009<0532:WNIFFM>2.0.CO;2
- Atkins, N. T., and R. M. Wakimoto, 1991: Wet microburst activity over the southeastern United States: Implications for forecasting. Wea. Forecasting, 6, 470-482
- Wheeler, M., and S. M. Spratt, 1995: Forecasting the potential for central Florida microbursts. NWS Southern Region Tech. Memo. 163 (MDPI operational form, CT=30K; AMU verification NASA-CR-201354, 1996)
- Smith, T. M., and K. L. Elmore, 2004: The use of radial velocity derivatives to diagnose rotation and divergence (LLSD). 11th Conf. on Aviation, Range, and Aerospace Meteorology, AMS (preprocessing kernel reproduced in Smith et al. 2004 WAF)
- Eilts, M. D., et al., 1996: Severe weather warning decision support / damaging downburst precursors, Preprints 18th Conf. on Severe Local Storms, AMS (2-6 km AGL deep convergence precursor; cited via Smith et al. 2004)
- NWS Louisville operational guidance: Squall line / bow echo / QLCS radar interrogation, https://www.weather.gov/lmk/squallbow (MARC >50 kt at 3-7 km, 15-20 min lead)
- WDTB AWOC Severe Track FY10: QLCS storm-scale interrogation and warning considerations, https://training.weather.gov/wdtd/courses/woc/documentation/severe/qlcs.pdf

## HAIL VERIFICATION
# Adversarial verification of the hail-algorithm spec — findings

**Method.** Verified against primary sources directly: Witt et al. (1998) full text (AMS, fetched verbatim), Murillo & Homeyer (2019) full text (PMC8050948, verbatim HTML grep), the 2021 AMS Corrigendum (JAMC-D-20-0271.1, verbatim), Murillo/Homeyer/Allen (2021) Table 1 (PMC8050942), Ortega et al. (2016) full text (AMS, verbatim), Cintineo et al. (2012) full text (AMS), WDTD SHI/POSH pages, Forcadell et al. (2024, AMT), Aregger et al.-type AMT 17:4529 (Foote POH polynomial), and a fresh clone of pyhail (`C:\Users\drew\AppData\Local\Temp\pyhail\src\pyhail\` — `mesh_grid.py`, `mesh_formulas.py`, `hsda.py`, `hsda_mf.py`, `hdr.py`).

**Bottom line.** The load-bearing numerics (Witt Ė/W(Z)/W_T/SHI/POSH/MESH constants, corrigendum coefficients 15.096/0.206 and 22.157/0.212, HSDA membership tables, HDR constants, the analytic unit test) are **correct**. Found **3 wrong constants/claims**, **3 wrong attributions**, and **4 provenance/semantics problems** that should be fixed before implementation.

---

## A. WRONG — fix before implementing

### A1. §2 decision thresholds for *significant* hail (47 mm / 83 mm) — wrong, found in no paper
- The spec claims (attributing Murillo et al. 2021): significant (≥2 in.) peak skill at **MESH₇₅ ≥ 47 mm or MESH₉₅ ≥ 83 mm**. Neither number appears in Murillo & Homeyer 2019 (text says only "Peak skill shifts to higher values for each metric", no numbers) nor in Murillo, Homeyer & Allen 2021.
- **Correct values** (Murillo, Homeyer & Allen 2021, *Mon. Wea. Rev.*, 149(4), 945–958, Table 1, "Max CSI adjusted significant severe", verified verbatim): **MESH₇₅ = 50.55 mm (1.99 in.), MESH₉₅ = 76.71 mm (3.02 in.)**, MESH_Witt = 45.72 mm (1.80 in.).

### A2. §2 decision thresholds for *severe* hail (40 mm / 64 mm) — right numbers, wrong source; conflicts with the cited paper
- 40/64 mm do exist, but in **M&H19 itself** (PMC8050948, section 4 verbatim: "changes in the MESH thresholds at which peak skill was achieved, with an increase from 29 mm to 40 and 64 mm, respectively"), **not** in Murillo et al. 2021.
- Murillo et al. 2021 Table 1 ("Max CSI adjusted severe") prints slightly different values: **MESH₇₅ = 41.91 mm (1.65 in.), MESH₉₅ = 63.25 mm (2.49 in.)**, MESH_Witt = **35.56 mm** (not 29). Pick one source and cite it; do not cite Murillo et al. 2021 for 40/64. Also note that paper is **MWR**, not JAMC.
- Cintineo et al. 2012 (WAF 27, 1235–1248) 29-mm severe threshold: **verified verbatim** ("areas of severe hail (MESH ≥ 29 mm)"; "any hail" = 21 mm). ✓

### A3. §2 rationale "Witt-MESH systematically underestimates big hail (M&H19)" — opposite of what M&H19 says
- M&H19 verbatim: "the original MESH equation resulted in an **underestimate of smaller hail sizes and an overestimate of larger hail sizes** relative to the 75th percentile of the total hail report distribution (Fig. 13)." pyhail's docstring agrees ("tends to overestimate at high SHI"). The MESH₉₅-as-default recommendation can stand on M&H19's bounding argument (and MESH₉₅ > Witt for SHI < ~1846 J m⁻¹ s⁻¹ anyway), but the stated justification must be rewritten.

### A4. §6 HSDA decision rule 1 — "→ unclassified" is wrong
- Ortega et al. 2016 verbatim: "1) if the membership function value for any of the three polarimetric parameters was less than 0.2, **the aggregation value for the associated hail size designation was set to zero**; … **If no designation could be made following the above rules the default designation was small hail.**" It is per-class zeroing with a small-hail fallback, never "unclassified". pyhail `hsda.py:469` implements exactly this (per-class `out = 0`, final fallback lands on class 1 via rule 2).
- The spec also **omits Ortega rule 4**: "a 'despeckle' method along each radial downgraded isolated, single pixels of giant hail to large hail and isolated, single pixels of large hail to small hail." (pyhail omits it too — flag as a known deviation if you skip it.)

### A5. §6 weights/MF attribution — per-layer weights and Z_H-dependent ZDR bounds are Ortega 2016, not Ryzhkov 2013
- Ortega et al. 2016 verbatim lists as **their** modifications: "2) defining some Z_DR membership function bounds as functions of Z_h, 3) adding a tunable ΔZ_DR parameter, and 4) modifying the W vector with different weights for each variable per height interval. The new weights W are listed in Table 1, and the new membership functions are summarized in Table 2." Cite **Ortega et al. 2016 Tables 1–2** as the canonical source of what pyhail `hsda_mf.py` encodes (Ryzhkov et al. 2013 is the algorithm's origin, not the table source).
- Minor wording: the layer-3 g-functions are **shallower**, not "steeper" (g2/g3 slope 0.075 vs f2/f3's 0.1; g1 = −0.9 + 1.5e−2·Z + 5e−4·Z²).

### A6. §1.3 height-datum "verification" — sources do NOT agree on ARL
- Witt 1998 = **ARL** (verbatim: Fig. 2 heights ARL; "H0 (km) is measured ARL" for WT). M&H19 Eq. (3) defines H, H0, H_m20 as **AGL** (verbatim: "H is the height above ground level (AGL) of the radar observation…"). pyhail uses **m ASL** (`alt_vec = grid.z + radar_altitude`, "units m at ASL required for NWP data"). The spec's "Verified: M&H19 Eq. (3), pyhail" for ARL is therefore false as a verification claim.
- The spec's *choice* of ARL is still the right one: W_T only needs a consistent datum, and the POSH warning threshold **requires H0 in km ARL** (Witt verbatim + WDTD POSH page: "Ho (km) is the melting level measured above radar level (ARL)"). Note pyhail feeds an **ASL** melting level into `57.5*(meltlayer/1000) - 121` — a known inconsistency for high-altitude radars; do not replicate.

---

## B. Unverifiable / provenance corrections

### B1. §1.7 POH table — not from Witt 1998 and not "Waldvogel's Table 1"; it is the Foote et al. (2005) curve
- Witt 1998 contains **no numeric POH table**; POH is the Fig. 2 *curve* ("derived from Waldvogel et al. 1979", H45 and H0 both ARL).
- The spec's 11-entry table matches the **Foote, Krauss & Makitov (2005)** third-order polynomial operational at MeteoSwiss — `POH = −1.20231 + 1.00184·ΔH − 0.17018·ΔH² + 0.01086·ΔH³` (ΔH in km; POH=0 below 1.65 km, =1 at 5.8 km; verified in AMT 17, 4529–4552, 2024, which also quotes "4.2 km ⇒ 80%"). Numeric check of the spec's table against this polynomial: deciles reproduced within ±3 percentage points (exact at 80%); the 1.65-km row is the clamp point (polynomial value 3.6%, clamped to 0).
- **Fix:** keep the table (or better, use the polynomial directly + clamps), cite "Waldvogel et al. 1979 via Foote et al. 2005 (16th Conf. on Planned and Inadvertent Weather Modification, AMS)", and drop the claim that this is Witt's/the WSR-88D's table — the operational WSR-88D POH lookup was never published numerically in Witt 1998.

### B2. §5 HDR size relation — constants verified against pyhail, but it is a pyhail digitization, not a printed Depue equation
- pyhail `hdr.py` metadata verbatim: "transform from HDR (dB) to hail size (mm); **function scaled from paper figure**". So `D = 0.0284·HDR² − 0.366·HDR + 11.69` should be cited as "pyhail's fit to Depue et al. (2007) Fig.", not as an equation from the paper. Depue's actual published thresholds (abstract, verified): **HDR ≥ 21 dB → large hail (>19 mm), HDR ≥ 30 dB → structural damage, CSI ≈ 0.77** — the spec's ">~30 dB structurally damaging" claim is correct.
- Side note: pyhail's own docstring credits "Aydin and Zhao 1990 (IEEE TGRS 28, 412–422)" for HDR; the standard original is the spec's **Aydin, Seliga & Balaji 1986, J. Climate Appl. Meteor., 25, 1475–1484** (citation confirmed) — keep the spec's citation.

### B3. §6 Ortega skill numbers — verified, with context fix
- Verbatim abstract: "probability of detection of **0.594**, false-alarm ratio of **0.136**, and resulting critical success index (CSI) equal to **0.543**", vs CSI 0.324 for the operational single-pol HDA; validation vs **>3000 SHAVE reports** (not specifically "severe reports"). Ortega 2016 also used **ΔZ_DR = −0.2 dB** in its evaluation — relevant to the spec's `dzdr` default of 0.

### B4. Report-count nit
- M&H19 = **5954** reports (Table 2 total, verified) — spec's "~5,954" ✓. pyhail's README/comments say "5897" — pyhail is the outlier; don't propagate it.

---

## C. Verified correct (primary-source confirmations worth recording in code comments)

- **Ė = 5×10⁻⁶·10^(0.084Z)·W(Z)**; **W(Z) ramp 40→50 dBZ** (M&H19 Eqs. 2,4 verbatim; pyhail `mesh_grid.py:289-298`; WDTD SHI page "ZL = 40 dBZ and ZU = 50 dBZ"; Forcadell et al. 2024). ✓
- **SHI = 0.1∫_{H0}^{H_T} W_T Ė dH, H_T = storm top.** Witt 1998 verbatim: "where H_T is the height of the top of the storm cell." M&H19's printed Eq. (5) also reads ∫_{H0}^{H_T} (the spec's worry about an H_m20 misreading is moot — the PMC text is correct). pyhail sums the whole column (`shi = 0.1 * np.sum(w_t * hke, axis=0) * d_z`, exactly as the spec quotes). ✓
- **POSH block fully verified from Witt's own text**: "WT (J m⁻¹ s⁻¹) is the warning threshold and H0 (km) is measured ARL. If WT < 20 J m⁻¹ s⁻¹, then WT is set to 20 J m⁻¹ s⁻¹"; "POSH values <0 are set to 0, and POSH values >100 are set to 100… rounded off to the nearest 10%… Note that when SHI = WT, POSH = 50%." (57.5/−121 coefficients corroborated by WDTD + pyhail `mesh_grid.py:338`.) ✓
- **MESH_Witt = 2.54·SHI^0.5, 147 reports, ~75% bounding** — Witt verbatim ("a total of 147 severe hail reports… around 75% of the hail observations would be less than the corresponding predictions"; also rounds output to nearest 6.35 mm/0.25 in. operationally). Note for POSH semantics: Witt's "severe" = **diameter ≥19 mm** (pre-2010 criterion). ✓
- **§2 corrigendum — verified verbatim from JAMC 60(3), p. 423, doi:10.1175/JAMC-D-20-0271.1**: printed-wrong 16.566·SHI^0.181 / 17.270·SHI^0.272; corrected **15.096·SHI^0.206** / **22.157·SHI^0.212**; "All of the analyses presented therein used the correct coefficients"; "Thanks are given to Nathan Wendt from the NOAA Storm Prediction Center." pyhail matches. The blend-pivot claim is verified: pyhail's default `mesh_smooth_blend` pivots at SHI* ≈ 429.3 J m⁻¹ s⁻¹ / ≈52.6 mm (logistic blend Witt→MH75). ✓
- **GridRad 0.5-km vertical below 7 km MSL; hourly RAP 0°C/−20°C** — M&H19 verbatim. ✓
- **HSDA**: classes <25 / 25–50 / >50 mm ✓; six layers bounded by **wet-bulb** 0°C and −25°C ✓ (Ortega verbatim + pyhail `hsda.py:317-328` exactly as the spec's layer table); aggregation A = Σwᵢqᵢ·MFᵢ/Σwᵢqᵢ ✓; rules 2 (max A < 0.6 → small) and 3 (ZDR ≥ 2 dB → small) ✓ verbatim; runs only on HCA "rain/hail" pixels ✓ verbatim; f1/f2/f3 and all quoted MF breakpoints match pyhail `hsda_mf.py` exactly ✓; per-layer weights [1.0,0.3,0.6]/[0.8,0.5,0.6]/[0.7,0.8,0.6]/[0.7,1.0,0.6] confirmed in pyhail (canonical source = Ortega 2016 Table 1). pyhail also **smooths Z/ZDR/ρhv with a 5-gate radial running mean** before classification — worth adding to the spec.
- **HDR piecewise f(ZDR)** (27 / 19·ZDR+27 / 60 at 1.74 dB) — matches pyhail `hdr.py:154-157` exactly. ✓
- **§8 arithmetic** all recomputed and correct: Ė(55)=0.2084, SHI=114.6, MESH_Witt=27.2 mm, MESH₉₅=60.5 mm, WT(3 km)=51.5, POSH=73.2%→70%. ✓
- Citations checked good: Witt WAF 13, 286–303 + DOI ✓; M&H19 JAMC 58, 947–970 ✓; Waldvogel et al. 1979 JAM 18, 1521–1525 ("Criteria for the detection of hail cells") ✓; Ryzhkov et al. 2013 JAMC 52(12), 2871– ✓; Ortega et al. 2016 JAMC 55(4), 829–848 ✓; Park et al. 2009 WAF 24(3), 730–748 ✓; Smith et al. 2016 BAMS 97(9), 1617–1630 ✓; Forcadell et al. 2024 AMT 17, 6707–6734 ("Severe-hail detection with C-band dual-polarisation radars using convolutional neural networks") — restates SHI = 0.1∫_{H0}^{H_T} ✓; Aydin et al. 1986 JCAM 25, 1475–1484 ✓; pyhail paths `src/pyhail/{mesh_grid,mesh_formulas,hsda,hsda_mf,hdr}.py` all exist ✓.

## D. Caveats for the §8 validation plan (pyhail cross-check will NOT match naively)

1. **pyhail has no WT floor**: `warning_threshold = 57.5*(meltlayer/1000) - 121` with no `max(…, 20)`, and no round-to-10. POSH will diverge from the spec's (correct, Witt-faithful) implementation at low melting levels.
2. **pyhail heights are ASL**, the spec's are ARL — feed pyhail `levels` consistently when cross-checking SHI (W_T cancels the datum only if both profile heights and levels use the same one).
3. pyhail applies a **C-band reflectivity correction** (`dbz*1.113 − 3.929`, Brook et al. 2023) when `radar_band="C"` — pass `"S"` for NEXRAD; it also speckle-filters SHI and masks within 10 km of the radar by default.
4. pyhail clips dBZ to ±100 (spec's claim ✓, `mesh_grid.py:294-295`).

## Files
- pyhail reference clone: `C:\Users\drew\AppData\Local\Temp\pyhail\src\pyhail\` (mesh_grid.py, mesh_formulas.py, hsda.py, hsda_mf.py, hdr.py)
- Verbatim source dumps: `C:\Users\drew\AppData\Local\Temp\{mh19,corr,witt98,ortega16,cintineo12}.html`
- Spec implementation target (unchanged): `C:\Users\drew\radar-work\radar-rs-analyst\crates\render2d\src\volumetric.rs`


## WIND VERIFICATION
# Adversarial Verification of the Damaging-Wind Algorithm Spec

Verified 2026-06-10 against primary full texts (local PDFs: `C:\Users\drew\AppData\Local\Temp\ddpda.pdf`, `kuster.pdf`, `wgr.pdf`, `krupar.pdf`; freshly fetched: NWS SR-163 memo, NASA AMU CR-201354, AWOC FY10 QLCS lesson — text extractions saved alongside) plus NWS LMK operational pages and AMS records.

**Bottom line: the spec is substantially correct.** Every Rust constant in §2.1–§2.3 traces to its claimed source verbatim. I found **1 outright factual error** (Krupar gust-ratio attribution), **2 misattributions/omissions** (MDPI citation + missing 650–500-mb layer; WINDEX zero-floor convention presented as published), and **5 nuances** that should be corrected before the constants ship in doc-comments.

---

## A. WRONG — must fix

### A1. Krupar et al. (2016) "0.67–0.86 × VAD 0–500-m" gust ratio — WRONG attribution (spec §1.2 item 5, §3)
Spec claims: *"ASOS 10-m gust ≈ 0.67–0.86 × the VAD 0–500-m mean boundary-layer wind (site-dependent)."* The paper (full text verified, `krupar_text.txt`) defines **two different WSRs**:
1. **Mean WSR = ASOS 10-m standardized MEAN wind / VAD 0–200-m mean wind** — this is the site-dependent ratio whose means range **0.67–0.86** (Table 5; e.g., KBRO 0.67, KMOB 0.86).
2. **Gust WSR = ASOS 10-m 3-s gust / VAD 0–500-m MBL wind** — deliberately **not site-segregated**; logistic-PDF **mean 0.7310, σ 0.0692**. Best gust predictor is a **non-site-specific linear regression on the 0–500-m MBL wind (slope ≈ 0.8651, nonzero intercept)**, 1.07% more accurate than the single gust WSR. Only **2.15%** of gusts exceeded the 0–500-m MBL wind.

The spec spliced ratio (1)'s numbers onto ratio (2)'s definition. **Correction:** "10-m mean wind ≈ 0.67–0.86 × VAD 0–200-m mean (site-dependent); 10-m gust ≈ 0.73 × VAD 0–500-m MBL (single logistic mean) or regression slope ≈ 0.87 with nonzero intercept." The "best 10-m mean predictor = site-specific regression on VAD 0–200-m" sentence is correct. (TC-only scoping is correct.)

### A2. MDPI — misattributed citation and missing layer bound (spec §1.4, §2.2)
- **The MDPI formula is not in NWS SR Tech. Memo 163.** SR-163 (Wheeler & Spratt, *Forecasting the Potential for Central Florida Microbursts*, full text verified) presents only the Atkins & Wakimoto Δθe ≥ 20 K / ≤ 13 K approach — the strings "MDPI" and "CT" never appear. The MDPI definition is in the **AMU report: NASA Contractor Report CR-201354** (Wheeler, AMU/ENSCO, ~1996, *Verification and Implementation of MDPI and WINDEX Forecasting Tools at Cape Canaveral Air Station*), §3.1.
- **The spec's "min θe aloft" omits the published layer:** CR-201354 defines **MDPI = (max θe in the lowest 150 mb − min θe between 650 and 500 mb) / CT, CT = 30 K** (locally tuned; 150-mb layer per Roeder 1995, higher CT per Wheeler 1995). An implementer following the spec as written would search the whole column for the minimum — wrong on soundings with multiple dry layers. Also note the operational rule: thunderstorm probability ≥ 60% AND MDPI ≥ 1 → >90% probability of wet microbursts at KSC/CCAS.

### A3. WINDEX "negative radicand ⇒ WI = 0" — presented as published, actually a convention (spec §1.4)
The formula, units (kt), and **R_Q = Q_L/12 capped at 1** verify against McCann (1994, WAF 9, 532–541) secondary records. But the zero-floor for a negative radicand appears in neither the search-accessible McCann material nor CR-201354. It is the standard implementation convention — re-tag it **[ENG]**, consistent with the spec's own flagging discipline.

---

## B. Nuances / soft corrections

1. **Schmocker (1996) threshold is published as a 25–30 m/s band, not a bare 25:** the SLU/COMET research summary states *"Magnitudes of the MARC velocity differential of 25–30 m s⁻¹ or greater proved to be a reliable precursor of severe wind gusts of 25 m s⁻¹ or greater."* `MARC_DELTAV_WARN = 25.0` is defensible (LMK operational page: "over 50 kts; 25 m/s"), but the doc-comment should say "25–30 m/s band; 25 = floor." Also "(50 kt)" ≈ 25.7 m/s ≠ 25.0 — the LMK page itself conflates; pick one and note it.
2. **The MARC viewing-angle "verified quote" is real but mis-located:** *"MARC is limited when velocities within mid-level convergence zones are oriented normal to the radar beam and thus greatly underestimated or masked"* is verbatim from **Funk/DeWald/Lin (weather.gov/lmk/paper-51495), Section 6** — it is NOT on the squallbow page. Cite the case paper.
3. **DDPDA lead times:** 5.5 / 0 min are **medians of MEAN lead times across 100 resamples**, and the scoring method **caps maximum lead at 15 min** ("Given the short-lived nature... the maximum lead time is 15 min"). Worth a doc-comment so the UI doesn't over-promise.
4. **Ibrahim et al. (2023) method name:** the paper describes "a variant of the VAD method" using "an image processing method to partition scans into regions, representing events and the background flows" — "Partitioned-VAD" is a reasonable paraphrase but not the paper's formal name. Bonus verified nuance: distribution agreement is good **within 4 km** of the radar; 4–10-km stations show retrieved velocities biased **high** vs ASOS.
5. **AWOC mesovortex companion numbers are single-case:** the FY10 QLCS lesson's mean Vr ≈ **24 kt (12 m/s)** for tornadic mesovortices is from the **10 June 2003 case only** ("There needs to be more work...the take home message is that the tornadic mesovortices are much stronger at low-levels"); azshear "**>10×10⁻³ s⁻¹**" ("right in line with any significant supercell mesocyclone") and "**only 2 km (1.2 nm)**" diameter are from one example mesovortex (also: strongest Vr often 1–2 km above the lowest scan; mesovortices shallow, <1.5 km AGL per Atkins et al. 2005). All three numbers verify verbatim from the lesson PDF, but encode the single-case caveat.

---

## C. VERIFIED CORRECT (checked against primary text, verbatim where quoted)

**MARC (NWS LMK squallbow + paper-51495 + Schmocker via SLU/COMET summary + Funk reference list):**
- ΔV > 50 kt (25 m/s), "persistent, deep-layered", ~3–7 km, lead "up to 15–20 minutes" (LMK); "25 m/s (50 kt) or more at 3–7 km preceded onset by up to 20 min" (Schmocker). ✓
- ΔV = max inbound − max outbound **within 6 km along a radial**, computed on **SRM** data (Funk). ✓
- 14 May 1995 case: max ≈ 38 m/s at 4.5–5.5 km; convergence depth 6–8 km; MARC width 1–3 km; initial signal 30–35 min before bow/initial damage; peak MARC 10–20 min before damage onset and 30–40 min before max damage. ✓ (`MARC_DELTAV_EXTREME = 38.0` ✓)
- Citation "15th Conf. WAF, Norfolk, AMS, **306–311**" ✓ (Funk et al. reference list, exact).
- Przybylinski 1995, *WAF* **10**, 203–218 ✓.

**DDPDA — Smith, Elmore & Dulin 2004, WAF 19, 240–250 (full text, 11 pp):** every constant verbatim —
- Scope: ≤80 km, bands 20–45/45–80 km (Table 1: 50+41 events, 492+755 nulls), no equations <20 km (mid/upper storm unsampled), sfc–500-mb shear 0–15 m/s, moderate-high CAPE, 0.95° beam/250-m gates ✓.
- LLSD: 3×3 median filter (3 gates × 3 azimuths) → 5×5 LS template, u_r = Σ(i·u_ij)/(50·Δr), u_s = Σ(j·u_ij)/(50·r₀·Δφ), i,j ∈ {−2..2}, Δφ = beamwidth in meters, results median-filtered again ✓ (and 50 = Σi² over the 5×5 checks arithmetically).
- Table 2: CONV006/004/002/001 (cross-sectional area, 1–6 km MSL), DPTHC (>0.004 s⁻¹), DPTHDV (>10 m/s), C16/DV16 (1–6 km MSL), CNVMELT/DVMELT (0 °C), CTHTE/DVTHTE (min θe), MAXR17/MINR17 (1–7 km MSL), VIL, MASSHT, ASP (depth/width), MAXDBZ, DBZHT, DBZp7KM, ZTHTE/ZATHTE, VOL, SHI; 5-km radius of centroid ✓ all verbatim.
- Detection: u_r > 0.0008 s⁻¹ default, ≥1.5 km segments, within 5 km of centroid, beam centerline < 1 km above radar pedestal; **SEVERE: ARWS ≥ 25 or ΔV ≥ 40 m/s; MODERATE (explicitly "intended for aviation use"): ARWS ≥ 18 or ΔV ≥ 25 m/s** ✓ verbatim.
- Severe-cell criteria (the "1:1 gust equivalence"): measured 50-kt (~26 m/s) gust, damage, or radar wind 25 m/s / ΔV > 40 m/s within 1 km of surface ✓; Hjelmfelt ≥50% underestimate cited exactly as spec quotes ✓.
- Skill: median HSS 0.40 / 0.17; leads 5.5 / 0 min ✓ (see B3). First-echo→outflow ~15 min ✓. NWS severe = 26 m/s (50 kt) ✓.
- Predictor rankings: 20–45 km → VIL, SHI, MASSHT, ASP, with CONV006 the dominant velocity parameter; 45–80 km → SHI, VIL, ASP ✓ verbatim. LDA caveats (65/35 resampling ×100, SCIT errors removed 24% of cells, time-height trend display Fig. 8c) ✓ — "don't port coefficients" advice is sound.
- Eilts et al. 1996a citation (18th SLS, San Francisco) ✓ from DDPDA refs; "85 damaging downbursts... rapidly descending reflectivity core, strong and deep convergence at midaltitudes (2–6 km AGL), reflectivity core that initially begins higher" ✓ verbatim.

**Wilson et al. 1984, JCAM 23(6), 898–915:** microburst onset = differential velocity ≥ 10 m/s, max inbound to max outbound **within 4 km** ✓. Bonus published constants: median ΔV 22 m/s over avg 3.1 km; max-ΔV height ~75 m; median 5 min from initial divergence to max ΔV.

**Roberts & Wilson 1989** (*A Proposed Microburst Nowcasting Procedure Using Single-Doppler Radar*), JAM 28(4), 285–303: 31 Colorado storms; precursors (descending core, increasing in-cloud convergence, rotation, notch) typically **2–6 min** before outflow ✓.

**Hjelmfelt 1988, JAM 27(8), 900–927:** outflow nose profile peaking **50–100 m AGL** (max ≈ 75 m) ✓.

**Kuster et al. 2021, WAF 36(4), 1183–1198, WAF-D-21-0005.1 (full text):** KDP core = **KDP ≥ 1.0° km⁻¹ near or within 3 km below the environmental melting layer** ✓ verbatim; developed **≤31 min (mean 15 min)** before downburst ✓; downburst = 0.5° divergent ΔV ≥ 10 m/s ✓; 81 downbursts, 10 states, 24 days ✓; **75% temporal peak** in core max-KDP before max intensity ✓; strong ≥ 25.7 m/s (50 kt)/damage vs weak ≤ 15.4 m/s (30 kt) ✓; larger KDP + larger vertical KDP gradient below ML favor strong ✓. (Only 2 null KDP cores in 24 days — strong base-rate caveat for FAR.)

**Sherburn, Bunkers & Mose 2021, WAF 36, WAF-D-20-0221.1 (full text, DOI on PDF):** n = 943 CONUS; **median 1.44, mean 1.68, 25th 1.19, 75th 1.91** ✓ verbatim; lower for linear/QLCS modes, higher with steeper low-level lapse rates/momentum-transfer thermodynamics, WGR and its IQR decrease with boundary speed ✓; paper itself frames boundary speed as a "floor" ✓ (supports the spec's gust-floor framing).

**Ibrahim, Kopp & Sills 2023, JTECH 40(2), JTECH-D-22-0028.1:** >2,600 events, 19 radar–ASOS pairs < 10 km, **MAE 1.5–4.5 m/s increasing with height**, exponential mean-error correction improving high-velocity tails ✓ (see B4 for naming).

**McCann 1994, WAF 9(4), 532–541:** WI = 5[H_M·R_Q·(Γ² − 30 + Q_L − 2Q_M)]^0.5 in **knots**; H_M melting-level height (km), Γ sfc→melting lapse (°C/km), Q_L lowest-1-km mean mixing ratio, Q_M melting-level mixing ratio, **R_Q = Q_L/12, not > 1** ✓ (see A3 for the zero-floor).

**Atkins & Wakimoto 1991, WAF 6(4), 470–482 (via SR-163 full text):** all 5 MIST wet-microburst days Δθe (surface max → min aloft) **≥ 20 K**; all 3 null thunderstorm days **≤ 13 K**; ≥20 ⇒ high potential, <13 ⇒ unlikely ✓ verbatim.

**AWOC Severe Track FY10 QLCS lesson (full PDF):** mean Vr tornadic mesovortices ≈ 24 kt (12 m/s); azshear >10×10⁻³ s⁻¹; ~2-km diameter ✓ (see B5 for single-case caveat).

---

## D. Unverifiable (could not be confirmed this session)
- **Exact DOI string for Smith et al. 2004** (`<0240:ADDPAD>`): consistent with AMS legacy format and the title, but I could not fetch the AMS landing page (403). Low risk.
- **WINDEX zero-floor** (see A3) — convention, not located in a primary text.
- The claim that NWS guidance "deliberately avoids a fixed percentage" for low-level velocity → gust: consistent with everything fetched (no percentage anywhere on LMK pages), but it's a negative claim — phrase as "no fixed percentage found in NWS operational guidance."

## E. Net recommended edits to the spec
1. Rewrite §1.2 item 5 (Krupar) per A1 — this is the only numerically wrong constant.
2. §1.4/§2.2 MDPI: cite **NASA CR-201354 (AMU, Wheeler 1996)** for the formula; keep SR-163 for the Δθe thresholds; add "min θe between **650–500 mb**" to the formula and the HRRR-chip implementation.
3. Re-tag WINDEX zero-floor as [ENG].
4. MARC doc-comment: "Schmocker et al.: 25–30 m/s band; 25 m/s floor = LMK operational threshold"; move the viewing-angle quote citation to Funk et al. (paper-51495, §6).
5. Add DDPDA footnote: lead times are medians of means, capped at 15 min by the scoring method.
6. Add single-case caveat to the AWOC mesovortex companion-product numbers.

Sources: [NWS LMK squallbow](https://www.weather.gov/lmk/squallbow), [Funk/DeWald/Lin 14 May 1995 case](https://www.weather.gov/lmk/paper-51495), [SLU/COMET MARC research summary](https://www.comet.ucar.edu/sites/default/files/outreach/media/documents/9671862.htm), [Wilson et al. 1984 (AMS)](https://journals.ametsoc.org/view/journals/apme/23/6/1520-0450_1984_023_0898_mwsaeo_2_0_co_2.xml), [Roberts & Wilson 1989 (AMS)](https://journals.ametsoc.org/view/journals/apme/28/4/1520-0450_1989_028_0285_apmnpu_2_0_co_2.xml), [Hjelmfelt 1988 (AMS)](https://journals.ametsoc.org/view/journals/apme/27/8/1520-0450_1988_027_0900_salcom_2_0_co_2.xml), [Ibrahim et al. 2023 (AMS)](https://journals.ametsoc.org/view/journals/atot/40/2/JTECH-D-22-0028.1.xml), [Kuster et al. 2021 (AMS)](https://journals.ametsoc.org/view/journals/wefo/36/4/WAF-D-21-0005.1.xml), [McCann 1994 (AMS)](https://journals.ametsoc.org/view/journals/wefo/9/4/1520-0434_1994_009_0532_wniffm_2_0_co_2.xml), [Atkins & Wakimoto 1991 (AMS)](https://journals.ametsoc.org/view/journals/wefo/6/4/1520-0434_1991_006_0470_wmaots_2_0_co_2.xml), [NWS SR-163 (Wheeler & Spratt)](https://www.weather.gov/media/mlb/research/SR_TechMemo163.pdf), [AMU CR-201354 MDPI/WINDEX report](https://kscweather.ksc.nasa.gov/amu/files/final-reports/mdpi-windex.pdf), [AWOC Severe FY10 QLCS lesson](https://training.weather.gov/wdtd/courses/woc/documentation/severe/qlcs.pdf). Local full texts: `ddpda.pdf`, `kuster.pdf`, `wgr.pdf`, `krupar.pdf` in `C:\Users\drew\AppData\Local\Temp\` (extractions `*_text.txt` same dir); `sr163_text.txt`, `amu_mdpi_text.txt`, `awoc_qlcs_text.txt` in `C:\Users\drew\.claude\projects\C--Users-drew\2c09b16f-3f0d-478d-b6dc-78d5612dd641\tool-results\`.