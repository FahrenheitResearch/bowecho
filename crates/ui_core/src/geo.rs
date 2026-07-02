//! Shared map projection: the azimuthal-equidistant forward/inverse pair
//! both frontends project through (tiles.rs assumes exactly this projection
//! for tile quad corners). Free pure functions — no app state.

/// Forward azimuthal-equidistant: (lat, lon) → (east, north) km from center.
/// Spherical earth, R chosen so 1° latitude = 111.32 km (matches the radar
/// raster's planar convention).
pub fn aeqd_forward_km(center_lat: f64, center_lon: f64, lat: f64, lon: f64) -> (f64, f64) {
    const R_KM: f64 = 111.32 * 180.0 / std::f64::consts::PI;
    let (phi0, lam0) = (center_lat.to_radians(), center_lon.to_radians());
    let (phi, lam) = (lat.to_radians(), lon.to_radians());
    let dlam = lam - lam0;
    let cos_c = (phi0.sin() * phi.sin() + phi0.cos() * phi.cos() * dlam.cos()).clamp(-1.0, 1.0);
    let c = cos_c.acos();
    if c.abs() < 1e-12 {
        return (0.0, 0.0);
    }
    let k = R_KM * c / c.sin();
    let east = k * phi.cos() * dlam.sin();
    let north = k * (phi0.cos() * phi.sin() - phi0.sin() * phi.cos() * dlam.cos());
    (east, north)
}

/// Inverse azimuthal-equidistant: (east, north) km from center → (lat, lon).
pub fn aeqd_inverse_km(
    center_lat: f64,
    center_lon: f64,
    east_km: f64,
    north_km: f64,
) -> (f64, f64) {
    const R_KM: f64 = 111.32 * 180.0 / std::f64::consts::PI;
    let rho = east_km.hypot(north_km);
    if rho < 1e-9 {
        return (center_lat, center_lon);
    }
    // Clamp just short of the antipode: beyond ρ = πR the inverse wraps to
    // garbage on the far side of the globe (review finding F1/F6).
    let c = (rho / R_KM).min(std::f64::consts::PI - 1e-6);
    let (phi0, lam0) = (center_lat.to_radians(), center_lon.to_radians());
    let (sin_c, cos_c) = c.sin_cos();
    let phi = (cos_c * phi0.sin() + north_km * sin_c * phi0.cos() / rho)
        .clamp(-1.0, 1.0)
        .asin();
    let lam =
        lam0 + (east_km * sin_c).atan2(rho * phi0.cos() * cos_c - north_km * phi0.sin() * sin_c);
    (phi.to_degrees(), lam.to_degrees())
}

#[cfg(test)]
mod aeqd_tests {
    use super::{aeqd_forward_km, aeqd_inverse_km};

    #[test]
    fn round_trips_everywhere() {
        for &(clat, clon) in &[
            (39.0f64, -94.6f64),
            (48.4, -100.9),
            (64.5, -165.4),
            (21.0, -157.0),
        ] {
            for dlat in [-3.0f64, -1.0, 0.0, 0.5, 2.5] {
                for dlon in [-4.0f64, -1.5, 0.0, 1.0, 3.5] {
                    let (e, n) = aeqd_forward_km(clat, clon, clat + dlat, clon + dlon);
                    let (lat, lon) = aeqd_inverse_km(clat, clon, e, n);
                    assert!(
                        (lat - (clat + dlat)).abs() < 1e-6 && (lon - (clon + dlon)).abs() < 1e-6,
                        "round trip failed at center ({clat},{clon}) offset ({dlat},{dlon})"
                    );
                }
            }
        }
    }

    #[test]
    fn one_degree_latitude_is_111_32_km() {
        let (e, n) = aeqd_forward_km(45.0, -100.0, 46.0, -100.0);
        assert!(e.abs() < 1e-9);
        assert!((n - 111.32).abs() < 0.01, "{n}");
    }

    #[test]
    fn east_west_distance_shrinks_with_latitude() {
        // 1° of longitude at 60°N ≈ 55.66 km (cos 60 = 0.5) — the error class
        // the equirectangular mapping got wrong away from the center latitude.
        let (e, n) = aeqd_forward_km(60.0, -100.0, 60.0, -99.0);
        assert!(
            (e - 111.32 * 60.0f64.to_radians().cos()).abs() < 0.05,
            "{e}"
        );
        assert!(n.abs() < 0.6, "{n}"); // tiny great-circle northing
    }

    #[test]
    fn matches_planar_enu_near_the_center() {
        // Within radar display ranges the AEQD frame and the raster's planar
        // ENU about a centered radar agree to small fractions of a km.
        let (e, n) = aeqd_forward_km(39.0, -94.6, 39.9, -93.5);
        let planar_n = 0.9 * 111.32;
        let planar_e = 1.1 * 111.32 * 39.45f64.to_radians().cos(); // mid-lat scale
        assert!((n - planar_n).abs() < 1.0, "{n} vs {planar_n}");
        assert!((e - planar_e).abs() < 1.0, "{e} vs {planar_e}");
    }
}
