//! Tropical cyclone data — a unified model plus parsers for the two free,
//! keyless sources BowEcho aggregates:
//!
//! - **NHC** `CurrentStorms.json` — official for the Atlantic and East/Central
//!   Pacific; carries max wind (kt), min pressure (mb), and motion directly.
//! - **GDACS** `geteventlist/EVENTS4APP?eventtypes=TC` — a global aggregator
//!   (JTWC/JMA/etc.) that covers every other basin, including the West Pacific.
//!   Its `getgeometry` endpoint returns the track, cone, and impact polygons —
//!   but NO honest per-point forecast wind (it repeats the storm's *current*
//!   severity on every point).
//! - **JTWC** (Joint Typhoon Warning Center) Tropical Cyclone Warning text —
//!   the official U.S. forecast authority for the basins NHC does not cover
//!   (West Pacific, Indian Ocean, Southern Hemisphere). Its fixed-format
//!   `wpNNyyweb.txt` bulletin carries a per-point forecast track WITH max
//!   sustained wind (kt) at 12/24/36/48/72/96/120 h — the West-Pacific analogue
//!   of NHC's TCM. Active warnings are discovered from the JTWC RSS feed, then
//!   matched to a GDACS storm by name to enrich its forecast dots with real
//!   Saffir–Simpson intensity (the cone/track still come from GDACS).
//!
//! The two feed a single [`TropicalCyclone`] so the UI never cares which
//! center issued a storm. Fields are intentionally comprehensive (wind, gusts,
//! pressure, motion, category) — each source fills what it has, and richer
//! intensity sources (ATCF, CIMSS ADT) can enrich the same record later.
//!
//! Scales/definitions: Saffir–Simpson by 1-min max sustained wind (kt); the
//! West Pacific "(Super) Typhoon" labels map onto the same wind thresholds.

use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use serde::Deserialize;

/// A lon/lat point in degrees (east/north positive), matching the app's other
/// geographic records.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GeoPoint {
    pub lon: f32,
    pub lat: f32,
}

/// Ocean basin a storm lives in. Used for labeling ("Hurricane" vs "Typhoon")
/// and for grouping the storm list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Basin {
    Atlantic,
    EastPacific,
    CentralPacific,
    WestPacific,
    NorthIndian,
    SouthIndian,
    SouthPacific,
    Other,
}

impl Basin {
    /// Approximate basin from position — the fallback when a source does not
    /// name one (GDACS). Boundaries follow the WMO/agency areas of
    /// responsibility closely enough for labeling.
    pub fn from_lon_lat(lon: f32, lat: f32) -> Self {
        let lon = normalize_lon(lon);
        if lat >= 0.0 {
            if (-100.0..=0.0).contains(&lon) || lon > 0.0 && lon <= 20.0 {
                Basin::Atlantic
            } else if (-180.0..-140.0).contains(&lon) {
                Basin::CentralPacific
            } else if (-140.0..-100.0).contains(&lon) {
                Basin::EastPacific
            } else if (100.0..=180.0).contains(&lon) || (-180.0..-160.0).contains(&lon) {
                Basin::WestPacific
            } else if (30.0..100.0).contains(&lon) {
                Basin::NorthIndian
            } else {
                Basin::Other
            }
        } else if (30.0..135.0).contains(&lon) {
            Basin::SouthIndian
        } else if (135.0..=180.0).contains(&lon) || (-180.0..-70.0).contains(&lon) {
            Basin::SouthPacific
        } else {
            Basin::Other
        }
    }

    /// The intensity noun used in this basin at/above hurricane force.
    fn strong_noun(self) -> &'static str {
        match self {
            Basin::Atlantic | Basin::EastPacific | Basin::CentralPacific => "Hurricane",
            Basin::WestPacific => "Typhoon",
            _ => "Cyclone",
        }
    }
}

/// Which center/aggregator a record came from (shown in the card for honesty).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Nhc,
    Gdacs,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Nhc => "NHC",
            Source::Gdacs => "GDACS",
        }
    }
}

/// Saffir–Simpson bin by 1-min max sustained wind (kt).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    TropicalDepression,
    TropicalStorm,
    One,
    Two,
    Three,
    Four,
    Five,
}

impl Category {
    /// Saffir–Simpson thresholds (kt): TD < 34, TS 34–63, 1: 64–82, 2: 83–95,
    /// 3: 96–112, 4: 113–136, 5: ≥ 137.
    pub fn from_wind_kt(kt: f32) -> Self {
        if kt < 34.0 {
            Category::TropicalDepression
        } else if kt < 64.0 {
            Category::TropicalStorm
        } else if kt < 83.0 {
            Category::One
        } else if kt < 96.0 {
            Category::Two
        } else if kt < 113.0 {
            Category::Three
        } else if kt < 137.0 {
            Category::Four
        } else {
            Category::Five
        }
    }

    /// A basin-aware label, e.g. "Category 4 Typhoon", "Super Typhoon",
    /// "Tropical Storm".
    pub fn label(self, basin: Basin) -> String {
        match self {
            Category::TropicalDepression => "Tropical Depression".to_owned(),
            Category::TropicalStorm => "Tropical Storm".to_owned(),
            Category::Five if basin == Basin::WestPacific => "Super Typhoon".to_owned(),
            Category::One => format!("Category 1 {}", basin.strong_noun()),
            Category::Two => format!("Category 2 {}", basin.strong_noun()),
            Category::Three => format!("Category 3 {}", basin.strong_noun()),
            Category::Four => format!("Category 4 {}", basin.strong_noun()),
            Category::Five => format!("Category 5 {}", basin.strong_noun()),
        }
    }
}

/// One point on the forecast track.
#[derive(Clone, Debug, PartialEq)]
pub struct ForecastPoint {
    pub position: GeoPoint,
    pub valid_time: Option<DateTime<Utc>>,
    pub max_wind_kt: Option<f32>,
}

/// A storm's track/cone geometry (from GDACS `getgeometry`, or NHC GIS later).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StormGeometry {
    pub centroid: Option<GeoPoint>,
    /// Track polylines (past + forecast). GDACS delivers the track as many
    /// short, independently-oriented segments, so these are kept SEPARATE (not
    /// concatenated) — flattening them into one polyline zigzags and draws
    /// spurious connecting lines. Each inner Vec is one drawable segment.
    pub track: Vec<Vec<GeoPoint>>,
    /// Cone-of-uncertainty outer ring.
    pub cone: Vec<GeoPoint>,
    /// Official forecast track points (position + valid time + per-point max
    /// wind, where the office provides it). This is the transport that carries
    /// the parsed forecast up to [`TropicalCyclone::forecast`]; see the parsers
    /// below for exactly what each source supplies.
    pub forecast: Vec<ForecastPoint>,
}

/// One active tropical cyclone, source-agnostic.
#[derive(Clone, Debug, PartialEq)]
pub struct TropicalCyclone {
    /// Stable id, e.g. `nhc:al012026` or `gdacs:1001279:17`.
    pub id: String,
    /// Storm name without any season suffix, e.g. `Alberto`, `Bavi`.
    pub name: String,
    pub basin: Basin,
    pub source: Source,
    /// Human label, e.g. "Category 4 Typhoon".
    pub classification: String,
    pub category: Option<Category>,
    pub position: GeoPoint,
    pub max_wind_kt: Option<f32>,
    pub gust_kt: Option<f32>,
    pub min_pressure_mb: Option<f32>,
    pub movement_dir_deg: Option<f32>,
    pub movement_speed_kt: Option<f32>,
    /// When the underlying advisory/analysis was issued.
    pub advisory_time: Option<DateTime<Utc>>,
    /// GDACS alert level ("Red"/"Orange"/"Green"), if any.
    pub alert_level: Option<String>,
    /// Land areas at risk (GDACS `country`), if any.
    pub affected_areas: Option<String>,
    pub forecast: Vec<ForecastPoint>,
    pub cone: Vec<GeoPoint>,
    /// A human report page to open externally (never scraped).
    pub report_url: Option<String>,
    /// The source URL to fetch this storm's track/cone geometry.
    pub geometry_url: Option<String>,
    /// For a JTWC-covered GDACS storm (West Pacific, Indian Ocean, Southern
    /// Hemisphere), the JTWC Tropical Cyclone Warning text URL carrying
    /// per-point forecast intensity (`wpNNyyweb.txt`). Set by matching the JTWC
    /// RSS feed to this storm's name; enriches the GDACS getgeometry track/cone
    /// with real per-point max wind. `None` when no active JTWC warning matches.
    pub forecast_url: Option<String>,
}

impl TropicalCyclone {
    pub fn max_wind_mph(&self) -> Option<f32> {
        self.max_wind_kt.map(|kt| kt / KT_PER_MPH)
    }

    pub fn max_wind_kmh(&self) -> Option<f32> {
        self.max_wind_kt.map(|kt| kt / KT_PER_KMH)
    }

    /// Max sustained wind across the units meteorologists read, e.g.
    /// "145 kt · 167 mph · 269 km/h". None when wind is unknown.
    pub fn wind_summary(&self) -> Option<String> {
        let kt = self.max_wind_kt?;
        Some(format!(
            "{:.0} kt · {:.0} mph · {:.0} km/h",
            kt,
            kt / KT_PER_MPH,
            kt / KT_PER_KMH
        ))
    }

    /// Minimum central pressure, e.g. "965 mb".
    pub fn pressure_summary(&self) -> Option<String> {
        self.min_pressure_mb.map(|mb| format!("{mb:.0} mb"))
    }

    /// Motion toward a heading, e.g. "NNW (340°) at 12 kt". None when either
    /// component is unknown.
    pub fn motion_summary(&self) -> Option<String> {
        let dir = self.movement_dir_deg?;
        let speed = self.movement_speed_kt?;
        Some(format!(
            "{} ({:.0}°) at {:.0} kt",
            compass_16(dir),
            dir,
            speed
        ))
    }
}

/// 16-point compass label for a bearing in degrees (0° = N, clockwise).
pub fn compass_16(deg: f32) -> &'static str {
    const POINTS: [&str; 16] = [
        "N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE", "S", "SSW", "SW", "WSW", "W", "WNW",
        "NW", "NNW",
    ];
    let idx = ((deg.rem_euclid(360.0) / 22.5).round() as usize) % 16;
    POINTS[idx]
}

pub const KT_PER_KMH: f32 = 0.539_957;
pub const KT_PER_MPH: f32 = 0.868_976;

fn normalize_lon(lon: f32) -> f32 {
    let mut l = lon % 360.0;
    if l > 180.0 {
        l -= 360.0;
    } else if l < -180.0 {
        l += 360.0;
    }
    l
}

/// Parse a timestamp that may be RFC 3339 (`...Z`) or a naive ISO string
/// (GDACS, implicitly UTC).
fn parse_time(raw: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
        return Some(dt.with_timezone(&Utc));
    }
    NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S")
        .ok()
        .map(|naive| naive.and_utc())
}

/// GDACS event names carry a season suffix ("BAVI-26"); NHC names are clean.
/// Return a title-cased bare name.
fn clean_storm_name(raw: &str) -> String {
    let bare = raw
        .trim()
        .trim_start_matches("Tropical Cyclone ")
        .split('-')
        .next()
        .unwrap_or(raw)
        .trim();
    title_case(bare)
}

fn title_case(s: &str) -> String {
    s.split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => {
                    first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// NHC CurrentStorms.json
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NhcCurrentStorms {
    #[serde(default)]
    active_storms: Vec<NhcStorm>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NhcStorm {
    id: String,
    name: String,
    #[serde(default)]
    classification: String,
    #[serde(default)]
    intensity: String,
    #[serde(default)]
    pressure: String,
    latitude_numeric: f32,
    longitude_numeric: f32,
    #[serde(default)]
    movement_dir: Option<f32>,
    #[serde(default)]
    movement_speed: Option<f32>,
    #[serde(default)]
    last_update: Option<String>,
    #[serde(default)]
    public_advisory: Option<NhcProduct>,
    /// The Tropical Cyclone Forecast/Advisory (TCM) product — the machine-
    /// readable forecast track WITH per-point max wind. Its `url` is a stable
    /// "latest" bulletin, e.g. `.../text/MIATCMAT1.shtml`.
    #[serde(default)]
    forecast_advisory: Option<NhcProduct>,
}

#[derive(Deserialize)]
struct NhcProduct {
    #[serde(default)]
    url: Option<String>,
}

/// NHC classification codes → whether the wind label should override.
fn nhc_basin(id: &str) -> Basin {
    match id.get(0..2) {
        Some("al") | Some("AL") => Basin::Atlantic,
        Some("ep") | Some("EP") => Basin::EastPacific,
        Some("cp") | Some("CP") => Basin::CentralPacific,
        _ => Basin::Atlantic,
    }
}

/// Parse NHC's `CurrentStorms.json` into unified records.
pub fn parse_nhc_current_storms(json: &str) -> Result<Vec<TropicalCyclone>, String> {
    let parsed: NhcCurrentStorms =
        serde_json::from_str(json).map_err(|err| format!("NHC parse: {err}"))?;
    Ok(parsed
        .active_storms
        .into_iter()
        .map(nhc_to_cyclone)
        .collect())
}

fn nhc_to_cyclone(storm: NhcStorm) -> TropicalCyclone {
    let basin = nhc_basin(&storm.id);
    let max_wind_kt = storm.intensity.trim().parse::<f32>().ok();
    let category = max_wind_kt.map(Category::from_wind_kt);
    let classification = category
        .map(|category| category.label(basin))
        .unwrap_or_else(|| nhc_classification_label(&storm.classification));
    TropicalCyclone {
        id: format!("nhc:{}", storm.id),
        name: title_case(storm.name.trim()),
        basin,
        source: Source::Nhc,
        classification,
        category,
        position: GeoPoint {
            lon: storm.longitude_numeric,
            lat: storm.latitude_numeric,
        },
        max_wind_kt,
        gust_kt: None,
        min_pressure_mb: storm.pressure.trim().parse::<f32>().ok(),
        movement_dir_deg: storm.movement_dir,
        movement_speed_kt: storm.movement_speed,
        advisory_time: storm.last_update.as_deref().and_then(parse_time),
        alert_level: None,
        affected_areas: None,
        forecast: Vec::new(),
        cone: Vec::new(),
        report_url: storm.public_advisory.and_then(|advisory| advisory.url),
        // The forecast-advisory (TCM) URL is fetched on the second pass and
        // parsed into `forecast` by `parse_nhc_forecast_advisory`.
        geometry_url: storm.forecast_advisory.and_then(|advisory| advisory.url),
        // NHC's own TCM already carries per-point intensity via geometry_url;
        // no separate JTWC enrichment needed for NHC basins.
        forecast_url: None,
    }
}

fn nhc_classification_label(code: &str) -> String {
    match code.trim() {
        "TD" => "Tropical Depression",
        "TS" => "Tropical Storm",
        "HU" => "Hurricane",
        "MH" => "Major Hurricane",
        "PTC" => "Potential Tropical Cyclone",
        "STD" => "Subtropical Depression",
        "STS" => "Subtropical Storm",
        other if !other.is_empty() => return other.to_owned(),
        _ => "Tropical Cyclone",
    }
    .to_owned()
}

// ---------------------------------------------------------------------------
// NHC Tropical Cyclone Forecast/Advisory (TCM) — per-point forecast + intensity
// ---------------------------------------------------------------------------

/// Parse the official forecast track from an NHC Tropical Cyclone
/// Forecast/Advisory (a.k.a. TCM / "FORECAST/ADVISORY", WMO header e.g.
/// `MIATCMAT4`). Each forecast point is a `FORECAST VALID`/`OUTLOOK VALID` line
/// (`DD/HHMMZ  LATn  LONw`) immediately followed by a `MAX WIND nnn KT` line, so
/// NHC carries per-point **valid time, position, and max sustained wind (kt)** —
/// exactly what the Saffir–Simpson color ramp needs.
///
/// This is NHC's machine-readable forecast product (linked from
/// `CurrentStorms.json` as `forecastAdvisory.url`); we parse the fixed columnar
/// bulletin text, never the human advisory web page. Product/format reference:
/// NHC "Tropical Cyclone Forecast/Advisory" description,
/// <https://www.nhc.noaa.gov/help/tcm.shtml>.
pub fn parse_nhc_forecast_advisory(text: &str) -> Vec<ForecastPoint> {
    let issued = nhc_issuance_date(text);
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    for (i, raw) in lines.iter().enumerate() {
        let line = raw.trim();
        let Some(rest) = line
            .strip_prefix("FORECAST VALID ")
            .or_else(|| line.strip_prefix("OUTLOOK VALID "))
        else {
            continue;
        };
        let Some((position, valid_time)) = parse_nhc_valid_line(rest, issued) else {
            continue;
        };
        // The `MAX WIND` line is the next non-empty line (look a couple ahead to
        // tolerate a stray blank line).
        let max_wind_kt = lines
            .iter()
            .skip(i + 1)
            .take(3)
            .find_map(|l| parse_nhc_max_wind(l.trim()));
        out.push(ForecastPoint {
            position,
            valid_time,
            max_wind_kt,
        });
    }
    out
}

/// Pull the issuance `(year, month, day)` from the TCM datestamp line, e.g.
/// `2100 UTC TUE OCT 08 2024`. The forecast lines carry only a day-of-month, so
/// this reference resolves the real month/year across month/year rollover.
fn nhc_issuance_date(text: &str) -> Option<(i32, u32, u32)> {
    for line in text.lines() {
        let toks: Vec<&str> = line.split_whitespace().collect();
        if toks.len() < 6 || toks[1] != "UTC" {
            continue;
        }
        let month = month_from_abbrev(toks[toks.len() - 3]);
        let day = toks[toks.len() - 2].parse::<u32>();
        let year = toks[toks.len() - 1].parse::<i32>();
        if let (Some(month), Ok(day), Ok(year)) = (month, day, year) {
            return Some((year, month, day));
        }
    }
    None
}

fn month_from_abbrev(abbrev: &str) -> Option<u32> {
    Some(match abbrev.to_ascii_uppercase().as_str() {
        "JAN" => 1,
        "FEB" => 2,
        "MAR" => 3,
        "APR" => 4,
        "MAY" => 5,
        "JUN" => 6,
        "JUL" => 7,
        "AUG" => 8,
        "SEP" => 9,
        "OCT" => 10,
        "NOV" => 11,
        "DEC" => 12,
        _ => return None,
    })
}

/// Parse `09/0600Z 23.8N  86.4W` (optionally `...INLAND` / `...OVER WATER`).
fn parse_nhc_valid_line(
    rest: &str,
    issued: Option<(i32, u32, u32)>,
) -> Option<(GeoPoint, Option<DateTime<Utc>>)> {
    let mut toks = rest.split_whitespace();
    let time_tok = toks.next()?;
    let lat_tok = toks.next()?;
    let lon_tok = toks.next()?;
    let lat = parse_signed_coord(lat_tok, 'N', 'S')?;
    // A trailing "...INLAND"/"...POST-TROP" rides on the longitude token.
    let lon_clean = lon_tok.split("...").next().unwrap_or(lon_tok);
    let lon = parse_signed_coord(lon_clean, 'E', 'W')?;
    let valid_time = parse_tcm_time(time_tok, issued);
    Some((GeoPoint { lon, lat }, valid_time))
}

/// `23.8N` -> +23.8, `86.4W` -> -86.4 (direction is the trailing letter).
fn parse_signed_coord(tok: &str, positive: char, negative: char) -> Option<f32> {
    let dir = tok.chars().last()?;
    let magnitude: f32 = tok[..tok.len() - dir.len_utf8()].parse().ok()?;
    if dir == positive {
        Some(magnitude)
    } else if dir == negative {
        Some(-magnitude)
    } else {
        None
    }
}

/// `09/0600Z` + the issuance `(year, month, day)` -> a UTC instant. TCM forecast
/// times only ever run FORWARD from issuance (out to day 5), so a day-of-month
/// below the issuance day means the track crossed into the next month/year.
fn parse_tcm_time(tok: &str, issued: Option<(i32, u32, u32)>) -> Option<DateTime<Utc>> {
    let stamp = tok.trim_end_matches('Z');
    let (day_s, hhmm_s) = stamp.split_once('/')?;
    resolve_forward_time(day_s.parse().ok()?, hhmm_s.parse().ok()?, issued)
}

/// Resolve a `(day-of-month, HHMM)` stamp against the issuance `(year, month,
/// day)`, given that official forecast valid times only ever run FORWARD from
/// issuance (out to ~day 5). A day-of-month below the issuance day therefore
/// means the track crossed into the next month (and year, at a Dec boundary).
/// Shared by the NHC TCM (`DD/HHMMZ`) and JTWC warning (`DDHHMMZ`) parsers.
fn resolve_forward_time(
    day: u32,
    hhmm: u32,
    issued: Option<(i32, u32, u32)>,
) -> Option<DateTime<Utc>> {
    let (year, month, issue_day) = issued?;
    let (mut y, mut m) = (year, month);
    if day < issue_day {
        if m == 12 {
            m = 1;
            y += 1;
        } else {
            m += 1;
        }
    }
    let date = NaiveDate::from_ymd_opt(y, m, day)?;
    let time = NaiveTime::from_hms_opt(hhmm / 100, hhmm % 100, 0)?;
    Some(date.and_time(time).and_utc())
}

/// `MAX WIND 145 KT...GUSTS 175 KT.` -> `145`.
fn parse_nhc_max_wind(line: &str) -> Option<f32> {
    line.strip_prefix("MAX WIND")?
        .split_whitespace()
        .next()?
        .parse::<f32>()
        .ok()
}

// ---------------------------------------------------------------------------
// JTWC — RSS discovery + Tropical Cyclone Warning (per-point forecast + wind)
// ---------------------------------------------------------------------------

/// The JTWC public RSS feed listing active Tropical Cyclone Warnings and the
/// URLs of their text/graphic products (keyless, no auth). See
/// <https://www.metoc.navy.mil/jtwc/jtwc.html>.
pub const JTWC_RSS_URL: &str = "https://www.metoc.navy.mil/jtwc/rss/jtwc.rss?tc";

/// One active JTWC warning discovered from the RSS feed: its designation
/// (e.g. `09W`), storm name (e.g. `Bavi`), and the Tropical Cyclone Warning
/// text URL (`wpNNyyweb.txt`) carrying the per-point forecast + intensity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JtwcWarningRef {
    pub designation: String,
    pub name: String,
    pub warning_url: String,
}

/// Parse the JTWC RSS feed into the list of active Tropical Cyclone Warnings.
///
/// Each active storm appears as a bold header `<... NNX (Name) Warning #NN ...>`
/// followed by a `TC Warning Text` link to its `{basin}{NN}{yy}web.txt` product.
/// We pair each storm-warning URL with the storm header that immediately
/// precedes it. Non-storm `web.txt` products (the basin-wide "Significant
/// Tropical Weather Advisory" outlooks `abpwweb.txt`/`abioweb.txt`, which have
/// no storm number) are rejected by [`is_jtwc_warning_url`].
pub fn parse_jtwc_rss(xml: &str) -> Vec<JtwcWarningRef> {
    let mut out: Vec<JtwcWarningRef> = Vec::new();
    let needle = "web.txt";
    let mut cursor = 0;
    while let Some(rel) = xml[cursor..].find(needle) {
        let end = cursor + rel + needle.len();
        cursor = end;
        // The URL runs from the last `http` before the match to the end of
        // `web.txt` (RSS hrefs are absolute).
        let Some(start) = xml[..end].rfind("http") else {
            continue;
        };
        let url = &xml[start..end];
        if !is_jtwc_warning_url(url) {
            continue;
        }
        if let Some((designation, name)) = last_jtwc_designation(&xml[..start]) {
            out.push(JtwcWarningRef {
                designation,
                name,
                warning_url: url.to_owned(),
            });
        }
    }
    out
}

/// A storm-warning product URL ends in `{2 letters}{2-digit storm}{2-digit
/// year}web.txt` (e.g. `wp0926web.txt`). The basin-wide outlook products
/// (`abpwweb.txt`, `abioweb.txt`) have no numeric storm/year and are excluded.
fn is_jtwc_warning_url(url: &str) -> bool {
    let file = url.rsplit('/').next().unwrap_or(url);
    let Some(stem) = file.strip_suffix("web.txt") else {
        return false;
    };
    let bytes = stem.as_bytes();
    bytes.len() == 6
        && bytes[..2].iter().all(u8::is_ascii_alphabetic)
        && bytes[2..].iter().all(u8::is_ascii_digit)
}

/// The last `NNX (Name)` designation+name in `before` (the text preceding a
/// warning URL) — the header of the storm that URL belongs to. `NNX` is two
/// digits and one or more letters (`09W`, `01B`, `02S`); the name is in the
/// following parentheses. Rejects non-storm parentheticals like `(JTWC CDO)`
/// or `(Western/South Pacific Ocean)`.
fn last_jtwc_designation(before: &str) -> Option<(String, String)> {
    let mut found = None;
    for (i, _) in before.match_indices('(') {
        let after = &before[i + 1..];
        let Some(close) = after.find(')') else {
            continue;
        };
        let name = after[..close].trim();
        if name.is_empty() || name.len() > 20 || !name.chars().all(|c| c.is_alphanumeric()) {
            continue;
        }
        let designation = before[..i]
            .trim_end()
            .rsplit(char::is_whitespace)
            .next()
            .unwrap_or_default();
        if is_jtwc_designation(designation) {
            found = Some((designation.to_owned(), title_case(name)));
        }
    }
    found
}

/// `09W`, `01B`, `02S` — two ASCII digits followed by one or more letters.
fn is_jtwc_designation(tok: &str) -> bool {
    let bytes = tok.as_bytes();
    bytes.len() >= 3
        && bytes[..2].iter().all(u8::is_ascii_digit)
        && bytes[2..].iter().all(u8::is_ascii_uppercase)
}

/// Set `forecast_url` on each GDACS storm whose name matches an active JTWC
/// warning, so the geometry pipeline can enrich it with per-point intensity.
/// (NHC storms already carry per-point wind in their own TCM.)
pub fn attach_jtwc_forecast_urls(storms: &mut [TropicalCyclone], refs: &[JtwcWarningRef]) {
    for storm in storms.iter_mut() {
        if storm.source != Source::Gdacs {
            continue;
        }
        if let Some(matched) = refs
            .iter()
            .find(|r| r.name.eq_ignore_ascii_case(&storm.name) && !storm.name.is_empty())
        {
            storm.forecast_url = Some(matched.warning_url.clone());
        }
    }
}

/// Parse the official forecast track from a JTWC Tropical Cyclone Warning
/// (`wpNNyyweb.txt`) — the West-Pacific/Indian-Ocean analogue of NHC's TCM.
/// Under `FORECASTS:` (and its `EXTENDED`/`LONG RANGE OUTLOOK` continuations)
/// each point is a `NN HRS, VALID AT:` header, then a `DDHHMMZ --- LATn LONe`
/// position line, then a `MAX SUSTAINED WINDS - nnn KT` line — carrying
/// per-point **valid time, position, and max sustained wind (kt)**, exactly
/// what the Saffir–Simpson color ramp needs. The current `WARNING POSITION`
/// (analysis) point is intentionally excluded (only forecast points are
/// returned). Format reference: JTWC product descriptions,
/// <https://www.metoc.navy.mil/jtwc/jtwc.html>.
pub fn parse_jtwc_forecast_warning(text: &str) -> Vec<ForecastPoint> {
    let issued = jtwc_issuance_date(text);
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    for (i, raw) in lines.iter().enumerate() {
        // A forecast block starts with e.g. "12 HRS, VALID AT:".
        if !raw.trim().ends_with("HRS, VALID AT:") {
            continue;
        }
        // Position line is the next line that parses as `DDHHMMZ --- lat lon`.
        let Some((position, valid_time)) = lines
            .iter()
            .skip(i + 1)
            .take(3)
            .find_map(|l| parse_jtwc_valid_line(l.trim(), issued))
        else {
            continue;
        };
        // Intensity is the first `MAX SUSTAINED WINDS` line of this block.
        let max_wind_kt = lines
            .iter()
            .skip(i + 1)
            .take(6)
            .find_map(|l| parse_jtwc_max_wind(l.trim()));
        out.push(ForecastPoint {
            position,
            valid_time,
            max_wind_kt,
        });
    }
    out
}

/// Pull the issuance `(year, month, day)` from a JTWC warning's `DDMONYY`
/// datestamp (e.g. `06JUL26` in the REMARKS block). The forecast lines carry
/// only a day-of-month, so this reference resolves month/year across rollover.
fn jtwc_issuance_date(text: &str) -> Option<(i32, u32, u32)> {
    for tok in text.split_whitespace() {
        let t = tok.trim_matches(|c: char| !c.is_ascii_alphanumeric());
        if t.len() != 7 || !t.is_ascii() {
            continue;
        }
        let day = t[0..2].parse::<u32>().ok();
        let month = month_from_abbrev(&t[2..5]);
        let yy = t[5..7].parse::<i32>().ok();
        if let (Some(day), Some(month), Some(yy)) = (day, month, yy) {
            return Some((2000 + yy, month, day));
        }
    }
    None
}

/// Parse a JTWC forecast position line: `061200Z --- 15.1N 142.5E` (the
/// `WARNING POSITION` variant `060000Z --- NEAR 14.3N 145.0E` is also handled).
/// West-Pacific longitudes are E (positive).
fn parse_jtwc_valid_line(
    line: &str,
    issued: Option<(i32, u32, u32)>,
) -> Option<(GeoPoint, Option<DateTime<Utc>>)> {
    let mut toks = line.split_whitespace();
    let time_tok = toks.next()?;
    if !time_tok.ends_with('Z') {
        return None;
    }
    // Skip the `---` separator and an optional `NEAR`.
    let lat_tok = toks.find(|t| *t != "---" && *t != "NEAR")?;
    let lon_tok = toks.next()?;
    let lat = parse_signed_coord(lat_tok, 'N', 'S')?;
    let lon = parse_signed_coord(lon_tok, 'E', 'W')?;
    Some((GeoPoint { lon, lat }, parse_jtwc_time(time_tok, issued)))
}

/// `061200Z` (DDHHMM) + issuance `(year, month, day)` -> a UTC instant.
fn parse_jtwc_time(tok: &str, issued: Option<(i32, u32, u32)>) -> Option<DateTime<Utc>> {
    let stamp = tok.trim_end_matches('Z');
    if stamp.len() != 6 || !stamp.is_ascii() {
        return None;
    }
    let day: u32 = stamp[0..2].parse().ok()?;
    let hhmm: u32 = stamp[2..6].parse().ok()?;
    resolve_forward_time(day, hhmm, issued)
}

/// `MAX SUSTAINED WINDS - 145 KT, GUSTS 175 KT` -> `145`.
fn parse_jtwc_max_wind(line: &str) -> Option<f32> {
    let rest = line.strip_prefix("MAX SUSTAINED WINDS")?;
    rest.split(|c: char| !c.is_ascii_digit())
        .find(|s| !s.is_empty())?
        .parse::<f32>()
        .ok()
}

// ---------------------------------------------------------------------------
// GDACS event list + geometry
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct GdacsCollection {
    #[serde(default)]
    features: Vec<GdacsFeature>,
}

#[derive(Deserialize)]
struct GdacsFeature {
    #[serde(default)]
    geometry: Option<serde_json::Value>,
    properties: GdacsProps,
}

#[derive(Deserialize)]
struct GdacsProps {
    #[serde(default)]
    eventtype: String,
    #[serde(default)]
    eventid: Option<i64>,
    #[serde(default)]
    episodeid: Option<i64>,
    #[serde(default)]
    eventname: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    alertlevel: Option<String>,
    #[serde(default)]
    fromdate: Option<String>,
    #[serde(default)]
    datemodified: Option<String>,
    #[serde(default)]
    country: Option<String>,
    #[serde(default)]
    severitydata: Option<GdacsSeverity>,
    #[serde(default)]
    url: Option<GdacsUrls>,
    #[serde(default, rename = "Class")]
    class: Option<String>,
    /// On `getgeometry` forecast points: `MMDDHHMM` valid-time stamp (no year).
    #[serde(default)]
    key: Option<String>,
    /// On `getgeometry` forecast points: the analysis ("current") time —
    /// identical across a storm's points, so it is the past/forecast pivot.
    #[serde(default)]
    todate: Option<String>,
}

#[derive(Deserialize)]
struct GdacsSeverity {
    #[serde(default)]
    severity: Option<f32>,
    #[serde(default)]
    severityunit: Option<String>,
}

#[derive(Deserialize)]
struct GdacsUrls {
    #[serde(default)]
    geometry: Option<String>,
    #[serde(default)]
    report: Option<String>,
}

/// Parse GDACS `EVENTS4APP` (or `SEARCH`) output, keeping only tropical
/// cyclones.
pub fn parse_gdacs_event_list(json: &str) -> Result<Vec<TropicalCyclone>, String> {
    let parsed: GdacsCollection =
        serde_json::from_str(json).map_err(|err| format!("GDACS parse: {err}"))?;
    Ok(parsed
        .features
        .into_iter()
        .filter(|feature| feature.properties.eventtype.eq_ignore_ascii_case("TC"))
        .filter_map(gdacs_to_cyclone)
        .collect())
}

fn gdacs_to_cyclone(feature: GdacsFeature) -> Option<TropicalCyclone> {
    let position = feature.geometry.as_ref().and_then(geojson_point)?;
    let props = feature.properties;

    // GDACS severity for TC is the max sustained wind, defaulting to km/h.
    let max_wind_kt = props.severitydata.as_ref().and_then(|severity| {
        let value = severity.severity?;
        let unit = severity.severityunit.as_deref().unwrap_or("km/h");
        Some(match unit {
            u if u.eq_ignore_ascii_case("mph") => value * KT_PER_MPH,
            u if u.eq_ignore_ascii_case("kt") || u.eq_ignore_ascii_case("kn") => value,
            _ => value * KT_PER_KMH,
        })
    });
    let basin = Basin::from_lon_lat(position.lon, position.lat);
    let category = max_wind_kt.map(Category::from_wind_kt);
    let name_raw = props
        .eventname
        .or(props.name.clone())
        .unwrap_or_else(|| "Unnamed".to_owned());
    let classification = category
        .map(|category| category.label(basin))
        .unwrap_or_else(|| "Tropical Cyclone".to_owned());

    let (eventid, episodeid) = (props.eventid?, props.episodeid.unwrap_or(0));
    let urls = props.url.unwrap_or(GdacsUrls {
        geometry: None,
        report: None,
    });

    Some(TropicalCyclone {
        id: format!("gdacs:{eventid}:{episodeid}"),
        name: clean_storm_name(&name_raw),
        basin,
        source: Source::Gdacs,
        classification,
        category,
        position,
        max_wind_kt,
        gust_kt: None,
        min_pressure_mb: None,
        movement_dir_deg: None,
        movement_speed_kt: None,
        advisory_time: props
            .datemodified
            .as_deref()
            .or(props.fromdate.as_deref())
            .and_then(parse_time),
        alert_level: props.alertlevel,
        affected_areas: props.country.filter(|country| !country.is_empty()),
        forecast: Vec::new(),
        cone: Vec::new(),
        report_url: urls.report,
        geometry_url: urls.geometry,
        // Filled later by matching the JTWC RSS feed (see
        // `attach_jtwc_forecast_urls`) when an official JTWC warning is active
        // for this storm.
        forecast_url: None,
    })
}

/// Parse a GDACS `getgeometry` FeatureCollection into a storm's track, cone, and
/// forecast points. Track segments are `Line_Line_<n>`; the cone is
/// `Poly_Cones`; the current center is `Point_Centroid`.
///
/// The forecast track is delivered as `Point_Polygon_Point_<n>` features — one
/// per 6/12-hourly track point (past AND future), each a small wind-radii circle
/// whose center is the track position, with a `key` (`MMDDHHMM`) valid-time
/// stamp and a `todate` analysis time. GDACS repeats only the storm's *current*
/// severity on every point (not a per-point forecast), so forecast points get
/// `max_wind_kt = None` and are colored by the storm's current category — unless
/// [`fetch_storm_geometry`] later replaces them with the JTWC warning's honest
/// per-point intensity. We keep the points strictly AFTER the analysis time (the
/// forecast; earlier points are the observed past already drawn as `Line_Line`
/// segments).
pub fn parse_gdacs_geometry(json: &str) -> Result<StormGeometry, String> {
    let parsed: GdacsCollection =
        serde_json::from_str(json).map_err(|err| format!("GDACS geometry parse: {err}"))?;

    let mut centroid = None;
    let mut cone = Vec::new();
    let mut lines: Vec<(u32, Vec<GeoPoint>)> = Vec::new();
    // (index, center, MMDDHHMM key); the year comes from `reference` below.
    let mut point_stamps: Vec<(u32, GeoPoint, String)> = Vec::new();
    let mut reference: Option<DateTime<Utc>> = None;

    for feature in &parsed.features {
        let Some(class) = feature.properties.class.as_deref() else {
            continue;
        };
        let Some(geometry) = feature.geometry.as_ref() else {
            continue;
        };
        if class == "Point_Centroid" {
            centroid = geojson_point(geometry);
        } else if class == "Poly_Cones" {
            cone = geojson_polygon_outer(geometry);
        } else if let Some(index) = class.strip_prefix("Line_Line_")
            && let Ok(index) = index.parse::<u32>()
        {
            lines.push((index, geojson_line(geometry)));
        } else if let Some(index) = class.strip_prefix("Point_Polygon_Point_")
            && let Ok(index) = index.parse::<u32>()
            && let Some(center) = geojson_polygon_centroid(geometry)
            && let Some(key) = feature.properties.key.as_deref()
        {
            if reference.is_none() {
                reference = feature.properties.todate.as_deref().and_then(parse_time);
            }
            point_stamps.push((index, center, key.to_owned()));
        }
    }

    lines.sort_by_key(|(index, _)| *index);
    // Keep each GDACS segment as its own polyline (see StormGeometry::track).
    let track = lines
        .into_iter()
        .map(|(_, points)| points)
        .filter(|points| points.len() >= 2)
        .collect();

    let forecast = gdacs_forecast_points(point_stamps, reference);

    Ok(StormGeometry {
        centroid,
        track,
        cone,
        forecast,
    })
}

/// Resolve `MMDDHHMM` stamps against the analysis time and keep the forecast
/// (strictly-future) points, in chronological order.
fn gdacs_forecast_points(
    mut point_stamps: Vec<(u32, GeoPoint, String)>,
    reference: Option<DateTime<Utc>>,
) -> Vec<ForecastPoint> {
    let Some(reference) = reference else {
        return Vec::new();
    };
    point_stamps.sort_by_key(|(index, _, _)| *index);
    point_stamps
        .into_iter()
        .filter_map(|(_, center, key)| {
            let valid_time = gdacs_key_time(&key, reference)?;
            (valid_time > reference).then_some(ForecastPoint {
                position: center,
                valid_time: Some(valid_time),
                max_wind_kt: None,
            })
        })
        .collect()
}

/// `MMDDHHMM` + the analysis time -> a UTC instant. The stamp carries no year;
/// every point sits within a few days of the analysis time, so we pick the year
/// (prev/this/next) that lands the point closest to it — correct on either side
/// of a New-Year boundary.
fn gdacs_key_time(key: &str, reference: DateTime<Utc>) -> Option<DateTime<Utc>> {
    if key.len() != 8 {
        return None;
    }
    let month: u32 = key.get(0..2)?.parse().ok()?;
    let day: u32 = key.get(2..4)?.parse().ok()?;
    let hour: u32 = key.get(4..6)?.parse().ok()?;
    let minute: u32 = key.get(6..8)?.parse().ok()?;
    let time = NaiveTime::from_hms_opt(hour, minute, 0)?;
    let base = reference.year();
    [base - 1, base, base + 1]
        .into_iter()
        .filter_map(|year| {
            let dt = NaiveDate::from_ymd_opt(year, month, day)?
                .and_time(time)
                .and_utc();
            Some(((dt - reference).num_seconds().abs(), dt))
        })
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, dt)| dt)
}

// ---- GeoJSON coordinate extraction (tolerant of lon/lat f64 nesting) -------

fn coord_pair(value: &serde_json::Value) -> Option<GeoPoint> {
    let array = value.as_array()?;
    let lon = array.first()?.as_f64()? as f32;
    let lat = array.get(1)?.as_f64()? as f32;
    Some(GeoPoint { lon, lat })
}

fn geojson_point(geometry: &serde_json::Value) -> Option<GeoPoint> {
    if geometry.get("type")?.as_str()? != "Point" {
        return None;
    }
    coord_pair(geometry.get("coordinates")?)
}

fn geojson_line(geometry: &serde_json::Value) -> Vec<GeoPoint> {
    geometry
        .get("coordinates")
        .and_then(|coords| coords.as_array())
        .map(|array| array.iter().filter_map(coord_pair).collect())
        .unwrap_or_default()
}

fn geojson_polygon_outer(geometry: &serde_json::Value) -> Vec<GeoPoint> {
    geometry
        .get("coordinates")
        .and_then(|coords| coords.as_array())
        .and_then(|rings| rings.first())
        .and_then(|ring| ring.as_array())
        .map(|array| array.iter().filter_map(coord_pair).collect())
        .unwrap_or_default()
}

/// The centroid of a polygon's outer ring (mean of its vertices). GDACS delivers
/// each forecast track point as a small wind-radii circle; its center is the
/// track position. The closing vertex duplicates the first, but on a many-vertex
/// ring that bias is far below plotting resolution.
fn geojson_polygon_centroid(geometry: &serde_json::Value) -> Option<GeoPoint> {
    let ring = geojson_polygon_outer(geometry);
    if ring.is_empty() {
        return None;
    }
    let count = ring.len() as f64;
    let (sum_lon, sum_lat) = ring.iter().fold((0.0_f64, 0.0_f64), |(lon, lat), point| {
        (lon + point.lon as f64, lat + point.lat as f64)
    });
    Some(GeoPoint {
        lon: (sum_lon / count) as f32,
        lat: (sum_lat / count) as f32,
    })
}

// ---------------------------------------------------------------------------
// Fetch + merge
// ---------------------------------------------------------------------------

pub const NHC_CURRENT_STORMS_URL: &str = "https://www.nhc.noaa.gov/CurrentStorms.json";
pub const GDACS_TC_LIST_URL: &str =
    "https://www.gdacs.org/gdacsapi/api/events/geteventlist/EVENTS4APP?eventtypes=TC";

/// Combine NHC and GDACS into one deduplicated list: NHC is authoritative for
/// the basins it covers (Atlantic, East/Central Pacific — it carries wind AND
/// pressure), so GDACS storms in those basins are dropped to avoid doubles;
/// GDACS supplies every other basin (West Pacific, Indian Ocean, Southern
/// Hemisphere). Kept pure so the merge is unit-tested without a network.
pub fn merge_sources(
    nhc: Vec<TropicalCyclone>,
    gdacs: Vec<TropicalCyclone>,
) -> Vec<TropicalCyclone> {
    let nhc_basins = |basin: Basin| {
        matches!(
            basin,
            Basin::Atlantic | Basin::EastPacific | Basin::CentralPacific
        )
    };
    let mut merged = nhc;
    merged.extend(gdacs.into_iter().filter(|storm| !nhc_basins(storm.basin)));
    // Strongest first — that is the ordering the storm list wants.
    merged.sort_by(|a, b| {
        b.max_wind_kt
            .unwrap_or(0.0)
            .total_cmp(&a.max_wind_kt.unwrap_or(0.0))
    });
    merged
}

/// Fetch and merge every active tropical cyclone worldwide. Resilient: if one
/// source fails, the other's storms are still returned; only a total failure
/// is an error. As a best-effort last step, active JTWC warnings are matched to
/// the GDACS storms so each carries a `forecast_url` for per-point intensity —
/// a JTWC outage silently leaves the honest GDACS-only fallback in place.
pub fn fetch_active_cyclones(
    client: &reqwest::blocking::Client,
) -> Result<Vec<TropicalCyclone>, String> {
    let nhc =
        fetch_text(client, NHC_CURRENT_STORMS_URL).and_then(|body| parse_nhc_current_storms(&body));
    let gdacs =
        fetch_text(client, GDACS_TC_LIST_URL).and_then(|body| parse_gdacs_event_list(&body));

    let mut storms = match (nhc, gdacs) {
        (Ok(nhc), Ok(gdacs)) => merge_sources(nhc, gdacs),
        // A partial failure must NOT masquerade as "no active cyclones". Trust a
        // single surviving source only when it actually reports storms; when it
        // is EMPTY we cannot distinguish "genuinely quiet" from "the source that
        // carried the storms is down" — GDACS is the only feed for the West
        // Pacific (e.g. the live BAVI-26 typhoon), while NHC covers just the
        // Atlantic/E-Pac. Surface the failure so the caller retries instead of
        // showing a false all-clear.
        (Ok(nhc), Err(_)) if !nhc.is_empty() => nhc,
        (Ok(_), Err(gdacs_err)) => {
            return Err(format!(
                "GDACS unavailable (NHC reports no Atlantic/E-Pac storms): {gdacs_err}"
            ));
        }
        (Err(_), Ok(gdacs)) if !gdacs.is_empty() => merge_sources(Vec::new(), gdacs),
        (Err(nhc_err), Ok(_)) => {
            return Err(format!(
                "NHC unavailable (GDACS reports no storms): {nhc_err}"
            ));
        }
        (Err(nhc_err), Err(gdacs_err)) => {
            return Err(format!(
                "both sources failed — NHC: {nhc_err}; GDACS: {gdacs_err}"
            ));
        }
    };
    // Best-effort: match active JTWC warnings to GDACS storms so each carries a
    // forecast_url for real per-point West-Pacific intensity (a JTWC outage
    // silently leaves the honest GDACS-only fallback in place).
    if let Ok(rss) = fetch_text(client, JTWC_RSS_URL) {
        attach_jtwc_forecast_urls(&mut storms, &parse_jtwc_rss(&rss));
    }
    Ok(storms)
}

/// Fetch one storm's forecast geometry from its `geometry_url`, parsed per
/// source: GDACS `getgeometry` yields track + cone + forecast points; the NHC
/// forecast-advisory (TCM) yields forecast points (with per-point wind) only.
///
/// `forecast_url` is the optional JTWC Tropical Cyclone Warning for a
/// GDACS-covered storm: when present and parseable, its per-point forecast
/// (position + valid time + **real max sustained wind**) REPLACES the
/// intensity-less GDACS forecast points, so the West-Pacific dots color by the
/// official JTWC per-point Saffir–Simpson category. The GDACS track and cone are
/// always kept. A failed/empty JTWC fetch leaves the GDACS fallback untouched.
pub fn fetch_storm_geometry(
    client: &reqwest::blocking::Client,
    source: Source,
    url: &str,
    forecast_url: Option<&str>,
) -> Result<StormGeometry, String> {
    let body = fetch_text(client, url)?;
    match source {
        Source::Gdacs => {
            let mut geometry = parse_gdacs_geometry(&body)?;
            if let Some(warning_url) = forecast_url
                && let Ok(warning) = fetch_text(client, warning_url)
            {
                let jtwc = parse_jtwc_forecast_warning(&warning);
                if !jtwc.is_empty() {
                    geometry.forecast = jtwc;
                }
            }
            Ok(geometry)
        }
        Source::Nhc => Ok(StormGeometry {
            forecast: parse_nhc_forecast_advisory(&body),
            ..StormGeometry::default()
        }),
    }
}

fn fetch_text(client: &reqwest::blocking::Client, url: &str) -> Result<String, String> {
    let response = client
        .get(url)
        .send()
        .map_err(|err| format!("GET {url}: {err}"))?;
    if !response.status().is_success() {
        return Err(format!("GET {url}: HTTP {}", response.status()));
    }
    response.text().map_err(|err| format!("body {url}: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NHC: &str = include_str!("../tests/fixtures/tropical/nhc_active_storms.json");
    const GDACS_LIST: &str = include_str!("../tests/fixtures/tropical/gdacs_tc_list.json");
    const GDACS_GEOM: &str = include_str!("../tests/fixtures/tropical/gdacs_bavi_geometry.json");
    // Real products captured live for the forecast-dot feature:
    //  - a trimmed BAVI-26 `getgeometry` carrying `Point_Polygon_Point_*`
    //    forecast points (real centers/keys/`todate`, minimal rings), and
    //  - Hurricane Milton's actual Forecast/Advisory (TCM) #15 (AL142024).
    const GDACS_FCST: &str =
        include_str!("../tests/fixtures/tropical/gdacs_bavi_forecast_geometry.json");
    const NHC_TCM: &str =
        include_str!("../tests/fixtures/tropical/nhc_milton_forecast_advisory.txt");
    // Real JTWC products captured live for the West-Pacific per-point intensity
    // feature: the active RSS feed and Super Typhoon 09W (BAVI) Warning #21.
    const JTWC_RSS: &str = include_str!("../tests/fixtures/tropical/jtwc_rss.xml");
    const JTWC_WARNING: &str = include_str!("../tests/fixtures/tropical/jtwc_bavi_warning.txt");

    #[test]
    fn nhc_parses_storm_vitals() {
        let storms = parse_nhc_current_storms(NHC).expect("parse");
        assert_eq!(storms.len(), 1);
        let s = &storms[0];
        assert_eq!(s.name, "Alberto");
        assert_eq!(s.id, "nhc:al012026");
        assert_eq!(s.basin, Basin::Atlantic);
        assert_eq!(s.source, Source::Nhc);
        assert_eq!(s.max_wind_kt, Some(85.0));
        assert_eq!(s.min_pressure_mb, Some(968.0));
        assert_eq!(s.movement_dir_deg, Some(340.0));
        assert_eq!(s.category, Some(Category::Two)); // 85 kt
        assert_eq!(s.classification, "Category 2 Hurricane");
        assert!(s.advisory_time.is_some());
        assert!(s.report_url.as_deref().unwrap().contains("nhc.noaa.gov"));
        assert!((s.position.lat - 24.5).abs() < 1e-3);
        assert!((s.position.lon + 88.9).abs() < 1e-3);
    }

    #[test]
    fn gdacs_parses_live_typhoon() {
        let storms = parse_gdacs_event_list(GDACS_LIST).expect("parse");
        assert_eq!(storms.len(), 2, "BAVI + MAYSAK");
        let bavi = storms
            .iter()
            .find(|s| s.name == "Bavi")
            .expect("BAVI present");
        assert_eq!(bavi.basin, Basin::WestPacific);
        assert_eq!(bavi.source, Source::Gdacs);
        assert_eq!(bavi.alert_level.as_deref(), Some("Red"));
        // 268.5 km/h -> ~145 kt -> Cat 5 -> Super Typhoon in the W Pacific.
        let wind = bavi.max_wind_kt.expect("wind");
        assert!((wind - 145.0).abs() < 2.0, "wind={wind}");
        assert_eq!(bavi.category, Some(Category::Five));
        assert_eq!(bavi.classification, "Super Typhoon");
        assert!((bavi.position.lon - 148.9).abs() < 1e-3);
        assert!((bavi.position.lat - 12.9).abs() < 1e-3);
        assert!(
            bavi.geometry_url
                .as_deref()
                .unwrap()
                .contains("getgeometry")
        );
        assert!(bavi.affected_areas.as_deref().unwrap().contains("Guam"));
    }

    #[test]
    fn gdacs_geometry_extracts_track_centroid_cone() {
        let geometry = parse_gdacs_geometry(GDACS_GEOM).expect("parse");
        let centroid = geometry.centroid.expect("centroid");
        assert!((centroid.lon - 148.9).abs() < 1.0);
        assert!(!geometry.track.is_empty(), "track points present");
        assert!(geometry.cone.len() >= 3, "cone is a polygon ring");
    }

    #[test]
    fn nhc_tcm_parses_forecast_points_with_intensity() {
        // Milton (AL142024) advisory 15, issued 2100 UTC TUE OCT 08 2024.
        let fc = parse_nhc_forecast_advisory(NHC_TCM);
        // 6 `FORECAST VALID` (to day 3) + 2 `OUTLOOK VALID` (days 4–5).
        assert_eq!(fc.len(), 8, "forecast + outlook points");

        // First point: FORECAST VALID 09/0600Z 23.8N 86.4W / MAX WIND 145 KT.
        let first = &fc[0];
        assert!((first.position.lat - 23.8).abs() < 1e-3);
        assert!(
            (first.position.lon + 86.4).abs() < 1e-3,
            "W longitude is negative"
        );
        assert_eq!(first.max_wind_kt, Some(145.0));
        assert_eq!(
            first.max_wind_kt.map(Category::from_wind_kt),
            Some(Category::Five)
        );
        let expect = NaiveDate::from_ymd_opt(2024, 10, 9)
            .unwrap()
            .and_hms_opt(6, 0, 0)
            .unwrap()
            .and_utc();
        assert_eq!(first.valid_time, Some(expect));

        // Per-point intensity really varies (the whole point of the feature):
        // 145 → 130 → 110 → 75 kt, i.e. Cat 5 → 4 → 3 → 1.
        assert_eq!(
            fc[1].max_wind_kt.map(Category::from_wind_kt),
            Some(Category::Four)
        );
        assert_eq!(
            fc[2].max_wind_kt.map(Category::from_wind_kt),
            Some(Category::Three)
        );
        assert_eq!(
            fc[3].max_wind_kt.map(Category::from_wind_kt),
            Some(Category::One)
        );
        assert_eq!(
            fc[5].max_wind_kt.map(Category::from_wind_kt),
            Some(Category::TropicalStorm)
        );

        // Valid times are strictly increasing.
        for pair in fc.windows(2) {
            assert!(pair[0].valid_time.unwrap() < pair[1].valid_time.unwrap());
        }
    }

    #[test]
    fn nhc_tcm_time_rolls_over_month_and_year() {
        // Synthetic edge check only: a late-December advisory whose forecast
        // days wrap into January of the next year (no live storm exercises it).
        let text = "\
0300 UTC WED DEC 31 2025
FORECAST VALID 31/1200Z 25.0N 70.0W
MAX WIND 60 KT...GUSTS 75 KT.
FORECAST VALID 02/0000Z 28.0N 68.0W
MAX WIND 50 KT...GUSTS 65 KT.
";
        let fc = parse_nhc_forecast_advisory(text);
        assert_eq!(fc.len(), 2);
        assert_eq!(
            fc[0].valid_time,
            NaiveDate::from_ymd_opt(2025, 12, 31)
                .unwrap()
                .and_hms_opt(12, 0, 0)
                .map(|dt| dt.and_utc())
        );
        assert_eq!(
            fc[1].valid_time,
            NaiveDate::from_ymd_opt(2026, 1, 2)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .map(|dt| dt.and_utc())
        );
    }

    #[test]
    fn jtwc_rss_lists_active_warning_and_rejects_outlooks() {
        let refs = parse_jtwc_rss(JTWC_RSS);
        // Exactly one active storm warning; the basin-wide ABPW/ABIO outlook
        // `web.txt` products (no storm number) must NOT be treated as warnings.
        assert_eq!(refs.len(), 1, "one active storm warning: {refs:?}");
        let r = &refs[0];
        assert_eq!(r.designation, "09W");
        assert_eq!(r.name, "Bavi");
        assert_eq!(
            r.warning_url,
            "https://www.metoc.navy.mil/jtwc/products/wp0926web.txt"
        );
        // The filename gate distinguishes storm warnings from basin outlooks.
        assert!(is_jtwc_warning_url(
            "https://www.metoc.navy.mil/jtwc/products/wp0926web.txt"
        ));
        assert!(!is_jtwc_warning_url(
            "https://www.metoc.navy.mil/jtwc/products/abpwweb.txt"
        ));
        assert!(!is_jtwc_warning_url(
            "https://www.metoc.navy.mil/jtwc/products/abioweb.txt"
        ));
    }

    #[test]
    fn jtwc_warning_parses_forecast_points_with_intensity() {
        let fc = parse_jtwc_forecast_warning(JTWC_WARNING);
        // 3 FORECASTS + 3 EXTENDED OUTLOOK + 2 LONG RANGE = 8 forecast points;
        // the current WARNING POSITION (analysis) point is excluded.
        assert_eq!(fc.len(), 8, "forecast + outlook points");

        // First point: 061200Z --- 15.1N 142.5E / MAX SUSTAINED WINDS 145 KT.
        let first = &fc[0];
        assert!((first.position.lat - 15.1).abs() < 1e-3);
        assert!(
            (first.position.lon - 142.5).abs() < 1e-3,
            "West-Pacific longitude is positive (E)"
        );
        assert_eq!(first.max_wind_kt, Some(145.0));
        assert_eq!(
            first.max_wind_kt.map(Category::from_wind_kt),
            Some(Category::Five)
        );
        // Issuance 06JUL26 → first valid time 2026-07-06 12:00Z.
        assert_eq!(
            first.valid_time,
            NaiveDate::from_ymd_opt(2026, 7, 6)
                .unwrap()
                .and_hms_opt(12, 0, 0)
                .map(|dt| dt.and_utc())
        );

        // Real per-point intensity: 145,145,150,145,140,135,125,110 kt, i.e.
        // Cat 5 holding then weakening through Cat 4 to Cat 3 by 120 h.
        let cats: Vec<_> = fc
            .iter()
            .map(|p| p.max_wind_kt.map(Category::from_wind_kt))
            .collect();
        assert_eq!(
            cats,
            vec![
                Some(Category::Five),
                Some(Category::Five),
                Some(Category::Five),
                Some(Category::Five),
                Some(Category::Five),
                Some(Category::Four),
                Some(Category::Four),
                Some(Category::Three),
            ]
        );
        // Every forecast point carries an honest per-point wind (the whole point).
        assert!(fc.iter().all(|p| p.max_wind_kt.is_some()));

        // Last point: 110000Z --- 25.9N 122.4E, five days out.
        let last = fc.last().unwrap();
        assert!((last.position.lat - 25.9).abs() < 1e-3);
        assert!((last.position.lon - 122.4).abs() < 1e-3);
        assert_eq!(last.max_wind_kt, Some(110.0));

        // Valid times strictly increasing.
        for pair in fc.windows(2) {
            assert!(pair[0].valid_time.unwrap() < pair[1].valid_time.unwrap());
        }
    }

    #[test]
    fn jtwc_forecast_url_attaches_to_matching_gdacs_storm() {
        // GDACS list carries BAVI + MAYSAK; RSS has an active warning for BAVI.
        let mut storms = parse_gdacs_event_list(GDACS_LIST).unwrap();
        let refs = parse_jtwc_rss(JTWC_RSS);
        attach_jtwc_forecast_urls(&mut storms, &refs);
        let bavi = storms.iter().find(|s| s.name == "Bavi").unwrap();
        assert_eq!(
            bavi.forecast_url.as_deref(),
            Some("https://www.metoc.navy.mil/jtwc/products/wp0926web.txt"),
            "BAVI matched to its JTWC warning by name"
        );
        // MAYSAK has no active JTWC warning in the feed → no forecast URL.
        let maysak = storms.iter().find(|s| s.name == "Maysak").unwrap();
        assert_eq!(maysak.forecast_url, None);
    }

    #[test]
    fn jtwc_intensity_replaces_intensityless_gdacs_forecast() {
        // Mirrors the `fetch_storm_geometry` enrichment: the GDACS getgeometry
        // forecast points carry NO honest wind; the matched JTWC warning
        // replaces them with real per-point intensity while GDACS keeps the
        // track/cone. This is what turns the West-Pacific dots from "current
        // category on every dot" into official per-point Saffir–Simpson colors.
        let mut geometry = parse_gdacs_geometry(GDACS_FCST).unwrap();
        assert!(
            geometry.forecast.iter().all(|p| p.max_wind_kt.is_none()),
            "GDACS alone gives no per-point wind"
        );
        let jtwc = parse_jtwc_forecast_warning(JTWC_WARNING);
        assert!(!jtwc.is_empty());
        geometry.forecast = jtwc;
        // The track/cone survive from GDACS; the forecast now colors per point.
        assert!(!geometry.track.is_empty() && geometry.cone.len() >= 3);
        assert!(geometry.forecast.iter().all(|p| p.max_wind_kt.is_some()));
        let categories: Vec<_> = geometry
            .forecast
            .iter()
            .filter_map(|p| p.max_wind_kt.map(Category::from_wind_kt))
            .collect();
        assert!(
            categories.iter().any(|c| *c != categories[0]),
            "per-point intensity spans multiple categories: {categories:?}"
        );
    }

    #[test]
    fn jtwc_warning_time_rolls_over_month() {
        // Synthetic edge check only: a warning issued 30 SEP whose long-range
        // forecast days wrap into October (no live storm exercises rollover).
        let text = "\
SUBJ/TYPHOON 20W (TEST) WARNING NR 001//
   FORECASTS:
   12 HRS, VALID AT:
   301200Z --- 20.0N 130.0E
   MAX SUSTAINED WINDS - 80 KT, GUSTS 100 KT
   120 HRS, VALID AT:
   050000Z --- 28.0N 128.0E
   MAX SUSTAINED WINDS - 45 KT, GUSTS 60 KT
REMARKS:
30SEP26. TYPHOON 20W (TEST).//
";
        let fc = parse_jtwc_forecast_warning(text);
        assert_eq!(fc.len(), 2);
        assert_eq!(
            fc[0].valid_time,
            NaiveDate::from_ymd_opt(2026, 9, 30)
                .unwrap()
                .and_hms_opt(12, 0, 0)
                .map(|dt| dt.and_utc())
        );
        assert_eq!(
            fc[1].valid_time,
            NaiveDate::from_ymd_opt(2026, 10, 5)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .map(|dt| dt.and_utc())
        );
    }

    #[test]
    fn gdacs_geometry_extracts_forecast_points() {
        let geom = parse_gdacs_geometry(GDACS_FCST).expect("parse");
        // Still yields the observed pieces.
        let centroid = geom.centroid.expect("centroid");
        assert!((centroid.lon - 145.0).abs() < 0.5 && (centroid.lat - 14.3).abs() < 0.5);
        assert!(!geom.track.is_empty());
        assert!(geom.cone.len() >= 3);

        // Analysis time is 2026-07-06T00:00Z; only strictly-later points are
        // forecast, so past points 18/19 and the current point 20 drop out and
        // 21/22/28 remain, in chronological order.
        assert_eq!(geom.forecast.len(), 3, "future-only");
        let f0 = &geom.forecast[0];
        assert!((f0.position.lon - 142.5).abs() < 1e-2);
        assert!((f0.position.lat - 15.1).abs() < 1e-2);
        assert_eq!(
            f0.valid_time,
            NaiveDate::from_ymd_opt(2026, 7, 6)
                .unwrap()
                .and_hms_opt(12, 0, 0)
                .map(|dt| dt.and_utc())
        );
        // GDACS repeats only the current severity, so there is no honest
        // per-point forecast wind — left None (dot inherits current category).
        assert!(geom.forecast.iter().all(|p| p.max_wind_kt.is_none()));

        let last = geom.forecast.last().unwrap();
        assert!((last.position.lon - 122.4).abs() < 1e-2);
        assert_eq!(
            last.valid_time,
            NaiveDate::from_ymd_opt(2026, 7, 11)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .map(|dt| dt.and_utc())
        );

        let reference = NaiveDate::from_ymd_opt(2026, 7, 6)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        assert!(
            geom.forecast
                .iter()
                .all(|p| p.valid_time.unwrap() > reference)
        );
    }

    #[test]
    fn empty_nhc_is_no_storms() {
        assert!(
            parse_nhc_current_storms(r#"{"activeStorms":[]}"#)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn merge_prefers_nhc_in_its_basins_and_sorts_by_wind() {
        let nhc = parse_nhc_current_storms(NHC).unwrap(); // Atlantic "Alberto"
        let gdacs = parse_gdacs_event_list(GDACS_LIST).unwrap(); // W Pacific BAVI + MAYSAK
        let merged = merge_sources(nhc, gdacs);
        // NHC Atlantic storm kept; both W-Pacific GDACS storms kept (different basin).
        assert_eq!(merged.len(), 3);
        // Strongest first: BAVI (~145 kt) leads.
        assert_eq!(merged[0].name, "Bavi");
        assert!(merged[0].max_wind_kt.unwrap() >= merged[1].max_wind_kt.unwrap());
        // No GDACS storm survived in an NHC basin.
        assert!(
            !merged
                .iter()
                .any(|s| s.source == Source::Gdacs && s.basin == Basin::Atlantic)
        );
    }

    #[test]
    fn display_helpers_format_vitals() {
        let bavi = parse_gdacs_event_list(GDACS_LIST)
            .unwrap()
            .into_iter()
            .find(|s| s.name == "Bavi")
            .unwrap();
        let wind = bavi.wind_summary().expect("wind");
        assert!(
            wind.contains("kt") && wind.contains("mph") && wind.contains("km/h"),
            "{wind}"
        );
        assert!(wind.starts_with("145 kt"), "{wind}");

        let alberto = parse_nhc_current_storms(NHC).unwrap().pop().unwrap();
        assert_eq!(alberto.pressure_summary().as_deref(), Some("968 mb"));
        assert_eq!(
            alberto.motion_summary().as_deref(),
            Some("NNW (340°) at 12 kt")
        );
    }

    #[test]
    fn compass_16_bins() {
        assert_eq!(compass_16(0.0), "N");
        assert_eq!(compass_16(45.0), "NE");
        assert_eq!(compass_16(315.0), "NW");
        assert_eq!(compass_16(340.0), "NNW");
        assert_eq!(compass_16(359.0), "N");
    }

    #[test]
    fn saffir_simpson_bins_and_basin_nouns() {
        assert_eq!(Category::from_wind_kt(30.0), Category::TropicalDepression);
        assert_eq!(Category::from_wind_kt(50.0), Category::TropicalStorm);
        assert_eq!(Category::from_wind_kt(140.0), Category::Five);
        assert_eq!(
            Category::Four.label(Basin::Atlantic),
            "Category 4 Hurricane"
        );
        assert_eq!(Category::Five.label(Basin::WestPacific), "Super Typhoon");
        assert_eq!(
            Category::Three.label(Basin::WestPacific),
            "Category 3 Typhoon"
        );
    }
}
