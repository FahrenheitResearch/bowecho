//! Native-resolution GOES map compositor.
//!
//! BowEcho's satellite player intentionally keeps a compact `.rws` preview.
//! The map is different: it may be zoomed far past that preview's stride, so
//! this module samples rw-sat's retained NetCDF archive through its bounded XYZ
//! renderer and drapes those tiles into BowEcho's azimuthal-equidistant view.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use eframe::egui;
use image::RgbaImage;

use crate::aeqd_inverse_km;
use crate::sat_worker::{NativeSatMapSource, RemoteSatMapSource};

const WEB_MERCATOR_MAX_LAT: f64 = 85.051_128_78;
const WEB_MERCATOR_METERS_PER_PIXEL_Z0: f64 = 156_543.033_928_040_97;
const TILE_SIZE: u32 = rw_sat::DEFAULT_TILE_SIZE;
const TILE_CACHE_CAPACITY: usize = 96;
const MAX_VIEW_TILES: usize = 512;

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
}

enum NativeTileSource {
    Local(NativeSatMapSource),
    Remote(RemoteSatMapSource),
}

impl NativeTileRenderer {
    pub(crate) fn new(source: NativeSatMapSource) -> Self {
        Self {
            source: NativeTileSource::Local(source),
            cache: Mutex::new(TileCache::default()),
        }
    }

    pub(crate) fn new_remote(source: RemoteSatMapSource) -> Self {
        Self {
            source: NativeTileSource::Remote(source),
            cache: Mutex::new(TileCache::default()),
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
        let mut samples = Vec::with_capacity(raster_width.saturating_mul(raster_height));
        let mut needed = HashSet::new();
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
                let sample = xyz_sample(zoom, latitude, longitude);
                if let Some(sample) = sample {
                    needed.insert(sample.key);
                }
                samples.push(sample);
            }
        }
        if needed.len() > MAX_VIEW_TILES {
            return Err(format!(
                "native satellite view needs {} XYZ tiles; maximum is {MAX_VIEW_TILES}",
                needed.len()
            ));
        }

        let mut keys = needed.into_iter().collect::<Vec<_>>();
        keys.sort_unstable();
        let mut tiles = HashMap::with_capacity(keys.len());
        for key in keys {
            if cancel.load(Ordering::Relaxed) {
                return Err("native satellite map render cancelled".to_owned());
            }
            tiles.insert(key, self.load_tile(key)?);
        }
        self.verify_source_revision()?;

        let mut pixels = vec![egui::Color32::TRANSPARENT; samples.len()];
        for (row, (pixel_row, sample_row)) in pixels
            .chunks_mut(raster_width)
            .zip(samples.chunks(raster_width))
            .enumerate()
        {
            if row % 8 == 0 && cancel.load(Ordering::Relaxed) {
                return Err("native satellite map render cancelled".to_owned());
            }
            for (pixel, sample) in pixel_row.iter_mut().zip(sample_row) {
                let Some(sample) = sample else {
                    continue;
                };
                let Some(tile) = tiles.get(&sample.key) else {
                    continue;
                };
                let rgba = tile.get_pixel(sample.pixel_x, sample.pixel_y).0;
                *pixel = egui::Color32::from_rgba_unmultiplied(rgba[0], rgba[1], rgba[2], rgba[3]);
            }
        }
        Ok(egui::ColorImage {
            size: [raster_width, raster_height],
            source_size: egui::vec2(logical_width as f32, logical_height as f32),
            pixels,
        })
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
        let encoded: Arc<Vec<u8>> = match &self.source {
            NativeTileSource::Local(source) => rw_sat::render_native_xyz_tile(
                &source.store_root,
                &source.platform,
                &source.sector,
                source.product,
                &source.frame_id,
                key.z,
                key.x,
                key.y,
                TILE_SIZE,
            )
            .map(|rendered| Arc::new(rendered.png))
            .map_err(|error| error.to_string())?,
            NativeTileSource::Remote(source) => source.fetch_tile_png(key.z, key.x, key.y)?,
        };
        let image = image::load_from_memory_with_format(encoded.as_ref(), image::ImageFormat::Png)
            .map_err(|error| error.to_string())?
            .to_rgba8();
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
}
