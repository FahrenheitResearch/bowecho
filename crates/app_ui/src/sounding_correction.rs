//! Pure, source-bound model-sounding correction engine.
//!
//! The editor is deliberately kept out of this module.  A recipe is always
//! evaluated against an untouched [`SoundingColumn`], which makes preview,
//! reset, and convective-adjustment undo deterministic and prevents edits
//! from accumulating through repeated UI frames.

use rustwx_sounding::SoundingColumn;
use serde::{Deserialize, Serialize};

use sharppyrs::sharprs::{constants::ROCP, thermo};

const MS_TO_KT: f64 = 1.943_844_49;
const ZERO_C_K: f64 = 273.15;
const WATER_VAPOR_MASS_RATIO: f64 = 0.621_97;
const MIN_PRESSURE_HPA: f64 = 1.0e-6;
const MAX_SPECIFIC_HUMIDITY: f64 = 1.0 - 1.0e-12;
const DEFAULT_BLEND_DEPTH_M: f64 = 500.0;
const MAX_BLEND_DEPTH_M: f64 = 20_000.0;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum ThermalMode {
    Temperature,
    #[default]
    PotentialTemperature,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) enum ThermalTarget {
    TemperatureC(f64),
    PotentialTemperatureK(f64),
}

impl ThermalTarget {
    pub(crate) fn mode(self) -> ThermalMode {
        match self {
            Self::TemperatureC(_) => ThermalMode::Temperature,
            Self::PotentialTemperatureK(_) => ThermalMode::PotentialTemperature,
        }
    }

    /// Change the user-facing coordinate without changing the target air
    /// state at the anchor pressure.
    pub(crate) fn converted(self, mode: ThermalMode, pressure_hpa: f64) -> Option<Self> {
        convert_thermal_target(self, mode, pressure_hpa)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum MoistureMode {
    #[default]
    Dewpoint,
    MixingRatio,
    SpecificHumidity,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) enum MoistureTarget {
    DewpointC(f64),
    MixingRatioGKg(f64),
    SpecificHumidityGKg(f64),
}

impl MoistureTarget {
    pub(crate) fn mode(self) -> MoistureMode {
        match self {
            Self::DewpointC(_) => MoistureMode::Dewpoint,
            Self::MixingRatioGKg(_) => MoistureMode::MixingRatio,
            Self::SpecificHumidityGKg(_) => MoistureMode::SpecificHumidity,
        }
    }

    /// Change the user-facing moisture coordinate while preserving canonical
    /// specific humidity at the anchor pressure.
    pub(crate) fn converted(self, mode: MoistureMode, pressure_hpa: f64) -> Option<Self> {
        convert_moisture_target(self, mode, pressure_hpa)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum WindMode {
    #[default]
    DirectionSpeed,
    Components,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) enum WindTarget {
    DirectionSpeed { direction_deg: f64, speed_kt: f64 },
    UV { u_ms: f64, v_ms: f64 },
}

impl WindTarget {
    pub(crate) fn mode(self) -> WindMode {
        match self {
            Self::DirectionSpeed { .. } => WindMode::DirectionSpeed,
            Self::UV { .. } => WindMode::Components,
        }
    }

    pub(crate) fn converted(self, mode: WindMode) -> Option<Self> {
        convert_wind_target(self, mode)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum BlendExtent {
    #[default]
    SymmetricLocal,
    UpwardFromAnchor,
    /// Carry the anchor increment unchanged down to the model surface, then
    /// use the selected shape above the anchor.
    SurfaceLayer,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct BlendControlPoint {
    /// Normalized distance through the blend domain, clamped to `[0, 1]`.
    pub x: f64,
    /// Correction weight, clamped to `[0, 1]`.
    pub y: f64,
}

impl BlendControlPoint {
    pub(crate) const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) enum BlendShape {
    Cosine,
    Linear,
    /// Unit weight through the lower part of the domain, with a half-cosine
    /// transition confined to the upper `taper_fraction`.
    LayerConstantUpperCosine {
        taper_fraction: f64,
    },
    /// Piecewise-linear W(x).  Evaluation filters non-finite points, clamps
    /// both coordinates, sorts/deduplicates x, and pins `(0,1)` / `(1,0)`.
    Custom {
        points: Vec<BlendControlPoint>,
    },
}

impl Default for BlendShape {
    fn default() -> Self {
        Self::Cosine
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct BlendSpec {
    pub depth_m: f64,
    #[serde(default)]
    pub extent: BlendExtent,
    #[serde(default)]
    pub shape: BlendShape,
}

impl Default for BlendSpec {
    fn default() -> Self {
        Self::local_cosine(DEFAULT_BLEND_DEPTH_M)
    }
}

impl BlendSpec {
    pub(crate) fn local_cosine(depth_m: f64) -> Self {
        Self {
            depth_m,
            extent: BlendExtent::SymmetricLocal,
            shape: BlendShape::Cosine,
        }
        .normalized()
    }

    pub(crate) fn mixed_layer(depth_m: f64) -> Self {
        Self {
            depth_m,
            extent: BlendExtent::UpwardFromAnchor,
            shape: BlendShape::LayerConstantUpperCosine {
                taper_fraction: 0.25,
            },
        }
        .normalized()
    }

    pub(crate) fn normalized(&self) -> Self {
        let depth_m = if self.depth_m.is_finite() {
            self.depth_m.clamp(0.0, MAX_BLEND_DEPTH_M)
        } else {
            DEFAULT_BLEND_DEPTH_M
        };
        let shape = match &self.shape {
            BlendShape::Cosine => BlendShape::Cosine,
            BlendShape::Linear => BlendShape::Linear,
            BlendShape::LayerConstantUpperCosine { taper_fraction } => {
                BlendShape::LayerConstantUpperCosine {
                    taper_fraction: if taper_fraction.is_finite() {
                        taper_fraction.clamp(0.01, 1.0)
                    } else {
                        0.25
                    },
                }
            }
            BlendShape::Custom { points } => BlendShape::Custom {
                points: normalize_control_points(points),
            },
        };
        Self {
            depth_m,
            extent: self.extent,
            shape,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct ThermalEdit {
    pub target: ThermalTarget,
    pub blend: BlendSpec,
}

impl ThermalEdit {
    pub(crate) fn new(target: ThermalTarget) -> Self {
        let blend = match target.mode() {
            ThermalMode::Temperature => BlendSpec::local_cosine(DEFAULT_BLEND_DEPTH_M),
            ThermalMode::PotentialTemperature => BlendSpec::mixed_layer(1_500.0),
        };
        Self { target, blend }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct MoistureEdit {
    pub target: MoistureTarget,
    pub blend: BlendSpec,
}

impl MoistureEdit {
    pub(crate) fn new(target: MoistureTarget) -> Self {
        Self {
            target,
            blend: BlendSpec::local_cosine(DEFAULT_BLEND_DEPTH_M),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct WindEdit {
    pub target: WindTarget,
    pub blend: BlendSpec,
}

impl WindEdit {
    pub(crate) fn new(target: WindTarget) -> Self {
        Self {
            target,
            blend: BlendSpec::local_cosine(1_000.0),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct CorrectionLevel {
    pub target_agl_m: f64,
    #[serde(default)]
    pub thermal: Option<ThermalEdit>,
    #[serde(default)]
    pub moisture: Option<MoistureEdit>,
    #[serde(default)]
    pub wind: Option<WindEdit>,
}

impl CorrectionLevel {
    pub(crate) fn at_height(target_agl_m: f64) -> Self {
        Self {
            target_agl_m,
            thermal: None,
            moisture: None,
            wind: None,
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        self.thermal.is_some() || self.moisture.is_some() || self.wind.is_some()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct ConvectiveAdjustmentConfig {
    #[serde(default)]
    pub enabled: bool,
    /// The lowest part of the sounding is allowed to remain superadiabatic.
    pub protected_surface_depth_m: f64,
}

impl Default for ConvectiveAdjustmentConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            protected_surface_depth_m: 100.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct QcConfig {
    pub dry_stability_protected_surface_depth_m: f64,
    pub dry_instability_min_depth_m: f64,
    pub dry_instability_theta_drop_k: f64,
    pub supersaturation_tolerance_percent: f64,
    /// Increase in vector-shear-gradient discontinuity over the source.
    pub wind_kink_increase_s_inv: f64,
    /// Absolute vector-shear-gradient discontinuity in the corrected profile.
    pub wind_kink_absolute_s_inv: f64,
}

impl Default for QcConfig {
    fn default() -> Self {
        Self {
            dry_stability_protected_surface_depth_m: 100.0,
            dry_instability_min_depth_m: 100.0,
            dry_instability_theta_drop_k: 0.05,
            supersaturation_tolerance_percent: 0.5,
            wind_kink_increase_s_inv: 0.005,
            wind_kink_absolute_s_inv: 0.015,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct CorrectionRecipe {
    #[serde(default)]
    pub levels: Vec<CorrectionLevel>,
    #[serde(default)]
    pub convective_adjustment: ConvectiveAdjustmentConfig,
    #[serde(default)]
    pub qc: QcConfig,
}

impl CorrectionRecipe {
    pub(crate) fn active_level_count(&self) -> usize {
        self.levels.iter().filter(|level| level.is_active()).count()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum QcSeverity {
    Advisory,
    Warning,
    Error,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum QcIssueKind {
    Structural,
    InvalidTarget,
    InvalidMoisture,
    Supersaturation,
    DryStaticInstability,
    WindShearKink,
    ConvectiveAdjustmentAborted,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct QcIssue {
    pub severity: QcSeverity,
    pub kind: QcIssueKind,
    /// Recipe row responsible for an input/application issue.
    pub correction_index: Option<usize>,
    /// Native sounding level affected by a profile/QC issue.
    pub level_index: Option<usize>,
    pub end_level_index: Option<usize>,
    pub message: String,
}

impl QcIssue {
    fn at_level(
        severity: QcSeverity,
        kind: QcIssueKind,
        level_index: usize,
        message: impl Into<String>,
    ) -> Self {
        Self {
            severity,
            kind,
            correction_index: None,
            level_index: Some(level_index),
            end_level_index: None,
            message: message.into(),
        }
    }

    fn general(severity: QcSeverity, kind: QcIssueKind, message: impl Into<String>) -> Self {
        Self {
            severity,
            kind,
            correction_index: None,
            level_index: None,
            end_level_index: None,
            message: message.into(),
        }
    }

    fn for_correction(mut self, correction_index: usize) -> Self {
        self.correction_index = Some(correction_index);
        self
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ConvectiveAdjustmentReport {
    pub attempted: bool,
    pub applied: bool,
    pub adjusted_levels: usize,
    pub mixed_blocks: usize,
    pub sensible_enthalpy_before_j_kg: f64,
    pub sensible_enthalpy_after_j_kg: f64,
    pub relative_enthalpy_residual: f64,
    pub aborted_reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CorrectionResult {
    pub column: SoundingColumn,
    pub issues: Vec<QcIssue>,
    pub convective_adjustment: ConvectiveAdjustmentReport,
}

impl CorrectionResult {
    pub(crate) fn has_errors(&self) -> bool {
        self.issues
            .iter()
            .any(|issue| issue.severity == QcSeverity::Error)
    }
}

#[derive(Clone, Debug)]
struct ScalarContribution {
    anchor: usize,
    weights: Vec<f64>,
    /// Full-strength change requested at every native level. Blend weights are
    /// applied only when the common piecewise profile is assembled.
    desired_delta: Vec<f64>,
}

fn combine_scalar_profile(
    source: &[f64],
    heights_m_msl: &[f64],
    contributions: &[ScalarContribution],
) -> Vec<f64> {
    if contributions.is_empty() {
        return source.to_vec();
    }
    let mut anchors: Vec<usize> = contributions.iter().map(|value| value.anchor).collect();
    anchors.sort_unstable();
    anchors.dedup();

    let group_delta = |anchor: usize, index: usize| -> Option<f64> {
        let values: Vec<_> = contributions
            .iter()
            .filter(|contribution| contribution.anchor == anchor)
            .filter_map(|contribution| contribution.desired_delta.get(index).copied())
            .filter(|value| value.is_finite())
            .collect();
        (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
    };
    let envelope_weight = |index: usize| -> f64 {
        contributions
            .iter()
            .filter_map(|contribution| contribution.weights.get(index).copied())
            .filter(|value| value.is_finite())
            .fold(0.0_f64, f64::max)
            .clamp(0.0, 1.0)
    };

    (0..source.len())
        .map(|index| {
            let first = anchors[0];
            let last = *anchors.last().expect("one anchor exists");
            if index <= first {
                let delta = group_delta(first, index).unwrap_or(0.0);
                return source[index] + envelope_weight(index) * delta;
            }
            if index >= last {
                let delta = group_delta(last, index).unwrap_or(0.0);
                return source[index] + envelope_weight(index) * delta;
            }

            for pair in anchors.windows(2) {
                let (left, right) = (pair[0], pair[1]);
                if index < left || index > right {
                    continue;
                }
                let left_delta = group_delta(left, index).unwrap_or(0.0);
                let right_delta = group_delta(right, index).unwrap_or(0.0);
                let left_height = heights_m_msl.get(left).copied().unwrap_or(left as f64);
                let right_height = heights_m_msl.get(right).copied().unwrap_or(right as f64);
                let height = heights_m_msl.get(index).copied().unwrap_or(index as f64);
                let fraction = if right_height > left_height {
                    ((height - left_height) / (right_height - left_height)).clamp(0.0, 1.0)
                } else {
                    (index - left) as f64 / (right - left).max(1) as f64
                };
                let interpolated_delta = left_delta + fraction * (right_delta - left_delta);
                return source[index] + envelope_weight(index) * interpolated_delta;
            }
            source[index]
        })
        .collect()
}

/// Evaluate a complete recipe from the untouched source column.
pub(crate) fn apply_correction_recipe(
    source: &SoundingColumn,
    recipe: &CorrectionRecipe,
) -> CorrectionResult {
    apply_correction_recipe_impl(source, recipe, recipe.convective_adjustment.enabled)
}

/// Preview the recipe with dry convective repair enabled without changing the
/// stored recipe.  Apply/undo in the UI is simply toggling `enabled` and
/// rebuilding from the untouched source.
pub(crate) fn preview_convective_adjustment(
    source: &SoundingColumn,
    recipe: &CorrectionRecipe,
) -> CorrectionResult {
    apply_correction_recipe_impl(source, recipe, true)
}

pub(crate) fn convert_thermal_target(
    target: ThermalTarget,
    mode: ThermalMode,
    pressure_hpa: f64,
) -> Option<ThermalTarget> {
    if !pressure_hpa.is_finite() || pressure_hpa <= MIN_PRESSURE_HPA {
        return None;
    }
    let temperature_c = match target {
        ThermalTarget::TemperatureC(value) => value,
        ThermalTarget::PotentialTemperatureK(value) => {
            temperature_c_from_potential_temperature_k(pressure_hpa, value)?
        }
    };
    if !temperature_c.is_finite() || temperature_c <= -ZERO_C_K {
        return None;
    }
    match mode {
        ThermalMode::Temperature => Some(ThermalTarget::TemperatureC(temperature_c)),
        ThermalMode::PotentialTemperature => Some(ThermalTarget::PotentialTemperatureK(
            potential_temperature_k(pressure_hpa, temperature_c)?,
        )),
    }
}

pub(crate) fn convert_moisture_target(
    target: MoistureTarget,
    mode: MoistureMode,
    pressure_hpa: f64,
) -> Option<MoistureTarget> {
    let specific_humidity = moisture_target_to_specific_humidity(target, pressure_hpa)?;
    specific_humidity_to_moisture_target(specific_humidity, mode, pressure_hpa)
}

pub(crate) fn convert_wind_target(target: WindTarget, mode: WindMode) -> Option<WindTarget> {
    let (u_ms, v_ms) = wind_target_to_uv(target)?;
    match mode {
        WindMode::Components => Some(WindTarget::UV { u_ms, v_ms }),
        WindMode::DirectionSpeed => {
            let (direction_deg, speed_kt) = uv_to_direction_speed_kt(u_ms, v_ms)?;
            Some(WindTarget::DirectionSpeed {
                direction_deg,
                speed_kt,
            })
        }
    }
}

pub(crate) fn normalize_control_points(points: &[BlendControlPoint]) -> Vec<BlendControlPoint> {
    let mut normalized: Vec<_> = points
        .iter()
        .copied()
        .filter(|point| point.x.is_finite() && point.y.is_finite())
        .map(|point| BlendControlPoint::new(point.x.clamp(0.0, 1.0), point.y.clamp(0.0, 1.0)))
        .collect();
    normalized.sort_by(|left, right| left.x.total_cmp(&right.x));

    // An absolute target must retain unit influence at the anchor and vanish
    // at the declared edge.  Treat user endpoint handles as pinned; all
    // interior handles remain fully editable.
    let mut safe = vec![BlendControlPoint::new(0.0, 1.0)];
    for point in normalized {
        if point.x <= f64::EPSILON || point.x >= 1.0 - f64::EPSILON {
            continue;
        }
        if let Some(previous) = safe.last_mut()
            && (previous.x - point.x).abs() <= 1.0e-9
        {
            // Stable sorting makes the later duplicate the user's most recent
            // effective handle at that x coordinate.
            previous.y = point.y;
        } else {
            safe.push(point);
        }
    }
    safe.push(BlendControlPoint::new(1.0, 0.0));
    safe
}

/// Public to the UI so the custom-curve editor can draw the exact function
/// that the correction engine will evaluate.
pub(crate) fn blend_weight(
    spec: &BlendSpec,
    height_m_msl: f64,
    anchor_height_m_msl: f64,
    surface_height_m_msl: f64,
) -> f64 {
    if !height_m_msl.is_finite()
        || !anchor_height_m_msl.is_finite()
        || !surface_height_m_msl.is_finite()
    {
        return 0.0;
    }
    let spec = spec.normalized();
    let delta = height_m_msl - anchor_height_m_msl;
    if spec.depth_m <= f64::EPSILON {
        return match spec.extent {
            BlendExtent::SymmetricLocal | BlendExtent::UpwardFromAnchor => {
                if delta.abs() <= 1.0e-6 { 1.0 } else { 0.0 }
            }
            BlendExtent::SurfaceLayer => {
                if height_m_msl + 1.0e-6 >= surface_height_m_msl && delta <= 1.0e-6 {
                    1.0
                } else {
                    0.0
                }
            }
        };
    }

    let x = match spec.extent {
        BlendExtent::SymmetricLocal => delta.abs() / spec.depth_m,
        BlendExtent::UpwardFromAnchor => {
            if delta < -1.0e-6 {
                return 0.0;
            }
            delta.max(0.0) / spec.depth_m
        }
        BlendExtent::SurfaceLayer => {
            if height_m_msl < surface_height_m_msl - 1.0e-6 {
                return 0.0;
            }
            if delta <= 0.0 {
                0.0
            } else {
                delta / spec.depth_m
            }
        }
    };
    shape_weight(&spec.shape, x)
}

fn apply_correction_recipe_impl(
    source: &SoundingColumn,
    recipe: &CorrectionRecipe,
    apply_adjustment: bool,
) -> CorrectionResult {
    if let Err(error) = source.validate() {
        return CorrectionResult {
            column: source.clone(),
            issues: vec![QcIssue::general(
                QcSeverity::Error,
                QcIssueKind::Structural,
                format!("Source sounding is structurally invalid: {error}"),
            )],
            convective_adjustment: ConvectiveAdjustmentReport::default(),
        };
    }

    let mut corrected = source.clone();
    let mut issues = Vec::new();
    let mut specific_humidity: Vec<f64> = source
        .pressure_hpa
        .iter()
        .zip(&source.dewpoint_c)
        .map(|(&pressure_hpa, &dewpoint_c)| {
            specific_humidity_from_dewpoint(pressure_hpa, dewpoint_c).unwrap_or(f64::NAN)
        })
        .collect();
    let surface_height = source.height_m_msl[0];
    let mut thermal_contributions = Vec::new();
    let mut moisture_contributions = Vec::new();
    let mut u_contributions = Vec::new();
    let mut v_contributions = Vec::new();

    for (recipe_index, level) in recipe
        .levels
        .iter()
        .enumerate()
        .filter(|(_, level)| level.is_active())
    {
        if !level.target_agl_m.is_finite() {
            issues.push(
                QcIssue::general(
                    QcSeverity::Error,
                    QcIssueKind::InvalidTarget,
                    format!(
                        "Correction row {} has a non-finite target height",
                        recipe_index + 1
                    ),
                )
                .for_correction(recipe_index),
            );
            continue;
        }
        let Some(anchor) = nearest_native_level(source, level.target_agl_m) else {
            issues.push(
                QcIssue::general(
                    QcSeverity::Error,
                    QcIssueKind::InvalidTarget,
                    format!(
                        "Correction row {} has no usable native anchor",
                        recipe_index + 1
                    ),
                )
                .for_correction(recipe_index),
            );
            continue;
        };
        let anchor_height = source.height_m_msl[anchor];

        if let Some(edit) = &level.thermal {
            let weights = weights_for(source, &edit.blend, anchor_height, surface_height);
            let desired_delta = match edit.target {
                ThermalTarget::TemperatureC(target) if target.is_finite() && target > -ZERO_C_K => {
                    Some(vec![target - source.temperature_c[anchor]; source.len()])
                }
                ThermalTarget::PotentialTemperatureK(target)
                    if target.is_finite() && target > 0.0 =>
                {
                    let Some(anchor_theta) = potential_temperature_k(
                        source.pressure_hpa[anchor],
                        source.temperature_c[anchor],
                    ) else {
                        issues.push(
                            QcIssue::at_level(
                                QcSeverity::Error,
                                QcIssueKind::InvalidTarget,
                                anchor,
                                format!(
                                    "Correction row {} cannot derive anchor theta",
                                    recipe_index + 1
                                ),
                            )
                            .for_correction(recipe_index),
                        );
                        continue;
                    };
                    let delta_theta = target - anchor_theta;
                    let mut deltas = Vec::with_capacity(source.len());
                    for index in 0..source.len() {
                        let delta_temperature = potential_temperature_k(
                            source.pressure_hpa[index],
                            source.temperature_c[index],
                        )
                        .and_then(|theta| {
                            temperature_c_from_potential_temperature_k(
                                source.pressure_hpa[index],
                                theta + delta_theta,
                            )
                        })
                        .map(|target_temperature| target_temperature - source.temperature_c[index])
                        .unwrap_or(0.0);
                        deltas.push(delta_temperature);
                    }
                    Some(deltas)
                }
                _ => {
                    issues.push(
                        QcIssue::at_level(
                            QcSeverity::Error,
                            QcIssueKind::InvalidTarget,
                            anchor,
                            format!(
                                "Correction row {} has an invalid thermal target",
                                recipe_index + 1
                            ),
                        )
                        .for_correction(recipe_index),
                    );
                    None
                }
            };
            if let Some(desired_delta) = desired_delta {
                thermal_contributions.push(ScalarContribution {
                    anchor,
                    weights,
                    desired_delta,
                });
            }
        }

        if let Some(edit) = &level.moisture {
            let weights = weights_for(source, &edit.blend, anchor_height, surface_height);
            if let Some(target) =
                moisture_target_to_specific_humidity(edit.target, source.pressure_hpa[anchor])
            {
                let delta = target - specific_humidity[anchor];
                moisture_contributions.push(ScalarContribution {
                    anchor,
                    weights,
                    desired_delta: vec![delta; source.len()],
                });
            } else {
                issues.push(
                    QcIssue::at_level(
                        QcSeverity::Error,
                        QcIssueKind::InvalidTarget,
                        anchor,
                        format!(
                            "Correction row {} has an invalid moisture target",
                            recipe_index + 1
                        ),
                    )
                    .for_correction(recipe_index),
                );
            }
        }

        if let Some(edit) = &level.wind {
            let weights = weights_for(source, &edit.blend, anchor_height, surface_height);
            if let Some((target_u, target_v)) = wind_target_to_uv(edit.target) {
                u_contributions.push(ScalarContribution {
                    anchor,
                    weights: weights.clone(),
                    desired_delta: vec![target_u - source.u_ms[anchor]; source.len()],
                });
                v_contributions.push(ScalarContribution {
                    anchor,
                    weights,
                    desired_delta: vec![target_v - source.v_ms[anchor]; source.len()],
                });
            } else {
                issues.push(
                    QcIssue::at_level(
                        QcSeverity::Error,
                        QcIssueKind::InvalidTarget,
                        anchor,
                        format!(
                            "Correction row {} has an invalid wind target",
                            recipe_index + 1
                        ),
                    )
                    .for_correction(recipe_index),
                );
            }
        }
    }

    // Build each corrected field once from the untouched source. Overlapping
    // edits form a height-local, order-independent target profile: the
    // strongest envelope retains the requested taper while the participating
    // absolute deltas are smoothly blended. Exact native anchors remain exact;
    // conflicting edits snapped to the same anchor resolve to their mean.
    corrected.temperature_c = combine_scalar_profile(
        &source.temperature_c,
        &source.height_m_msl,
        &thermal_contributions,
    );
    specific_humidity = combine_scalar_profile(
        &specific_humidity,
        &source.height_m_msl,
        &moisture_contributions,
    );
    corrected.u_ms = combine_scalar_profile(&source.u_ms, &source.height_m_msl, &u_contributions);
    corrected.v_ms = combine_scalar_profile(&source.v_ms, &source.height_m_msl, &v_contributions);

    reconstruct_dewpoint(&mut corrected, &specific_humidity, &mut issues);

    let mut adjustment_report = ConvectiveAdjustmentReport::default();
    if apply_adjustment {
        let (candidate, report, adjustment_issue) = repair_dry_static_instability(
            &corrected,
            &specific_humidity,
            &recipe.convective_adjustment,
        );
        corrected = candidate;
        adjustment_report = report;
        if let Some(issue) = adjustment_issue {
            issues.push(issue);
        }
    }

    run_quality_control(
        source,
        &corrected,
        &specific_humidity,
        &recipe.qc,
        &mut issues,
    );

    CorrectionResult {
        column: corrected,
        issues,
        convective_adjustment: adjustment_report,
    }
}

fn potential_temperature_k(pressure_hpa: f64, temperature_c: f64) -> Option<f64> {
    if !pressure_hpa.is_finite()
        || pressure_hpa <= MIN_PRESSURE_HPA
        || !temperature_c.is_finite()
        || temperature_c <= -ZERO_C_K
    {
        return None;
    }
    let theta_k = thermo::theta(pressure_hpa, temperature_c, 1_000.0) + ZERO_C_K;
    (theta_k.is_finite() && theta_k > 0.0).then_some(theta_k)
}

fn temperature_c_from_potential_temperature_k(pressure_hpa: f64, theta_k: f64) -> Option<f64> {
    if !pressure_hpa.is_finite()
        || pressure_hpa <= MIN_PRESSURE_HPA
        || !theta_k.is_finite()
        || theta_k <= 0.0
    {
        return None;
    }
    let temperature_c = theta_k * (pressure_hpa / 1_000.0).powf(ROCP) - ZERO_C_K;
    (temperature_c.is_finite() && temperature_c > -ZERO_C_K).then_some(temperature_c)
}

fn moisture_target_to_specific_humidity(target: MoistureTarget, pressure_hpa: f64) -> Option<f64> {
    if !pressure_hpa.is_finite() || pressure_hpa <= MIN_PRESSURE_HPA {
        return None;
    }
    let q = match target {
        MoistureTarget::DewpointC(dewpoint_c) => {
            specific_humidity_from_dewpoint(pressure_hpa, dewpoint_c)?
        }
        MoistureTarget::MixingRatioGKg(mixing_ratio_g_kg) => {
            mixing_ratio_g_kg_to_specific_humidity(mixing_ratio_g_kg)?
        }
        MoistureTarget::SpecificHumidityGKg(specific_humidity_g_kg) => {
            specific_humidity_g_kg / 1_000.0
        }
    };
    (q.is_finite() && q >= 0.0 && q < 1.0).then_some(q)
}

fn specific_humidity_to_moisture_target(
    specific_humidity: f64,
    mode: MoistureMode,
    pressure_hpa: f64,
) -> Option<MoistureTarget> {
    if !specific_humidity.is_finite()
        || !(0.0..=MAX_SPECIFIC_HUMIDITY).contains(&specific_humidity)
        || !pressure_hpa.is_finite()
        || pressure_hpa <= MIN_PRESSURE_HPA
    {
        return None;
    }
    match mode {
        MoistureMode::Dewpoint => Some(MoistureTarget::DewpointC(dewpoint_from_specific_humidity(
            pressure_hpa,
            specific_humidity,
        )?)),
        MoistureMode::MixingRatio => Some(MoistureTarget::MixingRatioGKg(
            specific_humidity_to_mixing_ratio_g_kg(specific_humidity)?,
        )),
        MoistureMode::SpecificHumidity => Some(MoistureTarget::SpecificHumidityGKg(
            specific_humidity * 1_000.0,
        )),
    }
}

fn specific_humidity_from_dewpoint(pressure_hpa: f64, dewpoint_c: f64) -> Option<f64> {
    if !pressure_hpa.is_finite()
        || pressure_hpa <= MIN_PRESSURE_HPA
        || !dewpoint_c.is_finite()
        || dewpoint_c < -ZERO_C_K
    {
        return None;
    }
    if dewpoint_c == -ZERO_C_K {
        return Some(0.0);
    }
    mixing_ratio_g_kg_to_specific_humidity(thermo::mixratio(pressure_hpa, dewpoint_c))
}

fn dewpoint_from_specific_humidity(pressure_hpa: f64, specific_humidity: f64) -> Option<f64> {
    if specific_humidity == 0.0 {
        // The mathematical dewpoint of perfectly dry air is -infinity. A
        // finite absolute-zero sentinel keeps SoundingColumn structurally
        // representable while QC and the UI can still report q=0 exactly.
        return Some(-ZERO_C_K);
    }
    let mixing_ratio_g_kg = specific_humidity_to_mixing_ratio_g_kg(specific_humidity)?;
    dewpoint_from_matching_mixratio(pressure_hpa, mixing_ratio_g_kg)
}

/// Numerically invert the exact `sharprs::thermo::mixratio` function used by
/// the forward Td -> q conversion. `temp_at_mixrat` is a different empirical
/// approximation and can shift saturated profiles by 0.02-0.15 C, enough to
/// manufacture false supersaturation after an otherwise no-op correction.
fn dewpoint_from_matching_mixratio(
    pressure_hpa: f64,
    target_mixing_ratio_g_kg: f64,
) -> Option<f64> {
    if !pressure_hpa.is_finite()
        || pressure_hpa <= MIN_PRESSURE_HPA
        || !target_mixing_ratio_g_kg.is_finite()
        || target_mixing_ratio_g_kg < 0.0
    {
        return None;
    }
    if target_mixing_ratio_g_kg == 0.0 {
        return Some(-ZERO_C_K);
    }

    // Do not use one warm fixed endpoint: aloft it can lie beyond the
    // e_s(T)=p denominator pole, where mixratio becomes finite-but-negative.
    // Walk monotonically from the cold side and stop at the first positive
    // bracket before that pole.
    let mut lower = -150.0;
    let mut lower_value = thermo::mixratio(pressure_hpa, lower);
    if !lower_value.is_finite() || lower_value < 0.0 || lower_value > target_mixing_ratio_g_kg {
        return None;
    }
    let mut bracket = None;
    for step in 1..=1_000 {
        let candidate = -150.0 + step as f64 * 0.25;
        let value = thermo::mixratio(pressure_hpa, candidate);
        if !value.is_finite() || value < 0.0 || value + 1.0e-12 < lower_value {
            break;
        }
        if value >= target_mixing_ratio_g_kg {
            bracket = Some((lower, candidate));
            break;
        }
        lower = candidate;
        lower_value = value;
    }
    let (mut lower, mut upper) = bracket?;

    for _ in 0..80 {
        let midpoint = 0.5 * (lower + upper);
        let value = thermo::mixratio(pressure_hpa, midpoint);
        if !value.is_finite() {
            upper = midpoint;
        } else if value < target_mixing_ratio_g_kg {
            lower = midpoint;
        } else {
            upper = midpoint;
        }
    }
    let dewpoint = 0.5 * (lower + upper);
    dewpoint.is_finite().then_some(dewpoint)
}

fn mixing_ratio_g_kg_to_specific_humidity(mixing_ratio_g_kg: f64) -> Option<f64> {
    if !mixing_ratio_g_kg.is_finite() || mixing_ratio_g_kg < 0.0 {
        return None;
    }
    let mixing_ratio = mixing_ratio_g_kg / 1_000.0;
    let specific_humidity = mixing_ratio / (1.0 + mixing_ratio);
    (specific_humidity >= 0.0 && specific_humidity < 1.0).then_some(specific_humidity)
}

fn specific_humidity_to_mixing_ratio_g_kg(specific_humidity: f64) -> Option<f64> {
    if !specific_humidity.is_finite() || specific_humidity < 0.0 || specific_humidity >= 1.0 {
        return None;
    }
    Some(specific_humidity / (1.0 - specific_humidity) * 1_000.0)
}

fn wind_target_to_uv(target: WindTarget) -> Option<(f64, f64)> {
    match target {
        WindTarget::UV { u_ms, v_ms } if u_ms.is_finite() && v_ms.is_finite() => Some((u_ms, v_ms)),
        WindTarget::DirectionSpeed {
            direction_deg,
            speed_kt,
        } if direction_deg.is_finite() && speed_kt.is_finite() && speed_kt >= 0.0 => {
            let radians = direction_deg.rem_euclid(360.0).to_radians();
            let speed_ms = speed_kt / MS_TO_KT;
            Some((-speed_ms * radians.sin(), -speed_ms * radians.cos()))
        }
        _ => None,
    }
}

fn uv_to_direction_speed_kt(u_ms: f64, v_ms: f64) -> Option<(f64, f64)> {
    if !u_ms.is_finite() || !v_ms.is_finite() {
        return None;
    }
    let speed_ms = u_ms.hypot(v_ms);
    if speed_ms <= f64::EPSILON {
        return Some((0.0, 0.0));
    }
    let direction_deg = (-u_ms).atan2(-v_ms).to_degrees().rem_euclid(360.0);
    Some((direction_deg, speed_ms * MS_TO_KT))
}

fn shape_weight(shape: &BlendShape, x: f64) -> f64 {
    if !x.is_finite() || x >= 1.0 {
        return 0.0;
    }
    if x <= 0.0 {
        return 1.0;
    }
    match shape {
        BlendShape::Cosine => 0.5 * (1.0 + (std::f64::consts::PI * x).cos()),
        BlendShape::Linear => 1.0 - x,
        BlendShape::LayerConstantUpperCosine { taper_fraction } => {
            let taper_fraction = if taper_fraction.is_finite() {
                taper_fraction.clamp(0.01, 1.0)
            } else {
                0.25
            };
            let taper_start = 1.0 - taper_fraction;
            if x <= taper_start {
                1.0
            } else {
                let taper_x = (x - taper_start) / taper_fraction;
                0.5 * (1.0 + (std::f64::consts::PI * taper_x).cos())
            }
        }
        BlendShape::Custom { points } => {
            let points = normalize_control_points(points);
            for pair in points.windows(2) {
                if x <= pair[1].x {
                    let span = pair[1].x - pair[0].x;
                    if span <= f64::EPSILON {
                        return pair[1].y;
                    }
                    let fraction = (x - pair[0].x) / span;
                    return (pair[0].y + fraction * (pair[1].y - pair[0].y)).clamp(0.0, 1.0);
                }
            }
            0.0
        }
    }
    .clamp(0.0, 1.0)
}

fn weights_for(
    source: &SoundingColumn,
    blend: &BlendSpec,
    anchor_height: f64,
    surface_height: f64,
) -> Vec<f64> {
    source
        .height_m_msl
        .iter()
        .map(|&height| blend_weight(blend, height, anchor_height, surface_height))
        .collect()
}

pub(crate) fn nearest_native_level(column: &SoundingColumn, target_agl_m: f64) -> Option<usize> {
    let surface_m = *column.height_m_msl.first()?;
    let target_msl = surface_m + target_agl_m.max(0.0);
    column
        .height_m_msl
        .iter()
        .enumerate()
        .filter(|(_, height)| height.is_finite())
        .min_by(|(_, left), (_, right)| {
            (*left - target_msl)
                .abs()
                .total_cmp(&(*right - target_msl).abs())
        })
        .map(|(index, _)| index)
}

fn reconstruct_dewpoint(
    column: &mut SoundingColumn,
    specific_humidity: &[f64],
    issues: &mut Vec<QcIssue>,
) {
    for index in 0..column.len() {
        match dewpoint_from_specific_humidity(column.pressure_hpa[index], specific_humidity[index])
        {
            Some(dewpoint) => column.dewpoint_c[index] = dewpoint,
            None => {
                column.dewpoint_c[index] = f64::NAN;
                issues.push(QcIssue::at_level(
                    QcSeverity::Error,
                    QcIssueKind::InvalidMoisture,
                    index,
                    format!(
                        "Specific humidity at level {index} is not representable (q={})",
                        specific_humidity[index]
                    ),
                ));
            }
        }
    }
}

fn repair_dry_static_instability(
    column: &SoundingColumn,
    specific_humidity: &[f64],
    config: &ConvectiveAdjustmentConfig,
) -> (SoundingColumn, ConvectiveAdjustmentReport, Option<QcIssue>) {
    const CP_DRY_J_KG_K: f64 = 1_005.7;

    let mut report = ConvectiveAdjustmentReport {
        attempted: true,
        ..Default::default()
    };
    let n = column.len();
    if n < 2 || specific_humidity.len() != n {
        let reason = "Convective adjustment needs matching thermodynamic columns".to_owned();
        report.aborted_reason = Some(reason.clone());
        return (
            column.clone(),
            report,
            Some(QcIssue::general(
                QcSeverity::Error,
                QcIssueKind::ConvectiveAdjustmentAborted,
                reason,
            )),
        );
    }

    let protected_depth = if config.protected_surface_depth_m.is_finite() {
        config.protected_surface_depth_m.max(0.0)
    } else {
        100.0
    };
    let surface_height = column.height_m_msl[0];
    let Some(first) = column
        .height_m_msl
        .iter()
        .position(|height| *height - surface_height >= protected_depth)
    else {
        return (column.clone(), report, None);
    };
    if first + 1 >= n {
        return (column.clone(), report, None);
    }

    let mass = pressure_layer_mass_weights(&column.pressure_hpa);
    let mut theta = Vec::with_capacity(n);
    let mut exner = Vec::with_capacity(n);
    for index in 0..n {
        let Some(value) =
            potential_temperature_k(column.pressure_hpa[index], column.temperature_c[index])
        else {
            let reason = format!("Cannot derive potential temperature at level {index}");
            report.aborted_reason = Some(reason.clone());
            return (
                column.clone(),
                report,
                Some(QcIssue::at_level(
                    QcSeverity::Error,
                    QcIssueKind::ConvectiveAdjustmentAborted,
                    index,
                    reason,
                )),
            );
        };
        theta.push(value);
        exner.push((column.pressure_hpa[index] / 1_000.0).powf(ROCP));
    }

    #[derive(Clone, Debug)]
    struct PavaBlock {
        start: usize,
        end: usize,
        weighted_theta_sum: f64,
        weight_sum: f64,
    }

    impl PavaBlock {
        fn mean(&self) -> f64 {
            self.weighted_theta_sum / self.weight_sum
        }
    }

    let mut blocks: Vec<PavaBlock> = Vec::new();
    for index in first..n {
        // Weighting theta by m*Pi means the pooled theta is
        // sum(m*T)/sum(m*Pi), exactly conserving discrete sensible enthalpy
        // when the pooled levels are reconstructed as T=theta*Pi.
        let weight = (mass[index] * exner[index]).max(1.0e-12);
        blocks.push(PavaBlock {
            start: index,
            end: index,
            weighted_theta_sum: weight * theta[index],
            weight_sum: weight,
        });
        while blocks.len() >= 2 {
            let right = blocks.len() - 1;
            let left = right - 1;
            if blocks[left].mean() <= blocks[right].mean() + 1.0e-12 {
                break;
            }
            let right_block = blocks.pop().expect("right PAVA block exists");
            let left_block = blocks.pop().expect("left PAVA block exists");
            blocks.push(PavaBlock {
                start: left_block.start,
                end: right_block.end,
                weighted_theta_sum: left_block.weighted_theta_sum + right_block.weighted_theta_sum,
                weight_sum: left_block.weight_sum + right_block.weight_sum,
            });
        }
    }

    let mut adjusted_theta = theta.clone();
    report.mixed_blocks = blocks
        .iter()
        .filter(|block| block.end > block.start)
        .count();
    for block in &blocks {
        let mean = block.mean();
        for value in &mut adjusted_theta[block.start..=block.end] {
            *value = mean;
        }
    }

    let mut candidate = column.clone();
    let affected_indices: Vec<_> = blocks
        .iter()
        .filter(|block| block.end > block.start)
        .flat_map(|block| block.start..=block.end)
        .collect();
    for index in first..n {
        if (adjusted_theta[index] - theta[index]).abs() > 1.0e-9 {
            report.adjusted_levels += 1;
        }
        if let Some(temperature) = temperature_c_from_potential_temperature_k(
            candidate.pressure_hpa[index],
            adjusted_theta[index],
        ) {
            candidate.temperature_c[index] = temperature;
        }
    }
    if report.adjusted_levels == 0 {
        return (column.clone(), report, None);
    }

    let total_mass: f64 = mass[first..].iter().sum::<f64>().max(1.0e-12);
    report.sensible_enthalpy_before_j_kg = CP_DRY_J_KG_K
        * mass[first..]
            .iter()
            .zip(&column.temperature_c[first..])
            .map(|(mass, temperature)| mass * (temperature + ZERO_C_K))
            .sum::<f64>()
        / total_mass;
    report.sensible_enthalpy_after_j_kg = CP_DRY_J_KG_K
        * mass[first..]
            .iter()
            .zip(&candidate.temperature_c[first..])
            .map(|(mass, temperature)| mass * (temperature + ZERO_C_K))
            .sum::<f64>()
        / total_mass;
    report.relative_enthalpy_residual =
        (report.sensible_enthalpy_after_j_kg - report.sensible_enthalpy_before_j_kg).abs()
            / report.sensible_enthalpy_before_j_kg.abs().max(1.0);

    const SATURATION_EPSILON: f64 = 1.0e-10;
    for &index in &affected_indices {
        let Some(initial_saturation_q) = specific_humidity_from_dewpoint(
            column.pressure_hpa[index],
            column.temperature_c[index],
        ) else {
            let reason = format!(
                "Dry convective adjustment cannot evaluate initial saturation at level {index}"
            );
            report.aborted_reason = Some(reason.clone());
            return (
                column.clone(),
                report,
                Some(QcIssue::at_level(
                    QcSeverity::Error,
                    QcIssueKind::ConvectiveAdjustmentAborted,
                    index,
                    reason,
                )),
            );
        };
        let Some(candidate_saturation_q) = specific_humidity_from_dewpoint(
            candidate.pressure_hpa[index],
            candidate.temperature_c[index],
        ) else {
            let reason = format!(
                "Dry convective adjustment cannot evaluate candidate saturation at level {index}"
            );
            report.aborted_reason = Some(reason.clone());
            return (
                column.clone(),
                report,
                Some(QcIssue::at_level(
                    QcSeverity::Error,
                    QcIssueKind::ConvectiveAdjustmentAborted,
                    index,
                    reason,
                )),
            );
        };
        let initially_saturated =
            specific_humidity[index] >= initial_saturation_q * (1.0 - SATURATION_EPSILON);
        let candidate_saturated =
            specific_humidity[index] >= candidate_saturation_q * (1.0 - SATURATION_EPSILON);
        if initially_saturated || candidate_saturated {
            let initial_rh = relative_humidity_percent(
                column.pressure_hpa[index],
                specific_humidity[index],
                initial_saturation_q,
            )
            .unwrap_or(f64::NAN);
            let candidate_rh = relative_humidity_percent(
                candidate.pressure_hpa[index],
                specific_humidity[index],
                candidate_saturation_q,
            )
            .unwrap_or(f64::NAN);
            let reason = format!(
                "Dry convective adjustment cannot safely mix level {index}: RH before {initial_rh:.2}%, after {candidate_rh:.2}%"
            );
            report.applied = false;
            report.aborted_reason = Some(reason.clone());
            return (
                column.clone(),
                report,
                Some(QcIssue::at_level(
                    QcSeverity::Error,
                    QcIssueKind::ConvectiveAdjustmentAborted,
                    index,
                    reason,
                )),
            );
        }
    }

    report.applied = true;
    (candidate, report, None)
}

fn pressure_layer_mass_weights(pressure_hpa: &[f64]) -> Vec<f64> {
    const GRAVITY_MS2: f64 = 9.806_65;
    let n = pressure_hpa.len();
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        return vec![1.0];
    }
    (0..n)
        .map(|index| {
            let lower_hpa = if index == 0 {
                pressure_hpa[0] + 0.5 * (pressure_hpa[0] - pressure_hpa[1]).abs()
            } else {
                0.5 * (pressure_hpa[index - 1] + pressure_hpa[index])
            };
            let upper_hpa = if index + 1 == n {
                (pressure_hpa[index] - 0.5 * (pressure_hpa[index - 1] - pressure_hpa[index]).abs())
                    .max(0.0)
            } else {
                0.5 * (pressure_hpa[index] + pressure_hpa[index + 1])
            };
            ((lower_hpa - upper_hpa).abs() * 100.0 / GRAVITY_MS2).max(1.0e-12)
        })
        .collect()
}

fn run_quality_control(
    source: &SoundingColumn,
    corrected: &SoundingColumn,
    specific_humidity: &[f64],
    config: &QcConfig,
    issues: &mut Vec<QcIssue>,
) {
    let n = corrected.len();
    let lengths_match = corrected.height_m_msl.len() == n
        && corrected.temperature_c.len() == n
        && corrected.dewpoint_c.len() == n
        && corrected.u_ms.len() == n
        && corrected.v_ms.len() == n
        && specific_humidity.len() == n;
    if n < 2 || !lengths_match {
        issues.push(QcIssue::general(
            QcSeverity::Error,
            QcIssueKind::Structural,
            "Corrected sounding arrays do not have matching usable lengths",
        ));
        return;
    }

    for index in 0..n {
        let structural_values = [
            corrected.pressure_hpa[index],
            corrected.height_m_msl[index],
            corrected.temperature_c[index],
            corrected.dewpoint_c[index],
            corrected.u_ms[index],
            corrected.v_ms[index],
        ];
        if structural_values.iter().any(|value| !value.is_finite()) {
            issues.push(QcIssue::at_level(
                QcSeverity::Error,
                QcIssueKind::Structural,
                index,
                format!("Corrected sounding has a non-finite value at level {index}"),
            ));
        }
        let q = specific_humidity[index];
        if !q.is_finite() || q < 0.0 || q >= 1.0 {
            if !issues.iter().any(|issue| {
                issue.kind == QcIssueKind::InvalidMoisture && issue.level_index == Some(index)
            }) {
                issues.push(QcIssue::at_level(
                    QcSeverity::Error,
                    QcIssueKind::InvalidMoisture,
                    index,
                    format!("Specific humidity is outside (0,1) at level {index}: {q}"),
                ));
            }
            continue;
        }
        if let Some(saturation_q) = specific_humidity_from_dewpoint(
            corrected.pressure_hpa[index],
            corrected.temperature_c[index],
        ) {
            let rh = relative_humidity_percent(corrected.pressure_hpa[index], q, saturation_q)
                .unwrap_or(f64::NAN);
            let tolerance = normalized_nonnegative(config.supersaturation_tolerance_percent, 0.5);
            if q > saturation_q * (1.0 + tolerance / 100.0)
                || corrected.dewpoint_c[index] > corrected.temperature_c[index] + 0.1
            {
                issues.push(QcIssue::at_level(
                    QcSeverity::Warning,
                    QcIssueKind::Supersaturation,
                    index,
                    format!(
                        "Supersaturated level {index}: q={:.5} g/kg specific, RH={rh:.2}%, Td={:.2} C > T={:.2} C",
                        q * 1_000.0,
                        corrected.dewpoint_c[index],
                        corrected.temperature_c[index]
                    ),
                ));
            }
        }
    }

    dry_stability_qc(corrected, config, issues);
    wind_kink_qc(source, corrected, config, issues);
}

fn dry_stability_qc(column: &SoundingColumn, config: &QcConfig, issues: &mut Vec<QcIssue>) {
    let theta: Vec<_> = column
        .pressure_hpa
        .iter()
        .zip(&column.temperature_c)
        .map(|(&pressure, &temperature)| potential_temperature_k(pressure, temperature))
        .collect();
    let drop_tolerance = normalized_nonnegative(config.dry_instability_theta_drop_k, 0.05);
    let min_depth = normalized_nonnegative(config.dry_instability_min_depth_m, 100.0);
    let protected_depth =
        normalized_nonnegative(config.dry_stability_protected_surface_depth_m, 100.0);
    let surface_height = column.height_m_msl[0];
    let first = column
        .height_m_msl
        .iter()
        .position(|height| *height - surface_height >= protected_depth)
        .unwrap_or(column.len().saturating_sub(1));
    let mut start = None;
    for index in first..column.len() - 1 {
        let unstable = theta[index]
            .zip(theta[index + 1])
            .is_some_and(|(lower, upper)| upper < lower - drop_tolerance);
        if unstable {
            start.get_or_insert(index);
        }
        if (!unstable || index + 2 == column.len())
            && let Some(run_start) = start.take()
        {
            let run_end = if unstable { index + 1 } else { index };
            let depth = column.height_m_msl[run_end] - column.height_m_msl[run_start];
            if depth >= min_depth {
                issues.push(QcIssue {
                    severity: QcSeverity::Warning,
                    kind: QcIssueKind::DryStaticInstability,
                    correction_index: None,
                    level_index: Some(run_start),
                    end_level_index: Some(run_end),
                    message: format!(
                        "Potential temperature decreases through a {depth:.0} m layer (levels {run_start}-{run_end})"
                    ),
                });
            }
        }
    }
}

fn wind_kink_qc(
    source: &SoundingColumn,
    corrected: &SoundingColumn,
    config: &QcConfig,
    issues: &mut Vec<QcIssue>,
) {
    let absolute = normalized_nonnegative(config.wind_kink_absolute_s_inv, 0.015);
    let increase = normalized_nonnegative(config.wind_kink_increase_s_inv, 0.005);
    for index in 1..corrected.len() - 1 {
        let Some(corrected_kink) = shear_gradient_kink(corrected, index) else {
            continue;
        };
        let source_kink = shear_gradient_kink(source, index).unwrap_or(0.0);
        if corrected_kink >= absolute && corrected_kink - source_kink >= increase {
            issues.push(QcIssue::at_level(
                QcSeverity::Warning,
                QcIssueKind::WindShearKink,
                index,
                format!(
                    "Wind-vector shear has a blend-seam kink at level {index} ({corrected_kink:.4} s^-1; source {source_kink:.4} s^-1)"
                ),
            ));
        }
    }
}

fn shear_gradient_kink(column: &SoundingColumn, index: usize) -> Option<f64> {
    let dz_below = column.height_m_msl[index] - column.height_m_msl[index - 1];
    let dz_above = column.height_m_msl[index + 1] - column.height_m_msl[index];
    if !dz_below.is_finite()
        || !dz_above.is_finite()
        || dz_below <= f64::EPSILON
        || dz_above <= f64::EPSILON
    {
        return None;
    }
    let du_below = (column.u_ms[index] - column.u_ms[index - 1]) / dz_below;
    let dv_below = (column.v_ms[index] - column.v_ms[index - 1]) / dz_below;
    let du_above = (column.u_ms[index + 1] - column.u_ms[index]) / dz_above;
    let dv_above = (column.v_ms[index + 1] - column.v_ms[index]) / dz_above;
    Some((du_above - du_below).hypot(dv_above - dv_below))
}

fn relative_humidity_percent(pressure_hpa: f64, q: f64, saturation_q: f64) -> Option<f64> {
    let vapor_pressure = vapor_pressure_from_specific_humidity(pressure_hpa, q)?;
    let saturation_vapor_pressure =
        vapor_pressure_from_specific_humidity(pressure_hpa, saturation_q)?;
    (saturation_vapor_pressure > 0.0).then_some(100.0 * vapor_pressure / saturation_vapor_pressure)
}

fn vapor_pressure_from_specific_humidity(pressure_hpa: f64, q: f64) -> Option<f64> {
    if !pressure_hpa.is_finite()
        || pressure_hpa <= 0.0
        || !q.is_finite()
        || !(0.0..1.0).contains(&q)
    {
        return None;
    }
    let mixing_ratio = q / (1.0 - q);
    Some(pressure_hpa * mixing_ratio / (WATER_VAPOR_MASS_RATIO + mixing_ratio))
}

fn normalized_nonnegative(value: f64, fallback: f64) -> f64 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        fallback
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(actual: f64, expected: f64, tolerance: f64) {
        assert!(
            (actual - expected).abs() <= tolerance,
            "expected {expected:.15}, got {actual:.15} (|delta|={:.3e}, tolerance={tolerance:.3e})",
            (actual - expected).abs()
        );
    }

    fn column(
        pressure_hpa: Vec<f64>,
        height_m_msl: Vec<f64>,
        temperature_c: Vec<f64>,
        dewpoint_c: Vec<f64>,
        u_ms: Vec<f64>,
        v_ms: Vec<f64>,
    ) -> SoundingColumn {
        let len = pressure_hpa.len();
        let column = SoundingColumn {
            pressure_hpa,
            height_m_msl,
            temperature_c,
            dewpoint_c,
            u_ms,
            v_ms,
            omega_pa_s: vec![0.0; len],
            metadata: rustwx_sounding::SoundingMetadata::default(),
        };
        column.validate().expect("test sounding must be valid");
        column
    }

    #[test]
    fn dewpoint_specific_humidity_roundtrip_is_exact_across_troposphere() {
        for pressure_hpa in [1_000.0, 500.0, 200.0, 100.0] {
            for dewpoint_c in [20.0, 0.0, -20.0, -50.0] {
                let q = specific_humidity_from_dewpoint(pressure_hpa, dewpoint_c)
                    .expect("forward Td to q conversion");
                let roundtrip = dewpoint_from_specific_humidity(pressure_hpa, q)
                    .expect("inverse q to Td conversion");
                assert_close(roundtrip, dewpoint_c, 1.0e-10);
            }
        }
    }

    #[test]
    fn no_op_recipe_does_not_manufacture_supersaturation() {
        let source = column(
            vec![1_000.0, 850.0, 700.0, 500.0, 300.0, 200.0, 100.0],
            vec![0.0, 1_500.0, 3_000.0, 5_500.0, 9_000.0, 12_000.0, 16_000.0],
            vec![25.0, 15.0, 5.0, -10.0, -30.0, -45.0, -60.0],
            vec![25.0, 15.0, 5.0, -10.0, -30.0, -45.0, -60.0],
            vec![0.0; 7],
            vec![0.0; 7],
        );

        let result = apply_correction_recipe(&source, &CorrectionRecipe::default());

        assert!(
            !result.has_errors(),
            "unexpected errors: {:?}",
            result.issues
        );
        assert!(
            result
                .issues
                .iter()
                .all(|issue| issue.kind != QcIssueKind::Supersaturation),
            "no-op roundtrip raised false supersaturation: {:?}",
            result.issues
        );
        for (actual, expected) in result.column.dewpoint_c.iter().zip(&source.dewpoint_c) {
            assert_close(*actual, *expected, 1.0e-10);
        }
    }

    #[test]
    fn multi_anchor_profiles_respect_weight_gaps_and_are_order_independent() {
        let source = column(
            vec![1_000.0, 900.0, 800.0, 700.0, 600.0, 500.0, 400.0],
            vec![0.0, 1_000.0, 2_000.0, 3_000.0, 4_000.0, 5_000.0, 6_000.0],
            vec![20.0, 15.0, 10.0, 5.0, 0.0, -5.0, -10.0],
            vec![-30.0; 7],
            vec![0.0; 7],
            vec![0.0; 7],
        );

        // Absolute targets with disjoint compact support must not create a
        // bridge through the gap merely because their anchor heights bracket
        // it.
        let mut far_low = CorrectionLevel::at_height(1_000.0);
        far_low.thermal = Some(ThermalEdit {
            target: ThermalTarget::TemperatureC(18.0),
            blend: BlendSpec::local_cosine(400.0),
        });
        let mut far_high = CorrectionLevel::at_height(5_000.0);
        far_high.thermal = Some(ThermalEdit {
            target: ThermalTarget::TemperatureC(-8.0),
            blend: BlendSpec::local_cosine(400.0),
        });
        let separated = apply_correction_recipe(
            &source,
            &CorrectionRecipe {
                levels: vec![far_low, far_high],
                ..Default::default()
            },
        );
        assert_close(separated.column.temperature_c[1], 18.0, 1.0e-12);
        assert_close(separated.column.temperature_c[5], -8.0, 1.0e-12);
        for index in 2..=4 {
            assert_close(
                separated.column.temperature_c[index],
                source.temperature_c[index],
                1.0e-12,
            );
        }

        // Overlapping tapers use one common, order-independent target
        // profile. Reversing the UI rows cannot change the meteorology.
        let mut low = CorrectionLevel::at_height(1_000.0);
        low.thermal = Some(ThermalEdit {
            target: ThermalTarget::TemperatureC(18.0),
            blend: BlendSpec::local_cosine(2_000.0),
        });
        let mut high = CorrectionLevel::at_height(3_000.0);
        high.thermal = Some(ThermalEdit {
            target: ThermalTarget::TemperatureC(-1.0),
            blend: BlendSpec::local_cosine(2_000.0),
        });

        let forward = apply_correction_recipe(
            &source,
            &CorrectionRecipe {
                levels: vec![low.clone(), high.clone()],
                ..Default::default()
            },
        );
        let reverse = apply_correction_recipe(
            &source,
            &CorrectionRecipe {
                levels: vec![high, low],
                ..Default::default()
            },
        );

        for (left, right) in forward
            .column
            .temperature_c
            .iter()
            .zip(&reverse.column.temperature_c)
        {
            assert_close(*left, *right, 1.0e-12);
        }
        assert_close(forward.column.temperature_c[1], 18.0, 1.0e-12);
        assert_close(forward.column.temperature_c[3], -1.0, 1.0e-12);
    }

    #[test]
    fn potential_temperature_mixed_layer_keeps_constant_offset_until_top_taper() {
        let pressure_hpa = vec![1_000.0, 950.0, 900.0, 850.0, 800.0, 750.0];
        let temperature_c: Vec<_> = pressure_hpa
            .iter()
            .map(|pressure| {
                temperature_c_from_potential_temperature_k(*pressure, 300.0)
                    .expect("theta to temperature")
            })
            .collect();
        let source = column(
            pressure_hpa,
            vec![0.0, 500.0, 1_000.0, 1_500.0, 2_000.0, 2_500.0],
            temperature_c,
            vec![-50.0; 6],
            vec![0.0; 6],
            vec![0.0; 6],
        );
        let mut level = CorrectionLevel::at_height(0.0);
        level.thermal = Some(ThermalEdit {
            target: ThermalTarget::PotentialTemperatureK(310.0),
            blend: BlendSpec::mixed_layer(2_000.0),
        });

        let result = apply_correction_recipe(
            &source,
            &CorrectionRecipe {
                levels: vec![level],
                ..Default::default()
            },
        );

        for index in 0..=3 {
            let theta = potential_temperature_k(
                result.column.pressure_hpa[index],
                result.column.temperature_c[index],
            )
            .expect("corrected theta");
            assert_close(theta, 310.0, 1.0e-10);
        }
        for index in 4..source.len() {
            let theta = potential_temperature_k(
                result.column.pressure_hpa[index],
                result.column.temperature_c[index],
            )
            .expect("uncorrected theta");
            assert_close(theta, 300.0, 1.0e-10);
        }
    }

    #[test]
    fn moisture_blends_in_q_and_direction_speed_wind_blends_in_uv() {
        let source_wind = wind_target_to_uv(WindTarget::DirectionSpeed {
            direction_deg: 350.0,
            speed_kt: 10.0 * MS_TO_KT,
        })
        .expect("source wind");
        let target_wind = wind_target_to_uv(WindTarget::DirectionSpeed {
            direction_deg: 10.0,
            speed_kt: 10.0 * MS_TO_KT,
        })
        .expect("target wind");
        let source = column(
            vec![1_000.0, 900.0, 800.0],
            vec![0.0, 1_000.0, 2_000.0],
            vec![25.0, 20.0, 15.0],
            vec![5.0, 0.0, -5.0],
            vec![source_wind.0; 3],
            vec![source_wind.1; 3],
        );
        let source_q: Vec<_> = source
            .pressure_hpa
            .iter()
            .zip(&source.dewpoint_c)
            .map(|(&pressure, &dewpoint)| {
                specific_humidity_from_dewpoint(pressure, dewpoint).expect("source q")
            })
            .collect();
        let blend = BlendSpec {
            depth_m: 2_000.0,
            extent: BlendExtent::SymmetricLocal,
            shape: BlendShape::Linear,
        };
        let mut level = CorrectionLevel::at_height(0.0);
        level.moisture = Some(MoistureEdit {
            target: MoistureTarget::SpecificHumidityGKg(10.0),
            blend: blend.clone(),
        });
        level.wind = Some(WindEdit {
            target: WindTarget::DirectionSpeed {
                direction_deg: 10.0,
                speed_kt: 10.0 * MS_TO_KT,
            },
            blend,
        });

        let result = apply_correction_recipe(
            &source,
            &CorrectionRecipe {
                levels: vec![level],
                ..Default::default()
            },
        );
        let corrected_q: Vec<_> = result
            .column
            .pressure_hpa
            .iter()
            .zip(&result.column.dewpoint_c)
            .map(|(&pressure, &dewpoint)| {
                specific_humidity_from_dewpoint(pressure, dewpoint).expect("corrected q")
            })
            .collect();

        assert_close(corrected_q[0], 0.010, 1.0e-12);
        assert_close(
            corrected_q[1],
            source_q[1] + 0.5 * (0.010 - source_q[0]),
            1.0e-12,
        );
        assert_close(corrected_q[2], source_q[2], 1.0e-12);

        assert_close(result.column.u_ms[0], target_wind.0, 1.0e-12);
        assert_close(result.column.v_ms[0], target_wind.1, 1.0e-12);
        assert_close(
            result.column.u_ms[1],
            0.5 * (source_wind.0 + target_wind.0),
            1.0e-12,
        );
        assert_close(
            result.column.v_ms[1],
            0.5 * (source_wind.1 + target_wind.1),
            1.0e-12,
        );
        assert_close(result.column.u_ms[1], 0.0, 1.0e-12);
        assert!(result.column.v_ms[1] < -9.0);
    }

    #[test]
    fn dry_pava_restores_stability_and_conserves_sensible_enthalpy() {
        let pressure_hpa = vec![1_000.0, 900.0, 800.0, 700.0, 600.0];
        let source_theta = [300.0, 295.0, 305.0, 302.0, 310.0];
        let temperature_c: Vec<_> = pressure_hpa
            .iter()
            .zip(source_theta)
            .map(|(&pressure, theta)| {
                temperature_c_from_potential_temperature_k(pressure, theta)
                    .expect("theta to temperature")
            })
            .collect();
        let source = column(
            pressure_hpa,
            vec![0.0, 1_000.0, 2_000.0, 3_000.0, 4_000.0],
            temperature_c,
            vec![-60.0; 5],
            vec![0.0; 5],
            vec![0.0; 5],
        );
        let mut recipe = CorrectionRecipe::default();
        recipe.convective_adjustment.enabled = true;
        recipe.convective_adjustment.protected_surface_depth_m = 0.0;

        let result = apply_correction_recipe(&source, &recipe);
        let theta: Vec<_> = result
            .column
            .pressure_hpa
            .iter()
            .zip(&result.column.temperature_c)
            .map(|(&pressure, &temperature)| {
                potential_temperature_k(pressure, temperature).expect("corrected theta")
            })
            .collect();

        assert!(result.convective_adjustment.attempted);
        assert!(
            result.convective_adjustment.applied,
            "adjustment aborted: {:?}",
            result.convective_adjustment.aborted_reason
        );
        assert!(result.convective_adjustment.adjusted_levels > 0);
        assert!(theta.windows(2).all(|pair| pair[1] + 1.0e-10 >= pair[0]));
        assert_close(
            result.convective_adjustment.sensible_enthalpy_after_j_kg,
            result.convective_adjustment.sensible_enthalpy_before_j_kg,
            1.0e-8,
        );
        assert!(result.convective_adjustment.relative_enthalpy_residual <= 1.0e-12);
    }

    #[test]
    fn ordinary_supersaturation_is_a_warning_not_an_error() {
        let source = column(
            vec![1_000.0, 900.0, 800.0],
            vec![0.0, 1_000.0, 2_000.0],
            vec![20.0, 15.0, 10.0],
            vec![10.0, 5.0, 0.0],
            vec![0.0; 3],
            vec![0.0; 3],
        );
        let mut level = CorrectionLevel::at_height(0.0);
        level.moisture = Some(MoistureEdit {
            target: MoistureTarget::DewpointC(25.0),
            blend: BlendSpec::local_cosine(0.0),
        });

        let result = apply_correction_recipe(
            &source,
            &CorrectionRecipe {
                levels: vec![level],
                ..Default::default()
            },
        );

        assert!(result.issues.iter().any(|issue| {
            issue.kind == QcIssueKind::Supersaturation
                && issue.severity == QcSeverity::Warning
                && issue.level_index == Some(0)
        }));
        assert!(result.issues.iter().all(|issue| {
            issue.kind != QcIssueKind::Supersaturation || issue.severity != QcSeverity::Error
        }));
        assert!(
            !result.has_errors(),
            "supersaturation became fatal: {:?}",
            result.issues
        );
    }
}
