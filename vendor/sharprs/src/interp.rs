//! Interpolation routines for vertical atmospheric profiles.
//!
//! This module mirrors SHARPpy's `sharptab/interp.py`.  All pressure-based
//! interpolation uses **log10(p)** as the vertical coordinate (log-linear
//! interpolation), which is standard practice in meteorology because pressure
//! decreases roughly exponentially with height.
//!
//! Functions accept raw slices (`&[f64]`) for the profile arrays rather than a
//! monolithic Profile struct, keeping the API flexible and zero-copy.
//!
//! # Missing data convention
//! * Input arrays may contain `f64::NAN` to represent missing observations.
//! * Functions return `Option<f64>`, with `None` indicating that the result
//!   could not be computed (target outside profile bounds, all data missing,
//!   etc.).

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Returns `true` if a value is finite and not NaN (i.e., it represents a
/// valid observation).
#[inline]
fn is_valid(v: f64) -> bool {
    v.is_finite()
}

/// Build a Vec of indices where **both** `a` and `b` are valid (non-NaN,
/// finite).  This mirrors SHARPpy's mask-merging logic.
#[inline]
fn valid_pair_indices(a: &[f64], b: &[f64]) -> Vec<usize> {
    assert_eq!(a.len(), b.len());
    (0..a.len())
        .filter(|&i| is_valid(a[i]) && is_valid(b[i]))
        .collect()
}

/// One-dimensional linear interpolation of `target` within the sorted
/// coordinate array `xp` (ascending), returning the corresponding
/// interpolated value from `fp`.
///
/// If `target` is outside the range of `xp`, returns `None` (equivalent to
/// SHARPpy / numpy's `left=masked, right=masked`).
fn interp1d(target: f64, xp: &[f64], fp: &[f64]) -> Option<f64> {
    if xp.is_empty() || fp.is_empty() || !is_valid(target) {
        return None;
    }
    debug_assert_eq!(xp.len(), fp.len());

    let n = xp.len();

    // Exact match on boundaries
    if (target - xp[0]).abs() < 1e-12 {
        return Some(fp[0]);
    }
    if (target - xp[n - 1]).abs() < 1e-12 {
        return Some(fp[n - 1]);
    }

    // Out of bounds
    if target < xp[0] || target > xp[n - 1] {
        return None;
    }

    // Binary search for the bracketing interval
    let mut lo = 0usize;
    let mut hi = n - 1;
    while hi - lo > 1 {
        let mid = (lo + hi) / 2;
        if xp[mid] <= target {
            lo = mid;
        } else {
            hi = mid;
        }
    }

    let dx = xp[hi] - xp[lo];
    if dx.abs() < 1e-30 {
        return Some(fp[lo]);
    }
    let frac = (target - xp[lo]) / dx;
    Some(fp[lo] + frac * (fp[hi] - fp[lo]))
}

// ---------------------------------------------------------------------------
// Generic interpolation: pressure-based (log10 p)
// ---------------------------------------------------------------------------

/// Interpolate a field value at a target pressure using log10(p) as the
/// vertical coordinate.
///
/// This is the Rust equivalent of SHARPpy's `generic_interp_pres`.
///
/// # Arguments
/// * `p`     - target pressure (hPa).
/// * `pres`  - pressure array of the profile (hPa), **surface (highest) first,
///             decreasing upward**.  The routine handles reversal internally.
/// * `field` - the variable to interpolate (same length/order as `pres`).
///
/// # Returns
/// `Some(value)` on success, `None` if the target falls outside the valid
/// range or all data are missing.
pub fn generic_interp_pres(p: f64, pres: &[f64], field: &[f64]) -> Option<f64> {
    if !is_valid(p) || p <= 0.0 {
        return None;
    }

    let log_target = p.log10();

    // Collect valid (non-NaN) pairs and compute log10 of pressure.
    let idx = valid_pair_indices(pres, field);
    if idx.is_empty() {
        return None;
    }

    // SHARPpy reverses the arrays so that logp is ascending for np.interp.
    // We build ascending-sorted vectors directly.
    let mut coords: Vec<(f64, f64)> = idx.iter().map(|&i| (pres[i].log10(), field[i])).collect();
    coords.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());

    let xp: Vec<f64> = coords.iter().map(|c| c.0).collect();
    let fp: Vec<f64> = coords.iter().map(|c| c.1).collect();

    interp1d(log_target, &xp, &fp)
}

// ---------------------------------------------------------------------------
// Generic interpolation: height-based
// ---------------------------------------------------------------------------

/// Interpolate a field value at a target height.
///
/// This is the Rust equivalent of SHARPpy's `generic_interp_hght`.
///
/// # Arguments
/// * `h`     - target height (m MSL).
/// * `hght`  - height array of the profile (m MSL), **ascending**.
/// * `field` - the variable to interpolate (same length/order as `hght`).
/// * `log`   - if `true`, the `field` values are log10-encoded and the result
///             is exponentiated back: `10^(interp)`.  Used by [`pres`] to
///             return pressure in hPa from log10(p).
///
/// # Returns
/// `Some(value)` on success, `None` if the target falls outside the valid
/// range or all data are missing.
pub fn generic_interp_hght(h: f64, hght: &[f64], field: &[f64], log: bool) -> Option<f64> {
    if !is_valid(h) {
        return None;
    }

    let idx = valid_pair_indices(hght, field);
    if idx.is_empty() {
        return None;
    }

    // Height should already be ascending, but we guarantee it.
    let mut coords: Vec<(f64, f64)> = idx.iter().map(|&i| (hght[i], field[i])).collect();
    coords.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());

    let xp: Vec<f64> = coords.iter().map(|c| c.0).collect();
    let fp: Vec<f64> = coords.iter().map(|c| c.1).collect();

    let val = interp1d(h, &xp, &fp)?;
    if log {
        Some(10.0_f64.powf(val))
    } else {
        Some(val)
    }
}

// ---------------------------------------------------------------------------
// Profile-level convenience functions
// ---------------------------------------------------------------------------

/// Interpolate **pressure** (hPa) at a given height (m MSL).
///
/// Uses height as the independent coordinate and log10(p) as the dependent
/// variable, then exponentiates back to pressure.
///
/// Equivalent to SHARPpy's `interp.pres(prof, h)`.
///
/// # Arguments
/// * `h`    - target height (m MSL).
/// * `hght` - profile height array (m MSL, ascending).
/// * `logp` - profile log10(pressure) array (same length/order as `hght`).
pub fn pres(h: f64, hght: &[f64], logp: &[f64]) -> Option<f64> {
    generic_interp_hght(h, hght, logp, true)
}

/// Interpolate **height** (m MSL) at a given pressure (hPa).
///
/// Uses log10(p) as the independent coordinate.
///
/// Equivalent to SHARPpy's `interp.hght(prof, p)`.
///
/// # Arguments
/// * `p`        - target pressure (hPa).
/// * `pres_arr` - profile pressure array (hPa, surface-first / descending).
/// * `hght_arr` - profile height array (m MSL, same order as `pres_arr`).
pub fn hght(p: f64, pres_arr: &[f64], hght_arr: &[f64]) -> Option<f64> {
    generic_interp_pres(p, pres_arr, hght_arr)
}

/// Interpolate **temperature** (deg C) at a given pressure (hPa).
///
/// Equivalent to SHARPpy's `interp.temp(prof, p)`.
pub fn temp(p: f64, pres_arr: &[f64], tmpc: &[f64]) -> Option<f64> {
    generic_interp_pres(p, pres_arr, tmpc)
}

/// Interpolate **dewpoint** (deg C) at a given pressure (hPa).
///
/// Equivalent to SHARPpy's `interp.dwpt(prof, p)`.
pub fn dwpt(p: f64, pres_arr: &[f64], dwpc: &[f64]) -> Option<f64> {
    generic_interp_pres(p, pres_arr, dwpc)
}

/// Interpolate **virtual temperature** (deg C) at a given pressure (hPa).
///
/// Equivalent to SHARPpy's `interp.vtmp(prof, p)`.
pub fn vtmp(p: f64, pres_arr: &[f64], vtmp_arr: &[f64]) -> Option<f64> {
    generic_interp_pres(p, pres_arr, vtmp_arr)
}

/// Interpolate **omega** (microbar/s) at a given pressure (hPa).
///
/// Equivalent to SHARPpy's `interp.omeg(prof, p)`.
pub fn omeg(p: f64, pres_arr: &[f64], omeg_arr: &[f64]) -> Option<f64> {
    generic_interp_pres(p, pres_arr, omeg_arr)
}

/// Interpolate **theta-e** (K) at a given pressure (hPa).
///
/// Equivalent to SHARPpy's `interp.thetae(prof, p)`.
pub fn thetae(p: f64, pres_arr: &[f64], thetae_arr: &[f64]) -> Option<f64> {
    generic_interp_pres(p, pres_arr, thetae_arr)
}

/// Interpolate **theta** (K) at a given pressure (hPa).
///
/// Equivalent to SHARPpy's `interp.theta(prof, p)`.
pub fn theta(p: f64, pres_arr: &[f64], theta_arr: &[f64]) -> Option<f64> {
    generic_interp_pres(p, pres_arr, theta_arr)
}

/// Interpolate **mixing ratio** (g/kg) at a given pressure (hPa).
///
/// Equivalent to SHARPpy's `interp.mixratio(prof, p)`.
pub fn mixratio(p: f64, pres_arr: &[f64], wvmr: &[f64]) -> Option<f64> {
    generic_interp_pres(p, pres_arr, wvmr)
}

/// Interpolate **wetbulb temperature** (deg C) at a given pressure (hPa).
///
/// Equivalent to SHARPpy's `interp.wetbulb(prof, p)`.
pub fn wetbulb(p: f64, pres_arr: &[f64], wetbulb_arr: &[f64]) -> Option<f64> {
    generic_interp_pres(p, pres_arr, wetbulb_arr)
}

/// Interpolate **U and V wind components** (kt) at a given pressure (hPa).
///
/// Equivalent to SHARPpy's `interp.components(prof, p)`.
///
/// # Returns
/// `Some((u, v))` on success, `None` if either component cannot be computed.
pub fn components(p: f64, pres_arr: &[f64], u_arr: &[f64], v_arr: &[f64]) -> Option<(f64, f64)> {
    let u = generic_interp_pres(p, pres_arr, u_arr)?;
    let v = generic_interp_pres(p, pres_arr, v_arr)?;
    Some((u, v))
}

/// Convert U/V wind components to direction (meteorological degrees) and
/// speed (kt).
///
/// Wind direction follows the meteorological convention: the direction
/// **from** which the wind blows, measured clockwise from north.
fn comp2vec(u: f64, v: f64) -> (f64, f64) {
    let speed = (u * u + v * v).sqrt();
    if speed < 1e-10 {
        return (0.0, 0.0);
    }
    let dir_rad = (-u).atan2(-v);
    let mut dir_deg = dir_rad.to_degrees();
    if dir_deg < 0.0 {
        dir_deg += 360.0;
    }
    if dir_deg >= 360.0 {
        dir_deg -= 360.0;
    }
    (dir_deg, speed)
}

/// Interpolate **wind direction and speed** at a given pressure (hPa).
///
/// Interpolates U and V components separately, then converts back to
/// direction/speed.
///
/// Equivalent to SHARPpy's `interp.vec(prof, p)`.
///
/// # Returns
/// `Some((direction_deg, speed_kt))` on success, `None` if the wind
/// components cannot be interpolated.
pub fn vec(p: f64, pres_arr: &[f64], u_arr: &[f64], v_arr: &[f64]) -> Option<(f64, f64)> {
    let (u, v) = components(p, pres_arr, u_arr, v_arr)?;
    Some(comp2vec(u, v))
}

// ---------------------------------------------------------------------------
// Height conversion helpers
// ---------------------------------------------------------------------------

/// Convert a height from MSL to AGL.
///
/// Equivalent to SHARPpy's `interp.to_agl(prof, h)`.
///
/// # Arguments
/// * `h`       - height (m MSL).
/// * `sfc_hgt` - surface elevation (m MSL), i.e., `prof.hght[prof.sfc]`.
#[inline]
pub fn to_agl(h: f64, sfc_hgt: f64) -> f64 {
    h - sfc_hgt
}

/// Convert a height from AGL to MSL.
///
/// Equivalent to SHARPpy's `interp.to_msl(prof, h)`.
///
/// # Arguments
/// * `h`       - height (m AGL).
/// * `sfc_hgt` - surface elevation (m MSL).
#[inline]
pub fn to_msl(h: f64, sfc_hgt: f64) -> f64 {
    h + sfc_hgt
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Typical sounding snippet (surface to ~300 hPa).
    /// Pressure descends, height ascends.
    fn sample_profile() -> (Vec<f64>, Vec<f64>, Vec<f64>, Vec<f64>, Vec<f64>) {
        // pres (hPa), hght (m MSL), tmpc (deg C), dwpc (deg C), logp
        let pres = vec![1013.0, 1000.0, 925.0, 850.0, 700.0, 500.0, 300.0];
        let hght = vec![110.0, 200.0, 800.0, 1500.0, 3100.0, 5600.0, 9200.0];
        let tmpc = vec![30.0, 28.0, 22.0, 16.0, 2.0, -15.0, -40.0];
        let dwpc = vec![20.0, 18.0, 12.0, 6.0, -8.0, -30.0, -55.0];
        let logp: Vec<f64> = pres.iter().map(|p: &f64| p.log10()).collect();
        (pres, hght, tmpc, dwpc, logp)
    }

    fn sample_wind() -> (Vec<f64>, Vec<f64>) {
        // u (kt), v (kt) -- 7 levels matching the pressure profile above.
        let u = vec![5.0, 8.0, 15.0, 20.0, 30.0, 25.0, 20.0];
        let v = vec![-5.0, -3.0, 0.0, 5.0, 10.0, 15.0, 10.0];
        (u, v)
    }

    // -----------------------------------------------------------------------
    // generic_interp_pres
    // -----------------------------------------------------------------------

    #[test]
    fn interp_pres_at_existing_level() {
        let (pr, _, tmpc, _, _) = sample_profile();
        // Interpolate temperature at an existing pressure level (850 hPa)
        let t = generic_interp_pres(850.0, &pr, &tmpc).unwrap();
        assert!((t - 16.0).abs() < 0.01, "Expected 16.0, got {t}");
    }

    #[test]
    fn interp_pres_between_levels() {
        let (pr, _, tmpc, _, _) = sample_profile();
        // 775 hPa is between 850 and 700 hPa
        let t = generic_interp_pres(775.0, &pr, &tmpc).unwrap();
        // Should be between 16.0 and 2.0
        assert!(t > 2.0 && t < 16.0, "Expected between 2 and 16, got {t}");
    }

    #[test]
    fn interp_pres_out_of_bounds() {
        let (pr, _, tmpc, _, _) = sample_profile();
        // Above the profile
        assert!(generic_interp_pres(200.0, &pr, &tmpc).is_none());
        // Below the profile
        assert!(generic_interp_pres(1100.0, &pr, &tmpc).is_none());
    }

    #[test]
    fn interp_pres_invalid_pressure() {
        let (pr, _, tmpc, _, _) = sample_profile();
        assert!(generic_interp_pres(f64::NAN, &pr, &tmpc).is_none());
        assert!(generic_interp_pres(-10.0, &pr, &tmpc).is_none());
        assert!(generic_interp_pres(0.0, &pr, &tmpc).is_none());
    }

    #[test]
    fn interp_pres_with_missing_data() {
        let pr = vec![1000.0, 925.0, 850.0, 700.0, 500.0];
        let tmpc = vec![28.0, 22.0, f64::NAN, 2.0, -15.0];
        // Should skip the NaN at 850 hPa and still interpolate
        let t = generic_interp_pres(800.0, &pr, &tmpc).unwrap();
        assert!(t > 2.0 && t < 22.0);
    }

    #[test]
    fn interp_pres_all_missing() {
        let pr = vec![1000.0, 850.0, 700.0];
        let tmpc = vec![f64::NAN, f64::NAN, f64::NAN];
        assert!(generic_interp_pres(900.0, &pr, &tmpc).is_none());
    }

    // -----------------------------------------------------------------------
    // generic_interp_hght
    // -----------------------------------------------------------------------

    #[test]
    fn interp_hght_at_existing_level() {
        let (_, hght, tmpc, _, _) = sample_profile();
        let t = generic_interp_hght(1500.0, &hght, &tmpc, false).unwrap();
        assert!((t - 16.0).abs() < 0.01);
    }

    #[test]
    fn interp_hght_between_levels() {
        let (_, hght, tmpc, _, _) = sample_profile();
        let t = generic_interp_hght(2000.0, &hght, &tmpc, false).unwrap();
        assert!(t > 2.0 && t < 16.0);
    }

    #[test]
    fn interp_hght_out_of_bounds() {
        let (_, hght, tmpc, _, _) = sample_profile();
        assert!(generic_interp_hght(50.0, &hght, &tmpc, false).is_none());
        assert!(generic_interp_hght(15000.0, &hght, &tmpc, false).is_none());
    }

    // -----------------------------------------------------------------------
    // pres (height to pressure)
    // -----------------------------------------------------------------------

    #[test]
    fn pres_at_surface() {
        let (_pr, hght, _, _, logp) = sample_profile();
        let p = pres(110.0, &hght, &logp).unwrap();
        assert!((p - 1013.0).abs() < 0.5, "Expected ~1013, got {p}");
    }

    #[test]
    fn pres_interpolated() {
        let (_, hght, _, _, logp) = sample_profile();
        let p = pres(3100.0, &hght, &logp).unwrap();
        assert!((p - 700.0).abs() < 1.0, "Expected ~700, got {p}");
    }

    #[test]
    fn pres_mid_level() {
        let (_, hght, _, _, logp) = sample_profile();
        // ~2300 m should give something between 850 and 700 hPa
        let p = pres(2300.0, &hght, &logp).unwrap();
        assert!(p > 700.0 && p < 850.0, "Expected 700-850, got {p}");
    }

    // -----------------------------------------------------------------------
    // hght (pressure to height)
    // -----------------------------------------------------------------------

    #[test]
    fn hght_at_existing_level() {
        let (pr, hght_arr, _, _, _) = sample_profile();
        let h = hght(850.0, &pr, &hght_arr).unwrap();
        assert!((h - 1500.0).abs() < 1.0, "Expected 1500, got {h}");
    }

    #[test]
    fn hght_interpolated() {
        let (pr, hght_arr, _, _, _) = sample_profile();
        let h = hght(775.0, &pr, &hght_arr).unwrap();
        assert!(h > 1500.0 && h < 3100.0, "Expected 1500-3100, got {h}");
    }

    // -----------------------------------------------------------------------
    // temp / dwpt / vtmp
    // -----------------------------------------------------------------------

    #[test]
    fn temp_at_500() {
        let (pr, _, tmpc, _, _) = sample_profile();
        let t = temp(500.0, &pr, &tmpc).unwrap();
        assert!((t - (-15.0)).abs() < 0.01);
    }

    #[test]
    fn dwpt_at_700() {
        let (pr, _, _, dwpc, _) = sample_profile();
        let d = dwpt(700.0, &pr, &dwpc).unwrap();
        assert!((d - (-8.0)).abs() < 0.01);
    }

    #[test]
    fn vtmp_interp() {
        let (pr, _, tmpc, _, _) = sample_profile();
        // Using tmpc as a stand-in for vtmp array
        let v = vtmp(850.0, &pr, &tmpc).unwrap();
        assert!((v - 16.0).abs() < 0.01);
    }

    // -----------------------------------------------------------------------
    // components / vec
    // -----------------------------------------------------------------------

    #[test]
    fn components_at_existing_level() {
        let (pr, _, _, _, _) = sample_profile();
        let (u_arr, v_arr) = sample_wind();
        let (u, v) = components(850.0, &pr, &u_arr, &v_arr).unwrap();
        assert!((u - 20.0).abs() < 0.01);
        assert!((v - 5.0).abs() < 0.01);
    }

    #[test]
    fn components_interpolated() {
        let (pr, _, _, _, _) = sample_profile();
        let (u_arr, v_arr) = sample_wind();
        let (u, v) = components(775.0, &pr, &u_arr, &v_arr).unwrap();
        // Between 850 and 700 levels
        assert!(u > 20.0 && u < 30.0);
        assert!(v > 5.0 && v < 10.0);
    }

    #[test]
    fn vec_direction_speed() {
        let (pr, _, _, _, _) = sample_profile();
        let (u_arr, v_arr) = sample_wind();
        let (dir, spd) = vec(850.0, &pr, &u_arr, &v_arr).unwrap();
        // u=20, v=5 -> speed = sqrt(425) ~ 20.6
        assert!((spd - 20.6155).abs() < 0.01);
        // Direction should be defined (meteorological convention)
        assert!(dir >= 0.0 && dir < 360.0);
    }

    // -----------------------------------------------------------------------
    // comp2vec
    // -----------------------------------------------------------------------

    #[test]
    fn comp2vec_south_wind() {
        // South wind (from 180): u = -spd*sin(180) = 0, v = -spd*cos(180) = +10
        let (dir, spd) = comp2vec(0.0, 10.0);
        assert!((dir - 180.0).abs() < 0.1, "Expected 180, got {dir}");
        assert!((spd - 10.0).abs() < 0.01);
    }

    #[test]
    fn comp2vec_west_wind() {
        // West wind (from 270): u = -spd*sin(270) = +10, v = -spd*cos(270) = 0
        let (dir, spd) = comp2vec(10.0, 0.0);
        assert!((dir - 270.0).abs() < 0.1, "Expected 270, got {dir}");
        assert!((spd - 10.0).abs() < 0.01);
    }

    #[test]
    fn comp2vec_calm() {
        let (_dir, spd) = comp2vec(0.0, 0.0);
        assert!(spd.abs() < 1e-10);
    }

    // -----------------------------------------------------------------------
    // to_agl / to_msl
    // -----------------------------------------------------------------------

    #[test]
    fn to_agl_basic() {
        assert!((to_agl(1500.0, 200.0) - 1300.0).abs() < 1e-10);
    }

    #[test]
    fn to_msl_basic() {
        assert!((to_msl(1300.0, 200.0) - 1500.0).abs() < 1e-10);
    }

    #[test]
    fn agl_msl_roundtrip() {
        let sfc = 350.0;
        let msl = 5000.0;
        let agl = to_agl(msl, sfc);
        let back = to_msl(agl, sfc);
        assert!((back - msl).abs() < 1e-10);
    }

    // -----------------------------------------------------------------------
    // Log-pressure interpolation accuracy
    // -----------------------------------------------------------------------

    #[test]
    fn log_pressure_interpolation_is_not_linear() {
        // Verify that our pressure interpolation is truly log-linear, not
        // just linear-in-pressure.  The midpoint in log10 space between
        // 1000 and 100 hPa is 10^(1.5) ~ 316.23 hPa, NOT 550 hPa.
        let pr = vec![1000.0, 100.0];
        let field = vec![0.0, 100.0];
        let mid = generic_interp_pres(316.2278, &pr, &field).unwrap();
        // At the log-midpoint the field should be ~50
        assert!(
            (mid - 50.0).abs() < 0.5,
            "Log-linear midpoint should give ~50, got {mid}"
        );
        // And the linear midpoint (550 hPa) should NOT give 50
        let lin_mid = generic_interp_pres(550.0, &pr, &field).unwrap();
        assert!(
            (lin_mid - 50.0).abs() > 2.0,
            "Linear midpoint should NOT give 50, got {lin_mid}"
        );
    }

    // -----------------------------------------------------------------------
    // Edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn single_valid_level() {
        // Only one valid level -- cannot interpolate away from it
        let pr = vec![850.0];
        let field = vec![16.0];
        // At exact level it should work (boundary match)
        let val = generic_interp_pres(850.0, &pr, &field).unwrap();
        assert!((val - 16.0).abs() < 0.01);
        // Between levels should fail
        assert!(generic_interp_pres(800.0, &pr, &field).is_none());
    }

    #[test]
    fn empty_arrays() {
        let pr: Vec<f64> = vec![];
        let field: Vec<f64> = vec![];
        assert!(generic_interp_pres(500.0, &pr, &field).is_none());
        assert!(generic_interp_hght(3000.0, &pr, &field, false).is_none());
    }

    // -----------------------------------------------------------------------
    // Integrated workflow: pres -> hght roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn pres_hght_roundtrip() {
        let (pr, hght_arr, _, _, logp) = sample_profile();
        // Start at 2000 m, get pressure, then go back to height
        let p = pres(2000.0, &hght_arr, &logp).unwrap();
        let h = hght(p, &pr, &hght_arr).unwrap();
        assert!(
            (h - 2000.0).abs() < 1.0,
            "Roundtrip: expected ~2000 m, got {h}"
        );
    }
}
