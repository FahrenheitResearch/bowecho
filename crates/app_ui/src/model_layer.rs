//! Model field as a MAP LAYER: an NWP field from the rusty-weather store
//! rendered under the radar in BowEcho's AEQD view.
//!
//! The store grid carries per-point lat/lon arrays, so no projection math
//! is needed: an inverse lookup table (lat/lon bins → grid index) is built
//! once per grid on a background thread, after which screen rendering is
//! O(1) per pixel. The layer raster renders at half resolution on a
//! background thread (model fields are smooth — texture filtering upscales
//! invisibly) and caches per quantized viewport, so panning stays on the
//! fast path.

use rustwx_render::LeveledColormap;
use rw_ui::FieldData;
use rw_ui::colormap::Colormap;
use std::sync::Arc;

use color_tables::{ColorTable, ColorTableFamily};

/// Inverse geolocation: lat/lon bin → row-major grid index.
pub struct InverseLut {
    lat0: f32,
    lon0: f32,
    inv_dlat: f32,
    inv_dlon: f32,
    width: usize,
    height: usize,
    index: Vec<u32>,
}

/// Hard floor on the bin size — degenerate-data guard only (~11 m). The real
/// lower bound comes from [`MAX_LUT_BINS`]; a fixed coarse floor here is what
/// quantized 250 m WRF grids into ~3.3 km blocks (the old 0.03° floor made
/// every gate in a bucket resolve to ONE model cell, and the sampler's local
/// 3x3 stencil search could never reach the true cell from a seed ~7 cells
/// away — giant constant wedges on the synthetic radar).
const MIN_BIN_DEG: f32 = 0.0001;
/// Total-bin budget for the inverse index (u32 each, ~64 MB worst case).
/// Small fine grids (250 m WRF: ~0.7 M bins) index at native spacing; huge
/// domains (full-disk satellite) are budget-limited to roughly the same bin
/// size the old fixed 0.03° floor produced, so their memory profile is
/// unchanged.
const MAX_LUT_BINS: f32 = 16_000_000.0;
const HOLE_FILL_PASSES: usize = 3;

impl InverseLut {
    /// Build from the grid's lat/lon arrays (~a second for CONUS HRRR;
    /// run on a background thread).
    pub fn build(lat: &[f32], lon: &[f32]) -> Option<Self> {
        Self::build_inner(lat, lon, None)
    }

    /// Build from a shaped lat/lon grid. Satellite grids can be strongly
    /// curvilinear, so estimate spacing from real row/column neighbors
    /// instead of only 1D consecutive samples.
    pub fn build_with_shape(lat: &[f32], lon: &[f32], nx: usize, ny: usize) -> Option<Self> {
        if nx == 0 || ny == 0 || nx.saturating_mul(ny) != lat.len() || lat.len() != lon.len() {
            return Self::build(lat, lon);
        }
        Self::build_inner(lat, lon, Some((nx, ny)))
    }

    fn build_inner(lat: &[f32], lon: &[f32], shape: Option<(usize, usize)>) -> Option<Self> {
        let mut lat_min = f32::INFINITY;
        let mut lat_max = f32::NEG_INFINITY;
        let mut lon_min = f32::INFINITY;
        let mut lon_max = f32::NEG_INFINITY;
        for (&la, &lo) in lat.iter().zip(lon.iter()) {
            if la.is_finite() && lo.is_finite() {
                lat_min = lat_min.min(la);
                lat_max = lat_max.max(la);
                lon_min = lon_min.min(lo);
                lon_max = lon_max.max(lo);
            }
        }
        if !lat_min.is_finite() || lat_max <= lat_min || lon_max <= lon_min {
            return None;
        }
        // Bin size adapts to the grid's spacing (HRRR ~0.03°, GFS 0.25°):
        // bins comparable to the spacing keep holes within one cell of a
        // sample, which the fill passes close. Median of non-degenerate
        // consecutive steps — row-wrap jumps (tens of degrees between the
        // end of one row and the start of the next) are filtered out.
        let spacing = shape
            .and_then(|(nx, ny)| shaped_grid_spacing(lat, lon, nx, ny))
            .unwrap_or_else(|| consecutive_spacing(lat, lon).unwrap_or(MIN_BIN_DEG));
        // Bin = the grid's own spacing, floored by the total-bin budget so a
        // huge domain cannot allocate an unbounded index. The budget floor —
        // not a fixed degree floor — is what lets a 250 m grid index at native
        // resolution while a full-disk satellite grid stays ~64 MB.
        let budget_floor = (((lat_max - lat_min) * (lon_max - lon_min)) / MAX_LUT_BINS).sqrt();
        let bin = (spacing * if shape.is_some() { 1.25 } else { 1.1 })
            .max(budget_floor)
            .max(MIN_BIN_DEG);
        let width = (((lon_max - lon_min) / bin).ceil() as usize + 1).min(8192);
        let height = (((lat_max - lat_min) / bin).ceil() as usize + 1).min(8192);
        let mut index = vec![u32::MAX; width * height];
        for (i, (&la, &lo)) in lat.iter().zip(lon.iter()).enumerate() {
            if !la.is_finite() || !lo.is_finite() {
                continue;
            }
            let bx = ((lo - lon_min) / bin) as usize;
            let by = ((la - lat_min) / bin) as usize;
            if bx < width && by < height {
                index[by * width + bx] = i as u32;
            }
        }
        // Hole fill: model grid spacing can exceed the bin size away from
        // the grid center; dilate a few passes so bins between grid points
        // resolve to a neighbor.
        for _ in 0..HOLE_FILL_PASSES {
            let snapshot = index.clone();
            for by in 0..height {
                for bx in 0..width {
                    if snapshot[by * width + bx] != u32::MAX {
                        continue;
                    }
                    let mut fill = u32::MAX;
                    for (dy, dx) in [(0i64, 1i64), (0, -1), (1, 0), (-1, 0)] {
                        let ny = by as i64 + dy;
                        let nx = bx as i64 + dx;
                        if ny < 0 || nx < 0 || ny >= height as i64 || nx >= width as i64 {
                            continue;
                        }
                        let v = snapshot[ny as usize * width + nx as usize];
                        if v != u32::MAX {
                            fill = v;
                            break;
                        }
                    }
                    if fill != u32::MAX {
                        index[by * width + bx] = fill;
                    }
                }
            }
        }
        Some(Self {
            lat0: lat_min,
            lon0: lon_min,
            inv_dlat: 1.0 / bin,
            inv_dlon: 1.0 / bin,
            width,
            height,
            index,
        })
    }

    /// Grid index for a lat/lon, or None outside the grid.
    #[inline]
    pub fn lookup(&self, lat: f32, lon: f32) -> Option<usize> {
        let bx = ((lon - self.lon0) * self.inv_dlon) as isize;
        let by = ((lat - self.lat0) * self.inv_dlat) as isize;
        if bx < 0 || by < 0 || bx as usize >= self.width || by as usize >= self.height {
            return None;
        }
        let v = self.index[by as usize * self.width + bx as usize];
        (v != u32::MAX).then_some(v as usize)
    }
}

fn consecutive_spacing(lat: &[f32], lon: &[f32]) -> Option<f32> {
    let mut steps: Vec<f32> = Vec::with_capacity(4096);
    for source in [lat, lon] {
        for pair in source.windows(2).take(4096) {
            if pair[0].is_finite() && pair[1].is_finite() {
                let step = (pair[1] - pair[0]).abs();
                if step > 1e-6 && step < 2.0 {
                    steps.push(step);
                }
            }
        }
    }
    percentile_step(steps, 0.5)
}

fn shaped_grid_spacing(lat: &[f32], lon: &[f32], nx: usize, ny: usize) -> Option<f32> {
    let mut steps: Vec<f32> = Vec::with_capacity(8192);
    let step_x = (nx / 96).max(1);
    let step_y = (ny / 96).max(1);
    for y in (0..ny).step_by(step_y) {
        for x in (0..nx.saturating_sub(1)).step_by(step_x) {
            push_neighbor_step(&mut steps, lat, lon, y * nx + x, y * nx + x + 1);
        }
    }
    for y in (0..ny.saturating_sub(1)).step_by(step_y) {
        for x in (0..nx).step_by(step_x) {
            push_neighbor_step(&mut steps, lat, lon, y * nx + x, (y + 1) * nx + x);
        }
    }
    percentile_step(steps, 0.75)
}

fn push_neighbor_step(steps: &mut Vec<f32>, lat: &[f32], lon: &[f32], a: usize, b: usize) {
    let (lat_a, lon_a, lat_b, lon_b) = (lat[a], lon[a], lat[b], lon[b]);
    if !lat_a.is_finite() || !lon_a.is_finite() || !lat_b.is_finite() || !lon_b.is_finite() {
        return;
    }
    let step = (lat_a - lat_b).abs().max(wrapped_lon_delta(lon_a, lon_b));
    if step > 1e-5 && step < 5.0 {
        steps.push(step);
    }
}

fn wrapped_lon_delta(a: f32, b: f32) -> f32 {
    let raw = (a - b).abs().rem_euclid(360.0);
    raw.min(360.0 - raw)
}

fn percentile_step(mut steps: Vec<f32>, percentile: f32) -> Option<f32> {
    if steps.is_empty() {
        return None;
    }
    steps.sort_by(f32::total_cmp);
    let index = ((steps.len() - 1) as f32 * percentile.clamp(0.0, 1.0)).round() as usize;
    steps.get(index).copied()
}

/// The active model map layer: a field + its inverse LUT + display params.
pub struct ModelMapLayer {
    pub field: Arc<FieldData>,
    pub lut: Arc<InverseLut>,
    /// PRODUCTION colortable (rusty-weather per-product styles) when the
    /// field ships one; the generic ramp is the fallback.
    pub production: Option<Arc<LeveledColormap>>,
    pub colormap: Colormap,
    pub opacity: f32,
    /// Hidden layers keep their data (inspector + soundings still read the
    /// store) but skip the map draw.
    pub visible: bool,
    /// Optional BowEcho color-table family override. When absent, model
    /// layers use Rusty Weather's production style, then generic ramp.
    pub custom_color_family: Option<ColorTableFamily>,
    /// Solarpower07 WRF-Runner palette resolved from the field's name + units
    /// (see [`color_tables::solar_model_field_table`]). For WRF/local model
    /// fields this is the layer's default look; for downloaded models it only
    /// fills in where Rusty Weather has no production style, replacing the
    /// generic ramp. Ranks below an explicit `custom_color_family` override.
    /// Credit: Solarpower07 (handle only, credit pending per project policy).
    pub model_table: Option<Arc<ColorTable>>,
    /// Bumped when field/LUT changes — keys the rendered texture.
    pub generation: u64,
}

/// Sample a model field at a screen-derived lat/lon. The LUT gives the
/// nearest grid point cheaply; when the run grid is present, refine that to a
/// fractional position inside a neighboring grid cell so map overlays render
/// as continuous model fields instead of nearest-neighbor blocks.
pub fn sample_field_value(
    field: &FieldData,
    nearest_index: usize,
    lat: f32,
    lon: f32,
) -> Option<f32> {
    if let Some(grid) = field.grid.as_ref()
        && grid.nx == field.nx
        && grid.ny == field.ny
        && grid.lat.len() == field.values.len()
        && grid.lon.len() == field.values.len()
        && let Some(value) = sample_curvilinear_field(field, grid, nearest_index, lat, lon)
    {
        return Some(value);
    }
    field
        .values
        .get(nearest_index)
        .copied()
        .filter(|value| value.is_finite())
}

fn sample_curvilinear_field(
    field: &FieldData,
    grid: &rw_store::grid::GridFile,
    nearest_index: usize,
    lat: f32,
    lon: f32,
) -> Option<f32> {
    if field.nx < 2 || field.ny < 2 || nearest_index >= field.values.len() {
        return None;
    }
    let row = nearest_index / field.nx;
    let col = nearest_index % field.nx;
    let row_starts = neighboring_cell_starts(row, field.ny);
    let col_starts = neighboring_cell_starts(col, field.nx);
    for y0 in row_starts.into_iter().flatten() {
        for x0 in col_starts.into_iter().flatten() {
            if let Some(value) = sample_cell(field, grid, x0, y0, lat, lon) {
                return Some(value);
            }
        }
    }
    None
}

pub(crate) fn neighboring_cell_starts(index: usize, len: usize) -> [Option<usize>; 2] {
    if len < 2 {
        return [None, None];
    }
    let first = index.saturating_sub(1).min(len - 2);
    let second = index.min(len - 2);
    if first == second {
        [Some(first), None]
    } else {
        [Some(first), Some(second)]
    }
}

fn sample_cell(
    field: &FieldData,
    grid: &rw_store::grid::GridFile,
    x0: usize,
    y0: usize,
    target_lat: f32,
    target_lon: f32,
) -> Option<f32> {
    let nx = field.nx;
    let i00 = y0 * nx + x0;
    let i10 = i00 + 1;
    let i01 = i00 + nx;
    let i11 = i01 + 1;
    let values = [
        *field.values.get(i00)?,
        *field.values.get(i10)?,
        *field.values.get(i01)?,
        *field.values.get(i11)?,
    ];
    if values.iter().any(|value| !value.is_finite()) {
        return None;
    }
    let target_lon = f64::from(target_lon);
    let target_lat = f64::from(target_lat);
    let corners = [
        (
            unwrap_lon_near(f64::from(*grid.lon.get(i00)?), target_lon),
            f64::from(*grid.lat.get(i00)?),
        ),
        (
            unwrap_lon_near(f64::from(*grid.lon.get(i10)?), target_lon),
            f64::from(*grid.lat.get(i10)?),
        ),
        (
            unwrap_lon_near(f64::from(*grid.lon.get(i01)?), target_lon),
            f64::from(*grid.lat.get(i01)?),
        ),
        (
            unwrap_lon_near(f64::from(*grid.lon.get(i11)?), target_lon),
            f64::from(*grid.lat.get(i11)?),
        ),
    ];
    let (u, v) = solve_bilinear_coords(corners, target_lon, target_lat)?;
    if !((-0.08..=1.08).contains(&u) && (-0.08..=1.08).contains(&v)) {
        return None;
    }
    let u = u.clamp(0.0, 1.0) as f32;
    let v = v.clamp(0.0, 1.0) as f32;
    let top = values[0] * (1.0 - u) + values[1] * u;
    let bottom = values[2] * (1.0 - u) + values[3] * u;
    Some(top * (1.0 - v) + bottom * v)
}

pub(crate) fn solve_bilinear_coords(
    corners: [(f64, f64); 4],
    target_x: f64,
    target_y: f64,
) -> Option<(f64, f64)> {
    let [(x00, y00), (x10, y10), (x01, y01), (x11, y11)] = corners;
    let mut u = 0.5;
    let mut v = 0.5;
    for _ in 0..8 {
        let one_u = 1.0 - u;
        let one_v = 1.0 - v;
        let x = one_u * one_v * x00 + u * one_v * x10 + one_u * v * x01 + u * v * x11;
        let y = one_u * one_v * y00 + u * one_v * y10 + one_u * v * y01 + u * v * y11;
        let rx = target_x - x;
        let ry = target_y - y;
        if rx.abs().max(ry.abs()) < 1e-6 {
            return Some((u, v));
        }
        let dx_du = -one_v * x00 + one_v * x10 - v * x01 + v * x11;
        let dx_dv = -one_u * x00 - u * x10 + one_u * x01 + u * x11;
        let dy_du = -one_v * y00 + one_v * y10 - v * y01 + v * y11;
        let dy_dv = -one_u * y00 - u * y10 + one_u * y01 + u * y11;
        let det = dx_du * dy_dv - dx_dv * dy_du;
        if det.abs() < 1e-12 {
            return None;
        }
        let du = (rx * dy_dv - dx_dv * ry) / det;
        let dv = (dx_du * ry - rx * dy_du) / det;
        u += du;
        v += dv;
        if !u.is_finite() || !v.is_finite() || u.abs().max(v.abs()) > 3.0 {
            return None;
        }
    }
    Some((u, v))
}

pub(crate) fn unwrap_lon_near(mut lon: f64, target: f64) -> f64 {
    while lon - target > 180.0 {
        lon -= 360.0;
    }
    while lon - target < -180.0 {
        lon += 360.0;
    }
    lon
}

#[cfg(test)]
mod tests {
    use super::*;
    use rw_store::grid::GridFile;

    #[test]
    fn lut_round_trips_a_regular_grid() {
        // 50x40 regular grid over (30..40N, -100..-90E).
        let (nx, ny) = (50usize, 40usize);
        let mut lat = Vec::with_capacity(nx * ny);
        let mut lon = Vec::with_capacity(nx * ny);
        for j in 0..ny {
            for i in 0..nx {
                lat.push(30.0 + 10.0 * j as f32 / (ny - 1) as f32);
                lon.push(-100.0 + 10.0 * i as f32 / (nx - 1) as f32);
            }
        }
        let lut = InverseLut::build(&lat, &lon).expect("lut");
        // Interior points resolve to a grid index whose lat/lon is close.
        for &(qlat, qlon) in &[(32.0f32, -97.5f32), (38.7, -91.2), (30.4, -99.6)] {
            let index = lut.lookup(qlat, qlon).expect("inside grid");
            assert!((lat[index] - qlat).abs() < 0.5, "{} vs {qlat}", lat[index]);
            assert!((lon[index] - qlon).abs() < 0.5, "{} vs {qlon}", lon[index]);
        }
        // Far outside: None.
        assert!(lut.lookup(50.0, -80.0).is_none());
    }

    #[test]
    fn shaped_lut_uses_row_column_spacing_for_curvilinear_grids() {
        let (nx, ny) = (120usize, 80usize);
        let mut lat = Vec::with_capacity(nx * ny);
        let mut lon = Vec::with_capacity(nx * ny);
        for y in 0..ny {
            for x in 0..nx {
                let yf = y as f32;
                let xf = x as f32;
                lat.push(5.0 + yf * 0.16 + (xf * 0.05).sin() * 0.01);
                lon.push(55.0 + xf * 0.18 + yf * 0.015);
            }
        }

        let spacing = shaped_grid_spacing(&lat, &lon, nx, ny).expect("spacing");
        assert!(
            spacing > 0.12,
            "shape-aware spacing should follow grid cells, got {spacing}"
        );

        let lut = InverseLut::build_with_shape(&lat, &lon, nx, ny).expect("lut");
        let midpoint = |a: usize, b: usize, c: usize, d: usize, values: &[f32]| {
            (values[a] + values[b] + values[c] + values[d]) * 0.25
        };
        let x = 47;
        let y = 31;
        let i = y * nx + x;
        let query_lat = midpoint(i, i + 1, i + nx, i + nx + 1, &lat);
        let query_lon = midpoint(i, i + 1, i + nx, i + nx + 1, &lon);
        assert!(lut.lookup(query_lat, query_lon).is_some());
    }

    #[test]
    fn model_field_sampler_interpolates_between_grid_cells() {
        let grid = Arc::new(GridFile {
            nx: 2,
            ny: 2,
            lat: vec![40.0, 40.0, 41.0, 41.0],
            lon: vec![-100.0, -99.0, -100.0, -99.0],
            projection: None,
            hash: "test".to_owned(),
        });
        let field = FieldData {
            key: rw_ui::FieldKey {
                hour: rw_ui::HourKey {
                    model: "gfs".to_owned(),
                    run: "20260626_00z".to_owned(),
                    hour: 0,
                },
                var: "temperature_2m".to_owned(),
            },
            units: "degF".to_owned(),
            nx: 2,
            ny: 2,
            values: vec![10.0, 20.0, 30.0, 40.0],
            range: Some((10.0, 40.0)),
            grid: Some(grid),
            lat_descending: false,
            style: None,
        };

        let value = sample_field_value(&field, 0, 40.5, -99.5).expect("interpolated value");
        assert!(
            (value - 25.0).abs() < 1e-3,
            "expected bilinear midpoint, got {value}"
        );
    }
}
