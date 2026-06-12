//! DMI (Denmark) open radar data provider.
//!
//! Catalog: DMI's radardata STAC API at
//! `https://opendataapi.dmi.dk/v1/radardata/collections/volume/items`
//! (a STAC 1.0.0 / OGC API Features FeatureCollection). Per the DMI radar
//! data API documentation (dmi.dk/friedata/dokumentation/radar-data-api)
//! the items endpoint accepts `stationId`, `datetime`, `limit`, `offset`,
//! and `sortorder`; `sortorder=datetime,DESC&limit=1&stationId={id}` yields
//! exactly the newest volume for one station. Probed live 2026-06-12: no
//! `api-key` is required for either the items query or the asset download
//! (the docs describe no authentication for the radardata API).
//!
//! Each feature's `asset.data.href` is a full ODIM_H5 PVOL (EUMETNET OPERA
//! Data Information Model; Michelson et al., OPERA WP 2.1/2.2, v2.2-2.3),
//! e.g. `.../download/dkste_202606120635.vol.h5` — raw unfiltered volume
//! scans per DMI's collection description. Frame identity: the feature id
//! (`{site}_{yyyymmddhhmm}.vol.h5`).

use std::collections::BTreeMap;

use serde::Deserialize;

use super::{FramePlan, IntlProvider, IntlSite, PlanPart, SiteCache};

const ITEMS_URL: &str = "https://opendataapi.dmi.dk/v1/radardata/collections/volume/items";

/// How many newest items to scan when discovering stations. DMI runs five
/// radars at a 5-minute cadence, so the newest 50 items span every active
/// station several times over.
const SITE_DISCOVERY_LIMIT: u32 = 50;

/// Station names from DMI's radar station list
/// (dmi.dk/friedata/dokumentation/radar-data): the active network is
/// Bornholm, Rømø, Sindal, Stevns, and Samsø. Unknown station ids fall back
/// to the feed's file prefix (e.g. `DKSTE`).
const STATION_LABELS: &[(&str, &str)] = &[
    ("06036", "Sindal"),
    ("06133", "Samsø"),
    ("06177", "Stevns"),
    ("06194", "Bornholm"),
    ("60960", "Rømø/Juvre"),
    // Listed inactive by DMI; labeled in case archive items surface it.
    ("06103", "Virring Skanderborg"),
];

/// DMI Denmark: single-file ODIM PVOL frames from the `volume` collection.
pub struct DmiProvider {
    sites: SiteCache,
}

impl DmiProvider {
    pub fn new() -> Self {
        Self {
            sites: SiteCache::new(),
        }
    }
}

impl Default for DmiProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl IntlProvider for DmiProvider {
    fn id(&self) -> &'static str {
        "dmi"
    }

    fn label(&self) -> &'static str {
        "DMI Denmark"
    }

    fn country(&self) -> &'static str {
        "Denmark"
    }

    fn list_sites(&self) -> Result<Vec<IntlSite>, String> {
        self.sites.get_or_fill(|| {
            let url = format!("{ITEMS_URL}?limit={SITE_DISCOVERY_LIMIT}&sortorder=datetime%2CDESC");
            let json = crate::fetch_text(&url)
                .map_err(|err| format!("DMI volume items ({url}): {err}"))?;
            sites_from_items(&json)
        })
    }

    fn latest(&self, site_id: &str) -> Result<FramePlan, String> {
        validate_station_id(site_id)?;
        let url = format!("{ITEMS_URL}?stationId={site_id}&limit=1&sortorder=datetime%2CDESC");
        let json = crate::fetch_text(&url)
            .map_err(|err| format!("DMI volume items for station {site_id} ({url}): {err}"))?;
        plan_from_items(site_id, &json)
    }
}

/// Station ids are query-string values; DMI's are numeric WMO-style ids
/// (e.g. `06177`, `60960`).
fn validate_station_id(site_id: &str) -> Result<(), String> {
    if !site_id.is_empty() && site_id.bytes().all(|byte| byte.is_ascii_digit()) {
        Ok(())
    } else {
        Err(format!("DMI: invalid station id '{site_id}'"))
    }
}

fn sites_from_items(json: &str) -> Result<Vec<IntlSite>, String> {
    let collection: FeatureCollection = serde_json::from_str(json)
        .map_err(|err| format!("DMI volume items JSON parse failed: {err}"))?;
    // BTreeMap keyed by station id: dedupes repeats and sorts the picker.
    let mut stations = BTreeMap::<String, IntlSite>::new();
    for feature in &collection.features {
        let Some(station_id) = feature.properties.station_id.as_deref() else {
            continue;
        };
        if stations.contains_key(station_id) {
            continue;
        }
        stations.insert(
            station_id.to_owned(),
            IntlSite {
                provider_id: "dmi",
                site_id: station_id.to_owned(),
                label: station_label(station_id, &feature.id),
                country: "Denmark",
                latitude_deg: None,
                longitude_deg: None,
            },
        );
    }
    if stations.is_empty() {
        return Err("DMI volume items listed no stations".to_owned());
    }
    Ok(stations.into_values().collect())
}

fn station_label(station_id: &str, feature_id: &str) -> String {
    if let Some((_, label)) = STATION_LABELS.iter().find(|(id, _)| *id == station_id) {
        return (*label).to_owned();
    }
    // Fall back to the feed's file prefix, e.g. "dkste_2026....vol.h5"
    // -> "DKSTE".
    let prefix = feature_id.split('_').next().unwrap_or(feature_id);
    if prefix.is_empty() {
        station_id.to_owned()
    } else {
        prefix.to_ascii_uppercase()
    }
}

fn plan_from_items(station_id: &str, json: &str) -> Result<FramePlan, String> {
    let collection: FeatureCollection = serde_json::from_str(json)
        .map_err(|err| format!("DMI volume items JSON parse failed: {err}"))?;
    let feature = collection
        .features
        .first()
        .ok_or_else(|| format!("DMI returned no volume items for station {station_id}"))?;
    let href = feature
        .asset
        .as_ref()
        .and_then(|asset| asset.data.as_ref())
        .and_then(|data| data.href.clone())
        .ok_or_else(|| {
            format!(
                "DMI volume item '{}' has no data asset download href",
                feature.id
            )
        })?;
    if feature.id.is_empty() {
        return Err(format!(
            "DMI volume item for station {station_id} has an empty id"
        ));
    }
    Ok(FramePlan {
        identity: feature.id.clone(),
        parts: vec![PlanPart { url: href }],
        merge: false,
    })
}

#[derive(Debug, Deserialize)]
struct FeatureCollection {
    #[serde(default)]
    features: Vec<Feature>,
}

#[derive(Debug, Deserialize)]
struct Feature {
    #[serde(default)]
    id: String,
    properties: FeatureProperties,
    // DMI emits singular "asset"; accept the standard STAC "assets" too.
    #[serde(default, alias = "assets")]
    asset: Option<FeatureAsset>,
}

#[derive(Debug, Deserialize)]
struct FeatureProperties {
    #[serde(rename = "stationId")]
    station_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FeatureAsset {
    data: Option<FeatureAssetData>,
}

#[derive(Debug, Deserialize)]
struct FeatureAssetData {
    href: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recorded live from
    /// `GET .../volume/items?limit=3&sortorder=datetime,DESC` on 2026-06-12
    /// (no api-key).
    const ITEMS_FIXTURE: &str = include_str!("fixtures/dmi_items_sorted.json");

    #[test]
    fn items_yield_deduped_stations_sorted_by_id() {
        let sites = sites_from_items(ITEMS_FIXTURE).expect("items parse");
        // The newest three items were dkste/dkrom/dksin -> three stations.
        let ids = sites
            .iter()
            .map(|site| site.site_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["06036", "06177", "60960"]);
        assert!(sites.iter().all(|site| site.provider_id == "dmi"));

        let stevns = sites
            .iter()
            .find(|site| site.site_id == "06177")
            .expect("stevns present");
        assert_eq!(stevns.label, "Stevns");
    }

    #[test]
    fn newest_item_becomes_a_single_file_plan() {
        let plan = plan_from_items("06177", ITEMS_FIXTURE).expect("plan parse");
        assert_eq!(plan.identity, "dkste_202606120635.vol.h5");
        assert!(!plan.merge);
        assert_eq!(plan.parts.len(), 1);
        assert_eq!(
            plan.parts[0].url,
            "https://opendataapi.dmi.dk/v1/radardata/download/dkste_202606120635.vol.h5"
        );
    }

    #[test]
    fn empty_or_malformed_items_are_descriptive_errors() {
        let err =
            plan_from_items("06177", r#"{"type":"FeatureCollection","features":[]}"#).unwrap_err();
        assert!(err.contains("no volume items"), "unexpected error: {err}");

        let err = plan_from_items("06177", "not json").unwrap_err();
        assert!(err.contains("parse failed"), "unexpected error: {err}");

        let missing_asset =
            r#"{"features":[{"id":"dkste_x.vol.h5","properties":{"stationId":"06177"}}]}"#;
        let err = plan_from_items("06177", missing_asset).unwrap_err();
        assert!(err.contains("no data asset"), "unexpected error: {err}");
    }

    #[test]
    fn unknown_stations_fall_back_to_the_file_prefix_label() {
        assert_eq!(station_label("06036", "dksin_x.vol.h5"), "Sindal");
        assert_eq!(station_label("99999", "dkxyz_x.vol.h5"), "DKXYZ");
        assert_eq!(station_label("99999", ""), "99999");
    }

    #[test]
    fn station_ids_are_validated_before_query_interpolation() {
        assert!(validate_station_id("06177").is_ok());
        assert!(validate_station_id("").is_err());
        assert!(validate_station_id("06177&limit=999").is_err());
    }
}
