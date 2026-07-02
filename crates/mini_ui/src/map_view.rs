//! Map view state + pure gesture classification (miniderecho-spec §13
//! Task 6) and the raster tile basemap under the radar (§11a: pulled
//! forward from M4). The view lives in radar-relative AEQD kilometres (the
//! radar is world (0,0); `ui_core::geo` supplies the lat/lon transform).
//! Because `ViewportRasterOptions` is screen-space, radar drawing is a
//! single textured quad; during gestures the stale texture is transformed
//! cheaply while a coalesced re-render streams from the worker. Tile
//! drawing is the minimal path lifted from BowEcho's `draw_tile_source`
//! (app_ui main.rs): AEQD curved-mesh quads, full-mesh-bounds culling, the
//! antipode guard — north-up/rotation-0, which is what mini is.

use eframe::egui;
use render2d::ViewportRasterOptions;
use ui_core::geo::{aeqd_forward_km, aeqd_inverse_km};
use ui_core::tiles::{self, TileId, TileLayer, TileSource, TileStyle};

/// §13 Task 6: clamp for the zoom range.
pub const MIN_KM_PER_PT: f32 = 0.05;
pub const MAX_KM_PER_PT: f32 = 20.0;

/// Initial scale: ~0.35 km/pt shows the full ~460 km Level-II range ring
/// comfortably inside a laptop window.
pub const DEFAULT_KM_PER_PT: f32 = 0.35;

/// View state relative to the radar (km east/north of it; km per egui
/// point — physical km-per-pixel is derived with `pixels_per_point`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MapView {
    pub center_east_km: f64,
    pub center_north_km: f64,
    pub km_per_pt: f32,
}

impl Default for MapView {
    fn default() -> Self {
        Self {
            center_east_km: 0.0,
            center_north_km: 0.0,
            km_per_pt: DEFAULT_KM_PER_PT,
        }
    }
}

/// Classified map gesture for one frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MapIntent {
    /// Drag delta in points.
    Pan { delta_pts: egui::Vec2 },
    /// Multiplicative zoom (> 1 zooms in) about a screen position.
    Zoom { factor: f32, at: egui::Pos2 },
}

/// Pure gesture classifier (unit-tested): raw per-frame input facts →
/// at most one intent. Pan wins over zoom when both occur in one frame.
///
/// `drag_delta` is the response's drag delta (points; zero when not
/// dragging), `zoom_delta` egui's pinch/ctrl-scroll factor, `scroll_y` the
/// smooth scroll wheel delta, `hover_pos` the pointer position.
pub fn map_intent(
    drag_delta: egui::Vec2,
    zoom_delta: f32,
    scroll_y: f32,
    hover_pos: Option<egui::Pos2>,
    rect: egui::Rect,
) -> Option<MapIntent> {
    if drag_delta != egui::Vec2::ZERO {
        return Some(MapIntent::Pan {
            delta_pts: drag_delta,
        });
    }
    let factor = zoom_delta * scroll_zoom_factor(scroll_y);
    if (factor - 1.0).abs() > f32::EPSILON {
        let at = hover_pos
            .filter(|position| rect.contains(*position))
            .unwrap_or_else(|| rect.center());
        return Some(MapIntent::Zoom { factor, at });
    }
    None
}

/// Wheel → zoom factor; BowEcho's curve at default zoom speed
/// (`scroll_zoom_factor`, app_ui main.rs — pattern-lifted).
fn scroll_zoom_factor(scroll: f32) -> f32 {
    (1.0_f32 + scroll / 600.0).clamp(0.75, 1.35)
}

impl MapView {
    /// Apply one classified intent. Zoom keeps the world point under the
    /// pointer fixed (zoom-to-cursor / pinch-to-centroid).
    pub fn apply(&mut self, intent: MapIntent, rect: egui::Rect) {
        match intent {
            MapIntent::Pan { delta_pts } => {
                self.center_east_km -= f64::from(delta_pts.x * self.km_per_pt);
                self.center_north_km += f64::from(delta_pts.y * self.km_per_pt);
            }
            MapIntent::Zoom { factor, at } => {
                let old = self.km_per_pt;
                let new = (old / factor.max(f32::EPSILON)).clamp(MIN_KM_PER_PT, MAX_KM_PER_PT);
                let offset = at - rect.center();
                self.center_east_km += f64::from(offset.x * (old - new));
                self.center_north_km -= f64::from(offset.y * (old - new));
                self.km_per_pt = new;
            }
        }
    }

    /// Screen position of a lon/lat under this view, projected through the
    /// shared AEQD transform centered on the radar (`radar` = (lat, lon)).
    /// North-up: +north is up, no rotation term.
    pub fn screen_from_lon_lat(
        &self,
        rect: egui::Rect,
        radar: (f64, f64),
        lon: f64,
        lat: f64,
    ) -> egui::Pos2 {
        let (east_km, north_km) = aeqd_forward_km(radar.0, radar.1, lat, lon);
        egui::pos2(
            rect.center().x + ((east_km - self.center_east_km) / f64::from(self.km_per_pt)) as f32,
            rect.center().y
                - ((north_km - self.center_north_km) / f64::from(self.km_per_pt)) as f32,
        )
    }

    /// The view center's geographic position.
    pub fn center_lat_lon(&self, radar: (f64, f64)) -> (f64, f64) {
        aeqd_inverse_km(radar.0, radar.1, self.center_east_km, self.center_north_km)
    }

    /// Geographic bounds of the visible view (the BowEcho pattern: under
    /// AEQD the lat/lon extremes sit on the EDGES, so sample the four
    /// corners plus the four edge midpoints; if a pole is inside the view
    /// radius every longitude is visible).
    pub fn visible_geo_bounds(&self, rect: egui::Rect, radar: (f64, f64)) -> GeoBounds {
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
            let offset = sample - rect.center();
            let east_km = self.center_east_km + f64::from(offset.x * self.km_per_pt);
            let north_km = self.center_north_km - f64::from(offset.y * self.km_per_pt);
            let (lat, lon) = aeqd_inverse_km(radar.0, radar.1, east_km, north_km);
            bounds.west = bounds.west.min(lon as f32);
            bounds.east = bounds.east.max(lon as f32);
            bounds.south = bounds.south.min(lat as f32);
            bounds.north = bounds.north.max(lat as f32);
        }
        const KM_PER_DEG: f64 = 111.32;
        let (center_lat, _) = self.center_lat_lon(radar);
        let view_radius_km = f64::from(rect.width().hypot(rect.height()) * 0.5 * self.km_per_pt);
        if (90.0 - center_lat) * KM_PER_DEG < view_radius_km
            || (90.0 + center_lat) * KM_PER_DEG < view_radius_km
        {
            bounds.west = -180.0;
            bounds.east = 180.0;
        }
        bounds.south = bounds.south.clamp(-85.0, 85.0);
        bounds.north = bounds.north.clamp(-85.0, 85.0);
        bounds
    }

    /// Screen-space raster options for the current view (§13 Task 6):
    /// `rotation_rad = 0.0` for M0, square pixels, physical resolution.
    pub fn raster_options(&self, rect: egui::Rect, pixels_per_point: f32) -> ViewportRasterOptions {
        let ppp = pixels_per_point.max(0.5);
        let width = (rect.width() * ppp).round().max(1.0) as u32;
        let height = (rect.height() * ppp).round().max(1.0) as u32;
        let km_per_px = self.km_per_pt / ppp;
        ViewportRasterOptions {
            width,
            height,
            radar_x_px: width as f32 / 2.0 - self.center_east_km as f32 / km_per_px,
            radar_y_px: height as f32 / 2.0 + self.center_north_km as f32 / km_per_px,
            km_per_px_x: km_per_px,
            km_per_px_y: km_per_px,
            rotation_rad: 0.0,
        }
    }
}

/// Where a texture rendered with `rendered` options lands on screen under
/// the `current` options — the stale-texture transform (pattern-lifted from
/// app_ui's `anchored_radar_texture_rect`): anchor at the radar pixel and
/// scale by the km/px ratio.
pub fn anchored_texture_rect(
    rect: egui::Rect,
    pixels_per_point: f32,
    rendered: ViewportRasterOptions,
    current: ViewportRasterOptions,
) -> egui::Rect {
    let ppp = pixels_per_point.max(0.5);
    let scale_x = positive_ratio(rendered.km_per_px_x, current.km_per_px_x);
    let scale_y = positive_ratio(rendered.km_per_px_y, current.km_per_px_y);
    let left_px = current.radar_x_px - rendered.radar_x_px * scale_x;
    let top_px = current.radar_y_px - rendered.radar_y_px * scale_y;
    egui::Rect::from_min_size(
        egui::pos2(rect.left() + left_px / ppp, rect.top() + top_px / ppp),
        egui::vec2(
            rendered.width as f32 * scale_x / ppp,
            rendered.height as f32 * scale_y / ppp,
        ),
    )
}

fn positive_ratio(numerator: f32, denominator: f32) -> f32 {
    if numerator.is_finite() && denominator.is_finite() && numerator > 0.0 && denominator > 0.0 {
        numerator / denominator
    } else {
        1.0
    }
}

/// Geographic bounds of the visible view (degrees).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GeoBounds {
    pub west: f32,
    pub south: f32,
    pub east: f32,
    pub north: f32,
}

/// Tile debug logging env var (miniderecho's own; BowEcho's
/// `BOWECHO_TILE_DEBUG` is untouched, spec §9).
pub const TILE_DEBUG_ENV: &str = "MINIDERECHO_TILE_DEBUG";

/// Hard cap so degenerate bounds never flood the tile queue (BowEcho's
/// value).
const MAX_TILES_PER_DRAW: u64 = 120;

/// Inclusive tile x/y range covering `bounds` at `zoom`; `None` when the
/// range exceeds the flood cap.
fn tile_range(bounds: GeoBounds, zoom: u8) -> Option<(u32, u32, u32, u32)> {
    let (x0, y0) = tiles::tile_coords(f64::from(bounds.west), f64::from(bounds.north), zoom);
    let (x1, y1) = tiles::tile_coords(f64::from(bounds.east), f64::from(bounds.south), zoom);
    let n = 1u32 << zoom;
    let clamp_tile = |v: f64| (v.floor().max(0.0) as u32).min(n - 1);
    let (tx0, tx1) = (clamp_tile(x0), clamp_tile(x1));
    let (ty0, ty1) = (clamp_tile(y0), clamp_tile(y1));
    let count = u64::from(tx1.saturating_sub(tx0) + 1) * u64::from(ty1.saturating_sub(ty0) + 1);
    (count <= MAX_TILES_PER_DRAW).then_some((tx0, tx1, ty0, ty1))
}

/// Mesh density for a tile spanning `span_px` on screen: more cells the
/// bigger the tile, so AEQD curvature bends the texture instead of
/// shearing a single quad (BowEcho's thresholds).
fn mesh_grid_for_span(span_px: f32) -> u32 {
    if span_px > 512.0 {
        16
    } else if span_px > 160.0 {
        8
    } else {
        4
    }
}

/// Great-circle central angle, for dropping tiles near the AEQD antipode
/// where the projection smears them across the rim.
fn central_angle_deg(lat_a: f64, lon_a: f64, lat_b: f64, lon_b: f64) -> f64 {
    let (la, lb) = (lat_a.to_radians(), lat_b.to_radians());
    let dlon = (lon_b - lon_a).to_radians();
    (la.sin() * lb.sin() + la.cos() * lb.cos() * dlon.cos())
        .clamp(-1.0, 1.0)
        .acos()
        .to_degrees()
}

/// Draw the raster tile basemap under the radar (spec §11a) — the minimal
/// path lifted from BowEcho's `draw_tile_basemap`/`draw_tile_source`.
/// `DarkVector` draws nothing (the vector basemap crate is M4; the dark
/// fill underneath serves until then).
#[allow(clippy::too_many_arguments)]
pub fn draw_tile_basemap(
    layer: &mut TileLayer,
    painter: &egui::Painter,
    rect: egui::Rect,
    view: &MapView,
    radar: (f64, f64),
    style: TileStyle,
    pixels_per_point: f32,
) {
    let debug = std::env::var_os(TILE_DEBUG_ENV).is_some();
    if style == TileStyle::DarkVector {
        return;
    }
    let source = TileSource::Basemap(style);
    let (center_lat, center_lon) = view.center_lat_lon(radar);
    let zoom = tiles::zoom_for_km_per_px(view.km_per_pt, center_lat as f32, pixels_per_point);
    let bounds = view.visible_geo_bounds(rect, radar);
    let Some((tx0, tx1, ty0, ty1)) = tile_range(bounds, zoom) else {
        if debug {
            eprintln!("TILES: over tile cap, skipping");
        }
        return;
    };
    if debug {
        eprintln!(
            "TILES: zoom {zoom} x {tx0}..{tx1} y {ty0}..{ty1} bounds W{:.2} E{:.2} S{:.2} N{:.2}",
            bounds.west, bounds.east, bounds.south, bounds.north
        );
    }
    for ty in ty0..=ty1 {
        for tx in tx0..=tx1 {
            let tile = TileId { zoom, x: tx, y: ty };
            // Far-hemisphere guard: a tile near the antipode projects to
            // an enormous smeared ring — raster bows out there.
            let (tile_center_lon, tile_center_lat) =
                tiles::tile_corner_lon_lat(f64::from(tx) + 0.5, f64::from(ty) + 0.5, zoom);
            if central_angle_deg(center_lat, center_lon, tile_center_lat, tile_center_lon) > 140.0 {
                continue;
            }
            let Some(texture_id) = layer
                .texture_source(&source, tile)
                .map(|texture| texture.id())
            else {
                layer.request_source(source.clone(), tile);
                continue;
            };
            // Coarse 4-corner probe sizes the tile on screen and picks the
            // mesh density.
            let probe = [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)].map(|(fx, fy)| {
                let (lon, lat) =
                    tiles::tile_corner_lon_lat(f64::from(tx) + fx, f64::from(ty) + fy, zoom);
                view.screen_from_lon_lat(rect, radar, lon, lat)
            });
            let span_px = {
                let probe_bounds = egui::Rect::from_points(&probe);
                probe_bounds.width().max(probe_bounds.height())
            };
            let grid = mesh_grid_for_span(span_px);
            let mut vertices = Vec::with_capacity(((grid + 1) * (grid + 1)) as usize);
            let mut mesh_bounds: Option<egui::Rect> = None;
            for gy in 0..=grid {
                for gx in 0..=grid {
                    let (fx, fy) = (
                        f64::from(gx) / f64::from(grid),
                        f64::from(gy) / f64::from(grid),
                    );
                    let (lon, lat) =
                        tiles::tile_corner_lon_lat(f64::from(tx) + fx, f64::from(ty) + fy, zoom);
                    let pos = view.screen_from_lon_lat(rect, radar, lon, lat);
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
                continue; // beyond the projection's validity
            }
            // Cull on the FULL mesh bounds — curved edges bow outside the
            // corner hull, so 4-corner culling drops live tiles.
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
                    color: egui::Color32::WHITE,
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
    if let Some(attribution) = style.attribution() {
        painter.text(
            egui::pos2(rect.left() + 6.0, rect.bottom() - 4.0),
            egui::Align2::LEFT_BOTTOM,
            attribution,
            egui::FontId::proportional(9.0),
            egui::Color32::from_rgba_unmultiplied(230, 234, 238, 150),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Rect, pos2, vec2};

    fn rect() -> Rect {
        Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0))
    }

    #[test]
    fn no_input_classifies_to_nothing() {
        assert_eq!(
            map_intent(egui::Vec2::ZERO, 1.0, 0.0, Some(pos2(10.0, 10.0)), rect()),
            None
        );
    }

    #[test]
    fn drag_classifies_as_pan_and_wins_over_zoom() {
        let intent = map_intent(vec2(5.0, -3.0), 1.2, 120.0, Some(pos2(10.0, 10.0)), rect());
        assert_eq!(
            intent,
            Some(MapIntent::Pan {
                delta_pts: vec2(5.0, -3.0)
            })
        );
    }

    #[test]
    fn scroll_classifies_as_zoom_at_the_pointer() {
        let at = pos2(100.0, 50.0);
        match map_intent(egui::Vec2::ZERO, 1.0, 120.0, Some(at), rect()) {
            Some(MapIntent::Zoom { factor, at: got }) => {
                assert!(factor > 1.0, "wheel-up zooms in, factor {factor}");
                assert_eq!(got, at);
            }
            other => panic!("want zoom, got {other:?}"),
        }
    }

    #[test]
    fn pinch_zoom_delta_classifies_without_scroll() {
        match map_intent(egui::Vec2::ZERO, 1.25, 0.0, Some(pos2(1.0, 1.0)), rect()) {
            Some(MapIntent::Zoom { factor, .. }) => assert!((factor - 1.25).abs() < 1e-6),
            other => panic!("want zoom, got {other:?}"),
        }
    }

    #[test]
    fn zoom_outside_the_rect_anchors_at_center() {
        match map_intent(
            egui::Vec2::ZERO,
            1.0,
            120.0,
            Some(pos2(-50.0, -50.0)),
            rect(),
        ) {
            Some(MapIntent::Zoom { at, .. }) => assert_eq!(at, rect().center()),
            other => panic!("want zoom, got {other:?}"),
        }
    }

    #[test]
    fn pan_shifts_center_against_the_drag() {
        let mut view = MapView {
            center_east_km: 0.0,
            center_north_km: 0.0,
            km_per_pt: 1.0,
        };
        // Dragging the map right/down reveals terrain west/north of center.
        view.apply(
            MapIntent::Pan {
                delta_pts: vec2(10.0, 20.0),
            },
            rect(),
        );
        assert_eq!(view.center_east_km, -10.0);
        assert_eq!(view.center_north_km, 20.0);
    }

    #[test]
    fn zoom_about_a_point_keeps_its_world_position() {
        let mut view = MapView {
            center_east_km: 12.0,
            center_north_km: -7.0,
            km_per_pt: 1.0,
        };
        let at = pos2(600.0, 150.0); // off-center
        let offset = at - rect().center();
        let world_east = view.center_east_km + f64::from(offset.x * view.km_per_pt);
        let world_north = view.center_north_km - f64::from(offset.y * view.km_per_pt);

        view.apply(MapIntent::Zoom { factor: 2.0, at }, rect());

        assert!((view.km_per_pt - 0.5).abs() < 1e-6);
        let east_after = view.center_east_km + f64::from(offset.x * view.km_per_pt);
        let north_after = view.center_north_km - f64::from(offset.y * view.km_per_pt);
        assert!(
            (east_after - world_east).abs() < 1e-4,
            "{east_after} vs {world_east}"
        );
        assert!(
            (north_after - world_north).abs() < 1e-4,
            "{north_after} vs {world_north}"
        );
    }

    #[test]
    fn zoom_clamps_to_the_km_per_pt_range() {
        let mut view = MapView::default();
        for _ in 0..200 {
            view.apply(
                MapIntent::Zoom {
                    factor: 2.0,
                    at: rect().center(),
                },
                rect(),
            );
        }
        assert_eq!(view.km_per_pt, MIN_KM_PER_PT);
        for _ in 0..200 {
            view.apply(
                MapIntent::Zoom {
                    factor: 0.5,
                    at: rect().center(),
                },
                rect(),
            );
        }
        assert_eq!(view.km_per_pt, MAX_KM_PER_PT);
    }

    #[test]
    fn raster_options_center_the_view_and_flip_north_up() {
        let view = MapView {
            center_east_km: 100.0,
            center_north_km: 50.0,
            km_per_pt: 1.0,
        };
        let options = view.raster_options(rect(), 2.0);
        assert_eq!((options.width, options.height), (1600, 1200));
        assert!((options.km_per_px_x - 0.5).abs() < 1e-6);
        // Radar (world 0,0) sits west and south of the view center:
        // x: 800 - 100/0.5 = 600; y: 600 + 50/0.5 = 700.
        assert!((options.radar_x_px - 600.0).abs() < 1e-3);
        assert!((options.radar_y_px - 700.0).abs() < 1e-3);
        assert_eq!(options.rotation_rad, 0.0);
    }

    #[test]
    fn identical_options_anchor_the_texture_to_the_full_rect() {
        let view = MapView::default();
        let options = view.raster_options(rect(), 1.0);
        let anchored = anchored_texture_rect(rect(), 1.0, options, options);
        assert!((anchored.left() - rect().left()).abs() < 0.5);
        assert!((anchored.top() - rect().top()).abs() < 0.5);
        assert!((anchored.width() - rect().width()).abs() < 1.0);
        assert!((anchored.height() - rect().height()).abs() < 1.0);
    }

    /// KTLX's pad, the projection center for the geo tests.
    const RADAR: (f64, f64) = (35.3331, -97.2777);

    #[test]
    fn screen_projection_is_north_up_about_the_radar() {
        let view = MapView::default();
        // The radar itself sits at the view center (center = (0,0) km).
        let at_radar = view.screen_from_lon_lat(rect(), RADAR, RADAR.1, RADAR.0);
        assert!((at_radar - rect().center()).length() < 0.5, "{at_radar:?}");
        // One degree north maps straight up by 111.32 km / km_per_pt.
        let north = view.screen_from_lon_lat(rect(), RADAR, RADAR.1, RADAR.0 + 1.0);
        assert!((north.x - rect().center().x).abs() < 1.0, "{north:?}");
        let expected_up = 111.32 / DEFAULT_KM_PER_PT;
        assert!(
            (rect().center().y - north.y - expected_up).abs() < 2.0,
            "{north:?}"
        );
        // East maps right.
        let east = view.screen_from_lon_lat(rect(), RADAR, RADAR.1 + 1.0, RADAR.0);
        assert!(east.x > rect().center().x + 100.0, "{east:?}");
        // Panning the view shifts the projection with it.
        let panned = MapView {
            center_east_km: 50.0,
            center_north_km: 0.0,
            km_per_pt: 1.0,
        };
        let shifted = panned.screen_from_lon_lat(rect(), RADAR, RADAR.1, RADAR.0);
        assert!((shifted.x - (rect().center().x - 50.0)).abs() < 0.5);
    }

    #[test]
    fn visible_geo_bounds_bracket_the_radar_and_handle_the_pole() {
        let view = MapView::default();
        let bounds = view.visible_geo_bounds(rect(), RADAR);
        assert!(bounds.west < RADAR.1 as f32 && (RADAR.1 as f32) < bounds.east);
        assert!(bounds.south < RADAR.0 as f32 && (RADAR.0 as f32) < bounds.north);
        // Roughly symmetric about a centered radar.
        assert!(
            ((bounds.north - RADAR.0 as f32) - (RADAR.0 as f32 - bounds.south)).abs() < 0.2,
            "{bounds:?}"
        );

        // A pole inside the view radius opens every longitude.
        let planet = MapView {
            center_east_km: 0.0,
            center_north_km: 0.0,
            km_per_pt: MAX_KM_PER_PT,
        };
        let bounds = planet.visible_geo_bounds(rect(), (80.0, -100.0));
        assert_eq!((bounds.west, bounds.east), (-180.0, 180.0));
        assert!(bounds.north <= 85.0 && bounds.south >= -85.0);
    }

    #[test]
    fn tile_range_covers_the_bounds_and_caps_floods() {
        let bounds = GeoBounds {
            west: -98.0,
            south: 34.5,
            east: -96.5,
            north: 36.0,
        };
        let (tx0, tx1, ty0, ty1) = tile_range(bounds, 8).expect("county view fits the cap");
        assert!(tx0 <= tx1 && ty0 <= ty1);
        // The range brackets the bounds' corner tiles.
        let (x, y) = tiles::tile_coords(-97.2777, 35.3331, 8);
        assert!((tx0..=tx1).contains(&(x as u32)) && (ty0..=ty1).contains(&(y as u32)));
        // Whole-world bounds at a deep zoom exceed the flood cap.
        let world = GeoBounds {
            west: -180.0,
            south: -85.0,
            east: 180.0,
            north: 85.0,
        };
        assert_eq!(tile_range(world, 12), None);
        // ... but fit comfortably at planet zooms.
        assert!(tile_range(world, 2).is_some());
    }

    #[test]
    fn mesh_density_scales_with_on_screen_span() {
        assert_eq!(mesh_grid_for_span(80.0), 4);
        assert_eq!(mesh_grid_for_span(300.0), 8);
        assert_eq!(mesh_grid_for_span(1000.0), 16);
    }

    #[test]
    fn antipode_angle_flags_far_hemisphere_tiles() {
        assert!(central_angle_deg(35.0, -97.0, 35.0, -97.0) < 1e-6);
        // The antipode of Oklahoma sits in the Indian Ocean: > 140°.
        assert!(central_angle_deg(35.0, -97.0, -35.0, 83.0) > 170.0);
        assert!(central_angle_deg(35.0, -97.0, 45.0, -100.0) < 20.0);
    }

    #[test]
    fn zooming_in_scales_the_stale_texture_up_about_the_radar() {
        let rendered_view = MapView {
            center_east_km: 0.0,
            center_north_km: 0.0,
            km_per_pt: 1.0,
        };
        let mut current_view = rendered_view;
        current_view.apply(
            MapIntent::Zoom {
                factor: 2.0,
                at: rect().center(),
            },
            rect(),
        );
        let rendered = rendered_view.raster_options(rect(), 1.0);
        let current = current_view.raster_options(rect(), 1.0);
        let anchored = anchored_texture_rect(rect(), 1.0, rendered, current);
        // 2x zoom-in doubles the stale quad about the (centered) radar.
        assert!(
            (anchored.width() - 1600.0).abs() < 1.0,
            "{}",
            anchored.width()
        );
        assert!((anchored.center().x - rect().center().x).abs() < 1.0);
        assert!((anchored.center().y - rect().center().y).abs() < 1.0);
    }
}
