//! Geo-anchored map annotations: the GR2Analyst-style "point at the thing"
//! tools (crosshair marker, box, arrow, freehand sketch).
//!
//! Anchors are stored as lon/lat so shapes stay glued to the weather while
//! the user pans/zooms and while the history loop animates. Annotations are
//! painted into the map canvas (after hazard overlays, before hover UI), so
//! screenshots and loop recordings include them automatically.

use eframe::egui;

use crate::ViewerApp;

// Default annotation style. Grouped here so a future style registry
// (docs/customization-spec.md style work) can lift them wholesale.
pub(crate) const ANNOTATION_COLOR: egui::Color32 = egui::Color32::from_rgb(230, 40, 40);
pub(crate) const ANNOTATION_STROKE_WIDTH: f32 = 2.5;
/// Subtle dark halo painted underneath so red strokes stay visible on top of
/// any echo color.
pub(crate) const ANNOTATION_HALO_COLOR: egui::Color32 =
    egui::Color32::from_rgba_premultiplied(8, 10, 14, 150);
pub(crate) const ANNOTATION_HALO_WIDTH: f32 = ANNOTATION_STROKE_WIDTH + 3.0;
const CROSSHAIR_RING_RADIUS_PX: f32 = 10.0;
const CROSSHAIR_TICK_INNER_PX: f32 = 4.0;
const CROSSHAIR_TICK_OUTER_PX: f32 = 17.0;
const ARROW_HEAD_LENGTH_PX: f32 = 14.0;
const ARROW_HEAD_ANGLE_DEG: f32 = 26.0;
const FREEHAND_MIN_POINT_SPACING_PX: f32 = 4.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct GeoPoint {
    pub(crate) lon: f32,
    pub(crate) lat: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShapeKind {
    Crosshair,
    Box,
    Arrow,
    Freehand,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Annotation {
    Crosshair(GeoPoint),
    Box { a: GeoPoint, b: GeoPoint },
    Arrow { tail: GeoPoint, head: GeoPoint },
    Freehand(Vec<GeoPoint>),
}

pub(crate) struct AnnotationState {
    pub(crate) shapes: Vec<Annotation>,
    /// `Some` while annotate mode is active.
    pub(crate) active_tool: Option<ShapeKind>,
    /// Tool restored when annotate mode is toggled back on.
    pub(crate) last_tool: ShapeKind,
    /// In-progress drag, drawn live and committed on release.
    pub(crate) draft: Option<Annotation>,
}

impl Default for AnnotationState {
    fn default() -> Self {
        Self {
            shapes: Vec::new(),
            active_tool: None,
            last_tool: ShapeKind::Crosshair,
            draft: None,
        }
    }
}

impl ViewerApp {
    pub(crate) fn annotate_top_bar_ui(&mut self, ui: &mut egui::Ui) {
        let active = self.annotations.active_tool.is_some();
        if ui
            .add_sized(
                egui::vec2(72.0, crate::PANEL_BUTTON_HEIGHT),
                egui::Button::selectable(active, "Annotate"),
            )
            .on_hover_text(
                "Draw geo-anchored pointers on the map (crosshair, box, arrow, \
                 freehand). They follow pan/zoom, animate with the loop, and \
                 appear in screenshots/recordings. Esc exits draw mode.",
            )
            .clicked()
        {
            self.annotations.active_tool = if active {
                None
            } else {
                Some(self.annotations.last_tool)
            };
            self.annotations.draft = None;
        }

        if let Some(current) = self.annotations.active_tool {
            for (kind, label, tip) in [
                (ShapeKind::Crosshair, "Mark", "Click to drop a crosshair"),
                (ShapeKind::Box, "Box", "Drag two corners to box an area"),
                (ShapeKind::Arrow, "Arrow", "Drag from tail to head"),
                (ShapeKind::Freehand, "Draw", "Drag to sketch freehand"),
            ] {
                if ui
                    .add_sized(
                        egui::vec2(52.0, crate::PANEL_BUTTON_HEIGHT),
                        egui::Button::selectable(current == kind, label),
                    )
                    .on_hover_text(tip)
                    .clicked()
                {
                    self.annotations.active_tool = Some(kind);
                    self.annotations.last_tool = kind;
                    self.annotations.draft = None;
                }
            }
        }

        if (!self.annotations.shapes.is_empty() || self.annotations.draft.is_some())
            && crate::fixed_action_button(ui, "Clear", 50.0)
                .on_hover_text("Remove all annotations")
                .clicked()
        {
            self.annotations.shapes.clear();
            self.annotations.draft = None;
        }
    }

    /// Routes map pointer input to annotation drawing while annotate mode is
    /// active. Returns true when the pointer is owned by annotations so the
    /// map canvas skips pan and click-to-select handling.
    pub(crate) fn handle_annotation_input(
        &mut self,
        rect: egui::Rect,
        response: &egui::Response,
        ui: &egui::Ui,
    ) -> bool {
        if self.annotations.active_tool.is_some()
            && ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            self.annotations.active_tool = None;
            self.annotations.draft = None;
        }
        let Some(tool) = self.annotations.active_tool else {
            return false;
        };
        if response.hovered() {
            ui.ctx()
                .output_mut(|output| output.cursor_icon = egui::CursorIcon::Crosshair);
        }

        let pointer_geo = response.interact_pointer_pos().map(|position| {
            let (lon, lat) = self.screen_to_lon_lat(rect, position);
            GeoPoint { lon, lat }
        });

        if response.clicked()
            && tool == ShapeKind::Crosshair
            && let Some(geo) = pointer_geo
        {
            self.annotations.shapes.push(Annotation::Crosshair(geo));
        }

        if response.drag_started()
            && tool != ShapeKind::Crosshair
            && let Some(geo) = pointer_geo
        {
            self.annotations.draft = Some(start_draft(tool, geo));
        } else if response.dragged()
            && let Some(geo) = pointer_geo
        {
            let spacing_ok = match &self.annotations.draft {
                Some(Annotation::Freehand(points)) => points.last().is_none_or(|last| {
                    let last_screen = self.lon_lat_to_screen(rect, last.lon, last.lat);
                    let current_screen = self.lon_lat_to_screen(rect, geo.lon, geo.lat);
                    last_screen.distance(current_screen) >= FREEHAND_MIN_POINT_SPACING_PX
                }),
                _ => true,
            };
            if let Some(draft) = &mut self.annotations.draft {
                update_draft(draft, geo, spacing_ok);
            }
        }

        if response.drag_stopped()
            && let Some(draft) = self.annotations.draft.take()
            && draft_is_valid(&draft)
        {
            self.annotations.shapes.push(draft);
        }

        true
    }

    pub(crate) fn draw_map_annotations(&self, painter: &egui::Painter, rect: egui::Rect) {
        if self.annotations.shapes.is_empty() && self.annotations.draft.is_none() {
            return;
        }
        let project = |point: GeoPoint| self.lon_lat_to_screen(rect, point.lon, point.lat);
        draw_annotations(
            painter,
            &self.annotations.shapes,
            self.annotations.draft.as_ref(),
            &project,
        );
    }
}

pub(crate) fn start_draft(tool: ShapeKind, geo: GeoPoint) -> Annotation {
    match tool {
        ShapeKind::Crosshair => Annotation::Crosshair(geo),
        ShapeKind::Box => Annotation::Box { a: geo, b: geo },
        ShapeKind::Arrow => Annotation::Arrow {
            tail: geo,
            head: geo,
        },
        ShapeKind::Freehand => Annotation::Freehand(vec![geo]),
    }
}

pub(crate) fn update_draft(draft: &mut Annotation, geo: GeoPoint, spacing_ok: bool) {
    match draft {
        Annotation::Crosshair(point) => *point = geo,
        Annotation::Box { b, .. } => *b = geo,
        Annotation::Arrow { head, .. } => *head = geo,
        Annotation::Freehand(points) => {
            if spacing_ok {
                points.push(geo);
            }
        }
    }
}

pub(crate) fn draft_is_valid(draft: &Annotation) -> bool {
    match draft {
        Annotation::Crosshair(_) => true,
        Annotation::Box { a, b } => a != b,
        Annotation::Arrow { tail, head } => tail != head,
        Annotation::Freehand(points) => points.len() >= 2,
    }
}

/// Corners of the two-corner box in draw order: a lat/lon-aligned
/// quadrilateral. Under the map's AEQD projection the projected edges are
/// very slightly curved; drawing straight segments between the projected
/// corners is indistinguishable at storm scale and keeps the box anchored.
pub(crate) fn box_corners(a: GeoPoint, b: GeoPoint) -> [GeoPoint; 4] {
    [
        a,
        GeoPoint {
            lon: b.lon,
            lat: a.lat,
        },
        b,
        GeoPoint {
            lon: a.lon,
            lat: b.lat,
        },
    ]
}

/// The two barb endpoints of an arrow head at `head`, swept back toward
/// `tail` by `angle_deg` on each side of the shaft.
pub(crate) fn arrow_head_points(
    tail: egui::Pos2,
    head: egui::Pos2,
    length: f32,
    angle_deg: f32,
) -> [egui::Pos2; 2] {
    let shaft = head - tail;
    if shaft.length_sq() <= f32::EPSILON {
        return [head, head];
    }
    let back = -shaft.normalized();
    let rotation = egui::emath::Rot2::from_angle(angle_deg.to_radians());
    [
        head + length * (rotation * back),
        head + length * (rotation.inverse() * back),
    ]
}

pub(crate) fn draw_annotations<F: Fn(GeoPoint) -> egui::Pos2>(
    painter: &egui::Painter,
    shapes: &[Annotation],
    draft: Option<&Annotation>,
    project: &F,
) {
    for annotation in shapes.iter().chain(draft) {
        draw_annotation(painter, annotation, project);
    }
}

fn draw_annotation<F: Fn(GeoPoint) -> egui::Pos2>(
    painter: &egui::Painter,
    annotation: &Annotation,
    project: &F,
) {
    match annotation {
        Annotation::Crosshair(point) => draw_crosshair(painter, project(*point)),
        Annotation::Box { a, b } => {
            let corners = box_corners(*a, *b).map(project);
            stroke_polyline(painter, corners.to_vec(), true);
        }
        Annotation::Arrow { tail, head } => {
            let tail_screen = project(*tail);
            let head_screen = project(*head);
            let [left, right] = arrow_head_points(
                tail_screen,
                head_screen,
                ARROW_HEAD_LENGTH_PX,
                ARROW_HEAD_ANGLE_DEG,
            );
            stroke_polyline(painter, vec![tail_screen, head_screen], false);
            stroke_polyline(painter, vec![left, head_screen, right], false);
        }
        Annotation::Freehand(points) => {
            let screen_points = points.iter().map(|point| project(*point)).collect();
            stroke_polyline(painter, screen_points, false);
        }
    }
}

fn draw_crosshair(painter: &egui::Painter, center: egui::Pos2) {
    for stroke in [halo_stroke(), main_stroke()] {
        painter.circle_stroke(center, CROSSHAIR_RING_RADIUS_PX, stroke);
        for (dx, dy) in [(1.0_f32, 0.0_f32), (-1.0, 0.0), (0.0, 1.0), (0.0, -1.0)] {
            let direction = egui::vec2(dx, dy);
            painter.line_segment(
                [
                    center + CROSSHAIR_TICK_INNER_PX * direction,
                    center + CROSSHAIR_TICK_OUTER_PX * direction,
                ],
                stroke,
            );
        }
    }
}

/// Draws a polyline twice: dark halo underneath, red stroke on top.
fn stroke_polyline(painter: &egui::Painter, points: Vec<egui::Pos2>, closed: bool) {
    if points.len() < 2 {
        return;
    }
    if closed {
        painter.add(egui::Shape::closed_line(points.clone(), halo_stroke()));
        painter.add(egui::Shape::closed_line(points, main_stroke()));
    } else {
        painter.add(egui::Shape::line(points.clone(), halo_stroke()));
        painter.add(egui::Shape::line(points, main_stroke()));
    }
}

fn halo_stroke() -> egui::Stroke {
    egui::Stroke::new(ANNOTATION_HALO_WIDTH, ANNOTATION_HALO_COLOR)
}

fn main_stroke() -> egui::Stroke {
    egui::Stroke::new(ANNOTATION_STROKE_WIDTH, ANNOTATION_COLOR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrow_head_barbs_are_symmetric_about_the_shaft() {
        let tail = egui::pos2(0.0, 0.0);
        let head = egui::pos2(10.0, 0.0);
        let [left, right] = arrow_head_points(tail, head, 14.0, 26.0);
        // Both barbs sit `length` from the head...
        assert!(((left - head).length() - 14.0).abs() < 1e-4);
        assert!(((right - head).length() - 14.0).abs() < 1e-4);
        // ...behind the head along the shaft, mirrored across it.
        assert!(left.x < head.x && right.x < head.x);
        assert!((left.y + right.y).abs() < 1e-4);
        assert!((left.y.abs() - 14.0 * 26.0_f32.to_radians().sin()).abs() < 1e-3);
    }

    #[test]
    fn degenerate_arrow_collapses_to_head() {
        let head = egui::pos2(3.0, 4.0);
        assert_eq!(arrow_head_points(head, head, 14.0, 26.0), [head, head]);
    }

    #[test]
    fn box_corners_form_axis_aligned_rectangle() {
        let a = GeoPoint {
            lon: -97.8,
            lat: 35.1,
        };
        let b = GeoPoint {
            lon: -97.2,
            lat: 35.6,
        };
        let corners = box_corners(a, b);
        assert_eq!(corners[0], a);
        assert_eq!(corners[2], b);
        // Adjacent corners share exactly one axis with their neighbors.
        assert_eq!(corners[1].lat, a.lat);
        assert_eq!(corners[1].lon, b.lon);
        assert_eq!(corners[3].lat, b.lat);
        assert_eq!(corners[3].lon, a.lon);
    }

    #[test]
    fn drag_draft_flow_builds_box_and_arrow() {
        let start = GeoPoint {
            lon: -98.0,
            lat: 34.0,
        };
        let end = GeoPoint {
            lon: -97.0,
            lat: 35.0,
        };

        let mut draft = start_draft(ShapeKind::Box, start);
        assert!(!draft_is_valid(&draft), "zero-size box must not commit");
        update_draft(&mut draft, end, true);
        assert_eq!(draft, Annotation::Box { a: start, b: end });
        assert!(draft_is_valid(&draft));

        let mut arrow = start_draft(ShapeKind::Arrow, start);
        update_draft(&mut arrow, end, true);
        assert_eq!(
            arrow,
            Annotation::Arrow {
                tail: start,
                head: end
            }
        );
        assert!(draft_is_valid(&arrow));
    }

    #[test]
    fn freehand_draft_respects_point_spacing_gate() {
        let start = GeoPoint {
            lon: -98.0,
            lat: 34.0,
        };
        let near = GeoPoint {
            lon: -97.999,
            lat: 34.0,
        };
        let far = GeoPoint {
            lon: -97.5,
            lat: 34.2,
        };
        let mut draft = start_draft(ShapeKind::Freehand, start);
        update_draft(&mut draft, near, false);
        assert!(!draft_is_valid(&draft), "single point sketch is invalid");
        update_draft(&mut draft, far, true);
        match &draft {
            Annotation::Freehand(points) => assert_eq!(points.as_slice(), &[start, far]),
            other => panic!("unexpected draft: {other:?}"),
        }
        assert!(draft_is_valid(&draft));
    }
}
