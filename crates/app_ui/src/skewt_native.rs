//! Native egui skew-T — a vector port of the sharprs sounding render.
//!
//! Pixel-parity spec harvested from vendor/sharprs/src/render/* (see
//! docs/skewt-parity-spec.md): the local coordinate system is the sharprs
//! skew-T sub-canvas plot region (1176 × 1120 at 2× supersample), uniformly
//! scaled into the widget rect — so the plot is CRISP AT EVERY SIZE (the
//! reason this port exists; the PNG cannot scale).
//!
//! Compute stays sharprs-verified: everything here draws from
//! `NativeSounding { profile, params, verified_ecape }` with zero
//! re-derivation — parcel traces come from `ParcelResult::{ptrace, ttrace}`
//! (virtual temperature), the CAPE/CIN fill compares against
//! `cape::interp_vtmp_pub`, and the parameter values are the same numbers
//! the production PNG prints.
//!
//! This pass covers spec P0–P4 (grid, traces, shading, markers, barbs) plus
//! a hover readout the raster never had. Hodograph/table panels follow.

use eframe::egui::{self, Color32, Pos2, Rect, Shape, Stroke, vec2};
use rustwx_sounding::NativeSounding;
use sharprs::params::cape;

// ---- spec colors (skewt.rs constants, RGBA) ----
const COL_BG: Color32 = Color32::from_rgba_premultiplied(10, 10, 22, 255);
#[inline]
#[allow(non_snake_case)]
fn COL_GRID() -> Color32 {
    Color32::from_rgba_unmultiplied(38, 42, 52, 175)
}
#[inline]
#[allow(non_snake_case)]
fn COL_GRID_ZERO() -> Color32 {
    Color32::from_rgba_unmultiplied(90, 135, 230, 210)
}
#[inline]
#[allow(non_snake_case)]
fn COL_ISOBAR() -> Color32 {
    Color32::from_rgba_unmultiplied(58, 62, 76, 215)
}
#[inline]
#[allow(non_snake_case)]
fn COL_DRY_AD() -> Color32 {
    Color32::from_rgba_unmultiplied(120, 90, 55, 70)
}
#[inline]
#[allow(non_snake_case)]
fn COL_MOIST_AD() -> Color32 {
    Color32::from_rgba_unmultiplied(35, 120, 70, 62)
}
#[inline]
#[allow(non_snake_case)]
fn COL_MIX_RATIO() -> Color32 {
    Color32::from_rgba_unmultiplied(35, 110, 65, 55)
}
const COL_TEMP: Color32 = Color32::from_rgb(255, 50, 50);
const COL_DEWP: Color32 = Color32::from_rgb(50, 255, 50);
#[inline]
#[allow(non_snake_case)]
fn COL_WETBULB() -> Color32 {
    Color32::from_rgba_unmultiplied(0, 220, 220, 180)
}
const COL_PARCEL_ML: Color32 = Color32::from_rgb(255, 210, 50);
#[inline]
#[allow(non_snake_case)]
fn COL_PARCEL_MU() -> Color32 {
    Color32::from_rgba_unmultiplied(255, 165, 0, 230)
}
#[inline]
#[allow(non_snake_case)]
fn COL_DCAPE() -> Color32 {
    Color32::from_rgba_unmultiplied(220, 80, 220, 200)
}
#[inline]
#[allow(non_snake_case)]
fn COL_CAPE_FILL() -> Color32 {
    Color32::from_rgba_unmultiplied(255, 60, 40, 70)
}
#[inline]
#[allow(non_snake_case)]
fn COL_CIN_FILL() -> Color32 {
    Color32::from_rgba_unmultiplied(60, 80, 255, 60)
}
const COL_WIND_BARB: Color32 = Color32::from_rgb(0, 220, 220);
const COL_LABEL: Color32 = Color32::from_rgb(200, 200, 210);
const COL_LABEL_LARGE: Color32 = Color32::from_rgb(220, 220, 230);
const COL_HEIGHT_MARK: Color32 = Color32::from_rgb(0, 220, 220);
#[inline]
#[allow(non_snake_case)]
fn COL_EFF_INFLOW() -> Color32 {
    Color32::from_rgba_unmultiplied(0, 220, 220, 220)
}
#[inline]
#[allow(non_snake_case)]
fn COL_OMEGA() -> Color32 {
    Color32::from_rgba_unmultiplied(40, 200, 40, 200)
}
#[inline]
#[allow(non_snake_case)]
fn COL_OMEGA_ZERO() -> Color32 {
    Color32::from_rgba_unmultiplied(35, 35, 45, 200)
}
const COL_LCL: Color32 = Color32::from_rgb(0, 255, 0);
const COL_LFC: Color32 = Color32::from_rgb(255, 255, 0);
const COL_EL: Color32 = Color32::from_rgb(255, 100, 255);
#[inline]
#[allow(non_snake_case)]
fn COL_LABEL_BG() -> Color32 {
    Color32::from_rgba_unmultiplied(10, 10, 22, 200)
}

// ---- spec local geometry (sharprs skew-T sub-canvas, 1176x1120) ----
const LOCAL_W: f32 = 1176.0;
const LOCAL_H: f32 = 1120.0;
const PLOT_L: f32 = 70.0;
const PLOT_R: f32 = 1121.0; // 1176 - 55
const PLOT_T: f32 = 28.0;
const PLOT_B: f32 = 1082.0; // 1120 - 38
const P_BOT: f64 = 1050.0;
const P_TOP: f64 = 100.0;
const T_MIN: f64 = -40.0;
const T_SPAN: f64 = 90.0;
const SKEW_PER_LNP: f64 = 25.0;
const BARB_X: f32 = 1148.5;
const ROCP: f64 = 0.285_714_29; // Rd/Cp

/// Local-spec → screen mapping (uniform fit into the widget rect).
struct Geom {
    origin: Pos2,
    s: f32,
}

impl Geom {
    fn fit(rect: Rect) -> Self {
        let s = (rect.width() / LOCAL_W).min(rect.height() / LOCAL_H);
        let used = vec2(LOCAL_W * s, LOCAL_H * s);
        let origin = rect.min + (rect.size() - used) / 2.0;
        Self { origin, s }
    }

    #[inline]
    fn pos(&self, x: f32, y: f32) -> Pos2 {
        self.origin + vec2(x * self.s, y * self.s)
    }

    /// log-p vertical (spec yn formula).
    #[inline]
    fn sy(&self, p: f64) -> f32 {
        let yn = (P_BOT.ln() - p.ln()) / (P_BOT.ln() - P_TOP.ln());
        PLOT_T + (1.0 - yn as f32) * (PLOT_B - PLOT_T)
    }

    /// Skewed temperature horizontal (spec t_shifted formula).
    #[inline]
    fn sx(&self, t_c: f64, p: f64) -> f32 {
        let shifted = t_c + SKEW_PER_LNP * (P_BOT.ln() - p.ln());
        PLOT_L + (((shifted - T_MIN) / T_SPAN) as f32) * (PLOT_R - PLOT_L)
    }

    /// Inverse of sy (for the hover readout).
    fn pressure_at(&self, screen_y: f32) -> f64 {
        let y_local = (screen_y - self.origin.y) / self.s;
        let frac = 1.0 - (y_local - PLOT_T) / (PLOT_B - PLOT_T);
        (P_BOT.ln() - frac as f64 * (P_BOT.ln() - P_TOP.ln())).exp()
    }

    fn stroke(&self, width_local: f32, color: Color32) -> Stroke {
        Stroke::new((width_local * self.s).max(0.6), color)
    }

    fn font(&self, local_px: f32) -> egui::FontId {
        egui::FontId::proportional((local_px * self.s).max(7.0))
    }
}

/// Draw the full native skew-T into `rect`. Returns the hovered pressure
/// (for caller-side readouts) when the cursor is inside the plot.
pub fn draw_skewt(ui: &mut egui::Ui, rect: Rect, sounding: &NativeSounding) -> Option<f64> {
    let painter = ui.painter_at(rect);
    let g = Geom::fit(rect);
    painter.rect_filled(rect, 0.0, COL_BG);

    let plot_clip = Rect::from_min_max(g.pos(PLOT_L, PLOT_T), g.pos(PLOT_R, PLOT_B));
    let clipped = ui.painter_at(plot_clip.intersect(rect));

    draw_grid(&clipped, &g);
    draw_labels(&painter, &g, sounding);
    draw_cape_fill(&clipped, &g, sounding);
    draw_traces(&clipped, &g, sounding);
    draw_markers(&painter, &g, sounding);
    draw_omega(&painter, &g, sounding);
    draw_barbs(&painter, &g, sounding);

    // Hover readout (native bonus): nearest-level values at the cursor.
    let hover = ui
        .input(|i| i.pointer.hover_pos())
        .filter(|p| plot_clip.contains(*p));
    if let Some(pointer) = hover {
        let p_hpa = g.pressure_at(pointer.y).clamp(P_TOP, P_BOT);
        let profile = &sounding.profile;
        let t = profile.interp_tmpc(p_hpa);
        let h = profile.interp_hght(p_hpa);
        let (dir, spd) = profile.interp_vec(p_hpa);
        let agl_km = (h - profile.sfc_height()) / 1000.0;
        let label = format!(
            "{:.0} hPa · {:.1} km AGL\nT {:.1}°C · wind {:03.0}°/{:.0} kt",
            p_hpa, agl_km, t, dir, spd
        );
        let font = g.font(20.0);
        let galley = painter.layout_no_wrap(label, font, Color32::from_rgb(235, 240, 245));
        let pos = pointer + vec2(14.0, -10.0);
        let bg = Rect::from_min_size(pos, galley.size() + vec2(8.0, 6.0));
        painter.rect_filled(bg, 3.0, COL_LABEL_BG());
        painter.galley(pos + vec2(4.0, 3.0), galley, Color32::WHITE);
        return Some(p_hpa);
    }
    None
}

// ---- P0: background grid ----

fn draw_grid(painter: &egui::Painter, g: &Geom) {
    // Mixing-ratio dashed lines: w ∈ {1,2,4,7,10,16,24} g/kg, 1050→400.
    for &w in &[1.0f64, 2.0, 4.0, 7.0, 10.0, 16.0, 24.0] {
        let mut points = Vec::new();
        let mut p = P_BOT;
        while p >= 400.0 {
            let es = w / 1000.0 * p / (0.622 + w / 1000.0);
            let td = 243.5 * (es / 6.112).ln() / (17.67 - (es / 6.112).ln());
            points.push(g.pos(g.sx(td, p), g.sy(p)));
            p -= 20.0;
        }
        painter.add(Shape::dashed_line(
            &points,
            g.stroke(1.0, COL_MIX_RATIO()),
            4.0 * g.s,
            4.0 * g.s,
        ));
    }
    // Moist adiabats: start T −28..36 step 4 at 1050, Euler dp = −10.
    for start in (-28..=36).step_by(4) {
        let mut t = start as f64;
        let mut p = P_BOT;
        let mut points = vec![g.pos(g.sx(t, p), g.sy(p))];
        while p > P_TOP {
            let dp = -10.0;
            t += moist_lapse_dt(t, p, dp);
            p += dp;
            points.push(g.pos(g.sx(t, p), g.sy(p)));
        }
        painter.add(Shape::line(points, g.stroke(1.0, COL_MOIST_AD())));
    }
    // Dry adiabats: θ −40..80 step 20 (°C), T = θ·(p/1000)^ROCP.
    for theta_c in (-40..=80).step_by(20) {
        let theta_k = theta_c as f64 + 273.15;
        let mut points = Vec::new();
        let mut p = P_BOT;
        while p >= P_TOP {
            let t = theta_k * (p / 1000.0).powf(ROCP) - 273.15;
            points.push(g.pos(g.sx(t, p), g.sy(p)));
            p -= 10.0;
        }
        painter.add(Shape::line(points, g.stroke(1.0, COL_DRY_AD())));
    }
    // Isobars.
    for &p in &[
        1000.0f64, 925.0, 850.0, 700.0, 500.0, 400.0, 300.0, 250.0, 200.0, 150.0, 100.0,
    ] {
        let y = g.sy(p);
        painter.line_segment(
            [g.pos(PLOT_L, y), g.pos(PLOT_R, y)],
            g.stroke(1.0, COL_ISOBAR()),
        );
    }
    // Isotherms −80..60 step 10; 0 °C thick blue.
    for t in (-80..=60).step_by(10) {
        let t = t as f64;
        let a = g.pos(g.sx(t, P_BOT), g.sy(P_BOT));
        let b = g.pos(g.sx(t, P_TOP), g.sy(P_TOP));
        let (width, color) = if t == 0.0 {
            (3.0, COL_GRID_ZERO())
        } else {
            (1.0, COL_GRID())
        };
        painter.line_segment([a, b], g.stroke(width, color));
    }
}

/// Pseudoadiabatic dT for a dp step (sharprs moist_lapse_rate form).
fn moist_lapse_dt(t_c: f64, p_hpa: f64, dp: f64) -> f64 {
    const RD: f64 = 287.04;
    const G: f64 = 9.806_65;
    const CP: f64 = 1005.7;
    const LV: f64 = 2.501e6;
    const RV: f64 = 461.5;
    let tk = t_c + 273.15;
    let es = 6.112 * (17.67 * t_c / (t_c + 243.5)).exp();
    let ws = 0.622 * es / (p_hpa - es).max(1.0);
    let dz = -(RD * tk / G) * (dp / p_hpa);
    let gamma_m =
        (G / CP) * (1.0 + LV * ws / (RD * tk)) / (1.0 + LV * LV * ws * 0.622 / (CP * RV * tk * tk));
    -gamma_m * dz
}

// ---- pressure/temperature labels ----

fn draw_labels(painter: &egui::Painter, g: &Geom, sounding: &NativeSounding) {
    for &p in &[1000.0f64, 850.0, 700.0, 500.0, 300.0, 200.0, 100.0] {
        painter.text(
            g.pos(PLOT_L - 4.0, g.sy(p) - 10.0),
            egui::Align2::RIGHT_TOP,
            format!("{p:.0}"),
            g.font(42.0),
            COL_LABEL_LARGE,
        );
    }
    // Minor isobar labels (1x tier, spec).
    for &p in &[925.0f64, 400.0, 250.0, 150.0] {
        painter.text(
            g.pos(PLOT_L - 3.0, g.sy(p) - 5.0),
            egui::Align2::RIGHT_TOP,
            format!("{p:.0}"),
            g.font(22.0),
            COL_LABEL,
        );
    }
    for t in (-30..=40).step_by(10) {
        painter.text(
            g.pos(g.sx(t as f64, P_BOT), PLOT_B + 4.0),
            egui::Align2::CENTER_TOP,
            format!("{t}C"),
            g.font(42.0),
            COL_LABEL_LARGE,
        );
    }
    // Surface °F labels (T red / Td green).
    let profile = &sounding.profile;
    let sfc = profile.sfc;
    if let (Some(&t), Some(&td)) = (profile.tmpc.get(sfc), profile.dwpc.get(sfc)) {
        if t.is_finite() {
            painter.text(
                g.pos(g.sx(t, P_BOT), PLOT_B + 34.0),
                egui::Align2::CENTER_TOP,
                format!("{:.0}F", t * 9.0 / 5.0 + 32.0),
                g.font(30.0),
                COL_TEMP,
            );
        }
        if td.is_finite() {
            painter.text(
                g.pos(g.sx(td, P_BOT), PLOT_B + 34.0),
                egui::Align2::CENTER_TOP,
                format!("{:.0}F", td * 9.0 / 5.0 + 32.0),
                g.font(30.0),
                COL_DEWP,
            );
        }
    }
}

// ---- P2: ML-parcel CAPE/CIN fill (vs environment virtual temperature) ----

fn draw_cape_fill(painter: &egui::Painter, g: &Geom, sounding: &NativeSounding) {
    let parcel = &sounding.params.mlpcl;
    let profile = &sounding.profile;
    let n = parcel.ptrace.len().min(parcel.ttrace.len());
    if n < 2 {
        return;
    }
    // cape's interp_vtmp wants its own lifted-profile form (same arrays,
    // virtual temps precomputed) — exactly how the sharprs render builds it.
    let cape_prof = cape::Profile::new(
        profile.pres.clone(),
        profile.hght.clone(),
        profile.tmpc.clone(),
        profile.dwpc.clone(),
        profile.sfc,
    );
    for i in 0..n - 1 {
        let (p0, p1) = (parcel.ptrace[i], parcel.ptrace[i + 1]);
        let (t0, t1) = (parcel.ttrace[i], parcel.ttrace[i + 1]);
        if !(p0.is_finite() && p1.is_finite() && t0.is_finite() && t1.is_finite()) {
            continue;
        }
        if p0 < P_TOP || p1 < P_TOP || p0 > P_BOT || p1 > P_BOT {
            continue;
        }
        let e0 = cape::interp_vtmp_pub(&cape_prof, p0);
        let e1 = cape::interp_vtmp_pub(&cape_prof, p1);
        if !(e0.is_finite() && e1.is_finite()) {
            continue;
        }
        let buoyant = (t0 + t1) / 2.0 > (e0 + e1) / 2.0;
        let color = if buoyant {
            COL_CAPE_FILL()
        } else {
            COL_CIN_FILL()
        };
        let quad = vec![
            g.pos(g.sx(t0, p0), g.sy(p0)),
            g.pos(g.sx(t1, p1), g.sy(p1)),
            g.pos(g.sx(e1, p1), g.sy(p1)),
            g.pos(g.sx(e0, p0), g.sy(p0)),
        ];
        painter.add(Shape::convex_polygon(quad, color, Stroke::NONE));
    }
}

// ---- P1: traces ----

fn profile_trace(g: &Geom, pres: &[f64], values: &[f64]) -> Vec<Pos2> {
    pres.iter()
        .zip(values.iter())
        .filter(|(p, v)| p.is_finite() && v.is_finite() && **p >= P_TOP && **p <= P_BOT)
        .map(|(&p, &v)| g.pos(g.sx(v, p), g.sy(p)))
        .collect()
}

fn draw_traces(painter: &egui::Painter, g: &Geom, sounding: &NativeSounding) {
    let profile = &sounding.profile;
    // Wet-bulb (1 px), then T/Td thick (5 px), parcels dashed.
    let wb = profile_trace(g, &profile.pres, &profile.wetbulb);
    painter.add(Shape::line(wb, g.stroke(1.0, COL_WETBULB())));
    let t = profile_trace(g, &profile.pres, &profile.tmpc);
    painter.add(Shape::line(t, g.stroke(5.0, COL_TEMP)));
    let td = profile_trace(g, &profile.pres, &profile.dwpc);
    painter.add(Shape::line(td, g.stroke(5.0, COL_DEWP)));

    // Parcel traces (ptrace/ttrace = sharprs-verified virtual temperature).
    for (parcel, color, dash, gap) in [
        (&sounding.params.mlpcl, COL_PARCEL_ML, 8.0, 5.0),
        (&sounding.params.mupcl, COL_PARCEL_MU(), 8.0, 5.0),
    ] {
        let points = profile_trace(g, &parcel.ptrace, &parcel.ttrace);
        if points.len() >= 2 {
            painter.add(Shape::dashed_line(
                &points,
                g.stroke(2.0, color),
                dash * g.s,
                gap * g.s,
            ));
        }
    }
    let dcape = &sounding.params.dcape;
    let points = profile_trace(g, &dcape.ptrace, &dcape.ttrace);
    if points.len() >= 2 {
        painter.add(Shape::dashed_line(
            &points,
            g.stroke(2.0, COL_DCAPE()),
            6.0 * g.s,
            4.0 * g.s,
        ));
    }
}

// ---- P3: markers ----

fn level_marker(painter: &egui::Painter, g: &Geom, p: f64, text: &str, color: Color32) {
    if !p.is_finite() || !(P_TOP..=P_BOT).contains(&p) {
        return;
    }
    let y = g.sy(p);
    painter.line_segment([g.pos(72.0, y), g.pos(82.0, y)], g.stroke(2.0, color));
    let font = g.font(42.0);
    let galley = painter.layout_no_wrap(text.to_owned(), font, color);
    let pos = g.pos(84.0, y - 10.0);
    painter.rect_filled(
        Rect::from_min_size(pos, galley.size() + vec2(6.0, 4.0)),
        2.0,
        COL_LABEL_BG(),
    );
    painter.galley(pos + vec2(3.0, 2.0), galley, color);
}

fn draw_markers(painter: &egui::Painter, g: &Geom, sounding: &NativeSounding) {
    let profile = &sounding.profile;
    // LCL/LFC/EL from the SB parcel (spec).
    let sb = &sounding.params.sfcpcl;
    level_marker(painter, g, sb.lclpres, "LCL", COL_LCL);
    level_marker(painter, g, sb.lfcpres, "LFC", COL_LFC);
    level_marker(painter, g, sb.elpres, "EL", COL_EL);

    // Height markers 0–15 km AGL.
    let sfc_h = profile.sfc_height();
    for &km in &[0.0f64, 1.0, 3.0, 6.0, 9.0, 12.0, 15.0] {
        let p = profile.pres_at_height(sfc_h + km * 1000.0);
        if !p.is_finite() || !(P_TOP..=P_BOT).contains(&p) {
            continue;
        }
        let y = g.sy(p);
        painter.line_segment(
            [g.pos(62.0, y), g.pos(PLOT_L, y)],
            g.stroke(2.0, COL_HEIGHT_MARK),
        );
        painter.text(
            g.pos(PLOT_L - 4.0, y + 3.0),
            egui::Align2::RIGHT_TOP,
            format!("{km:.0} km"),
            g.font(26.0),
            COL_HEIGHT_MARK,
        );
    }

    // Effective inflow bracket.
    let (top, bot) = sounding.params.eff_inflow;
    if top.is_finite() && bot.is_finite() && top > 0.0 && bot > 0.0 {
        let (y_top, y_bot) = (g.sy(top.min(bot)), g.sy(top.max(bot)));
        let x = 76.0;
        painter.line_segment(
            [g.pos(x, y_top), g.pos(x, y_bot)],
            g.stroke(4.0, COL_EFF_INFLOW()),
        );
        for y in [y_top, y_bot] {
            painter.line_segment(
                [g.pos(71.0, y), g.pos(82.0, y)],
                g.stroke(3.0, COL_EFF_INFLOW()),
            );
        }
        painter.text(
            g.pos(84.0, (y_top + y_bot) / 2.0 - 5.0),
            egui::Align2::LEFT_TOP,
            "EFF",
            g.font(22.0),
            COL_EFF_INFLOW(),
        );
    }
}

// ---- omega strip ----

fn draw_omega(painter: &egui::Painter, g: &Geom, sounding: &NativeSounding) {
    let profile = &sounding.profile;
    painter.line_segment(
        [g.pos(25.0, PLOT_T), g.pos(25.0, PLOT_B)],
        g.stroke(1.0, COL_OMEGA_ZERO()),
    );
    let mut points = Vec::new();
    for (&p, &omega) in profile.pres.iter().zip(profile.omeg.iter()) {
        if !p.is_finite() || !omega.is_finite() || !(P_TOP..=P_BOT).contains(&p) {
            continue;
        }
        let x = (25.0 + omega as f32 * 20.0).clamp(5.0, 48.0);
        points.push(g.pos(x, g.sy(p)));
    }
    if points.len() >= 2 {
        painter.add(Shape::line(points, g.stroke(1.0, COL_OMEGA())));
    }
}

// ---- P4: wind barbs (spec glyph geometry) ----

const BARB_LEVELS: [f64; 17] = [
    1000.0, 925.0, 850.0, 800.0, 750.0, 700.0, 650.0, 600.0, 550.0, 500.0, 450.0, 400.0, 350.0,
    300.0, 250.0, 200.0, 150.0,
];

fn draw_barbs(painter: &egui::Painter, g: &Geom, sounding: &NativeSounding) {
    let profile = &sounding.profile;
    for &p in &BARB_LEVELS {
        let (dir, spd) = profile.interp_vec(p);
        if !dir.is_finite() || !spd.is_finite() || spd < 0.5 {
            continue;
        }
        draw_wind_barb(painter, g, g.pos(BARB_X, g.sy(p)), dir, spd);
    }
}

fn draw_wind_barb(painter: &egui::Painter, g: &Geom, tip: Pos2, dir_deg: f64, spd_kt: f64) {
    let stroke = g.stroke(1.0, COL_WIND_BARB);
    if spd_kt < 2.5 {
        painter.circle_filled(tip, 2.0 * g.s, COL_WIND_BARB);
        return;
    }
    let dir = dir_deg.to_radians();
    let u = -spd_kt * dir.sin();
    let v = spd_kt * dir.cos();
    // Screen y-down: upwind unit vector.
    let tail = vec2((-u / spd_kt) as f32, (v / spd_kt) as f32);
    let perp = vec2(-tail.y, tail.x);
    let shaft = 23.0 * g.s;
    let spacing = (23.0f32 * 0.16).max(2.0) * g.s;
    let full_h = 23.0 * 0.40 * g.s;
    let full_w = 23.0 * 0.25 * g.s;
    let end = tip + tail * shaft;
    painter.line_segment([tip, end], stroke);

    let mut remaining = ((spd_kt + 2.5) / 5.0).floor() * 5.0;
    let mut offset = shaft;
    let mut drew_any = false;
    while remaining >= 50.0 {
        let base = tip + tail * offset;
        let apex = base + perp * full_h - tail * (full_w * 0.5);
        let corner = base - tail * full_w;
        painter.add(Shape::convex_polygon(
            vec![base, apex, corner],
            COL_WIND_BARB,
            stroke,
        ));
        offset -= full_w + spacing;
        remaining -= 50.0;
        drew_any = true;
    }
    while remaining >= 10.0 {
        let base = tip + tail * offset;
        painter.line_segment([base, base + perp * full_h + tail * (full_w * 0.5)], stroke);
        offset -= spacing;
        remaining -= 10.0;
        drew_any = true;
    }
    if remaining >= 5.0 {
        if !drew_any {
            offset -= 1.5 * spacing;
        }
        let base = tip + tail * offset;
        painter.line_segment(
            [base, base + perp * (full_h * 0.5) + tail * (full_w * 0.25)],
            stroke,
        );
    }
}
