//! Pure validation primitives shared by the WRF radar operator and its UI.
//!
//! This module deliberately owns no file dialogs, workers, or viewer state.
//! It extracts an observed scan into an immutable replay plan, builds an
//! exact-geometry synthetic-minus-observed volume, carries pulse-volume
//! support fractions, and records compact dual-pol quantization loss.

use std::collections::BTreeSet;
use std::hash::{Hash, Hasher};

use chrono::{DateTime, Duration, TimeZone, Utc};
use radar_core::{
    ElevationCut, GateRange, MomentGrid, MomentStorage, MomentType, RadarSite, RadarVolume, Radial,
    RadialStatus, VcpInfo,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const MODEL_COVERAGE_MOMENT: &str = "MCOV";
pub const TERRAIN_UNBLOCKED_MOMENT: &str = "TUNB";
pub const METEOROLOGICAL_SIGNAL_MOMENT: &str = "MSIG";

/// How `Radial::time_offset_ms` is encoded in a source volume.
///
/// Archive-II stores collection milliseconds since UTC midnight. BowEcho's
/// synthetic, CfRadial, and DORADE paths store an offset from `volume_time`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RadialTimeEncoding {
    NexradMillisecondsSinceMidnight,
    RelativeToVolumeStart,
}

/// One observed radial, stripped of measured values but retaining every
/// acquisition coordinate needed by the forward operator.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReplayRayPlan {
    pub source_radial_index: usize,
    pub azimuth_deg: f32,
    pub elevation_deg: f32,
    pub source_time_offset_ms: i32,
    pub acquisition_time_utc: DateTime<Utc>,
    pub acquisition_offset_ms: i64,
    pub gate_range: GateRange,
    pub nyquist_velocity_mps: Option<f32>,
    pub radial_status: Option<RadialStatus>,
}

/// Exact availability and gate geometry of one measured moment within a cut.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReplayMomentPlan {
    pub moment: MomentType,
    pub gate_range: GateRange,
    pub radial_indices: Vec<usize>,
}

/// One observed physical cut. Equal-elevation split cuts remain distinct.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReplayCutPlan {
    pub source_cut_index: usize,
    pub elevation_deg: f32,
    pub elevation_number: Option<u8>,
    pub rays: Vec<ReplayRayPlan>,
    pub moments: Vec<ReplayMomentPlan>,
}

/// Immutable scan template used by the WRF replay operator.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ObservedScanTemplate {
    pub site: RadarSite,
    pub volume_time: DateTime<Utc>,
    pub vcp: Option<VcpInfo>,
    /// Source-declared volume PRT. This is replay metadata, not an inference
    /// from a VCP number or Appendix-C PRF code.
    #[serde(default)]
    pub source_prt_s: Option<f32>,
    /// Source-declared unambiguous range, retained independently of PRT.
    #[serde(default)]
    pub source_unambiguous_range_km: Option<f32>,
    pub time_encoding: RadialTimeEncoding,
    pub cuts: Vec<ReplayCutPlan>,
}

/// Public name used by the renderer: this is exact observed acquisition
/// geometry, never a reconstructed or idealized VCP.
pub type ExactScanTemplate = ObservedScanTemplate;

#[derive(Clone, Debug, Error, PartialEq)]
pub enum ReplayTemplateError {
    #[error("observed radar volume has no cuts")]
    EmptyVolume,
    #[error("observed radar site {site_id} has no latitude/longitude")]
    MissingSiteLocation { site_id: String },
    #[error("observed cut {cut} has no radials")]
    EmptyCut { cut: usize },
    #[error("observed cut {cut} radial {radial} has invalid gate geometry {geometry:?}")]
    InvalidRadialGeometry {
        cut: usize,
        radial: usize,
        geometry: GateRange,
    },
    #[error("observed cut {cut} moment {moment} has invalid gate geometry {geometry:?}")]
    InvalidMomentGeometry {
        cut: usize,
        moment: String,
        geometry: GateRange,
    },
    #[error("observed cut {cut} moment {moment} refers to missing radial {radial}")]
    MissingMomentRadial {
        cut: usize,
        moment: String,
        radial: usize,
    },
    #[error("observed cut {cut} moment {moment} repeats radial {radial}")]
    DuplicateMomentRadial {
        cut: usize,
        moment: String,
        radial: usize,
    },
    #[error("invalid NEXRAD collection milliseconds {offset_ms} on cut {cut} radial {radial}")]
    InvalidNexradTime {
        cut: usize,
        radial: usize,
        offset_ms: i32,
    },
    #[error("radial time overflow on cut {cut} radial {radial}")]
    RadialTimeOverflow { cut: usize, radial: usize },
}

impl ObservedScanTemplate {
    pub fn from_volume(volume: &RadarVolume) -> Result<Self, ReplayTemplateError> {
        Self::from_volume_with_encoding(volume, radial_time_encoding(volume))
    }

    pub fn from_volume_with_encoding(
        volume: &RadarVolume,
        time_encoding: RadialTimeEncoding,
    ) -> Result<Self, ReplayTemplateError> {
        if volume.cuts.is_empty() {
            return Err(ReplayTemplateError::EmptyVolume);
        }
        if volume.site.latitude_deg.is_none() || volume.site.longitude_deg.is_none() {
            return Err(ReplayTemplateError::MissingSiteLocation {
                site_id: volume.site.id.clone(),
            });
        }

        let mut cuts = Vec::with_capacity(volume.cuts.len());
        for (cut_index, cut) in volume.cuts.iter().enumerate() {
            if cut.radials.is_empty() {
                return Err(ReplayTemplateError::EmptyCut { cut: cut_index + 1 });
            }
            let mut rays = Vec::with_capacity(cut.radials.len());
            for (radial_index, radial) in cut.radials.iter().enumerate() {
                validate_gate_range(&radial.gate_range).map_err(|()| {
                    ReplayTemplateError::InvalidRadialGeometry {
                        cut: cut_index + 1,
                        radial: radial_index + 1,
                        geometry: radial.gate_range.clone(),
                    }
                })?;
                let acquisition_time_utc = radial_acquisition_time_utc_with_encoding(
                    volume.volume_time,
                    radial.time_offset_ms,
                    time_encoding,
                )
                .ok_or_else(|| match time_encoding {
                    RadialTimeEncoding::NexradMillisecondsSinceMidnight => {
                        ReplayTemplateError::InvalidNexradTime {
                            cut: cut_index + 1,
                            radial: radial_index + 1,
                            offset_ms: radial.time_offset_ms,
                        }
                    }
                    RadialTimeEncoding::RelativeToVolumeStart => {
                        ReplayTemplateError::RadialTimeOverflow {
                            cut: cut_index + 1,
                            radial: radial_index + 1,
                        }
                    }
                })?;
                rays.push(ReplayRayPlan {
                    source_radial_index: radial_index,
                    azimuth_deg: radial.azimuth_deg,
                    elevation_deg: radial.elevation_deg,
                    source_time_offset_ms: radial.time_offset_ms,
                    acquisition_time_utc,
                    acquisition_offset_ms: (acquisition_time_utc - volume.volume_time)
                        .num_milliseconds(),
                    gate_range: radial.gate_range.clone(),
                    nyquist_velocity_mps: radial.nyquist_velocity_mps,
                    radial_status: radial.radial_status,
                });
            }

            let mut moments = Vec::with_capacity(cut.moments.len());
            for (moment, grid) in &cut.moments {
                validate_gate_range(&grid.gate_range).map_err(|()| {
                    ReplayTemplateError::InvalidMomentGeometry {
                        cut: cut_index + 1,
                        moment: moment.short_name().to_owned(),
                        geometry: grid.gate_range.clone(),
                    }
                })?;
                let mut seen = BTreeSet::new();
                for &radial_index in &grid.radial_indices {
                    if radial_index >= cut.radials.len() {
                        return Err(ReplayTemplateError::MissingMomentRadial {
                            cut: cut_index + 1,
                            moment: moment.short_name().to_owned(),
                            radial: radial_index,
                        });
                    }
                    if !seen.insert(radial_index) {
                        return Err(ReplayTemplateError::DuplicateMomentRadial {
                            cut: cut_index + 1,
                            moment: moment.short_name().to_owned(),
                            radial: radial_index,
                        });
                    }
                }
                moments.push(ReplayMomentPlan {
                    moment: moment.clone(),
                    gate_range: grid.gate_range.clone(),
                    radial_indices: grid.radial_indices.clone(),
                });
            }
            cuts.push(ReplayCutPlan {
                source_cut_index: cut_index,
                elevation_deg: cut.elevation_deg,
                elevation_number: cut.elevation_number,
                rays,
                moments,
            });
        }

        Ok(Self {
            site: volume.site.clone(),
            volume_time: volume.volume_time,
            vcp: volume.vcp.clone(),
            source_prt_s: volume.metadata.prt_s,
            source_unambiguous_range_km: volume.metadata.unambiguous_range_km,
            time_encoding,
            cuts,
        })
    }

    /// Stable within the BowEcho build and independent of measured gate values.
    pub fn geometry_fingerprint(&self) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.site.id.hash(&mut hasher);
        self.site.latitude_deg.map(f32::to_bits).hash(&mut hasher);
        self.site.longitude_deg.map(f32::to_bits).hash(&mut hasher);
        self.site.elevation_m.map(f32::to_bits).hash(&mut hasher);
        self.volume_time.timestamp_millis().hash(&mut hasher);
        self.vcp.as_ref().map(|vcp| vcp.pattern).hash(&mut hasher);
        self.source_prt_s.map(f32::to_bits).hash(&mut hasher);
        self.source_unambiguous_range_km
            .map(f32::to_bits)
            .hash(&mut hasher);
        (self.time_encoding as u8).hash(&mut hasher);
        for cut in &self.cuts {
            cut.source_cut_index.hash(&mut hasher);
            cut.elevation_deg.to_bits().hash(&mut hasher);
            cut.elevation_number.hash(&mut hasher);
            for ray in &cut.rays {
                ray.source_radial_index.hash(&mut hasher);
                ray.azimuth_deg.to_bits().hash(&mut hasher);
                ray.elevation_deg.to_bits().hash(&mut hasher);
                ray.source_time_offset_ms.hash(&mut hasher);
                ray.acquisition_time_utc
                    .timestamp_millis()
                    .hash(&mut hasher);
                ray.acquisition_offset_ms.hash(&mut hasher);
                hash_gate_range(&ray.gate_range, &mut hasher);
                ray.nyquist_velocity_mps.map(f32::to_bits).hash(&mut hasher);
                format!("{:?}", ray.radial_status).hash(&mut hasher);
            }
            for moment in &cut.moments {
                moment.moment.hash(&mut hasher);
                hash_gate_range(&moment.gate_range, &mut hasher);
                moment.radial_indices.hash(&mut hasher);
            }
        }
        hasher.finish()
    }

    pub fn latest_acquisition_offset_ms(&self) -> i64 {
        self.cuts
            .iter()
            .flat_map(|cut| &cut.rays)
            .map(|ray| ray.acquisition_offset_ms)
            .max()
            .unwrap_or(0)
    }
}

fn hash_gate_range(range: &GateRange, hasher: &mut impl Hasher) {
    range.first_gate_m.hash(hasher);
    range.gate_spacing_m.hash(hasher);
    range.gate_count.hash(hasher);
}

fn validate_gate_range(range: &GateRange) -> Result<(), ()> {
    if range.gate_spacing_m > 0 && range.gate_count > 0 {
        Ok(())
    } else {
        Err(())
    }
}

pub fn radial_time_encoding(volume: &RadarVolume) -> RadialTimeEncoding {
    let archive = volume
        .metadata
        .archive_version
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_ascii_uppercase();
    if archive.starts_with("AR2") {
        RadialTimeEncoding::NexradMillisecondsSinceMidnight
    } else {
        RadialTimeEncoding::RelativeToVolumeStart
    }
}

pub fn radial_acquisition_time_utc(volume: &RadarVolume, radial: &Radial) -> Option<DateTime<Utc>> {
    radial_acquisition_time_utc_with_encoding(
        volume.volume_time,
        radial.time_offset_ms,
        radial_time_encoding(volume),
    )
}

pub fn radial_acquisition_time_utc_with_encoding(
    volume_time: DateTime<Utc>,
    time_offset_ms: i32,
    encoding: RadialTimeEncoding,
) -> Option<DateTime<Utc>> {
    match encoding {
        RadialTimeEncoding::RelativeToVolumeStart => {
            volume_time.checked_add_signed(Duration::milliseconds(i64::from(time_offset_ms)))
        }
        RadialTimeEncoding::NexradMillisecondsSinceMidnight => {
            const DAY_MS: i32 = 86_400_000;
            if !(0..DAY_MS).contains(&time_offset_ms) {
                return None;
            }
            let midnight = volume_time
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .map(|naive| Utc.from_utc_datetime(&naive))?;
            let same_day =
                midnight.checked_add_signed(Duration::milliseconds(i64::from(time_offset_ms)))?;
            [
                same_day.checked_sub_signed(Duration::days(1)),
                Some(same_day),
                same_day.checked_add_signed(Duration::days(1)),
            ]
            .into_iter()
            .flatten()
            .min_by_key(|candidate| (*candidate - volume_time).num_milliseconds().unsigned_abs())
        }
    }
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum DifferenceVolumeError {
    #[error("observed/synthetic site differs: {observed} vs {synthetic}")]
    SiteMismatch { observed: String, synthetic: String },
    #[error("observed/synthetic radar latitude, longitude, or altitude differs")]
    SiteGeometryMismatch,
    #[error("observed/synthetic cut count differs: {observed} vs {synthetic}")]
    CutCountMismatch { observed: usize, synthetic: usize },
    #[error("cut {cut} elevation identity differs")]
    CutIdentityMismatch { cut: usize },
    #[error("cut {cut} radial count differs: {observed} vs {synthetic}")]
    RadialCountMismatch {
        cut: usize,
        observed: usize,
        synthetic: usize,
    },
    #[error("cut {cut} radial {radial} geometry/timing differs: {field}")]
    RadialMismatch {
        cut: usize,
        radial: usize,
        field: &'static str,
    },
    #[error("cut {cut} synthetic volume is missing observed moment {moment}")]
    MissingSyntheticMoment { cut: usize, moment: String },
    #[error("cut {cut} moment {moment} gate geometry differs")]
    MomentGeometryMismatch { cut: usize, moment: String },
    #[error("cut {cut} moment {moment} radial availability differs")]
    MomentRadialsMismatch { cut: usize, moment: String },
    #[error("observed and synthetic volumes have no comparable moments")]
    NoComparableMoments,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnavailableObservedMoment {
    pub cut: usize,
    pub moment: String,
    pub reason: String,
}

pub struct DifferenceVolumeOverlap {
    pub volume: RadarVolume,
    pub unavailable_observed_moments: Vec<UnavailableObservedMoment>,
}

/// Build an exact-geometry difference over the canonical moments the forward
/// operator could actually emit. Missing canonical observations are reported,
/// never silently converted to all-NaN difference grids.
pub fn build_difference_volume_overlap(
    observed: &RadarVolume,
    synthetic: &RadarVolume,
) -> Result<DifferenceVolumeOverlap, DifferenceVolumeError> {
    let mut comparable = observed.clone();
    let mut unavailable_observed_moments = Vec::new();
    for (cut_index, (observed_cut, synthetic_cut)) in
        observed.cuts.iter().zip(&synthetic.cuts).enumerate()
    {
        for moment in observed_cut.moments.keys() {
            if is_canonical_radar_moment(moment) && !synthetic_cut.moments.contains_key(moment) {
                comparable.cuts[cut_index].moments.remove(moment);
                unavailable_observed_moments.push(UnavailableObservedMoment {
                    cut: cut_index + 1,
                    moment: moment.short_name().to_owned(),
                    reason: "forward operator did not provide this observed canonical moment"
                        .to_owned(),
                });
            }
        }
    }
    let volume = build_difference_volume(&comparable, synthetic)?;
    Ok(DifferenceVolumeOverlap {
        volume,
        unavailable_observed_moments,
    })
}

/// Build `synthetic - observed` on the observed scan's exact polar geometry.
/// Synthetic-only diagnostics (for example MCOV/TUNB/MSIG) are ignored. Every
/// canonical observed radar moment must have an exact synthetic counterpart;
/// app-derived `Unknown` moments are compared only when both volumes carry it.
pub fn build_difference_volume(
    observed: &RadarVolume,
    synthetic: &RadarVolume,
) -> Result<RadarVolume, DifferenceVolumeError> {
    validate_volume_geometry(observed, synthetic)?;

    let mut difference = RadarVolume::new(observed.site.clone(), observed.volume_time);
    difference.site.name = Some(format!("{} synthetic minus observed", observed.site.id));
    difference.vcp = observed.vcp.clone();
    difference.metadata = synthetic.metadata.clone();
    difference.metadata.source_path = None;
    difference.metadata.archive_version = Some("bowecho-sim-minus-observed-v1".to_owned());
    difference.metadata.forward_operator =
        Some("BowEcho exact-geometry synthetic-minus-observed difference v1".to_owned());
    let comparison = format!(
        "comparison=synthetic_minus_observed; observed_site={}; observed_time={}; synthetic_time={}",
        observed.site.id,
        observed.volume_time.to_rfc3339(),
        synthetic.volume_time.to_rfc3339(),
    );
    match difference.metadata.forward_operator_config.as_mut() {
        Some(config) => {
            config.push_str("; ");
            config.push_str(&comparison);
        }
        None => difference.metadata.forward_operator_config = Some(comparison),
    }

    let mut compared_moments = 0usize;
    for (cut_index, (observed_cut, synthetic_cut)) in
        observed.cuts.iter().zip(&synthetic.cuts).enumerate()
    {
        let mut cut = ElevationCut::new(observed_cut.elevation_deg, observed_cut.elevation_number);
        cut.radials = observed_cut.radials.clone();
        for (moment, observed_grid) in &observed_cut.moments {
            let Some(synthetic_grid) = synthetic_cut.moments.get(moment) else {
                if is_canonical_radar_moment(moment) {
                    return Err(DifferenceVolumeError::MissingSyntheticMoment {
                        cut: cut_index + 1,
                        moment: moment.short_name().to_owned(),
                    });
                }
                // BowEcho attaches several derived Unknown moments to decoded
                // observations. They are not scan measurements and are not
                // required of the forward operator; compare an Unknown only
                // when the synthetic volume explicitly carries the same id.
                continue;
            };
            if observed_grid.gate_range != synthetic_grid.gate_range {
                return Err(DifferenceVolumeError::MomentGeometryMismatch {
                    cut: cut_index + 1,
                    moment: moment.short_name().to_owned(),
                });
            }
            if observed_grid.radial_indices != synthetic_grid.radial_indices {
                return Err(DifferenceVolumeError::MomentRadialsMismatch {
                    cut: cut_index + 1,
                    moment: moment.short_name().to_owned(),
                });
            }
            let gate_count = observed_grid.gate_range.gate_count;
            let mut values = Vec::with_capacity(observed_grid.radial_count() * gate_count);
            for row in 0..observed_grid.radial_count() {
                for gate in 0..gate_count {
                    let value = observed_grid
                        .scaled_value(row, gate)
                        .zip(synthetic_grid.scaled_value(row, gate))
                        .map(|(observed, synthetic)| {
                            if *moment == MomentType::DifferentialPhase {
                                circular_phase_difference_deg(synthetic, observed)
                            } else {
                                synthetic - observed
                            }
                        })
                        .filter(|value| value.is_finite())
                        .unwrap_or(f32::NAN);
                    values.push(value);
                }
            }
            let difference_moment = difference_moment_type(moment);
            cut.moments.insert(
                difference_moment.clone(),
                MomentGrid {
                    moment: difference_moment,
                    gate_range: observed_grid.gate_range.clone(),
                    scale: 1.0,
                    offset: 0.0,
                    nodata: None,
                    range_folded: None,
                    radial_indices: observed_grid.radial_indices.clone(),
                    storage: MomentStorage::F32(values),
                },
            );
            compared_moments += 1;
        }
        difference.cuts.push(cut);
    }
    difference.metadata.decoded_radial_count =
        difference.cuts.iter().map(|cut| cut.radials.len()).sum();
    if compared_moments == 0 {
        return Err(DifferenceVolumeError::NoComparableMoments);
    }
    Ok(difference)
}

fn is_canonical_radar_moment(moment: &MomentType) -> bool {
    !matches!(moment, MomentType::Unknown(_))
}

fn validate_volume_geometry(
    observed: &RadarVolume,
    synthetic: &RadarVolume,
) -> Result<(), DifferenceVolumeError> {
    if observed.site.id != synthetic.site.id {
        return Err(DifferenceVolumeError::SiteMismatch {
            observed: observed.site.id.clone(),
            synthetic: synthetic.site.id.clone(),
        });
    }
    if observed.site.latitude_deg.map(f32::to_bits) != synthetic.site.latitude_deg.map(f32::to_bits)
        || observed.site.longitude_deg.map(f32::to_bits)
            != synthetic.site.longitude_deg.map(f32::to_bits)
        || observed.site.elevation_m.map(f32::to_bits)
            != synthetic.site.elevation_m.map(f32::to_bits)
    {
        return Err(DifferenceVolumeError::SiteGeometryMismatch);
    }
    if observed.cuts.len() != synthetic.cuts.len() {
        return Err(DifferenceVolumeError::CutCountMismatch {
            observed: observed.cuts.len(),
            synthetic: synthetic.cuts.len(),
        });
    }
    for (cut_index, (observed_cut, synthetic_cut)) in
        observed.cuts.iter().zip(&synthetic.cuts).enumerate()
    {
        if observed_cut.elevation_deg.to_bits() != synthetic_cut.elevation_deg.to_bits()
            || observed_cut.elevation_number != synthetic_cut.elevation_number
        {
            return Err(DifferenceVolumeError::CutIdentityMismatch { cut: cut_index + 1 });
        }
        if observed_cut.radials.len() != synthetic_cut.radials.len() {
            return Err(DifferenceVolumeError::RadialCountMismatch {
                cut: cut_index + 1,
                observed: observed_cut.radials.len(),
                synthetic: synthetic_cut.radials.len(),
            });
        }
        for (radial_index, (observed_ray, synthetic_ray)) in observed_cut
            .radials
            .iter()
            .zip(&synthetic_cut.radials)
            .enumerate()
        {
            let mismatch = if observed_ray.azimuth_deg.to_bits()
                != synthetic_ray.azimuth_deg.to_bits()
                || observed_ray.elevation_deg.to_bits() != synthetic_ray.elevation_deg.to_bits()
            {
                Some("angles")
            } else if observed_ray.gate_range != synthetic_ray.gate_range {
                Some("gate range")
            } else if observed_ray.nyquist_velocity_mps.map(f32::to_bits)
                != synthetic_ray.nyquist_velocity_mps.map(f32::to_bits)
            {
                Some("Nyquist velocity")
            } else if observed_ray.radial_status != synthetic_ray.radial_status {
                Some("radial status")
            } else if radial_acquisition_time_utc(observed, observed_ray)
                != radial_acquisition_time_utc(synthetic, synthetic_ray)
            {
                Some("acquisition time")
            } else {
                None
            };
            if let Some(field) = mismatch {
                return Err(DifferenceVolumeError::RadialMismatch {
                    cut: cut_index + 1,
                    radial: radial_index + 1,
                    field,
                });
            }
        }
    }
    Ok(())
}

pub fn difference_moment_type(moment: &MomentType) -> MomentType {
    let suffix = match moment {
        MomentType::Reflectivity => "REF".to_owned(),
        MomentType::Velocity => "VEL".to_owned(),
        MomentType::SpectrumWidth => "SW".to_owned(),
        MomentType::DifferentialReflectivity => "ZDR".to_owned(),
        MomentType::CorrelationCoefficient => "RHO".to_owned(),
        MomentType::DifferentialPhase => "PHI".to_owned(),
        MomentType::SpecificDifferentialPhase => "KDP".to_owned(),
        MomentType::Unknown(name) => name
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() {
                    character.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect(),
    };
    MomentType::Unknown(format!("DIF_{suffix}"))
}

fn circular_phase_difference_deg(synthetic: f32, observed: f32) -> f32 {
    (synthetic - observed + 180.0).rem_euclid(360.0) - 180.0
}

/// Quadrature support retained at the three meaningful operator boundaries.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GateQuality {
    pub total_weight: f64,
    pub model_covered_weight: f64,
    pub terrain_unblocked_weight: f64,
    pub meteorological_signal_weight: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GateQualityFractions {
    pub model_coverage_fraction: f32,
    pub terrain_unblocked_fraction: f32,
    pub meteorological_signal_fraction: f32,
}

impl GateQuality {
    pub fn fractions(self) -> GateQualityFractions {
        if !self.total_weight.is_finite() || self.total_weight <= 0.0 {
            return GateQualityFractions::default();
        }
        let fraction = |weight: f64| {
            if weight.is_finite() {
                (weight / self.total_weight).clamp(0.0, 1.0) as f32
            } else {
                0.0
            }
        };
        let model = fraction(self.model_covered_weight);
        let unblocked = fraction(self.terrain_unblocked_weight).min(model);
        let signal = fraction(self.meteorological_signal_weight).min(unblocked);
        GateQualityFractions {
            model_coverage_fraction: model,
            terrain_unblocked_fraction: unblocked,
            meteorological_signal_fraction: signal,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QualityMoment {
    ModelCoverage,
    TerrainUnblocked,
    MeteorologicalSignal,
}

impl QualityMoment {
    pub const ALL: [Self; 3] = [
        Self::ModelCoverage,
        Self::TerrainUnblocked,
        Self::MeteorologicalSignal,
    ];

    pub const fn id(self) -> &'static str {
        match self {
            Self::ModelCoverage => MODEL_COVERAGE_MOMENT,
            Self::TerrainUnblocked => TERRAIN_UNBLOCKED_MOMENT,
            Self::MeteorologicalSignal => METEOROLOGICAL_SIGNAL_MOMENT,
        }
    }

    pub fn moment_type(self) -> MomentType {
        MomentType::Unknown(self.id().to_owned())
    }
}

pub fn encode_quality_fraction(value: f32) -> u8 {
    if value.is_finite() {
        (value.clamp(0.0, 1.0) * u8::MAX as f32).round() as u8
    } else {
        0
    }
}

pub fn compact_quality_grid(
    quality: QualityMoment,
    gate_range: GateRange,
    radial_indices: Vec<usize>,
    encoded: Vec<u8>,
) -> Result<MomentGrid, String> {
    let expected = radial_indices
        .len()
        .checked_mul(gate_range.gate_count)
        .ok_or_else(|| format!("{} quality grid dimensions overflow", quality.id()))?;
    if encoded.len() != expected {
        return Err(format!(
            "{} quality grid has {} values, expected {expected}",
            quality.id(),
            encoded.len()
        ));
    }
    let moment = quality.moment_type();
    Ok(MomentGrid {
        moment,
        gate_range,
        scale: u8::MAX as f32,
        offset: 0.0,
        nodata: None,
        range_folded: None,
        radial_indices,
        storage: MomentStorage::U8(encoded),
    })
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct QuantizationMetric {
    pub samples: u64,
    pub non_finite_inputs: u64,
    pub quantized_to_zero: u64,
    pub clamped_low: u64,
    pub clamped_high: u64,
    pub max_abs_reconstruction_error: f32,
}

impl QuantizationMetric {
    pub fn observe(&mut self, input: f32, reconstructed: f32, minimum: f32, maximum: f32) {
        if !input.is_finite() {
            self.non_finite_inputs = self.non_finite_inputs.saturating_add(1);
            return;
        }
        self.samples = self.samples.saturating_add(1);
        if input < minimum {
            self.clamped_low = self.clamped_low.saturating_add(1);
        }
        if input > maximum {
            self.clamped_high = self.clamped_high.saturating_add(1);
        }
        if input != 0.0 && reconstructed == 0.0 {
            self.quantized_to_zero = self.quantized_to_zero.saturating_add(1);
        }
        if reconstructed.is_finite() {
            self.max_abs_reconstruction_error = self
                .max_abs_reconstruction_error
                .max((reconstructed - input).abs());
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CompactPolarPrecisionAudit {
    pub zdr_db: QuantizationMetric,
    pub rho_hv: QuantizationMetric,
    pub covariance_phase_deg: QuantizationMetric,
    pub kdp_deg_km: QuantizationMetric,
    pub ah_db_km: QuantizationMetric,
    pub adp_db_km: QuantizationMetric,
    pub fall_speed_mps: QuantizationMetric,
    pub fall_speed_std_mps: QuantizationMetric,
    pub max_zv_relative_error: f32,
    pub max_covariance_magnitude_relative_error: f32,
    pub max_av_abs_error_db_km: f32,
    pub max_fall_variance_abs_error_m2s2: f32,
}

impl CompactPolarPrecisionAudit {
    pub fn total_clamps(&self) -> u64 {
        self.metrics()
            .into_iter()
            .map(|metric| metric.clamped_low.saturating_add(metric.clamped_high))
            .sum()
    }

    pub fn total_quantized_to_zero(&self) -> u64 {
        self.metrics()
            .into_iter()
            .map(|metric| metric.quantized_to_zero)
            .sum()
    }

    pub fn provenance_fragment(&self) -> String {
        let fields = [
            ("zdr", &self.zdr_db),
            ("rho", &self.rho_hv),
            ("phase", &self.covariance_phase_deg),
            ("kdp", &self.kdp_deg_km),
            ("ah", &self.ah_db_km),
            ("adp", &self.adp_db_km),
            ("fall", &self.fall_speed_mps),
            ("fall_std", &self.fall_speed_std_mps),
        ];
        fields
            .into_iter()
            .map(|(name, metric)| {
                format!(
                    "{name}:n{},zero{},lo{},hi{},err{:.6}",
                    metric.samples,
                    metric.quantized_to_zero,
                    metric.clamped_low,
                    metric.clamped_high,
                    metric.max_abs_reconstruction_error,
                )
            })
            .collect::<Vec<_>>()
            .join("|")
    }

    fn metrics(&self) -> [&QuantizationMetric; 8] {
        [
            &self.zdr_db,
            &self.rho_hv,
            &self.covariance_phase_deg,
            &self.kdp_deg_km,
            &self.ah_db_km,
            &self.adp_db_km,
            &self.fall_speed_mps,
            &self.fall_speed_std_mps,
        ]
    }
}

pub fn relative_error(reference: f32, reconstructed: f32) -> f32 {
    if !reference.is_finite() || !reconstructed.is_finite() {
        return 0.0;
    }
    let denominator = reference.abs().max(f32::MIN_POSITIVE);
    ((reconstructed - reference) / denominator).abs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f32_grid(
        moment: MomentType,
        gate_range: GateRange,
        radial_indices: Vec<usize>,
        values: Vec<f32>,
    ) -> MomentGrid {
        MomentGrid {
            moment,
            gate_range,
            scale: 1.0,
            offset: 0.0,
            nodata: None,
            range_folded: None,
            radial_indices,
            storage: MomentStorage::F32(values),
        }
    }

    fn observed_fixture() -> RadarVolume {
        let time = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let mut site = RadarSite::new("KTLX");
        site.latitude_deg = Some(35.333);
        site.longitude_deg = Some(-97.277);
        let mut volume = RadarVolume::new(site, time);
        volume.metadata.archive_version = Some("AR2V0006".to_owned());
        volume.metadata.prt_s = Some(0.001);
        volume.metadata.unambiguous_range_km = Some(149.9);
        for cut_number in 0..2 {
            let mut cut = ElevationCut::new(0.5, Some(cut_number + 1));
            for (radial_index, azimuth_deg) in [12.0, 47.5, 301.0].into_iter().enumerate() {
                cut.radials.push(Radial {
                    azimuth_deg,
                    elevation_deg: 0.48 + radial_index as f32 * 0.01,
                    time_offset_ms: 80_000_000 + (cut_number as i32 * 10_000) + radial_index as i32,
                    gate_range: GateRange {
                        first_gate_m: 500,
                        gate_spacing_m: 250,
                        gate_count: 3,
                    },
                    nyquist_velocity_mps: Some(24.5 + radial_index as f32),
                    radial_status: Some(if radial_index == 0 {
                        RadialStatus::StartElevation
                    } else {
                        RadialStatus::Intermediate
                    }),
                });
            }
            cut.moments.insert(
                MomentType::Reflectivity,
                f32_grid(
                    MomentType::Reflectivity,
                    GateRange {
                        first_gate_m: 500,
                        gate_spacing_m: 1_000,
                        gate_count: 2,
                    },
                    vec![0, 2],
                    vec![10.0, 20.0, 30.0, 40.0],
                ),
            );
            cut.moments.insert(
                MomentType::Velocity,
                f32_grid(
                    MomentType::Velocity,
                    GateRange {
                        first_gate_m: 250,
                        gate_spacing_m: 250,
                        gate_count: 3,
                    },
                    vec![1, 2],
                    vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0],
                ),
            );
            volume.cuts.push(cut);
        }
        volume
    }

    #[test]
    fn observed_template_preserves_repeated_cuts_missing_sectors_and_moment_geometry() {
        let volume = observed_fixture();
        let template = ObservedScanTemplate::from_volume(&volume).unwrap();
        assert_eq!(
            template.time_encoding,
            RadialTimeEncoding::NexradMillisecondsSinceMidnight
        );
        assert_eq!(template.cuts.len(), 2);
        assert_eq!(template.source_prt_s, Some(0.001));
        assert_eq!(template.source_unambiguous_range_km, Some(149.9));
        assert_eq!(
            template.cuts[0].elevation_deg,
            template.cuts[1].elevation_deg
        );
        assert_eq!(template.cuts[0].rays.len(), 3);
        assert_eq!(template.cuts[0].rays[2].azimuth_deg, 301.0);
        assert_eq!(template.cuts[0].rays[1].nyquist_velocity_mps, Some(25.5));
        assert_eq!(template.cuts[0].moments[0].radial_indices, vec![0, 2]);
        assert_ne!(
            template.cuts[0].moments[0].gate_range,
            template.cuts[0].moments[1].gate_range
        );
        let fingerprint = template.geometry_fingerprint();
        assert_eq!(fingerprint, template.clone().geometry_fingerprint());
        let mut changed = template.clone();
        changed.cuts[0].rays[0].azimuth_deg += 0.01;
        assert_ne!(fingerprint, changed.geometry_fingerprint());
    }

    #[test]
    fn nexrad_midnight_offsets_normalize_to_the_following_day() {
        let start = Utc.with_ymd_and_hms(2026, 7, 12, 23, 59, 50).unwrap();
        let before = radial_acquisition_time_utc_with_encoding(
            start,
            86_390_000,
            RadialTimeEncoding::NexradMillisecondsSinceMidnight,
        )
        .unwrap();
        let after = radial_acquisition_time_utc_with_encoding(
            start,
            1_000,
            RadialTimeEncoding::NexradMillisecondsSinceMidnight,
        )
        .unwrap();
        assert_eq!((before - start).num_milliseconds(), 0);
        assert_eq!((after - start).num_milliseconds(), 11_000);
    }

    #[test]
    fn difference_builder_subtracts_exact_geometry_and_wraps_phase() {
        let mut observed = observed_fixture();
        observed.metadata.archive_version = Some("test-relative".to_owned());
        for cut in &mut observed.cuts {
            for radial in &mut cut.radials {
                radial.time_offset_ms -= 80_000_000;
            }
        }
        let mut synthetic = observed.clone();
        synthetic.metadata.archive_version = Some("simulated-wrf".to_owned());
        if let MomentStorage::F32(values) = &mut synthetic.cuts[0]
            .moments
            .get_mut(&MomentType::Reflectivity)
            .unwrap()
            .storage
        {
            values[0] += 5.0;
        }
        let phase_range = GateRange {
            first_gate_m: 250,
            gate_spacing_m: 250,
            gate_count: 1,
        };
        observed.cuts[0].moments.insert(
            MomentType::DifferentialPhase,
            f32_grid(
                MomentType::DifferentialPhase,
                phase_range.clone(),
                vec![0],
                vec![179.0],
            ),
        );
        synthetic.cuts[0].moments.insert(
            MomentType::DifferentialPhase,
            f32_grid(
                MomentType::DifferentialPhase,
                phase_range,
                vec![0],
                vec![-179.0],
            ),
        );

        let difference = build_difference_volume(&observed, &synthetic).unwrap();
        let ref_grid = &difference.cuts[0].moments[&MomentType::Unknown("DIF_REF".to_owned())];
        assert_eq!(ref_grid.scaled_value(0, 0), Some(5.0));
        let phi_grid = &difference.cuts[0].moments[&MomentType::Unknown("DIF_PHI".to_owned())];
        assert_eq!(phi_grid.scaled_value(0, 0), Some(2.0));
    }

    #[test]
    fn difference_builder_reports_gate_geometry_mismatch() {
        let observed = observed_fixture();
        let mut synthetic = observed.clone();
        synthetic.cuts[0]
            .moments
            .get_mut(&MomentType::Velocity)
            .unwrap()
            .gate_range
            .gate_spacing_m = 500;
        assert!(matches!(
            build_difference_volume(&observed, &synthetic),
            Err(DifferenceVolumeError::MomentGeometryMismatch { .. })
        ));
    }

    #[test]
    fn overlap_difference_reports_unavailable_canonical_moment() {
        let observed = observed_fixture();
        let mut synthetic = observed.clone();
        for cut in &mut synthetic.cuts {
            cut.moments.remove(&MomentType::Velocity);
        }
        let overlap = build_difference_volume_overlap(&observed, &synthetic).unwrap();
        assert_eq!(overlap.unavailable_observed_moments.len(), 2);
        assert!(
            overlap
                .unavailable_observed_moments
                .iter()
                .all(|entry| entry.moment == "VEL")
        );
        assert!(overlap.volume.cuts.iter().all(|cut| {
            !cut.moments
                .contains_key(&MomentType::Unknown("DIF_VEL".to_owned()))
        }));
    }

    #[test]
    fn gate_quality_is_nested_and_round_trips_through_compact_grid() {
        let fractions = GateQuality {
            total_weight: 12.0,
            model_covered_weight: 9.0,
            terrain_unblocked_weight: 6.0,
            meteorological_signal_weight: 3.0,
        }
        .fractions();
        assert_eq!(fractions.model_coverage_fraction, 0.75);
        assert_eq!(fractions.terrain_unblocked_fraction, 0.5);
        assert_eq!(fractions.meteorological_signal_fraction, 0.25);
        let grid = compact_quality_grid(
            QualityMoment::ModelCoverage,
            GateRange {
                first_gate_m: 0,
                gate_spacing_m: 250,
                gate_count: 1,
            },
            vec![0],
            vec![encode_quality_fraction(fractions.model_coverage_fraction)],
        )
        .unwrap();
        assert!((grid.scaled_value(0, 0).unwrap() - 0.75).abs() <= 1.0 / 255.0);
    }

    #[test]
    fn quantization_metric_counts_zero_clamps_and_error() {
        let mut metric = QuantizationMetric::default();
        metric.observe(0.0004, 0.0, 0.0, 0.255);
        metric.observe(0.5, 0.255, 0.0, 0.255);
        metric.observe(-0.1, 0.0, 0.0, 0.255);
        assert_eq!(metric.samples, 3);
        assert_eq!(metric.quantized_to_zero, 2);
        assert_eq!(metric.clamped_low, 1);
        assert_eq!(metric.clamped_high, 1);
        assert!((metric.max_abs_reconstruction_error - 0.245).abs() < 1.0e-6);
    }
}
