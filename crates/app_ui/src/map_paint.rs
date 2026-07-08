//! Map-paint sub-move A (projection): the azimuthal-equidistant screen
//! projection primitives and the `GeoBounds` view-extent type, moved
//! VERBATIM out of `main.rs` (v0.30 decomposition, the final `map_paint`
//! extraction). These are called from the residual paint dispatch in
//! `main.rs` (`single_pane_canvas`/`grid_canvas` and the first `impl
//! ViewerApp` block) and from sibling feature modules (annotate, tropical,
//! tor_tracks, hazard_ui, event_explorer, gbvtd_retrieval, max_ref_swath,
//! overlays), so the moved items are promoted to `pub(crate)`.

use crate::*;

impl ViewerApp {
    /// Azimuthal-equidistant projection about the map center (north up):
    /// screen offsets are true great-circle kilometres, so range and azimuth
    /// are exact at the center and the frame matches the radar raster's
    /// planar ENU geometry (the equirectangular mapping it replaces skewed
    /// east-west distances away from the center latitude).
    pub(crate) fn lon_lat_to_screen(
        &self,
        rect: egui::Rect,
        longitude_deg: f32,
        latitude_deg: f32,
    ) -> egui::Pos2 {
        let (east_km, north_km) = aeqd_forward_km(
            self.map_center_lat as f64,
            self.map_center_lon as f64,
            latitude_deg as f64,
            longitude_deg as f64,
        );
        let px_per_km = self.map_scale / 111.32;
        egui::pos2(
            rect.center().x + east_km as f32 * px_per_km,
            rect.center().y - north_km as f32 * px_per_km,
        )
    }

    pub(crate) fn screen_to_lon_lat(&self, rect: egui::Rect, position: egui::Pos2) -> (f32, f32) {
        let km_per_px = 111.32 / self.map_scale;
        let east_km = (position.x - rect.center().x) * km_per_px;
        let north_km = (rect.center().y - position.y) * km_per_px;
        let (lat, lon) = aeqd_inverse_km(
            self.map_center_lat as f64,
            self.map_center_lon as f64,
            east_km as f64,
            north_km as f64,
        );
        (normalize_lon(lon as f32), lat as f32)
    }

    pub(crate) fn visible_geo_bounds(&self, rect: egui::Rect) -> GeoBounds {
        // Under AEQD the lat/lon extremes of the view sit on the EDGES, not
        // two corners (parallels bow poleward, meridians converge) — sample
        // four corners plus four edge midpoints (review finding F1).
        let samples = [
            rect.left_top(),
            rect.right_top(),
            rect.left_bottom(),
            rect.right_bottom(),
            egui::pos2(rect.center().x, rect.top()),
            egui::pos2(rect.center().x, rect.bottom()),
            egui::pos2(rect.left(), rect.center().y),
            egui::pos2(rect.right(), rect.center().y),
        ];
        let mut bounds = GeoBounds {
            west: f32::INFINITY,
            east: f32::NEG_INFINITY,
            south: f32::INFINITY,
            north: f32::NEG_INFINITY,
        };
        for sample in samples {
            let (lon, lat) = self.screen_to_lon_lat(rect, sample);
            bounds.west = bounds.west.min(lon);
            bounds.east = bounds.east.max(lon);
            bounds.south = bounds.south.min(lat);
            bounds.north = bounds.north.max(lat);
        }
        // If a pole is inside the view radius every longitude is visible.
        let km_per_px = 111.32 / self.map_scale;
        let view_radius_km = (rect.width().hypot(rect.height()) * 0.5 * km_per_px) as f64;
        const KM_PER_DEG: f64 = 111.32;
        let north_pole_km = (90.0 - self.map_center_lat as f64) * KM_PER_DEG;
        let south_pole_km = (90.0 + self.map_center_lat as f64) * KM_PER_DEG;
        if north_pole_km < view_radius_km || south_pole_km < view_radius_km {
            bounds.west = -180.0;
            bounds.east = 180.0;
        }
        bounds.south = bounds.south.clamp(-85.0, 85.0);
        bounds.north = bounds.north.clamp(-85.0, 85.0);
        bounds
    }

    /// Deviation of local "screen north" from straight up at a geo point —
    /// the AEQD meridian-convergence angle (radians, clockwise positive).
    /// The radar raster is planar ENU about the radar, so its quad is
    /// rotated by this angle to sit correctly in the AEQD frame (F2).
    pub(crate) fn aeqd_north_angle(
        &self,
        rect: egui::Rect,
        latitude_deg: f32,
        longitude_deg: f32,
    ) -> f32 {
        let base = self.lon_lat_to_screen(rect, longitude_deg, latitude_deg);
        let north = self.lon_lat_to_screen(rect, longitude_deg, latitude_deg + 0.05);
        let v = north - base;
        if v.length_sq() < 1e-12 {
            return 0.0;
        }
        v.x.atan2(-v.y)
    }

    pub(crate) fn clamp_map_center(&mut self) {
        self.map_center_lon = normalize_lon(self.map_center_lon);
        self.map_center_lat = self.map_center_lat.clamp(-85.0, 85.0);
    }

    pub(crate) fn lon_screen_scale(&self) -> f32 {
        self.map_center_lat.to_radians().cos().abs().max(0.02)
    }

    pub(crate) fn lon_pixels_per_degree(&self) -> f32 {
        self.map_scale * self.lon_screen_scale()
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct GeoBounds {
    pub(crate) west: f32,
    pub(crate) south: f32,
    pub(crate) east: f32,
    pub(crate) north: f32,
}

impl GeoBounds {
    pub(crate) fn expand(self, degrees: f32) -> Self {
        Self {
            west: self.west - degrees,
            south: self.south - degrees,
            east: self.east + degrees,
            north: self.north + degrees,
        }
    }

    pub(crate) fn intersect(self, other: Self) -> Option<Self> {
        let bounds = Self {
            west: self.west.max(other.west),
            south: self.south.max(other.south),
            east: self.east.min(other.east),
            north: self.north.min(other.north),
        };
        (bounds.west < bounds.east && bounds.south < bounds.north).then_some(bounds)
    }

    pub(crate) fn contains(self, longitude_deg: f32, latitude_deg: f32) -> bool {
        longitude_deg >= self.west
            && longitude_deg <= self.east
            && latitude_deg >= self.south
            && latitude_deg <= self.north
    }

    pub(crate) fn intersects_bbox(self, bbox: [f32; 4]) -> bool {
        bbox[2] >= self.west
            && bbox[0] <= self.east
            && bbox[3] >= self.south
            && bbox[1] <= self.north
    }
}
