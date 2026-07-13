//! Scheme-native particle-size-distribution reconstruction and integration.
//!
//! This module deliberately separates three concerns:
//! - reconstructing a distribution from native microphysics state,
//! - producing auditable quadrature nodes with physical particle geometry,
//! - summing a caller-provided per-particle scattering operator in additive
//!   space.
//!
//! The first supported distribution is Jensen ISHMAEL's gamma distribution.
//! It is reconstructed from the native `QICE`, `QNICE`, `QVOLI`, and `QAOLI`
//! tuple. No characteristic-particle closure, orientation assumption,
//! dielectric model, or terminal-speed proxy is introduced here.

use std::{
    error::Error,
    f64::consts::PI,
    fmt::{self, Display, Formatter},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    AdditiveScattering, IshmaelIceCategory, OutputError, P3Category, PolarAccumulatorQuantities,
    Sha256Digest,
};

/// Version stamped on the first scheme-native PSD implementation.
pub const SCHEME_PSD_REVISION: &str = "scheme-psd-v1";
/// Version stamped on the ISHMAEL reconstruction equations.
pub const ISHMAEL_PSD_REVISION: &str = "wrf-jensen-ishmael-gamma-v1";

/// ISHMAEL's fixed gamma shape parameter, `nu`.
pub const ISHMAEL_GAMMA_SHAPE: f64 = 4.0;
/// ISHMAEL's monomer semi-axis scale, 0.1 micrometre.
pub const ISHMAEL_MONOMER_SEMI_AXIS_M: f64 = 0.1e-6;
/// Bounds applied by WRF's ISHMAEL state checks.
pub const ISHMAEL_DELTA_RANGE: [f64; 2] = [0.55, 1.30];
/// Bounds applied by WRF's ISHMAEL state checks.
pub const ISHMAEL_DENSITY_RANGE_KG_M3: [f64; 2] = [50.0, 920.0];
/// Absolute coarse/refined convergence tolerances in
/// [`AdditiveScattering::components`] order. Each entry therefore retains
/// the component's native unit instead of sharing a dimensionally invalid
/// scalar floor.
pub const DEFAULT_ADDITIVE_ABSOLUTE_TOLERANCES: [f64; AdditiveScattering::COMPONENT_COUNT] = [
    1.0e-10, 1.0e-10, 1.0e-10, 1.0e-10, 1.0e-8, 1.0e-8, 1.0e-8, 1.0e-10, 1.0e-10,
];

const GL8_ABSCISSAE: [f64; 8] = [
    -0.960_289_856_497_536_3,
    -0.796_666_477_413_626_7,
    -0.525_532_409_916_329,
    -0.183_434_642_495_649_8,
    0.183_434_642_495_649_8,
    0.525_532_409_916_329,
    0.796_666_477_413_626_7,
    0.960_289_856_497_536_3,
];
const GL8_WEIGHTS: [f64; 8] = [
    0.101_228_536_290_376_3,
    0.222_381_034_453_374_5,
    0.313_706_645_877_887_3,
    0.362_683_783_378_362,
    0.362_683_783_378_362,
    0.313_706_645_877_887_3,
    0.222_381_034_453_374_5,
    0.101_228_536_290_376_3,
];
const GL8_POINTS_PER_PANEL: usize = 8;
const REFINEMENT_FACTOR: usize = 2;
const NUMERICAL_MAX_ITERATIONS: usize = 256;
const NUMERICAL_EPSILON: f64 = 2.0e-14;
const NUMERICAL_FLOOR: f64 = 1.0e-300;
const RECONSTRUCTION_RELATIVE_TOLERANCE: f64 = 2.0e-12;
const SPHERICAL_LOG_AXIS_TOLERANCE: f64 = 1.0e-12;
// Native ISHMAEL prognostics are WRF REAL values. Reconstructing a bounded
// diagnostic from several transported f32 fields (ratios, roots, and a gamma
// moment) can accumulate slightly more than eight single-precision epsilons.
// Sixteen epsilons is still a narrowly bounded ~1.9 ppm transport allowance;
// values admitted by it are canonicalized to the exact source bound below.
const SOURCE_BOUND_RELATIVE_TOLERANCE: f64 = 16.0 * f32::EPSILON as f64;

/// The native distribution revision selected by a PSD integration config.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemePsdRevision {
    IshmaelGammaV1,
}

/// Deterministic bounded quadrature implemented by this revision.
///
/// The gamma-weighted integral is evaluated on a finite interval using an
/// eight-point Gauss-Legendre rule per panel. The interval is selected from
/// analytic gamma tails. A second evaluation at twice the panel count forms
/// the base convergence audit, and a bounded third grid is available when
/// the measured additive integral requires one further refinement.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PsdQuadratureRule {
    CompositeGaussLegendre8RefinedV1,
    /// The same bounded GL8 rules, with one further factor-of-two grid
    /// admitted when the first coarse/refined scattering comparison exceeds
    /// its magnitude-scaled convergence tolerance and the configured node
    /// budget can represent the additional rule.
    CompositeGaussLegendre8AdaptiveRefinedV2,
}

/// Whether a physical ISHMAEL node is oblate, prolate, or effectively
/// spherical. This is diagnosed per node from the native `c(a)` relation,
/// not from the source category label.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PsdSpheroidHabit {
    Oblate,
    Prolate,
    Spherical,
}

/// Which convergence pass produced a callback failure.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PsdQuadratureLevel {
    Coarse,
    Refined,
    AdaptiveRefined,
}

/// Scheme/category identity retained separately from generic node geometry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PsdSourceCategory {
    Ishmael(IshmaelIceCategory),
    P3(P3Category),
}

/// Authority behind the positive-down terminal speed supplied to the
/// per-particle scattering callback.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PsdFallSpeedAuthority {
    WrfIshmaelMitchellHeymsfieldV1,
    TMatrixTableTerminalPolicyV1,
    ExternalVersionedResearch,
    SyntheticTestOnly,
}

/// Exact implementation/config digest for the terminal-speed law. The PSD
/// integrator does not invent a fall-speed proxy, so this token is mandatory
/// and travels with its Doppler-moment audit.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PsdFallSpeedProvenance {
    authority: PsdFallSpeedAuthority,
    implementation_sha256: Sha256Digest,
}

impl PsdFallSpeedProvenance {
    #[must_use]
    pub const fn new(
        authority: PsdFallSpeedAuthority,
        implementation_sha256: Sha256Digest,
    ) -> Self {
        Self {
            authority,
            implementation_sha256,
        }
    }

    #[must_use]
    pub const fn authority(self) -> PsdFallSpeedAuthority {
        self.authority
    }

    #[must_use]
    pub const fn implementation_sha256(self) -> Sha256Digest {
        self.implementation_sha256
    }
}

/// Native ISHMAEL state required to reconstruct one category distribution.
/// WRF mixing ratios and number are per kilogram of dry air, so the supplied
/// density must also be dry-air density before conversion to cubic metres.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IshmaelPsdInput {
    category: IshmaelIceCategory,
    qice_kgkg: f64,
    qnice_per_kg: f64,
    qvoli_m3_per_kg: f64,
    qaoli_m3_per_kg: f64,
    dry_air_density_kg_m3: f64,
}

impl IshmaelPsdInput {
    #[must_use]
    pub const fn new(
        category: IshmaelIceCategory,
        qice_kgkg: f64,
        qnice_per_kg: f64,
        qvoli_m3_per_kg: f64,
        qaoli_m3_per_kg: f64,
        dry_air_density_kg_m3: f64,
    ) -> Self {
        Self {
            category,
            qice_kgkg,
            qnice_per_kg,
            qvoli_m3_per_kg,
            qaoli_m3_per_kg,
            dry_air_density_kg_m3,
        }
    }

    #[must_use]
    pub const fn category(self) -> IshmaelIceCategory {
        self.category
    }

    #[must_use]
    pub const fn qice_kgkg(self) -> f64 {
        self.qice_kgkg
    }

    #[must_use]
    pub const fn qnice_per_kg(self) -> f64 {
        self.qnice_per_kg
    }

    #[must_use]
    pub const fn qvoli_m3_per_kg(self) -> f64 {
        self.qvoli_m3_per_kg
    }

    #[must_use]
    pub const fn qaoli_m3_per_kg(self) -> f64 {
        self.qaoli_m3_per_kg
    }

    #[must_use]
    pub const fn dry_air_density_kg_m3(self) -> f64 {
        self.dry_air_density_kg_m3
    }
}

/// Audit of the algebraic reconstruction from the four native ISHMAEL
/// prognostics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct IshmaelReconstructionAudit {
    pub qvoli_relative_error: f64,
    pub qaoli_relative_error: f64,
    pub mass_relative_error: f64,
    /// Signed raw excursion beyond the nearest WRF bound; zero when inside.
    pub delta_bound_excursion: f64,
    /// Signed raw excursion beyond the nearest WRF bound; zero when inside.
    pub density_bound_excursion_kg_m3: f64,
}

/// A reconstructed native ISHMAEL gamma distribution.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IshmaelPsd {
    input: IshmaelPsdInput,
    a_scale_m: f64,
    c_at_a_scale_m: f64,
    aspect_power_delta: f64,
    bulk_density_kg_m3: f64,
    mean_particle_mass_kg: f64,
    mean_equivolume_diameter_sixth_m6: f64,
    reconstruction: IshmaelReconstructionAudit,
}

impl IshmaelPsd {
    /// Reconstruct the exact gamma distribution and power-law habit relation
    /// represented by native ISHMAEL `QICE/QNICE/QVOLI/QAOLI` state.
    pub fn reconstruct(input: IshmaelPsdInput) -> Result<Self, PsdError> {
        positive("QICE", input.qice_kgkg)?;
        positive("QNICE", input.qnice_per_kg)?;
        positive("QVOLI", input.qvoli_m3_per_kg)?;
        positive("QAOLI", input.qaoli_m3_per_kg)?;
        positive("dry-air density", input.dry_air_density_kg_m3)?;

        let a_cubed = checked_positive_computation(
            "ISHMAEL a_n cubed",
            input.qvoli_m3_per_kg * input.qvoli_m3_per_kg
                / (input.qaoli_m3_per_kg * input.qnice_per_kg),
        )?;
        let c_cubed = checked_positive_computation(
            "ISHMAEL c_n cubed",
            input.qaoli_m3_per_kg * input.qaoli_m3_per_kg
                / (input.qvoli_m3_per_kg * input.qnice_per_kg),
        )?;
        let a_scale_m = a_cubed.cbrt();
        let raw_c_at_a_scale_m = c_cubed.cbrt();
        if a_scale_m <= ISHMAEL_MONOMER_SEMI_AXIS_M {
            return Err(PsdError::OutsideReconstructionBound {
                field: "ISHMAEL a_n",
                value: a_scale_m,
                minimum: ISHMAEL_MONOMER_SEMI_AXIS_M,
                maximum: f64::MAX,
            });
        }

        let denominator = (a_scale_m / ISHMAEL_MONOMER_SEMI_AXIS_M).ln();
        let raw_aspect_power_delta =
            (raw_c_at_a_scale_m / ISHMAEL_MONOMER_SEMI_AXIS_M).ln() / denominator;
        let (aspect_power_delta, delta_bound_excursion) =
            source_bounded("ISHMAEL delta", raw_aspect_power_delta, ISHMAEL_DELTA_RANGE)?;
        let c_at_a_scale_m = ISHMAEL_MONOMER_SEMI_AXIS_M.powf(1.0 - aspect_power_delta)
            * a_scale_m.powf(aspect_power_delta);

        let mean_volume_m3 =
            mean_particle_volume(a_scale_m, aspect_power_delta, ISHMAEL_GAMMA_SHAPE)?;
        let mean_particle_mass_kg = input.qice_kgkg / input.qnice_per_kg;
        let raw_bulk_density_kg_m3 = checked_positive_computation(
            "ISHMAEL bulk density",
            mean_particle_mass_kg / mean_volume_m3,
        )?;
        let (bulk_density_kg_m3, density_bound_excursion_kg_m3) = source_bounded(
            "ISHMAEL bulk density",
            raw_bulk_density_kg_m3,
            ISHMAEL_DENSITY_RANGE_KG_M3,
        )?;

        let mean_equivolume_diameter_sixth_m6 = checked_positive_computation(
            "ISHMAEL mean equivalent-volume diameter sixth moment",
            64.0 * ISHMAEL_MONOMER_SEMI_AXIS_M.powf(2.0 * (1.0 - aspect_power_delta))
                * analytic_gamma_moment(
                    1.0,
                    a_scale_m,
                    ISHMAEL_GAMMA_SHAPE,
                    4.0 + 2.0 * aspect_power_delta,
                )?,
        )?;

        let reconstructed_qvoli = input.qnice_per_kg * a_scale_m * a_scale_m * raw_c_at_a_scale_m;
        let reconstructed_qaoli =
            input.qnice_per_kg * a_scale_m * raw_c_at_a_scale_m * raw_c_at_a_scale_m;
        let reconstructed_qice = input.qnice_per_kg * raw_bulk_density_kg_m3 * mean_volume_m3;
        let reconstruction = IshmaelReconstructionAudit {
            qvoli_relative_error: relative_error(reconstructed_qvoli, input.qvoli_m3_per_kg, 0.0),
            qaoli_relative_error: relative_error(reconstructed_qaoli, input.qaoli_m3_per_kg, 0.0),
            mass_relative_error: relative_error(reconstructed_qice, input.qice_kgkg, 0.0),
            delta_bound_excursion,
            density_bound_excursion_kg_m3,
        };
        for (moment, error) in [
            ("QVOLI", reconstruction.qvoli_relative_error),
            ("QAOLI", reconstruction.qaoli_relative_error),
            ("QICE", reconstruction.mass_relative_error),
        ] {
            if error > RECONSTRUCTION_RELATIVE_TOLERANCE {
                return Err(PsdError::ReconstructionClosure {
                    moment,
                    relative_error: error,
                    maximum: RECONSTRUCTION_RELATIVE_TOLERANCE,
                });
            }
        }

        Ok(Self {
            input,
            a_scale_m,
            c_at_a_scale_m,
            aspect_power_delta,
            bulk_density_kg_m3,
            mean_particle_mass_kg,
            mean_equivolume_diameter_sixth_m6,
            reconstruction,
        })
    }

    #[must_use]
    pub const fn input(self) -> IshmaelPsdInput {
        self.input
    }

    #[must_use]
    pub const fn category(self) -> IshmaelIceCategory {
        self.input.category
    }

    #[must_use]
    pub const fn a_scale_m(self) -> f64 {
        self.a_scale_m
    }

    #[must_use]
    pub const fn c_at_a_scale_m(self) -> f64 {
        self.c_at_a_scale_m
    }

    #[must_use]
    pub const fn aspect_power_delta(self) -> f64 {
        self.aspect_power_delta
    }

    #[must_use]
    pub const fn bulk_density_kg_m3(self) -> f64 {
        self.bulk_density_kg_m3
    }

    #[must_use]
    pub const fn number_density_m3(self) -> f64 {
        self.input.qnice_per_kg * self.input.dry_air_density_kg_m3
    }

    #[must_use]
    pub const fn mass_concentration_kg_m3(self) -> f64 {
        self.input.qice_kgkg * self.input.dry_air_density_kg_m3
    }

    #[must_use]
    pub const fn mean_particle_mass_kg(self) -> f64 {
        self.mean_particle_mass_kg
    }

    #[must_use]
    pub const fn mean_equivolume_diameter_sixth_m6(self) -> f64 {
        self.mean_equivolume_diameter_sixth_m6
    }

    #[must_use]
    pub const fn reconstruction_audit(self) -> IshmaelReconstructionAudit {
        self.reconstruction
    }

    #[cfg(test)]
    fn equivolume_diameter_at_scaled_a(self, scaled_a: f64) -> Result<f64, PsdError> {
        positive("scaled ISHMAEL a coordinate", scaled_a)?;
        let diameter_at_scale =
            2.0 * (self.a_scale_m * self.a_scale_m * self.c_at_a_scale_m).cbrt();
        checked_positive_computation(
            "ISHMAEL equivalent-volume diameter at scaled a",
            diameter_at_scale * scaled_a.powf((2.0 + self.aspect_power_delta) / 3.0),
        )
    }

    fn scaled_a_for_diameter(self, diameter_m: f64) -> f64 {
        let diameter_at_scale =
            2.0 * (self.a_scale_m * self.a_scale_m * self.c_at_a_scale_m).cbrt();
        (((diameter_m.ln() - diameter_at_scale.ln()) * 3.0) / (2.0 + self.aspect_power_delta)).exp()
    }

    fn support_intervals(
        self,
        support: PsdParticleSupport,
        upper_scaled_a: f64,
    ) -> Vec<SupportInterval> {
        let exponent = self.aspect_power_delta - 1.0;
        let log_ratio_at_scale = (self.c_at_a_scale_m / self.a_scale_m).ln();
        let mut intervals = Vec::with_capacity(3);

        if exponent == 0.0 {
            if let Some(domain) = support.spherical
                && contains(domain.bulk_density_kg_m3, self.bulk_density_kg_m3)
                && contains(domain.minor_to_major_axis_ratio, 1.0)
            {
                push_clipped_interval(
                    &mut intervals,
                    self.scaled_a_for_diameter(domain.equivolume_diameter_m[0]),
                    self.scaled_a_for_diameter(domain.equivolume_diameter_m[1]),
                    upper_scaled_a,
                );
            }
            return intervals;
        }

        for (habit, domain) in [
            (PsdSpheroidHabit::Oblate, support.oblate),
            (PsdSpheroidHabit::Prolate, support.prolate),
        ] {
            let Some(domain) = domain else {
                continue;
            };
            if !contains(domain.bulk_density_kg_m3, self.bulk_density_kg_m3) {
                continue;
            }

            let diameter_lower = self.scaled_a_for_diameter(domain.equivolume_diameter_m[0]);
            let diameter_upper = self.scaled_a_for_diameter(domain.equivolume_diameter_m[1]);
            let [ratio_minimum, ratio_maximum] = domain.minor_to_major_axis_ratio;
            let raw_log_bounds = match habit {
                PsdSpheroidHabit::Oblate => [ratio_minimum.ln(), ratio_maximum.ln()],
                PsdSpheroidHabit::Prolate => [-ratio_maximum.ln(), -ratio_minimum.ln()],
                PsdSpheroidHabit::Spherical => unreachable!("spherical handled above"),
            };
            let first = ((raw_log_bounds[0] - log_ratio_at_scale) / exponent).exp();
            let second = ((raw_log_bounds[1] - log_ratio_at_scale) / exponent).exp();
            let axis_lower = first.min(second);
            let axis_upper = first.max(second);
            push_clipped_interval(
                &mut intervals,
                diameter_lower.max(axis_lower),
                diameter_upper.min(axis_upper),
                upper_scaled_a,
            );
        }
        intervals.sort_by(|left, right| left.lower.total_cmp(&right.lower));
        intervals
    }

    fn node(
        self,
        index: usize,
        scaled_a: f64,
        number_fraction: f64,
    ) -> Result<PsdParticleNode, PsdError> {
        let a_semi_axis_m =
            checked_positive_computation("ISHMAEL node a semi-axis", self.a_scale_m * scaled_a)?;
        let c_semi_axis_m = checked_positive_computation(
            "ISHMAEL node c semi-axis",
            ISHMAEL_MONOMER_SEMI_AXIS_M.powf(1.0 - self.aspect_power_delta)
                * a_semi_axis_m.powf(self.aspect_power_delta),
        )?;
        let volume_m3 = checked_positive_computation(
            "ISHMAEL node volume",
            (4.0 / 3.0) * PI * a_semi_axis_m * a_semi_axis_m * c_semi_axis_m,
        )?;
        let mass_kg =
            checked_positive_computation("ISHMAEL node mass", self.bulk_density_kg_m3 * volume_m3)?;
        let equivolume_diameter_m = checked_positive_computation(
            "ISHMAEL node equivalent-volume diameter",
            2.0 * (a_semi_axis_m * a_semi_axis_m * c_semi_axis_m).cbrt(),
        )?;
        let axis_ratio = c_semi_axis_m / a_semi_axis_m;
        let log_axis_ratio = axis_ratio.ln();
        let habit = if log_axis_ratio.abs() <= SPHERICAL_LOG_AXIS_TOLERANCE {
            PsdSpheroidHabit::Spherical
        } else if axis_ratio < 1.0 {
            PsdSpheroidHabit::Oblate
        } else {
            PsdSpheroidHabit::Prolate
        };
        let minor_to_major_axis_ratio = if axis_ratio <= 1.0 {
            axis_ratio
        } else {
            1.0 / axis_ratio
        };
        if !(minor_to_major_axis_ratio.is_finite()
            && 0.0 < minor_to_major_axis_ratio
            && minor_to_major_axis_ratio <= 1.0)
        {
            return Err(PsdError::InvalidComputation {
                field: "ISHMAEL node minor-to-major axis ratio",
                value: minor_to_major_axis_ratio,
            });
        }
        let number_density_m3 = checked_positive_computation(
            "ISHMAEL quadrature-node number density",
            self.number_density_m3() * number_fraction,
        )?;

        Ok(PsdParticleNode {
            index,
            source_category: PsdSourceCategory::Ishmael(self.category()),
            scaled_a,
            a_semi_axis_m,
            c_semi_axis_m,
            equivolume_diameter_m,
            bulk_density_kg_m3: self.bulk_density_kg_m3,
            minor_to_major_axis_ratio,
            habit,
            particle_volume_m3: volume_m3,
            particle_mass_kg: mass_kg,
            number_fraction,
            number_density_m3,
            rime_mass_fraction: None,
            rime_density_kg_m3: None,
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct SupportInterval {
    lower: f64,
    upper: f64,
}

fn push_clipped_interval(
    intervals: &mut Vec<SupportInterval>,
    first: f64,
    second: f64,
    upper_scaled_a: f64,
) {
    let lower = first.min(second).max(0.0);
    let upper = first.max(second).min(upper_scaled_a);
    if lower.is_finite() && upper.is_finite() && upper > lower {
        intervals.push(SupportInterval { lower, upper });
    }
}

/// One physical particle and its quadrature population weight.
///
/// Scattering callbacks must return the additive response normalized to one
/// particle per cubic metre. [`integrate_ishmael_psd`] applies
/// `number_density_m3` exactly once.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PsdParticleNode {
    index: usize,
    source_category: PsdSourceCategory,
    scaled_a: f64,
    a_semi_axis_m: f64,
    c_semi_axis_m: f64,
    equivolume_diameter_m: f64,
    bulk_density_kg_m3: f64,
    minor_to_major_axis_ratio: f64,
    habit: PsdSpheroidHabit,
    particle_volume_m3: f64,
    particle_mass_kg: f64,
    number_fraction: f64,
    number_density_m3: f64,
    rime_mass_fraction: Option<f64>,
    rime_density_kg_m3: Option<f64>,
}

impl PsdParticleNode {
    #[must_use]
    pub const fn index(self) -> usize {
        self.index
    }

    #[must_use]
    pub const fn source_category(self) -> PsdSourceCategory {
        self.source_category
    }

    #[must_use]
    pub const fn ishmael_category(self) -> Option<IshmaelIceCategory> {
        match self.source_category {
            PsdSourceCategory::Ishmael(category) => Some(category),
            PsdSourceCategory::P3(_) => None,
        }
    }

    #[must_use]
    pub const fn scaled_a(self) -> f64 {
        self.scaled_a
    }

    #[must_use]
    pub const fn a_semi_axis_m(self) -> f64 {
        self.a_semi_axis_m
    }

    #[must_use]
    pub const fn c_semi_axis_m(self) -> f64 {
        self.c_semi_axis_m
    }

    #[must_use]
    pub const fn equivolume_diameter_m(self) -> f64 {
        self.equivolume_diameter_m
    }

    #[must_use]
    pub const fn bulk_density_kg_m3(self) -> f64 {
        self.bulk_density_kg_m3
    }

    #[must_use]
    pub const fn minor_to_major_axis_ratio(self) -> f64 {
        self.minor_to_major_axis_ratio
    }

    #[must_use]
    pub const fn habit(self) -> PsdSpheroidHabit {
        self.habit
    }

    #[must_use]
    pub const fn particle_volume_m3(self) -> f64 {
        self.particle_volume_m3
    }

    #[must_use]
    pub const fn particle_mass_kg(self) -> f64 {
        self.particle_mass_kg
    }

    #[must_use]
    pub const fn number_fraction(self) -> f64 {
        self.number_fraction
    }

    #[must_use]
    pub const fn number_density_m3(self) -> f64 {
        self.number_density_m3
    }

    #[must_use]
    pub const fn rime_mass_fraction(self) -> Option<f64> {
        self.rime_mass_fraction
    }

    #[must_use]
    pub const fn rime_density_kg_m3(self) -> Option<f64> {
        self.rime_density_kg_m3
    }
}

/// Fail-closed physical domain represented by a scattering table family.
/// It is intentionally serialize-only: deserialization must not bypass
/// [`PsdParticleDomain::new`] validation.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct PsdParticleDomain {
    equivolume_diameter_m: [f64; 2],
    bulk_density_kg_m3: [f64; 2],
    minor_to_major_axis_ratio: [f64; 2],
}

impl PsdParticleDomain {
    pub fn new(
        equivolume_diameter_m: [f64; 2],
        bulk_density_kg_m3: [f64; 2],
        minor_to_major_axis_ratio: [f64; 2],
    ) -> Result<Self, PsdError> {
        positive_range("equivalent-volume diameter domain", equivolume_diameter_m)?;
        positive_range("bulk-density domain", bulk_density_kg_m3)?;
        positive_range(
            "minor-to-major axis-ratio domain",
            minor_to_major_axis_ratio,
        )?;
        if minor_to_major_axis_ratio[1] > 1.0 {
            return Err(PsdError::InvalidRange {
                field: "minor-to-major axis-ratio domain",
                minimum: minor_to_major_axis_ratio[0],
                maximum: minor_to_major_axis_ratio[1],
            });
        }
        Ok(Self {
            equivolume_diameter_m,
            bulk_density_kg_m3,
            minor_to_major_axis_ratio,
        })
    }

    #[must_use]
    pub const fn unbounded_physical() -> Self {
        Self {
            equivolume_diameter_m: [f64::MIN_POSITIVE, f64::MAX],
            bulk_density_kg_m3: [f64::MIN_POSITIVE, f64::MAX],
            minor_to_major_axis_ratio: [f64::MIN_POSITIVE, 1.0],
        }
    }

    #[must_use]
    pub const fn equivolume_diameter_range_m(self) -> [f64; 2] {
        self.equivolume_diameter_m
    }

    #[must_use]
    pub const fn bulk_density_range_kg_m3(self) -> [f64; 2] {
        self.bulk_density_kg_m3
    }

    #[must_use]
    pub const fn minor_to_major_axis_ratio_range(self) -> [f64; 2] {
        self.minor_to_major_axis_ratio
    }

    #[must_use]
    pub fn contains(self, node: PsdParticleNode) -> bool {
        contains(self.equivolume_diameter_m, node.equivolume_diameter_m)
            && contains(self.bulk_density_kg_m3, node.bulk_density_kg_m3)
            && contains(
                self.minor_to_major_axis_ratio,
                node.minor_to_major_axis_ratio,
            )
    }
}

impl Default for PsdParticleDomain {
    fn default() -> Self {
        Self::unbounded_physical()
    }
}

/// Habit-specific table support. Oblate and prolate LUTs commonly have
/// different size and density envelopes, so one rectangular domain is not a
/// sufficient science contract.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct PsdParticleSupport {
    oblate: Option<PsdParticleDomain>,
    prolate: Option<PsdParticleDomain>,
    spherical: Option<PsdParticleDomain>,
}

impl PsdParticleSupport {
    #[must_use]
    pub const fn new(
        oblate: Option<PsdParticleDomain>,
        prolate: Option<PsdParticleDomain>,
        spherical: Option<PsdParticleDomain>,
    ) -> Self {
        Self {
            oblate,
            prolate,
            spherical,
        }
    }

    #[must_use]
    pub const fn uniform(domain: PsdParticleDomain) -> Self {
        Self::new(Some(domain), Some(domain), Some(domain))
    }

    #[must_use]
    pub const fn oblate(self) -> Option<PsdParticleDomain> {
        self.oblate
    }

    #[must_use]
    pub const fn prolate(self) -> Option<PsdParticleDomain> {
        self.prolate
    }

    #[must_use]
    pub const fn spherical(self) -> Option<PsdParticleDomain> {
        self.spherical
    }

    #[must_use]
    pub const fn domain_for(self, habit: PsdSpheroidHabit) -> Option<PsdParticleDomain> {
        match habit {
            PsdSpheroidHabit::Oblate => self.oblate,
            PsdSpheroidHabit::Prolate => self.prolate,
            PsdSpheroidHabit::Spherical => self.spherical,
        }
    }

    #[must_use]
    pub fn contains(self, node: PsdParticleNode) -> bool {
        self.domain_for(node.habit())
            .is_some_and(|domain| domain.contains(node))
    }
}

impl Default for PsdParticleSupport {
    fn default() -> Self {
        Self::uniform(PsdParticleDomain::default())
    }
}

/// Versioned numerical and omission tolerances for PSD integration.
/// It is intentionally serialize-only until a validating custom
/// deserializer is provided.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct PsdIntegrationConfig {
    revision: SchemePsdRevision,
    quadrature: PsdQuadratureRule,
    coarse_panels: u16,
    maximum_refined_nodes: u32,
    maximum_scaled_a: f64,
    maximum_tail_fraction: f64,
    maximum_quadrature_closure_error: f64,
    maximum_additive_convergence_error: f64,
    additive_absolute_tolerances: [f64; AdditiveScattering::COMPONENT_COUNT],
    maximum_domain_omitted_number_fraction: f64,
    maximum_domain_omitted_mass_fraction: f64,
    maximum_domain_omitted_d6_fraction: f64,
}

impl PsdIntegrationConfig {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        coarse_panels: u16,
        maximum_refined_nodes: u32,
        maximum_scaled_a: f64,
        maximum_tail_fraction: f64,
        maximum_quadrature_closure_error: f64,
        maximum_additive_convergence_error: f64,
        additive_absolute_tolerances: [f64; AdditiveScattering::COMPONENT_COUNT],
        maximum_domain_omitted_number_fraction: f64,
        maximum_domain_omitted_mass_fraction: f64,
        maximum_domain_omitted_d6_fraction: f64,
    ) -> Result<Self, PsdError> {
        if coarse_panels == 0 {
            return Err(PsdError::InvalidIntegerConfig {
                field: "coarse panels",
                value: u64::from(coarse_panels),
            });
        }
        if maximum_refined_nodes == 0 {
            return Err(PsdError::InvalidIntegerConfig {
                field: "maximum refined nodes",
                value: u64::from(maximum_refined_nodes),
            });
        }
        positive("maximum scaled a", maximum_scaled_a)?;
        tolerance("maximum tail fraction", maximum_tail_fraction)?;
        tolerance(
            "maximum quadrature closure error",
            maximum_quadrature_closure_error,
        )?;
        tolerance(
            "maximum additive convergence error",
            maximum_additive_convergence_error,
        )?;
        for value in additive_absolute_tolerances {
            if !value.is_finite() || value < 0.0 {
                return Err(PsdError::InvalidInput {
                    field: "additive absolute convergence tolerance",
                    value,
                    requirement: "finite and nonnegative",
                });
            }
        }
        tolerance(
            "maximum omitted number fraction",
            maximum_domain_omitted_number_fraction,
        )?;
        tolerance(
            "maximum omitted mass fraction",
            maximum_domain_omitted_mass_fraction,
        )?;
        tolerance(
            "maximum omitted D6 fraction",
            maximum_domain_omitted_d6_fraction,
        )?;
        let refined_nodes = usize::from(coarse_panels)
            .checked_mul(REFINEMENT_FACTOR)
            .and_then(|panels| panels.checked_mul(GL8_POINTS_PER_PANEL))
            .ok_or(PsdError::NodeBudgetOverflow)?;
        if refined_nodes > maximum_refined_nodes as usize {
            return Err(PsdError::NodeBudgetExceeded {
                required: refined_nodes,
                maximum: maximum_refined_nodes as usize,
            });
        }
        Ok(Self {
            revision: SchemePsdRevision::IshmaelGammaV1,
            quadrature: PsdQuadratureRule::CompositeGaussLegendre8AdaptiveRefinedV2,
            coarse_panels,
            maximum_refined_nodes,
            maximum_scaled_a,
            maximum_tail_fraction,
            maximum_quadrature_closure_error,
            maximum_additive_convergence_error,
            additive_absolute_tolerances,
            maximum_domain_omitted_number_fraction,
            maximum_domain_omitted_mass_fraction,
            maximum_domain_omitted_d6_fraction,
        })
    }

    #[must_use]
    pub const fn revision(self) -> SchemePsdRevision {
        self.revision
    }

    #[must_use]
    pub const fn quadrature(self) -> PsdQuadratureRule {
        self.quadrature
    }

    #[must_use]
    pub const fn coarse_panels(self) -> u16 {
        self.coarse_panels
    }

    #[must_use]
    pub const fn base_refined_nodes(self) -> usize {
        self.coarse_panels as usize * REFINEMENT_FACTOR * GL8_POINTS_PER_PANEL
    }

    #[must_use]
    pub const fn maximum_refined_nodes(self) -> u32 {
        self.maximum_refined_nodes
    }

    #[must_use]
    pub const fn maximum_scaled_a(self) -> f64 {
        self.maximum_scaled_a
    }

    #[must_use]
    pub const fn maximum_tail_fraction(self) -> f64 {
        self.maximum_tail_fraction
    }

    #[must_use]
    pub const fn maximum_quadrature_closure_error(self) -> f64 {
        self.maximum_quadrature_closure_error
    }

    #[must_use]
    pub const fn maximum_additive_convergence_error(self) -> f64 {
        self.maximum_additive_convergence_error
    }

    #[must_use]
    pub const fn additive_absolute_tolerances(self) -> [f64; AdditiveScattering::COMPONENT_COUNT] {
        self.additive_absolute_tolerances
    }

    #[must_use]
    pub const fn maximum_domain_omitted_number_fraction(self) -> f64 {
        self.maximum_domain_omitted_number_fraction
    }

    #[must_use]
    pub const fn maximum_domain_omitted_mass_fraction(self) -> f64 {
        self.maximum_domain_omitted_mass_fraction
    }

    #[must_use]
    pub const fn maximum_domain_omitted_d6_fraction(self) -> f64 {
        self.maximum_domain_omitted_d6_fraction
    }
}

impl Default for PsdIntegrationConfig {
    fn default() -> Self {
        Self::new(
            8,
            256,
            96.0,
            1.0e-10,
            5.0e-8,
            5.0e-3,
            DEFAULT_ADDITIVE_ABSOLUTE_TOLERANCES,
            1.0e-6,
            1.0e-6,
            1.0e-6,
        )
        .expect("the versioned built-in PSD integration config is valid")
    }
}

/// Complete numerical, truncation, and table-domain audit for one result.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct PsdIntegrationAudit {
    pub config: PsdIntegrationConfig,
    pub support: PsdParticleSupport,
    pub revision: SchemePsdRevision,
    pub quadrature: PsdQuadratureRule,
    pub fall_speed: PsdFallSpeedProvenance,
    pub reconstruction: IshmaelReconstructionAudit,
    /// Number of magnitude-triggered factor-of-two refinements beyond the
    /// configured base coarse/refined pair.
    pub refinement_steps: u8,
    /// Node counts in the comparison pair that satisfied every convergence
    /// gate (or would have been reported by a final fail-closed error).
    pub coarse_nodes_evaluated: usize,
    pub refined_nodes_evaluated: usize,
    /// Total scattering callbacks consumed, including a rejected base grid
    /// when adaptive refinement was required.
    pub total_nodes_reduced: usize,
    pub upper_scaled_a: f64,
    pub expected_number_density_m3: f64,
    pub expected_mass_concentration_kg_m3: f64,
    pub expected_d6_concentration_m3: f64,
    pub represented_number_fraction: f64,
    pub represented_mass_fraction: f64,
    pub represented_d6_fraction: f64,
    pub domain_omitted_number_fraction: f64,
    pub domain_omitted_mass_fraction: f64,
    pub domain_omitted_d6_fraction: f64,
    pub truncation_tail_number_fraction: f64,
    pub truncation_tail_mass_fraction: f64,
    pub truncation_tail_d6_fraction: f64,
    pub number_closure_relative_error: f64,
    pub mass_closure_relative_error: f64,
    pub d6_closure_relative_error: f64,
    pub maximum_additive_convergence_error: f64,
    pub maximum_additive_convergence_component: usize,
}

/// Additive and application-accumulator forms of one integrated PSD.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PsdIntegrationResult {
    additive: AdditiveScattering,
    accumulator: PolarAccumulatorQuantities,
    audit: PsdIntegrationAudit,
}

impl PsdIntegrationResult {
    #[must_use]
    pub const fn additive(self) -> AdditiveScattering {
        self.additive
    }

    #[must_use]
    pub const fn accumulator(self) -> PolarAccumulatorQuantities {
        self.accumulator
    }

    #[must_use]
    pub const fn audit(self) -> PsdIntegrationAudit {
        self.audit
    }
}

/// Opaque, fully constructed ISHMAEL coarse/refined scattering workload.
///
/// Preparation fixes both quadrature grids, their original per-rule node
/// indices, support admission, and every analytic closure/audit quantity. It
/// does not evaluate scattering. A backend may therefore stage the exposed
/// ordered nodes while [`Self::finish`] remains the sole owner of population
/// scaling, serial reduction, convergence gates, and final output validation.
#[derive(Clone, Debug, PartialEq)]
pub struct PreparedIshmaelPsdIntegration {
    distribution: IshmaelPsd,
    config: PsdIntegrationConfig,
    support: PsdParticleSupport,
    fall_speed: PsdFallSpeedProvenance,
    coarse: PreparedPsdRule,
    refined: PreparedPsdRule,
    adaptive_refined: Option<PreparedPsdRule>,
    upper_scaled_a: f64,
    represented: MomentFractions,
    omitted: MomentFractions,
    tails: MomentFractions,
}

impl PreparedIshmaelPsdIntegration {
    /// Ordered supported nodes in the coarse convergence pass.
    pub fn coarse_nodes(
        &self,
    ) -> impl ExactSizeIterator<Item = (usize, PsdParticleNode)> + DoubleEndedIterator + '_ {
        self.coarse
            .nodes
            .iter()
            .copied()
            .map(|node| (node.index(), node))
    }

    /// Ordered supported nodes in the refined convergence pass.
    pub fn refined_nodes(
        &self,
    ) -> impl ExactSizeIterator<Item = (usize, PsdParticleNode)> + DoubleEndedIterator + '_ {
        self.refined
            .nodes
            .iter()
            .copied()
            .map(|node| (node.index(), node))
    }

    /// Ordered supported nodes in the optional magnitude-triggered retry.
    pub fn adaptive_refined_nodes(
        &self,
    ) -> impl DoubleEndedIterator<Item = (usize, PsdParticleNode)> + '_ {
        self.adaptive_refined
            .iter()
            .flat_map(|rule| rule.nodes.iter())
            .copied()
            .map(|node| (node.index(), node))
    }

    /// Complete possible callback order: base coarse, base refined, then the
    /// optional adaptive-refined rule. Level and original per-grid index
    /// remain explicit even when table support excludes intervening nodes.
    pub fn nodes(&self) -> impl Iterator<Item = (PsdQuadratureLevel, usize, PsdParticleNode)> + '_ {
        self.coarse_nodes()
            .map(|(index, node)| (PsdQuadratureLevel::Coarse, index, node))
            .chain(
                self.refined_nodes()
                    .map(|(index, node)| (PsdQuadratureLevel::Refined, index, node)),
            )
            .chain(
                self.adaptive_refined_nodes()
                    .map(|(index, node)| (PsdQuadratureLevel::AdaptiveRefined, index, node)),
            )
    }

    #[must_use]
    pub fn node_count(&self, level: PsdQuadratureLevel) -> usize {
        match level {
            PsdQuadratureLevel::Coarse => self.coarse.nodes.len(),
            PsdQuadratureLevel::Refined => self.refined.nodes.len(),
            PsdQuadratureLevel::AdaptiveRefined => self
                .adaptive_refined
                .as_ref()
                .map_or(0, |rule| rule.nodes.len()),
        }
    }

    #[must_use]
    pub const fn upper_scaled_a(&self) -> f64 {
        self.upper_scaled_a
    }

    /// Analytic/quadrature closure errors already fixed by preparation. The
    /// configured fail-closed limits are deliberately enforced only in
    /// [`Self::finish`], matching the original post-evaluation ordering.
    #[must_use]
    pub fn closure_relative_errors(&self) -> [f64; 3] {
        quadrature_closure_errors(&self.refined, self.tails)
    }

    /// Consume the base pair in scalar order, then consume the optional third
    /// rule only when the measured base result fails its numerical audit.
    pub fn finish<F, E>(
        &self,
        mut evaluate_particle: F,
    ) -> Result<PsdIntegrationResult, PsdIntegrationError<E>>
    where
        F: FnMut(PsdQuadratureLevel, usize, &PsdParticleNode) -> Result<AdditiveScattering, E>,
        E: Error + 'static,
    {
        let base_coarse = finish_prepared_rule(
            &self.coarse,
            PsdQuadratureLevel::Coarse,
            &mut evaluate_particle,
        )?;
        let base_refined = finish_prepared_rule(
            &self.refined,
            PsdQuadratureLevel::Refined,
            &mut evaluate_particle,
        )?;

        for (moment, omitted, maximum) in [
            (
                "number",
                self.omitted.number,
                self.config.maximum_domain_omitted_number_fraction,
            ),
            (
                "mass",
                self.omitted.mass,
                self.config.maximum_domain_omitted_mass_fraction,
            ),
            (
                "equivalent-volume D6",
                self.omitted.d6,
                self.config.maximum_domain_omitted_d6_fraction,
            ),
        ] {
            if omitted > maximum {
                return Err(PsdError::DomainOmission {
                    moment,
                    fraction: omitted,
                    maximum,
                }
                .into());
            }
        }

        // The retry is triggered only by a failed numerical audit computed
        // from the actual integrated additive magnitudes. It neither raises a
        // tolerance nor accepts a result that has not independently passed
        // the same absolute-plus-relative gate on the finer pair.
        let base_audit = quadrature_candidate_audit(
            self.config,
            &self.refined,
            self.tails,
            base_coarse,
            base_refined,
        );
        let (coarse, refined, convergence, refinement_steps, total_nodes_reduced) = match base_audit
        {
            Ok(convergence) => (
                base_coarse,
                base_refined,
                convergence,
                0,
                base_coarse.nodes_evaluated + base_refined.nodes_evaluated,
            ),
            Err(base_error) => {
                let Some(adaptive_rule) = self.adaptive_refined.as_ref() else {
                    return Err(base_error.into());
                };
                let adaptive_refined = finish_prepared_rule(
                    adaptive_rule,
                    PsdQuadratureLevel::AdaptiveRefined,
                    &mut evaluate_particle,
                )?;
                let convergence = quadrature_candidate_audit(
                    self.config,
                    adaptive_rule,
                    self.tails,
                    base_refined,
                    adaptive_refined,
                )?;
                (
                    base_refined,
                    adaptive_refined,
                    convergence,
                    1,
                    base_coarse.nodes_evaluated
                        + base_refined.nodes_evaluated
                        + adaptive_refined.nodes_evaluated,
                )
            }
        };

        let accumulator = refined
            .additive
            .to_polar_accumulator_quantities()
            .map_err(PsdIntegrationError::Output)?;
        let audit = PsdIntegrationAudit {
            config: self.config,
            support: self.support,
            revision: self.config.revision,
            quadrature: self.config.quadrature,
            fall_speed: self.fall_speed,
            reconstruction: self.distribution.reconstruction,
            refinement_steps,
            coarse_nodes_evaluated: coarse.nodes_evaluated,
            refined_nodes_evaluated: refined.nodes_evaluated,
            total_nodes_reduced,
            upper_scaled_a: self.upper_scaled_a,
            expected_number_density_m3: self.distribution.number_density_m3(),
            expected_mass_concentration_kg_m3: self.distribution.mass_concentration_kg_m3(),
            expected_d6_concentration_m3: self.distribution.number_density_m3()
                * self.distribution.mean_equivolume_diameter_sixth_m6,
            represented_number_fraction: self.represented.number,
            represented_mass_fraction: self.represented.mass,
            represented_d6_fraction: self.represented.d6,
            domain_omitted_number_fraction: self.omitted.number,
            domain_omitted_mass_fraction: self.omitted.mass,
            domain_omitted_d6_fraction: self.omitted.d6,
            truncation_tail_number_fraction: self.tails.number,
            truncation_tail_mass_fraction: self.tails.mass,
            truncation_tail_d6_fraction: self.tails.d6,
            number_closure_relative_error: convergence.closure_errors[0],
            mass_closure_relative_error: convergence.closure_errors[1],
            d6_closure_relative_error: convergence.closure_errors[2],
            maximum_additive_convergence_error: convergence.maximum_additive_error,
            maximum_additive_convergence_component: convergence.maximum_additive_component,
        };
        Ok(PsdIntegrationResult {
            additive: refined.additive,
            accumulator,
            audit,
        })
    }
}

/// Construct the ordered ISHMAEL convergence grids without evaluating any
/// scattering. In addition to the required base pair, preparation retains one
/// optional factor-of-two refinement when it fits the explicit node budget.
/// Finishing consumes that rule only when the base pair fails its numerical
/// audit against the actual integrated scattering magnitude.
pub fn prepare_ishmael_psd(
    distribution: &IshmaelPsd,
    config: PsdIntegrationConfig,
    support: PsdParticleSupport,
    fall_speed: PsdFallSpeedProvenance,
) -> Result<PreparedIshmaelPsdIntegration, PsdError> {
    let upper_scaled_a = tail_cutoff(*distribution, config)?;
    let support_intervals = distribution.support_intervals(support, upper_scaled_a);
    let tails = MomentFractions {
        number: regularized_gamma_q(ISHMAEL_GAMMA_SHAPE, upper_scaled_a)?,
        mass: regularized_gamma_q(
            ISHMAEL_GAMMA_SHAPE + 2.0 + distribution.aspect_power_delta,
            upper_scaled_a,
        )?,
        d6: regularized_gamma_q(
            ISHMAEL_GAMMA_SHAPE + 4.0 + 2.0 * distribution.aspect_power_delta,
            upper_scaled_a,
        )?,
    };
    let represented = exact_interval_fractions(*distribution, &support_intervals)?;
    let omitted = MomentFractions {
        number: (1.0 - tails.number - represented.number).max(0.0),
        mass: (1.0 - tails.mass - represented.mass).max(0.0),
        d6: (1.0 - tails.d6 - represented.d6).max(0.0),
    };

    let coarse = prepare_rule(
        *distribution,
        usize::from(config.coarse_panels),
        upper_scaled_a,
        support,
        &support_intervals,
        config.maximum_refined_nodes as usize,
    )?;
    let refined = prepare_rule(
        *distribution,
        usize::from(config.coarse_panels) * REFINEMENT_FACTOR,
        upper_scaled_a,
        support,
        &support_intervals,
        config.maximum_refined_nodes as usize,
    )?;
    let adaptive_refined = match prepare_rule(
        *distribution,
        usize::from(config.coarse_panels) * REFINEMENT_FACTOR * REFINEMENT_FACTOR,
        upper_scaled_a,
        support,
        &support_intervals,
        config.maximum_refined_nodes as usize,
    ) {
        Ok(rule) => Some(rule),
        Err(PsdError::NodeBudgetExceeded { .. }) => None,
        Err(error) => return Err(error),
    };

    Ok(PreparedIshmaelPsdIntegration {
        distribution: *distribution,
        config,
        support,
        fall_speed,
        coarse,
        refined,
        adaptive_refined,
        upper_scaled_a,
        represented,
        omitted,
        tails,
    })
}

/// Integrate an ISHMAEL PSD through a caller-supplied per-particle operator.
///
/// `evaluate_particle` must return all nine additive quantities normalized to
/// exactly one particle per cubic metre. It is called for both the coarse and
/// refined rules so the returned science carries an actual scattering-space
/// convergence audit, not only a gamma-moment check. The callback must be
/// deterministic and scientifically side-effect-free because those two rules
/// use different node grids.
pub fn integrate_ishmael_psd<F, E>(
    distribution: &IshmaelPsd,
    config: PsdIntegrationConfig,
    support: PsdParticleSupport,
    fall_speed: PsdFallSpeedProvenance,
    mut evaluate_particle: F,
) -> Result<PsdIntegrationResult, PsdIntegrationError<E>>
where
    F: FnMut(&PsdParticleNode) -> Result<AdditiveScattering, E>,
    E: Error + 'static,
{
    let prepared = prepare_ishmael_psd(distribution, config, support, fall_speed)
        .map_err(PsdIntegrationError::Psd)?;
    prepared.finish(|_, _, node| evaluate_particle(node))
}

/// Analytic raw moment of a gamma number distribution.
///
/// For total number `N`, gamma shape `nu`, scale `lambda`, and power `p`, this
/// returns `N * lambda^p * Gamma(nu+p)/Gamma(nu)`.
pub fn analytic_gamma_moment(
    total_number: f64,
    scale: f64,
    shape: f64,
    power: f64,
) -> Result<f64, PsdError> {
    positive("gamma total number", total_number)?;
    positive("gamma scale", scale)?;
    positive("gamma shape", shape)?;
    if !power.is_finite() || shape + power <= 0.0 {
        return Err(PsdError::InvalidInput {
            field: "gamma moment power",
            value: power,
            requirement: "finite with shape + power > 0",
        });
    }
    checked_positive_computation(
        "analytic gamma moment",
        total_number * (power * scale.ln() + ln_gamma(shape + power)? - ln_gamma(shape)?).exp(),
    )
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct MomentFractions {
    number: f64,
    mass: f64,
    d6: f64,
}

#[derive(Clone, Debug, PartialEq)]
struct PreparedPsdRule {
    /// Only nodes admitted by the explicit table-support contract require a
    /// scattering result. Their `PsdParticleNode::index` values still refer to
    /// the complete quadrature grid, including unsupported intervening nodes.
    nodes: Vec<PsdParticleNode>,
    all: MomentFractions,
}

#[derive(Clone, Copy, Debug)]
struct RuleAccumulation {
    additive: AdditiveScattering,
    nodes_evaluated: usize,
}

#[derive(Clone, Copy, Debug)]
struct QuadratureCandidateAudit {
    closure_errors: [f64; 3],
    maximum_additive_error: f64,
    maximum_additive_component: usize,
}

fn quadrature_closure_errors(refined_rule: &PreparedPsdRule, tails: MomentFractions) -> [f64; 3] {
    [
        (refined_rule.all.number + tails.number - 1.0).abs(),
        (refined_rule.all.mass + tails.mass - 1.0).abs(),
        (refined_rule.all.d6 + tails.d6 - 1.0).abs(),
    ]
}

fn quadrature_candidate_audit(
    config: PsdIntegrationConfig,
    refined_rule: &PreparedPsdRule,
    tails: MomentFractions,
    coarse: RuleAccumulation,
    refined: RuleAccumulation,
) -> Result<QuadratureCandidateAudit, PsdError> {
    let closure_errors = quadrature_closure_errors(refined_rule, tails);
    for (moment, error) in [
        ("number", closure_errors[0]),
        ("mass", closure_errors[1]),
        ("equivalent-volume D6", closure_errors[2]),
    ] {
        if error > config.maximum_quadrature_closure_error {
            return Err(PsdError::QuadratureClosure {
                moment,
                relative_error: error,
                maximum: config.maximum_quadrature_closure_error,
            });
        }
    }

    let coarse_components = coarse.additive.components();
    let refined_components = refined.additive.components();
    let mut maximum_additive_error = 0.0_f64;
    let mut maximum_additive_component = 0;
    for index in 0..AdditiveScattering::COMPONENT_COUNT {
        let absolute_error = (coarse_components[index] - refined_components[index]).abs();
        let magnitude = coarse_components[index]
            .abs()
            .max(refined_components[index].abs());
        let allowed = config.additive_absolute_tolerances[index]
            + config.maximum_additive_convergence_error * magnitude;
        let error = relative_error(
            coarse_components[index],
            refined_components[index],
            config.additive_absolute_tolerances[index],
        );
        if error > maximum_additive_error {
            maximum_additive_error = error;
            maximum_additive_component = index;
        }
        if absolute_error > allowed {
            return Err(PsdError::AdditiveConvergence {
                component: index,
                coarse_value: coarse_components[index],
                refined_value: refined_components[index],
                magnitude,
                absolute_error,
                relative_error: error,
                absolute_tolerance: config.additive_absolute_tolerances[index],
                relative_tolerance: config.maximum_additive_convergence_error,
            });
        }
    }
    Ok(QuadratureCandidateAudit {
        closure_errors,
        maximum_additive_error,
        maximum_additive_component,
    })
}

// Each argument is an independent part of quadrature construction or its
// audit context; bundling them would obscure the coarse/refined call sites.
#[allow(clippy::too_many_arguments)]
fn prepare_rule(
    distribution: IshmaelPsd,
    panels: usize,
    upper_scaled_a: f64,
    support: PsdParticleSupport,
    support_intervals: &[SupportInterval],
    maximum_nodes: usize,
) -> Result<PreparedPsdRule, PsdError> {
    let gamma_normalization = ln_gamma(ISHMAEL_GAMMA_SHAPE)?.exp();
    let panel_width = upper_scaled_a / panels as f64;
    let mut breakpoints = Vec::with_capacity(panels + 1 + 2 * support_intervals.len());
    for panel in 0..=panels {
        breakpoints.push(panel as f64 * panel_width);
    }
    for interval in support_intervals {
        breakpoints.push(interval.lower);
        breakpoints.push(interval.upper);
    }
    breakpoints.sort_by(f64::total_cmp);
    breakpoints.dedup_by(|left, right| {
        (*left - *right).abs() <= 32.0 * f64::EPSILON * left.abs().max(right.abs()).max(1.0)
    });
    let required_nodes = breakpoints
        .len()
        .saturating_sub(1)
        .checked_mul(GL8_POINTS_PER_PANEL)
        .ok_or(PsdError::NodeBudgetOverflow)?;
    if required_nodes > maximum_nodes {
        return Err(PsdError::NodeBudgetExceeded {
            required: required_nodes,
            maximum: maximum_nodes,
        });
    }
    let mut nodes = Vec::with_capacity(required_nodes);
    let mut all = MomentFractions::default();
    let mut index = 0;

    for segment in breakpoints.windows(2) {
        let half_width = 0.5 * (segment[1] - segment[0]);
        let midpoint = 0.5 * (segment[0] + segment[1]);
        for local in 0..GL8_POINTS_PER_PANEL {
            let scaled_a = midpoint + half_width * GL8_ABSCISSAE[local];
            let dx_weight = half_width * GL8_WEIGHTS[local];
            let gamma_pdf =
                scaled_a.powf(ISHMAEL_GAMMA_SHAPE - 1.0) * (-scaled_a).exp() / gamma_normalization;
            let number_fraction = checked_positive_computation(
                "ISHMAEL quadrature number fraction",
                dx_weight * gamma_pdf,
            )?;
            let node = distribution.node(index, scaled_a, number_fraction)?;
            let mass_fraction =
                number_fraction * node.particle_mass_kg / distribution.mean_particle_mass_kg;
            let d6_fraction = number_fraction * node.equivolume_diameter_m.powi(6)
                / distribution.mean_equivolume_diameter_sixth_m6;
            let fractions = MomentFractions {
                number: number_fraction,
                mass: mass_fraction,
                d6: d6_fraction,
            };
            add_fractions(&mut all, fractions);

            if support.contains(node) {
                nodes.push(node);
            }
            index += 1;
        }
    }

    Ok(PreparedPsdRule { nodes, all })
}

fn finish_prepared_rule<F, E>(
    prepared: &PreparedPsdRule,
    level: PsdQuadratureLevel,
    evaluate_particle: &mut F,
) -> Result<RuleAccumulation, PsdIntegrationError<E>>
where
    F: FnMut(PsdQuadratureLevel, usize, &PsdParticleNode) -> Result<AdditiveScattering, E>,
    E: Error + 'static,
{
    let mut additive = AdditiveScattering::default();
    for node in &prepared.nodes {
        let node_index = node.index();
        let per_particle = evaluate_particle(level, node_index, node).map_err(|source| {
            PsdIntegrationError::NodeEvaluation {
                level,
                node_index,
                source,
            }
        })?;
        let contribution = per_particle
            .checked_scale(node.number_density_m3)
            .map_err(PsdIntegrationError::Output)?;
        additive = additive
            .checked_add(contribution)
            .map_err(PsdIntegrationError::Output)?;
    }
    Ok(RuleAccumulation {
        additive,
        nodes_evaluated: prepared.nodes.len(),
    })
}

fn exact_interval_fractions(
    distribution: IshmaelPsd,
    intervals: &[SupportInterval],
) -> Result<MomentFractions, PsdError> {
    let shapes = [
        ISHMAEL_GAMMA_SHAPE,
        ISHMAEL_GAMMA_SHAPE + 2.0 + distribution.aspect_power_delta,
        ISHMAEL_GAMMA_SHAPE + 4.0 + 2.0 * distribution.aspect_power_delta,
    ];
    let mut fractions = [0.0_f64; 3];
    for interval in intervals {
        for (index, shape) in shapes.into_iter().enumerate() {
            fractions[index] += regularized_gamma_q(shape, interval.lower)?
                - regularized_gamma_q(shape, interval.upper)?;
        }
    }
    Ok(MomentFractions {
        number: fractions[0].clamp(0.0, 1.0),
        mass: fractions[1].clamp(0.0, 1.0),
        d6: fractions[2].clamp(0.0, 1.0),
    })
}

fn add_fractions(target: &mut MomentFractions, value: MomentFractions) {
    target.number += value.number;
    target.mass += value.mass;
    target.d6 += value.d6;
}

fn tail_cutoff(distribution: IshmaelPsd, config: PsdIntegrationConfig) -> Result<f64, PsdError> {
    let shapes = [
        ISHMAEL_GAMMA_SHAPE,
        ISHMAEL_GAMMA_SHAPE + 2.0 + distribution.aspect_power_delta,
        ISHMAEL_GAMMA_SHAPE + 4.0 + 2.0 * distribution.aspect_power_delta,
    ];
    let exceeds = |x: f64| -> Result<bool, PsdError> {
        for shape in shapes {
            if regularized_gamma_q(shape, x)? > config.maximum_tail_fraction {
                return Ok(true);
            }
        }
        Ok(false)
    };

    let mut upper = shapes.into_iter().fold(1.0_f64, f64::max);
    while upper < config.maximum_scaled_a && exceeds(upper)? {
        upper = (2.0 * upper).min(config.maximum_scaled_a);
    }
    if exceeds(upper)? {
        return Err(PsdError::TailToleranceUnreachable {
            maximum_scaled_a: config.maximum_scaled_a,
            requested_fraction: config.maximum_tail_fraction,
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

fn mean_particle_volume(a_scale_m: f64, delta: f64, gamma_shape: f64) -> Result<f64, PsdError> {
    checked_positive_computation(
        "ISHMAEL mean particle volume",
        (4.0 / 3.0)
            * PI
            * ISHMAEL_MONOMER_SEMI_AXIS_M.powf(1.0 - delta)
            * analytic_gamma_moment(1.0, a_scale_m, gamma_shape, 2.0 + delta)?,
    )
}

fn regularized_gamma_q(shape: f64, x: f64) -> Result<f64, PsdError> {
    positive("regularized-gamma shape", shape)?;
    if !x.is_finite() || x < 0.0 {
        return Err(PsdError::InvalidInput {
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
            return Err(PsdError::NumericalConvergence {
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
            return Err(PsdError::NumericalConvergence {
                operation: "regularized upper incomplete gamma continued fraction",
            });
        }
        log_prefactor.exp() * h
    };
    if !q.is_finite() || !(-1.0e-13..=1.0 + 1.0e-13).contains(&q) {
        return Err(PsdError::InvalidComputation {
            field: "regularized upper incomplete gamma",
            value: q,
        });
    }
    Ok(q.clamp(0.0, 1.0))
}

fn ln_gamma(value: f64) -> Result<f64, PsdError> {
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
        Err(PsdError::InvalidComputation {
            field: "log gamma",
            value: result,
        })
    }
}

fn positive(field: &'static str, value: f64) -> Result<(), PsdError> {
    if value.is_finite() && value > 0.0 {
        Ok(())
    } else {
        Err(PsdError::InvalidInput {
            field,
            value,
            requirement: "finite and positive",
        })
    }
}

fn tolerance(field: &'static str, value: f64) -> Result<(), PsdError> {
    if value.is_finite() && 0.0 < value && value < 1.0 {
        Ok(())
    } else {
        Err(PsdError::InvalidInput {
            field,
            value,
            requirement: "finite and strictly between zero and one",
        })
    }
}

fn source_bounded(
    field: &'static str,
    value: f64,
    range: [f64; 2],
) -> Result<(f64, f64), PsdError> {
    let tolerance = SOURCE_BOUND_RELATIVE_TOLERANCE * range[0].abs().max(range[1].abs()).max(1.0);
    if value.is_finite() && range[0] - tolerance <= value && value <= range[1] + tolerance {
        let canonical = value.clamp(range[0], range[1]);
        Ok((canonical, value - canonical))
    } else {
        Err(PsdError::OutsideReconstructionBound {
            field,
            value,
            minimum: range[0],
            maximum: range[1],
        })
    }
}

fn positive_range(field: &'static str, range: [f64; 2]) -> Result<(), PsdError> {
    if range[0].is_finite() && range[1].is_finite() && range[0] > 0.0 && range[1] >= range[0] {
        Ok(())
    } else {
        Err(PsdError::InvalidRange {
            field,
            minimum: range[0],
            maximum: range[1],
        })
    }
}

fn checked_positive_computation(field: &'static str, value: f64) -> Result<f64, PsdError> {
    if value.is_finite() && value > 0.0 {
        Ok(value)
    } else {
        Err(PsdError::InvalidComputation { field, value })
    }
}

fn contains(range: [f64; 2], value: f64) -> bool {
    value.is_finite() && range[0] <= value && value <= range[1]
}

fn relative_error(left: f64, right: f64, absolute_floor: f64) -> f64 {
    (left - right).abs() / left.abs().max(right.abs()).max(absolute_floor)
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum PsdError {
    #[error("{field} must be {requirement}, got {value}")]
    InvalidInput {
        field: &'static str,
        value: f64,
        requirement: &'static str,
    },
    #[error("{field} range must be finite, positive, and ordered, got [{minimum}, {maximum}]")]
    InvalidRange {
        field: &'static str,
        minimum: f64,
        maximum: f64,
    },
    #[error("{field} produced an invalid nonpositive or nonfinite value {value}")]
    InvalidComputation { field: &'static str, value: f64 },
    #[error("{field} value {value} is outside [{minimum}, {maximum}]")]
    OutsideReconstructionBound {
        field: &'static str,
        value: f64,
        minimum: f64,
        maximum: f64,
    },
    #[error("native {moment} reconstruction relative error {relative_error} exceeds {maximum}")]
    ReconstructionClosure {
        moment: &'static str,
        relative_error: f64,
        maximum: f64,
    },
    #[error("{field} must be positive, got {value}")]
    InvalidIntegerConfig { field: &'static str, value: u64 },
    #[error("quadrature node-budget arithmetic overflowed")]
    NodeBudgetOverflow,
    #[error("refined quadrature needs {required} nodes but the configured maximum is {maximum}")]
    NodeBudgetExceeded { required: usize, maximum: usize },
    #[error(
        "gamma tails cannot reach fraction {requested_fraction} by scaled a={maximum_scaled_a}"
    )]
    TailToleranceUnreachable {
        maximum_scaled_a: f64,
        requested_fraction: f64,
    },
    #[error("{operation} did not converge within the fixed iteration limit")]
    NumericalConvergence { operation: &'static str },
    #[error("{moment} quadrature closure error {relative_error} exceeds {maximum}")]
    QuadratureClosure {
        moment: &'static str,
        relative_error: f64,
        maximum: f64,
    },
    #[error("table domain omits {fraction} of PSD {moment}, exceeding {maximum}")]
    DomainOmission {
        moment: &'static str,
        fraction: f64,
        maximum: f64,
    },
    #[error(
        "additive component {component} coarse value {coarse_value}, refined value {refined_value}, magnitude {magnitude}, absolute error {absolute_error}, relative error {relative_error} exceeds abs_tol={absolute_tolerance} + rel_tol={relative_tolerance} * magnitude"
    )]
    AdditiveConvergence {
        component: usize,
        coarse_value: f64,
        refined_value: f64,
        magnitude: f64,
        absolute_error: f64,
        relative_error: f64,
        absolute_tolerance: f64,
        relative_tolerance: f64,
    },
}

/// PSD failures preserve the concrete scattering-callback error type.
#[derive(Debug)]
pub enum PsdIntegrationError<E> {
    Psd(PsdError),
    NodeEvaluation {
        level: PsdQuadratureLevel,
        node_index: usize,
        source: E,
    },
    Output(OutputError),
}

impl<E> From<PsdError> for PsdIntegrationError<E> {
    fn from(value: PsdError) -> Self {
        Self::Psd(value)
    }
}

impl<E> From<OutputError> for PsdIntegrationError<E> {
    fn from(value: OutputError) -> Self {
        Self::Output(value)
    }
}

impl<E: Display> Display for PsdIntegrationError<E> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Psd(error) => Display::fmt(error, formatter),
            Self::NodeEvaluation {
                level,
                node_index,
                source,
            } => write!(
                formatter,
                "{level:?} PSD node {node_index} scattering evaluation failed: {source}"
            ),
            Self::Output(error) => write!(formatter, "integrated additive output failed: {error}"),
        }
    }
}

impl<E: Error + 'static> Error for PsdIntegrationError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Psd(error) => Some(error),
            Self::NodeEvaluation { source, .. } => Some(source),
            Self::Output(error) => Some(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{convert::Infallible, error::Error, fmt};

    use super::*;

    fn assert_relative(actual: f64, expected: f64, tolerance: f64) {
        let error = relative_error(actual, expected, f64::MIN_POSITIVE);
        assert!(
            error <= tolerance,
            "actual={actual:e}, expected={expected:e}, relative error={error:e}"
        );
    }

    fn input_from_scales(
        category: IshmaelIceCategory,
        a_scale_m: f64,
        c_at_a_scale_m: f64,
        density_kg_m3: f64,
    ) -> IshmaelPsdInput {
        let number_per_kg = 1.0e5;
        let delta = (c_at_a_scale_m / ISHMAEL_MONOMER_SEMI_AXIS_M).ln()
            / (a_scale_m / ISHMAEL_MONOMER_SEMI_AXIS_M).ln();
        let mean_volume = mean_particle_volume(a_scale_m, delta, ISHMAEL_GAMMA_SHAPE).unwrap();
        let qice = number_per_kg * density_kg_m3 * mean_volume;
        IshmaelPsdInput::new(
            category,
            qice,
            number_per_kg,
            number_per_kg * a_scale_m * a_scale_m * c_at_a_scale_m,
            number_per_kg * a_scale_m * c_at_a_scale_m * c_at_a_scale_m,
            1.2,
        )
    }

    fn oblate_distribution() -> IshmaelPsd {
        IshmaelPsd::reconstruct(input_from_scales(
            IshmaelIceCategory::Planar,
            50.0e-6,
            25.0e-6,
            400.0,
        ))
        .unwrap()
    }

    fn synthetic_per_particle(node: &PsdParticleNode) -> AdditiveScattering {
        let diameter = node.equivolume_diameter_m();
        let zh = 1.0e-5 * (1.0 + 1.0e8 * diameter * diameter);
        let zv = 0.8 * zh;
        let speed = 0.25 + 1_000.0 * diameter;
        AdditiveScattering::from_components([
            zh,
            zv,
            0.7 * zh,
            0.0,
            1.0e-8 * diameter / 1.0e-4,
            1.0e-10,
            8.0e-11,
            zh * speed,
            zh * speed * speed,
        ])
        .unwrap()
    }

    fn synthetic_fall_speed_provenance() -> PsdFallSpeedProvenance {
        PsdFallSpeedProvenance::new(
            PsdFallSpeedAuthority::SyntheticTestOnly,
            Sha256Digest::compute(b"scheme-psd-test-size-speed-v1"),
        )
    }

    #[test]
    fn analytic_gamma_moments_match_integer_identities() {
        let scale = 2.5e-5;
        assert_relative(
            analytic_gamma_moment(7.0, scale, 4.0, 0.0).unwrap(),
            7.0,
            1.0e-14,
        );
        assert_relative(
            analytic_gamma_moment(7.0, scale, 4.0, 1.0).unwrap(),
            7.0 * 4.0 * scale,
            1.0e-14,
        );
        assert_relative(
            analytic_gamma_moment(7.0, scale, 4.0, 2.0).unwrap(),
            7.0 * 20.0 * scale * scale,
            1.0e-14,
        );
    }

    #[test]
    fn regularized_gamma_tail_matches_integer_closed_form() {
        let x: f64 = 7.25;
        let expected = (-x).exp() * (1.0 + x + x * x / 2.0 + x * x * x / 6.0);
        assert_relative(regularized_gamma_q(4.0, x).unwrap(), expected, 2.0e-14);
    }

    #[test]
    fn ishmael_reconstruction_closes_native_axes_number_and_mass() {
        let distribution = oblate_distribution();
        assert_relative(distribution.a_scale_m(), 50.0e-6, 2.0e-14);
        assert_relative(distribution.c_at_a_scale_m(), 25.0e-6, 2.0e-14);
        assert_relative(distribution.bulk_density_kg_m3(), 400.0, 2.0e-12);
        assert_relative(distribution.number_density_m3(), 1.2 * 1.0e5, 1.0e-15);
        let audit = distribution.reconstruction_audit();
        assert!(audit.qvoli_relative_error <= RECONSTRUCTION_RELATIVE_TOLERANCE);
        assert!(audit.qaoli_relative_error <= RECONSTRUCTION_RELATIVE_TOLERANCE);
        assert!(audit.mass_relative_error <= RECONSTRUCTION_RELATIVE_TOLERANCE);
        assert_eq!(audit.delta_bound_excursion, 0.0);
        assert_eq!(audit.density_bound_excursion_kg_m3, 0.0);
    }

    #[test]
    fn hard_coded_wrf_equation_golden_tuple_reconstructs_expected_state() {
        // Independently evaluated from the equations in
        // module_mp_jensen_ishmael.F for nu=4 and a0=0.1 micrometre.
        let distribution = IshmaelPsd::reconstruct(IshmaelPsdInput::new(
            IshmaelIceCategory::Planar,
            1.020_730_388_452_175_2e-3,
            100_000.0,
            6.250_000_000_000_000_5e-9,
            3.125_000_000_000_000_3e-9,
            1.2,
        ))
        .unwrap();
        assert_relative(distribution.a_scale_m(), 5.0e-5, 3.0e-15);
        assert_relative(distribution.c_at_a_scale_m(), 2.5e-5, 3.0e-15);
        assert_relative(
            distribution.aspect_power_delta(),
            0.888_464_860_602_243_7,
            3.0e-15,
        );
        assert_relative(distribution.bulk_density_kg_m3(), 400.0, 3.0e-14);
        assert_relative(
            distribution.mean_particle_mass_kg(),
            1.020_730_388_452_175_3e-8,
            3.0e-15,
        );
        assert_relative(
            distribution.mean_equivolume_diameter_sixth_m6(),
            9.173_845_314_887_259e-21,
            3.0e-14,
        );
    }

    #[test]
    fn node_habit_comes_from_geometry_not_category_label() {
        let oblate = oblate_distribution();
        let prolate = IshmaelPsd::reconstruct(input_from_scales(
            IshmaelIceCategory::Planar,
            50.0e-6,
            100.0e-6,
            400.0,
        ))
        .unwrap();
        let mut saw_oblate = false;
        integrate_ishmael_psd(
            &oblate,
            PsdIntegrationConfig::default(),
            PsdParticleSupport::default(),
            synthetic_fall_speed_provenance(),
            |node| {
                saw_oblate |= node.habit() == PsdSpheroidHabit::Oblate;
                Ok::<_, Infallible>(synthetic_per_particle(node))
            },
        )
        .unwrap();
        let mut saw_prolate = false;
        integrate_ishmael_psd(
            &prolate,
            PsdIntegrationConfig::default(),
            PsdParticleSupport::default(),
            synthetic_fall_speed_provenance(),
            |node| {
                saw_prolate |= node.habit() == PsdSpheroidHabit::Prolate;
                Ok::<_, Infallible>(synthetic_per_particle(node))
            },
        )
        .unwrap();
        assert!(saw_oblate);
        assert!(saw_prolate);
    }

    #[test]
    fn equal_native_axes_produce_spherical_nodes() {
        let spherical = IshmaelPsd::reconstruct(input_from_scales(
            IshmaelIceCategory::Aggregate,
            50.0e-6,
            50.0e-6,
            400.0,
        ))
        .unwrap();
        let mut saw_non_spherical = false;
        integrate_ishmael_psd(
            &spherical,
            PsdIntegrationConfig::default(),
            PsdParticleSupport::default(),
            synthetic_fall_speed_provenance(),
            |node| {
                saw_non_spherical |= node.habit() != PsdSpheroidHabit::Spherical;
                Ok::<_, Infallible>(synthetic_per_particle(node))
            },
        )
        .unwrap();
        assert!(!saw_non_spherical);
    }

    #[test]
    fn refined_integration_closes_number_mass_d6_and_preserves_tail_audit() {
        let distribution = oblate_distribution();
        let result = integrate_ishmael_psd(
            &distribution,
            PsdIntegrationConfig::default(),
            PsdParticleSupport::default(),
            synthetic_fall_speed_provenance(),
            |node| Ok::<_, Infallible>(synthetic_per_particle(node)),
        )
        .unwrap();
        let audit = result.audit();
        assert_eq!(audit.revision, SchemePsdRevision::IshmaelGammaV1);
        assert_eq!(
            audit.quadrature,
            PsdQuadratureRule::CompositeGaussLegendre8AdaptiveRefinedV2
        );
        assert_eq!(audit.fall_speed, synthetic_fall_speed_provenance());
        assert!(audit.number_closure_relative_error <= 5.0e-8);
        assert!(audit.mass_closure_relative_error <= 5.0e-8);
        assert!(audit.d6_closure_relative_error <= 5.0e-8);
        assert_relative(
            audit.represented_number_fraction + audit.truncation_tail_number_fraction,
            1.0,
            5.0e-8,
        );
        assert_relative(
            audit.represented_mass_fraction + audit.truncation_tail_mass_fraction,
            1.0,
            5.0e-8,
        );
        assert_relative(
            audit.represented_d6_fraction + audit.truncation_tail_d6_fraction,
            1.0,
            5.0e-8,
        );
        assert_eq!(audit.domain_omitted_number_fraction, 0.0);
        assert_eq!(audit.domain_omitted_mass_fraction, 0.0);
        assert_eq!(audit.domain_omitted_d6_fraction, 0.0);
        assert!(audit.coarse_nodes_evaluated >= 64);
        assert!(audit.refined_nodes_evaluated >= 128);
        assert!(audit.refined_nodes_evaluated > audit.coarse_nodes_evaluated);
        assert!(audit.refined_nodes_evaluated <= 256);
    }

    #[test]
    fn size_dependent_fall_moments_produce_nonzero_variance() {
        let result = integrate_ishmael_psd(
            &oblate_distribution(),
            PsdIntegrationConfig::default(),
            PsdParticleSupport::default(),
            synthetic_fall_speed_provenance(),
            |node| Ok::<_, Infallible>(synthetic_per_particle(node)),
        )
        .unwrap();
        assert!(result.accumulator().fall_speed_mps > 0.0);
        assert!(result.accumulator().fall_speed_variance_m2s2 > 0.0);
    }

    #[test]
    fn narrow_particle_domain_fails_instead_of_clamping_nodes() {
        let domain =
            PsdParticleDomain::new([1.0e-12, 1.0e-7], [50.0, 920.0], [f64::MIN_POSITIVE, 1.0])
                .unwrap();
        let error = integrate_ishmael_psd(
            &oblate_distribution(),
            PsdIntegrationConfig::default(),
            PsdParticleSupport::uniform(domain),
            synthetic_fall_speed_provenance(),
            |node| Ok::<_, Infallible>(synthetic_per_particle(node)),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            PsdIntegrationError::Psd(PsdError::DomainOmission { .. })
        ));
    }

    #[test]
    fn sub_node_width_supported_sliver_cannot_hide_domain_omission() {
        let distribution = oblate_distribution();
        let diameter_lower = distribution.equivolume_diameter_at_scaled_a(1.0).unwrap();
        let diameter_upper = distribution
            .equivolume_diameter_at_scaled_a(1.000_001)
            .unwrap();
        let sliver = PsdParticleDomain::new(
            [diameter_lower, diameter_upper],
            [50.0, 920.0],
            [f64::MIN_POSITIVE, 1.0],
        )
        .unwrap();
        let error = integrate_ishmael_psd(
            &distribution,
            PsdIntegrationConfig::default(),
            PsdParticleSupport::uniform(sliver),
            synthetic_fall_speed_provenance(),
            |node| Ok::<_, Infallible>(synthetic_per_particle(node)),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            PsdIntegrationError::Psd(PsdError::DomainOmission { .. })
        ));
    }

    #[test]
    fn reconstruction_fails_closed_on_native_state_bounds() {
        let invalid_delta = input_from_scales(
            IshmaelIceCategory::Planar,
            50.0e-6,
            ISHMAEL_MONOMER_SEMI_AXIS_M * (50.0e-6 / ISHMAEL_MONOMER_SEMI_AXIS_M).powf(0.4),
            400.0,
        );
        assert!(matches!(
            IshmaelPsd::reconstruct(invalid_delta),
            Err(PsdError::OutsideReconstructionBound {
                field: "ISHMAEL delta",
                ..
            })
        ));
        let excessive_density =
            input_from_scales(IshmaelIceCategory::Aggregate, 50.0e-6, 25.0e-6, 921.0);
        assert!(matches!(
            IshmaelPsd::reconstruct(excessive_density),
            Err(PsdError::OutsideReconstructionBound {
                field: "ISHMAEL bulk density",
                ..
            })
        ));
        let mut invalid_raw =
            input_from_scales(IshmaelIceCategory::Planar, 50.0e-6, 25.0e-6, 400.0);
        invalid_raw.qvoli_m3_per_kg = 0.0;
        assert!(matches!(
            IshmaelPsd::reconstruct(invalid_raw),
            Err(PsdError::InvalidInput { field: "QVOLI", .. })
        ));
    }

    #[test]
    fn f32_scale_boundary_roundoff_is_accepted_without_losing_mass_closure() {
        let a_scale: f64 = 50.0e-6;
        let raw_delta = ISHMAEL_DELTA_RANGE[0] - 0.25 * SOURCE_BOUND_RELATIVE_TOLERANCE;
        let c_scale = ISHMAEL_MONOMER_SEMI_AXIS_M.powf(1.0 - raw_delta) * a_scale.powf(raw_delta);
        let delta_boundary = IshmaelPsd::reconstruct(input_from_scales(
            IshmaelIceCategory::Planar,
            a_scale,
            c_scale,
            400.0,
        ))
        .unwrap();
        assert_eq!(delta_boundary.aspect_power_delta(), ISHMAEL_DELTA_RANGE[0]);
        assert_relative(
            delta_boundary.reconstruction_audit().delta_bound_excursion,
            raw_delta - ISHMAEL_DELTA_RANGE[0],
            2.0e-12,
        );

        let density_excursion =
            0.25 * SOURCE_BOUND_RELATIVE_TOLERANCE * ISHMAEL_DENSITY_RANGE_KG_M3[1];
        let density_boundary = IshmaelPsd::reconstruct(input_from_scales(
            IshmaelIceCategory::Aggregate,
            50.0e-6,
            25.0e-6,
            ISHMAEL_DENSITY_RANGE_KG_M3[1] + density_excursion,
        ))
        .unwrap();
        assert_eq!(
            density_boundary.bulk_density_kg_m3(),
            ISHMAEL_DENSITY_RANGE_KG_M3[1]
        );
        assert!(
            density_boundary
                .reconstruction_audit()
                .density_bound_excursion_kg_m3
                > 0.0
        );
    }

    #[test]
    fn exact_wrf_upper_density_transport_residue_is_canonicalized() {
        // Actual MP55 cell 1,951,079 reconstructed this value from native WRF
        // REAL QICE/QNICE/QVOLI/QAOLI. It is a ~1 ppm arithmetic excursion,
        // not a physically distinct density above ISHMAEL's 920 kg m^-3 cap.
        let transported_density = 920.000_932_689_841_8;
        let distribution = IshmaelPsd::reconstruct(input_from_scales(
            IshmaelIceCategory::Planar,
            50.0e-6,
            25.0e-6,
            transported_density,
        ))
        .unwrap();

        assert_eq!(
            distribution.bulk_density_kg_m3(),
            ISHMAEL_DENSITY_RANGE_KG_M3[1]
        );
        assert_relative(
            distribution
                .reconstruction_audit()
                .density_bound_excursion_kg_m3,
            transported_density - ISHMAEL_DENSITY_RANGE_KG_M3[1],
            2.0e-12,
        );
    }

    #[derive(Debug)]
    struct CallbackError;

    impl fmt::Display for CallbackError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("expected callback failure")
        }
    }

    impl Error for CallbackError {}

    #[test]
    fn callback_error_retains_quadrature_level_and_node() {
        let error = integrate_ishmael_psd(
            &oblate_distribution(),
            PsdIntegrationConfig::default(),
            PsdParticleSupport::default(),
            synthetic_fall_speed_provenance(),
            |_| Err::<AdditiveScattering, _>(CallbackError),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            PsdIntegrationError::NodeEvaluation {
                level: PsdQuadratureLevel::Coarse,
                node_index: 0,
                ..
            }
        ));
    }

    #[test]
    fn node_budget_and_tail_limit_are_fail_closed() {
        assert!(matches!(
            PsdIntegrationConfig::new(
                8,
                127,
                96.0,
                1.0e-10,
                5.0e-8,
                5.0e-3,
                DEFAULT_ADDITIVE_ABSOLUTE_TOLERANCES,
                1.0e-6,
                1.0e-6,
                1.0e-6,
            ),
            Err(PsdError::NodeBudgetExceeded { .. })
        ));
        let impossible_tail = PsdIntegrationConfig::new(
            8,
            256,
            5.0,
            1.0e-12,
            5.0e-8,
            5.0e-3,
            DEFAULT_ADDITIVE_ABSOLUTE_TOLERANCES,
            1.0e-6,
            1.0e-6,
            1.0e-6,
        )
        .unwrap();
        let error = integrate_ishmael_psd(
            &oblate_distribution(),
            impossible_tail,
            PsdParticleSupport::default(),
            synthetic_fall_speed_provenance(),
            |node| Ok::<_, Infallible>(synthetic_per_particle(node)),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            PsdIntegrationError::Psd(PsdError::TailToleranceUnreachable { .. })
        ));
    }

    #[test]
    fn prepared_workload_preserves_level_index_and_callback_order() {
        let prepared = prepare_ishmael_psd(
            &oblate_distribution(),
            PsdIntegrationConfig::default(),
            PsdParticleSupport::default(),
            synthetic_fall_speed_provenance(),
        )
        .unwrap();
        assert_eq!(prepared.node_count(PsdQuadratureLevel::Coarse), 72);
        assert_eq!(prepared.node_count(PsdQuadratureLevel::Refined), 136);
        assert_eq!(prepared.node_count(PsdQuadratureLevel::AdaptiveRefined), 0);

        let expected = prepared
            .nodes()
            .map(|(level, index, node)| {
                assert_eq!(node.index(), index);
                (level, index, node.scaled_a().to_bits())
            })
            .collect::<Vec<_>>();
        assert!(
            expected[..72]
                .iter()
                .all(|(level, ..)| *level == PsdQuadratureLevel::Coarse)
        );
        assert!(
            expected[72..]
                .iter()
                .all(|(level, ..)| *level == PsdQuadratureLevel::Refined)
        );

        let mut callback_order = Vec::new();
        prepared
            .finish(|level, index, node| {
                callback_order.push((level, index, node.scaled_a().to_bits()));
                Ok::<_, Infallible>(synthetic_per_particle(node))
            })
            .unwrap();
        assert_eq!(callback_order, expected);
    }

    #[test]
    fn prepared_cpu_finish_is_bit_identical_to_frozen_pre_refactor_result() {
        const EXPECTED_COMPONENT_BITS: [u64; AdditiveScattering::COMPONENT_COUNT] = [
            4_624_366_135_188_360_613,
            4_622_730_831_761_640_179,
            4_621_913_180_048_279_949,
            0,
            4_570_454_029_199_748_885,
            4_533_201_175_231_653_355,
            4_531_784_465_286_792_376,
            4_621_728_900_563_117_324,
            4_619_695_587_033_046_629,
        ];
        const EXPECTED_CLOSURE_BITS: [u64; 3] = [
            4_409_129_588_311_982_080,
            4_447_716_880_169_304_064,
            4_422_341_320_031_338_496,
        ];
        const EXPECTED_CONVERGENCE_BITS: u64 = 4_497_224_872_953_735_180;

        let distribution = oblate_distribution();
        let config = PsdIntegrationConfig::default();
        let support = PsdParticleSupport::default();
        let fall_speed = synthetic_fall_speed_provenance();
        let prepared = prepare_ishmael_psd(&distribution, config, support, fall_speed).unwrap();
        let direct = prepared
            .finish(|_, _, node| Ok::<_, Infallible>(synthetic_per_particle(node)))
            .unwrap();
        assert_eq!(
            direct.additive().components().map(f64::to_bits),
            EXPECTED_COMPONENT_BITS
        );
        let audit = direct.audit();
        assert_eq!(
            [
                audit.number_closure_relative_error.to_bits(),
                audit.mass_closure_relative_error.to_bits(),
                audit.d6_closure_relative_error.to_bits(),
            ],
            EXPECTED_CLOSURE_BITS
        );
        assert_eq!(
            audit.maximum_additive_convergence_error.to_bits(),
            EXPECTED_CONVERGENCE_BITS
        );
        assert_eq!(audit.maximum_additive_convergence_component, 4);

        let delegated = integrate_ishmael_psd(&distribution, config, support, fall_speed, |node| {
            Ok::<_, Infallible>(synthetic_per_particle(node))
        })
        .unwrap();
        assert_eq!(delegated, direct);
    }
}
