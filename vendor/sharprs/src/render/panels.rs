//! Right-side diagnostic panels for the sounding display.
//!
//! Six specialised panels that complement the Skew-T / hodograph:
//!
//! 1. **SARS – Sounding Analogs** — shows supercell and significant-hail
//!    analog matches with percentage breakdowns.
//! 2. **Effective Layer STP box-and-whisker** — climatological STP
//!    distributions by EF-scale category with the current sounding's value.
//! 3. **Storm Slinky** — updraft parcel trajectory through the hodograph.
//! 4. **Possible Hazard Type** — large color-coded label from the watch-type
//!    classifier.
//! 5. **Inferred Temperature Advection** — small panel showing inferred
//!    temperature advection profile.
//!
//! Every function renders into a caller-supplied sub-region of a [`Canvas`].

use super::Canvas;
use crate::watch_type::WatchType;

// =========================================================================
// Color constants (match the existing palette from the Skew-T renderer)
// =========================================================================

/// Main background color (used when clearing sub-panels).
#[allow(dead_code)]
const COL_BG: [u8; 4] = [0, 0, 0, 255];
const COL_PANEL_BG: [u8; 4] = [0, 0, 0, 255];
const COL_PANEL_BORDER: [u8; 4] = [215, 215, 215, 255];
const COL_TEXT: [u8; 4] = [230, 230, 230, 255];
const COL_TEXT_DIM: [u8; 4] = [140, 140, 150, 255];
const COL_WHITE: [u8; 4] = [255, 255, 255, 255];
const COL_CYAN: [u8; 4] = [0, 255, 255, 255];
#[allow(dead_code)]
const COL_YELLOW: [u8; 4] = [255, 255, 0, 255];

// Hazard-type colors
const COL_HAZ_NONE: [u8; 4] = [60, 180, 60, 255];
const COL_HAZ_MRGL_SVR: [u8; 4] = [180, 180, 50, 255];
const COL_HAZ_SVR: [u8; 4] = [220, 160, 30, 255];
const COL_HAZ_MRGL_TOR: [u8; 4] = [230, 120, 40, 255];
const COL_HAZ_TOR: [u8; 4] = [220, 40, 40, 255];
const COL_HAZ_PDS_TOR: [u8; 4] = [255, 40, 220, 255];

// Box-plot colors
const COL_BOX_FILL: [u8; 4] = [60, 100, 160, 160];
const COL_BOX_BORDER: [u8; 4] = [100, 150, 220, 255];
const COL_BOX_MEDIAN: [u8; 4] = [255, 255, 255, 255];
const COL_BOX_WHISKER: [u8; 4] = [140, 140, 160, 255];
const COL_STP_MARKER: [u8; 4] = [255, 60, 60, 255];

// Slinky dot colors (height-banded, matching hodograph convention)
const COL_SLINKY_LOW: [u8; 4] = [255, 80, 80, 255]; // 0-3 km — brighter red
const COL_SLINKY_MID: [u8; 4] = [80, 255, 80, 255]; // 3-6 km — brighter green
const COL_SLINKY_HIGH: [u8; 4] = [80, 160, 255, 255]; // 6-9 km — brighter blue
const COL_SLINKY_UPPER: [u8; 4] = [200, 100, 255, 255]; // 9+ km — brighter purple

// Temperature advection colors
const COL_WARM_ADV: [u8; 4] = [220, 80, 40, 255];
const COL_COLD_ADV: [u8; 4] = [60, 120, 220, 255];
const COL_NEUTRAL_ADV: [u8; 4] = [140, 140, 140, 255];

// Font metrics (matching the 7x10 bitmap font)
const FONT_W: i32 = 7;
const FONT_H: i32 = 10;
#[allow(dead_code)]
const CHAR_SPACING: i32 = 8; // FONT_W + 1
const LINE_H: i32 = 13; // FONT_H + 3
const TITLE_SCALE: i32 = 2;
const TITLE_H: i32 = FONT_H * TITLE_SCALE;

fn draw_panel_title(canvas: &mut Canvas, title: &str, rx: i32, ry: i32, rw: i32) -> i32 {
    let tw = Canvas::text_width_scaled(title, TITLE_SCALE);
    draw_text_scaled(
        canvas,
        title,
        rx + (rw - tw) / 2,
        ry + 5,
        COL_CYAN,
        TITLE_SCALE,
    );
    let sep_y = ry + TITLE_H + 10;
    canvas.draw_line(rx + 2, sep_y, rx + rw - 2, sep_y, COL_PANEL_BORDER);
    sep_y
}

// =========================================================================
// 1. SARS – Sounding Analogs
// =========================================================================

/// Analog match data for a single SARS category (supercell or sig-hail).
#[derive(Debug, Clone)]
pub struct SarsCategory {
    /// Number of loose matches found.
    pub loose_matches: u32,
    /// Number of quality (close) matches found.
    pub quality_matches: u32,
    /// Lines of text for quality-match details (e.g. "SARS: 44% TOR").
    /// Empty if no quality matches.
    pub quality_lines: Vec<String>,
}

/// Complete SARS panel data.
#[derive(Debug, Clone)]
pub struct SarsData {
    pub supercell: SarsCategory,
    pub sgfnt_hail: SarsCategory,
}

impl SarsData {
    /// Placeholder data for when no SARS database is available.
    pub fn placeholder() -> Self {
        Self {
            supercell: SarsCategory {
                loose_matches: 0,
                quality_matches: 0,
                quality_lines: Vec::new(),
            },
            sgfnt_hail: SarsCategory {
                loose_matches: 0,
                quality_matches: 0,
                quality_lines: Vec::new(),
            },
        }
    }
}

/// Render the SARS – Sounding Analogs panel.
///
/// Draws into the rectangle `(rx, ry, rw, rh)` on `canvas`.
///
/// # Layout
///
/// ```text
/// ┌──────── SARS - Sounding Analogs ────────┐
/// │  SUPERCELL         │  SGFNT HAIL         │
/// │  (N loose matches) │  (N loose matches)  │
/// │                    │                     │
/// │  SARS: 44% TOR     │  No Quality Matches │
/// │  SARS: 0% SIG      │                     │
/// └────────────────────┴─────────────────────┘
/// ```
// TODO: Implement actual SARS matching algorithm using a climatology
// database of historical sounding analogs. The algorithm should:
//   1. Compare the current sounding's parameter space (CAPE, shear, SRH,
//      LCL, etc.) against a database of ~30,000 proximity soundings.
//   2. "Loose" matches use wider tolerance bands; "quality" matches use
//      tighter bands matching SPC operational thresholds.
//   3. From quality matches, compute the percentage that were associated
//      with tornadoes (TOR), significant tornadoes (SIG), and significant
//      hail (SIG HAIL).
//   4. The database would be embedded as a binary resource or loaded from
//      a companion .dat file.
pub fn draw_sars_panel(canvas: &mut Canvas, data: &SarsData, rx: i32, ry: i32, rw: i32, rh: i32) {
    // Panel background
    canvas.fill_rect(rx, ry, rw, rh, COL_PANEL_BG);

    // Cyan border (matching reference)
    draw_panel_border(canvas, rx, ry, rw, rh);

    let sep_y = draw_panel_title(canvas, "SARS - Sounding Analogs", rx, ry, rw);

    // Dividing line between columns
    let mid_x = rx + rw / 2;
    canvas.draw_line(mid_x, sep_y, mid_x, ry + rh - 2, COL_PANEL_BORDER);

    let col_w = rw / 2;
    let content_y = sep_y + 8;

    // Draw left column: SUPERCELL
    draw_sars_column(
        canvas,
        &data.supercell,
        "SUPERCELL",
        rx + 6,
        content_y,
        col_w - 12,
    );

    // Draw right column: SGFNT HAIL
    draw_sars_column(
        canvas,
        &data.sgfnt_hail,
        "SGFNT HAIL",
        mid_x + 6,
        content_y,
        col_w - 12,
    );
}

fn draw_sars_column(
    canvas: &mut Canvas,
    cat: &SarsCategory,
    heading: &str,
    x: i32,
    y: i32,
    _w: i32,
) {
    // Column heading in white, slightly larger effect with bold
    draw_text_scaled(canvas, heading, x, y, COL_WHITE, 2);

    // Match count
    let mut cy = y + TITLE_H + 5;
    let match_text = format!("({} loose matches)", cat.loose_matches);
    canvas.draw_text(&match_text, x, cy, COL_TEXT_DIM);
    cy += LINE_H + 6;

    if cat.quality_matches == 0 {
        canvas.draw_text("No Quality Matches", x, cy, COL_TEXT_DIM);
    } else {
        let qm_text = format!("({} quality matches)", cat.quality_matches);
        canvas.draw_text(&qm_text, x, cy, COL_TEXT);
        cy += LINE_H + 2;
        for line in &cat.quality_lines {
            canvas.draw_text(line, x, cy, COL_CYAN);
            // Bold the percentage lines
            canvas.draw_text(line, x + 1, cy, COL_CYAN);
            cy += LINE_H;
        }
    }
}

// =========================================================================
// 2. Effective Layer STP Box-and-Whisker Plot
// =========================================================================

/// STP climatology for a single EF-scale category.
///
/// Values are the box-and-whisker statistics: minimum (10th-pctl whisker),
/// first quartile, median, third quartile, maximum (90th-pctl whisker).
#[derive(Debug, Clone, Copy)]
pub struct StpBoxStats {
    pub whisker_lo: f64,
    pub q1: f64,
    pub median: f64,
    pub q3: f64,
    pub whisker_hi: f64,
}

/// Full STP climatology across EF-scale categories.
///
/// Values sourced from Thompson et al. (2012, WAF) and SPC operational
/// documentation for the effective-layer STP (with CIN) distributions
/// from the significant tornado environment database.
#[derive(Debug, Clone)]
pub struct StpClimatology {
    /// EF4+ tornadoes
    pub ef4_plus: StpBoxStats,
    /// EF3 tornadoes
    pub ef3: StpBoxStats,
    /// EF2 tornadoes
    pub ef2: StpBoxStats,
    /// EF1 tornadoes
    pub ef1: StpBoxStats,
    /// EF0 tornadoes
    pub ef0: StpBoxStats,
    /// Non-tornadic supercells
    pub nontor: StpBoxStats,
}

impl StpClimatology {
    /// Standard STP climatology values from SPC publications.
    ///
    /// These are approximate values from Thompson et al. (2012) Figure 4
    /// and the SPC mesoanalysis climatology tables for the effective-layer
    /// STP (with CIN constraint) distributions.
    pub fn standard() -> Self {
        Self {
            ef4_plus: StpBoxStats {
                whisker_lo: 2.0,
                q1: 4.0,
                median: 6.5,
                q3: 9.0,
                whisker_hi: 11.0,
            },
            ef3: StpBoxStats {
                whisker_lo: 1.5,
                q1: 3.0,
                median: 5.0,
                q3: 7.5,
                whisker_hi: 10.0,
            },
            ef2: StpBoxStats {
                whisker_lo: 1.0,
                q1: 2.0,
                median: 3.5,
                q3: 5.5,
                whisker_hi: 8.0,
            },
            ef1: StpBoxStats {
                whisker_lo: 0.3,
                q1: 1.0,
                median: 2.0,
                q3: 3.5,
                whisker_hi: 6.0,
            },
            ef0: StpBoxStats {
                whisker_lo: 0.0,
                q1: 0.3,
                median: 1.0,
                q3: 2.0,
                whisker_hi: 4.0,
            },
            nontor: StpBoxStats {
                whisker_lo: 0.0,
                q1: 0.0,
                median: 0.2,
                q3: 0.8,
                whisker_hi: 2.0,
            },
        }
    }
}

/// Probability text lines for the right side of the STP panel.
#[derive(Debug, Clone)]
pub struct StpProbabilities {
    /// e.g. "29%"
    pub prob_ef2_plus: Option<String>,
    /// e.g. "based on CAPE: 22%"
    pub based_on_cape: Option<String>,
    /// e.g. "based on LCL: 15%"
    pub based_on_lcl: Option<String>,
    /// e.g. "based on SRH: 35%"
    pub based_on_srh: Option<String>,
    /// e.g. "based on shear: 28%"
    pub based_on_shear: Option<String>,
}

/// Render the Effective Layer STP box-and-whisker panel.
///
/// # Arguments
///
/// * `canvas` — target canvas
/// * `climo` — STP climatology data (box-plot statistics per EF category)
/// * `current_stp` — the current sounding's effective-layer STP value;
///   drawn as a horizontal marker line across the plot
/// * `probs` — optional probability breakdown text for the right margin
/// * `(rx, ry, rw, rh)` — panel bounding box
pub fn draw_stp_box_panel(
    canvas: &mut Canvas,
    climo: &StpClimatology,
    current_stp: f64,
    probs: Option<&StpProbabilities>,
    rx: i32,
    ry: i32,
    rw: i32,
    rh: i32,
) {
    // Panel background
    canvas.fill_rect(rx, ry, rw, rh, COL_PANEL_BG);
    draw_panel_border(canvas, rx, ry, rw, rh);

    let sep_y = draw_panel_title(canvas, "Effective Layer STP (with CIN)", rx, ry, rw);

    // Plot area (leave margins for axes and probability text)
    let prob_margin = if probs.is_some() { rw / 3 } else { 20 };
    let plot_left = rx + 32;
    let plot_right = rx + rw - 10 - prob_margin;
    let plot_top = sep_y + 8;
    let plot_bot = ry + rh - FONT_H - 12;
    let plot_w = plot_right - plot_left;
    let plot_h = plot_bot - plot_top;

    // Y-axis: STP values 0 to 11
    let stp_min = 0.0_f64;
    let stp_max = 11.0_f64;

    let stp_to_y = |stp: f64| -> i32 {
        let frac = (stp - stp_min) / (stp_max - stp_min);
        plot_bot - (frac * plot_h as f64) as i32
    };

    // Y-axis grid lines and labels
    for stp_val in 0..=11 {
        let y = stp_to_y(stp_val as f64);
        let col = if stp_val % 2 == 0 {
            [40, 40, 60, 255]
        } else {
            [30, 30, 45, 255]
        };
        canvas.draw_line(plot_left, y, plot_right, y, col);
        if stp_val % 2 == 0 {
            let label = format!("{}", stp_val);
            canvas.draw_text_right(&label, plot_left - 4, y - FONT_H / 2, COL_TEXT);
        }
    }

    // Category labels and box plots
    let categories: &[(&str, StpBoxStats)] = &[
        ("EF4+", climo.ef4_plus),
        ("EF3", climo.ef3),
        ("EF2", climo.ef2),
        ("EF1", climo.ef1),
        ("EF0", climo.ef0),
        ("NONTOR", climo.nontor),
    ];

    let n_cats = categories.len() as i32;
    let box_spacing = plot_w / n_cats;
    let box_w = (box_spacing as f64 * 0.55) as i32;

    for (i, (label, stats)) in categories.iter().enumerate() {
        let cx = plot_left + box_spacing * i as i32 + box_spacing / 2;
        let bx = cx - box_w / 2;

        // Whiskers (vertical line from whisker_lo to whisker_hi)
        let whi_y = stp_to_y(stats.whisker_hi.min(stp_max));
        let wlo_y = stp_to_y(stats.whisker_lo.max(stp_min));
        canvas.draw_line(cx, whi_y, cx, wlo_y, COL_BOX_WHISKER);

        // Whisker caps (small horizontal ticks)
        let cap_half = box_w / 3;
        canvas.draw_line(cx - cap_half, whi_y, cx + cap_half, whi_y, COL_BOX_WHISKER);
        canvas.draw_line(cx - cap_half, wlo_y, cx + cap_half, wlo_y, COL_BOX_WHISKER);

        // Box (Q1 to Q3) — filled with visible quartile box
        let q1_y = stp_to_y(stats.q1.max(stp_min).min(stp_max));
        let q3_y = stp_to_y(stats.q3.max(stp_min).min(stp_max));
        let box_h = q1_y - q3_y; // q1_y > q3_y since lower STP = higher y
        canvas.fill_rect(bx, q3_y, box_w, box_h.max(1), COL_BOX_FILL);
        canvas.draw_rect(bx, q3_y, box_w, box_h.max(1), COL_BOX_BORDER);

        // Median line (thicker for visibility)
        let med_y = stp_to_y(stats.median.max(stp_min).min(stp_max));
        canvas.draw_line(bx, med_y, bx + box_w, med_y, COL_BOX_MEDIAN);
        canvas.draw_line(bx, med_y + 1, bx + box_w, med_y + 1, COL_BOX_MEDIAN);

        // X-axis label in white for visibility
        let lw = Canvas::text_width(label);
        canvas.draw_text(label, cx - lw / 2, plot_bot + 4, COL_WHITE);
    }

    // Current STP value — prominent horizontal marker line across the full plot
    if current_stp.is_finite() && current_stp >= stp_min {
        let marker_y = stp_to_y(current_stp.min(stp_max));

        // Thick dashed marker line (3 pixels thick for prominence)
        for dy in -1..=1_i32 {
            let mut dx = plot_left;
            while dx < plot_right {
                let end = (dx + 8).min(plot_right);
                canvas.draw_line(dx, marker_y + dy, end, marker_y + dy, COL_STP_MARKER);
                dx += 12;
            }
        }

        // Label the value with bold effect
        let val_text = format!("{:.1}", current_stp);
        canvas.draw_text(
            &val_text,
            plot_right + 4,
            marker_y - FONT_H / 2,
            COL_STP_MARKER,
        );
        canvas.draw_text(
            &val_text,
            plot_right + 5,
            marker_y - FONT_H / 2,
            COL_STP_MARKER,
        );
    }

    // Probability text on the right side
    if let Some(p) = probs {
        let prob_x = plot_right + 12;
        let mut py = plot_top + 4;

        canvas.draw_text("Prob EF2+ torn", prob_x, py, COL_TEXT_DIM);
        py += LINE_H;
        canvas.draw_text("with supercell", prob_x, py, COL_TEXT_DIM);
        py += LINE_H + 4;

        if let Some(ref val) = p.prob_ef2_plus {
            // Draw the probability percentage in large white text
            draw_text_scaled(canvas, val, prob_x, py, COL_WHITE, 2);
            py += FONT_H * 2 + 6;
        }

        if let Some(ref val) = p.based_on_cape {
            canvas.draw_text(val, prob_x, py, COL_TEXT);
            py += LINE_H;
        }
        if let Some(ref val) = p.based_on_lcl {
            canvas.draw_text(val, prob_x, py, COL_TEXT);
            py += LINE_H;
        }
        if let Some(ref val) = p.based_on_srh {
            canvas.draw_text(val, prob_x, py, COL_TEXT);
            py += LINE_H;
        }
        if let Some(ref val) = p.based_on_shear {
            canvas.draw_text(val, prob_x, py, COL_TEXT);
        }
    }
}

// =========================================================================
// 3. Storm Slinky
// =========================================================================

/// A single point in the storm slinky trajectory.
#[derive(Debug, Clone, Copy)]
pub struct SlinkyPoint {
    /// Height AGL in meters.
    pub height_m: f64,
    /// Storm-relative U displacement (kt).
    pub sr_u: f64,
    /// Storm-relative V displacement (kt).
    pub sr_v: f64,
}

/// Render the Storm Slinky panel.
///
/// The "slinky" shows how an updraft parcel would be displaced at each
/// altitude by the storm-relative winds.  Each dot represents 1 km of
/// altitude, plotted in storm-relative (u, v) space, color-coded by
/// height band.
///
/// # Arguments
///
/// * `canvas` — target canvas
/// * `points` — slinky trajectory points (one per km, from surface up)
/// * `tilt_deg` — optional tilt angle in degrees for the degree label
/// * `(rx, ry, rw, rh)` — panel bounding box
pub fn draw_storm_slinky(
    canvas: &mut Canvas,
    points: &[SlinkyPoint],
    tilt_deg: Option<f64>,
    rx: i32,
    ry: i32,
    rw: i32,
    rh: i32,
) {
    // Panel background
    canvas.fill_rect(rx, ry, rw, rh, COL_PANEL_BG);
    draw_panel_border(canvas, rx, ry, rw, rh);

    let sep_y = draw_panel_title(canvas, "Storm Slinky", rx, ry, rw);

    if points.is_empty() {
        canvas.draw_text("No Data", rx + rw / 2 - 24, ry + rh / 2, COL_TEXT_DIM);
        return;
    }

    // Plot area
    let plot_margin = 28;
    let plot_top = sep_y + 8;
    let plot_size = (rw - 2 * plot_margin).min(rh - (plot_top - ry) - plot_margin - 8);

    let cx = rx + rw / 2;
    let cy = plot_top + plot_size / 2;

    // Find the maximum displacement to set the scale
    let max_disp = points
        .iter()
        .map(|p| (p.sr_u * p.sr_u + p.sr_v * p.sr_v).sqrt())
        .fold(0.0_f64, f64::max)
        .max(4.0);

    let scale = (plot_size as f64 / 2.0 - 8.0) / max_disp;

    // Crosshairs (dim)
    canvas.draw_line(
        cx - plot_size / 2,
        cy,
        cx + plot_size / 2,
        cy,
        [45, 45, 65, 255],
    );
    canvas.draw_line(cx, plot_top, cx, plot_top + plot_size, [45, 45, 65, 255]);

    // Reference rings at 1/3 and 2/3 of the max displacement
    for frac in &[0.33, 0.66] {
        let ring_r = ((max_disp * frac) * scale) as i32;
        if ring_r > 4 {
            canvas.draw_circle(cx, cy, ring_r, [40, 40, 60, 255]);
        }
    }

    let scale_text = format!("{:.0} kt radius", max_disp);
    canvas.draw_text_scaled(&scale_text, rx + 12, plot_top + 4, COL_TEXT_DIM, 2);

    // Draw connecting lines between successive dots (brighter)
    for pair in points.windows(2) {
        let x0 = cx + (pair[0].sr_u * scale) as i32;
        let y0 = cy - (pair[0].sr_v * scale) as i32;
        let x1 = cx + (pair[1].sr_u * scale) as i32;
        let y1 = cy - (pair[1].sr_v * scale) as i32;
        canvas.draw_thick_line_aa(
            x0 as f64,
            y0 as f64,
            x1 as f64,
            y1 as f64,
            [120, 120, 150, 200],
            2,
        );
    }

    // Draw dots — LARGER (radius 5) and BRIGHTER
    let dot_radius: i32 = 7;
    for pt in points {
        let px = cx + (pt.sr_u * scale) as i32;
        let py_pt = cy - (pt.sr_v * scale) as i32;

        let col = slinky_color(pt.height_m);
        canvas.fill_circle(px, py_pt, dot_radius, col);
        // Bright outline for contrast
        canvas.draw_circle(px, py_pt, dot_radius, COL_WHITE);
    }

    // Degree label (tilt angle) — prominent if provided
    if let Some(deg) = tilt_deg {
        let deg_text = format!("{:.0} deg", deg);
        let dtw = Canvas::text_width_scaled(&deg_text, 2);
        // Draw at top-right of plot area
        canvas.draw_text_scaled(&deg_text, rx + rw - dtw - 12, plot_top + 4, COL_WHITE, 2);
    }

    // Height-band legend (bottom-left)
    let leg_x = rx + 6;
    let legend_line_h = 24;
    let leg_y = ry + rh - 4 * legend_line_h - 10;
    let bands: &[(&str, [u8; 4])] = &[
        ("0-3km", COL_SLINKY_LOW),
        ("3-6km", COL_SLINKY_MID),
        ("6-9km", COL_SLINKY_HIGH),
        ("9+km", COL_SLINKY_UPPER),
    ];
    for (i, (label, col)) in bands.iter().enumerate() {
        let ly = leg_y + i as i32 * legend_line_h;
        // Larger color swatch (filled circle to match dots)
        canvas.fill_circle(leg_x + 8, ly + 8, 5, *col);
        canvas.draw_text_scaled(label, leg_x + 22, ly + 2, COL_TEXT, 1);
    }
}

/// Return the appropriate color for a slinky dot based on its height AGL.
fn slinky_color(height_m: f64) -> [u8; 4] {
    if height_m < 3000.0 {
        COL_SLINKY_LOW
    } else if height_m < 6000.0 {
        COL_SLINKY_MID
    } else if height_m < 9000.0 {
        COL_SLINKY_HIGH
    } else {
        COL_SLINKY_UPPER
    }
}

/// Compute storm slinky points from profile wind data.
///
/// For each km from 0 to `max_km` AGL, this computes the cumulative
/// storm-relative displacement that an updraft parcel would experience.
///
/// # Arguments
///
/// * `wind_u_at_height` — closure that returns the environmental U-wind
///   (kt) at a given height AGL (m)
/// * `wind_v_at_height` — closure that returns the environmental V-wind
///   (kt) at a given height AGL (m)
/// * `storm_u` — storm motion U component (kt)
/// * `storm_v` — storm motion V component (kt)
/// * `max_km` — maximum height in km (typically 9 or 12)
pub fn compute_slinky_points(
    wind_u_at_height: impl Fn(f64) -> f64,
    wind_v_at_height: impl Fn(f64) -> f64,
    storm_u: f64,
    storm_v: f64,
    max_km: u32,
) -> Vec<SlinkyPoint> {
    let mut points = Vec::with_capacity(max_km as usize + 1);

    for km in 0..=max_km {
        let h = km as f64 * 1000.0;
        let env_u = wind_u_at_height(h);
        let env_v = wind_v_at_height(h);

        if !env_u.is_finite() || !env_v.is_finite() {
            continue;
        }

        let sr_u = env_u - storm_u;
        let sr_v = env_v - storm_v;

        points.push(SlinkyPoint {
            height_m: h,
            sr_u,
            sr_v,
        });
    }

    points
}

// =========================================================================
// 4. Possible Hazard Type
// =========================================================================

/// Render the Possible Hazard Type panel.
///
/// Displays a large color-coded label indicating the most severe hazard
/// type determined by the watch-type classifier.  The text is rendered
/// at 3x scale to be the most prominent element in this panel.
///
/// # Colors
///
/// | Watch Type      | Color                          |
/// |-----------------|--------------------------------|
/// | `NONE`          | Green                          |
/// | `MRGL SVR`      | Yellow                         |
/// | `SVR`           | Orange                         |
/// | `MRGL TOR`      | Dark orange                    |
/// | `TOR`           | Red                            |
/// | `PDS TOR`       | Magenta                        |
/// | `FLASH FLOOD`   | Blue                           |
/// | `BLIZZARD`      | Cyan                           |
/// | `EXCESSIVE HEAT`| Orange                         |
pub fn draw_hazard_type_panel(
    canvas: &mut Canvas,
    watch: WatchType,
    rx: i32,
    ry: i32,
    rw: i32,
    rh: i32,
) {
    // Panel background
    canvas.fill_rect(rx, ry, rw, rh, COL_PANEL_BG);
    draw_panel_border(canvas, rx, ry, rw, rh);

    let sep_y = draw_panel_title(canvas, "Possible Hazard Type", rx, ry, rw);

    // Hazard label
    let label = watch.label();
    let col = hazard_color(watch);

    // Content area
    let content_top = sep_y + 4;
    let content_h = rh - (content_top - ry) - 6;

    // Subtle background tint bar behind the text
    let bar_col = [col[0], col[1], col[2], 35];
    canvas.fill_rect(rx + 2, content_top, rw - 4, content_h, bar_col);

    // 3x scaled text for MAXIMUM prominence (the biggest text in the panel)
    let scale = 3;
    let scaled_char_w = FONT_W * scale + scale;
    let n_chars = label.len() as i32;
    let large_w = n_chars * scaled_char_w - scale;
    let large_h = FONT_H * scale;

    // If text is too wide at 3x, fall back to 2x
    let (final_scale, final_w, final_h) = if large_w > rw - 12 {
        let s2 = 2;
        let w2 = n_chars * (FONT_W * s2 + s2) - s2;
        let h2 = FONT_H * s2;
        (s2, w2, h2)
    } else {
        (scale, large_w, large_h)
    };

    let lx = rx + (rw - final_w) / 2;
    let ly = content_top + (content_h - final_h) / 2;

    // Draw at chosen scale with glow effect for prominence
    // Glow: draw slightly offset in a dimmer version
    let glow_col = [col[0], col[1], col[2], 80];
    draw_text_scaled(canvas, label, lx + 1, ly + 1, glow_col, final_scale);
    draw_text_scaled(canvas, label, lx - 1, ly - 1, glow_col, final_scale);
    // Main text
    draw_text_scaled(canvas, label, lx, ly, col, final_scale);
}

/// Draw text at Nx size by reading the bitmap for each character and
/// rendering each set pixel as an NxN block.  This produces a clean
/// scaled-up version of the 7x10 bitmap font.
fn draw_text_scaled(canvas: &mut Canvas, text: &str, x: i32, y: i32, col: [u8; 4], scale: i32) {
    canvas.draw_text_scaled(text, x, y, col, scale);
}

/// Return the display color for a given watch type.
fn hazard_color(w: WatchType) -> [u8; 4] {
    match w {
        WatchType::None => COL_HAZ_NONE,
        WatchType::MarginalSevere => COL_HAZ_MRGL_SVR,
        WatchType::Severe => COL_HAZ_SVR,
        WatchType::MarginalTornado => COL_HAZ_MRGL_TOR,
        WatchType::Tornado => COL_HAZ_TOR,
        WatchType::PdsTornado => COL_HAZ_PDS_TOR,
        WatchType::FlashFlood => [60, 100, 255, 255],
        WatchType::Blizzard => COL_CYAN,
        WatchType::ExcessiveHeat => COL_HAZ_SVR,
    }
}

// =========================================================================
// 5. Inferred Temperature Advection
// =========================================================================

/// A single level in the inferred temperature advection profile.
#[derive(Debug, Clone, Copy)]
pub struct TempAdvectionLevel {
    /// Pressure level (mb).
    pub pressure_mb: f64,
    /// Height AGL (m).
    pub height_m: f64,
    /// Inferred temperature advection (K/hr, positive = warm advection).
    pub advection_k_per_hr: f64,
}

/// Render the Inferred Temperature Advection panel.
///
/// Draws a small profile of inferred temperature advection with warm
/// advection in red and cold advection in blue.
///
/// # Arguments
///
/// * `canvas` — target canvas
/// * `levels` — advection profile levels from surface up
/// * `(rx, ry, rw, rh)` — panel bounding box
pub fn draw_temp_advection_panel(
    canvas: &mut Canvas,
    levels: &[TempAdvectionLevel],
    rx: i32,
    ry: i32,
    rw: i32,
    rh: i32,
) {
    // Panel background
    canvas.fill_rect(rx, ry, rw, rh, COL_PANEL_BG);
    draw_panel_border(canvas, rx, ry, rw, rh);

    let sep_y = draw_panel_title(canvas, "Inferred Temp Adv", rx, ry, rw);

    if levels.is_empty() {
        canvas.draw_text("N/A", rx + rw / 2 - 10, ry + rh / 2, COL_TEXT_DIM);
        return;
    }

    // Plot area
    let plot_left = rx + 8;
    let plot_right = rx + rw - 8;
    let plot_top = sep_y + 6;
    let plot_bot = ry + rh - 6;
    let plot_w = plot_right - plot_left;
    let plot_h = plot_bot - plot_top;
    let plot_cx = plot_left + plot_w / 2; // zero-advection axis

    // Height range
    let min_h = levels
        .iter()
        .map(|l| l.height_m)
        .fold(f64::INFINITY, f64::min);
    let max_h = levels
        .iter()
        .map(|l| l.height_m)
        .fold(f64::NEG_INFINITY, f64::max);
    let h_range = (max_h - min_h).max(1000.0);

    // Advection range (symmetric)
    let max_adv = levels
        .iter()
        .map(|l| l.advection_k_per_hr.abs())
        .fold(0.0_f64, f64::max)
        .max(1.0);
    let adv_range = max_adv;

    let height_to_y = |h: f64| -> i32 {
        let frac = (h - min_h) / h_range;
        plot_bot - (frac * plot_h as f64) as i32
    };

    let adv_to_x = |adv: f64| -> i32 {
        let frac = adv / adv_range;
        plot_cx + (frac * (plot_w as f64 / 2.0 - 2.0)) as i32
    };

    // Zero line (vertical)
    canvas.draw_line(plot_cx, plot_top, plot_cx, plot_bot, [50, 50, 70, 255]);

    // Horizontal height markers
    let n_markers = 4;
    for i in 0..=n_markers {
        let h = min_h + h_range * (i as f64 / n_markers as f64);
        let y = height_to_y(h);
        canvas.draw_line(plot_left, y, plot_right, y, [35, 35, 50, 255]);
    }

    // Draw profile as filled bars from zero
    for level in levels {
        let y = height_to_y(level.height_m);
        let x = adv_to_x(level.advection_k_per_hr);
        let bar_h = (plot_h / levels.len() as i32).max(2).min(6);

        let col = if level.advection_k_per_hr > 0.05 {
            [COL_WARM_ADV[0], COL_WARM_ADV[1], COL_WARM_ADV[2], 180]
        } else if level.advection_k_per_hr < -0.05 {
            [COL_COLD_ADV[0], COL_COLD_ADV[1], COL_COLD_ADV[2], 180]
        } else {
            [
                COL_NEUTRAL_ADV[0],
                COL_NEUTRAL_ADV[1],
                COL_NEUTRAL_ADV[2],
                100,
            ]
        };

        if x >= plot_cx {
            canvas.fill_rect(plot_cx, y - bar_h / 2, x - plot_cx, bar_h, col);
        } else {
            canvas.fill_rect(x, y - bar_h / 2, plot_cx - x, bar_h, col);
        }
    }

    // Draw profile line connecting the advection values
    if levels.len() >= 2 {
        for pair in levels.windows(2) {
            let x0 = adv_to_x(pair[0].advection_k_per_hr);
            let y0 = height_to_y(pair[0].height_m);
            let x1 = adv_to_x(pair[1].advection_k_per_hr);
            let y1 = height_to_y(pair[1].height_m);
            canvas.draw_line_aa(x0 as f64, y0 as f64, x1 as f64, y1 as f64, COL_WHITE);
        }
    }

    // Labels: "WAA" on right, "CAA" on left
    canvas.draw_text(
        "WAA",
        plot_right - Canvas::text_width("WAA") - 1,
        plot_top + 1,
        COL_WARM_ADV,
    );
    canvas.draw_text("CAA", plot_left + 1, plot_top + 1, COL_COLD_ADV);
}

// =========================================================================
// Panel border helper (cyan/dim gray matching reference)
// =========================================================================

/// Draw a visible panel border matching the reference SHARPpy box layout.
/// Uses a brighter border color than the internal separators.
fn draw_panel_border(canvas: &mut Canvas, rx: i32, ry: i32, rw: i32, rh: i32) {
    let border_col = COL_PANEL_BORDER;
    // Double-thick border for visibility: draw rect at boundary and 1px inset
    canvas.draw_rect(rx, ry, rw, rh, border_col);
    canvas.draw_rect(rx + 1, ry + 1, rw - 2, rh - 2, [40, 40, 60, 255]);
}

// =========================================================================
// Convenience: render all panels in a vertical stack
// =========================================================================

/// Layout specification for the right-side panels.
///
/// All panels are stacked vertically within the bounding box
/// `(rx, ry, rw, rh)`.  Heights are allocated as fractions of the
/// total height.
pub fn draw_all_panels(
    canvas: &mut Canvas,
    _sars: &SarsData,
    _climo: &StpClimatology,
    _current_stp: f64,
    _stp_probs: Option<&StpProbabilities>,
    slinky_points: &[SlinkyPoint],
    slinky_tilt_deg: Option<f64>,
    _hazard: WatchType,
    _temp_advection: &[TempAdvectionLevel],
    rx: i32,
    ry: i32,
    rw: i32,
    rh: i32,
) {
    // The modern layout prioritizes readable diagnostics over the original
    // small multi-panel stack. SARS/STP/watch text is still available in the
    // parameter table; the lower-right area is reserved for a usable slinky.
    draw_storm_slinky(canvas, slinky_points, slinky_tilt_deg, rx, ry, rw, rh);
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slinky_color_bands() {
        assert_eq!(slinky_color(0.0), COL_SLINKY_LOW);
        assert_eq!(slinky_color(2999.0), COL_SLINKY_LOW);
        assert_eq!(slinky_color(3000.0), COL_SLINKY_MID);
        assert_eq!(slinky_color(6000.0), COL_SLINKY_HIGH);
        assert_eq!(slinky_color(9000.0), COL_SLINKY_UPPER);
    }

    #[test]
    fn hazard_colors_match_severity() {
        // Just verify no panic and distinct colors
        let none_col = hazard_color(WatchType::None);
        let tor_col = hazard_color(WatchType::Tornado);
        let pds_col = hazard_color(WatchType::PdsTornado);
        assert_ne!(none_col, tor_col);
        assert_ne!(tor_col, pds_col);
    }

    #[test]
    fn stp_climatology_monotonic_medians() {
        let c = StpClimatology::standard();
        assert!(c.ef4_plus.median > c.ef3.median);
        assert!(c.ef3.median > c.ef2.median);
        assert!(c.ef2.median > c.ef1.median);
        assert!(c.ef1.median > c.ef0.median);
        assert!(c.ef0.median > c.nontor.median);
    }

    #[test]
    fn sars_placeholder_is_empty() {
        let d = SarsData::placeholder();
        assert_eq!(d.supercell.loose_matches, 0);
        assert_eq!(d.sgfnt_hail.quality_matches, 0);
    }

    #[test]
    fn compute_slinky_basic() {
        // Constant wind of 20 kt from the west (u=20, v=0),
        // storm motion u=10, v=0 => SR wind is u=10, v=0 everywhere
        let pts = compute_slinky_points(|_| 20.0, |_| 0.0, 10.0, 0.0, 9);
        assert_eq!(pts.len(), 10); // 0 through 9 km
        for pt in &pts {
            assert!((pt.sr_u - 10.0).abs() < 1e-6);
            assert!((pt.sr_v - 0.0).abs() < 1e-6);
        }
        assert!((pts[0].height_m - 0.0).abs() < 1e-6);
        assert!((pts[9].height_m - 9000.0).abs() < 1e-6);
    }

    #[test]
    fn compute_slinky_skips_nan() {
        let pts = compute_slinky_points(
            |h| if h > 5000.0 { f64::NAN } else { 15.0 },
            |_| 5.0,
            10.0,
            0.0,
            9,
        );
        // Only 0-5 km should have valid data (6 points: 0,1,2,3,4,5)
        assert_eq!(pts.len(), 6);
    }

    #[test]
    fn temp_advection_level_created() {
        let level = TempAdvectionLevel {
            pressure_mb: 850.0,
            height_m: 1500.0,
            advection_k_per_hr: 1.5,
        };
        assert!(level.advection_k_per_hr > 0.0);
    }
}
