//! Pure temporal planning plus strict raw-state interpolation primitives for
//! WRF simulated radar.
//!
//! Timed acquisition and atmosphere sampling are separate choices. The legacy
//! renderer remains frozen at the volume start; both adjacent-time modes
//! require a compatible later WRF scene covering the whole scan. Interpolation
//! weights are bounded per ray with no extrapolation. [`interpolate_raw_gate`]
//! is the generic raw-atmosphere contract, while
//! [`interpolate_raw_state_linear`] also blends scheme-native P3/ISHMAEL
//! properties before nonlinear closure/scattering. The compatibility renderer
//! can use the same plan for linear Z, wind, and additive scattering
//! quantities, but must label that interpolation space explicitly rather than
//! claiming raw state.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::hash::Hash;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::wrf_property_reader::{
    RawPropertyBlendError, RawPropertyCell, WeightedRawPropertyCell, WrfPropertyReadError,
    WrfPropertyScene, blend_raw_property_cells,
};
use crate::wrf_scene_inventory::{WrfSceneGroup, WrfSceneLocator};

pub const MAX_RAW_STATE_SPATIAL_WEIGHTS_PER_SCENE: usize = 8;
const RAW_STATE_WEIGHT_SUM_TOLERANCE: f64 = 1.0e-9;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AtmosphereTimeMode {
    /// Bit-compatible legacy behavior: every ray samples the anchor scene.
    #[default]
    FrozenAtVolumeStart,
    /// Interpolate a declared linear atmosphere/scattering representation from
    /// the anchor to the next compatible model time using each ray's
    /// acquisition time. Consumers must stamp which representation they used.
    LinearAdjacent,
    /// Interpolate raw thermodynamics, dynamics, and scheme-native
    /// microphysics before nonlinear closure and scattering. This mode is
    /// fail-closed when the adjacent raw inventories are not identical.
    RawStateLinear,
}

impl AtmosphereTimeMode {
    /// Concise user-facing label which does not overstate the interpolation
    /// space used by the legacy derived-field path.
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::FrozenAtVolumeStart => "Frozen at volume start",
            Self::LinearAdjacent => "Linear adjacent (derived/additive)",
            Self::RawStateLinear => "Raw-state linear (pre-closure)",
        }
    }

    /// Stable value for scan provenance. `linear_adjacent` deliberately stays
    /// unchanged for settings and files written before RawStateLinear existed.
    #[must_use]
    pub const fn provenance_name(self) -> &'static str {
        match self {
            Self::FrozenAtVolumeStart => "frozen_at_volume_start",
            Self::LinearAdjacent => "linear_adjacent",
            Self::RawStateLinear => "raw_state_linear",
        }
    }

    /// Scientific space in which the temporal blend occurs.
    #[must_use]
    pub const fn interpolation_space_name(self) -> &'static str {
        match self {
            Self::FrozenAtVolumeStart => "anchor model state",
            Self::LinearAdjacent => "linear Z, winds, and additive scattering quantities",
            Self::RawStateLinear => {
                "raw thermodynamics, dynamics, and scheme-native microphysics before closure/scattering"
            }
        }
    }

    #[must_use]
    pub const fn uses_adjacent_scene(self) -> bool {
        matches!(self, Self::LinearAdjacent | Self::RawStateLinear)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissingNeighborPolicy {
    /// Build from the anchor scene and record that temporal sampling was held.
    #[default]
    HoldAnchor,
    /// Omit the volume rather than claim time interpolation.
    DropFrame,
    /// Stop the run with a readable error.
    Error,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HoldReason {
    NoLaterScene,
    ScanCrossesNeighbor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TemporalSamplingOutcome {
    Frozen,
    LinearAdjacent,
    HeldAnchor(HoldReason),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TemporalScenePlan {
    pub anchor: WrfSceneLocator,
    pub anchor_time: DateTime<Utc>,
    pub neighbor: Option<WrfSceneLocator>,
    pub neighbor_time: Option<DateTime<Utc>>,
    pub scan_duration_ms: i64,
    /// Retained so provenance can distinguish a derived/additive blend from a
    /// raw-state pre-closure blend even though both use the same time bracket.
    pub mode: AtmosphereTimeMode,
    pub outcome: TemporalSamplingOutcome,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TemporalPlanError {
    AnchorOutOfRange {
        index: usize,
        scene_count: usize,
    },
    UntimedAnchor {
        anchor: WrfSceneLocator,
    },
    NegativeScanDuration {
        milliseconds: i64,
    },
    MissingNeighbor {
        anchor: WrfSceneLocator,
    },
    ScanCrossesNeighbor {
        anchor: WrfSceneLocator,
        scan_end: DateTime<Utc>,
        neighbor_time: DateTime<Utc>,
    },
    RayBeforeAnchor {
        offset_ms: i64,
    },
    RayPastScan {
        offset_ms: i64,
        scan_duration_ms: i64,
    },
    RayPastNeighbor {
        offset_ms: i64,
        bracket_ms: i64,
    },
    DegenerateBracket,
}

impl fmt::Display for TemporalPlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AnchorOutOfRange { index, scene_count } => {
                write!(f, "WRF scene index {index} is outside {scene_count} scenes")
            }
            Self::UntimedAnchor { anchor } => write!(
                f,
                "WRF scene {} time {} has no valid internal or filename time",
                anchor.path.display(),
                anchor.time_index
            ),
            Self::NegativeScanDuration { milliseconds } => {
                write!(f, "scan duration cannot be negative ({milliseconds} ms)")
            }
            Self::MissingNeighbor { anchor } => write!(
                f,
                "WRF scene {} time {} has no later compatible scene",
                anchor.path.display(),
                anchor.time_index
            ),
            Self::ScanCrossesNeighbor {
                scan_end,
                neighbor_time,
                ..
            } => write!(
                f,
                "scan ends at {scan_end}, beyond the next WRF time {neighbor_time}"
            ),
            Self::RayBeforeAnchor { offset_ms } => {
                write!(f, "ray offset {offset_ms} ms is before the anchor scene")
            }
            Self::RayPastScan {
                offset_ms,
                scan_duration_ms,
            } => write!(
                f,
                "ray offset {offset_ms} ms exceeds the {scan_duration_ms} ms scan plan"
            ),
            Self::RayPastNeighbor {
                offset_ms,
                bracket_ms,
            } => write!(
                f,
                "ray offset {offset_ms} ms exceeds the {bracket_ms} ms WRF bracket"
            ),
            Self::DegenerateBracket => write!(f, "WRF scene bracket has no positive duration"),
        }
    }
}

impl std::error::Error for TemporalPlanError {}

/// Choose the temporal bracket for one output volume. `None` means the
/// configured policy deliberately dropped the frame.
pub fn plan_for_scene(
    group: &WrfSceneGroup,
    anchor_index: usize,
    scan_duration: Duration,
    mode: AtmosphereTimeMode,
    policy: MissingNeighborPolicy,
) -> Result<Option<TemporalScenePlan>, TemporalPlanError> {
    let Some(anchor) = group.scenes.get(anchor_index) else {
        return Err(TemporalPlanError::AnchorOutOfRange {
            index: anchor_index,
            scene_count: group.scenes.len(),
        });
    };
    let anchor_time =
        anchor
            .time
            .valid_time()
            .cloned()
            .ok_or_else(|| TemporalPlanError::UntimedAnchor {
                anchor: anchor.locator(),
            })?;
    let scan_duration_ms = scan_duration.num_milliseconds();
    if scan_duration_ms < 0 {
        return Err(TemporalPlanError::NegativeScanDuration {
            milliseconds: scan_duration_ms,
        });
    }

    if !mode.uses_adjacent_scene() {
        return Ok(Some(TemporalScenePlan {
            anchor: anchor.locator(),
            anchor_time,
            neighbor: None,
            neighbor_time: None,
            scan_duration_ms,
            mode,
            outcome: TemporalSamplingOutcome::Frozen,
        }));
    }

    let neighbor = group.scenes[anchor_index + 1..].iter().find(|scene| {
        scene
            .time
            .valid_time()
            .is_some_and(|time| time > &anchor_time)
    });
    let Some(neighbor) = neighbor else {
        return missing_neighbor(anchor.locator(), policy, HoldReason::NoLaterScene).map(
            |choice| {
                choice.map(|outcome| TemporalScenePlan {
                    anchor: anchor.locator(),
                    anchor_time,
                    neighbor: None,
                    neighbor_time: None,
                    scan_duration_ms,
                    mode,
                    outcome,
                })
            },
        );
    };
    let neighbor_time = neighbor
        .time
        .valid_time()
        .cloned()
        .expect("neighbor search kept only timed scenes");
    let scan_end = anchor_time.to_owned() + scan_duration;
    if scan_end > neighbor_time {
        return match policy {
            MissingNeighborPolicy::HoldAnchor => Ok(Some(TemporalScenePlan {
                anchor: anchor.locator(),
                anchor_time,
                // Retain the discovered boundary in provenance even though
                // this policy samples the anchor at alpha=0 for every ray.
                neighbor: Some(neighbor.locator()),
                neighbor_time: Some(neighbor_time),
                scan_duration_ms,
                mode,
                outcome: TemporalSamplingOutcome::HeldAnchor(HoldReason::ScanCrossesNeighbor),
            })),
            MissingNeighborPolicy::DropFrame => Ok(None),
            MissingNeighborPolicy::Error => Err(TemporalPlanError::ScanCrossesNeighbor {
                anchor: anchor.locator(),
                scan_end,
                neighbor_time: neighbor_time.to_owned(),
            }),
        };
    }

    Ok(Some(TemporalScenePlan {
        anchor: anchor.locator(),
        anchor_time,
        neighbor: Some(neighbor.locator()),
        neighbor_time: Some(neighbor_time),
        scan_duration_ms,
        mode,
        outcome: TemporalSamplingOutcome::LinearAdjacent,
    }))
}

fn missing_neighbor(
    anchor: WrfSceneLocator,
    policy: MissingNeighborPolicy,
    reason: HoldReason,
) -> Result<Option<TemporalSamplingOutcome>, TemporalPlanError> {
    match policy {
        MissingNeighborPolicy::HoldAnchor => Ok(Some(TemporalSamplingOutcome::HeldAnchor(reason))),
        MissingNeighborPolicy::DropFrame => Ok(None),
        MissingNeighborPolicy::Error => Err(TemporalPlanError::MissingNeighbor { anchor }),
    }
}

impl TemporalScenePlan {
    /// One bounded interpolation weight for an entire ray. Frozen/held plans
    /// always return zero. Linear plans reject extrapolation.
    pub fn ray_alpha(&self, acquisition_offset_ms: i64) -> Result<f64, TemporalPlanError> {
        if acquisition_offset_ms < 0 {
            return Err(TemporalPlanError::RayBeforeAnchor {
                offset_ms: acquisition_offset_ms,
            });
        }
        if acquisition_offset_ms > self.scan_duration_ms {
            return Err(TemporalPlanError::RayPastScan {
                offset_ms: acquisition_offset_ms,
                scan_duration_ms: self.scan_duration_ms,
            });
        }
        if self.outcome != TemporalSamplingOutcome::LinearAdjacent {
            return Ok(0.0);
        }
        let neighbor_time = self
            .neighbor_time
            .as_ref()
            .expect("linear temporal plan carries a neighbor time");
        let bracket_ms =
            (neighbor_time.to_owned() - self.anchor_time.to_owned()).num_milliseconds();
        if bracket_ms <= 0 {
            return Err(TemporalPlanError::DegenerateBracket);
        }
        if acquisition_offset_ms > bracket_ms {
            return Err(TemporalPlanError::RayPastNeighbor {
                offset_ms: acquisition_offset_ms,
                bracket_ms,
            });
        }
        Ok(acquisition_offset_ms as f64 / bracket_ms as f64)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScenePropertySignature {
    pub microphysics_scheme_id: Option<i32>,
    pub reflectivity_source: String,
    pub required_raw_fields: BTreeSet<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PropertyCompatibility {
    Compatible,
    SchemeMismatch {
        left: Option<i32>,
        right: Option<i32>,
    },
    ReflectivitySourceMismatch {
        left: String,
        right: String,
    },
    RawFieldInventoryMismatch {
        only_left: BTreeSet<String>,
        only_right: BTreeSet<String>,
    },
}

pub fn assess_property_compatibility(
    left: &ScenePropertySignature,
    right: &ScenePropertySignature,
) -> PropertyCompatibility {
    if left.microphysics_scheme_id != right.microphysics_scheme_id {
        return PropertyCompatibility::SchemeMismatch {
            left: left.microphysics_scheme_id,
            right: right.microphysics_scheme_id,
        };
    }
    if left.reflectivity_source != right.reflectivity_source {
        return PropertyCompatibility::ReflectivitySourceMismatch {
            left: left.reflectivity_source.clone(),
            right: right.reflectivity_source.clone(),
        };
    }
    if left.required_raw_fields != right.required_raw_fields {
        return PropertyCompatibility::RawFieldInventoryMismatch {
            only_left: left
                .required_raw_fields
                .difference(&right.required_raw_fields)
                .cloned()
                .collect(),
            only_right: right
                .required_raw_fields
                .difference(&left.required_raw_fields)
                .cloned()
                .collect(),
        };
    }
    PropertyCompatibility::Compatible
}

/// Raw state sampled at one physical gate location in one WRF scene. The
/// named fields contain hydrometeor mass, number, and scheme-native property
/// variables. They must match exactly across a temporal bracket.
#[derive(Clone, Debug, PartialEq)]
pub struct RawGateState {
    pub wind_u_mps: f32,
    pub wind_v_mps: f32,
    pub wind_w_mps: f32,
    pub temperature_k: f32,
    pub pressure_pa: f32,
    pub air_density_kgm3: f32,
    pub fields: BTreeMap<String, f32>,
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum RawInterpolationError {
    #[error("raw-state interpolation alpha {0} is outside the closed interval [0, 1]")]
    AlphaOutOfRange(f64),
    #[error("raw-state interpolation has no left model coverage")]
    MissingLeftCoverage,
    #[error("raw-state interpolation has no right model coverage")]
    MissingRightCoverage,
    #[error("raw-state field inventory differs between model times")]
    FieldInventoryMismatch,
    #[error("raw-state field {field} is non-finite")]
    NonFiniteValue { field: String },
}

fn validate_raw_gate_state(state: &RawGateState) -> Result<(), RawInterpolationError> {
    for (field, value) in [
        ("wind_u_mps", state.wind_u_mps),
        ("wind_v_mps", state.wind_v_mps),
        ("wind_w_mps", state.wind_w_mps),
        ("temperature_k", state.temperature_k),
        ("pressure_pa", state.pressure_pa),
        ("air_density_kgm3", state.air_density_kgm3),
    ] {
        if !value.is_finite() {
            return Err(RawInterpolationError::NonFiniteValue {
                field: field.to_owned(),
            });
        }
    }
    if let Some((field, _)) = state.fields.iter().find(|(_, value)| !value.is_finite()) {
        return Err(RawInterpolationError::NonFiniteValue {
            field: field.clone(),
        });
    }
    Ok(())
}

/// Interpolate two raw gate states. Missing domain coverage is never treated
/// as clear air; exact endpoints need only their endpoint scene.
pub fn interpolate_raw_gate(
    left: Option<&RawGateState>,
    right: Option<&RawGateState>,
    alpha: f64,
) -> Result<RawGateState, RawInterpolationError> {
    if !alpha.is_finite() || !(0.0..=1.0).contains(&alpha) {
        return Err(RawInterpolationError::AlphaOutOfRange(alpha));
    }
    if alpha == 0.0 {
        let left = left.ok_or(RawInterpolationError::MissingLeftCoverage)?;
        validate_raw_gate_state(left)?;
        return Ok(left.clone());
    }
    if alpha == 1.0 {
        let right = right.ok_or(RawInterpolationError::MissingRightCoverage)?;
        validate_raw_gate_state(right)?;
        return Ok(right.clone());
    }
    let left = left.ok_or(RawInterpolationError::MissingLeftCoverage)?;
    let right = right.ok_or(RawInterpolationError::MissingRightCoverage)?;
    if left.fields.keys().ne(right.fields.keys()) {
        return Err(RawInterpolationError::FieldInventoryMismatch);
    }

    let blend = |name: &str, a: f32, b: f32| -> Result<f32, RawInterpolationError> {
        if !a.is_finite() || !b.is_finite() {
            return Err(RawInterpolationError::NonFiniteValue {
                field: name.to_owned(),
            });
        }
        Ok((a as f64 + (b as f64 - a as f64) * alpha) as f32)
    };
    let mut fields = BTreeMap::new();
    for ((name, &a), (_, &b)) in left.fields.iter().zip(&right.fields) {
        fields.insert(name.clone(), blend(name, a, b)?);
    }
    Ok(RawGateState {
        wind_u_mps: blend("wind_u_mps", left.wind_u_mps, right.wind_u_mps)?,
        wind_v_mps: blend("wind_v_mps", left.wind_v_mps, right.wind_v_mps)?,
        wind_w_mps: blend("wind_w_mps", left.wind_w_mps, right.wind_w_mps)?,
        temperature_k: blend("temperature_k", left.temperature_k, right.temperature_k)?,
        pressure_pa: blend("pressure_pa", left.pressure_pa, right.pressure_pa)?,
        air_density_kgm3: blend(
            "air_density_kgm3",
            left.air_density_kgm3,
            right.air_density_kgm3,
        )?,
        fields,
    })
}

/// One covered endpoint for raw-state temporal interpolation. The property
/// scene owns normalized scheme-native P3/ISHMAEL fields; `gate_state` carries
/// the collocated thermodynamics/dynamics used by the renderer.
#[derive(Clone, Copy, Debug)]
pub struct RawStateLinearEndpoint<'a> {
    pub property_scene: &'a WrfPropertyScene,
    property_coverage: RawPropertyCoverage<'a>,
    pub gate_state: &'a RawGateState,
}

#[derive(Clone, Copy, Debug)]
enum RawPropertyCoverage<'a> {
    Single(usize),
    Weighted(&'a [(usize, f64)]),
}

impl<'a> RawStateLinearEndpoint<'a> {
    #[must_use]
    pub const fn new(
        property_scene: &'a WrfPropertyScene,
        property_cell_index: usize,
        gate_state: &'a RawGateState,
    ) -> Self {
        Self {
            property_scene,
            property_coverage: RawPropertyCoverage::Single(property_cell_index),
            gate_state,
        }
    }

    /// One endpoint sampled from its actual spatial interpolation stencil.
    /// Weights must be normalized, positive-coverage contributions and may
    /// contain at most the eight nodes of one trilinear WRF stencil.
    #[must_use]
    pub const fn with_spatial_weights(
        property_scene: &'a WrfPropertyScene,
        property_weights: &'a [(usize, f64)],
        gate_state: &'a RawGateState,
    ) -> Self {
        Self {
            property_scene,
            property_coverage: RawPropertyCoverage::Weighted(property_weights),
            gate_state,
        }
    }
}

/// Raw, pre-closure gate state ready for exactly one nonlinear closure and
/// scattering evaluation.
#[derive(Clone, Debug, PartialEq)]
pub struct RawStateLinearCell {
    pub property_cell: RawPropertyCell,
    pub gate_state: RawGateState,
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum RawStateLinearError {
    #[error("raw-state temporal alpha {0} is outside the closed interval [0, 1]")]
    AlphaOutOfRange(f64),
    #[error("raw-state temporal interpolation has no left model coverage")]
    MissingLeftCoverage,
    #[error("raw-state temporal interpolation has no right model coverage")]
    MissingRightCoverage,
    #[error("raw-state temporal microphysics scheme differs: left {left}, right {right}")]
    SchemeMismatch { left: i32, right: i32 },
    #[error("raw-state temporal required-field inventory differs between model times")]
    PropertyInventoryMismatch,
    #[error("raw-state temporal property category layout differs between model times")]
    PropertyCategoryLayoutMismatch,
    #[error("raw-state temporal rain availability differs between model times")]
    RainAvailabilityMismatch,
    #[error("raw-state endpoint has {actual} spatial weights; maximum is {maximum}")]
    TooManySpatialWeights { actual: usize, maximum: usize },
    #[error("raw-state endpoint spatial coverage is empty")]
    EmptySpatialWeights,
    #[error(
        "raw-state endpoint spatial weight {weight} at index {index} must be finite and nonnegative"
    )]
    InvalidSpatialWeight { index: usize, weight: f64 },
    #[error("raw-state endpoint spatial weights sum to {sum}, expected 1")]
    SpatialWeightSum { sum: f64 },
    #[error(transparent)]
    PropertyRead(#[from] WrfPropertyReadError),
    #[error("blend raw property state: {0}")]
    PropertyBlend(RawPropertyBlendError),
    #[error("interpolate raw thermodynamics/dynamics: {0}")]
    Gate(#[source] RawInterpolationError),
}

fn map_raw_property_blend_error(error: RawPropertyBlendError) -> RawStateLinearError {
    match error {
        RawPropertyBlendError::FieldSignatureMismatch { .. } => {
            RawStateLinearError::PropertyInventoryMismatch
        }
        RawPropertyBlendError::CategoryLayoutMismatch { .. } => {
            RawStateLinearError::PropertyCategoryLayoutMismatch
        }
        RawPropertyBlendError::RainAvailabilityMismatch { .. } => {
            RawStateLinearError::RainAvailabilityMismatch
        }
        RawPropertyBlendError::Sample(source) => RawStateLinearError::PropertyRead(source),
        other => RawStateLinearError::PropertyBlend(other),
    }
}

fn exact_raw_state_endpoint(
    endpoint: RawStateLinearEndpoint<'_>,
) -> Result<RawStateLinearCell, RawStateLinearError> {
    validate_raw_gate_state(endpoint.gate_state).map_err(RawStateLinearError::Gate)?;
    let property_cell = match endpoint.property_coverage {
        RawPropertyCoverage::Single(cell_index) => endpoint.property_scene.raw_cell(cell_index)?,
        RawPropertyCoverage::Weighted(weights) => {
            let mut samples = Vec::with_capacity(weights.len());
            append_endpoint_property_samples(endpoint, 1.0, &mut samples)?;
            normalize_raw_property_sample_residual(&mut samples)?;
            blend_raw_property_cells(&samples).map_err(map_raw_property_blend_error)?
        }
    };
    Ok(RawStateLinearCell {
        property_cell,
        gate_state: endpoint.gate_state.clone(),
    })
}

fn append_endpoint_property_samples<'a>(
    endpoint: RawStateLinearEndpoint<'a>,
    temporal_weight: f64,
    samples: &mut Vec<WeightedRawPropertyCell<'a>>,
) -> Result<(), RawStateLinearError> {
    match endpoint.property_coverage {
        RawPropertyCoverage::Single(cell_index) => samples.push(WeightedRawPropertyCell::new(
            endpoint.property_scene,
            cell_index,
            temporal_weight,
        )),
        RawPropertyCoverage::Weighted(weights) => {
            if weights.is_empty() {
                return Err(RawStateLinearError::EmptySpatialWeights);
            }
            if weights.len() > MAX_RAW_STATE_SPATIAL_WEIGHTS_PER_SCENE {
                return Err(RawStateLinearError::TooManySpatialWeights {
                    actual: weights.len(),
                    maximum: MAX_RAW_STATE_SPATIAL_WEIGHTS_PER_SCENE,
                });
            }
            let mut spatial_sum = 0.0;
            for (index, &(cell_index, weight)) in weights.iter().enumerate() {
                if !weight.is_finite() || weight < 0.0 {
                    return Err(RawStateLinearError::InvalidSpatialWeight { index, weight });
                }
                spatial_sum += weight;
                samples.push(WeightedRawPropertyCell::new(
                    endpoint.property_scene,
                    cell_index,
                    temporal_weight * weight,
                ));
            }
            if (spatial_sum - 1.0).abs() > RAW_STATE_WEIGHT_SUM_TOLERANCE {
                return Err(RawStateLinearError::SpatialWeightSum { sum: spatial_sum });
            }
        }
    }
    Ok(())
}

fn normalize_raw_property_sample_residual(
    samples: &mut [WeightedRawPropertyCell<'_>],
) -> Result<(), RawStateLinearError> {
    let sum = samples.iter().map(|sample| sample.weight).sum::<f64>();
    if !sum.is_finite() || (sum - 1.0).abs() > RAW_STATE_WEIGHT_SUM_TOLERANCE {
        return Err(RawStateLinearError::SpatialWeightSum { sum });
    }
    if let Some(target) = samples
        .iter_mut()
        .max_by(|left, right| left.weight.total_cmp(&right.weight))
    {
        target.weight += 1.0 - sum;
    }
    Ok(())
}

/// Interpolate complete raw state across two adjacent model times.
///
/// Alpha is closed and bounded; extrapolation is rejected. Exact endpoints
/// need only their endpoint coverage and are returned without a zero-weight
/// blend, preserving exact equality and source-cell provenance. Interior
/// samples require both endpoints and exact agreement in microphysics scheme,
/// normalized raw-field inventory/category layout, and rain availability.
/// [`blend_raw_property_cells`] performs the property blend so nonlinear
/// closure/scattering can occur exactly once afterward.
pub fn interpolate_raw_state_linear(
    left: Option<RawStateLinearEndpoint<'_>>,
    right: Option<RawStateLinearEndpoint<'_>>,
    alpha: f64,
) -> Result<RawStateLinearCell, RawStateLinearError> {
    if !alpha.is_finite() || !(0.0..=1.0).contains(&alpha) {
        return Err(RawStateLinearError::AlphaOutOfRange(alpha));
    }
    if alpha == 0.0 {
        return exact_raw_state_endpoint(left.ok_or(RawStateLinearError::MissingLeftCoverage)?);
    }
    if alpha == 1.0 {
        return exact_raw_state_endpoint(right.ok_or(RawStateLinearError::MissingRightCoverage)?);
    }

    let left = left.ok_or(RawStateLinearError::MissingLeftCoverage)?;
    let right = right.ok_or(RawStateLinearError::MissingRightCoverage)?;
    let left_scheme = left.property_scene.microphysics_scheme_id();
    let right_scheme = right.property_scene.microphysics_scheme_id();
    if left_scheme != right_scheme {
        return Err(RawStateLinearError::SchemeMismatch {
            left: left_scheme,
            right: right_scheme,
        });
    }
    if left.property_scene.required_field_signature()
        != right.property_scene.required_field_signature()
    {
        return Err(RawStateLinearError::PropertyInventoryMismatch);
    }

    let mut property_samples = Vec::with_capacity(2 * MAX_RAW_STATE_SPATIAL_WEIGHTS_PER_SCENE);
    append_endpoint_property_samples(left, 1.0 - alpha, &mut property_samples)?;
    append_endpoint_property_samples(right, alpha, &mut property_samples)?;
    normalize_raw_property_sample_residual(&mut property_samples)?;
    let property_cell =
        blend_raw_property_cells(&property_samples).map_err(map_raw_property_blend_error)?;
    let gate_state = interpolate_raw_gate(Some(left.gate_state), Some(right.gate_state), alpha)
        .map_err(RawStateLinearError::Gate)?;
    Ok(RawStateLinearCell {
        property_cell,
        gate_state,
    })
}

/// Interpolate reflectivity through linear equivalent reflectivity factor.
pub fn interpolate_dbz_linear_z(left_dbz: f32, right_dbz: f32, alpha: f64) -> Option<f32> {
    if !left_dbz.is_finite()
        || !right_dbz.is_finite()
        || !alpha.is_finite()
        || !(0.0..=1.0).contains(&alpha)
    {
        return None;
    }
    let left_z = 10.0f64.powf(left_dbz as f64 / 10.0);
    let right_z = 10.0f64.powf(right_dbz as f64 / 10.0);
    let z = left_z + (right_z - left_z) * alpha;
    (z > 0.0 && z.is_finite()).then(|| (10.0 * z.log10()) as f32)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TemporalMemoryEstimate {
    pub cells_per_scene: usize,
    pub dense_fields_per_scene: usize,
    pub bytes_per_dense_value: usize,
    pub compact_bytes_per_scene: usize,
    pub shared_static_bytes: usize,
    pub output_bytes: usize,
}

impl TemporalMemoryEstimate {
    pub fn scene_bytes(self) -> Option<usize> {
        self.cells_per_scene
            .checked_mul(self.dense_fields_per_scene)?
            .checked_mul(self.bytes_per_dense_value)?
            .checked_add(self.compact_bytes_per_scene)
    }

    pub fn rolling_two_scene_peak_bytes(self) -> Option<usize> {
        self.scene_bytes()?
            .checked_mul(2)?
            .checked_add(self.shared_static_bytes)?
            .checked_add(self.output_bytes)
    }

    pub fn preflight(self, budget_bytes: usize) -> TemporalMemoryDecision {
        match self.rolling_two_scene_peak_bytes() {
            Some(required_bytes) if required_bytes <= budget_bytes => {
                TemporalMemoryDecision::Fits {
                    required_bytes,
                    budget_bytes,
                }
            }
            Some(required_bytes) => TemporalMemoryDecision::Exceeds {
                required_bytes,
                budget_bytes,
            },
            None => TemporalMemoryDecision::Overflow,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TemporalMemoryDecision {
    Fits {
        required_bytes: usize,
        budget_bytes: usize,
    },
    Exceeds {
        required_bytes: usize,
        budget_bytes: usize,
    },
    Overflow,
}

/// Minimal rolling cache with a hard two-scene invariant.
#[derive(Clone, Debug)]
pub struct TwoSceneCache<K, V> {
    entries: VecDeque<(K, V)>,
}

impl<K, V> Default for TwoSceneCache<K, V> {
    fn default() -> Self {
        Self {
            entries: VecDeque::with_capacity(2),
        }
    }
}

impl<K: Eq + Hash, V> TwoSceneCache<K, V> {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, key: &K) -> Option<&V> {
        self.entries
            .iter()
            .find_map(|(candidate, value)| (candidate == key).then_some(value))
    }

    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.entries.iter().map(|(_, value)| value)
    }

    pub fn remove(&mut self, key: &K) -> Option<(K, V)> {
        let position = self
            .entries
            .iter()
            .position(|(candidate, _)| candidate == key)?;
        self.entries.remove(position)
    }

    /// Insert/replace and return an evicted oldest scene, if any.
    pub fn insert(&mut self, key: K, value: V) -> Option<(K, V)> {
        if let Some(position) = self
            .entries
            .iter()
            .position(|(candidate, _)| *candidate == key)
        {
            self.entries.remove(position);
        }
        let evicted = (self.entries.len() == 2)
            .then(|| self.entries.pop_front())
            .flatten();
        self.entries.push_back((key, value));
        debug_assert!(self.entries.len() <= 2);
        evicted
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use chrono::TimeZone;
    use radar_scattering::OrientationDefinition;

    use super::*;
    use crate::wrf_property_reader::{
        PropertyFieldProvider, RawPropertyField, WrfPropertyScene, close_raw_property_cell,
        read_property_scene,
    };
    use crate::wrf_scene_inventory::{
        WrfDomainId, WrfGridSignature, WrfRunDomain, WrfRunId, WrfScene, WrfSceneDiagnostics,
        WrfSceneGroupKey, WrfSceneTime, WrfSourceIdentity,
    };

    fn scene(name: &str, hour: u32) -> WrfScene {
        WrfScene {
            path: PathBuf::from(name),
            time_index: 0,
            run_domain: WrfRunDomain {
                run: WrfRunId("run".to_owned()),
                domain: WrfDomainId(1),
            },
            grid_signature: WrfGridSignature::from_meters(
                2,
                2,
                Some(1),
                Some(3_000.0),
                Some(3_000.0),
                "lambert",
                7,
            ),
            source_identity: WrfSourceIdentity(name.to_owned()),
            time: WrfSceneTime::InternalTimes {
                valid_time: Utc.with_ymd_and_hms(2026, 7, 12, hour, 0, 0).unwrap(),
                raw: format!("2026-07-12_{hour:02}:00:00"),
            },
        }
    }

    fn group(hours: &[u32]) -> WrfSceneGroup {
        let scenes: Vec<_> = hours
            .iter()
            .map(|hour| scene(&format!("wrfout_{hour}"), *hour))
            .collect();
        WrfSceneGroup {
            key: WrfSceneGroupKey {
                run_domain: scenes[0].run_domain.clone(),
                grid_signature: scenes[0].grid_signature.clone(),
            },
            scenes,
            diagnostics: WrfSceneDiagnostics::default(),
        }
    }

    #[test]
    fn frozen_mode_needs_no_neighbor_and_alpha_is_zero() {
        let plan = plan_for_scene(
            &group(&[0]),
            0,
            Duration::minutes(6),
            AtmosphereTimeMode::FrozenAtVolumeStart,
            MissingNeighborPolicy::Error,
        )
        .unwrap()
        .unwrap();
        assert_eq!(plan.outcome, TemporalSamplingOutcome::Frozen);
        assert_eq!(plan.ray_alpha(359_999).unwrap(), 0.0);
    }

    #[test]
    fn linear_plan_uses_one_bounded_alpha_per_ray() {
        let plan = plan_for_scene(
            &group(&[0, 1]),
            0,
            Duration::hours(1),
            AtmosphereTimeMode::LinearAdjacent,
            MissingNeighborPolicy::Error,
        )
        .unwrap()
        .unwrap();
        assert_eq!(plan.outcome, TemporalSamplingOutcome::LinearAdjacent);
        assert_eq!(plan.ray_alpha(0).unwrap(), 0.0);
        assert!((plan.ray_alpha(1_800_000).unwrap() - 0.5).abs() < 1.0e-12);
        assert_eq!(plan.ray_alpha(3_600_000).unwrap(), 1.0);
        assert!(matches!(
            plan.ray_alpha(3_600_001),
            Err(TemporalPlanError::RayPastScan { .. })
        ));

        let raw_plan = plan_for_scene(
            &group(&[0, 1]),
            0,
            Duration::minutes(30),
            AtmosphereTimeMode::RawStateLinear,
            MissingNeighborPolicy::Error,
        )
        .unwrap()
        .unwrap();
        assert_eq!(raw_plan.mode, AtmosphereTimeMode::RawStateLinear);
        assert_eq!(raw_plan.outcome, TemporalSamplingOutcome::LinearAdjacent);
        assert_eq!(raw_plan.ray_alpha(1_800_000).unwrap(), 0.5);
    }

    #[test]
    fn final_frame_policies_hold_drop_or_error_explicitly() {
        let scenes = group(&[0]);
        let held = plan_for_scene(
            &scenes,
            0,
            Duration::minutes(5),
            AtmosphereTimeMode::LinearAdjacent,
            MissingNeighborPolicy::HoldAnchor,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            held.outcome,
            TemporalSamplingOutcome::HeldAnchor(HoldReason::NoLaterScene)
        );
        assert!(
            plan_for_scene(
                &scenes,
                0,
                Duration::minutes(5),
                AtmosphereTimeMode::LinearAdjacent,
                MissingNeighborPolicy::DropFrame,
            )
            .unwrap()
            .is_none()
        );
        assert!(matches!(
            plan_for_scene(
                &scenes,
                0,
                Duration::minutes(5),
                AtmosphereTimeMode::LinearAdjacent,
                MissingNeighborPolicy::Error,
            ),
            Err(TemporalPlanError::MissingNeighbor { .. })
        ));
    }

    #[test]
    fn scan_must_fit_inside_one_adjacent_bracket() {
        let held = plan_for_scene(
            &group(&[0, 1]),
            0,
            Duration::minutes(61),
            AtmosphereTimeMode::LinearAdjacent,
            MissingNeighborPolicy::HoldAnchor,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            held.outcome,
            TemporalSamplingOutcome::HeldAnchor(HoldReason::ScanCrossesNeighbor)
        );
        assert!(held.neighbor.is_some());
        assert_eq!(
            held.neighbor_time,
            Some(Utc.with_ymd_and_hms(2026, 7, 12, 1, 0, 0).unwrap())
        );
        assert_eq!(held.ray_alpha(30_000).unwrap(), 0.0);
    }

    #[test]
    fn property_compatibility_never_silently_mixes_schemes_or_fields() {
        let base = ScenePropertySignature {
            microphysics_scheme_id: Some(55),
            reflectivity_source: "property LUT".to_owned(),
            required_raw_fields: ["QICE", "QNICE", "QVOLI"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        };
        assert_eq!(
            assess_property_compatibility(&base, &base),
            PropertyCompatibility::Compatible
        );
        let mut changed = base.clone();
        changed.microphysics_scheme_id = Some(53);
        assert!(matches!(
            assess_property_compatibility(&base, &changed),
            PropertyCompatibility::SchemeMismatch { .. }
        ));
        changed = base.clone();
        changed.required_raw_fields.remove("QVOLI");
        assert!(matches!(
            assess_property_compatibility(&base, &changed),
            PropertyCompatibility::RawFieldInventoryMismatch { .. }
        ));
    }

    fn raw(qrain: f32, u: f32) -> RawGateState {
        RawGateState {
            wind_u_mps: u,
            wind_v_mps: 0.0,
            wind_w_mps: 1.0,
            temperature_k: 280.0,
            pressure_pa: 90_000.0,
            air_density_kgm3: 1.1,
            fields: [("QRAIN".to_owned(), qrain)].into_iter().collect(),
        }
    }

    struct TemporalPropertyProvider {
        identity: WrfSourceIdentity,
        scheme_id: i32,
        fields: BTreeMap<&'static str, RawPropertyField>,
    }

    impl PropertyFieldProvider for TemporalPropertyProvider {
        fn source_identity(&self) -> WrfSourceIdentity {
            self.identity.clone()
        }

        fn microphysics_scheme_id(&self) -> Result<i32, String> {
            Ok(self.scheme_id)
        }

        fn cell_count(&self) -> usize {
            self.fields
                .values()
                .next()
                .map_or(0, |field| field.values.len())
        }

        fn time_count(&self) -> usize {
            1
        }

        fn has_field(&self, name: &str) -> bool {
            self.fields.contains_key(name)
        }

        fn read_field(&self, name: &str, _time_index: usize) -> Result<RawPropertyField, String> {
            self.fields
                .get(name)
                .cloned()
                .ok_or_else(|| format!("{name} absent"))
        }

        fn clear_cache(&self) {}
    }

    // Test builder keeps each independently varied native WRF coordinate
    // explicit so mismatch cases remain readable at their call sites.
    #[allow(clippy::too_many_arguments)]
    fn property_provider(
        name: &str,
        scheme_id: i32,
        temperature_k: f64,
        qice: f64,
        qnice: f64,
        qir: f64,
        qib: f64,
        rain: Option<(f64, f64)>,
    ) -> TemporalPropertyProvider {
        let mut fields = BTreeMap::from([
            ("QICE", RawPropertyField::new(vec![qice], "kg kg-1")),
            ("QNICE", RawPropertyField::new(vec![qnice], "kg-1")),
            ("QIR", RawPropertyField::new(vec![qir], "kg kg-1")),
            ("QIB", RawPropertyField::new(vec![qib], "m3 kg-1")),
            ("T", RawPropertyField::new(vec![temperature_k - 300.0], "K")),
            ("P", RawPropertyField::new(vec![0.0], "Pa")),
            ("PB", RawPropertyField::new(vec![100_000.0], "Pa")),
            ("QVAPOR", RawPropertyField::new(vec![0.0], "kg kg-1")),
        ]);
        if let Some((qrain, qnrain)) = rain {
            fields.insert("QRAIN", RawPropertyField::new(vec![qrain], "kg kg-1"));
            fields.insert("QNRAIN", RawPropertyField::new(vec![qnrain], "kg-1"));
        }
        TemporalPropertyProvider {
            identity: WrfSourceIdentity(name.to_owned()),
            scheme_id,
            fields,
        }
    }

    fn property_scene(
        name: &str,
        temperature_k: f64,
        qice: f64,
        qnice: f64,
        qir: f64,
        qib: f64,
        rain: Option<(f64, f64)>,
    ) -> WrfPropertyScene {
        read_property_scene(
            &property_provider(name, 50, temperature_k, qice, qnice, qir, qib, rain),
            0,
        )
        .unwrap()
    }

    fn two_cell_property_scene(name: &str, qice: [f64; 2]) -> WrfPropertyScene {
        let fields = BTreeMap::from([
            ("QICE", RawPropertyField::new(qice.to_vec(), "kg kg-1")),
            ("QNICE", RawPropertyField::new(vec![1.0e6; 2], "kg-1")),
            ("QIR", RawPropertyField::new(vec![4.0e-5; 2], "kg kg-1")),
            ("QIB", RawPropertyField::new(vec![1.0e-7; 2], "m3 kg-1")),
            ("T", RawPropertyField::new(vec![-30.0; 2], "K")),
            ("P", RawPropertyField::new(vec![0.0; 2], "Pa")),
            ("PB", RawPropertyField::new(vec![100_000.0; 2], "Pa")),
            ("QVAPOR", RawPropertyField::new(vec![0.0; 2], "kg kg-1")),
            ("QRAIN", RawPropertyField::new(vec![0.0; 2], "kg kg-1")),
            ("QNRAIN", RawPropertyField::new(vec![0.0; 2], "kg-1")),
        ]);
        read_property_scene(
            &TemporalPropertyProvider {
                identity: WrfSourceIdentity(name.to_owned()),
                scheme_id: 50,
                fields,
            },
            0,
        )
        .unwrap()
    }

    fn endpoint_gate(temperature_k: f32, qrain: f32, wind_u_mps: f32) -> RawGateState {
        RawGateState {
            wind_u_mps,
            wind_v_mps: -5.0,
            wind_w_mps: 1.0,
            temperature_k,
            pressure_pa: 100_000.0,
            air_density_kgm3: 100_000.0 / (287.05 * temperature_k),
            fields: [("QRAIN".to_owned(), qrain)].into_iter().collect(),
        }
    }

    #[test]
    fn raw_state_interpolation_supports_echo_birth_and_exact_endpoints() {
        let clear = raw(0.0, 10.0);
        let storm = raw(0.002, 30.0);
        let mid = interpolate_raw_gate(Some(&clear), Some(&storm), 0.5).unwrap();
        assert!((mid.fields["QRAIN"] - 0.001).abs() < 1.0e-8);
        assert_eq!(mid.wind_u_mps, 20.0);
        assert_eq!(
            interpolate_raw_gate(Some(&clear), None, 0.0).unwrap(),
            clear
        );
        assert_eq!(
            interpolate_raw_gate(None, Some(&storm), 1.0).unwrap(),
            storm
        );
    }

    #[test]
    fn raw_state_linear_preserves_endpoints_and_supports_echo_birth_and_decay() {
        let clear_scene = property_scene("clear", 268.0, 0.0, 0.0, 0.0, 0.0, Some((0.0, 0.0)));
        let echo_scene = property_scene(
            "echo",
            272.0,
            1.0e-4,
            1.0e6,
            4.0e-5,
            1.0e-7,
            Some((2.0e-3, 1.0e6)),
        );
        let clear_gate = endpoint_gate(268.0, 0.0, 10.0);
        let echo_gate = endpoint_gate(272.0, 2.0e-3, 30.0);
        let clear = RawStateLinearEndpoint::new(&clear_scene, 0, &clear_gate);
        let echo = RawStateLinearEndpoint::new(&echo_scene, 0, &echo_gate);

        let left_endpoint = interpolate_raw_state_linear(Some(clear), None, 0.0).unwrap();
        assert_eq!(
            left_endpoint.property_cell,
            clear_scene.raw_cell(0).unwrap()
        );
        assert_eq!(left_endpoint.gate_state, clear_gate);
        let right_endpoint = interpolate_raw_state_linear(None, Some(echo), 1.0).unwrap();
        assert_eq!(
            right_endpoint.property_cell,
            echo_scene.raw_cell(0).unwrap()
        );
        assert_eq!(right_endpoint.gate_state, echo_gate);

        let birth = interpolate_raw_state_linear(Some(clear), Some(echo), 0.25).unwrap();
        assert!((birth.gate_state.fields["QRAIN"] - 5.0e-4).abs() < 1.0e-9);
        assert_eq!(birth.gate_state.wind_u_mps, 15.0);
        assert_eq!(birth.property_cell.source_cell_index(), None);
        assert!((birth.property_cell.environment().temperature_k() - 269.0).abs() < 2.0e-5);
        assert!((birth.property_cell.categories()[0].mixing_ratio_kgkg() - 2.5e-5).abs() < 1.0e-10);

        let decay = interpolate_raw_state_linear(Some(echo), Some(clear), 0.75).unwrap();
        assert!((decay.property_cell.categories()[0].mixing_ratio_kgkg() - 2.5e-5).abs() < 1.0e-10);
    }

    #[test]
    fn raw_state_linear_normalizes_reported_sub_qsmall_p3_echo_tail_to_clear() {
        let clear_scene =
            property_scene("qsmall-clear", 270.0, 0.0, 0.0, 0.0, 0.0, Some((0.0, 0.0)));
        let echo_scene = property_scene(
            "qsmall-echo",
            270.0,
            1.0e-4,
            1.0e6,
            4.0e-5,
            1.0e-7,
            Some((0.0, 0.0)),
        );
        let gate = endpoint_gate(270.0, 0.0, 10.0);
        let clear = RawStateLinearEndpoint::new(&clear_scene, 0, &gate);
        let echo = RawStateLinearEndpoint::new(&echo_scene, 0, &gate);
        let reported_tail = 7.072_708_808_391_012e-16;
        let echo_mass = echo_scene.raw_cell(0).unwrap().categories()[0].mixing_ratio_kgkg();
        let alpha = reported_tail / echo_mass;

        let interpolated = interpolate_raw_state_linear(Some(clear), Some(echo), alpha).unwrap();
        assert_eq!(
            interpolated.property_cell.categories()[0].mixing_ratio_kgkg(),
            0.0
        );
        assert!(
            close_raw_property_cell(
                &interpolated.property_cell,
                OrientationDefinition::SchemeDefault,
            )
            .unwrap()
            .categories()
            .is_empty()
        );
    }

    #[test]
    fn raw_state_linear_combines_full_spatial_stencils_before_temporal_blend() {
        let left_scene = two_cell_property_scene("left-spatial", [1.0e-4, 3.0e-4]);
        let right_scene = two_cell_property_scene("right-spatial", [5.0e-4, 9.0e-4]);
        let left_gate = endpoint_gate(270.0, 0.0, 12.0);
        let right_gate = endpoint_gate(270.0, 0.0, 28.0);
        let left_weights = [(0, 0.25), (1, 0.75)];
        let right_weights = [(0, 0.5), (1, 0.5)];
        let left =
            RawStateLinearEndpoint::with_spatial_weights(&left_scene, &left_weights, &left_gate);
        let right =
            RawStateLinearEndpoint::with_spatial_weights(&right_scene, &right_weights, &right_gate);

        let exact_left = interpolate_raw_state_linear(Some(left), None, 0.0).unwrap();
        assert!(
            (exact_left.property_cell.categories()[0].mixing_ratio_kgkg() - 2.5e-4).abs() < 1.0e-10
        );
        let blended = interpolate_raw_state_linear(Some(left), Some(right), 0.4).unwrap();
        // left spatial=2.5e-4, right spatial=7e-4, temporal=0.6/0.4.
        assert!(
            (blended.property_cell.categories()[0].mixing_ratio_kgkg() - 4.3e-4).abs() < 1.0e-10
        );
        assert!((blended.gate_state.wind_u_mps - 18.4).abs() < 1.0e-6);
        assert_eq!(blended.property_cell.source_cell_index(), None);
    }

    #[test]
    fn raw_state_closure_is_not_additive_endpoint_interpolation() {
        let left_scene = property_scene(
            "left",
            268.0,
            1.0e-4,
            1.0e6,
            4.0e-5,
            1.0e-7,
            Some((0.0, 0.0)),
        );
        let right_scene = property_scene(
            "right",
            272.0,
            8.0e-4,
            2.0e6,
            3.2e-4,
            8.0e-7,
            Some((0.0, 0.0)),
        );
        let left_gate = endpoint_gate(268.0, 0.0, 10.0);
        let right_gate = endpoint_gate(272.0, 0.0, 30.0);
        let midpoint = interpolate_raw_state_linear(
            Some(RawStateLinearEndpoint::new(&left_scene, 0, &left_gate)),
            Some(RawStateLinearEndpoint::new(&right_scene, 0, &right_gate)),
            0.5,
        )
        .unwrap();
        let left_closed = close_raw_property_cell(
            &left_scene.raw_cell(0).unwrap(),
            OrientationDefinition::SchemeDefault,
        )
        .unwrap();
        let right_closed = close_raw_property_cell(
            &right_scene.raw_cell(0).unwrap(),
            OrientationDefinition::SchemeDefault,
        )
        .unwrap();
        let midpoint_closed = close_raw_property_cell(
            &midpoint.property_cell,
            OrientationDefinition::SchemeDefault,
        )
        .unwrap();
        let additive_diameter = 0.5
            * (left_closed.categories()[0]
                .closed()
                .characteristic_diameter_m()
                .value()
                + right_closed.categories()[0]
                    .closed()
                    .characteristic_diameter_m()
                    .value());
        let raw_state_diameter = midpoint_closed.categories()[0]
            .closed()
            .characteristic_diameter_m()
            .value();
        assert!(
            (raw_state_diameter - additive_diameter).abs() > 1.0e-8,
            "nonlinear closure must follow raw-state blending, not endpoint-output averaging"
        );
    }

    #[test]
    fn raw_state_linear_rejects_scheme_inventory_rain_and_extrapolation_mismatches() {
        let left_provider = property_provider(
            "left",
            50,
            270.0,
            1.0e-4,
            1.0e6,
            4.0e-5,
            1.0e-7,
            Some((1.0e-4, 1.0e6)),
        );
        let left_scene = read_property_scene(&left_provider, 0).unwrap();
        let gate = endpoint_gate(270.0, 1.0e-4, 20.0);
        let left = RawStateLinearEndpoint::new(&left_scene, 0, &gate);

        let scheme_scene = read_property_scene(
            &property_provider(
                "scheme",
                51,
                270.0,
                1.0e-4,
                1.0e6,
                4.0e-5,
                1.0e-7,
                Some((1.0e-4, 1.0e6)),
            ),
            0,
        )
        .unwrap();
        assert_eq!(
            interpolate_raw_state_linear(
                Some(left),
                Some(RawStateLinearEndpoint::new(&scheme_scene, 0, &gate)),
                0.5,
            ),
            Err(RawStateLinearError::SchemeMismatch {
                left: 50,
                right: 51,
            })
        );

        let inventory_scene =
            property_scene("inventory", 270.0, 1.0e-4, 1.0e6, 4.0e-5, 1.0e-7, None);
        assert_eq!(
            interpolate_raw_state_linear(
                Some(left),
                Some(RawStateLinearEndpoint::new(&inventory_scene, 0, &gate)),
                0.5,
            ),
            Err(RawStateLinearError::PropertyInventoryMismatch)
        );

        let invalid_rain_scene = property_scene(
            "invalid-rain",
            270.0,
            1.0e-4,
            1.0e6,
            4.0e-5,
            1.0e-7,
            // P3 repairs finite nonpositive QNRAIN and clears all QRAIN below
            // qsmall. A nonfinite number remains genuinely unavailable.
            Some((1.0e-4, f64::NAN)),
        );
        assert_eq!(
            interpolate_raw_state_linear(
                Some(left),
                Some(RawStateLinearEndpoint::new(&invalid_rain_scene, 0, &gate,)),
                0.5,
            ),
            Err(RawStateLinearError::RainAvailabilityMismatch)
        );
        assert!(matches!(
            interpolate_raw_state_linear(Some(left), Some(left), 1.000_001),
            Err(RawStateLinearError::AlphaOutOfRange(_))
        ));
    }

    #[test]
    fn atmosphere_time_mode_serde_keeps_legacy_linear_adjacent_stable() {
        assert_eq!(
            serde_json::from_str::<AtmosphereTimeMode>("\"linear_adjacent\"").unwrap(),
            AtmosphereTimeMode::LinearAdjacent
        );
        assert_eq!(
            serde_json::to_string(&AtmosphereTimeMode::LinearAdjacent).unwrap(),
            "\"linear_adjacent\""
        );
        assert_eq!(
            serde_json::from_str::<AtmosphereTimeMode>("\"raw_state_linear\"").unwrap(),
            AtmosphereTimeMode::RawStateLinear
        );
        assert_eq!(
            AtmosphereTimeMode::LinearAdjacent.interpolation_space_name(),
            "linear Z, winds, and additive scattering quantities"
        );
        assert_eq!(
            AtmosphereTimeMode::RawStateLinear.provenance_name(),
            "raw_state_linear"
        );
    }

    #[test]
    fn missing_domain_coverage_is_not_clear_air() {
        let state = raw(0.001, 20.0);
        assert_eq!(
            interpolate_raw_gate(Some(&state), None, 0.5),
            Err(RawInterpolationError::MissingRightCoverage)
        );
        assert_eq!(
            interpolate_raw_gate(None, Some(&state), 0.5),
            Err(RawInterpolationError::MissingLeftCoverage)
        );
    }

    #[test]
    fn reflectivity_interpolates_in_linear_z_not_dbz() {
        let midpoint = interpolate_dbz_linear_z(0.0, 20.0, 0.5).unwrap();
        assert!((midpoint - 17.032913).abs() < 1.0e-5);
        assert!(
            (midpoint - 10.0).abs() > 7.0,
            "direct dBZ averaging is forbidden"
        );
    }

    #[test]
    fn memory_preflight_counts_exactly_two_dense_scenes() {
        let estimate = TemporalMemoryEstimate {
            cells_per_scene: 800 * 800 * 79,
            dense_fields_per_scene: 5,
            bytes_per_dense_value: 4,
            compact_bytes_per_scene: 200_000_000,
            shared_static_bytes: 100_000_000,
            output_bytes: 300_000_000,
        };
        let scene = (800usize * 800 * 79 * 5 * 4) + 200_000_000;
        assert_eq!(estimate.scene_bytes(), Some(scene));
        assert_eq!(
            estimate.rolling_two_scene_peak_bytes(),
            Some(scene * 2 + 400_000_000)
        );
        assert!(matches!(
            estimate.preflight(2_000_000_000),
            TemporalMemoryDecision::Exceeds { .. }
        ));
    }

    #[test]
    fn rolling_cache_never_retains_more_than_two_scenes() {
        let mut cache = TwoSceneCache::default();
        assert!(cache.insert(0, "zero").is_none());
        assert!(cache.insert(1, "one").is_none());
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.insert(2, "two"), Some((0, "zero")));
        assert_eq!(cache.len(), 2);
        assert!(cache.get(&0).is_none());
        assert_eq!(cache.get(&1), Some(&"one"));
        assert_eq!(cache.insert(1, "ONE"), None);
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get(&1), Some(&"ONE"));
    }
}
