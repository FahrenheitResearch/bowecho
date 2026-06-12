//! FARM live mobile-radar quicklooks: DOW/COW field deployments render
//! georeferenced PPI quicklooks every ~20 s to svr.guru (the FARM
//! facility's public live-data page). BowEcho probes the tiny index in
//! the background and lights a LIVE chip the moment a sensor starts
//! plotting; the window plays the live frame loop per product.
//!
//! Quicklooks courtesy of the FARM facility (Flexible Array of Radars
//! and Mesonets). Raw volumes are not public; when a deployment serves
//! a GR2A-style polled feed, the custom URL poller displays it natively
//! (nexrad_io decodes DORADE and Level-II mobile conversions directly).

use chrono::{DateTime, NaiveDateTime, Utc};
use eframe::egui;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const BASE: &str = "https://svr.guru";
pub const CREDIT: &str = "Live quicklooks courtesy of the FARM facility · svr.guru";
/// A sensor is LIVE when its newest plot is fresher than this (minutes).
const LIVE_WINDOW_MIN: i64 = 10;
/// Index probe cadence — the page is ~2 KB, the site's own viewer
/// reloads frames every 200 ms, so this is far politer than browsing.
const INDEX_PROBE_SECS: u64 = 120;
/// Live frame-list refresh while the window is open (plots land ~20 s).
const FRAMES_REFRESH_SECS: u64 = 20;

#[derive(Clone, Debug)]
pub struct FarmSensor {
    pub id: u32,
    pub name: String,
    /// The product the index page links to (varies per sensor: DOW7
    /// serves DBZHC, COW2 serves the filtered DBZHC_F).
    pub default_product: String,
    pub last_plot: Option<DateTime<Utc>>,
}

impl FarmSensor {
    pub fn is_live(&self) -> bool {
        self.last_plot
            .map(|t| (Utc::now() - t).num_minutes() < LIVE_WINDOW_MIN)
            .unwrap_or(false)
    }
}

/// Sensor entries on the index page: a data.php link with the sensor
/// name, followed by a "Last Plot: <span ...>STAMP UTC</span>" line.
pub fn parse_index(html: &str) -> Vec<FarmSensor> {
    let mut sensors: Vec<FarmSensor> = Vec::new();
    for chunk in html.split("data.php?id=").skip(1) {
        let Some(amp) = chunk.find("&prod=") else {
            continue;
        };
        let Ok(id) = chunk[..amp].parse::<u32>() else {
            continue;
        };
        let rest = &chunk[amp + 6..];
        let Some(quote) = rest.find('"') else {
            continue;
        };
        let product = rest[..quote].to_owned();
        let Some(gt) = rest.find('>') else {
            continue;
        };
        let Some(end) = rest[gt..].find("</a>") else {
            continue;
        };
        let name = rest[gt + 1..gt + end].trim().to_owned();
        let last_plot = rest.find("Last Plot:").and_then(|lp| {
            let tail = &rest[lp..];
            let open = tail.find('>')?;
            let close = tail.find("</span>")?;
            let stamp = tail
                .get(open + 1..close)?
                .trim()
                .trim_end_matches(" UTC")
                .trim();
            NaiveDateTime::parse_from_str(stamp, "%Y-%m-%d %H:%M:%S")
                .ok()
                .map(|t| t.and_utc())
        });
        if !name.is_empty() && !sensors.iter().any(|s| s.id == id) {
            sensors.push(FarmSensor {
                id,
                name,
                default_product: product,
                last_plot,
            });
        }
    }
    sensors
}

/// One sensor's data page: the frame loop (img srcs, oldest→newest)
/// plus the product links offered for that sensor.
pub struct FarmPage {
    pub frames: Vec<String>,
    pub products: Vec<String>,
}

pub fn parse_data_page(html: &str) -> FarmPage {
    let mut frames: Vec<String> = Vec::new();
    for chunk in html.split("src=\"").skip(1) {
        if let Some(quote) = chunk.find('"') {
            let path = &chunk[..quote];
            if path.starts_with("img/") && path.ends_with(".png") {
                let url = format!("{BASE}/{path}");
                if !frames.contains(&url) {
                    frames.push(url);
                }
            }
        }
    }
    let mut products: Vec<String> = Vec::new();
    for chunk in html.split("data.php?id=").skip(1) {
        if let Some(p) = chunk.find("&prod=")
            && let Some(quote) = chunk[p..].find('"')
        {
            let product = chunk[p + 6..p + quote].to_owned();
            if !product.is_empty() && !products.contains(&product) {
                products.push(product);
            }
        }
    }
    FarmPage { frames, products }
}

pub fn fetch_index() -> Result<Vec<FarmSensor>, String> {
    let text = data_source::fetch_text(&format!("{BASE}/index.php")).map_err(|e| e.to_string())?;
    let sensors = parse_index(&text);
    if sensors.is_empty() {
        return Err("no FARM sensors on the index".to_owned());
    }
    Ok(sensors)
}

pub fn fetch_data_page(id: u32, product: &str) -> Result<FarmPage, String> {
    let text = data_source::fetch_text(&format!("{BASE}/data.php?id={id}&prod={product}"))
        .map_err(|e| e.to_string())?;
    let page = parse_data_page(&text);
    if page.frames.is_empty() {
        return Err("no frames on the data page".to_owned());
    }
    Ok(page)
}

/// Frame valid time from the filename
/// (…COW2-PPI-DBZHC_F-20260611212522-14544.png → "21:25:22Z").
pub fn frame_label(url: &str) -> String {
    let stem = url.rsplit('/').next().unwrap_or(url);
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() >= 2 {
        let ts = parts[parts.len() - 2];
        if ts.len() == 14 && ts.bytes().all(|b| b.is_ascii_digit()) {
            return format!("{}:{}:{}Z", &ts[8..10], &ts[10..12], &ts[12..14]);
        }
    }
    String::new()
}

/// Friendly product names (DORADE/solo field-name conventions; the _F
/// suffix is the filtered channel).
pub fn product_label(slug: &str) -> String {
    let (base, filtered) = match slug.strip_suffix("_F") {
        Some(b) => (b, true),
        None => (slug, false),
    };
    let name = match base {
        "DBZHC" | "DBZ" => "Reflectivity",
        "VEL" => "Velocity",
        "ZDR" => "ZDR",
        "RHOHV" => "RhoHV",
        "KDP" => "KDP",
        "WIDTH" | "SW" => "Spectrum width",
        other => other,
    };
    if filtered {
        format!("{name} (filtered)")
    } else {
        name.to_owned()
    }
}

// ================= map drape: georeferenced quicklooks =================
//
// The quicklook PNGs are PPI plots in radar-centered Cartesian km space
// (equirectangular around the radar is exact to well under a gate width
// at the <=120 km plot scale; cf. Snyder 1987, "Map Projections — A
// Working Manual", USGS Professional Paper 1395, local plane sections).
// svr.guru publishes no coordinates (no PNG tEXt chunks, no JSON), so the
// georeference is recovered from the plot itself:
//
// * Plot frame: the figure background color (sampled in the left margin)
//   surrounds the map axes; the axes are the long non-background span of
//   the middle rows/columns (observed 105..=895 x 41..=839 on every
//   896x840 frame from both COW2 and DOW7 deployments).
// * Scale: the "Nkm ticks" lattice — white '+' marks every N km in data
//   space (observed 166 px / 30 km on COW2, 207.5 px / 15 km on DOW7).
// * Radar pixel: the lattice node nearest the echo-speckle centroid (the
//   scan disk is radar-centered; on every sampled frame this was the node
//   at (447, 419), which is also kept as a fallback anchor).
// * Absolute position: town markers — ~6 px solid white dots at place
//   locations (the same Census "place" class as BASEMAP_US_TOWN_LABELS).
//   Each (dot, town) pair votes for a radar origin in a lat/lon
//   accumulator (Hough-style voting: Ballard 1981, "Generalizing the
//   Hough transform to detect arbitrary shapes", Pattern Recognition
//   13(2), doi:10.1016/0031-3203(81)90009-1; the dot constellation acts
//   as a geometric-hash signature: Lamdan & Wolfson 1988, "Geometric
//   hashing: a general and efficient model-based recognition scheme",
//   Proc. ICCV, doi:10.1109/CCV.1988.589995). The top cell is verified by
//   nearest-town residuals. Tick spacing is auto-detected by running the
//   vote at candidate spacings — only the true scale verifies.
//
// Threshold calibration (live frames, COW2 2026-06-11 near Colchester IL
// + DOW7 2026-06-10 near Bethune CO): the true fix matched 42/42 dots at
// 0.47 km RMS; the densest false accumulator cell (NJ/PA town sprawl)
// managed 12/42 within 1.2 km at >0.75 km RMS. A 3-dot frame (DOW7, High
// Plains) verified at 461 distinct locations — hence the >=6 dot floor,
// with manual placement as the fallback.

/// Earth scale matching the app's spherical AEQD (111.32 km/deg).
const KM_PER_DEG: f64 = 111.32;
/// Accumulator cell size (deg). ~3.3 km — covers dot-centroid jitter.
const VOTE_BIN_DEG: f64 = 0.03;
/// Verification: nearest-town inlier radius (km).
const INLIER_KM: f64 = 1.2;
/// Verification: accepted fix must keep RMS under this (km).
const ACCEPT_RMS_KM: f64 = 0.7;
/// Minimum town dots for the constellation vote to be trustworthy.
const MIN_DOTS: usize = 6;
/// Tick spacings to try, most common first (label says "30km ticks" /
/// "15km ticks" on the observed deployments).
const TICK_TRY_ORDER: &[f64] = &[30.0, 15.0, 20.0, 10.0, 25.0, 50.0];
/// Tick choices offered for manual placement.
pub const TICK_CHOICES: &[f64] = &[10.0, 15.0, 20.0, 25.0, 30.0, 50.0];
/// Echo classification: min chroma (max-min RGB) for "colored echo".
const ECHO_CHROMA: u8 = 45;
/// Drape "echoes only" mask keeps pixels at least this chromatic.
const MASK_CHROMA: u8 = 35;
/// Cached drape frames (half-res CPU images; ~0.8 MB each).
const DRAPE_CACHE_FRAMES: usize = 48;
/// Fallback radar anchor when no echo is on the plot: every sampled
/// frame (two deployments, two image scales) put the radar at this pixel.
const EMPIRICAL_RADAR_PX: (f64, f64) = (447.0, 419.0);

/// Map-axes pixel rectangle inside the quicklook (inclusive bounds).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AxesRect {
    pub left: usize,
    pub top: usize,
    pub right: usize,
    pub bottom: usize,
}

/// Pixel-space plot geometry: enough to scale the image, but not to
/// place it on the globe.
#[derive(Clone, Debug)]
pub struct PlotGeometry {
    /// Radar pixel (lattice node nearest the echo centroid).
    pub radar_px: (f64, f64),
    /// Tick-lattice spacing in pixels (= one "Nkm ticks" interval).
    pub spacing_px: f64,
}

/// A full georeference: radar deployment lat/lon + image scale.
#[derive(Clone, Debug)]
pub struct GeoRef {
    pub lat: f64,
    pub lon: f64,
    pub px_per_km: f64,
    pub radar_px: (f64, f64),
    pub tick_km: f64,
    /// Scan id (last filename component) the fix was computed for; a new
    /// deployment gets a new id, which re-triggers auto-location.
    pub scan_id: String,
    pub manual: bool,
    /// Auto fix quality: matched town dots + residual RMS.
    pub matched_dots: usize,
    pub rms_km: f64,
}

impl GeoRef {
    pub fn to_entry(&self, sensor_id: u32) -> settings::FarmGeorefEntry {
        settings::FarmGeorefEntry {
            sensor_id,
            lat_e6: (self.lat * 1e6).round() as i64,
            lon_e6: (self.lon * 1e6).round() as i64,
            px_per_km_e3: (self.px_per_km * 1e3).round() as i64,
            radar_px_x_e1: (self.radar_px.0 * 10.0).round() as i64,
            radar_px_y_e1: (self.radar_px.1 * 10.0).round() as i64,
            tick_m: (self.tick_km * 1000.0).round() as u32,
            scan_id: self.scan_id.clone(),
            manual: self.manual,
        }
    }

    pub fn from_entry(entry: &settings::FarmGeorefEntry) -> Self {
        Self {
            lat: entry.lat_e6 as f64 / 1e6,
            lon: entry.lon_e6 as f64 / 1e6,
            px_per_km: entry.px_per_km_e3 as f64 / 1e3,
            radar_px: (
                entry.radar_px_x_e1 as f64 / 10.0,
                entry.radar_px_y_e1 as f64 / 10.0,
            ),
            tick_km: entry.tick_m as f64 / 1000.0,
            scan_id: entry.scan_id.clone(),
            manual: entry.manual,
            matched_dots: 0,
            rms_km: 0.0,
        }
    }
}

/// Scan id from a frame URL (…COW2-PPI-DBZHC_F-20260611220359-14544.png
/// → "14544"). Changes when the deployment restarts/moves.
pub fn scan_id_of(url: &str) -> String {
    let stem = url.rsplit('/').next().unwrap_or(url);
    let stem = stem.strip_suffix(".png").unwrap_or(stem);
    stem.rsplit('-').next().unwrap_or("").to_owned()
}

fn rgb_eq(a: egui::Color32, b: egui::Color32) -> bool {
    a.r() == b.r() && a.g() == b.g() && a.b() == b.b()
}

fn chroma(c: egui::Color32) -> u8 {
    let max = c.r().max(c.g()).max(c.b());
    let min = c.r().min(c.g()).min(c.b());
    max - min
}

fn is_pure_white(c: egui::Color32) -> bool {
    c.r() == 255 && c.g() == 255 && c.b() == 255
}

/// The map axes: long non-background spans of the middle rows/columns.
/// The figure background is sampled in the left margin at mid-height.
pub fn detect_axes(img: &egui::ColorImage) -> Option<AxesRect> {
    let [w, h] = img.size;
    if w < 300 || h < 300 {
        return None;
    }
    let bg = img.pixels[(h / 2) * w + 5];
    // Longest contiguous non-background span (>100 px segments merged:
    // basemap pixels occasionally equal the background color exactly).
    let span = |line: &mut dyn Iterator<Item = egui::Color32>| -> Option<(usize, usize)> {
        let mut best: Option<(usize, usize)> = None;
        let mut run_start: Option<usize> = None;
        for (index, color) in line.chain(std::iter::once(bg)).enumerate() {
            if !rgb_eq(color, bg) {
                run_start.get_or_insert(index);
            } else if let Some(start) = run_start.take()
                && index - start > 100
            {
                best = Some(match best {
                    Some((lo, hi)) => (lo.min(start), hi.max(index - 1)),
                    None => (start, index - 1),
                });
            }
        }
        best
    };
    let mut lefts = Vec::new();
    let mut rights = Vec::new();
    for y in [h / 4, h / 2, 3 * h / 4] {
        let row = &img.pixels[y * w..(y + 1) * w];
        if let Some((lo, hi)) = span(&mut row.iter().copied()) {
            lefts.push(lo);
            rights.push(hi);
        }
    }
    let mut tops = Vec::new();
    let mut bottoms = Vec::new();
    for x in [w / 4, w / 2, 3 * w / 4] {
        if let Some((lo, hi)) = span(&mut (0..h).map(|y| img.pixels[y * w + x])) {
            tops.push(lo);
            bottoms.push(hi);
        }
    }
    if lefts.len() < 2 || tops.len() < 2 {
        return None;
    }
    let median = |values: &mut Vec<usize>| {
        values.sort_unstable();
        values[values.len() / 2]
    };
    let rect = AxesRect {
        left: median(&mut lefts),
        top: median(&mut tops),
        right: median(&mut rights),
        bottom: median(&mut bottoms),
    };
    (rect.right > rect.left + w / 2 && rect.bottom > rect.top + h / 2).then_some(rect)
}

/// A connected component of pure-white pixels inside the axes.
struct Blob {
    x0: usize,
    x1: usize,
    y0: usize,
    y1: usize,
    area: usize,
    sum_x: f64,
    sum_y: f64,
}

impl Blob {
    fn center(&self) -> (f64, f64) {
        (self.sum_x / self.area as f64, self.sum_y / self.area as f64)
    }
    fn width(&self) -> usize {
        self.x1 - self.x0 + 1
    }
    fn height(&self) -> usize {
        self.y1 - self.y0 + 1
    }
}

/// 4-connected components of pure-white pixels inside the axes rect.
fn white_blobs(img: &egui::ColorImage, axes: AxesRect) -> Vec<Blob> {
    let [w, _h] = img.size;
    let mut visited = vec![false; img.pixels.len()];
    let mut blobs = Vec::new();
    let mut stack = Vec::new();
    for y in axes.top..=axes.bottom {
        for x in axes.left..=axes.right {
            let index = y * w + x;
            if visited[index] || !is_pure_white(img.pixels[index]) {
                continue;
            }
            visited[index] = true;
            stack.push((x, y));
            let mut blob = Blob {
                x0: x,
                x1: x,
                y0: y,
                y1: y,
                area: 0,
                sum_x: 0.0,
                sum_y: 0.0,
            };
            while let Some((bx, by)) = stack.pop() {
                blob.area += 1;
                blob.sum_x += bx as f64;
                blob.sum_y += by as f64;
                blob.x0 = blob.x0.min(bx);
                blob.x1 = blob.x1.max(bx);
                blob.y0 = blob.y0.min(by);
                blob.y1 = blob.y1.max(by);
                let neighbors = [
                    (bx.wrapping_sub(1), by),
                    (bx + 1, by),
                    (bx, by.wrapping_sub(1)),
                    (bx, by + 1),
                ];
                for (nx, ny) in neighbors {
                    if nx < axes.left || nx > axes.right || ny < axes.top || ny > axes.bottom {
                        continue;
                    }
                    let ni = ny * w + nx;
                    if !visited[ni] && is_pure_white(img.pixels[ni]) {
                        visited[ni] = true;
                        stack.push((nx, ny));
                    }
                }
            }
            if blob.area >= 8 && blob.area <= 400 {
                blobs.push(blob);
            }
        }
    }
    blobs
}

/// '+' lattice tick: 9-17 px square-ish cross. Cross signature: white
/// center and four arm tips, empty bbox corners AND quadrant centers —
/// the corner test alone let small text smudges through, which seeded
/// false lattice intervals (live-frame regression: spacing came out as
/// 166/3 px on a COW2 frame).
fn is_plus(img: &egui::ColorImage, blob: &Blob) -> bool {
    let (bw, bh) = (blob.width(), blob.height());
    if !(9..=17).contains(&bw) || !(9..=17).contains(&bh) || bw.abs_diff(bh) > 2 {
        return false;
    }
    if !(18..=90).contains(&blob.area) {
        return false;
    }
    let [w, _] = img.size;
    let white = |x: usize, y: usize| is_pure_white(img.pixels[y * w + x]);
    let (cx, cy) = ((blob.x0 + blob.x1) / 2, (blob.y0 + blob.y1) / 2);
    let (qx, qy) = (bw / 4, bh / 4);
    white(cx, cy)
        // arm tips
        && white(blob.x0, cy)
        && white(blob.x1, cy)
        && white(cx, blob.y0)
        && white(cx, blob.y1)
        // bbox corners
        && !white(blob.x0, blob.y0)
        && !white(blob.x1, blob.y0)
        && !white(blob.x0, blob.y1)
        && !white(blob.x1, blob.y1)
        // quadrant centers
        && !white(blob.x0 + qx, blob.y0 + qy)
        && !white(blob.x1 - qx, blob.y0 + qy)
        && !white(blob.x0 + qx, blob.y1 - qy)
        && !white(blob.x1 - qx, blob.y1 - qy)
}

/// Town marker: 4-9 px solid round dot (fill > 0.7 of the bbox;
/// calibrated on live frames: 6-8 px, area 38-56, zero text hits).
fn is_town_dot(blob: &Blob) -> bool {
    let (bw, bh) = (blob.width(), blob.height());
    (4..=9).contains(&bw)
        && (4..=9).contains(&bh)
        && bw.abs_diff(bh) <= 2
        && (20..=80).contains(&blob.area)
        && blob.area as f64 / (bw * bh) as f64 > 0.7
}

/// Cluster sorted 1-D coordinates within a 3 px tolerance.
fn cluster_1d(mut coords: Vec<f64>) -> Vec<f64> {
    coords.sort_by(|a, b| a.total_cmp(b));
    let mut clusters: Vec<(f64, usize)> = Vec::new();
    for value in coords {
        match clusters.last_mut() {
            Some((sum, n)) if (value - *sum / *n as f64).abs() <= 3.0 => {
                *sum += value;
                *n += 1;
            }
            _ => clusters.push((value, 1)),
        }
    }
    clusters
        .into_iter()
        .map(|(sum, n)| sum / n as f64)
        .collect()
}

/// Lattice spacing from consecutive cluster gaps. The base interval is
/// the MODE of the gaps (most repeated value; larger wins ties so a
/// stray short gap from any surviving false tick can't set the scale);
/// missing nodes then fold in as integer multiples of that base.
fn lattice_spacing(x_clusters: &[f64], y_clusters: &[f64]) -> Option<f64> {
    let mut diffs = Vec::new();
    for clusters in [x_clusters, y_clusters] {
        for pair in clusters.windows(2) {
            let d = pair[1] - pair[0];
            if (40.0..=420.0).contains(&d) {
                diffs.push(d);
            }
        }
    }
    let mut base: Option<(usize, f64)> = None; // (support, value)
    for &candidate in &diffs {
        let support = diffs
            .iter()
            .filter(|d| (**d - candidate).abs() <= 4.0)
            .count();
        let better = base
            .map(|(s, v)| support > s || (support == s && candidate > v))
            .unwrap_or(true);
        if better {
            base = Some((support, candidate));
        }
    }
    let (_, base) = base?;
    let (mut total, mut steps) = (0.0, 0.0);
    for d in diffs {
        let k = (d / base).round();
        if k >= 1.0 && (d - k * base).abs() <= 5.0 {
            total += d;
            steps += k;
        }
    }
    (steps > 0.0).then(|| total / steps)
}

/// Detect plot geometry + town-dot pixel positions in one pass.
pub fn analyze_plot(img: &egui::ColorImage) -> Result<(PlotGeometry, Vec<(f64, f64)>), String> {
    let axes = detect_axes(img).ok_or("plot frame not found")?;
    let blobs = white_blobs(img, axes);
    let mut plus_centers = Vec::new();
    let mut dots = Vec::new();
    for blob in &blobs {
        if is_plus(img, blob) {
            plus_centers.push(blob.center());
        } else if is_town_dot(blob) {
            dots.push(blob.center());
        }
    }
    let x_clusters = cluster_1d(plus_centers.iter().map(|p| p.0).collect());
    let y_clusters = cluster_1d(plus_centers.iter().map(|p| p.1).collect());
    let spacing =
        lattice_spacing(&x_clusters, &y_clusters).ok_or("tick lattice not visible yet")?;
    // Echo speckle centroid anchors the radar to a lattice node (the scan
    // disk is radar-centered). No echo at all → the empirical fixed pixel.
    let [w, _h] = img.size;
    let (mut count, mut sum_x, mut sum_y) = (0usize, 0.0f64, 0.0f64);
    for y in axes.top..=axes.bottom {
        for x in axes.left..=axes.right {
            if chroma(img.pixels[y * w + x]) >= ECHO_CHROMA {
                count += 1;
                sum_x += x as f64;
                sum_y += y as f64;
            }
        }
    }
    let anchor = if count >= 300 {
        (sum_x / count as f64, sum_y / count as f64)
    } else {
        EMPIRICAL_RADAR_PX
    };
    let snap = |clusters: &[f64], target: f64| -> f64 {
        let c0 = clusters[0];
        c0 + ((target - c0) / spacing).round() * spacing
    };
    let radar_px = (
        snap(
            if x_clusters.is_empty() {
                &y_clusters
            } else {
                &x_clusters
            },
            anchor.0,
        ),
        snap(
            if y_clusters.is_empty() {
                &x_clusters
            } else {
                &y_clusters
            },
            anchor.1,
        ),
    );
    Ok((
        PlotGeometry {
            radar_px,
            spacing_px: spacing,
        },
        dots,
    ))
}

/// Refine a candidate radar origin against the gazetteer: iterate
/// nearest-town matching + mean-residual shift on a SHRINKING inlier
/// radius — the vote-cell center starts up to ~3 km off truth, so the
/// first passes must capture matches loosely; the acceptance stats come
/// from the final INLIER_KM passes. Returns (lat, lon, inliers, rms_km).
fn refine_fix(
    dots: &[(f64, f64)],
    radar_px: (f64, f64),
    px_per_km: f64,
    mut lat: f64,
    mut lon: f64,
) -> Option<(f64, f64, usize, f64)> {
    let towns = crate::basemap_towns::BASEMAP_US_TOWN_LABELS;
    let mut result = None;
    for radius in [5.0, 3.0, 2.0, INLIER_KM, INLIER_KM, INLIER_KM] {
        let coslat = lat.to_radians().cos();
        let (mut n, mut sum_e, mut sum_n, mut sum_sq) = (0usize, 0.0, 0.0, 0.0);
        for &(px, py) in dots {
            let dot_lat = lat + (radar_px.1 - py) / px_per_km / KM_PER_DEG;
            let dot_lon = lon + (px - radar_px.0) / px_per_km / (KM_PER_DEG * coslat);
            let mut best = f64::INFINITY;
            let (mut best_e, mut best_n) = (0.0, 0.0);
            for town in towns {
                let dn = (town.lat as f64 - dot_lat) * KM_PER_DEG;
                if dn.abs() > radius + 0.5 {
                    continue;
                }
                let de = (town.lon as f64 - dot_lon) * KM_PER_DEG * coslat;
                let d2 = de * de + dn * dn;
                if d2 < best {
                    best = d2;
                    best_e = de;
                    best_n = dn;
                }
            }
            if best.sqrt() < radius {
                n += 1;
                sum_e += best_e;
                sum_n += best_n;
                sum_sq += best;
            }
        }
        if n < 4 {
            return None;
        }
        lat += sum_n / n as f64 / KM_PER_DEG;
        lon += sum_e / n as f64 / (KM_PER_DEG * coslat);
        result = Some((lat, lon, n, (sum_sq / n as f64).sqrt()));
    }
    result
}

/// Locate the deployment from the town-dot constellation. Tries each
/// candidate tick spacing; only the true scale survives verification.
pub fn locate_deployment(
    dots: &[(f64, f64)],
    geometry: &PlotGeometry,
    scan_id: &str,
) -> Result<GeoRef, String> {
    if dots.len() < MIN_DOTS {
        return Err(format!(
            "only {} town markers visible — place the radar manually",
            dots.len()
        ));
    }
    let towns = crate::basemap_towns::BASEMAP_US_TOWN_LABELS;
    let mut best: Option<GeoRef> = None;
    for &tick in TICK_TRY_ORDER {
        let px_per_km = geometry.spacing_px / tick;
        if !(1.5..=60.0).contains(&px_per_km) {
            continue;
        }
        // Hough-style vote: every (dot, town) pair proposes a radar
        // origin; bins are deduped per dot so a cell's count = number of
        // distinct dots agreeing (2x2 smear avoids cell-edge splits).
        let mut counts: HashMap<(i32, i32), u32> = HashMap::with_capacity(1 << 17);
        let mut seen: HashSet<(i32, i32)> = HashSet::with_capacity(1 << 14);
        for &(px, py) in dots {
            seen.clear();
            let dx_km = (px - geometry.radar_px.0) / px_per_km;
            let dy_km = (geometry.radar_px.1 - py) / px_per_km;
            for town in towns {
                let town_lat = town.lat as f64;
                let radar_lat = town_lat - dy_km / KM_PER_DEG;
                let radar_lon =
                    town.lon as f64 - dx_km / (KM_PER_DEG * town_lat.to_radians().cos());
                if !(20.0..55.0).contains(&radar_lat) || !(-130.0..-60.0).contains(&radar_lon) {
                    continue;
                }
                let i0 = (radar_lat / VOTE_BIN_DEG).floor() as i32;
                let j0 = (radar_lon / VOTE_BIN_DEG).floor() as i32;
                for cell in [(i0, j0), (i0 + 1, j0), (i0, j0 + 1), (i0 + 1, j0 + 1)] {
                    seen.insert(cell);
                }
            }
            for &cell in &seen {
                *counts.entry(cell).or_insert(0) += 1;
            }
        }
        let Some(votes) = counts.values().copied().max() else {
            continue;
        };
        if (votes as usize) < MIN_DOTS || (votes as usize) * 10 < dots.len() * 7 {
            continue;
        }
        // The 2x2 smear makes the true peak a small CLUSTER of tied-max
        // cells; their mean center starts the refinement well inside the
        // first capture radius. (Scattered ties = a non-peak — refine
        // simply fails to verify.)
        let (mut sum_lat, mut sum_lon, mut tied) = (0.0, 0.0, 0usize);
        for (&(bi, bj), &count) in &counts {
            if count == votes {
                sum_lat += (bi as f64 + 0.5) * VOTE_BIN_DEG;
                sum_lon += (bj as f64 + 0.5) * VOTE_BIN_DEG;
                tied += 1;
            }
        }
        let lat0 = sum_lat / tied as f64;
        let lon0 = sum_lon / tied as f64;
        let Some((lat, lon, inliers, rms)) =
            refine_fix(dots, geometry.radar_px, px_per_km, lat0, lon0)
        else {
            continue;
        };
        // Acceptance calibrated on live frames: true fix 42/42 @ 0.47 km;
        // densest false cell 12/42 @ >0.75 km.
        if inliers < MIN_DOTS.max(dots.len() * 3 / 5) || rms > ACCEPT_RMS_KM {
            continue;
        }
        let candidate = GeoRef {
            lat,
            lon,
            px_per_km,
            radar_px: geometry.radar_px,
            tick_km: tick,
            scan_id: scan_id.to_owned(),
            manual: false,
            matched_dots: inliers,
            rms_km: rms,
        };
        let better = best
            .as_ref()
            .map(|b| (inliers, -rms) > (b.matched_dots, -b.rms_km))
            .unwrap_or(true);
        if better {
            best = Some(candidate);
        }
        if inliers == dots.len() && rms < 0.6 {
            break; // perfect constellation match — no need to try other scales
        }
    }
    best.ok_or_else(|| {
        "no confident town-constellation match — try a reflectivity product or place manually"
            .to_owned()
    })
}

/// Full-frame analysis (background thread): geometry always, georef when
/// the constellation verifies.
pub struct AnalysisOutcome {
    pub scan_id: String,
    pub geometry: Option<PlotGeometry>,
    pub georef: Result<GeoRef, String>,
}

pub fn analyze_frame(img: &egui::ColorImage, scan_id: &str) -> AnalysisOutcome {
    match analyze_plot(img) {
        Ok((geometry, dots)) => {
            let georef = locate_deployment(&dots, &geometry, scan_id);
            AnalysisOutcome {
                scan_id: scan_id.to_owned(),
                geometry: Some(geometry),
                georef,
            }
        }
        Err(e) => AnalysisOutcome {
            scan_id: scan_id.to_owned(),
            geometry: None,
            georef: Err(e),
        },
    }
}

/// One cached drape frame: half-res crop of the axes area, in both a
/// plain variant and an "echoes only" variant (alpha from the fraction
/// of chromatic source pixels per 2x2 block).
struct DrapeImage {
    plain: egui::ColorImage,
    echoes: egui::ColorImage,
    crop_left: usize,
    crop_top: usize,
    /// Crop size in FULL-image pixels (even; = 2x the half-res size).
    full_w: usize,
    full_h: usize,
}

fn build_drape_image(img: &egui::ColorImage, axes: AxesRect) -> DrapeImage {
    let [w, _h] = img.size;
    let half_w = (axes.right - axes.left).div_ceil(2);
    let half_h = (axes.bottom - axes.top).div_ceil(2);
    let mut plain = vec![egui::Color32::TRANSPARENT; half_w * half_h];
    let mut echoes = vec![egui::Color32::TRANSPARENT; half_w * half_h];
    for oy in 0..half_h {
        for ox in 0..half_w {
            let (mut r, mut g, mut b) = (0u32, 0u32, 0u32);
            let (mut er, mut eg, mut eb, mut en) = (0u32, 0u32, 0u32, 0u32);
            for dy in 0..2 {
                for dx in 0..2 {
                    let sx = axes.left + ox * 2 + dx;
                    let sy = axes.top + oy * 2 + dy;
                    let c = img.pixels[sy * w + sx];
                    r += c.r() as u32;
                    g += c.g() as u32;
                    b += c.b() as u32;
                    if chroma(c) >= MASK_CHROMA {
                        er += c.r() as u32;
                        eg += c.g() as u32;
                        eb += c.b() as u32;
                        en += 1;
                    }
                }
            }
            let out = oy * half_w + ox;
            plain[out] = egui::Color32::from_rgb((r / 4) as u8, (g / 4) as u8, (b / 4) as u8);
            if en > 0 {
                echoes[out] = egui::Color32::from_rgba_unmultiplied(
                    (er / en) as u8,
                    (eg / en) as u8,
                    (eb / en) as u8,
                    (en * 255 / 4) as u8,
                );
            }
        }
    }
    let size = [half_w, half_h];
    let source_size = egui::vec2(half_w as f32, half_h as f32);
    DrapeImage {
        plain: egui::ColorImage {
            size,
            source_size,
            pixels: plain,
        },
        echoes: egui::ColorImage {
            size,
            source_size,
            pixels: echoes,
        },
        crop_left: axes.left,
        crop_top: axes.top,
        full_w: half_w * 2,
        full_h: half_h * 2,
    }
}

/// The currently uploaded drape texture (keyed so frame/mask changes
/// trigger a rebuild from the CPU cache).
struct DrapeTexture {
    url: String,
    echoes_only: bool,
    handle: egui::TextureHandle,
    crop_left: usize,
    crop_top: usize,
    full_w: usize,
    full_h: usize,
}

/// Map-drape state: georeference, frame cache, texture, placement mode.
pub struct DrapeState {
    pub enabled: bool,
    pub opacity: f32,
    pub echoes_only: bool,
    /// Armed: the next map right-click pins the radar location.
    pub place_armed: bool,
    /// Tick spacing assumed for manual placement (auto fixes detect it).
    pub manual_tick_km: f64,
    pub georef: Option<GeoRef>,
    pub geometry: Option<PlotGeometry>,
    pub status: String,
    saved: HashMap<u32, GeoRef>,
    dirty: bool,
    images: HashMap<String, DrapeImage>,
    order: VecDeque<String>,
    texture: Option<DrapeTexture>,
    analysis_rx: Option<mpsc::Receiver<AnalysisOutcome>>,
    /// Scan id we already auto-located (or failed on) — retried only on
    /// a new scan id or an explicit re-locate.
    attempted_scan: Option<String>,
}

impl Default for DrapeState {
    fn default() -> Self {
        Self {
            enabled: false,
            opacity: 0.85,
            echoes_only: true,
            place_armed: false,
            manual_tick_km: 30.0,
            georef: None,
            geometry: None,
            status: String::new(),
            saved: HashMap::new(),
            dirty: false,
            images: HashMap::new(),
            order: VecDeque::new(),
            texture: None,
            analysis_rx: None,
            attempted_scan: None,
        }
    }
}

impl DrapeState {
    pub fn from_entries(entries: &[settings::FarmGeorefEntry]) -> Self {
        let mut state = Self::default();
        for entry in entries {
            state
                .saved
                .insert(entry.sensor_id, GeoRef::from_entry(entry));
        }
        state
    }

    pub fn to_entries(&self) -> Vec<settings::FarmGeorefEntry> {
        let mut ids: Vec<u32> = self.saved.keys().copied().collect();
        ids.sort_unstable();
        ids.iter().map(|id| self.saved[id].to_entry(*id)).collect()
    }

    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// Switch sensors: clear the frame cache, drop any in-flight
    /// analysis (it belongs to the old sensor), restore the saved fix.
    fn on_sensor_change(&mut self, sensor_id: Option<u32>) {
        self.images.clear();
        self.order.clear();
        self.texture = None;
        self.geometry = None;
        self.attempted_scan = None;
        self.analysis_rx = None;
        self.place_armed = false;
        self.status = String::new();
        self.georef = sensor_id.and_then(|id| self.saved.get(&id)).cloned();
    }

    /// Forget the auto-locate attempt so the next frame re-runs it.
    pub fn force_relocate(&mut self) {
        self.attempted_scan = None;
        self.status = "re-locating from the next frame…".to_owned();
    }

    /// Pin the radar manually (map right-click while armed). Scale comes
    /// from the detected lattice + the user's tick choice; an existing
    /// auto fix keeps its own detected tick.
    pub fn place_radar(&mut self, sensor_id: Option<u32>, lat: f64, lon: f64, scan: String) {
        let (px_per_km, radar_px, tick_km) = if let Some(g) = &self.geometry {
            (
                g.spacing_px / self.manual_tick_km,
                g.radar_px,
                self.manual_tick_km,
            )
        } else if let Some(g) = &self.georef {
            (g.px_per_km, g.radar_px, g.tick_km)
        } else {
            self.status = "no frame analyzed yet — open a quicklook first".to_owned();
            return;
        };
        let fix = GeoRef {
            lat,
            lon,
            px_per_km,
            radar_px,
            tick_km,
            scan_id: scan,
            manual: true,
            matched_dots: 0,
            rms_km: 0.0,
        };
        self.adopt(fix, sensor_id);
        self.place_armed = false;
    }

    fn adopt(&mut self, fix: GeoRef, sensor_id: Option<u32>) {
        self.status = if fix.manual {
            format!("placed manually at {:.4}, {:.4}", fix.lat, fix.lon)
        } else {
            format!(
                "auto-located {:.4}, {:.4} · {} towns · RMS {:.2} km · {:.0} km ticks",
                fix.lat, fix.lon, fix.matched_dots, fix.rms_km, fix.tick_km
            )
        };
        if let Some(id) = sensor_id {
            self.saved.insert(id, fix.clone());
            self.dirty = true;
        }
        self.georef = Some(fix);
    }

    /// Per-frame ingest (image fetch thread already decoded the PNG):
    /// cache a drape crop and kick the auto-locator when needed.
    fn ingest(&mut self, url: &str, image: &egui::ColorImage) {
        if !self.enabled {
            return;
        }
        let scan = scan_id_of(url);
        if !self.images.contains_key(url)
            && let Some(axes) = detect_axes(image)
        {
            self.images
                .insert(url.to_owned(), build_drape_image(image, axes));
            self.order.push_back(url.to_owned());
            while self.order.len() > DRAPE_CACHE_FRAMES {
                if let Some(old) = self.order.pop_front() {
                    self.images.remove(&old);
                }
            }
        }
        let have_fix_for_scan = self
            .georef
            .as_ref()
            .map(|g| g.scan_id == scan)
            .unwrap_or(false);
        let already_tried = self.attempted_scan.as_deref() == Some(scan.as_str());
        if !have_fix_for_scan && !already_tried && self.analysis_rx.is_none() {
            self.attempted_scan = Some(scan.clone());
            self.status = "locating deployment…".to_owned();
            let (tx, rx) = mpsc::channel();
            self.analysis_rx = Some(rx);
            let image = image.clone();
            thread::spawn(move || {
                let _ = tx.send(analyze_frame(&image, &scan));
            });
        }
    }

    /// Drain analysis results + keep the drape texture current.
    fn pump(&mut self, ctx: &egui::Context, sensor_id: Option<u32>, current_url: Option<&str>) {
        if let Some(rx) = &self.analysis_rx {
            match rx.try_recv() {
                Ok(outcome) => {
                    self.analysis_rx = None;
                    if outcome.geometry.is_some() {
                        self.geometry = outcome.geometry;
                    }
                    match outcome.georef {
                        Ok(fix) => self.adopt(fix, sensor_id),
                        Err(e) => {
                            let stale = self
                                .georef
                                .as_ref()
                                .map(|g| g.scan_id != outcome.scan_id)
                                .unwrap_or(false);
                            self.status = if stale {
                                format!("{e} (showing previous fix — deployment may have moved)")
                            } else {
                                e
                            };
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => self.analysis_rx = None,
            }
        }
        if !self.enabled || self.georef.is_none() {
            return;
        }
        // Texture follows the playhead; if that frame isn't cached yet,
        // hold the newest cached one.
        let url = current_url
            .filter(|u| self.images.contains_key(*u))
            .map(str::to_owned)
            .or_else(|| self.order.back().cloned());
        let Some(url) = url else {
            return;
        };
        let fresh = self
            .texture
            .as_ref()
            .map(|t| t.url == url && t.echoes_only == self.echoes_only)
            .unwrap_or(false);
        if fresh {
            return;
        }
        let Some(di) = self.images.get(&url) else {
            return;
        };
        let image = if self.echoes_only {
            di.echoes.clone()
        } else {
            di.plain.clone()
        };
        let handle = ctx.load_texture("farm-drape", image, egui::TextureOptions::LINEAR);
        self.texture = Some(DrapeTexture {
            url,
            echoes_only: self.echoes_only,
            handle,
            crop_left: di.crop_left,
            crop_top: di.crop_top,
            full_w: di.full_w,
            full_h: di.full_h,
        });
    }

    /// Draw the drape on the radar map (call between the basemap/model
    /// underlays and the radar layer). The quicklook is radar-centered
    /// Cartesian km, so vertices map km offsets → lat/lon (equirect at
    /// the radar) → the app's AEQD screen projection; an 8x8 mesh keeps
    /// the two projections aligned across the tile at any zoom.
    pub fn draw(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        map_center_lat: f32,
        map_center_lon: f32,
        map_scale: f32,
    ) {
        if !self.enabled {
            return;
        }
        let (Some(fix), Some(tex)) = (&self.georef, &self.texture) else {
            return;
        };
        const GRID: usize = 8;
        let px_per_km_screen = map_scale / 111.32;
        let coslat = fix.lat.to_radians().cos();
        let mut positions = Vec::with_capacity((GRID + 1) * (GRID + 1));
        let mut visible = false;
        for j in 0..=GRID {
            for i in 0..=GRID {
                let u = i as f64 / GRID as f64;
                let v = j as f64 / GRID as f64;
                let img_x = tex.crop_left as f64 + u * tex.full_w as f64;
                let img_y = tex.crop_top as f64 + v * tex.full_h as f64;
                let dx_km = (img_x - fix.radar_px.0) / fix.px_per_km;
                let dy_km = (fix.radar_px.1 - img_y) / fix.px_per_km;
                let lat = fix.lat + dy_km / KM_PER_DEG;
                let lon = fix.lon + dx_km / (KM_PER_DEG * coslat);
                let (east, north) =
                    crate::aeqd_forward_km(map_center_lat as f64, map_center_lon as f64, lat, lon);
                let pos = egui::pos2(
                    rect.center().x + east as f32 * px_per_km_screen,
                    rect.center().y - north as f32 * px_per_km_screen,
                );
                visible |= rect.contains(pos);
                positions.push((pos, egui::pos2(u as f32, v as f32)));
            }
        }
        // Cheap cull: also visible when vertices straddle the viewport.
        let xs = positions.iter().map(|(p, _)| p.x);
        let straddles = xs.clone().any(|x| x < rect.right())
            && positions.iter().any(|(p, _)| p.x > rect.left())
            && positions.iter().any(|(p, _)| p.y < rect.bottom())
            && positions.iter().any(|(p, _)| p.y > rect.top());
        if !visible && !straddles {
            return;
        }
        let tint = egui::Color32::from_white_alpha((self.opacity * 255.0) as u8);
        let mut mesh = egui::epaint::Mesh::with_texture(tex.handle.id());
        for (pos, uv) in &positions {
            mesh.vertices.push(egui::epaint::Vertex {
                pos: *pos,
                uv: *uv,
                color: tint,
            });
        }
        for j in 0..GRID {
            for i in 0..GRID {
                let v0 = (j * (GRID + 1) + i) as u32;
                let v1 = v0 + 1;
                let v2 = v0 + (GRID + 1) as u32;
                let v3 = v2 + 1;
                mesh.indices.extend_from_slice(&[v0, v1, v2, v1, v3, v2]);
            }
        }
        painter.add(egui::Shape::mesh(mesh));
        // FARM credit stays visible whenever the drape is on the map.
        let anchor = positions[(GRID + 1) * GRID].0; // bottom-left vertex
        painter.text(
            anchor + egui::vec2(2.0, 2.0),
            egui::Align2::LEFT_TOP,
            "FARM quicklook · svr.guru",
            egui::FontId::proportional(10.0),
            egui::Color32::from_white_alpha(150),
        );
    }
}

pub struct FarmState {
    pub open: bool,
    pub sensors: Vec<FarmSensor>,
    index_rx: Option<mpsc::Receiver<Result<Vec<FarmSensor>, String>>>,
    last_index_probe: Option<Instant>,
    pub sensor_id: Option<u32>,
    pub product: String,
    pub frames: Vec<String>,
    pub products: Vec<String>,
    frames_rx: Option<mpsc::Receiver<Result<FarmPage, String>>>,
    last_frames_fetch: Option<Instant>,
    pub frame_index: usize,
    pub playing: bool,
    /// Snap to the newest frame whenever new plots arrive.
    pub follow_live: bool,
    last_advance: Instant,
    pub textures: HashMap<String, egui::TextureHandle>,
    image_rx: Option<mpsc::Receiver<(String, Result<egui::ColorImage, String>)>>,
    pending: Vec<String>,
    pub status: String,
    /// Georeferenced map drape ("Show on map").
    pub drape: DrapeState,
}

impl Default for FarmState {
    fn default() -> Self {
        Self {
            open: false,
            sensors: Vec::new(),
            index_rx: None,
            last_index_probe: None,
            sensor_id: None,
            product: String::new(),
            frames: Vec::new(),
            products: Vec::new(),
            frames_rx: None,
            last_frames_fetch: None,
            frame_index: 0,
            playing: true,
            follow_live: true,
            last_advance: Instant::now(),
            textures: HashMap::new(),
            image_rx: None,
            pending: Vec::new(),
            status: String::new(),
            drape: DrapeState::default(),
        }
    }
}

impl FarmState {
    /// Construct with georeferences restored from settings.
    pub fn with_saved(entries: &[settings::FarmGeorefEntry]) -> Self {
        Self {
            drape: DrapeState::from_entries(entries),
            ..Self::default()
        }
    }

    pub fn live_sensor(&self) -> Option<&FarmSensor> {
        self.sensors.iter().find(|s| s.is_live())
    }

    /// Scan id of the currently displayed frame.
    pub fn current_scan_id(&self) -> String {
        self.frames
            .get(self.frame_index)
            .map(|url| scan_id_of(url))
            .unwrap_or_default()
    }

    /// Push the displayed frame back through the fetch pipeline so its
    /// PIXELS reach the drape ingest — the window cache holds GPU
    /// handles only, and a stale loop (no new plots arriving) would
    /// otherwise never produce a drape image after "Show on map" is
    /// switched on.
    pub fn kickstart_drape(&mut self) {
        if let Some(url) = self.frames.get(self.frame_index) {
            self.textures.remove(url);
        }
    }

    /// Draw the georeferenced quicklook drape on the radar map.
    pub fn draw_on_map(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        map_center_lat: f32,
        map_center_lon: f32,
        map_scale: f32,
    ) {
        self.drape
            .draw(painter, rect, map_center_lat, map_center_lon, map_scale);
    }

    /// Background index probe — runs even with the window closed so the
    /// LIVE chip appears the moment a deployment starts plotting.
    pub fn pump_index(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.index_rx {
            match rx.try_recv() {
                Ok(Ok(sensors)) => {
                    self.index_rx = None;
                    self.sensors = sensors;
                }
                Ok(Err(_)) => self.index_rx = None,
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => self.index_rx = None,
            }
        }
        let due = self
            .last_index_probe
            .map(|at| at.elapsed().as_secs() > INDEX_PROBE_SECS)
            .unwrap_or(true);
        if due && self.index_rx.is_none() {
            self.last_index_probe = Some(Instant::now());
            let (tx, rx) = mpsc::channel();
            self.index_rx = Some(rx);
            let ctx_clone = ctx.clone();
            thread::spawn(move || {
                let result = fetch_index();
                let _ = tx.send(result);
                ctx_clone.request_repaint();
            });
        }
    }

    /// Window-open work: frame-list refresh, texture fetches, playback.
    pub fn pump_window(&mut self, ctx: &egui::Context) {
        // Default selection: the live sensor, else the first.
        if self.sensor_id.is_none() {
            let pick = self
                .live_sensor()
                .or(self.sensors.first())
                .map(|s| (s.id, s.default_product.clone()));
            if let Some((id, product)) = pick {
                self.sensor_id = Some(id);
                self.product = product;
                self.drape.on_sensor_change(Some(id));
            }
        }
        let Some(id) = self.sensor_id else {
            return;
        };
        if let Some(rx) = &self.frames_rx {
            match rx.try_recv() {
                Ok(Ok(page)) => {
                    self.frames_rx = None;
                    let grew = page.frames.len() != self.frames.len()
                        || page.frames.last() != self.frames.last();
                    self.frames = page.frames;
                    self.products = page.products;
                    if self.frame_index >= self.frames.len() || (grew && self.follow_live) {
                        self.frame_index = self.frames.len().saturating_sub(1);
                    }
                    self.status = format!("{} frames", self.frames.len());
                }
                Ok(Err(e)) => {
                    self.frames_rx = None;
                    self.status = format!("FARM: {e}");
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => self.frames_rx = None,
            }
        }
        let due = self
            .last_frames_fetch
            .map(|at| at.elapsed().as_secs() > FRAMES_REFRESH_SECS)
            .unwrap_or(true);
        if due && self.frames_rx.is_none() && !self.product.is_empty() {
            self.last_frames_fetch = Some(Instant::now());
            let (tx, rx) = mpsc::channel();
            self.frames_rx = Some(rx);
            let product = self.product.clone();
            let ctx_clone = ctx.clone();
            thread::spawn(move || {
                let result = fetch_data_page(id, &product);
                let _ = tx.send(result);
                ctx_clone.request_repaint();
            });
        }
        // Texture pipeline: one fetch in flight, current frame first,
        // then neighbors so the loop fills in around the playhead.
        if let Some(rx) = &self.image_rx {
            match rx.try_recv() {
                Ok((url, Ok(image))) => {
                    self.image_rx = None;
                    self.pending.retain(|u| u != &url);
                    // Evict frames that left the loop (don't clear-all:
                    // that refetches the whole loop every eviction).
                    if self.textures.len() > 120 {
                        let keep: Vec<String> = self.frames.clone();
                        self.textures.retain(|k, _| keep.contains(k));
                    }
                    self.drape.ingest(&url, &image);
                    let handle = ctx.load_texture(url.clone(), image, egui::TextureOptions::LINEAR);
                    self.textures.insert(url, handle);
                }
                Ok((url, Err(e))) => {
                    self.image_rx = None;
                    self.pending.retain(|u| u != &url);
                    let mut msg = e;
                    msg.truncate(90);
                    self.status = format!("FARM: {msg}");
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => self.image_rx = None,
            }
        }
        if self.image_rx.is_none()
            && let Some(url) = self.next_wanted_url()
        {
            self.pending.push(url.clone());
            let (tx, rx) = mpsc::channel();
            self.image_rx = Some(rx);
            let ctx_clone = ctx.clone();
            thread::spawn(move || {
                let result = crate::wofs::fetch_image(&url);
                let _ = tx.send((url, result));
                ctx_clone.request_repaint();
            });
        }
        // Map drape upkeep: drain the locator, follow the playhead.
        let current_url = self.frames.get(self.frame_index).cloned();
        self.drape.pump(ctx, self.sensor_id, current_url.as_deref());
        // Playback over LOADED frames only (skipping holes would jitter
        // the loop; holding until the texture lands reads better).
        if self.playing && self.frames.len() > 1 {
            if self.last_advance.elapsed() > Duration::from_millis(180) {
                let next = (self.frame_index + 1) % self.frames.len();
                if self.textures.contains_key(&self.frames[next]) {
                    self.frame_index = next;
                    self.last_advance = Instant::now();
                }
            }
            ctx.request_repaint_after(Duration::from_millis(60));
        }
    }

    /// Next texture to fetch: the current frame, then outward from the
    /// playhead.
    fn next_wanted_url(&self) -> Option<String> {
        let n = self.frames.len();
        if n == 0 {
            return None;
        }
        let wanted = std::iter::once(self.frame_index)
            .chain(
                (1..n).flat_map(|d| [(self.frame_index + d) % n, (self.frame_index + n - d) % n]),
            )
            .take(2 * n);
        for index in wanted {
            let url = &self.frames[index];
            if !self.textures.contains_key(url) && !self.pending.contains(url) {
                return Some(url.clone());
            }
        }
        None
    }

    /// Select a sensor (resets the loop to its default product).
    pub fn select_sensor(&mut self, id: u32) {
        if self.sensor_id == Some(id) {
            return;
        }
        self.sensor_id = Some(id);
        if let Some(sensor) = self.sensors.iter().find(|s| s.id == id) {
            self.product = sensor.default_product.clone();
        }
        self.frames.clear();
        self.products.clear();
        self.frame_index = 0;
        self.last_frames_fetch = None;
        self.drape.on_sensor_change(Some(id));
    }

    /// Select a product for the current sensor.
    pub fn select_product(&mut self, product: &str) {
        if self.product == product {
            return;
        }
        self.product = product.to_owned();
        self.frames.clear();
        self.frame_index = 0;
        self.last_frames_fetch = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INDEX_FIXTURE: &str = r#"<p class="medium"><a href="data.php?id=4&prod=DBZHC">DOW7</a></p><p class="tiny">Last Plot: <span style="color: white; padding: 2px;">2026-06-10 20:51:17 UTC</span></p><br/><p class="medium"><a href="data.php?id=9&prod=DBZHC_F">COW2</a></p><p class="tiny">Last Plot: <span style="color: black; background: #77FF77; padding: 2px;">2026-06-11 21:25:36 UTC</span></p><br/>"#;

    #[test]
    fn index_parses_sensors_and_stamps() {
        let sensors = parse_index(INDEX_FIXTURE);
        assert_eq!(sensors.len(), 2);
        assert_eq!(sensors[0].name, "DOW7");
        assert_eq!(sensors[0].id, 4);
        assert_eq!(sensors[0].default_product, "DBZHC");
        assert_eq!(sensors[1].name, "COW2");
        assert_eq!(
            sensors[1].last_plot.unwrap().to_rfc3339(),
            "2026-06-11T21:25:36+00:00"
        );
    }

    #[test]
    fn data_page_parses_frames_and_products() {
        let html = r#"<img id="0-0" src="img/9/COW2-PPI-DBZHC_F-20260611211857-14544.png">
<img id="0-1" src="img/9/COW2-PPI-DBZHC_F-20260611211857-14544.png">
<img id="0-2" src="img/9/COW2-PPI-DBZHC_F-20260611211919-14544.png">
<a href="data.php?id=9&prod=DBZHC_F">Z</a><a href="data.php?id=9&prod=VEL_F">V</a>"#;
        let page = parse_data_page(html);
        // duplicate preload frame deduped
        assert_eq!(page.frames.len(), 2);
        assert!(page.frames[0].starts_with("https://svr.guru/img/9/"));
        assert_eq!(page.products, vec!["DBZHC_F", "VEL_F"]);
    }

    #[test]
    fn frame_labels_and_product_names() {
        assert_eq!(
            frame_label("https://svr.guru/img/9/COW2-PPI-DBZHC_F-20260611212522-14544.png"),
            "21:25:22Z"
        );
        assert_eq!(product_label("DBZHC_F"), "Reflectivity (filtered)");
        assert_eq!(product_label("VEL_F"), "Velocity (filtered)");
        assert_eq!(product_label("DBZHC"), "Reflectivity");
    }

    #[test]
    fn scan_id_from_url() {
        assert_eq!(
            scan_id_of("https://svr.guru/img/9/COW2-PPI-DBZHC_F-20260611220359-14544.png"),
            "14544"
        );
        assert_eq!(scan_id_of("weird"), "weird");
    }

    #[test]
    fn georef_entry_roundtrip() {
        let fix = GeoRef {
            lat: 40.5252,
            lon: -90.7931,
            px_per_km: 166.0 / 30.0,
            radar_px: (447.0, 419.0),
            tick_km: 30.0,
            scan_id: "14544".to_owned(),
            manual: false,
            matched_dots: 42,
            rms_km: 0.47,
        };
        let back = GeoRef::from_entry(&fix.to_entry(9));
        assert!((back.lat - fix.lat).abs() < 1e-5);
        assert!((back.lon - fix.lon).abs() < 1e-5);
        assert!((back.px_per_km - fix.px_per_km).abs() < 1e-2);
        assert_eq!(back.radar_px, fix.radar_px);
        assert_eq!(back.tick_km, fix.tick_km);
        assert_eq!(back.scan_id, fix.scan_id);
        assert!(!back.manual);
    }

    // -------- synthetic quicklook (mirrors live COW2 frame layout) --------

    /// Layout constants measured on live frames (see the module-level
    /// drape notes): 896x840 figure, axes 105..=895 x 41..=839, figure
    /// background (45,38,33), basemap land (90,75,65).
    const SYN_W: usize = 896;
    const SYN_H: usize = 840;
    const SYN_AXES: AxesRect = AxesRect {
        left: 105,
        top: 41,
        right: 895,
        bottom: 839,
    };
    const SYN_RADAR: (f64, f64) = (447.0, 419.0);
    const SYN_SPACING: f64 = 166.0; // 30 km ticks
    const SYN_LAT: f64 = 40.5252;
    const SYN_LON: f64 = -90.7931;

    fn synth_quicklook() -> (egui::ColorImage, Vec<(f64, f64)>) {
        let bg = egui::Color32::from_rgb(45, 38, 33);
        let land = egui::Color32::from_rgb(90, 75, 65);
        let white = egui::Color32::WHITE;
        let mut pixels = vec![bg; SYN_W * SYN_H];
        for y in SYN_AXES.top..=SYN_AXES.bottom {
            for x in SYN_AXES.left..=SYN_AXES.right {
                pixels[y * SYN_W + x] = land;
            }
        }
        // Echo speckle disk (deterministic LCG), radius 330 px ≈ 60 km.
        let mut seed: u64 = 0x2545F491_4F6CDD1D;
        for _ in 0..6000 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let dx = ((seed >> 20) % 661) as f64 - 330.0;
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let dy = ((seed >> 20) % 661) as f64 - 330.0;
            if dx * dx + dy * dy > 330.0 * 330.0 {
                continue;
            }
            let x = (SYN_RADAR.0 + dx) as usize;
            let y = (SYN_RADAR.1 + dy) as usize;
            pixels[y * SYN_W + x] = egui::Color32::from_rgb(30, 140, 40);
        }
        // '+' tick lattice (12x12, 4 px arms — the live marker raster).
        for gy in -2i64..=2 {
            for gx in -2i64..=2 {
                let cx = (SYN_RADAR.0 + gx as f64 * SYN_SPACING) as i64;
                let cy = (SYN_RADAR.1 + gy as f64 * SYN_SPACING) as i64;
                for dy in -6i64..6 {
                    for dx in -6i64..6 {
                        let horizontal = (-2..2).contains(&dy);
                        let vertical = (-2..2).contains(&dx);
                        if horizontal || vertical {
                            let (x, y) = ((cx + dx) as usize, (cy + dy) as usize);
                            if (SYN_AXES.left..=SYN_AXES.right).contains(&x)
                                && (SYN_AXES.top..=SYN_AXES.bottom).contains(&y)
                            {
                                pixels[y * SYN_W + x] = white;
                            }
                        }
                    }
                }
            }
        }
        // Town dots at real gazetteer positions around the chosen origin
        // (6x6 solid white squares — same blob signature as the live 'o'
        // markers). px_per_km mirrors the live 166 px / 30 km scale.
        let px_per_km = SYN_SPACING / 30.0;
        let coslat = SYN_LAT.to_radians().cos();
        let mut dots = Vec::new();
        for town in crate::basemap_towns::BASEMAP_US_TOWN_LABELS {
            let dx_km = (town.lon as f64 - SYN_LON) * KM_PER_DEG * coslat;
            let dy_km = (town.lat as f64 - SYN_LAT) * KM_PER_DEG;
            if dx_km.abs() > 55.0 || dy_km.abs() > 55.0 {
                continue;
            }
            let px = SYN_RADAR.0 + dx_km * px_per_km;
            let py = SYN_RADAR.1 - dy_km * px_per_km;
            // Keep clear of the tick lattice so blobs don't merge.
            let near_tick = |v: f64, origin: f64| {
                let m = (v - origin).rem_euclid(SYN_SPACING);
                !(14.0..=SYN_SPACING - 14.0).contains(&m)
            };
            if near_tick(px, SYN_RADAR.0) && near_tick(py, SYN_RADAR.1) {
                continue;
            }
            let (x0, y0) = (px.round() as usize, py.round() as usize);
            for dy in 0..6usize {
                for dx in 0..6usize {
                    pixels[(y0 + dy) * SYN_W + (x0 + dx)] = white;
                }
            }
            dots.push((px, py));
        }
        let image = egui::ColorImage {
            size: [SYN_W, SYN_H],
            source_size: egui::vec2(SYN_W as f32, SYN_H as f32),
            pixels,
        };
        (image, dots)
    }

    #[test]
    fn synthetic_axes_detected() {
        let (image, _) = synth_quicklook();
        assert_eq!(detect_axes(&image), Some(SYN_AXES));
    }

    #[test]
    fn synthetic_geometry_and_georef_recovered() {
        let (image, dots) = synth_quicklook();
        assert!(
            dots.len() >= MIN_DOTS,
            "gazetteer must seed at least {MIN_DOTS} towns near the test origin (got {})",
            dots.len()
        );
        let (geometry, found_dots) = analyze_plot(&image).expect("plot analysis");
        assert!(
            (geometry.spacing_px - SYN_SPACING).abs() < 1.5,
            "spacing {} != {SYN_SPACING}",
            geometry.spacing_px
        );
        assert!(
            (geometry.radar_px.0 - SYN_RADAR.0).abs() < 3.0
                && (geometry.radar_px.1 - SYN_RADAR.1).abs() < 3.0,
            "radar px {:?}",
            geometry.radar_px
        );
        assert!(found_dots.len() >= dots.len().saturating_sub(2));
        let fix = locate_deployment(&found_dots, &geometry, "14544").expect("georef");
        assert_eq!(fix.tick_km, 30.0, "auto-detected tick spacing");
        assert!(
            (fix.lat - SYN_LAT).abs() < 0.01 && (fix.lon - SYN_LON).abs() < 0.013,
            "fix at {:.4},{:.4} (truth {SYN_LAT},{SYN_LON})",
            fix.lat,
            fix.lon
        );
        assert!(fix.matched_dots >= MIN_DOTS);
        assert!(fix.rms_km < ACCEPT_RMS_KM);
    }

    #[test]
    fn sparse_constellation_is_rejected() {
        // 3 dots verified at 461 distinct CONUS locations in the offline
        // calibration — the locator must refuse to guess.
        let (image, _) = synth_quicklook();
        let (geometry, found_dots) = analyze_plot(&image).expect("plot analysis");
        let sparse: Vec<(f64, f64)> = found_dots.into_iter().take(3).collect();
        assert!(locate_deployment(&sparse, &geometry, "x").is_err());
    }
}
