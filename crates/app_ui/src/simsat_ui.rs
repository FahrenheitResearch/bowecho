//! Embedded SimSat pane and serialized render worker.
//!
//! SimSat's public render API is deliberately synchronous. This module keeps it off
//! the egui thread, admits only one job at a time, and reports cancellation honestly:
//! downloads may stop at a chunk boundary, while an in-flight render always finishes
//! its current frame before the worker checks the cancel flag again. Every finished
//! product enters the shared satellite store so Satellite remains the one playback and
//! map-display path. Raw thermal/derived scalars and rendered RGBA retain their full
//! per-pixel mesh for the native plot window.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};

use chrono::Utc;
use eframe::egui;
use simsat::api::{
    BlueMarble, FractionalCloudMode, FrameData, Product, RenderBackend, RenderIntent, RenderParams,
    RenderResult, SunOverride,
};
use simsat::bricks::StorageProfile;
use simsat::camera::{GeoNavigation, ResolutionMode, SatellitePreset, ViewMode};
use simsat::clouds::{CloudMultiscatterMode, StepQuality};
use simsat::derived::DerivedField;
use simsat::instrument_footprint::InstrumentFootprint;
use simsat::optics::CloudOpticsMode;
use simsat::render::{
    CLOUD_SOFTCLIP_KNEE, DEFAULT_EXPOSURE, GROUND_DAY_LIFT, LandAppearanceConfig, RHO_HIGHLIGHT_MAX,
};
use simsat::store_out::{self, IrFrame, VisibleFrame};
use simsat::thermal_sensor::ThermalSensor;
use simsat::wv::WvBand;

use crate::sat_plot::{SatellitePlotPalette, SatellitePlotSource};
use crate::simsat_hrrr::{HrrrNativeSpec, discover_native_files, download_native, latest_specs};
use crate::simsat_store::{DerivedFrame, write_derived_frame};

// SimSat v0.1.9 moved the compact brick schema from v5 to v6. A versioned
// directory keeps old cached-only manifests from masquerading as current inputs
// while raw WRF/GRIB sources re-ingest normally. The engine adds a further
// disjoint namespace for its optional ScienceCloudF16 v7 profile.
const ENGINE_CACHE_SUBDIR: &str = "engine-v6";

/// A completion request owned by the app shell. The pane never reaches into the
/// Satellite or native-plot viewer state directly.
pub(crate) enum SimSatAction {
    /// A frame is durable in the normal satellite store. The shell refreshes the
    /// satellite worker, selects this run/time, and opens the Satellite viewer.
    SatelliteFrameWritten { key: rw_ui::SatRunKey, hhmm: u16 },
    /// Open the latest retained SimSat output in the native mesh plot window.
    OpenPlot(SatellitePlotSource),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SourceMode {
    Local,
    CachedHrrr,
    DownloadHrrr,
}

impl SourceMode {
    const ALL: [Self; 3] = [Self::Local, Self::CachedHrrr, Self::DownloadHrrr];

    fn label(self) -> &'static str {
        match self {
            Self::Local => "Local WRF / GRIB",
            Self::CachedHrrr => "Downloaded HRRR",
            Self::DownloadHrrr => "Download HRRR",
        }
    }
}

/// User-facing product list. Keeping this app-side enum makes routing and stable
/// store tokens explicit instead of depending on debug formatting of SimSat enums.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SimSatProduct {
    Visible,
    GeoColor,
    Sandwich,
    Ir13,
    Wv8,
    Wv9,
    Wv10,
    PrecipitableWater,
    CloudTopTemperature,
    CloudOpticalDepth,
}

impl SimSatProduct {
    const ALL: [Self; 10] = [
        Self::Visible,
        Self::GeoColor,
        Self::Sandwich,
        Self::Ir13,
        Self::Wv8,
        Self::Wv9,
        Self::Wv10,
        Self::PrecipitableWater,
        Self::CloudTopTemperature,
        Self::CloudOpticalDepth,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Visible => "Visible true color",
            Self::GeoColor => "SimSat day / night color (GeoColor style)",
            Self::Sandwich => "Sandwich (visible + cold-top IR)",
            Self::Ir13 => "IR 10.3 um (band 13)",
            Self::Wv8 => "Water vapor 6.2 um (band 8)",
            Self::Wv9 => "Water vapor 6.9 um (band 9)",
            Self::Wv10 => "Water vapor 7.3 um (band 10)",
            Self::PrecipitableWater => "Precipitable water",
            Self::CloudTopTemperature => "Cloud-top temperature",
            Self::CloudOpticalDepth => "Cloud optical depth",
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Self::Visible => "visible",
            Self::GeoColor => "geocolor",
            Self::Sandwich => "sandwich",
            Self::Ir13 => "ir13",
            Self::Wv8 => "wv08",
            Self::Wv9 => "wv09",
            Self::Wv10 => "wv10",
            Self::PrecipitableWater => "pw",
            Self::CloudTopTemperature => "ctt",
            Self::CloudOpticalDepth => "cod",
        }
    }

    fn api_product(self) -> Product {
        match self {
            Self::Visible => Product::VisibleRgb,
            Self::GeoColor => Product::GeoColor,
            Self::Sandwich => Product::Sandwich,
            Self::Ir13 => Product::Ir,
            Self::Wv8 => Product::WaterVapor {
                band: WvBand::Upper,
            },
            Self::Wv9 => Product::WaterVapor { band: WvBand::Mid },
            Self::Wv10 => Product::WaterVapor { band: WvBand::Low },
            Self::PrecipitableWater => Product::Derived {
                field: DerivedField::PrecipitableWater,
            },
            Self::CloudTopTemperature => Product::Derived {
                field: DerivedField::CloudTopTemp,
            },
            Self::CloudOpticalDepth => Product::Derived {
                field: DerivedField::CloudOpticalDepth,
            },
        }
    }

    fn thermal_band(self) -> Option<u8> {
        match self {
            Self::Ir13 => Some(13),
            Self::Wv8 => Some(8),
            Self::Wv9 => Some(9),
            Self::Wv10 => Some(10),
            _ => None,
        }
    }

    fn uses_visible_ground(self) -> bool {
        matches!(self, Self::Visible | Self::GeoColor | Self::Sandwich)
    }

    fn is_visible_family(self) -> bool {
        self.uses_visible_ground()
    }

    fn has_band13_component(self) -> bool {
        matches!(self, Self::Ir13 | Self::GeoColor | Self::Sandwich)
    }

    fn supports_native_cloud_optics(self) -> bool {
        self == Self::Visible
    }

    fn supports_sensor_qa(self) -> bool {
        matches!(self, Self::Visible | Self::Ir13)
    }

    fn derived_field(self) -> Option<DerivedField> {
        match self {
            Self::PrecipitableWater => Some(DerivedField::PrecipitableWater),
            Self::CloudTopTemperature => Some(DerivedField::CloudTopTemp),
            Self::CloudOpticalDepth => Some(DerivedField::CloudOpticalDepth),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutputView {
    Geostationary,
    TopDown,
}

impl OutputView {
    const ALL: [Self; 2] = [Self::Geostationary, Self::TopDown];

    fn label(self) -> &'static str {
        match self {
            Self::Geostationary => "Geostationary (from space)",
            Self::TopDown => "Top-down map",
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Self::Geostationary => "geo",
            Self::TopDown => "topdown",
        }
    }

    fn api_view(self) -> ViewMode {
        match self {
            Self::Geostationary => ViewMode::Geostationary,
            Self::TopDown => ViewMode::TopDownMap,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SatelliteChoice {
    GoesEast,
    GoesWest,
    Himawari,
}

impl SatelliteChoice {
    const ALL: [Self; 3] = [Self::GoesEast, Self::GoesWest, Self::Himawari];

    fn label(self) -> &'static str {
        match self {
            Self::GoesEast => "GOES-East",
            Self::GoesWest => "GOES-West",
            Self::Himawari => "Himawari",
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Self::GoesEast => "goes-east",
            Self::GoesWest => "goes-west",
            Self::Himawari => "himawari",
        }
    }

    fn api_satellite(self) -> SatellitePreset {
        match self {
            Self::GoesEast => SatellitePreset::GoesEast,
            Self::GoesWest => SatellitePreset::GoesWest,
            Self::Himawari => SatellitePreset::Himawari,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RenderQuality {
    Final,
    Preview,
}

impl RenderQuality {
    fn label(self) -> &'static str {
        match self {
            Self::Final => "Final (384 steps)",
            Self::Preview => "Preview (192 steps)",
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Self::Final => "offline",
            Self::Preview => "interactive",
        }
    }

    fn steps(self) -> StepQuality {
        match self {
            Self::Final => StepQuality::Offline,
            Self::Preview => StepQuality::Interactive,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SimSatQuickMode {
    RecommendedDisplay,
    HighQualityVisible,
    SensorQa,
}

impl SimSatQuickMode {
    const ALL: [Self; 3] = [
        Self::RecommendedDisplay,
        Self::HighQualityVisible,
        Self::SensorQa,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::RecommendedDisplay => "Recommended",
            Self::HighQualityVisible => "High Quality",
            Self::SensorQa => "Sensor QA",
        }
    }

    fn full_label(self) -> &'static str {
        match self {
            Self::RecommendedDisplay => "Recommended Display",
            Self::HighQualityVisible => "High Quality Visible",
            Self::SensorQa => "Sensor QA",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::RecommendedDisplay => {
                "Owner-reviewed visible defaults: CPU-quality rendering, model-native output, \
                 OD 0.15, exposure 1.5, AOD 0.05, fixed optics, Effective OD, corrections and \
                 edge feathering on, and experimental storage/footprints off."
            }
            Self::HighQualityVisible => {
                "Recommended Display plus the deterministic four-subcolumn cloud reference and \
                 a 0.45 highlight knee. It is slower, but does not silently enable experimental \
                 storage, sensor footprint, or scheme-native particle optics."
            }
            Self::SensorQa => {
                "CPU sensor-comparison setup: exact GOES-R fixed-grid navigation and neutral \
                 visible transforms, or the official FM4/GOES-19 Band 13 response for a \
                 compatible GOES-East IR selection."
            }
        }
    }
}

const CLOUD_TRANSPORTS: [CloudMultiscatterMode; 5] = [
    CloudMultiscatterMode::LegacyOctaves,
    CloudMultiscatterMode::SingleScatter,
    CloudMultiscatterMode::DeltaFluxV1,
    CloudMultiscatterMode::DeltaFluxV2,
    CloudMultiscatterMode::DeltaFluxV3,
];

fn cloud_transport_label(mode: CloudMultiscatterMode) -> &'static str {
    match mode {
        CloudMultiscatterMode::LegacyOctaves => "Legacy octaves (shipped)",
        CloudMultiscatterMode::SingleScatter => "Single scatter",
        CloudMultiscatterMode::DeltaFluxV1 => "Delta-flux v1 (experimental)",
        CloudMultiscatterMode::DeltaFluxV2 => "Delta-flux v2b P1 (experimental)",
        CloudMultiscatterMode::DeltaFluxV3 => "Delta-flux v3 memory (experimental)",
    }
}

fn fractional_cloud_mode_label(mode: FractionalCloudMode) -> &'static str {
    match mode {
        FractionalCloudMode::Off => "Off",
        FractionalCloudMode::EffectiveOd => "Effective OD (fast / default)",
        FractionalCloudMode::Deterministic4 => "Deterministic 4 (reference)",
        FractionalCloudMode::Deterministic8 => "Deterministic 8 (convergence)",
        FractionalCloudMode::Deterministic16 => "Deterministic 16 (convergence)",
    }
}

fn cloud_optics_label(mode: CloudOpticsMode) -> &'static str {
    match mode {
        CloudOpticsMode::Fixed => "Fixed radii (production default)",
        CloudOpticsMode::NsslNative => "NSSL MP18 native moments (experimental)",
        CloudOpticsMode::HrrrThompsonNative => "HRRR Thompson native moments (experimental)",
    }
}

fn resolution_short_label(mode: ResolutionMode) -> &'static str {
    match mode {
        ResolutionMode::Native => "Model native",
        ResolutionMode::Abi1km => "ABI 1 km",
        ResolutionMode::Abi2km => "ABI 2 km",
    }
}

fn resolution_slug(mode: ResolutionMode) -> &'static str {
    match mode {
        ResolutionMode::Native => "native",
        ResolutionMode::Abi1km => "abi1km",
        ResolutionMode::Abi2km => "abi2km",
    }
}

fn resolution_from_slug(value: &str) -> Option<ResolutionMode> {
    match value {
        "native" => Some(ResolutionMode::Native),
        "abi1km" | "abi-1km" => Some(ResolutionMode::Abi1km),
        "abi2km" | "abi-2km" => Some(ResolutionMode::Abi2km),
        _ => None,
    }
}

fn near_f32(left: f32, right: f32) -> bool {
    (left - right).abs() <= 1.0e-5
}

#[derive(Clone, Debug)]
enum JobSource {
    Local(PathBuf),
    CachedHrrr {
        path: PathBuf,
        spec: Option<HrrrNativeSpec>,
    },
    DownloadHrrr {
        spec: HrrrNativeSpec,
        root: PathBuf,
    },
}

impl JobSource {
    fn group_base(&self) -> String {
        match self {
            Self::Local(path) => source_group_base(path),
            Self::CachedHrrr {
                spec: Some(spec), ..
            } => format!("hrrr_{}_t{:02}z", spec.date, spec.cycle),
            Self::CachedHrrr {
                path, spec: None, ..
            } => source_group_base(path),
            Self::DownloadHrrr { spec, .. } => {
                format!("hrrr_{}_t{:02}z", spec.date, spec.cycle)
            }
        }
    }
}

#[derive(Clone, Debug)]
struct RenderJob {
    source: JobSource,
    backend: RenderBackend,
    storage_profile: StorageProfile,
    render_intent: RenderIntent,
    product: SimSatProduct,
    view: OutputView,
    satellite: SatelliteChoice,
    geo_navigation: GeoNavigation,
    resolution: ResolutionMode,
    quality: RenderQuality,
    margin_frac: f32,
    granulation: bool,
    bluemarble_download: bool,
    bluemarble_month: u32,
    exposure: f32,
    ground_gain: f32,
    cloud_softclip: f32,
    cloud_highlight_max: f32,
    aerosol_optical_depth: f32,
    rh_aerosol_swelling: bool,
    atmosphere_correction: bool,
    terrain_atmosphere: bool,
    land_sza_normalization: bool,
    land_sza_max_gain: f32,
    land_dark_toe: bool,
    land_dark_toe_knee: f32,
    land_dark_toe_gamma: f32,
    land_dark_toe_max_gain: f32,
    clouds: bool,
    fractional_clouds: bool,
    fractional_cloud_mode: FractionalCloudMode,
    cloud_optical_depth_scale: f32,
    cloud_optics: CloudOpticsMode,
    feather_exposed_domain_edges: bool,
    cloud_transport: CloudMultiscatterMode,
    beer_powder: bool,
    topdown_stratiform_regularization: bool,
    topdown_cloud_footprint: bool,
    thermal_sensor: ThermalSensor,
    instrument_footprint: InstrumentFootprint,
    sun_override: bool,
    sun_elevation_deg: f32,
    sun_azimuth_deg: f32,
    cache_root: PathBuf,
    store_root: PathBuf,
    sector: String,
}

#[derive(Clone, Debug)]
struct FrameInput {
    path: PathBuf,
    timestep: usize,
    sort_key: String,
    label: String,
}

#[derive(Clone, Debug)]
struct StoredFrame {
    key: rw_ui::SatRunKey,
    hhmm: u16,
}

#[derive(Clone, Debug, PartialEq)]
enum PlotPixels {
    Scalar {
        values: Vec<f32>,
        units: String,
        palette: Option<SatellitePlotPalette>,
    },
    Rgba(Vec<u8>),
}

/// Worker-safe, constructor-neutral plot data. `SatellitePlotSource` is created on
/// the UI thread only when the user opens it.
#[derive(Clone, Debug, PartialEq)]
struct PlotPayload {
    title: String,
    subtitle_left: String,
    subtitle_right: String,
    nx: usize,
    ny: usize,
    lat: Vec<f32>,
    lon: Vec<f32>,
    pixels: PlotPixels,
}

impl PlotPayload {
    fn to_plot_source(&self) -> Result<SatellitePlotSource, String> {
        match &self.pixels {
            PlotPixels::Scalar {
                values,
                units,
                palette,
            } => {
                let subtitle_right = if units.is_empty() {
                    self.subtitle_right.clone()
                } else {
                    format!("{} · {units}", self.subtitle_right)
                };
                SatellitePlotSource::scalar_from_mesh_with_palette(
                    self.title.clone(),
                    self.subtitle_left.clone(),
                    subtitle_right,
                    units.clone(),
                    self.nx,
                    self.ny,
                    values.clone(),
                    self.lat.clone(),
                    self.lon.clone(),
                    None,
                    palette.clone(),
                )
            }
            PlotPixels::Rgba(rgba) => {
                let pixels_len = self
                    .nx
                    .checked_mul(self.ny)
                    .ok_or_else(|| "SimSat RGBA dimensions overflow.".to_owned())?;
                let bytes_len = pixels_len
                    .checked_mul(4)
                    .ok_or_else(|| "SimSat RGBA byte count overflows.".to_owned())?;
                if rgba.len() != bytes_len {
                    return Err(format!(
                        "SimSat RGBA has {} bytes, expected {bytes_len}",
                        rgba.len()
                    ));
                }
                let pixels = rgba
                    .chunks_exact(4)
                    .map(|px| rustwx_render::Color::rgba(px[0], px[1], px[2], px[3]))
                    .collect();
                SatellitePlotSource::rgba_from_mesh(
                    self.title.clone(),
                    self.subtitle_left.clone(),
                    self.subtitle_right.clone(),
                    self.nx,
                    self.ny,
                    pixels,
                    self.lat.clone(),
                    self.lon.clone(),
                    None,
                )
            }
        }
    }
}

enum WorkerEvent {
    Started {
        total: usize,
    },
    Progress {
        index: usize,
        total: usize,
        message: String,
    },
    FrameComplete {
        label: String,
        plot: Box<PlotPayload>,
        stored: Option<StoredFrame>,
        store_error: Option<String>,
        warning: Option<String>,
    },
    FrameFailed {
        label: String,
        error: String,
    },
    Finished {
        completed: usize,
        failed: usize,
        cancelled: bool,
    },
    Fatal(String),
}

struct RenderTask {
    rx: Receiver<WorkerEvent>,
    cancel: Arc<AtomicBool>,
}

const SIMSAT_STATE_SCHEMA: u32 = 1;

/// Versioned opaque payload stored inside BowEcho's application settings. Source
/// paths, cached-file selections, live jobs, progress, and rendered output stay
/// session-only; this snapshot contains only reusable render/control choices.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(default)]
struct SimSatSavedState {
    schema: u32,
    product: String,
    view: String,
    satellite: String,
    geo_navigation: String,
    resolution: String,
    quality: String,
    storage_profile: String,
    render_intent: String,
    margin_frac: f32,
    granulation: bool,
    bluemarble_download: bool,
    bluemarble_month: u32,
    exposure: f32,
    ground_gain: f32,
    cloud_softclip: f32,
    cloud_highlight_max: f32,
    aerosol_optical_depth: f32,
    rh_aerosol_swelling: bool,
    atmosphere_correction: bool,
    terrain_atmosphere: bool,
    land_sza_normalization: bool,
    land_sza_max_gain: f32,
    land_dark_toe: bool,
    land_dark_toe_knee: f32,
    land_dark_toe_gamma: f32,
    land_dark_toe_max_gain: f32,
    clouds: bool,
    fractional_clouds: bool,
    fractional_cloud_mode: String,
    cloud_optical_depth_scale: f32,
    cloud_optics: String,
    feather_exposed_domain_edges: bool,
    cloud_transport: String,
    beer_powder: bool,
    topdown_stratiform_regularization: bool,
    topdown_cloud_footprint: bool,
    thermal_sensor: String,
    instrument_footprint: String,
    sun_override: bool,
    sun_elevation_deg: f32,
    sun_azimuth_deg: f32,
}

impl Default for SimSatSavedState {
    fn default() -> Self {
        Self::from_pane(&SimSatPane::default())
    }
}

impl SimSatSavedState {
    fn from_pane(pane: &SimSatPane) -> Self {
        Self {
            schema: SIMSAT_STATE_SCHEMA,
            product: pane.product.slug().to_owned(),
            view: pane.view.slug().to_owned(),
            satellite: pane.satellite.slug().to_owned(),
            geo_navigation: pane.geo_navigation.slug().to_owned(),
            resolution: resolution_slug(pane.resolution).to_owned(),
            quality: pane.quality.slug().to_owned(),
            storage_profile: pane.storage_profile.slug().to_owned(),
            render_intent: pane.render_intent.slug().to_owned(),
            margin_frac: pane.margin_frac,
            granulation: pane.granulation,
            bluemarble_download: pane.bluemarble_download,
            bluemarble_month: pane.bluemarble_month,
            exposure: pane.exposure,
            ground_gain: pane.ground_gain,
            cloud_softclip: pane.cloud_softclip,
            cloud_highlight_max: pane.cloud_highlight_max,
            aerosol_optical_depth: pane.aerosol_optical_depth,
            rh_aerosol_swelling: pane.rh_aerosol_swelling,
            atmosphere_correction: pane.atmosphere_correction,
            terrain_atmosphere: pane.terrain_atmosphere,
            land_sza_normalization: pane.land_sza_normalization,
            land_sza_max_gain: pane.land_sza_max_gain,
            land_dark_toe: pane.land_dark_toe,
            land_dark_toe_knee: pane.land_dark_toe_knee,
            land_dark_toe_gamma: pane.land_dark_toe_gamma,
            land_dark_toe_max_gain: pane.land_dark_toe_max_gain,
            clouds: pane.clouds,
            fractional_clouds: pane.fractional_clouds,
            fractional_cloud_mode: pane.fractional_cloud_mode.slug().to_owned(),
            cloud_optical_depth_scale: pane.cloud_optical_depth_scale,
            cloud_optics: pane.cloud_optics.token().to_owned(),
            feather_exposed_domain_edges: pane.feather_exposed_domain_edges,
            cloud_transport: pane.cloud_transport.slug().to_owned(),
            beer_powder: pane.beer_powder,
            topdown_stratiform_regularization: pane.topdown_stratiform_regularization,
            topdown_cloud_footprint: pane.topdown_cloud_footprint,
            thermal_sensor: pane.thermal_sensor.slug().to_owned(),
            instrument_footprint: pane.instrument_footprint.slug().to_owned(),
            sun_override: pane.sun_override,
            sun_elevation_deg: pane.sun_elevation_deg,
            sun_azimuth_deg: pane.sun_azimuth_deg,
        }
    }

    fn apply_to(&self, pane: &mut SimSatPane) {
        if let Some(product) = SimSatProduct::ALL
            .into_iter()
            .find(|candidate| candidate.slug() == self.product)
        {
            pane.product = product;
        }
        if let Some(view) = OutputView::ALL
            .into_iter()
            .find(|candidate| candidate.slug() == self.view)
        {
            pane.view = view;
        }
        if let Some(satellite) = SatelliteChoice::ALL
            .into_iter()
            .find(|candidate| candidate.slug() == self.satellite)
        {
            pane.satellite = satellite;
        }
        if let Some(navigation) = GeoNavigation::ALL
            .into_iter()
            .find(|candidate| candidate.slug() == self.geo_navigation)
        {
            pane.geo_navigation = navigation;
        }
        if let Some(resolution) = resolution_from_slug(&self.resolution) {
            pane.resolution = resolution;
        }
        if let Some(quality) = [RenderQuality::Final, RenderQuality::Preview]
            .into_iter()
            .find(|candidate| candidate.slug() == self.quality)
        {
            pane.quality = quality;
        }
        if let Some(profile) = StorageProfile::parse(&self.storage_profile) {
            pane.storage_profile = profile;
        }
        pane.render_intent = match self.render_intent.as_str() {
            "sensor-fast-gray" => RenderIntent::SensorFastGray,
            _ => RenderIntent::Display,
        };
        pane.margin_frac = self.margin_frac.clamp(0.0, 0.75);
        pane.granulation = self.granulation;
        pane.bluemarble_download = self.bluemarble_download;
        pane.bluemarble_month = self.bluemarble_month.min(12);
        pane.exposure = self.exposure.clamp(0.25, 4.0);
        pane.ground_gain = self.ground_gain.clamp(0.25, 4.0);
        pane.cloud_softclip = self.cloud_softclip.clamp(0.05, 1.0);
        pane.cloud_highlight_max = self.cloud_highlight_max.clamp(0.25, 4.0);
        pane.aerosol_optical_depth = self.aerosol_optical_depth.clamp(0.0, 0.6);
        pane.rh_aerosol_swelling = self.rh_aerosol_swelling;
        pane.atmosphere_correction = self.atmosphere_correction;
        pane.terrain_atmosphere = self.terrain_atmosphere;
        pane.land_sza_normalization = self.land_sza_normalization;
        pane.land_sza_max_gain = self.land_sza_max_gain.clamp(1.0, 4.0);
        pane.land_dark_toe = self.land_dark_toe;
        pane.land_dark_toe_knee = self.land_dark_toe_knee.clamp(0.001, 0.25);
        pane.land_dark_toe_gamma = self.land_dark_toe_gamma.clamp(0.05, 1.0);
        pane.land_dark_toe_max_gain = self.land_dark_toe_max_gain.clamp(1.0, 4.0);
        pane.clouds = self.clouds;
        pane.fractional_clouds = self.fractional_clouds;
        pane.fractional_cloud_mode = match self.fractional_cloud_mode.as_str() {
            "off" => FractionalCloudMode::Off,
            "deterministic-4" => FractionalCloudMode::Deterministic4,
            "deterministic-8" => FractionalCloudMode::Deterministic8,
            "deterministic-16" => FractionalCloudMode::Deterministic16,
            _ => FractionalCloudMode::EffectiveOd,
        };
        pane.cloud_optical_depth_scale = self.cloud_optical_depth_scale.clamp(0.0, 4.0);
        if let Some(optics) = CloudOpticsMode::parse(&self.cloud_optics) {
            pane.cloud_optics = optics;
        }
        pane.feather_exposed_domain_edges = self.feather_exposed_domain_edges;
        if let Some(transport) = CLOUD_TRANSPORTS
            .into_iter()
            .find(|candidate| candidate.slug() == self.cloud_transport)
        {
            pane.cloud_transport = transport;
        }
        pane.beer_powder = self.beer_powder;
        pane.topdown_stratiform_regularization = self.topdown_stratiform_regularization;
        pane.topdown_cloud_footprint = self.topdown_cloud_footprint;
        if let Some(sensor) = ThermalSensor::parse(&self.thermal_sensor) {
            pane.thermal_sensor = sensor;
        }
        if let Some(footprint) = InstrumentFootprint::parse(&self.instrument_footprint) {
            pane.instrument_footprint = footprint;
        }
        pane.sun_override = self.sun_override;
        pane.sun_elevation_deg = self.sun_elevation_deg.clamp(-10.0, 90.0);
        pane.sun_azimuth_deg = self.sun_azimuth_deg.clamp(0.0, 360.0);
        pane.normalize_incompatible_controls();
    }
}

/// State for the docked/floating SimSat pane. It owns no Satellite or plot viewer
/// internals; all cross-window effects leave as [`SimSatAction`].
pub(crate) struct SimSatPane {
    source_mode: SourceMode,
    local_path: String,
    cached_hrrr: Vec<crate::simsat_hrrr::CachedNativeInput>,
    cached_selected: usize,
    cached_loaded: bool,
    hrrr_date: String,
    hrrr_cycle: u8,
    hrrr_forecast_hour: u16,
    product: SimSatProduct,
    view: OutputView,
    satellite: SatelliteChoice,
    geo_navigation: GeoNavigation,
    resolution: ResolutionMode,
    quality: RenderQuality,
    storage_profile: StorageProfile,
    render_intent: RenderIntent,
    margin_frac: f32,
    granulation: bool,
    bluemarble_download: bool,
    bluemarble_month: u32,
    exposure: f32,
    ground_gain: f32,
    cloud_softclip: f32,
    cloud_highlight_max: f32,
    aerosol_optical_depth: f32,
    rh_aerosol_swelling: bool,
    atmosphere_correction: bool,
    terrain_atmosphere: bool,
    land_sza_normalization: bool,
    land_sza_max_gain: f32,
    land_dark_toe: bool,
    land_dark_toe_knee: f32,
    land_dark_toe_gamma: f32,
    land_dark_toe_max_gain: f32,
    clouds: bool,
    fractional_clouds: bool,
    fractional_cloud_mode: FractionalCloudMode,
    cloud_optical_depth_scale: f32,
    cloud_optics: CloudOpticsMode,
    feather_exposed_domain_edges: bool,
    cloud_transport: CloudMultiscatterMode,
    beer_powder: bool,
    topdown_stratiform_regularization: bool,
    topdown_cloud_footprint: bool,
    thermal_sensor: ThermalSensor,
    instrument_footprint: InstrumentFootprint,
    sun_override: bool,
    sun_elevation_deg: f32,
    sun_azimuth_deg: f32,
    task: Option<RenderTask>,
    status: String,
    error: Option<String>,
    total: usize,
    completed: usize,
    failed: usize,
    cancellation_requested: bool,
    last_notice: Option<String>,
    last_plot: Option<PlotPayload>,
    last_plot_label: Option<String>,
    last_stored: Option<StoredFrame>,
    persisted_state_dirty: bool,
}

impl Default for SimSatPane {
    fn default() -> Self {
        let now = Utc::now();
        let fallback_date = now.format("%Y%m%d").to_string();
        let fallback_cycle = now.format("%H").to_string().parse().unwrap_or(0);
        let latest = latest_specs(now, 0).into_iter().next();
        let (date, cycle) = latest
            .map(|spec| (spec.date, spec.cycle))
            .unwrap_or((fallback_date, fallback_cycle));
        let land = LandAppearanceConfig::default();
        Self {
            source_mode: SourceMode::Local,
            local_path: String::new(),
            cached_hrrr: Vec::new(),
            cached_selected: 0,
            cached_loaded: false,
            hrrr_date: date,
            hrrr_cycle: cycle,
            hrrr_forecast_hour: 0,
            product: SimSatProduct::Visible,
            view: OutputView::Geostationary,
            satellite: SatelliteChoice::GoesEast,
            geo_navigation: GeoNavigation::ModelSphere,
            resolution: ResolutionMode::Native,
            quality: RenderQuality::Final,
            storage_profile: StorageProfile::CompactU8,
            render_intent: RenderIntent::Display,
            margin_frac: 0.0,
            granulation: false,
            bluemarble_download: true,
            bluemarble_month: 0,
            exposure: DEFAULT_EXPOSURE as f32,
            ground_gain: GROUND_DAY_LIFT as f32,
            cloud_softclip: CLOUD_SOFTCLIP_KNEE as f32,
            cloud_highlight_max: RHO_HIGHLIGHT_MAX as f32,
            aerosol_optical_depth: simsat::atmosphere::DEFAULT_AOD as f32,
            rh_aerosol_swelling: false,
            atmosphere_correction: true,
            terrain_atmosphere: true,
            land_sza_normalization: land.sza_normalization,
            land_sza_max_gain: land.sza_max_gain as f32,
            land_dark_toe: land.dark_toe,
            land_dark_toe_knee: land.dark_toe_knee as f32,
            land_dark_toe_gamma: land.dark_toe_gamma as f32,
            land_dark_toe_max_gain: land.dark_toe_max_gain as f32,
            clouds: true,
            fractional_clouds: true,
            fractional_cloud_mode: FractionalCloudMode::EffectiveOd,
            cloud_optical_depth_scale: simsat::clouds::DEFAULT_CLOUD_OPTICAL_DEPTH_SCALE,
            cloud_optics: CloudOpticsMode::Fixed,
            feather_exposed_domain_edges: true,
            cloud_transport: CloudMultiscatterMode::LegacyOctaves,
            beer_powder: false,
            topdown_stratiform_regularization: false,
            topdown_cloud_footprint: false,
            thermal_sensor: ThermalSensor::FastGray,
            instrument_footprint: InstrumentFootprint::Off,
            sun_override: false,
            sun_elevation_deg: 45.0,
            sun_azimuth_deg: 180.0,
            task: None,
            status: "Choose a WRF/GRIB source or an HRRR native-level file.".to_owned(),
            error: None,
            total: 0,
            completed: 0,
            failed: 0,
            cancellation_requested: false,
            last_notice: None,
            last_plot: None,
            last_plot_label: None,
            last_stored: None,
            persisted_state_dirty: false,
        }
    }
}

impl SimSatPane {
    pub(crate) fn new(saved: Option<&serde_json::Value>) -> Self {
        let mut pane = Self::default();
        if let Some(saved) = saved
            && let Ok(state) = serde_json::from_value::<SimSatSavedState>(saved.clone())
            && state.schema == SIMSAT_STATE_SCHEMA
        {
            state.apply_to(&mut pane);
        }
        pane.persisted_state_dirty = false;
        pane
    }

    pub(crate) fn take_persisted_state_if_dirty(&mut self) -> Option<serde_json::Value> {
        if !self.persisted_state_dirty {
            return None;
        }
        let value = serde_json::to_value(SimSatSavedState::from_pane(self)).ok()?;
        self.persisted_state_dirty = false;
        Some(value)
    }

    fn normalize_incompatible_controls(&mut self) {
        if !self.product.supports_native_cloud_optics() {
            self.cloud_optics = CloudOpticsMode::Fixed;
        }
        if !self.product.has_band13_component() {
            self.instrument_footprint = InstrumentFootprint::Off;
        }
        if self.satellite == SatelliteChoice::Himawari
            && self.geo_navigation == GeoNavigation::GoesRAbiFixedGrid
        {
            self.geo_navigation = GeoNavigation::ModelSphere;
        }
        if self.instrument_footprint != InstrumentFootprint::Off {
            if self.satellite == SatelliteChoice::Himawari {
                self.satellite = SatelliteChoice::GoesEast;
            }
            self.view = OutputView::Geostationary;
            self.geo_navigation = GeoNavigation::GoesRAbiFixedGrid;
            self.resolution = ResolutionMode::Abi2km;
            self.thermal_sensor = ThermalSensor::GoesRAbiBand13Fm4;
        }
    }

    fn apply_display_baseline(&mut self) {
        let land = LandAppearanceConfig::default();
        self.storage_profile = StorageProfile::CompactU8;
        self.instrument_footprint = InstrumentFootprint::Off;
        self.resolution = ResolutionMode::Native;
        self.render_intent = RenderIntent::Display;
        self.quality = RenderQuality::Final;
        self.aerosol_optical_depth = simsat::atmosphere::DEFAULT_AOD as f32;
        self.rh_aerosol_swelling = false;
        self.atmosphere_correction = true;
        self.terrain_atmosphere = true;
        self.land_sza_normalization = land.sza_normalization;
        self.land_sza_max_gain = land.sza_max_gain as f32;
        self.land_dark_toe = land.dark_toe;
        self.land_dark_toe_knee = land.dark_toe_knee as f32;
        self.land_dark_toe_gamma = land.dark_toe_gamma as f32;
        self.land_dark_toe_max_gain = land.dark_toe_max_gain as f32;
        self.clouds = true;
        self.fractional_clouds = true;
        self.fractional_cloud_mode = FractionalCloudMode::EffectiveOd;
        self.cloud_optical_depth_scale = simsat::clouds::DEFAULT_CLOUD_OPTICAL_DEPTH_SCALE;
        self.cloud_optics = CloudOpticsMode::Fixed;
        self.feather_exposed_domain_edges = true;
        self.cloud_transport = CloudMultiscatterMode::LegacyOctaves;
        self.beer_powder = false;
        self.granulation = false;
        self.topdown_stratiform_regularization = false;
        self.topdown_cloud_footprint = false;
        self.exposure = DEFAULT_EXPOSURE as f32;
        self.ground_gain = GROUND_DAY_LIFT as f32;
        self.cloud_softclip = CLOUD_SOFTCLIP_KNEE as f32;
        self.cloud_highlight_max = RHO_HIGHLIGHT_MAX as f32;
    }

    fn apply_quick_mode(&mut self, mode: SimSatQuickMode) -> Result<(), String> {
        match mode {
            SimSatQuickMode::RecommendedDisplay | SimSatQuickMode::HighQualityVisible => {
                if !self.product.is_visible_family() {
                    return Err(format!(
                        "{} does not use the visible display path. Select Visible, SimSat day / night color, or Sandwich first; Quick mode never changes the product.",
                        self.product.label()
                    ));
                }
                self.apply_display_baseline();
                if mode == SimSatQuickMode::HighQualityVisible {
                    self.fractional_cloud_mode = FractionalCloudMode::Deterministic4;
                    self.cloud_softclip = 0.45;
                }
            }
            SimSatQuickMode::SensorQa => {
                if !self.product.supports_sensor_qa() {
                    return Err(format!(
                        "{} is not an honest Sensor QA target. Select Visible or IR Band 13; Quick mode never converts the product.",
                        self.product.label()
                    ));
                }
                if self.satellite == SatelliteChoice::Himawari {
                    return Err(
                        "GOES-R Sensor QA is incompatible with Himawari. Select a GOES satellite first; the preset will not relabel the platform.".to_owned(),
                    );
                }
                if self.product == SimSatProduct::Ir13
                    && self.satellite != SatelliteChoice::GoesEast
                {
                    return Err(
                        "The available official Band 13 response is FM4 / GOES-19 (GOES-East), not GOES-West. Select GOES-East first.".to_owned(),
                    );
                }
                self.storage_profile = StorageProfile::CompactU8;
                self.instrument_footprint = InstrumentFootprint::Off;
                self.view = OutputView::Geostationary;
                self.geo_navigation = GeoNavigation::GoesRAbiFixedGrid;
                self.render_intent = RenderIntent::SensorFastGray;
                self.quality = RenderQuality::Final;
                match self.product {
                    SimSatProduct::Visible => {
                        self.apply_sensor_qa_visible();
                    }
                    SimSatProduct::Ir13 => {
                        self.resolution = ResolutionMode::Abi2km;
                        self.thermal_sensor = ThermalSensor::GoesRAbiBand13Fm4;
                    }
                    _ => unreachable!("Sensor QA scope validated above"),
                }
            }
        }
        self.persisted_state_dirty = true;
        Ok(())
    }

    fn apply_sensor_qa_visible(&mut self) {
        let land = LandAppearanceConfig::identity();
        self.resolution = ResolutionMode::Abi1km;
        self.aerosol_optical_depth = simsat::atmosphere::DEFAULT_AOD as f32;
        self.rh_aerosol_swelling = false;
        self.atmosphere_correction = false;
        self.terrain_atmosphere = true;
        self.land_sza_normalization = land.sza_normalization;
        self.land_sza_max_gain = land.sza_max_gain as f32;
        self.land_dark_toe = land.dark_toe;
        self.land_dark_toe_knee = land.dark_toe_knee as f32;
        self.land_dark_toe_gamma = land.dark_toe_gamma as f32;
        self.land_dark_toe_max_gain = land.dark_toe_max_gain as f32;
        self.clouds = true;
        self.fractional_clouds = true;
        self.fractional_cloud_mode = FractionalCloudMode::EffectiveOd;
        self.cloud_optical_depth_scale = 1.0;
        self.cloud_optics = CloudOpticsMode::Fixed;
        self.feather_exposed_domain_edges = false;
        self.cloud_transport = CloudMultiscatterMode::LegacyOctaves;
        self.beer_powder = false;
        self.granulation = false;
        self.topdown_stratiform_regularization = false;
        self.topdown_cloud_footprint = false;
        self.exposure = 1.0;
        self.ground_gain = 1.0;
        self.cloud_softclip = 1.0;
        self.cloud_highlight_max = 1.0;
    }

    fn active_quick_mode(&self) -> Option<SimSatQuickMode> {
        if self.sensor_qa_matches() {
            Some(SimSatQuickMode::SensorQa)
        } else if self.display_baseline_matches(true) {
            Some(SimSatQuickMode::HighQualityVisible)
        } else if self.display_baseline_matches(false) {
            Some(SimSatQuickMode::RecommendedDisplay)
        } else {
            None
        }
    }

    fn display_baseline_matches(&self, high_quality: bool) -> bool {
        let land = LandAppearanceConfig::default();
        self.product.is_visible_family()
            && self.storage_profile == StorageProfile::CompactU8
            && self.instrument_footprint == InstrumentFootprint::Off
            && self.resolution == ResolutionMode::Native
            && self.render_intent == RenderIntent::Display
            && self.quality == RenderQuality::Final
            && near_f32(
                self.aerosol_optical_depth,
                simsat::atmosphere::DEFAULT_AOD as f32,
            )
            && !self.rh_aerosol_swelling
            && self.atmosphere_correction
            && self.terrain_atmosphere
            && self.land_sza_normalization == land.sza_normalization
            && near_f32(self.land_sza_max_gain, land.sza_max_gain as f32)
            && self.land_dark_toe == land.dark_toe
            && near_f32(self.land_dark_toe_knee, land.dark_toe_knee as f32)
            && near_f32(self.land_dark_toe_gamma, land.dark_toe_gamma as f32)
            && near_f32(self.land_dark_toe_max_gain, land.dark_toe_max_gain as f32)
            && self.clouds
            && self.fractional_clouds
            && self.fractional_cloud_mode
                == if high_quality {
                    FractionalCloudMode::Deterministic4
                } else {
                    FractionalCloudMode::EffectiveOd
                }
            && near_f32(
                self.cloud_optical_depth_scale,
                simsat::clouds::DEFAULT_CLOUD_OPTICAL_DEPTH_SCALE,
            )
            && self.cloud_optics == CloudOpticsMode::Fixed
            && self.feather_exposed_domain_edges
            && self.cloud_transport == CloudMultiscatterMode::LegacyOctaves
            && !self.beer_powder
            && !self.granulation
            && !self.topdown_stratiform_regularization
            && !self.topdown_cloud_footprint
            && near_f32(self.exposure, DEFAULT_EXPOSURE as f32)
            && near_f32(self.ground_gain, GROUND_DAY_LIFT as f32)
            && near_f32(
                self.cloud_softclip,
                if high_quality {
                    0.45
                } else {
                    CLOUD_SOFTCLIP_KNEE as f32
                },
            )
            && near_f32(self.cloud_highlight_max, RHO_HIGHLIGHT_MAX as f32)
    }

    fn sensor_qa_matches(&self) -> bool {
        let common = self.product.supports_sensor_qa()
            && self.satellite != SatelliteChoice::Himawari
            && self.storage_profile == StorageProfile::CompactU8
            && self.instrument_footprint == InstrumentFootprint::Off
            && self.view == OutputView::Geostationary
            && self.geo_navigation == GeoNavigation::GoesRAbiFixedGrid
            && self.render_intent == RenderIntent::SensorFastGray
            && self.quality == RenderQuality::Final;
        if !common {
            return false;
        }
        match self.product {
            SimSatProduct::Visible => {
                let land = LandAppearanceConfig::identity();
                self.resolution == ResolutionMode::Abi1km
                    && near_f32(
                        self.aerosol_optical_depth,
                        simsat::atmosphere::DEFAULT_AOD as f32,
                    )
                    && !self.rh_aerosol_swelling
                    && !self.atmosphere_correction
                    && self.terrain_atmosphere
                    && self.land_sza_normalization == land.sza_normalization
                    && self.land_dark_toe == land.dark_toe
                    && self.clouds
                    && self.fractional_clouds
                    && self.fractional_cloud_mode == FractionalCloudMode::EffectiveOd
                    && near_f32(self.cloud_optical_depth_scale, 1.0)
                    && self.cloud_optics == CloudOpticsMode::Fixed
                    && !self.feather_exposed_domain_edges
                    && self.cloud_transport == CloudMultiscatterMode::LegacyOctaves
                    && !self.beer_powder
                    && !self.granulation
                    && !self.topdown_stratiform_regularization
                    && !self.topdown_cloud_footprint
                    && near_f32(self.exposure, 1.0)
                    && near_f32(self.ground_gain, 1.0)
                    && near_f32(self.cloud_softclip, 1.0)
                    && near_f32(self.cloud_highlight_max, 1.0)
            }
            SimSatProduct::Ir13 => {
                self.satellite == SatelliteChoice::GoesEast
                    && self.resolution == ResolutionMode::Abi2km
                    && self.thermal_sensor == ThermalSensor::GoesRAbiBand13Fm4
            }
            _ => false,
        }
    }

    pub(crate) fn is_busy(&self) -> bool {
        self.task.is_some()
    }

    /// Poll even while the pane is hidden so long renders/downloads finish and the
    /// Satellite store refresh is never gated on window visibility.
    pub(crate) fn poll(&mut self, ctx: &egui::Context) -> Vec<SimSatAction> {
        let mut events = Vec::new();
        let mut disconnected = false;
        if let Some(task) = &self.task {
            loop {
                match task.rx.try_recv() {
                    Ok(event) => events.push(event),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }

        let mut actions = Vec::new();
        let mut terminal = false;
        for event in events {
            match event {
                WorkerEvent::Started { total } => {
                    self.total = total;
                    self.status = format!("Discovered {total} frame(s). Starting render...");
                }
                WorkerEvent::Progress {
                    index,
                    total,
                    message,
                } => {
                    self.total = total;
                    self.status = format!("Frame {index}/{total}: {message}");
                }
                WorkerEvent::FrameComplete {
                    label,
                    plot,
                    stored,
                    store_error,
                    warning,
                } => {
                    self.completed += 1;
                    self.last_plot = Some(*plot);
                    self.last_plot_label = Some(label.clone());
                    if let Some(stored) = stored {
                        self.last_stored = Some(stored);
                    }
                    if let Some(err) = store_error {
                        self.failed += 1;
                        self.error = Some(format!("{label}: satellite-store write failed: {err}"));
                    } else if let Some(warning) = warning {
                        self.last_notice = Some(warning);
                        self.status = format!("Rendered {label} with a notice.");
                    } else {
                        self.status = format!("Rendered {label}.");
                    }
                }
                WorkerEvent::FrameFailed { label, error } => {
                    self.failed += 1;
                    self.error = Some(format!("{label}: {error}"));
                    self.status = format!("Skipped failed frame {label}; continuing.");
                }
                WorkerEvent::Finished {
                    completed,
                    failed,
                    cancelled,
                } => {
                    self.completed = completed;
                    self.failed = failed;
                    self.status = if cancelled {
                        format!(
                            "Cancelled at a frame boundary: {completed} complete, {failed} failed."
                        )
                    } else if failed > 0 {
                        format!("Finished: {completed} complete, {failed} failed.")
                    } else {
                        format!("Finished {completed} frame(s).")
                    };
                    terminal = true;
                }
                WorkerEvent::Fatal(error) => {
                    self.error = Some(error);
                    self.status = "SimSat job failed.".to_owned();
                    terminal = true;
                }
            }
        }

        if disconnected && !terminal && self.task.is_some() {
            self.error = Some("SimSat worker exited without a completion message.".to_owned());
            self.status = "SimSat worker stopped unexpectedly.".to_owned();
            terminal = true;
        }
        if terminal {
            if let Some(stored) = self.last_stored.clone() {
                actions.push(SimSatAction::SatelliteFrameWritten {
                    key: stored.key,
                    hhmm: stored.hhmm,
                });
            } else if let Some(plot) = &self.last_plot {
                match plot.to_plot_source() {
                    Ok(source) => actions.push(SimSatAction::OpenPlot(source)),
                    Err(error) => {
                        let plot_error =
                            format!("Rendered output could not open in the native plot: {error}");
                        self.error = Some(match self.error.take() {
                            Some(existing) => format!("{existing}\n{plot_error}"),
                            None => plot_error,
                        });
                    }
                }
            }
            self.task = None;
            self.cancellation_requested = false;
        }
        if self.task.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        actions
    }

    pub(crate) fn ui(&mut self, ui: &mut egui::Ui) -> Vec<SimSatAction> {
        let state_before = SimSatSavedState::from_pane(self);
        let mut actions = self.poll(ui.ctx());
        if !self.cached_loaded {
            self.refresh_cached_hrrr();
        }

        ui.heading("SimSat");
        ui.label(
            "Render physically based simulated satellite imagery from WRF or HRRR native levels. \
             CPU output lands in BowEcho's satellite player and loops by source run; the optional \
             one-frame GPU preview opens only in Native plot. Raw physical scalar fields stay \
             georeferenced for plotting and PNG export.",
        );
        ui.add_space(6.0);

        ui.group(|ui| {
            ui.strong("Source");
            ui.horizontal_wrapped(|ui| {
                for mode in SourceMode::ALL {
                    ui.radio_value(&mut self.source_mode, mode, mode.label());
                }
            });
            ui.add_space(4.0);
            match self.source_mode {
                SourceMode::Local => self.local_source_ui(ui),
                SourceMode::CachedHrrr => self.cached_hrrr_ui(ui),
                SourceMode::DownloadHrrr => self.download_hrrr_ui(ui),
            }
        });

        ui.add_space(6.0);
        let mut product_changed = false;
        let mut satellite_changed = false;
        ui.group(|ui| {
            ui.strong("Product and view");
            egui::Grid::new("simsat-product-view-grid")
                .num_columns(2)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    ui.label("Product");
                    product_changed = egui::ComboBox::from_id_salt("simsat-product")
                        .selected_text(self.product.label())
                        .show_ui(ui, |ui| {
                            for product in SimSatProduct::ALL {
                                ui.selectable_value(&mut self.product, product, product.label());
                            }
                        })
                        .response
                        .changed();
                    ui.end_row();

                    ui.label("Intent");
                    egui::ComboBox::from_id_salt("simsat-render-intent")
                        .selected_text(self.render_intent.label())
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.render_intent,
                                RenderIntent::Display,
                                RenderIntent::Display.label(),
                            );
                            ui.selectable_value(
                                &mut self.render_intent,
                                RenderIntent::SensorFastGray,
                                RenderIntent::SensorFastGray.label(),
                            );
                        })
                        .response
                        .on_hover_text(
                            "Display preserves SimSat's reviewed appearance. Sensor Fast Gray \
                             applies the strict simsat-fast-gray-v1 operator on a temporary copy, \
                             reports every neutralized display transform, and requires CPU. It is \
                             not yet a complete ABI/AHI channel simulator.",
                        );
                    ui.end_row();

                    ui.label("View");
                    egui::ComboBox::from_id_salt("simsat-view")
                        .selected_text(self.view.label())
                        .show_ui(ui, |ui| {
                            for view in OutputView::ALL {
                                ui.selectable_value(&mut self.view, view, view.label());
                            }
                        });
                    ui.end_row();

                    ui.label("Satellite");
                    ui.add_enabled_ui(self.view == OutputView::Geostationary, |ui| {
                        satellite_changed = egui::ComboBox::from_id_salt("simsat-satellite")
                            .selected_text(self.satellite.label())
                            .show_ui(ui, |ui| {
                                for satellite in SatelliteChoice::ALL {
                                    ui.selectable_value(
                                        &mut self.satellite,
                                        satellite,
                                        satellite.label(),
                                    );
                                }
                            })
                            .response
                            .changed();
                    });
                    ui.end_row();

                    ui.label("Navigation");
                    ui.add_enabled_ui(
                        self.view == OutputView::Geostationary
                            && self.satellite != SatelliteChoice::Himawari,
                        |ui| {
                            egui::ComboBox::from_id_salt("simsat-geo-navigation")
                                .selected_text(self.geo_navigation.label())
                                .show_ui(ui, |ui| {
                                    for navigation in GeoNavigation::ALL {
                                        ui.selectable_value(
                                            &mut self.geo_navigation,
                                            navigation,
                                            navigation.label(),
                                        );
                                    }
                                })
                                .response
                                .on_hover_text(
                                    "Model sphere preserves the WRF-consistent shipped camera. \
                                     GOES-R ABI uses official sweep-x ellipsoid navigation and is \
                                     CPU-only. Navigation geometry does not by itself make the \
                                     radiometry sensor-exact.",
                                );
                        },
                    );
                    ui.end_row();

                    ui.label("Resolution");
                    egui::ComboBox::from_id_salt("simsat-resolution")
                        .selected_text(resolution_short_label(self.resolution))
                        .show_ui(ui, |ui| {
                            for resolution in ResolutionMode::ALL {
                                ui.selectable_value(
                                    &mut self.resolution,
                                    resolution,
                                    resolution_short_label(resolution),
                                );
                            }
                        })
                        .response
                        .on_hover_text(
                            "Model native keeps one output pixel per source grid cell. ABI 1 km / \
                             2 km use physical map spacing in top-down view and scan pitch in \
                             geostationary view.",
                        );
                    ui.end_row();
                });
            if self.view == OutputView::TopDown {
                ui.small(
                    "Top-down is map-registered; satellite choice is ignored, while Native / ABI \
                     resolution remains active.",
                );
            }
        });
        if product_changed || satellite_changed {
            let previous_footprint = self.instrument_footprint;
            let previous_optics = self.cloud_optics;
            if satellite_changed && self.satellite == SatelliteChoice::Himawari {
                self.instrument_footprint = InstrumentFootprint::Off;
            }
            self.normalize_incompatible_controls();
            if self.instrument_footprint != previous_footprint
                || self.cloud_optics != previous_optics
            {
                self.last_notice = Some(
                    "Cleared a hidden SimSat science control that is incompatible with the new product or satellite selection."
                        .to_owned(),
                );
            }
        }

        ui.add_space(6.0);
        self.quick_mode_ui(ui);

        ui.add_space(6.0);
        egui::CollapsingHeader::new("Render controls")
            .id_salt("simsat-render-controls")
            .show(ui, |ui| {
                egui::ComboBox::from_id_salt("simsat-quality")
                    .selected_text(self.quality.label())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.quality,
                            RenderQuality::Final,
                            RenderQuality::Final.label(),
                        );
                        ui.selectable_value(
                            &mut self.quality,
                            RenderQuality::Preview,
                            RenderQuality::Preview.label(),
                        );
                    });
                ui.add(
                    egui::Slider::new(&mut self.margin_frac, 0.0..=0.75)
                        .text("earth margin")
                        .custom_formatter(|value, _| format!("{:.0}%", value * 100.0)),
                );
                self.science_precision_ui(ui);
                if self.product.has_band13_component() {
                    self.thermal_response_ui(ui);
                }
                if self.product.uses_visible_ground() {
                    self.visible_controls_ui(ui);
                }
                ui.small(
                    "Satellite frames and loops always use the full CPU path. First use ingests a \
                     reusable SimSat v6 brick; a full HRRR native file can briefly require more \
                     than 2 GB of memory. ScienceCloudF16 uses an isolated v7 cache.",
                );
            });

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if self.is_busy() {
                ui.spinner();
                if ui
                    .add_enabled(
                        !self.cancellation_requested,
                        egui::Button::new("Cancel after current frame"),
                    )
                    .clicked()
                {
                    self.request_cancel();
                }
            } else {
                if ui.button("Render to Satellite").clicked() {
                    self.start_current_job(ui.ctx(), RenderBackend::Cpu);
                }
                let gpu_unavailable = self.gpu_preview_unavailable_reason();
                let gpu_ready = gpu_unavailable.is_none();
                if ui
                    .add_enabled(gpu_ready, egui::Button::new("GPU preview"))
                    .on_hover_text(
                        "Render the first frame through SimSat's synchronous wgpu preview and \
                         open it in Native plot. Preview output is never added to Satellite loops, \
                         and every temporary compatibility substitution is reported.",
                    )
                    .on_disabled_hover_text(
                        gpu_unavailable.unwrap_or(
                            "GPU preview is unavailable for the selected SimSat controls.",
                        ),
                    )
                    .clicked()
                {
                    self.start_current_job(ui.ctx(), RenderBackend::GpuPreview);
                }
            }
            if self.total > 0 {
                let progress = self.completed.min(self.total) as f32 / self.total as f32;
                ui.add(
                    egui::ProgressBar::new(progress)
                        .desired_width(180.0)
                        .text(format!("{}/{}", self.completed, self.total)),
                );
            }
        });
        ui.label(&self.status);
        if let Some(notice) = &self.last_notice {
            ui.small(notice);
        }
        if self.cancellation_requested {
            ui.small(
                "Cancellation requested. A download may stop between chunks; an active render \
                 finishes this frame before the sequence stops.",
            );
        }
        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }

        if let Some(label) = &self.last_plot_label {
            ui.add_space(8.0);
            ui.separator();
            ui.horizontal_wrapped(|ui| {
                ui.strong(format!("Latest: {label}"));
                if ui
                    .add_enabled(
                        self.last_plot.is_some(),
                        egui::Button::new("Open native plot"),
                    )
                    .clicked()
                    && let Some(payload) = &self.last_plot
                {
                    match payload.to_plot_source() {
                        Ok(source) => actions.push(SimSatAction::OpenPlot(source)),
                        Err(error) => self.error = Some(format!("Could not open plot: {error}")),
                    }
                }
            });
            if let Some(stored) = &self.last_stored {
                ui.small(format!(
                    "Satellite frame: {} t{:04}",
                    stored.key, stored.hhmm
                ));
            }
        }

        if state_before != SimSatSavedState::from_pane(self) {
            self.persisted_state_dirty = true;
        }
        actions
    }

    fn quick_mode_ui(&mut self, ui: &mut egui::Ui) {
        ui.group(|ui| {
            ui.horizontal_wrapped(|ui| {
                ui.strong("Quick mode");
                for mode in SimSatQuickMode::ALL {
                    if ui
                        .add_enabled(!self.is_busy(), egui::Button::new(mode.label()))
                        .on_hover_text(mode.description())
                        .clicked()
                    {
                        match self.apply_quick_mode(mode) {
                            Ok(()) => {
                                self.error = None;
                                self.status = format!(
                                    "Applied {}. Render again to update the image.",
                                    mode.full_label()
                                );
                            }
                            Err(error) => self.error = Some(error),
                        }
                    }
                }
                let active = self.active_quick_mode();
                let current = active.map_or("Custom", SimSatQuickMode::full_label);
                ui.weak(format!("Current: {current}"));
            });
            ui.small(
                "Presets never change the selected source or product. Every individual control \
                 remains available below; manual edits intentionally change Current to Custom.",
            );
        });
    }

    fn science_precision_ui(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Science and precision")
            .id_salt("simsat-science-precision")
            .show(ui, |ui| {
                let mut science_f16 = self.storage_profile == StorageProfile::ScienceCloudF16;
                if ui
                    .checkbox(
                        &mut science_f16,
                        "ScienceCloudF16 extinction precision (CPU, experimental)",
                    )
                    .on_hover_text(
                        "Stores liquid, ice, snow, and total-precipitation extinction as bounded \
                         log2-f16 source values in an isolated v7 cache. CompactU8 v6 remains the \
                         production default. Switching profiles re-ingests the original source.",
                    )
                    .changed()
                {
                    self.storage_profile = if science_f16 {
                        StorageProfile::ScienceCloudF16
                    } else {
                        StorageProfile::CompactU8
                    };
                }
                ui.weak(if science_f16 {
                    "ScienceCloudF16 selected: larger isolated cache; GPU preview unavailable."
                } else {
                    "CompactU8 selected: production default."
                });
            });
    }

    fn thermal_response_ui(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Thermal response")
            .id_salt("simsat-thermal-response")
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label("Band 13 sensor");
                    egui::ComboBox::from_id_salt("simsat-thermal-sensor")
                        .selected_text(self.thermal_sensor.label())
                        .show_ui(ui, |ui| {
                            for sensor in ThermalSensor::ALL {
                                ui.selectable_value(
                                    &mut self.thermal_sensor,
                                    sensor,
                                    sensor.label(),
                                );
                            }
                        })
                        .response
                        .on_hover_text(
                            "Fast gray preserves the historical center-wavelength path. The FM4 \
                             option integrates Planck emission through NOAA's official GOES-19 \
                             Band 13 spectral response and uses that response for BT inversion.",
                        );
                });
                if let Some(warning) = self.thermal_sensor.limitation_warning() {
                    ui.colored_label(
                        ui.visuals().warn_fg_color,
                        format!("Science limitation: {warning}"),
                    );
                }
                ui.separator();
                let mut footprint_on =
                    self.instrument_footprint == InstrumentFootprint::GoesRAbiBand13Mtf;
                if ui
                    .checkbox(
                        &mut footprint_on,
                        "ABI Band 13 MTF footprint (experimental)",
                    )
                    .on_hover_text(
                        "Applies the GOES-16-measured Band 13 east-west MTF-informed response to \
                         complete FM4 channel radiance before BT inversion. Enabling it selects \
                         Geostationary, exact GOES-R navigation, ABI 2 km, FM4, and CPU-only \
                         rendering. Transfer from GOES-16 to GOES-19 remains unvalidated.",
                    )
                    .changed()
                {
                    self.instrument_footprint = if footprint_on {
                        if self.satellite == SatelliteChoice::Himawari {
                            self.satellite = SatelliteChoice::GoesEast;
                        }
                        self.view = OutputView::Geostationary;
                        self.geo_navigation = GeoNavigation::GoesRAbiFixedGrid;
                        self.resolution = ResolutionMode::Abi2km;
                        self.thermal_sensor = ThermalSensor::GoesRAbiBand13Fm4;
                        InstrumentFootprint::GoesRAbiBand13Mtf
                    } else {
                        InstrumentFootprint::Off
                    };
                }
                if self.instrument_footprint != InstrumentFootprint::Off {
                    let compatible = self.view == OutputView::Geostationary
                        && self.satellite != SatelliteChoice::Himawari
                        && self.geo_navigation == GeoNavigation::GoesRAbiFixedGrid
                        && self.resolution == ResolutionMode::Abi2km
                        && self.thermal_sensor == ThermalSensor::GoesRAbiBand13Fm4;
                    ui.colored_label(
                        if compatible {
                            ui.visuals().strong_text_color()
                        } else {
                            ui.visuals().warn_fg_color
                        },
                        if compatible {
                            "Exact 56-urad ABI lattice active; crop and invalid-mask perimeter are no-data."
                        } else {
                            "Footprint needs Geostationary + exact GOES-R + ABI 2 km + FM4. Toggle it off/on to restore those requirements."
                        },
                    );
                    ui.weak(
                        "GOES-16 east-west MTF is transferred to GOES-19/FM4 as an unvalidated \
                         hypothesis; north-south MTF, temporal integration, and detector variation \
                         are not modeled.",
                    );
                }
            });
    }

    fn gpu_preview_unavailable_reason(&self) -> Option<&'static str> {
        if self.product != SimSatProduct::Visible {
            Some(
                "GPU preview is limited to Visible true color. Use CPU for composites, thermal, and derived products.",
            )
        } else if self.storage_profile == StorageProfile::ScienceCloudF16 {
            Some("ScienceCloudF16 is CPU-only because the GPU path consumes CompactU8 codes.")
        } else if self.render_intent == RenderIntent::SensorFastGray {
            Some("Sensor Fast Gray is CPU-only so its strict operator cannot be weakened.")
        } else if self.instrument_footprint != InstrumentFootprint::Off {
            Some("Instrument footprints are CPU-only.")
        } else if self.view == OutputView::Geostationary
            && self.geo_navigation == GeoNavigation::GoesRAbiFixedGrid
        {
            Some("Exact GOES-R ellipsoid navigation is CPU-only.")
        } else {
            None
        }
    }

    fn visible_controls_ui(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Atmosphere and surface")
            .id_salt("simsat-atmosphere-controls")
            .default_open(true)
            .show(ui, |ui| {
                ui.add(egui::Slider::new(&mut self.exposure, 0.25..=4.0).text("Exposure"))
                    .on_hover_text(
                        "Finished-visible display gain. SimSat's shipped value is 1.5; 1.0 is \
                     neutral physical reflectance.",
                    );
                ui.add(
                    egui::Slider::new(&mut self.aerosol_optical_depth, 0.0..=0.6)
                        .text("Aerosol optical depth")
                        .fixed_decimals(2),
                )
                .on_hover_text(
                    "Visible aerosol optical depth at 550 nm. Zero removes aerosol but keeps \
                     molecular Rayleigh scattering.",
                );
                ui.horizontal_wrapped(|ui| {
                    ui.checkbox(&mut self.rh_aerosol_swelling, "RH aerosol swelling")
                        .on_hover_text(
                            "Apply SimSat's documented 1.5x humid-growth multiplier to aerosol \
                             extinction.",
                        );
                    if self.rh_aerosol_swelling {
                        ui.weak(format!(
                            "effective AOD {:.2}",
                            self.aerosol_optical_depth * 1.5
                        ));
                    }
                });
                ui.checkbox(
                    &mut self.atmosphere_correction,
                    "Daytime aerial-veil correction",
                )
                .on_hover_text(
                    "Reduce modeled daytime path airlight for finished true color. Off retains \
                     the full modeled veil.",
                );
                ui.checkbox(&mut self.terrain_atmosphere, "Terrain-height atmosphere")
                    .on_hover_text(
                        "Shorten the view and sunlight atmospheric columns to each model pixel's \
                     terrain elevation. On is the physical shipped path.",
                    );
            });

        egui::CollapsingHeader::new("Clouds")
            .id_salt("simsat-cloud-controls")
            .default_open(true)
            .show(ui, |ui| {
                ui.checkbox(&mut self.clouds, "Volumetric clouds");
                ui.add_enabled_ui(self.clouds, |ui| {
                    if ui
                        .checkbox(&mut self.fractional_clouds, "Use model cloud fraction")
                        .on_hover_text(
                            "Use WRF CLDFRA or HRRR's native cloud-fraction field for fractional \
                             subcolumns. Missing fields safely fall back to full cells.",
                        )
                        .changed()
                        && self.fractional_clouds
                        && self.fractional_cloud_mode == FractionalCloudMode::Off
                    {
                        self.fractional_cloud_mode = FractionalCloudMode::EffectiveOd;
                    }
                    if self.fractional_clouds {
                        ui.horizontal_wrapped(|ui| {
                            ui.label("Fractional closure");
                            egui::ComboBox::from_id_salt("simsat-fractional-cloud-mode")
                                .selected_text(fractional_cloud_mode_label(
                                    self.fractional_cloud_mode,
                                ))
                                .show_ui(ui, |ui| {
                                    for mode in [
                                        FractionalCloudMode::EffectiveOd,
                                        FractionalCloudMode::Deterministic4,
                                        FractionalCloudMode::Deterministic8,
                                        FractionalCloudMode::Deterministic16,
                                    ] {
                                        ui.selectable_value(
                                            &mut self.fractional_cloud_mode,
                                            mode,
                                            fractional_cloud_mode_label(mode),
                                        );
                                    }
                                });
                        });
                        if let Some(count) = self
                            .fractional_cloud_mode
                            .deterministic_subcolumn_count()
                        {
                            ui.weak(format!(
                                "Deterministic {count}-member fixed-stratified CPU reference: roughly {count}x cloud-march cost."
                            ));
                        }
                    }
                    ui.horizontal_wrapped(|ui| {
                        ui.label("Cloud transport");
                        egui::ComboBox::from_id_salt("simsat-cloud-transport")
                            .selected_text(cloud_transport_label(self.cloud_transport))
                            .show_ui(ui, |ui| {
                                for mode in CLOUD_TRANSPORTS {
                                    ui.selectable_value(
                                        &mut self.cloud_transport,
                                        mode,
                                        cloud_transport_label(mode),
                                    );
                                }
                            });
                    });
                    ui.weak(match self.cloud_transport {
                        CloudMultiscatterMode::LegacyOctaves => {
                            "Established bright-anvil transport; the shipped default."
                        }
                        CloudMultiscatterMode::SingleScatter => {
                            "Direct single scattering only; a dimmer diagnostic path."
                        }
                        CloudMultiscatterMode::DeltaFluxV1 => {
                            "Research Stage-2 isotropic closure; CPU-only and opt-in."
                        }
                        CloudMultiscatterMode::DeltaFluxV2 => {
                            "Research brightness-neutral P1 closure; CPU-only and opt-in."
                        }
                        CloudMultiscatterMode::DeltaFluxV3 => {
                            "Research bounded second-order angular-memory closure; CPU-only and opt-in."
                        }
                    });
                    ui.add(
                        egui::Slider::new(&mut self.cloud_optical_depth_scale, 0.0..=4.0)
                            .text("Cloud optical-depth scale")
                            .fixed_decimals(2),
                    )
                    .on_hover_text(
                        "Visible cloud extinction sensitivity. SimSat ships 0.15; 1.00 uses \
                         model extinction unscaled. IR and quantitative cloud optical depth do \
                         not consume this display control.",
                    );
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("Reset 0.15").clicked() {
                            self.cloud_optical_depth_scale =
                                simsat::clouds::DEFAULT_CLOUD_OPTICAL_DEPTH_SCALE;
                        }
                        if ui.button("Unscaled 1.00").clicked() {
                            self.cloud_optical_depth_scale = 1.0;
                        }
                    });
                    ui.add_enabled_ui(self.product.supports_native_cloud_optics(), |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.label("Particle optics");
                            egui::ComboBox::from_id_salt("simsat-cloud-optics")
                                .selected_text(cloud_optics_label(self.cloud_optics))
                                .show_ui(ui, |ui| {
                                    for mode in [
                                        CloudOpticsMode::Fixed,
                                        CloudOpticsMode::NsslNative,
                                        CloudOpticsMode::HrrrThompsonNative,
                                    ] {
                                        ui.selectable_value(
                                            &mut self.cloud_optics,
                                            mode,
                                            cloud_optics_label(mode),
                                        );
                                    }
                                })
                                .response
                                .on_hover_text(
                                    "Native modes use scheme-provided mass/number/temperature \
                                     moments where valid and fall back per cell. They use distinct \
                                     caches and are visible-only because thermal mass recovery \
                                     remains tied to fixed radii.",
                                );
                        });
                    });
                    if !self.product.supports_native_cloud_optics() {
                        ui.weak(
                            "GeoColor and Sandwich keep fixed particle optics so their Band 13 \
                             mass recovery remains internally consistent.",
                        );
                    }
                    ui.checkbox(
                        &mut self.feather_exposed_domain_edges,
                        "Feather exposed domain edges",
                    )
                    .on_hover_text(
                        "Fade finished visible cloud extinction across the outer 4% only when \
                         the camera exposes the finite WRF/HRRR boundary.",
                    );
                    ui.checkbox(&mut self.beer_powder, "Beer-powder shading")
                        .on_hover_text(
                            "Optional cloud appearance shaping. It is off in SimSat's shipped \
                             preset because the transport model already supplies buildup.",
                        );
                    ui.checkbox(
                        &mut self.granulation,
                        "Sub-grid cloud granulation (experimental)",
                    )
                    .on_hover_text(
                        "Subtract-only edge detail for unresolved boundary-layer cumulus. It is \
                         off by default and never changes thermal or derived products.",
                    );
                    ui.add_enabled_ui(self.view == OutputView::TopDown, |ui| {
                        ui.checkbox(
                            &mut self.topdown_stratiform_regularization,
                            "Top-down stratiform reconstruction (experimental)",
                        )
                        .on_hover_text(
                            "Opt-in v0.1.6 observation operator for broad low/liquid decks. It \
                             can reduce native-grid HRRR rings while conserving selected-area \
                             optical depth; geostationary and raw-band output are unchanged.",
                        );
                        ui.checkbox(
                            &mut self.topdown_cloud_footprint,
                            "Top-down cloud footprint (experimental)",
                        )
                        .on_hover_text(
                            "Display-only seven-tap footprint applied to pre-tonemap cloud \
                             radiance while terrain remains sharp. CPU-only, top-down visible \
                             output only; thermal, derived, and geostationary products ignore it.",
                        );
                    });
                    if self.view != OutputView::TopDown {
                        ui.weak(
                            "Stratiform reconstruction and cloud footprint apply only to top-down visible output.",
                        );
                    }
                });
            });

        egui::CollapsingHeader::new("Lighting")
            .id_salt("simsat-lighting-controls")
            .show(ui, |ui| {
                ui.checkbox(&mut self.sun_override, "Override sun (what-if)")
                    .on_hover_text(
                        "Use a chosen sun position instead of source valid time. This is a \
                         non-physical visualization override.",
                    );
                ui.add_enabled_ui(self.sun_override, |ui| {
                    ui.add(
                        egui::Slider::new(&mut self.sun_elevation_deg, -10.0..=90.0)
                            .text("Sun elevation"),
                    );
                    ui.add(
                        egui::Slider::new(&mut self.sun_azimuth_deg, 0.0..=360.0)
                            .text("Sun azimuth"),
                    );
                    ui.colored_label(ui.visuals().warn_fg_color, "what-if lighting");
                });
            });

        egui::CollapsingHeader::new("Ground / Blue Marble")
            .id_salt("simsat-ground-controls")
            .show(ui, |ui| {
                ui.checkbox(
                    &mut self.bluemarble_download,
                    "Download missing 2 km Blue Marble months",
                );
                ui.add(
                    egui::Slider::new(&mut self.bluemarble_month, 0..=12)
                        .text("Blue Marble month")
                        .custom_formatter(|value, _| {
                            if value < 0.5 {
                                "Auto".to_owned()
                            } else {
                                format!("{value:.0}")
                            }
                        }),
                )
                .on_hover_text(
                    "Auto blends seasonal ground imagery for valid date; 1-12 forces a \
                     specific month as a what-if surface.",
                );
            });

        egui::CollapsingHeader::new("Advanced display calibration")
            .id_salt("simsat-display-calibration")
            .show(ui, |ui| {
                ui.add(
                    egui::Slider::new(&mut self.ground_gain, 0.25..=4.0).text("Ground lift"),
                )
                .on_hover_text("Display-only daytime ground gain; 1.0 is neutral.");
                ui.add(
                    egui::Slider::new(&mut self.cloud_softclip, 0.05..=1.0)
                        .text("Highlight knee"),
                );
                ui.add(
                    egui::Slider::new(&mut self.cloud_highlight_max, 0.25..=4.0)
                        .text("Highlight ceiling"),
                );
                ui.separator();
                ui.checkbox(
                    &mut self.land_sza_normalization,
                    "Land solar-zenith normalization",
                );
                ui.add_enabled_ui(self.land_sza_normalization, |ui| {
                    ui.add(
                        egui::Slider::new(&mut self.land_sza_max_gain, 1.0..=4.0)
                            .text("SZA max gain"),
                    );
                });
                ui.checkbox(&mut self.land_dark_toe, "Dark-land reflectance toe");
                ui.add_enabled_ui(self.land_dark_toe, |ui| {
                    ui.add(
                        egui::Slider::new(&mut self.land_dark_toe_knee, 0.001..=0.25)
                            .text("Toe knee"),
                    );
                    ui.add(
                        egui::Slider::new(&mut self.land_dark_toe_gamma, 0.05..=1.0)
                            .text("Toe gamma"),
                    );
                    ui.add(
                        egui::Slider::new(&mut self.land_dark_toe_max_gain, 1.0..=4.0)
                            .text("Toe max gain"),
                    );
                });
                if ui.button("Restore shipped display calibration").clicked() {
                    let land = LandAppearanceConfig::default();
                    self.exposure = DEFAULT_EXPOSURE as f32;
                    self.ground_gain = GROUND_DAY_LIFT as f32;
                    self.cloud_softclip = CLOUD_SOFTCLIP_KNEE as f32;
                    self.cloud_highlight_max = RHO_HIGHLIGHT_MAX as f32;
                    self.land_sza_normalization = land.sza_normalization;
                    self.land_sza_max_gain = land.sza_max_gain as f32;
                    self.land_dark_toe = land.dark_toe;
                    self.land_dark_toe_knee = land.dark_toe_knee as f32;
                    self.land_dark_toe_gamma = land.dark_toe_gamma as f32;
                    self.land_dark_toe_max_gain = land.dark_toe_max_gain as f32;
                }
                ui.weak(
                    "Display-only: raw visible bands, IR, water vapor, and derived fields are unchanged.",
                );
            });
    }

    fn local_source_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.local_path)
                    .desired_width(360.0)
                    .hint_text("wrfout / GRIB2 file, SimSat run.json, or folder"),
            );
            #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
            {
                // Keep this unfiltered: normal wrfout files are extensionless.
                if ui.button("File...").clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .set_title("Open WRF / HRRR input")
                        .pick_file()
                {
                    self.local_path = path.display().to_string();
                }
                if ui.button("Folder...").clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .set_title("Open WRF / HRRR sequence folder")
                        .pick_folder()
                {
                    self.local_path = path.display().to_string();
                }
            }
        });
        ui.small(
            "Folders are probed and sorted by valid time. HRRR requires wrfnat native-level \
             GRIB2; pressure/surface products cannot drive SimSat.",
        );
    }

    fn cached_hrrr_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let selected = self
                .cached_hrrr
                .get(self.cached_selected)
                .map(|entry| entry.label())
                .unwrap_or_else(|| "No cached wrfnat files".to_owned());
            egui::ComboBox::from_id_salt("simsat-cached-hrrr")
                .selected_text(selected)
                .show_ui(ui, |ui| {
                    for (index, entry) in self.cached_hrrr.iter().enumerate() {
                        ui.selectable_value(&mut self.cached_selected, index, entry.label());
                    }
                });
            if ui.button("Refresh").clicked() {
                self.refresh_cached_hrrr();
            }
        });
        ui.small(format!("SimSat inputs: {}", hrrr_input_dir().display()));
        ui.small(format!(
            "Also checks BowEcho's model cache: {}",
            settings::model_cache_dir().display()
        ));
    }

    fn download_hrrr_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label("Date");
            ui.add(
                egui::TextEdit::singleline(&mut self.hrrr_date)
                    .desired_width(90.0)
                    .hint_text("YYYYMMDD"),
            );
            ui.label("Cycle");
            ui.add(
                egui::DragValue::new(&mut self.hrrr_cycle)
                    .range(0..=23)
                    .suffix("z"),
            );
            ui.label("Forecast hour");
            ui.add(
                egui::DragValue::new(&mut self.hrrr_forecast_hour)
                    .range(0..=48)
                    .prefix("f"),
            );
            if ui.button("Latest candidate").clicked()
                && let Some(spec) = latest_specs(Utc::now(), self.hrrr_forecast_hour)
                    .into_iter()
                    .next()
            {
                self.hrrr_date = spec.date;
                self.hrrr_cycle = spec.cycle;
            }
        });
        ui.small(
            "Render downloads the selected HRRR wrfnat file first, with resumable cache reuse. \
             The pressure/surface model downloads remain separate.",
        );
    }

    fn refresh_cached_hrrr(&mut self) {
        self.cached_hrrr = discover_cached_hrrr();
        self.cached_selected = self
            .cached_selected
            .min(self.cached_hrrr.len().saturating_sub(1));
        self.cached_loaded = true;
    }

    fn selected_job_source(&self) -> Result<JobSource, String> {
        match self.source_mode {
            SourceMode::Local => {
                let trimmed = self.local_path.trim();
                if trimmed.is_empty() {
                    return Err("Choose a WRF/GRIB file or folder first.".to_owned());
                }
                let path = PathBuf::from(trimmed);
                if !path.exists() {
                    return Err(format!("Source does not exist: {}", path.display()));
                }
                Ok(JobSource::Local(path))
            }
            SourceMode::CachedHrrr => self
                .cached_hrrr
                .get(self.cached_selected)
                .map(|entry| JobSource::CachedHrrr {
                    path: entry.path.clone(),
                    spec: entry.spec.clone(),
                })
                .ok_or_else(|| "No cached HRRR wrfnat file is selected.".to_owned()),
            SourceMode::DownloadHrrr => {
                let spec = HrrrNativeSpec::new(
                    self.hrrr_date.trim().to_owned(),
                    self.hrrr_cycle,
                    self.hrrr_forecast_hour,
                )
                .map_err(|error| error.to_string())?;
                spec.validate().map_err(|error| error.to_string())?;
                Ok(JobSource::DownloadHrrr {
                    spec,
                    root: hrrr_input_dir(),
                })
            }
        }
    }

    fn start_current_job(&mut self, ctx: &egui::Context, backend: RenderBackend) {
        if self.task.is_some() {
            return;
        }
        let source = match self.selected_job_source() {
            Ok(source) => source,
            Err(error) => {
                self.error = Some(error);
                return;
            }
        };
        let cache_root = settings::simsat_cache_dir().join(ENGINE_CACHE_SUBDIR);
        let store_root = settings::sat_store_dir();
        let sector = qualified_sector(&source.group_base(), self.product, self.view);
        let job = RenderJob {
            source,
            backend,
            storage_profile: self.storage_profile,
            render_intent: self.render_intent,
            product: self.product,
            view: self.view,
            satellite: if self.view == OutputView::TopDown {
                SatelliteChoice::GoesEast
            } else {
                self.satellite
            },
            geo_navigation: self.geo_navigation,
            resolution: self.resolution,
            quality: self.quality,
            margin_frac: self.margin_frac,
            granulation: self.granulation,
            bluemarble_download: self.bluemarble_download,
            bluemarble_month: self.bluemarble_month,
            exposure: self.exposure,
            ground_gain: self.ground_gain,
            cloud_softclip: self.cloud_softclip,
            cloud_highlight_max: self.cloud_highlight_max,
            aerosol_optical_depth: self.aerosol_optical_depth,
            rh_aerosol_swelling: self.rh_aerosol_swelling,
            atmosphere_correction: self.atmosphere_correction,
            terrain_atmosphere: self.terrain_atmosphere,
            land_sza_normalization: self.land_sza_normalization,
            land_sza_max_gain: self.land_sza_max_gain,
            land_dark_toe: self.land_dark_toe,
            land_dark_toe_knee: self.land_dark_toe_knee,
            land_dark_toe_gamma: self.land_dark_toe_gamma,
            land_dark_toe_max_gain: self.land_dark_toe_max_gain,
            clouds: self.clouds,
            fractional_clouds: self.fractional_clouds,
            fractional_cloud_mode: self.fractional_cloud_mode,
            cloud_optical_depth_scale: self.cloud_optical_depth_scale,
            cloud_optics: self.cloud_optics,
            feather_exposed_domain_edges: self.feather_exposed_domain_edges,
            cloud_transport: self.cloud_transport,
            beer_powder: self.beer_powder,
            topdown_stratiform_regularization: self.topdown_stratiform_regularization,
            topdown_cloud_footprint: self.topdown_cloud_footprint,
            thermal_sensor: self.thermal_sensor,
            instrument_footprint: self.instrument_footprint,
            sun_override: self.sun_override,
            sun_elevation_deg: self.sun_elevation_deg,
            sun_azimuth_deg: self.sun_azimuth_deg,
            cache_root,
            store_root,
            sector,
        };
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_worker = Arc::clone(&cancel);
        let repaint = ctx.clone();
        let panic_tx = tx.clone();
        match std::thread::Builder::new()
            .name("bowecho-simsat".to_owned())
            .spawn(move || {
                simsat::platform::lower_worker_thread_priority();
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_render_job(job, cancel_worker, tx, &repaint)
                }));
                if outcome.is_err() {
                    let _ = panic_tx.send(WorkerEvent::Fatal(
                        "SimSat worker panicked; no further frames were attempted.".to_owned(),
                    ));
                    repaint.request_repaint();
                }
            }) {
            Ok(_) => {
                self.task = Some(RenderTask { rx, cancel });
                self.total = 0;
                self.completed = 0;
                self.failed = 0;
                self.cancellation_requested = false;
                self.last_notice = None;
                self.last_plot = None;
                self.last_plot_label = None;
                self.last_stored = None;
                self.error = None;
                self.status = "Preparing SimSat job...".to_owned();
                ctx.request_repaint();
            }
            Err(error) => {
                self.error = Some(format!("Could not start SimSat worker: {error}"));
            }
        }
    }

    fn request_cancel(&mut self) {
        if let Some(task) = &self.task {
            task.cancel.store(true, Ordering::Relaxed);
            self.cancellation_requested = true;
            self.status =
                "Cancellation requested; stopping the download or finishing the current frame."
                    .to_owned();
        }
    }
}

fn hrrr_input_dir() -> PathBuf {
    settings::simsat_input_dir()
}

fn discover_cached_hrrr() -> Vec<crate::simsat_hrrr::CachedNativeInput> {
    let mut inputs = discover_native_files(&hrrr_input_dir());
    inputs.extend(discover_native_files(&settings::model_cache_dir()));

    let mut seen = HashSet::new();
    inputs.retain(|input| seen.insert(input.path.clone()));
    inputs.sort_by(
        |left_input, right_input| match (&left_input.spec, &right_input.spec) {
            (Some(left_spec), Some(right_spec)) => right_spec
                .cmp(left_spec)
                .then_with(|| left_input.path.cmp(&right_input.path)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => left_input.path.cmp(&right_input.path),
        },
    );
    inputs
}

fn run_render_job(
    job: RenderJob,
    cancel: Arc<AtomicBool>,
    tx: std::sync::mpsc::Sender<WorkerEvent>,
    repaint: &egui::Context,
) {
    if let Err(error) = std::fs::create_dir_all(&job.cache_root) {
        let _ = tx.send(WorkerEvent::Fatal(format!(
            "Could not create SimSat cache {}: {error}",
            job.cache_root.display()
        )));
        repaint.request_repaint();
        return;
    }
    if job.backend == RenderBackend::Cpu
        && let Err(error) = std::fs::create_dir_all(&job.store_root)
    {
        let _ = tx.send(WorkerEvent::Fatal(format!(
            "Could not create satellite store {}: {error}",
            job.store_root.display()
        )));
        repaint.request_repaint();
        return;
    }

    let source_path = match &job.source {
        JobSource::DownloadHrrr { spec, root } => {
            let _ = tx.send(WorkerEvent::Progress {
                index: 0,
                total: 0,
                message: format!("Downloading {}...", spec.filename()),
            });
            repaint.request_repaint();
            match download_native(spec, root, cancel.as_ref()) {
                Ok(outcome) if outcome.is_ready() => outcome.path,
                Ok(outcome) if outcome.is_cancelled() => {
                    let _ = tx.send(WorkerEvent::Finished {
                        completed: 0,
                        failed: 0,
                        cancelled: true,
                    });
                    repaint.request_repaint();
                    return;
                }
                Ok(outcome) => {
                    let _ = tx.send(WorkerEvent::Fatal(format!(
                        "HRRR download did not produce a ready file: {}",
                        outcome.path.display()
                    )));
                    repaint.request_repaint();
                    return;
                }
                Err(error) => {
                    let _ = tx.send(WorkerEvent::Fatal(format!("HRRR download failed: {error}")));
                    repaint.request_repaint();
                    return;
                }
            }
        }
        JobSource::Local(path) | JobSource::CachedHrrr { path, .. } => path.clone(),
    };

    if cancel.load(Ordering::Relaxed) {
        let _ = tx.send(WorkerEvent::Finished {
            completed: 0,
            failed: 0,
            cancelled: true,
        });
        repaint.request_repaint();
        return;
    }

    let mut inputs = match discover_frame_inputs(&source_path) {
        Ok(inputs) => inputs,
        Err(error) => {
            let _ = tx.send(WorkerEvent::Fatal(error));
            repaint.request_repaint();
            return;
        }
    };
    sort_frame_inputs(&mut inputs);
    if job.backend == RenderBackend::GpuPreview {
        inputs.truncate(1);
    }
    let total = inputs.len();
    let _ = tx.send(WorkerEvent::Started { total });
    repaint.request_repaint();
    let mut completed = 0usize;
    let mut failed = 0usize;
    let mut cancelled = false;

    for (ordinal, frame) in inputs.into_iter().enumerate() {
        // The only render cancellation point: the previous frame is fully rendered and,
        // where applicable, durably stored before this check.
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }
        let index = ordinal + 1;
        let _ = tx.send(WorkerEvent::Progress {
            index,
            total,
            message: format!("Rendering {}", frame.label),
        });
        repaint.request_repaint();

        let params = render_params_for(&job, &frame);
        match simsat::api::render(&params, job.product.api_product()) {
            Ok(result) => {
                let store = if job.backend == RenderBackend::Cpu {
                    write_result_to_store(&job, &result)
                } else {
                    Ok(None)
                };
                let warning = result_warning(&result);
                match plot_payload_from_result(job.product, &frame.label, result) {
                    Ok(plot) => {
                        let (stored, store_error) = match store {
                            Ok(stored) => (stored, None),
                            Err(error) => {
                                failed += 1;
                                (None, Some(error))
                            }
                        };
                        completed += 1;
                        let _ = tx.send(WorkerEvent::FrameComplete {
                            label: frame.label,
                            plot: Box::new(plot),
                            stored,
                            store_error,
                            warning,
                        });
                    }
                    Err(error) => {
                        failed += 1;
                        let _ = tx.send(WorkerEvent::FrameFailed {
                            label: frame.label,
                            error,
                        });
                    }
                }
            }
            Err(error) => {
                failed += 1;
                let _ = tx.send(WorkerEvent::FrameFailed {
                    label: frame.label,
                    error,
                });
            }
        }
        repaint.request_repaint();
    }

    let _ = tx.send(WorkerEvent::Finished {
        completed,
        failed,
        cancelled,
    });
    repaint.request_repaint();
}

fn render_params_for(job: &RenderJob, frame: &FrameInput) -> RenderParams {
    let mut params = RenderParams::new(frame.path.clone());
    params.backend = job.backend;
    params.storage_profile = job.storage_profile;
    params.intent = job.render_intent;
    params.timestep = frame.timestep;
    params.cache = job.cache_root.clone();
    params.satellite = job.satellite.api_satellite();
    params.geo_navigation = job.geo_navigation;
    params.view = job.view.api_view();
    params.resolution = job.resolution;
    params.margin_frac = job.margin_frac;
    params.exposure = f64::from(job.exposure);
    params.ground_gain = Some(f64::from(job.ground_gain));
    params.cloud_softclip = Some(f64::from(job.cloud_softclip));
    params.cloud_highlight_max = Some(f64::from(job.cloud_highlight_max));
    params.aerosol_optical_depth = job.aerosol_optical_depth;
    params.rh_aerosol_swelling = job.rh_aerosol_swelling;
    params.atmosphere_correction = job.atmosphere_correction;
    params.terrain_atmosphere = job.terrain_atmosphere;
    params.land_appearance = LandAppearanceConfig {
        sza_normalization: job.land_sza_normalization,
        sza_max_gain: f64::from(job.land_sza_max_gain),
        dark_toe: job.land_dark_toe,
        dark_toe_knee: f64::from(job.land_dark_toe_knee),
        dark_toe_gamma: f64::from(job.land_dark_toe_gamma),
        dark_toe_max_gain: f64::from(job.land_dark_toe_max_gain),
    };
    params.steps = job.quality.steps();
    params.multiscatter = job.cloud_transport == CloudMultiscatterMode::LegacyOctaves;
    params.cloud_multiscatter = Some(job.cloud_transport);
    params.beer_powder = job.beer_powder;
    params.clouds = job.clouds;
    params.fractional_clouds = job.fractional_clouds;
    params.fractional_cloud_mode = job.fractional_cloud_mode;
    params.cloud_optical_depth_scale = job.cloud_optical_depth_scale;
    params.cloud_optics = job.cloud_optics;
    params.feather_exposed_domain_edges = job.feather_exposed_domain_edges;
    params.granulation = Some(job.clouds && job.granulation && job.product.uses_visible_ground());
    params.topdown_stratiform_regularization =
        job.topdown_stratiform_regularization && job.view == OutputView::TopDown;
    params.topdown_cloud_footprint = job.topdown_cloud_footprint && job.view == OutputView::TopDown;
    params.thermal_sensor = job.thermal_sensor;
    params.instrument_footprint = job.instrument_footprint;
    params.sun_override = job.sun_override.then_some(SunOverride {
        elev_deg: Some(f64::from(job.sun_elevation_deg)),
        az_deg: Some(f64::from(job.sun_azimuth_deg)),
    });
    params.derived_colormap = false;
    params.ir_enhancement = None;
    params.bluemarble = if job.product.uses_visible_ground() {
        BlueMarble::Seasonal {
            month_override: (1..=12)
                .contains(&job.bluemarble_month)
                .then_some(job.bluemarble_month),
            download: job.bluemarble_download,
        }
    } else {
        BlueMarble::FlatAlbedo
    };
    params
}

fn discover_frame_inputs(path: &Path) -> Result<Vec<FrameInput>, String> {
    if path.is_file() {
        return probe_source_file(path);
    }
    if !path.is_dir() {
        return Err(format!(
            "SimSat source is neither a file nor a folder: {}",
            path.display()
        ));
    }
    let manifest = path.join("run.json");
    if manifest.is_file() {
        return probe_source_file(&manifest);
    }
    let mut inputs = Vec::new();
    let entries = std::fs::read_dir(path)
        .map_err(|error| format!("Could not read source folder {}: {error}", path.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("Could not read source entry: {error}"))?;
        let candidate = entry.path();
        if !candidate.is_file() || !looks_like_sequence_source(&candidate) {
            continue;
        }
        // A mixed folder may contain unrelated files. A bad candidate is skipped; if
        // every candidate fails, the final empty-input error remains actionable.
        if let Ok(mut found) = probe_source_file(&candidate) {
            inputs.append(&mut found);
        }
    }
    if inputs.is_empty() {
        return Err(format!(
            "No readable wrfout or native-level GRIB2 frames found in {}",
            path.display()
        ));
    }
    Ok(inputs)
}

fn looks_like_sequence_source(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let ext = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    name.starts_with("wrfout")
        || name.contains("wrfnat")
        || matches!(ext.as_str(), "nc" | "grib2" | "grb2" | "grib" | "grb")
}

fn probe_source_file(path: &Path) -> Result<Vec<FrameInput>, String> {
    if path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
    {
        let manifest = simsat::bricks::RunManifest::load(path)
            .map_err(|error| format!("Invalid SimSat run.json {}: {error}", path.display()))?;
        if manifest.timesteps.is_empty() {
            return Err(format!("SimSat run has no timesteps: {}", path.display()));
        }
        return Ok(manifest
            .timesteps
            .iter()
            .enumerate()
            .map(|(timestep, entry)| {
                let key = entry.time_iso.clone().unwrap_or_else(|| entry.key.clone());
                FrameInput {
                    path: path.to_owned(),
                    timestep,
                    sort_key: key.clone(),
                    label: key,
                }
            })
            .collect());
    }

    if simsat::ingest_grib::is_grib_input(path) {
        let probe = simsat::ingest_grib::probe_grib(path)
            .map_err(|error| format!("GRIB probe failed for {}: {error}", path.display()))?;
        return Ok(vec![FrameInput {
            path: path.to_owned(),
            timestep: 0,
            sort_key: probe.time_iso.clone(),
            label: probe.time_iso,
        }]);
    }

    let probe = simsat::ingest::probe_wrf(path)
        .map_err(|error| format!("WRF probe failed for {}: {error}", path.display()))?;
    let count = probe.nt.max(1);
    let fallback = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("wrfout")
        .to_owned();
    Ok((0..count)
        .map(|timestep| {
            let valid = probe
                .times
                .get(timestep)
                .cloned()
                .unwrap_or_else(|| format!("{fallback} t{timestep:04}"));
            FrameInput {
                path: path.to_owned(),
                timestep,
                sort_key: valid.clone(),
                label: valid,
            }
        })
        .collect())
}

fn sort_frame_inputs(inputs: &mut [FrameInput]) {
    inputs.sort_by(|left, right| {
        left.sort_key
            .cmp(&right.sort_key)
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.timestep.cmp(&right.timestep))
    });
}

fn write_result_to_store(
    job: &RenderJob,
    result: &RenderResult,
) -> Result<Option<StoredFrame>, String> {
    let hhmm = frame_hhmm(result);
    let common = (
        result.nx,
        result.ny,
        result.georef.lat.clone(),
        result.georef.lon.clone(),
        job.sector.clone(),
        job.satellite.api_satellite(),
        result.time.year,
        result.time.month,
        result.time.day,
        hhmm,
    );
    let written = if job.product.is_visible_family() {
        let FrameData::Visible { rgba, .. } = &result.data else {
            return Err("SimSat returned the wrong frame type for a visible product.".to_owned());
        };
        let n = result
            .nx
            .checked_mul(result.ny)
            .ok_or_else(|| "visible frame dimensions overflow.".to_owned())?;
        if rgba.len() != n * 4 {
            return Err(format!(
                "visible RGBA has {} bytes, expected {}",
                rgba.len(),
                n * 4
            ));
        }
        let mut rgb_r = vec![f32::NAN; n];
        let mut rgb_g = vec![f32::NAN; n];
        let mut rgb_b = vec![f32::NAN; n];
        for (index, pixel) in rgba.chunks_exact(4).enumerate() {
            if pixel[3] >= 128 {
                rgb_r[index] = pixel[0] as f32;
                rgb_g[index] = pixel[1] as f32;
                rgb_b[index] = pixel[2] as f32;
            }
        }
        let frame = VisibleFrame {
            nx: common.0,
            ny: common.1,
            rgb_r,
            rgb_g,
            rgb_b,
            lat: common.2,
            lon: common.3,
            sector: common.4,
            satellite: common.5,
            band: 2,
            year: common.6,
            month: common.7,
            day: common.8,
            hhmm: common.9,
        };
        Some(store_out::write_visible_frame(&job.store_root, &frame)?)
    } else if let Some(band) = job.product.thermal_band() {
        let FrameData::Ir { bt_kelvin, .. } = &result.data else {
            return Err("SimSat returned the wrong frame type for a thermal product.".to_owned());
        };
        let frame = IrFrame::new_band(
            common.0,
            common.1,
            bt_kelvin.clone(),
            common.2,
            common.3,
            common.4,
            common.5,
            band,
            common.6,
            common.7,
            common.8,
            common.9,
        );
        Some(store_out::write_ir_frame(&job.store_root, &frame)?)
    } else if let Some(expected_field) = job.product.derived_field() {
        let FrameData::Scalar { values, field, .. } = &result.data else {
            return Err("SimSat returned the wrong frame type for a derived product.".to_owned());
        };
        if *field != expected_field {
            return Err(format!(
                "SimSat returned derived field {} for requested {}",
                field.slug(),
                expected_field.slug()
            ));
        }
        let written = write_derived_frame(
            &job.store_root,
            &DerivedFrame {
                nx: common.0,
                ny: common.1,
                values: values.clone(),
                lat: common.2,
                lon: common.3,
                sector: common.4,
                satellite: common.5,
                field: *field,
                year: common.6,
                month: common.7,
                day: common.8,
                hhmm: common.9,
            },
        )?;
        return Ok(Some(StoredFrame {
            key: rw_ui::SatRunKey {
                model: written.model,
                run: written.run,
            },
            hhmm: written.hhmm,
        }));
    } else {
        None
    };
    Ok(written.map(|written| StoredFrame {
        key: rw_ui::SatRunKey {
            model: written.model,
            run: written.run,
        },
        hhmm: written.hhmm,
    }))
}

fn plot_payload_from_result(
    product: SimSatProduct,
    source_label: &str,
    result: RenderResult,
) -> Result<PlotPayload, String> {
    let subtitle_right = format!(
        "{:04}-{:02}-{:02} {:04} UTC · {}",
        result.time.year,
        result.time.month,
        result.time.day,
        frame_hhmm(&result),
        result.georef.view.slug()
    );
    plot_payload_from_parts(
        product,
        source_label.to_owned(),
        subtitle_right,
        result.nx,
        result.ny,
        result.data,
        result.georef.lat,
        result.georef.lon,
    )
}

#[allow(clippy::too_many_arguments)]
fn plot_payload_from_parts(
    product: SimSatProduct,
    subtitle_left: String,
    subtitle_right: String,
    nx: usize,
    ny: usize,
    data: FrameData,
    lat: Vec<f32>,
    lon: Vec<f32>,
) -> Result<PlotPayload, String> {
    let expected = nx
        .checked_mul(ny)
        .ok_or_else(|| "SimSat plot dimensions overflow.".to_owned())?;
    let expected_rgba = expected
        .checked_mul(4)
        .ok_or_else(|| "SimSat RGBA byte count overflows.".to_owned())?;
    if lat.len() != expected || lon.len() != expected {
        return Err(format!(
            "SimSat georef mesh is {} / {} values, expected {expected}",
            lat.len(),
            lon.len()
        ));
    }
    let pixels = match data {
        FrameData::Visible { rgba, .. } if product.is_visible_family() => {
            if rgba.len() != expected_rgba {
                return Err(format!(
                    "SimSat RGBA has {} bytes, expected {}",
                    rgba.len(),
                    expected_rgba
                ));
            }
            PlotPixels::Rgba(rgba)
        }
        FrameData::Ir { bt_kelvin, .. } if product.thermal_band().is_some() => {
            if bt_kelvin.len() != expected {
                return Err(format!(
                    "SimSat thermal field has {} values, expected {expected}",
                    bt_kelvin.len()
                ));
            }
            PlotPixels::Scalar {
                values: bt_kelvin,
                units: "K".to_owned(),
                palette: Some(SatellitePlotPalette::from_satellite_anchors(
                    rw_sat::palette::band_anchors(
                        product
                            .thermal_band()
                            .expect("thermal payload guard checked the band"),
                    ),
                )),
            }
        }
        FrameData::Scalar { values, field, .. }
            if matches!(product.api_product(), Product::Derived { .. }) =>
        {
            if values.len() != expected {
                return Err(format!(
                    "SimSat scalar field has {} values, expected {expected}",
                    values.len()
                ));
            }
            PlotPixels::Scalar {
                values,
                units: field.units().to_owned(),
                palette: Some(SatellitePlotPalette::from_simsat_derived(field)),
            }
        }
        _ => {
            return Err(
                "SimSat returned a frame type that does not match the requested product."
                    .to_owned(),
            );
        }
    };
    Ok(PlotPayload {
        title: format!("SimSat · {}", product.label()),
        subtitle_left,
        subtitle_right,
        nx,
        ny,
        lat,
        lon,
        pixels,
    })
}

fn result_warning(result: &RenderResult) -> Option<String> {
    let mut notices = vec![format!(
        "operator {}; storage {}",
        result.observation_operator,
        result.storage_profile.slug()
    )];
    if !result.intent_adjustments.is_empty() {
        notices.push(format!(
            "strict-intent adjustments: {}",
            result
                .intent_adjustments
                .iter()
                .map(|adjustment| adjustment.label())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(sensor) = result.thermal_sensor
        && sensor != ThermalSensor::FastGray
    {
        notices.push(format!("thermal response: {}", sensor.label()));
    }
    if result.instrument_footprint != InstrumentFootprint::Off {
        notices.push(format!(
            "instrument footprint: {}",
            result.instrument_footprint.label()
        ));
    }
    if result.topdown_cloud_footprint {
        notices.push("top-down cloud-radiance footprint applied".to_owned());
    }
    notices.extend(
        result
            .science_warnings
            .iter()
            .map(|warning| format!("science limitation: {warning}")),
    );
    if result.time_is_fallback {
        notices.push(
            "source had no parseable valid time; SimSat used its documented fallback date"
                .to_owned(),
        );
    }
    if !result.ground_status.is_empty() {
        notices.push(result.ground_status.join(" · "));
    }
    if result.res_clamped {
        notices
            .push("requested output resolution was capped with aspect ratio preserved".to_owned());
    }
    if result.backend == RenderBackend::GpuPreview {
        let adapter = result.gpu_adapter.as_deref().unwrap_or("unknown adapter");
        notices.push(format!(
            "GPU preview on {adapter}; opened in Native plot and not written to Satellite"
        ));
        if !result.diagnostics.is_empty() {
            notices.push(format!(
                "temporary preview substitutions: {}",
                result
                    .diagnostics
                    .iter()
                    .map(|adjustment| adjustment.label())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    (!notices.is_empty()).then(|| notices.join(" · "))
}

fn frame_hhmm(result: &RenderResult) -> u16 {
    let total_minutes = (result.time.ut * 60.0).round().clamp(0.0, 1439.0) as u16;
    (total_minutes / 60) * 100 + total_minutes % 60
}

/// One sector for the whole job. Product and view are always in the key, preventing
/// plain-visible/GeoColor/Sandwich overwrites at the same valid time.
fn qualified_sector(base: &str, product: SimSatProduct, view: OutputView) -> String {
    store_out::sanitize_store_token(&format!("{}_{}_{}", base, product.slug(), view.slug()))
}

fn source_group_base(path: &Path) -> String {
    if path.is_dir() {
        return path
            .file_name()
            .and_then(|value| value.to_str())
            .map(store_out::sanitize_store_token)
            .unwrap_or_else(|| "simsat".to_owned());
    }
    let raw = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("simsat");
    let lower = raw.to_ascii_lowercase();
    let cut = lower
        .find(".wrfnatf")
        .or_else(|| lower.find("_wrfnatf"))
        .or_else(|| lower.find("wrfnatf"))
        .or_else(|| lower.find("_20"))
        .or_else(|| lower.find("-20"))
        .unwrap_or(raw.len());
    let trimmed = raw[..cut]
        .trim_end_matches(['.', '_', '-'])
        .trim_end_matches("wrfout");
    let token = store_out::sanitize_store_token(trimmed);
    if token == "unknown" {
        "simsat".to_owned()
    } else {
        token
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_maps_every_public_product_to_the_expected_family() {
        assert!(matches!(
            SimSatProduct::Visible.api_product(),
            Product::VisibleRgb
        ));
        assert!(matches!(
            SimSatProduct::GeoColor.api_product(),
            Product::GeoColor
        ));
        assert!(matches!(
            SimSatProduct::Sandwich.api_product(),
            Product::Sandwich
        ));
        assert!(matches!(SimSatProduct::Ir13.api_product(), Product::Ir));
        assert!(matches!(
            SimSatProduct::Wv8.api_product(),
            Product::WaterVapor {
                band: WvBand::Upper
            }
        ));
        assert!(matches!(
            SimSatProduct::PrecipitableWater.api_product(),
            Product::Derived {
                field: DerivedField::PrecipitableWater
            }
        ));
        assert_eq!(SimSatProduct::Wv10.thermal_band(), Some(10));
        assert_eq!(SimSatProduct::CloudOpticalDepth.thermal_band(), None);
    }

    #[test]
    fn render_controls_map_to_the_released_simsat_api() {
        let job = RenderJob {
            source: JobSource::Local(PathBuf::from("wrfout")),
            backend: RenderBackend::GpuPreview,
            storage_profile: StorageProfile::ScienceCloudF16,
            render_intent: RenderIntent::SensorFastGray,
            product: SimSatProduct::Visible,
            view: OutputView::TopDown,
            satellite: SatelliteChoice::GoesWest,
            geo_navigation: GeoNavigation::GoesRAbiFixedGrid,
            resolution: ResolutionMode::Abi1km,
            quality: RenderQuality::Preview,
            margin_frac: 0.25,
            granulation: true,
            bluemarble_download: false,
            bluemarble_month: 1,
            exposure: 2.25,
            ground_gain: 1.25,
            cloud_softclip: 0.7,
            cloud_highlight_max: 1.75,
            aerosol_optical_depth: 0.12,
            rh_aerosol_swelling: true,
            atmosphere_correction: false,
            terrain_atmosphere: false,
            land_sza_normalization: false,
            land_sza_max_gain: 1.4,
            land_dark_toe: false,
            land_dark_toe_knee: 0.06,
            land_dark_toe_gamma: 0.75,
            land_dark_toe_max_gain: 1.3,
            clouds: true,
            fractional_clouds: false,
            fractional_cloud_mode: FractionalCloudMode::Deterministic8,
            cloud_optical_depth_scale: 0.42,
            cloud_optics: CloudOpticsMode::NsslNative,
            feather_exposed_domain_edges: false,
            cloud_transport: CloudMultiscatterMode::DeltaFluxV2,
            beer_powder: true,
            topdown_stratiform_regularization: true,
            topdown_cloud_footprint: true,
            thermal_sensor: ThermalSensor::GoesRAbiBand13Fm4,
            instrument_footprint: InstrumentFootprint::Off,
            sun_override: true,
            sun_elevation_deg: 35.0,
            sun_azimuth_deg: 225.0,
            cache_root: PathBuf::from("cache"),
            store_root: PathBuf::from("store"),
            sector: "test_visible_geo".to_owned(),
        };
        let frame = FrameInput {
            path: PathBuf::from("wrfout"),
            timestep: 3,
            sort_key: "time".to_owned(),
            label: "time".to_owned(),
        };
        let params = render_params_for(&job, &frame);
        assert_eq!(params.backend, RenderBackend::GpuPreview);
        assert_eq!(params.storage_profile, StorageProfile::ScienceCloudF16);
        assert_eq!(params.intent, RenderIntent::SensorFastGray);
        assert_eq!(params.satellite, SatellitePreset::GoesWest);
        assert_eq!(params.geo_navigation, GeoNavigation::GoesRAbiFixedGrid);
        assert_eq!(params.view, ViewMode::TopDownMap);
        assert_eq!(params.resolution, ResolutionMode::Abi1km);
        assert_eq!(params.timestep, 3);
        assert_eq!(params.margin_frac, 0.25);
        assert_eq!(params.exposure, 2.25);
        assert_eq!(params.ground_gain, Some(1.25));
        assert_eq!(params.cloud_softclip, Some(f64::from(0.7_f32)));
        assert_eq!(params.cloud_highlight_max, Some(1.75));
        assert_eq!(params.aerosol_optical_depth, 0.12);
        assert!(params.rh_aerosol_swelling);
        assert!(!params.atmosphere_correction);
        assert!(!params.terrain_atmosphere);
        assert!(!params.land_appearance.sza_normalization);
        assert!(!params.land_appearance.dark_toe);
        assert!(params.clouds);
        assert!(!params.multiscatter);
        assert_eq!(
            params.cloud_multiscatter,
            Some(CloudMultiscatterMode::DeltaFluxV2)
        );
        assert!(params.beer_powder);
        assert!(!params.fractional_clouds);
        assert_eq!(
            params.fractional_cloud_mode,
            FractionalCloudMode::Deterministic8
        );
        assert_eq!(params.cloud_optical_depth_scale, 0.42);
        assert_eq!(params.cloud_optics, CloudOpticsMode::NsslNative);
        assert!(!params.feather_exposed_domain_edges);
        assert_eq!(params.granulation, Some(true));
        assert!(params.topdown_stratiform_regularization);
        assert!(params.topdown_cloud_footprint);
        assert_eq!(params.thermal_sensor, ThermalSensor::GoesRAbiBand13Fm4);
        assert_eq!(params.instrument_footprint, InstrumentFootprint::Off);
        assert_eq!(
            params.sun_override,
            Some(SunOverride {
                elev_deg: Some(35.0),
                az_deg: Some(225.0),
            })
        );
        assert!(matches!(
            params.bluemarble,
            BlueMarble::Seasonal {
                month_override: Some(1),
                download: false,
            }
        ));
    }

    #[test]
    fn pane_defaults_match_the_simsat_shipped_preset() {
        let pane = SimSatPane::default();
        let land = LandAppearanceConfig::default();
        assert_eq!(pane.resolution, ResolutionMode::Native);
        assert_eq!(pane.quality, RenderQuality::Final);
        assert_eq!(pane.storage_profile, StorageProfile::CompactU8);
        assert_eq!(pane.render_intent, RenderIntent::Display);
        assert_eq!(pane.geo_navigation, GeoNavigation::ModelSphere);
        assert_eq!(pane.exposure, DEFAULT_EXPOSURE as f32);
        assert_eq!(
            pane.aerosol_optical_depth,
            simsat::atmosphere::DEFAULT_AOD as f32
        );
        assert!(!pane.rh_aerosol_swelling);
        assert!(pane.atmosphere_correction);
        assert!(pane.terrain_atmosphere);
        assert_eq!(pane.land_sza_normalization, land.sza_normalization);
        assert_eq!(pane.land_dark_toe, land.dark_toe);
        assert!(pane.clouds);
        assert!(pane.fractional_clouds);
        assert_eq!(pane.fractional_cloud_mode, FractionalCloudMode::EffectiveOd);
        assert_eq!(
            pane.cloud_optical_depth_scale,
            simsat::clouds::DEFAULT_CLOUD_OPTICAL_DEPTH_SCALE
        );
        assert!(pane.feather_exposed_domain_edges);
        assert_eq!(pane.cloud_transport, CloudMultiscatterMode::LegacyOctaves);
        assert_eq!(pane.cloud_optics, CloudOpticsMode::Fixed);
        assert!(!pane.beer_powder);
        assert!(!pane.granulation);
        assert!(!pane.topdown_stratiform_regularization);
        assert!(!pane.topdown_cloud_footprint);
        assert_eq!(pane.thermal_sensor, ThermalSensor::FastGray);
        assert_eq!(pane.instrument_footprint, InstrumentFootprint::Off);
    }

    #[test]
    fn sector_tokens_are_product_view_qualified_and_constant_source_based() {
        let visible = qualified_sector(
            "hrrr_20260710_t20z",
            SimSatProduct::Visible,
            OutputView::Geostationary,
        );
        let geocolor = qualified_sector(
            "hrrr_20260710_t20z",
            SimSatProduct::GeoColor,
            OutputView::Geostationary,
        );
        let topdown = qualified_sector(
            "hrrr_20260710_t20z",
            SimSatProduct::Visible,
            OutputView::TopDown,
        );
        assert_eq!(visible, "hrrr_20260710_t20z_visible_geo");
        assert_ne!(visible, geocolor);
        assert_ne!(visible, topdown);
        assert_eq!(
            source_group_base(Path::new("hrrr.t20z.wrfnatf17.grib2")),
            "hrrr_t20z"
        );
        assert_eq!(
            source_group_base(Path::new("wrfout_d03_2025-06-21_02-15-00")),
            "wrfout_d03"
        );
    }

    #[test]
    fn sequence_sort_is_valid_time_then_path_then_timestep() {
        let mut frames = vec![
            FrameInput {
                path: PathBuf::from("b"),
                timestep: 0,
                sort_key: "2026-07-10T03:00:00Z".to_owned(),
                label: "three".to_owned(),
            },
            FrameInput {
                path: PathBuf::from("a"),
                timestep: 1,
                sort_key: "2026-07-10T01:00:00Z".to_owned(),
                label: "one-b".to_owned(),
            },
            FrameInput {
                path: PathBuf::from("a"),
                timestep: 0,
                sort_key: "2026-07-10T01:00:00Z".to_owned(),
                label: "one-a".to_owned(),
            },
        ];
        sort_frame_inputs(&mut frames);
        assert_eq!(
            frames
                .iter()
                .map(|frame| frame.label.as_str())
                .collect::<Vec<_>>(),
            vec!["one-a", "one-b", "three"]
        );
    }

    #[test]
    fn sequence_discovery_accepts_any_extensionless_wrfout_name() {
        for name in ["wrfout", "wrfout_any_name_without_an_extension"] {
            assert!(
                looks_like_sequence_source(Path::new(name)),
                "extensionless WRF input must remain discoverable: {name}"
            );
        }
    }

    #[test]
    fn scalar_result_conversion_preserves_raw_values_and_mesh() {
        let payload = plot_payload_from_parts(
            SimSatProduct::PrecipitableWater,
            "source".to_owned(),
            "time".to_owned(),
            2,
            1,
            FrameData::Scalar {
                values: vec![12.5, f32::NAN],
                rgb: None,
                field: DerivedField::PrecipitableWater,
            },
            vec![40.0, 40.0],
            vec![-100.0, -99.0],
        )
        .unwrap();
        assert_eq!(payload.nx, 2);
        assert_eq!(payload.lat, vec![40.0, 40.0]);
        let PlotPixels::Scalar {
            values,
            units,
            palette,
        } = payload.pixels
        else {
            panic!("expected scalar payload");
        };
        assert_eq!(values[0], 12.5);
        assert!(values[1].is_nan());
        assert_eq!(units, "mm");
        assert!(palette.is_some(), "derived plots need a stable palette");
    }

    #[test]
    fn rgba_result_conversion_preserves_alpha_and_rejects_bad_shape() {
        let rgba = vec![10, 20, 30, 255, 40, 50, 60, 0];
        let payload = plot_payload_from_parts(
            SimSatProduct::Visible,
            "source".to_owned(),
            "time".to_owned(),
            2,
            1,
            FrameData::Visible {
                rgb: vec![10, 20, 30, 0, 0, 0],
                rgba: rgba.clone(),
            },
            vec![40.0, f32::NAN],
            vec![-100.0, f32::NAN],
        )
        .unwrap();
        assert_eq!(payload.pixels, PlotPixels::Rgba(rgba));

        let error = plot_payload_from_parts(
            SimSatProduct::Visible,
            "source".to_owned(),
            "time".to_owned(),
            2,
            1,
            FrameData::Visible {
                rgb: Vec::new(),
                rgba: vec![0; 4],
            },
            vec![40.0, 40.0],
            vec![-100.0, -99.0],
        )
        .unwrap_err();
        assert!(error.contains("expected 8"));

        let payload = PlotPayload {
            title: "bad rgba".to_owned(),
            subtitle_left: String::new(),
            subtitle_right: String::new(),
            nx: 1,
            ny: 1,
            lat: vec![40.0],
            lon: vec![-100.0],
            pixels: PlotPixels::Rgba(vec![1, 2, 3, 4, 5]),
        };
        assert!(payload.to_plot_source().unwrap_err().contains("expected 4"));
    }

    #[test]
    fn quick_modes_apply_reviewed_values_and_refuse_mislabeled_sensor_qa() {
        let mut pane = SimSatPane::default();
        pane.apply_quick_mode(SimSatQuickMode::HighQualityVisible)
            .unwrap();
        assert_eq!(
            pane.active_quick_mode(),
            Some(SimSatQuickMode::HighQualityVisible)
        );
        assert_eq!(
            pane.fractional_cloud_mode,
            FractionalCloudMode::Deterministic4
        );
        assert!(near_f32(pane.cloud_softclip, 0.45));

        pane.product = SimSatProduct::Ir13;
        pane.satellite = SatelliteChoice::GoesWest;
        assert!(
            pane.apply_quick_mode(SimSatQuickMode::SensorQa)
                .unwrap_err()
                .contains("GOES-East")
        );
        pane.satellite = SatelliteChoice::GoesEast;
        pane.apply_quick_mode(SimSatQuickMode::SensorQa).unwrap();
        assert_eq!(pane.active_quick_mode(), Some(SimSatQuickMode::SensorQa));
        assert_eq!(pane.geo_navigation, GeoNavigation::GoesRAbiFixedGrid);
        assert_eq!(pane.resolution, ResolutionMode::Abi2km);
        assert_eq!(pane.thermal_sensor, ThermalSensor::GoesRAbiBand13Fm4);
    }

    #[test]
    fn persisted_state_round_trips_controls_but_not_source_or_runtime() {
        let mut pane = SimSatPane {
            product: SimSatProduct::Ir13,
            satellite: SatelliteChoice::GoesEast,
            ..SimSatPane::default()
        };
        pane.apply_quick_mode(SimSatQuickMode::SensorQa).unwrap();
        pane.local_path = "C:/private/wrfout".to_owned();
        let saved = pane.take_persisted_state_if_dirty().unwrap();

        let restored = SimSatPane::new(Some(&saved));
        assert_eq!(restored.product, SimSatProduct::Ir13);
        assert_eq!(restored.render_intent, RenderIntent::SensorFastGray);
        assert_eq!(restored.thermal_sensor, ThermalSensor::GoesRAbiBand13Fm4);
        assert!(restored.local_path.is_empty());
        assert!(restored.task.is_none());
        assert!(!restored.persisted_state_dirty);
    }
}
