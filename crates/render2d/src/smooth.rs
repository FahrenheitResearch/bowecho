//! Polar-domain smoothing for display: a NaN-aware 3×3 binomial kernel
//! ([1 2 1]⊗[1 2 1]) over azimuth × range on the moment's physical values.
//! Smoothing the GRID once (cached per volume/cut/product by the render
//! worker) and rendering it through the existing nearest-gate fast path
//! keeps pans at full speed — the smoothed look costs one ~5–10 ms pass per
//! product instead of per-pixel work every frame.
//!
//! Range-folded and missing gates contribute nothing (weights renormalize);
//! a gate with no finite neighbors stays empty. Note: RF gates therefore
//! render transparent in smoothed mode — analysts who need the RF purple
//! should use the native (unsmoothed) display.
//!
//! Differential phase is angular modulo 360 degrees, so it uses a weighted
//! circular mean. The arithmetic mean used by scalar moments would turn a
//! physically adjacent 359/1-degree neighborhood into a false 180-degree
//! feature. All other moments retain the existing scalar kernel.

use radar_core::{MomentGrid, MomentStorage, MomentType};
use rayon::prelude::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MomentAlgebra {
    Linear,
    CircularDegrees,
}

fn moment_algebra(moment: &MomentType) -> MomentAlgebra {
    match moment {
        MomentType::DifferentialPhase => MomentAlgebra::CircularDegrees,
        _ => MomentAlgebra::Linear,
    }
}

/// Smooth a moment grid's values into a new F32 grid with identical
/// geometry. Azimuth wraps; range is clamped at the ends.
pub fn smooth_moment_grid(grid: &MomentGrid) -> MomentGrid {
    let rows = grid.radial_count();
    let gates = grid.gate_range.gate_count;
    let mut values = vec![f32::NAN; rows * gates];
    if rows > 0 && gates > 0 {
        // Materialize scaled values once (NaN for missing/RF).
        let mut source = vec![f32::NAN; rows * gates];
        source
            .par_chunks_mut(gates)
            .enumerate()
            .for_each(|(row, out_row)| {
                for (gate, cell) in out_row.iter_mut().enumerate() {
                    if let Some(v) = grid.scaled_value(row, gate).filter(|v| v.is_finite()) {
                        *cell = v;
                    }
                }
            });
        const KERNEL: [f32; 3] = [1.0, 2.0, 1.0];
        let algebra = moment_algebra(&grid.moment);
        values
            .par_chunks_mut(gates)
            .enumerate()
            .for_each(|(row, out_row)| {
                for (gate, cell) in out_row.iter_mut().enumerate() {
                    // A gate only renders where the native display would —
                    // smoothing must not grow coverage.
                    if !source[row * gates + gate].is_finite() {
                        continue;
                    }
                    let mut linear_sum = 0.0f32;
                    let mut linear_weight = 0.0f32;
                    let mut circular_x = 0.0f64;
                    let mut circular_y = 0.0f64;
                    let mut circular_weight = 0.0f64;
                    for (di, &kr) in KERNEL.iter().enumerate() {
                        let r = ((row as i64 + di as i64 - 1).rem_euclid(rows as i64)) as usize;
                        for (dj, &kg) in KERNEL.iter().enumerate() {
                            let g = gate as i64 + dj as i64 - 1;
                            if g < 0 || g >= gates as i64 {
                                continue;
                            }
                            let v = source[r * gates + g as usize];
                            if v.is_finite() {
                                let k = kr * kg;
                                match algebra {
                                    MomentAlgebra::Linear => {
                                        // Keep the original f32 accumulation
                                        // order and precision for scalar
                                        // moments.
                                        linear_sum += v * k;
                                        linear_weight += k;
                                    }
                                    MomentAlgebra::CircularDegrees => {
                                        let radians = f64::from(v).to_radians();
                                        let weight = f64::from(k);
                                        circular_x += weight * radians.cos();
                                        circular_y += weight * radians.sin();
                                        circular_weight += weight;
                                    }
                                }
                            }
                        }
                    }
                    match algebra {
                        MomentAlgebra::Linear if linear_weight > 0.0 => {
                            *cell = linear_sum / linear_weight;
                        }
                        MomentAlgebra::CircularDegrees if circular_weight > 0.0 => {
                            // An exactly antipodal neighborhood has no defined
                            // mean direction. Preserve its measured center
                            // instead of inventing an angle.
                            if circular_x.hypot(circular_y) < 1e-12 * circular_weight {
                                *cell = source[row * gates + gate];
                            } else {
                                let degrees = circular_y.atan2(circular_x).to_degrees();
                                *cell = degrees.rem_euclid(360.0) as f32;
                            }
                        }
                        _ => {}
                    }
                }
            });
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
    use radar_core::{GateRange, MomentType};

    fn grid(rows: usize, gates: usize, data: Vec<f32>) -> MomentGrid {
        MomentGrid {
            moment: MomentType::Reflectivity,
            gate_range: GateRange {
                first_gate_m: 250,
                gate_spacing_m: 250,
                gate_count: gates,
            },
            scale: 1.0,
            offset: 0.0,
            nodata: None,
            range_folded: None,
            radial_indices: (0..rows).collect(),
            storage: MomentStorage::F32(data),
        }
    }

    #[test]
    fn uniform_field_is_unchanged() {
        let g = grid(8, 8, vec![35.0; 64]);
        let s = smooth_moment_grid(&g);
        for row in 0..8 {
            for gate in 0..8 {
                let v = s.scaled_value(row, gate).unwrap();
                assert!((v - 35.0).abs() < 1e-4, "{v}");
            }
        }
    }

    #[test]
    fn steps_soften_and_coverage_does_not_grow() {
        // Left half 20 dBZ, right half NaN.
        let mut data = vec![f32::NAN; 64];
        for row in 0..8 {
            for gate in 0..4 {
                data[row * 8 + gate] = 20.0;
            }
        }
        let s = smooth_moment_grid(&grid(8, 8, data));
        // Edge gate keeps its value (NaN neighbors renormalize)…
        assert!((s.scaled_value(0, 3).unwrap() - 20.0).abs() < 1e-4);
        // …and empty gates STAY empty (no coverage bleed).
        assert!(s.scaled_value(0, 4).is_none_or(|v| v.is_nan()));
    }

    #[test]
    fn interior_step_blends() {
        // Gate column 4 jumps 0 -> 40: smoothed neighbors blend toward each
        // other across the step.
        let mut data = vec![0.0f32; 64];
        for row in 0..8 {
            for gate in 4..8 {
                data[row * 8 + gate] = 40.0;
            }
        }
        let s = smooth_moment_grid(&grid(8, 8, data));
        let low_side = s.scaled_value(3, 3).unwrap();
        let high_side = s.scaled_value(3, 4).unwrap();
        assert!(low_side > 0.0 && low_side < 20.0, "{low_side}");
        assert!(high_side > 20.0 && high_side < 40.0, "{high_side}");
    }

    fn phi_grid(rows: usize, gates: usize, data: Vec<f32>) -> MomentGrid {
        let mut grid = grid(rows, gates, data);
        grid.moment = MomentType::DifferentialPhase;
        grid
    }

    #[test]
    fn phi_smooths_across_the_360_wrap() {
        let mut data = vec![0.0f32; 64];
        for row in 0..8 {
            for gate in 0..8 {
                data[row * 8 + gate] = if gate < 4 { 359.0 } else { 1.0 };
            }
        }

        let smoothed = smooth_moment_grid(&phi_grid(8, 8, data));
        for row in 0..8 {
            for gate in 0..8 {
                let value = smoothed.scaled_value(row, gate).unwrap();
                let distance_from_zero = value.rem_euclid(360.0);
                let distance_from_zero = distance_from_zero.min(360.0 - distance_from_zero);
                assert!(
                    distance_from_zero <= 1.0,
                    "gate ({row},{gate}) smoothed to {value}"
                );
            }
        }
    }

    #[test]
    fn scalar_moments_keep_the_arithmetic_kernel() {
        let mut data = vec![0.0f32; 64];
        for row in 0..8 {
            for gate in 0..8 {
                data[row * 8 + gate] = if gate < 4 { 359.0 } else { 1.0 };
            }
        }

        let smoothed = smooth_moment_grid(&grid(8, 8, data));
        assert_eq!(smoothed.scaled_value(3, 3), Some(269.5));
    }

    #[test]
    fn wrap_free_phi_matches_the_scalar_mean() {
        let mut data = vec![0.0f32; 64];
        for row in 0..8 {
            for gate in 0..8 {
                data[row * 8 + gate] = 40.0 + gate as f32 * 3.0;
            }
        }

        let circular = smooth_moment_grid(&phi_grid(8, 8, data.clone()));
        let linear = smooth_moment_grid(&grid(8, 8, data));
        for row in 0..8 {
            for gate in 0..8 {
                let circular = circular.scaled_value(row, gate).unwrap();
                let linear = linear.scaled_value(row, gate).unwrap();
                assert!((circular - linear).abs() < 0.05, "{circular} vs {linear}");
            }
        }
    }

    #[test]
    fn uniform_phi_at_the_wrap_is_unchanged() {
        for expected in [0.0f32, 90.0, 180.0, 359.5] {
            let smoothed = smooth_moment_grid(&phi_grid(8, 8, vec![expected; 64]));
            for row in 0..8 {
                for gate in 0..8 {
                    let value = smoothed.scaled_value(row, gate).unwrap();
                    let delta = (value - expected).rem_euclid(360.0);
                    assert!(delta.min(360.0 - delta) < 1e-2, "{expected} -> {value}");
                }
            }
        }
    }

    #[test]
    fn phi_smoothing_does_not_grow_coverage() {
        let mut data = vec![f32::NAN; 64];
        for row in 0..8 {
            for gate in 0..4 {
                data[row * 8 + gate] = 359.0;
            }
        }

        let smoothed = smooth_moment_grid(&phi_grid(8, 8, data));
        assert!(
            smoothed
                .scaled_value(0, 4)
                .is_none_or(|value| value.is_nan())
        );
        assert!((smoothed.scaled_value(0, 3).unwrap() - 359.0).abs() < 1e-2);
    }
}
