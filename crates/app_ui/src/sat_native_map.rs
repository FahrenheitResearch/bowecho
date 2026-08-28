//! Native-resolution GOES map compositor.
//!
//! BowEcho's satellite player intentionally keeps a compact `.rws` preview.
//! The map is different: it may be zoomed far past that preview's stride, so
//! this module samples rw-sat's retained NetCDF archive through its bounded XYZ
//! renderer and drapes those tiles into BowEcho's azimuthal-equidistant view.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;
use image::RgbaImage;
use rw_sat::GoesAbiProduct;

use crate::aeqd_inverse_km;
use crate::sat_worker::{NativeSatMapSource, RemoteSatMapSource};

const WEB_MERCATOR_MAX_LAT: f64 = 85.051_128_78;
const WEB_MERCATOR_METERS_PER_PIXEL_Z0: f64 = 156_543.033_928_040_97;
const TILE_SIZE: u32 = rw_sat::DEFAULT_TILE_SIZE;
/// A typical zoomed-out CONUS viewport needs about 122 tiles. Keep one full
/// view plus headroom so a render does not evict its own center tiles before
/// it completes (256 RGBA 256px tiles are about 64 MiB).
const TILE_CACHE_CAPACITY: usize = 256;
const MAX_VIEW_TILES: usize = 512;
const PARTIAL_RASTER_MIN_INTERVAL: Duration = Duration::from_millis(125);
const LOCAL_TILE_WORKERS: usize = 4;
const REMOTE_TILE_WORKERS: usize = 8;

/// Cross-generation bound on active NetCDF/HTTP tile reads. A superseded
/// render cannot interrupt a decoder already inside one bounded window, but
/// it also must not leave another 4/8 active reads behind on every wheel tick.
struct TileLoadLimiter {
    available: Mutex<usize>,
    wake: Condvar,
}

impl TileLoadLimiter {
    fn new(limit: usize) -> Self {
        Self {
            available: Mutex::new(limit.max(1)),
            wake: Condvar::new(),
        }
    }

    fn acquire(&self, cancel: &AtomicBool) -> Option<TileLoadPermit<'_>> {
        let mut available = self
            .available
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            if *available > 0 {
                *available -= 1;
                return Some(TileLoadPermit { limiter: self });
            }
            let (next, _) = self
                .wake
                .wait_timeout(available, Duration::from_millis(20))
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            available = next;
        }
    }
}

struct TileLoadPermit<'a> {
    limiter: &'a TileLoadLimiter,
}

impl Drop for TileLoadPermit<'_> {
    fn drop(&mut self) {
        let mut available = self
            .limiter
            .available
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *available += 1;
        self.limiter.wake.notify_one();
    }
}

fn native_tile_load_limiter(remote: bool) -> &'static TileLoadLimiter {
    static LOCAL: OnceLock<TileLoadLimiter> = OnceLock::new();
    static REMOTE: OnceLock<TileLoadLimiter> = OnceLock::new();
    if remote {
        REMOTE.get_or_init(|| TileLoadLimiter::new(REMOTE_TILE_WORKERS))
    } else {
        LOCAL.get_or_init(|| TileLoadLimiter::new(LOCAL_TILE_WORKERS))
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct TileKey {
    z: u8,
    x: u32,
    y: u32,
}

#[derive(Clone, Copy, Debug)]
struct TileSample {
    key: TileKey,
    pixel_x: u32,
    pixel_y: u32,
}

#[derive(Clone, Copy, Debug)]
struct TileTarget {
    output_index: usize,
    pixel_x: u32,
    pixel_y: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DirtyRect {
    min: [usize; 2],
    max: [usize; 2],
}

impl DirtyRect {
    fn from_output_index(output_index: usize, raster_width: usize) -> Self {
        let x = output_index % raster_width;
        let y = output_index / raster_width;
        Self {
            min: [x, y],
            max: [x + 1, y + 1],
        }
    }

    fn include_output_index(&mut self, output_index: usize, raster_width: usize) {
        let x = output_index % raster_width;
        let y = output_index / raster_width;
        self.min[0] = self.min[0].min(x);
        self.min[1] = self.min[1].min(y);
        self.max[0] = self.max[0].max(x + 1);
        self.max[1] = self.max[1].max(y + 1);
    }

    fn merge(&mut self, other: Self) {
        self.min[0] = self.min[0].min(other.min[0]);
        self.min[1] = self.min[1].min(other.min[1]);
        self.max[0] = self.max[0].max(other.max[0]);
        self.max[1] = self.max[1].max(other.max[1]);
    }

    fn size(self) -> [usize; 2] {
        [self.max[0] - self.min[0], self.max[1] - self.min[1]]
    }
}

#[derive(Default)]
struct TileCache {
    tiles: HashMap<TileKey, Arc<RgbaImage>>,
    newest: VecDeque<TileKey>,
}

impl TileCache {
    fn get(&mut self, key: TileKey) -> Option<Arc<RgbaImage>> {
        let image = self.tiles.get(&key).cloned()?;
        self.newest.retain(|candidate| *candidate != key);
        self.newest.push_front(key);
        Some(image)
    }

    fn insert(&mut self, key: TileKey, image: Arc<RgbaImage>) {
        self.tiles.insert(key, image);
        self.newest.retain(|candidate| *candidate != key);
        self.newest.push_front(key);
        while self.newest.len() > TILE_CACHE_CAPACITY {
            if let Some(oldest) = self.newest.pop_back() {
                self.tiles.remove(&oldest);
            }
        }
    }
}

/// One exact frame/product renderer. Its cache is source-revision-local, so a
/// new scan or republished minute cannot reuse pixels from the previous bytes.
pub(crate) struct NativeTileRenderer {
    source: NativeTileSource,
    cache: Mutex<TileCache>,
    /// Cache only a successful preparation. A transient file/lock failure
    /// must be retried by the existing render backoff instead of becoming a
    /// permanent error for this frame.
    prepared_local: Mutex<Option<Arc<rw_sat::PreparedNativeSatelliteTileRenderer>>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NativeTileProgress {
    pub(crate) zoom: u8,
    pub(crate) loaded: usize,
    pub(crate) total: usize,
}

/// A bounded update to the current-view raster. The UI allocates the complete
/// transparent texture once, then uploads only this dirty rectangle as tiles
/// finish instead of cloning/replacing the entire viewport texture.
#[derive(Debug)]
pub(crate) struct NativeTilePatch {
    pub(crate) origin: [usize; 2],
    pub(crate) image: egui::ColorImage,
}

struct ProgressiveRaster {
    pixels: Vec<egui::Color32>,
    raster_width: usize,
    raster_height: usize,
    zoom: u8,
    loaded: usize,
    total: usize,
    pending_dirty: Option<DirtyRect>,
    attempted_visible_patch: bool,
    last_publish_attempt: Instant,
}

impl ProgressiveRaster {
    fn new(raster_width: usize, raster_height: usize, zoom: u8, total: usize) -> Self {
        Self {
            pixels: vec![egui::Color32::TRANSPARENT; raster_width * raster_height],
            raster_width,
            raster_height,
            zoom,
            loaded: 0,
            total,
            pending_dirty: None,
            attempted_visible_patch: false,
            last_publish_attempt: Instant::now(),
        }
    }

    fn apply_tile(
        &mut self,
        tile: &RgbaImage,
        targets: &[TileTarget],
        emit_partial_images: bool,
        on_update: &mut impl FnMut(NativeTileProgress, Option<NativeTilePatch>) -> bool,
    ) {
        let mut dirty: Option<DirtyRect> = None;
        for target in targets {
            let rgba = tile.get_pixel(target.pixel_x, target.pixel_y).0;
            let pixel = egui::Color32::from_rgba_unmultiplied(rgba[0], rgba[1], rgba[2], rgba[3]);
            if self.pixels[target.output_index] == pixel {
                continue;
            }
            self.pixels[target.output_index] = pixel;
            if rgba[3] > 0 {
                match &mut dirty {
                    Some(dirty) => {
                        dirty.include_output_index(target.output_index, self.raster_width)
                    }
                    None => {
                        dirty = Some(DirtyRect::from_output_index(
                            target.output_index,
                            self.raster_width,
                        ));
                    }
                }
            }
        }
        if let Some(dirty) = dirty {
            match &mut self.pending_dirty {
                Some(pending) => pending.merge(dirty),
                None => self.pending_dirty = Some(dirty),
            }
        }

        self.loaded += 1;
        let progress = NativeTileProgress {
            zoom: self.zoom,
            loaded: self.loaded,
            total: self.total,
        };
        let patch_due = emit_partial_images
            && tile_stream_patch_due(
                progress.loaded,
                progress.total,
                self.attempted_visible_patch,
                self.pending_dirty.is_some(),
                self.last_publish_attempt.elapsed(),
            );
        if !patch_due {
            let _ = on_update(progress, None);
            return;
        }

        let dirty = self
            .pending_dirty
            .expect("a progressive patch is due only with dirty pixels");
        let patch = self.copy_patch(dirty);
        // A full sync_channel is normal when egui is between frames. Retain
        // the dirty rectangle and merge later completions into it, but bound
        // retry/copy work to the same cadence as successful uploads.
        self.last_publish_attempt = Instant::now();
        self.attempted_visible_patch = true;
        if on_update(progress, Some(patch)) {
            self.pending_dirty = None;
        }
    }

    fn copy_patch(&self, dirty: DirtyRect) -> NativeTilePatch {
        let [width, height] = dirty.size();
        let mut pixels = Vec::with_capacity(width * height);
        for y in dirty.min[1]..dirty.max[1] {
            let row_start = y * self.raster_width + dirty.min[0];
            pixels.extend_from_slice(&self.pixels[row_start..row_start + width]);
        }
        NativeTilePatch {
            origin: dirty.min,
            image: egui::ColorImage {
                size: [width, height],
                source_size: egui::vec2(width as f32, height as f32),
                pixels,
            },
        }
    }

    fn into_image(self, logical_width: f64, logical_height: f64) -> egui::ColorImage {
        debug_assert_eq!(self.pixels.len(), self.raster_width * self.raster_height);
        egui::ColorImage {
            size: [self.raster_width, self.raster_height],
            source_size: egui::vec2(logical_width as f32, logical_height as f32),
            pixels: self.pixels,
        }
    }
}

enum NativeTileSource {
    Local(NativeSatMapSource),
    Remote(Box<RemoteSatMapSource>),
}

impl NativeTileRenderer {
    pub(crate) fn new(source: NativeSatMapSource) -> Self {
        Self {
            source: NativeTileSource::Local(source),
            cache: Mutex::new(TileCache::default()),
            prepared_local: Mutex::new(None),
        }
    }

    pub(crate) fn new_remote(source: RemoteSatMapSource) -> Self {
        Self {
            source: NativeTileSource::Remote(Box::new(source)),
            cache: Mutex::new(TileCache::default()),
            prepared_local: Mutex::new(None),
        }
    }

    pub(crate) fn coverage_center(&self) -> Option<(f32, f32)> {
        let center = match &self.source {
            NativeTileSource::Local(source) => source.coverage_center_e6,
            // rw-server v3 publishes world TileJSON bounds for every sector
            // and no scene center. Do not fabricate a GOES/Meso center from
            // that placeholder extent.
            NativeTileSource::Remote(_) => None,
        };
        center.map(|center| {
            (
                center[0] as f32 / 1_000_000.0,
                center[1] as f32 / 1_000_000.0,
            )
        })
    }

    /// GOES platform slug when this exact tile source is a Full Disk frame.
    ///
    /// Keep this on the revision-bound source instead of guessing from the
    /// currently selected panel controls: the user can change the picker
    /// after installing a frame, and rw-server run names are deliberately an
    /// implementation detail. Both local manifests and validated remote
    /// TileJSON carry the same platform/sector identity.
    pub(crate) fn goes_full_disk_platform(&self) -> Option<&str> {
        let (platform, sector) = match &self.source {
            NativeTileSource::Local(source) => (source.platform.as_str(), source.sector.as_str()),
            NativeTileSource::Remote(source) => (
                source.tile_source.cache_identity.platform.as_str(),
                source.tile_source.cache_identity.sector.as_str(),
            ),
        };
        let is_goes = matches!(
            platform.trim().to_ascii_lowercase().as_str(),
            "g16" | "g17" | "g18" | "g19" | "goes16" | "goes17" | "goes18" | "goes19"
        );
        let is_full_disk = matches!(
            sector.trim().to_ascii_lowercase().as_str(),
            "fulldisk" | "full_disk" | "full-disk" | "fd"
        );
        (is_goes && is_full_disk).then_some(platform)
    }

    pub(crate) fn is_remote(&self) -> bool {
        matches!(&self.source, NativeTileSource::Remote(_))
    }

    pub(crate) fn daylight_only(&self) -> bool {
        match &self.source {
            NativeTileSource::Local(source) => source.product.daylight_only(),
            NativeTileSource::Remote(source) => source
                .preview_product
                .is_some_and(GoesAbiProduct::daylight_only),
        }
    }

    /// Render the complete logical map rectangle into a possibly bounded
    /// raster. Pixel centers are mapped back through the logical dimensions,
    /// so reducing `raster_*` changes only screen detail, never map extent.
    #[allow(dead_code)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_aeqd(
        &self,
        center_lat: f64,
        center_lon: f64,
        map_scale: f32,
        logical_width: f64,
        logical_height: f64,
        raster_width: usize,
        raster_height: usize,
    ) -> Result<egui::ColorImage, String> {
        let cancel = AtomicBool::new(false);
        self.render_aeqd_cancellable(
            center_lat,
            center_lon,
            map_scale,
            logical_width,
            logical_height,
            raster_width,
            raster_height,
            &cancel,
        )
    }

    /// Same native renderer with a cooperative latest-view-wins stop token.
    /// Cancellation is checked per output row and between bounded tile reads,
    /// which prevents a superseded pan/scrub from continuing to decode an
    /// entire old viewport in the background.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_aeqd_cancellable(
        &self,
        center_lat: f64,
        center_lon: f64,
        map_scale: f32,
        logical_width: f64,
        logical_height: f64,
        raster_width: usize,
        raster_height: usize,
        cancel: &AtomicBool,
    ) -> Result<egui::ColorImage, String> {
        self.render_aeqd_cancellable_with_progress(
            center_lat,
            center_lon,
            map_scale,
            logical_width,
            logical_height,
            raster_width,
            raster_height,
            cancel,
            |_| {},
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_aeqd_cancellable_with_progress(
        &self,
        center_lat: f64,
        center_lon: f64,
        map_scale: f32,
        logical_width: f64,
        logical_height: f64,
        raster_width: usize,
        raster_height: usize,
        cancel: &AtomicBool,
        mut on_progress: impl FnMut(NativeTileProgress),
    ) -> Result<egui::ColorImage, String> {
        self.render_aeqd_cancellable_inner(
            center_lat,
            center_lon,
            map_scale,
            logical_width,
            logical_height,
            raster_width,
            raster_height,
            cancel,
            false,
            |progress, _| {
                on_progress(progress);
                true
            },
        )
    }

    /// Render the exact current view while exposing a few bounded partial
    /// rasters as its native XYZ tiles arrive. Partial images are always in
    /// the requested view/projection; callers never need to stretch an old
    /// viewport texture while waiting for a sharper zoom.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_aeqd_cancellable_progressive(
        &self,
        center_lat: f64,
        center_lon: f64,
        map_scale: f32,
        logical_width: f64,
        logical_height: f64,
        raster_width: usize,
        raster_height: usize,
        cancel: &AtomicBool,
        on_update: impl FnMut(NativeTileProgress, Option<NativeTilePatch>) -> bool,
    ) -> Result<egui::ColorImage, String> {
        self.render_aeqd_cancellable_inner(
            center_lat,
            center_lon,
            map_scale,
            logical_width,
            logical_height,
            raster_width,
            raster_height,
            cancel,
            true,
            on_update,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn render_aeqd_cancellable_inner(
        &self,
        center_lat: f64,
        center_lon: f64,
        map_scale: f32,
        logical_width: f64,
        logical_height: f64,
        raster_width: usize,
        raster_height: usize,
        cancel: &AtomicBool,
        emit_partial_images: bool,
        mut on_update: impl FnMut(NativeTileProgress, Option<NativeTilePatch>) -> bool,
    ) -> Result<egui::ColorImage, String> {
        if !center_lat.is_finite()
            || !center_lon.is_finite()
            || !map_scale.is_finite()
            || map_scale <= 0.0
            || !logical_width.is_finite()
            || !logical_height.is_finite()
            || raster_width == 0
            || raster_height == 0
        {
            return Err("native satellite map view is invalid".to_owned());
        }
        self.verify_source_revision()?;

        let zoom = self.tile_zoom(map_scale, center_lat);
        let km_per_point = 111.32 / f64::from(map_scale);
        let pixel_count = raster_width
            .checked_mul(raster_height)
            .ok_or_else(|| "native satellite map raster is too large".to_owned())?;
        let mut targets = HashMap::<TileKey, Vec<TileTarget>>::new();
        for row in 0..raster_height {
            if cancel.load(Ordering::Relaxed) {
                return Err("native satellite map render cancelled".to_owned());
            }
            for column in 0..raster_width {
                let logical_x = (column as f64 + 0.5) * logical_width / raster_width as f64;
                let logical_y = (row as f64 + 0.5) * logical_height / raster_height as f64;
                let east_km = (logical_x - logical_width * 0.5) * km_per_point;
                let north_km = (logical_height * 0.5 - logical_y) * km_per_point;
                let (latitude, longitude) =
                    aeqd_inverse_km(center_lat, center_lon, east_km, north_km);
                if let Some(sample) = xyz_sample(zoom, latitude, longitude) {
                    targets.entry(sample.key).or_default().push(TileTarget {
                        output_index: row * raster_width + column,
                        pixel_x: sample.pixel_x,
                        pixel_y: sample.pixel_y,
                    });
                }
            }
        }
        if targets.len() > MAX_VIEW_TILES {
            return Err(format!(
                "native satellite view needs {} XYZ tiles; maximum is {MAX_VIEW_TILES}",
                targets.len()
            ));
        }

        let mut keys = targets.keys().copied().collect::<Vec<_>>();
        sort_tile_keys_center_first(
            &mut keys,
            xyz_sample(zoom, center_lat, center_lon).map(|sample| sample.key),
        );
        let total = keys.len();
        let _ = on_update(
            NativeTileProgress {
                zoom,
                loaded: 0,
                total,
            },
            None,
        );
        let mut raster = ProgressiveRaster::new(raster_width, raster_height, zoom, total);
        debug_assert_eq!(raster.pixels.len(), pixel_count);

        // Load the exact center tile first. This makes the common case paint
        // useful pixels after one decode/fetch instead of waiting for a batch
        // of arbitrary edge tiles. Remaining independent tiles then fan out
        // through a small scoped pool.
        let mut keys = keys.into_iter();
        if let Some(key) = keys.next() {
            if cancel.load(Ordering::Relaxed) {
                return Err("native satellite map render cancelled".to_owned());
            }
            let tile = self.load_tile_bounded(key, cancel)?;
            raster.apply_tile(
                tile.as_ref(),
                targets.get(&key).map(Vec::as_slice).unwrap_or_default(),
                emit_partial_images,
                &mut on_update,
            );
        }

        let remaining = keys.collect::<VecDeque<_>>();
        let worker_count = native_tile_worker_count(self.is_remote(), remaining.len());
        let mut parallel_error = None;
        if worker_count > 0 {
            let queue = Mutex::new(remaining);
            let stop = AtomicBool::new(false);
            thread::scope(|scope| {
                let (result_sender, result_receiver) = mpsc::sync_channel(worker_count);
                for _ in 0..worker_count {
                    let result_sender = result_sender.clone();
                    let queue = &queue;
                    let stop = &stop;
                    scope.spawn(move || {
                        loop {
                            if cancel.load(Ordering::Relaxed) || stop.load(Ordering::Relaxed) {
                                return;
                            }
                            let key = queue
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .pop_front();
                            let Some(key) = key else {
                                return;
                            };
                            let mut result = (key, self.load_tile_bounded(key, cancel));
                            let failed = result.1.is_err();
                            loop {
                                if cancel.load(Ordering::Relaxed) || stop.load(Ordering::Relaxed) {
                                    return;
                                }
                                match result_sender.try_send(result) {
                                    Ok(()) => break,
                                    Err(mpsc::TrySendError::Full(returned)) => {
                                        result = returned;
                                        thread::yield_now();
                                    }
                                    Err(mpsc::TrySendError::Disconnected(_)) => return,
                                }
                            }
                            if failed {
                                return;
                            }
                        }
                    });
                }
                drop(result_sender);

                loop {
                    if cancel.load(Ordering::Relaxed) {
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                    match result_receiver.recv_timeout(Duration::from_millis(20)) {
                        Ok((key, Ok(tile))) => raster.apply_tile(
                            tile.as_ref(),
                            targets.get(&key).map(Vec::as_slice).unwrap_or_default(),
                            emit_partial_images,
                            &mut on_update,
                        ),
                        Ok((_, Err(error))) => {
                            parallel_error = Some(error);
                            stop.store(true, Ordering::Relaxed);
                            break;
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                stop.store(true, Ordering::Relaxed);
            });
        }
        if cancel.load(Ordering::Relaxed) {
            return Err("native satellite map render cancelled".to_owned());
        }
        if let Some(error) = parallel_error {
            return Err(error);
        }
        self.verify_source_revision()?;
        Ok(raster.into_image(logical_width, logical_height))
    }

    fn verify_source_revision(&self) -> Result<(), String> {
        let NativeTileSource::Local(source) = &self.source else {
            // Remote revision/recipe/frame identity is verified on every PNG
            // response by RemoteSatelliteClient.
            return Ok(());
        };
        let current = rw_sat::resolve_native_frame_with_revision(
            &source.store_root,
            &source.platform,
            &source.sector,
            source.product,
            &source.frame_id,
        )
        .map_err(|error| error.to_string())?;
        if current.source_revision != source.source_revision {
            return Err(format!(
                "native satellite source revision changed for {}",
                source.frame_id
            ));
        }
        Ok(())
    }

    fn tile_zoom(&self, map_scale: f32, center_lat: f64) -> u8 {
        let zoom = native_tile_zoom(map_scale, center_lat);
        match &self.source {
            NativeTileSource::Local(_) => zoom,
            NativeTileSource::Remote(source) => {
                zoom.clamp(source.tile_source.min_zoom, source.tile_source.max_zoom)
            }
        }
    }

    fn load_tile(&self, key: TileKey) -> Result<Arc<RgbaImage>, String> {
        if let Some(image) = self
            .cache
            .lock()
            .map_err(|_| "native satellite tile cache is poisoned".to_owned())?
            .get(key)
        {
            return Ok(image);
        }
        let image = match &self.source {
            NativeTileSource::Local(source) => {
                let prepared = {
                    let mut cached = self
                        .prepared_local
                        .lock()
                        .map_err(|_| "native satellite prepared renderer is poisoned".to_owned())?;
                    if let Some(prepared) = cached.as_ref() {
                        Arc::clone(prepared)
                    } else {
                        let prepared = Arc::new(
                            rw_sat::PreparedNativeSatelliteTileRenderer::open(
                                &source.store_root,
                                &source.platform,
                                &source.sector,
                                source.product,
                                &source.frame_id,
                            )
                            .map_err(|error| error.to_string())?,
                        );
                        *cached = Some(Arc::clone(&prepared));
                        prepared
                    }
                };
                let rendered = prepared
                    .render_rgba_xyz_tile(key.z, key.x, key.y, TILE_SIZE)
                    .map_err(|error| error.to_string())?;
                RgbaImage::from_raw(rendered.width, rendered.height, rendered.rgba).ok_or_else(
                    || {
                        format!(
                            "native satellite tile {} / {} / {} returned an invalid RGBA plane",
                            key.z, key.x, key.y
                        )
                    },
                )?
            }
            NativeTileSource::Remote(source) => {
                let encoded = source.fetch_tile_png(key.z, key.x, key.y)?;
                image::load_from_memory_with_format(encoded.as_ref(), image::ImageFormat::Png)
                    .map_err(|error| error.to_string())?
                    .to_rgba8()
            }
        };
        if image.dimensions() != (TILE_SIZE, TILE_SIZE) {
            return Err(format!(
                "native satellite tile {} / {} / {} decoded as {}x{}",
                key.z,
                key.x,
                key.y,
                image.width(),
                image.height()
            ));
        }
        let image = Arc::new(image);
        self.cache
            .lock()
            .map_err(|_| "native satellite tile cache is poisoned".to_owned())?
            .insert(key, Arc::clone(&image));
        Ok(image)
    }

    fn load_tile_bounded(
        &self,
        key: TileKey,
        cancel: &AtomicBool,
    ) -> Result<Arc<RgbaImage>, String> {
        let Some(_permit) = native_tile_load_limiter(self.is_remote()).acquire(cancel) else {
            return Err("native satellite map render cancelled".to_owned());
        };
        self.load_tile(key)
    }
}

fn native_tile_zoom(map_scale: f32, center_lat: f64) -> u8 {
    if !map_scale.is_finite() || map_scale <= 0.0 || !center_lat.is_finite() {
        return 0;
    }
    let target_meters_per_point = 111_320.0 / f64::from(map_scale);
    let mercator_scale = center_lat
        .clamp(-WEB_MERCATOR_MAX_LAT, WEB_MERCATOR_MAX_LAT)
        .to_radians()
        .cos()
        .abs()
        .max(0.01);
    let z = (WEB_MERCATOR_METERS_PER_PIXEL_Z0 * mercator_scale / target_meters_per_point.max(0.01))
        .log2()
        .ceil();
    z.clamp(0.0, f64::from(rw_sat::MAXIMUM_TILE_ZOOM)) as u8
}

fn native_tile_worker_count(remote: bool, remaining_tiles: usize) -> usize {
    remaining_tiles.min(if remote {
        REMOTE_TILE_WORKERS
    } else {
        LOCAL_TILE_WORKERS
    })
}

fn sort_tile_keys_center_first(keys: &mut [TileKey], center: Option<TileKey>) {
    let Some(center) = center else {
        keys.sort_unstable();
        return;
    };
    let side = 1_u32.checked_shl(u32::from(center.z)).unwrap_or(u32::MAX);
    keys.sort_unstable_by_key(|key| {
        let raw_dx = key.x.abs_diff(center.x);
        let wrapped_dx = raw_dx.min(side.saturating_sub(raw_dx));
        let dy = key.y.abs_diff(center.y);
        (
            u64::from(wrapped_dx).pow(2) + u64::from(dy).pow(2),
            key.y,
            key.x,
        )
    });
}

/// Publish the first dirty/visible tile immediately. After that, cap dirty
/// extraction and GPU uploads by elapsed time rather than tile-count
/// quartiles: expensive local tiles must never leave the map blank for the
/// first 25% of a large view.
fn tile_stream_patch_due(
    loaded: usize,
    total: usize,
    attempted_visible_patch: bool,
    has_pending_dirty: bool,
    since_last: Duration,
) -> bool {
    if loaded == 0 || loaded >= total || !has_pending_dirty {
        return false;
    }
    !attempted_visible_patch || since_last >= PARTIAL_RASTER_MIN_INTERVAL
}

fn xyz_sample(zoom: u8, latitude: f64, longitude: f64) -> Option<TileSample> {
    if !latitude.is_finite()
        || !longitude.is_finite()
        || !(-WEB_MERCATOR_MAX_LAT..=WEB_MERCATOR_MAX_LAT).contains(&latitude)
    {
        return None;
    }
    let tiles = 1_u32.checked_shl(u32::from(zoom))?;
    let world_pixels = f64::from(tiles) * f64::from(TILE_SIZE);
    let longitude = (longitude + 180.0).rem_euclid(360.0) - 180.0;
    let world_x =
        ((longitude + 180.0) / 360.0 * world_pixels).clamp(0.0, world_pixels - f64::EPSILON);
    let latitude_rad = latitude.to_radians();
    let world_y = ((1.0
        - (latitude_rad.tan() + 1.0 / latitude_rad.cos()).ln() / std::f64::consts::PI)
        * 0.5
        * world_pixels)
        .clamp(0.0, world_pixels - f64::EPSILON);
    let tile_size = f64::from(TILE_SIZE);
    let tile_x = (world_x / tile_size).floor() as u32;
    let tile_y = (world_y / tile_size).floor() as u32;
    Some(TileSample {
        key: TileKey {
            z: zoom,
            x: tile_x,
            y: tile_y,
        },
        pixel_x: (world_x - f64::from(tile_x) * tile_size).floor() as u32,
        pixel_y: (world_y - f64::from(tile_y) * tile_size).floor() as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xyz_sample_maps_prime_meridian_and_equator_to_expected_child() {
        let sample = xyz_sample(1, 0.0, 0.0).expect("equator is representable");
        assert_eq!(sample.key, TileKey { z: 1, x: 1, y: 1 });
        assert_eq!((sample.pixel_x, sample.pixel_y), (0, 0));
    }

    #[test]
    fn xyz_sample_rejects_web_mercator_poles_and_wraps_longitude() {
        assert!(xyz_sample(4, 89.0, 0.0).is_none());
        assert_eq!(
            xyz_sample(4, 0.0, 181.0).map(|sample| sample.key),
            xyz_sample(4, 0.0, -179.0).map(|sample| sample.key)
        );
    }

    #[test]
    fn tile_zoom_increases_with_map_detail_and_is_bounded() {
        let wide = native_tile_zoom(8.0, 40.0);
        let regional = native_tile_zoom(80.0, 40.0);
        let local = native_tile_zoom(8_000.0, 40.0);
        assert!(wide < regional && regional < local);
        assert!(local <= rw_sat::MAXIMUM_TILE_ZOOM);
        assert_eq!(native_tile_zoom(f32::INFINITY, 40.0), 0);
    }

    #[test]
    fn progressive_tiles_publish_first_visible_immediately_then_time_bound_updates() {
        assert!(tile_stream_patch_due(17, 122, false, true, Duration::ZERO,));
        assert!(!tile_stream_patch_due(
            1,
            122,
            false,
            false,
            Duration::from_secs(5),
        ));
        assert!(!tile_stream_patch_due(
            18,
            122,
            true,
            true,
            PARTIAL_RASTER_MIN_INTERVAL - Duration::from_millis(1),
        ));
        assert!(tile_stream_patch_due(
            19,
            122,
            true,
            true,
            PARTIAL_RASTER_MIN_INTERVAL,
        ));
        assert!(!tile_stream_patch_due(
            122,
            122,
            true,
            true,
            Duration::from_secs(1),
        ));
    }

    #[test]
    fn progressive_tiles_start_at_view_center_and_wrap_the_antimeridian() {
        let center = TileKey { z: 3, x: 0, y: 3 };
        let mut keys = vec![
            TileKey { z: 3, x: 4, y: 3 },
            TileKey { z: 3, x: 7, y: 3 },
            center,
            TileKey { z: 3, x: 1, y: 3 },
        ];
        sort_tile_keys_center_first(&mut keys, Some(center));
        assert_eq!(keys[0], center);
        assert_eq!(keys[1].x, 1);
        assert_eq!(keys[2].x, 7);
        assert_eq!(keys[3].x, 4);
    }

    #[test]
    fn progressive_dirty_rect_merges_rejected_uploads_without_full_raster_copy() {
        let mut dirty = DirtyRect {
            min: [7, 11],
            max: [9, 13],
        };
        dirty.merge(DirtyRect {
            min: [3, 20],
            max: [5, 22],
        });
        assert_eq!(dirty.min, [3, 11]);
        assert_eq!(dirty.max, [9, 22]);
        assert_eq!(dirty.size(), [6, 11]);
    }

    #[test]
    fn rejected_progressive_patch_is_retained_and_merged_for_retry() {
        let tile = RgbaImage::from_pixel(1, 1, image::Rgba([10, 20, 30, 255]));
        let mut raster = ProgressiveRaster::new(3, 3, 4, 3);
        let first = [TileTarget {
            output_index: 0,
            pixel_x: 0,
            pixel_y: 0,
        }];
        let second = [TileTarget {
            output_index: 8,
            pixel_x: 0,
            pixel_y: 0,
        }];

        raster.apply_tile(&tile, &first, true, &mut |_, patch| {
            assert_eq!(patch.expect("first visible patch").origin, [0, 0]);
            false
        });
        assert!(raster.pending_dirty.is_some());
        raster.last_publish_attempt = Instant::now() - PARTIAL_RASTER_MIN_INTERVAL;

        let mut retried = None;
        raster.apply_tile(&tile, &second, true, &mut |_, patch| {
            retried = patch.map(|patch| (patch.origin, patch.image.size));
            true
        });
        assert_eq!(retried, Some(([0, 0], [3, 3])));
        assert!(raster.pending_dirty.is_none());
    }

    #[test]
    fn tile_parallelism_is_bounded_by_source_kind_and_work() {
        assert_eq!(native_tile_worker_count(false, 122), LOCAL_TILE_WORKERS);
        assert_eq!(native_tile_worker_count(true, 122), REMOTE_TILE_WORKERS);
        assert_eq!(native_tile_worker_count(false, 2), 2);
        assert_eq!(native_tile_worker_count(true, 0), 0);
    }

    #[test]
    fn cancelled_generation_does_not_wait_for_a_global_tile_permit() {
        let limiter = TileLoadLimiter::new(1);
        let live = AtomicBool::new(false);
        let held = limiter.acquire(&live).expect("first permit");
        let cancelled = AtomicBool::new(true);
        assert!(limiter.acquire(&cancelled).is_none());
        drop(held);
        assert!(limiter.acquire(&live).is_some());
    }
}
