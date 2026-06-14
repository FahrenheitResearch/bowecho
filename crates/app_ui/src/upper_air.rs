use std::path::PathBuf;
use std::sync::Arc;

use rw_store::grid::GridFile;
use rw_ui::{HourKey, StoreView};

pub const QUICKLOOK_LEVELS_HPA: [u16; 3] = [500, 700, 850];
const HEIGHT_CONTOUR_M: f32 = 20.0;
const HEIGHT_MAJOR_M: f32 = 60.0;
const TEMP_CONTOUR_C: f32 = 2.0;

#[derive(Clone, Debug)]
pub struct UpperAirLayer {
    pub hour: HourKey,
    pub level_hpa: u16,
    pub nx: usize,
    pub ny: usize,
    pub grid: Arc<GridFile>,
    pub u_ms: Vec<f32>,
    pub v_ms: Vec<f32>,
    pub height_contours: Vec<ContourSegment>,
    pub temp_contours: Vec<ContourSegment>,
    pub wind_step: usize,
    pub visible: bool,
    pub opacity: f32,
    pub summary: String,
}

impl UpperAirLayer {
    pub fn short_label(&self) -> String {
        format!(
            "{} {} F{:03} {} mb",
            self.hour.model.to_uppercase(),
            self.hour.run,
            self.hour.hour,
            self.level_hpa
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ContourPoint {
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ContourSegment {
    pub level: f32,
    pub a: ContourPoint,
    pub b: ContourPoint,
    pub major: bool,
}

pub fn build_layer(
    store_root: PathBuf,
    hour: HourKey,
    level_hpa: u16,
) -> Result<UpperAirLayer, String> {
    let store = StoreView::new(store_root);
    let reader = store
        .open_hour(&hour.model, &hour.run, hour.hour)
        .map_err(|err| format!("open {} {} F{:03}: {err}", hour.model, hour.run, hour.hour))?;
    let grid = Arc::new(
        store
            .open_grid(&hour.model, &hour.run)
            .map_err(|err| format!("open grid for {} {}: {err}", hour.model, hour.run))?,
    );
    let (nx, ny) = (reader.meta().nx, reader.meta().ny);
    if grid.nx != nx || grid.ny != ny {
        return Err(format!(
            "grid mismatch: hour is {nx}x{ny}, grid is {}x{}",
            grid.nx, grid.ny
        ));
    }

    let height_meta = reader
        .variable("height_iso")
        .ok_or_else(|| "height_iso metadata missing".to_owned())?;
    let temp_meta = reader
        .variable("temperature_iso")
        .ok_or_else(|| "temperature_iso metadata missing".to_owned())?;
    let levels = if height_meta.levels_hpa.contains(&level_hpa) {
        &height_meta.levels_hpa
    } else {
        return Err(format!(
            "{} mb not present in height_iso levels {:?}",
            level_hpa, height_meta.levels_hpa
        ));
    };

    let height_m = extract_level_plane(
        &reader
            .read_full_3d("height_iso")
            .map_err(|err| format!("height_iso: {err}"))?,
        levels,
        level_hpa,
        nx,
        ny,
    )?;
    let mut temp_c = extract_level_plane(
        &reader
            .read_full_3d("temperature_iso")
            .map_err(|err| format!("temperature_iso: {err}"))?,
        &temp_meta.levels_hpa,
        level_hpa,
        nx,
        ny,
    )?;
    if temp_meta.units.eq_ignore_ascii_case("K") {
        for value in &mut temp_c {
            if value.is_finite() {
                *value -= 273.15;
            }
        }
    }
    let u_ms = extract_level_plane(
        &reader
            .read_full_3d("u_iso")
            .map_err(|err| format!("u_iso: {err}"))?,
        &reader
            .variable("u_iso")
            .ok_or_else(|| "u_iso metadata missing".to_owned())?
            .levels_hpa,
        level_hpa,
        nx,
        ny,
    )?;
    let v_ms = extract_level_plane(
        &reader
            .read_full_3d("v_iso")
            .map_err(|err| format!("v_iso: {err}"))?,
        &reader
            .variable("v_iso")
            .ok_or_else(|| "v_iso metadata missing".to_owned())?
            .levels_hpa,
        level_hpa,
        nx,
        ny,
    )?;

    let height_contours =
        contour_segments(&height_m, nx, ny, HEIGHT_CONTOUR_M, Some(HEIGHT_MAJOR_M));
    let temp_contours = contour_segments(&temp_c, nx, ny, TEMP_CONTOUR_C, Some(10.0));
    let wind_step = wind_step_for_grid(&grid).max(1);
    let wind_count = ny.div_ceil(wind_step) * nx.div_ceil(wind_step);
    let summary = format!(
        "{} {} F{:03} {} mb upper-air: {} m height contours (bold every {} m), {} C temp contours, ~{} wind barbs",
        hour.model.to_uppercase(),
        hour.run,
        hour.hour,
        level_hpa,
        HEIGHT_CONTOUR_M as u16,
        HEIGHT_MAJOR_M as u16,
        TEMP_CONTOUR_C as u16,
        wind_count
    );

    Ok(UpperAirLayer {
        hour,
        level_hpa,
        nx,
        ny,
        grid,
        u_ms,
        v_ms,
        height_contours,
        temp_contours,
        wind_step,
        visible: true,
        opacity: 0.92,
        summary,
    })
}

pub fn grid_lon_lat(grid: &GridFile, x: f32, y: f32) -> Option<(f32, f32)> {
    if grid.nx == 0 || grid.ny == 0 || !x.is_finite() || !y.is_finite() {
        return None;
    }
    let max_x = (grid.nx - 1) as f32;
    let max_y = (grid.ny - 1) as f32;
    if x < 0.0 || y < 0.0 || x > max_x || y > max_y {
        return None;
    }
    let x0 = x.floor().min(max_x) as usize;
    let y0 = y.floor().min(max_y) as usize;
    let x1 = (x0 + 1).min(grid.nx - 1);
    let y1 = (y0 + 1).min(grid.ny - 1);
    let tx = x - x0 as f32;
    let ty = y - y0 as f32;
    let idx = |xx: usize, yy: usize| yy * grid.nx + xx;
    let bilinear = |values: &[f32]| {
        let v00 = values[idx(x0, y0)];
        let v10 = values[idx(x1, y0)];
        let v01 = values[idx(x0, y1)];
        let v11 = values[idx(x1, y1)];
        if !(v00.is_finite() && v10.is_finite() && v01.is_finite() && v11.is_finite()) {
            return None;
        }
        let top = v00 + (v10 - v00) * tx;
        let bottom = v01 + (v11 - v01) * tx;
        Some(top + (bottom - top) * ty)
    };
    Some((bilinear(&grid.lon)?, bilinear(&grid.lat)?))
}

pub(crate) fn extract_level_plane(
    values: &[f32],
    levels_hpa: &[u16],
    level_hpa: u16,
    nx: usize,
    ny: usize,
) -> Result<Vec<f32>, String> {
    let plane_len = nx
        .checked_mul(ny)
        .ok_or_else(|| format!("grid dimensions {nx}x{ny} overflow"))?;
    let expected = plane_len
        .checked_mul(levels_hpa.len())
        .ok_or_else(|| "3D level count overflows".to_owned())?;
    if values.len() != expected {
        return Err(format!(
            "3D volume length mismatch: got {}, expected {} for {} levels on {nx}x{ny}",
            values.len(),
            expected,
            levels_hpa.len()
        ));
    }
    let level_index = levels_hpa
        .iter()
        .position(|&level| level == level_hpa)
        .ok_or_else(|| format!("{} mb not present in levels {:?}", level_hpa, levels_hpa))?;
    let start = level_index * plane_len;
    Ok(values[start..start + plane_len].to_vec())
}

fn contour_segments(
    values: &[f32],
    nx: usize,
    ny: usize,
    interval: f32,
    major_interval: Option<f32>,
) -> Vec<ContourSegment> {
    if values.len() != nx * ny || nx < 2 || ny < 2 || interval <= 0.0 {
        return Vec::new();
    }
    let Some((min, max)) = finite_range(values) else {
        return Vec::new();
    };
    if (max - min).abs() < f32::EPSILON {
        return Vec::new();
    }
    let mut first = (min / interval).ceil() * interval;
    if first <= min + interval * 0.001 {
        first += interval;
    }
    let mut last = (max / interval).floor() * interval;
    if last >= max - interval * 0.001 {
        last -= interval;
    }
    if first > last {
        return Vec::new();
    }
    let mut segments = Vec::new();
    let mut level = first;
    while level <= last + interval * 0.25 {
        contour_level(values, nx, ny, level, major_interval, &mut segments);
        level += interval;
    }
    segments
}

fn contour_level(
    values: &[f32],
    nx: usize,
    ny: usize,
    level: f32,
    major_interval: Option<f32>,
    out: &mut Vec<ContourSegment>,
) {
    for y in 0..ny - 1 {
        for x in 0..nx - 1 {
            let idx = |xx: usize, yy: usize| yy * nx + xx;
            let v = [
                values[idx(x, y)],
                values[idx(x + 1, y)],
                values[idx(x + 1, y + 1)],
                values[idx(x, y + 1)],
            ];
            if !v.iter().all(|value| value.is_finite()) {
                continue;
            }
            let p = [
                ContourPoint {
                    x: x as f32,
                    y: y as f32,
                },
                ContourPoint {
                    x: (x + 1) as f32,
                    y: y as f32,
                },
                ContourPoint {
                    x: (x + 1) as f32,
                    y: (y + 1) as f32,
                },
                ContourPoint {
                    x: x as f32,
                    y: (y + 1) as f32,
                },
            ];
            let edges = [(0usize, 1usize), (1, 2), (2, 3), (3, 0)];
            let mut hits = Vec::with_capacity(4);
            for &(a, b) in &edges {
                if let Some(point) = edge_crossing(p[a], v[a], p[b], v[b], level) {
                    hits.push(point);
                }
            }
            let major = major_interval
                .map(|interval| {
                    let nearest = (level / interval).round() * interval;
                    (level - nearest).abs() <= interval * 0.01
                })
                .unwrap_or(false);
            match hits.as_slice() {
                [a, b] => out.push(ContourSegment {
                    level,
                    a: *a,
                    b: *b,
                    major,
                }),
                [a, b, c, d] => {
                    out.push(ContourSegment {
                        level,
                        a: *a,
                        b: *b,
                        major,
                    });
                    out.push(ContourSegment {
                        level,
                        a: *c,
                        b: *d,
                        major,
                    });
                }
                _ => {}
            }
        }
    }
}

fn edge_crossing(
    a: ContourPoint,
    va: f32,
    b: ContourPoint,
    vb: f32,
    level: f32,
) -> Option<ContourPoint> {
    if (va - vb).abs() < f32::EPSILON {
        return None;
    }
    let crosses = (va < level && vb >= level) || (vb < level && va >= level);
    if !crosses {
        return None;
    }
    let t = ((level - va) / (vb - va)).clamp(0.0, 1.0);
    Some(ContourPoint {
        x: a.x + (b.x - a.x) * t,
        y: a.y + (b.y - a.y) * t,
    })
}

fn finite_range(values: &[f32]) -> Option<(f32, f32)> {
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    for &value in values {
        if value.is_finite() {
            min = min.min(value);
            max = max.max(value);
        }
    }
    min.is_finite().then_some((min, max))
}

fn wind_step_for_grid(grid: &GridFile) -> usize {
    let approx_km = estimate_cell_km(grid).unwrap_or(13.0);
    (110.0 / approx_km).round().clamp(4.0, 18.0) as usize
}

fn estimate_cell_km(grid: &GridFile) -> Option<f32> {
    if grid.nx < 2 || grid.ny == 0 {
        return None;
    }
    let y = grid.ny / 2;
    for x in 0..grid.nx - 1 {
        let a = y * grid.nx + x;
        let b = a + 1;
        let (lat1, lon1, lat2, lon2) = (grid.lat[a], grid.lon[a], grid.lat[b], grid.lon[b]);
        if lat1.is_finite() && lon1.is_finite() && lat2.is_finite() && lon2.is_finite() {
            return Some(haversine_km(lat1, lon1, lat2, lon2).max(1.0));
        }
    }
    None
}

fn haversine_km(lat1: f32, lon1: f32, lat2: f32, lon2: f32) -> f32 {
    let r = 6371.0_f32;
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let lat1 = lat1.to_radians();
    let lat2 = lat2.to_radians();
    let a = (dlat * 0.5).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon * 0.5).sin().powi(2);
    2.0 * r * a.sqrt().asin()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_level_major_plane() {
        let values = vec![1.0, 2.0, 3.0, 4.0, 10.0, 20.0, 30.0, 40.0];
        let plane = extract_level_plane(&values, &[700, 500], 500, 2, 2).unwrap();
        assert_eq!(plane, vec![10.0, 20.0, 30.0, 40.0]);
    }

    #[test]
    fn marching_squares_draws_simple_gradient() {
        let values = vec![0.0, 1.0, 0.0, 1.0];
        let segments = contour_segments(&values, 2, 2, 0.5, None);
        assert_eq!(segments.len(), 1);
        let segment = segments[0];
        assert!((segment.a.x - 0.5).abs() < 1e-6);
        assert!((segment.b.x - 0.5).abs() < 1e-6);
    }
}
