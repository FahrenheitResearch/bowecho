//! Site-domain helpers over the ONE union catalog (v0.29 spec Phase 3).
//!
//! This module is the app-side home for "which radar sites are here /
//! match this query" logic: beam-ranked candidates, the SITE search box
//! rows, nearest-overlay dispatch, and the `SiteRef` encode/resolve
//! helpers. Feature code elsewhere goes through `data_source::sites`
//! (`resolve` / `all_sites` / `sites_near`) or these helpers — raw
//! live-US storage / `intl_static_sites()` iteration is confined here and
//! ratcheted by `tests/sites_catalog_guard.rs`.

use data_source::RadarSite;
use data_source::sites::{SiteKind, SiteRef};

use crate::{
    ViewerApp, custom_poll_entry_label, custom_poll_entry_lat_lon,
    custom_poll_entry_matches_community_feed, format_site_label, haversine_km, site_location,
};

/// One row of the lowest-beam ranking. Identity is a [`SiteRef`] — the
/// v0.28 `BeamTarget::Conus(usize)` catalog index died in v0.29 Phase 3
/// (string refs survive catalog reorder; kind decides the activation
/// path). Beam height at 0.5° is pure geometry, so intl sites rank in
/// the same lowest-beam list as WSR-88Ds (parity: international radars
/// are not second-class).
#[derive(Clone, Debug)]
pub(crate) struct BeamCandidate {
    pub(crate) site: data_source::sites::SiteRef,
    /// Row head: site id (CONUS) or site/marker label.
    pub(crate) label: String,
    /// Weak provenance suffix for non-CONUS rows ("SMHI Sweden",
    /// "research feed"); None for the home catalog.
    pub(crate) origin: Option<String>,
    pub(crate) beam_m: f32,
    pub(crate) distance_km: f32,
}

/// Kind-tagged picker row adapted from the live NOAA Level-II catalog.
///
/// The union catalog is compiled-in and therefore cannot contain a brand-new
/// site discovered by the asynchronous S3 catalog refresh. This adapter keeps
/// that loader boundary in this module while feature UI deals only in stable
/// [`SiteRef`] identity plus catalog-derived [`SiteKind`].
#[derive(Clone, Debug)]
pub(crate) struct LiveUsSiteRow {
    pub(crate) site: SiteRef,
    pub(crate) kind: SiteKind,
    pub(crate) label: String,
}

impl ViewerApp {
    /// The sole adapter onto the app's refreshable legacy US-site storage.
    /// All feature-facing helpers below return stable refs or owned loader
    /// values, so consumers never iterate or retain raw catalog indices.
    fn live_us_sites(&self) -> &[RadarSite] {
        &self.sites
    }

    pub(crate) fn live_us_site_rows(&self) -> Vec<LiveUsSiteRow> {
        self.live_us_sites()
            .iter()
            .map(|site| LiveUsSiteRow {
                site: SiteRef::Us {
                    level2_id: site.level2_id.clone(),
                },
                kind: us_site_kind(site),
                label: format_site_label(site),
            })
            .collect()
    }

    pub(crate) fn live_us_site_ref_at(&self, index: usize) -> Option<SiteRef> {
        self.live_us_sites().get(index).map(|site| SiteRef::Us {
            level2_id: site.level2_id.clone(),
        })
    }

    pub(crate) fn live_us_site_label_at(&self, index: usize) -> Option<String> {
        self.live_us_sites().get(index).map(format_site_label)
    }

    pub(crate) fn live_us_site_index(&self, site: &SiteRef) -> Option<usize> {
        let SiteRef::Us { level2_id } = site else {
            return None;
        };
        self.live_us_sites()
            .iter()
            .position(|row| row.level2_id.eq_ignore_ascii_case(level2_id))
    }

    pub(crate) fn live_us_radar_site_at(&self, index: usize) -> Option<RadarSite> {
        self.live_us_sites().get(index).cloned()
    }

    /// Lowest-beam radar candidates for a geo point across the primary
    /// Level II site catalog, sorted by 0.5° beam height ascending (slant
    /// range → 4/3-Earth beam height). Community/research feeds stay
    /// explicit operator picks so Ctrl-click/right-click nearest does not
    /// jump to non-NEXRAD feeds.
    /// Geometry only — the terrain-blockage version needs a coverage
    /// dataset. TDWRs (Txxx) have ~90 km range and C-band attenuation, so
    /// they list in their own menu section ([`Self::tdwr_candidates`])
    /// instead of competing in this ranking.
    pub(crate) fn best_radar_candidates(&self, lat: f32, lon: f32) -> Vec<BeamCandidate> {
        let beam_at = |distance_km: f32| {
            radar_core::beam_height_above_radar_m(distance_km as f64 * 1000.0, 0.5) as f32
        };
        // ONE ranking over the union catalog (`sites_near`, v0.29 Phase 3):
        // US and international sites compete on the same 460 km fence and
        // the same 0.5° geometry — the lowest usable beam over Stockholm
        // is an SMHI radar, not "no radar" (field report: the menu was
        // dead across Europe/Australia/Japan). Explicit kind gate:
        // research feeds stay operator picks; TDWRs rank in their own
        // menu section.
        let mut candidates: Vec<BeamCandidate> = data_source::sites::sites_near(lat, lon, 460.0)
            .into_iter()
            .filter_map(|(record, distance_km)| {
                match record.kind {
                    SiteKind::Wsr88d | SiteKind::Intl { .. } => {}
                    SiteKind::Tdwr | SiteKind::Research => return None,
                }
                let label = match &record.site {
                    SiteRef::Us { level2_id } => level2_id.clone(),
                    SiteRef::Intl { .. } => record.label.clone(),
                };
                Some(BeamCandidate {
                    site: record.site,
                    label,
                    origin: record.origin,
                    beam_m: beam_at(distance_km),
                    distance_km,
                })
            })
            .collect();
        candidates.sort_by(|a, b| a.beam_m.total_cmp(&b.beam_m));
        candidates
    }

    /// Nearby TDWRs for the context menu's own section: Txxx sites by
    /// ground distance. TDWRs are C-band terminal radars with very low
    /// tilts (0.1-0.6°) but ~90 km Doppler range and rain attenuation
    /// (Vasiloff 2001 WAF; Istok et al. 2009, 25th IIPS), so they list
    /// separately instead of competing in the lowest-beam ranking.
    pub(crate) fn tdwr_candidates(&self, lat: f32, lon: f32) -> Vec<(SiteRef, String, f32)> {
        // Union-catalog query, nearest first; explicit kind gate keeps
        // exactly the catalog-data TDWRs.
        data_source::sites::sites_near(lat, lon, 120.0)
            .into_iter()
            .filter_map(|(record, distance_km)| {
                match record.kind {
                    SiteKind::Tdwr => {}
                    SiteKind::Wsr88d | SiteKind::Research | SiteKind::Intl { .. } => return None,
                }
                let SiteRef::Us { level2_id } = &record.site else {
                    return None;
                };
                Some((record.site.clone(), level2_id.clone(), distance_km))
            })
            .collect()
    }

    pub(crate) fn community_feed_candidates(
        &self,
        lat: f32,
        lon: f32,
        max_km: f32,
    ) -> Vec<(data_source::community_feeds::CommunityFeed, f32)> {
        let mut candidates = data_source::community_feeds::community_feeds()
            .iter()
            .filter_map(|feed| {
                let distance_km = haversine_km(lat, lon, feed.latitude_deg, feed.longitude_deg);
                (distance_km <= max_km).then_some((*feed, distance_km))
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| left.1.total_cmp(&right.1));
        candidates
    }

    pub(crate) fn intl_radar_candidates(
        &self,
        lat: f32,
        lon: f32,
        max_km: f32,
    ) -> Vec<(data_source::international::IntlSite, f32)> {
        let mut candidates = data_source::international::intl_static_sites()
            .iter()
            .filter_map(|site| {
                let (Some(site_lat), Some(site_lon)) = (site.latitude_deg, site.longitude_deg)
                else {
                    return None;
                };
                let distance_km = haversine_km(lat, lon, site_lat, site_lon);
                (distance_km <= max_km).then_some((site.clone(), distance_km))
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| left.1.total_cmp(&right.1));
        candidates
    }

    pub(crate) fn custom_poll_candidates(
        &self,
        lat: f32,
        lon: f32,
        max_km: f32,
    ) -> Vec<(usize, String, f32)> {
        let mut candidates = self
            .app_settings
            .custom_poll_links
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                if custom_poll_entry_matches_community_feed(entry) {
                    return None;
                }
                let (entry_lat, entry_lon) = custom_poll_entry_lat_lon(entry)?;
                let distance_km = haversine_km(lat, lon, entry_lat, entry_lon);
                (distance_km <= max_km)
                    .then(|| (index, custom_poll_entry_label(entry), distance_km))
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| left.2.total_cmp(&right.2));
        candidates
    }

    pub(crate) fn find_intl_site(
        provider_id: &str,
        site_id: &str,
    ) -> Option<data_source::international::IntlSite> {
        data_source::international::intl_static_sites()
            .iter()
            .find(|site| site.provider_id == provider_id && site.site_id == site_id)
            .cloned()
    }
}

/// Radar rows for the SITE search box across BOTH catalogs (v0.29 spec
/// Phase 3 gate: "site search offers intl radars"). Rows are
/// `(SiteRef, label)` — kind-tagged identity, never a catalog index.
///
/// US matching keeps the v0.28 behavior byte-for-byte over the LIVE
/// `sites` catalog (which can carry brand-new S3 sites the embedded
/// catalog does not know yet): `level2_id` exact, optional-K exact, then
/// prefix. International rows come from the union catalog and match by
/// site id or picker label — no K-stripping (a NEXRAD naming
/// convention) — with the provider origin riding the row label so a bare
/// city name never masquerades as a place-search hit.
pub(crate) fn radar_site_search_matches(
    query: &str,
    sites: &[RadarSite],
    limit: usize,
) -> Vec<(SiteRef, String)> {
    let compact_ascii_upper = |text: &str| {
        text.chars()
            .filter(|ch| ch.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_uppercase()
    };
    let compact = compact_ascii_upper(query);
    if compact.len() < 2 || limit == 0 {
        return Vec::new();
    }
    let no_k_query = compact.strip_prefix('K').unwrap_or(&compact);
    let mut matches = sites
        .iter()
        .enumerate()
        .filter_map(|(index, site)| {
            let id = site.level2_id.to_ascii_uppercase();
            let no_k_id = id.strip_prefix('K').unwrap_or(&id);
            let score = if id == compact {
                0
            } else if no_k_id == no_k_query {
                1
            } else if id.starts_with(&compact) || no_k_id.starts_with(no_k_query) {
                2
            } else {
                return None;
            };
            let site_ref = SiteRef::Us {
                level2_id: site.level2_id.clone(),
            };
            Some((score, index, site_ref, format_site_label(site)))
        })
        .collect::<Vec<_>>();
    for (ordinal, record) in data_source::sites::all_sites().enumerate() {
        let SiteKind::Intl { .. } = record.kind else {
            continue;
        };
        let SiteRef::Intl { site_id, .. } = &record.site else {
            continue;
        };
        let id = compact_ascii_upper(site_id);
        let label = compact_ascii_upper(&record.label);
        let score = if id == compact || label == compact {
            0
        } else if id.starts_with(&compact) || label.starts_with(&compact) {
            2
        } else {
            continue;
        };
        let row_label = match &record.origin {
            Some(origin) => format!("{} · {origin}", record.label),
            None => record.label.clone(),
        };
        // Ordinal offset keeps the historical tiebreak stable: equal
        // score + label prefers the US catalog row.
        matches.push((score, sites.len() + ordinal, record.site.clone(), row_label));
    }
    matches.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.3.cmp(&right.3))
            .then_with(|| left.1.cmp(&right.1))
    });
    matches.truncate(limit);
    matches
        .into_iter()
        .map(|(_, _, site, label)| (site, label))
        .collect()
}

/// [`SiteKind`] of a US-catalog [`RadarSite`], resolved against the one
/// compiled-in catalog (`data_source::sites`) — the v0.29 Phase-3
/// replacement for the deleted `site_is_tdwr` `'T'`-prefix heuristic and
/// `site_is_primary_level2_catalog_site`. Classification is CATALOG DATA
/// now (TJUA's WSR-88D exception, the TDWR table, community research
/// feeds), so the JMA TAKA/TANE/TOJI prefix-leak class is gone for good.
///
/// Ids the catalog does not know (a brand-new site surfacing in an S3
/// listing before the embedded table updates, test fixtures) classify as
/// [`SiteKind::Wsr88d`] — the permissive default that keeps them
/// selectable and rankable, matching how v0.28 treated any unknown
/// non-`T` id. Mobile radars (DOW/COW) stay volume-only and never enter
/// `self.sites`, so they never reach this gate.
pub(crate) fn us_site_kind(site: &RadarSite) -> SiteKind {
    data_source::sites::resolve(&SiteRef::Us {
        level2_id: site.level2_id.clone(),
    })
    .map(|record| record.kind)
    .unwrap_or(SiteKind::Wsr88d)
}

/// US level2 id of a [`SiteRef`], if it is a US ref.
pub(crate) fn pin_us_id(pin: &SiteRef) -> Option<&str> {
    match pin {
        SiteRef::Us { level2_id } => Some(level2_id),
        SiteRef::Intl { .. } => None,
    }
}

/// `(provider_id, site_id)` of a [`SiteRef`], if it is an intl ref.
pub(crate) fn pin_intl_ids(pin: &SiteRef) -> Option<(&str, &str)> {
    match pin {
        SiteRef::Us { .. } => None,
        SiteRef::Intl {
            provider_id,
            site_id,
        } => Some((provider_id, site_id)),
    }
}

/// What a Ctrl+right-click "add nearest radar overlay" resolves to.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum NearestOverlayTarget {
    /// Index into the primary US Level II site catalog.
    Us(usize),
    /// International static-catalog site.
    Intl(data_source::international::IntlSite),
}

/// Overlay adds share the context menu's 460 km fence: a click with
/// nothing in range must be an honest no-op, never a transatlantic layer.
const NEAREST_OVERLAY_MAX_KM: f32 = 460.0;

/// Pure v0.21 dispatch for Ctrl+right-click overlay adds: the nearest
/// primary-catalog US site vs the nearest international site by ground
/// distance, whichever is closer, both capped at
/// [`NEAREST_OVERLAY_MAX_KM`]. `None` = nothing in range.
pub(crate) fn nearest_overlay_dispatch(
    sites: &[RadarSite],
    intl_sites: &[data_source::international::IntlSite],
    lat: f32,
    lon: f32,
) -> Option<NearestOverlayTarget> {
    let us = sites
        .iter()
        .enumerate()
        // Overlay adds accept the whole NEXRAD/TDWR program; research
        // feeds stay explicit operator picks (explicit kind gate).
        .filter(|(_, site)| match us_site_kind(site) {
            SiteKind::Wsr88d | SiteKind::Tdwr => true,
            SiteKind::Research | SiteKind::Intl { .. } => false,
        })
        .filter_map(|(index, site)| {
            let (site_lat, site_lon) = site_location(site)?;
            let distance_km = haversine_km(lat, lon, site_lat, site_lon);
            (distance_km <= NEAREST_OVERLAY_MAX_KM).then_some((index, distance_km))
        })
        .min_by(|left, right| left.1.total_cmp(&right.1));
    let intl = intl_sites
        .iter()
        .filter_map(|site| {
            let (Some(site_lat), Some(site_lon)) = (site.latitude_deg, site.longitude_deg) else {
                return None;
            };
            let distance_km = haversine_km(lat, lon, site_lat, site_lon);
            (distance_km <= NEAREST_OVERLAY_MAX_KM).then_some((site, distance_km))
        })
        .min_by(|left, right| left.1.total_cmp(&right.1));
    match (us, intl) {
        (Some((index, us_km)), Some((_, intl_km))) if us_km <= intl_km => {
            Some(NearestOverlayTarget::Us(index))
        }
        (Some((index, _)), None) => Some(NearestOverlayTarget::Us(index)),
        (_, Some((site, _))) => Some(NearestOverlayTarget::Intl(site.clone())),
        (None, None) => None,
    }
}
