//! DWD (Deutscher Wetterdienst, Germany) per-sweep radar volume feed.
//!
//! Catalog: `https://opendata.dwd.de/weather/radar/sites/`, an nginx
//! autoindex tree (captured live 2026-06-12 UTC):
//!
//! ```text
//! sites/
//!   sweep_vol_z/asb/unfiltered/        product / station / variant
//!     ras07-vol5minng01_sweeph5onem_th_00-2026061206455700-asb-10103-hd5
//!     ras07-vol5minng01_sweeph5onem_th_00-LATEST-asb-10103-hd5
//!     ... (one file per sweep index 00..09, ~2-day retention)
//!   sweep_vol_v/asb/hdf5/filter_polarimetric/   (no unfiltered variant)
//!     ras07-vol5minng01_sweeph5onem_vradh_00-2026061206455700-asb-10103-hd5
//! ```
//!
//! Each file is ONE sweep (single ODIM_H5 dataset; EUMETNET OPERA Data
//! Information Model, Michelson et al., OPERA WP 2.1/2.2, v2.2-2.3) of one
//! quantity, so a full volume is 10 sweeps x N products merged with
//! `radar_core::merge_radar_volumes`.
//!
//! LATEST naming, confirmed against the live `sweep_vol_z/asb/unfiltered/`
//! listing: for every unfiltered quantity and sweep index there is exactly
//! one `..._NN-LATEST-...` alias whose content is rewritten to the newest
//! sweep each ~5-minute cycle. The filtered velocity directories
//! (`hdf5/filter_polarimetric/`, `hdf5/filter_simple/`) carry NO LATEST
//! aliases at all — timestamped files only. This provider therefore pins
//! explicit timestamped files for every part instead of downloading the
//! LATEST aliases: it keeps all parts of a [`FramePlan`] immutable and
//! cycle-consistent (a LATEST alias downloaded mid-cycle can already point
//! at the next scan for low sweep indices), while the resolved cycle
//! timestamp from the listing provides the frame identity.
//!
//! Cycle resolution: file timestamps are sweep start times
//! (`YYYYMMDDHHMMSScc`, centisecond suffix). Within one `vol5minng01` cycle
//! the sweeps run in ascending index order over ~3 minutes (live capture:
//! `th_00` 06:40:57 ... `th_09` 06:44:02, repeating every 5 minutes), so
//! the newest timestamp of the HIGHEST sweep index marks the most recent
//! complete cycle, and every sweep's file for that cycle is the newest one
//! within the trailing 5-minute window.

use chrono::NaiveDateTime;

use super::listing::{ListingEntry, fnv1a64, has_dir, join_url, parse_autoindex};
use super::{FramePlan, IntlProvider, IntlSite, PlanPart};
use crate::{fetch_listing_text, fetch_text};

const DWD_SITES_ROOT: &str = "https://opendata.dwd.de/weather/radar/sites/";
/// One 5-minute scan cycle: a sweep belongs to the cycle ending at the
/// anchor when its start time is inside this trailing window.
const CYCLE_WINDOW_MINUTES: i64 = 5;

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
    /// Also assemble ZDR/RhoHV/PhiDP. Off by default: each extra product
    /// costs a ~2 MB listing fetch per poll plus ten sweep downloads per
    /// frame, and reflectivity+velocity already make a working display.
    include_dual_pol: bool,
}

impl DwdProvider {
    pub fn new() -> Self {
        Self {
            include_dual_pol: false,
        }
    }

    /// Assemble ZDR, RhoHV, and PhiDP sweeps too (more bandwidth).
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

        let mut anchor: Option<NaiveDateTime> = None;
        let mut parts: Vec<PlanPart> = Vec::new();
        for product in self.included_products() {
            let resolved = match resolve_product_dir(site_id, product) {
                Ok(resolved) => resolved,
                Err(err) if product.required => return Err(err),
                Err(_) => continue,
            };
            let sweeps = parse_dwd_sweeps(&resolved.entries, resolved.quantity);
            if product.required && sweeps.is_empty() {
                return Err(format!(
                    "DWD {}/{site_id}: no timestamped '{}' sweep files in {}",
                    product.dir, resolved.quantity, resolved.dir_url
                ));
            }

            // The base product (first required, sweep_vol_z) anchors the
            // cycle for every other product.
            if anchor.is_none() {
                anchor = newest_complete_cycle_anchor(&sweeps);
            }
            let Some(anchor) = anchor else {
                return Err(format!(
                    "DWD {}/{site_id}: could not resolve a cycle anchor from {}",
                    product.dir, resolved.dir_url
                ));
            };

            let chosen = sweeps_in_cycle(&sweeps, anchor);
            if product.required && chosen.is_empty() {
                return Err(format!(
                    "DWD {}/{site_id}: no '{}' sweeps inside the cycle ending {anchor} \
                     ({} timestamped files inspected)",
                    product.dir,
                    resolved.quantity,
                    sweeps.len()
                ));
            }
            parts.extend(chosen.into_iter().map(|sweep| PlanPart {
                url: join_url(&resolved.dir_url, &sweep.name),
            }));
        }

        let Some(anchor) = anchor else {
            return Err(format!("DWD: no products resolved for site '{site_id}'"));
        };
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

/// A product directory resolved down to the variant that actually carries
/// data files, plus the quantity chosen from it.
struct ResolvedProductDir {
    dir_url: String,
    entries: Vec<ListingEntry>,
    quantity: &'static str,
}

/// Resolve `sites/{product}/{site}/` to its data directory: `unfiltered/`
/// when present (LATEST-bearing raw quantities), else `hdf5/` descending
/// into `filter_polarimetric/` over `filter_simple/` (filtered quantities,
/// timestamped files only).
fn resolve_product_dir(site_id: &str, product: &DwdProduct) -> Result<ResolvedProductDir, String> {
    let station_url = format!("{DWD_SITES_ROOT}{}/{site_id}/", product.dir);
    let station_html =
        fetch_text(&station_url).map_err(|err| format!("DWD station dir {station_url}: {err}"))?;
    let station_entries = parse_autoindex(&station_html);

    let dir_url = if has_dir(&station_entries, "unfiltered") {
        format!("{station_url}unfiltered/")
    } else if has_dir(&station_entries, "hdf5") {
        let hdf5_url = format!("{station_url}hdf5/");
        let hdf5_html =
            fetch_text(&hdf5_url).map_err(|err| format!("DWD filter dir {hdf5_url}: {err}"))?;
        let hdf5_entries = parse_autoindex(&hdf5_html);
        let filter = ["filter_polarimetric", "filter_simple"]
            .into_iter()
            .find(|name| has_dir(&hdf5_entries, name))
            .ok_or_else(|| format!("DWD filter dir {hdf5_url}: no filter_* subdirectory"))?;
        format!("{hdf5_url}{filter}/")
    } else {
        return Err(format!(
            "DWD station dir {station_url}: neither unfiltered/ nor hdf5/ present"
        ));
    };

    // Sweep listings run ~2 MB (full retention, no server gzip): use the
    // long-timeout listing fetch.
    let html = fetch_listing_text(&dir_url)
        .map_err(|err| format!("DWD sweep listing {dir_url}: {err}"))?;
    let entries = parse_autoindex(&html);
    let quantity = product
        .quantities
        .iter()
        .find(|quantity| {
            let marker = quantity_marker(quantity);
            entries.iter().any(|entry| entry.name.contains(&marker))
        })
        .ok_or_else(|| {
            format!(
                "DWD sweep listing {dir_url}: none of the quantities {:?} present",
                product.quantities
            )
        })?;
    Ok(ResolvedProductDir {
        dir_url,
        entries,
        quantity,
    })
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

/// The newest timestamp of the highest sweep index: sweeps scan in
/// ascending index order, so this marks the end of the most recent
/// COMPLETE cycle (the next cycle's low sweeps may already be uploaded).
fn newest_complete_cycle_anchor(sweeps: &[DwdSweepFile]) -> Option<NaiveDateTime> {
    let last_sweep = sweeps.iter().map(|sweep| sweep.sweep).max()?;
    sweeps
        .iter()
        .filter(|sweep| sweep.sweep == last_sweep)
        .map(|sweep| sweep.time)
        .max()
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
        // sweep_vol_z/asb has BOTH unfiltered/ and hdf5/ -> unfiltered wins.
        let z_entries = parse_autoindex(Z_STATION_DIR);
        assert!(has_dir(&z_entries, "unfiltered"));
        assert!(has_dir(&z_entries, "hdf5"));
        // sweep_vol_v/asb has only hdf5/, which holds the filter variants.
        let v_entries = parse_autoindex(V_STATION_DIR);
        assert!(!has_dir(&v_entries, "unfiltered"));
        assert!(has_dir(&v_entries, "hdf5"));
        let v_hdf5 = parse_autoindex(V_HDF5_DIR);
        assert!(has_dir(&v_hdf5, "filter_polarimetric"));
        assert!(has_dir(&v_hdf5, "filter_simple"));
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
    fn anchor_is_newest_stamp_of_highest_sweep_index() {
        let entries = parse_autoindex(Z_FILES);
        let th = parse_dwd_sweeps(&entries, "th");
        assert_eq!(
            newest_complete_cycle_anchor(&th),
            Some(timestamp("20260612064402"))
        );
        assert_eq!(newest_complete_cycle_anchor(&[]), None);
    }

    #[test]
    fn cycle_window_selects_one_file_per_sweep_of_the_complete_cycle() {
        let entries = parse_autoindex(Z_FILES);
        let th = parse_dwd_sweeps(&entries, "th");
        let anchor = newest_complete_cycle_anchor(&th).expect("anchor");
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
