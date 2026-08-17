//! Fire weather parameters.
//!
//! Rust port of `sharppy/sharptab/fire.py`.
//! Original Python by Greg Blumberg (CIMMS) and Kelton Halbert (OU SoM),
//! with contributions from Nickolai Reimer (NWS Billings, MT).

use crate::constants::{HAINES_HIGH, HAINES_LOW, HAINES_MID, MISSING};

// ---------------------------------------------------------------------------
// Helpers (inlined conversions so this module is self-contained)
// ---------------------------------------------------------------------------

/// Celsius to Fahrenheit.
#[inline]
fn ctof(tc: f64) -> f64 {
    (tc * 9.0 / 5.0) + 32.0
}

/// Knots to miles per hour.
#[inline]
fn kts2mph(kts: f64) -> f64 {
    kts * 1.15078
}

/// Compute relative humidity (%) from pressure (mb), temperature (°C), and
/// dewpoint (°C) using the Bolton (1980) saturation vapour-pressure formula.
#[inline]
fn relh(tmpc: f64, dwpc: f64) -> f64 {
    let es = 6.112 * ((17.67 * tmpc) / (tmpc + 243.5)).exp();
    let e = 6.112 * ((17.67 * dwpc) / (dwpc + 243.5)).exp();
    (e / es * 100.0).clamp(0.0, 100.0)
}

// ---------------------------------------------------------------------------
// Fosberg Fire Weather Index
// ---------------------------------------------------------------------------

/// Compute the Fosberg Fire Weather Index (FWI).
///
/// The FWI provides a non-linear filter of meteorological data yielding a
/// linear relationship between the combined variables of relative humidity
/// and wind speed and the behaviour of wildfires.  Values range 0–100.
///
/// Adapted from code donated by Rich Thompson (NOAA SPC).
///
/// # Arguments
///
/// * `tmpc` – surface temperature (°C)
/// * `dwpc` – surface dewpoint (°C)
/// * `wspd_kts` – surface wind speed (knots)
///
/// # Returns
///
/// Fosberg index value (clamped to 0–100).
pub fn fosberg(tmpc: f64, dwpc: f64, wspd_kts: f64) -> f64 {
    let tmpf = ctof(tmpc);
    let fmph = kts2mph(wspd_kts);
    let rh = relh(tmpc, dwpc);

    // Equilibrium moisture content (em)
    let em = if rh <= 10.0 {
        0.03229 + 0.281073 * rh - 0.000578 * rh * tmpf
    } else if rh <= 50.0 {
        2.22749 + 0.160107 * rh - 0.014784 * tmpf
    } else {
        21.0606 + 0.005565 * rh * rh - 0.00035 * rh * tmpf - 0.483199 * rh
    };

    let em30 = em / 30.0;
    let u_sq = fmph * fmph;
    let fmdc = 1.0 - 2.0 * em30 + 1.5 * em30 * em30 - 0.5 * em30 * em30 * em30;

    let param = (fmdc * (1.0 + u_sq).sqrt()) / 0.3002;

    // SHARPpy clips at 100
    param.min(100.0)
}

// ---------------------------------------------------------------------------
// Haines Index
// ---------------------------------------------------------------------------

/// Haines Index elevation category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HainesElevation {
    /// Below 1 000 ft (305 m).
    Low,
    /// 1 000 – 3 000 ft (305 – 914 m).
    Mid,
    /// Above 3 000 ft (914 m).
    High,
}

impl HainesElevation {
    /// Convert to the legacy numeric constant used in SHARPpy.
    pub fn as_code(self) -> u8 {
        match self {
            Self::Low => HAINES_LOW,
            Self::Mid => HAINES_MID,
            Self::High => HAINES_HIGH,
        }
    }
}

/// Determine the Haines Index elevation regime from surface elevation (m).
///
/// * < 305 m → Low
/// * 305 – 914 m → Mid
/// * > 914 m → High
pub fn haines_height(sfc_elevation_m: f64) -> HainesElevation {
    if sfc_elevation_m < 305.0 {
        HainesElevation::Low
    } else if sfc_elevation_m <= 914.0 {
        HainesElevation::Mid
    } else {
        HainesElevation::High
    }
}

/// Helper: classify a value into the 1/2/3 lapse-rate or dewpoint-depression
/// term given the lower and upper breakpoints.
#[inline]
fn classify_term(value: f64, low: f64, high: f64) -> u8 {
    if value < low {
        1
    } else if value <= high {
        2
    } else {
        3
    }
}

/// Compute the **Low-elevation** Haines Index.
///
/// * Lapse rate: T(950 mb) − T(850 mb)
/// * Dewpoint depression: T(850 mb) − Td(850 mb)
///
/// # Arguments
///
/// * `t950` – temperature at 950 mb (°C)
/// * `t850` – temperature at 850 mb (°C)
/// * `td850` – dewpoint at 850 mb (°C)
///
/// All inputs must be valid (not `MISSING`). Returns `None` if any input
/// equals `MISSING`.
pub fn haines_low(t950: f64, t850: f64, td850: f64) -> Option<u8> {
    if t950 == MISSING || t850 == MISSING || td850 == MISSING {
        return None;
    }

    let lapse_rate = t950 - t850;
    let dewpoint_depression = t850 - td850;

    let a = classify_term(lapse_rate, 4.0, 7.0);
    let b = classify_term(dewpoint_depression, 6.0, 9.0);
    Some(a + b)
}

/// Compute the **Mid-elevation** Haines Index.
///
/// * Lapse rate: T(850 mb) − T(700 mb)
/// * Dewpoint depression: T(850 mb) − Td(850 mb)
///
/// Returns `None` when any input equals `MISSING`.
pub fn haines_mid(t850: f64, t700: f64, td850: f64) -> Option<u8> {
    if t850 == MISSING || t700 == MISSING || td850 == MISSING {
        return None;
    }

    let lapse_rate = t850 - t700;
    let dewpoint_depression = t850 - td850;

    let a = classify_term(lapse_rate, 6.0, 10.0);
    let b = classify_term(dewpoint_depression, 6.0, 12.0);
    Some(a + b)
}

/// Compute the **High-elevation** Haines Index.
///
/// * Lapse rate: T(700 mb) − T(500 mb)
/// * Dewpoint depression: T(700 mb) − Td(700 mb)
///
/// Returns `None` when any input equals `MISSING`.
pub fn haines_high(t700: f64, t500: f64, td700: f64) -> Option<u8> {
    if t700 == MISSING || t500 == MISSING || td700 == MISSING {
        return None;
    }

    let lapse_rate = t700 - t500;
    let dewpoint_depression = t700 - td700;

    let a = classify_term(lapse_rate, 18.0, 21.0);
    let b = classify_term(dewpoint_depression, 15.0, 20.0);
    Some(a + b)
}

/// Compute the Haines Index for the appropriate elevation regime, given
/// surface elevation and the required level temperatures/dewpoints.
///
/// This is a convenience wrapper that calls [`haines_height`] and then
/// dispatches to the correct variant.
///
/// # Arguments
///
/// * `sfc_elev_m` – surface elevation (m)
/// * `t950` – temperature at 950 mb (°C)
/// * `t850` – temperature at 850 mb (°C)
/// * `t700` – temperature at 700 mb (°C)
/// * `t500` – temperature at 500 mb (°C)
/// * `td850` – dewpoint at 850 mb (°C)
/// * `td700` – dewpoint at 700 mb (°C)
///
/// Returns `(elevation_category, Option<index_value>)`.
pub fn haines(
    sfc_elev_m: f64,
    t950: f64,
    t850: f64,
    t700: f64,
    t500: f64,
    td850: f64,
    td700: f64,
) -> (HainesElevation, Option<u8>) {
    let elev = haines_height(sfc_elev_m);
    let idx = match elev {
        HainesElevation::Low => haines_low(t950, t850, td850),
        HainesElevation::Mid => haines_mid(t850, t700, td850),
        HainesElevation::High => haines_high(t700, t500, td700),
    };
    (elev, idx)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- Fosberg ---

    #[test]
    fn fosberg_calm_moist() {
        // Very moist, calm wind → low index
        let val = fosberg(20.0, 19.0, 0.0);
        assert!(val < 10.0, "expected low FWI, got {val}");
    }

    #[test]
    fn fosberg_hot_dry_windy() {
        // Hot, dry, strong wind → high index
        let val = fosberg(38.0, 5.0, 30.0);
        assert!(val > 50.0, "expected high FWI, got {val}");
    }

    #[test]
    fn fosberg_max_100() {
        // Extremely dry with gale-force wind should cap at 100
        let val = fosberg(45.0, -10.0, 60.0);
        assert!(val <= 100.0, "FWI should cap at 100, got {val}");
    }

    #[test]
    fn fosberg_low_rh_branch() {
        // RH ≤ 10 branch
        // tmpc=35, dwpc=-5 gives RH ~ 5%
        let val = fosberg(35.0, -5.0, 10.0);
        assert!(val > 20.0);
    }

    #[test]
    fn fosberg_mid_rh_branch() {
        // 10 < RH ≤ 50 branch
        // tmpc=25, dwpc=10 gives RH ~ 39%
        let val = fosberg(25.0, 10.0, 15.0);
        assert!(val > 0.0);
    }

    #[test]
    fn fosberg_high_rh_branch() {
        // RH > 50 branch
        // tmpc=20, dwpc=18 gives RH ~ 88%
        let val = fosberg(20.0, 18.0, 5.0);
        assert!(val < 30.0);
    }

    // --- Haines Height ---

    #[test]
    fn haines_height_low() {
        assert_eq!(haines_height(100.0), HainesElevation::Low);
        assert_eq!(haines_height(0.0), HainesElevation::Low);
    }

    #[test]
    fn haines_height_mid() {
        assert_eq!(haines_height(305.0), HainesElevation::Mid);
        assert_eq!(haines_height(600.0), HainesElevation::Mid);
        assert_eq!(haines_height(914.0), HainesElevation::Mid);
    }

    #[test]
    fn haines_height_high_elev() {
        assert_eq!(haines_height(1000.0), HainesElevation::High);
        assert_eq!(haines_height(2000.0), HainesElevation::High);
    }

    // --- Haines Low ---

    #[test]
    fn haines_low_minimal() {
        // lapse < 4, depression < 6 → 1+1=2
        assert_eq!(haines_low(10.0, 8.0, 5.0), Some(2));
    }

    #[test]
    fn haines_low_maximal() {
        // lapse > 7, depression > 9 → 3+3=6
        assert_eq!(haines_low(20.0, 10.0, -5.0), Some(6));
    }

    #[test]
    fn haines_low_mid_terms() {
        // lapse=5 (4..=7 → 2), depression=7 (6..=9 → 2) → 4
        assert_eq!(haines_low(15.0, 10.0, 3.0), Some(4));
    }

    #[test]
    fn haines_low_missing() {
        assert_eq!(haines_low(MISSING, 10.0, 5.0), None);
    }

    // --- Haines Mid ---

    #[test]
    fn haines_mid_minimal() {
        // lapse < 6, depression < 6 → 2
        assert_eq!(haines_mid(10.0, 6.0, 6.0), Some(2));
    }

    #[test]
    fn haines_mid_maximal() {
        // lapse > 10, depression > 12 → 6
        assert_eq!(haines_mid(20.0, 5.0, 0.0), Some(6));
    }

    // --- Haines High ---

    #[test]
    fn haines_high_minimal() {
        // lapse < 18, depression < 15 → 2
        assert_eq!(haines_high(0.0, -10.0, -5.0), Some(2));
    }

    #[test]
    fn haines_high_maximal() {
        // lapse=30 (>21→3), depression=25 (>20→3) → 6
        assert_eq!(haines_high(0.0, -30.0, -25.0), Some(6));
    }

    // --- Unified haines() ---

    #[test]
    fn haines_dispatches_correctly() {
        let (elev, val) = haines(100.0, 10.0, 8.0, 0.0, -20.0, 5.0, -5.0);
        assert_eq!(elev, HainesElevation::Low);
        assert!(val.is_some());

        let (elev, val) = haines(600.0, 10.0, 8.0, 0.0, -20.0, 5.0, -5.0);
        assert_eq!(elev, HainesElevation::Mid);
        assert!(val.is_some());

        let (elev, val) = haines(2000.0, 10.0, 8.0, 0.0, -20.0, 5.0, -5.0);
        assert_eq!(elev, HainesElevation::High);
        assert!(val.is_some());
    }
}
