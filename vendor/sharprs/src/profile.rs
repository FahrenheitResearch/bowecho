//! Central sounding data structure for sharprs.
//!
//! The [`Profile`] holds a vertical atmospheric sounding — pressure, height,
//! temperature, dewpoint, wind, and derived thermodynamic fields — mirroring
//! the SHARPpy `BasicProfile` class.  It is the primary input to every
//! analysis routine in the crate.
//!
//! # Design notes
//!
//! * Every vertical array is `Vec<f64>`.  Missing values are represented as
//!   `f64::NAN`; helpers ([`is_valid`], [`first_valid_index`], …) make it easy
//!   to skip them.  This is the Rust equivalent of NumPy masked arrays.
//! * Construction always sorts levels in **descending** pressure order
//!   (surface first), matching SHARPpy convention.
//! * Derived fields (theta, thetae, mixing ratio, …) are computed eagerly in
//!   [`Profile::new`] so that downstream code can borrow them cheaply.

use crate::constants::*;
use std::fmt;

// ---------------------------------------------------------------------------
// Small helpers (pub so other modules can reuse them)
// ---------------------------------------------------------------------------

/// Returns `true` if the value is finite and not equal to [`MISSING`].
#[inline]
pub fn is_valid(v: f64) -> bool {
    v.is_finite() && (v - MISSING).abs() > TOL
}

/// Replace a value with `NAN` if it is missing (equals [`MISSING`] or is
/// non-finite).
#[inline]
pub fn mask_missing(v: f64) -> f64 {
    if is_valid(v) {
        v
    } else {
        f64::NAN
    }
}

/// Find the index of the first non-NAN element, or `None`.
pub fn first_valid_index(arr: &[f64]) -> Option<usize> {
    arr.iter().position(|v| v.is_finite())
}

/// Find the index of the last non-NAN element, or `None`.
pub fn last_valid_index(arr: &[f64]) -> Option<usize> {
    arr.iter().rposition(|v| v.is_finite())
}

// ---------------------------------------------------------------------------
// Wind helpers
// ---------------------------------------------------------------------------

/// Convert meteorological wind direction (degrees) and speed (knots) to
/// u- and v-components (knots).  Returns `(u, v)`.
///
/// Meteorological convention: wind direction is where the wind blows *from*,
/// measured clockwise from north.
pub fn vec2comp(wdir: f64, wspd: f64) -> (f64, f64) {
    if !wdir.is_finite() || !wspd.is_finite() {
        return (f64::NAN, f64::NAN);
    }
    let rad = wdir.to_radians();
    let u = -wspd * rad.sin();
    let v = -wspd * rad.cos();
    (u, v)
}

/// Convert u/v wind components (knots) to meteorological direction (degrees)
/// and speed (knots).  Returns `(wdir, wspd)`.
pub fn comp2vec(u: f64, v: f64) -> (f64, f64) {
    if !u.is_finite() || !v.is_finite() {
        return (f64::NAN, f64::NAN);
    }
    let wspd = (u * u + v * v).sqrt();
    if wspd < TOL {
        return (0.0, 0.0);
    }
    let mut wdir = (u.atan2(v)).to_degrees() + 180.0;
    if wdir >= 360.0 {
        wdir -= 360.0;
    }
    if wdir < 0.0 {
        wdir += 360.0;
    }
    (wdir, wspd)
}

/// Vector magnitude.
#[inline]
pub fn mag(u: f64, v: f64) -> f64 {
    (u * u + v * v).sqrt()
}

// ---------------------------------------------------------------------------
// Basic thermodynamic helpers (self-contained so Profile can compute derived
// fields without pulling in a full `thermo` module yet).
// ---------------------------------------------------------------------------

/// Celsius to Kelvin.
#[inline]
pub fn ctok(tc: f64) -> f64 {
    tc + ZEROCNK
}

/// Kelvin to Celsius.
#[inline]
pub fn ktoc(tk: f64) -> f64 {
    tk - ZEROCNK
}

/// Celsius to Fahrenheit.
#[inline]
pub fn ctof(tc: f64) -> f64 {
    tc * 9.0 / 5.0 + 32.0
}

/// Fahrenheit to Celsius.
#[inline]
pub fn ftoc(tf: f64) -> f64 {
    (tf - 32.0) * 5.0 / 9.0
}

/// Potential temperature (C) given pressure (hPa) and temperature (C).
///
/// θ = T * (1000 / p)^(R/Cp)   — returned in **Celsius**.
pub fn theta(pres: f64, tmpc: f64) -> f64 {
    if !pres.is_finite() || !tmpc.is_finite() || pres <= 0.0 {
        return f64::NAN;
    }
    let tk = ctok(tmpc);
    ktoc(tk * (1000.0 / pres).powf(ROCP))
}

/// Saturation vapor pressure (hPa) via Bolton (1980).
pub fn sat_vapor_pressure(tmpc: f64) -> f64 {
    if !tmpc.is_finite() {
        return f64::NAN;
    }
    6.112 * ((17.67 * tmpc) / (tmpc + 243.5)).exp()
}

/// Water-vapor mixing ratio (g/kg) given pressure (hPa) and dewpoint (C).
pub fn mixratio(pres: f64, dwpc: f64) -> f64 {
    if !pres.is_finite() || !dwpc.is_finite() || pres <= 0.0 {
        return f64::NAN;
    }
    let e = sat_vapor_pressure(dwpc);
    let w = EPSILON * e / (pres - e);
    w * 1000.0 // g/kg
}

/// Virtual temperature (C) given pressure (hPa), temperature (C), and
/// dewpoint (C).
pub fn virtemp(pres: f64, tmpc: f64, dwpc: f64) -> f64 {
    if !pres.is_finite() || !tmpc.is_finite() || !dwpc.is_finite() {
        // If dewpoint is missing, fall back to dry temperature.
        if pres.is_finite() && tmpc.is_finite() {
            return tmpc;
        }
        return f64::NAN;
    }
    let tk = ctok(tmpc);
    let w = mixratio(pres, dwpc) / 1000.0; // kg/kg
    let tv = tk * (1.0 + w / EPSILON) / (1.0 + w);
    ktoc(tv)
}

/// Relative humidity (%) given pressure (hPa), temperature (C), and dewpoint (C).
pub fn relh(pres: f64, tmpc: f64, dwpc: f64) -> f64 {
    if !pres.is_finite() || !tmpc.is_finite() || !dwpc.is_finite() {
        return f64::NAN;
    }
    let _ = pres; // pres unused for simple RH but kept for API symmetry
    let e = sat_vapor_pressure(dwpc);
    let es = sat_vapor_pressure(tmpc);
    if es.abs() < TOL {
        return f64::NAN;
    }
    (e / es * 100.0).clamp(0.0, 100.0)
}

/// Equivalent potential temperature (K) via Bolton (1980) approximation.
///
/// Returns θ_e in **Kelvin**.
pub fn thetae(pres: f64, tmpc: f64, dwpc: f64) -> f64 {
    if !pres.is_finite() || !tmpc.is_finite() || !dwpc.is_finite() || pres <= 0.0 {
        return f64::NAN;
    }
    let tk = ctok(tmpc);
    let w = mixratio(pres, dwpc) / 1000.0; // kg/kg
                                           // LCL temperature (Bolton 1980 Eq. 15)
    let e = sat_vapor_pressure(dwpc);
    if e <= 0.0 {
        return f64::NAN;
    }

    // Bolton (1980) Eq. 43:
    // θ_e = θ_d * exp[(3.376/T_L - 0.00254) * r * (1 + 0.81e-3 * r)]
    // where T_L = LCL temperature, r = mixing ratio in g/kg, θ_d = dry potential temp

    // LCL temperature via Bolton Eq. 15:
    let tdk = ctok(dwpc);
    let t_lcl = 56.0 + 1.0 / (1.0 / (tdk - 56.0) + (tk / tdk).ln() / 800.0);
    let r_gkg = w * 1000.0; // back to g/kg for formula

    // Dry potential temperature (K)
    let theta_d = tk * (1000.0 / pres).powf(0.2854 * (1.0 - 0.00028 * r_gkg));

    // Bolton Eq. 43
    theta_d * ((3.376 / t_lcl - 0.00254) * r_gkg * (1.0 + 0.00081 * r_gkg)).exp()
}

/// Wetbulb temperature (C) by iterative technique.
///
/// Finds the temperature along the saturation mixing-ratio line from
/// (pres, dwpc) up to where it meets the dry adiabat from (pres, tmpc).
/// Uses a simple bisection for robustness.
pub fn wetbulb(pres: f64, tmpc: f64, dwpc: f64) -> f64 {
    if !pres.is_finite() || !tmpc.is_finite() || !dwpc.is_finite() || pres <= 0.0 {
        return f64::NAN;
    }
    // Bisection: wetbulb is between dwpc and tmpc (or a bit beyond).
    let mut lo = dwpc.min(tmpc) - 1.0;
    let mut hi = tmpc.max(dwpc) + 1.0;
    // The wetbulb satisfies: mixing_ratio(pres, Tw) ≈ (saturated w at Tw)
    // and lies on the moist adiabat from (pres, tmpc).
    // Simpler approach: find Tw such that
    //   theta_w(pres, Tw, Tw) ≈ thetae(pres, tmpc, dwpc)
    // i.e. saturated theta-e at Tw equals the actual theta-e.
    let target = thetae(pres, tmpc, dwpc);
    if !target.is_finite() {
        return f64::NAN;
    }
    for _ in 0..50 {
        let mid = (lo + hi) / 2.0;
        let te = thetae(pres, mid, mid); // saturated at mid
        if !te.is_finite() {
            return f64::NAN;
        }
        if te < target {
            lo = mid;
        } else {
            hi = mid;
        }
        if (hi - lo) < 0.005 {
            break;
        }
    }
    (lo + hi) / 2.0
}

// ---------------------------------------------------------------------------
// Station metadata
// ---------------------------------------------------------------------------

/// Station metadata associated with a sounding.
#[derive(Debug, Clone, Default)]
pub struct StationInfo {
    /// Station identifier (e.g., "OUN", "72357").
    pub station_id: String,
    /// Latitude (degrees, north positive).
    pub latitude: f64,
    /// Longitude (degrees, east positive).
    pub longitude: f64,
    /// Station elevation (m MSL).
    pub elevation: f64,
    /// Observation date/time as ISO-8601 string.
    pub datetime: String,
}

// ---------------------------------------------------------------------------
// Profile
// ---------------------------------------------------------------------------

/// A vertical atmospheric sounding profile.
///
/// Levels are ordered by **descending** pressure (surface at index 0, top of
/// sounding at the end).  Missing values are stored as `f64::NAN`.
#[derive(Debug, Clone)]
pub struct Profile {
    // --- raw input arrays (all same length) ---
    /// Pressure (hPa), descending.
    pub pres: Vec<f64>,
    /// Geopotential height (m MSL).
    pub hght: Vec<f64>,
    /// Temperature (C).
    pub tmpc: Vec<f64>,
    /// Dewpoint temperature (C).
    pub dwpc: Vec<f64>,
    /// Wind direction (meteorological degrees).
    pub wdir: Vec<f64>,
    /// Wind speed (knots).
    pub wspd: Vec<f64>,
    /// Vertical velocity (Pa/s).  Optional — filled with NAN if absent.
    pub omeg: Vec<f64>,

    // --- derived wind components ---
    /// U-component of wind (knots).
    pub u: Vec<f64>,
    /// V-component of wind (knots).
    pub v: Vec<f64>,

    // --- derived thermodynamic profiles ---
    /// Log10 of pressure.
    pub logp: Vec<f64>,
    /// Virtual temperature (C).
    pub vtmp: Vec<f64>,
    /// Potential temperature (K).
    pub theta: Vec<f64>,
    /// Equivalent potential temperature (K).
    pub thetae: Vec<f64>,
    /// Water-vapor mixing ratio (g/kg).
    pub wvmr: Vec<f64>,
    /// Relative humidity (%).
    pub relh: Vec<f64>,
    /// Wetbulb temperature (C).
    pub wetbulb: Vec<f64>,

    // --- indices into the arrays ---
    /// Index of the surface level (lowest valid temperature).
    pub sfc: usize,
    /// Index of the top level (highest valid temperature).
    pub top: usize,

    // --- metadata ---
    pub station: StationInfo,
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can occur during profile construction or I/O.
#[derive(Debug, Clone)]
pub enum ProfileError {
    /// An input array has fewer than 2 elements or arrays differ in length.
    InvalidLength(String),
    /// All pressure values are below 100 hPa (likely bad data).
    PressureTooLow,
    /// No valid temperature levels found.
    NoValidData,
    /// I/O or parse error.
    ParseError(String),
}

impl fmt::Display for ProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLength(msg) => write!(f, "invalid array length: {msg}"),
            Self::PressureTooLow => write!(f, "all pressure values are below 100 hPa"),
            Self::NoValidData => write!(f, "no valid temperature data found"),
            Self::ParseError(msg) => write!(f, "parse error: {msg}"),
        }
    }
}

impl std::error::Error for ProfileError {}

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

impl Profile {
    /// Create a new profile from raw sounding arrays.
    ///
    /// # Arguments
    ///
    /// * `pres` — Pressure levels (hPa).
    /// * `hght` — Heights (m MSL).
    /// * `tmpc` — Temperatures (C).
    /// * `dwpc` — Dewpoints (C).
    /// * `wdir` — Wind directions (degrees).  Pass empty slice if unavailable.
    /// * `wspd` — Wind speeds (knots).  Pass empty slice if unavailable.
    /// * `omeg` — Vertical velocity (Pa/s).  Pass empty slice if unavailable.
    /// * `station` — Station metadata.
    ///
    /// Arrays are validated, missing-flagged values are converted to NAN, and
    /// levels are sorted by descending pressure.  Derived fields (theta,
    /// thetae, mixing ratio, etc.) are computed.
    pub fn new(
        pres: &[f64],
        hght: &[f64],
        tmpc: &[f64],
        dwpc: &[f64],
        wdir: &[f64],
        wspd: &[f64],
        omeg: &[f64],
        station: StationInfo,
    ) -> Result<Self, ProfileError> {
        let n = pres.len();
        if n < 2 {
            return Err(ProfileError::InvalidLength(
                "pressure array must have at least 2 levels".into(),
            ));
        }
        if hght.len() != n || tmpc.len() != n || dwpc.len() != n {
            return Err(ProfileError::InvalidLength(
                "pres, hght, tmpc, and dwpc arrays must all have the same length".into(),
            ));
        }

        // Wind: if provided, must match length.
        let have_wind_dir = !wdir.is_empty();
        if have_wind_dir {
            if wdir.len() != n || wspd.len() != n {
                return Err(ProfileError::InvalidLength(
                    "wdir and wspd arrays must match pres length".into(),
                ));
            }
        }

        let have_omeg = !omeg.is_empty();
        if have_omeg && omeg.len() != n {
            return Err(ProfileError::InvalidLength(
                "omeg array must match pres length".into(),
            ));
        }

        // Mask missing values and build owned arrays.
        let mut pres_v: Vec<f64> = pres.iter().map(|&v| mask_missing(v)).collect();
        let mut hght_v: Vec<f64> = hght.iter().map(|&v| mask_missing(v)).collect();
        let mut tmpc_v: Vec<f64> = tmpc.iter().map(|&v| mask_missing(v)).collect();
        let mut dwpc_v: Vec<f64> = dwpc.iter().map(|&v| mask_missing(v)).collect();
        let mut wdir_v: Vec<f64> = if have_wind_dir {
            wdir.iter().map(|&v| mask_missing(v)).collect()
        } else {
            vec![f64::NAN; n]
        };
        let mut wspd_v: Vec<f64> = if have_wind_dir {
            wspd.iter().map(|&v| mask_missing(v)).collect()
        } else {
            vec![f64::NAN; n]
        };
        let mut omeg_v: Vec<f64> = if have_omeg {
            omeg.iter().map(|&v| mask_missing(v)).collect()
        } else {
            vec![f64::NAN; n]
        };

        // Cross-mask winds: if one component is NAN, mask the other.
        for i in 0..n {
            if wdir_v[i].is_nan() || wspd_v[i].is_nan() {
                wdir_v[i] = f64::NAN;
                wspd_v[i] = f64::NAN;
            }
        }

        // Sort levels by descending pressure (surface first).
        let mut indices: Vec<usize> = (0..n).collect();
        indices.sort_by(|&a, &b| {
            let pa = if pres_v[a].is_nan() {
                f64::NEG_INFINITY
            } else {
                pres_v[a]
            };
            let pb = if pres_v[b].is_nan() {
                f64::NEG_INFINITY
            } else {
                pres_v[b]
            };
            pb.partial_cmp(&pa).unwrap_or(std::cmp::Ordering::Equal)
        });

        let reorder = |src: &mut Vec<f64>| {
            let tmp: Vec<f64> = indices.iter().map(|&i| src[i]).collect();
            *src = tmp;
        };

        reorder(&mut pres_v);
        reorder(&mut hght_v);
        reorder(&mut tmpc_v);
        reorder(&mut dwpc_v);
        reorder(&mut wdir_v);
        reorder(&mut wspd_v);
        reorder(&mut omeg_v);

        // Check for reasonable pressure range.
        let max_pres = pres_v
            .iter()
            .copied()
            .filter(|v| v.is_finite())
            .fold(f64::NEG_INFINITY, f64::max);
        if max_pres <= 100.0 {
            return Err(ProfileError::PressureTooLow);
        }

        // Surface / top indices (first/last valid temperature).
        let sfc = first_valid_index(&tmpc_v).ok_or(ProfileError::NoValidData)?;
        let top = last_valid_index(&tmpc_v).ok_or(ProfileError::NoValidData)?;

        // Compute u/v from wdir/wspd.
        let mut u_v = Vec::with_capacity(n);
        let mut v_v = Vec::with_capacity(n);
        for i in 0..n {
            let (u, v) = vec2comp(wdir_v[i], wspd_v[i]);
            u_v.push(u);
            v_v.push(v);
        }

        // Derived fields.
        let logp: Vec<f64> = pres_v
            .iter()
            .map(|&p| {
                if p.is_finite() && p > 0.0 {
                    p.log10()
                } else {
                    f64::NAN
                }
            })
            .collect();

        let vtmp_v: Vec<f64> = (0..n)
            .map(|i| {
                let vt = virtemp(pres_v[i], tmpc_v[i], dwpc_v[i]);
                // Fall back to dry temperature if dewpoint is missing.
                if vt.is_nan() && tmpc_v[i].is_finite() {
                    tmpc_v[i]
                } else {
                    vt
                }
            })
            .collect();

        let theta_v: Vec<f64> = (0..n)
            .map(|i| {
                let th = theta(pres_v[i], tmpc_v[i]);
                if th.is_finite() {
                    ctok(th)
                } else {
                    f64::NAN
                }
            })
            .collect();

        let thetae_v: Vec<f64> = (0..n)
            .map(|i| thetae(pres_v[i], tmpc_v[i], dwpc_v[i]))
            .collect();

        let wvmr_v: Vec<f64> = (0..n).map(|i| mixratio(pres_v[i], dwpc_v[i])).collect();

        let relh_v: Vec<f64> = (0..n)
            .map(|i| relh(pres_v[i], tmpc_v[i], dwpc_v[i]))
            .collect();

        let wetbulb_v: Vec<f64> = (0..n)
            .map(|i| wetbulb(pres_v[i], tmpc_v[i], dwpc_v[i]))
            .collect();

        Ok(Self {
            pres: pres_v,
            hght: hght_v,
            tmpc: tmpc_v,
            dwpc: dwpc_v,
            wdir: wdir_v,
            wspd: wspd_v,
            omeg: omeg_v,
            u: u_v,
            v: v_v,
            logp,
            vtmp: vtmp_v,
            theta: theta_v,
            thetae: thetae_v,
            wvmr: wvmr_v,
            relh: relh_v,
            wetbulb: wetbulb_v,
            sfc,
            top,
            station,
        })
    }

    /// Convenience constructor when winds are supplied as u/v components
    /// instead of direction/speed.
    pub fn from_uv(
        pres: &[f64],
        hght: &[f64],
        tmpc: &[f64],
        dwpc: &[f64],
        u: &[f64],
        v: &[f64],
        omeg: &[f64],
        station: StationInfo,
    ) -> Result<Self, ProfileError> {
        let n = pres.len();
        if u.len() != n || v.len() != n {
            return Err(ProfileError::InvalidLength(
                "u and v arrays must match pres length".into(),
            ));
        }
        // Convert u/v → wdir/wspd, then delegate.
        let mut wdir = Vec::with_capacity(n);
        let mut wspd = Vec::with_capacity(n);
        for i in 0..n {
            let (d, s) = comp2vec(mask_missing(u[i]), mask_missing(v[i]));
            wdir.push(d);
            wspd.push(s);
        }
        Self::new(pres, hght, tmpc, dwpc, &wdir, &wspd, omeg, station)
    }

    /// Number of levels in the sounding.
    #[inline]
    pub fn num_levels(&self) -> usize {
        self.pres.len()
    }

    /// Surface pressure (hPa).
    #[inline]
    pub fn sfc_pressure(&self) -> f64 {
        self.pres[self.sfc]
    }

    /// Surface height (m MSL).
    #[inline]
    pub fn sfc_height(&self) -> f64 {
        self.hght[self.sfc]
    }

    /// Convert a height AGL (m) to MSL (m) using the surface height.
    #[inline]
    pub fn to_msl(&self, agl: f64) -> f64 {
        agl + self.sfc_height()
    }

    /// Convert a height MSL (m) to AGL (m) using the surface height.
    #[inline]
    pub fn to_agl(&self, msl: f64) -> f64 {
        msl - self.sfc_height()
    }
}

// ---------------------------------------------------------------------------
// Interpolation
// ---------------------------------------------------------------------------

impl Profile {
    /// Log-linear interpolation of a field to a given pressure level.
    ///
    /// `field` must be the same length as `self.pres`.  Interpolation is
    /// log-linear in pressure, following SHARPpy convention.  Returns `NAN`
    /// if the pressure is outside the sounding range or data are missing.
    pub fn interp_by_pressure(&self, field: &[f64], target_pres: f64) -> f64 {
        if !target_pres.is_finite() || target_pres <= 0.0 {
            return f64::NAN;
        }
        let log_target = target_pres.ln();

        // Find the bounding levels.  pres is descending, so walk until we
        // pass below target_pres.
        let n = self.pres.len();
        for i in 0..n - 1 {
            let p0 = self.pres[i];
            let p1 = self.pres[i + 1];
            if !p0.is_finite() || !p1.is_finite() {
                continue;
            }
            if target_pres <= p0 && target_pres >= p1 {
                let f0 = field[i];
                let f1 = field[i + 1];
                if !f0.is_finite() || !f1.is_finite() {
                    return f64::NAN;
                }
                let lp0 = p0.ln();
                let lp1 = p1.ln();
                let denom = lp0 - lp1;
                if denom.abs() < TOL {
                    return f0;
                }
                let frac = (log_target - lp1) / denom;
                return f1 + (f0 - f1) * frac;
            }
        }
        f64::NAN
    }

    /// Interpolate temperature (C) to a given pressure level.
    pub fn interp_tmpc(&self, pres: f64) -> f64 {
        self.interp_by_pressure(&self.tmpc, pres)
    }

    /// Interpolate dewpoint (C) to a given pressure level.
    pub fn interp_dwpc(&self, pres: f64) -> f64 {
        self.interp_by_pressure(&self.dwpc, pres)
    }

    /// Interpolate height (m MSL) to a given pressure level.
    pub fn interp_hght(&self, pres: f64) -> f64 {
        self.interp_by_pressure(&self.hght, pres)
    }

    /// Interpolate wind components and return (u, v) in knots at a given
    /// pressure level.
    pub fn interp_wind(&self, pres: f64) -> (f64, f64) {
        let ui = self.interp_by_pressure(&self.u, pres);
        let vi = self.interp_by_pressure(&self.v, pres);
        (ui, vi)
    }

    /// Interpolate wind and return (wdir, wspd) at a given pressure level.
    pub fn interp_vec(&self, pres: f64) -> (f64, f64) {
        let (u, v) = self.interp_wind(pres);
        comp2vec(u, v)
    }

    /// Find the pressure (hPa) at a given height (m MSL) by interpolation.
    ///
    /// Uses log-linear interpolation between the two surrounding height
    /// levels.
    pub fn pres_at_height(&self, target_hght: f64) -> f64 {
        if !target_hght.is_finite() {
            return f64::NAN;
        }
        let n = self.pres.len();
        // Heights increase as pressure decreases (index increases).
        for i in 0..n - 1 {
            let h0 = self.hght[i];
            let h1 = self.hght[i + 1];
            let p0 = self.pres[i];
            let p1 = self.pres[i + 1];
            if !h0.is_finite() || !h1.is_finite() || !p0.is_finite() || !p1.is_finite() {
                continue;
            }
            if (target_hght >= h0 && target_hght <= h1) || (target_hght <= h0 && target_hght >= h1)
            {
                let dh = h1 - h0;
                if dh.abs() < TOL {
                    return p0;
                }
                let frac = (target_hght - h0) / dh;
                let lp0 = p0.ln();
                let lp1 = p1.ln();
                return (lp0 + frac * (lp1 - lp0)).exp();
            }
        }
        f64::NAN
    }
}

// ---------------------------------------------------------------------------
// Profile manipulation
// ---------------------------------------------------------------------------

impl Profile {
    /// Return a new profile truncated to levels between `pbot` and `ptop`
    /// (inclusive, hPa).  `pbot` > `ptop` (surface to top).
    pub fn truncate(&self, pbot: f64, ptop: f64) -> Result<Self, ProfileError> {
        let idx: Vec<usize> = (0..self.num_levels())
            .filter(|&i| {
                let p = self.pres[i];
                p.is_finite() && p <= pbot && p >= ptop
            })
            .collect();

        if idx.len() < 2 {
            return Err(ProfileError::InvalidLength(
                "truncated profile has fewer than 2 valid levels".into(),
            ));
        }

        let extract = |src: &[f64]| -> Vec<f64> { idx.iter().map(|&i| src[i]).collect() };

        Profile::new(
            &extract(&self.pres),
            &extract(&self.hght),
            &extract(&self.tmpc),
            &extract(&self.dwpc),
            &extract(&self.wdir),
            &extract(&self.wspd),
            &extract(&self.omeg),
            self.station.clone(),
        )
    }
}

// ---------------------------------------------------------------------------
// I/O: SHARPpy text format
// ---------------------------------------------------------------------------

impl Profile {
    /// Parse a sounding from the SHARPpy `%RAW%` text format.
    ///
    /// The format has a `%TITLE%` header line, column header, then
    /// `%RAW%` / `%END%` delimited CSV data with columns:
    /// `PRES, HGHT, TMPC, DWPC, WDIR, WSPD`
    ///
    /// Missing values are encoded as `-9999.00`.
    pub fn from_sharppy_text(text: &str) -> Result<Self, ProfileError> {
        let mut pres = Vec::new();
        let mut hght = Vec::new();
        let mut tmpc = Vec::new();
        let mut dwpc = Vec::new();
        let mut wdir = Vec::new();
        let mut wspd = Vec::new();
        let mut station = StationInfo::default();

        let mut in_raw = false;
        let mut title_next = false;

        for line in text.lines() {
            let trimmed = line.trim();

            if trimmed == "%TITLE%" {
                title_next = true;
                continue;
            }
            if title_next {
                // Title line: "STATION   YYMMDD/HHMM"
                let parts: Vec<&str> = trimmed.split_whitespace().collect();
                if !parts.is_empty() {
                    station.station_id = parts[0].to_string();
                }
                if parts.len() > 1 {
                    station.datetime = parts[1].to_string();
                }
                title_next = false;
                continue;
            }
            if trimmed == "%RAW%" {
                in_raw = true;
                continue;
            }
            if trimmed == "%END%" {
                break;
            }
            if !in_raw {
                continue;
            }

            // Data line: comma or whitespace separated.
            let vals: Vec<f64> = trimmed
                .split(|c: char| c == ',' || c.is_whitespace())
                .filter(|s| !s.is_empty())
                .map(|s| s.trim().parse::<f64>().unwrap_or(f64::NAN))
                .collect();

            if vals.len() >= 6 {
                pres.push(vals[0]);
                hght.push(vals[1]);
                tmpc.push(vals[2]);
                dwpc.push(vals[3]);
                wdir.push(vals[4]);
                wspd.push(vals[5]);
            }
        }

        if pres.len() < 2 {
            return Err(ProfileError::ParseError(
                "fewer than 2 data lines in %RAW% section".into(),
            ));
        }

        Profile::new(&pres, &hght, &tmpc, &dwpc, &wdir, &wspd, &[], station)
    }

    /// Parse a sounding from the University of Wyoming upper-air text format.
    ///
    /// Typical URL: `http://weather.uwyo.edu/cgi-bin/sounding.py?...&TYPE=TEXT%3ALIST`
    ///
    /// Expected columns (whitespace separated):
    /// `PRES  HGHT  TEMP  DWPT  RELH  MIXR  DRCT  SKNT  THTA  THTE  THTV`
    ///
    /// The station header line (e.g., `72357 OUN ...`) provides the station ID
    /// and number.  Latitude/longitude/elevation are parsed from the footer.
    pub fn from_wyoming(text: &str) -> Result<Self, ProfileError> {
        let mut pres = Vec::new();
        let mut hght = Vec::new();
        let mut tmpc = Vec::new();
        let mut dwpc = Vec::new();
        let mut wdir = Vec::new();
        let mut wspd = Vec::new();
        let mut station = StationInfo::default();

        let mut in_data = false;
        let mut dash_count = 0;

        for line in text.lines() {
            let trimmed = line.trim();

            // The data block is bracketed by dashed-line separators.
            if trimmed.starts_with("---") {
                dash_count += 1;
                if dash_count == 1 {
                    // After first dashes, the next non-header lines are data.
                    in_data = false; // column header follows
                    continue;
                }
                if dash_count == 2 {
                    in_data = true; // data starts after second dashes
                    continue;
                }
                if dash_count >= 3 {
                    in_data = false; // data ends at third dashes
                    continue;
                }
            }

            // Parse station metadata from header/footer.
            if trimmed.contains("Station number:") || trimmed.contains("Station number :") {
                if let Some(num) = trimmed.split(':').nth(1) {
                    station.station_id = num.trim().to_string();
                }
            }
            if trimmed.contains("Station latitude:") || trimmed.contains("Station latitude :") {
                if let Some(val) = trimmed.split(':').nth(1) {
                    station.latitude = val.trim().parse().unwrap_or(f64::NAN);
                }
            }
            if trimmed.contains("Station longitude:") || trimmed.contains("Station longitude :") {
                if let Some(val) = trimmed.split(':').nth(1) {
                    station.longitude = val.trim().parse().unwrap_or(f64::NAN);
                }
            }
            if trimmed.contains("Station elevation:") || trimmed.contains("Station elevation :") {
                if let Some(val) = trimmed.split(':').nth(1) {
                    station.elevation = val
                        .trim()
                        .trim_end_matches('m')
                        .trim()
                        .parse()
                        .unwrap_or(f64::NAN);
                }
            }
            if trimmed.contains("Observation time:") || trimmed.contains("Observation time :") {
                if let Some(val) = trimmed.split(':').nth(1) {
                    station.datetime = val.trim().to_string();
                }
            }

            if !in_data {
                continue;
            }

            // Data line.  Columns: PRES HGHT TEMP DWPT RELH MIXR DRCT SKNT ...
            let vals: Vec<&str> = trimmed.split_whitespace().collect();
            if vals.len() < 8 {
                continue;
            }
            let parse = |s: &str| -> f64 { s.parse::<f64>().unwrap_or(f64::NAN) };

            pres.push(parse(vals[0]));
            hght.push(parse(vals[1]));
            tmpc.push(parse(vals[2]));
            dwpc.push(parse(vals[3]));
            // vals[4] = RELH, vals[5] = MIXR — skip, we compute our own.
            wdir.push(parse(vals[6]));
            wspd.push(parse(vals[7]));
        }

        if pres.len() < 2 {
            return Err(ProfileError::ParseError(
                "fewer than 2 valid data lines in Wyoming sounding".into(),
            ));
        }

        Profile::new(&pres, &hght, &tmpc, &dwpc, &wdir, &wspd, &[], station)
    }

    /// Parse a simple CSV sounding file.
    ///
    /// Expected header row: `pres,hght,tmpc,dwpc,wdir,wspd`
    /// (case-insensitive, optional additional columns ignored).
    /// Lines starting with `#` are comments.
    pub fn from_csv(text: &str) -> Result<Self, ProfileError> {
        let mut pres = Vec::new();
        let mut hght = Vec::new();
        let mut tmpc = Vec::new();
        let mut dwpc = Vec::new();
        let mut wdir = Vec::new();
        let mut wspd = Vec::new();
        let mut omeg = Vec::new();

        let mut header_indices: Option<(usize, usize, usize, usize, usize, usize, Option<usize>)> =
            None;

        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            let cols: Vec<&str> = trimmed.split(',').map(|s| s.trim()).collect();

            // Detect header row.
            if header_indices.is_none() {
                let lower: Vec<String> = cols.iter().map(|s| s.to_lowercase()).collect();
                let find = |name: &str| lower.iter().position(|s| s == name);
                if let (Some(ip), Some(ih), Some(it), Some(id), Some(iwd), Some(iws)) = (
                    find("pres"),
                    find("hght"),
                    find("tmpc"),
                    find("dwpc"),
                    find("wdir"),
                    find("wspd"),
                ) {
                    let io = find("omeg");
                    header_indices = Some((ip, ih, it, id, iwd, iws, io));
                    continue;
                }
                // If no header detected, assume default order.
                header_indices = Some((
                    0,
                    1,
                    2,
                    3,
                    4,
                    5,
                    if cols.len() > 6 { Some(6) } else { None },
                ));
            }

            let (ip, ih, it, id, iwd, iws, io) = header_indices.unwrap();
            let parse = |idx: usize| -> f64 {
                cols.get(idx)
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or(f64::NAN)
            };

            pres.push(parse(ip));
            hght.push(parse(ih));
            tmpc.push(parse(it));
            dwpc.push(parse(id));
            wdir.push(parse(iwd));
            wspd.push(parse(iws));
            if let Some(oi) = io {
                omeg.push(parse(oi));
            }
        }

        if pres.len() < 2 {
            return Err(ProfileError::ParseError(
                "fewer than 2 data rows in CSV".into(),
            ));
        }

        let omeg_slice = if omeg.len() == pres.len() {
            &omeg[..]
        } else {
            &[]
        };
        Profile::new(
            &pres,
            &hght,
            &tmpc,
            &dwpc,
            &wdir,
            &wspd,
            omeg_slice,
            StationInfo::default(),
        )
    }

    /// Serialize the profile to SHARPpy `%RAW%` text format.
    pub fn to_sharppy_text(&self) -> String {
        let mut out = String::new();
        out.push_str("%TITLE%\n");
        out.push_str(&format!(
            " {}   {}\n",
            self.station.station_id, self.station.datetime
        ));
        out.push_str("   LEVEL       HGHT       TEMP       DWPT       WDIR       WSPD\n");
        out.push_str("-------------------------------------------------------------------\n");
        out.push_str("%RAW%\n");

        for i in 0..self.num_levels() {
            let qc = |v: f64| -> f64 {
                if v.is_finite() {
                    v
                } else {
                    MISSING
                }
            };
            out.push_str(&format!(
                "{:>8.2},  {:>8.2},  {:>8.2},  {:>8.2},  {:>8.2},  {:>8.2}\n",
                qc(self.pres[i]),
                qc(self.hght[i]),
                qc(self.tmpc[i]),
                qc(self.dwpc[i]),
                qc(self.wdir[i]),
                qc(self.wspd[i]),
            ));
        }
        out.push_str("%END%\n");
        out
    }
}

// ---------------------------------------------------------------------------
// Display
// ---------------------------------------------------------------------------

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Profile({} levels, sfc={:.1} hPa / {:.0} m, top={:.1} hPa / {:.0} m, station={})",
            self.num_levels(),
            self.pres[self.sfc],
            self.hght[self.sfc],
            self.pres[self.top],
            self.hght[self.top],
            self.station.station_id,
        )
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a small 5-level test sounding.
    fn test_sounding() -> Profile {
        // Loosely modeled on a typical warm-season sounding.
        let pres = [1000.0, 925.0, 850.0, 700.0, 500.0];
        let hght = [100.0, 800.0, 1500.0, 3100.0, 5600.0];
        let tmpc = [30.0, 24.0, 18.0, 4.0, -15.0];
        let dwpc = [22.0, 18.0, 12.0, -4.0, -30.0];
        let wdir = [180.0, 200.0, 220.0, 250.0, 270.0];
        let wspd = [10.0, 15.0, 20.0, 30.0, 50.0];

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

    #[test]
    fn construction_basic() {
        let prof = test_sounding();
        assert_eq!(prof.num_levels(), 5);
        assert_eq!(prof.sfc, 0);
        assert_eq!(prof.top, 4);
        assert!((prof.pres[0] - 1000.0).abs() < TOL);
        assert!((prof.pres[4] - 500.0).abs() < TOL);
    }

    #[test]
    fn rejects_too_short() {
        let res = Profile::new(
            &[1000.0],
            &[100.0],
            &[20.0],
            &[10.0],
            &[],
            &[],
            &[],
            StationInfo::default(),
        );
        assert!(res.is_err());
    }

    #[test]
    fn rejects_mismatched_lengths() {
        let res = Profile::new(
            &[1000.0, 500.0],
            &[100.0, 5600.0, 999.0],
            &[20.0, -10.0],
            &[10.0, -20.0],
            &[],
            &[],
            &[],
            StationInfo::default(),
        );
        assert!(res.is_err());
    }

    #[test]
    fn sorts_by_descending_pressure() {
        // Supply levels in ascending pressure (wrong order).
        let pres = [500.0, 700.0, 850.0, 925.0, 1000.0];
        let hght = [5600.0, 3100.0, 1500.0, 800.0, 100.0];
        let tmpc = [-15.0, 4.0, 18.0, 24.0, 30.0];
        let dwpc = [-30.0, -4.0, 12.0, 18.0, 22.0];
        let wdir = [270.0, 250.0, 220.0, 200.0, 180.0];
        let wspd = [50.0, 30.0, 20.0, 15.0, 10.0];

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
        // After sorting, surface (highest pressure) should be first.
        assert!((prof.pres[0] - 1000.0).abs() < TOL);
        assert!((prof.tmpc[0] - 30.0).abs() < TOL);
    }

    #[test]
    fn missing_values_become_nan() {
        let pres = [1000.0, 850.0, 500.0];
        let hght = [100.0, 1500.0, 5600.0];
        let tmpc = [30.0, MISSING, -15.0];
        let dwpc = [22.0, 12.0, MISSING];
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
        assert!(prof.tmpc[1].is_nan()); // level at 850
        assert!(prof.dwpc[2].is_nan()); // level at 500
    }

    #[test]
    fn derived_fields_computed() {
        let prof = test_sounding();
        // Theta should be > 0 K at every valid level.
        for i in 0..prof.num_levels() {
            assert!(prof.theta[i].is_finite());
            assert!(prof.theta[i] > 273.0);
        }
        // Theta-e should be > theta at humid levels.
        assert!(prof.thetae[0] > prof.theta[0]);
        // Mixing ratio should be positive at surface.
        assert!(prof.wvmr[0] > 0.0);
        // RH should be between 0 and 100.
        for &rh in &prof.relh {
            if rh.is_finite() {
                assert!(rh >= 0.0 && rh <= 100.0);
            }
        }
    }

    #[test]
    fn wind_conversions_roundtrip() {
        let (u, v) = vec2comp(225.0, 20.0);
        let (d, s) = comp2vec(u, v);
        assert!((d - 225.0).abs() < 0.01);
        assert!((s - 20.0).abs() < 0.01);
    }

    #[test]
    fn wind_north() {
        let (u, v) = vec2comp(0.0, 10.0);
        assert!(u.abs() < 0.01);
        assert!((v - (-10.0)).abs() < 0.01);
    }

    #[test]
    fn interp_tmpc_exact_level() {
        let prof = test_sounding();
        let t = prof.interp_tmpc(850.0);
        assert!((t - 18.0).abs() < 0.01);
    }

    #[test]
    fn interp_tmpc_between_levels() {
        let prof = test_sounding();
        let t = prof.interp_tmpc(775.0);
        // Should be between 18C (850) and 4C (700).
        assert!(t > 4.0 && t < 18.0);
    }

    #[test]
    fn interp_hght() {
        let prof = test_sounding();
        let h = prof.interp_hght(700.0);
        assert!((h - 3100.0).abs() < 0.1);
    }

    #[test]
    fn pres_at_height_roundtrip() {
        let prof = test_sounding();
        let h = prof.interp_hght(775.0);
        let p = prof.pres_at_height(h);
        assert!((p - 775.0).abs() < 0.5);
    }

    #[test]
    fn truncate() {
        let prof = test_sounding();
        let trunc = prof.truncate(925.0, 500.0).unwrap();
        assert_eq!(trunc.num_levels(), 4);
        assert!((trunc.pres[0] - 925.0).abs() < TOL);
    }

    #[test]
    fn from_uv_constructor() {
        let pres = [1000.0, 850.0, 500.0];
        let hght = [100.0, 1500.0, 5600.0];
        let tmpc = [30.0, 18.0, -15.0];
        let dwpc = [22.0, 12.0, -30.0];
        let u = [-5.0, -10.0, -25.0];
        let v = [-8.66, -12.0, 0.0];

        let prof = Profile::from_uv(
            &pres,
            &hght,
            &tmpc,
            &dwpc,
            &u,
            &v,
            &[],
            StationInfo::default(),
        )
        .unwrap();
        // Wind speed at first level: sqrt(25+75) = 10
        assert!((prof.wspd[0] - 10.0).abs() < 0.1);
    }

    #[test]
    fn to_msl_to_agl() {
        let prof = test_sounding();
        let msl = prof.to_msl(1000.0);
        assert!((msl - 1100.0).abs() < TOL);
        let agl = prof.to_agl(msl);
        assert!((agl - 1000.0).abs() < TOL);
    }

    #[test]
    fn sharppy_text_roundtrip() {
        let prof = test_sounding();
        let text = prof.to_sharppy_text();
        let prof2 = Profile::from_sharppy_text(&text).unwrap();
        assert_eq!(prof.num_levels(), prof2.num_levels());
        for i in 0..prof.num_levels() {
            assert!((prof.pres[i] - prof2.pres[i]).abs() < 0.01);
            assert!((prof.tmpc[i] - prof2.tmpc[i]).abs() < 0.01);
        }
    }

    #[test]
    fn csv_parse() {
        let csv = "\
# comment
pres,hght,tmpc,dwpc,wdir,wspd
1000,100,30,22,180,10
850,1500,18,12,220,20
500,5600,-15,-30,270,50
";
        let prof = Profile::from_csv(csv).unwrap();
        assert_eq!(prof.num_levels(), 3);
        assert!((prof.pres[0] - 1000.0).abs() < TOL);
    }

    #[test]
    fn wyoming_parse() {
        let text = "\
                         Station number : 72357
                         Station latitude : 35.18
                         Station longitude : -97.44
                         Station elevation : 357.0

           -----------------------------------------------------------------------------
              PRES   HGHT   TEMP   DWPT   RELH   MIXR   DRCT   SKNT   THTA   THTE   THTV
              hPa     m      C      C      %    g/kg    deg    knot     K      K      K
           -----------------------------------------------------------------------------
            965.0    357   28.4   20.4     61  15.53    170     10  302.9  350.5  305.6
            932.4    660   25.6   19.4     68  14.76    185     15  302.8  348.2  305.3
            850.0   1481   19.0   13.0     68  10.69    220     25  302.6  336.9  304.5
            700.0   3153    5.4   -4.6     44   4.36    250     35  305.7  321.3  306.5
            500.0   5720  -14.9  -28.9     19   0.84    270     55  310.1  313.3  310.2
           -----------------------------------------------------------------------------
";
        let prof = Profile::from_wyoming(text).unwrap();
        assert_eq!(prof.num_levels(), 5);
        assert_eq!(prof.station.station_id, "72357");
        assert!((prof.station.latitude - 35.18).abs() < 0.01);
        assert!((prof.pres[0] - 965.0).abs() < 0.1);
    }

    #[test]
    fn theta_surface() {
        // At 1000 hPa, theta ~ tmpc (in K).
        let th = theta(1000.0, 20.0);
        assert!((ctok(th) - 293.15).abs() < 0.5);
    }

    #[test]
    fn mixratio_positive() {
        let w = mixratio(1000.0, 20.0);
        assert!(w > 10.0 && w < 20.0);
    }

    #[test]
    fn relh_saturated() {
        // T == Td → 100% RH.
        let rh = relh(1000.0, 20.0, 20.0);
        assert!((rh - 100.0).abs() < 0.1);
    }

    #[test]
    fn wetbulb_between_t_and_td() {
        let wb = wetbulb(850.0, 20.0, 10.0);
        assert!(wb > 10.0 && wb < 20.0);
    }

    #[test]
    fn display_format() {
        let prof = test_sounding();
        let s = format!("{prof}");
        assert!(s.contains("5 levels"));
        assert!(s.contains("1000.0 hPa"));
    }
}
