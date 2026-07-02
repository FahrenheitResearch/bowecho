//! SMHI (Sweden) open radar data provider.
//!
//! Catalog: SMHI's open-data radar REST API at
//! `https://opendata-download-radar.smhi.se/api/version/latest`. The root
//! document lists radar areas; each area's `qcvol` product (the
//! quality-controlled polar volume) exposes a `lastFiles` array whose newest
//! entry names the current ODIM_H5 PVOL file (EUMETNET OPERA Data
//! Information Model; Michelson et al., OPERA WP 2.1/2.2, v2.2-2.3).
//!
//! Frame identity: the `lastFiles` key (e.g.
//! `radar_angelholm_qcvol_202606120625`) — the `latest.h5` convenience link
//! alone carries no identity, so the provider downloads the *dated* URL
//! derived from that key (`.../qcvol/{yyyy}/{mm}/{dd}/{key}.h5`), keeping
//! identity and bytes in lockstep. Probed live 2026-06-12: 13 areas, dated
//! URLs serve HDF5 (`\x89HDF`) anonymously.

use chrono::{Datelike, NaiveDate};
use serde::Deserialize;

use super::{ArchiveFrames, FramePlan, IntlProvider, IntlSite, PlanPart, RecentFrames, SiteCache};

const API_BASE: &str = "https://opendata-download-radar.smhi.se/api/version/latest";

/// The national composite area: it only offers `comp` products (no `qcvol`
/// polar volume), so it is not a selectable radar site.
const COMPOSITE_AREA: &str = "sweden";

/// Proper Swedish site names (with diacritics) and radar coordinates for
/// the ASCII-folded area keys the API uses (the SMHI catalog itself
/// carries no coordinates). Unknown keys fall back to a capitalized key
/// without coordinates.
///
/// Coordinates: EUMETNET OPERA radar database, `OPERA_RADARS_DB.json`
/// (fetched 2026-06-12) from
/// <https://eumetnet.eu/activities/observations-programme/current-activities/opera/>,
/// matched by location name; the OPERA ODIM code is in each trailing
/// comment. All twelve Swedish radars are listed operational (status 1).
const SMHI_SITES: &[(&str, &str, f32, f32)] = &[
    ("angelholm", "Ängelholm", 56.3675, 12.8517),   // seang
    ("atvidaberg", "Åtvidaberg", 58.1059, 15.9365), // seatv (Vilebo)
    ("balsta", "Bålsta", 59.6110, 17.5833),         // sebaa
    ("hemse", "Hemse", 57.3035, 18.4001),           // sehem (Ase)
    ("hudiksvall", "Hudiksvall", 61.5771, 16.7144), // sehuv
    ("karlskrona", "Karlskrona", 56.2955, 15.6102), // sekaa
    ("kiruna", "Kiruna", 67.7088, 20.6178),         // sekrn
    ("leksand", "Leksand", 60.7230, 14.8776),       // selek
    ("lulea", "Luleå", 65.4309, 21.8650),           // sella (Rosvik)
    ("ornskoldsvik", "Örnsköldsvik", 63.6395, 18.4019), // seoer
    ("ostersund", "Östersund", 63.2951, 14.7591),   // seosd
    ("vara", "Vara", 58.2556, 12.8260),             // sevax
];

/// SMHI Sweden: single-file ODIM PVOL frames from the `qcvol` product.
pub struct SmhiProvider {
    sites: SiteCache,
}

impl SmhiProvider {
    pub fn new() -> Self {
        Self {
            sites: SiteCache::new(),
        }
    }
}

impl Default for SmhiProvider {
    fn default() -> Self {
        Self::new()
    }
}

pub fn smhi_archive_plans_for_day(area: &str, date: NaiveDate) -> Result<Vec<FramePlan>, String> {
    validate_area_key(area)?;
    let url = format!(
        "{API_BASE}/area/{area}/product/qcvol/{:04}/{:02}/{:02}",
        date.year(),
        date.month(),
        date.day()
    );
    let json = crate::fetch_text(&url)
        .map_err(|err| format!("SMHI qcvol day {date} for '{area}' ({url}): {err}"))?;
    plans_from_qcvol_day_catalog(area, &json)
}

impl IntlProvider for SmhiProvider {
    fn id(&self) -> &'static str {
        "smhi"
    }

    fn label(&self) -> &'static str {
        "SMHI Sweden"
    }

    fn country(&self) -> &'static str {
        "Sweden"
    }

    fn list_sites(&self) -> Result<Vec<IntlSite>, String> {
        self.sites.get_or_fill(|| {
            let json = crate::fetch_text(API_BASE)
                .map_err(|err| format!("SMHI area catalog ({API_BASE}): {err}"))?;
            sites_from_area_catalog(&json)
        })
    }

    fn latest(&self, site_id: &str) -> Result<FramePlan, String> {
        validate_area_key(site_id)?;
        let url = format!("{API_BASE}/area/{site_id}/product/qcvol");
        let json = crate::fetch_text(&url)
            .map_err(|err| format!("SMHI qcvol catalog for '{site_id}' ({url}): {err}"))?;
        plan_from_qcvol_catalog(site_id, &json)
    }

    fn recent_source(&self) -> Option<&dyn RecentFrames> {
        Some(self)
    }

    fn archive_source(&self) -> Option<&dyn ArchiveFrames> {
        Some(self)
    }

    fn static_sites(&self) -> Vec<IntlSite> {
        SMHI_SITES
            .iter()
            .map(|&(key, label, latitude_deg, longitude_deg)| IntlSite {
                provider_id: self.id(),
                site_id: key.to_owned(),
                label: label.to_owned(),
                country: self.country(),
                latitude_deg: Some(latitude_deg),
                longitude_deg: Some(longitude_deg),
            })
            .collect()
    }
}

impl RecentFrames for SmhiProvider {
    fn recent_frames(&self, site_id: &str, count: usize) -> Result<Vec<FramePlan>, String> {
        validate_area_key(site_id)?;
        let url = format!("{API_BASE}/area/{site_id}/product/qcvol");
        let json = crate::fetch_text(&url)
            .map_err(|err| format!("SMHI qcvol catalog for '{site_id}' ({url}): {err}"))?;
        recent_plans_from_qcvol_tree(site_id, &json, count.max(1))
    }
}

impl ArchiveFrames for SmhiProvider {
    /// Verbatim wrap of [`smhi_archive_plans_for_day`]: one dated qcvol
    /// day-catalog probe, plans oldest-first. Window lookups use the
    /// trait's day-folding default — the dated tree is day-granular.
    fn day_plans(&self, site_id: &str, date_utc: NaiveDate) -> Result<Vec<FramePlan>, String> {
        smhi_archive_plans_for_day(site_id, date_utc)
    }
}

/// Area keys are path segments of the URLs we build; reject anything that
/// is not a plain lowercase token so a corrupt saved selection can never
/// rewrite the request path.
fn validate_area_key(site_id: &str) -> Result<(), String> {
    if !site_id.is_empty()
        && site_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        Ok(())
    } else {
        Err(format!("SMHI: invalid area key '{site_id}'"))
    }
}

fn sites_from_area_catalog(json: &str) -> Result<Vec<IntlSite>, String> {
    let catalog: AreaCatalog = serde_json::from_str(json)
        .map_err(|err| format!("SMHI area catalog JSON parse failed: {err}"))?;
    let sites = catalog
        .areas
        .into_iter()
        .filter(|area| area.key != COMPOSITE_AREA)
        .map(|area| {
            let known = SMHI_SITES.iter().find(|(key, ..)| *key == area.key);
            IntlSite {
                provider_id: "smhi",
                site_id: area.key.clone(),
                label: area_label(&area.key),
                country: "Sweden",
                latitude_deg: known.map(|&(_, _, latitude_deg, _)| latitude_deg),
                longitude_deg: known.map(|&(_, _, _, longitude_deg)| longitude_deg),
            }
        })
        .collect::<Vec<_>>();
    if sites.is_empty() {
        return Err("SMHI area catalog listed no radar areas".to_owned());
    }
    Ok(sites)
}

fn area_label(key: &str) -> String {
    if let Some((_, label, _, _)) = SMHI_SITES.iter().find(|(known, ..)| *known == key) {
        return (*label).to_owned();
    }
    let mut chars = key.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => key.to_owned(),
    }
}

fn plan_from_qcvol_catalog(area: &str, json: &str) -> Result<FramePlan, String> {
    plans_from_qcvol_catalog(area, json, 1)?
        .pop()
        .ok_or_else(|| format!("SMHI qcvol catalog for '{area}' has no lastFiles entry"))
}

/// The newest `count` qcvol entries as plans, OLDEST FIRST — the
/// `lastFiles` keys embed `yyyymmddhhmm`, so lexicographic order is
/// chronological. This is what makes Load Loop work on Swedish radars.
fn plans_from_qcvol_catalog(
    area: &str,
    json: &str,
    count: usize,
) -> Result<Vec<FramePlan>, String> {
    let product: QcvolCatalog = serde_json::from_str(json)
        .map_err(|err| format!("SMHI qcvol catalog JSON parse failed for '{area}': {err}"))?;
    if product.last_files.is_empty() {
        return Err(format!(
            "SMHI qcvol catalog for '{area}' has no lastFiles entry"
        ));
    }
    plans_from_file_entries(area, product.last_files, count)
}

/// Walk SMHI's dated year/month/day catalog for recent-loop loads. The root
/// product endpoint can publish the newest `lastFiles` entry before today's
/// day listing has caught up, so seed with `lastFiles` and use the tree only
/// to backfill older frames.
fn recent_plans_from_qcvol_tree(
    area: &str,
    root_json: &str,
    count: usize,
) -> Result<Vec<FramePlan>, String> {
    let product: QcvolCatalog = serde_json::from_str(root_json)
        .map_err(|err| format!("SMHI qcvol catalog JSON parse failed for '{area}': {err}"))?;
    let mut entries = product.last_files;
    if entries.len() < count {
        let mut years = product.years;
        years.sort_by(|left, right| right.key.cmp(&left.key));
        for year in years {
            let year_json = crate::fetch_text(&year.link)
                .map_err(|err| format!("SMHI qcvol year {} for '{area}': {err}", year.key))?;
            let mut months: Vec<LinkEntry> = serde_json::from_str::<YearCatalog>(&year_json)
                .map_err(|err| {
                    format!(
                        "SMHI qcvol year {} JSON parse failed for '{area}': {err}",
                        year.key
                    )
                })?
                .months;
            months.sort_by(|left, right| right.key.cmp(&left.key));
            for month in months {
                let month_json = crate::fetch_text(&month.link).map_err(|err| {
                    format!(
                        "SMHI qcvol month {}-{} for '{area}': {err}",
                        year.key, month.key
                    )
                })?;
                let mut days: Vec<LinkEntry> = serde_json::from_str::<MonthCatalog>(&month_json)
                    .map_err(|err| {
                        format!(
                            "SMHI qcvol month {}-{} JSON parse failed for '{area}': {err}",
                            year.key, month.key
                        )
                    })?
                    .days;
                days.sort_by(|left, right| right.key.cmp(&left.key));
                for day in days {
                    let day_json = crate::fetch_text(&day.link).map_err(|err| {
                        format!(
                            "SMHI qcvol day {}-{}-{} for '{area}': {err}",
                            year.key, month.key, day.key
                        )
                    })?;
                    let day_catalog: DayCatalog =
                        serde_json::from_str(&day_json).map_err(|err| {
                            format!(
                                "SMHI qcvol day {}-{}-{} JSON parse failed for '{area}': {err}",
                                year.key, month.key, day.key
                            )
                        })?;
                    entries.extend(day_catalog.files);
                    if newest_unique_entries(&entries).len() >= count {
                        return plans_from_file_entries(area, entries, count);
                    }
                }
            }
        }
    }
    plans_from_file_entries(area, entries, count)
}

fn plans_from_file_entries(
    area: &str,
    entries: Vec<FileEntry>,
    count: usize,
) -> Result<Vec<FramePlan>, String> {
    let entries = newest_unique_entries(&entries);
    if entries.is_empty() {
        return Err(format!(
            "SMHI qcvol catalog for '{area}' has no file entries"
        ));
    }
    let skip = entries.len().saturating_sub(count);
    entries[skip..]
        .iter()
        .map(|entry| frame_plan_from_file_entry(area, entry))
        .collect()
}

fn plans_from_qcvol_day_catalog(area: &str, json: &str) -> Result<Vec<FramePlan>, String> {
    let day: DayCatalog = serde_json::from_str(json)
        .map_err(|err| format!("SMHI qcvol day JSON parse failed for '{area}': {err}"))?;
    plans_from_file_entries(area, day.files, usize::MAX)
}

fn newest_unique_entries(entries: &[FileEntry]) -> Vec<FileEntry> {
    let mut entries = entries.to_vec();
    entries.sort_by(|left, right| left.key.cmp(&right.key));
    entries.dedup_by(|left, right| left.key == right.key);
    entries
}

fn frame_plan_from_file_entry(area: &str, entry: &FileEntry) -> Result<FramePlan, String> {
    // Prefer the dated URL derived from the key so the downloaded bytes
    // always match the identity; fall back to the API's h5 link (the
    // identity-less `latest.h5`) if the key shape ever changes.
    let url = match dated_url_from_key(area, &entry.key) {
        Some(url) => url,
        None => entry
            .formats
            .iter()
            .find(|format| format.key == "h5")
            .map(|format| format.link.clone())
            .ok_or_else(|| {
                format!(
                    "SMHI qcvol entry '{}' for '{area}' has no h5 format link",
                    entry.key
                )
            })?,
    };
    Ok(FramePlan {
        identity: entry.key.clone(),
        parts: vec![PlanPart { url }],
        merge: false,
    })
}

/// `radar_{area}_qcvol_{yyyymmddhhmm}` -> the dated download URL.
fn dated_url_from_key(area: &str, key: &str) -> Option<String> {
    let stamp = key.rsplit('_').next()?;
    if stamp.len() != 12 || !stamp.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let (year, rest) = stamp.split_at(4);
    let (month, rest) = rest.split_at(2);
    let day = &rest[..2];
    Some(format!(
        "{API_BASE}/area/{area}/product/qcvol/{year}/{month}/{day}/{key}.h5"
    ))
}

#[derive(Debug, Deserialize)]
struct AreaCatalog {
    #[serde(default)]
    areas: Vec<AreaEntry>,
}

#[derive(Debug, Deserialize)]
struct AreaEntry {
    key: String,
}

#[derive(Debug, Deserialize)]
struct QcvolCatalog {
    #[serde(rename = "lastFiles", default)]
    last_files: Vec<FileEntry>,
    #[serde(default)]
    years: Vec<LinkEntry>,
}

#[derive(Clone, Debug, Deserialize)]
struct LinkEntry {
    key: String,
    link: String,
}

#[derive(Debug, Deserialize)]
struct YearCatalog {
    #[serde(default)]
    months: Vec<LinkEntry>,
}

#[derive(Debug, Deserialize)]
struct MonthCatalog {
    #[serde(default)]
    days: Vec<LinkEntry>,
}

#[derive(Debug, Deserialize)]
struct DayCatalog {
    #[serde(default)]
    files: Vec<FileEntry>,
}

#[derive(Clone, Debug, Deserialize)]
struct FileEntry {
    key: String,
    #[serde(default)]
    formats: Vec<FormatEntry>,
}

#[derive(Clone, Debug, Deserialize)]
struct FormatEntry {
    key: String,
    link: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recorded live from `GET /api/version/latest` on 2026-06-12.
    const AREAS_FIXTURE: &str = include_str!("fixtures/smhi_areas.json");
    /// Recorded live from `GET .../area/angelholm/product/qcvol` on
    /// 2026-06-12.
    const QCVOL_FIXTURE: &str = include_str!("fixtures/smhi_qcvol_angelholm.json");

    #[test]
    fn area_catalog_yields_radar_sites_without_the_national_composite() {
        let sites = sites_from_area_catalog(AREAS_FIXTURE).expect("areas parse");
        assert_eq!(sites.len(), 12, "13 areas minus the 'sweden' composite");
        assert!(sites.iter().all(|site| site.provider_id == "smhi"));
        assert!(sites.iter().all(|site| site.site_id != COMPOSITE_AREA));

        let angelholm = sites
            .iter()
            .find(|site| site.site_id == "angelholm")
            .expect("angelholm present");
        assert_eq!(angelholm.label, "Ängelholm");
        assert_eq!(angelholm.latitude_deg, Some(56.3675));
        assert_eq!(angelholm.longitude_deg, Some(12.8517));
        // Every live-listed area is in the static table -> all have coords.
        assert!(
            sites
                .iter()
                .all(|site| site.latitude_deg.is_some() && site.longitude_deg.is_some()),
            "live catalog should carry static coordinates for every area"
        );
    }

    #[test]
    fn qcvol_catalog_yields_a_dated_single_file_plan() {
        let plan = plan_from_qcvol_catalog("angelholm", QCVOL_FIXTURE).expect("qcvol parse");
        assert_eq!(plan.identity, "radar_angelholm_qcvol_202606120625");
        assert!(!plan.merge);
        assert_eq!(plan.parts.len(), 1);
        assert_eq!(
            plan.parts[0].url,
            "https://opendata-download-radar.smhi.se/api/version/latest/area/angelholm\
             /product/qcvol/2026/06/12/radar_angelholm_qcvol_202606120625.h5"
        );
    }

    #[test]
    fn qcvol_catalog_without_last_files_is_a_descriptive_error() {
        let err = plan_from_qcvol_catalog("angelholm", r#"{"lastFiles":[]}"#).unwrap_err();
        assert!(err.contains("no lastFiles"), "unexpected error: {err}");

        let err = plan_from_qcvol_catalog("angelholm", "not json").unwrap_err();
        assert!(err.contains("parse failed"), "unexpected error: {err}");
    }

    #[test]
    fn qcvol_day_catalog_entries_become_oldest_first_recent_plans() {
        let day: DayCatalog = serde_json::from_str(
            r#"{
                "files": [
                    {
                        "key": "radar_angelholm_qcvol_202606220000",
                        "formats": [
                            {
                                "key": "h5",
                                "link": "https://example.test/older.h5"
                            }
                        ]
                    },
                    {
                        "key": "radar_angelholm_qcvol_202606220005",
                        "formats": [
                            {
                                "key": "h5",
                                "link": "https://example.test/newer.h5"
                            }
                        ]
                    }
                ]
            }"#,
        )
        .expect("day parse");

        let plans = plans_from_file_entries("angelholm", day.files, 2).expect("plans");
        assert_eq!(
            plans
                .iter()
                .map(|plan| plan.identity.as_str())
                .collect::<Vec<_>>(),
            vec![
                "radar_angelholm_qcvol_202606220000",
                "radar_angelholm_qcvol_202606220005"
            ]
        );
        assert_eq!(
            plans[1].parts[0].url,
            "https://opendata-download-radar.smhi.se/api/version/latest/area/angelholm\
             /product/qcvol/2026/06/22/radar_angelholm_qcvol_202606220005.h5"
        );
    }

    #[test]
    fn qcvol_recent_entries_dedupe_and_keep_newest_count() {
        let entries = vec![
            FileEntry {
                key: "radar_angelholm_qcvol_202606220000".to_owned(),
                formats: vec![],
            },
            FileEntry {
                key: "radar_angelholm_qcvol_202606220005".to_owned(),
                formats: vec![],
            },
            FileEntry {
                key: "radar_angelholm_qcvol_202606220005".to_owned(),
                formats: vec![],
            },
            FileEntry {
                key: "radar_angelholm_qcvol_202606220010".to_owned(),
                formats: vec![],
            },
        ];

        let plans = plans_from_file_entries("angelholm", entries, 2).expect("plans");
        assert_eq!(
            plans
                .iter()
                .map(|plan| plan.identity.as_str())
                .collect::<Vec<_>>(),
            vec![
                "radar_angelholm_qcvol_202606220005",
                "radar_angelholm_qcvol_202606220010"
            ]
        );
    }

    #[test]
    fn dated_url_requires_a_twelve_digit_stamp() {
        assert_eq!(
            dated_url_from_key("vara", "radar_vara_qcvol_202606120625").as_deref(),
            Some(
                "https://opendata-download-radar.smhi.se/api/version/latest/area/vara\
                 /product/qcvol/2026/06/12/radar_vara_qcvol_202606120625.h5"
            )
        );
        assert_eq!(dated_url_from_key("vara", "radar_vara_qcvol_2026"), None);
        assert_eq!(dated_url_from_key("vara", "radar_vara_qcvol_latest"), None);
    }

    #[test]
    fn area_keys_are_validated_before_url_interpolation() {
        assert!(validate_area_key("angelholm").is_ok());
        assert!(validate_area_key("../escape").is_err());
        assert!(validate_area_key("").is_err());
        assert!(validate_area_key("Has/Slash").is_err());
    }
}
