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
    /// Base colors as SPC publishes them (opaque); draw code applies the
    /// style registry's outlook alphas.
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
        let fill = hex_color(props["fill"].as_str().unwrap_or(""));
        let stroke = hex_color(props["stroke"].as_str().unwrap_or(""));
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
            None => live_outlook_urls(day, kind, Utc::now())
                .into_iter()
                .find_map(|url| data_source::fetch_text(&url).ok()),
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
