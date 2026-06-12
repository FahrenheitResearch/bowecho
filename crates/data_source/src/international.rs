//! International (non-NEXRAD) radar feed provider scaffolding.
//!
//! BowEcho's decode layer is already source-agnostic: `nexrad_io`'s shared
//! magic-byte router decodes ODIM_H5 polar volumes (EUMETNET OPERA Data
//! Information Model; Michelson et al., OPERA WP 2.1/2.2, v2.2-2.3),
//! CfRadial 1.x classic netCDF, DORADE sweepfiles, and NEXRAD Archive II
//! from plain byte buffers. What national feeds differ in is *cataloging*:
//! how to list sites and how to name the newest frame. This module defines
//! that catalog contract so each provider (DMI Denmark, SMHI Sweden,
//! GeoSphere Austria, FMI Finland, SHMU Slovakia, DWD Germany, CHMI Czechia,
//! KNMI Netherlands, JMA Japan via polar-coordinate GRIB2 per the JMA
//! technical format documentation, ...) is a small adapter, not a fork of
//! the polling pipeline.
//!
//! # Consumer pipeline
//!
//! A poller drives a provider like this:
//!
//! 1. [`IntlProvider::list_sites`] populates the site picker.
//! 2. On each poll tick, [`IntlProvider::latest`] returns a [`FramePlan`].
//! 3. If [`FramePlan::identity`] equals the identity of the frame already
//!    installed, the poller does nothing — no part is downloaded.
//! 4. Otherwise every [`PlanPart::url`] is fetched with
//!    [`crate::fetch_volume_bytes`] and decoded with
//!    `nexrad_io::decode_supported_volume_bytes`; multi-part plans with
//!    [`FramePlan::merge`] set are then assembled with
//!    `radar_core::merge_radar_volumes`.
//!
//! Providers therefore never download data themselves: `latest` does the
//! (cheap) catalog probe — via [`crate::fetch_text`] or an equivalent
//! listing helper — and describes the download; the shared poller owns
//! bytes, retries, decode, and merge.
//!
//! One provider-specific decode exception: JMA tars are multi-station
//! archives, and `nexrad_io::decode_supported_volume_bytes` decodes only
//! the FIRST station of such a tar. The poll consumer must therefore pass
//! the selected site as a `site_filter` to
//! `nexrad_io::jma::decode_jma_tar_volumes` when the plan came from
//! [`JmaProvider`] (see its docs).

use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Datelike, Utc};
use serde::Deserialize;

mod chmi;
mod dmi;
mod dwd;
mod fmi;
mod geosphere;
pub mod listing;
mod shmu;
mod smhi;

pub use chmi::ChmiProvider;
pub use dmi::DmiProvider;
pub use dwd::DwdProvider;
pub use fmi::FmiProvider;
pub use geosphere::GeoSphereProvider;
pub use shmu::ShmuProvider;
pub use smhi::SmhiProvider;

/// One selectable radar site offered by a provider.
#[derive(Clone, Debug, PartialEq)]
pub struct IntlSite {
    /// Owning provider's [`IntlProvider::id`], for routing a site selection
    /// back to its provider.
    pub provider_id: &'static str,
    /// Provider-scoped site identifier, passed verbatim to
    /// [`IntlProvider::latest`] (e.g. `"dkste"`, `"angelholm"`, `"skjav"`).
    pub site_id: String,
    /// Human-readable site name for the picker (e.g. `"Stevns"`).
    pub label: String,
    /// ISO-ish country label shown alongside the site (e.g. `"Denmark"`).
    pub country: &'static str,
    /// Site latitude in degrees north, when the catalog provides it.
    pub latitude_deg: Option<f32>,
    /// Site longitude in degrees east, when the catalog provides it.
    pub longitude_deg: Option<f32>,
}

/// One downloadable piece of a frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanPart {
    /// Absolute URL, fetched with [`crate::fetch_volume_bytes`] and decoded
    /// with `nexrad_io::decode_supported_volume_bytes`.
    pub url: String,
}

/// A provider's description of the newest available frame for one site.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FramePlan {
    /// Dedupe key for this frame, in the same role as the custom URL
    /// poller's `poll_last_file`: the poller stores the identity of the
    /// frame it last installed and skips the download when an equal
    /// identity comes back.
    ///
    /// Stability contract: the identity must be a pure function of the
    /// upstream frame — same upstream data, same identity, across repeated
    /// `latest` calls and across process restarts. It must change whenever
    /// new frame data is available (a new timestamp, but also e.g. a new
    /// part appearing for the same timestamp on a split feed). Never embed
    /// fetch times, random values, or signed/expiring URL query strings.
    /// Upstream file names or `{site}_{timestamp}` strings are good
    /// identities.
    pub identity: String,
    /// Parts to download, in decode-and-merge order.
    ///
    /// Ordering contract when [`Self::merge`] is set: parts are decoded and
    /// passed to `radar_core::merge_radar_volumes` in vector order, and the
    /// FIRST part is the merge base — it supplies the site record, VCP, and
    /// metadata, and wins moment-type collisions. Providers must put the
    /// most authoritative part first (conventionally the reflectivity
    /// volume). Later parts contribute their moments to elevation-matched
    /// cuts and add unmatched cuts.
    pub parts: Vec<PlanPart>,
    /// `false`: a single-file frame; `parts` must hold exactly one entry
    /// and the decoded volume installs directly (DMI/SMHI/Austria-style
    /// full PVOLs). `true`: a split frame; `parts` may hold one or more
    /// entries that decode to partial volumes of the SAME site and scan and
    /// merge into one (SHMU per-product PVOLs, DWD/CHMI per-sweep files).
    pub merge: bool,
}

/// A national/agency radar feed adapter.
///
/// Implementations must be cheap to construct and safe to share across the
/// UI and poller threads (`Send + Sync`, interior state behind sync
/// primitives if any). Methods are called on poller threads: they may block
/// on catalog HTTP (through [`crate::fetch_text`]-style helpers) but must
/// never panic on malformed upstream data — return a descriptive `Err`
/// instead, and never `unwrap()` network-derived values.
pub trait IntlProvider: Send + Sync {
    /// Stable machine id for settings/persistence (e.g. `"smhi"`). Must
    /// never change once shipped: saved site selections reference it.
    fn id(&self) -> &'static str;

    /// Human-readable provider name for the picker (e.g. `"SMHI Sweden"`).
    fn label(&self) -> &'static str;

    /// Country label shown in the picker (e.g. `"Sweden"`).
    fn country(&self) -> &'static str;

    /// Enumerate selectable sites. May hit the network; implementations
    /// should cache internally where the catalog is static. Every returned
    /// site's `provider_id` must equal [`Self::id`].
    fn list_sites(&self) -> Result<Vec<IntlSite>, String>;

    /// Describe the newest frame for `site_id` (a [`IntlSite::site_id`]
    /// this provider returned). This is the per-poll-tick catalog probe:
    /// keep it cheap — list/inspect, don't download volume bytes. Returns
    /// `Err` with a descriptive message when the site is unknown or the
    /// upstream catalog is unreachable/malformed.
    fn latest(&self, site_id: &str) -> Result<FramePlan, String>;
}

/// Registry of all built-in international providers.
///
/// Single-file ODIM PVOL feeds (one HDF5 download per frame): SMHI Sweden,
/// DMI Denmark, GeoSphere Austria, FMI Finland. Split-volume assembly
/// feeds (one frame = several ODIM files merged with
/// `radar_core::merge_radar_volumes`): SHMU Slovakia, DWD Germany, CHMI
/// Czechia. Multi-station tar feed (site-filtered decode, see
/// [`JmaProvider`]): JMA Japan.
pub fn intl_providers() -> Vec<Box<dyn IntlProvider>> {
    vec![
        Box::new(SmhiProvider::new()),
        Box::new(DmiProvider::new()),
        Box::new(GeoSphereProvider::new()),
        Box::new(FmiProvider::new()),
        Box::new(ShmuProvider::new()),
        Box::new(DwdProvider::new()),
        Box::new(ChmiProvider::new()),
        Box::new(JmaProvider),
    ]
}

/// Process-lifetime memoization of a provider's site catalog.
///
/// National radar networks change on a years scale, so the first successful
/// [`IntlProvider::list_sites`] answer is good for the whole session; errors
/// are never cached, so a flaky first call retries naturally.
pub(crate) struct SiteCache(Mutex<Option<Vec<IntlSite>>>);

impl SiteCache {
    pub(crate) const fn new() -> Self {
        Self(Mutex::new(None))
    }

    /// Return the cached catalog, or run `fill` and cache its success.
    pub(crate) fn get_or_fill(
        &self,
        fill: impl FnOnce() -> std::result::Result<Vec<IntlSite>, String>,
    ) -> std::result::Result<Vec<IntlSite>, String> {
        if let Ok(guard) = self.0.lock()
            && let Some(sites) = guard.as_ref()
        {
            return Ok(sites.clone());
        }
        let sites = fill()?;
        if let Ok(mut guard) = self.0.lock() {
            *guard = Some(sites.clone());
        }
        Ok(sites)
    }
}

/// One parsed S3-style `ListObjectsV2` page — real AWS S3 (FMI's
/// `fmi-opendata-radar-volume-hdf5` bucket) or an S3-compatible store
/// (GeoSphere Austria's `public.hub.geosphere.at/datahub`).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct S3StyleListing {
    /// Object keys, in the order the endpoint returned them (S3 lists keys
    /// in ascending lexicographic order, so the last key is the newest for
    /// zero-padded-timestamp file names).
    pub(crate) keys: Vec<String>,
    /// `CommonPrefixes` from a delimited listing ("subdirectories").
    pub(crate) common_prefixes: Vec<String>,
    /// Whether more keys follow this page.
    pub(crate) is_truncated: bool,
}

/// Build a `ListObjectsV2` query URL against an S3-style endpoint.
///
/// `endpoint` is scheme+host with no trailing slash. Prefixes and keys in
/// the radar feeds this serves are URL-safe (ASCII alphanumerics plus
/// `/ _ - .`), so no percent-encoding is applied.
pub(crate) fn s3_style_listing_url(
    endpoint: &str,
    prefix: &str,
    delimiter: Option<&str>,
    start_after: Option<&str>,
    max_keys: u32,
) -> String {
    let mut url = format!("{endpoint}/?list-type=2&max-keys={max_keys}&prefix={prefix}");
    if let Some(delimiter) = delimiter {
        url.push_str("&delimiter=");
        url.push_str(delimiter);
    }
    if let Some(start_after) = start_after {
        url.push_str("&start-after=");
        url.push_str(start_after);
    }
    url
}

/// Fetch and parse one S3-style listing page.
pub(crate) fn fetch_s3_style_listing(url: &str) -> std::result::Result<S3StyleListing, String> {
    let xml = crate::fetch_text(url).map_err(|err| format!("listing {url}: {err}"))?;
    parse_s3_style_listing(&xml).map_err(|err| format!("listing {url}: {err}"))
}

/// Parse an S3-style `ListBucketResult` XML document.
pub(crate) fn parse_s3_style_listing(xml: &str) -> std::result::Result<S3StyleListing, String> {
    let parsed: ListBucketResultXml = quick_xml::de::from_str(xml)
        .map_err(|err| format!("S3-style ListBucketResult XML parse failed: {err}"))?;
    Ok(S3StyleListing {
        keys: parsed
            .contents
            .into_iter()
            .map(|contents| contents.key)
            .collect(),
        common_prefixes: parsed
            .common_prefixes
            .into_iter()
            .map(|prefix| prefix.prefix)
            .collect(),
        is_truncated: parsed
            .is_truncated
            .as_deref()
            .is_some_and(|flag| flag.eq_ignore_ascii_case("true")),
    })
}

#[derive(Debug, Deserialize)]
struct ListBucketResultXml {
    #[serde(rename = "IsTruncated", default)]
    is_truncated: Option<String>,
    #[serde(rename = "Contents", default)]
    contents: Vec<ListingContentsXml>,
    #[serde(rename = "CommonPrefixes", default)]
    common_prefixes: Vec<ListingPrefixXml>,
}

#[derive(Debug, Deserialize)]
struct ListingContentsXml {
    #[serde(rename = "Key")]
    key: String,
}

#[derive(Debug, Deserialize)]
struct ListingPrefixXml {
    #[serde(rename = "Prefix")]
    prefix: String,
}

// ---------------------------------------------------------------------------
// JMA Japan (NICT-mirrored polar-coordinate GRIB2 tars)
// ---------------------------------------------------------------------------

/// NICT public mirror of the JMA polar-coordinates radar GRIB2 feed.
const JMA_BASE_URL: &str = "https://pawr.nict.go.jp/jmadata/JMA-PolarCoordsRadar";
/// Reflectivity tar (`Pze` members). The sibling `N6` tar at the same stamp
/// carries radial velocity (`Pvr`).
const JMA_REFLECTIVITY_PRODUCT: &str = "N5";
/// Tar stamps are aligned to 5-minute boundaries.
const JMA_STAMP_STEP_MINUTES: i64 = 5;
/// How far back `latest` probes for the newest published tar. Publication
/// lags a few minutes and (observed live) some 5-minute slots are skipped,
/// so the window spans several slots.
const JMA_LOOKBACK_MINUTES: i64 = 40;

/// Japan Meteorological Agency operational radar network, via the NICT
/// public mirror of the JMA polar-coordinates GRIB2 feed
/// (`Z__C_RJTD_{stamp}_RDR_JMAGPV_{N5|N6}_grib2.tar`, JMA GRIB2 templates
/// 3.50120/4.51022/5.200 per the JMA technical format documentation).
///
/// Catalog model: one tar carries every station of the network, so
/// [`IntlProvider::list_sites`] downloads the newest reflectivity tar once,
/// decodes only the per-station GRIB2 headers
/// (`nexrad_io::jma::jma_tar_station_headers`), and caches the station list
/// in-memory for the life of the process. [`IntlProvider::latest`] HEAD-
/// probes backward over [`JMA_LOOKBACK_MINUTES`] of 5-minute stamps for the
/// newest tar that exists.
///
/// Decode contract: the plan's single part is the N5 (reflectivity) tar
/// containing ALL stations — the poll consumer must decode it with
/// `nexrad_io::jma::decode_jma_tar_volumes(bytes, Some(site_id))`; the
/// generic `decode_supported_volume_bytes` router would return the tar's
/// first station regardless of the selection. Consumers that also want
/// Doppler velocity can fetch the `_N6_` sibling at the same stamp and
/// merge per elevation.
pub struct JmaProvider;

/// Process-lifetime station-list cache: the JMA network is static within a
/// session and rebuilding it costs a full tar download.
fn jma_site_cache() -> &'static Mutex<Option<Vec<IntlSite>>> {
    static CACHE: OnceLock<Mutex<Option<Vec<IntlSite>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

fn jma_tar_url(product: &str, stamp: DateTime<Utc>) -> String {
    format!(
        "{JMA_BASE_URL}/{:04}/{:02}/{:02}/Z__C_RJTD_{}_RDR_JMAGPV_{product}_grib2.tar",
        stamp.year(),
        stamp.month(),
        stamp.day(),
        stamp.format("%Y%m%d%H%M%S"),
    )
}

/// Candidate stamps to probe, newest first: `now` floored to the 5-minute
/// grid, then one candidate per step back through the lookback window.
fn jma_candidate_stamps(now: DateTime<Utc>, lookback_minutes: i64) -> Vec<DateTime<Utc>> {
    let step_seconds = JMA_STAMP_STEP_MINUTES * 60;
    let floored = now.timestamp() - now.timestamp().rem_euclid(step_seconds);
    (0..=(lookback_minutes.max(0) / JMA_STAMP_STEP_MINUTES))
        .filter_map(|step| DateTime::<Utc>::from_timestamp(floored - step * step_seconds, 0))
        .collect()
}

/// Newest stamp whose reflectivity tar exists on the mirror, by HEAD probe.
fn jma_newest_stamp() -> Result<DateTime<Utc>, String> {
    let mut last_error: Option<String> = None;
    for stamp in jma_candidate_stamps(Utc::now(), JMA_LOOKBACK_MINUTES) {
        let url = jma_tar_url(JMA_REFLECTIVITY_PRODUCT, stamp);
        match crate::url_exists(&url) {
            Ok(true) => return Ok(stamp),
            Ok(false) => {}
            Err(err) => {
                last_error.get_or_insert_with(|| format!("{url}: {err}"));
            }
        }
    }
    Err(match last_error {
        Some(error) => format!(
            "no JMA tar reachable in the last {JMA_LOOKBACK_MINUTES} minutes (first probe error: {error})"
        ),
        None => format!("no JMA tar published in the last {JMA_LOOKBACK_MINUTES} minutes"),
    })
}

/// The frame plan for one already-probed stamp (pure; unit-testable).
fn jma_frame_plan(stamp: DateTime<Utc>, site_id: &str) -> FramePlan {
    FramePlan {
        identity: format!("{}_{site_id}", stamp.format("%Y%m%d%H%M%S")),
        parts: vec![PlanPart {
            url: jma_tar_url(JMA_REFLECTIVITY_PRODUCT, stamp),
        }],
        merge: false,
    }
}

impl IntlProvider for JmaProvider {
    fn id(&self) -> &'static str {
        "jma"
    }

    fn label(&self) -> &'static str {
        "JMA Japan"
    }

    fn country(&self) -> &'static str {
        "Japan"
    }

    fn list_sites(&self) -> Result<Vec<IntlSite>, String> {
        if let Ok(cache) = jma_site_cache().lock()
            && let Some(sites) = cache.as_ref()
        {
            return Ok(sites.clone());
        }

        let stamp = jma_newest_stamp()?;
        let url = jma_tar_url(JMA_REFLECTIVITY_PRODUCT, stamp);
        let bytes = crate::fetch_volume_bytes(&url)
            .map_err(|err| format!("JMA station catalog download failed ({url}): {err}"))?;
        let stations = nexrad_io::jma::jma_tar_station_headers(&bytes)
            .map_err(|err| format!("JMA station catalog decode failed ({url}): {err}"))?;

        let mut sites: Vec<IntlSite> = stations
            .into_iter()
            .map(|station| IntlSite {
                provider_id: self.id(),
                site_id: station.id.clone(),
                label: format!("{} (RS{})", station.id, station.number),
                country: self.country(),
                latitude_deg: Some(station.latitude_deg as f32),
                longitude_deg: Some(station.longitude_deg as f32),
            })
            .collect();
        sites.sort_by(|left, right| left.site_id.cmp(&right.site_id));
        sites.dedup_by(|left, right| left.site_id == right.site_id);

        if let Ok(mut cache) = jma_site_cache().lock() {
            *cache = Some(sites.clone());
        }
        Ok(sites)
    }

    fn latest(&self, site_id: &str) -> Result<FramePlan, String> {
        let sites = self.list_sites()?;
        if !sites.iter().any(|site| site.site_id == site_id) {
            return Err(format!("unknown JMA site '{site_id}'"));
        }
        Ok(jma_frame_plan(jma_newest_stamp()?, site_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// Compile-time proof the trait stays object safe and thread-shareable
    /// (the registry and the poller both rely on `Box<dyn IntlProvider>`
    /// crossing threads).
    fn assert_provider_box_is_send_sync<T: Send + Sync + ?Sized>() {}

    struct FakeProvider;

    impl IntlProvider for FakeProvider {
        fn id(&self) -> &'static str {
            "fake"
        }

        fn label(&self) -> &'static str {
            "Fake Provider"
        }

        fn country(&self) -> &'static str {
            "Nowhere"
        }

        fn list_sites(&self) -> Result<Vec<IntlSite>, String> {
            Ok(vec![IntlSite {
                provider_id: self.id(),
                site_id: "nwsit".to_owned(),
                label: "Nowhere Site".to_owned(),
                country: self.country(),
                latitude_deg: Some(55.5),
                longitude_deg: Some(12.0),
            }])
        }

        fn latest(&self, site_id: &str) -> Result<FramePlan, String> {
            if site_id != "nwsit" {
                return Err(format!("unknown site '{site_id}'"));
            }
            Ok(FramePlan {
                identity: "nwsit_202606110000".to_owned(),
                parts: vec![PlanPart {
                    url: "https://example.invalid/nwsit_202606110000.h5".to_owned(),
                }],
                merge: false,
            })
        }
    }

    #[test]
    fn registry_lists_every_provider_with_unique_stable_ids() {
        let providers = intl_providers();
        let ids = providers
            .iter()
            .map(|provider| provider.id())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            vec![
                "smhi",
                "dmi",
                "geosphere",
                "fmi",
                "shmu",
                "dwd",
                "chmi",
                "jma"
            ]
        );
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "provider ids must be unique");
        for provider in &providers {
            assert!(!provider.label().is_empty());
            assert!(!provider.country().is_empty());
        }
        assert_provider_box_is_send_sync::<dyn IntlProvider>();
    }

    #[test]
    fn site_cache_memoizes_success_and_retries_after_errors() {
        let cache = SiteCache::new();
        let err = cache.get_or_fill(|| Err("offline".to_owned())).unwrap_err();
        assert_eq!(err, "offline");

        let site = IntlSite {
            provider_id: "fake",
            site_id: "nwsit".to_owned(),
            label: "Nowhere Site".to_owned(),
            country: "Nowhere",
            latitude_deg: None,
            longitude_deg: None,
        };
        let filled = cache
            .get_or_fill(|| Ok(vec![site.clone()]))
            .expect("fill succeeds");
        assert_eq!(filled, vec![site.clone()]);

        // Second call must serve the cache, not the (failing) closure.
        let cached = cache
            .get_or_fill(|| Err("must not be called".to_owned()))
            .expect("cache hit");
        assert_eq!(cached, vec![site]);
    }

    #[test]
    fn s3_style_listing_url_builds_expected_queries() {
        assert_eq!(
            s3_style_listing_url("https://bucket.example", "a/b/", Some("/"), None, 1000),
            "https://bucket.example/?list-type=2&max-keys=1000&prefix=a/b/&delimiter=/"
        );
        assert_eq!(
            s3_style_listing_url(
                "https://bucket.example",
                "a/",
                None,
                Some("a/k_0001.hdf"),
                100
            ),
            "https://bucket.example/?list-type=2&max-keys=100&prefix=a/&start-after=a/k_0001.hdf"
        );
    }

    #[test]
    fn jma_tar_url_follows_the_nict_layout() {
        let stamp = chrono::Utc.with_ymd_and_hms(2026, 6, 12, 6, 40, 0).unwrap();
        assert_eq!(
            jma_tar_url("N5", stamp),
            "https://pawr.nict.go.jp/jmadata/JMA-PolarCoordsRadar/2026/06/12/\
             Z__C_RJTD_20260612064000_RDR_JMAGPV_N5_grib2.tar"
        );
        assert_eq!(
            jma_tar_url("N6", stamp),
            "https://pawr.nict.go.jp/jmadata/JMA-PolarCoordsRadar/2026/06/12/\
             Z__C_RJTD_20260612064000_RDR_JMAGPV_N6_grib2.tar"
        );
    }

    #[test]
    fn s3_style_listing_parser_reads_keys_prefixes_and_truncation() {
        // Recorded from the live GeoSphere datahub probe (2026-06-12),
        // trimmed to three Contents entries; IsTruncated/continuation kept.
        let truncated = parse_s3_style_listing(include_str!(
            "international/fixtures/geosphere_listing_truncated.xml"
        ))
        .expect("truncated fixture parses");
        assert!(truncated.is_truncated);
        assert!(truncated.common_prefixes.is_empty());
        assert_eq!(truncated.keys.len(), 3);
        assert_eq!(
            truncated.keys[0],
            "resources/radar_volumen_hochficht-v1-5min/filelisting/WXRHOF_202606100000.hdf"
        );

        // Recorded from the live FMI bucket probe (2026-06-12): a delimited
        // listing that answers with CommonPrefixes only.
        let delimited =
            parse_s3_style_listing(include_str!("international/fixtures/fmi_site_prefixes.xml"))
                .expect("delimited fixture parses");
        assert!(!delimited.is_truncated);
        assert!(delimited.keys.is_empty());
        assert_eq!(delimited.common_prefixes.len(), 12);
        assert_eq!(delimited.common_prefixes[0], "2026/06/12/fianj/");

        let err = parse_s3_style_listing("not xml").unwrap_err();
        assert!(err.contains("parse failed"), "unexpected error: {err}");
    }

    #[test]
    fn jma_candidate_stamps_walk_the_five_minute_grid_newest_first() {
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 6, 12, 6, 43, 17)
            .unwrap();
        let stamps = jma_candidate_stamps(now, 40);
        assert_eq!(stamps.len(), 9, "0..=40 minutes in 5-minute steps");
        assert_eq!(
            stamps[0],
            chrono::Utc.with_ymd_and_hms(2026, 6, 12, 6, 40, 0).unwrap(),
            "now floors onto the grid"
        );
        assert_eq!(
            *stamps.last().unwrap(),
            chrono::Utc.with_ymd_and_hms(2026, 6, 12, 6, 0, 0).unwrap()
        );
        for pair in stamps.windows(2) {
            assert_eq!(
                (pair[0] - pair[1]).num_minutes(),
                JMA_STAMP_STEP_MINUTES,
                "strictly descending in 5-minute steps"
            );
        }
        // A day boundary keeps the date path consistent with the stamp.
        let midnight_probe = jma_candidate_stamps(
            chrono::Utc.with_ymd_and_hms(2026, 6, 12, 0, 2, 0).unwrap(),
            10,
        );
        assert_eq!(
            jma_tar_url("N5", midnight_probe[1]),
            "https://pawr.nict.go.jp/jmadata/JMA-PolarCoordsRadar/2026/06/11/\
             Z__C_RJTD_20260611235500_RDR_JMAGPV_N5_grib2.tar"
        );
    }

    /// Live NICT roundtrip: site list from the newest tar, frame plan via
    /// HEAD probes, tar download, and the documented site-filtered decode.
    /// Network test; run with:
    /// `cargo test -p data_source jma_live -- --ignored --nocapture`
    #[test]
    #[ignore = "live NICT endpoint probe — run manually with --ignored"]
    fn jma_live_roundtrip_lists_plans_downloads_and_decodes() {
        let provider = JmaProvider;
        let sites = provider.list_sites().expect("live JMA site list");
        assert!(!sites.is_empty(), "JMA tar must list stations");
        println!("{} JMA sites, first={:?}", sites.len(), sites[0]);

        let site = &sites[0];
        let plan = provider.latest(&site.site_id).expect("live JMA frame plan");
        assert!(!plan.merge);
        assert_eq!(plan.parts.len(), 1);
        println!("plan identity={} url={}", plan.identity, plan.parts[0].url);

        let bytes = crate::fetch_volume_bytes(&plan.parts[0].url).expect("live tar download");
        let volumes = nexrad_io::jma::decode_jma_tar_volumes(&bytes, Some(&site.site_id))
            .expect("site-filtered decode");
        assert_eq!(volumes.len(), 1, "filter must select exactly one station");
        assert_eq!(volumes[0].site.id, site.site_id);
        assert!(!volumes[0].cuts.is_empty());
        println!(
            "decoded {} at {}: {} cuts, {} radials",
            volumes[0].site.id,
            volumes[0].volume_time,
            volumes[0].cuts.len(),
            volumes[0].metadata.decoded_radial_count
        );
    }

    #[test]
    fn jma_frame_plan_is_a_single_unmerged_tar_with_a_stable_identity() {
        let stamp = chrono::Utc.with_ymd_and_hms(2026, 6, 12, 6, 40, 0).unwrap();
        let plan = jma_frame_plan(stamp, "ITOK");
        assert_eq!(plan.identity, "20260612064000_ITOK");
        assert!(!plan.merge);
        assert_eq!(plan.parts.len(), 1);
        assert!(plan.parts[0].url.ends_with("_N5_grib2.tar"));
        // Same upstream frame -> same plan (dedupe key stability).
        assert_eq!(jma_frame_plan(stamp, "ITOK"), plan);
    }

    #[test]
    fn trait_contract_round_trips_through_a_boxed_provider() {
        let provider: Box<dyn IntlProvider> = Box::new(FakeProvider);
        let sites = provider.list_sites().unwrap();
        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].provider_id, provider.id());

        let plan = provider.latest(&sites[0].site_id).unwrap();
        assert!(!plan.merge);
        assert_eq!(plan.parts.len(), 1);
        // Same upstream frame -> same identity (dedupe key stability).
        assert_eq!(provider.latest(&sites[0].site_id).unwrap(), plan);

        let err = provider.latest("missing").unwrap_err();
        assert!(err.contains("missing"));
    }
}
