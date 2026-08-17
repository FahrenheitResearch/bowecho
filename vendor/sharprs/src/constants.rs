//! Physical and meteorological constants used throughout SHARPpy/sharprs.
//!
//! All values are stored as `f64` and match the original SHARPpy Python constants.
//! Additional standard atmospheric constants (Rd, Rv, Cp, Lv, epsilon) are included
//! for use by thermodynamic routines even though the original `constants.py` only
//! exposed a subset.

// ---------------------------------------------------------------------------
// Missing / sentinel value
// ---------------------------------------------------------------------------

/// Missing data flag.  Any field set to this value is treated as absent.
pub const MISSING: f64 = -9999.0;

// ---------------------------------------------------------------------------
// Core meteorological / physical constants
// ---------------------------------------------------------------------------

/// Ratio of the gas constant to the specific heat at constant pressure
/// for dry air: R_d / C_pd = 2/7 ≈ 0.28571426 (dimensionless).
pub const ROCP: f64 = 0.28571426;

/// Zero degrees Celsius expressed in Kelvin (K).
pub const ZEROCNK: f64 = 273.15;

/// Standard acceleration due to gravity (m s⁻²).
pub const G: f64 = 9.80665;

/// Floating-point tolerance used for near-zero comparisons.
pub const TOL: f64 = 1e-10;

/// Specific gas constant for dry air (J kg⁻¹ K⁻¹).
pub const RD: f64 = 287.04;

/// Specific gas constant for water vapour (J kg⁻¹ K⁻¹).
pub const RV: f64 = 461.5;

/// Specific heat of dry air at constant pressure (J kg⁻¹ K⁻¹).
pub const CP: f64 = 1005.7;

/// Latent heat of vaporisation at 0 °C (J kg⁻¹).
pub const LV: f64 = 2.501e6;

/// Latent heat of fusion at 0 °C (J kg⁻¹).
pub const LF: f64 = 3.34e5;

/// Latent heat of sublimation at 0 °C (J kg⁻¹).
pub const LS: f64 = 2.834e6;

/// Ratio of molecular weight of water to dry air: Mw/Md ≈ 0.62197
/// (same as `eps` in SHARPpy's thermo.py).
pub const EPSILON: f64 = 0.62197;

// ---------------------------------------------------------------------------
// Haines Index elevation regime flags
// ---------------------------------------------------------------------------

/// Haines Index — low elevation regime.
pub const HAINES_LOW: u8 = 0;

/// Haines Index — mid elevation regime.
pub const HAINES_MID: u8 = 1;

/// Haines Index — high elevation regime.
pub const HAINES_HIGH: u8 = 2;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_value() {
        assert_eq!(MISSING, -9999.0);
    }

    #[test]
    fn rocp_approx() {
        // R/Cp for dry air ≈ 2/7
        assert!((ROCP - 2.0 / 7.0).abs() < 1e-4);
    }

    #[test]
    fn zero_cnk() {
        assert_eq!(ZEROCNK, 273.15);
    }

    #[test]
    fn gravity() {
        assert!((G - 9.80665).abs() < 1e-10);
    }

    #[test]
    fn gas_constant_ratio() {
        // epsilon ≈ Rd / Rv
        assert!((EPSILON - RD / RV).abs() < 0.003);
    }

    #[test]
    fn rocp_from_components() {
        // Rd / Cp should be close to ROCP
        assert!((RD / CP - ROCP).abs() < 0.002);
    }

    #[test]
    fn haines_ordering() {
        assert!(HAINES_LOW < HAINES_MID);
        assert!(HAINES_MID < HAINES_HIGH);
    }
}
