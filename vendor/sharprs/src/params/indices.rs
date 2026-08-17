//! Stability indices and layer-mean thermodynamic parameters.
//!
//! This module ports the non-parcel stability indices and layer-mean
//! calculations from SHARPpy's `sharptab/params.py` (lines ~731–3383).
//! Every function operates on a [`Profile`] and returns `Option<f64>` when
//! the required levels are missing or outside the sounding domain.
//!
//! ## Conventions
//!
//! | Quantity | Unit |
//! |---|---|
//! | Pressure | hPa (= mb) |
//! | Temperature | °C (unless explicitly Kelvin) |
//! | Height | m MSL or m AGL as documented |
//! | Mixing ratio | g kg⁻¹ |
//! | Precipitable water | inches |
//! | Lapse rate | °C km⁻¹ (positive = temperature decreasing with height) |
//! | Relative humidity | % (0–100) |

use crate::constants::{ROCP, ZEROCNK};
use crate::profile::{self, Profile};

// ═══════════════════════════════════════════════════════════════════════════
// Private helpers — thin wrappers so index code reads cleanly
// ═══════════════════════════════════════════════════════════════════════════

/// Celsius to Kelvin.
#[inline]
fn ctok(t: f64) -> f64 {
    t + ZEROCNK
}

/// Kelvin to Celsius.
#[inline]
fn ktoc(t: f64) -> f64 {
    t - ZEROCNK
}

/// Interpolate temperature (°C) at a pressure level, returning `Option`.
#[inline]
fn temp(prof: &Profile, p: f64) -> Option<f64> {
    let v = prof.interp_tmpc(p);
    if v.is_finite() {
        Some(v)
    } else {
        None
    }
}

/// Interpolate dewpoint (°C) at a pressure level.
#[inline]
fn dwpt(prof: &Profile, p: f64) -> Option<f64> {
    let v = prof.interp_dwpc(p);
    if v.is_finite() {
        Some(v)
    } else {
        None
    }
}

/// Interpolate height (m MSL) at a pressure level.
#[inline]
fn hght(prof: &Profile, p: f64) -> Option<f64> {
    let v = prof.interp_hght(p);
    if v.is_finite() {
        Some(v)
    } else {
        None
    }
}

/// Virtual temperature (°C) at a pressure level.
#[inline]
fn vtmp(prof: &Profile, p: f64) -> Option<f64> {
    let v = prof.interp_by_pressure(&prof.vtmp, p);
    if v.is_finite() {
        Some(v)
    } else {
        None
    }
}

/// Pressure (hPa) at a given MSL height.
#[inline]
fn pres_at(prof: &Profile, h_msl: f64) -> Option<f64> {
    let v = prof.pres_at_height(h_msl);
    if v.is_finite() {
        Some(v)
    } else {
        None
    }
}

/// Mixing ratio (g/kg) using the profile module's implementation.
#[inline]
fn mixratio(p: f64, td: f64) -> f64 {
    profile::mixratio(p, td)
}

/// Theta-e (K) using the profile module's Bolton (1980) implementation.
#[inline]
fn thetae(p: f64, t: f64, td: f64) -> f64 {
    profile::thetae(p, t, td)
}

/// Theta (°C) at given pressure and temperature.
#[inline]
fn theta(p: f64, t: f64) -> f64 {
    profile::theta(p, t)
}

/// Relative humidity (%) from the profile module.
#[inline]
fn relh(p: f64, t: f64, td: f64) -> f64 {
    profile::relh(p, t, td)
}

// ═══════════════════════════════════════════════════════════════════════════
// K-Index
// ═══════════════════════════════════════════════════════════════════════════

/// K-Index.
///
/// ```text
/// K = T_850 − T_500 + Td_850 − (T_700 − Td_700)
/// ```
///
/// Assesses thunderstorm potential; higher values indicate greater
/// low/mid-level instability and moisture.  Typical thresholds:
///
/// | K | Thunderstorm probability |
/// |---|---|
/// | < 20 | None |
/// | 20–25 | Isolated |
/// | 26–30 | Widely scattered |
/// | 31–35 | Scattered |
/// | > 35 | Numerous |
///
/// Returns `None` when any of the three mandatory levels (850, 700, 500 hPa)
/// cannot be interpolated.
pub fn k_index(prof: &Profile) -> Option<f64> {
    let t8 = temp(prof, 850.0)?;
    let t7 = temp(prof, 700.0)?;
    let t5 = temp(prof, 500.0)?;
    let td7 = dwpt(prof, 700.0)?;
    let td8 = dwpt(prof, 850.0)?;
    Some(t8 - t5 + td8 - (t7 - td7))
}

// ═══════════════════════════════════════════════════════════════════════════
// Total Totals family
// ═══════════════════════════════════════════════════════════════════════════

/// Vertical Totals = T_850 − T_500 (°C).
///
/// Measures the static stability between 850 and 500 hPa.  Larger values
/// indicate a steeper lapse rate.
pub fn v_totals(prof: &Profile) -> Option<f64> {
    Some(temp(prof, 850.0)? - temp(prof, 500.0)?)
}

/// Cross Totals = Td_850 − T_500 (°C).
///
/// Combines low-level moisture (850 hPa dewpoint) with upper-level
/// temperature (500 hPa).  Values > 18 suggest increasing convective
/// potential.
pub fn c_totals(prof: &Profile) -> Option<f64> {
    Some(dwpt(prof, 850.0)? - temp(prof, 500.0)?)
}

/// Total Totals = Cross Totals + Vertical Totals.
///
/// ```text
/// TT = Td_850 + T_850 − 2·T_500
/// ```
///
/// Values > 44 suggest increasing thunderstorm potential; values > 55 are
/// often associated with severe convection.
pub fn t_totals(prof: &Profile) -> Option<f64> {
    Some(c_totals(prof)? + v_totals(prof)?)
}

// ═══════════════════════════════════════════════════════════════════════════
// Precipitable Water
// ═══════════════════════════════════════════════════════════════════════════

/// Precipitable water (inches) in the layer from `pbot` to `ptop` (hPa).
///
/// Default: surface to 400 hPa.  Integration uses the trapezoidal rule at
/// 1-hPa steps:
///
/// ```text
/// PW = Σ[ (w_i + w_{i+1})/2 · Δp ] × 0.00040173
/// ```
///
/// where *w* is mixing ratio (g kg⁻¹), Δp is pressure thickness (hPa), and
/// the constant converts (g kg⁻¹ · hPa) to inches.
pub fn precip_water(prof: &Profile, pbot: Option<f64>, ptop: Option<f64>) -> Option<f64> {
    let pbot = pbot.unwrap_or_else(|| prof.sfc_pressure());
    let mut ptop = ptop.unwrap_or(400.0);
    // Clamp ptop to the highest available level if the sounding is shallow
    let top_p = prof.pres[prof.top];
    if top_p.is_finite() && top_p > ptop {
        ptop = top_p;
    }
    if pbot <= ptop {
        return None;
    }

    let n = (pbot - ptop).ceil() as usize + 1;
    let mut pw = 0.0f64;
    let td0 = dwpt(prof, pbot)?;
    let mut w_prev = mixratio(pbot, td0);
    if !w_prev.is_finite() {
        return None;
    }
    let mut p_prev = pbot;

    for i in 1..n {
        let p_cur = (pbot - i as f64).max(ptop);
        let td_cur = dwpt(prof, p_cur)?;
        let w_cur = mixratio(p_cur, td_cur);
        if !w_cur.is_finite() {
            return None;
        }
        pw += (w_prev + w_cur) / 2.0 * (p_prev - p_cur);
        w_prev = w_cur;
        p_prev = p_cur;
        if p_cur <= ptop {
            break;
        }
    }
    Some(pw * 0.00040173)
}

// ═══════════════════════════════════════════════════════════════════════════
// Mean Mixing Ratio
// ═══════════════════════════════════════════════════════════════════════════

/// Mean mixing ratio (g kg⁻¹) in the layer from `pbot` to `ptop`.
///
/// Default layer: surface to surface − 100 hPa.  Simple (unweighted) average
/// of mixing ratios at 1-hPa intervals, matching SHARPpy's fast-path.
pub fn mean_mixratio(prof: &Profile, pbot: Option<f64>, ptop: Option<f64>) -> Option<f64> {
    let pbot = pbot.unwrap_or_else(|| prof.sfc_pressure());
    let ptop = ptop.unwrap_or_else(|| prof.sfc_pressure() - 100.0);
    if pbot <= ptop {
        return None;
    }
    let n = (pbot - ptop).ceil() as usize + 1;
    let mut sum = 0.0f64;
    let mut count = 0u32;
    for i in 0..n {
        let p = (pbot - i as f64).max(ptop);
        if let Some(td) = dwpt(prof, p) {
            let w = mixratio(p, td);
            if w.is_finite() {
                sum += w;
                count += 1;
            }
        }
    }
    if count == 0 {
        None
    } else {
        Some(sum / count as f64)
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Mean Potential Temperature
// ═══════════════════════════════════════════════════════════════════════════

/// Mean potential temperature (°C) in the layer from `pbot` to `ptop`.
///
/// Default layer: surface to surface − 100 hPa.  Pressure-weighted average
/// at 1-hPa intervals, matching SHARPpy convention.
pub fn mean_theta(prof: &Profile, pbot: Option<f64>, ptop: Option<f64>) -> Option<f64> {
    let pbot = pbot.unwrap_or_else(|| prof.sfc_pressure());
    let ptop = ptop.unwrap_or_else(|| prof.sfc_pressure() - 100.0);
    if pbot <= ptop {
        return None;
    }
    let n = (pbot - ptop).ceil() as usize + 1;
    let mut wt_sum = 0.0f64;
    let mut wt_total = 0.0f64;
    for i in 0..n {
        let p = (pbot - i as f64).max(ptop);
        if let Some(t) = temp(prof, p) {
            let th = theta(p, t);
            if th.is_finite() {
                wt_sum += th * p;
                wt_total += p;
            }
        }
    }
    if wt_total == 0.0 {
        None
    } else {
        Some(wt_sum / wt_total)
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Mean Equivalent Potential Temperature
// ═══════════════════════════════════════════════════════════════════════════

/// Mean equivalent potential temperature (K) in the layer from `pbot` to
/// `ptop`.
///
/// Default layer: surface to surface − 100 hPa.  Pressure-weighted average
/// at 1-hPa intervals using Bolton (1980) θ_e.
pub fn mean_thetae(prof: &Profile, pbot: Option<f64>, ptop: Option<f64>) -> Option<f64> {
    let pbot = pbot.unwrap_or_else(|| prof.sfc_pressure());
    let ptop = ptop.unwrap_or_else(|| prof.sfc_pressure() - 100.0);
    if pbot <= ptop {
        return None;
    }
    let n = (pbot - ptop).ceil() as usize + 1;
    let mut wt_sum = 0.0f64;
    let mut wt_total = 0.0f64;
    for i in 0..n {
        let p = (pbot - i as f64).max(ptop);
        if let (Some(t), Some(td)) = (temp(prof, p), dwpt(prof, p)) {
            let te = thetae(p, t, td);
            if te.is_finite() {
                wt_sum += te * p;
                wt_total += p;
            }
        }
    }
    if wt_total == 0.0 {
        None
    } else {
        Some(wt_sum / wt_total)
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Maximum Surface Temperature Forecast
// ═══════════════════════════════════════════════════════════════════════════

/// Forecast maximum surface temperature (°C).
///
/// Takes the temperature at the top of a mixed layer (`mixlayer` hPa deep,
/// default 100 hPa above the surface), adds 2 K, and dry-adiabatically
/// brings the parcel back to the surface:
///
/// ```text
/// T_max = (T_top_K + 2) × (p_sfc / p_top)^(R/Cp) − 273.15
/// ```
pub fn max_temp(prof: &Profile, mixlayer: Option<f64>) -> Option<f64> {
    let depth = mixlayer.unwrap_or(100.0);
    let p_top = prof.sfc_pressure() - depth;
    let t_top = temp(prof, p_top)?;
    let t_top_k = ctok(t_top) + 2.0;
    Some(ktoc(t_top_k * (prof.sfc_pressure() / p_top).powf(ROCP)))
}

// ═══════════════════════════════════════════════════════════════════════════
// Mean Relative Humidity
// ═══════════════════════════════════════════════════════════════════════════

/// Mean relative humidity (%) in the layer from `pbot` to `ptop`.
///
/// Default layer: surface to surface − 100 hPa.  Pressure-weighted average
/// at 1-hPa intervals.
pub fn mean_relh(prof: &Profile, pbot: Option<f64>, ptop: Option<f64>) -> Option<f64> {
    let pbot = pbot.unwrap_or_else(|| prof.sfc_pressure());
    let ptop = ptop.unwrap_or_else(|| prof.sfc_pressure() - 100.0);
    if pbot <= ptop {
        return None;
    }
    let n = (pbot - ptop).ceil() as usize + 1;
    let mut wt_sum = 0.0f64;
    let mut wt_total = 0.0f64;
    for i in 0..n {
        let p = (pbot - i as f64).max(ptop);
        if let (Some(t), Some(td)) = (temp(prof, p), dwpt(prof, p)) {
            let rh = relh(p, t, td);
            if rh.is_finite() {
                wt_sum += rh * p;
                wt_total += p;
            }
        }
    }
    if wt_total == 0.0 {
        None
    } else {
        Some(wt_sum / wt_total)
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Lapse Rate
// ═══════════════════════════════════════════════════════════════════════════

/// Environmental lapse rate (°C km⁻¹) between two levels.
///
/// When `pres_coords` is `true`, `lower` and `upper` are pressure levels
/// (hPa) — `lower` is the higher pressure (lower altitude).
/// When `false`, they are heights in metres AGL.
///
/// Uses virtual temperature.  Positive values indicate temperature
/// decreasing with height (conditionally/absolutely unstable direction).
pub fn lapse_rate(prof: &Profile, lower: f64, upper: f64, pres_coords: bool) -> Option<f64> {
    let (p1, p2, z1, z2);
    if pres_coords {
        p1 = lower;
        p2 = upper;
        // Check that top pressure is within the sounding
        let top_p = prof.pres[prof.top];
        if top_p.is_finite() && top_p > upper {
            return None;
        }
        z1 = hght(prof, p1)?;
        z2 = hght(prof, p2)?;
    } else {
        let z1_msl = prof.to_msl(lower);
        let z2_msl = prof.to_msl(upper);
        p1 = pres_at(prof, z1_msl)?;
        p2 = pres_at(prof, z2_msl)?;
        z1 = z1_msl;
        z2 = z2_msl;
    }
    let tv1 = vtmp(prof, p1)?;
    let tv2 = vtmp(prof, p2)?;
    let dz = z2 - z1;
    if dz.abs() < 1.0 {
        return None;
    }
    Some((tv2 - tv1) / dz * -1000.0)
}

// ═══════════════════════════════════════════════════════════════════════════
// Maximum Lapse Rate
// ═══════════════════════════════════════════════════════════════════════════

/// Result of [`max_lapse_rate`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MaxLapseRate {
    /// Maximum lapse rate (°C km⁻¹).
    pub value: f64,
    /// Bottom pressure of the max-lapse-rate layer (hPa).
    pub pbot: f64,
    /// Top pressure of the max-lapse-rate layer (hPa).
    pub ptop: f64,
}

/// Maximum lapse rate (°C km⁻¹) found by sliding a `depth`-m layer through
/// the sounding from `lower` to `upper` (AGL, m) at `interval`-m steps.
///
/// Defaults: lower = 2000 m, upper = 6000 m, interval = 250 m, depth = 2000 m.
///
/// Virtual temperature is used, consistent with SHARPpy.
pub fn max_lapse_rate(
    prof: &Profile,
    lower: Option<f64>,
    upper: Option<f64>,
    interval: Option<f64>,
    depth: Option<f64>,
) -> Option<MaxLapseRate> {
    let lower_agl = lower.unwrap_or(2000.0);
    let upper_agl = upper.unwrap_or(6000.0);
    let step = interval.unwrap_or(250.0);
    let depth = depth.unwrap_or(2000.0);

    let mut best: Option<MaxLapseRate> = None;
    let mut bot = lower_agl;
    while bot + depth <= upper_agl + step * 0.5 {
        let z_bot = prof.to_msl(bot);
        let z_top = prof.to_msl(bot + depth);
        let p_bot = pres_at(prof, z_bot)?;
        let p_top = pres_at(prof, z_top)?;
        let tv_bot = vtmp(prof, p_bot)?;
        let tv_top = vtmp(prof, p_top)?;
        let lr = (tv_top - tv_bot) * -1000.0 / depth;
        match &best {
            Some(b) if lr <= b.value => {}
            _ => {
                best = Some(MaxLapseRate {
                    value: lr,
                    pbot: p_bot,
                    ptop: p_top,
                });
            }
        }
        bot += step;
    }
    best
}

// ═══════════════════════════════════════════════════════════════════════════
// Temperature Inversion Detection
// ═══════════════════════════════════════════════════════════════════════════

/// A detected temperature inversion.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Inversion {
    /// Pressure at the base (hPa).
    pub pbot: f64,
    /// Pressure at the top (hPa).
    pub ptop: f64,
    /// Temperature at the base (°C).
    pub tbot: f64,
    /// Temperature at the top (°C).
    pub ttop: f64,
    /// Strength: temperature increase across the layer (°C, positive).
    pub strength: f64,
}

/// Find the lowest temperature inversion in the sounding.
///
/// An inversion is a layer where temperature *increases* with height.  The
/// routine searches upward from the surface.  Returns `None` if no inversion
/// is found.
pub fn inversion(prof: &Profile) -> Option<Inversion> {
    if prof.num_levels() < 2 {
        return None;
    }
    let mut in_inv = false;
    let mut inv_bot = 0usize;

    for i in 0..prof.num_levels() - 1 {
        let t_lo = prof.tmpc[i];
        let t_hi = prof.tmpc[i + 1];
        if !t_lo.is_finite() || !t_hi.is_finite() {
            continue;
        }
        if !in_inv && t_hi > t_lo {
            in_inv = true;
            inv_bot = i;
        } else if in_inv && t_hi <= t_lo {
            return Some(Inversion {
                pbot: prof.pres[inv_bot],
                ptop: prof.pres[i],
                tbot: prof.tmpc[inv_bot],
                ttop: prof.tmpc[i],
                strength: prof.tmpc[i] - prof.tmpc[inv_bot],
            });
        }
    }
    if in_inv {
        let last = prof.num_levels() - 1;
        return Some(Inversion {
            pbot: prof.pres[inv_bot],
            ptop: prof.pres[last],
            tbot: prof.tmpc[inv_bot],
            ttop: prof.tmpc[last],
            strength: prof.tmpc[last] - prof.tmpc[inv_bot],
        });
    }
    None
}

// ═══════════════════════════════════════════════════════════════════════════
// Convective Temperature
// ═══════════════════════════════════════════════════════════════════════════

/// Convective temperature (°C) via the Convective Condensation Level (CCL)
/// method.
///
/// 1. Compute the mean mixing ratio in the lowest 100 hPa.
/// 2. Follow the saturation mixing-ratio line upward to where it intersects
///    the environmental temperature profile — this is the CCL.
/// 3. Descend dry-adiabatically from the CCL back to the surface to obtain
///    the convective temperature.
///
/// The full SHARPpy version (`convective_temp`) iteratively lifts parcels
/// with increasing surface temperature until CINH is eliminated — that
/// requires a complete CAPE routine.  This CCL-based estimate gives
/// equivalent results for most soundings and is self-contained.
pub fn conv_t(prof: &Profile) -> Option<f64> {
    let mmr = mean_mixratio(prof, None, None)?;
    let sfc_p = prof.sfc_pressure();

    // Search upward (1-hPa steps) for the CCL: where the saturation mixing
    // ratio of the environment equals the mean surface-layer mixing ratio.
    let n = (sfc_p - 100.0).max(0.0).ceil() as usize;
    let mut ccl_p = None;
    let mut ccl_t = None;
    for i in 0..n {
        let p = sfc_p - i as f64;
        if let Some(t) = temp(prof, p) {
            // Saturation mixing ratio at environmental temperature
            let ws = mixratio(p, t);
            if ws.is_finite() && ws <= mmr {
                ccl_p = Some(p);
                ccl_t = Some(t);
                break;
            }
        }
    }
    let ccl_p = ccl_p?;
    let ccl_t_val = ccl_t?;

    // Dry-adiabatic descent from CCL to surface
    let theta_ccl = ctok(ccl_t_val) * (1000.0 / ccl_p).powf(ROCP);
    let t_conv = theta_ccl * (sfc_p / 1000.0).powf(ROCP);
    Some(ktoc(t_conv))
}

// ═══════════════════════════════════════════════════════════════════════════
// Enhanced Stretching Potential (ESP)
// ═══════════════════════════════════════════════════════════════════════════

/// Enhanced Stretching Potential (ESP).
///
/// ```text
/// ESP = (MLCAPE_0-3km / 50) × (LR_0-3km − 7)
/// ```
///
/// Identifies environments where low-level buoyancy and steep low-level
/// lapse rates co-exist, favouring vortex stretching and tornado potential.
///
/// Returns 0 when `lr_0_3km` < 7 °C km⁻¹ or `mlcape` < 250 J kg⁻¹.
///
/// `mlcape` and `mlcape_3km` (0–3 km MLCAPE) must be pre-computed since a
/// full parcel routine is required.
pub fn esp(prof: &Profile, mlcape: f64, mlcape_3km: f64) -> Option<f64> {
    let lr03 = lapse_rate(prof, 0.0, 3000.0, false)?;
    if lr03 < 7.0 || mlcape < 250.0 {
        return Some(0.0);
    }
    Some((mlcape_3km / 50.0) * (lr03 - 7.0))
}

// ═══════════════════════════════════════════════════════════════════════════
// Theta-E Difference
// ═══════════════════════════════════════════════════════════════════════════

/// Theta-E difference (K) in the lowest 3000 m AGL.
///
/// Finds the maximum and minimum θ_e in the lowest 3 km.  Returns the
/// difference only when the maximum θ_e is below the minimum (i.e., the
/// max is at higher pressure / lower altitude), indicating an inverted
/// θ_e profile that favours downdraft production from evaporational
/// cooling.  Otherwise returns 0.
///
/// Matches SHARPpy's `thetae_diff()` (adapted from Rich Thompson, SPC).
pub fn thetae_diff(prof: &Profile) -> Option<f64> {
    let sfc_h = prof.sfc_height();
    let top_h = sfc_h + 3000.0;

    let mut max_te = f64::NEG_INFINITY;
    let mut min_te = f64::INFINITY;
    let mut max_p = 0.0f64;
    let mut min_p = 0.0f64;
    let mut found = false;

    for i in 0..prof.num_levels() {
        let h = prof.hght[i];
        if !h.is_finite() || h > top_h {
            break;
        }
        let t = prof.tmpc[i];
        let td = prof.dwpc[i];
        if !t.is_finite() || !td.is_finite() {
            continue;
        }
        let te = thetae(prof.pres[i], t, td);
        if !te.is_finite() {
            continue;
        }
        found = true;
        if te > max_te {
            max_te = te;
            max_p = prof.pres[i];
        }
        if te < min_te {
            min_te = te;
            min_p = prof.pres[i];
        }
    }
    if !found {
        return None;
    }
    // max_p > min_p means max θ_e is at a lower altitude (higher pressure)
    if max_p > min_p {
        Some(max_te - min_te)
    } else {
        Some(0.0)
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Large Hail Parameter (LHP)
// ═══════════════════════════════════════════════════════════════════════════

/// Large Hail Parameter (LHP) — Johnson and Sugden (2014), EJSSM.
///
/// Requires pre-computed kinematic and parcel parameters since the full
/// calculation depends on storm motion, effective-inflow layer, and CAPE.
///
/// ## Inputs
///
/// - `mucape` — Most-Unstable CAPE (J kg⁻¹)
/// - `mag06_shr` — 0–6 km bulk shear magnitude (m s⁻¹)
/// - `lr75` — 700–500 hPa lapse rate (°C km⁻¹)
/// - `hgz_thickness` — Thickness of the Hail Growth Zone (−10 °C to −30 °C)
///   in metres
/// - `shear_el` — Shear magnitude from surface to EL (m s⁻¹)
/// - `grw_alpha_el` — Angle between EL wind direction and 3–6 km mean wind
///   direction (degrees)
/// - `srw_alpha_mid` — Angle between 3–6 km and 0–1 km storm-relative wind
///   directions (degrees)
///
/// Returns 0 when MUCAPE < 400 J kg⁻¹ or 0–6 km shear < 14 m s⁻¹.
pub fn lhp(
    mucape: f64,
    mag06_shr: f64,
    lr75: f64,
    hgz_thickness: f64,
    shear_el: f64,
    grw_alpha_el: f64,
    srw_alpha_mid: f64,
) -> f64 {
    if mucape < 400.0 || mag06_shr < 14.0 {
        return 0.0;
    }

    let mut term_a =
        ((mucape - 2000.0) / 1000.0) + ((3200.0 - hgz_thickness) / 500.0) + ((lr75 - 6.5) / 2.0);
    if term_a < 0.0 {
        term_a = 0.0;
    }

    let alpha_el = if grw_alpha_el > 180.0 {
        -10.0
    } else {
        grw_alpha_el
    };

    let mut term_b =
        ((shear_el - 25.0) / 5.0) + ((alpha_el + 5.0) / 20.0) + ((srw_alpha_mid - 80.0) / 10.0);
    if term_b < 0.0 {
        term_b = 0.0;
    }

    term_a * term_b + 5.0
}

// ═══════════════════════════════════════════════════════════════════════════
// Dendritic Growth Zone (DGZ)
// ═══════════════════════════════════════════════════════════════════════════

/// Dendritic Growth Zone pressures `(pbot, ptop)` in hPa.
///
/// The DGZ spans the −12 °C to −17 °C temperature layer — the regime where
/// dendritic (snowflake) ice crystal growth is most efficient.
///
/// If either temperature level cannot be found, the surface pressure is
/// substituted for that bound.
pub fn dgz(prof: &Profile) -> (f64, f64) {
    let pbot = temp_lvl(prof, -12.0, false).unwrap_or_else(|| prof.sfc_pressure());
    let ptop = temp_lvl(prof, -17.0, false).unwrap_or_else(|| prof.sfc_pressure());
    (pbot, ptop)
}

// ═══════════════════════════════════════════════════════════════════════════
// Temperature Level
// ═══════════════════════════════════════════════════════════════════════════

/// Find the pressure (hPa) of the first occurrence of a given temperature
/// (°C) in the sounding.
///
/// When `use_wetbulb` is `true`, the wet-bulb temperature profile is
/// searched instead of dry-bulb.
///
/// Searches from the surface upward (decreasing pressure).  Returns `None`
/// if the temperature never occurs.
pub fn temp_lvl(prof: &Profile, target_temp: f64, use_wetbulb: bool) -> Option<f64> {
    let n = prof.num_levels();
    if n < 2 {
        return None;
    }

    // Select the temperature profile to search
    let profile_vals: &[f64] = if use_wetbulb {
        &prof.wetbulb
    } else {
        &prof.tmpc
    };

    // Compute differences from target
    let diffs: Vec<f64> = profile_vals
        .iter()
        .map(|&v| {
            if v.is_finite() {
                v - target_temp
            } else {
                f64::NAN
            }
        })
        .collect();

    // Check if values exist on both sides of zero
    let has_pos = diffs.iter().any(|&d| d.is_finite() && d >= 0.0);
    let has_neg = diffs.iter().any(|&d| d.is_finite() && d <= 0.0);
    if !has_pos || !has_neg {
        return None;
    }

    // Check for exact match
    for (i, &d) in diffs.iter().enumerate() {
        if d.is_finite() && d.abs() < 1e-10 {
            return Some(prof.pres[i]);
        }
    }

    // Find first sign change and interpolate in log-pressure
    for i in 0..n - 1 {
        if !diffs[i].is_finite() || !diffs[i + 1].is_finite() {
            continue;
        }
        if diffs[i] * diffs[i + 1] < 0.0 {
            // Log-pressure interpolation
            let lp_lo = prof.pres[i].ln();
            let lp_hi = prof.pres[i + 1].ln();
            let t_lo = profile_vals[i];
            let t_hi = profile_vals[i + 1];
            if (t_hi - t_lo).abs() < 1e-12 {
                return Some(prof.pres[i]);
            }
            let frac = (target_temp - t_lo) / (t_hi - t_lo);
            let logp = lp_lo + frac * (lp_hi - lp_lo);
            return Some(logp.exp());
        }
    }
    None
}

// ═══════════════════════════════════════════════════════════════════════════
// Wet-Bulb Zero Height
// ═══════════════════════════════════════════════════════════════════════════

/// Height (m AGL) of the wet-bulb zero level — where the wet-bulb
/// temperature first reaches 0 °C.
///
/// Important for hail-size estimation and precipitation-type
/// discrimination.  Returns `None` if the wet-bulb profile never reaches
/// 0 °C.
pub fn wet_bulb_zero(prof: &Profile) -> Option<f64> {
    let p0 = temp_lvl(prof, 0.0, true)?;
    let h_msl = hght(prof, p0)?;
    Some(prof.to_agl(h_msl))
}

// ═══════════════════════════════════════════════════════════════════════════
// Coniglio MCS Maintenance Parameter (MMP)
// ═══════════════════════════════════════════════════════════════════════════

/// MCS Maintenance Probability (MMP) — Coniglio et al. (2006).
///
/// Logistic regression:
///
/// ```text
/// MMP = 1 / (1 + exp(a₀ + a₁·S + a₂·Γ + a₃·C + a₄·V̄))
/// ```
///
/// | Coefficient | Value | Units |
/// |---|---|---|
/// | a₀ | 13.0 | — |
/// | a₁ | −4.59 × 10⁻² | m⁻¹ s |
/// | a₂ | −1.16 | K⁻¹ km |
/// | a₃ | −6.17 × 10⁻⁴ | J⁻¹ kg |
/// | a₄ | −0.17 | m⁻¹ s |
///
/// - `S` = maximum bulk shear between all winds in 0–1 km AGL and all winds
///   in 6–10 km AGL (m s⁻¹)
/// - `Γ` = 3–8 km lapse rate (°C km⁻¹)
/// - `C` = MUCAPE (J kg⁻¹)
/// - `V̄` = 3–12 km mean wind speed (m s⁻¹)
///
/// Returns 0 when MUCAPE < 100 J kg⁻¹.
pub fn coniglio(mucape: f64, max_bulk_shear: f64, lr38: f64, mean_wind_3_12: f64) -> f64 {
    if mucape < 100.0 {
        return 0.0;
    }
    let a0 = 13.0;
    let a1 = -4.59e-2;
    let a2 = -1.16;
    let a3 = -6.17e-4;
    let a4 = -0.17;
    let exponent = a0 + a1 * max_bulk_shear + a2 * lr38 + a3 * mucape + a4 * mean_wind_3_12;
    1.0 / (1.0 + exponent.exp())
}

// ═══════════════════════════════════════════════════════════════════════════
// Hail Growth Zone (HGZ)
// ═══════════════════════════════════════════════════════════════════════════

/// Hail Growth Zone pressures `(pbot, ptop)` in hPa.
///
/// The HGZ spans the −10 °C to −30 °C layer, the favoured regime for hail
/// embryo growth.  If either temperature level is not found, the surface
/// pressure is substituted.
pub fn hgz(prof: &Profile) -> (f64, f64) {
    let pbot = temp_lvl(prof, -10.0, false).unwrap_or_else(|| prof.sfc_pressure());
    let ptop = temp_lvl(prof, -30.0, false).unwrap_or_else(|| prof.sfc_pressure());
    (pbot, ptop)
}

// ═══════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::StationInfo;

    /// Build a 17-level idealised sounding.
    ///
    /// Surface: 1000 hPa, 0 m MSL, 30 °C / 20 °C dewpoint.
    /// Rough standard-atmosphere lapse rate with decreasing moisture aloft.
    fn test_profile() -> Profile {
        let pres = [
            1000.0, 950.0, 900.0, 850.0, 800.0, 750.0, 700.0, 650.0, 600.0, 550.0, 500.0, 450.0,
            400.0, 350.0, 300.0, 250.0, 200.0,
        ];
        let hght = [
            0.0, 540.0, 1000.0, 1500.0, 2000.0, 2500.0, 3000.0, 3500.0, 4200.0, 4900.0, 5600.0,
            6400.0, 7200.0, 8100.0, 9200.0, 10400.0, 11800.0,
        ];
        let tmpc = [
            30.0, 25.0, 20.0, 15.0, 10.0, 5.0, 0.0, -5.0, -12.0, -20.0, -28.0, -36.0, -45.0, -52.0,
            -58.0, -62.0, -60.0,
        ];
        let dwpc = [
            20.0, 17.0, 14.0, 10.0, 6.0, 1.0, -5.0, -12.0, -20.0, -28.0, -36.0, -44.0, -52.0,
            -58.0, -62.0, -68.0, -72.0,
        ];
        Profile::new(
            &pres,
            &hght,
            &tmpc,
            &dwpc,
            &[],
            &[],
            &[],
            StationInfo::default(),
        )
        .expect("test sounding should be valid")
    }

    /// Build a sounding with a low-level inversion.
    fn inversion_profile() -> Profile {
        let pres = [1000.0, 950.0, 900.0, 850.0, 800.0, 700.0, 500.0];
        let hght = [0.0, 540.0, 1000.0, 1500.0, 2000.0, 3000.0, 5600.0];
        let tmpc = [15.0, 10.0, 12.0, 8.0, 4.0, -2.0, -20.0];
        let dwpc = [10.0, 6.0, 5.0, 2.0, -2.0, -10.0, -30.0];
        Profile::new(
            &pres,
            &hght,
            &tmpc,
            &dwpc,
            &[],
            &[],
            &[],
            StationInfo::default(),
        )
        .expect("inversion sounding should be valid")
    }

    // ---- K-Index --------------------------------------------------------

    #[test]
    fn k_index_basic() {
        let prof = test_profile();
        let ki = k_index(&prof).unwrap();
        // T850=15, T500=-28, Td850=10, T700=0, Td700=-5
        // K = 15-(-28) + 10 - (0-(-5)) = 43 + 10 - 5 = 48
        assert!((ki - 48.0).abs() < 0.5, "K-Index should be ~48, got {ki}");
    }

    #[test]
    fn k_index_shallow_sounding() {
        // Only 2 levels, missing 850/700/500
        let prof = Profile::new(
            &[1000.0, 950.0],
            &[0.0, 540.0],
            &[30.0, 25.0],
            &[20.0, 17.0],
            &[],
            &[],
            &[],
            StationInfo::default(),
        )
        .unwrap();
        assert!(k_index(&prof).is_none());
    }

    // ---- Total Totals family --------------------------------------------

    #[test]
    fn v_totals_basic() {
        let prof = test_profile();
        let vt = v_totals(&prof).unwrap();
        assert!((vt - 43.0).abs() < 0.5, "V-Totals should be ~43, got {vt}");
    }

    #[test]
    fn c_totals_basic() {
        let prof = test_profile();
        let ct = c_totals(&prof).unwrap();
        // Td850=10, T500=-28 → CT = 38
        assert!((ct - 38.0).abs() < 0.5, "C-Totals should be ~38, got {ct}");
    }

    #[test]
    fn t_totals_basic() {
        let prof = test_profile();
        let tt = t_totals(&prof).unwrap();
        assert!((tt - 81.0).abs() < 1.0, "T-Totals should be ~81, got {tt}");
    }

    // ---- Precipitable Water ---------------------------------------------

    #[test]
    fn precip_water_positive() {
        let prof = test_profile();
        let pw = precip_water(&prof, None, None).unwrap();
        assert!(pw > 0.0, "PW should be > 0, got {pw}");
        assert!(pw < 5.0, "PW should be < 5 inches, got {pw}");
    }

    #[test]
    fn precip_water_shallow_less_than_deep() {
        let prof = test_profile();
        let pw_deep = precip_water(&prof, None, Some(400.0)).unwrap();
        let pw_shallow = precip_water(&prof, None, Some(700.0)).unwrap();
        assert!(
            pw_shallow < pw_deep,
            "Shallow PW ({pw_shallow}) should be < deep PW ({pw_deep})"
        );
    }

    // ---- Mean Mixing Ratio ----------------------------------------------

    #[test]
    fn mean_mixratio_in_range() {
        let prof = test_profile();
        let mmr = mean_mixratio(&prof, None, None).unwrap();
        // Surface Td=20°C at 1000 hPa → MR ~14.7 g/kg; layer mean somewhat less
        assert!(mmr > 5.0 && mmr < 20.0, "Mean MR should be 5–20, got {mmr}");
    }

    // ---- Mean Theta -----------------------------------------------------

    #[test]
    fn mean_theta_reasonable() {
        let prof = test_profile();
        let mt = mean_theta(&prof, None, None).unwrap();
        // Surface θ at 1000 hPa/30°C ≈ 30°C → layer mean 29–31°C
        assert!(
            mt > 25.0 && mt < 35.0,
            "Mean theta should be 25–35, got {mt}"
        );
    }

    // ---- Mean Theta-E ---------------------------------------------------

    #[test]
    fn mean_thetae_reasonable() {
        let prof = test_profile();
        let mte = mean_thetae(&prof, None, None).unwrap();
        assert!(
            mte > 300.0 && mte < 400.0,
            "Mean theta-e should be 300–400 K, got {mte}"
        );
    }

    // ---- Max Temp -------------------------------------------------------

    #[test]
    fn max_temp_above_surface() {
        let prof = test_profile();
        let mt = max_temp(&prof, None).unwrap();
        assert!(
            mt > 28.0 && mt < 50.0,
            "Max temp should be 28–50°C, got {mt}"
        );
    }

    // ---- Mean RH --------------------------------------------------------

    #[test]
    fn mean_relh_range() {
        let prof = test_profile();
        let mrh = mean_relh(&prof, None, None).unwrap();
        assert!(
            mrh > 0.0 && mrh <= 100.0,
            "Mean RH should be 0–100%, got {mrh}"
        );
    }

    // ---- Lapse Rate -----------------------------------------------------

    #[test]
    fn lapse_rate_pressure_coords() {
        let prof = test_profile();
        let lr = lapse_rate(&prof, 700.0, 500.0, true).unwrap();
        assert!(lr > 5.0, "700-500 LR should be > 5, got {lr}");
    }

    #[test]
    fn lapse_rate_height_coords() {
        let prof = test_profile();
        let lr = lapse_rate(&prof, 0.0, 3000.0, false).unwrap();
        assert!(lr > 5.0 && lr < 15.0, "0-3km LR should be 5–15, got {lr}");
    }

    // ---- Max Lapse Rate -------------------------------------------------

    #[test]
    fn max_lapse_rate_positive() {
        let prof = test_profile();
        let mlr = max_lapse_rate(&prof, None, None, None, None).unwrap();
        assert!(mlr.value > 0.0, "Max LR should be > 0, got {}", mlr.value);
        assert!(
            mlr.pbot > mlr.ptop,
            "pbot ({}) should be > ptop ({})",
            mlr.pbot,
            mlr.ptop
        );
    }

    // ---- Inversion Detection --------------------------------------------

    #[test]
    fn inversion_found() {
        let prof = inversion_profile();
        let inv = inversion(&prof).unwrap();
        // Inversion is at 950→900 hPa (10°C → 12°C)
        assert!(
            (inv.pbot - 950.0).abs() < 1.0,
            "Inversion base should be ~950, got {}",
            inv.pbot
        );
        assert!(inv.strength > 0.0, "Inversion strength should be > 0");
    }

    #[test]
    fn no_inversion_in_monotone() {
        let prof = test_profile();
        // First 10 levels are monotonically decreasing temperature
        let trunc = prof.truncate(1000.0, 550.0).unwrap();
        assert!(
            inversion(&trunc).is_none(),
            "No inversion expected in monotonically cooling profile"
        );
    }

    // ---- Convective Temperature -----------------------------------------

    #[test]
    fn conv_t_reasonable() {
        let prof = test_profile();
        if let Some(ct) = conv_t(&prof) {
            assert!(ct >= 25.0, "Conv temp should be >= 25°C, got {ct}");
        }
    }

    // ---- ESP ------------------------------------------------------------

    #[test]
    fn esp_zero_low_cape() {
        let prof = test_profile();
        let e = esp(&prof, 100.0, 50.0).unwrap();
        assert_eq!(e, 0.0, "ESP should be 0 when MLCAPE < 250");
    }

    #[test]
    fn esp_positive_favorable() {
        let prof = test_profile();
        let lr03 = lapse_rate(&prof, 0.0, 3000.0, false).unwrap();
        if lr03 >= 7.0 {
            let e = esp(&prof, 500.0, 200.0).unwrap();
            assert!(e > 0.0, "ESP should be > 0 with favorable conditions");
        }
    }

    // ---- Theta-E Difference ---------------------------------------------

    #[test]
    fn thetae_diff_nonnegative() {
        let prof = test_profile();
        let ted = thetae_diff(&prof).unwrap();
        assert!(ted >= 0.0, "Theta-E diff should be >= 0, got {ted}");
    }

    // ---- LHP ------------------------------------------------------------

    #[test]
    fn lhp_zero_low_cape() {
        assert_eq!(lhp(300.0, 20.0, 7.0, 3000.0, 30.0, 10.0, 90.0), 0.0);
    }

    #[test]
    fn lhp_zero_low_shear() {
        assert_eq!(lhp(500.0, 10.0, 7.0, 3000.0, 30.0, 10.0, 90.0), 0.0);
    }

    #[test]
    fn lhp_positive_favorable() {
        let val = lhp(3000.0, 25.0, 7.5, 2800.0, 35.0, 15.0, 100.0);
        assert!(
            val > 5.0,
            "LHP should be > 5 with favorable params, got {val}"
        );
    }

    #[test]
    fn lhp_alpha_el_clamp() {
        // grw_alpha_el > 180 should be treated as -10
        let val = lhp(3000.0, 25.0, 7.5, 2800.0, 35.0, 200.0, 100.0);
        assert!(val >= 5.0, "LHP should handle alpha_el > 180");
    }

    // ---- DGZ ------------------------------------------------------------

    #[test]
    fn dgz_ordered() {
        let prof = test_profile();
        let (pbot, ptop) = dgz(&prof);
        assert!(pbot > ptop, "DGZ pbot ({pbot}) should be > ptop ({ptop})");
    }

    // ---- Temp Level -----------------------------------------------------

    #[test]
    fn temp_lvl_freezing() {
        let prof = test_profile();
        let p0 = temp_lvl(&prof, 0.0, false).unwrap();
        // 0°C is at 700 hPa in the test profile
        assert!(
            (p0 - 700.0).abs() < 1.0,
            "0°C level should be ~700 hPa, got {p0}"
        );
    }

    #[test]
    fn temp_lvl_minus20() {
        let prof = test_profile();
        let p = temp_lvl(&prof, -20.0, false).unwrap();
        // -20°C is at 550 hPa
        assert!(
            (p - 550.0).abs() < 1.0,
            "-20°C level should be ~550 hPa, got {p}"
        );
    }

    #[test]
    fn temp_lvl_not_found() {
        let prof = test_profile();
        assert!(
            temp_lvl(&prof, -100.0, false).is_none(),
            "-100°C should not be found"
        );
    }

    #[test]
    fn temp_lvl_wetbulb() {
        let prof = test_profile();
        // Wet-bulb zero should be somewhere below the 0°C level
        let wb0 = temp_lvl(&prof, 0.0, true);
        let dry0 = temp_lvl(&prof, 0.0, false).unwrap();
        if let Some(wb0_p) = wb0 {
            // Wet-bulb zero is at higher pressure (lower altitude) than freezing level
            assert!(
                wb0_p >= dry0 - 50.0,
                "WBZ ({wb0_p}) should be near or below freezing level ({dry0})"
            );
        }
    }

    // ---- Wet-Bulb Zero --------------------------------------------------

    #[test]
    fn wet_bulb_zero_positive() {
        let prof = test_profile();
        if let Some(wbz) = wet_bulb_zero(&prof) {
            assert!(
                wbz > 0.0 && wbz < 5000.0,
                "WBZ height should be 0–5000 m AGL, got {wbz}"
            );
        }
    }

    // ---- Coniglio MMP ---------------------------------------------------

    #[test]
    fn coniglio_zero_low_cape() {
        assert_eq!(coniglio(50.0, 30.0, 7.0, 15.0), 0.0);
    }

    #[test]
    fn coniglio_positive() {
        let m = coniglio(2500.0, 40.0, 7.5, 18.0);
        assert!(m > 0.0 && m <= 1.0, "MMP should be in (0, 1], got {m}");
    }

    #[test]
    fn coniglio_probability_range() {
        for cape in [100.0, 500.0, 1000.0, 2000.0, 4000.0] {
            for shear in [10.0, 30.0, 50.0] {
                for lr in [5.0, 7.0, 9.0] {
                    for wind in [5.0, 15.0, 25.0] {
                        let m = coniglio(cape, shear, lr, wind);
                        assert!(
                            (0.0..=1.0).contains(&m),
                            "MMP({cape},{shear},{lr},{wind}) = {m} not in [0,1]"
                        );
                    }
                }
            }
        }
    }

    // ---- HGZ ------------------------------------------------------------

    #[test]
    fn hgz_ordered() {
        let prof = test_profile();
        let (pbot, ptop) = hgz(&prof);
        assert!(pbot > ptop, "HGZ pbot ({pbot}) should be > ptop ({ptop})");
    }
}
