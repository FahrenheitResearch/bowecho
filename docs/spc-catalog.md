# SPC Mesoscale Analysis product catalog → OA composites mapping

Phase 2 of the SPC-mesoanalysis arc (`oa_derived.rs`). This document
enumerates the **actual product menu** of the SPC Mesoscale Analysis page
(https://www.spc.noaa.gov/exper/mesoanalysis/ — menu enumerated 2026-06-11
from the sector viewer, `new/viewsector.php`) and classifies every product
against what our OA composite pass can produce.

Per-product SPC formulation pages follow the pattern
`https://www.spc.noaa.gov/exper/mesoanalysis/help/help_<slug>.html`
(e.g. `help_dcp.html`); the parameter primer is
`https://www.spc.noaa.gov/exper/mesoanalysis/help/begin.html`.

## Inputs available to the OA pass

The composite pass (`oa_derived::composite_pass`) builds, per strided grid
cell, a full `sharprs::profile::Profile` from the model store variables —
`temperature_2m`, `dewpoint_2m`, `u_10m`, `v_10m`, `surface_pressure`,
`orography`, plus the isobaric `temperature_iso` / `dewpoint_iso` /
`height_iso` / `u_iso` / `v_iso` volumes — with the surface first corrected
by the Bratseth objective analysis of live obs (`mesoanalysis.rs`,
"analyze, then derive"; Bothwell, Hart & Thompson 2002). One
`compute_all_params` call per cell then yields the SHARPpy-faithful
parameter suite.

**Not in the inputs**: omega/vertical velocity, soil fields, precipitation
fields, simulated reflectivity, snow fields, cloud fields, surface visibility,
3-hourly history (the pass sees one analysis time only).

## Classification

- **(a)** already exposed in our suite before this branch (14 fields)
- **(b)** already returned by `sharprs::render::compositor::ComputedParams`
  (or `ParcelResult`) — exposing it is free → **implemented on this branch**
- **(c)** derivable from the per-cell profile / OA grids but needs new
  computation — implemented only where the formulation is published and the
  cost is trivial (marked ✔); the rest are documented honestly below
- **(d)** not derivable from the store variables — reason given

---

## Basic Surface

| SPC product | Class | Notes |
|---|---|---|
| MSL Pressure/Wind | (d) | display product (contours + barbs), not a scalar layer; `mslp` exists in the store but the OA layer machinery renders scalar fills |
| Temp / Wind / Dwpt | (a) | the Bratseth OA layers themselves (T2m/Td2m/10 m wind) |
| MSL Pressure/Theta-E/Wind | (b/c) ✔ | scalar part = surface theta-e (Bolton 1980, Eq. 39) — exposed as "Sfc theta-e" |
| Moisture Convergence | (c) | needs horizontal divergence of q·V from the OA u10/v10/Td grids (finite differences with grid spacing); cheap math but a *spatial-derivative* lane the per-cell pass doesn't have — deferred (Banacos & Schultz 2005, WAF 20, 351–366) |
| Theta-E Advection | (c) | same spatial-derivative lane (−V·∇θe) — deferred |
| Mixing Ratio / Theta | (c) | trivial per-cell from psfc/T/Td; low value as a fill — deferred |
| Instantaneous Contraction Rate / Fluid Trapping / Velocity Tensor Magnitude | (c) | kinematic tensor fields of the surface wind (Cohen & Schultz 2005, MWR 133, 1353–1369); spatial-derivative lane — deferred |
| Divergence and Vorticity (sfc) | (c) | spatial-derivative lane — deferred |
| Deformation / Axes of Dilatation | (c) | spatial-derivative lane — deferred |
| 2-h Pressure / 3-h Temp / Dwpt / Mixing-ratio / Theta-E Change | (d) | needs the previous analysis kept in memory; the pass is single-time. Future: cache last composite run and difference |

## Basic Upper Air

| SPC product | Class | Notes |
|---|---|---|
| 925/850/700/500/300 mb Analyses | (d)* | multi-field chart products (height contours + barbs + fills). The scalar ingredients (T/Td/wind at levels) are plain store planes, not composite-suite material |
| Temp Advection (925/850/700) | (c) | spatial-derivative lane — deferred (rustwx-products has `temperature_advection_850mb`/`700mb` recipes for the model lane already) |
| Frontogenesis (sfc…700-500mb) | (c) | Petterssen 1936 2D frontogenesis needs ∇T and deformation of the level wind — spatial-derivative lane, deferred |
| Deep Moist Convergence, Diff. Vorticity Advection, Pot. Vorticity Advection, Diff. Divergence, Jet Circulation | (c/d) | spatial-derivative (some need ω or multiple levels of derivatives) — deferred |
| 12-h 500 mb Height Change | (d) | needs history |
| Fluid Trapping (500/250 mb) | (c) | spatial-derivative lane — deferred |

## Thermodynamics

| SPC product | Class | Notes |
|---|---|---|
| CAPE — Surface-Based | (b) ✔ | `sfcpcl.bplus` (+ SBCIN `sfcpcl.bminus`); virtual-temperature parcel math per Doswell & Rasmussen 1994 (WAF 9, 625–629) |
| CAPE — 100 mb Mixed-Layer | (b) ✔ | `mlpcl.bplus` / MLCIN `mlpcl.bminus` |
| CAPE — Most-Unstable / LPL Height | (a)/(b) ✔ | MUCAPE was exposed; MUCIN `mupcl.bminus` added |
| EL Temp / MUCAPE / MUCIN | (b) ✔ partial | MU EL height `mupcl.elhght` exposed; EL temp itself skipped (chart product) |
| CAPE — Normalized | (c) ✔ | NCAPE = MUCAPE / (EL − LFC depth); Blanchard 1998 (WAF 13, 870–877) |
| CAPE — Downdraft | (b) ✔ | `dcape` (SPC/John Hart algorithm: min 100-mb-mean θe in lowest 400 mb lowered moist-adiabatically) |
| Surface-Based Lifted Index | (b) ✔ | `sfcpcl.li5` (Galway 1956, BAMS 37, 528–529) |
| Mid-Level Lapse Rates (700–500) | (b) ✔ | `lr75` |
| Low-Level Lapse Rates (0–3 km) | (b) ✔ | `lr03` (+ 850–500 `lr85`, 3–6 km `lr36` also exposed) |
| Max 2–6 km AGL Lapse Rate | (c) | `sharprs::params::indices::max_lapse_rate` exists; deferred (adds another scan per cell, marginal value vs lr36) |
| LCL Height | (b) ✔ | `mlpcl.lclhght` (SPC plots the ML parcel LCL); SB LCL `sfcpcl.lclhght` also exposed |
| LFC Height | (b) ✔ | `mlpcl.lfchght` (Davies 2004, WAF 19, 714–726 motivates LFC for tornado environments) |
| LCL-LFC Mean RH | (c) ✔ | `indices::mean_relh` over the ML parcel LCL→LFC pressure span |
| 3/6-h CAPE/CIN/LR Changes | (d) | needs history |
| Skew-T Maps | n/a | the native sounding window already covers point soundings |

## Wind Shear

| SPC product | Class | Notes |
|---|---|---|
| Bulk Shear — Effective | (a) | `effective_bwd` (Thompson, Mead & Edwards 2007, WAF 22, 102–115) |
| Bulk Shear — Sfc-1/3/6/8 km | (b) ✔ | `shr01/03/06/08` magnitudes (kt) |
| BRN Shear | (c) ✔ | ½·|V̄(0–6 km) − V̄(0–500 m)|² (Weisman & Klemp 1982, MWR 110, 504–520; Stensrud et al. 1997 interpretation) |
| SR Helicity — Effective | (a) | `effective_srh` (Thompson et al. 2007) |
| SR Helicity — Sfc-500 m | (c) ✔ | `winds::helicity(0, 500 m)` w/ Bunkers RM motion (Coffer et al. 2019, WAF 34, 1417–1435) |
| SR Helicity — Sfc-1/3 km | (a) | `srh01`, `srh03` (Davies-Jones, Burgess & Foster 1990) |
| SR Wind — Sfc-2 km / 4-6 km / 9-11 km / Anvil | (c) | `winds::sr_wind` exists; deferred (storm-relative wind layers are niche w/o the vector display) |
| 850-300 mb Mean Wind | (c) | one `mean_wind` call; deferred (value mostly as a chart) |
| 850 and 500 mb Winds | (d)* | chart product; raw planes exist in the store |
| 3-h Shear/SRH Changes | (d) | needs history |
| Hodograph Map | n/a | covered by the sounding window |
| *(extra)* 0-6 km Mean Wind | (c) ✔ | `mean_wind_06` magnitude — exposed because DCP consumes it and analysts sanity-check the term |
| *(extra)* Bunkers RM/LM storm motion speed | (b) ✔ | `rstu/rstv`, `lstu/lstv` magnitudes (Bunkers et al. 2000, WAF 15, 61–79) |
| *(extra)* Effective inflow base | (b/c) ✔ | `eff_inflow.0` pressure → height AGL via the per-cell profile (Thompson et al. 2007) |
| *(extra)* Critical Angle | (b) ✔ | `critical_angle` (Esterheld & Giuliano 2008, EJSSM 3(2)) — SPC lists it under Composite Indices |

## Composite Indices

| SPC product | Class | Notes |
|---|---|---|
| Supercell Composite | (a) | `scp` (Thompson et al. 2003, WAF 18, 1243–1261; effective-layer update Thompson et al. 2007) |
| Supercell Composite (left-moving) | (c) | needs SRH recomputed with LM motion per cell; cheap but niche — deferred |
| Sgfnt Tornado (fixed layer) | (a) | `stp_fixed` (Thompson et al. 2003) |
| Sgfnt Tornado (effective layer) | (a) | `stp_cin` (Thompson, Smith, Grams, Dean & Broyles 2012, WAF 27, 1136–1154) |
| Sgfnt Tornado (0-500 m SRH) | (c) | STP-500 (Coffer et al. 2019); we expose SRH-500 itself, the STP variant's exact term normalizations were not ported into sharprs — deferred |
| Cond. Prob. Sigtor (Eqn 1/2) | (d) | logistic-regression coefficients from SPC's SSCRAM lineage (Hart & Cohen 2016, WAF 31, 1697–1714) are not published in a portable form |
| Non-Supercell Tornado | (c) | needs surface vorticity (spatial-derivative lane) × 0-3 km MLCAPE terms (Baumgardt & Cook 2006) — deferred; the thermo half (0-3 km MLCAPE) **is** exposed |
| Violent Tornado Parameter | (b) ✔ | `vtp_mod` (Hampshire et al. 2018, J. Operational Meteor. 6, 1–12) |
| Sgfnt Hail (SHIP) | (a) | `ship` (SPC-developed, no formal paper; help_sigh.html) |
| SARS Hail Size / Sig. Hail % | (d) | needs the SARS sounding-analog database (Jewell & Brimelow 2009) |
| Large Hail Parameter | (c) | Johnson & Sugden 2014 (EJSSM 9(5)); `indices::lhp` exists in sharprs but needs storm-relative-wind terms wired per cell — deferred |
| Derecho Composite | (c) ✔ | `composites::dcp` (Evans & Doswell 2001, WAF 16, 329–342; formula per help_dcp.html) |
| Craven/Brooks Sgfnt Severe | (c) ✔ | MLCAPE × 0-6 km shear (m/s) (Craven & Brooks 2004, Natl. Wea. Digest 28, 13–24) |
| Bulk Richardson Number | (c) ✔ | MLCAPE / BRN-shear (Weisman & Klemp 1982) |
| MCS Maintenance | (c) ✔ | `composites::mmp` logistic regression (Coniglio, Stensrud & Wicker 2006/2007; per-cell max 0-1→6-10 km bulk shear, 3-8 km LR, 3-12 km mean wind) |
| Microburst Composite | (c) | `composites::mburst` exists, but its lowest-3-km θe-difference term needs a per-cell θe scan not yet wired — deferred |
| Enhanced Stretching Potential | (c) ✔ | `composites::esp` (J. Davies; help_esp.html) |
| EHI Sfc-1/3 km | (a) | `ehi01/03` (Hart & Korotky 1991; Rasmussen & Blanchard 1998, WAF 13, 1148–1164) |
| VGP Sfc-3 km | (c) | Rasmussen & Blanchard 1998; needs hodograph-length mean shear per cell — deferred |
| Critical Angle | (b) ✔ | see Wind Shear above |

## Multi-Parameter Fields

All are overlay charts of fields classified above (e.g. "MLCAPE / Eff Bulk
Shear", "Sfc Vorticity / 0-3 km MLCAPE"). The scalar ingredients we can
produce are exposed individually; the combined chart styling is a display
concern, class (d) as charts. "Sfc-3km MLCAPE" ingredient = `mlpcl.b3km` ✔
(exposed as "MLCAPE 0-3 km", per Rasmussen 2003 low-level buoyancy work).

## Heavy Rain

| SPC product | Class | Notes |
|---|---|---|
| Precipitable Water | (a) | `precip_water` |
| PW w/ 850 mb Moisture Transport Vector | (d)* | vector chart |
| 850/925/925-850 mb Moisture Transport | (c) | q·|V| at level — cheap per cell; deferred (fill-only value is marginal without vectors) |
| Upwind Propagation Vector | (b) ✔ partial | Corfidi vectors (Corfidi 2003, WAF 18, 997–1017) — upshear **speed** exposed; the vector display is deferred |
| Precipitation Potential Placement | (d) | SPC-internal blend (PW × corfidi × …), formulation not published |
| 100 mb Mean Mixing Ratio | (b) ✔ | `mean_mixr` |
| *(extra)* Mean RH (sfc-based / mid-level) | (b) ✔ | `mean_rh_low`, `mean_rh_mid` |
| *(extra)* Theta-E Index (TEI) | (b) ✔ | `tei` = max−min θe in the low levels (SPC heavy-rain/downburst diagnostic) |

## Winter Weather

| SPC product | Class | Notes |
|---|---|---|
| Precipitation Type | (d) | needs precip occurrence + partial thickness logic tuned to model p-type fields we don't ingest |
| Near-Freezing Surface Temp | (a)* | plain T2m (OA layer) at 32°F — display styling, not a new field |
| Surface Wet-Bulb Temp | (c) | per-cell wet-bulb from psfc/T/Td (`thermo::wetbulb`); deferred (rustwx-products already has a `wetbulb_2m` model lane) |
| Freezing Level | (a) | `frz_lvl` |
| Wet-Bulb Zero Height | (b) ✔ | `wb_zero` (hail-size discriminator; Miller 1972) |
| Critical Thicknesses, EPVg, Lake Effect, Snow Squall, DGZ depth/RH, Max Wet Bulb | (c/d) | DGZ depth/RH derivable (`indices::dgz`); Snow Squall Parameter (Banacos, Loconto & DeVoir 2014, J. Operational Meteor. 2, 130–151) needs 0-2 km mean RH + θe delta + wind — both deferred as winter is out of scope for the severe suite v1; EPVg needs spatial derivatives; lake-effect needs lake temps (d) |

## Fire Weather

| SPC product | Class | Notes |
|---|---|---|
| Sfc RH / Temp / Wind | (a)* | OA surface layers (RH = trivial from T/Td) |
| Fosberg Index | (c) ✔ | `sharprs::fire::fosberg` from the OA-corrected surface (Fosberg 1978, AMS Conf. Sierra Nevada Meteorology) |
| LCL-LFC Mean RH (fire wx) | (c) ✔ | same field as Thermodynamics row |
| *(not listed but adjacent)* Haines Index | (c) | `fire::haines` exists (Haines 1988, Natl. Wea. Digest 13, 23–27); deferred — elevation-variant selection per cell needs care |

## Classic

| SPC product | Class | Notes |
|---|---|---|
| Total Totals | (b) ✔ | `t_totals` (Miller 1972, AWS TR-200) |
| K-Index | (a) | `k_index` (George 1960, *Weather Forecasting for Aeronautics*) |
| Showalter Index | (d) | not computed by sharprs (no `showalter` in params); portable formula exists (Showalter 1953, BAMS 34, 250–252) — could be added to sharprs upstream |

## Beta

| SPC product | Class | Notes |
|---|---|---|
| SHERBE | (c) ✔ | `composites::sherb(effective)` (Sherburn & Parker 2014, WAF 29, 854–877) + SHERBS3 fixed-layer variant ✔ |
| Modified SHERBE | (c) ✔ | `composites::moshe` (per sharprs port; Sherburn-lineage HSLC research) |
| CWASP | (d) | Craven-Wiedenfeld Aggregate Severe Parameter — 28-term aggregate, coefficients not portably published |
| Tornadic 0-1 km EHI | (b) ✔ | `tehi` |
| Tornadic Tilting & Stretching | (b) ✔ | `tts` |
| OPRH | (d) | formulation not publicly documented |
| Prob EF0+/EF2+/EF4+ (cond. on RM supercell) | (d) | SSCRAM-lineage regression coefficients not published |
| PW * 3 km RH | (c) | trivial product of two exposed fields — deferred (compose visually instead) |

---

## Net result on this branch

The suite grows from 14 → 64 cached fields, organized in the picker by the
SPC section names above (fields cached on the strided analysis lattice,
block-expanded on layer push — ~36x less memory than full-grid caching).
Classification summary:

- (a) previously exposed: 14
- (b) newly exposed for free from `ComputedParams`: 22
- (c) implemented with new (cheap, published) computation: 8
  (NCAPE, LCL-LFC mean RH, SRH 0-500 m, BRN shear, BRN, sig-severe, DCP,
  MMP, ESP, SHERBS3/SHERBE/MOSHE, Fosberg, sfc θe — counting families)
- (c) deferred: all spatial-derivative lanes (advection, frontogenesis,
  vorticity/divergence, moisture convergence), SARS-style analog products,
  vector-display products, time-change products
- (d) blocked by data: history-difference fields, p-type, lake-effect,
  unpublished SPC regressions (SSCRAM, CWASP, OPRH, PPP)

## References

- Bothwell, P. D., J. A. Hart, and R. L. Thompson, 2002: An integrated
  three-dimensional objective analysis scheme in use at the Storm
  Prediction Center. *Preprints, 21st Conf. Severe Local Storms*, AMS.
- Bolton, D., 1980: The computation of equivalent potential temperature.
  *Mon. Wea. Rev.*, **108**, 1046–1053.
- Bunkers, M. J., B. A. Klimowski, J. W. Zeitler, R. L. Thompson, and
  M. L. Weisman, 2000: Predicting supercell motion using a new hodograph
  technique. *Wea. Forecasting*, **15**, 61–79.
- Coffer, B. E., M. D. Parker, R. L. Thompson, B. T. Smith, and
  R. E. Jewell, 2019: Using near-ground storm relative helicity in
  supercell tornado forecasting. *Wea. Forecasting*, **34**, 1417–1435.
- Coniglio, M. C., D. J. Stensrud, and L. J. Wicker, 2006: Effects of
  upper-level shear on the structure and maintenance of strong
  quasi-linear MCSs. *J. Atmos. Sci.*, **63**, 1231–1251 (MMP regression
  as ported by SHARPpy/sharprs; see also Coniglio et al. 2007, WAF **22**,
  556–570).
- Corfidi, S. F., 2003: Cold pools and MCS propagation: Forecasting the
  motion of downwind-developing MCSs. *Wea. Forecasting*, **18**, 997–1017.
- Craven, J. P., and H. E. Brooks, 2004: Baseline climatology of sounding
  derived parameters associated with deep moist convection. *Natl. Wea.
  Digest*, **28**, 13–24.
- Davies-Jones, R., D. Burgess, and M. Foster, 1990: Test of helicity as a
  tornado forecast parameter. *Preprints, 16th Conf. Severe Local Storms*.
- Doswell, C. A. III, and E. N. Rasmussen, 1994: The effect of neglecting
  the virtual temperature correction on CAPE calculations.
  *Wea. Forecasting*, **9**, 625–629.
- Esterheld, J. M., and D. J. Giuliano, 2008: Discriminating between
  tornadic and non-tornadic supercells: A new hodograph technique.
  *Electronic J. Severe Storms Meteor.*, **3** (2).
- Evans, J. S., and C. A. Doswell III, 2001: Examination of derecho
  environments using proximity soundings. *Wea. Forecasting*, **16**,
  329–342.
- Fosberg, M. A., 1978: Weather in wildland fire management: The fire
  weather index. *Conf. on Sierra Nevada Meteorology*, AMS, 1–4.
- Hampshire, N. L., R. M. Mosier, T. M. Ryan, and D. E. Cavanaugh, 2018:
  Relationship of low-level instability and tornado damage rating based on
  observed soundings. *J. Operational Meteor.*, **6**, 1–12.
- Hart, J. A., and W. Korotky, 1991: The SHARP workstation v1.50 users
  guide. NOAA/NWS.
- Hart, J. A., and A. E. Cohen, 2016: The statistical severe convective
  risk assessment model. *Wea. Forecasting*, **31**, 1697–1714.
- Johnson, A. W., and K. E. Sugden, 2014: Evaluation of sounding-derived
  thermodynamic and wind-related parameters associated with large hail
  events. *Electronic J. Severe Storms Meteor.*, **9** (5).
- Miller, R. C., 1972: Notes on analysis and severe-storm forecasting
  procedures of the Air Force Global Weather Central. AWS TR-200.
- Rasmussen, E. N., and D. O. Blanchard, 1998: A baseline climatology of
  sounding-derived supercell and tornado forecast parameters.
  *Wea. Forecasting*, **13**, 1148–1164.
- Sherburn, K. D., and M. D. Parker, 2014: Climatology and ingredients of
  significant severe convection in high-shear, low-CAPE environments.
  *Wea. Forecasting*, **29**, 854–877.
- Showalter, A. K., 1953: A stability index for thunderstorm forecasting.
  *Bull. Amer. Meteor. Soc.*, **34**, 250–252.
- Thompson, R. L., R. Edwards, J. A. Hart, K. L. Elmore, and P. Markowski,
  2003: Close proximity soundings within supercell environments obtained
  from the Rapid Update Cycle. *Wea. Forecasting*, **18**, 1243–1261.
- Thompson, R. L., C. M. Mead, and R. Edwards, 2007: Effective
  storm-relative helicity and bulk shear in supercell thunderstorm
  environments. *Wea. Forecasting*, **22**, 102–115.
- Thompson, R. L., B. T. Smith, J. S. Grams, A. R. Dean, and C. Broyles,
  2012: Convective modes for significant severe thunderstorms in the
  contiguous United States. Part II. *Wea. Forecasting*, **27**, 1136–1154.
- Weisman, M. L., and J. B. Klemp, 1982: The dependence of numerically
  simulated convective storms on vertical wind shear and buoyancy.
  *Mon. Wea. Rev.*, **110**, 504–520.
