//! Surface observations layer — METAR station plots on the radar map.
//!
//! v1 source: aviationweather.gov's full METAR cache (one gzipped CSV,
//! every reporting station, no API key, updated ~minutely). Source
//! selection + QC thresholds follow the user's hrrr-mesoanalysis obs
//! pipeline (github.com/FahrenheitResearch/hrrr-mesoanalysis); the IEM
//! multi-network mesonet density (currents.json) is the planned v1.5.
//!
//! Plots draw GR2A-style: temperature (°F, red) upper-left, dewpoint
//! (°F, green) lower-left, wind barb at the station, gusts as "G##",
//! station id at high zoom. A screen-grid declutter keeps roughly one
//! station per cell, preferring fuller reports.

use chrono::{DateTime, Utc};
use std::io::Read;

const METAR_CACHE_URL: &str = "https://aviationweather.gov/data/cache/metars.cache.csv.gz";

/// One decoded surface observation (units as plotted).
#[derive(Clone, Debug)]
#[allow(dead_code)] // time/altimeter feed the upcoming inspector ob readout
pub struct SurfaceOb {
    pub station_id: String,
    pub time_utc: Option<DateTime<Utc>>,
    pub lat: f32,
    pub lon: f32,
    pub temp_c: Option<f32>,
    pub dewpoint_c: Option<f32>,
    pub wind_dir_deg: Option<f32>,
    pub wind_speed_kt: Option<f32>,
    pub wind_gust_kt: Option<f32>,
    pub altim_in_hg: Option<f32>,
    /// Field count — declutter priority (fuller reports win a cell).
    pub completeness: u8,
}

/// Fetch + decode the full METAR cache. Blocking — call on a worker
/// thread. QC per the mesoanalysis pipeline: finite/plausible coords
/// (the cache marks unknown locations as -99.99) and physical ranges
/// (T/Td in [-60, 55] °C, wind < 250 kt).
pub fn fetch_surface_obs() -> Result<Vec<SurfaceOb>, String> {
    let gz = data_source::fetch_bytes(METAR_CACHE_URL).map_err(|err| err.to_string())?;
    let mut text = String::new();
    flate2::read::GzDecoder::new(gz.as_slice())
        .read_to_string(&mut text)
        .map_err(|err| format!("gunzip: {err}"))?;
    parse_metar_cache(&text)
}

fn parse_metar_cache(text: &str) -> Result<Vec<SurfaceOb>, String> {
    // The cache has preamble lines; the header row starts with "raw_text".
    let mut columns: Option<Vec<&str>> = None;
    let mut obs = Vec::with_capacity(6000);
    for line in text.lines() {
        if columns.is_none() {
            if line.starts_with("raw_text") {
                columns = Some(line.split(',').collect());
            }
            continue;
        }
        let cols = columns.as_ref().unwrap();
        if let Some(ob) = parse_row(cols, line) {
            obs.push(ob);
        }
    }
    if columns.is_none() {
        return Err("METAR cache header not found".to_owned());
    }
    Ok(obs)
}

/// Split a CSV row honoring quotes (raw_text contains commas).
fn split_csv(line: &str) -> Vec<&str> {
    let mut fields = Vec::with_capacity(48);
    let bytes = line.as_bytes();
    let mut start = 0usize;
    let mut in_quotes = false;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'"' => in_quotes = !in_quotes,
            b',' if !in_quotes => {
                fields.push(line[start..i].trim_matches('"'));
                start = i + 1;
            }
            _ => {}
        }
    }
    fields.push(line[start..].trim_matches('"'));
    fields
}

fn parse_row(columns: &[&str], line: &str) -> Option<SurfaceOb> {
    let fields = split_csv(line);
    let get = |name: &str| -> Option<&str> {
        let index = columns.iter().position(|c| *c == name)?;
        fields.get(index).copied().filter(|v| !v.is_empty())
    };
    let f32_of = |name: &str| get(name).and_then(|v| v.parse::<f32>().ok());

    let lat = f32_of("latitude")?;
    let lon = f32_of("longitude")?;
    // Unknown locations are encoded as -99.99 in the cache.
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) || lat == -99.99 {
        return None;
    }
    let temp_c = f32_of("temp_c").filter(|t| (-60.0..=55.0).contains(t));
    let dewpoint_c = f32_of("dewpoint_c").filter(|t| (-60.0..=40.0).contains(t));
    let wind_speed_kt = f32_of("wind_speed_kt").filter(|w| (0.0..250.0).contains(w));
    let wind_dir_deg = f32_of("wind_dir_degrees").filter(|d| (0.0..=360.0).contains(d));
    let wind_gust_kt = f32_of("wind_gust_kt").filter(|w| (0.0..250.0).contains(w));
    let altim_in_hg = f32_of("altim_in_hg").filter(|a| (25.0..=33.0).contains(a));
    let completeness = temp_c.is_some() as u8
        + dewpoint_c.is_some() as u8
        + wind_speed_kt.is_some() as u8
        + altim_in_hg.is_some() as u8
        + wind_gust_kt.is_some() as u8;
    if completeness == 0 {
        return None;
    }
    let time_utc = get("observation_time")
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Utc));
    Some(SurfaceOb {
        station_id: get("station_id").unwrap_or("?").to_owned(),
        time_utc,
        lat,
        lon,
        temp_c,
        dewpoint_c,
        wind_dir_deg,
        wind_speed_kt,
        wind_gust_kt,
        altim_in_hg,
        completeness,
    })
}

/// Time-keyed observation pool: every fetch merges in (METARs are hourly
/// plus specials, so 5-minute fetches accumulate the in-between reports).
/// Radar frames then draw the ob valid AT THE FRAME'S TIME — obs scrub
/// in sync with the radar loop. Pruned beyond `RETAIN`.
pub struct ObPool {
    /// Per station, obs sorted ascending by time.
    by_station: std::collections::HashMap<String, Vec<SurfaceOb>>,
    pub station_count: usize,
}

/// Keep ~3 h of history (a dozen live-loop frames plus slack).
const RETAIN: chrono::Duration = chrono::Duration::hours(3);
/// A station with no ob within this window of the frame time is HIDDEN —
/// honest absence beats stale data masquerading as current.
const MATCH_WINDOW: chrono::Duration = chrono::Duration::minutes(90);

impl ObPool {
    pub fn new() -> Self {
        Self {
            by_station: std::collections::HashMap::new(),
            station_count: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.by_station.is_empty()
    }

    /// Merge a fetch (dedup by station+time), prune old, re-sort.
    pub fn merge(&mut self, fetched: Vec<SurfaceOb>) {
        let cutoff = Utc::now() - RETAIN;
        for ob in fetched {
            let entry = self.by_station.entry(ob.station_id.clone()).or_default();
            if !entry.iter().any(|have| have.time_utc == ob.time_utc) {
                entry.push(ob);
            }
        }
        for entry in self.by_station.values_mut() {
            entry.retain(|ob| ob.time_utc.map(|t| t > cutoff).unwrap_or(false));
            entry.sort_by_key(|ob| ob.time_utc);
        }
        self.by_station.retain(|_, list| !list.is_empty());
        self.station_count = self.by_station.len();
    }

    /// The ob to draw for `frame_time`: the newest report at-or-before
    /// (falling back to the nearest after, e.g. a special that just
    /// landed), within the match window. None = hide the station.
    pub fn ob_at<'a>(&'a self, station: &str, frame_time: DateTime<Utc>) -> Option<&'a SurfaceOb> {
        let list = self.by_station.get(station)?;
        let best = list
            .iter()
            .rfind(|ob| ob.time_utc.map(|t| t <= frame_time).unwrap_or(false))
            .or_else(|| list.first());
        best.filter(|ob| {
            ob.time_utc
                .map(|t| (frame_time - t).num_minutes().abs() <= MATCH_WINDOW.num_minutes())
                .unwrap_or(false)
        })
    }

    /// Iterate one representative ob per station for `frame_time`.
    pub fn frame_obs(&self, frame_time: DateTime<Utc>) -> impl Iterator<Item = &SurfaceOb> {
        self.by_station
            .keys()
            .filter_map(move |station| self.ob_at(station, frame_time))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cache_rows_with_quoted_commas() {
        let text = "No errors\nNo warnings\n2 ms\ndata source=metars\n5 results\nraw_text,station_id,observation_time,latitude,longitude,temp_c,dewpoint_c,wind_dir_degrees,wind_speed_kt,wind_gust_kt,visibility_statute_mi,altim_in_hg\n\"METAR KTST 110610Z 27005G13KT, RMK\",KTST,2026-06-11T06:10:00.000Z,39.1,-94.5,25,10,270,5,13,10,29.83\n\"BAD -99\",KBAD,2026-06-11T06:10:00.000Z,-99.99,-99.99,20,10,180,5,,10,29.90\n";
        let obs = parse_metar_cache(text).expect("parse");
        assert_eq!(obs.len(), 1, "QC drops the -99.99 row");
        let ob = &obs[0];
        assert_eq!(ob.station_id, "KTST");
        assert_eq!(ob.temp_c, Some(25.0));
        assert_eq!(ob.wind_gust_kt, Some(13.0));
        assert!(ob.completeness >= 4);
    }

    #[test]
    fn pool_serves_time_matched_obs() {
        use chrono::TimeZone;
        let t0 = Utc.with_ymd_and_hms(2100, 1, 1, 12, 0, 0).unwrap();
        let make = |minutes: i64, temp: f32| SurfaceOb {
            station_id: "KTST".into(),
            time_utc: Some(t0 + chrono::Duration::minutes(minutes)),
            lat: 39.0,
            lon: -94.0,
            temp_c: Some(temp),
            dewpoint_c: None,
            wind_dir_deg: None,
            wind_speed_kt: None,
            wind_gust_kt: None,
            altim_in_hg: None,
            completeness: 1,
        };
        let mut pool = ObPool::new();
        // NOTE: merge prunes vs wall clock; use raw insert semantics by
        // checking ob_at directly on a hand-built pool.
        pool.by_station.insert(
            "KTST".into(),
            vec![make(0, 10.0), make(30, 12.0), make(60, 14.0)],
        );
        // Frame at +35 min -> the +30 ob (newest at-or-before).
        let ob = pool
            .ob_at("KTST", t0 + chrono::Duration::minutes(35))
            .unwrap();
        assert_eq!(ob.temp_c, Some(12.0));
        // Frame 4 hours later -> outside the window, hidden.
        assert!(
            pool.ob_at("KTST", t0 + chrono::Duration::hours(4))
                .is_none()
        );
    }
}
