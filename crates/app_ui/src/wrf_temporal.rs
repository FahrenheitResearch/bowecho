//! Pure temporal planning plus strict raw-state interpolation primitives for
//! WRF simulated radar.
//!
//! Timed acquisition and atmosphere sampling are separate choices. The legacy
//! renderer remains frozen at the volume start; [`AtmosphereTimeMode::LinearAdjacent`]
//! requires a compatible later WRF scene covering the whole scan. Interpolation
//! weights are bounded per ray with no extrapolation. [`interpolate_raw_gate`]
//! is the contract for a property-aware renderer: raw fields must be blended
//! before nonlinear closure/scattering. The compatibility renderer can use the
//! same plan for linear Z, wind, and additive scattering quantities, but must
//! label that interpolation space explicitly rather than claiming raw state.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::hash::Hash;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::wrf_scene_inventory::{WrfSceneGroup, WrfSceneLocator};

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

    if mode == AtmosphereTimeMode::FrozenAtVolumeStart {
        return Ok(Some(TemporalScenePlan {
            anchor: anchor.locator(),
            anchor_time,
            neighbor: None,
            neighbor_time: None,
            scan_duration_ms,
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

#[derive(Clone, Debug, PartialEq)]
pub enum RawInterpolationError {
    AlphaOutOfRange(f64),
    MissingLeftCoverage,
    MissingRightCoverage,
    FieldInventoryMismatch,
    NonFiniteValue { field: String },
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
        return left
            .cloned()
            .ok_or(RawInterpolationError::MissingLeftCoverage);
    }
    if alpha == 1.0 {
        return right
            .cloned()
            .ok_or(RawInterpolationError::MissingRightCoverage);
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
    use std::path::PathBuf;

    use chrono::TimeZone;

    use super::*;
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
