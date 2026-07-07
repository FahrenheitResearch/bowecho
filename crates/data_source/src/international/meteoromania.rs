//! ANM (Administrația Națională de Meteorologie, Romania) radar volume feed.
//!
//! Catalog: `https://opendata.meteoromania.ro/radar/`, the "Portal Open
//! Data Meteo Romania" — ANM's official open-data host. One nginx
//! autoindex directory per radar plus a national `COMPOSITE/` directory
//! (captured live 2026-07-07 UTC):
//!
//! ```text
//! radar/
//!   BAR/ BOB/ BUC/ CRA/ MED/ ORA/ TIM/     stations
//!     BUC_2026070718400200dBZ.hdf          {DIR}_{yyyymmddHHMMSScc}{moment}.hdf
//!     BUC_2026070718400200V.hdf            5-minute stamps, ~3-day rolling
//!     BUC_2026070718400200ZDR.hdf          window (~460 stamps per site)
//!     ... KDP / RhoHV / dBR / Height
//!   COMPOSITE/                             national composites (not polar)
//! ```
//!
//! Each of dBZ/V/ZDR/KDP/RhoHV is a FULL polar volume (ODIM_H5 `PVOL`,
//! H5rad 2.3; EUMETNET OPERA Data Information Model; Michelson et al.,
//! OPERA WP 2.1/2.2) carrying that moment across all 12 cuts (0.5–19.5°,
//! 920 bins @ 250 m) — confirmed by live decode of every station's dBZ and
//! BUC's full moment set. A multi-moment volume is assembled SHMU-style:
//! one PVOL per moment at a common timestamp, merged with
//! `radar_core::merge_radar_volumes`, dBZ first (merge base), V second,
//! then ZDR/KDP/RhoHV when present at that stamp. ODIM quantities inside
//! the files are the canonical codes (`DBZH`, `VRADH`, `ZDR`, `KDP`,
//! `RHOHV`), so the existing `nexrad_io` ODIM decode path handles every
//! part unchanged.
//!
//! LISTED BUT SKIPPED — `dBR` and `Height`: those files are NOT polar
//! volumes but ODIM `IMAGE` objects (Cartesian 700×700 aeqd grids; `dBR` =
//! PCAPPI rain rate `RATE`, `Height` = echo-top `HGHT`, verified live
//! 2026-07-07). They belong to the same product family as the `COMPOSITE/`
//! directory and BowEcho has no polar display pipeline for them, so the
//! stamp grouper recognizes and excludes them rather than bolting new
//! display code onto this provider. Revisit alongside COMPOSITE support.
//!
//! UPLOAD LIFECYCLE (measured live 2026-07-07, twice): for each new stamp
//! ANM first uploads a SMALL Cartesian PCAPPI `IMAGE` under the *dBZ file
//! name* (~4 minutes after the stamp, 70–130 KB), then overwrites it
//! in-place with the real PVOL (~6 minutes after the stamp, 450–800 KB;
//! watched a BAR stamp flip 82 K → 679 K between +5 and +6 minutes). A
//! frame is therefore only anchored on a stamp at least
//! [`STAMP_SETTLE_MINUTES`] old; if nothing in the listing is that old
//! (feed gap or local clock skew), the newest complete stamp is used
//! anyway — the ODIM decoder rejects `IMAGE` objects with a clear error,
//! so a too-early download fails the poll tick and retries, it never
//! installs a wrong volume.
//!
//! Relationship to EUMETNET ORD: ANM pushes the SAME per-moment ODIM
//! PVOLs to ORD (the ORD `robuc` DBZH object is byte-identical to the
//! native dBZ file), but ORD carries only DBZH/TH/VRADH — the native
//! portal adds the full dual-pol set (ZDR/KDP/RhoHV). This provider is
//! the live/recent Romania source (rolling ~3 days); ORD remains the
//! deep-archive path, and `ord.rs` defers its seven `ro*` picker/marker
//! rows to this provider the same way it defers `eesur` to the KAIA
//! Estonia bridge.
//!
//! License: the portal states no explicit license text (checked
//! 2026-07-07); it is ANM's official open-data host, and the identical
//! volumes are served through EUMETNET ORD under CC BY 4.0. BowEcho
//! credits "Data: Administrația Națională de Meteorologie (ANM) România";
//! owner to confirm the final attribution wording before release.

use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDateTime, Utc};

use super::listing::{digit_run, fnv1a64, join_url, parse_autoindex};
use super::{FramePlan, IntlProvider, IntlSite, PlanPart, RecentFrames};
use crate::fetch_listing_text;

const ANM_RADAR_ROOT: &str = "https://opendata.meteoromania.ro/radar/";

/// Polar-volume moments in merge order (dBZ is the merge base). The first
/// two are required for a frame; the dual-pol tail merges in when present
/// at the chosen timestamp.
const MOMENT_ORDER: [&str; 5] = ["dBZ", "V", "ZDR", "KDP", "RhoHV"];
/// How many leading [`MOMENT_ORDER`] entries a stamp must have to anchor a
/// frame (dBZ and V — the same required pair as SHMU/DWD).
const REQUIRED_MOMENTS: usize = 2;

/// Cartesian `IMAGE` products listed alongside the volumes (see the module
/// doc): recognized so the grouper can prove it skips them deliberately.
const CARTESIAN_PRODUCTS: [&str; 2] = ["dBR", "Height"];

/// Youngest stamp age (minutes) trusted to hold real PVOLs. ANM overwrites
/// each dBZ file in-place — Cartesian placeholder at ~stamp+4 min, real
/// PVOL at ~stamp+6 min (module doc) — so anchoring before the rewrite
/// would download the wrong object family. 9 minutes = the observed
/// rewrite completion plus a 3-minute margin, still under two 5-minute
/// cycles behind real time.
const STAMP_SETTLE_MINUTES: i64 = 9;

/// Station directories, labels, and coordinates. Directory names are the
/// upstream site ids; the ODIM `source` codes inside the files are the
/// NOD forms ORD uses (`BAR`→`robar`, ..., `TIM`→`rotim`). Labels follow
/// the ORD site table; coordinates were read from the `/where` group of
/// live PVOLs (2026-07-07) and agree with the ORD/EDR rows to the 4
/// decimals kept here.
const ANM_STATIONS: [(&str, &str, f32, f32); 7] = [
    ("BAR", "Bârnova", 47.0118, 27.5825),
    ("BOB", "Bobohalma", 46.3602, 24.2252),
    ("BUC", "București", 44.5127, 26.0773),
    ("CRA", "Craiova", 44.3103, 23.8674),
    ("MED", "Medgidia", 44.2434, 28.2506),
    ("ORA", "Oradea", 47.0922, 21.9429),
    ("TIM", "Timișoara", 45.7717, 21.2577),
];

/// Romania's ANM open-data radar volume feed (per-moment full PVOLs).
#[derive(Clone, Copy, Debug, Default)]
pub struct MeteoRomaniaProvider;

impl MeteoRomaniaProvider {
    pub fn new() -> Self {
        Self
    }
}

impl IntlProvider for MeteoRomaniaProvider {
    fn id(&self) -> &'static str {
        "meteoromania"
    }

    fn label(&self) -> &'static str {
        "ANM Romania"
    }

    fn country(&self) -> &'static str {
        "Romania"
    }

    fn list_sites(&self) -> Result<Vec<IntlSite>, String> {
        // The network is a fixed set of seven WSR-98D/METEOR stations; the
        // compiled-in table IS the catalog (the root listing would only
        // add the COMPOSITE pseudo-directory this provider skips).
        Ok(self.static_sites())
    }

    fn latest(&self, site_id: &str) -> Result<FramePlan, String> {
        let mut plans = anm_recent_plans(site_id, 1, Utc::now())?;
        plans
            .pop()
            .ok_or_else(|| format!("ANM site '{site_id}': no frames resolved"))
    }

    fn recent_source(&self) -> Option<&dyn RecentFrames> {
        Some(self)
    }

    fn static_sites(&self) -> Vec<IntlSite> {
        ANM_STATIONS
            .iter()
            .map(|&(site_id, label, latitude_deg, longitude_deg)| IntlSite {
                provider_id: self.id(),
                site_id: site_id.to_owned(),
                label: label.to_owned(),
                country: self.country(),
                latitude_deg: Some(latitude_deg),
                longitude_deg: Some(longitude_deg),
            })
            .collect()
    }
}

impl RecentFrames for MeteoRomaniaProvider {
    fn recent_frames(&self, site_id: &str, count: usize) -> Result<Vec<FramePlan>, String> {
        anm_recent_plans(site_id, count, Utc::now())
    }
}

/// Up to `count` frames, OLDEST FIRST, from one site-directory listing.
/// With `count = 1` this is exactly the frame `latest` describes (same
/// stamp choice, part order, and identity), so the loop's newest frame
/// stays the live poll's dedupe key.
fn anm_recent_plans(
    site_id: &str,
    count: usize,
    now: DateTime<Utc>,
) -> Result<Vec<FramePlan>, String> {
    if !ANM_STATIONS.iter().any(|(dir, ..)| *dir == site_id) {
        return Err(format!("unknown ANM site '{site_id}'"));
    }
    let site_url = format!("{ANM_RADAR_ROOT}{site_id}/");
    let html =
        fetch_listing_text(&site_url).map_err(|err| format!("ANM listing {site_url}: {err}"))?;
    let groups = stamp_moment_groups(&html, site_id);
    let stamps = anchor_stamps(&groups, count, now);
    if stamps.is_empty() {
        return Err(format!(
            "ANM site '{site_id}': no timestamp with both dBZ and V \
             ({} stamps listed)",
            groups.len()
        ));
    }
    Ok(plans_from_groups(site_id, &stamps, &groups))
}

/// Parse a site-directory listing into `16-digit stamp -> (moment -> file
/// name)`, volume moments only. File names are `{DIR}_{stamp}{moment}.hdf`
/// with a 16-digit stamp (`yyyymmddHHMMSScc`, centisecond suffix — the
/// scan second varies between volumes, so the full run is the group key).
/// Names are kept verbatim from the listing for URL joining. The Cartesian
/// `dBR`/`Height` products and anything else (README files, the parent
/// link) contribute nothing.
fn stamp_moment_groups(
    listing_html: &str,
    site_id: &str,
) -> BTreeMap<String, BTreeMap<&'static str, String>> {
    let mut groups: BTreeMap<String, BTreeMap<&'static str, String>> = BTreeMap::new();
    for entry in parse_autoindex(listing_html) {
        if entry.is_dir {
            continue;
        }
        let Some((stamp, moment)) = volume_moment(&entry.name, site_id) else {
            continue;
        };
        groups.entry(stamp).or_default().insert(moment, entry.name);
    }
    groups
}

/// Classify one listing file name: `Some((stamp, moment))` for a polar
/// volume moment of this site, `None` for the Cartesian products and any
/// foreign name.
fn volume_moment(name: &str, site_id: &str) -> Option<(String, &'static str)> {
    let rest = name.strip_prefix(site_id)?.strip_prefix('_')?;
    let stamp = digit_run(rest, 16)?;
    let product = rest
        .strip_prefix(stamp)?
        .strip_suffix(".hdf")
        .filter(|product| !product.is_empty())?;
    if CARTESIAN_PRODUCTS.contains(&product) {
        return None; // recognized, deliberately skipped (module doc)
    }
    let moment = MOMENT_ORDER
        .iter()
        .copied()
        .find(|moment| *moment == product)?;
    Some((stamp.to_owned(), moment))
}

/// `true` when the stamp's group holds every required moment (dBZ and V).
fn is_complete(group: &BTreeMap<&'static str, String>) -> bool {
    MOMENT_ORDER[..REQUIRED_MOMENTS]
        .iter()
        .all(|moment| group.contains_key(moment))
}

/// Scan-start time of a stamp (the leading 14 digits; the trailing 2 are
/// centiseconds).
fn stamp_time(stamp: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(stamp.get(..14)?, "%Y%m%d%H%M%S").ok()
}

/// The newest `count` complete stamps old enough to anchor on, NEWEST
/// FIRST. Stamps younger than [`STAMP_SETTLE_MINUTES`] are still inside
/// ANM's in-place rewrite window (module doc) and are passed over; when
/// NOTHING is old enough (stalled feed timestamps in the future, or local
/// clock skew), the newest complete stamps are returned regardless so the
/// site stays pollable — the decoder's IMAGE rejection is the backstop.
fn anchor_stamps(
    groups: &BTreeMap<String, BTreeMap<&'static str, String>>,
    count: usize,
    now: DateTime<Utc>,
) -> Vec<String> {
    let count = count.max(1);
    let complete = || {
        groups
            .iter()
            .rev()
            .filter(|(_, group)| is_complete(group))
            .map(|(stamp, _)| stamp)
    };
    let settled: Vec<String> = complete()
        .filter(|stamp| {
            stamp_time(stamp)
                .is_some_and(|time| (now.naive_utc() - time).num_minutes() >= STAMP_SETTLE_MINUTES)
        })
        .take(count)
        .cloned()
        .collect();
    if !settled.is_empty() {
        return settled;
    }
    complete().take(count).cloned().collect()
}

/// Build one plan per stamp from the grouped listing, OLDEST FIRST (pure;
/// unit-testable). Part order per frame: [`MOMENT_ORDER`] — dBZ (merge
/// base), V, then each dual-pol moment present at that stamp.
fn plans_from_groups(
    site_id: &str,
    stamps_newest_first: &[String],
    groups: &BTreeMap<String, BTreeMap<&'static str, String>>,
) -> Vec<FramePlan> {
    let site_url = format!("{ANM_RADAR_ROOT}{site_id}");
    stamps_newest_first
        .iter()
        .rev()
        .filter_map(|stamp| {
            let group = groups.get(stamp)?;
            let parts: Vec<PlanPart> = MOMENT_ORDER
                .iter()
                .filter_map(|moment| group.get(moment))
                .map(|name| PlanPart {
                    url: join_url(&site_url, name),
                })
                .collect();
            if parts.len() < REQUIRED_MOMENTS {
                return None;
            }
            Some(FramePlan {
                identity: plan_identity(site_id, stamp, &parts),
                parts,
                merge: true,
            })
        })
        .collect()
}

/// `{site}_{stamp}_p{N}_h{url-hash}` (the SHMU identity grammar): stable
/// for one upstream frame, and a late-arriving dual-pol moment at the same
/// timestamp changes the part count/hash so the poller picks the richer
/// frame up.
fn plan_identity(site_id: &str, stamp: &str, parts: &[PlanPart]) -> String {
    let joined = parts
        .iter()
        .map(|part| part.url.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "{site_id}_{stamp}_p{}_h{:016x}",
        parts.len(),
        fnv1a64(&joined)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// Live capture 2026-07-07 ~18:52 UTC, trimmed to the oldest stamp and
    /// the three newest (the full page listed 464 stamps / 3,245 files).
    const BUC_LISTING: &str = include_str!("fixtures/meteoromania_buc_listing.html");

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
    }

    #[test]
    fn listing_fixture_groups_volume_moments_and_skips_cartesian_products() {
        let groups = stamp_moment_groups(BUC_LISTING, "BUC");
        assert_eq!(
            groups.keys().collect::<Vec<_>>(),
            vec![
                "2026070420550200",
                "2026070718350200",
                "2026070718400200",
                "2026070718450200",
            ],
            "grouped by the full 16-digit stamp, oldest window edge kept"
        );
        for (stamp, group) in &groups {
            assert_eq!(
                group.keys().copied().collect::<Vec<_>>(),
                // BTreeMap order, not merge order — merge order is the
                // plan builder's job.
                vec!["KDP", "RhoHV", "V", "ZDR", "dBZ"],
                "{stamp}: all five volume moments, dBR/Height skipped"
            );
        }
        // Names come back verbatim for URL joining.
        assert_eq!(
            groups["2026070718450200"]["dBZ"],
            "BUC_2026070718450200dBZ.hdf"
        );
    }

    #[test]
    fn volume_moment_classifies_names_and_rejects_foreign_ones() {
        assert_eq!(
            volume_moment("BUC_2026070718450200dBZ.hdf", "BUC"),
            Some(("2026070718450200".to_owned(), "dBZ"))
        );
        assert_eq!(
            volume_moment("BUC_2026070718450200RhoHV.hdf", "BUC"),
            Some(("2026070718450200".to_owned(), "RhoHV"))
        );
        // The Cartesian IMAGE products are recognized and skipped.
        assert_eq!(volume_moment("BUC_2026070718450200dBR.hdf", "BUC"), None);
        assert_eq!(volume_moment("BUC_2026070718450200Height.hdf", "BUC"), None);
        // Foreign shapes: wrong site, unknown product, no stamp, no .hdf.
        assert_eq!(volume_moment("BAR_2026070718450200dBZ.hdf", "BUC"), None);
        assert_eq!(volume_moment("BUC_2026070718450200dBuZ.hdf", "BUC"), None);
        assert_eq!(volume_moment("BUC_latestdBZ.hdf", "BUC"), None);
        assert_eq!(volume_moment("BUC_2026070718450200dBZ", "BUC"), None);
        assert_eq!(volume_moment("BUC_2026070718450200.hdf", "BUC"), None);
    }

    #[test]
    fn seven_moment_stamp_builds_a_five_part_merge_plan_in_moment_order() {
        let groups = stamp_moment_groups(BUC_LISTING, "BUC");
        // 18:52:44Z: the newest stamp (18:45:02) is 7 minutes old — still
        // inside ANM's in-place rewrite window — so 18:40:02 anchors.
        let stamps = anchor_stamps(&groups, 1, utc(2026, 7, 7, 18, 52, 44));
        assert_eq!(stamps, vec!["2026070718400200".to_owned()]);

        let plans = plans_from_groups("BUC", &stamps, &groups);
        assert_eq!(plans.len(), 1);
        let plan = &plans[0];
        assert!(plan.merge);
        assert!(plan.identity.starts_with("BUC_2026070718400200_p5_h"));
        let urls: Vec<&str> = plan.parts.iter().map(|part| part.url.as_str()).collect();
        assert_eq!(
            urls,
            vec![
                "https://opendata.meteoromania.ro/radar/BUC/BUC_2026070718400200dBZ.hdf",
                "https://opendata.meteoromania.ro/radar/BUC/BUC_2026070718400200V.hdf",
                "https://opendata.meteoromania.ro/radar/BUC/BUC_2026070718400200ZDR.hdf",
                "https://opendata.meteoromania.ro/radar/BUC/BUC_2026070718400200KDP.hdf",
                "https://opendata.meteoromania.ro/radar/BUC/BUC_2026070718400200RhoHV.hdf",
            ],
            "dBZ is the merge base, V second, dual-pol tail after"
        );
    }

    #[test]
    fn settle_window_passes_over_fresh_stamps_and_recovers_once_settled() {
        let groups = stamp_moment_groups(BUC_LISTING, "BUC");
        // 18:55:00Z: 18:45:02 is 9m58s old — settled, anchors.
        assert_eq!(
            anchor_stamps(&groups, 1, utc(2026, 7, 7, 18, 55, 0)),
            vec!["2026070718450200".to_owned()]
        );
        // 18:46:00Z: 18:45 and 18:40 both unsettled; 18:35:02 anchors.
        assert_eq!(
            anchor_stamps(&groups, 1, utc(2026, 7, 7, 18, 46, 0)),
            vec!["2026070718350200".to_owned()]
        );
        // Clock far behind upstream: nothing looks settled — fall back to
        // the newest complete stamp instead of going dark.
        assert_eq!(
            anchor_stamps(&groups, 1, utc(2026, 7, 1, 0, 0, 0)),
            vec!["2026070718450200".to_owned()]
        );
    }

    /// A trailing stamp whose PVOL uploads are still missing required
    /// moments must not anchor a frame, and a missing dual-pol moment
    /// shrinks the plan instead of failing it.
    #[test]
    fn missing_moment_tail_skips_required_and_shrinks_optional() {
        let listing = concat!(
            r#"<a href="TIM_2026070718400200dBZ.hdf">x</a>"#,
            r#"<a href="TIM_2026070718400200V.hdf">x</a>"#,
            r#"<a href="TIM_2026070718400200ZDR.hdf">x</a>"#,
            r#"<a href="TIM_2026070718400200KDP.hdf">x</a>"#,
            // 18:45: dual-pol tail lagging — RhoHV missing everywhere,
            // and the newest stamp has no V yet (only dBZ + Height).
            r#"<a href="TIM_2026070718450200dBZ.hdf">x</a>"#,
            r#"<a href="TIM_2026070718450200Height.hdf">x</a>"#,
        );
        let groups = stamp_moment_groups(listing, "TIM");
        let stamps = anchor_stamps(&groups, 5, utc(2026, 7, 7, 19, 30, 0));
        assert_eq!(
            stamps,
            vec!["2026070718400200".to_owned()],
            "the V-less tail stamp never anchors"
        );
        let plans = plans_from_groups("TIM", &stamps, &groups);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].parts.len(), 4, "no RhoHV part at this stamp");
        assert!(plans[0].identity.starts_with("TIM_2026070718400200_p4_h"));
        assert!(
            plans[0].parts[3].url.ends_with("KDP.hdf"),
            "tail keeps moment order without RhoHV"
        );
    }

    /// Plans come back OLDEST FIRST with the newest frame LAST — and the
    /// newest frame is exactly the single frame a count-1 (`latest`) build
    /// produces, so the loop ends on the poll dedupe key.
    #[test]
    fn recent_plans_are_oldest_first_and_end_on_the_latest_frame() {
        let groups = stamp_moment_groups(BUC_LISTING, "BUC");
        let now = utc(2026, 7, 7, 18, 55, 0);
        let stamps = anchor_stamps(&groups, 3, now);
        assert_eq!(
            stamps,
            vec![
                "2026070718450200".to_owned(),
                "2026070718400200".to_owned(),
                "2026070718350200".to_owned(),
            ],
            "NEWEST FIRST from the anchor pick"
        );
        let plans = plans_from_groups("BUC", &stamps, &groups);
        assert_eq!(plans.len(), 3);
        assert!(plans[0].identity.starts_with("BUC_2026070718350200_p5_h"));
        assert!(plans[2].identity.starts_with("BUC_2026070718450200_p5_h"));
        assert!(plans.iter().all(|plan| plan.merge));

        let latest_stamps = anchor_stamps(&groups, 1, now);
        let latest = plans_from_groups("BUC", &latest_stamps, &groups);
        assert_eq!(plans.last(), latest.first());
    }

    #[test]
    fn identity_is_stable_and_part_sensitive() {
        let parts = vec![
            PlanPart {
                url: "https://a/1.hdf".to_owned(),
            },
            PlanPart {
                url: "https://a/2.hdf".to_owned(),
            },
        ];
        let identity = plan_identity("BUC", "2026070718450200", &parts);
        assert_eq!(identity, plan_identity("BUC", "2026070718450200", &parts));
        assert!(identity.starts_with("BUC_2026070718450200_p2_h"));
        assert_ne!(
            identity,
            plan_identity("BUC", "2026070718450200", &parts[..1])
        );
    }

    /// The provider must advertise the real loop it has, and its static
    /// table must mirror the network: seven stations whose ids are the
    /// upstream directory names and whose coordinates agree with the ORD
    /// `ro*` rows (same radars, `NOD:ro...` sources — the table values are
    /// the live PVOL `/where` groups rounded to 4 decimals).
    #[test]
    fn site_table_lists_the_seven_anm_radars_with_ord_consistent_coords() {
        let provider = MeteoRomaniaProvider::new();
        assert!(provider.recent_source().is_some());
        assert!(provider.supports_recent());
        assert!(
            !provider.supports_archive(),
            "ORD keeps the RO deep archive"
        );

        let sites = provider.list_sites().expect("static catalog");
        assert_eq!(sites, provider.static_sites());
        let rows: Vec<(&str, &str, f32, f32)> = sites
            .iter()
            .map(|site| {
                assert_eq!(site.provider_id, "meteoromania");
                assert_eq!(site.country, "Romania");
                (
                    site.site_id.as_str(),
                    site.label.as_str(),
                    site.latitude_deg.unwrap(),
                    site.longitude_deg.unwrap(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                ("BAR", "Bârnova", 47.0118, 27.5825),
                ("BOB", "Bobohalma", 46.3602, 24.2252),
                ("BUC", "București", 44.5127, 26.0773),
                ("CRA", "Craiova", 44.3103, 23.8674),
                ("MED", "Medgidia", 44.2434, 28.2506),
                ("ORA", "Oradea", 47.0922, 21.9429),
                ("TIM", "Timișoara", 45.7717, 21.2577),
            ]
        );
    }

    #[test]
    fn unknown_site_ids_are_rejected_before_any_fetch() {
        let err = anm_recent_plans("COMPOSITE", 1, utc(2026, 7, 7, 18, 55, 0)).unwrap_err();
        assert!(err.contains("unknown ANM site"), "{err}");
        let err = anm_recent_plans("../etc", 1, utc(2026, 7, 7, 18, 55, 0)).unwrap_err();
        assert!(err.contains("unknown ANM site"), "{err}");
    }

    /// Live ANM roundtrip: listing, plan, download, per-part ODIM decode
    /// (the poll consumer owns the merge). Network test; run with
    /// `cargo test -p data_source anm_live -- --ignored --nocapture`
    #[test]
    #[ignore = "live opendata.meteoromania.ro probe — run manually with --ignored"]
    fn anm_live_roundtrip_lists_plans_downloads_and_decodes() {
        let provider = MeteoRomaniaProvider::new();
        let plan = provider.latest("BUC").expect("live BUC frame plan");
        println!("identity={} parts={}", plan.identity, plan.parts.len());
        assert!(plan.merge);
        assert!(plan.parts.len() >= REQUIRED_MOMENTS);

        for part in &plan.parts {
            println!("downloading {}", part.url);
            let bytes = crate::fetch_volume_bytes(&part.url).expect("live download");
            let volume =
                nexrad_io::decode_supported_volume_bytes(&bytes).expect("ODIM PVOL decode");
            println!(
                "decoded {}: {} cuts, {} radials",
                volume.site.id,
                volume.cuts.len(),
                volume.metadata.decoded_radial_count
            );
            // The shared ODIM decode uppercases the NOD source code.
            assert_eq!(volume.site.id, "ROBUC", "ODIM NOD source for BUC");
            assert!(!volume.cuts.is_empty());
        }
    }
}
