//! Anonymous NOAA/NWS MADIS aircraft-profile soundings.
//!
//! This is deliberately narrower than "all AMDAR": the public real-time
//! `acarsProfiles/netcdf` directory exposes a limited aircraft subset (largely
//! WVSS-II-equipped aircraft). Airline-restricted observations are not part of
//! that anonymous live subset and only become public after their delay.
//!
//! Files are hourly gzip-wrapped classic NetCDF (CDF-1 today). MADIS provides
//! pressure altitude rather than observed pressure, so BowEcho derives pressure
//! from the ICAO standard atmosphere and marks that fact on every sounding.

use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, TimeZone, Timelike, Utc};
use nexrad_io::netcdf3::{Nc3File, NcArray};

pub const SOURCE_NAME: &str = "NOAA/NWS MADIS aircraft profiles";
pub const SOURCE_URL: &str = "https://madis.ncep.noaa.gov/madis_acars.shtml";
const PUBLIC_BASE_URL: &str =
    "https://madis-data.ncep.noaa.gov/madisPublic1/data/point/acarsProfiles/netcdf";
const MAX_GZIP_BYTES: usize = 16 * 1024 * 1024;
const MAX_DECODED_BYTES: usize = 64 * 1024 * 1024;
const FILE_LOOKBACK_HOURS: i64 = 6;
const CACHE_KEEP_FILES: usize = 12;
const MAX_RECENT_PROFILES: usize = 512;

#[derive(Clone, Debug, PartialEq)]
pub struct AircraftProfile {
    pub airport: String,
    pub latitude: f32,
    pub longitude: f32,
    /// Reported level-by-level flight path in source order. MADIS does not
    /// expose a stable aircraft identity in this anonymous hourly feed, so
    /// this is a profile trajectory rather than a continuous live track.
    pub track: Vec<(f32, f32)>,
    pub valid_time: DateTime<Utc>,
    pub ascending: bool,
    pub source: String,
    pub pressure_is_derived: bool,
    pub column: rustwx_sounding::SoundingColumn,
}

impl AircraftProfile {
    pub fn direction_label(&self) -> &'static str {
        if self.ascending { "ascent" } else { "descent" }
    }

    pub fn display_id(&self) -> String {
        format!("{} {}", self.airport, self.direction_label())
    }

    pub fn marker_position(&self) -> (f32, f32) {
        self.track
            .last()
            .copied()
            .unwrap_or((self.latitude, self.longitude))
    }
}

/// Case-insensitive all-terms filter shared by the current-profile and recent
/// history browsers. The anonymous feed reliably exposes airport code,
/// direction, source, and UTC valid time; it does not provide dependable full
/// airport names or a stable aircraft identity.
pub fn profile_matches_search(profile: &AircraftProfile, query: &str) -> bool {
    let query = query.trim().to_ascii_lowercase();
    if query.is_empty() {
        return true;
    }
    let haystack = format!(
        "{} {} {} {} {}",
        profile.airport,
        profile.direction_label(),
        profile.source,
        profile.valid_time.to_rfc3339(),
        profile.valid_time.format("%Y%m%d %H%MZ")
    )
    .to_ascii_lowercase();
    query.split_whitespace().all(|term| haystack.contains(term))
}

#[derive(Clone, Debug)]
pub struct AircraftSnapshot {
    pub profiles: Vec<AircraftProfile>,
    pub file_hour: DateTime<Utc>,
    pub file_name: String,
}

#[derive(Clone, Debug)]
pub struct AircraftHistorySnapshot {
    pub profiles: Vec<AircraftProfile>,
    pub newest_hour: DateTime<Utc>,
    pub oldest_hour: DateTime<Utc>,
    pub files_loaded: usize,
}

#[derive(Debug)]
struct MadisArrays {
    records: usize,
    max_levels: usize,
    airport_width: usize,
    profile_time: Vec<f64>,
    profile_airport: Vec<u8>,
    n_levels: Vec<f64>,
    profile_type: Vec<f64>,
    data_source: Vec<f64>,
    data_source_labels: Vec<(i32, String)>,
    latitude: Vec<f64>,
    longitude: Vec<f64>,
    track_lat: Vec<f64>,
    track_lon: Vec<f64>,
    altitude: Vec<f64>,
    temperature: Vec<f64>,
    dewpoint: Vec<f64>,
    wind_dir: Vec<f64>,
    wind_speed: Vec<f64>,
    altitude_qc: Vec<u8>,
    temperature_qc: Vec<u8>,
    dewpoint_qc: Vec<u8>,
    wind_dir_qc: Vec<u8>,
    wind_speed_qc: Vec<u8>,
}

/// Fetch the newest public hourly file, falling back through the previous six
/// hours and to the on-disk cache. Blocking; call on a worker thread.
pub fn fetch_latest(cache_dir: &Path, now: DateTime<Utc>) -> Result<AircraftSnapshot, String> {
    fs::create_dir_all(cache_dir)
        .map_err(|error| format!("create MADIS aircraft cache: {error}"))?;
    let hour = now
        .with_minute(0)
        .and_then(|time| time.with_second(0))
        .and_then(|time| time.with_nanosecond(0))
        .ok_or("round MADIS request time")?;
    let mut failures = Vec::new();
    for offset in 0..FILE_LOOKBACK_HOURS {
        let candidate = hour - Duration::hours(offset);
        let file_name = format!("{}.gz", candidate.format("%Y%m%d_%H00"));
        let cache_path = cache_dir.join(&file_name);
        let gzip = match fs::read(&cache_path) {
            Ok(bytes) => bytes,
            Err(_) => {
                let url = format!("{PUBLIC_BASE_URL}/{file_name}");
                match data_source::fetch_bytes(&url) {
                    Ok(bytes) => {
                        if bytes.len() > MAX_GZIP_BYTES {
                            failures.push(format!("{file_name}: compressed file too large"));
                            continue;
                        }
                        if let Err(error) = fs::write(&cache_path, &bytes) {
                            failures.push(format!("{file_name}: cache write failed: {error}"));
                        }
                        bytes
                    }
                    Err(error) => {
                        failures.push(format!("{file_name}: {error}"));
                        continue;
                    }
                }
            }
        };
        match parse_gzip_netcdf(&gzip) {
            Ok(profiles) if !profiles.is_empty() => {
                prune_cache(cache_dir);
                return Ok(AircraftSnapshot {
                    profiles: latest_profile_per_airport(profiles),
                    file_hour: candidate,
                    file_name,
                });
            }
            Ok(_) => failures.push(format!("{file_name}: no profiles passed MADIS QC")),
            Err(error) => failures.push(format!("{file_name}: {error}")),
        }
    }
    Err(format!(
        "no usable public MADIS aircraft-profile file: {}",
        failures
            .last()
            .cloned()
            .unwrap_or_else(|| "no candidates".to_owned())
    ))
}

/// Fetch and merge the bounded public history window on explicit user
/// request. This is deliberately separate from [`fetch_latest`]: opening the
/// live map layer stays one-file cheap, while the history browser may read up
/// to six hourly files on its own worker. Duplicate rolling-file records are
/// collapsed by airport, valid time, and ascent/descent identity.
pub fn fetch_recent_history(
    cache_dir: &Path,
    now: DateTime<Utc>,
) -> Result<AircraftHistorySnapshot, String> {
    fs::create_dir_all(cache_dir)
        .map_err(|error| format!("create MADIS aircraft cache: {error}"))?;
    let newest_hour = rounded_hour(now)?;
    let mut oldest_hour = newest_hour;
    let mut files_loaded = 0usize;
    let mut profiles = Vec::new();
    let mut failures = Vec::new();
    for offset in 0..FILE_LOOKBACK_HOURS {
        let candidate = newest_hour - Duration::hours(offset);
        match fetch_hour_profiles(cache_dir, candidate) {
            Ok(hour_profiles) if !hour_profiles.is_empty() => {
                oldest_hour = candidate;
                files_loaded += 1;
                profiles.extend(hour_profiles);
            }
            Ok(_) => failures.push(format!(
                "{}: no profiles passed MADIS QC",
                candidate.format("%Y%m%d_%H00.gz")
            )),
            Err(error) => failures.push(error),
        }
    }
    let profiles = normalize_recent_profiles(profiles);
    if profiles.is_empty() {
        return Err(format!(
            "no usable public MADIS aircraft-profile history: {}",
            failures
                .last()
                .cloned()
                .unwrap_or_else(|| "no candidates".to_owned())
        ));
    }
    prune_cache(cache_dir);
    Ok(AircraftHistorySnapshot {
        profiles,
        newest_hour,
        oldest_hour,
        files_loaded,
    })
}

fn rounded_hour(now: DateTime<Utc>) -> Result<DateTime<Utc>, String> {
    now.with_minute(0)
        .and_then(|time| time.with_second(0))
        .and_then(|time| time.with_nanosecond(0))
        .ok_or_else(|| "round MADIS request time".to_owned())
}

fn fetch_hour_profiles(
    cache_dir: &Path,
    candidate: DateTime<Utc>,
) -> Result<Vec<AircraftProfile>, String> {
    let file_name = format!("{}.gz", candidate.format("%Y%m%d_%H00"));
    let cache_path = cache_dir.join(&file_name);
    let gzip = match fs::read(&cache_path) {
        Ok(bytes) => bytes,
        Err(_) => {
            let url = format!("{PUBLIC_BASE_URL}/{file_name}");
            let bytes =
                data_source::fetch_bytes(&url).map_err(|error| format!("{file_name}: {error}"))?;
            if bytes.len() > MAX_GZIP_BYTES {
                return Err(format!("{file_name}: compressed file too large"));
            }
            if let Err(error) = fs::write(&cache_path, &bytes) {
                // The requested history is still usable in memory; keep the
                // cache failure attached only when decoding itself fails.
                let decoded = parse_gzip_netcdf(&bytes).map_err(|decode| {
                    format!("{file_name}: {decode}; cache write failed: {error}")
                })?;
                return Ok(decoded);
            }
            bytes
        }
    };
    parse_gzip_netcdf(&gzip).map_err(|error| format!("{file_name}: {error}"))
}

fn normalize_recent_profiles(mut profiles: Vec<AircraftProfile>) -> Vec<AircraftProfile> {
    profiles.sort_by_key(|profile| std::cmp::Reverse(profile.valid_time));
    let mut seen = HashSet::new();
    profiles.retain(|profile| {
        seen.insert((
            profile.airport.clone(),
            profile.valid_time.timestamp(),
            profile.ascending,
        ))
    });
    profiles.truncate(MAX_RECENT_PROFILES);
    profiles
}

pub fn cache_dir(root: &Path) -> PathBuf {
    root.join("madis-aircraft-profiles")
}

fn prune_cache(cache_dir: &Path) {
    let Ok(entries) = fs::read_dir(cache_dir) else {
        return;
    };
    let mut files = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "gz"))
        .collect::<Vec<_>>();
    files.sort();
    let remove_count = files.len().saturating_sub(CACHE_KEEP_FILES);
    for path in files.into_iter().take(remove_count) {
        let _ = fs::remove_file(path);
    }
}

pub fn parse_gzip_netcdf(gzip: &[u8]) -> Result<Vec<AircraftProfile>, String> {
    if gzip.len() > MAX_GZIP_BYTES {
        return Err("compressed MADIS aircraft file exceeds 16 MiB".to_owned());
    }
    let mut decoder = flate2::read::GzDecoder::new(gzip);
    let mut decoded = Vec::new();
    decoder
        .by_ref()
        .take((MAX_DECODED_BYTES + 1) as u64)
        .read_to_end(&mut decoded)
        .map_err(|error| format!("gunzip MADIS aircraft file: {error}"))?;
    if decoded.len() > MAX_DECODED_BYTES {
        return Err("decoded MADIS aircraft file exceeds 64 MiB".to_owned());
    }
    let file = Nc3File::open(&decoded).map_err(|error| format!("open CDF-1: {error}"))?;
    let arrays = arrays_from_netcdf(&file)?;
    profiles_from_arrays(&arrays)
}

fn arrays_from_netcdf(file: &Nc3File<'_>) -> Result<MadisArrays, String> {
    let title = file.gattr_str("title").unwrap_or_default();
    if !title.to_ascii_uppercase().contains("MADIS ACARS PROFILE") {
        return Err(format!("unexpected NetCDF title {title:?}"));
    }
    let (records, max_levels) = profile_shape(file, "altitude")?;
    let airport_dims = variable_shape(file, "profileAirport")?;
    if airport_dims.len() != 2 || airport_dims[0] != records || airport_dims[1] == 0 {
        return Err(format!(
            "profileAirport shape {airport_dims:?} does not match {records} records"
        ));
    }
    let airport_width = airport_dims[1];
    let data_source_labels = file
        .vars
        .get("dataSource")
        .map(|variable| {
            variable
                .attrs
                .iter()
                .filter_map(|(name, value)| {
                    let code = name.strip_prefix("value_")?.parse::<i32>().ok()?;
                    Some((code, value.as_str()?.trim().to_owned()))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let arrays = MadisArrays {
        records,
        max_levels,
        airport_width,
        profile_time: numeric_var(file, "profileTime")?,
        profile_airport: char_var(file, "profileAirport")?,
        n_levels: numeric_var(file, "nLevels")?,
        profile_type: numeric_var(file, "profileType")?,
        data_source: numeric_var(file, "dataSource")?,
        data_source_labels,
        latitude: numeric_var(file, "latitude")?,
        longitude: numeric_var(file, "longitude")?,
        track_lat: numeric_profile_var(file, "trackLat", records, max_levels)?,
        track_lon: numeric_profile_var(file, "trackLon", records, max_levels)?,
        altitude: numeric_profile_var(file, "altitude", records, max_levels)?,
        temperature: numeric_profile_var(file, "temperature", records, max_levels)?,
        dewpoint: numeric_profile_var(file, "dewpoint", records, max_levels)?,
        wind_dir: numeric_profile_var(file, "windDir", records, max_levels)?,
        wind_speed: numeric_profile_var(file, "windSpeed", records, max_levels)?,
        altitude_qc: char_profile_var(file, "altitudeDD", records, max_levels)?,
        temperature_qc: char_profile_var(file, "temperatureDD", records, max_levels)?,
        dewpoint_qc: char_profile_var(file, "dewpointDD", records, max_levels)?,
        wind_dir_qc: char_profile_var(file, "windDirDD", records, max_levels)?,
        wind_speed_qc: char_profile_var(file, "windSpeedDD", records, max_levels)?,
    };
    for (name, len) in [
        ("profileTime", arrays.profile_time.len()),
        ("nLevels", arrays.n_levels.len()),
        ("profileType", arrays.profile_type.len()),
        ("dataSource", arrays.data_source.len()),
        ("latitude", arrays.latitude.len()),
        ("longitude", arrays.longitude.len()),
    ] {
        if len < records {
            return Err(format!("{name} has {len} values for {records} records"));
        }
    }
    if arrays.profile_airport.len() < records * airport_width {
        return Err("profileAirport character array is truncated".to_owned());
    }
    Ok(arrays)
}

fn variable_shape(file: &Nc3File<'_>, name: &str) -> Result<Vec<usize>, String> {
    let variable = file
        .vars
        .get(name)
        .ok_or_else(|| format!("MADIS variable {name} is absent"))?;
    Ok(file.var_dims(variable))
}

fn profile_shape(file: &Nc3File<'_>, name: &str) -> Result<(usize, usize), String> {
    let dims = variable_shape(file, name)?;
    match dims.as_slice() {
        [records, levels] if *records > 0 && *levels > 0 => Ok((*records, *levels)),
        _ => Err(format!(
            "MADIS variable {name} has unsupported shape {dims:?}"
        )),
    }
}

fn numeric_var(file: &Nc3File<'_>, name: &str) -> Result<Vec<f64>, String> {
    let values = file
        .read_var(name)
        .map_err(|error| format!("read MADIS {name}: {error}"))?;
    (0..values.len())
        .map(|index| {
            values
                .get_f64(index)
                .ok_or_else(|| format!("MADIS {name} is not numeric"))
        })
        .collect()
}

fn numeric_profile_var(
    file: &Nc3File<'_>,
    name: &str,
    records: usize,
    max_levels: usize,
) -> Result<Vec<f64>, String> {
    if profile_shape(file, name)? != (records, max_levels) {
        return Err(format!(
            "MADIS {name} profile shape disagrees with altitude"
        ));
    }
    numeric_var(file, name)
}

fn char_var(file: &Nc3File<'_>, name: &str) -> Result<Vec<u8>, String> {
    match file
        .read_var(name)
        .map_err(|error| format!("read MADIS {name}: {error}"))?
    {
        NcArray::Char(values) => Ok(values),
        _ => Err(format!("MADIS {name} is not NC_CHAR")),
    }
}

fn char_profile_var(
    file: &Nc3File<'_>,
    name: &str,
    records: usize,
    max_levels: usize,
) -> Result<Vec<u8>, String> {
    if profile_shape(file, name)? != (records, max_levels) {
        return Err(format!(
            "MADIS {name} profile shape disagrees with altitude"
        ));
    }
    char_var(file, name)
}

fn profiles_from_arrays(arrays: &MadisArrays) -> Result<Vec<AircraftProfile>, String> {
    let expected = arrays.records.saturating_mul(arrays.max_levels);
    for (name, len) in [
        ("trackLat", arrays.track_lat.len()),
        ("trackLon", arrays.track_lon.len()),
        ("altitude", arrays.altitude.len()),
        ("temperature", arrays.temperature.len()),
        ("dewpoint", arrays.dewpoint.len()),
        ("windDir", arrays.wind_dir.len()),
        ("windSpeed", arrays.wind_speed.len()),
        ("altitudeDD", arrays.altitude_qc.len()),
        ("temperatureDD", arrays.temperature_qc.len()),
        ("dewpointDD", arrays.dewpoint_qc.len()),
        ("windDirDD", arrays.wind_dir_qc.len()),
        ("windSpeedDD", arrays.wind_speed_qc.len()),
    ] {
        if len < expected {
            return Err(format!(
                "MADIS {name} array is truncated ({len} < {expected})"
            ));
        }
    }

    let mut profiles = Vec::new();
    for record in 0..arrays.records {
        let Some(valid_time) = timestamp(arrays.profile_time[record]) else {
            continue;
        };
        let airport_start = record * arrays.airport_width;
        let airport_end = airport_start + arrays.airport_width;
        let airport = String::from_utf8_lossy(&arrays.profile_airport[airport_start..airport_end])
            .trim_matches(['\0', ' '])
            .to_ascii_uppercase();
        if airport.is_empty() {
            continue;
        }
        let n_levels = arrays.n_levels[record]
            .round()
            .clamp(0.0, arrays.max_levels as f64) as usize;
        let mut raw = Vec::new();
        for level in 0..n_levels {
            let index = record * arrays.max_levels + level;
            if ![
                arrays.altitude_qc[index],
                arrays.temperature_qc[index],
                arrays.dewpoint_qc[index],
                arrays.wind_dir_qc[index],
                arrays.wind_speed_qc[index],
            ]
            .into_iter()
            .all(madis_qc_usable)
            {
                continue;
            }
            let altitude = arrays.altitude[index];
            let temperature_k = arrays.temperature[index];
            let dewpoint_k = arrays.dewpoint[index];
            let direction = arrays.wind_dir[index];
            let speed = arrays.wind_speed[index];
            if !altitude.is_finite()
                || !(-500.0..=25_000.0).contains(&altitude)
                || !temperature_k.is_finite()
                || !(180.0..=330.0).contains(&temperature_k)
                || !dewpoint_k.is_finite()
                || !(150.0..=330.0).contains(&dewpoint_k)
                || dewpoint_k > temperature_k + 0.5
                || !direction.is_finite()
                || !(0.0..360.0).contains(&direction)
                || !speed.is_finite()
                || !(0.0..=150.0).contains(&speed)
            {
                continue;
            }
            let pressure_hpa = pressure_from_pressure_altitude(altitude);
            if !pressure_hpa.is_finite() || !(20.0..=1100.0).contains(&pressure_hpa) {
                continue;
            }
            raw.push((
                altitude,
                pressure_hpa,
                temperature_k - 273.15,
                dewpoint_k.min(temperature_k) - 273.15,
                direction,
                speed,
            ));
        }
        raw.sort_by(|left, right| left.0.total_cmp(&right.0));
        raw.dedup_by(|left, right| (left.0 - right.0).abs() < 0.5);
        if raw.len() < 10 {
            continue;
        }

        let track_start = record * arrays.max_levels;
        let track_end = track_start + n_levels;
        let mut track = arrays.track_lat[track_start..track_end]
            .iter()
            .zip(&arrays.track_lon[track_start..track_end])
            .filter(|(lat, lon)| valid_coordinates(**lat, **lon))
            .map(|(lat, lon)| (*lat as f32, *lon as f32))
            .collect::<Vec<_>>();
        track.dedup_by(|left, right| {
            (left.0 - right.0).abs() < 0.0001 && (left.1 - right.1).abs() < 0.0001
        });

        let mut latitude = arrays.latitude[record];
        let mut longitude = arrays.longitude[record];
        if !valid_coordinates(latitude, longitude)
            && let Some((lat, lon)) = track.first().copied()
        {
            latitude = lat as f64;
            longitude = lon as f64;
        }
        if !valid_coordinates(latitude, longitude) {
            continue;
        }

        let mut pressure_hpa = Vec::with_capacity(raw.len());
        let mut height_m_msl = Vec::with_capacity(raw.len());
        let mut temperature_c = Vec::with_capacity(raw.len());
        let mut dewpoint_c = Vec::with_capacity(raw.len());
        let mut u_ms = Vec::with_capacity(raw.len());
        let mut v_ms = Vec::with_capacity(raw.len());
        for (altitude, pressure, temperature, dewpoint, direction, speed) in raw {
            let radians = direction.to_radians();
            pressure_hpa.push(pressure);
            height_m_msl.push(altitude);
            temperature_c.push(temperature);
            dewpoint_c.push(dewpoint);
            u_ms.push(-speed * radians.sin());
            v_ms.push(-speed * radians.cos());
        }
        let source_code = arrays.data_source[record].round() as i32;
        let source = arrays
            .data_source_labels
            .iter()
            .find_map(|(code, label)| (*code == source_code).then_some(label.clone()))
            .unwrap_or_else(|| format!("MADIS source {source_code}"));
        let station_id = format!("{airport} MADIS aircraft profile (pressure-altitude p)");
        let column = rustwx_sounding::SoundingColumn {
            pressure_hpa,
            height_m_msl,
            temperature_c,
            dewpoint_c,
            u_ms,
            v_ms,
            omega_pa_s: vec![0.0; n_levels],
            metadata: rustwx_sounding::SoundingMetadata {
                station_id,
                valid_time: valid_time.format("%Y-%m-%d %H:%MZ").to_string(),
                latitude_deg: Some(latitude),
                longitude_deg: Some(longitude),
                ..Default::default()
            },
        };
        // `n_levels` is the raw count; duplicate/QC filtering can shrink it.
        let mut column = column;
        column.omega_pa_s.resize(column.pressure_hpa.len(), 0.0);
        if column.validate().is_err() {
            continue;
        }
        profiles.push(AircraftProfile {
            airport,
            latitude: latitude as f32,
            longitude: longitude as f32,
            track,
            valid_time,
            ascending: arrays.profile_type[record] >= 0.0,
            source,
            pressure_is_derived: true,
            column,
        });
    }
    Ok(profiles)
}

fn timestamp(seconds: f64) -> Option<DateTime<Utc>> {
    if !seconds.is_finite() || seconds <= 0.0 || seconds > i64::MAX as f64 {
        return None;
    }
    Utc.timestamp_opt(seconds.round() as i64, 0).single()
}

fn valid_coordinates(latitude: f64, longitude: f64) -> bool {
    latitude.is_finite()
        && longitude.is_finite()
        && (-90.0..=90.0).contains(&latitude)
        && (-180.0..=180.0).contains(&longitude)
}

/// MADIS data-descriptor values: C/S/V passed progressively deeper automated
/// stages; G is a manual-good override. Z is unchecked, Q/X failed automated
/// QC, and B is a manual-bad override, so all four are excluded.
fn madis_qc_usable(value: u8) -> bool {
    matches!(value.to_ascii_uppercase(), b'C' | b'S' | b'V' | b'G')
}

/// ICAO standard-atmosphere pressure from pressure altitude. MADIS's
/// `altitude` variable is explicitly "pressure altitude, msl"; this is not a
/// measured environmental pressure and callers label it as derived.
fn pressure_from_pressure_altitude(height_m: f64) -> f64 {
    if height_m <= 11_000.0 {
        1013.25 * (1.0 - 2.255_769_564e-5 * height_m).powf(5.255_785_96)
    } else {
        226.321 * (-(height_m - 11_000.0) / 6341.62).exp()
    }
}

fn latest_profile_per_airport(mut profiles: Vec<AircraftProfile>) -> Vec<AircraftProfile> {
    profiles.sort_by_key(|profile| std::cmp::Reverse(profile.valid_time));
    let mut airports = HashSet::new();
    profiles.retain(|profile| airports.insert(profile.airport.clone()));
    profiles
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_arrays(levels: usize) -> MadisArrays {
        let mut altitude = Vec::new();
        let mut temperature = Vec::new();
        let mut dewpoint = Vec::new();
        let mut wind_dir = Vec::new();
        let mut wind_speed = Vec::new();
        let mut track_lat = Vec::new();
        let mut track_lon = Vec::new();
        for level in 0..levels {
            altitude.push(200.0 + level as f64 * 250.0);
            temperature.push(295.0 - level as f64 * 1.5);
            dewpoint.push(290.0 - level as f64 * 1.7);
            wind_dir.push(180.0 + level as f64);
            wind_speed.push(5.0 + level as f64 * 0.5);
            track_lat.push(38.75);
            track_lon.push(-90.37);
        }
        MadisArrays {
            records: 1,
            max_levels: levels,
            airport_width: 6,
            profile_time: vec![1_784_581_260.0],
            profile_airport: b"STL\0\0\0".to_vec(),
            n_levels: vec![levels as f64],
            profile_type: vec![1.0],
            data_source: vec![1.0],
            data_source_labels: vec![(1, "MDCRS public subset".to_owned())],
            latitude: vec![38.74],
            longitude: vec![-90.36],
            track_lat,
            track_lon,
            altitude,
            temperature,
            dewpoint,
            wind_dir,
            wind_speed,
            altitude_qc: vec![b'S'; levels],
            temperature_qc: vec![b'S'; levels],
            dewpoint_qc: vec![b'S'; levels],
            wind_dir_qc: vec![b'C'; levels],
            wind_speed_qc: vec![b'C'; levels],
        }
    }

    #[test]
    fn qc_rejects_failed_and_unchecked_levels_but_keeps_ten_good_levels() {
        let mut arrays = test_arrays(12);
        arrays.temperature_qc[0] = b'Q';
        arrays.wind_speed_qc[1] = b'Z';

        let profiles = profiles_from_arrays(&arrays).expect("decode fixture arrays");

        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].column.len(), 10);
        assert!(profiles[0].pressure_is_derived);
        assert!(
            profiles[0]
                .column
                .pressure_hpa
                .windows(2)
                .all(|pair| pair[0] > pair[1])
        );
    }

    #[test]
    fn fewer_than_ten_qc_usable_levels_drops_the_profile() {
        let mut arrays = test_arrays(12);
        arrays.temperature_qc[..3].fill(b'X');

        assert!(profiles_from_arrays(&arrays).unwrap().is_empty());
    }

    #[test]
    fn pressure_altitude_conversion_is_explicit_and_standard_atmosphere() {
        assert!((pressure_from_pressure_altitude(0.0) - 1013.25).abs() < 0.01);
        assert!((pressure_from_pressure_altitude(11_000.0) - 226.32).abs() < 0.1);
        let profile = profiles_from_arrays(&test_arrays(12)).unwrap().remove(0);
        assert!(
            profile
                .column
                .metadata
                .station_id
                .contains("pressure-altitude p")
        );
    }

    #[test]
    fn profile_keeps_reported_track_and_uses_its_endpoint_for_the_marker() {
        let mut arrays = test_arrays(12);
        for level in 0..12 {
            arrays.track_lat[level] = 38.70 + level as f64 * 0.01;
            arrays.track_lon[level] = -90.50 + level as f64 * 0.02;
        }

        let profile = profiles_from_arrays(&arrays).unwrap().remove(0);

        assert_eq!(profile.track.len(), 12);
        let (lat, lon) = profile.marker_position();
        assert!((lat - 38.81).abs() < 0.001);
        assert!((lon + 90.28).abs() < 0.001);
    }

    #[test]
    fn recent_history_is_newest_first_and_collapses_rolling_file_duplicates() {
        let base = profiles_from_arrays(&test_arrays(12)).unwrap().remove(0);
        let mut older = base.clone();
        older.valid_time -= Duration::hours(1);
        let mut descent = older.clone();
        descent.ascending = false;

        let normalized =
            normalize_recent_profiles(vec![older.clone(), base.clone(), older, descent.clone()]);

        assert_eq!(normalized.len(), 3);
        assert_eq!(normalized[0].valid_time, base.valid_time);
        assert!(
            normalized
                .windows(2)
                .all(|pair| { pair[0].valid_time >= pair[1].valid_time })
        );
        assert!(normalized.iter().any(|profile| !profile.ascending));
    }

    #[test]
    fn profile_search_matches_all_terms_across_airport_direction_source_and_time() {
        let profile = profiles_from_arrays(&test_arrays(12)).unwrap().remove(0);

        assert!(profile_matches_search(&profile, ""));
        assert!(profile_matches_search(&profile, "stl"));
        assert!(profile_matches_search(&profile, "STL ASCENT"));
        assert!(profile_matches_search(&profile, "mdcrs public"));
        assert!(profile_matches_search(&profile, "202607"));
        assert!(!profile_matches_search(&profile, "stl descent"));
        assert!(!profile_matches_search(&profile, "ord"));
    }

    #[test]
    fn invalid_gzip_or_non_madis_netcdf_is_rejected() {
        assert!(parse_gzip_netcdf(b"not gzip").is_err());
    }
}
