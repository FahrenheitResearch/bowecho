//! Composite severe weather parameters.
//!
//! These are the operational composite indices used by the Storm Prediction
//! Center (SPC) and the broader severe-storms community to discriminate
//! between significant-severe, marginal, and non-severe convective
//! environments.  Every formula is transcribed verbatim from the SHARPpy
//! `sharptab/params.py` reference implementation so that coefficient values,
//! clamp thresholds, and edge-case handling match exactly.
//!
//! # Naming convention
//!
//! Each public function is named after the standard abbreviation (STP, SCP,
//! SHIP, etc.).  Where SHARPpy exposes two variants of the same parameter
//! (e.g. `stp_fixed` vs `stp_cin`), both are provided.
//!
//! # Missing-data protocol
//!
//! All functions accept `Option<f64>` for any input that might be missing
//! from the sounding.  When a required input is `None`, the function returns
//! `None` rather than producing a garbage number.  This mirrors the
//! `numpy.ma.masked` / `np.nan` convention in SHARPpy.

use std::f64::consts::PI;

/// Knots-to-m/s conversion factor (1 kt = 0.514444... m/s).
const KTS2MS: f64 = 0.514_444;

// ---------------------------------------------------------------------------
// Helper: quality-check an f64 (not NaN, not missing sentinel)
// ---------------------------------------------------------------------------

/// Returns `true` if `v` is a usable numeric value.
#[inline]
fn qc(v: f64) -> bool {
    v.is_finite() && (v - crate::constants::MISSING).abs() > 1.0
}

// =========================================================================
// Significant Tornado Parameter — fixed layer  (Thompson et al. 2003)
// =========================================================================

/// Significant Tornado Parameter (fixed layer).
///
/// # Formula
///
/// ```text
/// STP_fixed = (SBCAPE / 1500) * LCL_term * (SRH01 / 150) * BWD6_term
/// ```
///
/// where
///
/// * **LCL_term** = 1.0 if SBLCL < 1000 m, 0.0 if SBLCL > 2000 m,
///   linearly interpolated in between.
/// * **BWD6_term** = 0-6 km BWD clamped to \[0, 30\] m/s then divided by 20;
///   set to 0 when BWD < 12.5 m/s.
///
/// # Reference
///
/// Thompson, R. L., R. Edwards, J. A. Hart, K. L. Elmore, and P. Markowski,
/// 2003: Close proximity soundings within supercell environments obtained
/// from the Rapid Update Cycle. *Wea. Forecasting*, **18**, 1243-1261.
///
/// # Physical interpretation
///
/// Combines surface-based buoyancy (SBCAPE), low-level wind shear (0-1 km
/// SRH), deep-layer shear (0-6 km BWD), and boundary-layer moisture depth
/// (LCL height) into a single discriminator for significant (EF2+) tornado
/// environments.  Values >= 1 are considered favourable.
pub fn stp_fixed(sbcape: f64, sblcl: f64, srh01: f64, bwd6: f64) -> Option<f64> {
    if !qc(sbcape) || !qc(sblcl) || !qc(srh01) || !qc(bwd6) {
        return None;
    }

    // LCL term
    let lcl_term = if sblcl < 1000.0 {
        1.0
    } else if sblcl > 2000.0 {
        0.0
    } else {
        (2000.0 - sblcl) / 1000.0
    };

    // 0-6 km BWD term
    let bwd6_clamped = if bwd6 < 12.5 {
        0.0
    } else if bwd6 > 30.0 {
        30.0
    } else {
        bwd6
    };
    let bwd6_term = bwd6_clamped / 20.0;

    let cape_term = sbcape / 1500.0;
    let srh_term = srh01 / 150.0;

    Some(cape_term * lcl_term * srh_term * bwd6_term)
}

// =========================================================================
// Significant Tornado Parameter — CIN variant  (Thompson et al. 2012)
// =========================================================================

/// Significant Tornado Parameter with CIN term.
///
/// # Formula
///
/// ```text
/// STP_cin = max(0, CAPE_term * ESRH_term * EBWD_term * LCL_term * CIN_term)
/// ```
///
/// where each term is defined as in `stp_fixed` but uses the effective
/// inflow layer (effective SRH, effective BWD) and adds an explicit CIN
/// penalty:
///
/// * **CIN_term** = 1.0 if MLCIN > -50 J/kg, 0.0 if MLCIN < -200 J/kg,
///   linearly interpolated in between.
/// * **EBWD_term** = 0.0 if EBWD < 12.5 m/s, 1.5 if EBWD > 30 m/s,
///   EBWD / 20 otherwise.
///
/// # Reference
///
/// Thompson, R. L., B. T. Smith, J. S. Grams, A. R. Dean, and C. Broyles,
/// 2012: Convective modes for significant severe thunderstorms in the
/// contiguous United States. Part II: Supercell and QLCS tornado
/// environments. *Wea. Forecasting*, **27**, 1136-1154.
///
/// # Physical interpretation
///
/// An evolution of `stp_fixed` that accounts for convective inhibition and
/// uses the effective inflow layer (Thompson et al. 2007) instead of fixed
/// height layers, improving discrimination in elevated-storm environments.
pub fn stp_cin(mlcape: f64, esrh: f64, ebwd: f64, mllcl: f64, mlcinh: f64) -> Option<f64> {
    if !qc(mlcape) || !qc(esrh) || !qc(ebwd) || !qc(mllcl) || !qc(mlcinh) {
        return None;
    }

    let cape_term = mlcape / 1500.0;
    let eshr_term = esrh / 150.0;

    let ebwd_term = if ebwd < 12.5 {
        0.0
    } else if ebwd > 30.0 {
        1.5
    } else {
        ebwd / 20.0
    };

    let lcl_term = if mllcl < 1000.0 {
        1.0
    } else if mllcl > 2000.0 {
        0.0
    } else {
        (2000.0 - mllcl) / 1000.0
    };

    let cinh_term = if mlcinh > -50.0 {
        1.0
    } else if mlcinh < -200.0 {
        0.0
    } else {
        (mlcinh + 200.0) / 150.0
    };

    let val = cape_term * eshr_term * ebwd_term * lcl_term * cinh_term;
    Some(val.max(0.0))
}

// =========================================================================
// SPC beta tornado composites
// =========================================================================

fn fixed_layer_tornado_shear_term(shear6: f64) -> f64 {
    if shear6 < 12.5 {
        0.0
    } else if shear6 > 30.0 {
        1.5
    } else {
        shear6 / 20.0
    }
}

fn tornadic_low_level_limit_exceeded(mllcl: f64, mlcin: f64, sbcin: f64) -> bool {
    mllcl > 1700.0 || mlcin < -100.0 || sbcin < -200.0
}

/// Tornadic 0-1 km Energy-Helicity Index.
///
/// Mirrors the SPC beta `tehi` product:
///
/// ```text
/// TEHI = ((SRH1 * MLCAPE) / 160000) * ML3CAPE_term * 6BWD_term
/// ```
///
/// The low-level tornado gates zero the output when MLLCL is too high or
/// CIN is too strong.
pub fn tehi(
    srh01: f64,
    mlcape: f64,
    mlcape_03: f64,
    shear06: f64,
    mllcl: f64,
    mlcin: f64,
    sbcin: f64,
) -> Option<f64> {
    if !qc(srh01)
        || !qc(mlcape)
        || !qc(mlcape_03)
        || !qc(shear06)
        || !qc(mllcl)
        || !qc(mlcin)
        || !qc(sbcin)
    {
        return None;
    }

    let mut mlcape_03_term = if mlcape_03 > 300.0 {
        1.5
    } else {
        mlcape_03 / 200.0
    };
    if mlcape > 1500.0 {
        mlcape_03_term = mlcape_03_term.max(1.0);
    }

    let value =
        ((srh01 * mlcape) / 160000.0) * mlcape_03_term * fixed_layer_tornado_shear_term(shear06);

    if tornadic_low_level_limit_exceeded(mllcl, mlcin, sbcin) || value < 0.0 {
        Some(0.0)
    } else {
        Some(value)
    }
}

/// Tornadic Tilting and Stretching.
///
/// Mirrors the SPC beta `tts` product, not the Total Totals index:
///
/// ```text
/// TTS = ((SRH1 * min(ML3CAPE, 150)) / 6500) * MLCAPE_term * 6BWD_term
/// ```
pub fn tts(
    srh01: f64,
    mlcape_03: f64,
    mlcape: f64,
    shear06: f64,
    mllcl: f64,
    mlcin: f64,
    sbcin: f64,
) -> Option<f64> {
    if !qc(srh01)
        || !qc(mlcape_03)
        || !qc(mlcape)
        || !qc(shear06)
        || !qc(mllcl)
        || !qc(mlcin)
        || !qc(sbcin)
    {
        return None;
    }

    let mlcape_03_capped = mlcape_03.min(150.0);
    let mlcape_term = if mlcape < 2000.0 {
        1.0
    } else if mlcape > 3000.0 {
        1.5
    } else {
        mlcape / 2000.0
    };

    let value = ((srh01 * mlcape_03_capped) / 6500.0)
        * mlcape_term
        * fixed_layer_tornado_shear_term(shear06);

    if tornadic_low_level_limit_exceeded(mllcl, mlcin, sbcin) || value < 0.0 {
        Some(0.0)
    } else {
        Some(value)
    }
}

/// Modified Violent Tornado Parameter.
///
/// Inputs are MLCAPE, effective SRH, effective bulk wind difference, MLLCL,
/// MLCIN, 0-3 km MLCAPE, and the 700-500 hPa lapse rate.
pub fn vtp_mod(
    mlcape: f64,
    esrh: f64,
    ebwd: f64,
    mllcl: f64,
    mlcin: f64,
    mlcape_03: f64,
    lr700_500: f64,
) -> Option<f64> {
    if !qc(mlcape)
        || !qc(esrh)
        || !qc(ebwd)
        || !qc(mllcl)
        || !qc(mlcin)
        || !qc(mlcape_03)
        || !qc(lr700_500)
    {
        return None;
    }

    let ebwd_term = if ebwd <= 20.0 {
        0.0
    } else if ebwd >= 45.0 {
        1.5
    } else {
        ebwd / 30.0
    };
    let mllcl_term = if mllcl >= 1750.0 {
        0.0
    } else if mllcl <= 750.0 {
        1.0
    } else {
        (1750.0 - mllcl) / 750.0
    };
    let mlcin_term = if mlcin <= -200.0 {
        0.0
    } else if mlcin >= -50.0 {
        1.0
    } else {
        (mlcin + 200.0) / 150.0
    };
    let mlcape_03_term = if mlcape_03 >= 100.0 {
        2.0
    } else {
        mlcape_03 / 50.0
    };
    let lr_term = if lr700_500 <= 4.5 {
        0.0
    } else if lr700_500 >= 8.5 {
        2.0
    } else {
        (lr700_500 - 4.5) / 2.0
    };

    let p1 = (mlcape / 1700.0) * (esrh / 250.0) * ebwd_term * mllcl_term;
    let p2 = mlcin_term * mlcape_03_term * lr_term;
    Some(p1 * p2)
}

// =========================================================================
// Supercell Composite Parameter  (Thompson et al. 2004)
// =========================================================================

/// Supercell Composite Parameter (SCP).
///
/// # Formula
///
/// ```text
/// SCP = (MUCAPE / 1000) * (effective_SRH / 50) * (EBWD_term)
/// ```
///
/// where EBWD is clamped: 0 if < 10 m/s, capped at 20 m/s, divided by 20.
///
/// # Reference
///
/// Thompson, R. L., R. Edwards, and C. M. Mead, 2004: An update to the
/// supercell composite and significant tornado parameters. Preprints, 22nd
/// Conf. Severe Local Storms, Hyannis, MA.
///
/// # Physical interpretation
///
/// Identifies environments favouring supercell thunderstorms by combining
/// MUCAPE (instability), effective SRH (low-level mesocyclone potential),
/// and effective BWD (deep-layer shear for storm organisation).
pub fn scp(mucape: f64, srh: f64, ebwd: f64) -> Option<f64> {
    if !qc(mucape) || !qc(srh) || !qc(ebwd) {
        return None;
    }

    let ebwd_clamped = if ebwd < 10.0 {
        0.0
    } else if ebwd > 20.0 {
        20.0
    } else {
        ebwd
    };

    let mucape_term = mucape / 1000.0;
    let esrh_term = srh / 50.0;
    let ebwd_term = ebwd_clamped / 20.0;

    Some(mucape_term * esrh_term * ebwd_term)
}

// =========================================================================
// Significant Hail Parameter  (Johnson and Sugden 2014)
// =========================================================================

/// Significant Hail Parameter (SHIP).
///
/// # Formula
///
/// ```text
/// SHIP = -1 * (MUCAPE * MUMR * LR75 * T500 * SHR06) / 42_000_000
/// ```
///
/// with post-multiplier penalties when:
/// - MUCAPE < 1300 J/kg   => multiply by MUCAPE/1300
/// - LR75  < 5.8 C/km     => multiply by LR75/5.8
/// - FRZ_LVL < 2400 m     => multiply by FRZ_LVL/2400
///
/// Input clamps:
/// - SHR06 clamped to \[7, 27\] m/s
/// - MUMR  clamped to \[11, 13.6\] g/kg
/// - T500  capped at -5.5 C (values warmer than -5.5 set to -5.5)
///
/// # Reference
///
/// Johnson, A. W., and K. E. Sugden, 2014: Evaluation of sounding-derived
/// thermodynamic and wind-related parameters associated with large hail
/// events. *E-Journal of Severe Storms Meteorology*, **9(5)**.
///
/// # Physical interpretation
///
/// Identifies environments favourable for significant hail (>= 2 in
/// diameter).  Combines MUCAPE, moisture (mixing ratio at MU parcel
/// level), mid-level lapse rate (700-500 mb), 500 mb temperature (hail
/// growth zone proxy), and deep-layer shear.
///
/// # Arguments
///
/// * `mucape` - Most-Unstable CAPE (J/kg)
/// * `mumr` - Mixing ratio at the MU parcel level (g/kg)
/// * `lr75` - 700-500 mb lapse rate (C/km)
/// * `h5_temp` - 500 mb temperature (C)
/// * `shr06` - 0-6 km shear magnitude (m/s)
/// * `frz_lvl` - Freezing level height (m AGL)
pub fn ship(
    mucape: f64,
    mumr: f64,
    lr75: f64,
    h5_temp: f64,
    shr06: f64,
    frz_lvl: f64,
) -> Option<f64> {
    if !qc(mucape) || !qc(mumr) || !qc(lr75) || !qc(h5_temp) || !qc(shr06) || !qc(frz_lvl) {
        return None;
    }

    // Clamp inputs
    let shr06_c = shr06.clamp(7.0, 27.0);
    let mumr_c = mumr.clamp(11.0, 13.6);
    let h5_c = if h5_temp > -5.5 { -5.5 } else { h5_temp };

    let mut val = -1.0 * (mucape * mumr_c * lr75 * h5_c * shr06_c) / 42_000_000.0;

    // Post-multiplier penalties
    if mucape < 1300.0 {
        val *= mucape / 1300.0;
    }
    if lr75 < 5.8 {
        val *= lr75 / 5.8;
    }
    if frz_lvl < 2400.0 {
        val *= frz_lvl / 2400.0;
    }

    Some(val)
}

// =========================================================================
// 0-6 km shear magnitude  (helper used by SCP, SHIP, etc.)
// =========================================================================

/// Compute the 0-6 km bulk wind difference magnitude.
///
/// Given the U and V components of the shear vector between the surface
/// and 6 km AGL (both in knots, matching SHARPpy's `wind_shear` output),
/// returns the scalar magnitude in **m/s**.
///
/// # Arguments
///
/// * `u_shr` - U-component of the 0-6 km shear vector (knots)
/// * `v_shr` - V-component of the 0-6 km shear vector (knots)
pub fn shr_sfc_to_6km(u_shr: f64, v_shr: f64) -> Option<f64> {
    if !qc(u_shr) || !qc(v_shr) {
        return None;
    }
    let mag_kt = (u_shr * u_shr + v_shr * v_shr).sqrt();
    Some(mag_kt * KTS2MS)
}

// =========================================================================
// Enhanced Stretching Potential  (ESP)
// =========================================================================

/// Enhanced Stretching Potential (ESP).
///
/// # Formula
///
/// ```text
/// ESP = (MLCAPE_0_3km / 50) * ((LR03 - 7.0) / 1.0)
/// ```
///
/// Returns 0 when LR03 < 7 C/km **or** total MLCAPE < 250 J/kg.
///
/// # Physical interpretation
///
/// Identifies co-location of low-level buoyancy and steep low-level lapse
/// rates, which favours low-level vortex stretching and tornado potential.
/// Higher values indicate greater stretching potential.
///
/// # Arguments
///
/// * `mlcape_03` - Mixed-layer CAPE in the 0-3 km layer (J/kg)
/// * `lr03` - 0-3 km lapse rate (C/km)
/// * `total_mlcape` - Total mixed-layer CAPE (J/kg); used only for the
///   250 J/kg threshold check.
pub fn esp(mlcape_03: f64, lr03: f64, total_mlcape: f64) -> Option<f64> {
    if !qc(mlcape_03) || !qc(lr03) || !qc(total_mlcape) {
        return None;
    }
    if lr03 < 7.0 || total_mlcape < 250.0 {
        return Some(0.0);
    }
    Some((mlcape_03 / 50.0) * (lr03 - 7.0))
}

// =========================================================================
// MCS Maintenance Probability  (Coniglio et al. 2006)
// =========================================================================

/// MCS Maintenance Probability (MMP).
///
/// # Formula
///
/// ```text
/// MMP = 1 / (1 + exp(a0 + a1*max_bulk_shear + a2*lr38 + a3*mucape + a4*mean_wind_3_12))
/// ```
///
/// with coefficients:
/// - a0 =  13.0
/// - a1 = -4.59e-2   (m^-1 s)
/// - a2 = -1.16       (K^-1 km)
/// - a3 = -6.17e-4   (J^-1 kg)
/// - a4 = -0.17       (m^-1 s)
///
/// Returns 0.0 when MUCAPE < 100 J/kg.
///
/// # Reference
///
/// Coniglio, M. C., D. J. Stensrud, and L. J. Wicker, 2006: Effects of
/// upper-level shear on the structure and maintenance of strong quasi-linear
/// mesoscale convective systems. *J. Atmos. Sci.*, **63**, 1231-1251.
///
/// # Physical interpretation
///
/// Estimates the probability that a mature MCS will maintain peak intensity
/// over the next hour, based on MUCAPE, 3-8 km lapse rate, maximum bulk
/// shear between the 0-1 km and 6-10 km layers, and 3-12 km mean wind.
///
/// # Arguments
///
/// * `mucape` - Most-Unstable CAPE (J/kg)
/// * `max_bulk_shear` - Maximum bulk shear between 0-1 km and 6-10 km (m/s)
/// * `lr38` - 3-8 km lapse rate (C/km)
/// * `mean_wind_3_12` - 3-12 km mean wind speed (m/s)
pub fn mmp(mucape: f64, max_bulk_shear: f64, lr38: f64, mean_wind_3_12: f64) -> Option<f64> {
    if !qc(mucape) || !qc(max_bulk_shear) || !qc(lr38) || !qc(mean_wind_3_12) {
        return None;
    }
    if mucape < 100.0 {
        return Some(0.0);
    }

    let a0 = 13.0_f64;
    let a1 = -4.59e-2_f64;
    let a2 = -1.16_f64;
    let a3 = -6.17e-4_f64;
    let a4 = -0.17_f64;

    let exponent = a0 + a1 * max_bulk_shear + a2 * lr38 + a3 * mucape + a4 * mean_wind_3_12;
    Some(1.0 / (1.0 + exponent.exp()))
}

// =========================================================================
// Wind Damage Parameter  (WNDG)
// =========================================================================

/// Wind Damage Parameter (WNDG).
///
/// # Formula
///
/// ```text
/// WNDG = (MLCAPE / 2000) * (LR03 / 9) * (mean_wind_1_3.5km / 15) * ((50 + MLCIN) / 40)
/// ```
///
/// where LR03 is set to 0 when < 7 C/km, and MLCIN is clamped to >= -50.
///
/// # Physical interpretation
///
/// A non-dimensional composite identifying areas where large CAPE, steep
/// low-level lapse rates, enhanced low-to-mid-level flow, and minimal CIN
/// are co-located.  Values > 1 favour damaging outflow gusts with multicell
/// clusters, primarily during summer afternoons.
///
/// # Arguments
///
/// * `mlcape` - Mixed-layer CAPE (J/kg)
/// * `lr03` - 0-3 km lapse rate (C/km)
/// * `mean_wind` - 1-3.5 km mean wind speed (m/s)
/// * `mlcin` - Mixed-layer CIN (J/kg, negative values)
pub fn wndg(mlcape: f64, lr03: f64, mean_wind: f64, mlcin: f64) -> Option<f64> {
    if !qc(mlcape) || !qc(lr03) || !qc(mean_wind) || !qc(mlcin) {
        return None;
    }

    let lr03_adj = if lr03 < 7.0 { 0.0 } else { lr03 };
    let mlcin_adj = if mlcin < -50.0 { -50.0 } else { mlcin };

    Some((mlcape / 2000.0) * (lr03_adj / 9.0) * (mean_wind / 15.0) * ((50.0 + mlcin_adj) / 40.0))
}

// =========================================================================
// Derecho Composite Parameter  (Evans and Doswell 2001)
// =========================================================================

/// Derecho Composite Parameter (DCP).
///
/// # Formula
///
/// ```text
/// DCP = (DCAPE / 980) * (MUCAPE / 2000) * (shear_06 / 20) * (mean_wind_06 / 16)
/// ```
///
/// All shear and mean wind values are in **knots** (matching SHARPpy, which
/// uses `utils.mag` on the raw knot-valued wind vectors without converting).
///
/// # Reference
///
/// Evans, J. S., and C. A. Doswell, 2001: Examination of derecho
/// environments using proximity soundings. *Wea. Forecasting*, **16**,
/// 329-342.
///
/// # Physical interpretation
///
/// Identifies environments favourable for cold-pool-driven damaging wind
/// events (derechos) through four mechanisms:
/// 1. Cold pool production (DCAPE)
/// 2. Ability to sustain strong storms along a gust front (MUCAPE)
/// 3. Organisation potential (0-6 km shear)
/// 4. Sufficient ambient flow for downstream development (0-6 km mean wind)
///
/// # Arguments
///
/// * `dcape` - Downdraft CAPE (J/kg)
/// * `mucape` - Most-Unstable CAPE (J/kg)
/// * `shear_06_kt` - 0-6 km shear magnitude (knots)
/// * `mean_wind_06_kt` - 0-6 km mean wind speed (knots)
pub fn dcp(dcape: f64, mucape: f64, shear_06_kt: f64, mean_wind_06_kt: f64) -> Option<f64> {
    if !qc(dcape) || !qc(mucape) || !qc(shear_06_kt) || !qc(mean_wind_06_kt) {
        return None;
    }
    Some((dcape / 980.0) * (mucape / 2000.0) * (shear_06_kt / 20.0) * (mean_wind_06_kt / 16.0))
}

// =========================================================================
// Microburst Composite Index  (Entremont, NWS JAN; Thompson SPC)
// =========================================================================

/// Microburst Composite Index.
///
/// A weighted sum of categorical terms derived from:
/// - Surface-based CAPE
/// - Surface-based Lifted Index (LI at 500 mb)
/// - 0-3 km lapse rate
/// - Vertical Totals (T850 - T500)
/// - DCAPE
/// - Precipitable water
/// - Theta-E difference (max - min in lowest 3 km, positive = max below min)
/// - Surface Theta-E (flag if >= 355 K)
///
/// Interpretation: 3-4 = slight chance, 5-8 = chance, >= 9 = likely.
/// The result is floored at 0.
///
/// # Arguments
///
/// * `sbcape` - Surface-based CAPE (J/kg)
/// * `sbli` - Surface-based lifted index at 500 mb (C)
/// * `lr03` - 0-3 km lapse rate (C/km)
/// * `vt` - Vertical totals: T_850 - T_500 (C)
/// * `dcape` - Downdraft CAPE (J/kg)
/// * `pwat` - Precipitable water (inches)
/// * `thetae_diff` - Max minus min theta-e in lowest 3 km (K);
///   only counted when max is below min (positive sense).
/// * `sfc_thetae` - Surface theta-e (K)
pub fn mburst(
    sbcape: f64,
    sbli: f64,
    lr03: f64,
    vt: f64,
    dcape: f64,
    pwat: f64,
    thetae_diff: f64,
    sfc_thetae: f64,
) -> Option<i32> {
    // Check all inputs
    if !qc(sbcape)
        || !qc(sbli)
        || !qc(lr03)
        || !qc(vt)
        || !qc(dcape)
        || !qc(pwat)
        || !qc(thetae_diff)
        || !qc(sfc_thetae)
    {
        return None;
    }

    // Surface Theta-E term (note: SHARPpy converts to Kelvin via ctok)
    let te_term: i32 = if (sfc_thetae + crate::constants::ZEROCNK) >= 355.0 {
        1
    } else {
        0
    };

    // SBCAPE term
    let sbcape_term: i32 = if sbcape >= 4300.0 {
        4
    } else if sbcape >= 3700.0 {
        2
    } else if sbcape >= 3300.0 {
        1
    } else if sbcape >= 2000.0 {
        0
    } else {
        -5
    };

    // SBLI term
    let sbli_term: i32 = if sbli <= -10.0 {
        3
    } else if sbli <= -9.0 {
        2
    } else if sbli <= -7.5 {
        1
    } else {
        0
    };

    // PWAT term
    let pwat_term: i32 = if pwat < 1.5 { -3 } else { 0 };

    // DCAPE term (only counts when PWAT > 1.70)
    let dcape_term: i32 = if pwat > 1.70 && dcape > 900.0 { 1 } else { 0 };

    // Lapse rate term
    let lr03_term: i32 = if lr03 > 8.4 { 1 } else { 0 };

    // Vertical Totals term
    let vt_term: i32 = if vt >= 29.0 {
        3
    } else if vt >= 28.0 {
        2
    } else if vt >= 27.0 {
        1
    } else {
        0
    };

    // Theta-E difference term
    let ted_term: i32 = if thetae_diff >= 35.0 { 1 } else { 0 };

    let total =
        te_term + sbcape_term + sbli_term + pwat_term + dcape_term + lr03_term + vt_term + ted_term;

    Some(total.max(0))
}

// =========================================================================
// Energy-Helicity Index  (EHI)
// =========================================================================

/// Energy-Helicity Index (EHI).
///
/// # Formula
///
/// ```text
/// EHI = (CAPE * helicity) / 160_000
/// ```
///
/// # Physical interpretation
///
/// Combines buoyancy (CAPE) and low-level rotational potential
/// (storm-relative helicity) into a single tornado-threat discriminator.
/// Typically computed for the 0-1 km or 0-3 km SRH layers.
///
/// # Arguments
///
/// * `cape` - CAPE from any parcel (J/kg)
/// * `helicity` - Storm-relative helicity for the chosen layer (m^2/s^2)
pub fn ehi(cape: f64, helicity: f64) -> Option<f64> {
    if !qc(cape) || !qc(helicity) {
        return None;
    }
    Some((helicity * cape) / 160_000.0)
}

// =========================================================================
// SWEAT Index  (Miller 1972)
// =========================================================================

/// Severe Weather Threat (SWEAT) Index.
///
/// # Formula
///
/// ```text
/// SWEAT = term1 + term2 + term3 + term4 + term5
/// ```
///
/// where:
/// - term1 = 12 * Td_850   (0 if Td_850 < 0)
/// - term2 = 20 * (TT - 49)  (0 if TT < 49)
/// - term3 = 2 * speed_850  (knots)
/// - term4 = speed_500      (knots)
/// - term5 = 125 * (sin(dir500 - dir850) + 0.2) when all of:
///   - 130 <= dir850 <= 250
///   - 210 <= dir500 <= 310
///   - dir500 - dir850 > 0
///   - speed850 >= 15 kt
///   - speed500 >= 15 kt
///   Otherwise term5 = 0.
///
/// # Reference
///
/// Miller, R. C., 1972: Notes on Analysis and Severe-Storm Forecasting
/// Procedures of the Air Force Global Weather Central. Air Weather Service
/// Tech. Report 200 (Rev.).
///
/// # Physical interpretation
///
/// A classic composite index combining low-level moisture, instability
/// (Total Totals), wind speeds at 850 and 500 mb, and veering between
/// those levels to identify severe thunderstorm potential.
///
/// # Arguments
///
/// * `td850` - 850 mb dewpoint (C)
/// * `tt` - Total Totals index (C)
/// * `wdir850` - 850 mb wind direction (degrees)
/// * `wspd850` - 850 mb wind speed (knots)
/// * `wdir500` - 500 mb wind direction (degrees)
/// * `wspd500` - 500 mb wind speed (knots)
pub fn sweat(
    td850: f64,
    tt: f64,
    wdir850: f64,
    wspd850: f64,
    wdir500: f64,
    wspd500: f64,
) -> Option<f64> {
    if !qc(td850) || !qc(tt) || !qc(wdir850) || !qc(wspd850) || !qc(wdir500) || !qc(wspd500) {
        return None;
    }

    let term1 = if td850 > 0.0 { 12.0 * td850 } else { 0.0 };
    let term2 = if tt < 49.0 { 0.0 } else { 20.0 * (tt - 49.0) };
    let term3 = 2.0 * wspd850;
    let term4 = wspd500;

    let term5 = if (130.0..=250.0).contains(&wdir850)
        && (210.0..=310.0).contains(&wdir500)
        && (wdir500 - wdir850) > 0.0
        && wspd850 >= 15.0
        && wspd500 >= 15.0
    {
        125.0 * (((wdir500 - wdir850) * PI / 180.0).sin() + 0.2)
    } else {
        0.0
    };

    Some(term1 + term2 + term3 + term4 + term5)
}

// =========================================================================
// Theta-E Index  (TEI)
// =========================================================================

/// Theta-E Index (TEI).
///
/// # Formula
///
/// ```text
/// TEI = max_theta_e - min_theta_e   (in lowest 400 mb AGL)
/// ```
///
/// Note: the SPC online soundings use max minus min (not surface minus min)
/// and SHARPpy follows the online behaviour.
///
/// # Physical interpretation
///
/// Measures the vertical instability of the theta-e profile in the lowest
/// 400 mb.  Larger values indicate greater potential instability and
/// susceptibility to convective overturning once a layer is lifted.
///
/// # Arguments
///
/// * `max_thetae` - Maximum theta-e in the lowest 400 mb (K)
/// * `min_thetae` - Minimum theta-e in the lowest 400 mb (K)
pub fn tei(max_thetae: f64, min_thetae: f64) -> Option<f64> {
    if !qc(max_thetae) || !qc(min_thetae) {
        return None;
    }
    Some(max_thetae - min_thetae)
}

// =========================================================================
// SHERB  (Sherburn et al. 2014)
// =========================================================================

/// Severe Hazards in Environments with Reduced Buoyancy (SHERB).
///
/// # Formula (non-effective)
///
/// ```text
/// SHERB = (shear_03 / 26) * (LR03 / 5.2) * (LR75 / 5.6)
/// ```
///
/// # Formula (effective layer)
///
/// ```text
/// SHERB_eff = (eff_shear / 27) * (LR03 / 5.2) * (LR75 / 5.6)
/// ```
///
/// The only difference between the two variants is the shear denominator
/// (26 for fixed 0-3 km, 27 for effective layer).
///
/// # Reference
///
/// Sherburn, K. D., M. D. Parker, J. R. King, and G. M. Lackmann, 2014:
/// Composite environments of severe and nonsevere high-shear, low-CAPE
/// convective events. *Wea. Forecasting*, **29**, 899-920.
///
/// # Physical interpretation
///
/// Designed for High-Shear Low-CAPE (HSLC) environments to discriminate
/// between significant severe and non-severe convection.  Values above 1
/// are more likely associated with significant severe weather (tornadoes
/// and/or significant wind).
///
/// # Arguments
///
/// * `shear` - Shear magnitude (m/s): 0-3 km for non-effective, effective
///   BWD for effective variant.
/// * `lr03` - 0-3 km lapse rate (C/km)
/// * `lr75` - 700-500 mb lapse rate (C/km)
/// * `effective` - If `true`, use the effective-layer denominator (27);
///   otherwise use the fixed-layer denominator (26).
pub fn sherb(shear: f64, lr03: f64, lr75: f64, effective: bool) -> Option<f64> {
    if !qc(shear) || !qc(lr03) || !qc(lr75) {
        return None;
    }
    let shear_denom = if effective { 27.0 } else { 26.0 };
    Some((shear / shear_denom) * (lr03 / 5.2) * (lr75 / 5.6))
}

// =========================================================================
// Modified SHERBE (MOSHE)
// =========================================================================

/// Modified SHERBE (MOSHE).
///
/// MOSHE is a community modification of SHERB that adds a low-level shear
/// term to better capture tornado potential in HSLC environments.  It is
/// not present in the SHARPpy codebase but is included here for
/// completeness as it appears in the research literature (Sherburn and
/// Parker 2014, WAF).
///
/// # Formula
///
/// ```text
/// MOSHE = SHERB_eff * (0-1 km shear / 15)
/// ```
///
/// where SHERB_eff is the effective-layer variant of SHERB.
///
/// # Arguments
///
/// * `eff_shear` - Effective BWD shear magnitude (m/s)
/// * `lr03` - 0-3 km lapse rate (C/km)
/// * `lr75` - 700-500 mb lapse rate (C/km)
/// * `shear_01` - 0-1 km shear magnitude (m/s)
pub fn moshe(eff_shear: f64, lr03: f64, lr75: f64, shear_01: f64) -> Option<f64> {
    let sherb_val = sherb(eff_shear, lr03, lr75, true)?;
    if !qc(shear_01) {
        return None;
    }
    Some(sherb_val * (shear_01 / 15.0))
}

// =========================================================================
// Significant Severe  (Craven and Brooks 2004)
// =========================================================================

/// Significant Severe parameter (Craven and Brooks 2004).
///
/// # Formula
///
/// ```text
/// SigSevere = MLCAPE * SHR06
/// ```
///
/// Units are m^3/s^3 (J/kg * m/s).
///
/// # Arguments
///
/// * `mlcape` - Mixed-layer CAPE (J/kg)
/// * `shr06` - 0-6 km shear magnitude (m/s)
pub fn sig_severe(mlcape: f64, shr06: f64) -> Option<f64> {
    if !qc(mlcape) || !qc(shr06) {
        return None;
    }
    Some(mlcape * shr06)
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f64 = 1e-6;

    fn assert_close(actual: f64, expected: f64, eps: f64, name: &str) {
        assert!(
            (actual - expected).abs() < eps,
            "{name}: expected {expected}, got {actual}"
        );
    }

    // ---- STP fixed ----

    #[test]
    fn stp_fixed_classic_case() {
        // SBCAPE=2500, LCL=800m, SRH01=250, BWD6=25 m/s
        // cape_term = 2500/1500 = 1.6667
        // lcl_term = 1.0 (< 1000m)
        // srh_term = 250/150 = 1.6667
        // bwd6_term = 25/20 = 1.25
        let result = stp_fixed(2500.0, 800.0, 250.0, 25.0).unwrap();
        let expected = (2500.0 / 1500.0) * 1.0 * (250.0 / 150.0) * (25.0 / 20.0);
        assert!((result - expected).abs() < EPS);
    }

    #[test]
    fn stp_fixed_high_lcl() {
        // LCL at 1500m => lcl_term = (2000-1500)/1000 = 0.5
        let result = stp_fixed(1500.0, 1500.0, 150.0, 20.0).unwrap();
        let expected = 1.0 * 0.5 * 1.0 * 1.0;
        assert!((result - expected).abs() < EPS);
    }

    #[test]
    fn stp_fixed_low_shear() {
        // BWD6 = 10 m/s (< 12.5) => bwd6_term = 0 => STP = 0
        let result = stp_fixed(3000.0, 500.0, 300.0, 10.0).unwrap();
        assert!((result - 0.0).abs() < EPS);
    }

    #[test]
    fn stp_fixed_lcl_above_2000() {
        // LCL > 2000 m => lcl_term = 0 => STP = 0
        let result = stp_fixed(3000.0, 2500.0, 300.0, 25.0).unwrap();
        assert!((result - 0.0).abs() < EPS);
    }

    #[test]
    fn stp_fixed_capped_bwd() {
        // BWD6 = 40 m/s => clamped to 30 => term = 30/20 = 1.5
        let result = stp_fixed(1500.0, 800.0, 150.0, 40.0).unwrap();
        let expected = 1.0 * 1.0 * 1.0 * 1.5;
        assert!((result - expected).abs() < EPS);
    }

    #[test]
    fn stp_fixed_missing_input() {
        assert!(stp_fixed(crate::constants::MISSING, 800.0, 150.0, 20.0).is_none());
    }

    // ---- STP CIN ----

    #[test]
    fn stp_cin_classic() {
        // MLCAPE=2000, ESRH=200, EBWD=20, MLLCL=900, MLCINH=-30
        let result = stp_cin(2000.0, 200.0, 20.0, 900.0, -30.0).unwrap();
        let expected = (2000.0 / 1500.0) * (200.0 / 150.0) * (20.0 / 20.0) * 1.0 * 1.0;
        assert!((result - expected).abs() < EPS);
    }

    #[test]
    fn stp_cin_low_shear() {
        // EBWD < 12.5 => ebwd_term = 0 => result = 0
        let result = stp_cin(2000.0, 200.0, 10.0, 900.0, -30.0).unwrap();
        assert!((result - 0.0).abs() < EPS);
    }

    #[test]
    fn stp_cin_high_cin() {
        // MLCINH = -250 (< -200) => cinh_term = 0 => result = 0
        let result = stp_cin(2000.0, 200.0, 20.0, 900.0, -250.0).unwrap();
        assert!((result - 0.0).abs() < EPS);
    }

    #[test]
    fn stp_cin_intermediate_cin() {
        // MLCINH = -125 => cinh_term = (-125+200)/150 = 0.5
        let result = stp_cin(1500.0, 150.0, 20.0, 800.0, -125.0).unwrap();
        let expected = 1.0 * 1.0 * 1.0 * 1.0 * 0.5;
        assert!((result - expected).abs() < EPS);
    }

    #[test]
    fn stp_cin_high_lcl() {
        // MLLCL = 1500 => lcl_term = 0.5
        let result = stp_cin(1500.0, 150.0, 20.0, 1500.0, -30.0).unwrap();
        let expected = 1.0 * 1.0 * 1.0 * 0.5 * 1.0;
        assert!((result - expected).abs() < EPS);
    }

    #[test]
    fn stp_cin_nonnegative() {
        // Negative SRH should still produce >= 0
        let result = stp_cin(1500.0, -100.0, 20.0, 800.0, -30.0).unwrap();
        assert!(result >= 0.0);
    }

    // ---- SCP ----

    #[test]
    fn scp_classic() {
        // MUCAPE=3000, SRH=200, EBWD=18
        let result = scp(3000.0, 200.0, 18.0).unwrap();
        let expected = (3000.0 / 1000.0) * (200.0 / 50.0) * (18.0 / 20.0);
        assert!((result - expected).abs() < EPS);
    }

    #[test]
    fn scp_low_shear() {
        // EBWD < 10 => term = 0 => SCP = 0
        let result = scp(3000.0, 200.0, 8.0).unwrap();
        assert!((result - 0.0).abs() < EPS);
    }

    #[test]
    fn scp_capped_shear() {
        // EBWD = 25 > 20 => clamped to 20 => term = 1.0
        let result = scp(1000.0, 50.0, 25.0).unwrap();
        let expected = 1.0 * 1.0 * 1.0;
        assert!((result - expected).abs() < EPS);
    }

    // ---- SPC beta tornado composites ----

    #[test]
    fn tehi_matches_spc_beta_formula() {
        let result = tehi(200.0, 1000.0, 100.0, 20.0, 1000.0, -50.0, -50.0).unwrap();
        assert_close(result, 0.625, 1e-12, "tehi formula");
    }

    #[test]
    fn tehi_uses_mlcape3_floor_when_total_mlcape_is_large() {
        let result = tehi(160.0, 1600.0, 50.0, 20.0, 1000.0, -50.0, -50.0).unwrap();
        assert_close(result, 1.6, 1e-12, "tehi mlcape3 floor");
    }

    #[test]
    fn tehi_applies_limiters_and_shear_gate() {
        let cases = [
            tehi(200.0, 1000.0, 100.0, 10.0, 1000.0, -50.0, -50.0).unwrap(),
            tehi(200.0, 1000.0, 100.0, 20.0, 1800.0, -50.0, -50.0).unwrap(),
            tehi(200.0, 1000.0, 100.0, 20.0, 1000.0, -150.0, -50.0).unwrap(),
            tehi(200.0, 1000.0, 100.0, 20.0, 1000.0, -50.0, -250.0).unwrap(),
        ];
        for value in cases {
            assert_close(value, 0.0, 1e-12, "tehi limiter");
        }
    }

    #[test]
    fn tts_matches_spc_beta_formula() {
        let result = tts(100.0, 100.0, 2500.0, 20.0, 1000.0, -50.0, -50.0).unwrap();
        assert_close(result, 1.9230769230769231, 1e-12, "tts formula");
    }

    #[test]
    fn tts_uses_mlcape_floor_and_caps_mlcape3_and_shear() {
        let result = tts(100.0, 200.0, 1500.0, 35.0, 1000.0, -50.0, -50.0).unwrap();
        assert_close(result, 3.4615384615384617, 1e-12, "tts caps");
    }

    #[test]
    fn tts_applies_limiters_and_negative_floor() {
        let cases = [
            tts(-100.0, 100.0, 2500.0, 20.0, 1000.0, -50.0, -50.0).unwrap(),
            tts(100.0, 100.0, 2500.0, 20.0, 1800.0, -50.0, -50.0).unwrap(),
            tts(100.0, 100.0, 2500.0, 20.0, 1000.0, -150.0, -50.0).unwrap(),
            tts(100.0, 100.0, 2500.0, 20.0, 1000.0, -50.0, -250.0).unwrap(),
        ];
        for value in cases {
            assert_close(value, 0.0, 1e-12, "tts limiter");
        }
    }

    #[test]
    fn vtp_mod_matches_in_range_formula() {
        let result = vtp_mod(1700.0, 250.0, 30.0, 1000.0, -100.0, 50.0, 6.5).unwrap();
        assert_close(result, 2.0 / 3.0, 1e-12, "vtp_mod formula");
    }

    #[test]
    fn vtp_mod_applies_component_cutoffs_and_caps() {
        assert_close(
            vtp_mod(1700.0, 250.0, 20.0, 1000.0, -50.0, 50.0, 6.5).unwrap(),
            0.0,
            1e-12,
            "vtp ebwd cutoff",
        );
        assert_close(
            vtp_mod(1700.0, 250.0, 45.0, 1000.0, -50.0, 50.0, 6.5).unwrap(),
            1.5,
            1e-12,
            "vtp ebwd cap",
        );
        assert_close(
            vtp_mod(1700.0, 250.0, 30.0, 1750.0, -50.0, 50.0, 6.5).unwrap(),
            0.0,
            1e-12,
            "vtp lcl cutoff",
        );
        assert_close(
            vtp_mod(1700.0, 250.0, 30.0, 1000.0, -200.0, 50.0, 6.5).unwrap(),
            0.0,
            1e-12,
            "vtp cin cutoff",
        );
        assert_close(
            vtp_mod(1700.0, 250.0, 30.0, 1000.0, -50.0, 100.0, 6.5).unwrap(),
            2.0,
            1e-12,
            "vtp ml3cape cap",
        );
        assert_close(
            vtp_mod(1700.0, 250.0, 30.0, 1000.0, -50.0, 50.0, 8.5).unwrap(),
            2.0,
            1e-12,
            "vtp lapse cap",
        );
    }

    // ---- SHIP ----

    #[test]
    fn ship_typical() {
        // MUCAPE=2500, MUMR=12, LR75=7.0, T500=-15, SHR06=20, FRZ=3500
        // shr06_c=20, mumr_c=12, h5_c=-15
        // ship = -1*(2500*12*7.0*(-15)*20)/42e6
        // = -1*(-63000000)/42e6 = 1.5
        // no penalties (mucape>1300, lr75>5.8, frz>2400)
        let result = ship(2500.0, 12.0, 7.0, -15.0, 20.0, 3500.0).unwrap();
        let expected = -1.0 * (2500.0 * 12.0 * 7.0 * (-15.0) * 20.0) / 42_000_000.0;
        assert!((result - expected).abs() < EPS);
    }

    #[test]
    fn ship_low_cape_penalty() {
        // MUCAPE = 1000 < 1300 => extra factor of 1000/1300
        let result = ship(1000.0, 12.0, 7.0, -15.0, 20.0, 3500.0).unwrap();
        let base = -1.0 * (1000.0 * 12.0 * 7.0 * (-15.0) * 20.0) / 42_000_000.0;
        let expected = base * (1000.0 / 1300.0);
        assert!((result - expected).abs() < EPS);
    }

    #[test]
    fn ship_clamps_inputs() {
        // SHR06=5 (< 7 => clamped to 7), MUMR=15 (> 13.6 => clamped to 13.6),
        // T500=-3 (> -5.5 => set to -5.5)
        let result = ship(2000.0, 15.0, 7.0, -3.0, 5.0, 3500.0).unwrap();
        let expected = -1.0 * (2000.0 * 13.6 * 7.0 * (-5.5) * 7.0) / 42_000_000.0;
        assert!((result - expected).abs() < EPS);
    }

    #[test]
    fn ship_low_freezing_level() {
        // FRZ = 2000 < 2400 => extra factor of 2000/2400
        let result = ship(2000.0, 12.0, 7.0, -15.0, 20.0, 2000.0).unwrap();
        let base = -1.0 * (2000.0 * 12.0 * 7.0 * (-15.0) * 20.0) / 42_000_000.0;
        let expected = base * (2000.0 / 2400.0);
        assert!((result - expected).abs() < EPS);
    }

    // ---- shr_sfc_to_6km ----

    #[test]
    fn shr_6km_basic() {
        // 40 kt shear => sqrt(40^2+0^2)*0.514444 = 20.578 m/s
        let result = shr_sfc_to_6km(40.0, 0.0).unwrap();
        let expected = 40.0 * KTS2MS;
        assert!((result - expected).abs() < EPS);
    }

    #[test]
    fn shr_6km_components() {
        let result = shr_sfc_to_6km(30.0, 40.0).unwrap();
        let expected = 50.0 * KTS2MS; // 3-4-5 triangle
        assert!((result - expected).abs() < EPS);
    }

    // ---- ESP ----

    #[test]
    fn esp_favourable() {
        // mlcape_03=200, lr03=8.5, total=500
        let result = esp(200.0, 8.5, 500.0).unwrap();
        let expected = (200.0 / 50.0) * (8.5 - 7.0);
        assert!((result - expected).abs() < EPS);
    }

    #[test]
    fn esp_low_lapse_rate() {
        let result = esp(200.0, 6.5, 500.0).unwrap();
        assert!((result - 0.0).abs() < EPS);
    }

    #[test]
    fn esp_low_cape() {
        let result = esp(200.0, 8.5, 200.0).unwrap();
        assert!((result - 0.0).abs() < EPS);
    }

    // ---- MMP ----

    #[test]
    fn mmp_low_cape() {
        let result = mmp(50.0, 30.0, 7.0, 15.0).unwrap();
        assert!((result - 0.0).abs() < EPS);
    }

    #[test]
    fn mmp_regression() {
        // Known calculation:
        // exponent = 13 + (-0.0459*30) + (-1.16*7.5) + (-0.000617*2000) + (-0.17*12)
        //          = 13 - 1.377 - 8.7 - 1.234 - 2.04 = -0.351
        // mmp = 1/(1+exp(-0.351)) = 1/(1+0.7039) = 0.5869
        let result = mmp(2000.0, 30.0, 7.5, 12.0).unwrap();
        let exponent: f64 =
            13.0 + (-0.0459 * 30.0) + (-1.16 * 7.5) + (-0.000617 * 2000.0) + (-0.17 * 12.0);
        let expected = 1.0 / (1.0 + exponent.exp());
        assert!((result - expected).abs() < EPS);
    }

    // ---- WNDG ----

    #[test]
    fn wndg_above_threshold() {
        // mlcape=3000, lr03=8.0, mean_wind=12, mlcin=-20
        let result = wndg(3000.0, 8.0, 12.0, -20.0).unwrap();
        let expected = (3000.0 / 2000.0) * (8.0 / 9.0) * (12.0 / 15.0) * ((50.0 - 20.0) / 40.0);
        assert!((result - expected).abs() < EPS);
    }

    #[test]
    fn wndg_low_lapse_rate() {
        // lr03 < 7 => lr03_adj = 0 => WNDG = 0
        let result = wndg(3000.0, 6.5, 12.0, -20.0).unwrap();
        assert!((result - 0.0).abs() < EPS);
    }

    #[test]
    fn wndg_high_cin() {
        // mlcin = -80 < -50 => clamped to -50 => cin_term = 0/40 = 0
        let result = wndg(3000.0, 8.0, 12.0, -80.0).unwrap();
        let expected = (3000.0 / 2000.0) * (8.0 / 9.0) * (12.0 / 15.0) * (0.0 / 40.0);
        assert!((result - expected).abs() < EPS);
    }

    // ---- DCP ----

    #[test]
    fn dcp_classic() {
        let result = dcp(980.0, 2000.0, 20.0, 16.0).unwrap();
        assert!((result - 1.0).abs() < EPS);
    }

    #[test]
    fn dcp_double() {
        let result = dcp(1960.0, 2000.0, 20.0, 16.0).unwrap();
        assert!((result - 2.0).abs() < EPS);
    }

    // ---- Microburst Composite ----

    #[test]
    fn mburst_all_low() {
        // sbcape=1000 (<2000 => -5), sbli=0 (>-7.5 => 0), lr03=7 (<=8.4 => 0),
        // vt=25 (<27 => 0), dcape=500, pwat=1.0 (<1.5 => -3),
        // thetae_diff=10 (<35 => 0), sfc_thetae=60 (60+273.15=333.15 <355 => 0)
        // total = 0 + (-5) + 0 + (-3) + 0 + 0 + 0 + 0 = -8 => clamped to 0
        let result = mburst(1000.0, 0.0, 7.0, 25.0, 500.0, 1.0, 10.0, 60.0).unwrap();
        assert_eq!(result, 0);
    }

    #[test]
    fn mburst_high_risk() {
        // sbcape=4500 (>=4300 => 4), sbli=-11 (<=-10 => 3), lr03=9.0 (>8.4 => 1),
        // vt=30 (>=29 => 3), dcape=1000 (pwat>1.7, dcape>900 => 1),
        // pwat=2.0 (>=1.5 => 0), thetae_diff=40 (>=35 => 1),
        // sfc_thetae=85 (85+273.15=358.15 >=355 => 1)
        // total = 1 + 4 + 3 + 0 + 1 + 1 + 3 + 1 = 14
        let result = mburst(4500.0, -11.0, 9.0, 30.0, 1000.0, 2.0, 40.0, 85.0).unwrap();
        assert_eq!(result, 14);
    }

    // ---- EHI ----

    #[test]
    fn ehi_classic() {
        let result = ehi(2000.0, 200.0).unwrap();
        let expected = (200.0 * 2000.0) / 160_000.0;
        assert!((result - expected).abs() < EPS);
    }

    #[test]
    fn ehi_zero_cape() {
        let result = ehi(0.0, 200.0).unwrap();
        assert!((result - 0.0).abs() < EPS);
    }

    // ---- SWEAT ----

    #[test]
    fn sweat_no_veering_term() {
        // td850=15, tt=55, dir850=180, spd850=20, dir500=270, spd500=30
        // All directional criteria met: 130<=180<=250, 210<=270<=310,
        // 270-180=90>0, spd850>=15, spd500>=15
        let result = sweat(15.0, 55.0, 180.0, 20.0, 270.0, 30.0).unwrap();
        let term1 = 12.0 * 15.0;
        let term2 = 20.0 * (55.0 - 49.0);
        let term3 = 2.0 * 20.0;
        let term4 = 30.0;
        let term5 = 125.0 * (((270.0 - 180.0) * PI / 180.0).sin() + 0.2);
        let expected = term1 + term2 + term3 + term4 + term5;
        assert!((result - expected).abs() < EPS);
    }

    #[test]
    fn sweat_negative_dewpoint() {
        // td850 < 0 => term1 = 0
        let result = sweat(-5.0, 55.0, 100.0, 10.0, 200.0, 10.0).unwrap();
        // term1=0, term2=20*(55-49)=120, term3=20, term4=10, term5=0 (dir out of range)
        let expected = 0.0 + 120.0 + 20.0 + 10.0 + 0.0;
        assert!((result - expected).abs() < EPS);
    }

    #[test]
    fn sweat_low_tt() {
        // TT < 49 => term2 = 0
        let result = sweat(10.0, 45.0, 180.0, 20.0, 270.0, 30.0).unwrap();
        let term1 = 12.0 * 10.0;
        let term2 = 0.0;
        let term3 = 2.0 * 20.0;
        let term4 = 30.0;
        let term5 = 125.0 * (((270.0 - 180.0) * PI / 180.0).sin() + 0.2);
        let expected = term1 + term2 + term3 + term4 + term5;
        assert!((result - expected).abs() < EPS);
    }

    // ---- TEI ----

    #[test]
    fn tei_basic() {
        let result = tei(350.0, 320.0).unwrap();
        assert!((result - 30.0).abs() < EPS);
    }

    // ---- SHERB ----

    #[test]
    fn sherb_fixed() {
        // shear=20 m/s, lr03=6.0, lr75=6.5
        let result = sherb(20.0, 6.0, 6.5, false).unwrap();
        let expected = (20.0 / 26.0) * (6.0 / 5.2) * (6.5 / 5.6);
        assert!((result - expected).abs() < EPS);
    }

    #[test]
    fn sherb_effective() {
        let result = sherb(20.0, 6.0, 6.5, true).unwrap();
        let expected = (20.0 / 27.0) * (6.0 / 5.2) * (6.5 / 5.6);
        assert!((result - expected).abs() < EPS);
    }

    // ---- MOSHE ----

    #[test]
    fn moshe_basic() {
        let result = moshe(20.0, 6.0, 6.5, 12.0).unwrap();
        let sherb_val = (20.0 / 27.0) * (6.0 / 5.2) * (6.5 / 5.6);
        let expected = sherb_val * (12.0 / 15.0);
        assert!((result - expected).abs() < EPS);
    }

    // ---- Sig Severe ----

    #[test]
    fn sig_severe_basic() {
        let result = sig_severe(2000.0, 25.0).unwrap();
        assert!((result - 50_000.0).abs() < EPS);
    }

    #[test]
    fn sig_severe_missing() {
        assert!(sig_severe(crate::constants::MISSING, 25.0).is_none());
    }

    // ---- Edge cases: all missing ----

    #[test]
    fn missing_propagation() {
        let m = crate::constants::MISSING;
        assert!(stp_fixed(m, m, m, m).is_none());
        assert!(stp_cin(m, m, m, m, m).is_none());
        assert!(scp(m, m, m).is_none());
        assert!(ship(m, m, m, m, m, m).is_none());
        assert!(ehi(m, m).is_none());
        assert!(sweat(m, m, m, m, m, m).is_none());
        assert!(tei(m, m).is_none());
        assert!(sherb(m, m, m, false).is_none());
        assert!(moshe(m, m, m, m).is_none());
        assert!(wndg(m, m, m, m).is_none());
        assert!(dcp(m, m, m, m).is_none());
        assert!(esp(m, m, m).is_none());
        assert!(mmp(m, m, m, m).is_none());
        assert!(sig_severe(m, m).is_none());
    }

    // ---- NaN propagation ----

    #[test]
    fn nan_propagation() {
        assert!(stp_fixed(f64::NAN, 800.0, 150.0, 20.0).is_none());
        assert!(scp(f64::NAN, 200.0, 18.0).is_none());
        assert!(ehi(f64::NAN, 200.0).is_none());
    }
}
