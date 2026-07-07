//! Offline verification for the JTWC wind-radii + 34-kt danger-area feature.
//!
//! Fetches the live JTWC Tropical Cyclone Warning (default: Super Typhoon 09W
//! BAVI, `wp0926web.txt`), parses the 34/50/64-kt quadrant wind radii at the
//! analysis point and every forecast time, prints them, and rasterizes the same
//! geometry `draw_tropical` builds — wind-radii roses, the 34-kt danger area,
//! the forecast track + category dots — to a PNG using the app's exact AEQD
//! projection (`ui_core::geo::aeqd_forward_km`). This lets the shapes be looked
//! at directly on REAL bulletin data (never synthetic), the way the in-app
//! overlay would draw them at a West-Pacific map view.
//!
//! Usage:
//!   cargo run -p app_ui --example tropical_radii_probe [-- <url-or-path> <out.png> [envelope|hull]]
//! With no args it fetches the live BAVI warning and falls back to the checked-in
//! real capture `crates/data_source/tests/fixtures/tropical/jtwc_bavi_warning.txt`.
//! The optional third arg picks the 34-kt swath construction: `envelope`
//! (default — the shipped tapered `track_circle_envelope`, hugging a curved
//! track) or `hull` (the pre-v0.29.3 convex hull, kept for before/after
//! comparison: on a recurving track it straight-lines beginning-to-end and
//! draws the cone fat across the bend).

use data_source::tropical::{
    self, Category, GeoPoint, WindRadii, convex_hull, danger_area_34kt, wind_radii_ring,
};
use ui_core::geo::aeqd_forward_km;

const LIVE_URL: &str = "https://www.metoc.navy.mil/jtwc/products/wp0926web.txt";
const FIXTURE: &str = "crates/data_source/tests/fixtures/tropical/jtwc_bavi_warning.txt";

// A West-Pacific map view framing Guam → the East China Sea — wide enough for
// warning #25's full recurve (16°N 140°E west then poleward to 29°N 119°E)
// plus its ~260-NM gale radii.
const W: u32 = 1200;
const H: u32 = 900;
const CENTER_LAT: f64 = 20.0;
const CENTER_LON: f64 = 132.0;
const MAP_SCALE: f64 = 33.0; // pixels per degree of latitude (== app's map_scale)

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let source_arg = args.first().map(String::as_str);
    let out_path = args
        .get(1)
        .map(String::as_str)
        .unwrap_or("tropical_radii_probe.png");

    let mode = args.get(2).map(String::as_str).unwrap_or("envelope");

    let (text, origin) = load_warning(source_arg);
    println!(
        "== JTWC warning source: {origin} ({} bytes) ==\n",
        text.len()
    );

    let current = tropical::parse_jtwc_current_radii(&text);
    let forecast = tropical::parse_jtwc_forecast_warning(&text);

    // ---- print the parsed radii per time (REAL-DATA proof) ----------------
    println!("ANALYSIS (current)  radii: {}", fmt_radii_set(&current));
    println!();
    println!(
        "{:<10} {:>6} {:>4}  34/50/64-kt quadrant radii NE/SE/SW/NW (NM)",
        "VALID", "WIND", "CAT"
    );
    for p in &forecast {
        let cat = p.max_wind_kt.map(Category::from_wind_kt);
        let t = p
            .valid_time
            .map(|v| v.format("%d/%H%MZ").to_string())
            .unwrap_or_else(|| "-".into());
        println!(
            "{:<10} {:>4}kt {:>4}  {}",
            t,
            p.max_wind_kt.map(|k| k as i32).unwrap_or(0),
            cat.map(cat_short).unwrap_or("?"),
            fmt_radii_set(&p.wind_radii),
        );
    }

    // ---- render the geometry to a PNG -------------------------------------
    let mut img = image::RgbaImage::from_pixel(W, H, image::Rgba([18, 24, 34, 255]));
    draw_graticule(&mut img);

    // 34-kt danger area (teal fill + red outline): the shipped tapered
    // envelope, or the legacy convex hull when comparing before/after.
    let danger_pts: Vec<(GeoPoint, &[WindRadii])> =
        std::iter::once((current_position(&text), current.as_slice()))
            .chain(
                forecast
                    .iter()
                    .map(|p| (p.position, p.wind_radii.as_slice())),
            )
            .collect();
    let swath = match mode {
        "hull" => legacy_hull_34kt(danger_pts.iter().copied()),
        _ => danger_area_34kt(danger_pts.iter().copied()),
    };
    println!("\n34-kt swath mode={mode}: {} boundary points", swath.len());
    let swath_px: Vec<(f64, f64)> = swath.iter().map(|g| project(*g)).collect();
    fill_polygon(&mut img, &swath_px, [30, 200, 190], 0.16);
    draw_closed(&mut img, &swath_px, [235, 70, 70], 0.85, 2);

    // Wind-radii roses at the current position + each forecast time.
    let rose_centers = std::iter::once((current_position(&text), &current))
        .chain(forecast.iter().map(|p| (p.position, &p.wind_radii)));
    for (center, set) in rose_centers {
        for r in set {
            let ring: Vec<(f64, f64)> = wind_radii_ring(center, r, 10)
                .iter()
                .map(|g| project(*g))
                .collect();
            let (color, alpha) = wind_radii_color(r.kt);
            draw_closed(&mut img, &ring, color, alpha, 1);
        }
    }

    // Forecast track line + category dots + current glyph.
    let mut track: Vec<(f64, f64)> = vec![project(current_position(&text))];
    track.extend(forecast.iter().map(|p| project(p.position)));
    draw_polyline(&mut img, &track, [245, 245, 245], 0.85, 1);
    for p in &forecast {
        let cat = p.max_wind_kt.map(Category::from_wind_kt);
        fill_circle(&mut img, project(p.position), 5.0, cat_color(cat), 1.0);
        stroke_circle(&mut img, project(p.position), 5.0, [0, 0, 0], 1.0);
    }
    let cur_cat = forecast
        .first()
        .and_then(|p| p.max_wind_kt)
        .map(Category::from_wind_kt);
    fill_circle(
        &mut img,
        project(current_position(&text)),
        7.0,
        cat_color(cur_cat),
        1.0,
    );
    stroke_circle(
        &mut img,
        project(current_position(&text)),
        7.0,
        [0, 0, 0],
        1.0,
    );

    // Reference markers (yellow) — Guam and Taiwan — so the fan direction reads.
    let guam = GeoPoint {
        lon: 144.79,
        lat: 13.44,
    };
    let taiwan = GeoPoint {
        lon: 120.96,
        lat: 23.70,
    };
    for m in [guam, taiwan] {
        fill_circle(&mut img, project(m), 4.0, [255, 230, 40], 1.0);
    }
    println!(
        "\nReference px  Guam={:?}  Taiwan={:?}  (yellow dots)",
        pxi(project(guam)),
        pxi(project(taiwan))
    );

    img.save(out_path).expect("save png");
    println!("wrote {out_path} ({W}x{H})");
}

/// The pre-v0.29.3 danger-area construction — convex hull of the sampled
/// 34-kt roses — reproduced here only for `hull` mode so before/after PNGs of
/// the cone-taper fix can be rendered from the same bulletin. On a recurving
/// track (Bavi warning #25) its straight sides cut the inside of the bend and
/// bulge far outside it; the shipped envelope hugs the curve.
fn legacy_hull_34kt<'a>(
    points: impl Iterator<Item = (GeoPoint, &'a [WindRadii])>,
) -> Vec<GeoPoint> {
    let mut hull_input: Vec<GeoPoint> = Vec::new();
    for (center, radii) in points {
        if let Some(r34) = radii.iter().find(|r| r.kt == 34) {
            hull_input.extend(wind_radii_ring(center, r34, 6));
        }
    }
    if hull_input.len() < 3 {
        return Vec::new();
    }
    convex_hull(&hull_input)
}

// ---------------------------------------------------------------------------
// warning loading
// ---------------------------------------------------------------------------

fn load_warning(source: Option<&str>) -> (String, String) {
    if let Some(s) = source {
        if s.starts_with("http") {
            if let Some(body) = http_get(s) {
                return (body, format!("live {s}"));
            }
        } else if let Ok(body) = std::fs::read_to_string(s) {
            return (body, format!("file {s}"));
        }
    }
    // Default: try live BAVI, fall back to the checked-in real capture.
    if let Some(body) = http_get(LIVE_URL) {
        return (body, format!("live {LIVE_URL}"));
    }
    let body = std::fs::read_to_string(FIXTURE)
        .or_else(|_| std::fs::read_to_string(format!("../../{FIXTURE}")))
        .expect("fixture readable");
    (body, format!("fixture {FIXTURE}"))
}

fn http_get(url: &str) -> Option<String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("BowEcho tropical probe")
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .ok()?;
    let resp = client.get(url).send().ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.text().ok()
}

/// The `WARNING POSITION` (analysis) center — the anchor for the current radii.
fn current_position(text: &str) -> GeoPoint {
    // e.g. "060000Z --- NEAR 14.3N 145.0E"; parse the first lat/lon after it.
    for line in text.lines() {
        let l = line.trim();
        if let Some(rest) = l
            .strip_prefix("060000Z")
            .or_else(|| l.contains("NEAR").then_some(l))
            && let Some(p) = parse_latlon(rest)
        {
            return p;
        }
    }
    GeoPoint {
        lon: 145.0,
        lat: 14.3,
    }
}

fn parse_latlon(s: &str) -> Option<GeoPoint> {
    let toks: Vec<&str> = s.split_whitespace().collect();
    let lat_i = toks
        .iter()
        .position(|t| t.ends_with('N') || t.ends_with('S'))?;
    let lat_t = toks[lat_i];
    let lon_t = toks.get(lat_i + 1)?;
    let lat = lat_t[..lat_t.len() - 1].parse::<f32>().ok()?
        * if lat_t.ends_with('S') { -1.0 } else { 1.0 };
    let lon = lon_t[..lon_t.len() - 1].parse::<f32>().ok()?
        * if lon_t.ends_with('W') { -1.0 } else { 1.0 };
    Some(GeoPoint { lon, lat })
}

// ---------------------------------------------------------------------------
// formatting
// ---------------------------------------------------------------------------

fn fmt_radii_set(set: &[WindRadii]) -> String {
    if set.is_empty() {
        return "(none)".into();
    }
    set.iter()
        .map(|r| {
            format!(
                "{}:{}/{}/{}/{}",
                r.kt, r.ne_nm as i32, r.se_nm as i32, r.sw_nm as i32, r.nw_nm as i32
            )
        })
        .collect::<Vec<_>>()
        .join("  ")
}

fn cat_short(c: Category) -> &'static str {
    match c {
        Category::Five => "5",
        Category::Four => "4",
        Category::Three => "3",
        Category::Two => "2",
        Category::One => "1",
        Category::TropicalStorm => "TS",
        Category::TropicalDepression => "TD",
    }
}

fn cat_color(c: Option<Category>) -> [u8; 3] {
    match c {
        Some(Category::Five) => [255, 110, 245],
        Some(Category::Four) => [232, 66, 66],
        Some(Category::Three) => [245, 130, 50],
        Some(Category::Two) => [245, 200, 60],
        Some(Category::One) => [240, 240, 90],
        Some(Category::TropicalStorm) => [90, 210, 120],
        _ => [120, 190, 235],
    }
}

fn wind_radii_color(kt: u16) -> ([u8; 3], f32) {
    match kt {
        64 => ([255, 60, 130], 0.92),
        50 => ([255, 110, 205], 0.80),
        _ => ([210, 150, 235], 0.60),
    }
}

// ---------------------------------------------------------------------------
// projection + rasterization
// ---------------------------------------------------------------------------

fn project(g: GeoPoint) -> (f64, f64) {
    let (east_km, north_km) = aeqd_forward_km(CENTER_LAT, CENTER_LON, g.lat as f64, g.lon as f64);
    let px_per_km = MAP_SCALE / 111.32;
    (
        W as f64 / 2.0 + east_km * px_per_km,
        H as f64 / 2.0 - north_km * px_per_km,
    )
}

fn pxi(p: (f64, f64)) -> (i32, i32) {
    (p.0.round() as i32, p.1.round() as i32)
}

fn blend(img: &mut image::RgbaImage, x: i32, y: i32, c: [u8; 3], a: f32) {
    if x < 0 || y < 0 || x >= W as i32 || y >= H as i32 {
        return;
    }
    let px = img.get_pixel_mut(x as u32, y as u32);
    for i in 0..3 {
        px[i] = (c[i] as f32 * a + px[i] as f32 * (1.0 - a)).round() as u8;
    }
}

fn draw_graticule(img: &mut image::RgbaImage) {
    // 5° lon/lat lines across the framed region.
    for lon in (100..=160).step_by(5) {
        let pts: Vec<(f64, f64)> = (0..=60)
            .map(|i| {
                project(GeoPoint {
                    lon: lon as f32,
                    lat: (i as f32) - 5.0,
                })
            })
            .collect();
        draw_polyline(img, &pts, [55, 66, 82], 1.0, 1);
    }
    for lat in (0..=40).step_by(5) {
        let pts: Vec<(f64, f64)> = (100..=160)
            .map(|i| {
                project(GeoPoint {
                    lon: i as f32,
                    lat: lat as f32,
                })
            })
            .collect();
        draw_polyline(img, &pts, [55, 66, 82], 1.0, 1);
    }
}

fn draw_polyline(img: &mut image::RgbaImage, pts: &[(f64, f64)], c: [u8; 3], a: f32, r: i32) {
    for w in pts.windows(2) {
        draw_line(img, w[0], w[1], c, a, r);
    }
}

fn draw_closed(img: &mut image::RgbaImage, pts: &[(f64, f64)], c: [u8; 3], a: f32, r: i32) {
    draw_polyline(img, pts, c, a, r);
    if let (Some(f), Some(l)) = (pts.first(), pts.last()) {
        draw_line(img, *l, *f, c, a, r);
    }
}

fn draw_line(
    img: &mut image::RgbaImage,
    p0: (f64, f64),
    p1: (f64, f64),
    c: [u8; 3],
    a: f32,
    r: i32,
) {
    let dx = p1.0 - p0.0;
    let dy = p1.1 - p0.1;
    let steps = dx.abs().max(dy.abs()).ceil().max(1.0) as i32;
    for s in 0..=steps {
        let t = s as f64 / steps as f64;
        let x = (p0.0 + dx * t).round() as i32;
        let y = (p0.1 + dy * t).round() as i32;
        for oy in -r..=r {
            for ox in -r..=r {
                blend(img, x + ox, y + oy, c, a);
            }
        }
    }
}

fn fill_circle(img: &mut image::RgbaImage, ctr: (f64, f64), rad: f64, c: [u8; 3], a: f32) {
    let r = rad.ceil() as i32;
    for oy in -r..=r {
        for ox in -r..=r {
            if (ox as f64).hypot(oy as f64) <= rad {
                blend(img, ctr.0 as i32 + ox, ctr.1 as i32 + oy, c, a);
            }
        }
    }
}

fn stroke_circle(img: &mut image::RgbaImage, ctr: (f64, f64), rad: f64, c: [u8; 3], a: f32) {
    for deg in 0..360 {
        let t = (deg as f64).to_radians();
        let x = ctr.0 + rad * t.cos();
        let y = ctr.1 + rad * t.sin();
        blend(img, x.round() as i32, y.round() as i32, c, a);
    }
}

/// Even-odd scanline polygon fill — exact for any simple ring, including the
/// tapered envelope's concave inner bend.
fn fill_polygon(img: &mut image::RgbaImage, pts: &[(f64, f64)], c: [u8; 3], a: f32) {
    if pts.len() < 3 {
        return;
    }
    let min_y = pts
        .iter()
        .map(|p| p.1)
        .fold(f64::INFINITY, f64::min)
        .floor()
        .max(0.0) as i32;
    let max_y = pts
        .iter()
        .map(|p| p.1)
        .fold(f64::NEG_INFINITY, f64::max)
        .ceil()
        .min(H as f64) as i32;
    for y in min_y..max_y {
        let yc = y as f64 + 0.5;
        let mut xs: Vec<f64> = Vec::new();
        for i in 0..pts.len() {
            let a0 = pts[i];
            let b0 = pts[(i + 1) % pts.len()];
            let (y0, y1) = (a0.1, b0.1);
            if (y0 <= yc && y1 > yc) || (y1 <= yc && y0 > yc) {
                let t = (yc - y0) / (y1 - y0);
                xs.push(a0.0 + t * (b0.0 - a0.0));
            }
        }
        xs.sort_by(|p, q| p.partial_cmp(q).unwrap());
        for pair in xs.chunks(2) {
            if let [x0, x1] = pair {
                for x in (x0.round() as i32)..=(x1.round() as i32) {
                    blend(img, x, y, c, a);
                }
            }
        }
    }
}
