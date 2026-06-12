//! GeoSphere Austria (Hochficht) open radar data provider.
//!
//! Catalog: GeoSphere's public datahub, an anonymous S3-compatible store at
//! `https://public.hub.geosphere.at/datahub`. The
//! `radar_volumen_hochficht-v1-5min` dataset publishes one full ODIM_H5
//! PVOL (EUMETNET OPERA Data Information Model; Michelson et al., OPERA
//! WP 2.1/2.2, v2.2-2.3) every five minutes as
//! `.../filelisting/WXRHOF_{yyyymmddhhmm}.hdf`.
//!
//! Newest-frame discovery: the bucket lists keys in ascending lexicographic
//! (= chronological) order and holds far more than one page of history, so
//! a plain `ListObjectsV2` returns the *oldest* page. The provider instead
//! starts the listing just behind "now" with `start-after` (12 h, then a
//! 72 h fallback for feed outages) and follows continuation pages until the
//! final, newest key. Probed live 2026-06-12: anonymous listing and
//! download both work (`\x89HDF` magic confirmed).

use chrono::{Duration, Utc};

use super::{
    FramePlan, IntlProvider, IntlSite, PlanPart, fetch_s3_style_listing, s3_style_listing_url,
};

const DATAHUB_BASE: &str = "https://public.hub.geosphere.at/datahub";
const FILE_PREFIX: &str = "resources/radar_volumen_hochficht-v1-5min/filelisting/";
const SITE_ID: &str = "hochficht";

/// Listing lookback windows: a fresh feed answers within 12 h; the 72 h
/// fallback still finds the newest frame across a multi-day outage without
/// paging through the dataset's full history.
const LOOKBACK_HOURS: [i64; 2] = [12, 72];

/// Continuation-page cap per lookback window. At the 5-minute cadence even
/// the 72 h window is under one 1000-key page; the cap only bounds work if
/// the feed ever bursts.
const MAX_LISTING_PAGES: usize = 12;

/// GeoSphere Austria: the Hochficht research radar, single-file ODIM PVOL.
pub struct GeoSphereProvider;

impl GeoSphereProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for GeoSphereProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl IntlProvider for GeoSphereProvider {
    fn id(&self) -> &'static str {
        "geosphere"
    }

    fn label(&self) -> &'static str {
        "GeoSphere Austria"
    }

    fn country(&self) -> &'static str {
        "Austria"
    }

    fn list_sites(&self) -> Result<Vec<IntlSite>, String> {
        Ok(vec![IntlSite {
            provider_id: self.id(),
            site_id: SITE_ID.to_owned(),
            label: "Hochficht".to_owned(),
            country: self.country(),
            latitude_deg: None,
            longitude_deg: None,
        }])
    }

    fn latest(&self, site_id: &str) -> Result<FramePlan, String> {
        if site_id != SITE_ID {
            return Err(format!(
                "GeoSphere: unknown site '{site_id}' (only '{SITE_ID}')"
            ));
        }

        let now = Utc::now();
        for hours in LOOKBACK_HOURS {
            let start_stamp = (now - Duration::hours(hours)).format("%Y%m%d%H%M");
            let mut start_after = format!("{FILE_PREFIX}WXRHOF_{start_stamp}.hdf");
            let mut newest: Option<String> = None;

            for _page in 0..MAX_LISTING_PAGES {
                let url =
                    s3_style_listing_url(DATAHUB_BASE, FILE_PREFIX, None, Some(&start_after), 1000);
                let listing = fetch_s3_style_listing(&url)
                    .map_err(|err| format!("GeoSphere Hochficht {err}"))?;
                if let Some(page_newest) = newest_wxrhof_key(&listing.keys) {
                    newest = Some(page_newest.to_owned());
                }
                let Some(last_key) = listing.keys.last() else {
                    break;
                };
                if !listing.is_truncated {
                    break;
                }
                start_after = last_key.clone();
            }

            if let Some(key) = newest {
                let file_name = key.rsplit('/').next().unwrap_or(&key).to_owned();
                return Ok(FramePlan {
                    identity: file_name,
                    parts: vec![PlanPart {
                        url: format!("{DATAHUB_BASE}/{key}"),
                    }],
                    merge: false,
                });
            }
        }

        Err(format!(
            "GeoSphere Hochficht listing returned no WXRHOF_*.hdf files in \
             the last {} h",
            LOOKBACK_HOURS[LOOKBACK_HOURS.len() - 1]
        ))
    }
}

/// The newest `WXRHOF_*.hdf` key on a page. Keys carry zero-padded UTC
/// stamps, so the lexicographic maximum is the chronological maximum;
/// non-matching keys (sidecar files, anything else under the prefix) are
/// ignored.
fn newest_wxrhof_key(keys: &[String]) -> Option<&str> {
    keys.iter()
        .filter(|key| {
            key.rsplit('/').next().is_some_and(|file_name| {
                file_name.starts_with("WXRHOF_") && file_name.ends_with(".hdf")
            })
        })
        .max_by(|left, right| left.cmp(right))
        .map(String::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::international::parse_s3_style_listing;

    /// Recorded live from the datahub `start-after` probe on 2026-06-12,
    /// trimmed to the first two and last two Contents entries.
    const RECENT_FIXTURE: &str = include_str!("fixtures/geosphere_listing_recent.xml");

    #[test]
    fn recent_window_listing_yields_the_newest_key() {
        let listing = parse_s3_style_listing(RECENT_FIXTURE).expect("fixture parses");
        assert!(!listing.is_truncated);
        assert_eq!(listing.keys.len(), 4);
        assert_eq!(
            newest_wxrhof_key(&listing.keys),
            Some("resources/radar_volumen_hochficht-v1-5min/filelisting/WXRHOF_202606120635.hdf")
        );
    }

    #[test]
    fn newest_key_selection_ignores_non_wxrhof_keys() {
        let keys = vec![
            format!("{FILE_PREFIX}WXRHOF_202606120000.hdf"),
            format!("{FILE_PREFIX}ZZZ_999912312359.txt"),
            format!("{FILE_PREFIX}WXRHOF_202606120630.hdf"),
        ];
        assert_eq!(
            newest_wxrhof_key(&keys),
            Some(format!("{FILE_PREFIX}WXRHOF_202606120630.hdf").as_str())
        );
        assert_eq!(newest_wxrhof_key(&[]), None);
        assert_eq!(
            newest_wxrhof_key(&[format!("{FILE_PREFIX}notes.txt")]),
            None
        );
    }

    #[test]
    fn provider_serves_exactly_one_site_and_rejects_others() {
        let provider = GeoSphereProvider::new();
        let sites = provider.list_sites().expect("static site list");
        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].site_id, SITE_ID);

        let err = provider.latest("vienna").unwrap_err();
        assert!(err.contains("unknown site"), "unexpected error: {err}");
    }
}
