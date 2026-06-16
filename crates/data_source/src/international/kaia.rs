//! KAIA Estonia open-data radar provider.
//!
//! Estonia publishes radar volume files through the national KAIA open-data
//! repository. The public document query API lists `VOL` HDF5 files for
//! each radar, and the file endpoint returns the ODIM_H5 payload directly
//! (HDF5 magic confirmed live 2026-06-15 for Harku). KAIA exposes both
//! Harku and Sürgavere with the richer national product set, so BowEcho uses
//! this provider for Estonia instead of mixing one KAIA marker with one ORD
//! marker.

use std::time::Duration;

use chrono::{DateTime, Days, SecondsFormat, Utc};
use serde::Deserialize;
use serde_json::json;

use super::{FramePlan, IntlProvider, IntlSite, PlanPart, SiteCache};

const QUERY_URL: &str = "https://avaandmed.keskkonnaportaal.ee/api/lists/active/items/query";
const FILE_BASE: &str = "https://avaandmed.keskkonnaportaal.ee/api/lists/active";
const LOOKBACK_DAYS: u64 = 14;
const PAGE_SIZE: usize = 5000;
const MAX_PAGES: usize = 4;

#[derive(Clone, Copy, Debug)]
struct KaiaSite {
    site_id: &'static str,
    label: &'static str,
    radar_filter: &'static str,
    latitude_deg: f32,
    longitude_deg: f32,
}

const KAIA_SITES: &[KaiaSite] = &[
    KaiaSite {
        site_id: "eehar",
        label: "Harku",
        radar_filter: "Harku radar (HAR)",
        latitude_deg: 59.3971,
        longitude_deg: 24.6021,
    },
    KaiaSite {
        site_id: "eesur",
        label: "Sürgavere",
        radar_filter: "Sürgavere radar (SUR)",
        latitude_deg: 58.4823,
        longitude_deg: 25.5187,
    },
];

/// KAIA Estonia: single-file ODIM PVOL frames from the active document API.
pub struct KaiaEstoniaProvider {
    sites: SiteCache,
}

impl KaiaEstoniaProvider {
    pub fn new() -> Self {
        Self {
            sites: SiteCache::new(),
        }
    }
}

impl Default for KaiaEstoniaProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl IntlProvider for KaiaEstoniaProvider {
    fn id(&self) -> &'static str {
        "kaia"
    }

    fn label(&self) -> &'static str {
        "KAIA Estonia"
    }

    fn country(&self) -> &'static str {
        "Estonia"
    }

    fn list_sites(&self) -> Result<Vec<IntlSite>, String> {
        self.sites
            .get_or_fill(|| Ok(static_sites(self.id(), self.country())))
    }

    fn latest(&self, site_id: &str) -> Result<FramePlan, String> {
        self.recent(site_id, 1)?
            .pop()
            .ok_or_else(|| format!("KAIA returned no VOL files for {site_id}"))
    }

    fn recent(&self, site_id: &str, count: usize) -> Result<Vec<FramePlan>, String> {
        let site = kaia_site(site_id)?;
        let since = Utc::now()
            .checked_sub_days(Days::new(LOOKBACK_DAYS))
            .unwrap_or_else(Utc::now);
        let entries = query_recent_entries(site, since)?;
        entries_to_plans(site, entries, count.max(1))
    }

    fn static_sites(&self) -> Vec<IntlSite> {
        static_sites(self.id(), self.country())
    }
}

fn static_sites(provider_id: &'static str, country: &'static str) -> Vec<IntlSite> {
    KAIA_SITES
        .iter()
        .map(|site| IntlSite {
            provider_id,
            site_id: site.site_id.to_owned(),
            label: site.label.to_owned(),
            country,
            latitude_deg: Some(site.latitude_deg),
            longitude_deg: Some(site.longitude_deg),
        })
        .collect()
}

fn kaia_site(site_id: &str) -> Result<&'static KaiaSite, String> {
    KAIA_SITES
        .iter()
        .find(|site| site.site_id == site_id)
        .ok_or_else(|| format!("KAIA: unknown site '{site_id}'"))
}

fn query_recent_entries(
    site: &'static KaiaSite,
    since: DateTime<Utc>,
) -> Result<Vec<KaiaFrameEntry>, String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("BowEcho KAIA Estonia radar client")
        .timeout(Duration::from_secs(25))
        .build()
        .map_err(|err| format!("KAIA HTTP client: {err}"))?;
    let mut entries = Vec::new();
    let mut bookmark: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let response_text = client
            .post(QUERY_URL)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(kaia_query_body(site, since, bookmark.as_deref()).to_string())
            .send()
            .map_err(|err| format!("KAIA query for {}: {err}", site.site_id))?
            .error_for_status()
            .map_err(|err| format!("KAIA query status for {}: {err}", site.site_id))?
            .text()
            .map_err(|err| format!("KAIA query body for {}: {err}", site.site_id))?;
        let page_entries = entries_from_query_json(site, &response_text)?;
        let page: KaiaQueryResponse = serde_json::from_str(&response_text)
            .map_err(|err| format!("KAIA query JSON parse for {}: {err}", site.site_id))?;
        entries.extend(page_entries);
        let Some(next) = page.next_bookmark.filter(|value| value != "*") else {
            break;
        };
        if bookmark.as_deref() == Some(next.as_str()) {
            break;
        }
        bookmark = Some(next);
    }
    Ok(entries)
}

fn kaia_query_body(
    site: &KaiaSite,
    since: DateTime<Utc>,
    bookmark: Option<&str>,
) -> serde_json::Value {
    json!({
        "filter": {
            "and": {
                "children": [
                    { "isEqual": { "field": "Radar", "value": site.radar_filter } },
                    { "isEqual": { "field": "Phenomenon", "value": "VOL" } },
                    {
                        "greaterThanOrEqual": {
                            "field": "Timestamp",
                            "value": since.to_rfc3339_opts(SecondsFormat::Millis, true)
                        }
                    }
                ]
            }
        },
        "pageSize": PAGE_SIZE,
        "fields": ["Timestamp", "Phenomenon", "Radar", "RMTitle", "RMFileSize"],
        "includeFileMetadata": true,
        "bookmark": bookmark
    })
}

fn entries_from_query_json(
    site: &'static KaiaSite,
    json: &str,
) -> Result<Vec<KaiaFrameEntry>, String> {
    let page: KaiaQueryResponse = serde_json::from_str(json)
        .map_err(|err| format!("KAIA query JSON parse for {}: {err}", site.site_id))?;
    page.documents
        .into_iter()
        .filter_map(|document| entry_from_document(site, document).transpose())
        .collect()
}

fn entry_from_document(
    site: &'static KaiaSite,
    document: KaiaDocument,
) -> Result<Option<KaiaFrameEntry>, String> {
    let Some(timestamp_text) = document.metadata.timestamp.as_deref() else {
        return Ok(None);
    };
    let timestamp = DateTime::parse_from_rfc3339(timestamp_text)
        .map_err(|err| {
            format!(
                "KAIA {} Timestamp '{}' parse failed: {err}",
                site.site_id, timestamp_text
            )
        })?
        .with_timezone(&Utc);
    let Some(file) = document
        .file_metadata
        .iter()
        .find(|file| {
            file.name
                .as_deref()
                .is_some_and(|name| name.ends_with(".h5"))
        })
        .or_else(|| document.file_metadata.first())
    else {
        return Ok(None);
    };
    let identity = document
        .metadata
        .title
        .or_else(|| file.name.clone())
        .unwrap_or_else(|| format!("kaia-{}-{}", document.id, file.id));
    Ok(Some(KaiaFrameEntry {
        timestamp,
        identity,
        url: format!("{FILE_BASE}/items/{}/files/{}", document.id, file.id),
    }))
}

fn entries_to_plans(
    site: &'static KaiaSite,
    mut entries: Vec<KaiaFrameEntry>,
    count: usize,
) -> Result<Vec<FramePlan>, String> {
    if entries.is_empty() {
        return Err(format!(
            "KAIA returned no recent VOL files for {} in the last {LOOKBACK_DAYS} days",
            site.site_id
        ));
    }
    entries.sort_by(|left, right| {
        left.timestamp
            .cmp(&right.timestamp)
            .then_with(|| left.identity.cmp(&right.identity))
    });
    entries.dedup_by(|left, right| left.identity == right.identity);
    let skip = entries.len().saturating_sub(count);
    Ok(entries[skip..]
        .iter()
        .map(|entry| FramePlan {
            identity: entry.identity.clone(),
            parts: vec![PlanPart {
                url: entry.url.clone(),
            }],
            merge: false,
        })
        .collect())
}

#[derive(Clone, Debug)]
struct KaiaFrameEntry {
    timestamp: DateTime<Utc>,
    identity: String,
    url: String,
}

#[derive(Debug, Deserialize)]
struct KaiaQueryResponse {
    #[serde(rename = "nextBookmark")]
    next_bookmark: Option<String>,
    #[serde(default)]
    documents: Vec<KaiaDocument>,
}

#[derive(Debug, Deserialize)]
struct KaiaDocument {
    id: u64,
    #[serde(default)]
    metadata: KaiaMetadata,
    #[serde(default, rename = "fileMetadata")]
    file_metadata: Vec<KaiaFileMetadata>,
}

#[derive(Default, Debug, Deserialize)]
struct KaiaMetadata {
    #[serde(rename = "Timestamp")]
    timestamp: Option<String>,
    #[serde(rename = "RMTitle")]
    title: Option<String>,
}

#[derive(Debug, Deserialize)]
struct KaiaFileMetadata {
    id: u64,
    name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUERY_FIXTURE: &str = r#"{
        "nextBookmark": "*",
        "documents": [
            {
                "id": 100,
                "metadata": {
                    "Timestamp": "2026-06-15T03:30:00.0000000+03:00",
                    "RMTitle": "HAR.202606150030.VOL.h5"
                },
                "fileMetadata": [
                    { "id": 1, "name": "HAR.202606150030.VOL.h5" }
                ]
            },
            {
                "id": 104,
                "metadata": {
                    "Timestamp": "2026-06-15T03:35:00.0000000+03:00",
                    "RMTitle": "HAR.202606150035.VOL.h5"
                },
                "fileMetadata": [
                    { "id": 1, "name": "HAR.202606150035.VOL.h5" }
                ]
            },
            {
                "id": 108,
                "metadata": {
                    "Timestamp": "2026-06-15T03:25:00.0000000+03:00",
                    "RMTitle": "HAR.202606150025.VOL.h5"
                },
                "fileMetadata": [
                    { "id": 1, "name": "HAR.202606150025.VOL.h5" }
                ]
            }
        ]
    }"#;

    #[test]
    fn static_catalog_exposes_estonian_radars() {
        let provider = KaiaEstoniaProvider::new();
        let sites = provider.static_sites();
        assert_eq!(sites.len(), 2);
        assert_eq!(sites[0].provider_id, "kaia");
        assert_eq!(sites[0].site_id, "eehar");
        assert_eq!(sites[0].label, "Harku");
        assert_eq!(sites[0].country, "Estonia");
        assert_eq!(sites[0].latitude_deg, Some(59.3971));
        assert_eq!(sites[0].longitude_deg, Some(24.6021));
        assert_eq!(sites[1].provider_id, "kaia");
        assert_eq!(sites[1].site_id, "eesur");
        assert_eq!(sites[1].label, "Sürgavere");
        assert_eq!(sites[1].country, "Estonia");
        assert_eq!(sites[1].latitude_deg, Some(58.4823));
        assert_eq!(sites[1].longitude_deg, Some(25.5187));
    }

    #[test]
    fn query_documents_become_oldest_first_recent_plans() {
        let site = kaia_site("eehar").expect("known site");
        let entries = entries_from_query_json(site, QUERY_FIXTURE).expect("fixture parses");
        let plans = entries_to_plans(site, entries, 2).expect("plans");

        assert_eq!(
            plans
                .iter()
                .map(|plan| plan.identity.as_str())
                .collect::<Vec<_>>(),
            vec!["HAR.202606150030.VOL.h5", "HAR.202606150035.VOL.h5"]
        );
        assert_eq!(
            plans[1].parts[0].url,
            "https://avaandmed.keskkonnaportaal.ee/api/lists/active/items/104/files/1"
        );
        assert!(!plans[1].merge);
    }
}
