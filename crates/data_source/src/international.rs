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

use chrono::{DateTime, Datelike, NaiveDate, Utc};
use serde::Deserialize;

mod australia_nci;
mod chmi;
mod dmi;
mod dwd;
mod fmi;
mod geosphere;
mod kaia;
pub mod listing;
mod lombardia;
mod ord;
mod piemonte;
mod shmu;
mod smhi;

pub use australia_nci::AustraliaNciProvider;
pub use chmi::ChmiProvider;
pub use dmi::DmiProvider;
pub use dwd::DwdProvider;
pub use fmi::FmiProvider;
pub use geosphere::GeoSphereProvider;
pub use kaia::KaiaEstoniaProvider;
pub use lombardia::LombardiaProvider;
pub use ord::{OrdArchivePlan, OrdProvider, archive_plan_nearest, archive_plans_for_hour};
pub use piemonte::PiemonteProvider;
pub use shmu::ShmuProvider;
pub use smhi::{SmhiProvider, smhi_archive_plans_for_day};

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

/// Rolling multi-frame `recent` support, for providers whose upstream
/// catalog exposes more than the newest frame. Implemented on the provider
/// type and handed back through [`IntlProvider::recent_source`], which is
/// what routes [`IntlProvider::recent`] here and drives the derived
/// [`IntlProvider::supports_recent`] capability.
pub trait RecentFrames {
    /// Same contract as [`IntlProvider::recent`]: up to `count` frames,
    /// OLDEST FIRST, catalog probes only — never volume downloads.
    fn recent_frames(&self, site_id: &str, count: usize) -> Result<Vec<FramePlan>, String>;
}

/// Historical archive lookup, for providers whose upstream exposes dated
/// holdings beyond the rolling live window. Implemented on the provider
/// type and handed back through [`IntlProvider::archive_source`], which is
/// what drives the derived [`IntlProvider::supports_archive`] capability —
/// the exact mirror of the proven [`RecentFrames`]/
/// [`IntlProvider::recent_source`] pattern.
pub trait ArchiveFrames {
    /// All frames anchored on the UTC calendar date `date_utc` for
    /// `site_id`, OLDEST FIRST. Same cheapness contract as
    /// [`IntlProvider::recent`]: catalog probes only — never volume
    /// downloads.
    fn day_plans(&self, site_id: &str, date_utc: NaiveDate) -> Result<Vec<FramePlan>, String>;

    /// Frames inside `[start, end]`, OLDEST FIRST, capped to the NEWEST
    /// `max` (the frames nearest the window's end anchor — the
    /// loop-ending-at-scan shape). Provided: folds [`Self::day_plans`]
    /// over every UTC date the window touches. [`FramePlan`]s carry no
    /// timestamp, so the default trims at day granularity plus the count
    /// cap — boundary-date frames outside the window ride along;
    /// hour-granular listers (ORD) override for tight windows. Days that
    /// error are skipped and the first error is reported only when the
    /// whole window yields nothing (a partial archive loop beats none).
    fn window_plans(
        &self,
        site_id: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        max: usize,
    ) -> Result<Vec<FramePlan>, String> {
        if end < start {
            return Err(format!("archive window end {end} precedes start {start}"));
        }
        if max == 0 {
            return Ok(Vec::new());
        }
        let mut plans = Vec::new();
        let mut first_error: Option<String> = None;
        let mut date = start.date_naive();
        let last_date = end.date_naive();
        loop {
            match self.day_plans(site_id, date) {
                Ok(mut day) => plans.append(&mut day),
                Err(err) => {
                    first_error.get_or_insert(err);
                }
            }
            if date >= last_date {
                break;
            }
            match date.succ_opt() {
                Some(next) => date = next,
                None => break,
            }
        }
        if plans.is_empty() {
            return Err(first_error.unwrap_or_else(|| {
                format!("no archive frames for '{site_id}' between {start} and {end}")
            }));
        }
        plans.dedup_by(|left, right| left.identity == right.identity);
        let skip = plans.len().saturating_sub(max);
        Ok(plans.split_off(skip))
    }
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

    /// Describe up to `count` recent frames for `site_id`, OLDEST FIRST
    /// (install order — the newest frame is last, and its identity is what
    /// the live poll dedupes against). Same cheapness contract as
    /// [`Self::latest`]: list/inspect the catalog, never download volume
    /// bytes. Provided — never override: providers whose catalogs only
    /// expose the newest frame inherit a one-frame "loop", and providers
    /// with a rolling archive implement [`RecentFrames`] and hand it back
    /// from [`Self::recent_source`] so the app's Load Loop works on
    /// international feeds the way it does on US ones (field request).
    fn recent(&self, site_id: &str, count: usize) -> Result<Vec<FramePlan>, String> {
        match self.recent_source() {
            Some(source) => source.recent_frames(site_id, count),
            None => Ok(vec![self.latest(site_id)?]),
        }
    }

    /// The provider's rolling-window loop implementation, when its upstream
    /// catalog exposes more than the newest frame. THE single override
    /// point for multi-frame Load Loop support: implement [`RecentFrames`]
    /// on the provider type and return `Some(self)` here. Both
    /// [`Self::recent`] and the derived [`Self::supports_recent`]
    /// capability route through this method, so a provider cannot gain a
    /// real loop without advertising it (nor advertise one it lacks).
    fn recent_source(&self) -> Option<&dyn RecentFrames> {
        None
    }

    /// Whether [`Self::recent`] returns a real multi-frame window. Derived
    /// from [`Self::recent_source`] — never override.
    fn supports_recent(&self) -> bool {
        self.recent_source().is_some()
    }

    /// The provider's historical archive implementation, when its upstream
    /// exposes dated holdings. THE single override point for archive
    /// lookup: implement [`ArchiveFrames`] on the provider type and return
    /// `Some(self)` here. Both archive routing and the derived
    /// [`Self::supports_archive`] capability go through this method, so a
    /// provider cannot gain a real archive without advertising it (nor
    /// advertise one it lacks).
    fn archive_source(&self) -> Option<&dyn ArchiveFrames> {
        None
    }

    /// Whether this provider offers historical archive lookup. Derived
    /// from [`Self::archive_source`] — never override.
    fn supports_archive(&self) -> bool {
        self.archive_source().is_some()
    }

    /// The provider's EMBEDDED site catalog: every currently operational
    /// site with its static coordinates, straight from the provider's
    /// compiled-in table. Never touches the network, so it is safe on the
    /// UI thread — this is what map markers draw from before any poll or
    /// catalog fetch has happened. May lag reality (a brand-new radar
    /// appears here only after a table refresh), so pickers wanting
    /// freshness still call [`Self::list_sites`]. Contract: every returned
    /// site has `Some` finite latitude/longitude inside its country.
    fn static_sites(&self) -> Vec<IntlSite>;
}

/// Registry of all built-in international providers.
///
/// Single-file ODIM PVOL feeds (one HDF5 download per frame): SMHI Sweden,
/// DMI Denmark, GeoSphere Austria, FMI Finland. Split-volume assembly
/// feeds (one frame = several ODIM files merged with
/// `radar_core::merge_radar_volumes`): SHMU Slovakia, DWD Germany (REF+VEL
/// by default), CHMI Czechia. Multi-station tar feed (site-filtered decode, see
/// [`JmaProvider`]): JMA Japan. Single-site KAIA bridge for Estonia's Harku
/// radar, which is not currently present in ORD's rolling cache:
/// [`KaiaEstoniaProvider`]. Multi-country feed mixing single-file and split
/// plan shapes per site: EUMETNET ORD ([`OrdProvider`], 14 European countries
/// without a national BowEcho provider).
pub fn intl_providers() -> Vec<Box<dyn IntlProvider>> {
    vec![
        Box::new(SmhiProvider::new()),
        Box::new(AustraliaNciProvider::new()),
        Box::new(DmiProvider::new()),
        Box::new(GeoSphereProvider::new()),
        Box::new(FmiProvider::new()),
        Box::new(ShmuProvider::new()),
        Box::new(DwdProvider::new()),
        Box::new(ChmiProvider::new()),
        Box::new(PiemonteProvider::new()),
        Box::new(LombardiaProvider::new()),
        Box::new(JmaProvider),
        Box::new(KaiaEstoniaProvider::new()),
        Box::new(OrdProvider::new()),
    ]
}

/// Every provider's embedded static site table, flattened in registry
/// order and memoized for the life of the process.
///
/// This is the map-marker catalog: a pure function over the providers'
/// compiled-in tables ([`IntlProvider::static_sites`]), it never touches
/// the network and is therefore safe to call on the UI thread every frame.
pub fn intl_static_sites() -> &'static [IntlSite] {
    static SITES: OnceLock<Vec<IntlSite>> = OnceLock::new();
    SITES.get_or_init(|| {
        intl_providers()
            .iter()
            .flat_map(|provider| provider.static_sites())
            .collect()
    })
}

/// User-facing capability summary for one built-in international provider.
/// This is intentionally conservative: dynamic probe results may prove a
/// specific site/date works, but these labels describe what BowEcho can offer
/// predictably from the adapter contract.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IntlProviderCapability {
    pub provider_id: &'static str,
    pub provider_label: &'static str,
    pub country: &'static str,
    pub visible_sites: usize,
    pub live: bool,
    /// Derived from [`IntlProvider::supports_recent`] — `true` iff the
    /// provider implements a real multi-frame [`IntlProvider::recent`]
    /// (never hand-maintained, so it cannot go stale against the code).
    pub recent_loop: bool,
    /// Derived from [`IntlProvider::supports_archive`] — `true` iff the
    /// provider hands back a real [`ArchiveFrames`] from
    /// [`IntlProvider::archive_source`] (never hand-maintained; the
    /// parity audit caught the hand-kept flag lying in both directions —
    /// SMHI false above its own working day loader, NCI true without a
    /// dated lookup).
    pub archive_lookup: bool,
    pub current_window: &'static str,
    pub upstream_window: &'static str,
    pub bowecho_status: &'static str,
    pub next_unlock: &'static str,
}

/// Capability cards for the Data tab coverage explorer and diagnostics. The
/// site count is read from the same embedded tables that draw markers.
pub fn intl_provider_capabilities() -> Vec<IntlProviderCapability> {
    intl_providers()
        .into_iter()
        .map(|provider| {
            let visible_sites = provider.static_sites().len();
            let (current_window, upstream_window, bowecho_status, next_unlock) = match provider.id()
            {
                "smhi" => (
                    "dated tree: recent frames + whole-day archive",
                    "year/month/day qcvol tree; observed 2025-2026 by site",
                    "recent loop and day archive lookup from the dated tree",
                    "arbitrary date/window picker over the dated tree",
                ),
                "fmi" => (
                    "today + yesterday",
                    "public ODIM HDF5 archive, roughly 2007-present",
                    "recent loop only",
                    "walk historical date prefixes",
                ),
                "australia-nci" => (
                    "dated tarlists: recent frames + day/window archive, ~3 days delayed",
                    "NCI rq0 ODIM HDF5 archive; daily tarlists and direct zip-member reads",
                    "recent loop and dated archive lookup from the daily tarlists; NCI \
                     ingests BOM data ~3 days behind real time by design",
                    "arbitrary date/window picker over the multi-year tarlists",
                ),
                "dmi" => (
                    "newest N STAC items",
                    "STAC date ranges with pagination",
                    "recent loop from the STAC items query",
                    "use STAC date-range queries for archive lookup",
                ),
                "geosphere" => (
                    "newest N frames of the rolling window",
                    "rolling ~3 days",
                    "recent loop from the rolling listing",
                    "add date/window picker over the rolling days",
                ),
                "dwd" => (
                    "newest N 5-minute sweep cycles",
                    "rolling ~2 days",
                    "recent loop from timestamped sweep files",
                    "add date/window picker over the rolling days",
                ),
                "shmu" => (
                    "newest N frames, dated directories",
                    "observed rolling ~1 month",
                    "recent loop from the dated directories",
                    "add arbitrary date/window picker",
                ),
                "chmi" => (
                    "newest N frames of the rolling window",
                    "observed rolling ~89 hours",
                    "recent loop from the volume file listings",
                    "add date/window picker over the rolling window",
                ),
                "arpa-piemonte" => (
                    "last hour",
                    "rolling last hour of OPERA HDF5 volumes",
                    "real in-app recent loop",
                    "add historical/event access if ARPA exposes it",
                ),
                "arpa-lombardia" => (
                    "rolling live window",
                    "gzip-wrapped ODIM HDF5 product volumes",
                    "real in-app recent loop",
                    "add historical/event access if ARPA exposes it",
                ),
                "kaia" => (
                    "14 days",
                    "historical repository likely deeper, not guaranteed",
                    "real in-app recent archive",
                    "probe longer retention",
                ),
                "ord" => (
                    "rolling 24h + per-site archive lookup",
                    "ORD single-site archive is partial/opportunistic; OPERA composites separate",
                    "recent loop and date/hour lookup",
                    "cache per-site coverage probes",
                ),
                "jma" => (
                    "latest only",
                    "NICT mirror recent operational tars",
                    "latest frame only",
                    "add tar directory scan if needed",
                ),
                _ => (
                    "latest only",
                    "unknown",
                    "latest frame only",
                    "provider-specific probe",
                ),
            };
            IntlProviderCapability {
                provider_id: provider.id(),
                provider_label: provider.label(),
                country: provider.country(),
                visible_sites,
                live: true,
                recent_loop: provider.supports_recent(),
                archive_lookup: provider.supports_archive(),
                current_window,
                upstream_window,
                bowecho_status,
                next_unlock,
            }
        })
        .collect()
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
/// Radial-velocity tar (`Pvr` members), same stamp/stations as `N5` when
/// published.
const JMA_VELOCITY_PRODUCT: &str = "N6";
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
/// Decode contract: the plan's first part is the N5 (reflectivity) tar
/// containing ALL stations — the poll consumer must decode JMA parts with
/// `nexrad_io::jma::decode_jma_tar_volumes(bytes, Some(site_id))`; the
/// generic `decode_supported_volume_bytes` router would return the tar's
/// first station regardless of the selection. When the `_N6_` sibling exists
/// at the same stamp, the plan includes it and requests a per-elevation merge
/// so Japan exposes Doppler velocity in the same live frame.
pub struct JmaProvider;

/// JMA operational radar stations: id, station number, latitude, longitude.
///
/// Decoded from the live N5 reflectivity tar
/// `Z__C_RJTD_20260612083000_RDR_JMAGPV_N5_grib2.tar` (NICT mirror, fetched
/// 2026-06-12) via `nexrad_io::jma::jma_tar_station_headers` — the same
/// per-station GRIB2 product-section headers (JMA GRIB2 template 4.51022
/// per the JMA technical format documentation) that the live catalog path
/// reads, so the static table and a live listing agree on ids and
/// coordinates. Regenerate by running
/// `cargo test -p data_source jma_regenerate_static_station_table -- --ignored --nocapture`
/// and pasting the printed rows.
const JMA_STATIONS: &[(&str, u16, f32, f32)] = &[
    ("AKIT", 47582, 39.7178, 140.0994),
    ("FUNC", 47909, 28.3942, 129.5519),
    ("HAIG", 47792, 34.2703, 132.5933),
    ("HAKO", 47432, 41.9336, 140.7814),
    ("ISHI", 47920, 24.4267, 124.1822),
    ("ITOK", 47937, 26.1533, 127.765),
    ("KASH", 47695, 35.8597, 139.9597),
    ("KURU", 47611, 36.1031, 138.1958),
    ("KUSH", 47419, 42.9608, 144.5175),
    ("MAKI", 47659, 34.7428, 138.1336),
    ("MISA", 47791, 35.5417, 133.1036),
    ("MURO", 47899, 33.2525, 134.1772),
    ("NAGO", 47636, 35.1681, 136.9653),
    ("SAPP", 47415, 43.1389, 141.0097),
    ("SEFU", 47806, 33.4344, 130.3569),
    ("SEND", 47590, 38.2622, 140.8972),
    ("TAKA", 47773, 34.6164, 135.6564),
    ("TANE", 47869, 30.6397, 130.9792),
    ("TOJI", 47705, 36.2375, 136.1422),
    ("YAHI", 47572, 37.7186, 138.8161),
];

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

fn jma_velocity_available(stamp: DateTime<Utc>) -> bool {
    crate::url_exists(&jma_tar_url(JMA_VELOCITY_PRODUCT, stamp)).unwrap_or(false)
}

/// The frame plan for one already-probed stamp (pure; unit-testable).
fn jma_frame_plan(stamp: DateTime<Utc>, site_id: &str, include_velocity: bool) -> FramePlan {
    let mut parts = vec![PlanPart {
        url: jma_tar_url(JMA_REFLECTIVITY_PRODUCT, stamp),
    }];
    if include_velocity {
        parts.push(PlanPart {
            url: jma_tar_url(JMA_VELOCITY_PRODUCT, stamp),
        });
    }
    let products = if include_velocity { "N5_N6" } else { "N5" };
    FramePlan {
        identity: format!("{}_{site_id}_{products}", stamp.format("%Y%m%d%H%M%S")),
        parts,
        merge: include_velocity,
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

        let live = (|| -> Result<Vec<IntlSite>, String> {
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
            Ok(sites)
        })();

        match live {
            Ok(sites) => {
                // Only a LIVE answer is cached: the static fallback below
                // must not stop a later call from retrying the (fresher,
                // authoritative) tar headers.
                if let Ok(mut cache) = jma_site_cache().lock() {
                    *cache = Some(sites.clone());
                }
                Ok(sites)
            }
            // Mirror unreachable: seed the picker from the embedded table
            // so Japan stays selectable offline-of-NICT; site ids match
            // the tar headers, so a selection made from this list polls
            // identically once the mirror answers.
            Err(_) => Ok(self.static_sites()),
        }
    }

    fn latest(&self, site_id: &str) -> Result<FramePlan, String> {
        let sites = self.list_sites()?;
        if !sites.iter().any(|site| site.site_id == site_id) {
            return Err(format!("unknown JMA site '{site_id}'"));
        }
        let stamp = jma_newest_stamp()?;
        Ok(jma_frame_plan(
            stamp,
            site_id,
            jma_velocity_available(stamp),
        ))
    }

    fn static_sites(&self) -> Vec<IntlSite> {
        JMA_STATIONS
            .iter()
            .map(|&(id, number, latitude_deg, longitude_deg)| IntlSite {
                provider_id: self.id(),
                site_id: id.to_owned(),
                // Same label grammar as the live tar-derived catalog.
                label: format!("{id} (RS{number})"),
                country: self.country(),
                latitude_deg: Some(latitude_deg),
                longitude_deg: Some(longitude_deg),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::collections::BTreeSet;

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

        fn static_sites(&self) -> Vec<IntlSite> {
            self.list_sites().unwrap_or_default()
        }
    }

    /// A provider with a rolling window: implements [`RecentFrames`] and
    /// hands it back from `recent_source` — the one act that must both
    /// route `recent()` and flip `supports_recent()`.
    struct FakeLoopProvider;

    impl RecentFrames for FakeLoopProvider {
        fn recent_frames(&self, site_id: &str, count: usize) -> Result<Vec<FramePlan>, String> {
            Ok((0..count)
                .map(|index| FramePlan {
                    identity: format!("{site_id}_frame_{index}"),
                    parts: vec![PlanPart {
                        url: format!("https://example.invalid/{site_id}_frame_{index}.h5"),
                    }],
                    merge: false,
                })
                .collect())
        }
    }

    impl IntlProvider for FakeLoopProvider {
        fn id(&self) -> &'static str {
            "fake-loop"
        }

        fn label(&self) -> &'static str {
            "Fake Loop Provider"
        }

        fn country(&self) -> &'static str {
            "Nowhere"
        }

        fn list_sites(&self) -> Result<Vec<IntlSite>, String> {
            FakeProvider.list_sites()
        }

        fn latest(&self, site_id: &str) -> Result<FramePlan, String> {
            FakeProvider.latest(site_id)
        }

        fn recent_source(&self) -> Option<&dyn RecentFrames> {
            Some(self)
        }

        fn static_sites(&self) -> Vec<IntlSite> {
            self.list_sites().unwrap_or_default()
        }
    }

    /// A provider with a dated archive: implements [`ArchiveFrames`] and
    /// hands it back from `archive_source` — the one act that must both
    /// route archive lookups and flip `supports_archive()`.
    struct FakeArchiveProvider;

    impl ArchiveFrames for FakeArchiveProvider {
        fn day_plans(&self, site_id: &str, date_utc: NaiveDate) -> Result<Vec<FramePlan>, String> {
            if site_id != "nwsit" {
                return Err(format!("unknown site '{site_id}'"));
            }
            Ok((0..2)
                .map(|index| {
                    let stamp = date_utc.format("%Y%m%d");
                    FramePlan {
                        identity: format!("{site_id}_{stamp}_{index}"),
                        parts: vec![PlanPart {
                            url: format!("https://example.invalid/{site_id}_{stamp}_{index}.h5"),
                        }],
                        merge: false,
                    }
                })
                .collect())
        }
    }

    impl IntlProvider for FakeArchiveProvider {
        fn id(&self) -> &'static str {
            "fake-archive"
        }

        fn label(&self) -> &'static str {
            "Fake Archive Provider"
        }

        fn country(&self) -> &'static str {
            "Nowhere"
        }

        fn list_sites(&self) -> Result<Vec<IntlSite>, String> {
            FakeProvider.list_sites()
        }

        fn latest(&self, site_id: &str) -> Result<FramePlan, String> {
            FakeProvider.latest(site_id)
        }

        fn archive_source(&self) -> Option<&dyn ArchiveFrames> {
            Some(self)
        }

        fn static_sites(&self) -> Vec<IntlSite> {
            self.list_sites().unwrap_or_default()
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
                "australia-nci",
                "dmi",
                "geosphere",
                "fmi",
                "shmu",
                "dwd",
                "chmi",
                "arpa-piemonte",
                "arpa-lombardia",
                "jma",
                "kaia",
                "ord"
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
    fn capability_catalog_tracks_registered_providers() {
        let provider_ids = intl_providers()
            .iter()
            .map(|provider| provider.id())
            .collect::<BTreeSet<_>>();
        let capabilities = intl_provider_capabilities();
        let capability_ids = capabilities
            .iter()
            .map(|capability| capability.provider_id)
            .collect::<BTreeSet<_>>();
        assert_eq!(capability_ids, provider_ids);
        assert!(capabilities.iter().all(|capability| capability.live));
        assert!(
            capabilities
                .iter()
                .all(|capability| capability.visible_sites > 0)
        );

        let smhi = capabilities
            .iter()
            .find(|capability| capability.provider_id == "smhi")
            .expect("SMHI capability");
        assert!(smhi.recent_loop);
        assert!(smhi.current_window.contains("dated tree"));
        assert!(
            smhi.archive_lookup,
            "SMHI's card goes honest-true: its day loader now routes \
             through archive_source()"
        );

        let ord = capabilities
            .iter()
            .find(|capability| capability.provider_id == "ord")
            .expect("ORD capability");
        assert!(ord.recent_loop);
        assert!(ord.archive_lookup);

        let nci = capabilities
            .iter()
            .find(|capability| capability.provider_id == "australia-nci")
            .expect("NCI capability");
        assert!(
            nci.archive_lookup,
            "NCI's card goes honest-true: its dated tarlists now route \
             through archive_source()"
        );
        assert!(
            nci.current_window.contains("~3 days delayed")
                && nci.bowecho_status.contains("~3 days behind real time"),
            "the card must keep saying NCI data runs ~3 days late BY DESIGN \
             — the archive works, the delay is the upstream's honesty story"
        );
    }

    /// The Load Loop capability is DERIVED, not hand-maintained:
    /// implementing [`RecentFrames`] and returning it from `recent_source`
    /// is the one act that both routes [`IntlProvider::recent`] to the
    /// rolling window and flips the capability card, so the card can never
    /// go stale against the code. The expected id set is the review
    /// tripwire: a provider gaining or losing a real loop must show up
    /// here deliberately.
    #[test]
    fn recent_loop_capability_is_derived_from_recent_source() {
        let providers = intl_providers();
        let capabilities = intl_provider_capabilities();
        for provider in &providers {
            let capability = capabilities
                .iter()
                .find(|capability| capability.provider_id == provider.id())
                .unwrap_or_else(|| panic!("{}: missing capability card", provider.id()));
            assert_eq!(
                capability.recent_loop,
                provider.supports_recent(),
                "{}: capability card must mirror the provider",
                provider.id()
            );
            assert_eq!(
                provider.supports_recent(),
                provider.recent_source().is_some(),
                "{}: supports_recent must stay derived from recent_source",
                provider.id()
            );
        }
        let loop_ids = providers
            .iter()
            .filter(|provider| provider.supports_recent())
            .map(|provider| provider.id())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            loop_ids,
            BTreeSet::from([
                "arpa-lombardia",
                "arpa-piemonte",
                "australia-nci",
                "chmi",
                "dmi",
                "dwd",
                "fmi",
                "geosphere",
                "kaia",
                "ord",
                "shmu",
                "smhi",
            ]),
            "JMA is the one intentional single-frame holdout (tar/stamp \
             probing is its own task)"
        );
    }

    /// Without a `recent_source`, `recent()` degrades to a one-frame loop
    /// (exactly `latest`) and the provider reports no loop support.
    #[test]
    fn default_recent_is_a_single_frame_and_reports_no_loop_support() {
        let provider = FakeProvider;
        assert!(provider.recent_source().is_none());
        assert!(!provider.supports_recent());
        let plans = provider.recent("nwsit", 5).expect("single-frame fallback");
        assert_eq!(plans, vec![provider.latest("nwsit").unwrap()]);
    }

    /// With a `recent_source`, `recent()` routes to the rolling window and
    /// `supports_recent()` flips true — one override point, two effects.
    #[test]
    fn recent_source_routes_recent_and_flips_supports_recent_together() {
        let provider = FakeLoopProvider;
        assert!(provider.supports_recent());
        let plans = provider.recent("nwsit", 2).expect("rolling window");
        assert_eq!(plans.len(), 2, "must not fall back to a single frame");
        assert_eq!(plans[0].identity, "nwsit_frame_0");
        assert_eq!(plans[1].identity, "nwsit_frame_1");
    }

    /// Without an `archive_source`, a provider honestly reports no
    /// archive lookup.
    #[test]
    fn default_archive_source_is_absent_and_reports_no_archive_support() {
        let provider = FakeProvider;
        assert!(provider.archive_source().is_none());
        assert!(!provider.supports_archive());
    }

    /// With an `archive_source`, day lookups route to the dated archive
    /// and `supports_archive()` flips true — one override point, two
    /// effects (the [`RecentFrames`] pattern, mirrored).
    #[test]
    fn archive_source_routes_day_plans_and_flips_supports_archive_together() {
        let provider = FakeArchiveProvider;
        assert!(provider.supports_archive());
        let date = NaiveDate::from_ymd_opt(2026, 6, 9).expect("date");
        let plans = provider
            .archive_source()
            .expect("archive source")
            .day_plans("nwsit", date)
            .expect("day plans");
        assert_eq!(
            plans
                .iter()
                .map(|plan| plan.identity.as_str())
                .collect::<Vec<_>>(),
            vec!["nwsit_20260609_0", "nwsit_20260609_1"],
            "oldest first"
        );
    }

    /// The provided `window_plans` folds `day_plans` over every UTC date
    /// the window touches, stays oldest-first, and caps to the NEWEST
    /// `max` frames (the loop-ending-at-scan tail).
    #[test]
    fn default_window_plans_folds_days_oldest_first_and_caps_to_the_newest() {
        use chrono::TimeZone;
        let provider = FakeArchiveProvider;
        let source = provider.archive_source().expect("archive source");
        let start = Utc.with_ymd_and_hms(2026, 6, 9, 6, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 6, 10, 18, 0, 0).unwrap();

        let plans = source
            .window_plans("nwsit", start, end, 3)
            .expect("window plans");
        assert_eq!(
            plans
                .iter()
                .map(|plan| plan.identity.as_str())
                .collect::<Vec<_>>(),
            vec!["nwsit_20260609_1", "nwsit_20260610_0", "nwsit_20260610_1"],
            "two folded days, oldest trimmed by the cap"
        );

        assert!(
            source
                .window_plans("nwsit", start, end, 0)
                .expect("empty cap")
                .is_empty()
        );
        let err = source.window_plans("nwsit", end, start, 3).unwrap_err();
        assert!(err.contains("precedes"), "unexpected error: {err}");
        let err = source.window_plans("missing", start, end, 3).unwrap_err();
        assert!(
            err.contains("missing"),
            "an all-error fold must surface the first day error: {err}"
        );
    }

    /// The archive capability is DERIVED, not hand-maintained: handing an
    /// [`ArchiveFrames`] back from `archive_source` is the one act that
    /// both routes archive lookup and flips the capability card, so the
    /// card can never go stale against the code (the parity audit caught
    /// the hand-kept flag lying in both directions). The expected id set
    /// is the review tripwire: a provider gaining or losing a real dated
    /// archive must show up here deliberately.
    #[test]
    fn archive_capability_is_derived_from_archive_source() {
        let providers = intl_providers();
        let capabilities = intl_provider_capabilities();
        for provider in &providers {
            let capability = capabilities
                .iter()
                .find(|capability| capability.provider_id == provider.id())
                .unwrap_or_else(|| panic!("{}: missing capability card", provider.id()));
            assert_eq!(
                capability.archive_lookup,
                provider.supports_archive(),
                "{}: capability card must mirror the provider",
                provider.id()
            );
            assert_eq!(
                provider.supports_archive(),
                provider.archive_source().is_some(),
                "{}: supports_archive must stay derived from archive_source",
                provider.id()
            );
        }
        let archive_ids = providers
            .iter()
            .filter(|provider| provider.supports_archive())
            .map(|provider| provider.id())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            archive_ids,
            BTreeSet::from(["australia-nci", "ord", "smhi"]),
            "FMI bucket walks, DMI STAC, and JMA dated tars are later pure \
             adapter work — each flips its card by landing a real \
             ArchiveFrames impl, and shows up here deliberately"
        );
    }

    /// Coordinate sanity for every provider's EMBEDDED catalog: each static
    /// site has finite coordinates inside a generous national bounding box
    /// (radars sit on national territory; a swapped lat/lon, a missing
    /// minus sign, or a degrees/radians slip all land far outside).
    /// `(lat_min, lat_max, lon_min, lon_max)` for each SITE country — the
    /// multi-country ORD provider spans 14 of them, so the lookup keys off
    /// [`IntlSite::country`] rather than the provider's own label.
    fn national_bounding_box(country: &str) -> Option<(f32, f32, f32, f32)> {
        Some(match country {
            "Sweden" => (55.0, 69.5, 10.5, 24.5),
            "Denmark" => (54.5, 58.0, 8.0, 15.5),
            "Austria" => (46.3, 49.1, 9.4, 17.2),
            "Finland" => (59.5, 70.5, 19.0, 31.8),
            "Slovakia" => (47.7, 49.7, 16.8, 22.6),
            "Germany" => (47.2, 55.1, 5.8, 15.1),
            "Czechia" => (48.5, 51.1, 12.0, 18.9),
            "Italy" => (35.4, 47.3, 6.5, 18.8),
            "Australia" => (-44.0, -10.0, 112.0, 154.5),
            // Japan incl. the southwest island arcs (Okinawa, Ishigaki).
            "Japan" => (24.0, 45.6, 122.5, 146.0),
            // EUMETNET ORD countries (France incl. Corsica).
            "Belgium" => (49.4, 51.6, 2.4, 6.5),
            "Switzerland" => (45.7, 47.9, 5.9, 10.6),
            "Estonia" => (57.4, 59.8, 21.6, 28.3),
            "France" => (41.2, 51.2, -5.3, 9.7),
            "Croatia" => (42.3, 46.6, 13.4, 19.5),
            "Ireland" => (51.3, 55.5, -10.7, -5.9),
            "Iceland" => (63.2, 66.7, -24.6, -13.4),
            "Lithuania" => (53.8, 56.5, 20.9, 26.9),
            "Malta" => (35.7, 36.1, 14.1, 14.6),
            "Netherlands" => (50.7, 53.6, 3.3, 7.3),
            "Norway" => (57.9, 71.3, 4.5, 31.2),
            "Poland" => (49.0, 54.9, 14.1, 24.2),
            "Romania" => (43.6, 48.3, 20.2, 29.8),
            "Slovenia" => (45.4, 46.9, 13.3, 16.6),
            _ => return None,
        })
    }

    #[test]
    fn every_static_site_has_finite_coords_in_its_national_bounding_box() {
        for provider in intl_providers() {
            let sites = provider.static_sites();
            assert!(
                !sites.is_empty(),
                "{}: static catalog must not be empty",
                provider.id()
            );
            for site in &sites {
                let (lat_min, lat_max, lon_min, lon_max) = national_bounding_box(site.country)
                    .unwrap_or_else(|| panic!("no bounding box for {}", site.country));
                assert_eq!(site.provider_id, provider.id());
                assert!(!site.site_id.is_empty() && !site.label.is_empty());
                let (Some(latitude), Some(longitude)) = (site.latitude_deg, site.longitude_deg)
                else {
                    panic!(
                        "{}/{}: static site without coordinates",
                        provider.id(),
                        site.site_id
                    );
                };
                assert!(
                    latitude.is_finite()
                        && longitude.is_finite()
                        && (lat_min..=lat_max).contains(&latitude)
                        && (lon_min..=lon_max).contains(&longitude),
                    "{}/{} ({}): ({latitude}, {longitude}) outside {} box",
                    provider.id(),
                    site.site_id,
                    site.label,
                    site.country
                );
            }
        }
    }

    /// The flattened marker catalog covers every provider and never
    /// duplicates a (provider, site) pair.
    #[test]
    fn intl_static_sites_flattens_every_provider_once() {
        let sites = intl_static_sites();
        for provider in intl_providers() {
            assert!(
                sites.iter().any(|site| site.provider_id == provider.id()),
                "{} missing from intl_static_sites",
                provider.id()
            );
        }
        let mut keys: Vec<(&str, &str)> = sites
            .iter()
            .map(|site| (site.provider_id, site.site_id.as_str()))
            .collect();
        keys.sort_unstable();
        let before = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), before, "duplicate (provider, site) pair");
        // Memoized: repeated calls hand back the same slice.
        assert!(std::ptr::eq(sites, intl_static_sites()));
    }

    /// Regenerates [`JMA_STATIONS`] from the live tar headers. Network
    /// test; run with
    /// `cargo test -p data_source jma_regenerate_static_station_table -- --ignored --nocapture`
    /// and paste the printed rows (and the printed tar URL into the table's
    /// doc comment) when the JMA network changes.
    #[test]
    #[ignore = "live NICT endpoint probe — run manually with --ignored"]
    fn jma_regenerate_static_station_table() {
        let stamp = jma_newest_stamp().expect("newest JMA stamp");
        let url = jma_tar_url(JMA_REFLECTIVITY_PRODUCT, stamp);
        println!("// source tar: {url}");
        let bytes = crate::fetch_volume_bytes(&url).expect("tar download");
        let mut stations =
            nexrad_io::jma::jma_tar_station_headers(&bytes).expect("station headers");
        stations.sort_by(|left, right| left.id.cmp(&right.id));
        for station in &stations {
            println!(
                "    (\"{}\", {}, {:.4}, {:.4}),",
                station.id, station.number, station.latitude_deg, station.longitude_deg
            );
        }
        assert!(!stations.is_empty());
    }

    /// The static table must agree with the live tar headers (same ids,
    /// numbers, and coordinates to table precision). Network test; run with
    /// `cargo test -p data_source jma_static_table -- --ignored --nocapture`
    #[test]
    #[ignore = "live NICT endpoint probe — run manually with --ignored"]
    fn jma_static_table_matches_live_tar_headers() {
        let stamp = jma_newest_stamp().expect("newest JMA stamp");
        let url = jma_tar_url(JMA_REFLECTIVITY_PRODUCT, stamp);
        let bytes = crate::fetch_volume_bytes(&url).expect("tar download");
        let stations = nexrad_io::jma::jma_tar_station_headers(&bytes).expect("station headers");
        assert_eq!(stations.len(), JMA_STATIONS.len(), "station count changed");
        for station in &stations {
            let (_, number, latitude, longitude) = JMA_STATIONS
                .iter()
                .find(|(id, ..)| *id == station.id)
                .unwrap_or_else(|| panic!("{} missing from JMA_STATIONS", station.id));
            assert_eq!(*number, station.number, "{}", station.id);
            assert!((f64::from(*latitude) - station.latitude_deg).abs() < 5e-4);
            assert!((f64::from(*longitude) - station.longitude_deg).abs() < 5e-4);
        }
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
        assert!(plan.parts[0].url.ends_with("_N5_grib2.tar"));
        if plan.merge {
            assert_eq!(plan.parts.len(), 2);
            assert!(plan.parts[1].url.ends_with("_N6_grib2.tar"));
        } else {
            assert_eq!(plan.parts.len(), 1);
        }
        println!("plan identity={} parts={}", plan.identity, plan.parts.len());

        for part in &plan.parts {
            println!("downloading {}", part.url);
            let bytes = crate::fetch_volume_bytes(&part.url).expect("live tar download");
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
    }

    #[test]
    fn jma_frame_plan_without_velocity_is_a_single_unmerged_tar_with_a_stable_identity() {
        let stamp = chrono::Utc.with_ymd_and_hms(2026, 6, 12, 6, 40, 0).unwrap();
        let plan = jma_frame_plan(stamp, "ITOK", false);
        assert_eq!(plan.identity, "20260612064000_ITOK_N5");
        assert!(!plan.merge);
        assert_eq!(plan.parts.len(), 1);
        assert!(plan.parts[0].url.ends_with("_N5_grib2.tar"));
        // Same upstream frame -> same plan (dedupe key stability).
        assert_eq!(jma_frame_plan(stamp, "ITOK", false), plan);
    }

    #[test]
    fn jma_frame_plan_with_velocity_merges_reflectivity_and_velocity_tars() {
        let stamp = chrono::Utc.with_ymd_and_hms(2026, 6, 12, 6, 40, 0).unwrap();
        let plan = jma_frame_plan(stamp, "ITOK", true);
        assert_eq!(plan.identity, "20260612064000_ITOK_N5_N6");
        assert!(plan.merge);
        assert_eq!(plan.parts.len(), 2);
        assert!(plan.parts[0].url.ends_with("_N5_grib2.tar"));
        assert!(plan.parts[1].url.ends_with("_N6_grib2.tar"));
        // Same upstream frame -> same plan (dedupe key stability).
        assert_eq!(jma_frame_plan(stamp, "ITOK", true), plan);
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
