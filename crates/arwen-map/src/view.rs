// SPDX-License-Identifier: Apache-2.0
//
// Screen projection + raster-tile mesh warp copied as pattern with
// attribution from BowEcho crates/app_ui/src/map_paint.rs @ 6dfcb9f
// (azimuthal-equidistant screen projection, GeoBounds edge sampling,
// far-hemisphere tile guard, curvature-adaptive tile meshes). The AEQD
// forward/inverse pair itself is ui_core::geo (git dep).

//! The map camera: center + scale, screen↔geo projection, pan/zoom, and
//! the basemap painters (raster tile layer, vector lines).

use eframe::egui;
use ui_core::geo::{aeqd_forward_km, aeqd_inverse_km};
use ui_core::tiles::{self, TileLayer, TileSource};

use crate::basemap_data::BasemapLine;
use crate::geo::normalize_lon;

pub const MIN_SCALE: f32 = 2.0;
pub const MAX_SCALE: f32 = 4000.0;
const KM_PER_DEG: f32 = 111.32;

/// Camera over the world: AEQD about (center_lat, center_lon), `scale` in
/// screen pixels per degree of latitude.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MapView {
    pub center_lat: f32,
    pub center_lon: f32,
    pub scale: f32,
}

impl Default for MapView {
    fn default() -> Self {
        // Continental overview over the CONUS: the "point at the world"
        // empty state.
        Self {
            center_lat: 38.5,
            center_lon: -97.0,
            scale: 11.0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct GeoBounds {
    pub west: f32,
    pub south: f32,
    pub east: f32,
    pub north: f32,
}

impl GeoBounds {
    pub fn contains(self, lon: f32, lat: f32) -> bool {
        lon >= self.west && lon <= self.east && lat >= self.south && lat <= self.north
    }

    pub fn intersects_bbox(self, bbox: [f32; 4]) -> bool {
        bbox[2] >= self.west
            && bbox[0] <= self.east
            && bbox[3] >= self.south
            && bbox[1] <= self.north
    }

    pub fn expand(self, degrees: f32) -> Self {
        Self {
            west: self.west - degrees,
            south: self.south - degrees,
            east: self.east + degrees,
            north: self.north + degrees,
        }
    }
}

impl MapView {
    pub fn lon_lat_to_screen(&self, rect: egui::Rect, lon: f32, lat: f32) -> egui::Pos2 {
        let (east_km, north_km) = aeqd_forward_km(
            self.center_lat as f64,
            self.center_lon as f64,
            lat as f64,
            lon as f64,
        );
        let px_per_km = self.scale / KM_PER_DEG;
        egui::pos2(
            rect.center().x + east_km as f32 * px_per_km,
            rect.center().y - north_km as f32 * px_per_km,
        )
    }

    pub fn screen_to_lon_lat(&self, rect: egui::Rect, position: egui::Pos2) -> (f32, f32) {
        let km_per_px = KM_PER_DEG / self.scale;
        let east_km = (position.x - rect.center().x) * km_per_px;
        let north_km = (rect.center().y - position.y) * km_per_px;
        let (lat, lon) = aeqd_inverse_km(
            self.center_lat as f64,
            self.center_lon as f64,
            east_km as f64,
            north_km as f64,
        );
        (normalize_lon(lon as f32), lat as f32)
    }

    /// Shift the camera by a screen-pixel delta (positive x = look east).
    pub fn pan_by_pixels(&mut self, delta: egui::Vec2) {
        let km_per_px = KM_PER_DEG / self.scale;
        let (lat, lon) = aeqd_inverse_km(
            self.center_lat as f64,
            self.center_lon as f64,
            (delta.x * km_per_px) as f64,
            (-delta.y * km_per_px) as f64,
        );
        self.center_lat = lat as f32;
        self.center_lon = lon as f32;
        self.clamp_center();
    }

    /// Zoom by `factor`, keeping the geo point under `cursor` fixed. The
    /// pan is refined iteratively because AEQD is nonlinear away from the
    /// center — one linear correction leaves visible drift at continental
    /// zoom levels.
    pub fn zoom_about(&mut self, rect: egui::Rect, cursor: egui::Pos2, factor: f32) {
        let (lon, lat) = self.screen_to_lon_lat(rect, cursor);
        self.scale = (self.scale * factor).clamp(MIN_SCALE, MAX_SCALE);
        for _ in 0..8 {
            let now = self.lon_lat_to_screen(rect, lon, lat);
            let delta = now - cursor;
            if delta.length() < 0.25 {
                break;
            }
            self.pan_by_pixels(delta);
        }
    }

    pub fn clamp_center(&mut self) {
        self.center_lon = normalize_lon(self.center_lon);
        self.center_lat = self.center_lat.clamp(-85.0, 85.0);
    }

    /// Lat/lon extremes of the view. Under AEQD they sit on edges, not
    /// corners — sample corners + edge midpoints (BowEcho F1 finding).
    pub fn visible_geo_bounds(&self, rect: egui::Rect) -> GeoBounds {
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
        let km_per_px = KM_PER_DEG / self.scale;
        let view_radius_km = (rect.width().hypot(rect.height()) * 0.5 * km_per_px) as f64;
        let north_pole_km = ((90.0 - self.center_lat) * KM_PER_DEG) as f64;
        let south_pole_km = ((90.0 + self.center_lat) * KM_PER_DEG) as f64;
        if north_pole_km < view_radius_km || south_pole_km < view_radius_km {
            bounds.west = -180.0;
            bounds.east = 180.0;
        }
        bounds.south = bounds.south.clamp(-85.0, 85.0);
        bounds.north = bounds.north.clamp(-85.0, 85.0);
        bounds
    }

    /// Project one (lon, lat) polyline to screen points.
    pub fn project_polyline(
        &self,
        rect: egui::Rect,
        points: impl IntoIterator<Item = (f64, f64)>,
    ) -> Vec<egui::Pos2> {
        points
            .into_iter()
            .map(|(lon, lat)| self.lon_lat_to_screen(rect, lon as f32, lat as f32))
            .collect()
    }

    /// Paint one raster tile source as curvature-adaptive textured meshes.
    pub fn paint_raster_tiles(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        layer: &mut TileLayer,
        source: &TileSource,
        tint: egui::Color32,
    ) {
        let pixels_per_point = painter.ctx().pixels_per_point().max(0.5);
        let km_per_px = KM_PER_DEG / self.scale;
        let zoom = tiles::zoom_for_km_per_px(km_per_px, self.center_lat, pixels_per_point);
        let bounds = self.visible_geo_bounds(rect);
        let (x0, y0) = tiles::tile_coords(bounds.west as f64, bounds.north as f64, zoom);
        let (x1, y1) = tiles::tile_coords(bounds.east as f64, bounds.south as f64, zoom);
        let n = 1u32 << zoom;
        let clamp_tile = |v: f64| (v.floor().max(0.0) as u32).min(n - 1);
        let (tx0, tx1) = (clamp_tile(x0), clamp_tile(x1));
        let (ty0, ty1) = (clamp_tile(y0), clamp_tile(y1));
        // Hard cap so degenerate bounds never flood the queue.
        if (tx1.saturating_sub(tx0) + 1) as u64 * (ty1.saturating_sub(ty0) + 1) as u64 > 120 {
            return;
        }
        fn central_angle_deg(lat_a: f32, lon_a: f32, lat_b: f32, lon_b: f32) -> f32 {
            let (la, lb) = (lat_a.to_radians(), lat_b.to_radians());
            let dlon = (lon_b - lon_a).to_radians();
            (la.sin() * lb.sin() + la.cos() * lb.cos() * dlon.cos())
                .clamp(-1.0, 1.0)
                .acos()
                .to_degrees()
        }
        for ty in ty0..=ty1 {
            for tx in tx0..=tx1 {
                let tile = tiles::TileId { zoom, x: tx, y: ty };
                // Far-hemisphere guard: antipodal tiles smear across the
                // AEQD rim.
                let (center_lon, center_lat) =
                    tiles::tile_corner_lon_lat(tx as f64 + 0.5, ty as f64 + 0.5, zoom);
                if central_angle_deg(
                    self.center_lat,
                    self.center_lon,
                    center_lat as f32,
                    center_lon as f32,
                ) > 140.0
                {
                    continue;
                }
                let Some(texture_id) = layer.texture_source(source, tile).map(|t| t.id()) else {
                    layer.request_source(source.clone(), tile);
                    continue;
                };
                // Mesh density scales with on-screen size so curvature
                // bends the texture instead of shearing one quad.
                let probe = [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)].map(|(fx, fy)| {
                    let (lon, lat) =
                        tiles::tile_corner_lon_lat(tx as f64 + fx, ty as f64 + fy, zoom);
                    self.lon_lat_to_screen(rect, lon as f32, lat as f32)
                });
                let span_px = {
                    let probe_bounds = egui::Rect::from_points(&probe);
                    probe_bounds.width().max(probe_bounds.height())
                };
                let grid: u32 = if span_px > 512.0 {
                    16
                } else if span_px > 160.0 {
                    8
                } else {
                    4
                };
                let mut vertices = Vec::with_capacity(((grid + 1) * (grid + 1)) as usize);
                let mut mesh_bounds: Option<egui::Rect> = None;
                for gy in 0..=grid {
                    for gx in 0..=grid {
                        let (fx, fy) = (
                            f64::from(gx) / f64::from(grid),
                            f64::from(gy) / f64::from(grid),
                        );
                        let (lon, lat) =
                            tiles::tile_corner_lon_lat(tx as f64 + fx, ty as f64 + fy, zoom);
                        let pos = self.lon_lat_to_screen(rect, lon as f32, lat as f32);
                        mesh_bounds = Some(match mesh_bounds {
                            Some(bounds) => bounds.union(egui::Rect::from_min_max(pos, pos)),
                            None => egui::Rect::from_min_max(pos, pos),
                        });
                        vertices.push((pos, egui::pos2(fx as f32, fy as f32)));
                    }
                }
                if vertices
                    .iter()
                    .any(|(pos, _)| !pos.x.is_finite() || !pos.y.is_finite())
                {
                    continue;
                }
                let Some(mesh_bounds) = mesh_bounds else {
                    continue;
                };
                if !rect.intersects(mesh_bounds) {
                    continue;
                }
                let mut mesh = egui::epaint::Mesh::with_texture(texture_id);
                for (pos, uv) in &vertices {
                    mesh.vertices.push(egui::epaint::Vertex {
                        pos: *pos,
                        uv: *uv,
                        color: tint,
                    });
                }
                let stride = grid + 1;
                for gy in 0..grid {
                    for gx in 0..grid {
                        let i = gy * stride + gx;
                        mesh.indices.extend_from_slice(&[
                            i,
                            i + 1,
                            i + stride,
                            i + 1,
                            i + stride + 1,
                            i + stride,
                        ]);
                    }
                }
                painter.add(egui::Shape::mesh(mesh));
            }
        }
    }

    /// Paint a vector line table (states, countries) with bbox culling.
    pub fn paint_basemap_lines(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        lines: &[BasemapLine],
        stroke: egui::Stroke,
    ) {
        let bounds = self.visible_geo_bounds(rect).expand(0.25);
        for line in lines {
            if !bounds.intersects_bbox(line.bbox) {
                continue;
            }
            let points: Vec<egui::Pos2> = line
                .points
                .iter()
                .map(|&(lon, lat)| self.lon_lat_to_screen(rect, lon, lat))
                .collect();
            if points.len() >= 2 {
                painter.add(egui::Shape::line(points, stroke));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect() -> egui::Rect {
        egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200.0, 800.0))
    }

    #[test]
    fn screen_projection_round_trips() {
        let view = MapView::default();
        let position = view.lon_lat_to_screen(rect(), -95.2, 36.4);
        let (lon, lat) = view.screen_to_lon_lat(rect(), position);
        assert!((lon + 95.2).abs() < 1e-4, "{lon}");
        assert!((lat - 36.4).abs() < 1e-4, "{lat}");
    }

    #[test]
    fn center_projects_to_rect_center() {
        let view = MapView::default();
        let position = view.lon_lat_to_screen(rect(), view.center_lon, view.center_lat);
        assert!((position - rect().center()).length() < 1e-3);
    }

    #[test]
    fn zoom_about_keeps_the_cursor_point_fixed() {
        let mut view = MapView::default();
        let cursor = egui::pos2(900.0, 200.0);
        let (lon, lat) = view.screen_to_lon_lat(rect(), cursor);
        view.zoom_about(rect(), cursor, 2.0);
        let after = view.lon_lat_to_screen(rect(), lon, lat);
        assert!(
            (after - cursor).length() < 0.5,
            "anchor drifted {:?} -> {after:?}",
            cursor
        );
        assert!((view.scale - 22.0).abs() < 1e-6);
    }

    #[test]
    fn pan_moves_the_center_the_right_way() {
        let mut view = MapView::default();
        let before_lon = view.center_lon;
        view.pan_by_pixels(egui::vec2(100.0, 0.0)); // look east
        assert!(view.center_lon > before_lon);
        let before_lat = view.center_lat;
        view.pan_by_pixels(egui::vec2(0.0, 100.0)); // positive y = look south
        assert!(view.center_lat < before_lat);
    }

    #[test]
    fn visible_bounds_contain_the_center_and_respect_scale() {
        let view = MapView::default();
        let bounds = view.visible_geo_bounds(rect());
        assert!(bounds.contains(view.center_lon, view.center_lat));
        assert!(bounds.west < view.center_lon && bounds.east > view.center_lon);
        // Zooming in shrinks the visible span.
        let zoomed = MapView {
            scale: 200.0,
            ..view
        };
        let small = zoomed.visible_geo_bounds(rect());
        assert!(small.east - small.west < bounds.east - bounds.west);
    }
}
