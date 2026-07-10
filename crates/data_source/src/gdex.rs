//! GDEX (NSF NCAR GDEX / THREDDS) data-source core — Stage 1a plumbing.
//!
//! GDEX's web "Data Access" pages are a UI over a THREDDS Data Server (TDS).
//! The machine API is the TDS catalog: every `catalog.html` has a
//! `catalog.xml` twin. This module provides the tested, UI-free plumbing:
//!
//! 1. **Catalog crawl** — recursively fetch `catalog.xml`
//!    ([`fetch_and_parse_catalog`] for one level; [`crawl_dataset`] for a full,
//!    disk-cached crawl). `<catalogRef xlink:href>` = subdir (resolved relative
//!    and recursed), `<dataset urlPath>` = a leaf file (kept when the urlPath
//!    ends with a data extension, dropping the stray scan `dump`).
//! 2. **NCSS metadata + subset URL** — [`fetch_ncss_dataset`] parses the grid
//!    `dataset.xml` (variables, lat/lon box, time span); [`ncss_subset_url`]
//!    builds a subset request. NCSS grid rejects `accept=netcdf4` (HTTP 400) —
//!    this module always requests `accept=netcdf` (classic NetCDF-3, which
//!    `netcrust` reads).
//! 3. **Resumable download** — [`download_to_path`] streams a URL to a
//!    `.download` temp with HTTP `Range` resume, verifies the final size, and
//!    atomically renames. Local paths sanitize `:` -> `_` (the leaf filenames
//!    carry `:` which is illegal on NTFS).
//! 4. **GDEX-strength retry** — the server 503s and returns empty 200 bodies
//!    under load; every catalog / NCSS / download-start request retries 5xx and
//!    empty bodies with backoff.
//!
//! ## Ingest handoff (Stage 1b seam)
//!
//! This module lives in `data_source`, which `app_ui` depends on — so it must
//! not reach back into `app_ui` (that would be a dependency cycle). The
//! download therefore stops at returning a local path
//! ([`DownloadOutcome::path`]). Stage 1b, inside `app_ui`, feeds that path to
//! the existing, unchanged importer:
//!
//! ```ignore
//! let outcome = data_source::gdex::download_leaf(&leaf, &cache_dir)?;
//! let task = crate::local_import::spawn_import_paths(vec![outcome.path], store_root);
//! ```
//!
//! A downloaded CONUS II `.nc` (wrf2d surface or wrf3d TK/Z/P) already ingests
//! through `local_import` with zero changes (recon-confirmed).

use std::collections::HashSet;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration as StdDuration;

use chrono::Utc;
use reqwest::header::{CONTENT_LENGTH, RANGE};
use serde::{Deserialize, Serialize};

use crate::{DataSourceError, Result};

/// TDS catalog base (a dataset's crawl entry point lives under here).
const CATALOG_BASE: &str = "https://tds.gdex.ucar.edu/thredds/catalog/";
/// Direct-download service base (`HTTPServer`). `download_url = FILESERVER_BASE + urlPath`.
const FILESERVER_BASE: &str = "https://tds.gdex.ucar.edu/thredds/fileServer/";
/// NetCDF Subset Service grid base. `ncss_url = NCSS_GRID_BASE + urlPath`.
const NCSS_GRID_BASE: &str = "https://tds.gdex.ucar.edu/thredds/ncss/grid/";

/// Leaf extensions kept by the crawl (drops the scan-root `dump` entry).
const DATA_EXTENSIONS: &[&str] = &[".nc", ".grb", ".grib", ".grb2"];

/// Crawl concurrency cap — the server is flaky; be polite (doc §5).
const GDEX_CRAWL_CONCURRENCY: usize = 4;

/// Backoff schedule for the GDEX retry wrapper. `len + 1` total attempts (6):
/// one initial try plus five retries at 2s, 3s, 6s, 6s, 6s.
const GDEX_RETRY_BACKOFFS: &[StdDuration] = &[
    StdDuration::from_secs(2),
    StdDuration::from_secs(3),
    StdDuration::from_secs(6),
    StdDuration::from_secs(6),
    StdDuration::from_secs(6),
];

// ---------------------------------------------------------------------------
// Public data types
// ---------------------------------------------------------------------------

/// One physical file discovered by the catalog crawl.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Leaf {
    /// Display name (the leaf `<dataset name>`, e.g. `wrf2d_d01_2080-01-01_00:00:00.nc`).
    pub name: String,
    /// THREDDS `urlPath` (e.g. `files/g/d612005/future2D/208001/...nc`).
    pub url_path: String,
    /// Full direct-download URL (`fileServer` + urlPath).
    pub download_url: String,
    /// NCSS grid base URL (append `/dataset.xml` for metadata; feed to
    /// [`ncss_subset_url`] for a subset).
    pub ncss_url: String,
    /// File size in bytes from `<dataSize>` when the catalog advertised it
    /// (decimal units: Mbytes = 1e6). `None` if absent.
    pub size_bytes: Option<u64>,
    /// Modification timestamp from `<date type="modified">` when present.
    pub date: Option<String>,
}

/// One level of a crawl: the child catalogs to recurse into, plus the leaves
/// found at this level. Stage 1b can drive lazy per-node tree expansion with
/// this directly instead of a full up-front [`crawl_dataset`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedCatalog {
    /// The catalog URL this was parsed from.
    pub catalog_url: String,
    /// Resolved absolute URLs of child `catalog.xml` documents (TDS-internal
    /// only; external `<catalogRef>` web links are filtered out).
    pub child_catalog_urls: Vec<String>,
    /// Data-file leaves discovered at this level.
    pub leaves: Vec<Leaf>,
}

/// A full-crawl result, serialized to the on-disk crawl cache.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogCache {
    /// Dataset id (e.g. `d612005`).
    pub dataset_id: String,
    /// RFC-3339 timestamp of the crawl.
    pub crawled_at: String,
    /// Every data-file leaf under the dataset.
    pub leaves: Vec<Leaf>,
}

/// A variable advertised by an NCSS grid `dataset.xml`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NcssVariable {
    /// Variable / grid name (e.g. `T2`).
    pub name: String,
    /// Units string when present (e.g. `K`).
    pub units: Option<String>,
    /// Human description when present (e.g. `TEMP at 2 M`).
    pub description: Option<String>,
}

/// Geographic bounding box (degrees) as advertised by NCSS `<LatLonBox>` and
/// consumed by [`NcssSubset::bbox`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LatLonBox {
    /// Northern edge (degrees north).
    pub north: f64,
    /// Southern edge (degrees north).
    pub south: f64,
    /// Eastern edge (degrees east).
    pub east: f64,
    /// Western edge (degrees east).
    pub west: f64,
}

/// Time coverage from NCSS `<TimeSpan>` (ISO-8601 strings, verbatim).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimeSpan {
    /// Start time.
    pub begin: String,
    /// End time.
    pub end: String,
}

/// Parsed NCSS grid metadata, enough to drive a subset UI.
#[derive(Clone, Debug, PartialEq)]
pub struct NcssGridDataset {
    /// Data variables (grids).
    pub variables: Vec<NcssVariable>,
    /// Full-grid lat/lon box when advertised.
    pub lat_lon_box: Option<LatLonBox>,
    /// Time coverage when advertised.
    pub time_span: Option<TimeSpan>,
}

/// An NCSS subset request. Spatial and temporal fields are optional; omitting
/// them requests the full grid / all times.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NcssSubset {
    /// Variables to include (each becomes a `var=` parameter).
    pub vars: Vec<String>,
    /// Optional spatial bounding box.
    pub bbox: Option<LatLonBox>,
    /// A single time (`time=`); takes precedence over the range fields.
    pub time: Option<String>,
    /// Range start (`time_start=`), used only when [`NcssSubset::time`] is `None`.
    pub time_start: Option<String>,
    /// Range end (`time_end=`), used only when [`NcssSubset::time`] is `None`.
    pub time_end: Option<String>,
}

/// Result of a [`download_to_path`] call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadOutcome {
    /// The final local path (temp renamed into place).
    pub path: PathBuf,
    /// Bytes on disk after the download.
    pub bytes: u64,
    /// True when an existing `.download` temp was resumed via `Range`.
    pub resumed: bool,
    /// True when the file already existed at the expected size (no transfer).
    pub cache_hit: bool,
}

// ---------------------------------------------------------------------------
// Dataset registry
// ---------------------------------------------------------------------------

/// One GDEX dataset the in-app browser can open. The registry is the single
/// source of truth for dataset ids and their user-facing text — the browser's
/// picker rows, tree-root label, and attribution header all render from here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GdexDataset {
    /// GDEX/RDA dataset id (e.g. `d612005`) — the THREDDS scan root lives at
    /// `files/g/<id>/` (see [`dataset_catalog_url`]).
    pub id: &'static str,
    /// Short display name (picker rows and the browser's tree root).
    pub label: &'static str,
    /// One-line description (picker hover text).
    pub blurb: &'static str,
    /// What the dataset's leaves are ("NetCDF" / "GRIB1") — a UI hint only;
    /// the import gate decides per file.
    pub format: &'static str,
    /// Attribution line shown in the browser's header.
    pub attribution: &'static str,
}

impl GdexDataset {
    /// The dataset's crawl entry-point catalog URL.
    pub fn catalog_url(&self) -> String {
        dataset_catalog_url(self.id)
    }
}

/// The datasets offered by the browser's picker. The FIRST entry is the
/// default (CONUS II — the dataset the pre-picker browser was hardwired to;
/// its downloads stay flat in the cache root for that reason).
///
/// Both ids verified against the live TDS on 2026-07-09:
/// `https://tds.gdex.ucar.edu/thredds/catalog/files/g/<id>/catalog.xml`.
/// ERA-20C (`d626000`) answers with `dataFormat: GRIB-1`, Rights "Freely
/// Available", and nine `e20c.oper.*` subtrees.
pub const GDEX_DATASETS: &[GdexDataset] = &[
    GdexDataset {
        id: "d612005",
        label: "CONUS II (d612005)",
        blurb: "NSF NCAR CONUS II regional climate WRF — present + future periods, NetCDF",
        format: "NetCDF",
        attribution: "NSF NCAR GDEX · CONUS II regional climate WRF (present + future) · \
                      CC-BY 4.0 · DOI 10.5065/49SN-8E08",
    },
    GdexDataset {
        id: "d626000",
        label: "ERA-20C (d626000)",
        blurb: "ECMWF ERA-20C — 20th-century reanalysis 1900-2010, GRIB1",
        format: "GRIB1",
        attribution: "NSF NCAR GDEX · ECMWF ERA-20C 20th-century reanalysis (1900-2010) · \
                      GRIB1 · rda.ucar.edu/datasets/d626000",
    },
];

/// Look a registry dataset up by id.
pub fn dataset_by_id(id: &str) -> Option<&'static GdexDataset> {
    GDEX_DATASETS.iter().find(|dataset| dataset.id == id)
}

/// The dataset id embedded in a THREDDS `urlPath` (`files/g/<id>/...`), when
/// the path has that shape. Lets download destinations be derived from the
/// LEAF being downloaded rather than from whatever dataset the picker shows.
pub fn dataset_id_from_url_path(url_path: &str) -> Option<&str> {
    let rest = url_path.strip_prefix("files/g/")?;
    let id = rest.split('/').next()?;
    if id.is_empty() { None } else { Some(id) }
}

// ---------------------------------------------------------------------------
// URL builders
// ---------------------------------------------------------------------------

/// The crawl entry-point catalog URL for a dataset id (e.g. `d612005`).
pub fn dataset_catalog_url(dataset_id: &str) -> String {
    format!("{CATALOG_BASE}files/g/{dataset_id}/catalog.xml")
}

/// The NCSS grid metadata URL (`.../ncss/grid/<urlPath>/dataset.xml`).
pub fn ncss_dataset_url(url_path: &str) -> String {
    format!("{NCSS_GRID_BASE}{url_path}/dataset.xml")
}

/// Build the NCSS subset request URL. Always ends with `accept=netcdf`
/// (classic NetCDF-3 — the grid endpoint rejects `netcdf4`). Parameters are
/// emitted in a fixed order (vars, bbox, time, accept) for deterministic URLs.
pub fn ncss_subset_url(url_path: &str, subset: &NcssSubset) -> String {
    let mut params: Vec<(&str, String)> = Vec::new();
    for var in &subset.vars {
        params.push(("var", var.clone()));
    }
    if let Some(bbox) = &subset.bbox {
        params.push(("north", fmt_coord(bbox.north)));
        params.push(("south", fmt_coord(bbox.south)));
        params.push(("east", fmt_coord(bbox.east)));
        params.push(("west", fmt_coord(bbox.west)));
    }
    if let Some(time) = &subset.time {
        params.push(("time", time.clone()));
    } else {
        if let Some(start) = &subset.time_start {
            params.push(("time_start", start.clone()));
        }
        if let Some(end) = &subset.time_end {
            params.push(("time_end", end.clone()));
        }
    }
    params.push(("accept", "netcdf".to_owned()));

    let query = params
        .iter()
        .map(|(key, value)| format!("{key}={}", encode_query_value(value)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{NCSS_GRID_BASE}{url_path}?{query}")
}

/// Format a bbox coordinate without a trailing `.0` (server accepts both;
/// this keeps URLs tidy and tests deterministic).
fn fmt_coord(value: f64) -> String {
    if value.fract() == 0.0 && value.is_finite() {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// Percent-encode a query-parameter value (RFC-3986 unreserved passes through).
fn encode_query_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Retry wrapper (5xx / empty-200-body -> retry with backoff)
// ---------------------------------------------------------------------------

/// Classification of an HTTP response for the GDEX retry policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResponseClass {
    /// 2xx with a non-empty body — use it.
    Accept,
    /// 5xx, or an empty 2xx body — transient, retry with backoff.
    Retry,
    /// A definite answer that is not success (4xx, redirects we won't follow).
    Fatal,
}

/// The heart of the GDEX retry policy: 5xx and empty 2xx bodies are transient.
fn classify_response(status: u16, body_empty: bool) -> ResponseClass {
    if (500..600).contains(&status) {
        ResponseClass::Retry
    } else if !(200..300).contains(&status) {
        ResponseClass::Fatal
    } else if body_empty {
        ResponseClass::Retry
    } else {
        ResponseClass::Accept
    }
}

/// One attempt's outcome for [`with_retry`].
enum Attempt<T> {
    /// Success — stop and return the value.
    Accept(T),
    /// Transient failure — sleep (if attempts remain) and try again.
    Retry(DataSourceError),
    /// Permanent failure — stop and return the error immediately.
    Fatal(DataSourceError),
}

/// Drive `attempt` up to `backoffs.len() + 1` times, sleeping the matching
/// backoff between transient retries. Returns the first `Accept`, any `Fatal`,
/// or the last `Retry` error once attempts are exhausted.
fn with_retry<T>(
    backoffs: &[StdDuration],
    mut attempt: impl FnMut(usize) -> Attempt<T>,
) -> Result<T> {
    let mut last: Option<DataSourceError> = None;
    for index in 0..=backoffs.len() {
        match attempt(index) {
            Attempt::Accept(value) => return Ok(value),
            Attempt::Fatal(err) => return Err(err),
            Attempt::Retry(err) => {
                last = Some(err);
                if index < backoffs.len() && !backoffs[index].is_zero() {
                    thread::sleep(backoffs[index]);
                }
            }
        }
    }
    Err(last.unwrap_or_else(|| gdex_error("gdex request exhausted retries")))
}

/// A GDEX protocol error carried on the shared [`DataSourceError::Io`] variant
/// (no new enum variant needed for this module).
fn gdex_error(message: impl Into<String>) -> DataSourceError {
    DataSourceError::Io(io::Error::other(message.into()))
}

/// Fetch a text resource (catalog.xml / dataset.xml) with the GDEX retry
/// policy on the long-timeout download client (these documents run to
/// hundreds of KB).
fn gdex_fetch_text(url: &str) -> Result<String> {
    with_retry(GDEX_RETRY_BACKOFFS, |_| {
        let client = crate::download_http_client();
        match client.get(url).send() {
            Err(err) => Attempt::Retry(DataSourceError::Http(err)),
            Ok(response) => {
                let status = response.status().as_u16();
                match response.text() {
                    Err(err) => Attempt::Retry(DataSourceError::Http(err)),
                    Ok(body) => match classify_response(status, body.trim().is_empty()) {
                        ResponseClass::Accept => Attempt::Accept(body),
                        ResponseClass::Retry => Attempt::Retry(gdex_error(format!(
                            "gdex {url}: status {status} or empty body"
                        ))),
                        ResponseClass::Fatal => {
                            Attempt::Fatal(gdex_error(format!("gdex {url}: status {status}")))
                        }
                    },
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Catalog crawl
// ---------------------------------------------------------------------------

/// Fetch and parse a single catalog level.
pub fn fetch_and_parse_catalog(catalog_url: &str) -> Result<ParsedCatalog> {
    let xml = gdex_fetch_text(catalog_url)?;
    parse_catalog(&xml, catalog_url)
}

/// Parse catalog XML already in hand (the offline-testable core of
/// [`fetch_and_parse_catalog`]).
fn parse_catalog(xml: &str, catalog_url: &str) -> Result<ParsedCatalog> {
    let catalog: CatalogXml = quick_xml::de::from_str(xml)?;
    let mut leaves = Vec::new();
    let mut child_urls = Vec::new();
    for cref in &catalog.catalog_refs {
        push_child_catalog(cref, catalog_url, &mut child_urls);
    }
    for dataset in &catalog.datasets {
        collect_from_dataset(dataset, catalog_url, &mut leaves, &mut child_urls);
    }
    child_urls.sort();
    child_urls.dedup();
    Ok(ParsedCatalog {
        catalog_url: catalog_url.to_owned(),
        child_catalog_urls: child_urls,
        leaves,
    })
}

/// Walk one `<dataset>` subtree, collecting leaves and child catalog URLs.
fn collect_from_dataset(
    dataset: &DatasetXml,
    catalog_url: &str,
    leaves: &mut Vec<Leaf>,
    child_urls: &mut Vec<String>,
) {
    for cref in &dataset.catalog_refs {
        push_child_catalog(cref, catalog_url, child_urls);
    }
    if let Some(url_path) = &dataset.url_path
        && has_data_extension(url_path)
    {
        leaves.push(make_leaf(dataset, url_path));
    }
    for nested in &dataset.datasets {
        collect_from_dataset(nested, catalog_url, leaves, child_urls);
    }
}

/// Resolve a `<catalogRef>` href and keep it only if it is a TDS-internal
/// catalog (external web links are dropped).
fn push_child_catalog(cref: &CatalogRefXml, catalog_url: &str, child_urls: &mut Vec<String>) {
    let Some(href) = &cref.href else {
        return;
    };
    let resolved = resolve_relative(catalog_url, href);
    if is_tds_catalog_url(&resolved) {
        child_urls.push(resolved);
    }
}

/// Build a [`Leaf`] from a leaf `<dataset>` and its `urlPath`.
fn make_leaf(dataset: &DatasetXml, url_path: &str) -> Leaf {
    let name = if dataset.name.trim().is_empty() {
        url_path.rsplit('/').next().unwrap_or(url_path).to_owned()
    } else {
        dataset.name.clone()
    };
    Leaf {
        name,
        url_path: url_path.to_owned(),
        download_url: format!("{FILESERVER_BASE}{url_path}"),
        ncss_url: format!("{NCSS_GRID_BASE}{url_path}"),
        size_bytes: dataset.data_size.as_ref().and_then(data_size_to_bytes),
        date: dataset
            .dates
            .iter()
            .find_map(|date| date.value.as_ref().map(|value| value.trim().to_owned())),
    }
}

/// `true` when `url_path` ends with a whitelisted data extension.
fn has_data_extension(url_path: &str) -> bool {
    let lower = url_path.to_ascii_lowercase();
    DATA_EXTENSIONS.iter().any(|ext| lower.ends_with(ext))
}

/// `true` when a resolved catalogRef points back into this TDS's catalog tree.
fn is_tds_catalog_url(url: &str) -> bool {
    url.starts_with(CATALOG_BASE)
}

/// Convert a `<dataSize units="...">` into bytes (decimal SI units).
fn data_size_to_bytes(data_size: &DataSizeXml) -> Option<u64> {
    let value: f64 = data_size.value.as_ref()?.trim().parse().ok()?;
    let factor = match data_size
        .units
        .as_deref()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("bytes") => 1.0,
        Some("kbytes") => 1_000.0,
        Some("mbytes") => 1_000_000.0,
        Some("gbytes") => 1_000_000_000.0,
        Some("tbytes") => 1_000_000_000_000.0,
        _ => 1.0,
    };
    Some((value * factor) as u64)
}

/// Resolve `href` against the current catalog URL (RFC-3986-ish: absolute and
/// scheme-relative pass through; otherwise join to the base directory and
/// collapse `.`/`..`).
fn resolve_relative(base_url: &str, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_owned();
    }
    let (scheme_authority, base_path) = split_scheme_authority(base_url);
    if let Some(rest) = href.strip_prefix("//") {
        let scheme = base_url.split("://").next().unwrap_or("https");
        return format!("{scheme}://{rest}");
    }
    let joined = if href.starts_with('/') {
        href.to_owned()
    } else {
        let base_dir = match base_path.rfind('/') {
            Some(index) => &base_path[..=index],
            None => "/",
        };
        format!("{base_dir}{href}")
    };
    format!("{scheme_authority}{}", normalize_path(&joined))
}

/// Split `https://host/path` into (`https://host`, `/path`).
fn split_scheme_authority(url: &str) -> (String, String) {
    if let Some(scheme_end) = url.find("://") {
        let after = scheme_end + 3;
        if let Some(slash) = url[after..].find('/') {
            let authority_end = after + slash;
            (
                url[..authority_end].to_owned(),
                url[authority_end..].to_owned(),
            )
        } else {
            (url.to_owned(), "/".to_owned())
        }
    } else {
        (String::new(), url.to_owned())
    }
}

/// Collapse `.` and `..` segments in an absolute path.
fn normalize_path(path: &str) -> String {
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    let mut out = String::from("/");
    out.push_str(&segments.join("/"));
    out
}

/// Recursively crawl a dataset and cache the flat leaf list to disk as JSON.
/// With `refresh == false` a valid cache is returned without touching the
/// network; `refresh == true` always re-crawls and rewrites the cache.
pub fn crawl_dataset(dataset_id: &str, cache_dir: &Path, refresh: bool) -> Result<CatalogCache> {
    let cache_path = catalog_cache_path(cache_dir, dataset_id);
    if !refresh && let Some(cached) = read_catalog_cache(&cache_path)? {
        return Ok(cached);
    }
    let leaves = crawl_from(&dataset_catalog_url(dataset_id))?;
    let cache = CatalogCache {
        dataset_id: dataset_id.to_owned(),
        crawled_at: Utc::now().to_rfc3339(),
        leaves,
    };
    write_catalog_cache(&cache_path, &cache)?;
    Ok(cache)
}

/// Breadth-first crawl from a starting catalog URL, capping concurrency and
/// guarding against revisits.
fn crawl_from(start_url: &str) -> Result<Vec<Leaf>> {
    let mut frontier = vec![start_url.to_owned()];
    let mut visited: HashSet<String> = HashSet::new();
    visited.insert(start_url.to_owned());
    let mut all_leaves = Vec::new();

    while !frontier.is_empty() {
        let parsed: Mutex<Vec<ParsedCatalog>> = Mutex::new(Vec::new());
        crate::for_each_concurrent(&frontier, GDEX_CRAWL_CONCURRENCY, |url| {
            let level = fetch_and_parse_catalog(url)?;
            if let Ok(mut guard) = parsed.lock() {
                guard.push(level);
            }
            Ok(())
        })?;

        let mut next = Vec::new();
        for level in parsed.into_inner().unwrap_or_default() {
            all_leaves.extend(level.leaves);
            for child in level.child_catalog_urls {
                if visited.insert(child.clone()) {
                    next.push(child);
                }
            }
        }
        frontier = next;
    }
    Ok(all_leaves)
}

/// Path of the on-disk crawl cache for a dataset.
fn catalog_cache_path(cache_dir: &Path, dataset_id: &str) -> PathBuf {
    cache_dir.join(format!("gdex_catalog_{dataset_id}.json"))
}

/// Read a crawl cache. Missing file -> `Ok(None)`; a corrupt file is treated
/// as a miss (`Ok(None)`) so a bad cache never wedges the crawl.
fn read_catalog_cache(path: &Path) -> Result<Option<CatalogCache>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(serde_json::from_str(&text).ok()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

/// Write a crawl cache as pretty JSON (creating the cache dir if needed).
fn write_catalog_cache(path: &Path, cache: &CatalogCache) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(cache)?;
    fs::write(path, json)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// NCSS grid metadata
// ---------------------------------------------------------------------------

/// Fetch and parse an NCSS grid `dataset.xml` for a leaf's `urlPath`.
pub fn fetch_ncss_dataset(url_path: &str) -> Result<NcssGridDataset> {
    let xml = gdex_fetch_text(&ncss_dataset_url(url_path))?;
    parse_ncss_dataset(&xml)
}

/// Parse NCSS grid `dataset.xml` already in hand (offline-testable core).
fn parse_ncss_dataset(xml: &str) -> Result<NcssGridDataset> {
    let parsed: GridDatasetXml = quick_xml::de::from_str(xml)?;
    let mut variables: Vec<NcssVariable> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for grid_set in &parsed.grid_sets {
        for grid in &grid_set.grids {
            if !seen.insert(grid.name.clone()) {
                continue;
            }
            let units = grid.find_attr("units");
            let description = grid
                .desc
                .clone()
                .filter(|value| !value.trim().is_empty())
                .or_else(|| grid.find_attr("description"));
            variables.push(NcssVariable {
                name: grid.name.clone(),
                units,
                description,
            });
        }
    }
    Ok(NcssGridDataset {
        variables,
        lat_lon_box: parsed.lat_lon_box.map(|box_| LatLonBox {
            north: box_.north,
            south: box_.south,
            east: box_.east,
            west: box_.west,
        }),
        time_span: parsed.time_span.map(|span| TimeSpan {
            begin: span.begin,
            end: span.end,
        }),
    })
}

// ---------------------------------------------------------------------------
// Streaming resumable download
// ---------------------------------------------------------------------------

/// Sanitize a leaf filename for the local filesystem: `:` (and the other
/// Windows-reserved characters) become `_`. The GDEX leaves carry `:` in the
/// timestamp (`..._00:00:00.nc`), which is illegal on NTFS.
pub fn sanitize_leaf_filename(name: &str) -> String {
    name.chars()
        .map(|ch| match ch {
            ':' | '<' | '>' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            other if (other as u32) < 0x20 => '_',
            other => other,
        })
        .collect()
}

/// The local download path for a leaf under `cache_dir` (with the filename
/// sanitized for NTFS).
pub fn local_path_for_leaf(cache_dir: &Path, leaf: &Leaf) -> PathBuf {
    cache_dir.join(sanitize_leaf_filename(&leaf.name))
}

/// Download a leaf to `cache_dir`, choosing the local path from its (sanitized)
/// name. The returned [`DownloadOutcome::path`] is the ingest seam for Stage 1b.
pub fn download_leaf(leaf: &Leaf, cache_dir: &Path) -> Result<DownloadOutcome> {
    let dest = local_path_for_leaf(cache_dir, leaf);
    download_to_path(&leaf.download_url, &dest)
}

/// Chunk size for the cancellable copy loop — large enough that syscall
/// overhead is negligible on a multi-GB stream, small enough that a cancel
/// lands within a fraction of a second at typical GDEX throughput.
const COPY_CHUNK_BYTES: usize = 64 * 1024;

/// How [`copy_with_cancel`] ended. Both ends carry the bytes written (and
/// flushed) to the writer during this call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CopyEnd {
    /// The reader hit EOF — the copy ran to completion.
    Complete(u64),
    /// The cancel flag was observed between chunks — the copy stopped early.
    /// Everything read so far was written and flushed; the writer is left
    /// intact (the resume contract depends on the partial temp surviving).
    Cancelled(u64),
}

/// `io::copy` with a cooperative cancel check between chunks. Flushes the
/// writer on both exits (completion and cancel) so a partial temp holds every
/// byte it claims; never truncates or deletes anything.
fn copy_with_cancel(
    reader: &mut impl Read,
    writer: &mut impl Write,
    cancel: &AtomicBool,
) -> io::Result<CopyEnd> {
    let mut buf = vec![0u8; COPY_CHUNK_BYTES];
    let mut written: u64 = 0;
    loop {
        if cancel.load(Ordering::Relaxed) {
            writer.flush()?;
            return Ok(CopyEnd::Cancelled(written));
        }
        let n = match reader.read(&mut buf) {
            Ok(0) => {
                writer.flush()?;
                return Ok(CopyEnd::Complete(written));
            }
            Ok(n) => n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        writer.write_all(&buf[..n])?;
        written += n as u64;
    }
}

/// Stream `url` to `dest`, resuming an interrupted `.download` temp via HTTP
/// `Range` when the server supports it. Verifies the final size against the
/// server's `Content-Length` (when advertised) and atomically renames the temp
/// into place. Never buffers the body in memory.
pub fn download_to_path(url: &str, dest: &Path) -> Result<DownloadOutcome> {
    download_to_path_with_cancel(url, dest, &AtomicBool::new(false))
}

/// [`download_to_path`] with cooperative cancellation: `cancel` is checked
/// between copy chunks and at every request(-retry) boundary, so a set flag
/// stops a multi-GB stream within one chunk instead of running to the end.
///
/// On cancel the partial `.download` temp is flushed, closed, and LEFT IN
/// PLACE — the next call for the same `dest` Range-resumes it exactly like an
/// interrupted download — and `Err(`[`DataSourceError::DownloadCancelled`]`)`
/// is returned so callers can tell a user stop from a failure.
pub fn download_to_path_with_cancel(
    url: &str,
    dest: &Path,
    cancel: &AtomicBool,
) -> Result<DownloadOutcome> {
    let cancelled = || DataSourceError::DownloadCancelled {
        url: url.to_owned(),
    };
    if cancel.load(Ordering::Relaxed) {
        return Err(cancelled());
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }

    // Best-effort total size (fileServer advertises Content-Length; NCSS may
    // not). Used for the cache-hit shortcut and the final size check.
    let expected_total = head_content_length(url, cancel);
    if let Some(total) = expected_total
        && dest
            .metadata()
            .map(|meta| meta.len() == total)
            .unwrap_or(false)
    {
        return Ok(DownloadOutcome {
            path: dest.to_owned(),
            bytes: total,
            resumed: false,
            cache_hit: true,
        });
    }

    let temp_path = dest.with_extension("download");
    let have = temp_path.metadata().map(|meta| meta.len()).unwrap_or(0);
    let want_resume = have > 0 && expected_total.map(|total| have < total).unwrap_or(true);

    // Retry only the request start (send + status); a mid-body break is
    // recovered by the caller re-invoking, which Range-resumes the temp.
    let mut response = with_retry(GDEX_RETRY_BACKOFFS, |_| {
        // Fatal short-circuits the remaining retries, so a cancel during the
        // flaky-server backoff schedule stops at the next attempt instead of
        // sleeping out the rest (up to ~23 s).
        if cancel.load(Ordering::Relaxed) {
            return Attempt::Fatal(cancelled());
        }
        let mut request = crate::download_http_client().get(url);
        if want_resume {
            request = request.header(RANGE, format!("bytes={have}-"));
        }
        match request.send() {
            Err(err) => Attempt::Retry(DataSourceError::Http(err)),
            Ok(response) => {
                let status = response.status().as_u16();
                if (500..600).contains(&status) {
                    Attempt::Retry(gdex_error(format!("gdex {url}: status {status}")))
                } else if status == 416 || (200..300).contains(&status) {
                    // 416 Range Not Satisfiable = the temp already holds every
                    // byte; accept and let the size check confirm below.
                    Attempt::Accept(response)
                } else {
                    Attempt::Fatal(gdex_error(format!("gdex {url}: status {status}")))
                }
            }
        }
    })?;

    let status = response.status().as_u16();
    let resumed = if status == 416 {
        // 416 Range Not Satisfiable: the temp already holds every byte.
        have > 0
    } else if status == 206 && want_resume {
        // Partial content: append to the existing temp.
        let mut file = fs::OpenOptions::new().append(true).open(&temp_path)?;
        if let CopyEnd::Cancelled(_) = copy_with_cancel(&mut response, &mut file, cancel)? {
            // Close the temp and leave it on disk (flushed by the copy):
            // returning before the size check / rename below is what keeps
            // the partial resumable.
            drop(file);
            return Err(cancelled());
        }
        true
    } else {
        // 200 (server ignored Range, or a fresh start): rewrite from scratch.
        let mut file = fs::File::create(&temp_path)?;
        if let CopyEnd::Cancelled(_) = copy_with_cancel(&mut response, &mut file, cancel)? {
            drop(file);
            return Err(cancelled());
        }
        false
    };

    let final_len = temp_path.metadata()?.len();
    if let Some(total) = expected_total
        && final_len != total
    {
        return Err(DataSourceError::DownloadSizeMismatch {
            url: url.to_owned(),
            expected: total,
            actual: final_len,
        });
    }

    if dest.exists() {
        fs::remove_file(dest)?;
    }
    fs::rename(&temp_path, dest)?;
    Ok(DownloadOutcome {
        path: dest.to_owned(),
        bytes: final_len,
        resumed,
        cache_hit: false,
    })
}

/// Best-effort `Content-Length` via HEAD (metadata client), retrying 5xx with
/// the GDEX backoff — the server 503s readily on back-to-back requests, so a
/// single-shot HEAD would usually miss the size and defeat the size-verify /
/// cache-hit shortcut. Any ultimate failure (exhausted retries, a 4xx, or a
/// missing header) yields `None`; the download then relies on the GET's own
/// status rather than failing.
fn head_content_length(url: &str, cancel: &AtomicBool) -> Option<u64> {
    with_retry(GDEX_RETRY_BACKOFFS, |_| {
        // A cancel during the HEAD's flaky-server retries falls out as `None`
        // here; the caller's own cancel checks then stop the download before
        // any bytes move.
        if cancel.load(Ordering::Relaxed) {
            return Attempt::Fatal(DataSourceError::DownloadCancelled {
                url: url.to_owned(),
            });
        }
        match crate::metadata_http_client().head(url).send() {
            Err(err) => Attempt::Retry(DataSourceError::Http(err)),
            Ok(response) => {
                let status = response.status().as_u16();
                if (500..600).contains(&status) {
                    Attempt::Retry(gdex_error(format!("gdex HEAD {url}: status {status}")))
                } else if !(200..300).contains(&status) {
                    Attempt::Fatal(gdex_error(format!("gdex HEAD {url}: status {status}")))
                } else {
                    Attempt::Accept(
                        response
                            .headers()
                            .get(CONTENT_LENGTH)
                            .and_then(|value| value.to_str().ok())
                            .and_then(|value| value.parse::<u64>().ok()),
                    )
                }
            }
        }
    })
    .ok()
    .flatten()
}

// ---------------------------------------------------------------------------
// quick-xml serde models (THREDDS InvCatalog + NCSS gridDataset)
// ---------------------------------------------------------------------------
//
// Namespaced attributes are matched with an `alias` for the un-prefixed name so
// parsing is robust to quick-xml's raw-name handling either way.

#[derive(Debug, Deserialize)]
struct CatalogXml {
    #[serde(rename = "catalogRef", default)]
    catalog_refs: Vec<CatalogRefXml>,
    #[serde(rename = "dataset", default)]
    datasets: Vec<DatasetXml>,
}

#[derive(Debug, Deserialize)]
struct DatasetXml {
    #[serde(rename = "@name", default)]
    name: String,
    #[serde(rename = "@urlPath")]
    url_path: Option<String>,
    #[serde(rename = "catalogRef", default)]
    catalog_refs: Vec<CatalogRefXml>,
    #[serde(rename = "dataset", default)]
    datasets: Vec<DatasetXml>,
    #[serde(rename = "dataSize")]
    data_size: Option<DataSizeXml>,
    #[serde(rename = "date", default)]
    dates: Vec<DateXml>,
}

#[derive(Debug, Deserialize)]
struct CatalogRefXml {
    #[serde(rename = "@xlink:href", alias = "@href")]
    href: Option<String>,
    #[serde(rename = "@xlink:title", alias = "@title", default)]
    #[allow(dead_code)]
    title: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DataSizeXml {
    #[serde(rename = "@units")]
    units: Option<String>,
    #[serde(rename = "$text", default)]
    value: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DateXml {
    #[serde(rename = "$text", default)]
    value: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GridDatasetXml {
    #[serde(rename = "gridSet", default)]
    grid_sets: Vec<GridSetXml>,
    #[serde(rename = "LatLonBox")]
    lat_lon_box: Option<LatLonBoxXml>,
    #[serde(rename = "TimeSpan")]
    time_span: Option<TimeSpanXml>,
}

#[derive(Debug, Deserialize)]
struct GridSetXml {
    #[serde(rename = "grid", default)]
    grids: Vec<GridXml>,
}

#[derive(Debug, Deserialize)]
struct GridXml {
    #[serde(rename = "@name")]
    name: String,
    #[serde(rename = "@desc", default)]
    desc: Option<String>,
    #[serde(rename = "attribute", default)]
    attributes: Vec<GridAttrXml>,
}

impl GridXml {
    /// Value of the child `<attribute name="{name}" value="...">`, if present
    /// and non-empty.
    fn find_attr(&self, name: &str) -> Option<String> {
        self.attributes
            .iter()
            .find(|attr| attr.name == name)
            .and_then(|attr| attr.value.clone())
            .filter(|value| !value.trim().is_empty())
    }
}

#[derive(Debug, Deserialize)]
struct GridAttrXml {
    #[serde(rename = "@name")]
    name: String,
    #[serde(rename = "@value", default)]
    value: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LatLonBoxXml {
    west: f64,
    east: f64,
    south: f64,
    north: f64,
}

#[derive(Debug, Deserialize)]
struct TimeSpanXml {
    begin: String,
    end: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    const CATALOG_TOP: &str = include_str!("fixtures/gdex_catalog_top.xml");
    const CATALOG_LEAF: &str = include_str!("fixtures/gdex_catalog_leaf.xml");
    const NCSS_DATASET: &str = include_str!("fixtures/gdex_ncss_dataset.xml");
    const CATALOG_ERA20C_TOP: &str = include_str!("fixtures/gdex_catalog_era20c_top.xml");
    const CATALOG_ERA20C_DECADE: &str = include_str!("fixtures/gdex_catalog_era20c_decade.xml");

    const TOP_URL: &str = "https://tds.gdex.ucar.edu/thredds/catalog/files/g/d612005/catalog.xml";
    const LEAF_URL: &str =
        "https://tds.gdex.ucar.edu/thredds/catalog/files/g/d612005/future2D/208001/catalog.xml";
    const ERA20C_TOP_URL: &str =
        "https://tds.gdex.ucar.edu/thredds/catalog/files/g/d626000/catalog.xml";
    const ERA20C_DECADE_URL: &str = "https://tds.gdex.ucar.edu/thredds/catalog/files/g/d626000/e20c.oper.an.sfc.3hr/1900_1909/catalog.xml";

    #[test]
    fn dataset_registry_ids_are_unique_and_urls_well_formed() {
        // The default (first) entry is CONUS II — the pre-picker dataset whose
        // downloads are grandfathered flat in the cache root.
        assert_eq!(GDEX_DATASETS[0].id, "d612005");

        let mut seen = HashSet::new();
        for dataset in GDEX_DATASETS {
            assert!(seen.insert(dataset.id), "duplicate id {}", dataset.id);
            assert_eq!(
                dataset.catalog_url(),
                format!("{CATALOG_BASE}files/g/{}/catalog.xml", dataset.id)
            );
            assert_eq!(dataset_by_id(dataset.id), Some(dataset));
            assert!(!dataset.label.is_empty());
            assert!(!dataset.attribution.is_empty());
        }
        assert_eq!(dataset_by_id("d000000"), None);

        // The two shipped datasets, by exact URL (verified live 2026-07-09).
        assert_eq!(dataset_catalog_url("d612005"), TOP_URL);
        assert_eq!(dataset_catalog_url("d626000"), ERA20C_TOP_URL);
    }

    #[test]
    fn dataset_id_parses_from_url_path_shapes() {
        assert_eq!(
            dataset_id_from_url_path(
                "files/g/d612005/future2D/208001/wrf2d_d01_2080-01-01_00:00:00.nc"
            ),
            Some("d612005")
        );
        assert_eq!(
            dataset_id_from_url_path(
                "files/g/d626000/e20c.oper.an.sfc.3hr/1900_1909/e20c.oper.an.sfc.3hr.128_151_msl.regn80sc.1900010100_1900123121.grb"
            ),
            Some("d626000")
        );
        // The bare scan root and non-TDS shapes yield nothing.
        assert_eq!(dataset_id_from_url_path("files/g/"), None);
        assert_eq!(dataset_id_from_url_path("other/d612005/x.nc"), None);
    }

    #[test]
    fn era20c_top_catalog_lists_nine_subtrees_and_drops_the_dump_leaf() {
        let parsed = parse_catalog(CATALOG_ERA20C_TOP, ERA20C_TOP_URL).expect("era20c top parses");
        // The root's only <dataset urlPath> is the extensionless scan `dump`,
        // which the extension whitelist drops.
        assert!(parsed.leaves.is_empty(), "dump must not survive as a leaf");
        assert_eq!(parsed.child_catalog_urls.len(), 9, "nine e20c subtrees");
        for expected in [
            "/e20c.oper.an.sfc.3hr/catalog.xml",
            "/e20c.oper.an.sfc.6hr/catalog.xml",
            "/e20c.oper.an.pl.3hr/catalog.xml",
            "/e20c.oper.invariant/catalog.xml",
        ] {
            assert!(
                parsed
                    .child_catalog_urls
                    .iter()
                    .any(|url| url.ends_with(expected)),
                "missing subtree {expected}"
            );
        }
        assert!(
            parsed
                .child_catalog_urls
                .iter()
                .all(|url| url.contains("/files/g/d626000/")),
            "every child resolves inside the d626000 tree"
        );
    }

    #[test]
    fn era20c_decade_catalog_keeps_grb_leaves_with_size_and_date() {
        let parsed =
            parse_catalog(CATALOG_ERA20C_DECADE, ERA20C_DECADE_URL).expect("decade parses");
        assert!(parsed.child_catalog_urls.is_empty(), "a decade dir is flat");
        assert_eq!(parsed.leaves.len(), 3);

        let msl = parsed
            .leaves
            .iter()
            .find(|leaf| leaf.name.contains("128_151_msl"))
            .expect("msl leaf present");
        assert_eq!(
            msl.url_path,
            "files/g/d626000/e20c.oper.an.sfc.3hr/1900_1909/e20c.oper.an.sfc.3hr.128_151_msl.regn80sc.1900010100_1900123121.grb"
        );
        assert_eq!(
            msl.download_url,
            format!("{FILESERVER_BASE}{}", msl.url_path),
            "fileServer download URL builds for .grb exactly as for .nc"
        );
        // <dataSize units="Mbytes">299.3</dataSize> -> decimal bytes.
        assert_eq!(msl.size_bytes, Some(299_300_000));
        assert_eq!(msl.date.as_deref(), Some("2014-11-04T18:47:44Z"));
    }

    #[test]
    fn top_catalog_discovers_five_subtrees_and_no_leaves() {
        let parsed = parse_catalog(CATALOG_TOP, TOP_URL).expect("top catalog parses");
        assert!(parsed.leaves.is_empty(), "the scan root has no file leaves");
        assert_eq!(
            parsed.child_catalog_urls,
            vec![
                "https://tds.gdex.ucar.edu/thredds/catalog/files/g/d612005/INVARIANT/catalog.xml"
                    .to_owned(),
                "https://tds.gdex.ucar.edu/thredds/catalog/files/g/d612005/future2D/catalog.xml"
                    .to_owned(),
                "https://tds.gdex.ucar.edu/thredds/catalog/files/g/d612005/future3D/catalog.xml"
                    .to_owned(),
                "https://tds.gdex.ucar.edu/thredds/catalog/files/g/d612005/hist2D/catalog.xml"
                    .to_owned(),
                "https://tds.gdex.ucar.edu/thredds/catalog/files/g/d612005/hist3D/catalog.xml"
                    .to_owned(),
            ],
            "the five CONUS II subtrees, resolved relative to the catalog URL"
        );
    }

    #[test]
    fn leaf_catalog_keeps_data_files_and_drops_the_dump_entry() {
        let parsed = parse_catalog(CATALOG_LEAF, LEAF_URL).expect("leaf catalog parses");
        assert!(
            parsed.child_catalog_urls.is_empty(),
            "a leaf catalog has no sub-catalogs"
        );
        // Six real .nc leaves; the synthetic no-extension `dump` is dropped.
        assert_eq!(parsed.leaves.len(), 6, "dump entry must be filtered out");
        assert!(
            !parsed
                .leaves
                .iter()
                .any(|leaf| leaf.url_path.ends_with("dump")),
            "the dump entry leaked through the extension whitelist"
        );

        let first = &parsed.leaves[0];
        assert_eq!(first.name, "wrf2d_d01_2080-01-01_00:00:00.nc");
        assert_eq!(
            first.url_path,
            "files/g/d612005/future2D/208001/wrf2d_d01_2080-01-01_00:00:00.nc"
        );
        assert_eq!(
            first.download_url,
            "https://tds.gdex.ucar.edu/thredds/fileServer/files/g/d612005/future2D/208001/wrf2d_d01_2080-01-01_00:00:00.nc"
        );
        assert_eq!(
            first.ncss_url,
            "https://tds.gdex.ucar.edu/thredds/ncss/grid/files/g/d612005/future2D/208001/wrf2d_d01_2080-01-01_00:00:00.nc"
        );
        assert_eq!(first.size_bytes, Some(163_700_000));
        assert_eq!(first.date.as_deref(), Some("2024-03-25T21:34:51.095Z"));
    }

    #[test]
    fn relative_hrefs_resolve_against_the_catalog_url() {
        assert_eq!(
            resolve_relative(TOP_URL, "INVARIANT/catalog.xml"),
            "https://tds.gdex.ucar.edu/thredds/catalog/files/g/d612005/INVARIANT/catalog.xml"
        );
        // A parent-relative href collapses `..`.
        assert_eq!(
            resolve_relative(LEAF_URL, "../209912/catalog.xml"),
            "https://tds.gdex.ucar.edu/thredds/catalog/files/g/d612005/future2D/209912/catalog.xml"
        );
        // An absolute external link passes through unchanged (and would be
        // filtered by is_tds_catalog_url downstream).
        assert_eq!(
            resolve_relative(TOP_URL, "https://example.org/other.xml"),
            "https://example.org/other.xml"
        );
        assert!(!is_tds_catalog_url("https://example.org/other.xml"));
    }

    #[test]
    fn data_extension_whitelist_matches_expected_types() {
        assert!(has_data_extension("a/b/wrf2d_d01.nc"));
        assert!(has_data_extension("x.grb"));
        assert!(has_data_extension("x.grib"));
        assert!(has_data_extension("x.grb2"));
        assert!(has_data_extension("X.NC"));
        assert!(!has_data_extension("files/g/d626000/dump"));
        assert!(!has_data_extension("notes.txt"));
    }

    #[test]
    fn colon_sanitized_to_underscore_for_local_path() {
        assert_eq!(
            sanitize_leaf_filename("wrf2d_d01_2080-01-01_00:00:00.nc"),
            "wrf2d_d01_2080-01-01_00_00_00.nc"
        );
        let leaf = Leaf {
            name: "wrf2d_d01_2080-01-01_00:00:00.nc".to_owned(),
            url_path: "files/g/d612005/future2D/208001/wrf2d_d01_2080-01-01_00:00:00.nc".to_owned(),
            download_url: String::new(),
            ncss_url: String::new(),
            size_bytes: None,
            date: None,
        };
        let path = local_path_for_leaf(Path::new("cache"), &leaf);
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("wrf2d_d01_2080-01-01_00_00_00.nc")
        );
    }

    #[test]
    fn ncss_dataset_parses_variables_bbox_and_timespan() {
        let dataset = parse_ncss_dataset(NCSS_DATASET).expect("ncss dataset parses");
        let names: Vec<&str> = dataset
            .variables
            .iter()
            .map(|var| var.name.as_str())
            .collect();
        assert_eq!(names, vec!["ACDEWC", "T2", "TK", "U", "V"]);

        let t2 = dataset
            .variables
            .iter()
            .find(|var| var.name == "T2")
            .expect("T2 present");
        assert_eq!(t2.units.as_deref(), Some("K"));
        assert_eq!(t2.description.as_deref(), Some("TEMP at 2 M"));

        let bbox = dataset.lat_lon_box.expect("LatLonBox present");
        assert!((bbox.west - (-156.8905)).abs() < 1e-6);
        assert!((bbox.east - (-40.2612)).abs() < 1e-6);
        assert!((bbox.south - 15.0072).abs() < 1e-6);
        assert!((bbox.north - 73.2915).abs() < 1e-6);

        let span = dataset.time_span.expect("TimeSpan present");
        assert_eq!(span.begin, "2080-01-01T00:00:00Z");
        assert_eq!(span.end, "2080-01-01T00:00:00Z");
    }

    #[test]
    fn ncss_subset_url_is_deterministic_and_requests_classic_netcdf() {
        let url_path = "files/g/d612005/future2D/208001/wrf2d_d01_2080-01-01_00:00:00.nc";

        // Vars only (no spatial/temporal narrowing).
        let vars_only = NcssSubset {
            vars: vec!["T2".to_owned()],
            ..NcssSubset::default()
        };
        assert_eq!(
            ncss_subset_url(url_path, &vars_only),
            "https://tds.gdex.ucar.edu/thredds/ncss/grid/files/g/d612005/future2D/208001/wrf2d_d01_2080-01-01_00:00:00.nc?var=T2&accept=netcdf"
        );

        // Full subset: var + bbox + single time. Colons in the time value are
        // percent-encoded; accept=netcdf (NOT netcdf4) trails.
        let full = NcssSubset {
            vars: vec!["T2".to_owned()],
            bbox: Some(LatLonBox {
                north: 45.0,
                south: 40.0,
                east: -95.0,
                west: -100.0,
            }),
            time: Some("2080-01-01T00:00:00Z".to_owned()),
            ..NcssSubset::default()
        };
        assert_eq!(
            ncss_subset_url(url_path, &full),
            "https://tds.gdex.ucar.edu/thredds/ncss/grid/files/g/d612005/future2D/208001/wrf2d_d01_2080-01-01_00:00:00.nc\
             ?var=T2&north=45&south=40&east=-95&west=-100&time=2080-01-01T00%3A00%3A00Z&accept=netcdf"
        );

        // A time range is honored when no single time is set.
        let ranged = NcssSubset {
            vars: vec!["T2".to_owned(), "U".to_owned()],
            time_start: Some("2080-01-01T00:00:00Z".to_owned()),
            time_end: Some("2080-01-01T06:00:00Z".to_owned()),
            ..NcssSubset::default()
        };
        let url = ncss_subset_url(url_path, &ranged);
        assert!(url.contains("var=T2&var=U"));
        assert!(url.contains("time_start=2080-01-01T00%3A00%3A00Z"));
        assert!(url.contains("time_end=2080-01-01T06%3A00%3A00Z"));
        assert!(url.ends_with("&accept=netcdf"));
    }

    #[test]
    fn response_classification_retries_5xx_and_empty_bodies() {
        assert_eq!(classify_response(503, false), ResponseClass::Retry);
        assert_eq!(classify_response(500, false), ResponseClass::Retry);
        assert_eq!(classify_response(200, true), ResponseClass::Retry);
        assert_eq!(classify_response(200, false), ResponseClass::Accept);
        assert_eq!(classify_response(404, false), ResponseClass::Fatal);
        assert_eq!(classify_response(400, false), ResponseClass::Fatal);
    }

    #[test]
    fn with_retry_stops_on_first_success() {
        use std::cell::Cell;
        // Two transient failures, then success on the third attempt.
        let calls = Cell::new(0usize);
        let no_sleep = [StdDuration::ZERO, StdDuration::ZERO, StdDuration::ZERO];
        let result: Result<&str> = with_retry(&no_sleep, |_| {
            let n = calls.get();
            calls.set(n + 1);
            if n < 2 {
                Attempt::Retry(gdex_error("transient"))
            } else {
                Attempt::Accept("ok")
            }
        });
        assert_eq!(result.expect("succeeds by the third try"), "ok");
        assert_eq!(calls.get(), 3, "must stop the instant it succeeds");
    }

    #[test]
    fn with_retry_returns_fatal_immediately() {
        use std::cell::Cell;
        let calls = Cell::new(0usize);
        let no_sleep = [StdDuration::ZERO, StdDuration::ZERO];
        let result: Result<&str> = with_retry(&no_sleep, |_| {
            calls.set(calls.get() + 1);
            Attempt::Fatal(gdex_error("permanent"))
        });
        assert!(result.is_err());
        assert_eq!(calls.get(), 1, "a fatal answer is not retried");
    }

    #[test]
    fn with_retry_exhausts_then_errors() {
        use std::cell::Cell;
        let calls = Cell::new(0usize);
        let no_sleep = [StdDuration::ZERO, StdDuration::ZERO];
        let result: Result<&str> = with_retry(&no_sleep, |_| -> Attempt<&str> {
            calls.set(calls.get() + 1);
            Attempt::Retry(gdex_error("always transient"))
        });
        assert!(result.is_err());
        assert_eq!(calls.get(), 3, "one initial try plus two backoff retries");
    }

    /// Yields one prebuilt chunk per `read` call; optionally sets a shared
    /// cancel flag as a side effect of returning the k-th chunk (1-based) —
    /// the user pressing Cancel while a chunk is in flight.
    struct ChunkReader {
        chunks: Vec<Vec<u8>>,
        next: usize,
        cancel_after: Option<(usize, std::sync::Arc<AtomicBool>)>,
    }

    impl ChunkReader {
        fn new(
            chunks: Vec<Vec<u8>>,
            cancel_after: Option<(usize, std::sync::Arc<AtomicBool>)>,
        ) -> Self {
            Self {
                chunks,
                next: 0,
                cancel_after,
            }
        }
    }

    impl Read for ChunkReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let Some(chunk) = self.chunks.get(self.next) else {
                return Ok(0);
            };
            assert!(buf.len() >= chunk.len(), "test chunks fit the copy buffer");
            buf[..chunk.len()].copy_from_slice(chunk);
            self.next += 1;
            if let Some((after, flag)) = &self.cancel_after
                && self.next == *after
            {
                flag.store(true, Ordering::Relaxed);
            }
            Ok(chunk.len())
        }
    }

    /// Records written bytes and counts `flush` calls — the cancel contract
    /// requires the partial to be flushed before the copy returns.
    #[derive(Default)]
    struct FlushTracking {
        bytes: Vec<u8>,
        flushes: usize,
    }

    impl Write for FlushTracking {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    #[test]
    fn copy_with_cancel_without_cancel_copies_everything() {
        let chunks: Vec<Vec<u8>> = (0..5u8).map(|i| vec![i; 100]).collect();
        let expected: Vec<u8> = chunks.concat();
        let mut reader = ChunkReader::new(chunks, None);
        let mut writer = FlushTracking::default();

        let end = copy_with_cancel(&mut reader, &mut writer, &AtomicBool::new(false))
            .expect("in-memory copy cannot fail");

        assert_eq!(end, CopyEnd::Complete(expected.len() as u64));
        assert_eq!(
            writer.bytes, expected,
            "a never-cancelled copy is byte-identical"
        );
        assert!(writer.flushes >= 1, "completion flushes the writer");
    }

    #[test]
    fn copy_with_cancel_stops_between_chunks_and_keeps_flushed_partial() {
        use std::sync::Arc;
        let chunks: Vec<Vec<u8>> = (0..5u8).map(|i| vec![i; 100]).collect();
        let expected_partial: Vec<u8> = chunks[..2].concat();
        let flag = Arc::new(AtomicBool::new(false));
        // The flag is set while chunk 2 is in flight: that chunk still lands,
        // then the check at the top of the next iteration stops the copy.
        let mut reader = ChunkReader::new(chunks, Some((2, flag.clone())));
        let mut writer = FlushTracking::default();

        let end =
            copy_with_cancel(&mut reader, &mut writer, &flag).expect("in-memory copy cannot fail");

        assert_eq!(end, CopyEnd::Cancelled(expected_partial.len() as u64));
        assert_eq!(
            writer.bytes, expected_partial,
            "every byte read before the cancel is written, none after"
        );
        assert!(
            writer.flushes >= 1,
            "the partial is flushed before returning"
        );
        assert_eq!(
            reader.next, 2,
            "no further chunks are pulled after the cancel"
        );
    }

    #[test]
    fn cancelled_copy_keeps_partial_temp_that_resumes_to_identical_bytes() {
        use std::sync::Arc;
        let dir = unique_temp_dir("gdex-cancel-partial");
        fs::create_dir_all(&dir).expect("temp dir");
        // Mirrors `dest.with_extension("download")` in download_to_path.
        let temp_path = dir.join("wrf2d_test.nc.download");
        let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let chunk = 4096;

        // Pass 1 — a fresh download (the 200 path: File::create) cancelled
        // after 10 chunks. The partial temp must survive, exactly as written.
        let chunks: Vec<Vec<u8>> = payload.chunks(chunk).map(<[u8]>::to_vec).collect();
        let flag = Arc::new(AtomicBool::new(false));
        let mut reader = ChunkReader::new(chunks, Some((10, flag.clone())));
        let mut file = fs::File::create(&temp_path).expect("create temp");
        let end = copy_with_cancel(&mut reader, &mut file, &flag).expect("file copy ok");
        drop(file);
        let have = (10 * chunk) as u64;
        assert_eq!(end, CopyEnd::Cancelled(have));
        assert_eq!(
            fs::metadata(&temp_path)
                .expect("temp survives the cancel")
                .len(),
            have,
            "the partial temp is kept on disk with every flushed byte"
        );

        // Pass 2 — the resume (the 206 path: append the remainder), as the
        // next download_to_path call does after its Range request.
        let rest: Vec<Vec<u8>> = payload[have as usize..]
            .chunks(chunk)
            .map(<[u8]>::to_vec)
            .collect();
        let mut reader = ChunkReader::new(rest, None);
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&temp_path)
            .expect("append temp");
        let end = copy_with_cancel(&mut reader, &mut file, &AtomicBool::new(false))
            .expect("file copy ok");
        drop(file);
        assert_eq!(end, CopyEnd::Complete(payload.len() as u64 - have));
        assert_eq!(
            fs::read(&temp_path).expect("read back"),
            payload,
            "cancel + resume reassembles the exact original bytes"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn crawl_cache_round_trips_through_disk() {
        let dir = unique_temp_dir("gdex-cache-roundtrip");
        fs::create_dir_all(&dir).expect("temp dir");

        // A cold read of a never-written cache is a miss.
        let cache_path = catalog_cache_path(&dir, "d612005");
        assert_eq!(
            read_catalog_cache(&cache_path).expect("cold read ok"),
            None,
            "missing cache reads as None"
        );

        let cache = CatalogCache {
            dataset_id: "d612005".to_owned(),
            crawled_at: "2026-07-09T00:00:00+00:00".to_owned(),
            leaves: vec![Leaf {
                name: "wrf2d_d01_2080-01-01_00:00:00.nc".to_owned(),
                url_path: "files/g/d612005/future2D/208001/wrf2d_d01_2080-01-01_00:00:00.nc"
                    .to_owned(),
                download_url: format!(
                    "{FILESERVER_BASE}files/g/d612005/future2D/208001/wrf2d_d01_2080-01-01_00:00:00.nc"
                ),
                ncss_url: format!(
                    "{NCSS_GRID_BASE}files/g/d612005/future2D/208001/wrf2d_d01_2080-01-01_00:00:00.nc"
                ),
                size_bytes: Some(163_700_000),
                date: Some("2024-03-25T21:34:51.095Z".to_owned()),
            }],
        };
        write_catalog_cache(&cache_path, &cache).expect("write cache");
        let read_back = read_catalog_cache(&cache_path)
            .expect("warm read ok")
            .expect("cache present after write");
        assert_eq!(read_back, cache, "cache must round-trip byte-for-byte");

        let _ = fs::remove_dir_all(&dir);
    }

    /// A unique scratch directory under the OS temp dir (no external dep).
    fn unique_temp_dir(tag: &str) -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("{tag}-{}-{nanos}", std::process::id()))
    }

    // -----------------------------------------------------------------------
    // Live proofs — run ONCE on a build node, never in CI (they hit the
    // network; keep them LIGHT — no 156 MB pulls).
    //   cargo test -p data_source gdex -- --ignored
    // -----------------------------------------------------------------------

    #[test]
    #[ignore = "live: crawls GDEX; run once on a node"]
    fn live_crawl_d612005_top_and_one_month() {
        // Top scan root -> the five CONUS II subtrees.
        let top =
            fetch_and_parse_catalog(&dataset_catalog_url("d612005")).expect("live top catalog");
        eprintln!(
            "live top: {} child catalogs, {} leaves",
            top.child_catalog_urls.len(),
            top.leaves.len()
        );
        assert_eq!(top.child_catalog_urls.len(), 5, "five subtrees");
        assert!(
            top.child_catalog_urls
                .iter()
                .any(|url| url.ends_with("/future2D/catalog.xml"))
        );

        // One real month leaf catalog -> its .nc leaves (light: ~240 KB XML).
        let month = fetch_and_parse_catalog(LEAF_URL).expect("live month catalog");
        eprintln!("live future2D/208001: {} leaves", month.leaves.len());
        assert!(
            month.leaves.len() > 100,
            "a month has hundreds of hourly files"
        );
        assert!(
            month
                .leaves
                .iter()
                .all(|leaf| leaf.download_url.starts_with(FILESERVER_BASE))
        );
        assert!(
            month
                .leaves
                .iter()
                .any(|leaf| leaf.name == "wrf2d_d01_2080-01-01_00:00:00.nc")
        );
    }

    #[test]
    #[ignore = "live: lists the ERA-20C catalog root + one decade; run once on a node"]
    fn live_era20c_catalog_root_and_one_decade() {
        // Root scan -> the nine e20c.oper.* subtrees; the extensionless scan
        // `dump` must not surface as a leaf (light: ~8 KB XML).
        let top =
            fetch_and_parse_catalog(&dataset_catalog_url("d626000")).expect("live era20c top");
        eprintln!(
            "live d626000 top: {} child catalogs, {} leaves",
            top.child_catalog_urls.len(),
            top.leaves.len()
        );
        assert_eq!(top.child_catalog_urls.len(), 9, "nine e20c subtrees");
        assert!(top.leaves.is_empty(), "dump filtered at the root");
        assert!(
            top.child_catalog_urls
                .iter()
                .any(|url| url.ends_with("/e20c.oper.an.sfc.3hr/catalog.xml"))
        );

        // One decade dir of the 3-hourly surface analysis (~390 KB XML):
        // hundreds of .grb leaves, every one a fileServer download URL with an
        // advertised size — what the picker's Download button consumes.
        let decade = fetch_and_parse_catalog(ERA20C_DECADE_URL).expect("live era20c decade");
        eprintln!(
            "live e20c.oper.an.sfc.3hr/1900_1909: {} leaves",
            decade.leaves.len()
        );
        assert!(
            decade.leaves.len() > 500,
            "a decade dir carries hundreds of per-variable year files"
        );
        assert!(
            decade
                .leaves
                .iter()
                .all(|leaf| leaf.url_path.to_ascii_lowercase().ends_with(".grb")
                    && leaf.download_url.starts_with(FILESERVER_BASE))
        );
        assert!(
            decade.leaves.iter().any(|leaf| leaf.name
                == "e20c.oper.an.sfc.3hr.128_151_msl.regn80sc.1900010100_1900123121.grb"),
            "the fixture's msl leaf exists live"
        );
        assert!(
            decade.leaves.iter().all(|leaf| leaf.size_bytes.is_some()),
            "decade leaves advertise sizes (drives the progress bar)"
        );
    }

    #[test]
    #[ignore = "live: NCSS subset download; run once on a node"]
    fn live_ncss_tiny_subset_is_valid_netcdf3() {
        let url_path = "files/g/d612005/future2D/208001/wrf2d_d01_2080-01-01_00:00:00.nc";

        // Confirm the grid metadata parses live.
        let meta = fetch_ncss_dataset(url_path).expect("live ncss dataset.xml");
        eprintln!("live ncss vars: {}", meta.variables.len());
        assert!(meta.variables.iter().any(|var| var.name == "T2"));

        // Tiny subset: one var, small bbox, single time.
        let subset = NcssSubset {
            vars: vec!["T2".to_owned()],
            bbox: Some(LatLonBox {
                north: 45.0,
                south: 40.0,
                east: -95.0,
                west: -100.0,
            }),
            time: Some("2080-01-01T00:00:00Z".to_owned()),
            ..NcssSubset::default()
        };
        let url = ncss_subset_url(url_path, &subset);
        let dir = unique_temp_dir("gdex-ncss-subset");
        let dest = dir.join("subset_T2.nc");
        let outcome = download_to_path(&url, &dest).expect("live subset download");
        eprintln!("live subset: {} bytes -> {}", outcome.bytes, dest.display());

        let bytes = fs::read(&dest).expect("read subset");
        assert!(bytes.len() > 100, "subset should be non-trivial");
        // Classic NetCDF-3 magic: 'C' 'D' 'F' 0x01.
        assert_eq!(&bytes[..4], b"CDF\x01", "not a classic NetCDF-3 file");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "live: Range probe (1 KB, not a full download); run once on a node"]
    fn live_fileserver_supports_range_resume() {
        let url = "https://tds.gdex.ucar.edu/thredds/fileServer/files/g/d612005/future2D/208001/wrf2d_d01_2080-01-01_00:00:00.nc";

        // HEAD advertises the full size (recon: ~156 MB).
        let total = head_content_length(url, &AtomicBool::new(false)).expect("HEAD Content-Length");
        eprintln!("live HEAD Content-Length: {total}");
        assert!(total > 100_000_000, "the 00:00 file is ~156 MB");

        // A 1 KB partial GET must return 206 with exactly 1024 bytes — proof
        // the server honors Range (so an interrupted big download can resume).
        // Retry through the server's transient 503s (see the doc's flakiness
        // note) so the proof is stable.
        let response = with_retry(GDEX_RETRY_BACKOFFS, |_| match crate::download_http_client()
            .get(url)
            .header(RANGE, "bytes=0-1023")
            .send()
        {
            Err(err) => Attempt::Retry(DataSourceError::Http(err)),
            Ok(resp) => {
                let status = resp.status().as_u16();
                if (500..600).contains(&status) {
                    Attempt::Retry(gdex_error(format!("range probe: status {status}")))
                } else {
                    Attempt::Accept(resp)
                }
            }
        })
        .expect("range GET");
        assert_eq!(
            response.status().as_u16(),
            206,
            "expected 206 Partial Content"
        );
        let body = response.bytes().expect("range body");
        eprintln!("live Range bytes=0-1023 -> {} bytes", body.len());
        assert_eq!(body.len(), 1024, "Range must yield exactly 1024 bytes");
    }
}
