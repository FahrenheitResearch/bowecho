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
//! controls and a sync-to-radar-frame action. Map draping waits on the
//! daily LCC domain corners (not yet exposed anonymously).

use eframe::egui;
use std::collections::HashMap;
use std::sync::mpsc;
use std::thread;
use std::time::Instant;

const API: &str = "https://cbwofs.vlab.noaa.gov/Forecast";
const CDN: &str = "https://ep-wofs-postv2-dndma2fqexfhexfs.a01.azurefd.net/primary";
pub const CREDIT: &str =
    "WoFS data courtesy of the National Severe Storms Laboratory using federal funding";

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
                    // "not yet".
                    self.status = if e.contains("404") {
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
}
