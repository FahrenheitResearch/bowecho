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

#[derive(Clone)]
pub struct WofsRun {
    pub id: String,
    pub name: String,
    pub rundate: String,
    /// Init times as "HHMM", newest first.
    pub inits: Vec<String>,
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
                                .map(|rt| rt[rt.len().saturating_sub(4)..].to_owned())
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
    // Live runs appear in the catalog BEFORE their imagery posts (field
    // report: newest init 404'd). Demote inits whose f000 isn't up yet.
    if let Some(newest) = runs.first_mut() {
        let mut keep = newest.inits.clone();
        while keep.len() > 1 {
            let probe = format!(
                "{CDN}/{}/{}/{}/img/comp_dz__paintballs_thresh_40_f000.png",
                newest.id, newest.rundate, keep[0]
            );
            if data_source::fetch_bytes(&probe).is_ok() {
                break;
            }
            keep.remove(0);
        }
        newest.inits = keep;
    }
    // Menu tree + per-product time grids for the newest run.
    let newest = &runs[0];
    let init = newest.inits.first().cloned().unwrap_or("1700".to_owned());
    let query = format!(
        "model={}&rd={}&rt={}{}&product=t_2__ens_mean&sector=wofs",
        newest.id, newest.rundate, newest.rundate, init
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
    Ok(WofsCatalog {
        runs,
        groups,
        times,
    })
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
    format!(
        "{CDN}/{}/{}/{}/img/{}_f{minute:03}.png",
        run.id, run.rundate, init, product
    )
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
    let url = format!(
        "{API}/stations?model={run_id}&rd={rundate}&rt={rundate}{init}&product={SND_REF_PRODUCT}&sector=wofs&type=stations"
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
    format!(
        "{SND_CDN}/{}/{}{init}/{frame}/wofs_snd_{station}.png",
        run.id, run.rundate
    )
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
    /// Georef cache by RUN id (the domain is per-run, not per-frame).
    pub georef_cache: HashMap<String, Arc<WofsGeoref>>,
    /// Runs whose calibration failed (message) — drape disabled for them.
    pub georef_failed: HashMap<String, String>,
    /// In-flight calibration: (run id, result channel).
    pub georef_rx: Option<(String, mpsc::Receiver<Result<WofsGeoref, String>>)>,
    /// Stations processed by the in-flight calibration (status line).
    pub georef_progress: Arc<AtomicUsize>,
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
            status: String::new(),
            soundings_mode: false,
            stations: Vec::new(),
            stations_key: String::new(),
            stations_rx: None,
            stations_failed: None,
            selected_station: None,
            snd_frame: None,
            drape_on_map: false,
            drape_opacity: 0.7,
            georef_cache: HashMap::new(),
            georef_failed: HashMap::new(),
            georef_rx: None,
            georef_progress: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl WofsState {
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
        if let Some(rx) = &self.image_rx {
            match rx.try_recv() {
                Ok((url, Ok(image))) => {
                    self.image_rx = None;
                    self.pending_urls.retain(|u| u != &url);
                    if self.textures.len() > 72 {
                        self.textures.clear(); // simple bound; refetch is cheap
                    }
                    let handle = ctx.load_texture(url.clone(), image, egui::TextureOptions::LINEAR);
                    self.textures.insert(url, handle);
                }
                Ok((url, Err(e))) => {
                    self.image_rx = None;
                    self.pending_urls.retain(|u| u != &url);
                    // Short, layout-safe status: live runs post imagery a
                    // few minutes behind the catalog — a 404 just means
                    // "not yet" (the sounding service says 500 for the
                    // same thing: frames past the posted edge).
                    self.status = if e.contains("404") || e.contains("500") {
                        "frame not posted yet (live run) — retrying shortly".to_owned()
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
            let rd_init = format!("{}{}", run.rundate, self.init);
            let ctx_clone = ctx.clone();
            thread::spawn(move || {
                let result = wofs_georef::build_georef(&run_id, &rd_init, Some(&progress));
                let _ = tx.send(result);
                ctx_clone.request_repaint();
            });
        }
    }

    /// "Show on map" toggle + opacity + calibration status, rendered inside
    /// the WoFS window.
    pub fn drape_controls_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.drape_on_map, "Show on map")
                .on_hover_text(
                    "Drape the current product onto the radar map. The georeference is \
                 calibrated per run by OCR-ing ~20 sounding-PNG titles (the stations' \
                 true lat/lons) and bilinear-fitting the domain.",
                );
            if !self.drape_on_map {
                return;
            }
            ui.add(
                egui::Slider::new(&mut self.drape_opacity, 0.05..=1.0)
                    .text("opacity")
                    .show_value(false),
            );
            let Some(run_id) = self
                .catalog
                .as_ref()
                .and_then(|c| c.runs.get(self.run_index))
                .map(|r| r.id.clone())
            else {
                return;
            };
            if let Some(georef) = self.georef_cache.get(&run_id) {
                ui.weak(format!(
                    "georef OK · {} lat / {} lon inliers · max resid {:.2}/{:.2}°",
                    georef.lat_inliers,
                    georef.lon_inliers,
                    georef.lat_max_resid,
                    georef.lon_max_resid
                ));
            } else if let Some(error) = self.georef_failed.get(&run_id) {
                ui.colored_label(
                    egui::Color32::from_rgb(230, 150, 90),
                    format!("drape disabled for this run: {error}"),
                );
            } else if self.georef_rx.as_ref().is_some_and(|(id, _)| *id == run_id) {
                ui.spinner();
                ui.weak(format!(
                    "calibrating georef… {}/{} soundings",
                    self.georef_progress
                        .load(Ordering::Relaxed)
                        .min(wofs_georef::CALIBRATION_TOTAL),
                    wofs_georef::CALIBRATION_TOTAL
                ));
            } else {
                ui.weak("waiting for catalog…");
            }
        });
    }

    /// Drape the current product (and stacked overlays) onto the radar map
    /// as a textured mesh: vertices at `project(lonlat_of(u, v))`, UVs into
    /// the axes-box subrect of the already-fetched window texture.
    ///
    /// Gated on the window being OPEN: pump + radar-time sync only run
    /// while the window shows, so a drape that outlived the window would
    /// silently freeze at a stale forecast minute as the user scrubs.
    pub fn draw_drape(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        project: &dyn Fn(f32, f32) -> egui::Pos2,
    ) {
        if !self.drape_on_map || !self.open {
            return;
        }
        let Some(catalog) = &self.catalog else {
            return;
        };
        let Some(run) = catalog.runs.get(self.run_index) else {
            return;
        };
        let Some(georef) = self.georef_cache.get(&run.id) else {
            return;
        };
        let mut urls = vec![image_url(run, &self.init, &self.product, self.minute)];
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
                painter.add(egui::Shape::mesh(mesh));
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
                    egui::Stroke::new(1.5, egui::Color32::WHITE),
                );
            } else if hovered {
                painter.circle(
                    *pos,
                    4.0,
                    egui::Color32::from_rgb(36, 92, 214),
                    egui::Stroke::new(1.5, egui::Color32::WHITE),
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
