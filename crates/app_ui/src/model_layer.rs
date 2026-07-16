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
/// Per-axis ceiling for the inverse index. The allocation has always been
/// capped to this size, but the bin spacing must honor the same ceiling: if
/// `width.min(MAX_LUT_AXIS_BINS)` truncates a long, narrow domain without
/// increasing `bin`, every longitude beyond the retained western bins becomes
/// unaddressable. Reserve one bin below the ceiling to absorb f32 rounding at
/// the maximum coordinate.
const MAX_LUT_AXIS_BINS: usize = 8192;
const HOLE_FILL_PASSES: usize = 3;

impl InverseLut {
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(self.index.len().saturating_mul(std::mem::size_of::<u32>()))
    }

    /// Build from the grid's lat/lon arrays (~a second for CONUS HRRR;
    /// run on a background thread).
    pub fn build(lat: &[f32], lon: &[f32]) -> Option<Self> {
        Self::build_inner(lat, lon, None, false)
    }

    /// Build from a shaped lat/lon grid. Satellite grids can be strongly
    /// curvilinear, so estimate spacing from real row/column neighbors
    /// instead of only 1D consecutive samples.
    pub fn build_with_shape(lat: &[f32], lon: &[f32], nx: usize, ny: usize) -> Option<Self> {
        if nx == 0 || ny == 0 || nx.saturating_mul(ny) != lat.len() || lat.len() != lon.len() {
            return Self::build(lat, lon);
        }
        Self::build_inner(lat, lon, Some((nx, ny)), false)
    }

    /// Build from a shaped grid whose row/column perimeter IS the true data
    /// boundary (WRF and similar regional model domains). Identical to
    /// [`Self::build_with_shape`] except the hole-fill dilation is confined
    /// to bins inside that perimeter polygon, so a query point outside the
    /// curvilinear domain edge — but still inside the rectangular lat/lon
    /// bbox the bins cover — returns `None` instead of the nearest edge
    /// cell (the smeared ~3-bin ring on the synthetic radar's domain
    /// boundary). Seeded bins and interior dilation are untouched, so
    /// in-domain lookups resolve exactly as before.
    ///
    /// Satellite layers must keep [`Self::build_with_shape`]: a full-disk
    /// grid's valid data ends at the earth's limb (NaN regions INSIDE the
    /// grid), not at the grid perimeter. Misuse still degrades safely — any
    /// non-finite perimeter point disables the mask and this build is then
    /// bin-for-bin identical to `build_with_shape`.
    pub fn build_with_shape_domain_bounded(
        lat: &[f32],
        lon: &[f32],
        nx: usize,
        ny: usize,
    ) -> Option<Self> {
        if nx == 0 || ny == 0 || nx.saturating_mul(ny) != lat.len() || lat.len() != lon.len() {
            return Self::build(lat, lon);
        }
        Self::build_inner(lat, lon, Some((nx, ny)), true)
    }

    fn build_inner(
        lat: &[f32],
        lon: &[f32],
        shape: Option<(usize, usize)>,
        bound_to_perimeter: bool,
    ) -> Option<Self> {
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
        let lat_span = lat_max - lat_min;
        let lon_span = lon_max - lon_min;
        let budget_floor = ((lat_span * lon_span) / MAX_LUT_BINS).sqrt();
        let axis_floor = lat_span.max(lon_span) / (MAX_LUT_AXIS_BINS - 2) as f32;
        let bin = (spacing * if shape.is_some() { 1.25 } else { 1.1 })
            .max(budget_floor)
            .max(axis_floor)
            .max(MIN_BIN_DEG);
        let width = ((lon_span / bin).ceil() as usize + 1).min(MAX_LUT_AXIS_BINS);
        let height = ((lat_span / bin).ceil() as usize + 1).min(MAX_LUT_AXIS_BINS);
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
        // True-domain mask (opt-in, WRF/model path): confine the dilation
        // below to bins inside the grid's perimeter polygon, so bins between
        // the curvilinear domain edge and the rectangular bbox stay empty
        // (lookup → None) instead of inheriting the nearest edge cell.
        let domain_mask = if bound_to_perimeter {
            shape.and_then(|(nx, ny)| {
                perimeter_bin_mask(lat, lon, nx, ny, lat_min, lon_min, bin, width, height)
            })
        } else {
            None
        };
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
                    if let Some(mask) = domain_mask.as_deref()
                        && !mask[by * width + bx]
                    {
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

/// The grid's outer boundary as a closed (lon, lat) ring — south row W→E,
/// east column S→N, north row E→W, west column N→S in grid order. `None`
/// when the grid is degenerate or ANY perimeter point is non-finite (a
/// full-disk satellite grid, whose valid data ends at the earth's limb, not
/// at the grid perimeter) — callers then skip domain masking entirely.
fn perimeter_ring(lat: &[f32], lon: &[f32], nx: usize, ny: usize) -> Option<Vec<(f64, f64)>> {
    if nx < 2 || ny < 2 {
        return None;
    }
    let mut ids: Vec<usize> = Vec::with_capacity(2 * (nx + ny));
    ids.extend(0..nx);
    ids.extend((1..ny).map(|y| y * nx + (nx - 1)));
    ids.extend((0..nx - 1).rev().map(|x| (ny - 1) * nx + x));
    ids.extend((1..ny - 1).rev().map(|y| y * nx));
    let mut ring = Vec::with_capacity(ids.len() + 1);
    for id in ids {
        let (la, lo) = (lat[id], lon[id]);
        if !la.is_finite() || !lo.is_finite() {
            return None;
        }
        ring.push((f64::from(lo), f64::from(la)));
    }
    let first = ring[0];
    ring.push(first);
    Some(ring)
}

/// Rasterize the grid's perimeter polygon into the bin grid: `true` for
/// bins whose CENTER lies inside the polygon (even-odd scanline fill) plus
/// every bin the boundary itself passes through (edges sampled at half-bin
/// steps), so bins straddling the boundary stay fillable and the in-domain
/// side never shrinks. O(bin rows × perimeter edges) — a few ms for the
/// largest WRF domains. `None` disables masking (see [`perimeter_ring`]).
#[allow(clippy::too_many_arguments)]
fn perimeter_bin_mask(
    lat: &[f32],
    lon: &[f32],
    nx: usize,
    ny: usize,
    lat_min: f32,
    lon_min: f32,
    bin: f32,
    width: usize,
    height: usize,
) -> Option<Vec<bool>> {
    let ring = perimeter_ring(lat, lon, nx, ny)?;
    let (lat0, lon0, bin) = (f64::from(lat_min), f64::from(lon_min), f64::from(bin));
    if !bin.is_finite() || bin <= 0.0 || width == 0 || height == 0 {
        return None;
    }
    let mut mask = vec![false; width * height];

    // Even-odd scanline fill at bin-center latitudes. The strict `>` on
    // both edge endpoints keeps crossing counts even when a scanline grazes
    // a vertex.
    let mut crossings: Vec<f64> = Vec::new();
    for by in 0..height {
        let y = lat0 + (by as f64 + 0.5) * bin;
        crossings.clear();
        for edge in ring.windows(2) {
            let ((x0, y0), (x1, y1)) = (edge[0], edge[1]);
            if (y0 > y) != (y1 > y) {
                crossings.push(x0 + (y - y0) * (x1 - x0) / (y1 - y0));
            }
        }
        crossings.sort_by(f64::total_cmp);
        for pair in crossings.chunks_exact(2) {
            let lo = ((pair[0] - lon0) / bin - 0.5).ceil().max(0.0) as usize;
            let hi = ((pair[1] - lon0) / bin - 0.5).floor();
            if hi < 0.0 || lo >= width {
                continue;
            }
            let hi = (hi as usize).min(width - 1);
            for bx in lo..=hi {
                mask[by * width + bx] = true;
            }
        }
    }

    // Boundary bins: walk each edge at half-bin steps so a bin the domain
    // edge merely clips still counts as in-domain.
    for edge in ring.windows(2) {
        let ((x0, y0), (x1, y1)) = (edge[0], edge[1]);
        let steps = (((x1 - x0).abs().max((y1 - y0).abs()) / bin * 2.0).ceil() as usize).max(1);
        for step in 0..=steps {
            let t = step as f64 / steps as f64;
            let bx = (x0 + t * (x1 - x0) - lon0) / bin;
            let by = (y0 + t * (y1 - y0) - lat0) / bin;
            if bx >= 0.0 && by >= 0.0 && (bx as usize) < width && (by as usize) < height {
                mask[by as usize * width + bx as usize] = true;
            }
        }
    }
    Some(mask)
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
    /// generic ramp. Ranks below an explicit `custom_color_family` override,
    /// and is left `None` when the 🎨 color-table editor binds a user palette
    /// to the product — the user's table arrives compiled into `production`,
    /// which must then win (user override → Solar → production → generic).
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
    fn cached_antimeridian_satellite_grid_keeps_both_longitude_sides_when_env_is_set() {
        let Some(path) = std::env::var_os("BOWECHO_SAT_ANTIMERIDIAN_GRID") else {
            return;
        };
        let grid =
            GridFile::open(std::path::Path::new(&path)).expect("cached satellite grid opens");
        eprintln!(
            "cached sat grid {}x{}, lat descending {:?}, finite {} / {}",
            grid.nx,
            grid.ny,
            grid.lat_descending(),
            grid.lat
                .iter()
                .zip(&grid.lon)
                .filter(|(lat, lon)| lat.is_finite() && lon.is_finite())
                .count(),
            grid.lat.len()
        );
        let lut = InverseLut::build_with_shape(&grid.lat, &grid.lon, grid.nx, grid.ny)
            .expect("cached satellite LUT builds");
        let positive_lon_point = grid
            .lat
            .iter()
            .zip(&grid.lon)
            .find(|(lat, lon)| lat.is_finite() && lon.is_finite() && **lon > 150.0)
            .map(|(&lat, &lon)| (lat, lon))
            .expect("cached GOES-West grid crosses onto positive longitudes");
        eprintln!(
            "LUT {}x{} ({} MiB), positive-longitude point {positive_lon_point:?} -> {:?}, California {:?}",
            lut.width,
            lut.height,
            lut.retained_bytes() / (1024 * 1024),
            lut.lookup(positive_lon_point.0, positive_lon_point.1),
            lut.lookup(36.7, -119.4),
        );
        assert!(
            lut.lookup(positive_lon_point.0, positive_lon_point.1)
                .is_some(),
            "the per-axis cap must not truncate the positive-longitude side"
        );
        assert!(lut.lookup(36.7, -119.4).is_some());
    }

    #[test]
    fn axis_limited_lut_keeps_the_far_edge_of_a_long_narrow_grid() {
        // Regression fixture for GOES CONUS: native-spaced bins can require
        // more than 8192 longitude bins while the short latitude axis keeps
        // the total comfortably below MAX_LUT_BINS. Capping only `width`
        // used to discard the far end of the domain.
        let (nx, ny) = (12_001usize, 11usize);
        let mut lat = Vec::with_capacity(nx * ny);
        let mut lon = Vec::with_capacity(nx * ny);
        for y in 0..ny {
            for x in 0..nx {
                lat.push(y as f32 * 0.01);
                lon.push(-160.0 + x as f32 * 0.01);
            }
        }

        let lut = InverseLut::build_with_shape(&lat, &lon, nx, ny).expect("panoramic LUT");

        assert!(lut.width <= MAX_LUT_AXIS_BINS);
        for lon in [-159.0, -100.0, -41.0] {
            assert!(
                lut.lookup(0.05, lon).is_some(),
                "longitude {lon} must remain addressable"
            );
        }
    }

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

    /// A rotated (curvilinear) n×n grid centred on (39, -95): its true edge
    /// is NOT the lat/lon bbox, so bbox corners/edges expose the off-domain
    /// leak the domain-bounded build fixes.
    fn rotated_grid(n: usize, spacing: f32, theta_deg: f32) -> (Vec<f32>, Vec<f32>) {
        let c = (n as f32 - 1.0) / 2.0;
        let (sin_t, cos_t) = theta_deg.to_radians().sin_cos();
        let mut lat = Vec::with_capacity(n * n);
        let mut lon = Vec::with_capacity(n * n);
        for j in 0..n {
            for i in 0..n {
                let x = (i as f32 - c) * spacing;
                let y = (j as f32 - c) * spacing;
                lon.push(-95.0 + x * cos_t - y * sin_t);
                lat.push(39.0 + x * sin_t + y * cos_t);
            }
        }
        (lat, lon)
    }

    /// The WRF/model-path fix: a query outside the true curvilinear domain
    /// edge but inside the rectangular bbox must return `None` from the
    /// domain-bounded build (the plain build leaks the nearest edge cell
    /// there — the smeared boundary ring), while interior lookups resolve
    /// identically in both builds.
    #[test]
    fn domain_bounded_lut_rejects_bbox_points_outside_the_curvilinear_edge() {
        let n = 41usize;
        let spacing = 0.02f32;
        let theta = 30.0f32;
        let (lat, lon) = rotated_grid(n, spacing, theta);
        let plain = InverseLut::build_with_shape(&lat, &lon, n, n).expect("plain lut");
        let bounded =
            InverseLut::build_with_shape_domain_bounded(&lat, &lon, n, n).expect("bounded lut");

        let (sin_t, cos_t) = theta.to_radians().sin_cos();
        let to_lat_lon =
            |x: f32, y: f32| (39.0 + x * sin_t + y * cos_t, -95.0 + x * cos_t - y * sin_t);

        // Interior queries: identical resolution in both builds.
        for &x in &[-0.3f32, -0.15, 0.0, 0.15, 0.3] {
            for &y in &[-0.3f32, -0.15, 0.0, 0.15, 0.3] {
                let (qlat, qlon) = to_lat_lon(x, y);
                let inside = bounded.lookup(qlat, qlon);
                assert!(inside.is_some(), "interior ({x}, {y}) must resolve");
                assert_eq!(
                    inside,
                    plain.lookup(qlat, qlon),
                    "interior ({x}, {y}) must be unchanged by the mask"
                );
            }
        }

        // Just inside each edge still resolves.
        let half = (n as f32 - 1.0) / 2.0 * spacing; // 0.4°
        for (x, y) in [(half, 0.0), (-half, 0.0), (0.0, half), (0.0, -half)] {
            let pull = 0.5 * spacing;
            let (qlat, qlon) = to_lat_lon(x - x.signum() * pull, y - y.signum() * pull);
            assert!(
                bounded.lookup(qlat, qlon).is_some(),
                "just inside edge ({x}, {y}) must still resolve"
            );
        }

        // Two bins OUTSIDE each side midpoint (inside the bbox): the old
        // dilation leaked the nearest edge cell there; bounded says None.
        let bin = spacing * theta.to_radians().cos() * 1.25; // shaped p75 step × 1.25
        let out = half + 2.0 * bin;
        for (x, y) in [(out, 0.0), (-out, 0.0), (0.0, out), (0.0, -out)] {
            let (qlat, qlon) = to_lat_lon(x, y);
            assert!(
                plain.lookup(qlat, qlon).is_some(),
                "plain build leaks an edge value at ({x}, {y}) — the bug this fixes"
            );
            assert!(
                bounded.lookup(qlat, qlon).is_none(),
                "bounded build must reject off-domain ({x}, {y})"
            );
        }
    }

    /// The satellite constraint: a full-disk-style grid keeps its NaN
    /// margins INSIDE the grid, so its perimeter is non-finite and the
    /// domain mask must disable itself — the bounded build degrades to a
    /// bin-for-bin copy of `build_with_shape`, including the limb dilation
    /// satellite rendering relies on. (The satellite path itself still
    /// calls `build_with_shape`; this proves even misuse cannot regress it.)
    #[test]
    fn domain_bounded_lut_is_inert_for_satellite_style_nan_margins() {
        // A full-disk-style grid: valid lat/lon only inside a DISK (the
        // earth), NaN in the corners (off-earth) — so the valid-data
        // boundary is the limb, not the grid's row/col perimeter.
        let n = 60i32;
        let (center, radius) = (30i32, 25i32);
        let mut lat = vec![f32::NAN; (n * n) as usize];
        let mut lon = vec![f32::NAN; (n * n) as usize];
        for j in 0..n {
            for i in 0..n {
                let (di, dj) = (i - center, j - center);
                if di * di + dj * dj <= radius * radius {
                    lat[(j * n + i) as usize] = 20.0 + dj as f32 * 0.1;
                    lon[(j * n + i) as usize] = 130.0 + di as f32 * 0.1;
                }
            }
        }
        let (nx, ny) = (n as usize, n as usize);
        let plain = InverseLut::build_with_shape(&lat, &lon, nx, ny).expect("plain lut");
        let bounded = InverseLut::build_with_shape_domain_bounded(&lat, &lon, nx, ny)
            .expect("bounded lut on satellite-style grid");
        assert_eq!(bounded.width, plain.width);
        assert_eq!(bounded.height, plain.height);
        assert_eq!(
            bounded.index, plain.index,
            "non-finite perimeter must disable the mask — identical bins"
        );

        // The dilation just past the limb (what keeps the satellite edge
        // render seamless) works identically in both builds…
        let just_off_limb = plain.lookup(21.874, 131.874); // r ≈ 2.65° > 2.5° disk
        assert!(
            just_off_limb.is_some(),
            "limb dilation must keep filling just past the valid-data edge"
        );
        assert_eq!(bounded.lookup(21.874, 131.874), just_off_limb);
        // …and the deep off-earth bbox corner stays empty in both.
        assert!(plain.lookup(22.4, 132.4).is_none());
        assert!(bounded.lookup(22.4, 132.4).is_none());
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
                    exact_time: None,
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
