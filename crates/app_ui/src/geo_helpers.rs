//! Pure geometry helpers: great-circle distance, longitude normalization,
//! angle deltas, graticule spacing, bounds/extent math, and the basemap
//! point-in-polygon test — params in, value out, no `ViewerApp` state.
//!
//! v0.29.4 phase-1 extraction #6 (docs/main-decomposition-plan.md): every
//! body moved VERBATIM from main.rs — the only edits are `use` lines and
//! the pub(crate) promotions listed in the extraction commit message.

use crate::basemap_data;

pub(crate) fn basemap_line_contains_lon_lat(
    line: &basemap_data::BasemapLine,
    lon: f32,
    lat: f32,
) -> bool {
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

pub(crate) fn range_radius_deg(latitude_deg: f32, range_km: f32) -> (f32, f32) {
    let lat_radius = range_km / 111.32;
    let lon_scale = (111.32 * latitude_deg.to_radians().cos().abs()).max(22.0);
    (lat_radius, range_km / lon_scale)
}

pub(crate) fn angle_delta_deg(left: f32, right: f32) -> f32 {
    let delta = (left - right).abs().rem_euclid(360.0);
    delta.min(360.0 - delta)
}

/// Two screen-corner (lon, lat) samples -> rw-ui plot-domain bounds
/// `(west, east, south, north)`, whichever way the box was dragged.
pub(crate) fn plot_domain_bounds_from_corners(
    a: (f32, f32),
    b: (f32, f32),
) -> (f64, f64, f64, f64) {
    (
        f64::from(a.0.min(b.0)),
        f64::from(a.0.max(b.0)),
        f64::from(a.1.min(b.1)),
        f64::from(a.1.max(b.1)),
    )
}

pub(crate) fn graticule_step(visible_degrees: f32) -> f32 {
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

pub(crate) fn normalize_lon(longitude_deg: f32) -> f32 {
    let mut longitude_deg = longitude_deg;
    while longitude_deg > 180.0 {
        longitude_deg -= 360.0;
    }
    while longitude_deg < -180.0 {
        longitude_deg += 360.0;
    }
    longitude_deg
}

pub(crate) fn haversine_km(lat_a: f32, lon_a: f32, lat_b: f32, lon_b: f32) -> f32 {
    let earth_radius_km = 6371.0_f32;
    let d_lat = (lat_b - lat_a).to_radians();
    let d_lon = (lon_b - lon_a).to_radians();
    let lat_a = lat_a.to_radians();
    let lat_b = lat_b.to_radians();
    let a = (d_lat / 2.0).sin().powi(2) + lat_a.cos() * lat_b.cos() * (d_lon / 2.0).sin().powi(2);
    2.0 * earth_radius_km * a.sqrt().atan2((1.0 - a).max(0.0).sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn haversine_is_zero_for_same_point() {
        assert!(haversine_km(35.333, -97.278, 35.333, -97.278) < 0.001);
    }

    #[test]
    fn plot_domain_bounds_order_any_drag_direction() {
        // North-east drag and south-west drag produce identical bounds.
        let bounds = plot_domain_bounds_from_corners((-99.5, 36.2), (-97.0, 34.1));
        let flipped = plot_domain_bounds_from_corners((-97.0, 34.1), (-99.5, 36.2));
        assert_eq!(bounds, flipped);
        let (west, east, south, north) = bounds;
        assert!(west < east);
        assert!(south < north);
        // f32 corners widen to f64 bounds: compare within f32 precision.
        assert!((west - -99.5).abs() < 1e-4);
        assert!((east - -97.0).abs() < 1e-4);
        assert!((south - 34.1).abs() < 1e-4);
        assert!((north - 36.2).abs() < 1e-4);
    }
}
