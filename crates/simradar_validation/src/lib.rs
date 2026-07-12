//! Exact-geometry, multi-case validation harness for BowEcho simulated radar.
//!
//! The harness deliberately does not regrid or silently pair nearest gates.
//! Reference samples name the observed/replayed cut, source radial, gate and
//! physical ray geometry. A mismatch is counted and excluded, making a score
//! impossible to improve through accidental spatial or temporal misalignment.

use std::collections::BTreeMap;
use std::path::PathBuf;

use radar_core::{MomentGrid, MomentType, RadarVolume};
use serde::{Deserialize, Serialize};

pub const VALIDATION_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IndependentOperatorProvenance {
    pub name: String,
    pub version: String,
    pub configuration_sha256: String,
    /// Must be true for the default scientific-validation path. Same-generator
    /// table checks remain useful engineering tests but are not independent.
    pub scientifically_independent: bool,
    #[serde(default)]
    pub source_url: Option<String>,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReferenceSample {
    pub cut_index: usize,
    pub radial_index: usize,
    pub gate_index: usize,
    pub moment: String,
    pub value: f32,
    pub azimuth_deg: f32,
    pub elevation_deg: f32,
    pub range_m: f32,
    pub acquisition_offset_ms: i32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IndependentReferenceSet {
    pub schema_version: u32,
    pub case_id: String,
    pub operator: IndependentOperatorProvenance,
    #[serde(default)]
    pub source_identity: Option<String>,
    pub samples: Vec<ReferenceSample>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ValidationManifest {
    pub schema_version: u32,
    pub cases: Vec<ValidationCaseManifest>,
    #[serde(default)]
    pub tolerances: GeometryTolerances,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ValidationCaseManifest {
    pub id: String,
    pub bowecho_volume: PathBuf,
    pub independent_reference: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct GeometryTolerances {
    pub azimuth_deg: f32,
    pub elevation_deg: f32,
    pub range_m: f32,
    pub acquisition_time_ms: i32,
}

impl Default for GeometryTolerances {
    fn default() -> Self {
        Self {
            azimuth_deg: 0.01,
            elevation_deg: 0.01,
            range_m: 0.5,
            acquisition_time_ms: 1,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ValidationError {
    UnsupportedSchema { found: u32, expected: u32 },
    EmptyCaseId,
    CaseIdMismatch { manifest: String, reference: String },
    InvalidProvenance { field: &'static str },
    NonIndependentReference { operator: String },
    InvalidTolerance { field: &'static str },
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedSchema { found, expected } => {
                write!(
                    formatter,
                    "validation schema {found} is unsupported; expected {expected}"
                )
            }
            Self::EmptyCaseId => formatter.write_str("validation case id is empty"),
            Self::CaseIdMismatch {
                manifest,
                reference,
            } => write!(
                formatter,
                "validation case id '{manifest}' does not match reference case id '{reference}'"
            ),
            Self::InvalidProvenance { field } => {
                write!(
                    formatter,
                    "reference operator provenance field '{field}' is invalid"
                )
            }
            Self::NonIndependentReference { operator } => write!(
                formatter,
                "reference operator '{operator}' is not marked scientifically independent"
            ),
            Self::InvalidTolerance { field } => {
                write!(
                    formatter,
                    "validation geometry tolerance '{field}' is invalid"
                )
            }
        }
    }
}

impl std::error::Error for ValidationError {}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MomentScore {
    pub paired_count: usize,
    pub bias: f64,
    pub mean_absolute_error: f64,
    pub root_mean_square_error: f64,
    pub median_absolute_error: f64,
    pub p95_absolute_error: f64,
    pub pearson_correlation: Option<f64>,
    pub candidate_mean: f64,
    pub reference_mean: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GeometryMismatchExample {
    pub moment: String,
    pub cut_index: usize,
    pub radial_index: usize,
    pub gate_index: usize,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CaseScorecard {
    pub id: String,
    pub site_id: String,
    pub volume_time_utc: String,
    pub candidate_forward_operator: Option<String>,
    pub candidate_forward_operator_config: Option<String>,
    pub reference_operator: IndependentOperatorProvenance,
    pub moments: BTreeMap<String, MomentScore>,
    pub requested_samples: usize,
    pub paired_samples: usize,
    pub missing_candidate_samples: usize,
    pub nonfinite_reference_samples: usize,
    pub geometry_mismatch_samples: usize,
    pub geometry_mismatch_examples: Vec<GeometryMismatchExample>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MultiCaseScorecard {
    pub schema_version: u32,
    pub cases: Vec<CaseScorecard>,
    pub pooled_moments: BTreeMap<String, MomentScore>,
    pub total_requested_samples: usize,
    pub total_paired_samples: usize,
    pub total_missing_candidate_samples: usize,
    pub total_geometry_mismatch_samples: usize,
}

#[derive(Clone, Copy, Debug)]
struct Pair {
    candidate: f64,
    reference: f64,
    delta: f64,
}

#[derive(Debug)]
struct CaseComputation {
    scorecard: CaseScorecard,
    pairs: BTreeMap<String, Vec<Pair>>,
}

#[derive(Clone, Copy)]
pub struct ValidationCaseData<'a> {
    pub id: &'a str,
    pub candidate: &'a RadarVolume,
    pub reference: &'a IndependentReferenceSet,
}

pub fn validate_cases(
    cases: &[ValidationCaseData<'_>],
    tolerances: GeometryTolerances,
    require_independent: bool,
) -> Result<MultiCaseScorecard, ValidationError> {
    validate_tolerances(tolerances)?;
    let mut computations = Vec::with_capacity(cases.len());
    for case in cases {
        computations.push(compare_case(*case, tolerances, require_independent)?);
    }
    let mut pooled: BTreeMap<String, Vec<Pair>> = BTreeMap::new();
    for computation in &computations {
        for (moment, pairs) in &computation.pairs {
            pooled
                .entry(moment.clone())
                .or_default()
                .extend_from_slice(pairs);
        }
    }
    let pooled_moments = pooled
        .iter()
        .map(|(moment, pairs)| (moment.clone(), score_pairs(pairs)))
        .collect();
    let scorecards = computations
        .into_iter()
        .map(|computation| computation.scorecard)
        .collect::<Vec<_>>();
    Ok(MultiCaseScorecard {
        schema_version: VALIDATION_SCHEMA_VERSION,
        total_requested_samples: scorecards.iter().map(|case| case.requested_samples).sum(),
        total_paired_samples: scorecards.iter().map(|case| case.paired_samples).sum(),
        total_missing_candidate_samples: scorecards
            .iter()
            .map(|case| case.missing_candidate_samples)
            .sum(),
        total_geometry_mismatch_samples: scorecards
            .iter()
            .map(|case| case.geometry_mismatch_samples)
            .sum(),
        cases: scorecards,
        pooled_moments,
    })
}

fn compare_case(
    case: ValidationCaseData<'_>,
    tolerances: GeometryTolerances,
    require_independent: bool,
) -> Result<CaseComputation, ValidationError> {
    if case.id.trim().is_empty() {
        return Err(ValidationError::EmptyCaseId);
    }
    if case.reference.schema_version != VALIDATION_SCHEMA_VERSION {
        return Err(ValidationError::UnsupportedSchema {
            found: case.reference.schema_version,
            expected: VALIDATION_SCHEMA_VERSION,
        });
    }
    if case.reference.case_id != case.id {
        return Err(ValidationError::CaseIdMismatch {
            manifest: case.id.to_owned(),
            reference: case.reference.case_id.clone(),
        });
    }
    if case.reference.operator.name.trim().is_empty() {
        return Err(ValidationError::InvalidProvenance { field: "name" });
    }
    if case.reference.operator.version.trim().is_empty() {
        return Err(ValidationError::InvalidProvenance { field: "version" });
    }
    let configuration_sha256 = case.reference.operator.configuration_sha256.as_bytes();
    if configuration_sha256.len() != 64
        || !configuration_sha256
            .iter()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(ValidationError::InvalidProvenance {
            field: "configuration_sha256",
        });
    }
    if require_independent && !case.reference.operator.scientifically_independent {
        return Err(ValidationError::NonIndependentReference {
            operator: case.reference.operator.name.clone(),
        });
    }
    let mut pairs: BTreeMap<String, Vec<Pair>> = BTreeMap::new();
    let mut missing_candidate_samples = 0usize;
    let mut nonfinite_reference_samples = 0usize;
    let mut geometry_mismatch_samples = 0usize;
    let mut mismatch_examples = Vec::new();
    for sample in &case.reference.samples {
        if !sample.value.is_finite() {
            nonfinite_reference_samples += 1;
            continue;
        }
        let Some((candidate, geometry)) = candidate_sample(case.candidate, sample) else {
            missing_candidate_samples += 1;
            continue;
        };
        if let Some(reason) = geometry_mismatch(sample, geometry, tolerances) {
            geometry_mismatch_samples += 1;
            if mismatch_examples.len() < 12 {
                mismatch_examples.push(GeometryMismatchExample {
                    moment: sample.moment.clone(),
                    cut_index: sample.cut_index,
                    radial_index: sample.radial_index,
                    gate_index: sample.gate_index,
                    reason,
                });
            }
            continue;
        }
        if !candidate.is_finite() {
            missing_candidate_samples += 1;
            continue;
        }
        let reference = f64::from(sample.value);
        let candidate = f64::from(candidate);
        let delta = if is_phase_moment(&sample.moment) {
            circular_difference_deg(candidate, reference)
        } else {
            candidate - reference
        };
        pairs
            .entry(sample.moment.to_ascii_uppercase())
            .or_default()
            .push(Pair {
                candidate,
                reference,
                delta,
            });
    }
    let moments = pairs
        .iter()
        .map(|(moment, pairs)| (moment.clone(), score_pairs(pairs)))
        .collect();
    let paired_samples = pairs.values().map(Vec::len).sum();
    Ok(CaseComputation {
        scorecard: CaseScorecard {
            id: case.id.to_owned(),
            site_id: case.candidate.site.id.clone(),
            volume_time_utc: case.candidate.volume_time.to_rfc3339(),
            candidate_forward_operator: case.candidate.metadata.forward_operator.clone(),
            candidate_forward_operator_config: case
                .candidate
                .metadata
                .forward_operator_config
                .clone(),
            reference_operator: case.reference.operator.clone(),
            moments,
            requested_samples: case.reference.samples.len(),
            paired_samples,
            missing_candidate_samples,
            nonfinite_reference_samples,
            geometry_mismatch_samples,
            geometry_mismatch_examples: mismatch_examples,
        },
        pairs,
    })
}

#[derive(Clone, Copy)]
struct CandidateGeometry {
    azimuth_deg: f32,
    elevation_deg: f32,
    range_m: f32,
    acquisition_offset_ms: i32,
}

fn candidate_sample(
    volume: &RadarVolume,
    reference: &ReferenceSample,
) -> Option<(f32, CandidateGeometry)> {
    let cut = volume.cuts.get(reference.cut_index)?;
    let radial = cut.radials.get(reference.radial_index)?;
    let grid = find_moment_grid(cut.moments.iter(), &reference.moment)?;
    let row = grid
        .radial_indices
        .iter()
        .position(|radial_index| *radial_index == reference.radial_index)?;
    let value = grid.scaled_value(row, reference.gate_index)?;
    let range_m = grid.gate_range.first_gate_m as f32
        + reference.gate_index as f32 * grid.gate_range.gate_spacing_m as f32;
    Some((
        value,
        CandidateGeometry {
            azimuth_deg: radial.azimuth_deg,
            elevation_deg: radial.elevation_deg,
            range_m,
            acquisition_offset_ms: radial.time_offset_ms,
        },
    ))
}

fn find_moment_grid<'a>(
    moments: impl Iterator<Item = (&'a MomentType, &'a MomentGrid)>,
    requested: &str,
) -> Option<&'a MomentGrid> {
    moments
        .into_iter()
        .find(|(moment, _)| moment.short_name().eq_ignore_ascii_case(requested))
        .map(|(_, grid)| grid)
}

fn geometry_mismatch(
    reference: &ReferenceSample,
    candidate: CandidateGeometry,
    tolerances: GeometryTolerances,
) -> Option<String> {
    let azimuth_error = circular_difference_deg(
        f64::from(candidate.azimuth_deg),
        f64::from(reference.azimuth_deg),
    )
    .abs() as f32;
    if azimuth_error > tolerances.azimuth_deg {
        return Some(format!(
            "azimuth differs by {azimuth_error:.4} deg (limit {:.4})",
            tolerances.azimuth_deg
        ));
    }
    let elevation_error = (candidate.elevation_deg - reference.elevation_deg).abs();
    if elevation_error > tolerances.elevation_deg {
        return Some(format!(
            "elevation differs by {elevation_error:.4} deg (limit {:.4})",
            tolerances.elevation_deg
        ));
    }
    let range_error = (candidate.range_m - reference.range_m).abs();
    if range_error > tolerances.range_m {
        return Some(format!(
            "range differs by {range_error:.3} m (limit {:.3})",
            tolerances.range_m
        ));
    }
    let time_error = (i64::from(candidate.acquisition_offset_ms)
        - i64::from(reference.acquisition_offset_ms))
    .unsigned_abs();
    if time_error > tolerances.acquisition_time_ms.max(0) as u64 {
        return Some(format!(
            "acquisition time differs by {time_error} ms (limit {})",
            tolerances.acquisition_time_ms
        ));
    }
    None
}

fn score_pairs(pairs: &[Pair]) -> MomentScore {
    if pairs.is_empty() {
        return MomentScore::default();
    }
    let count = pairs.len();
    let denominator = count as f64;
    let candidate_mean = pairs.iter().map(|pair| pair.candidate).sum::<f64>() / denominator;
    let reference_mean = pairs.iter().map(|pair| pair.reference).sum::<f64>() / denominator;
    let bias = pairs.iter().map(|pair| pair.delta).sum::<f64>() / denominator;
    let mean_absolute_error = pairs.iter().map(|pair| pair.delta.abs()).sum::<f64>() / denominator;
    let root_mean_square_error = (pairs
        .iter()
        .map(|pair| pair.delta * pair.delta)
        .sum::<f64>()
        / denominator)
        .sqrt();
    let mut absolute_errors = pairs
        .iter()
        .map(|pair| pair.delta.abs())
        .collect::<Vec<_>>();
    absolute_errors.sort_by(f64::total_cmp);
    let median_absolute_error = percentile(&absolute_errors, 0.5);
    let p95_absolute_error = percentile(&absolute_errors, 0.95);
    let mut covariance = 0.0;
    let mut candidate_variance = 0.0;
    let mut reference_variance = 0.0;
    for pair in pairs {
        let candidate_anomaly = pair.candidate - candidate_mean;
        let reference_anomaly = pair.reference - reference_mean;
        covariance += candidate_anomaly * reference_anomaly;
        candidate_variance += candidate_anomaly * candidate_anomaly;
        reference_variance += reference_anomaly * reference_anomaly;
    }
    let pearson_correlation = (candidate_variance > 0.0 && reference_variance > 0.0)
        .then(|| covariance / (candidate_variance * reference_variance).sqrt());
    MomentScore {
        paired_count: count,
        bias,
        mean_absolute_error,
        root_mean_square_error,
        median_absolute_error,
        p95_absolute_error,
        pearson_correlation,
        candidate_mean,
        reference_mean,
    }
}

fn percentile(sorted: &[f64], probability: f64) -> f64 {
    if sorted.len() == 1 {
        return sorted[0];
    }
    let index = probability.clamp(0.0, 1.0) * (sorted.len() - 1) as f64;
    let lower = index.floor() as usize;
    let upper = index.ceil() as usize;
    let alpha = index - lower as f64;
    sorted[lower] + alpha * (sorted[upper] - sorted[lower])
}

fn is_phase_moment(moment: &str) -> bool {
    matches!(
        moment.to_ascii_uppercase().as_str(),
        "PHI" | "PHIDP" | "DIF_PHI"
    )
}

fn circular_difference_deg(left: f64, right: f64) -> f64 {
    (left - right + 180.0).rem_euclid(360.0) - 180.0
}

fn validate_tolerances(tolerances: GeometryTolerances) -> Result<(), ValidationError> {
    for (field, value) in [
        ("azimuth_deg", tolerances.azimuth_deg),
        ("elevation_deg", tolerances.elevation_deg),
        ("range_m", tolerances.range_m),
    ] {
        if !value.is_finite() || value < 0.0 {
            return Err(ValidationError::InvalidTolerance { field });
        }
    }
    if tolerances.acquisition_time_ms < 0 {
        return Err(ValidationError::InvalidTolerance {
            field: "acquisition_time_ms",
        });
    }
    Ok(())
}

pub fn render_markdown(scorecard: &MultiCaseScorecard) -> String {
    use std::fmt::Write as _;

    let mut output = String::new();
    let _ = writeln!(output, "# Simulated radar validation scorecard\n");
    let _ = writeln!(
        output,
        "Cases: {}  \nPaired samples: {} / {}  \nMissing candidate: {}  \nGeometry mismatches: {}\n",
        scorecard.cases.len(),
        scorecard.total_paired_samples,
        scorecard.total_requested_samples,
        scorecard.total_missing_candidate_samples,
        scorecard.total_geometry_mismatch_samples,
    );
    output.push_str("## Pooled moments\n\n");
    output.push_str("| Moment | N | Bias | MAE | RMSE | P95 abs | Correlation |\n");
    output.push_str("|---|---:|---:|---:|---:|---:|---:|\n");
    for (moment, score) in &scorecard.pooled_moments {
        let correlation = score
            .pearson_correlation
            .map(|value| format!("{value:.4}"))
            .unwrap_or_else(|| "n/a".to_owned());
        let _ = writeln!(
            output,
            "| {moment} | {} | {:.4} | {:.4} | {:.4} | {:.4} | {correlation} |",
            score.paired_count,
            score.bias,
            score.mean_absolute_error,
            score.root_mean_square_error,
            score.p95_absolute_error,
        );
    }
    for case in &scorecard.cases {
        let _ = writeln!(output, "\n## {}\n", case.id);
        let _ = writeln!(
            output,
            "Candidate: {} at {}  \nReference: {} {} (`{}`)  \nPaired: {} / {}; missing {}; geometry mismatches {}\n",
            case.site_id,
            case.volume_time_utc,
            case.reference_operator.name,
            case.reference_operator.version,
            case.reference_operator.configuration_sha256,
            case.paired_samples,
            case.requested_samples,
            case.missing_candidate_samples,
            case.geometry_mismatch_samples,
        );
    }
    output
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use radar_core::{ElevationCut, GateRange, MomentStorage, RadarSite, Radial};

    use super::*;

    fn volume(values: &[f32]) -> RadarVolume {
        let mut volume = RadarVolume::new(
            RadarSite::new("KVAL"),
            Utc.with_ymd_and_hms(2026, 7, 12, 12, 0, 0).unwrap(),
        );
        let gate_range = GateRange {
            first_gate_m: 500,
            gate_spacing_m: 250,
            gate_count: values.len(),
        };
        let mut cut = ElevationCut::new(0.5, Some(1));
        cut.radials.push(Radial {
            azimuth_deg: 359.9,
            elevation_deg: 0.5,
            time_offset_ms: 125,
            gate_range: gate_range.clone(),
            nyquist_velocity_mps: Some(25.0),
            radial_status: None,
        });
        cut.moments.insert(
            MomentType::Reflectivity,
            MomentGrid {
                moment: MomentType::Reflectivity,
                gate_range,
                scale: 1.0,
                offset: 0.0,
                nodata: None,
                range_folded: None,
                radial_indices: vec![0],
                storage: MomentStorage::F32(values.to_vec()),
            },
        );
        volume.cuts.push(cut);
        volume
    }

    fn reference(case_id: &str, values: &[f32]) -> IndependentReferenceSet {
        IndependentReferenceSet {
            schema_version: VALIDATION_SCHEMA_VERSION,
            case_id: case_id.to_owned(),
            operator: IndependentOperatorProvenance {
                name: "independent".to_owned(),
                version: "1".to_owned(),
                configuration_sha256: "a".repeat(64),
                scientifically_independent: true,
                source_url: None,
                notes: Vec::new(),
            },
            source_identity: None,
            samples: values
                .iter()
                .enumerate()
                .map(|(gate, value)| ReferenceSample {
                    cut_index: 0,
                    radial_index: 0,
                    gate_index: gate,
                    moment: "REF".to_owned(),
                    value: *value,
                    azimuth_deg: -0.1,
                    elevation_deg: 0.5,
                    range_m: 500.0 + gate as f32 * 250.0,
                    acquisition_offset_ms: 125,
                })
                .collect(),
        }
    }

    #[test]
    fn exact_geometry_scorecard_computes_metrics() {
        let candidate = volume(&[1.0, 3.0, 5.0]);
        let reference = reference("one", &[0.0, 4.0, 4.0]);
        let result = validate_cases(
            &[ValidationCaseData {
                id: "one",
                candidate: &candidate,
                reference: &reference,
            }],
            GeometryTolerances::default(),
            true,
        )
        .unwrap();
        let score = result.pooled_moments.get("REF").unwrap();
        assert_eq!(score.paired_count, 3);
        assert!((score.bias - 1.0 / 3.0).abs() < 1.0e-12);
        assert!((score.mean_absolute_error - 1.0).abs() < 1.0e-12);
        assert_eq!(result.total_geometry_mismatch_samples, 0);
    }

    #[test]
    fn geometry_mismatch_is_not_scored() {
        let candidate = volume(&[1.0]);
        let mut reference = reference("mismatch", &[1.0]);
        reference.samples[0].range_m += 100.0;
        let result = validate_cases(
            &[ValidationCaseData {
                id: "mismatch",
                candidate: &candidate,
                reference: &reference,
            }],
            GeometryTolerances::default(),
            true,
        )
        .unwrap();
        assert_eq!(result.total_paired_samples, 0);
        assert_eq!(result.total_geometry_mismatch_samples, 1);
    }

    #[test]
    fn non_independent_reference_fails_closed() {
        let candidate = volume(&[1.0]);
        let mut reference = reference("blocked", &[1.0]);
        reference.operator.scientifically_independent = false;
        assert!(matches!(
            validate_cases(
                &[ValidationCaseData {
                    id: "blocked",
                    candidate: &candidate,
                    reference: &reference,
                }],
                GeometryTolerances::default(),
                true,
            ),
            Err(ValidationError::NonIndependentReference { .. })
        ));
    }

    #[test]
    fn pooled_metrics_use_samples_not_average_of_case_scores() {
        let candidate_a = volume(&[1.0]);
        let reference_a = reference("a", &[0.0]);
        let candidate_b = volume(&[2.0, 2.0, 2.0]);
        let reference_b = reference("b", &[0.0, 0.0, 0.0]);
        let result = validate_cases(
            &[
                ValidationCaseData {
                    id: "a",
                    candidate: &candidate_a,
                    reference: &reference_a,
                },
                ValidationCaseData {
                    id: "b",
                    candidate: &candidate_b,
                    reference: &reference_b,
                },
            ],
            GeometryTolerances::default(),
            true,
        )
        .unwrap();
        assert_eq!(result.pooled_moments["REF"].paired_count, 4);
        assert!((result.pooled_moments["REF"].bias - 1.75).abs() < 1.0e-12);
    }

    #[test]
    fn markdown_contains_auditable_counts_and_operator() {
        let candidate = volume(&[1.0]);
        let reference = reference("markdown", &[1.0]);
        let result = validate_cases(
            &[ValidationCaseData {
                id: "markdown",
                candidate: &candidate,
                reference: &reference,
            }],
            GeometryTolerances::default(),
            true,
        )
        .unwrap();
        let markdown = render_markdown(&result);
        assert!(markdown.contains("Paired samples: 1 / 1"));
        assert!(markdown.contains("Reference: independent 1"));
    }
}
