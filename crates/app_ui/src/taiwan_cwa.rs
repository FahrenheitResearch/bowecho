//! Taiwan CWA composite reflectivity map-layer ingest.
//!
//! CWA `O-A0059-001` is a numeric lon/lat dBZ grid.  It is not native polar
//! radar data, so BowEcho exposes it as a Grid / Composites overlay.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use eframe::egui;

pub(crate) const TAIWAN_CWA_RENDER_SCALE: f32 = 1.0;

#[derive(Clone)]
pub(crate) struct TaiwanCwaRasterFrame {
    pub(crate) time: DateTime<Utc>,
    pub(crate) identity: String,
    pub(crate) nx: usize,
    pub(crate) ny: usize,
    pub(crate) start_lon: f32,
    pub(crate) start_lat: f32,
    pub(crate) resolution_deg: f32,
    values: Arc<Vec<f32>>,
    pub(crate) generation: u64,
}

pub(crate) fn load_latest_frame() -> Result<TaiwanCwaRasterFrame, String> {
    let grid = data_source::grid_products::taiwan_cwa_latest_radar_grid()?;
    rasterize_grid(grid)
}

fn rasterize_grid(
    grid: data_source::grid_products::TaiwanCwaRadarGrid,
) -> Result<TaiwanCwaRasterFrame, String> {
    if grid.nx == 0 || grid.ny == 0 {
        return Err("Taiwan CWA composite decoded with an empty grid".to_owned());
    }
    let expected = grid
        .nx
        .checked_mul(grid.ny)
        .ok_or_else(|| "Taiwan CWA composite dimensions overflow".to_owned())?;
    if grid.values.len() != expected {
        return Err(format!(
            "Taiwan CWA composite value count mismatch: got {}, expected {expected}",
            grid.values.len()
        ));
    }

    let mut lat = Vec::with_capacity(expected);
    let mut lon = Vec::with_capacity(expected);
    for image_row in 0..grid.ny {
        let source_y = grid.ny - 1 - image_row;
        let row_lat = grid.start_lat + source_y as f32 * grid.resolution_deg;
        for x in 0..grid.nx {
            lat.push(row_lat);
            lon.push(grid.start_lon + x as f32 * grid.resolution_deg);
        }
    }

    if lat.iter().all(|value| !value.is_finite()) || lon.iter().all(|value| !value.is_finite()) {
        return Err("Taiwan CWA composite has no usable geolocation".to_owned());
    }
    let identity = grid.source_identity();
    let generation = taiwan_cwa_source_generation(&identity);
    Ok(TaiwanCwaRasterFrame {
        time: grid.time,
        identity,
        nx: grid.nx,
        ny: grid.ny,
        start_lon: grid.start_lon,
        start_lat: grid.start_lat,
        resolution_deg: grid.resolution_deg,
        values: Arc::new(grid.values),
        generation,
    })
}

pub(crate) fn taiwan_cwa_source_generation(identity: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    identity.hash(&mut hasher);
    hasher.finish()
}

fn colorize_reflectivity(value: f32) -> egui::Color32 {
    if data_source::grid_products::taiwan_cwa_is_nodata(value) {
        return egui::Color32::TRANSPARENT;
    }
    interpolate_stops(value, &REFLECTIVITY_STOPS)
}

pub(crate) fn sample_reflectivity_color(
    frame: &TaiwanCwaRasterFrame,
    lat: f32,
    lon: f32,
) -> egui::Color32 {
    if frame.resolution_deg <= 0.0 {
        return egui::Color32::TRANSPARENT;
    }
    let snap_grid_coord = |coord: f32| {
        let rounded = coord.round();
        if (coord - rounded).abs() < 1e-3 {
            rounded
        } else {
            coord
        }
    };
    let fx = snap_grid_coord((lon - frame.start_lon) / frame.resolution_deg);
    let fy = snap_grid_coord((lat - frame.start_lat) / frame.resolution_deg);
    if fx < 0.0
        || fy < 0.0
        || fx > (frame.nx.saturating_sub(1)) as f32
        || fy > (frame.ny.saturating_sub(1)) as f32
    {
        return egui::Color32::TRANSPARENT;
    }

    let x0 = fx.floor() as usize;
    let y0 = fy.floor() as usize;
    let x1 = (x0 + 1).min(frame.nx.saturating_sub(1));
    let y1 = (y0 + 1).min(frame.ny.saturating_sub(1));
    let tx = fx - x0 as f32;
    let ty = fy - y0 as f32;
    let get = |x: usize, y: usize| frame.values.get(y * frame.nx + x).copied();
    let Some(v00) = get(x0, y0) else {
        return egui::Color32::TRANSPARENT;
    };
    let Some(v10) = get(x1, y0) else {
        return egui::Color32::TRANSPARENT;
    };
    let Some(v01) = get(x0, y1) else {
        return egui::Color32::TRANSPARENT;
    };
    let Some(v11) = get(x1, y1) else {
        return egui::Color32::TRANSPARENT;
    };

    let samples = [
        (v00, (1.0 - tx) * (1.0 - ty)),
        (v10, tx * (1.0 - ty)),
        (v01, (1.0 - tx) * ty),
        (v11, tx * ty),
    ];
    let mut total = 0.0;
    let mut weight = 0.0;
    for (value, sample_weight) in samples {
        if !data_source::grid_products::taiwan_cwa_is_nodata(value) && value.is_finite() {
            total += value * sample_weight;
            weight += sample_weight;
        }
    }
    if weight <= f32::EPSILON {
        return egui::Color32::TRANSPARENT;
    }
    colorize_reflectivity(total / weight)
}

const REFLECTIVITY_STOPS: [(f32, [u8; 4]); 10] = [
    (-5.0, [0, 0, 0, 0]),
    (5.0, [4, 70, 150, 175]),
    (15.0, [36, 140, 255, 205]),
    (25.0, [35, 210, 80, 225]),
    (35.0, [245, 225, 40, 235]),
    (45.0, [245, 130, 28, 245]),
    (55.0, [220, 35, 35, 250]),
    (65.0, [190, 55, 210, 250]),
    (75.0, [250, 250, 250, 255]),
    (85.0, [180, 180, 180, 255]),
];

fn interpolate_stops(value: f32, stops: &[(f32, [u8; 4])]) -> egui::Color32 {
    let Some(&(first_value, first_color)) = stops.first() else {
        return egui::Color32::TRANSPARENT;
    };
    if value <= first_value {
        return rgba(first_color);
    }
    for pair in stops.windows(2) {
        let (left_v, left_c) = pair[0];
        let (right_v, right_c) = pair[1];
        if value <= right_v {
            let t = ((value - left_v) / (right_v - left_v).max(f32::EPSILON)).clamp(0.0, 1.0);
            let lerp = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
            return egui::Color32::from_rgba_unmultiplied(
                lerp(left_c[0], right_c[0]),
                lerp(left_c[1], right_c[1]),
                lerp(left_c[2], right_c[2]),
                lerp(left_c[3], right_c[3]),
            );
        }
    }
    rgba(
        stops
            .last()
            .map(|(_, color)| *color)
            .unwrap_or([0, 0, 0, 0]),
    )
}

fn rgba(color: [u8; 4]) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(color[0], color[1], color[2], color[3])
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn taiwan_cwa_raster_samples_bottom_left_numeric_grid() {
        let grid = data_source::grid_products::TaiwanCwaRadarGrid {
            time: Utc.with_ymd_and_hms(2026, 6, 27, 2, 50, 0).unwrap(),
            nx: 3,
            ny: 2,
            start_lon: 115.0,
            start_lat: 18.0,
            resolution_deg: 0.0125,
            units: "dBZ".to_owned(),
            values: vec![10.0, -99.0, 30.0, 40.0, 50.0, -999.0],
        };
        let frame = rasterize_grid(grid).expect("rasterize Taiwan CWA grid");
        assert_eq!(frame.nx, 3);
        assert_eq!(frame.ny, 2);
        assert_ne!(
            sample_reflectivity_color(&frame, 18.0, 115.0),
            egui::Color32::TRANSPARENT
        );
        assert_eq!(
            sample_reflectivity_color(&frame, 18.0, 115.0125),
            egui::Color32::TRANSPARENT
        );
        assert_eq!(
            sample_reflectivity_color(&frame, 17.5, 115.0),
            egui::Color32::TRANSPARENT
        );
    }
}
