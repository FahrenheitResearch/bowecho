//! Public EUMETView (EUMETSAT WMS) client for Meteosat Third Generation.
//!
//! Interactive imagery is intentionally sourced from EUMETView rather than
//! downloading 700-900 MB FCI Data Store packages for every frame. The WMS is
//! public, supplies explicit timestamps, and returns already-rendered imagery
//! suitable for BowEcho's shared satellite player/map/native-plot store.

use chrono::{DateTime, Utc};
use image::ImageFormat;
use quick_xml::events::Event;

pub(crate) const EUMETVIEW_WMS_URL: &str = "https://view.eumetsat.int/geoserver/wms";
const MAX_IMAGE_EDGE: u32 = 2_048;
const MAX_IMAGE_PIXELS: u64 = 4_194_304;
const MAX_IMAGE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum MtgProduct {
    #[default]
    GeoColour,
    TrueColour,
    Ir105Hrfi,
    Vis06Hrfi,
    CloudPhase,
    CloudType,
    Dust,
    FireTemperature,
    FogLowCloud,
    Snow,
    LightningAfa,
}

impl MtgProduct {
    pub(crate) const ALL: [Self; 11] = [
        Self::GeoColour,
        Self::TrueColour,
        Self::Ir105Hrfi,
        Self::Vis06Hrfi,
        Self::CloudPhase,
        Self::CloudType,
        Self::Dust,
        Self::FireTemperature,
        Self::FogLowCloud,
        Self::Snow,
        Self::LightningAfa,
    ];

    pub(crate) fn slug(self) -> &'static str {
        match self {
            Self::GeoColour => "geo_colour",
            Self::TrueColour => "true_colour",
            Self::Ir105Hrfi => "ir_105_hrfi",
            Self::Vis06Hrfi => "vis_06_hrfi",
            Self::CloudPhase => "cloud_phase",
            Self::CloudType => "cloud_type",
            Self::Dust => "dust",
            Self::FireTemperature => "fire_temperature",
            Self::FogLowCloud => "fog_low_cloud",
            Self::Snow => "snow",
            Self::LightningAfa => "lightning_afa",
        }
    }

    pub(crate) fn layer(self) -> &'static str {
        match self {
            Self::GeoColour => "mtg_fd:rgb_geocolour",
            Self::TrueColour => "mtg_fd:rgb_truecolour",
            Self::Ir105Hrfi => "mtg_fd:ir105_hrfi",
            Self::Vis06Hrfi => "mtg_fd:vis06_hrfi",
            Self::CloudPhase => "mtg_fd:rgb_cloudphase",
            Self::CloudType => "mtg_fd:rgb_cloudtype",
            Self::Dust => "mtg_fd:rgb_dust",
            Self::FireTemperature => "mtg_fd:rgb_firetemperature",
            Self::FogLowCloud => "mtg_fd:rgb_fog",
            Self::Snow => "mtg_fd:rgb_snow",
            Self::LightningAfa => "mtg_fd:li_afa",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::GeoColour => "Geo Colour",
            Self::TrueColour => "True Colour",
            Self::Ir105Hrfi => "IR 10.5 µm · HRFI",
            Self::Vis06Hrfi => "Visible 0.6 µm · HRFI",
            Self::CloudPhase => "Cloud Phase",
            Self::CloudType => "Cloud Type",
            Self::Dust => "Dust",
            Self::FireTemperature => "Fire Temperature",
            Self::FogLowCloud => "Fog / Low Cloud",
            Self::Snow => "Snow",
            Self::LightningAfa => "Lightning AFA · 5 min",
        }
    }

    pub(crate) fn cadence_minutes(self) -> i64 {
        match self {
            Self::LightningAfa => 5,
            _ => 10,
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        let normalized = value.trim().to_ascii_lowercase().replace('-', "_");
        Self::ALL.into_iter().find(|product| {
            product.slug() == normalized
                || product.layer().eq_ignore_ascii_case(value.trim())
                || (product == &Self::GeoColour && normalized == "geocolour")
                || (product == &Self::TrueColour && normalized == "truecolour")
                || (product == &Self::LightningAfa
                    && matches!(normalized.as_str(), "li_afa" | "lightning"))
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct WmsBounds {
    pub west_deg: f64,
    pub south_deg: f64,
    pub east_deg: f64,
    pub north_deg: f64,
}

impl WmsBounds {
    pub(crate) fn validate(self) -> Result<Self, String> {
        let finite = [
            self.west_deg,
            self.south_deg,
            self.east_deg,
            self.north_deg,
        ]
        .into_iter()
        .all(f64::is_finite);
        if !finite
            || self.west_deg < -180.0
            || self.east_deg > 180.0
            || self.south_deg < -90.0
            || self.north_deg > 90.0
            || self.west_deg >= self.east_deg
            || self.south_deg >= self.north_deg
        {
            return Err(format!(
                "invalid EUMETView bounds west/south/east/north = {},{},{},{}",
                self.west_deg, self.south_deg, self.east_deg, self.north_deg
            ));
        }
        Ok(self)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LayerCapability {
    pub product: MtgProduct,
    pub title: String,
    pub bounds: WmsBounds,
    pub first_time: DateTime<Utc>,
    pub latest_time: DateTime<Utc>,
    pub cadence_minutes: i64,
}

impl LayerCapability {
    /// Explicit frame timestamps in chronological order, ending at the WMS
    /// default/latest timestamp. Explicit values keep store identity stable
    /// and avoid a moving `time=latest` cache key.
    pub(crate) fn recent_times(&self, count: usize) -> Vec<DateTime<Utc>> {
        let count = count.clamp(1, 72);
        let cadence = self.cadence_minutes.max(1);
        let mut times = (0..count)
            .rev()
            .filter_map(|offset| {
                self.latest_time
                    .checked_sub_signed(chrono::Duration::minutes(cadence * offset as i64))
            })
            .filter(|time| *time >= self.first_time)
            .collect::<Vec<_>>();
        times.sort_unstable();
        times.dedup();
        times
    }
}

#[derive(Default)]
struct LayerDraft {
    name: String,
    title: String,
    time_extent: String,
    default_time: String,
    west: String,
    south: String,
    east: String,
    north: String,
}

#[derive(Clone, Copy)]
enum TextTarget {
    Name,
    Title,
    Time,
    West,
    South,
    East,
    North,
}

pub(crate) fn parse_capabilities(xml: &str) -> Result<Vec<LayerCapability>, String> {
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut stack: Vec<LayerDraft> = Vec::new();
    let mut target: Option<TextTarget> = None;
    let mut out = Vec::new();
    let mut seen_layers = 0usize;
    let mut seen_names = Vec::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) => {
                let local = event.local_name();
                let name = local.as_ref();
                if name == b"Layer" {
                    stack.push(LayerDraft::default());
                    target = None;
                } else if let Some(draft) = stack.last_mut() {
                    target = match name {
                        b"Name" => Some(TextTarget::Name),
                        b"Title" => Some(TextTarget::Title),
                        b"westBoundLongitude" => Some(TextTarget::West),
                        b"southBoundLatitude" => Some(TextTarget::South),
                        b"eastBoundLongitude" => Some(TextTarget::East),
                        b"northBoundLatitude" => Some(TextTarget::North),
                        b"Dimension" => {
                            let is_time = event.attributes().flatten().any(|attribute| {
                                attribute.key.local_name().as_ref() == b"name"
                                    && attribute
                                        .decode_and_unescape_value(event.decoder())
                                        .is_ok_and(|value| value.eq_ignore_ascii_case("time"))
                            });
                            if is_time {
                                draft.default_time = event
                                    .attributes()
                                    .flatten()
                                    .find(|attribute| {
                                        attribute.key.local_name().as_ref() == b"default"
                                    })
                                    .and_then(|attribute| {
                                        attribute
                                            .decode_and_unescape_value(event.decoder())
                                            .ok()
                                    })
                                    .map(|value| value.into_owned())
                                    .unwrap_or_default();
                                Some(TextTarget::Time)
                            } else {
                                None
                            }
                        }
                        _ => None,
                    };
                }
            }
            Ok(Event::Text(text)) => {
                if let (Some(draft), Some(target)) = (stack.last_mut(), target) {
                    let value = text
                        .decode()
                        .map_err(|error| format!("EUMETView capabilities text decode failed: {error}"))?
                        .trim()
                        .to_owned();
                    match target {
                        TextTarget::Name => draft.name = value,
                        TextTarget::Title => draft.title = value,
                        TextTarget::Time => draft.time_extent = value,
                        TextTarget::West => draft.west = value,
                        TextTarget::South => draft.south = value,
                        TextTarget::East => draft.east = value,
                        TextTarget::North => draft.north = value,
                    }
                }
            }
            Ok(Event::End(event)) => {
                if event.local_name().as_ref() == b"Layer" {
                    if let Some(draft) = stack.pop()
                    {
                        seen_layers += 1;
                        if !draft.name.is_empty() && seen_names.len() < 8 {
                            seen_names.push(draft.name.clone());
                        }
                        if let Some(product) = MtgProduct::parse(&draft.name) {
                            out.push(finish_layer(product, draft)?);
                        }
                    }
                }
                target = None;
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => {
                return Err(format!("EUMETView capabilities XML parse failed: {error}"));
            }
        }
    }
    if out.is_empty() {
        return Err(format!(
            "EUMETView capabilities contained none of BowEcho's MTG layers (inspected {seen_layers} layers; first names: {})",
            seen_names.join(", ")
        ));
    }
    out.sort_by_key(|layer| {
        MtgProduct::ALL
            .iter()
            .position(|candidate| *candidate == layer.product)
            .unwrap_or(usize::MAX)
    });
    Ok(out)
}

fn finish_layer(product: MtgProduct, draft: LayerDraft) -> Result<LayerCapability, String> {
    let bounds = WmsBounds {
        west_deg: parse_bound("west", &draft.west)?,
        south_deg: parse_bound("south", &draft.south)?,
        east_deg: parse_bound("east", &draft.east)?,
        north_deg: parse_bound("north", &draft.north)?,
    }
    .validate()?;
    let (first_time, interval_latest, cadence_minutes) =
        parse_time_extent(&draft.time_extent, product.cadence_minutes())?;
    let latest_time = if draft.default_time.trim().is_empty() {
        interval_latest
    } else {
        parse_wms_time(&draft.default_time)?
    };
    Ok(LayerCapability {
        product,
        title: if draft.title.trim().is_empty() {
            product.label().to_owned()
        } else {
            draft.title
        },
        bounds,
        first_time,
        latest_time,
        cadence_minutes,
    })
}

fn parse_bound(label: &str, value: &str) -> Result<f64, String> {
    value
        .trim()
        .parse::<f64>()
        .map_err(|error| format!("EUMETView {label} bound '{value}' is invalid: {error}"))
}

fn parse_wms_time(value: &str) -> Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(value.trim())
        .map(|time| time.with_timezone(&Utc))
        .map_err(|error| format!("EUMETView timestamp '{value}' is invalid: {error}"))
}

fn parse_time_extent(
    value: &str,
    fallback_cadence_minutes: i64,
) -> Result<(DateTime<Utc>, DateTime<Utc>, i64), String> {
    let parts = value.trim().split('/').collect::<Vec<_>>();
    if parts.len() != 3 {
        return Err(format!(
            "EUMETView time extent '{value}' is not start/end/period"
        ));
    }
    let first = parse_wms_time(parts[0])?;
    let latest = parse_wms_time(parts[1])?;
    let cadence = parse_iso_minutes(parts[2]).unwrap_or(fallback_cadence_minutes.max(1));
    if latest < first {
        return Err(format!("EUMETView time extent ends before it starts: {value}"));
    }
    Ok((first, latest, cadence))
}

fn parse_iso_minutes(value: &str) -> Option<i64> {
    let value = value.trim().to_ascii_uppercase();
    value
        .strip_prefix("PT")?
        .strip_suffix('M')?
        .parse::<i64>()
        .ok()
        .filter(|minutes| *minutes > 0)
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct GetMapRequest {
    pub product: MtgProduct,
    pub time: DateTime<Utc>,
    pub bounds: WmsBounds,
    pub width: u32,
    pub height: u32,
}

impl GetMapRequest {
    pub(crate) fn validate(&self) -> Result<(), String> {
        self.bounds.validate()?;
        if self.width < 2
            || self.height < 2
            || self.width > MAX_IMAGE_EDGE
            || self.height > MAX_IMAGE_EDGE
            || u64::from(self.width) * u64::from(self.height) > MAX_IMAGE_PIXELS
        {
            return Err(format!(
                "EUMETView image size {}x{} is outside BowEcho's 2..{MAX_IMAGE_EDGE} / {MAX_IMAGE_PIXELS}-pixel bound",
                self.width, self.height
            ));
        }
        Ok(())
    }

    pub(crate) fn url(&self) -> Result<reqwest::Url, String> {
        self.validate()?;
        let mut url = reqwest::Url::parse(EUMETVIEW_WMS_URL)
            .map_err(|error| format!("invalid EUMETView endpoint: {error}"))?;
        let bbox = format!(
            "{},{},{},{}",
            self.bounds.west_deg,
            self.bounds.south_deg,
            self.bounds.east_deg,
            self.bounds.north_deg
        );
        let width = self.width.to_string();
        let height = self.height.to_string();
        let time = self.time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        url.query_pairs_mut()
            .append_pair("service", "WMS")
            .append_pair("version", "1.3.0")
            .append_pair("request", "GetMap")
            .append_pair("layers", self.product.layer())
            .append_pair("styles", "")
            .append_pair("format", "image/png")
            .append_pair("transparent", "true")
            // CRS:84 keeps the conventional west,south,east,north axis order.
            .append_pair("crs", "CRS:84")
            .append_pair("bbox", &bbox)
            .append_pair("width", &width)
            .append_pair("height", &height)
            .append_pair("time", &time);
        Ok(url)
    }
}

pub(crate) fn image_size_for_bounds(bounds: WmsBounds, max_edge: u32) -> (u32, u32) {
    let max_edge = max_edge.clamp(256, MAX_IMAGE_EDGE);
    let width_span = (bounds.east_deg - bounds.west_deg).max(0.001);
    let height_span = (bounds.north_deg - bounds.south_deg).max(0.001);
    if width_span >= height_span {
        (
            max_edge,
            ((f64::from(max_edge) * height_span / width_span).round() as u32).clamp(256, max_edge),
        )
    } else {
        (
            ((f64::from(max_edge) * width_span / height_span).round() as u32).clamp(256, max_edge),
            max_edge,
        )
    }
}

pub(crate) struct WmsImage {
    pub width: usize,
    pub height: usize,
    pub rgb: Vec<u8>,
    pub alpha: Vec<u8>,
}

pub(crate) struct EumetViewClient {
    http: reqwest::blocking::Client,
}

impl EumetViewClient {
    pub(crate) fn new() -> Result<Self, String> {
        let http = reqwest::blocking::Client::builder()
            .user_agent(concat!("BowEcho/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(std::time::Duration::from_secs(15))
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|error| format!("could not build EUMETView client: {error}"))?;
        Ok(Self { http })
    }

    pub(crate) fn capabilities(&self) -> Result<Vec<LayerCapability>, String> {
        let response = self
            .http
            .get(EUMETVIEW_WMS_URL)
            .query(&[
                ("service", "WMS"),
                ("version", "1.3.0"),
                ("request", "GetCapabilities"),
            ])
            .send()
            .map_err(|error| format!("EUMETView capabilities request failed: {error}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("EUMETView capabilities returned HTTP {status}"));
        }
        let text = response
            .text()
            .map_err(|error| format!("EUMETView capabilities body failed: {error}"))?;
        parse_capabilities(&text)
    }

    pub(crate) fn fetch_map(&self, request: &GetMapRequest) -> Result<WmsImage, String> {
        let url = request.url()?;
        let response = self
            .http
            .get(url)
            .send()
            .map_err(|error| format!("EUMETView image request failed: {error}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("EUMETView image returned HTTP {status}"));
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !content_type.starts_with("image/png") && !content_type.starts_with("image/jpeg") {
            return Err(format!(
                "EUMETView returned '{content_type}' instead of an image"
            ));
        }
        if response.content_length().is_some_and(|bytes| bytes > MAX_IMAGE_BYTES) {
            return Err(format!(
                "EUMETView image exceeds BowEcho's {} MiB response limit",
                MAX_IMAGE_BYTES / (1024 * 1024)
            ));
        }
        let bytes = response
            .bytes()
            .map_err(|error| format!("EUMETView image body failed: {error}"))?;
        if bytes.len() as u64 > MAX_IMAGE_BYTES {
            return Err(format!(
                "EUMETView image exceeds BowEcho's {} MiB response limit",
                MAX_IMAGE_BYTES / (1024 * 1024)
            ));
        }
        let image = decode_image(&bytes, &content_type)?;
        if image.width != request.width as usize || image.height != request.height as usize {
            return Err(format!(
                "EUMETView returned {}x{} for a {}x{} request",
                image.width, image.height, request.width, request.height
            ));
        }
        Ok(image)
    }
}

fn decode_image(bytes: &[u8], content_type: &str) -> Result<WmsImage, String> {
    let format = if content_type.starts_with("image/jpeg") {
        ImageFormat::Jpeg
    } else {
        ImageFormat::Png
    };
    let image = image::load_from_memory_with_format(bytes, format)
        .map_err(|error| format!("EUMETView image decode failed: {error}"))?
        .into_rgba8();
    let (width, height) = image.dimensions();
    let mut rgb = Vec::with_capacity(width as usize * height as usize * 3);
    let mut alpha = Vec::with_capacity(width as usize * height as usize);
    for pixel in image.pixels() {
        rgb.extend_from_slice(&pixel.0[..3]);
        alpha.push(pixel.0[3]);
    }
    Ok(WmsImage {
        width: width as usize,
        height: height as usize,
        rgb,
        alpha,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Timelike};

    const SAMPLE: &str = r#"<?xml version="1.0"?>
      <WMS_Capabilities xmlns="http://www.opengis.net/wms">
        <Capability><Layer><Title>root</Title><Layer>
          <Name>mtg_fd:rgb_geocolour</Name>
          <Title>Geo Colour RGB - MTG-I - 0 degree</Title>
          <EX_GeographicBoundingBox>
            <westBoundLongitude>-81.25</westBoundLongitude>
            <eastBoundLongitude>81.25</eastBoundLongitude>
            <southBoundLatitude>-77.3</southBoundLatitude>
            <northBoundLatitude>77.3</northBoundLatitude>
          </EX_GeographicBoundingBox>
          <Dimension name="time" default="2026-07-11T18:20:00Z" nearestValue="1">2024-09-23T00:00:00.000Z/2026-07-11T18:20:00.000Z/PT10M</Dimension>
        </Layer><Layer>
          <Name>mtg_fd:li_afa</Name><Title>LI Accumulated Flash Area</Title>
          <EX_GeographicBoundingBox><westBoundLongitude>-70</westBoundLongitude><eastBoundLongitude>70</eastBoundLongitude><southBoundLatitude>-70</southBoundLatitude><northBoundLatitude>70</northBoundLatitude></EX_GeographicBoundingBox>
          <Dimension name="time" default="2026-07-11T18:35:00Z">2025-05-30T15:00:00.000Z/2026-07-11T18:35:00.000Z/PT5M</Dimension>
        </Layer></Layer></Capability>
      </WMS_Capabilities>"#;

    #[test]
    fn product_catalog_covers_imagery_and_lightning() {
        assert_eq!(MtgProduct::ALL.len(), 11);
        assert_eq!(MtgProduct::parse("mtg_fd:ir105_hrfi"), Some(MtgProduct::Ir105Hrfi));
        assert_eq!(MtgProduct::parse("li_afa"), Some(MtgProduct::LightningAfa));
        assert_eq!(MtgProduct::LightningAfa.cadence_minutes(), 5);
    }

    #[test]
    fn capabilities_parse_bbox_default_time_and_cadence() {
        let layers = parse_capabilities(SAMPLE).expect("capabilities");
        assert_eq!(layers.len(), 2);
        let geo = &layers[0];
        assert_eq!(geo.product, MtgProduct::GeoColour);
        assert_eq!(geo.bounds.west_deg, -81.25);
        assert_eq!(geo.latest_time, Utc.with_ymd_and_hms(2026, 7, 11, 18, 20, 0).unwrap());
        assert_eq!(geo.cadence_minutes, 10);
        let lightning = &layers[1];
        assert_eq!(lightning.product, MtgProduct::LightningAfa);
        assert_eq!(lightning.cadence_minutes, 5);
        let times = lightning.recent_times(3);
        assert_eq!(times.len(), 3);
        assert_eq!(times[0].minute(), 25);
        assert_eq!(times[2].minute(), 35);
    }

    #[test]
    fn get_map_uses_crs84_explicit_time_and_bounded_size() {
        let request = GetMapRequest {
            product: MtgProduct::GeoColour,
            time: Utc.with_ymd_and_hms(2026, 7, 11, 18, 20, 0).unwrap(),
            bounds: WmsBounds { west_deg: -20.0, south_deg: 20.0, east_deg: 30.0, north_deg: 60.0 },
            width: 1024,
            height: 800,
        };
        let url = request.url().expect("URL");
        let pairs = url.query_pairs().into_owned().collect::<std::collections::HashMap<_, _>>();
        assert_eq!(pairs.get("crs").map(String::as_str), Some("CRS:84"));
        assert_eq!(pairs.get("bbox").map(String::as_str), Some("-20,20,30,60"));
        assert_eq!(pairs.get("time").map(String::as_str), Some("2026-07-11T18:20:00Z"));
        assert_eq!(pairs.get("layers").map(String::as_str), Some("mtg_fd:rgb_geocolour"));
    }

    #[test]
    fn image_size_preserves_bbox_aspect_and_caps_edges() {
        let (width, height) = image_size_for_bounds(
            WmsBounds { west_deg: -70.0, south_deg: -35.0, east_deg: 70.0, north_deg: 35.0 },
            1_600,
        );
        assert_eq!((width, height), (1_600, 800));
        let (width, height) = image_size_for_bounds(
            WmsBounds { west_deg: -10.0, south_deg: -70.0, east_deg: 10.0, north_deg: 70.0 },
            4_096,
        );
        assert_eq!(height, MAX_IMAGE_EDGE);
        assert_eq!(width, 293);
    }

    /// One-shot production-service proof. Kept ignored so ordinary workspace
    /// tests remain deterministic; release work runs it explicitly on a node.
    #[test]
    #[ignore = "live EUMETView network smoke"]
    fn live_public_geocolour_capabilities_and_png_round_trip() {
        let client = EumetViewClient::new().expect("client");
        let capability = client
            .capabilities()
            .expect("live capabilities")
            .into_iter()
            .find(|layer| layer.product == MtgProduct::GeoColour)
            .expect("Geo Colour layer");
        let image = client
            .fetch_map(&GetMapRequest {
                product: MtgProduct::GeoColour,
                time: capability.latest_time,
                bounds: capability.bounds,
                width: 256,
                height: 256,
            })
            .expect("live PNG");
        assert_eq!((image.width, image.height), (256, 256));
        assert_eq!(image.rgb.len(), 256 * 256 * 3);
        assert_eq!(image.alpha.len(), 256 * 256);
        assert!(image.alpha.iter().any(|alpha| *alpha > 0));
    }
}
