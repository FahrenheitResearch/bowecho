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

const MIN_BIN_DEG: f32 = 0.03;
const HOLE_FILL_PASSES: usize = 3;

impl InverseLut {
    /// Build from the grid's lat/lon arrays (~a second for CONUS HRRR;
    /// run on a background thread).
    pub fn build(lat: &[f32], lon: &[f32]) -> Option<Self> {
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
        steps.sort_by(f32::total_cmp);
        let spacing = steps.get(steps.len() / 2).copied().unwrap_or(MIN_BIN_DEG);
        let bin = (spacing * 1.1).max(MIN_BIN_DEG);
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
    /// Bumped when field/LUT changes — keys the rendered texture.
    pub generation: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
