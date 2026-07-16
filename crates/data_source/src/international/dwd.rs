//! DWD (Deutscher Wetterdienst, Germany) per-sweep radar volume feed.
//!
//! Catalog: `https://opendata.dwd.de/weather/radar/sites/`, an nginx
//! autoindex tree (captured live 2026-06-12 UTC):
//!
//! ```text
//! sites/
//!   sweep_vol_z/asb/hdf5/filter_polarimetric/
//!     ras07-vol5minng01_sweeph5onem_dbzh_00-2026061206455700-asb-10103-hd5
//!     ... (quality-controlled reflectivity; one file per sweep index 00..09)
//!   sweep_vol_z/asb/unfiltered/        product / station / raw fallback
//!     ras07-vol5minng01_sweeph5onem_th_00-2026061206455700-asb-10103-hd5
//!   sweep_vol_v/asb/hdf5/filter_polarimetric/   (no unfiltered variant)
//!     ras07-vol5minng01_sweeph5onem_vradh_00-2026061206455700-asb-10103-hd5
//! ```
//!
//! Each file is ONE sweep (single ODIM_H5 dataset; EUMETNET OPERA Data
//! Information Model, Michelson et al., OPERA WP 2.1/2.2, v2.2-2.3) of one
//! quantity, so a full volume is 10 sweeps x N products merged with
//! `radar_core::merge_radar_volumes`.
//!
//! DWD publishes the operational reflectivity in three variants. BowEcho
//! prefers `filter_polarimetric`, then `filter_simple`, and uses
//! `unfiltered` only as a compatibility fallback. The filtered directories
//! are the already quality-controlled products; their ODIM data plane has
//! the clutter decisions applied directly and does not require a separate
//! quality-mask decode. The provider pins explicit timestamped files for
//! every part instead of following mutable LATEST aliases, keeping every
//! [`FramePlan`] immutable and cycle-consistent.
//!
//! Cycle resolution: file timestamps are sweep start times
//! (`YYYYMMDDHHMMSScc`, centisecond suffix). Within one `vol5minng01` cycle
//! the sweeps run in ascending index order over ~3 minutes (live capture:
//! `dbzh_00` 06:40:57 ... `dbzh_09` 06:44:02, repeating every 5 minutes), so
//! the newest timestamp of the HIGHEST sweep index marks the most recent
//! complete cycle, and every sweep's file for that cycle is the newest one
//! within the trailing 5-minute window.

use chrono::NaiveDateTime;

use super::listing::{ListingEntry, fnv1a64, has_dir, join_url, parse_autoindex};
use super::{FramePlan, IntlProvider, IntlSite, PlanPart, RecentFrames};
use crate::{fetch_listing_text, fetch_text};

const DWD_SITES_ROOT: &str = "https://opendata.dwd.de/weather/radar/sites/";
/// One 5-minute scan cycle: a sweep belongs to the cycle ending at the
/// anchor when its start time is inside this trailing window.
const CYCLE_WINDOW_MINUTES: i64 = 5;
/// DWD's operational `vol5minng01` ladder is exactly sweep indices 00..09.
/// Seeing the terminal sweep is necessary but not sufficient: a listing can
/// expose sweep 09 while an intermediate object is absent or still settling.
const EXPECTED_SWEEP_FIRST: u8 = 0;
const EXPECTED_SWEEP_LAST: u8 = 9;

/// One DWD sweep product directory and the ODIM quantities accepted for it,
/// in preference order (first match in the resolved variant directory
/// wins). The unfiltered variant publishes total power `th` for
/// `sweep_vol_z` and `u`-prefixed dual-pol quantities; the `hdf5/filter_*`
/// variants publish clutter-filtered `dbzh`/`vradh`.
struct DwdProduct {
    dir: &'static str,
    quantities: &'static [&'static str],
    required: bool,
}

const DWD_PRODUCTS: [DwdProduct; 5] = [
    DwdProduct {
        dir: "sweep_vol_z",
        quantities: &["dbzh", "zh", "th"],
        required: true,
    },
    DwdProduct {
        dir: "sweep_vol_v",
        quantities: &["vradh", "vradv"],
        required: true,
    },
    DwdProduct {
        dir: "sweep_vol_zdr",
        quantities: &["zdr", "uzdr"],
        required: false,
    },
    DwdProduct {
        dir: "sweep_vol_rhohv",
        quantities: &["rhohv", "urhohv"],
        required: false,
    },
    DwdProduct {
        dir: "sweep_vol_phidp",
        quantities: &["phidp", "uphidp"],
        required: false,
    },
];

/// DWD radar network station labels (place names, DWD station catalog) and
/// radar coordinates (the open-data catalog tree carries none).
///
/// Coordinates: EUMETNET OPERA radar database, `OPERA_RADARS_DB.json`
/// (fetched 2026-06-12) from
/// <https://eumetnet.eu/activities/observations-programme/current-activities/opera/>,
/// matched by the `de{code}` ODIM site code directly (e.g. `asb` ->
/// `deasb`). All seventeen stations are listed operational (status 1).
const DWD_STATIONS: [(&str, &str, f32, f32); 17] = [
    ("asb", "Borkum (ASR)", 53.5640, 6.7482),
    ("boo", "Boostedt", 54.0043, 10.0468),
    ("drs", "Dresden", 51.1246, 13.7686),
    ("eis", "Eisberg", 49.5407, 12.4028),
    ("ess", "Essen", 51.4055, 6.9669),
    ("fbg", "Feldberg", 47.8736, 8.0039),
    ("fld", "Flechtdorf", 51.3112, 8.8020),
    ("hnr", "Hannover", 52.4600, 9.6945),
    ("isn", "Isen", 48.1747, 12.1017),
    ("mem", "Memmingen", 48.0421, 10.2192),
    ("neu", "Neuhaus", 50.5001, 11.1351),
    ("nhb", "Neuheilenbach", 50.1097, 6.5483),
    ("oft", "Offenthal", 49.9847, 8.7129),
    ("pro", "Prötzel", 52.6486, 13.8580),
    ("ros", "Rostock", 54.1757, 12.0580),
    ("tur", "Türkheim", 48.5853, 9.7828),
    ("umd", "Ummendorf", 52.1601, 11.1761),
];

/// Germany's DWD open-data sweep feed (one file per sweep per product).
#[derive(Clone, Copy, Debug)]
pub struct DwdProvider {
    /// Also assemble every ZDR/RhoHV/PhiDP product available at the selected
    /// radar. Individual optional products may be absent at a station.
    include_dual_pol: bool,
}

impl DwdProvider {
    pub fn new() -> Self {
        Self {
            include_dual_pol: false,
        }
    }

    /// Assemble every available ZDR, RhoHV, and PhiDP sweep too.
    pub fn with_dual_pol() -> Self {
        Self {
            include_dual_pol: true,
        }
    }

    fn included_products(&self) -> impl Iterator<Item = &'static DwdProduct> {
        let include_dual_pol = self.include_dual_pol;
        DWD_PRODUCTS
            .iter()
            .filter(move |product| product.required || include_dual_pol)
    }

    /// Fetch and parse the sweep listing of every included product (the
    /// one catalog probe both `latest` and `recent` run). The first entry
    /// is always the required `sweep_vol_z` listing with at least one
    /// timestamped sweep — missing or empty required listings are errors —
    /// so callers can anchor cycles on `products[0]`.
    fn fetch_product_sweeps(&self, site_id: &str) -> Result<Vec<DwdProductSweeps>, String> {
        let mut products = Vec::new();
        for product in self.included_products() {
            let resolved = match resolve_product_dir(site_id, product) {
                Ok(resolved) => resolved,
                Err(err) if product.required => return Err(err),
                Err(_) => continue,
            };
            let sweeps = parse_dwd_sweeps(&resolved.entries, resolved.quantity);
            if sweeps.is_empty() {
                if product.required {
                    return Err(format!(
                        "DWD {}/{site_id}: no timestamped '{}' sweep files in {}",
                        product.dir, resolved.quantity, resolved.dir_url
                    ));
                }
                continue;
            }
            products.push(DwdProductSweeps {
                dir: product.dir,
                required: product.required,
                dir_url: resolved.dir_url,
                quantity: resolved.quantity,
                sweeps,
            });
        }
        if products.is_empty() {
            return Err(format!("DWD: no products resolved for site '{site_id}'"));
        }
        Ok(products)
    }
}

impl Default for DwdProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl IntlProvider for DwdProvider {
    fn id(&self) -> &'static str {
        "dwd"
    }

    fn label(&self) -> &'static str {
        "DWD Germany"
    }

    fn country(&self) -> &'static str {
        "Germany"
    }

    fn list_sites(&self) -> Result<Vec<IntlSite>, String> {
        let stations_url = format!("{DWD_SITES_ROOT}sweep_vol_z/");
        let html = fetch_text(&stations_url)
            .map_err(|err| format!("DWD station listing {stations_url}: {err}"))?;
        let mut sites: Vec<IntlSite> = parse_autoindex(&html)
            .into_iter()
            .filter(|entry| entry.is_dir)
            .map(|entry| {
                let known = DWD_STATIONS.iter().find(|(id, ..)| *id == entry.name);
                IntlSite {
                    provider_id: self.id(),
                    label: known.map_or_else(
                        || entry.name.to_ascii_uppercase(),
                        |(_, label, _, _)| (*label).to_owned(),
                    ),
                    // The catalog tree carries no coordinates; the static
                    // station table (OPERA database) does.
                    latitude_deg: known.map(|&(_, _, latitude_deg, _)| latitude_deg),
                    longitude_deg: known.map(|&(_, _, _, longitude_deg)| longitude_deg),
                    site_id: entry.name,
                    country: self.country(),
                }
            })
            .collect();
        if sites.is_empty() {
            return Err(format!(
                "DWD station listing {stations_url} held no station directories"
            ));
        }
        sites.sort_by(|left, right| left.site_id.cmp(&right.site_id));
        Ok(sites)
    }

    fn latest(&self, site_id: &str) -> Result<FramePlan, String> {
        if !is_safe_path_segment(site_id) {
            return Err(format!("DWD: invalid site id '{site_id}'"));
        }
        let products = self.fetch_product_sweeps(site_id)?;
        // The base product (first required, sweep_vol_z) anchors the cycle
        // for every other product.
        let Some(anchor) = cycle_anchors(&products[0].sweeps, 1).into_iter().next() else {
            return Err(format!(
                "DWD {}/{site_id}: could not resolve a cycle anchor from {}",
                products[0].dir, products[0].dir_url
            ));
        };
        assemble_cycle(site_id, anchor, &products)
    }

    fn recent_source(&self) -> Option<&dyn RecentFrames> {
        Some(self)
    }

    fn static_sites(&self) -> Vec<IntlSite> {
        DWD_STATIONS
            .iter()
            .map(|&(code, label, latitude_deg, longitude_deg)| IntlSite {
                provider_id: self.id(),
                site_id: code.to_owned(),
                label: label.to_owned(),
                country: self.country(),
                latitude_deg: Some(latitude_deg),
                longitude_deg: Some(longitude_deg),
            })
            .collect()
    }
}

impl RecentFrames for DwdProvider {
    fn recent_frames(&self, site_id: &str, count: usize) -> Result<Vec<FramePlan>, String> {
        if !is_safe_path_segment(site_id) {
            return Err(format!("DWD: invalid site id '{site_id}'"));
        }
        let products = self.fetch_product_sweeps(site_id)?;
        let anchors = cycle_anchors(&products[0].sweeps, count);
        if anchors.is_empty() {
            return Err(format!(
                "DWD {}/{site_id}: could not resolve a cycle anchor from {}",
                products[0].dir, products[0].dir_url
            ));
        }
        let mut plans = Vec::with_capacity(anchors.len());
        for (index, anchor) in anchors.iter().enumerate() {
            match assemble_cycle(site_id, *anchor, &products) {
                Ok(plan) => plans.push(plan),
                // The newest cycle is the poll dedupe key: its failure is
                // the loop's failure. An older, partially-retained cycle
                // just shortens the loop.
                Err(err) if index == 0 => return Err(err),
                Err(_) => continue,
            }
        }
        plans.reverse();
        Ok(plans)
    }
}

/// One fetched-and-parsed product sweep listing.
struct DwdProductSweeps {
    dir: &'static str,
    required: bool,
    dir_url: String,
    quantity: &'static str,
    sweeps: Vec<DwdSweepFile>,
}

/// Up to `count` candidate cycle anchors, NEWEST FIRST: the distinct
/// timestamps of expected terminal sweep 09. Sweeps scan in ascending index
/// order, so sweep 09 marks a candidate cycle end (the next cycle's low
/// sweeps may already be uploaded). [`assemble_cycle`] separately requires
/// every expected index in every required product before publishing it.
/// `cycle_anchors(sweeps, 1)` is exactly the anchor `latest` uses.
fn cycle_anchors(sweeps: &[DwdSweepFile], count: usize) -> Vec<NaiveDateTime> {
    let mut times: Vec<NaiveDateTime> = sweeps
        .iter()
        .filter(|sweep| sweep.sweep == EXPECTED_SWEEP_LAST)
        .map(|sweep| sweep.time)
        .collect();
    times.sort_unstable();
    times.dedup();
    times.reverse();
    times.truncate(count.max(1));
    times
}

/// Assemble the frame for the cycle ending at `anchor` from
/// already-fetched product sweep listings (pure; unit-testable). Identity
/// and part order are exactly what `latest` has always produced for its
/// (newest) anchor.
fn assemble_cycle(
    site_id: &str,
    anchor: NaiveDateTime,
    products: &[DwdProductSweeps],
) -> Result<FramePlan, String> {
    let mut parts: Vec<PlanPart> = Vec::new();
    for product in products {
        let chosen = sweeps_in_cycle(&product.sweeps, anchor);
        let missing = missing_expected_sweeps(&chosen);
        if product.required && !missing.is_empty() {
            return Err(format!(
                "DWD {}/{site_id}: '{}' cycle ending {anchor} is incomplete; \
                 missing expected sweep indices {} ({} timestamped files inspected)",
                product.dir,
                product.quantity,
                format_sweep_indices(&missing),
                product.sweeps.len()
            ));
        }
        parts.extend(chosen.into_iter().map(|sweep| PlanPart {
            url: join_url(&product.dir_url, &sweep.name),
        }));
    }
    let joined = parts
        .iter()
        .map(|part| part.url.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    Ok(FramePlan {
        identity: format!(
            "{site_id}_{}_p{}_h{:016x}",
            anchor.format("%Y%m%d%H%M%S"),
            parts.len(),
            fnv1a64(&joined)
        ),
        parts,
        merge: true,
    })
}

/// A product directory resolved down to the variant that actually carries
/// data files, plus the quantity chosen from it.
struct ResolvedProductDir {
    dir_url: String,
    entries: Vec<ListingEntry>,
    quantity: &'static str,
}

/// Return the available variant URLs in scientific preference order.
///
/// DWD's `filter_polarimetric` reflectivity has the dual-polarization clutter
/// classification applied; `filter_simple` is the less capable filtered
/// fallback. Raw `unfiltered` TH/UZDR/URHOHV/UPHIDP remains useful where a
/// station does not publish a filtered version of that moment.
fn variant_dir_urls(
    station_url: &str,
    station_entries: &[ListingEntry],
    hdf5_entries: Option<&[ListingEntry]>,
) -> Vec<String> {
    let mut urls = Vec::new();
    if let Some(hdf5_entries) = hdf5_entries {
        for filter in ["filter_polarimetric", "filter_simple"] {
            if has_dir(hdf5_entries, filter) {
                urls.push(format!("{station_url}hdf5/{filter}/"));
            }
        }
    }
    if has_dir(station_entries, "unfiltered") {
        urls.push(format!("{station_url}unfiltered/"));
    }
    urls
}

/// Resolve `sites/{product}/{site}/` to the first variant that actually
/// carries one of the product's supported quantities. A temporarily absent
/// or empty preferred directory falls through to the next variant instead
/// of taking the whole station offline.
fn resolve_product_dir(site_id: &str, product: &DwdProduct) -> Result<ResolvedProductDir, String> {
    let station_url = format!("{DWD_SITES_ROOT}{}/{site_id}/", product.dir);
    let station_html =
        fetch_text(&station_url).map_err(|err| format!("DWD station dir {station_url}: {err}"))?;
    let station_entries = parse_autoindex(&station_html);

    let mut attempts = Vec::new();
    let hdf5_entries = if has_dir(&station_entries, "hdf5") {
        let hdf5_url = format!("{station_url}hdf5/");
        match fetch_text(&hdf5_url) {
            Ok(html) => Some(parse_autoindex(&html)),
            Err(err) => {
                attempts.push(format!("filter catalog {hdf5_url}: {err}"));
                None
            }
        }
    } else {
        None
    };

    let dir_urls = variant_dir_urls(&station_url, &station_entries, hdf5_entries.as_deref());
    if dir_urls.is_empty() {
        attempts.push(format!(
            "station dir {station_url}: no filter_polarimetric, filter_simple, or unfiltered variant"
        ));
    }

    for dir_url in dir_urls {
        // Sweep listings run ~2 MB (full retention, no server gzip): use the
        // long-timeout listing fetch.
        let html = match fetch_listing_text(&dir_url) {
            Ok(html) => html,
            Err(err) => {
                attempts.push(format!("sweep listing {dir_url}: {err}"));
                continue;
            }
        };
        let entries = parse_autoindex(&html);
        let quantity = product
            .quantities
            .iter()
            .find(|quantity| !parse_dwd_sweeps(&entries, quantity).is_empty());
        if let Some(&quantity) = quantity {
            return Ok(ResolvedProductDir {
                dir_url,
                entries,
                quantity,
            });
        }
        attempts.push(format!(
            "sweep listing {dir_url}: no timestamped sweeps for quantities {:?}",
            product.quantities
        ));
    }

    Err(format!(
        "DWD {}/{site_id}: no usable product variant ({})",
        product.dir,
        attempts.join("; ")
    ))
}

fn quantity_marker(quantity: &str) -> String {
    format!("_sweeph5onem_{quantity}_")
}

/// One timestamped sweep file (LATEST aliases are excluded).
#[derive(Clone, Debug, PartialEq)]
struct DwdSweepFile {
    sweep: u8,
    time: NaiveDateTime,
    name: String,
}

/// Parse `..._sweeph5onem_{quantity}_NN-YYYYMMDDHHMMSScc-...` entries.
fn parse_dwd_sweeps(entries: &[ListingEntry], quantity: &str) -> Vec<DwdSweepFile> {
    let marker = quantity_marker(quantity);
    entries
        .iter()
        .filter(|entry| !entry.is_dir)
        .filter_map(|entry| {
            let after = &entry.name[entry.name.find(&marker)? + marker.len()..];
            let (sweep_digits, after_sweep) = after.split_at_checked(2)?;
            let sweep = sweep_digits.parse::<u8>().ok()?;
            if !(EXPECTED_SWEEP_FIRST..=EXPECTED_SWEEP_LAST).contains(&sweep) {
                return None;
            }
            let stamp = after_sweep.strip_prefix('-')?.get(..16)?;
            if !stamp.bytes().all(|byte| byte.is_ascii_digit()) {
                return None; // LATEST alias or unexpected naming.
            }
            let time = NaiveDateTime::parse_from_str(&stamp[..14], "%Y%m%d%H%M%S").ok()?;
            Some(DwdSweepFile {
                sweep,
                time,
                name: entry.name.clone(),
            })
        })
        .collect()
}

/// For every sweep index, the newest file whose start time falls inside
/// the cycle ending at `anchor` (exclusive 5 minutes before, inclusive at
/// the anchor), in ascending sweep order.
fn sweeps_in_cycle(sweeps: &[DwdSweepFile], anchor: NaiveDateTime) -> Vec<DwdSweepFile> {
    let window_start = anchor - chrono::Duration::minutes(CYCLE_WINDOW_MINUTES);
    let mut newest_per_sweep: Vec<DwdSweepFile> = Vec::new();
    for sweep in sweeps {
        if sweep.time <= window_start || sweep.time > anchor {
            continue;
        }
        match newest_per_sweep
            .iter_mut()
            .find(|chosen| chosen.sweep == sweep.sweep)
        {
            Some(chosen) => {
                if sweep.time > chosen.time {
                    *chosen = sweep.clone();
                }
            }
            None => newest_per_sweep.push(sweep.clone()),
        }
    }
    newest_per_sweep.sort_by_key(|sweep| sweep.sweep);
    newest_per_sweep
}

fn missing_expected_sweeps(sweeps: &[DwdSweepFile]) -> Vec<u8> {
    (EXPECTED_SWEEP_FIRST..=EXPECTED_SWEEP_LAST)
        .filter(|expected| !sweeps.iter().any(|sweep| sweep.sweep == *expected))
        .collect()
}

fn format_sweep_indices(indices: &[u8]) -> String {
    indices
        .iter()
        .map(|index| format!("{index:02}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn is_safe_path_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    const Z_STATION_DIR: &str = include_str!("../../tests/fixtures/dwd_asb_z_station_dir.html");
    const V_STATION_DIR: &str = include_str!("../../tests/fixtures/dwd_asb_v_station_dir.html");
    const V_HDF5_DIR: &str = include_str!("../../tests/fixtures/dwd_asb_v_hdf5_dir.html");
    const Z_FILES: &str = include_str!("../../tests/fixtures/dwd_asb_z_unfiltered_files.html");
    const V_FILES: &str =
        include_str!("../../tests/fixtures/dwd_asb_v_filter_polarimetric_files.html");

    fn timestamp(value: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(value, "%Y%m%d%H%M%S").expect("test timestamp")
    }

    #[test]
    fn station_variant_dirs_match_the_live_layout() {
        // sweep_vol_z/asb has BOTH unfiltered/ and hdf5/. The provider must
        // use DWD's quality-controlled variants before raw TH; the old order
        // selected unfiltered first and exposed the Memmingen clutter.
        let z_entries = parse_autoindex(Z_STATION_DIR);
        assert!(has_dir(&z_entries, "unfiltered"));
        assert!(has_dir(&z_entries, "hdf5"));
        let v_hdf5 = parse_autoindex(V_HDF5_DIR);
        assert!(has_dir(&v_hdf5, "filter_polarimetric"));
        assert!(has_dir(&v_hdf5, "filter_simple"));
        assert_eq!(
            variant_dir_urls(
                "https://opendata.dwd.de/weather/radar/sites/sweep_vol_z/asb/",
                &z_entries,
                Some(&v_hdf5),
            ),
            [
                "https://opendata.dwd.de/weather/radar/sites/sweep_vol_z/asb/hdf5/\
                 filter_polarimetric/",
                "https://opendata.dwd.de/weather/radar/sites/sweep_vol_z/asb/hdf5/\
                 filter_simple/",
                "https://opendata.dwd.de/weather/radar/sites/sweep_vol_z/asb/unfiltered/",
            ]
        );

        // sweep_vol_v/asb has only hdf5/, so it never invents an unfiltered
        // fallback that the official tree does not expose.
        let v_entries = parse_autoindex(V_STATION_DIR);
        assert!(!has_dir(&v_entries, "unfiltered"));
        assert!(has_dir(&v_entries, "hdf5"));
        assert_eq!(
            variant_dir_urls(
                "https://opendata.dwd.de/weather/radar/sites/sweep_vol_v/asb/",
                &v_entries,
                Some(&v_hdf5),
            )
            .len(),
            2
        );
    }

    #[test]
    fn raw_variant_remains_a_fallback_when_filtered_catalog_is_unavailable() {
        let z_entries = parse_autoindex(Z_STATION_DIR);
        assert_eq!(
            variant_dir_urls(
                "https://opendata.dwd.de/weather/radar/sites/sweep_vol_z/asb/",
                &z_entries,
                None,
            ),
            ["https://opendata.dwd.de/weather/radar/sites/sweep_vol_z/asb/unfiltered/"]
        );
    }

    #[test]
    fn sweep_parser_reads_quantity_index_and_stamp_and_skips_latest() {
        let entries = parse_autoindex(Z_FILES);
        let th = parse_dwd_sweeps(&entries, "th");
        // Trimmed capture: 3 timestamped files per sweep index, 10 indices;
        // the 10 LATEST aliases parse out.
        assert_eq!(th.len(), 30);
        assert!(th.iter().all(|sweep| sweep.sweep <= 9));
        assert!(th.iter().any(
            |sweep| sweep.name.ends_with("th_09-2026061206440200-asb-10103-hd5")
                && sweep.time == timestamp("20260612064402")
        ));
        // tv coexists in the same directory and does not leak into th.
        assert!(th.iter().all(|sweep| sweep.name.contains("_th_")));
        assert_eq!(parse_dwd_sweeps(&entries, "tv").len(), 30);
        assert!(parse_dwd_sweeps(&entries, "dbzh").is_empty());
    }

    #[test]
    fn anchors_are_newest_stamps_of_expected_terminal_sweep_newest_first() {
        let entries = parse_autoindex(Z_FILES);
        let th = parse_dwd_sweeps(&entries, "th");
        assert_eq!(cycle_anchors(&th, 1), vec![timestamp("20260612064402")]);
        // Every th_09 stamp in the trimmed capture ends one cycle; asking
        // for more stops at the listing's oldest cycle.
        assert_eq!(
            cycle_anchors(&th, 99),
            vec![
                timestamp("20260612064402"),
                timestamp("20260612063902"),
                timestamp("20260612063402"),
            ]
        );
        assert!(cycle_anchors(&[], 3).is_empty());
    }

    #[test]
    fn cycle_window_selects_one_file_per_sweep_of_the_complete_cycle() {
        let entries = parse_autoindex(Z_FILES);
        let th = parse_dwd_sweeps(&entries, "th");
        let anchor = cycle_anchors(&th, 1).into_iter().next().expect("anchor");
        let chosen = sweeps_in_cycle(&th, anchor);
        assert_eq!(chosen.len(), 10, "all ten sweeps of the cycle");
        assert_eq!(
            chosen.iter().map(|sweep| sweep.sweep).collect::<Vec<_>>(),
            (0..10).collect::<Vec<_>>()
        );
        // Live capture cycle: th_00 06:40:57 .. th_09 06:44:02. The NEXT
        // cycle's th_00 (06:45:57) is newer but after the anchor, and the
        // PREVIOUS cycle's th_09 (06:39:02) sits exactly on the exclusive
        // window edge.
        assert_eq!(chosen[0].time, timestamp("20260612064057"));
        assert_eq!(chosen[9].time, timestamp("20260612064402"));
        assert!(chosen.iter().all(|sweep| sweep.time <= anchor));
    }

    #[test]
    fn velocity_files_share_the_z_cycle_stamps() {
        let entries = parse_autoindex(V_FILES);
        let vradh = parse_dwd_sweeps(&entries, "vradh");
        assert_eq!(vradh.len(), 30);
        // Same cycle anchored from z (06:44:02): velocity sweeps carry the
        // identical start stamps.
        let chosen = sweeps_in_cycle(&vradh, timestamp("20260612064402"));
        assert_eq!(chosen.len(), 10);
        assert_eq!(chosen[0].time, timestamp("20260612064057"));
        assert_eq!(chosen[9].time, timestamp("20260612064402"));
    }

    /// The provider must advertise the real loop it now has (fails on the
    /// old single-frame DwdProvider).
    #[test]
    fn provider_advertises_a_real_recent_loop() {
        let provider = DwdProvider::new();
        assert!(provider.recent_source().is_some());
        assert!(provider.supports_recent());
    }

    fn fixture_products() -> Vec<DwdProductSweeps> {
        vec![
            DwdProductSweeps {
                dir: "sweep_vol_z",
                required: true,
                dir_url: "https://opendata.dwd.de/weather/radar/sites/sweep_vol_z/asb/unfiltered/"
                    .to_owned(),
                quantity: "th",
                sweeps: parse_dwd_sweeps(&parse_autoindex(Z_FILES), "th"),
            },
            DwdProductSweeps {
                dir: "sweep_vol_v",
                required: true,
                dir_url: "https://opendata.dwd.de/weather/radar/sites/sweep_vol_v/asb/hdf5/\
                          filter_polarimetric/"
                    .to_owned(),
                quantity: "vradh",
                sweeps: parse_dwd_sweeps(&parse_autoindex(V_FILES), "vradh"),
            },
        ]
    }

    /// Older cycles assemble from the same listings with the same identity
    /// grammar and part order as the newest one; a cycle the retention has
    /// already lost a required product for errors (and the recent loop
    /// drops it), so applying the recent walk to the fixtures yields two
    /// loop frames, oldest first, ending on the `latest` frame.
    #[test]
    fn older_cycles_assemble_and_partial_cycles_drop_out() {
        let products = fixture_products();
        let anchors = cycle_anchors(&products[0].sweeps, 3);
        assert_eq!(anchors.len(), 3);

        let newest = assemble_cycle("asb", anchors[0], &products).expect("newest cycle");
        assert!(newest.identity.starts_with("asb_20260612064402_p20_h"));
        assert!(newest.merge);
        assert_eq!(newest.parts.len(), 20, "10 th + 10 vradh sweeps");

        let older = assemble_cycle("asb", anchors[1], &products).expect("older cycle");
        assert!(older.identity.starts_with("asb_20260612063902_p20_h"));
        assert_ne!(older.identity, newest.identity);
        // Product-major (z before v), ascending sweep index inside each.
        assert!(older.parts[0].url.contains("/sweep_vol_z/"));
        assert!(older.parts[0].url.contains("_th_00-2026061206355700"));
        assert!(older.parts[9].url.contains("_th_09-2026061206390200"));
        assert!(older.parts[10].url.contains("/sweep_vol_v/"));
        assert!(older.parts[10].url.contains("_vradh_00-2026061206355700"));
        // Same upstream cycle -> same plan (dedupe key stability).
        assert_eq!(
            assemble_cycle("asb", anchors[1], &products).expect("older cycle again"),
            older
        );

        // The oldest fixture cycle (ending 06:34:02) predates complete
        // retention: reflectivity has already lost sweep 00 (and velocity has
        // lost the whole cycle), so the first incomplete required product is
        // rejected.
        let err = assemble_cycle("asb", anchors[2], &products).unwrap_err();
        assert!(
            err.contains("'th' cycle ending") && err.contains("missing expected sweep indices 00"),
            "unexpected error: {err}"
        );

        // The recent walk over these anchors: keep Ok frames, then reverse
        // to oldest-first — the last frame is the `latest` frame.
        let survivors: Vec<FramePlan> = anchors
            .iter()
            .filter_map(|anchor| assemble_cycle("asb", *anchor, &products).ok())
            .rev()
            .collect();
        assert_eq!(survivors.len(), 2);
        assert_eq!(survivors[0], older);
        assert_eq!(survivors[1], newest);
    }

    #[test]
    fn required_product_with_missing_middle_sweep_is_not_published() {
        let mut products = fixture_products();
        let anchor = timestamp("20260612064402");
        products[1]
            .sweeps
            .retain(|sweep| !(sweep.sweep == 4 && sweep.time > timestamp("20260612064000")));

        let err = assemble_cycle("asb", anchor, &products).unwrap_err();
        assert!(
            err.contains("sweep_vol_v/asb")
                && err.contains("'vradh' cycle ending")
                && err.contains("missing expected sweep indices 04"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn unavailable_optional_dual_pol_product_does_not_block_the_base_volume() {
        let mut products = fixture_products();
        products.push(DwdProductSweeps {
            dir: "sweep_vol_zdr",
            required: false,
            dir_url: "https://opendata.dwd.de/weather/radar/sites/sweep_vol_zdr/asb/unfiltered/"
                .to_owned(),
            quantity: "uzdr",
            sweeps: Vec::new(),
        });

        let plan = assemble_cycle("asb", timestamp("20260612064402"), &products)
            .expect("missing optional ZDR must not block REF+VEL");
        assert_eq!(plan.parts.len(), 20);
        assert!(plan.identity.starts_with("asb_20260612064402_p20_h"));
    }

    #[test]
    fn unexpected_higher_index_cannot_become_a_cycle_anchor() {
        let mut sweeps = parse_dwd_sweeps(&parse_autoindex(Z_FILES), "th");
        sweeps.push(DwdSweepFile {
            sweep: 99,
            time: timestamp("20260612064959"),
            name: "synthetic-unexpected-sweep".to_owned(),
        });

        assert_eq!(cycle_anchors(&sweeps, 1), vec![timestamp("20260612064402")]);
    }

    #[test]
    fn product_inclusion_follows_the_dual_pol_flag() {
        let base: Vec<&str> = DwdProvider::new()
            .included_products()
            .map(|product| product.dir)
            .collect();
        assert_eq!(base, ["sweep_vol_z", "sweep_vol_v"]);
        let full: Vec<&str> = DwdProvider::with_dual_pol()
            .included_products()
            .map(|product| product.dir)
            .collect();
        assert_eq!(
            full,
            [
                "sweep_vol_z",
                "sweep_vol_v",
                "sweep_vol_zdr",
                "sweep_vol_rhohv",
                "sweep_vol_phidp"
            ]
        );
    }
}
