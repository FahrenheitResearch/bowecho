// SPDX-License-Identifier: Apache-2.0
//
// Pattern copied with attribution from BowEcho crates/app_ui/src/
// geo_helpers.rs and hazard_geom.rs @ 6dfcb9f (pure geometry helpers).

//! Pure geometry: great-circle distance, longitude normalization, bbox
//! and point-in-polygon tests over the vendored basemap tables.

use crate::basemap_data::BasemapLine;

pub fn normalize_lon(longitude_deg: f32) -> f32 {
    let mut longitude_deg = longitude_deg;
    while longitude_deg > 180.0 {
        longitude_deg -= 360.0;
    }
    while longitude_deg < -180.0 {
        longitude_deg += 360.0;
    }
    longitude_deg
}

pub fn haversine_km(lat_a: f32, lon_a: f32, lat_b: f32, lon_b: f32) -> f32 {
    let earth_radius_km = 6371.0_f32;
    let d_lat = (lat_b - lat_a).to_radians();
    let d_lon = (lon_b - lon_a).to_radians();
    let lat_a = lat_a.to_radians();
    let lat_b = lat_b.to_radians();
    let a = (d_lat / 2.0).sin().powi(2) + lat_a.cos() * lat_b.cos() * (d_lon / 2.0).sin().powi(2);
    2.0 * earth_radius_km * a.sqrt().atan2((1.0 - a).max(0.0).sqrt())
}

pub fn bbox_contains(bbox: [f32; 4], lon: f32, lat: f32) -> bool {
    lon >= bbox[0] && lon <= bbox[2] && lat >= bbox[1] && lat <= bbox[3]
}

/// Even-odd point-in-polygon over one basemap outline.
pub fn basemap_line_contains_lon_lat(line: &BasemapLine, lon: f32, lat: f32) -> bool {
    if line.points.len() < 3 {
        return false;
    }
    let mut inside = false;
    let mut previous = line.points[line.points.len() - 1];
    for current in line.points {
        let crosses = (current.1 > lat) != (previous.1 > lat);
        if crosses {
            let lon_at_lat =
                (previous.0 - current.0) * (lat - current.1) / (previous.1 - current.1) + current.0;
            if lon < lon_at_lat {
                inside = !inside;
            }
        }
        previous = *current;
    }
    inside
}

/// Graticule spacing for a given visible span (degrees).
pub fn graticule_step(visible_degrees: f32) -> f32 {
    if visible_degrees > 140.0 {
        30.0
    } else if visible_degrees > 80.0 {
        20.0
    } else if visible_degrees > 40.0 {
        10.0
    } else if visible_degrees > 16.0 {
        5.0
    } else if visible_degrees > 6.0 {
        2.0
    } else if visible_degrees > 2.0 {
        1.0
    } else if visible_degrees > 0.7 {
        0.5
    } else {
        0.25
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn haversine_reference_points() {
        assert!(haversine_km(35.0, -97.0, 35.0, -97.0) < 0.001);
        // One degree of latitude ≈ 111.2 km.
        let km = haversine_km(35.0, -97.0, 36.0, -97.0);
        assert!((km - 111.2).abs() < 0.5, "{km}");
    }

    #[test]
    fn normalize_lon_wraps_both_ways() {
        assert_eq!(normalize_lon(190.0), -170.0);
        assert_eq!(normalize_lon(-190.0), 170.0);
        assert_eq!(normalize_lon(45.0), 45.0);
    }
}
