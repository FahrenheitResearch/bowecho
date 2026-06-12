//! SHMU (Slovenský hydrometeorologický ústav, Slovakia) radar volume feed.
//!
//! Catalog: `https://opendata.shmu.sk/meteorology/weather/radar/volume/`,
//! an Apache autoindex tree (captured live 2026-06-12 UTC):
//!
//! ```text
//! volume/
//!   skjav/                      station (Malý Javorník)
//!     dBZ/ V/ ZDR/ RhoHV/ PhiDP/ KDP/ W/ dBuZ/      products
//!       20260612/               UTC date directories
//!         T_PAGZ41_C_LZIB_20260612060000.hdf        5-minute files
//! ```
//!
//! Every product file is a FULL polar volume (ODIM_H5 `PVOL`, EUMETNET
//! OPERA Data Information Model; Michelson et al., OPERA WP 2.1/2.2,
//! v2.2-2.3) carrying that product's single moment across all 12 cuts —
//! confirmed live: `skjav` dBZ and V each decode to 12 PPI cuts of 360
//! radials. A complete multi-moment volume is therefore assembled by
//! downloading one PVOL per product at a common timestamp and merging them
//! (`radar_core::merge_radar_volumes`), which is exactly what the
//! [`FramePlan`] returned by [`ShmuProvider::latest`] describes: dBZ first
//! (merge base), V second, then ZDR/RhoHV/PhiDP/KDP when present at that
//! timestamp.
//!
//! The WMO bulletin header in the file name varies per station and product
//! (`T_PAGZ41_C_LZIB_*` is skjav dBZ, `T_PAHZ41_*` skjav V, `T_PAGZ51_*`
//! skkoj dBZ, ...), so file names are always taken verbatim from the
//! listing and matched by their 14-digit timestamp only.

use std::collections::BTreeMap;

use super::listing::{digit_run, fnv1a64, join_url, parse_autoindex};
use super::{FramePlan, IntlProvider, IntlSite, PlanPart};
use crate::fetch_text;

const SHMU_VOLUME_ROOT: &str = "https://opendata.shmu.sk/meteorology/weather/radar/volume/";
/// Products required to build a frame, in merge order (dBZ is the base).
const REQUIRED_PRODUCTS: [&str; 2] = ["dBZ", "V"];
/// Products merged in when a file exists at the chosen timestamp.
const OPTIONAL_PRODUCTS: [&str; 4] = ["ZDR", "RhoHV", "PhiDP", "KDP"];

/// Station labels and coordinates, verified 2026-06-12 against the `/where`
/// group of live SHMU ODIM volumes (lat/lon as decoded, 4 decimals).
const SHMU_STATIONS: [(&str, &str, f32, f32); 4] = [
    ("skjav", "Malý Javorník", 48.2556, 17.1524),
    ("skkoj", "Kojšovská hoľa", 48.7827, 20.9873),
    ("skkub", "Kubínska hoľa", 49.2717, 19.2493),
    ("sklaz", "Španí laz", 48.2404, 19.2573),
];

/// Slovakia's SHMU open-data radar volume feed (per-product full PVOLs).
#[derive(Clone, Copy, Debug, Default)]
pub struct ShmuProvider;

impl ShmuProvider {
    pub fn new() -> Self {
        Self
    }
}

impl IntlProvider for ShmuProvider {
    fn id(&self) -> &'static str {
        "shmu"
    }

    fn label(&self) -> &'static str {
        "SHMU Slovakia"
    }

    fn country(&self) -> &'static str {
        "Slovakia"
    }

    fn list_sites(&self) -> Result<Vec<IntlSite>, String> {
        let html = fetch_text(SHMU_VOLUME_ROOT)
            .map_err(|err| format!("SHMU station listing {SHMU_VOLUME_ROOT}: {err}"))?;
        let mut sites: Vec<IntlSite> = parse_autoindex(&html)
            .into_iter()
            .filter(|entry| entry.is_dir && entry.name != "metadata")
            .map(|entry| {
                let known = SHMU_STATIONS.iter().find(|(id, _, _, _)| *id == entry.name);
                IntlSite {
                    provider_id: self.id(),
                    label: known.map_or_else(
                        || entry.name.to_ascii_uppercase(),
                        |(_, label, _, _)| (*label).to_owned(),
                    ),
                    latitude_deg: known.map(|(_, _, lat, _)| *lat),
                    longitude_deg: known.map(|(_, _, _, lon)| *lon),
                    site_id: entry.name,
                    country: self.country(),
                }
            })
            .collect();
        if sites.is_empty() {
            return Err(format!(
                "SHMU station listing {SHMU_VOLUME_ROOT} held no station directories"
            ));
        }
        sites.sort_by(|left, right| left.site_id.cmp(&right.site_id));
        Ok(sites)
    }

    fn latest(&self, site_id: &str) -> Result<FramePlan, String> {
        if !is_safe_path_segment(site_id) {
            return Err(format!("SHMU: invalid site id '{site_id}'"));
        }
        let site_url = format!("{SHMU_VOLUME_ROOT}{site_id}/");
        let site_html = fetch_text(&site_url)
            .map_err(|err| format!("SHMU product listing {site_url}: {err}"))?;
        let products: Vec<String> = parse_autoindex(&site_html)
            .into_iter()
            .filter(|entry| entry.is_dir)
            .map(|entry| entry.name)
            .collect();
        for required in REQUIRED_PRODUCTS {
            if !products.iter().any(|product| product == required) {
                return Err(format!(
                    "SHMU site '{site_id}' is missing required product directory '{required}'"
                ));
            }
        }

        // Newest timestamp present in BOTH dBZ and V. The lookup is widened
        // to the previous date directory when the newest date's
        // intersection is empty (e.g. right after the UTC date rollover one
        // product has already opened the new directory and the other has
        // not).
        let mut dbz = product_files_for_newest_date(site_id, "dBZ", 0)?;
        let mut vel = product_files_for_newest_date(site_id, "V", 0)?;
        let mut stamp = newest_common_stamp(&dbz, &vel);
        if stamp.is_none() {
            if let Ok(previous_dbz) = product_files_for_newest_date(site_id, "dBZ", 1) {
                dbz.extend(previous_dbz);
            }
            if let Ok(previous_vel) = product_files_for_newest_date(site_id, "V", 1) {
                vel.extend(previous_vel);
            }
            stamp = newest_common_stamp(&dbz, &vel);
        }
        let Some(stamp) = stamp else {
            return Err(format!(
                "SHMU site '{site_id}': no timestamp common to dBZ and V \
                 ({} dBZ files, {} V files inspected)",
                dbz.len(),
                vel.len()
            ));
        };
        let date = &stamp[..8];

        let mut parts = vec![
            PlanPart {
                url: product_file_url(site_id, "dBZ", date, &dbz[&stamp]),
            },
            PlanPart {
                url: product_file_url(site_id, "V", date, &vel[&stamp]),
            },
        ];
        for product in OPTIONAL_PRODUCTS {
            if !products.iter().any(|name| name == product) {
                continue;
            }
            // Optional products lag the required pair by a file or two at
            // times (live capture: KDP one stamp behind dBZ/V); they are
            // merged in only when the exact timestamp exists.
            let Ok(files) = product_files_for_date(site_id, product, date) else {
                continue;
            };
            if let Some(name) = files.get(&stamp) {
                parts.push(PlanPart {
                    url: product_file_url(site_id, product, date, name),
                });
            }
        }

        Ok(FramePlan {
            identity: plan_identity(site_id, &stamp, &parts),
            parts,
            merge: true,
        })
    }
}

/// `{site}_{stamp}_p{N}_h{url-hash}`: stable for one upstream frame, and a
/// late-arriving optional product at the same timestamp changes the part
/// count/hash so the poller picks the richer frame up.
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

fn product_file_url(site_id: &str, product: &str, date: &str, name: &str) -> String {
    join_url(
        &format!("{SHMU_VOLUME_ROOT}{site_id}/{product}/{date}"),
        name,
    )
}

/// Map `timestamp -> file name` for one product on its newest (or
/// `dates_back`-th newest) date directory.
fn product_files_for_newest_date(
    site_id: &str,
    product: &str,
    dates_back: usize,
) -> Result<BTreeMap<String, String>, String> {
    let dates_url = format!("{SHMU_VOLUME_ROOT}{site_id}/{product}/");
    let html =
        fetch_text(&dates_url).map_err(|err| format!("SHMU date listing {dates_url}: {err}"))?;
    let mut dates: Vec<String> = parse_autoindex(&html)
        .into_iter()
        .filter(|entry| {
            entry.is_dir && entry.name.len() == 8 && digit_run(&entry.name, 8).is_some()
        })
        .map(|entry| entry.name)
        .collect();
    dates.sort();
    let Some(date) = dates.iter().rev().nth(dates_back) else {
        return Err(format!(
            "SHMU date listing {dates_url}: no date directory {dates_back} back \
             ({} available)",
            dates.len()
        ));
    };
    product_files_for_date(site_id, product, date)
}

/// Map `timestamp -> file name` for one product/date directory.
fn product_files_for_date(
    site_id: &str,
    product: &str,
    date: &str,
) -> Result<BTreeMap<String, String>, String> {
    let files_url = format!("{SHMU_VOLUME_ROOT}{site_id}/{product}/{date}/");
    let html =
        fetch_text(&files_url).map_err(|err| format!("SHMU file listing {files_url}: {err}"))?;
    Ok(stamp_map(&html))
}

/// Parse a SHMU file listing into `14-digit timestamp -> file name`.
fn stamp_map(listing_html: &str) -> BTreeMap<String, String> {
    parse_autoindex(listing_html)
        .into_iter()
        .filter(|entry| !entry.is_dir)
        .filter_map(|entry| {
            let stamp = digit_run(&entry.name, 14)?.to_owned();
            Some((stamp, entry.name))
        })
        .collect()
}

/// Newest timestamp present in both maps.
fn newest_common_stamp(
    dbz: &BTreeMap<String, String>,
    vel: &BTreeMap<String, String>,
) -> Option<String> {
    dbz.keys()
        .rev()
        .find(|stamp| vel.contains_key(*stamp))
        .cloned()
}

/// Site ids come back out of our own listings, but `latest` is a public
/// trait method: refuse anything that could escape the URL path.
fn is_safe_path_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRODUCTS_PAGE: &str = include_str!("../../tests/fixtures/shmu_skjav_products.html");
    const DATES_PAGE: &str = include_str!("../../tests/fixtures/shmu_skjav_dbz_dates.html");
    const DBZ_FILES: &str = include_str!("../../tests/fixtures/shmu_skjav_dbz_files.html");
    const V_FILES: &str = include_str!("../../tests/fixtures/shmu_skjav_v_files.html");

    #[test]
    fn product_page_lists_required_and_optional_products() {
        let products: Vec<String> = parse_autoindex(PRODUCTS_PAGE)
            .into_iter()
            .filter(|entry| entry.is_dir)
            .map(|entry| entry.name)
            .collect();
        for required in REQUIRED_PRODUCTS {
            assert!(products.iter().any(|name| name == required), "{required}");
        }
        for optional in OPTIONAL_PRODUCTS {
            assert!(products.iter().any(|name| name == optional), "{optional}");
        }
    }

    #[test]
    fn newest_date_directory_wins() {
        let mut dates: Vec<String> = parse_autoindex(DATES_PAGE)
            .into_iter()
            .filter(|entry| entry.is_dir && digit_run(&entry.name, 8).is_some())
            .map(|entry| entry.name)
            .collect();
        dates.sort();
        assert_eq!(dates.last().map(String::as_str), Some("20260612"));
        assert!(dates.len() > 20, "retention window should be weeks");
    }

    #[test]
    fn stamp_maps_intersect_on_the_newest_common_timestamp() {
        let dbz = stamp_map(DBZ_FILES);
        let vel = stamp_map(V_FILES);
        assert_eq!(dbz.len(), 8);
        assert_eq!(vel.len(), 8);
        assert_eq!(
            newest_common_stamp(&dbz, &vel).as_deref(),
            Some("20260612065000")
        );
        assert_eq!(dbz["20260612065000"], "T_PAGZ41_C_LZIB_20260612065000.hdf");
        assert_eq!(vel["20260612065000"], "T_PAHZ41_C_LZIB_20260612065000.hdf");
    }

    #[test]
    fn common_stamp_ignores_products_that_lag() {
        let mut dbz = BTreeMap::new();
        dbz.insert("20260612064500".to_owned(), "a.hdf".to_owned());
        dbz.insert("20260612065000".to_owned(), "b.hdf".to_owned());
        let mut vel = BTreeMap::new();
        vel.insert("20260612064500".to_owned(), "c.hdf".to_owned());
        assert_eq!(
            newest_common_stamp(&dbz, &vel).as_deref(),
            Some("20260612064500")
        );
        assert_eq!(newest_common_stamp(&dbz, &BTreeMap::new()), None);
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
        let identity = plan_identity("skjav", "20260612065000", &parts);
        assert_eq!(identity, plan_identity("skjav", "20260612065000", &parts));
        assert!(identity.starts_with("skjav_20260612065000_p2_h"));
        let fewer = plan_identity("skjav", "20260612065000", &parts[..1]);
        assert_ne!(identity, fewer);
    }

    #[test]
    fn site_id_path_segments_are_validated() {
        assert!(is_safe_path_segment("skjav"));
        assert!(!is_safe_path_segment(""));
        assert!(!is_safe_path_segment("../etc"));
        assert!(!is_safe_path_segment("a/b"));
    }
}
