//! Live Hurricane Hunters aircraft overlay from the four official NHC HDOB
//! bulletins (Atlantic/Pacific × USAF/NOAA).
//!
//! HDOB records are 30-second flight-level observations. BowEcho polls only
//! while the user enables the layer, rejects stale bulletins, and accumulates
//! successive 20-record bulletins into bounded tracks that survive restarts.
//! The parser follows NHC's official HDOB Tables G-4/G-5; questionable fields
//! are withheld according to their two quality-control flags rather than being
//! drawn as if nominal.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use chrono::{DateTime, Days, NaiveDate, NaiveTime, TimeZone, Timelike, Utc};
use eframe::egui;
use ui_core::worker_slot::{SlotPoll, WorkerSlot};

pub const HUNTER_REFRESH_SECONDS: u64 = 45;
pub const HUNTER_RETRY_SECONDS: u64 = 20;
pub const HUNTER_MAX_AGE_HOURS: i64 = 12;
const HUNTER_FUTURE_TOLERANCE_MINUTES: i64 = 15;
const HUNTER_MAX_TRACKS: usize = 8;
const HUNTER_MAX_POINTS_PER_TRACK: usize = 1_500;
const HUNTER_BARB_SPACING_PX: f32 = 54.0;
const HUNTER_CACHE_VERSION: u8 = 1;
const HUNTER_CACHE_MAX_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HunterAgency {
    AirForce,
    Noaa,
}

impl HunterAgency {
    fn label(self) -> &'static str {
        match self {
            Self::AirForce => "USAF",
            Self::Noaa => "NOAA",
        }
    }

    fn color(self) -> egui::Color32 {
        match self {
            Self::AirForce => egui::Color32::from_rgb(85, 205, 255),
            Self::Noaa => egui::Color32::from_rgb(255, 183, 70),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum HunterBasin {
    Atlantic,
    Pacific,
}

impl HunterBasin {
    fn label(self) -> &'static str {
        match self {
            Self::Atlantic => "Atlantic",
            Self::Pacific => "E/C Pacific",
        }
    }
}

#[derive(Clone, Copy)]
struct HdobFeed {
    url: &'static str,
    product: &'static str,
    agency: HunterAgency,
    basin: HunterBasin,
}

const HDOB_FEEDS: [HdobFeed; 4] = [
    HdobFeed {
        url: "https://www.nhc.noaa.gov/text/URNT15-USAF.shtml",
        product: "URNT15",
        agency: HunterAgency::AirForce,
        basin: HunterBasin::Atlantic,
    },
    HdobFeed {
        url: "https://www.nhc.noaa.gov/text/URNT15-NOAA.shtml",
        product: "URNT15",
        agency: HunterAgency::Noaa,
        basin: HunterBasin::Atlantic,
    },
    HdobFeed {
        url: "https://www.nhc.noaa.gov/text/URPN15-USAF.shtml",
        product: "URPN15",
        agency: HunterAgency::AirForce,
        basin: HunterBasin::Pacific,
    },
    HdobFeed {
        url: "https://www.nhc.noaa.gov/text/URPN15-NOAA.shtml",
        product: "URPN15",
        agency: HunterAgency::Noaa,
        basin: HunterBasin::Pacific,
    },
];

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct HunterObservation {
    pub(crate) time: DateTime<Utc>,
    pub(crate) lat: f32,
    pub(crate) lon: f32,
    pub(crate) static_pressure_hpa: Option<f32>,
    pub(crate) geopotential_height_m: Option<f32>,
    pub(crate) extrapolated_surface_pressure_hpa: Option<f32>,
    pub(crate) temperature_c: Option<f32>,
    pub(crate) dewpoint_c: Option<f32>,
    pub(crate) wind_direction_deg: Option<f32>,
    pub(crate) wind_speed_kt: Option<f32>,
    pub(crate) max_flight_wind_kt: Option<f32>,
    pub(crate) sfmr_wind_kt: Option<f32>,
    pub(crate) sfmr_rain_rate_mm_hr: Option<f32>,
    pub(crate) qc_flags: [u8; 2],
}

#[derive(Clone, Debug)]
struct HunterBulletin {
    mission_id: String,
    aircraft: String,
    observation_number: u8,
    agency: HunterAgency,
    basin: HunterBasin,
    observations: Vec<HunterObservation>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct HunterTrack {
    key: String,
    pub(crate) mission_id: String,
    pub(crate) aircraft: String,
    pub(crate) agency: HunterAgency,
    basin: HunterBasin,
    last_observation_number: u8,
    pub(crate) observations: Vec<HunterObservation>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct HunterTrackCache {
    version: u8,
    tracks: Vec<HunterTrack>,
}

impl HunterTrack {
    pub(crate) fn newest(&self) -> Option<&HunterObservation> {
        self.observations.last()
    }

    fn short_label(&self) -> String {
        format!(
            "{} · {} {}",
            self.aircraft,
            self.agency.label(),
            self.basin.label()
        )
    }
}

#[derive(Debug)]
struct HunterFetch {
    bulletins: Vec<HunterBulletin>,
    sources_ok: usize,
    source_errors: Vec<String>,
}

type HunterFetchResult = Result<HunterFetch, String>;

/// Live reconnaissance state. No polling occurs while the persisted overlay
/// toggle is off; recent tracks are restored from a bounded local cache.
pub(crate) struct HurricaneHunterState {
    fetch_rx: WorkerSlot<HunterFetchResult>,
    tracks: HashMap<String, HunterTrack>,
    pub(crate) status: String,
    last_refresh: Option<Instant>,
    last_fetch_ok: Option<bool>,
    cache_loaded: bool,
}

impl Default for HurricaneHunterState {
    fn default() -> Self {
        Self {
            fetch_rx: WorkerSlot::idle("hurricane-hunters-hdob"),
            tracks: HashMap::new(),
            status: "Hurricane Hunters off".to_owned(),
            last_refresh: None,
            last_fetch_ok: None,
            cache_loaded: false,
        }
    }
}

impl HurricaneHunterState {
    pub(crate) fn maybe_refresh(&mut self, ctx: &egui::Context, enabled: bool) {
        if !enabled {
            return;
        }
        if !self.cache_loaded {
            self.load_cache(Utc::now());
        }
        let interval = if self.last_fetch_ok == Some(true) {
            Duration::from_secs(HUNTER_REFRESH_SECONDS)
        } else {
            Duration::from_secs(HUNTER_RETRY_SECONDS)
        };
        let due = self
            .last_refresh
            .is_none_or(|last| last.elapsed() >= interval);
        if due && !self.fetch_rx.in_flight() {
            self.last_refresh = Some(Instant::now());
            self.status = if self.tracks.is_empty() {
                "Checking official NHC HDOB feeds…".to_owned()
            } else {
                self.status.clone()
            };
            self.fetch_rx.spawn(ctx, |tx| {
                let result =
                    hunter_http_client().and_then(|client| fetch_hdob(&client, Utc::now()));
                let _ = tx.send(result);
            });
        }
        ctx.request_repaint_after(Duration::from_secs(1));
    }

    /// Drain a finished worker even if the user switched the layer off while
    /// its request was in flight; this keeps the WorkerSlot reusable without
    /// starting another request.
    pub(crate) fn poll(&mut self, now: DateTime<Utc>) {
        match self.fetch_rx.poll() {
            SlotPoll::Ready(Ok(fetch)) => {
                self.last_fetch_ok = Some(true);
                self.ingest(fetch, now);
                if let Err(error) = self.save_cache() {
                    self.status
                        .push_str(&format!(" · track cache unavailable: {error}"));
                }
            }
            SlotPoll::Ready(Err(err)) => {
                self.last_fetch_ok = Some(false);
                self.prune(now);
                self.status = format!("Hurricane Hunters unavailable — retrying ({err})");
            }
            SlotPoll::Idle | SlotPoll::Pending | SlotPoll::Disconnected => {
                self.prune(now);
            }
        }
    }

    fn ingest(&mut self, fetch: HunterFetch, now: DateTime<Utc>) {
        for bulletin in fetch.bulletins {
            let key = format!(
                "{}|{}|{}|{}",
                bulletin.agency.label(),
                bulletin.basin.label(),
                bulletin.aircraft,
                bulletin.mission_id
            );
            let track = self
                .tracks
                .entry(key.clone())
                .or_insert_with(|| HunterTrack {
                    key,
                    mission_id: bulletin.mission_id.clone(),
                    aircraft: bulletin.aircraft.clone(),
                    agency: bulletin.agency,
                    basin: bulletin.basin,
                    last_observation_number: bulletin.observation_number,
                    observations: Vec::new(),
                });
            track.last_observation_number = bulletin.observation_number;
            for observation in bulletin.observations {
                match track
                    .observations
                    .binary_search_by_key(&observation.time, |item| item.time)
                {
                    Ok(index) => track.observations[index] = observation,
                    Err(index) => track.observations.insert(index, observation),
                }
            }
            if track.observations.len() > HUNTER_MAX_POINTS_PER_TRACK {
                let remove = track.observations.len() - HUNTER_MAX_POINTS_PER_TRACK;
                track.observations.drain(..remove);
            }
        }
        self.prune(now);

        let active = self.active_tracks(now).len();
        let source_note = if fetch.source_errors.is_empty() {
            "4/4 official feeds".to_owned()
        } else {
            format!(
                "{}/4 feeds · {} unavailable",
                fetch.sources_ok,
                fetch.source_errors.len()
            )
        };
        self.status = match active {
            0 => format!("No live reconnaissance · {source_note}"),
            1 => format!("1 live reconnaissance aircraft · {source_note}"),
            count => format!("{count} live reconnaissance aircraft · {source_note}"),
        };
    }

    fn load_cache(&mut self, now: DateTime<Utc>) {
        self.cache_loaded = true;
        let path = hunter_cache_path();
        let text = match settings::read_text_capped(&path, HUNTER_CACHE_MAX_BYTES) {
            Ok(text) => text,
            Err(error) if error.is_not_found() => return,
            Err(error) => {
                self.status = format!("Could not restore Hurricane Hunters track cache: {error}");
                return;
            }
        };
        match decode_track_cache(&text) {
            Ok(tracks) => {
                self.tracks = tracks;
                self.prune(now);
                let active = self.active_tracks(now).len();
                if active > 0 {
                    self.status = format!(
                        "Restored {active} live reconnaissance track{}; checking NHC for updates…",
                        if active == 1 { "" } else { "s" }
                    );
                }
            }
            Err(error) => {
                self.status = format!("Hurricane Hunters track cache ignored: {error}");
            }
        }
    }

    fn save_cache(&self) -> Result<(), String> {
        let mut tracks: Vec<HunterTrack> = self.tracks.values().cloned().collect();
        tracks.sort_by(|left, right| left.key.cmp(&right.key));
        settings::atomic_write_json(
            &hunter_cache_path(),
            &HunterTrackCache {
                version: HUNTER_CACHE_VERSION,
                tracks,
            },
            HUNTER_CACHE_MAX_BYTES,
        )
        .map_err(|error| error.to_string())
    }

    fn prune(&mut self, now: DateTime<Utc>) {
        let cutoff = now - chrono::Duration::hours(HUNTER_MAX_AGE_HOURS);
        for track in self.tracks.values_mut() {
            track.observations.retain(|observation| {
                observation.time >= cutoff
                    && observation.time
                        <= now + chrono::Duration::minutes(HUNTER_FUTURE_TOLERANCE_MINUTES)
            });
        }
        self.tracks
            .retain(|_, track| !track.observations.is_empty());
        if self.tracks.len() > HUNTER_MAX_TRACKS {
            let mut oldest: Vec<(String, DateTime<Utc>)> = self
                .tracks
                .values()
                .filter_map(|track| {
                    track
                        .newest()
                        .map(|latest| (track.key.clone(), latest.time))
                })
                .collect();
            oldest.sort_by_key(|(_, time)| *time);
            for (key, _) in oldest
                .into_iter()
                .take(self.tracks.len() - HUNTER_MAX_TRACKS)
            {
                self.tracks.remove(&key);
            }
        }
    }

    pub(crate) fn active_tracks(&self, now: DateTime<Utc>) -> Vec<&HunterTrack> {
        let cutoff = now - chrono::Duration::hours(HUNTER_MAX_AGE_HOURS);
        let mut tracks: Vec<&HunterTrack> = self
            .tracks
            .values()
            .filter(|track| track.newest().is_some_and(|latest| latest.time >= cutoff))
            .collect();
        tracks.sort_by_key(|track| track.newest().map(|latest| latest.time));
        tracks.reverse();
        tracks
    }

    pub(crate) fn newest_position(&self, now: DateTime<Utc>) -> Option<(f32, f32)> {
        self.active_tracks(now)
            .first()
            .and_then(|track| track.newest())
            .map(|observation| (observation.lat, observation.lon))
    }

    pub(crate) fn status_ui(&self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new(&self.status).small().weak());
        ui.horizontal_wrapped(|ui| {
            legend_swatch(ui, HunterAgency::AirForce);
            legend_swatch(ui, HunterAgency::Noaa);
            ui.weak("line = recent flight track · barbs = flight-level wind · ✈ = newest");
        });
        for track in self.active_tracks(Utc::now()) {
            let Some(latest) = track.newest() else {
                continue;
            };
            let mut details = vec![track.short_label(), age_label(Utc::now(), latest.time)];
            if let (Some(direction), Some(speed)) =
                (latest.wind_direction_deg, latest.wind_speed_kt)
            {
                details.push(format!("{direction:.0}°/{speed:.0} kt FL wind"));
            }
            if let Some(pressure) = latest.static_pressure_hpa {
                details.push(format!("{pressure:.1} hPa"));
            }
            if let Some(temperature) = latest.temperature_c {
                details.push(format!("T {temperature:.1}°C"));
            }
            if let Some(dewpoint) = latest.dewpoint_c {
                details.push(format!("Td {dewpoint:.1}°C"));
            }
            ui.label(egui::RichText::new(details.join(" · ")).small())
                .on_hover_text(format!("Mission {}", track.mission_id));
        }
    }
}

fn hunter_cache_path() -> PathBuf {
    settings::data_dir_override()
        .or_else(settings::active_storage_root)
        .unwrap_or_else(|| PathBuf::from("bowecho-data"))
        .join("hurricane-hunters")
        .join("tracks.json")
}

fn decode_track_cache(text: &str) -> Result<HashMap<String, HunterTrack>, String> {
    let cache: HunterTrackCache = serde_json::from_str(text).map_err(|error| error.to_string())?;
    if cache.version != HUNTER_CACHE_VERSION {
        return Err(format!(
            "unsupported cache version {} (expected {HUNTER_CACHE_VERSION})",
            cache.version
        ));
    }
    let mut tracks = HashMap::new();
    for mut track in cache.tracks {
        if track.key.trim().is_empty()
            || track.aircraft.trim().is_empty()
            || track.mission_id.trim().is_empty()
        {
            continue;
        }
        track.observations.retain(|observation| {
            observation.lat.is_finite()
                && observation.lon.is_finite()
                && (-90.0..=90.0).contains(&observation.lat)
                && (-180.0..=180.0).contains(&observation.lon)
        });
        track
            .observations
            .sort_by_key(|observation| observation.time);
        track
            .observations
            .dedup_by_key(|observation| observation.time);
        if track.observations.len() > HUNTER_MAX_POINTS_PER_TRACK {
            let remove = track.observations.len() - HUNTER_MAX_POINTS_PER_TRACK;
            track.observations.drain(..remove);
        }
        if !track.observations.is_empty() {
            tracks.insert(track.key.clone(), track);
        }
    }
    Ok(tracks)
}

fn legend_swatch(ui: &mut egui::Ui, agency: HunterAgency) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(11.0, 11.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 2.0, agency.color());
    ui.label(egui::RichText::new(agency.label()).small());
}

fn age_label(now: DateTime<Utc>, time: DateTime<Utc>) -> String {
    let age = (now - time).num_minutes().max(0);
    if age < 60 {
        format!("{age}m old")
    } else {
        format!("{}h {}m old", age / 60, age % 60)
    }
}

fn hunter_http_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent("BowEcho Hurricane Hunters HDOB layer (github.com/FahrenheitResearch/bowecho)")
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|error| error.to_string())
}

fn fetch_hdob(client: &reqwest::blocking::Client, now: DateTime<Utc>) -> HunterFetchResult {
    let mut bulletins = Vec::new();
    let mut sources_ok = 0;
    let mut source_errors = Vec::new();
    for feed in HDOB_FEEDS {
        let result = client
            .get(feed.url)
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .and_then(reqwest::blocking::Response::text)
            .map_err(|error| error.to_string())
            .and_then(|body| parse_hdob_page(&body, feed, now));
        match result {
            Ok(Some(bulletin)) => {
                sources_ok += 1;
                bulletins.push(bulletin);
            }
            Ok(None) => sources_ok += 1,
            Err(error) => source_errors.push(format!(
                "{} {}: {error}",
                feed.basin.label(),
                feed.agency.label()
            )),
        }
    }
    if sources_ok == 0 {
        Err(source_errors.join("; "))
    } else {
        Ok(HunterFetch {
            bulletins,
            sources_ok,
            source_errors,
        })
    }
}

fn parse_hdob_page(
    body: &str,
    feed: HdobFeed,
    now: DateTime<Utc>,
) -> Result<Option<HunterBulletin>, String> {
    let text = extract_pre_text(body);
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let header_index = lines
        .iter()
        .position(|line| line.starts_with(feed.product))
        .ok_or_else(|| format!("missing {} communications header", feed.product))?;
    let mission_line = lines
        .iter()
        .skip(header_index + 1)
        .find(|line| line.contains(" HDOB "))
        .ok_or_else(|| "missing HDOB mission identifier".to_owned())?;
    let (mission_left, mission_right) = mission_line
        .split_once(" HDOB ")
        .ok_or_else(|| "malformed HDOB mission identifier".to_owned())?;
    let mission_id = mission_left.trim().to_owned();
    let aircraft = mission_id
        .split_whitespace()
        .next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "HDOB mission has no aircraft id".to_owned())?
        .to_owned();
    let mut mission_fields = mission_right.split_whitespace();
    let observation_number = mission_fields
        .next()
        .ok_or_else(|| "HDOB mission has no observation number".to_owned())?
        .parse::<u8>()
        .map_err(|_| "invalid HDOB observation number".to_owned())?;
    let date = NaiveDate::parse_from_str(
        mission_fields
            .next()
            .ok_or_else(|| "HDOB mission has no date".to_owned())?,
        "%Y%m%d",
    )
    .map_err(|error| format!("invalid HDOB mission date: {error}"))?;

    let mission_position = lines
        .iter()
        .position(|line| *line == *mission_line)
        .expect("mission came from lines");
    let mut observations = Vec::new();
    let mut record_date = date;
    let mut previous_seconds = None;
    for line in lines.iter().skip(mission_position + 1) {
        if line.starts_with("$$") || line.starts_with(';') {
            break;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 13 || fields[0].len() != 6 {
            continue;
        }
        let time = parse_hhmmss(fields[0])?;
        let seconds = time.num_seconds_from_midnight();
        if previous_seconds.is_some_and(|previous| seconds < previous) {
            record_date = record_date
                .checked_add_days(Days::new(1))
                .ok_or_else(|| "HDOB date overflow".to_owned())?;
        }
        previous_seconds = Some(seconds);
        if let Some(observation) = parse_hdob_record(&fields, record_date, time)? {
            observations.push(observation);
        }
    }
    observations.sort_by_key(|observation| observation.time);
    observations.dedup_by_key(|observation| observation.time);
    let Some(newest) = observations.last() else {
        return Ok(None);
    };
    let age = now - newest.time;
    if age > chrono::Duration::hours(HUNTER_MAX_AGE_HOURS)
        || age < -chrono::Duration::minutes(HUNTER_FUTURE_TOLERANCE_MINUTES)
    {
        return Ok(None);
    }

    Ok(Some(HunterBulletin {
        mission_id,
        aircraft,
        observation_number,
        agency: feed.agency,
        basin: feed.basin,
        observations,
    }))
}

fn extract_pre_text(body: &str) -> String {
    let Some(start_tag) = body.to_ascii_lowercase().find("<pre") else {
        return body.to_owned();
    };
    let Some(content_start_rel) = body[start_tag..].find('>') else {
        return body.to_owned();
    };
    let content_start = start_tag + content_start_rel + 1;
    let lower_tail = body[content_start..].to_ascii_lowercase();
    let content_end = lower_tail
        .find("</pre>")
        .map(|offset| content_start + offset)
        .unwrap_or(body.len());
    body[content_start..content_end]
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&#36;", "$")
}

fn parse_hhmmss(value: &str) -> Result<NaiveTime, String> {
    if value.len() != 6 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("invalid HDOB time {value}"));
    }
    let hour = value[0..2].parse::<u32>().map_err(|_| "invalid hour")?;
    let minute = value[2..4].parse::<u32>().map_err(|_| "invalid minute")?;
    let second = value[4..6].parse::<u32>().map_err(|_| "invalid second")?;
    NaiveTime::from_hms_opt(hour, minute, second)
        .ok_or_else(|| format!("invalid HDOB time {value}"))
}

fn parse_hdob_record(
    fields: &[&str],
    date: NaiveDate,
    time: NaiveTime,
) -> Result<Option<HunterObservation>, String> {
    let qc = parse_qc(fields[12])?;
    // QC first digit 1/3 explicitly says latitude/longitude is questionable.
    if matches!(qc[0], 1 | 3) {
        return Ok(None);
    }
    let Some(lat) = parse_coordinate(fields[1], false)? else {
        return Ok(None);
    };
    let Some(lon) = parse_coordinate(fields[2], true)? else {
        return Ok(None);
    };
    // 0000N 00000W is used by the NHC placeholder bulletin and is not an
    // aircraft position.
    if lat == 0.0 && lon == 0.0 {
        return Ok(None);
    }
    let timestamp = Utc.from_utc_datetime(&date.and_time(time));

    let pressure_height_bad = matches!(qc[0], 2 | 3);
    let thermodynamics_bad = matches!(qc[1], 1 | 4 | 5 | 9);
    let flight_wind_bad = matches!(qc[1], 2 | 4 | 6 | 9);
    let sfmr_bad = matches!(qc[1], 3 | 5 | 6 | 9);

    let static_pressure_hpa = (!pressure_height_bad)
        .then(|| decode_pressure_tenths(fields[3]))
        .flatten();
    let geopotential_height_m = (!pressure_height_bad)
        .then(|| parse_unsigned(fields[4], 99_999).map(|value| value as f32))
        .flatten();
    let extrapolated_surface_pressure_hpa = static_pressure_hpa
        .filter(|pressure| *pressure >= 550.0)
        .and_then(|_| decode_pressure_tenths(fields[5]));
    let temperature_c = (!thermodynamics_bad)
        .then(|| parse_signed_tenths(fields[6]))
        .flatten();
    let dewpoint_c = (!thermodynamics_bad)
        .then(|| parse_signed_tenths(fields[7]))
        .flatten();
    let (wind_direction_deg, wind_speed_kt) = if flight_wind_bad {
        (None, None)
    } else {
        parse_wind(fields[8])
    };

    Ok(Some(HunterObservation {
        time: timestamp,
        lat,
        lon,
        static_pressure_hpa,
        geopotential_height_m,
        extrapolated_surface_pressure_hpa,
        temperature_c,
        dewpoint_c,
        wind_direction_deg,
        wind_speed_kt,
        max_flight_wind_kt: (!flight_wind_bad)
            .then(|| parse_unsigned(fields[9], 998).map(|value| value as f32))
            .flatten(),
        sfmr_wind_kt: (!sfmr_bad)
            .then(|| parse_unsigned(fields[10], 998).map(|value| value as f32))
            .flatten(),
        sfmr_rain_rate_mm_hr: (!sfmr_bad)
            .then(|| parse_unsigned(fields[11], 998).map(|value| value as f32))
            .flatten(),
        qc_flags: qc,
    }))
}

fn parse_qc(value: &str) -> Result<[u8; 2], String> {
    let bytes = value.as_bytes();
    if bytes.len() != 2 || !bytes.iter().all(u8::is_ascii_digit) {
        return Err(format!("invalid HDOB QC flags {value}"));
    }
    Ok([bytes[0] - b'0', bytes[1] - b'0'])
}

fn parse_coordinate(value: &str, longitude: bool) -> Result<Option<f32>, String> {
    if value.contains('/') {
        return Ok(None);
    }
    let digits = if longitude { 5 } else { 4 };
    if value.len() != digits + 1 {
        return Err(format!("invalid HDOB coordinate {value}"));
    }
    let hemisphere = value.as_bytes()[digits] as char;
    let degrees_digits = digits - 2;
    let degrees = value[..degrees_digits]
        .parse::<f32>()
        .map_err(|_| format!("invalid HDOB coordinate {value}"))?;
    let minutes = value[degrees_digits..digits]
        .parse::<f32>()
        .map_err(|_| format!("invalid HDOB coordinate {value}"))?;
    if minutes >= 60.0 {
        return Err(format!("invalid HDOB coordinate minutes {value}"));
    }
    let sign = match (longitude, hemisphere) {
        (false, 'N') | (true, 'E') => 1.0,
        (false, 'S') | (true, 'W') => -1.0,
        _ => return Err(format!("invalid HDOB hemisphere {value}")),
    };
    Ok(Some(sign * (degrees + minutes / 60.0)))
}

/// Static/extrapolated pressure is tenths of hPa with the leading 1 omitted
/// for values >= 1000 hPa (0164 -> 1016.4 hPa; 9412 -> 941.2 hPa).
fn decode_pressure_tenths(value: &str) -> Option<f32> {
    let raw = parse_unsigned(value, 9_999)?;
    let tenths = if raw < 1_000 { raw + 10_000 } else { raw };
    Some(tenths as f32 / 10.0)
}

fn parse_signed_tenths(value: &str) -> Option<f32> {
    if value.len() != 4 || value.contains('/') {
        return None;
    }
    let sign = match value.as_bytes()[0] {
        b'+' => 1.0,
        b'-' => -1.0,
        _ => return None,
    };
    let magnitude = value[1..].parse::<u16>().ok()?;
    Some(sign * magnitude as f32 / 10.0)
}

fn parse_wind(value: &str) -> (Option<f32>, Option<f32>) {
    if value.len() != 6 || value.contains('/') {
        return (None, None);
    }
    let direction = value[..3].parse::<u16>().ok();
    let speed = value[3..].parse::<u16>().ok();
    match (direction, speed) {
        (Some(direction), Some(speed)) if direction <= 360 && speed < 999 => {
            (Some(direction as f32), Some(speed as f32))
        }
        _ => (None, None),
    }
}

fn parse_unsigned(value: &str, maximum: u32) -> Option<u32> {
    if value.contains('/') || value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse::<u32>().ok().filter(|value| *value <= maximum)
}

impl crate::ViewerApp {
    /// Draw every live aircraft on both the primary map and split panes. The
    /// toggle is independent from active-storm cones, so reconnaissance still
    /// appears when an invest has no numbered tropical cyclone yet.
    pub(crate) fn draw_hurricane_hunters(&self, painter: &egui::Painter, rect: egui::Rect) {
        if !self.app_settings.show_hurricane_hunters {
            return;
        }
        let tracks = self.tropical.hurricane_hunters.active_tracks(Utc::now());
        if tracks.is_empty() {
            return;
        }
        let jump_px = rect.width().max(rect.height());
        for track in tracks {
            let color = track.agency.color();
            let path: Vec<egui::Pos2> = track
                .observations
                .iter()
                .map(|observation| self.lon_lat_to_screen(rect, observation.lon, observation.lat))
                .collect();
            let mut shapes = Vec::new();
            crate::push_solid_open_line(
                &mut shapes,
                &path,
                egui::Stroke::new(2.0_f32, color.gamma_multiply(0.82)),
                rect,
                jump_px,
            );
            painter.extend(shapes);

            // Work newest-to-oldest so the current flight-level barb always
            // wins decluttering; older barbs are admitted by screen distance.
            let mut barb_positions = Vec::new();
            let mut barb_style = self.style_registry.obs().clone();
            barb_style.barb_color = [color.r(), color.g(), color.b(), 235];
            barb_style.barb_width = 1.3;
            for observation in track.observations.iter().rev() {
                let (Some(direction), Some(speed)) =
                    (observation.wind_direction_deg, observation.wind_speed_kt)
                else {
                    continue;
                };
                let pos = self.lon_lat_to_screen(rect, observation.lon, observation.lat);
                if !rect.expand(30.0).contains(pos)
                    || barb_positions
                        .iter()
                        .any(|last: &egui::Pos2| last.distance(pos) < HUNTER_BARB_SPACING_PX)
                {
                    continue;
                }
                crate::draw_station_barb(painter, pos, direction, speed, &barb_style);
                barb_positions.push(pos);
            }

            let Some(latest) = track.newest() else {
                continue;
            };
            let pos = self.lon_lat_to_screen(rect, latest.lon, latest.lat);
            if !rect.expand(60.0).contains(pos) {
                continue;
            }
            painter.circle_filled(pos, 8.0, egui::Color32::BLACK.gamma_multiply(0.78));
            painter.circle_stroke(pos, 8.0, egui::Stroke::new(1.8_f32, color));
            crate::draw_halo_text(
                painter,
                pos,
                egui::Align2::CENTER_CENTER,
                "✈",
                egui::FontId::proportional(14.0),
                color,
                egui::Color32::BLACK,
            );
            let wind = latest
                .wind_speed_kt
                .map(|speed| format!(" · {speed:.0} kt FL"))
                .unwrap_or_default();
            crate::draw_halo_text(
                painter,
                pos + egui::vec2(11.0, -1.0),
                egui::Align2::LEFT_CENTER,
                &format!("{}{wind}", track.aircraft),
                egui::FontId::proportional(11.0),
                color,
                egui::Color32::BLACK,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
<pre>888
URNT15 KNHC 281426
AF302 1712A KATRINA            HDOB 41 20050928
142030 2608N 08756W 7093 03047 9333 +192 +134 133083 089 080 999 00
142100 2609N 08755W 7091 03054 9330 +166 +146 133106 115 103 999 00
142130 2610N 08754W 7058 03040 9295 +134 +134 135121 124 111 999 00
$$
</pre>"#;

    fn atlantic_usaf() -> HdobFeed {
        HDOB_FEEDS[0]
    }

    #[test]
    fn official_sample_decodes_mission_position_and_meteorology() {
        let now = Utc.with_ymd_and_hms(2005, 9, 28, 15, 0, 0).unwrap();
        let bulletin = parse_hdob_page(SAMPLE, atlantic_usaf(), now)
            .expect("parse")
            .expect("fresh bulletin");

        assert_eq!(bulletin.mission_id, "AF302 1712A KATRINA");
        assert_eq!(bulletin.aircraft, "AF302");
        assert_eq!(bulletin.observation_number, 41);
        assert_eq!(bulletin.observations.len(), 3);
        let first = &bulletin.observations[0];
        assert_eq!(
            first.time,
            Utc.with_ymd_and_hms(2005, 9, 28, 14, 20, 30).unwrap()
        );
        assert!((first.lat - (26.0 + 8.0 / 60.0)).abs() < 1e-5);
        assert!((first.lon - (-87.0 - 56.0 / 60.0)).abs() < 1e-5);
        assert_eq!(first.static_pressure_hpa, Some(709.3));
        assert_eq!(first.geopotential_height_m, Some(3047.0));
        assert_eq!(first.extrapolated_surface_pressure_hpa, Some(933.3));
        assert_eq!(first.temperature_c, Some(19.2));
        assert_eq!(first.dewpoint_c, Some(13.4));
        assert_eq!(first.wind_direction_deg, Some(133.0));
        assert_eq!(first.wind_speed_kt, Some(83.0));
        assert_eq!(first.max_flight_wind_kt, Some(89.0));
        assert_eq!(first.sfmr_wind_kt, Some(80.0));
        assert_eq!(first.sfmr_rain_rate_mm_hr, None);
    }

    #[test]
    fn pressure_leading_one_and_qc_flags_are_honored() {
        let fields: Vec<&str> =
            "222100 3025N 08855W 0164 00164 0163 +273 +223 291002 008 010 026 26"
                .split_whitespace()
                .collect();
        let observation = parse_hdob_record(
            &fields,
            NaiveDate::from_ymd_opt(2026, 7, 14).unwrap(),
            NaiveTime::from_hms_opt(22, 21, 0).unwrap(),
        )
        .expect("parse")
        .expect("position nominal");

        assert_eq!(
            observation.static_pressure_hpa, None,
            "QC 2: pressure questionable"
        );
        assert_eq!(observation.geopotential_height_m, None);
        assert_eq!(observation.temperature_c, Some(27.3));
        assert_eq!(observation.dewpoint_c, Some(22.3));
        assert_eq!(
            observation.wind_direction_deg, None,
            "QC 6: FL wind questionable"
        );
        assert_eq!(observation.sfmr_wind_kt, None, "QC 6: SFMR questionable");
        assert_eq!(observation.sfmr_rain_rate_mm_hr, None);
        assert_eq!(decode_pressure_tenths("0164"), Some(1016.4));

        let mut bad_position = fields;
        bad_position[12] = "10";
        assert!(
            parse_hdob_record(
                &bad_position,
                NaiveDate::from_ymd_opt(2026, 7, 14).unwrap(),
                NaiveTime::from_hms_opt(22, 21, 0).unwrap(),
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn stale_and_placeholder_bulletins_do_not_create_ghost_aircraft() {
        let stale_now = Utc.with_ymd_and_hms(2005, 9, 29, 3, 0, 1).unwrap();
        assert!(
            parse_hdob_page(SAMPLE, atlantic_usaf(), stale_now)
                .unwrap()
                .is_none()
        );

        let placeholder = SAMPLE
            .replace("2608N 08756W", "0000N 00000W")
            .replace("2609N 08755W", "0000N 00000W")
            .replace("2610N 08754W", "0000N 00000W");
        let now = Utc.with_ymd_and_hms(2005, 9, 28, 15, 0, 0).unwrap();
        assert!(
            parse_hdob_page(&placeholder, atlantic_usaf(), now)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn midnight_rollover_and_session_dedupe_are_exact() {
        let page = SAMPLE
            .replace("20050928", "20260714")
            .replace("142030", "235930")
            .replace("142100", "000000")
            .replace("142130", "000030");
        let now = Utc.with_ymd_and_hms(2026, 7, 15, 0, 5, 0).unwrap();
        let bulletin = parse_hdob_page(&page, atlantic_usaf(), now)
            .unwrap()
            .unwrap();
        assert_eq!(
            bulletin.observations[0].time.date_naive(),
            NaiveDate::from_ymd_opt(2026, 7, 14).unwrap()
        );
        assert_eq!(
            bulletin.observations[1].time.date_naive(),
            NaiveDate::from_ymd_opt(2026, 7, 15).unwrap()
        );

        let mut state = HurricaneHunterState::default();
        for _ in 0..2 {
            state.ingest(
                HunterFetch {
                    bulletins: vec![bulletin.clone()],
                    sources_ok: 4,
                    source_errors: Vec::new(),
                },
                now,
            );
        }
        let tracks = state.active_tracks(now);
        assert_eq!(tracks.len(), 1);
        assert_eq!(
            tracks[0].observations.len(),
            3,
            "same bulletin is deduplicated"
        );
    }

    #[test]
    fn session_tracks_prune_old_points_and_cap_memory() {
        let now = Utc.with_ymd_and_hms(2026, 7, 15, 0, 0, 0).unwrap();
        let mut observations = Vec::with_capacity(HUNTER_MAX_POINTS_PER_TRACK + 25);
        let template = HunterObservation {
            time: now,
            lat: 25.0,
            lon: -80.0,
            static_pressure_hpa: Some(700.0),
            geopotential_height_m: Some(3_000.0),
            extrapolated_surface_pressure_hpa: Some(950.0),
            temperature_c: Some(15.0),
            dewpoint_c: Some(12.0),
            wind_direction_deg: Some(90.0),
            wind_speed_kt: Some(50.0),
            max_flight_wind_kt: Some(55.0),
            sfmr_wind_kt: Some(60.0),
            sfmr_rain_rate_mm_hr: Some(10.0),
            qc_flags: [0, 0],
        };
        for index in 0..(HUNTER_MAX_POINTS_PER_TRACK + 25) {
            observations.push(HunterObservation {
                time: now - chrono::Duration::seconds((index as i64) * 10),
                ..template.clone()
            });
        }
        observations.push(HunterObservation {
            time: now - chrono::Duration::hours(HUNTER_MAX_AGE_HOURS + 1),
            ..template
        });

        let mut state = HurricaneHunterState::default();
        state.ingest(
            HunterFetch {
                bulletins: vec![HunterBulletin {
                    mission_id: "AF300 TEST".to_owned(),
                    aircraft: "AF300".to_owned(),
                    observation_number: 1,
                    agency: HunterAgency::AirForce,
                    basin: HunterBasin::Atlantic,
                    observations,
                }],
                sources_ok: 4,
                source_errors: Vec::new(),
            },
            now,
        );

        let track = state.active_tracks(now)[0];
        assert!(track.observations.len() <= HUNTER_MAX_POINTS_PER_TRACK);
        assert!(track.observations.iter().all(|observation| {
            observation.time >= now - chrono::Duration::hours(HUNTER_MAX_AGE_HOURS)
        }));
    }

    #[test]
    fn disabled_layer_never_starts_a_network_worker() {
        let mut state = HurricaneHunterState::default();
        let ctx = egui::Context::default();
        state.maybe_refresh(&ctx, false);
        assert!(state.last_refresh.is_none());
        assert!(!state.fetch_rx.in_flight());
    }

    #[test]
    fn recent_track_cache_round_trips_observations() {
        let now = Utc.with_ymd_and_hms(2005, 9, 28, 15, 0, 0).unwrap();
        let bulletin = parse_hdob_page(SAMPLE, atlantic_usaf(), now)
            .unwrap()
            .unwrap();
        let key = "USAF|Atlantic|AF302|AF302 1712A KATRINA".to_owned();
        let document = HunterTrackCache {
            version: HUNTER_CACHE_VERSION,
            tracks: vec![HunterTrack {
                key: key.clone(),
                mission_id: bulletin.mission_id,
                aircraft: bulletin.aircraft,
                agency: bulletin.agency,
                basin: bulletin.basin,
                last_observation_number: bulletin.observation_number,
                observations: bulletin.observations,
            }],
        };
        let json = serde_json::to_string(&document).expect("serialize track cache");
        let restored = decode_track_cache(&json).expect("decode track cache");

        let track = restored.get(&key).expect("restored track");
        assert_eq!(track.observations.len(), 3);
        assert_eq!(track.newest().unwrap().wind_speed_kt, Some(121.0));
    }
}
