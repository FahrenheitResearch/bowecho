//! SPC layers: Day-1 convective outlooks + filtered storm reports (live
//! or any archived convective day) + tornado track segments.
//!
//! Outlooks come from SPC's own GeoJSON (categorical + tornado/wind/hail
//! probabilistic), which carries its OWN fill/stroke styling per risk —
//! we draw exactly the colors SPC publishes. Polygons render as stroked
//! outlines with a translucent fill pass on the closed ring (outlook
//! rings are large; outline-first matches how radar workstations draw
//! them). Features store the BASE colors; fill/stroke alphas come from
//! the style registry at draw time so style edits never refetch.
//! Reports are the live filtered CSVs (the same parser family the
//! archive's tornado events use), drawn as age-aware markers.
//!
//! DATED reports (the Event Explorer): SPC publishes one combined CSV per
//! CONVECTIVE day at spc.noaa.gov/climo/reports/YYMMDD_rpts_filtered.csv
//! (raw `_rpts.csv` for older days where no filtered file exists; nothing
//! before 2004). The SPC convention day runs 12Z -> 12Z next day, so a
//! 03Z report belongs to the PREVIOUS convective day's file
//! ([`spc_convective_date`]).
//!
//! Tornado TRACK segments (begin AND end coordinates) come from the SPC
//! WCM severe-weather database per-year files
//! (spc.noaa.gov/wcm/data/{yyyy}_torn.csv, "onetor" format; Schaefer &
//! Edwards 1999, 11th Conf. Applied Climatology — the same database
//! behind SPC's tornado climatology pages). The daily climo CSVs carry a
//! single point per report; the WCM database is where surveyed begin/end
//! paths live. The current year's file does not exist yet, so for recent
//! days the torn reports stand in as zero-length segments.

use chrono::{DateTime, Datelike, Duration, NaiveDate, Timelike, Utc};
use eframe::egui;
use std::time::Instant;

pub const OUTLOOK_KINDS: [(&str, &str); 8] = [
    ("cat", "Categorical"),
    ("torn", "Tornado %"),
    ("wind", "Wind %"),
    ("hail", "Hail %"),
    ("fire_wind", "Fire wind/RH"),
    ("fire_dryt", "Fire dry T"),
    ("wpc_ero", "WPC ERO"),
    ("wpc_river_flood", "WPC river flood"),
];
pub const DAY2_OUTLOOK_KINDS: [(&str, &str); 9] = [
    ("cat", "Categorical"),
    ("torn", "Tornado %"),
    ("wind", "Wind %"),
    ("hail", "Hail %"),
    ("prob", "Any Severe %"),
    ("fire_wind", "Fire wind/RH"),
    ("fire_dryt", "Fire dry T"),
    ("wpc_ero", "WPC ERO"),
    ("wpc_river_flood", "WPC river flood"),
];
pub const DAY3_OUTLOOK_KINDS: [(&str, &str); 6] = [
    ("cat", "Categorical"),
    ("prob", "Any Severe %"),
    ("fire_wind", "Fire wind/RH"),
    ("fire_dryt", "Fire dry T"),
    ("wpc_ero", "WPC ERO"),
    ("wpc_river_flood", "WPC river flood"),
];

pub const ESTOFEX_OUTLOOK_KIND: &str = "estofex";

/// Operator-selectable SPC Day-1 issuance.  SPC's 06Z Day-1 outlook is
/// archived under the product's 12Z valid-time slot (`_1200_`), while the
/// other archive slots match their issue times.  Keeping that distinction in
/// one typed value prevents the UI from presenting `_1200_` as a noon issue.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SpcDay1Issue {
    /// Existing behavior: current headline outlook for live data, or the
    /// newest available archive slot for a dated view.
    #[default]
    Auto,
    At0100,
    At0600,
    At1300,
    At1630,
    At2000,
}

pub const SPC_DAY1_FIXED_ISSUES: [SpcDay1Issue; 5] = [
    SpcDay1Issue::At0100,
    SpcDay1Issue::At0600,
    SpcDay1Issue::At1300,
    SpcDay1Issue::At1630,
    SpcDay1Issue::At2000,
];

const SPC_DAY1_AUTO_ARCHIVE_ORDER: [SpcDay1Issue; 5] = [
    SpcDay1Issue::At2000,
    SpcDay1Issue::At1630,
    SpcDay1Issue::At1300,
    SpcDay1Issue::At0600,
    SpcDay1Issue::At0100,
];

impl SpcDay1Issue {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto / latest",
            Self::At0100 => "01:00Z",
            Self::At0600 => "06:00Z",
            Self::At1300 => "13:00Z",
            Self::At1630 => "16:30Z",
            Self::At2000 => "20:00Z",
        }
    }

    /// Filename slot used by SPC's official archived GeoJSON.
    pub fn archive_slot(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::At0100 => Some("0100"),
            // The 06Z issuance becomes valid at 12Z and is archived as 1200.
            Self::At0600 => Some("1200"),
            Self::At1300 => Some("1300"),
            Self::At1630 => Some("1630"),
            Self::At2000 => Some("2000"),
        }
    }

    fn from_archive_slot(slot: &str) -> Option<Self> {
        match slot {
            "0100" => Some(Self::At0100),
            "1200" => Some(Self::At0600),
            "1300" => Some(Self::At1300),
            "1630" => Some(Self::At1630),
            "2000" => Some(Self::At2000),
            _ => None,
        }
    }

    pub fn scheduled_at(self, date: NaiveDate) -> Option<DateTime<Utc>> {
        let (hour, minute) = match self {
            Self::Auto => return None,
            Self::At0100 => (1, 0),
            Self::At0600 => (6, 0),
            Self::At1300 => (13, 0),
            Self::At1630 => (16, 30),
            Self::At2000 => (20, 0),
        };
        date.and_hms_opt(hour, minute, 0).map(|time| time.and_utc())
    }

    pub fn is_not_yet_issued(self, date: NaiveDate, now: DateTime<Utc>) -> bool {
        self.scheduled_at(date).is_some_and(|issue| issue > now)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpcDay1IssueStatus {
    AutoArchiveLoaded {
        date: NaiveDate,
        issue: SpcDay1Issue,
        loaded: usize,
        requested: usize,
    },
    AutoArchiveMissing {
        date: NaiveDate,
    },
    SelectedLoaded {
        date: NaiveDate,
        issue: SpcDay1Issue,
        loaded: usize,
        requested: usize,
    },
    SelectedNotYetIssued {
        date: NaiveDate,
        issue: SpcDay1Issue,
    },
    SelectedMissing {
        date: NaiveDate,
        issue: SpcDay1Issue,
    },
    NoStandardProductSelected,
}

/// Old builds exposed dedicated CIG switches even though the ordinary SPC
/// products already contain those conditional-intensity regions. Keep this
/// recognizer for settings migration while hiding/ignoring the duplicates.
pub fn is_legacy_cig_kind(kind: &str) -> bool {
    matches!(kind, "cigtorn" | "cigwind" | "cighail" | "cigprob")
}

pub fn outlook_kind_options(day: u8) -> &'static [(&'static str, &'static str)] {
    match day {
        2 => &DAY2_OUTLOOK_KINDS,
        3 => &DAY3_OUTLOOK_KINDS,
        _ => &OUTLOOK_KINDS,
    }
}

pub fn effective_spc_outlook_kinds(day: u8, requested: &[&str]) -> Vec<&'static str> {
    let mut out = Vec::new();
    for &kind in requested {
        let Some(kind) = effective_spc_outlook_kind(day, kind) else {
            continue;
        };
        if !out.contains(&kind) {
            out.push(kind);
        }
    }
    out
}

fn effective_spc_outlook_kind(day: u8, kind: &str) -> Option<&'static str> {
    match (day, kind) {
        (_, ESTOFEX_OUTLOOK_KIND) => None,
        (1, "cat") => Some("cat"),
        (1, "torn") => Some("torn"),
        (1, "wind") => Some("wind"),
        (1, "hail") => Some("hail"),
        (1, "fire_wind") => Some("fire_wind"),
        (1, "fire_dryt") => Some("fire_dryt"),
        (1, "wpc_ero") => Some("wpc_ero"),
        (1, "wpc_river_flood") => Some("wpc_river_flood"),
        (2, "cat") => Some("cat"),
        (2, "torn") => Some("torn"),
        (2, "wind") => Some("wind"),
        (2, "hail") => Some("hail"),
        (2, "prob") => Some("prob"),
        (2, "fire_wind") => Some("fire_wind"),
        (2, "fire_dryt") => Some("fire_dryt"),
        (2, "wpc_ero") => Some("wpc_ero"),
        (2, "wpc_river_flood") => Some("wpc_river_flood"),
        (3, "cat") => Some("cat"),
        (3, "prob" | "torn" | "wind" | "hail") => Some("prob"),
        (3, "fire_wind") => Some("fire_wind"),
        (3, "fire_dryt") => Some("fire_dryt"),
        (3, "wpc_ero") => Some("wpc_ero"),
        (3, "wpc_river_flood") => Some("wpc_river_flood"),
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct OutlookPolygon {
    pub outer: Vec<(f32, f32)>,
    pub holes: Vec<Vec<(f32, f32)>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OutlookFeature {
    pub label: String,
    #[allow(dead_code)] // long name for the hover card
    pub label2: String,
    /// Base colors as SPC publishes them (opaque); draw code applies the
    /// style registry's outlook alphas.
    pub fill: egui::Color32,
    pub stroke: egui::Color32,
    /// True when the ring is safe to fill as a polygon. SPC GeoJSON and
    /// ESTOFEX XML are closed. Raw PTS rings are closed during parsing when
    /// used as the fast live fallback.
    pub fill_enabled: bool,
    /// Outer rings, (lon, lat), kept for older SPC/PTX code paths and tests.
    pub rings: Vec<Vec<(f32, f32)>>,
    pub polygons: Vec<OutlookPolygon>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EstofexIssue {
    pub id: String,
    pub issued_at: DateTime<Utc>,
    pub valid_from: DateTime<Utc>,
    pub valid_to: DateTime<Utc>,
    pub polygons: Vec<OutlookFeature>,
}

#[derive(Default)]
pub struct SpcData {
    /// kind slug -> features (drawn in file order: SPC orders low->high risk).
    pub outlooks: Vec<(String, Vec<OutlookFeature>)>,
    pub estofex_issues: Vec<EstofexIssue>,
    pub reports: Vec<StormReport>,
    pub fetched_at: Option<Instant>,
    /// Live raw PTS says a newer outlook issue exists than the GeoJSON
    /// products we rendered. UI polling uses this to retry quickly while
    /// staying on official GeoJSON geometry.
    pub outlook_geojson_lagging: bool,
    /// Result of the Day-1 issuance choice, used to distinguish a future
    /// scheduled issue from an archive file that is genuinely missing.
    pub day1_issue_status: Option<SpcDay1IssueStatus>,
}

#[derive(Clone)]
#[allow(dead_code)] // time/magnitude/location/remark feed the hover card next
pub struct StormReport {
    pub kind: ReportKind,
    pub time_hhmm: String,
    /// Absolute report time (the convective-day file date plus the 12Z
    /// wrap: HHMM < 1200 is the NEXT calendar day).
    pub time_utc: DateTime<Utc>,
    pub lat: f32,
    pub lon: f32,
    pub magnitude: String,
    pub location: String,
    pub remark: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportKind {
    Tornado,
    Wind,
    Hail,
}

impl ReportKind {
    /// Style-registry key ("tornado" | "wind" | "hail").
    pub fn style_key(self) -> &'static str {
        match self {
            ReportKind::Tornado => "tornado",
            ReportKind::Wind => "wind",
            ReportKind::Hail => "hail",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ReportKind::Tornado => "TORNADO",
            ReportKind::Wind => "WIND",
            ReportKind::Hail => "HAIL",
        }
    }
}

impl StormReport {
    /// Display magnitude per the SPC filtered-CSV conventions
    /// (spc.noaa.gov/climo/reports): wind speed in mph, hail size in
    /// hundredths of an inch, tornado F_Scale as given ("EF2"). None for
    /// UNK/empty.
    pub fn magnitude_label(&self) -> Option<String> {
        let m = self.magnitude.trim();
        if m.is_empty() || m.eq_ignore_ascii_case("UNK") {
            return None;
        }
        Some(match self.kind {
            ReportKind::Wind => format!("{m} mph"),
            ReportKind::Hail => m
                .parse::<f32>()
                .map(|h| format!("{:.2}\"", h / 100.0))
                .unwrap_or_else(|_| m.to_owned()),
            ReportKind::Tornado => m.to_owned(),
        })
    }

    /// Hover-card text: kind + magnitude + time, location, remark.
    pub fn hover_text(&self) -> String {
        let mut head = self.kind.label().to_owned();
        if let Some(mag) = self.magnitude_label() {
            head.push_str(&format!(" {mag}"));
        }
        head.push_str(&format!(" · {}Z", self.time_hhmm));
        let mut out = format!("{head}\n{}", self.location);
        if !self.remark.is_empty() {
            let remark: String = self.remark.chars().take(160).collect();
            out.push_str(&format!(
                "\n{remark}{}",
                if self.remark.chars().count() > 160 {
                    "…"
                } else {
                    ""
                }
            ));
        }
        out
    }
}

fn hex_color(value: &str) -> egui::Color32 {
    let v = value.trim_start_matches('#');
    if v.len() != 6 {
        return egui::Color32::from_rgb(128, 128, 128);
    }
    let p = |i: usize| u8::from_str_radix(&v[i..i + 2], 16).unwrap_or(128);
    egui::Color32::from_rgb(p(0), p(2), p(4))
}

fn outlook_property_str<'a>(props: &'a serde_json::Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| props.get(*key).and_then(|value| value.as_str()))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn outlook_color_from_properties(props: &serde_json::Value, stroke: bool) -> egui::Color32 {
    if let Some(value) = outlook_property_str(props, &[if stroke { "stroke" } else { "fill" }]) {
        return hex_color(value);
    }
    let label = outlook_property_str(props, &["LABEL", "label", "outlook", "product", "snippet"])
        .unwrap_or_default()
        .to_ascii_uppercase();
    let color = if label.contains("HIGH") {
        "#FF00FF"
    } else if label.contains("MODERATE") || label.contains("MDT") {
        "#FF0000"
    } else if label.contains("SLIGHT") || label.contains("SLGT") {
        "#FFFF00"
    } else if label.contains("MARGINAL") || label.contains("MRGL") {
        "#00C853"
    } else if label.contains("OCCURRING") {
        "#E53935"
    } else if label.contains("LIKELY") {
        "#FB8C00"
    } else if label.contains("POSSIBLE") {
        "#FDD835"
    } else if label.contains("EXTREME") || label.contains("EXTM") {
        "#FF00FF"
    } else if label.contains("CRITICAL") || label.contains("CRIT") {
        "#FF0000"
    } else if label.contains("ELEVATED") || label.contains("ELEV") {
        "#FFBF80"
    } else if label.contains("SCATTERED") || label.contains("SCT") {
        "#FF7F00"
    } else if label.contains("ISOLATED") || label.contains("ISO") {
        "#C89BFF"
    } else {
        "#808080"
    };
    hex_color(color)
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
        let label =
            outlook_property_str(props, &["LABEL", "label", "outlook", "snippet", "product"])
                .unwrap_or("")
                .to_owned();
        let label2 = outlook_property_str(
            props,
            &[
                "LABEL2",
                "label2",
                "product",
                "valid_time",
                "VALID_ISO",
                "valid",
            ],
        )
        .unwrap_or("")
        .to_owned();
        let fill = outlook_color_from_properties(props, false);
        let stroke = outlook_color_from_properties(props, true);
        let geom = &feature["geometry"];
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
        let parse_polygon = |polygon: &serde_json::Value| -> Option<OutlookPolygon> {
            let rings = polygon.as_array()?;
            let outer = parse_ring(rings.first()?);
            if outer.len() < 3 {
                return None;
            }
            let holes = rings
                .iter()
                .skip(1)
                .map(parse_ring)
                .filter(|ring| ring.len() >= 3)
                .collect();
            Some(OutlookPolygon { outer, holes })
        };
        let mut polygons: Vec<OutlookPolygon> = Vec::new();
        match geom["type"].as_str() {
            Some("Polygon") => {
                if let Some(polygon) = parse_polygon(&geom["coordinates"]) {
                    polygons.push(polygon);
                }
            }
            Some("MultiPolygon") => {
                if let Some(polys) = geom["coordinates"].as_array() {
                    for poly in polys {
                        if let Some(polygon) = parse_polygon(poly) {
                            polygons.push(polygon);
                        }
                    }
                }
            }
            Some("GeometryCollection") => {
                if let Some(geometries) = geom["geometries"].as_array() {
                    for geometry in geometries {
                        match geometry["type"].as_str() {
                            Some("Polygon") => {
                                if let Some(polygon) = parse_polygon(&geometry["coordinates"]) {
                                    polygons.push(polygon);
                                }
                            }
                            Some("MultiPolygon") => {
                                if let Some(polys) = geometry["coordinates"].as_array() {
                                    for poly in polys {
                                        if let Some(polygon) = parse_polygon(poly) {
                                            polygons.push(polygon);
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
        let mut rings = polygons
            .iter()
            .map(|polygon| polygon.outer.clone())
            .collect::<Vec<_>>();
        rings.retain(|r| r.len() >= 3);
        if !polygons.is_empty() {
            out.push(OutlookFeature {
                label,
                label2,
                fill,
                stroke,
                fill_enabled: true,
                rings,
                polygons,
            });
        }
    }
    out
}

#[cfg(test)]
fn geojson_valid_key(text: &str) -> Option<i64> {
    let root = serde_json::from_str::<serde_json::Value>(text).ok()?;
    let feature = root.get("features")?.as_array()?.first()?;
    let valid = feature.get("properties")?.get("VALID")?;
    if let Some(valid) = valid.as_str() {
        valid.parse::<i64>().ok()
    } else {
        valid.as_i64()
    }
}

fn shifted_year_month(year: i32, month: u32, offset: i32) -> (i32, u32) {
    let zero_based = year * 12 + month as i32 - 1 + offset;
    (
        zero_based.div_euclid(12),
        zero_based.rem_euclid(12) as u32 + 1,
    )
}

fn infer_ddhhmm_time(token: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let token = token.trim_end_matches('Z');
    if token.len() != 6 {
        return None;
    }
    let day = token[0..2].parse::<u32>().ok()?;
    let hour = token[2..4].parse::<u32>().ok()?;
    let minute = token[4..6].parse::<u32>().ok()?;
    if hour > 23 || minute > 59 {
        return None;
    }
    let mut best: Option<(i64, i64)> = None;
    for month_offset in -1..=1 {
        let (year, month) = shifted_year_month(now.year(), now.month(), month_offset);
        let Some(date) = NaiveDate::from_ymd_opt(year, month, day) else {
            continue;
        };
        let Some(naive) = date.and_hms_opt(hour, minute, 0) else {
            continue;
        };
        let candidate = DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc);
        let delta = (candidate - now).num_seconds().abs();
        if best
            .map(|(best_delta, _)| delta < best_delta)
            .unwrap_or(true)
        {
            best = Some((delta, candidate.timestamp()));
        }
    }
    best.and_then(|(_, timestamp)| DateTime::<Utc>::from_timestamp(timestamp, 0))
}

#[cfg(test)]
fn issue_key(time: DateTime<Utc>) -> i64 {
    (time.year() as i64) * 100_000_000
        + (time.month() as i64) * 1_000_000
        + (time.day() as i64) * 10_000
        + (time.hour() as i64) * 100
        + time.minute() as i64
}

#[cfg(test)]
fn pts_valid_time(text: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let token = text.lines().find_map(|line| {
        let trimmed = line.trim();
        let rest = trimmed.strip_prefix("VALID TIME ")?;
        rest.split_whitespace().next()
    })?;
    infer_ddhhmm_time(token, now)
}

#[cfg(test)]
fn pts_valid_key(text: &str, now: DateTime<Utc>) -> Option<i64> {
    pts_valid_time(text, now).map(issue_key)
}

fn pts_issue_time(text: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let token = text
        .lines()
        .next()?
        .split_whitespace()
        .find(|part| part.len() == 6 && part.chars().all(|ch| ch.is_ascii_digit()))?;
    infer_ddhhmm_time(token, now)
}

fn geojson_issue_time(text: &str) -> Option<DateTime<Utc>> {
    let root = serde_json::from_str::<serde_json::Value>(text).ok()?;
    let feature = root.get("features")?.as_array()?.first()?;
    let props = feature.get("properties")?;
    if let Some(issue_iso) = props.get("ISSUE_ISO").and_then(|v| v.as_str())
        && let Ok(time) = DateTime::parse_from_rfc3339(issue_iso)
    {
        return Some(time.with_timezone(&Utc));
    }
    let issue = props.get("ISSUE")?;
    let key = issue
        .as_str()
        .and_then(|value| value.parse::<i64>().ok())
        .or_else(|| issue.as_i64())?;
    let year = (key / 100_000_000) as i32;
    let month = ((key / 1_000_000) % 100) as u32;
    let day = ((key / 10_000) % 100) as u32;
    let hour = ((key / 100) % 100) as u32;
    let minute = (key % 100) as u32;
    NaiveDate::from_ymd_opt(year, month, day)?
        .and_hms_opt(hour, minute, 0)
        .map(|naive| DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
}

fn live_pts_url(day: u8) -> Option<&'static str> {
    match day {
        1 => Some("https://tgftp.nws.noaa.gov/data/raw/wu/wuus01.kwns.pts.dy1.txt"),
        2 => Some("https://tgftp.nws.noaa.gov/data/raw/wu/wuus02.kwns.pts.dy2.txt"),
        3 => Some("https://tgftp.nws.noaa.gov/data/raw/wu/wuus03.kwns.pts.dy3.txt"),
        4..=8 => Some("https://tgftp.nws.noaa.gov/data/raw/wu/wuus48.kwns.pts.d48.txt"),
        _ => None,
    }
}

fn standard_spc_outlook_kind(kind: &str) -> bool {
    matches!(kind, "cat" | "torn" | "wind" | "hail" | "prob")
}

fn pts_section_name(kind: &str) -> Option<&'static str> {
    match kind {
        "cat" => Some("CATEGORICAL"),
        "torn" => Some("TORNADO"),
        "wind" => Some("WIND"),
        "hail" => Some("HAIL"),
        "prob" => Some("ANY SEVERE"),
        _ => None,
    }
}

fn categorical_label2(label: &str) -> &'static str {
    match label {
        "TSTM" => "General Thunderstorms Risk",
        "MRGL" => "Marginal Risk",
        "SLGT" => "Slight Risk",
        "ENH" => "Enhanced Risk",
        "MDT" => "Moderate Risk",
        "HIGH" => "High Risk",
        _ => "Categorical Risk",
    }
}

fn categorical_colors(label: &str) -> (egui::Color32, egui::Color32) {
    match label {
        "TSTM" => (hex_color("#C1E9C1"), hex_color("#55BB55")),
        "MRGL" => (hex_color("#66A366"), hex_color("#005500")),
        "SLGT" => (hex_color("#FFE066"), hex_color("#DDAA00")),
        "ENH" => (hex_color("#FFB266"), hex_color("#FF6600")),
        "MDT" => (hex_color("#E066E0"), hex_color("#A000A0")),
        "HIGH" => (hex_color("#FF66FF"), hex_color("#CC00CC")),
        _ => (egui::Color32::from_rgb(128, 128, 128), egui::Color32::WHITE),
    }
}

fn probability_colors(label: &str) -> (egui::Color32, egui::Color32) {
    match label {
        "0.02" => (hex_color("#79BA7A"), hex_color("#1A731D")),
        "0.05" => (hex_color("#C5A392"), hex_color("#8B4726")),
        "0.10" => (hex_color("#FFE066"), hex_color("#DDAA00")),
        "0.15" => (hex_color("#FFEB7F"), hex_color("#FF9600")),
        "0.30" => (hex_color("#FF7F7F"), hex_color("#FF0000")),
        "0.45" => (hex_color("#DDA0DD"), hex_color("#800080")),
        "0.60" => (hex_color("#FF66FF"), hex_color("#CC00CC")),
        _ => (egui::Color32::from_rgb(128, 128, 128), egui::Color32::WHITE),
    }
}

fn pts_label2(kind: &str, label: &str) -> String {
    if kind == "cat" {
        return categorical_label2(label).to_owned();
    }
    let product = match kind {
        "torn" => "Tornado",
        "wind" => "Wind",
        "hail" => "Hail",
        "prob" => "Any Severe",
        _ => "Severe",
    };
    // Same long names SPC's lyr.geojson publishes for these features
    // (e.g. "10% Significant Tornado Risk", "Wind Conditional Intensity
    // Group 1 Risk").
    if label == "SIGN" {
        return format!("10% Significant {product} Risk");
    }
    if let Some(group) = label.strip_prefix("CIG") {
        return format!("{product} Conditional Intensity Group {group} Risk");
    }
    let percent = label
        .parse::<f32>()
        .map(|value| format!("{:.0}", value * 100.0))
        .unwrap_or_else(|_| label.to_owned());
    format!("{percent}% {product} Risk")
}

fn pts_colors(kind: &str, label: &str) -> (egui::Color32, egui::Color32) {
    if kind == "cat" {
        categorical_colors(label)
    } else if is_pts_hatched_label(label) {
        // SPC's lyr.geojson styles SIGN and CIG areas gray-on-black
        // (fill #888888, stroke #000000) — the hatched overlay.
        (hex_color("#888888"), hex_color("#000000"))
    } else {
        probability_colors(label)
    }
}

/// SIGN (significant severe) and CIG1/CIG2/... (conditional intensity
/// group) blocks share the probability sections in raw PTS text; each is
/// its own hatched area, exactly like the SIGN/CIG features SPC's GeoJSON
/// carries.
fn is_pts_hatched_label(token: &str) -> bool {
    token == "SIGN"
        || token
            .strip_prefix("CIG")
            .map(|group| !group.is_empty() && group.chars().all(|ch| ch.is_ascii_digit()))
            .unwrap_or(false)
}

fn is_pts_label(kind: &str, token: &str) -> bool {
    if kind == "cat" {
        matches!(token, "TSTM" | "MRGL" | "SLGT" | "ENH" | "MDT" | "HIGH")
    } else {
        is_pts_hatched_label(token) || (token.contains('.') && token.parse::<f32>().is_ok())
    }
}

fn parse_pts_coord(token: &str) -> Option<(f32, f32)> {
    if token.len() != 8 || !token.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    let lat = token[0..4].parse::<f32>().ok()? / 100.0;
    let mut lon = token[4..8].parse::<f32>().ok()? / 100.0;
    // SPC point strings omit the leading "1" for longitudes west of 100W
    // (e.g. 31641340 = 31.64N, 113.40W); per the PTS spec, any parsed
    // longitude below 55.00 had that leading "1" dropped.
    if lon < 55.0 {
        lon += 100.0;
    }
    Some((-lon, lat))
}

#[derive(Default)]
struct PtsFeatureBuilder {
    label: String,
    rings: Vec<Vec<(f32, f32)>>,
    current: Vec<(f32, f32)>,
}

impl PtsFeatureBuilder {
    fn new(label: &str) -> Self {
        Self {
            label: label.to_owned(),
            ..Default::default()
        }
    }

    fn finish_ring(&mut self) {
        if let Some(ring) = close_pts_ring(std::mem::take(&mut self.current)) {
            self.rings.push(ring);
        } else {
            self.current.clear();
        }
    }

    fn finish(mut self, kind: &str) -> Option<OutlookFeature> {
        self.finish_ring();
        if self.rings.is_empty() {
            return None;
        }
        let (fill, stroke) = pts_colors(kind, &self.label);
        let polygons = self
            .rings
            .iter()
            .cloned()
            .map(|outer| OutlookPolygon {
                outer,
                holes: Vec::new(),
            })
            .collect();
        Some(OutlookFeature {
            label2: pts_label2(kind, &self.label),
            label: self.label,
            fill,
            stroke,
            fill_enabled: true,
            rings: self.rings,
            polygons,
        })
    }
}

fn close_pts_ring(mut ring: Vec<(f32, f32)>) -> Option<Vec<(f32, f32)>> {
    if ring.len() < 3 {
        return None;
    }
    ring.dedup_by(|left, right| {
        (left.0 - right.0).abs() < 0.001 && (left.1 - right.1).abs() < 0.001
    });
    if ring.len() < 3 {
        return None;
    }
    let needs_close = ring
        .first()
        .zip(ring.last())
        .map(|(first, last)| (first.0 - last.0).abs() > 0.001 || (first.1 - last.1).abs() > 0.001)
        .unwrap_or(false);
    if needs_close {
        ring.push(ring[0]);
    }
    Some(ring)
}

/// Parse raw SPC PTS point blocks. This is the fast live path: PTS products
/// usually appear before SPC's direct GeoJSON. `99999999` splits separate
/// rings, and each ring is closed before drawing so the layer can be filled
/// until the official GeoJSON catches up. SIGN and CIG1/CIG2/... tokens
/// start their own hatched features — never extra rings of the previous
/// probability contour — matching the GeoJSON representation.
pub fn parse_pts_outlook(text: &str, kind: &str) -> Vec<OutlookFeature> {
    let Some(section_name) = pts_section_name(kind) else {
        return Vec::new();
    };
    let mut active = false;
    let mut builder: Option<PtsFeatureBuilder> = None;
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "&&" && active {
            break;
        }
        if trimmed.starts_with("...") && trimmed.ends_with("...") {
            let name = trimmed.trim_matches('.').trim();
            active = name.eq_ignore_ascii_case(section_name);
            continue;
        }
        if !active || trimmed.is_empty() {
            continue;
        }
        for token in trimmed.split_whitespace() {
            if is_pts_label(kind, token) {
                if let Some(previous) = builder.take().and_then(|b| b.finish(kind)) {
                    out.push(previous);
                }
                builder = Some(PtsFeatureBuilder::new(token));
                continue;
            }
            let Some(builder) = builder.as_mut() else {
                continue;
            };
            if token == "99999999" {
                builder.finish_ring();
            } else if let Some(point) = parse_pts_coord(token) {
                builder.current.push(point);
            }
        }
    }
    if let Some(feature) = builder.and_then(|b| b.finish(kind)) {
        out.push(feature);
    }
    out
}

fn estofex_colors(label: &str) -> (egui::Color32, egui::Color32) {
    match label {
        // ESTOFEX official legend: lightning probability is yellow, level 1
        // orange, level 2 red, level 3 magenta.
        "EU TSTM15" | "EU TSTM50" => (hex_color("#FFFF00"), hex_color("#FFFF00")),
        "EU L1" => (hex_color("#FF8000"), hex_color("#FF8000")),
        "EU L2" => (hex_color("#FF0000"), hex_color("#FF0000")),
        "EU L3" => (hex_color("#FF00FF"), hex_color("#FF00FF")),
        _ => (egui::Color32::from_rgb(160, 160, 160), egui::Color32::WHITE),
    }
}

fn estofex_label(risktype: &str) -> Option<(&'static str, &'static str)> {
    match risktype.trim().to_ascii_lowercase().as_str() {
        "level 1" => Some(("EU L1", "ESTOFEX Level 1")),
        "level 2" => Some(("EU L2", "ESTOFEX Level 2")),
        "level 3" => Some(("EU L3", "ESTOFEX Level 3")),
        "15thunder" => Some(("EU TSTM15", "ESTOFEX 15% thunder")),
        "50thunder" => Some(("EU TSTM50", "ESTOFEX 50% thunder")),
        _ => None,
    }
}

pub fn estofex_feature_draw_rank(feature: &OutlookFeature) -> u8 {
    estofex_label_draw_rank(&feature.label)
}

fn estofex_label_draw_rank(label: &str) -> u8 {
    match label {
        "EU TSTM15" => 0,
        "EU TSTM50" => 1,
        "EU L1" => 2,
        "EU L2" => 3,
        "EU L3" => 4,
        _ => u8::MAX,
    }
}

fn attr_value(event: &quick_xml::events::BytesStart<'_>, key: &[u8]) -> Option<String> {
    event.attributes().flatten().find_map(|attr| {
        (attr.key.as_ref() == key)
            .then(|| {
                attr.decode_and_unescape_value(event.decoder())
                    .ok()
                    .map(|value| value.into_owned())
            })
            .flatten()
    })
}

struct EstofexAreaBuilder {
    label: String,
    label2: String,
    rings: Vec<Vec<(f32, f32)>>,
    current_ring: Vec<(f32, f32)>,
}

impl EstofexAreaBuilder {
    fn finish_ring(&mut self) {
        if let Some(ring) = close_pts_ring(std::mem::take(&mut self.current_ring)) {
            self.rings.push(ring);
        } else {
            self.current_ring.clear();
        }
    }

    fn finish(mut self) -> Option<OutlookFeature> {
        self.finish_ring();
        if self.rings.is_empty() {
            return None;
        }
        let polygons = rings_to_polygons_with_holes(self.rings);
        if polygons.is_empty() {
            return None;
        }
        let rings = polygons
            .iter()
            .map(|polygon| polygon.outer.clone())
            .collect();
        let (fill, stroke) = estofex_colors(&self.label);
        Some(OutlookFeature {
            label: self.label,
            label2: self.label2,
            fill,
            stroke,
            fill_enabled: true,
            rings,
            polygons,
        })
    }
}

#[cfg(test)]
pub fn parse_estofex_outlook_xml(text: &str) -> Vec<OutlookFeature> {
    parse_estofex_area_features_xml(text)
}

pub fn parse_estofex_issue_xml(text: &str, id_hint: Option<&str>) -> Option<EstofexIssue> {
    let issued_at = estofex_time_tag_value(text, "issue_time")?;
    let valid_from = estofex_time_tag_value(text, "start_time")?;
    let valid_to = estofex_time_tag_value(text, "expiry_time")?;
    if valid_to <= valid_from {
        return None;
    }
    let id = id_hint.map(str::to_owned).unwrap_or_else(|| {
        format!(
            "{}-{}",
            valid_from.format("%Y%m%d%H"),
            issued_at.format("%Y%m%d%H%M")
        )
    });
    Some(EstofexIssue {
        id,
        issued_at,
        valid_from,
        valid_to,
        polygons: parse_estofex_area_features_xml(text),
    })
}

fn parse_estofex_area_features_xml(text: &str) -> Vec<OutlookFeature> {
    use quick_xml::events::Event;

    let polygon_text = estofex_polygon_fragment(text).unwrap_or(text);
    let mut reader = quick_xml::Reader::from_str(polygon_text);
    reader.config_mut().trim_text(true);
    let mut current: Option<EstofexAreaBuilder> = None;
    let mut out = Vec::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) if event.name().as_ref() == b"area" => {
                current = attr_value(&event, b"risktype").and_then(|risk| {
                    let (label, label2) = estofex_label(&risk)?;
                    Some(EstofexAreaBuilder {
                        label: label.to_owned(),
                        label2: label2.to_owned(),
                        rings: Vec::new(),
                        current_ring: Vec::new(),
                    })
                });
            }
            Ok(Event::Start(event)) if estofex_ring_boundary_tag(event.name().as_ref()) => {
                if let Some(area) = current.as_mut() {
                    area.finish_ring();
                }
            }
            Ok(Event::Empty(event)) if event.name().as_ref() == b"point" => {
                if let Some(area) = current.as_mut() {
                    let lat =
                        attr_value(&event, b"lat").and_then(|value| value.parse::<f32>().ok());
                    let lon =
                        attr_value(&event, b"lon").and_then(|value| value.parse::<f32>().ok());
                    if let (Some(lat), Some(lon)) = (lat, lon) {
                        area.current_ring.push((lon, lat));
                    }
                }
            }
            Ok(Event::End(event)) if estofex_ring_boundary_tag(event.name().as_ref()) => {
                if let Some(area) = current.as_mut() {
                    area.finish_ring();
                }
            }
            Ok(Event::End(event)) if event.name().as_ref() == b"area" => {
                if let Some(area) = current.take().and_then(EstofexAreaBuilder::finish) {
                    out.push(area);
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }

    out.sort_by_key(estofex_feature_draw_rank);
    out
}

fn estofex_ring_boundary_tag(name: &[u8]) -> bool {
    matches!(
        name,
        b"ring" | b"polygon" | b"contour" | b"path" | b"part" | b"hole"
    )
}

fn parse_estofex_time_value(value: &str) -> Option<DateTime<Utc>> {
    let trimmed = value.trim();
    let normalized;
    let value = match trimmed.len() {
        10 => {
            normalized = format!("{trimmed}00");
            normalized.as_str()
        }
        12 => trimmed,
        _ => return None,
    };
    chrono::NaiveDateTime::parse_from_str(value, "%Y%m%d%H%M")
        .ok()
        .map(|naive| DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
}

fn estofex_time_tag_value(text: &str, tag: &str) -> Option<DateTime<Utc>> {
    let start = text.find(&format!("<{tag}"))?;
    let tag_text = &text[start..text[start..].find('>').map(|end| start + end)?];
    let value_start = tag_text
        .find("value=\"")
        .map(|index| index + "value=\"".len())?;
    let value_rest = &tag_text[value_start..];
    let value_end = value_rest.find('"')?;
    parse_estofex_time_value(&value_rest[..value_end])
}

fn rings_to_polygons_with_holes(mut rings: Vec<Vec<(f32, f32)>>) -> Vec<OutlookPolygon> {
    rings.retain(|ring| ring.len() >= 3);
    rings.sort_by(|left, right| {
        ring_area(right)
            .abs()
            .partial_cmp(&ring_area(left).abs())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut polygons: Vec<OutlookPolygon> = Vec::new();
    for ring in rings {
        let center = ring_centroid(&ring);
        if let Some(parent) = polygons.iter_mut().find(|polygon| {
            ring_contains_point(&polygon.outer, center)
                && !polygon
                    .holes
                    .iter()
                    .any(|hole| ring_contains_point(hole, center))
        }) {
            parent.holes.push(ring);
        } else {
            polygons.push(OutlookPolygon {
                outer: ring,
                holes: Vec::new(),
            });
        }
    }
    polygons
}

fn ring_area(ring: &[(f32, f32)]) -> f32 {
    if ring.len() < 3 {
        return 0.0;
    }
    let mut area = 0.0;
    for index in 0..ring.len() {
        let current = ring[index];
        let next = ring[(index + 1) % ring.len()];
        area += current.0 * next.1 - next.0 * current.1;
    }
    area * 0.5
}

fn ring_centroid(ring: &[(f32, f32)]) -> (f32, f32) {
    let mut sum_lon = 0.0;
    let mut sum_lat = 0.0;
    let mut count = 0usize;
    for &(lon, lat) in ring {
        sum_lon += lon;
        sum_lat += lat;
        count += 1;
    }
    let denom = count.max(1) as f32;
    (sum_lon / denom, sum_lat / denom)
}

fn ring_contains_point(ring: &[(f32, f32)], point: (f32, f32)) -> bool {
    if ring.len() < 3 {
        return false;
    }
    let mut inside = false;
    let mut previous = ring[ring.len() - 1];
    for &current in ring {
        let crosses = (current.1 > point.1) != (previous.1 > point.1);
        if crosses {
            let lon_at_lat = (previous.0 - current.0) * (point.1 - current.1)
                / (previous.1 - current.1)
                + current.0;
            if point.0 < lon_at_lat {
                inside = !inside;
            }
        }
        previous = current;
    }
    inside
}

#[cfg(test)]
pub fn outlook_polygon_contains_point(polygon: &OutlookPolygon, lon: f32, lat: f32) -> bool {
    let point = (lon, lat);
    ring_contains_point(&polygon.outer, point)
        && !polygon
            .holes
            .iter()
            .any(|hole| ring_contains_point(hole, point))
}

#[cfg(test)]
pub fn outlook_feature_contains_point(feature: &OutlookFeature, lon: f32, lat: f32) -> bool {
    feature
        .polygons
        .iter()
        .any(|polygon| outlook_polygon_contains_point(polygon, lon, lat))
}

pub fn estofex_issue_valid_at(issue: &EstofexIssue, displayed_time: DateTime<Utc>) -> bool {
    issue.issued_at <= displayed_time
        && issue.valid_from <= displayed_time
        && displayed_time < issue.valid_to
}

pub fn selected_estofex_issue<'a>(
    issues: &'a [EstofexIssue],
    selected_id: Option<&str>,
    displayed_time: DateTime<Utc>,
) -> Option<&'a EstofexIssue> {
    if let Some(selected_id) = selected_id {
        return issues.iter().find(|issue| issue.id == selected_id);
    }
    issues
        .iter()
        .filter(|issue| estofex_issue_valid_at(issue, displayed_time))
        .max_by_key(|issue| issue.issued_at)
}

pub fn estofex_issue_label(issue: &EstofexIssue) -> String {
    format!(
        "{} update - valid {} to {}",
        issue.issued_at.format("%b %-d %HZ"),
        issue.valid_from.format("%b %-d %HZ"),
        issue.valid_to.format("%b %-d %HZ")
    )
}

fn estofex_polygon_fragment(text: &str) -> Option<&str> {
    let start = text.find("<area")?;
    let end = text
        .rfind("</area>")
        .map(|index| index + "</area>".len())
        .unwrap_or(text.len());
    (start < end).then(|| &text[start..end])
}

fn fetch_estofex_issues() -> Vec<EstofexIssue> {
    const BASE: &str = "https://www.estofex.org/cgi-bin/polygon/showforecast.cgi";
    let mut issues = data_source::fetch_text(&format!("{BASE}?listvalid=yes"))
        .map(|text| estofex_fcstfiles_from_listing(&text))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|fcstfile| {
            let url = format!("{BASE}?xml=yes&fcstfile={fcstfile}");
            data_source::fetch_text(&url)
                .ok()
                .and_then(|text| parse_estofex_issue_xml(&text, Some(&fcstfile)))
        })
        .collect::<Vec<_>>();
    if issues.is_empty()
        && let Ok(text) = data_source::fetch_text(&format!("{BASE}?xml=yes"))
        && let Some(issue) = parse_estofex_issue_xml(&text, None)
    {
        issues.push(issue);
    }
    issues.sort_by_key(|issue| issue.issued_at);
    issues.dedup_by(|left, right| left.id == right.id);
    issues
}

fn estofex_fcstfiles_from_listing(text: &str) -> Vec<String> {
    let mut files = Vec::new();
    let mut rest = text;
    while let Some(index) = rest.find("fcstfile=") {
        rest = &rest[index + "fcstfile=".len()..];
        let end = rest
            .find(['"', '\'', '&', '<', '>', ')', ' '])
            .unwrap_or(rest.len());
        let file = rest[..end].trim();
        if file.ends_with(".xml") && !files.iter().any(|known| known == file) {
            files.push(file.to_owned());
        }
        rest = &rest[end..];
    }
    files
}

/// The SPC convective day containing `when`: report days run 12Z -> 12Z
/// next day, so anything before 12Z belongs to the PREVIOUS day's file
/// (spc.noaa.gov/climo/reports: "reports are for the 1200 UTC day").
pub fn spc_convective_date(when: DateTime<Utc>) -> NaiveDate {
    use chrono::Timelike;
    if when.hour() < 12 {
        when.date_naive() - Duration::days(1)
    } else {
        when.date_naive()
    }
}

/// Absolute UTC time of an HHMM report inside `convective` day's file
/// (HHMM < 1200 wraps to the next calendar day per the 12Z convention).
pub fn report_time_utc(convective: NaiveDate, hhmm: u32) -> Option<DateTime<Utc>> {
    let (hour, minute) = (hhmm / 100, hhmm % 100);
    if hour > 23 || minute > 59 {
        return None;
    }
    let date = if hour < 12 {
        convective + Duration::days(1)
    } else {
        convective
    };
    date.and_hms_opt(hour, minute, 0)
        .map(|naive| DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
}

/// Parse one section of a filtered storm-report CSV
/// (Time,Mag,Location,County,State,Lat,Lon,Comments) for `convective` day.
pub fn parse_reports(kind: ReportKind, convective: NaiveDate, text: &str) -> Vec<StormReport> {
    let mut out = Vec::new();
    for line in text.lines().skip(1) {
        if let Some(report) = parse_report_row(kind, convective, line) {
            out.push(report);
        }
    }
    out
}

fn parse_report_row(kind: ReportKind, convective: NaiveDate, line: &str) -> Option<StormReport> {
    let cols: Vec<&str> = line.splitn(8, ',').collect();
    if cols.len() < 8 {
        return None;
    }
    let (Ok(lat), Ok(lon)) = (cols[5].trim().parse::<f32>(), cols[6].trim().parse::<f32>()) else {
        return None;
    };
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return None;
    }
    let time_utc = cols[0]
        .trim()
        .parse::<u32>()
        .ok()
        .and_then(|hhmm| report_time_utc(convective, hhmm))?;
    Some(StormReport {
        kind,
        time_hhmm: cols[0].trim().to_owned(),
        time_utc,
        lat,
        lon,
        magnitude: cols[1].trim().to_owned(),
        location: format!("{}, {} {}", cols[2].trim(), cols[3].trim(), cols[4].trim()),
        remark: cols[7].trim().to_owned(),
    })
}

/// Parse the COMBINED per-day report CSV: three sections, each opened by
/// its own header row ("Time,F_Scale,…" tornado — "F-Scale" in pre-2012
/// files — then "Time,Speed,…" wind, then "Time,Size,…" hail). One fetch
/// covers the whole day. Unknown sections are skipped, never an error.
pub fn parse_reports_combined(convective: NaiveDate, text: &str) -> Vec<StormReport> {
    let mut out = Vec::new();
    let mut kind: Option<ReportKind> = None;
    for line in text.lines() {
        if line.starts_with("Time,") {
            kind = match line.split(',').nth(1).unwrap_or("") {
                "F_Scale" | "F-Scale" => Some(ReportKind::Tornado),
                "Speed" => Some(ReportKind::Wind),
                "Size" => Some(ReportKind::Hail),
                _ => None,
            };
            continue;
        }
        if let Some(kind) = kind
            && let Some(report) = parse_report_row(kind, convective, line)
        {
            out.push(report);
        }
    }
    out
}

/// One tornado track segment for the event-day map: a surveyed begin/end
/// path from the SPC WCM database, or a zero-length stand-in synthesized
/// from a torn report when the year's database file is not published yet.
#[derive(Clone, Debug)]
pub struct TornadoSegment {
    pub time_utc: DateTime<Utc>,
    /// "EF3" / "F2" / "EF?" (rating -9 = unknown).
    pub ef_label: String,
    /// County/state for synthesized segments, state for WCM rows.
    pub location: String,
    pub begin_lat: f32,
    pub begin_lon: f32,
    /// None for zero-length segments (unknown or unsurveyed end point —
    /// the WCM database stores those as 0.0/0.0).
    pub end: Option<(f32, f32)>,
    /// Surveyed lift time where the database carries one (the
    /// consolidated `actual_tornadoes` files' edat/etime columns,
    /// populated 2007+); None = estimate from the path length.
    pub end_time_utc: Option<DateTime<Utc>>,
    pub length_mi: f32,
    pub width_yd: f32,
}

impl TornadoSegment {
    pub fn is_track(&self) -> bool {
        self.end.is_some()
    }

    pub fn end_or_begin(&self) -> (f32, f32) {
        self.end.unwrap_or((self.begin_lat, self.begin_lon))
    }

    /// Hover-card text, same grammar as the report dots.
    pub fn hover_text(&self) -> String {
        let mut out = format!(
            "TORNADO {} · {}Z\n{}",
            self.ef_label,
            self.time_utc.format("%H%M"),
            self.location
        );
        if let Some(wind) = tornado_wind_estimate_label(&self.ef_label) {
            out.push_str(&format!("\nEF-scale wind estimate: {wind}"));
        }
        if self.is_track() && self.length_mi > 0.0 {
            out.push_str(&format!(
                "\n{:.1} mi path · {:.0} yd wide",
                self.length_mi, self.width_yd
            ));
        }
        out.push_str("\nClick: load the radar loop for this track");
        out
    }
}

pub fn tornado_rating_index(label: &str) -> Option<u8> {
    label
        .chars()
        .rev()
        .find(|ch| ch.is_ascii_digit())
        .and_then(|ch| ch.to_digit(10))
        .and_then(|value| (value <= 5).then_some(value as u8))
}

pub fn tornado_wind_estimate_label(label: &str) -> Option<&'static str> {
    match tornado_rating_index(label)? {
        0 => Some("65-85 mph"),
        1 => Some("86-110 mph"),
        2 => Some("111-135 mph"),
        3 => Some("136-165 mph"),
        4 => Some("166-200 mph"),
        5 => Some("201+ mph"),
        _ => None,
    }
}

/// Parse the WCM per-year tornado file ("onetor" format: om,yr,mo,dy,
/// date,time,tz,st,stf,stn,mag,inj,fat,loss,closs,slat,slon,elat,elon,
/// len,wid,ns,sn,sg,…; Schaefer & Edwards 1999) down to `convective`
/// day's full-track segments.
///
/// Row selection: sg == 1 only — that is the ENTIRE track (single-state
/// tornadoes, and the whole-track summary row of multi-state tornadoes;
/// sg == 2 rows are the per-state pieces of the same track and sg == -9
/// rows are county-list continuations with zeroed coordinates).
/// Times are CST in the database (tz == 3; tz == 9 marks the few GMT
/// rows): UTC = CST + 6 h, and the convective day filter runs on the
/// UTC time.
pub fn parse_wcm_torn_segments(convective: NaiveDate, text: &str) -> Vec<TornadoSegment> {
    let mut out = Vec::new();
    for line in text.lines().skip(1) {
        let cols: Vec<&str> = line.split(',').collect();
        if cols.len() < 24 || cols[23].trim() != "1" {
            continue;
        }
        let Ok(date) = NaiveDate::parse_from_str(cols[4].trim(), "%Y-%m-%d") else {
            continue;
        };
        let Ok(time) = chrono::NaiveTime::parse_from_str(cols[5].trim(), "%H:%M:%S") else {
            continue;
        };
        // tz 3 = CST (the database norm); tz 9 = GMT; anything else is
        // legacy/unknown and treated as CST, the documented default.
        let offset_hours = if cols[6].trim() == "9" { 0 } else { 6 };
        let time_utc = DateTime::<Utc>::from_naive_utc_and_offset(date.and_time(time), Utc)
            + Duration::hours(offset_hours);
        if spc_convective_date(time_utc) != convective {
            continue;
        }
        let parse_coord = |value: &str| value.trim().parse::<f32>().ok();
        let (Some(begin_lat), Some(begin_lon)) = (parse_coord(cols[15]), parse_coord(cols[16]))
        else {
            continue;
        };
        if !(-90.0..=90.0).contains(&begin_lat) || begin_lat == 0.0 {
            continue;
        }
        let end = match (parse_coord(cols[17]), parse_coord(cols[18])) {
            (Some(lat), Some(lon))
                if lat != 0.0
                    && lon != 0.0
                    && (lat, lon) != (begin_lat, begin_lon)
                    && (-90.0..=90.0).contains(&lat) =>
            {
                Some((lat, lon))
            }
            _ => None,
        };
        let rating = cols[10].trim();
        // EF scale adopted 2007-02-01; earlier ratings are F scale.
        let year = cols[1].trim().parse::<i32>().unwrap_or(0);
        let scale = if year >= 2007 { "EF" } else { "F" };
        let ef_label = if rating == "-9" {
            format!("{scale}?")
        } else {
            format!("{scale}{rating}")
        };
        // The consolidated `actual_tornadoes` files append edat/etime —
        // the surveyed END time (same CST convention), populated 2007+.
        let end_time_utc = (cols.len() >= 31)
            .then(|| {
                let edat = NaiveDate::parse_from_str(cols[29].trim(), "%Y-%m-%d").ok()?;
                let etime = chrono::NaiveTime::parse_from_str(cols[30].trim(), "%H:%M:%S").ok()?;
                let end_utc = DateTime::<Utc>::from_naive_utc_and_offset(edat.and_time(etime), Utc)
                    + Duration::hours(offset_hours);
                (end_utc >= time_utc).then_some(end_utc)
            })
            .flatten();
        out.push(TornadoSegment {
            time_utc,
            ef_label,
            location: cols[7].trim().to_owned(),
            begin_lat,
            begin_lon,
            end,
            end_time_utc,
            length_mi: cols[19].trim().parse().unwrap_or(0.0),
            width_yd: cols[20].trim().parse().unwrap_or(0.0),
        });
    }
    out
}

/// Everything the Event Explorer knows about one convective day.
#[derive(Default)]
pub struct EventDayData {
    pub reports: Vec<StormReport>,
    pub segments: Vec<TornadoSegment>,
    /// SPC answered 404 for both the filtered and raw report CSV — a
    /// quiet/pre-2004 day, NOT a fetch failure (those leave the day
    /// uncached so a later attempt retries).
    pub reports_file_missing: bool,
}

/// Blocking fetch of one convective day's reports + tornado segments —
/// worker thread only. `Err` = transport failure (retryable); a 404 is a
/// successful "nothing published" answer ([`EventDayData::reports_file_missing`]).
pub fn fetch_event_day(convective: NaiveDate) -> Result<EventDayData, String> {
    let mut data = EventDayData::default();
    let stamp = convective.format("%y%m%d");
    // Filtered first; older days (pre-~2012) only have the raw file.
    let mut found = false;
    for name in [
        format!("{stamp}_rpts_filtered.csv"),
        format!("{stamp}_rpts.csv"),
    ] {
        match data_source::fetch_text(&format!("https://www.spc.noaa.gov/climo/reports/{name}")) {
            Ok(text) => {
                data.reports = parse_reports_combined(convective, &text);
                // A day with zero reports still serves its header rows;
                // anything else (e.g. an HTML splash) is "no file".
                found = text.lines().any(|line| line.starts_with("Time,"));
                if found {
                    break;
                }
            }
            Err(err) if err.is_not_found() => {}
            Err(err) => return Err(err.to_string()),
        }
    }
    data.reports_file_missing = !found;

    // WCM database segments. A convective day can span New Year (the
    // 12Z window of Dec 31 reaches into Jan 1), so probe both years.
    let mut years = vec![convective.year()];
    let next_year = (convective + Duration::days(1)).year();
    if next_year != convective.year() {
        years.push(next_year);
    }
    let mut missing_years = Vec::new();
    for year in &years {
        // A missing year file (per-year files exist ~2008 onward and not
        // for the unpublished current year) falls to the consolidated
        // database below; transport failures fall through to the
        // zero-length stand-ins rather than discarding the reports
        // already in hand.
        if let Ok(text) = data_source::fetch_text(&format!(
            "https://www.spc.noaa.gov/wcm/data/{year}_torn.csv"
        )) {
            data.segments
                .extend(parse_wcm_torn_segments(convective, &text));
        } else {
            missing_years.push(*year);
        }
    }
    if !missing_years.is_empty() {
        // Consolidated fallback (1950-{Y}_actual_tornadoes.csv, ~9 MB on
        // the long-budget client; it also carries the surveyed END
        // times). Per-year files only exist from ~2008 on. A candidate
        // is only valid when it spans EVERY year of the window — then it
        // supersedes whatever the per-year files gave (same database),
        // so replace, never mix. Days newer than the last compiled year
        // (the current year) get no candidate and fall through to the
        // zero-length stand-ins.
        let current_year = Utc::now().year();
        for end_year in [current_year - 1, current_year - 2] {
            if years.iter().any(|year| *year > end_year) {
                continue;
            }
            if let Ok(text) = data_source::fetch_listing_text(&format!(
                "https://www.spc.noaa.gov/wcm/data/1950-{end_year}_actual_tornadoes.csv"
            )) {
                data.segments = parse_wcm_torn_segments(convective, &text);
                break;
            }
        }
    }
    if data.segments.is_empty() {
        // No database coverage: each torn report becomes a zero-length
        // segment so tracks stay clickable on recent days.
        data.segments = data
            .reports
            .iter()
            .filter(|report| report.kind == ReportKind::Tornado)
            .map(|report| TornadoSegment {
                time_utc: report.time_utc,
                ef_label: report.magnitude_label().unwrap_or_else(|| "EF?".to_owned()),
                location: report.location.clone(),
                begin_lat: report.lat,
                begin_lon: report.lon,
                end: None,
                end_time_utc: None,
                length_mi: 0.0,
                width_yd: 0.0,
            })
            .collect();
    } else {
        data.segments.sort_by_key(|segment| segment.time_utc);
    }
    Ok(data)
}

fn live_outlook_urls(day: u8, kind: &str, now: DateTime<Utc>) -> Vec<String> {
    match kind {
        "cigtorn" | "cigwind" | "cighail" if matches!(day, 1 | 2) => {
            return vec![format!(
                "https://www.spc.noaa.gov/products/outlook/day{day}otlk_{kind}.lyr.geojson"
            )];
        }
        "cigprob" if day == 3 => {
            return vec![
                "https://www.spc.noaa.gov/products/outlook/day3otlk_cigprob.lyr.geojson".to_owned(),
            ];
        }
        "fire_wind" if matches!(day, 1 | 2) => {
            return vec![format!(
                "https://www.spc.noaa.gov/products/fire_wx/day{day}fw_windrh.lyr.geojson"
            )];
        }
        "fire_dryt" if matches!(day, 1 | 2) => {
            return vec![format!(
                "https://www.spc.noaa.gov/products/fire_wx/day{day}fw_dryt.lyr.geojson"
            )];
        }
        "fire_wind" if day >= 3 => {
            return vec![format!(
                "https://www.spc.noaa.gov/products/exper/fire_wx/day{day}fw_windrhcat.lyr.geojson"
            )];
        }
        "fire_dryt" if day >= 3 => {
            return vec![format!(
                "https://www.spc.noaa.gov/products/exper/fire_wx/day{day}fw_drytcat.lyr.geojson"
            )];
        }
        "wpc_ero" if (1..=5).contains(&day) => {
            return vec![format!(
                "https://mapservices.weather.noaa.gov/vector/rest/services/hazards/wpc_precip_hazards/MapServer/{}/query?where=1%3D1&outFields=*&returnGeometry=true&f=geojson",
                day - 1
            )];
        }
        "wpc_river_flood" => {
            return vec![
                "https://mapservices.weather.noaa.gov/vector/rest/services/outlooks/sig_riv_fld_outlk/MapServer/0/query?where=1%3D1&outFields=*&returnGeometry=true&f=geojson"
                    .to_owned(),
            ];
        }
        _ => {}
    }
    let live_url =
        format!("https://www.spc.noaa.gov/products/outlook/day{day}otlk_{kind}.lyr.geojson");
    if day == 1 && (1..12).contains(&now.hour()) {
        let y = now.year();
        let m = now.month();
        let d = now.day();
        vec![
            format!(
                "https://www.spc.noaa.gov/products/outlook/archive/{y}/day1otlk_{y}{m:02}{d:02}_0100_{kind}.lyr.geojson"
            ),
            live_url,
        ]
    } else {
        vec![live_url]
    }
}

/// Exact official SPC GeoJSON archive URL for one filename slot.
fn archived_outlook_url(date: NaiveDate, day: u8, archive_slot: &str, kind: &str) -> String {
    format!(
        "https://www.spc.noaa.gov/products/outlook/archive/{}/day{day}otlk_{}_{archive_slot}_{kind}.lyr.geojson",
        date.year(),
        date.format("%Y%m%d")
    )
}

fn selected_day1_archive_url(date: NaiveDate, issue: SpcDay1Issue, kind: &str) -> Option<String> {
    Some(archived_outlook_url(date, 1, issue.archive_slot()?, kind))
}

/// Blocking fetch of everything enabled — worker thread only.
/// `archive_date`: when viewing archive data, fetch THAT day's outlook
/// from SPC's archive (latest issuance found, walking 2000 -> 1630 ->
/// 1300 -> 1200 -> 0100); None = the live outlook. `day`: 1-3.
/// A fixed `day1_issue` uses exactly that official archive slot even for
/// today's live view; `Auto` retains the existing live/latest behavior.
pub fn fetch_spc(
    outlook_kinds: &[&str],
    want_reports: bool,
    day: u8,
    archive_date: Option<(i32, u32, u32)>,
    day1_issue: SpcDay1Issue,
) -> SpcData {
    let mut data = SpcData {
        fetched_at: Some(Instant::now()),
        ..Default::default()
    };
    let now = Utc::now();
    let archive_date_naive =
        archive_date.and_then(|(year, month, day)| NaiveDate::from_ymd_opt(year, month, day));
    let target_date = archive_date_naive.unwrap_or_else(|| now.date_naive());
    let day1_issue = if day == 1 {
        day1_issue
    } else {
        SpcDay1Issue::Auto
    };
    let fixed_issue_not_yet_issued = day1_issue.is_not_yet_issued(target_date, now);
    let spc_kinds = effective_spc_outlook_kinds(day, outlook_kinds);
    let wants_estofex = outlook_kinds.contains(&ESTOFEX_OUTLOOK_KIND);
    let wants_standard_spc = spc_kinds.iter().any(|kind| standard_spc_outlook_kind(kind));
    let live_pts =
        if archive_date.is_none() && day1_issue == SpcDay1Issue::Auto && wants_standard_spc {
            live_pts_url(day).and_then(|url| data_source::fetch_text(url).ok())
        } else {
            None
        };
    let live_pts_issue = live_pts
        .as_deref()
        .and_then(|text| pts_issue_time(text, now));
    let mut latest_geojson_issue: Option<DateTime<Utc>> = None;
    let mut missing_geojson_outlook = false;
    let mut standard_requested = 0usize;
    let mut standard_loaded = 0usize;
    // Resolve an archive Auto slot once with the first selected standard
    // product, then reuse that exact slot for the remaining kinds. This keeps
    // all enabled fields on one issuance without walking all five URLs for
    // every field.
    let mut auto_archive_resolved = false;
    let mut auto_archive_slot: Option<&'static str> = None;
    for kind in &spc_kinds {
        let kind = *kind;
        let standard = standard_spc_outlook_kind(kind);
        if standard {
            standard_requested += 1;
        }
        let text = if day == 1 && day1_issue != SpcDay1Issue::Auto && standard {
            if fixed_issue_not_yet_issued {
                None
            } else {
                selected_day1_archive_url(target_date, day1_issue, kind)
                    .and_then(|url| data_source::fetch_text(&url).ok())
            }
        } else {
            match archive_date_naive {
                None => live_outlook_urls(day, kind, now)
                    .into_iter()
                    .find_map(|url| data_source::fetch_text(&url).ok()),
                Some(_) if !standard => None,
                Some(date) if auto_archive_resolved => auto_archive_slot.and_then(|slot| {
                    data_source::fetch_text(&archived_outlook_url(date, day, slot, kind)).ok()
                }),
                Some(date) => {
                    let mut found = None;
                    for issue in SPC_DAY1_AUTO_ARCHIVE_ORDER {
                        let slot = issue
                            .archive_slot()
                            .expect("fixed Day-1 archive issue has a filename slot");
                        if let Ok(text) =
                            data_source::fetch_text(&archived_outlook_url(date, day, slot, kind))
                        {
                            auto_archive_slot = Some(slot);
                            found = Some(text);
                            break;
                        }
                    }
                    auto_archive_resolved = true;
                    found
                }
            }
        };
        if let Some(mut text) = text {
            if standard {
                standard_loaded += 1;
            }
            let mut geojson_issue = geojson_issue_time(&text);
            if archive_date.is_none() && day1_issue == SpcDay1Issue::Auto {
                let pts_is_ahead = live_pts_issue
                    .zip(geojson_issue)
                    .map(|(pts_issue, geojson_issue)| {
                        pts_issue - geojson_issue > Duration::minutes(10)
                    })
                    .unwrap_or(false);
                if pts_is_ahead {
                    let live_url = format!(
                        "https://www.spc.noaa.gov/products/outlook/day{day}otlk_{kind}.lyr.geojson"
                    );
                    if let Ok(live_text) = data_source::fetch_text(&live_url) {
                        let live_issue = geojson_issue_time(&live_text);
                        let live_is_current = live_pts_issue
                            .zip(live_issue)
                            .map(|(pts_issue, live_issue)| {
                                pts_issue - live_issue <= Duration::minutes(10)
                            })
                            .unwrap_or(false);
                        let live_is_newer = live_issue
                            .zip(geojson_issue)
                            .map(|(live_issue, geojson_issue)| live_issue > geojson_issue)
                            .unwrap_or(false);
                        if live_is_current || live_is_newer {
                            text = live_text;
                            geojson_issue = live_issue;
                        }
                    }
                }
            }
            if let Some(issue) = geojson_issue {
                latest_geojson_issue = Some(
                    latest_geojson_issue
                        .map(|latest| latest.max(issue))
                        .unwrap_or(issue),
                );
            }
            let overlay_pts = archive_date.is_none()
                && day1_issue == SpcDay1Issue::Auto
                && standard_spc_outlook_kind(kind)
                && live_pts_issue
                    .zip(geojson_issue)
                    .map(|(pts_issue, geojson_issue)| {
                        pts_issue - geojson_issue > Duration::minutes(10)
                    })
                    .unwrap_or(false);
            if overlay_pts {
                let pts_features = live_pts
                    .as_deref()
                    .map(|pts_text| parse_pts_outlook(pts_text, kind))
                    .unwrap_or_default();
                if !pts_features.is_empty() {
                    data.outlooks.push((kind.to_owned(), pts_features));
                    continue;
                }
            }
            let features = parse_outlook(&text);
            data.outlooks.push((kind.to_owned(), features));
        } else {
            if standard_spc_outlook_kind(kind) {
                missing_geojson_outlook = true;
            }
            if archive_date.is_none()
                && day1_issue == SpcDay1Issue::Auto
                && standard_spc_outlook_kind(kind)
            {
                let pts_features = live_pts
                    .as_deref()
                    .map(|pts_text| parse_pts_outlook(pts_text, kind))
                    .unwrap_or_default();
                if !pts_features.is_empty() {
                    data.outlooks.push((kind.to_owned(), pts_features));
                }
            }
        }
    }
    if wants_estofex {
        data.estofex_issues = fetch_estofex_issues();
    }
    data.day1_issue_status = if day != 1 {
        None
    } else if day1_issue == SpcDay1Issue::Auto {
        archive_date_naive.and_then(|date| {
            if standard_requested == 0 {
                Some(SpcDay1IssueStatus::NoStandardProductSelected)
            } else if let Some(issue) = auto_archive_slot.and_then(SpcDay1Issue::from_archive_slot)
            {
                Some(SpcDay1IssueStatus::AutoArchiveLoaded {
                    date,
                    issue,
                    loaded: standard_loaded,
                    requested: standard_requested,
                })
            } else {
                Some(SpcDay1IssueStatus::AutoArchiveMissing { date })
            }
        })
    } else if standard_requested == 0 {
        Some(SpcDay1IssueStatus::NoStandardProductSelected)
    } else if fixed_issue_not_yet_issued {
        Some(SpcDay1IssueStatus::SelectedNotYetIssued {
            date: target_date,
            issue: day1_issue,
        })
    } else if standard_loaded > 0 {
        Some(SpcDay1IssueStatus::SelectedLoaded {
            date: target_date,
            issue: day1_issue,
            loaded: standard_loaded,
            requested: standard_requested,
        })
    } else {
        Some(SpcDay1IssueStatus::SelectedMissing {
            date: target_date,
            issue: day1_issue,
        })
    };
    data.outlook_geojson_lagging = archive_date.is_none()
        && day1_issue == SpcDay1Issue::Auto
        && wants_standard_spc
        && live_pts_issue
            .map(|pts_issue| {
                missing_geojson_outlook
                    || latest_geojson_issue
                        .map(|geojson_issue| pts_issue - geojson_issue > Duration::minutes(10))
                        .unwrap_or(true)
            })
            .unwrap_or(false);
    if want_reports {
        // "today" on SPC's side is the CURRENT convective day (12Z-12Z).
        let convective = spc_convective_date(Utc::now());
        for (slug, kind) in [
            ("torn", ReportKind::Tornado),
            ("wind", ReportKind::Wind),
            ("hail", ReportKind::Hail),
        ] {
            let url = format!("https://www.spc.noaa.gov/climo/reports/today_filtered_{slug}.csv");
            if let Ok(text) = data_source::fetch_text(&url) {
                data.reports.extend(parse_reports(kind, convective, &text));
            }
        }
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn parses_outlook_features() {
        let sample = r##"{"features":[{"properties":{"LABEL":"SLGT","LABEL2":"Slight Risk","fill":"#FFE066","stroke":"#DDAA00"},"geometry":{"type":"MultiPolygon","coordinates":[[[[-95.0,40.0],[-94.0,40.0],[-94.0,41.0],[-95.0,40.0]]]]}}]}"##;
        let parsed = parse_outlook(sample);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].label, "SLGT");
        assert_eq!(parsed[0].rings[0].len(), 4);
        // Base colors, fully opaque — alphas are a draw-time style concern.
        assert_eq!(parsed[0].fill, egui::Color32::from_rgb(0xFF, 0xE0, 0x66));
        assert_eq!(parsed[0].stroke, egui::Color32::from_rgb(0xDD, 0xAA, 0x00));
    }

    #[test]
    fn parses_wpc_outlook_features_without_spc_style_properties() {
        let sample = r#"{"features":[{"properties":{"outlook":"Likely","product":"WPC river flood outlook","valid_time":"Sat Jun 27 2026"},"geometry":{"type":"Polygon","coordinates":[[[-95.0,40.0],[-94.0,40.0],[-94.0,41.0],[-95.0,40.0]]]}}]}"#;
        let parsed = parse_outlook(sample);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].label, "Likely");
        assert_eq!(parsed[0].label2, "WPC river flood outlook");
        assert_eq!(parsed[0].fill, egui::Color32::from_rgb(0xFB, 0x8C, 0x00));
        assert_eq!(parsed[0].rings[0].len(), 4);
    }

    #[test]
    fn parses_raw_pts_categorical_sections_and_splits_offshore_rings() {
        let sample = "WUUS01 KWNS 140600\n\
PTSDY1\n\
\n\
VALID TIME 141200Z - 151200Z\n\
\n\
CATEGORICAL OUTLOOK POINTS DAY 1\n\
\n\
... CATEGORICAL ...\n\
\n\
MRGL   31480729 32560725 33160661 31480729\n\
TSTM   31641340 34521417 36361477 99999999\n\
       30808083 30658239 31178351\n\
&&\n\
THERE IS A MARGINAL RISK OF SVR TSTMS...\n";

        let parsed = parse_pts_outlook(sample, "cat");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].label, "MRGL");
        assert_eq!(parsed[0].rings.len(), 1);
        assert_eq!(parsed[1].label, "TSTM");
        assert_eq!(parsed[1].rings.len(), 2);
        assert!(parsed[1].fill_enabled);
        assert_eq!(parsed[1].rings[0].first(), parsed[1].rings[0].last());
        // 31641340 is 31.64N, 113.40W. PTS omits the leading "1" west
        // of 100W, so this catches the common raw-PTS longitude trap.
        assert_eq!(parsed[1].rings[0][0], (-113.40, 31.64));
        assert_eq!(parsed[1].rings[1][0], (-80.83, 30.80));
    }

    #[test]
    fn parses_raw_pts_probability_sections() {
        let sample = "WUUS03 KWNS 140612\n\
PTSDY3\n\
\n\
VALID TIME 161200Z - 171200Z\n\
\n\
PROBABILISTIC OUTLOOK POINTS DAY 3\n\
\n\
... WIND ...\n\
\n\
0.05   30808083 30658239 31178351 30808083\n\
0.15   36057492 35657601 35387761 36057492\n\
&&\n";

        let parsed = parse_pts_outlook(sample, "wind");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].label, "0.05");
        assert_eq!(parsed[0].label2, "5% Wind Risk");
        assert_eq!(parsed[1].label, "0.15");
    }

    #[test]
    fn raw_pts_sign_and_cig_blocks_become_their_own_features() {
        // Live PTS products carry CIG1 conditional-intensity blocks (and
        // older products the SIGN significant-severe block) inline in each
        // probability section. Every labeled block starts a NEW feature —
        // hatched areas must never fuse into the last probability contour
        // (regression: they rendered as phantom rings of the top contour).
        let sample = "WUUS01 KWNS 012007\n\
PTSDY1\n\
\n\
VALID TIME 012000Z - 021200Z\n\
\n\
PROBABILISTIC OUTLOOK POINTS DAY 1\n\
\n\
... WIND ...\n\
\n\
0.05   30808083 30658239 31178351 30808083\n\
0.15   36057492 35657601 35387761 36057492\n\
SIGN   38127401 37457784 36518360 38127401\n\
CIG1   42381367 43591322 44161215 42381367\n\
CIG1   37409972 36630011 33460208 37409972\n\
&&\n";

        let parsed = parse_pts_outlook(sample, "wind");
        assert_eq!(parsed.len(), 5);
        // The 15% contour keeps ONLY its own ring.
        assert_eq!(parsed[1].label, "0.15");
        assert_eq!(parsed[1].rings.len(), 1);
        assert_eq!(parsed[1].rings[0].len(), 4);
        // SIGN and each CIG block land in their own group, styled the way
        // SPC's lyr.geojson publishes them (gray fill, black stroke).
        assert_eq!(parsed[2].label, "SIGN");
        assert_eq!(parsed[2].label2, "10% Significant Wind Risk");
        assert_eq!(
            parsed[2].rings,
            vec![vec![
                (-74.01, 38.12),
                (-77.84, 37.45),
                (-83.60, 36.51),
                (-74.01, 38.12),
            ]]
        );
        assert_eq!(parsed[2].fill, egui::Color32::from_rgb(0x88, 0x88, 0x88));
        assert_eq!(parsed[2].stroke, egui::Color32::from_rgb(0x00, 0x00, 0x00));
        assert_eq!(parsed[3].label, "CIG1");
        assert_eq!(parsed[3].label2, "Wind Conditional Intensity Group 1 Risk");
        assert_eq!(parsed[3].rings.len(), 1);
        // 42381367 sits west of 100W (implied leading "1": 113.67W).
        assert_eq!(parsed[3].rings[0][0], (-113.67, 42.38));
        assert_eq!(parsed[4].label, "CIG1");
        assert_eq!(parsed[4].rings.len(), 1);
        assert_eq!(parsed[4].rings[0][0], (-99.72, 37.40));
    }

    #[test]
    fn pts_coords_reconstruct_dropped_leading_1_up_to_55w() {
        // Longitude digits below 5500 had a leading "1" dropped: 4490 is
        // 144.90W (Gulf of Alaska marine areas), not 44.90W. A too-tight
        // 30.00 threshold left 30.00-54.99 unfolded in the east Atlantic.
        assert_eq!(parse_pts_coord("58804490"), Some((-144.90, 58.80)));
        // 55.00 and above are literal longitudes east of 100W.
        assert_eq!(parse_pts_coord("40185500"), Some((-55.00, 40.18)));
        assert_eq!(parse_pts_coord("31641340"), Some((-113.40, 31.64)));
    }

    #[test]
    fn parses_day3_any_severe_pts_probability_section() {
        let sample = "WUUS03 KWNS 161934\n\
PTSDY3\n\
\n\
VALID TIME 181200Z - 191200Z\n\
\n\
PROBABILISTIC OUTLOOK POINTS DAY 3\n\
\n\
... ANY SEVERE ...\n\
\n\
0.05   28159459 31809331 33219378 28159459\n\
0.15   38127401 37457784 36518360 38127401\n\
&&\n";

        let parsed = parse_pts_outlook(sample, "prob");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].label, "0.05");
        assert_eq!(parsed[0].label2, "5% Any Severe Risk");
        assert_eq!(parsed[1].label, "0.15");
    }

    #[test]
    fn day3_hazard_probability_requests_collapse_to_any_severe() {
        assert_eq!(
            effective_spc_outlook_kinds(3, &["cat", "torn", "wind", "hail"]),
            vec!["cat", "prob"]
        );
        assert_eq!(
            live_outlook_urls(
                3,
                "prob",
                Utc.with_ymd_and_hms(2026, 6, 16, 20, 0, 0).unwrap()
            ),
            vec!["https://www.spc.noaa.gov/products/outlook/day3otlk_prob.lyr.geojson"]
        );
    }

    #[test]
    fn alert_qol_cig_controls_hidden_and_legacy_selections_ignored() {
        for day in 1..=3 {
            assert!(
                outlook_kind_options(day)
                    .iter()
                    .all(|(kind, _)| !is_legacy_cig_kind(kind))
            );
        }
        assert_eq!(
            effective_spc_outlook_kinds(1, &["cat", "torn", "cigtorn", "cigwind"]),
            vec!["cat", "torn"]
        );
    }

    #[test]
    fn new_outlook_sources_use_official_live_urls() {
        let now = Utc.with_ymd_and_hms(2026, 6, 27, 18, 0, 0).unwrap();
        assert_eq!(
            live_outlook_urls(1, "cigwind", now),
            vec!["https://www.spc.noaa.gov/products/outlook/day1otlk_cigwind.lyr.geojson"]
        );
        assert_eq!(
            live_outlook_urls(2, "fire_dryt", now),
            vec!["https://www.spc.noaa.gov/products/fire_wx/day2fw_dryt.lyr.geojson"]
        );
        assert_eq!(
            live_outlook_urls(3, "fire_wind", now),
            vec!["https://www.spc.noaa.gov/products/exper/fire_wx/day3fw_windrhcat.lyr.geojson"]
        );
        assert_eq!(
            live_outlook_urls(1, "wpc_ero", now),
            vec![
                "https://mapservices.weather.noaa.gov/vector/rest/services/hazards/wpc_precip_hazards/MapServer/0/query?where=1%3D1&outFields=*&returnGeometry=true&f=geojson"
            ]
        );
    }

    #[test]
    fn day1_issue_selector_maps_issue_times_to_official_archive_slots() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 19).unwrap();
        assert_eq!(
            selected_day1_archive_url(date, SpcDay1Issue::At0100, "cat").as_deref(),
            Some(
                "https://www.spc.noaa.gov/products/outlook/archive/2026/day1otlk_20260719_0100_cat.lyr.geojson"
            )
        );
        assert_eq!(
            selected_day1_archive_url(date, SpcDay1Issue::At0600, "torn").as_deref(),
            Some(
                "https://www.spc.noaa.gov/products/outlook/archive/2026/day1otlk_20260719_1200_torn.lyr.geojson"
            ),
            "SPC archives the 06Z issuance under its 12Z valid-time slot"
        );
        assert_eq!(
            selected_day1_archive_url(date, SpcDay1Issue::Auto, "cat"),
            None,
            "Auto remains a resolver, never a fabricated archive filename"
        );
    }

    #[test]
    fn day1_issue_selector_distinguishes_future_from_available_slots() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 20).unwrap();
        let before_06z = Utc.with_ymd_and_hms(2026, 7, 20, 5, 59, 59).unwrap();
        let at_06z = Utc.with_ymd_and_hms(2026, 7, 20, 6, 0, 0).unwrap();

        assert!(SpcDay1Issue::At0600.is_not_yet_issued(date, before_06z));
        assert!(!SpcDay1Issue::At0600.is_not_yet_issued(date, at_06z));
        assert!(!SpcDay1Issue::Auto.is_not_yet_issued(date, before_06z));
        assert_eq!(SpcDay1Issue::At0600.label(), "06:00Z");
        assert_eq!(SpcDay1Issue::At0600.archive_slot(), Some("1200"));
    }

    #[test]
    fn parses_estofex_xml_areas() {
        let sample = r#"
<forecast>
  <area risktype="level 2">
    <point lat="45.2" lon="20.7"/>
    <point lat="44.8" lon="20.3"/>
    <point lat="44.7" lon="19.5"/>
  </area>
  <area risktype="15thunder">
    <point lat="56.1" lon="21.4"/>
    <point lat="57.6" lon="21.8"/>
    <point lat="59.0" lon="23.5"/>
  </area>
  <area risktype="severe storms unlikely"></area>
</forecast>
"#;
        let parsed = parse_estofex_outlook_xml(sample);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].label, "EU TSTM15");
        assert_eq!(parsed[0].stroke, hex_color("#FFFF00"));
        assert_eq!(parsed[1].label, "EU L2");
        assert_eq!(parsed[1].label2, "ESTOFEX Level 2");
        assert_eq!(parsed[1].fill, hex_color("#FF0000"));
        assert_eq!(parsed[1].stroke, hex_color("#FF0000"));
        assert!(parsed[0].fill_enabled);
        assert_eq!(parsed[0].rings[0].first(), parsed[0].rings[0].last());
    }

    #[test]
    fn estofex_draw_order_puts_higher_levels_on_top() {
        let sample = r#"
<forecast>
  <area risktype="level 3">
    <point lat="45.0" lon="20.0"/>
    <point lat="45.0" lon="21.0"/>
    <point lat="46.0" lon="21.0"/>
  </area>
  <area risktype="level 1">
    <point lat="45.0" lon="20.0"/>
    <point lat="45.0" lon="22.0"/>
    <point lat="47.0" lon="22.0"/>
  </area>
  <area risktype="50thunder">
    <point lat="44.0" lon="19.0"/>
    <point lat="44.0" lon="23.0"/>
    <point lat="48.0" lon="23.0"/>
  </area>
  <area risktype="level 2">
    <point lat="45.5" lon="20.5"/>
    <point lat="45.5" lon="21.5"/>
    <point lat="46.5" lon="21.5"/>
  </area>
  <area risktype="15thunder">
    <point lat="43.0" lon="18.0"/>
    <point lat="43.0" lon="24.0"/>
    <point lat="49.0" lon="24.0"/>
  </area>
</forecast>
"#;
        let labels = parse_estofex_outlook_xml(sample)
            .into_iter()
            .map(|feature| feature.label)
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            vec!["EU TSTM15", "EU TSTM50", "EU L1", "EU L2", "EU L3"]
        );
    }

    #[test]
    fn estofex_interior_rings_remain_holes() {
        let sample = r#"
<forecast>
  <start_time value="2026062206"/>
  <expiry_time value="2026062306"/>
  <issue_time value="202606220500"/>
  <area risktype="15thunder">
    <ring>
      <point lat="0.0" lon="0.0"/>
      <point lat="0.0" lon="10.0"/>
      <point lat="10.0" lon="10.0"/>
      <point lat="10.0" lon="0.0"/>
      <point lat="0.0" lon="0.0"/>
    </ring>
    <ring>
      <point lat="3.0" lon="3.0"/>
      <point lat="3.0" lon="7.0"/>
      <point lat="7.0" lon="7.0"/>
      <point lat="7.0" lon="3.0"/>
      <point lat="3.0" lon="3.0"/>
    </ring>
  </area>
</forecast>
"#;
        let issue = parse_estofex_issue_xml(sample, Some("donut")).expect("issue");
        let feature = &issue.polygons[0];

        assert_eq!(feature.polygons.len(), 1);
        assert_eq!(feature.polygons[0].holes.len(), 1);
        assert_eq!(
            feature.rings.len(),
            1,
            "hole must not become a duplicate label/risk ring"
        );
        assert!(outlook_feature_contains_point(feature, 1.0, 1.0));
        assert!(!outlook_feature_contains_point(feature, 5.0, 5.0));
    }

    #[test]
    fn estofex_same_day_updates_are_separately_selectable() {
        let first = EstofexIssue {
            id: "early".to_owned(),
            issued_at: Utc.with_ymd_and_hms(2026, 6, 22, 6, 0, 0).unwrap(),
            valid_from: Utc.with_ymd_and_hms(2026, 6, 22, 6, 0, 0).unwrap(),
            valid_to: Utc.with_ymd_and_hms(2026, 6, 23, 6, 0, 0).unwrap(),
            polygons: Vec::new(),
        };
        let second = EstofexIssue {
            id: "update".to_owned(),
            issued_at: Utc.with_ymd_and_hms(2026, 6, 22, 18, 0, 0).unwrap(),
            ..first.clone()
        };
        let issues = vec![first, second];
        let displayed = Utc.with_ymd_and_hms(2026, 6, 22, 19, 0, 0).unwrap();

        assert_eq!(
            selected_estofex_issue(&issues, None, displayed).map(|issue| issue.id.as_str()),
            Some("update")
        );
        assert_eq!(
            selected_estofex_issue(&issues, Some("early"), displayed)
                .map(|issue| issue.id.as_str()),
            Some("early")
        );
    }

    #[test]
    fn estofex_listing_extracts_distinct_issue_files() {
        let html = r#"
<A HREF="/cgi-bin/polygon/showforecast.cgi?text=yes&fcstfile=2026062306_202606211403_1_stormforecast.xml">one</A>
<IMG SRC="/cgi-bin/polygon/showforecast.cgi?map=yes&fcstfile=2026062306_202606211403_1_stormforecast.xml">
<A HREF="/cgi-bin/polygon/showforecast.cgi?text=yes&fcstfile=2026062306_202606221800_1_stormforecast.xml">two</A>
"#;

        assert_eq!(
            estofex_fcstfiles_from_listing(html),
            vec![
                "2026062306_202606211403_1_stormforecast.xml".to_owned(),
                "2026062306_202606221800_1_stormforecast.xml".to_owned(),
            ]
        );
    }

    #[test]
    fn estofex_auto_issue_expires_at_valid_to() {
        let issue = EstofexIssue {
            id: "expires".to_owned(),
            issued_at: Utc.with_ymd_and_hms(2026, 6, 22, 5, 0, 0).unwrap(),
            valid_from: Utc.with_ymd_and_hms(2026, 6, 22, 6, 0, 0).unwrap(),
            valid_to: Utc.with_ymd_and_hms(2026, 6, 23, 6, 0, 0).unwrap(),
            polygons: Vec::new(),
        };
        let issues = vec![issue];

        assert!(
            selected_estofex_issue(
                &issues,
                None,
                Utc.with_ymd_and_hms(2026, 6, 23, 5, 59, 59).unwrap()
            )
            .is_some()
        );
        assert!(
            selected_estofex_issue(
                &issues,
                None,
                Utc.with_ymd_and_hms(2026, 6, 23, 6, 0, 0).unwrap()
            )
            .is_none()
        );
    }

    #[test]
    fn estofex_auto_never_displays_update_before_issue_time() {
        let issue = EstofexIssue {
            id: "late-update".to_owned(),
            issued_at: Utc.with_ymd_and_hms(2026, 6, 22, 18, 0, 0).unwrap(),
            valid_from: Utc.with_ymd_and_hms(2026, 6, 22, 6, 0, 0).unwrap(),
            valid_to: Utc.with_ymd_and_hms(2026, 6, 23, 6, 0, 0).unwrap(),
            polygons: Vec::new(),
        };
        let issues = vec![issue];

        assert!(
            selected_estofex_issue(
                &issues,
                None,
                Utc.with_ymd_and_hms(2026, 6, 22, 17, 59, 59).unwrap()
            )
            .is_none()
        );
        assert!(
            selected_estofex_issue(
                &issues,
                None,
                Utc.with_ymd_and_hms(2026, 6, 22, 18, 0, 0).unwrap()
            )
            .is_some()
        );
    }

    #[test]
    fn parses_estofex_xml_after_malformed_preamble() {
        let sample = r#"
<?xml version="1.0"?>
<forecast>
  <graphicname>&begin=2026061606</graphicname>
  <text>A level 1 was issued.<BR><BR>DISCUSSION</text>
  <area risktype="level 1">
    <point lat="49.3" lon="7.6"/>
    <point lat="50.3" lon="6.7"/>
    <point lat="51.5" lon="7.4"/>
    <point lat="49.3" lon="7.6"/>
  </area>
</forecast>
"#;
        let parsed = parse_estofex_outlook_xml(sample);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].label, "EU L1");
        assert_eq!(parsed[0].label2, "ESTOFEX Level 1");
        assert_eq!(parsed[0].stroke, hex_color("#FF8000"));
    }

    #[test]
    fn estofex_official_legend_colors_are_pinned() {
        assert_eq!(estofex_colors("EU TSTM15").1, hex_color("#FFFF00"));
        assert_eq!(estofex_colors("EU TSTM50").1, hex_color("#FFFF00"));
        assert_eq!(estofex_colors("EU L1").1, hex_color("#FF8000"));
        assert_eq!(estofex_colors("EU L2").1, hex_color("#FF0000"));
        assert_eq!(estofex_colors("EU L3").1, hex_color("#FF00FF"));
    }

    #[test]
    #[ignore = "network: fetches live SPC raw PTS and ESTOFEX XML"]
    fn live_outlook_sources_parse_smoke() {
        let pts = data_source::fetch_text(live_pts_url(1).unwrap()).expect("SPC PTS fetch");
        assert!(
            !parse_pts_outlook(&pts, "cat").is_empty(),
            "current SPC PTS categorical outlook should parse"
        );
        let estofex = data_source::fetch_text(
            "https://www.estofex.org/cgi-bin/polygon/showforecast.cgi?xml=yes",
        )
        .expect("ESTOFEX XML fetch");
        assert!(
            estofex.contains("<forecast"),
            "ESTOFEX endpoint should return forecast XML"
        );
        assert!(
            !parse_estofex_outlook_xml(&estofex).is_empty(),
            "current ESTOFEX XML should expose active polygon areas"
        );
    }

    #[test]
    fn pts_valid_key_infers_month_near_now() {
        let sample = "WUUS01 KWNS 140600\n\
PTSDY1\n\
\n\
VALID TIME 141200Z - 151200Z\n";
        let now = Utc.with_ymd_and_hms(2026, 6, 14, 6, 30, 0).unwrap();
        assert_eq!(pts_valid_key(sample, now), Some(202606141200));
    }

    #[test]
    fn geojson_valid_key_reads_spc_properties() {
        let sample = r##"{"features":[{"properties":{"VALID":"202606141200","LABEL":"SLGT"},"geometry":{"type":"Polygon","coordinates":[]}}]}"##;
        assert_eq!(geojson_valid_key(sample), Some(202606141200));
    }

    #[test]
    fn pts_issue_time_reads_raw_header() {
        let sample = "WUUS01 KWNS 140600\nPTSDY1\n";
        let now = Utc.with_ymd_and_hms(2026, 6, 14, 6, 30, 0).unwrap();
        assert_eq!(
            pts_issue_time(sample, now),
            Some(Utc.with_ymd_and_hms(2026, 6, 14, 6, 0, 0).unwrap())
        );
    }

    #[test]
    fn geojson_issue_time_reads_spc_issue_properties() {
        let sample = r##"{"features":[{"properties":{"ISSUE":"202606140558","ISSUE_ISO":"2026-06-14T05:58:00+00:00","LABEL":"SLGT"},"geometry":{"type":"Polygon","coordinates":[]}}]}"##;
        assert_eq!(
            geojson_issue_time(sample),
            Some(Utc.with_ymd_and_hms(2026, 6, 14, 5, 58, 0).unwrap())
        );
    }

    #[test]
    fn live_day1_prefers_valid_now_0100_outlook_before_12z() {
        let now = Utc.with_ymd_and_hms(2026, 6, 13, 6, 30, 0).unwrap();
        let urls = live_outlook_urls(1, "cat", now);

        assert_eq!(
            urls[0],
            "https://www.spc.noaa.gov/products/outlook/archive/2026/day1otlk_20260613_0100_cat.lyr.geojson"
        );
        assert_eq!(
            urls[1],
            "https://www.spc.noaa.gov/products/outlook/day1otlk_cat.lyr.geojson"
        );
    }

    #[test]
    fn live_day1_uses_headline_outlook_after_12z() {
        let now = Utc.with_ymd_and_hms(2026, 6, 13, 12, 0, 0).unwrap();
        let urls = live_outlook_urls(1, "cat", now);

        assert_eq!(
            urls,
            vec!["https://www.spc.noaa.gov/products/outlook/day1otlk_cat.lyr.geojson"]
        );
    }

    #[test]
    fn parses_report_rows() {
        let date = NaiveDate::from_ymd_opt(2026, 6, 11).unwrap();
        let csv = "Time,Speed,Location,County,State,Lat,Lon,Comments\n1215,UNK,3 W Dallas Center,Dallas,IA,41.69,-94.02,Tree damage. (DMX)\n";
        let parsed = parse_reports(ReportKind::Wind, date, csv);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].time_hhmm, "1215");
        assert!(parsed[0].location.contains("Dallas"));
        assert_eq!(
            parsed[0].time_utc,
            Utc.with_ymd_and_hms(2026, 6, 11, 12, 15, 0).unwrap()
        );
    }

    #[test]
    fn convective_date_wraps_at_12z() {
        // 11:59Z belongs to the PREVIOUS convective day; 12:00Z starts
        // the new one (SPC climo reports convention).
        let before = Utc.with_ymd_and_hms(2026, 6, 12, 11, 59, 0).unwrap();
        let at = Utc.with_ymd_and_hms(2026, 6, 12, 12, 0, 0).unwrap();
        assert_eq!(
            spc_convective_date(before),
            NaiveDate::from_ymd_opt(2026, 6, 11).unwrap()
        );
        assert_eq!(
            spc_convective_date(at),
            NaiveDate::from_ymd_opt(2026, 6, 12).unwrap()
        );
        // Year boundary: 03Z Jan 1 is still Dec 31's day.
        let new_year = Utc.with_ymd_and_hms(2026, 1, 1, 3, 0, 0).unwrap();
        assert_eq!(
            spc_convective_date(new_year),
            NaiveDate::from_ymd_opt(2025, 12, 31).unwrap()
        );
    }

    #[test]
    fn report_times_wrap_to_the_next_calendar_day() {
        let day = NaiveDate::from_ymd_opt(2026, 6, 11).unwrap();
        // 2105 = same calendar day; 0047 = the next one (a 00:47Z report
        // lives in the previous convective day's file).
        assert_eq!(
            report_time_utc(day, 2105).unwrap(),
            Utc.with_ymd_and_hms(2026, 6, 11, 21, 5, 0).unwrap()
        );
        assert_eq!(
            report_time_utc(day, 47).unwrap(),
            Utc.with_ymd_and_hms(2026, 6, 12, 0, 47, 0).unwrap()
        );
        assert_eq!(report_time_utc(day, 2461), None);
    }

    #[test]
    fn combined_csv_splits_into_kind_sections() {
        let day = NaiveDate::from_ymd_opt(2026, 6, 11).unwrap();
        let csv = "Time,F_Scale,Location,County,State,Lat,Lon,Comments\n\
                   2242,UNK,2 S Streator,Livingston,IL,41.09,-88.84,Large tornado. (LOT)\n\
                   Time,Speed,Location,County,State,Lat,Lon,Comments\n\
                   1215,61,3 W Dallas Center,Dallas,IA,41.69,-94.02,Trees. (DMX)\n\
                   Time,Size,Location,County,State,Lat,Lon,Comments\n\
                   2310,175,Union Grove,Kenosha,WI,42.63,-88.05,(MKX)\n";
        let parsed = parse_reports_combined(day, csv);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].kind, ReportKind::Tornado);
        assert_eq!(parsed[1].kind, ReportKind::Wind);
        assert_eq!(parsed[2].kind, ReportKind::Hail);
    }

    #[test]
    fn combined_csv_accepts_the_pre2012_fscale_header_and_rejects_html() {
        let day = NaiveDate::from_ymd_opt(2011, 4, 27).unwrap();
        let old = "Time,F-Scale,Location,County,State,Lat,Lon,Comments\n\
                   1240,UNK,1 NW TRENTON,DADE,GA,34.88,-85.52,EF1 SURVEYED. (FFC)\n";
        let parsed = parse_reports_combined(day, old);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].kind, ReportKind::Tornado);
        // SPC's 404 splash is HTML — must parse to nothing, never error.
        assert!(parse_reports_combined(day, "<!DOCTYPE HTML>\n<html></html>").is_empty());
    }

    #[test]
    fn wcm_rows_carry_begin_and_end_and_filter_to_the_convective_day() {
        let day = NaiveDate::from_ymd_opt(2011, 4, 27).unwrap();
        let header = "om,yr,mo,dy,date,time,tz,st,stf,stn,mag,inj,fat,loss,closs,slat,slon,elat,elon,len,wid,ns,sn,sg,f1,f2,f3,f4,fc\n";
        // 14:05 CST = 20:05Z on 4/27 (in the day); a whole-track ns=2
        // summary row (sg=1), its per-state sg=2 piece, an sg=-9 county
        // continuation, a 03:01 CST row (= 09:01Z -> PREVIOUS convective
        // day), and a 23:30 CST row (= 05:30Z 4/28, still 4/27's day).
        let csv = format!(
            "{header}\
             309488,2011,4,27,2011-04-27,14:05:00,3,AL,1,0,5,72,31,0,0,34.1043,-88.1479,35.0857,-86.1511,67.8,1320,2,0,1,77,33,79,83,0\n\
             309488,2011,4,27,2011-04-27,14:05:00,3,AL,1,0,5,72,31,0,0,34.1043,-88.1479,34.9915,-86.365,64.0,1320,2,1,2,77,33,79,83,0\n\
             307109,2011,4,27,2011-04-27,14:40:00,3,AL,1,0,0,0,0,0,0,0.0,0.0,0.0,0.0,0.0,0,1,0,-9,1,3,5,7,0\n\
             302195,2011,4,27,2011-04-27,03:01:00,3,AL,1,0,2,0,0,0,0,34.9406,-88.0564,35.0055,-87.9181,9.3,800,2,0,1,71,77,0,0,0\n\
             310999,2011,4,27,2011-04-27,23:30:00,3,MS,28,0,-9,0,0,0,0,32.5,-89.5,0.0,0.0,0.2,50,1,1,1,89,0,0,0,0\n"
        );
        let parsed = parse_wcm_torn_segments(day, &csv);
        assert_eq!(parsed.len(), 2);
        // The whole-track summary, with surveyed begin AND end.
        assert_eq!(parsed[0].ef_label, "EF5");
        assert_eq!(parsed[0].begin_lat, 34.1043);
        assert_eq!(parsed[0].end, Some((35.0857, -86.1511)));
        assert_eq!(
            parsed[0].time_utc,
            Utc.with_ymd_and_hms(2011, 4, 27, 20, 5, 0).unwrap()
        );
        assert!(parsed[0].is_track());
        // The late-evening row: zeroed end coords -> zero-length.
        assert_eq!(parsed[1].ef_label, "EF?");
        assert_eq!(parsed[1].end, None);
        assert!(!parsed[1].is_track());
        // 03:01 CST belongs to 4/26's convective day.
        let previous = NaiveDate::from_ymd_opt(2011, 4, 26).unwrap();
        let previous_rows = parse_wcm_torn_segments(previous, &csv);
        assert_eq!(previous_rows.len(), 1);
        assert_eq!(previous_rows[0].ef_label, "EF2");
    }

    #[test]
    fn tornado_ef_labels_map_to_estimated_wind_ranges() {
        assert_eq!(tornado_rating_index("EF4"), Some(4));
        assert_eq!(tornado_rating_index("F2"), Some(2));
        assert_eq!(tornado_rating_index("EF?"), None);
        assert_eq!(tornado_wind_estimate_label("EF4"), Some("166-200 mph"));
        assert_eq!(tornado_wind_estimate_label("EF5"), Some("201+ mph"));
    }

    #[test]
    fn wcm_consolidated_rows_carry_the_surveyed_end_time() {
        let day = NaiveDate::from_ymd_opt(2011, 4, 27).unwrap();
        let header = "om,yr,mo,dy,date,time,tz,st,stf,stn,mag,inj,fat,loss,closs,slat,slon,elat,elon,len,wid,ns,sn,sg,f1,f2,f3,f4,fc,edat,etime\n";
        // The consolidated actual_tornadoes files append edat/etime
        // (CST, like the begin time): 14:05 -> 15:30 CST = 20:05 ->
        // 21:30Z. A second row with blank end columns stays None.
        let csv = format!(
            "{header}\
             309488,2011,4,27,2011-04-27,14:05:00,3,AL,1,0,5,72,31,0,0,34.1043,-88.1479,35.0857,-86.1511,67.8,1320,2,0,1,77,33,79,83,0,2011-04-27,15:30:00\n\
             310999,2011,4,27,2011-04-27,23:30:00,3,MS,28,0,2,0,0,0,0,32.5,-89.5,32.6,-89.4,8.0,100,1,1,1,89,0,0,0,0,,\n"
        );
        let parsed = parse_wcm_torn_segments(day, &csv);
        assert_eq!(parsed.len(), 2);
        assert_eq!(
            parsed[0].end_time_utc,
            Some(Utc.with_ymd_and_hms(2011, 4, 27, 21, 30, 0).unwrap())
        );
        assert_eq!(parsed[1].end_time_utc, None);
    }

    #[test]
    fn wcm_pre2007_rows_label_as_f_scale() {
        let day = NaiveDate::from_ymd_opt(1999, 5, 3).unwrap();
        let header = "om,yr,mo,dy,date,time,tz,st,stf,stn,mag,inj,fat,loss,closs,slat,slon,elat,elon,len,wid,ns,sn,sg,f1,f2,f3,f4,fc\n";
        let csv = format!(
            "{header}675,1999,5,3,1999-05-03,17:26:00,3,OK,40,53,5,583,36,0,0,34.89,-97.99,35.36,-97.42,38.0,1500,1,1,1,31,87,109,0,0\n"
        );
        let parsed = parse_wcm_torn_segments(day, &csv);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].ef_label, "F5");
    }

    /// Live validation against SPC — network required, run with
    /// `cargo test -p app_ui -- --ignored spc_live`.
    #[test]
    #[ignore = "network: fetches live SPC report + WCM files"]
    fn spc_live_event_days_fetch() {
        // 2026-06-11: the Illinois derecho day — dense reports, and no
        // WCM file for 2026 yet, so torn reports stand in as zero-length
        // segments.
        let derecho = NaiveDate::from_ymd_opt(2026, 6, 11).unwrap();
        let data = fetch_event_day(derecho).expect("fetch 2026-06-11");
        assert!(!data.reports_file_missing);
        assert!(
            data.reports.len() > 200,
            "expected a dense day, got {} reports",
            data.reports.len()
        );
        assert!(!data.segments.is_empty());
        println!(
            "2026-06-11: {} reports, {} segments ({} with tracks)",
            data.reports.len(),
            data.segments.len(),
            data.segments.iter().filter(|s| s.is_track()).count()
        );

        // 2011-04-27 (the historic outbreak): the filtered CSV is not
        // archived that far back (raw fallback) and the WCM database has
        // surveyed begin/end tracks.
        let outbreak = NaiveDate::from_ymd_opt(2011, 4, 27).unwrap();
        let data = fetch_event_day(outbreak).expect("fetch 2011-04-27");
        assert!(!data.reports_file_missing);
        assert!(data.reports.len() > 300);
        let tracks = data.segments.iter().filter(|s| s.is_track()).count();
        assert!(
            tracks > 100,
            "expected a hundred-plus surveyed tracks, got {tracks}"
        );
        println!(
            "2011-04-27: {} reports, {} segments ({tracks} with tracks)",
            data.reports.len(),
            data.segments.len()
        );

        // Pre-archive day: 404 on both CSVs must come back as a clean
        // "missing", never an error.
        let quiet = NaiveDate::from_ymd_opt(1999, 5, 3).unwrap();
        let data = fetch_event_day(quiet).expect("fetch 1999-05-03");
        assert!(data.reports_file_missing);
        assert!(data.reports.is_empty());
        // The WCM database still covers 1999 — Bridge Creek-Moore day.
        assert!(data.segments.iter().filter(|s| s.is_track()).count() > 30);
    }
}
