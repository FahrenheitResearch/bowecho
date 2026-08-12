// SPDX-License-Identifier: Apache-2.0
//
// Projection math ADOPTED from rusty-weather rw-sim/src/domain.rs
// (Apache-2.0, @ 31b681e3): the WPS module_llxy-compatible Lambert
// forward/inverse pair and the DomainSpec outline sampling, per the
// 2026-08-07 recon ("ADOPT the projection math; DROP approximate_state_gib
// — ArWen owns real numbers"). The HRRR source-window check and the
// case-named preset were deliberately not carried over.

//! The Lambert forecast domain Studio draws and edits on the map.
//!
//! Geometry only: nx/ny/dx and the projection. Vertical levels, physics,
//! time step ratification — everything else — is engine-owned and arrives
//! through `--resolve`.

use std::f64::consts::PI;

use serde::{Deserialize, Serialize};

const EARTH_RADIUS_M: f64 = 6_370_000.0;
const RAD_PER_DEG: f64 = PI / 180.0;
const DEG_PER_RAD: f64 = 180.0 / PI;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LambertDomain {
    /// Mass-grid dimensions.
    pub nx: u32,
    pub ny: u32,
    /// Grid spacing, meters (Lambert requires dx == dy).
    pub dx_m: f64,
    /// Projection reference = domain center.
    pub ref_lat: f64,
    pub ref_lon: f64,
    pub truelat1: f64,
    pub truelat2: f64,
    pub stand_lon: f64,
}

impl LambertDomain {
    /// A fresh domain centered at a clicked/dragged point with the WRF
    /// convention true latitudes for the hemisphere.
    pub fn centered_at(ref_lat: f64, ref_lon: f64, nx: u32, ny: u32, dx_m: f64) -> Self {
        let (truelat1, truelat2) = if ref_lat < 0.0 {
            (-30.0, -60.0)
        } else {
            (30.0, 60.0)
        };
        Self {
            nx,
            ny,
            dx_m,
            ref_lat,
            ref_lon,
            truelat1,
            truelat2,
            stand_lon: ref_lon,
        }
    }

    pub fn width_km(&self) -> f64 {
        self.nx as f64 * self.dx_m / 1000.0
    }

    pub fn height_km(&self) -> f64 {
        self.ny as f64 * self.dx_m / 1000.0
    }

    /// WRF practice: ~5 s of time step per km of dx (engine ratifies).
    pub fn recommended_time_step_seconds(&self) -> f64 {
        ((self.dx_m / 1_000.0) * 5.0).floor().max(1.0)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.nx < 16 || self.ny < 16 {
            return Err("nx and ny must each be at least 16".into());
        }
        if !self.dx_m.is_finite() || self.dx_m <= 0.0 {
            return Err("dx must be finite and positive".into());
        }
        for (name, value) in [
            ("ref_lat", self.ref_lat),
            ("ref_lon", self.ref_lon),
            ("truelat1", self.truelat1),
            ("truelat2", self.truelat2),
            ("stand_lon", self.stand_lon),
        ] {
            if !value.is_finite() {
                return Err(format!("{name} must be finite"));
            }
        }
        if !(-89.0..89.0).contains(&self.ref_lat) {
            return Err("ref_lat must be inside (-89, 89)".into());
        }
        if !(-180.0..=180.0).contains(&self.ref_lon) || !(-180.0..=180.0).contains(&self.stand_lon)
        {
            return Err("ref_lon and stand_lon must be inside [-180, 180]".into());
        }
        if !(0.0..89.0).contains(&self.truelat1.abs())
            || !(0.0..89.0).contains(&self.truelat2.abs())
            || self.truelat1.signum() != self.truelat2.signum()
        {
            return Err(
                "Lambert true latitudes must be nonzero, share one hemisphere, and stay below 89°"
                    .into(),
            );
        }
        let (lat, lon) =
            self.latlon_at_grid((self.nx as f64 + 1.0) / 2.0, (self.ny as f64 + 1.0) / 2.0);
        if !lat.is_finite() || !lon.is_finite() {
            return Err("Lambert projection produced non-finite coordinates".into());
        }
        Ok(())
    }

    /// WPS/module_llxy-compatible grid coordinate → (lat, lon). One-based;
    /// the reference point is ((nx+1)/2, (ny+1)/2); the outer outline runs
    /// x=0.5..nx+0.5, y=0.5..ny+0.5.
    pub fn latlon_at_grid(&self, x: f64, y: f64) -> (f64, f64) {
        lambert_grid_latlon(
            self.ref_lat,
            self.ref_lon,
            self.truelat1,
            self.truelat2,
            self.stand_lon,
            self.dx_m,
            (self.nx as f64 + 1.0) / 2.0,
            (self.ny as f64 + 1.0) / 2.0,
            x,
            y,
        )
    }

    /// Inverse: (lat, lon) → fractional one-based grid coordinate.
    pub fn grid_at_latlon(&self, lat: f64, lon: f64) -> (f64, f64) {
        lambert_grid_ij(
            self.ref_lat,
            self.ref_lon,
            self.truelat1,
            self.truelat2,
            self.stand_lon,
            self.dx_m,
            (self.nx as f64 + 1.0) / 2.0,
            (self.ny as f64 + 1.0) / 2.0,
            lat,
            lon,
        )
    }

    /// Closed outer-boundary outline as (lon, lat), `samples_per_edge`
    /// points per edge.
    pub fn outline_lon_lat(&self, samples_per_edge: usize) -> Vec<(f64, f64)> {
        let samples = samples_per_edge.max(2);
        let x0 = 0.5;
        let x1 = self.nx as f64 + 0.5;
        let y0 = 0.5;
        let y1 = self.ny as f64 + 0.5;
        let mut points = Vec::with_capacity(samples * 4 + 1);
        let mut push = |x: f64, y: f64| {
            let (lat, lon) = self.latlon_at_grid(x, y);
            points.push((lon, lat));
        };
        for index in 0..samples {
            let t = index as f64 / (samples - 1) as f64;
            push(x0 + (x1 - x0) * t, y0);
        }
        for index in 1..samples {
            let t = index as f64 / (samples - 1) as f64;
            push(x1, y0 + (y1 - y0) * t);
        }
        for index in 1..samples {
            let t = index as f64 / (samples - 1) as f64;
            push(x1 - (x1 - x0) * t, y1);
        }
        for index in 1..samples {
            let t = index as f64 / (samples - 1) as f64;
            push(x0, y1 - (y1 - y0) * t);
        }
        if let Some(first) = points.first().copied() {
            points.push(first);
        }
        points
    }

    /// The four outer corners as (lon, lat): SW, SE, NE, NW.
    pub fn corners_lon_lat(&self) -> [(f64, f64); 4] {
        let x1 = self.nx as f64 + 0.5;
        let y1 = self.ny as f64 + 0.5;
        [(0.5, 0.5), (x1, 0.5), (x1, y1), (0.5, y1)].map(|(x, y)| {
            let (lat, lon) = self.latlon_at_grid(x, y);
            (lon, lat)
        })
    }
}

/// One nest level's placement inside its parent, as the ENGINE fitted it
/// (`[[domain]]` keys verbatim). Studio uses this to PAINT the resolved
/// tree; legality of any placement stays an engine answer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NestPlacement {
    pub i_parent_start: f64,
    pub j_parent_start: f64,
    pub parent_grid_ratio: f64,
    pub nx: u32,
    pub ny: u32,
}

/// Fold a coordinate of the DEEPEST grid of `path` up to root-grid
/// coordinates (the WPS convention: child mass point 1 sits at parent
/// mass point `i_parent_start`, so child x maps to parent
/// `i_parent_start + (x - 1) / ratio`).
pub fn path_grid_to_root(path: &[NestPlacement], x: f64, y: f64) -> (f64, f64) {
    let (mut gx, mut gy) = (x, y);
    for level in path.iter().rev() {
        gx = level.i_parent_start + (gx - 1.0) / level.parent_grid_ratio;
        gy = level.j_parent_start + (gy - 1.0) / level.parent_grid_ratio;
    }
    (gx, gy)
}

/// The inverse: descend a ROOT-grid coordinate into the deepest grid of
/// `path` (empty path = identity). This is how a cursor position becomes
/// parent-cell coordinates for nest placement.
pub fn root_grid_to_path(path: &[NestPlacement], gx: f64, gy: f64) -> (f64, f64) {
    let (mut x, mut y) = (gx, gy);
    for level in path {
        x = (x - level.i_parent_start) * level.parent_grid_ratio + 1.0;
        y = (y - level.j_parent_start) * level.parent_grid_ratio + 1.0;
    }
    (x, y)
}

/// The closed outline of a nest reached through `path` (outermost nest
/// first), in (lon, lat). Paint-grade WPS convention: child mass point 1
/// sits at parent mass point `i_parent_start`, so a child grid
/// coordinate x maps to parent `i_parent_start + (x - 1) / ratio`.
pub fn nest_outline_lon_lat(
    root: &LambertDomain,
    path: &[NestPlacement],
    samples_per_edge: usize,
) -> Vec<(f64, f64)> {
    nest_ring_lon_lat(root, path, 0.0, samples_per_edge)
}

/// The closed outline of the deepest domain of `path`, INSET by
/// `inset_cells` of its OWN grid on every side (0.0 = the outer
/// boundary). Empty path = the root domain itself. Used to paint the
/// engine-reported boundary-clearance keepout band inside a parent's
/// edge; the inset never goes past the domain's midline.
pub fn nest_ring_lon_lat(
    root: &LambertDomain,
    path: &[NestPlacement],
    inset_cells: f64,
    samples_per_edge: usize,
) -> Vec<(f64, f64)> {
    let (nx, ny) = match path.last() {
        Some(deepest) => (deepest.nx as f64, deepest.ny as f64),
        None => (root.nx as f64, root.ny as f64),
    };
    let samples = samples_per_edge.max(2);
    let inset = inset_cells
        .max(0.0)
        .min((nx - 1.0) / 2.0)
        .min((ny - 1.0) / 2.0);
    let x0 = 0.5 + inset;
    let x1 = nx + 0.5 - inset;
    let y0 = 0.5 + inset;
    let y1 = ny + 0.5 - inset;
    let mut points = Vec::with_capacity(samples * 4 + 1);
    let mut push = |x: f64, y: f64| {
        let (gx, gy) = path_grid_to_root(path, x, y);
        let (lat, lon) = root.latlon_at_grid(gx, gy);
        points.push((lon, lat));
    };
    for index in 0..samples {
        let t = index as f64 / (samples - 1) as f64;
        push(x0 + (x1 - x0) * t, y0);
    }
    for index in 1..samples {
        let t = index as f64 / (samples - 1) as f64;
        push(x1, y0 + (y1 - y0) * t);
    }
    for index in 1..samples {
        let t = index as f64 / (samples - 1) as f64;
        push(x1 - (x1 - x0) * t, y1);
    }
    for index in 1..samples {
        let t = index as f64 / (samples - 1) as f64;
        push(x0, y1 - (y1 - y0) * t);
    }
    if let Some(first) = points.first().copied() {
        points.push(first);
    }
    points
}

#[allow(clippy::too_many_arguments)]
fn lambert_grid_latlon(
    ref_lat: f64,
    ref_lon: f64,
    truelat1: f64,
    truelat2: f64,
    stand_lon: f64,
    dx_m: f64,
    known_x: f64,
    known_y: f64,
    x: f64,
    y: f64,
) -> (f64, f64) {
    let hemi = if truelat1 < 0.0 { -1.0 } else { 1.0 };
    let cone = lambert_cone(truelat1, truelat2);
    let rebydx = EARTH_RADIUS_M / dx_m;
    let delta_lon = wrap_180(ref_lon - stand_lon);
    let ctl1r = (truelat1 * RAD_PER_DEG).cos();
    let rsw = rebydx * ctl1r / cone
        * (((90.0 * hemi - ref_lat) * RAD_PER_DEG / 2.0).tan()
            / ((90.0 * hemi - truelat1) * RAD_PER_DEG / 2.0).tan())
        .powf(cone);
    let arg = cone * delta_lon * RAD_PER_DEG;
    let pole_i = hemi * known_x - hemi * rsw * arg.sin();
    let pole_j = hemi * known_y + rsw * arg.cos();
    let chi1 = (90.0 - hemi * truelat1) * RAD_PER_DEG;
    let chi2 = (90.0 - hemi * truelat2) * RAD_PER_DEG;
    let xx = hemi * x - pole_i;
    let yy = pole_j - hemi * y;
    let radius_squared = xx * xx + yy * yy;
    if radius_squared == 0.0 {
        return (90.0 * hemi, wrap_180(stand_lon));
    }
    let radius = radius_squared.sqrt() / rebydx;
    let lon = wrap_180(stand_lon + DEG_PER_RAD * (hemi * xx).atan2(yy) / cone);
    let chi = if chi1 == chi2 {
        2.0 * ((radius / chi1.tan()).powf(1.0 / cone) * (chi1 * 0.5).tan()).atan()
    } else {
        2.0 * ((radius * cone / chi1.sin()).powf(1.0 / cone) * (chi1 * 0.5).tan()).atan()
    };
    ((90.0 - chi * DEG_PER_RAD) * hemi, lon)
}

#[allow(clippy::too_many_arguments)]
fn lambert_grid_ij(
    ref_lat: f64,
    ref_lon: f64,
    truelat1: f64,
    truelat2: f64,
    stand_lon: f64,
    dx_m: f64,
    known_x: f64,
    known_y: f64,
    lat: f64,
    lon: f64,
) -> (f64, f64) {
    let hemi = if truelat1 < 0.0 { -1.0 } else { 1.0 };
    let cone = lambert_cone(truelat1, truelat2);
    let rebydx = EARTH_RADIUS_M / dx_m;
    let ctl1r = (truelat1 * RAD_PER_DEG).cos();
    let reference_radius = rebydx * ctl1r / cone
        * (((90.0 * hemi - ref_lat) * RAD_PER_DEG / 2.0).tan()
            / ((90.0 * hemi - truelat1) * RAD_PER_DEG / 2.0).tan())
        .powf(cone);
    let reference_arg = cone * wrap_180(ref_lon - stand_lon) * RAD_PER_DEG;
    let pole_i = hemi * known_x - hemi * reference_radius * reference_arg.sin();
    let pole_j = hemi * known_y + reference_radius * reference_arg.cos();
    let radius = rebydx * ctl1r / cone
        * (((90.0 * hemi - lat) * RAD_PER_DEG / 2.0).tan()
            / ((90.0 * hemi - truelat1) * RAD_PER_DEG / 2.0).tan())
        .powf(cone);
    let arg = cone * wrap_180(lon - stand_lon) * RAD_PER_DEG;
    let x = pole_i + hemi * radius * arg.sin();
    let y = pole_j - radius * arg.cos();
    (hemi * x, hemi * y)
}

fn lambert_cone(truelat1: f64, truelat2: f64) -> f64 {
    if (truelat1 - truelat2).abs() > 0.1 {
        let numerator =
            (truelat1 * RAD_PER_DEG).cos().log10() - (truelat2 * RAD_PER_DEG).cos().log10();
        let denominator = ((45.0 - truelat1.abs() / 2.0) * RAD_PER_DEG).tan().log10()
            - ((45.0 - truelat2.abs() / 2.0) * RAD_PER_DEG).tan().log10();
        numerator / denominator
    } else {
        (truelat1.abs() * RAD_PER_DEG).sin()
    }
}

fn wrap_180(value: f64) -> f64 {
    let mut wrapped = value;
    while wrapped > 180.0 {
        wrapped -= 360.0;
    }
    while wrapped < -180.0 {
        wrapped += 360.0;
    }
    wrapped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> LambertDomain {
        LambertDomain::centered_at(35.5, -97.5, 300, 240, 3_000.0)
    }

    #[test]
    fn reference_coordinate_is_exact_domain_center() {
        let domain = sample();
        let (lat, lon) = domain.latlon_at_grid(
            (domain.nx as f64 + 1.0) / 2.0,
            (domain.ny as f64 + 1.0) / 2.0,
        );
        assert!((lat - domain.ref_lat).abs() < 1.0e-10, "{lat}");
        assert!((lon - domain.ref_lon).abs() < 1.0e-10, "{lon}");
    }

    #[test]
    fn forward_inverse_round_trips_across_the_grid() {
        let domain = sample();
        for (x, y) in [(0.5, 0.5), (1.0, 1.0), (150.5, 120.5), (300.5, 240.5)] {
            let (lat, lon) = domain.latlon_at_grid(x, y);
            let (xi, yj) = domain.grid_at_latlon(lat, lon);
            assert!((xi - x).abs() < 1.0e-8, "x {x} → {xi}");
            assert!((yj - y).abs() < 1.0e-8, "y {y} → {yj}");
        }
    }

    #[test]
    fn outline_is_closed_and_finite() {
        let points = sample().outline_lon_lat(17);
        assert_eq!(points.first(), points.last());
        assert!(points.len() >= 65);
        assert!(
            points
                .iter()
                .all(|(lon, lat)| lon.is_finite() && lat.is_finite())
        );
    }

    #[test]
    fn southern_hemisphere_domain_validates_and_centers() {
        let domain = LambertDomain::centered_at(-32.0, 135.0, 120, 100, 3_000.0);
        domain.validate().unwrap();
        assert_eq!(domain.truelat1, -30.0);
        let (lat, lon) = domain.latlon_at_grid(
            (domain.nx as f64 + 1.0) / 2.0,
            (domain.ny as f64 + 1.0) / 2.0,
        );
        assert!((lat - domain.ref_lat).abs() < 1.0e-10, "{lat}");
        assert!((lon - domain.ref_lon).abs() < 1.0e-10, "{lon}");
    }

    #[test]
    fn mixed_hemisphere_true_latitudes_are_rejected() {
        let mut domain = sample();
        domain.truelat2 = -60.0;
        assert!(domain.validate().is_err());
    }

    #[test]
    fn nest_outline_stays_inside_its_parent_and_is_closed() {
        // A 4:1 nest anchored at parent (52, 42) spanning 102x80 child
        // points covers parent columns 52 .. 52+101/4 ≈ 77.25.
        let root = sample();
        let nest = NestPlacement {
            i_parent_start: 52.0,
            j_parent_start: 42.0,
            parent_grid_ratio: 4.0,
            nx: 102,
            ny: 80,
        };
        let outline = nest_outline_lon_lat(&root, &[nest], 9);
        assert_eq!(outline.first(), outline.last());
        for (lon, lat) in &outline {
            let (gx, gy) = root.grid_at_latlon(*lat, *lon);
            assert!(gx > 51.0 && gx < 78.0, "gx {gx}");
            assert!(gy > 41.0 && gy < 63.0, "gy {gy}");
        }
        // Two levels: a 3:1 nest inside the 4:1 nest maps through both.
        let inner = NestPlacement {
            i_parent_start: 20.0,
            j_parent_start: 15.0,
            parent_grid_ratio: 3.0,
            nx: 60,
            ny: 45,
        };
        let deep = nest_outline_lon_lat(&root, &[nest, inner], 5);
        for (lon, lat) in &deep {
            let (gx, _gy) = root.grid_at_latlon(*lat, *lon);
            // Inner nest spans child columns 20..~39.7 of the 4:1 nest →
            // parent columns 52+19/4 .. 52+38.7/4.
            assert!(gx > 56.0 && gx < 62.5, "gx {gx}");
        }
        // Empty path = the root outline itself.
        assert_eq!(nest_outline_lon_lat(&root, &[], 9), root.outline_lon_lat(9));
    }

    #[test]
    fn path_grid_round_trips_up_and_down_two_levels() {
        let outer = NestPlacement {
            i_parent_start: 52.0,
            j_parent_start: 42.0,
            parent_grid_ratio: 4.0,
            nx: 408,
            ny: 320,
        };
        let inner = NestPlacement {
            i_parent_start: 150.0,
            j_parent_start: 120.0,
            parent_grid_ratio: 3.0,
            nx: 240,
            ny: 180,
        };
        let path = [outer, inner];
        // Child point 1 sits AT the parent anchor.
        let (gx, gy) = path_grid_to_root(&path[..1], 1.0, 1.0);
        assert_eq!((gx, gy), (52.0, 42.0));
        // Descend inverts fold exactly, through both levels.
        for (x, y) in [(1.0, 1.0), (120.5, 90.5), (240.0, 180.0)] {
            let (gx, gy) = path_grid_to_root(&path, x, y);
            let (bx, by) = root_grid_to_path(&path, gx, gy);
            assert!((bx - x).abs() < 1e-9, "{x} → {bx}");
            assert!((by - y).abs() < 1e-9, "{y} → {by}");
        }
        // One-level descent gives the outer nest's own coordinates.
        let (dx, dy) = root_grid_to_path(&path[..1], 52.0, 42.0);
        assert_eq!((dx, dy), (1.0, 1.0));
    }

    #[test]
    fn inset_ring_stays_strictly_inside_the_outline() {
        let root = sample();
        let nest = NestPlacement {
            i_parent_start: 52.0,
            j_parent_start: 42.0,
            parent_grid_ratio: 4.0,
            nx: 102,
            ny: 80,
        };
        // Root ring, inset 10 cells: every point maps into (10.5, nx-9.5).
        let ring = nest_ring_lon_lat(&root, &[], 10.0, 9);
        assert_eq!(ring.first(), ring.last());
        for (lon, lat) in &ring {
            let (gx, gy) = root.grid_at_latlon(*lat, *lon);
            assert!(gx > 10.4 && gx < root.nx as f64 - 9.4, "gx {gx}");
            assert!(gy > 10.4 && gy < root.ny as f64 - 9.4, "gy {gy}");
        }
        // Zero inset equals the plain outline.
        assert_eq!(
            nest_ring_lon_lat(&root, &[nest], 0.0, 9),
            nest_outline_lon_lat(&root, &[nest], 9)
        );
        // An absurd inset clamps at the midline instead of inverting.
        let clamped = nest_ring_lon_lat(&root, &[nest], 1e6, 5);
        assert!(
            clamped
                .iter()
                .all(|(lon, lat)| lon.is_finite() && lat.is_finite())
        );
    }

    #[test]
    fn size_and_time_step_helpers() {
        let domain = sample();
        assert!((domain.width_km() - 900.0).abs() < 1e-9);
        assert!((domain.height_km() - 720.0).abs() < 1e-9);
        assert_eq!(domain.recommended_time_step_seconds(), 15.0);
    }
}
