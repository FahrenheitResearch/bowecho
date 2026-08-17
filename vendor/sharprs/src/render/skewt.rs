//! Skew-T Log-P diagram renderer for sharprs.
//!
//! Produces a SHARPpy-style dark-background Skew-T diagram with hodograph
//! and text parameter panels.  Matches the reference output from the
//! wxsection.com HRRR sounding product.
//!
//! # Required elements (all 20)
//!
//! 1.  Log-pressure Y axis (1000-100 hPa)
//! 2.  Skewed temperature X axis
//! 3.  Isobars (gray horizontal lines at standard levels)
//! 4.  Isotherms (gray skewed lines, 0 C highlighted blue)
//! 5.  Dry adiabats (brown/tan curves)
//! 6.  Moist adiabats (green curves)
//! 7.  Mixing ratio lines (green dashed)
//! 8.  RED temperature trace (solid, thick)
//! 9.  GREEN dewpoint trace (solid, thick)
//! 10. GOLD/ORANGE parcel trace (dashed) for ML and MU parcels
//! 11. LCL, LFC, EL labels on diagram (colored text)
//! 12. Height markers on left (0-15 km) in CYAN
//! 13. Effective inflow layer bracket (CYAN vertical bar)
//! 14. CAPE shading (red/orange fill)
//! 15. CIN shading (blue fill)
//! 16. CYAN wind barbs on right side
//! 17. Surface temperature labels in Fahrenheit
//! 18. Omega profile on far left (green line)
//! 19. Wet-bulb temperature trace (cyan, thin)
//! 20. DCAPE downdraft trace (magenta dashed)

use crate::constants::*;
use crate::params::cape::{self, DcapeResult, ParcelResult, ParcelType};
use crate::profile::Profile;
use crate::render::canvas::{Canvas, FONT_H};

// =========================================================================
// Layout constants
// =========================================================================

/// Fraction of total width for the Skew-T diagram.
const SKEWT_FRAC: f64 = 0.70;

// Margins within the Skew-T area
const MARGIN_LEFT: f64 = 70.0; // room for omega profile + height labels
const MARGIN_RIGHT: f64 = 55.0; // room for wind barbs
const MARGIN_TOP: f64 = 28.0;
const MARGIN_BOT: f64 = 38.0; // extra room for surface temp labels

// Pressure range
const P_TOP: f64 = 100.0;
const P_BOT: f64 = 1050.0;

// Temperature range at bottom of diagram
const T_MIN: f64 = -40.0;
const T_MAX: f64 = 50.0;

/// Skew factor: how much temperature axis shifts per unit log-pressure.
const SKEW: f64 = 1.0;

// Standard pressure levels for isobars
const STD_PRESSURES: &[f64] = &[
    1000.0, 925.0, 850.0, 700.0, 500.0, 400.0, 300.0, 250.0, 200.0, 150.0, 100.0,
];

// Labeled pressure levels (the most important ones get labels)
const LABELED_PRESSURES: &[f64] = &[1000.0, 850.0, 700.0, 500.0, 300.0, 200.0, 100.0];

// =========================================================================
// Color palette (SHARPpy-inspired dark background) — BRIGHTER
// =========================================================================

const COL_BG: [u8; 4] = [0, 0, 0, 255];
const COL_GRID: [u8; 4] = [38, 42, 52, 175];
const COL_GRID_ZERO: [u8; 4] = [90, 135, 230, 210];
const COL_ISOBAR: [u8; 4] = [105, 105, 105, 225];
const COL_DRY_AD: [u8; 4] = [120, 90, 55, 70];
const COL_MOIST_AD: [u8; 4] = [35, 120, 70, 62];
const COL_MIX_RATIO: [u8; 4] = [35, 110, 65, 55];
const COL_TEMP: [u8; 4] = [255, 50, 50, 255]; // bright red
const COL_DEWP: [u8; 4] = [50, 255, 50, 255]; // bright green
const COL_WETBULB: [u8; 4] = [0, 220, 220, 180];
const COL_PARCEL_ML: [u8; 4] = [255, 210, 50, 255]; // bright gold
const COL_PARCEL_MU: [u8; 4] = [255, 165, 0, 230]; // bright orange
const COL_DCAPE: [u8; 4] = [220, 80, 220, 200]; // magenta
const COL_CAPE_FILL: [u8; 4] = [255, 60, 40, 70]; // semi-transparent red
const COL_CIN_FILL: [u8; 4] = [60, 80, 255, 60]; // semi-transparent blue
const COL_WIND_BARB: [u8; 4] = [0, 220, 220, 255]; // cyan
const COL_LABEL: [u8; 4] = [200, 200, 210, 255]; // brighter labels
const COL_LABEL_LARGE: [u8; 4] = [220, 220, 230, 255]; // for large axis labels
const COL_TEXT: [u8; 4] = [230, 230, 230, 255];
const COL_TEXT_DIM: [u8; 4] = [140, 140, 150, 255];
const COL_TEXT_HEADER: [u8; 4] = [100, 180, 255, 255];
const COL_PANEL_BG: [u8; 4] = [0, 0, 0, 255];
const COL_PANEL_BORDER: [u8; 4] = [215, 215, 215, 255];
const COL_HEIGHT_MARK: [u8; 4] = [0, 220, 220, 255]; // cyan
const COL_EFF_INFLOW: [u8; 4] = [0, 220, 220, 220]; // cyan, more opaque
const COL_OMEGA: [u8; 4] = [40, 200, 40, 200]; // green
const COL_LCL_LABEL: [u8; 4] = [0, 255, 0, 255]; // brighter green
const COL_LFC_LABEL: [u8; 4] = [255, 255, 0, 255]; // yellow
const COL_EL_LABEL: [u8; 4] = [255, 100, 255, 255]; // brighter magenta
const COL_LABEL_BG: [u8; 4] = [0, 0, 0, 215]; // dark background box

// Hodograph colors
const COL_HODO_0_3: [u8; 4] = [255, 60, 60, 255];
const COL_HODO_3_6: [u8; 4] = [60, 220, 60, 255];
const COL_HODO_6_9: [u8; 4] = [60, 120, 255, 255];
const COL_HODO_9_12: [u8; 4] = [180, 80, 220, 255];
const COL_HODO_RING: [u8; 4] = [55, 55, 65, 255];
const COL_HODO_BUNKERS: [u8; 4] = [255, 200, 60, 255];
const COL_HODO_MEAN: [u8; 4] = [200, 200, 200, 255];

// =========================================================================
// Coordinate transforms
// =========================================================================

/// Normalized Y from pressure (0 = bottom, 1 = top).
#[inline]
fn y_from_p(p: f64) -> f64 {
    (P_BOT.ln() - p.ln()) / (P_BOT.ln() - P_TOP.ln())
}

/// Screen coords within the Skew-T plot area.
#[inline]
fn tp_to_screen(t: f64, p: f64, plot_w: f64, plot_h: f64) -> (f64, f64) {
    let yn = y_from_p(p);
    let t_shifted = t + SKEW * (P_BOT.ln() - p.ln()) * 25.0;
    let xn = (t_shifted - T_MIN) / (T_MAX - T_MIN);
    let sx = MARGIN_LEFT + xn * plot_w;
    let sy = MARGIN_TOP + (1.0 - yn) * plot_h;
    (sx, sy)
}

// =========================================================================
// Thermodynamic helpers (for background lines only -- parcel traces use
// the proper cape module)
// =========================================================================

fn sat_vapor_pressure(temp_c: f64) -> f64 {
    6.112 * ((17.67 * temp_c) / (temp_c + 243.5)).exp()
}

fn sat_mixing_ratio(temp_c: f64, pres_mb: f64) -> f64 {
    let es = sat_vapor_pressure(temp_c);
    0.622 * es / (pres_mb - es).max(0.1)
}

fn moist_lapse_rate(temp_c: f64, pres_mb: f64) -> f64 {
    if !temp_c.is_finite() || !pres_mb.is_finite() || pres_mb <= 0.0 {
        return G / CP;
    }
    let t_k = temp_c + ZEROCNK;
    if t_k <= 0.0 {
        return G / CP;
    }
    let ws = sat_mixing_ratio(temp_c, pres_mb);
    let numer = 1.0 + LV * ws / (RD * t_k);
    let denom = 1.0 + LV * LV * ws / (CP * RV * t_k * t_k);
    if denom.abs() < 1e-10 {
        return G / CP;
    }
    let result = (G / CP) * numer / denom;
    if result.is_finite() {
        result
    } else {
        G / CP
    }
}

fn temp_from_theta(theta_k: f64, pres_mb: f64) -> f64 {
    theta_k * (pres_mb / 1000.0).powf(ROCP) - ZEROCNK
}

fn dewpoint_from_mixing_ratio(w: f64, pres_mb: f64) -> f64 {
    let es = w * pres_mb / (0.622 + w);
    let es = es.max(0.001);
    let val = es.ln() - 6.112_f64.ln();
    let denom = 17.67 - val;
    if denom.abs() < 0.001 {
        return -40.0;
    }
    243.5 * val / denom
}

/// Celsius to Fahrenheit.
#[inline]
fn ctof(tc: f64) -> f64 {
    tc * 9.0 / 5.0 + 32.0
}

/// (wdir, wspd) from (u, v).
#[inline]
fn comp2vec(u: f64, v: f64) -> (f64, f64) {
    let wspd = (u * u + v * v).sqrt();
    if wspd < 1e-10 {
        return (0.0, 0.0);
    }
    let mut wdir = u.atan2(v).to_degrees() + 180.0;
    if wdir >= 360.0 {
        wdir -= 360.0;
    }
    (wdir, wspd)
}

// =========================================================================
// Computed parameters for the text panel
// =========================================================================

#[derive(Debug, Clone, Default)]
struct SkewTParams {
    sb_cape: f64,
    sb_cin: f64,
    ml_cape: f64,
    ml_cin: f64,
    mu_cape: f64,
    mu_cin: f64,
    lcl_hgt: f64,
    lfc_hgt: f64,
    el_hgt: f64,
    lr_0_3: f64,
    lr_3_6: f64,
    lr_700_500: f64,
    shear_0_1: f64,
    shear_0_3: f64,
    shear_0_6: f64,
    #[allow(dead_code)]
    srh_0_1: f64,
    #[allow(dead_code)]
    srh_0_3: f64,
    pwat: f64,
    k_index: f64,
    total_totals: f64,
    dcape: f64,
    // Bunkers storm motion (u, v)
    bunkers_rm: (f64, f64),
    bunkers_lm: (f64, f64),
    mean_wind: (f64, f64),
    // Parcel results for rendering traces
    sb_pcl: Option<ParcelResult>,
    ml_pcl: Option<ParcelResult>,
    mu_pcl: Option<ParcelResult>,
    dcape_result: Option<DcapeResult>,
    // Effective inflow layer
    eff_inflow_bot: f64,
    eff_inflow_top: f64,
}

fn compute_skewt_params(prof: &Profile, cape_prof: &cape::Profile) -> SkewTParams {
    let mut params = SkewTParams::default();

    // Surface-based parcel
    let sb_lpl = cape::define_parcel(cape_prof, ParcelType::Surface);
    let sb_pcl = cape::parcelx(cape_prof, &sb_lpl, None, None);
    params.sb_cape = if sb_pcl.bplus.is_nan() {
        0.0
    } else {
        sb_pcl.bplus
    };
    params.sb_cin = if sb_pcl.bminus.is_nan() {
        0.0
    } else {
        sb_pcl.bminus
    };
    params.lcl_hgt = if sb_pcl.lclhght.is_nan() {
        0.0
    } else {
        sb_pcl.lclhght
    };
    params.lfc_hgt = if sb_pcl.lfchght.is_nan() {
        0.0
    } else {
        sb_pcl.lfchght
    };
    params.el_hgt = if sb_pcl.elhght.is_nan() {
        0.0
    } else {
        sb_pcl.elhght
    };

    // Mixed-layer parcel (100 hPa depth)
    let ml_lpl = cape::define_parcel(cape_prof, ParcelType::MixedLayer { depth_hpa: 100.0 });
    let ml_pcl = cape::parcelx(cape_prof, &ml_lpl, None, None);
    params.ml_cape = if ml_pcl.bplus.is_nan() {
        0.0
    } else {
        ml_pcl.bplus
    };
    params.ml_cin = if ml_pcl.bminus.is_nan() {
        0.0
    } else {
        ml_pcl.bminus
    };

    // Most-unstable parcel (300 hPa depth)
    let mu_lpl = cape::define_parcel(cape_prof, ParcelType::MostUnstable { depth_hpa: 300.0 });
    let mu_pcl = cape::parcelx(cape_prof, &mu_lpl, None, None);
    params.mu_cape = if mu_pcl.bplus.is_nan() {
        0.0
    } else {
        mu_pcl.bplus
    };
    params.mu_cin = if mu_pcl.bminus.is_nan() {
        0.0
    } else {
        mu_pcl.bminus
    };

    // DCAPE
    let dcape_res = cape::dcape(cape_prof);
    params.dcape = dcape_res.dcape;

    // Effective inflow layer
    let (eff_bot, eff_top) = cape::effective_inflow_layer(cape_prof, 100.0, -250.0, Some(&mu_pcl));
    params.eff_inflow_bot = eff_bot;
    params.eff_inflow_top = eff_top;

    // Store parcels for trace rendering
    params.sb_pcl = Some(sb_pcl);
    params.ml_pcl = Some(ml_pcl);
    params.mu_pcl = Some(mu_pcl);
    params.dcape_result = Some(dcape_res);

    // Lapse rates from profile
    let sfc_h = prof.hght[prof.sfc];
    if prof.num_levels() > 2 {
        let t_sfc = prof.tmpc[prof.sfc];
        let h_3km_msl = sfc_h + 3000.0;
        let h_6km_msl = sfc_h + 6000.0;
        let t_3km = prof.interp_by_pressure(&prof.tmpc, prof.pres_at_height(h_3km_msl));
        let t_6km = prof.interp_by_pressure(&prof.tmpc, prof.pres_at_height(h_6km_msl));
        if t_3km.is_finite() {
            params.lr_0_3 = (t_sfc - t_3km) / 3.0;
        }
        if t_3km.is_finite() && t_6km.is_finite() {
            params.lr_3_6 = (t_3km - t_6km) / 3.0;
        }
        let t_700 = prof.interp_tmpc(700.0);
        let t_500 = prof.interp_tmpc(500.0);
        let h_700 = prof.interp_hght(700.0);
        let h_500 = prof.interp_hght(500.0);
        if t_700.is_finite() && t_500.is_finite() && h_700.is_finite() && h_500.is_finite() {
            params.lr_700_500 = (t_700 - t_500) / ((h_500 - h_700) / 1000.0);
        }
    }

    // Bulk shear from profile wind components
    let sfc_u = prof.u[prof.sfc];
    let sfc_v = prof.v[prof.sfc];
    if sfc_u.is_finite() && sfc_v.is_finite() {
        let h_1km_msl = sfc_h + 1000.0;
        let h_3km_msl = sfc_h + 3000.0;
        let h_6km_msl = sfc_h + 6000.0;

        let p_1km = prof.pres_at_height(h_1km_msl);
        let p_3km = prof.pres_at_height(h_3km_msl);
        let p_6km = prof.pres_at_height(h_6km_msl);

        let (u1, v1) = prof.interp_wind(p_1km);
        let (u3, v3) = prof.interp_wind(p_3km);
        let (u6, v6) = prof.interp_wind(p_6km);

        if u1.is_finite() && v1.is_finite() {
            params.shear_0_1 = ((u1 - sfc_u).powi(2) + (v1 - sfc_v).powi(2)).sqrt();
        }
        if u3.is_finite() && v3.is_finite() {
            params.shear_0_3 = ((u3 - sfc_u).powi(2) + (v3 - sfc_v).powi(2)).sqrt();
        }
        if u6.is_finite() && v6.is_finite() {
            params.shear_0_6 = ((u6 - sfc_u).powi(2) + (v6 - sfc_v).powi(2)).sqrt();
        }
    }

    // PWAT (precipitable water)
    let mut pwat = 0.0_f64;
    for i in 0..prof.num_levels() - 1 {
        let p0 = prof.pres[i];
        let p1 = prof.pres[i + 1];
        let w0 = prof.wvmr[i];
        let w1 = prof.wvmr[i + 1];
        if p0.is_finite() && p1.is_finite() && w0.is_finite() && w1.is_finite() {
            let dp = (p0 - p1).abs() * 100.0; // Pa
            let w_avg = (w0 + w1) / 2.0 / 1000.0; // kg/kg
            pwat += w_avg * dp / (G * 1000.0); // mm
        }
    }
    params.pwat = pwat;

    // K-index
    let t_850 = prof.interp_tmpc(850.0);
    let t_700 = prof.interp_tmpc(700.0);
    let t_500 = prof.interp_tmpc(500.0);
    let td_850 = prof.interp_dwpc(850.0);
    let td_700 = prof.interp_dwpc(700.0);
    if [t_850, t_700, t_500, td_850, td_700]
        .iter()
        .all(|v| v.is_finite())
    {
        params.k_index = (t_850 - t_500) + td_850 - (t_700 - td_700);
    }

    // Total totals
    if t_850.is_finite() && t_500.is_finite() && td_850.is_finite() {
        params.total_totals = (t_850 - t_500) + (td_850 - t_500);
    }

    // Bunkers storm motion (mean wind 0-6 km approximation)
    if sfc_u.is_finite() && sfc_v.is_finite() {
        let mut u_mean = 0.0;
        let mut v_mean = 0.0;
        let n_steps = 60;
        for i in 0..=n_steps {
            let h = sfc_h + (i as f64 / n_steps as f64) * 6000.0;
            let p = prof.pres_at_height(h);
            if p.is_finite() {
                let (u, v) = prof.interp_wind(p);
                if u.is_finite() && v.is_finite() {
                    u_mean += u;
                    v_mean += v;
                }
            }
        }
        u_mean /= (n_steps + 1) as f64;
        v_mean /= (n_steps + 1) as f64;
        params.mean_wind = (u_mean, v_mean);

        // Simple Bunkers: deviate 7.5 m/s perpendicular to shear
        let p_6km = prof.pres_at_height(sfc_h + 6000.0);
        let (u6, v6) = prof.interp_wind(p_6km);
        if u6.is_finite() && v6.is_finite() {
            let du = u6 - sfc_u;
            let dv = v6 - sfc_v;
            let shear_mag = (du * du + dv * dv).sqrt().max(1.0);
            let d = 7.5 * 1.94384; // 7.5 m/s to knots
            let nx = -dv / shear_mag;
            let ny = du / shear_mag;
            params.bunkers_rm = (u_mean + nx * d, v_mean + ny * d);
            params.bunkers_lm = (u_mean - nx * d, v_mean - ny * d);
        }
    }

    params
}

/// Build a `cape::Profile` from a sharprs `Profile`.
fn make_cape_profile(prof: &Profile) -> cape::Profile {
    let pres = prof.pres.clone();
    let hght = prof.hght.clone();
    let tmpc = prof.tmpc.clone();
    let dwpc = prof.dwpc.clone();
    cape::Profile::new(pres, hght, tmpc, dwpc, prof.sfc)
}

// =========================================================================
// Helper: draw text with a dark background box for readability
// =========================================================================

fn draw_text_with_bg(
    c: &mut Canvas,
    text: &str,
    x: i32,
    y: i32,
    fg: [u8; 4],
    bg: [u8; 4],
    pad: i32,
) {
    let tw = Canvas::text_width(text);
    c.fill_rect(x - pad, y - pad, tw + pad * 2, FONT_H + pad * 2, bg);
    c.draw_text(text, x, y, fg);
}

/// Draw text at 2x scale (each pixel of the 7x10 font becomes a 2x2 block).
fn draw_text_2x(c: &mut Canvas, text: &str, px: i32, py: i32, col: [u8; 4]) {
    c.draw_text_scaled(text, px, py, col, 2);
}

/// Width of 2x-scale text.
fn text_width_2x(text: &str) -> i32 {
    Canvas::text_width_scaled(text, 2)
}

/// Draw text with a dark background box at 2x scale.
fn draw_text_with_bg_2x(
    c: &mut Canvas,
    text: &str,
    x: i32,
    y: i32,
    fg: [u8; 4],
    bg: [u8; 4],
    pad: i32,
) {
    let tw = text_width_2x(text);
    let th = FONT_H * 2;
    c.fill_rect(x - pad, y - pad, tw + pad * 2, th + pad * 2, bg);
    draw_text_2x(c, text, x, y, fg);
}

// =========================================================================
// Public API
// =========================================================================

/// Render a complete Skew-T diagram to an RGBA pixel buffer.
///
/// Returns `Vec<u8>` of length `width * height * 4`.
pub fn render_skewt(prof: &Profile, width: u32, height: u32) -> Vec<u8> {
    render_skewt_impl(prof, width, height, true)
}

/// Render only the Skew-T plotting region and its in-plot diagnostics.
pub fn render_skewt_only(prof: &Profile, width: u32, height: u32) -> Vec<u8> {
    render_skewt_impl(prof, width, height, false)
}

fn render_skewt_impl(
    prof: &Profile,
    width: u32,
    height: u32,
    include_side_panels: bool,
) -> Vec<u8> {
    let mut c = Canvas::new(width, height, COL_BG);

    let cape_prof = make_cape_profile(prof);
    let params = compute_skewt_params(prof, &cape_prof);

    let skewt_w = if include_side_panels {
        (width as f64 * SKEWT_FRAC) as u32
    } else {
        width
    };
    let right_x = skewt_w as i32;
    let right_w = width - skewt_w;

    let plot_w = skewt_w as f64 - MARGIN_LEFT - MARGIN_RIGHT;
    let plot_h = height as f64 - MARGIN_TOP - MARGIN_BOT;

    // ── 1. Background grid ──────────────────────────────────────────
    draw_mixing_ratio_lines(&mut c, plot_w, plot_h);
    draw_moist_adiabats(&mut c, plot_w, plot_h);
    draw_dry_adiabats(&mut c, plot_w, plot_h);
    draw_isobars(&mut c, plot_w, plot_h, skewt_w);
    draw_isotherms(&mut c, plot_w, plot_h);

    // ── 2. CAPE/CIN shading (using ML parcel) ───────────────────────
    if let Some(ref ml_pcl) = params.ml_pcl {
        draw_cape_cin_fills(&mut c, &cape_prof, ml_pcl, plot_w, plot_h);
    }

    // ── 3. Temperature and dewpoint traces (THICK, prominent) ───────
    draw_profile_trace(&mut c, prof, false, COL_TEMP, 5, plot_w, plot_h);
    draw_profile_trace(&mut c, prof, true, COL_DEWP, 5, plot_w, plot_h);

    // ── 19. Wet-bulb trace (thin, cyan) ─────────────────────────────
    draw_wetbulb_trace(&mut c, prof, plot_w, plot_h);

    // ── 10. Parcel traces (ML = gold, MU = orange dashed) ───────────
    if let Some(ref ml_pcl) = params.ml_pcl {
        draw_parcel_trace(&mut c, ml_pcl, COL_PARCEL_ML, plot_w, plot_h);
    }
    if let Some(ref mu_pcl) = params.mu_pcl {
        draw_parcel_trace(&mut c, mu_pcl, COL_PARCEL_MU, plot_w, plot_h);
    }

    // ── 20. DCAPE downdraft trace (magenta dashed) ──────────────────
    if let Some(ref dcape_res) = params.dcape_result {
        draw_dcape_trace(&mut c, dcape_res, plot_w, plot_h);
    }

    // ── 16. Wind barbs (cyan) ───────────────────────────────────────
    draw_wind_barbs(&mut c, prof, plot_w, plot_h, skewt_w);

    // ── 18. Omega profile on far left ───────────────────────────────
    draw_omega_profile(&mut c, prof, plot_h);

    // ── 12. Height markers (cyan, left side) ────────────────────────
    draw_height_markers(&mut c, prof, plot_w, plot_h);

    // ── 13. Effective inflow layer bracket ──────────────────────────
    draw_effective_inflow_bracket(&mut c, &params, plot_w, plot_h);

    // ── 3/5. Pressure and temperature labels ────────────────────────
    draw_pressure_labels(&mut c, plot_w, plot_h);
    draw_temp_labels(&mut c, plot_w, plot_h);

    // ── 17. Surface temperature labels in Fahrenheit ────────────────
    draw_sfc_temp_label(&mut c, prof, plot_w, plot_h);

    // ── 11. LCL, LFC, EL labels ────────────────────────────────────
    draw_level_labels(&mut c, &params, &cape_prof, plot_w, plot_h);

    // ── Title ───────────────────────────────────────────────────────
    // ── Right panel: hodograph ──────────────────────────────────────
    if include_side_panels && right_w > 0 {
        let hodo_h = (height as i32) / 2;
        draw_hodograph(&mut c, prof, &params, right_x, 0, right_w as i32, hodo_h);

        // ── Right panel: text indices ───────────────────────────────────
        draw_text_panel(
            &mut c,
            &params,
            right_x,
            hodo_h,
            right_w as i32,
            height as i32 - hodo_h,
        );
    }

    c.pixels
}

/// Render to PNG bytes.
#[cfg(feature = "render-png")]
pub fn render_skewt_png(prof: &Profile, width: u32, height: u32) -> Vec<u8> {
    let rgba = render_skewt(prof, width, height);
    let mut buf = Vec::new();
    {
        let mut encoder = image::codecs::png::PngEncoder::new(&mut buf);
        use image::ImageEncoder;
        encoder
            .write_image(&rgba, width, height, image::ExtendedColorType::Rgba8)
            .expect("PNG encoding failed");
    }
    buf
}

// =========================================================================
// Drawing routines
// =========================================================================

fn draw_isobars(c: &mut Canvas, plot_w: f64, plot_h: f64, skewt_w: u32) {
    for &p in STD_PRESSURES {
        let (_, y) = tp_to_screen(0.0, p, plot_w, plot_h);
        let yi = y as i32;
        c.draw_line(
            MARGIN_LEFT as i32,
            yi,
            (skewt_w as f64 - MARGIN_RIGHT) as i32,
            yi,
            COL_ISOBAR,
        );
    }
}

fn draw_pressure_labels(c: &mut Canvas, plot_w: f64, plot_h: f64) {
    // SHARPpy keeps pressure labels compact so they do not compete with traces.
    for &p in LABELED_PRESSURES {
        let (_, y) = tp_to_screen(0.0, p, plot_w, plot_h);
        let label = format!("{}", p as i32);
        let tw = Canvas::text_width(&label);
        let lx = MARGIN_LEFT as i32 - tw - 4;
        c.draw_text(&label, lx.max(2), y as i32 - FONT_H / 2, COL_LABEL_LARGE);
    }
    // Draw smaller labels for the remaining standard levels
    for &p in STD_PRESSURES {
        if LABELED_PRESSURES.contains(&p) {
            continue;
        }
        let (_, y) = tp_to_screen(0.0, p, plot_w, plot_h);
        let label = format!("{}", p as i32);
        let lx = MARGIN_LEFT as i32 - Canvas::text_width(&label) - 3;
        c.draw_text(&label, lx.max(2), y as i32 - FONT_H / 2, COL_LABEL);
    }
}

fn draw_isotherms(c: &mut Canvas, plot_w: f64, plot_h: f64) {
    for t in (-80..=60).step_by(10) {
        let is_zero = t == 0;
        let col = if is_zero { COL_GRID_ZERO } else { COL_GRID };
        let (x0, y0) = tp_to_screen(t as f64, P_BOT, plot_w, plot_h);
        let (x1, y1) = tp_to_screen(t as f64, P_TOP, plot_w, plot_h);
        if is_zero {
            // Prominent 0C isotherm — thick and bright
            c.draw_thick_line_aa(x0, y0, x1, y1, col, 3);
        } else {
            c.draw_line(x0 as i32, y0 as i32, x1 as i32, y1 as i32, col);
        }
    }
}

fn draw_temp_labels(c: &mut Canvas, plot_w: f64, plot_h: f64) {
    let y_bot = (MARGIN_TOP + plot_h) as i32 + 4;
    // Label every 10 degrees from -30 to 40
    for t in (-30..=40).step_by(10) {
        let (x, _) = tp_to_screen(t as f64, P_BOT, plot_w, plot_h);
        let label = format!("{}", t);
        let tw = Canvas::text_width(&label);
        c.draw_text(&label, x as i32 - tw / 2, y_bot, COL_LABEL_LARGE);
    }
}

fn draw_dry_adiabats(c: &mut Canvas, plot_w: f64, plot_h: f64) {
    for start_t in (-40..=80).step_by(20) {
        let th = (start_t as f64) + ZEROCNK;
        let mut prev: Option<(i32, i32)> = None;
        let mut p = P_BOT;
        while p >= P_TOP {
            let t = temp_from_theta(th, p);
            let (sx, sy) = tp_to_screen(t, p, plot_w, plot_h);
            if let Some((px, py)) = prev {
                c.draw_line(px, py, sx as i32, sy as i32, COL_DRY_AD);
            }
            prev = Some((sx as i32, sy as i32));
            p -= 10.0;
        }
    }
}

fn draw_moist_adiabats(c: &mut Canvas, plot_w: f64, plot_h: f64) {
    for start_t in (-28..=36).step_by(4) {
        let mut t = start_t as f64;
        let mut p = 1050.0;
        let dp = -10.0;
        let mut prev: Option<(i32, i32)> = None;
        while p > P_TOP {
            if !t.is_finite() {
                break;
            }
            let (sx, sy) = tp_to_screen(t, p, plot_w, plot_h);
            if let Some((px, py)) = prev {
                c.draw_line(px, py, sx as i32, sy as i32, COL_MOIST_AD);
            }
            prev = Some((sx as i32, sy as i32));
            let t_k = t + ZEROCNK;
            let new_p = p + dp;
            if new_p < P_TOP {
                break;
            }
            let dz = -(RD * t_k / G) * (dp / p);
            let gamma_m = moist_lapse_rate(t, p);
            t -= gamma_m * dz;
            p = new_p;
        }
    }
}

fn draw_mixing_ratio_lines(c: &mut Canvas, plot_w: f64, plot_h: f64) {
    let ratios = [1.0, 2.0, 4.0, 7.0, 10.0, 16.0, 24.0];
    for &w in &ratios {
        let w_kg = w / 1000.0;
        let mut prev: Option<(f64, f64)> = None;
        let mut p = P_BOT;
        while p >= 400.0 {
            let td = dewpoint_from_mixing_ratio(w_kg, p);
            let (sx, sy) = tp_to_screen(td, p, plot_w, plot_h);
            if let Some((px, py)) = prev {
                c.draw_dashed_line(px, py, sx, sy, COL_MIX_RATIO, 4.0, 4.0);
            }
            prev = Some((sx, sy));
            p -= 20.0;
        }
    }
}

// ── Profile traces ──────────────────────────────────────────────────────

fn draw_profile_trace(
    c: &mut Canvas,
    prof: &Profile,
    dewpoint: bool,
    col: [u8; 4],
    thickness: i32,
    plot_w: f64,
    plot_h: f64,
) {
    let field = if dewpoint { &prof.dwpc } else { &prof.tmpc };
    for i in 1..prof.num_levels() {
        let t0 = field[i - 1];
        let t1 = field[i];
        let p0 = prof.pres[i - 1];
        let p1 = prof.pres[i];
        if !t0.is_finite() || !t1.is_finite() || !p0.is_finite() || !p1.is_finite() {
            continue;
        }
        if p0 < P_TOP || p1 > P_BOT {
            continue;
        }
        let (x0, y0) = tp_to_screen(t0, p0, plot_w, plot_h);
        let (x1, y1) = tp_to_screen(t1, p1, plot_w, plot_h);
        c.draw_thick_line_aa(x0, y0, x1, y1, col, thickness);
    }
}

fn draw_wetbulb_trace(c: &mut Canvas, prof: &Profile, plot_w: f64, plot_h: f64) {
    for i in 1..prof.num_levels() {
        let t0 = prof.wetbulb[i - 1];
        let t1 = prof.wetbulb[i];
        let p0 = prof.pres[i - 1];
        let p1 = prof.pres[i];
        if !t0.is_finite() || !t1.is_finite() || !p0.is_finite() || !p1.is_finite() {
            continue;
        }
        if p0 < P_TOP || p1 > P_BOT {
            continue;
        }
        let (x0, y0) = tp_to_screen(t0, p0, plot_w, plot_h);
        let (x1, y1) = tp_to_screen(t1, p1, plot_w, plot_h);
        c.draw_line_aa(x0, y0, x1, y1, COL_WETBULB);
    }
}

fn draw_parcel_trace(c: &mut Canvas, pcl: &ParcelResult, col: [u8; 4], plot_w: f64, plot_h: f64) {
    if pcl.ptrace.len() < 2 || pcl.ttrace.len() < 2 {
        return;
    }
    let n = pcl.ptrace.len().min(pcl.ttrace.len());
    for i in 1..n {
        let p0 = pcl.ptrace[i - 1];
        let p1 = pcl.ptrace[i];
        // ttrace contains virtual temperature; convert back to approximate T
        // for Skew-T plotting (close enough for display).
        let t0 = pcl.ttrace[i - 1];
        let t1 = pcl.ttrace[i];
        if !p0.is_finite() || !p1.is_finite() || !t0.is_finite() || !t1.is_finite() {
            continue;
        }
        if p0 < P_TOP || p1 > P_BOT {
            continue;
        }
        let (x0, y0) = tp_to_screen(t0, p0, plot_w, plot_h);
        let (x1, y1) = tp_to_screen(t1, p1, plot_w, plot_h);
        c.draw_thick_dashed_line(x0, y0, x1, y1, col, 2, 8.0, 5.0);
    }
}

fn draw_dcape_trace(c: &mut Canvas, dcape: &DcapeResult, plot_w: f64, plot_h: f64) {
    if dcape.ptrace.len() < 2 || dcape.ttrace.len() < 2 {
        return;
    }
    let n = dcape.ptrace.len().min(dcape.ttrace.len());
    for i in 1..n {
        let p0 = dcape.ptrace[i - 1];
        let p1 = dcape.ptrace[i];
        let t0 = dcape.ttrace[i - 1];
        let t1 = dcape.ttrace[i];
        if !p0.is_finite() || !p1.is_finite() || !t0.is_finite() || !t1.is_finite() {
            continue;
        }
        if p0 < P_TOP || p1 > P_BOT {
            continue;
        }
        let (x0, y0) = tp_to_screen(t0, p0, plot_w, plot_h);
        let (x1, y1) = tp_to_screen(t1, p1, plot_w, plot_h);
        c.draw_thick_dashed_line(x0, y0, x1, y1, COL_DCAPE, 2, 6.0, 4.0);
    }
}

// ── CAPE/CIN fills ──────────────────────────────────────────────────────

fn draw_cape_cin_fills(
    c: &mut Canvas,
    cape_prof: &cape::Profile,
    pcl: &ParcelResult,
    plot_w: f64,
    plot_h: f64,
) {
    if pcl.ptrace.len() < 2 || pcl.ttrace.len() < 2 {
        return;
    }
    let n = pcl.ptrace.len().min(pcl.ttrace.len());
    // Draw multiple scanlines per pressure level for denser fill
    for i in 0..n {
        let pp = pcl.ptrace[i];
        let pt = pcl.ttrace[i]; // parcel virtual temp
        if !pp.is_finite() || !pt.is_finite() || pp < P_TOP || pp > P_BOT {
            continue;
        }
        // Get environment virtual temperature at this pressure
        let env_vt = cape::interp_vtmp_pub(cape_prof, pp);
        if !env_vt.is_finite() {
            continue;
        }
        let (parcel_sx, sy) = tp_to_screen(pt, pp, plot_w, plot_h);
        let (env_sx, _) = tp_to_screen(env_vt, pp, plot_w, plot_h);
        let yi = sy as i32;
        if pt > env_vt {
            // CAPE — fill multiple rows for density
            for dy in -1..=1 {
                c.fill_span(yi + dy, env_sx as i32, parcel_sx as i32, COL_CAPE_FILL);
            }
        } else {
            // CIN — fill multiple rows for density
            for dy in -1..=1 {
                c.fill_span(yi + dy, parcel_sx as i32, env_sx as i32, COL_CIN_FILL);
            }
        }
    }
}

// ── Wind barbs ──────────────────────────────────────────────────────────

fn draw_wind_barbs(c: &mut Canvas, prof: &Profile, plot_w: f64, plot_h: f64, skewt_w: u32) {
    let bx = (skewt_w as f64 - MARGIN_RIGHT / 2.0) as f64;
    let barb_pressures = [
        1000.0, 925.0, 850.0, 800.0, 750.0, 700.0, 650.0, 600.0, 550.0, 500.0, 450.0, 400.0, 350.0,
        300.0, 250.0, 200.0, 150.0,
    ];

    for &p in &barb_pressures {
        if p < P_TOP || p > P_BOT {
            continue;
        }
        let (wdir, wspd) = prof.interp_vec(p);
        if !wdir.is_finite() || !wspd.is_finite() || wspd < 0.5 {
            continue;
        }
        let (_, sy) = tp_to_screen(0.0, p, plot_w, plot_h);
        c.draw_wind_barb_met(bx, sy, wdir, wspd, COL_WIND_BARB, 23.0, 1);
    }
}

// ── Omega profile ───────────────────────────────────────────────────────

fn draw_omega_profile(c: &mut Canvas, prof: &Profile, plot_h: f64) {
    // Draw omega (Pa/s) as a line on the far left (x = 5..50)
    let omega_x_center = 25.0;
    let omega_x_scale = 20.0; // pixels per 1 Pa/s
    let x_min = 5;
    let x_max = 48;

    // Zero line
    let y_top = MARGIN_TOP as i32;
    let y_bot = (MARGIN_TOP + plot_h) as i32;
    c.draw_line(
        omega_x_center as i32,
        y_top,
        omega_x_center as i32,
        y_bot,
        [35, 35, 45, 200],
    );

    for i in 1..prof.num_levels() {
        let o0 = prof.omeg[i - 1];
        let o1 = prof.omeg[i];
        let p0 = prof.pres[i - 1];
        let p1 = prof.pres[i];
        if !o0.is_finite() || !o1.is_finite() || !p0.is_finite() || !p1.is_finite() {
            continue;
        }
        if p0 < P_TOP || p1 > P_BOT {
            continue;
        }
        // Omega in Pa/s; typical range -2 to +2 Pa/s
        let sx0 = (omega_x_center + o0 * omega_x_scale).clamp(x_min as f64, x_max as f64);
        let sx1 = (omega_x_center + o1 * omega_x_scale).clamp(x_min as f64, x_max as f64);
        let yn0 = y_from_p(p0);
        let yn1 = y_from_p(p1);
        let sy0 = MARGIN_TOP + (1.0 - yn0) * plot_h;
        let sy1 = MARGIN_TOP + (1.0 - yn1) * plot_h;
        c.draw_line_aa(sx0, sy0, sx1, sy1, COL_OMEGA);
    }
}

// ── Height markers (cyan, left side) ────────────────────────────────────

fn draw_height_markers(c: &mut Canvas, prof: &Profile, plot_w: f64, plot_h: f64) {
    let sfc_h = prof.hght[prof.sfc];
    let heights_km = [0.0, 1.0, 3.0, 6.0, 9.0, 12.0, 15.0];

    for &hkm in &heights_km {
        let h_msl = sfc_h + hkm * 1000.0;
        let p = prof.pres_at_height(h_msl);
        if !p.is_finite() || p < P_TOP || p > P_BOT {
            continue;
        }
        let (_, sy) = tp_to_screen(0.0, p, plot_w, plot_h);
        let yi = sy as i32;
        let label = format!("{}KM", hkm as i32);
        // Draw at 2x scale for readability
        let tw = text_width_2x(&label);
        let lx = MARGIN_LEFT as i32 - tw - 4;
        draw_text_2x(c, &label, lx.max(2), yi - FONT_H, COL_HEIGHT_MARK);
        // Horizontal tick mark (wider and thicker)
        c.draw_line(
            MARGIN_LEFT as i32 - 8,
            yi,
            MARGIN_LEFT as i32,
            yi,
            COL_HEIGHT_MARK,
        );
        c.draw_line(
            MARGIN_LEFT as i32 - 8,
            yi - 1,
            MARGIN_LEFT as i32,
            yi - 1,
            COL_HEIGHT_MARK,
        );
    }
}

// ── Effective inflow layer bracket ──────────────────────────────────────

fn draw_effective_inflow_bracket(c: &mut Canvas, params: &SkewTParams, plot_w: f64, plot_h: f64) {
    let pbot = params.eff_inflow_bot;
    let ptop = params.eff_inflow_top;
    if !pbot.is_finite() || !ptop.is_finite() {
        return;
    }

    let (_, y_bot) = tp_to_screen(0.0, pbot, plot_w, plot_h);
    let (_, y_top) = tp_to_screen(0.0, ptop, plot_w, plot_h);

    let bx = MARGIN_LEFT as i32 + 6;

    // Thick vertical bar (4 pixels wide)
    for dx in 0..4 {
        c.draw_line(bx + dx, y_top as i32, bx + dx, y_bot as i32, COL_EFF_INFLOW);
    }

    // Thick horizontal ticks at top and bottom
    for dy in -1..=1 {
        c.draw_line(
            bx - 5,
            y_bot as i32 + dy,
            bx + 6,
            y_bot as i32 + dy,
            COL_EFF_INFLOW,
        );
        c.draw_line(
            bx - 5,
            y_top as i32 + dy,
            bx + 6,
            y_top as i32 + dy,
            COL_EFF_INFLOW,
        );
    }

    // Label with background box
    let label = "EFF";
    let mid_y = (y_top as i32 + y_bot as i32) / 2;
    draw_text_with_bg(
        c,
        label,
        bx + 8,
        mid_y - FONT_H / 2,
        COL_EFF_INFLOW,
        COL_LABEL_BG,
        2,
    );
}

// ── Surface temperature labels in Fahrenheit ─────────────────────────

fn draw_sfc_temp_label(c: &mut Canvas, prof: &Profile, plot_w: f64, plot_h: f64) {
    let t_sfc = prof.tmpc[prof.sfc];
    let td_sfc = prof.dwpc[prof.sfc];
    if !t_sfc.is_finite() {
        return;
    }

    let y_bot = (MARGIN_TOP + plot_h) as i32 + 4;

    // Temperature in F (drawn at 2x for visibility)
    let t_f = ctof(t_sfc);
    let label_t = format!("{:.0}F", t_f);
    let (xt, _) = tp_to_screen(t_sfc, prof.pres[prof.sfc], plot_w, plot_h);
    let tw_t = text_width_2x(&label_t);
    draw_text_2x(c, &label_t, xt as i32 - tw_t / 2, y_bot, COL_TEMP);

    // Dewpoint in F
    if td_sfc.is_finite() {
        let td_f = ctof(td_sfc);
        let label_td = format!("{:.0}F", td_f);
        let (xtd, _) = tp_to_screen(td_sfc, prof.pres[prof.sfc], plot_w, plot_h);
        let tw_td = text_width_2x(&label_td);
        draw_text_2x(c, &label_td, xtd as i32 - tw_td / 2, y_bot, COL_DEWP);
    }

    // Wet-bulb temperature in F
    let wb_sfc = prof.wetbulb[prof.sfc];
    if wb_sfc.is_finite() {
        let wb_f = ctof(wb_sfc);
        let label_wb = format!("{:.0}F", wb_f);
        let (xwb, _) = tp_to_screen(wb_sfc, prof.pres[prof.sfc], plot_w, plot_h);
        let tw_wb = text_width_2x(&label_wb);
        draw_text_2x(
            c,
            &label_wb,
            xwb as i32 - tw_wb / 2,
            y_bot + FONT_H * 2 + 2,
            COL_WETBULB,
        );
    }
}

// ── LCL, LFC, EL labels ────────────────────────────────────────────────

fn draw_level_labels(
    c: &mut Canvas,
    params: &SkewTParams,
    _cape_prof: &cape::Profile,
    plot_w: f64,
    plot_h: f64,
) {
    // Use the SB parcel for level pressures
    let sb_pcl = match &params.sb_pcl {
        Some(p) => p,
        None => return,
    };

    let plot_right = MARGIN_LEFT + plot_w;
    let label_x = plot_right as i32 - 66;
    let dash_x0 = plot_right - 78.0;
    let dash_x1 = plot_right - 12.0;

    // LCL — with dark background box, 2x scale
    if sb_pcl.lclpres.is_finite() && sb_pcl.lclpres > P_TOP && sb_pcl.lclpres < P_BOT {
        let (_, y) = tp_to_screen(0.0, sb_pcl.lclpres, plot_w, plot_h);
        draw_text_with_bg_2x(
            c,
            "LCL",
            label_x,
            y as i32 - FONT_H,
            COL_LCL_LABEL,
            COL_LABEL_BG,
            3,
        );
        // Horizontal dash at the level
        c.draw_thick_line_aa(dash_x0, y, dash_x1, y, COL_LCL_LABEL, 2);
    }

    // LFC — with dark background box, 2x scale
    if sb_pcl.lfcpres.is_finite() && sb_pcl.lfcpres > P_TOP && sb_pcl.lfcpres < P_BOT {
        let (_, y) = tp_to_screen(0.0, sb_pcl.lfcpres, plot_w, plot_h);
        draw_text_with_bg_2x(
            c,
            "LFC",
            label_x,
            y as i32 - FONT_H,
            COL_LFC_LABEL,
            COL_LABEL_BG,
            3,
        );
        c.draw_thick_line_aa(dash_x0, y, dash_x1, y, COL_LFC_LABEL, 2);
    }

    // EL — with dark background box, 2x scale
    if sb_pcl.elpres.is_finite() && sb_pcl.elpres > P_TOP && sb_pcl.elpres < P_BOT {
        let (_, y) = tp_to_screen(0.0, sb_pcl.elpres, plot_w, plot_h);
        draw_text_with_bg_2x(
            c,
            "EL",
            label_x,
            y as i32 - FONT_H,
            COL_EL_LABEL,
            COL_LABEL_BG,
            3,
        );
        c.draw_thick_line_aa(dash_x0, y, dash_x1, y, COL_EL_LABEL, 2);
    }
}

// ── Hodograph ───────────────────────────────────────────────────────────

fn draw_hodograph(
    c: &mut Canvas,
    prof: &Profile,
    params: &SkewTParams,
    rx: i32,
    ry: i32,
    rw: i32,
    rh: i32,
) {
    c.fill_rect(rx, ry, rw, rh, COL_PANEL_BG);
    c.draw_rect(rx, ry, rw, rh, COL_PANEL_BORDER);

    let cx = rx + rw / 2;
    let cy = ry + rh / 2 + 8;
    let max_radius = (rw.min(rh) / 2 - 20).max(20);
    let scale = max_radius as f64 / 60.0;

    c.draw_text("HODOGRAPH", rx + 4, ry + 3, COL_TEXT_HEADER);

    // Concentric rings
    for &kt in &[20, 40, 60] {
        let r = (kt as f64 * scale) as i32;
        c.draw_circle(cx, cy, r, COL_HODO_RING);
        let label = format!("{}", kt);
        c.draw_text(&label, cx + r + 2, cy - FONT_H / 2, COL_TEXT_DIM);
    }

    // Cross-hairs
    let r60 = (60.0 * scale) as i32;
    c.draw_line(cx - r60, cy, cx + r60, cy, COL_HODO_RING);
    c.draw_line(cx, cy - r60, cx, cy + r60, COL_HODO_RING);

    if prof.num_levels() < 2 {
        return;
    }

    let sfc_h = prof.hght[prof.sfc];

    let uv_to_screen =
        |u: f64, v: f64| -> (i32, i32) { (cx + (u * scale) as i32, cy - (v * scale) as i32) };

    let height_color = |h_agl: f64| -> [u8; 4] {
        if h_agl < 3000.0 {
            COL_HODO_0_3
        } else if h_agl < 6000.0 {
            COL_HODO_3_6
        } else if h_agl < 9000.0 {
            COL_HODO_6_9
        } else {
            COL_HODO_9_12
        }
    };

    let max_h = sfc_h + 12000.0;
    let dh = 100.0;
    let mut h = sfc_h;
    let mut prev: Option<(i32, i32, f64)> = None;

    while h <= max_h {
        let p = prof.pres_at_height(h);
        if !p.is_finite() {
            h += dh;
            continue;
        }
        let (u, v) = prof.interp_wind(p);
        if !u.is_finite() || !v.is_finite() {
            h += dh;
            continue;
        }
        let (sx, sy) = uv_to_screen(u, v);
        let h_agl = h - sfc_h;
        let col = height_color(h_agl);

        if let Some((px, py, _)) = prev {
            if sx >= rx && sx < rx + rw && sy >= ry && sy < ry + rh {
                c.draw_line_aa(px as f64, py as f64, sx as f64, sy as f64, col);
            }
        }
        prev = Some((sx, sy, h_agl));
        h += dh;
    }

    // Bunkers RM marker (+)
    {
        let (u, v) = params.bunkers_rm;
        if u.is_finite() && v.is_finite() {
            let (sx, sy) = uv_to_screen(u, v);
            c.draw_line(sx - 4, sy, sx + 4, sy, COL_HODO_BUNKERS);
            c.draw_line(sx, sy - 4, sx, sy + 4, COL_HODO_BUNKERS);
        }
    }

    // Mean wind marker (o)
    {
        let (u, v) = params.mean_wind;
        if u.is_finite() && v.is_finite() {
            let (sx, sy) = uv_to_screen(u, v);
            c.draw_circle(sx, sy, 4, COL_HODO_MEAN);
        }
    }

    // Height legend
    let legend_items: &[(&str, [u8; 4])] = &[
        ("0-3KM", COL_HODO_0_3),
        ("3-6KM", COL_HODO_3_6),
        ("6-9KM", COL_HODO_6_9),
        ("9-12KM", COL_HODO_9_12),
    ];
    let lx = rx + 4;
    let mut ly = ry + rh - (legend_items.len() as i32) * (FONT_H + 2) - 4;
    for &(label, col) in legend_items {
        c.fill_rect(lx, ly + 2, 10, FONT_H - 4, col);
        c.draw_text(label, lx + 14, ly, COL_TEXT_DIM);
        ly += FONT_H + 2;
    }
}

// ── Text parameter panel ────────────────────────────────────────────────

fn draw_text_panel(c: &mut Canvas, params: &SkewTParams, rx: i32, ry: i32, rw: i32, rh: i32) {
    c.fill_rect(rx, ry, rw, rh, COL_PANEL_BG);
    c.draw_rect(rx, ry, rw, rh, COL_PANEL_BORDER);

    let lx = rx + 6;
    let vx = rx + rw - 8;
    let mut y = ry + 6;
    let line_h = FONT_H + 3;

    let section = |c: &mut Canvas, y: &mut i32, title: &str| {
        c.draw_text(title, lx, *y, COL_TEXT_HEADER);
        *y += line_h;
    };

    let row = |c: &mut Canvas, y: &mut i32, label: &str, value: &str| {
        c.draw_text(label, lx, *y, COL_TEXT_DIM);
        c.draw_text_right(value, vx, *y, COL_TEXT);
        *y += line_h;
    };

    // CAPE / CIN
    section(c, &mut y, "CAPE / CIN");
    row(c, &mut y, "SB CAPE", &format!("{:.0} J/KG", params.sb_cape));
    row(c, &mut y, "ML CAPE", &format!("{:.0} J/KG", params.ml_cape));
    row(c, &mut y, "MU CAPE", &format!("{:.0} J/KG", params.mu_cape));
    row(c, &mut y, "SB CIN", &format!("{:.0} J/KG", params.sb_cin));
    row(c, &mut y, "ML CIN", &format!("{:.0} J/KG", params.ml_cin));
    row(c, &mut y, "DCAPE", &format!("{:.0} J/KG", params.dcape));
    y += 2;

    // Heights
    section(c, &mut y, "HEIGHTS");
    row(c, &mut y, "LCL", &format!("{:.0} M", params.lcl_hgt));
    row(c, &mut y, "LFC", &format!("{:.0} M", params.lfc_hgt));
    row(c, &mut y, "EL", &format!("{:.0} M", params.el_hgt));
    y += 2;

    // Lapse rates
    section(c, &mut y, "LAPSE RATES");
    row(c, &mut y, "0-3 KM", &format!("{:.1} C/KM", params.lr_0_3));
    row(c, &mut y, "3-6 KM", &format!("{:.1} C/KM", params.lr_3_6));
    row(
        c,
        &mut y,
        "700-500",
        &format!("{:.1} C/KM", params.lr_700_500),
    );
    y += 2;

    // Shear
    section(c, &mut y, "BULK SHEAR");
    row(c, &mut y, "0-1 KM", &format!("{:.0} KT", params.shear_0_1));
    row(c, &mut y, "0-3 KM", &format!("{:.0} KT", params.shear_0_3));
    row(c, &mut y, "0-6 KM", &format!("{:.0} KT", params.shear_0_6));
    y += 2;

    // Other
    section(c, &mut y, "OTHER");
    row(c, &mut y, "PWAT", &format!("{:.1} MM", params.pwat));
    row(c, &mut y, "K-INDEX", &format!("{:.0}", params.k_index));
    row(c, &mut y, "TT", &format!("{:.0}", params.total_totals));
    y += 2;

    // Storm motion
    section(c, &mut y, "STORM MOTION");
    let (rm_dir, rm_spd) = comp2vec(-params.bunkers_rm.0, -params.bunkers_rm.1);
    row(
        c,
        &mut y,
        "BNK RM",
        &format!("{:.0}/{:.0} KT", rm_dir, rm_spd),
    );
    let (lm_dir, lm_spd) = comp2vec(-params.bunkers_lm.0, -params.bunkers_lm.1);
    row(
        c,
        &mut y,
        "BNK LM",
        &format!("{:.0}/{:.0} KT", lm_dir, lm_spd),
    );
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::StationInfo;

    fn test_profile() -> Profile {
        let pres = [
            1000.0, 975.0, 950.0, 925.0, 900.0, 850.0, 800.0, 750.0, 700.0, 650.0, 600.0, 550.0,
            500.0, 450.0, 400.0, 350.0, 300.0, 250.0, 200.0, 150.0,
        ];
        let hght = [
            110.0, 330.0, 554.0, 782.0, 1014.0, 1494.0, 1999.0, 2533.0, 3103.0, 3717.0, 4387.0,
            5127.0, 5960.0, 6915.0, 8032.0, 9374.0, 11024.0, 13105.0, 15834.0, 19620.0,
        ];
        let tmpc = [
            30.0, 27.0, 24.0, 21.0, 18.0, 12.0, 6.5, 1.5, -3.5, -9.0, -15.0, -22.0, -30.0, -38.5,
            -48.0, -58.0, -69.0, -68.0, -64.0, -58.0,
        ];
        let dwpc = [
            22.0, 21.0, 20.0, 18.0, 16.0, 10.0, 2.0, -6.0, -14.0, -22.0, -30.0, -38.0, -46.0,
            -54.0, -62.0, -70.0, -78.0, -76.0, -72.0, -66.0,
        ];
        let wdir = [
            180.0, 185.0, 190.0, 200.0, 210.0, 220.0, 230.0, 240.0, 250.0, 260.0, 265.0, 270.0,
            270.0, 270.0, 275.0, 280.0, 285.0, 290.0, 280.0, 270.0,
        ];
        let wspd = [
            10.0, 12.0, 15.0, 18.0, 20.0, 25.0, 30.0, 35.0, 40.0, 45.0, 50.0, 55.0, 60.0, 55.0,
            50.0, 45.0, 40.0, 35.0, 30.0, 25.0,
        ];
        let omeg = [
            0.0, -0.1, -0.2, -0.3, -0.5, -0.8, -1.0, -0.8, -0.5, -0.3, -0.1, 0.0, 0.1, 0.2, 0.3,
            0.2, 0.1, 0.0, 0.0, 0.0,
        ];

        Profile::new(
            &pres,
            &hght,
            &tmpc,
            &dwpc,
            &wdir,
            &wspd,
            &omeg,
            StationInfo {
                station_id: "TEST".into(),
                datetime: "2026032000".into(),
                ..Default::default()
            },
        )
        .expect("test profile should be valid")
    }

    #[test]
    fn render_produces_pixels() {
        let prof = test_profile();
        let pixels = render_skewt(&prof, 1200, 800);
        assert_eq!(pixels.len(), 1200 * 800 * 4);

        // Check that something was drawn (not all background)
        let non_bg = pixels
            .chunks(4)
            .any(|p| p[0] != COL_BG[0] || p[1] != COL_BG[1] || p[2] != COL_BG[2]);
        assert!(non_bg, "Expected non-background pixels");
    }

    #[test]
    fn coordinate_transform_roundtrip() {
        let plot_w = 600.0;
        let plot_h = 500.0;
        let (x, y) = tp_to_screen(0.0, 500.0, plot_w, plot_h);
        assert!(x.is_finite());
        assert!(y.is_finite());
        assert!(y > MARGIN_TOP);
        assert!(y < MARGIN_TOP + plot_h);
    }

    #[test]
    fn params_computed() {
        let prof = test_profile();
        let cape_prof = make_cape_profile(&prof);
        let params = compute_skewt_params(&prof, &cape_prof);
        // With this warm, moist test sounding we should get positive CAPE
        assert!(params.sb_cape > 0.0, "SB CAPE should be positive");
        assert!(params.lr_0_3 > 0.0, "0-3 km lapse rate should be positive");
    }
}
