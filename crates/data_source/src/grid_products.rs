//! Catalog surface for gridded/composite radar and warning products.
//!
//! This is deliberately separate from `international::IntlProvider`.
//! International providers describe site-centered polar volumes that decode
//! into `RadarVolume`; these entries describe time-indexed grids, rasters,
//! nowcasts, QPE products, and warning polygons. The catalog gives the UI and
//! future decoders a typed target without pretending that every European
//! product is a radar site.

use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
use reqwest::header::{ACCEPT, CONTENT_TYPE, REFERER};
use serde::{Deserialize, Serialize};

pub mod imgw;

const ITALY_DPC_API_BASE: &str = "https://radar-api.protezionecivile.it";
const ITALY_DPC_ORIGIN: &str = "https://radar.protezionecivile.it";
const ITALY_DPC_WMTS_BASE: &str = "https://radar-geowebcache.protezionecivile.it/service/wmts";
const ITALY_DPC_WMTS_MATRIX_SET: &str = "EPSG:900913";
const ITALY_DPC_WMTS_FORMAT: &str = "image/png";
const TAIWAN_CWA_DATASET_ID: &str = "O-A0059-001";
const TAIWAN_CWA_DEFAULT_AUTHORIZATION: &str = "rdec-key-123-45678-011121314";
const TAIWAN_CWA_FILE_API_BASE: &str = "https://opendata.cwa.gov.tw/fileapi/v1/opendataapi";
const TAIWAN_CWA_HISTORY_API_BASE: &str = "https://opendata.cwa.gov.tw/historyapi/v1";

/// The product family a gridded source contributes to BowEcho.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GridProductKind {
    MaxReflectivity,
    ReflectivityComposite,
    RainRate,
    Accumulation,
    Qpe,
    Nowcast,
    EchoTops,
    ConstantAltitudePpi,
    HailProbability,
    CellTracking,
    RotationTracks,
    ThreeDimensionalComposite,
    VerticallyIntegratedLiquid,
    VerticalMaximumIntensity,
    DualPolarizationMaximum,
    HeavyRainDetection,
    Lightning,
    RadarStatus,
    CloudCover,
    Temperature,
    Warning,
    Discovery,
}

/// Container/transfer format the product is expected to use.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GridCodec {
    OdimH5Grid,
    CloudOptimizedGeoTiff,
    Grib2,
    GeoTiff,
    Hdf5Grid,
    NetcdfGrid,
    Zarr,
    GeoReferencedImage,
    GeoJson,
    EdrJson,
    ApiJson,
    MqttNotification,
}

/// How BowEcho should discover or fetch the product once a decoder exists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GridAccess {
    AnonymousBucket,
    OpenHttp,
    RestApi,
    EdrApi,
    Mqtt,
    WebSocket,
    WmsWmts,
    PortalDownload,
}

/// Current implementation state. `Catalogued` is intentionally not
/// user-visible ingest: it means the product is known and typed, and still
/// needs the matching decoder/fetcher before it can be displayed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GridImplementationStatus {
    Catalogued,
    Fetchable,
    DecoderNeeded,
}

/// One gridded/composite product available from a provider.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridProduct {
    pub slug: &'static str,
    pub label: &'static str,
    pub kind: GridProductKind,
    pub cadence_minutes: Option<u16>,
    pub resolution_km: Option<f32>,
    pub forecast_hours: Option<u16>,
    pub codecs: &'static [GridCodec],
    pub access: &'static [GridAccess],
    pub status: GridImplementationStatus,
    pub source_hint: &'static str,
}

/// Static implementation of the future `GridProductProvider` path.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StaticGridProductProvider {
    pub id: &'static str,
    pub label: &'static str,
    pub region: &'static str,
    pub docs_url: &'static str,
    pub products: &'static [GridProduct],
}

/// A source of time-indexed gridded radar products or companion alert layers.
pub trait GridProductProvider: Send + Sync {
    fn id(&self) -> &'static str;
    fn label(&self) -> &'static str;
    fn region(&self) -> &'static str;
    fn products(&self) -> &'static [GridProduct];
}

impl GridProductProvider for StaticGridProductProvider {
    fn id(&self) -> &'static str {
        self.id
    }

    fn label(&self) -> &'static str {
        self.label
    }

    fn region(&self) -> &'static str {
        self.region
    }

    fn products(&self) -> &'static [GridProduct] {
        self.products
    }
}

/// One Italy DPC v2 REST-downloadable product.
///
/// These product type ids are the values accepted by
/// `findLastProductByType` and `downloadProduct`. They are kept separate
/// from the display catalog because a few DPC layers, such as `SITES`, are
/// visible through WMTS/status paths but are not downloadable through the
/// raw-file endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ItalyDpcProductSpec {
    pub product_type: &'static str,
    pub slug: &'static str,
}

/// One Italy DPC WMTS layer available as a Web-Mercator PNG tile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ItalyDpcWmtsLayerSpec {
    pub key: &'static str,
    pub wmts_layer: &'static str,
    pub style: &'static str,
    pub product_type: Option<&'static str>,
    pub product_slug: Option<&'static str>,
}

const ITALY_DPC_FETCHABLE_PRODUCTS: &[ItalyDpcProductSpec] = &[
    ItalyDpcProductSpec {
        product_type: "VMI",
        slug: "italy-dpc-vmi",
    },
    ItalyDpcProductSpec {
        product_type: "SRI",
        slug: "italy-dpc-sri",
    },
    ItalyDpcProductSpec {
        product_type: "SRT1",
        slug: "italy-dpc-srt",
    },
    ItalyDpcProductSpec {
        product_type: "CUM3",
        slug: "italy-dpc-cum3",
    },
    ItalyDpcProductSpec {
        product_type: "CUM6",
        slug: "italy-dpc-cum6",
    },
    ItalyDpcProductSpec {
        product_type: "CUM12",
        slug: "italy-dpc-cum12",
    },
    ItalyDpcProductSpec {
        product_type: "CUM24",
        slug: "italy-dpc-cum24",
    },
    ItalyDpcProductSpec {
        product_type: "CAPPI_1",
        slug: "italy-dpc-cappi-1km",
    },
    ItalyDpcProductSpec {
        product_type: "CAPPI_2",
        slug: "italy-dpc-cappi-2km",
    },
    ItalyDpcProductSpec {
        product_type: "CAPPI_3",
        slug: "italy-dpc-cappi-3km",
    },
    ItalyDpcProductSpec {
        product_type: "CAPPI_4",
        slug: "italy-dpc-cappi-4km",
    },
    ItalyDpcProductSpec {
        product_type: "CAPPI_5",
        slug: "italy-dpc-cappi-5km",
    },
    ItalyDpcProductSpec {
        product_type: "CAPPI_6",
        slug: "italy-dpc-cappi-6km",
    },
    ItalyDpcProductSpec {
        product_type: "CAPPI_7",
        slug: "italy-dpc-cappi-7km",
    },
    ItalyDpcProductSpec {
        product_type: "CAPPI_8",
        slug: "italy-dpc-cappi-8km",
    },
    ItalyDpcProductSpec {
        product_type: "CAPPI_9",
        slug: "italy-dpc-cappi-9km",
    },
    ItalyDpcProductSpec {
        product_type: "CAPPI_10",
        slug: "italy-dpc-cappi-10km",
    },
    ItalyDpcProductSpec {
        product_type: "VIL",
        slug: "italy-dpc-vil",
    },
    ItalyDpcProductSpec {
        product_type: "ETM",
        slug: "italy-dpc-etm",
    },
    ItalyDpcProductSpec {
        product_type: "POH",
        slug: "italy-dpc-poh",
    },
    ItalyDpcProductSpec {
        product_type: "IR_108",
        slug: "italy-dpc-ir108",
    },
    ItalyDpcProductSpec {
        product_type: "TEMP",
        slug: "italy-dpc-temp",
    },
];

const ITALY_DPC_WMTS_LAYERS: &[ItalyDpcWmtsLayerSpec] = &[
    ItalyDpcWmtsLayerSpec {
        key: "vmi",
        wmts_layer: "radar:vmi",
        style: "vmi",
        product_type: Some("VMI"),
        product_slug: Some("italy-dpc-vmi"),
    },
    ItalyDpcWmtsLayerSpec {
        key: "sri",
        wmts_layer: "radar:sri",
        style: "sri",
        product_type: Some("SRI"),
        product_slug: Some("italy-dpc-sri"),
    },
    ItalyDpcWmtsLayerSpec {
        key: "srt1",
        wmts_layer: "radar:srt1",
        style: "srt",
        product_type: Some("SRT1"),
        product_slug: Some("italy-dpc-srt"),
    },
    ItalyDpcWmtsLayerSpec {
        key: "srt3",
        wmts_layer: "radar:srt3",
        style: "srt",
        product_type: Some("CUM3"),
        product_slug: Some("italy-dpc-cum3"),
    },
    ItalyDpcWmtsLayerSpec {
        key: "srt6",
        wmts_layer: "radar:srt6",
        style: "srt",
        product_type: Some("CUM6"),
        product_slug: Some("italy-dpc-cum6"),
    },
    ItalyDpcWmtsLayerSpec {
        key: "srt12",
        wmts_layer: "radar:srt12",
        style: "srt",
        product_type: Some("CUM12"),
        product_slug: Some("italy-dpc-cum12"),
    },
    ItalyDpcWmtsLayerSpec {
        key: "srt24",
        wmts_layer: "radar:srt24",
        style: "srt",
        product_type: Some("CUM24"),
        product_slug: Some("italy-dpc-cum24"),
    },
    ItalyDpcWmtsLayerSpec {
        key: "hrd",
        wmts_layer: "radar:hrd",
        style: "polygon",
        product_type: None,
        product_slug: Some("italy-dpc-heavy-rain"),
    },
    ItalyDpcWmtsLayerSpec {
        key: "radardpc",
        wmts_layer: "radar:radardpc",
        style: "radardpc",
        product_type: None,
        product_slug: Some("italy-dpc-sites"),
    },
    ItalyDpcWmtsLayerSpec {
        key: "ir108",
        wmts_layer: "radar:ir108",
        style: "ir108",
        product_type: Some("IR_108"),
        product_slug: Some("italy-dpc-ir108"),
    },
    ItalyDpcWmtsLayerSpec {
        key: "temperature",
        wmts_layer: "radar:temperature",
        style: "temperature",
        product_type: Some("TEMP"),
        product_slug: Some("italy-dpc-temp"),
    },
];

/// Italy DPC product types that BowEcho can plan through the v2 REST API.
pub fn italy_dpc_fetchable_products() -> &'static [ItalyDpcProductSpec] {
    ITALY_DPC_FETCHABLE_PRODUCTS
}

/// Italy DPC layers that can be fetched through the OGC WMTS endpoint.
pub fn italy_dpc_wmts_layers() -> &'static [ItalyDpcWmtsLayerSpec] {
    ITALY_DPC_WMTS_LAYERS
}

/// Build a DPC WMTS GetTile URL for an EPSG:900913/Web-Mercator tile.
///
/// Passing `None` for `time` intentionally requests DPC's current value for
/// the layer. Renderers that cache tiles should pass an explicit product
/// timestamp and include that timestamp in their cache key.
pub fn italy_dpc_wmts_tile_url(
    layer_key: &str,
    zoom: u8,
    tile_col: u32,
    tile_row: u32,
    time: Option<DateTime<Utc>>,
) -> Result<String, String> {
    let layer = italy_dpc_wmts_layer(layer_key)?;
    let mut url = format!(
        "{ITALY_DPC_WMTS_BASE}?SERVICE=WMTS&REQUEST=GetTile&VERSION=1.0.0&LAYER={}&STYLE={}&TILEMATRIXSET={ITALY_DPC_WMTS_MATRIX_SET}&TILEMATRIX={ITALY_DPC_WMTS_MATRIX_SET}:{zoom}&TILEROW={tile_row}&TILECOL={tile_col}&FORMAT={ITALY_DPC_WMTS_FORMAT}",
        layer.wmts_layer, layer.style
    );
    if let Some(time) = time {
        url.push_str("&TIME=");
        url.push_str(&format_italy_dpc_wmts_time(time));
    }
    Ok(url)
}

fn italy_dpc_wmts_layer(layer_key: &str) -> Result<&'static ItalyDpcWmtsLayerSpec, String> {
    let key = layer_key.trim();
    ITALY_DPC_WMTS_LAYERS
        .iter()
        .find(|layer| {
            layer.key.eq_ignore_ascii_case(key)
                || layer.wmts_layer.eq_ignore_ascii_case(key)
                || layer
                    .product_type
                    .map(|product_type| product_type.eq_ignore_ascii_case(key))
                    .unwrap_or(false)
                || layer
                    .product_slug
                    .map(|slug| slug.eq_ignore_ascii_case(key))
                    .unwrap_or(false)
        })
        .ok_or_else(|| format!("Italy DPC: unsupported WMTS layer '{layer_key}'"))
}

fn format_italy_dpc_wmts_time(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Latest timestamp metadata for one Italy DPC product.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ItalyDpcLatestProduct {
    pub product_type: String,
    pub product_time_millis: i64,
    pub period: String,
}

impl ItalyDpcLatestProduct {
    pub fn time_utc(&self) -> Option<DateTime<Utc>> {
        Utc.timestamp_millis_opt(self.product_time_millis).single()
    }
}

/// Download plan for one Italy DPC raw product file.
///
/// The URL is short-lived. Use `identity` for dedupe/cache keys because it is
/// derived from the stable S3 bucket/key and ignores the expiring signature.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ItalyDpcDownloadPlan {
    pub product_type: String,
    pub product_time_millis: i64,
    pub period: Option<String>,
    pub bucket: String,
    pub key: String,
    pub url: String,
    pub expires_seconds: Option<u32>,
    pub identity: String,
}

/// Query the current DPC timestamp for a downloadable product type.
pub fn italy_dpc_latest_product(product_type: &str) -> Result<ItalyDpcLatestProduct, String> {
    let product_type = canonical_italy_dpc_product_type(product_type)?;
    let url = format!(
        "{ITALY_DPC_API_BASE}/findLastProductByType?type={product_type}&origin={ITALY_DPC_ORIGIN}"
    );
    let text =
        crate::fetch_text(&url).map_err(|err| format!("Italy DPC latest {product_type}: {err}"))?;
    parse_italy_dpc_latest_response(&text, product_type)
}

/// Request a short-lived raw-file URL for a known DPC product timestamp.
pub fn italy_dpc_download_plan(
    product_type: &str,
    product_time_millis: i64,
) -> Result<ItalyDpcDownloadPlan, String> {
    let product_type = canonical_italy_dpc_product_type(product_type)?;
    request_italy_dpc_download_plan(product_type, product_time_millis, None)
}

/// Query the latest timestamp, then request a short-lived raw-file URL.
pub fn italy_dpc_latest_download_plan(product_type: &str) -> Result<ItalyDpcDownloadPlan, String> {
    let latest = italy_dpc_latest_product(product_type)?;
    request_italy_dpc_download_plan(
        &latest.product_type,
        latest.product_time_millis,
        Some(latest.period),
    )
}

fn request_italy_dpc_download_plan(
    product_type: &str,
    product_time_millis: i64,
    period: Option<String>,
) -> Result<ItalyDpcDownloadPlan, String> {
    let client = crate::metadata_http_client();
    let request = ItalyDpcDownloadRequest {
        product_type,
        product_date: product_time_millis,
    };
    let body = serde_json::to_string(&request)
        .map_err(|err| format!("Italy DPC download {product_type}: JSON encode failed: {err}"))?;
    let response = client
        .post(format!("{ITALY_DPC_API_BASE}/downloadProduct"))
        .header(ACCEPT, "application/json,*/*")
        .header(CONTENT_TYPE, "application/json")
        .header("origin", ITALY_DPC_ORIGIN)
        .header(REFERER, format!("{ITALY_DPC_ORIGIN}/"))
        .body(body)
        .send()
        .and_then(|response| response.error_for_status())
        .map_err(|err| {
            format!(
                "Italy DPC download {product_type}: {}",
                crate::reqwest_error_chain(&err)
            )
        })?
        .text()
        .map_err(|err| {
            format!(
                "Italy DPC download {product_type}: {}",
                crate::reqwest_error_chain(&err)
            )
        })?;
    let parsed = parse_italy_dpc_download_response(&response, product_type, product_time_millis)?;
    Ok(ItalyDpcDownloadPlan {
        product_type: product_type.to_owned(),
        product_time_millis,
        period,
        identity: format!("italy-dpc/{}/{}", parsed.bucket, parsed.key),
        bucket: parsed.bucket,
        key: parsed.key,
        url: parsed.url,
        expires_seconds: parsed.expires_seconds,
    })
}

fn canonical_italy_dpc_product_type(product_type: &str) -> Result<&'static str, String> {
    ITALY_DPC_FETCHABLE_PRODUCTS
        .iter()
        .find(|spec| spec.product_type.eq_ignore_ascii_case(product_type.trim()))
        .map(|spec| spec.product_type)
        .ok_or_else(|| format!("Italy DPC: unsupported downloadable product type '{product_type}'"))
}

fn parse_italy_dpc_latest_response(
    text: &str,
    product_type: &str,
) -> Result<ItalyDpcLatestProduct, String> {
    let response: ItalyDpcLatestResponse = serde_json::from_str(text)
        .map_err(|err| format!("Italy DPC latest {product_type}: JSON parse failed: {err}"))?;
    if response.total == 0 || response.last_products.is_empty() {
        return Err(format!(
            "Italy DPC latest {product_type}: no product available"
        ));
    }
    let latest = response
        .last_products
        .into_iter()
        .find(|product| product.product_type.eq_ignore_ascii_case(product_type))
        .ok_or_else(|| {
            format!("Italy DPC latest {product_type}: response did not include requested product")
        })?;
    Ok(ItalyDpcLatestProduct {
        product_type: product_type.to_owned(),
        product_time_millis: latest.time,
        period: latest.period,
    })
}

fn parse_italy_dpc_download_response(
    text: &str,
    product_type: &str,
    product_time_millis: i64,
) -> Result<ItalyDpcDownloadResponse, String> {
    let response: ItalyDpcDownloadResponse = serde_json::from_str(text)
        .map_err(|err| format!("Italy DPC download {product_type}: JSON parse failed: {err}"))?;
    if response.bucket.is_empty() || response.key.is_empty() || response.url.is_empty() {
        return Err(format!(
            "Italy DPC download {product_type}/{product_time_millis}: missing bucket, key, or url"
        ));
    }
    Ok(response)
}

#[derive(Debug, Deserialize)]
struct ItalyDpcLatestResponse {
    total: usize,
    #[serde(rename = "lastProducts", default)]
    last_products: Vec<ItalyDpcLatestProductResponse>,
}

#[derive(Debug, Deserialize)]
struct ItalyDpcLatestProductResponse {
    #[serde(rename = "productType")]
    product_type: String,
    time: i64,
    period: String,
}

#[derive(Debug, Serialize)]
struct ItalyDpcDownloadRequest<'a> {
    #[serde(rename = "productType")]
    product_type: &'a str,
    #[serde(rename = "productDate")]
    product_date: i64,
}

#[derive(Debug, Deserialize)]
struct ItalyDpcDownloadResponse {
    bucket: String,
    key: String,
    url: String,
    #[serde(rename = "expiresSeconds")]
    expires_seconds: Option<u32>,
}

/// Numeric Taiwan CWA composite reflectivity grid.
///
/// The official `O-A0059-001` payload is a lon/lat grid, not native polar
/// site data. Values are stored with the lower-left point first, then
/// west-to-east rows, then south-to-north rows.
#[derive(Clone, Debug, PartialEq)]
pub struct TaiwanCwaRadarGrid {
    pub time: DateTime<Utc>,
    pub nx: usize,
    pub ny: usize,
    pub start_lon: f32,
    pub start_lat: f32,
    pub resolution_deg: f32,
    pub units: String,
    pub values: Vec<f32>,
}

impl TaiwanCwaRadarGrid {
    pub fn value_at_source_xy(&self, x: usize, y: usize) -> Option<f32> {
        (x < self.nx && y < self.ny).then(|| self.values[y * self.nx + x])
    }

    pub fn source_identity(&self) -> String {
        format!(
            "taiwan-cwa/{TAIWAN_CWA_DATASET_ID}/{}",
            self.time.to_rfc3339()
        )
    }
}

pub fn taiwan_cwa_latest_radar_grid() -> Result<TaiwanCwaRadarGrid, String> {
    let url = taiwan_cwa_latest_json_url();
    let text = crate::fetch_listing_text(&url)
        .map_err(|err| format!("Taiwan CWA latest radar download: {err}"))?;
    parse_taiwan_cwa_latest_json(&text)
}

pub fn taiwan_cwa_latest_json_url() -> String {
    format!(
        "{TAIWAN_CWA_FILE_API_BASE}/{TAIWAN_CWA_DATASET_ID}?Authorization={}&format=JSON",
        taiwan_cwa_authorization()
    )
}

pub fn taiwan_cwa_history_metadata_url() -> String {
    format!(
        "{TAIWAN_CWA_HISTORY_API_BASE}/getMetadata/{TAIWAN_CWA_DATASET_ID}?Authorization={}",
        taiwan_cwa_authorization()
    )
}

fn taiwan_cwa_authorization() -> String {
    std::env::var("CWA_AUTHORIZATION")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| TAIWAN_CWA_DEFAULT_AUTHORIZATION.to_owned())
}

pub fn parse_taiwan_cwa_latest_json(text: &str) -> Result<TaiwanCwaRadarGrid, String> {
    let payload: TaiwanCwaLatestPayload = serde_json::from_str(text)
        .map_err(|err| format!("Taiwan CWA latest radar JSON parse failed: {err}"))?;
    let open_data = payload.cwaopendata;
    if open_data.dataid.trim() != TAIWAN_CWA_DATASET_ID {
        return Err(format!(
            "Taiwan CWA latest radar: expected dataid {TAIWAN_CWA_DATASET_ID}, got {}",
            open_data.dataid
        ));
    }
    let params = open_data.dataset.dataset_info.parameter_set;
    let nx = parse_usize_field("GridDimensionX", &params.grid_dimension_x)?;
    let ny = parse_usize_field("GridDimensionY", &params.grid_dimension_y)?;
    let expected = nx
        .checked_mul(ny)
        .ok_or_else(|| "Taiwan CWA latest radar grid dimensions overflow".to_owned())?;
    let time = DateTime::parse_from_rfc3339(params.date_time.trim())
        .map_err(|err| format!("Taiwan CWA DateTime parse failed: {err}"))?
        .with_timezone(&Utc);
    let values = parse_taiwan_cwa_values(&open_data.dataset.contents.content, expected)?;
    Ok(TaiwanCwaRadarGrid {
        time,
        nx,
        ny,
        start_lon: parse_f32_field("StartPointLongitude", &params.start_point_longitude)?,
        start_lat: parse_f32_field("StartPointLatitude", &params.start_point_latitude)?,
        resolution_deg: parse_f32_field("GridResolution", &params.grid_resolution)?,
        units: params.reflectivity.unwrap_or_else(|| "dBZ".to_owned()),
        values,
    })
}

pub fn parse_taiwan_cwa_history_product_urls(text: &str) -> Result<Vec<String>, String> {
    use quick_xml::Reader;
    use quick_xml::events::Event;

    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut in_product_url = false;
    let mut urls = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(event)) => {
                in_product_url = event.name().as_ref().eq_ignore_ascii_case(b"ProductURL");
            }
            Ok(Event::Text(text)) if in_product_url => {
                let value = text
                    .decode()
                    .map_err(|err| format!("Taiwan CWA metadata text decode failed: {err}"))?
                    .trim()
                    .to_owned();
                if !value.is_empty() {
                    urls.push(value);
                }
            }
            Ok(Event::End(event)) => {
                if event.name().as_ref().eq_ignore_ascii_case(b"ProductURL") {
                    in_product_url = false;
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(err) => return Err(format!("Taiwan CWA metadata XML parse failed: {err}")),
        }
        buf.clear();
    }
    Ok(urls)
}

pub fn taiwan_cwa_is_nodata(value: f32) -> bool {
    !value.is_finite() || (value + 99.0).abs() < 0.01 || (value + 999.0).abs() < 0.01
}

fn parse_taiwan_cwa_values(text: &str, expected: usize) -> Result<Vec<f32>, String> {
    let mut values = Vec::with_capacity(expected);
    for token in text.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let value = token
            .parse::<f32>()
            .map_err(|err| format!("Taiwan CWA grid value parse failed for '{token}': {err}"))?;
        values.push(value);
    }
    if values.len() != expected {
        return Err(format!(
            "Taiwan CWA grid value count mismatch: got {}, expected {expected}",
            values.len()
        ));
    }
    Ok(values)
}

fn parse_f32_field(label: &str, value: &str) -> Result<f32, String> {
    value
        .trim()
        .parse::<f32>()
        .map_err(|err| format!("Taiwan CWA {label} parse failed for '{value}': {err}"))
}

fn parse_usize_field(label: &str, value: &str) -> Result<usize, String> {
    value
        .trim()
        .parse::<usize>()
        .map_err(|err| format!("Taiwan CWA {label} parse failed for '{value}': {err}"))
}

#[derive(Debug, Deserialize)]
struct TaiwanCwaLatestPayload {
    cwaopendata: TaiwanCwaOpenData,
}

#[derive(Debug, Deserialize)]
struct TaiwanCwaOpenData {
    dataid: String,
    dataset: TaiwanCwaDataset,
}

#[derive(Debug, Deserialize)]
struct TaiwanCwaDataset {
    #[serde(rename = "datasetInfo")]
    dataset_info: TaiwanCwaDatasetInfo,
    contents: TaiwanCwaContents,
}

#[derive(Debug, Deserialize)]
struct TaiwanCwaDatasetInfo {
    #[serde(rename = "parameterSet")]
    parameter_set: TaiwanCwaParameterSet,
}

#[derive(Debug, Deserialize)]
struct TaiwanCwaParameterSet {
    #[serde(rename = "StartPointLongitude")]
    start_point_longitude: String,
    #[serde(rename = "StartPointLatitude")]
    start_point_latitude: String,
    #[serde(rename = "GridResolution")]
    grid_resolution: String,
    #[serde(rename = "DateTime")]
    date_time: String,
    #[serde(rename = "GridDimensionX")]
    grid_dimension_x: String,
    #[serde(rename = "GridDimensionY")]
    grid_dimension_y: String,
    #[serde(rename = "Reflectivity")]
    reflectivity: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TaiwanCwaContents {
    content: String,
}

const OPERA_CODECS: &[GridCodec] = &[GridCodec::OdimH5Grid, GridCodec::CloudOptimizedGeoTiff];
const MRMS_CODECS: &[GridCodec] = &[GridCodec::Grib2];
const ORD_API_CODECS: &[GridCodec] = &[
    GridCodec::EdrJson,
    GridCodec::ApiJson,
    GridCodec::MqttNotification,
];
const HDF5_GRID: &[GridCodec] = &[GridCodec::Hdf5Grid];
const KNMI_GRID: &[GridCodec] = &[GridCodec::Hdf5Grid, GridCodec::NetcdfGrid];
const SWISS_GRID: &[GridCodec] = &[GridCodec::Hdf5Grid, GridCodec::NetcdfGrid];
const IMGW_CMAX_CODECS: &[GridCodec] = &[GridCodec::OdimH5Grid];
const RADOLAN_CODECS: &[GridCodec] = &[GridCodec::ApiJson, GridCodec::Hdf5Grid];
const ITALY_CODECS: &[GridCodec] = &[
    GridCodec::GeoTiff,
    GridCodec::GeoReferencedImage,
    GridCodec::Zarr,
];
const TAIWAN_CWA_CODECS: &[GridCodec] = &[GridCodec::ApiJson, GridCodec::GeoReferencedImage];
const IMAGE_CODECS: &[GridCodec] = &[GridCodec::GeoReferencedImage];
const METEOALARM_CODECS: &[GridCodec] = &[
    GridCodec::GeoJson,
    GridCodec::EdrJson,
    GridCodec::MqttNotification,
];

const BUCKET_AND_API: &[GridAccess] = &[GridAccess::AnonymousBucket, GridAccess::RestApi];
const API_AND_MQTT: &[GridAccess] = &[GridAccess::RestApi, GridAccess::Mqtt, GridAccess::EdrApi];
const OPEN_BUCKET: &[GridAccess] = &[GridAccess::AnonymousBucket];
const REST_API: &[GridAccess] = &[GridAccess::RestApi];
const ITALY_DPC_ACCESS: &[GridAccess] = &[
    GridAccess::RestApi,
    GridAccess::WmsWmts,
    GridAccess::WebSocket,
];
const ITALY_DPC_WMTS: &[GridAccess] = &[GridAccess::WmsWmts];
const OPEN_HTTP: &[GridAccess] = &[GridAccess::OpenHttp];
const PORTAL_DOWNLOAD: &[GridAccess] = &[GridAccess::PortalDownload];

const OPERA_PRODUCTS: &[GridProduct] = &[
    GridProduct {
        slug: "opera-cirrus-max-reflectivity",
        label: "Europe OPERA/CIRRUS Max Reflectivity",
        kind: GridProductKind::MaxReflectivity,
        cadence_minutes: Some(5),
        resolution_km: Some(1.0),
        forecast_hours: None,
        codecs: OPERA_CODECS,
        access: BUCKET_AND_API,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "ORD OPERA composite prefix; gridded, not a polar site",
    },
    GridProduct {
        slug: "opera-rain-rate",
        label: "Europe OPERA Rain Rate",
        kind: GridProductKind::RainRate,
        cadence_minutes: Some(5),
        resolution_km: Some(1.0),
        forecast_hours: None,
        codecs: OPERA_CODECS,
        access: BUCKET_AND_API,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "ORD OPERA composite instantaneous precipitation product",
    },
    GridProduct {
        slug: "opera-accum-1h",
        label: "Europe OPERA 1h Accumulation",
        kind: GridProductKind::Accumulation,
        cadence_minutes: Some(5),
        resolution_km: Some(1.0),
        forecast_hours: None,
        codecs: OPERA_CODECS,
        access: BUCKET_AND_API,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "ORD OPERA composite one-hour accumulation product",
    },
    GridProduct {
        slug: "ord-api-mqtt-discovery",
        label: "ORD API/MQTT Discovery",
        kind: GridProductKind::Discovery,
        cadence_minutes: None,
        resolution_km: None,
        forecast_hours: None,
        codecs: ORD_API_CODECS,
        access: API_AND_MQTT,
        status: GridImplementationStatus::Catalogued,
        source_hint: "MeteoGate ORD API and notification service for metadata/archive refresh",
    },
];

const MRMS_PRODUCTS: &[GridProduct] = &[
    GridProduct {
        slug: "mrms-composite-reflectivity",
        label: "NOAA MRMS Composite Reflectivity",
        kind: GridProductKind::ReflectivityComposite,
        cadence_minutes: Some(2),
        resolution_km: Some(1.0),
        forecast_hours: None,
        codecs: MRMS_CODECS,
        access: OPEN_HTTP,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "NOAA/NCEP MRMS public 2D GRIB2 grid; decoder/display layer not wired yet",
    },
    GridProduct {
        slug: "mrms-merged-reflectivity-lowest-altitude",
        label: "NOAA MRMS Merged Reflectivity Lowest Altitude",
        kind: GridProductKind::ReflectivityComposite,
        cadence_minutes: Some(2),
        resolution_km: Some(1.0),
        forecast_hours: None,
        codecs: MRMS_CODECS,
        access: OPEN_HTTP,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "NOAA/NCEP MRMS public 2D GRIB2 grid; decoder/display layer not wired yet",
    },
    GridProduct {
        slug: "mrms-precip-rate",
        label: "NOAA MRMS Precipitation Rate",
        kind: GridProductKind::RainRate,
        cadence_minutes: Some(2),
        resolution_km: Some(1.0),
        forecast_hours: None,
        codecs: MRMS_CODECS,
        access: OPEN_HTTP,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "NOAA/NCEP MRMS public 2D GRIB2 grid; decoder/display layer not wired yet",
    },
    GridProduct {
        slug: "mrms-rotation-tracks",
        label: "NOAA MRMS Rotation Tracks",
        kind: GridProductKind::RotationTracks,
        cadence_minutes: Some(2),
        resolution_km: Some(1.0),
        forecast_hours: None,
        codecs: MRMS_CODECS,
        access: OPEN_HTTP,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "NOAA/NCEP MRMS azimuthal-shear/rotation-track family; decoder/display layer not wired yet",
    },
];

const UK_PRODUCTS: &[GridProduct] = &[GridProduct {
    slug: "uk-metoffice-rain-rate",
    label: "UK Met Office Rain Rate",
    kind: GridProductKind::RainRate,
    cadence_minutes: Some(15),
    resolution_km: None,
    forecast_hours: None,
    codecs: HDF5_GRID,
    access: OPEN_BUCKET,
    status: GridImplementationStatus::DecoderNeeded,
    source_hint: "Met Office AWS radar composite HDF5 rolling archive",
}];

const KNMI_PRODUCTS: &[GridProduct] = &[
    GridProduct {
        slug: "knmi-reflectivity-composite",
        label: "Netherlands KNMI Reflectivity Composite",
        kind: GridProductKind::ReflectivityComposite,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: KNMI_GRID,
        access: REST_API,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "KNMI Data Platform radar grid product family",
    },
    GridProduct {
        slug: "knmi-reflectivity-nowcast",
        label: "Netherlands KNMI Reflectivity Nowcast",
        kind: GridProductKind::Nowcast,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: Some(2),
        codecs: KNMI_GRID,
        access: REST_API,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "KNMI 0-2 hour radar nowcast family",
    },
    GridProduct {
        slug: "knmi-radar-gauge-accum",
        label: "Netherlands KNMI Radar/Gauge Accumulation",
        kind: GridProductKind::Qpe,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: KNMI_GRID,
        access: REST_API,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "KNMI radar/gauge quantitative precipitation products",
    },
    GridProduct {
        slug: "knmi-hail-probability",
        label: "Netherlands KNMI Hail Probability",
        kind: GridProductKind::HailProbability,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: KNMI_GRID,
        access: REST_API,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "KNMI radar hail product family",
    },
    GridProduct {
        slug: "knmi-echo-tops",
        label: "Netherlands KNMI Echo Tops",
        kind: GridProductKind::EchoTops,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: KNMI_GRID,
        access: REST_API,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "KNMI echo-top height composite",
    },
    GridProduct {
        slug: "knmi-3d-composite",
        label: "Netherlands KNMI 3D Radar Composite",
        kind: GridProductKind::ThreeDimensionalComposite,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: KNMI_GRID,
        access: REST_API,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "KNMI real-time 3D radar composite",
    },
    GridProduct {
        slug: "knmi-cellwarn",
        label: "Netherlands KNMI CellWarn",
        kind: GridProductKind::CellTracking,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: KNMI_GRID,
        access: REST_API,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "KNMI tracking/risk output",
    },
];

const METEOSWISS_PRODUCTS: &[GridProduct] = &[
    GridProduct {
        slug: "meteoswiss-precip",
        label: "Swiss PRECIP",
        kind: GridProductKind::Qpe,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: SWISS_GRID,
        access: REST_API,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "MeteoSwiss national precipitation radar estimate",
    },
    GridProduct {
        slug: "meteoswiss-combiprecip",
        label: "Swiss CombiPrecip",
        kind: GridProductKind::Qpe,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: SWISS_GRID,
        access: REST_API,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "MeteoSwiss radar plus rain-gauge precipitation estimate",
    },
    GridProduct {
        slug: "meteoswiss-hail",
        label: "Swiss Hail Products",
        kind: GridProductKind::HailProbability,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: SWISS_GRID,
        access: REST_API,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "MeteoSwiss radar hail product family",
    },
    GridProduct {
        slug: "meteoswiss-convection",
        label: "Swiss Convection Products",
        kind: GridProductKind::Nowcast,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: SWISS_GRID,
        access: REST_API,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "MeteoSwiss convection radar product family",
    },
    GridProduct {
        slug: "meteoswiss-polar-3d",
        label: "Swiss Polar 3D Products",
        kind: GridProductKind::ThreeDimensionalComposite,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: SWISS_GRID,
        access: REST_API,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "MeteoSwiss polar 3D radar product family",
    },
];

const DWD_GRID_PRODUCTS: &[GridProduct] = &[
    GridProduct {
        slug: "dwd-radolan-qpe",
        label: "Germany RADOLAN QPE",
        kind: GridProductKind::Qpe,
        cadence_minutes: Some(60),
        resolution_km: None,
        forecast_hours: None,
        codecs: RADOLAN_CODECS,
        access: OPEN_HTTP,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "DWD gauge-adjusted radar precipitation analysis",
    },
    GridProduct {
        slug: "dwd-radvor-nowcast",
        label: "Germany RADVOR Nowcast",
        kind: GridProductKind::Nowcast,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: RADOLAN_CODECS,
        access: OPEN_HTTP,
        status: GridImplementationStatus::DecoderNeeded,
        source_hint: "DWD radar precipitation nowcast product family",
    },
];

const IMGW_POLRAD_PRODUCTS: &[GridProduct] = &[
    GridProduct {
        slug: "imgw-polrad-cmax-kdp",
        label: "Poland IMGW POLRAD CMAX KDP",
        kind: GridProductKind::DualPolarizationMaximum,
        cadence_minutes: Some(5),
        resolution_km: Some(1.0),
        forecast_hours: None,
        codecs: IMGW_CMAX_CODECS,
        access: PORTAL_DOWNLOAD,
        status: GridImplementationStatus::Fetchable,
        source_hint: "IMGW national datastore ODIM_H5 MAX grid; site-centered maximum projection, not a polar volume",
    },
    GridProduct {
        slug: "imgw-polrad-cmax-rhohv",
        label: "Poland IMGW POLRAD CMAX RHOHV",
        kind: GridProductKind::DualPolarizationMaximum,
        cadence_minutes: Some(5),
        resolution_km: Some(1.0),
        forecast_hours: None,
        codecs: IMGW_CMAX_CODECS,
        access: PORTAL_DOWNLOAD,
        status: GridImplementationStatus::Fetchable,
        source_hint: "IMGW national datastore ODIM_H5 MAX grid; quantity availability varies by site",
    },
    GridProduct {
        slug: "imgw-polrad-cmax-zdr",
        label: "Poland IMGW POLRAD CMAX ZDR",
        kind: GridProductKind::DualPolarizationMaximum,
        cadence_minutes: Some(5),
        resolution_km: Some(1.0),
        forecast_hours: None,
        codecs: IMGW_CMAX_CODECS,
        access: PORTAL_DOWNLOAD,
        status: GridImplementationStatus::Fetchable,
        source_hint: "IMGW national datastore ODIM_H5 MAX grid; values require each file's gain/offset",
    },
    GridProduct {
        slug: "imgw-polrad-cmax-phidp",
        label: "Poland IMGW POLRAD CMAX PHIDP",
        kind: GridProductKind::DualPolarizationMaximum,
        cadence_minutes: Some(5),
        resolution_km: Some(1.0),
        forecast_hours: None,
        codecs: IMGW_CMAX_CODECS,
        access: PORTAL_DOWNLOAD,
        status: GridImplementationStatus::Fetchable,
        source_hint: "IMGW national datastore ODIM_H5 MAX grid; availability varies by site and is validated against the live listing when fetched",
    },
];

const ITALY_PRODUCTS: &[GridProduct] = &[
    GridProduct {
        slug: "italy-dpc-vmi",
        label: "Italy DPC VMI",
        kind: GridProductKind::VerticalMaximumIntensity,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: ITALY_CODECS,
        access: ITALY_DPC_ACCESS,
        status: GridImplementationStatus::Fetchable,
        source_hint: "DPC REST type VMI; raw GeoTIFF download plus WMTS tiles, composite not polar site data",
    },
    GridProduct {
        slug: "italy-dpc-sri",
        label: "Italy DPC SRI",
        kind: GridProductKind::RainRate,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: ITALY_CODECS,
        access: ITALY_DPC_ACCESS,
        status: GridImplementationStatus::Fetchable,
        source_hint: "DPC REST type SRI; surface rainfall-intensity GeoTIFF composite",
    },
    GridProduct {
        slug: "italy-dpc-srt",
        label: "Italy DPC SRT 1h",
        kind: GridProductKind::Accumulation,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: ITALY_CODECS,
        access: ITALY_DPC_ACCESS,
        status: GridImplementationStatus::Fetchable,
        source_hint: "DPC REST type SRT1; one-hour radar/rain-gauge accumulation",
    },
    GridProduct {
        slug: "italy-dpc-cum3",
        label: "Italy DPC Accum 3h",
        kind: GridProductKind::Accumulation,
        cadence_minutes: Some(30),
        resolution_km: None,
        forecast_hours: None,
        codecs: ITALY_CODECS,
        access: ITALY_DPC_ACCESS,
        status: GridImplementationStatus::Fetchable,
        source_hint: "DPC REST type CUM3; 3-hour station-derived accumulation GeoTIFF",
    },
    GridProduct {
        slug: "italy-dpc-cum6",
        label: "Italy DPC Accum 6h",
        kind: GridProductKind::Accumulation,
        cadence_minutes: Some(30),
        resolution_km: None,
        forecast_hours: None,
        codecs: ITALY_CODECS,
        access: ITALY_DPC_ACCESS,
        status: GridImplementationStatus::Fetchable,
        source_hint: "DPC REST type CUM6; 6-hour station-derived accumulation GeoTIFF",
    },
    GridProduct {
        slug: "italy-dpc-cum12",
        label: "Italy DPC Accum 12h",
        kind: GridProductKind::Accumulation,
        cadence_minutes: Some(30),
        resolution_km: None,
        forecast_hours: None,
        codecs: ITALY_CODECS,
        access: ITALY_DPC_ACCESS,
        status: GridImplementationStatus::Fetchable,
        source_hint: "DPC REST type CUM12; 12-hour station-derived accumulation GeoTIFF",
    },
    GridProduct {
        slug: "italy-dpc-cum24",
        label: "Italy DPC Accum 24h",
        kind: GridProductKind::Accumulation,
        cadence_minutes: Some(30),
        resolution_km: None,
        forecast_hours: None,
        codecs: ITALY_CODECS,
        access: ITALY_DPC_ACCESS,
        status: GridImplementationStatus::Fetchable,
        source_hint: "DPC REST type CUM24; 24-hour station-derived accumulation GeoTIFF",
    },
    GridProduct {
        slug: "italy-dpc-cappi-1km",
        label: "Italy DPC CAPPI 1 km",
        kind: GridProductKind::ConstantAltitudePpi,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: ITALY_CODECS,
        access: ITALY_DPC_ACCESS,
        status: GridImplementationStatus::Fetchable,
        source_hint: "DPC REST type CAPPI_1; gridded constant-altitude reflectivity, not a native radar volume",
    },
    GridProduct {
        slug: "italy-dpc-cappi-2km",
        label: "Italy DPC CAPPI 2 km",
        kind: GridProductKind::ConstantAltitudePpi,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: ITALY_CODECS,
        access: ITALY_DPC_ACCESS,
        status: GridImplementationStatus::Fetchable,
        source_hint: "DPC REST type CAPPI_2; gridded constant-altitude reflectivity, not a native radar volume",
    },
    GridProduct {
        slug: "italy-dpc-cappi-3km",
        label: "Italy DPC CAPPI 3 km",
        kind: GridProductKind::ConstantAltitudePpi,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: ITALY_CODECS,
        access: ITALY_DPC_ACCESS,
        status: GridImplementationStatus::Fetchable,
        source_hint: "DPC REST type CAPPI_3; gridded constant-altitude reflectivity, not a native radar volume",
    },
    GridProduct {
        slug: "italy-dpc-cappi-4km",
        label: "Italy DPC CAPPI 4 km",
        kind: GridProductKind::ConstantAltitudePpi,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: ITALY_CODECS,
        access: ITALY_DPC_ACCESS,
        status: GridImplementationStatus::Fetchable,
        source_hint: "DPC REST type CAPPI_4; gridded constant-altitude reflectivity, not a native radar volume",
    },
    GridProduct {
        slug: "italy-dpc-cappi-5km",
        label: "Italy DPC CAPPI 5 km",
        kind: GridProductKind::ConstantAltitudePpi,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: ITALY_CODECS,
        access: ITALY_DPC_ACCESS,
        status: GridImplementationStatus::Fetchable,
        source_hint: "DPC REST type CAPPI_5; gridded constant-altitude reflectivity, not a native radar volume",
    },
    GridProduct {
        slug: "italy-dpc-cappi-6km",
        label: "Italy DPC CAPPI 6 km",
        kind: GridProductKind::ConstantAltitudePpi,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: ITALY_CODECS,
        access: ITALY_DPC_ACCESS,
        status: GridImplementationStatus::Fetchable,
        source_hint: "DPC REST type CAPPI_6; gridded constant-altitude reflectivity, not a native radar volume",
    },
    GridProduct {
        slug: "italy-dpc-cappi-7km",
        label: "Italy DPC CAPPI 7 km",
        kind: GridProductKind::ConstantAltitudePpi,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: ITALY_CODECS,
        access: ITALY_DPC_ACCESS,
        status: GridImplementationStatus::Fetchable,
        source_hint: "DPC REST type CAPPI_7; gridded constant-altitude reflectivity, not a native radar volume",
    },
    GridProduct {
        slug: "italy-dpc-cappi-8km",
        label: "Italy DPC CAPPI 8 km",
        kind: GridProductKind::ConstantAltitudePpi,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: ITALY_CODECS,
        access: ITALY_DPC_ACCESS,
        status: GridImplementationStatus::Fetchable,
        source_hint: "DPC REST type CAPPI_8; gridded constant-altitude reflectivity, not a native radar volume",
    },
    GridProduct {
        slug: "italy-dpc-cappi-9km",
        label: "Italy DPC CAPPI 9 km",
        kind: GridProductKind::ConstantAltitudePpi,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: ITALY_CODECS,
        access: ITALY_DPC_ACCESS,
        status: GridImplementationStatus::Fetchable,
        source_hint: "DPC REST type CAPPI_9; gridded constant-altitude reflectivity, not a native radar volume",
    },
    GridProduct {
        slug: "italy-dpc-cappi-10km",
        label: "Italy DPC CAPPI 10 km",
        kind: GridProductKind::ConstantAltitudePpi,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: ITALY_CODECS,
        access: ITALY_DPC_ACCESS,
        status: GridImplementationStatus::Fetchable,
        source_hint: "DPC REST type CAPPI_10; gridded constant-altitude reflectivity, not a native radar volume",
    },
    GridProduct {
        slug: "italy-dpc-vil",
        label: "Italy DPC VIL",
        kind: GridProductKind::VerticallyIntegratedLiquid,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: ITALY_CODECS,
        access: ITALY_DPC_ACCESS,
        status: GridImplementationStatus::Fetchable,
        source_hint: "DPC REST type VIL; gridded vertically integrated liquid product",
    },
    GridProduct {
        slug: "italy-dpc-etm",
        label: "Italy DPC Echo Top Map",
        kind: GridProductKind::EchoTops,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: ITALY_CODECS,
        access: ITALY_DPC_ACCESS,
        status: GridImplementationStatus::Fetchable,
        source_hint: "DPC REST type ETM; gridded echo-top map product",
    },
    GridProduct {
        slug: "italy-dpc-poh",
        label: "Italy DPC Probability of Hail",
        kind: GridProductKind::HailProbability,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: ITALY_CODECS,
        access: ITALY_DPC_ACCESS,
        status: GridImplementationStatus::Fetchable,
        source_hint: "DPC REST type POH; gridded hail-probability product",
    },
    GridProduct {
        slug: "italy-dpc-heavy-rain",
        label: "Italy DPC Heavy Rain Detection",
        kind: GridProductKind::HeavyRainDetection,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: ITALY_CODECS,
        access: ITALY_DPC_WMTS,
        status: GridImplementationStatus::Catalogued,
        source_hint: "DPC WMTS layer radar:hrd; live REST download endpoint returned no raw HRD file in 2026-06 probe",
    },
    GridProduct {
        slug: "italy-dpc-lightning",
        label: "Italy DPC Lightning",
        kind: GridProductKind::Lightning,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: ITALY_CODECS,
        access: ITALY_DPC_WMTS,
        status: GridImplementationStatus::Catalogued,
        source_hint: "DPC platform lightning overlay; not exposed by the v2 raw-file endpoint in current docs/probe",
    },
    GridProduct {
        slug: "italy-dpc-sites",
        label: "Italy DPC Radar Sites/Status",
        kind: GridProductKind::RadarStatus,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: ITALY_CODECS,
        access: ITALY_DPC_WMTS,
        status: GridImplementationStatus::Catalogued,
        source_hint: "DPC WMTS layer radar:radardpc and REST type SITES metadata; not a polar site-volume provider",
    },
    GridProduct {
        slug: "italy-dpc-ir108",
        label: "Italy DPC MSG IR 10.8",
        kind: GridProductKind::CloudCover,
        cadence_minutes: Some(5),
        resolution_km: None,
        forecast_hours: None,
        codecs: ITALY_CODECS,
        access: ITALY_DPC_ACCESS,
        status: GridImplementationStatus::Fetchable,
        source_hint: "DPC REST type IR_108; companion satellite cloud-cover GeoTIFF",
    },
    GridProduct {
        slug: "italy-dpc-temp",
        label: "Italy DPC Temperature",
        kind: GridProductKind::Temperature,
        cadence_minutes: Some(60),
        resolution_km: None,
        forecast_hours: None,
        codecs: ITALY_CODECS,
        access: ITALY_DPC_ACCESS,
        status: GridImplementationStatus::Fetchable,
        source_hint: "DPC REST type TEMP; station-derived temperature GeoTIFF",
    },
];

const TAIWAN_CWA_PRODUCTS: &[GridProduct] = &[GridProduct {
    slug: "taiwan-cwa-composite-reflectivity",
    label: "Taiwan CWA Composite Reflectivity",
    kind: GridProductKind::ReflectivityComposite,
    cadence_minutes: Some(10),
    resolution_km: None,
    forecast_hours: None,
    codecs: TAIWAN_CWA_CODECS,
    access: REST_API,
    status: GridImplementationStatus::Fetchable,
    source_hint: "CWA O-A0059-001 lon/lat numeric dBZ composite grid; not native polar site data",
}];

const AEMET_PRODUCTS: &[GridProduct] = &[GridProduct {
    slug: "aemet-lowest-elevation-reflectivity",
    label: "Spain AEMET Lowest-Elevation Reflectivity",
    kind: GridProductKind::ReflectivityComposite,
    cadence_minutes: None,
    resolution_km: None,
    forecast_hours: None,
    codecs: IMAGE_CODECS,
    access: REST_API,
    status: GridImplementationStatus::DecoderNeeded,
    source_hint: "AEMET OpenData/public radar image path",
}];

const IPMA_PRODUCTS: &[GridProduct] = &[GridProduct {
    slug: "ipma-precipitation-intensity",
    label: "Portugal IPMA Precipitation Intensity",
    kind: GridProductKind::RainRate,
    cadence_minutes: Some(60),
    resolution_km: None,
    forecast_hours: None,
    codecs: IMAGE_CODECS,
    access: OPEN_HTTP,
    status: GridImplementationStatus::DecoderNeeded,
    source_hint: "IPMA radar imagery for mainland, Azores, and Madeira",
}];

const METEOALARM_PRODUCTS: &[GridProduct] = &[GridProduct {
    slug: "meteoalarm-warnings",
    label: "MeteoAlarm Warnings",
    kind: GridProductKind::Warning,
    cadence_minutes: None,
    resolution_km: None,
    forecast_hours: None,
    codecs: METEOALARM_CODECS,
    access: API_AND_MQTT,
    status: GridImplementationStatus::DecoderNeeded,
    source_hint: "Pan-European warning polygons and metadata",
}];

const GRID_PROVIDERS: &[StaticGridProductProvider] = &[
    StaticGridProductProvider {
        id: "opera",
        label: "OPERA/CIRRUS Europe",
        region: "Europe",
        docs_url: "https://eumetnet.github.io/openradardata-documentation/",
        products: OPERA_PRODUCTS,
    },
    StaticGridProductProvider {
        id: "mrms",
        label: "NOAA MRMS",
        region: "United States",
        docs_url: "https://www.nssl.noaa.gov/projects/mrms/",
        products: MRMS_PRODUCTS,
    },
    StaticGridProductProvider {
        id: "metoffice-uk",
        label: "Met Office UK Radar",
        region: "United Kingdom",
        docs_url: "https://registry.opendata.aws/met-office-uk-radar-observations/",
        products: UK_PRODUCTS,
    },
    StaticGridProductProvider {
        id: "knmi",
        label: "KNMI Netherlands",
        region: "Netherlands",
        docs_url: "https://dataplatform.knmi.nl/",
        products: KNMI_PRODUCTS,
    },
    StaticGridProductProvider {
        id: "meteoswiss",
        label: "MeteoSwiss",
        region: "Switzerland",
        docs_url: "https://opendatadocs.meteoswiss.ch/",
        products: METEOSWISS_PRODUCTS,
    },
    StaticGridProductProvider {
        id: "dwd-grid",
        label: "DWD Gridded Products",
        region: "Germany",
        docs_url: "https://opendata.dwd.de/weather/radar/",
        products: DWD_GRID_PRODUCTS,
    },
    StaticGridProductProvider {
        id: "imgw-polrad",
        label: "IMGW-PIB POLRAD",
        region: "Poland",
        docs_url: imgw::IMGW_DATASTORE_URL,
        products: IMGW_POLRAD_PRODUCTS,
    },
    StaticGridProductProvider {
        id: "italy-dpc",
        label: "Italy DPC / ItaliaMeteo",
        region: "Italy",
        docs_url: "https://dpc-radar.readthedocs.io/it/latest/api.html",
        products: ITALY_PRODUCTS,
    },
    StaticGridProductProvider {
        id: "taiwan-cwa",
        label: "Taiwan CWA",
        region: "Taiwan",
        docs_url: "https://opendata.cwa.gov.tw/",
        products: TAIWAN_CWA_PRODUCTS,
    },
    StaticGridProductProvider {
        id: "aemet",
        label: "AEMET Spain",
        region: "Spain",
        docs_url: "https://www.aemet.es/",
        products: AEMET_PRODUCTS,
    },
    StaticGridProductProvider {
        id: "ipma",
        label: "IPMA Portugal",
        region: "Portugal",
        docs_url: "https://www.ipma.pt/",
        products: IPMA_PRODUCTS,
    },
    StaticGridProductProvider {
        id: "meteoalarm",
        label: "MeteoAlarm",
        region: "Europe",
        docs_url: "https://api.meteoalarm.org/",
        products: METEOALARM_PRODUCTS,
    },
];

/// Built-in gridded/composite product providers, separate from polar sites.
pub fn grid_product_providers() -> &'static [StaticGridProductProvider] {
    GRID_PROVIDERS
}

/// Flatten every catalogued grid product in provider order.
pub fn grid_products()
-> impl Iterator<Item = (&'static StaticGridProductProvider, &'static GridProduct)> {
    GRID_PROVIDERS.iter().flat_map(|provider| {
        provider
            .products
            .iter()
            .map(move |product| (provider, product))
    })
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;

    #[test]
    fn grid_provider_ids_are_unique_and_nonempty() {
        let providers = grid_product_providers();
        let ids: BTreeSet<_> = providers.iter().map(|provider| provider.id).collect();
        assert_eq!(ids.len(), providers.len());
        for provider in providers {
            assert!(!provider.id.is_empty());
            assert!(!provider.label.is_empty());
            assert!(!provider.products.is_empty(), "{}", provider.id);
        }
    }

    #[test]
    fn roadmap_sources_are_catalogued_separately_from_polar_sites() {
        let slugs: BTreeSet<_> = grid_products().map(|(_, product)| product.slug).collect();
        for required in [
            "opera-cirrus-max-reflectivity",
            "opera-rain-rate",
            "opera-accum-1h",
            "ord-api-mqtt-discovery",
            "mrms-composite-reflectivity",
            "mrms-merged-reflectivity-lowest-altitude",
            "mrms-precip-rate",
            "mrms-rotation-tracks",
            "uk-metoffice-rain-rate",
            "knmi-reflectivity-nowcast",
            "knmi-hail-probability",
            "knmi-echo-tops",
            "meteoswiss-precip",
            "meteoswiss-combiprecip",
            "dwd-radolan-qpe",
            "imgw-polrad-cmax-kdp",
            "imgw-polrad-cmax-rhohv",
            "imgw-polrad-cmax-zdr",
            "imgw-polrad-cmax-phidp",
            "italy-dpc-vmi",
            "italy-dpc-sri",
            "italy-dpc-srt",
            "italy-dpc-cappi-1km",
            "italy-dpc-vil",
            "italy-dpc-etm",
            "italy-dpc-poh",
            "italy-dpc-sites",
            "taiwan-cwa-composite-reflectivity",
            "aemet-lowest-elevation-reflectivity",
            "ipma-precipitation-intensity",
            "meteoalarm-warnings",
        ] {
            assert!(slugs.contains(required), "{required}");
        }
    }

    #[test]
    fn imgw_polrad_catalog_is_fetchable_dual_pol_cmax_not_polar_volume() {
        let imgw = grid_product_providers()
            .iter()
            .find(|provider| provider.id == "imgw-polrad")
            .expect("IMGW POLRAD provider");
        assert_eq!(imgw.region, "Poland");
        assert_eq!(imgw.products.len(), 4);
        for product in imgw.products {
            assert_eq!(product.kind, GridProductKind::DualPolarizationMaximum);
            assert_eq!(product.status, GridImplementationStatus::Fetchable);
            assert!(product.codecs.contains(&GridCodec::OdimH5Grid));
            assert!(product.access.contains(&GridAccess::PortalDownload));
            assert_eq!(product.cadence_minutes, Some(5));
        }
        assert!(
            imgw.products
                .iter()
                .any(|product| product.source_hint.contains("not a polar volume"))
        );
    }

    #[test]
    fn opera_products_are_grid_products_not_radar_sites() {
        let opera = grid_product_providers()
            .iter()
            .find(|provider| provider.id == "opera")
            .expect("opera provider");
        assert!(
            opera
                .products
                .iter()
                .all(|product| product.codecs.contains(&GridCodec::OdimH5Grid)
                    || product.kind == GridProductKind::Discovery)
        );
        assert!(
            opera
                .products
                .iter()
                .any(|product| product.access.contains(&GridAccess::Mqtt))
        );
    }

    #[test]
    fn mrms_products_are_catalogued_as_decoder_needed_grids() {
        let mrms = grid_product_providers()
            .iter()
            .find(|provider| provider.id == "mrms")
            .expect("MRMS provider");
        assert!(mrms.products.len() >= 4);
        assert!(
            mrms.products
                .iter()
                .any(|product| product.kind == GridProductKind::ReflectivityComposite)
        );
        assert!(
            mrms.products
                .iter()
                .any(|product| product.kind == GridProductKind::RotationTracks)
        );
        for product in mrms.products {
            assert_eq!(product.status, GridImplementationStatus::DecoderNeeded);
            assert!(product.codecs.contains(&GridCodec::Grib2));
            assert!(product.access.contains(&GridAccess::OpenHttp));
        }
    }

    #[test]
    fn italy_dpc_catalog_marks_fetchable_products_without_creating_sites() {
        let italy = grid_product_providers()
            .iter()
            .find(|provider| provider.id == "italy-dpc")
            .expect("Italy DPC provider");
        let slugs: BTreeSet<_> = italy.products.iter().map(|product| product.slug).collect();
        for spec in italy_dpc_fetchable_products() {
            let product = italy
                .products
                .iter()
                .find(|product| product.slug == spec.slug)
                .unwrap_or_else(|| panic!("{} missing from Italy catalog", spec.slug));
            assert_eq!(product.status, GridImplementationStatus::Fetchable);
            assert!(product.access.contains(&GridAccess::RestApi));
            assert!(product.codecs.contains(&GridCodec::GeoTiff));
        }
        assert!(slugs.contains("italy-dpc-sites"));
        let sites = italy
            .products
            .iter()
            .find(|product| product.slug == "italy-dpc-sites")
            .expect("DPC sites/status row");
        assert_eq!(sites.status, GridImplementationStatus::Catalogued);
        assert!(
            sites
                .source_hint
                .contains("not a polar site-volume provider"),
            "{}",
            sites.source_hint
        );
    }

    #[test]
    fn taiwan_cwa_catalog_marks_reflectivity_composite_fetchable() {
        let taiwan = grid_product_providers()
            .iter()
            .find(|provider| provider.id == "taiwan-cwa")
            .expect("Taiwan CWA provider");
        let product = taiwan
            .products
            .iter()
            .find(|product| product.slug == "taiwan-cwa-composite-reflectivity")
            .expect("Taiwan CWA reflectivity product");
        assert_eq!(product.kind, GridProductKind::ReflectivityComposite);
        assert_eq!(product.status, GridImplementationStatus::Fetchable);
        assert!(product.codecs.contains(&GridCodec::ApiJson));
        assert!(product.access.contains(&GridAccess::RestApi));
        assert!(product.source_hint.contains("not native polar site data"));
    }

    #[test]
    fn taiwan_cwa_latest_json_parser_maps_rows_and_nodata() {
        let json = r#"{
          "cwaopendata": {
            "dataid": "O-A0059-001",
            "dataset": {
              "datasetInfo": {
                "parameterSet": {
                  "StartPointLongitude": "115.0",
                  "StartPointLatitude": "18.0",
                  "GridResolution": "0.0125",
                  "DateTime": "2026-06-27T10:50:00+08:00",
                  "GridDimensionX": "3",
                  "GridDimensionY": "2",
                  "Reflectivity": "dBZ"
                }
              },
              "contents": {
                "content": "1,2,-99,4,-999,6"
              }
            }
          }
        }"#;
        let grid = parse_taiwan_cwa_latest_json(json).expect("parse Taiwan CWA JSON");
        assert_eq!(grid.nx, 3);
        assert_eq!(grid.ny, 2);
        assert_eq!(grid.value_at_source_xy(0, 0), Some(1.0));
        assert_eq!(grid.value_at_source_xy(2, 1), Some(6.0));
        assert!(taiwan_cwa_is_nodata(grid.value_at_source_xy(2, 0).unwrap()));
        assert!(taiwan_cwa_is_nodata(grid.value_at_source_xy(1, 1).unwrap()));
        assert_eq!(
            grid.time.to_rfc3339_opts(SecondsFormat::Secs, true),
            "2026-06-27T02:50:00Z"
        );
    }

    #[test]
    #[ignore = "live CWA endpoint smoke"]
    fn taiwan_cwa_latest_live_smoke() {
        let grid = taiwan_cwa_latest_radar_grid().expect("live Taiwan CWA latest grid");
        assert_eq!(grid.units, "dBZ");
        assert!(grid.nx > 100);
        assert!(grid.ny > 100);
        assert_eq!(grid.values.len(), grid.nx * grid.ny);
    }

    #[test]
    fn taiwan_cwa_history_metadata_parser_extracts_product_urls() {
        let xml = r#"
            <cwaopendata>
              <dataset>
                <resource>
                  <ProductURL>https://opendata.cwa.gov.tw/historyapi/v1/getData/O-A0059-001/2026/06/27/10/50/00?Authorization=x</ProductURL>
                </resource>
                <resource><ProductURL> https://example.test/second </ProductURL></resource>
              </dataset>
            </cwaopendata>
        "#;
        let urls = parse_taiwan_cwa_history_product_urls(xml).expect("parse history URLs");
        assert_eq!(urls.len(), 2);
        assert!(urls[0].contains("/O-A0059-001/2026/06/27/10/50/00"));
        assert_eq!(urls[1], "https://example.test/second");
    }

    #[test]
    fn italy_dpc_srt_and_cum_product_types_are_distinct() {
        let fetchable: BTreeMap<_, _> = italy_dpc_fetchable_products()
            .iter()
            .map(|spec| (spec.product_type, spec.slug))
            .collect();

        assert_eq!(fetchable.get("SRT1"), Some(&"italy-dpc-srt"));
        assert_eq!(fetchable.get("CUM12"), Some(&"italy-dpc-cum12"));
        assert_eq!(fetchable.get("CUM24"), Some(&"italy-dpc-cum24"));
        assert_ne!(fetchable.get("CUM12"), fetchable.get("CUM24"));
    }

    #[test]
    fn italy_dpc_latest_response_parses_epoch_ms_and_period() {
        let latest = parse_italy_dpc_latest_response(
            r#"{
                "total": 1,
                "lastProducts": [{
                    "productType": "VMI",
                    "time": 1782498000000,
                    "period": "PT5M"
                }]
            }"#,
            "VMI",
        )
        .expect("latest response parses");

        assert_eq!(latest.product_type, "VMI");
        assert_eq!(latest.period, "PT5M");
        assert_eq!(
            latest.time_utc().expect("valid timestamp").to_rfc3339(),
            "2026-06-26T18:20:00+00:00"
        );
    }

    #[test]
    fn italy_dpc_download_identity_uses_stable_bucket_key() {
        let parsed = parse_italy_dpc_download_response(
            r#"{
                "bucket": "s3-prod-dpc-radar",
                "key": "VMI/26-06-2026-18-20.tif",
                "url": "https://s3-prod-dpc-radar.s3.eu-south-1.amazonaws.com/VMI/26-06-2026-18-20.tif?X-Amz-Signature=abc",
                "expiresSeconds": 300
            }"#,
            "VMI",
            1782498000000,
        )
        .expect("download response parses");
        let plan = ItalyDpcDownloadPlan {
            product_type: "VMI".to_owned(),
            product_time_millis: 1782498000000,
            period: Some("PT5M".to_owned()),
            identity: format!("italy-dpc/{}/{}", parsed.bucket, parsed.key),
            bucket: parsed.bucket,
            key: parsed.key,
            url: parsed.url,
            expires_seconds: parsed.expires_seconds,
        };

        assert_eq!(
            plan.identity,
            "italy-dpc/s3-prod-dpc-radar/VMI/26-06-2026-18-20.tif"
        );
        assert!(
            !plan.identity.contains("X-Amz"),
            "identity must ignore expiring signature"
        );
        assert_eq!(plan.expires_seconds, Some(300));
    }

    #[test]
    fn italy_dpc_product_type_validation_is_canonical_and_closed() {
        assert_eq!(canonical_italy_dpc_product_type("vmi").unwrap(), "VMI");
        assert_eq!(
            canonical_italy_dpc_product_type(" CAPPI_10 ").unwrap(),
            "CAPPI_10"
        );
        assert!(canonical_italy_dpc_product_type("SITES").is_err());
        assert!(canonical_italy_dpc_product_type("../VMI").is_err());
    }

    #[test]
    fn italy_dpc_wmts_tile_url_uses_official_web_mercator_parameters() {
        let url = italy_dpc_wmts_tile_url("vmi", 5, 16, 11, None).expect("VMI WMTS URL");

        assert!(url.starts_with("https://radar-geowebcache.protezionecivile.it/service/wmts?"));
        assert!(url.contains("SERVICE=WMTS"));
        assert!(url.contains("REQUEST=GetTile"));
        assert!(url.contains("LAYER=radar:vmi"));
        assert!(url.contains("STYLE=vmi"));
        assert!(url.contains("TILEMATRIXSET=EPSG:900913"));
        assert!(url.contains("TILEMATRIX=EPSG:900913:5"));
        assert!(url.contains("TILEROW=11"));
        assert!(url.contains("TILECOL=16"));
        assert!(url.contains("FORMAT=image/png"));
        assert!(
            !url.contains("TIME="),
            "omitting time requests DPC current value"
        );
    }

    #[test]
    fn italy_dpc_wmts_tile_url_accepts_product_aliases_and_styles() {
        let url = italy_dpc_wmts_tile_url("CUM3", 8, 131, 89, None).expect("CUM3 WMTS URL");
        assert!(url.contains("LAYER=radar:srt3"), "{url}");
        assert!(
            url.contains("STYLE=srt"),
            "DPC SRT accumulation layers share the srt style: {url}"
        );

        let sites = italy_dpc_wmts_tile_url("italy-dpc-sites", 8, 131, 89, None)
            .expect("radar site/status WMTS URL");
        assert!(sites.contains("LAYER=radar:radardpc"), "{sites}");
        assert!(sites.contains("STYLE=radardpc"), "{sites}");
    }

    #[test]
    fn italy_dpc_wmts_tile_url_formats_optional_time_as_iso8601_millis() {
        let time = Utc
            .timestamp_millis_opt(1782498000000)
            .single()
            .expect("valid timestamp");
        let url =
            italy_dpc_wmts_tile_url("radar:vmi", 5, 16, 11, Some(time)).expect("timed WMTS URL");

        assert!(url.contains("TIME=2026-06-26T18:20:00.000Z"), "{url}");
        assert_eq!(format_italy_dpc_wmts_time(time), "2026-06-26T18:20:00.000Z");
        assert!(italy_dpc_wmts_tile_url("regioni", 5, 16, 11, None).is_err());
    }

    /// Live DPC smoke test. It queries the latest VMI timestamp and asks the
    /// REST API for a presigned GeoTIFF URL without downloading the file.
    ///
    /// Run manually with:
    /// `cargo test -p data_source italy_dpc_live_latest_download_plan -- --ignored --nocapture`
    #[test]
    #[ignore = "live Italy DPC endpoint probe"]
    fn italy_dpc_live_latest_download_plan() {
        let plan = italy_dpc_latest_download_plan("VMI").expect("DPC VMI plan");
        println!("{plan:?}");
        assert_eq!(plan.product_type, "VMI");
        assert!(plan.key.ends_with(".tif"));
        assert!(plan.identity.contains(&plan.key));
        assert!(!plan.identity.contains("X-Amz"));
    }
}
