//! RAP environmental wind profiles for the model-anchored dealias engine.
//!
//! The v4 dealiaser (`render2d::dealias_volume_v4`) accepts an optional
//! [`EnvironmentalWindProfile`] — the external absolute Nyquist-branch
//! reference (dealias-v4 spec §4a; precedents: Eilts & Smith 1990, *JTECH*
//! 7, 118–128 — the WSR-88D VDA's environmental wind constraints — and
//! James & Houze 2001, *JTECH* 18, 1674–1683 — 4DD's sounding
//! initialization). Per the spec §16 owner decision the profile source is
//! **RAP ONLY** (0-h analysis, `awp130pgrb`, CONUS 13 km grid) and
//! **CONUS ONLY**: international sites run the engine without a profile
//! (the graceful no-env path), and the picker copy says so.
//!
//! Plumbing reuse: this module deliberately builds NO new fetcher. It rides
//! the same rusty-weather stack the model-data dock's ingest already uses —
//! `rustwx_models` resolves the RAP cycle URL, `rustwx_io::fetch_bytes_with_cache`
//! performs the `.idx`-subset byte-range fetch from the public
//! `noaa-rap-pds` mirror (UGRD/VGRD/HGT records only, a few MB instead of
//! the full file), and `rustwx_io::extract_field_values_partial_*` decodes
//! the GRIB2 messages. GRIB bytes land in the ingest's own cache root
//! (`settings::model_cache_dir()`), so the one-click ingest and this module
//! share downloads.
//!
//! Threading contract: all network/decode work runs on one background
//! thread. The UI thread only enqueues requests ([`DealiasEnvCache::ensure_requested`]),
//! drains results ([`DealiasEnvCache::pump`]), and reads the cache
//! ([`DealiasEnvCache::profile_for`]) — rendering never blocks on a fetch.
//! Profiles are cached per (site, cycle) for the whole session; a failed
//! cycle is retried no sooner than [`FAILED_RETRY`].

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

use chrono::{DateTime, Duration, DurationRound, Utc};
use render2d::{EnvWindLevel, EnvironmentalWindProfile};
use rustwx_core::{CanonicalField, FieldSelector, LatLonGrid, ModelId};
use rustwx_io::PartialValuesExtraction;

/// The engine ignores profiles further than 3 h from the volume time
/// (`render2d::dealias_v4::ENV_PROFILE_MAX_AGE`); requesting cycles beyond
/// that bound would fetch data the solver then discards.
const MAX_CYCLE_DISTANCE: Duration = Duration::hours(3);
/// Re-attempt a failed (site, cycle) fetch after this long. Failures are
/// usually "cycle not on the mirror yet" or a network blip.
const FAILED_RETRY: Duration = Duration::minutes(5);
/// RAP `awp130pgrb` isobaric levels: 100–1000 hPa every 25 hPa.
const RAP_LEVEL_STEP_HPA: u16 = 25;
const RAP_LEVEL_MIN_HPA: u16 = 100;
const RAP_LEVEL_MAX_HPA: u16 = 1000;
/// A usable branch reference needs some vertical structure.
const MIN_PROFILE_LEVELS: usize = 4;
/// Nearest-grid-point guard: RAP CONUS spacing is ~13 km, so any on-grid
/// site has a neighbor within ~9.2 km. A larger distance means the site is
/// OFF the grid (Alaska/Hawaii/Guam/San Juan edge cases and every intl
/// site) — those get no profile, by decision, not by accident.
const MAX_GRID_POINT_DISTANCE_KM: f32 = 20.0;
/// Cheap pre-gate before any network work: rough RAP-CONUS bounding box.
/// The precise answer is the nearest-grid-point guard above; this only
/// prevents pointless fetch attempts for obviously non-CONUS sites.
const CONUS_LAT_RANGE: std::ops::RangeInclusive<f32> = 16.0..=56.0;
const CONUS_LON_RANGE: std::ops::RangeInclusive<f32> = -130.0..=-60.0;

/// One profile fetch: everything the worker needs, as plain data.
#[derive(Clone, Debug, PartialEq)]
pub struct EnvProfileRequest {
    pub site_id: String,
    pub latitude_deg: f32,
    pub longitude_deg: f32,
    /// Station elevation (m MSL) from the volume metadata; when absent the
    /// model's own terrain height at the grid point stands in.
    pub elevation_m: Option<f32>,
    /// RAP 0-h analysis cycle (top of an hour, UTC).
    pub cycle: DateTime<Utc>,
}

enum EnvSlot {
    Pending,
    Ready(Arc<EnvironmentalWindProfile>),
    Failed { at: DateTime<Utc> },
}

type EnvKey = (String, i64);
type EnvResponse = (EnvKey, Result<EnvironmentalWindProfile, String>);

/// Session cache + background fetch worker for RAP site profiles.
#[derive(Default)]
pub struct DealiasEnvCache {
    slots: HashMap<EnvKey, EnvSlot>,
    worker: Option<EnvWorker>,
    /// Human-readable note for the most recent failure (status surface).
    last_error: Option<String>,
}

struct EnvWorker {
    tx: Sender<EnvProfileRequest>,
    rx: Receiver<EnvResponse>,
}

impl DealiasEnvCache {
    /// The freshest usable profile for `site_id` at `volume_time`, if one
    /// has landed. Read-only and allocation-free apart from the Arc clone —
    /// safe to call from render-request builders every frame.
    pub fn profile_for(
        &self,
        site_id: &str,
        volume_time: DateTime<Utc>,
    ) -> Option<Arc<EnvironmentalWindProfile>> {
        self.slots
            .iter()
            .filter(|((slot_site, _), _)| slot_site == site_id)
            .filter_map(|(_, slot)| match slot {
                EnvSlot::Ready(profile) if profile.usable_for(volume_time) => Some(profile),
                _ => None,
            })
            .min_by_key(|profile| {
                (profile.valid_time - volume_time)
                    .num_seconds()
                    .unsigned_abs()
            })
            .map(Arc::clone)
    }

    /// True while any fetch is in flight (the caller keeps a repaint timer
    /// alive so a landing profile repaints promptly even on an idle UI).
    pub fn any_pending(&self) -> bool {
        self.slots
            .values()
            .any(|slot| matches!(slot, EnvSlot::Pending))
    }

    /// Most recent fetch failure, for the status line.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Make sure a profile request is queued (or already resolved) for the
    /// best candidate cycle near `volume_time`. Never blocks: the fetch runs
    /// on the worker thread. Non-CONUS sites are skipped entirely — they run
    /// the engine's no-env path.
    pub fn ensure_requested(
        &mut self,
        site_id: &str,
        latitude_deg: f32,
        longitude_deg: f32,
        elevation_m: Option<f32>,
        volume_time: DateTime<Utc>,
        now: DateTime<Utc>,
    ) {
        if !site_in_rap_conus_box(latitude_deg, longitude_deg) {
            return;
        }
        for cycle in preferred_cycles(volume_time, now) {
            let key = (site_id.to_owned(), cycle.timestamp());
            match self.slots.get(&key) {
                Some(EnvSlot::Ready(_)) | Some(EnvSlot::Pending) => return,
                Some(EnvSlot::Failed { at }) if now - *at < FAILED_RETRY => continue,
                Some(EnvSlot::Failed { .. }) | None => {
                    let request = EnvProfileRequest {
                        site_id: site_id.to_owned(),
                        latitude_deg,
                        longitude_deg,
                        elevation_m,
                        cycle,
                    };
                    if self.worker_tx().send(request).is_ok() {
                        self.slots.insert(key, EnvSlot::Pending);
                    }
                    return;
                }
            }
        }
    }

    /// Drain worker responses into the cache. Returns true when a NEW
    /// profile landed (the caller invalidates dealias-dependent textures).
    pub fn pump(&mut self) -> bool {
        let Some(worker) = &self.worker else {
            return false;
        };
        let mut landed = false;
        while let Ok((key, result)) = worker.rx.try_recv() {
            match result {
                Ok(profile) => {
                    self.slots.insert(key, EnvSlot::Ready(Arc::new(profile)));
                    landed = true;
                }
                Err(message) => {
                    self.last_error = Some(message);
                    self.slots.insert(key, EnvSlot::Failed { at: Utc::now() });
                }
            }
        }
        landed
    }

    fn worker_tx(&mut self) -> &Sender<EnvProfileRequest> {
        if self.worker.is_none() {
            let (request_tx, request_rx) = channel::<EnvProfileRequest>();
            let (response_tx, response_rx) = channel::<EnvResponse>();
            std::thread::Builder::new()
                .name("dealias-env-worker".to_owned())
                .spawn(move || {
                    while let Ok(request) = request_rx.recv() {
                        let key = (request.site_id.clone(), request.cycle.timestamp());
                        let result = fetch_rap_profile(&request);
                        if response_tx.send((key, result)).is_err() {
                            return;
                        }
                    }
                })
                .expect("spawn dealias env worker");
            self.worker = Some(EnvWorker {
                tx: request_tx,
                rx: response_rx,
            });
        }
        &self.worker.as_ref().expect("worker just created").tx
    }
}

/// Rough RAP-CONUS pre-gate (see the constant docs).
pub fn site_in_rap_conus_box(latitude_deg: f32, longitude_deg: f32) -> bool {
    latitude_deg.is_finite()
        && longitude_deg.is_finite()
        && CONUS_LAT_RANGE.contains(&latitude_deg)
        && CONUS_LON_RANGE.contains(&longitude_deg)
}

/// Candidate RAP 0-h analysis cycles for a volume at `volume_time`, best
/// first: ordered by |cycle − volume_time|, published no later than
/// `now − publication lag`, and within the engine's 3 h staleness bound.
/// RAP cycles hourly; the publication lag comes from the same helper the
/// one-click ingest uses (`ingest_worker::publication_lag_minutes`).
pub fn preferred_cycles(volume_time: DateTime<Utc>, now: DateTime<Utc>) -> Vec<DateTime<Utc>> {
    let Ok(floor) = volume_time.duration_trunc(Duration::hours(1)) else {
        return Vec::new();
    };
    let lag = Duration::minutes(crate::ingest_worker::publication_lag_minutes(ModelId::Rap));
    let newest_published = now - lag;
    let mut candidates: Vec<DateTime<Utc>> = (-3i64..=4)
        .map(|offset| floor + Duration::hours(offset))
        .filter(|cycle| *cycle <= newest_published)
        .filter(|cycle| (*cycle - volume_time).abs() <= MAX_CYCLE_DISTANCE)
        .collect();
    candidates.sort_by_key(|cycle| {
        (
            (*cycle - volume_time).num_seconds().unsigned_abs(),
            // Deterministic tie-break at exactly ±30 min: prefer the
            // EARLIER cycle (guaranteed published longer).
            cycle.timestamp(),
        )
    });
    candidates
}

/// Fetch + decode one RAP 0-h analysis profile at the request's site.
/// Runs on the worker thread only.
fn fetch_rap_profile(request: &EnvProfileRequest) -> Result<EnvironmentalWindProfile, String> {
    let cycle_spec = rustwx_core::CycleSpec::new(
        request.cycle.format("%Y%m%d").to_string(),
        u8::try_from(chrono::Timelike::hour(&request.cycle)).unwrap_or(0),
    )
    .map_err(|err| format!("RAP cycle spec: {err}"))?;
    let run = rustwx_core::ModelRunRequest::new(ModelId::Rap, cycle_spec, 0, "awp130pgrb")
        .map_err(|err| format!("RAP run request: {err}"))?;
    let fetch = rustwx_io::FetchRequest {
        request: run,
        source_override: None,
        // `.idx` subset patterns (VAR:level-substring): every isobaric
        // UGRD/VGRD/HGT record plus the surface geopotential (terrain)
        // for the elevation fallback.
        variable_patterns: vec![
            "UGRD:mb".to_owned(),
            "VGRD:mb".to_owned(),
            "HGT:mb".to_owned(),
            "HGT:surface".to_owned(),
        ],
    };
    let fetched = rustwx_io::fetch_bytes_with_cache(&fetch, &settings::model_cache_dir(), true)
        .map_err(|err| {
            format!(
                "RAP {} f00 fetch: {err}",
                request.cycle.format("%Y%m%d %Hz")
            )
        })?;
    let extraction = rustwx_io::extract_field_values_partial_from_model_bytes_at_forecast_hour(
        ModelId::Rap,
        &fetched.result.bytes,
        None,
        &profile_selectors(),
        Some(0),
    )
    .map_err(|err| format!("RAP profile extract: {err}"))?;
    profile_from_extraction(
        &extraction,
        request.latitude_deg,
        request.longitude_deg,
        request.elevation_m,
        request.cycle,
    )
}

/// The GRIB selectors the profile needs: (u, v, height) on every RAP
/// pressure level, plus surface geopotential for the elevation fallback.
fn profile_selectors() -> Vec<FieldSelector> {
    let mut selectors = Vec::new();
    let mut level = RAP_LEVEL_MIN_HPA;
    while level <= RAP_LEVEL_MAX_HPA {
        selectors.push(FieldSelector::isobaric(CanonicalField::UWind, level));
        selectors.push(FieldSelector::isobaric(CanonicalField::VWind, level));
        selectors.push(FieldSelector::isobaric(
            CanonicalField::GeopotentialHeight,
            level,
        ));
        level += RAP_LEVEL_STEP_HPA;
    }
    selectors.push(FieldSelector::surface(CanonicalField::GeopotentialHeight));
    selectors
}

/// Nearest grid point to (lat, lon) with its distance in km
/// (equirectangular metric — exact enough at 13 km grid spacing).
fn nearest_grid_index(grid: &LatLonGrid, lat: f32, lon: f32) -> Option<(usize, f32)> {
    let cos_lat = lat.to_radians().cos().max(0.05);
    let mut best: Option<(usize, f32)> = None;
    for (index, (&point_lat, &point_lon)) in
        grid.lat_deg.iter().zip(grid.lon_deg.iter()).enumerate()
    {
        if !point_lat.is_finite() || !point_lon.is_finite() {
            continue;
        }
        let dlat = point_lat - lat;
        // Normalize the longitude difference (grids may use 0..360).
        let mut dlon = (point_lon - lon).rem_euclid(360.0);
        if dlon > 180.0 {
            dlon -= 360.0;
        }
        let metric = dlat * dlat + (dlon * cos_lat) * (dlon * cos_lat);
        if best.map(|(_, current)| metric < current).unwrap_or(true) {
            best = Some((index, metric));
        }
    }
    best.map(|(index, metric)| (index, metric.sqrt() * 111.32))
}

/// Build the engine's profile from a decoded extraction. Pure — unit
/// tested offline; the network path above stays a thin shell around it.
fn profile_from_extraction(
    extraction: &PartialValuesExtraction,
    latitude_deg: f32,
    longitude_deg: f32,
    elevation_m: Option<f32>,
    cycle: DateTime<Utc>,
) -> Result<EnvironmentalWindProfile, String> {
    // All awp130 fields share one grid; resolve the nearest point per
    // distinct grid up front so a mixed-grid file cannot mis-sample.
    let nearest_per_grid: Vec<Option<(usize, f32)>> = extraction
        .grids
        .iter()
        .map(|shared| nearest_grid_index(&shared.grid, latitude_deg, longitude_deg))
        .collect();
    let nearest_for = |grid_index: usize| -> Option<(usize, f32)> {
        nearest_per_grid.get(grid_index).copied().flatten()
    };

    let sample = |field: CanonicalField, level_hpa: Option<u16>| -> Option<f32> {
        let extracted = extraction.extracted.iter().find(|entry| {
            entry.selector.field == field
                && match level_hpa {
                    Some(level) => {
                        entry.selector.vertical == rustwx_core::VerticalSelector::IsobaricHpa(level)
                    }
                    None => entry.selector.vertical == rustwx_core::VerticalSelector::Surface,
                }
        })?;
        let (index, distance_km) = nearest_for(extracted.grid_index)?;
        if distance_km > MAX_GRID_POINT_DISTANCE_KM {
            return None;
        }
        extracted
            .values
            .get(index)
            .copied()
            .filter(|value| value.is_finite())
    };

    // CONUS guard, precisely: the site must sit ON the grid.
    let on_grid = extraction
        .extracted
        .first()
        .and_then(|entry| nearest_for(entry.grid_index))
        .is_some_and(|(_, distance_km)| distance_km <= MAX_GRID_POINT_DISTANCE_KM);
    if !on_grid {
        return Err(format!(
            "site ({latitude_deg:.2}, {longitude_deg:.2}) is outside the RAP CONUS grid \
             — the engine runs without a model anchor here"
        ));
    }

    // Station elevation, or the model terrain height at the grid point.
    let reference_elevation_m = elevation_m
        .filter(|value| value.is_finite())
        .or_else(|| sample(CanonicalField::GeopotentialHeight, None))
        .ok_or_else(|| "no station elevation and no model terrain height".to_owned())?;

    // 1000 hPa upward (ascending height); keep above-radar levels only.
    let mut levels = Vec::new();
    let mut previous_height = f32::NEG_INFINITY;
    let mut level_hpa = RAP_LEVEL_MAX_HPA;
    loop {
        let height_msl = sample(CanonicalField::GeopotentialHeight, Some(level_hpa));
        let u = sample(CanonicalField::UWind, Some(level_hpa));
        let v = sample(CanonicalField::VWind, Some(level_hpa));
        if let (Some(height_msl), Some(u), Some(v)) = (height_msl, u, v) {
            let height_arl = height_msl - reference_elevation_m;
            if height_arl > 0.0 && height_arl > previous_height {
                levels.push(EnvWindLevel {
                    height_m_arl: height_arl,
                    u_mps: u,
                    v_mps: v,
                });
                previous_height = height_arl;
            }
        }
        if level_hpa == RAP_LEVEL_MIN_HPA {
            break;
        }
        level_hpa -= RAP_LEVEL_STEP_HPA;
    }
    if levels.len() < MIN_PROFILE_LEVELS {
        return Err(format!(
            "only {} usable profile levels (need {MIN_PROFILE_LEVELS})",
            levels.len()
        ));
    }
    Ok(EnvironmentalWindProfile {
        levels,
        valid_time: cycle,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use rustwx_core::{GridProjection, GridShape, VerticalSelector};
    use rustwx_io::{ExtractedFieldValues, SharedExtractionGrid};

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    #[test]
    fn preferred_cycles_orders_by_distance_and_respects_publication_lag() {
        // Volume at 18:40Z, wall clock 18:45Z: 19z is neither published nor
        // initialized sensibly; 18z (40 min old volume-relative) is nearest
        // but 18z published only at ~18:55Z — so 17z leads.
        let volume = utc(2026, 7, 2, 18, 40);
        let now = utc(2026, 7, 2, 18, 45);
        let cycles = preferred_cycles(volume, now);
        assert_eq!(cycles.first(), Some(&utc(2026, 7, 2, 17, 0)));
        assert!(!cycles.contains(&utc(2026, 7, 2, 18, 0)), "18z unpublished");

        // Archive volume: everything is published, nearest cycle wins and
        // the list stays within the engine's 3 h staleness bound.
        let archive = utc(2013, 5, 20, 20, 5);
        let cycles = preferred_cycles(archive, utc(2026, 7, 2, 0, 0));
        assert_eq!(cycles.first(), Some(&utc(2013, 5, 20, 20, 0)));
        assert!(
            cycles
                .iter()
                .all(|cycle| (*cycle - archive).abs() <= Duration::hours(3)),
            "{cycles:?}"
        );
        // ±30 min tie: the earlier cycle wins deterministically.
        let half = utc(2013, 5, 20, 20, 30);
        let cycles = preferred_cycles(half, utc(2026, 7, 2, 0, 0));
        assert_eq!(cycles.first(), Some(&utc(2013, 5, 20, 20, 0)));
    }

    #[test]
    fn conus_box_admits_conus_and_rejects_intl_and_pacific_sites() {
        assert!(site_in_rap_conus_box(35.33, -97.28), "KTLX");
        assert!(site_in_rap_conus_box(38.81, -93.55), "KEAX");
        assert!(!site_in_rap_conus_box(61.16, -149.98), "PAHG Anchorage");
        assert!(!site_in_rap_conus_box(21.13, -157.18), "PHMO Molokai");
        assert!(!site_in_rap_conus_box(50.04, 8.56), "DWD Offenthal");
        assert!(!site_in_rap_conus_box(f32::NAN, -97.0));
    }

    /// A 3×3 synthetic grid around (35, -97): sampling picks the nearest
    /// point's column, heights convert MSL→ARL against the station
    /// elevation, below-radar levels drop, and the result is the engine's
    /// strictly-increasing shape.
    fn synthetic_extraction() -> PartialValuesExtraction {
        let lats = vec![34.9, 34.9, 34.9, 35.0, 35.0, 35.0, 35.1, 35.1, 35.1];
        let lons = vec![
            -97.1, -97.0, -96.9, -97.1, -97.0, -96.9, -97.1, -97.0, -96.9,
        ];
        let grid = LatLonGrid::new(GridShape { nx: 3, ny: 3 }, lats, lons).unwrap();
        let center = 4usize; // (35.0, -97.0)
        let field = |field: CanonicalField, vertical: VerticalSelector, center_value: f32| {
            let mut values = vec![-9999.0f32; 9];
            values[center] = center_value;
            ExtractedFieldValues {
                selector: FieldSelector::new(field, vertical),
                units: "test".to_owned(),
                values,
                grid_index: 0,
            }
        };
        let mut extracted = Vec::new();
        // 1000 hPa sits BELOW the 400 m station; 925/850/700/500 above.
        for (level, height, u, v) in [
            (1000u16, 110.0f32, 1.0f32, 2.0f32),
            (925, 780.0, 5.0, 6.0),
            (850, 1_480.0, 10.0, 11.0),
            (700, 3_090.0, 15.0, 16.0),
            (500, 5_800.0, 25.0, 26.0),
        ] {
            extracted.push(field(
                CanonicalField::GeopotentialHeight,
                VerticalSelector::IsobaricHpa(level),
                height,
            ));
            extracted.push(field(
                CanonicalField::UWind,
                VerticalSelector::IsobaricHpa(level),
                u,
            ));
            extracted.push(field(
                CanonicalField::VWind,
                VerticalSelector::IsobaricHpa(level),
                v,
            ));
        }
        extracted.push(field(
            CanonicalField::GeopotentialHeight,
            VerticalSelector::Surface,
            380.0,
        ));
        // Make the non-center values NaN so a wrong nearest-point pick
        // fails loudly instead of passing with plausible numbers.
        for entry in &mut extracted {
            for (index, value) in entry.values.iter_mut().enumerate() {
                if index != 4 {
                    *value = f32::NAN;
                }
            }
        }
        PartialValuesExtraction {
            extracted,
            missing: Vec::new(),
            grids: vec![SharedExtractionGrid {
                grid,
                projection: Some(GridProjection::Geographic),
            }],
        }
    }

    #[test]
    fn profile_builder_samples_nearest_point_and_drops_subterranean_levels() {
        let cycle = utc(2026, 7, 2, 18, 0);
        let profile =
            profile_from_extraction(&synthetic_extraction(), 35.02, -97.03, Some(400.0), cycle)
                .expect("profile");
        assert_eq!(profile.valid_time, cycle);
        // 1000 hPa (110 m MSL < 400 m station) dropped; 4 levels remain.
        assert_eq!(profile.levels.len(), 4);
        assert!((profile.levels[0].height_m_arl - 380.0).abs() < 0.5);
        assert!((profile.levels[0].u_mps - 5.0).abs() < 1e-3);
        assert!(
            profile
                .levels
                .windows(2)
                .all(|pair| pair[0].height_m_arl < pair[1].height_m_arl)
        );
        // The engine itself must accept what this module builds.
        assert!(profile.usable_for(cycle + Duration::minutes(90)));
    }

    #[test]
    fn profile_builder_falls_back_to_model_terrain_without_station_elevation() {
        let cycle = utc(2026, 7, 2, 18, 0);
        let profile = profile_from_extraction(&synthetic_extraction(), 35.0, -97.0, None, cycle)
            .expect("profile");
        // Terrain fallback is 380 m: 925 hPa now sits at 400 m ARL.
        assert!((profile.levels[0].height_m_arl - 400.0).abs() < 0.5);
    }

    #[test]
    fn profile_builder_rejects_off_grid_sites() {
        let cycle = utc(2026, 7, 2, 18, 0);
        // Anchorage vs the 3×3 Oklahoma grid: nearest point is very far.
        let err = profile_from_extraction(&synthetic_extraction(), 61.16, -149.98, None, cycle)
            .expect_err("off-grid site must not get a profile");
        assert!(err.contains("outside the RAP CONUS grid"), "{err}");
    }

    #[test]
    fn cache_serves_the_freshest_usable_profile_per_site() {
        let mut cache = DealiasEnvCache::default();
        let volume_time = utc(2026, 7, 2, 18, 40);
        let near = EnvironmentalWindProfile {
            levels: vec![
                EnvWindLevel {
                    height_m_arl: 100.0,
                    u_mps: 1.0,
                    v_mps: 1.0,
                },
                EnvWindLevel {
                    height_m_arl: 1_000.0,
                    u_mps: 2.0,
                    v_mps: 2.0,
                },
            ],
            valid_time: utc(2026, 7, 2, 18, 0),
        };
        let far = EnvironmentalWindProfile {
            valid_time: utc(2026, 7, 2, 16, 0),
            ..near.clone()
        };
        cache.slots.insert(
            ("KTLX".to_owned(), near.valid_time.timestamp()),
            EnvSlot::Ready(Arc::new(near)),
        );
        cache.slots.insert(
            ("KTLX".to_owned(), far.valid_time.timestamp()),
            EnvSlot::Ready(Arc::new(far)),
        );
        let picked = cache.profile_for("KTLX", volume_time).expect("profile");
        assert_eq!(picked.valid_time, utc(2026, 7, 2, 18, 0));
        assert!(cache.profile_for("KEAX", volume_time).is_none());
        // Outside the 3 h staleness bound nothing is served.
        assert!(cache.profile_for("KTLX", utc(2026, 7, 2, 23, 0)).is_none());
    }

    /// LIVE-NETWORK validation of the whole fetch path (idx-subset RAP
    /// download from `noaa-rap-pds` → GRIB decode → profile at KTLX).
    /// Ignored by default so the suite stays hermetic; run explicitly:
    /// `cargo test -p app_ui rap_profile_live -- --ignored --nocapture`.
    #[test]
    #[ignore = "network: fetches a real RAP analysis from the public AWS mirror"]
    fn rap_profile_live_fetch_builds_a_usable_ktlx_profile() {
        let now = Utc::now();
        let volume_time = now - Duration::hours(2); // safely published
        let cycle = *preferred_cycles(volume_time, now)
            .first()
            .expect("published cycle exists");
        let request = EnvProfileRequest {
            site_id: "KTLX".to_owned(),
            latitude_deg: 35.333,
            longitude_deg: -97.278,
            elevation_m: Some(370.0),
            cycle,
        };
        let profile = fetch_rap_profile(&request).expect("live RAP profile");
        assert!(profile.levels.len() >= MIN_PROFILE_LEVELS);
        assert!(profile.usable_for(volume_time));
        // A real Oklahoma column tops out well above 10 km ARL.
        assert!(profile.levels.last().expect("levels").height_m_arl > 10_000.0);
        println!(
            "live RAP {} profile: {} levels, lowest ({:.0} m ARL, u {:.1}, v {:.1})",
            cycle.format("%Y-%m-%d %Hz"),
            profile.levels.len(),
            profile.levels[0].height_m_arl,
            profile.levels[0].u_mps,
            profile.levels[0].v_mps,
        );
    }
}
