//! First-run site selection (miniderecho-spec §3.2, §11 M1): the pure
//! fallback chain `saved site → IP geolocation → KTLX`, with the one
//! geolocation fetch behind a trait so tests are offline and the iOS
//! wrapper can swap in CoreLocation later. Mitigations per spec §12.8:
//! single https fetch on a WorkerSlot, 2 s timeout enforced at the boot
//! state machine, silent fallback, MiniSettings kill-switch
//! (`ip_geolocation`), endpoint named here for the M4 About privacy text.

use std::time::Duration;

use data_source::sites::{self, SiteKind, SiteRef};
use ui_core::worker_slot::SlotPoll;

/// The ipapi-class endpoint (spec §2 "iOS boundary" names the class; this
/// exact URL goes in About's privacy text at M4). One anonymous https GET,
/// city-level accuracy — plenty to pick the nearest radar.
pub const IP_GEOLOCATION_ENDPOINT: &str = "https://ipapi.co/json/";

/// How long boot waits for geolocation before silently falling through
/// (spec §3.2). The fetch itself keeps running on its slot; a late result
/// is simply never read (drop-rx cancels it).
pub const GEOLOCATION_TIMEOUT: Duration = Duration::from_secs(2);

/// Radius within which a "nearest" radar is credible. Beyond it (mid-
/// ocean, another continent) the chain falls through to KTLX rather than
/// pinning a user to a radar a hemisphere away.
const NEAREST_SITE_RADIUS_KM: f32 = 500.0;

/// The end-of-chain default.
pub const FALLBACK_SITE: &str = "KTLX";

/// One blocking location fix, bounded by the caller's timeout policy.
/// `None` = no fix (network failure, parse failure, kill-switch upstream).
pub trait GeoLocator: Send + 'static {
    fn locate(&self) -> Option<(f32, f32)>;
}

/// The real v1 impl: one https fetch through data_source's shared client.
pub struct IpApiGeolocator;

impl GeoLocator for IpApiGeolocator {
    fn locate(&self) -> Option<(f32, f32)> {
        let body = data_source::fetch_text(IP_GEOLOCATION_ENDPOINT).ok()?;
        parse_ip_geolocation(&body)
    }
}

/// Parse an ipapi-style JSON body to (lat, lon). Total: anything malformed
/// or out of range is `None` (the chain falls through silently).
pub fn parse_ip_geolocation(json: &str) -> Option<(f32, f32)> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let lat = value.get("latitude")?.as_f64()? as f32;
    let lon = value.get("longitude")?.as_f64()? as f32;
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some((lat, lon))
}

/// How the startup site was chosen — drives the honest status text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SiteChoiceSource {
    Saved,
    Nearest,
    Fallback,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SiteChoice {
    pub site: SiteRef,
    pub source: SiteChoiceSource,
}

/// v1 live scope (spec §3): the US Level-II realtime chain only.
pub fn site_is_us_live(kind: &SiteKind) -> bool {
    matches!(kind, SiteKind::Wsr88d | SiteKind::Tdwr | SiteKind::Research)
}

/// THE first-run chain (pure, spec §3.2): last-used site → geolocated
/// nearest US radar → KTLX. A saved key that no longer resolves (or
/// resolves outside the US live scope) falls through rather than dead-
/// ending the app on an unpollable site.
pub fn choose_startup_site(saved: Option<&str>, located: Option<(f32, f32)>) -> SiteChoice {
    if let Some(key) = saved {
        let site = SiteRef::parse_settings_key(key);
        if let Some(record) = sites::resolve(&site)
            && site_is_us_live(&record.kind)
        {
            return SiteChoice {
                site: record.site,
                source: SiteChoiceSource::Saved,
            };
        }
    }
    // First-run lands on the nearest OPERATIONAL WSR-88D: TDWRs are
    // short-range terminal radars (every one sits inside 88D coverage)
    // and research feeds go dark for weeks — the picker still lists both.
    if let Some((lat, lon)) = located
        && let Some((record, _)) = sites::sites_near(lat, lon, NEAREST_SITE_RADIUS_KM)
            .into_iter()
            .find(|(record, _)| record.kind == SiteKind::Wsr88d)
    {
        return SiteChoice {
            site: record.site,
            source: SiteChoiceSource::Nearest,
        };
    }
    SiteChoice {
        site: SiteRef::parse_settings_key(FALLBACK_SITE),
        source: SiteChoiceSource::Fallback,
    }
}

/// One boot-machine step over the geolocation slot (pure, truth-table
/// tested): `None` = keep waiting; `Some(fix)` = run the chain now with
/// this (possibly absent) fix. Pending past the timeout, a panicked
/// worker, and a never-spawned slot all resolve to "no fix".
pub fn locate_step(
    poll: SlotPoll<Option<(f32, f32)>>,
    timed_out: bool,
) -> Option<Option<(f32, f32)>> {
    match poll {
        SlotPoll::Ready(fix) => Some(fix),
        SlotPoll::Disconnected | SlotPoll::Idle => Some(None),
        SlotPoll::Pending if timed_out => Some(None),
        SlotPoll::Pending => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn us(id: &str) -> SiteRef {
        SiteRef::Us {
            level2_id: id.to_owned(),
        }
    }

    #[test]
    fn saved_site_wins_the_chain() {
        // Even with a perfectly good fix near another radar.
        let choice = choose_startup_site(Some("KEAX"), Some((35.3, -97.3)));
        assert_eq!(choice.site, us("KEAX"));
        assert_eq!(choice.source, SiteChoiceSource::Saved);
    }

    #[test]
    fn stale_or_non_us_saved_keys_fall_through() {
        // Unresolvable key → geolocation picks the nearest US radar
        // (Norman, OK sits on KTLX's doorstep).
        let choice = choose_startup_site(Some("ZZZZ"), Some((35.3, -97.3)));
        assert_eq!(choice.site, us("KTLX"));
        assert_eq!(choice.source, SiteChoiceSource::Nearest);

        // An international key is outside the v1 US live scope.
        let choice = choose_startup_site(Some("intl:smhi:angelholm"), None);
        assert_eq!(choice.source, SiteChoiceSource::Fallback);
    }

    #[test]
    fn geolocation_picks_the_nearest_operational_wsr88d() {
        // Kansas City → KEAX Pleasant Hill; the TMCI terminal radar may
        // rank closer but first-run never lands on a TDWR/research feed.
        let choice = choose_startup_site(None, Some((39.1, -94.6)));
        assert_eq!(choice.site, us("KEAX"));
        assert_eq!(choice.source, SiteChoiceSource::Nearest);

        // Probe ON the Norman Testbed research pad: the pad itself (0 km,
        // Research) and TOKC (~6 km, TDWR) are both skipped; the nearest
        // catalog WSR-88D is the ROC's Norman test radar ROCO2 (~1 km).
        let choice = choose_startup_site(None, Some((35.238, -97.460)));
        assert_eq!(choice.site, us("ROCO2"));
        assert_eq!(choice.source, SiteChoiceSource::Nearest);
    }

    #[test]
    fn far_from_any_us_radar_falls_back_to_ktlx() {
        // Central Europe: sites exist (SMHI et al.) but none in the US
        // live scope within the radius.
        let choice = choose_startup_site(None, Some((50.1, 8.7)));
        assert_eq!(choice.site, us(FALLBACK_SITE));
        assert_eq!(choice.source, SiteChoiceSource::Fallback);
        // Open ocean, and no fix at all.
        assert_eq!(
            choose_startup_site(None, Some((0.0, -160.0))).source,
            SiteChoiceSource::Fallback
        );
        assert_eq!(
            choose_startup_site(None, None).source,
            SiteChoiceSource::Fallback
        );
    }

    #[test]
    fn locate_step_truth_table() {
        let fix = Some((35.0f32, -97.0f32));
        // Result available → chain runs with the fix (or its absence).
        assert_eq!(locate_step(SlotPoll::Ready(fix), false), Some(fix));
        assert_eq!(locate_step(SlotPoll::Ready(None), true), Some(None));
        // Worker panic / never spawned → no fix, chain runs.
        assert_eq!(locate_step(SlotPoll::Disconnected, false), Some(None));
        assert_eq!(locate_step(SlotPoll::Idle, false), Some(None));
        // In flight: wait until the 2 s budget, then move on without it.
        assert_eq!(locate_step(SlotPoll::Pending, false), None);
        assert_eq!(locate_step(SlotPoll::Pending, true), Some(None));
    }

    #[test]
    fn ip_geolocation_parse_is_total() {
        assert_eq!(
            parse_ip_geolocation(r#"{"ip":"1.2.3.4","latitude":35.3,"longitude":-97.3}"#),
            Some((35.3, -97.3))
        );
        for bad in [
            "",
            "not json",
            "{}",
            r#"{"latitude":"35.3","longitude":"-97.3"}"#, // strings, not numbers
            r#"{"latitude":135.3,"longitude":-97.3}"#,    // out of range
            r#"{"latitude":35.3}"#,                       // missing lon
        ] {
            assert_eq!(parse_ip_geolocation(bad), None, "{bad:?}");
        }
    }
}
