//! Thermodynamic routines: parcel lifting, temperature conversions, moisture.
//!
//! Port of SHARPpy's `sharptab/thermo.py`.
//!
//! All temperatures are in **Celsius** unless a function name indicates
//! otherwise (e.g., `ctok`).  Pressures are in **hPa (mb)**.
//!
//! Functions that accept scalar `f64` inputs never fail -- they return `f64`.
//! `Option`-aware wrappers are provided where callers need missing-value
//! propagation.

use crate::constants::{ROCP, ZEROCNK};

// -------------------------------------------------------------------------
// Internal coefficients (match SHARPpy thermo.py exactly)
// -------------------------------------------------------------------------
const C1: f64 = 0.0498646455;
const C2: f64 = 2.4082965;
const C3: f64 = 7.07475;
const C4: f64 = 38.9114;
const C5: f64 = 0.0915;
const C6: f64 = 1.2035;

/// Ratio of molecular weights of water vapour to dry air.
const EPS: f64 = 0.62197;

// =========================================================================
// Temperature conversions
// =========================================================================

/// Celsius to Fahrenheit.
///
/// Formula: F = 1.8 * C + 32
#[inline]
pub fn ctof(t: f64) -> f64 {
    1.8 * t + 32.0
}

/// Fahrenheit to Celsius.
///
/// Formula: C = (F - 32) * 5/9
#[inline]
pub fn ftoc(t: f64) -> f64 {
    (t - 32.0) * (5.0 / 9.0)
}

/// Kelvin to Celsius.
///
/// Formula: C = K - 273.15
#[inline]
pub fn ktoc(t: f64) -> f64 {
    t - ZEROCNK
}

/// Celsius to Kelvin.
///
/// Formula: K = C + 273.15
#[inline]
pub fn ctok(t: f64) -> f64 {
    t + ZEROCNK
}

/// Kelvin to Fahrenheit.
#[inline]
pub fn ktof(t: f64) -> f64 {
    ctof(ktoc(t))
}

/// Fahrenheit to Kelvin.
#[inline]
pub fn ftok(t: f64) -> f64 {
    ctok(ftoc(t))
}

// =========================================================================
// Moisture
// =========================================================================

/// Saturation vapor pressure (hPa) at temperature `t` (C).
///
/// Uses the polynomial approximation from SHARPpy (8th-degree rational).
///
/// ```text
/// pol = 0.99999683 + t*(-9.082695e-03 + t*(...))
/// vappres = 6.1078 / pol^8
/// ```
pub fn vappres(t: f64) -> f64 {
    let pol = t * (1.1112018e-17 + t * (-3.0994571e-20));
    let pol = t * (2.1874425e-13 + t * (-1.789232e-15 + pol));
    let pol = t * (4.3884180e-09 + t * (-2.988388e-11 + pol));
    let pol = t * (7.8736169e-05 + t * (-6.111796e-07 + pol));
    let pol = 0.99999683 + t * (-9.082695e-03 + pol);
    6.1078 / pol.powi(8)
}

/// Mixing ratio (g/kg) given pressure `p` (hPa) and temperature `t` (C).
///
/// ```text
/// x = 0.02 * (t - 12.5 + 7500/p)
/// wfw = 1 + 0.0000045*p + 0.0014*x^2
/// fwesw = wfw * vappres(t)
/// w = 621.97 * fwesw / (p - fwesw)
/// ```
pub fn mixratio(p: f64, t: f64) -> f64 {
    let x = 0.02 * (t - 12.5 + 7500.0 / p);
    let wfw = 1.0 + 0.0000045 * p + 0.0014 * x * x;
    let fwesw = wfw * vappres(t);
    621.97 * (fwesw / (p - fwesw))
}

/// Temperature (C) at a given mixing ratio `w` (g/kg) and pressure `p` (hPa).
///
/// Empirical polynomial from the original SHARP code.
///
/// ```text
/// x = log10(w * p / (622 + w))
/// T = 10^(C1*x + C2) - C3 + C4*(10^(C5*x) - C6)^2 - 273.15
/// ```
pub fn temp_at_mixrat(w: f64, p: f64) -> f64 {
    let x = (w * p / (622.0 + w)).log10();
    10.0_f64.powf(C1 * x + C2) - C3 + C4 * (10.0_f64.powf(C5 * x) - C6).powi(2) - ZEROCNK
}

/// Temperature (C) from vapor pressure (hPa) via Clausius-Clapeyron inversion.
///
/// ```text
/// a = L / Rv,  b = 1/T0
/// T_K = -1 / ((1/a) * ln(e/e_s0) - b)
/// ```
///
/// Uses L = 2.5e6 J/kg, Rv = 461.5 J/(kg K), e_s0 = 6.11 hPa, T0 = 273.15 K.
pub fn temp_at_vappres(e: f64) -> f64 {
    let l = 2.5e6_f64;
    let r_v = 461.5_f64;
    let t_o = 273.15_f64;
    let e_so = 6.11_f64;
    let a = l / r_v;
    let b = 1.0 / t_o;
    let exponent = (1.0 / a) * (e / e_so).ln() - b;
    ktoc((-exponent).recip())
}

/// Relative humidity (%) given pressure, temperature, and dewpoint (all hPa/C).
///
/// ```text
/// RH = 100 * vappres(td) / vappres(t)
/// ```
pub fn relh(_p: f64, t: f64, td: f64) -> f64 {
    100.0 * vappres(td) / vappres(t)
}

/// Virtual temperature (C).
///
/// Returns `t` unchanged when `td` is not available (mirrors SHARPpy
/// behaviour for masked dewpoints).
///
/// ```text
/// w = 0.001 * mixratio(p, td)
/// Tv = Tk * (1 + w/eps) / (1 + w) - 273.15
/// ```
pub fn virtemp(p: f64, t: f64, td: Option<f64>) -> f64 {
    match td {
        None => t,
        Some(td_val) => {
            let tk = t + ZEROCNK;
            let w = 0.001 * mixratio(p, td_val);
            let vt = (tk * (1.0 + w / EPS) / (1.0 + w)) - ZEROCNK;
            if vt.is_nan() || vt.is_infinite() {
                t
            } else {
                vt
            }
        }
    }
}

// =========================================================================
// Potential temperature family
// =========================================================================

/// Potential temperature (C) of a parcel at pressure `p` (hPa) with
/// temperature `t` (C), referenced to `p2` (hPa, default 1000).
///
/// ```text
/// theta = (T + 273.15) * (p2/p)^ROCP - 273.15
/// ```
pub fn theta(p: f64, t: f64, p2: f64) -> f64 {
    (t + ZEROCNK) * (p2 / p).powf(ROCP) - ZEROCNK
}

/// Equivalent potential temperature (C).
///
/// Lifts parcel dry-adiabatically to LCL, then moist-adiabatically to
/// 100 hPa, then computes theta referenced to 1000 hPa.
///
/// ```text
/// theta_e = theta(100, wetlift(p_lcl, t_lcl, 100), 1000)
/// ```
pub fn thetae(p: f64, t: f64, td: f64) -> f64 {
    let (p2, t2) = drylift(p, t, td);
    theta(100.0, wetlift(p2, t2, 100.0), 1000.0)
}

/// Wet-bulb potential temperature (C).
///
/// Lifts parcel to LCL then moist-adiabatically to 1000 hPa.
///
/// ```text
/// theta_w = wetlift(p_lcl, t_lcl, 1000)
/// ```
pub fn thetaw(p: f64, t: f64, td: f64) -> f64 {
    let (p2, t2) = drylift(p, t, td);
    wetlift(p2, t2, 1000.0)
}

// =========================================================================
// Parcel lifting
// =========================================================================

/// LCL temperature (C) given parcel temperature and dewpoint (both C).
///
/// Empirical formula after Bolton (1980) as implemented in the original
/// SHARP code.
///
/// ```text
/// s = t - td
/// dlt = s * (1.2185 + 0.001278*t + s*(-0.00219 + 1.173e-5*s - 0.0000052*t))
/// T_lcl = t - dlt
/// ```
pub fn lcltemp(t: f64, td: f64) -> f64 {
    let s = t - td;
    let dlt = s * (1.2185 + 0.001278 * t + s * (-0.00219 + 1.173e-5 * s - 0.0000052 * t));
    t - dlt
}

/// Pressure level (hPa) of a parcel with potential temperature `theta` (C)
/// at temperature `t` (C).
///
/// Inverse of `theta()`:
/// ```text
/// p = 1000 / (theta_K / T_K)^(1/ROCP)
/// ```
pub fn thalvl(theta_c: f64, t: f64) -> f64 {
    let t_k = t + ZEROCNK;
    let theta_k = theta_c + ZEROCNK;
    1000.0 / (theta_k / t_k).powf(1.0 / ROCP)
}

/// Dry-lift a parcel to its LCL.
///
/// Returns `(lcl_pressure, lcl_temperature)` in (hPa, C).
pub fn drylift(p: f64, t: f64, td: f64) -> (f64, f64) {
    let t2 = lcltemp(t, td);
    let p2 = thalvl(theta(p, t, 1000.0), t2);
    (p2, t2)
}

/// Wobus function for moist-adiabat computation (scalar).
///
/// Returns the correction to theta for saturated potential temperature.
///
/// Two polynomial branches selected by `(t - 20)`:
/// - `t_adj <= 0`: `15.13 / npol^4`
/// - `t_adj > 0`:  `29.93 / ppol^4 + 0.96*t_adj - 14.8`
///
/// Reference: Wobus (1973), with caveats from Davies-Jones (2008).
pub fn wobf(t: f64) -> f64 {
    let t = t - 20.0;
    if t <= 0.0 {
        let npol = 1.0
            + t * (-8.841660499999999e-3
                + t * (1.4714143e-4
                    + t * (-9.671989000000001e-7 + t * (-3.2607217e-8 + t * (-3.8598073e-10)))));
        15.13 / npol.powi(4)
    } else {
        let ppol = t
            * (4.9618922e-07
                + t * (-6.1059365e-09
                    + t * (3.9401551e-11 + t * (-1.2588129e-13 + t * (1.6688280e-16)))));
        let ppol = 1.0 + t * (3.6182989e-03 + t * (-1.3603273e-05 + ppol));
        (29.93 / ppol.powi(4)) + (0.96 * t) - 14.8
    }
}

/// Temperature (C) of a saturated parcel lifted to pressure `p` (hPa),
/// given saturated potential temperature `thetam` (C).
///
/// Iterative solver; `conv` is the convergence criterion in C (default 0.1).
///
/// This is a Newton-Raphson iteration matching the SHARPpy implementation
/// exactly (including the first-pass / successive-pass structure).
pub fn satlift(p: f64, thetam: f64, conv: f64) -> f64 {
    if (p - 1000.0).abs() - 0.001 <= 0.0 {
        return thetam;
    }

    let pwrp = (p / 1000.0).powf(ROCP);

    // First pass
    let mut t1 = (thetam + ZEROCNK) * pwrp - ZEROCNK;
    let mut e1 = wobf(t1) - wobf(thetam);
    let mut rate = 1.0_f64;

    let mut t2 = t1 - e1 * rate;
    let mut e2 = (t2 + ZEROCNK) / pwrp - ZEROCNK;
    e2 += wobf(t2) - wobf(e2) - thetam;
    let mut eor = e2 * rate;

    // Successive passes (cap at 200 iterations for safety)
    let mut iter = 0;
    while eor.abs() - conv > 0.0 && iter < 200 {
        rate = (t2 - t1) / (e2 - e1);
        t1 = t2;
        e1 = e2;
        t2 = t1 - e1 * rate;
        e2 = (t2 + ZEROCNK) / pwrp - ZEROCNK;
        e2 += wobf(t2) - wobf(e2) - thetam;
        eor = e2 * rate;
        iter += 1;
    }
    t2 - eor
}

/// Moist-adiabatic lift: temperature (C) when lifting from `p` to `p2`.
///
/// Computes the saturated potential temperature at (p, t), then calls
/// `satlift` to find the temperature at `p2`.
///
/// ```text
/// thta = theta(p, t, 1000)
/// thetam = thta - wobf(thta) + wobf(t)
/// T_new = satlift(p2, thetam)
/// ```
pub fn wetlift(p: f64, t: f64, p2: f64) -> f64 {
    let thta = theta(p, t, 1000.0);
    let thetam = thta - wobf(thta) + wobf(t);
    satlift(p2, thetam, 0.1)
}

/// Temperature (C) of a parcel defined by `(p, t, td)` lifted to `lev` (hPa).
///
/// Dry-adiabatic to LCL, then moist-adiabatic above.
pub fn lifted(p: f64, t: f64, td: f64, lev: f64) -> f64 {
    let (p2, t2) = drylift(p, t, td);
    wetlift(p2, t2, lev)
}

/// Wet-bulb temperature (C) for a parcel at `(p, t, td)`.
///
/// Lifts dry-adiabatically to LCL, then moist-adiabatically back down
/// to the original pressure.
pub fn wetbulb(p: f64, t: f64, td: f64) -> f64 {
    let (p2, t2) = drylift(p, t, td);
    wetlift(p2, t2, p)
}

// =========================================================================
// Option-aware convenience wrappers
// =========================================================================

/// Lift a parcel, propagating `None` for any missing input.
pub fn drylift_opt(p: Option<f64>, t: Option<f64>, td: Option<f64>) -> Option<(f64, f64)> {
    Some(drylift(p?, t?, td?))
}

/// Wet-bulb temperature, propagating `None`.
pub fn wetbulb_opt(p: Option<f64>, t: Option<f64>, td: Option<f64>) -> Option<f64> {
    Some(wetbulb(p?, t?, td?))
}

/// Virtual temperature, propagating `None` for p and t (td may be None).
pub fn virtemp_opt(p: Option<f64>, t: Option<f64>, td: Option<f64>) -> Option<f64> {
    Some(virtemp(p?, t?, td))
}

/// Potential temperature, propagating `None`.
pub fn theta_opt(p: Option<f64>, t: Option<f64>, p2: f64) -> Option<f64> {
    Some(theta(p?, t?, p2))
}

/// Equivalent potential temperature, propagating `None`.
pub fn thetae_opt(p: Option<f64>, t: Option<f64>, td: Option<f64>) -> Option<f64> {
    Some(thetae(p?, t?, td?))
}

/// Wet-bulb potential temperature, propagating `None`.
pub fn thetaw_opt(p: Option<f64>, t: Option<f64>, td: Option<f64>) -> Option<f64> {
    Some(thetaw(p?, t?, td?))
}

/// Mixing ratio, propagating `None`.
pub fn mixratio_opt(p: Option<f64>, t: Option<f64>) -> Option<f64> {
    Some(mixratio(p?, t?))
}

/// Temperature at mixing ratio, propagating `None`.
pub fn temp_at_mixrat_opt(w: Option<f64>, p: Option<f64>) -> Option<f64> {
    Some(temp_at_mixrat(w?, p?))
}

/// Relative humidity, propagating `None`.
pub fn relh_opt(p: Option<f64>, t: Option<f64>, td: Option<f64>) -> Option<f64> {
    Some(relh(p?, t?, td?))
}

/// Lifted parcel temperature, propagating `None`.
pub fn lifted_opt(
    p: Option<f64>,
    t: Option<f64>,
    td: Option<f64>,
    lev: Option<f64>,
) -> Option<f64> {
    Some(lifted(p?, t?, td?, lev?))
}

/// Wetlift, propagating `None`.
pub fn wetlift_opt(p: Option<f64>, t: Option<f64>, p2: Option<f64>) -> Option<f64> {
    Some(wetlift(p?, t?, p2?))
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: assert two f64 values are within `eps` of each other.
    fn assert_close(a: f64, b: f64, eps: f64, msg: &str) {
        let diff = (a - b).abs();
        assert!(diff < eps, "{msg}: {a} vs {b}, diff={diff}, eps={eps}");
    }

    // ---------------------------------------------------------------
    // Unit conversions
    // ---------------------------------------------------------------

    #[test]
    fn test_ctof() {
        assert_close(ctof(0.0), 32.0, 1e-10, "ctof(0)");
        assert_close(ctof(100.0), 212.0, 1e-10, "ctof(100)");
        assert_close(ctof(-40.0), -40.0, 1e-10, "ctof(-40)");
    }

    #[test]
    fn test_ftoc() {
        assert_close(ftoc(32.0), 0.0, 1e-10, "ftoc(32)");
        assert_close(ftoc(212.0), 100.0, 1e-10, "ftoc(212)");
        assert_close(ftoc(-40.0), -40.0, 1e-10, "ftoc(-40)");
    }

    #[test]
    fn test_ktoc_ctok() {
        assert_close(ktoc(273.15), 0.0, 1e-10, "ktoc");
        assert_close(ctok(0.0), 273.15, 1e-10, "ctok");
    }

    #[test]
    fn test_ktof_ftok() {
        assert_close(ktof(273.15), 32.0, 1e-10, "ktof");
        assert_close(ftok(32.0), 273.15, 1e-10, "ftok");
    }

    #[test]
    fn test_roundtrip_conversions() {
        // C -> F -> C
        assert_close(ftoc(ctof(25.0)), 25.0, 1e-10, "C->F->C");
        // C -> K -> C
        assert_close(ktoc(ctok(-10.0)), -10.0, 1e-10, "C->K->C");
        // F -> K -> F
        assert_close(ktof(ftok(72.0)), 72.0, 1e-10, "F->K->F");
    }

    // ---------------------------------------------------------------
    // Vapor pressure / mixing ratio
    // ---------------------------------------------------------------

    #[test]
    fn test_vappres() {
        // At 0 C, saturation vapor pressure = 6.1078 hPa (polynomial base)
        assert_close(vappres(0.0), 6.1078, 0.001, "vappres(0)");
        // At 20 C
        assert_close(vappres(20.0), 23.37, 0.05, "vappres(20)");
        // At 25 C
        assert_close(vappres(25.0), 31.671, 0.01, "vappres(25)");
        // At -10 C
        assert_close(vappres(-10.0), 2.8627, 0.01, "vappres(-10)");
    }

    #[test]
    fn test_mixratio() {
        // 1000 hPa, 20 C dewpoint
        let w = mixratio(1000.0, 20.0);
        assert_close(w, 14.91, 0.1, "mixratio(1000,20)");
        // 850 hPa, 10 C
        let w2 = mixratio(850.0, 10.0);
        assert!(w2 > 8.0 && w2 < 11.0, "mixratio(850,10) = {w2}");
        // 1000 hPa, 15 C
        assert_close(mixratio(1000.0, 15.0), 10.834, 0.01, "mixratio(1000,15)");
    }

    #[test]
    fn test_temp_at_mixrat() {
        // Round-trip: mixratio -> temp_at_mixrat
        let w = mixratio(1000.0, 20.0);
        let t_back = temp_at_mixrat(w, 1000.0);
        assert_close(t_back, 20.0, 0.5, "temp_at_mixrat round-trip at 20C");
    }

    #[test]
    fn test_temp_at_vappres_roundtrip() {
        let e = vappres(20.0);
        let t_back = temp_at_vappres(e);
        assert_close(t_back, 20.0, 0.5, "temp_at_vappres round-trip at 20C");
    }

    // ---------------------------------------------------------------
    // Relative humidity / virtual temperature
    // ---------------------------------------------------------------

    #[test]
    fn test_relh() {
        // Saturated
        assert_close(relh(1000.0, 20.0, 20.0), 100.0, 1e-6, "relh saturated");
        // Typical spread
        let rh = relh(1000.0, 30.0, 10.0);
        assert!(rh > 20.0 && rh < 40.0, "relh(1000,30,10) = {rh}");
    }

    #[test]
    fn test_virtemp() {
        // Moist air: Tv > T
        let vt = virtemp(1000.0, 20.0, Some(20.0));
        assert!(vt > 20.0, "virtemp should exceed t for moist air");
        assert_close(vt, 22.625, 0.1, "virtemp(1000,20,20)");
        // Missing dewpoint
        assert_close(
            virtemp(1000.0, 20.0, None),
            20.0,
            1e-10,
            "virtemp missing td",
        );
    }

    // ---------------------------------------------------------------
    // Potential temperature
    // ---------------------------------------------------------------

    #[test]
    fn test_theta() {
        // At 1000 hPa, theta == t
        assert_close(theta(1000.0, 20.0, 1000.0), 20.0, 1e-10, "theta at 1000");
        // 850 hPa, 10 C
        assert_close(theta(850.0, 10.0, 1000.0), 23.458, 0.01, "theta(850,10)");
        // 500 hPa, -10 C
        assert_close(theta(500.0, -10.0, 1000.0), 47.633, 0.01, "theta(500,-10)");
    }

    #[test]
    fn test_thalvl() {
        // Inverse of theta: thalvl(theta(p,t), t) ~ p
        let th = theta(850.0, 10.0, 1000.0);
        let p_back = thalvl(th, 10.0);
        assert_close(p_back, 850.0, 0.01, "thalvl round-trip");
    }

    // ---------------------------------------------------------------
    // Wobus function
    // ---------------------------------------------------------------

    #[test]
    fn test_wobf() {
        // t = 20 => t-20 = 0 => npol branch, npol=1 => 15.13
        assert_close(wobf(20.0), 15.13, 0.01, "wobf(20)");
        // Cold
        let w_cold = wobf(-20.0);
        assert!(w_cold > 0.0, "wobf(-20) should be positive: {w_cold}");
        // Warm
        let w_warm = wobf(40.0);
        assert!(w_warm > 0.0, "wobf(40) should be positive: {w_warm}");
    }

    #[test]
    fn test_wobf_continuity() {
        // Check that the two branches give similar values near the boundary (t=20)
        let below = wobf(19.99);
        let above = wobf(20.01);
        let diff = (below - above).abs();
        assert!(diff < 0.1, "wobf discontinuity at boundary: {diff}");
    }

    // ---------------------------------------------------------------
    // LCL / dry lift
    // ---------------------------------------------------------------

    #[test]
    fn test_lcltemp() {
        // Saturated: LCL temp == t
        assert_close(lcltemp(20.0, 20.0), 20.0, 1e-10, "lcltemp saturated");
        // Typical spread
        let lcl_t = lcltemp(30.0, 20.0);
        assert!(lcl_t < 30.0 && lcl_t > 15.0, "lcltemp(30,20) = {lcl_t}");
    }

    #[test]
    fn test_drylift() {
        let (p_lcl, t_lcl) = drylift(1000.0, 30.0, 20.0);
        assert!(p_lcl < 1000.0, "LCL p < surface");
        assert!(p_lcl > 700.0, "LCL p > 700");
        assert!(t_lcl < 30.0, "LCL t < sfc t");
    }

    #[test]
    fn test_drylift_saturated() {
        // When t == td, LCL is at the surface
        let (p_lcl, t_lcl) = drylift(1000.0, 20.0, 20.0);
        assert_close(p_lcl, 1000.0, 1.0, "drylift saturated p");
        assert_close(t_lcl, 20.0, 0.1, "drylift saturated t");
    }

    // ---------------------------------------------------------------
    // Moist lifting
    // ---------------------------------------------------------------

    #[test]
    fn test_satlift() {
        // At 1000 hPa, returns thetam unchanged
        assert_close(satlift(1000.0, 15.0, 0.1), 15.0, 0.01, "satlift at 1000");
        // Lift to 500 hPa => much colder
        let t500 = satlift(500.0, 15.0, 0.1);
        assert!(t500 < 0.0, "satlift to 500 should be < 0: {t500}");
    }

    #[test]
    fn test_wetlift() {
        // Lifting from 850 to 700 should cool
        let t700 = wetlift(850.0, 10.0, 700.0);
        assert!(t700 < 10.0, "wetlift 850->700 should cool");
        assert!(t700 > -20.0, "wetlift shouldn't be extremely cold: {t700}");
    }

    #[test]
    fn test_lifted() {
        // 1000 hPa, 30C, Td=20C, lift to 500
        let t500 = lifted(1000.0, 30.0, 20.0, 500.0);
        assert!(t500 < 0.0 && t500 > -30.0, "lifted to 500: {t500}");
    }

    #[test]
    fn test_lifted_extreme() {
        // Very dry parcel lifted to 200 hPa
        let t200 = lifted(1000.0, 35.0, -5.0, 200.0);
        assert!(t200 < -40.0, "lifted to 200 should be very cold: {t200}");
    }

    // ---------------------------------------------------------------
    // Wet-bulb, theta-w, theta-e
    // ---------------------------------------------------------------

    #[test]
    fn test_wetbulb() {
        // Saturated: wetbulb ~ t
        let wb_sat = wetbulb(1000.0, 20.0, 20.0);
        assert_close(wb_sat, 20.0, 0.3, "wetbulb saturated");
        // Dry: td < wb < t
        let wb = wetbulb(1000.0, 30.0, 15.0);
        assert!(wb > 15.0 && wb < 30.0, "wetbulb(1000,30,15) = {wb}");
        // Specific value
        assert_close(
            wetbulb(1000.0, 25.0, 15.0),
            18.6,
            0.5,
            "wetbulb(1000,25,15)",
        );
    }

    #[test]
    fn test_thetaw() {
        let tw = thetaw(850.0, 10.0, 5.0);
        assert!(tw > -5.0 && tw < 20.0, "thetaw(850,10,5) = {tw}");
    }

    #[test]
    fn test_thetae() {
        // theta_e > theta for moist air
        let te = thetae(1000.0, 30.0, 20.0);
        let th = theta(1000.0, 30.0, 1000.0);
        assert!(te > th, "thetae should exceed theta");
        // Typical summer surface: 50-100 C range
        assert!(te > 50.0 && te < 100.0, "thetae(1000,30,20) = {te}");
    }

    #[test]
    fn test_thetae_specific() {
        let te = thetae(1000.0, 25.0, 15.0);
        assert!(te > 55.0 && te < 70.0, "thetae(1000,25,15) = {te}");
    }

    // ---------------------------------------------------------------
    // Option-wrapped variants
    // ---------------------------------------------------------------

    #[test]
    fn test_option_some() {
        assert!(theta_opt(Some(850.0), Some(10.0), 1000.0).is_some());
        assert!(wetlift_opt(Some(850.0), Some(10.0), Some(700.0)).is_some());
        assert!(lifted_opt(Some(1000.0), Some(30.0), Some(20.0), Some(500.0)).is_some());
        assert!(drylift_opt(Some(1000.0), Some(30.0), Some(20.0)).is_some());
        assert!(wetbulb_opt(Some(1000.0), Some(30.0), Some(20.0)).is_some());
        assert!(thetaw_opt(Some(850.0), Some(10.0), Some(5.0)).is_some());
        assert!(thetae_opt(Some(1000.0), Some(30.0), Some(20.0)).is_some());
        assert!(mixratio_opt(Some(1000.0), Some(20.0)).is_some());
        assert!(temp_at_mixrat_opt(Some(10.0), Some(1000.0)).is_some());
        assert!(relh_opt(Some(1000.0), Some(30.0), Some(20.0)).is_some());
    }

    #[test]
    fn test_option_none() {
        assert!(theta_opt(None, Some(10.0), 1000.0).is_none());
        assert!(wetlift_opt(Some(850.0), None, Some(700.0)).is_none());
        assert!(lifted_opt(Some(1000.0), Some(30.0), None, Some(500.0)).is_none());
        assert!(drylift_opt(None, Some(30.0), Some(20.0)).is_none());
        assert!(wetbulb_opt(Some(1000.0), None, Some(20.0)).is_none());
        assert!(thetaw_opt(Some(850.0), Some(10.0), None).is_none());
        assert!(thetae_opt(None, Some(30.0), Some(20.0)).is_none());
        assert!(mixratio_opt(None, Some(20.0)).is_none());
        assert!(temp_at_mixrat_opt(Some(10.0), None).is_none());
        assert!(relh_opt(Some(1000.0), None, Some(20.0)).is_none());
    }

    #[test]
    fn test_virtemp_opt() {
        // p and t present, td present
        assert!(virtemp_opt(Some(1000.0), Some(20.0), Some(15.0)).is_some());
        // p and t present, td None => still returns Some(t)
        assert!(virtemp_opt(Some(1000.0), Some(20.0), None).is_some());
        // p missing => None
        assert!(virtemp_opt(None, Some(20.0), Some(15.0)).is_none());
    }

    // ---------------------------------------------------------------
    // Consistency / cross-checks
    // ---------------------------------------------------------------

    #[test]
    fn test_theta_thalvl_inverse() {
        // For a range of pressures, theta -> thalvl should round-trip
        for &p in &[950.0, 850.0, 700.0, 500.0, 300.0, 200.0] {
            let t = -5.0; // arbitrary temperature
            let th = theta(p, t, 1000.0);
            let p_back = thalvl(th, t);
            assert_close(p_back, p, 0.1, &format!("theta/thalvl roundtrip at {p}"));
        }
    }

    #[test]
    fn test_wetbulb_between_td_and_t() {
        // Wet-bulb should always be between td and t
        for &(t, td) in &[(30.0, 10.0), (25.0, 20.0), (15.0, 5.0), (0.0, -10.0)] {
            let wb = wetbulb(1000.0, t, td);
            assert!(
                wb >= td - 0.5 && wb <= t + 0.5,
                "wetbulb({t},{td}) = {wb} not between {td} and {t}"
            );
        }
    }

    #[test]
    fn test_relh_range() {
        // RH should be 0-100% when td <= t
        for &td in &[-20.0, -10.0, 0.0, 10.0, 20.0] {
            let rh = relh(1000.0, 25.0, td);
            assert!(rh > 0.0 && rh <= 100.0, "relh(1000,25,{td}) = {rh}");
        }
    }

    // ---------------------------------------------------------------
    // Exact cross-validation against SHARPpy Python output
    // (tolerance 1e-10 == floating-point identical)
    // ---------------------------------------------------------------

    #[test]
    fn xval_vappres() {
        assert_close(vappres(0.0), 6.107954896017587, 1e-10, "xval vappres(0)");
        assert_close(vappres(25.0), 31.670078513287617, 1e-10, "xval vappres(25)");
        assert_close(
            vappres(-10.0),
            2.862720771104215,
            1e-10,
            "xval vappres(-10)",
        );
    }

    #[test]
    fn xval_mixratio() {
        assert_close(
            mixratio(1000.0, 20.0),
            14.955321833573537,
            1e-10,
            "xval mixratio(1000,20)",
        );
        assert_close(
            mixratio(1000.0, 15.0),
            10.834359059077558,
            1e-10,
            "xval mixratio(1000,15)",
        );
    }

    #[test]
    fn xval_theta() {
        assert_close(
            theta(850.0, 10.0, 1000.0),
            23.457812111895066,
            1e-10,
            "xval theta(850,10)",
        );
        assert_close(
            theta(500.0, -10.0, 1000.0),
            47.633437386332787,
            1e-10,
            "xval theta(500,-10)",
        );
    }

    #[test]
    fn xval_wobf() {
        assert_close(wobf(20.0), 15.130000000000001, 1e-10, "xval wobf(20)");
        assert_close(wobf(-20.0), 2.268446379039877, 1e-10, "xval wobf(-20)");
        assert_close(wobf(40.0), 27.230425856811724, 1e-10, "xval wobf(40)");
    }

    #[test]
    fn xval_lcltemp() {
        assert_close(
            lcltemp(30.0, 20.0),
            17.654470000000000,
            1e-10,
            "xval lcltemp(30,20)",
        );
    }

    #[test]
    fn xval_drylift() {
        let (p, t) = drylift(1000.0, 30.0, 20.0);
        assert_close(p, 864.574182648404303, 1e-8, "xval drylift p");
        assert_close(t, 17.654470000000000, 1e-10, "xval drylift t");
    }

    #[test]
    fn xval_wetlift() {
        assert_close(
            wetlift(850.0, 10.0, 700.0),
            1.792703825115132,
            1e-8,
            "xval wetlift",
        );
    }

    #[test]
    fn xval_lifted() {
        assert_close(
            lifted(1000.0, 30.0, 20.0, 500.0),
            -3.428511862540861,
            1e-8,
            "xval lifted",
        );
    }

    #[test]
    fn xval_wetbulb() {
        assert_close(
            wetbulb(1000.0, 25.0, 15.0),
            18.545800905204654,
            1e-8,
            "xval wetbulb",
        );
    }

    #[test]
    fn xval_thetaw() {
        assert_close(
            thetaw(850.0, 10.0, 5.0),
            14.175166637188527,
            1e-8,
            "xval thetaw",
        );
    }

    #[test]
    fn xval_thetae() {
        assert_close(
            thetae(1000.0, 30.0, 20.0),
            76.777259652772216,
            1e-8,
            "xval thetae(30,20)",
        );
        assert_close(
            thetae(1000.0, 25.0, 15.0),
            58.894960198293631,
            1e-8,
            "xval thetae(25,15)",
        );
    }

    #[test]
    fn xval_virtemp() {
        assert_close(
            virtemp(1000.0, 20.0, Some(20.0)),
            22.625400511641203,
            1e-10,
            "xval virtemp",
        );
    }

    #[test]
    fn xval_relh() {
        assert_close(
            relh(1000.0, 30.0, 10.0),
            28.923793739840995,
            1e-10,
            "xval relh",
        );
    }

    #[test]
    fn xval_temp_at_mixrat() {
        assert_close(
            temp_at_mixrat(14.0, 1000.0),
            19.077251998769270,
            1e-10,
            "xval temp_at_mixrat",
        );
    }
}
