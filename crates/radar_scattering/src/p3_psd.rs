//! Source-qualified WRF P3 particle-size-distribution reconstruction.
//!
//! This module follows the official WRF v4.7.1 P3 implementation pinned at
//! commit [`f52c197e`](https://github.com/wrf-model/WRF/tree/f52c197ed39d12e087d02c50f412d90d418f6186):
//! `phys/module_mp_p3.F`, `run/create_p3_lookupTable_1.f90-v5.4`, and lookup
//! tables `p3_lookupTable_1.dat-v5.4_{2momI,3momI}`. The governing particle
//! model is Morrison and Milbrandt (2015), DOI 10.1175/JAS-D-14-0065.1; the
//! multiple-free-category extension used by WRF `mp_physics=52` is Milbrandt
//! and Morrison (2016), DOI 10.1175/JAS-D-15-0204.1. Triple-moment context is
//! Morrison, Milbrandt, and Cholette (2025), DOI 10.1029/2024MS004644; the
//! pinned WRF source remains authoritative for its current QZI transform.
//!
//! WRF does not derive two-moment P3 `lambda` and `mu` from a closed analytic
//! expression. It interpolates a versioned lookup table generated using
//! incomplete-gamma mass integrals. Triple-moment P3 additionally iterates
//! through that table while recovering `mu` from `M0/M3/M6`. Consequently this
//! crate requires a typed implementation of [`P3LookupTableV54`] and refuses to
//! replace the missing official data with a characteristic-particle closure.
//!
//! The piecewise mass and projected-area laws are implemented directly from
//! the pinned table generator. P3 supplies maximum dimension and projected
//! area, but no unique spheroidal axis ratio or orientation distribution; node
//! geometry states that limitation explicitly instead of inventing T-matrix
//! shape information.

use std::f64::consts::PI;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{P3Category, Sha256Digest};

pub const P3_PSD_REVISION: &str = "wrf-p3-v4.5.2-table-v5.4-psd-v2";
pub const P3_WRF_SOURCE_COMMIT: &str = "f52c197ed39d12e087d02c50f412d90d418f6186";
pub const P3_WRF_RELEASE: &str = "v4.7.1";
pub const P3_MODULE_VERSION: &str = "4.5.2";
pub const P3_TABLE_GENERATOR_VERSION: &str = "5.4";
pub const P3_TWO_MOMENT_TABLE_VERSION: &str = "5.4_2momI";
pub const P3_THREE_MOMENT_TABLE_VERSION: &str = "5.4_3momI";
/// SHA-256 of the raw official table file at [`P3_WRF_SOURCE_COMMIT`].
pub const P3_TWO_MOMENT_TABLE_SHA256: &str =
    "be1ab6fb03481e376e47c6c79d808af5d8ab069f2b242931e9c54801bad4ae84";
/// SHA-256 of the raw official table file at [`P3_WRF_SOURCE_COMMIT`].
pub const P3_THREE_MOMENT_TABLE_SHA256: &str =
    "9a3c57ecc09498802c8d7cb3931dbb0200dcf0f51466b3e288120c080271e6dc";
pub const P3_PART_I_DOI: &str = "10.1175/JAS-D-14-0065.1";
pub const P3_MULTICATEGORY_DOI: &str = "10.1175/JAS-D-15-0204.1";
pub const P3_TRIPLE_MOMENT_CONTEXT_DOI: &str = "10.1029/2024MS004644";

pub const P3_RIME_DENSITY_RANGE_KG_M3: [f64; 2] = [50.0, 900.0];
pub const P3_MU_RANGE: [f64; 2] = [0.0, 20.0];
/// P3's default-`REAL` minimum ice mass mixing ratio.
pub const P3_WRF_QSMALL_KGKG: f32 = 1.0e-14;
/// P3's default-`REAL` minimum ice number mixing ratio.
pub const P3_WRF_NSMALL_PER_KG: f32 = 1.0e-16;

const SOLID_ICE_DENSITY_KG_M3: f64 = 900.0;
const UNRIMED_MASS_COEFFICIENT: f64 = 0.0121;
const UNRIMED_MASS_EXPONENT: f64 = 1.9;
const RIMED_MASS_EXPONENT: f64 = 3.0;
const UNRIMED_AREA_EXPONENT: f64 = 1.88;
// Exact evaluation of WRF's `0.2285 * 100**1.88 / 100**2` unit conversion.
const UNRIMED_AREA_COEFFICIENT: f64 = 0.131_488_025_681_54;
const ARBITRARY_LARGE_DIAMETER_M: f64 = 1.0e6;
const RIME_DENSITY_ITERATION_TOLERANCE: f64 = 0.01;
const MAXIMUM_RIME_DENSITY_ITERATIONS: usize = 256;
const NUMERICAL_MAX_ITERATIONS: usize = 256;
const NUMERICAL_EPSILON: f64 = 2.0e-14;
const NUMERICAL_FLOOR: f64 = 1.0e-300;
const GL8_POINTS: usize = 8;

const GL8_ABSCISSAE: [f64; GL8_POINTS] = [
    -0.960_289_856_497_536_3,
    -0.796_666_477_413_626_7,
    -0.525_532_409_916_329,
    -0.183_434_642_495_649_8,
    0.183_434_642_495_649_8,
    0.525_532_409_916_329,
    0.796_666_477_413_626_7,
    0.960_289_856_497_536_3,
];
const GL8_WEIGHTS: [f64; GL8_POINTS] = [
    0.101_228_536_290_376_3,
    0.222_381_034_453_374_5,
    0.313_706_645_877_887_3,
    0.362_683_783_378_362,
    0.362_683_783_378_362,
    0.313_706_645_877_887_3,
    0.222_381_034_453_374_5,
    0.101_228_536_290_376_3,
];

/// The four P3 configurations exposed by official WRF.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum P3WrfScheme {
    Mp50OneIceFixedCloudNumber,
    Mp51OneIcePredictedCloudNumber,
    Mp52TwoIcePredictedCloudNumber,
    Mp53OneIceTripleMoment,
}

impl P3WrfScheme {
    #[must_use]
    pub const fn mp_physics(self) -> i32 {
        match self {
            Self::Mp50OneIceFixedCloudNumber => 50,
            Self::Mp51OneIcePredictedCloudNumber => 51,
            Self::Mp52TwoIcePredictedCloudNumber => 52,
            Self::Mp53OneIceTripleMoment => 53,
        }
    }

    #[must_use]
    pub const fn moment_order(self) -> P3IceMomentOrder {
        match self {
            Self::Mp53OneIceTripleMoment => P3IceMomentOrder::TripleMomentQzi,
            _ => P3IceMomentOrder::TwoMoment,
        }
    }

    #[must_use]
    pub const fn category_count(self) -> usize {
        match self {
            Self::Mp52TwoIcePredictedCloudNumber => 2,
            _ => 1,
        }
    }

    #[must_use]
    pub const fn required_table_version(self) -> &'static str {
        match self.moment_order() {
            P3IceMomentOrder::TwoMoment => P3_TWO_MOMENT_TABLE_VERSION,
            P3IceMomentOrder::TripleMomentQzi => P3_THREE_MOMENT_TABLE_VERSION,
        }
    }

    #[must_use]
    pub const fn required_table_sha256(self) -> &'static str {
        match self.moment_order() {
            P3IceMomentOrder::TwoMoment => P3_TWO_MOMENT_TABLE_SHA256,
            P3IceMomentOrder::TripleMomentQzi => P3_THREE_MOMENT_TABLE_SHA256,
        }
    }
}

impl TryFrom<i32> for P3WrfScheme {
    type Error = P3PsdError;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            50 => Ok(Self::Mp50OneIceFixedCloudNumber),
            51 => Ok(Self::Mp51OneIcePredictedCloudNumber),
            52 => Ok(Self::Mp52TwoIcePredictedCloudNumber),
            53 => Ok(Self::Mp53OneIceTripleMoment),
            _ => Err(P3PsdError::UnsupportedScheme { mp_physics: value }),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum P3IceMomentOrder {
    TwoMoment,
    TripleMomentQzi,
}

/// The optional third ice moment as stored in a WRF history file.
///
/// `module_mp_p3.F` advects `sqrt(QNICE * M6)`. Before P3_MAIN it recovers the
/// native sixth-moment mixing ratio as `QZI^2 / QNICE`; after the call it
/// applies the inverse transform. This type prevents treating history-file
/// QZI itself as M6.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum P3IceMomentInput {
    TwoMoment,
    WrfAdvectedQzi { qzi_sqrt_n_times_m6: f64 },
}

/// Native WRF P3 state for one free ice category.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct P3PsdInput {
    pub scheme: P3WrfScheme,
    pub category: P3Category,
    pub total_ice_kgkg: f64,
    pub total_number_per_kg: f64,
    pub rime_mass_kgkg: f64,
    pub rime_volume_m3_per_kg: f64,
    pub dry_air_density_kg_m3: f64,
    pub moment: P3IceMomentInput,
}

impl P3PsdInput {
    #[must_use]
    pub const fn two_moment(
        scheme: P3WrfScheme,
        category: P3Category,
        total_ice_kgkg: f64,
        total_number_per_kg: f64,
        rime_mass_kgkg: f64,
        rime_volume_m3_per_kg: f64,
        dry_air_density_kg_m3: f64,
    ) -> Self {
        Self {
            scheme,
            category,
            total_ice_kgkg,
            total_number_per_kg,
            rime_mass_kgkg,
            rime_volume_m3_per_kg,
            dry_air_density_kg_m3,
            moment: P3IceMomentInput::TwoMoment,
        }
    }

    #[must_use]
    pub const fn triple_moment_qzi(
        total_ice_kgkg: f64,
        total_number_per_kg: f64,
        rime_mass_kgkg: f64,
        rime_volume_m3_per_kg: f64,
        qzi_sqrt_n_times_m6: f64,
        dry_air_density_kg_m3: f64,
    ) -> Self {
        Self {
            scheme: P3WrfScheme::Mp53OneIceTripleMoment,
            category: P3Category::Category1,
            total_ice_kgkg,
            total_number_per_kg,
            rime_mass_kgkg,
            rime_volume_m3_per_kg,
            dry_air_density_kg_m3,
            moment: P3IceMomentInput::WrfAdvectedQzi {
                qzi_sqrt_n_times_m6,
            },
        }
    }
}

/// Exact provenance required of a lookup table implementation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct P3LookupTableDescriptor {
    pub wrf_source_commit: String,
    pub p3_module_version: String,
    pub generator_version: String,
    pub table_version: String,
    pub table_sha256: Sha256Digest,
}

impl P3LookupTableDescriptor {
    fn validate_for(&self, scheme: P3WrfScheme) -> Result<(), P3PsdError> {
        for (field, actual, expected) in [
            (
                "WRF source commit",
                self.wrf_source_commit.as_str(),
                P3_WRF_SOURCE_COMMIT,
            ),
            (
                "P3 module version",
                self.p3_module_version.as_str(),
                P3_MODULE_VERSION,
            ),
            (
                "P3 table generator version",
                self.generator_version.as_str(),
                P3_TABLE_GENERATOR_VERSION,
            ),
            (
                "P3 table version",
                self.table_version.as_str(),
                scheme.required_table_version(),
            ),
        ] {
            if actual != expected {
                return Err(P3PsdError::LookupRevisionMismatch {
                    field,
                    expected: expected.to_owned(),
                    actual: actual.to_owned(),
                });
            }
        }
        let expected_digest = scheme.required_table_sha256();
        let actual_digest = self.table_sha256.to_hex();
        if actual_digest != expected_digest {
            return Err(P3PsdError::LookupDigestMismatch {
                expected: expected_digest.to_owned(),
                actual: actual_digest,
            });
        }
        Ok(())
    }
}

/// Physical coordinates passed to the official table interpolation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct P3LookupQuery {
    pub scheme: P3WrfScheme,
    pub category: P3Category,
    pub rime_mass_fraction: f64,
    pub rime_density_kg_m3: f64,
    /// Unmodified WRF history-field value. Negative finite leading-edge values
    /// are repaired by the exact P3 number-limiter sequence during lookup.
    pub total_number_per_kg: f64,
    pub total_ice_kgkg: f64,
    pub sixth_moment_per_kg: Option<f64>,
}

/// Exact default-`REAL` P3 number repair and mean-size limiter audit.
///
/// WRF first applies `Ni=max(Ni,nsmall)`, then interpolates the table fields
/// `i_qsmall` (`inv_Qmin`, field 7) and `i_qlarge` (`inv_Qmax`, field 8) at the
/// same coordinates as lambda/mu, and finally applies the upper and lower Ni
/// bounds in that order.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct P3NumberLimiterAudit {
    pub original_total_number_per_kg: f64,
    pub wrf_real_original_total_number_per_kg: f64,
    pub after_nsmall_total_number_per_kg: f64,
    pub inverse_qmin_per_kg: f64,
    pub inverse_qmax_per_kg: f64,
    pub maximum_total_number_per_kg: f64,
    pub minimum_total_number_per_kg: f64,
    pub repaired_total_number_per_kg: f64,
    pub nsmall_applied: bool,
    pub maximum_applied: bool,
    pub minimum_applied: bool,
}

impl P3NumberLimiterAudit {
    /// Apply the pinned WRF statement sequence using binary32 after validating
    /// that every input can be represented as default `REAL`.
    pub fn from_wrf_real(
        original_total_number_per_kg: f64,
        total_ice_kgkg: f64,
        inverse_qmin_per_kg: f64,
        inverse_qmax_per_kg: f64,
    ) -> Result<Self, P3LookupFailure> {
        let original = wrf_real("total ice number", original_total_number_per_kg)?;
        let total_ice = wrf_real("total ice mass", total_ice_kgkg)?;
        let inverse_qmin = wrf_real("P3 table inv_Qmin", inverse_qmin_per_kg)?;
        let inverse_qmax = wrf_real("P3 table inv_Qmax", inverse_qmax_per_kg)?;
        if total_ice <= 0.0 {
            return Err(P3LookupFailure::OutsideDomain(format!(
                "total ice mass must be positive, got {total_ice}"
            )));
        }
        if inverse_qmin <= 0.0 || inverse_qmax <= 0.0 {
            return Err(P3LookupFailure::Corrupt(format!(
                "P3 number-limit multipliers must be positive, got inv_Qmin={inverse_qmin}, inv_Qmax={inverse_qmax}"
            )));
        }

        let after_nsmall = original.max(P3_WRF_NSMALL_PER_KG);
        let maximum = inverse_qmin * total_ice;
        let minimum = inverse_qmax * total_ice;
        if !maximum.is_finite() || !minimum.is_finite() || maximum < minimum {
            return Err(P3LookupFailure::Corrupt(format!(
                "P3 number limits are invalid: minimum={minimum}, maximum={maximum}"
            )));
        }
        let after_maximum = after_nsmall.min(maximum);
        let repaired = after_maximum.max(minimum);
        Ok(Self {
            original_total_number_per_kg,
            wrf_real_original_total_number_per_kg: f64::from(original),
            after_nsmall_total_number_per_kg: f64::from(after_nsmall),
            inverse_qmin_per_kg: f64::from(inverse_qmin),
            inverse_qmax_per_kg: f64::from(inverse_qmax),
            maximum_total_number_per_kg: f64::from(maximum),
            minimum_total_number_per_kg: f64::from(minimum),
            repaired_total_number_per_kg: f64::from(repaired),
            nsmall_applied: after_nsmall.to_bits() != original.to_bits(),
            maximum_applied: after_maximum.to_bits() != after_nsmall.to_bits(),
            minimum_applied: repaired.to_bits() != after_maximum.to_bits(),
        })
    }

    pub(crate) fn number_after_nsmall_wrf_real(
        original_total_number_per_kg: f64,
    ) -> Result<f32, P3LookupFailure> {
        Ok(wrf_real("total ice number", original_total_number_per_kg)?.max(P3_WRF_NSMALL_PER_KG))
    }
}

/// PSD parameters returned by exact interpolation of the required official
/// table. Axis clamping is retained because it is explicit WRF behavior.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct P3LookupSolution {
    pub slope_lambda_m_inv: f64,
    pub shape_mu: f64,
    pub axis_clamps: P3LookupAxisClamps,
    /// Interpolated table field 7 (`i_qsmall`/`inv_Qmin`).
    pub inverse_qmin_per_kg: f64,
    /// Interpolated table field 8 (`i_qlarge`/`inv_Qmax`).
    pub inverse_qmax_per_kg: f64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct P3LookupAxisClamps {
    pub normalized_mass: bool,
    pub rime_fraction: bool,
    pub rime_density: bool,
    pub shape: bool,
}

impl P3LookupAxisClamps {
    #[must_use]
    pub const fn any(self) -> bool {
        self.normalized_mass || self.rime_fraction || self.rime_density || self.shape
    }
}

/// Failure produced by a concrete official-table reader/interpolator.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum P3LookupFailure {
    #[error("P3 lookup state is outside the loaded table: {0}")]
    OutsideDomain(String),
    #[error("P3 lookup table is corrupt or incomplete: {0}")]
    Corrupt(String),
    #[error("P3 lookup operation is unsupported: {0}")]
    Unsupported(String),
}

/// Required exact-data seam. Implementations must reproduce the index mapping
/// and multilinear interpolation in the pinned `module_mp_p3.F`, including its
/// documented axis clamps. This crate intentionally provides no fallback.
pub trait P3LookupTableV54: Send + Sync {
    fn descriptor(&self) -> &P3LookupTableDescriptor;
    fn lookup_psd(&self, query: P3LookupQuery) -> Result<P3LookupSolution, P3LookupFailure>;
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct P3ReconstructionConfig {
    /// Maximum numerical relative error allowed after mass-normalizing the
    /// reconstructed PSD. Number and sixth-moment residuals remain audited,
    /// but are not hard gates: WRF linearly interpolates its coarse table and
    /// applies the number limiters after lookup, so those moments are not
    /// algebraically guaranteed to close between table nodes.
    pub maximum_moment_relative_error: f64,
}

impl Default for P3ReconstructionConfig {
    fn default() -> Self {
        Self {
            // The official generator mass-normalizes N0 after selecting
            // lambda/mu. Retain a strict numerical check on that authoritative
            // mass closure without rejecting expected interpolation residuals
            // in number or the optional sixth moment.
            maximum_moment_relative_error: 0.03,
        }
    }
}

/// Exact four-region P3 mass and projected-area law.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct P3PiecewiseParticleLaw {
    pub rime_mass_fraction: f64,
    pub rime_density_kg_m3: f64,
    pub small_sphere_limit_m: f64,
    pub dense_unrimed_to_graupel_m: f64,
    pub graupel_to_partially_rimed_m: f64,
    pub graupel_mass_coefficient: f64,
    pub partially_rimed_mass_coefficient: f64,
    pub partially_rimed_mass_exponent: f64,
    pub rime_density_iterations: u16,
}

impl P3PiecewiseParticleLaw {
    pub fn reconstruct(
        rime_mass_fraction: f64,
        rime_density_kg_m3: f64,
    ) -> Result<Self, P3PsdError> {
        fraction("P3 rime mass fraction", rime_mass_fraction)?;
        if rime_mass_fraction > 0.0 {
            in_range(
                "P3 rime density",
                rime_density_kg_m3,
                P3_RIME_DENSITY_RANGE_KG_M3,
            )?;
        }

        let small_sphere_limit_m = (PI / (6.0 * UNRIMED_MASS_COEFFICIENT)
            * SOLID_ICE_DENSITY_KG_M3)
            .powf(1.0 / (UNRIMED_MASS_EXPONENT - RIMED_MASS_EXPONENT));
        let effective_rime_density = if rime_mass_fraction > 0.0 {
            rime_density_kg_m3
        } else {
            P3_RIME_DENSITY_RANGE_KG_M3[0]
        };
        let rime_sphere_coefficient = effective_rime_density * PI / 6.0;
        let mut graupel_mass_coefficient = rime_sphere_coefficient;
        let (
            dense_unrimed_to_graupel_m,
            graupel_to_partially_rimed_m,
            partially_rimed_mass_coefficient,
            partially_rimed_mass_exponent,
            rime_density_iterations,
        ) = if rime_mass_fraction == 0.0 {
            (
                ARBITRARY_LARGE_DIAMETER_M,
                ARBITRARY_LARGE_DIAMETER_M,
                UNRIMED_MASS_COEFFICIENT,
                UNRIMED_MASS_EXPONENT,
                0,
            )
        } else if rime_mass_fraction == 1.0 {
            let dense_limit = (UNRIMED_MASS_COEFFICIENT / graupel_mass_coefficient)
                .powf(1.0 / (RIMED_MASS_EXPONENT - UNRIMED_MASS_EXPONENT));
            (
                dense_limit,
                ARBITRARY_LARGE_DIAMETER_M,
                graupel_mass_coefficient,
                RIMED_MASS_EXPONENT,
                0,
            )
        } else {
            let mut iterations = 0usize;
            let (dense_limit, partial_limit, partial_coefficient) = loop {
                iterations += 1;
                if iterations > MAXIMUM_RIME_DENSITY_ITERATIONS {
                    return Err(P3PsdError::NumericalConvergence {
                        operation: "P3 graupel-density fixed-point iteration",
                    });
                }
                let exponent = 1.0 / (RIMED_MASS_EXPONENT - UNRIMED_MASS_EXPONENT);
                let dense_limit =
                    (UNRIMED_MASS_COEFFICIENT / graupel_mass_coefficient).powf(exponent);
                let partial_coefficient = UNRIMED_MASS_COEFFICIENT / (1.0 - rime_mass_fraction);
                let partial_limit = (partial_coefficient / graupel_mass_coefficient).powf(exponent);
                let deposition_density = 6.0 * UNRIMED_MASS_COEFFICIENT
                    / (PI * (UNRIMED_MASS_EXPONENT - 2.0))
                    * (partial_limit.powf(UNRIMED_MASS_EXPONENT - 2.0)
                        - dense_limit.powf(UNRIMED_MASS_EXPONENT - 2.0))
                    / (partial_limit - dense_limit);
                let previous = graupel_mass_coefficient;
                graupel_mass_coefficient = rime_sphere_coefficient * rime_mass_fraction
                    + deposition_density * (1.0 - rime_mass_fraction) * PI / 6.0;
                if ((graupel_mass_coefficient - previous) / graupel_mass_coefficient).abs()
                    < RIME_DENSITY_ITERATION_TOLERANCE
                {
                    break (dense_limit, partial_limit, partial_coefficient);
                }
            };
            (
                dense_limit,
                partial_limit,
                partial_coefficient,
                UNRIMED_MASS_EXPONENT,
                iterations as u16,
            )
        };

        for (field, value) in [
            ("P3 small-sphere threshold", small_sphere_limit_m),
            (
                "P3 dense-unrimed/graupel threshold",
                dense_unrimed_to_graupel_m,
            ),
            (
                "P3 graupel/partially-rimed threshold",
                graupel_to_partially_rimed_m,
            ),
            ("P3 graupel mass coefficient", graupel_mass_coefficient),
            (
                "P3 partially-rimed mass coefficient",
                partially_rimed_mass_coefficient,
            ),
        ] {
            positive(field, value)?;
        }
        if dense_unrimed_to_graupel_m + f64::EPSILON < small_sphere_limit_m
            || graupel_to_partially_rimed_m + f64::EPSILON < dense_unrimed_to_graupel_m
        {
            return Err(P3PsdError::InvalidPiecewiseOrdering);
        }

        Ok(Self {
            rime_mass_fraction,
            rime_density_kg_m3: effective_rime_density,
            small_sphere_limit_m,
            dense_unrimed_to_graupel_m,
            graupel_to_partially_rimed_m,
            graupel_mass_coefficient,
            partially_rimed_mass_coefficient,
            partially_rimed_mass_exponent,
            rime_density_iterations,
        })
    }

    pub fn particle(&self, maximum_dimension_m: f64) -> Result<P3ParticleGeometry, P3PsdError> {
        positive("P3 particle maximum dimension", maximum_dimension_m)?;
        let (region, mass_kg, projected_area_m2) =
            if maximum_dimension_m <= self.small_sphere_limit_m {
                (
                    P3ParticleRegion::SmallDenseSphere,
                    PI / 6.0 * SOLID_ICE_DENSITY_KG_M3 * maximum_dimension_m.powi(3),
                    PI / 4.0 * maximum_dimension_m.powi(2),
                )
            } else if maximum_dimension_m <= self.dense_unrimed_to_graupel_m {
                (
                    P3ParticleRegion::DenseUnrimed,
                    UNRIMED_MASS_COEFFICIENT * maximum_dimension_m.powf(UNRIMED_MASS_EXPONENT),
                    UNRIMED_AREA_COEFFICIENT * maximum_dimension_m.powf(UNRIMED_AREA_EXPONENT),
                )
            } else if maximum_dimension_m <= self.graupel_to_partially_rimed_m {
                (
                    P3ParticleRegion::FullyRimedSphere,
                    self.graupel_mass_coefficient * maximum_dimension_m.powi(3),
                    PI / 4.0 * maximum_dimension_m.powi(2),
                )
            } else {
                let mass = self.partially_rimed_mass_coefficient
                    * maximum_dimension_m.powf(self.partially_rimed_mass_exponent);
                let unrimed_area =
                    UNRIMED_AREA_COEFFICIENT * maximum_dimension_m.powf(UNRIMED_AREA_EXPONENT);
                let graupel_area = PI / 4.0 * maximum_dimension_m.powi(2);
                let unrimed_mass =
                    UNRIMED_MASS_COEFFICIENT * maximum_dimension_m.powf(UNRIMED_MASS_EXPONENT);
                let graupel_mass = self.graupel_mass_coefficient * maximum_dimension_m.powi(3);
                let area = if self.rime_mass_fraction == 0.0 {
                    unrimed_area
                } else {
                    unrimed_area
                        + (mass - unrimed_mass) * (graupel_area - unrimed_area)
                            / (graupel_mass - unrimed_mass)
                };
                (P3ParticleRegion::PartiallyRimed, mass, area)
            };
        positive("P3 particle mass", mass_kg)?;
        positive("P3 particle projected area", projected_area_m2)?;
        Ok(P3ParticleGeometry {
            maximum_dimension_m,
            mass_kg,
            projected_area_m2,
            effective_spherical_density_kg_m3: mass_kg / (PI / 6.0 * maximum_dimension_m.powi(3)),
            region,
            shape_authority: P3ShapeAuthority::MaximumDimensionAndProjectedAreaOnly,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum P3ParticleRegion {
    SmallDenseSphere,
    DenseUnrimed,
    FullyRimedSphere,
    PartiallyRimed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum P3ShapeAuthority {
    /// WRF P3 provides maximum dimension and projected area, but not a unique
    /// spheroidal axis ratio/canting distribution for nonspherical particles.
    MaximumDimensionAndProjectedAreaOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct P3ParticleGeometry {
    pub maximum_dimension_m: f64,
    pub mass_kg: f64,
    pub projected_area_m2: f64,
    pub effective_spherical_density_kg_m3: f64,
    pub region: P3ParticleRegion,
    pub shape_authority: P3ShapeAuthority,
}

impl P3ParticleGeometry {
    /// True only where the pinned P3 particle law itself specifies a sphere.
    /// Dense-unrimed and partially-rimed particles retain only maximum
    /// dimension and projected area; treating them as spheroids would require
    /// an additional, non-scheme-native shape closure.
    #[must_use]
    pub const fn is_exact_sphere(self) -> bool {
        matches!(
            self.region,
            P3ParticleRegion::SmallDenseSphere | P3ParticleRegion::FullyRimedSphere
        )
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct P3PsdProvenance {
    pub revision: String,
    pub wrf_release: String,
    pub wrf_source_commit: String,
    pub p3_module_version: String,
    pub table: P3LookupTableDescriptor,
    pub number_limiter: P3NumberLimiterAudit,
    pub primary_source_dois: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct P3MomentClosureAudit {
    pub expected_number_density_m3: f64,
    pub reconstructed_number_density_m3: f64,
    pub number_relative_error: f64,
    pub expected_mass_concentration_kg_m3: f64,
    pub reconstructed_mass_concentration_kg_m3: f64,
    pub mass_relative_error: f64,
    pub expected_sixth_moment_m3: Option<f64>,
    pub reconstructed_sixth_moment_m3: f64,
    pub sixth_moment_relative_error: Option<f64>,
    pub expected_rime_mass_concentration_kg_m3: f64,
    pub reconstructed_rime_mass_concentration_kg_m3: f64,
    pub rime_mass_relative_error: f64,
    pub expected_rime_volume_concentration_m3_m3: f64,
    pub reconstructed_rime_volume_concentration_m3_m3: f64,
    pub rime_volume_relative_error: f64,
    pub table_axis_clamps: P3LookupAxisClamps,
}

/// Reconstructed gamma number distribution `N0 D^mu exp(-lambda D)`.
#[derive(Clone, Debug, PartialEq)]
pub struct P3Psd {
    input: P3PsdInput,
    lambda_m_inv: f64,
    mu: f64,
    n0_intercept_si: f64,
    law: P3PiecewiseParticleLaw,
    closure: P3MomentClosureAudit,
    provenance: P3PsdProvenance,
}

impl P3Psd {
    pub fn reconstruct(
        input: P3PsdInput,
        table: &dyn P3LookupTableV54,
        config: P3ReconstructionConfig,
    ) -> Result<Self, P3PsdError> {
        validate_input(input)?;
        fraction_open(
            "P3 reconstruction tolerance",
            config.maximum_moment_relative_error,
        )?;
        table.descriptor().validate_for(input.scheme)?;

        let rime_fraction = input.rime_mass_kgkg / input.total_ice_kgkg;
        let rime_density = if input.rime_mass_kgkg > 0.0 {
            wrf_real_rime_density_kg_m3(input.rime_mass_kgkg, input.rime_volume_m3_per_kg)
                .map_err(P3PsdError::Lookup)?
        } else {
            // `calc_bulkRhoRime` returns zero for unrimed state; WRF's table
            // indexer then applies the documented lower density-axis clamp.
            0.0
        };
        let number_after_nsmall =
            P3NumberLimiterAudit::number_after_nsmall_wrf_real(input.total_number_per_kg)
                .map_err(P3PsdError::Lookup)?;
        let sixth_moment_per_kg = match input.moment {
            P3IceMomentInput::TwoMoment => None,
            P3IceMomentInput::WrfAdvectedQzi {
                qzi_sqrt_n_times_m6,
            } => Some(qzi_sqrt_n_times_m6.powi(2) / f64::from(number_after_nsmall)),
        };
        let query = P3LookupQuery {
            scheme: input.scheme,
            category: input.category,
            rime_mass_fraction: rime_fraction,
            rime_density_kg_m3: rime_density,
            total_number_per_kg: input.total_number_per_kg,
            total_ice_kgkg: input.total_ice_kgkg,
            sixth_moment_per_kg,
        };
        let solution = table.lookup_psd(query).map_err(P3PsdError::Lookup)?;
        positive("P3 lambda", solution.slope_lambda_m_inv)?;
        in_range("P3 mu", solution.shape_mu, P3_MU_RANGE)?;
        let number_limiter = P3NumberLimiterAudit::from_wrf_real(
            input.total_number_per_kg,
            input.total_ice_kgkg,
            solution.inverse_qmin_per_kg,
            solution.inverse_qmax_per_kg,
        )
        .map_err(P3PsdError::Lookup)?;
        if input.scheme == P3WrfScheme::Mp53OneIceTripleMoment
            && (number_limiter.nsmall_applied
                || number_limiter.maximum_applied
                || number_limiter.minimum_applied)
        {
            return Err(P3PsdError::TripleMomentNumberRepairRequiresZiLimiter {
                original_number_per_kg: number_limiter.original_total_number_per_kg,
                repaired_number_per_kg: number_limiter.repaired_total_number_per_kg,
            });
        }

        let law = P3PiecewiseParticleLaw::reconstruct(rime_fraction, rime_density)?;
        let repaired_number_per_kg = number_limiter.repaired_total_number_per_kg;
        let number_density = repaired_number_per_kg * input.dry_air_density_kg_m3;
        let expected_mass = input.total_ice_kgkg * input.dry_air_density_kg_m3;
        // The pinned lookup-table generator deliberately derives final N0
        // from Q and the piecewise mass integral, rather than from N, because
        // WRF may subsequently adjust N to enforce its mean-size bounds. Do
        // the same here. Interpolated lambda/mu plus repaired Ni need not close
        // number exactly between the coarse table nodes, but QICE remains the
        // authoritative total condensed mass for the scattering population.
        let unit_intercept_mass = integrate_piecewise_mass(
            1.0,
            solution.slope_lambda_m_inv,
            solution.shape_mu,
            law,
            0.0,
            f64::INFINITY,
        )?;
        positive("P3 unit-intercept mass integral", unit_intercept_mass)?;
        let n0_intercept_si = expected_mass / unit_intercept_mass;
        positive("P3 gamma intercept N0", n0_intercept_si)?;

        let reconstructed_number = gamma_raw_moment(
            n0_intercept_si,
            solution.slope_lambda_m_inv,
            solution.shape_mu,
            0.0,
        )?;
        let reconstructed_mass = integrate_piecewise_mass(
            n0_intercept_si,
            solution.slope_lambda_m_inv,
            solution.shape_mu,
            law,
            0.0,
            f64::INFINITY,
        )?;
        let reconstructed_m6 = gamma_raw_moment(
            n0_intercept_si,
            solution.slope_lambda_m_inv,
            solution.shape_mu,
            6.0,
        )?;
        let expected_m6 = sixth_moment_per_kg.map(|moment| moment * input.dry_air_density_kg_m3);
        let number_error = relative_error(reconstructed_number, number_density);
        let mass_error = relative_error(reconstructed_mass, expected_mass);
        let sixth_error = expected_m6.map(|expected| relative_error(reconstructed_m6, expected));
        if mass_error > config.maximum_moment_relative_error {
            return Err(P3PsdError::MomentClosure {
                moment: "mass",
                relative_error: mass_error,
                maximum: config.maximum_moment_relative_error,
            });
        }
        let expected_rime_mass = input.rime_mass_kgkg * input.dry_air_density_kg_m3;
        let reconstructed_rime_mass = reconstructed_mass * rime_fraction;
        let expected_rime_volume = input.rime_volume_m3_per_kg * input.dry_air_density_kg_m3;
        let reconstructed_rime_volume = if input.rime_mass_kgkg > 0.0 {
            reconstructed_rime_mass / rime_density
        } else {
            0.0
        };
        let mut primary_source_dois = vec![P3_PART_I_DOI.to_owned()];
        if input.scheme == P3WrfScheme::Mp52TwoIcePredictedCloudNumber {
            primary_source_dois.push(P3_MULTICATEGORY_DOI.to_owned());
        }
        if input.scheme == P3WrfScheme::Mp53OneIceTripleMoment {
            primary_source_dois.push(P3_TRIPLE_MOMENT_CONTEXT_DOI.to_owned());
        }
        let closure = P3MomentClosureAudit {
            expected_number_density_m3: number_density,
            reconstructed_number_density_m3: reconstructed_number,
            number_relative_error: number_error,
            expected_mass_concentration_kg_m3: expected_mass,
            reconstructed_mass_concentration_kg_m3: reconstructed_mass,
            mass_relative_error: mass_error,
            expected_sixth_moment_m3: expected_m6,
            reconstructed_sixth_moment_m3: reconstructed_m6,
            sixth_moment_relative_error: sixth_error,
            expected_rime_mass_concentration_kg_m3: expected_rime_mass,
            reconstructed_rime_mass_concentration_kg_m3: reconstructed_rime_mass,
            rime_mass_relative_error: relative_error(reconstructed_rime_mass, expected_rime_mass),
            expected_rime_volume_concentration_m3_m3: expected_rime_volume,
            reconstructed_rime_volume_concentration_m3_m3: reconstructed_rime_volume,
            rime_volume_relative_error: relative_error(
                reconstructed_rime_volume,
                expected_rime_volume,
            ),
            table_axis_clamps: solution.axis_clamps,
        };
        Ok(Self {
            input,
            lambda_m_inv: solution.slope_lambda_m_inv,
            mu: solution.shape_mu,
            n0_intercept_si,
            law,
            closure,
            provenance: P3PsdProvenance {
                revision: P3_PSD_REVISION.to_owned(),
                wrf_release: P3_WRF_RELEASE.to_owned(),
                wrf_source_commit: P3_WRF_SOURCE_COMMIT.to_owned(),
                p3_module_version: P3_MODULE_VERSION.to_owned(),
                table: table.descriptor().clone(),
                number_limiter,
                primary_source_dois,
            },
        })
    }

    #[must_use]
    pub const fn input(&self) -> P3PsdInput {
        self.input
    }

    #[must_use]
    pub const fn lambda_m_inv(&self) -> f64 {
        self.lambda_m_inv
    }

    #[must_use]
    pub const fn mu(&self) -> f64 {
        self.mu
    }

    /// Gamma intercept in SI units, whose length exponent depends on `mu`.
    #[must_use]
    pub const fn n0_intercept_si(&self) -> f64 {
        self.n0_intercept_si
    }

    #[must_use]
    pub const fn particle_law(&self) -> P3PiecewiseParticleLaw {
        self.law
    }

    #[must_use]
    pub const fn closure_audit(&self) -> P3MomentClosureAudit {
        self.closure
    }

    #[must_use]
    pub fn provenance(&self) -> &P3PsdProvenance {
        &self.provenance
    }

    #[must_use]
    pub const fn number_limiter_audit(&self) -> P3NumberLimiterAudit {
        self.provenance.number_limiter
    }

    pub fn quadrature(&self, config: P3QuadratureConfig) -> Result<P3Quadrature, P3PsdError> {
        self.quadrature_with_dimension_breakpoints(config, &[])
    }

    /// Build the same exact P3 quadrature while forcing additional physical
    /// maximum-dimension boundaries. This lets a downstream, fail-closed
    /// scattering integrator align panels to its table envelope without
    /// clipping or moving any particle onto a LUT edge.
    pub fn quadrature_with_dimension_breakpoints(
        &self,
        config: P3QuadratureConfig,
        additional_breakpoints_m: &[f64],
    ) -> Result<P3Quadrature, P3PsdError> {
        config.validate()?;
        for &breakpoint in additional_breakpoints_m {
            positive("P3 additional quadrature breakpoint", breakpoint)?;
        }
        let upper_m = self.tail_cutoff(config.maximum_tail_fraction, config.maximum_scaled_d)?;
        let required_base_nodes = usize::from(config.panels)
            .checked_mul(GL8_POINTS)
            .ok_or(P3PsdError::NodeBudgetOverflow)?;
        if required_base_nodes > config.maximum_nodes as usize {
            return Err(P3PsdError::NodeBudgetExceeded {
                required: required_base_nodes,
                maximum: config.maximum_nodes as usize,
            });
        }

        let mut breakpoints = (0..=usize::from(config.panels))
            .map(|index| upper_m * index as f64 / f64::from(config.panels))
            .collect::<Vec<_>>();
        for threshold in [
            self.law.small_sphere_limit_m,
            self.law.dense_unrimed_to_graupel_m,
            self.law.graupel_to_partially_rimed_m,
        ] {
            if threshold > 0.0 && threshold < upper_m {
                breakpoints.push(threshold);
            }
        }
        for &breakpoint in additional_breakpoints_m {
            if breakpoint < upper_m {
                breakpoints.push(breakpoint);
            }
        }
        breakpoints.sort_by(f64::total_cmp);
        breakpoints.dedup_by(|left, right| {
            (*left - *right).abs() <= 32.0 * f64::EPSILON * left.abs().max(right.abs()).max(1.0)
        });
        let required_nodes = breakpoints
            .len()
            .saturating_sub(1)
            .checked_mul(GL8_POINTS)
            .ok_or(P3PsdError::NodeBudgetOverflow)?;
        if required_nodes > config.maximum_nodes as usize {
            return Err(P3PsdError::NodeBudgetExceeded {
                required: required_nodes,
                maximum: config.maximum_nodes as usize,
            });
        }

        let mut nodes = Vec::with_capacity(required_nodes);
        let mut integrated_number = 0.0;
        let mut integrated_mass = 0.0;
        let mut integrated_m6 = 0.0;
        for segment in breakpoints.windows(2) {
            let half_width = 0.5 * (segment[1] - segment[0]);
            let midpoint = 0.5 * (segment[1] + segment[0]);
            for local in 0..GL8_POINTS {
                let diameter = midpoint + half_width * GL8_ABSCISSAE[local];
                let integration_width = half_width * GL8_WEIGHTS[local];
                let number_weight = self.number_density_per_m(diameter)? * integration_width;
                let particle = self.law.particle(diameter)?;
                integrated_number += number_weight;
                integrated_mass += number_weight * particle.mass_kg;
                integrated_m6 += number_weight * diameter.powi(6);
                nodes.push(P3QuadratureNode {
                    maximum_dimension_m: diameter,
                    number_concentration_m3: number_weight,
                    particle,
                });
            }
        }

        let total_number = self.closure.reconstructed_number_density_m3;
        let total_mass = self.closure.reconstructed_mass_concentration_kg_m3;
        let total_m6 = self.closure.reconstructed_sixth_moment_m3;
        let tail_number = gamma_raw_moment_interval(
            self.n0_intercept_si,
            self.lambda_m_inv,
            self.mu,
            0.0,
            upper_m,
            f64::INFINITY,
        )?;
        let tail_mass = integrate_piecewise_mass(
            self.n0_intercept_si,
            self.lambda_m_inv,
            self.mu,
            self.law,
            upper_m,
            f64::INFINITY,
        )?;
        let tail_m6 = gamma_raw_moment_interval(
            self.n0_intercept_si,
            self.lambda_m_inv,
            self.mu,
            6.0,
            upper_m,
            f64::INFINITY,
        )?;
        let number_error = relative_error(integrated_number, total_number - tail_number);
        let mass_error = relative_error(integrated_mass, total_mass - tail_mass);
        let m6_error = relative_error(integrated_m6, total_m6 - tail_m6);
        for (moment, error) in [
            ("number", number_error),
            ("mass", mass_error),
            ("sixth", m6_error),
        ] {
            if error > config.maximum_quadrature_relative_error {
                return Err(P3PsdError::QuadratureClosure {
                    moment,
                    relative_error: error,
                    maximum: config.maximum_quadrature_relative_error,
                });
            }
        }
        Ok(P3Quadrature {
            nodes,
            audit: P3QuadratureAudit {
                config,
                upper_dimension_m: upper_m,
                nodes_evaluated: required_nodes,
                omission: P3OmissionTailAudit {
                    lower_domain_omitted_number_fraction: 0.0,
                    lower_domain_omitted_mass_fraction: 0.0,
                    lower_domain_omitted_sixth_moment_fraction: 0.0,
                    upper_tail_number_fraction: tail_number / total_number,
                    upper_tail_mass_fraction: tail_mass / total_mass,
                    upper_tail_sixth_moment_fraction: tail_m6 / total_m6,
                },
                represented_number_density_m3: integrated_number,
                represented_mass_concentration_kg_m3: integrated_mass,
                represented_sixth_moment_m3: integrated_m6,
                number_quadrature_relative_error: number_error,
                mass_quadrature_relative_error: mass_error,
                sixth_moment_quadrature_relative_error: m6_error,
            },
        })
    }

    fn number_density_per_m(&self, diameter_m: f64) -> Result<f64, P3PsdError> {
        positive("P3 quadrature diameter", diameter_m)?;
        let log_value =
            self.n0_intercept_si.ln() + self.mu * diameter_m.ln() - self.lambda_m_inv * diameter_m;
        let value = log_value.exp();
        if value.is_finite() && value >= 0.0 {
            Ok(value)
        } else {
            Err(P3PsdError::InvalidComputation {
                field: "P3 gamma number density",
                value,
            })
        }
    }

    fn tail_cutoff(&self, maximum_tail: f64, maximum_scaled_d: f64) -> Result<f64, P3PsdError> {
        let exceeds = |diameter: f64| -> Result<bool, P3PsdError> {
            let number_tail = gamma_raw_moment_interval(
                self.n0_intercept_si,
                self.lambda_m_inv,
                self.mu,
                0.0,
                diameter,
                f64::INFINITY,
            )? / self.closure.reconstructed_number_density_m3;
            let mass_tail = integrate_piecewise_mass(
                self.n0_intercept_si,
                self.lambda_m_inv,
                self.mu,
                self.law,
                diameter,
                f64::INFINITY,
            )? / self.closure.reconstructed_mass_concentration_kg_m3;
            let m6_tail = gamma_raw_moment_interval(
                self.n0_intercept_si,
                self.lambda_m_inv,
                self.mu,
                6.0,
                diameter,
                f64::INFINITY,
            )? / self.closure.reconstructed_sixth_moment_m3;
            Ok(number_tail > maximum_tail || mass_tail > maximum_tail || m6_tail > maximum_tail)
        };
        let maximum_d = maximum_scaled_d / self.lambda_m_inv;
        let mut upper = (self.mu + 7.0).max(1.0) / self.lambda_m_inv;
        while upper < maximum_d && exceeds(upper)? {
            upper = (upper * 2.0).min(maximum_d);
        }
        if exceeds(upper)? {
            return Err(P3PsdError::TailToleranceUnreachable {
                maximum_scaled_d,
                requested_fraction: maximum_tail,
            });
        }
        let mut lower = 0.0;
        for _ in 0..80 {
            let midpoint = 0.5 * (lower + upper);
            if exceeds(midpoint)? {
                lower = midpoint;
            } else {
                upper = midpoint;
            }
        }
        Ok(upper)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct P3QuadratureConfig {
    pub panels: u16,
    pub maximum_nodes: u32,
    pub maximum_tail_fraction: f64,
    pub maximum_scaled_d: f64,
    pub maximum_quadrature_relative_error: f64,
}

impl Default for P3QuadratureConfig {
    fn default() -> Self {
        Self {
            panels: 64,
            maximum_nodes: 2_048,
            maximum_tail_fraction: 1.0e-10,
            maximum_scaled_d: 512.0,
            maximum_quadrature_relative_error: 2.0e-7,
        }
    }
}

impl P3QuadratureConfig {
    fn validate(self) -> Result<(), P3PsdError> {
        if self.panels == 0 {
            return Err(P3PsdError::InvalidIntegerConfig {
                field: "P3 quadrature panels",
                value: 0,
            });
        }
        if self.maximum_nodes == 0 {
            return Err(P3PsdError::InvalidIntegerConfig {
                field: "P3 quadrature maximum nodes",
                value: 0,
            });
        }
        fraction_open("P3 maximum tail fraction", self.maximum_tail_fraction)?;
        positive("P3 maximum scaled diameter", self.maximum_scaled_d)?;
        fraction_open(
            "P3 quadrature relative tolerance",
            self.maximum_quadrature_relative_error,
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct P3QuadratureNode {
    pub maximum_dimension_m: f64,
    /// Integrated population carried by this quadrature node, # m^-3.
    pub number_concentration_m3: f64,
    pub particle: P3ParticleGeometry,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct P3OmissionTailAudit {
    pub lower_domain_omitted_number_fraction: f64,
    pub lower_domain_omitted_mass_fraction: f64,
    pub lower_domain_omitted_sixth_moment_fraction: f64,
    pub upper_tail_number_fraction: f64,
    pub upper_tail_mass_fraction: f64,
    pub upper_tail_sixth_moment_fraction: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct P3QuadratureAudit {
    pub config: P3QuadratureConfig,
    pub upper_dimension_m: f64,
    pub nodes_evaluated: usize,
    pub omission: P3OmissionTailAudit,
    pub represented_number_density_m3: f64,
    pub represented_mass_concentration_kg_m3: f64,
    pub represented_sixth_moment_m3: f64,
    pub number_quadrature_relative_error: f64,
    pub mass_quadrature_relative_error: f64,
    pub sixth_moment_quadrature_relative_error: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct P3Quadrature {
    pub nodes: Vec<P3QuadratureNode>,
    pub audit: P3QuadratureAudit,
}

#[derive(Debug, Error)]
pub enum P3PsdError {
    #[error("WRF mp_physics={mp_physics} is not a supported P3 configuration")]
    UnsupportedScheme { mp_physics: i32 },
    #[error("P3 category {category:?} is unavailable for WRF mp_physics={mp_physics}")]
    CategoryUnavailable {
        mp_physics: i32,
        category: P3Category,
    },
    #[error("WRF mp_physics={mp_physics} requires {required:?}, got {actual:?}")]
    MomentOrderMismatch {
        mp_physics: i32,
        required: P3IceMomentOrder,
        actual: P3IceMomentOrder,
    },
    #[error("{field} must be {requirement}, got {value}")]
    InvalidInput {
        field: &'static str,
        value: f64,
        requirement: &'static str,
    },
    #[error("{field} value {value} is outside [{minimum}, {maximum}]")]
    OutsideRange {
        field: &'static str,
        value: f64,
        minimum: f64,
        maximum: f64,
    },
    #[error("rime mass and volume must both be zero or both be positive")]
    InconsistentRimeState,
    #[error("P3 rime mass {rime_mass} exceeds total ice mass {total_mass}")]
    RimeMassExceedsTotal { rime_mass: f64, total_mass: f64 },
    #[error("P3 piecewise particle thresholds are not ordered")]
    InvalidPiecewiseOrdering,
    #[error("P3 lookup {field} mismatch: expected '{expected}', got '{actual}'")]
    LookupRevisionMismatch {
        field: &'static str,
        expected: String,
        actual: String,
    },
    #[error("P3 lookup SHA-256 mismatch: expected {expected}, got {actual}")]
    LookupDigestMismatch { expected: String, actual: String },
    #[error("P3 official-table lookup failed: {0}")]
    Lookup(#[source] P3LookupFailure),
    #[error(
        "P3 triple-moment number repair from {original_number_per_kg} to {repaired_number_per_kg} kg-1 requires the coupled WRF Zi limiter"
    )]
    TripleMomentNumberRepairRequiresZiLimiter {
        original_number_per_kg: f64,
        repaired_number_per_kg: f64,
    },
    #[error("{field} produced invalid value {value}")]
    InvalidComputation { field: &'static str, value: f64 },
    #[error("{operation} did not converge")]
    NumericalConvergence { operation: &'static str },
    #[error("P3 {moment} reconstruction error {relative_error} exceeds {maximum}")]
    MomentClosure {
        moment: &'static str,
        relative_error: f64,
        maximum: f64,
    },
    #[error("quadrature node-budget arithmetic overflowed")]
    NodeBudgetOverflow,
    #[error("P3 quadrature needs {required} nodes but maximum is {maximum}")]
    NodeBudgetExceeded { required: usize, maximum: usize },
    #[error(
        "P3 gamma tails cannot reach {requested_fraction} by scaled diameter {maximum_scaled_d}"
    )]
    TailToleranceUnreachable {
        maximum_scaled_d: f64,
        requested_fraction: f64,
    },
    #[error("P3 {moment} quadrature error {relative_error} exceeds {maximum}")]
    QuadratureClosure {
        moment: &'static str,
        relative_error: f64,
        maximum: f64,
    },
    #[error("{field} must be positive, got {value}")]
    InvalidIntegerConfig { field: &'static str, value: u64 },
}

fn validate_input(input: P3PsdInput) -> Result<(), P3PsdError> {
    positive("P3 total ice mixing ratio", input.total_ice_kgkg)?;
    let total_ice_wrf_real = input.total_ice_kgkg as f32;
    if !total_ice_wrf_real.is_finite() || total_ice_wrf_real < P3_WRF_QSMALL_KGKG {
        return Err(P3PsdError::InvalidInput {
            field: "P3 total ice mixing ratio",
            value: input.total_ice_kgkg,
            requirement: "representable as WRF REAL and at least P3 qsmall=1e-14 kg kg-1",
        });
    }
    if !input.total_number_per_kg.is_finite() {
        return Err(P3PsdError::InvalidInput {
            field: "P3 total number mixing ratio",
            value: input.total_number_per_kg,
            requirement: "finite; P3 repairs finite leading-edge values",
        });
    }
    nonnegative("P3 rime mass mixing ratio", input.rime_mass_kgkg)?;
    nonnegative("P3 rime volume mixing ratio", input.rime_volume_m3_per_kg)?;
    positive("P3 dry-air density", input.dry_air_density_kg_m3)?;
    if input.rime_mass_kgkg > input.total_ice_kgkg {
        return Err(P3PsdError::RimeMassExceedsTotal {
            rime_mass: input.rime_mass_kgkg,
            total_mass: input.total_ice_kgkg,
        });
    }
    if (input.rime_mass_kgkg == 0.0) != (input.rime_volume_m3_per_kg == 0.0) {
        return Err(P3PsdError::InconsistentRimeState);
    }
    if input.category == P3Category::Category2 && input.scheme.category_count() != 2 {
        return Err(P3PsdError::CategoryUnavailable {
            mp_physics: input.scheme.mp_physics(),
            category: input.category,
        });
    }
    let actual_moment = match input.moment {
        P3IceMomentInput::TwoMoment => P3IceMomentOrder::TwoMoment,
        P3IceMomentInput::WrfAdvectedQzi {
            qzi_sqrt_n_times_m6,
        } => {
            positive("P3 WRF-advected QZI", qzi_sqrt_n_times_m6)?;
            P3IceMomentOrder::TripleMomentQzi
        }
    };
    let required = input.scheme.moment_order();
    if actual_moment != required {
        return Err(P3PsdError::MomentOrderMismatch {
            mp_physics: input.scheme.mp_physics(),
            required,
            actual: actual_moment,
        });
    }
    Ok(())
}

fn integrate_piecewise_mass(
    n0: f64,
    lambda: f64,
    mu: f64,
    law: P3PiecewiseParticleLaw,
    lower: f64,
    upper: f64,
) -> Result<f64, P3PsdError> {
    let regions = [
        (
            0.0,
            law.small_sphere_limit_m,
            PI / 6.0 * SOLID_ICE_DENSITY_KG_M3,
            3.0,
        ),
        (
            law.small_sphere_limit_m,
            law.dense_unrimed_to_graupel_m,
            UNRIMED_MASS_COEFFICIENT,
            UNRIMED_MASS_EXPONENT,
        ),
        (
            law.dense_unrimed_to_graupel_m,
            law.graupel_to_partially_rimed_m,
            law.graupel_mass_coefficient,
            3.0,
        ),
        (
            law.graupel_to_partially_rimed_m,
            f64::INFINITY,
            law.partially_rimed_mass_coefficient,
            law.partially_rimed_mass_exponent,
        ),
    ];
    let mut total = 0.0;
    for (region_lower, region_upper, coefficient, exponent) in regions {
        let start = lower.max(region_lower);
        let end = upper.min(region_upper);
        if end > start {
            total += coefficient * gamma_raw_moment_interval(n0, lambda, mu, exponent, start, end)?;
        }
    }
    if total.is_finite() && total >= 0.0 {
        Ok(total)
    } else {
        Err(P3PsdError::InvalidComputation {
            field: "P3 piecewise mass integral",
            value: total,
        })
    }
}

fn gamma_raw_moment(n0: f64, lambda: f64, mu: f64, power: f64) -> Result<f64, P3PsdError> {
    gamma_raw_moment_interval(n0, lambda, mu, power, 0.0, f64::INFINITY)
}

fn gamma_raw_moment_interval(
    n0: f64,
    lambda: f64,
    mu: f64,
    power: f64,
    lower: f64,
    upper: f64,
) -> Result<f64, P3PsdError> {
    let shape = mu + power + 1.0;
    positive("P3 gamma moment shape", shape)?;
    let lower_q = regularized_gamma_q(shape, lambda * lower)?;
    let upper_q = if upper.is_infinite() {
        0.0
    } else {
        regularized_gamma_q(shape, lambda * upper)?
    };
    let fraction = (lower_q - upper_q).max(0.0);
    let value = (n0.ln() + ln_gamma(shape)? - shape * lambda.ln()).exp() * fraction;
    if value.is_finite() && value >= 0.0 {
        Ok(value)
    } else {
        Err(P3PsdError::InvalidComputation {
            field: "P3 analytic gamma moment",
            value,
        })
    }
}

fn regularized_gamma_q(shape: f64, x: f64) -> Result<f64, P3PsdError> {
    positive("regularized-gamma shape", shape)?;
    if !x.is_finite() || x < 0.0 {
        return Err(P3PsdError::InvalidInput {
            field: "regularized-gamma coordinate",
            value: x,
            requirement: "finite and nonnegative",
        });
    }
    if x == 0.0 {
        return Ok(1.0);
    }
    let log_prefactor = -x + shape * x.ln() - ln_gamma(shape)?;
    let q = if x < shape + 1.0 {
        let mut ap = shape;
        let mut term = 1.0 / shape;
        let mut sum = term;
        let mut converged = false;
        for _ in 0..NUMERICAL_MAX_ITERATIONS {
            ap += 1.0;
            term *= x / ap;
            sum += term;
            if term.abs() <= sum.abs() * NUMERICAL_EPSILON {
                converged = true;
                break;
            }
        }
        if !converged {
            return Err(P3PsdError::NumericalConvergence {
                operation: "regularized lower incomplete gamma series",
            });
        }
        1.0 - sum * log_prefactor.exp()
    } else {
        let mut b = x + 1.0 - shape;
        let mut c = 1.0 / NUMERICAL_FLOOR;
        let mut d = 1.0 / b.max(NUMERICAL_FLOOR);
        let mut h = d;
        let mut converged = false;
        for iteration in 1..=NUMERICAL_MAX_ITERATIONS {
            let i = iteration as f64;
            let an = -i * (i - shape);
            b += 2.0;
            d = an * d + b;
            if d.abs() < NUMERICAL_FLOOR {
                d = NUMERICAL_FLOOR;
            }
            c = b + an / c;
            if c.abs() < NUMERICAL_FLOOR {
                c = NUMERICAL_FLOOR;
            }
            d = 1.0 / d;
            let delta = d * c;
            h *= delta;
            if (delta - 1.0).abs() <= NUMERICAL_EPSILON {
                converged = true;
                break;
            }
        }
        if !converged {
            return Err(P3PsdError::NumericalConvergence {
                operation: "regularized upper incomplete gamma fraction",
            });
        }
        log_prefactor.exp() * h
    };
    if q.is_finite() && (-1.0e-13..=1.0 + 1.0e-13).contains(&q) {
        Ok(q.clamp(0.0, 1.0))
    } else {
        Err(P3PsdError::InvalidComputation {
            field: "regularized upper incomplete gamma",
            value: q,
        })
    }
}

fn ln_gamma(value: f64) -> Result<f64, P3PsdError> {
    positive("gamma argument", value)?;
    const COEFFICIENTS: [f64; 8] = [
        676.520_368_121_885_1,
        -1_259.139_216_722_402_8,
        771.323_428_777_653_1,
        -176.615_029_162_140_6,
        12.507_343_278_686_905,
        -0.138_571_095_265_720_12,
        9.984_369_578_019_572e-6,
        1.505_632_735_149_311_6e-7,
    ];
    let result = if value < 0.5 {
        PI.ln() - (PI * value).sin().ln() - ln_gamma(1.0 - value)?
    } else {
        let z = value - 1.0;
        let mut series = 0.999_999_999_999_809_9;
        for (index, coefficient) in COEFFICIENTS.into_iter().enumerate() {
            series += coefficient / (z + index as f64 + 1.0);
        }
        let t = z + 7.5;
        0.5 * (2.0 * PI).ln() + (z + 0.5) * t.ln() - t + series.ln()
    };
    if result.is_finite() {
        Ok(result)
    } else {
        Err(P3PsdError::InvalidComputation {
            field: "log gamma",
            value: result,
        })
    }
}

fn positive(field: &'static str, value: f64) -> Result<(), P3PsdError> {
    if value.is_finite() && value > 0.0 {
        Ok(())
    } else {
        Err(P3PsdError::InvalidInput {
            field,
            value,
            requirement: "finite and positive",
        })
    }
}

fn wrf_real(field: &str, value: f64) -> Result<f32, P3LookupFailure> {
    let narrowed = value as f32;
    if value.is_finite() && narrowed.is_finite() {
        Ok(narrowed)
    } else {
        Err(P3LookupFailure::OutsideDomain(format!(
            "{field} cannot be represented as finite WRF REAL: {value}"
        )))
    }
}

fn wrf_real_rime_density_kg_m3(
    rime_mass_kgkg: f64,
    rime_volume_m3_per_kg: f64,
) -> Result<f64, P3LookupFailure> {
    let mass = wrf_real("P3 rime mass", rime_mass_kgkg)?;
    let volume = wrf_real("P3 rime volume", rime_volume_m3_per_kg)?;
    if mass <= 0.0 || volume <= 0.0 {
        return Err(P3LookupFailure::OutsideDomain(format!(
            "positive P3 rime mass requires positive rime volume, got mass={mass}, volume={volume}"
        )));
    }
    // `calc_bulkRhoRime` performs this division and bound in default REAL.
    // Widening the independently stored f32 tuple before division can move an
    // exact 50/900 kg m-3 boundary a few ulps outside the accepted interval.
    let density = (mass / volume).clamp(
        P3_RIME_DENSITY_RANGE_KG_M3[0] as f32,
        P3_RIME_DENSITY_RANGE_KG_M3[1] as f32,
    );
    Ok(f64::from(density))
}

fn nonnegative(field: &'static str, value: f64) -> Result<(), P3PsdError> {
    if value.is_finite() && value >= 0.0 {
        Ok(())
    } else {
        Err(P3PsdError::InvalidInput {
            field,
            value,
            requirement: "finite and nonnegative",
        })
    }
}

fn fraction(field: &'static str, value: f64) -> Result<(), P3PsdError> {
    if value.is_finite() && (0.0..=1.0).contains(&value) {
        Ok(())
    } else {
        Err(P3PsdError::InvalidInput {
            field,
            value,
            requirement: "within [0, 1]",
        })
    }
}

fn fraction_open(field: &'static str, value: f64) -> Result<(), P3PsdError> {
    if value.is_finite() && (0.0..1.0).contains(&value) {
        Ok(())
    } else {
        Err(P3PsdError::InvalidInput {
            field,
            value,
            requirement: "within (0, 1)",
        })
    }
}

fn in_range(field: &'static str, value: f64, range: [f64; 2]) -> Result<(), P3PsdError> {
    if value.is_finite() && (range[0]..=range[1]).contains(&value) {
        Ok(())
    } else {
        Err(P3PsdError::OutsideRange {
            field,
            value,
            minimum: range[0],
            maximum: range[1],
        })
    }
}

fn relative_error(actual: f64, expected: f64) -> f64 {
    (actual - expected).abs() / expected.abs().max(f64::MIN_POSITIVE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct ExactFixtureTable {
        descriptor: P3LookupTableDescriptor,
        lambda: f64,
        mu: f64,
        inverse_qmin: f64,
        inverse_qmax: f64,
    }

    impl P3LookupTableV54 for ExactFixtureTable {
        fn descriptor(&self) -> &P3LookupTableDescriptor {
            &self.descriptor
        }

        fn lookup_psd(&self, _query: P3LookupQuery) -> Result<P3LookupSolution, P3LookupFailure> {
            Ok(P3LookupSolution {
                slope_lambda_m_inv: self.lambda,
                shape_mu: self.mu,
                axis_clamps: P3LookupAxisClamps::default(),
                inverse_qmin_per_kg: self.inverse_qmin,
                inverse_qmax_per_kg: self.inverse_qmax,
            })
        }
    }

    fn fixture_table(scheme: P3WrfScheme, lambda: f64, mu: f64) -> ExactFixtureTable {
        ExactFixtureTable {
            descriptor: P3LookupTableDescriptor {
                wrf_source_commit: P3_WRF_SOURCE_COMMIT.to_owned(),
                p3_module_version: P3_MODULE_VERSION.to_owned(),
                generator_version: P3_TABLE_GENERATOR_VERSION.to_owned(),
                table_version: scheme.required_table_version().to_owned(),
                table_sha256: Sha256Digest::from_hex(scheme.required_table_sha256()).unwrap(),
            },
            lambda,
            mu,
            inverse_qmin: 1.0e30,
            inverse_qmax: 1.0e-30,
        }
    }

    fn state_from_exact_distribution(
        scheme: P3WrfScheme,
        lambda: f64,
        mu: f64,
        number_per_kg: f64,
        density: f64,
    ) -> P3PsdInput {
        let law = P3PiecewiseParticleLaw::reconstruct(0.0, 50.0).unwrap();
        let number_density = number_per_kg * density;
        let n0 =
            (number_density.ln() + (mu + 1.0) * lambda.ln() - ln_gamma(mu + 1.0).unwrap()).exp();
        let mass = integrate_piecewise_mass(n0, lambda, mu, law, 0.0, f64::INFINITY).unwrap();
        if scheme == P3WrfScheme::Mp53OneIceTripleMoment {
            let m6_volume = gamma_raw_moment(n0, lambda, mu, 6.0).unwrap();
            let m6_per_kg = m6_volume / density;
            P3PsdInput::triple_moment_qzi(
                mass / density,
                number_per_kg,
                0.0,
                0.0,
                (number_per_kg * m6_per_kg).sqrt(),
                density,
            )
        } else {
            P3PsdInput::two_moment(
                scheme,
                P3Category::Category1,
                mass / density,
                number_per_kg,
                0.0,
                0.0,
                density,
            )
        }
    }

    #[test]
    fn wrf_scheme_variants_keep_category_and_qzi_contracts_distinct() {
        assert_eq!(P3WrfScheme::Mp50OneIceFixedCloudNumber.category_count(), 1);
        assert_eq!(
            P3WrfScheme::Mp51OneIcePredictedCloudNumber.category_count(),
            1
        );
        assert_eq!(
            P3WrfScheme::Mp52TwoIcePredictedCloudNumber.category_count(),
            2
        );
        assert_eq!(
            P3WrfScheme::Mp53OneIceTripleMoment.moment_order(),
            P3IceMomentOrder::TripleMomentQzi
        );

        let mut wrong = state_from_exact_distribution(
            P3WrfScheme::Mp50OneIceFixedCloudNumber,
            1.0e5,
            2.0,
            1.0e5,
            1.0,
        );
        wrong.category = P3Category::Category2;
        assert!(matches!(
            validate_input(wrong),
            Err(P3PsdError::CategoryUnavailable { .. })
        ));
        wrong.category = P3Category::Category1;
        wrong.moment = P3IceMomentInput::WrfAdvectedQzi {
            qzi_sqrt_n_times_m6: 1.0,
        };
        assert!(matches!(
            validate_input(wrong),
            Err(P3PsdError::MomentOrderMismatch { .. })
        ));
    }

    #[test]
    fn piecewise_boundaries_match_official_mass_and_area_semantics() {
        let law = P3PiecewiseParticleLaw::reconstruct(0.5, 400.0).unwrap();
        let boundary = |threshold: f64| {
            (
                law.particle(threshold * (1.0 - 1.0e-9)).unwrap(),
                law.particle(threshold * (1.0 + 1.0e-9)).unwrap(),
            )
        };
        let (small_sphere, dense_unrimed) = boundary(law.small_sphere_limit_m);
        let (dense_unrimed_large, fully_rimed) = boundary(law.dense_unrimed_to_graupel_m);
        let (fully_rimed_large, partially_rimed) = boundary(law.graupel_to_partially_rimed_m);

        for (below, above) in [
            (small_sphere, dense_unrimed),
            (dense_unrimed_large, fully_rimed),
            (fully_rimed_large, partially_rimed),
        ] {
            assert!(relative_error(below.mass_kg, above.mass_kg) < 0.011);
        }

        assert_eq!(small_sphere.region, P3ParticleRegion::SmallDenseSphere);
        assert_eq!(dense_unrimed.region, P3ParticleRegion::DenseUnrimed);
        assert_eq!(dense_unrimed_large.region, P3ParticleRegion::DenseUnrimed);
        assert_eq!(fully_rimed.region, P3ParticleRegion::FullyRimedSphere);
        assert_eq!(fully_rimed_large.region, P3ParticleRegion::FullyRimedSphere);
        assert_eq!(partially_rimed.region, P3ParticleRegion::PartiallyRimed);

        // The pinned WRF generator defines the first two breakpoints by mass.
        // It deliberately switches area laws without matching their
        // coefficients: solid sphere -> empirical unrimed area is a downward
        // jump, and empirical unrimed -> fully rimed sphere is an upward jump.
        assert!(small_sphere.projected_area_m2 > dense_unrimed.projected_area_m2);
        assert!(dense_unrimed_large.projected_area_m2 < fully_rimed.projected_area_m2);

        // The partially-rimed area is instead mass-interpolated from the
        // unrimed and graupel laws. Its boundary mismatch is therefore bounded
        // by the source's one-percent graupel-density fixed-point tolerance.
        assert!(
            relative_error(
                fully_rimed_large.projected_area_m2,
                partially_rimed.projected_area_m2,
            ) < 0.011
        );
    }

    #[test]
    fn two_moment_reconstruction_closes_supplied_number_and_mass() {
        let scheme = P3WrfScheme::Mp52TwoIcePredictedCloudNumber;
        let lambda = 2.0e5;
        let mu = 2.0;
        let input = state_from_exact_distribution(scheme, lambda, mu, 3.0e5, 0.9);
        let psd = P3Psd::reconstruct(
            input,
            &fixture_table(scheme, lambda, mu),
            P3ReconstructionConfig {
                maximum_moment_relative_error: 1.0e-10,
            },
        )
        .unwrap();
        assert!(psd.closure.number_relative_error < 1.0e-12);
        assert!(psd.closure.mass_relative_error < 1.0e-12);
        assert!(psd.closure.expected_sixth_moment_m3.is_none());
        assert_eq!(
            psd.number_limiter_audit().original_total_number_per_kg,
            input.total_number_per_kg
        );
        assert_eq!(
            psd.number_limiter_audit().repaired_total_number_per_kg,
            f64::from(input.total_number_per_kg as f32)
        );
        assert!(!psd.number_limiter_audit().nsmall_applied);
        assert!(!psd.number_limiter_audit().maximum_applied);
        assert!(!psd.number_limiter_audit().minimum_applied);
    }

    #[test]
    fn table_interpolation_number_residual_is_audited_while_mass_remains_exact() {
        let scheme = P3WrfScheme::Mp50OneIceFixedCloudNumber;
        let input = state_from_exact_distribution(scheme, 2.0e5, 2.0, 3.0e5, 0.9);
        // Mimic an interpolated official-table solution that does not exactly
        // invert the supplied Q/N tuple. The pinned generator normalizes N0
        // from Q in this situation; the repaired Ni remains an audit target.
        let psd = P3Psd::reconstruct(
            input,
            &fixture_table(scheme, 1.6e5, 2.0),
            P3ReconstructionConfig {
                maximum_moment_relative_error: 1.0e-10,
            },
        )
        .unwrap();

        assert!(psd.closure.mass_relative_error < 1.0e-12);
        assert!(psd.closure.number_relative_error > 0.03);
        assert_eq!(
            psd.closure.expected_number_density_m3,
            psd.number_limiter_audit().repaired_total_number_per_kg * input.dry_air_density_kg_m3
        );
    }

    #[test]
    fn wrf_real_number_limiter_repairs_negative_leading_edge_and_preserves_valid_number() {
        let qice = 2.33e-9_f64;
        let inverse_qmin = 2.0e15_f64;
        let inverse_qmax = 4.0e13_f64;
        let repaired =
            P3NumberLimiterAudit::from_wrf_real(-3.0e-7, qice, inverse_qmin, inverse_qmax).unwrap();
        let qice_real = qice as f32;
        let expected_minimum = (inverse_qmax as f32) * qice_real;
        assert!(repaired.nsmall_applied);
        assert!(!repaired.maximum_applied);
        assert!(repaired.minimum_applied);
        assert_eq!(
            (repaired.repaired_total_number_per_kg as f32).to_bits(),
            expected_minimum.to_bits()
        );

        let unchanged = P3NumberLimiterAudit::from_wrf_real(1.0e6, 1.0e-4, 1.0e12, 1.0e3).unwrap();
        assert_eq!(
            unchanged.repaired_total_number_per_kg,
            unchanged.wrf_real_original_total_number_per_kg
        );
        assert!(!unchanged.nsmall_applied);
        assert!(!unchanged.maximum_applied);
        assert!(!unchanged.minimum_applied);

        let upper_limited =
            P3NumberLimiterAudit::from_wrf_real(1.0e9, 1.0e-4, 1.0e12, 1.0e3).unwrap();
        assert!(!upper_limited.nsmall_applied);
        assert!(upper_limited.maximum_applied);
        assert!(!upper_limited.minimum_applied);
        assert_eq!(
            upper_limited.repaired_total_number_per_kg,
            upper_limited.maximum_total_number_per_kg
        );

        for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(matches!(
                P3NumberLimiterAudit::from_wrf_real(invalid, 1.0e-4, 1.0e12, 1.0e3),
                Err(P3LookupFailure::OutsideDomain(_))
            ));
        }
    }

    #[test]
    fn wrf_real_rime_density_clamps_f32_boundary_roundoff_before_widening() {
        let mass = 4.999_999_912_258_35e-14_f64;
        let low_volume = 1.000_000_003_627_493_7e-15_f64;
        let high_volume = 5.555_555_428_653_967e-17_f64;
        assert!(mass / low_volume < P3_RIME_DENSITY_RANGE_KG_M3[0]);
        assert!(mass / high_volume > P3_RIME_DENSITY_RANGE_KG_M3[1]);
        assert_eq!(
            wrf_real_rime_density_kg_m3(mass, low_volume).unwrap(),
            P3_RIME_DENSITY_RANGE_KG_M3[0]
        );
        assert_eq!(
            wrf_real_rime_density_kg_m3(mass, high_volume).unwrap(),
            P3_RIME_DENSITY_RANGE_KG_M3[1]
        );

        let middle_volume = f64::from((mass as f32) / 400.0_f32);
        assert_eq!(
            wrf_real_rime_density_kg_m3(mass, middle_volume).unwrap(),
            400.0
        );
    }

    #[test]
    fn reconstruction_uses_repaired_negative_leading_edge_number_and_audits_source() {
        let scheme = P3WrfScheme::Mp50OneIceFixedCloudNumber;
        let lambda = 2.0e5;
        let mu = 2.0;
        let target_number = 2.0e5;
        let mut input = state_from_exact_distribution(scheme, lambda, mu, target_number, 0.9);
        input.total_number_per_kg = -4.0e-8;
        let mut table = fixture_table(scheme, lambda, mu);
        table.inverse_qmax = target_number / input.total_ice_kgkg;
        table.inverse_qmin = 10.0 * table.inverse_qmax;
        let psd = P3Psd::reconstruct(
            input,
            &table,
            P3ReconstructionConfig {
                maximum_moment_relative_error: 2.0e-6,
            },
        )
        .unwrap();
        let audit = psd.number_limiter_audit();
        assert_eq!(audit.original_total_number_per_kg, -4.0e-8);
        assert!(audit.nsmall_applied);
        assert!(audit.minimum_applied);
        assert!((audit.repaired_total_number_per_kg - target_number).abs() < 0.1);

        let mut invalid = input;
        invalid.total_number_per_kg = f64::NAN;
        assert!(matches!(
            P3Psd::reconstruct(invalid, &table, P3ReconstructionConfig::default()),
            Err(P3PsdError::InvalidInput {
                field: "P3 total number mixing ratio",
                ..
            })
        ));

        let triple_scheme = P3WrfScheme::Mp53OneIceTripleMoment;
        let mut triple =
            state_from_exact_distribution(triple_scheme, lambda, mu, target_number, 0.9);
        triple.total_number_per_kg = -4.0e-8;
        let mut triple_table = fixture_table(triple_scheme, lambda, mu);
        triple_table.inverse_qmax = target_number / triple.total_ice_kgkg;
        triple_table.inverse_qmin = 10.0 * triple_table.inverse_qmax;
        assert!(matches!(
            P3Psd::reconstruct(triple, &triple_table, P3ReconstructionConfig::default()),
            Err(P3PsdError::TripleMomentNumberRepairRequiresZiLimiter { .. })
        ));
    }

    #[test]
    fn rimed_reconstruction_closes_total_rime_mass_and_rime_volume() {
        let scheme = P3WrfScheme::Mp50OneIceFixedCloudNumber;
        let lambda: f64 = 8.0e3;
        let mu: f64 = 1.0;
        let number_per_kg: f64 = 8.0e4;
        let dry_density: f64 = 0.8;
        let rime_fraction: f64 = 0.5;
        let rime_density: f64 = 400.0;
        let law = P3PiecewiseParticleLaw::reconstruct(rime_fraction, rime_density).unwrap();
        let number_density = number_per_kg * dry_density;
        let n0 =
            (number_density.ln() + (mu + 1.0) * lambda.ln() - ln_gamma(mu + 1.0).unwrap()).exp();
        let mass = integrate_piecewise_mass(n0, lambda, mu, law, 0.0, f64::INFINITY).unwrap();
        let total_ice = mass / dry_density;
        let rime_mass = rime_fraction * total_ice;
        let input = P3PsdInput::two_moment(
            scheme,
            P3Category::Category1,
            total_ice,
            number_per_kg,
            rime_mass,
            rime_mass / rime_density,
            dry_density,
        );
        let psd = P3Psd::reconstruct(
            input,
            &fixture_table(scheme, lambda, mu),
            P3ReconstructionConfig {
                maximum_moment_relative_error: 1.0e-10,
            },
        )
        .unwrap();
        assert!(psd.closure.rime_mass_relative_error < 1.0e-12);
        assert!(psd.closure.rime_volume_relative_error < 1.0e-12);
        assert_eq!(
            psd.law.particle(20.0e-3).unwrap().shape_authority,
            P3ShapeAuthority::MaximumDimensionAndProjectedAreaOnly
        );
    }

    #[test]
    fn triple_moment_qzi_reconstructs_m6_after_wrf_history_transform() {
        let scheme = P3WrfScheme::Mp53OneIceTripleMoment;
        let lambda = 1.5e5;
        let mu = 4.0;
        let input = state_from_exact_distribution(scheme, lambda, mu, 2.0e5, 1.1);
        let psd = P3Psd::reconstruct(
            input,
            &fixture_table(scheme, lambda, mu),
            P3ReconstructionConfig {
                maximum_moment_relative_error: 1.0e-10,
            },
        )
        .unwrap();
        assert!(psd.closure.sixth_moment_relative_error.unwrap() < 1.0e-12);
    }

    #[test]
    fn missing_or_wrong_official_table_fails_closed() {
        let scheme = P3WrfScheme::Mp50OneIceFixedCloudNumber;
        let input = state_from_exact_distribution(scheme, 2.0e5, 2.0, 1.0e5, 1.0);
        let mut table = fixture_table(scheme, 2.0e5, 2.0);
        table.descriptor.table_version = P3_THREE_MOMENT_TABLE_VERSION.to_owned();
        assert!(matches!(
            P3Psd::reconstruct(input, &table, P3ReconstructionConfig::default()),
            Err(P3PsdError::LookupRevisionMismatch { .. })
        ));
        table.descriptor.table_version = scheme.required_table_version().to_owned();
        table.descriptor.table_sha256 = Sha256Digest::compute(b"not the official P3 table");
        assert!(matches!(
            P3Psd::reconstruct(input, &table, P3ReconstructionConfig::default()),
            Err(P3PsdError::LookupDigestMismatch { .. })
        ));
    }

    #[test]
    fn quadrature_reports_tail_and_closes_retained_moments() {
        let scheme = P3WrfScheme::Mp53OneIceTripleMoment;
        let lambda = 1.8e5;
        let mu = 3.0;
        let input = state_from_exact_distribution(scheme, lambda, mu, 2.0e5, 1.0);
        let psd = P3Psd::reconstruct(
            input,
            &fixture_table(scheme, lambda, mu),
            P3ReconstructionConfig {
                maximum_moment_relative_error: 1.0e-10,
            },
        )
        .unwrap();
        let quadrature = psd.quadrature(P3QuadratureConfig::default()).unwrap();
        assert!(!quadrature.nodes.is_empty());
        assert!(quadrature.audit.omission.upper_tail_number_fraction <= 1.01e-10);
        assert!(quadrature.audit.omission.upper_tail_mass_fraction <= 1.01e-10);
        assert!(quadrature.audit.omission.upper_tail_sixth_moment_fraction <= 1.01e-10);
        assert!(quadrature.audit.number_quadrature_relative_error <= 2.0e-7);
        assert!(quadrature.audit.mass_quadrature_relative_error <= 2.0e-7);
        assert!(quadrature.audit.sixth_moment_quadrature_relative_error <= 2.0e-7);
    }
}
