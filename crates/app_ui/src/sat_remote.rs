//! Strict client adapter for Rusty Weather's native satellite HTTP surface.
//!
//! The adapter deliberately resolves `latest` only through the bounded frames
//! catalog. Rendering always uses an exact frame plus the server's 64-byte
//! source revision and renderer recipe. The bearer token remains inside
//! `CommunityCacheClient` and therefore never participates in a tile cache key.

use std::collections::{BTreeMap, BTreeSet};

use chrono::NaiveDateTime;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::community_cache::{
    CommunityCacheClient, CommunityCacheError, RemoteSatelliteHttpResponse,
};

pub(crate) const SATELLITE_CATALOG_SCHEMA: &str = "rw-server.satellite-catalog.v3";
pub(crate) const SATELLITE_FRAMES_SCHEMA: &str = "rw-server.satellite-frames.v3";
pub(crate) const SATELLITE_RENDERER_RECIPE: &str = "rw-sat-native-v2";

const MAX_CATALOG_BYTES: u64 = 2 * 1024 * 1024;
const MAX_FRAMES_BYTES: u64 = 8 * 1024 * 1024;
const MAX_TILEJSON_BYTES: u64 = 64 * 1024;
const MAX_TILE_BYTES: u64 = 4 * 1024 * 1024;
/// One bounded timeline page. Multi-day results are grouped by the full
/// `YYYYMMDD` component before BowEcho's HHMM run listings consume them.
pub(crate) const MAX_FRAME_RESULTS: usize = 512;
const MAX_PLATFORMS: usize = 32;
const MAX_SECTORS: usize = 16;
const MAX_PRODUCTS: usize = 128;
const MAX_ENHANCEMENTS: usize = 64;
const MAX_ENHANCEMENT_STOPS: usize = 512;

#[derive(Debug, thiserror::Error)]
pub(crate) enum RemoteSatelliteError {
    #[error(transparent)]
    Transport(#[from] CommunityCacheError),
    #[error("Rusty Weather satellite response is invalid: {0}")]
    Invalid(&'static str),
}

#[derive(Clone)]
pub(crate) struct RemoteSatelliteClient {
    transport: CommunityCacheClient,
}

impl std::fmt::Debug for RemoteSatelliteClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteSatelliteClient")
            .field("origin", &self.transport.satellite_origin_url())
            .field("bearer_token", &"[redacted]")
            .finish()
    }
}

impl RemoteSatelliteClient {
    pub(crate) fn new(transport: CommunityCacheClient) -> Self {
        Self { transport }
    }

    pub(crate) fn catalog(
        &self,
        include_raw_channels: bool,
    ) -> Result<RemoteSatelliteCatalog, RemoteSatelliteError> {
        let response = self.transport.get_satellite_path(
            if include_raw_channels {
                "/v1/satellite/catalog?include_raw_channels=true"
            } else {
                "/v1/satellite/catalog?include_raw_channels=false"
            },
            MAX_CATALOG_BYTES,
        )?;
        require_json(&response)?;
        decode_catalog(&response.bytes)
    }

    pub(crate) fn frames(
        &self,
        catalog: &RemoteSatelliteCatalog,
        platform: &str,
        sector: &str,
        product: &str,
        limit: usize,
    ) -> Result<RemoteSatelliteFrames, RemoteSatelliteError> {
        if limit == 0 || limit > MAX_FRAME_RESULTS {
            return Err(RemoteSatelliteError::Invalid("frame limit"));
        }
        let expected = ExpectedSelection::from_catalog(catalog, platform, sector, product)?;
        let path = format!("/v1/satellite/{platform}/{sector}/{product}/frames?limit={limit}");
        let response = self.transport.get_satellite_path(&path, MAX_FRAMES_BYTES)?;
        require_json(&response)?;
        decode_frames(&response.bytes, &expected, limit)
    }

    /// Resolve an exact immutable tile source. `frame_id` must be one of the
    /// already validated frame-catalog entries; the `latest` alias is never
    /// accepted or placed in a tile URL.
    pub(crate) fn tile_source(
        &self,
        catalog: &RemoteSatelliteCatalog,
        frames: &RemoteSatelliteFrames,
        frame_id: &str,
    ) -> Result<RemoteSatelliteTileSource, RemoteSatelliteError> {
        if frame_id.eq_ignore_ascii_case("latest") {
            return Err(RemoteSatelliteError::Invalid("latest tile frame"));
        }
        let frame = frames
            .frames
            .iter()
            .find(|frame| frame.id == frame_id)
            .ok_or(RemoteSatelliteError::Invalid("unknown exact frame"))?;
        let expected = ExpectedTileJson {
            origin_url: self.transport.satellite_origin_url(),
            platform: &frames.platform,
            sector: &frames.sector,
            product: &frames.product.id,
            frame,
            catalog,
        };
        let path = format!(
            "/v1/satellite/{}/{}/{}/{}/tilejson.json",
            frames.platform, frames.sector, frames.product.id, frame.id
        );
        let response = self
            .transport
            .get_satellite_path(&path, MAX_TILEJSON_BYTES)?;
        require_json(&response)?;
        require_satellite_identity(
            &response,
            &frame.id,
            &catalog.renderer_recipe,
            &frame.source_revision,
        )?;
        let tilejson = decode_tilejson(&response.bytes, &expected)?;
        Ok(RemoteSatelliteTileSource::from_validated(
            self.transport.satellite_origin_url(),
            &frames.platform,
            &frames.sector,
            &frames.product.id,
            frame,
            tilejson,
        ))
    }

    pub(crate) fn fetch_tile(
        &self,
        source: &RemoteSatelliteTileSource,
        zoom: u8,
        x: u32,
        y: u32,
    ) -> Result<Vec<u8>, RemoteSatelliteError> {
        if source.cache_identity.origin_url != self.transport.satellite_origin_url() {
            return Err(RemoteSatelliteError::Invalid("tile origin identity"));
        }
        let url = source.tile_url(zoom, x, y)?;
        let response = self
            .transport
            .get_satellite_absolute(&url, MAX_TILE_BYTES)?;
        require_png(&response)?;
        require_satellite_identity(
            &response,
            &source.cache_identity.frame,
            &source.cache_identity.renderer_recipe,
            &source.cache_identity.source_revision,
        )?;
        let cache_control = response
            .cache_control
            .as_deref()
            .ok_or(RemoteSatelliteError::Invalid("tile cache control"))?
            .to_ascii_lowercase();
        if !cache_control
            .split(',')
            .any(|value| value.trim() == "immutable")
            || cache_control.contains("no-store")
            || cache_control.contains("no-cache")
        {
            return Err(RemoteSatelliteError::Invalid("tile cache control"));
        }
        validate_png_dimensions(&response.bytes, source.tile_size)?;
        Ok(response.bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteSatelliteCatalog {
    pub(crate) schema: String,
    pub(crate) platforms: Vec<RemoteSatellitePlatform>,
    pub(crate) sectors: Vec<RemoteSatelliteSector>,
    pub(crate) products: Vec<RemoteSatelliteProduct>,
    pub(crate) enhancements: Vec<RemoteSatelliteEnhancement>,
    pub(crate) native_source_archive: bool,
    pub(crate) full_disk_native_window_reads: bool,
    pub(crate) latest_frame_alias: String,
    pub(crate) maximum_tile_zoom: u8,
    pub(crate) tile_size: u32,
    pub(crate) renderer_recipe: String,
    pub(crate) geocolor_note: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteSatellitePlatform {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) role: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteSatelliteSector {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) cadence_seconds: u64,
    pub(crate) default_poll_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteSatelliteProduct {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) description: String,
    pub(crate) category: String,
    pub(crate) required_channels: Vec<u8>,
    pub(crate) base_channel: u8,
    pub(crate) native_resolution_km: f32,
    pub(crate) daylight_only: bool,
    pub(crate) enhancement: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteSatelliteEnhancement {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) value_units: String,
    pub(crate) stops: Vec<RemoteSatelliteEnhancementStop>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteSatelliteEnhancementStop {
    pub(crate) value: f32,
    pub(crate) rgb: [u8; 3],
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteSatelliteFrames {
    pub(crate) schema: String,
    pub(crate) platform: String,
    pub(crate) sector: String,
    pub(crate) product: RemoteSatelliteProduct,
    pub(crate) cadence_seconds: u64,
    pub(crate) frames: Vec<RemoteSatelliteFrame>,
}

impl RemoteSatelliteFrames {
    /// Group exact frame descriptors by UTC day before an HHMM-only
    /// `SatRunListing` is built. This prevents two different days' 12:00 scans
    /// from colliding in one local run timeline.
    pub(crate) fn by_utc_day(&self) -> BTreeMap<String, Vec<&RemoteSatelliteFrame>> {
        let mut days = BTreeMap::<String, Vec<&RemoteSatelliteFrame>>::new();
        for frame in &self.frames {
            days.entry(frame.id[..8].to_owned())
                .or_default()
                .push(frame);
        }
        days
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteSatelliteFrame {
    pub(crate) id: String,
    pub(crate) source_revision: String,
    pub(crate) scan_start_unix: i64,
    pub(crate) scan_end_unix: i64,
    pub(crate) channels: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RemoteTileJson {
    tilejson: String,
    name: String,
    description: String,
    scheme: String,
    tiles: Vec<String>,
    minzoom: u8,
    maxzoom: u8,
    bounds: [f64; 4],
    attribution: String,
    tile_size: u32,
    renderer_recipe: String,
    frame: String,
    source_revision: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct RemoteSatelliteTileCacheIdentity {
    pub(crate) origin_url: String,
    pub(crate) platform: String,
    pub(crate) sector: String,
    pub(crate) product: String,
    pub(crate) frame: String,
    pub(crate) renderer_recipe: String,
    pub(crate) source_revision: String,
}

impl RemoteSatelliteTileCacheIdentity {
    /// Filesystem-safe cache namespace. It contains the trusted origin and all
    /// immutable render/source identity, but cannot contain a bearer token.
    pub(crate) fn namespace_sha256(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(b"bowecho-rw-satellite-tile-v1\0");
        for value in [
            &self.origin_url,
            &self.platform,
            &self.sector,
            &self.product,
            &self.frame,
            &self.renderer_recipe,
            &self.source_revision,
        ] {
            digest.update((value.len() as u64).to_be_bytes());
            digest.update(value.as_bytes());
        }
        format!("{:x}", digest.finalize())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RemoteSatelliteTileSource {
    pub(crate) cache_identity: RemoteSatelliteTileCacheIdentity,
    pub(crate) template: String,
    pub(crate) min_zoom: u8,
    pub(crate) max_zoom: u8,
    pub(crate) bounds: [f64; 4],
    pub(crate) tile_size: u32,
    pub(crate) attribution: String,
}

impl RemoteSatelliteTileSource {
    fn from_validated(
        origin_url: &str,
        platform: &str,
        sector: &str,
        product: &str,
        frame: &RemoteSatelliteFrame,
        tilejson: RemoteTileJson,
    ) -> Self {
        Self {
            cache_identity: RemoteSatelliteTileCacheIdentity {
                origin_url: origin_url.to_owned(),
                platform: platform.to_owned(),
                sector: sector.to_owned(),
                product: product.to_owned(),
                frame: frame.id.clone(),
                renderer_recipe: tilejson.renderer_recipe.clone(),
                source_revision: frame.source_revision.clone(),
            },
            template: tilejson.tiles[0].clone(),
            min_zoom: tilejson.minzoom,
            max_zoom: tilejson.maxzoom,
            bounds: tilejson.bounds,
            tile_size: tilejson.tile_size,
            attribution: tilejson.attribution,
        }
    }

    pub(crate) fn tile_url(
        &self,
        zoom: u8,
        x: u32,
        y: u32,
    ) -> Result<String, RemoteSatelliteError> {
        if zoom < self.min_zoom || zoom > self.max_zoom || zoom >= 32 {
            return Err(RemoteSatelliteError::Invalid("tile coordinate"));
        }
        let side = 1_u32 << zoom;
        if x >= side || y >= side {
            return Err(RemoteSatelliteError::Invalid("tile coordinate"));
        }
        Ok(self
            .template
            .replace("{z}", &zoom.to_string())
            .replace("{x}", &x.to_string())
            .replace("{y}", &y.to_string()))
    }
}

struct ExpectedSelection<'a> {
    platform: &'a RemoteSatellitePlatform,
    sector: &'a RemoteSatelliteSector,
    product: &'a RemoteSatelliteProduct,
}

impl<'a> ExpectedSelection<'a> {
    fn from_catalog(
        catalog: &'a RemoteSatelliteCatalog,
        platform: &str,
        sector: &str,
        product: &str,
    ) -> Result<Self, RemoteSatelliteError> {
        let platform = catalog
            .platforms
            .iter()
            .find(|entry| entry.id == platform)
            .ok_or(RemoteSatelliteError::Invalid("satellite platform"))?;
        let sector = catalog
            .sectors
            .iter()
            .find(|entry| entry.id == sector)
            .ok_or(RemoteSatelliteError::Invalid("satellite sector"))?;
        let product = catalog
            .products
            .iter()
            .find(|entry| entry.id == product)
            .ok_or(RemoteSatelliteError::Invalid("satellite product"))?;
        Ok(Self {
            platform,
            sector,
            product,
        })
    }
}

struct ExpectedTileJson<'a> {
    origin_url: &'a str,
    platform: &'a str,
    sector: &'a str,
    product: &'a str,
    frame: &'a RemoteSatelliteFrame,
    catalog: &'a RemoteSatelliteCatalog,
}

fn decode_catalog(bytes: &[u8]) -> Result<RemoteSatelliteCatalog, RemoteSatelliteError> {
    let catalog: RemoteSatelliteCatalog =
        serde_json::from_slice(bytes).map_err(|_| RemoteSatelliteError::Invalid("catalog JSON"))?;
    validate_catalog(&catalog)?;
    Ok(catalog)
}

fn validate_catalog(catalog: &RemoteSatelliteCatalog) -> Result<(), RemoteSatelliteError> {
    if catalog.schema != SATELLITE_CATALOG_SCHEMA
        || catalog.platforms.is_empty()
        || catalog.platforms.len() > MAX_PLATFORMS
        || catalog.sectors.is_empty()
        || catalog.sectors.len() > MAX_SECTORS
        || catalog.products.is_empty()
        || catalog.products.len() > MAX_PRODUCTS
        || catalog.enhancements.len() > MAX_ENHANCEMENTS
        || !catalog.native_source_archive
        || !catalog.full_disk_native_window_reads
        || catalog.latest_frame_alias != "latest"
        || catalog.maximum_tile_zoom == 0
        || catalog.maximum_tile_zoom > rw_sat::MAXIMUM_TILE_ZOOM
        || catalog.tile_size != rw_sat::DEFAULT_TILE_SIZE
        || catalog.renderer_recipe != SATELLITE_RENDERER_RECIPE
        || !bounded_text(&catalog.geocolor_note, 4096, false)
    {
        return Err(RemoteSatelliteError::Invalid("catalog contract"));
    }

    let mut ids = BTreeSet::new();
    for platform in &catalog.platforms {
        if !valid_token(&platform.id, 32)
            || !bounded_text(&platform.title, 128, false)
            || !valid_token(&platform.role, 64)
            || !ids.insert(platform.id.as_str())
        {
            return Err(RemoteSatelliteError::Invalid("platform catalog"));
        }
    }
    ids.clear();
    for sector in &catalog.sectors {
        if !valid_token(&sector.id, 32)
            || !bounded_text(&sector.title, 128, false)
            || sector.cadence_seconds == 0
            || sector.cadence_seconds > 24 * 60 * 60
            || sector.default_poll_seconds == 0
            || sector.default_poll_seconds > 24 * 60 * 60
            || !ids.insert(sector.id.as_str())
        {
            return Err(RemoteSatelliteError::Invalid("sector catalog"));
        }
    }
    ids.clear();
    for product in &catalog.products {
        if !validate_product(product) || !ids.insert(product.id.as_str()) {
            return Err(RemoteSatelliteError::Invalid("product catalog"));
        }
    }
    let product_enhancements = catalog
        .products
        .iter()
        .filter_map(|product| product.enhancement.as_deref())
        .collect::<BTreeSet<_>>();
    ids.clear();
    for enhancement in &catalog.enhancements {
        if !valid_token(&enhancement.id, 64)
            || !bounded_text(&enhancement.title, 128, false)
            || !bounded_text(&enhancement.value_units, 64, false)
            || enhancement.stops.is_empty()
            || enhancement.stops.len() > MAX_ENHANCEMENT_STOPS
            || enhancement.stops.iter().any(|stop| !stop.value.is_finite())
            || enhancement
                .stops
                .windows(2)
                .any(|pair| pair[0].value > pair[1].value)
            || !ids.insert(enhancement.id.as_str())
        {
            return Err(RemoteSatelliteError::Invalid("enhancement catalog"));
        }
    }
    if product_enhancements.iter().any(|id| !ids.contains(id)) {
        return Err(RemoteSatelliteError::Invalid("product enhancement"));
    }
    Ok(())
}

fn validate_product(product: &RemoteSatelliteProduct) -> bool {
    const CATEGORIES: [&str; 7] = [
        "favorites",
        "visible",
        "infrared",
        "water_vapor",
        "rgb_composite",
        "fire",
        "advanced",
    ];
    valid_token(&product.id, 64)
        && bounded_text(&product.title, 128, false)
        && bounded_text(&product.description, 2048, false)
        && CATEGORIES.contains(&product.category.as_str())
        && !product.required_channels.is_empty()
        && product.required_channels.len() <= 16
        && product
            .required_channels
            .iter()
            .all(|channel| (1..=16).contains(channel))
        && product
            .required_channels
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .len()
            == product.required_channels.len()
        && product.required_channels.contains(&product.base_channel)
        && product.native_resolution_km.is_finite()
        && product.native_resolution_km > 0.0
        && product.native_resolution_km <= 100.0
        && product
            .enhancement
            .as_deref()
            .is_none_or(|value| valid_token(value, 64))
}

fn decode_frames(
    bytes: &[u8],
    expected: &ExpectedSelection<'_>,
    requested_limit: usize,
) -> Result<RemoteSatelliteFrames, RemoteSatelliteError> {
    let frames: RemoteSatelliteFrames =
        serde_json::from_slice(bytes).map_err(|_| RemoteSatelliteError::Invalid("frames JSON"))?;
    if frames.schema != SATELLITE_FRAMES_SCHEMA
        || frames.platform != expected.platform.id
        || frames.sector != expected.sector.id
        || frames.product != *expected.product
        || frames.cadence_seconds != expected.sector.cadence_seconds
        || frames.frames.len() > requested_limit
        || frames.frames.len() > MAX_FRAME_RESULTS
    {
        return Err(RemoteSatelliteError::Invalid("frames contract"));
    }
    let required = expected
        .product
        .required_channels
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let mut ids = BTreeSet::new();
    let mut previous_start = i64::MAX;
    for frame in &frames.frames {
        let parsed =
            parse_frame_unix(&frame.id).ok_or(RemoteSatelliteError::Invalid("frame identity"))?;
        let channels = frame.channels.iter().copied().collect::<BTreeSet<_>>();
        if !is_sha256_hex(&frame.source_revision)
            || frame.scan_start_unix <= 0
            || frame.scan_end_unix < frame.scan_start_unix
            || parsed.div_euclid(60) != frame.scan_start_unix.div_euclid(60)
            || frame.scan_start_unix > previous_start
            || frame.channels.is_empty()
            || frame.channels.len() > 16
            || channels.len() != frame.channels.len()
            || channels.iter().any(|channel| !(1..=16).contains(channel))
            || !required.is_subset(&channels)
            || !ids.insert(frame.id.as_str())
        {
            return Err(RemoteSatelliteError::Invalid("frame contract"));
        }
        previous_start = frame.scan_start_unix;
    }
    Ok(frames)
}

fn decode_tilejson(
    bytes: &[u8],
    expected: &ExpectedTileJson<'_>,
) -> Result<RemoteTileJson, RemoteSatelliteError> {
    let tilejson: RemoteTileJson = serde_json::from_slice(bytes)
        .map_err(|_| RemoteSatelliteError::Invalid("TileJSON JSON"))?;
    let expected_template = format!(
        "{}/v1/satellite/{}/{}/{}/{}/tiles/{}/{}/{{z}}/{{x}}/{{y}}.png",
        expected.origin_url.trim_end_matches('/'),
        expected.platform,
        expected.sector,
        expected.product,
        expected.frame.id,
        expected.catalog.renderer_recipe,
        expected.frame.source_revision,
    );
    if tilejson.tilejson != "3.0.0"
        || tilejson.scheme != "xyz"
        || tilejson.tiles.as_slice() != [expected_template.as_str()]
        || tilejson.minzoom > tilejson.maxzoom
        || tilejson.maxzoom > expected.catalog.maximum_tile_zoom
        || tilejson.tile_size != expected.catalog.tile_size
        || tilejson.renderer_recipe != expected.catalog.renderer_recipe
        || tilejson.frame != expected.frame.id
        || tilejson.source_revision != expected.frame.source_revision
        || !bounded_text(&tilejson.name, 512, false)
        || !bounded_text(&tilejson.description, 4096, false)
        || !bounded_text(&tilejson.attribution, 1024, false)
        || !valid_bounds(tilejson.bounds)
        || tilejson.tiles[0].to_ascii_lowercase().contains("/latest/")
    {
        return Err(RemoteSatelliteError::Invalid("TileJSON contract"));
    }
    Ok(tilejson)
}

fn require_json(response: &RemoteSatelliteHttpResponse) -> Result<(), RemoteSatelliteError> {
    let content_type = response
        .content_type
        .as_deref()
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .unwrap_or_default();
    if content_type.eq_ignore_ascii_case("application/json") {
        Ok(())
    } else {
        Err(RemoteSatelliteError::Invalid("JSON content type"))
    }
}

fn require_png(response: &RemoteSatelliteHttpResponse) -> Result<(), RemoteSatelliteError> {
    let content_type = response
        .content_type
        .as_deref()
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .unwrap_or_default();
    if content_type.eq_ignore_ascii_case("image/png") {
        Ok(())
    } else {
        Err(RemoteSatelliteError::Invalid("PNG content type"))
    }
}

fn require_satellite_identity(
    response: &RemoteSatelliteHttpResponse,
    expected_frame: &str,
    expected_recipe: &str,
    expected_revision: &str,
) -> Result<(), RemoteSatelliteError> {
    if response.frame.as_deref() == Some(expected_frame)
        && response.renderer_recipe.as_deref() == Some(expected_recipe)
        && response.source_revision.as_deref() == Some(expected_revision)
    {
        Ok(())
    } else {
        Err(RemoteSatelliteError::Invalid("satellite identity headers"))
    }
}

fn validate_png_dimensions(bytes: &[u8], expected: u32) -> Result<(), RemoteSatelliteError> {
    if bytes.len() < 24
        || &bytes[..8] != b"\x89PNG\r\n\x1a\n"
        || u32::from_be_bytes(bytes[8..12].try_into().unwrap_or_default()) != 13
        || &bytes[12..16] != b"IHDR"
        || u32::from_be_bytes(bytes[16..20].try_into().unwrap_or_default()) != expected
        || u32::from_be_bytes(bytes[20..24].try_into().unwrap_or_default()) != expected
    {
        return Err(RemoteSatelliteError::Invalid("PNG tile dimensions"));
    }
    Ok(())
}

fn parse_frame_unix(value: &str) -> Option<i64> {
    if value.len() != 13 || value.as_bytes().get(8) != Some(&b'T') {
        return None;
    }
    NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M")
        .ok()
        .map(|time| time.and_utc().timestamp())
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_token(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
}

fn bounded_text(value: &str, maximum: usize, empty_ok: bool) -> bool {
    (empty_ok || !value.trim().is_empty())
        && value.len() <= maximum
        && !value.chars().any(char::is_control)
}

fn valid_bounds(bounds: [f64; 4]) -> bool {
    bounds.iter().all(|value| value.is_finite())
        && (-180.0..=180.0).contains(&bounds[0])
        && (-85.051_128_78..=85.051_128_78).contains(&bounds[1])
        && (-180.0..=180.0).contains(&bounds[2])
        && (-85.051_128_78..=85.051_128_78).contains(&bounds[3])
        && bounds[0] < bounds[2]
        && bounds[1] < bounds[3]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    const REVISION: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const FRAME: &str = "20260825T1200";
    const ORIGIN: &str = "https://weather.example.edu/rw";

    fn catalog_json() -> Value {
        json!({
            "schema": SATELLITE_CATALOG_SCHEMA,
            "platforms": [{
                "id": "g19", "title": "GOES-19 East", "role": "operational_east"
            }],
            "sectors": [{
                "id": "conus", "title": "CONUS", "cadence_seconds": 300,
                "default_poll_seconds": 60
            }],
            "products": [{
                "id": "clean_ir", "title": "Clean infrared", "description": "ABI C13",
                "category": "infrared", "required_channels": [13], "base_channel": 13,
                "native_resolution_km": 2.0, "daylight_only": false,
                "enhancement": "clean_ir"
            }],
            "enhancements": [{
                "id": "clean_ir", "title": "Clean infrared", "value_units": "K",
                "stops": [{"value": 180.0, "rgb": [255, 255, 255]},
                          {"value": 330.0, "rgb": [0, 0, 0]}]
            }],
            "native_source_archive": true,
            "full_disk_native_window_reads": true,
            "latest_frame_alias": "latest",
            "maximum_tile_zoom": 14,
            "tile_size": 256,
            "renderer_recipe": SATELLITE_RENDERER_RECIPE,
            "geocolor_note": "Native ABI rendering"
        })
    }

    fn catalog() -> RemoteSatelliteCatalog {
        decode_catalog(&serde_json::to_vec(&catalog_json()).unwrap()).unwrap()
    }

    fn frame_json() -> Value {
        let catalog = catalog_json();
        json!({
            "schema": SATELLITE_FRAMES_SCHEMA,
            "platform": "g19",
            "sector": "conus",
            "product": catalog["products"][0].clone(),
            "cadence_seconds": 300,
            "frames": [{
                "id": FRAME,
                "source_revision": REVISION,
                "scan_start_unix": 1787659200_i64,
                "scan_end_unix": 1787659500_i64,
                "channels": [13]
            }]
        })
    }

    fn frames(catalog: &RemoteSatelliteCatalog) -> RemoteSatelliteFrames {
        let expected =
            ExpectedSelection::from_catalog(catalog, "g19", "conus", "clean_ir").unwrap();
        decode_frames(&serde_json::to_vec(&frame_json()).unwrap(), &expected, 8).unwrap()
    }

    fn tilejson_json() -> Value {
        json!({
            "tilejson": "3.0.0",
            "name": "GOES-19 clean infrared",
            "description": "ABI C13",
            "scheme": "xyz",
            "tiles": [format!(
                "{ORIGIN}/v1/satellite/g19/conus/clean_ir/{FRAME}/tiles/{SATELLITE_RENDERER_RECIPE}/{REVISION}/{{z}}/{{x}}/{{y}}.png"
            )],
            "minzoom": 0,
            "maxzoom": 14,
            "bounds": [-180.0, -85.05112878, 180.0, 85.05112878],
            "attribution": "NOAA/NESDIS; rendered by Rusty Weather",
            "tileSize": 256,
            "rendererRecipe": SATELLITE_RENDERER_RECIPE,
            "frame": FRAME,
            "sourceRevision": REVISION
        })
    }

    fn tile_source() -> RemoteSatelliteTileSource {
        let catalog = catalog();
        let frames = frames(&catalog);
        let expected = ExpectedTileJson {
            origin_url: ORIGIN,
            platform: &frames.platform,
            sector: &frames.sector,
            product: &frames.product.id,
            frame: &frames.frames[0],
            catalog: &catalog,
        };
        let tilejson =
            decode_tilejson(&serde_json::to_vec(&tilejson_json()).unwrap(), &expected).unwrap();
        RemoteSatelliteTileSource::from_validated(
            ORIGIN,
            "g19",
            "conus",
            "clean_ir",
            &frames.frames[0],
            tilejson,
        )
    }

    #[test]
    fn v3_catalog_and_frames_accept_the_exact_server_contract() {
        let catalog = catalog();
        assert_eq!(catalog.renderer_recipe, SATELLITE_RENDERER_RECIPE);
        let frames = frames(&catalog);
        assert_eq!(frames.frames[0].id, FRAME);
        assert_eq!(frames.frames[0].source_revision, REVISION);
    }

    #[test]
    fn catalog_is_schema_bounded_and_denies_unknown_fields() {
        for mutation in ["schema", "renderer_recipe", "tile_size"] {
            let mut value = catalog_json();
            value[mutation] = match mutation {
                "schema" => json!("rw-server.satellite-catalog.v2"),
                "renderer_recipe" => json!("future-recipe"),
                _ => json!(512),
            };
            assert!(decode_catalog(&serde_json::to_vec(&value).unwrap()).is_err());
        }
        let mut unknown = catalog_json();
        unknown["surprise"] = json!(true);
        assert!(decode_catalog(&serde_json::to_vec(&unknown).unwrap()).is_err());
    }

    #[test]
    fn frames_reject_aliases_bad_revisions_and_missing_product_channels() {
        let catalog = catalog();
        let expected =
            ExpectedSelection::from_catalog(&catalog, "g19", "conus", "clean_ir").unwrap();
        for (field, value) in [
            ("id", json!("latest")),
            ("source_revision", json!("A".repeat(64))),
            ("channels", json!([2])),
        ] {
            let mut body = frame_json();
            body["frames"][0][field] = value;
            assert!(decode_frames(&serde_json::to_vec(&body).unwrap(), &expected, 8).is_err());
        }
    }

    #[test]
    fn multi_day_same_hhmm_frames_remain_in_separate_utc_runs() {
        let catalog = catalog();
        let expected =
            ExpectedSelection::from_catalog(&catalog, "g19", "conus", "clean_ir").unwrap();
        let mut body = frame_json();
        let mut older = body["frames"][0].clone();
        older["id"] = json!("20260824T1200");
        older["scan_start_unix"] = json!(1_787_572_800_i64);
        older["scan_end_unix"] = json!(1_787_573_100_i64);
        body["frames"].as_array_mut().unwrap().push(older);
        let frames = decode_frames(
            &serde_json::to_vec(&body).unwrap(),
            &expected,
            MAX_FRAME_RESULTS,
        )
        .unwrap();
        let days = frames.by_utc_day();
        assert_eq!(days.len(), 2);
        assert_eq!(days["20260825"][0].id, "20260825T1200");
        assert_eq!(days["20260824"][0].id, "20260824T1200");
    }

    #[test]
    fn tilejson_accepts_only_the_exact_revision_bound_same_origin_template() {
        let catalog = catalog();
        let frames = frames(&catalog);
        let expected = ExpectedTileJson {
            origin_url: ORIGIN,
            platform: &frames.platform,
            sector: &frames.sector,
            product: &frames.product.id,
            frame: &frames.frames[0],
            catalog: &catalog,
        };
        assert!(decode_tilejson(&serde_json::to_vec(&tilejson_json()).unwrap(), &expected).is_ok());
        for template in [
            format!(
                "{ORIGIN}/v1/satellite/g19/conus/clean_ir/latest/tiles/{SATELLITE_RENDERER_RECIPE}/{REVISION}/{{z}}/{{x}}/{{y}}.png"
            ),
            format!(
                "https://evil.example/v1/satellite/g19/conus/clean_ir/{FRAME}/tiles/{SATELLITE_RENDERER_RECIPE}/{REVISION}/{{z}}/{{x}}/{{y}}.png"
            ),
            format!(
                "{ORIGIN}/v1/satellite/g19/conus/clean_ir/{FRAME}/tiles/future/{REVISION}/{{z}}/{{x}}/{{y}}.png"
            ),
        ] {
            let mut body = tilejson_json();
            body["tiles"][0] = json!(template);
            assert!(decode_tilejson(&serde_json::to_vec(&body).unwrap(), &expected).is_err());
        }
    }

    #[test]
    fn tile_cache_identity_changes_with_source_revision_and_contains_no_credential() {
        let first = tile_source();
        let mut second = first.cache_identity.clone();
        second.source_revision = "b".repeat(64);
        assert_ne!(
            first.cache_identity.namespace_sha256(),
            second.namespace_sha256()
        );
        assert_eq!(first.cache_identity.namespace_sha256().len(), 64);
        assert!(!format!("{:?}", first.cache_identity).contains("bearer"));
        assert_eq!(
            first.tile_url(4, 3, 6).unwrap(),
            format!(
                "{ORIGIN}/v1/satellite/g19/conus/clean_ir/{FRAME}/tiles/{SATELLITE_RENDERER_RECIPE}/{REVISION}/4/3/6.png"
            )
        );
        assert!(first.tile_url(4, 16, 0).is_err());
        assert!(first.tile_url(15, 0, 0).is_err());
    }

    #[test]
    fn png_gate_requires_the_advertised_tile_dimensions() {
        let mut png = Vec::from(&b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR"[..]);
        png.extend_from_slice(&256_u32.to_be_bytes());
        png.extend_from_slice(&256_u32.to_be_bytes());
        assert!(validate_png_dimensions(&png, 256).is_ok());
        assert!(validate_png_dimensions(&png, 512).is_err());
        png[16..20].copy_from_slice(&0_u32.to_be_bytes());
        assert!(validate_png_dimensions(&png, 256).is_err());
    }

    #[test]
    fn all_three_server_identity_headers_must_agree() {
        let response = RemoteSatelliteHttpResponse {
            bytes: Vec::new(),
            content_type: Some("image/png".into()),
            cache_control: Some("public, max-age=31536000, immutable".into()),
            source_revision: Some(REVISION.into()),
            frame: Some(FRAME.into()),
            renderer_recipe: Some(SATELLITE_RENDERER_RECIPE.into()),
        };
        assert!(
            require_satellite_identity(&response, FRAME, SATELLITE_RENDERER_RECIPE, REVISION)
                .is_ok()
        );
        let mut wrong = response;
        wrong.renderer_recipe = Some("future-recipe".into());
        assert!(
            require_satellite_identity(&wrong, FRAME, SATELLITE_RENDERER_RECIPE, REVISION).is_err()
        );
    }
}
