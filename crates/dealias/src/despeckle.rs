//! Post-unfold speckle removal (step 5 of the crate-level docs).

use crate::resolve_nyquist;

/// Remove isolated single-gate outliers from an unfolded field: a gate whose
/// velocity differs from the median of its finite 4-neighbours by more than a
/// Nyquist is snapped onto the Nyquist multiple nearest that median
/// (dual-PRF/processor speckle; Holleman & Beekhuis 2003, *JTECH* 20,
/// 443–453; Altube et al. 2017, *JTECH* 34, 1529–1543,
/// doi:10.1175/JTECH-D-16-0065.1).
///
/// `velocity` is row-major `nyquist.len() × gates`; `NaN` gates are ignored.
/// [`dealias`](crate::dealias) already runs this — call it directly only if
/// you build a pipeline from [`compute_folds`](crate::compute_folds).
pub fn despeckle(velocity: &mut [f32], gates: usize, nyquist: &[f32]) {
    let nyq = resolve_nyquist(nyquist);
    despeckle_tracking_folds(velocity, gates, &nyq, None);
}

/// `despeckle` against pre-resolved Nyquist values, optionally keeping a
/// fold-count field consistent with the snaps it applies (every snap is a
/// whole number of 2·Nyquist intervals).
pub(crate) fn despeckle_tracking_folds(
    velocity: &mut [f32],
    gates: usize,
    nyq: &[f32],
    mut folds: Option<&mut [i32]>,
) {
    let rows = nyq.len();
    if rows < 3 || gates < 3 || velocity.len() != rows.saturating_mul(gates) {
        return;
    }
    if let Some(folds) = &folds
        && folds.len() != velocity.len()
    {
        return;
    }
    let snapshot = velocity.to_vec();
    #[allow(clippy::needless_range_loop)]
    for row in 0..rows {
        let n = nyq[row];
        if !n.is_finite() {
            continue;
        }
        for gate in 0..gates {
            let idx = row * gates + gate;
            let v = snapshot[idx];
            if !v.is_finite() {
                continue;
            }
            let mut neigh = [0.0f32; 4];
            let mut count = 0;
            for (nr, ng) in [
                (row.wrapping_sub(1), gate),
                (row + 1, gate),
                (row, gate.wrapping_sub(1)),
                (row, gate + 1),
            ] {
                if nr >= rows || ng >= gates {
                    continue;
                }
                let nv = snapshot[nr * gates + ng];
                if nv.is_finite() {
                    neigh[count] = nv;
                    count += 1;
                }
            }
            if count < 3 {
                continue;
            }
            let median = median_small_f32(&mut neigh, count);
            if (v - median).abs() > n {
                // collapse the outlier onto the nearest Nyquist multiple of the
                // local consensus.
                let fold = ((median - v) / (2.0 * n)).round();
                velocity[idx] = v + 2.0 * n * fold;
                if let Some(folds) = &mut folds {
                    folds[idx] += fold as i32;
                }
            }
        }
    }
}

fn median_small_f32(values: &mut [f32], count: usize) -> f32 {
    debug_assert!(count > 0 && count <= values.len());
    values[..count].sort_by(f32::total_cmp);
    values[count / 2]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snaps_isolated_outlier_onto_local_consensus() {
        // 3×3 field of ~5 m/s with the centre gate folded down to -15
        // (one fold below at Nyquist 10): despeckle must lift it back up.
        let mut velocity = vec![
            5.0, 5.0, 5.0, //
            5.0, -15.0, 5.0, //
            5.0, 5.0, 5.0,
        ];
        despeckle(&mut velocity, 3, &[10.0, 10.0, 10.0]);
        assert_eq!(velocity[4], 5.0);
    }

    #[test]
    fn leaves_coherent_fields_alone() {
        let original = vec![
            2.0, 3.0, 4.0, //
            3.0, 4.0, 5.0, //
            4.0, 5.0, 6.0,
        ];
        let mut velocity = original.clone();
        despeckle(&mut velocity, 3, &[10.0, 10.0, 10.0]);
        assert_eq!(velocity, original);
    }
}
