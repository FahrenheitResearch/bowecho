//! Fail-closed runtime binding for research PyTMatrix 0.3.3 LUTs.
//!
//! A table is never selected from its rectilinear axes alone.  Loading binds
//! the exact external generator config to the file digest and turns every
//! supported physics choice into a typed descriptor.  Evaluation then checks
//! that descriptor against a closed particle category before interpolation.

use std::f64::consts::PI;

use serde::Deserialize;
use thiserror::Error;

use crate::{
    AdditiveScattering, AxisCoordinate, AxisKind, ClosedParticleCategory, ConventionalHydrometeor,
    DiagnosticWetCategory, EffectiveMediumRule, InterpolationError, KernelModel, LutError,
    MeltingModel, MicrophysicsFamily, MixtureTopology, OfflineLut, OrientationModel, OutputError,
    ParticleState, PsdError, PsdFallSpeedAuthority, PsdFallSpeedProvenance, PsdParticleDomain,
    PsdParticleNode, PsdSpheroidHabit, Sha256Digest, TMatrixImplementation, TableValidation,
    TemporalSampling, Unit,
};

const PYTMATRIX_KERNEL: &str = "pytmatrix-0.3.3";
const RESEARCH_STATUS: &str = "research_only_unvalidated";
const MONODISPERSE_NODE: &str = "monodisperse_node";
const CONSTANT_S_BAND_DIELECTRIC: &str = "constant_over_configured_s_band_nodes";
const SPEED_OF_LIGHT_M_S: f64 = 299_792_458.0;
const BACKSCATTER_GEOMETRY_DEG: [f64; 6] = [90.0, 90.0, 0.0, 180.0, 0.0, 0.0];
const FORWARD_GEOMETRY_DEG: [f64; 6] = [90.0, 90.0, 0.0, 0.0, 0.0, 0.0];

/// Microphysics family and exact native category represented by a table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TMatrixParticleCategory {
    Conventional(ConventionalHydrometeor),
    /// Characteristic-particle node explicitly shared by closed P3 and
    /// ISHMAEL states; this is not a conventional-category alias or PSD.
    PropertyAwareFrozenCharacteristicParticle,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TMatrixPopulationRole {
    OrdinaryConventional,
    ConventionalRainStandaloneAndResidual,
    PropertyAwareDryCharacteristicParticle,
    PropertyAwareWetCharacteristicParticle,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DensityApplicability {
    ConventionalCategory,
    DryBulkDensity15To917KgM3Above1225Air,
    WetCondensedVolumeFraction00015To1Above1225Air,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TMatrixExecutionDescriptor {
    FreshProcessPerGridPoint,
    FreshProcessPerMaterialStateGroup {
        material_state_axes: Vec<AxisKind>,
        tmatrix_state_axes: Vec<AxisKind>,
        geometry_axis: AxisKind,
        maximum_points_per_process: u32,
        group_timeout_seconds: u64,
    },
}

impl TMatrixParticleCategory {
    #[must_use]
    pub const fn conventional_family(self) -> Option<MicrophysicsFamily> {
        match self {
            Self::Conventional(_) => Some(MicrophysicsFamily::Conventional),
            Self::PropertyAwareFrozenCharacteristicParticle => None,
        }
    }
}

/// Geometric meaning of the table's minor-to-major aspect-ratio coordinate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpheroidConvention {
    /// Rotational axis is the minor axis. PyTMatrix receives
    /// `horizontal/rotational = 1 / minor_to_major`.
    OblateMinorVertical,
    /// Rotational axis is the major axis. PyTMatrix receives
    /// `horizontal/rotational = minor_to_major`.
    ProlateMajorVertical,
}

impl SpheroidConvention {
    pub fn pytmatrix_axis_ratio(self, minor_to_major: f64) -> Result<f64, EvaluationError> {
        if !(minor_to_major.is_finite() && 0.0 < minor_to_major && minor_to_major <= 1.0) {
            return Err(EvaluationError::InvalidQuery {
                field: "minor-to-major axis ratio",
                value: minor_to_major,
            });
        }
        Ok(match self {
            Self::OblateMinorVertical => 1.0 / minor_to_major,
            Self::ProlateMajorVertical => minor_to_major,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ComplexRefractiveIndex {
    pub real: f64,
    pub imaginary: f64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HomogeneousMaterial {
    LiquidWater,
    Ice,
}

/// Exact dielectric/material topology used at generation time.
#[derive(Clone, Debug, PartialEq)]
pub enum TMatrixMaterial {
    Homogeneous {
        material: HomogeneousMaterial,
        refractive_index: ComplexRefractiveIndex,
        mass_density_kg_m3: f64,
        temperature_k: f64,
    },
    MaxwellGarnettIceHostWaterInclusion {
        ice_refractive_index: ComplexRefractiveIndex,
        liquid_water_refractive_index: ComplexRefractiveIndex,
        ice_density_kg_m3: f64,
        liquid_water_density_kg_m3: f64,
        temperature_k: f64,
    },
    SymmetricBruggemanSphericalAirIceWaterV1 {
        air_relative_permittivity: ComplexRefractiveIndex,
        ice_permittivity_model: String,
        liquid_water_permittivity_model: String,
        ice_temperature_treatment: String,
        ice_material_density_kg_m3: f64,
        liquid_water_density_kg_m3: f64,
        homotopy_steps: u32,
        newton_max_iterations: u32,
        newton_relative_tolerance: f64,
        temperature_range_k: [f64; 2],
    },
    SymmetricBruggemanSphericalAirIceMatzler2006V1 {
        air_relative_permittivity: ComplexRefractiveIndex,
        ice_material_density_kg_m3: f64,
        homotopy_steps: u32,
        newton_max_iterations: u32,
        newton_relative_tolerance: f64,
        temperature_range_k: [f64; 2],
    },
    TemperatureDependentLiquidWaterLiebe1991 {
        mass_density_kg_m3: f64,
        temperature_range_k: [f64; 2],
        frequency_range_hz: [f64; 2],
    },
}

impl TMatrixMaterial {
    #[must_use]
    pub const fn fixed_temperature_k(&self) -> Option<f64> {
        match self {
            Self::Homogeneous { temperature_k, .. }
            | Self::MaxwellGarnettIceHostWaterInclusion { temperature_k, .. } => {
                Some(*temperature_k)
            }
            Self::SymmetricBruggemanSphericalAirIceWaterV1 { .. } => None,
            Self::SymmetricBruggemanSphericalAirIceMatzler2006V1 { .. } => None,
            Self::TemperatureDependentLiquidWaterLiebe1991 { .. } => None,
        }
    }
}

/// The exact orientation integration represented by each LUT value.
#[derive(Clone, Debug, PartialEq)]
pub enum TMatrixOdfConvention {
    FixedAlignedVertical {
        pytmatrix_alpha_deg: f64,
        pytmatrix_beta_deg: f64,
    },
    GaussianCanting {
        mean_deg: f64,
        standard_deviation_deg: f64,
        alpha_quadrature_points: u16,
        beta_quadrature_points: u16,
    },
}

impl TMatrixOdfConvention {
    fn orientation_model(&self) -> OrientationModel {
        match *self {
            Self::FixedAlignedVertical { .. } => OrientationModel::FixedEuler {
                yaw_deg: 0.0,
                pitch_deg: 0.0,
                roll_deg: 0.0,
            },
            Self::GaussianCanting {
                mean_deg,
                standard_deviation_deg,
                alpha_quadrature_points,
                beta_quadrature_points,
            } => OrientationModel::GaussianCanting {
                mean_deg,
                standard_deviation_deg,
                quadrature_points: alpha_quadrature_points * beta_quadrature_points,
            },
        }
    }
}

/// The only radar-basis convention accepted by this schema-v1 runtime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RadarHvConvention {
    /// PyTMatrix horizontal back/forward geometries, with complex covariance
    /// `HH * conjugate(VV)` and phase supplied by `delta_hv`.
    PytMatrixHorizontalHhConjugateVv,
}

/// Beam view represented by the PyTMatrix propagation geometry. Azimuth is
/// omitted because the accepted ODFs are axisymmetric about vertical.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RadarViewGeometry {
    beam_elevation_deg: f64,
}

impl RadarViewGeometry {
    pub fn new(beam_elevation_deg: f64) -> Result<Self, EvaluationError> {
        if !beam_elevation_deg.is_finite() || !(-90.0..=90.0).contains(&beam_elevation_deg) {
            return Err(EvaluationError::InvalidQuery {
                field: "beam elevation",
                value: beam_elevation_deg,
            });
        }
        Ok(Self { beam_elevation_deg })
    }

    #[must_use]
    pub const fn horizontal() -> Self {
        Self {
            beam_elevation_deg: 0.0,
        }
    }

    #[must_use]
    pub const fn beam_elevation_deg(self) -> f64 {
        self.beam_elevation_deg
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RadarViewApplicability {
    HorizontalSingletonZeroDegreeAxis,
    PpiElevationAxisMinus05To20AxisymmetricGaussian,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RadarConventionDescriptor {
    pub convention: RadarHvConvention,
    pub view_applicability: RadarViewApplicability,
    pub reference_water_dielectric_factor_squared: f64,
    pub solver_ddelt: f64,
    pub solver_ndgs: u32,
}

/// Policy for the small discontinuity where the piecewise Schiller-Naumann
/// drag approximation switches to its constant high-Reynolds drag value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DragTransitionBoundaryPolicy {
    /// Select the exact transition-Reynolds speed when the one-sided drag
    /// residuals straddle zero and the piecewise approximation has no exact
    /// force-balance root.
    SelectExactTransitionReynoldsBoundaryWhenPiecewiseDragResidualJumpStraddlesZero,
}

/// Terminal-speed law folded into each stored table node's additive fall
/// moments by the generator. Runtime category evaluation replaces those two
/// stored moments with the closed or diagnosed category's positive-downward
/// fall speed before number-density scaling.
#[derive(Clone, Debug, PartialEq)]
pub enum TerminalSpeedPolicy {
    AtlasRain1973Exponential {
        a_m_s: f64,
        b_m_s: f64,
        c_per_mm: f64,
        valid_diameter_range_m: [f64; 2],
    },
    SchillerNaumannGravityDrag {
        gravity_m_s2: f64,
        air_density_kg_m3: f64,
        air_dynamic_viscosity_pa_s: f64,
        drag_transition_reynolds: f64,
        high_reynolds_drag_coefficient: f64,
        drag_transition_boundary_policy: DragTransitionBoundaryPolicy,
        maximum_iterations: u32,
        relative_tolerance: f64,
    },
}

/// Physics identity that must accompany a research LUT through selection.
#[derive(Clone, Debug, PartialEq)]
pub struct TMatrixTableDescriptor {
    table_id: String,
    category: TMatrixParticleCategory,
    population_role: TMatrixPopulationRole,
    density_applicability: DensityApplicability,
    spheroid: SpheroidConvention,
    material: TMatrixMaterial,
    odf: TMatrixOdfConvention,
    radar: RadarConventionDescriptor,
    terminal_speed: TerminalSpeedPolicy,
    terminal_speed_sha256: Sha256Digest,
    execution: TMatrixExecutionDescriptor,
    normalization_number_concentration_m3: f64,
}

impl TMatrixTableDescriptor {
    #[must_use]
    pub fn table_id(&self) -> &str {
        &self.table_id
    }

    #[must_use]
    pub const fn category(&self) -> TMatrixParticleCategory {
        self.category
    }

    #[must_use]
    pub const fn population_role(&self) -> TMatrixPopulationRole {
        self.population_role
    }

    #[must_use]
    pub const fn density_applicability(&self) -> DensityApplicability {
        self.density_applicability
    }

    #[must_use]
    pub const fn spheroid(&self) -> SpheroidConvention {
        self.spheroid
    }

    #[must_use]
    pub const fn material(&self) -> &TMatrixMaterial {
        &self.material
    }

    #[must_use]
    pub const fn odf(&self) -> &TMatrixOdfConvention {
        &self.odf
    }

    #[must_use]
    pub const fn radar(&self) -> &RadarConventionDescriptor {
        &self.radar
    }

    #[must_use]
    pub const fn terminal_speed(&self) -> &TerminalSpeedPolicy {
        &self.terminal_speed
    }

    #[must_use]
    pub const fn execution(&self) -> &TMatrixExecutionDescriptor {
        &self.execution
    }

    #[must_use]
    pub const fn normalization_number_concentration_m3(&self) -> f64 {
        self.normalization_number_concentration_m3
    }
}

/// Query-time facts not encoded by [`ClosedParticleCategory`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TMatrixEvaluationRequest {
    frequency_hz: f64,
    spheroid: SpheroidConvention,
    view: RadarViewGeometry,
}

/// One dry scheme-PSD particle query against a table normalized to exactly
/// one particle per cubic metre.
///
/// Population weighting is deliberately absent. Callers integrate the
/// returned [`AdditiveScattering`] with their PSD node's number-density
/// weight. Orientation, spheroid family, exact frequency, and every table
/// coordinate remain fail-closed through the existing runtime binding.
#[derive(Clone, Debug, PartialEq)]
pub struct TMatrixParticleNodeQuery {
    temperature_k: f64,
    equivolume_diameter_m: f64,
    bulk_density_kg_m3: f64,
    minor_to_major_axis_ratio: f64,
    habit: PsdSpheroidHabit,
    rime_mass_fraction: Option<f64>,
    rime_density_kg_m3: Option<f64>,
    positive_down_fall_speed_m_s: f64,
    fall_speed: PsdFallSpeedProvenance,
    orientation: OrientationModel,
    request: TMatrixEvaluationRequest,
}

impl TMatrixParticleNodeQuery {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        temperature_k: f64,
        equivolume_diameter_m: f64,
        bulk_density_kg_m3: f64,
        minor_to_major_axis_ratio: f64,
        habit: PsdSpheroidHabit,
        rime_mass_fraction: Option<f64>,
        rime_density_kg_m3: Option<f64>,
        positive_down_fall_speed_m_s: f64,
        fall_speed: PsdFallSpeedProvenance,
        orientation: OrientationModel,
        request: TMatrixEvaluationRequest,
    ) -> Result<Self, EvaluationError> {
        for (field, value) in [
            ("particle-node temperature", temperature_k),
            (
                "particle-node equivalent-volume diameter",
                equivolume_diameter_m,
            ),
            ("particle-node bulk density", bulk_density_kg_m3),
            (
                "particle-node positive-down fall speed",
                positive_down_fall_speed_m_s,
            ),
        ] {
            if !value.is_finite() || value <= 0.0 {
                return Err(EvaluationError::InvalidQuery { field, value });
            }
        }
        if !(minor_to_major_axis_ratio.is_finite()
            && 0.0 < minor_to_major_axis_ratio
            && minor_to_major_axis_ratio <= 1.0)
        {
            return Err(EvaluationError::InvalidQuery {
                field: "particle-node minor-to-major axis ratio",
                value: minor_to_major_axis_ratio,
            });
        }
        let effectively_spherical = (minor_to_major_axis_ratio - 1.0).abs() <= 1.0e-10;
        if effectively_spherical != (habit == PsdSpheroidHabit::Spherical) {
            return Err(EvaluationError::ParticleNodeHabitGeometryMismatch {
                habit,
                minor_to_major_axis_ratio,
            });
        }
        if let Some(value) = rime_mass_fraction
            && (!value.is_finite() || !(0.0..=1.0).contains(&value))
        {
            return Err(EvaluationError::InvalidQuery {
                field: "particle-node rime mass fraction",
                value,
            });
        }
        if let Some(value) = rime_density_kg_m3
            && (!value.is_finite() || value <= 0.0)
        {
            return Err(EvaluationError::InvalidQuery {
                field: "particle-node rime density",
                value,
            });
        }
        Ok(Self {
            temperature_k,
            equivolume_diameter_m,
            bulk_density_kg_m3,
            minor_to_major_axis_ratio,
            habit,
            rime_mass_fraction,
            rime_density_kg_m3,
            positive_down_fall_speed_m_s,
            fall_speed,
            orientation,
            request,
        })
    }

    pub fn from_psd_node(
        node: &PsdParticleNode,
        temperature_k: f64,
        positive_down_fall_speed_m_s: f64,
        fall_speed: PsdFallSpeedProvenance,
        orientation: OrientationModel,
        request: TMatrixEvaluationRequest,
    ) -> Result<Self, EvaluationError> {
        Self::new(
            temperature_k,
            node.equivolume_diameter_m(),
            node.bulk_density_kg_m3(),
            node.minor_to_major_axis_ratio(),
            node.habit(),
            node.rime_mass_fraction(),
            node.rime_density_kg_m3(),
            positive_down_fall_speed_m_s,
            fall_speed,
            orientation,
            request,
        )
    }

    #[must_use]
    pub const fn temperature_k(&self) -> f64 {
        self.temperature_k
    }

    #[must_use]
    pub const fn equivolume_diameter_m(&self) -> f64 {
        self.equivolume_diameter_m
    }

    #[must_use]
    pub const fn bulk_density_kg_m3(&self) -> f64 {
        self.bulk_density_kg_m3
    }

    #[must_use]
    pub const fn minor_to_major_axis_ratio(&self) -> f64 {
        self.minor_to_major_axis_ratio
    }

    #[must_use]
    pub const fn habit(&self) -> PsdSpheroidHabit {
        self.habit
    }

    #[must_use]
    pub const fn rime_mass_fraction(&self) -> Option<f64> {
        self.rime_mass_fraction
    }

    #[must_use]
    pub const fn rime_density_kg_m3(&self) -> Option<f64> {
        self.rime_density_kg_m3
    }

    #[must_use]
    pub const fn positive_down_fall_speed_m_s(&self) -> f64 {
        self.positive_down_fall_speed_m_s
    }

    #[must_use]
    pub const fn fall_speed(&self) -> PsdFallSpeedProvenance {
        self.fall_speed
    }

    #[must_use]
    pub const fn orientation(&self) -> &OrientationModel {
        &self.orientation
    }

    #[must_use]
    pub const fn request(&self) -> TMatrixEvaluationRequest {
        self.request
    }
}

/// Auditable conversion from a closure's per-dry-air number to the number
/// density used to scale a per-1-m3 monodisperse table node.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NumberScalingPolicy {
    ClosedCategoryNumberPerKgTimesAirDensity,
    PreserveFrozenParticleNumberForWetCategory,
    PreserveRainPsdShapeScaleNumberByResidualMassFraction,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FallMomentPolicy {
    ClosedCategoryPositiveDownZeroWithinCategoryVariance,
    DiagnosticWetCategoryPositiveDownZeroWithinCategoryVariance,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScaledScatteringContribution {
    additive: AdditiveScattering,
    number_density_m3: f64,
    represented_mixing_ratio_kgkg: f64,
    consumed_paired_liquid_mass_kgkg: f64,
    number_scaling: NumberScalingPolicy,
    fall_moments: FallMomentPolicy,
}

impl ScaledScatteringContribution {
    #[must_use]
    pub const fn additive(self) -> AdditiveScattering {
        self.additive
    }

    #[must_use]
    pub const fn number_density_m3(self) -> f64 {
        self.number_density_m3
    }

    #[must_use]
    pub const fn represented_mixing_ratio_kgkg(self) -> f64 {
        self.represented_mixing_ratio_kgkg
    }

    #[must_use]
    pub const fn consumed_paired_liquid_mass_kgkg(self) -> f64 {
        self.consumed_paired_liquid_mass_kgkg
    }

    #[must_use]
    pub const fn number_scaling(self) -> NumberScalingPolicy {
        self.number_scaling
    }

    #[must_use]
    pub const fn fall_moments(self) -> FallMomentPolicy {
        self.fall_moments
    }
}

impl TMatrixEvaluationRequest {
    pub fn new(
        frequency_hz: f64,
        spheroid: SpheroidConvention,
        view: RadarViewGeometry,
    ) -> Result<Self, EvaluationError> {
        if !frequency_hz.is_finite() || frequency_hz <= 0.0 {
            return Err(EvaluationError::InvalidQuery {
                field: "radar frequency",
                value: frequency_hz,
            });
        }
        Ok(Self {
            frequency_hz,
            spheroid,
            view,
        })
    }

    #[must_use]
    pub const fn frequency_hz(self) -> f64 {
        self.frequency_hz
    }

    #[must_use]
    pub const fn spheroid(self) -> SpheroidConvention {
        self.spheroid
    }

    #[must_use]
    pub const fn view(self) -> RadarViewGeometry {
        self.view
    }
}

/// An immutable table that has passed the full research-runtime binding gate.
#[derive(Clone, Debug, PartialEq)]
pub struct ResearchTMatrixLut {
    lut: OfflineLut,
    descriptor: TMatrixTableDescriptor,
    file_sha256: Sha256Digest,
}

impl ResearchTMatrixLut {
    /// Load a table only when the complete file and the exact external config
    /// match their declared identities. `OfflineLut` performs the independent
    /// magic/schema/header/config/payload checks before the typed binding.
    pub fn load(
        lut_bytes: &[u8],
        expected_lut_sha256: Sha256Digest,
        exact_external_config_utf8: &[u8],
    ) -> Result<Self, TMatrixLoadError> {
        let actual_lut_sha256 = Sha256Digest::compute(lut_bytes);
        if actual_lut_sha256 != expected_lut_sha256 {
            return Err(TMatrixLoadError::FileDigestMismatch {
                expected: expected_lut_sha256,
                actual: actual_lut_sha256,
            });
        }

        let lut = OfflineLut::from_bytes(lut_bytes)?;
        lut.verify_generator_config(exact_external_config_utf8)?;
        if lut.header().generator_config_utf8().as_bytes() != exact_external_config_utf8 {
            return Err(TMatrixLoadError::ExternalConfigBytesMismatch);
        }
        let config: RawGeneratorConfig = serde_json::from_slice(exact_external_config_utf8)
            .map_err(TMatrixLoadError::GeneratorConfigJson)?;
        let descriptor = bind_descriptor(&lut, config)?;
        Ok(Self {
            lut,
            descriptor,
            file_sha256: actual_lut_sha256,
        })
    }

    #[must_use]
    pub const fn descriptor(&self) -> &TMatrixTableDescriptor {
        &self.descriptor
    }

    #[must_use]
    pub const fn file_sha256(&self) -> Sha256Digest {
        self.file_sha256
    }

    #[must_use]
    pub const fn offline_lut(&self) -> &OfflineLut {
        &self.lut
    }

    /// Exact diameter/density/aspect envelope of a dry property-particle
    /// table, suitable for PSD omission accounting before node evaluation.
    pub fn dry_particle_node_domain(&self) -> Result<PsdParticleDomain, EvaluationError> {
        if self.descriptor.category
            != TMatrixParticleCategory::PropertyAwareFrozenCharacteristicParticle
            || self.descriptor.population_role
                != TMatrixPopulationRole::PropertyAwareDryCharacteristicParticle
        {
            return Err(EvaluationError::DryParticleNodeTableRequired {
                actual_category: self.descriptor.category,
                actual_role: self.descriptor.population_role,
            });
        }
        let bounds = |kind| -> Result<[f64; 2], EvaluationError> {
            let axis = self
                .lut
                .header()
                .axes()
                .iter()
                .find(|axis| axis.kind() == kind)
                .ok_or(EvaluationError::MissingParticleNodeTableAxis(kind))?;
            let coordinates = axis.coordinates();
            let minimum = *coordinates
                .first()
                .ok_or(EvaluationError::MissingParticleNodeTableAxis(kind))?;
            let maximum = *coordinates
                .last()
                .ok_or(EvaluationError::MissingParticleNodeTableAxis(kind))?;
            Ok([minimum, maximum])
        };
        PsdParticleDomain::new(
            bounds(AxisKind::EquivolumeDiameter)?,
            bounds(AxisKind::BulkDensity)?,
            bounds(AxisKind::MinorToMajorAxisRatio)?,
        )
        .map_err(EvaluationError::ParticleNodeDomain)
    }

    /// Exact identity of the terminal-speed policy bound to this dry particle
    /// table. The digest covers only the versioned law and all of its numeric
    /// parameters, so shape-specific tables with the same law produce the
    /// same provenance token.
    pub fn dry_particle_node_fall_speed_provenance(
        &self,
    ) -> Result<PsdFallSpeedProvenance, EvaluationError> {
        if self.descriptor.category
            != TMatrixParticleCategory::PropertyAwareFrozenCharacteristicParticle
            || self.descriptor.population_role
                != TMatrixPopulationRole::PropertyAwareDryCharacteristicParticle
        {
            return Err(EvaluationError::DryParticleNodeTableRequired {
                actual_category: self.descriptor.category,
                actual_role: self.descriptor.population_role,
            });
        }
        if !matches!(
            &self.descriptor.terminal_speed,
            TerminalSpeedPolicy::SchillerNaumannGravityDrag { .. }
        ) {
            return Err(EvaluationError::DryParticleNodeTerminalSpeedPolicyRequired);
        }
        Ok(PsdFallSpeedProvenance::new(
            PsdFallSpeedAuthority::TMatrixTableTerminalPolicyV1,
            self.descriptor.terminal_speed_sha256,
        ))
    }

    /// Reproduce the exact Schiller-Naumann terminal-speed solver declared by
    /// the table generator for one PSD particle node.
    pub fn dry_particle_node_terminal_speed_m_s(
        &self,
        node: &PsdParticleNode,
    ) -> Result<f64, EvaluationError> {
        if self.descriptor.category
            != TMatrixParticleCategory::PropertyAwareFrozenCharacteristicParticle
            || self.descriptor.population_role
                != TMatrixPopulationRole::PropertyAwareDryCharacteristicParticle
        {
            return Err(EvaluationError::DryParticleNodeTableRequired {
                actual_category: self.descriptor.category,
                actual_role: self.descriptor.population_role,
            });
        }
        schiller_naumann_terminal_speed_m_s(
            &self.descriptor.terminal_speed,
            node.equivolume_diameter_m(),
            node.bulk_density_kg_m3(),
        )
    }

    /// Interpolate a per-particle table node and scale every additive output
    /// by `number_per_kg * air_density_kg_m3`.
    pub fn evaluate(
        &self,
        particle: &ClosedParticleCategory,
        request: TMatrixEvaluationRequest,
    ) -> Result<AdditiveScattering, EvaluationError> {
        let state = particle.record().state();
        let actual_family = state.family();
        match self.descriptor.category {
            TMatrixParticleCategory::Conventional(_) => {
                if actual_family != MicrophysicsFamily::Conventional {
                    return Err(EvaluationError::FamilyMismatch {
                        expected: MicrophysicsFamily::Conventional,
                        actual: actual_family,
                    });
                }
            }
            TMatrixParticleCategory::PropertyAwareFrozenCharacteristicParticle => {
                if !matches!(
                    actual_family,
                    MicrophysicsFamily::P3 | MicrophysicsFamily::Ishmael
                ) {
                    return Err(EvaluationError::PopulationApplicabilityMismatch {
                        expected: self.descriptor.category,
                        actual: actual_family,
                    });
                }
            }
        }

        let (category, environment, shape, number_per_kg) = match state {
            ParticleState::Conventional(state) => (
                TMatrixParticleCategory::Conventional(state.category()),
                state.environment(),
                state.shape(),
                state
                    .number_per_kg()
                    .ok_or(EvaluationError::MissingNumberConcentration)?,
            ),
            ParticleState::P3(state) => (
                TMatrixParticleCategory::PropertyAwareFrozenCharacteristicParticle,
                state.environment(),
                state.shape(),
                state.total_ice_number_per_kg(),
            ),
            ParticleState::Ishmael(state) => (
                TMatrixParticleCategory::PropertyAwareFrozenCharacteristicParticle,
                state.environment(),
                state.shape(),
                state.number_per_kg(),
            ),
        };
        if category != self.descriptor.category {
            return Err(EvaluationError::CategoryMismatch {
                expected: self.descriptor.category,
                actual: category,
            });
        }
        match self.descriptor.population_role {
            TMatrixPopulationRole::PropertyAwareDryCharacteristicParticle => {
                if shape.liquid_mass_fraction() != 0.0 {
                    return Err(EvaluationError::PhaseRegimeMismatch {
                        expected: "exactly dry liquid_mass_fraction=0",
                        actual_liquid_mass_fraction: shape.liquid_mass_fraction(),
                    });
                }
            }
            TMatrixPopulationRole::PropertyAwareWetCharacteristicParticle => {
                return Err(EvaluationError::WetCategoryInputRequired);
            }
            TMatrixPopulationRole::OrdinaryConventional
            | TMatrixPopulationRole::ConventionalRainStandaloneAndResidual => {}
        }
        if request.spheroid != self.descriptor.spheroid {
            return Err(EvaluationError::SpheroidConventionMismatch {
                expected: self.descriptor.spheroid,
                actual: request.spheroid,
            });
        }
        let expected_orientation = self.descriptor.odf.orientation_model();
        if particle.orientation().model() != &expected_orientation {
            return Err(EvaluationError::OrientationMismatch {
                expected: expected_orientation,
                actual: particle.orientation().model().clone(),
            });
        }
        if let Some(expected_k) = self.descriptor.material.fixed_temperature_k()
            && environment.temperature_k() != expected_k
        {
            return Err(EvaluationError::FixedDielectricTemperatureMismatch {
                expected_k,
                actual_k: environment.temperature_k(),
            });
        }
        match self.descriptor.material {
            TMatrixMaterial::Homogeneous {
                mass_density_kg_m3, ..
            } => {
                if shape.bulk_density_kg_m3() != mass_density_kg_m3 {
                    return Err(EvaluationError::MaterialDensityMismatch {
                        expected_kg_m3: mass_density_kg_m3,
                        actual_kg_m3: shape.bulk_density_kg_m3(),
                    });
                }
            }
            TMatrixMaterial::MaxwellGarnettIceHostWaterInclusion { .. } => {
                return Err(EvaluationError::UnsupportedWetCoexistence);
            }
            TMatrixMaterial::SymmetricBruggemanSphericalAirIceWaterV1 { .. } => {}
            TMatrixMaterial::SymmetricBruggemanSphericalAirIceMatzler2006V1 { .. } => {}
            TMatrixMaterial::TemperatureDependentLiquidWaterLiebe1991 {
                mass_density_kg_m3,
                ..
            } => {
                if shape.bulk_density_kg_m3() != mass_density_kg_m3 {
                    return Err(EvaluationError::MaterialDensityMismatch {
                        expected_kg_m3: mass_density_kg_m3,
                        actual_kg_m3: shape.bulk_density_kg_m3(),
                    });
                }
            }
        }

        let mut coordinates = Vec::with_capacity(self.lut.header().axes().len());
        for axis in self.lut.header().axes() {
            let value = match axis.kind() {
                AxisKind::EquivolumeDiameter => particle.characteristic_diameter_m().value(),
                AxisKind::Temperature => environment.temperature_k(),
                AxisKind::BulkDensity => particle.effective_density_kg_m3().value(),
                AxisKind::CondensedVolumeFraction => condensed_volume_fraction(
                    particle.effective_density_kg_m3().value(),
                    shape.liquid_mass_fraction(),
                )?,
                AxisKind::LiquidMassFraction => shape.liquid_mass_fraction(),
                AxisKind::MinorToMajorAxisRatio => particle.minor_to_major_axis_ratio().value(),
                AxisKind::Frequency => request.frequency_hz,
                AxisKind::RadarElevation => request.view.beam_elevation_deg(),
                AxisKind::RimeMassFraction => particle
                    .rime_mass_fraction()
                    .ok_or(EvaluationError::MissingAxisProperty(axis.kind()))?
                    .value(),
                AxisKind::RimeDensity => particle
                    .rime_density_kg_m3()
                    .ok_or(EvaluationError::MissingAxisProperty(axis.kind()))?
                    .value(),
                AxisKind::CantingAngle | AxisKind::TimeOffset => {
                    return Err(EvaluationError::UnsupportedAxis(axis.kind()));
                }
            };
            coordinates.push(AxisCoordinate::new(axis.kind(), value)?);
        }

        let per_m3 = replace_fall_moments(
            self.lut.interpolate(&coordinates)?,
            particle.fall_speed_m_s().value(),
        )?;
        let number_m3 = number_per_kg * environment.air_density_kg_m3();
        if !number_m3.is_finite() || number_m3 <= 0.0 {
            return Err(EvaluationError::InvalidNumberDensity { value: number_m3 });
        }
        per_m3
            .checked_scale(number_m3)
            .map_err(EvaluationError::Output)
    }

    /// Interpolate one dry property-aware particle without applying any PSD
    /// population scale.
    ///
    /// The table loader already requires exactly one particle per cubic metre
    /// and forbids extrapolation. This method adds a typed scheme-PSD seam
    /// while preserving every existing table identity, material, ODF,
    /// frequency, view, shape, and axis check.
    pub fn evaluate_dry_particle_node_per_m3(
        &self,
        query: &TMatrixParticleNodeQuery,
    ) -> Result<AdditiveScattering, EvaluationError> {
        if self.descriptor.category
            != TMatrixParticleCategory::PropertyAwareFrozenCharacteristicParticle
            || self.descriptor.population_role
                != TMatrixPopulationRole::PropertyAwareDryCharacteristicParticle
        {
            return Err(EvaluationError::DryParticleNodeTableRequired {
                actual_category: self.descriptor.category,
                actual_role: self.descriptor.population_role,
            });
        }
        if self.descriptor.normalization_number_concentration_m3 != 1.0 {
            return Err(EvaluationError::ParticleNodeNormalizationMismatch {
                actual_m3: self.descriptor.normalization_number_concentration_m3,
            });
        }
        let request = query.request;
        if request.spheroid != self.descriptor.spheroid {
            return Err(EvaluationError::SpheroidConventionMismatch {
                expected: self.descriptor.spheroid,
                actual: request.spheroid,
            });
        }
        let habit_matches = match query.habit {
            PsdSpheroidHabit::Oblate => request.spheroid == SpheroidConvention::OblateMinorVertical,
            PsdSpheroidHabit::Prolate => {
                request.spheroid == SpheroidConvention::ProlateMajorVertical
            }
            PsdSpheroidHabit::Spherical => true,
        };
        if !habit_matches {
            return Err(EvaluationError::ParticleNodeSpheroidMismatch {
                habit: query.habit,
                actual: request.spheroid,
            });
        }
        let expected_fall_speed = self.dry_particle_node_fall_speed_provenance()?;
        if query.fall_speed != expected_fall_speed {
            return Err(EvaluationError::ParticleNodeFallSpeedProvenanceMismatch {
                expected: expected_fall_speed,
                actual: query.fall_speed,
            });
        }
        let expected_positive_down_speed = schiller_naumann_terminal_speed_m_s(
            &self.descriptor.terminal_speed,
            query.equivolume_diameter_m,
            query.bulk_density_kg_m3,
        )?;
        if query.positive_down_fall_speed_m_s != expected_positive_down_speed {
            return Err(EvaluationError::ParticleNodeFallSpeedValueMismatch {
                expected_m_s: expected_positive_down_speed,
                actual_m_s: query.positive_down_fall_speed_m_s,
            });
        }
        let expected_orientation = self.descriptor.odf.orientation_model();
        if query.orientation != expected_orientation {
            return Err(EvaluationError::OrientationMismatch {
                expected: expected_orientation,
                actual: query.orientation.clone(),
            });
        }
        if let Some(expected_k) = self.descriptor.material.fixed_temperature_k()
            && query.temperature_k != expected_k
        {
            return Err(EvaluationError::FixedDielectricTemperatureMismatch {
                expected_k,
                actual_k: query.temperature_k,
            });
        }
        match self.descriptor.material {
            TMatrixMaterial::Homogeneous {
                material: HomogeneousMaterial::Ice,
                mass_density_kg_m3,
                ..
            } => {
                if query.bulk_density_kg_m3 != mass_density_kg_m3 {
                    return Err(EvaluationError::MaterialDensityMismatch {
                        expected_kg_m3: mass_density_kg_m3,
                        actual_kg_m3: query.bulk_density_kg_m3,
                    });
                }
            }
            TMatrixMaterial::SymmetricBruggemanSphericalAirIceMatzler2006V1 { .. } => {}
            TMatrixMaterial::Homogeneous {
                material: HomogeneousMaterial::LiquidWater,
                ..
            }
            | TMatrixMaterial::MaxwellGarnettIceHostWaterInclusion { .. }
            | TMatrixMaterial::SymmetricBruggemanSphericalAirIceWaterV1 { .. }
            | TMatrixMaterial::TemperatureDependentLiquidWaterLiebe1991 { .. } => {
                return Err(EvaluationError::DryParticleNodeMaterialRequired);
            }
        }

        let frequency_axis = self
            .lut
            .header()
            .axes()
            .iter()
            .find(|axis| axis.kind() == AxisKind::Frequency)
            .ok_or(EvaluationError::MissingParticleNodeTableAxis(
                AxisKind::Frequency,
            ))?;
        if frequency_axis.coordinates().len() != 1 {
            return Err(EvaluationError::ParticleNodeFrequencyMustBeSingleton {
                actual_coordinates: frequency_axis.coordinates().len(),
            });
        }
        let exact_frequency_hz = frequency_axis.coordinates()[0];
        if request.frequency_hz != exact_frequency_hz {
            return Err(EvaluationError::ParticleNodeFrequencyMismatch {
                expected_hz: exact_frequency_hz,
                actual_hz: request.frequency_hz,
            });
        }

        let mut coordinates = Vec::with_capacity(self.lut.header().axes().len());
        for axis in self.lut.header().axes() {
            let value = match axis.kind() {
                AxisKind::EquivolumeDiameter => query.equivolume_diameter_m,
                AxisKind::Temperature => query.temperature_k,
                AxisKind::BulkDensity => query.bulk_density_kg_m3,
                AxisKind::CondensedVolumeFraction => {
                    condensed_volume_fraction(query.bulk_density_kg_m3, 0.0)?
                }
                AxisKind::LiquidMassFraction => 0.0,
                AxisKind::MinorToMajorAxisRatio => query.minor_to_major_axis_ratio,
                AxisKind::Frequency => request.frequency_hz,
                AxisKind::RadarElevation => request.view.beam_elevation_deg(),
                AxisKind::RimeMassFraction => query.rime_mass_fraction.ok_or(
                    EvaluationError::MissingParticleNodeAxisProperty(axis.kind()),
                )?,
                AxisKind::RimeDensity => query.rime_density_kg_m3.ok_or(
                    EvaluationError::MissingParticleNodeAxisProperty(axis.kind()),
                )?,
                AxisKind::CantingAngle | AxisKind::TimeOffset => {
                    return Err(EvaluationError::UnsupportedAxis(axis.kind()));
                }
            };
            coordinates.push(AxisCoordinate::new(axis.kind(), value)?);
        }
        replace_fall_moments(
            self.lut.interpolate(&coordinates)?,
            query.positive_down_fall_speed_m_s,
        )
    }

    /// Add a category contribution without exposing any nonlinear derived
    /// product as an accumulation primitive.
    pub fn accumulate(
        &self,
        accumulated: AdditiveScattering,
        particle: &ClosedParticleCategory,
        request: TMatrixEvaluationRequest,
    ) -> Result<AdditiveScattering, EvaluationError> {
        accumulated
            .checked_add(self.evaluate(particle, request)?)
            .map_err(EvaluationError::Output)
    }

    /// Evaluate one wet frozen category as exactly one mixed particle
    /// population. Frozen particle number is preserved; paired liquid changes
    /// the mass-derived diameter, density, liquid fraction and aspect. The
    /// returned consumed-liquid field is the audit token that prevents that
    /// rain mass from being evaluated again as residual rain.
    pub fn evaluate_wet_category(
        &self,
        wet: &DiagnosticWetCategory,
        request: TMatrixEvaluationRequest,
    ) -> Result<ScaledScatteringContribution, EvaluationError> {
        if self.descriptor.category
            != TMatrixParticleCategory::PropertyAwareFrozenCharacteristicParticle
            || self.descriptor.population_role
                != TMatrixPopulationRole::PropertyAwareWetCharacteristicParticle
        {
            return Err(EvaluationError::WetCategoryTableRequired);
        }
        if !matches!(
            self.descriptor.material,
            TMatrixMaterial::SymmetricBruggemanSphericalAirIceWaterV1 { .. }
        ) {
            return Err(EvaluationError::WetCategoryTableRequired);
        }
        if wet.mixture().topology() != MixtureTopology::HomogeneousMixedPhase {
            return Err(EvaluationError::UnsupportedMixtureTopology(
                wet.mixture().topology(),
            ));
        }
        if wet.wet_fraction() <= 0.0 {
            return Err(EvaluationError::PhaseRegimeMismatch {
                expected: "strictly wet liquid_mass_fraction>0",
                actual_liquid_mass_fraction: wet.wet_fraction(),
            });
        }
        self.verify_request_shape(request)?;

        let source_state = wet.source_category().record().state();
        let (environment, number_per_kg) = match source_state {
            ParticleState::P3(state) => (state.environment(), state.total_ice_number_per_kg()),
            ParticleState::Ishmael(state) => (state.environment(), state.number_per_kg()),
            ParticleState::Conventional(_) => {
                return Err(EvaluationError::PopulationApplicabilityMismatch {
                    expected: self.descriptor.category,
                    actual: MicrophysicsFamily::Conventional,
                });
            }
        };
        let (mean_deg, standard_deviation_deg, quadrature_points) = wet
            .canting()
            .effective_gaussian()
            .ok_or(EvaluationError::WetOrientationUnavailable)?;
        let actual_orientation = OrientationModel::GaussianCanting {
            mean_deg,
            standard_deviation_deg,
            quadrature_points,
        };
        let expected_orientation = self.descriptor.odf.orientation_model();
        if actual_orientation != expected_orientation {
            return Err(EvaluationError::OrientationMismatch {
                expected: expected_orientation,
                actual: actual_orientation,
            });
        }

        let density = wet.effective_density_kg_m3().value();
        let diameter = (6.0 * wet.wet_total_mass_kgkg() / (PI * density * number_per_kg)).cbrt();
        if !diameter.is_finite() || diameter <= 0.0 {
            return Err(EvaluationError::InvalidWetCharacteristicDiameter { value: diameter });
        }
        let coordinates = self.coordinates_for_values(
            wet.source_category(),
            request,
            environment.temperature_k(),
            diameter,
            density,
            wet.wet_fraction(),
            wet.minor_to_major_axis_ratio().value(),
        )?;
        let number_density_m3 = number_per_kg * environment.air_density_kg_m3();
        if !number_density_m3.is_finite() || number_density_m3 <= 0.0 {
            return Err(EvaluationError::InvalidNumberDensity {
                value: number_density_m3,
            });
        }
        let additive = replace_fall_moments(
            self.lut.interpolate(&coordinates)?,
            wet.fall_speed_m_s().value(),
        )?
        .checked_scale(number_density_m3)
        .map_err(EvaluationError::Output)?;
        Ok(ScaledScatteringContribution {
            additive,
            number_density_m3,
            represented_mixing_ratio_kgkg: wet.wet_total_mass_kgkg(),
            consumed_paired_liquid_mass_kgkg: wet.paired_liquid_mass_kgkg(),
            number_scaling: NumberScalingPolicy::PreserveFrozenParticleNumberForWetCategory,
            fall_moments:
                FallMomentPolicy::DiagnosticWetCategoryPositiveDownZeroWithinCategoryVariance,
        })
    }

    /// Evaluate only the unpaired rain remainder. Preserving PSD shape means
    /// both number and every additive table output are scaled by
    /// `q_residual / q_original`; the original full rain number is forbidden.
    pub fn evaluate_unused_rain(
        &self,
        source: &ClosedParticleCategory,
        unused_mixing_ratio_kgkg: f64,
        request: TMatrixEvaluationRequest,
    ) -> Result<ScaledScatteringContribution, EvaluationError> {
        if self.descriptor.category
            != TMatrixParticleCategory::Conventional(ConventionalHydrometeor::Rain)
            || self.descriptor.population_role
                != TMatrixPopulationRole::ConventionalRainStandaloneAndResidual
        {
            return Err(EvaluationError::ResidualRainTableRequired);
        }
        let ParticleState::Conventional(state) = source.record().state() else {
            return Err(EvaluationError::ResidualRainSourceRequired);
        };
        if state.category() != ConventionalHydrometeor::Rain {
            return Err(EvaluationError::ResidualRainSourceRequired);
        }
        let original = state.mixing_ratio_kgkg();
        if !unused_mixing_ratio_kgkg.is_finite()
            || unused_mixing_ratio_kgkg <= 0.0
            || unused_mixing_ratio_kgkg > original
        {
            return Err(EvaluationError::InvalidResidualRainMass {
                residual_kgkg: unused_mixing_ratio_kgkg,
                original_kgkg: original,
            });
        }
        let fraction = unused_mixing_ratio_kgkg / original;
        let full = self.evaluate(source, request)?;
        let additive = full
            .checked_scale(fraction)
            .map_err(EvaluationError::Output)?;
        let original_number_per_kg = state
            .number_per_kg()
            .ok_or(EvaluationError::MissingNumberConcentration)?;
        let number_density_m3 =
            original_number_per_kg * state.environment().air_density_kg_m3() * fraction;
        Ok(ScaledScatteringContribution {
            additive,
            number_density_m3,
            represented_mixing_ratio_kgkg: unused_mixing_ratio_kgkg,
            consumed_paired_liquid_mass_kgkg: 0.0,
            number_scaling:
                NumberScalingPolicy::PreserveRainPsdShapeScaleNumberByResidualMassFraction,
            fall_moments: FallMomentPolicy::ClosedCategoryPositiveDownZeroWithinCategoryVariance,
        })
    }

    fn verify_request_shape(
        &self,
        request: TMatrixEvaluationRequest,
    ) -> Result<(), EvaluationError> {
        if request.spheroid != self.descriptor.spheroid {
            Err(EvaluationError::SpheroidConventionMismatch {
                expected: self.descriptor.spheroid,
                actual: request.spheroid,
            })
        } else {
            Ok(())
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn coordinates_for_values(
        &self,
        source: &ClosedParticleCategory,
        request: TMatrixEvaluationRequest,
        temperature_k: f64,
        diameter_m: f64,
        density_kg_m3: f64,
        liquid_mass_fraction: f64,
        minor_to_major: f64,
    ) -> Result<Vec<AxisCoordinate>, EvaluationError> {
        let mut coordinates = Vec::with_capacity(self.lut.header().axes().len());
        for axis in self.lut.header().axes() {
            let value = match axis.kind() {
                AxisKind::EquivolumeDiameter => diameter_m,
                AxisKind::Temperature => temperature_k,
                AxisKind::BulkDensity => density_kg_m3,
                AxisKind::CondensedVolumeFraction => {
                    condensed_volume_fraction(density_kg_m3, liquid_mass_fraction)?
                }
                AxisKind::LiquidMassFraction => liquid_mass_fraction,
                AxisKind::MinorToMajorAxisRatio => minor_to_major,
                AxisKind::Frequency => request.frequency_hz,
                AxisKind::RadarElevation => request.view.beam_elevation_deg(),
                AxisKind::RimeMassFraction => source
                    .rime_mass_fraction()
                    .ok_or(EvaluationError::MissingAxisProperty(axis.kind()))?
                    .value(),
                AxisKind::RimeDensity => source
                    .rime_density_kg_m3()
                    .ok_or(EvaluationError::MissingAxisProperty(axis.kind()))?
                    .value(),
                AxisKind::CantingAngle | AxisKind::TimeOffset => {
                    return Err(EvaluationError::UnsupportedAxis(axis.kind()));
                }
            };
            coordinates.push(AxisCoordinate::new(axis.kind(), value)?);
        }
        Ok(coordinates)
    }
}

fn terminal_speed_policy_sha256(policy: &TerminalSpeedPolicy) -> Sha256Digest {
    let mut canonical = Vec::with_capacity(96);
    let push_f64 = |buffer: &mut Vec<u8>, value: f64| {
        buffer.extend_from_slice(&value.to_bits().to_le_bytes());
    };
    match policy {
        TerminalSpeedPolicy::AtlasRain1973Exponential {
            a_m_s,
            b_m_s,
            c_per_mm,
            valid_diameter_range_m,
        } => {
            canonical.extend_from_slice(b"atlas-rain-1973-exponential-v1\0");
            push_f64(&mut canonical, *a_m_s);
            push_f64(&mut canonical, *b_m_s);
            push_f64(&mut canonical, *c_per_mm);
            push_f64(&mut canonical, valid_diameter_range_m[0]);
            push_f64(&mut canonical, valid_diameter_range_m[1]);
        }
        TerminalSpeedPolicy::SchillerNaumannGravityDrag {
            gravity_m_s2,
            air_density_kg_m3,
            air_dynamic_viscosity_pa_s,
            drag_transition_reynolds,
            high_reynolds_drag_coefficient,
            drag_transition_boundary_policy,
            maximum_iterations,
            relative_tolerance,
        } => {
            canonical.extend_from_slice(b"schiller-naumann-gravity-drag-v1\0");
            push_f64(&mut canonical, *gravity_m_s2);
            push_f64(&mut canonical, *air_density_kg_m3);
            push_f64(&mut canonical, *air_dynamic_viscosity_pa_s);
            push_f64(&mut canonical, *drag_transition_reynolds);
            push_f64(&mut canonical, *high_reynolds_drag_coefficient);
            canonical.extend_from_slice(match drag_transition_boundary_policy {
                DragTransitionBoundaryPolicy::SelectExactTransitionReynoldsBoundaryWhenPiecewiseDragResidualJumpStraddlesZero => {
                    b"exact-transition-on-residual-jump-v1\0"
                }
            });
            canonical.extend_from_slice(&maximum_iterations.to_le_bytes());
            push_f64(&mut canonical, *relative_tolerance);
        }
    }
    Sha256Digest::compute(&canonical)
}

fn schiller_naumann_terminal_speed_m_s(
    policy: &TerminalSpeedPolicy,
    diameter_m: f64,
    particle_density_kg_m3: f64,
) -> Result<f64, EvaluationError> {
    let TerminalSpeedPolicy::SchillerNaumannGravityDrag {
        gravity_m_s2,
        air_density_kg_m3,
        air_dynamic_viscosity_pa_s,
        drag_transition_reynolds,
        high_reynolds_drag_coefficient,
        drag_transition_boundary_policy:
            DragTransitionBoundaryPolicy::SelectExactTransitionReynoldsBoundaryWhenPiecewiseDragResidualJumpStraddlesZero,
        maximum_iterations,
        relative_tolerance,
    } = policy.clone()
    else {
        return Err(EvaluationError::DryParticleNodeTerminalSpeedPolicyRequired);
    };
    let density_difference = particle_density_kg_m3 - air_density_kg_m3;
    if !density_difference.is_finite() || density_difference <= 0.0 {
        return Err(EvaluationError::ParticleNodeDensityNotAboveAir {
            particle_density_kg_m3,
            air_density_kg_m3,
        });
    }
    let force_scale =
        (4.0 * gravity_m_s2 * diameter_m * density_difference) / (3.0 * air_density_kg_m3);
    let transition_speed =
        drag_transition_reynolds * air_dynamic_viscosity_pa_s / (air_density_kg_m3 * diameter_m);
    let low_re_transition_drag =
        (24.0 / drag_transition_reynolds) * (1.0 + 0.15 * drag_transition_reynolds.powf(0.687));
    let residual_below_transition =
        transition_speed * transition_speed * low_re_transition_drag - force_scale;
    let residual_above_transition =
        transition_speed * transition_speed * high_reynolds_drag_coefficient - force_scale;
    if residual_below_transition <= 0.0 && residual_above_transition >= 0.0 {
        return Ok(transition_speed);
    }

    let residual = |speed: f64| {
        let reynolds =
            (air_density_kg_m3 * speed * diameter_m / air_dynamic_viscosity_pa_s).max(1.0e-15);
        let drag = if reynolds < drag_transition_reynolds {
            (24.0 / reynolds) * (1.0 + 0.15 * reynolds.powf(0.687))
        } else {
            high_reynolds_drag_coefficient
        };
        speed * speed * drag - force_scale
    };
    let mut lower = 0.0;
    let mut upper = 1.0;
    let mut bracket_iterations = 0_u32;
    while residual(upper) < 0.0 && bracket_iterations < maximum_iterations {
        upper *= 2.0;
        bracket_iterations += 1;
    }
    if residual(upper) < 0.0 {
        return Err(EvaluationError::ParticleNodeTerminalSpeedNotBracketed {
            diameter_m,
            density_kg_m3: particle_density_kg_m3,
        });
    }
    for _ in bracket_iterations..maximum_iterations {
        let midpoint = 0.5 * (lower + upper);
        if upper - lower <= relative_tolerance * midpoint.max(1.0) {
            return Ok(midpoint);
        }
        if residual(midpoint) < 0.0 {
            lower = midpoint;
        } else {
            upper = midpoint;
        }
    }
    Err(EvaluationError::ParticleNodeTerminalSpeedDidNotConverge {
        diameter_m,
        density_kg_m3: particle_density_kg_m3,
        maximum_iterations,
    })
}

#[derive(Debug, Error)]
pub enum TMatrixLoadError {
    #[error("whole LUT SHA-256 mismatch: expected {expected}, got {actual}")]
    FileDigestMismatch {
        expected: Sha256Digest,
        actual: Sha256Digest,
    },
    #[error("external generator config bytes differ from the exact bytes embedded in the LUT")]
    ExternalConfigBytesMismatch,
    #[error("generator config is not strict schema-v1 JSON: {0}")]
    GeneratorConfigJson(#[source] serde_json::Error),
    #[error("unsupported or inconsistent generator config field {field}: {detail}")]
    InvalidConfig { field: &'static str, detail: String },
    #[error("generator config axis {index} does not exactly match the LUT header")]
    AxisMismatch { index: usize },
    #[error("generator config has {config} axes but the LUT header has {header}")]
    AxisCountMismatch { config: usize, header: usize },
    #[error("generator config science does not exactly match LUT header science: {field}")]
    ScienceMismatch { field: &'static str },
    #[error(transparent)]
    OfflineLut(#[from] LutError),
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum EvaluationError {
    #[error("{field} must be finite and positive, got {value}")]
    InvalidQuery { field: &'static str, value: f64 },
    #[error("table family mismatch: expected {expected:?}, got {actual:?}")]
    FamilyMismatch {
        expected: MicrophysicsFamily,
        actual: MicrophysicsFamily,
    },
    #[error("table population {expected:?} is not applicable to state family {actual:?}")]
    PopulationApplicabilityMismatch {
        expected: TMatrixParticleCategory,
        actual: MicrophysicsFamily,
    },
    #[error("table category mismatch: expected {expected:?}, got {actual:?}")]
    CategoryMismatch {
        expected: TMatrixParticleCategory,
        actual: TMatrixParticleCategory,
    },
    #[error(
        "dry per-particle PSD evaluation requires a dry property-aware table, got {actual_category:?}/{actual_role:?}"
    )]
    DryParticleNodeTableRequired {
        actual_category: TMatrixParticleCategory,
        actual_role: TMatrixPopulationRole,
    },
    #[error("per-particle PSD table normalization must be exactly 1 m^-3, got {actual_m3}")]
    ParticleNodeNormalizationMismatch { actual_m3: f64 },
    #[error("PSD node habit {habit:?} cannot use requested spheroid convention {actual:?}")]
    ParticleNodeSpheroidMismatch {
        habit: PsdSpheroidHabit,
        actual: SpheroidConvention,
    },
    #[error(
        "PSD node habit {habit:?} is inconsistent with minor-to-major ratio {minor_to_major_axis_ratio}"
    )]
    ParticleNodeHabitGeometryMismatch {
        habit: PsdSpheroidHabit,
        minor_to_major_axis_ratio: f64,
    },
    #[error("dry per-particle PSD evaluation requires a dry ice material table")]
    DryParticleNodeMaterialRequired,
    #[error("dry per-particle PSD evaluation requires the table's Schiller-Naumann speed policy")]
    DryParticleNodeTerminalSpeedPolicyRequired,
    #[error(
        "PSD particle-node terminal-speed provenance mismatch: expected {expected:?}, got {actual:?}"
    )]
    ParticleNodeFallSpeedProvenanceMismatch {
        expected: PsdFallSpeedProvenance,
        actual: PsdFallSpeedProvenance,
    },
    #[error(
        "PSD particle-node terminal speed mismatch: exact table policy gives {expected_m_s} m s^-1, query supplied {actual_m_s} m s^-1"
    )]
    ParticleNodeFallSpeedValueMismatch { expected_m_s: f64, actual_m_s: f64 },
    #[error(
        "PSD particle density {particle_density_kg_m3} kg m^-3 must exceed terminal-policy air density {air_density_kg_m3} kg m^-3"
    )]
    ParticleNodeDensityNotAboveAir {
        particle_density_kg_m3: f64,
        air_density_kg_m3: f64,
    },
    #[error(
        "could not bracket PSD particle terminal speed at D={diameter_m} m, density={density_kg_m3} kg m^-3"
    )]
    ParticleNodeTerminalSpeedNotBracketed { diameter_m: f64, density_kg_m3: f64 },
    #[error(
        "PSD particle terminal speed did not converge in {maximum_iterations} iterations at D={diameter_m} m, density={density_kg_m3} kg m^-3"
    )]
    ParticleNodeTerminalSpeedDidNotConverge {
        diameter_m: f64,
        density_kg_m3: f64,
        maximum_iterations: u32,
    },
    #[error("particle category has no number concentration; PSD integration is unsupported")]
    MissingNumberConcentration,
    #[error("wet-coexistence/mixed-material evaluation is not implemented")]
    UnsupportedWetCoexistence,
    #[error("wet-category evaluation requires the property-aware Bruggeman table")]
    WetCategoryTableRequired,
    #[error("wet property tables require DiagnosticWetCategory input")]
    WetCategoryInputRequired,
    #[error(
        "property phase mismatch: expected {expected}, got liquid mass fraction {actual_liquid_mass_fraction}"
    )]
    PhaseRegimeMismatch {
        expected: &'static str,
        actual_liquid_mass_fraction: f64,
    },
    #[error("unused-rain evaluation requires a declared residual conventional-rain table")]
    ResidualRainTableRequired,
    #[error("unused-rain source must be a closed conventional rain category")]
    ResidualRainSourceRequired,
    #[error("wet-category mixture topology {0:?} is unsupported")]
    UnsupportedMixtureTopology(MixtureTopology),
    #[error("wet-category canting cannot be represented as an exact Gaussian ODF")]
    WetOrientationUnavailable,
    #[error("wet-category mass/number/density produced invalid characteristic diameter {value}")]
    InvalidWetCharacteristicDiameter { value: f64 },
    #[error(
        "bulk density {bulk_density_kg_m3} kg m^-3 and liquid fraction {liquid_mass_fraction} produce invalid condensed volume fraction {value}"
    )]
    InvalidCondensedVolumeFraction {
        bulk_density_kg_m3: f64,
        liquid_mass_fraction: f64,
        value: f64,
    },
    #[error("closure-derived positive-down fall speed is invalid: {value} m s^-1")]
    InvalidClosureFallSpeed { value: f64 },
    #[error(
        "residual rain mass {residual_kgkg} kg/kg must be positive and no greater than original {original_kgkg} kg/kg"
    )]
    InvalidResidualRainMass {
        residual_kgkg: f64,
        original_kgkg: f64,
    },
    #[error("table shape convention mismatch: expected {expected:?}, got {actual:?}")]
    SpheroidConventionMismatch {
        expected: SpheroidConvention,
        actual: SpheroidConvention,
    },
    #[error("ODF mismatch: table uses {expected:?}, particle closure uses {actual:?}")]
    OrientationMismatch {
        expected: OrientationModel,
        actual: OrientationModel,
    },
    #[error("fixed dielectric temperature mismatch: expected {expected_k} K, got {actual_k} K")]
    FixedDielectricTemperatureMismatch { expected_k: f64, actual_k: f64 },
    #[error(
        "homogeneous material density mismatch: expected {expected_kg_m3} kg m^-3, got {actual_kg_m3} kg m^-3"
    )]
    MaterialDensityMismatch {
        expected_kg_m3: f64,
        actual_kg_m3: f64,
    },
    #[error("table axis {0:?} is unsupported by the closed-particle evaluator")]
    UnsupportedAxis(AxisKind),
    #[error("closed particle does not provide property required by axis {0:?}")]
    MissingAxisProperty(AxisKind),
    #[error("PSD particle node does not provide property required by axis {0:?}")]
    MissingParticleNodeAxisProperty(AxisKind),
    #[error("dry particle-node table does not provide required axis {0:?}")]
    MissingParticleNodeTableAxis(AxisKind),
    #[error("dry particle-node table domain is invalid: {0}")]
    ParticleNodeDomain(#[source] PsdError),
    #[error(
        "dry particle-node table must bind exactly one frequency coordinate, got {actual_coordinates}"
    )]
    ParticleNodeFrequencyMustBeSingleton { actual_coordinates: usize },
    #[error(
        "dry particle-node table requires exact frequency {expected_hz} Hz, got {actual_hz} Hz"
    )]
    ParticleNodeFrequencyMismatch { expected_hz: f64, actual_hz: f64 },
    #[error("number concentration per cubic metre is invalid: {value}")]
    InvalidNumberDensity { value: f64 },
    #[error(transparent)]
    Interpolation(#[from] InterpolationError),
    #[error("scaled/accumulated additive output is invalid: {0}")]
    Output(#[source] OutputError),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGeneratorConfig {
    schema: u16,
    status: String,
    kernel: String,
    table_id: String,
    particle_population: RawPopulation,
    axes: Vec<RawAxis>,
    dielectric: RawDielectric,
    orientation: RawOrientation,
    radar: RawRadar,
    terminal_velocity: RawTerminalSpeed,
    temporal: RawTemporal,
    execution: RawExecution,
    payload: RawPayload,
    references: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPopulation {
    microphysics_family: String,
    category: String,
    shape_family: String,
    size_distribution: String,
    normalization_number_concentration_m3: f64,
    #[serde(default)]
    phase_regime: Option<String>,
    #[serde(default)]
    state_descriptor: Option<RawPropertyStateDescriptor>,
    #[serde(default)]
    coexistence_descriptor: Option<RawCoexistenceDescriptor>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCoexistenceDescriptor {
    role: String,
    allocation_rule: String,
    double_count_policy: String,
    over_pairing_policy: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPropertyStateDescriptor {
    compatible_closed_state_families: Vec<String>,
    characteristic_diameter_mapping: String,
    bulk_density_mapping: String,
    #[serde(default)]
    condensed_volume_fraction_definition: Option<String>,
    shape_mapping: String,
    liquid_mapping: String,
    phase_dispatch: String,
    rime_axes: String,
    rime_effect_on_dielectric: String,
    psd_mapping: String,
    extrapolation: String,
    density_applicability: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAxis {
    kind: AxisKind,
    unit: Unit,
    coordinates: Vec<f64>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawComplexIndex {
    real: f64,
    imaginary: f64,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "model", deny_unknown_fields)]
enum RawDielectric {
    #[serde(rename = "explicit_homogeneous")]
    ExplicitHomogeneous {
        material: String,
        refractive_index: RawComplexIndex,
        mass_density_kg_m3: f64,
        temperature_k: f64,
        frequency_dependence: String,
    },
    #[serde(rename = "maxwell_garnett_ice_host_water_inclusion")]
    MaxwellGarnettIceHostWaterInclusion {
        ice_refractive_index: RawComplexIndex,
        liquid_water_refractive_index: RawComplexIndex,
        ice_density_kg_m3: f64,
        liquid_water_density_kg_m3: f64,
        temperature_k: f64,
        mass_to_volume_fraction_conversion: String,
        frequency_dependence: String,
    },
    #[serde(rename = "symmetric_bruggeman_spherical_air_ice_water_v1")]
    SymmetricBruggemanSphericalAirIceWaterV1 {
        air_relative_permittivity: RawComplexIndex,
        ice_permittivity_model: String,
        liquid_water_permittivity_model: String,
        ice_temperature_treatment: String,
        ice_material_density_kg_m3: f64,
        liquid_water_density_kg_m3: f64,
        condensed_volume_fraction_interpretation: String,
        liquid_mass_fraction_interpretation: String,
        component_volume_fraction_conversion: String,
        bulk_density_reconstruction: String,
        mixing_equation: String,
        root_selection: String,
        homotopy_steps: u32,
        newton_max_iterations: u32,
        newton_relative_tolerance: f64,
        temperature_range_k: [f64; 2],
        applicability: String,
    },
    #[serde(rename = "temperature_dependent_liquid_water_liebe_1991")]
    TemperatureDependentLiquidWaterLiebe1991 {
        liquid_water_permittivity_model: String,
        mass_density_kg_m3: f64,
        temperature_range_k: [f64; 2],
        frequency_range_hz: [f64; 2],
        applicability: String,
    },
    #[serde(rename = "symmetric_bruggeman_spherical_air_ice_matzler_2006_v1")]
    SymmetricBruggemanSphericalAirIceMatzler2006V1 {
        air_relative_permittivity: RawComplexIndex,
        ice_permittivity_model: String,
        ice_material_density_kg_m3: f64,
        bulk_density_interpretation: String,
        component_volume_fraction_conversion: String,
        mixing_equation: String,
        root_selection: String,
        homotopy_steps: u32,
        newton_max_iterations: u32,
        newton_relative_tolerance: f64,
        temperature_range_k: [f64; 2],
        temperature_evidence: String,
        applicability: String,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "model", deny_unknown_fields)]
enum RawOrientation {
    #[serde(rename = "fixed_euler")]
    FixedEuler {
        yaw_deg: f64,
        pitch_deg: f64,
        roll_deg: f64,
        pytmatrix_alpha_deg: f64,
        pytmatrix_beta_deg: f64,
        symmetry_axis: String,
    },
    #[serde(rename = "gaussian_canting")]
    GaussianCanting {
        mean_deg: f64,
        standard_deviation_deg: f64,
        alpha_quadrature_points: u16,
        beta_quadrature_points: u16,
        quadrature_method: String,
        reference_symmetry_axis: String,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRadar {
    speed_of_light_m_s: f64,
    reference_water_dielectric_factor_squared: f64,
    length_unit_passed_to_pytmatrix: String,
    backscatter_geometry_deg: [f64; 6],
    forward_scatter_geometry_deg: [f64; 6],
    covariance_phase_convention: String,
    beam_elevation_transform: String,
    polarization_basis: String,
    view_applicability: String,
    solver: RawSolver,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSolver {
    shape: String,
    ddelt: f64,
    ndgs: u32,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "law", deny_unknown_fields)]
enum RawTerminalSpeed {
    #[serde(rename = "atlas_rain_1973_exponential")]
    AtlasRain1973Exponential {
        a_m_s: f64,
        b_m_s: f64,
        c_per_mm: f64,
        valid_diameter_range_m: [f64; 2],
    },
    #[serde(rename = "schiller_naumann_gravity_drag")]
    SchillerNaumannGravityDrag {
        gravity_m_s2: f64,
        air_density_kg_m3: f64,
        air_dynamic_viscosity_pa_s: f64,
        drag_transition_reynolds: f64,
        high_reynolds_drag_coefficient: f64,
        drag_transition_boundary_policy: String,
        maximum_iterations: u32,
        relative_tolerance: f64,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTemporal {
    sampling: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawExecution {
    point_timeout_seconds: u64,
    process_isolation: String,
    result_collection_order: String,
    partial_grid_policy: String,
    thread_count_per_process: u32,
    #[serde(default)]
    grouping: Option<RawExecutionGrouping>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawExecutionGrouping {
    model: String,
    material_state_axis_kinds: Vec<AxisKind>,
    tmatrix_state_axis_kinds: Vec<AxisKind>,
    geometry_axis_kind: AxisKind,
    partial_group_policy: String,
    maximum_points_per_process: u32,
    group_timeout_seconds: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPayload {
    encoding: String,
}

fn bind_descriptor(
    lut: &OfflineLut,
    config: RawGeneratorConfig,
) -> Result<TMatrixTableDescriptor, TMatrixLoadError> {
    exact_u16("schema", config.schema, 1)?;
    exact_text("status", &config.status, RESEARCH_STATUS)?;
    exact_text("kernel", &config.kernel, PYTMATRIX_KERNEL)?;
    nonempty("table_id", &config.table_id)?;
    if config
        .particle_population
        .normalization_number_concentration_m3
        != 1.0
    {
        return invalid(
            "particle_population.normalization_number_concentration_m3",
            "must be exactly 1 m^-3",
        );
    }
    let (category, spheroid, population_role, density_applicability) =
        bind_population(config.particle_population)?;

    verify_axes(lut, &config.axes)?;
    let material = bind_material(config.dielectric)?;
    verify_axis_contract(lut, &material)?;
    verify_category_material(category, population_role, &material)?;
    let odf = bind_orientation(config.orientation)?;
    let radar = bind_radar(config.radar, lut, population_role)?;
    let terminal_speed = bind_terminal_speed(config.terminal_velocity, lut)?;
    verify_category_terminal(category, &terminal_speed)?;
    let terminal_speed_sha256 = terminal_speed_policy_sha256(&terminal_speed);
    exact_text(
        "temporal.sampling",
        &config.temporal.sampling,
        "instantaneous",
    )?;
    let execution = verify_execution(config.execution, &material)?;
    exact_text(
        "payload.encoding",
        &config.payload.encoding,
        "f64_le_point_major_last_axis_fastest",
    )?;
    if config.references.is_empty()
        || config
            .references
            .iter()
            .any(|value| value.trim().is_empty())
    {
        return invalid("references", "must contain only nonempty citations");
    }

    verify_header_science(lut, &material, &odf)?;
    Ok(TMatrixTableDescriptor {
        table_id: config.table_id,
        category,
        population_role,
        density_applicability,
        spheroid,
        material,
        odf,
        radar,
        terminal_speed,
        terminal_speed_sha256,
        execution,
        normalization_number_concentration_m3: 1.0,
    })
}

fn bind_population(
    raw: RawPopulation,
) -> Result<
    (
        TMatrixParticleCategory,
        SpheroidConvention,
        TMatrixPopulationRole,
        DensityApplicability,
    ),
    TMatrixLoadError,
> {
    let spheroid = match raw.shape_family.as_str() {
        "oblate_spheroid" => SpheroidConvention::OblateMinorVertical,
        "prolate_spheroid" => SpheroidConvention::ProlateMajorVertical,
        other => {
            return invalid(
                "particle_population.shape_family",
                format!("unsupported shape convention {other:?}"),
            );
        }
    };
    let (category, role, density_applicability) = match raw.microphysics_family.as_str() {
        "conventional" => {
            if raw.phase_regime.is_some() {
                return invalid(
                    "particle_population.phase_regime",
                    "is forbidden for conventional tables",
                );
            }
            exact_text(
                "particle_population.size_distribution",
                &raw.size_distribution,
                MONODISPERSE_NODE,
            )?;
            if raw.state_descriptor.is_some() {
                return invalid(
                    "particle_population.state_descriptor",
                    "is forbidden for conventional tables",
                );
            }
            let category = match raw.category.as_str() {
                "rain" => TMatrixParticleCategory::Conventional(ConventionalHydrometeor::Rain),
                "hail" => TMatrixParticleCategory::Conventional(ConventionalHydrometeor::Hail),
                other => {
                    return invalid(
                        "particle_population.category",
                        format!("unsupported conventional category {other:?}"),
                    );
                }
            };
            let role = if let Some(descriptor) = raw.coexistence_descriptor {
                if category != TMatrixParticleCategory::Conventional(ConventionalHydrometeor::Rain)
                {
                    return invalid(
                        "particle_population.coexistence_descriptor",
                        "is valid only for conventional rain",
                    );
                }
                verify_coexistence_descriptor(descriptor)?;
                TMatrixPopulationRole::ConventionalRainStandaloneAndResidual
            } else {
                TMatrixPopulationRole::OrdinaryConventional
            };
            (category, role, DensityApplicability::ConventionalCategory)
        }
        "property_aware_p3_ishmael" => {
            let phase = match raw.phase_regime.as_deref() {
                Some("dry") => TMatrixPopulationRole::PropertyAwareDryCharacteristicParticle,
                Some("wet") => TMatrixPopulationRole::PropertyAwareWetCharacteristicParticle,
                actual => {
                    return invalid(
                        "particle_population.phase_regime",
                        format!("must be exactly dry or wet, got {actual:?}"),
                    );
                }
            };
            exact_text(
                "particle_population.category",
                &raw.category,
                "frozen_characteristic_particle",
            )?;
            exact_text(
                "particle_population.size_distribution",
                &raw.size_distribution,
                "monodisperse_characteristic_particle_node",
            )?;
            let density_applicability = verify_property_state_descriptor(
                raw.state_descriptor
                    .ok_or_else(|| TMatrixLoadError::InvalidConfig {
                        field: "particle_population.state_descriptor",
                        detail: "is required for property-aware tables".to_owned(),
                    })?,
                phase,
            )?;
            if raw.coexistence_descriptor.is_some() {
                return invalid(
                    "particle_population.coexistence_descriptor",
                    "is forbidden for property-aware characteristic tables",
                );
            }
            (
                TMatrixParticleCategory::PropertyAwareFrozenCharacteristicParticle,
                phase,
                density_applicability,
            )
        }
        other => {
            return invalid(
                "particle_population.microphysics_family",
                format!("unsupported family {other:?}"),
            );
        }
    };
    Ok((category, spheroid, role, density_applicability))
}

fn verify_coexistence_descriptor(raw: RawCoexistenceDescriptor) -> Result<(), TMatrixLoadError> {
    for (field, actual, expected) in [
        (
            "particle_population.coexistence_descriptor.role",
            raw.role.as_str(),
            "standalone_rain_and_residual_after_mixed_phase_pairing",
        ),
        (
            "particle_population.coexistence_descriptor.allocation_rule",
            raw.allocation_rule.as_str(),
            "max_total_rain_mass_minus_liquid_mass_paired_into_wet_frozen_categories_zero",
        ),
        (
            "particle_population.coexistence_descriptor.double_count_policy",
            raw.double_count_policy.as_str(),
            "paired_liquid_mass_removed_exactly_once_before_rain_lookup",
        ),
        (
            "particle_population.coexistence_descriptor.over_pairing_policy",
            raw.over_pairing_policy.as_str(),
            "reject",
        ),
    ] {
        exact_text(field, actual, expected)?;
    }
    Ok(())
}

fn verify_property_state_descriptor(
    raw: RawPropertyStateDescriptor,
    phase: TMatrixPopulationRole,
) -> Result<DensityApplicability, TMatrixLoadError> {
    if raw.compatible_closed_state_families != ["p3", "ishmael"] {
        return invalid(
            "particle_population.state_descriptor.compatible_closed_state_families",
            "must exactly equal [\"p3\", \"ishmael\"]",
        );
    }
    let (bulk_mapping, liquid_mapping, phase_dispatch) = match phase {
        TMatrixPopulationRole::PropertyAwareDryCharacteristicParticle => (
            "closure_derived_effective_bulk_density_including_rime_mass_and_rime_density",
            "required_exactly_zero_liquid_mass_fraction",
            "liquid_mass_fraction_equal_zero_selects_dry_table",
        ),
        TMatrixPopulationRole::PropertyAwareWetCharacteristicParticle => (
            "closure_bulk_density_and_liquid_mass_fraction_mapped_to_condensed_volume_fraction",
            "diagnosed_or_prescribed_strictly_positive_liquid_mass_fraction",
            "liquid_mass_fraction_greater_than_zero_selects_wet_table",
        ),
        _ => unreachable!("only property-aware roles reach state descriptor validation"),
    };
    for (field, actual, expected) in [
        (
            "particle_population.state_descriptor.characteristic_diameter_mapping",
            raw.characteristic_diameter_mapping.as_str(),
            "closure_derived_equivolume_characteristic_diameter",
        ),
        (
            "particle_population.state_descriptor.bulk_density_mapping",
            raw.bulk_density_mapping.as_str(),
            bulk_mapping,
        ),
        (
            "particle_population.state_descriptor.shape_mapping",
            raw.shape_mapping.as_str(),
            "closure_derived_minor_to_major_axis_ratio",
        ),
        (
            "particle_population.state_descriptor.liquid_mapping",
            raw.liquid_mapping.as_str(),
            liquid_mapping,
        ),
        (
            "particle_population.state_descriptor.phase_dispatch",
            raw.phase_dispatch.as_str(),
            phase_dispatch,
        ),
        (
            "particle_population.state_descriptor.rime_axes",
            raw.rime_axes.as_str(),
            "not_explicit_rime_influences_only_through_bulk_density_and_shape",
        ),
        (
            "particle_population.state_descriptor.rime_effect_on_dielectric",
            raw.rime_effect_on_dielectric.as_str(),
            "none_given_bulk_density",
        ),
        (
            "particle_population.state_descriptor.psd_mapping",
            raw.psd_mapping.as_str(),
            "none_monodisperse_characteristic_particle_not_scheme_native_psd",
        ),
        (
            "particle_population.state_descriptor.extrapolation",
            raw.extrapolation.as_str(),
            "forbidden",
        ),
    ] {
        exact_text(field, actual, expected)?;
    }
    let density_applicability = match phase {
        TMatrixPopulationRole::PropertyAwareDryCharacteristicParticle => {
            if raw.condensed_volume_fraction_definition.is_some() {
                return invalid(
                    "particle_population.state_descriptor.condensed_volume_fraction_definition",
                    "is forbidden for dry bulk-density tables",
                );
            }
            exact_text(
                "particle_population.state_descriptor.density_applicability",
                &raw.density_applicability,
                "bulk_density_1p5_to_917_kg_m3_downward_fall_requires_density_above_1p225_kg_m3_air",
            )?;
            DensityApplicability::DryBulkDensity15To917KgM3Above1225Air
        }
        TMatrixPopulationRole::PropertyAwareWetCharacteristicParticle => {
            exact_text(
                "particle_population.state_descriptor.condensed_volume_fraction_definition",
                raw.condensed_volume_fraction_definition
                    .as_deref()
                    .ok_or_else(|| TMatrixLoadError::InvalidConfig {
                        field: "particle_population.state_descriptor.condensed_volume_fraction_definition",
                        detail: "is required for wet tables".to_owned(),
                    })?,
                "rho_bulk_times_open_parenthesis_one_minus_w_over_917_plus_w_over_999p84_close_parenthesis",
            )?;
            exact_text(
                "particle_population.state_descriptor.density_applicability",
                &raw.density_applicability,
                "condensed_volume_fraction_0p0015_to_1_downward_fall_requires_reconstructed_density_above_1p225_kg_m3_air",
            )?;
            DensityApplicability::WetCondensedVolumeFraction00015To1Above1225Air
        }
        _ => unreachable!("only property-aware roles reach state descriptor validation"),
    };
    Ok(density_applicability)
}

fn verify_axes(lut: &OfflineLut, config_axes: &[RawAxis]) -> Result<(), TMatrixLoadError> {
    let header_axes = lut.header().axes();
    if config_axes.len() != header_axes.len() {
        return Err(TMatrixLoadError::AxisCountMismatch {
            config: config_axes.len(),
            header: header_axes.len(),
        });
    }
    for (index, (config, header)) in config_axes.iter().zip(header_axes).enumerate() {
        if config.kind != header.kind()
            || config.unit != header.unit()
            || config.coordinates.as_slice() != header.coordinates()
        {
            return Err(TMatrixLoadError::AxisMismatch { index });
        }
    }
    Ok(())
}

fn verify_axis_contract(
    lut: &OfflineLut,
    material: &TMatrixMaterial,
) -> Result<(), TMatrixLoadError> {
    let actual: Vec<_> = lut.header().axes().iter().map(|axis| axis.kind()).collect();
    let expected: &[AxisKind] = match material {
        TMatrixMaterial::Homogeneous { .. } => &[
            AxisKind::EquivolumeDiameter,
            AxisKind::MinorToMajorAxisRatio,
            AxisKind::Frequency,
            AxisKind::RadarElevation,
        ],
        TMatrixMaterial::MaxwellGarnettIceHostWaterInclusion { .. } => &[
            AxisKind::EquivolumeDiameter,
            AxisKind::LiquidMassFraction,
            AxisKind::MinorToMajorAxisRatio,
            AxisKind::Frequency,
            AxisKind::RadarElevation,
        ],
        TMatrixMaterial::SymmetricBruggemanSphericalAirIceWaterV1 { .. } => &[
            AxisKind::EquivolumeDiameter,
            AxisKind::Temperature,
            AxisKind::CondensedVolumeFraction,
            AxisKind::LiquidMassFraction,
            AxisKind::MinorToMajorAxisRatio,
            AxisKind::Frequency,
            AxisKind::RadarElevation,
        ],
        TMatrixMaterial::SymmetricBruggemanSphericalAirIceMatzler2006V1 { .. } => &[
            AxisKind::EquivolumeDiameter,
            AxisKind::Temperature,
            AxisKind::BulkDensity,
            AxisKind::MinorToMajorAxisRatio,
            AxisKind::Frequency,
            AxisKind::RadarElevation,
        ],
        TMatrixMaterial::TemperatureDependentLiquidWaterLiebe1991 { .. } => &[
            AxisKind::EquivolumeDiameter,
            AxisKind::Temperature,
            AxisKind::MinorToMajorAxisRatio,
            AxisKind::Frequency,
            AxisKind::RadarElevation,
        ],
    };
    if actual != expected {
        return invalid(
            "axes",
            format!("expected exact research-generator order {expected:?}, got {actual:?}"),
        );
    }
    if let Some(temperature) = lut
        .header()
        .axes()
        .iter()
        .find(|axis| axis.kind() == AxisKind::Temperature)
    {
        let bounds = [
            temperature.coordinates()[0],
            temperature.coordinates()[temperature.coordinates().len() - 1],
        ];
        let expected_bounds = match material {
            TMatrixMaterial::SymmetricBruggemanSphericalAirIceWaterV1 { .. } => {
                Some([269.15, 275.15])
            }
            TMatrixMaterial::SymmetricBruggemanSphericalAirIceMatzler2006V1 {
                temperature_range_k,
                ..
            } => Some(*temperature_range_k),
            TMatrixMaterial::TemperatureDependentLiquidWaterLiebe1991 {
                temperature_range_k,
                ..
            } => Some(*temperature_range_k),
            _ => None,
        };
        if let Some(expected_bounds) = expected_bounds
            && bounds != expected_bounds
        {
            return invalid(
                "axes.temperature",
                format!("must exactly span {expected_bounds:?} K, got {bounds:?}"),
            );
        }
    }
    for axis in lut.header().axes() {
        for &value in axis.coordinates() {
            let valid = match axis.kind() {
                AxisKind::EquivolumeDiameter => 0.0 < value && value <= 0.1,
                AxisKind::Temperature => (150.0..=350.0).contains(&value),
                AxisKind::BulkDensity => 0.0 < value && value <= 1_100.0,
                AxisKind::CondensedVolumeFraction => 0.0 < value && value <= 1.0,
                AxisKind::LiquidMassFraction => (0.0..=1.0).contains(&value),
                AxisKind::MinorToMajorAxisRatio => 0.0 < value && value <= 1.0,
                AxisKind::Frequency => (2.0e9..=4.0e9).contains(&value),
                AxisKind::RadarElevation => (-90.0..=90.0).contains(&value),
                _ => false,
            };
            if !valid {
                return invalid(
                    "axes",
                    format!("invalid {:?} coordinate {value}", axis.kind()),
                );
            }
        }
    }
    Ok(())
}

fn bind_material(raw: RawDielectric) -> Result<TMatrixMaterial, TMatrixLoadError> {
    match raw {
        RawDielectric::ExplicitHomogeneous {
            material,
            refractive_index,
            mass_density_kg_m3,
            temperature_k,
            frequency_dependence,
        } => {
            exact_text(
                "dielectric.frequency_dependence",
                &frequency_dependence,
                CONSTANT_S_BAND_DIELECTRIC,
            )?;
            positive("dielectric.mass_density_kg_m3", mass_density_kg_m3)?;
            positive("dielectric.temperature_k", temperature_k)?;
            let refractive_index =
                bind_refractive_index("dielectric.refractive_index", refractive_index)?;
            let material = match material.as_str() {
                "liquid_water" => HomogeneousMaterial::LiquidWater,
                "ice" => HomogeneousMaterial::Ice,
                other => {
                    return invalid(
                        "dielectric.material",
                        format!("unsupported homogeneous material {other:?}"),
                    );
                }
            };
            Ok(TMatrixMaterial::Homogeneous {
                material,
                refractive_index,
                mass_density_kg_m3,
                temperature_k,
            })
        }
        RawDielectric::MaxwellGarnettIceHostWaterInclusion {
            ice_refractive_index,
            liquid_water_refractive_index,
            ice_density_kg_m3,
            liquid_water_density_kg_m3,
            temperature_k,
            mass_to_volume_fraction_conversion,
            frequency_dependence,
        } => {
            exact_text(
                "dielectric.mass_to_volume_fraction_conversion",
                &mass_to_volume_fraction_conversion,
                "component_specific_volume",
            )?;
            exact_text(
                "dielectric.frequency_dependence",
                &frequency_dependence,
                CONSTANT_S_BAND_DIELECTRIC,
            )?;
            positive("dielectric.ice_density_kg_m3", ice_density_kg_m3)?;
            positive(
                "dielectric.liquid_water_density_kg_m3",
                liquid_water_density_kg_m3,
            )?;
            positive("dielectric.temperature_k", temperature_k)?;
            Ok(TMatrixMaterial::MaxwellGarnettIceHostWaterInclusion {
                ice_refractive_index: bind_refractive_index(
                    "dielectric.ice_refractive_index",
                    ice_refractive_index,
                )?,
                liquid_water_refractive_index: bind_refractive_index(
                    "dielectric.liquid_water_refractive_index",
                    liquid_water_refractive_index,
                )?,
                ice_density_kg_m3,
                liquid_water_density_kg_m3,
                temperature_k,
            })
        }
        RawDielectric::SymmetricBruggemanSphericalAirIceWaterV1 {
            air_relative_permittivity,
            ice_permittivity_model,
            liquid_water_permittivity_model,
            ice_temperature_treatment,
            ice_material_density_kg_m3,
            liquid_water_density_kg_m3,
            condensed_volume_fraction_interpretation,
            liquid_mass_fraction_interpretation,
            component_volume_fraction_conversion,
            bulk_density_reconstruction,
            mixing_equation,
            root_selection,
            homotopy_steps,
            newton_max_iterations,
            newton_relative_tolerance,
            temperature_range_k,
            applicability,
        } => {
            let air_relative_permittivity = bind_refractive_index(
                "dielectric.air_relative_permittivity",
                air_relative_permittivity,
            )?;
            if air_relative_permittivity
                != (ComplexRefractiveIndex {
                    real: 1.0,
                    imaginary: 0.0,
                })
            {
                return invalid(
                    "dielectric.air_relative_permittivity",
                    "must exactly equal 1+0i",
                );
            }
            exact_text(
                "dielectric.ice_permittivity_model",
                &ice_permittivity_model,
                "matzler_2006",
            )?;
            exact_text(
                "dielectric.liquid_water_permittivity_model",
                &liquid_water_permittivity_model,
                "liebe_hufford_manabe_1991_double_debye",
            )?;
            exact_text(
                "dielectric.ice_temperature_treatment",
                &ice_temperature_treatment,
                "minimum_environment_temperature_and_273p15_k_phase_equilibrium",
            )?;
            if ice_material_density_kg_m3 != 917.0 {
                return invalid(
                    "dielectric.ice_material_density_kg_m3",
                    "must exactly equal property-closure-v1 density 917 kg m^-3",
                );
            }
            if liquid_water_density_kg_m3 != 999.84 {
                return invalid(
                    "dielectric.liquid_water_density_kg_m3",
                    "must exactly equal 999.84 kg m^-3",
                );
            }
            for (field, actual, expected) in [
                (
                    "dielectric.condensed_volume_fraction_interpretation",
                    condensed_volume_fraction_interpretation.as_str(),
                    "ice_plus_liquid_component_volume_over_outer_spheroid_volume",
                ),
                (
                    "dielectric.liquid_mass_fraction_interpretation",
                    liquid_mass_fraction_interpretation.as_str(),
                    "liquid_mass_over_total_condensed_mass",
                ),
                (
                    "dielectric.component_volume_fraction_conversion",
                    component_volume_fraction_conversion.as_str(),
                    "condensed_volume_fraction_times_mass_specific_volume_shares",
                ),
                (
                    "dielectric.bulk_density_reconstruction",
                    bulk_density_reconstruction.as_str(),
                    "condensed_volume_fraction_divided_by_total_component_specific_volume",
                ),
                (
                    "dielectric.mixing_equation",
                    mixing_equation.as_str(),
                    "sum_f_j_times_eps_j_minus_eps_eff_over_eps_j_plus_2eps_eff_equals_zero",
                ),
                (
                    "dielectric.root_selection",
                    root_selection.as_str(),
                    "vacuum_to_constituents_homotopy_passive_continuous_branch",
                ),
                (
                    "dielectric.applicability",
                    applicability.as_str(),
                    "quasistatic_spherical_inclusions_homogeneous_effective_medium",
                ),
            ] {
                exact_text(field, actual, expected)?;
            }
            if homotopy_steps != 64 || newton_max_iterations != 100 {
                return invalid(
                    "dielectric solver iterations",
                    "homotopy_steps must be 64 and newton_max_iterations must be 100",
                );
            }
            if newton_relative_tolerance != 1.0e-12 {
                return invalid(
                    "dielectric.newton_relative_tolerance",
                    "must exactly equal 1e-12",
                );
            }
            if temperature_range_k != [269.15, 275.15] {
                return invalid(
                    "dielectric.temperature_range_k",
                    "must exactly equal [269.15, 275.15] K",
                );
            }
            Ok(TMatrixMaterial::SymmetricBruggemanSphericalAirIceWaterV1 {
                air_relative_permittivity,
                ice_permittivity_model,
                liquid_water_permittivity_model,
                ice_temperature_treatment,
                ice_material_density_kg_m3,
                liquid_water_density_kg_m3,
                homotopy_steps,
                newton_max_iterations,
                newton_relative_tolerance,
                temperature_range_k,
            })
        }
        RawDielectric::TemperatureDependentLiquidWaterLiebe1991 {
            liquid_water_permittivity_model,
            mass_density_kg_m3,
            temperature_range_k,
            frequency_range_hz,
            applicability,
        } => {
            exact_text(
                "dielectric.liquid_water_permittivity_model",
                &liquid_water_permittivity_model,
                "liebe_hufford_manabe_1991_double_debye",
            )?;
            if mass_density_kg_m3 != 999.84 {
                return invalid(
                    "dielectric.mass_density_kg_m3",
                    "must exactly equal 999.84 kg m^-3",
                );
            }
            if temperature_range_k != [250.0, 313.15] {
                return invalid(
                    "dielectric.temperature_range_k",
                    "must exactly equal [250.0, 313.15] K",
                );
            }
            if frequency_range_hz != [2.0e9, 4.0e9] {
                return invalid(
                    "dielectric.frequency_range_hz",
                    "must exactly equal [2e9, 4e9] Hz",
                );
            }
            exact_text(
                "dielectric.applicability",
                &applicability,
                "pure_fresh_supercooled_or_liquid_water_250_to_313p15_k",
            )?;
            Ok(TMatrixMaterial::TemperatureDependentLiquidWaterLiebe1991 {
                mass_density_kg_m3,
                temperature_range_k,
                frequency_range_hz,
            })
        }
        RawDielectric::SymmetricBruggemanSphericalAirIceMatzler2006V1 {
            air_relative_permittivity,
            ice_permittivity_model,
            ice_material_density_kg_m3,
            bulk_density_interpretation,
            component_volume_fraction_conversion,
            mixing_equation,
            root_selection,
            homotopy_steps,
            newton_max_iterations,
            newton_relative_tolerance,
            temperature_range_k,
            temperature_evidence,
            applicability,
        } => {
            let air_relative_permittivity = bind_refractive_index(
                "dielectric.air_relative_permittivity",
                air_relative_permittivity,
            )?;
            if air_relative_permittivity
                != (ComplexRefractiveIndex {
                    real: 1.0,
                    imaginary: 0.0,
                })
            {
                return invalid(
                    "dielectric.air_relative_permittivity",
                    "must exactly equal 1+0i",
                );
            }
            exact_text(
                "dielectric.ice_permittivity_model",
                &ice_permittivity_model,
                "matzler_2006",
            )?;
            if ice_material_density_kg_m3 != 917.0 {
                return invalid(
                    "dielectric.ice_material_density_kg_m3",
                    "must exactly equal 917 kg m^-3",
                );
            }
            for (field, actual, expected) in [
                (
                    "dielectric.bulk_density_interpretation",
                    bulk_density_interpretation.as_str(),
                    "total_ice_mass_per_outer_spheroid_volume",
                ),
                (
                    "dielectric.component_volume_fraction_conversion",
                    component_volume_fraction_conversion.as_str(),
                    "bulk_density_divided_by_ice_material_density",
                ),
                (
                    "dielectric.mixing_equation",
                    mixing_equation.as_str(),
                    "sum_f_j_times_eps_j_minus_eps_eff_over_eps_j_plus_2eps_eff_equals_zero",
                ),
                (
                    "dielectric.root_selection",
                    root_selection.as_str(),
                    "vacuum_to_constituents_homotopy_passive_continuous_branch",
                ),
                (
                    "dielectric.temperature_evidence",
                    temperature_evidence.as_str(),
                    "matzler_2006_formula_warren_brandt_2008_reports_accurate_fit_190_to_258_k_warm_extension_declared_to_273p15_k",
                ),
                (
                    "dielectric.applicability",
                    applicability.as_str(),
                    "quasistatic_spherical_air_in_ice_or_ice_in_air_topology_neutral_homogeneous_effective_medium",
                ),
            ] {
                exact_text(field, actual, expected)?;
            }
            if homotopy_steps != 64
                || newton_max_iterations != 100
                || newton_relative_tolerance != 1.0e-12
                || temperature_range_k != [190.0, 273.15]
            {
                return invalid(
                    "dielectric dry-property solver/range",
                    "requires 64 homotopy steps, 100 Newton iterations, 1e-12 tolerance, and [190,273.15] K",
                );
            }
            Ok(
                TMatrixMaterial::SymmetricBruggemanSphericalAirIceMatzler2006V1 {
                    air_relative_permittivity,
                    ice_material_density_kg_m3,
                    homotopy_steps,
                    newton_max_iterations,
                    newton_relative_tolerance,
                    temperature_range_k,
                },
            )
        }
    }
}

fn bind_refractive_index(
    field: &'static str,
    raw: RawComplexIndex,
) -> Result<ComplexRefractiveIndex, TMatrixLoadError> {
    positive(field, raw.real)?;
    if !raw.imaginary.is_finite() || raw.imaginary < 0.0 {
        return invalid(field, "imaginary part must be finite and nonnegative");
    }
    Ok(ComplexRefractiveIndex {
        real: raw.real,
        imaginary: raw.imaginary,
    })
}

fn verify_category_material(
    category: TMatrixParticleCategory,
    role: TMatrixPopulationRole,
    material: &TMatrixMaterial,
) -> Result<(), TMatrixLoadError> {
    let compatible = matches!(
        (category, material),
        (
            TMatrixParticleCategory::Conventional(ConventionalHydrometeor::Rain),
            TMatrixMaterial::Homogeneous {
                material: HomogeneousMaterial::LiquidWater,
                ..
            }
        ) | (
            TMatrixParticleCategory::Conventional(ConventionalHydrometeor::Hail),
            TMatrixMaterial::Homogeneous {
                material: HomogeneousMaterial::Ice,
                ..
            }
        ) | (
            TMatrixParticleCategory::Conventional(ConventionalHydrometeor::Hail),
            TMatrixMaterial::MaxwellGarnettIceHostWaterInclusion { .. }
        ) | (
            TMatrixParticleCategory::PropertyAwareFrozenCharacteristicParticle,
            TMatrixMaterial::SymmetricBruggemanSphericalAirIceWaterV1 { .. }
        ) | (
            TMatrixParticleCategory::PropertyAwareFrozenCharacteristicParticle,
            TMatrixMaterial::SymmetricBruggemanSphericalAirIceMatzler2006V1 { .. }
        ) | (
            TMatrixParticleCategory::Conventional(ConventionalHydrometeor::Rain),
            TMatrixMaterial::TemperatureDependentLiquidWaterLiebe1991 { .. }
        )
    );
    if !compatible {
        return invalid(
            "particle_population.category",
            "category and material/mixing topology are incompatible",
        );
    }
    let role_compatible = matches!(
        (role, material),
        (
            TMatrixPopulationRole::OrdinaryConventional,
            TMatrixMaterial::Homogeneous { .. }
                | TMatrixMaterial::MaxwellGarnettIceHostWaterInclusion { .. }
        ) | (
            TMatrixPopulationRole::ConventionalRainStandaloneAndResidual,
            TMatrixMaterial::TemperatureDependentLiquidWaterLiebe1991 { .. }
        ) | (
            TMatrixPopulationRole::PropertyAwareDryCharacteristicParticle,
            TMatrixMaterial::SymmetricBruggemanSphericalAirIceMatzler2006V1 { .. }
        ) | (
            TMatrixPopulationRole::PropertyAwareWetCharacteristicParticle,
            TMatrixMaterial::SymmetricBruggemanSphericalAirIceWaterV1 { .. }
        )
    );
    if role_compatible {
        Ok(())
    } else {
        invalid(
            "particle_population.phase_regime",
            "population role and dielectric phase applicability are inconsistent",
        )
    }
}

fn bind_orientation(raw: RawOrientation) -> Result<TMatrixOdfConvention, TMatrixLoadError> {
    match raw {
        RawOrientation::FixedEuler {
            yaw_deg,
            pitch_deg,
            roll_deg,
            pytmatrix_alpha_deg,
            pytmatrix_beta_deg,
            symmetry_axis,
        } => {
            for (field, value) in [
                ("orientation.yaw_deg", yaw_deg),
                ("orientation.pitch_deg", pitch_deg),
                ("orientation.roll_deg", roll_deg),
                ("orientation.pytmatrix_alpha_deg", pytmatrix_alpha_deg),
                ("orientation.pytmatrix_beta_deg", pytmatrix_beta_deg),
            ] {
                if value != 0.0 {
                    return invalid(field, "must be exactly zero for aligned vertical ODF");
                }
            }
            exact_text("orientation.symmetry_axis", &symmetry_axis, "vertical")?;
            Ok(TMatrixOdfConvention::FixedAlignedVertical {
                pytmatrix_alpha_deg,
                pytmatrix_beta_deg,
            })
        }
        RawOrientation::GaussianCanting {
            mean_deg,
            standard_deviation_deg,
            alpha_quadrature_points,
            beta_quadrature_points,
            quadrature_method,
            reference_symmetry_axis,
        } => {
            if !mean_deg.is_finite() || !(0.0..180.0).contains(&mean_deg) {
                return invalid("orientation.mean_deg", "must lie in [0, 180) degrees");
            }
            positive("orientation.standard_deviation_deg", standard_deviation_deg)?;
            if alpha_quadrature_points == 0 || beta_quadrature_points == 0 {
                return invalid("orientation quadrature", "point counts must be nonzero");
            }
            alpha_quadrature_points
                .checked_mul(beta_quadrature_points)
                .ok_or_else(|| TMatrixLoadError::InvalidConfig {
                    field: "orientation quadrature",
                    detail: "total point count overflows u16".to_owned(),
                })?;
            exact_text(
                "orientation.quadrature_method",
                &quadrature_method,
                "pytmatrix_orient_averaged_fixed_gautschi",
            )?;
            exact_text(
                "orientation.reference_symmetry_axis",
                &reference_symmetry_axis,
                "vertical_at_zero_canting",
            )?;
            Ok(TMatrixOdfConvention::GaussianCanting {
                mean_deg,
                standard_deviation_deg,
                alpha_quadrature_points,
                beta_quadrature_points,
            })
        }
    }
}

fn bind_radar(
    raw: RawRadar,
    lut: &OfflineLut,
    population_role: TMatrixPopulationRole,
) -> Result<RadarConventionDescriptor, TMatrixLoadError> {
    if raw.speed_of_light_m_s != SPEED_OF_LIGHT_M_S {
        return invalid(
            "radar.speed_of_light_m_s",
            "must be exactly 299792458 m s^-1",
        );
    }
    positive(
        "radar.reference_water_dielectric_factor_squared",
        raw.reference_water_dielectric_factor_squared,
    )?;
    exact_text(
        "radar.length_unit_passed_to_pytmatrix",
        &raw.length_unit_passed_to_pytmatrix,
        "millimeter",
    )?;
    if raw.backscatter_geometry_deg != BACKSCATTER_GEOMETRY_DEG {
        return invalid(
            "radar.backscatter_geometry_deg",
            "must exactly equal PyTMatrix geom_horiz_back",
        );
    }
    if raw.forward_scatter_geometry_deg != FORWARD_GEOMETRY_DEG {
        return invalid(
            "radar.forward_scatter_geometry_deg",
            "must exactly equal PyTMatrix geom_horiz_forw",
        );
    }
    exact_text(
        "radar.covariance_phase_convention",
        &raw.covariance_phase_convention,
        "pytmatrix_delta_hv_hh_times_conjugate_vv",
    )?;
    exact_text(
        "radar.beam_elevation_transform",
        &raw.beam_elevation_transform,
        "pytmatrix_theta0_90_minus_e_theta_back_90_plus_e_theta_forward_90_minus_e_degrees",
    )?;
    exact_text(
        "radar.polarization_basis",
        &raw.polarization_basis,
        "pytmatrix_local_horizontal_vertical_scattering_basis",
    )?;
    let elevations = lut
        .header()
        .axes()
        .iter()
        .find(|axis| axis.kind() == AxisKind::RadarElevation)
        .expect("axis contract requires radar elevation")
        .coordinates();
    let view_applicability = if elevations == [0.0] {
        exact_text(
            "radar.view_applicability",
            &raw.view_applicability,
            "horizontal_singleton_zero_degree_axis",
        )?;
        RadarViewApplicability::HorizontalSingletonZeroDegreeAxis
    } else {
        if elevations[0] != -0.5 || elevations[elevations.len() - 1] != 20.0 {
            return invalid(
                "axes.radar_elevation",
                "view-aware tables must exactly span -0.5 through 20 degrees",
            );
        }
        exact_text(
            "radar.view_applicability",
            &raw.view_applicability,
            "ppi_beam_elevation_minus0p5_to_20_axisymmetric_gaussian_odf_not_general_body_frame",
        )?;
        RadarViewApplicability::PpiElevationAxisMinus05To20AxisymmetricGaussian
    };
    exact_text("radar.solver.shape", &raw.solver.shape, "spheroid")?;
    verify_solver_ddelt(raw.solver.ddelt)?;
    verify_solver_ndgs(population_role, raw.solver.ndgs)?;
    Ok(RadarConventionDescriptor {
        convention: RadarHvConvention::PytMatrixHorizontalHhConjugateVv,
        view_applicability,
        reference_water_dielectric_factor_squared: raw.reference_water_dielectric_factor_squared,
        solver_ddelt: raw.solver.ddelt,
        solver_ndgs: raw.solver.ndgs,
    })
}

fn verify_solver_ndgs(
    population_role: TMatrixPopulationRole,
    ndgs: u32,
) -> Result<(), TMatrixLoadError> {
    let expected = match population_role {
        TMatrixPopulationRole::OrdinaryConventional => 2,
        TMatrixPopulationRole::ConventionalRainStandaloneAndResidual
        | TMatrixPopulationRole::PropertyAwareDryCharacteristicParticle
        | TMatrixPopulationRole::PropertyAwareWetCharacteristicParticle => 14,
    };
    if ndgs == expected {
        Ok(())
    } else {
        invalid(
            "radar.solver.ndgs",
            format!("population role {population_role:?} requires exactly {expected}, got {ndgs}"),
        )
    }
}

fn verify_solver_ddelt(ddelt: f64) -> Result<(), TMatrixLoadError> {
    if ddelt == 0.001 {
        Ok(())
    } else {
        invalid(
            "radar.solver.ddelt",
            format!("all accepted table contracts require exactly 0.001, got {ddelt}"),
        )
    }
}

fn bind_terminal_speed(
    raw: RawTerminalSpeed,
    lut: &OfflineLut,
) -> Result<TerminalSpeedPolicy, TMatrixLoadError> {
    match raw {
        RawTerminalSpeed::AtlasRain1973Exponential {
            a_m_s,
            b_m_s,
            c_per_mm,
            valid_diameter_range_m,
        } => {
            positive("terminal_velocity.a_m_s", a_m_s)?;
            positive("terminal_velocity.b_m_s", b_m_s)?;
            positive("terminal_velocity.c_per_mm", c_per_mm)?;
            positive(
                "terminal_velocity.valid_diameter_range_m[0]",
                valid_diameter_range_m[0],
            )?;
            positive(
                "terminal_velocity.valid_diameter_range_m[1]",
                valid_diameter_range_m[1],
            )?;
            let diameter = lut
                .header()
                .axes()
                .iter()
                .find(|axis| axis.kind() == AxisKind::EquivolumeDiameter)
                .expect("axis contract requires diameter");
            if valid_diameter_range_m[0] >= valid_diameter_range_m[1]
                || diameter.coordinates()[0] < valid_diameter_range_m[0]
                || diameter.coordinates()[diameter.coordinates().len() - 1]
                    > valid_diameter_range_m[1]
            {
                return invalid(
                    "terminal_velocity.valid_diameter_range_m",
                    "must contain the complete diameter axis",
                );
            }
            Ok(TerminalSpeedPolicy::AtlasRain1973Exponential {
                a_m_s,
                b_m_s,
                c_per_mm,
                valid_diameter_range_m,
            })
        }
        RawTerminalSpeed::SchillerNaumannGravityDrag {
            gravity_m_s2,
            air_density_kg_m3,
            air_dynamic_viscosity_pa_s,
            drag_transition_reynolds,
            high_reynolds_drag_coefficient,
            drag_transition_boundary_policy,
            maximum_iterations,
            relative_tolerance,
        } => {
            for (field, value) in [
                ("terminal_velocity.gravity_m_s2", gravity_m_s2),
                ("terminal_velocity.air_density_kg_m3", air_density_kg_m3),
                (
                    "terminal_velocity.air_dynamic_viscosity_pa_s",
                    air_dynamic_viscosity_pa_s,
                ),
                (
                    "terminal_velocity.drag_transition_reynolds",
                    drag_transition_reynolds,
                ),
                (
                    "terminal_velocity.high_reynolds_drag_coefficient",
                    high_reynolds_drag_coefficient,
                ),
                ("terminal_velocity.relative_tolerance", relative_tolerance),
            ] {
                positive(field, value)?;
            }
            if maximum_iterations == 0 {
                return invalid("terminal_velocity.maximum_iterations", "must be nonzero");
            }
            exact_text(
                "terminal_velocity.drag_transition_boundary_policy",
                &drag_transition_boundary_policy,
                "select_exact_transition_reynolds_boundary_when_piecewise_drag_residual_jump_straddles_zero",
            )?;
            Ok(TerminalSpeedPolicy::SchillerNaumannGravityDrag {
                gravity_m_s2,
                air_density_kg_m3,
                air_dynamic_viscosity_pa_s,
                drag_transition_reynolds,
                high_reynolds_drag_coefficient,
                drag_transition_boundary_policy: DragTransitionBoundaryPolicy::SelectExactTransitionReynoldsBoundaryWhenPiecewiseDragResidualJumpStraddlesZero,
                maximum_iterations,
                relative_tolerance,
            })
        }
    }
}

fn verify_category_terminal(
    category: TMatrixParticleCategory,
    terminal: &TerminalSpeedPolicy,
) -> Result<(), TMatrixLoadError> {
    let compatible = matches!(
        (category, terminal),
        (
            TMatrixParticleCategory::Conventional(ConventionalHydrometeor::Rain),
            TerminalSpeedPolicy::AtlasRain1973Exponential { .. }
        ) | (
            TMatrixParticleCategory::Conventional(ConventionalHydrometeor::Hail),
            TerminalSpeedPolicy::SchillerNaumannGravityDrag { .. }
        ) | (
            TMatrixParticleCategory::PropertyAwareFrozenCharacteristicParticle,
            TerminalSpeedPolicy::SchillerNaumannGravityDrag { .. }
        )
    );
    if compatible {
        Ok(())
    } else {
        invalid(
            "terminal_velocity.law",
            "terminal-speed policy is incompatible with the declared category",
        )
    }
}

fn verify_execution(
    raw: RawExecution,
    material: &TMatrixMaterial,
) -> Result<TMatrixExecutionDescriptor, TMatrixLoadError> {
    if raw.point_timeout_seconds == 0 {
        return invalid("execution.point_timeout_seconds", "must be nonzero");
    }
    exact_text(
        "execution.result_collection_order",
        &raw.result_collection_order,
        "declared_axis_order_last_axis_fastest",
    )?;
    exact_text(
        "execution.partial_grid_policy",
        &raw.partial_grid_policy,
        "reject_entire_lut",
    )?;
    if raw.thread_count_per_process != 1 {
        return invalid("execution.thread_count_per_process", "must be exactly one");
    }
    match raw.process_isolation.as_str() {
        "fresh_python_subprocess_per_grid_point" => {
            if raw.grouping.is_some() {
                return invalid(
                    "execution.grouping",
                    "is forbidden for per-grid-point isolation",
                );
            }
            Ok(TMatrixExecutionDescriptor::FreshProcessPerGridPoint)
        }
        "fresh_python_subprocess_per_material_state_group" => {
            if raw.point_timeout_seconds != 300 {
                return invalid(
                    "execution.point_timeout_seconds",
                    "grouped property generation requires exactly 300 seconds",
                );
            }
            let grouping = raw
                .grouping
                .ok_or_else(|| TMatrixLoadError::InvalidConfig {
                    field: "execution.grouping",
                    detail: "is required for grouped material-state execution".to_owned(),
                })?;
            exact_text(
                "execution.grouping.model",
                &grouping.model,
                "fresh_crash_isolated_material_state_process",
            )?;
            exact_text(
                "execution.grouping.partial_group_policy",
                &grouping.partial_group_policy,
                "reject_entire_lut",
            )?;
            if !matches!(grouping.maximum_points_per_process, 2048 | 4096) {
                return invalid(
                    "execution.grouping.maximum_points_per_process",
                    "must be exactly 2048 or 4096",
                );
            }
            if grouping.group_timeout_seconds != 3600 {
                return invalid(
                    "execution.grouping.group_timeout_seconds",
                    "must be exactly 3600",
                );
            }
            let expected_material_axes: &[AxisKind] = match material {
                TMatrixMaterial::SymmetricBruggemanSphericalAirIceMatzler2006V1 { .. } => &[
                    AxisKind::Temperature,
                    AxisKind::BulkDensity,
                    AxisKind::Frequency,
                ],
                TMatrixMaterial::SymmetricBruggemanSphericalAirIceWaterV1 { .. } => &[
                    AxisKind::Temperature,
                    AxisKind::CondensedVolumeFraction,
                    AxisKind::LiquidMassFraction,
                    AxisKind::Frequency,
                ],
                TMatrixMaterial::TemperatureDependentLiquidWaterLiebe1991 { .. } => {
                    &[AxisKind::Temperature, AxisKind::Frequency]
                }
                _ => {
                    return invalid(
                        "execution.grouping",
                        "grouped execution is unsupported for this dielectric material",
                    );
                }
            };
            if grouping.material_state_axis_kinds != expected_material_axes {
                return invalid(
                    "execution.grouping.material_state_axis_kinds",
                    format!(
                        "expected {expected_material_axes:?}, got {:?}",
                        grouping.material_state_axis_kinds
                    ),
                );
            }
            let expected_tmatrix_axes = [
                AxisKind::EquivolumeDiameter,
                AxisKind::MinorToMajorAxisRatio,
            ];
            if grouping.tmatrix_state_axis_kinds != expected_tmatrix_axes {
                return invalid(
                    "execution.grouping.tmatrix_state_axis_kinds",
                    format!(
                        "expected {expected_tmatrix_axes:?}, got {:?}",
                        grouping.tmatrix_state_axis_kinds
                    ),
                );
            }
            if grouping.geometry_axis_kind != AxisKind::RadarElevation {
                return invalid(
                    "execution.grouping.geometry_axis_kind",
                    "must be radar_elevation",
                );
            }
            Ok(
                TMatrixExecutionDescriptor::FreshProcessPerMaterialStateGroup {
                    material_state_axes: grouping.material_state_axis_kinds,
                    tmatrix_state_axes: grouping.tmatrix_state_axis_kinds,
                    geometry_axis: grouping.geometry_axis_kind,
                    maximum_points_per_process: grouping.maximum_points_per_process,
                    group_timeout_seconds: grouping.group_timeout_seconds,
                },
            )
        }
        other => invalid(
            "execution.process_isolation",
            format!("unsupported isolation model {other:?}"),
        ),
    }
}

fn verify_header_science(
    lut: &OfflineLut,
    material: &TMatrixMaterial,
    odf: &TMatrixOdfConvention,
) -> Result<(), TMatrixLoadError> {
    let science = lut.header().science();
    if science.kernel()
        != &(KernelModel::TMatrix {
            implementation: TMatrixImplementation::PyTMatrix033,
        })
    {
        return Err(TMatrixLoadError::ScienceMismatch { field: "kernel" });
    }
    if science.validation() != &TableValidation::ResearchOnlyUnvalidated {
        return Err(TMatrixLoadError::ScienceMismatch {
            field: "validation status",
        });
    }
    if science.orientation() != &odf.orientation_model() {
        return Err(TMatrixLoadError::ScienceMismatch {
            field: "orientation",
        });
    }
    let expected_melting = match material {
        TMatrixMaterial::Homogeneous { .. }
        | TMatrixMaterial::TemperatureDependentLiquidWaterLiebe1991 { .. } => MeltingModel::Dry,
        TMatrixMaterial::MaxwellGarnettIceHostWaterInclusion { .. } => {
            MeltingModel::HomogeneousEffectiveMedium {
                rule: EffectiveMediumRule::MaxwellGarnett,
            }
        }
        TMatrixMaterial::SymmetricBruggemanSphericalAirIceWaterV1 { .. } => {
            MeltingModel::HomogeneousEffectiveMedium {
                rule: EffectiveMediumRule::Bruggeman,
            }
        }
        TMatrixMaterial::SymmetricBruggemanSphericalAirIceMatzler2006V1 { .. } => MeltingModel::Dry,
    };
    if science.melting() != &expected_melting {
        return Err(TMatrixLoadError::ScienceMismatch { field: "material" });
    }
    if science.temporal() != &TemporalSampling::Instantaneous {
        return Err(TMatrixLoadError::ScienceMismatch { field: "temporal" });
    }
    Ok(())
}

fn condensed_volume_fraction(
    bulk_density_kg_m3: f64,
    liquid_mass_fraction: f64,
) -> Result<f64, EvaluationError> {
    let value =
        bulk_density_kg_m3 * ((1.0 - liquid_mass_fraction) / 917.0 + liquid_mass_fraction / 999.84);
    if value.is_finite() && 0.0 < value && value <= 1.0 {
        Ok(value)
    } else {
        Err(EvaluationError::InvalidCondensedVolumeFraction {
            bulk_density_kg_m3,
            liquid_mass_fraction,
            value,
        })
    }
}

fn replace_fall_moments(
    additive: AdditiveScattering,
    positive_down_speed_m_s: f64,
) -> Result<AdditiveScattering, EvaluationError> {
    if !positive_down_speed_m_s.is_finite() || positive_down_speed_m_s <= 0.0 {
        return Err(EvaluationError::InvalidClosureFallSpeed {
            value: positive_down_speed_m_s,
        });
    }
    let mut components = additive.components();
    let zh = components[0];
    components[7] = zh * positive_down_speed_m_s;
    components[8] = zh * positive_down_speed_m_s * positive_down_speed_m_s;
    AdditiveScattering::from_components(components).map_err(EvaluationError::Output)
}

fn exact_text(field: &'static str, actual: &str, expected: &str) -> Result<(), TMatrixLoadError> {
    if actual == expected {
        Ok(())
    } else {
        invalid(field, format!("expected {expected:?}, got {actual:?}"))
    }
}

fn exact_u16(field: &'static str, actual: u16, expected: u16) -> Result<(), TMatrixLoadError> {
    if actual == expected {
        Ok(())
    } else {
        invalid(field, format!("expected {expected}, got {actual}"))
    }
}

fn nonempty(field: &'static str, value: &str) -> Result<(), TMatrixLoadError> {
    if value.trim().is_empty() {
        invalid(field, "must not be empty")
    } else {
        Ok(())
    }
}

fn positive(field: &'static str, value: f64) -> Result<(), TMatrixLoadError> {
    if value.is_finite() && value > 0.0 {
        Ok(())
    } else {
        invalid(field, format!("must be finite and positive, got {value}"))
    }
}

fn invalid<T>(field: &'static str, detail: impl Into<String>) -> Result<T, TMatrixLoadError> {
    Err(TMatrixLoadError::InvalidConfig {
        field,
        detail: detail.into(),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;
    use crate::{
        Axis, ClosureContext, ConventionalCategoryInput, DiagnosticCoexistenceInput,
        GeneratorMetadata, IshmaelIceCategory, IshmaelPsd, IshmaelPsdInput, OrientationDefinition,
        P3CategoryInput, PsdFallSpeedAuthority, PsdIntegrationConfig, PsdParticleSupport,
        ScienceMetadata, close_conventional_category, close_p3_category, integrate_ishmael_psd,
    };

    const FREQUENCY_HZ: f64 = 2_700_832_954.954_955;

    fn fixture_config(orientation_sigma_deg: f64) -> String {
        serde_json::to_string(&json!({
            "schema": 1,
            "status": "research_only_unvalidated",
            "kernel": "pytmatrix-0.3.3",
            "table_id": "software-test-conventional-dry-ice",
            "particle_population": {
                "microphysics_family": "conventional",
                "category": "hail",
                "shape_family": "oblate_spheroid",
                "size_distribution": "monodisperse_node",
                "normalization_number_concentration_m3": 1.0
            },
            "axes": [
                {"kind":"equivolume_diameter","unit":"meter","coordinates":[0.005,0.01]},
                {"kind":"minor_to_major_axis_ratio","unit":"unitless_fraction","coordinates":[0.8,1.0]},
                {"kind":"frequency","unit":"hertz","coordinates":[FREQUENCY_HZ]},
                {"kind":"radar_elevation","unit":"degree","coordinates":[0.0]}
            ],
            "dielectric": {
                "model":"explicit_homogeneous",
                "material":"ice",
                "refractive_index":{"real":1.7861,"imaginary":0.0000966},
                "mass_density_kg_m3":916.7,
                "temperature_k":273.15,
                "frequency_dependence":"constant_over_configured_s_band_nodes"
            },
            "orientation": {
                "model":"gaussian_canting",
                "mean_deg":0.0,
                "standard_deviation_deg":orientation_sigma_deg,
                "alpha_quadrature_points":5,
                "beta_quadrature_points":10,
                "quadrature_method":"pytmatrix_orient_averaged_fixed_gautschi",
                "reference_symmetry_axis":"vertical_at_zero_canting"
            },
            "radar": {
                "speed_of_light_m_s":299792458.0,
                "reference_water_dielectric_factor_squared":0.93,
                "length_unit_passed_to_pytmatrix":"millimeter",
                "backscatter_geometry_deg":[90.0,90.0,0.0,180.0,0.0,0.0],
                "forward_scatter_geometry_deg":[90.0,90.0,0.0,0.0,0.0,0.0],
                "covariance_phase_convention":"pytmatrix_delta_hv_hh_times_conjugate_vv",
                "beam_elevation_transform":"pytmatrix_theta0_90_minus_e_theta_back_90_plus_e_theta_forward_90_minus_e_degrees",
                "polarization_basis":"pytmatrix_local_horizontal_vertical_scattering_basis",
                "view_applicability":"horizontal_singleton_zero_degree_axis",
                "solver":{"shape":"spheroid","ddelt":0.001,"ndgs":2}
            },
            "terminal_velocity": {
                "law":"schiller_naumann_gravity_drag",
                "gravity_m_s2":9.80665,
                "air_density_kg_m3":1.225,
                "air_dynamic_viscosity_pa_s":0.000017894,
                "drag_transition_reynolds":1000.0,
                "high_reynolds_drag_coefficient":0.44,
                "drag_transition_boundary_policy":"select_exact_transition_reynolds_boundary_when_piecewise_drag_residual_jump_straddles_zero",
                "maximum_iterations":200,
                "relative_tolerance":1e-12
            },
            "temporal":{"sampling":"instantaneous"},
            "execution": {
                "point_timeout_seconds":120,
                "process_isolation":"fresh_python_subprocess_per_grid_point",
                "result_collection_order":"declared_axis_order_last_axis_fastest",
                "partial_grid_policy":"reject_entire_lut",
                "thread_count_per_process":1
            },
            "payload":{"encoding":"f64_le_point_major_last_axis_fastest"},
            "references":["software-test-only"]
        }))
        .unwrap()
    }

    fn fixture_with_config(config: String) -> (Vec<u8>, String) {
        let axes = vec![
            Axis::new(AxisKind::EquivolumeDiameter, Unit::Meter, vec![0.005, 0.01]).unwrap(),
            Axis::new(
                AxisKind::MinorToMajorAxisRatio,
                Unit::UnitlessFraction,
                vec![0.8, 1.0],
            )
            .unwrap(),
            Axis::new(AxisKind::Frequency, Unit::Hertz, vec![FREQUENCY_HZ]).unwrap(),
            Axis::new(AxisKind::RadarElevation, Unit::Degree, vec![0.0]).unwrap(),
        ];
        let mut packages = BTreeMap::new();
        packages.insert("pytmatrix".to_owned(), "0.3.3".to_owned());
        let generator = GeneratorMetadata::new(
            "software-test-generator",
            "1",
            "unit-test",
            "software-test-only",
            Some("3.11".to_owned()),
            packages,
        )
        .unwrap();
        let science = ScienceMetadata::new(
            KernelModel::TMatrix {
                implementation: TMatrixImplementation::PyTMatrix033,
            },
            OrientationModel::GaussianCanting {
                mean_deg: 0.0,
                standard_deviation_deg: 20.0,
                quadrature_points: 50,
            },
            MeltingModel::Dry,
            TemporalSampling::Instantaneous,
            TableValidation::ResearchOnlyUnvalidated,
        )
        .unwrap();
        let node =
            AdditiveScattering::from_components([2.0, 1.0, 0.5, 0.0, 0.1, 0.01, 0.01, 4.0, 8.0])
                .unwrap();
        let table =
            OfflineLut::new(axes, generator, config.clone(), science, vec![node; 4]).unwrap();
        (table.to_bytes().unwrap(), config)
    }

    fn test_generator() -> GeneratorMetadata {
        let mut packages = BTreeMap::new();
        packages.insert("pytmatrix".to_owned(), "0.3.3".to_owned());
        GeneratorMetadata::new(
            "software-test-generator",
            "1",
            "unit-test",
            "software-test-only",
            Some("3.11".to_owned()),
            packages,
        )
        .unwrap()
    }

    fn constant_runtime(
        axes: Vec<Axis>,
        science: ScienceMetadata,
        descriptor: TMatrixTableDescriptor,
    ) -> ResearchTMatrixLut {
        let count = axes
            .iter()
            .map(|axis| axis.coordinates().len())
            .product::<usize>();
        let node =
            AdditiveScattering::from_components([2.0, 1.0, 0.5, 0.0, 0.1, 0.01, 0.01, 4.0, 8.0])
                .unwrap();
        ResearchTMatrixLut {
            lut: OfflineLut::new(
                axes,
                test_generator(),
                r#"{"software_test_only":true}"#,
                science,
                vec![node; count],
            )
            .unwrap(),
            descriptor,
            file_sha256: Sha256Digest::compute(b"software-test-only"),
        }
    }

    fn gaussian20_odf() -> TMatrixOdfConvention {
        TMatrixOdfConvention::GaussianCanting {
            mean_deg: 0.0,
            standard_deviation_deg: 20.0,
            alpha_quadrature_points: 5,
            beta_quadrature_points: 10,
        }
    }

    fn test_radar_descriptor() -> RadarConventionDescriptor {
        RadarConventionDescriptor {
            convention: RadarHvConvention::PytMatrixHorizontalHhConjugateVv,
            view_applicability:
                RadarViewApplicability::PpiElevationAxisMinus05To20AxisymmetricGaussian,
            reference_water_dielectric_factor_squared: 0.93,
            solver_ddelt: 0.001,
            solver_ndgs: 14,
        }
    }

    fn test_drag_policy() -> TerminalSpeedPolicy {
        TerminalSpeedPolicy::SchillerNaumannGravityDrag {
            gravity_m_s2: 9.80665,
            air_density_kg_m3: 1.225,
            air_dynamic_viscosity_pa_s: 1.7894e-5,
            drag_transition_reynolds: 1_000.0,
            high_reynolds_drag_coefficient: 0.44,
            drag_transition_boundary_policy: DragTransitionBoundaryPolicy::SelectExactTransitionReynoldsBoundaryWhenPiecewiseDragResidualJumpStraddlesZero,
            maximum_iterations: 200,
            relative_tolerance: 1.0e-12,
        }
    }

    fn fixture() -> (ResearchTMatrixLut, Vec<u8>, String) {
        let (bytes, config) = fixture_with_config(fixture_config(20.0));
        let table =
            ResearchTMatrixLut::load(&bytes, Sha256Digest::compute(&bytes), config.as_bytes())
                .unwrap();
        (table, bytes, config)
    }

    fn closed_hail(diameter_m: f64, orientation: OrientationDefinition) -> ClosedParticleCategory {
        let context = ClosureContext::new(6, 273.15, 1.5)
            .unwrap()
            .with_orientation(orientation);
        let input =
            ConventionalCategoryInput::new(ConventionalHydrometeor::Hail, 1.0e-4, Some(2.0))
                .with_characteristic_diameter_m(diameter_m)
                .with_bulk_density_kg_m3(916.7)
                .with_minor_to_major_axis_ratio(0.9)
                .with_fall_speed_m_s(5.0);
        close_conventional_category(&context, &input).unwrap()
    }

    fn request(frequency_hz: f64, elevation_deg: f64) -> TMatrixEvaluationRequest {
        TMatrixEvaluationRequest::new(
            frequency_hz,
            SpheroidConvention::OblateMinorVertical,
            RadarViewGeometry::new(elevation_deg).unwrap(),
        )
        .unwrap()
    }

    fn synthetic_speed_provenance() -> PsdFallSpeedProvenance {
        PsdFallSpeedProvenance::new(
            PsdFallSpeedAuthority::SyntheticTestOnly,
            Sha256Digest::compute(b"tmatrix-particle-node-test-speed-v1"),
        )
    }

    fn dry_property_runtime(spheroid: SpheroidConvention) -> ResearchTMatrixLut {
        let axes = vec![
            Axis::new(
                AxisKind::EquivolumeDiameter,
                Unit::Meter,
                vec![1.0e-6, 0.01],
            )
            .unwrap(),
            Axis::new(AxisKind::Temperature, Unit::Kelvin, vec![200.0, 300.0]).unwrap(),
            Axis::new(
                AxisKind::BulkDensity,
                Unit::KilogramPerCubicMeter,
                vec![50.0, 917.0],
            )
            .unwrap(),
            Axis::new(
                AxisKind::MinorToMajorAxisRatio,
                Unit::UnitlessFraction,
                vec![0.1, 1.0],
            )
            .unwrap(),
            Axis::new(AxisKind::Frequency, Unit::Hertz, vec![FREQUENCY_HZ]).unwrap(),
            Axis::new(AxisKind::RadarElevation, Unit::Degree, vec![-0.5, 20.0]).unwrap(),
        ];
        let science = ScienceMetadata::new(
            KernelModel::TMatrix {
                implementation: TMatrixImplementation::PyTMatrix033,
            },
            gaussian20_odf().orientation_model(),
            MeltingModel::Dry,
            TemporalSampling::Instantaneous,
            TableValidation::ResearchOnlyUnvalidated,
        )
        .unwrap();
        let descriptor = TMatrixTableDescriptor {
            table_id: "software-test-dry-property-node".to_owned(),
            category: TMatrixParticleCategory::PropertyAwareFrozenCharacteristicParticle,
            population_role: TMatrixPopulationRole::PropertyAwareDryCharacteristicParticle,
            density_applicability: DensityApplicability::DryBulkDensity15To917KgM3Above1225Air,
            spheroid,
            material: TMatrixMaterial::SymmetricBruggemanSphericalAirIceMatzler2006V1 {
                air_relative_permittivity: ComplexRefractiveIndex {
                    real: 1.0,
                    imaginary: 0.0,
                },
                ice_material_density_kg_m3: 917.0,
                homotopy_steps: 64,
                newton_max_iterations: 100,
                newton_relative_tolerance: 1.0e-12,
                temperature_range_k: [200.0, 300.0],
            },
            odf: gaussian20_odf(),
            radar: test_radar_descriptor(),
            terminal_speed: test_drag_policy(),
            terminal_speed_sha256: terminal_speed_policy_sha256(&test_drag_policy()),
            execution: TMatrixExecutionDescriptor::FreshProcessPerGridPoint,
            normalization_number_concentration_m3: 1.0,
        };
        constant_runtime(axes, science, descriptor)
    }

    #[test]
    fn loader_binds_complete_digest_config_and_descriptor() {
        let (table, _, _) = fixture();
        assert_eq!(
            table.descriptor().category(),
            TMatrixParticleCategory::Conventional(ConventionalHydrometeor::Hail)
        );
        assert_eq!(
            table.descriptor().odf(),
            &TMatrixOdfConvention::GaussianCanting {
                mean_deg: 0.0,
                standard_deviation_deg: 20.0,
                alpha_quadrature_points: 5,
                beta_quadrature_points: 10,
            }
        );
        assert_eq!(
            table.descriptor().radar().view_applicability,
            RadarViewApplicability::HorizontalSingletonZeroDegreeAxis
        );
        assert_eq!(table.descriptor().terminal_speed(), &test_drag_policy());
    }

    #[test]
    fn loader_requires_exact_drag_transition_boundary_policy() {
        let mut missing: serde_json::Value = serde_json::from_str(&fixture_config(20.0)).unwrap();
        missing["terminal_velocity"]
            .as_object_mut()
            .unwrap()
            .remove("drag_transition_boundary_policy");
        let (bytes, config) = fixture_with_config(serde_json::to_string(&missing).unwrap());
        assert!(matches!(
            ResearchTMatrixLut::load(&bytes, Sha256Digest::compute(&bytes), config.as_bytes()),
            Err(TMatrixLoadError::GeneratorConfigJson(_))
        ));

        let mut unsupported: serde_json::Value =
            serde_json::from_str(&fixture_config(20.0)).unwrap();
        unsupported["terminal_velocity"]["drag_transition_boundary_policy"] =
            json!("interpolate_across_drag_jump");
        let (bytes, config) = fixture_with_config(serde_json::to_string(&unsupported).unwrap());
        assert!(matches!(
            ResearchTMatrixLut::load(&bytes, Sha256Digest::compute(&bytes), config.as_bytes()),
            Err(TMatrixLoadError::InvalidConfig {
                field: "terminal_velocity.drag_transition_boundary_policy",
                ..
            })
        ));
    }

    #[test]
    fn loader_rejects_whole_file_or_external_config_mismatch() {
        let (_, bytes, config) = fixture();
        assert!(matches!(
            ResearchTMatrixLut::load(&bytes, Sha256Digest::compute(b"wrong"), config.as_bytes()),
            Err(TMatrixLoadError::FileDigestMismatch { .. })
        ));
        let changed = format!("{config} ");
        assert!(matches!(
            ResearchTMatrixLut::load(&bytes, Sha256Digest::compute(&bytes), changed.as_bytes()),
            Err(TMatrixLoadError::OfflineLut(
                LutError::ExternalConfigDigestMismatch { .. }
            ))
        ));
    }

    #[test]
    fn loader_requires_config_odf_to_match_header_exactly() {
        let (bytes, config) = fixture_with_config(fixture_config(21.0));
        assert!(matches!(
            ResearchTMatrixLut::load(&bytes, Sha256Digest::compute(&bytes), config.as_bytes()),
            Err(TMatrixLoadError::ScienceMismatch {
                field: "orientation"
            })
        ));
    }

    #[test]
    fn evaluator_scales_every_additive_component_by_number_density() {
        let (table, _, _) = fixture();
        let closed = closed_hail(0.007, OrientationDefinition::Gaussian20Research);
        let output = table.evaluate(&closed, request(FREQUENCY_HZ, 0.0)).unwrap();
        // N_m-3 = 2 kg-1 * 1.5 kg m-3 = 3 m-3.
        for (actual, expected) in output
            .components()
            .into_iter()
            .zip([6.0, 3.0, 1.5, 0.0, 0.3, 0.03, 0.03, 30.0, 150.0])
        {
            assert!((actual - expected).abs() <= 1.0e-14);
        }
    }

    #[test]
    fn dry_particle_node_query_returns_exactly_one_particle_per_m3() {
        let table = dry_property_runtime(SpheroidConvention::OblateMinorVertical);
        let domain = table.dry_particle_node_domain().unwrap();
        assert_eq!(domain.equivolume_diameter_range_m(), [1.0e-6, 0.01]);
        assert_eq!(domain.bulk_density_range_kg_m3(), [50.0, 917.0]);
        assert_eq!(domain.minor_to_major_axis_ratio_range(), [0.1, 1.0]);
        let exact_speed =
            schiller_naumann_terminal_speed_m_s(table.descriptor().terminal_speed(), 1.0e-3, 400.0)
                .unwrap();
        let query = TMatrixParticleNodeQuery::new(
            260.0,
            1.0e-3,
            400.0,
            0.8,
            PsdSpheroidHabit::Oblate,
            None,
            None,
            exact_speed,
            table.dry_particle_node_fall_speed_provenance().unwrap(),
            gaussian20_odf().orientation_model(),
            request(FREQUENCY_HZ, 1.0),
        )
        .unwrap();
        let output = table.evaluate_dry_particle_node_per_m3(&query).unwrap();
        let components = output.components();
        assert_eq!(&components[..7], &[2.0, 1.0, 0.5, 0.0, 0.1, 0.01, 0.01]);
        assert_eq!(components[7], 2.0 * exact_speed);
        assert_eq!(components[8], 2.0 * exact_speed * exact_speed);
    }

    #[test]
    fn ishmael_psd_integrates_through_typed_particle_node_query() {
        let table = dry_property_runtime(SpheroidConvention::OblateMinorVertical);
        let domain = table.dry_particle_node_domain().unwrap();
        let support = PsdParticleSupport::new(Some(domain), None, Some(domain));
        let distribution = IshmaelPsd::reconstruct(IshmaelPsdInput::new(
            IshmaelIceCategory::Planar,
            1.020_730_388_452_175_2e-3,
            100_000.0,
            6.250_000_000_000_000_5e-9,
            3.125_000_000_000_000_3e-9,
            1.2,
        ))
        .unwrap();
        let speed_provenance = table.dry_particle_node_fall_speed_provenance().unwrap();
        let result = integrate_ishmael_psd(
            &distribution,
            PsdIntegrationConfig::default(),
            support,
            speed_provenance,
            |node| {
                let positive_down_speed = table.dry_particle_node_terminal_speed_m_s(node)?;
                let query = TMatrixParticleNodeQuery::from_psd_node(
                    node,
                    260.0,
                    positive_down_speed,
                    speed_provenance,
                    gaussian20_odf().orientation_model(),
                    request(FREQUENCY_HZ, 1.0),
                )?;
                table
                    .evaluate_dry_particle_node_per_m3(&query)?
                    .checked_scale(1.0e-4)
                    .map_err(EvaluationError::Output)
            },
        )
        .unwrap();
        assert!(result.additive().zh().get() > 0.0);
        assert!(result.accumulator().fall_speed_variance_m2s2 > 0.0);
        assert_eq!(result.audit().fall_speed, speed_provenance);
        assert_eq!(
            speed_provenance.authority(),
            PsdFallSpeedAuthority::TMatrixTableTerminalPolicyV1
        );
        assert!(result.audit().domain_omitted_number_fraction <= 1.0e-6);
    }

    #[test]
    fn particle_node_query_rejects_habit_frequency_and_wrong_table_role() {
        let table = dry_property_runtime(SpheroidConvention::OblateMinorVertical);
        let exact_speed =
            schiller_naumann_terminal_speed_m_s(table.descriptor().terminal_speed(), 1.0e-3, 400.0)
                .unwrap();
        assert!(matches!(
            TMatrixParticleNodeQuery::new(
                260.0,
                1.0e-3,
                400.0,
                1.0,
                PsdSpheroidHabit::Oblate,
                None,
                None,
                3.0,
                synthetic_speed_provenance(),
                gaussian20_odf().orientation_model(),
                request(FREQUENCY_HZ, 1.0),
            ),
            Err(EvaluationError::ParticleNodeHabitGeometryMismatch { .. })
        ));
        let prolate = TMatrixParticleNodeQuery::new(
            260.0,
            1.0e-3,
            400.0,
            0.8,
            PsdSpheroidHabit::Prolate,
            None,
            None,
            3.0,
            synthetic_speed_provenance(),
            gaussian20_odf().orientation_model(),
            request(FREQUENCY_HZ, 1.0),
        )
        .unwrap();
        assert!(matches!(
            table.evaluate_dry_particle_node_per_m3(&prolate),
            Err(EvaluationError::ParticleNodeSpheroidMismatch { .. })
        ));

        let wrong_provenance = TMatrixParticleNodeQuery::new(
            260.0,
            1.0e-3,
            400.0,
            0.8,
            PsdSpheroidHabit::Oblate,
            None,
            None,
            3.0,
            synthetic_speed_provenance(),
            gaussian20_odf().orientation_model(),
            request(FREQUENCY_HZ, 1.0),
        )
        .unwrap();
        assert!(matches!(
            table.evaluate_dry_particle_node_per_m3(&wrong_provenance),
            Err(EvaluationError::ParticleNodeFallSpeedProvenanceMismatch { .. })
        ));

        let wrong_speed = TMatrixParticleNodeQuery::new(
            260.0,
            1.0e-3,
            400.0,
            0.8,
            PsdSpheroidHabit::Oblate,
            None,
            None,
            exact_speed + 1.0e-9,
            table.dry_particle_node_fall_speed_provenance().unwrap(),
            gaussian20_odf().orientation_model(),
            request(FREQUENCY_HZ, 1.0),
        )
        .unwrap();
        assert!(matches!(
            table.evaluate_dry_particle_node_per_m3(&wrong_speed),
            Err(EvaluationError::ParticleNodeFallSpeedValueMismatch { .. })
        ));

        let wrong_frequency = TMatrixParticleNodeQuery::new(
            260.0,
            1.0e-3,
            400.0,
            0.8,
            PsdSpheroidHabit::Oblate,
            None,
            None,
            exact_speed,
            table.dry_particle_node_fall_speed_provenance().unwrap(),
            gaussian20_odf().orientation_model(),
            request(FREQUENCY_HZ + 1.0, 1.0),
        )
        .unwrap();
        assert!(matches!(
            table.evaluate_dry_particle_node_per_m3(&wrong_frequency),
            Err(EvaluationError::ParticleNodeFrequencyMismatch { .. })
        ));

        let (conventional, _, _) = fixture();
        assert!(matches!(
            conventional.evaluate_dry_particle_node_per_m3(&prolate),
            Err(EvaluationError::DryParticleNodeTableRequired { .. })
        ));
    }

    #[test]
    fn evaluator_rejects_category_and_exact_odf_mismatches() {
        let (table, _, _) = fixture();
        let rain_context = ClosureContext::new(6, 273.15, 1.5).unwrap();
        let rain = close_conventional_category(
            &rain_context,
            &ConventionalCategoryInput::new(ConventionalHydrometeor::Rain, 1.0e-4, Some(2.0))
                .with_characteristic_diameter_m(0.007)
                .with_bulk_density_kg_m3(916.7)
                .with_minor_to_major_axis_ratio(0.9),
        )
        .unwrap();
        assert!(matches!(
            table.evaluate(&rain, request(FREQUENCY_HZ, 0.0)),
            Err(EvaluationError::CategoryMismatch { .. })
        ));
        let scheme_default = closed_hail(0.007, OrientationDefinition::SchemeDefault);
        assert!(matches!(
            table.evaluate(&scheme_default, request(FREQUENCY_HZ, 0.0)),
            Err(EvaluationError::OrientationMismatch { .. })
        ));
    }

    #[test]
    fn frequency_elevation_and_diameter_never_extrapolate() {
        let (table, _, _) = fixture();
        let closed = closed_hail(0.007, OrientationDefinition::Gaussian20Research);
        for query in [request(FREQUENCY_HZ + 1.0, 0.0), request(FREQUENCY_HZ, 0.1)] {
            assert!(matches!(
                table.evaluate(&closed, query),
                Err(EvaluationError::Interpolation(
                    InterpolationError::OutsideAxis { .. }
                ))
            ));
        }
        let too_large = closed_hail(0.02, OrientationDefinition::Gaussian20Research);
        assert!(matches!(
            table.evaluate(&too_large, request(FREQUENCY_HZ, 0.0)),
            Err(EvaluationError::Interpolation(
                InterpolationError::OutsideAxis {
                    kind: AxisKind::EquivolumeDiameter,
                    ..
                }
            ))
        ));
    }

    #[test]
    fn p3_state_cannot_be_relabelled_as_conventional_hail() {
        let (table, _, _) = fixture();
        let context = ClosureContext::new(50, 273.15, 1.5)
            .unwrap()
            .with_orientation(OrientationDefinition::Gaussian20Research);
        let p3 = close_p3_category(
            &context,
            &P3CategoryInput::category1(1.0e-4, 1.0e6, 4.0e-5, 1.0e-7),
        )
        .unwrap();
        assert_eq!(
            table.evaluate(&p3, request(FREQUENCY_HZ, 0.0)),
            Err(EvaluationError::FamilyMismatch {
                expected: MicrophysicsFamily::Conventional,
                actual: MicrophysicsFamily::P3,
            })
        );
    }

    #[test]
    fn wet_category_preserves_frozen_number_and_reports_consumed_rain() {
        let axes = vec![
            Axis::new(
                AxisKind::EquivolumeDiameter,
                Unit::Meter,
                vec![1.0e-5, 0.05],
            )
            .unwrap(),
            Axis::new(AxisKind::Temperature, Unit::Kelvin, vec![269.15, 275.15]).unwrap(),
            Axis::new(
                AxisKind::CondensedVolumeFraction,
                Unit::UnitlessFraction,
                vec![1.0e-4, 1.0],
            )
            .unwrap(),
            Axis::new(
                AxisKind::LiquidMassFraction,
                Unit::UnitlessFraction,
                vec![0.0, 1.0],
            )
            .unwrap(),
            Axis::new(
                AxisKind::MinorToMajorAxisRatio,
                Unit::UnitlessFraction,
                vec![0.1, 1.0],
            )
            .unwrap(),
            Axis::new(AxisKind::Frequency, Unit::Hertz, vec![FREQUENCY_HZ]).unwrap(),
            Axis::new(AxisKind::RadarElevation, Unit::Degree, vec![-0.5, 20.0]).unwrap(),
        ];
        let science = ScienceMetadata::new(
            KernelModel::TMatrix {
                implementation: TMatrixImplementation::PyTMatrix033,
            },
            gaussian20_odf().orientation_model(),
            MeltingModel::HomogeneousEffectiveMedium {
                rule: EffectiveMediumRule::Bruggeman,
            },
            TemporalSampling::Instantaneous,
            TableValidation::ResearchOnlyUnvalidated,
        )
        .unwrap();
        let descriptor = TMatrixTableDescriptor {
            table_id: "software-test-wet-property".to_owned(),
            category: TMatrixParticleCategory::PropertyAwareFrozenCharacteristicParticle,
            population_role: TMatrixPopulationRole::PropertyAwareWetCharacteristicParticle,
            density_applicability:
                DensityApplicability::WetCondensedVolumeFraction00015To1Above1225Air,
            spheroid: SpheroidConvention::OblateMinorVertical,
            material: TMatrixMaterial::SymmetricBruggemanSphericalAirIceWaterV1 {
                air_relative_permittivity: ComplexRefractiveIndex {
                    real: 1.0,
                    imaginary: 0.0,
                },
                ice_permittivity_model: "matzler_2006".to_owned(),
                liquid_water_permittivity_model: "liebe_hufford_manabe_1991_double_debye"
                    .to_owned(),
                ice_temperature_treatment:
                    "minimum_environment_temperature_and_273p15_k_phase_equilibrium".to_owned(),
                ice_material_density_kg_m3: 917.0,
                liquid_water_density_kg_m3: 999.84,
                homotopy_steps: 64,
                newton_max_iterations: 100,
                newton_relative_tolerance: 1.0e-12,
                temperature_range_k: [269.15, 275.15],
            },
            odf: gaussian20_odf(),
            radar: test_radar_descriptor(),
            terminal_speed: test_drag_policy(),
            terminal_speed_sha256: terminal_speed_policy_sha256(&test_drag_policy()),
            execution: TMatrixExecutionDescriptor::FreshProcessPerGridPoint,
            normalization_number_concentration_m3: 1.0,
        };
        let table = constant_runtime(axes, science, descriptor);

        let context = ClosureContext::new(50, 272.15, 1.5)
            .unwrap()
            .with_orientation(OrientationDefinition::Gaussian20Research);
        let frozen = close_p3_category(
            &context,
            &P3CategoryInput::category1(1.0e-4, 1.0e6, 4.0e-5, 1.0e-7),
        )
        .unwrap();
        let rain = close_conventional_category(
            &context,
            &ConventionalCategoryInput::new(ConventionalHydrometeor::Rain, 1.0e-4, Some(1.0e6))
                .with_characteristic_diameter_m(5.0e-4)
                .with_bulk_density_kg_m3(999.84)
                .with_minor_to_major_axis_ratio(0.95)
                .with_fall_speed_m_s(2.0),
        )
        .unwrap();
        let diagnosis = DiagnosticCoexistenceInput::new(272.15, rain.clone(), vec![frozen.clone()])
            .unwrap()
            .diagnose()
            .unwrap();
        let wet = &diagnosis.wet_categories()[0];
        let contribution = table
            .evaluate_wet_category(wet, request(FREQUENCY_HZ, 1.0))
            .unwrap();
        assert_eq!(
            contribution.number_scaling(),
            NumberScalingPolicy::PreserveFrozenParticleNumberForWetCategory
        );
        assert_eq!(
            contribution.fall_moments(),
            FallMomentPolicy::DiagnosticWetCategoryPositiveDownZeroWithinCategoryVariance
        );
        assert_eq!(contribution.number_density_m3(), 1.5e6);
        assert_eq!(
            contribution.consumed_paired_liquid_mass_kgkg(),
            wet.paired_liquid_mass_kgkg()
        );
        assert_eq!(
            contribution.represented_mixing_ratio_kgkg(),
            wet.wet_total_mass_kgkg()
        );
        let components = contribution.additive().components();
        let closure_speed = wet.fall_speed_m_s().value();
        assert!((components[0] - 3.0e6).abs() <= 1.0e-8);
        assert!((components[7] - components[0] * closure_speed).abs() <= 1.0e-8);
        assert!((components[8] - components[0] * closure_speed * closure_speed).abs() <= 1.0e-8);

        // A zero-LMF node is valid as a wet-table interpolation boundary, but
        // a genuinely dry category must dispatch to the dry table.
        let dry_boundary = DiagnosticCoexistenceInput::new(269.15, rain, vec![frozen])
            .unwrap()
            .diagnose()
            .unwrap();
        assert_eq!(dry_boundary.wet_categories()[0].wet_fraction(), 0.0);
        assert_eq!(
            table.evaluate_wet_category(
                &dry_boundary.wet_categories()[0],
                request(FREQUENCY_HZ, 1.0),
            ),
            Err(EvaluationError::PhaseRegimeMismatch {
                expected: "strictly wet liquid_mass_fraction>0",
                actual_liquid_mass_fraction: 0.0,
            })
        );
    }

    #[test]
    fn residual_rain_scales_number_by_unpaired_mass_fraction_exactly_once() {
        let axes = vec![
            Axis::new(
                AxisKind::EquivolumeDiameter,
                Unit::Meter,
                vec![1.0e-4, 0.01],
            )
            .unwrap(),
            Axis::new(AxisKind::Temperature, Unit::Kelvin, vec![250.0, 313.15]).unwrap(),
            Axis::new(
                AxisKind::MinorToMajorAxisRatio,
                Unit::UnitlessFraction,
                vec![0.5, 1.0],
            )
            .unwrap(),
            Axis::new(AxisKind::Frequency, Unit::Hertz, vec![FREQUENCY_HZ]).unwrap(),
            Axis::new(AxisKind::RadarElevation, Unit::Degree, vec![-0.5, 20.0]).unwrap(),
        ];
        let science = ScienceMetadata::new(
            KernelModel::TMatrix {
                implementation: TMatrixImplementation::PyTMatrix033,
            },
            gaussian20_odf().orientation_model(),
            MeltingModel::Dry,
            TemporalSampling::Instantaneous,
            TableValidation::ResearchOnlyUnvalidated,
        )
        .unwrap();
        let rain_terminal_speed = TerminalSpeedPolicy::AtlasRain1973Exponential {
            a_m_s: 9.65,
            b_m_s: 10.3,
            c_per_mm: 0.6,
            valid_diameter_range_m: [1.0e-4, 0.01],
        };
        let descriptor = TMatrixTableDescriptor {
            table_id: "software-test-residual-rain".to_owned(),
            category: TMatrixParticleCategory::Conventional(ConventionalHydrometeor::Rain),
            population_role: TMatrixPopulationRole::ConventionalRainStandaloneAndResidual,
            density_applicability: DensityApplicability::ConventionalCategory,
            spheroid: SpheroidConvention::OblateMinorVertical,
            material: TMatrixMaterial::TemperatureDependentLiquidWaterLiebe1991 {
                mass_density_kg_m3: 999.84,
                temperature_range_k: [250.0, 313.15],
                frequency_range_hz: [2.0e9, 4.0e9],
            },
            odf: gaussian20_odf(),
            radar: test_radar_descriptor(),
            terminal_speed_sha256: terminal_speed_policy_sha256(&rain_terminal_speed),
            terminal_speed: rain_terminal_speed,
            execution: TMatrixExecutionDescriptor::FreshProcessPerGridPoint,
            normalization_number_concentration_m3: 1.0,
        };
        let table = constant_runtime(axes, science, descriptor);
        let context = ClosureContext::new(6, 273.15, 1.5)
            .unwrap()
            .with_orientation(OrientationDefinition::Gaussian20Research);
        let rain = close_conventional_category(
            &context,
            &ConventionalCategoryInput::new(ConventionalHydrometeor::Rain, 1.0e-4, Some(2.0))
                .with_characteristic_diameter_m(1.0e-3)
                .with_bulk_density_kg_m3(999.84)
                .with_minor_to_major_axis_ratio(0.9)
                .with_fall_speed_m_s(2.0),
        )
        .unwrap();
        let contribution = table
            .evaluate_unused_rain(&rain, 2.5e-5, request(FREQUENCY_HZ, 1.0))
            .unwrap();
        assert_eq!(
            contribution.number_scaling(),
            NumberScalingPolicy::PreserveRainPsdShapeScaleNumberByResidualMassFraction
        );
        assert_eq!(
            contribution.fall_moments(),
            FallMomentPolicy::ClosedCategoryPositiveDownZeroWithinCategoryVariance
        );
        assert_eq!(contribution.number_density_m3(), 0.75);
        assert_eq!(contribution.represented_mixing_ratio_kgkg(), 2.5e-5);
        assert_eq!(contribution.consumed_paired_liquid_mass_kgkg(), 0.0);
        assert!((contribution.additive().zh().get() - 1.5).abs() <= 1.0e-14);
    }

    fn raw_property_descriptor(wet: bool) -> RawPropertyStateDescriptor {
        RawPropertyStateDescriptor {
            compatible_closed_state_families: vec!["p3".to_owned(), "ishmael".to_owned()],
            characteristic_diameter_mapping:
                "closure_derived_equivolume_characteristic_diameter".to_owned(),
            bulk_density_mapping: if wet {
                "closure_bulk_density_and_liquid_mass_fraction_mapped_to_condensed_volume_fraction"
                    .to_owned()
            } else {
                "closure_derived_effective_bulk_density_including_rime_mass_and_rime_density"
                    .to_owned()
            },
            condensed_volume_fraction_definition: wet.then(|| {
                "rho_bulk_times_open_parenthesis_one_minus_w_over_917_plus_w_over_999p84_close_parenthesis"
                    .to_owned()
            }),
            shape_mapping: "closure_derived_minor_to_major_axis_ratio".to_owned(),
            liquid_mapping: if wet {
                "diagnosed_or_prescribed_strictly_positive_liquid_mass_fraction".to_owned()
            } else {
                "required_exactly_zero_liquid_mass_fraction".to_owned()
            },
            phase_dispatch: if wet {
                "liquid_mass_fraction_greater_than_zero_selects_wet_table".to_owned()
            } else {
                "liquid_mass_fraction_equal_zero_selects_dry_table".to_owned()
            },
            rime_axes:
                "not_explicit_rime_influences_only_through_bulk_density_and_shape".to_owned(),
            rime_effect_on_dielectric: "none_given_bulk_density".to_owned(),
            psd_mapping: "none_monodisperse_characteristic_particle_not_scheme_native_psd"
                .to_owned(),
            extrapolation: "forbidden".to_owned(),
            density_applicability: if wet {
                "condensed_volume_fraction_0p0015_to_1_downward_fall_requires_reconstructed_density_above_1p225_kg_m3_air".to_owned()
            } else {
                "bulk_density_1p5_to_917_kg_m3_downward_fall_requires_density_above_1p225_kg_m3_air".to_owned()
            },
        }
    }

    #[test]
    fn property_density_applicability_is_exact_and_phase_typed() {
        assert_eq!(
            verify_property_state_descriptor(
                raw_property_descriptor(false),
                TMatrixPopulationRole::PropertyAwareDryCharacteristicParticle,
            )
            .unwrap(),
            DensityApplicability::DryBulkDensity15To917KgM3Above1225Air
        );
        assert_eq!(
            verify_property_state_descriptor(
                raw_property_descriptor(true),
                TMatrixPopulationRole::PropertyAwareWetCharacteristicParticle,
            )
            .unwrap(),
            DensityApplicability::WetCondensedVolumeFraction00015To1Above1225Air
        );
        let mut changed = raw_property_descriptor(true);
        changed.density_applicability.push_str("_changed");
        assert!(
            verify_property_state_descriptor(
                changed,
                TMatrixPopulationRole::PropertyAwareWetCharacteristicParticle,
            )
            .is_err()
        );
    }

    fn wet_material_for_execution() -> TMatrixMaterial {
        TMatrixMaterial::SymmetricBruggemanSphericalAirIceWaterV1 {
            air_relative_permittivity: ComplexRefractiveIndex {
                real: 1.0,
                imaginary: 0.0,
            },
            ice_permittivity_model: "matzler_2006".to_owned(),
            liquid_water_permittivity_model: "liebe_hufford_manabe_1991_double_debye".to_owned(),
            ice_temperature_treatment:
                "minimum_environment_temperature_and_273p15_k_phase_equilibrium".to_owned(),
            ice_material_density_kg_m3: 917.0,
            liquid_water_density_kg_m3: 999.84,
            homotopy_steps: 64,
            newton_max_iterations: 100,
            newton_relative_tolerance: 1.0e-12,
            temperature_range_k: [269.15, 275.15],
        }
    }

    fn grouped_wet_execution() -> RawExecution {
        RawExecution {
            point_timeout_seconds: 300,
            process_isolation: "fresh_python_subprocess_per_material_state_group".to_owned(),
            result_collection_order: "declared_axis_order_last_axis_fastest".to_owned(),
            partial_grid_policy: "reject_entire_lut".to_owned(),
            thread_count_per_process: 1,
            grouping: Some(RawExecutionGrouping {
                model: "fresh_crash_isolated_material_state_process".to_owned(),
                material_state_axis_kinds: vec![
                    AxisKind::Temperature,
                    AxisKind::CondensedVolumeFraction,
                    AxisKind::LiquidMassFraction,
                    AxisKind::Frequency,
                ],
                tmatrix_state_axis_kinds: vec![
                    AxisKind::EquivolumeDiameter,
                    AxisKind::MinorToMajorAxisRatio,
                ],
                geometry_axis_kind: AxisKind::RadarElevation,
                partial_group_policy: "reject_entire_lut".to_owned(),
                maximum_points_per_process: 2048,
                group_timeout_seconds: 3600,
            }),
        }
    }

    #[test]
    fn grouped_execution_contract_is_exact_and_typed() {
        let bound =
            verify_execution(grouped_wet_execution(), &wet_material_for_execution()).unwrap();
        assert!(matches!(
            bound,
            TMatrixExecutionDescriptor::FreshProcessPerMaterialStateGroup {
                maximum_points_per_process: 2048,
                group_timeout_seconds: 3600,
                ..
            }
        ));
        let mut changed = grouped_wet_execution();
        changed
            .grouping
            .as_mut()
            .unwrap()
            .material_state_axis_kinds
            .swap(0, 1);
        assert!(verify_execution(changed, &wet_material_for_execution()).is_err());
        let mut expanded = grouped_wet_execution();
        expanded
            .grouping
            .as_mut()
            .unwrap()
            .maximum_points_per_process = 4096;
        assert!(matches!(
            verify_execution(expanded, &wet_material_for_execution()).unwrap(),
            TMatrixExecutionDescriptor::FreshProcessPerMaterialStateGroup {
                maximum_points_per_process: 4096,
                ..
            }
        ));
        let mut changed = grouped_wet_execution();
        changed
            .grouping
            .as_mut()
            .unwrap()
            .maximum_points_per_process = 1024;
        assert!(verify_execution(changed, &wet_material_for_execution()).is_err());
    }

    #[test]
    fn solver_order_is_exact_for_each_population_role() {
        for (role, expected_ndgs) in [
            (TMatrixPopulationRole::OrdinaryConventional, 2),
            (
                TMatrixPopulationRole::ConventionalRainStandaloneAndResidual,
                14,
            ),
            (
                TMatrixPopulationRole::PropertyAwareDryCharacteristicParticle,
                14,
            ),
            (
                TMatrixPopulationRole::PropertyAwareWetCharacteristicParticle,
                14,
            ),
        ] {
            verify_solver_ndgs(role, expected_ndgs).unwrap();
            assert!(verify_solver_ndgs(role, expected_ndgs - 1).is_err());
            assert!(verify_solver_ndgs(role, expected_ndgs + 1).is_err());
        }
    }

    #[test]
    fn solver_convergence_tolerance_is_exact() {
        verify_solver_ddelt(0.001).unwrap();
        for changed in [0.000_999, 0.001_001, 0.0, -0.001, f64::NAN] {
            assert!(verify_solver_ddelt(changed).is_err());
        }
    }

    #[test]
    fn wet_dielectric_temperature_range_is_bound_exactly() {
        let raw = |temperature_range_k| RawDielectric::SymmetricBruggemanSphericalAirIceWaterV1 {
            air_relative_permittivity: RawComplexIndex {
                real: 1.0,
                imaginary: 0.0,
            },
            ice_permittivity_model: "matzler_2006".to_owned(),
            liquid_water_permittivity_model: "liebe_hufford_manabe_1991_double_debye".to_owned(),
            ice_temperature_treatment:
                "minimum_environment_temperature_and_273p15_k_phase_equilibrium".to_owned(),
            ice_material_density_kg_m3: 917.0,
            liquid_water_density_kg_m3: 999.84,
            condensed_volume_fraction_interpretation:
                "ice_plus_liquid_component_volume_over_outer_spheroid_volume".to_owned(),
            liquid_mass_fraction_interpretation: "liquid_mass_over_total_condensed_mass".to_owned(),
            component_volume_fraction_conversion:
                "condensed_volume_fraction_times_mass_specific_volume_shares".to_owned(),
            bulk_density_reconstruction:
                "condensed_volume_fraction_divided_by_total_component_specific_volume".to_owned(),
            mixing_equation:
                "sum_f_j_times_eps_j_minus_eps_eff_over_eps_j_plus_2eps_eff_equals_zero".to_owned(),
            root_selection: "vacuum_to_constituents_homotopy_passive_continuous_branch".to_owned(),
            homotopy_steps: 64,
            newton_max_iterations: 100,
            newton_relative_tolerance: 1.0e-12,
            temperature_range_k,
            applicability: "quasistatic_spherical_inclusions_homogeneous_effective_medium"
                .to_owned(),
        };
        assert!(matches!(
            bind_material(raw([269.15, 275.15])).unwrap(),
            TMatrixMaterial::SymmetricBruggemanSphericalAirIceWaterV1 {
                temperature_range_k: [269.15, 275.15],
                ..
            }
        ));
        assert!(bind_material(raw([269.15, 276.15])).is_err());
    }
}
