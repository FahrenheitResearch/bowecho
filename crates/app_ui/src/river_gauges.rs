//! NOAA/NWS National Water Prediction Service river gauges.
//!
//! The map catalogue is fetched in bounded viewport tiles only while the
//! persisted layer is enabled. Gauge details and the (larger) stage/flow
//! series are fetched lazily after a marker click. Every endpoint used here
//! is the anonymous, official `api.water.noaa.gov` NWPS application API.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use chrono::{DateTime, Datelike, Utc};
use eframe::egui;
use serde::Deserialize;
use ui_core::worker_slot::{SlotPoll, WorkerSlot};

use crate::{DisplayTimeZone, ViewerApp};

const NWPS_BASE: &str = "https://api.water.noaa.gov/nwps/v1";
const TILE_WIDTH_DEG: f32 = 24.0;
const TILE_HEIGHT_DEG: f32 = 18.0;
const VIEW_MARGIN_DEG: f32 = 1.0;
const REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);
const RETRY_INTERVAL: Duration = Duration::from_secs(45);
const MAX_TILES_PER_FETCH: usize = 4;
const MAX_CACHED_TILES: usize = 36;
const MAX_DRAWN_NORMAL_MARKERS: usize = 600;
const MARKER_SPACING_PX: f32 = 18.0;
const MARKER_HIT_RADIUS_PX: f32 = 10.0;
const OBS_STALE_AFTER: chrono::Duration = chrono::Duration::hours(6);
const FORECAST_HORIZON: chrono::Duration = chrono::Duration::days(14);

// NWPS locations cover the United States and territories. The broad box
// includes Alaska, Hawaii, Puerto Rico, and the lower 48 without querying
// irrelevant global tiles when the map is over another continent.
const US_WEST: f32 = -180.0;
const US_EAST: f32 = -60.0;
const US_SOUTH: f32 = 15.0;
const US_NORTH: f32 = 72.0;

#[derive(Clone, Copy, Debug)]
pub(crate) struct RiverViewport {
    pub(crate) west: f32,
    pub(crate) east: f32,
    pub(crate) south: f32,
    pub(crate) north: f32,
    pub(crate) center_lon: f32,
    pub(crate) approximate_lon_half_span: f32,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct TileKey {
    x: i16,
    y: i16,
}

impl TileKey {
    fn for_lon_lat(lon: f32, lat: f32) -> Self {
        Self {
            x: ((lon + 180.0) / TILE_WIDTH_DEG).floor() as i16,
            y: ((lat + 90.0) / TILE_HEIGHT_DEG).floor() as i16,
        }
    }

    fn bounds(self) -> BBox {
        let west = -180.0 + f32::from(self.x) * TILE_WIDTH_DEG;
        let south = -90.0 + f32::from(self.y) * TILE_HEIGHT_DEG;
        BBox {
            west: west.max(US_WEST),
            east: (west + TILE_WIDTH_DEG).min(US_EAST),
            south: south.max(US_SOUTH),
            north: (south + TILE_HEIGHT_DEG).min(US_NORTH),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct BBox {
    west: f32,
    east: f32,
    south: f32,
    north: f32,
}

impl BBox {
    fn valid(self) -> bool {
        self.west < self.east && self.south < self.north
    }

    fn contains(self, lon: f32, lat: f32) -> bool {
        lon >= self.west && lon <= self.east && lat >= self.south && lat <= self.north
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FloodCategory {
    Stale,
    NotDefined,
    NoFlooding,
    Action,
    Minor,
    Moderate,
    Major,
    Unknown,
}

impl FloodCategory {
    fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "major" | "major_flooding" => Self::Major,
            "moderate" | "moderate_flooding" => Self::Moderate,
            "minor" | "minor_flooding" => Self::Minor,
            "action" | "action_stage" => Self::Action,
            "no_flooding" | "none" => Self::NoFlooding,
            "not_defined" => Self::NotDefined,
            "obs_not_current" | "fcst_not_current" | "not_current" => Self::Stale,
            _ => Self::Unknown,
        }
    }

    fn severity(self) -> u8 {
        match self {
            Self::Stale => 0,
            Self::NotDefined | Self::Unknown => 1,
            Self::NoFlooding => 2,
            Self::Action => 3,
            Self::Minor => 4,
            Self::Moderate => 5,
            Self::Major => 6,
        }
    }

    fn is_action_or_flood(self) -> bool {
        self.severity() >= Self::Action.severity()
    }

    fn label(self) -> &'static str {
        match self {
            Self::Stale => "stale / not current",
            Self::NotDefined => "flood stage not defined",
            Self::NoFlooding => "no flooding",
            Self::Action => "action stage",
            Self::Minor => "minor flooding",
            Self::Moderate => "moderate flooding",
            Self::Major => "major flooding",
            Self::Unknown => "category unavailable",
        }
    }

    fn color(self) -> egui::Color32 {
        match self {
            Self::Stale => egui::Color32::from_rgb(105, 112, 122),
            Self::NotDefined | Self::Unknown => egui::Color32::from_rgb(140, 151, 166),
            Self::NoFlooding => egui::Color32::from_rgb(48, 158, 218),
            Self::Action => egui::Color32::from_rgb(245, 210, 52),
            Self::Minor => egui::Color32::from_rgb(255, 145, 35),
            Self::Moderate => egui::Color32::from_rgb(232, 62, 58),
            Self::Major => egui::Color32::from_rgb(208, 66, 218),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Quantity {
    value: f64,
    unit: String,
}

impl Quantity {
    fn label(&self) -> String {
        if self.value.abs() >= 1_000.0 {
            format!("{:.0} {}", self.value, self.unit)
        } else {
            format!("{:.2} {}", self.value, self.unit)
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct GaugeReading {
    primary: Option<Quantity>,
    secondary: Option<Quantity>,
    category: FloodCategory,
    valid_time: Option<DateTime<Utc>>,
}

impl GaugeReading {
    fn is_current_observation(&self, now: DateTime<Utc>) -> bool {
        self.category != FloodCategory::Stale
            && self.valid_time.is_some_and(|time| {
                time <= now + chrono::Duration::hours(1) && time >= now - OBS_STALE_AFTER
            })
    }

    fn is_current_forecast(&self, now: DateTime<Utc>) -> bool {
        self.category != FloodCategory::Stale
            && self.valid_time.is_some_and(|time| {
                time >= now - chrono::Duration::hours(2) && time <= now + FORECAST_HORIZON
            })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct GaugeSummary {
    lid: String,
    name: String,
    wfo: Option<String>,
    state: Option<String>,
    lat: f32,
    lon: f32,
    observed: Option<GaugeReading>,
    forecast: Option<GaugeReading>,
}

impl GaugeSummary {
    fn display_category(&self, now: DateTime<Utc>) -> (FloodCategory, bool) {
        let observed = self
            .observed
            .as_ref()
            .filter(|reading| reading.is_current_observation(now))
            .map(|reading| reading.category);
        let forecast = self
            .forecast
            .as_ref()
            .filter(|reading| reading.is_current_forecast(now))
            .map(|reading| reading.category);
        match (observed, forecast) {
            (Some(observed), Some(forecast)) if forecast.severity() > observed.severity() => {
                (forecast, true)
            }
            (Some(observed), _) => (observed, false),
            (None, Some(forecast)) => (forecast, true),
            _ => {
                let fallback = self
                    .observed
                    .as_ref()
                    .map(|reading| reading.category)
                    .or_else(|| self.forecast.as_ref().map(|reading| reading.category))
                    .unwrap_or(FloodCategory::Stale);
                if fallback == FloodCategory::NotDefined || fallback == FloodCategory::Unknown {
                    (fallback, false)
                } else {
                    (FloodCategory::Stale, false)
                }
            }
        }
    }

    fn current_readout(&self) -> Option<String> {
        let reading = self.observed.as_ref()?;
        let mut values = Vec::new();
        if let Some(primary) = &reading.primary {
            values.push(primary.label());
        }
        if let Some(secondary) = &reading.secondary {
            values.push(secondary.label());
        }
        (!values.is_empty()).then(|| values.join(" / "))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RiverGaugeMarkerPoint {
    pub(crate) lid: String,
    pub(crate) position: egui::Pos2,
    category: FloodCategory,
    forecast_driven: bool,
    name: String,
    readout: Option<String>,
}

#[derive(Debug)]
struct CachedTile {
    gauges: Vec<GaugeSummary>,
    fetched_at: Instant,
    last_used_tick: u64,
}

#[derive(Debug)]
struct TileBatch {
    tiles: Vec<(TileKey, Vec<GaugeSummary>)>,
    errors: Vec<(TileKey, String)>,
}

type TileFetchResult = Result<TileBatch, String>;

#[derive(Clone, Debug, PartialEq)]
struct FloodThresholds {
    unit: String,
    action: Option<f64>,
    minor: Option<f64>,
    moderate: Option<f64>,
    major: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
struct FloodImpact {
    stage: Option<f64>,
    statement: String,
}

#[derive(Clone, Debug, PartialEq)]
struct Attribution {
    title: String,
    text: String,
    url: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
struct GaugeDetail {
    summary: GaugeSummary,
    thresholds: FloodThresholds,
    impacts: Vec<FloodImpact>,
    attributions: Vec<Attribution>,
}

#[derive(Clone, Debug, PartialEq)]
struct HydroPoint {
    time: DateTime<Utc>,
    primary: Option<f64>,
    secondary: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
struct HydroSeries {
    primary_name: String,
    primary_unit: String,
    secondary_name: String,
    secondary_unit: String,
    points: Vec<HydroPoint>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Hydrograph {
    observed: Option<HydroSeries>,
    forecast: Option<HydroSeries>,
}

impl Hydrograph {
    fn forecast_crest(&self) -> Option<(DateTime<Utc>, f64, &str)> {
        let series = self.forecast.as_ref()?;
        let point = series
            .points
            .iter()
            .filter_map(|point| point.primary.map(|value| (point, value)))
            .max_by(|left, right| left.1.total_cmp(&right.1))?;
        Some((point.0.time, point.1, series.primary_unit.as_str()))
    }
}

#[derive(Debug)]
struct DetailBundle {
    lid: String,
    detail: GaugeDetail,
    hydrograph: Option<Hydrograph>,
    hydrograph_error: Option<String>,
}

type DetailFetchResult = Result<DetailBundle, String>;

#[derive(Debug)]
struct SelectedGauge {
    summary: GaugeSummary,
    detail: Option<GaugeDetail>,
    hydrograph: Option<Hydrograph>,
    status: String,
}

/// In-memory, bounded NWPS state. The catalogue cache is intentionally not
/// persisted: five-minute observations should never masquerade as fresh after
/// an application restart.
pub(crate) struct RiverGaugeState {
    tiles: HashMap<TileKey, CachedTile>,
    failures: HashMap<TileKey, Instant>,
    tile_worker: WorkerSlot<TileFetchResult>,
    detail_worker: WorkerSlot<DetailFetchResult>,
    in_flight_tiles: Vec<TileKey>,
    tick: u64,
    force_refresh: bool,
    selected: Option<SelectedGauge>,
    detail_open: bool,
    pub(crate) status: String,
    last_success: Option<Instant>,
}

impl Default for RiverGaugeState {
    fn default() -> Self {
        Self {
            tiles: HashMap::new(),
            failures: HashMap::new(),
            tile_worker: WorkerSlot::idle("nwps-river-gauge-tiles"),
            detail_worker: WorkerSlot::idle("nwps-river-gauge-detail"),
            in_flight_tiles: Vec::new(),
            tick: 0,
            force_refresh: false,
            selected: None,
            detail_open: false,
            status: "River gauges off".to_owned(),
            last_success: None,
        }
    }
}

impl RiverGaugeState {
    pub(crate) fn poll(&mut self) {
        match self.tile_worker.poll() {
            SlotPoll::Ready(Ok(batch)) => {
                let now = Instant::now();
                self.apply_tile_batch(batch, now);
            }
            SlotPoll::Ready(Err(error)) => {
                let now = Instant::now();
                for key in self.in_flight_tiles.drain(..) {
                    self.failures.insert(key, now);
                }
                self.force_refresh = false;
                self.status = format!("NWPS river gauges unavailable: {error}");
            }
            SlotPoll::Disconnected => {
                self.in_flight_tiles.clear();
                self.status = "NWPS river-gauge worker stopped unexpectedly".to_owned();
            }
            SlotPoll::Idle | SlotPoll::Pending => {}
        }

        match self.detail_worker.poll() {
            SlotPoll::Ready(Ok(bundle)) => {
                if let Some(selected) = &mut self.selected
                    && selected.summary.lid == bundle.lid
                {
                    selected.summary = bundle.detail.summary.clone();
                    selected.detail = Some(bundle.detail);
                    selected.hydrograph = bundle.hydrograph;
                    selected.status = bundle
                        .hydrograph_error
                        .map(|error| format!("Gauge loaded; hydrograph unavailable: {error}"))
                        .unwrap_or_else(|| "Official NWPS gauge data".to_owned());
                }
            }
            SlotPoll::Ready(Err(error)) => {
                if let Some(selected) = &mut self.selected {
                    selected.status = format!("Gauge details unavailable: {error}");
                }
            }
            SlotPoll::Disconnected => {
                if let Some(selected) = &mut self.selected {
                    selected.status = "Gauge-detail worker stopped unexpectedly".to_owned();
                }
            }
            SlotPoll::Idle | SlotPoll::Pending => {}
        }
    }

    fn apply_tile_batch(&mut self, batch: TileBatch, now: Instant) {
        // Cached tiles from an earlier refresh do not make an all-error batch
        // successful. Advance freshness only when this batch actually landed
        // at least one requested tile (an empty gauge list is still a valid
        // successful tile response).
        let successful_tiles = batch.tiles.len();
        let error_count = batch.errors.len();
        for (key, gauges) in batch.tiles {
            self.failures.remove(&key);
            self.tiles.insert(
                key,
                CachedTile {
                    gauges,
                    fetched_at: now,
                    last_used_tick: self.tick,
                },
            );
        }
        for (key, _) in &batch.errors {
            self.failures.insert(*key, now);
        }
        self.in_flight_tiles.clear();
        self.force_refresh = false;
        self.prune_tiles();
        if successful_tiles > 0 {
            self.last_success = Some(now);
        }
        self.status = if error_count == 0 {
            format!("{} NWPS gauges cached", self.gauge_count())
        } else if self.tiles.is_empty() {
            format!("NWPS unavailable ({error_count} tile errors)")
        } else {
            format!(
                "{} NWPS gauges cached; {error_count} tile{} unavailable",
                self.gauge_count(),
                if error_count == 1 { "" } else { "s" }
            )
        };
    }

    pub(crate) fn maybe_refresh(
        &mut self,
        ctx: &egui::Context,
        enabled: bool,
        viewport: RiverViewport,
    ) {
        if !enabled {
            return;
        }
        self.tick = self.tick.wrapping_add(1);
        let keys = tile_keys_for_view(viewport);
        if keys.is_empty() {
            self.status = "Outside NOAA/NWS river-gauge coverage".to_owned();
            return;
        }
        for key in &keys {
            if let Some(tile) = self.tiles.get_mut(key) {
                tile.last_used_tick = self.tick;
            }
        }
        if self.tile_worker.in_flight() {
            ctx.request_repaint_after(Duration::from_secs(1));
            return;
        }

        let mut due = keys
            .into_iter()
            .filter(|key| {
                let stale = self.force_refresh
                    || self
                        .tiles
                        .get(key)
                        .is_none_or(|tile| tile.fetched_at.elapsed() >= REFRESH_INTERVAL);
                let retry_ready = self
                    .failures
                    .get(key)
                    .is_none_or(|failed| failed.elapsed() >= RETRY_INTERVAL);
                stale && retry_ready
            })
            .collect::<Vec<_>>();
        due.sort_by_key(|key| {
            tile_distance_key(
                *key,
                viewport.center_lon,
                (viewport.south + viewport.north) * 0.5,
            )
        });
        due.truncate(MAX_TILES_PER_FETCH);
        if due.is_empty() {
            ctx.request_repaint_after(REFRESH_INTERVAL);
            return;
        }

        self.in_flight_tiles = due.clone();
        self.status = format!(
            "Loading {} NWPS map tile{}...",
            due.len(),
            if due.len() == 1 { "" } else { "s" }
        );
        let spawned = self.tile_worker.spawn(ctx, move |tx| {
            let result = fetch_tile_batch(&due);
            let _ = tx.send(result);
        });
        if !spawned {
            self.in_flight_tiles.clear();
        }
        ctx.request_repaint_after(Duration::from_secs(1));
    }

    pub(crate) fn request_refresh(&mut self) {
        self.force_refresh = true;
        self.failures.clear();
        self.status = "Refreshing visible NWPS gauges...".to_owned();
    }

    pub(crate) fn is_fetching(&self) -> bool {
        self.tile_worker.in_flight()
    }

    pub(crate) fn status_line(&self) -> String {
        if self.is_fetching() {
            return self.status.clone();
        }
        if let Some(last) = self.last_success {
            format!(
                "{} gauges - updated {}m ago",
                self.gauge_count(),
                last.elapsed().as_secs() / 60
            )
        } else {
            self.status.clone()
        }
    }

    pub(crate) fn gauge_count(&self) -> usize {
        let mut ids = HashSet::new();
        for gauge in self.tiles.values().flat_map(|tile| &tile.gauges) {
            ids.insert(gauge.lid.as_str());
        }
        ids.len()
    }

    pub(crate) fn marker_points(
        &self,
        rect: egui::Rect,
        now: DateTime<Utc>,
        state_filter_enabled: bool,
        selected_states: &[String],
        mut project: impl FnMut(f32, f32) -> egui::Pos2,
    ) -> Vec<RiverGaugeMarkerPoint> {
        let mut by_lid: HashMap<&str, &GaugeSummary> = HashMap::new();
        for gauge in self.tiles.values().flat_map(|tile| &tile.gauges) {
            by_lid.entry(&gauge.lid).or_insert(gauge);
        }
        let selected_lid = self
            .selected
            .as_ref()
            .map(|selected| selected.summary.lid.as_str());
        let mut candidates = by_lid
            .into_values()
            .filter(|gauge| {
                river_gauge_visible_for_states(gauge, state_filter_enabled, selected_states)
            })
            .filter_map(|gauge| {
                let position = project(gauge.lon, gauge.lat);
                rect.expand(12.0).contains(position).then(|| {
                    let (category, forecast_driven) = gauge.display_category(now);
                    RiverGaugeMarkerPoint {
                        lid: gauge.lid.clone(),
                        position,
                        category,
                        forecast_driven,
                        name: gauge.name.clone(),
                        readout: gauge.current_readout(),
                    }
                })
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| {
            let left_selected = selected_lid == Some(left.lid.as_str());
            let right_selected = selected_lid == Some(right.lid.as_str());
            right_selected
                .cmp(&left_selected)
                .then_with(|| right.category.severity().cmp(&left.category.severity()))
                .then_with(|| left.lid.cmp(&right.lid))
        });
        declutter_markers(candidates)
    }

    pub(crate) fn draw_markers(
        &self,
        painter: &egui::Painter,
        points: &[RiverGaugeMarkerPoint],
        hovered_lid: Option<&str>,
    ) {
        let selected_lid = self
            .selected
            .as_ref()
            .map(|selected| selected.summary.lid.as_str());
        for point in points {
            let selected = selected_lid == Some(point.lid.as_str());
            let hovered = hovered_lid == Some(point.lid.as_str());
            let radius = if selected || hovered { 5.5 } else { 4.0 };
            painter.circle_filled(
                point.position,
                radius + 1.5,
                egui::Color32::from_black_alpha(210),
            );
            painter.circle_filled(point.position, radius, point.category.color());
            if point.forecast_driven {
                painter.circle_stroke(
                    point.position,
                    radius + 2.6,
                    egui::Stroke::new(1.2_f32, egui::Color32::from_rgb(255, 205, 80)),
                );
            }
            if selected {
                painter.circle_stroke(
                    point.position,
                    radius + 4.2,
                    egui::Stroke::new(1.5_f32, egui::Color32::WHITE),
                );
            }
            if hovered {
                let value = point
                    .readout
                    .as_deref()
                    .map(|value| format!(" - {value}"))
                    .unwrap_or_default();
                let forecast = if point.forecast_driven {
                    " - forecast"
                } else {
                    ""
                };
                let text = format!(
                    "{} ({}) - {}{}{}",
                    point.name,
                    point.lid,
                    point.category.label(),
                    value,
                    forecast
                );
                let width = (text.chars().count() as f32 * 6.2 + 14.0).min(480.0);
                let chip = egui::Rect::from_min_size(
                    point.position + egui::vec2(9.0, -26.0),
                    egui::vec2(width, 20.0),
                );
                painter.rect_filled(
                    chip,
                    4.0,
                    egui::Color32::from_rgba_unmultiplied(12, 17, 23, 238),
                );
                painter.text(
                    chip.left_center() + egui::vec2(7.0, 0.0),
                    egui::Align2::LEFT_CENTER,
                    text,
                    egui::FontId::proportional(11.0),
                    egui::Color32::WHITE,
                );
            }
        }
    }

    pub(crate) fn select(&mut self, lid: &str, ctx: &egui::Context) -> bool {
        let Some(summary) = self.find_summary(lid).cloned() else {
            return false;
        };
        self.detail_open = true;
        let already_loaded = self
            .selected
            .as_ref()
            .is_some_and(|selected| selected.summary.lid == lid && selected.detail.is_some());
        if already_loaded {
            return true;
        }
        self.selected = Some(SelectedGauge {
            summary,
            detail: None,
            hydrograph: None,
            status: "Loading official NWPS gauge details...".to_owned(),
        });
        self.detail_worker.cancel();
        let lid_owned = lid.to_owned();
        self.detail_worker.spawn(ctx, move |tx| {
            let result = fetch_detail_bundle(&lid_owned);
            let _ = tx.send(result);
        });
        true
    }

    pub(crate) fn details_ui(&mut self, ctx: &egui::Context, time_zone: DisplayTimeZone) {
        if !self.detail_open {
            return;
        }
        let mut open = true;
        egui::Window::new("River gauge")
            .default_width(430.0)
            .min_width(340.0)
            .resizable(true)
            .open(&mut open)
            .show(ctx, |ui| {
                let Some(selected) = &self.selected else {
                    ui.weak("Select a river-gauge marker on the map.");
                    return;
                };
                let summary = selected
                    .detail
                    .as_ref()
                    .map(|detail| &detail.summary)
                    .unwrap_or(&selected.summary);
                ui.heading(&summary.name);
                let mut location = vec![summary.lid.clone()];
                if let Some(state) = &summary.state {
                    location.push(state.clone());
                }
                if let Some(wfo) = &summary.wfo {
                    location.push(format!("WFO {wfo}"));
                }
                ui.weak(location.join(" - "));
                ui.add_space(4.0);
                reading_ui(ui, "Observed", summary.observed.as_ref(), time_zone);
                reading_ui(ui, "Forecast", summary.forecast.as_ref(), time_zone);

                if let Some(hydrograph) = &selected.hydrograph {
                    if let Some((time, value, unit)) = hydrograph.forecast_crest() {
                        ui.label(format!(
                            "Forecast crest: {value:.2} {unit} at {}",
                            time_zone.format_date_hm(time)
                        ));
                    }
                    if let Some(detail) = &selected.detail {
                        draw_hydrograph(ui, hydrograph, &detail.thresholds, time_zone);
                    }
                } else if self.detail_worker.in_flight() {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.weak("Loading thresholds and hydrograph...");
                    });
                }

                if let Some(detail) = &selected.detail {
                    threshold_ui(ui, &detail.thresholds);
                    if !detail.impacts.is_empty() {
                        egui::CollapsingHeader::new("Flood impacts")
                            .default_open(false)
                            .show(ui, |ui| {
                                for impact in detail.impacts.iter().take(8) {
                                    let prefix = impact
                                        .stage
                                        .map(|stage| format!("{stage:.1} {}: ", detail.thresholds.unit))
                                        .unwrap_or_default();
                                    ui.label(format!("{prefix}{}", impact.statement));
                                }
                            });
                    }
                    egui::CollapsingHeader::new("Data attribution")
                        .default_open(false)
                        .show(ui, |ui| {
                            ui.weak("NOAA/NWS National Water Prediction Service");
                            for attribution in &detail.attributions {
                                if let Some(url) = &attribution.url {
                                    ui.hyperlink_to(&attribution.title, url);
                                } else {
                                    ui.label(&attribution.title);
                                }
                                if attribution.text != attribution.title {
                                    ui.weak(&attribution.text);
                                }
                            }
                        });
                }

                ui.separator();
                ui.weak(&selected.status);
                ui.weak("Provisional observations and forecasts; verify official guidance for life-safety decisions.");
                ui.hyperlink_to(
                    "Open this gauge on NWPS",
                    format!("https://water.noaa.gov/gauges/{}", summary.lid),
                );
            });
        self.detail_open = open;
    }

    fn find_summary(&self, lid: &str) -> Option<&GaugeSummary> {
        self.tiles
            .values()
            .flat_map(|tile| &tile.gauges)
            .find(|gauge| gauge.lid == lid)
    }

    fn prune_tiles(&mut self) {
        if self.tiles.len() <= MAX_CACHED_TILES {
            return;
        }
        let mut oldest = self
            .tiles
            .iter()
            .map(|(key, tile)| (*key, tile.last_used_tick))
            .collect::<Vec<_>>();
        oldest.sort_by_key(|(_, tick)| *tick);
        for (key, _) in oldest.into_iter().take(self.tiles.len() - MAX_CACHED_TILES) {
            self.tiles.remove(&key);
            self.failures.remove(&key);
        }
    }
}

fn river_gauge_visible_for_states(
    gauge: &GaugeSummary,
    filter_enabled: bool,
    selected_states: &[String],
) -> bool {
    !filter_enabled
        || gauge.state.as_deref().is_some_and(|state| {
            selected_states
                .iter()
                .any(|selected| selected.eq_ignore_ascii_case(state))
        })
}

pub(crate) fn nearest_marker(
    points: &[RiverGaugeMarkerPoint],
    pointer: egui::Pos2,
) -> Option<&RiverGaugeMarkerPoint> {
    points
        .iter()
        .filter_map(|point| {
            let distance = point.position.distance(pointer);
            (distance <= MARKER_HIT_RADIUS_PX).then_some((point, distance))
        })
        .min_by(|left, right| left.1.total_cmp(&right.1))
        .map(|(point, _)| point)
}

impl ViewerApp {
    pub(crate) fn drive_river_gauges_for_view(&mut self, ctx: &egui::Context, rect: egui::Rect) {
        let bounds = self.visible_geo_bounds(rect);
        let half_span =
            (rect.width() * 0.5 / self.lon_pixels_per_degree().max(0.05)).clamp(0.25, 180.0);
        self.river_gauges.maybe_refresh(
            ctx,
            self.app_settings.overlay_river_gauges,
            RiverViewport {
                west: bounds.west,
                east: bounds.east,
                south: bounds.south,
                north: bounds.north,
                center_lon: self.map_center_lon,
                approximate_lon_half_span: half_span,
            },
        );
    }

    pub(crate) fn river_gauge_marker_points(&self, rect: egui::Rect) -> Vec<RiverGaugeMarkerPoint> {
        if !self.app_settings.overlay_river_gauges {
            return Vec::new();
        }
        self.river_gauges.marker_points(
            rect,
            Utc::now(),
            self.app_settings.overlay_river_gauge_state_filter_enabled,
            &self.app_settings.overlay_river_gauge_states,
            |lon, lat| self.lon_lat_to_screen(rect, lon, lat),
        )
    }

    pub(crate) fn draw_river_gauge_markers(
        &self,
        painter: &egui::Painter,
        points: &[RiverGaugeMarkerPoint],
        hovered_lid: Option<&str>,
    ) {
        self.river_gauges.draw_markers(painter, points, hovered_lid);
    }

    pub(crate) fn select_river_gauge_marker(
        &mut self,
        points: &[RiverGaugeMarkerPoint],
        pointer: egui::Pos2,
        ctx: &egui::Context,
    ) -> bool {
        let Some(lid) = nearest_marker(points, pointer).map(|point| point.lid.clone()) else {
            return false;
        };
        self.river_gauges.select(&lid, ctx)
    }

    pub(crate) fn show_river_gauge_details(&mut self, ctx: &egui::Context) {
        let time_zone = self.time_zone();
        self.river_gauges.details_ui(ctx, time_zone);
    }
}

fn tile_keys_for_view(viewport: RiverViewport) -> Vec<TileKey> {
    let south = (viewport.south - VIEW_MARGIN_DEG).max(US_SOUTH);
    let north = (viewport.north + VIEW_MARGIN_DEG).min(US_NORTH);
    if south >= north {
        return Vec::new();
    }
    let raw_width = viewport.east - viewport.west;
    let longitude_ranges = if raw_width > 180.0 && viewport.approximate_lon_half_span < 120.0 {
        let west = viewport.center_lon - viewport.approximate_lon_half_span - VIEW_MARGIN_DEG;
        let east = viewport.center_lon + viewport.approximate_lon_half_span + VIEW_MARGIN_DEG;
        if west < -180.0 {
            vec![(west + 360.0, 180.0), (-180.0, east)]
        } else if east > 180.0 {
            vec![(west, 180.0), (-180.0, east - 360.0)]
        } else {
            vec![(west, east)]
        }
    } else {
        vec![(
            viewport.west - VIEW_MARGIN_DEG,
            viewport.east + VIEW_MARGIN_DEG,
        )]
    };

    let mut keys = HashSet::new();
    for (west, east) in longitude_ranges {
        let west = west.max(US_WEST);
        let east = east.min(US_EAST);
        if west >= east {
            continue;
        }
        let first = TileKey::for_lon_lat(west, south);
        let last = TileKey::for_lon_lat(
            (east - f32::EPSILON).max(west),
            (north - f32::EPSILON).max(south),
        );
        for y in first.y..=last.y {
            for x in first.x..=last.x {
                let key = TileKey { x, y };
                if key.bounds().valid() {
                    keys.insert(key);
                }
            }
        }
    }
    let mut keys = keys.into_iter().collect::<Vec<_>>();
    keys.sort();
    keys
}

fn tile_distance_key(key: TileKey, lon: f32, lat: f32) -> u32 {
    let bounds = key.bounds();
    let center_lon = (bounds.west + bounds.east) * 0.5;
    let center_lat = (bounds.south + bounds.north) * 0.5;
    (((center_lon - lon).abs() + (center_lat - lat).abs()) * 100.0) as u32
}

fn fetch_tile_batch(keys: &[TileKey]) -> TileFetchResult {
    if keys.is_empty() {
        return Err("empty NWPS tile request".to_owned());
    }
    let mut tiles = Vec::new();
    let mut errors = Vec::new();
    for key in keys {
        match fetch_tile(*key) {
            Ok(gauges) => tiles.push((*key, gauges)),
            Err(error) => errors.push((*key, error)),
        }
    }
    if tiles.is_empty() {
        return Err(errors
            .into_iter()
            .map(|(_, error)| error)
            .collect::<Vec<_>>()
            .join("; "));
    }
    Ok(TileBatch { tiles, errors })
}

fn fetch_tile(key: TileKey) -> Result<Vec<GaugeSummary>, String> {
    let bounds = key.bounds();
    let url = gauge_catalog_url(bounds);
    let text = data_source::fetch_text(&url).map_err(|error| error.to_string())?;
    let mut gauges = parse_gauge_catalog(&text)?;
    // The API treats bbox edges as inclusive, so adjacent tiles can repeat a
    // boundary gauge. Retaining only this tile's box plus lid dedup at draw
    // time keeps the cache deterministic.
    gauges.retain(|gauge| bounds.contains(gauge.lon, gauge.lat));
    Ok(gauges)
}

fn gauge_catalog_url(bounds: BBox) -> String {
    format!(
        "{NWPS_BASE}/gauges?bbox.xmin={:.4}&bbox.ymin={:.4}&bbox.xmax={:.4}&bbox.ymax={:.4}&srid=EPSG_4326",
        bounds.west, bounds.south, bounds.east, bounds.north
    )
}

fn fetch_detail_bundle(lid: &str) -> DetailFetchResult {
    let detail_url = format!("{NWPS_BASE}/gauges/{lid}");
    let detail_text = data_source::fetch_text(&detail_url).map_err(|error| error.to_string())?;
    let detail = parse_gauge_detail(&detail_text)?;
    let hydro_url = format!("{NWPS_BASE}/gauges/{lid}/stageflow");
    let (hydrograph, hydrograph_error) = match data_source::fetch_text(&hydro_url) {
        Ok(text) => match parse_hydrograph(&text) {
            Ok(hydrograph) => (Some(hydrograph), None),
            Err(error) => (None, Some(error)),
        },
        Err(error) => (None, Some(error.to_string())),
    };
    Ok(DetailBundle {
        lid: lid.to_owned(),
        detail,
        hydrograph,
        hydrograph_error,
    })
}

#[derive(Deserialize)]
struct ApiGaugeCollection {
    #[serde(default)]
    gauges: Vec<ApiGauge>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiGauge {
    lid: String,
    name: String,
    latitude: f64,
    longitude: f64,
    wfo: Option<ApiOffice>,
    state: Option<ApiOffice>,
    status: Option<ApiGaugeStatus>,
}

#[derive(Deserialize)]
struct ApiOffice {
    abbreviation: Option<String>,
}

#[derive(Deserialize)]
struct ApiGaugeStatus {
    observed: Option<ApiReading>,
    forecast: Option<ApiReading>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiReading {
    primary: Option<f64>,
    primary_unit: Option<String>,
    secondary: Option<f64>,
    secondary_unit: Option<String>,
    flood_category: Option<String>,
    valid_time: Option<String>,
}

fn parse_gauge_catalog(text: &str) -> Result<Vec<GaugeSummary>, String> {
    let collection: ApiGaugeCollection =
        serde_json::from_str(text).map_err(|error| error.to_string())?;
    let mut gauges = collection
        .gauges
        .into_iter()
        .filter_map(gauge_from_api)
        .collect::<Vec<_>>();
    gauges.sort_by(|left, right| left.lid.cmp(&right.lid));
    gauges.dedup_by(|left, right| left.lid == right.lid);
    Ok(gauges)
}

fn gauge_from_api(gauge: ApiGauge) -> Option<GaugeSummary> {
    let lat = gauge.latitude as f32;
    let lon = gauge.longitude as f32;
    if gauge.lid.trim().is_empty()
        || gauge.name.trim().is_empty()
        || !lat.is_finite()
        || !lon.is_finite()
        || !(-90.0..=90.0).contains(&lat)
        || !(-180.0..=180.0).contains(&lon)
    {
        return None;
    }
    let status = gauge.status;
    Some(GaugeSummary {
        lid: gauge.lid,
        name: gauge.name,
        wfo: gauge.wfo.and_then(|office| nonempty(office.abbreviation)),
        state: gauge.state.and_then(|state| nonempty(state.abbreviation)),
        lat,
        lon,
        observed: status
            .as_ref()
            .and_then(|status| status.observed.as_ref())
            .map(reading_from_api),
        forecast: status
            .as_ref()
            .and_then(|status| status.forecast.as_ref())
            .map(reading_from_api),
    })
}

fn reading_from_api(reading: &ApiReading) -> GaugeReading {
    GaugeReading {
        primary: quantity(reading.primary, reading.primary_unit.as_deref()),
        secondary: quantity(reading.secondary, reading.secondary_unit.as_deref()),
        category: reading
            .flood_category
            .as_deref()
            .map(FloodCategory::parse)
            .unwrap_or(FloodCategory::Unknown),
        valid_time: reading.valid_time.as_deref().and_then(parse_api_time),
    }
}

fn quantity(value: Option<f64>, unit: Option<&str>) -> Option<Quantity> {
    let value = value?;
    let unit = unit?.trim();
    (value.is_finite() && value > -900.0 && !unit.is_empty()).then(|| Quantity {
        value,
        unit: unit.to_owned(),
    })
}

fn parse_api_time(value: &str) -> Option<DateTime<Utc>> {
    let time = DateTime::parse_from_rfc3339(value)
        .ok()?
        .with_timezone(&Utc);
    (time.year() >= 1900).then_some(time)
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.and_then(|value| (!value.trim().is_empty()).then_some(value))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiGaugeDetail {
    #[serde(flatten)]
    gauge: ApiGauge,
    flood: Option<ApiFlood>,
    #[serde(default)]
    data_attribution: Vec<ApiAttribution>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiFlood {
    stage_units: Option<String>,
    categories: Option<ApiCategories>,
    #[serde(default)]
    impacts: Vec<ApiImpact>,
}

#[derive(Deserialize)]
struct ApiCategories {
    action: Option<ApiThreshold>,
    minor: Option<ApiThreshold>,
    moderate: Option<ApiThreshold>,
    major: Option<ApiThreshold>,
}

#[derive(Deserialize)]
struct ApiThreshold {
    stage: Option<f64>,
}

#[derive(Deserialize)]
struct ApiImpact {
    stage: Option<f64>,
    statement: Option<String>,
}

#[derive(Deserialize)]
struct ApiAttribution {
    title: Option<String>,
    text: Option<String>,
    url: Option<String>,
}

fn parse_gauge_detail(text: &str) -> Result<GaugeDetail, String> {
    let api: ApiGaugeDetail = serde_json::from_str(text).map_err(|error| error.to_string())?;
    let summary = gauge_from_api(api.gauge)
        .ok_or_else(|| "NWPS detail has invalid gauge metadata".to_owned())?;
    let flood = api.flood;
    let unit = flood
        .as_ref()
        .and_then(|flood| flood.stage_units.clone())
        .filter(|unit| !unit.trim().is_empty())
        .unwrap_or_else(|| "stage units unavailable".to_owned());
    let categories = flood.as_ref().and_then(|flood| flood.categories.as_ref());
    let threshold =
        |value: Option<&ApiThreshold>| value.and_then(|value| valid_api_number(value.stage));
    let thresholds = FloodThresholds {
        unit,
        action: threshold(categories.and_then(|categories| categories.action.as_ref())),
        minor: threshold(categories.and_then(|categories| categories.minor.as_ref())),
        moderate: threshold(categories.and_then(|categories| categories.moderate.as_ref())),
        major: threshold(categories.and_then(|categories| categories.major.as_ref())),
    };
    let impacts = flood
        .map(|flood| {
            flood
                .impacts
                .into_iter()
                .filter_map(|impact| {
                    let statement = impact.statement?.trim().to_owned();
                    (!statement.is_empty()).then(|| FloodImpact {
                        stage: valid_api_number(impact.stage),
                        statement,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let attributions = api
        .data_attribution
        .into_iter()
        .filter_map(|attribution| {
            let title =
                nonempty(attribution.title).or_else(|| nonempty(attribution.text.clone()))?;
            let text = nonempty(attribution.text).unwrap_or_else(|| title.clone());
            Some(Attribution {
                title,
                text,
                url: nonempty(attribution.url),
            })
        })
        .collect();
    Ok(GaugeDetail {
        summary,
        thresholds,
        impacts,
        attributions,
    })
}

fn valid_api_number(value: Option<f64>) -> Option<f64> {
    value.filter(|value| value.is_finite() && *value > -900.0)
}

#[derive(Deserialize)]
struct ApiHydrograph {
    observed: Option<ApiHydroSeries>,
    forecast: Option<ApiHydroSeries>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiHydroSeries {
    primary_name: Option<String>,
    primary_units: Option<String>,
    secondary_name: Option<String>,
    secondary_units: Option<String>,
    #[serde(default)]
    data: Vec<ApiHydroPoint>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiHydroPoint {
    valid_time: Option<String>,
    primary: Option<f64>,
    secondary: Option<f64>,
}

fn parse_hydrograph(text: &str) -> Result<Hydrograph, String> {
    let api: ApiHydrograph = serde_json::from_str(text).map_err(|error| error.to_string())?;
    let observed = api
        .observed
        .and_then(|series| hydro_series_from_api(series, true));
    let forecast = api
        .forecast
        .and_then(|series| hydro_series_from_api(series, false));
    if observed.is_none() && forecast.is_none() {
        return Err("NWPS stageflow response has no usable series".to_owned());
    }
    Ok(Hydrograph { observed, forecast })
}

fn hydro_series_from_api(series: ApiHydroSeries, trim_observed: bool) -> Option<HydroSeries> {
    let mut points = series
        .data
        .into_iter()
        .filter_map(|point| {
            let time = point.valid_time.as_deref().and_then(parse_api_time)?;
            let primary = valid_api_number(point.primary);
            let secondary = valid_api_number(point.secondary);
            (primary.is_some() || secondary.is_some()).then_some(HydroPoint {
                time,
                primary,
                secondary,
            })
        })
        .collect::<Vec<_>>();
    points.sort_by_key(|point| point.time);
    points.dedup_by_key(|point| point.time);
    if trim_observed && let Some(latest) = points.last().map(|point| point.time) {
        let cutoff = latest - chrono::Duration::hours(72);
        points.retain(|point| point.time >= cutoff);
    }
    if points.is_empty() {
        return None;
    }
    Some(HydroSeries {
        primary_name: nonempty(series.primary_name).unwrap_or_else(|| "Stage/flow".to_owned()),
        primary_unit: nonempty(series.primary_units).unwrap_or_default(),
        secondary_name: nonempty(series.secondary_name).unwrap_or_default(),
        secondary_unit: nonempty(series.secondary_units).unwrap_or_default(),
        points,
    })
}

fn declutter_markers(mut candidates: Vec<RiverGaugeMarkerPoint>) -> Vec<RiverGaugeMarkerPoint> {
    // Caller sorts selected/flooding markers first. Flood/action markers are
    // never dropped; normal markers occupy a small screen grid and are capped.
    let mut occupied = HashSet::new();
    let mut normal_count = 0usize;
    candidates.retain(|candidate| {
        if candidate.category.is_action_or_flood() {
            return true;
        }
        if normal_count >= MAX_DRAWN_NORMAL_MARKERS {
            return false;
        }
        let cell = (
            (candidate.position.x / MARKER_SPACING_PX).floor() as i32,
            (candidate.position.y / MARKER_SPACING_PX).floor() as i32,
        );
        if occupied.insert(cell) {
            normal_count += 1;
            true
        } else {
            false
        }
    });
    candidates
}

fn reading_ui(
    ui: &mut egui::Ui,
    label: &str,
    reading: Option<&GaugeReading>,
    time_zone: DisplayTimeZone,
) {
    ui.horizontal_wrapped(|ui| {
        ui.strong(label);
        let Some(reading) = reading else {
            ui.weak("unavailable");
            return;
        };
        if let Some(primary) = &reading.primary {
            ui.label(primary.label());
        }
        if let Some(secondary) = &reading.secondary {
            ui.weak(format!("/ {}", secondary.label()));
        }
        ui.colored_label(reading.category.color(), reading.category.label());
        if let Some(time) = reading.valid_time {
            ui.weak(format!("valid {}", time_zone.format_date_hm(time)));
        } else {
            ui.weak("valid time unavailable");
        }
    });
}

fn threshold_ui(ui: &mut egui::Ui, thresholds: &FloodThresholds) {
    let values = [
        ("Action", thresholds.action, FloodCategory::Action),
        ("Minor", thresholds.minor, FloodCategory::Minor),
        ("Moderate", thresholds.moderate, FloodCategory::Moderate),
        ("Major", thresholds.major, FloodCategory::Major),
    ];
    if values.iter().all(|(_, value, _)| value.is_none()) {
        ui.weak("Flood thresholds are not defined for this gauge.");
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.strong("Flood thresholds");
        for (label, value, category) in values {
            if let Some(value) = value {
                ui.colored_label(
                    category.color(),
                    format!("{label} {value:.1} {}", thresholds.unit),
                );
            }
        }
    });
}

fn draw_hydrograph(
    ui: &mut egui::Ui,
    hydrograph: &Hydrograph,
    thresholds: &FloodThresholds,
    time_zone: DisplayTimeZone,
) {
    let mut all = Vec::new();
    if let Some(observed) = &hydrograph.observed {
        all.extend(
            observed
                .points
                .iter()
                .filter_map(|point| point.primary.map(|value| (point.time, value))),
        );
    }
    if let Some(forecast) = &hydrograph.forecast {
        all.extend(
            forecast
                .points
                .iter()
                .filter_map(|point| point.primary.map(|value| (point.time, value))),
        );
    }
    if all.len() < 2 {
        return;
    }
    let min_time = all.iter().map(|(time, _)| *time).min().unwrap();
    let max_time = all.iter().map(|(time, _)| *time).max().unwrap();
    let mut min_value = all
        .iter()
        .map(|(_, value)| *value)
        .fold(f64::INFINITY, f64::min);
    let mut max_value = all
        .iter()
        .map(|(_, value)| *value)
        .fold(f64::NEG_INFINITY, f64::max);
    for value in [
        thresholds.action,
        thresholds.minor,
        thresholds.moderate,
        thresholds.major,
    ]
    .into_iter()
    .flatten()
    {
        min_value = min_value.min(value);
        max_value = max_value.max(value);
    }
    if !min_value.is_finite() || !max_value.is_finite() {
        return;
    }
    if (max_value - min_value).abs() < 1e-6 {
        min_value -= 1.0;
        max_value += 1.0;
    }
    let (response, painter) = ui.allocate_painter(
        egui::vec2(ui.available_width().max(260.0), 142.0),
        egui::Sense::hover(),
    );
    let plot = response.rect.shrink2(egui::vec2(8.0, 16.0));
    painter.rect_filled(plot, 2.0, egui::Color32::from_rgb(12, 17, 23));
    painter.rect_stroke(
        plot,
        2.0,
        egui::Stroke::new(1.0_f32, egui::Color32::from_gray(60)),
        egui::StrokeKind::Inside,
    );
    let total_seconds = (max_time - min_time).num_seconds().max(1) as f64;
    let position = |time: DateTime<Utc>, value: f64| {
        let x = plot.left()
            + ((time - min_time).num_seconds() as f64 / total_seconds) as f32 * plot.width();
        let y =
            plot.bottom() - ((value - min_value) / (max_value - min_value)) as f32 * plot.height();
        egui::pos2(x, y)
    };
    for (value, category) in [
        (thresholds.action, FloodCategory::Action),
        (thresholds.minor, FloodCategory::Minor),
        (thresholds.moderate, FloodCategory::Moderate),
        (thresholds.major, FloodCategory::Major),
    ] {
        if let Some(value) = value {
            let y = position(min_time, value).y;
            painter.line_segment(
                [egui::pos2(plot.left(), y), egui::pos2(plot.right(), y)],
                egui::Stroke::new(0.8_f32, category.color().gamma_multiply(0.65)),
            );
        }
    }
    let draw_series = |series: &HydroSeries, color: egui::Color32, painter: &egui::Painter| {
        let points = series
            .points
            .iter()
            .filter_map(|point| point.primary.map(|value| position(point.time, value)))
            .collect::<Vec<_>>();
        if points.len() >= 2 {
            painter.add(egui::Shape::line(points, egui::Stroke::new(1.8_f32, color)));
        }
    };
    if let Some(observed) = &hydrograph.observed {
        draw_series(observed, egui::Color32::from_rgb(75, 205, 245), &painter);
    }
    if let Some(forecast) = &hydrograph.forecast {
        draw_series(forecast, egui::Color32::from_rgb(255, 185, 65), &painter);
    }
    let primary_label = hydrograph
        .observed
        .as_ref()
        .or(hydrograph.forecast.as_ref())
        .map(|series| format!("{} ({})", series.primary_name, series.primary_unit))
        .unwrap_or_else(|| "Stage / flow".to_owned());
    painter.text(
        response.rect.left_top(),
        egui::Align2::LEFT_TOP,
        primary_label,
        egui::FontId::proportional(10.0),
        egui::Color32::from_gray(190),
    );
    painter.text(
        response.rect.left_bottom(),
        egui::Align2::LEFT_BOTTOM,
        time_zone.format_date_hm(min_time),
        egui::FontId::proportional(9.0),
        egui::Color32::from_gray(150),
    );
    painter.text(
        response.rect.right_bottom(),
        egui::Align2::RIGHT_BOTTOM,
        time_zone.format_date_hm(max_time),
        egui::FontId::proportional(9.0),
        egui::Color32::from_gray(150),
    );
    painter.text(
        response.rect.right_top(),
        egui::Align2::RIGHT_TOP,
        "observed / forecast",
        egui::FontId::proportional(9.0),
        egui::Color32::from_gray(170),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const SUMMARY_JSON: &str = r#"{
      "gauges": [
        {
          "lid": "TEST1", "name": "Test River", "latitude": 35.1, "longitude": -97.5,
          "wfo": {"abbreviation": "OUN"}, "state": {"abbreviation": "OK"},
          "status": {
            "observed": {"primary": 12.5, "primaryUnit": "ft", "secondary": 3.2,
              "secondaryUnit": "kcfs", "floodCategory": "minor", "validTime": "2026-07-20T20:30:00Z"},
            "forecast": {"primary": -999, "primaryUnit": "", "secondary": -999,
              "secondaryUnit": "", "floodCategory": "fcst_not_current", "validTime": "0001-01-01T00:00:00Z"}
          }
        },
        {"lid": "BAD", "name": "", "latitude": 999, "longitude": 0}
      ]
    }"#;

    #[test]
    fn parses_catalog_and_rejects_nwps_sentinels() {
        let gauges = parse_gauge_catalog(SUMMARY_JSON).expect("catalog");
        assert_eq!(gauges.len(), 1);
        let gauge = &gauges[0];
        assert_eq!(gauge.lid, "TEST1");
        assert_eq!(
            gauge
                .observed
                .as_ref()
                .unwrap()
                .primary
                .as_ref()
                .unwrap()
                .value,
            12.5
        );
        let forecast = gauge.forecast.as_ref().unwrap();
        assert!(forecast.primary.is_none());
        assert!(forecast.secondary.is_none());
        assert!(forecast.valid_time.is_none());
        assert_eq!(forecast.category, FloodCategory::Stale);
    }

    #[test]
    fn current_category_prefers_more_severe_forecast() {
        let now = Utc.with_ymd_and_hms(2026, 7, 20, 20, 45, 0).unwrap();
        let mut gauge = parse_gauge_catalog(SUMMARY_JSON).unwrap().remove(0);
        gauge.forecast = Some(GaugeReading {
            primary: Some(Quantity {
                value: 22.0,
                unit: "ft".to_owned(),
            }),
            secondary: None,
            category: FloodCategory::Moderate,
            valid_time: Some(now + chrono::Duration::hours(6)),
        });
        assert_eq!(gauge.display_category(now), (FloodCategory::Moderate, true));
        let late = now + chrono::Duration::days(20);
        assert_eq!(gauge.display_category(late).0, FloodCategory::Stale);
    }

    #[test]
    fn state_filter_hides_unselected_and_unknown_gauges() {
        let gauge = parse_gauge_catalog(SUMMARY_JSON).unwrap().remove(0);
        assert!(river_gauge_visible_for_states(&gauge, false, &[]));
        assert!(river_gauge_visible_for_states(
            &gauge,
            true,
            &["ok".to_owned()]
        ));
        assert!(!river_gauge_visible_for_states(
            &gauge,
            true,
            &["MI".to_owned()]
        ));
        assert!(!river_gauge_visible_for_states(&gauge, true, &[]));

        let mut unknown = gauge;
        unknown.state = None;
        assert!(!river_gauge_visible_for_states(
            &unknown,
            true,
            &["OK".to_owned()]
        ));
    }

    #[test]
    fn detail_and_hydrograph_preserve_thresholds_crest_and_attribution() {
        let detail = parse_gauge_detail(r#"{
          "lid":"TEST1","name":"Test River","latitude":35.1,"longitude":-97.5,
          "status":{"observed":{"primary":12.5,"primaryUnit":"ft","secondary":3.2,"secondaryUnit":"kcfs","floodCategory":"minor","validTime":"2026-07-20T20:30:00Z"}},
          "flood":{"stageUnits":"ft","categories":{"action":{"stage":10},"minor":{"stage":12},"moderate":{"stage":15},"major":{"stage":20}},"impacts":[{"stage":15,"statement":"Road floods."}]},
          "dataAttribution":[{"title":"USGS","text":"Observations courtesy of USGS","url":"https://waterdata.usgs.gov/"}]
        }"#).expect("detail");
        assert_eq!(detail.thresholds.minor, Some(12.0));
        assert_eq!(detail.impacts[0].statement, "Road floods.");
        assert_eq!(detail.attributions[0].title, "USGS");

        let hydro = parse_hydrograph(r#"{
          "observed":{"primaryName":"Stage","primaryUnits":"ft","secondaryName":"Flow","secondaryUnits":"kcfs","data":[
            {"validTime":"2026-07-20T18:00:00Z","primary":11.0,"secondary":2.0},
            {"validTime":"2026-07-20T20:00:00Z","primary":12.0,"secondary":3.0}]},
          "forecast":{"primaryName":"Stage","primaryUnits":"ft","secondaryName":"Flow","secondaryUnits":"kcfs","data":[
            {"validTime":"2026-07-21T00:00:00Z","primary":14.0,"secondary":4.0},
            {"validTime":"2026-07-21T06:00:00Z","primary":16.5,"secondary":5.0},
            {"validTime":"2026-07-21T12:00:00Z","primary":15.0,"secondary":4.5}]}
        }"#).expect("hydrograph");
        let (time, value, unit) = hydro.forecast_crest().unwrap();
        assert_eq!(time, Utc.with_ymd_and_hms(2026, 7, 21, 6, 0, 0).unwrap());
        assert_eq!(value, 16.5);
        assert_eq!(unit, "ft");
    }

    #[test]
    fn failed_tile_batch_does_not_advance_success_freshness() {
        let mut state = RiverGaugeState::default();
        let previous_success = Instant::now() - Duration::from_secs(300);
        state.last_success = Some(previous_success);

        state.apply_tile_batch(
            TileBatch {
                tiles: Vec::new(),
                errors: vec![(TileKey { x: 4, y: 3 }, "upstream unavailable".to_owned())],
            },
            Instant::now(),
        );

        assert_eq!(state.last_success, Some(previous_success));
        assert!(state.status.contains("unavailable"));
    }

    #[test]
    fn successful_empty_tile_response_advances_success_freshness() {
        let mut state = RiverGaugeState::default();
        let completed_at = Instant::now();

        state.apply_tile_batch(
            TileBatch {
                tiles: vec![(TileKey { x: 4, y: 3 }, Vec::new())],
                errors: Vec::new(),
            },
            completed_at,
        );

        assert_eq!(state.last_success, Some(completed_at));
    }

    #[test]
    fn viewport_tiles_are_us_bounded_and_dateline_aware() {
        let oklahoma = tile_keys_for_view(RiverViewport {
            west: -99.0,
            east: -96.0,
            south: 34.0,
            north: 37.0,
            center_lon: -97.5,
            approximate_lon_half_span: 2.0,
        });
        assert!(!oklahoma.is_empty());
        assert!(oklahoma.iter().all(|key| key.bounds().valid()));

        let alaska = tile_keys_for_view(RiverViewport {
            west: -179.0,
            east: 179.0,
            south: 50.0,
            north: 65.0,
            center_lon: 179.0,
            approximate_lon_half_span: 12.0,
        });
        assert!(!alaska.is_empty());
        assert!(
            alaska.len() < 8,
            "wrapped Alaska view should not request the whole US"
        );

        let europe = tile_keys_for_view(RiverViewport {
            west: 0.0,
            east: 20.0,
            south: 40.0,
            north: 55.0,
            center_lon: 10.0,
            approximate_lon_half_span: 10.0,
        });
        assert!(europe.is_empty());
    }

    #[test]
    fn declutter_keeps_flood_marker_when_normal_marker_shares_cell() {
        let make = |lid: &str, category: FloodCategory| RiverGaugeMarkerPoint {
            lid: lid.to_owned(),
            position: egui::pos2(100.0, 100.0),
            category,
            forecast_driven: false,
            name: lid.to_owned(),
            readout: None,
        };
        let points = declutter_markers(vec![
            make("FLOOD", FloodCategory::Major),
            make("NORMAL1", FloodCategory::NoFlooding),
            make("NORMAL2", FloodCategory::NoFlooding),
        ]);
        assert!(points.iter().any(|point| point.lid == "FLOOD"));
        assert_eq!(
            points
                .iter()
                .filter(|point| point.lid.starts_with("NORMAL"))
                .count(),
            1
        );
    }
}
