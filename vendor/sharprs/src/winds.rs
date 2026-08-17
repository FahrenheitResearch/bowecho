//! Wind analysis routines: mean wind, shear, helicity, storm motion.
//!
//! Port of SHARPpy's `sharptab/winds.py`.
//!
//! All wind speeds are in **knots** and directions in **meteorological degrees**
//! unless stated otherwise.  Helicity is returned in m²/s² (the standard
//! meteorological unit), with an internal kts→m/s conversion matching SHARPpy.

use crate::constants::TOL;
use crate::error::SharpError;
use crate::profile::{is_valid, Profile};
use crate::utils::{kts2ms, ms2kts};

// =========================================================================
// Internal helpers
// =========================================================================

/// Count valid wind observations in the profile.
fn wind_count(prof: &Profile) -> usize {
    prof.u
        .iter()
        .zip(prof.v.iter())
        .filter(|(&u, &v)| u.is_finite() && v.is_finite())
        .count()
}

/// Check that there is at least one valid wind observation.
fn require_wind(prof: &Profile) -> Result<(), SharpError> {
    if wind_count(prof) == 0 {
        Err(SharpError::NoData { field: "wind" })
    } else {
        Ok(())
    }
}

/// Require a value to be finite and not MISSING.
fn require_valid(v: f64, name: &str) -> Result<(), SharpError> {
    if !is_valid(v) {
        Err(SharpError::InvalidInput(format!(
            "{name} is missing or invalid"
        )))
    } else {
        Ok(())
    }
}

/// Vector magnitude from raw f64 components.
#[inline]
fn mag_f64(u: f64, v: f64) -> f64 {
    (u * u + v * v).sqrt()
}

// =========================================================================
// Mean wind
// =========================================================================

/// Pressure-weighted mean wind (U, V in kts) through a layer.
///
/// Interpolates wind components at `dp`-hPa increments from `pbot` down to
/// `ptop`, then computes a pressure-weighted average.  Storm-motion components
/// `(stu, stv)` in kts are subtracted from the result.
///
/// Matches SHARPpy `mean_wind()`.
pub fn mean_wind(
    prof: &Profile,
    pbot: f64,
    ptop: f64,
    dp: f64,
    stu: f64,
    stv: f64,
) -> Result<(f64, f64), SharpError> {
    require_wind(prof)?;
    require_valid(pbot, "pbot")?;
    require_valid(ptop, "ptop")?;

    let dp = if dp > 0.0 { -dp } else { dp };

    // Build pressure levels from pbot descending to ptop
    let mut sum_u = 0.0;
    let mut sum_v = 0.0;
    let mut sum_w = 0.0;

    let mut p = pbot;
    while p >= ptop + dp - 0.0001 {
        let (ui, vi) = prof.interp_wind(p);
        if ui.is_finite() && vi.is_finite() {
            sum_u += ui * p;
            sum_v += vi * p;
            sum_w += p;
        }
        p += dp;
    }

    if sum_w < TOL {
        return Err(SharpError::NoData { field: "wind" });
    }

    Ok((sum_u / sum_w - stu, sum_v / sum_w - stv))
}

/// Non-pressure-weighted mean wind (U, V in kts) through a layer.
///
/// Simple arithmetic mean of wind components interpolated at `dp`-hPa
/// increments.  Matches SHARPpy `mean_wind_npw()`.
pub fn mean_wind_npw(
    prof: &Profile,
    pbot: f64,
    ptop: f64,
    dp: f64,
    stu: f64,
    stv: f64,
) -> Result<(f64, f64), SharpError> {
    require_wind(prof)?;
    require_valid(pbot, "pbot")?;
    require_valid(ptop, "ptop")?;

    let dp = if dp > 0.0 { -dp } else { dp };

    let mut sum_u = 0.0;
    let mut sum_v = 0.0;
    let mut n = 0u32;

    let mut p = pbot;
    while p >= ptop + dp - 0.0001 {
        let (ui, vi) = prof.interp_wind(p);
        if ui.is_finite() && vi.is_finite() {
            sum_u += ui;
            sum_v += vi;
            n += 1;
        }
        p += dp;
    }

    if n == 0 {
        return Err(SharpError::NoData { field: "wind" });
    }

    Ok((sum_u / n as f64 - stu, sum_v / n as f64 - stv))
}

// =========================================================================
// Storm-relative wind
// =========================================================================

/// Pressure-weighted mean storm-relative wind through a layer.
/// Thin wrapper around [`mean_wind`].
pub fn sr_wind(
    prof: &Profile,
    pbot: f64,
    ptop: f64,
    stu: f64,
    stv: f64,
    dp: f64,
) -> Result<(f64, f64), SharpError> {
    mean_wind(prof, pbot, ptop, dp, stu, stv)
}

/// Non-pressure-weighted mean storm-relative wind.
/// Thin wrapper around [`mean_wind_npw`].
pub fn sr_wind_npw(
    prof: &Profile,
    pbot: f64,
    ptop: f64,
    stu: f64,
    stv: f64,
    dp: f64,
) -> Result<(f64, f64), SharpError> {
    mean_wind_npw(prof, pbot, ptop, dp, stu, stv)
}

// =========================================================================
// Shear
// =========================================================================

/// Bulk wind shear (U, V in kts) between `pbot` and `ptop` (hPa).
///
/// Returns the vector difference (top − bottom).  Matches SHARPpy
/// `wind_shear()`.
pub fn wind_shear(prof: &Profile, pbot: f64, ptop: f64) -> Result<(f64, f64), SharpError> {
    require_wind(prof)?;
    require_valid(pbot, "pbot")?;
    require_valid(ptop, "ptop")?;

    let (ubot, vbot) = prof.interp_wind(pbot);
    let (utop, vtop) = prof.interp_wind(ptop);

    if !ubot.is_finite() || !vbot.is_finite() || !utop.is_finite() || !vtop.is_finite() {
        return Err(SharpError::NoData { field: "wind" });
    }

    Ok((utop - ubot, vtop - vbot))
}

// =========================================================================
// Helicity
// =========================================================================

/// Storm-relative helicity (m²/s²) in a layer from `lower` to `upper` (m AGL).
///
/// Returns `(total_helicity, positive_helicity, negative_helicity)`.
///
/// If `exact` is true, uses the native sounding levels within the layer plus
/// interpolated endpoints; otherwise interpolates at `dp` hPa intervals.
///
/// Wind components are converted from knots to m/s before computing the
/// cross products, matching SHARPpy `helicity()`.
///
/// `stu` and `stv` are storm-motion components in **knots**.
pub fn helicity(
    prof: &Profile,
    lower: f64,
    upper: f64,
    stu: f64,
    stv: f64,
    dp: f64,
    exact: bool,
) -> Result<(f64, f64, f64), SharpError> {
    require_wind(prof)?;
    require_valid(lower, "lower")?;
    require_valid(upper, "upper")?;
    require_valid(stu, "stu")?;
    require_valid(stv, "stv")?;

    // Zero-thickness layer
    if (lower - upper).abs() < TOL {
        return Ok((0.0, 0.0, 0.0));
    }

    let lower_msl = prof.to_msl(lower);
    let upper_msl = prof.to_msl(upper);
    let plower = prof.pres_at_height(lower_msl);
    let pupper = prof.pres_at_height(upper_msl);

    if !plower.is_finite() || !pupper.is_finite() {
        return Err(SharpError::NoData { field: "pressure" });
    }

    // Build arrays of (u, v) in knots through the layer
    let (us, vs) = if exact {
        let (u1, v1) = prof.interp_wind(plower);
        let (u2, v2) = prof.interp_wind(pupper);
        if !u1.is_finite() || !v1.is_finite() || !u2.is_finite() || !v2.is_finite() {
            return Err(SharpError::NoData { field: "wind" });
        }

        let mut u_vec = vec![u1];
        let mut v_vec = vec![v1];

        // Add interior sounding levels strictly between plower and pupper
        for i in 0..prof.pres.len() {
            let p = prof.pres[i];
            if !p.is_finite() || !prof.u[i].is_finite() || !prof.v[i].is_finite() {
                continue;
            }
            if p < plower && p > pupper {
                u_vec.push(prof.u[i]);
                v_vec.push(prof.v[i]);
            }
        }

        u_vec.push(u2);
        v_vec.push(v2);
        (u_vec, v_vec)
    } else {
        let dp = if dp > 0.0 { -dp } else { dp };
        let mut u_vec = Vec::new();
        let mut v_vec = Vec::new();
        let mut p = plower;
        while p >= pupper + dp - 0.0001 {
            let (ui, vi) = prof.interp_wind(p);
            if ui.is_finite() && vi.is_finite() {
                u_vec.push(ui);
                v_vec.push(vi);
            }
            p += dp;
        }
        (u_vec, v_vec)
    };

    if us.len() < 2 {
        return Err(SharpError::InsufficientLevels {
            need: 2,
            got: us.len(),
        });
    }

    // Storm-relative components converted to m/s (SHARPpy: KTS2MS(u - stu))
    let sru: Vec<f64> = us.iter().map(|&u| kts2ms(u - stu)).collect();
    let srv: Vec<f64> = vs.iter().map(|&v| kts2ms(v - stv)).collect();

    // Cross-product sum: SRU[i+1]*SRV[i] - SRU[i]*SRV[i+1]
    let mut phel = 0.0;
    let mut nhel = 0.0;
    for i in 0..sru.len() - 1 {
        let cross = sru[i + 1] * srv[i] - sru[i] * srv[i + 1];
        if cross > 0.0 {
            phel += cross;
        } else {
            nhel += cross;
        }
    }

    Ok((phel + nhel, phel, nhel))
}

// =========================================================================
// Storm motion
// =========================================================================

/// Bunkers non-parcel storm motion vectors.
///
/// Returns `(rstu, rstv, lstu, lstv)` — right-mover and left-mover
/// U/V components in **knots**.
///
/// Uses the 0-6 km non-pressure-weighted mean wind and 0-6 km bulk shear
/// with a 7.5 m/s deviation magnitude perpendicular to the shear vector.
///
/// Matches SHARPpy `non_parcel_bunkers_motion()`.
pub fn non_parcel_bunkers_motion(prof: &Profile) -> Result<(f64, f64, f64, f64), SharpError> {
    require_wind(prof)?;

    // 7.5 m/s deviation, converted to knots for internal arithmetic
    let d = ms2kts(7.5);

    let msl6km = prof.to_msl(6000.0);
    let p6km = prof.pres_at_height(msl6km);
    if !p6km.is_finite() {
        return Err(SharpError::NoData {
            field: "pressure at 6 km",
        });
    }

    // SFC-6km non-pressure-weighted mean wind (kts)
    let (mnu6, mnv6) = mean_wind_npw(prof, prof.sfc_pressure(), p6km, -1.0, 0.0, 0.0)?;

    // SFC-6km bulk shear (kts)
    let (shru, shrv) = wind_shear(prof, prof.sfc_pressure(), p6km)?;

    let shear_mag = mag_f64(shru, shrv);
    if shear_mag < TOL {
        return Err(SharpError::InvalidInput(
            "near-zero 0-6 km shear; Bunkers motion undefined".into(),
        ));
    }

    let tmp = d / shear_mag;
    let rstu = mnu6 + tmp * shrv;
    let rstv = mnv6 - tmp * shru;
    let lstu = mnu6 - tmp * shrv;
    let lstv = mnv6 + tmp * shru;

    Ok((rstu, rstv, lstu, lstv))
}

/// Parcel-based Bunkers storm motion `(rstu, rstv, lstu, lstv)` in **knots**.
///
/// This is the Bunkers et al. (2014) method SHARPpy's `ConvectiveProfile`
/// uses whenever an effective inflow layer exists: the 7.5 m/s deviation is
/// applied to the pressure-weighted mean wind through the layer from the
/// effective inflow base (`pbot`, hPa) up to 65% of the most-unstable parcel's
/// equilibrium level, rather than to the fixed SFC-6 km layer.
///
/// Falls back to [`non_parcel_bunkers_motion`] when the MU parcel is too weak
/// (MUCAPE <= 100 J/kg) or its EL / the inflow base height is unusable, which
/// is the same fall-back SHARPpy's `params.bunkers_storm_motion` performs.
///
/// Port of SHARPpy `params.bunkers_storm_motion`.
pub fn bunkers_storm_motion(
    prof: &Profile,
    mupcl: &crate::params::cape::ParcelResult,
    pbot: f64,
) -> (f64, f64, f64, f64) {
    let d = ms2kts(7.5); // 7.5 m/s deviation, in kts
    let mucape = mupcl.bplus;
    let muel = mupcl.elhght;
    let base = prof.to_agl(prof.interp_hght(pbot));
    if mucape > 100.0 && is_valid(muel) && is_valid(base) {
        let depth = muel - base;
        let htop = base + depth * (65.0 / 100.0);
        let ptop = prof.pres_at_height(prof.to_msl(htop));
        let (mnu, mnv) =
            mean_wind(prof, pbot, ptop, -1.0, 0.0, 0.0).unwrap_or((f64::NAN, f64::NAN));
        let (sru, srv) = wind_shear(prof, pbot, ptop).unwrap_or((f64::NAN, f64::NAN));
        let srmag = mag_f64(sru, srv);
        let uchg = d / srmag * srv;
        let vchg = d / srmag * sru;
        (mnu + uchg, mnv - vchg, mnu - uchg, mnv + vchg)
    } else {
        non_parcel_bunkers_motion(prof).unwrap_or((f64::NAN, f64::NAN, f64::NAN, f64::NAN))
    }
}

/// Experimental Bunkers storm motion (500 m / 5.5-6.0 km variant).
///
/// Uses pressure-weighted mean winds in the SFC-500 m and 5.5-6.0 km layers
/// to compute the shear vector, then deviates from the SFC-6 km mean wind.
///
/// Matches SHARPpy `non_parcel_bunkers_motion_experimental()`.
pub fn non_parcel_bunkers_motion_experimental(
    prof: &Profile,
) -> Result<(f64, f64, f64, f64), SharpError> {
    require_wind(prof)?;

    let d = ms2kts(7.5);

    let msl500m = prof.to_msl(500.0);
    let msl5500m = prof.to_msl(5500.0);
    let msl6000m = prof.to_msl(6000.0);

    let p500m = prof.pres_at_height(msl500m);
    let p5500m = prof.pres_at_height(msl5500m);
    let p6000m = prof.pres_at_height(msl6000m);

    if !p500m.is_finite() || !p5500m.is_finite() || !p6000m.is_finite() {
        return Err(SharpError::NoData { field: "pressure" });
    }

    // SFC-500m pressure-weighted mean wind
    let (mnu500, mnv500) = mean_wind(prof, prof.sfc_pressure(), p500m, -1.0, 0.0, 0.0)?;

    // 5.5-6.0 km pressure-weighted mean wind
    let (mnu55_60, mnv55_60) = mean_wind(prof, p5500m, p6000m, -1.0, 0.0, 0.0)?;

    // Shear between the two layers
    let shru = mnu55_60 - mnu500;
    let shrv = mnv55_60 - mnv500;

    // SFC-6km pressure-weighted mean wind
    let (mnu6, mnv6) = mean_wind(prof, prof.sfc_pressure(), p6000m, -1.0, 0.0, 0.0)?;

    let shear_mag = mag_f64(shru, shrv);
    if shear_mag < TOL {
        return Err(SharpError::InvalidInput(
            "near-zero shear; experimental Bunkers motion undefined".into(),
        ));
    }

    let tmp = d / shear_mag;
    let rstu = mnu6 + tmp * shrv;
    let rstv = mnv6 - tmp * shru;
    let lstu = mnu6 - tmp * shrv;
    let lstv = mnv6 + tmp * shru;

    Ok((rstu, rstv, lstu, lstv))
}

/// Corfidi MCS motion vectors (Meso-Beta Element vectors).
///
/// Returns `(upshear_u, upshear_v, downshear_u, downshear_v)` in **knots**.
///
/// Computes the upshear (propagation) and downshear vectors from the
/// tropospheric mean wind (850-300 hPa) and low-level mean wind (SFC-1.5 km).
/// If the surface pressure is below 850 hPa, the tropospheric mean uses
/// the surface pressure as the bottom.
///
/// Matches SHARPpy `corfidi_mcs_motion()`.
pub fn corfidi_mcs_motion(prof: &Profile) -> Result<(f64, f64, f64, f64), SharpError> {
    require_wind(prof)?;

    // Tropospheric mean wind (850-300 hPa or sfc-300 hPa, npw)
    let trop_pbot = if prof.sfc_pressure() < 850.0 {
        prof.sfc_pressure()
    } else {
        850.0
    };
    let (mnu1, mnv1) = mean_wind_npw(prof, trop_pbot, 300.0, -1.0, 0.0, 0.0)?;

    // Low-level mean wind (SFC-1500 m, npw)
    let msl1500 = prof.to_msl(1500.0);
    let p1500 = prof.pres_at_height(msl1500);
    if !p1500.is_finite() {
        return Err(SharpError::NoData {
            field: "pressure at 1.5 km",
        });
    }
    let (mnu2, mnv2) = mean_wind_npw(prof, prof.sfc_pressure(), p1500, -1.0, 0.0, 0.0)?;

    // Upshear (propagation) vector
    let upu = mnu1 - mnu2;
    let upv = mnv1 - mnv2;

    // Downshear vector
    let dnu = mnu1 + upu;
    let dnv = mnv1 + upv;

    Ok((upu, upv, dnu, dnv))
}

/// Alias for [`corfidi_mcs_motion`].
pub fn mbe_vectors(prof: &Profile) -> Result<(f64, f64, f64, f64), SharpError> {
    corfidi_mcs_motion(prof)
}

// =========================================================================
// Maximum wind
// =========================================================================

/// Maximum wind speed in a layer (`lower`..`upper` in m AGL).
///
/// Returns `(u, v, pressure)` of the maximum-wind level (lowest level in
/// case of ties).  All values in knots/hPa.
///
/// Matches SHARPpy `max_wind()`.
pub fn max_wind(prof: &Profile, lower: f64, upper: f64) -> Result<(f64, f64, f64), SharpError> {
    require_wind(prof)?;
    require_valid(lower, "lower")?;
    require_valid(upper, "upper")?;

    let lower_msl = prof.to_msl(lower);
    let upper_msl = prof.to_msl(upper);
    let plower = prof.pres_at_height(lower_msl);
    let pupper = prof.pres_at_height(upper_msl);

    if !plower.is_finite() || !pupper.is_finite() {
        return Err(SharpError::NoData { field: "pressure" });
    }

    let mut best_spd = -1.0_f64;
    let mut best_u = f64::NAN;
    let mut best_v = f64::NAN;
    let mut best_p = f64::NAN;

    for i in 0..prof.pres.len() {
        let p = prof.pres[i];
        if !p.is_finite() || !prof.u[i].is_finite() || !prof.v[i].is_finite() {
            continue;
        }
        // Level within the layer (inclusive with tolerance)
        if p > plower + 0.01 || p < pupper - 0.01 {
            continue;
        }
        let spd = mag_f64(prof.u[i], prof.v[i]);
        if spd > best_spd {
            best_spd = spd;
            best_u = prof.u[i];
            best_v = prof.v[i];
            best_p = p;
        }
    }

    if best_spd < 0.0 {
        return Err(SharpError::NoData {
            field: "wind in layer",
        });
    }

    Ok((best_u, best_v, best_p))
}

// =========================================================================
// Critical angle
// =========================================================================

/// Critical angle (degrees) per Esterheld and Giuliano (2008).
///
/// The angle between the 0-500 m AGL shear vector and the storm-relative
/// inflow vector at the surface.  90° indicates pure streamwise vorticity
/// in the lowest 500 m.
///
/// `stu` and `stv` are storm-motion components in **knots**.
///
/// Matches SHARPpy `critical_angle()`.
pub fn critical_angle(prof: &Profile, stu: f64, stv: f64) -> Result<f64, SharpError> {
    require_wind(prof)?;
    require_valid(stu, "stu")?;
    require_valid(stv, "stv")?;

    let msl500 = prof.to_msl(500.0);
    let p500 = prof.pres_at_height(msl500);
    if !p500.is_finite() {
        return Err(SharpError::NoData {
            field: "pressure at 500 m",
        });
    }

    let (u500, v500) = prof.interp_wind(p500);
    let (sfc_u, sfc_v) = prof.interp_wind(prof.sfc_pressure());

    if !u500.is_finite() || !v500.is_finite() || !sfc_u.is_finite() || !sfc_v.is_finite() {
        return Err(SharpError::NoData { field: "wind" });
    }

    // 0-500m shear vector
    let vec1_u = u500 - sfc_u;
    let vec1_v = v500 - sfc_v;

    // Storm-relative inflow vector (storm motion minus surface wind)
    let vec2_u = stu - sfc_u;
    let vec2_v = stv - sfc_v;

    let mag1 = mag_f64(vec1_u, vec1_v);
    let mag2 = mag_f64(vec2_u, vec2_v);

    if mag1 < TOL || mag2 < TOL {
        return Err(SharpError::InvalidInput(
            "zero-length vector in critical angle calculation".into(),
        ));
    }

    let dot = vec1_u * vec2_u + vec1_v * vec2_v;
    let cos_angle = (dot / (mag1 * mag2)).clamp(-1.0, 1.0);
    Ok(cos_angle.acos().to_degrees())
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::StationInfo;

    /// Build a veering, speed-increasing supercell-like test sounding.
    fn make_test_profile() -> Profile {
        // Pressure (hPa), Height (m MSL), Temp (C), Dewpt (C), Wdir (deg), Wspd (kts)
        let pres = [
            1000.0, 950.0, 900.0, 850.0, 800.0, 750.0, 700.0, 650.0, 600.0, 550.0, 500.0, 450.0,
            400.0, 350.0, 300.0, 250.0, 200.0,
        ];
        let hght = [
            100.0, 540.0, 1000.0, 1480.0, 1980.0, 2500.0, 3050.0, 3620.0, 4220.0, 4860.0, 5540.0,
            6280.0, 7100.0, 8000.0, 9100.0, 10400.0, 11800.0,
        ];
        let tmpc = [
            30.0, 25.0, 20.0, 16.0, 12.0, 8.0, 4.0, 0.0, -5.0, -10.0, -16.0, -22.0, -30.0, -38.0,
            -45.0, -55.0, -60.0,
        ];
        let dwpc = [
            22.0, 18.0, 12.0, 8.0, 3.0, -2.0, -8.0, -14.0, -20.0, -26.0, -32.0, -38.0, -44.0,
            -50.0, -55.0, -60.0, -65.0,
        ];
        // Veering winds: 180 at sfc → 290 at jet level, speed 10→80 kts
        let wdir = [
            180.0, 190.0, 210.0, 230.0, 240.0, 250.0, 260.0, 265.0, 270.0, 275.0, 280.0, 285.0,
            290.0, 290.0, 285.0, 280.0, 275.0,
        ];
        let wspd = [
            10.0, 15.0, 22.0, 30.0, 35.0, 40.0, 45.0, 50.0, 55.0, 60.0, 65.0, 70.0, 75.0, 78.0,
            80.0, 75.0, 65.0,
        ];

        Profile::new(
            &pres,
            &hght,
            &tmpc,
            &dwpc,
            &wdir,
            &wspd,
            &[],
            StationInfo::default(),
        )
        .expect("test sounding should be valid")
    }

    // ---- mean_wind ----

    #[test]
    fn test_mean_wind_pw() {
        let prof = make_test_profile();
        let (mu, mv) = mean_wind(&prof, 850.0, 250.0, -1.0, 0.0, 0.0).unwrap();
        // Should be roughly westerly (u > 0)
        assert!(mu > 0.0, "mean u should be positive (westerly), got {mu}");
        let spd = mag_f64(mu, mv);
        assert!(
            spd > 10.0 && spd < 100.0,
            "mean wind speed unreasonable: {spd}"
        );
    }

    #[test]
    fn test_mean_wind_npw_differs_from_pw() {
        let prof = make_test_profile();
        let (pw_u, pw_v) = mean_wind(&prof, 850.0, 250.0, -1.0, 0.0, 0.0).unwrap();
        let (npw_u, npw_v) = mean_wind_npw(&prof, 850.0, 250.0, -1.0, 0.0, 0.0).unwrap();
        // PW weights lower (higher-pressure) levels more → should differ
        let diff = ((pw_u - npw_u).powi(2) + (pw_v - npw_v).powi(2)).sqrt();
        assert!(diff > 0.01, "PW and NPW should differ");
    }

    #[test]
    fn test_mean_wind_storm_relative() {
        let prof = make_test_profile();
        let (mu, mv) = mean_wind(&prof, 850.0, 250.0, -1.0, 0.0, 0.0).unwrap();
        let (su, sv) = mean_wind(&prof, 850.0, 250.0, -1.0, 5.0, 3.0).unwrap();
        assert!((su - (mu - 5.0)).abs() < 1e-6);
        assert!((sv - (mv - 3.0)).abs() < 1e-6);
    }

    // ---- sr_wind wrappers ----

    #[test]
    fn test_sr_wind_equals_mean_wind() {
        let prof = make_test_profile();
        let mw = mean_wind(&prof, 850.0, 250.0, -1.0, 5.0, 3.0).unwrap();
        let sr = sr_wind(&prof, 850.0, 250.0, 5.0, 3.0, -1.0).unwrap();
        assert!((mw.0 - sr.0).abs() < 1e-10);
        assert!((mw.1 - sr.1).abs() < 1e-10);
    }

    // ---- wind_shear ----

    #[test]
    fn test_wind_shear_sfc_500mb() {
        let prof = make_test_profile();
        let (su, sv) = wind_shear(&prof, 1000.0, 500.0).unwrap();
        let shear_mag = mag_f64(su, sv);
        // Winds go from 10 kt at 180° to 65 kt at 280° → significant shear
        assert!(
            shear_mag > 30.0,
            "0-5km shear should be >30 kt, got {shear_mag}"
        );
    }

    #[test]
    fn test_wind_shear_invalid_pbot() {
        let prof = make_test_profile();
        assert!(wind_shear(&prof, f64::NAN, 500.0).is_err());
    }

    // ---- helicity ----

    #[test]
    fn test_helicity_0_1km() {
        let prof = make_test_profile();
        let (rstu, rstv, _, _) = non_parcel_bunkers_motion(&prof).unwrap();
        let (total, pos, neg) = helicity(&prof, 0.0, 1000.0, rstu, rstv, -1.0, true).unwrap();

        // Veering hodograph → positive SRH for right-mover
        assert!(total > 0.0, "0-1km SRH should be positive, got {total}");
        assert!(pos >= 0.0);
        assert!(neg <= 0.0);
        assert!((total - (pos + neg)).abs() < 1e-10);
    }

    #[test]
    fn test_helicity_0_3km_gt_0_1km() {
        let prof = make_test_profile();
        let (rstu, rstv, _, _) = non_parcel_bunkers_motion(&prof).unwrap();
        let (h1, _, _) = helicity(&prof, 0.0, 1000.0, rstu, rstv, -1.0, true).unwrap();
        let (h3, _, _) = helicity(&prof, 0.0, 3000.0, rstu, rstv, -1.0, true).unwrap();
        assert!(h3 > h1, "0-3km SRH ({h3}) should exceed 0-1km SRH ({h1})");
    }

    #[test]
    fn test_helicity_zero_layer() {
        let prof = make_test_profile();
        let (total, pos, neg) = helicity(&prof, 500.0, 500.0, 0.0, 0.0, -1.0, true).unwrap();
        assert!(total.abs() < 1e-10);
        assert!(pos.abs() < 1e-10);
        assert!(neg.abs() < 1e-10);
    }

    #[test]
    fn test_helicity_exact_vs_interp() {
        let prof = make_test_profile();
        let (rstu, rstv, _, _) = non_parcel_bunkers_motion(&prof).unwrap();
        let (he, _, _) = helicity(&prof, 0.0, 3000.0, rstu, rstv, -1.0, true).unwrap();
        let (hi, _, _) = helicity(&prof, 0.0, 3000.0, rstu, rstv, -1.0, false).unwrap();
        // Should be reasonably close
        let diff = (he - hi).abs();
        let denom = he.abs().max(1.0);
        assert!(
            diff / denom < 0.3,
            "exact ({he}) vs interp ({hi}) helicity differ too much"
        );
    }

    // ---- max_wind ----

    #[test]
    fn test_max_wind() {
        let prof = make_test_profile();
        // Max wind in 0-10000 m AGL
        let (u, v, p) = max_wind(&prof, 0.0, 10000.0).unwrap();
        let spd = mag_f64(u, v);
        // Strongest wind is 80 kt at 300 hPa
        assert!(
            (spd - 80.0).abs() < 1.0,
            "max wind should be ~80 kt, got {spd}"
        );
        assert!(
            (p - 300.0).abs() < 1.0,
            "max wind should be at ~300 hPa, got {p}"
        );
    }

    // ---- Bunkers motion ----

    #[test]
    fn test_bunkers_motion() {
        let prof = make_test_profile();
        let (rstu, rstv, lstu, lstv) = non_parcel_bunkers_motion(&prof).unwrap();

        // Right and left movers should both have reasonable speeds
        let rm_spd = mag_f64(rstu, rstv);
        let lm_spd = mag_f64(lstu, lstv);
        assert!(rm_spd > 0.0 && rm_spd < 100.0, "RM speed: {rm_spd}");
        assert!(lm_spd > 0.0 && lm_spd < 100.0, "LM speed: {lm_spd}");

        // Get mean wind for comparison
        let msl6 = prof.to_msl(6000.0);
        let p6 = prof.pres_at_height(msl6);
        let (mu, mv) = mean_wind_npw(&prof, prof.sfc_pressure(), p6, -1.0, 0.0, 0.0).unwrap();

        // Right and left movers should be equidistant from mean wind
        let rd = mag_f64(rstu - mu, rstv - mv);
        let ld = mag_f64(lstu - mu, lstv - mv);
        assert!(
            (rd - ld).abs() < 0.5,
            "RM/LM deviations should be equal: {rd} vs {ld}"
        );

        // Deviation should be ~7.5 m/s ≈ 14.58 kts
        let expected_dev_kts = ms2kts(7.5);
        assert!(
            (rd - expected_dev_kts).abs() < 1.0,
            "deviation should be ~{expected_dev_kts} kt, got {rd}"
        );
    }

    #[test]
    fn test_bunkers_experimental() {
        let prof = make_test_profile();
        let (rstu, rstv, lstu, lstv) = non_parcel_bunkers_motion_experimental(&prof).unwrap();

        let rm_spd = mag_f64(rstu, rstv);
        let lm_spd = mag_f64(lstu, lstv);
        assert!(rm_spd > 0.0 && rm_spd < 100.0);
        assert!(lm_spd > 0.0 && lm_spd < 100.0);
    }

    // ---- Corfidi / MBE ----

    #[test]
    fn test_corfidi_mcs_motion() {
        let prof = make_test_profile();
        let (upu, upv, dnu, dnv) = corfidi_mcs_motion(&prof).unwrap();

        let up_spd = mag_f64(upu, upv);
        let dn_spd = mag_f64(dnu, dnv);
        assert!(up_spd > 0.0, "upshear speed should be > 0");
        assert!(dn_spd > 0.0, "downshear speed should be > 0");
    }

    #[test]
    fn test_mbe_equals_corfidi() {
        let prof = make_test_profile();
        let cor = corfidi_mcs_motion(&prof).unwrap();
        let mbe = mbe_vectors(&prof).unwrap();
        assert_eq!(cor, mbe);
    }

    // ---- Critical angle ----

    #[test]
    fn test_critical_angle_range() {
        let prof = make_test_profile();
        let (rstu, rstv, _, _) = non_parcel_bunkers_motion(&prof).unwrap();
        let ca = critical_angle(&prof, rstu, rstv).unwrap();
        assert!(
            ca >= 0.0 && ca <= 180.0,
            "critical angle should be 0-180°, got {ca}"
        );
    }

    #[test]
    fn test_critical_angle_90_streamwise() {
        // Engineer a sounding where the 0-500m shear vector is perpendicular
        // to the storm-relative inflow vector → 90°.
        //
        // SFC: u=0, v=0 (calm).  500m: u=+10, v=0 (shear → east).
        // Storm motion: u=0, v=+10 (moving north → inflow from south).
        //
        // Shear vector = (10, 0).  Inflow vector = (0, 10).  Angle = 90°.
        let pres = [1000.0, 950.0, 900.0, 850.0, 700.0, 500.0];
        let hght = [0.0, 500.0, 1000.0, 1500.0, 3000.0, 5500.0];
        let tmpc = [30.0, 26.0, 22.0, 18.0, 4.0, -15.0];
        let dwpc = [20.0, 16.0, 12.0, 8.0, -4.0, -30.0];
        // wdir/wspd: calm at sfc, then 270°@10kt at 500m
        // vec2comp(270, 10) → u=10, v=0
        // vec2comp(0, 0) → u=0, v=0  (calm)
        let wdir = [0.0, 270.0, 270.0, 270.0, 270.0, 270.0];
        let wspd = [0.0, 10.0, 15.0, 20.0, 30.0, 50.0];

        let prof = Profile::new(
            &pres,
            &hght,
            &tmpc,
            &dwpc,
            &wdir,
            &wspd,
            &[],
            StationInfo::default(),
        )
        .unwrap();

        // Storm motion: u=0, v=10 (moving north)
        let ca = critical_angle(&prof, 0.0, 10.0).unwrap();
        assert!(
            (ca - 90.0).abs() < 1.0,
            "expected ~90° for perpendicular vectors, got {ca}°"
        );
    }

    #[test]
    fn test_critical_angle_invalid_storm() {
        let prof = make_test_profile();
        assert!(critical_angle(&prof, f64::NAN, 0.0).is_err());
    }

    // ---- Edge cases ----

    #[test]
    fn test_empty_profile_errors() {
        // Build a minimal profile with no wind data
        let pres = [1000.0, 500.0];
        let hght = [100.0, 5500.0];
        let tmpc = [30.0, -15.0];
        let dwpc = [20.0, -30.0];
        // No wind data
        let prof = Profile::new(
            &pres,
            &hght,
            &tmpc,
            &dwpc,
            &[],
            &[],
            &[],
            StationInfo::default(),
        )
        .unwrap();

        assert!(mean_wind(&prof, 850.0, 250.0, -1.0, 0.0, 0.0).is_err());
        assert!(wind_shear(&prof, 1000.0, 500.0).is_err());
        assert!(non_parcel_bunkers_motion(&prof).is_err());
        assert!(corfidi_mcs_motion(&prof).is_err());
    }

    // ---- Quantitative checks ----

    #[test]
    fn test_bulk_shear_0_6km() {
        let prof = make_test_profile();
        let msl6 = prof.to_msl(6000.0);
        let p6 = prof.pres_at_height(msl6);
        assert!(p6.is_finite());
        let (su, sv) = wind_shear(&prof, prof.sfc_pressure(), p6).unwrap();
        let shear_mag = mag_f64(su, sv);
        // Veering from 180/10 to ~280/65 over 6km → large shear
        assert!(
            shear_mag > 30.0 && shear_mag < 100.0,
            "0-6km shear should be 30-100 kt, got {shear_mag}"
        );
    }

    #[test]
    fn test_srh_values_reasonable() {
        // For a strongly veering hodograph, 0-3km SRH of 150-600 m²/s²
        // is typical for significant severe weather.
        let prof = make_test_profile();
        let (rstu, rstv, _, _) = non_parcel_bunkers_motion(&prof).unwrap();
        let (srh3, _, _) = helicity(&prof, 0.0, 3000.0, rstu, rstv, -1.0, true).unwrap();
        assert!(
            srh3 > 50.0 && srh3 < 1000.0,
            "0-3km SRH should be 50-1000 m²/s², got {srh3}"
        );
    }
}
