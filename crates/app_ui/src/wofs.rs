//! WoFS (Warn-on-Forecast System) viewer: NSSL's experimental rapid-
//! cycling ensemble, browsed inside BowEcho with radar-time sync.
//!
//! Data: the CB-WoFS public endpoints — the imagery CDN serves PNGs
//! anonymously (CORS *), and the /Forecast JSON API (runs, products,
//! latest) is likewise anonymous. Per the system's own published terms:
//! "WoFS data courtesy of the National Severe Storms Laboratory using
//! federal funding" — that acknowledgement renders permanently in the
//! window.
//!
//! v1 shows the product PNGs (900x800, palette; *_overlay_* products are
//! transparent stackables) in a window with run/product/forecast-minute
//! controls and a sync-to-radar-frame action. "Show on map" drapes the
//! current product onto the radar map: the endpoints expose no projection
//! metadata anonymously, so the georeference is recovered per run by
//! OCR-ing sounding-PNG titles (see `wofs_georef`).
//!
//! Soundings mode overlays the 20x20 sounding-station lattice on the
//! product image; clicking a station opens its ensemble skew-T PNG with
//! prev/next frame stepping. WoFS research basis: Stensrud et al. (2009,
//! Bull. Amer. Meteor. Soc., doi:10.1175/2009BAMS2795.1) — the
//! Warn-on-Forecast vision — and Skinner et al. (2018, Wea. Forecasting,
//! doi:10.1175/WAF-D-18-0020.1) — the prototype WoFS this viewer serves.

use crate::wofs_georef::{self, WofsGeoref};
use eframe::egui;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Instant;

const API: &str = "https://cbwofs.vlab.noaa.gov/Forecast";
const CDN: &str = "https://ep-wofs-postv2-dndma2fqexfhexfs.a01.azurefd.net/primary";
/// Per-station ensemble skew-T PNG service (anonymous, like the imagery CDN).
const SND_CDN: &str = "https://ep-wofs-sounding-etb5awe5cdfqawe8.a01.azurefd.net/api/sounding";
/// The stations endpoint and the sounding frame grid key off this product.
const SND_REF_PRODUCT: &str = "comp_dz__paintballs_thresh_40";
/// The run endpoint retains a much deeper and occasionally stale history than
/// the image CDN.  Keep the picker compact and, more importantly, prove every
/// exposed run has at least one posted analysis image before presenting it as
/// loadable.
const MAX_PICKER_RUNS: usize = 20;
pub const CREDIT: &str =
    "WoFS data courtesy of the National Severe Storms Laboratory using federal funding";

/// Plot axes box (x0, y0, x1, y1) in pixels inside the 900x800 product PNG.
/// Station lattice fractions map onto THIS box, not the full image, with y
/// measured from the BOTTOM. Verified visually 2026-06-11 against live run
/// WOFSRun20260611-144912d1 ("Midwest"): mapped to the full image, col-20
/// dots (x=0.962) land at ~866 px — past the box's right edge into the
/// whitespace margin — while the axes-box mapping yields a square, centered
/// lattice. Corner sounding titles (01_01 = 37.42N 94.03W SW, 20_20 =
/// 44.81N 84.16W NE) confirm row 01 = south = image bottom.
pub const AXES_BOX: (f32, f32, f32, f32) = (12.0, 43.0, 759.0, 790.0);
/// Product PNG pixel dimensions (the axes box sits inside this canvas).
pub const PRODUCT_PNG_SIZE: (f32, f32) = (900.0, 800.0);

/// WoFS product PNGs deliberately encode much of their guidance with low
/// source alpha (paintball pixels commonly arrive around 15% opacity). The
/// map also paints WoFS below the observed-radar rasters, so a single 70%
/// pass was easy to lose. High visibility repeats the exact same textured
/// mesh: alpha coverage increases, while fully transparent pixels, RGB
/// palette values, UVs, and georeferencing remain untouched.
const HIGH_VISIBILITY_PASSES: usize = 2;
const DEFAULT_DRAPE_OPACITY: f32 = 0.85;

#[derive(Clone)]
pub struct WofsRun {
    pub id: String,
    pub name: String,
    pub rundate: String,
    /// Exact init cycles as `YYYYMMDDHHMM`, newest first. Keeping the date is
    /// essential: many WoFS runs cross midnight, and the imagery/sounding
    /// services key their paths by the init's actual UTC date rather than the
    /// run's nominal `rundate`.
    pub inits: Vec<String>,
}

fn canonical_init_token(rundate: &str, init: &str) -> String {
    let trimmed = init.trim();
    if trimmed.len() == 12 && trimmed.bytes().all(|byte| byte.is_ascii_digit()) {
        trimmed.to_owned()
    } else {
        format!("{rundate}{}", init_hhmm(trimmed))
    }
}

pub fn init_hhmm(init: &str) -> &str {
    init.get(init.len().saturating_sub(4)..).unwrap_or(init)
}

fn init_date_and_hhmm(rundate: &str, init: &str) -> (String, String) {
    let token = canonical_init_token(rundate, init);
    (token[..8].to_owned(), token[8..].to_owned())
}

pub fn init_time_utc(run: &WofsRun, init: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::NaiveDateTime::parse_from_str(&canonical_init_token(&run.rundate, init), "%Y%m%d%H%M")
        .ok()
        .map(|time| time.and_utc())
}

#[derive(Clone)]
pub struct WofsCatalog {
    pub runs: Vec<WofsRun>,
    /// Menu tree from the `hierarchy` endpoint: (group, slugs).
    pub groups: Vec<(String, Vec<String>)>,
    /// Per-product valid times in SECONDS from `products?metadata=true`
    /// — each product has its own grid; never guess minutes.
    pub times: HashMap<String, Vec<u32>>,
}

/// Fetch the run list + product groups (blocking; worker thread).
pub fn fetch_catalog() -> Result<WofsCatalog, String> {
    let runs_text =
        data_source::fetch_text(&format!("{API}/runs")).map_err(|e| format!("runs: {e}"))?;
    let root: serde_json::Value = serde_json::from_str(&runs_text).map_err(|e| e.to_string())?;
    let mut runs: Vec<WofsRun> = Vec::new();
    if let Some(map) = root.as_object() {
        for (_date, entries) in map {
            if let Some(list) = entries.as_array() {
                for entry in list {
                    let id = entry["id"].as_str().unwrap_or("").to_owned();
                    if id.is_empty() {
                        continue;
                    }
                    let inits: Vec<String> = entry["times"]
                        .as_array()
                        .map(|t| {
                            t.iter()
                                .filter_map(|v| v.as_str())
                                .filter(|rt| {
                                    rt.len() == 12 && rt.bytes().all(|byte| byte.is_ascii_digit())
                                })
                                .map(ToOwned::to_owned)
                                .collect()
                        })
                        .unwrap_or_default();
                    runs.push(WofsRun {
                        id,
                        name: entry["name"].as_str().unwrap_or("").to_owned(),
                        rundate: entry["rundate"].as_str().unwrap_or("").to_owned(),
                        inits,
                    });
                }
            }
        }
    }
    runs.sort_by(|a, b| b.rundate.cmp(&a.rundate).then(b.id.cmp(&a.id)));
    if runs.is_empty() {
        return Err("no WoFS runs".to_owned());
    }
    // The API advertises live cycles before the CDN posts their imagery, and
    // it also retains stale run records after their files disappear.  The old
    // code checked only the first run and still kept its final 404 cycle.  A
    // cheap HEAD against f000 makes every picker entry an actually loadable
    // run.  Once one cycle is posted, older cycles in that run are monotonic
    // on this archive and can remain available without hundreds of probes.
    let mut posted_runs = Vec::with_capacity(MAX_PICKER_RUNS);
    for run in runs {
        let checked = retain_posted_inits(run, |url| {
            data_source::url_exists(url).map_err(|error| error.to_string())
        })
        .map_err(|error| format!("availability check: {error}"))?;
        if let Some(run) = checked {
            posted_runs.push(run);
            if posted_runs.len() == MAX_PICKER_RUNS {
                break;
            }
        }
    }
    let runs = posted_runs;
    if runs.is_empty() {
        return Err("no posted WoFS imagery is currently available".to_owned());
    }
    // Menu tree + per-product time grids for the newest run.
    let newest = &runs[0];
    let init = newest
        .inits
        .first()
        .cloned()
        .unwrap_or_else(|| format!("{}1700", newest.rundate));
    let query = format!(
        "model={}&rd={}&rt={}&product=t_2__ens_mean&sector=wofs",
        newest.id, newest.rundate, init
    );
    let hierarchy_text =
        data_source::fetch_text(&format!("{API}/hierarchy?{query}&type=hierarchy"))
            .map_err(|e| format!("hierarchy: {e}"))?;
    let hierarchy: serde_json::Value =
        serde_json::from_str(&hierarchy_text).map_err(|e| e.to_string())?;
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    if let Some(top) = hierarchy.as_object() {
        for (group, sub) in top {
            let mut slugs = Vec::new();
            collect_slugs(sub, &mut slugs);
            if !slugs.is_empty() {
                groups.push((group.clone(), slugs));
            }
        }
    }
    let products_text = data_source::fetch_text(&format!(
        "{API}/products?{query}&type=products&metadata=true"
    ))
    .map_err(|e| format!("products: {e}"))?;
    let products: serde_json::Value =
        serde_json::from_str(&products_text).map_err(|e| e.to_string())?;
    let mut times = HashMap::new();
    if let Some(map) = products.as_object() {
        for (slug, meta) in map {
            if let Some(list) = meta["times_available"].as_array() {
                let secs: Vec<u32> = list
                    .iter()
                    .filter_map(|v| v.as_u64().map(|s| s as u32))
                    .collect();
                if !secs.is_empty() {
                    times.insert(slug.clone(), secs);
                }
            }
        }
    }
    // `hierarchy` is a capability tree, not an availability response. It
    // contains products with no files for this run as well as transparent
    // overlay-only images. Present only metadata-backed base products here;
    // transparent products remain in the dedicated Overlays menu, which is
    // built from `times`.
    retain_available_base_products(&mut groups, &times);
    Ok(WofsCatalog {
        runs,
        groups,
        times,
    })
}

fn retain_available_base_products(
    groups: &mut Vec<(String, Vec<String>)>,
    times: &HashMap<String, Vec<u32>>,
) {
    for (_, slugs) in groups.iter_mut() {
        slugs.retain(|slug| times.contains_key(slug) && !slug.contains("_overlay_"));
    }
    groups.retain(|(_, slugs)| !slugs.is_empty());
}

/// Drop leading live cycles that the API has announced but the image CDN has
/// not posted yet.  A run with no verified cycle is omitted entirely, so the
/// run picker never promises a black/unloadable selection.
fn retain_posted_inits(
    mut run: WofsRun,
    mut exists: impl FnMut(&str) -> Result<bool, String>,
) -> Result<Option<WofsRun>, String> {
    let mut first_posted = None;
    for (index, init) in run.inits.iter().enumerate() {
        let probe = image_url(&run, init, SND_REF_PRODUCT, 0);
        if exists(&probe)? {
            first_posted = Some(index);
            break;
        }
    }
    let Some(first_posted) = first_posted else {
        return Ok(None);
    };
    run.inits.drain(..first_posted);
    Ok(Some(run))
}

/// Depth-first slug collection through the hierarchy's nested maps
/// (group -> category -> {items: {sub: [slugs]}}).
fn collect_slugs(node: &serde_json::Value, out: &mut Vec<String>) {
    match node {
        serde_json::Value::Array(items) => {
            for item in items {
                if let Some(slug) = item.as_str() {
                    out.push(slug.to_owned());
                }
            }
        }
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                if key == "help_url" {
                    continue;
                }
                collect_slugs(value, out);
            }
        }
        _ => {}
    }
}

/// Product image URL: forecast minute as f{MMM}.
pub fn image_url(run: &WofsRun, init: &str, product: &str, minute: u32) -> String {
    let (init_date, init_hhmm) = init_date_and_hhmm(&run.rundate, init);
    format!(
        "{CDN}/{}/{}/{}/img/{}_f{minute:03}.png",
        run.id, init_date, init_hhmm, product
    )
}

fn availability_key(run: &WofsRun, init: &str, product: &str) -> String {
    format!(
        "{}/{}/{}",
        run.id,
        canonical_init_token(&run.rundate, init),
        product
    )
}

/// Find the posted edge of the otherwise-theoretical metadata time grid.
/// WoFS publishes frames contiguously from f000, so a binary search proves the
/// live edge in at most eight HEAD requests instead of walking all 73 frames.
fn latest_posted_minute(
    run: &WofsRun,
    init: &str,
    product: &str,
    candidates: &[u32],
    mut exists: impl FnMut(&str) -> Result<bool, String>,
) -> Result<u32, String> {
    let mut minutes = candidates.to_vec();
    minutes.sort_unstable();
    minutes.dedup();
    let Some(&first) = minutes.first() else {
        return Err("product has no advertised forecast times".to_owned());
    };
    if !exists(&image_url(run, init, product, first))? {
        return Err("product has no posted analysis frame for this run/cycle".to_owned());
    }

    let mut low = 0usize;
    let mut high = minutes.len() - 1;
    while low < high {
        let middle = (low + high + 1) / 2;
        if exists(&image_url(run, init, product, minutes[middle]))? {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    Ok(minutes[low])
}

pub fn product_label(slug: &str) -> String {
    if let Some(label) = known_product_label(slug) {
        return label.to_owned();
    }

    let mut parts = slug.splitn(2, "__");
    let field = parts.next().unwrap_or(slug);
    let base = field_label(field);
    let Some(detail) = parts.next() else {
        return base;
    };
    let detail = detail_label(detail);
    if detail.is_empty() {
        base
    } else {
        format!("{base} ({detail})")
    }
}

fn known_product_label(slug: &str) -> Option<&'static str> {
    match slug {
        "comp_dz__paintballs_thresh_40" => Some("Composite reflectivity (paintballs >=40 dBZ)"),
        "comp_dz__ens_mean" => Some("Composite reflectivity (ensemble mean)"),
        "t_2__ens_mean" => Some("2 m temperature (ensemble mean)"),
        "td_2__ens_mean" => Some("2 m dewpoint (ensemble mean)"),
        "uh_0to2__paintballs_thresh_75" => Some("0-2 km updraft helicity (paintballs >=75)"),
        "uh_2to5__paintballs_thresh_75" => Some("2-5 km updraft helicity (paintballs >=75)"),
        _ => None,
    }
}

fn field_label(field: &str) -> String {
    match field {
        "comp_dz" => "Composite reflectivity".to_owned(),
        "dz" => "Reflectivity".to_owned(),
        "t_2" => "2 m temperature".to_owned(),
        "td_2" => "2 m dewpoint".to_owned(),
        "u_10" => "10 m U wind".to_owned(),
        "v_10" => "10 m V wind".to_owned(),
        "uh_0to2" => "0-2 km updraft helicity".to_owned(),
        "uh_2to5" => "2-5 km updraft helicity".to_owned(),
        "w_up" | "updraft" => "Updraft speed".to_owned(),
        "mlcape" => "Mixed-layer CAPE".to_owned(),
        "mucape" => "Most-unstable CAPE".to_owned(),
        "sbcape" => "Surface-based CAPE".to_owned(),
        _ => title_slug(field),
    }
}

fn detail_label(detail: &str) -> String {
    match detail {
        "ens_mean" => "ensemble mean".to_owned(),
        "ens_max" => "ensemble max".to_owned(),
        "ens_min" => "ensemble min".to_owned(),
        "ens_sd" | "ens_std" => "ensemble spread".to_owned(),
        _ if detail.starts_with("paintballs_thresh_") => format!(
            "paintballs >={}",
            detail.trim_start_matches("paintballs_thresh_")
        ),
        _ if detail.starts_with("prob_thresh_") => {
            format!(
                "probability >={}",
                detail.trim_start_matches("prob_thresh_")
            )
        }
        _ => title_slug(detail).to_ascii_lowercase(),
    }
}

fn title_slug(slug: &str) -> String {
    let words = slug
        .split('_')
        .filter(|word| !word.is_empty())
        .map(|word| match word {
            "dz" => "reflectivity".to_owned(),
            "uh" => "updraft helicity".to_owned(),
            "to" => "to".to_owned(),
            _ if word.contains("to") => word.replace("to", "-"),
            _ => word.to_owned(),
        })
        .collect::<Vec<_>>();
    let mut text = words.join(" ");
    if let Some(first) = text.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    text
}

/// One sounding station from the lattice JSON: `{station:"RR_CC", x, y}`
/// where x/y are fractions of the product-PNG axes box ([`AXES_BOX`]),
/// y from the bottom (row 01 = south).
#[derive(Clone, Debug)]
pub struct WofsStation {
    /// "RR_CC" — row from the south, column from the west, zero-padded
    /// exactly as the sounding filename wants them.
    pub id: String,
    pub x: f32,
    pub y: f32,
}

impl WofsStation {
    /// Position as a fraction of the FULL product image (u right, v down),
    /// ready to lerp into the on-screen image rect.
    pub fn image_uv(&self) -> (f32, f32) {
        let (x0, y0, x1, y1) = AXES_BOX;
        let (w, h) = PRODUCT_PNG_SIZE;
        let u = (x0 + self.x * (x1 - x0)) / w;
        let v = (y1 - self.y * (y1 - y0)) / h;
        (u, v)
    }
}

/// Fetch the sounding-station lattice for one run/init (blocking; worker
/// thread). The lattice JSON carries no lat/lon — only image fractions;
/// each sounding PNG's title states the station's lat/lon.
pub fn fetch_stations(run_id: &str, rundate: &str, init: &str) -> Result<Vec<WofsStation>, String> {
    let init = canonical_init_token(rundate, init);
    let url = format!(
        "{API}/stations?model={run_id}&rd={rundate}&rt={init}&product={SND_REF_PRODUCT}&sector=wofs&type=stations"
    );
    let text = data_source::fetch_text(&url).map_err(|e| format!("stations: {e}"))?;
    let root: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let mut stations = Vec::new();
    if let Some(list) = root.as_array() {
        for entry in list {
            let id = entry["station"].as_str().unwrap_or("").to_owned();
            let (Some(x), Some(y)) = (entry["x"].as_f64(), entry["y"].as_f64()) else {
                continue;
            };
            if !id.is_empty() {
                stations.push(WofsStation {
                    id,
                    x: x as f32,
                    y: y as f32,
                });
            }
        }
    }
    if stations.is_empty() {
        return Err("no WoFS sounding stations".to_owned());
    }
    Ok(stations)
}

/// Sounding image URL. `frame` indexes the 5-minute product time grid
/// (frame 0 = analysis, frame 36 = +180 min — verified live 2026-06-11:
/// frames 0..=72 exist for a 0..=360 min run, 73 returns HTTP 500).
pub fn sounding_url(run: &WofsRun, init: &str, frame: u32, station: &str) -> String {
    let init = canonical_init_token(&run.rundate, init);
    format!("{SND_CDN}/{}/{init}/{frame}/wofs_snd_{station}.png", run.id)
}

/// Fetch + decode one product PNG into an egui image (palette -> RGBA).
pub fn fetch_image(url: &str) -> Result<egui::ColorImage, String> {
    let bytes = data_source::fetch_bytes(url).map_err(|e| e.to_string())?;
    let img = image::load_from_memory(&bytes)
        .map_err(|e| e.to_string())?
        .to_rgba8();
    let (w, h) = (img.width() as usize, img.height() as usize);
    Ok(egui::ColorImage::from_rgba_unmultiplied(
        [w, h],
        img.as_raw(),
    ))
}

/// Station-lattice worker message: the "{run_id}/{init}" key it was
/// fetched for + the result.
pub type StationsMsg = (String, Result<Vec<WofsStation>, String>);
type AvailabilityMsg = (String, Result<u32, String>);

/// Disk cache path for one run's georef (run ids are CDN-safe already;
/// sanitize anyway).
fn georef_disk_path(run_id: &str) -> std::path::PathBuf {
    let safe: String = run_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    settings::wofs_georef_dir().join(format!("{safe}.json"))
}

/// Saved georefs re-pass the sanity check on load, so a corrupt or
/// stale-format file degrades to a fresh calibration, never a bad drape.
fn load_georef_from_disk(run_id: &str) -> Option<WofsGeoref> {
    let text = std::fs::read_to_string(georef_disk_path(run_id)).ok()?;
    let georef: WofsGeoref = serde_json::from_str(&text).ok()?;
    georef.sanity_check().ok()?;
    Some(georef)
}

/// Best-effort persist + prune (WoFS cycles daily; two weeks of runs).
fn save_georef_to_disk(run_id: &str, georef: &WofsGeoref) {
    let dir = settings::wofs_georef_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    if let Ok(json) = serde_json::to_string(georef) {
        let _ = std::fs::write(georef_disk_path(run_id), json);
    }
    if let Ok(entries) = std::fs::read_dir(&dir) {
        let mut files: Vec<_> = entries
            .flatten()
            .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
            .collect();
        files.sort();
        while files.len() > 14 {
            let (_, path) = files.remove(0);
            let _ = std::fs::remove_file(path);
        }
    }
}

pub struct WofsState {
    pub open: bool,
    pub catalog: Option<WofsCatalog>,
    pub catalog_rx: Option<mpsc::Receiver<Result<WofsCatalog, String>>>,
    pub run_index: usize,
    pub init: String,
    pub product: String,
    pub minute: u32,
    /// Stacked transparent overlays (paintball slugs).
    pub overlays: Vec<String>,
    pub sync_to_radar: bool,
    /// Texture cache by URL (bounded).
    pub textures: HashMap<String, egui::TextureHandle>,
    pub image_rx: Option<mpsc::Receiver<(String, Result<egui::ColorImage, String>)>>,
    pub pending_urls: Vec<String>,
    /// URLs that 404'd (imagery not posted yet for a live run) — retried
    /// only after the backoff so the fetcher never spams.
    pub missing: HashMap<String, Instant>,
    /// Highest CDN-posted forecast minute for each run/init/product. The WoFS
    /// metadata endpoint advertises the complete theoretical grid even while
    /// a live cycle has only posted part of it. Partial live edges are
    /// refreshed once per minute until the complete grid lands.
    max_posted_minutes: HashMap<String, (u32, Instant)>,
    availability_rx: Option<mpsc::Receiver<AvailabilityMsg>>,
    /// Last availability failure by selection: timestamp + diagnostic. The
    /// message lets the UI distinguish a proved-missing frame from a network
    /// error where availability is merely unknown.
    availability_failed: HashMap<String, (Instant, String)>,
    pub status: String,
    /// Soundings mode: station lattice overlay + per-station skew-T window.
    pub soundings_mode: bool,
    /// The 20x20 sounding-station lattice for `stations_key`.
    pub stations: Vec<WofsStation>,
    /// "{run_id}/{init}" the lattice was fetched for (refetch on change).
    pub stations_key: String,
    pub stations_rx: Option<mpsc::Receiver<StationsMsg>>,
    /// Last failed lattice fetch (key, when) — retried after a backoff.
    pub stations_failed: Option<(String, Instant)>,
    /// Station whose sounding window is open ("RR_CC").
    pub selected_station: Option<String>,
    /// Manual sounding frame; None follows the product forecast minute.
    pub snd_frame: Option<u32>,
    /// Drape the current product onto the radar map (georeferenced).
    pub drape_on_map: bool,
    pub drape_opacity: f32,
    /// Repeat the source-texture pass to lift WoFS's intentionally faint PNG
    /// alpha. This never recolors or reprojects the published imagery.
    pub drape_high_visibility: bool,
    /// Georef cache by RUN id (the domain is per-run, not per-frame).
    pub georef_cache: HashMap<String, Arc<WofsGeoref>>,
    /// Runs whose calibration failed (message) — drape disabled for them.
    pub georef_failed: HashMap<String, String>,
    /// In-flight calibration: (run id, result channel).
    pub georef_rx: Option<(String, mpsc::Receiver<Result<WofsGeoref, String>>)>,
    /// Stations processed by the in-flight calibration (status line).
    pub georef_progress: Arc<AtomicUsize>,
}

/// Honest map-layer readiness. Keeping this separate from `drape_on_map`
/// prevents the rail from calling a selected-but-undrawable frame "live".
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WofsDrapeReadiness {
    Off,
    WaitingForCatalog,
    CheckingAvailability,
    FrameUnavailable,
    AvailabilityError,
    LoadingFrame,
    InvalidFrame,
    CalibratingGeoref,
    GeorefFailed,
    Ready,
}

impl WofsDrapeReadiness {
    pub(crate) fn rail_state(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::WaitingForCatalog
            | Self::CheckingAvailability
            | Self::LoadingFrame
            | Self::CalibratingGeoref => "loading",
            Self::FrameUnavailable | Self::InvalidFrame => "unavailable",
            Self::AvailabilityError | Self::GeorefFailed => "error",
            Self::Ready => "ready",
        }
    }

    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::Off => "not selected for the map",
            Self::WaitingForCatalog => "waiting for the WoFS catalog and run selection",
            Self::CheckingAvailability => "checking whether this exact product frame is posted",
            Self::FrameUnavailable => "this exact product frame is unavailable at NSSL",
            Self::AvailabilityError => "could not verify this frame's availability",
            Self::LoadingFrame => "loading the selected base-product texture",
            Self::InvalidFrame => "the selected image is not the standard georeferenceable size",
            Self::CalibratingGeoref => "calibrating this run's map georeference",
            Self::GeorefFailed => "this run's map georeference failed validation",
            Self::Ready => "current base texture and validated georeference are ready",
        }
    }

    fn is_ready(self) -> bool {
        self == Self::Ready
    }
}

impl Default for WofsState {
    fn default() -> Self {
        Self {
            open: false,
            catalog: None,
            catalog_rx: None,
            run_index: 0,
            init: String::new(),
            product: "comp_dz__paintballs_thresh_40".to_owned(),
            minute: 60,
            overlays: Vec::new(),
            sync_to_radar: true,
            textures: HashMap::new(),
            image_rx: None,
            pending_urls: Vec::new(),
            missing: HashMap::new(),
            max_posted_minutes: HashMap::new(),
            availability_rx: None,
            availability_failed: HashMap::new(),
            status: String::new(),
            soundings_mode: false,
            stations: Vec::new(),
            stations_key: String::new(),
            stations_rx: None,
            stations_failed: None,
            selected_station: None,
            snd_frame: None,
            drape_on_map: false,
            drape_opacity: DEFAULT_DRAPE_OPACITY,
            drape_high_visibility: true,
            georef_cache: HashMap::new(),
            georef_failed: HashMap::new(),
            georef_rx: None,
            georef_progress: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl WofsState {
    fn current_base_url(&self) -> Option<String> {
        let catalog = self.catalog.as_ref()?;
        let run = catalog.runs.get(self.run_index)?;
        (!self.init.is_empty()).then(|| image_url(run, &self.init, &self.product, self.minute))
    }

    fn current_availability_key(&self) -> Option<String> {
        let catalog = self.catalog.as_ref()?;
        let run = catalog.runs.get(self.run_index)?;
        (!self.init.is_empty()).then(|| availability_key(run, &self.init, &self.product))
    }

    /// Whether the current selection can produce pixels on the map now.
    /// A checked visibility box is only intent; this method also requires a
    /// posted base frame, the standard PNG shape, and a validated georef.
    pub(crate) fn drape_readiness(&self) -> WofsDrapeReadiness {
        if !self.drape_on_map {
            return WofsDrapeReadiness::Off;
        }
        let Some(catalog) = &self.catalog else {
            return WofsDrapeReadiness::WaitingForCatalog;
        };
        let Some(run) = catalog.runs.get(self.run_index) else {
            return WofsDrapeReadiness::WaitingForCatalog;
        };
        if self.init.is_empty() {
            return WofsDrapeReadiness::WaitingForCatalog;
        }

        let availability = availability_key(run, &self.init, &self.product);
        if !self.max_posted_minutes.contains_key(&availability) {
            return match self.availability_failed.get(&availability) {
                Some((_, error))
                    if error.contains("no posted")
                        || error.contains("no forecast grid")
                        || error.contains("no advertised") =>
                {
                    WofsDrapeReadiness::FrameUnavailable
                }
                Some(_) => WofsDrapeReadiness::AvailabilityError,
                None => WofsDrapeReadiness::CheckingAvailability,
            };
        }

        let base_url = image_url(run, &self.init, &self.product, self.minute);
        if self.missing.contains_key(&base_url) {
            return WofsDrapeReadiness::FrameUnavailable;
        }
        let Some(texture) = self.textures.get(&base_url) else {
            return WofsDrapeReadiness::LoadingFrame;
        };
        if texture.size() != [PRODUCT_PNG_SIZE.0 as usize, PRODUCT_PNG_SIZE.1 as usize] {
            return WofsDrapeReadiness::InvalidFrame;
        }
        if self.georef_failed.contains_key(&run.id) {
            return WofsDrapeReadiness::GeorefFailed;
        }
        if !self.georef_cache.contains_key(&run.id) {
            return WofsDrapeReadiness::CalibratingGeoref;
        }
        WofsDrapeReadiness::Ready
    }

    fn drape_passes(&self) -> usize {
        if self.drape_high_visibility {
            HIGH_VISIBILITY_PASSES
        } else {
            1
        }
    }

    fn clamp_minute_to_posted_edge(&mut self) -> bool {
        let Some(key) = self.current_availability_key() else {
            return false;
        };
        let Some(&(max_posted, _)) = self.max_posted_minutes.get(&key) else {
            return false;
        };
        if self.minute <= max_posted {
            return false;
        }
        let clamped = self
            .catalog
            .as_ref()
            .and_then(|catalog| catalog.times.get(&self.product))
            .into_iter()
            .flatten()
            .map(|seconds| seconds / 60)
            .filter(|minute| *minute <= max_posted)
            .max()
            .unwrap_or(max_posted);
        self.minute = clamped;
        true
    }

    fn pump_availability(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.availability_rx {
            match rx.try_recv() {
                Ok((key, Ok(max_posted))) => {
                    self.availability_rx = None;
                    self.availability_failed.remove(&key);
                    self.max_posted_minutes
                        .insert(key.clone(), (max_posted, Instant::now()));
                    if self.current_availability_key().as_deref() == Some(key.as_str())
                        && self.clamp_minute_to_posted_edge()
                    {
                        self.status = format!(
                            "live cycle currently posted through f+{} min; selection adjusted",
                            self.minute
                        );
                    }
                }
                Ok((key, Err(error))) => {
                    self.availability_rx = None;
                    if self.current_availability_key().as_deref() == Some(key.as_str()) {
                        self.status = format!("WoFS availability: {error}");
                    }
                    self.availability_failed
                        .insert(key, (Instant::now(), error));
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => self.availability_rx = None,
            }
        }

        // Cached selections still need clamping when radar-time sync advances
        // past the live CDN edge.
        self.clamp_minute_to_posted_edge();
        if self.availability_rx.is_some() {
            return;
        }
        let Some(catalog) = &self.catalog else {
            return;
        };
        let Some(run) = catalog.runs.get(self.run_index).cloned() else {
            return;
        };
        if self.init.is_empty() {
            return;
        }
        let init = self.init.clone();
        let product = self.product.clone();
        let key = availability_key(&run, &init, &product);
        let advertised_max = catalog
            .times
            .get(&product)
            .into_iter()
            .flatten()
            .map(|seconds| seconds / 60)
            .max()
            .unwrap_or(0);
        if self
            .max_posted_minutes
            .get(&key)
            .is_some_and(|(posted, checked)| {
                *posted >= advertised_max || checked.elapsed().as_secs() <= 60
            })
        {
            return;
        }
        if self
            .availability_failed
            .get(&key)
            .is_some_and(|(at, _)| at.elapsed().as_secs() <= 60)
        {
            return;
        }
        let Some(candidates) = catalog.times.get(&product).map(|seconds| {
            seconds
                .iter()
                .map(|seconds| seconds / 60)
                .collect::<Vec<_>>()
        }) else {
            let error = "product has no forecast grid".to_owned();
            self.availability_failed
                .insert(key, (Instant::now(), error.clone()));
            self.status = format!("WoFS availability: {error}");
            return;
        };
        let (tx, rx) = mpsc::channel();
        // A backoff-expired failure is now being retried; expose "checking"
        // instead of leaving the stale error state visible during the probe.
        self.availability_failed.remove(&key);
        self.availability_rx = Some(rx);
        let ctx_clone = ctx.clone();
        thread::spawn(move || {
            let result = latest_posted_minute(&run, &init, &product, &candidates, |url| {
                data_source::url_exists(url).map_err(|error| error.to_string())
            });
            let _ = tx.send((key, result));
            ctx_clone.request_repaint();
        });
    }

    /// Nearest available forecast minute for the current product.
    pub fn snap_minute(&self, target_min: u32) -> u32 {
        let Some(catalog) = &self.catalog else {
            return target_min;
        };
        let Some(secs) = catalog.times.get(&self.product) else {
            return target_min;
        };
        secs.iter()
            .map(|s| s / 60)
            .min_by_key(|m| m.abs_diff(target_min))
            .unwrap_or(target_min)
    }

    /// The sounding frame grid (seconds): the reference product's
    /// `times_available` — soundings post on the same 5-min grid.
    fn snd_grid(&self) -> Option<&Vec<u32>> {
        self.catalog.as_ref().and_then(|c| {
            c.times
                .get(SND_REF_PRODUCT)
                .or_else(|| c.times.get(&self.product))
        })
    }

    /// Frame index on the sounding grid nearest a forecast minute.
    fn frame_for_minute(&self, minute: u32) -> u32 {
        if let Some(secs) = self.snd_grid()
            && !secs.is_empty()
        {
            let target = minute * 60;
            return secs
                .iter()
                .enumerate()
                .min_by_key(|(_, s)| s.abs_diff(target))
                .map(|(i, _)| i as u32)
                .unwrap_or(0);
        }
        minute / 5
    }

    /// Highest valid sounding frame index (72 on the standard 6 h grid).
    pub fn max_frame(&self) -> u32 {
        self.snd_grid()
            .map(|secs| secs.len().saturating_sub(1) as u32)
            .unwrap_or(72)
    }

    /// Forecast minute a sounding frame is valid at.
    fn frame_minute(&self, frame: u32) -> u32 {
        self.snd_grid()
            .and_then(|secs| secs.get(frame as usize).map(|s| s / 60))
            .unwrap_or(frame * 5)
    }

    /// Current sounding frame: manual prev/next override, else follow the
    /// product forecast minute.
    pub fn sounding_frame(&self) -> u32 {
        self.snd_frame
            .unwrap_or_else(|| self.frame_for_minute(self.minute))
    }

    /// Queue any missing textures for the current selection.
    pub fn want_urls(&self) -> Vec<String> {
        let Some(catalog) = &self.catalog else {
            return Vec::new();
        };
        let Some(run) = catalog.runs.get(self.run_index) else {
            return Vec::new();
        };
        let availability = availability_key(run, &self.init, &self.product);
        if !self.max_posted_minutes.contains_key(&availability) {
            return Vec::new();
        }
        let mut urls = vec![image_url(run, &self.init, &self.product, self.minute)];
        for overlay in &self.overlays {
            urls.push(image_url(run, &self.init, overlay, self.minute));
        }
        if self.soundings_mode
            && let Some(station) = &self.selected_station
        {
            urls.push(sounding_url(
                run,
                &self.init,
                self.sounding_frame(),
                station,
            ));
        }
        urls.into_iter()
            .filter(|u| !self.textures.contains_key(u))
            .filter(|u| {
                self.missing
                    .get(u)
                    .map(|at| at.elapsed().as_secs() > 60)
                    .unwrap_or(true)
            })
            .collect()
    }

    pub fn pump(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.catalog_rx {
            match rx.try_recv() {
                Ok(Ok(catalog)) => {
                    self.catalog_rx = None;
                    if self.init.is_empty()
                        && let Some(run) = catalog.runs.first()
                        && let Some(init) = run.inits.first()
                    {
                        self.init = init.clone();
                    }
                    self.status = format!(
                        "{} runs · {}",
                        catalog.runs.len(),
                        catalog
                            .runs
                            .first()
                            .map(|r| r.name.clone())
                            .unwrap_or_default()
                    );
                    self.catalog = Some(catalog);
                }
                Ok(Err(e)) => {
                    self.catalog_rx = None;
                    self.status = format!("WoFS catalog: {e}");
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => self.catalog_rx = None,
            }
        }
        self.pump_availability(ctx);
        if let Some(rx) = &self.image_rx {
            match rx.try_recv() {
                Ok((url, Ok(image))) => {
                    let is_current_base = self.current_base_url().as_deref() == Some(url.as_str());
                    self.image_rx = None;
                    self.pending_urls.retain(|u| u != &url);
                    self.missing.remove(&url);
                    if self.textures.len() > 72 {
                        self.textures.clear(); // simple bound; refetch is cheap
                    }
                    let handle = ctx.load_texture(url.clone(), image, egui::TextureOptions::LINEAR);
                    self.textures.insert(url, handle);
                    if is_current_base {
                        self.status = "WoFS frame ready".to_owned();
                    }
                }
                Ok((url, Err(e))) => {
                    let is_current_base = self.current_base_url().as_deref() == Some(url.as_str());
                    self.image_rx = None;
                    self.pending_urls.retain(|u| u != &url);
                    // A missing file can mean posting lag, a product absent
                    // from this domain, or archive expiry. Do not call every
                    // historical 404 a live-run delay.
                    self.status = if e.contains("404") || e.contains("410") || e.contains("500") {
                        if is_current_base {
                            "frame unavailable at NSSL (not posted or no longer retained)"
                                .to_owned()
                        } else {
                            "WoFS overlay unavailable for this frame".to_owned()
                        }
                    } else {
                        let mut msg = e;
                        msg.truncate(90);
                        format!("WoFS: {msg}")
                    };
                    self.missing.insert(url, Instant::now());
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => self.image_rx = None,
            }
        }
        // Station lattice for soundings mode (refetched when run/init
        // changes; one failure backs off 30 s).
        if let Some(rx) = &self.stations_rx {
            match rx.try_recv() {
                Ok((key, Ok(stations))) => {
                    self.stations_rx = None;
                    self.stations = stations;
                    self.stations_key = key;
                    self.stations_failed = None;
                }
                Ok((key, Err(e))) => {
                    self.stations_rx = None;
                    let mut msg = e;
                    msg.truncate(90);
                    self.status = format!("WoFS stations: {msg}");
                    self.stations_failed = Some((key, Instant::now()));
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => self.stations_rx = None,
            }
        }
        if self.soundings_mode
            && self.stations_rx.is_none()
            && let Some(catalog) = &self.catalog
            && let Some(run) = catalog.runs.get(self.run_index)
            && !self.init.is_empty()
        {
            let key = format!("{}/{}", run.id, self.init);
            let backoff_ok = self
                .stations_failed
                .as_ref()
                .map(|(k, at)| k != &key || at.elapsed().as_secs() > 30)
                .unwrap_or(true);
            if key != self.stations_key && backoff_ok {
                let (tx, rx) = mpsc::channel();
                self.stations_rx = Some(rx);
                let (run_id, rundate, init) =
                    (run.id.clone(), run.rundate.clone(), self.init.clone());
                let ctx_clone = ctx.clone();
                thread::spawn(move || {
                    let result = fetch_stations(&run_id, &rundate, &init);
                    let _ = tx.send((key, result));
                    ctx_clone.request_repaint();
                });
            }
        }
        // One in-flight fetch at a time; the CDN is fast.
        if self.image_rx.is_none()
            && let Some(url) = self
                .want_urls()
                .into_iter()
                .find(|u| !self.pending_urls.contains(u))
        {
            self.pending_urls.push(url.clone());
            let (tx, rx) = mpsc::channel();
            self.image_rx = Some(rx);
            let ctx_clone = ctx.clone();
            thread::spawn(move || {
                let result = fetch_image(&url);
                let _ = tx.send((url, result));
                ctx_clone.request_repaint();
            });
        }
        self.pump_georef(ctx);
    }

    /// Drape calibration lifecycle: collect a finished build, and kick off
    /// a new one when the drape is on and the selected run has no georef
    /// yet. Results cache per RUN id (the domain is per-run).
    fn pump_georef(&mut self, ctx: &egui::Context) {
        if let Some((run_id, rx)) = &self.georef_rx {
            match rx.try_recv() {
                Ok(Ok(georef)) => {
                    // Persist: calibration costs 8-18 s of sounding
                    // fetches and the result is stable per run.
                    save_georef_to_disk(run_id, &georef);
                    self.georef_cache.insert(run_id.clone(), Arc::new(georef));
                    self.georef_rx = None;
                }
                Ok(Err(error)) => {
                    self.georef_failed.insert(run_id.clone(), error);
                    self.georef_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => self.georef_rx = None,
            }
        }
        if self.drape_on_map
            && self.georef_rx.is_none()
            && let Some(catalog) = &self.catalog
            && let Some(run) = catalog.runs.get(self.run_index)
            && !self.init.is_empty()
            && !self.georef_cache.contains_key(&run.id)
            && !self.georef_failed.contains_key(&run.id)
        {
            if let Some(saved) = load_georef_from_disk(&run.id) {
                self.georef_cache.insert(run.id.clone(), Arc::new(saved));
                return;
            }
            let (tx, rx) = mpsc::channel();
            self.georef_rx = Some((run.id.clone(), rx));
            self.georef_progress = Arc::new(AtomicUsize::new(0));
            let progress = Arc::clone(&self.georef_progress);
            let run_id = run.id.clone();
            let rd_init = canonical_init_token(&run.rundate, &self.init);
            let ctx_clone = ctx.clone();
            thread::spawn(move || {
                let result = wofs_georef::build_georef(&run_id, &rd_init, Some(&progress));
                let _ = tx.send(result);
                ctx_clone.request_repaint();
            });
        }
    }

    fn retry_georef_for_run(&mut self, run_id: &str) {
        self.georef_failed.remove(run_id);
    }

    /// "Show on map" toggle + opacity + calibration status, rendered inside
    /// the WoFS window.
    pub fn drape_controls_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(&mut self.drape_on_map, "Show on map")
                .on_hover_text(
                    "Drape the current product onto the radar map. The georeference is \
                 calibrated per run by OCR-ing sounding-PNG titles (the stations' \
                 true lat/lons) and quadratic-fitting the Lambert domain.",
                );
            if !self.drape_on_map {
                return;
            }
            ui.add(
                egui::Slider::new(&mut self.drape_opacity, 0.05..=1.0)
                    .text("opacity")
                    .show_value(true),
            );
            ui.checkbox(&mut self.drape_high_visibility, "High visibility")
                .on_hover_text(
                    "Lift the WoFS PNG's intentionally low source alpha by drawing the exact \
                     same texture twice. Fully transparent pixels stay transparent; published \
                     RGB product colors and the georeference are unchanged. Observed radar \
                     intentionally draws above WoFS; lower Radar opacity to compare overlap.",
                );

            match self.drape_readiness() {
                WofsDrapeReadiness::Off => {}
                WofsDrapeReadiness::WaitingForCatalog => {
                    ui.spinner();
                    ui.weak("waiting for catalog…");
                }
                WofsDrapeReadiness::CheckingAvailability => {
                    ui.spinner();
                    ui.weak("checking selected frame…");
                }
                WofsDrapeReadiness::FrameUnavailable => {
                    ui.colored_label(
                        egui::Color32::from_rgb(230, 150, 90),
                        "map drape unavailable for this exact frame",
                    );
                }
                WofsDrapeReadiness::AvailabilityError => {
                    ui.colored_label(
                        egui::Color32::from_rgb(230, 150, 90),
                        "map drape paused: availability check failed",
                    );
                }
                WofsDrapeReadiness::LoadingFrame => {
                    ui.spinner();
                    ui.weak("loading selected frame…");
                }
                WofsDrapeReadiness::InvalidFrame => {
                    ui.colored_label(
                        egui::Color32::from_rgb(230, 150, 90),
                        "map drape disabled: unexpected image dimensions",
                    );
                }
                WofsDrapeReadiness::CalibratingGeoref => {
                    ui.spinner();
                    ui.weak(format!(
                        "calibrating georef… {}/{} soundings",
                        self.georef_progress
                            .load(Ordering::Relaxed)
                            .min(wofs_georef::CALIBRATION_TOTAL),
                        wofs_georef::CALIBRATION_TOTAL
                    ));
                }
                WofsDrapeReadiness::GeorefFailed => {
                    let run_id = self
                        .catalog
                        .as_ref()
                        .and_then(|catalog| catalog.runs.get(self.run_index))
                        .map(|run| run.id.clone())
                        .unwrap_or_default();
                    let error = self
                        .georef_failed
                        .get(&run_id)
                        .cloned()
                        .unwrap_or_else(|| "validation failed".to_owned());
                    ui.colored_label(
                        egui::Color32::from_rgb(230, 150, 90),
                        format!("drape disabled for this run: {error}"),
                    );
                    if ui
                        .small_button("Retry")
                        .on_hover_text("Retry this run's sounding-title calibration")
                        .clicked()
                    {
                        self.retry_georef_for_run(&run_id);
                    }
                }
                WofsDrapeReadiness::Ready => {
                    if let Some(georef) = self.catalog.as_ref().and_then(|catalog| {
                        let run = catalog.runs.get(self.run_index)?;
                        self.georef_cache.get(&run.id)
                    }) {
                        ui.weak(format!(
                            "map ready · georef resid {:.2}/{:.2}°",
                            georef.lat_max_resid, georef.lon_max_resid
                        ));
                    }
                }
            }
        });
    }

    /// Drape the current product (and stacked overlays) onto the radar map
    /// as a textured mesh: vertices at `project(lonlat_of(u, v))`, UVs into
    /// the axes-box subrect of the already-fetched window texture.
    ///
    /// The owner keeps pumping radar-time sync while a drape is enabled, so
    /// closing the WoFS window does not remove or silently freeze the map
    /// layer.
    pub fn draw_drape(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        project: &dyn Fn(f32, f32) -> egui::Pos2,
    ) {
        // Do not paint a stale texture or a transparent overlay by itself.
        // The same readiness gate feeds the layer rail, so its status and
        // actual map pixels cannot disagree.
        if !self.drape_readiness().is_ready() {
            return;
        }
        let Some(catalog) = &self.catalog else {
            return;
        };
        let Some(run) = catalog.runs.get(self.run_index) else {
            return;
        };
        let base_url = image_url(run, &self.init, &self.product, self.minute);
        let Some(georef) = self.georef_cache.get(&run.id) else {
            return;
        };
        let mut urls = vec![base_url];
        for overlay in &self.overlays {
            urls.push(image_url(run, &self.init, overlay, self.minute));
        }
        for url in urls {
            let Some(texture) = self.textures.get(&url) else {
                continue;
            };
            // The axes-box constants assume the standard 900x800 product
            // PNG; skip anything else rather than drape wrong.
            let size = texture.size_vec2();
            if (size.x - wofs_georef::PRODUCT_W).abs() > 0.5
                || (size.y - wofs_georef::PRODUCT_H).abs() > 0.5
            {
                continue;
            }
            let mesh = wofs_georef::drape_mesh(texture.id(), georef, self.drape_opacity, project);
            let bounds = mesh.vertices.iter().fold(egui::Rect::NOTHING, |acc, v| {
                acc.union(egui::Rect::from_min_max(v.pos, v.pos))
            });
            if bounds.intersects(rect) {
                // Repeating a source-over pass raises only coverage from the
                // PNG's low alpha. It does not alter RGB, UV, or vertex data.
                for _ in 0..self.drape_passes() {
                    painter.add(egui::Shape::mesh(mesh.clone()));
                }
            }
        }
    }

    pub fn start_catalog(&mut self, ctx: &egui::Context) {
        if self.catalog_rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.catalog_rx = Some(rx);
        self.status = "loading WoFS catalog…".to_owned();
        let ctx_clone = ctx.clone();
        thread::spawn(move || {
            let result = fetch_catalog();
            let _ = tx.send(result);
            ctx_clone.request_repaint();
        });
    }

    /// Controls-row toggle for soundings mode.
    pub fn soundings_toggle_ui(&mut self, ui: &mut egui::Ui) {
        ui.checkbox(&mut self.soundings_mode, "Soundings").on_hover_text(
            "Overlay NSSL's 20x20 sounding-station lattice on the product image — click a dot to open that station's ensemble skew-T",
        );
    }

    /// Clickable station-lattice overlay; call right after the product
    /// image is painted into `rect`. One interact for the whole rect:
    /// the click picks the nearest dot within a small radius.
    pub fn stations_overlay_ui(&mut self, ui: &mut egui::Ui, rect: egui::Rect) {
        if !self.soundings_mode || self.stations.is_empty() {
            return;
        }
        const PICK_RADIUS: f32 = 14.0;
        let response = ui.interact(
            rect,
            ui.id().with("wofs_station_lattice"),
            egui::Sense::click(),
        );
        let positions: Vec<egui::Pos2> = self
            .stations
            .iter()
            .map(|st| {
                let (u, v) = st.image_uv();
                egui::pos2(
                    rect.min.x + u * rect.width(),
                    rect.min.y + v * rect.height(),
                )
            })
            .collect();
        let nearest = response.hover_pos().and_then(|p| {
            positions
                .iter()
                .enumerate()
                .map(|(i, pos)| (i, p.distance(*pos)))
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .filter(|(_, d)| *d <= PICK_RADIUS)
                .map(|(i, _)| i)
        });
        let painter = ui.painter_at(rect);
        for (i, pos) in positions.iter().enumerate() {
            let selected = self.selected_station.as_deref() == Some(self.stations[i].id.as_str());
            let hovered = nearest == Some(i);
            if selected {
                painter.circle(
                    *pos,
                    4.5,
                    egui::Color32::from_rgb(214, 48, 36),
                    egui::Stroke::new(1.5_f32, egui::Color32::WHITE),
                );
            } else if hovered {
                painter.circle(
                    *pos,
                    4.0,
                    egui::Color32::from_rgb(36, 92, 214),
                    egui::Stroke::new(1.5_f32, egui::Color32::WHITE),
                );
            } else {
                painter.circle_filled(
                    *pos,
                    2.0,
                    egui::Color32::from_rgba_unmultiplied(40, 70, 170, 150),
                );
            }
        }
        if let Some(i) = nearest {
            painter.text(
                positions[i] + egui::vec2(8.0, -8.0),
                egui::Align2::LEFT_BOTTOM,
                &self.stations[i].id,
                egui::FontId::proportional(11.0),
                egui::Color32::from_rgb(20, 30, 60),
            );
            ui.output_mut(|o| o.cursor_icon = egui::CursorIcon::PointingHand);
        }
        if response.clicked()
            && let Some(i) = nearest
        {
            self.selected_station = Some(self.stations[i].id.clone());
            self.snd_frame = None; // a fresh pick follows the product time
        }
    }

    /// Per-station ensemble skew-T sub-window with prev/next frame
    /// stepping; call once per frame after the main WoFS window.
    pub fn sounding_window(&mut self, ctx: &egui::Context) {
        if !self.soundings_mode {
            return;
        }
        let Some(station) = self.selected_station.clone() else {
            return;
        };
        let Some(run) = self
            .catalog
            .as_ref()
            .and_then(|c| c.runs.get(self.run_index))
            .cloned()
        else {
            return;
        };
        let frame = self.sounding_frame();
        let max_frame = self.max_frame();
        let minute = self.frame_minute(frame);
        let url = sounding_url(&run, &self.init, frame, &station);
        let mut open = true;
        egui::Window::new(format!("WoFS Sounding {station}"))
            .id(egui::Id::new("wofs_sounding_window"))
            .open(&mut open)
            .default_size([840.0, 620.0])
            .min_size([420.0, 320.0])
            .resizable(true)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("◀").on_hover_text("previous frame").clicked() {
                        self.snd_frame = Some(frame.saturating_sub(1));
                    }
                    if ui.button("▶").on_hover_text("next frame").clicked() {
                        self.snd_frame = Some((frame + 1).min(max_frame));
                    }
                    ui.label(format!("f+{minute} min · frame {frame}/{max_frame}"));
                    if self.snd_frame.is_some()
                        && ui
                            .small_button("sync")
                            .on_hover_text("follow the product forecast minute again")
                            .clicked()
                    {
                        self.snd_frame = None;
                    }
                    if self.image_rx.is_some() && !self.textures.contains_key(&url) {
                        ui.spinner();
                    }
                });
                // The anonymous stations JSON has no lat/lon — the PNG's
                // own title states it (e.g. "WoFS Sounding 41.83N, -90.66W").
                ui.weak(format!(
                    "station {station} · {} {}z · lat/lon in the image title",
                    run.rundate, self.init
                ));
                let size = ui.available_size();
                let width = size
                    .x
                    .min((size.y - 18.0).max(150.0) * 1200.0 / 800.0)
                    .max(240.0);
                let rect_size = egui::vec2(width, width * 800.0 / 1200.0);
                let (rect, _) = ui.allocate_exact_size(rect_size, egui::Sense::hover());
                if let Some(texture) = self.textures.get(&url) {
                    ui.painter().image(
                        texture.id(),
                        rect,
                        egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                        egui::Color32::WHITE,
                    );
                } else {
                    ui.painter()
                        .rect_filled(rect, 4.0, egui::Color32::from_rgb(14, 16, 20));
                }
                ui.weak(CREDIT);
            });
        if !open {
            self.selected_station = None;
            self.snd_frame = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Station fractions map into the AXES BOX, never the margins. Guards
    /// the bug this geometry was verified against: mapped to the full
    /// 900 px image width, col-20 dots (x = 0.962) land at ~866 px —
    /// outside the plot box (right edge 759 px), in the whitespace margin.
    #[test]
    fn station_uv_stays_inside_axes_box() {
        let (x0, y0, x1, y1) = AXES_BOX;
        let (w, h) = PRODUCT_PNG_SIZE;
        let ne = WofsStation {
            id: "20_20".to_owned(),
            x: 0.962,
            y: 0.962,
        };
        let (u, v) = ne.image_uv();
        assert!(u < x1 / w, "col 20 must stay left of the box edge");
        assert!(u > 0.5);
        assert!(v > y0 / h, "row 20 (north) sits below the title strip");
        assert!(v < 0.5, "y is measured from the BOTTOM: row 20 = top");
        let sw = WofsStation {
            id: "01_01".to_owned(),
            x: 0.031,
            y: 0.031,
        };
        let (u, v) = sw.image_uv();
        assert!(u > x0 / w);
        assert!(v < y1 / h && v > 0.5, "row 01 = south = near image bottom");
    }

    #[test]
    fn sounding_url_uses_run_and_frame() {
        let run = WofsRun {
            id: "WOFSRun20260611-144912d1".to_owned(),
            name: String::new(),
            rundate: "20260611".to_owned(),
            inits: Vec::new(),
        };
        assert_eq!(
            sounding_url(&run, "1700", 36, "10_10"),
            "https://ep-wofs-sounding-etb5awe5cdfqawe8.a01.azurefd.net/api/sounding/WOFSRun20260611-144912d1/202606111700/36/wofs_snd_10_10.png"
        );
    }

    #[test]
    fn midnight_crossing_init_uses_its_own_utc_date() {
        let run = WofsRun {
            id: "WOFSRun20260711-130130d1".to_owned(),
            name: String::new(),
            rundate: "20260711".to_owned(),
            inits: vec!["202607120300".to_owned()],
        };

        assert_eq!(init_hhmm(&run.inits[0]), "0300");
        assert_eq!(
            init_time_utc(&run, &run.inits[0]).unwrap().to_rfc3339(),
            "2026-07-12T03:00:00+00:00"
        );
        assert!(
            image_url(&run, &run.inits[0], "comp_dz__paintballs_thresh_40", 60).ends_with(
                "/WOFSRun20260711-130130d1/20260712/0300/img/comp_dz__paintballs_thresh_40_f060.png"
            )
        );
        assert_eq!(
            sounding_url(&run, &run.inits[0], 12, "10_10"),
            "https://ep-wofs-sounding-etb5awe5cdfqawe8.a01.azurefd.net/api/sounding/WOFSRun20260711-130130d1/202607120300/12/wofs_snd_10_10.png"
        );
    }

    #[test]
    fn product_label_turns_common_slugs_readable() {
        assert_eq!(
            product_label("t_2__ens_mean"),
            "2 m temperature (ensemble mean)"
        );
        assert_eq!(
            product_label("comp_dz__paintballs_thresh_40"),
            "Composite reflectivity (paintballs >=40 dBZ)"
        );
        assert_eq!(
            product_label("uh_2to5__prob_thresh_75"),
            "2-5 km updraft helicity (probability >=75)"
        );
    }

    #[test]
    fn picker_drops_announced_cycles_until_first_posted_frame() {
        let run = WofsRun {
            id: "WOFSRun20260714-test".to_owned(),
            name: "Test domain".to_owned(),
            rundate: "20260714".to_owned(),
            inits: vec![
                "202607142100".to_owned(),
                "202607142030".to_owned(),
                "202607142000".to_owned(),
            ],
        };
        let mut probes = Vec::new();
        let filtered = retain_posted_inits(run, |url| {
            probes.push(url.to_owned());
            Ok(url.contains("/2030/"))
        })
        .unwrap()
        .expect("an older posted cycle should keep the run");

        assert_eq!(
            filtered.inits,
            ["202607142030".to_owned(), "202607142000".to_owned()]
        );
        assert_eq!(probes.len(), 2, "stop probing after the first posted init");
        assert!(probes[0].ends_with("_f000.png"));
    }

    #[test]
    fn picker_omits_run_when_no_cycle_has_a_posted_frame() {
        let run = WofsRun {
            id: "WOFSRun20260715-unposted".to_owned(),
            name: "Future domain".to_owned(),
            rundate: "20260715".to_owned(),
            inits: vec!["202607150500".to_owned()],
        };
        assert!(
            retain_posted_inits(run, |_| Ok(false)).unwrap().is_none(),
            "an API-only run must not appear loadable"
        );
    }

    #[test]
    fn posted_edge_probe_clamps_the_theoretical_time_grid() {
        let run = WofsRun {
            id: "WOFSRun20260714-live".to_owned(),
            name: "Live domain".to_owned(),
            rundate: "20260714".to_owned(),
            inits: vec!["202607142100".to_owned()],
        };
        let candidates = (0..=360).step_by(5).collect::<Vec<_>>();
        let mut probes = 0usize;
        let edge = latest_posted_minute(&run, &run.inits[0], SND_REF_PRODUCT, &candidates, |url| {
            probes += 1;
            let minute = url
                .rsplit_once("_f")
                .and_then(|(_, suffix)| suffix.strip_suffix(".png"))
                .and_then(|value| value.parse::<u32>().ok())
                .unwrap();
            Ok(minute <= 215)
        })
        .unwrap();

        assert_eq!(edge, 215);
        assert!(probes <= 8, "binary probe should remain cheap: {probes}");
    }

    #[test]
    fn live_posted_edge_clamps_a_minute_carried_from_an_older_run() {
        let run = WofsRun {
            id: "WOFSRun20260714-live".to_owned(),
            name: "Live domain".to_owned(),
            rundate: "20260714".to_owned(),
            inits: vec!["202607142100".to_owned()],
        };
        let product = SND_REF_PRODUCT.to_owned();
        let key = availability_key(&run, &run.inits[0], &product);
        let mut state = WofsState {
            catalog: Some(WofsCatalog {
                runs: vec![run.clone()],
                groups: Vec::new(),
                times: HashMap::from([(
                    product.clone(),
                    (0..=360).step_by(5).map(|minute| minute * 60).collect(),
                )]),
            }),
            init: run.inits[0].clone(),
            product,
            minute: 360,
            ..WofsState::default()
        };
        state.max_posted_minutes.insert(key, (215, Instant::now()));

        assert!(state.clamp_minute_to_posted_edge());
        assert_eq!(state.minute, 215);
    }

    #[test]
    fn base_product_groups_exclude_missing_and_overlay_only_images() {
        let mut groups = vec![
            (
                "Thermodynamics".to_owned(),
                vec!["t_2__ens_mean".to_owned(), "missing".to_owned()],
            ),
            (
                "Overlays".to_owned(),
                vec!["comp_dz_overlay__paintballs_thresh_40".to_owned()],
            ),
        ];
        let times = HashMap::from([
            ("t_2__ens_mean".to_owned(), vec![0, 300]),
            (
                "comp_dz_overlay__paintballs_thresh_40".to_owned(),
                vec![0, 300],
            ),
        ]);

        retain_available_base_products(&mut groups, &times);

        assert_eq!(
            groups,
            vec![(
                "Thermodynamics".to_owned(),
                vec!["t_2__ens_mean".to_owned()]
            )]
        );
    }

    #[test]
    fn failed_georef_can_be_retried_without_restarting() {
        let mut state = WofsState::default();
        let run_id = "WOFSRun20260711-130130d1";
        state
            .georef_failed
            .insert(run_id.to_owned(), "temporary CDN timeout".to_owned());
        state.retry_georef_for_run(run_id);
        assert!(!state.georef_failed.contains_key(run_id));
    }

    fn selected_test_drape() -> (WofsState, WofsRun, String) {
        let run = WofsRun {
            id: "WOFSRun20260715-test".to_owned(),
            name: "Test domain".to_owned(),
            rundate: "20260715".to_owned(),
            inits: vec!["202607152100".to_owned()],
        };
        let product = SND_REF_PRODUCT.to_owned();
        let state = WofsState {
            catalog: Some(WofsCatalog {
                runs: vec![run.clone()],
                groups: Vec::new(),
                times: HashMap::from([(product.clone(), vec![0, 300])]),
            }),
            init: run.inits[0].clone(),
            product: product.clone(),
            minute: 0,
            drape_on_map: true,
            ..WofsState::default()
        };
        (state, run, product)
    }

    #[test]
    fn drape_defaults_lift_low_alpha_but_keep_an_explicit_standard_mode() {
        let mut state = WofsState::default();
        assert_eq!(state.drape_opacity, DEFAULT_DRAPE_OPACITY);
        assert!(state.drape_high_visibility);
        assert_eq!(state.drape_passes(), HIGH_VISIBILITY_PASSES);

        state.drape_high_visibility = false;
        assert_eq!(state.drape_passes(), 1);
    }

    #[test]
    fn unavailable_selection_never_reports_a_ready_map_layer() {
        let (mut state, run, product) = selected_test_drape();
        let key = availability_key(&run, &state.init, &product);

        assert_eq!(
            state.drape_readiness(),
            WofsDrapeReadiness::CheckingAvailability
        );
        state.availability_failed.insert(
            key,
            (
                Instant::now(),
                "product has no posted analysis frame for this run/cycle".to_owned(),
            ),
        );
        let readiness = state.drape_readiness();
        assert_eq!(readiness, WofsDrapeReadiness::FrameUnavailable);
        assert_eq!(readiness.rail_state(), "unavailable");
    }

    #[test]
    fn failed_availability_check_is_not_mislabeled_as_proved_unavailable() {
        let (mut state, run, product) = selected_test_drape();
        let key = availability_key(&run, &state.init, &product);
        state
            .availability_failed
            .insert(key, (Instant::now(), "request timed out".to_owned()));

        let readiness = state.drape_readiness();
        assert_eq!(readiness, WofsDrapeReadiness::AvailabilityError);
        assert_eq!(readiness.rail_state(), "error");
    }

    #[test]
    fn drape_readiness_requires_both_current_texture_and_georef() {
        let (mut state, run, product) = selected_test_drape();
        let key = availability_key(&run, &state.init, &product);
        state.max_posted_minutes.insert(key, (0, Instant::now()));
        assert_eq!(state.drape_readiness(), WofsDrapeReadiness::LoadingFrame);

        let ctx = egui::Context::default();
        let base_url = image_url(&run, &state.init, &product, 0);
        let texture = ctx.load_texture(
            "wofs-readiness-test",
            egui::ColorImage::filled(
                [PRODUCT_PNG_SIZE.0 as usize, PRODUCT_PNG_SIZE.1 as usize],
                egui::Color32::TRANSPARENT,
            ),
            egui::TextureOptions::NEAREST,
        );
        state.textures.insert(base_url, texture);
        assert_eq!(
            state.drape_readiness(),
            WofsDrapeReadiness::CalibratingGeoref
        );

        state.georef_cache.insert(
            run.id.clone(),
            Arc::new(WofsGeoref::from_coeffs(
                [37.2, 0.3, 7.95, 0.0, -0.3, 0.0],
                [-94.4, 10.1, 0.05, 0.0, 0.9, 0.0],
                20,
                20,
                0.05,
                0.003,
            )),
        );
        assert_eq!(state.drape_readiness(), WofsDrapeReadiness::Ready);
    }

    /// Live round-trip against the running WoFS: catalog -> station
    /// lattice -> sounding PNG fetch + decode. Network test, run with
    /// --ignored. Validated 2026-06-11 against WOFSRun20260611-144912d1
    /// (rd 20260611, init 1700): 400 stations, sounding PNGs 1200x800,
    /// frames 0..=72 exist on the 5-min grid.
    #[test]
    #[ignore]
    fn live_wofs_sounding_roundtrip() {
        let catalog = fetch_catalog().expect("catalog");
        let run = catalog.runs.first().expect("no runs");
        // Newest init first; fall back to the oldest (posted hours ago)
        // if the sounding pipeline lags the imagery probe.
        let mut inits: Vec<&String> = Vec::new();
        inits.extend(run.inits.first());
        inits.extend(run.inits.last());
        inits.dedup();
        let mut ok = false;
        for init in inits {
            let stations = match fetch_stations(&run.id, &run.rundate, init) {
                Ok(s) => s,
                Err(e) => {
                    println!("{} {init}z stations: {e}", run.id);
                    continue;
                }
            };
            println!("{} {init}z: {} stations", run.id, stations.len());
            assert!(stations.len() >= 100, "expected a dense lattice");
            for st in &stations {
                assert!(st.x > 0.0 && st.x < 1.0, "{}: x={}", st.id, st.x);
                assert!(st.y > 0.0 && st.y < 1.0, "{}: y={}", st.id, st.y);
                let (u, v) = st.image_uv();
                assert!(u > 0.0 && u < 1.0 && v > 0.0 && v < 1.0);
            }
            let mid = &stations[stations.len() / 2];
            let url = sounding_url(run, init, 0, &mid.id);
            match fetch_image(&url) {
                Ok(image) => {
                    println!("{} frame 0: {}x{}", mid.id, image.size[0], image.size[1]);
                    assert_eq!(image.size, [1200, 800], "sounding PNG dimensions");
                    ok = true;
                }
                Err(e) => {
                    println!("{url}: {e}");
                    continue;
                }
            }
            // A later frame steps the valid time (not asserted hard: the
            // newest init may not have posted it yet).
            let later = sounding_url(run, init, 12, &mid.id);
            match fetch_image(&later) {
                Ok(image) => println!("{} frame 12: {}x{}", mid.id, image.size[0], image.size[1]),
                Err(e) => println!("frame 12 not posted yet: {e}"),
            }
            break;
        }
        assert!(ok, "no init produced a decodable sounding PNG");
    }
}
