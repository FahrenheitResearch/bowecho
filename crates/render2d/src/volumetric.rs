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

/// Linearly interpolate a height-sorted (height_m, dBZ) profile at height `z`.
/// Returns None outside the sampled span (no extrapolation).
/// How far below the lowest beam a section column may be extended (m).
/// Near the radar the lowest tilt sits a few hundred metres up, so sections
/// reach the ground; at far range we stop after this depth rather than paint
/// kilometres of single-gate columns ("barcode" artifact).
const PROFILE_GROUND_EXTENSION_M: f64 = 1_500.0;

fn interp_profile(prof: &[(f64, f32)], z: f64) -> Option<f32> {
    let first = prof.first()?;
    // Below the lowest beam, extend its value downward a bounded distance —
    // the display convention used by operational RHI views (and the same
    // surface-layer assumption VIL makes), depth-capped to stay honest.
    if z <= first.0 {
        return (first.0 - z <= PROFILE_GROUND_EXTENSION_M).then_some(first.1);
    }
    if prof.len() < 2 || z > prof[prof.len() - 1].0 {
        return None;
    }
    for w in prof.windows(2) {
        let (h0, v0) = w[0];
        let (h1, v1) = w[1];
        if z >= h0 && z <= h1 {
            if (h1 - h0).abs() < 1e-6 {
                return Some(v0);
            }
            let t = ((z - h0) / (h1 - h0)) as f32;
            return Some(v0 + (v1 - v0) * t);
        }
    }
    Some(prof[prof.len() - 1].1)
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
    cross_section_from_columns(&cols, start_km, end_km, width, height, top_m)
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
    // Own the dealiased grids so the borrowing CutColumns can reference them.
    let owned: Vec<(usize, MomentGrid)> = volume
        .cuts
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            let v = c.moments.get(&MomentType::Velocity)?;
            Some((i, crate::dealias_velocity_grid(c, v)))
        })
        .collect();
    let mut cols: Vec<CutColumn<'_>> = owned
        .iter()
        .filter_map(|(i, g)| CutColumn::new(volume, *i, g))
        .collect();
    cols.sort_by(|a, b| a.elevation_deg.total_cmp(&b.elevation_deg));
    cross_section_from_columns(&cols, start_km, end_km, width, height, top_m)
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
            let prof = column_profile(cols, az, s);
            let mut column = vec![f32::NAN; height];
            if prof.is_empty() {
                return column;
            }
            for (y, cell) in column.iter_mut().enumerate() {
                let z = top_m * (1.0 - y as f32 / (height - 1) as f32);
                if let Some(v) = interp_profile(&prof, z as f64) {
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
        let mut v = RadarVolume::default();
        v.cuts = cuts;
        v
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
