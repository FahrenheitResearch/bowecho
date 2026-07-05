//! Tropical cyclone data — a unified model plus parsers for the two free,
//! keyless sources BowEcho aggregates:
//!
//! - **NHC** `CurrentStorms.json` — official for the Atlantic and East/Central
//!   Pacific; carries max wind (kt), min pressure (mb), and motion directly.
//! - **GDACS** `geteventlist/EVENTS4APP?eventtypes=TC` — a global aggregator
//!   (JTWC/JMA/etc.) that covers every other basin, including the West Pacific.
//!   Its `getgeometry` endpoint returns the track, cone, and impact polygons.
//!
//! The two feed a single [`TropicalCyclone`] so the UI never cares which
//! center issued a storm. Fields are intentionally comprehensive (wind, gusts,
//! pressure, motion, category) — each source fills what it has, and richer
//! intensity sources (ATCF, CIMSS ADT) can enrich the same record later.
//!
//! Scales/definitions: Saffir–Simpson by 1-min max sustained wind (kt); the
//! West Pacific "(Super) Typhoon" labels map onto the same wind thresholds.

use chrono::{DateTime, NaiveDateTime, Utc};
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
    /// Best-track + forecast track, concatenated in order.
    pub track: Vec<GeoPoint>,
    /// Cone-of-uncertainty outer ring.
    pub cone: Vec<GeoPoint>,
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
        geometry_url: None,
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
    })
}

/// Parse a GDACS `getgeometry` FeatureCollection into a storm's track + cone.
/// Track segments are `Line_Line_<n>` (concatenated in order); the cone is
/// `Poly_Cones`; the current center is `Point_Centroid`.
pub fn parse_gdacs_geometry(json: &str) -> Result<StormGeometry, String> {
    let parsed: GdacsCollection =
        serde_json::from_str(json).map_err(|err| format!("GDACS geometry parse: {err}"))?;

    let mut centroid = None;
    let mut cone = Vec::new();
    let mut lines: Vec<(u32, Vec<GeoPoint>)> = Vec::new();

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
        }
    }

    lines.sort_by_key(|(index, _)| *index);
    let track = lines.into_iter().flat_map(|(_, points)| points).collect();
    Ok(StormGeometry {
        centroid,
        track,
        cone,
    })
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
/// is an error.
pub fn fetch_active_cyclones(
    client: &reqwest::blocking::Client,
) -> Result<Vec<TropicalCyclone>, String> {
    let nhc =
        fetch_text(client, NHC_CURRENT_STORMS_URL).and_then(|body| parse_nhc_current_storms(&body));
    let gdacs =
        fetch_text(client, GDACS_TC_LIST_URL).and_then(|body| parse_gdacs_event_list(&body));

    match (nhc, gdacs) {
        (Ok(nhc), Ok(gdacs)) => Ok(merge_sources(nhc, gdacs)),
        (Ok(nhc), Err(_)) => Ok(nhc),
        (Err(_), Ok(gdacs)) => Ok(merge_sources(Vec::new(), gdacs)),
        (Err(nhc_err), Err(gdacs_err)) => Err(format!(
            "both sources failed — NHC: {nhc_err}; GDACS: {gdacs_err}"
        )),
    }
}

/// Fetch one storm's track/cone geometry from its `geometry_url` (GDACS).
pub fn fetch_storm_geometry(
    client: &reqwest::blocking::Client,
    url: &str,
) -> Result<StormGeometry, String> {
    parse_gdacs_geometry(&fetch_text(client, url)?)
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
