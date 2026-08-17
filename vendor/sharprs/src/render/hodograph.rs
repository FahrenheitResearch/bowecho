//! Hodograph renderer for sharprs.
//!
//! Draws a SHARPpy-style hodograph panel on a [`Canvas`], including:
//!
//! - Speed rings labeled in knots (20, 40, 60, 80 kt)
//! - U = 0 / V = 0 axis lines
//! - Wind trace color-coded by height AGL
//! - Height dots labeled at each km
//! - Bunkers RM / LM storm motion markers
//! - Corfidi upshear / downshear vectors
//! - Mean wind vector
//! - Critical angle annotation
//! - Inferred temperature advection indicator
//! - Storm-relative wind barb indicators
//!
//! All wind components are in **knots**; directions in **meteorological degrees**.

use crate::render::canvas::{Canvas, FONT_H};

// =========================================================================
// Colour palette (SHARPpy dark-background style)
// =========================================================================

const COL_BG: [u8; 4] = [0, 0, 0, 255];
const COL_PANEL_BG: [u8; 4] = [0, 0, 0, 255];
const COL_PANEL_BORDER: [u8; 4] = [215, 215, 215, 255];
const COL_RING: [u8; 4] = [40, 40, 52, 255]; // subtle gray rings — don't compete with data
const COL_AXIS: [u8; 4] = [70, 70, 70, 255];
const COL_RING_LABEL: [u8; 4] = [130, 130, 155, 255];
const COL_TEXT: [u8; 4] = [240, 240, 240, 255];
const COL_TEXT_DIM: [u8; 4] = [150, 150, 165, 255];
const COL_TEXT_HEADER: [u8; 4] = [100, 190, 255, 255];
const COL_TEXT_WARN: [u8; 4] = [255, 210, 80, 255];
const COL_LABEL_BG: [u8; 4] = [0, 0, 0, 180]; // semi-transparent dark box behind labels
const COL_DOT_BG: [u8; 4] = [255, 255, 255, 255]; // white dot background for height markers
const COL_DOT_FG: [u8; 4] = [10, 10, 22, 255]; // dark text inside height dots

// Height-band trace colours — BRIGHT and saturated
const COL_0_1KM: [u8; 4] = [255, 30, 30, 255]; // Bright red
const COL_1_3KM: [u8; 4] = [255, 165, 0, 255]; // Bright orange
const COL_3_6KM: [u8; 4] = [255, 255, 0, 255]; // Bright yellow
const COL_6_9KM: [u8; 4] = [0, 230, 0, 255]; // Bright green
const COL_9_12KM: [u8; 4] = [50, 130, 255, 255]; // Bright blue
const COL_12_PLUS: [u8; 4] = [200, 80, 255, 255]; // Bright purple

// Marker colours
const COL_RM: [u8; 4] = [255, 50, 50, 255]; // Bunkers RM — bright red
const COL_LM: [u8; 4] = [60, 130, 255, 255]; // Bunkers LM — blue
const COL_MEAN: [u8; 4] = [210, 210, 210, 255]; // Mean wind — white/grey
const COL_CORFIDI_UP: [u8; 4] = [255, 180, 60, 255]; // Corfidi upshear — orange
const COL_CORFIDI_DN: [u8; 4] = [60, 220, 255, 255]; // Corfidi downshear — cyan
const COL_SR_WIND: [u8; 4] = [180, 255, 180, 200]; // Storm-relative wind

// Speed rings (knots)
const SPEED_RINGS: &[f64] = &[20.0, 40.0, 60.0, 80.0];

// Wind trace thickness in pixels
const TRACE_THICKNESS: i32 = 3;
const SMALL_TEXT_H: i32 = 14;

// =========================================================================
// Input data structures
// =========================================================================

/// A single wind observation at a known height.
#[derive(Debug, Clone, Copy)]
pub struct WindLevel {
    /// Height AGL in metres.
    pub height_agl_m: f64,
    /// U-component in knots (positive eastward).
    pub u_kts: f64,
    /// V-component in knots (positive northward).
    pub v_kts: f64,
}

/// Storm motion vector in knots (U, V) plus formatted label.
#[derive(Debug, Clone)]
pub struct StormMotion {
    /// U-component in knots.
    pub u_kts: f64,
    /// V-component in knots.
    pub v_kts: f64,
    /// Formatted label, e.g. "281/38 RM".
    pub label: String,
}

/// Corfidi MCS motion vector.
#[derive(Debug, Clone)]
pub struct CorfidiVector {
    /// U-component in knots.
    pub u_kts: f64,
    /// V-component in knots.
    pub v_kts: f64,
    /// Formatted label, e.g. "UP=320/38".
    pub label: String,
}

/// Storm-relative wind for a specific layer, drawn as a short barb from the
/// storm motion point.
#[derive(Debug, Clone)]
pub struct SRWindLayer {
    /// U-component of storm-relative wind (knots).
    pub sr_u_kts: f64,
    /// V-component of storm-relative wind (knots).
    pub sr_v_kts: f64,
    /// Label, e.g. "0-2 km".
    pub label: String,
}

/// All pre-computed data needed to render a hodograph.
#[derive(Debug, Clone)]
pub struct HodographData {
    /// Wind levels sorted by ascending height, interpolated at fine spacing.
    pub winds: Vec<WindLevel>,

    /// Bunkers right-mover storm motion.
    pub bunkers_rm: StormMotion,

    /// Bunkers left-mover storm motion.
    pub bunkers_lm: StormMotion,

    /// 0-6 km mean wind (U, V in knots) with label.
    pub mean_wind: StormMotion,

    /// Corfidi upshear vector.
    pub corfidi_up: CorfidiVector,

    /// Corfidi downshear vector.
    pub corfidi_dn: CorfidiVector,

    /// Critical angle in degrees (0-180).
    pub critical_angle: f64,

    /// Inferred temperature advection: positive = WAA, negative = CAA, 0 = neutral.
    pub temperature_advection: f64,

    /// Storm-relative wind layers (optional, drawn from RM point).
    pub sr_winds: Vec<SRWindLayer>,
}

// =========================================================================
// Internal helpers
// =========================================================================

/// Return the height-band colour for a given AGL height in metres.
fn height_color(h_agl_m: f64) -> [u8; 4] {
    if h_agl_m < 1000.0 {
        COL_0_1KM
    } else if h_agl_m < 3000.0 {
        COL_1_3KM
    } else if h_agl_m < 6000.0 {
        COL_3_6KM
    } else if h_agl_m < 9000.0 {
        COL_6_9KM
    } else if h_agl_m < 12000.0 {
        COL_9_12KM
    } else {
        COL_12_PLUS
    }
}

/// Convert U/V components (knots) to meteorological direction and speed.
fn comp2vec(u: f64, v: f64) -> (f64, f64) {
    let spd = (u * u + v * v).sqrt();
    if spd < 0.01 {
        return (0.0, 0.0);
    }
    let mut wdir = u.atan2(v).to_degrees() + 180.0;
    if wdir >= 360.0 {
        wdir -= 360.0;
    }
    if wdir < 0.0 {
        wdir += 360.0;
    }
    (wdir, spd)
}

/// Format a U/V vector as "DDD/SS" (3-digit direction, integer speed).
fn format_dir_spd(u: f64, v: f64) -> String {
    let (dir, spd) = comp2vec(u, v);
    format!("{:03.0}/{:.0}", dir, spd)
}

/// Draw text at 2x scale.
fn draw_text_2x(c: &mut Canvas, text: &str, px: i32, py: i32, col: [u8; 4]) {
    c.draw_text_scaled(text, px, py, col, 2);
}

/// Width of text at 2x scale in pixels.
fn text_width_2x(text: &str) -> i32 {
    Canvas::text_width_scaled(text, 2)
}

/// Height of text at 2x scale in pixels.
const fn text_height_2x() -> i32 {
    22
}

/// Draw text with a dark semi-transparent background box for readability.
fn draw_text_with_bg(c: &mut Canvas, text: &str, px: i32, py: i32, col: [u8; 4]) {
    let tw = Canvas::text_width(text);
    let pad = 2;
    c.fill_rect(
        px - pad,
        py - 2,
        tw + pad * 2,
        SMALL_TEXT_H + 4,
        COL_LABEL_BG,
    );
    c.draw_text(text, px, py, col);
}

/// Draw 2x text with a dark semi-transparent background box.
fn draw_text_2x_with_bg(c: &mut Canvas, text: &str, px: i32, py: i32, col: [u8; 4]) {
    let tw = text_width_2x(text);
    let th = text_height_2x();
    let pad = 3;
    c.fill_rect(px - pad, py - 2, tw + pad * 2, th + 4, COL_LABEL_BG);
    draw_text_2x(c, text, px, py, col);
}

fn clamp_label_xy(
    px: i32,
    py: i32,
    tw: i32,
    th: i32,
    rx: i32,
    ry: i32,
    rw: i32,
    rh: i32,
) -> (i32, i32) {
    let min_x = rx + 6;
    let min_y = ry + 6;
    let max_x = (rx + rw - tw - 6).max(min_x);
    let max_y = (ry + rh - th - 6).max(min_y);
    (px.clamp(min_x, max_x), py.clamp(min_y, max_y))
}

fn marker_label_xy(
    sx: i32,
    sy: i32,
    marker_r: i32,
    tw: i32,
    th: i32,
    bx: i32,
    by: i32,
    bw: i32,
    bh: i32,
) -> (i32, i32) {
    let right_x = sx + marker_r + 5;
    let left_x = sx - marker_r - 5 - tw;
    let px = if right_x + tw <= bx + bw - 8 {
        right_x
    } else {
        left_x
    };
    clamp_label_xy(px, sy - th / 2, tw, th, bx, by, bw, bh)
}

fn draw_text_with_bg_clamped(
    c: &mut Canvas,
    text: &str,
    px: i32,
    py: i32,
    col: [u8; 4],
    bounds: (i32, i32, i32, i32),
) {
    let tw = Canvas::text_width(text);
    let (x, y) = clamp_label_xy(
        px,
        py,
        tw,
        SMALL_TEXT_H,
        bounds.0,
        bounds.1,
        bounds.2,
        bounds.3,
    );
    draw_text_with_bg(c, text, x, y, col);
}

fn draw_text_2x_with_bg_clamped(
    c: &mut Canvas,
    text: &str,
    px: i32,
    py: i32,
    col: [u8; 4],
    bounds: (i32, i32, i32, i32),
) {
    let tw = text_width_2x(text);
    let th = text_height_2x();
    let (x, y) = clamp_label_xy(px, py, tw, th, bounds.0, bounds.1, bounds.2, bounds.3);
    draw_text_2x_with_bg(c, text, x, y, col);
}

// =========================================================================
// Hodograph renderer
// =========================================================================

/// Render a hodograph panel onto a [`Canvas`].
///
/// # Arguments
///
/// * `c` — Target canvas.
/// * `data` — Pre-computed hodograph data (wind levels, storm motion, etc.).
/// * `rx`, `ry` — Top-left corner of the panel in canvas coordinates.
/// * `rw`, `rh` — Panel width and height in pixels.
pub fn draw_hodograph(c: &mut Canvas, data: &HodographData, rx: i32, ry: i32, rw: i32, rh: i32) {
    // ── Panel background ──────────────────────────────────────────
    c.fill_rect(rx, ry, rw, rh, COL_PANEL_BG);
    c.draw_rect(rx, ry, rw, rh, COL_PANEL_BORDER);

    // ── Layout ────────────────────────────────────────────────────
    // Reserve space for title at top and text annotations at bottom.
    let title_h = text_height_2x() + 8;
    let plot_top = ry + title_h + 2;
    let plot_h = (rh - title_h - 8).max(90);
    let plot_w = rw - 16;

    // Centre of the hodograph plot (in canvas coords)
    let cx = rx + rw / 2;
    let cy = plot_top + plot_h / 2;
    let label_bounds = (rx + 6, plot_top + 6, rw - 12, plot_h - 12);

    // Determine scale: fit the largest speed ring (80 kt) within the plot,
    // leaving a small margin for labels.
    let max_ring = SPEED_RINGS.last().copied().unwrap_or(80.0);
    let max_radius = ((plot_w.min(plot_h)) / 2 - 8).max(30);
    let scale = max_radius as f64 / max_ring;

    // Closure: convert (u, v) in knots to screen (sx, sy).
    let uv_to_screen = |u: f64, v: f64| -> (i32, i32) {
        (
            (cx as f64 + u * scale) as i32,
            (cy as f64 - v * scale) as i32,
        )
    };

    // ── Title (2x scale) ─────────────────────────────────────────
    let title = "Hodograph (kts)";
    let title_w = text_width_2x(title);
    let title_x = rx + (rw - title_w) / 2;
    draw_text_2x(c, title, title_x, ry + 3, COL_TEXT_HEADER);

    // ── Speed rings (subtle) ─────────────────────────────────────
    for &kt in SPEED_RINGS {
        let r = (kt * scale) as i32;
        c.draw_circle(cx, cy, r, COL_RING);
        // Label on the right side of each ring — 2x scale for readability
        let label = format!("{:.0}", kt);
        let lx = cx + r + 4;
        let ly = cy - FONT_H; // vertically centred on ring
        draw_text_2x_with_bg_clamped(c, &label, lx, ly, COL_RING_LABEL, label_bounds);
    }

    // ── Axis lines (u = 0, v = 0) ────────────────────────────────
    let axis_extent = (max_ring * scale) as i32;
    c.draw_line(cx - axis_extent, cy, cx + axis_extent, cy, COL_AXIS);
    c.draw_line(cx, cy - axis_extent, cx, cy + axis_extent, COL_AXIS);

    // ── Wind trace (thick, anti-aliased) ──────────────────────────
    if data.winds.len() >= 2 {
        // Collect segments grouped by color band, then draw with polyline.
        let mut prev_sx: Option<(f64, f64)> = None;

        for w in &data.winds {
            let sx = cx as f64 + w.u_kts * scale;
            let sy = cy as f64 - w.v_kts * scale;
            let col = height_color(w.height_agl_m);

            if let Some((px, py)) = prev_sx {
                c.draw_thick_line_aa(px, py, sx, sy, col, TRACE_THICKNESS);
            }
            prev_sx = Some((sx, sy));
        }

        // ── Height dots at each whole km ──────────────────────────
        // White filled circles with dark km number inside.
        let max_h = data.winds.last().map(|w| w.height_agl_m).unwrap_or(0.0);
        let mut km = 0;
        while (km as f64) * 1000.0 <= max_h {
            let target_m = km as f64 * 1000.0;
            if let Some(w) = find_wind_at_height(&data.winds, target_m) {
                let (sx, sy) = uv_to_screen(w.u_kts, w.v_kts);
                let label_dot = matches!(km, 0 | 1 | 3 | 6 | 9 | 12);
                let dot_r = if label_dot { 7 } else { 4 };
                // White filled dot
                c.fill_circle(sx, sy, dot_r, COL_DOT_BG);
                // Dark border ring for contrast
                c.draw_circle(sx, sy, dot_r, height_color(target_m));
                if label_dot {
                    let label = format!("{}", km);
                    let tw = Canvas::text_width(&label);
                    c.draw_text(&label, sx - tw / 2, sy - SMALL_TEXT_H / 2, COL_DOT_FG);
                }
            }
            km += 1;
        }
    }

    // ── Bunkers RM marker (red filled circle, larger) ─────────────
    {
        let (sx, sy) = uv_to_screen(data.bunkers_rm.u_kts, data.bunkers_rm.v_kts);
        let marker_r = 6;
        c.fill_circle(sx, sy, marker_r, COL_RM);
        c.draw_circle(sx, sy, marker_r, [255, 255, 255, 200]);
        // Label with background
        let label = &data.bunkers_rm.label;
        let (lx, ly) = marker_label_xy(
            sx,
            sy,
            marker_r,
            Canvas::text_width(label),
            SMALL_TEXT_H,
            label_bounds.0,
            label_bounds.1,
            label_bounds.2,
            label_bounds.3,
        );
        draw_text_with_bg(c, label, lx, ly, COL_RM);
    }

    // ── Bunkers LM marker (blue filled triangle, larger) ─────────
    {
        let (sx, sy) = uv_to_screen(data.bunkers_lm.u_kts, data.bunkers_lm.v_kts);
        let sz = 7; // half-size of triangle
        c.fill_triangle(
            sx as f64,
            (sy - sz - 1) as f64,
            (sx - sz) as f64,
            (sy + sz) as f64,
            (sx + sz) as f64,
            (sy + sz) as f64,
            COL_LM,
        );
        c.draw_circle(sx, sy, sz, [255, 255, 255, 140]);
        // Label with background
        let label = &data.bunkers_lm.label;
        let (lx, ly) = marker_label_xy(
            sx,
            sy,
            sz,
            Canvas::text_width(label),
            SMALL_TEXT_H,
            label_bounds.0,
            label_bounds.1,
            label_bounds.2,
            label_bounds.3,
        );
        draw_text_with_bg(c, label, lx, ly, COL_LM);
    }

    // ── Mean wind marker (hollow circle + label with bg) ─────────
    {
        let (sx, sy) = uv_to_screen(data.mean_wind.u_kts, data.mean_wind.v_kts);
        c.draw_circle(sx, sy, 5, COL_MEAN);
        let label = &data.mean_wind.label;
        let (lx, ly) = marker_label_xy(
            sx,
            sy,
            5,
            Canvas::text_width(label),
            SMALL_TEXT_H,
            label_bounds.0,
            label_bounds.1,
            label_bounds.2,
            label_bounds.3,
        );
        draw_text_with_bg(c, label, lx, ly, COL_MEAN);
    }

    // ── Corfidi upshear vector ────────────────────────────────────
    {
        let (sx, sy) = uv_to_screen(data.corfidi_up.u_kts, data.corfidi_up.v_kts);
        // Draw larger X
        let xsz = 5;
        c.draw_line(sx - xsz, sy - xsz, sx + xsz, sy + xsz, COL_CORFIDI_UP);
        c.draw_line(sx - xsz, sy + xsz, sx + xsz, sy - xsz, COL_CORFIDI_UP);
        let label = &data.corfidi_up.label;
        draw_text_with_bg_clamped(
            c,
            label,
            sx + xsz + 3,
            sy - SMALL_TEXT_H / 2,
            COL_CORFIDI_UP,
            label_bounds,
        );
    }

    // ── Corfidi downshear vector ──────────────────────────────────
    {
        let (sx, sy) = uv_to_screen(data.corfidi_dn.u_kts, data.corfidi_dn.v_kts);
        // Draw larger +
        let psz = 5;
        c.draw_line(sx - psz, sy, sx + psz, sy, COL_CORFIDI_DN);
        c.draw_line(sx, sy - psz, sx, sy + psz, COL_CORFIDI_DN);
        let label = &data.corfidi_dn.label;
        draw_text_with_bg_clamped(
            c,
            label,
            sx + psz + 3,
            sy - SMALL_TEXT_H / 2,
            COL_CORFIDI_DN,
            label_bounds,
        );
    }

    // ── Storm-relative wind indicators ────────────────────────────
    if !data.sr_winds.is_empty() {
        let (rm_sx, rm_sy) = uv_to_screen(data.bunkers_rm.u_kts, data.bunkers_rm.v_kts);
        let sr_scale = 0.6;
        for (i, sr) in data.sr_winds.iter().enumerate() {
            let end_x = rm_sx as f64 + sr.sr_u_kts * scale * sr_scale;
            let end_y = rm_sy as f64 - sr.sr_v_kts * scale * sr_scale;
            c.draw_thick_line_aa(rm_sx as f64, rm_sy as f64, end_x, end_y, COL_SR_WIND, 2);
            draw_arrowhead(c, rm_sx as f64, rm_sy as f64, end_x, end_y, COL_SR_WIND);
            let label_offset = (i as i32 - 1) * (SMALL_TEXT_H + 2);
            let short_label = sr.label.replace(" KM SR", "");
            draw_text_with_bg_clamped(
                c,
                &short_label,
                end_x as i32 + 4,
                end_y as i32 - SMALL_TEXT_H / 2 + label_offset,
                COL_SR_WIND,
                label_bounds,
            );
        }
    }

    // ── Bottom text annotations ───────────────────────────────────
    let text_y_start = ry + rh - (text_height_2x() + 4) * 2 - 4;
    let line_h_2x = text_height_2x() + 4;
    let line_h = SMALL_TEXT_H + 4;
    let mut ty = text_y_start;

    // Critical angle (2x scale, prominent)
    let ca_str = format!("Critical Angle = {:.0} deg", data.critical_angle);
    let ca_col = if data.critical_angle >= 80.0 && data.critical_angle <= 120.0 {
        COL_TEXT_WARN // highlight near-optimal values
    } else {
        COL_TEXT
    };
    draw_text_2x(c, &ca_str, rx + 6, ty, ca_col);
    ty += line_h_2x;

    // Temperature advection (2x scale)
    let adv_str = if data.temperature_advection > 0.5 {
        "Temp Adv: WAA (WARM)"
    } else if data.temperature_advection < -0.5 {
        "Temp Adv: CAA (COLD)"
    } else {
        "Temp Adv: NEUTRAL"
    };
    let adv_col = if data.temperature_advection > 0.5 {
        COL_TEXT_WARN
    } else if data.temperature_advection < -0.5 {
        [100, 160, 255, 255]
    } else {
        COL_TEXT_DIM
    };
    draw_text_2x(c, adv_str, rx + 6, ty, adv_col);

    // ── Height legend ─────────────────────────────────────────────
    let legend_items: &[(&str, [u8; 4])] = &[
        ("0-1 KM", COL_0_1KM),
        ("1-3 KM", COL_1_3KM),
        ("3-6 KM", COL_3_6KM),
        ("6-9 KM", COL_6_9KM),
        ("9-12 KM", COL_9_12KM),
        ("12+ KM", COL_12_PLUS),
    ];

    // Layout legend in 2 columns x 3 rows, right of the annotations.
    let legend_x = rx + rw / 2 + 10;
    let legend_y = ry + rh - line_h * 3 - 8;
    let col_w = (rw / 2 - 20).max(80);
    for (i, &(label, col)) in legend_items.iter().enumerate() {
        let row = i % 3;
        let column = i / 3;
        let lx = legend_x + column as i32 * col_w / 2;
        let ly = legend_y + row as i32 * line_h;
        // Colour swatch
        c.fill_rect(lx, ly + 3, 12, SMALL_TEXT_H - 4, col);
        c.draw_text(label, lx + 16, ly, COL_TEXT_DIM);
    }
}

/// Render a standalone hodograph image and return the canvas.
///
/// Convenience wrapper that creates a canvas of the given size and fills it
/// with the hodograph.
pub fn render_hodograph(data: &HodographData, width: u32, height: u32) -> Canvas {
    let mut c = Canvas::new(width, height, COL_BG);
    draw_hodograph(&mut c, data, 0, 0, width as i32, height as i32);
    c
}

// =========================================================================
// Construction helpers — build HodographData from a Profile
// =========================================================================

/// Build [`HodographData`] directly from a sharprs [`Profile`].
///
/// This computes Bunkers motion, Corfidi vectors, mean wind, critical angle,
/// temperature advection, and storm-relative winds, returning a fully
/// populated `HodographData` ready for rendering.
pub fn hodograph_data_from_profile(
    prof: &crate::Profile,
) -> Result<HodographData, crate::SharpError> {
    use crate::winds;

    // ── Wind levels at 100 m spacing up to 14 km AGL ──────────────
    let sfc_h = prof.sfc_height();
    let max_agl = 14000.0_f64;
    let dh = 100.0;
    let mut wind_levels = Vec::new();
    let mut h = 0.0;
    while h <= max_agl {
        let msl = sfc_h + h;
        let p = prof.pres_at_height(msl);
        if p.is_finite() {
            let (u, v) = prof.interp_wind(p);
            if u.is_finite() && v.is_finite() {
                wind_levels.push(WindLevel {
                    height_agl_m: h,
                    u_kts: u,
                    v_kts: v,
                });
            }
        }
        h += dh;
    }

    // ── Bunkers storm motion ──────────────────────────────────────
    let (rstu, rstv, lstu, lstv) = winds::non_parcel_bunkers_motion(prof)?;
    let rm_label = format!("{} RM", format_dir_spd(rstu, rstv));
    let lm_label = format!("{} LM", format_dir_spd(lstu, lstv));

    // ── Corfidi vectors ───────────────────────────────────────────
    let (upu, upv, dnu, dnv) = winds::corfidi_mcs_motion(prof)?;
    let up_label = format!("UP={}", format_dir_spd(upu, upv));
    let dn_label = format!("DN={}", format_dir_spd(dnu, dnv));

    // ── Mean wind (0-6 km npw) ────────────────────────────────────
    let msl6 = prof.to_msl(6000.0);
    let p6 = prof.pres_at_height(msl6);
    let (mnu, mnv) = if p6.is_finite() {
        winds::mean_wind_npw(prof, prof.sfc_pressure(), p6, -1.0, 0.0, 0.0)?
    } else {
        (0.0, 0.0)
    };
    let mw_label = format!("MW={}", format_dir_spd(mnu, mnv));

    // ── Critical angle ────────────────────────────────────────────
    let ca = winds::critical_angle(prof, rstu, rstv).unwrap_or(0.0);

    // ── Temperature advection (inferred from 0-3 km veering) ──────
    let temp_adv = infer_temp_advection(&wind_levels);

    // ── Storm-relative winds ──────────────────────────────────────
    let mut sr_winds = Vec::new();
    let sr_layers: &[(f64, f64, &str)] = &[
        (0.0, 2000.0, "0-2 KM SR"),
        (4000.0, 6000.0, "4-6 KM SR"),
        (8000.0, 10000.0, "8-10 KM SR"),
    ];
    for &(bot_m, top_m, label) in sr_layers {
        let p_bot = prof.pres_at_height(prof.to_msl(bot_m));
        let p_top = prof.pres_at_height(prof.to_msl(top_m));
        if p_bot.is_finite() && p_top.is_finite() {
            if let Ok((su, sv)) = winds::mean_wind_npw(prof, p_bot, p_top, -1.0, rstu, rstv) {
                sr_winds.push(SRWindLayer {
                    sr_u_kts: su,
                    sr_v_kts: sv,
                    label: label.to_string(),
                });
            }
        }
    }

    Ok(HodographData {
        winds: wind_levels,
        bunkers_rm: StormMotion {
            u_kts: rstu,
            v_kts: rstv,
            label: rm_label,
        },
        bunkers_lm: StormMotion {
            u_kts: lstu,
            v_kts: lstv,
            label: lm_label,
        },
        mean_wind: StormMotion {
            u_kts: mnu,
            v_kts: mnv,
            label: mw_label,
        },
        corfidi_up: CorfidiVector {
            u_kts: upu,
            v_kts: upv,
            label: up_label,
        },
        corfidi_dn: CorfidiVector {
            u_kts: dnu,
            v_kts: dnv,
            label: dn_label,
        },
        critical_angle: ca,
        temperature_advection: temp_adv,
        sr_winds,
    })
}

// =========================================================================
// Private helpers
// =========================================================================

/// Find the wind level closest to a target height (metres AGL).
fn find_wind_at_height(winds: &[WindLevel], target_m: f64) -> Option<WindLevel> {
    if winds.is_empty() {
        return None;
    }

    // Try to interpolate between bounding levels first.
    for i in 0..winds.len().saturating_sub(1) {
        let a = &winds[i];
        let b = &winds[i + 1];
        if (a.height_agl_m <= target_m && b.height_agl_m >= target_m)
            || (b.height_agl_m <= target_m && a.height_agl_m >= target_m)
        {
            let dh = b.height_agl_m - a.height_agl_m;
            if dh.abs() < 1.0 {
                return Some(*a);
            }
            let frac = (target_m - a.height_agl_m) / dh;
            return Some(WindLevel {
                height_agl_m: target_m,
                u_kts: a.u_kts + frac * (b.u_kts - a.u_kts),
                v_kts: a.v_kts + frac * (b.v_kts - a.v_kts),
            });
        }
    }

    // Fall back to nearest level within 200 m.
    let mut best = winds[0];
    let mut best_diff = (best.height_agl_m - target_m).abs();
    for w in winds {
        let diff = (w.height_agl_m - target_m).abs();
        if diff < best_diff {
            best = *w;
            best_diff = diff;
        }
    }
    if best_diff < 200.0 {
        Some(best)
    } else {
        None
    }
}

/// Draw a small arrowhead at the tip of a line from (x0,y0) to (x1,y1).
fn draw_arrowhead(c: &mut Canvas, x0: f64, y0: f64, x1: f64, y1: f64, col: [u8; 4]) {
    let dx = x1 - x0;
    let dy = y1 - y0;
    let len = (dx * dx + dy * dy).sqrt();
    if len < 2.0 {
        return;
    }
    let ux = dx / len;
    let uy = dy / len;
    let arrow_len = 8.0;
    let arrow_half_w = 4.0;
    let bx = x1 - ux * arrow_len;
    let by = y1 - uy * arrow_len;
    let px = -uy * arrow_half_w;
    let py = ux * arrow_half_w;
    c.draw_line_aa(x1, y1, bx + px, by + py, col);
    c.draw_line_aa(x1, y1, bx - px, by - py, col);
}

/// Infer temperature advection from wind veering in the 0-3 km layer.
///
/// Returns a positive value for veering (WAA), negative for backing (CAA),
/// or near-zero for neutral.  The magnitude is the total direction change
/// in degrees (positive = clockwise with height).
fn infer_temp_advection(winds: &[WindLevel]) -> f64 {
    let sfc_winds: Vec<&WindLevel> = winds.iter().filter(|w| w.height_agl_m < 100.0).collect();
    let upper_winds: Vec<&WindLevel> = winds
        .iter()
        .filter(|w| (w.height_agl_m - 3000.0).abs() < 200.0)
        .collect();

    if sfc_winds.is_empty() || upper_winds.is_empty() {
        return 0.0;
    }

    let sfc = sfc_winds[0];
    let upper = upper_winds.last().unwrap();

    let (dir_sfc, _) = comp2vec(sfc.u_kts, sfc.v_kts);
    let (dir_upper, _) = comp2vec(upper.u_kts, upper.v_kts);

    let mut diff = dir_upper - dir_sfc;
    if diff > 180.0 {
        diff -= 360.0;
    }
    if diff < -180.0 {
        diff += 360.0;
    }

    diff
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_data() -> HodographData {
        let mut winds = Vec::new();
        for km in 0..=12 {
            let h = km as f64 * 1000.0;
            let frac = (h / 12000.0).min(1.0);
            let dir = 180.0 + frac * 100.0;
            let spd = 10.0 + frac * 50.0;
            let rad = dir.to_radians();
            let u = -spd * rad.sin();
            let v = -spd * rad.cos();
            winds.push(WindLevel {
                height_agl_m: h,
                u_kts: u,
                v_kts: v,
            });
        }

        HodographData {
            winds,
            bunkers_rm: StormMotion {
                u_kts: 15.0,
                v_kts: -5.0,
                label: "281/38 RM".to_string(),
            },
            bunkers_lm: StormMotion {
                u_kts: -10.0,
                v_kts: 8.0,
                label: "247/52 LM".to_string(),
            },
            mean_wind: StormMotion {
                u_kts: 5.0,
                v_kts: 2.0,
                label: "MW=260/25".to_string(),
            },
            corfidi_up: CorfidiVector {
                u_kts: -8.0,
                v_kts: 12.0,
                label: "UP=320/38".to_string(),
            },
            corfidi_dn: CorfidiVector {
                u_kts: 20.0,
                v_kts: -15.0,
                label: "DN=296/85".to_string(),
            },
            critical_angle: 72.0,
            temperature_advection: 35.0,
            sr_winds: vec![SRWindLayer {
                sr_u_kts: 10.0,
                sr_v_kts: 15.0,
                label: "0-2 KM SR".to_string(),
            }],
        }
    }

    #[test]
    fn test_render_does_not_panic() {
        let data = make_test_data();
        let canvas = render_hodograph(&data, 400, 400);
        assert_eq!(canvas.w, 400);
        assert_eq!(canvas.h, 400);
        assert_eq!(canvas.pixels.len(), 400 * 400 * 4);
    }

    #[test]
    fn test_draw_into_canvas_region() {
        let data = make_test_data();
        let mut canvas = Canvas::new(800, 600, [0, 0, 0, 255]);
        draw_hodograph(&mut canvas, &data, 400, 0, 400, 300);
        assert_eq!(canvas.pixels.len(), 800 * 600 * 4);
    }

    #[test]
    fn test_height_color_bands() {
        assert_eq!(height_color(0.0), COL_0_1KM);
        assert_eq!(height_color(500.0), COL_0_1KM);
        assert_eq!(height_color(1500.0), COL_1_3KM);
        assert_eq!(height_color(4000.0), COL_3_6KM);
        assert_eq!(height_color(7000.0), COL_6_9KM);
        assert_eq!(height_color(10000.0), COL_9_12KM);
        assert_eq!(height_color(13000.0), COL_12_PLUS);
    }

    #[test]
    fn test_comp2vec_roundtrip() {
        let (dir, spd) = comp2vec(10.0, -5.0);
        assert!(spd > 0.0);
        assert!(dir >= 0.0 && dir < 360.0);
    }

    #[test]
    fn test_find_wind_at_height_interpolates() {
        let winds = vec![
            WindLevel {
                height_agl_m: 0.0,
                u_kts: 0.0,
                v_kts: -10.0,
            },
            WindLevel {
                height_agl_m: 1000.0,
                u_kts: 10.0,
                v_kts: -10.0,
            },
        ];
        let w = find_wind_at_height(&winds, 500.0).unwrap();
        assert!((w.u_kts - 5.0).abs() < 0.1);
        assert!((w.v_kts - (-10.0)).abs() < 0.1);
    }

    #[test]
    fn test_format_dir_spd() {
        let s = format_dir_spd(10.0, 0.0);
        assert!(s.contains("270"));
    }

    #[test]
    fn test_infer_temp_advection_veering() {
        let winds = vec![
            WindLevel {
                height_agl_m: 0.0,
                u_kts: 0.0,
                v_kts: 10.0,
            },
            WindLevel {
                height_agl_m: 3000.0,
                u_kts: 20.0,
                v_kts: 0.0,
            },
        ];
        let adv = infer_temp_advection(&winds);
        assert!(adv > 0.0, "veering should indicate WAA, got {}", adv);
    }

    #[test]
    fn test_infer_temp_advection_backing() {
        let winds = vec![
            WindLevel {
                height_agl_m: 0.0,
                u_kts: 20.0,
                v_kts: 0.0,
            },
            WindLevel {
                height_agl_m: 3000.0,
                u_kts: 0.0,
                v_kts: 10.0,
            },
        ];
        let adv = infer_temp_advection(&winds);
        assert!(adv < 0.0, "backing should indicate CAA, got {}", adv);
    }

    #[test]
    fn test_empty_winds_no_panic() {
        let data = HodographData {
            winds: vec![],
            bunkers_rm: StormMotion {
                u_kts: 0.0,
                v_kts: 0.0,
                label: String::new(),
            },
            bunkers_lm: StormMotion {
                u_kts: 0.0,
                v_kts: 0.0,
                label: String::new(),
            },
            mean_wind: StormMotion {
                u_kts: 0.0,
                v_kts: 0.0,
                label: String::new(),
            },
            corfidi_up: CorfidiVector {
                u_kts: 0.0,
                v_kts: 0.0,
                label: String::new(),
            },
            corfidi_dn: CorfidiVector {
                u_kts: 0.0,
                v_kts: 0.0,
                label: String::new(),
            },
            critical_angle: 0.0,
            temperature_advection: 0.0,
            sr_winds: vec![],
        };
        let canvas = render_hodograph(&data, 300, 300);
        assert_eq!(canvas.w, 300);
    }
}
