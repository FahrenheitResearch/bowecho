//! Surface observations layer — METAR station plots on the radar map.
//!
//! v1 source: aviationweather.gov's full METAR cache (one gzipped CSV,
//! every reporting station, no API key, updated ~minutely), with the
//! Iowa Environmental Mesonet's global ASOS/AWOS/METAR archive as a
//! slower no-key history/backfill source. Source selection + QC
//! thresholds follow the user's hrrr-mesoanalysis obs pipeline
//! (github.com/FahrenheitResearch/hrrr-mesoanalysis); IEM multi-network
//! mesonet density (currents.json) provides non-METAR CONUS density.
//!
//! Plots draw GR2A-style: temperature (red) upper-left, dewpoint
//! (green) lower-left — °F, or °C when Settings ▸ Display ▸ Units is
//! Metric — wind barb at the station, gusts as "G##", station id at
//! high zoom. A screen-grid declutter keeps roughly one station per
//! cell, preferring fuller reports.

use chrono::{DateTime, Datelike, Timelike, Utc};
use std::io::Read;

const METAR_CACHE_URL: &str = "https://aviationweather.gov/data/cache/metars.cache.csv.gz";
const IEM_ASOS_URL: &str = "https://mesonet.agron.iastate.edu/cgi-bin/request/asos.py";

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
    /// Source network ("METAR", "KS_RWIS", "TX_DCP", …) — feeds
    /// per-network observation-error weighting in the analysis.
    pub network: String,
    /// Station elevation, m MSL (METAR cache only; mesonets fall back
    /// to model terrain at their cell in the analysis).
    pub elevation_m: Option<f32>,
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

/// Fetch the IEM global ASOS/AWOS/METAR archive for the trailing two
/// hours. This is a single bounded CSV request (not per-station fanout)
/// and intentionally excludes high-frequency one-minute ASOS reports so
/// it behaves like a global METAR history/backfill source.
pub fn fetch_iem_global_metar_obs() -> Result<Vec<SurfaceOb>, String> {
    fetch_iem_asos_window("hours=2")
}

/// Fetch the IEM global ASOS/AWOS/METAR archive around a HISTORICAL
/// valid time: the window [when − 90 min, when + 30 min], wide enough
/// that every station's hourly report (plus specials) lands inside for
/// an ob pick gated to ±60 min of `when`. Same request/parse path as
/// the live trailing-window fetch, with asos.py's explicit
/// year1…minute2 UTC range params replacing `hours=2`.
pub fn fetch_iem_asos_obs_at(when: DateTime<Utc>) -> Result<Vec<SurfaceOb>, String> {
    let start = when - chrono::Duration::minutes(90);
    let end = when + chrono::Duration::minutes(30);
    fetch_iem_asos_window(&iem_asos_range_params(start, end))
}

/// One bounded asos.py CSV request. `window_params` is either the live
/// `hours=N` trailing window or an explicit year1…minute2 UTC range.
fn fetch_iem_asos_window(window_params: &str) -> Result<Vec<SurfaceOb>, String> {
    let url = format!(
        "{IEM_ASOS_URL}?data=tmpf&data=dwpf&data=drct&data=sknt&data=gust&data=alti&data=metar\
         &{window_params}&tz=Etc%2FUTC&format=onlycomma&latlon=yes&elev=yes&missing=empty&trace=T\
         &report_type=3&report_type=4"
    );
    let text = data_source::fetch_listing_text(&url).map_err(|err| err.to_string())?;
    parse_iem_asos_csv(&text)
}

/// asos.py historical range params (UTC, inclusive start / exclusive
/// end per the IEM download form's semantics).
fn iem_asos_range_params(start: DateTime<Utc>, end: DateTime<Utc>) -> String {
    format!(
        "year1={}&month1={}&day1={}&hour1={}&minute1={}\
         &year2={}&month2={}&day2={}&hour2={}&minute2={}",
        start.year(),
        start.month(),
        start.day(),
        start.hour(),
        start.minute(),
        end.year(),
        end.month(),
        end.day(),
        end.hour(),
        end.minute()
    )
}

#[derive(Clone, Copy, Debug)]
pub struct ObBounds {
    pub west: f32,
    pub south: f32,
    pub east: f32,
    pub north: f32,
}

pub fn fetch_nws_latest_obs(stations: Vec<(String, f32, f32)>) -> Vec<SurfaceOb> {
    let results = std::sync::Mutex::new(Vec::new());
    let queue = std::sync::Mutex::new(stations.into_iter());
    std::thread::scope(|scope| {
        for _ in 0..6 {
            scope.spawn(|| {
                loop {
                    let Some((station_id, fallback_lat, fallback_lon)) =
                        queue.lock().ok().and_then(|mut q| q.next())
                    else {
                        return;
                    };
                    if let Ok(ob) =
                        fetch_nws_latest_station_ob(&station_id, fallback_lat, fallback_lon)
                        && let Ok(mut all) = results.lock()
                    {
                        all.push(ob);
                    }
                }
            });
        }
    });
    results.into_inner().unwrap_or_default()
}

fn fetch_nws_latest_station_ob(
    station_id: &str,
    fallback_lat: f32,
    fallback_lon: f32,
) -> Result<SurfaceOb, String> {
    let url = format!("https://api.weather.gov/stations/{station_id}/observations/latest");
    let body = data_source::fetch_text(&url).map_err(|err| err.to_string())?;
    let parsed: serde_json::Value = serde_json::from_str(&body).map_err(|err| err.to_string())?;
    parse_nws_latest_ob(&parsed, station_id, fallback_lat, fallback_lon)
        .ok_or_else(|| format!("NWS latest observation for {station_id} had no plottable data"))
}

fn parse_nws_latest_ob(
    parsed: &serde_json::Value,
    fallback_station_id: &str,
    fallback_lat: f32,
    fallback_lon: f32,
) -> Option<SurfaceOb> {
    let properties = parsed.get("properties")?;
    let station_id = properties
        .get("station")
        .and_then(|station| station.as_str())
        .and_then(|station_url| station_url.rsplit('/').next())
        .filter(|station| !station.is_empty())
        .unwrap_or(fallback_station_id)
        .to_owned();
    let (lon, lat) = parsed
        .get("geometry")
        .and_then(|geometry| geometry.get("coordinates"))
        .and_then(|coordinates| coordinates.as_array())
        .and_then(|coordinates| {
            Some((
                coordinates.first()?.as_f64()? as f32,
                coordinates.get(1)?.as_f64()? as f32,
            ))
        })
        .filter(|(lon, lat)| (-180.0..=180.0).contains(lon) && (-90.0..=90.0).contains(lat))
        .unwrap_or((fallback_lon, fallback_lat));
    if !(-180.0..=180.0).contains(&lon) || !(-90.0..=90.0).contains(&lat) {
        return None;
    }

    let time_utc = properties
        .get("timestamp")
        .and_then(|timestamp| timestamp.as_str())
        .and_then(|timestamp| DateTime::parse_from_rfc3339(timestamp).ok())
        .map(|time| time.with_timezone(&Utc));
    let temp_c =
        nws_quantity(properties, "temperature").filter(|value| (-60.0..=55.0).contains(value));
    let dewpoint_c =
        nws_quantity(properties, "dewpoint").filter(|value| (-60.0..=40.0).contains(value));
    let wind_dir_deg =
        nws_quantity(properties, "windDirection").filter(|value| (0.0..=360.0).contains(value));
    let wind_speed_kt = nws_quantity(properties, "windSpeed")
        .map(kmh_to_kt)
        .filter(|value| (0.0..250.0).contains(value));
    let wind_gust_kt = nws_quantity(properties, "windGust")
        .map(kmh_to_kt)
        .filter(|value| (0.0..250.0).contains(value));
    let altim_in_hg = nws_quantity(properties, "barometricPressure")
        .map(pa_to_in_hg)
        .filter(|value| (25.0..=33.0).contains(value));
    let elevation_m = properties
        .get("elevation")
        .and_then(|elevation| elevation.get("value"))
        .and_then(|value| value.as_f64())
        .map(|value| value as f32)
        .filter(|value| (-430.0..=4500.0).contains(value));

    let completeness = temp_c.is_some() as u8
        + dewpoint_c.is_some() as u8
        + wind_speed_kt.is_some() as u8
        + altim_in_hg.is_some() as u8
        + wind_gust_kt.is_some() as u8;
    if completeness == 0 {
        return None;
    }

    Some(SurfaceOb {
        station_id,
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
        network: "METAR".to_owned(),
        elevation_m,
    })
}

fn nws_quantity(properties: &serde_json::Value, key: &str) -> Option<f32> {
    properties
        .get(key)
        .and_then(|quantity| quantity.get("value"))
        .and_then(|value| value.as_f64())
        .map(|value| value as f32)
}

fn kmh_to_kt(kmh: f32) -> f32 {
    kmh / 1.852
}

fn pa_to_in_hg(pa: f32) -> f32 {
    pa / 3386.389
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
        elevation_m: f32_of("elevation_m").filter(|e| (-430.0..=4500.0).contains(e)),
        network: "METAR".to_owned(),
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

fn parse_iem_asos_csv(text: &str) -> Result<Vec<SurfaceOb>, String> {
    let mut lines = text.lines().filter(|line| !line.trim().is_empty());
    let Some(header) = lines.next() else {
        return Err("IEM ASOS CSV empty".to_owned());
    };
    if !header.starts_with("station,") {
        return Err("IEM ASOS CSV header not found".to_owned());
    }
    let columns = header.split(',').collect::<Vec<_>>();
    let mut obs = Vec::new();
    for line in lines {
        if let Some(ob) = parse_iem_asos_row(&columns, line) {
            obs.push(ob);
        }
    }
    Ok(obs)
}

fn parse_iem_asos_row(columns: &[&str], line: &str) -> Option<SurfaceOb> {
    let fields = split_csv(line);
    let get = |name: &str| -> Option<&str> {
        let index = columns.iter().position(|c| *c == name)?;
        fields.get(index).copied().filter(|v| !v.is_empty())
    };
    let f32_of = |name: &str| get(name).and_then(|v| v.parse::<f32>().ok());

    let lat = f32_of("lat")?;
    let lon = f32_of("lon")?;
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return None;
    }
    let f_to_c = |deg_f: f32| (deg_f - 32.0) * 5.0 / 9.0;
    let temp_c = f32_of("tmpf")
        .map(f_to_c)
        .filter(|t| (-60.0..=55.0).contains(t));
    let dewpoint_c = f32_of("dwpf")
        .map(f_to_c)
        .filter(|t| (-60.0..=40.0).contains(t));
    let wind_dir_deg = f32_of("drct").filter(|d| (0.0..=360.0).contains(d));
    let wind_speed_kt = f32_of("sknt").filter(|w| (0.0..250.0).contains(w));
    let wind_gust_kt = f32_of("gust").filter(|w| (0.0..250.0).contains(w));
    let altim_in_hg = f32_of("alti").filter(|a| (25.0..=33.0).contains(a));
    let completeness = temp_c.is_some() as u8
        + dewpoint_c.is_some() as u8
        + wind_speed_kt.is_some() as u8
        + altim_in_hg.is_some() as u8
        + wind_gust_kt.is_some() as u8;
    if completeness == 0 {
        return None;
    }
    let time_utc = get("valid")
        .and_then(parse_iem_valid_time)
        .or_else(|| get("metar").and_then(parse_raw_metar_time));
    let fallback_station = get("station").unwrap_or("?");
    let station_id = station_id_from_raw_metar(get("metar").unwrap_or(""), fallback_station);
    Some(SurfaceOb {
        station_id,
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
        network: "METAR".to_owned(),
        elevation_m: f32_of("elevation").filter(|e| (-430.0..=4500.0).contains(e)),
    })
}

fn parse_iem_valid_time(value: &str) -> Option<DateTime<Utc>> {
    chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M"))
        .ok()
        .map(|naive| DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
}

fn station_id_from_raw_metar(raw: &str, fallback: &str) -> String {
    let mut tokens = raw.split_whitespace();
    let mut candidate = tokens.next().unwrap_or(fallback);
    if matches!(candidate, "METAR" | "SPECI") {
        candidate = tokens.next().unwrap_or(fallback);
    }
    if candidate == "COR" {
        candidate = tokens.next().unwrap_or(fallback);
    }
    let candidate = candidate.trim_matches(|ch: char| !ch.is_ascii_alphanumeric());
    if candidate.is_empty() {
        fallback.to_ascii_uppercase()
    } else {
        candidate.to_ascii_uppercase()
    }
}

fn parse_raw_metar_time(raw: &str) -> Option<DateTime<Utc>> {
    let token = raw
        .split_whitespace()
        .find(|token| token.len() == 7 && token.ends_with('Z'))?;
    let day = token.get(0..2)?.parse::<u32>().ok()?;
    let hour = token.get(2..4)?.parse::<u32>().ok()?;
    let minute = token.get(4..6)?.parse::<u32>().ok()?;
    let now = Utc::now();
    let mut candidates = Vec::new();
    for month_offset in -1..=1 {
        let date = now.date_naive();
        let month0 = date.month0() as i32 + month_offset;
        let year = date.year() + month0.div_euclid(12);
        let month = month0.rem_euclid(12) as u32 + 1;
        if let Some(candidate) = chrono::NaiveDate::from_ymd_opt(year, month, day)
            .and_then(|date| date.and_hms_opt(hour, minute, 0))
        {
            candidates.push(DateTime::<Utc>::from_naive_utc_and_offset(candidate, Utc));
        }
    }
    candidates
        .into_iter()
        .min_by_key(|candidate| (now - *candidate).num_seconds().abs())
}

// ---- IEM multi-network mesonet density (v1.5 source) ----
//
// Iowa Environmental Mesonet currents.json per network — RWIS road
// sensors + DCP (RAWS/ALERT/state networks via GOES). COOP skipped
// (rarely real-time, per the hrrr-mesoanalysis source notes). Fields
// are imperial (tmpf/dwpf °F, sknt kt).

const IEM_CURRENTS_URL: &str = "https://mesonet.agron.iastate.edu/api/1/currents.json";
const CONUS_STATES: [&str; 49] = [
    "AL", "AZ", "AR", "CA", "CO", "CT", "DE", "FL", "GA", "ID", "IL", "IN", "IA", "KS", "KY", "LA",
    "ME", "MD", "MA", "MI", "MN", "MS", "MO", "MT", "NE", "NV", "NH", "NJ", "NM", "NY", "NC", "ND",
    "OH", "OK", "OR", "PA", "RI", "SC", "SD", "TN", "TX", "UT", "VT", "VA", "WA", "WV", "WI", "WY",
    "DC",
];

/// Fetch RWIS + DCP networks for every CONUS state (98 small requests,
/// bounded concurrency). Blocking — worker thread only. Failures are
/// per-network and silent: a missing state never blocks the rest.
pub fn fetch_mesonet_obs() -> Vec<SurfaceOb> {
    let networks: Vec<String> = CONUS_STATES
        .iter()
        .flat_map(|st| ["RWIS", "DCP"].iter().map(move |sfx| format!("{st}_{sfx}")))
        .collect();
    let results = std::sync::Mutex::new(Vec::new());
    let queue = std::sync::Mutex::new(networks.into_iter());
    std::thread::scope(|scope| {
        for _ in 0..10 {
            scope.spawn(|| {
                loop {
                    let Some(network) = queue.lock().ok().and_then(|mut q| q.next()) else {
                        return;
                    };
                    if let Ok(obs) = fetch_network(&network)
                        && let Ok(mut all) = results.lock()
                    {
                        all.extend(obs);
                    }
                }
            });
        }
    });
    results.into_inner().unwrap_or_default()
}

fn fetch_network(network: &str) -> Result<Vec<SurfaceOb>, String> {
    let url = format!("{IEM_CURRENTS_URL}?network={network}");
    let body = data_source::fetch_text(&url).map_err(|err| err.to_string())?;
    let parsed: serde_json::Value = serde_json::from_str(&body).map_err(|err| err.to_string())?;
    let mut obs = Vec::new();
    let Some(rows) = parsed.get("data").and_then(|d| d.as_array()) else {
        return Ok(obs);
    };
    for row in rows {
        let f = |key: &str| row.get(key).and_then(|v| v.as_f64()).map(|v| v as f32);
        let Some(lat) = f("lat").filter(|v| (-90.0..=90.0).contains(v)) else {
            continue;
        };
        let Some(lon) = f("lon").filter(|v| (-180.0..=180.0).contains(v)) else {
            continue;
        };
        let station = row
            .get("station")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_owned();
        if station.is_empty() {
            continue;
        }
        let f_to_c = |deg_f: f32| (deg_f - 32.0) * 5.0 / 9.0;
        let temp_c = f("tmpf").map(f_to_c).filter(|t| (-60.0..=55.0).contains(t));
        let dewpoint_c = f("dwpf").map(f_to_c).filter(|t| (-60.0..=40.0).contains(t));
        let wind_speed_kt = f("sknt").filter(|w| (0.0..250.0).contains(w));
        let wind_dir_deg = f("drct").filter(|d| (0.0..=360.0).contains(d));
        let wind_gust_kt = f("gust").filter(|w| (0.0..250.0).contains(w));
        let altim_in_hg = f("alti").filter(|a| (25.0..=33.0).contains(a));
        let completeness = temp_c.is_some() as u8
            + dewpoint_c.is_some() as u8
            + wind_speed_kt.is_some() as u8
            + altim_in_hg.is_some() as u8
            + wind_gust_kt.is_some() as u8;
        if completeness == 0 {
            continue;
        }
        let time_utc = row
            .get("utc_valid")
            .and_then(|v| v.as_str())
            .and_then(|t| {
                // IEM emits a trailing Z ("2026-06-11T19:19:00Z") — the
                // missing-Z parse made time None, and the pool DROPS
                // timeless obs on merge (field report: mesonets-alone
                // showed nothing).
                let t = t.trim_end_matches('Z');
                chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M:%S")
                    .or_else(|_| chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%d %H:%M"))
                    .ok()
            })
            .map(|naive| DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc));
        obs.push(SurfaceOb {
            elevation_m: None,
            network: network.to_owned(),
            station_id: station,
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
        });
    }
    Ok(obs)
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
            if let Some(existing) = entry.iter_mut().find(|have| have.time_utc == ob.time_utc) {
                if ob.completeness > existing.completeness {
                    *existing = ob;
                }
            } else {
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

    /// How many reports this station has in the pool — ≥3 over the
    /// retention window means a sub-hourly updater (loops filter to
    /// these so scrubbing animates instead of freezing slow stations).
    #[allow(dead_code)] // superseded by frame-freshness loop filter; kept for diagnostics
    pub fn report_count(&self, station: &str) -> usize {
        self.by_station
            .get(station)
            .map(|list| list.len())
            .unwrap_or(0)
    }

    pub fn station_series(
        &self,
        station: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Vec<&SurfaceOb> {
        self.by_station
            .get(station)
            .into_iter()
            .flatten()
            .filter(|ob| {
                ob.time_utc
                    .map(|time| time >= start && time <= end)
                    .unwrap_or(false)
            })
            .collect()
    }

    pub fn latest_station_ob(&self, station: &str) -> Option<&SurfaceOb> {
        self.by_station.get(station).and_then(|list| list.last())
    }

    /// Iterate one representative ob per station for `frame_time`.
    pub fn frame_obs(&self, frame_time: DateTime<Utc>) -> impl Iterator<Item = &SurfaceOb> {
        self.by_station
            .keys()
            .filter_map(move |station| self.ob_at(station, frame_time))
    }

    /// Iterate one representative ob per station for `frame_time`, after a
    /// cheap station-location cull. This avoids resolving/drawing dense
    /// global obs pools for stations far outside the current map view.
    pub fn frame_obs_in_bounds(
        &self,
        frame_time: DateTime<Utc>,
        bounds: ObBounds,
    ) -> impl Iterator<Item = &SurfaceOb> {
        self.by_station.iter().filter_map(move |(station, list)| {
            let latest = list.last()?;
            if !lon_in_bounds(latest.lon, bounds.west, bounds.east)
                || latest.lat < bounds.south
                || latest.lat > bounds.north
            {
                return None;
            }
            self.ob_at(station, frame_time)
        })
    }

    pub fn stale_metars_in_bounds(
        &self,
        bounds: ObBounds,
        center: (f32, f32),
        now: DateTime<Utc>,
        stale_after: chrono::Duration,
        limit: usize,
    ) -> Vec<(String, f32, f32)> {
        let (center_lat, center_lon) = center;
        let mut candidates = Vec::new();
        for (station, list) in &self.by_station {
            let Some(latest) = list.last() else {
                continue;
            };
            if latest.network != "METAR" || !station.starts_with('K') || station.len() != 4 {
                continue;
            }
            if latest.lon < bounds.west
                || latest.lon > bounds.east
                || latest.lat < bounds.south
                || latest.lat > bounds.north
            {
                continue;
            }
            let Some(time_utc) = latest.time_utc else {
                continue;
            };
            if now - time_utc <= stale_after {
                continue;
            }
            let dx = (latest.lon - center_lon) * center_lat.to_radians().cos().abs().max(0.15);
            let dy = latest.lat - center_lat;
            candidates.push((
                dx.mul_add(dx, dy * dy),
                station.clone(),
                latest.lat,
                latest.lon,
            ));
        }
        candidates.sort_by(|left, right| left.0.total_cmp(&right.0));
        candidates
            .into_iter()
            .take(limit)
            .map(|(_, station, lat, lon)| (station, lat, lon))
            .collect()
    }
}

fn lon_in_bounds(lon: f32, west: f32, east: f32) -> bool {
    if west <= east {
        lon >= west && lon <= east
    } else {
        lon >= west || lon <= east
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
    fn parses_iem_global_metar_rows_and_normalizes_station_ids() {
        use chrono::TimeZone;
        let text = concat!(
            "station,valid,lon,lat,elevation,tmpf,dwpf,drct,sknt,gust,alti,metar\n",
            "OYE,2026-06-26 18:25,-90.4900,28.8700,-18.00,86.00,75.20,130.00,7.00,,30.13,KOYE 261825Z AUTO 13007KT 9SM 30/24 A3013 RMK A01\n",
            "EYKA,2026-06-26 18:20,24.0848,54.9639,77.00,71.60,57.20,20.00,3.00,,30.12,EYKA 261820Z 02003KT CAVOK 22/14 Q1020\n",
            "BAD,2026-06-26 18:20,199.0,54.0,10.00,70.00,50.00,20.00,3.00,,30.12,BAD 261820Z AUTO\n",
        );
        let obs = parse_iem_asos_csv(text).expect("IEM ASOS parse");
        assert_eq!(obs.len(), 2, "QC drops the impossible longitude row");

        let gulf = &obs[0];
        assert_eq!(gulf.station_id, "KOYE");
        assert_eq!(
            gulf.time_utc,
            Some(Utc.with_ymd_and_hms(2026, 6, 26, 18, 25, 0).unwrap())
        );
        assert!((gulf.temp_c.unwrap() - 30.0).abs() < 0.01);
        assert!((gulf.dewpoint_c.unwrap() - 24.0).abs() < 0.01);
        assert_eq!(gulf.wind_dir_deg, Some(130.0));
        assert_eq!(gulf.wind_speed_kt, Some(7.0));
        assert_eq!(gulf.altim_in_hg, Some(30.13));
        assert_eq!(gulf.network, "METAR");

        let lithuania = &obs[1];
        assert_eq!(lithuania.station_id, "EYKA");
        assert!((lithuania.temp_c.unwrap() - 22.0).abs() < 0.01);
        assert_eq!(lithuania.elevation_m, Some(77.0));
    }

    #[test]
    fn iem_asos_range_params_span_the_historical_window() {
        use chrono::TimeZone;
        // The archive-sounding window around 2020-05-16 22Z:
        // [when − 90 min, when + 30 min].
        let when = Utc.with_ymd_and_hms(2020, 5, 16, 22, 0, 0).unwrap();
        let params = iem_asos_range_params(
            when - chrono::Duration::minutes(90),
            when + chrono::Duration::minutes(30),
        );
        assert_eq!(
            params,
            "year1=2020&month1=5&day1=16&hour1=20&minute1=30\
             &year2=2020&month2=5&day2=16&hour2=22&minute2=30"
        );
    }

    #[test]
    fn raw_metar_station_ids_skip_report_prefixes() {
        assert_eq!(
            station_id_from_raw_metar("METAR COR KOKC 261800Z 18010KT", "OKC"),
            "KOKC"
        );
        assert_eq!(
            station_id_from_raw_metar("SPECI CYOW 261801Z 24012KT", "YOW"),
            "CYOW"
        );
        assert_eq!(station_id_from_raw_metar("", "dsm"), "DSM");
    }

    #[test]
    #[ignore]
    fn live_iem_global_metar_backfill_smoke() {
        let obs = fetch_iem_global_metar_obs().expect("fetch IEM global METARs");
        println!("IEM global METAR rows: {}", obs.len());
        assert!(obs.len() > 1000, "unexpectedly sparse IEM METAR pull");
        assert!(
            obs.iter().any(|ob| ob.lon > -20.0 && ob.lon < 60.0),
            "expected Europe/Africa longitude coverage"
        );
        assert!(
            obs.iter().any(|ob| ob.lat < 0.0),
            "expected Southern Hemisphere METAR coverage"
        );
    }

    /// End-to-end proof for the station-history timeline on REAL data:
    /// fetch the live global METAR window, pool it, then pull the busiest
    /// station's multi-report series — the numeric temp/dewpoint/wind/
    /// pressure track the sparkline draws. Needs >=2 timestamps so a line
    /// (not a lone dot) renders.
    #[test]
    #[ignore]
    fn live_station_series_feeds_timeline() {
        let mut pool = ObPool::new();
        pool.merge(fetch_iem_global_metar_obs().expect("fetch IEM global METARs"));
        // Busiest reporter in the pooled window.
        let (station, count) = pool
            .by_station
            .iter()
            .map(|(id, list)| (id.clone(), list.len()))
            .max_by_key(|(_, n)| *n)
            .expect("pool has at least one station");
        println!("busiest station {station}: {count} reports in window");
        assert!(
            count >= 2,
            "expected a sub-hourly reporter with >=2 obs for a timeline line"
        );
        let now = Utc::now();
        let series = pool.station_series(&station, now - chrono::Duration::hours(3), now);
        assert!(series.len() >= 2, "series should carry the pooled reports");
        // Times strictly ascending (the sparkline assumes it).
        for pair in series.windows(2) {
            assert!(
                pair[0].time_utc <= pair[1].time_utc,
                "series must be sorted"
            );
        }
        // At least one numeric channel present per report -> something to plot.
        assert!(
            series.iter().all(|ob| ob.temp_c.is_some()
                || ob.dewpoint_c.is_some()
                || ob.wind_speed_kt.is_some()
                || ob.altim_in_hg.is_some()),
            "every pooled report should have a plottable value"
        );
        for ob in &series {
            println!(
                "  {} T={:?} Td={:?} wind={:?} alt={:?}",
                ob.time_utc
                    .map(|t| t.format("%H:%MZ").to_string())
                    .unwrap_or_default(),
                ob.temp_c,
                ob.dewpoint_c,
                ob.wind_speed_kt,
                ob.altim_in_hg
            );
        }
    }

    #[test]
    fn parses_nws_latest_observation_units() {
        let value = serde_json::json!({
            "geometry": {"coordinates": [-97.6003, 35.3843]},
            "properties": {
                "station": "https://api.weather.gov/stations/KOKC",
                "timestamp": "2026-06-14T23:20:00+00:00",
                "temperature": {"unitCode": "wmoUnit:degC", "value": 25.0},
                "dewpoint": {"unitCode": "wmoUnit:degC", "value": 14.0},
                "windDirection": {"unitCode": "wmoUnit:degree_(angle)", "value": 50.0},
                "windSpeed": {"unitCode": "wmoUnit:km_h-1", "value": 20.376},
                "windGust": {"unitCode": "wmoUnit:km_h-1", "value": null},
                "barometricPressure": {"unitCode": "wmoUnit:Pa", "value": 101760.98},
                "elevation": {"unitCode": "wmoUnit:m", "value": 391.0}
            }
        });
        let ob = parse_nws_latest_ob(&value, "KOKC", 0.0, 0.0).expect("NWS ob");
        assert_eq!(ob.station_id, "KOKC");
        assert_eq!(ob.network, "METAR");
        assert_eq!(ob.temp_c, Some(25.0));
        assert_eq!(ob.dewpoint_c, Some(14.0));
        assert_eq!(ob.wind_dir_deg, Some(50.0));
        assert!((ob.wind_speed_kt.unwrap() - 11.0).abs() < 0.01);
        assert!((ob.altim_in_hg.unwrap() - 30.05).abs() < 0.01);
        assert_eq!(ob.elevation_m, Some(391.0));
    }

    #[test]
    fn pool_serves_time_matched_obs() {
        use chrono::TimeZone;
        let t0 = Utc.with_ymd_and_hms(2100, 1, 1, 12, 0, 0).unwrap();
        let make = |minutes: i64, temp: f32| SurfaceOb {
            elevation_m: None,
            network: "METAR".into(),
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

    #[test]
    fn pool_returns_station_series_for_chart_window() {
        use chrono::TimeZone;
        let t0 = Utc.with_ymd_and_hms(2100, 1, 1, 12, 0, 0).unwrap();
        let make = |minutes: i64, temp: f32| SurfaceOb {
            elevation_m: None,
            network: "METAR".into(),
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
        pool.by_station.insert(
            "KTST".into(),
            vec![make(-90, 8.0), make(-30, 10.0), make(0, 12.0)],
        );

        let series = pool.station_series("KTST", t0 - chrono::Duration::hours(1), t0);

        assert_eq!(series.len(), 2);
        assert_eq!(series[0].temp_c, Some(10.0));
        assert_eq!(series[1].temp_c, Some(12.0));
    }

    #[test]
    fn pool_replaces_same_time_with_more_complete_ob() {
        let now = Utc::now();
        let mut weak = test_ob("KAAA", now, 35.0, -97.0);
        weak.dewpoint_c = None;
        weak.wind_speed_kt = None;
        weak.altim_in_hg = None;
        weak.completeness = 1;
        let mut better = weak.clone();
        better.dewpoint_c = Some(20.0);
        better.wind_speed_kt = Some(12.0);
        better.altim_in_hg = Some(30.02);
        better.completeness = 4;

        let mut pool = ObPool::new();
        pool.merge(vec![weak]);
        pool.merge(vec![better]);
        let ob = pool.ob_at("KAAA", now).expect("merged ob");
        assert_eq!(ob.completeness, 4);
        assert_eq!(ob.dewpoint_c, Some(20.0));
    }

    #[test]
    fn stale_metar_candidates_are_visible_airports_nearest_first() {
        let now = Utc::now();
        let mut pool = ObPool::new();
        pool.by_station.insert(
            "KOKC".into(),
            vec![test_ob(
                "KOKC",
                now - chrono::Duration::minutes(30),
                35.1,
                -97.1,
            )],
        );
        pool.by_station.insert(
            "KFAR".into(),
            vec![test_ob(
                "KFAR",
                now - chrono::Duration::minutes(30),
                36.5,
                -98.5,
            )],
        );
        pool.by_station.insert(
            "KFRESH".into(),
            vec![test_ob(
                "KFRESH",
                now - chrono::Duration::minutes(5),
                35.2,
                -97.2,
            )],
        );
        let mut mesonet = test_ob("MNET", now - chrono::Duration::minutes(30), 35.0, -97.0);
        mesonet.network = "OK_DCP".into();
        pool.by_station.insert("MNET".into(), vec![mesonet]);

        let candidates = pool.stale_metars_in_bounds(
            ObBounds {
                west: -99.0,
                south: 34.0,
                east: -96.0,
                north: 37.0,
            },
            (35.0, -97.0),
            now,
            chrono::Duration::minutes(18),
            8,
        );
        assert_eq!(
            candidates
                .iter()
                .map(|(station, ..)| station.as_str())
                .collect::<Vec<_>>(),
            vec!["KOKC", "KFAR"]
        );
    }

    fn test_ob(station: &str, time_utc: DateTime<Utc>, lat: f32, lon: f32) -> SurfaceOb {
        SurfaceOb {
            station_id: station.to_owned(),
            time_utc: Some(time_utc),
            lat,
            lon,
            temp_c: Some(25.0),
            dewpoint_c: Some(15.0),
            wind_dir_deg: Some(180.0),
            wind_speed_kt: Some(10.0),
            wind_gust_kt: None,
            altim_in_hg: Some(30.0),
            completeness: 4,
            network: "METAR".to_owned(),
            elevation_m: None,
        }
    }
}
