//! Streaming WRF-column adapter for model-atmosphere radar refraction.
//!
//! This module deliberately stops at a reusable profile/trace boundary. It
//! does not choose a simulated-radar propagation mode or mutate renderer
//! configuration. The real adapter reads one raw WRF field at a time, keeps
//! only four weighted vertical columns, and clears wrf-core's cache after each
//! read. Full thermodynamic volumes therefore do not survive profile creation.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use radar_core::{
    GateRange, PropagationRegime, RefractedBeamError, RefractedBeamPoint, RefractivityLevel,
    RefractivityProfile, RefractivityProfileError, propagation_regime, radio_refractivity_n_units,
    trace_refracted_beam,
};
use thiserror::Error;
use wrf_core::WrfFile;

use crate::model_layer::{neighboring_cell_starts, solve_bilinear_coords, unwrap_lon_near};

const WRF_REFERENCE_PRESSURE_PA: f64 = 100_000.0;
const WRF_KAPPA: f64 = 0.285_714_285_7;
const MAX_QVAPOR_DRY_MIXING_RATIO_KGKG: f64 = 0.2;
const MIN_TEMPERATURE_K: f64 = 100.0;
const MAX_TEMPERATURE_K: f64 = 400.0;
const MAX_ANTENNA_EXTRAPOLATION_M: f64 = 2_000.0;
const MIN_PROFILE_TOP_ABOVE_RADAR_M: f64 = 1_000.0;
const DEFAULT_TRACE_STEP_M: f64 = 250.0;
const MAX_TRACE_CACHE_ENTRIES: usize = 128;

#[derive(Clone, Copy)]
pub struct WrfRefractivityGrid<'a> {
    pub nx: usize,
    pub ny: usize,
    pub nz: usize,
    /// WRF mass-grid latitude, row-major `[ny, nx]`.
    pub latitude_deg: &'a [f32],
    /// WRF mass-grid longitude, row-major `[ny, nx]`.
    pub longitude_deg: &'a [f32],
    /// WRF mass-level height MSL, row-major `[nz, ny, nx]`.
    pub height_msl: &'a [f32],
    pub site_latitude_deg: f64,
    pub site_longitude_deg: f64,
    pub antenna_msl_m: f64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HumidityConversion {
    /// WRF `QVAPOR` is water-vapour mixing ratio per unit dry-air mass,
    /// `r`; ITU refractivity accepts moist-air specific humidity,
    /// `q = r / (1 + r)`.
    DryAirMixingRatioToMoistSpecificHumidity,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HorizontalColumnProvenance {
    pub cell_x: usize,
    pub cell_y: usize,
    pub corner_indices: [usize; 4],
    pub corner_weights: [f64; 4],
    pub site_latitude_deg: f64,
    pub site_longitude_deg: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RefractivityGradientProvenance {
    pub antenna_gradient_n_per_km: f64,
    pub antenna_regime: PropagationRegime,
    pub minimum_gradient_n_per_km: f64,
    pub maximum_gradient_n_per_km: f64,
    pub contains_ducting_layer: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WrfRefractivityProvenance {
    pub source: String,
    pub time_index: usize,
    pub pressure_source: &'static str,
    pub temperature_source: &'static str,
    pub humidity_source: &'static str,
    pub humidity_conversion: HumidityConversion,
    pub formula: &'static str,
    pub horizontal: HorizontalColumnProvenance,
    pub antenna_msl_m: f64,
    pub antenna_level_inserted: bool,
    pub antenna_extrapolation_m: f64,
    pub retained_level_count: usize,
    pub lowest_height_above_radar_m: f64,
    pub highest_height_above_radar_m: f64,
    pub gradients: RefractivityGradientProvenance,
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum WrfRefractivityError {
    #[error("WRF refractivity grid has invalid shape nx={nx}, ny={ny}, nz={nz}")]
    InvalidGridShape { nx: usize, ny: usize, nz: usize },
    #[error("WRF refractivity grid dimensions overflow")]
    GridSizeOverflow,
    #[error("WRF refractivity {field} has {actual} values, expected {expected}")]
    GridLength {
        field: &'static str,
        actual: usize,
        expected: usize,
    },
    #[error("WRF time index {index} is outside {count} times")]
    TimeOutOfRange { index: usize, count: usize },
    #[error("WRF refractivity requires source field {0}")]
    MissingField(&'static str),
    #[error("read WRF refractivity field {field}: {detail}")]
    FieldRead { field: &'static str, detail: String },
    #[error("radar site coordinates are invalid: lat={latitude_deg}, lon={longitude_deg}")]
    InvalidSite {
        latitude_deg: f64,
        longitude_deg: f64,
    },
    #[error("radar antenna MSL altitude is invalid: {0} m")]
    InvalidAntennaAltitude(f64),
    #[error("radar site lies outside a valid WRF horizontal grid cell")]
    SiteOutsideHorizontalGrid,
    #[error("WRF {field} is invalid at model level {level}: {value}")]
    InvalidLevelValue {
        field: &'static str,
        level: usize,
        value: f64,
    },
    #[error(
        "WRF mass-level height does not increase at model level {level}: {previous_m} then {current_m} m MSL"
    )]
    NonIncreasingHeight {
        level: usize,
        previous_m: f64,
        current_m: f64,
    },
    #[error(
        "radar antenna is {gap_m:.1} m below the first WRF mass level; maximum supported lower extrapolation is {maximum_m:.1} m"
    )]
    AntennaBelowProfile { gap_m: f64, maximum_m: f64 },
    #[error(
        "WRF refractivity column ends at {top_m:.1} m above radar; at least {minimum_m:.1} m is required"
    )]
    InsufficientVerticalCoverage { top_m: f64, minimum_m: f64 },
    #[error(transparent)]
    Profile(#[from] RefractivityProfileError),
    #[error("radar refractivity trace step must be finite in (0, 10000] m, got {0}")]
    InvalidTraceStep(f64),
    #[error(
        "invalid radar gate range: first={first_gate_m} m, spacing={gate_spacing_m} m, count={gate_count}"
    )]
    InvalidGateRange {
        first_gate_m: i32,
        gate_spacing_m: i32,
        gate_count: usize,
    },
    #[error("radar gate range overflows slant-range arithmetic")]
    GateRangeOverflow,
    #[error(transparent)]
    Trace(#[from] RefractedBeamError),
    #[error(
        "refracted ray leaves model profile coverage at {slant_range_m:.1} m slant range: height {height_m:.1} m, profile [{bottom_m:.1}, {top_m:.1}] m above radar"
    )]
    TraceLeavesProfile {
        slant_range_m: f64,
        height_m: f64,
        bottom_m: f64,
        top_m: f64,
    },
    #[error("refractivity trace cache mutex is poisoned")]
    CachePoisoned,
}

/// Pure provider seam. Implementations return one raw WRF field for one time;
/// the reader extracts its four site columns immediately and drops it before
/// requesting the next field.
pub trait RefractivityFieldProvider {
    fn source_label(&self) -> String;
    fn shape(&self) -> (usize, usize, usize);
    fn time_count(&self) -> usize;
    fn has_field(&self, name: &str) -> bool;
    fn read_field(&self, name: &'static str, time_index: usize) -> Result<Vec<f64>, String>;
    fn clear_cache(&self);
}

pub struct WrfFileRefractivityProvider<'a> {
    file: &'a WrfFile,
}

impl<'a> WrfFileRefractivityProvider<'a> {
    #[must_use]
    pub const fn new(file: &'a WrfFile) -> Self {
        Self { file }
    }
}

impl RefractivityFieldProvider for WrfFileRefractivityProvider<'_> {
    fn source_label(&self) -> String {
        self.file.path.display().to_string()
    }

    fn shape(&self) -> (usize, usize, usize) {
        (self.file.nx, self.file.ny, self.file.nz)
    }

    fn time_count(&self) -> usize {
        self.file.nt
    }

    fn has_field(&self, name: &str) -> bool {
        self.file.has_var(name)
    }

    fn read_field(&self, name: &'static str, time_index: usize) -> Result<Vec<f64>, String> {
        self.file
            .read_var(name, time_index)
            .map_err(|error| error.to_string())
    }

    fn clear_cache(&self) {
        self.file.clear_cache();
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RefractedGateTrace {
    pub elevation_deg: f64,
    pub gate_range: GateRange,
    /// One interpolated ray point per gate, aligned exactly with `gate_range`.
    pub points: Vec<RefractedBeamPoint>,
    pub encountered_ducting_layer: bool,
    pub minimum_gradient_n_per_km: f64,
    pub antenna_msl_m: f64,
}

impl RefractedGateTrace {
    #[must_use]
    pub fn point(&self, gate_index: usize) -> Option<&RefractedBeamPoint> {
        self.points.get(gate_index)
    }

    #[must_use]
    #[cfg(test)]
    pub fn beam_height_msl_at_gate(&self, gate_index: usize) -> Option<f64> {
        self.point(gate_index)
            .map(|point| self.antenna_msl_m + point.height_above_radar_m)
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct GateTraceKey {
    elevation_bits: u64,
    first_gate_m: i32,
    gate_spacing_m: i32,
    gate_count: usize,
}

#[derive(Default)]
struct TraceCache {
    values: BTreeMap<GateTraceKey, Arc<RefractedGateTrace>>,
    insertion_order: VecDeque<GateTraceKey>,
}

impl TraceCache {
    fn insert(&mut self, key: GateTraceKey, value: Arc<RefractedGateTrace>) {
        if !self.values.contains_key(&key) {
            self.insertion_order.push_back(key);
        }
        self.values.insert(key, value);
        while self.values.len() > MAX_TRACE_CACHE_ENTRIES {
            if let Some(oldest) = self.insertion_order.pop_front() {
                self.values.remove(&oldest);
            } else {
                break;
            }
        }
    }
}

/// Compact production result: one vertical profile, provenance, and a bounded
/// cache of per-elevation/per-gate-layout traces. No 3-D thermodynamic field is
/// retained here.
pub struct WrfRefractivityModel {
    profile: Arc<RefractivityProfile>,
    provenance: WrfRefractivityProvenance,
    trace_step_m: f64,
    cache: Mutex<TraceCache>,
}

impl WrfRefractivityModel {
    #[must_use]
    pub fn profile(&self) -> &RefractivityProfile {
        &self.profile
    }

    #[must_use]
    pub const fn provenance(&self) -> &WrfRefractivityProvenance {
        &self.provenance
    }

    pub fn trace_for_gate_range(
        &self,
        elevation_deg: f64,
        gate_range: &GateRange,
    ) -> Result<Arc<RefractedGateTrace>, WrfRefractivityError> {
        validate_gate_range(gate_range)?;
        let key = GateTraceKey {
            elevation_bits: elevation_deg.to_bits(),
            first_gate_m: gate_range.first_gate_m,
            gate_spacing_m: gate_range.gate_spacing_m,
            gate_count: gate_range.gate_count,
        };
        if let Some(trace) = self
            .cache
            .lock()
            .map_err(|_| WrfRefractivityError::CachePoisoned)?
            .values
            .get(&key)
            .cloned()
        {
            return Ok(trace);
        }

        let last_gate = gate_range.gate_count - 1;
        let last_range_m = gate_slant_range_m(gate_range, last_gate)?;
        let step_m = self.trace_step_m.min(f64::from(gate_range.gate_spacing_m));
        let raw = trace_refracted_beam(&self.profile, elevation_deg, last_range_m, step_m)?;
        let bottom_m = self
            .profile
            .levels()
            .first()
            .map_or(0.0, |level| level.height_m);
        let top_m = self
            .profile
            .levels()
            .last()
            .map_or(0.0, |level| level.height_m);
        for point in &raw.points {
            if point.height_above_radar_m < bottom_m - 1.0e-6
                || point.height_above_radar_m > top_m + 1.0e-6
            {
                return Err(WrfRefractivityError::TraceLeavesProfile {
                    slant_range_m: point.slant_range_m,
                    height_m: point.height_above_radar_m,
                    bottom_m,
                    top_m,
                });
            }
        }
        let points = (0..gate_range.gate_count)
            .map(|gate| {
                let slant_m = gate_slant_range_m(gate_range, gate)?;
                interpolate_trace_point(&raw.points, slant_m)
                    .ok_or(WrfRefractivityError::GateRangeOverflow)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let trace = Arc::new(RefractedGateTrace {
            elevation_deg,
            gate_range: gate_range.clone(),
            points,
            encountered_ducting_layer: raw.encountered_ducting_layer,
            minimum_gradient_n_per_km: raw.minimum_gradient_n_per_km,
            antenna_msl_m: self.provenance.antenna_msl_m,
        });
        self.cache
            .lock()
            .map_err(|_| WrfRefractivityError::CachePoisoned)?
            .insert(key, Arc::clone(&trace));
        Ok(trace)
    }

    #[cfg(test)]
    pub fn beam_height_msl_at_gate(
        &self,
        elevation_deg: f64,
        gate_range: &GateRange,
        gate_index: usize,
    ) -> Result<f64, WrfRefractivityError> {
        if gate_index >= gate_range.gate_count {
            return Err(WrfRefractivityError::InvalidGateRange {
                first_gate_m: gate_range.first_gate_m,
                gate_spacing_m: gate_range.gate_spacing_m,
                gate_count: gate_range.gate_count,
            });
        }
        self.trace_for_gate_range(elevation_deg, gate_range)?
            .beam_height_msl_at_gate(gate_index)
            .ok_or(WrfRefractivityError::GateRangeOverflow)
    }

    #[cfg(test)]
    pub fn cached_trace_count(&self) -> Result<usize, WrfRefractivityError> {
        Ok(self
            .cache
            .lock()
            .map_err(|_| WrfRefractivityError::CachePoisoned)?
            .values
            .len())
    }
}

/// Build the compact refractivity model from an already-open WRF file.
pub fn read_wrf_refractivity_model(
    file: &WrfFile,
    time_index: usize,
    grid: WrfRefractivityGrid<'_>,
) -> Result<WrfRefractivityModel, WrfRefractivityError> {
    read_wrf_refractivity_model_with_step(file, time_index, grid, DEFAULT_TRACE_STEP_M)
}

pub fn read_wrf_refractivity_model_with_step(
    file: &WrfFile,
    time_index: usize,
    grid: WrfRefractivityGrid<'_>,
    trace_step_m: f64,
) -> Result<WrfRefractivityModel, WrfRefractivityError> {
    read_refractivity_model(
        &WrfFileRefractivityProvider::new(file),
        time_index,
        grid,
        trace_step_m,
    )
}

pub fn read_refractivity_model<P: RefractivityFieldProvider + ?Sized>(
    provider: &P,
    time_index: usize,
    grid: WrfRefractivityGrid<'_>,
    trace_step_m: f64,
) -> Result<WrfRefractivityModel, WrfRefractivityError> {
    if !trace_step_m.is_finite() || trace_step_m <= 0.0 || trace_step_m > 10_000.0 {
        return Err(WrfRefractivityError::InvalidTraceStep(trace_step_m));
    }
    let (cells, volume_cells) = validate_grid(provider, time_index, grid)?;
    let stencil = horizontal_stencil(grid)?;
    let _cache_guard = ProviderCacheGuard(provider);
    for name in ["P", "PB", "T", "QVAPOR"] {
        if !provider.has_field(name) {
            return Err(WrfRefractivityError::MissingField(name));
        }
    }

    let p_perturbation = read_corner_columns(
        provider,
        "P",
        time_index,
        volume_cells,
        cells,
        grid.nz,
        &stencil,
    )?;
    let p_base = read_corner_columns(
        provider,
        "PB",
        time_index,
        volume_cells,
        cells,
        grid.nz,
        &stencil,
    )?;
    let theta_perturbation = read_corner_columns(
        provider,
        "T",
        time_index,
        volume_cells,
        cells,
        grid.nz,
        &stencil,
    )?;
    let qvapor_dry = read_corner_columns(
        provider,
        "QVAPOR",
        time_index,
        volume_cells,
        cells,
        grid.nz,
        &stencil,
    )?;
    let height = corner_columns_from_f32(
        grid.height_msl,
        volume_cells,
        cells,
        grid.nz,
        &stencil,
        "height_msl",
    )?;

    let mut levels = Vec::with_capacity(grid.nz + 1);
    let mut previous_height_msl = f64::NEG_INFINITY;
    for level in 0..grid.nz {
        let mut pressure_corners = [0.0; 4];
        let mut temperature_corners = [0.0; 4];
        let mut specific_humidity_corners = [0.0; 4];
        for corner in 0..4 {
            let pressure_pa = p_perturbation[level][corner] + p_base[level][corner];
            validate_positive(level, "pressure", pressure_pa)?;
            let theta_k = theta_perturbation[level][corner] + 300.0;
            validate_positive(level, "potential temperature", theta_k)?;
            let temperature_k = theta_k * (pressure_pa / WRF_REFERENCE_PRESSURE_PA).powf(WRF_KAPPA);
            if !temperature_k.is_finite()
                || !(MIN_TEMPERATURE_K..=MAX_TEMPERATURE_K).contains(&temperature_k)
            {
                return Err(WrfRefractivityError::InvalidLevelValue {
                    field: "temperature",
                    level,
                    value: temperature_k,
                });
            }
            let dry_mixing_ratio = qvapor_dry[level][corner];
            if !dry_mixing_ratio.is_finite()
                || !(0.0..=MAX_QVAPOR_DRY_MIXING_RATIO_KGKG).contains(&dry_mixing_ratio)
            {
                return Err(WrfRefractivityError::InvalidLevelValue {
                    field: "QVAPOR dry-air mixing ratio",
                    level,
                    value: dry_mixing_ratio,
                });
            }
            pressure_corners[corner] = pressure_pa;
            temperature_corners[corner] = temperature_k;
            specific_humidity_corners[corner] = dry_mixing_ratio / (1.0 + dry_mixing_ratio);
        }
        let height_msl = weighted(height[level], stencil.weights);
        if !height_msl.is_finite() {
            return Err(WrfRefractivityError::InvalidLevelValue {
                field: "height_msl",
                level,
                value: height_msl,
            });
        }
        if level > 0 && height_msl <= previous_height_msl {
            return Err(WrfRefractivityError::NonIncreasingHeight {
                level,
                previous_m: previous_height_msl,
                current_m: height_msl,
            });
        }
        previous_height_msl = height_msl;
        let pressure_pa = weighted(pressure_corners, stencil.weights);
        let temperature_k = weighted(temperature_corners, stencil.weights);
        let specific_humidity = weighted(specific_humidity_corners, stencil.weights);
        let refractivity_n =
            radio_refractivity_n_units(pressure_pa, temperature_k, specific_humidity).ok_or(
                WrfRefractivityError::InvalidLevelValue {
                    field: "radio refractivity",
                    level,
                    value: f64::NAN,
                },
            )?;
        levels.push(RefractivityLevel {
            height_m: height_msl - grid.antenna_msl_m,
            refractivity_n,
        });
    }

    let (levels, antenna_level_inserted, antenna_extrapolation_m) = insert_antenna_level(levels)?;
    let profile = RefractivityProfile::new(levels)?;
    let top_m = profile.levels().last().map_or(0.0, |level| level.height_m);
    if top_m < MIN_PROFILE_TOP_ABOVE_RADAR_M {
        return Err(WrfRefractivityError::InsufficientVerticalCoverage {
            top_m,
            minimum_m: MIN_PROFILE_TOP_ABOVE_RADAR_M,
        });
    }
    let gradients = gradient_provenance(&profile);
    let provenance = WrfRefractivityProvenance {
        source: provider.source_label(),
        time_index,
        pressure_source: "P + PB on WRF mass levels",
        temperature_source: "(T + 300 K) * ((P + PB) / 100000 Pa)^0.2857142857",
        humidity_source: "QVAPOR (kg water vapor per kg dry air)",
        humidity_conversion: HumidityConversion::DryAirMixingRatioToMoistSpecificHumidity,
        formula: "ITU-R P.453: N = 77.6 p/T + 3.732e5 e/T^2",
        horizontal: HorizontalColumnProvenance {
            cell_x: stencil.cell_x,
            cell_y: stencil.cell_y,
            corner_indices: stencil.indices,
            corner_weights: stencil.weights,
            site_latitude_deg: grid.site_latitude_deg,
            site_longitude_deg: grid.site_longitude_deg,
        },
        antenna_msl_m: grid.antenna_msl_m,
        antenna_level_inserted,
        antenna_extrapolation_m,
        retained_level_count: profile.levels().len(),
        lowest_height_above_radar_m: profile.levels().first().map_or(0.0, |level| level.height_m),
        highest_height_above_radar_m: top_m,
        gradients,
    };
    Ok(WrfRefractivityModel {
        profile: Arc::new(profile),
        provenance,
        trace_step_m,
        cache: Mutex::new(TraceCache::default()),
    })
}

struct ProviderCacheGuard<'a, P: RefractivityFieldProvider + ?Sized>(&'a P);

impl<P: RefractivityFieldProvider + ?Sized> Drop for ProviderCacheGuard<'_, P> {
    fn drop(&mut self) {
        self.0.clear_cache();
    }
}

#[derive(Clone, Copy, Debug)]
struct HorizontalStencil {
    cell_x: usize,
    cell_y: usize,
    indices: [usize; 4],
    weights: [f64; 4],
}

fn validate_grid<P: RefractivityFieldProvider + ?Sized>(
    provider: &P,
    time_index: usize,
    grid: WrfRefractivityGrid<'_>,
) -> Result<(usize, usize), WrfRefractivityError> {
    if grid.nx < 2 || grid.ny < 2 || grid.nz < 2 {
        return Err(WrfRefractivityError::InvalidGridShape {
            nx: grid.nx,
            ny: grid.ny,
            nz: grid.nz,
        });
    }
    if provider.shape() != (grid.nx, grid.ny, grid.nz) {
        let (nx, ny, nz) = provider.shape();
        return Err(WrfRefractivityError::InvalidGridShape { nx, ny, nz });
    }
    if time_index >= provider.time_count() {
        return Err(WrfRefractivityError::TimeOutOfRange {
            index: time_index,
            count: provider.time_count(),
        });
    }
    if !grid.site_latitude_deg.is_finite()
        || !(-90.0..=90.0).contains(&grid.site_latitude_deg)
        || !grid.site_longitude_deg.is_finite()
    {
        return Err(WrfRefractivityError::InvalidSite {
            latitude_deg: grid.site_latitude_deg,
            longitude_deg: grid.site_longitude_deg,
        });
    }
    if !grid.antenna_msl_m.is_finite() {
        return Err(WrfRefractivityError::InvalidAntennaAltitude(
            grid.antenna_msl_m,
        ));
    }
    let cells = grid
        .nx
        .checked_mul(grid.ny)
        .ok_or(WrfRefractivityError::GridSizeOverflow)?;
    let volume_cells = cells
        .checked_mul(grid.nz)
        .ok_or(WrfRefractivityError::GridSizeOverflow)?;
    for (field, actual, expected) in [
        ("latitude", grid.latitude_deg.len(), cells),
        ("longitude", grid.longitude_deg.len(), cells),
        ("height_msl", grid.height_msl.len(), volume_cells),
    ] {
        if actual != expected {
            return Err(WrfRefractivityError::GridLength {
                field,
                actual,
                expected,
            });
        }
    }
    Ok((cells, volume_cells))
}

fn horizontal_stencil(
    grid: WrfRefractivityGrid<'_>,
) -> Result<HorizontalStencil, WrfRefractivityError> {
    let target_lat = grid.site_latitude_deg;
    let target_lon = grid.site_longitude_deg;
    let cosine = target_lat.to_radians().cos().abs().max(0.05);
    let nearest = grid
        .latitude_deg
        .iter()
        .zip(grid.longitude_deg)
        .enumerate()
        .filter_map(|(index, (&latitude, &longitude))| {
            let latitude = f64::from(latitude);
            let longitude = unwrap_lon_near(f64::from(longitude), target_lon);
            if !latitude.is_finite() || !longitude.is_finite() {
                return None;
            }
            let distance =
                (latitude - target_lat).powi(2) + ((longitude - target_lon) * cosine).powi(2);
            Some((index, distance))
        })
        .min_by(|left, right| left.1.total_cmp(&right.1))
        .map(|(index, _)| index)
        .ok_or(WrfRefractivityError::SiteOutsideHorizontalGrid)?;
    let row = nearest / grid.nx;
    let column = nearest % grid.nx;
    for cell_y in neighboring_cell_starts(row, grid.ny).into_iter().flatten() {
        for cell_x in neighboring_cell_starts(column, grid.nx)
            .into_iter()
            .flatten()
        {
            let i00 = cell_y * grid.nx + cell_x;
            let indices = [i00, i00 + 1, i00 + grid.nx, i00 + grid.nx + 1];
            let mut corners = [(0.0, 0.0); 4];
            let mut valid = true;
            for (output, index) in corners.iter_mut().zip(indices) {
                let latitude = f64::from(grid.latitude_deg[index]);
                let longitude = unwrap_lon_near(f64::from(grid.longitude_deg[index]), target_lon);
                if !latitude.is_finite() || !longitude.is_finite() {
                    valid = false;
                    break;
                }
                *output = (longitude, latitude);
            }
            if !valid {
                continue;
            }
            let Some((u, v)) = solve_bilinear_coords(corners, target_lon, target_lat) else {
                continue;
            };
            if !((-0.02..=1.02).contains(&u) && (-0.02..=1.02).contains(&v)) {
                continue;
            }
            let u = u.clamp(0.0, 1.0);
            let v = v.clamp(0.0, 1.0);
            return Ok(HorizontalStencil {
                cell_x,
                cell_y,
                indices,
                weights: [(1.0 - u) * (1.0 - v), u * (1.0 - v), (1.0 - u) * v, u * v],
            });
        }
    }
    Err(WrfRefractivityError::SiteOutsideHorizontalGrid)
}

fn read_corner_columns<P: RefractivityFieldProvider + ?Sized>(
    provider: &P,
    field: &'static str,
    time_index: usize,
    expected: usize,
    cells: usize,
    nz: usize,
    stencil: &HorizontalStencil,
) -> Result<Vec<[f64; 4]>, WrfRefractivityError> {
    let result = provider.read_field(field, time_index);
    provider.clear_cache();
    let values = result.map_err(|detail| WrfRefractivityError::FieldRead { field, detail })?;
    if values.len() != expected {
        return Err(WrfRefractivityError::GridLength {
            field,
            actual: values.len(),
            expected,
        });
    }
    corner_columns(&values, cells, nz, stencil, field)
}

fn corner_columns_from_f32(
    values: &[f32],
    expected: usize,
    cells: usize,
    nz: usize,
    stencil: &HorizontalStencil,
    field: &'static str,
) -> Result<Vec<[f64; 4]>, WrfRefractivityError> {
    if values.len() != expected {
        return Err(WrfRefractivityError::GridLength {
            field,
            actual: values.len(),
            expected,
        });
    }
    let mut column = Vec::with_capacity(nz);
    for level in 0..nz {
        let base = level
            .checked_mul(cells)
            .ok_or(WrfRefractivityError::GridSizeOverflow)?;
        let mut corners = [0.0; 4];
        for (output, horizontal_index) in corners.iter_mut().zip(stencil.indices) {
            let index = base
                .checked_add(horizontal_index)
                .ok_or(WrfRefractivityError::GridSizeOverflow)?;
            let value = values.get(index).copied().map(f64::from).ok_or(
                WrfRefractivityError::GridLength {
                    field,
                    actual: values.len(),
                    expected: nz.saturating_mul(cells),
                },
            )?;
            if !value.is_finite() {
                return Err(WrfRefractivityError::InvalidLevelValue {
                    field,
                    level,
                    value,
                });
            }
            *output = value;
        }
        column.push(corners);
    }
    Ok(column)
}

fn corner_columns(
    values: &[f64],
    cells: usize,
    nz: usize,
    stencil: &HorizontalStencil,
    field: &'static str,
) -> Result<Vec<[f64; 4]>, WrfRefractivityError> {
    let mut column = Vec::with_capacity(nz);
    for level in 0..nz {
        let base = level
            .checked_mul(cells)
            .ok_or(WrfRefractivityError::GridSizeOverflow)?;
        let mut corners = [0.0; 4];
        for (output, horizontal_index) in corners.iter_mut().zip(stencil.indices) {
            let index = base
                .checked_add(horizontal_index)
                .ok_or(WrfRefractivityError::GridSizeOverflow)?;
            let value = values
                .get(index)
                .copied()
                .ok_or(WrfRefractivityError::GridLength {
                    field,
                    actual: values.len(),
                    expected: nz.saturating_mul(cells),
                })?;
            if !value.is_finite() {
                return Err(WrfRefractivityError::InvalidLevelValue {
                    field,
                    level,
                    value,
                });
            }
            *output = value;
        }
        column.push(corners);
    }
    Ok(column)
}

fn weighted(values: [f64; 4], weights: [f64; 4]) -> f64 {
    values
        .into_iter()
        .zip(weights)
        .map(|(value, weight)| value * weight)
        .sum()
}

fn validate_positive(
    level: usize,
    field: &'static str,
    value: f64,
) -> Result<(), WrfRefractivityError> {
    if value.is_finite() && value > 0.0 {
        Ok(())
    } else {
        Err(WrfRefractivityError::InvalidLevelValue {
            field,
            level,
            value,
        })
    }
}

fn insert_antenna_level(
    mut levels: Vec<RefractivityLevel>,
) -> Result<(Vec<RefractivityLevel>, bool, f64), WrfRefractivityError> {
    if levels.len() < 2 {
        return Err(RefractivityProfileError::TooFewLevels.into());
    }
    if levels.last().is_none_or(|level| level.height_m <= 0.0) {
        return Err(WrfRefractivityError::InsufficientVerticalCoverage {
            top_m: levels
                .last()
                .map_or(f64::NEG_INFINITY, |level| level.height_m),
            minimum_m: MIN_PROFILE_TOP_ABOVE_RADAR_M,
        });
    }
    if let Some(index) = levels
        .iter()
        .position(|level| level.height_m.abs() <= 1.0e-6)
    {
        levels[index].height_m = 0.0;
        return Ok((levels, false, 0.0));
    }
    let upper = levels.partition_point(|level| level.height_m < 0.0);
    if upper == 0 {
        let gap_m = levels[0].height_m;
        if gap_m > MAX_ANTENNA_EXTRAPOLATION_M {
            return Err(WrfRefractivityError::AntennaBelowProfile {
                gap_m,
                maximum_m: MAX_ANTENNA_EXTRAPOLATION_M,
            });
        }
        let lower = levels[0];
        let upper_level = levels[1];
        let gradient = (upper_level.refractivity_n - lower.refractivity_n)
            / (upper_level.height_m - lower.height_m);
        levels.insert(
            0,
            RefractivityLevel {
                height_m: 0.0,
                refractivity_n: lower.refractivity_n - lower.height_m * gradient,
            },
        );
        return Ok((levels, true, gap_m));
    }
    if upper < levels.len() {
        let lower = levels[upper - 1];
        let upper_level = levels[upper];
        let alpha = -lower.height_m / (upper_level.height_m - lower.height_m);
        levels.insert(
            upper,
            RefractivityLevel {
                height_m: 0.0,
                refractivity_n: lower.refractivity_n
                    + alpha * (upper_level.refractivity_n - lower.refractivity_n),
            },
        );
        return Ok((levels, true, 0.0));
    }
    Err(WrfRefractivityError::InsufficientVerticalCoverage {
        top_m: levels
            .last()
            .map_or(f64::NEG_INFINITY, |level| level.height_m),
        minimum_m: MIN_PROFILE_TOP_ABOVE_RADAR_M,
    })
}

fn gradient_provenance(profile: &RefractivityProfile) -> RefractivityGradientProvenance {
    let mut minimum = f64::INFINITY;
    let mut maximum = f64::NEG_INFINITY;
    let mut ducting = false;
    for levels in profile.levels().windows(2) {
        let gradient = 1_000.0 * (levels[1].refractivity_n - levels[0].refractivity_n)
            / (levels[1].height_m - levels[0].height_m);
        minimum = minimum.min(gradient);
        maximum = maximum.max(gradient);
        ducting |= propagation_regime(gradient) == PropagationRegime::Ducting;
    }
    let antenna_gradient = profile.gradient_n_per_km_at(0.0);
    RefractivityGradientProvenance {
        antenna_gradient_n_per_km: antenna_gradient,
        antenna_regime: propagation_regime(antenna_gradient),
        minimum_gradient_n_per_km: minimum,
        maximum_gradient_n_per_km: maximum,
        contains_ducting_layer: ducting,
    }
}

fn validate_gate_range(gate_range: &GateRange) -> Result<(), WrfRefractivityError> {
    if gate_range.first_gate_m < 0 || gate_range.gate_spacing_m <= 0 || gate_range.gate_count == 0 {
        return Err(WrfRefractivityError::InvalidGateRange {
            first_gate_m: gate_range.first_gate_m,
            gate_spacing_m: gate_range.gate_spacing_m,
            gate_count: gate_range.gate_count,
        });
    }
    gate_slant_range_m(gate_range, gate_range.gate_count - 1).map(|_| ())
}

fn gate_slant_range_m(
    gate_range: &GateRange,
    gate_index: usize,
) -> Result<f64, WrfRefractivityError> {
    if gate_index >= gate_range.gate_count {
        return Err(WrfRefractivityError::InvalidGateRange {
            first_gate_m: gate_range.first_gate_m,
            gate_spacing_m: gate_range.gate_spacing_m,
            gate_count: gate_range.gate_count,
        });
    }
    let offset = i64::from(gate_range.gate_spacing_m)
        .checked_mul(
            i64::try_from(gate_index).map_err(|_| WrfRefractivityError::GateRangeOverflow)?,
        )
        .ok_or(WrfRefractivityError::GateRangeOverflow)?;
    let range = i64::from(gate_range.first_gate_m)
        .checked_add(offset)
        .ok_or(WrfRefractivityError::GateRangeOverflow)?;
    Ok(range as f64)
}

fn interpolate_trace_point(
    points: &[RefractedBeamPoint],
    slant_range_m: f64,
) -> Option<RefractedBeamPoint> {
    let first = *points.first()?;
    if slant_range_m <= first.slant_range_m {
        return Some(first);
    }
    let last = *points.last()?;
    if slant_range_m >= last.slant_range_m {
        return Some(last);
    }
    let upper = points.partition_point(|point| point.slant_range_m < slant_range_m);
    let lower = points[upper - 1];
    let upper = points[upper];
    let alpha = (slant_range_m - lower.slant_range_m) / (upper.slant_range_m - lower.slant_range_m);
    let interpolate = |left: f64, right: f64| left + alpha * (right - left);
    let gradient = interpolate(lower.gradient_n_per_km, upper.gradient_n_per_km);
    Some(RefractedBeamPoint {
        slant_range_m,
        ground_range_m: interpolate(lower.ground_range_m, upper.ground_range_m),
        height_above_radar_m: interpolate(lower.height_above_radar_m, upper.height_above_radar_m),
        elevation_deg: interpolate(lower.elevation_deg, upper.elevation_deg),
        refractivity_n: interpolate(lower.refractivity_n, upper.refractivity_n),
        gradient_n_per_km: gradient,
        regime: propagation_regime(gradient),
    })
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use radar_core::STANDARD_REFRACTIVITY_GRADIENT_N_PER_KM;

    use super::*;

    struct FakeProvider {
        source: String,
        nx: usize,
        ny: usize,
        nz: usize,
        nt: usize,
        fields: BTreeMap<&'static str, Vec<f64>>,
        reads: RefCell<Vec<&'static str>>,
        clears: Cell<usize>,
    }

    impl RefractivityFieldProvider for FakeProvider {
        fn source_label(&self) -> String {
            self.source.clone()
        }

        fn shape(&self) -> (usize, usize, usize) {
            (self.nx, self.ny, self.nz)
        }

        fn time_count(&self) -> usize {
            self.nt
        }

        fn has_field(&self, name: &str) -> bool {
            self.fields.contains_key(name)
        }

        fn read_field(&self, name: &'static str, _time_index: usize) -> Result<Vec<f64>, String> {
            self.reads.borrow_mut().push(name);
            self.fields
                .get(name)
                .cloned()
                .ok_or_else(|| format!("missing {name}"))
        }

        fn clear_cache(&self) {
            self.clears.set(self.clears.get() + 1);
        }
    }

    fn repeated_levels(values: &[f64], cells: usize) -> Vec<f64> {
        values
            .iter()
            .flat_map(|value| std::iter::repeat_n(*value, cells))
            .collect()
    }

    fn provider_and_grid() -> (FakeProvider, Vec<f32>, Vec<f32>, Vec<f32>) {
        let (nx, ny, nz) = (2, 2, 3);
        let cells = nx * ny;
        let pressure: [f64; 3] = [100_000.0, 90_000.0, 55_000.0];
        let temperature: [f64; 3] = [290.0, 283.0, 255.0];
        let specific_humidity: [f64; 3] = [0.010, 0.006, 0.001];
        let perturbation_pressure = repeated_levels(&[2_000.0, 1_500.0, 500.0], cells);
        let base_pressure = pressure
            .iter()
            .zip([2_000.0, 1_500.0, 500.0])
            .map(|(full, perturbation)| *full - perturbation)
            .collect::<Vec<_>>();
        let base_pressure = repeated_levels(&base_pressure, cells);
        let theta_perturbation = pressure
            .iter()
            .zip(temperature)
            .map(|(pressure, temperature)| {
                temperature / (*pressure / WRF_REFERENCE_PRESSURE_PA).powf(WRF_KAPPA) - 300.0
            })
            .collect::<Vec<_>>();
        let theta_perturbation = repeated_levels(&theta_perturbation, cells);
        let dry_mixing = specific_humidity
            .iter()
            .map(|q| *q / (1.0 - *q))
            .collect::<Vec<_>>();
        let dry_mixing = repeated_levels(&dry_mixing, cells);
        let fields = BTreeMap::from([
            ("P", perturbation_pressure),
            ("PB", base_pressure),
            ("T", theta_perturbation),
            ("QVAPOR", dry_mixing),
        ]);
        let latitude = vec![39.0, 39.0, 40.0, 40.0];
        let longitude = vec![-98.0, -97.0, -98.0, -97.0];
        let height = repeated_levels(&[100.0, 1_000.0, 5_000.0], cells)
            .into_iter()
            .map(|value| value as f32)
            .collect();
        (
            FakeProvider {
                source: "pure-provider".to_owned(),
                nx,
                ny,
                nz,
                nt: 1,
                fields,
                reads: RefCell::new(Vec::new()),
                clears: Cell::new(0),
            },
            latitude,
            longitude,
            height,
        )
    }

    #[test]
    fn provider_extracts_only_site_column_and_converts_qvapor_basis() {
        let (provider, latitude, longitude, height) = provider_and_grid();
        let model = read_refractivity_model(
            &provider,
            0,
            WrfRefractivityGrid {
                nx: 2,
                ny: 2,
                nz: 3,
                latitude_deg: &latitude,
                longitude_deg: &longitude,
                height_msl: &height,
                site_latitude_deg: 39.5,
                site_longitude_deg: -97.5,
                antenna_msl_m: 50.0,
            },
            250.0,
        )
        .unwrap();

        assert_eq!(&*provider.reads.borrow(), &["P", "PB", "T", "QVAPOR"]);
        assert!(provider.clears.get() >= 5);
        assert_eq!(
            model.provenance().humidity_conversion,
            HumidityConversion::DryAirMixingRatioToMoistSpecificHumidity
        );
        assert_eq!(model.provenance().retained_level_count, 4);
        assert_eq!(model.profile().levels()[0].height_m, 0.0);
        let expected = radio_refractivity_n_units(100_000.0, 290.0, 0.010).unwrap();
        assert!((model.profile().levels()[1].refractivity_n - expected).abs() < 1.0e-9);
        assert_eq!(model.provenance().horizontal.corner_weights, [0.25; 4]);
    }

    fn constant_gradient_model(gradient_n_per_km: f64) -> WrfRefractivityModel {
        let profile = RefractivityProfile::new(vec![
            RefractivityLevel {
                height_m: 0.0,
                refractivity_n: 320.0,
            },
            RefractivityLevel {
                height_m: 10_000.0,
                refractivity_n: 320.0 + 10.0 * gradient_n_per_km,
            },
        ])
        .unwrap();
        let gradients = gradient_provenance(&profile);
        WrfRefractivityModel {
            profile: Arc::new(profile),
            provenance: WrfRefractivityProvenance {
                source: "pure-profile".to_owned(),
                time_index: 0,
                pressure_source: "test",
                temperature_source: "test",
                humidity_source: "test",
                humidity_conversion: HumidityConversion::DryAirMixingRatioToMoistSpecificHumidity,
                formula: "test",
                horizontal: HorizontalColumnProvenance {
                    cell_x: 0,
                    cell_y: 0,
                    corner_indices: [0, 1, 2, 3],
                    corner_weights: [0.25; 4],
                    site_latitude_deg: 0.5,
                    site_longitude_deg: 0.5,
                },
                antenna_msl_m: 100.0,
                antenna_level_inserted: false,
                antenna_extrapolation_m: 0.0,
                retained_level_count: 2,
                lowest_height_above_radar_m: 0.0,
                highest_height_above_radar_m: 10_000.0,
                gradients,
            },
            trace_step_m: 100.0,
            cache: Mutex::new(TraceCache::default()),
        }
    }

    #[test]
    fn provenance_distinguishes_standard_superrefractive_and_ducting() {
        let standard = constant_gradient_model(STANDARD_REFRACTIVITY_GRADIENT_N_PER_KM);
        let superrefractive = constant_gradient_model(-100.0);
        let ducting = constant_gradient_model(-180.0);
        assert_eq!(
            standard.provenance().gradients.antenna_regime,
            PropagationRegime::NearStandard
        );
        assert_eq!(
            superrefractive.provenance().gradients.antenna_regime,
            PropagationRegime::Superrefractive
        );
        assert_eq!(
            ducting.provenance().gradients.antenna_regime,
            PropagationRegime::Ducting
        );
        assert!(!standard.provenance().gradients.contains_ducting_layer);
        assert!(ducting.provenance().gradients.contains_ducting_layer);
    }

    #[test]
    fn gate_trace_is_interpolated_once_and_reused_from_bounded_cache() {
        let model = constant_gradient_model(STANDARD_REFRACTIVITY_GRADIENT_N_PER_KM);
        let gates = GateRange {
            first_gate_m: 125,
            gate_spacing_m: 250,
            gate_count: 201,
        };
        let first = model.trace_for_gate_range(0.5, &gates).unwrap();
        let second = model.trace_for_gate_range(0.5, &gates).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(first.points.len(), gates.gate_count);
        assert_eq!(first.points[0].slant_range_m, 125.0);
        assert_eq!(model.cached_trace_count().unwrap(), 1);
        assert_eq!(
            model.beam_height_msl_at_gate(0.5, &gates, 0).unwrap(),
            100.0 + first.points[0].height_above_radar_m
        );
    }

    #[test]
    fn missing_horizontal_or_thermodynamic_coverage_fails_closed() {
        let (mut provider, latitude, longitude, height) = provider_and_grid();
        let outside = read_refractivity_model(
            &provider,
            0,
            WrfRefractivityGrid {
                nx: 2,
                ny: 2,
                nz: 3,
                latitude_deg: &latitude,
                longitude_deg: &longitude,
                height_msl: &height,
                site_latitude_deg: 50.0,
                site_longitude_deg: -90.0,
                antenna_msl_m: 50.0,
            },
            250.0,
        );
        assert!(matches!(
            outside,
            Err(WrfRefractivityError::SiteOutsideHorizontalGrid)
        ));

        provider.fields.get_mut("QVAPOR").unwrap()[0] = -0.01;
        let invalid = read_refractivity_model(
            &provider,
            0,
            WrfRefractivityGrid {
                nx: 2,
                ny: 2,
                nz: 3,
                latitude_deg: &latitude,
                longitude_deg: &longitude,
                height_msl: &height,
                site_latitude_deg: 39.0,
                site_longitude_deg: -98.0,
                antenna_msl_m: 50.0,
            },
            250.0,
        );
        assert!(matches!(
            invalid,
            Err(WrfRefractivityError::InvalidLevelValue {
                field: "QVAPOR dry-air mixing ratio",
                ..
            })
        ));
    }
}
