//! Volume/column products built from multiple elevation cuts.
//!
//! BowEcho already has optimized implementations of CREF, echo tops, VIL,
//! VIL density, SHI/MESH/POSH/POH, and cross sections in `render2d`. This
//! module adds generic CAPPI/column statistics plus echo-base/depth and height
//! of maximum reflectivity without coupling `product_engine` back to the
//! renderer.

use radar_core::{
    MomentGrid, MomentStorage, MomentType, RadarVolume, beam_ground_range_m,
    beam_height_above_radar_m,
};

const EFFECTIVE_EARTH_RADIUS_M: f64 = 4.0 / 3.0 * 6_371_000.0;
const HALF_BEAMWIDTH_RAD: f64 = 0.475 * std::f64::consts::PI / 180.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CappiInterpolation {
    Nearest,
    /// Interpolate between bracketing tilts in elevation-angle space.
    LinearElevation,
}

struct CutSampler<'a> {
    elevation_deg: f32,
    grid: &'a MomentGrid,
    azimuth_rows: Vec<(f32, usize)>,
    ground_range_m: Vec<f64>,
    height_m: Vec<f64>,
}

impl<'a> CutSampler<'a> {
    fn new(volume: &'a RadarVolume, cut_index: usize, grid: &'a MomentGrid) -> Option<Self> {
        let cut = volume.cuts.get(cut_index)?;
        if grid.gate_range.gate_count == 0 || grid.gate_range.gate_spacing_m <= 0 {
            return None;
        }
        let mut azimuth_rows = grid
            .radial_indices
            .iter()
            .copied()
            .enumerate()
            .filter_map(|(row, radial_index)| {
                cut.radials
                    .get(radial_index)
                    .map(|radial| (radial.azimuth_deg.rem_euclid(360.0), row))
            })
            .collect::<Vec<_>>();
        if azimuth_rows.is_empty() {
            return None;
        }
        azimuth_rows.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut ground_range_m = Vec::with_capacity(grid.gate_range.gate_count);
        let mut height_m = Vec::with_capacity(grid.gate_range.gate_count);
        for gate in 0..grid.gate_range.gate_count {
            let slant_range_m = grid.gate_range.first_gate_m as f64
                + gate as f64 * grid.gate_range.gate_spacing_m as f64;
            ground_range_m.push(beam_ground_range_m(slant_range_m, cut.elevation_deg as f64));
            height_m.push(beam_height_above_radar_m(
                slant_range_m,
                cut.elevation_deg as f64,
            ));
        }
        Some(Self {
            elevation_deg: cut.elevation_deg,
            grid,
            azimuth_rows,
            ground_range_m,
            height_m,
        })
    }

    fn nearest_row(&self, azimuth_deg: f32) -> usize {
        match self
            .azimuth_rows
            .binary_search_by(|entry| entry.0.total_cmp(&azimuth_deg))
        {
            Ok(index) => self.azimuth_rows[index].1,
            Err(index) => {
                let lower = if index == 0 {
                    self.azimuth_rows.len() - 1
                } else {
                    index - 1
                };
                let upper = if index >= self.azimuth_rows.len() {
                    0
                } else {
                    index
                };
                if angular_distance(self.azimuth_rows[lower].0, azimuth_deg)
                    <= angular_distance(self.azimuth_rows[upper].0, azimuth_deg)
                {
                    self.azimuth_rows[lower].1
                } else {
                    self.azimuth_rows[upper].1
                }
            }
        }
    }

    fn gate_for_ground_range(&self, ground_range_m: f64) -> Option<usize> {
        let count = self.ground_range_m.len();
        if count == 0 {
            return None;
        }
        let half_gate = if count > 1 {
            0.5 * (self.ground_range_m[1] - self.ground_range_m[0]).abs()
        } else {
            0.0
        };
        if ground_range_m < self.ground_range_m[0] - half_gate
            || ground_range_m > self.ground_range_m[count - 1] + half_gate
        {
            return None;
        }
        match self
            .ground_range_m
            .binary_search_by(|value| value.total_cmp(&ground_range_m))
        {
            Ok(index) => Some(index),
            Err(0) => Some(0),
            Err(index) if index >= count => Some(count - 1),
            Err(index) => {
                let lower_distance = ground_range_m - self.ground_range_m[index - 1];
                let upper_distance = self.ground_range_m[index] - ground_range_m;
                Some(if lower_distance <= upper_distance {
                    index - 1
                } else {
                    index
                })
            }
        }
    }

    fn sample(&self, azimuth_deg: f32, ground_range_m: f64) -> Option<ColumnSample> {
        let gate = self.gate_for_ground_range(ground_range_m)?;
        let row = self.nearest_row(azimuth_deg);
        let value = self.grid.scaled_value(row, gate)?;
        if !value.is_finite() {
            return None;
        }
        let slant_range_m = self.grid.gate_range.first_gate_m as f64
            + gate as f64 * self.grid.gate_range.gate_spacing_m as f64;
        Some(ColumnSample {
            height_m: self.height_m[gate],
            elevation_deg: self.elevation_deg as f64,
            slant_range_m,
            value,
        })
    }

    fn row_azimuths(&self) -> Vec<f32> {
        let mut out = vec![f32::NAN; self.grid.radial_count()];
        for &(azimuth, row) in &self.azimuth_rows {
            if row < out.len() {
                out[row] = azimuth;
            }
        }
        out
    }
}

#[derive(Clone, Copy)]
struct ColumnSample {
    height_m: f64,
    elevation_deg: f64,
    slant_range_m: f64,
    value: f32,
}

pub fn cappi_grid(
    volume: &RadarVolume,
    moment: MomentType,
    height_m: f32,
    interpolation: CappiInterpolation,
) -> Option<MomentGrid> {
    if !height_m.is_finite() || height_m < 0.0 {
        return None;
    }
    let (base_index, base_grid) = base_cut(volume, &moment)?;
    let base = CutSampler::new(volume, base_index, base_grid)?;
    let columns = moment_columns(volume, &moment);
    let rows = base_grid.radial_count();
    let gates = base_grid.gate_range.gate_count;
    let azimuths = base.row_azimuths();
    let mut out = vec![f32::NAN; rows * gates];
    for row in 0..rows {
        let azimuth = azimuths[row];
        if !azimuth.is_finite() {
            continue;
        }
        for gate in 0..gates {
            let ground_range = base.ground_range_m[gate];
            let samples = column_profile(&columns, azimuth, ground_range);
            if let Some(value) =
                interpolate_cappi(&samples, ground_range, height_m as f64, interpolation)
            {
                out[row * gates + gate] = value;
            }
        }
    }
    let id = format!("CAPPI_{}_{:.1}KM", moment.short_name(), height_m / 1000.0);
    Some(f32_grid_like(base_grid, MomentType::Unknown(id), out))
}

pub fn column_max_grid(volume: &RadarVolume, moment: MomentType) -> Option<MomentGrid> {
    column_stat_grid(volume, moment, ColumnStatistic::Maximum)
}

pub fn column_min_grid(volume: &RadarVolume, moment: MomentType) -> Option<MomentGrid> {
    column_stat_grid(volume, moment, ColumnStatistic::Minimum)
}

pub fn column_mean_grid(volume: &RadarVolume, moment: MomentType) -> Option<MomentGrid> {
    column_stat_grid(volume, moment, ColumnStatistic::Mean)
}

pub fn low_level_composite_reflectivity_grid(
    volume: &RadarVolume,
    maximum_height_m: f32,
) -> Option<MomentGrid> {
    if !maximum_height_m.is_finite() || maximum_height_m <= 0.0 {
        return None;
    }
    let moment = MomentType::Reflectivity;
    let (base_index, base_grid) = base_cut(volume, &moment)?;
    let base = CutSampler::new(volume, base_index, base_grid)?;
    let columns = moment_columns(volume, &moment);
    let rows = base_grid.radial_count();
    let gates = base_grid.gate_range.gate_count;
    let azimuths = base.row_azimuths();
    let mut out = vec![f32::NAN; rows * gates];
    for row in 0..rows {
        let azimuth = azimuths[row];
        if !azimuth.is_finite() {
            continue;
        }
        for gate in 0..gates {
            let maximum = column_profile(&columns, azimuth, base.ground_range_m[gate])
                .into_iter()
                .filter(|sample| sample.height_m <= maximum_height_m as f64)
                .map(|sample| sample.value)
                .fold(f32::NEG_INFINITY, f32::max);
            if maximum.is_finite() {
                out[row * gates + gate] = maximum;
            }
        }
    }
    Some(f32_grid_like(
        base_grid,
        MomentType::Unknown("LLCREF".to_owned()),
        out,
    ))
}

pub fn echo_base_grid(volume: &RadarVolume, threshold_dbz: f32) -> Option<MomentGrid> {
    echo_boundary_grid(volume, threshold_dbz, EchoBoundary::Base)
}

pub fn echo_top_height_grid(volume: &RadarVolume, threshold_dbz: f32) -> Option<MomentGrid> {
    echo_boundary_grid(volume, threshold_dbz, EchoBoundary::Top)
}

pub fn echo_depth_grid(volume: &RadarVolume, threshold_dbz: f32) -> Option<MomentGrid> {
    let base = echo_base_grid(volume, threshold_dbz)?;
    let top = echo_top_height_grid(volume, threshold_dbz)?;
    if base.gate_range != top.gate_range || base.radial_indices != top.radial_indices {
        return None;
    }
    let rows = base.radial_count();
    let gates = base.gate_range.gate_count;
    let mut out = vec![f32::NAN; rows * gates];
    for row in 0..rows {
        for gate in 0..gates {
            if let (Some(base_height), Some(top_height)) =
                (base.scaled_value(row, gate), top.scaled_value(row, gate))
                && base_height.is_finite()
                && top_height.is_finite()
            {
                out[row * gates + gate] = (top_height - base_height).max(0.0);
            }
        }
    }
    Some(f32_grid_like(
        &base,
        MomentType::Unknown("EDEPTH".to_owned()),
        out,
    ))
}

pub fn height_of_max_reflectivity_grid(volume: &RadarVolume) -> Option<MomentGrid> {
    let moment = MomentType::Reflectivity;
    let (base_index, base_grid) = base_cut(volume, &moment)?;
    let base = CutSampler::new(volume, base_index, base_grid)?;
    let columns = moment_columns(volume, &moment);
    let rows = base_grid.radial_count();
    let gates = base_grid.gate_range.gate_count;
    let azimuths = base.row_azimuths();
    let mut out = vec![f32::NAN; rows * gates];
    for row in 0..rows {
        let azimuth = azimuths[row];
        if !azimuth.is_finite() {
            continue;
        }
        for gate in 0..gates {
            let best = column_profile(&columns, azimuth, base.ground_range_m[gate])
                .into_iter()
                .max_by(|left, right| left.value.total_cmp(&right.value));
            if let Some(best) = best {
                out[row * gates + gate] = best.height_m as f32;
            }
        }
    }
    Some(f32_grid_like(
        base_grid,
        MomentType::Unknown("HMAX".to_owned()),
        out,
    ))
}

#[derive(Clone, Copy)]
enum ColumnStatistic {
    Maximum,
    Minimum,
    Mean,
}

fn column_stat_grid(
    volume: &RadarVolume,
    moment: MomentType,
    statistic: ColumnStatistic,
) -> Option<MomentGrid> {
    let (base_index, base_grid) = base_cut(volume, &moment)?;
    let base = CutSampler::new(volume, base_index, base_grid)?;
    let columns = moment_columns(volume, &moment);
    let rows = base_grid.radial_count();
    let gates = base_grid.gate_range.gate_count;
    let azimuths = base.row_azimuths();
    let mut out = vec![f32::NAN; rows * gates];
    for row in 0..rows {
        let azimuth = azimuths[row];
        if !azimuth.is_finite() {
            continue;
        }
        for gate in 0..gates {
            let values = column_profile(&columns, azimuth, base.ground_range_m[gate])
                .into_iter()
                .map(|sample| sample.value)
                .collect::<Vec<_>>();
            if values.is_empty() {
                continue;
            }
            out[row * gates + gate] = match statistic {
                ColumnStatistic::Maximum => values.into_iter().fold(f32::NEG_INFINITY, f32::max),
                ColumnStatistic::Minimum => values.into_iter().fold(f32::INFINITY, f32::min),
                ColumnStatistic::Mean => values.iter().sum::<f32>() / values.len() as f32,
            };
        }
    }
    let prefix = match statistic {
        ColumnStatistic::Maximum => "CMAX",
        ColumnStatistic::Minimum => "CMIN",
        ColumnStatistic::Mean => "CMEAN",
    };
    Some(f32_grid_like(
        base_grid,
        MomentType::Unknown(format!("{prefix}_{}", moment.short_name())),
        out,
    ))
}

#[derive(Clone, Copy)]
enum EchoBoundary {
    Base,
    Top,
}

fn echo_boundary_grid(
    volume: &RadarVolume,
    threshold_dbz: f32,
    boundary: EchoBoundary,
) -> Option<MomentGrid> {
    let moment = MomentType::Reflectivity;
    let (base_index, base_grid) = base_cut(volume, &moment)?;
    let base = CutSampler::new(volume, base_index, base_grid)?;
    let columns = moment_columns(volume, &moment);
    let rows = base_grid.radial_count();
    let gates = base_grid.gate_range.gate_count;
    let azimuths = base.row_azimuths();
    let mut out = vec![f32::NAN; rows * gates];
    for row in 0..rows {
        let azimuth = azimuths[row];
        if !azimuth.is_finite() {
            continue;
        }
        for gate in 0..gates {
            let heights = column_profile(&columns, azimuth, base.ground_range_m[gate])
                .into_iter()
                .filter(|sample| sample.value >= threshold_dbz)
                .map(|sample| sample.height_m)
                .collect::<Vec<_>>();
            if heights.is_empty() {
                continue;
            }
            let height = match boundary {
                EchoBoundary::Base => heights.into_iter().fold(f64::INFINITY, f64::min),
                EchoBoundary::Top => heights.into_iter().fold(f64::NEG_INFINITY, f64::max),
            };
            out[row * gates + gate] = height as f32;
        }
    }
    let id = match boundary {
        EchoBoundary::Base => "EBASE",
        EchoBoundary::Top => "ET",
    };
    Some(f32_grid_like(
        base_grid,
        MomentType::Unknown(id.to_owned()),
        out,
    ))
}

fn base_cut<'a>(volume: &'a RadarVolume, moment: &MomentType) -> Option<(usize, &'a MomentGrid)> {
    volume
        .cuts
        .iter()
        .enumerate()
        .filter_map(|(index, cut)| {
            cut.moments
                .get(moment)
                .map(|grid| (index, cut.elevation_deg, grid))
        })
        .min_by(|left, right| left.1.total_cmp(&right.1))
        .map(|(index, _, grid)| (index, grid))
}

fn moment_columns<'a>(volume: &'a RadarVolume, moment: &MomentType) -> Vec<CutSampler<'a>> {
    let mut columns = volume
        .cuts
        .iter()
        .enumerate()
        .filter_map(|(index, cut)| {
            let grid = cut.moments.get(moment)?;
            CutSampler::new(volume, index, grid)
        })
        .collect::<Vec<_>>();
    columns.sort_by(|left, right| left.elevation_deg.total_cmp(&right.elevation_deg));
    columns
}

fn column_profile(
    columns: &[CutSampler<'_>],
    azimuth_deg: f32,
    ground_range_m: f64,
) -> Vec<ColumnSample> {
    let mut samples = columns
        .iter()
        .filter_map(|column| column.sample(azimuth_deg, ground_range_m))
        .collect::<Vec<_>>();
    samples.sort_by(|left, right| left.height_m.total_cmp(&right.height_m));
    samples
}

fn interpolate_cappi(
    samples: &[ColumnSample],
    ground_range_m: f64,
    target_height_m: f64,
    interpolation: CappiInterpolation,
) -> Option<f32> {
    let first = *samples.first()?;
    let last = *samples.last()?;
    if samples.len() == 1 {
        let allowance = (first.slant_range_m * HALF_BEAMWIDTH_RAD).max(250.0);
        return ((first.height_m - target_height_m).abs() <= allowance).then_some(first.value);
    }
    if target_height_m <= first.height_m {
        let allowance = (first.slant_range_m * HALF_BEAMWIDTH_RAD).max(250.0);
        return (first.height_m - target_height_m <= allowance).then_some(first.value);
    }
    if target_height_m >= last.height_m {
        let allowance = last.slant_range_m * HALF_BEAMWIDTH_RAD;
        return (target_height_m - last.height_m <= allowance).then_some(last.value);
    }
    for pair in samples.windows(2) {
        let lower = pair[0];
        let upper = pair[1];
        if target_height_m < lower.height_m || target_height_m > upper.height_m {
            continue;
        }
        return match interpolation {
            CappiInterpolation::Nearest => {
                if target_height_m - lower.height_m <= upper.height_m - target_height_m {
                    Some(lower.value)
                } else {
                    Some(upper.value)
                }
            }
            CappiInterpolation::LinearElevation => {
                let (_, target_elevation_deg) = invert_beam(ground_range_m, target_height_m);
                let span = upper.elevation_deg - lower.elevation_deg;
                if span.abs() <= 1.0e-8 {
                    Some(lower.value)
                } else {
                    let weight = ((target_elevation_deg - lower.elevation_deg) / span)
                        .clamp(0.0, 1.0) as f32;
                    Some(lower.value + weight * (upper.value - lower.value))
                }
            }
        };
    }
    None
}

fn invert_beam(ground_range_m: f64, height_m: f64) -> (f64, f64) {
    let central_angle = ground_range_m / EFFECTIVE_EARTH_RADIUS_M;
    let slant_range = (EFFECTIVE_EARTH_RADIUS_M.powi(2)
        + (EFFECTIVE_EARTH_RADIUS_M + height_m).powi(2)
        - 2.0
            * EFFECTIVE_EARTH_RADIUS_M
            * (EFFECTIVE_EARTH_RADIUS_M + height_m)
            * central_angle.cos())
    .max(0.0)
    .sqrt();
    if slant_range < 1.0 {
        return (0.0, 90.0);
    }
    let sine = (((EFFECTIVE_EARTH_RADIUS_M + height_m).powi(2)
        - EFFECTIVE_EARTH_RADIUS_M.powi(2)
        - slant_range.powi(2))
        / (2.0 * EFFECTIVE_EARTH_RADIUS_M * slant_range))
        .clamp(-1.0, 1.0);
    (slant_range, sine.asin().to_degrees())
}

fn angular_distance(left: f32, right: f32) -> f32 {
    let difference = (left - right).abs().rem_euclid(360.0);
    difference.min(360.0 - difference)
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
    use radar_core::{ElevationCut, GateRange, RadarSite, Radial};

    fn test_volume() -> RadarVolume {
        let mut volume = RadarVolume {
            site: RadarSite::new("TEST"),
            ..RadarVolume::default()
        };
        for (elevation, value) in [(0.5, 30.0), (2.5, 50.0)] {
            let mut cut = ElevationCut::new(elevation, None);
            cut.radials.push(Radial {
                azimuth_deg: 0.0,
                elevation_deg: elevation,
                time_offset_ms: 0,
                gate_range: GateRange {
                    first_gate_m: 1000,
                    gate_spacing_m: 1000,
                    gate_count: 5,
                },
                nyquist_velocity_mps: None,
                radial_status: None,
            });
            cut.moments.insert(
                MomentType::Reflectivity,
                MomentGrid {
                    moment: MomentType::Reflectivity,
                    gate_range: GateRange {
                        first_gate_m: 1000,
                        gate_spacing_m: 1000,
                        gate_count: 5,
                    },
                    scale: 1.0,
                    offset: 0.0,
                    nodata: None,
                    range_folded: None,
                    radial_indices: vec![0],
                    storage: MomentStorage::F32(vec![value; 5]),
                },
            );
            volume.cuts.push(cut);
        }
        volume
    }

    #[test]
    fn column_max_finds_upper_tilt_value() {
        let volume = test_volume();
        let maximum = column_max_grid(&volume, MomentType::Reflectivity).unwrap();
        assert_eq!(maximum.scaled_value(0, 3), Some(50.0));
    }

    #[test]
    fn echo_depth_is_nonnegative() {
        let volume = test_volume();
        let depth = echo_depth_grid(&volume, 20.0).unwrap();
        assert!(depth.scaled_value(0, 3).unwrap() >= 0.0);
    }
}
