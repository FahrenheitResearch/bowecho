//! Volume-derived radar products via a shared column-walk.
//!
//! All three products resample every elevation cut onto the lowest tilt's
//! (azimuth, range) grid by GROUND location and walk the resulting vertical
//! column using 4/3-Earth beam geometry (Doviak & Zrnić 1993, eqs. 2.28b/c):
//!
//! * **Composite reflectivity** — column-max reflectivity (NWS NCR concept).
//! * **Echo tops** — height of the highest tilt with Z ≥ threshold (NWS ET
//!   uses 18.3 dBZ).
//! * **VIL / VIL density** — Vertically Integrated Liquid (Greene & Clark 1972,
//!   *A Vertically Integrated Liquid Water Content Profile from Radar Data*,
//!   JAM 11(8); the operational discretization in Witt et al. 1998, WAF 13(2),
//!   with the 56 dBZ hail cap). VIL density = VIL / echo-top height.
//!
//! Output grids reuse the base tilt's geometry and `radial_indices`, so the
//! existing renderer/azimuth lookup draws them unchanged.

use radar_core::{
    MomentGrid, MomentStorage, MomentType, RadarVolume, beam_ground_range_m,
    beam_height_above_radar_m,
};
use rayon::prelude::*;

/// NWS echo-top reflectivity threshold (dBZ).
pub const ECHO_TOP_THRESHOLD_DBZ: f32 = 18.3;
/// Hail cap applied to reflectivity before VIL integration (dBZ).
const VIL_HAIL_CAP_DBZ: f32 = 56.0;

/// A single elevation cut resampled for column walking: a ground-range table
/// and beam-height table per gate, plus an azimuth→row index.
struct CutColumn<'a> {
    elevation_deg: f32,
    grid: &'a MomentGrid,
    az_rows: Vec<(f32, usize)>, // (azimuth_deg, row) sorted by azimuth
    ground_range_m: Vec<f64>,   // per gate
    height_m: Vec<f64>,         // beam-center height above radar per gate
}

impl<'a> CutColumn<'a> {
    fn new(volume: &'a RadarVolume, cut_index: usize, grid: &'a MomentGrid) -> Option<Self> {
        let cut = volume.cuts.get(cut_index)?;
        let gr = &grid.gate_range;
        if gr.gate_count == 0 {
            return None;
        }
        let mut az_rows: Vec<(f32, usize)> = grid
            .radial_indices
            .iter()
            .enumerate()
            .filter_map(|(row, ri)| {
                let az = cut.radials.get(*ri)?.azimuth_deg.rem_euclid(360.0);
                Some((az, row))
            })
            .collect();
        if az_rows.is_empty() {
            return None;
        }
        az_rows.sort_by(|a, b| a.0.total_cmp(&b.0));

        let elevation_deg = cut.elevation_deg;
        let (ground_range_m, height_m) = (0..gr.gate_count)
            .map(|g| {
                let r = gr.first_gate_m as f64 + g as f64 * gr.gate_spacing_m as f64;
                (
                    beam_ground_range_m(r, elevation_deg as f64),
                    beam_height_above_radar_m(r, elevation_deg as f64),
                )
            })
            .unzip();

        Some(Self {
            elevation_deg,
            grid,
            az_rows,
            ground_range_m,
            height_m,
        })
    }

    fn nearest_row(&self, az: f32) -> usize {
        match self.az_rows.binary_search_by(|p| p.0.total_cmp(&az)) {
            Ok(i) => self.az_rows[i].1,
            Err(i) => {
                let lo = if i == 0 {
                    self.az_rows.len() - 1
                } else {
                    i - 1
                };
                let hi = if i >= self.az_rows.len() { 0 } else { i };
                let dl = ang_dist(self.az_rows[lo].0, az);
                let dh = ang_dist(self.az_rows[hi].0, az);
                if dl <= dh {
                    self.az_rows[lo].1
                } else {
                    self.az_rows[hi].1
                }
            }
        }
    }

    /// Gate index whose ground range is closest to `s` (monotonic table).
    fn gate_for_ground_range(&self, s: f64) -> Option<usize> {
        let n = self.ground_range_m.len();
        if n == 0 {
            return None;
        }
        // No sample beyond the farthest gate, and none BELOW the first gate's
        // ground range (within half a gate). The latter matters for high tilts:
        // their beam only reaches the surface ground range `ground_range_m[0]`,
        // so clamping shorter ranges to gate 0 would smear elevated reflectivity
        // into the radar's cone of silence (false-high CREF/ET/VIL over the site).
        let half_gate = if n >= 2 {
            0.5 * (self.ground_range_m[1] - self.ground_range_m[0])
        } else {
            0.0
        };
        if s > self.ground_range_m[n - 1] || s < self.ground_range_m[0] - half_gate {
            return None;
        }
        match self.ground_range_m.binary_search_by(|g| g.total_cmp(&s)) {
            Ok(i) => Some(i),
            Err(i) => {
                if i == 0 {
                    Some(0)
                } else if i >= n {
                    Some(n - 1)
                } else if (self.ground_range_m[i] - s) < (s - self.ground_range_m[i - 1]) {
                    Some(i)
                } else {
                    Some(i - 1)
                }
            }
        }
    }

    /// Reflectivity (dBZ) and beam height (m) at ground range `s`, azimuth `az`.
    fn sample(&self, az: f32, s: f64) -> Option<(f32, f64)> {
        let gate = self.gate_for_ground_range(s)?;
        let row = self.nearest_row(az);
        let value = self.grid.scaled_value(row, gate)?;
        if !value.is_finite() {
            return None;
        }
        Some((value, self.height_m[gate]))
    }

    /// Inverse of `az_rows`: per-row azimuth (NaN for rows with no radial), so
    /// the column walk avoids an O(rows) scan per output row.
    fn row_azimuths(&self, rows: usize) -> Vec<f32> {
        let mut v = vec![f32::NAN; rows];
        for &(az, r) in &self.az_rows {
            if r < rows {
                v[r] = az;
            }
        }
        v
    }
}

fn ang_dist(a: f32, b: f32) -> f32 {
    let d = (a - b).rem_euclid(360.0);
    d.min(360.0 - d)
}

/// Lowest-elevation cut index that carries reflectivity, and its grid.
fn base_reflectivity_cut(volume: &RadarVolume) -> Option<(usize, &MomentGrid)> {
    volume
        .cuts
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            c.moments
                .get(&MomentType::Reflectivity)
                .map(|g| (i, c.elevation_deg, g))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _, g)| (i, g))
}

/// All reflectivity-bearing cuts as column samplers, sorted by elevation.
fn reflectivity_columns(volume: &RadarVolume) -> Vec<CutColumn<'_>> {
    let mut cols: Vec<CutColumn<'_>> = volume
        .cuts
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            let g = c.moments.get(&MomentType::Reflectivity)?;
            CutColumn::new(volume, i, g)
        })
        .collect();
    cols.sort_by(|a, b| a.elevation_deg.total_cmp(&b.elevation_deg));
    cols
}

/// Build an F32 output grid on the base tilt's geometry (NaN = no data).
/// Public alias for sibling modules building products on a base geometry.
pub(crate) fn f32_grid_like_pub(
    base: &MomentGrid,
    moment: MomentType,
    values: Vec<f32>,
) -> MomentGrid {
    f32_grid_like(base, moment, values)
}

fn f32_grid_like(base: &MomentGrid, moment: MomentType, values: Vec<f32>) -> MomentGrid {
    MomentGrid {
        moment,
        gate_range: base.gate_range.clone(),
        scale: 1.0,
        offset: 0.0,
        nodata: None,
        range_folded: None,
        radial_indices: base.radial_indices.clone(),
        storage: MomentStorage::F32(values),
    }
}

/// 4/3-effective-earth radius, m (Doviak & Zrnic 1993 Eq. 2.28 model).
const AE_M: f64 = 4.0 / 3.0 * 6_371_000.0;
/// WSR-88D half-power half-beamwidth, rad (0.95 deg aperture / 2).
const HALF_BW_RAD: f64 = 0.475 * std::f64::consts::PI / 180.0;

/// One cross-section profile sample: beam-center height, tilt elevation,
/// recovered slant range (for the beamwidth rules), value.
#[derive(Clone, Copy)]
struct ProfileSample {
    h: f64,
    theta_deg: f64,
    r_m: f64,
    v: f32,
}

/// Interpolation policy per moment family (docs/xsection-3d-spec.md):
/// reflectivity/ZDR blend linearly; CC must not blend through the melting
/// layer (Giangrande, Krause & Ryzhkov 2008: the rho_hv minimum is the
/// signature — blending fabricates intermediate values), so any bracket
/// below 0.97 falls back to nearest-gate; velocity guards against blending
/// across strong shear or residual aliasing.
#[derive(Clone, Copy, PartialEq)]
pub enum InterpPolicy {
    LinearAngle,
    CcGuard,
    VelocityGuard,
}

/// Slant range + elevation angle at (ground distance s, height h) — the
/// exact closed-form inverse of the 4/3-earth height equation (law of
/// cosines on the effective sphere; unit-tested to round-trip).
fn invert_beam(s: f64, h: f64) -> (f64, f64) {
    let sigma = s / AE_M;
    let r = (AE_M * AE_M + (AE_M + h) * (AE_M + h) - 2.0 * AE_M * (AE_M + h) * sigma.cos())
        .max(0.0)
        .sqrt();
    if r < 1.0 {
        return (0.0, 90.0);
    }
    let sin_theta =
        (((AE_M + h) * (AE_M + h) - AE_M * AE_M - r * r) / (2.0 * AE_M * r)).clamp(-1.0, 1.0);
    (r, sin_theta.asin().to_degrees())
}

/// Cross-section column: rich samples across all cuts, ascending height.
fn column_profile_xs(cols: &[CutColumn<'_>], az: f32, s: f64) -> Vec<ProfileSample> {
    let mut prof: Vec<ProfileSample> = cols
        .iter()
        .filter_map(|c| {
            c.sample(az, s).map(|(v, h)| {
                let (r_m, _) = invert_beam(s, h);
                ProfileSample {
                    h,
                    theta_deg: f64::from(c.elevation_deg),
                    r_m,
                    v,
                }
            })
        })
        .collect();
    prof.sort_by(|a, b| a.h.total_cmp(&b.h));
    prof
}

/// MRMS-style vertical interpolation at height z, ground distance s
/// (Zhang, Howard & Gourley 2005, Eqs. 5-7): linear IN ELEVATION ANGLE
/// between the bracketing tilts — not in height. Edge rule (Zhang et al.
/// 2011): values extend past the top/bottom tilt only within half a
/// beamwidth (range-dependent), never further; below the lowest beam we
/// keep a 300 m display floor so near-radar sections still reach ground
/// (operational RHI convention, documented divergence).
fn interp_profile_xs(prof: &[ProfileSample], z: f64, s: f64, policy: InterpPolicy) -> Option<f32> {
    let first = prof.first()?;
    let last = prof[prof.len() - 1];
    if z <= first.h {
        let extend = (first.r_m * HALF_BW_RAD).max(300.0);
        return (first.h - z <= extend).then_some(first.v);
    }
    if z >= last.h {
        let extend = last.r_m * HALF_BW_RAD;
        return (z - last.h <= extend).then_some(last.v);
    }
    let (_, theta_i) = invert_beam(s, z);
    for w in prof.windows(2) {
        let (lo, hi) = (w[0], w[1]);
        if z >= lo.h && z <= hi.h {
            let nearest = if (z - lo.h) <= (hi.h - z) { lo.v } else { hi.v };
            match policy {
                InterpPolicy::CcGuard if lo.v.min(hi.v) < 0.97 => return Some(nearest),
                InterpPolicy::VelocityGuard if (hi.v - lo.v).abs() > 30.0 => {
                    return Some(nearest);
                }
                _ => {}
            }
            let span = hi.theta_deg - lo.theta_deg;
            if span.abs() < 1e-6 {
                return Some(lo.v);
            }
            let w2 = ((theta_i - lo.theta_deg) / span).clamp(0.0, 1.0) as f32;
            return Some(lo.v + (hi.v - lo.v) * w2);
        }
    }
    Some(last.v)
}

/// Per-output-gate column at ground range `s`, azimuth `az`: (height_m, dbz)
/// pairs across all cuts, sorted ascending by height. Reused by all products.
fn column_profile(cols: &[CutColumn<'_>], az: f32, s: f64) -> Vec<(f64, f32)> {
    let mut prof: Vec<(f64, f32)> = cols
        .iter()
        .filter_map(|c| c.sample(az, s).map(|(dbz, h)| (h, dbz)))
        .collect();
    prof.sort_by(|a, b| a.0.total_cmp(&b.0));
    prof
}

/// Composite (column-max) reflectivity, on the base tilt geometry.
pub fn composite_reflectivity_grid(volume: &RadarVolume) -> Option<MomentGrid> {
    let (base_idx, base_grid) = base_reflectivity_cut(volume)?;
    let base = CutColumn::new(volume, base_idx, base_grid)?;
    let cols = reflectivity_columns(volume);
    let rows = base_grid.radial_indices.len();
    let gates = base_grid.gate_range.gate_count;
    let mut out = vec![f32::NAN; rows * gates];
    let row_az = base.row_azimuths(rows);
    // Parallel row/gate column walk (rows are independent).
    out.par_chunks_mut(gates)
        .enumerate()
        .for_each(|(row, out_row)| {
            let az = row_az[row];
            if !az.is_finite() {
                return;
            }
            for (gate, cell) in out_row.iter_mut().enumerate() {
                let s = base.ground_range_m[gate];
                let mut max_dbz = f32::NEG_INFINITY;
                for (_, dbz) in column_profile(&cols, az, s) {
                    if dbz > max_dbz {
                        max_dbz = dbz;
                    }
                }
                if max_dbz.is_finite() {
                    *cell = max_dbz;
                }
            }
        });
    Some(f32_grid_like(base_grid, MomentType::Reflectivity, out))
}

/// Echo-top height (metres above radar) of the highest tilt with Z ≥ threshold.
pub fn echo_top_grid(volume: &RadarVolume, threshold_dbz: f32) -> Option<MomentGrid> {
    let (base_idx, base_grid) = base_reflectivity_cut(volume)?;
    let base = CutColumn::new(volume, base_idx, base_grid)?;
    let cols = reflectivity_columns(volume);
    let rows = base_grid.radial_indices.len();
    let gates = base_grid.gate_range.gate_count;
    let mut out = vec![f32::NAN; rows * gates];
    let row_az = base.row_azimuths(rows);
    out.par_chunks_mut(gates)
        .enumerate()
        .for_each(|(row, out_row)| {
            let az = row_az[row];
            if !az.is_finite() {
                return;
            }
            for (gate, cell) in out_row.iter_mut().enumerate() {
                let s = base.ground_range_m[gate];
                let prof = column_profile(&cols, az, s);
                // highest height whose dbz >= threshold
                let top = prof
                    .iter()
                    .filter(|(_, dbz)| *dbz >= threshold_dbz)
                    .map(|(h, _)| *h)
                    .fold(f64::NEG_INFINITY, f64::max);
                if top.is_finite() {
                    *cell = top as f32;
                }
            }
        });
    Some(f32_grid_like(base_grid, MomentType::Reflectivity, out))
}

/// Convert reflectivity factor in dBZ to linear (mm^6 m^-3).
#[inline]
fn dbz_to_z(dbz: f32) -> f64 {
    10f64.powf(dbz as f64 / 10.0)
}

/// Vertically Integrated Liquid (kg m^-2), Greene & Clark (1972) with the
/// 56 dBZ hail cap (Witt et al. 1998). Returns (VIL grid, echo-top grid) so
/// callers can also derive VIL density without a second column walk.
pub fn vil_grid(volume: &RadarVolume) -> Option<MomentGrid> {
    let (base_idx, base_grid) = base_reflectivity_cut(volume)?;
    let base = CutColumn::new(volume, base_idx, base_grid)?;
    let cols = reflectivity_columns(volume);
    let rows = base_grid.radial_indices.len();
    let gates = base_grid.gate_range.gate_count;
    let mut out = vec![f32::NAN; rows * gates];
    let row_az = base.row_azimuths(rows);
    // VIL = Σ 3.44e-6 * Zbar^(4/7) * Δh ; cap reflectivity at hail cap.
    let vil_inc = |z_lin: f64, dh: f64| 3.44e-6 * z_lin.powf(4.0 / 7.0) * dh;
    out.par_chunks_mut(gates)
        .enumerate()
        .for_each(|(row, out_row)| {
            let az = row_az[row];
            if !az.is_finite() {
                return;
            }
            for (gate, cell) in out_row.iter_mut().enumerate() {
                let s = base.ground_range_m[gate];
                let prof = column_profile(&cols, az, s);
                if prof.is_empty() {
                    continue;
                }
                let mut vil = 0.0f64;
                // Surface layer: the lowest beam represents the column down to the
                // ground (operational convention; Greene & Clark 1972, Witt 1998),
                // so a single deep tilt still contributes rather than reporting 0.
                let (h0, z0) = prof[0];
                if h0 > 0.0 {
                    vil += vil_inc(dbz_to_z(z0.min(VIL_HAIL_CAP_DBZ)), h0);
                }
                for w in prof.windows(2) {
                    let (ha, za) = w[0];
                    let (hb, zb) = w[1];
                    let dh = (hb - ha).max(0.0);
                    let za_c = dbz_to_z(za.min(VIL_HAIL_CAP_DBZ));
                    let zb_c = dbz_to_z(zb.min(VIL_HAIL_CAP_DBZ));
                    vil += vil_inc(0.5 * (za_c + zb_c), dh);
                }
                if vil > 0.0 {
                    *cell = vil as f32;
                }
            }
        });
    Some(f32_grid_like(base_grid, MomentType::Reflectivity, out))
}

/// Maximum Expected Hail Size (mm) from the Severe Hail Index — the WSR-88D
/// Hail Detection Algorithm of Witt et al. 1998 (WAF 13(2), 286-303):
/// hail kinetic-energy flux  E = 5e-6 * 10^(0.084*Z) * W(Z)  with W(Z) a
/// linear ramp over 40-50 dBZ, height-weighted by a thermal ramp W_T(H)
/// between the melting level H0 and the -20C level, integrated upward:
/// SHI = 0.1 * sum W_T(H) * E * dH ;  MEHS = 2.54 * sqrt(SHI) (mm).
/// `freezing_level_m` / `minus20c_level_m` are heights above the RADAR (set
/// them from a sounding for best results; mid-latitude warm-season defaults
/// are roughly 3200 m / 6400 m).
/// MESH calibration: which SHI->size fit to apply.
///
/// References (constants adversarially verified against the corrigendum and
/// the pyhail reference implementation — see docs/hail-wind-algo-spec.md):
/// - Witt et al. 1998, Wea. Forecasting 13, 286-303
///   (doi:10.1175/1520-0434(1998)013<0286:AEHDAF>2.0.CO;2): MESH = 2.54*SHI^0.5.
///   Still what operational MRMS ships (Smith et al. 2016, BAMS 97).
/// - Murillo & Homeyer 2019, J. Appl. Meteor. Climatol. 58, 947-970
///   (doi:10.1175/JAMC-D-18-0247.1) refit on ~5,954 reports — WITH THE 2021
///   CORRIGENDUM COEFFICIENTS (doi:10.1175/JAMC-D-20-0271.1; the 2019 paper
///   text printed wrong values): P75 = 15.096*SHI^0.206, P95 = 22.157*SHI^0.212.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MeshCalibration {
    Witt1998,
    MurilloHomeyer2019P75,
    MurilloHomeyer2019P95,
}

impl MeshCalibration {
    #[inline]
    pub fn mesh_mm(self, shi: f64) -> f64 {
        match self {
            Self::Witt1998 => 2.54 * shi.sqrt(),
            Self::MurilloHomeyer2019P75 => 15.096 * shi.powf(0.206),
            Self::MurilloHomeyer2019P95 => 22.157 * shi.powf(0.212),
        }
    }
}

/// SHI + MESH + POSH in one column walk (Witt et al. 1998 Hail Detection
/// Algorithm). `mehs_grid` remains as the Witt-calibrated MESH wrapper.
pub struct HailGrids {
    /// Severe Hail Index, J m^-1 s^-1.
    pub shi: MomentGrid,
    /// Maximum Estimated Size of Hail, mm (per `MeshCalibration`).
    pub mesh_mm: MomentGrid,
    /// Probability of Severe Hail, percent (continuous 0-100; Witt's
    /// warning threshold WT = max(57.5*H0_km - 121, 20), POSH =
    /// 29*ln(SHI/WT) + 50 — SHI == WT gives exactly 50%).
    pub posh_pct: MomentGrid,
}

pub fn hail_grids(
    volume: &RadarVolume,
    freezing_level_m: f32,
    minus20c_level_m: f32,
    calibration: MeshCalibration,
) -> Option<HailGrids> {
    let (base_idx, base_grid) = base_reflectivity_cut(volume)?;
    let base = CutColumn::new(volume, base_idx, base_grid)?;
    let cols = reflectivity_columns(volume);
    let rows = base_grid.radial_indices.len();
    let gates = base_grid.gate_range.gate_count;
    let mut shi_out = vec![f32::NAN; rows * gates];
    let mut mesh_out = vec![f32::NAN; rows * gates];
    let mut posh_out = vec![f32::NAN; rows * gates];
    let row_az = base.row_azimuths(rows);
    let h0 = freezing_level_m.max(0.0) as f64;
    let hm20 = (minus20c_level_m.max(freezing_level_m + 1.0)) as f64;
    // POSH warning threshold (Witt 1998; floor of 20 per the operational
    // WSR-88D documentation).
    let wt_thresh = (57.5 * h0 / 1000.0 - 121.0).max(20.0);
    let ke_flux = |dbz: f64| -> f64 {
        let w = ((dbz - 40.0) / 10.0).clamp(0.0, 1.0);
        if w <= 0.0 {
            0.0
        } else {
            5.0e-6 * 10f64.powf(0.084 * dbz) * w
        }
    };
    let wt = |h: f64| ((h - h0) / (hm20 - h0)).clamp(0.0, 1.0);
    shi_out
        .par_chunks_mut(gates)
        .zip(mesh_out.par_chunks_mut(gates))
        .zip(posh_out.par_chunks_mut(gates))
        .enumerate()
        .for_each(|(row, ((shi_row, mesh_row), posh_row))| {
            let az = row_az[row];
            if !az.is_finite() {
                return;
            }
            for gate in 0..gates {
                let s = base.ground_range_m[gate];
                let prof = column_profile(&cols, az, s);
                if prof.len() < 2 {
                    continue;
                }
                let mut shi = 0.0f64;
                for w in prof.windows(2) {
                    let (ha, za) = w[0];
                    let (hb, zb) = w[1];
                    let dh = (hb - ha).max(0.0);
                    if hb <= h0 || dh <= 0.0 {
                        continue;
                    }
                    let mid_h = 0.5 * (ha + hb);
                    let mid_e = 0.5 * (ke_flux(za as f64) + ke_flux(zb as f64));
                    shi += wt(mid_h) * mid_e * dh;
                }
                shi *= 0.1;
                if shi > 1.0 {
                    shi_row[gate] = shi as f32;
                    mesh_row[gate] = calibration.mesh_mm(shi) as f32;
                    let posh = (29.0 * (shi / wt_thresh).ln() + 50.0).clamp(0.0, 100.0);
                    if posh > 0.0 {
                        posh_row[gate] = posh as f32;
                    }
                }
            }
        });
    Some(HailGrids {
        shi: f32_grid_like(base_grid, MomentType::Reflectivity, shi_out),
        mesh_mm: f32_grid_like(base_grid, MomentType::Reflectivity, mesh_out),
        posh_pct: f32_grid_like(base_grid, MomentType::Reflectivity, posh_out),
    })
}

/// POH — Probability of Hail (any size): the Waldvogel, Federer & Grimm
/// (1979, J. Appl. Meteor. 18, 1521-1525) hailpad-validated curve on the
/// height of the 45 dBZ echo top above the melting level. Linear
/// interpolation between the published table rows.
pub fn poh_grid(volume: &RadarVolume, freezing_level_m: f32) -> Option<MomentGrid> {
    const TABLE: [(f64, f64); 11] = [
        (1.65, 0.0),
        (1.80, 10.0),
        (1.97, 20.0),
        (2.17, 30.0),
        (2.40, 40.0),
        (2.70, 50.0),
        (3.07, 60.0),
        (3.55, 70.0),
        (4.20, 80.0),
        (5.00, 90.0),
        (5.80, 100.0),
    ];
    let et45 = echo_top_grid(volume, 45.0)?;
    let rows = et45.radial_count();
    let gates = et45.gate_range.gate_count;
    let mut out = vec![f32::NAN; rows * gates];
    let h0_km = freezing_level_m.max(0.0) as f64 / 1000.0;
    for row in 0..rows {
        for gate in 0..gates {
            let cell = &mut out[row * gates + gate];
            let Some(top_m) = et45.scaled_value(row, gate) else {
                continue;
            };
            if !top_m.is_finite() {
                continue;
            }
            let delta_km = top_m as f64 / 1000.0 - h0_km;
            if delta_km <= TABLE[0].0 {
                continue;
            }
            let poh = if delta_km >= TABLE[10].0 {
                100.0
            } else {
                let mut value = 0.0;
                for pair in TABLE.windows(2) {
                    let (d0, p0) = pair[0];
                    let (d1, p1) = pair[1];
                    if delta_km >= d0 && delta_km <= d1 {
                        value = p0 + (p1 - p0) * (delta_km - d0) / (d1 - d0);
                        break;
                    }
                }
                value
            };
            if poh > 0.0 {
                *cell = poh as f32;
            }
        }
    }
    Some(f32_grid_like(&et45, MomentType::Reflectivity, out))
}

pub fn mehs_grid(
    volume: &RadarVolume,
    freezing_level_m: f32,
    minus20c_level_m: f32,
) -> Option<MomentGrid> {
    let (base_idx, base_grid) = base_reflectivity_cut(volume)?;
    let base = CutColumn::new(volume, base_idx, base_grid)?;
    let cols = reflectivity_columns(volume);
    let rows = base_grid.radial_indices.len();
    let gates = base_grid.gate_range.gate_count;
    let mut out = vec![f32::NAN; rows * gates];
    let row_az = base.row_azimuths(rows);
    let h0 = freezing_level_m.max(0.0) as f64;
    let hm20 = (minus20c_level_m.max(freezing_level_m + 1.0)) as f64;
    // Hail KE flux with the 40-50 dBZ reflectivity ramp (Witt eq. 4-5).
    let ke_flux = |dbz: f64| -> f64 {
        let w = ((dbz - 40.0) / 10.0).clamp(0.0, 1.0);
        if w <= 0.0 {
            0.0
        } else {
            5.0e-6 * 10f64.powf(0.084 * dbz) * w
        }
    };
    // Thermal weight between the melting level and -20C (Witt eq. 7).
    let wt = |h: f64| ((h - h0) / (hm20 - h0)).clamp(0.0, 1.0);
    out.par_chunks_mut(gates)
        .enumerate()
        .for_each(|(row, out_row)| {
            let az = row_az[row];
            if !az.is_finite() {
                return;
            }
            for (gate, cell) in out_row.iter_mut().enumerate() {
                let s = base.ground_range_m[gate];
                let prof = column_profile(&cols, az, s);
                if prof.len() < 2 {
                    continue;
                }
                let mut shi = 0.0f64;
                for w in prof.windows(2) {
                    let (ha, za) = w[0];
                    let (hb, zb) = w[1];
                    let dh = (hb - ha).max(0.0);
                    if hb <= h0 || dh <= 0.0 {
                        continue;
                    }
                    let mid_h = 0.5 * (ha + hb);
                    let mid_e = 0.5 * (ke_flux(za as f64) + ke_flux(zb as f64));
                    shi += wt(mid_h) * mid_e * dh;
                }
                shi *= 0.1;
                if shi > 1.0 {
                    // MEHS in mm (Witt eq. 11).
                    *cell = (2.54 * shi.sqrt()) as f32;
                }
            }
        });
    Some(f32_grid_like(base_grid, MomentType::Reflectivity, out))
}

/// VIL Density (g m^-3) = VIL / echo-top depth — a depth-normalized large-hail
/// discriminator (values ≳ 3.5 g/m³ flag large hail far better than raw VIL;
/// Amburn & Wolf 1997, WAF 12(3)). Reuses the VIL and echo-top grids (same base
/// geometry); only computed where the echo top is meaningfully deep.
pub fn vil_density_grid(volume: &RadarVolume) -> Option<MomentGrid> {
    let vil = vil_grid(volume)?;
    let echo = echo_top_grid(volume, ECHO_TOP_THRESHOLD_DBZ)?;
    let rows = vil.radial_count();
    let gates = vil.gate_range.gate_count;
    let mut out = vec![f32::NAN; rows * gates];
    for row in 0..rows {
        for gate in 0..gates {
            let (Some(v), Some(h)) = (vil.scaled_value(row, gate), echo.scaled_value(row, gate))
            else {
                continue;
            };
            // Need a meaningful echo depth (>1.5 km) to avoid blow-ups.
            if v.is_finite() && h.is_finite() && h > 1_500.0 {
                out[row * gates + gate] = 1000.0 * v / h; // kg/m² ÷ m → g/m³
            }
        }
    }
    Some(MomentGrid {
        moment: MomentType::Reflectivity,
        gate_range: vil.gate_range.clone(),
        scale: 1.0,
        offset: 0.0,
        nodata: None,
        range_folded: None,
        radial_indices: vil.radial_indices.clone(),
        storage: MomentStorage::F32(out),
    })
}

/// A reconstructed vertical cross-section: `values[y * width + x]` in dBZ
/// (NaN = no data), with `y = 0` at `top_m` and `x = 0` at the start point.
pub struct CrossSection {
    pub width: usize,
    pub height: usize,
    pub top_m: f32,
    pub length_m: f32,
    pub values: Vec<f32>,
}

/// Reflectivity vertical cross-section between two ground points given as
/// (east_km, north_km) from the radar. Resamples every reflectivity tilt along
/// the path with 4/3-Earth beam geometry (Doviak & Zrnić 1993) and linearly
/// interpolates in height between tilt samples — the standard RHI-from-volume
/// reconstruction used to see BWER/vault, overhang and descending cores.
pub fn reflectivity_cross_section(
    volume: &RadarVolume,
    start_km: (f32, f32),
    end_km: (f32, f32),
    width: usize,
    height: usize,
    top_m: f32,
) -> Option<CrossSection> {
    let cols = reflectivity_columns(volume);
    cross_section_from_columns(
        &cols,
        start_km,
        end_km,
        width,
        height,
        top_m,
        InterpPolicy::LinearAngle,
    )
}

/// Cartesian box resample of the reflectivity volume for 3D direct
/// volume rendering: `n` cells per horizontal side over `±half_km` about
/// (center_east_km, center_north_km), `nz` levels 0..top_m. Returns
/// row-major \[z]\[y]\[x] values (NaN = no data), same MRMS-style
/// per-column reconstruction as the cross-sections.
pub fn volume_box_resample(
    volume: &RadarVolume,
    center_east_km: f32,
    center_north_km: f32,
    half_km: f32,
    n: usize,
    nz: usize,
    top_m: f32,
) -> Option<Vec<f32>> {
    if n < 8 || nz < 4 || half_km <= 1.0 {
        return None;
    }
    let cols = reflectivity_columns(volume);
    if cols.is_empty() {
        return None;
    }
    let mut out = vec![f32::NAN; n * n * nz];
    let slabs: Vec<Vec<f32>> = (0..n)
        .into_par_iter()
        .map(|yi| {
            let mut slab = vec![f32::NAN; n * nz];
            let north = center_north_km - half_km + 2.0 * half_km * yi as f32 / (n - 1) as f32;
            for xi in 0..n {
                let east = center_east_km - half_km + 2.0 * half_km * xi as f32 / (n - 1) as f32;
                let s = f64::from(east.hypot(north)) * 1000.0;
                let az = east.atan2(north).to_degrees().rem_euclid(360.0);
                let prof = column_profile_xs(&cols, az, s);
                if prof.is_empty() {
                    continue;
                }
                for zi in 0..nz {
                    let z = f64::from(top_m) * zi as f64 / (nz - 1) as f64;
                    if let Some(v) = interp_profile_xs(&prof, z, s, InterpPolicy::LinearAngle) {
                        slab[zi * n + xi] = v;
                    }
                }
            }
            slab
        })
        .collect();
    for (yi, slab) in slabs.iter().enumerate() {
        for zi in 0..nz {
            for xi in 0..n {
                out[zi * n * n + yi * n + xi] = slab[zi * n + xi];
            }
        }
    }
    Some(out)
}

/// Generic single-moment cross-section (CC, ZDR, …): same MRMS-style
/// reconstruction with the moment's interpolation policy.
#[allow(clippy::too_many_arguments)] // section geometry is irreducibly 6 values
pub fn moment_cross_section(
    volume: &RadarVolume,
    moment: MomentType,
    policy: InterpPolicy,
    start_km: (f32, f32),
    end_km: (f32, f32),
    width: usize,
    height: usize,
    top_m: f32,
) -> Option<CrossSection> {
    let mut cols: Vec<CutColumn<'_>> = volume
        .cuts
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            let g = c.moments.get(&moment)?;
            CutColumn::new(volume, i, g)
        })
        .collect();
    cols.sort_by(|a, b| a.elevation_deg.total_cmp(&b.elevation_deg));
    cross_section_from_columns(&cols, start_km, end_km, width, height, top_m, policy)
}

/// Dealiased-velocity vertical cross-section (m/s) — shows the RIJ descent /
/// downdraft and inflow/outflow vertical structure. Same RHI reconstruction as
/// the reflectivity section, but the columns sample each tilt's dealiased
/// velocity. NaN = no data.
pub fn velocity_cross_section(
    volume: &RadarVolume,
    start_km: (f32, f32),
    end_km: (f32, f32),
    width: usize,
    height: usize,
    top_m: f32,
) -> Option<CrossSection> {
    let mut cache = VolumeDealiasCache::new();
    velocity_cross_section_cached(volume, &mut cache, start_km, end_km, width, height, top_m)
}

/// Per-volume memo of every tilt's dealiased velocity. Dealiasing all tilts
/// costs ~100+ ms; an interactive endpoint drag recomputes the section every
/// frame, so the dealias must be paid ONCE per volume, not per frame.
pub struct VolumeDealiasCache {
    volume_ptr: usize,
    grids: Vec<(usize, MomentGrid)>,
}

impl VolumeDealiasCache {
    pub fn new() -> Self {
        Self {
            volume_ptr: 0,
            grids: Vec::new(),
        }
    }

    fn ensure(&mut self, volume: &RadarVolume) {
        let ptr = volume as *const RadarVolume as usize;
        if ptr == self.volume_ptr && !self.grids.is_empty() {
            return;
        }
        self.volume_ptr = ptr;
        self.grids = volume
            .cuts
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
                let v = c.moments.get(&MomentType::Velocity)?;
                Some((i, crate::dealias_velocity_grid(c, v)))
            })
            .collect();
    }
}

impl Default for VolumeDealiasCache {
    fn default() -> Self {
        Self::new()
    }
}

/// `velocity_cross_section` with a caller-held dealias memo — the fast path
/// for interactive section drags.
pub fn velocity_cross_section_cached(
    volume: &RadarVolume,
    cache: &mut VolumeDealiasCache,
    start_km: (f32, f32),
    end_km: (f32, f32),
    width: usize,
    height: usize,
    top_m: f32,
) -> Option<CrossSection> {
    cache.ensure(volume);
    let mut cols: Vec<CutColumn<'_>> = cache
        .grids
        .iter()
        .filter_map(|(i, g)| CutColumn::new(volume, *i, g))
        .collect();
    cols.sort_by(|a, b| a.elevation_deg.total_cmp(&b.elevation_deg));
    cross_section_from_columns(
        &cols,
        start_km,
        end_km,
        width,
        height,
        top_m,
        InterpPolicy::VelocityGuard,
    )
}

/// Shared RHI reconstruction: walk along the ground path, sample each column,
/// and interpolate in height. Works for any moment's columns.
fn cross_section_from_columns(
    cols: &[CutColumn<'_>],
    start_km: (f32, f32),
    end_km: (f32, f32),
    width: usize,
    height: usize,
    top_m: f32,
    policy: InterpPolicy,
) -> Option<CrossSection> {
    if width < 2 || height < 2 || top_m <= 0.0 || cols.is_empty() {
        return None;
    }
    let length_m = ((end_km.0 - start_km.0).hypot(end_km.1 - start_km.1) * 1000.0).max(0.0);
    // Columns are independent — compute them in parallel (keeps endpoint
    // drags fluid), then transpose into the row-major grid.
    let columns: Vec<Vec<f32>> = (0..width)
        .into_par_iter()
        .map(|x| {
            let f = x as f32 / (width - 1) as f32;
            let east = start_km.0 + (end_km.0 - start_km.0) * f;
            let north = start_km.1 + (end_km.1 - start_km.1) * f;
            let s = east.hypot(north) as f64 * 1000.0;
            let az = east.atan2(north).to_degrees().rem_euclid(360.0);
            let prof = column_profile_xs(cols, az, s);
            let mut column = vec![f32::NAN; height];
            if prof.is_empty() {
                return column;
            }
            for (y, cell) in column.iter_mut().enumerate() {
                let z = top_m * (1.0 - y as f32 / (height - 1) as f32);
                if let Some(v) = interp_profile_xs(&prof, z as f64, s, policy) {
                    *cell = v;
                }
            }
            column
        })
        .collect();
    let mut values = vec![f32::NAN; width * height];
    for (x, column) in columns.iter().enumerate() {
        for (y, v) in column.iter().enumerate() {
            values[y * width + x] = *v;
        }
    }
    // Path-sampling cleanup. Each column samples ONE nearest radial/gate, so
    // (a) some columns miss entirely (azimuth/gate gaps -> NaN stripes) and
    // (b) adjacent columns can disagree gate-to-gate ("barcode"). Two
    // NaN-aware passes fix both without touching heights: fill short gaps
    // (<= 2 columns) from horizontal neighbors, then a 3-tap blend — the same
    // smoothing every operational RHI display applies.
    let mut filled = values.clone();
    for y in 0..height {
        let row = y * width;
        for x in 0..width {
            if values[row + x].is_finite() {
                continue;
            }
            let mut sum = 0.0f32;
            let mut n = 0.0f32;
            for dx in [-2isize, -1, 1, 2] {
                let xi = x as isize + dx;
                if xi < 0 || xi >= width as isize {
                    continue;
                }
                let v = values[row + xi as usize];
                if v.is_finite() {
                    sum += v;
                    n += 1.0;
                }
            }
            if n >= 2.0 {
                filled[row + x] = sum / n;
            }
        }
    }
    let mut smoothed = filled.clone();
    for y in 0..height {
        let row = y * width;
        for x in 0..width {
            if !filled[row + x].is_finite() {
                continue;
            }
            let mut sum = 0.0f32;
            let mut n = 0.0f32;
            for dx in [-1isize, 0, 1] {
                let xi = x as isize + dx;
                if xi < 0 || xi >= width as isize {
                    continue;
                }
                let v = filled[row + xi as usize];
                if v.is_finite() {
                    sum += v;
                    n += 1.0;
                }
            }
            if n > 0.0 {
                smoothed[row + x] = sum / n;
            }
        }
    }
    Some(CrossSection {
        width,
        height,
        top_m,
        length_m,
        values: smoothed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use radar_core::{ElevationCut, GateRange, Radial};

    fn cut_with_ref(elev: f32, az_count: usize, gates: usize, dbz: f32) -> ElevationCut {
        let gate_range = GateRange {
            first_gate_m: 0,
            gate_spacing_m: 1_000,
            gate_count: gates,
        };
        let mut cut = ElevationCut::new(elev, None);
        for k in 0..az_count {
            cut.radials.push(Radial {
                azimuth_deg: k as f32 * (360.0 / az_count as f32),
                elevation_deg: elev,
                time_offset_ms: 0,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: None,
                radial_status: None,
            });
        }
        let grid = MomentGrid {
            moment: MomentType::Reflectivity,
            gate_range,
            scale: 1.0,
            offset: 0.0,
            nodata: None,
            range_folded: None,
            radial_indices: (0..az_count).collect(),
            storage: MomentStorage::F32(vec![dbz; az_count * gates]),
        };
        cut.moments.insert(MomentType::Reflectivity, grid);
        cut
    }

    fn volume_with(cuts: Vec<ElevationCut>) -> RadarVolume {
        RadarVolume {
            cuts,
            ..Default::default()
        }
    }

    #[test]
    fn composite_takes_column_max() {
        // low tilt 20 dBZ, high tilt 45 dBZ at same ground location.
        let v = volume_with(vec![
            cut_with_ref(0.5, 360, 60, 20.0),
            cut_with_ref(3.0, 360, 60, 45.0),
        ]);
        let comp = composite_reflectivity_grid(&v).expect("composite");
        // near range (gate 10 ~10km) both tilts overlap; max should be 45.
        let val = comp.scaled_value(0, 10).expect("val");
        assert!((val - 45.0).abs() < 0.6, "composite max was {val}");
    }

    #[test]
    fn echo_top_rises_with_higher_tilt() {
        let v = volume_with(vec![
            cut_with_ref(0.5, 360, 120, 30.0),
            cut_with_ref(5.0, 360, 120, 30.0),
        ]);
        let et = echo_top_grid(&v, ECHO_TOP_THRESHOLD_DBZ).expect("echo top");
        // at ~30 km ground range the 5° beam is far higher than the 0.5° beam.
        let h = et.scaled_value(0, 30).expect("h");
        assert!(h > 2000.0, "echo top height was {h} m");
    }

    #[test]
    fn derived_products_render_through_viewport_cache() {
        // End-to-end: compute each derived grid and render it through the same
        // ViewportMomentCache path the GUI worker uses, with its dedicated
        // color family. Asserts the render produces opaque pixels (no panic,
        // correct plumbing).
        use crate::{ViewportMomentCache, ViewportRasterOptions, viewport_rgba_buffer_len};
        use color_tables::{ColorTableFamily, ColorTableSet};

        let v = volume_with(vec![
            cut_with_ref(0.5, 360, 120, 45.0),
            cut_with_ref(3.0, 360, 120, 50.0),
        ]);
        let tables = ColorTableSet::default();
        let opts = ViewportRasterOptions {
            width: 256,
            height: 256,
            radar_x_px: 128.0,
            radar_y_px: 128.0,
            km_per_px_x: 1.0,
            km_per_px_y: 1.0,
            rotation_rad: 0.0,
        };
        let cases = [
            (
                composite_reflectivity_grid(&v),
                ColorTableFamily::Reflectivity,
            ),
            (
                echo_top_grid(&v, ECHO_TOP_THRESHOLD_DBZ),
                ColorTableFamily::EchoTops,
            ),
            (vil_grid(&v), ColorTableFamily::Vil),
        ];
        for (grid, family) in cases {
            let grid = grid.expect("derived grid");
            let cache = ViewportMomentCache::new_derived(&v, 0, grid, family, &tables)
                .expect("derived cache");
            let mut pixels = vec![0u8; viewport_rgba_buffer_len(opts)];
            cache
                .render_moment_rgba_into(&v, opts, &mut pixels)
                .expect("render");
            assert!(
                pixels.chunks_exact(4).any(|p| p[3] > 0),
                "{family:?} derived product rendered no opaque pixels"
            );
        }
    }

    #[test]
    fn cross_section_reconstructs_a_reflectivity_column() {
        // Two tilts of uniform 40 dBZ; a slice through them must report ~40 dBZ
        // in the height band the beams sample, and NaN well above the top beam.
        let v = volume_with(vec![
            cut_with_ref(0.5, 360, 200, 40.0),
            cut_with_ref(4.0, 360, 200, 40.0),
        ]);
        let xs = reflectivity_cross_section(&v, (10.0, 0.0), (60.0, 0.0), 120, 80, 18_000.0)
            .expect("cross section");
        assert_eq!(xs.values.len(), 120 * 80);
        // Some sampled cell must be ~40 dBZ.
        assert!(
            xs.values
                .iter()
                .any(|v| v.is_finite() && (*v - 40.0).abs() < 1.0),
            "no reconstructed reflectivity near 40 dBZ"
        );
        // The very top row (18 km) is above the 4° beam at these ranges -> NaN.
        assert!(
            xs.values[..120].iter().all(|v| v.is_nan()),
            "top of section should be empty above the highest beam"
        );
    }

    fn cut_with_vel(elev: f32, az_count: usize, gates: usize, vel: f32) -> ElevationCut {
        let gate_range = GateRange {
            first_gate_m: 0,
            gate_spacing_m: 1_000,
            gate_count: gates,
        };
        let mut cut = ElevationCut::new(elev, None);
        for k in 0..az_count {
            cut.radials.push(Radial {
                azimuth_deg: k as f32 * (360.0 / az_count as f32),
                elevation_deg: elev,
                time_offset_ms: 0,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: Some(60.0),
                radial_status: None,
            });
        }
        let grid = MomentGrid {
            moment: MomentType::Velocity,
            gate_range,
            scale: 1.0,
            offset: 0.0,
            nodata: None,
            range_folded: None,
            radial_indices: (0..az_count).collect(),
            storage: MomentStorage::F32(vec![vel; az_count * gates]),
        };
        cut.moments.insert(MomentType::Velocity, grid);
        cut
    }

    #[test]
    fn velocity_cross_section_reconstructs_velocity() {
        // Two velocity tilts of uniform +15 m/s (within Nyquist, so dealias is a
        // no-op); a slice through them must report ~+15 m/s where beams sample.
        let v = volume_with(vec![
            cut_with_vel(0.5, 360, 200, 15.0),
            cut_with_vel(4.0, 360, 200, 15.0),
        ]);
        let xs = velocity_cross_section(&v, (10.0, 0.0), (60.0, 0.0), 120, 80, 18_000.0)
            .expect("velocity cross section");
        assert_eq!(xs.values.len(), 120 * 80);
        assert!(
            xs.values
                .iter()
                .any(|v| v.is_finite() && (*v - 15.0).abs() < 1.0),
            "no reconstructed velocity near 15 m/s"
        );
    }

    #[test]
    fn derived_products_handle_degraded_inputs_without_panicking() {
        // Empty volume → None for everything.
        let empty = volume_with(vec![]);
        assert!(composite_reflectivity_grid(&empty).is_none());
        assert!(echo_top_grid(&empty, ECHO_TOP_THRESHOLD_DBZ).is_none());
        assert!(vil_grid(&empty).is_none());
        assert!(
            reflectivity_cross_section(&empty, (0.0, 0.0), (50.0, 0.0), 64, 32, 18_000.0).is_none()
        );
        assert!(
            velocity_cross_section(&empty, (0.0, 0.0), (50.0, 0.0), 64, 32, 18_000.0).is_none()
        );

        // Velocity-only volume → reflectivity products None, velocity XS Some.
        let vel_only = volume_with(vec![cut_with_vel(0.5, 360, 80, 12.0)]);
        assert!(composite_reflectivity_grid(&vel_only).is_none());
        assert!(
            reflectivity_cross_section(&vel_only, (0.0, 0.0), (50.0, 0.0), 64, 32, 18_000.0)
                .is_none()
        );
        assert!(
            velocity_cross_section(&vel_only, (0.0, 0.0), (50.0, 0.0), 64, 32, 18_000.0).is_some()
        );

        // Degenerate cross-section args → None (no panic).
        let v = volume_with(vec![cut_with_ref(0.5, 360, 80, 30.0)]);
        assert!(reflectivity_cross_section(&v, (0.0, 0.0), (50.0, 0.0), 1, 32, 18_000.0).is_none());
        assert!(reflectivity_cross_section(&v, (0.0, 0.0), (50.0, 0.0), 64, 32, 0.0).is_none());

        // All-NaN reflectivity column → finite products return empty grids, no panic.
        let nan = volume_with(vec![cut_with_ref(0.5, 360, 80, f32::NAN)]);
        let comp = composite_reflectivity_grid(&nan).expect("grid built");
        // No-data F32 cells read back as None or NaN; never a finite value.
        assert!((0..comp.radial_count()).all(|r| {
            (0..comp.gate_range.gate_count)
                .all(|g| comp.scaled_value(r, g).is_none_or(|v| v.is_nan()))
        }));
    }

    #[test]
    fn vil_positive_for_deep_reflectivity() {
        let v = volume_with(vec![
            cut_with_ref(0.5, 360, 120, 45.0),
            cut_with_ref(2.0, 360, 120, 45.0),
            cut_with_ref(5.0, 360, 120, 45.0),
        ]);
        let vil = vil_grid(&v).expect("vil");
        let val = vil.scaled_value(0, 30).expect("vil val");
        assert!(val > 0.0 && val < 80.0, "vil was {val} kg/m2");
    }

    #[test]
    fn mehs_flags_deep_intense_cores_only() {
        // A deep 60 dBZ column well above the melting level -> large hail
        // (MEHS comfortably over 25 mm / 1 inch); a 35 dBZ column -> nothing
        // (below the 40 dBZ KE-flux ramp).
        let hot = volume_with(vec![
            cut_with_ref(0.5, 360, 120, 60.0),
            cut_with_ref(4.0, 360, 120, 60.0),
            cut_with_ref(10.0, 360, 120, 60.0),
            cut_with_ref(19.5, 360, 120, 60.0),
        ]);
        let mehs = mehs_grid(&hot, 3200.0, 6400.0).expect("mehs");
        let v = mehs.scaled_value(0, 40).expect("value");
        assert!(v > 25.0 && v < 200.0, "MEHS was {v} mm");

        let weak = volume_with(vec![
            cut_with_ref(0.5, 360, 120, 35.0),
            cut_with_ref(4.0, 360, 120, 35.0),
        ]);
        let none = mehs_grid(&weak, 3200.0, 6400.0).expect("grid");
        assert!(
            none.scaled_value(0, 40).is_none_or(|v| v.is_nan()),
            "35 dBZ should produce no hail signal"
        );
    }

    #[test]
    fn vil_density_is_in_physical_range() {
        let v = volume_with(vec![
            cut_with_ref(0.5, 360, 120, 50.0),
            cut_with_ref(2.0, 360, 120, 50.0),
            cut_with_ref(5.0, 360, 120, 50.0),
        ]);
        let d = vil_density_grid(&v).expect("vil density");
        let val = d.scaled_value(0, 30).expect("density val");
        // g/m³; physical column densities are a few g/m³.
        assert!(val > 0.0 && val < 20.0, "vil density was {val} g/m3");
    }
}
