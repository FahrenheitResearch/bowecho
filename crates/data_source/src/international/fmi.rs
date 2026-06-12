//! FMI (Finland) open radar data provider.
//!
//! Catalog: the public AWS Open Data bucket
//! `fmi-opendata-radar-volume-hdf5` (registry.opendata.aws/fmi-radar),
//! anonymous `ListObjectsV2`. Keys follow
//! `{yyyy}/{mm}/{dd}/{site}/{yyyymmddhhmm}_{site}_PVOL.h5`; each object is
//! a full polar volume in HDF5 with ODIM 2.3 conventions per the registry
//! description (EUMETNET OPERA Data Information Model; Michelson et al.,
//! OPERA WP 2.1/2.2, v2.2-2.3). Files run ~16-24 MB.
//!
//! Site discovery: a delimited listing of today's UTC date prefix yields
//! one `CommonPrefix` per radar; around midnight (or after an outage) the
//! provider falls back to the previous UTC day. Newest frame: the
//! lexicographic maximum `*_PVOL.h5` key under `{date}/{site}/` —
//! zero-padded stamps make that the chronological maximum. Probed live
//! 2026-06-12: 12 sites listed, per-site pages under one 1000-key page.

use chrono::{Datelike, Days, NaiveDate, Utc};

use super::{
    FramePlan, IntlProvider, IntlSite, PlanPart, SiteCache, fetch_s3_style_listing,
    s3_style_listing_url,
};

const BUCKET_BASE: &str = "https://fmi-opendata-radar-volume-hdf5.s3.amazonaws.com";

/// Station names for FMI's radar network site codes. Codes missing here
/// (new radars) fall back to the uppercased code.
const SITE_LABELS: &[(&str, &str)] = &[
    ("fianj", "Anjalankoski"),
    ("fiika", "Ikaalinen"),
    ("fikan", "Kankaanpää"),
    ("fikes", "Kesälahti"),
    ("fikor", "Korppoo"),
    ("fikuo", "Kuopio"),
    ("filuo", "Luosto"),
    ("finur", "Nurmes"),
    ("fipet", "Petäjävesi"),
    ("fiuta", "Utajärvi"),
    ("fivih", "Vihti"),
    ("fivim", "Vimpeli"),
];

/// FMI Finland: single-file ODIM PVOL frames from the volume bucket.
pub struct FmiProvider {
    sites: SiteCache,
}

impl FmiProvider {
    pub fn new() -> Self {
        Self {
            sites: SiteCache::new(),
        }
    }
}

impl Default for FmiProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl IntlProvider for FmiProvider {
    fn id(&self) -> &'static str {
        "fmi"
    }

    fn label(&self) -> &'static str {
        "FMI Finland"
    }

    fn country(&self) -> &'static str {
        "Finland"
    }

    fn list_sites(&self) -> Result<Vec<IntlSite>, String> {
        self.sites.get_or_fill(|| {
            for date in candidate_utc_dates() {
                let url =
                    s3_style_listing_url(BUCKET_BASE, &date_prefix(date), Some("/"), None, 1000);
                let listing = fetch_s3_style_listing(&url).map_err(|err| format!("FMI {err}"))?;
                let sites = sites_from_prefixes(&listing.common_prefixes);
                if !sites.is_empty() {
                    return Ok(sites);
                }
            }
            Err("FMI bucket listed no radar sites for today or yesterday (UTC)".to_owned())
        })
    }

    fn latest(&self, site_id: &str) -> Result<FramePlan, String> {
        validate_site_code(site_id)?;
        for date in candidate_utc_dates() {
            let prefix = format!("{}{site_id}/", date_prefix(date));
            let url = s3_style_listing_url(BUCKET_BASE, &prefix, None, None, 1000);
            let listing = fetch_s3_style_listing(&url)
                .map_err(|err| format!("FMI site '{site_id}' {err}"))?;
            if let Some(key) = newest_pvol_key(&listing.keys, site_id) {
                let file_name = key.rsplit('/').next().unwrap_or(key).to_owned();
                return Ok(FramePlan {
                    identity: file_name,
                    parts: vec![PlanPart {
                        url: format!("{BUCKET_BASE}/{key}"),
                    }],
                    merge: false,
                });
            }
        }
        Err(format!(
            "FMI: no PVOL files for site '{site_id}' today or yesterday (UTC)"
        ))
    }
}

/// Today and (for the midnight/outage window) the previous UTC day.
fn candidate_utc_dates() -> [NaiveDate; 2] {
    let today = Utc::now().date_naive();
    let yesterday = today.checked_sub_days(Days::new(1)).unwrap_or(today);
    [today, yesterday]
}

fn date_prefix(date: NaiveDate) -> String {
    format!("{:04}/{:02}/{:02}/", date.year(), date.month(), date.day())
}

/// Site codes are key-path segments (e.g. `fianj`).
fn validate_site_code(site_id: &str) -> Result<(), String> {
    if !site_id.is_empty()
        && site_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        Ok(())
    } else {
        Err(format!("FMI: invalid site code '{site_id}'"))
    }
}

/// `2026/06/12/fianj/` -> site `fianj`, as an [`IntlSite`] list.
fn sites_from_prefixes(common_prefixes: &[String]) -> Vec<IntlSite> {
    let mut sites = common_prefixes
        .iter()
        .filter_map(|prefix| {
            let code = prefix.trim_end_matches('/').rsplit('/').next()?;
            if code.is_empty() {
                return None;
            }
            Some(IntlSite {
                provider_id: "fmi",
                site_id: code.to_owned(),
                label: site_label(code),
                country: "Finland",
                latitude_deg: None,
                longitude_deg: None,
            })
        })
        .collect::<Vec<_>>();
    sites.sort_by(|left, right| left.site_id.cmp(&right.site_id));
    sites.dedup_by(|left, right| left.site_id == right.site_id);
    sites
}

fn site_label(code: &str) -> String {
    if let Some((_, label)) = SITE_LABELS.iter().find(|(known, _)| *known == code) {
        return (*label).to_owned();
    }
    code.to_ascii_uppercase()
}

/// The newest `{stamp}_{site}_PVOL.h5` key for `site`. Zero-padded stamps
/// sort chronologically; other products under the same prefix are ignored.
fn newest_pvol_key<'k>(keys: &'k [String], site: &str) -> Option<&'k str> {
    let suffix = format!("_{site}_PVOL.h5");
    keys.iter()
        .filter(|key| {
            key.rsplit('/')
                .next()
                .is_some_and(|file_name| file_name.ends_with(&suffix))
        })
        .max_by(|left, right| left.cmp(right))
        .map(String::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::international::parse_s3_style_listing;

    /// Recorded live from the bucket's delimited date listing on
    /// 2026-06-12.
    const SITE_PREFIXES_FIXTURE: &str = include_str!("fixtures/fmi_site_prefixes.xml");
    /// Recorded live from the `2026/06/12/fianj/` listing on 2026-06-12,
    /// trimmed to the first two and last two Contents entries.
    const FIANJ_LISTING_FIXTURE: &str = include_str!("fixtures/fmi_fianj_listing.xml");

    #[test]
    fn delimited_date_listing_yields_the_site_catalog() {
        let listing = parse_s3_style_listing(SITE_PREFIXES_FIXTURE).expect("fixture parses");
        let sites = sites_from_prefixes(&listing.common_prefixes);
        assert_eq!(sites.len(), 12);
        assert!(sites.iter().all(|site| site.provider_id == "fmi"));

        let anjalankoski = sites
            .iter()
            .find(|site| site.site_id == "fianj")
            .expect("fianj present");
        assert_eq!(anjalankoski.label, "Anjalankoski");

        // Codes outside the label table keep a code-derived label.
        let kau = sites
            .iter()
            .find(|site| site.site_id == "fikau")
            .expect("fikau present");
        assert_eq!(kau.label, "FIKAU");
    }

    #[test]
    fn per_site_listing_yields_the_newest_pvol_key() {
        let listing = parse_s3_style_listing(FIANJ_LISTING_FIXTURE).expect("fixture parses");
        assert_eq!(
            newest_pvol_key(&listing.keys, "fianj"),
            Some("2026/06/12/fianj/202606120635_fianj_PVOL.h5")
        );
        // A different site's suffix never matches another site's keys.
        assert_eq!(newest_pvol_key(&listing.keys, "fikor"), None);
        assert_eq!(newest_pvol_key(&[], "fianj"), None);
    }

    #[test]
    fn date_prefixes_are_zero_padded() {
        let date = NaiveDate::from_ymd_opt(2026, 6, 1).expect("valid date");
        assert_eq!(date_prefix(date), "2026/06/01/");
    }

    #[test]
    fn site_codes_are_validated_before_key_interpolation() {
        assert!(validate_site_code("fianj").is_ok());
        assert!(validate_site_code("").is_err());
        assert!(validate_site_code("fi/anj").is_err());
        assert!(validate_site_code("FIANJ").is_err());
    }
}
