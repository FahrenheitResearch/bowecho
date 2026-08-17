//! Utility functions: unit conversions, quality control, wind vector math,
//! and formatting helpers.
//!
//! Numpy masked arrays are replaced by `Option<f64>` — `None` represents a
//! masked / missing value.  Slice-oriented variants accept `&[Option<f64>]`
//! and return `Vec<Option<f64>>`.

use crate::constants::{MISSING, TOL};

// =========================================================================
// Quality control
// =========================================================================

/// Returns `true` when `val` is present (i.e. not `None`).
///
/// This is the Rust equivalent of SHARPpy's `QC()`, which returns `False`
/// for masked values.
///
/// # Examples
/// ```
/// # use sharprs::utils::qc;
/// assert!(qc(Some(5.0)));
/// assert!(!qc(None));
/// ```
#[inline]
pub fn qc(val: Option<f64>) -> bool {
    val.is_some()
}

/// Returns `true` when `val` is present **and** not equal to [`MISSING`].
///
/// Use this when working with raw f64 values that may carry the sentinel.
#[inline]
pub fn qc_value(val: f64) -> bool {
    (val - MISSING).abs() > TOL
}

/// Converts a raw f64 to `Option<f64>`, mapping [`MISSING`] to `None`.
#[inline]
pub fn from_raw(val: f64) -> Option<f64> {
    if qc_value(val) {
        Some(val)
    } else {
        None
    }
}

// =========================================================================
// Formatting helpers
// =========================================================================

/// Convert a value to an integer string by rounding.
///
/// Returns `"--"` for `None` (the masked / missing case).
///
/// Equivalent to SHARPpy's `INT2STR`.
#[inline]
pub fn int2str(val: Option<f64>) -> String {
    match val {
        Some(v) if v.is_nan() => "--".to_string(),
        Some(v) => format!("{}", v.round() as i64),
        None => "--".to_string(),
    }
}

/// Convert a value to a float string with the given decimal precision.
///
/// Returns `"--"` for `None` or NaN.
///
/// Equivalent to SHARPpy's `FLOAT2STR`.
#[inline]
pub fn float2str(val: Option<f64>, precision: usize) -> String {
    match val {
        Some(v) if v.is_nan() => "--".to_string(),
        Some(v) => format!("{:.prec$}", v, prec = precision),
        None => "--".to_string(),
    }
}

// =========================================================================
// Unit conversions  (scalar)
// =========================================================================

/// Metres per second to knots.  1 m/s = 1.94384449 kt.
#[inline]
pub fn ms2kts(val: f64) -> f64 {
    val * 1.94384449
}

/// Knots to metres per second.  1 kt = 0.514444 m/s.
#[inline]
pub fn kts2ms(val: f64) -> f64 {
    val * 0.514444
}

/// Metres per second to miles per hour.  1 m/s = 2.23694 mph.
#[inline]
pub fn ms2mph(val: f64) -> f64 {
    val * 2.23694
}

/// Miles per hour to metres per second.  1 mph = 0.44704 m/s.
#[inline]
pub fn mph2ms(val: f64) -> f64 {
    val * 0.44704
}

/// Miles per hour to knots.  1 mph = 0.868976 kt.
#[inline]
pub fn mph2kts(val: f64) -> f64 {
    val * 0.868976
}

/// Knots to miles per hour.  1 kt = 1.15078 mph.
#[inline]
pub fn kts2mph(val: f64) -> f64 {
    val * 1.15078
}

/// Metres to feet.  1 m = 3.2808399 ft.
#[inline]
pub fn m2ft(val: f64) -> f64 {
    val * 3.2808399
}

/// Feet to metres.  1 ft = 0.3048 m.
#[inline]
pub fn ft2m(val: f64) -> f64 {
    val * 0.3048
}

/// Inches to centimetres.  1 in = 2.54 cm.
#[inline]
pub fn in2cm(val: f64) -> f64 {
    val * 2.54
}

/// Centimetres to inches.  1 cm = 1/2.54 in.
#[inline]
pub fn cm2in(val: f64) -> f64 {
    val / 2.54
}

// =========================================================================
// Unit conversions  (Option-aware)
// =========================================================================

/// Apply a scalar conversion function to an `Option<f64>`.
///
/// Returns `None` when the input is `None`.
#[inline]
pub fn convert_opt(val: Option<f64>, f: fn(f64) -> f64) -> Option<f64> {
    val.map(f)
}

// =========================================================================
// Wind vector helpers
// =========================================================================

/// Internal: convert meteorological wind direction and speed to U, V
/// components.
///
/// Wind direction is in **meteorological degrees** (0 = north, 90 = east).
/// The resulting U is positive eastward and V is positive northward, matching
/// the standard meteorological sign convention (wind *from* the given
/// direction).
///
/// # Arguments
/// * `wdir` — wind direction in degrees
/// * `wspd` — wind speed (any consistent unit)
#[inline]
fn vec2comp_raw(wdir: f64, wspd: f64) -> (f64, f64) {
    let wdir_rad = wdir.to_radians();
    let u = -wspd * wdir_rad.sin();
    let v = -wspd * wdir_rad.cos();
    (u, v)
}

/// Apply near-zero clamping (values with magnitude < [`TOL`] become 0.0).
#[inline]
fn clamp_tol(val: f64) -> f64 {
    if val.abs() < TOL {
        0.0
    } else {
        val
    }
}

/// Convert wind direction and speed into U and V components (scalar).
///
/// Returns `(None, None)` when either input is `None` or equal to
/// [`MISSING`].
///
/// # Arguments
/// * `wdir` — wind direction in meteorological degrees
/// * `wspd` — wind speed (output units match input units)
pub fn vec2comp(wdir: Option<f64>, wspd: Option<f64>) -> (Option<f64>, Option<f64>) {
    match (wdir, wspd) {
        (Some(d), Some(s)) if qc_value(d) && qc_value(s) => {
            let (u, v) = vec2comp_raw(d, s);
            (Some(clamp_tol(u)), Some(clamp_tol(v)))
        }
        _ => (None, None),
    }
}

/// Convert wind direction and speed arrays into U and V component arrays.
///
/// Each element is independently quality-controlled; masked inputs produce
/// masked outputs at the same index.
pub fn vec2comp_slice(
    wdir: &[Option<f64>],
    wspd: &[Option<f64>],
) -> (Vec<Option<f64>>, Vec<Option<f64>>) {
    assert_eq!(
        wdir.len(),
        wspd.len(),
        "wdir and wspd must have the same length"
    );
    let mut u_out = Vec::with_capacity(wdir.len());
    let mut v_out = Vec::with_capacity(wdir.len());
    for (&d, &s) in wdir.iter().zip(wspd.iter()) {
        let (u, v) = vec2comp(d, s);
        u_out.push(u);
        v_out.push(v);
    }
    (u_out, v_out)
}

/// Convert U and V components into wind direction and speed (scalar).
///
/// Direction is in meteorological degrees [0, 360).  Returns
/// `(None, None)` when either input is `None` or equal to [`MISSING`].
pub fn comp2vec(u: Option<f64>, v: Option<f64>) -> (Option<f64>, Option<f64>) {
    match (u, v) {
        (Some(uu), Some(vv)) if qc_value(uu) && qc_value(vv) => {
            let mut wdir = (-uu).atan2(-vv).to_degrees();
            if wdir < 0.0 {
                wdir += 360.0;
            }
            wdir = clamp_tol(wdir);
            let wspd = mag_raw(uu, vv);
            (Some(wdir), Some(wspd))
        }
        _ => (None, None),
    }
}

/// Convert U and V component arrays into direction and speed arrays.
pub fn comp2vec_slice(
    u: &[Option<f64>],
    v: &[Option<f64>],
) -> (Vec<Option<f64>>, Vec<Option<f64>>) {
    assert_eq!(u.len(), v.len(), "u and v must have the same length");
    let mut wdir_out = Vec::with_capacity(u.len());
    let mut wspd_out = Vec::with_capacity(u.len());
    for (&uu, &vv) in u.iter().zip(v.iter()) {
        let (d, s) = comp2vec(uu, vv);
        wdir_out.push(d);
        wspd_out.push(s);
    }
    (wdir_out, wspd_out)
}

// =========================================================================
// Magnitude
// =========================================================================

/// Raw vector magnitude (no missing-value handling).
#[inline]
fn mag_raw(u: f64, v: f64) -> f64 {
    (u * u + v * v).sqrt()
}

/// Compute the magnitude of a vector from its components.
///
/// Returns `None` when either component is `None` or [`MISSING`].
#[inline]
pub fn mag(u: Option<f64>, v: Option<f64>) -> Option<f64> {
    match (u, v) {
        (Some(uu), Some(vv)) if qc_value(uu) && qc_value(vv) => Some(mag_raw(uu, vv)),
        _ => None,
    }
}

/// Compute element-wise vector magnitude for paired slices.
pub fn mag_slice(u: &[Option<f64>], v: &[Option<f64>]) -> Vec<Option<f64>> {
    assert_eq!(u.len(), v.len(), "u and v must have the same length");
    u.iter()
        .zip(v.iter())
        .map(|(&uu, &vv)| mag(uu, vv))
        .collect()
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::FRAC_1_SQRT_2;

    const EPS: f64 = 1e-6;

    // ----- QC / sentinel helpers -----

    #[test]
    fn test_qc_some() {
        assert!(qc(Some(42.0)));
    }

    #[test]
    fn test_qc_none() {
        assert!(!qc(None));
    }

    #[test]
    fn test_qc_value_valid() {
        assert!(qc_value(100.0));
    }

    #[test]
    fn test_qc_value_missing() {
        assert!(!qc_value(MISSING));
    }

    #[test]
    fn test_from_raw_valid() {
        assert_eq!(from_raw(5.0), Some(5.0));
    }

    #[test]
    fn test_from_raw_missing() {
        assert_eq!(from_raw(MISSING), None);
    }

    // ----- Formatting -----

    #[test]
    fn test_int2str_normal() {
        assert_eq!(int2str(Some(3.7)), "4");
        assert_eq!(int2str(Some(-2.3)), "-2");
    }

    #[test]
    fn test_int2str_missing() {
        assert_eq!(int2str(None), "--");
    }

    #[test]
    fn test_int2str_nan() {
        assert_eq!(int2str(Some(f64::NAN)), "--");
    }

    #[test]
    fn test_float2str_normal() {
        assert_eq!(float2str(Some(3.14159), 2), "3.14");
    }

    #[test]
    fn test_float2str_missing() {
        assert_eq!(float2str(None, 3), "--");
    }

    #[test]
    fn test_float2str_nan() {
        assert_eq!(float2str(Some(f64::NAN), 1), "--");
    }

    // ----- Unit conversions -----

    #[test]
    fn test_ms2kts_roundtrip() {
        let v = 10.0;
        let kts = ms2kts(v);
        let back = kts2ms(kts);
        assert!((back - v).abs() < 0.001);
    }

    #[test]
    fn test_ms2mph_roundtrip() {
        let v = 15.0;
        let mph = ms2mph(v);
        let back = mph2ms(mph);
        assert!((back - v).abs() < 0.001);
    }

    #[test]
    fn test_mph2kts_roundtrip() {
        let v = 60.0;
        let kts = mph2kts(v);
        let back = kts2mph(kts);
        assert!((back - v).abs() < 0.01);
    }

    #[test]
    fn test_m2ft_roundtrip() {
        let v = 1000.0;
        let ft = m2ft(v);
        let back = ft2m(ft);
        assert!((back - v).abs() < 0.001);
    }

    #[test]
    fn test_in2cm_roundtrip() {
        let v = 12.0;
        let cm = in2cm(v);
        let back = cm2in(cm);
        assert!((back - v).abs() < 1e-10);
    }

    #[test]
    fn test_ms2kts_known() {
        // 1 m/s ≈ 1.944 kt
        assert!((ms2kts(1.0) - 1.94384449).abs() < EPS);
    }

    #[test]
    fn test_m2ft_known() {
        // 1 m ≈ 3.281 ft
        assert!((m2ft(1.0) - 3.2808399).abs() < EPS);
    }

    #[test]
    fn test_ft2m_known() {
        // 1 ft = 0.3048 m exactly
        assert!((ft2m(1.0) - 0.3048).abs() < EPS);
    }

    #[test]
    fn test_convert_opt_some() {
        assert_eq!(convert_opt(Some(10.0), ms2kts), Some(ms2kts(10.0)));
    }

    #[test]
    fn test_convert_opt_none() {
        assert_eq!(convert_opt(None, ms2kts), None);
    }

    // ----- Wind vector ↔ component -----

    #[test]
    fn test_vec2comp_north_wind() {
        // Wind FROM the north (360°) at 10 → u=0, v=-10
        let (u, v) = vec2comp(Some(360.0), Some(10.0));
        assert!(u.unwrap().abs() < EPS);
        assert!((v.unwrap() - (-10.0)).abs() < EPS);
    }

    #[test]
    fn test_vec2comp_south_wind() {
        // Wind FROM the south (180°) at 10 → u=0, v=10
        let (u, v) = vec2comp(Some(180.0), Some(10.0));
        assert!(u.unwrap().abs() < EPS);
        assert!((v.unwrap() - 10.0).abs() < EPS);
    }

    #[test]
    fn test_vec2comp_west_wind() {
        // Wind FROM the west (270°) at 10 → u=10, v=0
        let (u, v) = vec2comp(Some(270.0), Some(10.0));
        assert!((u.unwrap() - 10.0).abs() < EPS);
        assert!(v.unwrap().abs() < EPS);
    }

    #[test]
    fn test_vec2comp_east_wind() {
        // Wind FROM the east (90°) at 10 → u=-10, v=0
        let (u, v) = vec2comp(Some(90.0), Some(10.0));
        assert!((u.unwrap() - (-10.0)).abs() < EPS);
        assert!(v.unwrap().abs() < EPS);
    }

    #[test]
    fn test_vec2comp_sw_wind() {
        // Wind FROM 225° at 10 → u = 10*sin(225°)*-1, v = 10*cos(225°)*-1
        // sin(225°) = -√2/2, cos(225°) = -√2/2
        let (u, v) = vec2comp(Some(225.0), Some(10.0));
        let expected = 10.0 * FRAC_1_SQRT_2;
        assert!((u.unwrap() - expected).abs() < EPS);
        assert!((v.unwrap() - expected).abs() < EPS);
    }

    #[test]
    fn test_vec2comp_missing_dir() {
        let (u, v) = vec2comp(None, Some(10.0));
        assert!(u.is_none());
        assert!(v.is_none());
    }

    #[test]
    fn test_vec2comp_missing_spd() {
        let (u, v) = vec2comp(Some(180.0), None);
        assert!(u.is_none());
        assert!(v.is_none());
    }

    #[test]
    fn test_vec2comp_sentinel_missing() {
        let (u, v) = vec2comp(Some(MISSING), Some(10.0));
        assert!(u.is_none());
        assert!(v.is_none());
    }

    #[test]
    fn test_comp2vec_roundtrip() {
        let wdir_in = 225.0;
        let wspd_in = 30.0;
        let (u, v) = vec2comp(Some(wdir_in), Some(wspd_in));
        let (wdir_out, wspd_out) = comp2vec(u, v);
        assert!((wdir_out.unwrap() - wdir_in).abs() < EPS);
        assert!((wspd_out.unwrap() - wspd_in).abs() < EPS);
    }

    #[test]
    fn test_comp2vec_north() {
        // u=0, v=-10 → wind from north (360°), speed 10
        let (wdir, wspd) = comp2vec(Some(0.0), Some(-10.0));
        // atan2(0, 10) = 0 → direction = 0 or 360; SHARPpy treats 0 as 0
        // Actually: atan2(-0, -(-10)) = atan2(0, 10) = 0° — i.e. 360 wraps to 0
        assert!((wdir.unwrap() - 360.0).abs() < EPS || wdir.unwrap().abs() < EPS);
        assert!((wspd.unwrap() - 10.0).abs() < EPS);
    }

    #[test]
    fn test_comp2vec_missing() {
        let (wdir, wspd) = comp2vec(None, Some(5.0));
        assert!(wdir.is_none());
        assert!(wspd.is_none());
    }

    // ----- Magnitude -----

    #[test]
    fn test_mag_345() {
        assert!((mag(Some(3.0), Some(4.0)).unwrap() - 5.0).abs() < EPS);
    }

    #[test]
    fn test_mag_zero() {
        assert!((mag(Some(0.0), Some(0.0)).unwrap()).abs() < EPS);
    }

    #[test]
    fn test_mag_missing() {
        assert!(mag(None, Some(4.0)).is_none());
        assert!(mag(Some(3.0), None).is_none());
        assert!(mag(None, None).is_none());
    }

    #[test]
    fn test_mag_sentinel() {
        assert!(mag(Some(MISSING), Some(4.0)).is_none());
    }

    // ----- Slice variants -----

    #[test]
    fn test_vec2comp_slice_basic() {
        let wdir = vec![Some(180.0), Some(270.0), None];
        let wspd = vec![Some(10.0), Some(10.0), Some(5.0)];
        let (u, v) = vec2comp_slice(&wdir, &wspd);
        assert_eq!(u.len(), 3);
        // 180° → u ≈ 0
        assert!(u[0].unwrap().abs() < EPS);
        // 180° → v = 10
        assert!((v[0].unwrap() - 10.0).abs() < EPS);
        // 270° → u = 10
        assert!((u[1].unwrap() - 10.0).abs() < EPS);
        // missing dir → None
        assert!(u[2].is_none());
        assert!(v[2].is_none());
    }

    #[test]
    #[should_panic(expected = "same length")]
    fn test_vec2comp_slice_length_mismatch() {
        vec2comp_slice(&[Some(180.0)], &[Some(10.0), Some(20.0)]);
    }

    #[test]
    fn test_comp2vec_slice_basic() {
        let u = vec![Some(0.0), Some(-10.0)];
        let v = vec![Some(10.0), Some(0.0)];
        let (wdir, wspd) = comp2vec_slice(&u, &v);
        assert_eq!(wdir.len(), 2);
        // u=0, v=10 → from south (180°)
        assert!((wdir[0].unwrap() - 180.0).abs() < EPS);
        assert!((wspd[0].unwrap() - 10.0).abs() < EPS);
    }

    #[test]
    fn test_mag_slice_basic() {
        let u = vec![Some(3.0), None, Some(5.0)];
        let v = vec![Some(4.0), Some(2.0), Some(12.0)];
        let m = mag_slice(&u, &v);
        assert!((m[0].unwrap() - 5.0).abs() < EPS);
        assert!(m[1].is_none());
        assert!((m[2].unwrap() - 13.0).abs() < EPS);
    }
}
