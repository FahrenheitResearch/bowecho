use chrono::{DateTime, Utc};
use eframe::egui;
use serde::Deserialize;

#[derive(Clone, Debug)]
pub struct MpingReport {
    pub id: String,
    pub lat: f32,
    pub lon: f32,
    pub obtime: DateTime<Utc>,
    pub category: String,
    pub description: String,
    pub description_id: u32,
}

impl MpingReport {
    pub fn hover_text(&self) -> String {
        format!(
            "mPING {}\n{} #{}\n{}\n{}",
            self.id,
            self.category,
            self.description_id,
            self.description,
            self.obtime.format("%Y-%m-%d %H:%M:%SZ")
        )
    }
}

#[derive(Deserialize)]
struct FeatureCollection {
    features: Vec<Feature>,
}

#[derive(Deserialize)]
struct Feature {
    id: serde_json::Value,
    geometry: Geometry,
    properties: Properties,
}

#[derive(Deserialize)]
struct Geometry {
    coordinates: Vec<f64>,
}

#[derive(Deserialize)]
struct Properties {
    obtime: String,
    category: String,
    description: String,
    description_id: u32,
}

pub fn fetch_reports() -> Result<Vec<MpingReport>, String> {
    let text = data_source::fetch_mping_reports_geojson().map_err(|err| err.to_string())?;
    parse_reports(&text)
}

pub fn parse_reports(text: &str) -> Result<Vec<MpingReport>, String> {
    let collection: FeatureCollection =
        serde_json::from_str(text).map_err(|err| err.to_string())?;
    let mut reports = collection
        .features
        .into_iter()
        .filter_map(report_from_feature)
        .collect::<Vec<_>>();
    reports.sort_by_key(|report| report.obtime);
    Ok(reports)
}

fn report_from_feature(feature: Feature) -> Option<MpingReport> {
    let lon = *feature.geometry.coordinates.first()? as f32;
    let lat = *feature.geometry.coordinates.get(1)? as f32;
    if !(-180.0..=180.0).contains(&lon) || !(-90.0..=90.0).contains(&lat) {
        return None;
    }
    let obtime = DateTime::parse_from_rfc3339(&feature.properties.obtime)
        .ok()?
        .with_timezone(&Utc);
    let id = match feature.id {
        serde_json::Value::String(value) => value,
        serde_json::Value::Number(value) => value.to_string(),
        _ => String::new(),
    };
    Some(MpingReport {
        id,
        lat,
        lon,
        obtime,
        category: feature.properties.category,
        description: feature.properties.description,
        description_id: feature.properties.description_id,
    })
}

pub fn report_color(report: &MpingReport) -> egui::Color32 {
    match report.category.as_str() {
        "Rain/Snow" => egui::Color32::from_rgb(70, 170, 255),
        "Hail" => egui::Color32::from_rgb(255, 225, 80),
        "Wind" | "Wind/Storm Damage" | "Storm Damage" => egui::Color32::from_rgb(255, 130, 65),
        "Flood" => egui::Color32::from_rgb(55, 210, 170),
        "Reduced Visibility" => egui::Color32::from_rgb(195, 165, 255),
        "Winter Weather Impacts" => egui::Color32::from_rgb(180, 235, 255),
        "None" => egui::Color32::from_rgb(150, 160, 170),
        _ => egui::Color32::from_rgb(235, 235, 235),
    }
}

pub fn report_glyph(report: &MpingReport) -> &'static str {
    match report.category.as_str() {
        "Hail" => "H",
        "Wind" | "Wind/Storm Damage" | "Storm Damage" => "W",
        "Flood" => "F",
        "Reduced Visibility" => "V",
        "Winter Weather Impacts" => "I",
        "Rain/Snow" => "P",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_public_geojson_shape() {
        let reports = parse_reports(
            r#"{
              "type": "FeatureCollection",
              "features": [
                {
                  "id": 4869449,
                  "type": "Feature",
                  "geometry": {"type": "Point", "coordinates": [-81.4933474, 41.2684925]},
                  "properties": {
                    "obtime": "2026-06-14T20:30:28Z",
                    "category": "Storm Damage",
                    "description": "Trees uprooted",
                    "description_id": 68
                  }
                }
              ]
            }"#,
        )
        .expect("parse mPING");
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].id, "4869449");
        assert_eq!(reports[0].category, "Storm Damage");
        assert_eq!(reports[0].description, "Trees uprooted");
        assert_eq!(reports[0].description_id, 68);
        assert!((reports[0].lat - 41.26849).abs() < 0.001);
        assert!((reports[0].lon + 81.49335).abs() < 0.001);
        assert!(reports[0].hover_text().contains("mPING 4869449"));
    }
}
