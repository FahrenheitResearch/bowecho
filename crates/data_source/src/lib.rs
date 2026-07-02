//! Public radar data-source helpers.

pub mod community_feeds;
mod embedded_sites;
pub mod grid_products;
pub mod international;

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration as StdDuration, Instant};

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, Utc};
use reqwest::header::{ACCEPT, COOKIE, REFERER, SET_COOKIE};
use serde::Deserialize;
use thiserror::Error;

pub const LEVEL2_ARCHIVE_BUCKET: &str = "unidata-nexrad-level2";
pub const LEVEL2_CHUNKS_BUCKET: &str = "unidata-nexrad-level2-chunks";
const HTTP_CONNECT_TIMEOUT: StdDuration = StdDuration::from_secs(4);
const HTTP_METADATA_TIMEOUT: StdDuration = StdDuration::from_secs(25);
/// Whole-request budget on the download client. Field report: SMHI qcvol
/// volumes run 17-18 MB and a user on a slow link hit the old 45 s budget
/// MID-BODY every tick (surfacing as reqwest's cryptic "error decoding
/// response body"). 180 s admits ~120 KB/s links; pathological hangs
/// occupy only a background poll thread.
const HTTP_DOWNLOAD_TIMEOUT: StdDuration = StdDuration::from_secs(180);
const HTTP_VOLUME_RETRY_BACKOFF: StdDuration = StdDuration::from_secs(2);
const HTTP_USER_AGENT: &str = "bowecho (GR2Analyst-compatible placefile client)";
const REALTIME_VOLUME_ID_MODULUS: u16 = 1000;
const REALTIME_CHUNK_LIST_MAX_KEYS: usize = 1000;
const REALTIME_CHUNK_DOWNLOAD_CONCURRENCY: usize = 8;
const REALTIME_CHUNK_STALE_RESCAN_AGE_SECONDS: i64 = 20 * 60;
const REALTIME_CHUNK_STALE_RESCAN_MAX_PAGES: usize = 80;
const MIN_RECENT_LEVEL2_SITE_CATALOG_COUNT: usize = 100;
/// How long the per-site active-volume-id prefix listing may be served from
/// cache. Volume ids only change at volume rollover (every ~4-7 minutes), and
/// a complete "latest" volume forces an immediate fresh listing, so a short
/// TTL removes almost all 1 Hz prefix-list traffic without delaying rollover.
const REALTIME_ACTIVE_IDS_LISTING_TTL: StdDuration = StdDuration::from_secs(10);
const COMPLETED_VOLUME_CACHE_PER_SITE: usize = 8;
/// How long a live (incomplete) volume's chunk listing may be served from
/// cache. Dedupes the per-volume chunk LIST when several pollers watch the
/// same site (primary pane at 1 s, live multi-panes and overlays at 5 s)
/// while staying under the primary's 1 s cadence so it always re-lists.
const REALTIME_LIVE_VOLUME_LISTING_TTL: StdDuration = StdDuration::from_millis(900);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RadarDataLevel {
    Level2Archive,
    Level2RealtimeChunks,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataSourceKind {
    LocalFile,
    LocalDirectory,
    PublicLevel2Archive,
    PublicLevel2RealtimeChunks,
    NceiArchive,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourcePriority {
    pub sources: Vec<DataSourceKind>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RadarSite {
    pub level2_id: String,
    pub name: Option<String>,
    pub latitude_deg: Option<f32>,
    pub longitude_deg: Option<f32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct S3Object {
    pub key: String,
    pub size: u64,
    pub last_modified: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DownloadedObject {
    pub object: S3Object,
    pub path: PathBuf,
    pub url: String,
    pub cache_hit: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LatestObject {
    pub object: S3Object,
    pub cache_hit: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RealtimeChunkType {
    Start,
    Intermediate,
    End,
}

impl RealtimeChunkType {
    fn from_code(value: &str) -> Option<Self> {
        match value {
            "S" => Some(Self::Start),
            "I" => Some(Self::Intermediate),
            "E" => Some(Self::End),
            _ => None,
        }
    }

    fn is_end(self) -> bool {
        matches!(self, Self::End)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Intermediate => "intermediate",
            Self::End => "end",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RealtimeChunkObject {
    pub object: S3Object,
    pub site: String,
    pub volume_id: u16,
    pub volume_time: DateTime<Utc>,
    pub chunk_id: u16,
    pub chunk_type: RealtimeChunkType,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RealtimeLevel2Volume {
    pub site: String,
    pub volume_id: u16,
    pub volume_time: DateTime<Utc>,
    pub chunks: Vec<RealtimeChunkObject>,
    pub complete: bool,
    pub total_size: u64,
}

#[derive(Debug, Error)]
pub enum DataSourceError {
    // Full cause chain: reqwest's top-level Display hides the source, so
    // field statuses read "error decoding response body" when the actual
    // cause was a mid-body timeout, or "error sending request" when DNS
    // or a reset connection was at fault.
    #[error("HTTP request failed: {}", reqwest_error_chain(.0))]
    Http(#[from] reqwest::Error),
    #[error("S3 XML parse failed: {0}")]
    Xml(#[from] quick_xml::DeError),
    #[error("JSON parse failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("no objects found for {bucket}/{prefix}")]
    NoObjects { bucket: String, prefix: String },
    #[error("downloaded {url} size mismatch: expected {expected} bytes, got {actual}")]
    DownloadSizeMismatch {
        url: String,
        expected: u64,
        actual: u64,
    },
    #[error("realtime chunk download worker panicked")]
    DownloadWorkerPanic,
}

pub type Result<T> = std::result::Result<T, DataSourceError>;

impl DataSourceError {
    /// True when the server answered "this resource does not exist"
    /// (HTTP 404, or an empty S3 listing). Callers distinguishing "no
    /// file published for that date" from a real transport failure
    /// (SPC storm-report archives: a 404 means a quiet/pre-archive day,
    /// not an error worth surfacing or retrying).
    pub fn is_not_found(&self) -> bool {
        match self {
            DataSourceError::Http(err) => err.status() == Some(reqwest::StatusCode::NOT_FOUND),
            DataSourceError::NoObjects { .. } => true,
            _ => false,
        }
    }
}

impl Default for SourcePriority {
    fn default() -> Self {
        Self {
            sources: vec![
                DataSourceKind::LocalFile,
                DataSourceKind::PublicLevel2Archive,
            ],
        }
    }
}

impl RadarSite {
    pub fn new(level2_id: impl Into<String>) -> Self {
        let level2_id = level2_id.into().to_ascii_uppercase();
        Self {
            level2_id,
            name: None,
            latitude_deg: None,
            longitude_deg: None,
        }
    }

    pub fn with_location(
        mut self,
        name: Option<String>,
        latitude_deg: Option<f32>,
        longitude_deg: Option<f32>,
    ) -> Self {
        self.name = name;
        self.latitude_deg = latitude_deg;
        self.longitude_deg = longitude_deg;
        self
    }
}

pub fn fallback_sites() -> Vec<RadarSite> {
    // The embedded table carries COORDINATES, so offline the map markers,
    // site picker, and right-click beam lookup all still work (field
    // report: bad internet -> "no radar near").
    embedded_site_table()
}

/// The compiled-in station list (see embedded_sites.rs) as RadarSites.
fn embedded_site_table() -> Vec<RadarSite> {
    embedded_sites::EMBEDDED_SITES
        .iter()
        .map(|(id, name, lat, lon)| {
            RadarSite::new(*id).with_location(Some((*name).to_owned()), Some(*lat), Some(*lon))
        })
        .collect()
}

pub fn list_level2_sites_for_date(date: NaiveDate) -> Result<Vec<RadarSite>> {
    let prefix = format!("{:04}/{:02}/{:02}/", date.year(), date.month(), date.day());
    let listing = list_s3(LEVEL2_ARCHIVE_BUCKET, &prefix, Some("/"), None)?;
    let mut sites = listing
        .common_prefixes
        .into_iter()
        .filter_map(|prefix| {
            prefix
                .prefix
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .map(str::to_owned)
        })
        .filter(|site| !site.is_empty())
        .map(RadarSite::new)
        .collect::<Vec<_>>();
    sites.sort_by(|left, right| left.level2_id.cmp(&right.level2_id));
    sites.dedup_by(|left, right| left.level2_id == right.level2_id);
    Ok(sites)
}

pub fn list_recent_level2_sites(days_back: i64) -> Result<Vec<RadarSite>> {
    let today = Utc::now().date_naive();
    let mut sites_by_id = BTreeMap::<String, RadarSite>::new();
    for offset in 0..=days_back.max(0) {
        let date = today - Duration::days(offset);
        if let Ok(sites) = list_level2_sites_for_date(date) {
            for site in sites {
                sites_by_id.entry(site.level2_id.clone()).or_insert(site);
            }
            if sites_by_id.len() >= MIN_RECENT_LEVEL2_SITE_CATALOG_COUNT {
                break;
            }
        }
    }

    for site in fallback_sites() {
        sites_by_id.entry(site.level2_id.clone()).or_insert(site);
    }

    let mut sites = sites_by_id.into_values().collect::<Vec<_>>();
    sites.sort_by(|left, right| left.level2_id.cmp(&right.level2_id));
    Ok(sites)
}

pub fn fetch_weather_gov_radar_sites() -> Result<Vec<RadarSite>> {
    let client = metadata_http_client();
    let text = client
        .get("https://api.weather.gov/radar/stations")
        .send()?
        .error_for_status()?
        .text()?;
    let collection: WeatherGovFeatureCollection = serde_json::from_str(&text)?;
    let mut sites = collection
        .features
        .into_iter()
        .filter_map(|feature| {
            let id = feature.properties.id?;
            let coordinates = feature.geometry?.coordinates;
            if coordinates.len() < 2 {
                return None;
            }
            Some(RadarSite::new(id).with_location(
                feature.properties.name,
                Some(coordinates[1] as f32),
                Some(coordinates[0] as f32),
            ))
        })
        .collect::<Vec<_>>();
    sites.sort_by(|left, right| left.level2_id.cmp(&right.level2_id));
    sites.dedup_by(|left, right| left.level2_id == right.level2_id);
    Ok(sites)
}

pub fn fetch_text(url: &str) -> Result<String> {
    Ok(send_with_retry(&metadata_http_client(), url)?
        .error_for_status()?
        .text()?)
}

/// Fetch the public mPING display GeoJSON.
///
/// The documented full reports API is token-protected. The public display
/// layer uses a no-key GeoJSON endpoint, but the server expects the same-site
/// session cookie created by first visiting `/display/` plus a GeoJSON Accept
/// header. Keep those quirks in one place so callers can treat it as a normal
/// public display feed.
pub fn fetch_mping_reports_geojson() -> Result<String> {
    let client = metadata_http_client();
    let display_response = client
        .get("https://mping.ou.edu/display/")
        .header(ACCEPT, "text/html,*/*")
        .send()?
        .error_for_status()?;
    let cookie_header = display_response
        .headers()
        .get_all(SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(|value| value.split(';').next())
        .filter(|value| !value.trim().is_empty())
        .collect::<Vec<_>>()
        .join("; ");

    let mut request = client
        .get("https://mping.ou.edu/mping/api/v2/reports.geojson")
        .header(ACCEPT, "application/vnd.geo+json,*/*")
        .header(REFERER, "https://mping.ou.edu/display/");
    if !cookie_header.is_empty() {
        request = request.header(COOKIE, cookie_header);
    }
    Ok(request.send()?.error_for_status()?.text()?)
}

/// One GET with a single immediate retry on SEND-stage failures (the
/// stale-pooled-connection class: the server closed an idle keep-alive
/// and the first reuse fails before any response). Status and body
/// errors are NOT retried — they are real answers.
fn send_with_retry(
    client: &reqwest::blocking::Client,
    url: &str,
) -> std::result::Result<reqwest::blocking::Response, reqwest::Error> {
    match client.get(url).send() {
        Ok(response) => Ok(response),
        Err(first) if first.is_status() || first.is_body() || first.is_decode() => Err(first),
        Err(_transient) => client.get(url).send(),
    }
}

/// Fetch a large catalog/listing text resource on the download client.
///
/// Some international feed catalogs are multi-megabyte autoindex pages (a
/// DWD per-station sweep listing runs ~2 MB and the server does not gzip
/// it), which can outrun the 8-second metadata-client budget of
/// [`fetch_text`] on a slow link. Listings still must complete within the
/// 45-second download budget.
pub fn fetch_listing_text(url: &str) -> Result<String> {
    Ok(send_with_retry(&download_http_client(), url)?
        .error_for_status()?
        .text()?)
}

/// `Ok(true)` when a HEAD request says `url` exists (2xx), `Ok(false)` on
/// 404/410, `Err` on transport failures and other HTTP statuses. The cheap
/// existence probe for feeds whose newest file name must be guessed
/// (e.g. the 5-minute-aligned JMA/NICT tar stamps).
pub fn url_exists(url: &str) -> Result<bool> {
    let response = metadata_http_client().head(url).send()?;
    let status = response.status();
    if status.is_success() {
        return Ok(true);
    }
    if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::GONE {
        return Ok(false);
    }
    response.error_for_status()?;
    Ok(false)
}

/// Fetch a small binary resource (e.g. a placefile icon sheet). Capped at
/// 4 MiB — these are sprite sheets, not data files.
pub fn fetch_bytes(url: &str) -> Result<Vec<u8>> {
    let bytes = metadata_http_client()
        .get(url)
        .send()?
        .error_for_status()?
        .bytes()?;
    const MAX: usize = 4 * 1024 * 1024;
    if bytes.len() > MAX {
        return Err(DataSourceError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("resource too large: {} bytes", bytes.len()),
        )));
    }
    Ok(bytes.to_vec())
}

/// Fetch a radar volume from a polled feed. Volumes run 5–25 MB
/// (compressed NEXRAD or uncompressed msg31 conversions; international
/// ODIM PVOLs reach ~18 MB), so this uses the download client (long
/// timeout) with a generous cap — unlike `fetch_bytes`, which is sized
/// for sprite sheets on the metadata client and rejects anything over
/// 4 MiB.
pub fn fetch_volume_bytes(url: &str) -> Result<Vec<u8>> {
    let client = download_http_client();
    let result = send_with_retry(&client, url)
        .and_then(|response| response.error_for_status())
        .and_then(|response| response.bytes());
    let bytes = match result {
        Ok(bytes) => bytes,
        Err(err) if should_retry_volume_fetch(&err) => {
            thread::sleep(HTTP_VOLUME_RETRY_BACKOFF);
            send_with_retry(&client, url)
                .and_then(|response| response.error_for_status())
                .and_then(|response| response.bytes())?
        }
        Err(err) => return Err(err.into()),
    };
    const MAX: usize = 256 * 1024 * 1024;
    if bytes.len() > MAX {
        return Err(DataSourceError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("volume too large: {} bytes", bytes.len()),
        )));
    }
    Ok(bytes.to_vec())
}

fn should_retry_volume_fetch(err: &reqwest::Error) -> bool {
    !err.is_status() && (err.is_timeout() || err.is_body() || err.is_decode())
}

/// reqwest's Display drops the cause ("error decoding response body" with
/// the timeout hidden in source()) — join the whole chain for status text.
fn reqwest_error_chain(err: &reqwest::Error) -> String {
    use std::error::Error as _;
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !text.contains(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}

pub fn fetch_level2_radar_sites(days_back: i64) -> Result<Vec<RadarSite>> {
    // Embedded base FIRST, live API overlay second: site locations must
    // never depend on the network being up.
    let mut weather_by_id = embedded_site_table()
        .into_iter()
        .map(|site| (site.level2_id.clone(), site))
        .collect::<BTreeMap<_, _>>();
    for site in fetch_weather_gov_radar_sites().unwrap_or_default() {
        weather_by_id.insert(site.level2_id.clone(), site);
    }

    let mut sites = list_recent_level2_sites(days_back).unwrap_or_else(|_| fallback_sites());
    for site in &mut sites {
        if let Some(weather_site) = weather_by_id.get(&site.level2_id) {
            site.name = weather_site.name.clone();
            site.latitude_deg = weather_site.latitude_deg;
            site.longitude_deg = weather_site.longitude_deg;
        }
    }
    sites.sort_by(|left, right| left.level2_id.cmp(&right.level2_id));
    sites.dedup_by(|left, right| left.level2_id == right.level2_id);
    Ok(sites)
}

pub fn latest_level2_object(site: &str, days_back: i64) -> Result<S3Object> {
    recent_level2_objects(site, days_back, 1)?
        .into_iter()
        .next()
        .ok_or_else(|| DataSourceError::NoObjects {
            bucket: LEVEL2_ARCHIVE_BUCKET.to_owned(),
            prefix: site.to_owned(),
        })
}

/// All Level 2 volumes for one site on one UTC date, oldest first — the
/// archive-browser listing.
pub fn level2_objects_for_date(site: &str, date: NaiveDate) -> Result<Vec<S3Object>> {
    let site = site.to_ascii_uppercase();
    let prefix = format!(
        "{:04}/{:02}/{:02}/{}/",
        date.year(),
        date.month(),
        date.day(),
        site
    );
    let mut objects = list_s3(LEVEL2_ARCHIVE_BUCKET, &prefix, None, None)?
        .contents
        .into_iter()
        .filter(|object| object.size > 0 && !object.key.ends_with("_MDM"))
        .collect::<Vec<_>>();
    objects.sort_by(|left, right| left.key.cmp(&right.key));
    Ok(objects)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Level2ArchiveWindowRequest {
    pub start_utc: DateTime<Utc>,
    pub end_utc: DateTime<Utc>,
    pub anchor_utc: DateTime<Utc>,
    pub pad_scans: usize,
    pub extra_start_scans: usize,
    pub extra_end_scans: usize,
    pub max_objects: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Level2ArchiveWindowSelection {
    pub objects: Vec<S3Object>,
    pub selected_index: usize,
}

pub fn level2_objects_for_window(
    site: &str,
    request: &Level2ArchiveWindowRequest,
) -> Result<Level2ArchiveWindowSelection> {
    let site = site.to_ascii_uppercase();
    let end_utc = request.end_utc.max(request.start_utc);
    let mut objects = Vec::new();
    for date in level2_window_listing_dates(request) {
        match level2_objects_for_date(&site, date) {
            Ok(mut listed) => objects.append(&mut listed),
            Err(err) if err.is_not_found() => {}
            Err(err) => return Err(err),
        }
    }
    select_level2_objects_for_window(&objects, request).ok_or_else(|| DataSourceError::NoObjects {
        bucket: LEVEL2_ARCHIVE_BUCKET.to_owned(),
        prefix: format!(
            "{} {}..{}",
            site,
            request.start_utc.to_rfc3339(),
            end_utc.to_rfc3339()
        ),
    })
}

fn level2_window_listing_dates(request: &Level2ArchiveWindowRequest) -> Vec<NaiveDate> {
    let end_utc = request.end_utc.max(request.start_utc);
    let start_date = request.start_utc.date_naive();
    let end_date = end_utc.date_naive();
    // Context scans can live just outside the event's UTC dates. List only
    // the adjacent side(s) the request can actually consume; explicit
    // asymmetric context must work at midnight even when legacy pad_scans=0.
    let needs_previous_date = request.pad_scans > 0 || request.extra_start_scans > 0;
    let needs_next_date = request.pad_scans > 0 || request.extra_end_scans > 0;
    let first_date = if needs_previous_date {
        start_date.pred_opt().unwrap_or(start_date)
    } else {
        start_date
    };
    let last_date = if needs_next_date {
        end_date.succ_opt().unwrap_or(end_date)
    } else {
        end_date
    };
    let mut dates = Vec::new();
    let mut date = first_date;
    while date <= last_date {
        dates.push(date);
        let Some(next) = date.succ_opt() else {
            break;
        };
        date = next;
    }
    dates
}

pub fn select_level2_objects_for_window(
    objects: &[S3Object],
    request: &Level2ArchiveWindowRequest,
) -> Option<Level2ArchiveWindowSelection> {
    if request.max_objects == 0 {
        return None;
    }
    let mut timed = objects
        .iter()
        .filter_map(|object| Some((level2_object_time_utc(object)?, object.clone())))
        .collect::<Vec<_>>();
    if timed.is_empty() {
        return None;
    }
    timed.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.key.cmp(&right.1.key))
    });
    timed.dedup_by(|left, right| left.0 == right.0 && left.1.key == right.1.key);

    let end_utc = request.end_utc.max(request.start_utc);
    let mut start = object_active_at_or_before(&timed, request.start_utc);
    let end = object_active_at_or_before(&timed, end_utc).max(start);
    start = start.saturating_sub(request.pad_scans + request.extra_start_scans);
    let end = (end + request.pad_scans + request.extra_end_scans).min(timed.len() - 1);
    if end + 1 - start > request.max_objects {
        // Cap policy: keep the newest requested scans. For a one-point event
        // this trims pre-event context first and preserves post-event scans.
        // If the cap is smaller than anchor+after context, the anchor can fall
        // out and selected_index intentionally becomes the first available
        // post-anchor scan (nearest-time policy below), never a live/latest
        // volume from outside this archive selection.
        start = end + 1 - request.max_objects;
    }
    let objects = timed[start..=end]
        .iter()
        .map(|(_, object)| object.clone())
        .collect::<Vec<_>>();
    let selected_index = objects
        .iter()
        .enumerate()
        .filter_map(|(index, object)| {
            Some((
                index,
                (level2_object_time_utc(object)? - request.anchor_utc)
                    .num_milliseconds()
                    .unsigned_abs(),
            ))
        })
        .min_by_key(|(_, distance)| *distance)
        .map(|(index, _)| index)
        .unwrap_or(0);
    Some(Level2ArchiveWindowSelection {
        objects,
        selected_index,
    })
}

fn object_active_at_or_before(timed: &[(DateTime<Utc>, S3Object)], target: DateTime<Utc>) -> usize {
    timed
        .partition_point(|(time, _)| *time <= target)
        .saturating_sub(1)
}

pub fn level2_object_time_utc(object: &S3Object) -> Option<DateTime<Utc>> {
    parse_level2_object_time_utc(&object.key)
}

fn parse_level2_object_time_utc(key: &str) -> Option<DateTime<Utc>> {
    let name = key.rsplit('/').next()?;
    let underscore = name.find('_')?;
    if underscore < 8 || name.len() < underscore + 7 {
        return None;
    }
    let date = &name[underscore - 8..underscore];
    let time = &name[underscore + 1..underscore + 7];
    if !date.bytes().all(|byte| byte.is_ascii_digit())
        || !time.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let naive = NaiveDate::parse_from_str(date, "%Y%m%d")
        .ok()?
        .and_hms_opt(
            time[0..2].parse().ok()?,
            time[2..4].parse().ok()?,
            time[4..6].parse().ok()?,
        )?;
    Some(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
}

pub fn recent_level2_objects(
    site: &str,
    days_back: i64,
    max_count: usize,
) -> Result<Vec<S3Object>> {
    if max_count == 0 {
        return Ok(Vec::new());
    }

    let site = site.to_ascii_uppercase();
    let today = Utc::now().date_naive();
    let mut recent = Vec::with_capacity(max_count);
    for offset in 0..=days_back.max(0) {
        let date = today - Duration::days(offset);
        let prefix = format!(
            "{:04}/{:02}/{:02}/{}/",
            date.year(),
            date.month(),
            date.day(),
            site
        );
        let mut objects = list_s3(LEVEL2_ARCHIVE_BUCKET, &prefix, None, None)?
            .contents
            .into_iter()
            .filter(|object| object.size > 0 && !object.key.ends_with("_MDM"))
            .collect::<Vec<_>>();
        objects.sort_by(|left, right| left.key.cmp(&right.key));
        objects.reverse();
        for object in objects {
            recent.push(object);
            if recent.len() >= max_count {
                return Ok(recent);
            }
        }
    }
    if recent.is_empty() {
        Err(DataSourceError::NoObjects {
            bucket: LEVEL2_ARCHIVE_BUCKET.to_owned(),
            prefix: site,
        })
    } else {
        Ok(recent)
    }
}

pub fn latest_level2_object_cached(
    site: &str,
    days_back: i64,
    max_age: StdDuration,
) -> Result<LatestObject> {
    let site = site.to_ascii_uppercase();
    let days_back = days_back.max(0);
    let cache_key = LatestObjectCacheKey {
        site: site.clone(),
        days_back,
    };
    if let Ok(cache) = latest_object_cache().lock()
        && let Some(cached) = cache.get(&cache_key)
        && cached.fetched_at.elapsed() <= max_age
    {
        return Ok(LatestObject {
            object: cached.object.clone(),
            cache_hit: true,
        });
    }

    let object = latest_level2_object(&site, days_back)?;
    if let Ok(mut cache) = latest_object_cache().lock() {
        cache.insert(
            cache_key,
            CachedLatestObject {
                object: object.clone(),
                fetched_at: Instant::now(),
            },
        );
    }
    Ok(LatestObject {
        object,
        cache_hit: false,
    })
}

pub fn latest_realtime_level2_volume(site: &str) -> Result<RealtimeLevel2Volume> {
    latest_realtime_level2_volume_with_listing_ttl(site, REALTIME_ACTIVE_IDS_LISTING_TTL)
}

/// Like [`latest_realtime_level2_volume`] with an explicit TTL for the
/// per-site active-volume-id prefix listing. The live volume's chunk list is
/// memoized for at most `listing_ttl.min(REALTIME_LIVE_VOLUME_LISTING_TTL)`
/// (pass [`StdDuration::ZERO`] to force fresh listings); completed volumes
/// are served from an immutable cache, since their chunk lists can never
/// change again.
pub fn latest_realtime_level2_volume_with_listing_ttl(
    site: &str,
    listing_ttl: StdDuration,
) -> Result<RealtimeLevel2Volume> {
    let site = site.to_ascii_uppercase();
    let (volume, listing_was_cached) = resolve_latest_realtime_volume(&site, listing_ttl)?;
    // Rollover: when the "latest" volume is already complete but the id list
    // came from cache, a newer volume may have appeared since the listing was
    // cached — re-list immediately instead of waiting out the TTL.
    if volume.complete && listing_was_cached {
        active_ids_cache().invalidate(&site);
        let (fresh, _) = resolve_latest_realtime_volume(&site, listing_ttl)?;
        return Ok(fresh);
    }
    Ok(volume)
}

fn resolve_latest_realtime_volume(
    site: &str,
    listing_ttl: StdDuration,
) -> Result<(RealtimeLevel2Volume, bool)> {
    let site_prefix = format!("{site}/");
    let live_listing_ttl = listing_ttl.min(REALTIME_LIVE_VOLUME_LISTING_TTL);
    let (active_ids, listing_was_cached) = match active_ids_cache().get(site, listing_ttl) {
        Some(ids) => (ids, true),
        None => {
            let mut ids = list_s3(LEVEL2_CHUNKS_BUCKET, &site_prefix, Some("/"), None)?
                .common_prefixes
                .into_iter()
                .filter_map(|prefix| realtime_volume_id_from_prefix(site, &prefix.prefix))
                .collect::<Vec<_>>();
            ids.sort_unstable();
            ids.dedup();
            active_ids_cache().insert(site, ids.clone());
            (ids, false)
        }
    };

    let Some(volume_id) = latest_realtime_volume_id_from_active_ids(&active_ids) else {
        return Err(DataSourceError::NoObjects {
            bucket: LEVEL2_CHUNKS_BUCKET.to_owned(),
            prefix: site_prefix,
        });
    };

    let candidates = realtime_volume_candidate_ids_from_active_ids(&active_ids);
    let mut best_volume = None;
    let mut first_error = None;
    for candidate_id in candidates {
        let volume = match completed_volume_cache().get(site, candidate_id) {
            Some(cached) => Ok(cached),
            None => realtime_level2_volume_for_id_memoized(site, candidate_id, live_listing_ttl)
                .inspect(|volume| completed_volume_cache().insert(volume.clone())),
        };
        match volume {
            Ok(volume) => {
                if best_volume
                    .as_ref()
                    .is_none_or(|best: &RealtimeLevel2Volume| {
                        volume.volume_time > best.volume_time
                            || (volume.volume_time == best.volume_time
                                && volume.chunks.len() > best.chunks.len())
                    })
                {
                    best_volume = Some(volume);
                }
            }
            Err(err) => {
                first_error.get_or_insert(err);
            }
        }
    }

    if let Some(volume) = best_volume {
        if realtime_volume_is_stale(&volume, Utc::now())
            && let Ok(rescued) = latest_realtime_volume_by_chunk_scan(site)
            && rescued.volume_time > volume.volume_time
        {
            completed_volume_cache().insert(rescued.clone());
            return Ok((rescued, listing_was_cached));
        }
        return Ok((volume, listing_was_cached));
    }

    realtime_level2_volume_for_id_memoized(site, volume_id, live_listing_ttl)
        .inspect(|volume| completed_volume_cache().insert(volume.clone()))
        .map(|volume| (volume, listing_was_cached))
        .map_err(|_| {
            first_error.unwrap_or(DataSourceError::NoObjects {
                bucket: LEVEL2_CHUNKS_BUCKET.to_owned(),
                prefix: site_prefix,
            })
        })
}

fn realtime_volume_is_stale(volume: &RealtimeLevel2Volume, now: DateTime<Utc>) -> bool {
    now.signed_duration_since(volume.volume_time).num_seconds()
        > REALTIME_CHUNK_STALE_RESCAN_AGE_SECONDS
}

fn latest_realtime_volume_by_chunk_scan(site: &str) -> Result<RealtimeLevel2Volume> {
    let site_prefix = format!("{site}/");
    let listing = list_s3_all_limited(
        LEVEL2_CHUNKS_BUCKET,
        &site_prefix,
        None,
        Some(REALTIME_CHUNK_LIST_MAX_KEYS),
        REALTIME_CHUNK_STALE_RESCAN_MAX_PAGES,
    )?;
    let chunks = listing
        .contents
        .into_iter()
        .filter(|object| object.size > 0)
        .filter_map(parse_realtime_chunk_object);
    latest_realtime_volume_from_chunks(site, chunks).ok_or_else(|| DataSourceError::NoObjects {
        bucket: LEVEL2_CHUNKS_BUCKET.to_owned(),
        prefix: site_prefix,
    })
}

/// [`realtime_level2_volume_for_id`] behind the short live-volume listing
/// memo: within `max_age` of a fetch, pollers of the same (site, volume id)
/// reuse the listing instead of issuing duplicate chunk LISTs. The lock is
/// never held across the network fetch, so a race at most double-fetches;
/// errors are never memoized.
fn realtime_level2_volume_for_id_memoized(
    site: &str,
    volume_id: u16,
    max_age: StdDuration,
) -> Result<RealtimeLevel2Volume> {
    if let Some(cached) = live_volume_listing_cache().get(site, volume_id, max_age) {
        return Ok(cached);
    }
    realtime_level2_volume_for_id(site, volume_id)
        .inspect(|volume| live_volume_listing_cache().insert(volume.clone()))
}

fn realtime_level2_volume_for_id(site: &str, volume_id: u16) -> Result<RealtimeLevel2Volume> {
    let volume_prefix = format!("{site}/{volume_id}/");
    let mut chunks = list_s3_limited(
        LEVEL2_CHUNKS_BUCKET,
        &volume_prefix,
        None,
        None,
        Some(REALTIME_CHUNK_LIST_MAX_KEYS),
    )?
    .contents
    .into_iter()
    .filter(|object| object.size > 0)
    .filter_map(parse_realtime_chunk_object)
    .collect::<Vec<_>>();
    chunks.retain(|chunk| chunk.volume_id == volume_id);

    latest_realtime_volume_from_chunks(site, chunks).ok_or_else(|| DataSourceError::NoObjects {
        bucket: LEVEL2_CHUNKS_BUCKET.to_owned(),
        prefix: volume_prefix,
    })
}

fn latest_realtime_volume_from_chunks(
    site: &str,
    chunks: impl IntoIterator<Item = RealtimeChunkObject>,
) -> Option<RealtimeLevel2Volume> {
    let mut grouped: BTreeMap<(u16, DateTime<Utc>), Vec<RealtimeChunkObject>> = BTreeMap::new();
    for chunk in chunks {
        if chunk.site == site {
            grouped
                .entry((chunk.volume_id, chunk.volume_time))
                .or_default()
                .push(chunk);
        }
    }

    grouped
        .into_iter()
        .map(|((volume_id, volume_time), mut chunks)| {
            chunks.sort_by_key(|chunk| chunk.chunk_id);
            let complete = chunks.last().is_some_and(|chunk| chunk.chunk_type.is_end());
            let total_size = chunks.iter().map(|chunk| chunk.object.size).sum();
            RealtimeLevel2Volume {
                site: site.to_owned(),
                volume_id,
                volume_time,
                chunks,
                complete,
                total_size,
            }
        })
        .max_by(|left, right| {
            left.volume_time
                .cmp(&right.volume_time)
                .then_with(|| left.chunks.len().cmp(&right.chunks.len()))
        })
}

pub fn download_realtime_volume(
    volume: &RealtimeLevel2Volume,
    cache_dir: &Path,
) -> Result<DownloadedObject> {
    fs::create_dir_all(cache_dir)?;
    let filename = realtime_volume_cache_filename(volume);
    let path = cache_dir.join(&filename);
    let url = format!(
        "https://{}.s3.amazonaws.com/{}/{}/",
        LEVEL2_CHUNKS_BUCKET, volume.site, volume.volume_id
    );

    if path
        .metadata()
        .map(|metadata| metadata.len() == volume.total_size)
        .unwrap_or(false)
    {
        return Ok(DownloadedObject {
            object: S3Object {
                key: filename,
                size: volume.total_size,
                last_modified: volume
                    .chunks
                    .last()
                    .and_then(|chunk| chunk.object.last_modified),
            },
            path,
            url,
            cache_hit: true,
        });
    }

    let chunk_cache_dir = cache_dir.join(".chunks").join(format!(
        "{}_{}_{:03}",
        volume.site,
        volume.volume_time.format("%Y%m%d_%H%M%S"),
        volume.volume_id
    ));
    fs::create_dir_all(&chunk_cache_dir)?;

    let mut chunk_paths = Vec::with_capacity(volume.chunks.len());
    let mut missing = Vec::new();
    for chunk in &volume.chunks {
        let chunk_filename = chunk
            .object
            .key
            .rsplit('/')
            .next()
            .unwrap_or(&chunk.object.key);
        let chunk_path = chunk_cache_dir.join(chunk_filename);
        let cache_hit = chunk_path
            .metadata()
            .map(|metadata| metadata.len() == chunk.object.size)
            .unwrap_or(false);
        if !cache_hit {
            missing.push((chunk.object.clone(), chunk_path.clone()));
        }
        chunk_paths.push(chunk_path);
    }

    // No batch barrier: each worker claims the next missing chunk as soon as
    // it finishes its current one, so one slow chunk never stalls the rest.
    for_each_concurrent(
        &missing,
        REALTIME_CHUNK_DOWNLOAD_CONCURRENCY,
        |(object, path)| download_s3_object_to_path(LEVEL2_CHUNKS_BUCKET, object, path),
    )?;

    if let Ok(existing_len) = path.metadata().map(|metadata| metadata.len())
        && let Some(prefix_chunks) = chunk_prefix_count_for_size(volume, existing_len)
        && prefix_chunks > 0
        && prefix_chunks < chunk_paths.len()
    {
        append_realtime_chunks(
            &path,
            &chunk_paths[prefix_chunks..],
            existing_len,
            volume.total_size,
            &url,
        )?;
        return Ok(DownloadedObject {
            object: S3Object {
                key: filename,
                size: volume.total_size,
                last_modified: volume
                    .chunks
                    .last()
                    .and_then(|chunk| chunk.object.last_modified),
            },
            path,
            url,
            cache_hit: false,
        });
    }

    let temp_path = path.with_extension("download");
    let mut temp_file = fs::File::create(&temp_path)?;
    for chunk_path in &chunk_paths {
        let mut chunk_file = fs::File::open(chunk_path)?;
        io::copy(&mut chunk_file, &mut temp_file)?;
    }
    drop(temp_file);

    let copied = temp_path.metadata()?.len();
    if copied != volume.total_size {
        let _ = fs::remove_file(&temp_path);
        return Err(DataSourceError::DownloadSizeMismatch {
            url,
            expected: volume.total_size,
            actual: copied,
        });
    }
    if path.exists() {
        fs::remove_file(&path)?;
    }
    fs::rename(&temp_path, &path)?;

    Ok(DownloadedObject {
        object: S3Object {
            key: filename,
            size: volume.total_size,
            last_modified: volume
                .chunks
                .last()
                .and_then(|chunk| chunk.object.last_modified),
        },
        path,
        url,
        cache_hit: false,
    })
}

pub fn download_object(
    bucket: &str,
    object: S3Object,
    cache_dir: &Path,
) -> Result<DownloadedObject> {
    fs::create_dir_all(cache_dir)?;
    let filename = object.key.rsplit('/').next().unwrap_or(&object.key);
    let path = cache_dir.join(filename);
    let url = format!("https://{bucket}.s3.amazonaws.com/{}", object.key);
    if path
        .metadata()
        .map(|metadata| metadata.len() == object.size)
        .unwrap_or(false)
    {
        return Ok(DownloadedObject {
            object,
            path,
            url,
            cache_hit: true,
        });
    }

    download_s3_object_to_path(bucket, &object, &path)?;
    Ok(DownloadedObject {
        object,
        path,
        url,
        cache_hit: false,
    })
}

pub fn newest_cached_level2_path(cache_dir: &Path) -> Result<Option<PathBuf>> {
    if !cache_dir.exists() {
        return Ok(None);
    }

    let mut newest: Option<(String, PathBuf)> = None;
    for entry in fs::read_dir(cache_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.ends_with(".download") || name.ends_with("_MDM") {
            continue;
        }
        if path.metadata().map(|metadata| metadata.len() == 0)? {
            continue;
        }
        if newest
            .as_ref()
            .is_none_or(|(newest_name, _)| name > newest_name.as_str())
        {
            newest = Some((name.to_owned(), path));
        }
    }

    Ok(newest.map(|(_, path)| path))
}

fn list_s3(
    bucket: &str,
    prefix: &str,
    delimiter: Option<&str>,
    continuation_token: Option<&str>,
) -> Result<S3Listing> {
    list_s3_limited(bucket, prefix, delimiter, continuation_token, None)
}

fn list_s3_limited(
    bucket: &str,
    prefix: &str,
    delimiter: Option<&str>,
    continuation_token: Option<&str>,
    max_keys: Option<usize>,
) -> Result<S3Listing> {
    let url = format!("https://{bucket}.s3.amazonaws.com/");
    let client = metadata_http_client();
    let mut query = vec![("list-type", "2".to_owned()), ("prefix", prefix.to_owned())];
    if let Some(delimiter) = delimiter {
        query.push(("delimiter", delimiter.to_owned()));
    }
    if let Some(token) = continuation_token {
        query.push(("continuation-token", token.to_owned()));
    }
    if let Some(max_keys) = max_keys {
        query.push(("max-keys", max_keys.to_string()));
    }
    let text = client
        .get(url)
        .query(&query)
        .send()?
        .error_for_status()?
        .text()?;
    let parsed: S3ListingXml = quick_xml::de::from_str(&text)?;
    Ok(parsed.into())
}

fn list_s3_all_limited(
    bucket: &str,
    prefix: &str,
    delimiter: Option<&str>,
    max_keys: Option<usize>,
    max_pages: usize,
) -> Result<S3Listing> {
    let mut contents = Vec::new();
    let mut common_prefixes = Vec::new();
    let mut continuation_token = None;
    for _ in 0..max_pages.max(1) {
        let listing = list_s3_limited(
            bucket,
            prefix,
            delimiter,
            continuation_token.as_deref(),
            max_keys,
        )?;
        contents.extend(listing.contents);
        common_prefixes.extend(listing.common_prefixes);
        continuation_token = listing.next_continuation_token;
        if continuation_token.is_none() {
            break;
        }
    }
    Ok(S3Listing {
        contents,
        common_prefixes,
        next_continuation_token: continuation_token,
    })
}

fn realtime_volume_id_from_prefix(site: &str, prefix: &str) -> Option<u16> {
    let trimmed = prefix.trim_end_matches('/');
    let mut parts = trimmed.split('/');
    let prefix_site = parts.next()?;
    if prefix_site != site {
        return None;
    }
    let volume_id = parts.next()?.parse::<u16>().ok()?;
    if parts.next().is_some() || volume_id >= REALTIME_VOLUME_ID_MODULUS {
        return None;
    }
    Some(volume_id)
}

fn latest_realtime_volume_id_from_active_ids(ids: &[u16]) -> Option<u16> {
    let mut ids = ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return None;
    }
    if ids.len() == 1 {
        return ids.first().copied();
    }

    let mut largest_gap = 0u16;
    let mut latest_id = *ids.last()?;
    for (index, current) in ids.iter().copied().enumerate() {
        let next = if index + 1 == ids.len() {
            ids[0] + REALTIME_VOLUME_ID_MODULUS
        } else {
            ids[index + 1]
        };
        let gap = next - current;
        if gap > largest_gap {
            largest_gap = gap;
            latest_id = current;
        }
    }

    if largest_gap <= 1 {
        ids.last().copied()
    } else {
        Some(latest_id)
    }
}

fn realtime_volume_candidate_ids_from_active_ids(ids: &[u16]) -> Vec<u16> {
    let mut ids = ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return Vec::new();
    }
    if ids.len() == 1 {
        return ids;
    }

    let mut candidates = Vec::new();
    for (index, current) in ids.iter().copied().enumerate() {
        let next = if index + 1 == ids.len() {
            ids[0] + REALTIME_VOLUME_ID_MODULUS
        } else {
            ids[index + 1]
        };
        if next - current > 1 {
            candidates.push(current);
        }
    }
    if candidates.is_empty() {
        candidates.push(*ids.last().expect("non-empty ids"));
    }
    candidates
}

fn parse_realtime_chunk_object(object: S3Object) -> Option<RealtimeChunkObject> {
    let key = object.key.clone();
    let mut path_parts = key.split('/');
    let site = path_parts.next()?.to_owned();
    let volume_id = path_parts.next()?.parse::<u16>().ok()?;
    let filename = path_parts.next()?;
    if path_parts.next().is_some() || volume_id >= REALTIME_VOLUME_ID_MODULUS {
        return None;
    }

    let mut name_parts = filename.split('-');
    let date = name_parts.next()?;
    let time = name_parts.next()?;
    let chunk_id = name_parts.next()?.parse::<u16>().ok()?;
    let chunk_type = RealtimeChunkType::from_code(name_parts.next()?)?;
    if name_parts.next().is_some() {
        return None;
    }

    let volume_time = NaiveDateTime::parse_from_str(&format!("{date}{time}"), "%Y%m%d%H%M%S")
        .ok()?
        .and_utc();

    Some(RealtimeChunkObject {
        object,
        site,
        volume_id,
        volume_time,
        chunk_id,
        chunk_type,
    })
}

fn realtime_volume_cache_filename(volume: &RealtimeLevel2Volume) -> String {
    format!(
        "{}{}_RT{:03}_V06",
        volume.site,
        volume.volume_time.format("%Y%m%d_%H%M%S"),
        volume.volume_id
    )
}

fn chunk_prefix_count_for_size(volume: &RealtimeLevel2Volume, size: u64) -> Option<usize> {
    if size == 0 {
        return Some(0);
    }

    let mut prefix_size = 0u64;
    for (index, chunk) in volume.chunks.iter().enumerate() {
        prefix_size = prefix_size.checked_add(chunk.object.size)?;
        if prefix_size == size {
            return Some(index + 1);
        }
        if prefix_size > size {
            return None;
        }
    }

    None
}

fn append_realtime_chunks(
    path: &Path,
    chunk_paths: &[PathBuf],
    expected_existing: u64,
    expected_total: u64,
    url: &str,
) -> Result<()> {
    let mut output = fs::OpenOptions::new().append(true).open(path)?;
    for chunk_path in chunk_paths {
        let mut chunk_file = fs::File::open(chunk_path)?;
        io::copy(&mut chunk_file, &mut output)?;
    }
    drop(output);

    let actual = path.metadata()?.len();
    if actual != expected_total {
        return Err(DataSourceError::DownloadSizeMismatch {
            url: url.to_owned(),
            expected: expected_total,
            actual,
        });
    }
    if actual < expected_existing {
        return Err(DataSourceError::DownloadSizeMismatch {
            url: url.to_owned(),
            expected: expected_existing,
            actual,
        });
    }
    Ok(())
}

fn download_s3_object_to_path(bucket: &str, object: &S3Object, path: &Path) -> Result<()> {
    let url = format!("https://{bucket}.s3.amazonaws.com/{}", object.key);
    let mut response = download_http_client()
        .get(&url)
        .send()?
        .error_for_status()?;
    let temp_path = path.with_extension("download");
    let mut temp_file = fs::File::create(&temp_path)?;
    let copied = io::copy(&mut response, &mut temp_file)?;
    drop(temp_file);
    if copied != object.size {
        let _ = fs::remove_file(&temp_path);
        return Err(DataSourceError::DownloadSizeMismatch {
            url,
            expected: object.size,
            actual: copied,
        });
    }
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(&temp_path, path)?;
    Ok(())
}

fn metadata_http_client() -> reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            build_http_client(HTTP_METADATA_TIMEOUT)
                .expect("metadata HTTP client should be constructible")
        })
        .clone()
}

fn download_http_client() -> reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            build_http_client(HTTP_DOWNLOAD_TIMEOUT)
                .expect("download HTTP client should be constructible")
        })
        .clone()
}

/// Sectigo "Public Server Authentication CA DV R36" intermediate (valid to
/// 2036-03-21, chains to the Mozilla-trusted Sectigo Root R46), fetched from
/// the certificate's own AIA URL
/// (`http://crt.sectigo.com/SectigoPublicServerAuthenticationCADVR36.crt`).
///
/// SHMU's open-data server (opendata.shmu.sk, the Slovak radar volume feed)
/// sends an incomplete TLS chain — the leaf only. Browsers and schannel
/// repair that by chasing the AIA URL; rustls deliberately does not, so
/// without this anchor every fetch from the feed fails the TLS handshake.
const SECTIGO_DV_R36_INTERMEDIATE_PEM: &str =
    include_str!("../certs/sectigo_public_server_authentication_ca_dv_r36.pem");

fn build_http_client(timeout: StdDuration) -> Result<reqwest::blocking::Client> {
    let mut builder = reqwest::blocking::Client::builder()
        .user_agent(HTTP_USER_AGENT)
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        // Below S3's ~20 s idle close: the pollers tick every 60 s, so a
        // pooled keep-alive from the previous tick is ALWAYS stale by S3's
        // rules and reusing it fails the send (field report: FMI listing
        // "error sending request" every tick).
        .pool_idle_timeout(StdDuration::from_secs(15))
        .timeout(timeout);
    // Extra trust anchor for AIA-incomplete servers (see the constant's
    // docs). Skipped, never fatal, if the embedded PEM fails to parse.
    if let Ok(cert) = reqwest::Certificate::from_pem(SECTIGO_DV_R36_INTERMEDIATE_PEM.as_bytes()) {
        builder = builder.add_root_certificate(cert);
    }
    Ok(builder.build()?)
}

fn latest_object_cache() -> &'static Mutex<BTreeMap<LatestObjectCacheKey, CachedLatestObject>> {
    static CACHE: OnceLock<Mutex<BTreeMap<LatestObjectCacheKey, CachedLatestObject>>> =
        OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Run `f` over every item with at most `max_workers` concurrent calls and no
/// batch barrier: each worker claims the next unprocessed item as soon as it
/// finishes its current one. On failure, in-flight work completes, remaining
/// items are skipped, and the earliest failing item's error is returned.
fn for_each_concurrent<T, F>(items: &[T], max_workers: usize, f: F) -> Result<()>
where
    T: Sync,
    F: Fn(&T) -> Result<()> + Sync,
{
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    if items.is_empty() {
        return Ok(());
    }
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let first_error: Mutex<Option<(usize, DataSourceError)>> = Mutex::new(None);
    let workers = max_workers.max(1).min(items.len());

    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            handles.push(scope.spawn(|| {
                loop {
                    if failed.load(Ordering::Relaxed) {
                        break;
                    }
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= items.len() {
                        break;
                    }
                    if let Err(err) = f(&items[index]) {
                        failed.store(true, Ordering::Relaxed);
                        if let Ok(mut slot) = first_error.lock()
                            && slot.as_ref().is_none_or(|(earliest, _)| index < *earliest)
                        {
                            *slot = Some((index, err));
                        }
                    }
                }
            }));
        }
        for handle in handles {
            if handle.join().is_err() {
                failed.store(true, Ordering::Relaxed);
                if let Ok(mut slot) = first_error.lock()
                    && slot.is_none()
                {
                    *slot = Some((usize::MAX, DataSourceError::DownloadWorkerPanic));
                }
            }
        }
    });

    match first_error.into_inner() {
        Ok(Some((_, err))) => Err(err),
        _ => Ok(()),
    }
}

/// Per-site cache of the realtime chunk bucket's active-volume-id listing.
#[derive(Default)]
struct ActiveIdsCache {
    entries: Mutex<BTreeMap<String, (Vec<u16>, Instant)>>,
}

impl ActiveIdsCache {
    fn get(&self, site: &str, max_age: StdDuration) -> Option<Vec<u16>> {
        let entries = self.entries.lock().ok()?;
        let (ids, fetched_at) = entries.get(site)?;
        (fetched_at.elapsed() < max_age).then(|| ids.clone())
    }

    fn insert(&self, site: &str, ids: Vec<u16>) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.insert(site.to_owned(), (ids, Instant::now()));
        }
    }

    fn invalidate(&self, site: &str) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.remove(site);
        }
    }
}

fn active_ids_cache() -> &'static ActiveIdsCache {
    static CACHE: OnceLock<ActiveIdsCache> = OnceLock::new();
    CACHE.get_or_init(ActiveIdsCache::default)
}

/// Completed realtime volumes keyed by site: once the end chunk has been
/// observed the chunk list is immutable, so it never needs re-listing.
#[derive(Default)]
struct CompletedVolumeCache {
    entries: Mutex<BTreeMap<String, Vec<RealtimeLevel2Volume>>>,
}

impl CompletedVolumeCache {
    fn get(&self, site: &str, volume_id: u16) -> Option<RealtimeLevel2Volume> {
        let entries = self.entries.lock().ok()?;
        entries
            .get(site)?
            .iter()
            .find(|volume| volume.volume_id == volume_id)
            .cloned()
    }

    fn insert(&self, volume: RealtimeLevel2Volume) {
        if !volume.complete {
            return;
        }
        if let Ok(mut entries) = self.entries.lock() {
            let volumes = entries.entry(volume.site.clone()).or_default();
            volumes.retain(|existing| existing.volume_id != volume.volume_id);
            volumes.push(volume);
            volumes.sort_by_key(|volume| volume.volume_time);
            while volumes.len() > COMPLETED_VOLUME_CACHE_PER_SITE {
                volumes.remove(0);
            }
        }
    }
}

fn completed_volume_cache() -> &'static CompletedVolumeCache {
    static CACHE: OnceLock<CompletedVolumeCache> = OnceLock::new();
    CACHE.get_or_init(CompletedVolumeCache::default)
}

/// Live (possibly incomplete) volume chunk listings keyed by (site, volume
/// id). Unlike [`CompletedVolumeCache`] these listings can still grow, so an
/// entry is only served while younger than the caller's TTL, which
/// [`resolve_latest_realtime_volume`] caps at
/// [`REALTIME_LIVE_VOLUME_LISTING_TTL`].
#[derive(Default)]
struct LiveVolumeListingCache {
    entries: Mutex<BTreeMap<(String, u16), (RealtimeLevel2Volume, Instant)>>,
}

impl LiveVolumeListingCache {
    fn get(
        &self,
        site: &str,
        volume_id: u16,
        max_age: StdDuration,
    ) -> Option<RealtimeLevel2Volume> {
        let entries = self.entries.lock().ok()?;
        let (volume, fetched_at) = entries.get(&(site.to_owned(), volume_id))?;
        (fetched_at.elapsed() < max_age).then(|| volume.clone())
    }

    fn insert(&self, volume: RealtimeLevel2Volume) {
        if let Ok(mut entries) = self.entries.lock() {
            // No entry is ever served past the cap TTL, so pruning here
            // bounds the map to sites polled within the last TTL window.
            entries.retain(|_, (_, fetched_at)| {
                fetched_at.elapsed() < REALTIME_LIVE_VOLUME_LISTING_TTL
            });
            entries.insert(
                (volume.site.clone(), volume.volume_id),
                (volume, Instant::now()),
            );
        }
    }
}

fn live_volume_listing_cache() -> &'static LiveVolumeListingCache {
    static CACHE: OnceLock<LiveVolumeListingCache> = OnceLock::new();
    CACHE.get_or_init(LiveVolumeListingCache::default)
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct LatestObjectCacheKey {
    site: String,
    days_back: i64,
}

#[derive(Clone, Debug)]
struct CachedLatestObject {
    object: S3Object,
    fetched_at: Instant,
}

#[derive(Debug, Deserialize)]
struct S3ListingXml {
    #[serde(rename = "Contents", default)]
    contents: Vec<S3ObjectXml>,
    #[serde(rename = "CommonPrefixes", default)]
    common_prefixes: Vec<CommonPrefixXml>,
    #[serde(rename = "NextContinuationToken")]
    next_continuation_token: Option<String>,
}

impl From<S3ListingXml> for S3Listing {
    fn from(value: S3ListingXml) -> Self {
        Self {
            contents: value.contents.into_iter().map(Into::into).collect(),
            common_prefixes: value.common_prefixes.into_iter().map(Into::into).collect(),
            next_continuation_token: value.next_continuation_token,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CommonPrefix {
    prefix: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct S3Listing {
    contents: Vec<S3Object>,
    common_prefixes: Vec<CommonPrefix>,
    next_continuation_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WeatherGovFeatureCollection {
    features: Vec<WeatherGovFeature>,
}

#[derive(Debug, Deserialize)]
struct WeatherGovFeature {
    geometry: Option<WeatherGovGeometry>,
    properties: WeatherGovProperties,
}

#[derive(Debug, Deserialize)]
struct WeatherGovGeometry {
    coordinates: Vec<f64>,
}

#[derive(Debug, Deserialize)]
struct WeatherGovProperties {
    id: Option<String>,
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct S3ObjectXml {
    #[serde(rename = "Key")]
    key: String,
    #[serde(rename = "LastModified")]
    last_modified: Option<String>,
    #[serde(rename = "Size")]
    size: u64,
}

#[derive(Debug, Deserialize)]
struct CommonPrefixXml {
    #[serde(rename = "Prefix")]
    prefix: String,
}

impl From<S3ObjectXml> for S3Object {
    fn from(value: S3ObjectXml) -> Self {
        Self {
            key: value.key,
            size: value.size,
            last_modified: value
                .last_modified
                .as_deref()
                .and_then(parse_s3_last_modified),
        }
    }
}

fn parse_s3_last_modified(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

impl From<CommonPrefixXml> for CommonPrefix {
    fn from(value: CommonPrefixXml) -> Self {
        Self {
            prefix: value.prefix,
        }
    }
}

// The bare-id fallback list was superseded by embedded_sites.rs, which
// carries coordinates (208 stations, weather.gov-generated).

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn for_each_concurrent_runs_every_item_without_batch_barriers() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let items: Vec<usize> = (0..100).collect();
        let ran = AtomicUsize::new(0);
        let live = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let result: Result<()> = for_each_concurrent(&items, 4, |_| {
            let now = live.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            thread::sleep(StdDuration::from_millis(1));
            live.fetch_sub(1, Ordering::SeqCst);
            ran.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        assert!(result.is_ok());
        assert_eq!(ran.load(Ordering::SeqCst), 100);
        assert!(peak.load(Ordering::SeqCst) <= 4, "worker cap exceeded");
    }

    #[test]
    fn for_each_concurrent_propagates_the_earliest_item_error() {
        let items: Vec<usize> = (0..32).collect();
        let result = for_each_concurrent(&items, 4, |item| {
            if *item == 7 || *item == 21 {
                Err(DataSourceError::DownloadWorkerPanic)
            } else {
                Ok(())
            }
        });
        assert!(matches!(result, Err(DataSourceError::DownloadWorkerPanic)));
    }

    #[test]
    fn active_ids_cache_serves_within_ttl_and_expires() {
        let cache = ActiveIdsCache::default();
        assert!(cache.get("KEAX", StdDuration::from_secs(60)).is_none());
        cache.insert("KEAX", vec![41, 42]);
        assert_eq!(
            cache.get("KEAX", StdDuration::from_secs(60)),
            Some(vec![41, 42])
        );
        // A zero max-age always re-lists.
        assert!(cache.get("KEAX", StdDuration::ZERO).is_none());
        cache.invalidate("KEAX");
        assert!(cache.get("KEAX", StdDuration::from_secs(60)).is_none());
    }

    fn realtime_volume_fixture(volume_id: u16, complete: bool) -> RealtimeLevel2Volume {
        RealtimeLevel2Volume {
            site: "KEAX".to_owned(),
            volume_id,
            volume_time: chrono::Utc.with_ymd_and_hms(2026, 6, 9, 5, 51, 0).unwrap()
                + chrono::Duration::minutes(i64::from(volume_id)),
            chunks: Vec::new(),
            complete,
            total_size: 0,
        }
    }

    #[test]
    fn completed_volume_cache_only_keeps_complete_volumes() {
        let cache = CompletedVolumeCache::default();
        cache.insert(realtime_volume_fixture(7, false));
        assert!(cache.get("KEAX", 7).is_none(), "incomplete volume cached");
        cache.insert(realtime_volume_fixture(8, true));
        assert_eq!(cache.get("KEAX", 8).map(|v| v.volume_id), Some(8));
        assert!(cache.get("KTLX", 8).is_none(), "cache leaked across sites");
    }

    #[test]
    fn completed_volume_cache_evicts_oldest_per_site() {
        let cache = CompletedVolumeCache::default();
        for volume_id in 0..(COMPLETED_VOLUME_CACHE_PER_SITE as u16 + 3) {
            cache.insert(realtime_volume_fixture(volume_id, true));
        }
        assert!(
            cache.get("KEAX", 0).is_none(),
            "oldest volume should be evicted"
        );
        let newest = COMPLETED_VOLUME_CACHE_PER_SITE as u16 + 2;
        assert_eq!(cache.get("KEAX", newest).map(|v| v.volume_id), Some(newest));
    }

    #[test]
    fn live_volume_listing_cache_expires_by_ttl() {
        let cache = LiveVolumeListingCache::default();
        cache.insert(realtime_volume_fixture(7, false));
        assert_eq!(
            cache
                .get("KEAX", 7, StdDuration::from_secs(60))
                .map(|v| v.volume_id),
            Some(7),
            "live (incomplete) volumes must be memoizable"
        );
        // A zero max-age always re-lists.
        assert!(cache.get("KEAX", 7, StdDuration::ZERO).is_none());
    }

    #[test]
    fn live_volume_listing_cache_isolates_sites_and_volume_ids() {
        let cache = LiveVolumeListingCache::default();
        cache.insert(realtime_volume_fixture(7, false));
        cache.insert(realtime_volume_fixture(8, true));
        assert!(
            cache.get("KTLX", 7, StdDuration::from_secs(60)).is_none(),
            "cache leaked across sites"
        );
        assert!(
            cache.get("KEAX", 9, StdDuration::from_secs(60)).is_none(),
            "cache leaked across volume ids"
        );
        assert_eq!(
            cache
                .get("KEAX", 7, StdDuration::from_secs(60))
                .map(|v| v.complete),
            Some(false)
        );
        assert_eq!(
            cache
                .get("KEAX", 8, StdDuration::from_secs(60))
                .map(|v| v.volume_id),
            Some(8)
        );
    }

    #[test]
    fn live_volume_listing_ttl_stays_under_primary_poll_cadence() {
        // The primary realtime poller ticks at 1 s and must never be served
        // a memoized live listing across two of its own ticks.
        assert!(REALTIME_LIVE_VOLUME_LISTING_TTL < StdDuration::from_secs(1));
    }

    #[test]
    fn site_can_carry_location() {
        let site = RadarSite::new("KTLX").with_location(
            Some("Norman".to_owned()),
            Some(35.333),
            Some(-97.278),
        );
        assert_eq!(site.name.as_deref(), Some("Norman"));
        assert_eq!(site.latitude_deg, Some(35.333));
    }

    #[test]
    fn level2_object_time_parses_plain_and_compressed_archive_keys() {
        assert_eq!(
            parse_level2_object_time_utc("2026/06/09/KTLX/KTLX20260609_235423_V06")
                .expect("plain key")
                .to_rfc3339(),
            "2026-06-09T23:54:23+00:00"
        );
        assert_eq!(
            parse_level2_object_time_utc("2011/04/27/KBMX/KBMX20110427_221510_V03.gz")
                .expect("compressed key")
                .to_rfc3339(),
            "2011-04-27T22:15:10+00:00"
        );
        assert!(parse_level2_object_time_utc("bad-key").is_none());
    }

    #[test]
    fn archive_window_selection_pads_caps_and_keeps_tail() {
        let objects = test_level2_objects(
            "KTLX",
            &[
                "200000", "200500", "201000", "201500", "202000", "202500", "203000",
            ],
        );
        let request = Level2ArchiveWindowRequest {
            start_utc: test_time("20260609", "200800"),
            end_utc: test_time("20260609", "202300"),
            anchor_utc: test_time("20260609", "201500"),
            pad_scans: 1,
            extra_start_scans: 0,
            extra_end_scans: 0,
            max_objects: 4,
        };

        let selected = select_level2_objects_for_window(&objects, &request).expect("selection");

        assert_eq!(selected.objects.len(), 4);
        assert!(selected.objects[0].key.contains("_201000_"));
        assert!(selected.objects[3].key.contains("_202500_"));
        assert_eq!(selected.selected_index, 1);
    }

    #[test]
    fn archive_window_selection_can_span_midnight() {
        let objects = vec![
            test_level2_object("KTLX", "20260609", "235500"),
            test_level2_object("KTLX", "20260610", "000200"),
            test_level2_object("KTLX", "20260610", "000700"),
        ];
        let request = Level2ArchiveWindowRequest {
            start_utc: test_time("20260609", "235900"),
            end_utc: test_time("20260610", "000300"),
            anchor_utc: test_time("20260610", "000100"),
            pad_scans: 0,
            extra_start_scans: 0,
            extra_end_scans: 0,
            max_objects: 10,
        };

        let selected = select_level2_objects_for_window(&objects, &request).expect("selection");

        assert_eq!(selected.objects.len(), 2);
        assert!(selected.objects[0].key.contains("20260609_235500"));
        assert!(selected.objects[1].key.contains("20260610_000200"));
        assert_eq!(selected.selected_index, 1);
    }

    #[test]
    fn archive_window_selection_supports_asymmetric_extra_scans() {
        let objects = test_level2_objects(
            "KTLX",
            &[
                "195000", "195500", "200000", "200500", "201000", "201500", "202000", "202500",
                "203000",
            ],
        );
        let request = Level2ArchiveWindowRequest {
            start_utc: test_time("20260609", "200600"),
            end_utc: test_time("20260609", "201600"),
            anchor_utc: test_time("20260609", "201000"),
            pad_scans: 0,
            extra_start_scans: 2,
            extra_end_scans: 1,
            max_objects: 10,
        };

        let selected = select_level2_objects_for_window(&objects, &request).expect("selection");

        assert_eq!(
            selected
                .objects
                .iter()
                .filter_map(level2_object_time_utc)
                .map(|time| time.format("%H%M%S").to_string())
                .collect::<Vec<_>>(),
            vec!["195500", "200000", "200500", "201000", "201500", "202000"]
        );
        assert_eq!(selected.selected_index, 3);
    }

    #[test]
    fn point_archive_window_keeps_after_context_when_capped() {
        let objects = test_level2_objects(
            "KTLX",
            &[
                "195500", "200000", "200500", "201000", "201500", "202000", "202500",
            ],
        );
        let mut request = Level2ArchiveWindowRequest {
            start_utc: test_time("20260609", "200700"),
            end_utc: test_time("20260609", "200700"),
            anchor_utc: test_time("20260609", "200700"),
            pad_scans: 0,
            extra_start_scans: 2,
            extra_end_scans: 3,
            max_objects: 4,
        };

        let selected = select_level2_objects_for_window(&objects, &request).expect("selection");
        assert_eq!(
            selected
                .objects
                .iter()
                .filter_map(level2_object_time_utc)
                .map(|time| time.format("%H%M%S").to_string())
                .collect::<Vec<_>>(),
            vec!["200500", "201000", "201500", "202000"]
        );
        assert_eq!(selected.selected_index, 0, "anchor scan stays selected");

        request.max_objects = 2;
        let selected = select_level2_objects_for_window(&objects, &request).expect("selection");
        assert_eq!(
            selected
                .objects
                .iter()
                .filter_map(level2_object_time_utc)
                .map(|time| time.format("%H%M%S").to_string())
                .collect::<Vec<_>>(),
            vec!["201500", "202000"]
        );
        assert_eq!(
            selected.selected_index, 0,
            "when the cap excludes the anchor, select the first post-anchor scan"
        );
    }

    #[test]
    fn archive_window_listing_dates_include_only_requested_context_days() {
        let mut request = Level2ArchiveWindowRequest {
            start_utc: test_time("20260610", "000200"),
            end_utc: test_time("20260610", "000700"),
            anchor_utc: test_time("20260610", "000200"),
            pad_scans: 0,
            extra_start_scans: 0,
            extra_end_scans: 0,
            max_objects: 10,
        };

        assert_eq!(
            level2_window_listing_dates(&request),
            vec![NaiveDate::from_ymd_opt(2026, 6, 10).unwrap()]
        );

        request.extra_start_scans = 1;
        assert_eq!(
            level2_window_listing_dates(&request),
            vec![
                NaiveDate::from_ymd_opt(2026, 6, 9).unwrap(),
                NaiveDate::from_ymd_opt(2026, 6, 10).unwrap(),
            ]
        );

        request.extra_start_scans = 0;
        request.extra_end_scans = 1;
        assert_eq!(
            level2_window_listing_dates(&request),
            vec![
                NaiveDate::from_ymd_opt(2026, 6, 10).unwrap(),
                NaiveDate::from_ymd_opt(2026, 6, 11).unwrap(),
            ]
        );

        request.extra_end_scans = 0;
        request.pad_scans = 1;
        assert_eq!(
            level2_window_listing_dates(&request),
            vec![
                NaiveDate::from_ymd_opt(2026, 6, 9).unwrap(),
                NaiveDate::from_ymd_opt(2026, 6, 10).unwrap(),
                NaiveDate::from_ymd_opt(2026, 6, 11).unwrap(),
            ]
        );
    }

    #[test]
    fn fallback_has_many_sites() {
        assert!(fallback_sites().len() > 150);
    }

    #[test]
    fn newest_cached_level2_path_ignores_partial_empty_and_mdm_files() {
        let dir = std::env::temp_dir().join(format!("bowecho-cache-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("test cache dir");

        fs::write(dir.join("KTLX20260607_180000_V06"), b"old").expect("old cache file");
        fs::write(dir.join("KTLX20260607_181000_V06.download"), b"partial")
            .expect("partial cache file");
        fs::write(dir.join("KTLX20260607_182000_MDM"), b"mdm").expect("mdm cache file");
        fs::write(dir.join("KTLX20260607_183000_V06"), []).expect("empty cache file");
        fs::write(dir.join("KTLX20260607_184000_V06"), b"new").expect("new cache file");

        let newest = newest_cached_level2_path(&dir)
            .expect("cache scan")
            .expect("newest cache file");

        assert_eq!(
            newest.file_name().and_then(|name| name.to_str()),
            Some("KTLX20260607_184000_V06")
        );

        fs::remove_dir_all(&dir).expect("clean test cache dir");
    }

    #[test]
    fn realtime_latest_volume_id_handles_wraparound_window() {
        let wrapped_ids = [998, 999, 1, 2, 3];
        assert_eq!(
            latest_realtime_volume_id_from_active_ids(&wrapped_ids),
            Some(3)
        );

        let contiguous_ids = (102..=628).collect::<Vec<_>>();
        assert_eq!(
            latest_realtime_volume_id_from_active_ids(&contiguous_ids),
            Some(628)
        );
    }

    #[test]
    fn realtime_volume_candidates_include_each_active_run_end() {
        let wrapped_ids = [998, 999, 1, 2, 3];
        assert_eq!(
            realtime_volume_candidate_ids_from_active_ids(&wrapped_ids),
            vec![3, 999]
        );

        let kama_like_split_ids = [1, 2, 3, 73, 74, 75, 205, 206, 559];
        assert_eq!(
            realtime_volume_candidate_ids_from_active_ids(&kama_like_split_ids),
            vec![3, 75, 206, 559]
        );

        let contiguous_ids = (102..=628).collect::<Vec<_>>();
        assert_eq!(
            realtime_volume_candidate_ids_from_active_ids(&contiguous_ids),
            vec![628]
        );
    }

    #[test]
    fn realtime_chunk_scan_chooses_newer_volume_time_over_higher_stale_id() {
        let chunks = [
            "KLNX/423/20260629-112400-001-S",
            "KLNX/423/20260629-112400-016-E",
            "KLNX/393/20260701-050218-001-S",
            "KLNX/393/20260701-050218-013-I",
        ]
        .into_iter()
        .map(|key| {
            parse_realtime_chunk_object(S3Object {
                key: key.to_owned(),
                size: 100,
                last_modified: None,
            })
            .expect("valid chunk key")
        })
        .collect::<Vec<_>>();

        let volume =
            latest_realtime_volume_from_chunks("KLNX", chunks).expect("newest volume selected");

        assert_eq!(volume.volume_id, 393);
        assert_eq!(volume.volume_time.to_rfc3339(), "2026-07-01T05:02:18+00:00");
        assert!(!volume.complete);
        assert_eq!(volume.chunks.len(), 2);
    }

    #[test]
    fn realtime_chunk_scan_does_not_mix_reused_volume_ids() {
        let chunks = [
            "KLNX/299/20260701-050218-001-S",
            "KLNX/299/20260628-235652-055-E",
        ]
        .into_iter()
        .map(|key| {
            parse_realtime_chunk_object(S3Object {
                key: key.to_owned(),
                size: 100,
                last_modified: None,
            })
            .expect("valid chunk key")
        })
        .collect::<Vec<_>>();

        let volume =
            latest_realtime_volume_from_chunks("KLNX", chunks).expect("newest volume selected");

        assert_eq!(volume.volume_id, 299);
        assert_eq!(volume.volume_time.to_rfc3339(), "2026-07-01T05:02:18+00:00");
        assert_eq!(volume.chunks.len(), 1);
        assert!(!volume.complete);
    }

    #[test]
    #[ignore = "network: hits live Unidata realtime chunk bucket"]
    fn latest_realtime_level2_volume_live_klnx() {
        let volume =
            latest_realtime_level2_volume_with_listing_ttl("KLNX", StdDuration::ZERO).unwrap();
        println!(
            "KLNX realtime id={} time={} chunks={} complete={}",
            volume.volume_id,
            volume.volume_time,
            volume.chunks.len(),
            volume.complete
        );
        assert!(
            Utc::now()
                .signed_duration_since(volume.volume_time)
                .num_minutes()
                < 20,
            "resolver picked stale KLNX chunk volume {volume:?}"
        );
    }

    #[test]
    fn realtime_chunk_key_parser_extracts_volume_metadata() {
        let chunk = parse_realtime_chunk_object(S3Object {
            key: "KGGW/628/20260608-002828-025-I".to_owned(),
            size: 129_481,
            last_modified: None,
        })
        .expect("valid realtime chunk key");

        assert_eq!(chunk.site, "KGGW");
        assert_eq!(chunk.volume_id, 628);
        assert_eq!(chunk.chunk_id, 25);
        assert_eq!(chunk.chunk_type, RealtimeChunkType::Intermediate);
        assert_eq!(chunk.volume_time.to_rfc3339(), "2026-06-08T00:28:28+00:00");
    }

    #[test]
    fn s3_last_modified_parser_handles_aws_timestamp() {
        let parsed =
            parse_s3_last_modified("2026-06-08T22:23:33.000Z").expect("S3 LastModified parses");

        assert_eq!(parsed.to_rfc3339(), "2026-06-08T22:23:33+00:00");
    }

    #[test]
    fn realtime_chunk_prefix_size_accepts_only_chunk_boundaries() {
        let volume = test_realtime_volume_with_sizes(&[4, 6, 10]);

        assert_eq!(chunk_prefix_count_for_size(&volume, 0), Some(0));
        assert_eq!(chunk_prefix_count_for_size(&volume, 4), Some(1));
        assert_eq!(chunk_prefix_count_for_size(&volume, 10), Some(2));
        assert_eq!(chunk_prefix_count_for_size(&volume, 20), Some(3));
        assert_eq!(chunk_prefix_count_for_size(&volume, 5), None);
        assert_eq!(chunk_prefix_count_for_size(&volume, 21), None);
    }

    #[test]
    fn realtime_append_adds_only_missing_chunk_bytes() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "radar-rs-append-test-{}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("test dir");

        let assembled = dir.join("assembled");
        let chunk_two = dir.join("002-I");
        let chunk_three = dir.join("003-E");
        fs::write(&assembled, b"aaaa").expect("existing prefix");
        fs::write(&chunk_two, b"bb").expect("chunk two");
        fs::write(&chunk_three, b"cccc").expect("chunk three");

        append_realtime_chunks(
            &assembled,
            &[chunk_two, chunk_three],
            4,
            10,
            "test://chunks",
        )
        .expect("append missing chunks");

        assert_eq!(
            fs::read(&assembled).expect("assembled bytes"),
            b"aaaabbcccc"
        );
        fs::remove_dir_all(&dir).expect("clean append test dir");
    }

    fn test_realtime_volume_with_sizes(sizes: &[u64]) -> RealtimeLevel2Volume {
        let volume_time = Utc.with_ymd_and_hms(2026, 6, 8, 0, 0, 0).unwrap();
        let chunks = sizes
            .iter()
            .enumerate()
            .map(|(index, size)| {
                let chunk_id = u16::try_from(index + 1).expect("test chunk id");
                let chunk_type = if index == 0 {
                    RealtimeChunkType::Start
                } else if index + 1 == sizes.len() {
                    RealtimeChunkType::End
                } else {
                    RealtimeChunkType::Intermediate
                };
                RealtimeChunkObject {
                    object: S3Object {
                        key: format!("KTLX/1/20260608-000000-{chunk_id:03}-I"),
                        size: *size,
                        last_modified: None,
                    },
                    site: "KTLX".to_owned(),
                    volume_id: 1,
                    volume_time,
                    chunk_id,
                    chunk_type,
                }
            })
            .collect::<Vec<_>>();
        RealtimeLevel2Volume {
            site: "KTLX".to_owned(),
            volume_id: 1,
            volume_time,
            total_size: sizes.iter().sum(),
            complete: chunks.last().is_some_and(|chunk| chunk.chunk_type.is_end()),
            chunks,
        }
    }

    fn test_level2_objects(site: &str, times: &[&str]) -> Vec<S3Object> {
        times
            .iter()
            .map(|time| test_level2_object(site, "20260609", time))
            .collect()
    }

    fn test_level2_object(site: &str, date: &str, time: &str) -> S3Object {
        S3Object {
            key: format!(
                "{}/{}/{}/{}/{}{}_{}_V06",
                &date[0..4],
                &date[4..6],
                &date[6..8],
                site,
                site,
                date,
                time
            ),
            size: 100,
            last_modified: None,
        }
    }

    fn test_time(date: &str, time: &str) -> DateTime<Utc> {
        let naive = NaiveDate::parse_from_str(date, "%Y%m%d")
            .expect("test date")
            .and_hms_opt(
                time[0..2].parse().expect("hour"),
                time[2..4].parse().expect("minute"),
                time[4..6].parse().expect("second"),
            )
            .expect("test time");
        DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc)
    }
}
