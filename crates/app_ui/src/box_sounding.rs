//! Area-mean model soundings built from rw-store primitive columns.
//!
//! The sounding renderer derives parcels and diagnostics from [`SoundingData`].
//! This module deliberately works one layer below that: it selects grid cells
//! inside a geographic rectangle, averages the stored thermodynamic and wind
//! primitives over a common finite-cell mask at each pressure level, and only
//! then hands the representative column to the existing sounding path.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::Instant;

use eframe::egui;
use rw_ui::rw_store::format::{COL_X, COL_Y};
use rw_ui::rw_store::grid::GridFile;
use rw_ui::rw_store::reader::HourReader;
use rw_ui::{HourKey, ProfileVar, SoundingData, StoreView, SurfaceSample};

const MAX_RETAINED_PROFILE_BYTES: usize = 512 * 1024 * 1024;
const COLUMN_CHUNK_CELLS: usize = COL_X * COL_Y;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct BoxBounds {
    pub west: f64,
    pub east: f64,
    pub south: f64,
    pub north: f64,
}

impl BoxBounds {
    pub(crate) fn new(bounds: (f64, f64, f64, f64)) -> Result<Self, String> {
        let (west, east, south, north) = bounds;
        if ![west, east, south, north].into_iter().all(f64::is_finite) {
            return Err("box-sounding bounds must be finite".to_owned());
        }
        if west >= east || south >= north {
            return Err(format!(
                "box-sounding bounds are empty: west={west}, east={east}, south={south}, north={north}"
            ));
        }
        Ok(Self {
            west,
            east,
            south,
            north,
        })
    }

    fn contains(self, lat: f32, lon: f32) -> bool {
        let (lat, lon) = (f64::from(lat), f64::from(lon));
        lat.is_finite()
            && lon.is_finite()
            && (self.south..=self.north).contains(&lat)
            && (self.west..=self.east).contains(&lon)
    }

    pub(crate) fn label(self) -> String {
        format!(
            "{:.2}..{:.2} N, {:.2}..{:.2} E",
            self.south, self.north, self.west, self.east
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BoxSoundingSummary {
    pub hour: HourKey,
    pub requested: BoxBounds,
    pub sampled: BoxBounds,
    pub selected_cells: usize,
    pub surface_cells: usize,
    pub min_level_cells: usize,
    pub max_level_cells: usize,
    pub usable_levels: usize,
    pub clipped_to_grid: bool,
    pub moisture_name: String,
    pub used_approx_surface: bool,
    pub read_ms: f32,
}

impl BoxSoundingSummary {
    pub(crate) fn missing_surface_cells(&self) -> usize {
        self.selected_cells.saturating_sub(self.surface_cells)
    }

    pub(crate) fn method_label(&self) -> &'static str {
        "Arithmetic mean of stored T, moisture, u, v, height and surface primitives over common finite cells at each pressure level; sounding diagnostics are derived afterward."
    }
}

#[derive(Debug)]
pub(crate) struct BoxSoundingResult {
    pub data: SoundingData,
    pub summary: BoxSoundingSummary,
}

pub(crate) struct BoxSoundingTask {
    receiver: Receiver<Result<BoxSoundingResult, String>>,
}

impl BoxSoundingTask {
    pub(crate) fn spawn(
        store_root: PathBuf,
        hour: HourKey,
        bounds: BoxBounds,
        repaint: egui::Context,
    ) -> Self {
        let (sender, receiver) = channel();
        std::thread::Builder::new()
            .name("bowecho-box-sounding".to_owned())
            .spawn(move || {
                let result = build_box_sounding(&StoreView::new(store_root), hour, bounds);
                let _ = sender.send(result);
                repaint.request_repaint();
            })
            .ok();
        Self { receiver }
    }

    pub(crate) fn try_recv(&self) -> Option<Result<BoxSoundingResult, String>> {
        match self.receiver.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(
                "box-sounding worker stopped before returning a result".to_owned(),
            )),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct SelectedCell {
    index: usize,
    x: usize,
    y: usize,
}

fn select_cells(grid: &GridFile, bounds: BoxBounds) -> Vec<SelectedCell> {
    grid.lat
        .iter()
        .zip(&grid.lon)
        .enumerate()
        .filter_map(|(index, (&lat, &lon))| {
            bounds.contains(lat, lon).then_some(SelectedCell {
                index,
                x: index % grid.nx,
                y: index / grid.nx,
            })
        })
        .collect()
}

fn finite_extent(grid: &GridFile) -> Option<BoxBounds> {
    finite_extent_for_indices(grid, 0..grid.lat.len())
}

fn finite_extent_for_indices(
    grid: &GridFile,
    indices: impl IntoIterator<Item = usize>,
) -> Option<BoxBounds> {
    let mut west = f64::INFINITY;
    let mut east = f64::NEG_INFINITY;
    let mut south = f64::INFINITY;
    let mut north = f64::NEG_INFINITY;
    for index in indices {
        let (Some(&lat), Some(&lon)) = (grid.lat.get(index), grid.lon.get(index)) else {
            continue;
        };
        let (lat, lon) = (f64::from(lat), f64::from(lon));
        if !lat.is_finite() || !lon.is_finite() {
            continue;
        }
        west = west.min(lon);
        east = east.max(lon);
        south = south.min(lat);
        north = north.max(lat);
    }
    [west, east, south, north]
        .into_iter()
        .all(f64::is_finite)
        .then_some(BoxBounds {
            west,
            east,
            south,
            north,
        })
}

#[derive(Debug)]
struct PrimitiveMatrix {
    name: String,
    units: String,
    levels: Vec<u16>,
    /// `[common level][selected cell]`.
    values: Vec<f32>,
}

fn common_levels(reader: &HourReader, names: &[&str]) -> Result<Vec<u16>, String> {
    let first = reader
        .variable(names[0])
        .ok_or_else(|| format!("required sounding field '{}' is absent", names[0]))?;
    let mut levels = first.levels_hpa.clone();
    for &name in &names[1..] {
        let var = reader
            .variable(name)
            .ok_or_else(|| format!("required sounding field '{name}' is absent"))?;
        if var.kind != "pressure3d" {
            return Err(format!(
                "required sounding field '{name}' is not a pressure column"
            ));
        }
        let available: BTreeSet<u16> = var.levels_hpa.iter().copied().collect();
        levels.retain(|level| available.contains(level));
    }
    if levels.len() < 2 {
        return Err("model timestep has fewer than two common sounding pressure levels".to_owned());
    }
    Ok(levels)
}

fn read_primitive_matrix(
    reader: &HourReader,
    name: &str,
    levels: &[u16],
    selected: &[SelectedCell],
) -> Result<PrimitiveMatrix, String> {
    let var = reader
        .variable(name)
        .ok_or_else(|| format!("required sounding field '{name}' is absent"))?;
    if var.kind != "pressure3d" {
        return Err(format!(
            "required sounding field '{name}' is not a pressure column"
        ));
    }
    let source_levels: BTreeMap<u16, usize> = var
        .levels_hpa
        .iter()
        .copied()
        .enumerate()
        .map(|(index, level)| (level, index))
        .collect();
    let cells = reader
        .meta()
        .nx
        .checked_mul(reader.meta().ny)
        .ok_or_else(|| "model grid size overflows usize".to_owned())?;
    let mut values = vec![f32::NAN; levels.len().saturating_mul(selected.len())];
    // For a genuinely small box,
    // repeated column reads still decode much less than a CONUS-wide volume;
    // above the approximate break-even, decode every chunk once instead.
    if selected.len().saturating_mul(COLUMN_CHUNK_CELLS) < cells {
        for (selected_index, cell) in selected.iter().enumerate() {
            let column = reader
                .read_column_3d(name, cell.x, cell.y)
                .map_err(|error| format!("read {name} sounding column: {error}"))?;
            for (level_index, level) in levels.iter().enumerate() {
                let source_level = source_levels
                    .get(level)
                    .copied()
                    .ok_or_else(|| format!("{name} has no {level} hPa level"))?;
                values[level_index * selected.len() + selected_index] = column[source_level];
            }
        }
    } else {
        let volume = reader
            .read_full_3d(name)
            .map_err(|error| format!("read {name} sounding volume: {error}"))?;
        for (level_index, level) in levels.iter().enumerate() {
            let source_level = source_levels
                .get(level)
                .copied()
                .ok_or_else(|| format!("{name} has no {level} hPa level"))?;
            let plane = source_level * cells;
            for (selected_index, cell) in selected.iter().enumerate() {
                values[level_index * selected.len() + selected_index] = volume[plane + cell.index];
            }
        }
    }
    Ok(PrimitiveMatrix {
        name: name.to_owned(),
        units: var.units.clone(),
        levels: levels.to_vec(),
        values,
    })
}

fn aggregate_profile_matrices(
    matrices: &[PrimitiveMatrix],
    selected_cells: usize,
) -> Result<(Vec<ProfileVar>, Vec<usize>), String> {
    let Some(first) = matrices.first() else {
        return Err("no primitive sounding fields were supplied".to_owned());
    };
    if selected_cells == 0 {
        return Err("box contains no model grid cells".to_owned());
    }
    let levels = &first.levels;
    let expected = levels.len().saturating_mul(selected_cells);
    if matrices
        .iter()
        .any(|matrix| matrix.levels != *levels || matrix.values.len() != expected)
    {
        return Err(
            "box-sounding primitive matrices do not share one level/cell layout".to_owned(),
        );
    }

    let mut sums = vec![vec![0.0f64; levels.len()]; matrices.len()];
    let mut counts = vec![0usize; levels.len()];
    for level_index in 0..levels.len() {
        let start = level_index * selected_cells;
        for cell in 0..selected_cells {
            let index = start + cell;
            if matrices
                .iter()
                .all(|matrix| matrix.values[index].is_finite())
            {
                counts[level_index] += 1;
                for (matrix_index, matrix) in matrices.iter().enumerate() {
                    sums[matrix_index][level_index] += f64::from(matrix.values[index]);
                }
            }
        }
    }

    let vars = matrices
        .iter()
        .enumerate()
        .map(|(matrix_index, matrix)| ProfileVar {
            name: matrix.name.clone(),
            units: matrix.units.clone(),
            levels_hpa: levels.clone(),
            values: counts
                .iter()
                .enumerate()
                .map(|(level_index, &count)| {
                    if count == 0 {
                        f32::NAN
                    } else {
                        (sums[matrix_index][level_index] / count as f64) as f32
                    }
                })
                .collect(),
        })
        .collect();
    Ok((vars, counts))
}

#[derive(Debug)]
struct SurfaceCandidate {
    name: String,
    units: String,
    approximate: bool,
    values: Vec<f32>,
}

fn read_surface_candidate(
    reader: &HourReader,
    name: &str,
    approximate: bool,
    selected: &[SelectedCell],
) -> Result<Option<SurfaceCandidate>, String> {
    let Some(var) = reader.variable(name) else {
        return Ok(None);
    };
    if var.kind != "surface2d" {
        return Ok(None);
    }
    let min_x = selected.iter().map(|cell| cell.x).min().unwrap_or(0);
    let max_x = selected.iter().map(|cell| cell.x).max().unwrap_or(0);
    let min_y = selected.iter().map(|cell| cell.y).min().unwrap_or(0);
    let max_y = selected.iter().map(|cell| cell.y).max().unwrap_or(0);
    let window = reader
        .read_window_2d(name, min_x, min_y, max_x + 1, max_y + 1)
        .map_err(|error| format!("read {name} box window: {error}"))?;
    let values = selected
        .iter()
        .map(|cell| window.values[(cell.y - window.y0) * window.nx + (cell.x - window.x0)])
        .collect();
    Ok(Some(SurfaceCandidate {
        name: name.to_owned(),
        units: var.units.clone(),
        approximate,
        values,
    }))
}

fn best_surface_set(
    concepts: &[Vec<SurfaceCandidate>],
    selected_cells: usize,
) -> Result<(Vec<usize>, Vec<bool>), String> {
    if concepts.iter().any(Vec::is_empty) {
        return Err("model timestep lacks one or more required sounding surface fields".to_owned());
    }
    let combinations = concepts
        .iter()
        .try_fold(1usize, |count, choices| count.checked_mul(choices.len()))
        .ok_or_else(|| "surface-field choice count overflows usize".to_owned())?;
    let mut best: Option<(usize, usize, Vec<usize>, Vec<bool>)> = None;
    for ordinal in 0..combinations {
        let mut remainder = ordinal;
        let mut choice = Vec::with_capacity(concepts.len());
        for candidates in concepts {
            choice.push(remainder % candidates.len());
            remainder /= candidates.len();
        }
        let valid: Vec<bool> = (0..selected_cells)
            .map(|cell| {
                concepts
                    .iter()
                    .zip(&choice)
                    .all(|(candidates, &index)| candidates[index].values[cell].is_finite())
            })
            .collect();
        let valid_count = valid.iter().filter(|&&value| value).count();
        let exact_count = concepts
            .iter()
            .zip(&choice)
            .filter(|(candidates, index)| !candidates[**index].approximate)
            .count();
        if best.as_ref().is_none_or(|(best_valid, best_exact, _, _)| {
            (valid_count, exact_count) > (*best_valid, *best_exact)
        }) {
            best = Some((valid_count, exact_count, choice, valid));
        }
    }
    let Some((valid_count, _, choice, valid)) = best else {
        return Err("no sounding surface-field combination is available".to_owned());
    };
    if valid_count == 0 {
        return Err("no grid cell in the box has a complete sounding surface state".to_owned());
    }
    Ok((choice, valid))
}

fn mean_over_mask(values: &[f32], valid: &[bool]) -> f32 {
    let mut sum = 0.0f64;
    let mut count = 0usize;
    for (&value, &keep) in values.iter().zip(valid) {
        if keep && value.is_finite() {
            sum += f64::from(value);
            count += 1;
        }
    }
    if count == 0 {
        f32::NAN
    } else {
        (sum / count as f64) as f32
    }
}

fn build_box_sounding(
    view: &StoreView,
    hour: HourKey,
    bounds: BoxBounds,
) -> Result<BoxSoundingResult, String> {
    let started = Instant::now();
    let grid = view
        .open_grid(&hour.model, &hour.run)
        .map_err(|error| format!("open model grid for box sounding: {error}"))?;
    let selected = select_cells(&grid, bounds);
    if selected.is_empty() {
        return Err(format!(
            "No model grid cells fall inside {}. Draw the box over the displayed model domain.",
            bounds.label()
        ));
    }
    let sampled = finite_extent_for_indices(&grid, selected.iter().map(|cell| cell.index))
        .ok_or_else(|| "selected model cells have no finite geographic extent".to_owned())?;
    let grid_extent = finite_extent(&grid)
        .ok_or_else(|| "model grid has no finite geographic coverage".to_owned())?;
    let clipped_to_grid = bounds.west < grid_extent.west
        || bounds.east > grid_extent.east
        || bounds.south < grid_extent.south
        || bounds.north > grid_extent.north;

    let reader = view
        .open_hour(&hour.model, &hour.run, hour.hour)
        .map_err(|error| format!("open model timestep for box sounding: {error}"))?;
    if reader.meta().exact_time() != hour.exact_time {
        return Err("selected model timestep changed while the box sounding loaded".to_owned());
    }
    if (reader.meta().nx, reader.meta().ny) != (grid.nx, grid.ny) {
        return Err(format!(
            "model timestep grid {}x{} does not match run grid {}x{}",
            reader.meta().nx,
            reader.meta().ny,
            grid.nx,
            grid.ny
        ));
    }

    for required in ["temperature_iso", "u_iso", "v_iso", "height_iso"] {
        if reader
            .variable(required)
            .is_none_or(|var| var.kind != "pressure3d")
        {
            return Err(format!(
                "{} lacks required box-sounding column '{required}'",
                hour
            ));
        }
    }
    let moisture_candidates: Vec<&str> = ["dewpoint_iso", "rh_iso"]
        .into_iter()
        .filter(|name| {
            reader
                .variable(name)
                .is_some_and(|var| var.kind == "pressure3d")
        })
        .collect();
    if moisture_candidates.is_empty() {
        return Err(format!(
            "{} lacks required box-sounding moisture column (dewpoint_iso or rh_iso)",
            hour
        ));
    }

    #[allow(clippy::type_complexity)]
    let mut best_profile: Option<(usize, usize, String, Vec<ProfileVar>, Vec<usize>)> = None;
    for moisture in moisture_candidates {
        let names = ["temperature_iso", moisture, "u_iso", "v_iso", "height_iso"];
        let levels = common_levels(&reader, &names)?;
        let retained_bytes = levels
            .len()
            .checked_mul(selected.len())
            .and_then(|count| count.checked_mul(names.len()))
            .and_then(|count| count.checked_mul(std::mem::size_of::<f32>()))
            .ok_or_else(|| "box-sounding working set overflows usize".to_owned())?;
        if retained_bytes > MAX_RETAINED_PROFILE_BYTES {
            return Err(format!(
                "Box selects {} grid cells; exact primitive-column averaging would retain {:.1} GiB. Draw a smaller box (limit {:.1} GiB).",
                selected.len(),
                retained_bytes as f64 / 1024.0_f64.powi(3),
                MAX_RETAINED_PROFILE_BYTES as f64 / 1024.0_f64.powi(3)
            ));
        }
        let matrices = names
            .iter()
            .map(|name| read_primitive_matrix(&reader, name, &levels, &selected))
            .collect::<Result<Vec<_>, _>>()?;
        let (vars, counts) = aggregate_profile_matrices(&matrices, selected.len())?;
        let usable_levels = counts.iter().filter(|&&count| count > 0).count();
        let total_cells: usize = counts.iter().sum();
        let moisture_priority = usize::from(moisture == "dewpoint_iso");
        if best_profile
            .as_ref()
            .is_none_or(|(best_levels, best_cells, best_name, _, _)| {
                (usable_levels, total_cells, moisture_priority)
                    > (
                        *best_levels,
                        *best_cells,
                        usize::from(best_name == "dewpoint_iso"),
                    )
            })
        {
            best_profile = Some((
                usable_levels,
                total_cells,
                moisture.to_owned(),
                vars,
                counts,
            ));
        }
    }
    let Some((usable_levels, _, moisture_name, vars, level_counts)) = best_profile else {
        return Err("no compatible box-sounding primitive profile was available".to_owned());
    };
    if usable_levels < 2 {
        return Err("box mean has fewer than two complete pressure levels".to_owned());
    }

    let surface_pairs = [
        ("temperature_2m", "approx_temperature_2m"),
        ("dewpoint_2m", "approx_dewpoint_2m"),
        ("u_10m", "approx_u_10m"),
        ("v_10m", "approx_v_10m"),
        ("surface_pressure", "approx_surface_pressure"),
    ];
    let mut concepts = Vec::with_capacity(surface_pairs.len());
    for (exact, approximate) in surface_pairs {
        let mut choices = Vec::new();
        if let Some(candidate) = read_surface_candidate(&reader, exact, false, &selected)? {
            choices.push(candidate);
        }
        if let Some(candidate) = read_surface_candidate(&reader, approximate, true, &selected)? {
            choices.push(candidate);
        }
        concepts.push(choices);
    }
    let (surface_choice, surface_valid) = best_surface_set(&concepts, selected.len())?;
    let surface_cells = surface_valid.iter().filter(|&&valid| valid).count();
    let used_approx_surface = concepts
        .iter()
        .zip(&surface_choice)
        .any(|(candidates, &choice)| candidates[choice].approximate);
    let mut surface: Vec<SurfaceSample> = concepts
        .iter()
        .zip(&surface_choice)
        .map(|(candidates, &choice)| {
            let candidate = &candidates[choice];
            SurfaceSample {
                name: candidate.name.clone(),
                units: candidate.units.clone(),
                value: mean_over_mask(&candidate.values, &surface_valid),
            }
        })
        .collect();
    for optional in ["orography", "mslp"] {
        if let Some(candidate) = read_surface_candidate(&reader, optional, false, &selected)? {
            surface.push(SurfaceSample {
                name: candidate.name,
                units: candidate.units,
                value: mean_over_mask(&candidate.values, &surface_valid),
            });
        }
    }

    let center_x = selected.iter().map(|cell| cell.x as f64).sum::<f64>() / selected.len() as f64;
    let center_y = selected.iter().map(|cell| cell.y as f64).sum::<f64>() / selected.len() as f64;
    let center_lat = selected
        .iter()
        .map(|cell| f64::from(grid.lat[cell.index]))
        .sum::<f64>()
        / selected.len() as f64;
    let center_lon = selected
        .iter()
        .map(|cell| f64::from(grid.lon[cell.index]))
        .sum::<f64>()
        / selected.len() as f64;
    let read_ms = started.elapsed().as_secs_f32() * 1000.0;
    let data = SoundingData {
        hour: hour.clone(),
        fx: center_x,
        fy: center_y,
        lat: Some(center_lat as f32),
        lon: Some(center_lon as f32),
        vars,
        surface,
        read_ms,
    };
    // This derives only the representative display column, not parcel or
    // diagnostic grids. Fail here with the same actionable contract the
    // existing sounding viewer uses instead of opening a broken pane.
    rw_ui::skewt::build_sounding_column(&data)
        .map_err(|error| format!("box mean cannot build a sounding: {error}"))?;

    let min_level_cells = level_counts
        .iter()
        .copied()
        .filter(|&count| count > 0)
        .min()
        .unwrap_or(0);
    let max_level_cells = level_counts.iter().copied().max().unwrap_or(0);
    Ok(BoxSoundingResult {
        data,
        summary: BoxSoundingSummary {
            hour,
            requested: bounds,
            sampled,
            selected_cells: selected.len(),
            surface_cells,
            min_level_cells,
            max_level_cells,
            usable_levels,
            clipped_to_grid,
            moisture_name,
            used_approx_surface,
            read_ms,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matrix(name: &str, values: &[f32]) -> PrimitiveMatrix {
        PrimitiveMatrix {
            name: name.to_owned(),
            units: "unit".to_owned(),
            levels: vec![1000, 900],
            values: values.to_vec(),
        }
    }

    #[test]
    fn primitive_means_use_one_common_finite_mask_per_level() {
        let matrices = vec![
            matrix("temperature_iso", &[10.0, 20.0, f32::NAN, 30.0, 50.0, 70.0]),
            matrix("dewpoint_iso", &[1.0, f32::NAN, 3.0, 10.0, 20.0, 30.0]),
            matrix("u_iso", &[2.0, 4.0, 6.0, 4.0, 8.0, 12.0]),
        ];
        let (vars, counts) = aggregate_profile_matrices(&matrices, 3).unwrap();
        assert_eq!(counts, vec![1, 3]);
        assert_eq!(vars[0].values, vec![10.0, 50.0]);
        assert_eq!(vars[1].values, vec![1.0, 20.0]);
        assert_eq!(vars[2].values, vec![2.0, 8.0]);
    }

    #[test]
    fn surface_choice_maximizes_complete_coverage_then_exact_fields() {
        let candidate = |name: &str, approximate: bool, values: &[f32]| SurfaceCandidate {
            name: name.to_owned(),
            units: "unit".to_owned(),
            approximate,
            values: values.to_vec(),
        };
        let concepts = vec![
            vec![
                candidate("temperature_2m", false, &[1.0, f32::NAN, 3.0]),
                candidate("approx_temperature_2m", true, &[1.0, 2.0, 3.0]),
            ],
            vec![candidate("dewpoint_2m", false, &[1.0, 2.0, 3.0])],
        ];
        let (choice, valid) = best_surface_set(&concepts, 3).unwrap();
        assert_eq!(choice, vec![1, 0]);
        assert_eq!(valid, vec![true, true, true]);
    }

    #[test]
    fn bounds_reject_empty_and_format_selection() {
        assert!(BoxBounds::new((-100.0, -99.0, 34.0, 35.0)).is_ok());
        assert!(BoxBounds::new((-99.0, -100.0, 34.0, 35.0)).is_err());
        assert_eq!(
            BoxBounds::new((-100.0, -99.0, 34.0, 35.0)).unwrap().label(),
            "34.00..35.00 N, -100.00..-99.00 E"
        );
    }

    #[test]
    fn geographic_selection_includes_only_cells_inside_the_drawn_box() {
        let grid = GridFile {
            nx: 3,
            ny: 2,
            lat: vec![35.0, 35.0, 35.0, 34.0, 34.0, f32::NAN],
            lon: vec![-101.0, -100.0, -99.0, -101.0, -100.0, -99.0],
            projection: None,
            hash: "test".to_owned(),
        };
        let bounds = BoxBounds::new((-100.5, -98.5, 33.5, 35.5)).unwrap();
        let cells = select_cells(&grid, bounds);
        assert_eq!(
            cells.iter().map(|cell| cell.index).collect::<Vec<_>>(),
            vec![1, 2, 4]
        );
    }
}
