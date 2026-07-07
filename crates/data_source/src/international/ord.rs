//! EUMETNET ORD (Open Radar Data) multi-country provider.
//!
//! Catalog: the ORD 24-hour rolling cache bucket
//! `https://s3.waw3-1.cloudferro.com/openradar-24h`, the S3-compatible
//! store behind the EUMETNET/RODEO Open Radar Data API (ORD API
//! documentation, EUMETNET RODEO project,
//! <https://eumetnet.github.io/openradardata-documentation/>; the bucket
//! and its anonymous access are documented in the "discovering and
//! accessing data" chapter). Anonymous `ListObjectsV2` and `GET`, no key
//! or registration; data is CC BY 4.0 (EUMETNET OPERA). The HTTPS EDR
//! front end (`https://api.meteogate.eu/eu-eumetnet-weather-radar`) serves
//! the same holdings behind a shared anonymous rate limit, so this
//! provider talks to the bucket directly.
//!
//! Keys follow (probed live 2026-06-12):
//!
//! ```text
//! {yyyy}/{mm}/{dd}/{CC}/{site}/{PVOL|SCAN}/
//!     {site}@{yyyymmdd}T{hhmm}@{elev[_elev...]}@{MOMENT[_MOMENT...]}.h5
//! e.g. 2026/06/12/NL/nlhrw/PVOL/
//!     nlhrw@20260612T1455@0.3_0.8_..._90.0@DBZH_TH_VRADH.h5
//! ```
//!
//! Every object is ODIM_H5 (EUMETNET OPERA Data Information Model;
//! Michelson et al., OPERA WP 2.1/2.2, v2.2-2.3) — `PVOL/` holds full or
//! per-moment polar volumes, `SCAN/` holds single-sweep files. National
//! publishing shapes differ (all observed live 2026-06-12):
//!
//! - bundled PVOL, one file per frame (NL, HR, SI, MT, IE/iesha);
//! - per-moment PVOL splits, 1-2 volumes per stamp (BE, IS, NO, PL, RO,
//!   ES), with NO offsetting its velocity stamp a minute after
//!   reflectivity, and ES (AEMET, live in ORD since 2026-06-23) pairing a
//!   3-elevation long-range `DBZH_TH` volume on :x0 with a 2-elevation
//!   Doppler `DBZH_VRADH` volume ~3 minutes earlier/later (:x6-:x7), both
//!   on a 10-minute cadence (observed live 2026-07-07);
//! - per-sweep SCAN files carrying all moments (FR, CH, EE, LT).
//!
//! One frame is assembled DWD-style from a trailing window: the newest
//! stamp anchors the frame, files inside the trailing
//! [`CYCLE_WINDOW_MINUTES`] window are grouped (per moment set for PVOL,
//! per elevation for SCAN), each group keeps its newest file, and
//! reflectivity-bearing parts sort first per the [`FramePlan`] merge
//! contract.
//!
//! Velocity caveat: per the ORD API overview, "Dealiasing of VRADH is not
//! performed consistently at the national level, and is currently not
//! applied centrally within OPERA" — BowEcho's own region-based dealiaser
//! runs on decoded velocity moments, so aliased national feeds still
//! display correctly here.
//!
//! Countries already covered by BowEcho's national providers (SE/SMHI,
//! DK/DMI, AT/GeoSphere, FI/FMI, SK/SHMU, DE/DWD, CZ/CHMI) are excluded:
//! the national feeds stay preferred. EE and RO remain enabled but defer
//! their picker/marker rows to the richer national providers
//! (KAIA Estonia, ANM Romania) via [`site_superseded_by_native_provider`].
//! The `OPERA/` composite prefix is a pseudo-station (gridded composites,
//! not polar volumes) and is also excluded.

use std::collections::BTreeSet;

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Utc};

use super::listing::fnv1a64;
use super::{
    ArchiveFrames, FramePlan, IntlProvider, IntlSite, PlanPart, RecentFrames, SiteCache,
    fetch_s3_style_listing, s3_style_listing_url,
};

const BUCKET_BASE: &str = "https://s3.waw3-1.cloudferro.com/openradar-24h";
const ARCHIVE_BUCKET_BASE: &str = "https://s3.waw3-1.cloudferro.com/openradar-archive";

/// One scan cycle: a file belongs to the frame anchored at the newest
/// stamp when its own stamp is inside this trailing window (same role as
/// the DWD cycle window; ORD national cycles run 5 minutes or slower).
const CYCLE_WINDOW_MINUTES: i64 = 5;
/// Some ORD countries split product families across different directories.
/// Dublin, for example, publishes SCAN reflectivity near hourly while PVOL
/// carries VRADH every 15 minutes. A mixed REF+VEL frame may use the newest
/// velocity file at or before the reflectivity anchor, bounded so stale
/// Doppler data cannot silently ride along with newer reflectivity.
const MIXED_SOURCE_MAX_AGE_MINUTES: i64 = 20;
/// How much older a complete REF+VEL frame may be and still outrank a
/// newer reflectivity-only frame. Inside the window the completeness
/// preference holds, so a cycle whose velocity split is still uploading
/// (NO lags VRADH a minute; Dublin's PVOL velocity runs 15-minute
/// cadence) does not flap through a velocity-less frame. Past it the
/// newer frame wins outright: a velocity-lane outage must advance
/// `latest` to fresh reflectivity, never pin it to an hours-old pair.
const COMPLETE_FRAME_MAX_AGE_MINUTES: i64 = 20;

/// How many hourly key prefixes `latest` walks back from now before
/// declaring a site silent (covers publication lag and short outages
/// without ever listing the whole 24-hour cache).
const HOUR_LOOKBACK_SLOTS: i64 = 6;
/// The live ORD cache is a rolling 24-hour bucket. Load Loop walks that
/// whole window, newest hour back, then returns the newest requested frames
/// oldest-first for playback.
const RECENT_HOUR_LOOKBACK_SLOTS: i64 = 23;

/// ORD countries this provider enables for live/recent polling: lowercase
/// ODIM site-code prefix, bucket directory, and country label. Countries
/// with native BowEcho providers (SE/SMHI, DK/DMI, AT/GeoSphere, FI/FMI,
/// SK/SHMU, DE/DWD, CZ/CHMI) stay absent from the live picker.
const ORD_LIVE_COUNTRIES: &[(&str, &str, &str)] = &[
    ("be", "BE", "Belgium"),
    ("ch", "CH", "Switzerland"),
    ("ee", "EE", "Estonia"),
    ("es", "ES", "Spain"),
    ("fr", "FR", "France"),
    ("hr", "HR", "Croatia"),
    ("ie", "IE", "Ireland"),
    ("is", "IS", "Iceland"),
    ("lt", "LT", "Lithuania"),
    ("mt", "MT", "Malta"),
    ("nl", "NL", "Netherlands"),
    ("no", "NO", "Norway"),
    ("pl", "PL", "Poland"),
    ("ro", "RO", "Romania"),
    ("si", "SI", "Slovenia"),
];

/// ORD countries allowed for direct historical archive lookup. This is
/// intentionally broader than live/recent: native providers remain the
/// preferred live source, but the immutable archive bucket may be the only
/// path for older per-site ODIM files.
const ORD_ARCHIVE_COUNTRIES: &[(&str, &str, &str)] = &[
    ("at", "AT", "Austria"),
    ("be", "BE", "Belgium"),
    ("ch", "CH", "Switzerland"),
    ("cz", "CZ", "Czechia"),
    ("de", "DE", "Germany"),
    ("dk", "DK", "Denmark"),
    ("ee", "EE", "Estonia"),
    ("es", "ES", "Spain"),
    ("fi", "FI", "Finland"),
    ("fr", "FR", "France"),
    ("hr", "HR", "Croatia"),
    ("ie", "IE", "Ireland"),
    ("is", "IS", "Iceland"),
    ("lt", "LT", "Lithuania"),
    ("mt", "MT", "Malta"),
    ("nl", "NL", "Netherlands"),
    ("no", "NO", "Norway"),
    ("pl", "PL", "Poland"),
    ("ro", "RO", "Romania"),
    ("se", "SE", "Sweden"),
    ("si", "SI", "Slovenia"),
    ("sk", "SK", "Slovakia"),
];

/// ORD site table: ODIM code, label, latitude, longitude, and whether the
/// site publishes assembled `PVOL/` objects (`false` = per-sweep `SCAN/`),
/// used as the probe-order hint by [`OrdProvider::latest`].
///
/// Codes and coordinates: the ORD EDR locations catalog
/// (`https://api.meteogate.eu/eu-eumetnet-weather-radar/collections/observations/locations`,
/// fetched 2026-06-12; updated with BEHEL from the live 2026-06-16 bucket;
/// the 11 ES/AEMET sites — WIGOS `0-724-0-{code}`, live in ORD since
/// 2026-06-23 — added from the same catalog fetched 2026-07-07).
/// Labels left blank by
/// the EDR catalog (CH, NO, PL, RO) come from the EUMETNET OPERA radar
/// database, `OPERA_RADARS_DB.json` (fetched 2026-06-12) from
/// <https://eumetnet.eu/activities/observations-programme/current-activities/opera/>,
/// matched by ODIM code; both sources agree on coordinates to the 4
/// decimals kept here. Listed stations are OPERA status 1 (operational).
const ORD_SITES: &[(&str, &str, f32, f32, bool)] = &[
    ("behel", "Helchteren", 51.0702, 5.4054, true),
    ("bejab", "Jabbeke", 51.1917, 3.0642, true),
    ("bewid", "Wideumont", 49.9136, 5.5044, true),
    ("chalb", "Albis", 47.2843, 8.5120, false),
    ("chdol", "La Dole", 46.4251, 6.0994, false),
    ("chlem", "Monte Lema", 46.0408, 8.8332, false),
    ("chppm", "Plaine Morte", 46.3706, 7.4866, false),
    ("chwei", "Weissfluhgipfel", 46.8350, 9.7945, false),
    ("eesur", "Sürgavere", 58.4823, 25.5187, false),
    ("esahr", "Alhaurin Grande", 36.6134, -4.6593, true),
    ("esatn", "Artenara", 28.0188, -15.6145, true),
    ("esbnv", "Buenavista Norte", 28.3109, -16.8238, true),
    ("esclg", "Castillo las Guardas", 37.6887, -6.3331, true),
    ("esgld", "Gelida", 41.4082, 1.8849, true),
    ("eslid", "Valladolid", 41.9956, -4.6028, true),
    ("esnjr", "Nijar", 36.8324, -2.0821, true),
    ("espdg", "Perdiguera", 41.7340, -0.5459, true),
    ("essft", "Sierra Fuentes", 39.4288, -6.2853, true),
    ("essse", "San Sebastian", 43.4033, -2.8419, true),
    ("estjv", "Torrejon Velasco", 40.1759, -3.7137, true),
    ("frabb", "Abbeville", 50.1360, 1.8347, false),
    ("fraja", "Ajaccio", 41.9531, 8.7005, false),
    ("frave", "Avesnes", 50.1283, 3.8118, false),
    ("frbla", "Blaisy", 47.3552, 4.7759, false),
    ("frbol", "Bollène", 44.3231, 4.7622, false),
    ("frbor", "Bordeaux", 44.8315, -0.6919, false),
    ("frbou", "Bourges", 47.0586, 2.3596, false),
    ("frcae", "Falaise", 48.9272, -0.1495, false),
    ("frcol", "Collobrières", 43.2166, 6.3729, false),
    ("frgre", "Grèzes", 45.1044, 1.3697, false),
    ("frmom", "Momuy", 43.6245, -0.6094, false),
    ("frmtc", "Montancy", 47.3686, 7.0190, false),
    ("frnan", "Nancy", 48.7158, 6.5816, false),
    ("frnim", "Nîmes", 43.8061, 4.5027, false),
    ("frniz", "Saint-Nizier", 46.0678, 4.4454, false),
    ("fropo", "Opoul", 42.9184, 2.8650, false),
    ("frpla", "Plabennec", 48.4609, -4.4298, false),
    ("frtou", "Toulouse", 43.5743, 1.3763, false),
    ("frtre", "Treillières", 47.3374, -1.6563, false),
    ("frtro", "Arcis-sur-Aube", 48.4621, 4.3093, false),
    ("hrbil", "Bilogora", 45.8835, 17.2005, true),
    ("hrdeb", "Debeljak", 44.0452, 15.3764, true),
    ("hrgra", "Gradište", 45.1592, 18.7033, true),
    ("hrpun", "Puntijarka", 45.9078, 15.9684, true),
    ("hrulj", "Uljenje", 42.8944, 17.4783, true),
    ("iedub", "Dublin", 53.4299, -6.2443, true),
    ("iesha", "Shannon", 52.6928, -8.9200, true),
    ("isbjo", "Bjólfur", 65.2659, -14.0618, true),
    ("iskef", "Keflavík", 64.0257, -22.6354, true),
    ("isska", "Skagi", 66.0557, -20.2680, true),
    ("ltlau", "Laukuva", 55.6090, 22.2395, false),
    ("ltvil", "Vilnius", 54.6262, 25.1068, false),
    ("mtgud", "Gudja", 35.8528, 14.4747, true),
    ("nldhl", "Den Helder", 52.9528, 4.7906, true),
    ("nlhrw", "Herwijnen", 51.8369, 5.1381, true),
    ("noand", "Andøya", 69.2414, 16.0030, true),
    ("nober", "Berlevåg", 70.5107, 29.0184, true),
    ("nobml", "Bømlo", 59.8540, 5.0900, true),
    ("nohas", "Hasvik", 70.6052, 22.4430, true),
    ("nohfj", "Hafjell", 61.2318, 10.5273, true),
    ("nohgb", "Hægebostad", 58.3601, 7.1648, true),
    ("nohur", "Hurum", 59.6271, 10.5645, true),
    ("norsa", "Rissa", 63.6900, 10.2040, true),
    ("norsg", "Rássegálvárri", 69.2186, 23.4398, true),
    ("norst", "Røst", 67.5307, 12.0986, true),
    ("nosmn", "Sømna", 65.2199, 11.9926, true),
    ("nosta", "Stad", 62.1871, 5.1275, true),
    ("plbrz", "Brzuchania", 50.3942, 20.0832, true),
    ("plgdy", "Gdynia-Szemud", 54.5009, 18.2718, true),
    ("plgsa", "Góra Świętej Anny", 50.4639, 18.1532, true),
    ("plleg", "Legionowo", 52.4053, 20.9611, true),
    ("plpas", "Pastewnik", 50.8925, 16.0395, true),
    ("plpoz", "Poznań", 52.4133, 16.7970, true),
    ("plram", "Ramża", 50.1513, 18.7251, true),
    ("plrze", "Rzeszów", 50.1141, 22.0370, true),
    ("plswi", "Świdwin", 53.7958, 15.8368, true),
    ("pluzr", "Uzranki", 53.8557, 21.4123, true),
    ("robar", "Bârnova", 47.0118, 27.5825, true),
    ("robob", "Bobohalma", 46.3602, 24.2252, true),
    ("robuc", "București", 44.5127, 26.0773, true),
    ("rocra", "Craiova", 44.3103, 23.8674, true),
    ("romed", "Medgidia", 44.2434, 28.2506, true),
    ("roora", "Oradea", 47.0922, 21.9429, true),
    ("seatv", "Atvidaberg / Vilebo", 58.1059, 15.9365, true),
    ("rotim", "Timișoara", 45.7717, 21.2577, true),
    ("silis", "Lisca", 46.0678, 15.2849, true),
    ("sipas", "Pasja Ravan", 46.0980, 14.2282, true),
];

/// Which object directory a site publishes under.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ObjectKind {
    /// `PVOL/` — assembled (or per-moment) polar volumes.
    Pvol,
    /// `SCAN/` — one file per sweep, all moments bundled.
    Scan,
}

impl ObjectKind {
    fn dir(self) -> &'static str {
        match self {
            ObjectKind::Pvol => "PVOL",
            ObjectKind::Scan => "SCAN",
        }
    }
}

/// EUMETNET ORD: 15 additional European countries from the OPERA 24-hour
/// cache bucket, one provider.
pub struct OrdProvider {
    sites: SiteCache,
}

impl OrdProvider {
    pub fn new() -> Self {
        Self {
            sites: SiteCache::new(),
        }
    }
}

impl Default for OrdProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// One historical ORD scan resolved from the immutable archive bucket.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OrdArchivePlan {
    /// Scan anchor time in UTC.
    pub stamp_utc: DateTime<Utc>,
    /// ORD object directory used for the plan (`PVOL` or `SCAN`).
    pub object_kind: &'static str,
    /// The download/merge plan. URLs point at `openradar-archive`.
    pub frame: FramePlan,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum OrdPlanQuality {
    Other,
    VelocityOnly,
    ReflectivityOnly,
    ReflectivityAndVelocity,
}

#[derive(Clone, Debug)]
struct OrdFrameCandidate {
    stamp: NaiveDateTime,
    object_kind: ObjectKind,
    quality: OrdPlanQuality,
    frame: FramePlan,
}

#[derive(Clone, Debug)]
struct OrdPlanCollection {
    object_kind: ObjectKind,
    newest_stamp: DateTime<Utc>,
    quality: OrdPlanQuality,
    plans: Vec<OrdArchivePlan>,
}

/// Build download plans for every complete scan anchored inside one UTC hour.
///
/// The public CloudFerro archive uses the same key grammar as the live
/// 24-hour cache, so this reuses the live ORD split-file planner but swaps
/// the bucket base. Plans are returned oldest-first for loop installation.
pub fn archive_plans_for_hour(
    site_id: &str,
    hour_utc: DateTime<Utc>,
) -> Result<Vec<OrdArchivePlan>, String> {
    validate_site_code(site_id)?;
    let (_, dir, _) = country_for_archive_code(site_id)
        .ok_or_else(|| format!("ORD: site '{site_id}' is not in an enabled country"))?;
    let hour_utc = truncate_to_utc_hour(hour_utc);
    let pvol_hint = ORD_SITES
        .iter()
        .find(|(code, ..)| *code == site_id)
        .is_none_or(|&(.., pvol)| pvol);
    let kinds = if pvol_hint {
        [ObjectKind::Pvol, ObjectKind::Scan]
    } else {
        [ObjectKind::Scan, ObjectKind::Pvol]
    };

    let mut empty_kinds = Vec::new();
    let mut collections = Vec::new();
    let mut keys_by_kind = Vec::new();
    for kind in kinds {
        let mut keys = list_hour_keys_from_base(ARCHIVE_BUCKET_BASE, dir, site_id, kind, hour_utc)
            .map_err(|err| format!("ORD archive '{site_id}': {err}"))?;
        let previous = list_hour_keys_from_base(
            ARCHIVE_BUCKET_BASE,
            dir,
            site_id,
            kind,
            hour_utc - chrono::Duration::hours(1),
        )
        .map_err(|err| format!("ORD archive '{site_id}': {err}"))?;
        keys.extend(previous);
        keys.sort();
        keys.dedup();
        keys_by_kind.push((kind, keys.clone()));
        let plans = archive_plans_from_keys(site_id, kind, &keys, hour_utc)?;
        if !plans.is_empty() {
            collections.push(plan_collection(site_id, kind, plans));
        } else {
            empty_kinds.push(kind.dir());
        }
    }
    if let Some(collection) =
        mixed_plan_collection_from_kind_keys(ARCHIVE_BUCKET_BASE, site_id, &keys_by_kind, hour_utc)?
    {
        collections.push(collection);
    }

    if let Some(best) = best_plan_collection(collections) {
        return Ok(best.plans);
    }

    Err(format!(
        "ORD archive: no complete {} scan for {site_id} during {}Z",
        empty_kinds.join("/"),
        hour_utc.format("%Y-%m-%d %H")
    ))
}

/// Build the archive plan nearest a requested UTC time.
pub fn archive_plan_nearest(
    site_id: &str,
    target_utc: DateTime<Utc>,
) -> Result<OrdArchivePlan, String> {
    let hour = truncate_to_utc_hour(target_utc);
    let mut plans = Vec::new();
    let mut last_error = None;
    for offset_hours in [-1, 0, 1] {
        match archive_plans_for_hour(site_id, hour + chrono::Duration::hours(offset_hours)) {
            Ok(mut hour_plans) => plans.append(&mut hour_plans),
            Err(err) => last_error = Some(err),
        }
    }
    plans
        .into_iter()
        .min_by_key(|plan| (plan.stamp_utc - target_utc).num_seconds().unsigned_abs())
        .ok_or_else(|| {
            last_error.unwrap_or_else(|| {
                format!(
                    "ORD archive: no complete scan near {} for {site_id}",
                    target_utc.format("%Y-%m-%d %H:%MZ")
                )
            })
        })
}

impl IntlProvider for OrdProvider {
    fn id(&self) -> &'static str {
        "ord"
    }

    fn label(&self) -> &'static str {
        "EUMETNET ORD"
    }

    fn country(&self) -> &'static str {
        "Europe (OPERA)"
    }

    fn list_sites(&self) -> Result<Vec<IntlSite>, String> {
        self.sites.get_or_fill(|| {
            let mut sites: Vec<IntlSite> = Vec::new();
            let mut first_error: Option<String> = None;
            for &(_, dir, _) in ORD_LIVE_COUNTRIES {
                // Today first; the previous UTC day only when today's
                // directory is still empty (midnight / country outage).
                let mut found = false;
                for date in candidate_utc_dates() {
                    let prefix = format!("{}{dir}/", date_prefix(date));
                    let url = s3_style_listing_url(BUCKET_BASE, &prefix, Some("/"), None, 1000);
                    match fetch_s3_style_listing(&url) {
                        Ok(listing) => {
                            for site in sites_from_prefixes(&listing.common_prefixes) {
                                found = true;
                                sites.push(site);
                            }
                        }
                        Err(err) => {
                            first_error.get_or_insert(err);
                        }
                    }
                    if found {
                        break;
                    }
                }
            }
            if sites.is_empty() {
                return Err(match first_error {
                    Some(error) => format!("ORD bucket listed no sites ({error})"),
                    None => "ORD bucket listed no sites for today or yesterday (UTC)".to_owned(),
                });
            }
            sites.sort_by(|left, right| {
                (left.country, &left.site_id).cmp(&(right.country, &right.site_id))
            });
            sites.dedup_by(|left, right| left.site_id == right.site_id);
            Ok(sites)
        })
    }

    fn latest(&self, site_id: &str) -> Result<FramePlan, String> {
        validate_site_code(site_id)?;
        let (_, dir, _) = country_for_live_code(site_id)
            .ok_or_else(|| format!("ORD: site '{site_id}' is not in an enabled country"))?;

        // Static-table hint orders the probe; the other kind is still
        // tried so a country switching publishing shape keeps working.
        let pvol_hint = ORD_SITES
            .iter()
            .find(|(code, ..)| *code == site_id)
            .is_none_or(|&(.., pvol)| pvol);
        let kinds = if pvol_hint {
            [ObjectKind::Pvol, ObjectKind::Scan]
        } else {
            [ObjectKind::Scan, ObjectKind::Pvol]
        };

        let now = Utc::now();
        let mut best: Option<OrdFrameCandidate> = None;
        let mut keys_by_kind = Vec::new();
        for kind in kinds {
            let mut kind_keys = Vec::new();
            for slot in 0..=HOUR_LOOKBACK_SLOTS {
                let hour = now - chrono::Duration::hours(slot);
                // A transient listing failure must ERROR the tick, never
                // read as an empty hour: falling through to an older slot
                // would plan a stale frame under a different identity, and
                // the poller would install it, then flip back next tick
                // (review finding — the identity-flap mechanism).
                let keys = list_hour_keys(dir, site_id, kind, hour)
                    .map_err(|err| format!("ORD '{site_id}': {err}"))?;
                if keys.is_empty() {
                    continue;
                }
                // The trailing window can reach across the hour boundary,
                // so the adjacent older hour joins the candidate set. Same
                // rule: a failed adjacent listing errors the tick rather
                // than silently shrinking a boundary-straddling plan (e.g.
                // Norway's DBZH@x:59 + VRADH@(x+1):00 pairing collapsing
                // to a velocity-less frame).
                let mut all = keys;
                let previous =
                    list_hour_keys(dir, site_id, kind, hour - chrono::Duration::hours(1))
                        .map_err(|err| format!("ORD '{site_id}': {err}"))?;
                all.extend(previous);
                kind_keys.extend(all.iter().cloned());
                let candidate = plan_candidate_from_keys(site_id, kind, &all)?;
                if ord_candidate_is_better(&candidate, best.as_ref()) {
                    best = Some(candidate);
                }
            }
            if !kind_keys.is_empty() {
                kind_keys.sort();
                kind_keys.dedup();
                keys_by_kind.push((kind, kind_keys));
            }
        }
        if let Some(candidate) =
            mixed_candidate_from_kind_keys(BUCKET_BASE, site_id, &keys_by_kind)?
            && ord_candidate_is_better(&candidate, best.as_ref())
        {
            best = Some(candidate);
        }
        if let Some(best) = best {
            return Ok(best.frame);
        }
        Err(format!(
            "ORD: no files for site '{site_id}' in the last {HOUR_LOOKBACK_SLOTS} hours"
        ))
    }

    fn recent_source(&self) -> Option<&dyn RecentFrames> {
        Some(self)
    }

    fn archive_source(&self) -> Option<&dyn ArchiveFrames> {
        Some(self)
    }

    fn static_sites(&self) -> Vec<IntlSite> {
        ORD_SITES
            .iter()
            .filter_map(|&(code, label, latitude_deg, longitude_deg, _)| {
                if site_superseded_by_native_provider(code) {
                    return None;
                }
                let (_, _, country) = country_for_live_code(code)?;
                Some(IntlSite {
                    provider_id: self.id(),
                    site_id: code.to_owned(),
                    label: format!("{label} ({country})"),
                    country,
                    latitude_deg: Some(latitude_deg),
                    longitude_deg: Some(longitude_deg),
                })
            })
            .collect()
    }
}

impl RecentFrames for OrdProvider {
    fn recent_frames(&self, site_id: &str, count: usize) -> Result<Vec<FramePlan>, String> {
        if count == 0 {
            return Ok(Vec::new());
        }
        validate_site_code(site_id)?;
        let (_, dir, _) = country_for_live_code(site_id)
            .ok_or_else(|| format!("ORD: site '{site_id}' is not in an enabled country"))?;

        let pvol_hint = ORD_SITES
            .iter()
            .find(|(code, ..)| *code == site_id)
            .is_none_or(|&(.., pvol)| pvol);
        let kinds = if pvol_hint {
            [ObjectKind::Pvol, ObjectKind::Scan]
        } else {
            [ObjectKind::Scan, ObjectKind::Pvol]
        };

        let now = truncate_to_utc_hour(Utc::now());
        let mut first_error = None;
        let mut collections = Vec::new();
        let mut keys_by_kind = Vec::new();
        for kind in kinds {
            let mut plans = Vec::new();
            let mut kind_keys = Vec::new();
            for slot in 0..=RECENT_HOUR_LOOKBACK_SLOTS {
                let hour = now - chrono::Duration::hours(slot);
                let mut keys = match list_hour_keys(dir, site_id, kind, hour) {
                    Ok(keys) => keys,
                    Err(err) => {
                        first_error.get_or_insert(err);
                        continue;
                    }
                };
                if keys.is_empty() {
                    continue;
                }
                match list_hour_keys(dir, site_id, kind, hour - chrono::Duration::hours(1)) {
                    Ok(previous) => keys.extend(previous),
                    Err(err) => {
                        first_error.get_or_insert(err);
                    }
                }
                kind_keys.extend(keys.iter().cloned());
                let mut hour_plans =
                    plans_from_keys_for_hour(BUCKET_BASE, site_id, kind, &keys, hour)?;
                plans.append(&mut hour_plans);
                if plans.len() >= count {
                    break;
                }
            }
            if !plans.is_empty() {
                plans.sort_by_key(|plan| plan.stamp_utc);
                plans.dedup_by(|left, right| left.frame.identity == right.frame.identity);
                collections.push(plan_collection(site_id, kind, plans));
            }
            if !kind_keys.is_empty() {
                kind_keys.sort();
                kind_keys.dedup();
                keys_by_kind.push((kind, kind_keys));
            }
        }
        if let Some(collection) =
            mixed_recent_plan_collection_from_kind_keys(BUCKET_BASE, site_id, &keys_by_kind)?
        {
            collections.push(collection);
        }

        if let Some(best) = best_plan_collection(collections) {
            let skip = best.plans.len().saturating_sub(count);
            return Ok(best
                .plans
                .into_iter()
                .skip(skip)
                .map(|plan| plan.frame)
                .collect());
        }

        Err(first_error
            .map(|err| format!("ORD '{site_id}': {err}"))
            .unwrap_or_else(|| {
                format!("ORD: no recent files for site '{site_id}' in the 24-hour cache")
            }))
    }
}

impl ArchiveFrames for OrdProvider {
    /// Fold [`archive_plans_for_hour`] over the 24 UTC hours of
    /// `date_utc` — the immutable-archive-bucket wrapper. Hours that
    /// error (silent hour, transient listing failure) are skipped and
    /// the first error is reported only when the whole day yields
    /// nothing, mirroring [`RecentFrames::recent_frames`] above.
    fn day_plans(&self, site_id: &str, date_utc: NaiveDate) -> Result<Vec<FramePlan>, String> {
        let day_start =
            DateTime::<Utc>::from_naive_utc_and_offset(date_utc.and_time(NaiveTime::MIN), Utc);
        let mut plans = Vec::new();
        let mut first_error: Option<String> = None;
        for hour in 0..24 {
            match archive_plans_for_hour(site_id, day_start + chrono::Duration::hours(hour)) {
                Ok(mut hour_plans) => plans.append(&mut hour_plans),
                Err(err) => {
                    first_error.get_or_insert(err);
                }
            }
        }
        if plans.is_empty() {
            return Err(first_error.unwrap_or_else(|| {
                format!("ORD archive: no complete scans for {site_id} on {date_utc}")
            }));
        }
        Ok(archive_frames_oldest_first(plans, usize::MAX))
    }

    /// Hour-granular override of the day-folding default: ORD's archive
    /// keys carry per-scan stamps, so the window trims to the exact
    /// `[start, end]` bounds instead of whole days.
    fn window_plans(
        &self,
        site_id: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        max: usize,
    ) -> Result<Vec<FramePlan>, String> {
        if end < start {
            return Err(format!("archive window end {end} precedes start {start}"));
        }
        if max == 0 {
            return Ok(Vec::new());
        }
        let mut plans = Vec::new();
        let mut first_error: Option<String> = None;
        let mut hour = truncate_to_utc_hour(start);
        while hour <= end {
            match archive_plans_for_hour(site_id, hour) {
                Ok(mut hour_plans) => plans.append(&mut hour_plans),
                Err(err) => {
                    first_error.get_or_insert(err);
                }
            }
            hour += chrono::Duration::hours(1);
        }
        plans.retain(|plan| plan.stamp_utc >= start && plan.stamp_utc <= end);
        if plans.is_empty() {
            return Err(first_error.unwrap_or_else(|| {
                format!(
                    "ORD archive: no complete scans for {site_id} between {} and {}",
                    start.format("%Y-%m-%d %H:%MZ"),
                    end.format("%Y-%m-%d %H:%MZ")
                )
            }));
        }
        Ok(archive_frames_oldest_first(plans, max))
    }
}

/// Archive plans -> frame plans: chronological, identity-deduped, capped
/// to the NEWEST `max` while staying oldest-first (the same
/// tail-of-the-window shape as [`RecentFrames::recent_frames`]).
fn archive_frames_oldest_first(mut plans: Vec<OrdArchivePlan>, max: usize) -> Vec<FramePlan> {
    plans.sort_by_key(|plan| plan.stamp_utc);
    plans.dedup_by(|left, right| left.frame.identity == right.frame.identity);
    let skip = plans.len().saturating_sub(max);
    plans
        .into_iter()
        .skip(skip)
        .map(|plan| plan.frame)
        .collect()
}

/// Today and (for the midnight/outage window) the previous UTC day.
fn candidate_utc_dates() -> [chrono::NaiveDate; 2] {
    let today = Utc::now().date_naive();
    let yesterday = today
        .checked_sub_days(chrono::Days::new(1))
        .unwrap_or(today);
    [today, yesterday]
}

fn date_prefix(date: chrono::NaiveDate) -> String {
    use chrono::Datelike;
    format!("{:04}/{:02}/{:02}/", date.year(), date.month(), date.day())
}

fn truncate_to_utc_hour(time: DateTime<Utc>) -> DateTime<Utc> {
    time.with_minute(0)
        .and_then(|time| time.with_second(0))
        .and_then(|time| time.with_nanosecond(0))
        .unwrap_or(time)
}

/// Site codes are key-path segments (e.g. `nlhrw`); their first two
/// characters are the lowercase country prefix.
fn validate_site_code(site_id: &str) -> Result<(), String> {
    if site_id.len() >= 3
        && site_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        Ok(())
    } else {
        Err(format!("ORD: invalid site code '{site_id}'"))
    }
}

/// The live/recent enabled-country row for an ODIM site code, by prefix.
fn country_for_live_code(code: &str) -> Option<(&'static str, &'static str, &'static str)> {
    let prefix = code.get(..2)?;
    ORD_LIVE_COUNTRIES
        .iter()
        .find(|(lc, ..)| *lc == prefix)
        .copied()
}

/// The historical archive enabled-country row for an ODIM site code.
fn country_for_archive_code(code: &str) -> Option<(&'static str, &'static str, &'static str)> {
    let prefix = code.get(..2)?;
    ORD_ARCHIVE_COUNTRIES
        .iter()
        .find(|(lc, ..)| *lc == prefix)
        .copied()
}

fn site_superseded_by_native_provider(code: &str) -> bool {
    // Estonia's KAIA feed exposes both Harku and Sürgavere with the richer
    // national product set. Keep explicit ORD loads possible, but avoid a
    // mixed KAIA/ORD Estonia catalog in the picker and map markers.
    if code == "eesur" {
        return true;
    }
    // Romania's seven radars: the native ANM provider (`meteoromania`)
    // carries the same per-moment PVOLs ANM pushes to ORD *plus* the
    // dual-pol moments ORD strips (ZDR/KDP/RhoHV), so it owns the RO
    // picker/marker rows. RO stays in the live and archive country tables:
    // explicit ORD loads still work, and the immutable ORD archive remains
    // the deep-history path beyond ANM's ~3-day rolling window.
    matches!(
        code,
        "robar" | "robob" | "robuc" | "rocra" | "romed" | "roora" | "rotim"
    )
}

/// Picker/marker label: the static-table name when known (with the
/// country, since this provider's site list spans 15 of them), else the
/// uppercased code.
fn site_label(code: &str, country: &str) -> String {
    match ORD_SITES.iter().find(|(known, ..)| *known == code) {
        Some((_, label, ..)) => format!("{label} ({country})"),
        None => format!("{} ({country})", code.to_ascii_uppercase()),
    }
}

/// `2026/06/12/FR/frtou/` -> site `frtou`, as [`IntlSite`]s (codes outside
/// the enabled countries — and the `OPERA/` composite pseudo-station — are
/// dropped by the country-prefix lookup).
fn sites_from_prefixes(common_prefixes: &[String]) -> Vec<IntlSite> {
    common_prefixes
        .iter()
        .filter_map(|prefix| {
            let code = prefix.trim_end_matches('/').rsplit('/').next()?;
            validate_site_code(code).ok()?;
            if site_superseded_by_native_provider(code) {
                return None;
            }
            let (_, _, country) = country_for_live_code(code)?;
            let known = ORD_SITES.iter().find(|(id, ..)| *id == code);
            Some(IntlSite {
                provider_id: "ord",
                site_id: code.to_owned(),
                label: site_label(code, country),
                country,
                latitude_deg: known.map(|&(_, _, latitude_deg, _, _)| latitude_deg),
                longitude_deg: known.map(|&(_, _, _, longitude_deg, _)| longitude_deg),
            })
        })
        .collect()
}

/// List one hourly key prefix
/// (`{date}/{CC}/{site}/{kind}/{site}@{yyyymmdd}T{hh}`).
fn list_hour_keys(
    dir: &str,
    site_id: &str,
    kind: ObjectKind,
    hour: DateTime<Utc>,
) -> Result<Vec<String>, String> {
    list_hour_keys_from_base(BUCKET_BASE, dir, site_id, kind, hour)
}

fn list_hour_keys_from_base(
    bucket_base: &str,
    dir: &str,
    site_id: &str,
    kind: ObjectKind,
    hour: DateTime<Utc>,
) -> Result<Vec<String>, String> {
    let prefix = format!(
        "{}{dir}/{site_id}/{}/{site_id}@{}",
        date_prefix(hour.date_naive()),
        kind.dir(),
        hour.format("%Y%m%dT%H"),
    );
    let url = s3_style_listing_url(bucket_base, &prefix, None, None, 1000);
    let listing = fetch_s3_style_listing(&url)?;
    if listing.is_truncated {
        // S3 lists ascending, so truncation would hide the NEWEST keys —
        // error rather than plan from a silently incomplete hour (live
        // max observed is ~120 keys/site-hour against the 1000 cap).
        return Err(format!(
            "hour listing truncated at 1000 keys for {prefix} — refusing an incomplete plan"
        ));
    }
    Ok(listing.keys)
}

/// One parsed bucket object:
/// `{site}@{yyyymmdd}T{hhmm}@{elev[_elev...]}@{MOMENT[_MOMENT...]}.h5`.
#[derive(Clone, Debug, PartialEq)]
struct OrdFile {
    key: String,
    stamp: NaiveDateTime,
    elevations: String,
    moments: String,
}

impl OrdFile {
    fn parse(key: &str, site_id: &str) -> Option<Self> {
        let name = key.rsplit('/').next()?.strip_suffix(".h5")?;
        let mut fields = name.split('@');
        if fields.next()? != site_id {
            return None;
        }
        let stamp = NaiveDateTime::parse_from_str(fields.next()?, "%Y%m%dT%H%M").ok()?;
        let elevations = fields.next()?.to_owned();
        let moments = fields.next()?.to_owned();
        if fields.next().is_some() || elevations.is_empty() || moments.is_empty() {
            return None;
        }
        Some(Self {
            key: key.to_owned(),
            stamp,
            elevations,
            moments,
        })
    }

    fn elevation_count(&self) -> usize {
        self.elevations.split('_').count()
    }

    fn first_elevation(&self) -> f32 {
        self.elevations
            .split('_')
            .next()
            .and_then(|value| value.parse::<f32>().ok())
            .unwrap_or(f32::MAX)
    }

    fn elevation_tokens(&self) -> impl Iterator<Item = &str> {
        self.elevations.split('_').filter(|token| !token.is_empty())
    }

    fn has_reflectivity(&self) -> bool {
        self.moments.split('_').any(is_reflectivity_token)
    }

    fn has_velocity(&self) -> bool {
        self.moments.split('_').any(is_velocity_token)
    }

    /// Merge-order rank per the [`FramePlan`] contract (reflectivity
    /// first): cleaned reflectivity, then unfiltered reflectivity, then
    /// other moments, then velocity/spectrum-width-only files.
    fn moment_rank(&self) -> u8 {
        let tokens: Vec<&str> = self.moments.split('_').collect();
        if tokens
            .iter()
            .any(|token| *token == "DBZH" || *token == "DBZV")
        {
            0
        } else if tokens.iter().any(|token| *token == "TH" || *token == "TV") {
            1
        } else if tokens
            .iter()
            .all(|token| token.starts_with('V') || token.starts_with('W'))
        {
            3
        } else {
            2
        }
    }

    /// Every moment token is unfiltered reflectivity (TH/TV) — the only
    /// shape the DBZH shadow rule may drop.
    fn is_unfiltered_reflectivity_only(&self) -> bool {
        self.moments
            .split('_')
            .all(|token| token == "TH" || token == "TV")
    }
}

/// Assemble the newest frame from one site's listed keys (pure, so the
/// recorded-fixture tests drive every national publishing shape).
///
/// Anchor = the newest stamp; candidates = files inside the trailing
/// [`CYCLE_WINDOW_MINUTES`] window. PVOL splits group by moment set (the
/// newest file per set wins; stamp ties prefer more elevations, so BE's
/// 9-elevation Doppler `DBZH` never displaces the 11-elevation volume);
/// SCAN sweeps group per (elevation, moment set). A TH-only file whose
/// elevation set is identical to a chosen DBZH file is dropped: a lone TH
/// decodes as that part's reflectivity and collides with (and loses to)
/// the DBZH merge base on every cut — observed live on Norway's split
/// feed, where the TH file is also the largest part. Identity follows the
/// DWD grammar — site, anchor stamp, part count, FNV-1a of the key set —
/// a pure function of the listing per the [`FramePlan`] stability
/// contract.
#[cfg(test)]
fn plan_from_keys(site_id: &str, kind: ObjectKind, keys: &[String]) -> Result<FramePlan, String> {
    plan_from_keys_with_base(BUCKET_BASE, site_id, kind, keys)
}

fn plan_candidate_from_keys(
    site_id: &str,
    kind: ObjectKind,
    keys: &[String],
) -> Result<OrdFrameCandidate, String> {
    plan_candidate_from_keys_with_base(BUCKET_BASE, site_id, kind, keys)
}

#[cfg(test)]
fn plan_from_keys_with_base(
    bucket_base: &str,
    site_id: &str,
    kind: ObjectKind,
    keys: &[String],
) -> Result<FramePlan, String> {
    Ok(plan_candidate_from_keys_with_base(bucket_base, site_id, kind, keys)?.frame)
}

fn plan_candidate_from_keys_with_base(
    bucket_base: &str,
    site_id: &str,
    kind: ObjectKind,
    keys: &[String],
) -> Result<OrdFrameCandidate, String> {
    let files: Vec<OrdFile> = keys
        .iter()
        .filter_map(|key| OrdFile::parse(key, site_id))
        .collect();
    let anchor = select_frame_anchor(&files, kind)
        .ok_or_else(|| format!("ORD '{site_id}': no parseable volume keys in the listing"))?;
    plan_candidate_from_files_for_anchor(bucket_base, site_id, kind, &files, anchor)
}

fn archive_plans_from_keys(
    site_id: &str,
    kind: ObjectKind,
    keys: &[String],
    hour_utc: DateTime<Utc>,
) -> Result<Vec<OrdArchivePlan>, String> {
    plans_from_keys_for_hour(ARCHIVE_BUCKET_BASE, site_id, kind, keys, hour_utc)
}

fn plans_from_keys_for_hour(
    bucket_base: &str,
    site_id: &str,
    kind: ObjectKind,
    keys: &[String],
    hour_utc: DateTime<Utc>,
) -> Result<Vec<OrdArchivePlan>, String> {
    let files: Vec<OrdFile> = keys
        .iter()
        .filter_map(|key| OrdFile::parse(key, site_id))
        .collect();
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let hour_start = hour_utc.naive_utc();
    let hour_end = hour_start + chrono::Duration::hours(1);

    let all_files = files.clone();
    let mut remaining = files;
    let mut plans = Vec::new();
    while let Some(anchor) = select_frame_anchor(&remaining, kind) {
        let window_start = anchor - chrono::Duration::minutes(CYCLE_WINDOW_MINUTES);
        let chosen = choose_files_for_anchor(&remaining, kind, anchor);
        if anchor >= hour_start
            && anchor < hour_end
            && archive_chosen_files_are_complete(&all_files, &chosen)
        {
            let frame = plan_from_files_for_anchor(bucket_base, site_id, kind, &remaining, anchor)?;
            if !plans
                .iter()
                .any(|plan: &OrdArchivePlan| plan.frame.identity == frame.identity)
            {
                plans.push(OrdArchivePlan {
                    stamp_utc: DateTime::from_naive_utc_and_offset(anchor, Utc),
                    object_kind: kind.dir(),
                    frame,
                });
            }
        }
        remaining.retain(|file| file.stamp <= window_start || file.stamp > anchor);
    }
    plans.sort_by_key(|plan| plan.stamp_utc);
    Ok(plans)
}

fn plan_from_files_for_anchor(
    bucket_base: &str,
    site_id: &str,
    kind: ObjectKind,
    files: &[OrdFile],
    anchor: NaiveDateTime,
) -> Result<FramePlan, String> {
    Ok(plan_candidate_from_files_for_anchor(bucket_base, site_id, kind, files, anchor)?.frame)
}

fn plan_candidate_from_files_for_anchor(
    bucket_base: &str,
    site_id: &str,
    kind: ObjectKind,
    files: &[OrdFile],
    anchor: NaiveDateTime,
) -> Result<OrdFrameCandidate, String> {
    let chosen = choose_files_for_anchor(files, kind, anchor);
    plan_candidate_from_chosen_files(bucket_base, site_id, kind, anchor, chosen)
}

fn plan_candidate_from_chosen_files(
    bucket_base: &str,
    site_id: &str,
    kind: ObjectKind,
    anchor: NaiveDateTime,
    mut chosen: Vec<OrdFile>,
) -> Result<OrdFrameCandidate, String> {
    if chosen.is_empty() {
        return Err(format!(
            "ORD '{site_id}': no files matched scan window anchored at {}",
            anchor.format("%Y%m%dT%H%M")
        ));
    }

    // Redundant unfiltered reflectivity: parts carrying ONLY TH/TV and
    // fully shadowed by a DBZH part over the same elevations add bytes
    // but no moments. Literal TH/TV-only sets only (review finding: a
    // hypothetical TH_VRADH split must keep its velocity).
    let dbzh_elevations: Vec<String> = chosen
        .iter()
        .filter(|file| file.moment_rank() == 0)
        .map(|file| file.elevations.clone())
        .collect();
    chosen.retain(|file| {
        !file.is_unfiltered_reflectivity_only() || !dbzh_elevations.contains(&file.elevations)
    });

    // Reflectivity-bearing parts first (merge base), then by coverage and
    // sweep elevation — deterministic for identity stability.
    chosen.sort_by(|left, right| {
        left.moment_rank()
            .cmp(&right.moment_rank())
            .then(right.elevation_count().cmp(&left.elevation_count()))
            .then(left.first_elevation().total_cmp(&right.first_elevation()))
            .then(left.key.cmp(&right.key))
    });
    let quality = plan_quality_for_files(&chosen);

    let joined = chosen
        .iter()
        .map(|file| file.key.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let parts: Vec<PlanPart> = chosen
        .iter()
        .map(|file| PlanPart {
            url: format!("{bucket_base}/{}", file.key),
        })
        .collect();
    let frame = FramePlan {
        identity: format!(
            "{site_id}_{}_p{}_h{:016x}",
            anchor.format("%Y%m%dT%H%M"),
            parts.len(),
            fnv1a64(&joined)
        ),
        merge: parts.len() > 1,
        parts,
    };
    Ok(OrdFrameCandidate {
        stamp: anchor,
        object_kind: kind,
        quality,
        frame,
    })
}

fn mixed_candidate_from_kind_keys(
    bucket_base: &str,
    site_id: &str,
    keys_by_kind: &[(ObjectKind, Vec<String>)],
) -> Result<Option<OrdFrameCandidate>, String> {
    let mut best = None;
    for (ref_kind, ref_keys, vel_kind, vel_keys) in mixed_key_pairs(keys_by_kind) {
        if let Some(candidate) = mixed_ref_velocity_candidate_from_keys(
            bucket_base,
            site_id,
            ref_kind,
            ref_keys,
            vel_kind,
            vel_keys,
        )? && ord_candidate_is_better(&candidate, best.as_ref())
        {
            best = Some(candidate);
        }
    }
    Ok(best)
}

fn mixed_plan_collection_from_kind_keys(
    bucket_base: &str,
    site_id: &str,
    keys_by_kind: &[(ObjectKind, Vec<String>)],
    hour_utc: DateTime<Utc>,
) -> Result<Option<OrdPlanCollection>, String> {
    let mut plans = Vec::new();
    for (ref_kind, ref_keys, vel_kind, vel_keys) in mixed_key_pairs(keys_by_kind) {
        plans.extend(mixed_ref_velocity_plans_from_keys(
            bucket_base,
            site_id,
            ref_kind,
            ref_keys,
            vel_kind,
            vel_keys,
            hour_utc,
        )?);
    }
    Ok(plan_collection_from_mixed_plans(site_id, plans))
}

fn mixed_recent_plan_collection_from_kind_keys(
    bucket_base: &str,
    site_id: &str,
    keys_by_kind: &[(ObjectKind, Vec<String>)],
) -> Result<Option<OrdPlanCollection>, String> {
    let mut plans = Vec::new();
    for (ref_kind, ref_keys, vel_kind, vel_keys) in mixed_key_pairs(keys_by_kind) {
        plans.extend(mixed_ref_velocity_plans_from_keys_unbounded(
            bucket_base,
            site_id,
            ref_kind,
            ref_keys,
            vel_kind,
            vel_keys,
        )?);
    }
    Ok(plan_collection_from_mixed_plans(site_id, plans))
}

fn mixed_key_pairs(
    keys_by_kind: &[(ObjectKind, Vec<String>)],
) -> Vec<(ObjectKind, &[String], ObjectKind, &[String])> {
    let keys_for = |kind| {
        keys_by_kind
            .iter()
            .find(|(candidate, _)| *candidate == kind)
            .map(|(_, keys)| keys.as_slice())
            .unwrap_or(&[])
    };
    vec![
        (
            ObjectKind::Scan,
            keys_for(ObjectKind::Scan),
            ObjectKind::Pvol,
            keys_for(ObjectKind::Pvol),
        ),
        (
            ObjectKind::Pvol,
            keys_for(ObjectKind::Pvol),
            ObjectKind::Scan,
            keys_for(ObjectKind::Scan),
        ),
    ]
}

fn mixed_ref_velocity_candidate_from_keys(
    bucket_base: &str,
    site_id: &str,
    ref_kind: ObjectKind,
    ref_keys: &[String],
    vel_kind: ObjectKind,
    vel_keys: &[String],
) -> Result<Option<OrdFrameCandidate>, String> {
    let ref_files = parse_ord_files_for_site(ref_keys, site_id);
    let vel_files = parse_ord_files_for_site(vel_keys, site_id);
    let mut anchors = reflectivity_anchors_desc(&ref_files);
    for anchor in anchors.drain(..) {
        if let Some(candidate) = mixed_ref_velocity_candidate_for_anchor(
            bucket_base,
            site_id,
            ref_kind,
            &ref_files,
            vel_kind,
            &vel_files,
            anchor,
        )? {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

fn mixed_ref_velocity_plans_from_keys(
    bucket_base: &str,
    site_id: &str,
    ref_kind: ObjectKind,
    ref_keys: &[String],
    vel_kind: ObjectKind,
    vel_keys: &[String],
    hour_utc: DateTime<Utc>,
) -> Result<Vec<OrdArchivePlan>, String> {
    let hour_start = hour_utc.naive_utc();
    let hour_end = hour_start + chrono::Duration::hours(1);
    mixed_ref_velocity_plans_from_keys_inner(
        bucket_base,
        site_id,
        ref_kind,
        ref_keys,
        vel_kind,
        vel_keys,
        Some((hour_start, hour_end)),
    )
}

fn mixed_ref_velocity_plans_from_keys_unbounded(
    bucket_base: &str,
    site_id: &str,
    ref_kind: ObjectKind,
    ref_keys: &[String],
    vel_kind: ObjectKind,
    vel_keys: &[String],
) -> Result<Vec<OrdArchivePlan>, String> {
    mixed_ref_velocity_plans_from_keys_inner(
        bucket_base,
        site_id,
        ref_kind,
        ref_keys,
        vel_kind,
        vel_keys,
        None,
    )
}

fn mixed_ref_velocity_plans_from_keys_inner(
    bucket_base: &str,
    site_id: &str,
    ref_kind: ObjectKind,
    ref_keys: &[String],
    vel_kind: ObjectKind,
    vel_keys: &[String],
    hour_window: Option<(NaiveDateTime, NaiveDateTime)>,
) -> Result<Vec<OrdArchivePlan>, String> {
    let ref_files = parse_ord_files_for_site(ref_keys, site_id);
    let vel_files = parse_ord_files_for_site(vel_keys, site_id);
    let mut plans = Vec::new();
    for anchor in reflectivity_anchors_desc(&ref_files) {
        if let Some((start, end)) = hour_window
            && (anchor < start || anchor >= end)
        {
            continue;
        }
        if let Some(candidate) = mixed_ref_velocity_candidate_for_anchor(
            bucket_base,
            site_id,
            ref_kind,
            &ref_files,
            vel_kind,
            &vel_files,
            anchor,
        )? {
            plans.push(OrdArchivePlan {
                stamp_utc: DateTime::from_naive_utc_and_offset(candidate.stamp, Utc),
                object_kind: "SCAN+PVOL",
                frame: candidate.frame,
            });
        }
    }
    plans.sort_by_key(|plan| plan.stamp_utc);
    plans.dedup_by(|left, right| left.frame.identity == right.frame.identity);
    Ok(plans)
}

fn mixed_ref_velocity_candidate_for_anchor(
    bucket_base: &str,
    site_id: &str,
    ref_kind: ObjectKind,
    ref_files: &[OrdFile],
    vel_kind: ObjectKind,
    vel_files: &[OrdFile],
    anchor: NaiveDateTime,
) -> Result<Option<OrdFrameCandidate>, String> {
    let mut ref_chosen: Vec<OrdFile> = choose_files_for_anchor(ref_files, ref_kind, anchor)
        .into_iter()
        .filter(OrdFile::has_reflectivity)
        .collect();
    if ref_chosen.is_empty() {
        return Ok(None);
    }
    let mut vel_chosen = choose_velocity_files_at_or_before_anchor(vel_files, vel_kind, anchor);
    if vel_chosen.is_empty() {
        return Ok(None);
    }
    ref_chosen.append(&mut vel_chosen);
    if !chosen_has_reflectivity_and_velocity(&ref_chosen) {
        return Ok(None);
    }
    Ok(Some(plan_candidate_from_chosen_files(
        bucket_base,
        site_id,
        ref_kind,
        anchor,
        ref_chosen,
    )?))
}

fn parse_ord_files_for_site(keys: &[String], site_id: &str) -> Vec<OrdFile> {
    keys.iter()
        .filter_map(|key| OrdFile::parse(key, site_id))
        .collect()
}

fn reflectivity_anchors_desc(files: &[OrdFile]) -> Vec<NaiveDateTime> {
    let mut anchors: Vec<NaiveDateTime> = files
        .iter()
        .filter(|file| file.has_reflectivity())
        .map(|file| file.stamp)
        .collect();
    anchors.sort_unstable();
    anchors.dedup();
    anchors.reverse();
    anchors
}

fn choose_velocity_files_at_or_before_anchor(
    files: &[OrdFile],
    kind: ObjectKind,
    anchor: NaiveDateTime,
) -> Vec<OrdFile> {
    let oldest_allowed = anchor - chrono::Duration::minutes(MIXED_SOURCE_MAX_AGE_MINUTES);
    let mut chosen = Vec::new();
    for file in files {
        if !file.has_velocity() || file.stamp < oldest_allowed || file.stamp > anchor {
            continue;
        }
        let group = chosen.iter_mut().find(|other: &&mut OrdFile| match kind {
            ObjectKind::Pvol => other.moments == file.moments,
            ObjectKind::Scan => {
                other.moments == file.moments && other.elevations == file.elevations
            }
        });
        match group {
            Some(other) => {
                let newer = (file.stamp, file.elevation_count(), &file.key)
                    > (other.stamp, other.elevation_count(), &other.key);
                if newer {
                    *other = file.clone();
                }
            }
            None => chosen.push(file.clone()),
        }
    }
    chosen
}

fn plan_collection_from_mixed_plans(
    site_id: &str,
    mut plans: Vec<OrdArchivePlan>,
) -> Option<OrdPlanCollection> {
    if plans.is_empty() {
        return None;
    }
    plans.sort_by_key(|plan| plan.stamp_utc);
    plans.dedup_by(|left, right| left.frame.identity == right.frame.identity);
    let newest_stamp = plans.last()?.stamp_utc;
    let quality = plans
        .last()
        .map(|plan| frame_plan_quality(site_id, &plan.frame))
        .unwrap_or(OrdPlanQuality::Other);
    Some(OrdPlanCollection {
        object_kind: ObjectKind::Scan,
        newest_stamp,
        quality,
        plans,
    })
}

fn archive_chosen_files_are_complete(all_files: &[OrdFile], chosen: &[OrdFile]) -> bool {
    if chosen.is_empty() {
        return false;
    }
    let archive_has_reflectivity = all_files.iter().any(OrdFile::has_reflectivity);
    let archive_has_velocity = all_files.iter().any(OrdFile::has_velocity);
    if archive_has_reflectivity && archive_has_velocity {
        return chosen_has_reflectivity_and_velocity(chosen);
    }
    if archive_has_reflectivity {
        return chosen.iter().any(OrdFile::has_reflectivity);
    }
    if archive_has_velocity {
        return chosen.iter().any(OrdFile::has_velocity);
    }
    true
}

fn plan_quality_for_files(files: &[OrdFile]) -> OrdPlanQuality {
    let has_reflectivity = files.iter().any(OrdFile::has_reflectivity);
    let has_velocity = files.iter().any(OrdFile::has_velocity);
    match (
        chosen_has_reflectivity_and_velocity(files),
        has_reflectivity,
        has_velocity,
    ) {
        (true, _, _) => OrdPlanQuality::ReflectivityAndVelocity,
        (false, true, _) => OrdPlanQuality::ReflectivityOnly,
        (false, false, true) => OrdPlanQuality::VelocityOnly,
        (false, false, false) => OrdPlanQuality::Other,
    }
}

fn frame_plan_quality(site_id: &str, frame: &FramePlan) -> OrdPlanQuality {
    let files = frame
        .parts
        .iter()
        .filter_map(|part| OrdFile::parse(&part.url, site_id))
        .collect::<Vec<_>>();
    plan_quality_for_files(&files)
}

fn plan_collection(
    site_id: &str,
    object_kind: ObjectKind,
    plans: Vec<OrdArchivePlan>,
) -> OrdPlanCollection {
    let newest_stamp = plans
        .last()
        .map(|plan| plan.stamp_utc)
        .expect("plan collections are created only for non-empty plan lists");
    let quality = plans
        .last()
        .map(|plan| frame_plan_quality(site_id, &plan.frame))
        .unwrap_or(OrdPlanQuality::Other);
    OrdPlanCollection {
        object_kind,
        newest_stamp,
        quality,
        plans,
    }
}

fn best_plan_collection(collections: Vec<OrdPlanCollection>) -> Option<OrdPlanCollection> {
    collections.into_iter().max_by(compare_plan_collections)
}

fn compare_plan_collections(
    left: &OrdPlanCollection,
    right: &OrdPlanCollection,
) -> std::cmp::Ordering {
    left.quality
        .cmp(&right.quality)
        .then(left.newest_stamp.cmp(&right.newest_stamp))
        // Prefer the more complete assembled PVOL lane only as a final
        // deterministic tie. Product availability and freshness have already
        // decided the meaningful cases.
        .then(object_kind_tie_rank(left.object_kind).cmp(&object_kind_tie_rank(right.object_kind)))
}

fn ord_candidate_is_better(
    candidate: &OrdFrameCandidate,
    current: Option<&OrdFrameCandidate>,
) -> bool {
    let Some(current) = current else {
        return true;
    };
    // Completeness outranks freshness only between near-same-time frames
    // (the same-scan REF-only/REF+VEL flap guard); past the bounded
    // window the newer frame wins outright, so a velocity-lane outage
    // advances `latest` to fresh REF-only frames instead of pinning it
    // to an hours-old complete pair. Reflectivity-bearing frames still
    // beat velocity-only ones unconditionally: reflectivity is the
    // primary display product (Dublin's 15-minute VRADH lane must not
    // shadow its near-hourly reflectivity).
    if quality_has_reflectivity(candidate.quality) == quality_has_reflectivity(current.quality)
        && (candidate.stamp - current.stamp).abs()
            > chrono::Duration::minutes(COMPLETE_FRAME_MAX_AGE_MINUTES)
    {
        return candidate.stamp > current.stamp;
    }
    candidate
        .quality
        .cmp(&current.quality)
        .then(candidate.stamp.cmp(&current.stamp))
        .then(
            object_kind_tie_rank(candidate.object_kind)
                .cmp(&object_kind_tie_rank(current.object_kind)),
        )
        .is_gt()
}

fn quality_has_reflectivity(quality: OrdPlanQuality) -> bool {
    matches!(
        quality,
        OrdPlanQuality::ReflectivityAndVelocity | OrdPlanQuality::ReflectivityOnly
    )
}

fn object_kind_tie_rank(kind: ObjectKind) -> u8 {
    match kind {
        ObjectKind::Pvol => 1,
        ObjectKind::Scan => 0,
    }
}

fn select_frame_anchor(files: &[OrdFile], kind: ObjectKind) -> Option<NaiveDateTime> {
    let mut anchors: Vec<NaiveDateTime> = files.iter().map(|file| file.stamp).collect();
    anchors.sort_unstable();
    anchors.dedup();
    anchors.reverse();

    let mut fallback = None;
    for anchor in anchors {
        let chosen = choose_files_for_anchor(files, kind, anchor);
        if chosen.is_empty() {
            continue;
        }
        let newest = *fallback.get_or_insert(anchor);
        // The complete-cycle preference reaches back at most
        // [`COMPLETE_FRAME_MAX_AGE_MINUTES`] from the newest viable
        // anchor: during a velocity-lane outage the frame must anchor on
        // fresh reflectivity instead of an hours-old REF+VEL cycle.
        if newest - anchor > chrono::Duration::minutes(COMPLETE_FRAME_MAX_AGE_MINUTES) {
            break;
        }
        if chosen_has_reflectivity_and_velocity(&chosen) {
            return Some(anchor);
        }
    }
    fallback
}

fn choose_files_for_anchor(
    files: &[OrdFile],
    kind: ObjectKind,
    anchor: NaiveDateTime,
) -> Vec<OrdFile> {
    let window_start = anchor - chrono::Duration::minutes(CYCLE_WINDOW_MINUTES);
    let mut chosen: Vec<OrdFile> = Vec::new();
    for file in files {
        if file.stamp <= window_start || file.stamp > anchor {
            continue;
        }
        let group = chosen.iter_mut().find(|other| match kind {
            ObjectKind::Pvol => other.moments == file.moments,
            ObjectKind::Scan => {
                other.moments == file.moments && other.elevations == file.elevations
            }
        });
        match group {
            Some(other) => {
                let newer = (file.stamp, file.elevation_count(), &file.key)
                    > (other.stamp, other.elevation_count(), &other.key);
                if newer {
                    *other = file.clone();
                }
            }
            None => chosen.push(file.clone()),
        }
    }
    chosen
}

fn chosen_has_reflectivity_and_velocity(files: &[OrdFile]) -> bool {
    let mut reflectivity_elevations = BTreeSet::new();
    let mut velocity_elevations = BTreeSet::new();
    for file in files {
        let has_reflectivity = file.has_reflectivity();
        let has_velocity = file.has_velocity();
        if has_reflectivity && has_velocity {
            return true;
        }
        if has_reflectivity {
            reflectivity_elevations.extend(file.elevation_tokens().map(str::to_owned));
        }
        if has_velocity {
            velocity_elevations.extend(file.elevation_tokens().map(str::to_owned));
        }
    }
    reflectivity_elevations
        .iter()
        .any(|elevation| velocity_elevations.contains(elevation))
}

fn is_reflectivity_token(token: &str) -> bool {
    matches!(token, "DBZH" | "DBZV" | "DBZ" | "TH" | "TV")
}

fn is_velocity_token(token: &str) -> bool {
    token.starts_with('V')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::international::parse_s3_style_listing;

    /// All fixtures are live bucket captures from 2026-06-12 (hour-14 key
    /// listings trimmed to their newest entries; the FR site listing is
    /// complete) except the ES pair, captured 2026-07-07 (the complete
    /// esatn hour-17 key listing and the complete ES site listing).
    const FR_SITE_PREFIXES: &str = include_str!("fixtures/ord_fr_site_prefixes.xml");
    const ES_SITE_PREFIXES: &str = include_str!("fixtures/ord_es_site_prefixes.xml");
    const NLHRW_HOUR: &str = include_str!("fixtures/ord_nlhrw_hour.xml");
    const NOHUR_HOUR: &str = include_str!("fixtures/ord_nohur_hour.xml");
    const BEJAB_HOUR: &str = include_str!("fixtures/ord_bejab_hour.xml");
    const MTGUD_HOUR: &str = include_str!("fixtures/ord_mtgud_hour.xml");
    const FRTOU_HOUR: &str = include_str!("fixtures/ord_frtou_hour.xml");
    const IEDUB_HOUR: &str = include_str!("fixtures/ord_iedub_hour.xml");
    const ESATN_HOUR: &str = include_str!("fixtures/ord_esatn_hour.xml");

    fn fixture_keys(xml: &str) -> Vec<String> {
        parse_s3_style_listing(xml).expect("fixture parses").keys
    }

    #[test]
    fn country_table_covers_every_static_site_and_skips_native_feeds() {
        for &(code, ..) in ORD_SITES {
            assert!(
                country_for_archive_code(code).is_some(),
                "{code}: no archive country for prefix"
            );
            assert!(validate_site_code(code).is_ok(), "{code}: invalid code");
        }
        // Natively covered countries must stay excluded.
        for native in ["se", "dk", "at", "fi", "sk", "de", "cz"] {
            assert!(
                !ORD_LIVE_COUNTRIES.iter().any(|(lc, ..)| *lc == native),
                "{native} has a native BowEcho provider"
            );
        }
        assert_eq!(ORD_LIVE_COUNTRIES.len(), 15);
        assert!(ORD_ARCHIVE_COUNTRIES.iter().any(|(lc, ..)| *lc == "se"));
        assert_eq!(ORD_SITES.len(), 87);
        let visible = OrdProvider::new().static_sites();
        assert!(visible.iter().any(|site| site.site_id == "behel"
            && site.label == "Helchteren (Belgium)"
            && site.latitude_deg == Some(51.0702)
            && site.longitude_deg == Some(5.4054)));
        assert!(visible.iter().any(|site| site.site_id == "esatn"
            && site.label == "Artenara (Spain)"
            && site.latitude_deg == Some(28.0188)
            && site.longitude_deg == Some(-15.6145)));
        assert!(
            !visible.iter().any(|site| site.site_id == "eesur"),
            "Sürgavere is advertised by KAIA, not ORD"
        );
    }

    #[test]
    fn ord_archive_country_table_allows_sweden_without_live_advertising() {
        let visible = OrdProvider::new().static_sites();

        assert!(!visible.iter().any(|site| site.site_id == "seatv"));
        assert_eq!(
            country_for_archive_code("seatv").map(|(_, dir, country)| (dir, country)),
            Some(("SE", "Sweden"))
        );
        assert!(country_for_live_code("seatv").is_none());
    }

    /// Romania follows the Estonia/KAIA precedent: the native ANM provider
    /// (full dual-pol) owns the picker/marker rows, while RO stays in both
    /// country tables so explicit ORD loads and the deep ORD archive keep
    /// working.
    #[test]
    fn ord_romania_rows_defer_to_the_native_anm_provider() {
        let visible = OrdProvider::new().static_sites();
        for code in [
            "robar", "robob", "robuc", "rocra", "romed", "roora", "rotim",
        ] {
            assert!(
                site_superseded_by_native_provider(code),
                "{code}: must defer to meteoromania"
            );
            assert!(
                !visible.iter().any(|site| site.site_id == code),
                "{code}: advertised by ANM Romania, not ORD"
            );
            assert_eq!(
                country_for_live_code(code).map(|(_, dir, _)| dir),
                Some("RO"),
                "{code}: explicit ORD live loads stay possible"
            );
            assert_eq!(
                country_for_archive_code(code).map(|(_, dir, _)| dir),
                Some("RO"),
                "{code}: ORD remains the deep-archive path"
            );
        }
    }

    #[test]
    fn delimited_country_listing_yields_labelled_sites() {
        let listing = parse_s3_style_listing(FR_SITE_PREFIXES).expect("fixture parses");
        let sites = sites_from_prefixes(&listing.common_prefixes);
        assert_eq!(sites.len(), 20, "all 20 French radars");
        assert!(sites.iter().all(|site| site.provider_id == "ord"));
        assert!(sites.iter().all(|site| site.country == "France"));
        let toulouse = sites
            .iter()
            .find(|site| site.site_id == "frtou")
            .expect("frtou present");
        assert_eq!(toulouse.label, "Toulouse (France)");
        assert_eq!(toulouse.latitude_deg, Some(43.5743));
        assert_eq!(toulouse.longitude_deg, Some(1.3763));
        // Every live-listed code is in the static table -> all have coords.
        assert!(
            sites
                .iter()
                .all(|site| site.latitude_deg.is_some() && site.longitude_deg.is_some()),
            "live catalog should carry static coordinates for every site"
        );
    }

    #[test]
    fn composite_and_foreign_prefixes_are_dropped() {
        let prefixes = vec![
            "2026/06/12/OPERA/composites/".to_owned(),
            "2026/06/12/DE/deasb/".to_owned(), // native DWD coverage
            "2026/06/12/EE/eesur/".to_owned(), // native KAIA coverage
            "2026/06/12/NL/nldhl/".to_owned(),
        ];
        let sites = sites_from_prefixes(&prefixes);
        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].site_id, "nldhl");
        assert_eq!(sites[0].country, "Netherlands");
    }

    #[test]
    fn bundled_pvol_yields_a_single_unmerged_part() {
        let plan =
            plan_from_keys("nlhrw", ObjectKind::Pvol, &fixture_keys(NLHRW_HOUR)).expect("plan");
        assert!(!plan.merge);
        assert_eq!(plan.parts.len(), 1);
        assert!(plan.parts[0].url.starts_with(BUCKET_BASE));
        assert!(
            plan.parts[0]
                .url
                .ends_with("nlhrw@20260612T1455@0.3_0.8_1.2_2.0_2.8_4.5_6.0_8.0_10.0_12.0_15.0_20.0_25.0_90.0@DBZH_TH_VRADH.h5")
        );
        assert!(plan.identity.starts_with("nlhrw_20260612T1455_p1_h"));
        // Stability: same listing -> same plan.
        assert_eq!(
            plan_from_keys("nlhrw", ObjectKind::Pvol, &fixture_keys(NLHRW_HOUR)).expect("plan"),
            plan
        );
    }

    #[test]
    fn split_pvol_window_pairs_offset_velocity_with_reflectivity() {
        // Norway offsets VRADH (T1456, 8 elevations) one minute after
        // DBZH/TH (T1455, 10 elevations) and alternates a 12-elevation
        // scan strategy on the adjacent stamps — the trailing window must
        // keep exactly the newest pair, with the TH file (shadowed by
        // DBZH over the identical elevations) dropped.
        let plan =
            plan_from_keys("nohur", ObjectKind::Pvol, &fixture_keys(NOHUR_HOUR)).expect("plan");
        assert!(plan.merge);
        assert_eq!(plan.parts.len(), 2);
        assert!(plan.parts[0].url.contains("T1455@") && plan.parts[0].url.contains("@DBZH.h5"));
        assert!(plan.parts[1].url.contains("T1456@") && plan.parts[1].url.contains("@VRADH.h5"));
        assert!(plan.identity.starts_with("nohur_20260612T1456_p2_h"));
    }

    #[test]
    fn stamp_ties_prefer_the_volume_with_more_elevations() {
        // Belgium publishes two volumes per stamp: an 11-elevation
        // DBZH+TH pair and a 9-elevation Doppler DBZH+VRAD pair. The
        // 9-elevation DBZH must not displace the 11-elevation one, VRAD
        // (its own moment set) survives, and the TH file (same elevations
        // as the chosen DBZH) is dropped as redundant.
        let plan =
            plan_from_keys("bejab", ObjectKind::Pvol, &fixture_keys(BEJAB_HOUR)).expect("plan");
        assert!(plan.merge);
        assert_eq!(plan.parts.len(), 2);
        assert!(plan.parts[0].url.ends_with(
            "bejab@20260612T1455@0.3_0.9_1.5_2.2_2.9_3.8_4.8_6.5_9.0_13.0_25.0@DBZH.h5"
        ));
        assert!(
            plan.parts[1]
                .url
                .ends_with("bejab@20260612T1455@0.5_1.2_2.1_3.4_4.8_6.5_9.0_13.0_25.0@VRAD.h5")
        );
        assert!(plan.parts.iter().all(|part| !part.url.ends_with("@TH.h5")));
    }

    #[test]
    fn spain_split_pvol_merges_long_range_reflectivity_with_offset_doppler() {
        // AEMET (live in ORD since 2026-06-23) pairs a 3-elevation
        // long-range DBZH_TH volume on :x0 with a 2-elevation Doppler
        // DBZH_VRADH volume three minutes earlier at :x7. Both files
        // carry DBZH (moment rank 0), so the elevation-count tiebreak
        // must keep the long-range volume as the merge base with the
        // Doppler volume merged after it.
        let plan =
            plan_from_keys("esatn", ObjectKind::Pvol, &fixture_keys(ESATN_HOUR)).expect("plan");
        assert!(plan.merge);
        assert_eq!(plan.parts.len(), 2);
        assert!(
            plan.parts[0]
                .url
                .ends_with("esatn@20260707T1730@0.5_1.3_2.1@DBZH_TH.h5")
        );
        assert!(
            plan.parts[1]
                .url
                .ends_with("esatn@20260707T1727@0.5_1.5@DBZH_VRADH.h5")
        );
        assert!(plan.identity.starts_with("esatn_20260707T1730_p2_h"));
        // The pair is a complete frame: the Doppler part alone already
        // carries reflectivity and velocity.
        let candidate =
            plan_candidate_from_keys("esatn", ObjectKind::Pvol, &fixture_keys(ESATN_HOUR))
                .expect("candidate");
        assert_eq!(candidate.quality, OrdPlanQuality::ReflectivityAndVelocity);
    }

    #[test]
    fn spain_doppler_only_tail_still_plans_a_complete_frame() {
        // Between the :x7 Doppler upload and the next :x0 long-range
        // upload the newest stamp is the DBZH_VRADH volume alone. It
        // carries both reflectivity and velocity itself, so the frame
        // advances to it instead of waiting out the three-minute gap.
        let keys: Vec<String> = fixture_keys(ESATN_HOUR)
            .into_iter()
            .filter(|key| !key.contains("T1730@"))
            .collect();
        let candidate =
            plan_candidate_from_keys("esatn", ObjectKind::Pvol, &keys).expect("candidate");
        assert_eq!(candidate.quality, OrdPlanQuality::ReflectivityAndVelocity);
        assert!(!candidate.frame.merge);
        assert_eq!(candidate.frame.parts.len(), 1);
        assert!(
            candidate.frame.parts[0]
                .url
                .ends_with("esatn@20260707T1727@0.5_1.5@DBZH_VRADH.h5")
        );
    }

    #[test]
    fn spain_catalog_spans_mainland_and_the_canary_islands() {
        // Live ES site listing (2026-07-07) resolves to the 11 AEMET
        // sites with static coordinates.
        let listing = parse_s3_style_listing(ES_SITE_PREFIXES).expect("fixture parses");
        let listed = sites_from_prefixes(&listing.common_prefixes);
        assert_eq!(listed.len(), 11, "all 11 AEMET radars");
        assert!(listed.iter().all(|site| site.country == "Spain"));
        assert!(
            listed
                .iter()
                .all(|site| site.latitude_deg.is_some() && site.longitude_deg.is_some()),
            "every ES site must carry static coordinates"
        );

        let spain: Vec<_> = OrdProvider::new()
            .static_sites()
            .into_iter()
            .filter(|site| site.country == "Spain")
            .collect();
        let ids: Vec<&str> = spain.iter().map(|site| site.site_id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "esahr", "esatn", "esbnv", "esclg", "esgld", "eslid", "esnjr", "espdg", "essft",
                "essse", "estjv"
            ]
        );
        assert!(
            listed
                .iter()
                .all(|site| ids.contains(&site.site_id.as_str()))
        );
        // The network spans two worlds: the Canary Islands (Artenara on
        // Gran Canaria, Buenavista Norte on Tenerife, below 29°N and west
        // of 13°W) and the mainland (nine sites north of 36°N).
        let canaries: Vec<&str> = spain
            .iter()
            .filter(|site| site.longitude_deg.unwrap_or(0.0) < -13.0)
            .map(|site| site.site_id.as_str())
            .collect();
        assert_eq!(canaries, ["esatn", "esbnv"]);
        assert!(
            spain
                .iter()
                .filter(|site| !canaries.contains(&site.site_id.as_str()))
                .all(|site| {
                    site.latitude_deg.unwrap_or(0.0) > 36.0
                        && site.longitude_deg.unwrap_or(99.0) > -7.0
                }),
            "mainland sites sit inside the Iberian box"
        );
    }

    #[test]
    fn high_cadence_single_moment_sites_keep_only_the_newest_file() {
        // Malta uploads a DBZH volume every 1-2 minutes; the window holds
        // several but they are one moment set -> newest only, unmerged.
        let plan =
            plan_from_keys("mtgud", ObjectKind::Pvol, &fixture_keys(MTGUD_HOUR)).expect("plan");
        assert!(!plan.merge);
        assert_eq!(plan.parts.len(), 1);
        assert!(plan.parts[0].url.contains("mtgud@20260612T1459@"));
        assert!(plan.identity.starts_with("mtgud_20260612T1459_p1_h"));
    }

    #[test]
    fn scan_sites_assemble_one_sweep_per_elevation_in_ascending_order() {
        // France publishes one file per sweep (all moments bundled),
        // staggered over the cycle; grouping is per elevation, the merge
        // base is the lowest sweep, and the previous cycle's repeats of
        // the same elevation are superseded.
        let plan =
            plan_from_keys("frtou", ObjectKind::Scan, &fixture_keys(FRTOU_HOUR)).expect("plan");
        assert!(plan.merge);
        let elevations: Vec<&str> = plan
            .parts
            .iter()
            .map(|part| {
                part.url
                    .rsplit('/')
                    .next()
                    .and_then(|name| name.split('@').nth(2))
                    .expect("elevation field")
            })
            .collect();
        assert_eq!(
            elevations,
            ["0.78", "1.48", "2.5", "3.5", "6.49", "9.43"],
            "anchor T1459 window keeps one sweep per tilt, ascending; the \
             previous cycle's 0.81° sweep at T1454 sits exactly on the \
             exclusive window edge and stays out"
        );
        // The newest 2.5° sweep (T1457) supersedes the T1452 one.
        assert!(
            plan.parts
                .iter()
                .any(|part| part.url.contains("frtou@20260612T1457@2.5@"))
        );
        assert!(plan.identity.starts_with("frtou_20260612T1459_p6_h"));
    }

    #[test]
    fn velocity_only_sites_still_plan_a_frame() {
        // Dublin's PVOL lane is VRADH-only at 15-minute cadence: the plan
        // is a single velocity volume (BowEcho's own region-based
        // dealiaser handles the not-centrally-dealiased VRADH).
        let plan =
            plan_from_keys("iedub", ObjectKind::Pvol, &fixture_keys(IEDUB_HOUR)).expect("plan");
        assert!(!plan.merge);
        assert_eq!(plan.parts.len(), 1);
        assert!(plan.parts[0].url.contains("iedub@20260612T1445@"));
    }

    #[test]
    fn dublin_mixed_frame_combines_scan_reflectivity_with_pvol_velocity() {
        let pvol_keys = vec![
            "2026/06/25/IE/iedub/PVOL/iedub@20260625T1515@0.5_2.2_4.3@VRADH.h5".to_owned(),
            // Newer than the reflectivity anchor: must not be merged early.
            "2026/06/25/IE/iedub/PVOL/iedub@20260625T1545@0.5_2.2_4.3@VRADH.h5".to_owned(),
        ];
        let scan_keys = vec![
            "2026/06/25/IE/iedub/SCAN/iedub@20260625T1530@0.5@TH.h5".to_owned(),
            "2026/06/25/IE/iedub/SCAN/iedub@20260625T1531@2.9@TH.h5".to_owned(),
            "2026/06/25/IE/iedub/SCAN/iedub@20260625T1531@4.0@TH.h5".to_owned(),
        ];

        let pvol = plan_candidate_from_keys("iedub", ObjectKind::Pvol, &pvol_keys).unwrap();
        let scan = plan_candidate_from_keys("iedub", ObjectKind::Scan, &scan_keys).unwrap();

        assert_eq!(pvol.quality, OrdPlanQuality::VelocityOnly);
        assert_eq!(scan.quality, OrdPlanQuality::ReflectivityOnly);
        let mixed = mixed_candidate_from_kind_keys(
            BUCKET_BASE,
            "iedub",
            &[
                (ObjectKind::Pvol, pvol_keys.clone()),
                (ObjectKind::Scan, scan_keys.clone()),
            ],
        )
        .expect("mixed planning")
        .expect("mixed frame");

        assert_eq!(mixed.quality, OrdPlanQuality::ReflectivityAndVelocity);
        assert_eq!(
            mixed.stamp,
            NaiveDateTime::parse_from_str("20260625T1531", "%Y%m%dT%H%M").unwrap()
        );
        assert!(mixed.frame.merge);
        assert!(
            mixed
                .frame
                .parts
                .iter()
                .any(|part| part.url.contains("/SCAN/")
                    && part.url.contains("iedub@20260625T1530@0.5@TH.h5"))
        );
        assert!(mixed.frame.parts.iter().any(|part| {
            part.url.contains("/PVOL/")
                && part
                    .url
                    .contains("iedub@20260625T1515@0.5_2.2_4.3@VRADH.h5")
        }));
        assert!(
            !mixed
                .frame
                .parts
                .iter()
                .any(|part| part.url.contains("iedub@20260625T1545@")),
            "future velocity must not be shown before its scan time"
        );

        let best = best_plan_collection(vec![
            plan_collection(
                "iedub",
                ObjectKind::Pvol,
                vec![OrdArchivePlan {
                    stamp_utc: DateTime::from_naive_utc_and_offset(pvol.stamp, Utc),
                    object_kind: ObjectKind::Pvol.dir(),
                    frame: pvol.frame.clone(),
                }],
            ),
            plan_collection(
                "iedub",
                ObjectKind::Scan,
                vec![OrdArchivePlan {
                    stamp_utc: DateTime::from_naive_utc_and_offset(scan.stamp, Utc),
                    object_kind: ObjectKind::Scan.dir(),
                    frame: scan.frame.clone(),
                }],
            ),
            plan_collection_from_mixed_plans(
                "iedub",
                vec![OrdArchivePlan {
                    stamp_utc: DateTime::from_naive_utc_and_offset(mixed.stamp, Utc),
                    object_kind: "SCAN+PVOL",
                    frame: mixed.frame.clone(),
                }],
            )
            .expect("mixed collection"),
        ])
        .expect("best collection");
        assert_eq!(best.quality, OrdPlanQuality::ReflectivityAndVelocity);
        assert_eq!(best.object_kind, ObjectKind::Scan);
        assert_eq!(best.plans[0].object_kind, "SCAN+PVOL");
    }

    #[test]
    fn dublin_mixed_frame_rejects_future_only_velocity() {
        let pvol_keys =
            vec!["2026/06/25/IE/iedub/PVOL/iedub@20260625T1545@0.5_2.2_4.3@VRADH.h5".to_owned()];
        let scan_keys = vec![
            "2026/06/25/IE/iedub/SCAN/iedub@20260625T1530@0.5@TH.h5".to_owned(),
            "2026/06/25/IE/iedub/SCAN/iedub@20260625T1531@2.9@TH.h5".to_owned(),
        ];

        let mixed = mixed_candidate_from_kind_keys(
            BUCKET_BASE,
            "iedub",
            &[(ObjectKind::Pvol, pvol_keys), (ObjectKind::Scan, scan_keys)],
        )
        .expect("mixed planning");

        assert!(mixed.is_none());
    }

    #[test]
    fn ord_candidate_scoring_keeps_complete_pvol_over_scan_reflectivity_only() {
        let pvol_keys = vec![
            "2026/06/25/NL/nlhrw/PVOL/nlhrw@20260625T1515@0.5_2.2_4.3@DBZH.h5".to_owned(),
            "2026/06/25/NL/nlhrw/PVOL/nlhrw@20260625T1515@0.5_2.2_4.3@VRADH.h5".to_owned(),
        ];
        let scan_keys = vec![
            "2026/06/25/NL/nlhrw/SCAN/nlhrw@20260625T1531@0.5@TH.h5".to_owned(),
            "2026/06/25/NL/nlhrw/SCAN/nlhrw@20260625T1531@2.9@TH.h5".to_owned(),
        ];

        let pvol = plan_candidate_from_keys("nlhrw", ObjectKind::Pvol, &pvol_keys).unwrap();
        let scan = plan_candidate_from_keys("nlhrw", ObjectKind::Scan, &scan_keys).unwrap();

        assert_eq!(pvol.quality, OrdPlanQuality::ReflectivityAndVelocity);
        assert_eq!(scan.quality, OrdPlanQuality::ReflectivityOnly);
        assert!(
            !ord_candidate_is_better(&scan, Some(&pvol)),
            "a complete PVOL frame should remain preferred over REF-only SCAN"
        );
    }

    #[test]
    fn fresh_reflectivity_only_candidate_outranks_hours_old_complete_frame() {
        // Velocity-lane outage: the newest lookback slot yields DBZH-only
        // cycles while the last complete DBZH+VRADH pair is three hours
        // old. Freshness must win — completeness may not pin `latest` to
        // hours-old data.
        let elevs = "0.5_1.5_2.5_3.5_5.4_9.1_15.0_23.8";
        let fresh_keys = vec![format!(
            "2026/06/13/PL/plleg/PVOL/plleg@20260613T1951@{elevs}@DBZH.h5"
        )];
        let stale_keys = vec![
            format!("2026/06/13/PL/plleg/PVOL/plleg@20260613T1651@{elevs}@DBZH.h5"),
            format!("2026/06/13/PL/plleg/PVOL/plleg@20260613T1651@{elevs}@VRADH.h5"),
        ];

        let fresh = plan_candidate_from_keys("plleg", ObjectKind::Pvol, &fresh_keys).unwrap();
        let stale = plan_candidate_from_keys("plleg", ObjectKind::Pvol, &stale_keys).unwrap();

        assert_eq!(fresh.quality, OrdPlanQuality::ReflectivityOnly);
        assert_eq!(stale.quality, OrdPlanQuality::ReflectivityAndVelocity);
        assert!(
            ord_candidate_is_better(&fresh, Some(&stale)),
            "fresh REF-only must displace a three-hour-old REF+VEL frame"
        );
        assert!(
            !ord_candidate_is_better(&stale, Some(&fresh)),
            "an hours-old REF+VEL frame must not displace fresh reflectivity"
        );
    }

    #[test]
    fn same_cycle_still_prefers_the_complete_frame() {
        // The completeness preference survives inside the flap window: at
        // the same stamp REF+VEL beats REF-only, and a REF-only anchor one
        // cycle newer still waits for the complete pair.
        let elevs = "0.5_1.5_2.5_3.5_5.4_9.1_15.0_23.8";
        let complete_keys = vec![
            format!("2026/06/13/PL/plleg/PVOL/plleg@20260613T1651@{elevs}@DBZH.h5"),
            format!("2026/06/13/PL/plleg/PVOL/plleg@20260613T1651@{elevs}@VRADH.h5"),
        ];
        let ref_only_same = vec![format!(
            "2026/06/13/PL/plleg/PVOL/plleg@20260613T1651@{elevs}@DBZH.h5"
        )];
        let ref_only_next = vec![format!(
            "2026/06/13/PL/plleg/PVOL/plleg@20260613T1656@{elevs}@DBZH.h5"
        )];

        let complete = plan_candidate_from_keys("plleg", ObjectKind::Pvol, &complete_keys).unwrap();
        let same = plan_candidate_from_keys("plleg", ObjectKind::Pvol, &ref_only_same).unwrap();
        let next = plan_candidate_from_keys("plleg", ObjectKind::Pvol, &ref_only_next).unwrap();

        assert!(
            ord_candidate_is_better(&complete, Some(&same)),
            "REF+VEL beats REF-only at the same stamp"
        );
        assert!(!ord_candidate_is_better(&same, Some(&complete)));
        assert!(
            !ord_candidate_is_better(&next, Some(&complete)),
            "a REF-only anchor one cycle newer must not flap past the complete pair"
        );
    }

    #[test]
    fn velocity_outage_listing_anchors_on_fresh_reflectivity() {
        // Within one listing the anchor walk's complete-cycle preference
        // is bounded the same way: DBZH cycles kept publishing after the
        // VRADH lane stopped, so the frame anchors on the newest
        // reflectivity, not the 90-minute-old complete pair.
        let elevs = "0.5_1.5_2.5_3.5_5.4_9.1_15.0_23.8";
        let keys = vec![
            format!("2026/06/13/PL/plleg/PVOL/plleg@20260613T1651@{elevs}@DBZH.h5"),
            format!("2026/06/13/PL/plleg/PVOL/plleg@20260613T1651@{elevs}@VRADH.h5"),
            format!("2026/06/13/PL/plleg/PVOL/plleg@20260613T1721@{elevs}@DBZH.h5"),
            format!("2026/06/13/PL/plleg/PVOL/plleg@20260613T1821@{elevs}@DBZH.h5"),
        ];

        let candidate = plan_candidate_from_keys("plleg", ObjectKind::Pvol, &keys).unwrap();

        assert_eq!(candidate.quality, OrdPlanQuality::ReflectivityOnly);
        assert_eq!(
            candidate.stamp,
            NaiveDateTime::parse_from_str("20260613T1821", "%Y%m%dT%H%M").unwrap()
        );
        assert!(candidate.frame.identity.starts_with("plleg_20260613T1821_"));
    }

    #[test]
    fn newer_velocity_only_tail_does_not_displace_complete_pvol_cycle() {
        let elevs = "0.5_1.5_2.5_3.5_5.4_9.1_15.0_23.8";
        let keys = vec![
            format!("2026/06/13/PL/plleg/PVOL/plleg@20260613T1651@{elevs}@DBZH.h5"),
            format!("2026/06/13/PL/plleg/PVOL/plleg@20260613T1651@{elevs}@TH.h5"),
            format!("2026/06/13/PL/plleg/PVOL/plleg@20260613T1651@{elevs}@VRADH.h5"),
            format!("2026/06/13/PL/plleg/PVOL/plleg@20260613T1656@{elevs}@VRADH.h5"),
        ];

        let plan = plan_from_keys("plleg", ObjectKind::Pvol, &keys).expect("plan");
        let parts: Vec<&str> = plan.parts.iter().map(|part| part.url.as_str()).collect();

        assert!(plan.merge);
        assert_eq!(parts.len(), 2, "parts: {parts:?}");
        assert!(plan.identity.starts_with("plleg_20260613T1651_p2_h"));
        assert!(parts[0].contains("T1651@") && parts[0].ends_with("@DBZH.h5"));
        assert!(parts[1].contains("T1651@") && parts[1].ends_with("@VRADH.h5"));
        assert!(
            parts.iter().all(|part| !part.contains("T1656@")),
            "newer incomplete tail should wait for its cycle"
        );
    }

    #[test]
    fn archive_plan_uses_archive_bucket_and_drops_incomplete_tail() {
        let elevs = "0.5_1.5_2.5_3.5_5.4_9.1_15.0_23.8";
        let keys = vec![
            format!("2026/05/30/PL/plbrz/PVOL/plbrz@20260530T1716@{elevs}@DBZH.h5"),
            format!("2026/05/30/PL/plbrz/PVOL/plbrz@20260530T1716@{elevs}@TH.h5"),
            format!("2026/05/30/PL/plbrz/PVOL/plbrz@20260530T1716@{elevs}@VRADH.h5"),
            format!("2026/05/30/PL/plbrz/PVOL/plbrz@20260530T1721@{elevs}@VRADH.h5"),
        ];
        let plans = archive_plans_from_keys(
            "plbrz",
            ObjectKind::Pvol,
            &keys,
            utc_time(2026, 5, 30, 17, 0),
        )
        .expect("plans");

        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].stamp_utc, utc_time(2026, 5, 30, 17, 16));
        assert_eq!(plans[0].object_kind, "PVOL");
        let parts: Vec<&str> = plans[0]
            .frame
            .parts
            .iter()
            .map(|part| part.url.as_str())
            .collect();
        assert_eq!(parts.len(), 2, "parts: {parts:?}");
        assert!(
            parts
                .iter()
                .all(|part| part.starts_with(ARCHIVE_BUCKET_BASE))
        );
        assert!(parts[0].ends_with("@DBZH.h5"));
        assert!(parts[1].ends_with("@VRADH.h5"));
        assert!(parts.iter().all(|part| !part.contains("T1721@")));
    }

    #[test]
    fn archive_hour_plans_are_oldest_first() {
        let elevs = "0.5_1.5_2.5_3.5_5.4_9.1_15.0_23.8";
        let keys = vec![
            format!("2026/05/30/PL/plbrz/PVOL/plbrz@20260530T1706@{elevs}@DBZH.h5"),
            format!("2026/05/30/PL/plbrz/PVOL/plbrz@20260530T1706@{elevs}@VRADH.h5"),
            format!("2026/05/30/PL/plbrz/PVOL/plbrz@20260530T1716@{elevs}@DBZH.h5"),
            format!("2026/05/30/PL/plbrz/PVOL/plbrz@20260530T1716@{elevs}@VRADH.h5"),
        ];
        let plans = archive_plans_from_keys(
            "plbrz",
            ObjectKind::Pvol,
            &keys,
            utc_time(2026, 5, 30, 17, 0),
        )
        .expect("plans");

        let stamps: Vec<_> = plans.iter().map(|plan| plan.stamp_utc).collect();
        assert_eq!(
            stamps,
            [utc_time(2026, 5, 30, 17, 6), utc_time(2026, 5, 30, 17, 16)]
        );
    }

    #[test]
    fn archive_hour_collapses_staggered_split_products_into_scan_cycles() {
        let elevs = "0.5_1.5_2.5_3.5_5.4_9.1_15.0_23.8";
        let keys = vec![
            format!("2026/05/30/PL/plbrz/PVOL/plbrz@20260530T1706@{elevs}@DBZH.h5"),
            format!("2026/05/30/PL/plbrz/PVOL/plbrz@20260530T1707@{elevs}@TH.h5"),
            format!("2026/05/30/PL/plbrz/PVOL/plbrz@20260530T1708@{elevs}@VRADH.h5"),
            format!("2026/05/30/PL/plbrz/PVOL/plbrz@20260530T1716@{elevs}@DBZH.h5"),
            format!("2026/05/30/PL/plbrz/PVOL/plbrz@20260530T1717@{elevs}@TH.h5"),
            format!("2026/05/30/PL/plbrz/PVOL/plbrz@20260530T1718@{elevs}@VRADH.h5"),
        ];
        let plans = archive_plans_from_keys(
            "plbrz",
            ObjectKind::Pvol,
            &keys,
            utc_time(2026, 5, 30, 17, 0),
        )
        .expect("plans");

        let stamps: Vec<_> = plans.iter().map(|plan| plan.stamp_utc).collect();
        assert_eq!(
            stamps,
            [utc_time(2026, 5, 30, 17, 8), utc_time(2026, 5, 30, 17, 18)]
        );
        assert_eq!(plans[0].frame.parts.len(), 2);
        assert_eq!(plans[1].frame.parts.len(), 2);
    }

    #[test]
    fn newer_scan_tail_with_mismatched_product_heights_does_not_win() {
        let keys = vec![
            "2026/06/13/EE/eesur/SCAN/eesur@20260613T1651@0.5@DBZH.h5".to_owned(),
            "2026/06/13/EE/eesur/SCAN/eesur@20260613T1651@0.5@VRADH.h5".to_owned(),
            "2026/06/13/EE/eesur/SCAN/eesur@20260613T1656@0.5@DBZH.h5".to_owned(),
            "2026/06/13/EE/eesur/SCAN/eesur@20260613T1656@1.5@VRADH.h5".to_owned(),
        ];

        let plan = plan_from_keys("eesur", ObjectKind::Scan, &keys).expect("plan");
        let parts: Vec<&str> = plan.parts.iter().map(|part| part.url.as_str()).collect();

        assert!(plan.merge);
        assert_eq!(parts.len(), 2, "parts: {parts:?}");
        assert!(plan.identity.starts_with("eesur_20260613T1651_p2_h"));
        assert!(parts[0].contains("T1651@0.5@DBZH.h5"));
        assert!(parts[1].contains("T1651@0.5@VRADH.h5"));
        assert!(
            parts.iter().all(|part| !part.contains("T1656@")),
            "newer scan tail has reflectivity and velocity at different heights"
        );
    }

    #[test]
    fn unparseable_listings_are_descriptive_errors_never_panics() {
        let err = plan_from_keys("nlhrw", ObjectKind::Pvol, &[]).unwrap_err();
        assert!(err.contains("no parseable"), "unexpected error: {err}");
        let junk = vec![
            "2026/06/12/NL/nlhrw/PVOL/garbage".to_owned(),
            "2026/06/12/NL/nlhrw/PVOL/nlhrw@not-a-stamp@1.0@DBZH.h5".to_owned(),
            "2026/06/12/NL/nlhrw/PVOL/othersite@20260612T1455@1.0@DBZH.h5".to_owned(),
        ];
        let err = plan_from_keys("nlhrw", ObjectKind::Pvol, &junk).unwrap_err();
        assert!(err.contains("no parseable"), "unexpected error: {err}");
    }

    #[test]
    fn site_codes_are_validated_before_key_interpolation() {
        assert!(validate_site_code("nlhrw").is_ok());
        assert!(validate_site_code("").is_err());
        assert!(validate_site_code("nl").is_err());
        assert!(validate_site_code("NLHRW").is_err());
        assert!(validate_site_code("nl/hrw").is_err());
        assert!(validate_site_code("nl@hrw").is_err());
    }

    fn utc_time(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        let date = chrono::NaiveDate::from_ymd_opt(year, month, day).expect("date");
        DateTime::from_naive_utc_and_offset(date.and_hms_opt(hour, minute, 0).expect("time"), Utc)
    }

    #[test]
    fn moment_rank_orders_reflectivity_before_velocity() {
        let file = |moments: &str| OrdFile {
            key: String::new(),
            stamp: NaiveDateTime::parse_from_str("20260612T1455", "%Y%m%dT%H%M").unwrap(),
            elevations: "0.5".to_owned(),
            moments: moments.to_owned(),
        };
        assert_eq!(file("DBZH_TH_VRADH").moment_rank(), 0);
        assert_eq!(file("DBZH").moment_rank(), 0);
        assert_eq!(file("TH").moment_rank(), 1);
        assert_eq!(file("DBZH_RHOHV_TH_VRADH_ZDR").moment_rank(), 0);
        assert_eq!(file("ZDR_RHOHV").moment_rank(), 2);
        assert_eq!(file("VRADH").moment_rank(), 3);
        assert_eq!(file("VRAD").moment_rank(), 3);
        assert_eq!(file("VRADH_WRADH").moment_rank(), 3);
    }

    #[test]
    fn dbzh_shadow_drops_only_literal_th_or_tv_sets() {
        // Review finding: a hypothetical TH_VRADH split shares the TH rank
        // but carries velocity — the shadow rule must keep it.
        let elevs = "0.5_1.5_2.5";
        let keys = vec![
            format!("2026/06/12/NO/nohur/PVOL/nohur@20260612T1455@{elevs}@DBZH.h5"),
            format!("2026/06/12/NO/nohur/PVOL/nohur@20260612T1455@{elevs}@TH.h5"),
            format!("2026/06/12/NO/nohur/PVOL/nohur@20260612T1455@{elevs}@TH_VRADH.h5"),
            format!("2026/06/12/NO/nohur/PVOL/nohur@20260612T1455@{elevs}@TV.h5"),
            format!("2026/06/12/NO/nohur/PVOL/nohur@20260612T1455@{elevs}@TH_TV.h5"),
        ];
        let plan = plan_from_keys("nohur", ObjectKind::Pvol, &keys).expect("plan");
        let parts: Vec<&str> = plan.parts.iter().map(|part| part.url.as_str()).collect();
        // DBZH base + the velocity-bearing TH_VRADH survive; the pure
        // TH / TV / TH_TV parts are shadowed away.
        assert_eq!(parts.len(), 2, "parts: {parts:?}");
        assert!(parts[0].ends_with("@DBZH.h5"));
        assert!(parts[1].ends_with("@TH_VRADH.h5"));
    }

    #[test]
    fn hour_prefix_follows_the_bucket_layout() {
        use chrono::TimeZone;
        let hour = Utc.with_ymd_and_hms(2026, 6, 12, 14, 7, 30).unwrap();
        let prefix = format!(
            "{}NL/nlhrw/{}/nlhrw@{}",
            date_prefix(hour.date_naive()),
            ObjectKind::Pvol.dir(),
            hour.format("%Y%m%dT%H"),
        );
        assert_eq!(prefix, "2026/06/12/NL/nlhrw/PVOL/nlhrw@20260612T14");
    }

    /// The archive-lookup shaping shared by `day_plans`/`window_plans`:
    /// chronological, identity-deduped, and capped to the NEWEST `max`
    /// while staying oldest-first for loop installation.
    #[test]
    fn archive_frames_sort_dedupe_and_keep_the_newest_capped_tail() {
        let plan = |minute: u32, identity: &str| OrdArchivePlan {
            stamp_utc: utc_time(2026, 6, 9, 5, minute),
            object_kind: ObjectKind::Pvol.dir(),
            frame: FramePlan {
                identity: identity.to_owned(),
                parts: vec![PlanPart {
                    url: format!("https://example.invalid/{identity}.h5"),
                }],
                merge: false,
            },
        };
        let frames = archive_frames_oldest_first(
            vec![
                plan(30, "deess_0530"),
                plan(10, "deess_0510"),
                plan(10, "deess_0510"),
                plan(20, "deess_0520"),
                plan(0, "deess_0500"),
            ],
            3,
        );
        assert_eq!(
            frames
                .iter()
                .map(|frame| frame.identity.as_str())
                .collect::<Vec<_>>(),
            vec!["deess_0510", "deess_0520", "deess_0530"],
            "duplicate dropped, oldest trimmed by the cap, order oldest-first"
        );
        assert!(archive_frames_oldest_first(Vec::new(), 3).is_empty());
        assert!(archive_frames_oldest_first(vec![plan(0, "deess_0500")], 0).is_empty());
    }

    /// Live bucket roundtrip across multiple newly-enabled countries:
    /// list sites, plan, download every part, decode through the shared
    /// ODIM router, and (for split plans) merge. Network test; run with
    /// `cargo test -p data_source ord_live -- --ignored --nocapture`
    #[test]
    #[ignore = "live ORD bucket probe — run manually with --ignored"]
    fn ord_live_roundtrip_lists_plans_downloads_and_decodes() {
        let provider = OrdProvider::new();
        let sites = provider.list_sites().expect("live ORD site list");
        println!("{} ORD sites listed live", sites.len());
        assert!(sites.len() >= 30, "expected most of the 85 catalog sites");

        // One bundled-PVOL country (NL), split-PVOL countries (NO/PL and
        // Spain's dual-DBZH AEMET pairing), and one SCAN country (FR).
        for probe in ["nlhrw", "nohur", "plram", "frtou", "hrbil", "esatn"] {
            let site = sites
                .iter()
                .find(|site| site.site_id == probe)
                .unwrap_or_else(|| panic!("{probe} missing from live catalog"));
            let plan = provider.latest(&site.site_id).expect("live frame plan");
            println!(
                "{} ({}): identity={} parts={} merge={}",
                site.site_id,
                site.country,
                plan.identity,
                plan.parts.len(),
                plan.merge
            );
            let mut volumes = Vec::new();
            for part in &plan.parts {
                let bytes = crate::fetch_volume_bytes(&part.url).expect("part download");
                let volume = nexrad_io::decode_supported_volume_bytes(&bytes).expect("ODIM decode");
                volumes.push(volume);
            }
            let cuts: usize = volumes.iter().map(|volume| volume.cuts.len()).sum();
            let moments: std::collections::BTreeSet<String> = volumes
                .iter()
                .flat_map(|volume| volume.cuts.iter())
                .flat_map(|cut| cut.moments.keys())
                .map(|moment| moment.short_name().to_owned())
                .collect();
            println!(
                "  decoded {} part(s): site={} cuts={} moments=[{}]",
                volumes.len(),
                volumes
                    .first()
                    .map(|volume| volume.site.id.clone())
                    .unwrap_or_default(),
                cuts,
                moments.into_iter().collect::<Vec<_>>().join(", ")
            );
            assert!(cuts > 0, "{probe}: decoded no cuts");
        }
    }
}
