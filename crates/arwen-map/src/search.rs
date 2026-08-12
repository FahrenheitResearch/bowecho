// SPDX-License-Identifier: Apache-2.0
//
// Pattern copied with attribution from BowEcho crates/app_ui/src/main.rs
// (place_search_matches / parse_coordinate_search_query, ~line 42051) and
// map_paint.rs (place_search_context_for_lon_lat) @ 6dfcb9f.

//! Location search over the vendored basemap corpus: place names (US
//! places + towns, Canada/Mexico/Japan, world) with state/country context
//! and trailing-qualifier filters ("norman ok", "toronto canada"), plus
//! coordinate-pair parsing ("35.2N 97.4W", "35.2, -97.4 my label").

use crate::basemap_data::{self, BasemapLabel};
use crate::basemap_towns;
use crate::geo::{basemap_line_contains_lon_lat, bbox_contains, haversine_km, normalize_lon};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlaceSearchResult {
    pub name: &'static str,
    pub lon: f32,
    pub lat: f32,
    pub rank: u8,
    pub match_score: u8,
    pub distance_km: f32,
    /// US state abbreviation or country label for display context.
    pub context_label: Option<&'static str>,
}

impl PlaceSearchResult {
    /// "Norman, OK · 35.2N 97.4W"
    pub fn display_label(&self) -> String {
        let lat_suffix = if self.lat < 0.0 { "S" } else { "N" };
        let lon_suffix = if self.lon < 0.0 { "W" } else { "E" };
        let name = match self.context_label {
            Some(context) => format!("{}, {context}", self.name),
            None => self.name.to_string(),
        };
        format!(
            "{name} · {:.1}{lat_suffix} {:.1}{lon_suffix}",
            self.lat.abs(),
            self.lon.abs()
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CoordinateMarker {
    pub lat: f32,
    pub lon: f32,
    pub label: Option<String>,
}

struct SearchSource {
    labels: &'static [BasemapLabel],
    context_label: Option<&'static str>,
    source_rank: u8,
}

fn search_sources() -> [SearchSource; 9] {
    [
        SearchSource {
            labels: basemap_data::BASEMAP_US_PLACE_LABELS,
            context_label: None,
            source_rank: 0,
        },
        SearchSource {
            labels: basemap_towns::BASEMAP_US_TOWN_LABELS,
            context_label: None,
            source_rank: 1,
        },
        SearchSource {
            labels: basemap_data::BASEMAP_CANADA_PLACE_LABELS,
            context_label: Some("Canada"),
            source_rank: 2,
        },
        SearchSource {
            labels: basemap_data::BASEMAP_CANADA_ADMIN_LABELS,
            context_label: Some("Canada"),
            source_rank: 3,
        },
        SearchSource {
            labels: basemap_data::BASEMAP_MEXICO_PLACE_LABELS,
            context_label: Some("Mexico"),
            source_rank: 2,
        },
        SearchSource {
            labels: basemap_data::BASEMAP_MEXICO_ADMIN_LABELS,
            context_label: Some("Mexico"),
            source_rank: 3,
        },
        SearchSource {
            labels: basemap_data::BASEMAP_JAPAN_PLACE_LABELS,
            context_label: Some("Japan"),
            source_rank: 2,
        },
        SearchSource {
            labels: basemap_data::BASEMAP_JAPAN_ADMIN_LABELS,
            context_label: Some("Japan"),
            source_rank: 3,
        },
        SearchSource {
            labels: basemap_data::BASEMAP_WORLD_PLACE_LABELS,
            context_label: None,
            source_rank: 8,
        },
    ]
}

/// Name search: match-quality first, then distance from the map center.
pub fn place_search_matches(
    query: &str,
    center_lat: f32,
    center_lon: f32,
    limit: usize,
) -> Vec<PlaceSearchResult> {
    let (query, state_filter, country_filter) = parse_query_qualifiers(query);
    if query.len() < 2 || limit == 0 {
        return Vec::new();
    }
    let mut results: Vec<(PlaceSearchResult, u8)> = Vec::new();
    for source in search_sources() {
        if let Some(country) = country_filter
            && source.context_label != Some(country)
        {
            continue;
        }
        for label in source.labels {
            let Some(match_score) = place_name_match_score(label.name, &query) else {
                continue;
            };
            let state_abbr = source
                .context_label
                .is_none()
                .then(|| us_state_abbr_for_lon_lat(label.lat, label.lon))
                .flatten();
            if let Some(state_filter) = state_filter
                && state_abbr != Some(state_filter)
            {
                continue;
            }
            let context_label = state_abbr
                .or(source.context_label)
                .or_else(|| country_context_for_lon_lat(label.lat, label.lon));
            results.push((
                PlaceSearchResult {
                    name: label.name,
                    lon: label.lon,
                    lat: label.lat,
                    rank: label.rank,
                    match_score,
                    distance_km: haversine_km(center_lat, center_lon, label.lat, label.lon),
                    context_label,
                },
                source.source_rank,
            ));
        }
    }
    results.sort_by(|(a, a_source), (b, b_source)| {
        a.match_score
            .cmp(&b.match_score)
            .then_with(|| a.distance_km.total_cmp(&b.distance_km))
            .then(a.rank.cmp(&b.rank))
            .then(a_source.cmp(b_source))
            .then(a.name.cmp(b.name))
    });
    results.dedup_by(|(a, _), (b, _)| {
        a.name == b.name && haversine_km(a.lat, a.lon, b.lat, b.lon) < 8.0
    });
    results.truncate(limit);
    results.into_iter().map(|(result, _)| result).collect()
}

/// Coordinate parsing: `Ok(None)` when the query is not coordinate-shaped,
/// `Err` when it is but malformed.
pub fn parse_coordinate_search_query(query: &str) -> Result<Option<CoordinateMarker>, String> {
    let mut components: Vec<(f32, Option<Axis>)> = Vec::new();
    let mut label_pieces: Vec<&str> = Vec::new();
    for token in query.split(|ch: char| ch.is_whitespace() || ch == ',' || ch == ';') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        match parse_component(token) {
            Some(component) => components.push(component),
            None => label_pieces.push(token),
        }
    }
    match components.len() {
        0 | 1 => return Ok(None),
        2 => {}
        _ => return Err("Enter one latitude/longitude pair".to_owned()),
    }
    let (first, second) = (components[0], components[1]);
    let (lat, lon) = match (first.1, second.1) {
        (Some(Axis::Lat), Some(Axis::Lon)) | (Some(Axis::Lat), None) | (None, Some(Axis::Lon)) => {
            (first.0, second.0)
        }
        (Some(Axis::Lon), Some(Axis::Lat)) | (Some(Axis::Lon), None) | (None, Some(Axis::Lat)) => {
            (second.0, first.0)
        }
        (None, None) => {
            if valid_lat(first.0) && valid_lon(second.0) {
                (first.0, second.0)
            } else if valid_lon(first.0) && valid_lat(second.0) {
                (second.0, first.0)
            } else {
                return Err(
                    "Coordinates must be latitude -90..90 and longitude -180..180".to_owned(),
                );
            }
        }
        _ => return Err("Coordinates must be a latitude/longitude pair".to_owned()),
    };
    if !valid_lat(lat) || !valid_lon(lon) {
        return Err("Coordinates must be latitude -90..90 and longitude -180..180".to_owned());
    }
    let label = {
        let joined = label_pieces.join(" ");
        let trimmed = joined.trim_matches(|ch: char| {
            ch.is_whitespace() || matches!(ch, ',' | ';' | ':' | '@' | '=' | '(' | ')')
        });
        (!trimmed.is_empty()).then(|| trimmed.chars().take(48).collect::<String>())
    };
    Ok(Some(CoordinateMarker {
        lat,
        lon: normalize_lon(lon),
        label,
    }))
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Axis {
    Lat,
    Lon,
}

fn parse_component(token: &str) -> Option<(f32, Option<Axis>)> {
    let last = token.chars().next_back()?.to_ascii_uppercase();
    let (number, axis, negate) = match last {
        'N' => (&token[..token.len() - 1], Some(Axis::Lat), false),
        'S' => (&token[..token.len() - 1], Some(Axis::Lat), true),
        'E' => (&token[..token.len() - 1], Some(Axis::Lon), false),
        'W' => (&token[..token.len() - 1], Some(Axis::Lon), true),
        _ => (token, None, false),
    };
    let value = number.parse::<f32>().ok()?;
    if !value.is_finite() {
        return None;
    }
    Some((
        if negate {
            -value.abs()
        } else if axis.is_some() {
            value.abs()
        } else {
            value
        },
        axis,
    ))
}

fn valid_lat(value: f32) -> bool {
    value.is_finite() && (-90.0..=90.0).contains(&value)
}

fn valid_lon(value: f32) -> bool {
    value.is_finite() && (-180.0..=180.0).contains(&value)
}

fn place_name_match_score(name: &str, query_lower: &str) -> Option<u8> {
    let name_lower = name.to_ascii_lowercase();
    if name_lower == query_lower {
        Some(0)
    } else if name_lower.starts_with(query_lower) {
        Some(1)
    } else if name_lower
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|part| part.starts_with(query_lower))
    {
        Some(2)
    } else if name_lower.contains(query_lower) {
        Some(3)
    } else {
        None
    }
}

/// Trailing state/country qualifier ("norman ok" → ("norman", OK filter)).
fn parse_query_qualifiers(query: &str) -> (String, Option<&'static str>, Option<&'static str>) {
    let normalized = query
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    for (alias, abbr) in US_STATE_QUERY_ALIASES {
        let alias_tokens: Vec<&str> = alias.split_whitespace().collect();
        if normalized.len() > alias_tokens.len()
            && normalized[normalized.len() - alias_tokens.len()..] == alias_tokens[..]
        {
            return (
                normalized[..normalized.len() - alias_tokens.len()].join(" "),
                Some(*abbr),
                None,
            );
        }
    }
    for (alias, country) in COUNTRY_QUERY_ALIASES {
        if normalized.len() > 1 && normalized.last().map(String::as_str) == Some(*alias) {
            return (
                normalized[..normalized.len() - 1].join(" "),
                None,
                Some(*country),
            );
        }
    }
    (normalized.join(" "), None, None)
}

/// US state containing this point, by smallest containing state outline
/// (point-in-polygon over the vendored Census state lines).
pub fn us_state_abbr_for_lon_lat(lat: f32, lon: f32) -> Option<&'static str> {
    if lat >= 50.0 && (lon <= -130.0 || lon >= 170.0) {
        return Some("AK");
    }
    if (18.5..=23.0).contains(&lat) && (-161.0..=-154.0).contains(&lon) {
        return Some("HI");
    }
    let mut best_area = f32::INFINITY;
    let mut best_bbox = None;
    for line in basemap_data::BASEMAP_US_STATE_LINES {
        if !bbox_contains(line.bbox, lon, lat) || !basemap_line_contains_lon_lat(line, lon, lat) {
            continue;
        }
        let area = (line.bbox[2] - line.bbox[0]).abs() * (line.bbox[3] - line.bbox[1]).abs();
        if area < best_area {
            best_area = area;
            best_bbox = Some(line.bbox);
        }
    }
    best_bbox.map(us_state_abbr_for_bbox)
}

pub fn country_context_for_lon_lat(lat: f32, lon: f32) -> Option<&'static str> {
    if let Some(state) = us_state_abbr_for_lon_lat(lat, lon) {
        return Some(state);
    }
    if bbox_contains(basemap_data::BASEMAP_CANADA_BOUNDS, lon, lat) {
        Some("Canada")
    } else if bbox_contains(basemap_data::BASEMAP_MEXICO_BOUNDS, lon, lat) {
        Some("Mexico")
    } else if bbox_contains(basemap_data::BASEMAP_JAPAN_BOUNDS, lon, lat) {
        Some("Japan")
    } else {
        None
    }
}

fn us_state_abbr_for_bbox(bbox: [f32; 4]) -> &'static str {
    let lon = (bbox[0] + bbox[2]) * 0.5;
    let lat = (bbox[1] + bbox[3]) * 0.5;
    US_STATE_ANCHORS
        .iter()
        .min_by(|a, b| {
            haversine_km(lat, lon, a.1, a.0).total_cmp(&haversine_km(lat, lon, b.1, b.0))
        })
        .map(|anchor| anchor.2)
        .unwrap_or("US")
}

/// (lon, lat, abbr) interior anchors — nearest anchor to a state outline's
/// bbox center names the state (BowEcho convention).
#[rustfmt::skip]
const US_STATE_ANCHORS: &[(f32, f32, &str)] = &[
    (-86.8, 32.8, "AL"), (-152.0, 64.2, "AK"), (-111.7, 34.3, "AZ"), (-92.4, 34.9, "AR"),
    (-119.7, 36.6, "CA"), (-105.5, 39.0, "CO"), (-72.7, 41.6, "CT"), (-75.5, 39.0, "DE"),
    (-77.0, 38.9, "DC"), (-82.4, 28.6, "FL"), (-83.4, 32.7, "GA"), (-157.5, 20.9, "HI"),
    (-114.7, 44.1, "ID"), (-89.2, 40.0, "IL"), (-86.1, 39.9, "IN"), (-93.5, 42.1, "IA"),
    (-98.4, 38.5, "KS"), (-85.3, 37.5, "KY"), (-91.9, 30.9, "LA"), (-69.2, 45.3, "ME"),
    (-76.7, 39.0, "MD"), (-71.8, 42.3, "MA"), (-84.6, 44.3, "MI"), (-94.6, 46.3, "MN"),
    (-89.7, 32.7, "MS"), (-92.5, 38.5, "MO"), (-110.4, 46.9, "MT"), (-99.8, 41.5, "NE"),
    (-116.6, 39.3, "NV"), (-71.6, 43.7, "NH"), (-74.7, 40.1, "NJ"), (-106.1, 34.4, "NM"),
    (-75.5, 42.9, "NY"), (-79.4, 35.5, "NC"), (-100.5, 47.5, "ND"), (-82.8, 40.3, "OH"),
    (-97.5, 35.6, "OK"), (-120.6, 43.9, "OR"), (-77.8, 40.9, "PA"), (-71.5, 41.7, "RI"),
    (-80.9, 33.8, "SC"), (-100.2, 44.4, "SD"), (-86.4, 35.8, "TN"), (-99.3, 31.5, "TX"),
    (-111.7, 39.3, "UT"), (-72.7, 44.0, "VT"), (-78.7, 37.5, "VA"), (-120.4, 47.4, "WA"),
    (-80.6, 38.6, "WV"), (-89.8, 44.5, "WI"), (-107.6, 43.0, "WY"), (-66.5, 18.2, "PR"),
    (-64.8, 18.1, "VI"), (144.8, 13.4, "GU"), (145.6, 15.1, "MP"), (-170.7, -14.3, "AS"),
];

#[rustfmt::skip]
const US_STATE_QUERY_ALIASES: &[(&str, &str)] = &[
    ("al", "AL"), ("alabama", "AL"), ("ak", "AK"), ("alaska", "AK"),
    ("az", "AZ"), ("arizona", "AZ"), ("ar", "AR"), ("arkansas", "AR"),
    ("ca", "CA"), ("california", "CA"), ("co", "CO"), ("colorado", "CO"),
    ("ct", "CT"), ("connecticut", "CT"), ("de", "DE"), ("delaware", "DE"),
    ("dc", "DC"), ("district of columbia", "DC"), ("fl", "FL"), ("florida", "FL"),
    ("ga", "GA"), ("georgia", "GA"), ("hi", "HI"), ("hawaii", "HI"),
    ("id", "ID"), ("idaho", "ID"), ("il", "IL"), ("illinois", "IL"),
    ("in", "IN"), ("indiana", "IN"), ("ia", "IA"), ("iowa", "IA"),
    ("ks", "KS"), ("kansas", "KS"), ("ky", "KY"), ("kentucky", "KY"),
    ("la", "LA"), ("louisiana", "LA"), ("me", "ME"), ("maine", "ME"),
    ("md", "MD"), ("maryland", "MD"), ("ma", "MA"), ("massachusetts", "MA"),
    ("mi", "MI"), ("michigan", "MI"), ("mn", "MN"), ("minnesota", "MN"),
    ("ms", "MS"), ("mississippi", "MS"), ("mo", "MO"), ("missouri", "MO"),
    ("mt", "MT"), ("montana", "MT"), ("ne", "NE"), ("nebraska", "NE"),
    ("nv", "NV"), ("nevada", "NV"), ("nh", "NH"), ("new hampshire", "NH"),
    ("nj", "NJ"), ("new jersey", "NJ"), ("nm", "NM"), ("new mexico", "NM"),
    ("ny", "NY"), ("new york", "NY"), ("nc", "NC"), ("north carolina", "NC"),
    ("nd", "ND"), ("north dakota", "ND"), ("oh", "OH"), ("ohio", "OH"),
    ("ok", "OK"), ("oklahoma", "OK"), ("or", "OR"), ("oregon", "OR"),
    ("pa", "PA"), ("pennsylvania", "PA"), ("ri", "RI"), ("rhode island", "RI"),
    ("sc", "SC"), ("south carolina", "SC"), ("sd", "SD"), ("south dakota", "SD"),
    ("tn", "TN"), ("tennessee", "TN"), ("tx", "TX"), ("texas", "TX"),
    ("ut", "UT"), ("utah", "UT"), ("vt", "VT"), ("vermont", "VT"),
    ("va", "VA"), ("virginia", "VA"), ("wa", "WA"), ("washington", "WA"),
    ("wv", "WV"), ("west virginia", "WV"), ("wi", "WI"), ("wisconsin", "WI"),
    ("wy", "WY"), ("wyoming", "WY"), ("pr", "PR"), ("puerto rico", "PR"),
];

const COUNTRY_QUERY_ALIASES: &[(&str, &str)] = &[
    ("canada", "Canada"),
    ("mexico", "Mexico"),
    ("mx", "Mexico"),
    ("japan", "Japan"),
    ("jp", "Japan"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_city_match_wins_and_carries_state_context() {
        let results = place_search_matches("Oklahoma City", 35.0, -97.0, 5);
        assert!(!results.is_empty());
        assert_eq!(results[0].name, "Oklahoma City");
        assert_eq!(results[0].match_score, 0);
        assert_eq!(results[0].context_label, Some("OK"));
    }

    #[test]
    fn state_qualifier_filters_to_that_state() {
        let results = place_search_matches("norman ok", 35.0, -97.0, 5);
        assert!(!results.is_empty());
        assert!(
            results
                .iter()
                .all(|result| result.context_label == Some("OK")),
            "{results:?}"
        );
        assert!(results.iter().any(|result| result.name == "Norman"));
    }

    #[test]
    fn short_or_empty_queries_return_nothing() {
        assert!(place_search_matches("x", 35.0, -97.0, 5).is_empty());
        assert!(place_search_matches("", 35.0, -97.0, 5).is_empty());
    }

    #[test]
    fn coordinate_pairs_parse_in_every_supported_shape() {
        let marker = parse_coordinate_search_query("35.2N 97.4W")
            .unwrap()
            .unwrap();
        assert!((marker.lat - 35.2).abs() < 1e-6);
        assert!((marker.lon + 97.4).abs() < 1e-6);

        let marker = parse_coordinate_search_query("97.4W, 35.2N")
            .unwrap()
            .unwrap();
        assert!((marker.lat - 35.2).abs() < 1e-6);

        let marker = parse_coordinate_search_query("35.2, -97.4 storm target")
            .unwrap()
            .unwrap();
        assert!((marker.lon + 97.4).abs() < 1e-6);
        assert_eq!(marker.label.as_deref(), Some("storm target"));

        // A bare unambiguous pair where only one order is valid.
        let marker = parse_coordinate_search_query("-97.4 35.2")
            .unwrap()
            .unwrap();
        assert!((marker.lat - 35.2).abs() < 1e-6);
        assert!((marker.lon + 97.4).abs() < 1e-6);
    }

    #[test]
    fn non_coordinates_are_none_and_bad_pairs_are_errors() {
        assert_eq!(parse_coordinate_search_query("moore").unwrap(), None);
        assert!(parse_coordinate_search_query("95N 100W").is_err());
        assert!(parse_coordinate_search_query("1 2 3").is_err());
    }

    #[test]
    fn state_attribution_hits_known_points() {
        assert_eq!(us_state_abbr_for_lon_lat(35.47, -97.52), Some("OK"));
        assert_eq!(us_state_abbr_for_lon_lat(40.0, -83.0), Some("OH"));
        assert_eq!(us_state_abbr_for_lon_lat(64.0, -150.0), Some("AK"));
        assert_eq!(us_state_abbr_for_lon_lat(48.0, -3.0), None, "Atlantic");
    }
}
