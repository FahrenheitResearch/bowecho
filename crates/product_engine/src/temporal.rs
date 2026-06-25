//! Operations across already co-registered moment grids.
//!
//! These functions intentionally require identical polar geometry. Motion
//! compensation, Cartesian mosaicking, and multi-radar blending belong in a
//! separate geospatial layer; silently combining mismatched polar gates would
//! produce plausible-looking but incorrect products.

use radar_core::{MomentGrid, MomentStorage, MomentType};

pub fn difference_grid(
    newer: &MomentGrid,
    older: &MomentGrid,
    output_moment: MomentType,
) -> Option<MomentGrid> {
    binary_grid(newer, older, output_moment, |new, old| new - old)
}

/// Difference normalized to units per hour.
pub fn trend_grid(
    newer: &MomentGrid,
    older: &MomentGrid,
    elapsed_seconds: f64,
    output_moment: MomentType,
) -> Option<MomentGrid> {
    if !elapsed_seconds.is_finite() || elapsed_seconds <= 0.0 {
        return None;
    }
    let hours = (elapsed_seconds / 3600.0) as f32;
    binary_grid(newer, older, output_moment, |new, old| (new - old) / hours)
}

pub fn maximum_swath_grid(grids: &[&MomentGrid], output_moment: MomentType) -> Option<MomentGrid> {
    aggregate_grid(grids, output_moment, Aggregate::Maximum)
}

pub fn minimum_swath_grid(grids: &[&MomentGrid], output_moment: MomentType) -> Option<MomentGrid> {
    aggregate_grid(grids, output_moment, Aggregate::Minimum)
}

pub fn mean_grid(grids: &[&MomentGrid], output_moment: MomentType) -> Option<MomentGrid> {
    aggregate_grid(grids, output_moment, Aggregate::Mean)
}

/// Integrate rate grids (for example mm/h) using trapezoids between frame
/// timestamps. Timestamps are arbitrary monotonically increasing seconds.
pub fn accumulate_rate_grids(
    frames: &[(&MomentGrid, f64)],
    output_moment: MomentType,
) -> Option<MomentGrid> {
    let (first, _) = *frames.first()?;
    if frames.len() < 2
        || frames
            .iter()
            .any(|(grid, _)| !geometry_matches(first, grid))
    {
        return None;
    }
    if frames.windows(2).any(|window| {
        !window[0].1.is_finite() || !window[1].1.is_finite() || window[1].1 <= window[0].1
    }) {
        return None;
    }

    let len = value_len(first);
    let mut accumulated = vec![0.0f32; len];
    let mut seen = vec![false; len];
    for window in frames.windows(2) {
        let (left, left_time) = window[0];
        let (right, right_time) = window[1];
        let elapsed_hours = ((right_time - left_time) / 3600.0) as f32;
        for index in 0..len {
            let Some(left_rate) = flat_value(left, index) else {
                continue;
            };
            let Some(right_rate) = flat_value(right, index) else {
                continue;
            };
            if left_rate.is_finite() && right_rate.is_finite() {
                accumulated[index] +=
                    0.5 * (left_rate.max(0.0) + right_rate.max(0.0)) * elapsed_hours;
                seen[index] = true;
            }
        }
    }
    for (value, was_seen) in accumulated.iter_mut().zip(seen) {
        if !was_seen {
            *value = f32::NAN;
        }
    }
    Some(f32_grid_like(first, output_moment, accumulated))
}

/// Time above a threshold, in minutes, using linear occupancy between frames.
pub fn exceedance_duration_grid(
    frames: &[(&MomentGrid, f64)],
    threshold: f32,
    output_moment: MomentType,
) -> Option<MomentGrid> {
    let (first, _) = *frames.first()?;
    if frames.len() < 2
        || frames
            .iter()
            .any(|(grid, _)| !geometry_matches(first, grid))
    {
        return None;
    }
    if frames.windows(2).any(|window| {
        !window[0].1.is_finite() || !window[1].1.is_finite() || window[1].1 <= window[0].1
    }) {
        return None;
    }

    let len = value_len(first);
    let mut minutes = vec![0.0f32; len];
    let mut seen = vec![false; len];
    for window in frames.windows(2) {
        let (left, left_time) = window[0];
        let (right, right_time) = window[1];
        let elapsed_minutes = ((right_time - left_time) / 60.0) as f32;
        for index in 0..len {
            let Some(left_value) = flat_value(left, index) else {
                continue;
            };
            let Some(right_value) = flat_value(right, index) else {
                continue;
            };
            if !left_value.is_finite() || !right_value.is_finite() {
                continue;
            }
            let occupancy = match (left_value >= threshold, right_value >= threshold) {
                (true, true) => 1.0,
                (false, false) => 0.0,
                _ => 0.5,
            };
            minutes[index] += occupancy * elapsed_minutes;
            seen[index] = true;
        }
    }
    for (value, was_seen) in minutes.iter_mut().zip(seen) {
        if !was_seen {
            *value = f32::NAN;
        }
    }
    Some(f32_grid_like(first, output_moment, minutes))
}

/// Fraction of grids meeting a threshold, expressed as 0-100 percent.
pub fn exceedance_probability_grid(
    grids: &[&MomentGrid],
    threshold: f32,
    output_moment: MomentType,
) -> Option<MomentGrid> {
    let first = *grids.first()?;
    if grids.iter().any(|grid| !geometry_matches(first, grid)) {
        return None;
    }
    let len = value_len(first);
    let mut out = vec![f32::NAN; len];
    for (index, cell) in out.iter_mut().enumerate() {
        let mut valid = 0usize;
        let mut exceeded = 0usize;
        for grid in grids {
            if let Some(value) = flat_value(grid, index)
                && value.is_finite()
            {
                valid += 1;
                exceeded += usize::from(value >= threshold);
            }
        }
        if valid > 0 {
            *cell = 100.0 * exceeded as f32 / valid as f32;
        }
    }
    Some(f32_grid_like(first, output_moment, out))
}

enum Aggregate {
    Maximum,
    Minimum,
    Mean,
}

fn aggregate_grid(
    grids: &[&MomentGrid],
    output_moment: MomentType,
    aggregate: Aggregate,
) -> Option<MomentGrid> {
    let first = *grids.first()?;
    if grids.iter().any(|grid| !geometry_matches(first, grid)) {
        return None;
    }
    let len = value_len(first);
    let mut out = vec![f32::NAN; len];
    for (index, cell) in out.iter_mut().enumerate() {
        let values = grids
            .iter()
            .filter_map(|grid| flat_value(grid, index))
            .filter(|value| value.is_finite())
            .collect::<Vec<_>>();
        if values.is_empty() {
            continue;
        }
        *cell = match aggregate {
            Aggregate::Maximum => values.into_iter().fold(f32::NEG_INFINITY, f32::max),
            Aggregate::Minimum => values.into_iter().fold(f32::INFINITY, f32::min),
            Aggregate::Mean => values.iter().sum::<f32>() / values.len() as f32,
        };
    }
    Some(f32_grid_like(first, output_moment, out))
}

fn binary_grid(
    left: &MomentGrid,
    right: &MomentGrid,
    output_moment: MomentType,
    operation: impl Fn(f32, f32) -> f32,
) -> Option<MomentGrid> {
    if !geometry_matches(left, right) {
        return None;
    }
    let len = value_len(left);
    let mut out = vec![f32::NAN; len];
    for (index, cell) in out.iter_mut().enumerate() {
        let Some(left_value) = flat_value(left, index) else {
            continue;
        };
        let Some(right_value) = flat_value(right, index) else {
            continue;
        };
        if left_value.is_finite() && right_value.is_finite() {
            *cell = operation(left_value, right_value);
        }
    }
    Some(f32_grid_like(left, output_moment, out))
}

fn geometry_matches(left: &MomentGrid, right: &MomentGrid) -> bool {
    left.gate_range == right.gate_range && left.radial_indices == right.radial_indices
}

fn value_len(grid: &MomentGrid) -> usize {
    grid.radial_count() * grid.gate_range.gate_count
}

fn flat_value(grid: &MomentGrid, index: usize) -> Option<f32> {
    let gates = grid.gate_range.gate_count;
    (gates > 0).then_some(())?;
    grid.scaled_value(index / gates, index % gates)
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

#[cfg(test)]
mod tests {
    use super::*;
    use radar_core::GateRange;

    fn grid(values: &[f32]) -> MomentGrid {
        MomentGrid {
            moment: MomentType::Reflectivity,
            gate_range: GateRange {
                first_gate_m: 0,
                gate_spacing_m: 1000,
                gate_count: values.len(),
            },
            scale: 1.0,
            offset: 0.0,
            nodata: None,
            range_folded: None,
            radial_indices: vec![0],
            storage: MomentStorage::F32(values.to_vec()),
        }
    }

    #[test]
    fn difference_and_trend() {
        let older = grid(&[1.0, 2.0]);
        let newer = grid(&[3.0, 6.0]);
        let difference =
            difference_grid(&newer, &older, MomentType::Unknown("DIFF".to_owned())).unwrap();
        assert_eq!(difference.scaled_value(0, 0), Some(2.0));
        let trend = trend_grid(
            &newer,
            &older,
            1800.0,
            MomentType::Unknown("TREND".to_owned()),
        )
        .unwrap();
        assert_eq!(trend.scaled_value(0, 1), Some(8.0));
    }

    #[test]
    fn rate_accumulation_uses_trapezoids() {
        let first = grid(&[10.0]);
        let second = grid(&[20.0]);
        let accumulation = accumulate_rate_grids(
            &[(&first, 0.0), (&second, 3600.0)],
            MomentType::Unknown("ACCUM".to_owned()),
        )
        .unwrap();
        assert_eq!(accumulation.scaled_value(0, 0), Some(15.0));
    }

    #[test]
    fn probability_ignores_missing_values() {
        let first = grid(&[1.0, f32::NAN]);
        let second = grid(&[3.0, 4.0]);
        let probability = exceedance_probability_grid(
            &[&first, &second],
            2.0,
            MomentType::Unknown("PROB".to_owned()),
        )
        .unwrap();
        assert_eq!(probability.scaled_value(0, 0), Some(50.0));
        assert_eq!(probability.scaled_value(0, 1), Some(100.0));
    }
}
