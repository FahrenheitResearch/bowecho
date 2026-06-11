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
use std::collections::HashMap;
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
        }
    }
}

impl FarmState {
    pub fn live_sensor(&self) -> Option<&FarmSensor> {
        self.sensors.iter().find(|s| s.is_live())
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
}
