//! Sharprs parity port, P5–P7: hodograph, storm slinky, sounding summary,
//! and the full parameter table, composed into the production 2400×1800
//! layout (docs/skewt-parity-spec.md). The skew-T panel itself lives in
//! [`crate::skewt_native`]; this module adds everything around it.
//!
//! Numbers come from the SAME calls the production native_table makes
//! (sharprs `winds::`/`composites::`/`indices::`/`cape::` parameterized
//! exactly as `build_table_data`), so the table prints the verified values
//! — only the drawing is new.

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Shape, Stroke, vec2};
use rustwx_sounding::NativeSounding;
use sharprs::params::{cape, composites, indices};
use sharprs::profile::comp2vec;
use sharprs::winds;

// ---- native-table palette (spec §3) ----
const BG: Color32 = Color32::from_rgb(7, 10, 16);
const TITLE_BG: Color32 = Color32::from_rgb(18, 22, 31);
const PANEL_BG: Color32 = Color32::from_rgb(10, 14, 22);
const LINE: Color32 = Color32::from_rgb(58, 66, 82);
const LINE_DIM: Color32 = Color32::from_rgb(34, 41, 54);
const TEXT: Color32 = Color32::from_rgb(231, 235, 241);
const MUTED: Color32 = Color32::from_rgb(145, 154, 168);
const LABEL: Color32 = Color32::from_rgb(141, 214, 232);
const GOOD: Color32 = Color32::from_rgb(96, 220, 132);
const WATCH: Color32 = Color32::from_rgb(255, 210, 88);
const ORANGE: Color32 = Color32::from_rgb(255, 151, 79);
const DANGER: Color32 = Color32::from_rgb(255, 86, 95);

// ---- hodograph palette (spec §3) ----
const H_PANEL_BG: Color32 = Color32::from_rgb(18, 18, 32);
const H_BORDER: Color32 = Color32::from_rgb(50, 50, 70);
const H_RING: Color32 = Color32::from_rgb(40, 40, 52);
const H_AXIS: Color32 = Color32::from_rgb(55, 55, 70);
const H_RING_LABEL: Color32 = Color32::from_rgb(130, 130, 155);
const H_TEXT: Color32 = Color32::from_rgb(240, 240, 240);
const H_TEXT_DIM: Color32 = Color32::from_rgb(150, 150, 165);
const H_HEADER: Color32 = Color32::from_rgb(100, 190, 255);
const H_WARN: Color32 = Color32::from_rgb(255, 210, 80);
const H_RM: Color32 = Color32::from_rgb(255, 50, 50);
const H_LM: Color32 = Color32::from_rgb(60, 130, 255);
const H_MEAN: Color32 = Color32::from_rgb(210, 210, 210);
const H_CORFIDI_UP: Color32 = Color32::from_rgb(255, 180, 60);
const H_CORFIDI_DN: Color32 = Color32::from_rgb(60, 220, 255);
const H_CAA: Color32 = Color32::from_rgb(100, 160, 255);

fn band_color(km: f64) -> Color32 {
    if km < 1.0 {
        Color32::from_rgb(255, 30, 30)
    } else if km < 3.0 {
        Color32::from_rgb(255, 165, 0)
    } else if km < 6.0 {
        Color32::from_rgb(255, 255, 0)
    } else if km < 9.0 {
        Color32::from_rgb(0, 230, 0)
    } else if km < 12.0 {
        Color32::from_rgb(50, 130, 255)
    } else {
        Color32::from_rgb(200, 80, 255)
    }
}

fn sr_wind_color() -> Color32 {
    Color32::from_rgba_unmultiplied(180, 255, 180, 200)
}

// ---- composite canvas (2400×1800) ----
const CANVAS_W: f32 = 2400.0;
const CANVAS_H: f32 = 1800.0;

struct G {
    origin: Pos2,
    s: f32,
}

impl G {
    fn fit(rect: Rect) -> Self {
        let s = (rect.width() / CANVAS_W).min(rect.height() / CANVAS_H);
        let used = vec2(CANVAS_W * s, CANVAS_H * s);
        Self {
            origin: rect.min + (rect.size() - used) / 2.0,
            s,
        }
    }
    #[inline]
    fn pos(&self, x: f32, y: f32) -> Pos2 {
        self.origin + vec2(x * self.s, y * self.s)
    }
    fn rect(&self, x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect::from_min_size(self.pos(x, y), vec2(w * self.s, h * self.s))
    }
    fn font(&self, local_px: f32) -> FontId {
        // 0.88: egui's proportional face is wider than the spec's
        // SourceSans metrics; shrinking text relative to the layout keeps
        // the spec's column anchors collision-free.
        FontId::proportional((local_px * self.s * 0.88).max(6.5))
    }
    fn stroke(&self, w: f32, c: Color32) -> Stroke {
        Stroke::new((w * self.s).max(0.5), c)
    }
}

/// The full production sounding composite (title, skew-T, summary,
/// hodograph, slinky, parameter table). Resolution independent.
pub fn draw_full(ui: &mut egui::Ui, rect: Rect, sounding: &NativeSounding) {
    let painter = ui.painter_at(rect);
    let g = G::fit(rect);
    painter.rect_filled(g.rect(0.0, 0.0, CANVAS_W, CANVAS_H), 0.0, BG);

    // Title bar.
    painter.rect_filled(g.rect(0.0, 0.0, CANVAS_W, 44.0), 0.0, TITLE_BG);
    let meta = &sounding.metadata;
    let mut title = "BowEcho Sounding Analysis".to_owned();
    if let (Some(lat), Some(lon)) = (meta.latitude_deg, meta.longitude_deg) {
        title.push_str(&format!(" — {lat:.3}, {lon:.3}"));
    }
    if !meta.valid_time.is_empty() {
        title.push_str(&format!(" — {}", meta.valid_time));
    }
    painter.text(
        g.pos(CANVAS_W / 2.0, 3.0),
        Align2::CENTER_TOP,
        title,
        g.font(33.0),
        TEXT,
    );

    // Skew-T (the visible 1176-wide region of the sub-canvas).
    let skewt_rect = g.rect(0.0, 44.0, 1176.0, 1120.0);
    crate::skewt_native::draw_skewt(ui, skewt_rect, sounding);

    // Summary panel (replaces the locator pair; spec §8 summary columns).
    draw_summary(&painter, &g, sounding);

    // Hodograph + slinky.
    draw_hodograph(&painter, &g, sounding);
    draw_slinky(&painter, &g, sounding);

    // Parameter table.
    draw_table(&painter, &g, sounding);

    // Separators (spec §1).
    painter.line_segment(
        [g.pos(0.0, 1164.0), g.pos(CANVAS_W, 1164.0)],
        g.stroke(1.0, LINE),
    );
    painter.line_segment(
        [g.pos(1680.0, 44.0), g.pos(1680.0, 1163.0)],
        g.stroke(1.0, H_BORDER),
    );
    painter.line_segment(
        [g.pos(1680.0, 644.0), g.pos(CANVAS_W, 644.0)],
        g.stroke(1.0, H_BORDER),
    );
}

// ---- helpers ----

fn fmt(value: f64, decimals: usize) -> String {
    if value.is_finite() {
        format!("{value:.decimals$}")
    } else {
        "--".to_owned()
    }
}

fn fmt_opt(value: Option<f64>, decimals: usize) -> String {
    fmt(value.unwrap_or(f64::NAN), decimals)
}

fn dir_spd(dir: f64, spd: f64) -> String {
    if dir.is_finite() && spd.is_finite() {
        format!("{dir:.0}/{spd:.0}")
    } else {
        "--".to_owned()
    }
}

fn cape_color(v: f64) -> Color32 {
    if !v.is_finite() {
        MUTED
    } else if v >= 4000.0 {
        DANGER
    } else if v >= 2500.0 {
        ORANGE
    } else if v >= 1000.0 {
        WATCH
    } else {
        TEXT
    }
}

fn cin_color(v: f64) -> Color32 {
    if !v.is_finite() {
        MUTED
    } else if v <= -100.0 {
        DANGER
    } else if v <= -50.0 {
        ORANGE
    } else {
        GOOD
    }
}

fn lapse_color(v: f64) -> Color32 {
    if !v.is_finite() {
        MUTED
    } else if v >= 8.5 {
        DANGER
    } else if v >= 7.5 {
        ORANGE
    } else if v >= 6.5 {
        WATCH
    } else {
        TEXT
    }
}

fn srh_color(v: f64) -> Color32 {
    if !v.is_finite() {
        MUTED
    } else if v >= 300.0 {
        DANGER
    } else if v >= 150.0 {
        ORANGE
    } else if v >= 75.0 {
        WATCH
    } else {
        TEXT
    }
}

fn shear_color(v: f64) -> Color32 {
    if !v.is_finite() {
        MUTED
    } else if v >= 60.0 {
        DANGER
    } else if v >= 40.0 {
        WATCH
    } else {
        TEXT
    }
}

fn stp_color(v: f64) -> Color32 {
    if !v.is_finite() {
        MUTED
    } else if v >= 4.0 {
        DANGER
    } else if v >= 2.0 {
        ORANGE
    } else if v >= 1.0 {
        WATCH
    } else {
        TEXT
    }
}

fn scp_color(v: f64) -> Color32 {
    if !v.is_finite() {
        MUTED
    } else if v >= 8.0 {
        DANGER
    } else if v >= 4.0 {
        ORANGE
    } else if v >= 1.0 {
        WATCH
    } else {
        TEXT
    }
}

fn ship_color(v: f64) -> Color32 {
    if !v.is_finite() {
        MUTED
    } else if v >= 2.0 {
        DANGER
    } else if v >= 1.0 {
        WATCH
    } else {
        TEXT
    }
}

fn vector_mag(u: f64, v: f64) -> f64 {
    (u * u + v * v).sqrt()
}

fn mean_wind_mag(profile: &rustwx_sounding::SharprsProfile, pbot: f64, ptop: f64) -> f64 {
    winds::mean_wind(profile, pbot, ptop, -1.0, 0.0, 0.0)
        .map(|(u, v)| vector_mag(u, v))
        .unwrap_or(f64::NAN)
}

fn shear_mag(profile: &rustwx_sounding::SharprsProfile, pbot: f64, ptop: f64) -> f64 {
    winds::wind_shear(profile, pbot, ptop)
        .map(|(u, v)| vector_mag(u, v))
        .unwrap_or(f64::NAN)
}

// ---- summary panel (1176,44,504,1120) ----

#[allow(clippy::too_many_arguments)] // mirrors the spec's column signature
fn key_value_row(
    painter: &egui::Painter,
    g: &G,
    x: f32,
    y: f32,
    w: f32,
    label: &str,
    value: &str,
    color: Color32,
) {
    painter.text(g.pos(x, y), Align2::LEFT_TOP, label, g.font(28.0), LABEL);
    painter.text(
        g.pos(x + w, y - 1.0),
        Align2::RIGHT_TOP,
        value,
        g.font(28.0),
        color,
    );
}

fn section_title(painter: &egui::Painter, g: &G, x: f32, y: f32, w: f32, title: &str) {
    painter.text(g.pos(x, y), Align2::LEFT_TOP, title, g.font(34.0), LABEL);
    painter.line_segment(
        [g.pos(x, y + 44.0), g.pos(x + w, y + 44.0)],
        g.stroke(1.0, LINE),
    );
}

fn draw_summary(painter: &egui::Painter, g: &G, sounding: &NativeSounding) {
    let rect = g.rect(1176.0, 44.0, 504.0, 1120.0);
    painter.rect_filled(rect, 0.0, PANEL_BG);
    painter.rect_stroke(rect, 0.0, g.stroke(1.0, LINE), egui::StrokeKind::Inside);
    let (x0, y0) = (1190.0, 58.0);
    painter.text(
        g.pos(x0, y0),
        Align2::LEFT_TOP,
        "SOUNDING SUMMARY",
        g.font(34.0),
        LABEL,
    );
    painter.line_segment(
        [g.pos(x0, y0 + 44.0), g.pos(x0 + 472.0, y0 + 44.0)],
        g.stroke(1.0, LINE_DIM),
    );
    let p = &sounding.params;
    let e = &sounding.verified_ecape;
    let profile = &sounding.profile;

    // ENERGY
    let ey = y0 + 64.0;
    section_title(painter, g, x0, ey, 222.0, "ENERGY");
    let rows: [(&str, f64, Color32); 5] = [
        ("SB CAPE", p.sfcpcl.bplus, cape_color(p.sfcpcl.bplus)),
        (
            "SB ECAPE",
            e.surface_based.ecape,
            cape_color(e.surface_based.ecape),
        ),
        ("ML CAPE", p.mlpcl.bplus, cape_color(p.mlpcl.bplus)),
        ("MU CAPE", p.mupcl.bplus, cape_color(p.mupcl.bplus)),
        ("DCAPE", p.dcape.dcape, cape_color(p.dcape.dcape)),
    ];
    for (i, (label, value, color)) in rows.iter().enumerate() {
        key_value_row(
            painter,
            g,
            x0,
            ey + 58.0 + i as f32 * 36.0,
            222.0,
            label,
            &format!("{} J/kg", fmt(*value, 0)),
            *color,
        );
    }

    // LEVELS
    let lx = x0 + 250.0;
    section_title(painter, g, lx, ey, 222.0, "LEVELS");
    let levels: [(&str, f64, Color32); 5] = [
        ("LCL", p.sfcpcl.lclhght, TEXT),
        ("LFC", p.sfcpcl.lfchght, TEXT),
        ("EL", p.sfcpcl.elhght, TEXT),
        ("WB Zero", p.wb_zero.unwrap_or(f64::NAN), GOOD),
        ("Freezing", p.frz_lvl.unwrap_or(f64::NAN), TEXT),
    ];
    for (i, (label, value, color)) in levels.iter().enumerate() {
        key_value_row(
            painter,
            g,
            lx,
            ey + 58.0 + i as f32 * 36.0,
            222.0,
            label,
            &format!("{} m", fmt(*value, 0)),
            *color,
        );
    }

    // SHEAR / MOTION
    let sy = ey + 268.0;
    section_title(painter, g, x0, sy, 472.0, "SHEAR / MOTION");
    let shr03 = vector_mag(p.shr03.0, p.shr03.1);
    let shr06 = vector_mag(p.shr06.0, p.shr06.1);
    let motion_rows: [(&str, String, Color32); 4] = [
        (
            "0-3km SRH",
            format!("{} m2/s2", fmt(p.srh03.0, 0)),
            srh_color(p.srh03.0),
        ),
        (
            "0-3km Shear",
            format!("{} kt", fmt(shr03, 0)),
            shear_color(shr03),
        ),
        (
            "0-6km Shear",
            format!("{} kt", fmt(shr06, 0)),
            shear_color(shr06),
        ),
        (
            "Eff SRH",
            format!("{} m2/s2", fmt_opt(p.effective_srh, 0)),
            srh_color(p.effective_srh.unwrap_or(f64::NAN)),
        ),
    ];
    for (i, (label, value, color)) in motion_rows.iter().enumerate() {
        key_value_row(
            painter,
            g,
            x0,
            sy + 58.0 + i as f32 * 36.0,
            472.0,
            label,
            value,
            *color,
        );
    }

    // STORM MOTION quick row + surface info.
    let my = sy + 216.0;
    section_title(painter, g, x0, my, 472.0, "MOTION / SURFACE");
    let (rm_dir, rm_spd) = comp2vec(p.rstu, p.rstv);
    let (lm_dir, lm_spd) = comp2vec(p.lstu, p.lstv);
    let sfc = profile.sfc;
    let sfc_t = profile.tmpc.get(sfc).copied().unwrap_or(f64::NAN);
    let sfc_td = profile.dwpc.get(sfc).copied().unwrap_or(f64::NAN);
    let extra_rows: [(&str, String, Color32); 4] = [
        ("Bunkers RM", dir_spd(rm_dir, rm_spd), TEXT),
        ("Bunkers LM", dir_spd(lm_dir, lm_spd), TEXT),
        (
            "Sfc T/Td",
            format!(
                "{}/{} F",
                fmt(sfc_t * 9.0 / 5.0 + 32.0, 0),
                fmt(sfc_td * 9.0 / 5.0 + 32.0, 0)
            ),
            TEXT,
        ),
        ("PWAT", format!("{} in", fmt_opt(p.precip_water, 2)), TEXT),
    ];
    for (i, (label, value, color)) in extra_rows.iter().enumerate() {
        key_value_row(
            painter,
            g,
            x0,
            my + 58.0 + i as f32 * 36.0,
            472.0,
            label,
            value,
            *color,
        );
    }
}

// ---- hodograph (1680,44,720,600), spec §6 ----

fn draw_hodograph(painter: &egui::Painter, g: &G, sounding: &NativeSounding) {
    let (rx, ry, rw, _rh) = (1680.0f32, 44.0f32, 720.0f32, 600.0f32);
    painter.rect_filled(g.rect(rx, ry, 720.0, 600.0), 0.0, H_PANEL_BG);
    painter.rect_stroke(
        g.rect(rx, ry, 720.0, 600.0),
        0.0,
        g.stroke(1.0, H_BORDER),
        egui::StrokeKind::Inside,
    );
    painter.text(
        g.pos(rx + rw / 2.0, ry + 3.0),
        Align2::CENTER_TOP,
        "Hodograph (kts)",
        g.font(42.0),
        H_HEADER,
    );
    let plot_top = ry + 32.0;
    let (cx, cy) = (rx + 360.0, plot_top + 281.0);
    let max_r = 273.0f32;
    let scale = max_r / 80.0; // 3.4125 px/kt
    // Rings + labels.
    for ring in [20.0f32, 40.0, 60.0, 80.0] {
        painter.circle_stroke(g.pos(cx, cy), ring * scale * g.s, g.stroke(1.0, H_RING));
        painter.text(
            g.pos(cx + ring * scale + 4.0, cy - 10.0),
            Align2::LEFT_TOP,
            format!("{ring:.0}"),
            g.font(42.0),
            H_RING_LABEL,
        );
    }
    painter.line_segment(
        [g.pos(cx - max_r, cy), g.pos(cx + max_r, cy)],
        g.stroke(1.0, H_AXIS),
    );
    painter.line_segment(
        [g.pos(cx, cy - max_r), g.pos(cx, cy + max_r)],
        g.stroke(1.0, H_AXIS),
    );

    let profile = &sounding.profile;
    let params = &sounding.params;
    let sfc_h = profile.sfc_height();
    let at = |u: f64, v: f64| -> Pos2 { g.pos(cx + (u as f32) * scale, cy - (v as f32) * scale) };

    // Trace: every 100 m, 0–14 km AGL; segment colored by END height band.
    let mut previous: Option<(Pos2, f64)> = None;
    let mut km_dots: Vec<(Pos2, f64)> = Vec::new();
    for step in 0..=140 {
        let h_agl = step as f64 * 100.0;
        let p = profile.pres_at_height(sfc_h + h_agl);
        if !p.is_finite() {
            continue;
        }
        let (u, v) = profile.interp_wind(p);
        if !u.is_finite() || !v.is_finite() {
            previous = None;
            continue;
        }
        let point = at(u, v);
        if let Some((prev, _)) = previous {
            painter.line_segment([prev, point], g.stroke(3.0, band_color(h_agl / 1000.0)));
        }
        if step % 10 == 0 {
            km_dots.push((point, h_agl / 1000.0));
        }
        previous = Some((point, h_agl));
    }
    // Height dots: labeled km {0,1,3,6,9,12}, others small.
    for (point, km) in &km_dots {
        let labeled = matches!(*km as i64, 0 | 1 | 3 | 6 | 9 | 12);
        let radius = if labeled { 7.0 } else { 4.0 } * g.s;
        painter.circle_filled(*point, radius, Color32::WHITE);
        painter.circle_stroke(*point, radius, g.stroke(1.0, band_color(*km)));
        if labeled {
            painter.text(
                *point,
                Align2::CENTER_CENTER,
                format!("{km:.0}"),
                g.font(20.0),
                Color32::from_rgb(10, 10, 22),
            );
        }
    }

    // Storm motion markers.
    let (rm_dir, rm_spd) = comp2vec(params.rstu, params.rstv);
    let rm_point = at(params.rstu, params.rstv);
    if params.rstu.is_finite() {
        painter.circle_filled(rm_point, 6.0 * g.s, H_RM);
        painter.circle_stroke(
            rm_point,
            6.0 * g.s,
            g.stroke(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 200)),
        );
        painter.text(
            rm_point + vec2(11.0 * g.s, -7.0 * g.s),
            Align2::LEFT_TOP,
            format!("{rm_dir:03.0}/{rm_spd:.0} RM"),
            g.font(22.0),
            H_TEXT,
        );
    }
    if params.lstu.is_finite() {
        let (lm_dir, lm_spd) = comp2vec(params.lstu, params.lstv);
        let lm = at(params.lstu, params.lstv);
        let tri = vec![
            lm + vec2(0.0, -8.0 * g.s),
            lm + vec2(-7.0 * g.s, 7.0 * g.s),
            lm + vec2(7.0 * g.s, 7.0 * g.s),
        ];
        painter.add(Shape::convex_polygon(tri, H_LM, Stroke::NONE));
        painter.text(
            lm + vec2(11.0 * g.s, -7.0 * g.s),
            Align2::LEFT_TOP,
            format!("{lm_dir:03.0}/{lm_spd:.0} LM"),
            g.font(22.0),
            H_TEXT,
        );
    }
    // Mean wind (0–6 km npw, spec quirk 5).
    let p_sfc = profile.sfc_pressure();
    let p6km = profile.pres_at_height(profile.to_msl(6000.0));
    if let Ok((mu, mv)) = winds::mean_wind_npw(profile, p_sfc, p6km, -1.0, 0.0, 0.0) {
        let (mw_dir, mw_spd) = comp2vec(mu, mv);
        let mw = at(mu, mv);
        painter.circle_stroke(mw, 5.0 * g.s, g.stroke(1.5, H_MEAN));
        painter.text(
            mw + vec2(9.0 * g.s, -7.0 * g.s),
            Align2::LEFT_TOP,
            format!("MW={mw_dir:03.0}/{mw_spd:.0}"),
            g.font(22.0),
            H_TEXT_DIM,
        );
    }
    // Corfidi vectors: X = upshear, + = downshear.
    if params.corfidi_up_u.is_finite() {
        let (cu_dir, cu_spd) = comp2vec(params.corfidi_up_u, params.corfidi_up_v);
        let cu = at(params.corfidi_up_u, params.corfidi_up_v);
        let r = 5.0 * g.s;
        painter.line_segment(
            [cu + vec2(-r, -r), cu + vec2(r, r)],
            g.stroke(2.0, H_CORFIDI_UP),
        );
        painter.line_segment(
            [cu + vec2(-r, r), cu + vec2(r, -r)],
            g.stroke(2.0, H_CORFIDI_UP),
        );
        painter.text(
            cu + vec2(8.0 * g.s, -7.0 * g.s),
            Align2::LEFT_TOP,
            format!("UP={cu_dir:03.0}/{cu_spd:.0}"),
            g.font(22.0),
            H_TEXT_DIM,
        );
    }
    if params.corfidi_dn_u.is_finite() {
        let (cd_dir, cd_spd) = comp2vec(params.corfidi_dn_u, params.corfidi_dn_v);
        let cd = at(params.corfidi_dn_u, params.corfidi_dn_v);
        let r = 5.0 * g.s;
        painter.line_segment(
            [cd + vec2(-r, 0.0), cd + vec2(r, 0.0)],
            g.stroke(2.0, H_CORFIDI_DN),
        );
        painter.line_segment(
            [cd + vec2(0.0, -r), cd + vec2(0.0, r)],
            g.stroke(2.0, H_CORFIDI_DN),
        );
        painter.text(
            cd + vec2(8.0 * g.s, -7.0 * g.s),
            Align2::LEFT_TOP,
            format!("DN={cd_dir:03.0}/{cd_spd:.0}"),
            g.font(22.0),
            H_TEXT_DIM,
        );
    }
    // SR wind vectors from the RM point (0-2 / 4-6 / 8-10 km, npw − RM).
    if params.rstu.is_finite() {
        let layers: [(&str, f64, f64); 3] = [
            ("0-2", 0.0, 2000.0),
            ("4-6", 4000.0, 6000.0),
            ("8-10", 8000.0, 10000.0),
        ];
        for (i, (label, bot, top)) in layers.iter().enumerate() {
            let pb = profile.pres_at_height(profile.to_msl(*bot));
            let pt = profile.pres_at_height(profile.to_msl(*top));
            let Ok((mu, mv)) = winds::mean_wind_npw(profile, pb, pt, -1.0, 0.0, 0.0) else {
                continue;
            };
            let (su, sv) = (mu - params.rstu, mv - params.rstv);
            let tip = rm_point
                + vec2(
                    (su as f32) * scale * 0.6 * g.s,
                    -(sv as f32) * scale * 0.6 * g.s,
                );
            painter.line_segment([rm_point, tip], g.stroke(2.0, sr_wind_color()));
            // Arrowhead.
            let dir = (tip - rm_point).normalized();
            let perp = vec2(-dir.y, dir.x);
            for side in [1.0f32, -1.0] {
                painter.line_segment(
                    [tip, tip - dir * 8.0 * g.s + perp * 4.0 * g.s * side],
                    g.stroke(2.0, sr_wind_color()),
                );
            }
            painter.text(
                tip + vec2(4.0 * g.s, (i as f32 - 1.0) * 16.0 * g.s),
                Align2::LEFT_TOP,
                *label,
                g.font(22.0),
                H_TEXT_DIM,
            );
        }
    }
    // Critical angle + temperature advection lines.
    let ca = params.critical_angle;
    let ca_color = if (80.0..=120.0).contains(&ca) {
        H_WARN
    } else {
        H_TEXT
    };
    painter.text(
        g.pos(rx + 6.0, ry + 544.0),
        Align2::LEFT_TOP,
        format!("Critical Angle = {} deg", fmt(ca, 0)),
        g.font(28.0),
        ca_color,
    );
    let p3km = profile.pres_at_height(profile.to_msl(3000.0));
    let (dir0, _) = profile.interp_vec(p_sfc);
    let (dir3, _) = profile.interp_vec(p3km);
    let mut veer = dir3 - dir0;
    while veer > 180.0 {
        veer -= 360.0;
    }
    while veer < -180.0 {
        veer += 360.0;
    }
    let (adv_text, adv_color) = if !veer.is_finite() {
        ("Temp Adv: --", H_TEXT_DIM)
    } else if veer > 0.5 {
        ("Temp Adv: WAA (WARM)", H_WARN)
    } else if veer < -0.5 {
        ("Temp Adv: CAA (COLD)", H_CAA)
    } else {
        ("Temp Adv: NEUTRAL", H_TEXT_DIM)
    };
    painter.text(
        g.pos(rx + 6.0, ry + 570.0),
        Align2::LEFT_TOP,
        adv_text,
        g.font(24.0),
        adv_color,
    );
    // Legend (2 cols × 3 rows).
    let bands: [(&str, f64); 6] = [
        ("0-1 km", 0.5),
        ("1-3 km", 2.0),
        ("3-6 km", 4.5),
        ("6-9 km", 7.5),
        ("9-12 km", 10.5),
        ("12+ km", 13.0),
    ];
    for (i, (label, km)) in bands.iter().enumerate() {
        let col = (i / 3) as f32;
        let row = (i % 3) as f32;
        let x = rx + 370.0 + col * 170.0;
        let y = ry + 538.0 + row * 18.0;
        painter.rect_filled(g.rect(x, y, 12.0, 10.0), 0.0, band_color(*km));
        painter.text(
            g.pos(x + 16.0, y - 2.0),
            Align2::LEFT_TOP,
            *label,
            g.font(20.0),
            H_TEXT_DIM,
        );
    }
}

// ---- storm slinky (1680,644,720,520), spec §7 ----

fn draw_slinky(painter: &egui::Painter, g: &G, sounding: &NativeSounding) {
    let (rx, ry, rw, rh) = (1680.0f32, 644.0f32, 720.0f32, 520.0f32);
    painter.rect_filled(g.rect(rx, ry, rw, rh), 0.0, H_PANEL_BG);
    painter.rect_stroke(
        g.rect(rx, ry, rw, rh),
        0.0,
        g.stroke(1.0, Color32::from_rgb(60, 60, 85)),
        egui::StrokeKind::Inside,
    );
    painter.text(
        g.pos(rx + rw / 2.0, ry + 5.0),
        Align2::CENTER_TOP,
        "Storm Slinky",
        g.font(42.0),
        Color32::from_rgb(0, 255, 255),
    );
    painter.line_segment(
        [g.pos(rx + 2.0, ry + 30.0), g.pos(rx + rw - 2.0, ry + 30.0)],
        g.stroke(1.0, Color32::from_rgb(60, 60, 85)),
    );
    let profile = &sounding.profile;
    let params = &sounding.params;
    if !params.rstu.is_finite() || !params.rstv.is_finite() {
        painter.text(
            g.pos(rx + rw / 2.0, ry + rh / 2.0),
            Align2::CENTER_CENTER,
            "No Data",
            g.font(28.0),
            Color32::from_rgb(140, 140, 150),
        );
        return;
    }
    // Points from the SB parcel trace: SR displacement (kt) by height band.
    let sfc_h = profile.sfc_height();
    let mut points: Vec<(f64, f64, f64)> = Vec::new(); // (sr_u, sr_v, h_agl)
    for &p in &params.sfcpcl.ptrace {
        if !p.is_finite() {
            continue;
        }
        let h_agl = profile.interp_hght(p) - sfc_h;
        if !h_agl.is_finite() || h_agl < 0.0 {
            continue;
        }
        let (u, v) = profile.interp_wind(p);
        if !u.is_finite() || !v.is_finite() {
            continue;
        }
        points.push((u - params.rstu, v - params.rstv, h_agl));
    }
    if points.is_empty() {
        return;
    }
    let plot_top = ry + 38.0;
    let (cx, cy) = (rx + 360.0, plot_top + 223.0);
    let max_disp = points
        .iter()
        .map(|(u, v, _)| vector_mag(*u, *v))
        .fold(4.0f64, f64::max);
    let scale = (215.0 / max_disp as f32).max(0.01);
    // Crosshairs + rings.
    let cross = Color32::from_rgb(45, 45, 65);
    painter.line_segment(
        [g.pos(cx - 223.0, cy), g.pos(cx + 223.0, cy)],
        g.stroke(1.0, cross),
    );
    painter.line_segment(
        [g.pos(cx, cy - 223.0), g.pos(cx, cy + 223.0)],
        g.stroke(1.0, cross),
    );
    for frac in [0.33f32, 0.66] {
        let r = frac * max_disp as f32 * scale;
        if r > 4.0 {
            painter.circle_stroke(
                g.pos(cx, cy),
                r * g.s,
                g.stroke(1.0, Color32::from_rgb(40, 40, 60)),
            );
        }
    }
    painter.text(
        g.pos(rx + 12.0, plot_top + 4.0),
        Align2::LEFT_TOP,
        format!("{max_disp:.0} kt radius"),
        g.font(28.0),
        Color32::from_rgb(140, 140, 150),
    );
    // Connectors + dots.
    let slinky_color = |km: f64| -> Color32 {
        if km < 3.0 {
            Color32::from_rgb(255, 80, 80)
        } else if km < 6.0 {
            Color32::from_rgb(80, 255, 80)
        } else if km < 9.0 {
            Color32::from_rgb(80, 160, 255)
        } else {
            Color32::from_rgb(200, 100, 255)
        }
    };
    let mut prev: Option<Pos2> = None;
    for (u, v, h) in &points {
        let pos = g.pos(cx + *u as f32 * scale, cy - *v as f32 * scale);
        if let Some(prev_pos) = prev {
            painter.line_segment(
                [prev_pos, pos],
                g.stroke(2.0, Color32::from_rgba_unmultiplied(120, 120, 150, 200)),
            );
        }
        painter.circle_filled(pos, 7.0 * g.s, slinky_color(h / 1000.0));
        painter.circle_stroke(pos, 7.0 * g.s, g.stroke(1.0, Color32::WHITE));
        prev = Some(pos);
    }
    // Legend.
    let legend: [(&str, f64); 4] = [
        ("< 3 km", 1.0),
        ("3-6 km", 4.5),
        ("6-9 km", 7.5),
        ("9+ km", 10.0),
    ];
    for (i, (label, km)) in legend.iter().enumerate() {
        let y = ry + rh - 4.0 * 24.0 - 10.0 + i as f32 * 24.0;
        painter.circle_filled(g.pos(rx + 14.0, y + 8.0), 5.0 * g.s, slinky_color(*km));
        painter.text(
            g.pos(rx + 28.0, y),
            Align2::LEFT_TOP,
            *label,
            g.font(22.0),
            Color32::from_rgb(230, 230, 230),
        );
    }
}

// ---- parameter table (0,1164,2400,636), spec §9 ----

fn draw_table(painter: &egui::Painter, g: &G, sounding: &NativeSounding) {
    let profile = &sounding.profile;
    let p = &sounding.params;
    let e = &sounding.verified_ecape;
    // Panels.
    for (x, w) in [(10.0f32, 1100.0f32), (1130.0, 590.0), (1750.0, 630.0)] {
        painter.rect_filled(g.rect(x, 1174.0, w, 616.0), 0.0, PANEL_BG);
    }
    for x in [1119.0f32, 1739.0] {
        painter.line_segment(
            [g.pos(x, 1178.0), g.pos(x, 1775.0)],
            g.stroke(1.0, LINE_DIM),
        );
    }

    // === PARCELS ===
    let (px0, py0) = (20.0f32, 1184.0f32);
    section_title(painter, g, px0, py0, 1060.0, "PARCELS");
    let header_y = py0 + 54.0;
    let anchors: [(f32, &str, &str); 9] = [
        (182.0, "ECAPE", "J/kg"),
        (282.0, "NCAPE", ""),
        (374.0, "CAPE", "J/kg"),
        (468.0, "3CAPE", "J/kg"),
        (562.0, "6CAPE", "J/kg"),
        (650.0, "CINH", "J/kg"),
        (744.0, "LCL", "m"),
        (846.0, "LFC", "m"),
        (948.0, "EL", "m"),
    ];
    painter.text(
        g.pos(px0, header_y),
        Align2::LEFT_TOP,
        "PCL",
        g.font(28.0),
        LABEL,
    );
    for (anchor, name, units) in &anchors {
        painter.text(
            g.pos(px0 + anchor, header_y),
            Align2::RIGHT_TOP,
            *name,
            g.font(28.0),
            LABEL,
        );
        painter.text(
            g.pos(px0 + anchor, header_y + 26.0),
            Align2::RIGHT_TOP,
            *units,
            g.font(22.0),
            MUTED,
        );
    }
    painter.line_segment(
        [
            g.pos(px0, header_y + 46.0),
            g.pos(px0 + 1030.0, header_y + 46.0),
        ],
        g.stroke(1.0, LINE_DIM),
    );
    let parcels = [
        ("Surface", &e.surface_based, &p.sfcpcl),
        ("Mixed-Layer", &e.mixed_layer, &p.mlpcl),
        ("Most-Unstbl", &e.most_unstable, &p.mupcl),
    ];
    for (i, (label, ecape, parcel)) in parcels.iter().enumerate() {
        let y = header_y + 58.0 + i as f32 * 48.0;
        painter.text(g.pos(px0, y), Align2::LEFT_TOP, *label, g.font(28.0), TEXT);
        let cape_v = if parcel.bplus.is_finite() {
            parcel.bplus
        } else {
            ecape.cape
        };
        let cape3 = if parcel.b3km.is_finite() {
            parcel.b3km
        } else {
            ecape.cape_3km
        };
        let cape6 = if parcel.b6km.is_finite() {
            parcel.b6km
        } else {
            ecape.cape_6km
        };
        let cinh = if parcel.bminus.is_finite() {
            parcel.bminus
        } else {
            ecape.cinh
        };
        let cells: [(f32, String, Color32); 9] = [
            (182.0, fmt(ecape.ecape, 0), cape_color(ecape.ecape)),
            (282.0, fmt(ecape.ncape, 2), TEXT),
            (374.0, fmt(cape_v, 0), cape_color(cape_v)),
            (468.0, fmt(cape3, 0), TEXT),
            (562.0, fmt(cape6, 0), TEXT),
            (650.0, fmt(cinh, 0), cin_color(cinh)),
            (744.0, fmt(parcel.lclhght, 0), TEXT),
            (846.0, fmt(parcel.lfchght, 0), TEXT),
            (948.0, fmt(parcel.elhght, 0), TEXT),
        ];
        for (anchor, value, color) in &cells {
            painter.text(
                g.pos(px0 + anchor, y),
                Align2::RIGHT_TOP,
                value,
                g.font(28.0),
                *color,
            );
        }
    }

    // === STORM MOTIONS ===
    let (mx, my) = (20.0f32, 1448.0f32);
    section_title(painter, g, mx, my, 370.0, "STORM MOTIONS");
    let (rm_dir, rm_spd) = comp2vec(p.rstu, p.rstv);
    let (lm_dir, lm_spd) = comp2vec(p.lstu, p.lstv);
    let (cu_dir, cu_spd) = comp2vec(p.corfidi_up_u, p.corfidi_up_v);
    let (cd_dir, cd_spd) = comp2vec(p.corfidi_dn_u, p.corfidi_dn_v);
    let motions: [(&str, String, Color32); 6] = [
        ("Bunkers RM", dir_spd(rm_dir, rm_spd), TEXT),
        ("Bunkers LM", dir_spd(lm_dir, lm_spd), TEXT),
        ("Corfidi Down", dir_spd(cd_dir, cd_spd), TEXT),
        ("Corfidi Up", dir_spd(cu_dir, cu_spd), TEXT),
        ("1km wind", dir_spd(p.wind_1km.0, p.wind_1km.1), GOOD),
        ("6km wind", dir_spd(p.wind_6km.0, p.wind_6km.1), GOOD),
    ];
    for (i, (label, value, color)) in motions.iter().enumerate() {
        let gap = if i >= 4 { 8.0 } else { 0.0 };
        key_value_row(
            painter,
            g,
            mx,
            my + 56.0 + i as f32 * 46.0 + gap,
            320.0,
            label,
            value,
            *color,
        );
    }

    // === LAPSE RATES ===
    let (lx, ly) = (450.0f32, 1448.0f32);
    section_title(painter, g, lx, ly, 600.0, "LAPSE RATES");
    let lr03 = p.lr03.unwrap_or(f64::NAN);
    let lr36 = p.lr36.unwrap_or(f64::NAN);
    let sfc_lcl_lr = if p.sfcpcl.lclhght.is_finite() && p.sfcpcl.lclhght > 1.0 {
        let lcl_env_tmpc = profile.interp_tmpc(p.sfcpcl.lclpres);
        (profile.tmpc[profile.sfc] - lcl_env_tmpc) / p.sfcpcl.lclhght * 1000.0
    } else {
        f64::NAN
    };
    let lapse_rows: [(&str, f64); 6] = [
        ("Sfc-LCL", sfc_lcl_lr),
        (
            "950-850 mb",
            indices::lapse_rate(profile, 950.0, 850.0, true).unwrap_or(f64::NAN),
        ),
        ("Sfc-3km", lr03),
        ("3km-6km", lr36),
        ("850-500 mb", p.lr85.unwrap_or(f64::NAN)),
        ("700-500 mb", p.lr75.unwrap_or(f64::NAN)),
    ];
    for (i, (label, value)) in lapse_rows.iter().enumerate() {
        key_value_row(
            painter,
            g,
            lx,
            ly + 56.0 + i as f32 * 46.0,
            260.0,
            label,
            &format!("{} C/km", fmt(*value, 1)),
            lapse_color(*value),
        );
    }

    // === SHEAR / HELICITY ===
    let (sx, sy) = (1144.0f32, 1184.0f32);
    section_title(painter, g, sx, sy, 560.0, "SHEAR / HELICITY");
    let sh_header = sy + 54.0;
    let sh_anchors: [(f32, &str, &str); 5] = [
        (190.0, "EHI", ""),
        (270.0, "SRH", "m2/s2"),
        (350.0, "Shear", "kt"),
        (430.0, "Mean", "kt"),
        (540.0, "SRWind", "deg/kt"),
    ];
    painter.text(
        g.pos(sx, sh_header),
        Align2::LEFT_TOP,
        "Layer",
        g.font(28.0),
        LABEL,
    );
    for (anchor, name, units) in &sh_anchors {
        painter.text(
            g.pos(sx + anchor, sh_header),
            Align2::RIGHT_TOP,
            *name,
            g.font(28.0),
            LABEL,
        );
        painter.text(
            g.pos(sx + anchor, sh_header + 26.0),
            Align2::RIGHT_TOP,
            *units,
            g.font(22.0),
            MUTED,
        );
    }
    painter.line_segment(
        [
            g.pos(sx, sh_header + 46.0),
            g.pos(sx + 540.0, sh_header + 46.0),
        ],
        g.stroke(1.0, LINE_DIM),
    );
    let p_sfc = profile.sfc_pressure();
    let p500m = profile.pres_at_height(profile.to_msl(500.0));
    let p1km = profile.pres_at_height(profile.to_msl(1000.0));
    let p2km = profile.pres_at_height(profile.to_msl(2000.0));
    let p3km = profile.pres_at_height(profile.to_msl(3000.0));
    let p6km = profile.pres_at_height(profile.to_msl(6000.0));
    let agl = |pressure: f64| -> f64 {
        if pressure.is_finite() {
            let h = profile.interp_hght(pressure);
            if h.is_finite() {
                profile.to_agl(h)
            } else {
                f64::NAN
            }
        } else {
            f64::NAN
        }
    };
    let layers: [(&str, f64, f64, f64, f64); 8] = [
        ("Sfc-500m", p_sfc, p500m, 0.0, 500.0),
        ("Sfc-1km", p_sfc, p1km, 0.0, 1000.0),
        (
            "Eff Inflow",
            p.eff_inflow.0,
            p.eff_inflow.1,
            agl(p.eff_inflow.0),
            agl(p.eff_inflow.1),
        ),
        ("Sfc-3km", p_sfc, p3km, 0.0, 3000.0),
        ("1km-3km", p1km, p3km, 1000.0, 3000.0),
        ("3km-6km", p3km, p6km, 3000.0, 6000.0),
        ("Sfc-6km", p_sfc, p6km, 0.0, 6000.0),
        ("Sfc-2km", p_sfc, p2km, 0.0, 2000.0),
    ];
    for (i, (label, pbot, ptop, hbot, htop)) in layers.iter().enumerate() {
        let y = sh_header + 58.0 + i as f32 * 48.0;
        painter.text(g.pos(sx, y), Align2::LEFT_TOP, *label, g.font(28.0), TEXT);
        let valid = pbot.is_finite() && ptop.is_finite() && hbot.is_finite() && htop.is_finite();
        let (ehi, srh, shear, mean, srw) = if valid {
            let srh = winds::helicity(profile, *hbot, *htop, p.rstu, p.rstv, -1.0, false)
                .map(|value| value.0)
                .unwrap_or(f64::NAN);
            let ehi = composites::ehi(p.sfcpcl.bplus, srh).unwrap_or(f64::NAN);
            let srw = winds::sr_wind(profile, *pbot, *ptop, p.rstu, p.rstv, -1.0)
                .map(|(su, sv)| comp2vec(su, sv))
                .unwrap_or((f64::NAN, f64::NAN));
            (
                ehi,
                srh,
                shear_mag(profile, *pbot, *ptop),
                mean_wind_mag(profile, *pbot, *ptop),
                srw,
            )
        } else {
            (f64::NAN, f64::NAN, f64::NAN, f64::NAN, (f64::NAN, f64::NAN))
        };
        let cells: [(f32, String, Color32); 5] = [
            (190.0, fmt(ehi, 1), TEXT),
            (270.0, fmt(srh, 0), srh_color(srh)),
            (350.0, fmt(shear, 0), shear_color(shear)),
            (430.0, fmt(mean, 0), TEXT),
            (540.0, dir_spd(srw.0, srw.1), TEXT),
        ];
        for (anchor, value, color) in &cells {
            painter.text(
                g.pos(sx + anchor, y - 2.0),
                Align2::RIGHT_TOP,
                value,
                g.font(28.0),
                *color,
            );
        }
    }

    // === THERMODYNAMICS ===
    let (tx, ty) = (1764.0f32, 1184.0f32);
    section_title(painter, g, tx, ty, 610.0, "THERMODYNAMICS");
    let sfc_rh = profile.relh.get(profile.sfc).copied().unwrap_or(f64::NAN);
    let (dgz_bot, dgz_top) = indices::dgz(profile);
    let dgz_rh = indices::mean_relh(profile, Some(dgz_bot), Some(dgz_top)).unwrap_or(f64::NAN);
    let (_, lcl_temp_c) = cape::lcl(p_sfc, profile.tmpc[profile.sfc], profile.dwpc[profile.sfc]);
    let thermo_left: [(&str, String, Color32); 9] = [
        ("PWAT", format!("{} in", fmt_opt(p.precip_water, 2)), TEXT),
        (
            "Mean MixR",
            format!("{} g/kg", fmt_opt(p.mean_mixr, 1)),
            GOOD,
        ),
        ("Sfc RH", format!("{} %", fmt(sfc_rh, 0)), TEXT),
        ("Low RH", format!("{} %", fmt_opt(p.mean_rh_low, 0)), TEXT),
        ("Mid RH", format!("{} %", fmt_opt(p.mean_rh_mid, 0)), TEXT),
        ("DGZ RH", format!("{} %", fmt(dgz_rh, 0)), TEXT),
        ("Freezing", format!("{} m", fmt_opt(p.frz_lvl, 0)), TEXT),
        ("WB Zero", format!("{} m", fmt_opt(p.wb_zero, 0)), GOOD),
        ("MU MPL", format!("{} m", fmt(p.mupcl.mplhght, 0)), TEXT),
    ];
    let thermo_right: [(&str, String, Color32); 9] = [
        ("3km Theta", format!("{} K", fmt_opt(p.tei, 1)), TEXT),
        ("LCL Temp", format!("{} C", fmt(lcl_temp_c, 1)), TEXT),
        ("ConvT", format!("{} C", fmt_opt(p.conv_t, 1)), TEXT),
        ("MaxT", format!("{} C", fmt_opt(p.max_temp, 1)), TEXT),
        ("K Index", fmt_opt(p.k_index, 1), TEXT),
        ("TotTots", fmt_opt(p.t_totals, 1), TEXT),
        ("TEI", fmt_opt(p.tei, 1), TEXT),
        (
            "TEHI",
            fmt_opt(p.tehi, 1),
            stp_color(p.tehi.unwrap_or(f64::NAN)),
        ),
        (
            "TTS",
            fmt_opt(p.tts, 1),
            stp_color(p.tts.unwrap_or(f64::NAN)),
        ),
    ];
    for (i, (label, value, color)) in thermo_left.iter().enumerate() {
        key_value_row(
            painter,
            g,
            tx,
            ty + 56.0 + i as f32 * 34.0,
            245.0,
            label,
            value,
            *color,
        );
    }
    for (i, (label, value, color)) in thermo_right.iter().enumerate() {
        key_value_row(
            painter,
            g,
            tx + 310.0,
            ty + 56.0 + i as f32 * 34.0,
            275.0,
            label,
            value,
            *color,
        );
    }

    // === COMPOSITES ===
    let (cx0, cy0) = (1764.0f32, 1542.0f32);
    section_title(painter, g, cx0, cy0, 610.0, "COMPOSITES");
    let lr03_v = p
        .lr03
        .unwrap_or_else(|| indices::lapse_rate(profile, 0.0, 3000.0, false).unwrap_or(f64::NAN));
    let p3500m = profile.pres_at_height(profile.to_msl(3500.0));
    let mean_wind_1_35_ms = mean_wind_mag(profile, p1km, p3500m) * 0.514_444;
    let wndg = composites::wndg(p.mlpcl.bplus, lr03_v, mean_wind_1_35_ms, p.mlpcl.bminus)
        .unwrap_or(f64::NAN);
    let shr06_mag = vector_mag(p.shr06.0, p.shr06.1);
    let mean06_mag = vector_mag(p.mean_wind_06.0, p.mean_wind_06.1);
    let dcp =
        composites::dcp(p.dcape.dcape, p.mupcl.bplus, shr06_mag, mean06_mag).unwrap_or(f64::NAN);
    let esp = composites::esp(p.mlpcl.b3km, lr03_v, p.mlpcl.bplus).unwrap_or(f64::NAN);
    let down_t = p.dcape.ttrace.last().copied().unwrap_or(f64::NAN);
    let comp_left: [(&str, String, Color32); 6] = [
        (
            "STP cin",
            fmt_opt(p.stp_cin, 1),
            stp_color(p.stp_cin.unwrap_or(f64::NAN)),
        ),
        (
            "STP fixed",
            fmt_opt(p.stp_fixed, 1),
            stp_color(p.stp_fixed.unwrap_or(f64::NAN)),
        ),
        (
            "Supercell",
            fmt_opt(p.scp, 1),
            scp_color(p.scp.unwrap_or(f64::NAN)),
        ),
        (
            "SHIP",
            fmt_opt(p.ship, 1),
            ship_color(p.ship.unwrap_or(f64::NAN)),
        ),
        ("DCP", fmt(dcp, 1), TEXT),
        ("WNDG", fmt(wndg, 1), TEXT),
    ];
    let comp_right: [(&str, String, Color32); 6] = [
        (
            "VTP mod",
            fmt_opt(p.vtp_mod, 1),
            stp_color(p.vtp_mod.unwrap_or(f64::NAN)),
        ),
        ("DCAPE", format!("{} J/kg", fmt(p.dcape.dcape, 0)), TEXT),
        ("DownT", format!("{} C", fmt(down_t, 1)), TEXT),
        ("ESP", fmt(esp, 1), TEXT),
        ("SigSvr", "--".to_owned(), MUTED),
        ("LHP", "--".to_owned(), MUTED),
    ];
    for (i, (label, value, color)) in comp_left.iter().enumerate() {
        key_value_row(
            painter,
            g,
            cx0,
            cy0 + 56.0 + i as f32 * 34.0,
            245.0,
            label,
            value,
            *color,
        );
    }
    for (i, (label, value, color)) in comp_right.iter().enumerate() {
        key_value_row(
            painter,
            g,
            cx0 + 310.0,
            cy0 + 56.0 + i as f32 * 34.0,
            275.0,
            label,
            value,
            *color,
        );
    }
}
