//! SPC layers: Day-1 convective outlooks + live filtered storm reports.
//!
//! Outlooks come from SPC's own GeoJSON (categorical + tornado/wind/hail
//! probabilistic), which carries its OWN fill/stroke styling per risk —
//! we draw exactly the colors SPC publishes. Polygons render as stroked
//! outlines with a translucent fill pass on the closed ring (outlook
//! rings are large; outline-first matches how radar workstations draw
//! them). Reports are the live filtered CSVs (the same parser family the
//! archive's tornado events use), drawn as age-aware markers.

use eframe::egui;
use std::time::Instant;

pub const OUTLOOK_KINDS: [(&str, &str); 4] = [
    ("cat", "Categorical"),
    ("torn", "Tornado %"),
    ("wind", "Wind %"),
    ("hail", "Hail %"),
];

pub struct OutlookFeature {
    pub label: String,
    #[allow(dead_code)] // long name for the hover card
    pub label2: String,
    pub fill: egui::Color32,
    pub stroke: egui::Color32,
    /// Outer rings, (lon, lat).
    pub rings: Vec<Vec<(f32, f32)>>,
}

#[derive(Default)]
pub struct SpcData {
    /// kind slug -> features (drawn in file order: SPC orders low->high risk).
    pub outlooks: Vec<(String, Vec<OutlookFeature>)>,
    pub reports: Vec<StormReport>,
    pub fetched_at: Option<Instant>,
}

#[allow(dead_code)] // time/magnitude/location/remark feed the hover card next
pub struct StormReport {
    pub kind: ReportKind,
    pub time_hhmm: String,
    pub lat: f32,
    pub lon: f32,
    pub magnitude: String,
    pub location: String,
    pub remark: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ReportKind {
    Tornado,
    Wind,
    Hail,
}

impl ReportKind {
    pub fn color(self) -> egui::Color32 {
        match self {
            ReportKind::Tornado => egui::Color32::from_rgb(235, 60, 60),
            ReportKind::Wind => egui::Color32::from_rgb(90, 140, 245),
            ReportKind::Hail => egui::Color32::from_rgb(80, 200, 100),
        }
    }
}

fn hex_color(value: &str, alpha: u8) -> egui::Color32 {
    let v = value.trim_start_matches('#');
    if v.len() != 6 {
        return egui::Color32::from_rgba_unmultiplied(128, 128, 128, alpha);
    }
    let p = |i: usize| u8::from_str_radix(&v[i..i + 2], 16).unwrap_or(128);
    egui::Color32::from_rgba_unmultiplied(p(0), p(2), p(4), alpha)
}

/// Parse one SPC outlook GeoJSON (Polygon/MultiPolygon features with
/// LABEL/LABEL2/fill/stroke properties). Holes are dropped — v1 renders
/// outlines plus a translucent fill; SPC donut-holes are rare and read
/// fine as nested outlines.
pub fn parse_outlook(text: &str) -> Vec<OutlookFeature> {
    let Ok(root) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let Some(features) = root.get("features").and_then(|f| f.as_array()) else {
        return out;
    };
    for feature in features {
        let props = &feature["properties"];
        let label = props["LABEL"].as_str().unwrap_or("").to_owned();
        let label2 = props["LABEL2"].as_str().unwrap_or("").to_owned();
        let fill = hex_color(props["fill"].as_str().unwrap_or(""), 36);
        let stroke = hex_color(props["stroke"].as_str().unwrap_or(""), 230);
        let geom = &feature["geometry"];
        let mut rings: Vec<Vec<(f32, f32)>> = Vec::new();
        let parse_ring = |ring: &serde_json::Value| -> Vec<(f32, f32)> {
            ring.as_array()
                .map(|points| {
                    points
                        .iter()
                        .filter_map(|p| {
                            let lon = p.get(0)?.as_f64()? as f32;
                            let lat = p.get(1)?.as_f64()? as f32;
                            Some((lon, lat))
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        match geom["type"].as_str() {
            Some("Polygon") => {
                if let Some(outer) = geom["coordinates"].get(0) {
                    rings.push(parse_ring(outer));
                }
            }
            Some("MultiPolygon") => {
                if let Some(polys) = geom["coordinates"].as_array() {
                    for poly in polys {
                        if let Some(outer) = poly.get(0) {
                            rings.push(parse_ring(outer));
                        }
                    }
                }
            }
            _ => {}
        }
        rings.retain(|r| r.len() >= 3);
        if !rings.is_empty() {
            out.push(OutlookFeature {
                label,
                label2,
                fill,
                stroke,
                rings,
            });
        }
    }
    out
}

/// Parse a filtered storm-report CSV (Time,Mag,Location,County,State,Lat,Lon,Comments).
pub fn parse_reports(kind: ReportKind, text: &str) -> Vec<StormReport> {
    let mut out = Vec::new();
    for line in text.lines().skip(1) {
        let cols: Vec<&str> = line.splitn(8, ',').collect();
        if cols.len() < 8 {
            continue;
        }
        let (Ok(lat), Ok(lon)) = (cols[5].trim().parse::<f32>(), cols[6].trim().parse::<f32>())
        else {
            continue;
        };
        if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
            continue;
        }
        out.push(StormReport {
            kind,
            time_hhmm: cols[0].trim().to_owned(),
            lat,
            lon,
            magnitude: cols[1].trim().to_owned(),
            location: format!("{}, {} {}", cols[2].trim(), cols[3].trim(), cols[4].trim()),
            remark: cols[7].trim().to_owned(),
        });
    }
    out
}

/// Blocking fetch of everything enabled — worker thread only.
/// `archive_date`: when viewing archive data, fetch THAT day's outlook
/// from SPC's archive (latest issuance found, walking 2000 -> 1630 ->
/// 1300 -> 1200 -> 0100); None = the live outlook. `day`: 1-3.
pub fn fetch_spc(
    outlook_kinds: &[&str],
    want_reports: bool,
    day: u8,
    archive_date: Option<(i32, u32, u32)>,
) -> SpcData {
    let mut data = SpcData {
        fetched_at: Some(Instant::now()),
        ..Default::default()
    };
    for kind in outlook_kinds {
        let text = match archive_date {
            None => data_source::fetch_text(&format!(
                "https://www.spc.noaa.gov/products/outlook/day{day}otlk_{kind}.lyr.geojson"
            ))
            .ok(),
            Some((y, m, d)) => ["2000", "1630", "1300", "1200", "0100"]
                .iter()
                .find_map(|issue| {
                    data_source::fetch_text(&format!(
                        "https://www.spc.noaa.gov/products/outlook/archive/{y}/day{day}otlk_{y}{m:02}{d:02}_{issue}_{kind}.lyr.geojson"
                    ))
                    .ok()
                }),
        };
        if let Some(text) = text {
            data.outlooks
                .push(((*kind).to_owned(), parse_outlook(&text)));
        }
    }
    if want_reports {
        for (slug, kind) in [
            ("torn", ReportKind::Tornado),
            ("wind", ReportKind::Wind),
            ("hail", ReportKind::Hail),
        ] {
            let url = format!("https://www.spc.noaa.gov/climo/reports/today_filtered_{slug}.csv");
            if let Ok(text) = data_source::fetch_text(&url) {
                data.reports.extend(parse_reports(kind, &text));
            }
        }
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_outlook_features() {
        let sample = r##"{"features":[{"properties":{"LABEL":"SLGT","LABEL2":"Slight Risk","fill":"#FFE066","stroke":"#DDAA00"},"geometry":{"type":"MultiPolygon","coordinates":[[[[-95.0,40.0],[-94.0,40.0],[-94.0,41.0],[-95.0,40.0]]]]}}]}"##;
        let parsed = parse_outlook(sample);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].label, "SLGT");
        assert_eq!(parsed[0].rings[0].len(), 4);
    }

    #[test]
    fn parses_report_rows() {
        let csv = "Time,Speed,Location,County,State,Lat,Lon,Comments\n1215,UNK,3 W Dallas Center,Dallas,IA,41.69,-94.02,Tree damage. (DMX)\n";
        let parsed = parse_reports(ReportKind::Wind, csv);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].time_hhmm, "1215");
        assert!(parsed[0].location.contains("Dallas"));
    }
}
