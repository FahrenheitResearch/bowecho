//! CAPE, CIN, and parcel lifting routines.
//!
//! Rust port of the parcel-lifting core of `sharppy/sharptab/params.py`.
//! This implements the full parcel trace algorithm (`parcelx`), the
//! stripped-down fast CAPE/CIN calculator (`cape`), plus all supporting
//! functions: LCL, LFC, EL, DCAPE, effective inflow layer, and buoyancy.
//!
//! The integration uses the same trapezoidal rule in height-space that
//! SHARPpy uses (with virtual temperature corrections throughout).

use crate::constants::{EPSILON, G, ROCP, ZEROCNK};

// =========================================================================
// Thermo primitives (self-contained so this module has no thermo dep yet)
// =========================================================================

/// Celsius to Kelvin.
#[inline]
fn ctok(tc: f64) -> f64 {
    tc + ZEROCNK
}

/// Kelvin to Celsius.
#[inline]
#[allow(dead_code)]
fn ktoc(tk: f64) -> f64 {
    tk - ZEROCNK
}

/// Potential temperature (C).
///
/// `theta(p, t, p2)` returns the potential temperature of air at pressure
/// `p` (hPa) and temperature `t` (C) referenced to `p2` (hPa, default 1000).
#[inline]
fn theta(p: f64, t: f64, p2: f64) -> f64 {
    (t + ZEROCNK) * (p2 / p).powf(ROCP) - ZEROCNK
}

/// Saturation vapour pressure (hPa) via SHARPpy polynomial approximation.
#[inline]
fn vappres(t: f64) -> f64 {
    let pol = t * (1.1112018e-17 + t * (-3.0994571e-20));
    let pol = t * (2.1874425e-13 + t * (-1.789232e-15 + pol));
    let pol = t * (4.3884180e-09 + t * (-2.988388e-11 + pol));
    let pol = t * (7.8736169e-05 + t * (-6.111796e-07 + pol));
    let pol = 0.99999683 + t * (-9.082695e-03 + pol);
    6.1078 / pol.powi(8)
}

/// Mixing ratio (g/kg) from pressure (hPa) and temperature/dewpoint (C).
#[inline]
fn mixratio(p: f64, t: f64) -> f64 {
    let x = 0.02 * (t - 12.5 + 7500.0 / p);
    let wfw = 1.0 + 0.0000045 * p + 0.0014 * x * x;
    let fwesw = wfw * vappres(t);
    621.97 * (fwesw / (p - fwesw))
}

/// Temperature (C) at a given mixing ratio (g/kg) and pressure (hPa).
fn temp_at_mixrat(w: f64, p: f64) -> f64 {
    const C1: f64 = 0.0498646455;
    const C2: f64 = 2.4082965;
    const C3: f64 = 7.07475;
    const C4: f64 = 38.9114;
    const C5: f64 = 0.0915;
    const C6: f64 = 1.2035;
    let x = (w * p / (622.0 + w)).log10();
    (10.0_f64.powf(C1 * x + C2) - C3 + C4 * (10.0_f64.powf(C5 * x) - C6).powi(2)) - ZEROCNK
}

/// Virtual temperature (C).
///
/// Given pressure (hPa), temperature (C), and dewpoint/moisture-temperature (C),
/// returns the virtual temperature.  If the mixing ratio would be invalid the
/// dry temperature is returned unchanged (matching SHARPpy behaviour).
#[inline]
fn virtemp(p: f64, t: f64, td: f64) -> f64 {
    let tk = t + ZEROCNK;
    let w = 0.001 * mixratio(p, td);
    if w < 0.0 || !w.is_finite() {
        return t;
    }
    (tk * (1.0 + w / EPSILON) / (1.0 + w)) - ZEROCNK
}

/// Wobus function for moist-adiabat computation.
fn wobf(t: f64) -> f64 {
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
                    + t * (3.9401551e-11 + t * (-1.2588129e-13 + t * 1.6688280e-16))));
        let ppol = 1.0 + t * (3.6182989e-03 + t * (-1.3603273e-05 + ppol));
        29.93 / ppol.powi(4) + 0.96 * t - 14.8
    }
}

/// Saturated lift: returns temperature (C) of a saturated parcel at pressure
/// `p` (hPa) given its equivalent potential temperature `thetam` (C).
///
/// Uses the Wobus iterative convergence scheme with convergence tolerance
/// `conv` (default 0.1 C, matching SHARPpy).
fn satlift(p: f64, thetam: f64, conv: f64) -> f64 {
    if (p - 1000.0).abs() <= 0.001 {
        return thetam;
    }
    let pwrp = (p / 1000.0).powf(ROCP);
    let mut t1 = (thetam + ZEROCNK) * pwrp - ZEROCNK;
    let mut e1 = wobf(t1) - wobf(thetam);
    let mut rate = 1.0_f64;
    let mut eor = 999.0_f64;
    // Limit iterations to prevent infinite loops on pathological input
    let mut iters = 0;
    while eor.abs() > conv && iters < 100 {
        let t2 = t1 - e1 * rate;
        let e2_base = (t2 + ZEROCNK) / pwrp - ZEROCNK;
        let e2 = e2_base + wobf(t2) - wobf(e2_base) - thetam;
        eor = e2 * rate;
        rate = (t2 - t1) / (e2 - e1);
        if !rate.is_finite() {
            break;
        }
        t1 = t2;
        e1 = e2;
        iters += 1;
    }
    t1 - eor
}

/// Moist-adiabatic lift: lifts a parcel from `p` at temperature `t` (C) to
/// `p2` (hPa) along a pseudoadiabat, returning the new temperature (C).
fn wetlift(p: f64, t: f64, p2: f64) -> f64 {
    let thta = theta(p, t, 1000.0);
    let thetam = thta - wobf(thta) + wobf(t);
    satlift(p2, thetam, 0.1)
}

/// LCL temperature (C) from SHARPpy's polynomial approximation.
fn lcltemp(t: f64, td: f64) -> f64 {
    let s = t - td;
    let dlt = s * (1.2185 + 0.001278 * t + s * (-0.00219 + 1.173e-5 * s - 0.0000052 * t));
    t - dlt
}

/// Returns the pressure (hPa) at which air with the given potential
/// temperature `th` (C) has temperature `t` (C).
fn thalvl(th: f64, t: f64) -> f64 {
    let t_k = t + ZEROCNK;
    let th_k = th + ZEROCNK;
    1000.0 / (th_k / t_k).powf(1.0 / ROCP)
}

/// Dry lift to LCL: returns `(lcl_pressure, lcl_temperature)` in (hPa, C).
fn drylift(p: f64, t: f64, td: f64) -> (f64, f64) {
    let t2 = lcltemp(t, td);
    let p2 = thalvl(theta(p, t, 1000.0), t2);
    (p2, t2)
}

/// Wetbulb temperature (C) from pressure (hPa), temperature (C), and
/// dewpoint (C).
fn wetbulb(p: f64, t: f64, td: f64) -> f64 {
    let (p2, t2) = drylift(p, t, td);
    wetlift(p2, t2, p)
}

/// Equivalent potential temperature (C).
fn thetae(p: f64, t: f64, td: f64) -> f64 {
    let (p2, t2) = drylift(p, t, td);
    theta(100.0, wetlift(p2, t2, 100.0), 1000.0)
}

// =========================================================================
// Profile abstraction
// =========================================================================

/// A vertical sounding profile, analogous to SHARPpy's `Profile` object.
///
/// All arrays are ordered from the surface (index 0) upward to the top of
/// the sounding.  Missing values are represented as `f64::NAN`.
///
/// Heights are in metres **MSL**.  The caller must populate `sfc_index`
/// with the index of the surface observation (usually 0).
#[derive(Debug, Clone)]
pub struct Profile {
    /// Pressure levels (hPa), surface first.
    pub pres: Vec<f64>,
    /// Heights (m MSL).
    pub hght: Vec<f64>,
    /// Temperature (C).
    pub tmpc: Vec<f64>,
    /// Dewpoint (C).
    pub dwpc: Vec<f64>,
    /// Virtual temperature (C) — precomputed from tmpc and dwpc.
    pub vtmp: Vec<f64>,
    /// Theta-e (C) — precomputed.
    pub thetae: Vec<f64>,
    /// Wetbulb temperature (C) — precomputed.
    pub wetbulb: Vec<f64>,
    /// Index of the surface level in the arrays.
    pub sfc: usize,
    /// Index of the topmost valid level.
    pub top: usize,
}

impl Profile {
    /// Build a profile from raw observations.
    ///
    /// `pres`, `hght`, `tmpc`, `dwpc` must all have the same length and be
    /// sorted from surface upward (highest pressure first).  `sfc` is
    /// normally 0.  NAN values in tmpc/dwpc mark missing levels.
    pub fn new(pres: Vec<f64>, hght: Vec<f64>, tmpc: Vec<f64>, dwpc: Vec<f64>, sfc: usize) -> Self {
        let n = pres.len();
        assert_eq!(n, hght.len());
        assert_eq!(n, tmpc.len());
        assert_eq!(n, dwpc.len());

        let vtmp: Vec<f64> = (0..n)
            .map(|i| {
                if tmpc[i].is_nan() || dwpc[i].is_nan() {
                    f64::NAN
                } else {
                    virtemp(pres[i], tmpc[i], dwpc[i])
                }
            })
            .collect();

        let theta_e: Vec<f64> = (0..n)
            .map(|i| {
                if tmpc[i].is_nan() || dwpc[i].is_nan() {
                    f64::NAN
                } else {
                    thetae(pres[i], tmpc[i], dwpc[i])
                }
            })
            .collect();

        let wb: Vec<f64> = (0..n)
            .map(|i| {
                if tmpc[i].is_nan() || dwpc[i].is_nan() {
                    f64::NAN
                } else {
                    wetbulb(pres[i], tmpc[i], dwpc[i])
                }
            })
            .collect();

        let top = if n > 0 { n - 1 } else { 0 };

        Profile {
            pres,
            hght,
            tmpc,
            dwpc,
            vtmp,
            thetae: theta_e,
            wetbulb: wb,
            sfc,
            top,
        }
    }

    /// Number of levels.
    #[inline]
    pub fn nlevels(&self) -> usize {
        self.pres.len()
    }
}

// =========================================================================
// Interpolation helpers (linear in log-pressure, matching SHARPpy)
// =========================================================================

/// Interpolate a value at pressure `p` from parallel `pres` and `field`
/// arrays.  Uses linear interpolation in ln(p).  Returns NAN if `p` is
/// outside the range or the surrounding values are NAN.
fn interp_pres(p: f64, pres: &[f64], field: &[f64]) -> f64 {
    let n = pres.len();
    if n < 2 || p.is_nan() {
        return f64::NAN;
    }
    // Pressures are descending (surface = largest)
    if p > pres[0] || p < pres[n - 1] {
        return f64::NAN;
    }
    let lnp = p.ln();
    for i in 0..n - 1 {
        if pres[i].is_nan() || pres[i + 1].is_nan() {
            continue;
        }
        if p <= pres[i] && p >= pres[i + 1] {
            let f0 = field[i];
            let f1 = field[i + 1];
            if f0.is_nan() || f1.is_nan() {
                return f64::NAN;
            }
            let lnp0 = pres[i].ln();
            let lnp1 = pres[i + 1].ln();
            let frac = (lnp - lnp0) / (lnp1 - lnp0);
            return f0 + (f1 - f0) * frac;
        }
    }
    f64::NAN
}

/// Interpolate temperature at pressure level.
#[inline]
fn interp_temp(prof: &Profile, p: f64) -> f64 {
    interp_pres(p, &prof.pres, &prof.tmpc)
}

/// Interpolate dewpoint at pressure level.
#[inline]
fn interp_dwpt(prof: &Profile, p: f64) -> f64 {
    interp_pres(p, &prof.pres, &prof.dwpc)
}

/// Interpolate virtual temperature at pressure level.
#[inline]
fn interp_vtmp(prof: &Profile, p: f64) -> f64 {
    interp_pres(p, &prof.pres, &prof.vtmp)
}

/// Public wrapper for virtual temperature interpolation (used by renderers).
#[inline]
pub fn interp_vtmp_pub(prof: &Profile, p: f64) -> f64 {
    interp_vtmp(prof, p)
}

/// Interpolate height at pressure level.
#[inline]
fn interp_hght(prof: &Profile, p: f64) -> f64 {
    interp_pres(p, &prof.pres, &prof.hght)
}

/// Interpolate pressure at height `h` (m MSL).
fn interp_pres_from_hght(prof: &Profile, h: f64) -> f64 {
    let n = prof.hght.len();
    if n < 2 || h.is_nan() {
        return f64::NAN;
    }
    // Heights are ascending
    for i in 0..n - 1 {
        let h0 = prof.hght[i];
        let h1 = prof.hght[i + 1];
        if h0.is_nan() || h1.is_nan() {
            continue;
        }
        if h >= h0 && h <= h1 {
            let frac = (h - h0) / (h1 - h0);
            let lnp0 = prof.pres[i].ln();
            let lnp1 = prof.pres[i + 1].ln();
            return (lnp0 + (lnp1 - lnp0) * frac).exp();
        }
    }
    f64::NAN
}

/// Convert MSL height to AGL height.
#[inline]
fn to_agl(prof: &Profile, h_msl: f64) -> f64 {
    h_msl - prof.hght[prof.sfc]
}

/// Convert AGL height to MSL height.
#[inline]
fn to_msl(prof: &Profile, h_agl: f64) -> f64 {
    h_agl + prof.hght[prof.sfc]
}

/// Find the pressure where the environment reaches a given temperature.
fn temp_lvl(prof: &Profile, temp: f64) -> f64 {
    for i in 0..prof.nlevels() - 1 {
        let t0 = prof.tmpc[i];
        let t1 = prof.tmpc[i + 1];
        if t0.is_nan() || t1.is_nan() {
            continue;
        }
        if (t0 >= temp && t1 <= temp) || (t0 <= temp && t1 >= temp) {
            if (t1 - t0).abs() < 1e-12 {
                return prof.pres[i];
            }
            let frac = (temp - t0) / (t1 - t0);
            let lnp0 = prof.pres[i].ln();
            let lnp1 = prof.pres[i + 1].ln();
            return (lnp0 + (lnp1 - lnp0) * frac).exp();
        }
    }
    f64::NAN
}

// =========================================================================
// Mean layer helpers
// =========================================================================

/// Mean mixing ratio (g/kg) in the layer from `pbot` to `ptop` (hPa).
fn mean_mixratio(prof: &Profile, pbot: f64, ptop: f64) -> f64 {
    let dp = -1.0;
    let mut p = pbot;
    let mut sum = 0.0;
    let mut count = 0.0;
    while p >= ptop {
        let t = interp_dwpt(prof, p);
        if !t.is_nan() {
            sum += mixratio(p, t);
            count += 1.0;
        }
        p += dp;
    }
    if count > 0.0 {
        sum / count
    } else {
        f64::NAN
    }
}

/// Mean potential temperature (C) in the layer from `pbot` to `ptop` (hPa).
#[allow(dead_code)]
fn mean_theta(prof: &Profile, pbot: f64, ptop: f64) -> f64 {
    let dp = -1.0;
    let mut p = pbot;
    let mut sum = 0.0;
    let mut count = 0.0;
    while p >= ptop {
        let t = interp_temp(prof, p);
        if !t.is_nan() {
            sum += theta(p, t, 1000.0);
            count += 1.0;
        }
        p += dp;
    }
    if count > 0.0 {
        sum / count
    } else {
        f64::NAN
    }
}

/// SHARPpy-style "exact" mean mixing ratio (g/kg).
///
/// This mirrors `mean_mixratio(..., exact=True)`: interpolate endpoint
/// pressure/dewpoint values, duplicate native levels inside the layer, then
/// convert the averaged pressure/dewpoint back to a mixing ratio.
fn mean_mixratio_exact(prof: &Profile, pbot: f64, ptop: f64) -> f64 {
    let td_bot = interp_dwpt(prof, pbot);
    let td_top = interp_dwpt(prof, ptop);
    if !td_bot.is_finite() || !td_top.is_finite() || !pbot.is_finite() || !ptop.is_finite() {
        return f64::NAN;
    }

    let mut p_sum = pbot + ptop;
    let mut td_sum = td_bot + td_top;
    let mut weight = 2.0;

    for i in 0..prof.nlevels() {
        let p = prof.pres[i];
        let td = prof.dwpc[i];
        if p.is_finite() && td.is_finite() && p < pbot && p > ptop {
            p_sum += 2.0 * p;
            td_sum += 2.0 * td;
            weight += 2.0;
        }
    }

    mixratio(p_sum / weight, td_sum / weight)
}

/// SHARPpy-style "exact" mean potential temperature (C).
///
/// Endpoint values are interpolated, while native levels inside the layer are
/// duplicated before averaging, matching SHARPpy's exact mixed-layer parcel.
fn mean_theta_exact(prof: &Profile, pbot: f64, ptop: f64) -> f64 {
    let t_bot = interp_temp(prof, pbot);
    let t_top = interp_temp(prof, ptop);
    if !t_bot.is_finite() || !t_top.is_finite() || !pbot.is_finite() || !ptop.is_finite() {
        return f64::NAN;
    }

    let mut sum = theta(pbot, t_bot, 1000.0) + theta(ptop, t_top, 1000.0);
    let mut weight = 2.0;

    for i in 0..prof.nlevels() {
        let p = prof.pres[i];
        let t = prof.tmpc[i];
        if p.is_finite() && t.is_finite() && p < pbot && p > ptop {
            sum += 2.0 * theta(p, t, 1000.0);
            weight += 2.0;
        }
    }

    sum / weight
}

/// Mean theta-e (C) in the layer from `pbot` to `ptop` (hPa).
fn mean_thetae(prof: &Profile, pbot: f64, ptop: f64) -> f64 {
    let dp = -1.0;
    let mut p = pbot;
    let mut sum = 0.0;
    let mut count = 0.0;
    while p >= ptop {
        let t = interp_temp(prof, p);
        let td = interp_dwpt(prof, p);
        if !t.is_nan() && !td.is_nan() {
            sum += thetae(p, t, td);
            count += 1.0;
        }
        p += dp;
    }
    if count > 0.0 {
        sum / count
    } else {
        f64::NAN
    }
}

// =========================================================================
// Parcel types
// =========================================================================

/// Parcel type flag, corresponding to SHARPpy's `DefineParcel` flag values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ParcelType {
    /// flag=1: Observed surface parcel.
    Surface,
    /// flag=2: Forecast surface parcel (mixed BL temperature, mean BL moisture).
    Forecast { depth_hpa: f64 },
    /// flag=3: Most-unstable parcel in the lowest `depth_hpa`.
    MostUnstable { depth_hpa: f64 },
    /// flag=4: Mixed-layer parcel (mean theta/mixratio in lowest `depth_hpa`).
    MixedLayer { depth_hpa: f64 },
    /// flag=5: User-defined parcel.
    UserDefined { pres: f64, tmpc: f64, dwpc: f64 },
}

/// Lifted Parcel Layer values — the starting conditions for a lifted parcel.
#[derive(Debug, Clone, Copy)]
pub struct LiftedParcelLevel {
    pub pres: f64,
    pub tmpc: f64,
    pub dwpc: f64,
    pub parcel_type: ParcelType,
}

/// Define a lifted parcel from a profile and a parcel type specification.
///
/// This is the Rust equivalent of SHARPpy's `DefineParcel` class.
pub fn define_parcel(prof: &Profile, ptype: ParcelType) -> LiftedParcelLevel {
    match ptype {
        ParcelType::Surface => LiftedParcelLevel {
            pres: prof.pres[prof.sfc],
            tmpc: prof.tmpc[prof.sfc],
            dwpc: prof.dwpc[prof.sfc],
            parcel_type: ptype,
        },
        ParcelType::Forecast { depth_hpa } => {
            let pbot = prof.pres[prof.sfc];
            let ptop = pbot - depth_hpa;
            let mmr = mean_mixratio(prof, pbot, ptop);
            let pres = prof.pres[prof.sfc];
            let tmpc = prof.tmpc[prof.sfc];
            let dwpc = temp_at_mixrat(mmr, pres);
            LiftedParcelLevel {
                pres,
                tmpc,
                dwpc,
                parcel_type: ptype,
            }
        }
        ParcelType::MostUnstable { depth_hpa } => {
            let pbot = prof.pres[prof.sfc];
            let ptop = pbot - depth_hpa;
            let mu_pres = most_unstable_level(prof, pbot, ptop);
            let tmpc = interp_temp(prof, mu_pres);
            let dwpc = interp_dwpt(prof, mu_pres);
            LiftedParcelLevel {
                pres: mu_pres,
                tmpc,
                dwpc,
                parcel_type: ptype,
            }
        }
        ParcelType::MixedLayer { depth_hpa } => {
            let pbot = prof.pres[prof.sfc];
            let ptop = pbot - depth_hpa;
            let mtheta = mean_theta_exact(prof, pbot, ptop);
            let pres = pbot;
            let tmpc = theta(1000.0, mtheta, pres);
            let mmr = mean_mixratio_exact(prof, pbot, ptop);
            let dwpc = temp_at_mixrat(mmr, pres);
            LiftedParcelLevel {
                pres,
                tmpc,
                dwpc,
                parcel_type: ptype,
            }
        }
        ParcelType::UserDefined { pres, tmpc, dwpc } => LiftedParcelLevel {
            pres,
            tmpc,
            dwpc,
            parcel_type: ptype,
        },
    }
}

// =========================================================================
// Most-unstable level
// =========================================================================

/// Find the most unstable level (highest theta-e) between `pbot` and `ptop`.
///
/// Searches at 1 hPa increments (matching SHARPpy `dp=-1`).  Returns the
/// pressure (hPa) of the most unstable level.
pub fn most_unstable_level(prof: &Profile, pbot: f64, ptop: f64) -> f64 {
    let dp = -1.0;
    let mut best_pres = pbot;
    let mut best_val = f64::NEG_INFINITY;
    let mut p = pbot;
    while p >= ptop {
        let t = interp_temp(prof, p);
        let td = interp_dwpt(prof, p);
        if !t.is_nan() && !td.is_nan() {
            // SHARPpy computes drylift then wetlift to 1000 hPa to get
            // an effective theta-e ranking.
            let (p2, t2) = drylift(p, t, td);
            let mt = wetlift(p2, t2, 1000.0);
            if mt > best_val {
                best_val = mt;
                best_pres = p;
            }
        }
        p += dp;
    }
    best_pres
}

// =========================================================================
// Parcel result structure
// =========================================================================

/// Full parcel trace result, analogous to SHARPpy's `Parcel` object.
#[derive(Debug, Clone)]
pub struct ParcelResult {
    /// Parcel starting pressure (hPa).
    pub pres: f64,
    /// Parcel starting temperature (C).
    pub tmpc: f64,
    /// Parcel starting dewpoint (C).
    pub dwpc: f64,
    /// CAPE (J/kg) — total positive buoyancy area.
    pub bplus: f64,
    /// CIN (J/kg) — total negative buoyancy below 500 hPa (negative value).
    pub bminus: f64,
    /// LCL pressure (hPa).
    pub lclpres: f64,
    /// LCL height (m AGL).
    pub lclhght: f64,
    /// LFC pressure (hPa).  NAN if none found.
    pub lfcpres: f64,
    /// LFC height (m AGL).  NAN if none found.
    pub lfchght: f64,
    /// EL pressure (hPa).  NAN if none found.
    pub elpres: f64,
    /// EL height (m AGL).  NAN if none found.
    pub elhght: f64,
    /// MPL (Maximum Parcel Level) pressure (hPa).  NAN if none found.
    pub mplpres: f64,
    /// MPL height (m AGL).  NAN if none found.
    pub mplhght: f64,
    /// CAPE up to the freezing level (J/kg).
    pub bfzl: f64,
    /// CAPE up to 3 km AGL (J/kg).
    pub b3km: f64,
    /// CAPE up to 6 km AGL (J/kg).
    pub b6km: f64,
    /// 500 hPa lifted index (C).
    pub li5: f64,
    /// 300 hPa lifted index (C).
    pub li3: f64,
    /// Maximum lifted index (C).
    pub limax: f64,
    /// Pressure at maximum lifted index (hPa).
    pub limaxpres: f64,
    /// Cap strength (C).
    pub cap: f64,
    /// Pressure at cap strength (hPa).
    pub cappres: f64,
    /// Minimum buoyancy below 500 hPa (C).
    pub bmin: f64,
    /// Pressure at minimum buoyancy (hPa).
    pub bminpres: f64,
    /// Temperature levels.
    pub p0c: f64,
    pub pm10c: f64,
    pub pm20c: f64,
    pub pm30c: f64,
    pub hght0c: f64,
    pub hghtm10c: f64,
    pub hghtm20c: f64,
    pub hghtm30c: f64,
    /// Wetbulb-related CAPE values at temperature thresholds.
    pub wm10c: f64,
    pub wm20c: f64,
    pub wm30c: f64,
    /// Parcel trace: pressure values (hPa).
    pub ptrace: Vec<f64>,
    /// Parcel trace: virtual temperature values (C).
    pub ttrace: Vec<f64>,
}

impl Default for ParcelResult {
    fn default() -> Self {
        ParcelResult {
            pres: f64::NAN,
            tmpc: f64::NAN,
            dwpc: f64::NAN,
            bplus: f64::NAN,
            bminus: f64::NAN,
            lclpres: f64::NAN,
            lclhght: f64::NAN,
            lfcpres: f64::NAN,
            lfchght: f64::NAN,
            elpres: f64::NAN,
            elhght: f64::NAN,
            mplpres: f64::NAN,
            mplhght: f64::NAN,
            bfzl: f64::NAN,
            b3km: f64::NAN,
            b6km: f64::NAN,
            li5: f64::NAN,
            li3: f64::NAN,
            limax: f64::NAN,
            limaxpres: f64::NAN,
            cap: f64::NAN,
            cappres: f64::NAN,
            bmin: f64::NAN,
            bminpres: f64::NAN,
            p0c: f64::NAN,
            pm10c: f64::NAN,
            pm20c: f64::NAN,
            pm30c: f64::NAN,
            hght0c: f64::NAN,
            hghtm10c: f64::NAN,
            hghtm20c: f64::NAN,
            hghtm30c: f64::NAN,
            wm10c: f64::NAN,
            wm20c: f64::NAN,
            wm30c: f64::NAN,
            ptrace: Vec::new(),
            ttrace: Vec::new(),
        }
    }
}

// =========================================================================
// LCL — Lifted Condensation Level
// =========================================================================

/// Compute the Lifted Condensation Level for a parcel.
///
/// Returns `(lcl_pressure_hPa, lcl_temperature_C)`.
pub fn lcl(pres: f64, tmpc: f64, dwpc: f64) -> (f64, f64) {
    drylift(pres, tmpc, dwpc)
}

// =========================================================================
// Buoyancy at a single level
// =========================================================================

/// Compute the buoyancy (C) of a lifted parcel at pressure `p` (hPa).
///
/// Buoyancy is defined as `Tv_parcel - Tv_environment` using virtual
/// temperature corrections.  Positive = parcel is warmer (buoyant).
pub fn buoyancy(prof: &Profile, pcl_pres: f64, pcl_tmpc: f64, pcl_dwpc: f64, p: f64) -> f64 {
    let te = interp_vtmp(prof, p);
    if te.is_nan() {
        return f64::NAN;
    }
    // Lift the parcel to p
    let (lcl_p, lcl_t) = drylift(pcl_pres, pcl_tmpc, pcl_dwpc);
    let tp = if p >= lcl_p {
        // Below LCL: dry adiabatic — theta constant, use dry parcel virtmp
        let th = theta(pcl_pres, pcl_tmpc, 1000.0);
        let t_at_p = theta(1000.0, th, p);
        virtemp(p, t_at_p, temp_at_mixrat(mixratio(pcl_pres, pcl_dwpc), p))
    } else {
        // Above LCL: moist adiabatic
        let tp_sat = wetlift(lcl_p, lcl_t, p);
        virtemp(p, tp_sat, tp_sat)
    };
    tp - te
}

// =========================================================================
// cape — fast CAPE/CIN (stripped-down parcelx for iterative use)
// =========================================================================

/// Fast CAPE/CIN calculator.
///
/// This is the Rust equivalent of SHARPpy's `cape()` function — a stripped-
/// down version of `parcelx()` that only computes CAPE and CIN.  It is
/// designed for use in iterative routines (effective_inflow_layer,
/// convective_temp, etc.) where the full parcel trace is not needed.
///
/// Lifts the parcel specified by `lpl` through the profile.  `pbot` and
/// `ptop` optionally restrict the integration layer (defaults: surface to
/// top of sounding).
pub fn cape(
    prof: &Profile,
    lpl: &LiftedParcelLevel,
    pbot: Option<f64>,
    ptop: Option<f64>,
) -> ParcelResult {
    let mut pcl = ParcelResult::default();
    if prof.nlevels() < 2 {
        return pcl;
    }

    let pres = lpl.pres;
    let tmpc = lpl.tmpc;
    let dwpc = lpl.dwpc;
    pcl.pres = pres;
    pcl.tmpc = tmpc;
    pcl.dwpc = dwpc;

    let mut pbot = pbot.unwrap_or(prof.pres[prof.sfc]);
    let ptop = ptop.unwrap_or(prof.pres[prof.top]);

    // If parcel is above pbot, adjust
    if pbot > pres {
        pbot = pres;
    }

    if interp_vtmp(prof, pbot).is_nan() || interp_vtmp(prof, ptop).is_nan() {
        return pcl;
    }

    // --- Sub-LCL (dry adiabatic) CIN integration ---
    let mut totp = 0.0_f64;
    let mut totn = 0.0_f64;

    // Lift parcel dry-adiabatically to LCL
    let (lcl_pres, lcl_temp) = drylift(pres, tmpc, dwpc);
    if lcl_pres.is_nan() || !lcl_pres.is_finite() {
        return pcl;
    }
    let blupper = lcl_pres;

    // Lifted parcel theta (constant from LPL to LCL)
    let theta_parcel = theta(lcl_pres, lcl_temp, 1000.0);
    let blmr = mixratio(pres, dwpc);

    // Accumulate CIN in mixing layer below LCL at 1 hPa increments
    {
        let dp = -1.0;
        let mut p = pbot;
        let mut prev_tdef = f64::NAN;
        let mut prev_h = f64::NAN;
        while p >= blupper + dp {
            let h = interp_hght(prof, p);
            let env_theta = theta(p, interp_temp(prof, p), 1000.0);
            let env_dwpt = interp_dwpt(prof, p);
            let tv_env = if !env_dwpt.is_nan() {
                virtemp(p, env_theta, env_dwpt)
            } else {
                env_theta
            };
            let pcl_t = virtemp(p, theta_parcel, temp_at_mixrat(blmr, p));
            let tdef = (pcl_t - tv_env) / ctok(tv_env);

            if !prev_tdef.is_nan() && !prev_h.is_nan() && !h.is_nan() {
                let lyre = G * (prev_tdef + tdef) / 2.0 * (h - prev_h);
                if lyre < 0.0 {
                    totn += lyre;
                }
            }
            prev_tdef = tdef;
            prev_h = h;
            p += dp;
        }
    }

    // Move bottom to top of BL (LCL) for moist ascent
    if pbot > lcl_pres {
        pbot = lcl_pres;
    }

    if pbot < prof.pres[prof.top] {
        return pcl;
    }

    // Find lowest observation above pbot
    let lptr = match prof.pres.iter().position(|&pp| pp < pbot) {
        Some(i) => i,
        None => return pcl,
    };
    let uptr = prof
        .pres
        .iter()
        .rposition(|&pp| pp > ptop)
        .unwrap_or(prof.top);

    // --- Moist ascent from LCL ---
    let mut pe1 = pbot;
    let mut h1 = interp_hght(prof, pe1);
    let mut te1 = interp_vtmp(prof, pe1);
    let mut tp1 = lcl_temp;
    if (pbot - lcl_pres).abs() > 0.01 {
        tp1 = wetlift(lcl_pres, lcl_temp, pbot);
    }

    for i in lptr..prof.nlevels() {
        if prof.tmpc[i].is_nan() {
            continue;
        }
        let pe2 = prof.pres[i];
        let h2 = prof.hght[i];
        let te2 = prof.vtmp[i];
        let tp2 = wetlift(pe1, tp1, pe2);

        let tdef1 = (virtemp(pe1, tp1, tp1) - te1) / ctok(te1);
        let tdef2 = (virtemp(pe2, tp2, tp2) - te2) / ctok(te2);
        let lyre = G * (tdef1 + tdef2) / 2.0 * (h2 - h1);

        if lyre > 0.0 {
            totp += lyre;
        } else if pe2 > 500.0 {
            totn += lyre;
        }

        pe1 = pe2;
        h1 = h2;
        te1 = te2;
        tp1 = tp2;

        // Top of specified layer
        if i >= uptr && pcl.bplus.is_nan() {
            let pe3 = pe1;
            let h3 = h1;
            let te3 = te1;
            let tp3 = tp1;
            let lyrf = lyre;
            if lyrf > 0.0 {
                pcl.bplus = totp - lyrf;
                pcl.bminus = totn;
            } else {
                pcl.bplus = totp;
                pcl.bminus = if pe2 > 500.0 { totn + lyrf } else { totn };
            }
            // Interpolate to ptop
            let h2f = interp_hght(prof, ptop);
            let te2f = interp_vtmp(prof, ptop);
            let tp2f = wetlift(pe3, tp3, ptop);
            let tdef3 = (virtemp(pe3, tp3, tp3) - te3) / ctok(te3);
            let tdef2f = (virtemp(ptop, tp2f, tp2f) - te2f) / ctok(te2f);
            let lyrf2 = G * (tdef3 + tdef2f) / 2.0 * (h2f - h3);
            if lyrf2 > 0.0 {
                pcl.bplus += lyrf2;
            } else if ptop > 500.0 {
                pcl.bminus += lyrf2;
            }
            if pcl.bplus == 0.0 {
                pcl.bminus = 0.0;
            }
            break;
        }
    }

    if pcl.bplus.is_nan() {
        pcl.bplus = totp;
        pcl.bminus = totn;
    }

    pcl
}

// =========================================================================
// parcelx — full parcel trace with all derived parameters
// =========================================================================

/// Full parcel trace computation.
///
/// This is the Rust equivalent of SHARPpy's `parcelx()` function.  It lifts
/// the specified parcel through the profile, computing CAPE, CIN, LCL, LFC,
/// EL, MPL, lifted indices, cap strength, temperature-level CAPE partitions,
/// and the full parcel trace.
///
/// # Arguments
/// * `prof` — the sounding profile
/// * `lpl` — the lifted parcel level (from `define_parcel`)
/// * `pbot` — optional bottom of integration layer (default: surface)
/// * `ptop` — optional top of integration layer (default: top of sounding)
pub fn parcelx(
    prof: &Profile,
    lpl: &LiftedParcelLevel,
    pbot: Option<f64>,
    ptop: Option<f64>,
) -> ParcelResult {
    let mut pcl = ParcelResult::default();
    if prof.nlevels() < 2 {
        return pcl;
    }

    let pres = lpl.pres;
    let tmpc = lpl.tmpc;
    let dwpc = lpl.dwpc;
    pcl.pres = pres;
    pcl.tmpc = tmpc;
    pcl.dwpc = dwpc;

    let mut pbot = pbot.unwrap_or(prof.pres[prof.sfc]);
    let ptop = ptop.unwrap_or(prof.pres[prof.top]);

    if pbot > pres {
        pbot = pres;
    }

    let mut cap_strength = -9999.0_f64;
    let mut cap_strengthpres = -9999.0_f64;
    let mut li_max = -9999.0_f64;
    let mut li_maxpres = -9999.0_f64;
    let mut totp = 0.0_f64;
    let mut totn = 0.0_f64;
    let mut tote = 0.0_f64;

    // --- Begin with the mixing layer ---
    let pe1_init = pbot;
    let tp1_init = virtemp(pres, tmpc, dwpc);
    pcl.ptrace.push(pe1_init);
    pcl.ttrace.push(tp1_init);

    // Dry lift to LCL
    let (lcl_pres, lcl_temp) = drylift(pres, tmpc, dwpc);
    if lcl_pres.is_nan() || !lcl_pres.is_finite() {
        return pcl;
    }
    let h2_lcl = interp_hght(prof, lcl_pres);
    pcl.lclpres = lcl_pres.min(prof.pres[prof.sfc]);
    pcl.lclhght = to_agl(prof, h2_lcl);
    pcl.ptrace.push(lcl_pres);
    pcl.ttrace.push(virtemp(lcl_pres, lcl_temp, lcl_temp));

    let theta_parcel = theta(lcl_pres, lcl_temp, 1000.0);
    let blmr = mixratio(pres, dwpc);

    // Sub-LCL CIN at 1 hPa increments
    {
        let dp = -1.0;
        let mut p = pbot;
        let mut prev_tdef = f64::NAN;
        let mut prev_h = f64::NAN;
        while p >= lcl_pres + dp {
            let h = interp_hght(prof, p);
            let env_theta = theta(p, interp_temp(prof, p), 1000.0);
            let env_dwpt = interp_dwpt(prof, p);
            let tv_env = if !env_dwpt.is_nan() {
                virtemp(p, env_theta, env_dwpt)
            } else {
                env_theta
            };
            let pcl_t = virtemp(p, theta_parcel, temp_at_mixrat(blmr, p));
            let tdef = (pcl_t - tv_env) / ctok(tv_env);

            if !prev_tdef.is_nan() && !prev_h.is_nan() && !h.is_nan() {
                let lyre = G * (prev_tdef + tdef) / 2.0 * (h - prev_h);
                if lyre < 0.0 {
                    totn += lyre;
                }
            }
            prev_tdef = tdef;
            prev_h = h;
            p += dp;
        }
    }

    // Move bottom to LCL
    if pbot > lcl_pres {
        pbot = lcl_pres;
    }

    // Temperature levels
    let p0c = temp_lvl(prof, 0.0);
    let pm10c = temp_lvl(prof, -10.0);
    let pm20c = temp_lvl(prof, -20.0);
    let pm30c = temp_lvl(prof, -30.0);
    pcl.p0c = p0c;
    pcl.pm10c = pm10c;
    pcl.pm20c = pm20c;
    pcl.pm30c = pm30c;
    pcl.hght0c = interp_hght(prof, p0c);
    pcl.hghtm10c = interp_hght(prof, pm10c);
    pcl.hghtm20c = interp_hght(prof, pm20c);
    pcl.hghtm30c = interp_hght(prof, pm30c);

    if pbot < prof.pres[prof.top] {
        return pcl;
    }

    // Find lowest observation above pbot
    let lptr = match prof.pres.iter().position(|&pp| pp < pbot) {
        Some(i) => i,
        None => return pcl,
    };
    let uptr = prof
        .pres
        .iter()
        .rposition(|&pp| pp > ptop)
        .unwrap_or(prof.top);

    // --- Moist ascent from LCL ---
    let mut pe1 = pbot;
    let mut h1 = interp_hght(prof, pe1);
    let mut te1 = interp_vtmp(prof, pe1);
    let mut tp1 = wetlift(lcl_pres, lcl_temp, pe1);
    let mut lyre: f64 = 0.0;
    let mut lyrlast: f64;

    for i in lptr..prof.nlevels() {
        if prof.tmpc[i].is_nan() {
            continue;
        }
        let pe2 = prof.pres[i];
        let h2 = prof.hght[i];
        let te2 = prof.vtmp[i];
        let tp2 = wetlift(pe1, tp1, pe2);

        let tdef1 = (virtemp(pe1, tp1, tp1) - te1) / ctok(te1);
        let tdef2 = (virtemp(pe2, tp2, tp2) - te2) / ctok(te2);

        pcl.ptrace.push(pe2);
        pcl.ttrace.push(virtemp(pe2, tp2, tp2));

        lyrlast = lyre;
        lyre = G * (tdef1 + tdef2) / 2.0 * (h2 - h1);

        if lyre > 0.0 {
            totp += lyre;
        } else if pe2 > 500.0 {
            totn += lyre;
        }

        // Max lifted index
        let mli = virtemp(pe2, tp2, tp2) - te2;
        if mli > li_max {
            li_max = mli;
            li_maxpres = pe2;
        }

        // Max cap strength
        let mcap = te2 - mli;
        if mcap > cap_strength {
            cap_strength = mcap;
            cap_strengthpres = pe2;
        }

        tote += lyre;
        let pelast = pe1;
        pe1 = pe2;
        te1 = te2;
        tp1 = tp2;

        // --- Top of specified layer ---
        if i >= uptr && pcl.bplus.is_nan() {
            let pe3 = pe1;
            let h3 = h2;
            let te3 = te1;
            let tp3 = tp1;
            let lyrf = lyre;
            if lyrf > 0.0 {
                pcl.bplus = totp - lyrf;
                pcl.bminus = totn;
            } else {
                pcl.bplus = totp;
                pcl.bminus = if pe2 > 500.0 { totn + lyrf } else { totn };
            }
            let h2f = interp_hght(prof, ptop);
            let te2f = interp_vtmp(prof, ptop);
            let tp2f = wetlift(pe3, tp3, ptop);
            let tdef3 = (virtemp(pe3, tp3, tp3) - te3) / ctok(te3);
            let tdef2f = (virtemp(ptop, tp2f, tp2f) - te2f) / ctok(te2f);
            let lyrf2 = G * (tdef3 + tdef2f) / 2.0 * (h2f - h3);
            if lyrf2 > 0.0 {
                pcl.bplus += lyrf2;
            } else if ptop > 500.0 {
                pcl.bminus += lyrf2;
            }
            if pcl.bplus == 0.0 {
                pcl.bminus = 0.0;
            }
        }

        // --- Freezing level CAPE ---
        if te2 < 0.0 && pcl.bfzl.is_nan() {
            let pe3 = pelast;
            let h3 = interp_hght(prof, pe3);
            let te3 = interp_vtmp(prof, pe3);
            let tp3 = wetlift(pe1, tp1, pe3);
            if lyre > 0.0 {
                pcl.bfzl = totp - lyre;
            } else {
                pcl.bfzl = totp;
            }
            if p0c.is_nan() || p0c > pe3 {
                pcl.bfzl = 0.0;
            } else if !pe2.is_nan() {
                let te2f = interp_vtmp(prof, pe2);
                let tp2f = wetlift(pe3, tp3, pe2);
                let tdef3 = (virtemp(pe3, tp3, tp3) - te3) / ctok(te3);
                let tdef2f = (virtemp(pe2, tp2f, tp2f) - te2f) / ctok(te2f);
                let lyrf = G * (tdef3 + tdef2f) / 2.0 * (pcl.hght0c - h3);
                if lyrf > 0.0 {
                    pcl.bfzl += lyrf;
                }
            }
        }

        // --- -10C level CAPE ---
        if te2 < -10.0 && pcl.wm10c.is_nan() {
            let pe3 = pelast;
            let h3 = interp_hght(prof, pe3);
            let te3 = interp_vtmp(prof, pe3);
            let tp3 = wetlift(pe1, tp1, pe3);
            if lyre > 0.0 {
                pcl.wm10c = totp - lyre;
            } else {
                pcl.wm10c = totp;
            }
            if pm10c.is_nan() || pm10c > pcl.lclpres {
                pcl.wm10c = 0.0;
            } else if !pe2.is_nan() {
                let te2f = interp_vtmp(prof, pe2);
                let tp2f = wetlift(pe3, tp3, pe2);
                let tdef3 = (virtemp(pe3, tp3, tp3) - te3) / ctok(te3);
                let tdef2f = (virtemp(pe2, tp2f, tp2f) - te2f) / ctok(te2f);
                let lyrf = G * (tdef3 + tdef2f) / 2.0 * (pcl.hghtm10c - h3);
                if lyrf > 0.0 {
                    pcl.wm10c += lyrf;
                }
            }
        }

        // --- -20C level CAPE ---
        if te2 < -20.0 && pcl.wm20c.is_nan() {
            let pe3 = pelast;
            let h3 = interp_hght(prof, pe3);
            let te3 = interp_vtmp(prof, pe3);
            let tp3 = wetlift(pe1, tp1, pe3);
            if lyre > 0.0 {
                pcl.wm20c = totp - lyre;
            } else {
                pcl.wm20c = totp;
            }
            if pm20c.is_nan() || pm20c > pcl.lclpres {
                pcl.wm20c = 0.0;
            } else if !pe2.is_nan() {
                let te2f = interp_vtmp(prof, pe2);
                let tp2f = wetlift(pe3, tp3, pe2);
                let tdef3 = (virtemp(pe3, tp3, tp3) - te3) / ctok(te3);
                let tdef2f = (virtemp(pe2, tp2f, tp2f) - te2f) / ctok(te2f);
                let lyrf = G * (tdef3 + tdef2f) / 2.0 * (pcl.hghtm20c - h3);
                if lyrf > 0.0 {
                    pcl.wm20c += lyrf;
                }
            }
        }

        // --- -30C level CAPE ---
        if te2 < -30.0 && pcl.wm30c.is_nan() {
            let pe3 = pelast;
            let h3 = interp_hght(prof, pe3);
            let te3 = interp_vtmp(prof, pe3);
            let tp3 = wetlift(pe1, tp1, pe3);
            if lyre > 0.0 {
                pcl.wm30c = totp - lyre;
            } else {
                pcl.wm30c = totp;
            }
            if pm30c.is_nan() || pm30c > pcl.lclpres {
                pcl.wm30c = 0.0;
            } else if !pe2.is_nan() {
                let te2f = interp_vtmp(prof, pe2);
                let tp2f = wetlift(pe3, tp3, pe2);
                let tdef3 = (virtemp(pe3, tp3, tp3) - te3) / ctok(te3);
                let tdef2f = (virtemp(pe2, tp2f, tp2f) - te2f) / ctok(te2f);
                let lyrf = G * (tdef3 + tdef2f) / 2.0 * (pcl.hghtm30c - h3);
                if lyrf > 0.0 {
                    pcl.wm30c += lyrf;
                }
            }
        }

        // --- 3 km CAPE ---
        if pcl.lclhght < 3000.0 {
            let h1_agl = to_agl(prof, h1);
            let h2_agl = to_agl(prof, h2);
            if h1_agl <= 3000.0 && h2_agl >= 3000.0 && pcl.b3km.is_nan() {
                let pe3 = pelast;
                let h3 = interp_hght(prof, pe3);
                let te3 = interp_vtmp(prof, pe3);
                let tp3 = wetlift(pe1, tp1, pe3);
                if lyre > 0.0 {
                    pcl.b3km = totp - lyre;
                } else {
                    pcl.b3km = totp;
                }
                let h4 = to_msl(prof, 3000.0);
                let pe4 = interp_pres_from_hght(prof, h4);
                if !pe4.is_nan() {
                    let te2f = interp_vtmp(prof, pe4);
                    let tp2f = wetlift(pe3, tp3, pe4);
                    let tdef3 = (virtemp(pe3, tp3, tp3) - te3) / ctok(te3);
                    let tdef2f = (virtemp(pe4, tp2f, tp2f) - te2f) / ctok(te2f);
                    let lyrf = G * (tdef3 + tdef2f) / 2.0 * (h4 - h3);
                    if lyrf > 0.0 {
                        pcl.b3km += lyrf;
                    }
                }
            }
        } else if pcl.b3km.is_nan() {
            pcl.b3km = 0.0;
        }

        // --- 6 km CAPE ---
        if pcl.lclhght < 6000.0 {
            let h1_agl = to_agl(prof, h1);
            let h2_agl = to_agl(prof, h2);
            if h1_agl <= 6000.0 && h2_agl >= 6000.0 && pcl.b6km.is_nan() {
                let pe3 = pelast;
                let h3 = interp_hght(prof, pe3);
                let te3 = interp_vtmp(prof, pe3);
                let tp3 = wetlift(pe1, tp1, pe3);
                if lyre > 0.0 {
                    pcl.b6km = totp - lyre;
                } else {
                    pcl.b6km = totp;
                }
                let h4 = to_msl(prof, 6000.0);
                let pe4 = interp_pres_from_hght(prof, h4);
                if !pe4.is_nan() {
                    let te2f = interp_vtmp(prof, pe4);
                    let tp2f = wetlift(pe3, tp3, pe4);
                    let tdef3 = (virtemp(pe3, tp3, tp3) - te3) / ctok(te3);
                    let tdef2f = (virtemp(pe4, tp2f, tp2f) - te2f) / ctok(te2f);
                    let lyrf = G * (tdef3 + tdef2f) / 2.0 * (h4 - h3);
                    if lyrf > 0.0 {
                        pcl.b6km += lyrf;
                    }
                }
            }
        } else if pcl.b6km.is_nan() {
            pcl.b6km = 0.0;
        }

        h1 = h2;

        // --- LFC possibility ---
        if lyre >= 0.0 && lyrlast <= 0.0 {
            let tp3 = tp1;
            let pe3 = pelast;
            let wl = wetlift(pe1, tp3, pe3);
            if interp_vtmp(prof, pe3) < virtemp(pe3, wl, wl) {
                // Found an LFC
                pcl.lfcpres = pe3;
                pcl.lfchght = to_agl(prof, interp_hght(prof, pe3));
                pcl.elpres = f64::NAN;
                pcl.elhght = f64::NAN;
                pcl.mplpres = f64::NAN;
            } else {
                // Search downward in 5 hPa steps for exact crossing
                let mut pe3s = pe3;
                loop {
                    if pe3s <= 0.0 {
                        break;
                    }
                    let wl2 = wetlift(pe1, tp3, pe3s);
                    if interp_vtmp(prof, pe3s) < virtemp(pe3s, wl2, wl2) {
                        pcl.lfcpres = pe3s;
                        pcl.lfchght = to_agl(prof, interp_hght(prof, pe3s));
                        tote = 0.0;
                        li_max = -9999.0;
                        if cap_strength < 0.0 {
                            cap_strength = 0.0;
                        }
                        pcl.cap = cap_strength;
                        pcl.cappres = cap_strengthpres;
                        pcl.elpres = f64::NAN;
                        pcl.elhght = f64::NAN;
                        pcl.mplpres = f64::NAN;
                        break;
                    }
                    pe3s -= 5.0;
                }
            }
            // Force LFC >= LCL
            if !pcl.lfcpres.is_nan() && pcl.lfcpres >= pcl.lclpres {
                pcl.lfcpres = pcl.lclpres;
                pcl.lfchght = pcl.lclhght;
            }
        }

        // --- EL possibility ---
        if lyre <= 0.0 && lyrlast >= 0.0 {
            let tp3 = tp1;
            let mut pe3 = pelast;
            loop {
                let wl = wetlift(pe1, tp3, pe3);
                if interp_vtmp(prof, pe3) >= virtemp(pe3, wl, wl) {
                    break;
                }
                pe3 -= 5.0;
                if pe3 <= 0.0 {
                    break;
                }
            }
            pcl.elpres = pe3;
            pcl.elhght = to_agl(prof, interp_hght(prof, pe3));
            pcl.mplpres = f64::NAN;
            pcl.limax = -li_max;
            pcl.limaxpres = li_maxpres;
        }

        // --- MPL possibility ---
        if tote < 0.0 && pcl.mplpres.is_nan() && !pcl.elpres.is_nan() {
            let pe3_start = pelast;
            let h3_start = interp_hght(prof, pe3_start);
            let te3_start = interp_vtmp(prof, pe3_start);
            let tp3_start = wetlift(pe1, tp1, pe3_start);
            let mut totx = tote - lyre;
            let mut pe2m = pelast;
            let mut tp3m = tp3_start;
            let mut te3m = te3_start;
            let mut h3m = h3_start;
            let mut pe3m = pe3_start;
            while totx > 0.0 && pe2m > 0.0 {
                pe2m -= 1.0;
                let te2m = interp_vtmp(prof, pe2m);
                let tp2m = wetlift(pe3m, tp3m, pe2m);
                let h2m = interp_hght(prof, pe2m);
                let td3 = (virtemp(pe3m, tp3m, tp3m) - te3m) / ctok(te3m);
                let td2 = (virtemp(pe2m, tp2m, tp2m) - te2m) / ctok(te2m);
                let lyrf = G * (td3 + td2) / 2.0 * (h2m - h3m);
                totx += lyrf;
                tp3m = tp2m;
                te3m = te2m;
                pe3m = pe2m;
                h3m = h2m;
            }
            pcl.mplpres = pe2m;
            pcl.mplhght = to_agl(prof, interp_hght(prof, pe2m));
        }

        // --- 500 hPa lifted index ---
        if pe2 <= 500.0 && pcl.li5.is_nan() {
            let a = interp_vtmp(prof, 500.0);
            let b = wetlift(pe1, tp1, 500.0);
            pcl.li5 = a - virtemp(500.0, b, b);
        }

        // --- 300 hPa lifted index ---
        if pe2 <= 300.0 && pcl.li3.is_nan() {
            let a = interp_vtmp(prof, 300.0);
            let b = wetlift(pe1, tp1, 300.0);
            pcl.li3 = a - virtemp(300.0, b, b);
        }
    }

    if pcl.bplus.is_nan() {
        pcl.bplus = totp;
    }
    if pcl.bminus.is_nan() {
        pcl.bminus = totn;
    }
    if pcl.bplus.floor() == 0.0 {
        pcl.bminus = 0.0;
    }

    // Minimum buoyancy below 500 hPa (Trier et al. 2014)
    let mut bmin_val = f64::INFINITY;
    let mut bminpres_val = f64::NAN;
    for (idx, &pp) in pcl.ptrace.iter().enumerate() {
        if pp >= 500.0 && idx < pcl.ttrace.len() {
            let env_vt = interp_vtmp(prof, pp);
            if !env_vt.is_nan() {
                let b = pcl.ttrace[idx] - env_vt;
                if b < bmin_val {
                    bmin_val = b;
                    bminpres_val = pp;
                }
            }
        }
    }
    if bmin_val < f64::INFINITY {
        pcl.bmin = bmin_val;
        pcl.bminpres = bminpres_val;
    }

    pcl
}

// =========================================================================
// LFC — Level of Free Convection
// =========================================================================

/// Compute the Level of Free Convection for a parcel.
///
/// Returns `(lfc_pressure_hPa, lfc_height_m_AGL)`.  Returns `(NAN, NAN)` if
/// no LFC is found.
pub fn lfc(prof: &Profile, lpl: &LiftedParcelLevel) -> (f64, f64) {
    let pcl = parcelx(prof, lpl, None, None);
    (pcl.lfcpres, pcl.lfchght)
}

// =========================================================================
// EL — Equilibrium Level
// =========================================================================

/// Compute the Equilibrium Level for a parcel.
///
/// Returns `(el_pressure_hPa, el_height_m_AGL)`.  Returns `(NAN, NAN)` if
/// no EL is found.
pub fn el(prof: &Profile, lpl: &LiftedParcelLevel) -> (f64, f64) {
    let pcl = parcelx(prof, lpl, None, None);
    (pcl.elpres, pcl.elhght)
}

// =========================================================================
// CIN — Convective Inhibition
// =========================================================================

/// Compute the Convective Inhibition (CIN, J/kg) for a parcel.
///
/// Returns a negative value (energy required to lift the parcel to its LFC).
/// Returns NAN if computation fails.
pub fn cin(prof: &Profile, lpl: &LiftedParcelLevel) -> f64 {
    let pcl = parcelx(prof, lpl, None, None);
    pcl.bminus
}

// =========================================================================
// DCAPE — Downdraft CAPE
// =========================================================================

/// Downdraft CAPE result.
#[derive(Debug, Clone)]
pub struct DcapeResult {
    /// Downdraft CAPE (J/kg), typically negative.
    pub dcape: f64,
    /// Temperature trace (C) of the descending parcel.
    pub ttrace: Vec<f64>,
    /// Pressure trace (hPa) of the descending parcel.
    pub ptrace: Vec<f64>,
}

/// Compute Downdraft CAPE (DCAPE).
///
/// Adapted from John Hart's (SPC) DCAPE code.  Finds the minimum 100 hPa
/// layer-averaged theta-e in the lowest 400 hPa, then lowers that parcel
/// moist-adiabatically to the surface.  No virtual temperature correction
/// is applied (matching SHARPpy).
pub fn dcape(prof: &Profile) -> DcapeResult {
    let sfc_pres = prof.pres[prof.sfc];
    let n = prof.nlevels();

    // Build arrays excluding NAN theta-e values
    let mut valid_idx: Vec<usize> = Vec::new();
    for i in 0..n {
        if !prof.thetae[i].is_nan() && !prof.pres[i].is_nan() {
            valid_idx.push(i);
        }
    }

    // Find indices within the lowest 400 hPa
    let idx_400: Vec<usize> = valid_idx
        .iter()
        .copied()
        .filter(|&i| prof.pres[i] >= sfc_pres - 400.0)
        .collect();

    // Find minimum 100 hPa layer-averaged theta-e
    let mut mine = 1000.0_f64;
    let mut minp = -999.0_f64;
    for &i in &idx_400 {
        let thta_e_mean = mean_thetae(prof, prof.pres[i], prof.pres[i] - 100.0);
        if !thta_e_mean.is_nan() && thta_e_mean < mine {
            minp = prof.pres[i] - 50.0;
            mine = thta_e_mean;
        }
    }

    if minp < 0.0 {
        return DcapeResult {
            dcape: 0.0,
            ttrace: Vec::new(),
            ptrace: Vec::new(),
        };
    }

    let upper = minp;

    // Find the index of the level closest to `upper`
    let uptr = match valid_idx.iter().rposition(|&i| prof.pres[i] >= upper) {
        Some(vi) => valid_idx[vi],
        None => {
            return DcapeResult {
                dcape: 0.0,
                ttrace: Vec::new(),
                ptrace: Vec::new(),
            };
        }
    };

    // Define parcel starting point
    let tp1_init = wetbulb(upper, interp_temp(prof, upper), interp_dwpt(prof, upper));
    let mut tp1 = tp1_init;
    let mut pe1 = upper;
    let mut te1 = interp_temp(prof, pe1);
    let mut h1 = interp_hght(prof, pe1);
    let mut tote = 0.0_f64;

    let mut ttrace = vec![tp1_init];
    let mut ptrace = vec![upper];

    // Lower the parcel to the surface moist adiabatically
    let mut i = uptr;
    loop {
        if i <= prof.sfc {
            break;
        }
        i -= 1;
        if prof.pres[i].is_nan() || prof.tmpc[i].is_nan() {
            continue;
        }

        let pe2 = prof.pres[i];
        let te2 = prof.tmpc[i];
        let h2 = prof.hght[i];
        let tp2 = wetlift(pe1, tp1, pe2);

        if !te1.is_nan() && !te2.is_nan() {
            let tdef1 = (tp1 - te1) / ctok(te1);
            let tdef2 = (tp2 - te2) / ctok(te2);
            let lyre = G * (tdef1 + tdef2) / 2.0 * (h2 - h1);
            tote += lyre;
        }

        ttrace.push(tp2);
        ptrace.push(pe2);

        pe1 = pe2;
        te1 = te2;
        h1 = h2;
        tp1 = tp2;
    }

    // Reverse traces so they go from surface upward
    ttrace.reverse();
    ptrace.reverse();

    DcapeResult {
        dcape: tote,
        ttrace,
        ptrace,
    }
}

// =========================================================================
// Effective Inflow Layer
// =========================================================================

/// Compute the effective inflow layer (Thompson et al. 2007).
///
/// Returns `(pbot, ptop)` in hPa.  Returns `(NAN, NAN)` if no effective
/// inflow layer is found.
///
/// # Arguments
/// * `prof` — sounding profile
/// * `ecape` — minimum CAPE threshold (J/kg), default 100
/// * `ecinh` — maximum CIN threshold (J/kg, negative), default -250
/// * `mupcl` — optional pre-computed most-unstable parcel result
pub fn effective_inflow_layer(
    prof: &Profile,
    ecape: f64,
    ecinh: f64,
    mupcl: Option<&ParcelResult>,
) -> (f64, f64) {
    // Get or compute MU parcel
    let mu_owned;
    let mupcl = match mupcl {
        Some(p) => p,
        None => {
            let lpl = define_parcel(prof, ParcelType::MostUnstable { depth_hpa: 300.0 });
            mu_owned = cape(prof, &lpl, None, None);
            &mu_owned
        }
    };

    let mucape = mupcl.bplus;
    let mucinh = mupcl.bminus;

    if mucape.is_nan() || mucape == 0.0 {
        return (f64::NAN, f64::NAN);
    }
    if mucape < ecape || mucinh.is_nan() || mucinh <= ecinh {
        return (f64::NAN, f64::NAN);
    }

    let mut pbot = f64::NAN;
    let mut ptop = f64::NAN;

    // Search upward from surface for effective bottom
    for i in prof.sfc..prof.top {
        if prof.tmpc[i].is_nan() || prof.dwpc[i].is_nan() {
            continue;
        }
        let lpl = LiftedParcelLevel {
            pres: prof.pres[i],
            tmpc: prof.tmpc[i],
            dwpc: prof.dwpc[i],
            parcel_type: ParcelType::UserDefined {
                pres: prof.pres[i],
                tmpc: prof.tmpc[i],
                dwpc: prof.dwpc[i],
            },
        };
        let pcl = cape(prof, &lpl, None, None);
        if pcl.bplus >= ecape && pcl.bminus > ecinh {
            pbot = prof.pres[i];
            // Continue searching upward for the effective top
            for j in (i + 1)..prof.top {
                if prof.tmpc[j].is_nan() || prof.dwpc[j].is_nan() {
                    continue;
                }
                let lpl2 = LiftedParcelLevel {
                    pres: prof.pres[j],
                    tmpc: prof.tmpc[j],
                    dwpc: prof.dwpc[j],
                    parcel_type: ParcelType::UserDefined {
                        pres: prof.pres[j],
                        tmpc: prof.tmpc[j],
                        dwpc: prof.dwpc[j],
                    },
                };
                let pcl2 = cape(prof, &lpl2, None, None);
                if pcl2.bplus < ecape || pcl2.bminus <= ecinh {
                    let mut k = j - 1;
                    while k > i && (prof.tmpc[k].is_nan() || prof.dwpc[k].is_nan()) {
                        k -= 1;
                    }
                    ptop = prof.pres[k];
                    if ptop > pbot {
                        ptop = pbot;
                    }
                    break;
                }
            }
            break;
        }
    }

    (pbot, ptop)
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a simple idealized sounding for testing.
    ///
    /// Roughly a standard-atmosphere-like profile with some moisture:
    /// surface at 1000 hPa, top near 100 hPa.
    fn make_test_profile() -> Profile {
        let pres: Vec<f64> = vec![
            1000.0, 975.0, 950.0, 925.0, 900.0, 875.0, 850.0, 825.0, 800.0, 775.0, 750.0, 725.0,
            700.0, 675.0, 650.0, 625.0, 600.0, 575.0, 550.0, 525.0, 500.0, 475.0, 450.0, 425.0,
            400.0, 375.0, 350.0, 325.0, 300.0, 275.0, 250.0, 225.0, 200.0, 175.0, 150.0, 125.0,
            100.0,
        ];
        let hght: Vec<f64> = vec![
            110.0, 330.0, 554.0, 782.0, 1014.0, 1251.0, 1494.0, 1743.0, 1999.0, 2262.0, 2533.0,
            2813.0, 3103.0, 3404.0, 3717.0, 4044.0, 4387.0, 4747.0, 5127.0, 5530.0, 5960.0, 6420.0,
            6915.0, 7450.0, 8032.0, 8670.0, 9374.0, 10154.0, 11024.0, 12000.0, 13105.0, 14370.0,
            15834.0, 17555.0, 19620.0, 22140.0, 25350.0,
        ];
        let tmpc: Vec<f64> = vec![
            30.0, 27.0, 24.0, 21.0, 18.0, 15.0, 12.0, 9.0, 6.5, 4.0, 1.5, -1.0, -3.5, -6.0, -9.0,
            -12.0, -15.0, -18.5, -22.0, -26.0, -30.0, -34.0, -38.5, -43.0, -48.0, -53.0, -58.0,
            -63.5, -69.0, -70.0, -68.0, -66.0, -64.0, -62.0, -58.0, -52.0, -45.0,
        ];
        let dwpc: Vec<f64> = vec![
            22.0, 21.0, 20.0, 18.0, 16.0, 13.0, 10.0, 6.0, 2.0, -2.0, -6.0, -10.0, -14.0, -18.0,
            -22.0, -26.0, -30.0, -34.0, -38.0, -42.0, -46.0, -50.0, -54.0, -58.0, -62.0, -66.0,
            -70.0, -74.0, -78.0, -78.0, -76.0, -74.0, -72.0, -70.0, -66.0, -60.0, -55.0,
        ];
        Profile::new(pres, hght, tmpc, dwpc, 0)
    }

    #[test]
    fn test_lcl_basic() {
        let (p, t) = lcl(1000.0, 30.0, 22.0);
        assert!(p > 800.0 && p < 1000.0, "LCL pressure {p} out of range");
        assert!(t < 30.0 && t > 10.0, "LCL temperature {t} out of range");
    }

    #[test]
    fn test_drylift_roundtrip() {
        let (p2, t2) = drylift(1000.0, 20.0, 15.0);
        let th1 = theta(1000.0, 20.0, 1000.0);
        let th2 = theta(p2, t2, 1000.0);
        assert!(
            (th1 - th2).abs() < 0.5,
            "Theta not preserved in drylift: {th1} vs {th2}"
        );
    }

    #[test]
    fn test_exact_mixed_layer_native_weighting() {
        let prof = make_test_profile();
        let pbot = 1000.0;
        let ptop = 900.0;

        let expected_theta = (theta(1000.0, 30.0, 1000.0)
            + 2.0 * theta(975.0, 27.0, 1000.0)
            + 2.0 * theta(950.0, 24.0, 1000.0)
            + 2.0 * theta(925.0, 21.0, 1000.0)
            + theta(900.0, 18.0, 1000.0))
            / 8.0;
        let expected_mixratio = mixratio(950.0, 19.5);

        assert!((mean_theta_exact(&prof, pbot, ptop) - expected_theta).abs() < 1e-10);
        assert!((mean_mixratio_exact(&prof, pbot, ptop) - expected_mixratio).abs() < 1e-10);
    }

    #[test]
    fn test_wetlift_identity() {
        let t = wetlift(850.0, 10.0, 850.0);
        assert!(
            (t - 10.0).abs() < 0.2,
            "wetlift to same level should give same temp, got {t}"
        );
    }

    #[test]
    fn test_wetlift_cooling() {
        let t = wetlift(850.0, 10.0, 500.0);
        assert!(t < 10.0, "wetlift should cool: got {t}");
    }

    #[test]
    fn test_virtemp_warmer() {
        let vt = virtemp(1000.0, 20.0, 15.0);
        assert!(vt >= 20.0, "Virtual temp {vt} should be >= dry temp 20.0");
    }

    #[test]
    fn test_parcelx_surface() {
        let prof = make_test_profile();
        let lpl = define_parcel(&prof, ParcelType::Surface);
        let pcl = parcelx(&prof, &lpl, None, None);

        assert!(
            pcl.bplus > 0.0,
            "CAPE should be positive, got {}",
            pcl.bplus
        );
        assert!(!pcl.lclpres.is_nan(), "LCL pressure should not be NAN");
        assert!(pcl.lclhght > 0.0, "LCL height should be positive");
    }

    #[test]
    fn test_parcelx_mixed_layer() {
        let prof = make_test_profile();
        let lpl = define_parcel(&prof, ParcelType::MixedLayer { depth_hpa: 100.0 });
        let pcl = parcelx(&prof, &lpl, None, None);

        assert!(
            pcl.bplus > 0.0,
            "ML CAPE should be positive, got {}",
            pcl.bplus
        );
    }

    #[test]
    fn test_parcelx_most_unstable() {
        let prof = make_test_profile();
        let lpl = define_parcel(&prof, ParcelType::MostUnstable { depth_hpa: 300.0 });
        let pcl = parcelx(&prof, &lpl, None, None);

        assert!(
            pcl.bplus > 0.0,
            "MU CAPE should be positive, got {}",
            pcl.bplus
        );
    }

    #[test]
    fn test_cape_fast_vs_parcelx() {
        let prof = make_test_profile();
        let lpl = define_parcel(&prof, ParcelType::Surface);

        let fast = cape(&prof, &lpl, None, None);
        let full = parcelx(&prof, &lpl, None, None);

        let diff = (fast.bplus - full.bplus).abs();
        let max_val = fast.bplus.max(full.bplus);
        if max_val > 10.0 {
            assert!(
                diff / max_val < 0.15,
                "cape() and parcelx() CAPE differ too much: {} vs {}",
                fast.bplus,
                full.bplus
            );
        }
    }

    #[test]
    fn test_dcape_computes() {
        let prof = make_test_profile();
        let result = dcape(&prof);
        // DCAPE is the energy of a descending parcel.  The sign depends on
        // the profile: for our warm/moist test sounding the downdraft parcel
        // (minimum theta-e layer) may be warmer or cooler than the
        // environment at lower levels.  Just verify we get a finite result
        // and non-empty traces.
        assert!(
            result.dcape.is_finite(),
            "DCAPE should be finite, got {}",
            result.dcape
        );
        assert!(
            !result.ptrace.is_empty(),
            "DCAPE ptrace should not be empty"
        );
        assert_eq!(result.ptrace.len(), result.ttrace.len());
    }

    #[test]
    fn test_buoyancy_surface() {
        let prof = make_test_profile();
        let b = buoyancy(&prof, 1000.0, 30.0, 22.0, 1000.0);
        assert!(
            b.abs() < 1.0,
            "Buoyancy at surface should be near zero, got {b}"
        );
    }

    #[test]
    fn test_most_unstable_level_in_range() {
        let prof = make_test_profile();
        let mu = most_unstable_level(&prof, 1000.0, 700.0);
        assert!(
            mu >= 700.0 && mu <= 1000.0,
            "MU level {mu} not in range [700, 1000]"
        );
    }

    #[test]
    fn test_interp_pres_basic() {
        let pres = vec![1000.0, 500.0];
        let field = vec![0.0, 100.0];
        let val = interp_pres(750.0, &pres, &field);
        assert!(
            val > 0.0 && val < 100.0,
            "Interpolated value {val} out of range"
        );
    }

    #[test]
    fn test_stable_profile_no_cape() {
        let pres: Vec<f64> = vec![
            1000.0, 900.0, 800.0, 700.0, 600.0, 500.0, 400.0, 300.0, 200.0,
        ];
        let hght: Vec<f64> = vec![
            100.0, 1000.0, 2000.0, 3100.0, 4300.0, 5600.0, 7200.0, 9200.0, 11800.0,
        ];
        let tmpc: Vec<f64> = vec![10.0, 15.0, 10.0, 5.0, -5.0, -15.0, -30.0, -45.0, -60.0];
        let dwpc: Vec<f64> = vec![5.0, -5.0, -15.0, -25.0, -35.0, -45.0, -55.0, -65.0, -75.0];
        let prof = Profile::new(pres, hght, tmpc, dwpc, 0);

        let lpl = define_parcel(&prof, ParcelType::Surface);
        let pcl = parcelx(&prof, &lpl, None, None);

        assert!(
            pcl.bplus < 100.0,
            "Stable profile should have minimal CAPE, got {}",
            pcl.bplus
        );
    }

    #[test]
    fn test_theta_roundtrip() {
        let t = 20.0;
        let p = 850.0;
        let th = theta(p, t, 1000.0);
        let t_back = theta(1000.0, th, p);
        assert!(
            (t - t_back).abs() < 0.01,
            "Theta roundtrip failed: {t} vs {t_back}"
        );
    }

    #[test]
    fn test_mixratio_positive() {
        let w = mixratio(1000.0, 20.0);
        assert!(w > 0.0, "Mixing ratio should be positive, got {w}");
        assert!(w < 50.0, "Mixing ratio should be reasonable, got {w}");
    }
}
