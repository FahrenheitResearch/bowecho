//! Raster tile basemaps (satellite / streets / topo) drawn beneath the radar.
//!
//! Web-Mercator XYZ tiles are fetched on background threads (with an on-disk
//! cache), decoded off-thread, and drawn as textured quads whose corners are
//! projected through the app's AEQD transform — so tiles warp correctly into
//! the radar frame with zero per-pixel work. The UI thread never blocks on
//! the network: missing tiles simply leave the dark background until they
//! arrive (newest-request-first queue, LRU texture eviction).

use eframe::egui;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TileId {
    pub zoom: u8,
    pub x: u32,
    pub y: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TileStyle {
    DarkVector,
    Satellite,
    Streets,
    Topo,
}

impl TileStyle {
    pub fn from_key(key: &str) -> Self {
        match key {
            "satellite" => Self::Satellite,
            "streets" => Self::Streets,
            "topo" => Self::Topo,
            _ => Self::DarkVector,
        }
    }

    pub fn key(&self) -> &'static str {
        match self {
            Self::DarkVector => "dark",
            Self::Satellite => "satellite",
            Self::Streets => "streets",
            Self::Topo => "topo",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::DarkVector => "Dark (vector)",
            Self::Satellite => "Satellite",
            Self::Streets => "Streets",
            Self::Topo => "Topo",
        }
    }

    fn url(&self, tile: TileId) -> Option<String> {
        let service = match self {
            Self::DarkVector => return None,
            Self::Satellite => "World_Imagery",
            Self::Streets => "World_Street_Map",
            Self::Topo => "World_Topo_Map",
        };
        Some(format!(
            "https://server.arcgisonline.com/ArcGIS/rest/services/{service}/MapServer/tile/{}/{}/{}",
            tile.zoom, tile.y, tile.x
        ))
    }

    pub fn attribution(&self) -> Option<&'static str> {
        match self {
            Self::DarkVector => None,
            Self::Satellite => Some("Imagery © Esri, Maxar, Earthstar Geographics"),
            Self::Streets | Self::Topo => Some("Map tiles © Esri and contributors"),
        }
    }

    pub const ALL: [TileStyle; 4] = [Self::DarkVector, Self::Satellite, Self::Streets, Self::Topo];
}

/// lon/lat → fractional tile coordinates at `zoom` (Web Mercator).
pub fn tile_coords(lon: f64, lat: f64, zoom: u8) -> (f64, f64) {
    let n = (1u32 << zoom) as f64;
    let x = (lon + 180.0) / 360.0 * n;
    let lat = lat.clamp(-85.05112878, 85.05112878).to_radians();
    let y = (1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0 * n;
    (x, y)
}

/// Tile corner (x, y in tile units) → lon/lat.
pub fn tile_corner_lon_lat(x: f64, y: f64, zoom: u8) -> (f64, f64) {
    let n = (1u32 << zoom) as f64;
    let lon = x / n * 360.0 - 180.0;
    let lat = (std::f64::consts::PI * (1.0 - 2.0 * y / n))
        .sinh()
        .atan()
        .to_degrees();
    (lon, lat)
}

/// Pick the tile zoom whose ground resolution best matches the view.
pub fn zoom_for_km_per_px(km_per_px: f32, center_lat: f32, pixels_per_point: f32) -> u8 {
    let meters_per_px = (km_per_px * 1000.0 / pixels_per_point.max(0.5)) as f64;
    let equator_m_per_px = 156_543.033_92 * (center_lat as f64).to_radians().cos().max(0.05);
    let zoom = (equator_m_per_px / meters_per_px.max(1.0)).log2().round();
    zoom.clamp(2.0, 16.0) as u8
}

type DecodedTile = (TileId, u32, u32, Vec<u8>);

/// Background fetch pool + texture cache. One instance per app.
pub struct TileLayer {
    textures: HashMap<(u8, TileId), egui::TextureHandle>,
    lru: VecDeque<(u8, TileId)>,
    pending: HashSet<(u8, TileId)>,
    failed: HashMap<(u8, TileId), std::time::Instant>,
    queue: Arc<Mutex<VecDeque<(TileStyle, TileId)>>>,
    workers: Arc<AtomicUsize>,
    tx: mpsc::Sender<(u8, DecodedTile)>,
    rx: mpsc::Receiver<(u8, DecodedTile)>,
    cache_dir: Option<PathBuf>,
}

const MAX_TEXTURES: usize = 220;
const MAX_WORKERS: usize = 4;
const FAILURE_RETRY_SECS: u64 = 60;

fn style_slot(style: TileStyle) -> u8 {
    match style {
        TileStyle::DarkVector => 0,
        TileStyle::Satellite => 1,
        TileStyle::Streets => 2,
        TileStyle::Topo => 3,
    }
}

impl TileLayer {
    pub fn new(cache_dir: Option<PathBuf>) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            textures: HashMap::new(),
            lru: VecDeque::new(),
            pending: HashSet::new(),
            failed: HashMap::new(),
            queue: Arc::new(Mutex::new(VecDeque::new())),
            workers: Arc::new(AtomicUsize::new(0)),
            tx,
            rx,
            cache_dir,
        }
    }

    /// Install decoded tiles arriving from workers. Returns true if any new
    /// texture landed (callers repaint).
    pub fn poll(&mut self, ctx: &egui::Context) -> bool {
        let mut installed = false;
        while let Ok((slot, (tile, width, height, rgba))) = self.rx.try_recv() {
            self.pending.remove(&(slot, tile));
            if rgba.is_empty() {
                self.failed.insert((slot, tile), std::time::Instant::now());
                continue;
            }
            let image =
                egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], &rgba);
            let texture = ctx.load_texture(
                format!("tile-{slot}-{}-{}-{}", tile.zoom, tile.x, tile.y),
                image,
                egui::TextureOptions::LINEAR,
            );
            let key = (slot, tile);
            if self.textures.insert(key, texture).is_none() {
                self.lru.push_back(key);
            }
            installed = true;
        }
        while self.lru.len() > MAX_TEXTURES {
            if let Some(old) = self.lru.pop_front() {
                self.textures.remove(&old);
            }
        }
        installed
    }

    pub fn texture(&mut self, style: TileStyle, tile: TileId) -> Option<&egui::TextureHandle> {
        let key = (style_slot(style), tile);
        if self.textures.contains_key(&key) {
            // Touch for LRU.
            if let Some(index) = self.lru.iter().position(|entry| *entry == key) {
                let entry = self.lru.remove(index).expect("lru entry");
                self.lru.push_back(entry);
            }
            return self.textures.get(&key);
        }
        None
    }

    /// Drop in-memory tile state after the on-disk tile cache is cleared.
    pub fn clear_memory(&mut self) {
        self.textures.clear();
        self.lru.clear();
        self.pending.clear();
        self.failed.clear();
        if let Ok(mut queue) = self.queue.lock() {
            queue.clear();
        }
    }

    /// Queue a fetch for a missing tile (newest requests first).
    pub fn request(&mut self, style: TileStyle, tile: TileId) {
        if std::env::var_os("BOWECHO_TILE_DEBUG").is_some() {
            eprintln!(
                "TILE REQUEST: {}/{}/{}/{}",
                style.key(),
                tile.zoom,
                tile.x,
                tile.y
            );
        }
        let key = (style_slot(style), tile);
        if self.textures.contains_key(&key) || self.pending.contains(&key) {
            return;
        }
        if let Some(failed_at) = self.failed.get(&key) {
            if failed_at.elapsed().as_secs() < FAILURE_RETRY_SECS {
                return;
            }
            self.failed.remove(&key);
        }
        self.pending.insert(key);
        {
            let mut queue = self.queue.lock().expect("tile queue");
            queue.push_front((style, tile));
            while queue.len() > 96 {
                if let Some((style, tile)) = queue.pop_back() {
                    self.pending.remove(&(style_slot(style), tile));
                }
            }
        }
        self.spawn_workers();
    }

    fn spawn_workers(&self) {
        while self.workers.load(Ordering::Relaxed) < MAX_WORKERS {
            let queue = Arc::clone(&self.queue);
            let tx = self.tx.clone();
            let cache_dir = self.cache_dir.clone();
            self.workers.fetch_add(1, Ordering::Relaxed);
            let workers = Arc::clone(&self.workers);
            std::thread::spawn(move || {
                if std::env::var_os("BOWECHO_TILE_DEBUG").is_some() {
                    eprintln!("TILE WORKER START");
                }
                loop {
                    let job = queue.lock().expect("tile queue").pop_front();
                    let Some((style, tile)) = job else {
                        break;
                    };
                    let slot = style_slot(style);
                    let decoded = fetch_and_decode(style, tile, cache_dir.as_deref());
                    if std::env::var_os("BOWECHO_TILE_DEBUG").is_some() {
                        eprintln!(
                            "TILE WORKER: {}/{}/{}/{} -> {}",
                            style.key(),
                            tile.zoom,
                            tile.x,
                            tile.y,
                            if decoded.is_some() { "ok" } else { "FAILED" }
                        );
                    }
                    let payload = decoded.unwrap_or((tile, 0, 0, Vec::new()));
                    if tx.send((slot, payload)).is_err() {
                        break;
                    }
                }
                workers.fetch_sub(1, Ordering::Relaxed);
            });
        }
    }
}

fn fetch_and_decode(
    style: TileStyle,
    tile: TileId,
    cache_dir: Option<&std::path::Path>,
) -> Option<DecodedTile> {
    let cache_path = cache_dir.map(|dir| {
        dir.join(style.key())
            .join(tile.zoom.to_string())
            .join(format!("{}_{}.bin", tile.x, tile.y))
    });
    let bytes = if let Some(path) = cache_path.as_ref().filter(|p| p.exists()) {
        std::fs::read(path).ok()?
    } else {
        let url = style.url(tile)?;
        let bytes = data_source::fetch_bytes(&url).ok()?;
        if let Some(path) = &cache_path {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(path, &bytes);
        }
        bytes
    };
    let image = image::load_from_memory(&bytes).ok()?;
    let rgba = image.to_rgba8();
    Some((tile, rgba.width(), rgba.height(), rgba.into_raw()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_math_round_trips() {
        let (x, y) = tile_coords(-94.6, 39.0, 10);
        let (lon, lat) = tile_corner_lon_lat(x, y, 10);
        assert!((lon + 94.6).abs() < 1e-9 && (lat - 39.0).abs() < 1e-9);
    }

    #[test]
    fn zoom_tracks_scale() {
        // County-level radar zoom should land in the 9-12 range.
        let zoom = zoom_for_km_per_px(0.25, 39.0, 1.0);
        assert!((9..=12).contains(&zoom), "{zoom}");
        // Continental overview should be small.
        let zoom = zoom_for_km_per_px(8.0, 39.0, 1.0);
        assert!(zoom <= 6, "{zoom}");
    }
}
