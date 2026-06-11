//! Damaging-wind products (verified spec: docs/hail-wind-algo-spec.md).
//!
//! MARC — Mid-Altitude Radial Convergence (Schmocker, Przybylinski & Lin
//! 1996, 15th Conf. Wea. Analysis & Forecasting, 306–311; Przybylinski 1995,
//! Wea. Forecasting 10, 203–218): the velocity difference between the maximum
//! inbound and maximum outbound within ~6 km along a single radial, in the
//! 3–7 km layer. ΔV ≥ 25 m/s (50 kt), persistent and deep, precedes damaging
//! surface winds in bow echoes/QLCS by 15–20 min (NWS operational guidance).
//! Caveat from the literature: the signature is masked where mid-level flow
//! runs normal to the beam — the display is a *precursor aid*, not truth.
//!
//! Low-level gust proxy — Smith, Elmore & Dulin (2004, Wea. Forecasting 19,
//! 240–250): radar radial wind observed with the beam centerline below ~1 km
//! maps ≈1:1 to a surface gust in NWS research practice (≥25 m/s ≈ severe).
//! The product is |dealiased Vr| on the lowest velocity tilt, masked to
//! beam heights < 1 km above the radar.

use crate::dealias_velocity_grid;
use radar_core::{
    MomentGrid, MomentType, RadarVolume, beam_ground_range_m, beam_height_above_radar_m,
};
use rayon::prelude::*;

/// MARC layer bounds (m above radar) — Schmocker et al. 1996 / NWS LMK.
const MARC_LAYER_BOTTOM_M: f64 = 3000.0;
const MARC_LAYER_TOP_M: f64 = 7000.0;
/// Along-radial search half-window, meters. [ENG] 3 km each side of the
/// gate keeps the max inbound–outbound pair separation ≤ 6 km, matching the
/// published "within 6 km along a single radial" definition.
const MARC_HALF_WINDOW_M: f64 = 3000.0;

/// Sliding window max-inbound-vs-max-outbound convergence per gate.
/// Convergent orientation: outbound (positive Vr) NEARER the radar than
/// inbound (negative Vr) — i.e. ΔV = max(V near) − min(V far) > 0.
/// 3-gate median along the radial — kills single-gate dealias spikes that
/// would otherwise fabricate enormous ΔV (observed: 138 m/s on the KEAX
/// derecho from one bad gate pair). NaN-tolerant: needs 2 finite of 3.
fn median3(values: &[f32]) -> Vec<f32> {
    let n = values.len();
    let mut out = vec![f32::NAN; n];
    for (g, cell) in out.iter_mut().enumerate() {
        let mut window: Vec<f32> = (g.saturating_sub(1)..=(g + 1).min(n - 1))
            .map(|i| values[i])
            .filter(|v| v.is_finite())
            .collect();
        if window.len() >= 2 {
            window.sort_by(f32::total_cmp);
            *cell = window[window.len() / 2];
        }
    }
    out
}

fn radial_convergence_row(values: &[f32], half_window_gates: usize) -> Vec<f32> {
    let values = median3(values);
    let n = values.len();
    let mut out = vec![f32::NAN; n];
    if n == 0 {
        return out;
    }
    for g in 0..n {
        let near_start = g.saturating_sub(half_window_gates);
        let far_end = (g + half_window_gates).min(n - 1);
        let mut near_max = f32::NEG_INFINITY;
        for &v in &values[near_start..=g] {
            if v.is_finite() && v > near_max {
                near_max = v;
            }
        }
        let mut far_min = f32::INFINITY;
        for &v in &values[g..=far_end] {
            if v.is_finite() && v < far_min {
                far_min = v;
            }
        }
        if near_max.is_finite() && far_min.is_finite() {
            let delta = near_max - far_min;
            // [ENG] ΔV > 70 m/s exceeds anything in the MARC literature
            // (Funk et al. case max ≈ 38) — at that magnitude it is a
            // residual-fold artifact, not meteorology. Reject, don't cap.
            if delta > 0.0 && delta <= 70.0 {
                out[g] = delta;
            }
        }
    }
    out
}

/// One velocity cut prepared for the MARC composite.
struct VelCut {
    elevation_deg: f32,
    az_rows: Vec<(f32, usize)>,
    conv: Vec<f32>, // rows x gates ΔV field
    gates: usize,
    first_gate_m: f64,
    gate_spacing_m: f64,
}

impl VelCut {
    fn nearest_row(&self, az: f32) -> Option<usize> {
        if self.az_rows.is_empty() {
            return None;
        }
        let idx = self
            .az_rows
            .partition_point(|(a, _)| *a < az)
            .min(self.az_rows.len() - 1);
        let after = self.az_rows[idx];
        let before = self.az_rows[idx.saturating_sub(1)];
        let pick = |c: (f32, usize)| {
            let mut d = (c.0 - az).abs();
            if d > 180.0 {
                d = 360.0 - d;
            }
            (d, c.1)
        };
        let (da, ra) = pick(after);
        let (db, rb) = pick(before);
        let (d, row) = if da <= db { (da, ra) } else { (db, rb) };
        // ~1.5 beamwidths max — beyond that the radial doesn't cover az.
        (d <= 1.5).then_some(row)
    }
}

fn velocity_cuts(volume: &RadarVolume) -> Vec<VelCut> {
    let mut cuts: Vec<VelCut> = volume
        .cuts
        .iter()
        .filter_map(|cut| {
            let velocity = cut.moments.get(&MomentType::Velocity)?;
            let dealiased = dealias_velocity_grid(cut, velocity);
            let gr = dealiased.gate_range.clone();
            let rows = dealiased.radial_count();
            if gr.gate_count == 0 || rows == 0 {
                return None;
            }
            let half_gates =
                ((MARC_HALF_WINDOW_M / gr.gate_spacing_m as f64).round() as usize).max(2);
            let mut az_rows: Vec<(f32, usize)> = dealiased
                .radial_indices
                .iter()
                .enumerate()
                .filter_map(|(row, ri)| {
                    let az = cut.radials.get(*ri)?.azimuth_deg.rem_euclid(360.0);
                    Some((az, row))
                })
                .collect();
            az_rows.sort_by(|a, b| a.0.total_cmp(&b.0));
            // Per-row convergence (parallel over rows).
            let gates = gr.gate_count;
            let mut row_values = vec![f32::NAN; rows * gates];
            for row in 0..rows {
                for gate in 0..gates {
                    if let Some(v) = dealiased.scaled_value(row, gate) {
                        row_values[row * gates + gate] = v;
                    }
                }
            }
            let conv: Vec<f32> = row_values
                .par_chunks(gates)
                .flat_map_iter(|row| radial_convergence_row(row, half_gates))
                .collect();
            Some(VelCut {
                elevation_deg: cut.elevation_deg,
                az_rows,
                conv,
                gates,
                first_gate_m: gr.first_gate_m as f64,
                gate_spacing_m: gr.gate_spacing_m as f64,
            })
        })
        .collect();
    cuts.sort_by(|a, b| a.elevation_deg.total_cmp(&b.elevation_deg));
    // SAILS de-dupe: keep the first cut at each elevation (within 0.1°).
    cuts.dedup_by(|b, a| (a.elevation_deg - b.elevation_deg).abs() < 0.1);
    cuts
}

/// MARC ΔV composite (m/s): the max windowed radial convergence across all
/// velocity tilts whose beam centers the 3–7 km layer at that ground range.
/// Display guidance: ≥ 25 m/s is the published damaging-wind precursor.
pub fn marc_grid(volume: &RadarVolume) -> Option<MomentGrid> {
    let cuts = velocity_cuts(volume);
    if cuts.is_empty() {
        return None;
    }
    // Output geometry: the lowest velocity cut's grid.
    let (base_idx, base_grid) = volume
        .cuts
        .iter()
        .enumerate()
        .find_map(|(i, c)| c.moments.get(&MomentType::Velocity).map(|grid| (i, grid)))?;
    let base_cut = volume.cuts.get(base_idx)?;
    let rows = base_grid.radial_count();
    let gates = base_grid.gate_range.gate_count;
    let base_gr = &base_grid.gate_range;
    let base_elev = base_cut.elevation_deg as f64;
    let row_az: Vec<f32> = (0..rows)
        .map(|r| {
            base_grid
                .radial_indices
                .get(r)
                .and_then(|ri| base_cut.radials.get(*ri))
                .map(|radial| radial.azimuth_deg.rem_euclid(360.0))
                .unwrap_or(f32::NAN)
        })
        .collect();
    let mut out = vec![f32::NAN; rows * gates];
    out.par_chunks_mut(gates)
        .enumerate()
        .for_each(|(row, out_row)| {
            let az = row_az[row];
            if !az.is_finite() {
                return;
            }
            for (gate, cell) in out_row.iter_mut().enumerate() {
                let slant =
                    base_gr.first_gate_m as f64 + gate as f64 * base_gr.gate_spacing_m as f64;
                let ground = beam_ground_range_m(slant, base_elev);
                let mut best = f32::NAN;
                for cut in &cuts {
                    // Gate at this ground range on this tilt (slant ≈ ground
                    // at these elevations; refine via the inverse map).
                    let cut_gate =
                        ((ground - cut.first_gate_m) / cut.gate_spacing_m).round() as isize;
                    if cut_gate < 0 || cut_gate as usize >= cut.gates {
                        continue;
                    }
                    let cut_gate = cut_gate as usize;
                    let cut_slant = cut.first_gate_m + cut_gate as f64 * cut.gate_spacing_m;
                    let height = beam_height_above_radar_m(cut_slant, cut.elevation_deg as f64);
                    if !(MARC_LAYER_BOTTOM_M..=MARC_LAYER_TOP_M).contains(&height) {
                        continue;
                    }
                    let Some(cut_row) = cut.nearest_row(az) else {
                        continue;
                    };
                    let delta = cut.conv[cut_row * cut.gates + cut_gate];
                    if delta.is_finite() && (!best.is_finite() || delta > best) {
                        best = delta;
                    }
                }
                if best.is_finite() {
                    *cell = best;
                }
            }
        });
    Some(crate::volumetric::f32_grid_like_pub(
        base_grid,
        MomentType::Velocity,
        out,
    ))
}

/// Low-level gust proxy (m/s): |dealiased Vr| on the lowest velocity tilt,
/// masked to beam-center heights < 1 km above the radar (Smith, Elmore &
/// Dulin 2004: low-beam radial wind ≈ surface gust; ≥ 25 m/s ≈ severe).
pub fn gust_proxy_grid(volume: &RadarVolume) -> Option<MomentGrid> {
    let (cut, velocity) = volume
        .cuts
        .iter()
        .find_map(|c| c.moments.get(&MomentType::Velocity).map(|grid| (c, grid)))?;
    let dealiased = dealias_velocity_grid(cut, velocity);
    // Reflectivity-support mask: a gust claim needs an echo. Bird/insect
    // and clutter returns in clear air otherwise fabricate severe gusts
    // (observed: 89 m/s "gusts" on an echo-free volume).
    let reflectivity = cut.moments.get(&MomentType::Reflectivity);
    let rows = dealiased.radial_count();
    let gates = dealiased.gate_range.gate_count;
    let gr = &dealiased.gate_range;
    let elev = cut.elevation_deg as f64;
    let mut out = vec![f32::NAN; rows * gates];
    for row in 0..rows {
        let raw: Vec<f32> = (0..gates)
            .map(|gate| dealiased.scaled_value(row, gate).unwrap_or(f32::NAN))
            .collect();
        let filtered = median3(&raw);
        for (gate, &v) in filtered.iter().enumerate() {
            let slant = gr.first_gate_m as f64 + gate as f64 * gr.gate_spacing_m as f64;
            if beam_height_above_radar_m(slant, elev) >= 1000.0 {
                // Past this range the lowest beam overshoots the surface
                // layer — an honest product stops rather than extrapolates.
                break;
            }
            if let Some(ref_grid) = reflectivity {
                // REF gates are coarser (1 km vs 0.25 km) — map by range.
                let ref_gr = &ref_grid.gate_range;
                let ref_gate = ((slant - ref_gr.first_gate_m as f64) / ref_gr.gate_spacing_m as f64)
                    .round() as isize;
                let supported = ref_gate >= 0
                    && (ref_gate as usize) < ref_gr.gate_count
                    && ref_grid
                        .scaled_value(
                            row.min(ref_grid.radial_count().saturating_sub(1)),
                            ref_gate as usize,
                        )
                        .map(|z| z >= 10.0)
                        .unwrap_or(false);
                if !supported {
                    continue;
                }
            }
            if v.is_finite() {
                out[row * gates + gate] = v.abs();
            }
        }
    }
    Some(crate::volumetric::f32_grid_like_pub(
        &dealiased,
        MomentType::Velocity,
        out,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convergence_window_finds_couplet() {
        // Outbound +20 near, inbound -15 far — 3 gates wide each (the
        // median QC by design suppresses single-gate spikes).
        let mut v = vec![f32::NAN; 40];
        v[8..=10].fill(20.0);
        v[14..=16].fill(-15.0);
        let conv = radial_convergence_row(&v, 12);
        // Between the pair the windowed ΔV sees both: 35 m/s.
        assert!((conv[12] - 35.0).abs() < 1e-3, "{}", conv[12]);
        // Divergent orientation (inbound near, outbound far) must NOT fire.
        let mut d = vec![f32::NAN; 40];
        d[8..=10].fill(-15.0);
        d[14..=16].fill(20.0);
        let div = radial_convergence_row(&d, 12);
        assert!(div[12].is_nan() || div[12] <= 0.0);
    }

    #[test]
    fn median_qc_suppresses_single_gate_spike() {
        // A lone +60 gate in a ±10 field must not fabricate ΔV.
        let mut v = vec![10.0f32; 40];
        v[20] = 60.0;
        let conv = radial_convergence_row(&v, 12);
        for value in conv.iter().filter(|value| value.is_finite()) {
            assert!(*value < 5.0, "{value}");
        }
    }
}
