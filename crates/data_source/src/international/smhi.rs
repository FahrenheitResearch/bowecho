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

use serde::Deserialize;

use super::{FramePlan, IntlProvider, IntlSite, PlanPart, SiteCache};

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

    fn recent(&self, site_id: &str, count: usize) -> Result<Vec<FramePlan>, String> {
        validate_area_key(site_id)?;
        let url = format!("{API_BASE}/area/{site_id}/product/qcvol");
        let json = crate::fetch_text(&url)
            .map_err(|err| format!("SMHI qcvol catalog for '{site_id}' ({url}): {err}"))?;
        plans_from_qcvol_catalog(site_id, &json, count.max(1))
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
    let mut entries: Vec<_> = product.last_files.iter().collect();
    entries.sort_by(|left, right| left.key.cmp(&right.key));
    let skip = entries.len().saturating_sub(count);
    entries[skip..]
        .iter()
        .map(|newest| {
            // Prefer the dated URL derived from the key so the downloaded
            // bytes always match the identity; fall back to the API's h5
            // link (the identity-less `latest.h5`) if the key shape ever
            // changes.
            let url = match dated_url_from_key(area, &newest.key) {
                Some(url) => url,
                None => newest
                    .formats
                    .iter()
                    .find(|format| format.key == "h5")
                    .map(|format| format.link.clone())
                    .ok_or_else(|| {
                        format!(
                            "SMHI qcvol entry '{}' for '{area}' has no h5 format link",
                            newest.key
                        )
                    })?,
            };
            Ok(FramePlan {
                identity: newest.key.clone(),
                parts: vec![PlanPart { url }],
                merge: false,
            })
        })
        .collect()
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
}

#[derive(Debug, Deserialize)]
struct FileEntry {
    key: String,
    #[serde(default)]
    formats: Vec<FormatEntry>,
}

#[derive(Debug, Deserialize)]
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
