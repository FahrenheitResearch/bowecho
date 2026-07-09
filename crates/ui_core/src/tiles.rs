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
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TileId {
    pub zoom: u8,
    pub x: u32,
    pub y: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TileSource {
    Basemap(TileStyle),
}

impl TileSource {
    pub fn stable_source_zoom(&self) -> Option<u8> {
        // Scientific gridded products must not ride this presentation-tile
        // pathway. Basemaps intentionally follow the viewport zoom.
        None
    }

    pub fn geo_clip_bounds(&self) -> Option<(f32, f32, f32, f32)> {
        None
    }

    fn key(&self) -> String {
        match self {
            Self::Basemap(style) => style.key().to_owned(),
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Basemap(style) => style.key().to_owned(),
        }
    }

    fn url(&self, tile: TileId) -> Option<String> {
        match self {
            Self::Basemap(style) => style.url(tile),
        }
    }
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
type TileCacheKey = (String, TileId);

struct TileEntry {
    texture: egui::TextureHandle,
    last_used: u64,
}

struct TileQueue {
    jobs: Mutex<VecDeque<(TileSource, TileId)>>,
    wake: Condvar,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TilePollStats {
    pub processed: usize,
    pub installed: usize,
    pub failed: usize,
    pub deferred: bool,
    pub elapsed_ms: f32,
}

/// Per-app tile-layer knobs (miniderecho-spec §13 Task 1.3): the disk-cache
/// directory, texture/worker budgets, and the debug env-var name were
/// hardcoded BowEcho values before the ui_core extraction; each app now
/// injects its own (BowEcho passes exactly the old values — zero behavior
/// change).
#[derive(Clone, Debug)]
pub struct TileLayerConfig {
    /// On-disk tile cache root; `None` disables the disk cache.
    pub cache_dir: Option<PathBuf>,
    /// GPU texture LRU budget (BowEcho: 220).
    pub max_textures: usize,
    /// Fetch/decode worker pool size (BowEcho: 4).
    pub max_workers: usize,
    /// Env var that turns on tile request/worker logging
    /// (BowEcho: `BOWECHO_TILE_DEBUG`).
    pub debug_env: &'static str,
}

/// Background fetch pool + texture cache. One instance per app.
pub struct TileLayer {
    textures: HashMap<TileCacheKey, TileEntry>,
    usage_generation: u64,
    pending: HashSet<TileCacheKey>,
    failed: HashMap<TileCacheKey, std::time::Instant>,
    queue: Arc<TileQueue>,
    workers: Arc<AtomicUsize>,
    tx: mpsc::Sender<(String, DecodedTile)>,
    rx: mpsc::Receiver<(String, DecodedTile)>,
    config: TileLayerConfig,
    last_poll_stats: TilePollStats,
}

const FAILURE_RETRY_SECS: u64 = 60;
const MAX_TEXTURE_INSTALLS_PER_POLL: usize = 6;
const MAX_TILE_RESULTS_PER_POLL: usize = 24;
const TEXTURE_INSTALL_BUDGET: std::time::Duration = std::time::Duration::from_millis(2);
/// Encoded Esri tiles are normally tens to hundreds of KiB. This generous
/// ceiling bounds both stale/corrupt cache files and future fetch helpers
/// that may allow larger generic response bodies.
const MAX_TILE_ENCODED_BYTES: usize = 4 * 1024 * 1024;
const MAX_TILE_DIMENSION: u32 = 4096;
const MAX_TILE_PIXELS: u64 = 4 * 1024 * 1024;
/// Every provider currently represented by [`TileSource`] serves standard
/// 256x256 Web-Mercator tiles. Reject any other geometry before the image
/// crate allocates its decoded pixel buffer.
const BASEMAP_TILE_DIMENSION: u32 = 256;

impl TileLayer {
    pub fn new(config: TileLayerConfig) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            textures: HashMap::new(),
            usage_generation: 0,
            pending: HashSet::new(),
            failed: HashMap::new(),
            queue: Arc::new(TileQueue {
                jobs: Mutex::new(VecDeque::new()),
                wake: Condvar::new(),
            }),
            workers: Arc::new(AtomicUsize::new(0)),
            tx,
            rx,
            config,
            last_poll_stats: TilePollStats::default(),
        }
    }

    /// Install decoded tiles arriving from workers. Returns true if any new
    /// texture landed (callers repaint).
    pub fn poll(&mut self, ctx: &egui::Context) -> bool {
        self.poll_with_stats(ctx).installed > 0
    }

    pub fn last_poll_stats(&self) -> TilePollStats {
        self.last_poll_stats
    }

    fn poll_with_stats(&mut self, ctx: &egui::Context) -> TilePollStats {
        let mut stats = TilePollStats::default();
        let started_at = std::time::Instant::now();
        loop {
            if tile_poll_budget_exhausted(stats.processed, stats.installed, started_at.elapsed()) {
                stats.deferred = true;
                ctx.request_repaint();
                break;
            }
            let Ok((source_key, (tile, width, height, rgba))) = self.rx.try_recv() else {
                break;
            };
            stats.processed += 1;
            self.pending.remove(&(source_key.clone(), tile));
            if rgba.is_empty() {
                self.failed
                    .insert((source_key, tile), std::time::Instant::now());
                stats.failed += 1;
                continue;
            }
            let image =
                egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], &rgba);
            let texture = ctx.load_texture(
                format!("tile-{source_key}-{}-{}-{}", tile.zoom, tile.x, tile.y),
                image,
                egui::TextureOptions::LINEAR,
            );
            let key = (source_key, tile);
            let last_used = self.next_usage_generation();
            self.textures.insert(key, TileEntry { texture, last_used });
            stats.installed += 1;
        }
        self.trim_textures();
        stats.elapsed_ms = started_at.elapsed().as_secs_f32() * 1000.0;
        self.last_poll_stats = stats;
        stats
    }

    pub fn texture_source(
        &mut self,
        source: &TileSource,
        tile: TileId,
    ) -> Option<&egui::TextureHandle> {
        let key = (source.key(), tile);
        let last_used = self.next_usage_generation();
        self.textures.get_mut(&key).map(|entry| {
            entry.last_used = last_used;
            &entry.texture
        })
    }

    /// Drop in-memory tile state after the on-disk tile cache is cleared.
    pub fn clear_memory(&mut self) {
        self.textures.clear();
        self.usage_generation = 0;
        self.pending.clear();
        self.failed.clear();
        if let Ok(mut queue) = self.queue.jobs.lock() {
            queue.clear();
        }
        self.last_poll_stats = TilePollStats::default();
    }

    pub fn request_source(&mut self, source: TileSource, tile: TileId) {
        if std::env::var_os(self.config.debug_env).is_some() {
            eprintln!(
                "TILE REQUEST: {}/{}/{}/{}",
                source.label(),
                tile.zoom,
                tile.x,
                tile.y
            );
        }
        let key = (source.key(), tile);
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
            let mut queue = self.queue.jobs.lock().expect("tile queue");
            queue.push_front((source, tile));
            while queue.len() > 96 {
                if let Some((source, tile)) = queue.pop_back() {
                    self.pending.remove(&(source.key(), tile));
                }
            }
        }
        self.spawn_workers();
        self.queue.wake.notify_all();
    }

    fn spawn_workers(&self) {
        while self.workers.load(Ordering::Relaxed) < self.config.max_workers {
            let queue = Arc::clone(&self.queue);
            let tx = self.tx.clone();
            let cache_dir = self.config.cache_dir.clone();
            let debug_env = self.config.debug_env;
            self.workers.fetch_add(1, Ordering::Relaxed);
            let workers = Arc::clone(&self.workers);
            std::thread::spawn(move || {
                if std::env::var_os(debug_env).is_some() {
                    eprintln!("TILE WORKER START");
                }
                loop {
                    let (source, tile) = {
                        let mut guard = queue.jobs.lock().expect("tile queue");
                        loop {
                            if let Some(job) = guard.pop_front() {
                                break job;
                            }
                            guard = queue.wake.wait(guard).expect("tile queue wake");
                        }
                    };
                    let source_key = source.key();
                    let source_label = source.label();
                    let decoded = fetch_and_decode(&source, tile, cache_dir.as_deref());
                    if std::env::var_os(debug_env).is_some() {
                        eprintln!(
                            "TILE WORKER: {}/{}/{}/{} -> {}",
                            source_label,
                            tile.zoom,
                            tile.x,
                            tile.y,
                            if decoded.is_some() { "ok" } else { "FAILED" }
                        );
                    }
                    let payload = decoded.unwrap_or((tile, 0, 0, Vec::new()));
                    if tx.send((source_key, payload)).is_err() {
                        break;
                    }
                }
                // Workers are normally persistent; this only runs if the UI
                // receiver disappears during shutdown or tests.
                workers.fetch_sub(1, Ordering::Relaxed);
            });
        }
    }

    fn next_usage_generation(&mut self) -> u64 {
        self.usage_generation = self.usage_generation.saturating_add(1);
        self.usage_generation
    }

    fn trim_textures(&mut self) {
        while self.textures.len() > self.config.max_textures {
            let Some(oldest) = self
                .textures
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.textures.remove(&oldest);
        }
    }
}

fn tile_poll_budget_exhausted(
    processed: usize,
    installed: usize,
    elapsed: std::time::Duration,
) -> bool {
    processed >= MAX_TILE_RESULTS_PER_POLL
        || installed >= MAX_TEXTURE_INSTALLS_PER_POLL
        || (installed > 0 && elapsed >= TEXTURE_INSTALL_BUDGET)
}

fn fetch_and_decode(
    source: &TileSource,
    tile: TileId,
    cache_dir: Option<&std::path::Path>,
) -> Option<DecodedTile> {
    let cache_path = cache_dir.map(|dir| {
        dir.join(source.key())
            .join(tile.zoom.to_string())
            .join(format!("{}_{}.bin", tile.x, tile.y))
    });
    if let Some(path) = cache_path.as_ref().filter(|path| path.exists())
        && let Some(decoded) = load_cached_tile(path, source, tile)
    {
        return Some(decoded);
    }

    let url = source.url(tile)?;
    let bytes = data_source::fetch_bytes(&url).ok()?;
    let decoded = decode_tile_bytes(source, tile, &bytes)?;
    // Cache only bytes that passed encoded-size, header-dimension, provider
    // geometry, full decode, and decoded-dimension checks. A bad response
    // must not poison every retry through the disk cache.
    if let Some(path) = &cache_path {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, &bytes);
    }
    Some(decoded)
}

fn load_cached_tile(path: &Path, source: &TileSource, tile: TileId) -> Option<DecodedTile> {
    let decoded =
        read_tile_file_limited(path).and_then(|bytes| decode_tile_bytes(source, tile, &bytes));
    if decoded.is_none() {
        // A truncated, oversized, or otherwise corrupt tile must not remain
        // a permanent retry sink. Ignore removal errors (read-only cache,
        // antivirus race, another process already replaced it) and let the
        // caller attempt a fresh provider fetch.
        let _ = std::fs::remove_file(path);
    }
    decoded
}

fn read_tile_file_limited(path: &Path) -> Option<Vec<u8>> {
    let mut file = std::fs::File::open(path).ok()?;
    let declared = usize::try_from(file.metadata().ok()?.len()).ok()?;
    if declared > MAX_TILE_ENCODED_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(declared).ok()?;
    let mut chunk = [0u8; 16 * 1024];
    loop {
        let remaining = MAX_TILE_ENCODED_BYTES.saturating_sub(bytes.len());
        if remaining == 0 {
            let mut probe = [0u8; 1];
            if file.read(&mut probe).ok()? != 0 {
                return None;
            }
            break;
        }
        let read_len = remaining.min(chunk.len());
        let count = file.read(&mut chunk[..read_len]).ok()?;
        if count == 0 {
            break;
        }
        bytes.try_reserve(count).ok()?;
        bytes.extend_from_slice(&chunk[..count]);
    }
    Some(bytes)
}

fn decode_tile_bytes(source: &TileSource, tile: TileId, bytes: &[u8]) -> Option<DecodedTile> {
    if bytes.is_empty() || bytes.len() > MAX_TILE_ENCODED_BYTES {
        return None;
    }
    let reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let (width, height) = reader.into_dimensions().ok()?;
    if !tile_dimensions_allowed(source, width, height) {
        return None;
    }

    // Dimension validation above intentionally precedes this allocation.
    let image = image::load_from_memory(bytes).ok()?;
    if image.width() != width || image.height() != height {
        return None;
    }
    let rgba = image.to_rgba8();
    Some((tile, rgba.width(), rgba.height(), rgba.into_raw()))
}

fn tile_dimensions_allowed(source: &TileSource, width: u32, height: u32) -> bool {
    let sane = width > 0
        && height > 0
        && width <= MAX_TILE_DIMENSION
        && height <= MAX_TILE_DIMENSION
        && u64::from(width)
            .checked_mul(u64::from(height))
            .is_some_and(|pixels| pixels <= MAX_TILE_PIXELS);
    sane && match source {
        TileSource::Basemap(_) => {
            width == BASEMAP_TILE_DIMENSION && height == BASEMAP_TILE_DIMENSION
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_tile() -> TileId {
        TileId {
            zoom: 5,
            x: 16,
            y: 11,
        }
    }

    fn satellite_source() -> TileSource {
        TileSource::Basemap(TileStyle::Satellite)
    }

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([1, 2, 3, 255]));
        let mut encoded = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .expect("encode test PNG");
        encoded.into_inner()
    }

    fn unique_cache_path(label: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "bowecho-ui-core-tile-{label}-{}-{nonce}.bin",
            std::process::id()
        ))
    }

    fn test_config() -> TileLayerConfig {
        TileLayerConfig {
            cache_dir: None,
            max_textures: 220,
            max_workers: 4,
            debug_env: "UI_CORE_TILE_DEBUG_TEST",
        }
    }

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

    #[test]
    fn tile_poll_budget_limits_texture_install_bursts() {
        assert!(!tile_poll_budget_exhausted(
            0,
            0,
            std::time::Duration::from_secs(10)
        ));
        assert!(tile_poll_budget_exhausted(
            MAX_TILE_RESULTS_PER_POLL,
            0,
            std::time::Duration::ZERO
        ));
        assert!(tile_poll_budget_exhausted(
            0,
            MAX_TEXTURE_INSTALLS_PER_POLL,
            std::time::Duration::ZERO
        ));
        assert!(tile_poll_budget_exhausted(1, 1, TEXTURE_INSTALL_BUDGET));
    }

    #[test]
    fn tile_usage_generation_is_monotonic_and_reset_on_clear() {
        let mut layer = TileLayer::new(test_config());

        assert_eq!(layer.next_usage_generation(), 1);
        assert_eq!(layer.next_usage_generation(), 2);
        layer.clear_memory();
        assert_eq!(layer.next_usage_generation(), 1);
    }

    #[test]
    fn tile_source_is_basemap_only_after_dpc_raw_path() {
        let basemap = TileSource::Basemap(TileStyle::Satellite);
        assert_eq!(basemap.stable_source_zoom(), None);
        assert_eq!(basemap.geo_clip_bounds(), None);
        assert!(
            basemap
                .url(TileId {
                    zoom: 5,
                    x: 16,
                    y: 11
                })
                .is_some()
        );
    }

    #[test]
    fn tile_dimensions_are_checked_before_decode_allocation() {
        let source = satellite_source();
        assert!(tile_dimensions_allowed(
            &source,
            BASEMAP_TILE_DIMENSION,
            BASEMAP_TILE_DIMENSION
        ));
        assert!(!tile_dimensions_allowed(&source, 1, 1));
        assert!(!tile_dimensions_allowed(
            &source,
            MAX_TILE_DIMENSION + 1,
            BASEMAP_TILE_DIMENSION
        ));

        assert!(decode_tile_bytes(&source, test_tile(), &png_bytes(1, 1)).is_none());
        let decoded = decode_tile_bytes(
            &source,
            test_tile(),
            &png_bytes(BASEMAP_TILE_DIMENSION, BASEMAP_TILE_DIMENSION),
        )
        .expect("standard provider tile");
        assert_eq!((decoded.1, decoded.2), (256, 256));
        assert_eq!(decoded.3.len(), 256 * 256 * 4);
    }

    #[test]
    fn corrupt_cached_tile_is_invalidated() {
        let path = unique_cache_path("corrupt");
        std::fs::write(&path, b"not an image").expect("write corrupt cache fixture");

        assert!(load_cached_tile(&path, &satellite_source(), test_tile()).is_none());
        assert!(!path.exists(), "corrupt cache entry should be removed");
    }

    #[test]
    fn oversized_cached_tile_is_rejected_without_reading_body() {
        let path = unique_cache_path("oversized");
        let file = std::fs::File::create(&path).expect("create oversized cache fixture");
        file.set_len((MAX_TILE_ENCODED_BYTES + 1) as u64)
            .expect("size oversized cache fixture");

        assert!(load_cached_tile(&path, &satellite_source(), test_tile()).is_none());
        assert!(!path.exists(), "oversized cache entry should be removed");
    }
}
