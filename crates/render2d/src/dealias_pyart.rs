//! BowEcho adapter for the optimized Region Global sweep solver.
//!
//! The scientific core lives in the pinned `region-global-dealias` crate. It
//! is a dependency-free Rust port of Py-ART's `dealias_region_based` with
//! fold-identical union-find, edge-map, and heap optimizations. Keeping the
//! solver in its standalone crate prevents this copy from drifting back into a
//! slower fork while this module retains BowEcho's `MomentGrid` conversion and
//! fixed-point output contract.

use radar_core::{ElevationCut, MomentGrid, MomentStorage, MomentType};
use region_global_dealias::solver::{RiftContext, RiftOptions, dealias_sweep_rift, region_folds};

use crate::{
    DEALIASED_VELOCITY_NODATA, DEALIASED_VELOCITY_OFFSET, DEALIASED_VELOCITY_SCALE,
    copy_scaled_velocity_row, encode_dealiased_velocity, median_nyquist_mps, radial_azimuths,
    row_nyquist_mps, sweep_wraps,
};

pub fn dealias_velocity_grid_pyart_region(cut: &ElevationCut, source: &MomentGrid) -> MomentGrid {
    let (rows, gates, nyq, observed, azimuths) = sweep_inputs(cut, source);
    let folds = region_folds(&observed, &nyq, rows, gates, sweep_wraps(&azimuths));
    encode_folded_grid(source, &observed, &nyq, &folds, rows, gates)
}

/// Run region-global with the v0.2 RIFT gate-resolution refinement enabled.
///
/// RIFT is deliberately a separate, opt-in engine: it preserves the ordinary
/// region-global result as its baseline, then may make conservative local
/// corrections where an independent signed-couplet trigger and wrapped-vortex
/// fit agree. BowEcho currently supplies physical gate geometry but no
/// temporal, vertical, or environmental reference fields. If the feed does
/// not provide valid physical gate geometry, this falls back exactly to the
/// ordinary region-global adapter.
pub fn dealias_velocity_grid_region_global_rift(
    cut: &ElevationCut,
    source: &MomentGrid,
) -> MomentGrid {
    let (rows, gates, nyq, observed, azimuths) = sweep_inputs(cut, source);
    let first_gate_m = source.gate_range.first_gate_m as f32;
    let gate_spacing_m = source.gate_range.gate_spacing_m as f32;
    let refined = (first_gate_m.is_finite()
        && first_gate_m >= 0.0
        && gate_spacing_m.is_finite()
        && gate_spacing_m > 0.0)
        .then(|| {
            dealias_sweep_rift(
                &observed,
                &nyq,
                rows,
                gates,
                &azimuths,
                &RiftContext::default(),
                RiftOptions {
                    first_gate_m,
                    gate_spacing_m,
                    automatic_single_sweep: true,
                    ..RiftOptions::default()
                },
            )
        })
        .and_then(Result::ok);

    if let Some(refined) = refined {
        encode_unfolded_grid(source, &refined.velocity)
    } else {
        let folds = region_folds(&observed, &nyq, rows, gates, sweep_wraps(&azimuths));
        encode_folded_grid(source, &observed, &nyq, &folds, rows, gates)
    }
}

fn sweep_inputs(
    cut: &ElevationCut,
    source: &MomentGrid,
) -> (usize, usize, Vec<f32>, Vec<f32>, Vec<f32>) {
    let rows = source.radial_count();
    let gates = source.gate_range.gate_count;
    let total = rows.saturating_mul(gates);
    let fallback_nyquist = median_nyquist_mps(cut, source);

    let mut nyq = vec![f32::NAN; rows.max(1)];
    for (row, slot) in nyq.iter_mut().enumerate().take(rows) {
        *slot = row_nyquist_mps(cut, source, row)
            .or(fallback_nyquist)
            .filter(|value| value.is_finite() && *value > 0.0)
            .unwrap_or(f32::NAN);
    }

    let mut observed = vec![f32::NAN; total];
    if total > 0 {
        let mut row_buf = vec![f32::NAN; gates];
        for row in 0..rows {
            copy_scaled_velocity_row(source, row, &mut row_buf);
            observed[row * gates..(row + 1) * gates].copy_from_slice(&row_buf);
        }
    }

    (rows, gates, nyq, observed, radial_azimuths(cut, source))
}

fn encode_folded_grid(
    source: &MomentGrid,
    observed: &[f32],
    nyq: &[f32],
    folds: &[i32],
    rows: usize,
    gates: usize,
) -> MomentGrid {
    let mut unfolded = vec![f32::NAN; rows.saturating_mul(gates)];
    for (row, &n) in nyq.iter().enumerate().take(rows) {
        for gate in 0..gates {
            let idx = row * gates + gate;
            let value = observed[idx];
            if !value.is_finite() {
                continue;
            }
            unfolded[idx] = if n.is_finite() && n > 0.0 {
                value + 2.0 * n * folds[idx] as f32
            } else {
                value
            };
        }
    }
    encode_unfolded_grid(source, &unfolded)
}

fn encode_unfolded_grid(source: &MomentGrid, unfolded: &[f32]) -> MomentGrid {
    let corrected = unfolded
        .iter()
        .map(|&value| {
            if value.is_finite() {
                encode_dealiased_velocity(value)
            } else {
                DEALIASED_VELOCITY_NODATA
            }
        })
        .collect();

    MomentGrid {
        moment: MomentType::Velocity,
        gate_range: source.gate_range.clone(),
        scale: DEALIASED_VELOCITY_SCALE,
        offset: DEALIASED_VELOCITY_OFFSET,
        nodata: Some(DEALIASED_VELOCITY_NODATA),
        range_folded: None,
        radial_indices: source.radial_indices.clone(),
        storage: MomentStorage::U16(corrected),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use radar_core::{GateRange, Radial};

    fn quiet_sweep(gate_spacing_m: i32) -> (ElevationCut, MomentGrid) {
        let rows = 8;
        let gates = 6;
        let gate_range = GateRange {
            first_gate_m: 1_000,
            gate_spacing_m,
            gate_count: gates,
        };
        let mut cut = ElevationCut::new(0.5, Some(1));
        for row in 0..rows {
            cut.radials.push(Radial {
                azimuth_deg: row as f32 * 45.0,
                elevation_deg: 0.5,
                time_offset_ms: 0,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: Some(25.0),
                radial_status: None,
            });
        }
        let source = MomentGrid {
            moment: MomentType::Velocity,
            gate_range,
            scale: 1.0,
            offset: 0.0,
            nodata: None,
            range_folded: None,
            radial_indices: (0..rows).collect(),
            storage: MomentStorage::F32(vec![4.0; rows * gates]),
        };
        (cut, source)
    }

    #[test]
    fn rift_preserves_region_global_when_no_refinement_is_authorized() {
        let (cut, source) = quiet_sweep(250);
        let baseline = dealias_velocity_grid_pyart_region(&cut, &source);
        let rift = dealias_velocity_grid_region_global_rift(&cut, &source);

        assert_eq!(rift, baseline);
        assert_eq!(rift.gate_range, source.gate_range);
        assert_eq!(rift.radial_indices, source.radial_indices);
    }

    #[test]
    fn rift_falls_back_exactly_when_gate_geometry_is_invalid() {
        let (cut, source) = quiet_sweep(0);
        assert_eq!(
            dealias_velocity_grid_region_global_rift(&cut, &source),
            dealias_velocity_grid_pyart_region(&cut, &source)
        );
    }
}
