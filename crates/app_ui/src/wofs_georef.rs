//! WoFS map-drape georeference: recover the product PNGs' lat/lon mapping
//! from the sounding-PNG titles, then drape the imagery onto the radar map.
//!
//! The CB-WoFS endpoints expose no projection metadata anonymously, but the
//! sounding PNGs' title line prints the station's true coordinates
//! ("WoFS Sounding {lat}N, {lon}W"). The sounding stations form a fixed
//! lattice in DOMAIN FRACTIONS (station RR_CC, 01..20):
//!   fx = 0.031 + 0.049*(col-1),  fy = 0.031 + 0.049*(row-1)   (fy from bottom)
//! so OCR-ing ~20 station titles spread across the grid gives ground-control
//! points for a bilinear fit (fx, fy) -> (lat, lon). Validated live
//! 2026-06-11: fit residuals 0.05 deg (lat) / 0.003 deg (lon).
//!
//! Method notes (each step matters; bugs here poisoned early fits):
//! - The title strip is image rows 10..32 binarized at luma < 128, split by
//!   column-run segmentation; each segment is matched against 13 glyph
//!   templates harvested from the actual matplotlib rendering.
//! - Glyph matching must be FUZZY: subpixel text positioning wobbles each
//!   instance by a few bits, so the template slides +-2 px on both axes over
//!   the padded segment (classical binary template matching; cf. the
//!   matched-filter view in Brunelli, "Template Matching Techniques in
//!   Computer Vision", Wiley 2009, doi:10.1002/9780470744055). Score =
//!   mismatched bits / template area; accept best <= 0.09 only with a >= 0.03
//!   margin over the runner-up — a damaged '9' must NOT silently become a
//!   '0'; better to drop the character (hundreds of stations to spare).
//! - Numbers must have EXACTLY two decimals (WoFS titles print %.2f). A
//!   truncated read like "44." must REJECT, not parse as 44.0 — that exact
//!   failure poisoned early calibration fits.
//! - Lat and lon constraints are collected INDEPENDENTLY per station: a
//!   station with one clean number still contributes a half-constraint.
//! - The bilinear fit value = c0 + c1*fx + c2*fy + c3*fx*fy is solved by
//!   least squares with a RANSAC-style outlier trim (consensus inliers at
//!   residual < 0.1 deg, refit, require >= 6 inliers per axis; robust-fit
//!   paradigm after Fischler & Bolles 1981, CACM 24(6),
//!   doi:10.1145/358669.358692).
//!
//! BASIS ASSUMPTION (documented + runtime-checked): the station-fraction
//! unit square maps onto the AXES BOX of the 900x800 product PNG, pixels
//! (12,43)-(759,790), fy from bottom. Evidence: the axes box is square
//! (747x747) and the fitted domain is square (~885 km) — matplotlib imshow
//! renders a square data domain as a square axes box; and a live border
//! trace (IL-WI 42.4947N, IL-IN -87.526W polylines projected under both the
//! full-image and axes-box hypotheses) landed on the drawn state lines only
//! under the axes-box basis. `sanity_check` re-verifies the square-domain
//! evidence per run (aspect within 15% of square, residuals under
//! thresholds) — on failure the drape is disabled for that run with a status
//! message rather than draping wrong.
//!
//! The georef is cached per RUN id — the domain is fixed for a run, not per
//! forecast frame.

use eframe::egui;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Sounding-PNG endpoint (anonymous, CORS *):
/// `{SOUNDING_API}/{run_id}/{rundate+init}/0/wofs_snd_{row:02}_{col:02}.png`.
pub const SOUNDING_API: &str =
    "https://ep-wofs-sounding-etb5awe5cdfqawe8.a01.azurefd.net/api/sounding";

/// Product PNG geometry: the imagery CDN serves 900x800 PNGs whose axes box
/// (the georeferenced part) is the square pixel block (12,43)-(759,790).
pub const PRODUCT_W: f32 = 900.0;
pub const PRODUCT_H: f32 = 800.0;
pub const AXES_LEFT: f32 = 12.0;
pub const AXES_TOP: f32 = 43.0;
pub const AXES_SIZE: f32 = 747.0;

/// Calibration lattice stations (row, col), corners + spread first so an
/// early exit still sees the whole domain.
pub const CALIBRATION_STATIONS: &[(u32, u32)] = &[
    (1, 1),
    (1, 20),
    (20, 1),
    (20, 20),
    (10, 10),
    (5, 15),
    (15, 5),
    (12, 14),
    (1, 10),
    (20, 10),
    (10, 1),
    (10, 20),
    (5, 5),
    (15, 15),
    (3, 17),
    (17, 3),
    (2, 2),
    (2, 19),
    (19, 2),
    (19, 19),
    (7, 7),
    (13, 13),
    (4, 10),
    (16, 10),
    (10, 4),
    (10, 16),
    (6, 18),
    (18, 6),
];

/// Binarized glyph templates (char, width, row-major bits) auto-harvested
/// from the actual matplotlib title rendering of WoFS sounding PNGs.
#[rustfmt::skip]
const GLYPHS: &[(char, usize, &[u8])] = &[
    (',', 2, &[1,1,1,1,1,1,1,0]),
    ('-', 4, &[1,1,1,1]),
    ('.', 2, &[1,1,1,1]),
    ('0', 7, &[0,0,1,1,1,0,0,0,1,1,1,1,1,0,0,1,1,0,0,1,1,1,1,0,0,0,1,1,1,1,0,0,0,1,1,1,1,0,0,0,1,1,0,1,1,0,0,1,1,0,1,1,1,1,1,0,0,0,1,1,1,0,0]),
    ('1', 6, &[1,1,1,1,0,0,1,1,1,1,0,0,0,0,1,1,0,0,0,0,1,1,0,0,0,0,1,1,0,0,0,0,1,1,0,0,0,0,1,1,0,0,1,1,1,1,1,0,1,1,1,1,1,1]),
    ('2', 6, &[1,1,1,1,0,0,1,1,1,1,1,0,0,0,0,1,1,0,0,0,0,1,1,0,0,0,0,1,1,0,0,0,1,1,0,0,0,1,1,0,0,0,1,1,1,0,0,0,1,1,1,1,1,1]),
    ('3', 6, &[0,1,1,1,1,0,0,1,1,1,1,1,0,0,0,0,1,1,0,0,0,1,1,1,0,0,1,1,1,0,0,0,0,0,1,1,0,0,0,0,1,1,1,1,1,1,1,1,0,1,1,1,1,0]),
    ('4', 7, &[0,0,0,1,1,1,0,0,0,0,1,1,1,0,0,0,1,0,1,1,0,0,1,1,0,1,1,0,0,1,0,0,1,1,0,1,1,1,1,1,1,1,1,1,1,1,1,1,1,0,0,0,0,1,1,0,0,0,0,0,1,1,0]),
    ('5', 6, &[1,1,1,1,1,0,1,1,0,0,0,0,1,1,0,0,0,0,1,1,1,1,0,0,1,1,1,1,1,1,0,0,0,0,1,1,0,0,0,0,1,1,1,1,1,1,1,0,1,1,1,1,0,0]),
    ('6', 6, &[0,0,1,1,1,0,0,1,1,1,1,1,1,1,0,0,0,0,1,1,1,1,1,0,1,1,1,1,1,1,1,1,0,0,1,1,1,1,0,0,1,1,1,1,1,1,1,1,0,1,1,1,1,0]),
    ('7', 6, &[1,1,1,1,1,1,0,0,0,0,1,1,0,0,0,0,1,1,0,0,0,1,1,0,0,0,0,1,1,0,0,0,0,1,1,0,0,0,1,1,0,0,0,0,1,1,0,0,0,1,1,0,0,0]),
    ('8', 7, &[0,1,1,1,1,0,0,0,1,1,1,1,1,0,1,1,0,0,1,1,0,0,1,1,1,1,1,0,0,1,1,1,1,1,0,1,1,0,0,1,1,0,1,1,0,0,0,1,1,1,1,1,1,1,1,0,0,1,1,1,1,0,0]),
    ('9', 7, &[0,0,1,1,1,0,0,0,1,1,1,1,1,0,1,1,0,0,1,1,0,1,1,0,0,1,1,0,1,1,1,1,1,1,1,0,1,1,1,1,1,0,0,0,0,0,1,1,0,0,1,1,1,1,1,0,0,1,1,1,1,0,0]),
];

/// Title strip rows of the sounding PNG (binarized at luma < 128).
const STRIP_TOP: usize = 10;
const STRIP_BOTTOM: usize = 32;

/// Fitted bilinear georeference for one WoFS run, exposing axes-box
/// fractions (u, v) — v from TOP — to (lon, lat).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct WofsGeoref {
    /// value = c[0] + c[1]*fx + c[2]*fy + c[3]*fx*fy, fy from BOTTOM.
    lat_c: [f64; 4],
    lon_c: [f64; 4],
    pub lat_inliers: usize,
    pub lon_inliers: usize,
    pub lat_max_resid: f64,
    pub lon_max_resid: f64,
}

impl WofsGeoref {
    pub(crate) fn from_coeffs(
        lat_c: [f64; 4],
        lon_c: [f64; 4],
        lat_inliers: usize,
        lon_inliers: usize,
        lat_max_resid: f64,
        lon_max_resid: f64,
    ) -> Self {
        Self {
            lat_c,
            lon_c,
            lat_inliers,
            lon_inliers,
            lat_max_resid,
            lon_max_resid,
        }
    }

    /// (lon, lat) of an AXES-BOX fraction; u rightward, v DOWN from the top
    /// (texture convention). Internally fy = 1 - v (station fractions count
    /// from the bottom, matplotlib y-up).
    pub fn lonlat_of(&self, u: f32, v: f32) -> (f32, f32) {
        let (fx, fy) = (f64::from(u), 1.0 - f64::from(v));
        let basis = [1.0, fx, fy, fx * fy];
        let dot = |c: &[f64; 4]| c.iter().zip(basis).map(|(a, b)| a * b).sum::<f64>();
        (dot(&self.lon_c) as f32, dot(&self.lat_c) as f32)
    }

    /// Runtime verification of the square-domain basis evidence (see module
    /// docs). Errors describe why the drape was disabled for this run.
    pub fn sanity_check(&self) -> Result<(), String> {
        if self.lat_max_resid > 0.1 || self.lon_max_resid > 0.1 {
            return Err(format!(
                "fit residuals too large ({:.3}/{:.3} deg)",
                self.lat_max_resid, self.lon_max_resid
            ));
        }
        let (w_lon0, w_lat0) = self.lonlat_of(0.0, 0.5);
        let (w_lon1, w_lat1) = self.lonlat_of(1.0, 0.5);
        let (h_lon0, h_lat0) = self.lonlat_of(0.5, 0.0);
        let (h_lon1, h_lat1) = self.lonlat_of(0.5, 1.0);
        let width_km = haversine_km(
            f64::from(w_lat0),
            f64::from(w_lon0),
            f64::from(w_lat1),
            f64::from(w_lon1),
        );
        let height_km = haversine_km(
            f64::from(h_lat0),
            f64::from(h_lon0),
            f64::from(h_lat1),
            f64::from(h_lon1),
        );
        if !(400.0..=1500.0).contains(&width_km) || !(400.0..=1500.0).contains(&height_km) {
            return Err(format!(
                "implausible domain size ({width_km:.0} x {height_km:.0} km)"
            ));
        }
        let aspect = width_km / height_km;
        if !(0.85..=1.15).contains(&aspect) {
            return Err(format!(
                "domain not square (aspect {aspect:.2}) — axes-box basis assumption violated"
            ));
        }
        let (center_lon, center_lat) = self.lonlat_of(0.5, 0.5);
        if !(20.0..=55.0).contains(&center_lat) || !(-130.0..=-60.0).contains(&center_lon) {
            return Err(format!(
                "domain center off-CONUS ({center_lat:.2}, {center_lon:.2})"
            ));
        }
        Ok(())
    }
}

/// Great-circle distance (haversine, R = 6371 km).
fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let (dp, dl) = ((lat2 - lat1).to_radians(), (lon2 - lon1).to_radians());
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * 6371.0 * a.sqrt().asin()
}

// ---------------------------------------------------------------------------
// OCR: title strip -> (lat, lon)
// ---------------------------------------------------------------------------

/// Small binary bitmap (row-major bools).
struct Bitmap {
    w: usize,
    h: usize,
    bits: Vec<bool>,
}

impl Bitmap {
    fn get(&self, y: usize, x: usize) -> bool {
        self.bits[y * self.w + x]
    }
}

/// OCR result for one sounding title.
#[derive(Debug)]
pub struct TitleOcr {
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    /// Matched tokens — test assertions + failure diagnostics only.
    #[allow(dead_code)]
    pub raw: String,
}

/// OCR the title line of a sounding PNG (grayscale buffer, row-major).
pub fn ocr_title_gray(width: usize, height: usize, luma: &[u8]) -> TitleOcr {
    if height < STRIP_BOTTOM || luma.len() < width * STRIP_BOTTOM {
        return TitleOcr {
            lat: None,
            lon: None,
            raw: String::new(),
        };
    }
    let strip_h = STRIP_BOTTOM - STRIP_TOP;
    let bits: Vec<bool> = (0..strip_h * width)
        .map(|i| luma[(STRIP_TOP + i / width) * width + i % width] < 128)
        .collect();
    let strip = Bitmap {
        w: width,
        h: strip_h,
        bits,
    };

    // Tokens: runs of matched chars; break on unmatched segment, wide gap,
    // or comma — mirrors the validated calibration tokenizer exactly.
    let mut tokens: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut last_x1: Option<usize> = None;
    for (x0, x1) in column_segments(&strip) {
        let Some(seg) = bbox_trim(&strip, x0, x1) else {
            continue;
        };
        let ch = match_char(&seg);
        let gap_break = last_x1.is_some_and(|last| x0.saturating_sub(last) > 4);
        if ch.is_none() || gap_break || ch == Some(',') {
            if !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
            match ch {
                Some(',') => tokens.push(",".to_owned()),
                Some(c) => cur.push(c),
                None => {}
            }
        } else if let Some(c) = ch {
            cur.push(c);
        }
        last_x1 = Some(x1);
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }

    // Structural parse: lat and lon extracted INDEPENDENTLY (one clean
    // number still contributes a half-constraint).
    let (mut lat, mut lon) = (None, None);
    for token in &tokens {
        let Some(value) = parse_coord_token(token) else {
            continue;
        };
        if lat.is_none() && (20.0..=55.0).contains(&value) {
            lat = Some(value);
        } else if lon.is_none() && (-130.0..=-60.0).contains(&value) {
            lon = Some(value);
        }
    }
    TitleOcr {
        lat,
        lon,
        raw: tokens.join(" "),
    }
}

/// Contiguous runs of columns containing ink.
fn column_segments(strip: &Bitmap) -> Vec<(usize, usize)> {
    let mut segments = Vec::new();
    let mut start: Option<usize> = None;
    for x in 0..strip.w {
        let on = (0..strip.h).any(|y| strip.get(y, x));
        match (on, start) {
            (true, None) => start = Some(x),
            (false, Some(s)) => {
                segments.push((s, x));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        segments.push((s, strip.w));
    }
    segments
}

/// Vertically trim a column segment to its ink bounding box.
fn bbox_trim(strip: &Bitmap, x0: usize, x1: usize) -> Option<Bitmap> {
    let row_has_ink = |y: usize| (x0..x1).any(|x| strip.get(y, x));
    let r0 = (0..strip.h).find(|&y| row_has_ink(y))?;
    let r1 = (0..strip.h).rfind(|&y| row_has_ink(y))? + 1;
    let (w, h) = (x1 - x0, r1 - r0);
    let bits = (0..h * w)
        .map(|i| strip.get(r0 + i / w, x0 + i % w))
        .collect();
    Some(Bitmap { w, h, bits })
}

/// Sliding-alignment fuzzy match: the template slides over the padded
/// segment (effectively +-2 px both axes); score = mismatched bits /
/// template area. Strict accept (<= 0.09) + margin over the runner-up
/// (>= 0.03) or the char is dropped.
fn match_char(seg: &Bitmap) -> Option<char> {
    let (h, w) = (seg.h, seg.w);
    let mut scores: Vec<(char, f64)> = Vec::new();
    for &(ch, tw, tbits) in GLYPHS {
        let th = tbits.len() / tw;
        if th.abs_diff(h) > 2 || tw.abs_diff(w) > 2 {
            continue;
        }
        let (ch_h, ch_w) = (th.max(h) + 2, tw.max(w) + 2);
        let mut best = f64::INFINITY;
        for dy in 0..=(ch_h - th) {
            for dx in 0..=(ch_w - tw) {
                let mut mismatches = 0usize;
                for y in 0..ch_h {
                    for x in 0..ch_w {
                        // Segment pinned at offset (1,1) in the canvas.
                        let seg_bit =
                            y >= 1 && y < 1 + h && x >= 1 && x < 1 + w && seg.get(y - 1, x - 1);
                        let tmpl_bit = y >= dy
                            && y < dy + th
                            && x >= dx
                            && x < dx + tw
                            && tbits[(y - dy) * tw + (x - dx)] != 0;
                        if seg_bit != tmpl_bit {
                            mismatches += 1;
                        }
                    }
                }
                let score = mismatches as f64 / (th * tw) as f64;
                if score < best {
                    best = score;
                }
            }
        }
        scores.push((ch, best));
    }
    scores.sort_by(|a, b| a.1.total_cmp(&b.1));
    let (ch, best) = *scores.first()?;
    if best > 0.09 {
        return None;
    }
    if let Some((_, runner_up)) = scores.get(1)
        && runner_up - best < 0.03
    {
        return None;
    }
    Some(ch)
}

/// Parse a coordinate token: `^-?\d{2,3}\.\d{2}$`. WoFS titles print %.2f,
/// so a COMPLETE number has exactly two decimals — truncated reads like
/// "44." or "-94.0" must REJECT, not parse short.
pub fn parse_coord_token(token: &str) -> Option<f64> {
    let unsigned = token.strip_prefix('-').unwrap_or(token);
    let (int_part, frac_part) = unsigned.split_once('.')?;
    if !(2..=3).contains(&int_part.len())
        || frac_part.len() != 2
        || !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    token.parse().ok()
}

// ---------------------------------------------------------------------------
// Bilinear least-squares fit with RANSAC-style trim
// ---------------------------------------------------------------------------

/// Station-lattice domain fractions (fx rightward, fy from BOTTOM) of
/// sounding station RR_CC (1-based row/col, 01..20).
pub fn station_fraction(row: u32, col: u32) -> (f64, f64) {
    (
        0.031 + 0.049 * f64::from(col - 1),
        0.031 + 0.049 * f64::from(row - 1),
    )
}

fn bilinear_residual(c: &[f64; 4], (fx, fy, value): (f64, f64, f64)) -> f64 {
    (c[0] + c[1] * fx + c[2] * fy + c[3] * fx * fy - value).abs()
}

/// Plain least squares via 4x4 normal equations (well-conditioned for the
/// spread lattice; partial-pivot Gaussian elimination).
fn lstsq_bilinear(points: &[(f64, f64, f64)]) -> Option<[f64; 4]> {
    let mut m = [[0.0f64; 5]; 4];
    for &(fx, fy, value) in points {
        let basis = [1.0, fx, fy, fx * fy];
        for r in 0..4 {
            for c in 0..4 {
                m[r][c] += basis[r] * basis[c];
            }
            m[r][4] += basis[r] * value;
        }
    }
    // Gaussian elimination with partial pivoting on the augmented matrix.
    for col in 0..4 {
        let pivot = (col..4).max_by(|&a, &b| m[a][col].abs().total_cmp(&m[b][col].abs()))?;
        if m[pivot][col].abs() < 1e-12 {
            return None;
        }
        m.swap(col, pivot);
        let pivot_row = m[col];
        for (row, target_row) in m.iter_mut().enumerate() {
            if row == col {
                continue;
            }
            let factor = target_row[col] / pivot_row[col];
            for (target, pivot_value) in target_row.iter_mut().zip(pivot_row).skip(col) {
                *target -= factor * pivot_value;
            }
        }
    }
    Some([
        m[0][4] / m[0][0],
        m[1][4] / m[1][1],
        m[2][4] / m[2][2],
        m[3][4] / m[3][3],
    ])
}

/// RANSAC-style bilinear fit of (fx, fy) -> value (Fischler & Bolles 1981,
/// CACM, doi:10.1145/358669.358692 — here as a deterministic
/// fit/trim/refit since the lattice gives a strong initial consensus):
/// fit all points, keep residual < 0.1 deg, require >= 6 inliers, refit.
/// Returns (coeffs, inlier count, max inlier residual).
pub fn fit_bilinear(points: &[(f64, f64, f64)]) -> Option<([f64; 4], usize, f64)> {
    let initial = lstsq_bilinear(points)?;
    let inliers: Vec<(f64, f64, f64)> = points
        .iter()
        .copied()
        .filter(|&p| bilinear_residual(&initial, p) < 0.1)
        .collect();
    if inliers.len() < 6 {
        return None;
    }
    let refit = lstsq_bilinear(&inliers)?;
    let max_resid = inliers
        .iter()
        .map(|&p| bilinear_residual(&refit, p))
        .fold(0.0f64, f64::max);
    Some((refit, inliers.len(), max_resid))
}

// ---------------------------------------------------------------------------
// End-to-end georef build (blocking; run on a worker thread)
// ---------------------------------------------------------------------------

/// Number of stations the build may fetch (for progress display).
pub const CALIBRATION_TOTAL: usize = CALIBRATION_STATIONS.len();

/// Build the georef for one run by OCR-ing lattice sounding titles and
/// fitting the bilinear mapping. `rd_init` is rundate+init ("202606111700").
/// Blocking (~20 small CDN fetches) — call from a background thread;
/// `progress` counts stations processed for the status line.
pub fn build_georef(
    run_id: &str,
    rd_init: &str,
    progress: Option<&AtomicUsize>,
) -> Result<WofsGeoref, String> {
    let mut lat_pts: Vec<(f64, f64, f64)> = Vec::new();
    let mut lon_pts: Vec<(f64, f64, f64)> = Vec::new();
    for &(row, col) in CALIBRATION_STATIONS {
        if let Some(progress) = progress {
            progress.fetch_add(1, Ordering::Relaxed);
        }
        // Enough well-spread constraints already (corners come first).
        if lat_pts.len() >= 18 && lon_pts.len() >= 18 {
            break;
        }
        let url = format!("{SOUNDING_API}/{run_id}/{rd_init}/0/wofs_snd_{row:02}_{col:02}.png");
        let Ok(bytes) = data_source::fetch_bytes(&url) else {
            continue;
        };
        let Ok(decoded) = image::load_from_memory(&bytes) else {
            continue;
        };
        let gray = decoded.to_luma8();
        let ocr = ocr_title_gray(gray.width() as usize, gray.height() as usize, gray.as_raw());
        let (fx, fy) = station_fraction(row, col);
        if let Some(lat) = ocr.lat {
            lat_pts.push((fx, fy, lat));
        }
        if let Some(lon) = ocr.lon {
            lon_pts.push((fx, fy, lon));
        }
    }
    if lat_pts.len() < 8 || lon_pts.len() < 8 {
        return Err(format!(
            "too few OCR constraints ({} lat / {} lon)",
            lat_pts.len(),
            lon_pts.len()
        ));
    }
    let (lat_c, lat_inliers, lat_max_resid) =
        fit_bilinear(&lat_pts).ok_or("lat fit: too few inliers")?;
    let (lon_c, lon_inliers, lon_max_resid) =
        fit_bilinear(&lon_pts).ok_or("lon fit: too few inliers")?;
    let georef = WofsGeoref::from_coeffs(
        lat_c,
        lon_c,
        lat_inliers,
        lon_inliers,
        lat_max_resid,
        lon_max_resid,
    );
    georef.sanity_check()?;
    Ok(georef)
}

// ---------------------------------------------------------------------------
// Map drape mesh
// ---------------------------------------------------------------------------

/// Drape tessellation: an 8x8 quad grid absorbs the AEQD map projection's
/// curvature across the ~900 km domain.
pub const DRAPE_GRID: usize = 8;

/// Build the textured drape mesh: vertices at `project(lonlat_of(u, v))`,
/// UVs into the axes-box subrect of the full product texture.
pub fn drape_mesh(
    texture_id: egui::TextureId,
    georef: &WofsGeoref,
    opacity: f32,
    project: &dyn Fn(f32, f32) -> egui::Pos2,
) -> egui::epaint::Mesh {
    let n = DRAPE_GRID;
    let color = egui::Color32::from_white_alpha((opacity.clamp(0.0, 1.0) * 255.0) as u8);
    let mut mesh = egui::epaint::Mesh::with_texture(texture_id);
    for j in 0..=n {
        for i in 0..=n {
            let (u, v) = (i as f32 / n as f32, j as f32 / n as f32);
            let (lon, lat) = georef.lonlat_of(u, v);
            mesh.vertices.push(egui::epaint::Vertex {
                pos: project(lon, lat),
                uv: egui::pos2(
                    (AXES_LEFT + u * AXES_SIZE) / PRODUCT_W,
                    (AXES_TOP + v * AXES_SIZE) / PRODUCT_H,
                ),
                color,
            });
        }
    }
    let stride = (n + 1) as u32;
    for j in 0..n as u32 {
        for i in 0..n as u32 {
            let a = j * stride + i;
            let (b, c, d) = (a + 1, a + stride, a + stride + 1);
            mesh.add_triangle(a, c, b);
            mesh.add_triangle(b, c, d);
        }
    }
    mesh
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Title strips (rows 0..40, grayscale) cropped from real sounding PNGs
    /// of run WOFSRun20260611-144912d1 rt 202606111700, captured 2026-06-11.
    const TITLE_10_10: &[u8] = include_bytes!("../testdata/wofs_title_10_10.png");
    const TITLE_01_01: &[u8] = include_bytes!("../testdata/wofs_title_01_01.png");

    fn ocr_fixture(bytes: &[u8]) -> TitleOcr {
        let gray = image::load_from_memory(bytes)
            .expect("fixture decodes")
            .to_luma8();
        ocr_title_gray(gray.width() as usize, gray.height() as usize, gray.as_raw())
    }

    #[test]
    fn ocr_reads_clean_title_fixture() {
        // "WoFS Sounding 41.02N, -89.66W ..." — both numbers clean.
        let ocr = ocr_fixture(TITLE_10_10);
        assert_eq!(ocr.lat, Some(41.02), "raw: {}", ocr.raw);
        assert_eq!(ocr.lon, Some(-89.66), "raw: {}", ocr.raw);
    }

    #[test]
    fn ocr_keeps_half_constraint_and_rejects_truncated_lon() {
        // Station 01_01's lon glyph run reads back truncated ("-94.0…"):
        // the 2-decimal rule must reject it while the clean lat survives.
        let ocr = ocr_fixture(TITLE_01_01);
        assert_eq!(ocr.lat, Some(37.42), "raw: {}", ocr.raw);
        assert_eq!(ocr.lon, None, "raw: {}", ocr.raw);
    }

    #[test]
    fn coord_token_requires_exactly_two_decimals() {
        assert_eq!(parse_coord_token("37.42"), Some(37.42));
        assert_eq!(parse_coord_token("-89.66"), Some(-89.66));
        assert_eq!(parse_coord_token("-104.95"), Some(-104.95));
        // The poison cases: truncated reads must NOT parse short.
        assert_eq!(parse_coord_token("44."), None);
        assert_eq!(parse_coord_token("-94.0"), None);
        assert_eq!(parse_coord_token("44"), None);
        assert_eq!(parse_coord_token("4.42"), None);
        assert_eq!(parse_coord_token("1234.56"), None);
        assert_eq!(parse_coord_token("44.123"), None);
        assert_eq!(parse_coord_token("4a.12"), None);
        assert_eq!(parse_coord_token(""), None);
    }

    /// Synthetic lattice from realistic coefficients (live 2026-06-11 fit).
    fn synthetic_points(c: &[f64; 4]) -> Vec<(f64, f64, f64)> {
        CALIBRATION_STATIONS
            .iter()
            .map(|&(row, col)| {
                let (fx, fy) = station_fraction(row, col);
                (fx, fy, c[0] + c[1] * fx + c[2] * fy + c[3] * fx * fy)
            })
            .collect()
    }

    #[test]
    fn bilinear_fit_recovers_synthetic_lattice() {
        let truth = [37.2, 0.3, 7.2, -0.3];
        let (fit, inliers, max_resid) = fit_bilinear(&synthetic_points(&truth)).expect("fit");
        assert_eq!(inliers, CALIBRATION_STATIONS.len());
        assert!(max_resid < 1e-9, "max resid {max_resid}");
        for (a, b) in fit.iter().zip(truth) {
            assert!((a - b).abs() < 1e-9, "{fit:?} != {truth:?}");
        }
    }

    #[test]
    fn ransac_trim_survives_a_poisoned_constraint() {
        let truth = [-94.4, 9.6, 0.05, 0.9];
        let mut points = synthetic_points(&truth);
        points[5].2 += 1.5; // one badly mis-OCR'd station
        let (fit, inliers, max_resid) = fit_bilinear(&points).expect("fit");
        // The outlier's pull on the initial fit may trim a few good points
        // with it; what matters is that the consensus refit excludes the
        // poison and recovers the truth exactly (the survivors are exact).
        assert!(inliers < points.len(), "outlier must be dropped");
        assert!(inliers >= 6);
        assert!(max_resid < 1e-6, "max resid {max_resid}");
        for (a, b) in fit.iter().zip(truth) {
            assert!((a - b).abs() < 1e-6, "{fit:?} != {truth:?}");
        }
    }

    #[test]
    fn sanity_check_accepts_square_domain_and_rejects_stretched() {
        // ~885 km square domain (the live 2026-06-11 fit shape).
        let square = WofsGeoref::from_coeffs(
            [37.2, 0.3, 7.95, -0.3],
            [-94.4, 10.1, 0.05, 0.9],
            20,
            20,
            0.05,
            0.003,
        );
        assert!(square.sanity_check().is_ok(), "{:?}", square.sanity_check());
        // Same lon span but double the lat span: aspect breaks -> the
        // axes-box basis assumption is violated and the drape must disable.
        let stretched = WofsGeoref::from_coeffs(
            [30.0, 0.3, 16.0, -0.3],
            [-94.4, 10.1, 0.05, 0.9],
            20,
            20,
            0.05,
            0.003,
        );
        assert!(stretched.sanity_check().is_err());
    }

    #[test]
    fn lonlat_of_flips_v_from_top() {
        let georef = WofsGeoref::from_coeffs(
            [37.2, 0.3, 7.95, -0.3],
            [-94.4, 10.1, 0.05, 0.9],
            20,
            20,
            0.05,
            0.003,
        );
        let (_, top_lat) = georef.lonlat_of(0.5, 0.0);
        let (_, bottom_lat) = georef.lonlat_of(0.5, 1.0);
        assert!(
            top_lat > bottom_lat,
            "v=0 is the TOP of the image (north): {top_lat} vs {bottom_lat}"
        );
    }

    #[test]
    fn drape_mesh_covers_axes_box_uvs() {
        let georef = WofsGeoref::from_coeffs(
            [37.2, 0.3, 7.95, -0.3],
            [-94.4, 10.1, 0.05, 0.9],
            20,
            20,
            0.05,
            0.003,
        );
        let mesh = drape_mesh(egui::TextureId::default(), &georef, 1.0, &|lon, lat| {
            egui::pos2(lon, lat)
        });
        assert_eq!(mesh.vertices.len(), (DRAPE_GRID + 1) * (DRAPE_GRID + 1));
        assert_eq!(mesh.indices.len(), DRAPE_GRID * DRAPE_GRID * 6);
        let first = mesh.vertices.first().unwrap().uv;
        let last = mesh.vertices.last().unwrap().uv;
        assert!((first.x - AXES_LEFT / PRODUCT_W).abs() < 1e-6);
        assert!((first.y - AXES_TOP / PRODUCT_H).abs() < 1e-6);
        assert!((last.x - (AXES_LEFT + AXES_SIZE) / PRODUCT_W).abs() < 1e-6);
        assert!((last.y - (AXES_TOP + AXES_SIZE) / PRODUCT_H).abs() < 1e-6);
        // Vertex positions follow lonlat_of (identity projection here):
        // north (higher lat) at the top row of the mesh.
        assert!(mesh.vertices[0].pos.y > mesh.vertices.last().unwrap().pos.y);
    }

    /// Live end-to-end calibration of the validated reference run. Network.
    /// Run with: cargo test -p app_ui live_georef -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_georef_of_run_20260611_144912d1() {
        let georef = build_georef("WOFSRun20260611-144912d1", "202606111700", None)
            .expect("live georef build");
        // The validated 2026-06-11 17z fit: domain centered near
        // (41.0, -89.6), ~885 km square, residuals 0.05/0.003 deg.
        let (center_lon, center_lat) = georef.lonlat_of(0.5, 0.5);
        assert!(
            (40.0..=42.0).contains(&center_lat),
            "center lat {center_lat}"
        );
        assert!(
            (-91.0..=-88.0).contains(&center_lon),
            "center lon {center_lon}"
        );
        assert!(georef.lat_max_resid <= 0.1, "{}", georef.lat_max_resid);
        assert!(georef.lon_max_resid <= 0.1, "{}", georef.lon_max_resid);
        assert!(georef.lat_inliers >= 6 && georef.lon_inliers >= 6);
        println!(
            "live georef OK: {} lat / {} lon inliers, max resid {:.4}/{:.4} deg, center ({:.2}, {:.2})",
            georef.lat_inliers,
            georef.lon_inliers,
            georef.lat_max_resid,
            georef.lon_max_resid,
            center_lat,
            center_lon
        );
    }
}
