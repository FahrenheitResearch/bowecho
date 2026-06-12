//! Community-contributed US research-radar live feeds.
//!
//! Non-NEXRAD radars (university X-bands, testbeds, state networks) that
//! serve raw Level II over the GR2A dir.list convention: the shared
//! custom-URL poller fetches `{poll_url}/dir.list` (or discovers the one
//! site named by `{poll_url}/grlevel2.cfg`), downloads the newest entry,
//! and decodes it through `nexrad_io`'s magic-byte router. NEXRAD proper
//! loads natively from S3, so this catalog covers only what S3 can't.
//!
//! Coordinates are community-contributed (forwarded by a BowEcho user;
//! no agency catalog lists these radars). URLs were probed live
//! 2026-06-12: every Iowa Environmental Mesonet path answered
//! `{base}/{ID}/dir.list` with data (uppercase only — the host 404s
//! lowercase ids) except OP5R, whose radar is documented on the IEM index
//! page but had no directory that day; KXWA/KBPP answered on the North
//! Dakota State Water Commission host while K08D's directory existed but
//! was empty; the Laredo EWR host answered with `grlevel2.cfg` naming its
//! single site `LARE` but the site directory was empty. Feeds that are
//! quiet today still belong here — the poller reports an unreachable
//! dir.list as a status-line error and keeps the marker clickable.

use std::sync::OnceLock;

/// One community research-radar feed: a pollable GR2A-style URL plus the
/// community-contributed site location for its map marker.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CommunityFeed {
    /// Upstream site id, exactly as the feed host spells it (the IEM host
    /// is case-sensitive: `WILU`, not `wilu`).
    pub id: &'static str,
    /// Human-readable site name for menus and marker labels.
    pub label: &'static str,
    /// Two-letter US state, the Feeds-menu grouping key.
    pub state: &'static str,
    /// Site latitude in degrees north (community-contributed).
    pub latitude_deg: f32,
    /// Site longitude in degrees east (community-contributed).
    pub longitude_deg: f32,
    /// GR2A-style poll root (no trailing slash): the poller appends
    /// `/dir.list`, falling back to `grlevel2.cfg` site discovery.
    pub poll_url: &'static str,
    /// Marker-cluster label for feeds that share one physical location
    /// (the Norman Testbed ids all sit on one pad): feeds with equal
    /// `Some` labels collapse into a single map marker whose click opens
    /// a feed picker. `None`: the feed gets its own direct-click marker.
    pub cluster: Option<&'static str>,
}

/// Norman Testbed cluster label — eight feeds, one pad, one marker.
const NORMAN_TESTBED: &str = "Norman Testbed";
/// The Norman Testbed pad (community-contributed, shared by all eight).
const NORMAN_LAT: f32 = 35.238;
const NORMAN_LON: f32 = -97.460;

/// Helper for the table below: a direct-click (non-cluster) feed row.
const fn feed(
    id: &'static str,
    label: &'static str,
    state: &'static str,
    latitude_deg: f32,
    longitude_deg: f32,
    poll_url: &'static str,
) -> CommunityFeed {
    CommunityFeed {
        id,
        label,
        state,
        latitude_deg,
        longitude_deg,
        poll_url,
        cluster: None,
    }
}

/// Helper for the table below: a Norman Testbed cluster member.
const fn norman(id: &'static str, poll_url: &'static str) -> CommunityFeed {
    CommunityFeed {
        id,
        label: NORMAN_TESTBED,
        state: "OK",
        latitude_deg: NORMAN_LAT,
        longitude_deg: NORMAN_LON,
        poll_url,
        cluster: Some(NORMAN_TESTBED),
    }
}

/// The community feed catalog — the single source of truth for the Feeds
/// menu AND the map markers (one table, so they can't drift apart).
const COMMUNITY_FEED_TABLE: &[CommunityFeed] = &[
    // Iowa Environmental Mesonet community Level II host.
    feed(
        "FWLX",
        "WLX X-Band",
        "TN",
        35.254,
        -87.325,
        "https://mesonet-nexrad.agron.iastate.edu/level2/raw/FWLX",
    ),
    feed(
        "FUSA",
        "Denton",
        "MD",
        38.86949,
        -75.81616,
        "https://mesonet-nexrad.agron.iastate.edu/level2/raw/FUSA",
    ),
    feed(
        "GAWX",
        "Lawrenceville",
        "GA",
        33.98047,
        -84.00345,
        "https://mesonet-nexrad.agron.iastate.edu/level2/raw/GAWX",
    ),
    feed(
        "WILU",
        "Western Illinois University",
        "IL",
        40.465,
        -90.685,
        "https://mesonet-nexrad.agron.iastate.edu/level2/raw/WILU",
    ),
    feed(
        "KULM",
        "Monroe / ULM",
        "LA",
        32.529,
        -92.012,
        "https://mesonet-nexrad.agron.iastate.edu/level2/raw/KULM",
    ),
    feed(
        "MZZU",
        "Columbia / Mizzou",
        "MO",
        38.906,
        -92.269,
        "https://mesonet-nexrad.agron.iastate.edu/level2/raw/MZZU",
    ),
    feed(
        "OP5R",
        "Fort Greely",
        "AK",
        63.922,
        -145.833,
        "https://mesonet-nexrad.agron.iastate.edu/level2/raw/OP5R",
    ),
    // Norman Testbed: eight research feeds on one pad — one map marker.
    norman(
        "DAN1",
        "https://mesonet-nexrad.agron.iastate.edu/level2/raw/DAN1",
    ),
    norman(
        "DOP1",
        "https://mesonet-nexrad.agron.iastate.edu/level2/raw/DOP1",
    ),
    norman(
        "FOP1",
        "https://mesonet-nexrad.agron.iastate.edu/level2/raw/FOP1",
    ),
    norman(
        "NOP3",
        "https://mesonet-nexrad.agron.iastate.edu/level2/raw/NOP3",
    ),
    norman(
        "NOP4",
        "https://mesonet-nexrad.agron.iastate.edu/level2/raw/NOP4",
    ),
    norman(
        "ROP3",
        "https://mesonet-nexrad.agron.iastate.edu/level2/raw/ROP3",
    ),
    norman(
        "ROP4",
        "https://mesonet-nexrad.agron.iastate.edu/level2/raw/ROP4",
    ),
    norman(
        "KCRI",
        "https://mesonet-nexrad.agron.iastate.edu/level2/raw/KCRI",
    ),
    // North Dakota State Water Commission network.
    feed(
        "K08D",
        "Stanley ARB",
        "ND",
        48.301_45,
        -102.401_41,
        "https://level2.swc.nd.gov/raw/K08D",
    ),
    feed(
        "KXWA",
        "Williston ARB",
        "ND",
        48.263_44,
        -103.747_33,
        "https://level2.swc.nd.gov/raw/KXWA",
    ),
    feed(
        "KBPP",
        "Bowman ARB",
        "ND",
        46.187,
        -103.428,
        "https://level2.swc.nd.gov/raw/KBPP",
    ),
    // Laredo EWR: the poll root carries grlevel2.cfg naming the single
    // site LARE, so the shared poller's root-discovery convention finds
    // the live subdirectory itself.
    feed(
        "LARE",
        "KGNS EWR Doppler",
        "TX",
        27.541472,
        -99.457692,
        "http://offsitevpn.ewradar.com/Laredo/archive2.trans",
    ),
];

/// The community feed table. Pure compiled-in data — never the network —
/// so it is safe on the UI thread every frame.
pub fn community_feeds() -> &'static [CommunityFeed] {
    COMMUNITY_FEED_TABLE
}

/// One map marker for the community catalog: a single feed, or every feed
/// of a shared-pad cluster (the Norman Testbed) collapsed into one marker
/// whose click opens a feed picker.
#[derive(Clone, Debug, PartialEq)]
pub struct CommunityMarker {
    /// Marker label: the cluster label, or the lone feed's `id — label`.
    pub label: String,
    /// Marker location (a cluster's shared pad, or the lone feed's site).
    pub latitude_deg: f32,
    pub longitude_deg: f32,
    /// Indices into [`community_feeds`], table order. Exactly one for a
    /// direct-click marker; two or more for a cluster.
    pub feed_indices: Vec<usize>,
}

/// The community map-marker catalog: every feed exactly once, with
/// shared-`cluster` feeds folded into one marker at the first member's
/// table position. Memoized for the life of the process (pure data).
pub fn community_markers() -> &'static [CommunityMarker] {
    static MARKERS: OnceLock<Vec<CommunityMarker>> = OnceLock::new();
    MARKERS.get_or_init(|| {
        let feeds = community_feeds();
        let mut markers: Vec<CommunityMarker> = Vec::new();
        for (index, feed) in feeds.iter().enumerate() {
            if let Some(cluster) = feed.cluster {
                if let Some(marker) = markers.iter_mut().find(|marker| marker.label == cluster) {
                    marker.feed_indices.push(index);
                    continue;
                }
                markers.push(CommunityMarker {
                    label: cluster.to_owned(),
                    latitude_deg: feed.latitude_deg,
                    longitude_deg: feed.longitude_deg,
                    feed_indices: vec![index],
                });
            } else {
                markers.push(CommunityMarker {
                    label: format!("{} — {}", feed.id, feed.label),
                    latitude_deg: feed.latitude_deg,
                    longitude_deg: feed.longitude_deg,
                    feed_indices: vec![index],
                });
            }
        }
        markers
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Iowa Environmental Mesonet community Level II host (case-sensitive
    /// site directories).
    const IEM_BASE: &str = "https://mesonet-nexrad.agron.iastate.edu/level2/raw";
    /// North Dakota State Water Commission Level II host.
    const ND_SWC_BASE: &str = "https://level2.swc.nd.gov/raw";

    /// Generous per-state bounding boxes `(lat_min, lat_max, lon_min,
    /// lon_max)`: a swapped lat/lon, a missing minus sign, or a
    /// degrees/radians slip all land far outside.
    fn state_bounding_box(state: &str) -> Option<(f32, f32, f32, f32)> {
        Some(match state {
            "TN" => (34.8, 36.8, -90.4, -81.5),
            "MD" => (37.8, 39.8, -79.6, -74.9),
            "GA" => (30.3, 35.1, -85.7, -80.7),
            "IL" => (36.9, 42.6, -91.6, -87.0),
            "LA" => (28.8, 33.1, -94.1, -88.7),
            "MO" => (35.9, 40.7, -95.9, -89.0),
            "AK" => (51.0, 71.5, -180.0, -129.9),
            "OK" => (33.5, 37.1, -103.1, -94.4),
            "ND" => (45.8, 49.1, -104.2, -96.5),
            "TX" => (25.7, 36.6, -106.7, -93.4),
            _ => return None,
        })
    }

    #[test]
    fn feed_ids_are_unique_and_nonempty() {
        let feeds = community_feeds();
        assert!(!feeds.is_empty());
        let mut ids: Vec<&str> = feeds.iter().map(|feed| feed.id).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), before, "duplicate feed id");
        for feed in feeds {
            assert!(!feed.id.is_empty() && !feed.label.is_empty());
            assert_eq!(
                feed.id,
                feed.id.trim().to_ascii_uppercase(),
                "{}: ids are spelled uppercase (the IEM host 404s lowercase)",
                feed.id
            );
        }
    }

    #[test]
    fn every_feed_sits_inside_its_state_bounding_box() {
        for feed in community_feeds() {
            let (lat_min, lat_max, lon_min, lon_max) = state_bounding_box(feed.state)
                .unwrap_or_else(|| panic!("no bounding box for state {}", feed.state));
            assert!(
                feed.latitude_deg.is_finite()
                    && feed.longitude_deg.is_finite()
                    && (lat_min..=lat_max).contains(&feed.latitude_deg)
                    && (lon_min..=lon_max).contains(&feed.longitude_deg),
                "{} ({}): ({}, {}) outside {} box",
                feed.id,
                feed.label,
                feed.latitude_deg,
                feed.longitude_deg,
                feed.state
            );
        }
    }

    #[test]
    fn poll_urls_are_well_formed_roots() {
        for feed in community_feeds() {
            let url = feed.poll_url;
            assert!(
                url.starts_with("https://") || url.starts_with("http://"),
                "{}: {url}",
                feed.id
            );
            assert!(
                !url.ends_with('/'),
                "{}: poll roots carry no trailing slash (the poller appends /dir.list)",
                feed.id
            );
            assert!(
                !url.chars().any(char::is_whitespace),
                "{}: whitespace in {url}",
                feed.id
            );
            // IEM and ND SWC follow the {base}/{ID} site-subdirectory
            // convention; the Laredo root is discovered via grlevel2.cfg.
            if let Some(rest) = url
                .strip_prefix(&format!("{IEM_BASE}/"))
                .or_else(|| url.strip_prefix(&format!("{ND_SWC_BASE}/")))
            {
                assert_eq!(rest, feed.id, "site subdirectory must match the id");
            }
        }
    }

    #[test]
    fn markers_cover_every_feed_exactly_once() {
        let feeds = community_feeds();
        let markers = community_markers();
        let mut seen = BTreeSet::new();
        for marker in markers {
            assert!(!marker.label.is_empty());
            assert!(marker.latitude_deg.is_finite() && marker.longitude_deg.is_finite());
            assert!(!marker.feed_indices.is_empty());
            for &index in &marker.feed_indices {
                assert!(index < feeds.len(), "feed index out of range");
                assert!(seen.insert(index), "feed {index} in two markers");
                // Cluster members must actually share the marker's pad.
                let feed = &feeds[index];
                assert_eq!(feed.latitude_deg, marker.latitude_deg, "{}", feed.id);
                assert_eq!(feed.longitude_deg, marker.longitude_deg, "{}", feed.id);
            }
        }
        assert_eq!(seen.len(), feeds.len(), "every feed appears in a marker");
        // Memoized: repeated calls hand back the same slice.
        assert!(std::ptr::eq(markers, community_markers()));
    }

    #[test]
    fn the_norman_testbed_is_one_marker_of_eight_feeds() {
        let markers = community_markers();
        let clusters: Vec<&CommunityMarker> = markers
            .iter()
            .filter(|marker| marker.feed_indices.len() > 1)
            .collect();
        assert_eq!(clusters.len(), 1, "exactly one cluster marker");
        let norman = clusters[0];
        assert_eq!(norman.label, NORMAN_TESTBED);
        assert_eq!(norman.feed_indices.len(), 8);
        let feeds = community_feeds();
        let ids: BTreeSet<&str> = norman
            .feed_indices
            .iter()
            .map(|&index| feeds[index].id)
            .collect();
        let expected: BTreeSet<&str> = [
            "DAN1", "DOP1", "FOP1", "NOP3", "NOP4", "ROP3", "ROP4", "KCRI",
        ]
        .into_iter()
        .collect();
        assert_eq!(ids, expected);
        // Everything else is a direct-click single-feed marker.
        assert_eq!(markers.len(), feeds.len() - 7);
    }
}
