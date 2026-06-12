//! Painter-side rendering for map annotations.
//!
//! The front pips, hatch fills, warning-polygon dress, and icon designs
//! reimplement the GBW Overlay renderer (graphics vocabulary contributed by
//! its author, grayskieswx on YouTube) on egui's `Painter`: same spacing
//! and size ratios, same dash patterns, same color defaults. Everything
//! goes through the map painter, so screenshots and loop recordings pick
//! the shapes up automatically.

use eframe::egui::{self, Color32, Pos2, Stroke, vec2};

use super::geometry::{
    self, SMOOTH_SEGMENTS, catmull_rom, dash_segments, front_arc_radius, front_pip_spacing,
    front_triangle_size, glyph_stations, hatch_lines, polyline_length, sample_at,
    semicircle_points, squall_tick_half_len, squall_tick_spacing, triangle_points,
};
use super::{
    ANNOTATION_HALO_COLOR, ANNOTATION_HALO_EXTRA, Annotation, CROSSHAIR_RING_RADIUS_PX,
    CROSSHAIR_TICK_INNER_PX, CROSSHAIR_TICK_OUTER_PX, FrontKind, GeoPoint, IconKind, ShapeStyle,
    ToolKind, WarnKind, WatchKind, box_corners, tool_default_color,
};

/// GBW dash patterns (on/off px).
const OUTFLOW_DASH: [f32; 2] = [10.0, 7.0];
const TROUGH_DASH: [f32; 4] = [12.0, 5.0, 3.0, 5.0];
const WARN_DASH: [f32; 2] = [14.0, 5.0];
const MESO_DASH: [f32; 2] = [5.0, 3.0];
/// Watch-box hatch: GBW tiles its pattern at `2 × spacing` px.
const WATCH_HATCH_PERIOD: f32 = 20.0;
const FREE_HATCH_PERIOD: f32 = 16.0;
const HATCH_STROKE_WIDTH: f32 = 1.5;
const HATCH_STROKE_ALPHA: f32 = 0.65;
/// Straight/flow arrowhead barb sweep (GBW π/6).
const ARROW_BARB_DEG: f32 = 30.0;
/// Icons render at GBW geometry × this scale (the overlay targets a full
/// 1080p screen; the map wants them a notch smaller).
const ICON_SCALE: f32 = 0.75;
/// Filled pips get a thin dark outline instead of the backbone halo.
const PIP_OUTLINE_WIDTH: f32 = 1.5;
/// Semicircle pip tessellation.
const ARC_SEGMENTS: usize = 12;

/// Draws one annotation through `project` at `alpha` (1.0 committed,
/// lower for the in-progress draft preview).
pub(crate) fn annotation<F: Fn(GeoPoint) -> Pos2>(
    painter: &egui::Painter,
    annotation: &Annotation,
    project: &F,
    alpha: f32,
) {
    match annotation {
        Annotation::Crosshair { at, style } => {
            let color = resolve_color(style, ToolKind::Crosshair, alpha);
            draw_crosshair(painter, project(*at), style.thickness, color, alpha);
        }
        Annotation::Box { a, b, style } => {
            let pts: Vec<Pos2> = box_corners(*a, *b).iter().map(|p| project(*p)).collect();
            let color = resolve_color(style, ToolKind::Box, alpha);
            stroke_closed(painter, &pts, style.thickness, color, alpha);
        }
        Annotation::Arrow { tail, head, style } => {
            let color = resolve_color(style, ToolKind::Arrow, alpha);
            draw_straight_arrow(painter, project(*tail), project(*head), style, color, alpha);
        }
        Annotation::RangeCircle {
            center,
            edge,
            style,
        } => {
            let color = resolve_color(style, ToolKind::RangeCircle, alpha);
            draw_range_circle(painter, *center, *edge, project, style, color, alpha);
        }
        Annotation::Freehand { points, style } => {
            let pts: Vec<Pos2> = points.iter().map(|p| project(*p)).collect();
            let color = resolve_color(style, ToolKind::Freehand, alpha);
            stroke_open(painter, &pts, style.thickness, color, alpha);
        }
        Annotation::Front {
            front,
            points,
            flip,
            pips,
            style,
        } => {
            let pts: Vec<Pos2> = points.iter().map(|p| project(*p)).collect();
            draw_front(painter, *front, &pts, *flip, *pips, style, alpha);
        }
        Annotation::FlowArrow { points, style } => {
            let pts: Vec<Pos2> = points.iter().map(|p| project(*p)).collect();
            let color = resolve_color(style, ToolKind::FlowArrow, alpha);
            draw_flow_arrow(painter, &pts, style, color, alpha);
        }
        Annotation::WatchBox {
            watch,
            a,
            b,
            hatch,
            style,
        } => {
            let pts: Vec<Pos2> = box_corners(*a, *b).iter().map(|p| project(*p)).collect();
            draw_watch_box(painter, *watch, &pts, *hatch, style, alpha);
        }
        Annotation::WarnPolygon {
            warn,
            points,
            style,
        } => {
            let pts: Vec<Pos2> = points.iter().map(|p| project(*p)).collect();
            draw_warn_polygon(painter, *warn, &pts, style, alpha);
        }
        Annotation::Icon { icon, at, style } => {
            draw_icon(painter, *icon, project(*at), style, alpha);
        }
    }
}

/// White vertex dots on the active path draft (GBW's node markers).
pub(crate) fn draft_nodes(painter: &egui::Painter, points: &[Pos2]) {
    for p in points {
        painter.circle_filled(*p, 4.5, Color32::from_rgba_unmultiplied(255, 255, 255, 225));
        painter.circle_stroke(
            *p,
            4.5,
            Stroke::new(1.0, Color32::from_rgba_unmultiplied(0, 0, 0, 115)),
        );
    }
}

// ── COLOR / STROKE HELPERS ──────────────────────────────────────────────

/// The shape's stored override, else the tool's default, with the shape
/// opacity and draft alpha multiplied in.
/// Range circle: stroked circle (screen radius from the projected edge
/// anchor — exact under the map's AEQD) + a haloed radius label at
/// 12 o'clock reading geodesic distance, so the readout is honest at any
/// zoom. Haversine, spherical Earth R = 6371 km (<0.5% — fine for a label).
fn draw_range_circle<F: Fn(GeoPoint) -> Pos2>(
    painter: &egui::Painter,
    center: GeoPoint,
    edge: GeoPoint,
    project: &F,
    style: &ShapeStyle,
    color: Color32,
    alpha: f32,
) {
    let center_px = project(center);
    let radius_px = center_px.distance(project(edge));
    if radius_px < 2.0 {
        return;
    }
    painter.circle_stroke(
        center_px,
        radius_px,
        Stroke::new(style.thickness + ANNOTATION_HALO_EXTRA, halo_color(alpha)),
    );
    painter.circle_stroke(center_px, radius_px, Stroke::new(style.thickness, color));
    painter.circle_filled(center_px, 2.5, color);
    let km = geo_distance_km(center, edge);
    let label = format!("{:.1} mi / {:.1} km", km * 0.621_371, km);
    let anchor = center_px - vec2(0.0, radius_px + 5.0);
    let font = egui::FontId::proportional(12.0);
    painter.text(
        anchor + vec2(1.2, 1.2),
        egui::Align2::CENTER_BOTTOM,
        &label,
        font.clone(),
        halo_color(alpha),
    );
    painter.text(anchor, egui::Align2::CENTER_BOTTOM, label, font, color);
}

fn geo_distance_km(a: GeoPoint, b: GeoPoint) -> f32 {
    let (lat1, lon1) = (f64::from(a.lat).to_radians(), f64::from(a.lon).to_radians());
    let (lat2, lon2) = (f64::from(b.lat).to_radians(), f64::from(b.lon).to_radians());
    let s = ((lat2 - lat1) / 2.0).sin().powi(2)
        + lat1.cos() * lat2.cos() * ((lon2 - lon1) / 2.0).sin().powi(2);
    (2.0 * 6371.0 * s.sqrt().min(1.0).asin()) as f32
}

fn resolve_color(style: &ShapeStyle, tool: ToolKind, alpha: f32) -> Color32 {
    let base = style
        .color
        .map(|[r, g, b]| Color32::from_rgb(r, g, b))
        .unwrap_or_else(|| tool_default_color(tool));
    fade(base, style.opacity * alpha)
}

/// Multiplies a color's opacity by `factor`. `Color32` is premultiplied,
/// so this scales all channels (egui's gamma-space fade); repeated fades
/// compound multiplicatively, which is exactly what stacking shape opacity
/// × draft alpha × fill alpha wants.
fn fade(color: Color32, factor: f32) -> Color32 {
    color.gamma_multiply(factor.clamp(0.0, 1.0))
}

fn halo_color(alpha: f32) -> Color32 {
    fade(ANNOTATION_HALO_COLOR, alpha)
}

/// Open polyline with the BowEcho dark halo underneath.
fn stroke_open(painter: &egui::Painter, points: &[Pos2], width: f32, color: Color32, alpha: f32) {
    if points.len() < 2 {
        return;
    }
    painter.add(egui::Shape::line(
        points.to_vec(),
        Stroke::new(width + ANNOTATION_HALO_EXTRA, halo_color(alpha)),
    ));
    painter.add(egui::Shape::line(
        points.to_vec(),
        Stroke::new(width, color),
    ));
}

/// Closed polyline with the halo underneath.
fn stroke_closed(painter: &egui::Painter, points: &[Pos2], width: f32, color: Color32, alpha: f32) {
    if points.len() < 2 {
        return;
    }
    painter.add(egui::Shape::closed_line(
        points.to_vec(),
        Stroke::new(width + ANNOTATION_HALO_EXTRA, halo_color(alpha)),
    ));
    painter.add(egui::Shape::closed_line(
        points.to_vec(),
        Stroke::new(width, color),
    ));
}

/// Dashed polyline (arbitrary on/off pattern) with halo dashes underneath.
fn stroke_dashed(
    painter: &egui::Painter,
    points: &[Pos2],
    width: f32,
    color: Color32,
    pattern: &[f32],
    alpha: f32,
) {
    let dashes = dash_segments(points, pattern);
    for dash in &dashes {
        painter.add(egui::Shape::line(
            dash.clone(),
            Stroke::new(width + ANNOTATION_HALO_EXTRA, halo_color(alpha)),
        ));
    }
    for dash in dashes {
        painter.add(egui::Shape::line(dash, Stroke::new(width, color)));
    }
}

/// Filled convex pip with a thin dark outline for separation over echoes.
fn fill_convex(painter: &egui::Painter, points: Vec<Pos2>, color: Color32, alpha: f32) {
    painter.add(egui::Shape::closed_line(
        points.clone(),
        Stroke::new(PIP_OUTLINE_WIDTH, halo_color(alpha)),
    ));
    painter.add(egui::Shape::convex_polygon(points, color, Stroke::NONE));
}

/// Fills a simple (possibly concave) polygon via ear clipping.
fn fill_polygon(painter: &egui::Painter, points: &[Pos2], color: Color32) {
    if points.len() < 3 {
        return;
    }
    let mut mesh = egui::Mesh::default();
    for p in points {
        mesh.colored_vertex(*p, color);
    }
    for tri in geometry::ear_clip(points) {
        mesh.indices
            .extend(tri.iter().map(|&i| u32::try_from(i).unwrap_or(0)));
    }
    painter.add(egui::Shape::mesh(mesh));
}

/// Small text chip with a backing rect (watch/warning labels).
fn label_chip(painter: &egui::Painter, center: Pos2, text: &str, fg: Color32, bg: Color32) {
    let font = egui::FontId::proportional(11.0);
    let galley = painter.layout_no_wrap(text.to_owned(), font, fg);
    let rect = egui::Rect::from_center_size(center, galley.size() + vec2(10.0, 4.0));
    painter.rect_filled(rect, 3.0, bg);
    let text_pos = rect.center() - galley.size() * 0.5;
    painter.galley(text_pos, galley, fg);
}

// ── POINTERS ────────────────────────────────────────────────────────────

fn draw_crosshair(painter: &egui::Painter, center: Pos2, width: f32, color: Color32, alpha: f32) {
    for stroke in [
        Stroke::new(width + ANNOTATION_HALO_EXTRA, halo_color(alpha)),
        Stroke::new(width, color),
    ] {
        painter.circle_stroke(center, CROSSHAIR_RING_RADIUS_PX, stroke);
        for (dx, dy) in [(1.0_f32, 0.0_f32), (-1.0, 0.0), (0.0, 1.0), (0.0, -1.0)] {
            let direction = vec2(dx, dy);
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

// ── ARROWS ──────────────────────────────────────────────────────────────

/// The two barb endpoints of an arrow head at `head`, swept back along the
/// `angle` heading by `barb_deg` on each side.
pub(crate) fn arrow_head_points(head: Pos2, angle: f32, length: f32, barb_deg: f32) -> [Pos2; 2] {
    let barb = barb_deg.to_radians();
    [
        head - length * vec2((angle - barb).cos(), (angle - barb).sin()),
        head - length * vec2((angle + barb).cos(), (angle + barb).sin()),
    ]
}

/// Straight arrow with a filled head (GBW `drawArrow`): the shaft stops
/// short of the tip so it doesn't poke through the head.
fn draw_straight_arrow(
    painter: &egui::Painter,
    tail: Pos2,
    head: Pos2,
    style: &ShapeStyle,
    color: Color32,
    alpha: f32,
) {
    let shaft = head - tail;
    if shaft.length_sq() <= f32::EPSILON {
        return;
    }
    let angle = shaft.y.atan2(shaft.x);
    let sw = style.thickness + 1.0;
    let hs = (sw * 4.5).max(14.0);
    let shaft_end = head - 0.7 * hs * vec2(angle.cos(), angle.sin());
    stroke_open(painter, &[tail, shaft_end], sw, color, alpha);
    let [left, right] = arrow_head_points(head, angle, hs, ARROW_BARB_DEG);
    fill_convex(painter, vec![head, left, right], color, alpha);
}

/// Curved flow arrow (GBW `drawFrontArrow`): smoothed spline with a single
/// filled head at the terminal end, oriented to the local tangent.
fn draw_flow_arrow(
    painter: &egui::Painter,
    raw: &[Pos2],
    style: &ShapeStyle,
    color: Color32,
    alpha: f32,
) {
    if raw.len() < 2 {
        return;
    }
    let pts = catmull_rom(raw, SMOOTH_SEGMENTS);
    let total = polyline_length(&pts);
    let sw = style.thickness + 1.5;
    stroke_open(painter, &pts, sw, color, alpha);
    let end = sample_at(&pts, total - 2.0);
    let hs = (sw * 4.0).max(16.0);
    let [left, right] = arrow_head_points(end.pos, end.angle, hs, ARROW_BARB_DEG);
    fill_convex(painter, vec![end.pos, left, right], color, alpha);
}

// ── FRONTS ──────────────────────────────────────────────────────────────

/// Renders a front: smoothed backbone plus glyphs marching at even
/// arc-length spacing, oriented to the local tangent (GBW `drawFront` and
/// its pip functions). `flip` mirrors the glyph side.
fn draw_front(
    painter: &egui::Painter,
    kind: FrontKind,
    raw: &[Pos2],
    flip: bool,
    pips: bool,
    style: &ShapeStyle,
    alpha: f32,
) {
    if raw.len() < 2 {
        return;
    }
    let pts = catmull_rom(raw, SMOOTH_SEGMENTS);
    let total = polyline_length(&pts);
    let t = style.thickness;
    let side = if flip { 1.0 } else { -1.0 };
    let color = resolve_color(style, ToolKind::Front(kind), alpha);
    match kind {
        FrontKind::Cold => {
            stroke_open(painter, &pts, t + 1.5, color, alpha);
            front_triangles(painter, &pts, total, t, side, color, alpha, 1, 0);
        }
        FrontKind::Warm => {
            stroke_open(painter, &pts, t + 1.5, color, alpha);
            front_arcs(painter, &pts, total, t, side, color, alpha, 1, 0, true);
        }
        FrontKind::Stationary => {
            // Red semicircles on one side (even slots), blue triangles on
            // the other (odd slots). A color override tints the cool side;
            // the warm side stays NWS red.
            stroke_open(painter, &pts, t + 1.5, color, alpha);
            let warm = fade(
                tool_default_color(ToolKind::Front(FrontKind::Warm)),
                style.opacity * alpha,
            );
            front_arcs(painter, &pts, total, t, side, warm, alpha, 2, 0, true);
            front_triangles(painter, &pts, total, t, -side, color, alpha, 2, 1);
        }
        FrontKind::Occluded => {
            // Purple, alternating triangle then semicircle on the same side.
            stroke_open(painter, &pts, t + 1.5, color, alpha);
            front_triangles(painter, &pts, total, t, side, color, alpha, 2, 0);
            front_arcs(painter, &pts, total, t, side, color, alpha, 2, 1, true);
        }
        FrontKind::Dryline => {
            // Solid line with UNFILLED scallops (open arc strokes).
            stroke_open(painter, &pts, t + 1.5, color, alpha);
            front_arcs(painter, &pts, total, t, side, color, alpha, 1, 0, false);
        }
        FrontKind::Outflow => {
            stroke_dashed(painter, &pts, t.max(1.5), color, &OUTFLOW_DASH, alpha);
            if pips {
                front_arcs(painter, &pts, total, t, side, color, alpha, 1, 0, false);
            }
        }
        FrontKind::Trough => {
            stroke_dashed(painter, &pts, t + 1.0, color, &TROUGH_DASH, alpha);
        }
        FrontKind::Squall => {
            stroke_open(painter, &pts, t + 2.5, color, alpha);
            squall_ticks(painter, &pts, total, t, color, alpha);
        }
    }
}

/// Filled triangle pips every `every` stations starting at `offset`
/// (GBW `pipTri` / `pipTriAlt`).
#[expect(clippy::too_many_arguments, reason = "thin glyph-loop helper")]
fn front_triangles(
    painter: &egui::Painter,
    pts: &[Pos2],
    total: f32,
    thickness: f32,
    side: f32,
    color: Color32,
    alpha: f32,
    every: usize,
    offset: usize,
) {
    let spacing = front_pip_spacing(thickness);
    let size = front_triangle_size(thickness);
    for (idx, d) in glyph_stations(total, spacing).into_iter().enumerate() {
        if idx % every != offset {
            continue;
        }
        let s = sample_at(pts, d);
        let tri = triangle_points(s.pos, size, s.angle, side);
        fill_convex(painter, tri.to_vec(), color, alpha);
    }
}

/// Semicircle pips every `every` stations starting at `offset`: filled
/// half-discs for warm/stationary/occluded fronts, open scallop strokes
/// for drylines and outflow pips (GBW `pipArc` / `pipArcAlt`).
#[expect(clippy::too_many_arguments, reason = "thin glyph-loop helper")]
fn front_arcs(
    painter: &egui::Painter,
    pts: &[Pos2],
    total: f32,
    thickness: f32,
    side: f32,
    color: Color32,
    alpha: f32,
    every: usize,
    offset: usize,
    filled: bool,
) {
    let spacing = front_pip_spacing(thickness);
    let radius = front_arc_radius(thickness);
    for (idx, d) in glyph_stations(total, spacing).into_iter().enumerate() {
        if idx % every != offset {
            continue;
        }
        let s = sample_at(pts, d);
        let arc = semicircle_points(s.pos, radius, s.angle, side, ARC_SEGMENTS);
        if filled {
            fill_convex(painter, arc, color, alpha);
        } else {
            painter.add(egui::Shape::line(
                arc.clone(),
                Stroke::new(thickness + 1.5 + ANNOTATION_HALO_EXTRA, halo_color(alpha)),
            ));
            painter.add(egui::Shape::line(arc, Stroke::new(thickness + 1.5, color)));
        }
    }
}

/// Paired tick marks crossing the squall line (GBW `pipSquallTicks`).
fn squall_ticks(
    painter: &egui::Painter,
    pts: &[Pos2],
    total: f32,
    thickness: f32,
    color: Color32,
    alpha: f32,
) {
    let spacing = squall_tick_spacing(thickness);
    let half_len = squall_tick_half_len(thickness);
    let width = thickness.max(1.5);
    for d in glyph_stations(total, spacing) {
        let s = sample_at(pts, d);
        let normal_angle = s.angle + std::f32::consts::FRAC_PI_2;
        let n = vec2(normal_angle.cos(), normal_angle.sin());
        let tick = [s.pos - half_len * n, s.pos + half_len * n];
        painter.line_segment(
            tick,
            Stroke::new(width + ANNOTATION_HALO_EXTRA, halo_color(alpha)),
        );
        painter.line_segment(tick, Stroke::new(width, color));
    }
}

// ── WATCH BOXES / WARNING POLYGONS ──────────────────────────────────────

fn watch_label(kind: WatchKind) -> Option<&'static str> {
    match kind {
        WatchKind::Tor => Some("TOR WATCH"),
        WatchKind::Svr => Some("SVR WATCH"),
        WatchKind::Wind => Some("WIND WATCH"),
        WatchKind::Free => None,
    }
}

/// GBW hatch geometry: TOR/SVR hatch one diagonal, wind the other; the
/// free box hatches a touch tighter.
fn watch_hatch_params(kind: WatchKind) -> (f32, f32) {
    match kind {
        WatchKind::Wind => (45.0, WATCH_HATCH_PERIOD),
        WatchKind::Free => (135.0, FREE_HATCH_PERIOD),
        _ => (135.0, WATCH_HATCH_PERIOD),
    }
}

fn watch_fill_alpha(kind: WatchKind) -> f32 {
    match kind {
        WatchKind::Free => 0.06,
        _ => 0.08,
    }
}

fn draw_watch_box(
    painter: &egui::Painter,
    kind: WatchKind,
    pts: &[Pos2],
    hatch: bool,
    style: &ShapeStyle,
    alpha: f32,
) {
    if pts.len() < 3 {
        return;
    }
    let stroke_color = resolve_color(style, ToolKind::Watch(kind), alpha);
    let fill = fade(stroke_color, watch_fill_alpha(kind));
    fill_polygon(painter, pts, fill);
    if hatch {
        let (angle, period) = watch_hatch_params(kind);
        let hatch_stroke = Stroke::new(HATCH_STROKE_WIDTH, fade(stroke_color, HATCH_STROKE_ALPHA));
        for (a, b) in hatch_lines(pts, angle, period) {
            painter.line_segment([a, b], hatch_stroke);
        }
    }
    stroke_closed(painter, pts, style.thickness.max(2.5), stroke_color, alpha);
    if let Some(label) = watch_label(kind) {
        let centroid = pts.iter().fold(Pos2::ZERO, |acc, p| acc + p.to_vec2()) / pts.len() as f32;
        label_chip(
            painter,
            centroid,
            label,
            stroke_color,
            fade(Color32::BLACK, 0.65 * alpha),
        );
    }
}

fn warn_label(kind: WarnKind) -> Option<&'static str> {
    match kind {
        WarnKind::Tor => Some("TORNADO WARNING"),
        WarnKind::Svr => Some("SVR TSTM WARNING"),
        WarnKind::Ffw => Some("FLASH FLOOD WARNING"),
        WarnKind::Free => None,
    }
}

fn warn_fill_alpha(kind: WarnKind) -> f32 {
    match kind {
        WarnKind::Tor => 0.15,
        WarnKind::Svr => 0.12,
        _ => 0.10,
    }
}

/// NWS-style warning polygon: translucent fill, bold dark backing outline,
/// colored dashed border, label banner at the top (GBW `drawWarnBox`).
fn draw_warn_polygon(
    painter: &egui::Painter,
    kind: WarnKind,
    pts: &[Pos2],
    style: &ShapeStyle,
    alpha: f32,
) {
    if pts.len() < 3 {
        return;
    }
    let stroke_color = resolve_color(style, ToolKind::Warn(kind), alpha);
    fill_polygon(painter, pts, fade(stroke_color, warn_fill_alpha(kind)));
    let width = style.thickness + 0.5;
    painter.add(egui::Shape::closed_line(
        pts.to_vec(),
        Stroke::new(width + 2.5, fade(Color32::BLACK, 0.5 * alpha)),
    ));
    // Dashed colored border traced around the closed outline.
    let mut ring: Vec<Pos2> = pts.to_vec();
    ring.push(pts[0]);
    for dash in dash_segments(&ring, &WARN_DASH) {
        painter.add(egui::Shape::line(dash, Stroke::new(width, stroke_color)));
    }
    if let Some(label) = warn_label(kind) {
        let top = pts.iter().fold(f32::MAX, |acc, p| acc.min(p.y));
        let cx = pts.iter().fold(0.0, |acc, p| acc + p.x) / pts.len() as f32;
        label_chip(
            painter,
            Pos2::new(cx, top - 10.0),
            label,
            Color32::BLACK,
            stroke_color,
        );
    }
}

// ── ICONS ───────────────────────────────────────────────────────────────

/// Vector icon stamps (GBW `drawIcon` geometry × [`ICON_SCALE`]); sized in
/// screen px, anchored at the projected geo point.
fn draw_icon(
    painter: &egui::Painter,
    kind: IconKind,
    center: Pos2,
    style: &ShapeStyle,
    alpha: f32,
) {
    let a = style.opacity * alpha;
    match kind {
        IconKind::Low => pressure_icon(
            painter,
            center,
            "L",
            Color32::from_rgba_unmultiplied(160, 0, 0, 77),
            Color32::from_rgb(0xee, 0x22, 0x22),
            Color32::from_rgb(0xff, 0x33, 0x33),
            a,
        ),
        IconKind::High => pressure_icon(
            painter,
            center,
            "H",
            Color32::from_rgba_unmultiplied(0, 50, 200, 77),
            Color32::from_rgb(0x22, 0x55, 0xee),
            Color32::from_rgb(0x44, 0x77, 0xff),
            a,
        ),
        IconKind::Meso => meso_icon(painter, center, a),
        IconKind::Tornado => tornado_icon(painter, center, a),
        IconKind::Hail => hail_icon(painter, center, a),
    }
}

/// Circled pressure-center letter (GBW L/H icons: R=28, 34 px letter).
fn pressure_icon(
    painter: &egui::Painter,
    center: Pos2,
    letter: &str,
    fill: Color32,
    ring: Color32,
    text: Color32,
    alpha: f32,
) {
    let r = 28.0 * ICON_SCALE;
    painter.circle_filled(center, r + 2.0, fade(Color32::BLACK, 0.4 * alpha));
    painter.circle(
        center,
        r,
        fade(fill, alpha),
        Stroke::new(2.5, fade(ring, alpha)),
    );
    painter.text(
        center,
        egui::Align2::CENTER_CENTER,
        letter,
        egui::FontId::proportional(34.0 * ICON_SCALE),
        fade(text, alpha),
    );
}

/// Mesocyclone: translucent disc, dashed circulation ring, center dot + M.
fn meso_icon(painter: &egui::Painter, center: Pos2, alpha: f32) {
    let r = 23.0 * ICON_SCALE;
    let orange = Color32::from_rgb(0xff, 0x88, 0x00);
    painter.circle_filled(
        center,
        r,
        fade(Color32::from_rgba_unmultiplied(255, 120, 0, 38), alpha),
    );
    let ring: Vec<Pos2> = (0..=48)
        .map(|i| {
            let theta = std::f32::consts::TAU * (i as f32) / 48.0;
            center + r * vec2(theta.cos(), theta.sin())
        })
        .collect();
    for dash in dash_segments(&ring, &MESO_DASH) {
        painter.add(egui::Shape::line(
            dash,
            Stroke::new(2.5, fade(orange, alpha)),
        ));
    }
    painter.text(
        center,
        egui::Align2::CENTER_CENTER,
        "M",
        egui::FontId::proportional(14.0 * ICON_SCALE),
        fade(orange, alpha),
    );
}

/// Tornado funnel silhouette with swirl lines (GBW `icon_tornado`).
fn tornado_icon(painter: &egui::Painter, center: Pos2, alpha: f32) {
    let p = |dx: f32, dy: f32| center + ICON_SCALE * vec2(dx, dy);
    let funnel = vec![
        p(-13.0, -22.0),
        p(13.0, -22.0),
        p(8.0, -9.0),
        p(5.0, 8.0),
        p(1.0, 22.0),
        p(-1.0, 22.0),
        p(-5.0, 8.0),
        p(-8.0, -9.0),
    ];
    fill_polygon(
        painter,
        &funnel,
        fade(Color32::from_rgba_unmultiplied(155, 0, 200, 115), alpha),
    );
    painter.add(egui::Shape::closed_line(
        funnel,
        Stroke::new(2.0, fade(Color32::from_rgb(0xbb, 0x00, 0xee), alpha)),
    ));
    // Swirl lines: width shrinks toward the funnel tip.
    let swirl = fade(Color32::from_rgb(0xcc, 0x44, 0xff), alpha);
    for dy in [-19.0_f32, -12.0, -4.0] {
        let half_width = (dy - 10.0).abs() * 0.4 + 2.0;
        painter.line_segment(
            [p(-half_width, dy), p(half_width, dy)],
            Stroke::new(1.5, swirl),
        );
    }
}

/// Large hail: shadowed teal disc with "GH" (GBW `icon_hail`).
fn hail_icon(painter: &egui::Painter, center: Pos2, alpha: f32) {
    let r = 20.0 * ICON_SCALE;
    let teal = Color32::from_rgb(0x00, 0xcc, 0xcc);
    painter.circle_filled(center, r + 1.0, fade(Color32::BLACK, 0.4 * alpha));
    painter.circle(
        center,
        r,
        fade(Color32::from_rgba_unmultiplied(0, 180, 180, 64), alpha),
        Stroke::new(2.5, fade(teal, alpha)),
    );
    painter.text(
        center,
        egui::Align2::CENTER_CENTER,
        "GH",
        egui::FontId::proportional(14.0 * ICON_SCALE),
        fade(Color32::from_rgb(0x00, 0xdd, 0xdd), alpha),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::pos2;

    /// Painting every shape kind (plus degenerate drafts) must not panic —
    /// this exercises the projection → glyph → painter path end to end.
    #[test]
    fn painting_every_shape_kind_is_panic_free() {
        use super::super::{DraftFlags, ShapeStyle, start_draft};

        let ctx = egui::Context::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            let painter = egui::Painter::new(
                ui.ctx().clone(),
                egui::LayerId::background(),
                egui::Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0)),
            );
            // Linear toy projection: enough to spread shapes across screen.
            let project = |g: GeoPoint| pos2((g.lon + 100.0) * 40.0, (40.0 - g.lat) * 40.0);
            for shape in super::super::persist::every_shape_kind() {
                annotation(&painter, &shape, &project, 1.0);
                annotation(&painter, &shape, &project, 0.6);
            }
            // Single-vertex drafts (the first click) must render quietly.
            for d in super::super::TOOLS {
                let draft = start_draft(
                    d.tool,
                    GeoPoint {
                        lon: -97.5,
                        lat: 35.0,
                    },
                    ShapeStyle::default(),
                    DraftFlags {
                        flip: true,
                        pips: true,
                        hatch: true,
                    },
                );
                annotation(&painter, &draft, &project, 0.6);
            }
            draft_nodes(&painter, &[pos2(10.0, 10.0), pos2(50.0, 40.0)]);
        });
    }

    #[test]
    fn arrow_head_barbs_are_symmetric_about_the_shaft() {
        // Eastbound arrow at the origin.
        let head = pos2(10.0, 0.0);
        let [left, right] = arrow_head_points(head, 0.0, 14.0, 30.0);
        assert!(((left - head).length() - 14.0).abs() < 1e-4);
        assert!(((right - head).length() - 14.0).abs() < 1e-4);
        // Behind the head along the shaft, mirrored across it.
        assert!(left.x < head.x && right.x < head.x);
        assert!((left.y + right.y).abs() < 1e-4);
        assert!((left.y.abs() - 14.0 * 30.0_f32.to_radians().sin()).abs() < 1e-3);
    }

    #[test]
    fn fade_preserves_premultiplied_proportions() {
        // Color32 is premultiplied: a fade scales every channel together,
        // keeping the visual hue while halving coverage.
        let base = Color32::from_rgb(40, 80, 120);
        let half = fade(base, 0.5);
        assert!((i32::from(half.a()) - 128).abs() <= 1);
        assert!((i32::from(half.r()) - 20).abs() <= 1);
        assert!((i32::from(half.g()) - 40).abs() <= 1);
        assert!((i32::from(half.b()) - 60).abs() <= 1);
        assert_eq!(fade(base, 1.0), base);
        assert_eq!(fade(base, 0.0).a(), 0);
        // Compounding fades multiply: 0.5 then 0.5 ≈ 0.25.
        let quarter = fade(fade(base, 0.5), 0.5);
        assert!((i32::from(quarter.a()) - 64).abs() <= 2);
    }
}
