//! Hybrid 3-D + temporal velocity dealiasing.
//!
//! The region engine is fast and preserves genuine shear, but a connected
//! region's absolute Nyquist branch is under-determined.  The cascade engine
//! adds a broad wind reference from higher tilts, which is useful but can miss
//! a compact mesocyclone.  This engine keeps the region solver and supplies
//! three kinds of independent branch evidence:
//!
//! - the previous volume's matching elevation (same storm, same beam height),
//! - the nearest current-volume velocity tilt above/below the target,
//! - a range-band harmonic wind fit as a broad fallback.
//!
//! Supporting grids are mapped once onto the target polar lattice and the
//! region solver applies only decisive whole-region branch changes.  No
//! per-pixel work is added to viewport rendering; the result is a normal
//! cached [`MomentGrid`].

use radar_core::{ElevationCut, MomentGrid, MomentType, RadarVolume};
use rayon::prelude::*;

use crate::{
    DEALIASED_VELOCITY_NODATA, DenseVelocityReference, RangeBandReference, dealias_velocity_grid,
    dealias_velocity_grid_with_reference, dealias_velocity_grid_with_references,
    decode_dealiased_velocity, encode_dealiased_velocity, fit_range_band_reference,
    median_nyquist_mps, row_nyquist_mps,
};

/// Temporal references older than this are more likely to represent a
/// different storm structure than a useful continuity constraint.
const TEMPORAL_REFERENCE_MAX_AGE_SECONDS: i64 = 15 * 60;
/// Match the same nominal elevation between consecutive volumes.
const TEMPORAL_ELEVATION_TOLERANCE_DEG: f32 = 0.40;
/// Adjacent-tilt evidence beyond this separation is too vertically remote to
/// be a reliable local branch reference.
const SPATIAL_REFERENCE_MAX_ELEVATION_DELTA_DEG: f32 = 3.0;
/// Treat two independent references as agreeing when they are this close.
/// Correct/incorrect Nyquist branches are normally separated by 30-70 m/s,
/// so 12 m/s leaves room for real temporal and vertical shear.
const REFERENCE_AGREEMENT_MPS: f32 = 12.0;
/// Reject physically implausible reference samples before branch scoring.
const MAX_REFERENCE_ABS_VELOCITY_MPS: f32 = 160.0;

/// The region graph cannot repair a folded pocket when its wrapped values
/// numerically merge into a much larger legitimate opposite-sign region.
/// After the conservative region solve, allow a local Nyquist-branch proposal
/// only when the mapped 3-D/temporal reference wins by a large margin.
const LOCAL_BRANCH_MIN_IMPROVEMENT_MPS: f32 = 8.0;
const LOCAL_BRANCH_MIN_SEPARATION_MPS: f32 = 4.0;
const LOCAL_BRANCH_MAX_REFERENCE_ERROR_MPS: f32 = 18.0;
const LOCAL_BRANCH_MAX_REFERENCE_ERROR_NYQUIST_FRAC: f32 = 0.90;
/// A proposed branch must be spatially coherent.  Three agreeing cells in a
/// five-cell cross (self + four direct neighbors) keeps compact folded lobes
/// while rejecting isolated or two-gate noise, with one cheap O(n) pass.
const LOCAL_BRANCH_MIN_NEIGHBORS: u8 = 3;

/// Dealias `cut_index` using spatial and temporal branch references.
///
/// `previous_volume` is optional and must be from the same site.  A stale,
/// future, or elevation-incompatible volume is ignored.  When no dense
/// reference is available this gracefully reduces to an upper-tilt harmonic
/// reference, then to the plain region engine.
pub fn dealias_velocity_grid_hybrid(
    volume: &RadarVolume,
    cut_index: usize,
    previous_volume: Option<&RadarVolume>,
) -> Option<MomentGrid> {
    let target_cut = volume.cuts.get(cut_index)?;
    let target_grid = target_cut.moments.get(&MomentType::Velocity)?;

    let temporal = previous_volume
        .filter(|previous| temporal_volume_is_usable(volume, previous))
        .and_then(|previous| {
            let previous_index = matching_velocity_cut(previous, target_cut)?;
            let previous_cut = previous.cuts.get(previous_index)?;
            let previous_grid = previous_cut.moments.get(&MomentType::Velocity)?;
            let dealiased = dealias_with_nearest_upper_reference(
                previous,
                previous_index,
                previous_cut,
                previous_grid,
            );
            map_reference_to_target(target_cut, target_grid, previous_cut, &dealiased)
        });

    // With a same-elevation temporal reference, one adjacent tilt is enough.
    // Without it, use both sides when available.  Upper tilts are preferred in
    // a near tie because they usually carry the cleaner/higher-Nyquist field.
    let spatial_indices = spatial_reference_indices(volume, cut_index, temporal.is_some());
    let mut spatial = Vec::with_capacity(spatial_indices.len());
    let mut harmonic_reference: Option<RangeBandReference> = None;
    for index in spatial_indices {
        let Some(source_cut) = volume.cuts.get(index) else {
            continue;
        };
        let Some(source_grid) = source_cut.moments.get(&MomentType::Velocity) else {
            continue;
        };
        // Supporting tilts use the fast region pass.  Running a full cascade
        // here could feed the still-ambiguous target back into a lower support
        // tilt, creating circular evidence.  When this is an upper tilt, reuse
        // the same solved grid for the broad harmonic fit — no duplicate pass.
        let dealiased = dealias_velocity_grid(source_cut, source_grid);
        if harmonic_reference.is_none()
            && source_cut.elevation_deg > target_cut.elevation_deg + 0.12
        {
            harmonic_reference = Some(fit_range_band_reference(source_cut, &dealiased));
        }
        if let Some(mapped) =
            map_reference_to_target(target_cut, target_grid, source_cut, &dealiased)
        {
            spatial.push(mapped);
        }
    }

    let dense_reference = combine_references(
        target_cut,
        target_grid,
        temporal,
        spatial,
        harmonic_reference.as_ref(),
    );

    let dealiased = dealias_velocity_grid_with_references(
        target_cut,
        target_grid,
        harmonic_reference.as_ref(),
        dense_reference.as_ref(),
    );

    Some(match dense_reference.as_ref() {
        Some(reference) => {
            refine_local_branch_patches(target_cut, target_grid, dealiased, reference)
        }
        None => dealiased,
    })
}

fn refine_local_branch_patches(
    cut: &ElevationCut,
    source: &MomentGrid,
    mut dealiased: MomentGrid,
    reference: &DenseVelocityReference,
) -> MomentGrid {
    let rows = source.radial_count();
    let gates = source.gate_range.gate_count;
    let total = rows.saturating_mul(gates);
    if rows == 0 || gates == 0 || total == 0 {
        return dealiased;
    }

    let fallback_nyquist = median_nyquist_mps(cut, source);
    let nyquist = (0..rows)
        .map(|row| {
            row_nyquist_mps(cut, source, row)
                .or(fallback_nyquist)
                .filter(|value| value.is_finite() && *value > 0.0)
                .unwrap_or(f32::NAN)
        })
        .collect::<Vec<_>>();

    let radar_core::MomentStorage::U16(current_values) = &dealiased.storage else {
        return dealiased;
    };
    let mut proposal = vec![0i8; total];
    proposal
        .par_chunks_mut(gates)
        .enumerate()
        .for_each(|(row, output)| {
            let n = nyquist[row];
            if !n.is_finite() || n <= 0.0 {
                return;
            }
            let interval = 2.0 * n;
            let max_reference_error = LOCAL_BRANCH_MAX_REFERENCE_ERROR_MPS
                .min(LOCAL_BRANCH_MAX_REFERENCE_ERROR_NYQUIST_FRAC * n);
            for (gate, slot) in output.iter_mut().enumerate() {
                let raw = current_values[row * gates + gate];
                let Some(current) = decode_dealiased_velocity(raw) else {
                    continue;
                };
                let Some(predicted) = reference.eval(row, gate) else {
                    continue;
                };

                // The nearest Nyquist branch is analytic — no per-gate sort
                // or five-candidate scan in this million-gate hot pass.
                let branch_float = ((predicted - current) / interval).round();
                if !branch_float.is_finite() || !(-2.0..=2.0).contains(&branch_float) {
                    continue;
                }
                let branch = branch_float as i8;
                if branch == 0 {
                    continue;
                }
                let best_error = (current + branch as f32 * interval - predicted).abs();
                let current_error = (current - predicted).abs();
                // With rounding, the next-nearest branch is one interval
                // farther away (except exact half-interval ties, rejected by
                // the separation test).
                let second_error = interval - best_error;
                if current_error - best_error >= LOCAL_BRANCH_MIN_IMPROVEMENT_MPS
                    && second_error - best_error >= LOCAL_BRANCH_MIN_SEPARATION_MPS
                    && best_error <= max_reference_error
                {
                    *slot = branch;
                }
            }
        });

    // The proposal is independent of the region labels on purpose: a wrapped
    // outbound lobe can have the same observed value as the environmental
    // inbound field and therefore be fused into one huge region.  One direct-
    // neighbor consensus pass extracts coherent local branch patches.
    let mut coherent = vec![0i8; total];
    coherent
        .par_chunks_mut(gates)
        .enumerate()
        .for_each(|(row, output)| {
            for (gate, slot) in output.iter_mut().enumerate() {
                let delta = proposal[row * gates + gate];
                if delta == 0 {
                    continue;
                }
                let mut agreeing = 1u8; // self
                if row > 0 {
                    agreeing += u8::from(proposal[(row - 1) * gates + gate] == delta);
                }
                if row + 1 < rows {
                    agreeing += u8::from(proposal[(row + 1) * gates + gate] == delta);
                }
                if gate > 0 {
                    agreeing += u8::from(proposal[row * gates + gate - 1] == delta);
                }
                if gate + 1 < gates {
                    agreeing += u8::from(proposal[row * gates + gate + 1] == delta);
                }
                if agreeing >= LOCAL_BRANCH_MIN_NEIGHBORS {
                    *slot = delta;
                }
            }
        });

    let radar_core::MomentStorage::U16(values) = &mut dealiased.storage else {
        return dealiased;
    };
    values
        .par_chunks_mut(gates)
        .enumerate()
        .for_each(|(row, output)| {
            let n = nyquist[row];
            if !n.is_finite() || n <= 0.0 {
                return;
            }
            for (gate, raw) in output.iter_mut().enumerate() {
                let delta = coherent[row * gates + gate];
                if delta == 0 || *raw == DEALIASED_VELOCITY_NODATA {
                    continue;
                }
                let Some(current) = decode_dealiased_velocity(*raw) else {
                    continue;
                };
                *raw = encode_dealiased_velocity(current + delta as f32 * 2.0 * n);
            }
        });
    dealiased
}

fn nearest_upper_harmonic_reference(
    volume: &RadarVolume,
    target_index: usize,
) -> Option<RangeBandReference> {
    let target = volume.cuts.get(target_index)?;
    let (upper_index, _) = volume
        .cuts
        .iter()
        .enumerate()
        .filter(|(index, cut)| {
            *index != target_index && cut.moments.contains_key(&MomentType::Velocity)
        })
        .filter_map(|(index, cut)| {
            let delta = cut.elevation_deg - target.elevation_deg;
            (0.12..=SPATIAL_REFERENCE_MAX_ELEVATION_DELTA_DEG)
                .contains(&delta)
                .then_some((index, delta))
        })
        .min_by(|left, right| left.1.total_cmp(&right.1))?;
    let upper_cut = volume.cuts.get(upper_index)?;
    let upper_grid = upper_cut.moments.get(&MomentType::Velocity)?;
    let upper_dealiased = dealias_velocity_grid(upper_cut, upper_grid);
    Some(fit_range_band_reference(upper_cut, &upper_dealiased))
}

fn dealias_with_nearest_upper_reference(
    volume: &RadarVolume,
    cut_index: usize,
    cut: &ElevationCut,
    grid: &MomentGrid,
) -> MomentGrid {
    let reference = nearest_upper_harmonic_reference(volume, cut_index);
    dealias_velocity_grid_with_reference(cut, grid, reference.as_ref())
}

fn temporal_volume_is_usable(current: &RadarVolume, previous: &RadarVolume) -> bool {
    if !current.site.id.eq_ignore_ascii_case(&previous.site.id) {
        return false;
    }
    let age_seconds = current
        .volume_time
        .signed_duration_since(previous.volume_time)
        .num_seconds();
    age_seconds > 0 && age_seconds <= TEMPORAL_REFERENCE_MAX_AGE_SECONDS
}

fn matching_velocity_cut(volume: &RadarVolume, target: &ElevationCut) -> Option<usize> {
    volume
        .cuts
        .iter()
        .enumerate()
        .filter(|(_, cut)| cut.moments.contains_key(&MomentType::Velocity))
        .filter_map(|(index, cut)| {
            let delta = (cut.elevation_deg - target.elevation_deg).abs();
            (delta <= TEMPORAL_ELEVATION_TOLERANCE_DEG).then_some((
                index,
                delta,
                target.elevation_number.is_some()
                    && cut.elevation_number != target.elevation_number,
            ))
        })
        .min_by(|left, right| {
            left.1
                .total_cmp(&right.1)
                .then_with(|| left.2.cmp(&right.2))
        })
        .map(|(index, _, _)| index)
}

fn spatial_reference_indices(
    volume: &RadarVolume,
    target_index: usize,
    has_temporal: bool,
) -> Vec<usize> {
    let Some(target) = volume.cuts.get(target_index) else {
        return Vec::new();
    };
    let mut lower: Option<(usize, f32)> = None;
    let mut upper: Option<(usize, f32)> = None;

    for (index, cut) in volume.cuts.iter().enumerate() {
        if index == target_index || !cut.moments.contains_key(&MomentType::Velocity) {
            continue;
        }
        let signed = cut.elevation_deg - target.elevation_deg;
        let delta = signed.abs();
        // Ignore same-elevation SAILS/MESO-SAILS revisits.  They are separate
        // times, not a vertical reference.
        if !(0.12..=SPATIAL_REFERENCE_MAX_ELEVATION_DELTA_DEG).contains(&delta) {
            continue;
        }
        let slot = if signed < 0.0 { &mut lower } else { &mut upper };
        if slot.as_ref().is_none_or(|(_, best)| delta < *best) {
            *slot = Some((index, delta));
        }
    }

    if has_temporal {
        return match (lower, upper) {
            (Some((lower_index, lower_delta)), Some((upper_index, upper_delta))) => {
                // Small upper preference in a near tie.
                if upper_delta <= lower_delta + 0.20 {
                    vec![upper_index]
                } else {
                    vec![lower_index]
                }
            }
            (Some((index, _)), None) | (None, Some((index, _))) => vec![index],
            (None, None) => Vec::new(),
        };
    }

    // Upper first: it is usually less aliased.  Both are retained so their
    // agreement can provide a strong local reference when no temporal field
    // exists.
    let mut out = Vec::with_capacity(2);
    if let Some((index, _)) = upper {
        out.push(index);
    }
    if let Some((index, _)) = lower {
        out.push(index);
    }
    out
}

fn map_reference_to_target(
    target_cut: &ElevationCut,
    target_grid: &MomentGrid,
    source_cut: &ElevationCut,
    source_grid: &MomentGrid,
) -> Option<Vec<f32>> {
    let target_rows = target_grid.radial_count();
    let target_gates = target_grid.gate_range.gate_count;
    let source_rows = source_grid.radial_count();
    if target_rows == 0 || target_gates == 0 || source_rows == 0 {
        return None;
    }

    let row_map = nearest_azimuth_row_map(target_cut, target_grid, source_cut, source_grid);
    let gate_map = nearest_range_gate_map(target_grid, source_grid);
    if row_map.iter().all(Option::is_none) || gate_map.iter().all(Option::is_none) {
        return None;
    }

    let mut mapped = vec![f32::NAN; target_rows.saturating_mul(target_gates)];
    mapped
        .par_chunks_mut(target_gates)
        .enumerate()
        .for_each(|(target_row, output)| {
            let Some(source_row) = row_map[target_row] else {
                return;
            };
            for (target_gate, slot) in output.iter_mut().enumerate() {
                let Some(source_gate) = gate_map[target_gate] else {
                    continue;
                };
                let Some(value) =
                    source_grid
                        .scaled_value(source_row, source_gate)
                        .filter(|value| {
                            value.is_finite() && value.abs() <= MAX_REFERENCE_ABS_VELOCITY_MPS
                        })
                else {
                    continue;
                };
                *slot = value;
            }
        });
    mapped
        .iter()
        .any(|value| value.is_finite())
        .then_some(mapped)
}

fn nearest_azimuth_row_map(
    target_cut: &ElevationCut,
    target_grid: &MomentGrid,
    source_cut: &ElevationCut,
    source_grid: &MomentGrid,
) -> Vec<Option<usize>> {
    let mut source_rows = source_grid
        .radial_indices
        .iter()
        .enumerate()
        .filter_map(|(row, &radial_index)| {
            let azimuth = source_cut.radials.get(radial_index)?.azimuth_deg;
            azimuth
                .is_finite()
                .then_some((azimuth.rem_euclid(360.0), row))
        })
        .collect::<Vec<_>>();
    source_rows.sort_by(|left, right| left.0.total_cmp(&right.0));
    if source_rows.is_empty() {
        return vec![None; target_grid.radial_count()];
    }

    let max_gap = (typical_azimuth_spacing(&source_rows) * 2.5).clamp(0.75, 3.0);
    target_grid
        .radial_indices
        .iter()
        .map(|&radial_index| {
            let target_azimuth = target_cut
                .radials
                .get(radial_index)?
                .azimuth_deg
                .rem_euclid(360.0);
            let position = source_rows.partition_point(|(azimuth, _)| *azimuth < target_azimuth);
            let mut best: Option<(usize, f32)> = None;
            for candidate in [
                position.checked_sub(1),
                (position < source_rows.len()).then_some(position),
                Some(0),
                source_rows.len().checked_sub(1),
            ]
            .into_iter()
            .flatten()
            {
                let (source_azimuth, source_row) = source_rows[candidate];
                let distance = angular_distance_deg(target_azimuth, source_azimuth);
                if best.is_none_or(|(_, best_distance)| distance < best_distance) {
                    best = Some((source_row, distance));
                }
            }
            best.and_then(|(row, distance)| (distance <= max_gap).then_some(row))
        })
        .collect()
}

fn typical_azimuth_spacing(rows: &[(f32, usize)]) -> f32 {
    if rows.len() < 2 {
        return 1.0;
    }
    let mut gaps = rows
        .windows(2)
        .map(|pair| pair[1].0 - pair[0].0)
        .filter(|gap| gap.is_finite() && *gap > 0.001 && *gap < 10.0)
        .collect::<Vec<_>>();
    if gaps.is_empty() {
        return (360.0 / rows.len() as f32).clamp(0.25, 2.0);
    }
    let middle = gaps.len() / 2;
    gaps.select_nth_unstable_by(middle, |left, right| left.total_cmp(right));
    gaps[middle].clamp(0.20, 2.0)
}

fn angular_distance_deg(left: f32, right: f32) -> f32 {
    ((left - right + 180.0).rem_euclid(360.0) - 180.0).abs()
}

fn nearest_range_gate_map(target: &MomentGrid, source: &MomentGrid) -> Vec<Option<usize>> {
    let target_first = target.gate_range.first_gate_m as f64;
    let target_spacing = target.gate_range.gate_spacing_m.max(1) as f64;
    let source_first = source.gate_range.first_gate_m as f64;
    let source_spacing = source.gate_range.gate_spacing_m.max(1) as f64;
    let source_gates = source.gate_range.gate_count;
    let tolerance = 0.60 * target_spacing.max(source_spacing) + 1.0;

    (0..target.gate_range.gate_count)
        .map(|gate| {
            let target_range = target_first + gate as f64 * target_spacing;
            let source_position = (target_range - source_first) / source_spacing;
            let source_gate = source_position.round();
            if source_gate < 0.0 || source_gate >= source_gates as f64 {
                return None;
            }
            let source_range = source_first + source_gate * source_spacing;
            ((source_range - target_range).abs() <= tolerance).then_some(source_gate as usize)
        })
        .collect()
}

fn combine_references(
    target_cut: &ElevationCut,
    target_grid: &MomentGrid,
    temporal: Option<Vec<f32>>,
    spatial: Vec<Vec<f32>>,
    harmonic: Option<&RangeBandReference>,
) -> Option<DenseVelocityReference> {
    let rows = target_grid.radial_count();
    let gates = target_grid.gate_range.gate_count;
    let total = rows.saturating_mul(gates);
    if total == 0 {
        return None;
    }
    let spatial = spatial
        .into_iter()
        .filter(|values| values.len() == total)
        .take(2)
        .collect::<Vec<_>>();
    let temporal = temporal.filter(|values| values.len() == total);
    if temporal.is_none() && spatial.is_empty() {
        return None;
    }

    let row_azimuths = target_grid
        .radial_indices
        .iter()
        .map(|&radial_index| {
            target_cut
                .radials
                .get(radial_index)
                .map(|radial| radial.azimuth_deg.to_radians().sin_cos())
        })
        .collect::<Vec<_>>();

    let mut values = vec![f32::NAN; total];
    values
        .par_chunks_mut(gates)
        .enumerate()
        .for_each(|(row, output)| {
            for (gate, slot) in output.iter_mut().enumerate() {
                let index = row * gates + gate;
                let temporal_value = temporal
                    .as_ref()
                    .and_then(|field| finite_reference(field[index]));
                let first_spatial = spatial
                    .first()
                    .and_then(|field| finite_reference(field[index]));
                let second_spatial = spatial
                    .get(1)
                    .and_then(|field| finite_reference(field[index]));

                let spatial_value = match (first_spatial, second_spatial) {
                    (Some(left), Some(right))
                        if (left - right).abs() <= REFERENCE_AGREEMENT_MPS =>
                    {
                        Some(0.5 * (left + right))
                    }
                    (Some(left), Some(right)) => {
                        let predicted = row_azimuths[row].and_then(|(sin_az, cos_az)| {
                            harmonic.and_then(|reference| reference.eval(sin_az, cos_az, gate))
                        });
                        predicted.and_then(|predicted| {
                            let left_error = (left - predicted).abs();
                            let right_error = (right - predicted).abs();
                            ((left_error - right_error).abs() >= 3.0).then_some(
                                if left_error < right_error {
                                    left
                                } else {
                                    right
                                },
                            )
                        })
                    }
                    (Some(value), None) | (None, Some(value)) => Some(value),
                    (None, None) => None,
                };

                *slot = match (temporal_value, spatial_value) {
                    (Some(temporal), Some(spatial))
                        if (temporal - spatial).abs() <= REFERENCE_AGREEMENT_MPS =>
                    {
                        // Same-elevation temporal continuity is strongest when
                        // it agrees with the current-volume vertical evidence.
                        0.75 * temporal + 0.25 * spatial
                    }
                    (Some(temporal), Some(spatial)) => {
                        // A compact storm can translate several gates between
                        // volumes.  On disagreement, prefer current-time 3-D
                        // evidence unless the broad harmonic fit decisively
                        // supports the temporal branch.
                        let predicted = row_azimuths[row].and_then(|(sin_az, cos_az)| {
                            harmonic.and_then(|reference| reference.eval(sin_az, cos_az, gate))
                        });
                        predicted
                            .and_then(|predicted| {
                                let temporal_error = (temporal - predicted).abs();
                                let spatial_error = (spatial - predicted).abs();
                                ((temporal_error - spatial_error).abs() >= 3.0).then_some(
                                    if temporal_error < spatial_error {
                                        temporal
                                    } else {
                                        spatial
                                    },
                                )
                            })
                            .unwrap_or(spatial)
                    }
                    (Some(temporal), None) => temporal,
                    (None, Some(spatial)) => spatial,
                    (None, None) => f32::NAN,
                };
            }
        });

    DenseVelocityReference::new(rows, gates, values)
}

fn finite_reference(value: f32) -> Option<f32> {
    (value.is_finite() && value.abs() <= MAX_REFERENCE_ABS_VELOCITY_MPS).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};
    use radar_core::{GateRange, MomentStorage, RadarSite, Radial};

    fn tilt_with_wind(
        elevation: f32,
        speed: f32,
        toward_deg: f32,
        nyquist: f32,
        rows: usize,
        gates: usize,
    ) -> ElevationCut {
        let gate_range = GateRange {
            first_gate_m: 1000,
            gate_spacing_m: 250,
            gate_count: gates,
        };
        let mut cut = ElevationCut::new(elevation, None);
        let mut data = vec![f32::NAN; rows * gates];
        for row in 0..rows {
            let azimuth = row as f32 * (360.0 / rows as f32);
            cut.radials.push(Radial {
                azimuth_deg: azimuth,
                elevation_deg: elevation,
                time_offset_ms: 0,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: Some(nyquist),
                radial_status: None,
            });
            let truth = speed * ((azimuth - toward_deg).to_radians()).cos();
            let wrapped = (truth + nyquist).rem_euclid(2.0 * nyquist) - nyquist;
            for gate in 0..gates {
                data[row * gates + gate] = wrapped;
            }
        }
        cut.moments.insert(
            MomentType::Velocity,
            MomentGrid {
                moment: MomentType::Velocity,
                gate_range,
                scale: 1.0,
                offset: 0.0,
                nodata: None,
                range_folded: None,
                radial_indices: (0..rows).collect(),
                storage: MomentStorage::F32(data),
            },
        );
        cut
    }

    fn isolated_patch_tilt(
        elevation: f32,
        true_velocity: f32,
        nyquist: f32,
        rows: usize,
        gates: usize,
    ) -> ElevationCut {
        let gate_range = GateRange {
            first_gate_m: 1000,
            gate_spacing_m: 250,
            gate_count: gates,
        };
        let mut cut = ElevationCut::new(elevation, None);
        let mut data = vec![f32::NAN; rows * gates];
        let wrapped = (true_velocity + nyquist).rem_euclid(2.0 * nyquist) - nyquist;
        for row in 0..rows {
            let azimuth = row as f32 * (360.0 / rows as f32);
            cut.radials.push(Radial {
                azimuth_deg: azimuth,
                elevation_deg: elevation,
                time_offset_ms: 0,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: Some(nyquist),
                radial_status: None,
            });
            if (80..=110).contains(&row) {
                for gate in 20..gates.min(45) {
                    data[row * gates + gate] = wrapped;
                }
            }
        }
        cut.moments.insert(
            MomentType::Velocity,
            MomentGrid {
                moment: MomentType::Velocity,
                gate_range,
                scale: 1.0,
                offset: 0.0,
                nodata: None,
                range_folded: None,
                radial_indices: (0..rows).collect(),
                storage: MomentStorage::F32(data),
            },
        );
        cut
    }

    fn connected_alias_patch_tilt(
        elevation: f32,
        nyquist: f32,
        rows: usize,
        gates: usize,
    ) -> ElevationCut {
        let gate_range = GateRange {
            first_gate_m: 1000,
            gate_spacing_m: 250,
            gate_count: gates,
        };
        let mut cut = ElevationCut::new(elevation, None);
        let mut data = vec![f32::NAN; rows * gates];
        for row in 0..rows {
            let azimuth = row as f32 * (360.0 / rows as f32);
            cut.radials.push(Radial {
                azimuth_deg: azimuth,
                elevation_deg: elevation,
                time_offset_ms: 0,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: Some(nyquist),
                radial_status: None,
            });
            for gate in 0..gates {
                let truth = if (120..=180).contains(&row) && (30..=70).contains(&gate) {
                    35.0
                } else {
                    -5.0
                };
                data[row * gates + gate] = (truth + nyquist).rem_euclid(2.0 * nyquist) - nyquist;
            }
        }
        cut.moments.insert(
            MomentType::Velocity,
            MomentGrid {
                moment: MomentType::Velocity,
                gate_range,
                scale: 1.0,
                offset: 0.0,
                nodata: None,
                range_folded: None,
                radial_indices: (0..rows).collect(),
                storage: MomentStorage::F32(data),
            },
        );
        cut
    }

    fn wind_error(grid: &MomentGrid, speed: f32, toward_deg: f32) -> f32 {
        let rows = grid.radial_count();
        let gates = grid.gate_range.gate_count;
        let mut worst = 0.0f32;
        for row in 0..rows {
            let azimuth = row as f32 * (360.0 / rows as f32);
            let truth = speed * ((azimuth - toward_deg).to_radians()).cos();
            for gate in (0..gates).step_by(7) {
                if let Some(value) = grid
                    .scaled_value(row, gate)
                    .filter(|value| value.is_finite())
                {
                    worst = worst.max((value - truth).abs());
                }
            }
        }
        worst
    }

    #[test]
    fn temporal_reference_recovers_a_topmost_aliased_high_tilt() {
        let base_time = chrono::DateTime::<Utc>::UNIX_EPOCH + Duration::hours(1);
        let mut previous = RadarVolume::new(RadarSite::new("TEST"), base_time);
        previous.cuts = vec![
            tilt_with_wind(1.23, 35.0, 180.0, 20.0, 360, 120),
            tilt_with_wind(2.4, 35.0, 180.0, 40.0, 360, 120),
        ];

        // The current partial volume has reached 1.23° but no cleaner upper
        // tilt yet.  Temporal continuity should supply the missing branch.
        let mut current =
            RadarVolume::new(RadarSite::new("TEST"), base_time + Duration::minutes(5));
        current.cuts = vec![tilt_with_wind(1.23, 35.0, 180.0, 20.0, 360, 120)];

        let hybrid = dealias_velocity_grid_hybrid(&current, 0, Some(&previous)).expect("hybrid");
        assert!(
            wind_error(&hybrid, 35.0, 180.0) < 2.0,
            "temporal reference should recover the correct Nyquist branch"
        );
    }

    #[test]
    fn lower_current_tilt_can_reference_a_folded_higher_tilt() {
        let time = chrono::DateTime::<Utc>::UNIX_EPOCH + Duration::hours(2);
        let mut volume = RadarVolume::new(RadarSite::new("TEST"), time);
        volume.cuts = vec![
            // Cleaner lower sweep with a high Nyquist interval.
            tilt_with_wind(0.5, 35.0, 90.0, 40.0, 360, 100),
            // "Tilt 3"-like 1.23° sweep whose same wind aliases at ±20 m/s.
            tilt_with_wind(1.23, 35.0, 90.0, 20.0, 720, 100),
        ];

        let hybrid = dealias_velocity_grid_hybrid(&volume, 1, None).expect("hybrid");
        assert!(
            wind_error(&hybrid, 35.0, 90.0) < 2.0,
            "adjacent-tilt reference should recover the higher tilt branch"
        );
    }

    #[test]
    fn current_lower_tilt_fixes_an_isolated_high_tilt_branch() {
        let time = chrono::DateTime::<Utc>::UNIX_EPOCH + Duration::hours(3);
        let mut volume = RadarVolume::new(RadarSite::new("TEST"), time);
        volume.cuts = vec![
            isolated_patch_tilt(0.5, 35.0, 45.0, 360, 80),
            isolated_patch_tilt(1.23, 35.0, 20.0, 360, 80),
        ];

        let target = volume.cuts[1]
            .moments
            .get(&MomentType::Velocity)
            .expect("target velocity");
        let region = dealias_velocity_grid(&volume.cuts[1], target);
        assert!(
            (region.scaled_value(90, 30).expect("region value") + 5.0).abs() < 1.0,
            "an isolated group has no same-tilt evidence for its absolute branch"
        );

        let hybrid = dealias_velocity_grid_hybrid(&volume, 1, None).expect("hybrid");
        assert!(
            (hybrid.scaled_value(90, 30).expect("hybrid value") - 35.0).abs() < 1.0,
            "current-volume vertical evidence should choose the +1 Nyquist branch"
        );
        assert!(
            hybrid.scaled_value(20, 30).is_none(),
            "dealiasing must not invent gates outside native coverage"
        );
    }

    #[test]
    fn local_refinement_repairs_a_folded_patch_fused_into_legitimate_inbound() {
        let time = chrono::DateTime::<Utc>::UNIX_EPOCH + Duration::hours(4);
        let mut volume = RadarVolume::new(RadarSite::new("TEST"), time);
        volume.cuts = vec![
            // The lower reference sees the +35 m/s lobe without aliasing.
            connected_alias_patch_tilt(0.5, 45.0, 360, 100),
            // At ±20 m/s, +35 wraps to -5 — exactly the same observed value
            // as the surrounding legitimate inbound field.  A region-only
            // solver therefore fuses both into one giant region.
            connected_alias_patch_tilt(1.23, 20.0, 360, 100),
        ];

        let target = volume.cuts[1]
            .moments
            .get(&MomentType::Velocity)
            .expect("target velocity");
        let region = dealias_velocity_grid(&volume.cuts[1], target);
        assert!(
            (region.scaled_value(150, 50).expect("region value") + 5.0).abs() < 1.0,
            "region-only dealiasing cannot split a folded lobe from an identical background"
        );

        let hybrid = dealias_velocity_grid_hybrid(&volume, 1, None).expect("hybrid");
        assert!(
            (hybrid.scaled_value(150, 50).expect("hybrid patch") - 35.0).abs() < 1.0,
            "local branch refinement should recover the coherent folded lobe"
        );
        assert!(
            (hybrid.scaled_value(60, 50).expect("hybrid background") + 5.0).abs() < 1.0,
            "the legitimate surrounding inbound field must stay on its original branch"
        );
    }

    #[test]
    fn stale_temporal_volume_is_ignored() {
        let base_time = chrono::DateTime::<Utc>::UNIX_EPOCH + Duration::hours(3);
        let mut previous = RadarVolume::new(RadarSite::new("TEST"), base_time);
        previous.cuts = vec![tilt_with_wind(1.23, 35.0, 180.0, 40.0, 360, 80)];
        let mut current =
            RadarVolume::new(RadarSite::new("TEST"), base_time + Duration::minutes(30));
        current.cuts = vec![tilt_with_wind(1.23, 15.0, 180.0, 25.0, 360, 80)];

        let hybrid = dealias_velocity_grid_hybrid(&current, 0, Some(&previous)).expect("hybrid");
        let region = dealias_velocity_grid(
            &current.cuts[0],
            current.cuts[0]
                .moments
                .get(&MomentType::Velocity)
                .expect("velocity"),
        );
        assert_eq!(hybrid.storage, region.storage);
    }
}
