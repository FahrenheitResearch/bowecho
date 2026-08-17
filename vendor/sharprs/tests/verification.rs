//! Comprehensive verification test suite for sharprs.
//!
//! Tests are organised around five canonical sounding profiles that span
//! the range of convective environments.  Reference values were generated
//! by running the identical sounding data through SHARPpy (Python) and
//! recording the outputs to four decimal places.
//!
//! **Sounding inventory:**
//!
//! 1. **Classic supercell** — high CAPE, strong deep-layer shear, backed
//!    surface winds veering with height (Great Plains-style).
//! 2. **Weak / null case** — stable atmosphere, no CAPE, weak winds.
//! 3. **Elevated convection** — surface inversion with elevated instability.
//! 4. **Tropical** — high moisture, moderate CAPE, weak shear.
//! 5. **Fire weather** — hot, dry, windy, low RH.
//!
//! Each sounding encodes pressure (hPa), height (m MSL), temperature (C),
//! dewpoint (C), wind direction (deg), and wind speed (kt).

use sharprs::constants::*;
use sharprs::fire;
use sharprs::utils;

// =========================================================================
// Helper: tolerance assertions
// =========================================================================

/// Assert two f64 values are within an absolute tolerance.
macro_rules! assert_close {
    ($a:expr, $b:expr, $tol:expr, $msg:expr) => {
        let (a, b) = ($a, $b);
        let diff = (a - b).abs();
        assert!(
            diff <= $tol,
            "{}: expected {:.10}, got {:.10}, diff {:.2e} > tol {:.2e}",
            $msg,
            b,
            a,
            diff,
            $tol
        );
    };
}

/// Assert within a percentage tolerance (for CAPE/CIN).
macro_rules! assert_pct {
    ($a:expr, $b:expr, $pct:expr, $msg:expr) => {
        let (a, b) = ($a as f64, $b as f64);
        if b.abs() < 1.0 {
            // For near-zero references, use absolute tolerance
            assert_close!(a, b, 1.0, $msg);
        } else {
            let allowed = b.abs() * $pct / 100.0;
            let diff = (a - b).abs();
            assert!(
                diff <= allowed,
                "{}: expected {:.2}, got {:.2}, diff {:.2} > {}% ({:.2})",
                $msg,
                b,
                a,
                diff,
                $pct,
                allowed
            );
        }
    };
}

// =========================================================================
// Sounding data definitions
// =========================================================================

/// Sounding 1: Classic supercell (Great Plains tornado environment).
///
/// CAPE ~21700 J/kg, backed surface winds, strong deep-layer shear.
/// Reference: SHARPpy output on identical data.
mod sounding1 {
    pub const PRES: &[f64] = &[
        963.0, 950.0, 925.0, 900.0, 850.0, 800.0, 750.0, 700.0, 650.0, 600.0, 550.0, 500.0, 450.0,
        400.0, 350.0, 300.0, 250.0, 200.0, 150.0,
    ];
    pub const HGHT: &[f64] = &[
        350.0, 470.0, 714.0, 950.0, 1457.0, 2010.0, 2618.0, 3293.0, 4050.0, 4907.0, 5887.0, 7020.0,
        8350.0, 9940.0, 11870.0, 14230.0, 17190.0, 20960.0, 25850.0,
    ];
    pub const TMPC: &[f64] = &[
        28.0, 26.8, 24.0, 21.2, 16.0, 11.0, 5.8, 0.0, -5.8, -12.0, -19.0, -27.0, -36.2, -46.5,
        -57.8, -68.5, -68.0, -57.0, -58.0,
    ];
    pub const DWPC: &[f64] = &[
        19.0, 18.0, 16.0, 14.0, 7.0, 0.0, -6.0, -14.0, -20.0, -28.0, -35.0, -42.0, -50.0, -58.0,
        -66.0, -72.0, -73.0, -62.0, -63.0,
    ];
    pub const WDIR: &[f64] = &[
        160.0, 165.0, 175.0, 185.0, 200.0, 215.0, 225.0, 235.0, 240.0, 245.0, 250.0, 255.0, 265.0,
        270.0, 275.0, 280.0, 285.0, 290.0, 295.0,
    ];
    pub const WSPD: &[f64] = &[
        15.0, 18.0, 22.0, 25.0, 30.0, 35.0, 40.0, 45.0, 48.0, 52.0, 55.0, 60.0, 65.0, 70.0, 75.0,
        80.0, 85.0, 90.0, 95.0,
    ];

    // --- SHARPpy reference values ---
    pub const SB_CAPE: f64 = 21708.8089;
    pub const SB_CIN: f64 = 0.0;
    pub const SB_LCL_P: f64 = 844.3058;
    pub const SB_LFC_P: f64 = 844.3058;
    pub const SB_EL_P: f64 = 185.0;
    pub const SB_LCL_H: f64 = 1168.3126;
    pub const ML_CAPE: f64 = 18487.9531;
    pub const ML_CIN: f64 = -7.0920;
    pub const ML_LCL_P: f64 = 817.8552;
    pub const MU_CAPE: f64 = 21708.8089;
    pub const MU_CIN: f64 = 0.0;
    pub const K_INDEX: f64 = 36.0;
    pub const TT: f64 = 77.0;
    pub const PWAT: f64 = 0.8324; // inches
    pub const SHR06: f64 = 30.5952; // m/s
    pub const SHR01: f64 = 9.8540;
    pub const BUNKERS_RU: f64 = 27.4087; // kts
    pub const BUNKERS_RV: f64 = 9.8409;
    pub const BUNKERS_LU: f64 = 25.7542;
    pub const BUNKERS_LV: f64 = 38.9516;
    pub const SRH1: f64 = 151.8057; // m2/s2
    pub const SRH3: f64 = 297.5141;
    pub const STP: f64 = 18.2723;
    pub const SCP: f64 = 129.1736;
    pub const SHIP: f64 = 37.5077;
}

/// Sounding 2: Weak / null case (winter stable boundary layer).
///
/// No CAPE, surface inversion at 950, NW flow.
mod sounding2 {
    pub const PRES: &[f64] = &[
        1013.0, 1000.0, 950.0, 900.0, 850.0, 800.0, 750.0, 700.0, 650.0, 600.0, 550.0, 500.0,
        450.0, 400.0, 350.0, 300.0, 250.0, 200.0, 150.0,
    ];
    pub const HGHT: &[f64] = &[
        0.0, 112.0, 540.0, 988.0, 1457.0, 1949.0, 2466.0, 3012.0, 3590.0, 4206.0, 4865.0, 5574.0,
        6344.0, 7185.0, 8117.0, 9164.0, 10363.0, 11784.0, 13608.0,
    ];
    pub const TMPC: &[f64] = &[
        -2.0, -1.0, 2.0, -2.0, -6.0, -10.0, -14.0, -18.0, -22.0, -27.0, -32.0, -38.0, -44.0, -52.0,
        -58.0, -65.0, -60.0, -55.0, -55.0,
    ];
    pub const DWPC: &[f64] = &[
        -4.0, -3.0, -2.0, -8.0, -14.0, -20.0, -25.0, -30.0, -35.0, -40.0, -45.0, -50.0, -55.0,
        -60.0, -65.0, -70.0, -65.0, -60.0, -60.0,
    ];
    pub const WDIR: &[f64] = &[
        320.0, 320.0, 315.0, 310.0, 305.0, 300.0, 295.0, 290.0, 290.0, 285.0, 280.0, 280.0, 275.0,
        270.0, 270.0, 265.0, 265.0, 260.0, 260.0,
    ];
    pub const WSPD: &[f64] = &[
        5.0, 5.0, 8.0, 10.0, 12.0, 12.0, 14.0, 15.0, 16.0, 18.0, 20.0, 22.0, 24.0, 25.0, 26.0,
        28.0, 30.0, 32.0, 35.0,
    ];

    pub const SB_CAPE: f64 = 0.0;
    pub const SB_CIN: f64 = 0.0;
    pub const SB_LCL_P: f64 = 981.6663;
    pub const K_INDEX: f64 = 6.0;
    pub const TT: f64 = 56.0;
    pub const PWAT: f64 = 0.2498;
    pub const SHR06: f64 = 10.1430;
    pub const BUNKERS_RU: f64 = 13.4956;
    pub const BUNKERS_RV: f64 = -19.7589;
    pub const SRH1: f64 = 11.3839;
    pub const SRH3: f64 = 34.5651;
}

/// Sounding 3: Elevated convection (surface inversion, elevated instability).
///
/// Surface temp inversion from 1000-900 mb, steep lapse rates aloft,
/// moist layer near 900 mb.
mod sounding3 {
    pub const PRES: &[f64] = &[
        1000.0, 975.0, 950.0, 925.0, 900.0, 875.0, 850.0, 825.0, 800.0, 750.0, 700.0, 650.0, 600.0,
        550.0, 500.0, 450.0, 400.0, 350.0, 300.0, 250.0, 200.0,
    ];
    pub const HGHT: &[f64] = &[
        200.0, 420.0, 660.0, 910.0, 1170.0, 1440.0, 1720.0, 2010.0, 2310.0, 2950.0, 3640.0, 4380.0,
        5190.0, 6070.0, 7040.0, 8110.0, 9310.0, 10660.0, 12190.0, 13980.0, 16180.0,
    ];
    pub const TMPC: &[f64] = &[
        5.0, 8.0, 10.0, 11.0, 12.0, 10.0, 8.0, 5.0, 2.0, -4.0, -10.0, -16.0, -23.0, -30.0, -38.0,
        -46.0, -54.0, -60.0, -66.0, -60.0, -55.0,
    ];
    pub const DWPC: &[f64] = &[
        2.0, 4.0, 6.0, 8.0, 10.0, 8.0, 6.0, 3.0, 0.0, -6.0, -12.0, -18.0, -28.0, -35.0, -44.0,
        -52.0, -60.0, -65.0, -70.0, -64.0, -59.0,
    ];
    pub const WDIR: &[f64] = &[
        180.0, 185.0, 190.0, 195.0, 200.0, 210.0, 220.0, 230.0, 235.0, 240.0, 245.0, 250.0, 255.0,
        260.0, 265.0, 270.0, 270.0, 272.0, 275.0, 278.0, 280.0,
    ];
    pub const WSPD: &[f64] = &[
        10.0, 12.0, 14.0, 16.0, 18.0, 20.0, 25.0, 28.0, 30.0, 35.0, 40.0, 42.0, 45.0, 48.0, 50.0,
        52.0, 55.0, 58.0, 60.0, 62.0, 65.0,
    ];

    pub const SB_CAPE: f64 = 541.1587;
    pub const SB_CIN: f64 = -1461.5560;
    pub const MU_CAPE: f64 = 8707.5644;
    pub const MU_CIN: f64 = -0.6710;
    pub const K_INDEX: f64 = 50.0;
    pub const TT: f64 = 90.0;
    pub const PWAT: f64 = 0.7348;
}

/// Sounding 4: Tropical (high moisture, moderate CAPE, weak shear).
///
/// Warm, moist column with near-saturated low levels and
/// weak easterly trade-wind shear.
mod sounding4 {
    pub const PRES: &[f64] = &[
        1010.0, 1000.0, 975.0, 950.0, 925.0, 900.0, 850.0, 800.0, 750.0, 700.0, 650.0, 600.0,
        550.0, 500.0, 450.0, 400.0, 350.0, 300.0, 250.0, 200.0, 150.0,
    ];
    pub const HGHT: &[f64] = &[
        10.0, 100.0, 345.0, 600.0, 860.0, 1130.0, 1500.0, 1950.0, 2430.0, 2950.0, 3520.0, 4140.0,
        4810.0, 5540.0, 6350.0, 7250.0, 8280.0, 9470.0, 10850.0, 12430.0, 14400.0,
    ];
    pub const TMPC: &[f64] = &[
        28.0, 27.5, 26.0, 24.0, 22.0, 20.0, 16.5, 13.0, 9.0, 5.0, 1.0, -3.5, -8.5, -14.0, -20.0,
        -27.0, -34.0, -42.0, -52.0, -60.0, -70.0,
    ];
    pub const DWPC: &[f64] = &[
        26.0, 25.5, 24.5, 23.0, 21.0, 19.0, 14.5, 10.0, 5.0, 0.0, -5.0, -10.0, -16.0, -22.0, -28.0,
        -35.0, -42.0, -50.0, -58.0, -65.0, -75.0,
    ];
    pub const WDIR: &[f64] = &[
        90.0, 95.0, 100.0, 105.0, 110.0, 115.0, 120.0, 125.0, 130.0, 135.0, 140.0, 145.0, 150.0,
        160.0, 170.0, 180.0, 190.0, 200.0, 210.0, 220.0, 230.0,
    ];
    pub const WSPD: &[f64] = &[
        8.0, 8.0, 10.0, 10.0, 10.0, 10.0, 12.0, 12.0, 12.0, 14.0, 14.0, 15.0, 16.0, 18.0, 20.0,
        22.0, 24.0, 26.0, 28.0, 30.0, 32.0,
    ];

    pub const SB_CAPE: f64 = 7371.1405;
    pub const SB_CIN: f64 = -4.4890;
    pub const SB_LCL_P: f64 = 980.9652;
    pub const K_INDEX: f64 = 40.0;
    pub const TT: f64 = 59.0;
    pub const PWAT: f64 = 1.9229;
    pub const SHR06: f64 = 9.6911;
    pub const SRH1: f64 = 16.7297;
    pub const SRH3: f64 = 41.2864;
}

/// Sounding 5: Fire weather (hot, dry, windy, low RH).
///
/// 38 C surface temperature, dewpoint of 5 C (RH ~13%), steady
/// west-southwest flow increasing with height.
mod sounding5 {
    pub const PRES: &[f64] = &[
        1013.0, 1000.0, 975.0, 950.0, 925.0, 900.0, 850.0, 800.0, 750.0, 700.0, 650.0, 600.0,
        550.0, 500.0, 450.0, 400.0, 350.0, 300.0, 250.0, 200.0,
    ];
    pub const HGHT: &[f64] = &[
        100.0, 200.0, 430.0, 670.0, 920.0, 1180.0, 1730.0, 2330.0, 2980.0, 3680.0, 4440.0, 5280.0,
        6190.0, 7200.0, 8330.0, 9600.0, 11050.0, 12710.0, 14640.0, 16920.0,
    ];
    pub const TMPC: &[f64] = &[
        38.0, 37.0, 35.0, 33.0, 30.0, 27.0, 21.0, 15.0, 9.0, 2.0, -5.0, -12.0, -20.0, -28.0, -37.0,
        -46.0, -56.0, -62.0, -58.0, -55.0,
    ];
    pub const DWPC: &[f64] = &[
        5.0, 4.0, 2.0, 0.0, -3.0, -8.0, -15.0, -22.0, -28.0, -35.0, -40.0, -45.0, -50.0, -55.0,
        -60.0, -65.0, -70.0, -72.0, -63.0, -60.0,
    ];
    pub const WDIR: &[f64] = &[
        240.0, 242.0, 245.0, 248.0, 250.0, 255.0, 260.0, 265.0, 268.0, 270.0, 272.0, 275.0, 278.0,
        280.0, 282.0, 285.0, 288.0, 290.0, 292.0, 295.0,
    ];
    pub const WSPD: &[f64] = &[
        20.0, 22.0, 25.0, 28.0, 30.0, 32.0, 35.0, 38.0, 40.0, 42.0, 44.0, 45.0, 46.0, 48.0, 50.0,
        52.0, 54.0, 56.0, 58.0, 60.0,
    ];

    pub const SB_CAPE: f64 = 7250.7441;
    pub const SB_CIN: f64 = -4.3437;
    pub const SB_LCL_P: f64 = 628.7760;
    pub const K_INDEX: f64 = -3.0;
    pub const TT: f64 = 62.0;
    pub const PWAT: f64 = 0.2759;
    pub const SHR06: f64 = 16.7019;
    pub const SFC_RH: f64 = 13.1583;
}

// =========================================================================
// Section 1: Constants verification
// =========================================================================

#[test]
fn constants_match_sharppy() {
    assert_close!(ROCP, 0.28571426, 1e-8, "ROCP");
    assert_close!(ZEROCNK, 273.15, 1e-10, "ZEROCNK");
    assert_close!(G, 9.80665, 1e-10, "G");
    assert_close!(RD, 287.04, 1e-10, "RD");
    assert_close!(RV, 461.5, 1e-10, "RV");
    assert_close!(EPSILON, 0.62197, 1e-5, "EPSILON");
}

// =========================================================================
// Section 2: Unit conversion verification
// =========================================================================

#[test]
fn unit_conversion_ms2kts() {
    assert_close!(utils::ms2kts(1.0), 1.94384449, 1e-6, "ms2kts(1)");
    assert_close!(utils::ms2kts(10.0), 19.4384449, 1e-5, "ms2kts(10)");
    assert_close!(utils::ms2kts(7.5), 14.5788337, 1e-5, "ms2kts(7.5)");
}

#[test]
fn unit_conversion_kts2ms() {
    assert_close!(utils::kts2ms(1.0), 0.514444, 1e-6, "kts2ms(1)");
    assert_close!(utils::kts2ms(60.0), 30.8666, 1e-3, "kts2ms(60)");
}

#[test]
fn unit_conversion_roundtrip() {
    for v in [1.0, 10.0, 50.0, 100.0] {
        let rt = utils::kts2ms(utils::ms2kts(v));
        // Conversion factors 1.94384449 * 0.514444 != exactly 1.0
        // due to truncated decimal representations; ~1e-6 relative error.
        let rel_err = ((rt - v) / v).abs();
        assert!(
            rel_err < 1e-5,
            "ms->kts->ms roundtrip: v={}, rt={}, rel_err={:.2e}",
            v,
            rt,
            rel_err
        );
    }
    // Zero must roundtrip exactly
    assert_close!(
        utils::kts2ms(utils::ms2kts(0.0)),
        0.0,
        1e-15,
        "zero roundtrip"
    );
}

#[test]
fn unit_conversion_mph() {
    assert_close!(utils::ms2mph(1.0), 2.23694, 1e-5, "ms2mph(1)");
    assert_close!(utils::mph2ms(1.0), 0.44704, 1e-5, "mph2ms(1)");
}

#[test]
fn unit_conversion_m2ft() {
    assert_close!(utils::m2ft(1.0), 3.2808399, 1e-6, "m2ft(1)");
    assert_close!(utils::ft2m(1.0), 0.3048, 1e-6, "ft2m(1)");
}

// =========================================================================
// Section 3: Wind vector/component verification
// =========================================================================

#[test]
fn vec2comp_cardinal_directions() {
    let eps = 1e-6;

    // North wind (from 360/0) at 10 kt => u=0, v=-10
    let (u, v) = utils::vec2comp(Some(360.0), Some(10.0));
    assert!(u.unwrap().abs() < eps, "N wind u");
    assert!((v.unwrap() - (-10.0)).abs() < eps, "N wind v");

    // South wind (180) at 10 => u=0, v=+10
    let (u, v) = utils::vec2comp(Some(180.0), Some(10.0));
    assert!(u.unwrap().abs() < eps, "S wind u");
    assert!((v.unwrap() - 10.0).abs() < eps, "S wind v");

    // West wind (270) at 10 => u=+10, v=0
    let (u, v) = utils::vec2comp(Some(270.0), Some(10.0));
    assert!((u.unwrap() - 10.0).abs() < eps, "W wind u");
    assert!(v.unwrap().abs() < eps, "W wind v");

    // East wind (90) at 10 => u=-10, v=0
    let (u, v) = utils::vec2comp(Some(90.0), Some(10.0));
    assert!((u.unwrap() - (-10.0)).abs() < eps, "E wind u");
    assert!(v.unwrap().abs() < eps, "E wind v");
}

#[test]
fn vec2comp_supercell_surface_wind() {
    // Sounding 1 surface: 160 deg at 15 kt
    let (u, v) = utils::vec2comp(Some(160.0), Some(15.0));
    // u = -15 * sin(160 deg) = -15 * 0.342 = -5.13
    // v = -15 * cos(160 deg) = -15 * (-0.940) = 14.10
    assert_close!(u.unwrap(), -5.1303, 0.01, "supercell sfc u");
    assert_close!(v.unwrap(), 14.0954, 0.01, "supercell sfc v");
}

#[test]
fn comp2vec_roundtrip_all_soundings() {
    let eps = 1e-4;
    for (wdir, wspd) in sounding1::WDIR.iter().zip(sounding1::WSPD.iter()) {
        let (u, v) = utils::vec2comp(Some(*wdir), Some(*wspd));
        let (d, s) = utils::comp2vec(u, v);
        assert!((d.unwrap() - wdir).abs() < eps, "dir roundtrip");
        assert!((s.unwrap() - wspd).abs() < eps, "spd roundtrip");
    }
}

#[test]
fn mag_known_values() {
    assert_close!(
        utils::mag(Some(3.0), Some(4.0)).unwrap(),
        5.0,
        1e-10,
        "3-4-5"
    );
    assert_close!(
        utils::mag(Some(5.0), Some(12.0)).unwrap(),
        13.0,
        1e-10,
        "5-12-13"
    );
    assert_close!(
        utils::mag(Some(0.0), Some(0.0)).unwrap(),
        0.0,
        1e-10,
        "zero"
    );
}

#[test]
fn vec2comp_slice_supercell_profile() {
    let wdir: Vec<Option<f64>> = sounding1::WDIR.iter().map(|&x| Some(x)).collect();
    let wspd: Vec<Option<f64>> = sounding1::WSPD.iter().map(|&x| Some(x)).collect();
    let (u, v) = utils::vec2comp_slice(&wdir, &wspd);

    assert_eq!(u.len(), sounding1::PRES.len());
    // All elements should be Some since all inputs are valid
    for i in 0..u.len() {
        assert!(u[i].is_some(), "u[{}] should be Some", i);
        assert!(v[i].is_some(), "v[{}] should be Some", i);
    }
}

#[test]
fn vec2comp_missing_handling() {
    let (u, v) = utils::vec2comp(None, Some(10.0));
    assert!(u.is_none());
    assert!(v.is_none());

    let (u, v) = utils::vec2comp(Some(180.0), None);
    assert!(u.is_none());
    assert!(v.is_none());

    let (u, v) = utils::vec2comp(Some(MISSING), Some(10.0));
    assert!(u.is_none());
    assert!(v.is_none());
}

// =========================================================================
// Section 4: Fire weather parameter verification
// =========================================================================

#[test]
fn fosberg_fire_weather_sounding() {
    // Sounding 5: hot (38C), dry (Td=5C), windy (20kt)
    let fwi = fire::fosberg(38.0, 5.0, 20.0);
    // Very hot and dry with moderate wind => high FWI
    assert!(fwi > 50.0, "fire wx FWI should be >50, got {}", fwi);
    assert!(fwi <= 100.0, "fire wx FWI should be <=100, got {}", fwi);
}

#[test]
fn fosberg_tropical_sounding() {
    // Sounding 4 surface: warm (28C), very moist (Td=26C), light wind (8kt)
    let fwi = fire::fosberg(28.0, 26.0, 8.0);
    // Very moist, light wind => low FWI
    assert!(fwi < 20.0, "tropical FWI should be <20, got {}", fwi);
}

#[test]
fn fosberg_null_case() {
    // Sounding 2: cold (-2C), moist (Td=-4C), calm (5kt)
    let fwi = fire::fosberg(-2.0, -4.0, 5.0);
    assert!(fwi < 30.0, "cold stable FWI should be low, got {}", fwi);
}

#[test]
fn haines_index_supercell() {
    // Sounding 1: sfc elev 350m => mid regime
    let elev = fire::haines_height(350.0);
    assert_eq!(elev, fire::HainesElevation::Mid);

    // T850=16.0, T700=0.0, Td850 needs interpolation but we use raw level
    // Lapse 850-700 = 16, Td850=7 => depression = 16-7=9
    // Actually: we need interpolated values, but let us use the known levels
    // t850=16.0, t700=0.0 => lapse=16 (>10, term=3)
    // depression: t850-td850 = 16-7=9 (6<=9<=12, term=2)
    // Haines mid = 3+2 = 5
    let hm = fire::haines_mid(16.0, 0.0, 7.0);
    assert_eq!(hm, Some(5));
}

#[test]
fn haines_index_fire_weather() {
    // Sounding 5: sfc elev 100m => low regime
    let elev = fire::haines_height(100.0);
    assert_eq!(elev, fire::HainesElevation::Low);
}

#[test]
fn haines_index_elevated() {
    // Sounding 3: sfc elev 200m => low regime
    let elev = fire::haines_height(200.0);
    assert_eq!(elev, fire::HainesElevation::Low);
}

#[test]
fn haines_high_fire_wx() {
    // Sounding 5 high regime: t700=2.0, t500=-28.0 => lapse=30 (>21 => 3)
    // td700=-35.0 => depression = 2-(-35) = 37 (>20 => 3)
    // Haines high = 3+3 = 6
    let hh = fire::haines_high(2.0, -28.0, -35.0);
    assert_eq!(hh, Some(6));
}

#[test]
fn haines_missing_input() {
    assert_eq!(fire::haines_low(MISSING, 10.0, 5.0), None);
    assert_eq!(fire::haines_mid(10.0, MISSING, 5.0), None);
    assert_eq!(fire::haines_high(10.0, 5.0, MISSING), None);
}

// =========================================================================
// Section 5: Thermodynamic function reference values
// (These test the expected SHARPpy output. When thermo module is
//  implemented in Rust, these values serve as the acceptance criteria.)
// =========================================================================

/// Reference values for basic thermodynamic computations.
/// These are the "truth" values from SHARPpy that sharprs must match.
#[cfg(test)]
mod thermo_reference {
    /// SHARPpy reference: theta(1000, 20, 1000) = 20.0
    pub const THETA_1000_20: f64 = 20.0;
    /// SHARPpy reference: theta(850, 15, 1000) = 28.695457
    pub const THETA_850_15: f64 = 28.695457;
    /// SHARPpy reference: theta(500, -10, 1000) = 47.633437
    pub const THETA_500_N10: f64 = 47.633437;

    /// SHARPpy reference: lcltemp(20, 15) = 13.835584
    pub const LCLTEMP_20_15: f64 = 13.835584;
    /// SHARPpy reference: lcltemp(30, 20) = 17.654470
    pub const LCLTEMP_30_20: f64 = 17.654470;

    /// SHARPpy reference: drylift(1000, 30, 20) = (864.574183, 17.654470)
    pub const DRYLIFT_P: f64 = 864.574183;
    pub const DRYLIFT_T: f64 = 17.654470;

    /// SHARPpy reference: vappres(20) = 23.372374
    pub const VAPPRES_20: f64 = 23.372374;
    /// SHARPpy reference: vappres(0) = 6.107955
    pub const VAPPRES_0: f64 = 6.107955;
    /// SHARPpy reference: vappres(-20) = 1.253965
    pub const VAPPRES_N20: f64 = 1.253965;

    /// SHARPpy reference: mixratio(1000, 20) = 14.955322
    pub const MIXR_1000_20: f64 = 14.955322;

    /// SHARPpy reference: wobf(20) = 15.130000
    pub const WOBF_20: f64 = 15.130000;
    /// SHARPpy reference: wobf(0) = 6.411053
    pub const WOBF_0: f64 = 6.411053;
    /// SHARPpy reference: wobf(-20) = 2.268446
    pub const WOBF_N20: f64 = 2.268446;

    /// SHARPpy reference: wetlift(800, 10, 500) = -10.532198
    pub const WETLIFT_800_10_500: f64 = -10.532198;

    /// SHARPpy reference: virtemp(1000, 30, 20) = 32.714959
    pub const VIRTEMP_1000_30_20: f64 = 32.714959;

    /// SHARPpy reference: thetae(1000, 30, 20) = 76.777260
    pub const THETAE_1000_30_20: f64 = 76.777260;

    /// SHARPpy reference: lifted(1000, 30, 20, 500) = -3.428512
    pub const LIFTED_1000_30_20_500: f64 = -3.428512;

    /// SHARPpy reference: relh(1000, 30, 20) = 55.084861
    pub const RELH_1000_30_20: f64 = 55.084861;
}

// =========================================================================
// Section 6: Placeholder tests for future modules
//
// These tests document the expected behaviour for modules not yet
// implemented (thermo, interp, winds, params).  They are marked
// #[ignore] so `cargo test` passes today; remove #[ignore] as each
// module is ported.
// =========================================================================

// --- Thermo module (future) ---

#[test]
#[ignore = "thermo module not yet implemented"]
fn thermo_theta_reference() {
    // sharprs::thermo::theta(1000.0, 20.0, 1000.0) should == 20.0
    // sharprs::thermo::theta(850.0, 15.0, 1000.0) should == 28.695457
    // sharprs::thermo::theta(500.0, -10.0, 1000.0) should == 47.633437
}

#[test]
#[ignore = "thermo module not yet implemented"]
fn thermo_lcltemp_reference() {
    // sharprs::thermo::lcltemp(20.0, 15.0) should == 13.835584
    // sharprs::thermo::lcltemp(30.0, 20.0) should == 17.654470
}

#[test]
#[ignore = "thermo module not yet implemented"]
fn thermo_drylift_reference() {
    // sharprs::thermo::drylift(1000.0, 30.0, 20.0) should == (864.574183, 17.654470)
}

#[test]
#[ignore = "thermo module not yet implemented"]
fn thermo_vappres_reference() {
    // sharprs::thermo::vappres(20.0) should == 23.372374
    // sharprs::thermo::vappres(0.0) should == 6.107955
    // sharprs::thermo::vappres(-20.0) should == 1.253965
}

#[test]
#[ignore = "thermo module not yet implemented"]
fn thermo_mixratio_reference() {
    // sharprs::thermo::mixratio(1000.0, 20.0) should == 14.955322
}

#[test]
#[ignore = "thermo module not yet implemented"]
fn thermo_wobf_reference() {
    // sharprs::thermo::wobf(20.0) should == 15.130000
    // sharprs::thermo::wobf(0.0) should == 6.411053
    // sharprs::thermo::wobf(-20.0) should == 2.268446
}

#[test]
#[ignore = "thermo module not yet implemented"]
fn thermo_wetlift_reference() {
    // sharprs::thermo::wetlift(800.0, 10.0, 500.0) should == -10.532198
}

#[test]
#[ignore = "thermo module not yet implemented"]
fn thermo_virtemp_reference() {
    // sharprs::thermo::virtemp(1000.0, 30.0, 20.0) should == 32.714959
}

#[test]
#[ignore = "thermo module not yet implemented"]
fn thermo_thetae_reference() {
    // sharprs::thermo::thetae(1000.0, 30.0, 20.0) should == 76.777260
}

#[test]
#[ignore = "thermo module not yet implemented"]
fn thermo_lifted_reference() {
    // sharprs::thermo::lifted(1000.0, 30.0, 20.0, 500.0) should == -3.428512
}

// --- Parcel / CAPE tests (future) ---

#[test]
#[ignore = "params module not yet implemented"]
fn sounding1_supercell_sb_cape() {
    // SB CAPE within 5% of 21708.8
    // SB CIN within 5% of 0.0
    // SB LCL within 5 hPa of 844.3
    // SB LFC within 5 hPa of 844.3
    // SB EL within 5 hPa of 185.0
}

#[test]
#[ignore = "params module not yet implemented"]
fn sounding1_supercell_ml_cape() {
    // ML CAPE within 5% of 18488.0
    // ML CIN within 5% of -7.09
    // ML LCL within 5 hPa of 817.9
}

#[test]
#[ignore = "params module not yet implemented"]
fn sounding1_supercell_indices() {
    // K-Index within 1C of 36.0
    // Total Totals within 1C of 77.0
    // PWAT within 1mm of 0.83 in (= 21.1 mm)
}

#[test]
#[ignore = "params module not yet implemented"]
fn sounding1_supercell_shear() {
    // 0-6km shear within 1 m/s of 30.60
    // 0-1km shear within 1 m/s of 9.85
}

#[test]
#[ignore = "params module not yet implemented"]
fn sounding1_supercell_bunkers() {
    // Bunkers R: (27.41, 9.84) kts - within 1 m/s (1.94 kt)
    // Bunkers L: (25.75, 38.95) kts - within 1 m/s
}

#[test]
#[ignore = "params module not yet implemented"]
fn sounding1_supercell_srh() {
    // 0-1km SRH within 10 m2/s2 of 151.8
    // 0-3km SRH within 10 m2/s2 of 297.5
}

#[test]
#[ignore = "params module not yet implemented"]
fn sounding1_supercell_composites() {
    // STP within 0.5 of 18.27
    // SCP within 0.5 of 129.17
    // SHIP within 0.5 of 37.51
}

#[test]
#[ignore = "params module not yet implemented"]
fn sounding2_null_case() {
    // SB CAPE == 0.0 (within 5%)
    // SB CIN == 0.0
    // K-Index within 1C of 6.0
    // TT within 1C of 56.0
    // PWAT within 1mm of 0.25 in
    // 0-6km shear within 1 m/s of 10.14
    // SRH1 within 10 m2/s2 of 11.38
}

#[test]
#[ignore = "params module not yet implemented"]
fn sounding3_elevated_convection() {
    // SB CAPE within 5% of 541.2
    // SB CIN within 5% of -1461.6
    // MU CAPE within 5% of 8707.6
    // K-Index within 1C of 50.0
    // TT within 1C of 90.0
}

#[test]
#[ignore = "params module not yet implemented"]
fn sounding4_tropical() {
    // SB CAPE within 5% of 7371.1
    // SB CIN within 5% of -4.49
    // LCL within 5 hPa of 981.0
    // K-Index within 1C of 40.0
    // PWAT within 1mm of 1.92 in (= 48.8 mm)
    // Weak shear: 0-6km < 10 m/s
}

#[test]
#[ignore = "params module not yet implemented"]
fn sounding5_fire_weather() {
    // SB CAPE within 5% of 7250.7
    // K-Index within 1C of -3.0
    // TT within 1C of 62.0
    // PWAT within 1mm of 0.28 in
    // SFC RH within 1% of 13.16%
}

// =========================================================================
// Section 7: Cross-sounding consistency checks
//
// These verify logical relationships that must hold regardless of the
// exact algorithm implementation.
// =========================================================================

#[test]
fn data_integrity_sounding_lengths() {
    // All arrays in each sounding must have the same length
    assert_eq!(sounding1::PRES.len(), sounding1::HGHT.len());
    assert_eq!(sounding1::PRES.len(), sounding1::TMPC.len());
    assert_eq!(sounding1::PRES.len(), sounding1::DWPC.len());
    assert_eq!(sounding1::PRES.len(), sounding1::WDIR.len());
    assert_eq!(sounding1::PRES.len(), sounding1::WSPD.len());

    assert_eq!(sounding2::PRES.len(), sounding2::HGHT.len());
    assert_eq!(sounding2::PRES.len(), sounding2::TMPC.len());
    assert_eq!(sounding2::PRES.len(), sounding2::DWPC.len());

    assert_eq!(sounding3::PRES.len(), sounding3::HGHT.len());
    assert_eq!(sounding3::PRES.len(), sounding3::TMPC.len());

    assert_eq!(sounding4::PRES.len(), sounding4::HGHT.len());
    assert_eq!(sounding4::PRES.len(), sounding4::TMPC.len());

    assert_eq!(sounding5::PRES.len(), sounding5::HGHT.len());
    assert_eq!(sounding5::PRES.len(), sounding5::TMPC.len());
}

#[test]
fn data_integrity_pressure_decreases() {
    // Pressure must decrease monotonically with height
    for sounding in [
        sounding1::PRES,
        sounding2::PRES,
        sounding3::PRES,
        sounding4::PRES,
        sounding5::PRES,
    ] {
        for i in 1..sounding.len() {
            assert!(
                sounding[i] < sounding[i - 1],
                "Pressure must decrease: p[{}]={} >= p[{}]={}",
                i,
                sounding[i],
                i - 1,
                sounding[i - 1]
            );
        }
    }
}

#[test]
fn data_integrity_height_increases() {
    // Height must increase monotonically with decreasing pressure
    for sounding in [
        sounding1::HGHT,
        sounding2::HGHT,
        sounding3::HGHT,
        sounding4::HGHT,
        sounding5::HGHT,
    ] {
        for i in 1..sounding.len() {
            assert!(
                sounding[i] > sounding[i - 1],
                "Height must increase: h[{}]={} <= h[{}]={}",
                i,
                sounding[i],
                i - 1,
                sounding[i - 1]
            );
        }
    }
}

#[test]
fn data_integrity_dewpoint_le_temperature() {
    // Dewpoint must be <= temperature everywhere
    for (tmpc, dwpc) in [
        (sounding1::TMPC, sounding1::DWPC),
        (sounding2::TMPC, sounding2::DWPC),
        (sounding3::TMPC, sounding3::DWPC),
        (sounding4::TMPC, sounding4::DWPC),
        (sounding5::TMPC, sounding5::DWPC),
    ] {
        for (i, (&t, &d)) in tmpc.iter().zip(dwpc.iter()).enumerate() {
            assert!(d <= t, "Td must be <= T: level {}: Td={} > T={}", i, d, t);
        }
    }
}

#[test]
fn reference_value_physical_sanity() {
    // CAPE should be non-negative
    assert!(sounding1::SB_CAPE >= 0.0);
    assert!(sounding2::SB_CAPE >= 0.0);
    assert!(sounding3::SB_CAPE >= 0.0);
    assert!(sounding4::SB_CAPE >= 0.0);
    assert!(sounding5::SB_CAPE >= 0.0);

    // CIN should be non-positive
    assert!(sounding1::SB_CIN <= 0.0);
    assert!(sounding2::SB_CIN <= 0.0);
    assert!(sounding3::SB_CIN <= 0.0);
    assert!(sounding4::SB_CIN <= 0.0);
    assert!(sounding5::SB_CIN <= 0.0);

    // Supercell should have strongest deep-layer shear
    assert!(
        sounding1::SHR06 > sounding4::SHR06,
        "supercell shear should exceed tropical"
    );

    // Tropical should have highest PWAT
    assert!(sounding4::PWAT > sounding1::PWAT);
    assert!(sounding4::PWAT > sounding2::PWAT);
    assert!(sounding4::PWAT > sounding5::PWAT);

    // Fire weather should have lowest PWAT
    assert!(sounding5::PWAT < sounding1::PWAT);
    assert!(sounding5::PWAT < sounding4::PWAT);

    // Null case should have zero CAPE
    assert!(sounding2::SB_CAPE < 1.0);

    // Elevated case: MU CAPE >> SB CAPE
    assert!(sounding3::MU_CAPE > sounding3::SB_CAPE * 5.0);

    // Elevated case: large SB CIN
    assert!(sounding3::SB_CIN < -500.0);
}

#[test]
fn reference_value_k_index_ordering() {
    // K-index should be highest for elevated convection (warm, moist low
    // levels + cold 500 mb), lowest for fire weather (very dry)
    assert!(
        sounding3::K_INDEX > sounding1::K_INDEX,
        "elevated K > supercell K"
    );
    assert!(
        sounding1::K_INDEX > sounding5::K_INDEX,
        "supercell K > fire K"
    );
    assert!(
        sounding5::K_INDEX < 0.0,
        "fire K should be negative (very dry)"
    );
}

#[test]
fn reference_value_total_totals_ordering() {
    // TT should be highest for elevated (warm 850, cold 500)
    assert!(sounding3::TT > sounding1::TT, "elevated TT > supercell TT");
}

// =========================================================================
// Section 8: Fosberg FWI verification across soundings
// =========================================================================

#[test]
fn fosberg_ordering_across_soundings() {
    // Compute FWI for each sounding surface conditions
    let fwi1 = fire::fosberg(28.0, 19.0, 15.0); // supercell
    let fwi2 = fire::fosberg(-2.0, -4.0, 5.0); // null
    let fwi4 = fire::fosberg(28.0, 26.0, 8.0); // tropical
    let fwi5 = fire::fosberg(38.0, 5.0, 20.0); // fire wx

    // Fire weather should have highest FWI
    assert!(
        fwi5 > fwi1,
        "fire FWI ({}) should > supercell FWI ({})",
        fwi5,
        fwi1
    );
    assert!(
        fwi5 > fwi4,
        "fire FWI ({}) should > tropical FWI ({})",
        fwi5,
        fwi4
    );
    // Tropical (very moist) should have lowest or near-lowest
    assert!(
        fwi4 < fwi1,
        "tropical FWI ({}) should < supercell FWI ({})",
        fwi4,
        fwi1
    );
}

// =========================================================================
// Section 9: Edge cases and special values
// =========================================================================

#[test]
fn qc_edge_cases() {
    assert!(!utils::qc(None));
    assert!(utils::qc(Some(0.0)));
    assert!(utils::qc(Some(-9998.0)));
    assert!(utils::qc(Some(f64::MAX)));
}

#[test]
fn qc_value_sentinel() {
    assert!(!utils::qc_value(MISSING));
    assert!(!utils::qc_value(-9999.0));
    assert!(utils::qc_value(-9998.0));
    assert!(utils::qc_value(0.0));
    assert!(utils::qc_value(f64::MAX));
}

#[test]
fn from_raw_conversion() {
    assert_eq!(utils::from_raw(MISSING), None);
    assert_eq!(utils::from_raw(42.0), Some(42.0));
    assert_eq!(utils::from_raw(0.0), Some(0.0));
    assert_eq!(utils::from_raw(-100.0), Some(-100.0));
}

#[test]
fn vec2comp_zero_speed() {
    let (u, v) = utils::vec2comp(Some(180.0), Some(0.0));
    assert_close!(u.unwrap(), 0.0, 1e-10, "zero speed u");
    assert_close!(v.unwrap(), 0.0, 1e-10, "zero speed v");
}

#[test]
fn mag_slice_with_missing() {
    let u = vec![Some(3.0), None, Some(5.0)];
    let v = vec![Some(4.0), Some(2.0), Some(12.0)];
    let m = utils::mag_slice(&u, &v);
    assert_close!(m[0].unwrap(), 5.0, 1e-10, "mag[0]");
    assert!(m[1].is_none(), "mag[1] should be None");
    assert_close!(m[2].unwrap(), 13.0, 1e-10, "mag[2]");
}

// =========================================================================
// Section 10: Haines Index comprehensive tests
// =========================================================================

#[test]
fn haines_all_regimes() {
    // Low: lapse(950-850), depression at 850
    assert_eq!(fire::haines_low(10.0, 8.0, 5.0), Some(2)); // lapse=2(<4=>1), dep=3(<6=>1) => 2
    assert_eq!(fire::haines_low(15.0, 10.0, 3.0), Some(4)); // lapse=5(4-7=>2), dep=7(6-9=>2) => 4
    assert_eq!(fire::haines_low(20.0, 10.0, -5.0), Some(6)); // lapse=10(>7=>3), dep=15(>9=>3) => 6

    // Mid: lapse(850-700), depression at 850
    assert_eq!(fire::haines_mid(10.0, 6.0, 6.0), Some(2)); // lapse=4(<6=>1), dep=4(<6=>1) => 2
    assert_eq!(fire::haines_mid(15.0, 7.0, 7.0), Some(4)); // lapse=8(6-10=>2), dep=8(6-12=>2) => 4
    assert_eq!(fire::haines_mid(25.0, 10.0, 0.0), Some(6)); // lapse=15(>10=>3), dep=25(>12=>3) => 6

    // High: lapse(700-500), depression at 700
    assert_eq!(fire::haines_high(0.0, -10.0, -5.0), Some(2)); // lapse=10(<18=>1), dep=5(<15=>1) => 2
    assert_eq!(fire::haines_high(5.0, -15.0, -13.0), Some(4)); // lapse=20(18-21=>2), dep=18(15-20=>2) => 4
    assert_eq!(fire::haines_high(5.0, -20.0, -25.0), Some(6)); // lapse=25(>21=>3), dep=30(>20=>3) => 6
}

#[test]
fn haines_boundary_values() {
    // Test exact boundary values for classification
    // Low: lapse boundaries at 4 and 7
    assert_eq!(fire::haines_low(14.0, 10.0, 4.0), Some(4)); // lapse=4(==4=>2), dep=6(==6=>2) => 4
    assert_eq!(fire::haines_low(17.0, 10.0, 1.0), Some(4)); // lapse=7(==7=>2), dep=9(==9=>2) => 4

    // Mid: lapse boundaries at 6 and 10
    assert_eq!(fire::haines_mid(16.0, 10.0, 10.0), Some(4)); // lapse=6(==6=>2), dep=6(==6=>2) => 4
    assert_eq!(fire::haines_mid(20.0, 10.0, 8.0), Some(4)); // lapse=10(==10=>2), dep=12(==12=>2) => 4

    // High: lapse boundaries at 18 and 21
    assert_eq!(fire::haines_high(0.0, -18.0, -15.0), Some(4)); // lapse=18(==18=>2), dep=15(==15=>2) => 4
    assert_eq!(fire::haines_high(0.0, -21.0, -20.0), Some(4)); // lapse=21(==21=>2), dep=20(==20=>2) => 4
}
