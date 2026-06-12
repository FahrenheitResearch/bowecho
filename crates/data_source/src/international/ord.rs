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
//! - per-moment PVOL splits, 1-2 volumes per stamp (BE, IS, NO, PL, RO),
//!   with NO offsetting its velocity stamp a minute after reflectivity;
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
//! the national feeds stay preferred. The `OPERA/` composite prefix is a
//! pseudo-station (gridded composites, not polar volumes) and is also
//! excluded.

use chrono::{DateTime, NaiveDateTime, Utc};

use super::listing::fnv1a64;
use super::{
    FramePlan, IntlProvider, IntlSite, PlanPart, SiteCache, fetch_s3_style_listing,
    s3_style_listing_url,
};

const BUCKET_BASE: &str = "https://s3.waw3-1.cloudferro.com/openradar-24h";

/// One scan cycle: a file belongs to the frame anchored at the newest
/// stamp when its own stamp is inside this trailing window (same role as
/// the DWD cycle window; ORD national cycles run 5 minutes or slower).
const CYCLE_WINDOW_MINUTES: i64 = 5;

/// How many hourly key prefixes `latest` walks back from now before
/// declaring a site silent (covers publication lag and short outages
/// without ever listing the whole 24-hour cache).
const HOUR_LOOKBACK_SLOTS: i64 = 6;

/// ORD countries this provider enables: lowercase ODIM site-code prefix,
/// bucket directory, and country label. Countries with native BowEcho
/// providers (SE, DK, AT, FI, SK, DE, CZ) are deliberately absent.
const ORD_COUNTRIES: &[(&str, &str, &str)] = &[
    ("be", "BE", "Belgium"),
    ("ch", "CH", "Switzerland"),
    ("ee", "EE", "Estonia"),
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

/// ORD site table: ODIM code, label, latitude, longitude, and whether the
/// site publishes assembled `PVOL/` objects (`false` = per-sweep `SCAN/`),
/// used as the probe-order hint by [`OrdProvider::latest`].
///
/// Codes and coordinates: the ORD EDR locations catalog
/// (`https://api.meteogate.eu/eu-eumetnet-weather-radar/collections/observations/locations`,
/// fetched 2026-06-12; 74 stations across the enabled countries, exactly
/// matching the bucket's site directories that day). Labels left blank by
/// the EDR catalog (CH, NO, PL, RO) come from the EUMETNET OPERA radar
/// database, `OPERA_RADARS_DB.json` (fetched 2026-06-12) from
/// <https://eumetnet.eu/activities/observations-programme/current-activities/opera/>,
/// matched by ODIM code; both sources agree on coordinates to the 4
/// decimals kept here. All 74 stations are OPERA status 1 (operational).
const ORD_SITES: &[(&str, &str, f32, f32, bool)] = &[
    ("bejab", "Jabbeke", 51.1917, 3.0642, true),
    ("bewid", "Wideumont", 49.9136, 5.5044, true),
    ("chalb", "Albis", 47.2843, 8.5120, false),
    ("chdol", "La Dole", 46.4251, 6.0994, false),
    ("chlem", "Monte Lema", 46.0408, 8.8332, false),
    ("chppm", "Plaine Morte", 46.3706, 7.4866, false),
    ("chwei", "Weissfluhgipfel", 46.8350, 9.7945, false),
    ("eesur", "Sürgavere", 58.4823, 25.5187, false),
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

/// EUMETNET ORD: 14 additional European countries from the OPERA 24-hour
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
            for &(_, dir, _) in ORD_COUNTRIES {
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
        let (_, dir, _) = country_for_code(site_id)
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
        for kind in kinds {
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
                return plan_from_keys(site_id, kind, &all);
            }
        }
        Err(format!(
            "ORD: no files for site '{site_id}' in the last {HOUR_LOOKBACK_SLOTS} hours"
        ))
    }

    fn static_sites(&self) -> Vec<IntlSite> {
        ORD_SITES
            .iter()
            .filter_map(|&(code, label, latitude_deg, longitude_deg, _)| {
                let (_, _, country) = country_for_code(code)?;
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

/// The enabled-country row for an ODIM site code, by its 2-letter prefix.
fn country_for_code(code: &str) -> Option<(&'static str, &'static str, &'static str)> {
    let prefix = code.get(..2)?;
    ORD_COUNTRIES.iter().find(|(lc, ..)| *lc == prefix).copied()
}

/// Picker/marker label: the static-table name when known (with the
/// country, since this provider's site list spans 14 of them), else the
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
            let (_, _, country) = country_for_code(code)?;
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
    let prefix = format!(
        "{}{dir}/{site_id}/{}/{site_id}@{}",
        date_prefix(hour.date_naive()),
        kind.dir(),
        hour.format("%Y%m%dT%H"),
    );
    let url = s3_style_listing_url(BUCKET_BASE, &prefix, None, None, 1000);
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
fn plan_from_keys(site_id: &str, kind: ObjectKind, keys: &[String]) -> Result<FramePlan, String> {
    let files: Vec<OrdFile> = keys
        .iter()
        .filter_map(|key| OrdFile::parse(key, site_id))
        .collect();
    let anchor = files
        .iter()
        .map(|file| file.stamp)
        .max()
        .ok_or_else(|| format!("ORD '{site_id}': no parseable volume keys in the listing"))?;
    let window_start = anchor - chrono::Duration::minutes(CYCLE_WINDOW_MINUTES);

    let mut chosen: Vec<OrdFile> = Vec::new();
    for file in &files {
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

    let joined = chosen
        .iter()
        .map(|file| file.key.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let parts: Vec<PlanPart> = chosen
        .iter()
        .map(|file| PlanPart {
            url: format!("{BUCKET_BASE}/{}", file.key),
        })
        .collect();
    Ok(FramePlan {
        identity: format!(
            "{site_id}_{}_p{}_h{:016x}",
            anchor.format("%Y%m%dT%H%M"),
            parts.len(),
            fnv1a64(&joined)
        ),
        merge: parts.len() > 1,
        parts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::international::parse_s3_style_listing;

    /// All fixtures are live bucket captures from 2026-06-12 (hour-14 key
    /// listings trimmed to their newest entries; the FR site listing is
    /// complete).
    const FR_SITE_PREFIXES: &str = include_str!("fixtures/ord_fr_site_prefixes.xml");
    const NLHRW_HOUR: &str = include_str!("fixtures/ord_nlhrw_hour.xml");
    const NOHUR_HOUR: &str = include_str!("fixtures/ord_nohur_hour.xml");
    const BEJAB_HOUR: &str = include_str!("fixtures/ord_bejab_hour.xml");
    const MTGUD_HOUR: &str = include_str!("fixtures/ord_mtgud_hour.xml");
    const FRTOU_HOUR: &str = include_str!("fixtures/ord_frtou_hour.xml");
    const IEDUB_HOUR: &str = include_str!("fixtures/ord_iedub_hour.xml");

    fn fixture_keys(xml: &str) -> Vec<String> {
        parse_s3_style_listing(xml).expect("fixture parses").keys
    }

    #[test]
    fn country_table_covers_every_static_site_and_skips_native_feeds() {
        for &(code, ..) in ORD_SITES {
            assert!(
                country_for_code(code).is_some(),
                "{code}: no enabled country for prefix"
            );
            assert!(validate_site_code(code).is_ok(), "{code}: invalid code");
        }
        // Natively covered countries must stay excluded.
        for native in ["se", "dk", "at", "fi", "sk", "de", "cz"] {
            assert!(
                !ORD_COUNTRIES.iter().any(|(lc, ..)| *lc == native),
                "{native} has a native BowEcho provider"
            );
        }
        assert_eq!(ORD_COUNTRIES.len(), 14);
        assert_eq!(ORD_SITES.len(), 74);
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
        assert!(sites.len() >= 30, "expected most of the 74 catalog sites");

        // One bundled-PVOL country (NL), one split-PVOL country (NO/PL),
        // and one SCAN country (FR).
        for probe in ["nlhrw", "nohur", "plram", "frtou", "hrbil"] {
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
