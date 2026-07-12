use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::Sha256Digest;

/// Numerical scattering kernel used to generate a table.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "model", rename_all = "snake_case")]
pub enum KernelModel {
    RayleighSphere,
    TMatrix {
        implementation: TMatrixImplementation,
    },
    /// Reserved for analytic/unit-test data with no physical interpretation.
    SyntheticFixtureOnly,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "implementation", rename_all = "snake_case")]
pub enum TMatrixImplementation {
    #[serde(rename = "pytmatrix_0_3_3")]
    PyTMatrix033,
    ExternalResearch {
        engine: String,
        version: String,
    },
}

/// Orientation distribution represented by the generated values.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "model", rename_all = "snake_case")]
pub enum OrientationModel {
    /// Values remain in the particle body frame; callers must transform them.
    ExplicitBodyFrame,
    FixedEuler {
        yaw_deg: f64,
        pitch_deg: f64,
        roll_deg: f64,
    },
    GaussianCanting {
        mean_deg: f64,
        standard_deviation_deg: f64,
        quadrature_points: u16,
    },
    Isotropic {
        quadrature_points: u16,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectiveMediumRule {
    MaxwellGarnett,
    Bruggeman,
}

/// Dielectric/geometry representation of melting particles.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "model", rename_all = "snake_case")]
pub enum MeltingModel {
    Dry,
    HomogeneousEffectiveMedium { rule: EffectiveMediumRule },
    WaterCoated { shell_parameterization: String },
    SchemeResolved,
}

/// Time representation of one table lookup or generated sample.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "sampling", rename_all = "snake_case")]
pub enum TemporalSampling {
    Instantaneous,
    FrozenDuringBeamDwell,
    TimeAveraged { window_seconds: f64, samples: u32 },
}

/// Evidence status is explicit and has no implicit "production" default.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TableValidation {
    SyntheticFixtureOnly,
    ResearchOnlyUnvalidated,
    HeldOutValidated {
        report_id: String,
        report_sha256: Sha256Digest,
    },
}

/// Science choices that must travel with every LUT.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScienceMetadata {
    kernel: KernelModel,
    orientation: OrientationModel,
    melting: MeltingModel,
    temporal: TemporalSampling,
    validation: TableValidation,
}

impl ScienceMetadata {
    pub fn new(
        kernel: KernelModel,
        orientation: OrientationModel,
        melting: MeltingModel,
        temporal: TemporalSampling,
        validation: TableValidation,
    ) -> Result<Self, ScienceError> {
        let metadata = Self {
            kernel,
            orientation,
            melting,
            temporal,
            validation,
        };
        metadata.validate()?;
        Ok(metadata)
    }

    #[must_use]
    pub const fn kernel(&self) -> &KernelModel {
        &self.kernel
    }

    #[must_use]
    pub const fn orientation(&self) -> &OrientationModel {
        &self.orientation
    }

    #[must_use]
    pub const fn melting(&self) -> &MeltingModel {
        &self.melting
    }

    #[must_use]
    pub const fn temporal(&self) -> &TemporalSampling {
        &self.temporal
    }

    #[must_use]
    pub const fn validation(&self) -> &TableValidation {
        &self.validation
    }

    pub fn validate(&self) -> Result<(), ScienceError> {
        match &self.kernel {
            KernelModel::TMatrix {
                implementation: TMatrixImplementation::ExternalResearch { engine, version },
            } => {
                nonempty("external T-matrix engine", engine)?;
                nonempty("external T-matrix version", version)?;
            }
            KernelModel::RayleighSphere
            | KernelModel::TMatrix { .. }
            | KernelModel::SyntheticFixtureOnly => {}
        }

        match &self.orientation {
            OrientationModel::FixedEuler {
                yaw_deg,
                pitch_deg,
                roll_deg,
            } => {
                finite("orientation yaw", *yaw_deg)?;
                finite("orientation pitch", *pitch_deg)?;
                finite("orientation roll", *roll_deg)?;
            }
            OrientationModel::GaussianCanting {
                mean_deg,
                standard_deviation_deg,
                quadrature_points,
            } => {
                finite("canting mean", *mean_deg)?;
                finite_nonnegative("canting standard deviation", *standard_deviation_deg)?;
                if *quadrature_points == 0 {
                    return Err(ScienceError::ZeroQuadraturePoints);
                }
            }
            OrientationModel::Isotropic { quadrature_points } => {
                if *quadrature_points == 0 {
                    return Err(ScienceError::ZeroQuadraturePoints);
                }
            }
            OrientationModel::ExplicitBodyFrame => {}
        }

        if let MeltingModel::WaterCoated {
            shell_parameterization,
        } = &self.melting
        {
            nonempty("water-shell parameterization", shell_parameterization)?;
        }

        if let TemporalSampling::TimeAveraged {
            window_seconds,
            samples,
        } = &self.temporal
        {
            finite_positive("time-average window", *window_seconds)?;
            if *samples < 2 {
                return Err(ScienceError::TimeAverageSamples { samples: *samples });
            }
        }

        let synthetic_kernel = matches!(&self.kernel, KernelModel::SyntheticFixtureOnly);
        let synthetic_status = matches!(&self.validation, TableValidation::SyntheticFixtureOnly);
        if synthetic_kernel != synthetic_status {
            return Err(ScienceError::SyntheticLabelMismatch);
        }
        if let TableValidation::HeldOutValidated { report_id, .. } = &self.validation {
            nonempty("held-out validation report id", report_id)?;
        }
        Ok(())
    }

    /// Ensure an orientation choice has already produced radar-basis additive
    /// quantities. Body-frame amplitudes require the separate tensor transform
    /// before they can enter the schema-v1 additive LUT.
    pub fn validate_additive_lut_compatibility(&self) -> Result<(), ScienceError> {
        self.validate()?;
        if matches!(&self.orientation, OrientationModel::ExplicitBodyFrame) {
            return Err(ScienceError::BodyFrameRequiresAmplitudeTransform);
        }
        Ok(())
    }

    /// Require independently held-out validation before a consumer promotes a
    /// research table into an operational path.
    pub fn require_held_out_validation(&self) -> Result<(), ScienceError> {
        match &self.validation {
            TableValidation::HeldOutValidated { .. } => Ok(()),
            _ => Err(ScienceError::HeldOutValidationRequired),
        }
    }

    /// Verify the exact held-out report artifact named by the metadata.
    pub fn verify_held_out_report(
        &self,
        report_id: &str,
        report_bytes: &[u8],
    ) -> Result<(), ScienceError> {
        let TableValidation::HeldOutValidated {
            report_id: expected_id,
            report_sha256,
        } = &self.validation
        else {
            return Err(ScienceError::HeldOutValidationRequired);
        };
        if report_id != expected_id.as_str() {
            return Err(ScienceError::ValidationReportId {
                expected: expected_id.as_str().to_owned(),
                actual: report_id.to_owned(),
            });
        }
        let actual = Sha256Digest::compute(report_bytes);
        if actual != *report_sha256 {
            return Err(ScienceError::ValidationReportDigest {
                expected: *report_sha256,
                actual,
            });
        }
        Ok(())
    }
}

fn nonempty(field: &'static str, value: &str) -> Result<(), ScienceError> {
    if value.trim().is_empty() {
        Err(ScienceError::EmptyText { field })
    } else {
        Ok(())
    }
}

fn finite(field: &'static str, value: f64) -> Result<(), ScienceError> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(ScienceError::NonFinite { field, value })
    }
}

fn finite_nonnegative(field: &'static str, value: f64) -> Result<(), ScienceError> {
    finite(field, value)?;
    if value >= 0.0 {
        Ok(())
    } else {
        Err(ScienceError::OutOfRange { field, value })
    }
}

fn finite_positive(field: &'static str, value: f64) -> Result<(), ScienceError> {
    finite(field, value)?;
    if value > 0.0 {
        Ok(())
    } else {
        Err(ScienceError::OutOfRange { field, value })
    }
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum ScienceError {
    #[error("{field} must not be empty")]
    EmptyText { field: &'static str },
    #[error("{field} must be finite, got {value}")]
    NonFinite { field: &'static str, value: f64 },
    #[error("{field} is outside its valid range: {value}")]
    OutOfRange { field: &'static str, value: f64 },
    #[error("orientation quadrature must contain at least one point")]
    ZeroQuadraturePoints,
    #[error("a time average requires at least two samples, got {samples}")]
    TimeAverageSamples { samples: u32 },
    #[error("synthetic-fixture kernel and validation labels must appear together")]
    SyntheticLabelMismatch,
    #[error("independently generated held-out validation is required")]
    HeldOutValidationRequired,
    #[error(
        "body-frame amplitudes must be transformed into a declared radar H/V basis before entering an additive LUT"
    )]
    BodyFrameRequiresAmplitudeTransform,
    #[error("held-out validation report id mismatch: expected {expected:?}, got {actual:?}")]
    ValidationReportId { expected: String, actual: String },
    #[error("held-out validation report SHA-256 mismatch: expected {expected}, got {actual}")]
    ValidationReportDigest {
        expected: Sha256Digest,
        actual: Sha256Digest,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_label_cannot_be_applied_to_a_tmatrix_table() {
        let error = ScienceMetadata::new(
            KernelModel::TMatrix {
                implementation: TMatrixImplementation::PyTMatrix033,
            },
            OrientationModel::ExplicitBodyFrame,
            MeltingModel::Dry,
            TemporalSampling::Instantaneous,
            TableValidation::SyntheticFixtureOnly,
        )
        .unwrap_err();
        assert_eq!(error, ScienceError::SyntheticLabelMismatch);
    }

    #[test]
    fn research_table_must_explicitly_pass_held_out_gate() {
        let metadata = ScienceMetadata::new(
            KernelModel::TMatrix {
                implementation: TMatrixImplementation::PyTMatrix033,
            },
            OrientationModel::GaussianCanting {
                mean_deg: 0.0,
                standard_deviation_deg: 10.0,
                quadrature_points: 16,
            },
            MeltingModel::Dry,
            TemporalSampling::Instantaneous,
            TableValidation::ResearchOnlyUnvalidated,
        )
        .unwrap();
        assert_eq!(
            metadata.require_held_out_validation(),
            Err(ScienceError::HeldOutValidationRequired)
        );
    }

    #[test]
    fn held_out_status_verifies_the_exact_report_artifact() {
        let report = b"synthetic validation-report fixture";
        let metadata = ScienceMetadata::new(
            KernelModel::RayleighSphere,
            OrientationModel::FixedEuler {
                yaw_deg: 0.0,
                pitch_deg: 0.0,
                roll_deg: 0.0,
            },
            MeltingModel::Dry,
            TemporalSampling::Instantaneous,
            TableValidation::HeldOutValidated {
                report_id: "fixture-report-v1".to_owned(),
                report_sha256: Sha256Digest::compute(report),
            },
        )
        .unwrap();
        metadata
            .verify_held_out_report("fixture-report-v1", report)
            .unwrap();
        assert!(matches!(
            metadata.verify_held_out_report("fixture-report-v1", b"changed"),
            Err(ScienceError::ValidationReportDigest { .. })
        ));
    }

    #[test]
    fn additive_lut_rejects_untransformed_body_frame_metadata() {
        let metadata = ScienceMetadata::new(
            KernelModel::RayleighSphere,
            OrientationModel::ExplicitBodyFrame,
            MeltingModel::Dry,
            TemporalSampling::Instantaneous,
            TableValidation::ResearchOnlyUnvalidated,
        )
        .unwrap();
        assert_eq!(
            metadata.validate_additive_lut_compatibility(),
            Err(ScienceError::BodyFrameRequiresAmplitudeTransform)
        );
    }
}
