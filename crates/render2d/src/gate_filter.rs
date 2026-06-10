//! Reflectivity gate filter (the GR2Analyst "GateFilter"): hide gates of a
//! non-reflectivity moment wherever the SAME CUT's reflectivity is below a
//! threshold — the standard declutter for clear-air noise on velocity and
//! dual-pol products. Applied once per (volume, cut, product) on the render
//! worker and cached; the per-frame fast path is untouched.

use radar_core::{ElevationCut, MomentGrid, MomentStorage, MomentType};

/// Filter `grid` (any moment sharing `cut`'s radials) against the cut's
/// reflectivity: gates whose co-located REF is missing or below
/// `threshold_dbz` become empty. Gate spacings may differ (legacy VCPs mix
/// 250 m Doppler with 1000 m surveillance gates) — REF is sampled by true
/// range. Returns an F32 grid with identical geometry to the input.
pub fn apply_reflectivity_gate_filter(
    cut: &ElevationCut,
    grid: &MomentGrid,
    threshold_dbz: f32,
) -> MomentGrid {
    let rows = grid.radial_count();
    let gates = grid.gate_range.gate_count;
    let mut values = vec![f32::NAN; rows * gates];
    if let Some(reflectivity) = cut.moments.get(&MomentType::Reflectivity) {
        let ref_first = reflectivity.gate_range.first_gate_m as f64;
        let ref_spacing = reflectivity.gate_range.gate_spacing_m.max(1) as f64;
        let ref_gates = reflectivity.gate_range.gate_count;
        let own_first = grid.gate_range.first_gate_m as f64;
        let own_spacing = grid.gate_range.gate_spacing_m.max(1) as f64;
        // The two grids share the cut's radials but may index rows through
        // different radial_indices orderings; map by radial index.
        let mut ref_row_by_radial =
            vec![usize::MAX; cut.radials.len().max(reflectivity.radial_indices.len())];
        for (row, &radial) in reflectivity.radial_indices.iter().enumerate() {
            if radial < ref_row_by_radial.len() {
                ref_row_by_radial[radial] = row;
            }
        }
        for (row, &radial) in grid.radial_indices.iter().enumerate().take(rows) {
            let ref_row = ref_row_by_radial.get(radial).copied().unwrap_or(usize::MAX);
            if ref_row == usize::MAX {
                continue;
            }
            for gate in 0..gates {
                let Some(value) = grid.scaled_value(row, gate).filter(|v| v.is_finite()) else {
                    continue;
                };
                let range_m = own_first + gate as f64 * own_spacing;
                let ref_gate = ((range_m - ref_first) / ref_spacing).round();
                let passes = ref_gate >= 0.0
                    && (ref_gate as usize) < ref_gates
                    && reflectivity
                        .scaled_value(ref_row, ref_gate as usize)
                        .filter(|v| v.is_finite())
                        .is_some_and(|dbz| dbz >= threshold_dbz);
                if passes {
                    values[row * gates + gate] = value;
                }
            }
        }
    }
    MomentGrid {
        moment: grid.moment.clone(),
        gate_range: grid.gate_range.clone(),
        scale: 1.0,
        offset: 0.0,
        nodata: None,
        range_folded: None,
        radial_indices: grid.radial_indices.clone(),
        storage: MomentStorage::F32(values),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use radar_core::{GateRange, Radial};

    fn cut_with(ref_values: &[f32], vel_values: &[f32], gates: usize) -> ElevationCut {
        let rows = ref_values.len() / gates;
        let gate_range = GateRange {
            first_gate_m: 1000,
            gate_spacing_m: 250,
            gate_count: gates,
        };
        let mut cut = ElevationCut::new(0.5, None);
        for row in 0..rows {
            cut.radials.push(Radial {
                azimuth_deg: row as f32,
                elevation_deg: 0.5,
                time_offset_ms: 0,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: Some(28.0),
                radial_status: None,
            });
        }
        for (moment, data) in [
            (MomentType::Reflectivity, ref_values),
            (MomentType::Velocity, vel_values),
        ] {
            cut.moments.insert(
                moment.clone(),
                MomentGrid {
                    moment,
                    gate_range: gate_range.clone(),
                    scale: 1.0,
                    offset: 0.0,
                    nodata: None,
                    range_folded: None,
                    radial_indices: (0..rows).collect(),
                    storage: MomentStorage::F32(data.to_vec()),
                },
            );
        }
        cut
    }

    #[test]
    fn keeps_velocity_only_where_reflectivity_clears_the_threshold() {
        // 1 row x 4 gates: REF = [35, 5, NaN, 20]; threshold 10 dBZ.
        let cut = cut_with(&[35.0, 5.0, f32::NAN, 20.0], &[10.0, -12.0, 8.0, -20.0], 4);
        let vel = cut.moments.get(&MomentType::Velocity).unwrap();
        let filtered = apply_reflectivity_gate_filter(&cut, vel, 10.0);
        assert_eq!(filtered.scaled_value(0, 0), Some(10.0));
        assert!(filtered.scaled_value(0, 1).is_none_or(|v| v.is_nan()));
        assert!(filtered.scaled_value(0, 2).is_none_or(|v| v.is_nan()));
        assert_eq!(filtered.scaled_value(0, 3), Some(-20.0));
    }

    #[test]
    fn no_reflectivity_moment_blanks_everything() {
        let mut cut = cut_with(&[35.0, 35.0], &[10.0, -10.0], 2);
        cut.moments.remove(&MomentType::Reflectivity);
        let vel = cut.moments.get(&MomentType::Velocity).unwrap().clone();
        let filtered = apply_reflectivity_gate_filter(&cut, &vel, 0.0);
        assert!(filtered.scaled_value(0, 0).is_none_or(|v| v.is_nan()));
    }
}
