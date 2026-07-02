//! Map view state + pure gesture classification (miniderecho-spec §13
//! Task 6). The view lives in radar-relative AEQD kilometres (the radar is
//! world (0,0); `ui_core::geo` supplies the lat/lon transform when needed).
//! Because `ViewportRasterOptions` is screen-space, drawing is a single
//! textured quad; during gestures the stale texture is transformed cheaply
//! while a coalesced re-render streams from the worker.

use eframe::egui;
use render2d::ViewportRasterOptions;

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
