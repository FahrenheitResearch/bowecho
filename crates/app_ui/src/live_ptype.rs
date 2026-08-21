//! Live surface precipitation type: a model thermodynamic phase prior masked
//! by the current radar precipitation footprint.
//!
//! The expensive Modified Bourgouin classification is intentionally cached
//! independently from radar time.  A new radar scan only changes the cheap
//! occurrence mask and raster render; it never causes the model columns to be
//! decoded or classified again.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use chrono::{
    DateTime, Duration as ChronoDuration, NaiveDate, NaiveDateTime, TimeZone, Timelike, Utc,
};
use eframe::egui;
use radar_core::{MomentType, RadarVolume};
use rayon::prelude::*;
use render2d::{
    ColorTableFamily, ColorTableSet, ViewportMomentCache, ViewportRasterOptions,
    viewport_rgba_buffer_len,
};
use rustwx_core::{LatLonGrid, ModelId, SourceId};
use rustwx_products::gridded::{
    load_model_timestep_from_parts_cropped, load_surface_geometry_from_latest, resolve_model_run,
};
use rustwx_products::ptype::{
    CURRENT_PTYPE_ALGORITHM_VERSION, LivePtypeMetadata, PtypeAnalysisFrame, PtypeOptions, PtypeQc,
    SurfaceReplacementOptions, analyze_prepared_columns, prepare_hrrr_rap_columns,
    prepare_wrf_columns, replace_current_surface_from_analysis,
};
use rw_store::grid::GridFile;
use serde::{Deserialize, Serialize};

use crate::model_layer::{InverseLut, sample_stencils_for_point};
use crate::{ModelLayerView, map_layer_rerender_deferred, model_layer_view_needs_rerender};
use ui_core::geo::aeqd_inverse_km;

const DEFAULT_MIXED_THRESHOLD: f32 = 0.60;
const MIN_MIXED_THRESHOLD: f32 = 0.50;
const MAX_MIXED_THRESHOLD: f32 = 0.80;
const DEFAULT_OPACITY: f32 = 0.84;
const MAX_RASTER_AXIS: usize = 1_400;
const RENDER_SCALE: f32 = 0.72;
const RENDER_POLL: Duration = Duration::from_millis(80);
const MAX_OPERATIONAL_LOOKBACK_HOURS: u16 = 3;
const MAX_LOCAL_WRF_TARGET_MISMATCH_SECONDS: i64 = 90 * 60;
const RTMA_OPERATIONAL_WINDOW_HOURS: i64 = 72;

const COLOR_RAIN: [u8; 3] = [0x2f, 0x9e, 0x44];
const COLOR_SNOW: [u8; 3] = [0x4c, 0x8e, 0xd9];
const COLOR_FREEZING_RAIN: [u8; 3] = [0xd6, 0x33, 0x6c];
const COLOR_ICE_PELLETS: [u8; 3] = [0x9c, 0x5b, 0xd4];
const COLOR_MIXED: [u8; 3] = [0xb0, 0x82, 0x48];
const COLOR_UNKNOWN: [u8; 3] = [0x9a, 0xa0, 0xa8];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum LivePtypeModelSource {
    Auto,
    Hrrr,
    Rap,
    LocalWrf,
}

impl LivePtypeModelSource {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto (HRRR, then RAP)",
            Self::Hrrr => "HRRR",
            Self::Rap => "RAP",
            Self::LocalWrf => "Local WRF / ArWen",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum LivePtypeSurfaceMode {
    CurrentAnalysis,
    ModelOnly,
}

impl LivePtypeSurfaceMode {
    fn label(self) -> &'static str {
        match self {
            Self::CurrentAnalysis => "Current RTMA analysis",
            Self::ModelOnly => "Model surface",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum LivePtypeDisplayMode {
    Dominant,
    ProbabilityBlend,
    IceHazard,
    Diagnostics,
}

impl LivePtypeDisplayMode {
    fn label(self) -> &'static str {
        match self {
            Self::Dominant => "Dominant",
            Self::ProbabilityBlend => "Probability blend",
            Self::IceHazard => "Ice hazard",
            Self::Diagnostics => "Confidence / QC",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum LivePtypeOccurrenceMode {
    Radar,
    ModelOnlyDiagnostic,
}

impl LivePtypeOccurrenceMode {
    fn label(self) -> &'static str {
        match self {
            Self::Radar => "Radar footprint",
            Self::ModelOnlyDiagnostic => "Model prior (diagnostic)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct LivePtypeBounds {
    pub west: f64,
    pub east: f64,
    pub south: f64,
    pub north: f64,
}

impl LivePtypeBounds {
    pub(crate) fn new(west: f64, east: f64, south: f64, north: f64) -> Self {
        Self {
            west,
            east,
            south,
            north,
        }
        .sanitized()
    }

    fn sanitized(self) -> Self {
        let mut west = finite_or(self.west, -125.0).clamp(-180.0, 180.0);
        let mut east = finite_or(self.east, -66.0).clamp(-180.0, 180.0);
        let mut south = finite_or(self.south, 24.0).clamp(-90.0, 90.0);
        let mut north = finite_or(self.north, 50.0).clamp(-90.0, 90.0);
        if south > north {
            std::mem::swap(&mut south, &mut north);
        }
        if (east - west).abs() < 0.05 {
            west = (west - 0.5).max(-180.0);
            east = (east + 0.5).min(180.0);
        }
        if (north - south).abs() < 0.05 {
            south = (south - 0.5).max(-90.0);
            north = (north + 0.5).min(90.0);
        }
        Self {
            west,
            east,
            south,
            north,
        }
    }

    fn padded(self, degrees: f64) -> Self {
        let p = finite_or(degrees, 0.0).clamp(0.0, 10.0);
        Self::new(
            (self.west - p).max(-180.0),
            (self.east + p).min(180.0),
            (self.south - p).max(-90.0),
            (self.north + p).min(90.0),
        )
    }

    fn tuple(self) -> (f64, f64, f64, f64) {
        (self.west, self.east, self.south, self.north)
    }
}

impl Default for LivePtypeBounds {
    fn default() -> Self {
        Self::new(-125.0, -66.0, 24.0, 50.0)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct LivePtypeRadarOccurrence {
    pub size: [usize; 2],
    /// Row-major alpha/confidence values. Zero means no observed echo.
    pub alpha: Arc<[u8]>,
    pub source: String,
    pub scan_time: DateTime<Utc>,
}

/// Immutable radar inputs handed to the precipitation-type render worker.
///
/// The UI thread only clones the volume `Arc` and copies viewport metadata.
/// Polar sampling and the permissive -10 dBZ occurrence mask are both built
/// in the render worker, so enabling this layer cannot stall map interaction.
#[derive(Debug, Clone)]
pub(crate) struct LivePtypeRadarSourceSnapshot {
    pub volume: Arc<RadarVolume>,
    pub reflectivity_cut: usize,
    pub viewport: ViewportRasterOptions,
    pub source: String,
    pub scan_time: DateTime<Utc>,
    pub generation: u64,
}

impl LivePtypeRadarSourceSnapshot {
    pub(crate) fn new(
        volume: Arc<RadarVolume>,
        reflectivity_cut: usize,
        viewport: ViewportRasterOptions,
        source: impl Into<String>,
        generation: u64,
    ) -> Self {
        let scan_time = volume.volume_time;
        Self {
            volume,
            reflectivity_cut,
            viewport,
            source: source.into(),
            scan_time,
            generation,
        }
    }
}

impl LivePtypeRadarOccurrence {
    pub(crate) fn from_rgba(
        size: [usize; 2],
        rgba: &[u8],
        source: impl Into<String>,
        scan_time: DateTime<Utc>,
    ) -> Option<Self> {
        let cells = size[0].checked_mul(size[1])?;
        if rgba.len() != cells.checked_mul(4)? {
            return None;
        }
        let alpha = rgba
            .as_chunks::<4>()
            .0
            .iter()
            .map(|pixel| pixel[3])
            .collect();
        Some(Self {
            size,
            alpha,
            source: source.into(),
            scan_time,
        })
    }

    #[cfg(test)]
    fn uniform(size: [usize; 2], alpha: u8) -> Self {
        Self {
            size,
            alpha: vec![alpha; size[0] * size[1]].into(),
            source: "test radar".to_owned(),
            scan_time: Utc::now(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LivePtypeCategory {
    // Kept because zero is part of the signed/public ptype-code contract even
    // though the current map path represents no precipitation as transparency.
    #[allow(dead_code)]
    NoPrecip,
    Rain,
    Snow,
    FreezingRain,
    IcePellets,
    Mixed,
    Unknown,
}

impl LivePtypeCategory {
    // The app consumes categories directly; this conversion guards parity
    // with the public Rusty Weather product code in regression tests.
    #[allow(dead_code)]
    pub(crate) fn code(self) -> u8 {
        match self {
            Self::NoPrecip => 0,
            Self::Rain => 1,
            Self::Snow => 2,
            Self::FreezingRain => 3,
            Self::IcePellets => 4,
            Self::Mixed => 5,
            Self::Unknown => u8::MAX,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::NoPrecip => "No precipitation",
            Self::Rain => "Rain",
            Self::Snow => "Snow",
            Self::FreezingRain => "Freezing rain",
            Self::IcePellets => "Ice pellets",
            Self::Mixed => "Mixed / uncertain",
            Self::Unknown => "Unknown",
        }
    }

    fn color(self) -> [u8; 3] {
        match self {
            Self::NoPrecip => [0, 0, 0],
            Self::Rain => COLOR_RAIN,
            Self::Snow => COLOR_SNOW,
            Self::FreezingRain => COLOR_FREEZING_RAIN,
            Self::IcePellets => COLOR_ICE_PELLETS,
            Self::Mixed => COLOR_MIXED,
            Self::Unknown => COLOR_UNKNOWN,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct LivePtypeSample {
    pub category: LivePtypeCategory,
    /// Rain, snow, freezing-rain, and ice-pellet independent PoWT scores.
    pub scores_pct: [f32; 4],
    /// The same fields normalized only after geographic interpolation.
    pub fractions: [f32; 4],
    pub confidence: f32,
    pub qc_bits: u16,
}

impl LivePtypeSample {
    pub(crate) fn inspector_lines(&self) -> [String; 4] {
        [
            format!(
                "ptype prior {} · confidence {:.0}%",
                self.category.label(),
                self.confidence * 100.0
            ),
            format!(
                "PoWT rain {:.0}  snow {:.0}  FZRA {:.0}  PL {:.0}",
                self.scores_pct[0], self.scores_pct[1], self.scores_pct[2], self.scores_pct[3]
            ),
            format!(
                "fraction rain {:.0}%  snow {:.0}%  FZRA {:.0}%  PL {:.0}%",
                self.fractions[0] * 100.0,
                self.fractions[1] * 100.0,
                self.fractions[2] * 100.0,
                self.fractions[3] * 100.0,
            ),
            ptype_qc_label(self.qc_bits),
        ]
    }
}

#[derive(Debug, Clone)]
pub(crate) struct LivePtypeProvenance {
    pub algorithm_version: u16,
    pub model_id: String,
    pub model_cycle: DateTime<Utc>,
    pub model_valid: DateTime<Utc>,
    pub model_horizontal_resolution_m: f32,
    pub surface_analysis_id: String,
    pub surface_analysis_valid: DateTime<Utc>,
    pub surface_replaced_cells: Option<usize>,
    pub source_detail: String,
    pub local_time_index: Option<usize>,
}

impl LivePtypeProvenance {
    pub(crate) fn truth_label(&self) -> String {
        let resolution = if self.model_horizontal_resolution_m >= 1_000.0 {
            format!("{:.1} km", self.model_horizontal_resolution_m / 1_000.0)
        } else {
            format!("{:.0} m", self.model_horizontal_resolution_m)
        };
        format!(
            "radar-footprint resolution; phase prior from {} at {resolution}",
            self.model_id.to_ascii_uppercase()
        )
    }

    fn hover_text(&self, radar: Option<&LivePtypeRadarMetadata>, now: DateTime<Utc>) -> String {
        let model_age = signed_age_label(now, self.model_valid);
        let surface_age = signed_age_label(now, self.surface_analysis_valid);
        let radar_line = radar.map_or_else(
            || "Radar occurrence: waiting for a precipitation mask".to_owned(),
            |radar| {
                format!(
                    "Radar: {} {} (age {})",
                    radar.source,
                    radar.scan_time.format("%Y-%m-%d %H:%M:%SZ"),
                    signed_age_label(now, radar.scan_time)
                )
            },
        );
        let surface_detail = self.surface_replaced_cells.map_or_else(
            || "Surface replacement: model-native surface retained".to_owned(),
            |cells| format!("Surface replacement: {cells} model-grid cells"),
        );
        let local_time_detail = self
            .local_time_index
            .map(|index| format!("\nLocal WRF time index: {index}"))
            .unwrap_or_default();
        format!(
            "{}\nAlgorithm: Modified Bourgouin v{}\nModel: {} cycle {} valid {} (age {})\nSurface: {} {} (age {})\n{}\n{}\n{}{}",
            self.truth_label(),
            self.algorithm_version,
            self.model_id.to_ascii_uppercase(),
            self.model_cycle.format("%Y-%m-%d %H:%MZ"),
            self.model_valid.format("%Y-%m-%d %H:%MZ"),
            model_age,
            self.surface_analysis_id,
            self.surface_analysis_valid.format("%Y-%m-%d %H:%MZ"),
            surface_age,
            radar_line,
            surface_detail,
            self.source_detail,
            local_time_detail,
        )
    }
}

struct LivePtypeFrame {
    analysis: PtypeAnalysisFrame,
    grid: Arc<GridFile>,
    lut: Arc<InverseLut>,
    provenance: LivePtypeProvenance,
    request_key: LivePtypeRequestKey,
    local_target_window_unix: Option<(i64, i64)>,
    target_valid: DateTime<Utc>,
    generation: u64,
}

impl LivePtypeFrame {
    fn new(
        analysis: PtypeAnalysisFrame,
        lat_lon: LatLonGrid,
        provenance: LivePtypeProvenance,
        request_key: LivePtypeRequestKey,
        local_target_window_unix: Option<(i64, i64)>,
        target_valid: DateTime<Utc>,
        generation: u64,
    ) -> Result<Self, String> {
        let nx = analysis.grid.nx;
        let ny = analysis.grid.ny;
        let cells = nx
            .checked_mul(ny)
            .ok_or_else(|| "precipitation-type grid dimensions overflow".to_owned())?;
        if lat_lon.shape.nx != nx
            || lat_lon.shape.ny != ny
            || lat_lon.lat_deg.len() != cells
            || lat_lon.lon_deg.len() != cells
        {
            return Err("precipitation-type grid/geolocation dimensions disagree".to_owned());
        }
        for (name, len) in [
            ("rain", analysis.rain_powt_pct.len()),
            ("snow", analysis.snow_powt_pct.len()),
            ("freezing rain", analysis.freezing_rain_powt_pct.len()),
            ("ice pellets", analysis.ice_pellets_powt_pct.len()),
            ("QC", analysis.qc_bits.len()),
        ] {
            if len != cells {
                return Err(format!(
                    "precipitation-type {name} field has {len} cells; expected {cells}"
                ));
            }
        }
        let lut =
            InverseLut::build_with_shape_domain_bounded(&lat_lon.lat_deg, &lat_lon.lon_deg, nx, ny)
                .ok_or_else(|| "precipitation-type grid has no usable geolocation".to_owned())?;
        let grid = GridFile {
            nx,
            ny,
            lat: lat_lon.lat_deg,
            lon: lat_lon.lon_deg,
            projection: None,
            hash: format!(
                "live-ptype:{}:{}:{}",
                provenance.model_id,
                provenance.model_cycle.timestamp(),
                provenance.model_valid.timestamp()
            ),
        };
        Ok(Self {
            analysis,
            grid: Arc::new(grid),
            lut: Arc::new(lut),
            provenance,
            request_key,
            local_target_window_unix,
            target_valid,
            generation,
        })
    }

    fn matches_request(&self, wanted: &LivePtypeRequestKey) -> bool {
        if self.request_key.source != wanted.source
            || self.request_key.surface_mode != wanted.surface_mode
            || self.request_key.local_wrf_path != wanted.local_wrf_path
        {
            return false;
        }
        if wanted.source != LivePtypeModelSource::LocalWrf {
            return self.request_key.target == wanted.target;
        }
        self.local_target_window_unix.is_some_and(|(first, last)| {
            let target = wanted.target.timestamp();
            (first..=last).contains(&target)
        })
    }

    fn sample(&self, lat: f32, lon: f32, threshold: f32) -> Option<LivePtypeSample> {
        let nearest = self.lut.lookup(lat, lon)?;
        let scores = sample_four_fields(self, nearest, lat, lon)?;
        let fractions = normalize_scores(scores)?;
        let (winner, confidence) = winner_and_confidence(fractions);
        let category = if confidence < threshold {
            LivePtypeCategory::Mixed
        } else {
            category_for_index(winner)
        };
        Some(LivePtypeSample {
            category,
            scores_pct: scores,
            fractions,
            confidence,
            qc_bits: self.analysis.qc_bits.get(nearest).copied().unwrap_or(0),
        })
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct LivePtypeRequestKey {
    source: LivePtypeModelSource,
    surface_mode: LivePtypeSurfaceMode,
    target: DateTime<Utc>,
    local_wrf_path: Option<PathBuf>,
}

fn live_ptype_request_key(
    source: LivePtypeModelSource,
    surface_mode: LivePtypeSurfaceMode,
    local_wrf_path: Option<&Path>,
    target_valid: DateTime<Utc>,
) -> LivePtypeRequestKey {
    LivePtypeRequestKey {
        source,
        surface_mode,
        // Operational HRRR/RAP fields are hourly. Local WRF/ArWen files may
        // carry one-minute (or finer) history intervals, so their displayed
        // radar timestamp must remain exact instead of collapsing to the hour.
        target: if source == LivePtypeModelSource::LocalWrf {
            target_valid
        } else {
            target_analysis_hour(target_valid)
        },
        local_wrf_path: if source == LivePtypeModelSource::LocalWrf {
            local_wrf_path.map(Path::to_path_buf)
        } else {
            None
        },
    }
}

#[derive(Debug, Clone)]
struct LivePtypeFetchRequest {
    source: LivePtypeModelSource,
    surface_mode: LivePtypeSurfaceMode,
    target_valid: DateTime<Utc>,
    bounds: LivePtypeBounds,
    cache_root: PathBuf,
    local_wrf_path: Option<PathBuf>,
    generation: u64,
}

impl LivePtypeFetchRequest {
    fn key(&self) -> LivePtypeRequestKey {
        live_ptype_request_key(
            self.source,
            self.surface_mode,
            self.local_wrf_path.as_deref(),
            self.target_valid,
        )
    }
}

struct LivePtypeFetchResult {
    generation: u64,
    request_key: LivePtypeRequestKey,
    frame: Result<LivePtypeFrame, String>,
}

#[derive(Debug, Clone, PartialEq)]
struct LivePtypeRenderKey {
    frame_generation: u64,
    thermo_request_key: LivePtypeRequestKey,
    reference_time_millis: i64,
    view: ModelLayerView,
    size: [usize; 2],
    viewport_size_points: [u32; 2],
    display_mode: LivePtypeDisplayMode,
    occurrence_mode: LivePtypeOccurrenceMode,
    threshold_milli: u16,
    occurrence_generation: Option<u64>,
}

#[derive(Debug)]
struct LivePtypeRenderResult {
    serial: u64,
    key: LivePtypeRenderKey,
    image: egui::ColorImage,
    render_ms: f32,
    radar: Option<LivePtypeRadarMetadata>,
    radar_error: Option<String>,
}

#[derive(Debug, Clone)]
struct LivePtypeRadarMetadata {
    source: String,
    scan_time: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct PersistedLivePtypeSettings {
    enabled: bool,
    visible: bool,
    opacity: f32,
    source: LivePtypeModelSource,
    surface_mode: LivePtypeSurfaceMode,
    display_mode: LivePtypeDisplayMode,
    occurrence_mode: LivePtypeOccurrenceMode,
    mixed_threshold: f32,
    show_uncertainty_qc: bool,
}

impl Default for PersistedLivePtypeSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            visible: true,
            opacity: DEFAULT_OPACITY,
            source: LivePtypeModelSource::Auto,
            surface_mode: LivePtypeSurfaceMode::CurrentAnalysis,
            display_mode: LivePtypeDisplayMode::Dominant,
            occurrence_mode: LivePtypeOccurrenceMode::Radar,
            mixed_threshold: DEFAULT_MIXED_THRESHOLD,
            show_uncertainty_qc: false,
        }
    }
}

pub(crate) struct LivePtypeState {
    pub enabled: bool,
    pub visible: bool,
    pub window_open: bool,
    pub opacity: f32,
    pub source: LivePtypeModelSource,
    pub surface_mode: LivePtypeSurfaceMode,
    pub display_mode: LivePtypeDisplayMode,
    pub occurrence_mode: LivePtypeOccurrenceMode,
    pub mixed_threshold: f32,
    pub show_uncertainty_qc: bool,
    pub local_wrf_path: Option<PathBuf>,

    status: String,
    error: Option<String>,
    frame: Option<Arc<LivePtypeFrame>>,
    fetch_rx: Option<Receiver<LivePtypeFetchResult>>,
    render_rx: Option<Receiver<LivePtypeRenderResult>>,
    texture: Option<(egui::TextureHandle, LivePtypeRenderKey)>,
    generation: u64,
    render_serial: u64,
    active_render_serial: Option<u64>,
    last_render_ms: Option<f32>,
    last_radar: Option<LivePtypeRadarMetadata>,
    radar_error: Option<String>,
    persisted_dirty: bool,
    refresh_attempted: bool,
    desired_target_valid: Option<DateTime<Utc>>,
    last_attempted_request_key: Option<LivePtypeRequestKey>,
}

impl Default for LivePtypeState {
    fn default() -> Self {
        Self {
            enabled: false,
            visible: true,
            window_open: false,
            opacity: DEFAULT_OPACITY,
            source: LivePtypeModelSource::Auto,
            surface_mode: LivePtypeSurfaceMode::CurrentAnalysis,
            display_mode: LivePtypeDisplayMode::Dominant,
            occurrence_mode: LivePtypeOccurrenceMode::Radar,
            mixed_threshold: DEFAULT_MIXED_THRESHOLD,
            show_uncertainty_qc: false,
            local_wrf_path: None,
            status: "Not loaded".to_owned(),
            error: None,
            frame: None,
            fetch_rx: None,
            render_rx: None,
            texture: None,
            generation: 0,
            render_serial: 0,
            active_render_serial: None,
            last_render_ms: None,
            last_radar: None,
            radar_error: None,
            persisted_dirty: false,
            refresh_attempted: false,
            desired_target_valid: None,
            last_attempted_request_key: None,
        }
    }
}

impl LivePtypeState {
    pub(crate) fn from_persisted(value: Option<&serde_json::Value>) -> Self {
        let settings = value
            .and_then(|value| {
                serde_json::from_value::<PersistedLivePtypeSettings>(value.clone()).ok()
            })
            .unwrap_or_default();
        let mut state = Self::default();
        state.apply_persisted(settings);
        state
    }

    pub(crate) fn persisted_value(&self) -> serde_json::Value {
        serde_json::to_value(self.persisted_snapshot()).unwrap_or_default()
    }

    pub(crate) fn take_persisted_if_dirty(&mut self) -> Option<serde_json::Value> {
        if !std::mem::take(&mut self.persisted_dirty) {
            return None;
        }
        Some(self.persisted_value())
    }

    pub(crate) fn set_visible(&mut self, visible: bool) {
        if self.visible != visible {
            self.visible = visible;
            self.persisted_dirty = true;
        }
    }

    pub(crate) fn set_opacity(&mut self, opacity: f32) {
        let opacity = opacity.clamp(0.05, 1.0);
        if (self.opacity - opacity).abs() > f32::EPSILON {
            self.opacity = opacity;
            self.persisted_dirty = true;
        }
    }

    pub(crate) fn enable(&mut self) {
        let was_enabled = self.enabled;
        let changed = !self.enabled || !self.visible || !self.window_open;
        self.enabled = true;
        self.visible = true;
        self.window_open = true;
        self.persisted_dirty |= changed;
        if !was_enabled && self.frame.is_none() {
            self.refresh_attempted = false;
        }
    }

    pub(crate) fn configure_winter_archive_scene(&mut self) {
        self.enabled = true;
        self.visible = true;
        self.window_open = true;
        self.source = LivePtypeModelSource::Hrrr;
        // Historical RTMA is not available from the operational NOMADS-only
        // adapter.  A time-matched HRRR surface is preferable to silently
        // mixing an archive radar scan with today's analysis.
        self.surface_mode = LivePtypeSurfaceMode::ModelOnly;
        self.display_mode = LivePtypeDisplayMode::Dominant;
        self.occurrence_mode = LivePtypeOccurrenceMode::Radar;
        self.mixed_threshold = DEFAULT_MIXED_THRESHOLD;
        self.persisted_dirty = true;
    }

    pub(crate) fn open(&mut self) {
        let changed = !self.enabled;
        self.enabled = true;
        self.window_open = true;
        self.persisted_dirty |= changed;
    }

    pub(crate) fn remove(&mut self) {
        self.enabled = false;
        self.visible = false;
        self.window_open = false;
        self.frame = None;
        self.fetch_rx = None;
        self.render_rx = None;
        self.texture = None;
        self.active_render_serial = None;
        self.last_radar = None;
        self.radar_error = None;
        self.generation = self.generation.wrapping_add(1);
        self.status = "Removed".to_owned();
        self.error = None;
        self.persisted_dirty = true;
        self.refresh_attempted = false;
        self.desired_target_valid = None;
        self.last_attempted_request_key = None;
    }

    pub(crate) fn is_busy(&self) -> bool {
        self.fetch_rx.is_some() || self.render_rx.is_some()
    }

    /// One-shot startup trigger for a persisted enabled layer. Once any load
    /// has been attempted (success or failure), only the explicit Refresh
    /// control retries it; a source outage can therefore never cause a
    /// per-frame fetch loop.
    pub(crate) fn needs_initial_refresh(&self) -> bool {
        self.enabled && self.frame.is_none() && self.fetch_rx.is_none() && !self.refresh_attempted
    }

    pub(crate) fn has_frame(&self) -> bool {
        self.frame.is_some()
    }

    pub(crate) fn status_label(&self) -> String {
        if !self.enabled {
            return "Off".to_owned();
        }
        if self.fetch_rx.is_some() {
            return "Loading thermodynamic prior…".to_owned();
        }
        if self.render_rx.is_some() {
            return "Rendering…".to_owned();
        }
        if let Some(error) = &self.error {
            if self.frame.is_some() {
                return format!("Stale prior · {error}");
            }
            return format!("Error · {error}");
        }
        self.status.clone()
    }

    pub(crate) fn hover_text(&self) -> String {
        let mut text = self.status_label();
        if let Some(frame) = &self.frame {
            let reference_time = self
                .last_radar
                .as_ref()
                .map(|radar| radar.scan_time)
                .or(self.desired_target_valid)
                .unwrap_or(frame.target_valid);
            text.push('\n');
            text.push_str(
                &frame
                    .provenance
                    .hover_text(self.last_radar.as_ref(), reference_time),
            );
            text.push_str("\nAges are relative to the displayed radar frame.");
        }
        if let Some(error) = &self.radar_error {
            text.push_str("\nRadar occurrence unavailable: ");
            text.push_str(error);
        }
        text.push_str(
            "\nRadar determines where precipitation exists; the model column determines phase.",
        );
        text.push_str("\nKnown limit: freezing drizzle cannot be separated from freezing rain.");
        text
    }

    fn persisted_snapshot(&self) -> PersistedLivePtypeSettings {
        PersistedLivePtypeSettings {
            enabled: self.enabled,
            visible: self.visible,
            opacity: self.opacity.clamp(0.05, 1.0),
            source: self.source,
            surface_mode: self.surface_mode,
            display_mode: self.display_mode,
            occurrence_mode: self.occurrence_mode,
            mixed_threshold: self
                .mixed_threshold
                .clamp(MIN_MIXED_THRESHOLD, MAX_MIXED_THRESHOLD),
            show_uncertainty_qc: self.show_uncertainty_qc,
        }
    }

    fn apply_persisted(&mut self, settings: PersistedLivePtypeSettings) {
        self.enabled = settings.enabled;
        self.visible = settings.visible;
        self.opacity = settings.opacity.clamp(0.05, 1.0);
        self.source = settings.source;
        self.surface_mode = settings.surface_mode;
        self.display_mode = settings.display_mode;
        self.occurrence_mode = settings.occurrence_mode;
        self.mixed_threshold = settings
            .mixed_threshold
            .clamp(MIN_MIXED_THRESHOLD, MAX_MIXED_THRESHOLD);
        self.show_uncertainty_qc = settings.show_uncertainty_qc;
        // Local source paths are deliberately session-only and never written
        // into AppSettings or diagnostics.
        self.local_wrf_path = None;
        self.persisted_dirty = false;
    }

    fn request_key(&self, target_valid: DateTime<Utc>) -> LivePtypeRequestKey {
        live_ptype_request_key(
            self.source,
            self.surface_mode,
            self.local_wrf_path.as_deref(),
            target_valid,
        )
    }

    /// Keep a no-radar local-WRF request stable while its worker is running.
    /// Otherwise a per-frame `Utc::now()` target would invalidate the result
    /// before it could ever install.
    pub(crate) fn fallback_target_valid(&self) -> Option<DateTime<Utc>> {
        self.desired_target_valid
            .or_else(|| self.frame.as_ref().map(|frame| frame.target_valid))
    }

    pub(crate) fn sample(&self, lat: f64, lon: f64) -> Option<LivePtypeSample> {
        if !self.enabled || !self.visible || !lat.is_finite() || !lon.is_finite() {
            return None;
        }
        let frame = self.frame.as_ref()?;
        if self
            .desired_target_valid
            .is_some_and(|target| !frame.matches_request(&self.request_key(target)))
        {
            return None;
        }
        frame.sample(
            lat as f32,
            lon as f32,
            self.mixed_threshold
                .clamp(MIN_MIXED_THRESHOLD, MAX_MIXED_THRESHOLD),
        )
    }

    pub(crate) fn provenance(&self) -> Option<&LivePtypeProvenance> {
        let frame = self.frame.as_ref()?;
        if self
            .desired_target_valid
            .is_some_and(|target| !frame.matches_request(&self.request_key(target)))
        {
            return None;
        }
        Some(&frame.provenance)
    }

    pub(crate) fn request_refresh(
        &mut self,
        ctx: &egui::Context,
        cache_root: &Path,
        bounds: LivePtypeBounds,
        target_valid: DateTime<Utc>,
    ) {
        self.desired_target_valid = Some(target_valid);
        if self.fetch_rx.is_some() {
            return;
        }
        self.start_refresh(ctx, cache_root, bounds, target_valid);
    }

    /// Keep the thermodynamic prior aligned with the displayed radar time.
    /// Operational HRRR/RAP data are keyed hourly; local WRF/ArWen data keep
    /// exact timestamps so subhourly history intervals advance correctly. A
    /// failed request is attempted once until the user explicitly refreshes
    /// or selects another target/configuration.
    pub(crate) fn ensure_target(
        &mut self,
        ctx: &egui::Context,
        cache_root: &Path,
        bounds: LivePtypeBounds,
        target_valid: DateTime<Utc>,
    ) {
        if !self.enabled || !self.visible {
            return;
        }
        self.desired_target_valid = Some(target_valid);
        let request_key = self.request_key(target_valid);
        if self.fetch_rx.is_some()
            || self
                .frame
                .as_ref()
                .is_some_and(|frame| frame.matches_request(&request_key))
            || self.last_attempted_request_key.as_ref() == Some(&request_key)
        {
            return;
        }
        self.start_refresh(ctx, cache_root, bounds, target_valid);
    }

    fn start_refresh(
        &mut self,
        ctx: &egui::Context,
        cache_root: &Path,
        bounds: LivePtypeBounds,
        target_valid: DateTime<Utc>,
    ) {
        self.refresh_attempted = true;
        let request_key = self.request_key(target_valid);
        if self.source == LivePtypeModelSource::LocalWrf && self.local_wrf_path.is_none() {
            self.error = Some("Choose a wrfout file first".to_owned());
            self.status = "Local WRF file required".to_owned();
            self.last_attempted_request_key = Some(request_key);
            return;
        }
        if !self.enabled || !self.visible {
            self.persisted_dirty = true;
        }
        self.enabled = true;
        self.visible = true;
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.last_attempted_request_key = Some(request_key.clone());
        let request = LivePtypeFetchRequest {
            source: self.source,
            surface_mode: self.surface_mode,
            target_valid,
            bounds: bounds.padded(1.5),
            cache_root: cache_root.join("live-ptype"),
            local_wrf_path: self.local_wrf_path.clone(),
            generation,
        };
        let (sender, receiver) = mpsc::sync_channel(1);
        self.fetch_rx = Some(receiver);
        self.status = match self.source {
            LivePtypeModelSource::LocalWrf => "Reading and classifying local WRF columns…",
            _ => "Fetching and classifying model columns…",
        }
        .to_owned();
        self.error = None;
        let repaint = ctx.clone();
        let spawn = thread::Builder::new()
            .name("live-ptype-fetch".to_owned())
            .spawn(move || {
                let frame = std::panic::catch_unwind(|| load_frame(request.clone()))
                    .unwrap_or_else(|_| Err("precipitation-type worker panicked".to_owned()));
                let _ = sender.send(LivePtypeFetchResult {
                    generation,
                    request_key,
                    frame,
                });
                repaint.request_repaint();
            });
        if let Err(error) = spawn {
            self.fetch_rx = None;
            self.error = Some(format!(
                "could not start precipitation-type worker: {error}"
            ));
        } else {
            ctx.request_repaint_after(RENDER_POLL);
        }
    }

    pub(crate) fn poll(&mut self, ctx: &egui::Context) {
        if let Some(receiver) = &self.fetch_rx {
            match receiver.try_recv() {
                Ok(result) => {
                    self.fetch_rx = None;
                    let desired_key = self
                        .desired_target_valid
                        .map(|target| self.request_key(target));
                    if result.generation == self.generation && self.enabled {
                        match result.frame {
                            Ok(frame)
                                if desired_key
                                    .as_ref()
                                    .is_some_and(|wanted| frame.matches_request(wanted)) =>
                            {
                                let cells = frame.analysis.grid.nx * frame.analysis.grid.ny;
                                self.status = format!(
                                    "Ready · {} · {cells} columns",
                                    frame.provenance.model_id.to_ascii_uppercase()
                                );
                                self.frame = Some(Arc::new(frame));
                                self.error = None;
                                self.texture = None;
                                self.render_rx = None;
                                self.active_render_serial = None;
                                self.last_radar = None;
                                self.radar_error = None;
                            }
                            Err(error) if desired_key.as_ref() == Some(&result.request_key) => {
                                // Keep the last valid prior, but make its stale state explicit.
                                self.error = Some(error);
                            }
                            Ok(_) | Err(_) => {
                                self.status = "Waiting for the selected radar time…".to_owned();
                            }
                        }
                    } else if self.enabled {
                        self.status = "Waiting for the selected radar time…".to_owned();
                    }
                    ctx.request_repaint();
                }
                Err(TryRecvError::Empty) => ctx.request_repaint_after(RENDER_POLL),
                Err(TryRecvError::Disconnected) => {
                    self.fetch_rx = None;
                    self.error = Some("precipitation-type worker disconnected".to_owned());
                }
            }
        }

        if let Some(receiver) = &self.render_rx {
            match receiver.try_recv() {
                Ok(result) => {
                    self.render_rx = None;
                    let current = self.active_render_serial.take();
                    let desired_key = self
                        .desired_target_valid
                        .map(|target| self.request_key(target));
                    if current == Some(result.serial)
                        && self.frame.as_ref().is_some_and(|frame| {
                            frame.generation == result.key.frame_generation
                                && frame.request_key == result.key.thermo_request_key
                                && desired_key
                                    .as_ref()
                                    .is_none_or(|wanted| frame.matches_request(wanted))
                        })
                    {
                        let texture = ctx.load_texture(
                            "live-precipitation-type",
                            result.image,
                            egui::TextureOptions::LINEAR,
                        );
                        self.texture = Some((texture, result.key));
                        self.last_render_ms = Some(result.render_ms);
                        self.last_radar = result.radar;
                        self.radar_error = result.radar_error;
                    }
                    ctx.request_repaint();
                }
                Err(TryRecvError::Empty) => ctx.request_repaint_after(RENDER_POLL),
                Err(TryRecvError::Disconnected) => {
                    self.render_rx = None;
                    self.active_render_serial = None;
                    self.error = Some("precipitation-type render worker disconnected".to_owned());
                }
            }
        }
    }

    pub(crate) fn request_render(
        &mut self,
        ctx: &egui::Context,
        rect: egui::Rect,
        view: ModelLayerView,
        radar_source: Option<LivePtypeRadarSourceSnapshot>,
    ) {
        if !self.enabled
            || !self.visible
            || self.frame.is_none()
            || rect.width() < 2.0
            || rect.height() < 2.0
        {
            return;
        }
        if self.occurrence_mode == LivePtypeOccurrenceMode::Radar && radar_source.is_none() {
            self.texture = None;
            self.render_rx = None;
            self.active_render_serial = None;
            self.last_radar = None;
            self.radar_error = None;
            return;
        }
        let frame = Arc::clone(self.frame.as_ref().expect("checked frame"));
        let reference_time = radar_source
            .as_ref()
            .map(|source| source.scan_time)
            .or(self.desired_target_valid)
            .unwrap_or(frame.target_valid);
        let wanted_request_key = self.request_key(reference_time);
        if !frame.matches_request(&wanted_request_key) {
            self.texture = None;
            self.render_rx = None;
            self.active_render_serial = None;
            return;
        }
        let size = render_size(rect);
        // A real radar/archive scan keeps its exact timestamp in the render
        // key for age/QC. With no radar, the accepted thermo request already
        // supplies a stable operational hour or local-WRF target.
        let reference_key_time = if radar_source.is_some() {
            reference_time
        } else {
            wanted_request_key.target
        };
        let key = LivePtypeRenderKey {
            frame_generation: frame.generation,
            thermo_request_key: frame.request_key.clone(),
            reference_time_millis: reference_key_time.timestamp_millis(),
            view,
            size,
            viewport_size_points: [
                rect.width().round().max(1.0) as u32,
                rect.height().round().max(1.0) as u32,
            ],
            display_mode: self.display_mode,
            occurrence_mode: self.occurrence_mode,
            threshold_milli: (self
                .mixed_threshold
                .clamp(MIN_MIXED_THRESHOLD, MAX_MIXED_THRESHOLD)
                * 1_000.0)
                .round() as u16,
            occurrence_generation: occurrence_render_generation(
                self.occurrence_mode,
                radar_source.as_ref().map(|source| source.generation),
            ),
        };
        if self
            .texture
            .as_ref()
            .is_some_and(|(_, have)| render_key_matches(have, &key))
            || self.render_rx.is_some()
            || map_layer_rerender_deferred(ctx)
        {
            return;
        }

        self.render_serial = self.render_serial.wrapping_add(1);
        let serial = self.render_serial;
        self.active_render_serial = Some(serial);
        let (sender, receiver) = mpsc::sync_channel(1);
        self.render_rx = Some(receiver);
        let repaint = ctx.clone();
        let spawn = thread::Builder::new()
            .name("live-ptype-render".to_owned())
            .spawn(move || {
                let started = Instant::now();
                let (occurrence, radar, radar_error) =
                    match (key.occurrence_mode, radar_source.as_ref()) {
                        (LivePtypeOccurrenceMode::ModelOnlyDiagnostic, _) => (None, None, None),
                        (LivePtypeOccurrenceMode::Radar, Some(source)) => {
                            match build_radar_occurrence(source, size) {
                                Ok(mask) => {
                                    let radar = LivePtypeRadarMetadata {
                                        source: mask.source.clone(),
                                        scan_time: mask.scan_time,
                                    };
                                    (Some(mask), Some(radar), None)
                                }
                                Err(error) => (None, None, Some(error)),
                            }
                        }
                        (LivePtypeOccurrenceMode::Radar, None) => (None, None, None),
                    };
                let image = render_image(&frame, &key, occurrence.as_ref(), reference_time);
                let _ = sender.send(LivePtypeRenderResult {
                    serial,
                    key,
                    image,
                    render_ms: started.elapsed().as_secs_f32() * 1_000.0,
                    radar,
                    radar_error,
                });
                repaint.request_repaint();
            });
        if let Err(error) = spawn {
            self.render_rx = None;
            self.active_render_serial = None;
            self.error = Some(format!(
                "could not start precipitation-type renderer: {error}"
            ));
        } else {
            ctx.request_repaint_after(RENDER_POLL);
        }
    }

    pub(crate) fn paint(&self, painter: &egui::Painter, rect: egui::Rect, view: ModelLayerView) {
        if !self.enabled || !self.visible {
            return;
        }
        let Some((texture, key)) = &self.texture else {
            return;
        };
        if model_layer_view_needs_rerender(&key.view, &view) {
            return;
        }
        painter.image(
            texture.id(),
            rect,
            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
            egui::Color32::from_white_alpha((self.opacity.clamp(0.0, 1.0) * 255.0).round() as u8),
        );
    }

    pub(crate) fn show_window(
        &mut self,
        ctx: &egui::Context,
        cache_root: &Path,
        view_bounds: LivePtypeBounds,
        target_valid: DateTime<Utc>,
    ) {
        if !self.window_open {
            return;
        }
        let persisted_before = self.persisted_snapshot();
        let mut open = self.window_open;
        let mut refresh = false;
        let mut invalidate_render = false;
        egui::Window::new("Live precipitation type")
            .id(egui::Id::new("live-ptype-window"))
            .default_width(420.0)
            .resizable(true)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(
                    "Radar supplies the live precipitation footprint; a model column supplies the surface phase.",
                );
                ui.add_space(4.0);

                egui::ComboBox::from_label("Thermodynamic source")
                    .selected_text(self.source.label())
                    .show_ui(ui, |ui| {
                        for source in [
                            LivePtypeModelSource::Auto,
                            LivePtypeModelSource::Hrrr,
                            LivePtypeModelSource::Rap,
                            LivePtypeModelSource::LocalWrf,
                        ] {
                            ui.selectable_value(&mut self.source, source, source.label());
                        }
                    });
                if self.source == LivePtypeModelSource::LocalWrf {
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("Choose wrfout…").clicked()
                            && let Some(path) = rfd::FileDialog::new()
                                .set_title("Choose WRF / ArWen wrfout")
                                .pick_file()
                        {
                            self.local_wrf_path = Some(path);
                        }
                        ui.label(
                            self.local_wrf_path
                                .as_deref()
                                .and_then(Path::file_name)
                                .and_then(|name| name.to_str())
                                .unwrap_or("No file selected"),
                        );
                    });
                }

                if self.source == LivePtypeModelSource::LocalWrf {
                    ui.label("Surface correction: WRF-native surface");
                } else {
                    egui::ComboBox::from_label("Surface correction")
                        .selected_text(self.surface_mode.label())
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.surface_mode,
                                LivePtypeSurfaceMode::CurrentAnalysis,
                                LivePtypeSurfaceMode::CurrentAnalysis.label(),
                            );
                            ui.selectable_value(
                                &mut self.surface_mode,
                                LivePtypeSurfaceMode::ModelOnly,
                                LivePtypeSurfaceMode::ModelOnly.label(),
                            );
                        });
                }

                ui.separator();
                let previous_mode = self.display_mode;
                egui::ComboBox::from_label("Mode")
                    .selected_text(self.display_mode.label())
                    .show_ui(ui, |ui| {
                        for mode in [
                            LivePtypeDisplayMode::Dominant,
                            LivePtypeDisplayMode::ProbabilityBlend,
                            LivePtypeDisplayMode::IceHazard,
                            LivePtypeDisplayMode::Diagnostics,
                        ] {
                            ui.selectable_value(&mut self.display_mode, mode, mode.label());
                        }
                    });
                invalidate_render |= previous_mode != self.display_mode;

                let previous_occurrence = self.occurrence_mode;
                egui::ComboBox::from_label("Occurrence")
                    .selected_text(self.occurrence_mode.label())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.occurrence_mode,
                            LivePtypeOccurrenceMode::Radar,
                            LivePtypeOccurrenceMode::Radar.label(),
                        );
                        ui.selectable_value(
                            &mut self.occurrence_mode,
                            LivePtypeOccurrenceMode::ModelOnlyDiagnostic,
                            LivePtypeOccurrenceMode::ModelOnlyDiagnostic.label(),
                        );
                    });
                invalidate_render |= previous_occurrence != self.occurrence_mode;

                let old_threshold = self.mixed_threshold;
                ui.add(
                    egui::Slider::new(
                        &mut self.mixed_threshold,
                        MIN_MIXED_THRESHOLD..=MAX_MIXED_THRESHOLD,
                    )
                    .text("Mixed threshold")
                    .custom_formatter(|value, _| format!("{:.0}%", value * 100.0)),
                );
                invalidate_render |= (old_threshold - self.mixed_threshold).abs() > f32::EPSILON;
                ui.horizontal_wrapped(|ui| {
                    ui.label("Legend:");
                    for (label, rgb) in [
                        ("Rain", COLOR_RAIN),
                        ("Snow", COLOR_SNOW),
                        ("Freezing rain", COLOR_FREEZING_RAIN),
                        ("Ice pellets", COLOR_ICE_PELLETS),
                        ("Mixed", COLOR_MIXED),
                    ] {
                        ui.label(
                            egui::RichText::new(format!(" {label} "))
                                .color(egui::Color32::WHITE)
                                .background_color(egui::Color32::from_rgb(
                                    rgb[0], rgb[1], rgb[2],
                                )),
                        );
                    }
                });
                ui.add(egui::Slider::new(&mut self.opacity, 0.05..=1.0).text("Opacity"));
                ui.checkbox(&mut self.show_uncertainty_qc, "Show uncertainty / QC in inspector");

                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(!self.is_busy(), egui::Button::new("Refresh analysis"))
                        .clicked()
                    {
                        refresh = true;
                    }
                    ui.checkbox(&mut self.visible, "Visible");
                    if self.is_busy() {
                        ui.spinner();
                    }
                });
                ui.label(self.status_label());
                if let Some(frame) = &self.frame {
                    ui.small(frame.provenance.truth_label());
                    egui::Grid::new("live-ptype-provenance")
                        .num_columns(2)
                        .striped(true)
                        .show(ui, |ui| {
                            ui.label("Model cycle");
                            ui.monospace(frame.provenance.model_cycle.format("%Y-%m-%d %H:%MZ").to_string());
                            ui.end_row();
                            ui.label("Algorithm");
                            ui.monospace(format!(
                                "Modified Bourgouin v{}",
                                frame.provenance.algorithm_version
                            ));
                            ui.end_row();
                            ui.label("Model valid");
                            ui.monospace(frame.provenance.model_valid.format("%Y-%m-%d %H:%MZ").to_string());
                            ui.end_row();
                            ui.label("Surface analysis");
                            ui.monospace(format!(
                                "{} {}",
                                frame.provenance.surface_analysis_id,
                                frame.provenance.surface_analysis_valid.format("%Y-%m-%d %H:%MZ")
                            ));
                            ui.end_row();
                            if let Some(cells) = frame.provenance.surface_replaced_cells {
                                ui.label("Surface cells replaced");
                                ui.monospace(cells.to_string());
                                ui.end_row();
                            }
                            if let Some(index) = frame.provenance.local_time_index {
                                ui.label("WRF time index");
                                ui.monospace(index.to_string());
                                ui.end_row();
                            }
                            ui.label("Radar scan");
                            ui.monospace(self.last_radar.as_ref().map_or_else(
                                || "waiting for mask".to_owned(),
                                |radar| format!("{} {}", radar.source, radar.scan_time.format("%Y-%m-%d %H:%M:%SZ")),
                            ));
                            ui.end_row();
                            if let Some(ms) = self.last_render_ms {
                                ui.label("Last raster");
                                ui.monospace(format!("{ms:.1} ms"));
                                ui.end_row();
                            }
                        });
                }
                if self.occurrence_mode == LivePtypeOccurrenceMode::ModelOnlyDiagnostic {
                    ui.colored_label(
                        egui::Color32::YELLOW,
                        "Diagnostic mode paints the phase prior without proving precipitation is occurring.",
                    );
                }
                ui.collapsing("Known limitations", |ui| {
                    ui.label("• Freezing drizzle is not distinguishable from freezing rain.");
                    ui.label("• Model profile errors can shift the sleet/freezing-rain boundary.");
                    ui.label("• A missing radar mask is transparent in Radar footprint mode.");
                });
            });
        self.window_open = open;
        self.persisted_dirty |= persisted_before != self.persisted_snapshot();
        if invalidate_render {
            self.texture = None;
        }
        if refresh {
            self.request_refresh(ctx, cache_root, view_bounds, target_valid);
        }
    }
}

fn load_frame(request: LivePtypeFetchRequest) -> Result<LivePtypeFrame, String> {
    if request.source == LivePtypeModelSource::LocalWrf {
        return load_local_wrf_frame(request);
    }
    load_operational_frame(request)
}

fn load_operational_frame(request: LivePtypeFetchRequest) -> Result<LivePtypeFrame, String> {
    std::fs::create_dir_all(&request.cache_root)
        .map_err(|error| format!("create precipitation-type cache: {error}"))?;
    let mut failures = Vec::new();
    // Every candidate is valid at the displayed radar hour. f000 is tried
    // first, then earlier cycles at f001..f003 to tolerate publication lag
    // without ever drifting the thermodynamic prior to another valid time.
    for candidate in operational_model_candidates(request.source, request.target_valid) {
        match load_operational_model(&request, candidate) {
            Ok(frame) => return Ok(frame),
            Err(error) => failures.push(format!(
                "{} {} f{:03}: {error}",
                candidate.model.as_str().to_ascii_uppercase(),
                candidate.cycle.format("%Y%m%d %HZ"),
                candidate.forecast_hour,
            )),
        }
    }
    Err(format!(
        "no precipitation-type source loaded ({})",
        failures.join("; ")
    ))
}

#[derive(Debug, Clone, Copy)]
struct OperationalModelCandidate {
    model: ModelId,
    cycle: DateTime<Utc>,
    forecast_hour: u16,
}

fn operational_model_candidates(
    source: LivePtypeModelSource,
    target_valid: DateTime<Utc>,
) -> Vec<OperationalModelCandidate> {
    let target_hour = target_analysis_hour(target_valid);
    let models: &[ModelId] = match source {
        LivePtypeModelSource::Auto => &[ModelId::Hrrr, ModelId::Rap],
        LivePtypeModelSource::Hrrr => &[ModelId::Hrrr],
        LivePtypeModelSource::Rap => &[ModelId::Rap],
        LivePtypeModelSource::LocalWrf => return Vec::new(),
    };
    let mut candidates = Vec::new();
    for &model in models {
        for forecast_hour in 0..=MAX_OPERATIONAL_LOOKBACK_HOURS {
            let cycle = target_hour - ChronoDuration::hours(i64::from(forecast_hour));
            candidates.push(OperationalModelCandidate {
                model,
                cycle,
                forecast_hour,
            });
        }
    }
    candidates
}

fn load_operational_model(
    request: &LivePtypeFetchRequest,
    candidate: OperationalModelCandidate,
) -> Result<LivePtypeFrame, String> {
    let model = candidate.model;
    let date = candidate.cycle.format("%Y%m%d").to_string();
    let loaded = load_model_timestep_from_parts_cropped(
        model,
        &date,
        Some(candidate.cycle.hour() as u8),
        candidate.forecast_hour,
        SourceId::Aws,
        None,
        None,
        &request.cache_root,
        true,
        request.bounds.tuple(),
    )
    .map_err(|error| format!("load cropped thermodynamic fields: {error}"))?;

    let model_cycle = cycle_datetime(
        &loaded.latest.cycle.date_yyyymmdd,
        loaded.latest.cycle.hour_utc,
    )?;
    let model_valid = model_cycle + ChronoDuration::hours(i64::from(candidate.forecast_hour));
    let target_hour = target_analysis_hour(request.target_valid);
    if model_valid != target_hour {
        return Err(format!(
            "resolved valid time {} does not match radar hour {}",
            model_valid.format("%Y-%m-%d %H:%MZ"),
            target_hour.format("%Y-%m-%d %H:%MZ")
        ));
    }
    let resolved_model_source = loaded.latest.source;
    let mut columns =
        prepare_hrrr_rap_columns(&loaded.surface_decode.value, &loaded.pressure_decode.value)
            .map_err(|error| format!("prepare model columns: {error}"))?;
    let lat_lon = columns.grid.clone();

    let mut surface_id = model.as_str().to_ascii_uppercase();
    let mut surface_valid = model_valid;
    let mut replaced_cells = None;
    let mut surface_note = "model surface retained".to_owned();
    if request.surface_mode == LivePtypeSurfaceMode::CurrentAnalysis {
        if rtma_is_operationally_available(target_hour, Utc::now()) {
            let rtma_date = target_hour.format("%Y%m%d").to_string();
            match resolve_model_run(
                ModelId::Rtma,
                &rtma_date,
                Some(target_hour.hour() as u8),
                0,
                SourceId::Nomads,
            )
            .map_err(|error| error.to_string())
            .and_then(|latest| {
                load_surface_geometry_from_latest(latest, 0, None, &request.cache_root, true)
                    .map_err(|error| error.to_string())
            }) {
                Ok(rtma) => {
                    let report = replace_current_surface_from_analysis(
                        &mut columns,
                        &rtma.surface_decode.value,
                        SurfaceReplacementOptions::default(),
                    )
                    .map_err(|error| format!("apply RTMA surface analysis: {error}"))?;
                    surface_id = "RTMA".to_owned();
                    surface_valid = cycle_datetime(
                        &rtma.latest.cycle.date_yyyymmdd,
                        rtma.latest.cycle.hour_utc,
                    )?;
                    replaced_cells = Some(report.replaced_cells);
                    surface_note = format!(
                        "time-matched RTMA surface replacement applied to {} of {} cells",
                        report.replaced_cells, report.target_cells
                    );
                }
                Err(error) => {
                    // An explicit, honest fallback keeps a transient RTMA outage
                    // from discarding the usable, time-matched model profile.
                    surface_note =
                        format!("time-matched RTMA unavailable ({error}); model surface retained");
                }
            }
        } else {
            surface_note = format!(
                "historical archive hour {}; operational RTMA not requested; model surface retained",
                target_hour.format("%Y-%m-%d %H:%MZ")
            );
        }
    }

    let resolution_m = model_resolution_m(model);
    drop(loaded);
    let metadata = LivePtypeMetadata::new(
        model.as_str(),
        model_cycle,
        model_valid,
        resolution_m,
        surface_id.clone(),
        surface_valid,
    );
    let analysis = analyze_prepared_columns(&columns, None, &PtypeOptions::default(), metadata)
        .map_err(|error| format!("classify model columns: {error}"))?;
    let provenance = LivePtypeProvenance {
        algorithm_version: CURRENT_PTYPE_ALGORITHM_VERSION.0,
        model_id: model.as_str().to_owned(),
        model_cycle,
        model_valid,
        model_horizontal_resolution_m: resolution_m,
        surface_analysis_id: surface_id,
        surface_analysis_valid: surface_valid,
        surface_replaced_cells: replaced_cells,
        source_detail: format!(
            "{} via {}; {surface_note}",
            model.as_str().to_ascii_uppercase(),
            resolved_model_source
        ),
        local_time_index: None,
    };
    let request_key = request.key();
    LivePtypeFrame::new(
        analysis,
        lat_lon,
        provenance,
        request_key,
        None,
        request.target_valid,
        request.generation,
    )
}

fn load_local_wrf_frame(request: LivePtypeFetchRequest) -> Result<LivePtypeFrame, String> {
    let path = request
        .local_wrf_path
        .as_deref()
        .ok_or_else(|| "choose a wrfout file first".to_owned())?;
    let local_label = local_file_label(path);
    let file =
        wrf_core::WrfFile::open(path).map_err(|error| format!("open {local_label}: {error}"))?;
    if file.nt == 0 {
        return Err("WRF file has no timesteps".to_owned());
    }
    let times = file
        .times()
        .map_err(|error| format!("read WRF Times: {error}"))?;
    let (time_index, model_valid) =
        nearest_wrf_time_index(&times, request.target_valid).map_err(|error| {
            format!(
                "match WRF timestep to displayed {}: {error}",
                request.target_valid.format("%Y-%m-%d %H:%MZ")
            )
        })?;
    let local_target_window_unix = local_wrf_target_window(&times, model_valid);
    let model_cycle = times
        .first()
        .and_then(|value| parse_wrf_time(value))
        .unwrap_or(model_valid);

    let lat = file
        .xlat(time_index)
        .map_err(|error| format!("read WRF XLAT: {error}"))?;
    let lon = file
        .xlong(time_index)
        .map_err(|error| format!("read WRF XLONG: {error}"))?;
    let grid = LatLonGrid::new(
        rustwx_core::GridShape::new(file.nx, file.ny)
            .map_err(|error| format!("WRF grid shape: {error}"))?,
        lat.iter().map(|&value| value as f32).collect(),
        lon.iter().map(|&value| value as f32).collect(),
    )
    .map_err(|error| format!("WRF geolocation: {error}"))?;
    let pressure = file
        .full_pressure(time_index)
        .map_err(|error| format!("read WRF pressure: {error}"))?;
    let temperature = file
        .temperature_c(time_index)
        .map_err(|error| format!("read WRF temperature: {error}"))?;
    let mixing_ratio = file
        .qvapor(time_index)
        .map_err(|error| format!("read WRF QVAPOR: {error}"))?;
    let height_agl = file
        .height_agl(time_index)
        .map_err(|error| format!("read WRF height AGL: {error}"))?;
    let psfc = file
        .psfc(time_index)
        .map_err(|error| format!("read WRF PSFC: {error}"))?;
    let t2 = file
        .t2(time_index)
        .map_err(|error| format!("read WRF T2: {error}"))?;
    let q2 = file
        .q2(time_index)
        .map_err(|error| format!("read WRF Q2: {error}"))?;

    let columns = prepare_wrf_columns(
        grid.clone(),
        pressure.to_vec(),
        temperature.to_vec(),
        mixing_ratio.to_vec(),
        height_agl.to_vec(),
        psfc.to_vec(),
        t2.to_vec(),
        q2.to_vec(),
    )
    .map_err(|error| format!("prepare WRF columns: {error}"))?;
    let is_arwen = file.global_attr_str("GPUWM_VERSION").is_ok();
    let model_id = if is_arwen { "arwen" } else { "wrf" };
    let resolution_m = file.dx.max(file.dy).max(1.0) as f32;
    let metadata = LivePtypeMetadata::new(
        model_id,
        model_cycle,
        model_valid,
        resolution_m,
        "WRF surface",
        model_valid,
    );
    drop((
        pressure,
        temperature,
        mixing_ratio,
        height_agl,
        psfc,
        t2,
        q2,
    ));
    file.clear_cache();
    let analysis = analyze_prepared_columns(&columns, None, &PtypeOptions::default(), metadata)
        .map_err(|error| format!("classify WRF columns: {error}"))?;
    let provenance = LivePtypeProvenance {
        algorithm_version: CURRENT_PTYPE_ALGORITHM_VERSION.0,
        model_id: model_id.to_owned(),
        model_cycle,
        model_valid,
        model_horizontal_resolution_m: resolution_m,
        surface_analysis_id: "WRF surface".to_owned(),
        surface_analysis_valid: model_valid,
        surface_replaced_cells: None,
        source_detail: format!(
            "{local_label}; timestep matched to displayed {}",
            request.target_valid.format("%Y-%m-%d %H:%MZ")
        ),
        local_time_index: Some(time_index),
    };
    let request_key = request.key();
    LivePtypeFrame::new(
        analysis,
        grid,
        provenance,
        request_key,
        Some(local_target_window_unix),
        request.target_valid,
        request.generation,
    )
}

fn nearest_wrf_time_index<T: AsRef<str>>(
    times: &[T],
    target: DateTime<Utc>,
) -> Result<(usize, DateTime<Utc>), String> {
    let mut best: Option<(usize, DateTime<Utc>, i64)> = None;
    for (index, raw) in times.iter().enumerate() {
        let Some(valid) = parse_wrf_time(raw.as_ref()) else {
            continue;
        };
        let delta = valid
            .signed_duration_since(target)
            .num_seconds()
            .unsigned_abs() as i64;
        let replace = best.as_ref().is_none_or(|(_, best_valid, best_delta)| {
            delta < *best_delta
                || (delta == *best_delta
                    && ((valid <= target && *best_valid > target)
                        || ((valid <= target) == (*best_valid <= target) && valid > *best_valid)))
        });
        if replace {
            best = Some((index, valid, delta));
        }
    }
    let Some((index, valid, delta)) = best else {
        return Err("file has no parseable WRF Times values".to_owned());
    };
    if delta > MAX_LOCAL_WRF_TARGET_MISMATCH_SECONDS {
        return Err(format!(
            "nearest timestep {} is {:.1} hours away (limit 1.5 hours)",
            valid.format("%Y-%m-%d %H:%MZ"),
            delta as f64 / 3_600.0
        ));
    }
    Ok((index, valid))
}

/// Inclusive display-time window for which `selected` remains the nearest
/// parseable WRF timestep. Midpoint ties belong to the earlier timestep,
/// matching `nearest_wrf_time_index`; the outer edges retain the 90-minute
/// maximum-mismatch safeguard.
fn local_wrf_target_window<T: AsRef<str>>(times: &[T], selected: DateTime<Utc>) -> (i64, i64) {
    let selected = selected.timestamp();
    let mut previous = None;
    let mut next = None;
    for raw in times {
        let Some(valid) = parse_wrf_time(raw.as_ref()).map(|time| time.timestamp()) else {
            continue;
        };
        if valid < selected {
            previous = Some(previous.map_or(valid, |current: i64| current.max(valid)));
        } else if valid > selected {
            next = Some(next.map_or(valid, |current: i64| current.min(valid)));
        }
    }
    let mut first = selected - MAX_LOCAL_WRF_TARGET_MISMATCH_SECONDS;
    let mut last = selected + MAX_LOCAL_WRF_TARGET_MISMATCH_SECONDS;
    if let Some(previous) = previous {
        // The exact midpoint (when integral) belongs to `previous`, hence +1.
        first = first.max(previous + (selected - previous) / 2 + 1);
    }
    if let Some(next) = next {
        // The selected timestep is the earlier member of this pair, so an
        // exact midpoint remains inside its reuse window.
        last = last.min(selected + (next - selected) / 2);
    }
    (first, last)
}

fn build_radar_occurrence(
    source: &LivePtypeRadarSourceSnapshot,
    size: [usize; 2],
) -> Result<LivePtypeRadarOccurrence, String> {
    if size[0] == 0 || size[1] == 0 || source.viewport.width == 0 || source.viewport.height == 0 {
        return Err("radar occurrence viewport is empty".to_owned());
    }
    let scale_x = size[0] as f32 / source.viewport.width as f32;
    let scale_y = size[1] as f32 / source.viewport.height as f32;
    let options = ViewportRasterOptions {
        width: size[0] as u32,
        height: size[1] as u32,
        radar_x_px: source.viewport.radar_x_px * scale_x,
        radar_y_px: source.viewport.radar_y_px * scale_y,
        km_per_px_x: source.viewport.km_per_px_x / scale_x.max(f32::EPSILON),
        km_per_px_y: source.viewport.km_per_px_y / scale_y.max(f32::EPSILON),
        rotation_rad: source.viewport.rotation_rad,
    };
    let mut tables = ColorTableSet::default();
    let permissive_reflectivity = tables
        .for_family(ColorTableFamily::Reflectivity)
        .with_display_threshold(Some(-10.0), false);
    tables.set_family(ColorTableFamily::Reflectivity, permissive_reflectivity);
    let cache = ViewportMomentCache::new_with_color_tables_for_family(
        source.volume.as_ref(),
        source.reflectivity_cut,
        MomentType::Reflectivity,
        &tables,
        Some(ColorTableFamily::Reflectivity),
    )
    .map_err(|error| format!("build low-level reflectivity mask: {error}"))?;
    let mut rgba = vec![0_u8; viewport_rgba_buffer_len(options)];
    cache
        .render_moment_rgba_into(source.volume.as_ref(), options, &mut rgba)
        .map_err(|error| format!("render low-level reflectivity mask: {error}"))?;
    LivePtypeRadarOccurrence::from_rgba(size, &rgba, source.source.clone(), source.scan_time)
        .ok_or_else(|| "rendered reflectivity mask dimensions disagree".to_owned())
}

fn render_image(
    frame: &LivePtypeFrame,
    key: &LivePtypeRenderKey,
    occurrence: Option<&LivePtypeRadarOccurrence>,
    now: DateTime<Utc>,
) -> egui::ColorImage {
    let [width, height] = key.size;
    let mut pixels = vec![egui::Color32::TRANSPARENT; width * height];
    let km_per_point = 111.32 / f64::from(key.view.map_scale.max(1.0e-4));
    let points_per_pixel_x = f64::from(key.viewport_size_points[0]) / width as f64;
    let points_per_pixel_y = f64::from(key.viewport_size_points[1]) / height as f64;
    let mixed_threshold = f32::from(key.threshold_milli) / 1_000.0;
    let age_alpha = age_fade(now, frame.provenance.model_valid)
        * age_fade(now, frame.provenance.surface_analysis_valid);
    pixels
        .par_iter_mut()
        .enumerate()
        .for_each(|(index, pixel)| {
            let x = index % width;
            let y = index / width;
            let occurrence_alpha =
                occurrence_alpha(key.occurrence_mode, occurrence, x, y, width, height);
            if occurrence_alpha == 0 {
                return;
            }
            let east_km = (x as f64 - width as f64 * 0.5) * points_per_pixel_x * km_per_point;
            let north_km = (height as f64 * 0.5 - y as f64) * points_per_pixel_y * km_per_point;
            let (lat, lon) = aeqd_inverse_km(
                f64::from(key.view.center_lat),
                f64::from(key.view.center_lon),
                east_km,
                north_km,
            );
            let Some(sample) = frame.sample(lat as f32, lon as f32, mixed_threshold) else {
                return;
            };
            let (rgb, phase_alpha) = sample_color(&sample, key.display_mode);
            let alpha = (f32::from(occurrence_alpha) * phase_alpha * age_alpha)
                .round()
                .clamp(0.0, 255.0) as u8;
            if alpha > 0 {
                *pixel = egui::Color32::from_rgba_unmultiplied(rgb[0], rgb[1], rgb[2], alpha);
            }
        });
    egui::ColorImage {
        size: key.size,
        source_size: egui::vec2(width as f32, height as f32),
        pixels,
    }
}

fn sample_four_fields(
    frame: &LivePtypeFrame,
    nearest: usize,
    lat: f32,
    lon: f32,
) -> Option<[f32; 4]> {
    let fields = [
        frame.analysis.rain_powt_pct.as_slice(),
        frame.analysis.snow_powt_pct.as_slice(),
        frame.analysis.freezing_rain_powt_pct.as_slice(),
        frame.analysis.ice_pellets_powt_pct.as_slice(),
    ];
    for stencil in sample_stencils_for_point(&frame.grid, nearest, lat, lon)
        .into_iter()
        .flatten()
    {
        let (x0, y0, _, _) = stencil.window_bounds();
        let nx = frame.grid.nx;
        let ids = [
            y0 * nx + x0,
            y0 * nx + x0 + 1,
            (y0 + 1) * nx + x0,
            (y0 + 1) * nx + x0 + 1,
        ];
        let mut sampled = [0.0; 4];
        let mut valid = true;
        for (field_index, field) in fields.iter().enumerate() {
            let corners = [field[ids[0]], field[ids[1]], field[ids[2]], field[ids[3]]];
            match stencil.sample(corners) {
                Some(value) if value.is_finite() => sampled[field_index] = value.clamp(0.0, 100.0),
                _ => {
                    valid = false;
                    break;
                }
            }
        }
        if valid {
            return Some(sampled);
        }
    }
    let mut sampled = [0.0; 4];
    for (field_index, field) in fields.iter().enumerate() {
        let value = *field.get(nearest)?;
        if !value.is_finite() {
            return None;
        }
        sampled[field_index] = value.clamp(0.0, 100.0);
    }
    Some(sampled)
}

fn normalize_scores(scores: [f32; 4]) -> Option<[f32; 4]> {
    if scores.iter().any(|value| !value.is_finite()) {
        return None;
    }
    let clamped = scores.map(|value| value.clamp(0.0, 100.0));
    let sum: f32 = clamped.iter().sum();
    if !sum.is_finite() || sum <= f32::EPSILON {
        return None;
    }
    Some(clamped.map(|value| value / sum))
}

fn winner_and_confidence(fractions: [f32; 4]) -> (usize, f32) {
    let mut winner = 0;
    let mut confidence = fractions[0];
    for (index, value) in fractions.into_iter().enumerate().skip(1) {
        // Match rustwx-calc's deterministic tie rule: the earlier field wins
        // equal scores. At the default 0.60 threshold ties are Mixed anyway,
        // but 0.50 is a supported presentation choice.
        if value > confidence {
            winner = index;
            confidence = value;
        }
    }
    (winner, confidence)
}

fn category_for_index(index: usize) -> LivePtypeCategory {
    match index {
        0 => LivePtypeCategory::Rain,
        1 => LivePtypeCategory::Snow,
        2 => LivePtypeCategory::FreezingRain,
        3 => LivePtypeCategory::IcePellets,
        _ => LivePtypeCategory::Unknown,
    }
}

fn ptype_qc_label(bits: u16) -> String {
    let qc = PtypeQc::from_bits(bits);
    if qc.is_clean() {
        return "ptype QC clean".to_owned();
    }
    let flags = [
        (PtypeQc::ACTIVE_MASK_OFF, "inactive mask"),
        (
            PtypeQc::INVALID_INPUT_LEVEL_REMOVED,
            "invalid level removed",
        ),
        (PtypeQc::WET_BULB_FAILURE, "wet-bulb failure"),
        (PtypeQc::HEIGHTS_REORDERED, "heights reordered"),
        (
            PtypeQc::DUPLICATE_HEIGHT_REMOVED,
            "duplicate height removed",
        ),
        (PtypeQc::NEGATIVE_HEIGHT_CLAMPED, "negative height clamped"),
        (PtypeQc::INSUFFICIENT_PROFILE, "insufficient profile"),
        (PtypeQc::SURFACE_LEVEL_MISSING, "surface level missing"),
        (
            PtypeQc::NO_PRECIP_GENERATION_LAYER,
            "no precip-generation layer",
        ),
        (
            PtypeQc::UPPER_GENERATION_LAYER_REMOVED,
            "upper generation layer removed",
        ),
        (PtypeQc::ZERO_TOTAL_SCORE, "zero total score"),
        (
            PtypeQc::BELOW_GROUND_LEVEL_REMOVED,
            "below-ground level removed",
        ),
    ];
    let mut names = flags
        .iter()
        .filter(|(flag, _)| qc.contains(*flag))
        .map(|(_, name)| (*name).to_owned())
        .collect::<Vec<_>>();
    let known_bits = flags
        .iter()
        .fold(0_u16, |known, (flag, _)| known | flag.bits());
    let unknown_bits = bits & !known_bits;
    if unknown_bits != 0 {
        names.push(format!("unknown 0x{unknown_bits:04x}"));
    }
    format!("ptype QC: {}", names.join(", "))
}

fn sample_color(sample: &LivePtypeSample, mode: LivePtypeDisplayMode) -> ([u8; 3], f32) {
    match mode {
        LivePtypeDisplayMode::Dominant => (
            sample.category.color(),
            (0.52 + sample.confidence * 0.48).clamp(0.0, 1.0),
        ),
        LivePtypeDisplayMode::ProbabilityBlend => {
            let colors = [
                COLOR_RAIN,
                COLOR_SNOW,
                COLOR_FREEZING_RAIN,
                COLOR_ICE_PELLETS,
            ];
            let mut rgb = [0.0_f32; 3];
            for (fraction, color) in sample.fractions.iter().zip(colors) {
                for channel in 0..3 {
                    rgb[channel] += *fraction * f32::from(color[channel]);
                }
            }
            (
                rgb.map(|value| value.round().clamp(0.0, 255.0) as u8),
                (0.45 + sample.confidence * 0.55).clamp(0.0, 1.0),
            )
        }
        LivePtypeDisplayMode::IceHazard => {
            let freezing_rain = sample.fractions[2];
            let ice_pellets = sample.fractions[3];
            let hazard = (freezing_rain + ice_pellets).clamp(0.0, 1.0);
            if hazard <= 0.01 {
                return ([0, 0, 0], 0.0);
            }
            let fzra_share = freezing_rain / hazard;
            let rgb = [0, 1, 2].map(|channel| {
                (f32::from(COLOR_FREEZING_RAIN[channel]) * fzra_share
                    + f32::from(COLOR_ICE_PELLETS[channel]) * (1.0 - fzra_share))
                    .round()
                    .clamp(0.0, 255.0) as u8
            });
            (rgb, hazard.sqrt())
        }
        LivePtypeDisplayMode::Diagnostics => {
            let confidence = sample.confidence.clamp(0.0, 1.0);
            let rgb = [
                ((1.0 - confidence) * 235.0) as u8,
                (confidence * 210.0) as u8,
                (110.0 + confidence * 125.0) as u8,
            ];
            (rgb, 0.78)
        }
    }
}

fn occurrence_alpha(
    mode: LivePtypeOccurrenceMode,
    occurrence: Option<&LivePtypeRadarOccurrence>,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
) -> u8 {
    if mode == LivePtypeOccurrenceMode::ModelOnlyDiagnostic {
        return 220;
    }
    let Some(mask) = occurrence else {
        return 0;
    };
    if mask.size[0] == 0 || mask.size[1] == 0 || mask.alpha.len() != mask.size[0] * mask.size[1] {
        return 0;
    }
    let source_x = x.saturating_mul(mask.size[0]) / width.max(1);
    let source_y = y.saturating_mul(mask.size[1]) / height.max(1);
    mask.alpha[source_y.min(mask.size[1] - 1) * mask.size[0] + source_x.min(mask.size[0] - 1)]
}

fn occurrence_render_generation(
    mode: LivePtypeOccurrenceMode,
    radar_generation: Option<u64>,
) -> Option<u64> {
    (mode == LivePtypeOccurrenceMode::Radar)
        .then_some(radar_generation)
        .flatten()
}

fn render_size(rect: egui::Rect) -> [usize; 2] {
    let mut width = (rect.width().max(8.0) * RENDER_SCALE).round() as usize;
    let mut height = (rect.height().max(8.0) * RENDER_SCALE).round() as usize;
    let longest = width.max(height);
    if longest > MAX_RASTER_AXIS {
        let scale = MAX_RASTER_AXIS as f64 / longest as f64;
        width = (width as f64 * scale).round().max(8.0) as usize;
        height = (height as f64 * scale).round().max(8.0) as usize;
    }
    [width.max(8), height.max(8)]
}

fn render_key_matches(have: &LivePtypeRenderKey, wanted: &LivePtypeRenderKey) -> bool {
    have.frame_generation == wanted.frame_generation
        && have.thermo_request_key == wanted.thermo_request_key
        && have.reference_time_millis == wanted.reference_time_millis
        && have.size == wanted.size
        && have.viewport_size_points == wanted.viewport_size_points
        && have.display_mode == wanted.display_mode
        && have.occurrence_mode == wanted.occurrence_mode
        && have.threshold_milli == wanted.threshold_milli
        && have.occurrence_generation == wanted.occurrence_generation
        && !model_layer_view_needs_rerender(&have.view, &wanted.view)
}

fn target_analysis_hour(time: DateTime<Utc>) -> DateTime<Utc> {
    time.with_minute(0)
        .and_then(|time| time.with_second(0))
        .and_then(|time| time.with_nanosecond(0))
        .expect("UTC hour components are valid")
}

fn rtma_is_operationally_available(target: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    let age_seconds = now.signed_duration_since(target).num_seconds();
    (-3_600..=RTMA_OPERATIONAL_WINDOW_HOURS * 3_600).contains(&age_seconds)
}

fn age_fade(now: DateTime<Utc>, valid: DateTime<Utc>) -> f32 {
    let seconds = now.signed_duration_since(valid).num_seconds().max(0) as f32;
    let hours = seconds / 3_600.0;
    if hours <= 2.0 {
        1.0
    } else if hours >= 8.0 {
        0.30
    } else {
        1.0 - (hours - 2.0) / 6.0 * 0.70
    }
}

fn signed_age_label(now: DateTime<Utc>, valid: DateTime<Utc>) -> String {
    let seconds = now.signed_duration_since(valid).num_seconds();
    let (prefix, seconds) = if seconds < 0 {
        ("in ", -seconds)
    } else {
        ("", seconds)
    };
    if seconds < 60 {
        format!("{prefix}{seconds}s")
    } else if seconds < 3_600 {
        format!("{prefix}{}m", seconds / 60)
    } else {
        format!("{prefix}{:.1}h", seconds as f64 / 3_600.0)
    }
}

fn cycle_datetime(date_yyyymmdd: &str, hour_utc: u8) -> Result<DateTime<Utc>, String> {
    let date = NaiveDate::parse_from_str(date_yyyymmdd, "%Y%m%d")
        .map_err(|error| format!("invalid model cycle date {date_yyyymmdd}: {error}"))?;
    let naive = date
        .and_hms_opt(u32::from(hour_utc), 0, 0)
        .ok_or_else(|| format!("invalid model cycle hour {hour_utc}"))?;
    Ok(Utc.from_utc_datetime(&naive))
}

fn parse_wrf_time(value: &str) -> Option<DateTime<Utc>> {
    let naive = NaiveDateTime::parse_from_str(value.trim(), "%Y-%m-%d_%H:%M:%S").ok()?;
    Some(Utc.from_utc_datetime(&naive))
}

fn local_file_label(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("local wrfout")
        .to_owned()
}

fn model_resolution_m(model: ModelId) -> f32 {
    match model {
        ModelId::Hrrr => 3_000.0,
        ModelId::Rap => 13_000.0,
        _ => f32::NAN,
    }
}

fn finite_or(value: f64, fallback: f64) -> f64 {
    if value.is_finite() { value } else { fallback }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_scores_are_derived_after_interpolation() {
        let fractions = normalize_scores([80.0, 20.0, 0.0, 0.0]).expect("valid scores");
        assert_eq!(fractions, [0.8, 0.2, 0.0, 0.0]);
        let (winner, confidence) = winner_and_confidence(fractions);
        assert_eq!(category_for_index(winner), LivePtypeCategory::Rain);
        assert!((confidence - 0.8).abs() < 1.0e-6);
    }

    #[test]
    fn mixed_threshold_preserves_real_transition_zones() {
        let fractions = normalize_scores([2.0, 2.0, 5.0, 4.0]).expect("valid scores");
        let (winner, confidence) = winner_and_confidence(fractions);
        let category = if confidence < DEFAULT_MIXED_THRESHOLD {
            LivePtypeCategory::Mixed
        } else {
            category_for_index(winner)
        };
        assert_eq!(category, LivePtypeCategory::Mixed);
        assert!(confidence < DEFAULT_MIXED_THRESHOLD);
    }

    #[test]
    fn app_derivation_matches_products_contract_after_regridding() {
        let rain = [80.0, 25.0, 10.0, 50.0];
        let snow = [20.0, 25.0, 10.0, 50.0];
        let freezing_rain = [0.0, 40.0, 70.0, 0.0];
        let ice_pellets = [0.0, 10.0, 10.0, 0.0];
        let upstream = rustwx_products::ptype::derive_display_fields_after_regrid(
            &rain,
            &snow,
            &freezing_rain,
            &ice_pellets,
            f64::from(DEFAULT_MIXED_THRESHOLD),
        )
        .expect("valid display derivation");
        for index in 0..rain.len() {
            let fractions = normalize_scores([
                rain[index],
                snow[index],
                freezing_rain[index],
                ice_pellets[index],
            ])
            .expect("nonzero scores");
            let (winner, confidence) = winner_and_confidence(fractions);
            let category = if confidence < DEFAULT_MIXED_THRESHOLD {
                LivePtypeCategory::Mixed
            } else {
                category_for_index(winner)
            };
            assert_eq!(category.code(), upstream.display_type_code[index]);
            assert!((confidence - upstream.confidence[index]).abs() < 1.0e-6);
        }
    }

    #[test]
    fn radar_mask_is_resampled_without_inventing_echo() {
        let mask = LivePtypeRadarOccurrence::uniform([2, 2], 0);
        assert_eq!(
            occurrence_alpha(LivePtypeOccurrenceMode::Radar, Some(&mask), 3, 3, 4, 4),
            0
        );
        assert_eq!(
            occurrence_alpha(LivePtypeOccurrenceMode::Radar, None, 0, 0, 4, 4),
            0
        );
    }

    #[test]
    fn radar_scan_is_not_part_of_thermodynamic_generation() {
        let first_generation = 1;
        let second_generation = 2;
        // Radar changes key the render only. The frame generation is supplied
        // independently by the thermodynamic worker.
        let view = ModelLayerView {
            center_lat: 40.0,
            center_lon: -97.0,
            map_scale: 80.0,
        };
        let scan_time = Utc::now();
        let key = |generation| LivePtypeRenderKey {
            frame_generation: 7,
            thermo_request_key: live_ptype_request_key(
                LivePtypeModelSource::Hrrr,
                LivePtypeSurfaceMode::ModelOnly,
                None,
                scan_time,
            ),
            reference_time_millis: scan_time.timestamp_millis(),
            view,
            size: [100, 100],
            viewport_size_points: [100, 100],
            display_mode: LivePtypeDisplayMode::Dominant,
            occurrence_mode: LivePtypeOccurrenceMode::Radar,
            threshold_milli: 600,
            occurrence_generation: Some(generation),
        };
        assert_eq!(
            key(first_generation).frame_generation,
            key(second_generation).frame_generation
        );
        assert_ne!(
            key(first_generation).occurrence_generation,
            key(second_generation).occurrence_generation
        );
        assert_eq!(
            occurrence_render_generation(
                LivePtypeOccurrenceMode::ModelOnlyDiagnostic,
                Some(second_generation),
            ),
            None,
            "model-only diagnostic renders must not be invalidated by radar scans"
        );
    }

    #[test]
    fn ptype_codes_match_public_contract() {
        assert_eq!(LivePtypeCategory::NoPrecip.code(), 0);
        assert_eq!(LivePtypeCategory::Rain.code(), 1);
        assert_eq!(LivePtypeCategory::Snow.code(), 2);
        assert_eq!(LivePtypeCategory::FreezingRain.code(), 3);
        assert_eq!(LivePtypeCategory::IcePellets.code(), 4);
        assert_eq!(LivePtypeCategory::Mixed.code(), 5);
        assert_eq!(LivePtypeCategory::Unknown.code(), 255);
    }

    #[test]
    fn qc_text_names_flags_and_clean_state() {
        assert_eq!(ptype_qc_label(0), "ptype QC clean");
        let bits = PtypeQc::WET_BULB_FAILURE.bits() | PtypeQc::BELOW_GROUND_LEVEL_REMOVED.bits();
        let label = ptype_qc_label(bits);
        assert!(label.contains("wet-bulb failure"));
        assert!(label.contains("below-ground level removed"));
        assert!(!label.contains("0x0000"));
    }

    #[test]
    fn wrf_time_is_always_read_as_utc() {
        let parsed = parse_wrf_time("2026-01-15_12:34:56").expect("valid WRF time");
        assert_eq!(parsed.to_rfc3339(), "2026-01-15T12:34:56+00:00");
    }

    #[test]
    fn archive_target_is_floored_without_looking_into_the_future() {
        let target = Utc.with_ymd_and_hms(2026, 1, 25, 15, 8, 48).unwrap();
        assert_eq!(
            target_analysis_hour(target),
            Utc.with_ymd_and_hms(2026, 1, 25, 15, 0, 0).unwrap()
        );
    }

    #[test]
    fn operational_candidates_keep_one_exact_valid_hour_across_midnight() {
        let target = Utc.with_ymd_and_hms(2026, 1, 25, 0, 15, 0).unwrap();
        let candidates = operational_model_candidates(LivePtypeModelSource::Hrrr, target);
        assert_eq!(candidates.len(), 4);
        assert_eq!(
            candidates[0].cycle,
            Utc.with_ymd_and_hms(2026, 1, 25, 0, 0, 0).unwrap()
        );
        assert_eq!(candidates[0].forecast_hour, 0);
        assert_eq!(
            candidates[1].cycle,
            Utc.with_ymd_and_hms(2026, 1, 24, 23, 0, 0).unwrap()
        );
        assert_eq!(candidates[1].forecast_hour, 1);
        for candidate in candidates {
            assert_eq!(
                candidate.cycle + ChronoDuration::hours(i64::from(candidate.forecast_hour)),
                target_analysis_hour(target)
            );
        }
    }

    #[test]
    fn auto_candidates_exhaust_hrrr_before_falling_back_to_rap() {
        let target = Utc.with_ymd_and_hms(2026, 1, 25, 15, 8, 48).unwrap();
        let candidates = operational_model_candidates(LivePtypeModelSource::Auto, target);
        assert_eq!(candidates.len(), 8);
        assert!(
            candidates[..4]
                .iter()
                .all(|candidate| candidate.model == ModelId::Hrrr)
        );
        assert!(
            candidates[4..]
                .iter()
                .all(|candidate| candidate.model == ModelId::Rap)
        );
    }

    #[test]
    fn local_wrf_uses_nearest_displayed_time_and_prefers_the_prior_on_a_tie() {
        let times = [
            "2026-01-25_14:00:00".to_owned(),
            "2026-01-25_15:00:00".to_owned(),
            "2026-01-25_16:00:00".to_owned(),
        ];
        let target = Utc.with_ymd_and_hms(2026, 1, 25, 15, 30, 0).unwrap();
        let (index, valid) = nearest_wrf_time_index(&times, target).expect("matched time");
        assert_eq!(index, 1);
        assert_eq!(valid, Utc.with_ymd_and_hms(2026, 1, 25, 15, 0, 0).unwrap());
    }

    #[test]
    fn local_wrf_reuses_only_the_selected_subhourly_timestep_window() {
        let times = [
            "2026-01-25_15:04:00",
            "2026-01-25_15:05:00",
            "2026-01-25_15:06:00",
            "2026-01-25_15:55:00",
        ];
        let selected = Utc.with_ymd_and_hms(2026, 1, 25, 15, 5, 0).unwrap();
        let window = local_wrf_target_window(&times, selected);
        assert!(
            (window.0..=window.1).contains(
                &Utc.with_ymd_and_hms(2026, 1, 25, 15, 5, 20)
                    .unwrap()
                    .timestamp()
            )
        );
        assert!(
            !(window.0..=window.1).contains(
                &Utc.with_ymd_and_hms(2026, 1, 25, 15, 5, 31)
                    .unwrap()
                    .timestamp()
            )
        );
        let (index, valid) = nearest_wrf_time_index(
            &times,
            Utc.with_ymd_and_hms(2026, 1, 25, 15, 55, 0).unwrap(),
        )
        .expect("later subhourly timestep");
        assert_eq!(index, 3);
        assert_eq!(valid, Utc.with_ymd_and_hms(2026, 1, 25, 15, 55, 0).unwrap());
    }

    #[test]
    fn request_keys_bucket_operational_data_but_keep_local_wrf_time_exact() {
        let early = Utc.with_ymd_and_hms(2026, 1, 25, 15, 5, 0).unwrap();
        let late = Utc.with_ymd_and_hms(2026, 1, 25, 15, 55, 0).unwrap();
        assert_eq!(
            live_ptype_request_key(
                LivePtypeModelSource::Hrrr,
                LivePtypeSurfaceMode::ModelOnly,
                None,
                early,
            ),
            live_ptype_request_key(
                LivePtypeModelSource::Hrrr,
                LivePtypeSurfaceMode::ModelOnly,
                None,
                late,
            )
        );
        assert_ne!(
            live_ptype_request_key(
                LivePtypeModelSource::LocalWrf,
                LivePtypeSurfaceMode::ModelOnly,
                Some(Path::new("wrfout_d01")),
                early,
            ),
            live_ptype_request_key(
                LivePtypeModelSource::LocalWrf,
                LivePtypeSurfaceMode::ModelOnly,
                Some(Path::new("wrfout_d01")),
                late,
            )
        );
    }

    #[test]
    fn archive_age_uses_the_displayed_frame_not_wall_clock() {
        let model_valid = Utc.with_ymd_and_hms(2026, 1, 25, 15, 0, 0).unwrap();
        let displayed = Utc.with_ymd_and_hms(2026, 1, 25, 15, 8, 48).unwrap();
        assert_eq!(age_fade(displayed, model_valid), 1.0);
        assert!((age_fade(Utc::now(), model_valid) - 0.30).abs() < f32::EPSILON);
        assert!(!rtma_is_operationally_available(model_valid, Utc::now()));
    }

    #[test]
    fn persistence_accepts_old_unknown_json_and_never_writes_local_paths() {
        let old = serde_json::json!({
            "enabled": true,
            "source": "Rap",
            "future_field": { "ignored": true }
        });
        let mut state = LivePtypeState::from_persisted(Some(&old));
        assert!(state.enabled);
        assert_eq!(state.source, LivePtypeModelSource::Rap);
        state.local_wrf_path = Some(PathBuf::from(r"C:\private\run\wrfout_d01"));
        let stored = state.persisted_value();
        assert!(stored.get("local_wrf_path").is_none());
        assert!(!stored.to_string().contains("private"));
    }
}
