//! PyO3 bindings exposing sharprs as a Python module.
//!
//! Module hierarchy:
//!   sharprs
//!   ├── Profile          (pyclass — wraps profile::Profile)
//!   ├── params            (sub-module: CAPE/CIN, STP, SCP, SHIP, EIL)
//!   ├── thermo            (sub-module: theta, thetae, wetbulb, mixing_ratio)
//!   ├── winds             (sub-module: bunkers_motion, helicity, mean_wind, wind_shear)
//!   └── fire              (sub-module: haines_index, fosberg_fwi, hot_dry_windy)
//!
//! Build with: `maturin develop --features python`

use numpy::ndarray::Array1;
use numpy::{IntoPyArray, PyArray1, PyReadonlyArray1};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

use crate::constants::*;
use crate::profile as prof;

// ============================================================================
// PyProfile — Python wrapper around profile::Profile
// ============================================================================

/// A vertical atmospheric sounding profile.
///
/// Construct from Python lists or numpy arrays of pressure, height,
/// temperature, dewpoint, wind direction, and wind speed.
#[pyclass(name = "Profile")]
#[derive(Clone)]
pub struct PyProfile {
    inner: prof::Profile,
}

#[pymethods]
impl PyProfile {
    /// Create a new Profile.
    ///
    /// Args:
    ///     pres: pressure levels (hPa), highest first
    ///     hght: geopotential heights (m MSL)
    ///     tmpc: temperatures (°C)
    ///     dwpc: dewpoint temperatures (°C)
    ///     wdir: wind directions (meteorological degrees)
    ///     wspd: wind speeds (knots)
    #[new]
    #[pyo3(signature = (pres, hght, tmpc, dwpc, wdir, wspd))]
    fn new(
        pres: Vec<f64>,
        hght: Vec<f64>,
        tmpc: Vec<f64>,
        dwpc: Vec<f64>,
        wdir: Vec<f64>,
        wspd: Vec<f64>,
    ) -> PyResult<Self> {
        let inner = prof::Profile::new(
            &pres,
            &hght,
            &tmpc,
            &dwpc,
            &wdir,
            &wspd,
            &[],
            prof::StationInfo::default(),
        )
        .map_err(|e| PyValueError::new_err(e.to_string()))?;
        Ok(Self { inner })
    }

    /// Number of levels in the profile.
    fn __len__(&self) -> usize {
        self.inner.num_levels()
    }

    fn __repr__(&self) -> String {
        format!("{}", self.inner)
    }

    /// Pressure array (hPa).
    #[getter]
    fn pres<'py>(&self, py: Python<'py>) -> Bound<'py, PyArray1<f64>> {
        Array1::from_vec(self.inner.pres.clone()).into_pyarray(py)
    }

    /// Height array (m MSL).
    #[getter]
    fn hght<'py>(&self, py: Python<'py>) -> Bound<'py, PyArray1<f64>> {
        Array1::from_vec(self.inner.hght.clone()).into_pyarray(py)
    }

    /// Temperature array (°C).
    #[getter]
    fn tmpc<'py>(&self, py: Python<'py>) -> Bound<'py, PyArray1<f64>> {
        Array1::from_vec(self.inner.tmpc.clone()).into_pyarray(py)
    }

    /// Dewpoint array (°C).
    #[getter]
    fn dwpc<'py>(&self, py: Python<'py>) -> Bound<'py, PyArray1<f64>> {
        Array1::from_vec(self.inner.dwpc.clone()).into_pyarray(py)
    }

    /// Wind direction array (degrees).
    #[getter]
    fn wdir<'py>(&self, py: Python<'py>) -> Bound<'py, PyArray1<f64>> {
        Array1::from_vec(self.inner.wdir.clone()).into_pyarray(py)
    }

    /// Wind speed array (knots).
    #[getter]
    fn wspd<'py>(&self, py: Python<'py>) -> Bound<'py, PyArray1<f64>> {
        Array1::from_vec(self.inner.wspd.clone()).into_pyarray(py)
    }

    /// U-component of wind (knots).
    #[getter]
    fn u<'py>(&self, py: Python<'py>) -> Bound<'py, PyArray1<f64>> {
        Array1::from_vec(self.inner.u.clone()).into_pyarray(py)
    }

    /// V-component of wind (knots).
    #[getter]
    fn v<'py>(&self, py: Python<'py>) -> Bound<'py, PyArray1<f64>> {
        Array1::from_vec(self.inner.v.clone()).into_pyarray(py)
    }

    /// Potential temperature array (K).
    #[getter]
    fn theta<'py>(&self, py: Python<'py>) -> Bound<'py, PyArray1<f64>> {
        Array1::from_vec(self.inner.theta.clone()).into_pyarray(py)
    }

    /// Equivalent potential temperature array (K).
    #[getter]
    fn thetae<'py>(&self, py: Python<'py>) -> Bound<'py, PyArray1<f64>> {
        Array1::from_vec(self.inner.thetae.clone()).into_pyarray(py)
    }

    /// Mixing ratio array (g/kg).
    #[getter]
    fn wvmr<'py>(&self, py: Python<'py>) -> Bound<'py, PyArray1<f64>> {
        Array1::from_vec(self.inner.wvmr.clone()).into_pyarray(py)
    }

    /// Relative humidity array (%).
    #[getter]
    fn relh<'py>(&self, py: Python<'py>) -> Bound<'py, PyArray1<f64>> {
        Array1::from_vec(self.inner.relh.clone()).into_pyarray(py)
    }

    /// Wetbulb temperature array (°C).
    #[getter]
    fn wetbulb<'py>(&self, py: Python<'py>) -> Bound<'py, PyArray1<f64>> {
        Array1::from_vec(self.inner.wetbulb.clone()).into_pyarray(py)
    }

    /// Virtual temperature array (°C).
    #[getter]
    fn vtmp<'py>(&self, py: Python<'py>) -> Bound<'py, PyArray1<f64>> {
        Array1::from_vec(self.inner.vtmp.clone()).into_pyarray(py)
    }

    /// Surface level index.
    #[getter]
    fn sfc(&self) -> usize {
        self.inner.sfc
    }

    /// Top level index.
    #[getter]
    fn top(&self) -> usize {
        self.inner.top
    }

    /// Interpolate temperature (°C) at a given pressure level (hPa).
    fn interp_tmpc(&self, pressure: f64) -> f64 {
        self.inner.interp_tmpc(pressure)
    }

    /// Interpolate dewpoint (°C) at a given pressure level (hPa).
    fn interp_dwpc(&self, pressure: f64) -> f64 {
        self.inner.interp_dwpc(pressure)
    }

    /// Interpolate height (m MSL) at a given pressure level (hPa).
    fn interp_hght(&self, pressure: f64) -> f64 {
        self.inner.interp_hght(pressure)
    }

    /// Surface pressure (hPa).
    fn sfc_pressure(&self) -> f64 {
        self.inner.sfc_pressure()
    }

    /// Surface height (m MSL).
    fn sfc_height(&self) -> f64 {
        self.inner.sfc_height()
    }

    /// Parse a SHARPpy %RAW% text sounding.
    #[staticmethod]
    fn from_sharppy_text(text: &str) -> PyResult<Self> {
        let inner = prof::Profile::from_sharppy_text(text)
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        Ok(Self { inner })
    }

    /// Parse a University of Wyoming upper-air text sounding.
    #[staticmethod]
    fn from_wyoming(text: &str) -> PyResult<Self> {
        let inner =
            prof::Profile::from_wyoming(text).map_err(|e| PyValueError::new_err(e.to_string()))?;
        Ok(Self { inner })
    }

    /// Parse a CSV sounding file.
    #[staticmethod]
    fn from_csv(text: &str) -> PyResult<Self> {
        let inner =
            prof::Profile::from_csv(text).map_err(|e| PyValueError::new_err(e.to_string()))?;
        Ok(Self { inner })
    }

    /// Serialize to SHARPpy %RAW% text format.
    fn to_sharppy_text(&self) -> String {
        self.inner.to_sharppy_text()
    }
}

// ============================================================================
// Internal parcel-lifting helpers for CAPE/CIN
// (self-contained since params/cape.rs is a placeholder)
// ============================================================================

/// LCL pressure (hPa) and temperature (°C) via Bolton (1980).
fn calc_lcl(pres: f64, tmpc: f64, dwpc: f64) -> (f64, f64) {
    let tk = tmpc + ZEROCNK;
    let dk = dwpc + ZEROCNK;
    let tlcl = 56.0 + 1.0 / (1.0 / (dk - 56.0) + (tk / dk).ln() / 800.0);
    let plcl = pres * (tlcl / tk).powf(1.0 / ROCP);
    (plcl, tlcl - ZEROCNK)
}

/// Single moist-adiabatic lapse step: T (°C) at p, return T at p + dp (hPa).
fn moist_lapse_step(p: f64, t: f64, dp: f64) -> f64 {
    let tk = t + ZEROCNK;
    let es = prof::sat_vapor_pressure(t);
    let rs = EPSILON * es / (p - es); // kg/kg
    let numer = (RD * tk + LV * rs) / (p * 100.0);
    let denom = CP + (LV * LV * rs * EPSILON) / (RD * tk * tk);
    let dtdp = numer / denom;
    t + dtdp * dp * 100.0
}

/// Lift parcel moist-adiabatically from (start_p, start_t) to end_p.
fn lift_parcel_moist(start_p: f64, start_t: f64, end_p: f64) -> f64 {
    let mut p = start_p;
    let mut t = start_t;
    let dp = -5.0; // hPa step upward
    while p + dp > end_p {
        t = moist_lapse_step(p, t, dp);
        p += dp;
    }
    let rem = end_p - p;
    if rem.abs() > 0.01 {
        t = moist_lapse_step(p, t, rem);
    }
    t
}

/// CAPE/CIN computation for a surface-based parcel.
fn compute_cape_cin(p: &prof::Profile) -> (f64, f64) {
    let sfc = p.sfc;
    let sfc_p = p.pres[sfc];
    let sfc_t = p.tmpc[sfc];
    let sfc_d = p.dwpc[sfc];

    if !sfc_p.is_finite() || !sfc_t.is_finite() || !sfc_d.is_finite() {
        return (0.0, 0.0);
    }

    let (lcl_p, _) = calc_lcl(sfc_p, sfc_t, sfc_d);
    let sfc_theta = prof::theta(sfc_p, sfc_t); // returns °C
    let sfc_mr = prof::mixratio(sfc_p, sfc_d); // g/kg

    let mut cape = 0.0_f64;
    let mut cin = 0.0_f64;

    for i in sfc..p.pres.len() - 1 {
        let p_bot = p.pres[i];
        let p_top = p.pres[i + 1];
        if !p_bot.is_finite() || !p_top.is_finite() {
            continue;
        }
        let p_mid = (p_bot + p_top) / 2.0;

        // Environment
        let env_t = p.interp_tmpc(p_mid);
        let env_d = p.interp_dwpc(p_mid);
        if !env_t.is_finite() {
            continue;
        }
        let _env_mr = if env_d.is_finite() {
            prof::mixratio(p_mid, env_d)
        } else {
            0.0
        };
        let env_tv = prof::virtemp(p_mid, env_t, if env_d.is_finite() { env_d } else { env_t });

        // Parcel
        let parcel_t = if p_mid >= lcl_p {
            // Below LCL: dry adiabatic
            // theta(sfc) at p_mid: T = (theta_K) * (p/1000)^ROCP - 273.15
            let theta_k = prof::ctok(sfc_theta);
            prof::ktoc(theta_k * (p_mid / 1000.0).powf(ROCP))
        } else {
            // Above LCL: moist adiabatic
            let theta_k = prof::ctok(sfc_theta);
            let lcl_t_actual = prof::ktoc(theta_k * (lcl_p / 1000.0).powf(ROCP));
            lift_parcel_moist(lcl_p, lcl_t_actual, p_mid)
        };

        let _parcel_mr = if p_mid >= lcl_p {
            sfc_mr
        } else {
            // Saturated above LCL
            let es = prof::sat_vapor_pressure(parcel_t);
            EPSILON * es / (p_mid - es) * 1000.0
        };
        let parcel_tv = prof::virtemp(p_mid, parcel_t, parcel_t); // saturated: td=t

        if !parcel_tv.is_finite() || !env_tv.is_finite() {
            continue;
        }

        let dz = p.interp_hght(p_top) - p.interp_hght(p_bot);
        if !dz.is_finite() || dz <= 0.0 {
            continue;
        }
        let b = G * (parcel_tv - env_tv) / (env_tv + ZEROCNK) * dz;

        if b > 0.0 {
            cape += b;
        } else {
            cin += b;
        }
    }

    (cape.max(0.0), cin.min(0.0))
}

// ============================================================================
// Internal wind helpers (since winds.rs functions are todo!())
// ============================================================================

/// Mean wind (u, v) in knots over an AGL height layer.
fn calc_mean_wind_uv(p: &prof::Profile, bot_agl: f64, top_agl: f64) -> (f64, f64) {
    let sfc_h = p.hght[p.sfc];
    let bot_h = sfc_h + bot_agl;
    let top_h = sfc_h + top_agl;

    let mut su = 0.0;
    let mut sv = 0.0;
    let mut n = 0.0;
    for i in 0..p.pres.len() {
        let h = p.hght[i];
        if !h.is_finite() || h < bot_h || h > top_h {
            continue;
        }
        if p.u[i].is_finite() && p.v[i].is_finite() {
            su += p.u[i];
            sv += p.v[i];
            n += 1.0;
        }
    }
    if n < 1.0 {
        (0.0, 0.0)
    } else {
        (su / n, sv / n)
    }
}

/// Bunkers storm motion: ((rm_u, rm_v), (lm_u, lm_v), (mean_u, mean_v)) in kts.
fn calc_bunkers(p: &prof::Profile) -> ((f64, f64), (f64, f64), (f64, f64)) {
    let (mu, mv) = calc_mean_wind_uv(p, 0.0, 6000.0);
    let (llu, llv) = calc_mean_wind_uv(p, 0.0, 500.0);
    let (ulu, ulv) = calc_mean_wind_uv(p, 5500.0, 6000.0);

    let shr_u = ulu - llu;
    let shr_v = ulv - llv;
    let shr_mag = (shr_u * shr_u + shr_v * shr_v).sqrt().max(TOL);

    let d = 7.5 * 1.94384; // 7.5 m/s → knots
    let rm_u = mu + d * shr_v / shr_mag;
    let rm_v = mv - d * shr_u / shr_mag;
    let lm_u = mu - d * shr_v / shr_mag;
    let lm_v = mv + d * shr_u / shr_mag;

    ((rm_u, rm_v), (lm_u, lm_v), (mu, mv))
}

/// Storm-relative helicity (m^2/s^2) over AGL height layer.
fn calc_helicity(p: &prof::Profile, bot_agl: f64, top_agl: f64, su: f64, sv: f64) -> f64 {
    let sfc_h = p.hght[p.sfc];
    let bot_h = sfc_h + bot_agl;
    let top_h = sfc_h + top_agl;
    let kts2ms = 0.514444;

    let mut srh = 0.0;
    for i in 0..p.pres.len() - 1 {
        let h0 = p.hght[i];
        let h1 = p.hght[i + 1];
        if !h0.is_finite() || !h1.is_finite() || h1 < bot_h || h0 > top_h {
            continue;
        }
        let u0 = p.u[i];
        let v0 = p.v[i];
        let u1 = p.u[i + 1];
        let v1 = p.v[i + 1];
        if !u0.is_finite() || !v0.is_finite() || !u1.is_finite() || !v1.is_finite() {
            continue;
        }
        // Convert from knots to m/s for SRH
        let sru0 = (u0 - su) * kts2ms;
        let srv0 = (v0 - sv) * kts2ms;
        let sru1 = (u1 - su) * kts2ms;
        let srv1 = (v1 - sv) * kts2ms;

        srh += sru1 * srv0 - sru0 * srv1;
    }
    srh
}

/// Bulk wind shear magnitude (knots) between two AGL heights.
fn calc_bulk_shear(p: &prof::Profile, bot_agl: f64, top_agl: f64) -> f64 {
    let sfc_h = p.hght[p.sfc];
    let bot_h = sfc_h + bot_agl;
    let top_h = sfc_h + top_agl;

    let mut bot_idx = p.sfc;
    let mut top_idx = p.pres.len() - 1;
    for i in 0..p.hght.len() {
        if p.hght[i].is_finite() && p.hght[i] <= bot_h {
            bot_idx = i;
        }
        if p.hght[i].is_finite() && p.hght[i] <= top_h {
            top_idx = i;
        }
    }

    let bu = p.u[bot_idx];
    let bv = p.v[bot_idx];
    let tu = p.u[top_idx];
    let tv = p.v[top_idx];

    if bu.is_finite() && bv.is_finite() && tu.is_finite() && tv.is_finite() {
        ((tu - bu).powi(2) + (tv - bv).powi(2)).sqrt()
    } else {
        0.0
    }
}

// ============================================================================
// params sub-module functions
// ============================================================================

/// Compute CAPE and CIN (J/kg) for a surface-based parcel.
///
/// Returns (CAPE, CIN) as a tuple.
#[pyfunction]
fn cape_cin(prof: &PyProfile) -> PyResult<(f64, f64)> {
    Ok(compute_cape_cin(&prof.inner))
}

/// Significant Tornado Parameter (fixed layer).
///
/// STP = (CAPE/1500) * ((2000-LCL)/1000) * (SRH/150) * (EBWD/12) * ((200+CIN)/150)
#[pyfunction]
fn stp_fixed(prof: &PyProfile) -> PyResult<f64> {
    let p = &prof.inner;
    let (cape, cin) = compute_cape_cin(p);
    if cape < 1.0 {
        return Ok(0.0);
    }

    let sfc_h = p.hght[p.sfc];
    let (lcl_p, _) = calc_lcl(p.pres[p.sfc], p.tmpc[p.sfc], p.dwpc[p.sfc]);
    let lcl_h = p.interp_hght(lcl_p) - sfc_h;

    let (rm, _, _) = calc_bunkers(p);
    let srh = calc_helicity(p, 0.0, 1000.0, rm.0, rm.1);
    let shear_kts = calc_bulk_shear(p, 0.0, 6000.0);
    let shear_ms = shear_kts * 0.514444;

    let lcl_term = ((2000.0 - lcl_h) / 1000.0).clamp(0.0, 1.0);
    let cin_term = ((200.0 + cin) / 150.0).clamp(0.0, 1.0);

    Ok((cape / 1500.0) * lcl_term * (srh / 150.0) * (shear_ms / 12.0).clamp(0.0, 1.5) * cin_term)
}

/// Supercell Composite Parameter.
///
/// SCP = (muCAPE/1000) * (SRH/50) * (EBWD/20)
#[pyfunction]
fn scp(prof: &PyProfile) -> PyResult<f64> {
    let p = &prof.inner;
    let (cape, _) = compute_cape_cin(p);
    if cape < 1.0 {
        return Ok(0.0);
    }
    let (rm, _, _) = calc_bunkers(p);
    let srh = calc_helicity(p, 0.0, 3000.0, rm.0, rm.1);
    let shear_kts = calc_bulk_shear(p, 0.0, 6000.0);
    let shear_ms = shear_kts * 0.514444;

    Ok(((cape / 1000.0) * (srh / 50.0) * (shear_ms / 20.0)).max(0.0))
}

/// Significant Hail Parameter.
///
/// SHIP = muCAPE * MR * LAPSE * -T500 * SHEAR / 44,000,000
#[pyfunction]
fn ship(prof: &PyProfile) -> PyResult<f64> {
    let p = &prof.inner;
    let (cape, _) = compute_cape_cin(p);
    if cape < 1.0 {
        return Ok(0.0);
    }

    let mr = prof::mixratio(p.pres[p.sfc], p.dwpc[p.sfc]);
    let t700 = p.interp_tmpc(700.0);
    let t500 = p.interp_tmpc(500.0);
    let h700 = p.interp_hght(700.0);
    let h500 = p.interp_hght(500.0);

    if !t700.is_finite() || !t500.is_finite() || !h700.is_finite() || !h500.is_finite() {
        return Ok(0.0);
    }

    let lapse = (t700 - t500) / ((h500 - h700) / 1000.0);
    let neg_t500 = (-t500).max(1.0);
    let shear_kts = calc_bulk_shear(p, 0.0, 6000.0);
    let shear_ms = shear_kts * 0.514444;

    Ok((cape * mr * lapse * neg_t500 * shear_ms / 44_000_000.0).max(0.0))
}

/// Effective Inflow Layer (bottom_p, top_p) in hPa.
///
/// Finds the contiguous layer where CAPE >= 100 J/kg and CIN >= -250 J/kg.
#[pyfunction]
fn effective_inflow_layer(prof: &PyProfile) -> PyResult<(f64, f64)> {
    let p = &prof.inner;
    let cape_thresh = 100.0;
    let cin_thresh = -250.0;

    let mut bot = MISSING;
    let mut top = MISSING;

    for i in 0..p.pres.len() {
        let pp = p.pres[i];
        let t = p.tmpc[i];
        let d = p.dwpc[i];
        if !pp.is_finite() || !t.is_finite() || !d.is_finite() {
            continue;
        }

        let (lcl_p, _) = calc_lcl(pp, t, d);
        let theta_c = prof::theta(pp, t);
        let theta_k = prof::ctok(theta_c);
        let _mr = prof::mixratio(pp, d);

        let mut c = 0.0_f64;
        let mut ci = 0.0_f64;
        for j in i..p.pres.len() - 1 {
            let p_bot = p.pres[j];
            let p_top = p.pres[j + 1];
            if !p_bot.is_finite() || !p_top.is_finite() {
                continue;
            }
            let p_mid = (p_bot + p_top) / 2.0;

            let env_t = p.interp_tmpc(p_mid);
            if !env_t.is_finite() {
                continue;
            }

            let parcel_t = if p_mid >= lcl_p {
                prof::ktoc(theta_k * (p_mid / 1000.0).powf(ROCP))
            } else {
                let lcl_t_actual = prof::ktoc(theta_k * (lcl_p / 1000.0).powf(ROCP));
                lift_parcel_moist(lcl_p, lcl_t_actual, p_mid)
            };

            let dz = p.interp_hght(p_top) - p.interp_hght(p_bot);
            if !dz.is_finite() || dz <= 0.0 {
                continue;
            }
            let b = G * (parcel_t - env_t) / (env_t + ZEROCNK) * dz;
            if b > 0.0 {
                c += b;
            } else {
                ci += b;
            }
        }

        if c >= cape_thresh && ci >= cin_thresh {
            if bot == MISSING {
                bot = pp;
            }
            top = pp;
        } else if bot != MISSING {
            break;
        }
    }

    Ok((bot, top))
}

// ============================================================================
// winds sub-module functions
// ============================================================================

/// Bunkers storm motion vectors.
///
/// Returns ((rm_u, rm_v), (lm_u, lm_v), (mean_u, mean_v)) in knots.
#[pyfunction]
fn bunkers_motion(prof: &PyProfile) -> PyResult<((f64, f64), (f64, f64), (f64, f64))> {
    Ok(calc_bunkers(&prof.inner))
}

/// Storm-relative helicity (m^2/s^2).
///
/// Args:
///     prof: Profile
///     bottom: bottom of layer in m AGL (default 0)
///     top: top of layer in m AGL (default 3000)
///     storm_u: storm motion u-component (kts). If None, uses Bunkers RM.
///     storm_v: storm motion v-component (kts). If None, uses Bunkers RM.
#[pyfunction]
#[pyo3(signature = (prof, bottom=0.0, top=3000.0, storm_u=None, storm_v=None))]
fn helicity(
    prof: &PyProfile,
    bottom: f64,
    top: f64,
    storm_u: Option<f64>,
    storm_v: Option<f64>,
) -> PyResult<f64> {
    let (su, sv) = match (storm_u, storm_v) {
        (Some(u), Some(v)) => (u, v),
        _ => {
            let (rm, _, _) = calc_bunkers(&prof.inner);
            (rm.0, rm.1)
        }
    };
    Ok(calc_helicity(&prof.inner, bottom, top, su, sv))
}

/// Mean wind (u, v) in knots over an AGL height layer.
#[pyfunction]
#[pyo3(signature = (prof, bottom=0.0, top=6000.0))]
fn mean_wind(prof: &PyProfile, bottom: f64, top: f64) -> PyResult<(f64, f64)> {
    Ok(calc_mean_wind_uv(&prof.inner, bottom, top))
}

/// Bulk wind shear over an AGL height layer.
///
/// Returns (du, dv, magnitude) in knots.
#[pyfunction]
#[pyo3(signature = (prof, bottom=0.0, top=6000.0))]
fn wind_shear(prof: &PyProfile, bottom: f64, top: f64) -> PyResult<(f64, f64, f64)> {
    let p = &prof.inner;
    let sfc_h = p.hght[p.sfc];
    let bot_h = sfc_h + bottom;
    let top_h = sfc_h + top;

    let mut bot_idx = p.sfc;
    let mut top_idx = p.pres.len() - 1;
    for i in 0..p.hght.len() {
        if p.hght[i].is_finite() && p.hght[i] <= bot_h {
            bot_idx = i;
        }
        if p.hght[i].is_finite() && p.hght[i] <= top_h {
            top_idx = i;
        }
    }

    let bu = p.u[bot_idx];
    let bv = p.v[bot_idx];
    let tu = p.u[top_idx];
    let tv = p.v[top_idx];

    if bu.is_finite() && bv.is_finite() && tu.is_finite() && tv.is_finite() {
        let du = tu - bu;
        let dv = tv - bv;
        let mag = (du * du + dv * dv).sqrt();
        Ok((du, dv, mag))
    } else {
        Ok((0.0, 0.0, 0.0))
    }
}

// ============================================================================
// thermo sub-module functions
// ============================================================================

/// Potential temperature (K).
///
/// Accepts scalars or numpy arrays of pressure (hPa) and temperature (°C).
#[pyfunction]
#[pyo3(name = "theta", signature = (pressure, temperature))]
fn py_theta(
    py: Python<'_>,
    pressure: &Bound<'_, PyAny>,
    temperature: &Bound<'_, PyAny>,
) -> PyResult<PyObject> {
    if let (Ok(p_arr), Ok(t_arr)) = (
        pressure.extract::<PyReadonlyArray1<f64>>(),
        temperature.extract::<PyReadonlyArray1<f64>>(),
    ) {
        let p = p_arr.as_slice()?;
        let t = t_arr.as_slice()?;
        let n = p.len().min(t.len());
        let mut result = Array1::<f64>::zeros(n);
        for i in 0..n {
            result[i] = prof::ctok(prof::theta(p[i], t[i]));
        }
        Ok(result.into_pyarray(py).into_any().unbind())
    } else {
        let p: f64 = pressure.extract()?;
        let t: f64 = temperature.extract()?;
        Ok(prof::ctok(prof::theta(p, t))
            .into_pyobject(py)?
            .into_any()
            .unbind())
    }
}

/// Equivalent potential temperature (K).
///
/// Accepts scalars or numpy arrays.
#[pyfunction]
#[pyo3(name = "thetae", signature = (pressure, temperature, dewpoint))]
fn py_thetae(
    py: Python<'_>,
    pressure: &Bound<'_, PyAny>,
    temperature: &Bound<'_, PyAny>,
    dewpoint: &Bound<'_, PyAny>,
) -> PyResult<PyObject> {
    if let (Ok(p_arr), Ok(t_arr), Ok(d_arr)) = (
        pressure.extract::<PyReadonlyArray1<f64>>(),
        temperature.extract::<PyReadonlyArray1<f64>>(),
        dewpoint.extract::<PyReadonlyArray1<f64>>(),
    ) {
        let p = p_arr.as_slice()?;
        let t = t_arr.as_slice()?;
        let d = d_arr.as_slice()?;
        let n = p.len().min(t.len()).min(d.len());
        let mut result = Array1::<f64>::zeros(n);
        for i in 0..n {
            result[i] = prof::thetae(p[i], t[i], d[i]);
        }
        Ok(result.into_pyarray(py).into_any().unbind())
    } else {
        let p: f64 = pressure.extract()?;
        let t: f64 = temperature.extract()?;
        let d: f64 = dewpoint.extract()?;
        Ok(prof::thetae(p, t, d).into_pyobject(py)?.into_any().unbind())
    }
}

/// Wetbulb temperature (°C).
///
/// Accepts scalars or numpy arrays.
#[pyfunction]
#[pyo3(name = "wetbulb", signature = (pressure, temperature, dewpoint))]
fn py_wetbulb(
    py: Python<'_>,
    pressure: &Bound<'_, PyAny>,
    temperature: &Bound<'_, PyAny>,
    dewpoint: &Bound<'_, PyAny>,
) -> PyResult<PyObject> {
    if let (Ok(p_arr), Ok(t_arr), Ok(d_arr)) = (
        pressure.extract::<PyReadonlyArray1<f64>>(),
        temperature.extract::<PyReadonlyArray1<f64>>(),
        dewpoint.extract::<PyReadonlyArray1<f64>>(),
    ) {
        let p = p_arr.as_slice()?;
        let t = t_arr.as_slice()?;
        let d = d_arr.as_slice()?;
        let n = p.len().min(t.len()).min(d.len());
        let mut result = Array1::<f64>::zeros(n);
        for i in 0..n {
            result[i] = prof::wetbulb(p[i], t[i], d[i]);
        }
        Ok(result.into_pyarray(py).into_any().unbind())
    } else {
        let p: f64 = pressure.extract()?;
        let t: f64 = temperature.extract()?;
        let d: f64 = dewpoint.extract()?;
        Ok(prof::wetbulb(p, t, d)
            .into_pyobject(py)?
            .into_any()
            .unbind())
    }
}

/// Mixing ratio (g/kg) from pressure (hPa) and dewpoint (°C).
///
/// Accepts scalars or numpy arrays.
#[pyfunction]
#[pyo3(name = "mixing_ratio", signature = (pressure, dewpoint))]
fn py_mixing_ratio(
    py: Python<'_>,
    pressure: &Bound<'_, PyAny>,
    dewpoint: &Bound<'_, PyAny>,
) -> PyResult<PyObject> {
    if let (Ok(p_arr), Ok(d_arr)) = (
        pressure.extract::<PyReadonlyArray1<f64>>(),
        dewpoint.extract::<PyReadonlyArray1<f64>>(),
    ) {
        let p = p_arr.as_slice()?;
        let d = d_arr.as_slice()?;
        let n = p.len().min(d.len());
        let mut result = Array1::<f64>::zeros(n);
        for i in 0..n {
            result[i] = prof::mixratio(p[i], d[i]);
        }
        Ok(result.into_pyarray(py).into_any().unbind())
    } else {
        let p: f64 = pressure.extract()?;
        let d: f64 = dewpoint.extract()?;
        Ok(prof::mixratio(p, d).into_pyobject(py)?.into_any().unbind())
    }
}

// ============================================================================
// fire sub-module functions
// ============================================================================

/// Haines Index (Low / Mid / High regime).
///
/// Auto-detects elevation regime from the profile's surface height, or
/// accepts an explicit regime (0=Low, 1=Mid, 2=High).
#[pyfunction]
#[pyo3(signature = (prof, regime=None))]
fn haines_index(prof: &PyProfile, regime: Option<u8>) -> PyResult<PyObject> {
    let p = &prof.inner;
    let sfc_h = p.hght[p.sfc];

    let elev = match regime {
        Some(0) => crate::fire::HainesElevation::Low,
        Some(1) => crate::fire::HainesElevation::Mid,
        Some(2) | Some(_) => crate::fire::HainesElevation::High,
        None => crate::fire::haines_height(sfc_h),
    };

    // Interpolate required temperatures
    let t950 = p.interp_tmpc(950.0);
    let t850 = p.interp_tmpc(850.0);
    let t700 = p.interp_tmpc(700.0);
    let t500 = p.interp_tmpc(500.0);
    let td850 = p.interp_dwpc(850.0);
    let td700 = p.interp_dwpc(700.0);

    let result = match elev {
        crate::fire::HainesElevation::Low => crate::fire::haines_low(t950, t850, td850),
        crate::fire::HainesElevation::Mid => crate::fire::haines_mid(t850, t700, td850),
        crate::fire::HainesElevation::High => crate::fire::haines_high(t700, t500, td700),
    };

    Python::with_gil(|py| match result {
        Some(v) => Ok(v.into_pyobject(py)?.into_any().unbind()),
        None => Ok(py.None()),
    })
}

/// Fosberg Fire Weather Index.
///
/// Args:
///     tmpc: surface temperature (°C)
///     dwpc: surface dewpoint (°C)
///     wspd_kts: surface wind speed (knots)
#[pyfunction]
fn fosberg_fwi(tmpc: f64, dwpc: f64, wspd_kts: f64) -> PyResult<f64> {
    Ok(crate::fire::fosberg(tmpc, dwpc, wspd_kts))
}

/// Hot-Dry-Windy Index (HDW).
///
/// HDW = max(VPD * wind_speed) in the lowest 500m.
/// VPD = es(T) - e(Td) in hPa; wind in knots.
/// Returns HDW in hPa-kts.
#[pyfunction]
fn hot_dry_windy(prof: &PyProfile) -> PyResult<f64> {
    let p = &prof.inner;
    let sfc_h = p.hght[p.sfc];
    let max_h = sfc_h + 500.0;
    let mut hdw_max = 0.0_f64;

    for i in 0..p.pres.len() {
        if !p.hght[i].is_finite() || p.hght[i] > max_h {
            if p.hght[i].is_finite() {
                break;
            }
            continue;
        }
        if !p.tmpc[i].is_finite() || !p.dwpc[i].is_finite() || !p.wspd[i].is_finite() {
            continue;
        }
        let vpd = prof::sat_vapor_pressure(p.tmpc[i]) - prof::sat_vapor_pressure(p.dwpc[i]);
        let hdw = vpd.max(0.0) * p.wspd[i];
        hdw_max = hdw_max.max(hdw);
    }

    Ok(hdw_max)
}

// ============================================================================
// Module registration
// ============================================================================

fn register_params(parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let m = PyModule::new(parent.py(), "params")?;
    m.add_function(wrap_pyfunction!(cape_cin, &m)?)?;
    m.add_function(wrap_pyfunction!(stp_fixed, &m)?)?;
    m.add_function(wrap_pyfunction!(scp, &m)?)?;
    m.add_function(wrap_pyfunction!(ship, &m)?)?;
    m.add_function(wrap_pyfunction!(effective_inflow_layer, &m)?)?;
    parent.add_submodule(&m)?;
    parent
        .py()
        .import("sys")?
        .getattr("modules")?
        .set_item("sharprs.params", &m)?;
    Ok(())
}

fn register_thermo(parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let m = PyModule::new(parent.py(), "thermo")?;
    m.add_function(wrap_pyfunction!(py_theta, &m)?)?;
    m.add_function(wrap_pyfunction!(py_thetae, &m)?)?;
    m.add_function(wrap_pyfunction!(py_wetbulb, &m)?)?;
    m.add_function(wrap_pyfunction!(py_mixing_ratio, &m)?)?;
    parent.add_submodule(&m)?;
    parent
        .py()
        .import("sys")?
        .getattr("modules")?
        .set_item("sharprs.thermo", &m)?;
    Ok(())
}

fn register_winds(parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let m = PyModule::new(parent.py(), "winds")?;
    m.add_function(wrap_pyfunction!(bunkers_motion, &m)?)?;
    m.add_function(wrap_pyfunction!(helicity, &m)?)?;
    m.add_function(wrap_pyfunction!(mean_wind, &m)?)?;
    m.add_function(wrap_pyfunction!(wind_shear, &m)?)?;
    parent.add_submodule(&m)?;
    parent
        .py()
        .import("sys")?
        .getattr("modules")?
        .set_item("sharprs.winds", &m)?;
    Ok(())
}

fn register_fire(parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let m = PyModule::new(parent.py(), "fire")?;
    m.add_function(wrap_pyfunction!(haines_index, &m)?)?;
    m.add_function(wrap_pyfunction!(fosberg_fwi, &m)?)?;
    m.add_function(wrap_pyfunction!(hot_dry_windy, &m)?)?;
    parent.add_submodule(&m)?;
    parent
        .py()
        .import("sys")?
        .getattr("modules")?
        .set_item("sharprs.fire", &m)?;
    Ok(())
}

/// Root Python module: `import sharprs`
#[pymodule]
fn sharprs(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyProfile>()?;
    register_params(m)?;
    register_thermo(m)?;
    register_winds(m)?;
    register_fire(m)?;
    Ok(())
}
