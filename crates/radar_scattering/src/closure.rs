//! Deterministic closures from one raw WRF grid-point state to the particle
//! states used by this crate.
//!
//! This module does not read WRF files and does not evaluate a scattering
//! kernel.  Every property records whether it came from a native prognostic,
//! a WRF diagnostic, a documented closure, or an explicit assumption.

use std::f64::consts::PI;

use thiserror::Error;

use crate::{
    ClosureAssumption, ConventionalHydrometeor, ConventionalParticleState, ConventionalProvenance,
    IshmaelIceCategory, IshmaelParticleState, IshmaelProvenance, OrientationModel, P3ParticleState,
    P3Provenance, ParticleEnvironment, ParticleError, ParticleProvenance, ParticleRecord,
    ParticleShape, ParticleState, ProvenanceError, SourceVariable,
};

/// Versioned implementation identifier carried by all analytic properties.
pub const PROPERTY_CLOSURE_REVISION: &str = "wrf-property-closure-v1";

/// Cold edge (-5 C) of the explicitly diagnostic coexistence envelope.
pub const DIAGNOSTIC_COEXISTENCE_COLD_K: f64 = 268.15;

/// Warm edge (+2 C) of the explicitly diagnostic coexistence envelope.
pub const DIAGNOSTIC_COEXISTENCE_WARM_K: f64 = 275.15;

const ICE_MATERIAL_DENSITY_KG_M3: f64 = 917.0;
const WATER_DENSITY_KG_M3: f64 = 1_000.0;
const REFERENCE_AIR_DENSITY_KG_M3: f64 = 1.225;
const DEFAULT_GAUSSIAN_QUADRATURE_POINTS: u16 = 16;
const ISOTROPIC_QUADRATURE_POINTS: u16 = 64;

/// The evidence class for a closed scalar property.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PropertySourceKind {
    /// Algebra using only prognostic variables of the selected scheme.
    NativePrognostic,
    /// A WRF-emitted diagnostic took precedence over an analytic fallback.
    WrfDiagnostic,
    /// A formula documented by [`PROPERTY_CLOSURE_REVISION`].
    DocumentedClosure,
    /// A value not predicted by the microphysics scheme.
    Assumed,
}

/// Auditable provenance for one property.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PropertyProvenance {
    kind: PropertySourceKind,
    source_variables: Vec<&'static str>,
    method: &'static str,
}

impl PropertyProvenance {
    fn new(
        kind: PropertySourceKind,
        source_variables: Vec<&'static str>,
        method: &'static str,
    ) -> Self {
        Self {
            kind,
            source_variables,
            method,
        }
    }

    #[must_use]
    pub const fn kind(&self) -> PropertySourceKind {
        self.kind
    }

    #[must_use]
    pub fn source_variables(&self) -> &[&'static str] {
        &self.source_variables
    }

    #[must_use]
    pub const fn method(&self) -> &'static str {
        self.method
    }
}

/// A scalar whose numerical value cannot be separated from its provenance.
#[derive(Clone, Debug, PartialEq)]
pub struct SourcedScalar {
    value: f64,
    provenance: PropertyProvenance,
}

impl SourcedScalar {
    fn new(value: f64, provenance: PropertyProvenance) -> Self {
        Self { value, provenance }
    }

    #[must_use]
    pub const fn value(&self) -> f64 {
        self.value
    }

    #[must_use]
    pub const fn provenance(&self) -> &PropertyProvenance {
        &self.provenance
    }
}

/// Orientation policy requested by a caller of a grid-point closure.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OrientationDefinition {
    /// Scheme-family defaults. They are assumptions, never native predictions.
    #[default]
    SchemeDefault,
    /// A zero-width, zero-mean Gaussian canting override.
    Aligned,
    /// A true isotropic distribution, not a very broad Gaussian surrogate.
    Isotropic,
}

/// Validated orientation metadata associated with a closed category.
#[derive(Clone, Debug, PartialEq)]
pub struct ClosedOrientation {
    definition: OrientationDefinition,
    model: OrientationModel,
    provenance: PropertyProvenance,
}

impl ClosedOrientation {
    #[must_use]
    pub const fn definition(&self) -> OrientationDefinition {
        self.definition
    }

    #[must_use]
    pub const fn model(&self) -> &OrientationModel {
        &self.model
    }

    #[must_use]
    pub const fn provenance(&self) -> &PropertyProvenance {
        &self.provenance
    }

    /// Return Gaussian parameters when the selected model is Gaussian.
    #[must_use]
    pub fn gaussian_parameters(&self) -> Option<(f64, f64, u16)> {
        match self.model {
            OrientationModel::GaussianCanting {
                mean_deg,
                standard_deviation_deg,
                quadrature_points,
            } => Some((mean_deg, standard_deviation_deg, quadrature_points)),
            _ => None,
        }
    }
}

/// Grid-point context shared by all category closures.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClosureContext {
    wrf_mp_physics: i32,
    environment: ParticleEnvironment,
    orientation: OrientationDefinition,
}

impl ClosureContext {
    pub fn new(
        wrf_mp_physics: i32,
        temperature_k: f64,
        air_density_kg_m3: f64,
    ) -> Result<Self, ClosureError> {
        if wrf_mp_physics <= 0 {
            return Err(ClosureError::InvalidSchemeId {
                value: wrf_mp_physics,
            });
        }
        Ok(Self {
            wrf_mp_physics,
            environment: ParticleEnvironment::new(temperature_k, air_density_kg_m3)?,
            orientation: OrientationDefinition::SchemeDefault,
        })
    }

    pub fn with_environment(
        wrf_mp_physics: i32,
        environment: ParticleEnvironment,
    ) -> Result<Self, ClosureError> {
        if wrf_mp_physics <= 0 {
            return Err(ClosureError::InvalidSchemeId {
                value: wrf_mp_physics,
            });
        }
        Ok(Self {
            wrf_mp_physics,
            environment,
            orientation: OrientationDefinition::SchemeDefault,
        })
    }

    #[must_use]
    pub const fn with_orientation(mut self, orientation: OrientationDefinition) -> Self {
        self.orientation = orientation;
        self
    }

    #[must_use]
    pub const fn wrf_mp_physics(self) -> i32 {
        self.wrf_mp_physics
    }

    #[must_use]
    pub const fn environment(self) -> ParticleEnvironment {
        self.environment
    }

    #[must_use]
    pub const fn orientation(self) -> OrientationDefinition {
        self.orientation
    }
}

/// Native P3 ice category represented by one WRF scalar tuple.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum P3Category {
    Category1,
    /// The second free-ice category is valid only for WRF `mp_physics=52`.
    Category2,
}

/// Raw P3 scalar inputs. Optional storage lets a WRF adapter report absence
/// through [`ClosureError::MissingInput`] instead of substituting a value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct P3CategoryInput {
    category: P3Category,
    qice_kgkg: Option<f64>,
    qnice_per_kg: Option<f64>,
    qir_kgkg: Option<f64>,
    qib_m3_per_kg: Option<f64>,
    qzi: Option<f64>,
}

impl P3CategoryInput {
    #[must_use]
    pub const fn new(
        category: P3Category,
        qice_kgkg: f64,
        qnice_per_kg: f64,
        qir_kgkg: f64,
        qib_m3_per_kg: f64,
    ) -> Self {
        Self {
            category,
            qice_kgkg: Some(qice_kgkg),
            qnice_per_kg: Some(qnice_per_kg),
            qir_kgkg: Some(qir_kgkg),
            qib_m3_per_kg: Some(qib_m3_per_kg),
            qzi: None,
        }
    }

    #[must_use]
    pub const fn category1(
        qice_kgkg: f64,
        qnice_per_kg: f64,
        qir_kgkg: f64,
        qib_m3_per_kg: f64,
    ) -> Self {
        Self::new(
            P3Category::Category1,
            qice_kgkg,
            qnice_per_kg,
            qir_kgkg,
            qib_m3_per_kg,
        )
    }

    #[must_use]
    pub const fn category2(
        qice_kgkg: f64,
        qnice_per_kg: f64,
        qir_kgkg: f64,
        qib_m3_per_kg: f64,
    ) -> Self {
        Self::new(
            P3Category::Category2,
            qice_kgkg,
            qnice_per_kg,
            qir_kgkg,
            qib_m3_per_kg,
        )
    }

    #[must_use]
    pub const fn from_optional(
        category: P3Category,
        qice_kgkg: Option<f64>,
        qnice_per_kg: Option<f64>,
        qir_kgkg: Option<f64>,
        qib_m3_per_kg: Option<f64>,
        qzi: Option<f64>,
    ) -> Self {
        Self {
            category,
            qice_kgkg,
            qnice_per_kg,
            qir_kgkg,
            qib_m3_per_kg,
            qzi,
        }
    }

    #[must_use]
    pub const fn with_qzi(mut self, qzi: f64) -> Self {
        self.qzi = Some(qzi);
        self
    }

    #[must_use]
    pub const fn category(self) -> P3Category {
        self.category
    }
}

/// Optional ISHMAEL WRF diagnostics. A present diagnostic is authoritative:
/// an invalid value is an error and never falls back to a prognostic closure.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct IshmaelDiagnostics {
    d_ice_m: Option<f64>,
    rho_ice_kg_m3: Option<f64>,
    phi_ice: Option<f64>,
    v_ice_m_s: Option<f64>,
}

/// Exact WRF variable names behind one ISHMAEL tuple.
///
/// The WRF file adapter supplies these names because category 2/3 use
/// suffixes and official diagnostic variables are lower-case.  Keeping names
/// on the input prevents a successfully closed category from being stamped as
/// if it came from the unsuffixed shorthand tuple.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IshmaelSourceFields {
    qice: &'static str,
    qnice: &'static str,
    qvoli: &'static str,
    qaoli: &'static str,
    d_ice: &'static str,
    rho_ice: &'static str,
    phi_ice: &'static str,
    v_ice: &'static str,
}

impl IshmaelSourceFields {
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        qice: &'static str,
        qnice: &'static str,
        qvoli: &'static str,
        qaoli: &'static str,
        d_ice: &'static str,
        rho_ice: &'static str,
        phi_ice: &'static str,
        v_ice: &'static str,
    ) -> Self {
        Self {
            qice,
            qnice,
            qvoli,
            qaoli,
            d_ice,
            rho_ice,
            phi_ice,
            v_ice,
        }
    }

    #[must_use]
    pub const fn wrf_category_1_shorthand() -> Self {
        Self::new(
            "QICE", "QNICE", "QVOLI", "QAOLI", "D_ICE", "RHO_ICE", "PHI_ICE", "V_ICE",
        )
    }

    #[must_use]
    pub const fn required(self) -> [&'static str; 4] {
        [self.qice, self.qnice, self.qvoli, self.qaoli]
    }

    #[must_use]
    pub const fn diagnostics(self) -> [&'static str; 4] {
        [self.d_ice, self.rho_ice, self.phi_ice, self.v_ice]
    }
}

impl IshmaelDiagnostics {
    #[must_use]
    pub const fn new(
        d_ice_m: Option<f64>,
        rho_ice_kg_m3: Option<f64>,
        phi_ice: Option<f64>,
        v_ice_m_s: Option<f64>,
    ) -> Self {
        Self {
            d_ice_m,
            rho_ice_kg_m3,
            phi_ice,
            v_ice_m_s,
        }
    }

    #[must_use]
    pub const fn d_ice_m(self) -> Option<f64> {
        self.d_ice_m
    }

    #[must_use]
    pub const fn rho_ice_kg_m3(self) -> Option<f64> {
        self.rho_ice_kg_m3
    }

    #[must_use]
    pub const fn phi_ice(self) -> Option<f64> {
        self.phi_ice
    }

    #[must_use]
    pub const fn v_ice_m_s(self) -> Option<f64> {
        self.v_ice_m_s
    }
}

/// Raw scalar tuple for one ISHMAEL physical state.
///
/// WRF's three prognostic tuple ordinals are carried separately through
/// [`IshmaelSourceFields`], so this physical category never fabricates a field
/// mapping.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IshmaelCategoryInput {
    category: IshmaelIceCategory,
    qice_kgkg: Option<f64>,
    qnice_per_kg: Option<f64>,
    qvoli_m3_per_kg: Option<f64>,
    qaoli_m3_per_kg: Option<f64>,
    diagnostics: IshmaelDiagnostics,
    source_fields: IshmaelSourceFields,
}

impl IshmaelCategoryInput {
    #[must_use]
    pub const fn new(
        category: IshmaelIceCategory,
        qice_kgkg: f64,
        qnice_per_kg: f64,
        qvoli_m3_per_kg: f64,
        qaoli_m3_per_kg: f64,
    ) -> Self {
        Self {
            category,
            qice_kgkg: Some(qice_kgkg),
            qnice_per_kg: Some(qnice_per_kg),
            qvoli_m3_per_kg: Some(qvoli_m3_per_kg),
            qaoli_m3_per_kg: Some(qaoli_m3_per_kg),
            diagnostics: IshmaelDiagnostics::new(None, None, None, None),
            source_fields: IshmaelSourceFields::wrf_category_1_shorthand(),
        }
    }

    #[must_use]
    pub const fn from_optional(
        category: IshmaelIceCategory,
        qice_kgkg: Option<f64>,
        qnice_per_kg: Option<f64>,
        qvoli_m3_per_kg: Option<f64>,
        qaoli_m3_per_kg: Option<f64>,
        diagnostics: IshmaelDiagnostics,
    ) -> Self {
        Self {
            category,
            qice_kgkg,
            qnice_per_kg,
            qvoli_m3_per_kg,
            qaoli_m3_per_kg,
            diagnostics,
            source_fields: IshmaelSourceFields::wrf_category_1_shorthand(),
        }
    }

    #[must_use]
    pub const fn with_diagnostics(mut self, diagnostics: IshmaelDiagnostics) -> Self {
        self.diagnostics = diagnostics;
        self
    }

    /// Attach the exact WRF tuple names used by this input.
    #[must_use]
    pub const fn with_source_fields(mut self, source_fields: IshmaelSourceFields) -> Self {
        self.source_fields = source_fields;
        self
    }

    #[must_use]
    pub const fn source_fields(self) -> IshmaelSourceFields {
        self.source_fields
    }

    #[must_use]
    pub const fn category(self) -> IshmaelIceCategory {
        self.category
    }
}

/// Raw input for a conventional bulk category. Direct WRF diagnostics are
/// optional; without `D_HYDROMETEOR`, number concentration is required.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ConventionalCategoryInput {
    category: ConventionalHydrometeor,
    mixing_ratio_kgkg: Option<f64>,
    number_per_kg: Option<f64>,
    characteristic_diameter_m: Option<f64>,
    bulk_density_kg_m3: Option<f64>,
    minor_to_major_axis_ratio: Option<f64>,
    fall_speed_m_s: Option<f64>,
}

impl ConventionalCategoryInput {
    #[must_use]
    pub const fn new(
        category: ConventionalHydrometeor,
        mixing_ratio_kgkg: f64,
        number_per_kg: Option<f64>,
    ) -> Self {
        Self {
            category,
            mixing_ratio_kgkg: Some(mixing_ratio_kgkg),
            number_per_kg,
            characteristic_diameter_m: None,
            bulk_density_kg_m3: None,
            minor_to_major_axis_ratio: None,
            fall_speed_m_s: None,
        }
    }

    #[must_use]
    pub const fn from_optional(
        category: ConventionalHydrometeor,
        mixing_ratio_kgkg: Option<f64>,
        number_per_kg: Option<f64>,
    ) -> Self {
        Self {
            category,
            mixing_ratio_kgkg,
            number_per_kg,
            characteristic_diameter_m: None,
            bulk_density_kg_m3: None,
            minor_to_major_axis_ratio: None,
            fall_speed_m_s: None,
        }
    }

    #[must_use]
    pub const fn with_characteristic_diameter_m(mut self, value: f64) -> Self {
        self.characteristic_diameter_m = Some(value);
        self
    }

    #[must_use]
    pub const fn with_bulk_density_kg_m3(mut self, value: f64) -> Self {
        self.bulk_density_kg_m3 = Some(value);
        self
    }

    #[must_use]
    pub const fn with_minor_to_major_axis_ratio(mut self, value: f64) -> Self {
        self.minor_to_major_axis_ratio = Some(value);
        self
    }

    #[must_use]
    pub const fn with_fall_speed_m_s(mut self, value: f64) -> Self {
        self.fall_speed_m_s = Some(value);
        self
    }

    #[must_use]
    pub const fn category(self) -> ConventionalHydrometeor {
        self.category
    }
}

/// Properties produced alongside the pre-existing validated particle record.
#[derive(Clone, Debug, PartialEq)]
pub struct ClosedParticleCategory {
    record: ParticleRecord,
    characteristic_diameter_m: SourcedScalar,
    effective_density_kg_m3: SourcedScalar,
    minor_to_major_axis_ratio: SourcedScalar,
    fall_speed_m_s: SourcedScalar,
    rime_mass_fraction: Option<SourcedScalar>,
    rime_density_kg_m3: Option<SourcedScalar>,
    sixth_moment_m6: Option<SourcedScalar>,
    orientation: ClosedOrientation,
}

impl ClosedParticleCategory {
    #[must_use]
    pub const fn record(&self) -> &ParticleRecord {
        &self.record
    }

    #[must_use]
    pub const fn characteristic_diameter_m(&self) -> &SourcedScalar {
        &self.characteristic_diameter_m
    }

    #[must_use]
    pub const fn effective_density_kg_m3(&self) -> &SourcedScalar {
        &self.effective_density_kg_m3
    }

    #[must_use]
    pub const fn minor_to_major_axis_ratio(&self) -> &SourcedScalar {
        &self.minor_to_major_axis_ratio
    }

    #[must_use]
    pub const fn fall_speed_m_s(&self) -> &SourcedScalar {
        &self.fall_speed_m_s
    }

    #[must_use]
    pub const fn rime_mass_fraction(&self) -> Option<&SourcedScalar> {
        self.rime_mass_fraction.as_ref()
    }

    #[must_use]
    pub const fn rime_density_kg_m3(&self) -> Option<&SourcedScalar> {
        self.rime_density_kg_m3.as_ref()
    }

    /// WRF-native M6 units are retained; no scattering calibration is implied.
    #[must_use]
    pub const fn sixth_moment_m6(&self) -> Option<&SourcedScalar> {
        self.sixth_moment_m6.as_ref()
    }

    #[must_use]
    pub const fn orientation(&self) -> &ClosedOrientation {
        &self.orientation
    }

    #[must_use]
    pub fn mixing_ratio_kgkg(&self) -> f64 {
        state_mass(self.record.state())
    }

    #[must_use]
    pub fn shape(&self) -> ParticleShape {
        state_shape(self.record.state())
    }
}

/// Close one conventional bulk category.
pub fn close_conventional_category(
    context: &ClosureContext,
    input: &ConventionalCategoryInput,
) -> Result<ClosedParticleCategory, ClosureError> {
    // P3 and ISHMAEL retain a conventional two-moment liquid-rain tuple
    // (QRAIN/QNRAIN).  Other conventional frozen/cloud categories would
    // duplicate their property-aware scheme state and remain forbidden.
    if matches!(context.wrf_mp_physics, 50..=53 | 55)
        && input.category != ConventionalHydrometeor::Rain
    {
        return Err(ClosureError::SchemeFamilyMismatch {
            wrf_mp_physics: context.wrf_mp_physics,
            requested: "conventional",
        });
    }

    let mixing_ratio = required(
        input.mixing_ratio_kgkg,
        conventional_mass_name(input.category),
    )?;
    positive(conventional_mass_name(input.category), mixing_ratio)?;
    if let Some(number) = input.number_per_kg {
        positive(conventional_number_name(input.category), number)?;
    }

    let (density, density_source) = if let Some(value) = input.bulk_density_kg_m3 {
        physical_density("RHO_HYDROMETEOR", value, WATER_DENSITY_KG_M3)?;
        (
            value,
            PropertyProvenance::new(
                PropertySourceKind::WrfDiagnostic,
                vec!["RHO_HYDROMETEOR"],
                "direct conventional-category density diagnostic",
            ),
        )
    } else {
        (
            conventional_default_density(input.category),
            PropertyProvenance::new(
                PropertySourceKind::Assumed,
                vec![],
                "documented conventional-category density default",
            ),
        )
    };

    let (diameter, diameter_source) = if let Some(value) = input.characteristic_diameter_m {
        positive("D_HYDROMETEOR", value)?;
        (
            value,
            PropertyProvenance::new(
                PropertySourceKind::WrfDiagnostic,
                vec!["D_HYDROMETEOR"],
                "direct conventional-category characteristic diameter diagnostic",
            ),
        )
    } else {
        let number = input.number_per_kg.ok_or(ClosureError::MissingInput {
            field: "number concentration or D_HYDROMETEOR",
        })?;
        let value = mass_equivalent_diameter(mixing_ratio, number, density)?;
        (
            value,
            PropertyProvenance::new(
                PropertySourceKind::DocumentedClosure,
                vec![
                    conventional_mass_name(input.category),
                    conventional_number_name(input.category),
                ],
                "spherical mass-equivalent diameter from q, N, and category density",
            ),
        )
    };

    let (axis_ratio, axis_source) = if let Some(value) = input.minor_to_major_axis_ratio {
        axis_ratio_value("PHI_HYDROMETEOR", value)?;
        (
            value,
            PropertyProvenance::new(
                PropertySourceKind::WrfDiagnostic,
                vec!["PHI_HYDROMETEOR"],
                "direct conventional-category minor-to-major axis ratio diagnostic",
            ),
        )
    } else {
        (
            conventional_default_axis_ratio(input.category, diameter),
            PropertyProvenance::new(
                PropertySourceKind::Assumed,
                vec![],
                "documented conventional-category axis-ratio default",
            ),
        )
    };

    let (fall_speed, fall_source) = if let Some(value) = input.fall_speed_m_s {
        positive("V_HYDROMETEOR", value)?;
        (
            value,
            PropertyProvenance::new(
                PropertySourceKind::WrfDiagnostic,
                vec!["V_HYDROMETEOR"],
                "direct positive-downward terminal-speed diagnostic",
            ),
        )
    } else {
        (
            analytic_fall_speed(
                diameter,
                density,
                axis_ratio,
                context.environment.air_density_kg_m3(),
            )?,
            PropertyProvenance::new(
                PropertySourceKind::DocumentedClosure,
                vec![conventional_mass_name(input.category)],
                "property-closure-v1 positive-downward density/size/aspect fall-speed relation",
            ),
        )
    };

    let shape = ParticleShape::new(diameter, density, axis_ratio, 0.0)?;
    let state = ConventionalParticleState::new(
        input.category,
        context.environment,
        shape,
        mixing_ratio,
        input.number_per_kg,
    )?;
    let mut source_variables = vec![SourceVariable::new(
        conventional_mass_name(input.category),
        "kg kg-1 dry air",
    )?];
    if input.number_per_kg.is_some() {
        source_variables.push(SourceVariable::new(
            conventional_number_name(input.category),
            "kg-1 dry air",
        )?);
    }
    for (present, name, units) in [
        (
            input.characteristic_diameter_m.is_some(),
            "D_HYDROMETEOR",
            "m",
        ),
        (
            input.bulk_density_kg_m3.is_some(),
            "RHO_HYDROMETEOR",
            "kg m-3",
        ),
        (
            input.minor_to_major_axis_ratio.is_some(),
            "PHI_HYDROMETEOR",
            "1",
        ),
        (input.fall_speed_m_s.is_some(), "V_HYDROMETEOR", "m s-1"),
    ] {
        if present {
            source_variables.push(SourceVariable::new(name, units)?);
        }
    }
    let provenance = ConventionalProvenance::new(
        "WRF conventional bulk microphysics",
        PROPERTY_CLOSURE_REVISION,
        source_variables,
        vec![ClosureAssumption::DiagnosedShape {
            identifier: PROPERTY_CLOSURE_REVISION.to_owned(),
        }],
        Some(context.wrf_mp_physics),
    )?;
    let record = ParticleRecord::new(
        ParticleState::Conventional(state),
        ParticleProvenance::Conventional(provenance),
    )?;

    Ok(ClosedParticleCategory {
        record,
        characteristic_diameter_m: SourcedScalar::new(diameter, diameter_source),
        effective_density_kg_m3: SourcedScalar::new(density, density_source),
        minor_to_major_axis_ratio: SourcedScalar::new(axis_ratio, axis_source),
        fall_speed_m_s: SourcedScalar::new(fall_speed, fall_source),
        rime_mass_fraction: None,
        rime_density_kg_m3: None,
        sixth_moment_m6: None,
        orientation: close_orientation(
            context.orientation,
            conventional_default_canting(input.category),
            "conventional categories do not predict particle canting",
        ),
    })
}

/// Close one P3 category for WRF `mp_physics=50..=53`.
pub fn close_p3_category(
    context: &ClosureContext,
    input: &P3CategoryInput,
) -> Result<ClosedParticleCategory, ClosureError> {
    if !(50..=53).contains(&context.wrf_mp_physics) {
        return Err(ClosureError::SchemeFamilyMismatch {
            wrf_mp_physics: context.wrf_mp_physics,
            requested: "P3",
        });
    }
    if input.category == P3Category::Category2 && context.wrf_mp_physics != 52 {
        return Err(ClosureError::CategoryUnavailable {
            wrf_mp_physics: context.wrf_mp_physics,
            category: "P3 category 2",
        });
    }

    let names = p3_names(input.category);
    let qice = required(input.qice_kgkg, names.qice)?;
    let qnice = required(input.qnice_per_kg, names.qnice)?;
    let qir = required(input.qir_kgkg, names.qir)?;
    let qib = required(input.qib_m3_per_kg, names.qib)?;
    positive(names.qice, qice)?;
    positive(names.qnice, qnice)?;
    nonnegative(names.qir, qir)?;
    nonnegative(names.qib, qib)?;
    if qir > qice {
        return Err(ClosureError::InconsistentInputs {
            relation: "QIR must not exceed QICE",
            left: qir,
            right: qice,
        });
    }

    // The clamp is only a final floating-point physical bound after the
    // explicit QIR<=QICE validation above; it never hides invalid mass.
    let rime_fraction = (qir / qice).clamp(0.0, 1.0);
    let (rime_density, rime_density_source) = if qir > 0.0 {
        if qib <= 0.0 {
            return Err(ClosureError::MissingPositiveVolume {
                mass_field: names.qir,
                volume_field: names.qib,
            });
        }
        let density = checked_ratio("P3 rime density", qir, qib)?;
        physical_density("P3 rime density", density, ICE_MATERIAL_DENSITY_KG_M3)?;
        (
            density,
            PropertyProvenance::new(
                PropertySourceKind::NativePrognostic,
                vec![names.qir, names.qib],
                "QIR/QIB rime density",
            ),
        )
    } else {
        if qib != 0.0 {
            return Err(ClosureError::InconsistentInputs {
                relation: "QIB must be zero when QIR is zero",
                left: qib,
                right: qir,
            });
        }
        (
            ICE_MATERIAL_DENSITY_KG_M3,
            PropertyProvenance::new(
                PropertySourceKind::Assumed,
                vec![],
                "irrelevant dense-ice sentinel when rime mass fraction is exactly zero",
            ),
        )
    };

    let unrimed_mass = qice - qir;
    let represented_volume = qib + unrimed_mass / ICE_MATERIAL_DENSITY_KG_M3;
    let effective_density = checked_ratio("P3 effective density", qice, represented_volume)?;
    physical_density(
        "P3 effective density",
        effective_density,
        ICE_MATERIAL_DENSITY_KG_M3,
    )?;
    let effective_density_source = PropertyProvenance::new(
        PropertySourceKind::DocumentedClosure,
        vec![names.qice, names.qir, names.qib],
        "QICE / (QIB + (QICE-QIR)/917): constituent-volume effective density",
    );

    let (sixth_moment, diameter, diameter_source) = if context.wrf_mp_physics == 53 {
        if input.category != P3Category::Category1 {
            return Err(ClosureError::CategoryUnavailable {
                wrf_mp_physics: 53,
                category: "P3 category 2",
            });
        }
        let qzi = required(input.qzi, "QZI")?;
        positive("QZI", qzi)?;
        let qzi_squared = qzi * qzi;
        finite_result("QZI squared", qzi_squared)?;
        // Advected QZI=(N*Z)^0.5. Restore the bulk sixth moment
        // M6=Z=QZI^2/N first, then obtain the number-mean sixth power
        // D6=M6/N for the characteristic diameter. These are two distinct
        // divisions by N, not a duplicated algebra step.
        let bulk_m6 = checked_ratio("P3 bulk M6", qzi_squared, qnice)?;
        positive("P3 bulk M6", bulk_m6)?;
        let mean_d6 = checked_ratio("P3 number-mean D6", bulk_m6, qnice)?;
        let diameter = mean_d6.powf(1.0 / 6.0);
        positive("P3 sixth-moment characteristic diameter", diameter)?;
        (
            Some(SourcedScalar::new(
                bulk_m6,
                PropertyProvenance::new(
                    PropertySourceKind::NativePrognostic,
                    vec!["QZI", names.qnice],
                    "P3-53 exact bulk M6 recovery: M6=QZI^2/QNICE",
                ),
            )),
            diameter,
            PropertyProvenance::new(
                PropertySourceKind::DocumentedClosure,
                vec!["QZI", names.qnice],
                "number-mean D6=M6/QNICE; characteristic diameter D6^(1/6)",
            ),
        )
    } else {
        if input.qzi.is_some() {
            return Err(ClosureError::UnexpectedInput {
                field: "QZI",
                wrf_mp_physics: context.wrf_mp_physics,
            });
        }
        (
            None,
            mass_equivalent_diameter(qice, qnice, effective_density)?,
            PropertyProvenance::new(
                PropertySourceKind::DocumentedClosure,
                vec![names.qice, names.qnice, names.qir, names.qib],
                "spherical mass-equivalent diameter from q, N, and effective density",
            ),
        )
    };

    let axis_ratio = 0.6 + 0.4 * rime_fraction;
    axis_ratio_value("P3 minor-to-major axis ratio", axis_ratio)?;
    let fall_speed = analytic_fall_speed(
        diameter,
        effective_density,
        axis_ratio,
        context.environment.air_density_kg_m3(),
    )?;
    let shape = ParticleShape::new(diameter, effective_density, axis_ratio, 0.0)?;
    let state = P3ParticleState::new(
        context.environment,
        shape,
        qice,
        qnice,
        rime_fraction,
        rime_density,
    )?;

    let mut source_variables = vec![
        SourceVariable::new(names.qice, "kg kg-1 dry air")?,
        SourceVariable::new(names.qnice, "kg-1 dry air")?,
        SourceVariable::new(names.qir, "kg kg-1 dry air")?,
        SourceVariable::new(names.qib, "m3 kg-1 dry air")?,
    ];
    if context.wrf_mp_physics == 53 {
        source_variables.push(SourceVariable::new("QZI", "WRF native QZI units")?);
    }
    let provenance = P3Provenance::new(
        format!("WRF P3 mp_physics={}", context.wrf_mp_physics),
        PROPERTY_CLOSURE_REVISION,
        source_variables,
        vec![
            ClosureAssumption::SchemeNative,
            ClosureAssumption::DiagnosedShape {
                identifier: PROPERTY_CLOSURE_REVISION.to_owned(),
            },
        ],
        format!("P3-{}", context.wrf_mp_physics),
    )?;
    let record = ParticleRecord::new(ParticleState::P3(state), ParticleProvenance::P3(provenance))?;
    let default_canting = 10.0 + 30.0 * rime_fraction;

    Ok(ClosedParticleCategory {
        record,
        characteristic_diameter_m: SourcedScalar::new(diameter, diameter_source),
        effective_density_kg_m3: SourcedScalar::new(effective_density, effective_density_source),
        minor_to_major_axis_ratio: SourcedScalar::new(
            axis_ratio,
            PropertyProvenance::new(
                PropertySourceKind::DocumentedClosure,
                vec![names.qir, names.qice],
                "0.6 + 0.4*rime_mass_fraction; closure geometry, not a P3 LUT result",
            ),
        ),
        fall_speed_m_s: SourcedScalar::new(
            fall_speed,
            PropertyProvenance::new(
                PropertySourceKind::DocumentedClosure,
                vec![names.qice, names.qnice, names.qir, names.qib],
                "property-closure-v1 positive-downward density/size/aspect fall-speed relation",
            ),
        ),
        rime_mass_fraction: Some(SourcedScalar::new(
            rime_fraction,
            PropertyProvenance::new(
                PropertySourceKind::NativePrognostic,
                vec![names.qir, names.qice],
                "bounded QIR/QICE after explicit mass-consistency validation",
            ),
        )),
        rime_density_kg_m3: Some(SourcedScalar::new(rime_density, rime_density_source)),
        sixth_moment_m6: sixth_moment,
        orientation: close_orientation(
            context.orientation,
            default_canting,
            "P3 predicts no canting; default sigma=10+30*rime_fraction degrees",
        ),
    })
}

/// Close one physical ISHMAEL state for WRF `mp_physics=55`.
///
/// WRF currently carries three prognostic tuples.  [`IshmaelSourceFields`]
/// keeps their exact suffix/case distinct from the physical-category enum.
pub fn close_ishmael_category(
    context: &ClosureContext,
    input: &IshmaelCategoryInput,
) -> Result<ClosedParticleCategory, ClosureError> {
    if context.wrf_mp_physics != 55 {
        return Err(ClosureError::SchemeFamilyMismatch {
            wrf_mp_physics: context.wrf_mp_physics,
            requested: "ISHMAEL",
        });
    }
    let names = input.source_fields;
    let qice = required(input.qice_kgkg, names.qice)?;
    let qnice = required(input.qnice_per_kg, names.qnice)?;
    let qvoli = required(input.qvoli_m3_per_kg, names.qvoli)?;
    let qaoli = required(input.qaoli_m3_per_kg, names.qaoli)?;
    positive(names.qice, qice)?;
    positive(names.qnice, qnice)?;
    positive(names.qvoli, qvoli)?;
    positive(names.qaoli, qaoli)?;

    let (density, density_source) = if let Some(value) = input.diagnostics.rho_ice_kg_m3 {
        physical_density(names.rho_ice, value, ICE_MATERIAL_DENSITY_KG_M3)?;
        (
            value,
            PropertyProvenance::new(
                PropertySourceKind::WrfDiagnostic,
                vec![names.rho_ice],
                "WRF ice-density diagnostic takes precedence over prognostic mass/volume",
            ),
        )
    } else {
        let value = checked_ratio("ISHMAEL effective density", qice, qvoli)?;
        physical_density(
            "ISHMAEL effective density",
            value,
            ICE_MATERIAL_DENSITY_KG_M3,
        )?;
        (
            value,
            PropertyProvenance::new(
                PropertySourceKind::NativePrognostic,
                vec![names.qice, names.qvoli],
                "prognostic ice mass/volume bulk effective density fallback",
            ),
        )
    };

    let (diameter, diameter_source) = if let Some(value) = input.diagnostics.d_ice_m {
        positive(names.d_ice, value)?;
        (
            value,
            PropertyProvenance::new(
                PropertySourceKind::WrfDiagnostic,
                vec![names.d_ice],
                "WRF ice-diameter diagnostic takes precedence over the mass-equivalent size closure",
            ),
        )
    } else {
        (
            mass_equivalent_diameter(qice, qnice, density)?,
            PropertyProvenance::new(
                PropertySourceKind::DocumentedClosure,
                vec![names.qice, names.qnice, names.qvoli],
                "spherical mass-equivalent diameter from q, N, and selected density",
            ),
        )
    };

    let (axis_ratio, axis_source) = if let Some(value) = input.diagnostics.phi_ice {
        axis_ratio_value(names.phi_ice, value)?;
        (
            value,
            PropertyProvenance::new(
                PropertySourceKind::WrfDiagnostic,
                vec![names.phi_ice],
                "WRF ice-aspect diagnostic takes precedence over the aspect-volume closure metric",
            ),
        )
    } else {
        let value = checked_ratio("ISHMAEL QAOLI/QVOLI metric", qaoli, qvoli)?;
        axis_ratio_value("ISHMAEL QAOLI/QVOLI metric", value)?;
        (
            value,
            PropertyProvenance::new(
                PropertySourceKind::DocumentedClosure,
                vec![names.qaoli, names.qvoli],
                "volume-weighted QAOLI/QVOLI metric interpreted as closure-v1 shape; not PHI_ICE",
            ),
        )
    };

    let (fall_speed, fall_source) = if let Some(value) = input.diagnostics.v_ice_m_s {
        positive(names.v_ice, value)?;
        (
            value,
            PropertyProvenance::new(
                PropertySourceKind::WrfDiagnostic,
                vec![names.v_ice],
                "WRF ice-fall-speed diagnostic takes precedence over the analytic closure",
            ),
        )
    } else {
        (
            analytic_fall_speed(
                diameter,
                density,
                axis_ratio,
                context.environment.air_density_kg_m3(),
            )?,
            PropertyProvenance::new(
                PropertySourceKind::DocumentedClosure,
                vec![names.qice, names.qnice, names.qvoli, names.qaoli],
                "property-closure-v1 positive-downward density/size/aspect fall-speed relation",
            ),
        )
    };

    let rime_fraction = if input.category == IshmaelIceCategory::Rimed {
        1.0
    } else {
        0.0
    };
    let shape = ParticleShape::new(diameter, density, axis_ratio, 0.0)?;
    let state = IshmaelParticleState::new(
        input.category,
        context.environment,
        shape,
        qice,
        qnice,
        rime_fraction,
    )?;
    let mut source_variables = vec![
        SourceVariable::new(names.qice, "kg kg-1 dry air")?,
        SourceVariable::new(names.qnice, "kg-1 dry air")?,
        SourceVariable::new(names.qvoli, "m3 kg-1 dry air")?,
        SourceVariable::new(names.qaoli, "m3 kg-1 dry air")?,
    ];
    for (present, name, units) in [
        (input.diagnostics.d_ice_m.is_some(), names.d_ice, "m"),
        (
            input.diagnostics.rho_ice_kg_m3.is_some(),
            names.rho_ice,
            "kg m-3",
        ),
        (input.diagnostics.phi_ice.is_some(), names.phi_ice, "1"),
        (input.diagnostics.v_ice_m_s.is_some(), names.v_ice, "m s-1"),
    ] {
        if present {
            source_variables.push(SourceVariable::new(name, units)?);
        }
    }
    let provenance = IshmaelProvenance::new(
        "WRF Jensen ISHMAEL mp_physics=55",
        PROPERTY_CLOSURE_REVISION,
        source_variables,
        vec![
            ClosureAssumption::SchemeNative,
            ClosureAssumption::DiagnosedShape {
                identifier: PROPERTY_CLOSURE_REVISION.to_owned(),
            },
        ],
        "ISHMAEL-55",
    )?;
    let record = ParticleRecord::new(
        ParticleState::Ishmael(state),
        ParticleProvenance::Ishmael(provenance),
    )?;
    let default_canting = match input.category {
        IshmaelIceCategory::SmallIce
        | IshmaelIceCategory::Planar
        | IshmaelIceCategory::Columnar => 10.0,
        IshmaelIceCategory::Aggregate | IshmaelIceCategory::Rimed => 40.0,
    };

    Ok(ClosedParticleCategory {
        record,
        characteristic_diameter_m: SourcedScalar::new(diameter, diameter_source),
        effective_density_kg_m3: SourcedScalar::new(density, density_source),
        minor_to_major_axis_ratio: SourcedScalar::new(axis_ratio, axis_source),
        fall_speed_m_s: SourcedScalar::new(fall_speed, fall_source),
        rime_mass_fraction: Some(SourcedScalar::new(
            rime_fraction,
            PropertyProvenance::new(
                PropertySourceKind::Assumed,
                vec![],
                "binary closure label: Rimed=1 and other ISHMAEL categories=0",
            ),
        )),
        rime_density_kg_m3: None,
        sixth_moment_m6: None,
        orientation: close_orientation(
            context.orientation,
            default_canting,
            "ISHMAEL predicts no canting; small/planar/columnar sigma=10 and aggregate/rimed sigma=40 degrees",
        ),
    })
}

/// The only two mixture topology hooks exposed by the v1 diagnosis.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MixtureTopology {
    /// A future kernel may apply a declared effective-medium rule.
    #[default]
    HomogeneousMixedPhase,
    /// A future kernel may represent liquid water around a frozen core.
    WaterCoatedFrozenCore,
}

/// Explicit evidence that the diagnosis did not invent scattering amplitudes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MixtureScatteringStatus {
    NotEvaluatedNoLutOrAmplitude,
}

/// Metadata hook passed to a future mixture kernel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MixtureMetadata {
    topology: MixtureTopology,
    scattering_status: MixtureScatteringStatus,
}

impl MixtureMetadata {
    /// All topologies understood by this metadata schema. No amplitudes are
    /// associated with either value.
    pub const SUPPORTED_TOPOLOGIES: [MixtureTopology; 2] = [
        MixtureTopology::HomogeneousMixedPhase,
        MixtureTopology::WaterCoatedFrozenCore,
    ];

    #[must_use]
    pub const fn topology(self) -> MixtureTopology {
        self.topology
    }

    #[must_use]
    pub const fn scattering_status(self) -> MixtureScatteringStatus {
        self.scattering_status
    }
}

/// Input to the diagnostic melting/coexistence closure.
#[derive(Clone, Debug, PartialEq)]
pub struct DiagnosticCoexistenceInput {
    temperature_k: f64,
    rain: ClosedParticleCategory,
    frozen_categories: Vec<ClosedParticleCategory>,
    topology: MixtureTopology,
}

impl DiagnosticCoexistenceInput {
    pub fn new(
        temperature_k: f64,
        rain: ClosedParticleCategory,
        frozen_categories: Vec<ClosedParticleCategory>,
    ) -> Result<Self, ClosureError> {
        positive("diagnostic coexistence temperature", temperature_k)?;
        if !(DIAGNOSTIC_COEXISTENCE_COLD_K..=DIAGNOSTIC_COEXISTENCE_WARM_K).contains(&temperature_k)
        {
            return Err(ClosureError::OutsideCoexistenceEnvelope {
                temperature_k,
                cold_k: DIAGNOSTIC_COEXISTENCE_COLD_K,
                warm_k: DIAGNOSTIC_COEXISTENCE_WARM_K,
            });
        }
        if !matches!(
            rain.record.state(),
            ParticleState::Conventional(state)
                if state.category() == ConventionalHydrometeor::Rain
        ) {
            return Err(ClosureError::RainCategoryRequired);
        }
        if frozen_categories.is_empty() {
            return Err(ClosureError::NoFrozenCategories);
        }
        for (index, category) in frozen_categories.iter().enumerate() {
            if !is_frozen(category.record.state()) {
                return Err(ClosureError::FrozenCategoryRequired { index });
            }
        }
        Ok(Self {
            temperature_k,
            rain,
            frozen_categories,
            topology: MixtureTopology::HomogeneousMixedPhase,
        })
    }

    #[must_use]
    pub const fn with_topology(mut self, topology: MixtureTopology) -> Self {
        self.topology = topology;
        self
    }

    #[must_use]
    pub const fn temperature_k(&self) -> f64 {
        self.temperature_k
    }

    #[must_use]
    pub const fn rain(&self) -> &ClosedParticleCategory {
        &self.rain
    }

    #[must_use]
    pub fn frozen_categories(&self) -> &[ClosedParticleCategory] {
        &self.frozen_categories
    }

    #[must_use]
    pub const fn topology(&self) -> MixtureTopology {
        self.topology
    }

    pub fn diagnose(&self) -> Result<DiagnosticCoexistenceResult, ClosureError> {
        diagnose_coexistence(self)
    }
}

/// Gaussian canting transition when both dry endpoints are Gaussian. If an
/// endpoint is isotropic, `effective_gaussian` is `None` and the two explicit
/// endpoint models plus liquid weight remain available instead of fabricating
/// a Gaussian surrogate.
#[derive(Clone, Debug, PartialEq)]
pub struct DiagnosticCantingTransition {
    frozen: ClosedOrientation,
    rain: ClosedOrientation,
    liquid_weight: f64,
    effective_gaussian: Option<(f64, f64, u16)>,
    provenance: PropertyProvenance,
}

impl DiagnosticCantingTransition {
    #[must_use]
    pub const fn frozen(&self) -> &ClosedOrientation {
        &self.frozen
    }

    #[must_use]
    pub const fn rain(&self) -> &ClosedOrientation {
        &self.rain
    }

    #[must_use]
    pub const fn liquid_weight(&self) -> f64 {
        self.liquid_weight
    }

    #[must_use]
    pub const fn effective_gaussian(&self) -> Option<(f64, f64, u16)> {
        self.effective_gaussian
    }

    #[must_use]
    pub const fn provenance(&self) -> &PropertyProvenance {
        &self.provenance
    }
}

/// One frozen category after proportional pairing with rainwater.
#[derive(Clone, Debug, PartialEq)]
pub struct DiagnosticWetCategory {
    source_category: ClosedParticleCategory,
    frozen_mass_kgkg: f64,
    paired_liquid_mass_kgkg: f64,
    wet_total_mass_kgkg: f64,
    wet_fraction: f64,
    frozen_category_fraction: f64,
    wet_category_fraction: f64,
    effective_density_kg_m3: SourcedScalar,
    minor_to_major_axis_ratio: SourcedScalar,
    fall_speed_m_s: SourcedScalar,
    canting: DiagnosticCantingTransition,
    mixture: MixtureMetadata,
}

impl DiagnosticWetCategory {
    #[must_use]
    pub const fn source_category(&self) -> &ClosedParticleCategory {
        &self.source_category
    }

    #[must_use]
    pub const fn frozen_mass_kgkg(&self) -> f64 {
        self.frozen_mass_kgkg
    }

    #[must_use]
    pub const fn paired_liquid_mass_kgkg(&self) -> f64 {
        self.paired_liquid_mass_kgkg
    }

    #[must_use]
    pub const fn wet_total_mass_kgkg(&self) -> f64 {
        self.wet_total_mass_kgkg
    }

    #[must_use]
    pub const fn wet_fraction(&self) -> f64 {
        self.wet_fraction
    }

    #[must_use]
    pub const fn frozen_category_fraction(&self) -> f64 {
        self.frozen_category_fraction
    }

    #[must_use]
    pub const fn wet_category_fraction(&self) -> f64 {
        self.wet_category_fraction
    }

    #[must_use]
    pub const fn effective_density_kg_m3(&self) -> &SourcedScalar {
        &self.effective_density_kg_m3
    }

    #[must_use]
    pub const fn minor_to_major_axis_ratio(&self) -> &SourcedScalar {
        &self.minor_to_major_axis_ratio
    }

    #[must_use]
    pub const fn fall_speed_m_s(&self) -> &SourcedScalar {
        &self.fall_speed_m_s
    }

    #[must_use]
    pub const fn canting(&self) -> &DiagnosticCantingTransition {
        &self.canting
    }

    #[must_use]
    pub const fn mixture(&self) -> MixtureMetadata {
        self.mixture
    }
}

/// Mass-accounted result of `DiagnosticCoexistenceV1`.
#[derive(Clone, Debug, PartialEq)]
pub struct DiagnosticCoexistenceResult {
    input_rain_mass_kgkg: f64,
    input_frozen_mass_kgkg: f64,
    target_wet_fraction: f64,
    paired_liquid_mass_kgkg: f64,
    unused_rain_mass_kgkg: f64,
    wet_categories: Vec<DiagnosticWetCategory>,
    mixture: MixtureMetadata,
}

impl DiagnosticCoexistenceResult {
    /// This closure is intentionally never represented as scheme-native.
    #[must_use]
    pub const fn is_scheme_native(&self) -> bool {
        false
    }

    #[must_use]
    pub const fn model_identifier(&self) -> &'static str {
        "DiagnosticCoexistenceV1"
    }

    #[must_use]
    pub const fn input_rain_mass_kgkg(&self) -> f64 {
        self.input_rain_mass_kgkg
    }

    #[must_use]
    pub const fn input_frozen_mass_kgkg(&self) -> f64 {
        self.input_frozen_mass_kgkg
    }

    #[must_use]
    pub const fn input_total_mass_kgkg(&self) -> f64 {
        self.input_rain_mass_kgkg + self.input_frozen_mass_kgkg
    }

    #[must_use]
    pub const fn target_wet_fraction(&self) -> f64 {
        self.target_wet_fraction
    }

    #[must_use]
    pub const fn paired_liquid_mass_kgkg(&self) -> f64 {
        self.paired_liquid_mass_kgkg
    }

    /// Actual diagnosed liquid fraction after limiting the temperature target
    /// by the rain mass that is available for pairing.
    #[must_use]
    pub fn wet_fraction(&self) -> f64 {
        self.paired_liquid_mass_kgkg / (self.input_frozen_mass_kgkg + self.paired_liquid_mass_kgkg)
    }

    #[must_use]
    pub const fn unused_rain_mass_kgkg(&self) -> f64 {
        self.unused_rain_mass_kgkg
    }

    #[must_use]
    pub fn wet_categories(&self) -> &[DiagnosticWetCategory] {
        &self.wet_categories
    }

    #[must_use]
    pub fn output_total_mass_kgkg(&self) -> f64 {
        self.unused_rain_mass_kgkg
            + self
                .wet_categories
                .iter()
                .map(DiagnosticWetCategory::wet_total_mass_kgkg)
                .sum::<f64>()
    }

    #[must_use]
    pub const fn mixture(&self) -> MixtureMetadata {
        self.mixture
    }
}

/// Diagnose wet frozen categories without evaluating a LUT or scattering
/// amplitude. Rainwater paired to a frozen category is removed from the
/// separate rain remainder exactly once.
pub fn diagnose_coexistence(
    input: &DiagnosticCoexistenceInput,
) -> Result<DiagnosticCoexistenceResult, ClosureError> {
    let rain_mass = input.rain.mixing_ratio_kgkg();
    positive("coexisting rain mixing ratio", rain_mass)?;
    let frozen_mass = input
        .frozen_categories
        .iter()
        .map(ClosedParticleCategory::mixing_ratio_kgkg)
        .sum::<f64>();
    positive("coexisting frozen mixing ratio", frozen_mass)?;
    let target_wet_fraction = ((input.temperature_k - DIAGNOSTIC_COEXISTENCE_COLD_K)
        / (DIAGNOSTIC_COEXISTENCE_WARM_K - DIAGNOSTIC_COEXISTENCE_COLD_K))
        .clamp(0.0, 1.0);
    let liquid_needed = if target_wet_fraction >= 1.0 {
        f64::INFINITY
    } else {
        frozen_mass * target_wet_fraction / (1.0 - target_wet_fraction)
    };
    let paired_liquid = rain_mass.min(liquid_needed);
    let unused_rain = rain_mass - paired_liquid;
    nonnegative("unused rain mixing ratio", unused_rain)?;
    let wet_total = frozen_mass + paired_liquid;
    let wet_fraction = paired_liquid / wet_total;
    fraction("diagnosed wet fraction", wet_fraction)?;
    let rain_shape = input.rain.shape();
    let mixture = MixtureMetadata {
        topology: input.topology,
        scattering_status: MixtureScatteringStatus::NotEvaluatedNoLutOrAmplitude,
    };

    let mut wet_categories = Vec::with_capacity(input.frozen_categories.len());
    let mut assigned_liquid = 0.0;
    for (index, category) in input.frozen_categories.iter().enumerate() {
        let category_frozen_mass = category.mixing_ratio_kgkg();
        let frozen_fraction = category_frozen_mass / frozen_mass;
        let category_liquid = if index + 1 == input.frozen_categories.len() {
            // Assign the residual to the final category so binary rounding
            // cannot duplicate or lose rainwater across categories.
            paired_liquid - assigned_liquid
        } else {
            paired_liquid * frozen_fraction
        };
        assigned_liquid += category_liquid;
        let category_total = category_frozen_mass + category_liquid;
        let category_wet_fraction = category_liquid / category_total;
        fraction("category wet fraction", category_wet_fraction)?;
        let category_wet_share = category_total / wet_total;
        let frozen_shape = category.shape();
        let transition_source = PropertyProvenance::new(
            PropertySourceKind::DocumentedClosure,
            vec![],
            "DiagnosticCoexistenceV1 linear property transition by diagnosed wet mass fraction",
        );
        let density = lerp(
            frozen_shape.bulk_density_kg_m3(),
            rain_shape.bulk_density_kg_m3(),
            category_wet_fraction,
        );
        let axis_ratio = lerp(
            frozen_shape.minor_to_major_axis_ratio(),
            rain_shape.minor_to_major_axis_ratio(),
            category_wet_fraction,
        );
        let fall_speed = lerp(
            category.fall_speed_m_s.value,
            input.rain.fall_speed_m_s.value,
            category_wet_fraction,
        );
        physical_density(
            "diagnostic wet-particle density",
            density,
            WATER_DENSITY_KG_M3,
        )?;
        axis_ratio_value("diagnostic wet-particle axis ratio", axis_ratio)?;
        positive("diagnostic wet-particle fall speed", fall_speed)?;

        let frozen_gaussian = category.orientation.gaussian_parameters();
        let rain_gaussian = input.rain.orientation.gaussian_parameters();
        let effective_gaussian = match (frozen_gaussian, rain_gaussian) {
            (Some((f_mean, f_sigma, f_points)), Some((r_mean, r_sigma, r_points))) => Some((
                lerp(f_mean, r_mean, category_wet_fraction),
                lerp(f_sigma, r_sigma, category_wet_fraction),
                f_points.max(r_points),
            )),
            _ => None,
        };
        let canting = DiagnosticCantingTransition {
            frozen: category.orientation.clone(),
            rain: input.rain.orientation.clone(),
            liquid_weight: category_wet_fraction,
            effective_gaussian,
            provenance: transition_source.clone(),
        };

        wet_categories.push(DiagnosticWetCategory {
            source_category: category.clone(),
            frozen_mass_kgkg: category_frozen_mass,
            paired_liquid_mass_kgkg: category_liquid,
            wet_total_mass_kgkg: category_total,
            wet_fraction: category_wet_fraction,
            frozen_category_fraction: frozen_fraction,
            wet_category_fraction: category_wet_share,
            effective_density_kg_m3: SourcedScalar::new(density, transition_source.clone()),
            minor_to_major_axis_ratio: SourcedScalar::new(axis_ratio, transition_source.clone()),
            fall_speed_m_s: SourcedScalar::new(fall_speed, transition_source),
            canting,
            mixture,
        });
    }

    Ok(DiagnosticCoexistenceResult {
        input_rain_mass_kgkg: rain_mass,
        input_frozen_mass_kgkg: frozen_mass,
        target_wet_fraction,
        paired_liquid_mass_kgkg: paired_liquid,
        unused_rain_mass_kgkg: unused_rain,
        wet_categories,
        mixture,
    })
}

#[derive(Clone, Copy)]
struct P3VariableNames {
    qice: &'static str,
    qnice: &'static str,
    qir: &'static str,
    qib: &'static str,
}

fn p3_names(category: P3Category) -> P3VariableNames {
    match category {
        P3Category::Category1 => P3VariableNames {
            qice: "QICE",
            qnice: "QNICE",
            qir: "QIR",
            qib: "QIB",
        },
        P3Category::Category2 => P3VariableNames {
            qice: "QICE2",
            qnice: "QNICE2",
            qir: "QIR2",
            qib: "QIB2",
        },
    }
}

fn close_orientation(
    definition: OrientationDefinition,
    scheme_default_sigma_deg: f64,
    scheme_default_method: &'static str,
) -> ClosedOrientation {
    match definition {
        OrientationDefinition::SchemeDefault => ClosedOrientation {
            definition,
            model: OrientationModel::GaussianCanting {
                mean_deg: 0.0,
                standard_deviation_deg: scheme_default_sigma_deg,
                quadrature_points: DEFAULT_GAUSSIAN_QUADRATURE_POINTS,
            },
            provenance: PropertyProvenance::new(
                PropertySourceKind::Assumed,
                vec![],
                scheme_default_method,
            ),
        },
        OrientationDefinition::Aligned => ClosedOrientation {
            definition,
            model: OrientationModel::GaussianCanting {
                mean_deg: 0.0,
                standard_deviation_deg: 0.0,
                quadrature_points: 1,
            },
            provenance: PropertyProvenance::new(
                PropertySourceKind::Assumed,
                vec![],
                "explicit aligned Gaussian canting override",
            ),
        },
        OrientationDefinition::Isotropic => ClosedOrientation {
            definition,
            model: OrientationModel::Isotropic {
                quadrature_points: ISOTROPIC_QUADRATURE_POINTS,
            },
            provenance: PropertyProvenance::new(
                PropertySourceKind::Assumed,
                vec![],
                "explicit isotropic orientation override; not a Gaussian surrogate",
            ),
        },
    }
}

fn mass_equivalent_diameter(
    mixing_ratio_kgkg: f64,
    number_per_kg: f64,
    density_kg_m3: f64,
) -> Result<f64, ClosureError> {
    positive("mass-equivalent mixing ratio", mixing_ratio_kgkg)?;
    positive("mass-equivalent number concentration", number_per_kg)?;
    positive("mass-equivalent density", density_kg_m3)?;
    let value = (6.0 * mixing_ratio_kgkg / (PI * density_kg_m3 * number_per_kg)).cbrt();
    positive("mass-equivalent diameter", value)?;
    Ok(value)
}

fn analytic_fall_speed(
    diameter_m: f64,
    density_kg_m3: f64,
    axis_ratio: f64,
    air_density_kg_m3: f64,
) -> Result<f64, ClosureError> {
    positive("fall-speed diameter", diameter_m)?;
    positive("fall-speed density", density_kg_m3)?;
    axis_ratio_value("fall-speed axis ratio", axis_ratio)?;
    positive("fall-speed air density", air_density_kg_m3)?;
    // This bounded-scope relation is a deterministic aerodynamic proxy, not
    // the P3/ISHMAEL internal lookup table: 3 m/s at D=1 mm, rho=400 kg/m3,
    // aspect=1, and standard air density, with square-root similarity scaling.
    let value = 3.0
        * (diameter_m / 1.0e-3).sqrt()
        * (density_kg_m3 / 400.0).sqrt()
        * axis_ratio.sqrt()
        * (REFERENCE_AIR_DENSITY_KG_M3 / air_density_kg_m3).sqrt();
    positive("analytic positive-downward fall speed", value)?;
    Ok(value)
}

fn conventional_default_density(category: ConventionalHydrometeor) -> f64 {
    match category {
        ConventionalHydrometeor::CloudWater | ConventionalHydrometeor::Rain => WATER_DENSITY_KG_M3,
        ConventionalHydrometeor::CloudIce => ICE_MATERIAL_DENSITY_KG_M3,
        ConventionalHydrometeor::Snow => 100.0,
        ConventionalHydrometeor::Graupel => 400.0,
        ConventionalHydrometeor::Hail => 900.0,
    }
}

fn conventional_default_axis_ratio(category: ConventionalHydrometeor, diameter_m: f64) -> f64 {
    match category {
        ConventionalHydrometeor::CloudWater => 1.0,
        ConventionalHydrometeor::Rain => {
            // Documented closure bound for an oblate-drop proxy.
            (1.0 - 0.062 * diameter_m.mul_add(1_000.0, 0.0)).clamp(0.5, 1.0)
        }
        ConventionalHydrometeor::CloudIce => 0.6,
        ConventionalHydrometeor::Snow => 0.7,
        ConventionalHydrometeor::Graupel => 0.9,
        ConventionalHydrometeor::Hail => 0.95,
    }
}

fn conventional_default_canting(category: ConventionalHydrometeor) -> f64 {
    match category {
        ConventionalHydrometeor::CloudWater | ConventionalHydrometeor::Rain => 0.0,
        ConventionalHydrometeor::CloudIce => 10.0,
        ConventionalHydrometeor::Snow
        | ConventionalHydrometeor::Graupel
        | ConventionalHydrometeor::Hail => 40.0,
    }
}

fn conventional_mass_name(category: ConventionalHydrometeor) -> &'static str {
    match category {
        ConventionalHydrometeor::CloudWater => "QCLOUD",
        ConventionalHydrometeor::Rain => "QRAIN",
        ConventionalHydrometeor::CloudIce => "QICE",
        ConventionalHydrometeor::Snow => "QSNOW",
        ConventionalHydrometeor::Graupel => "QGRAUP",
        ConventionalHydrometeor::Hail => "QHAIL",
    }
}

fn conventional_number_name(category: ConventionalHydrometeor) -> &'static str {
    match category {
        ConventionalHydrometeor::CloudWater => "QNCLOUD",
        ConventionalHydrometeor::Rain => "QNRAIN",
        ConventionalHydrometeor::CloudIce => "QNICE",
        ConventionalHydrometeor::Snow => "QNSNOW",
        ConventionalHydrometeor::Graupel => "QNGRAUPEL",
        ConventionalHydrometeor::Hail => "QNHAIL",
    }
}

fn required(value: Option<f64>, field: &'static str) -> Result<f64, ClosureError> {
    value.ok_or(ClosureError::MissingInput { field })
}

fn finite_result(field: &'static str, value: f64) -> Result<(), ClosureError> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(ClosureError::NonFinite { field, value })
    }
}

fn positive(field: &'static str, value: f64) -> Result<(), ClosureError> {
    finite_result(field, value)?;
    if value > 0.0 {
        Ok(())
    } else {
        Err(ClosureError::OutOfRange { field, value })
    }
}

fn nonnegative(field: &'static str, value: f64) -> Result<(), ClosureError> {
    finite_result(field, value)?;
    if value >= 0.0 {
        Ok(())
    } else {
        Err(ClosureError::OutOfRange { field, value })
    }
}

fn fraction(field: &'static str, value: f64) -> Result<(), ClosureError> {
    finite_result(field, value)?;
    if (0.0..=1.0).contains(&value) {
        Ok(())
    } else {
        Err(ClosureError::OutOfRange { field, value })
    }
}

fn axis_ratio_value(field: &'static str, value: f64) -> Result<(), ClosureError> {
    fraction(field, value)?;
    if value > 0.0 {
        Ok(())
    } else {
        Err(ClosureError::OutOfRange { field, value })
    }
}

fn physical_density(field: &'static str, value: f64, maximum: f64) -> Result<(), ClosureError> {
    positive(field, value)?;
    if value <= maximum {
        Ok(())
    } else {
        Err(ClosureError::DensityAboveMaterialLimit {
            field,
            value,
            maximum,
        })
    }
}

fn checked_ratio(
    field: &'static str,
    numerator: f64,
    denominator: f64,
) -> Result<f64, ClosureError> {
    positive("ratio denominator", denominator)?;
    let value = numerator / denominator;
    finite_result(field, value)?;
    Ok(value)
}

fn lerp(frozen: f64, liquid: f64, liquid_weight: f64) -> f64 {
    frozen + liquid_weight * (liquid - frozen)
}

fn state_mass(state: ParticleState) -> f64 {
    match state {
        ParticleState::Conventional(value) => value.mixing_ratio_kgkg(),
        ParticleState::P3(value) => value.total_ice_mixing_ratio_kgkg(),
        ParticleState::Ishmael(value) => value.mixing_ratio_kgkg(),
    }
}

fn state_shape(state: ParticleState) -> ParticleShape {
    match state {
        ParticleState::Conventional(value) => value.shape(),
        ParticleState::P3(value) => value.shape(),
        ParticleState::Ishmael(value) => value.shape(),
    }
}

fn is_frozen(state: ParticleState) -> bool {
    match state {
        ParticleState::P3(_) | ParticleState::Ishmael(_) => true,
        ParticleState::Conventional(value) => matches!(
            value.category(),
            ConventionalHydrometeor::CloudIce
                | ConventionalHydrometeor::Snow
                | ConventionalHydrometeor::Graupel
                | ConventionalHydrometeor::Hail
        ),
    }
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum ClosureError {
    #[error("WRF microphysics scheme id must be positive, got {value}")]
    InvalidSchemeId { value: i32 },
    #[error("WRF mp_physics={wrf_mp_physics} is incompatible with the {requested} closure")]
    SchemeFamilyMismatch {
        wrf_mp_physics: i32,
        requested: &'static str,
    },
    #[error("{category} is unavailable for WRF mp_physics={wrf_mp_physics}")]
    CategoryUnavailable {
        wrf_mp_physics: i32,
        category: &'static str,
    },
    #[error("required raw WRF scalar {field} is missing")]
    MissingInput { field: &'static str },
    #[error("raw WRF scalar {field} is unexpected for mp_physics={wrf_mp_physics}")]
    UnexpectedInput {
        field: &'static str,
        wrf_mp_physics: i32,
    },
    #[error("{field} must be finite, got {value}")]
    NonFinite { field: &'static str, value: f64 },
    #[error("{field} is outside its valid physical range: {value}")]
    OutOfRange { field: &'static str, value: f64 },
    #[error("{field} density {value} exceeds material-density limit {maximum}")]
    DensityAboveMaterialLimit {
        field: &'static str,
        value: f64,
        maximum: f64,
    },
    #[error("inconsistent raw WRF inputs: {relation}; got {left} and {right}")]
    InconsistentInputs {
        relation: &'static str,
        left: f64,
        right: f64,
    },
    #[error("positive {volume_field} is required when {mass_field} is positive")]
    MissingPositiveVolume {
        mass_field: &'static str,
        volume_field: &'static str,
    },
    #[error(
        "temperature {temperature_k} K is outside DiagnosticCoexistenceV1 envelope [{cold_k}, {warm_k}] K"
    )]
    OutsideCoexistenceEnvelope {
        temperature_k: f64,
        cold_k: f64,
        warm_k: f64,
    },
    #[error("DiagnosticCoexistenceV1 requires a conventional rain category")]
    RainCategoryRequired,
    #[error("DiagnosticCoexistenceV1 requires at least one frozen category")]
    NoFrozenCategories,
    #[error("DiagnosticCoexistenceV1 input category {index} is not frozen")]
    FrozenCategoryRequired { index: usize },
    #[error(transparent)]
    Particle(#[from] ParticleError),
    #[error(transparent)]
    Provenance(#[from] ProvenanceError),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(scheme: i32) -> ClosureContext {
        ClosureContext::new(scheme, 271.65, 1.0).unwrap()
    }

    fn p3_input(category: P3Category, qice: f64) -> P3CategoryInput {
        let qir = 0.4 * qice;
        P3CategoryInput::new(category, qice, 1.0e6, qir, qir / 400.0)
    }

    fn ishmael_input(category: IshmaelIceCategory) -> IshmaelCategoryInput {
        IshmaelCategoryInput::new(category, 1.0e-4, 1.0e6, 2.0e-7, 1.4e-7)
    }

    fn assert_close(actual: f64, expected: f64) {
        let scale = expected.abs().max(1.0);
        assert!(
            (actual - expected).abs() <= 1.0e-12 * scale,
            "{actual} != {expected}"
        );
    }

    #[test]
    fn p3_50_and_51_close_native_qice_qnice_qir_qib() {
        for scheme in [50, 51] {
            let closed =
                close_p3_category(&context(scheme), &p3_input(P3Category::Category1, 1.0e-4))
                    .unwrap();
            let ParticleState::P3(state) = closed.record().state() else {
                panic!("expected P3 state");
            };
            assert_close(state.total_ice_mixing_ratio_kgkg(), 1.0e-4);
            assert_close(state.total_ice_number_per_kg(), 1.0e6);
            assert_close(state.rime_mass_fraction(), 0.4);
            assert_close(state.rime_density_kg_m3(), 400.0);
            assert_eq!(
                closed.rime_mass_fraction().unwrap().provenance().kind(),
                PropertySourceKind::NativePrognostic
            );
            assert_eq!(
                closed.effective_density_kg_m3().provenance().kind(),
                PropertySourceKind::DocumentedClosure
            );
            assert!(closed.sixth_moment_m6().is_none());
        }
    }

    #[test]
    fn p3_52_retains_both_native_categories_and_category2_field_names() {
        let first =
            close_p3_category(&context(52), &p3_input(P3Category::Category1, 1.0e-4)).unwrap();
        let second =
            close_p3_category(&context(52), &p3_input(P3Category::Category2, 3.0e-4)).unwrap();
        assert_close(first.mixing_ratio_kgkg(), 1.0e-4);
        assert_close(second.mixing_ratio_kgkg(), 3.0e-4);
        assert_eq!(
            second
                .rime_mass_fraction()
                .unwrap()
                .provenance()
                .source_variables(),
            &["QIR2", "QICE2"]
        );
        let ParticleProvenance::P3(provenance) = second.record().provenance() else {
            panic!("expected P3 provenance");
        };
        assert!(
            provenance
                .source_variables()
                .iter()
                .any(|value| value.name() == "QICE2")
        );
    }

    #[test]
    fn p3_53_recovers_qzi_sixth_moment_exactly() {
        let qnice: f64 = 1.0e6;
        let expected_m6: f64 = 6.4e-17;
        let qzi = (expected_m6 * qnice).sqrt();
        let input = P3CategoryInput::category1(1.0e-4, qnice, 4.0e-5, 1.0e-7).with_qzi(qzi);
        let closed = close_p3_category(&context(53), &input).unwrap();
        let m6 = closed.sixth_moment_m6().unwrap();
        assert_close(m6.value(), qzi * qzi / qnice);
        assert_close(m6.value(), expected_m6);
        assert_eq!(m6.provenance().kind(), PropertySourceKind::NativePrognostic);
        assert_eq!(m6.provenance().source_variables(), &["QZI", "QNICE"]);
        assert_close(closed.characteristic_diameter_m().value(), 2.0e-4);
    }

    #[test]
    fn p3_rejects_missing_nonfinite_and_inconsistent_native_state() {
        let missing = P3CategoryInput::from_optional(
            P3Category::Category1,
            None,
            Some(1.0e6),
            Some(1.0e-5),
            Some(2.5e-8),
            None,
        );
        assert_eq!(
            close_p3_category(&context(50), &missing).unwrap_err(),
            ClosureError::MissingInput { field: "QICE" }
        );

        let too_much_rime = P3CategoryInput::category1(1.0e-4, 1.0e6, 1.1e-4, 2.75e-7);
        assert!(matches!(
            close_p3_category(&context(50), &too_much_rime),
            Err(ClosureError::InconsistentInputs { .. })
        ));

        let no_rime_volume = P3CategoryInput::category1(1.0e-4, 1.0e6, 1.0e-5, 0.0);
        assert!(matches!(
            close_p3_category(&context(50), &no_rime_volume),
            Err(ClosureError::MissingPositiveVolume { .. })
        ));

        let missing_qzi = p3_input(P3Category::Category1, 1.0e-4);
        assert_eq!(
            close_p3_category(&context(53), &missing_qzi).unwrap_err(),
            ClosureError::MissingInput { field: "QZI" }
        );
        let nonfinite_qzi = missing_qzi.with_qzi(f64::NAN);
        assert!(matches!(
            close_p3_category(&context(53), &nonfinite_qzi),
            Err(ClosureError::NonFinite { field: "QZI", .. })
        ));
        let unexpected_qzi = missing_qzi.with_qzi(1.0e-8);
        assert!(matches!(
            close_p3_category(&context(50), &unexpected_qzi),
            Err(ClosureError::UnexpectedInput { field: "QZI", .. })
        ));
        assert!(matches!(
            close_p3_category(&context(51), &p3_input(P3Category::Category2, 1.0e-4)),
            Err(ClosureError::CategoryUnavailable { .. })
        ));
    }

    #[test]
    fn exactly_unrimed_p3_state_is_valid_and_explicit_about_density_sentinel() {
        let input = P3CategoryInput::category1(1.0e-4, 1.0e6, 0.0, 0.0);
        let closed = close_p3_category(&context(50), &input).unwrap();
        assert_close(closed.rime_mass_fraction().unwrap().value(), 0.0);
        assert_close(
            closed.rime_density_kg_m3().unwrap().value(),
            ICE_MATERIAL_DENSITY_KG_M3,
        );
        assert_eq!(
            closed.rime_density_kg_m3().unwrap().provenance().kind(),
            PropertySourceKind::Assumed
        );
    }

    #[test]
    fn all_five_ishmael_physical_states_retain_category_and_closed_properties() {
        let categories = [
            IshmaelIceCategory::SmallIce,
            IshmaelIceCategory::Planar,
            IshmaelIceCategory::Columnar,
            IshmaelIceCategory::Aggregate,
            IshmaelIceCategory::Rimed,
        ];
        for category in categories {
            let closed = close_ishmael_category(&context(55), &ishmael_input(category)).unwrap();
            let ParticleState::Ishmael(state) = closed.record().state() else {
                panic!("expected ISHMAEL state");
            };
            assert_eq!(state.category(), category);
            assert_close(state.mixing_ratio_kgkg(), 1.0e-4);
            assert_close(state.number_per_kg(), 1.0e6);
            assert_close(closed.effective_density_kg_m3().value(), 500.0);
            assert_close(closed.minor_to_major_axis_ratio().value(), 0.7);
            assert_close(
                state.rime_mass_fraction(),
                if category == IshmaelIceCategory::Rimed {
                    1.0
                } else {
                    0.0
                },
            );
        }
    }

    #[test]
    fn ishmael_diagnostics_take_precedence_field_by_field() {
        let diagnostics = IshmaelDiagnostics::new(Some(0.002), Some(300.0), Some(0.45), Some(2.5));
        let input = ishmael_input(IshmaelIceCategory::Aggregate).with_diagnostics(diagnostics);
        let closed = close_ishmael_category(&context(55), &input).unwrap();
        for (property, expected, variable) in [
            (closed.characteristic_diameter_m(), 0.002, "D_ICE"),
            (closed.effective_density_kg_m3(), 300.0, "RHO_ICE"),
            (closed.minor_to_major_axis_ratio(), 0.45, "PHI_ICE"),
            (closed.fall_speed_m_s(), 2.5, "V_ICE"),
        ] {
            assert_close(property.value(), expected);
            assert_eq!(
                property.provenance().kind(),
                PropertySourceKind::WrfDiagnostic
            );
            assert_eq!(property.provenance().source_variables(), &[variable]);
        }
    }

    #[test]
    fn ishmael_exact_tuple_names_flow_through_all_property_provenance() {
        for (category, suffix) in [
            (IshmaelIceCategory::Planar, ""),
            (IshmaelIceCategory::Columnar, "2"),
            (IshmaelIceCategory::Aggregate, "3"),
        ] {
            let (qice, qnice, qvoli, qaoli, d_ice, rho_ice, phi_ice, v_ice) = match suffix {
                "" => (
                    "QICE", "QNICE", "QVOLI", "QAOLI", "d_ice", "rho_ice", "phi_ice", "v_ice",
                ),
                "2" => (
                    "QICE2", "QNICE2", "QVOLI2", "QAOLI2", "d_ice2", "rho_ice2", "phi_ice2",
                    "v_ice2",
                ),
                "3" => (
                    "QICE3", "QNICE3", "QVOLI3", "QAOLI3", "d_ice3", "rho_ice3", "phi_ice3",
                    "v_ice3",
                ),
                _ => unreachable!(),
            };
            let input = ishmael_input(category)
                .with_diagnostics(IshmaelDiagnostics::new(
                    Some(0.002),
                    Some(300.0),
                    Some(0.45),
                    Some(2.5),
                ))
                .with_source_fields(IshmaelSourceFields::new(
                    qice, qnice, qvoli, qaoli, d_ice, rho_ice, phi_ice, v_ice,
                ));
            let closed = close_ishmael_category(&context(55), &input).unwrap();
            assert_eq!(
                closed
                    .characteristic_diameter_m()
                    .provenance()
                    .source_variables(),
                &[d_ice]
            );
            assert_eq!(
                closed
                    .effective_density_kg_m3()
                    .provenance()
                    .source_variables(),
                &[rho_ice]
            );
            assert_eq!(
                closed
                    .minor_to_major_axis_ratio()
                    .provenance()
                    .source_variables(),
                &[phi_ice]
            );
            assert_eq!(
                closed.fall_speed_m_s().provenance().source_variables(),
                &[v_ice]
            );
            let ParticleProvenance::Ishmael(record_provenance) = closed.record().provenance()
            else {
                panic!("expected ISHMAEL record provenance");
            };
            let names = record_provenance
                .source_variables()
                .iter()
                .map(SourceVariable::name)
                .collect::<Vec<_>>();
            assert_eq!(
                names,
                vec![qice, qnice, qvoli, qaoli, d_ice, rho_ice, phi_ice, v_ice]
            );
        }
    }

    #[test]
    fn ishmael_fallbacks_are_labeled_without_calling_qaoli_metric_phi_ice() {
        let closed =
            close_ishmael_category(&context(55), &ishmael_input(IshmaelIceCategory::Planar))
                .unwrap();
        assert_eq!(
            closed.effective_density_kg_m3().provenance().kind(),
            PropertySourceKind::NativePrognostic
        );
        let aspect_source = closed.minor_to_major_axis_ratio().provenance();
        assert_eq!(aspect_source.kind(), PropertySourceKind::DocumentedClosure);
        assert!(aspect_source.method().contains("not PHI_ICE"));
        assert_eq!(aspect_source.source_variables(), &["QAOLI", "QVOLI"]);
        assert_eq!(
            closed.fall_speed_m_s().provenance().kind(),
            PropertySourceKind::DocumentedClosure
        );
    }

    #[test]
    fn invalid_present_ishmael_diagnostic_never_falls_back() {
        let input = ishmael_input(IshmaelIceCategory::Aggregate)
            .with_diagnostics(IshmaelDiagnostics::new(None, Some(-1.0), None, None));
        assert!(matches!(
            close_ishmael_category(&context(55), &input),
            Err(ClosureError::OutOfRange {
                field: "RHO_ICE",
                ..
            })
        ));

        let invalid_metric =
            IshmaelCategoryInput::new(IshmaelIceCategory::Aggregate, 1.0e-4, 1.0e6, 2.0e-7, 3.0e-7);
        assert!(matches!(
            close_ishmael_category(&context(55), &invalid_metric),
            Err(ClosureError::OutOfRange {
                field: "ISHMAEL QAOLI/QVOLI metric",
                ..
            })
        ));

        let missing = IshmaelCategoryInput::from_optional(
            IshmaelIceCategory::SmallIce,
            Some(1.0e-4),
            Some(1.0e6),
            None,
            Some(1.0e-7),
            IshmaelDiagnostics::default(),
        );
        assert_eq!(
            close_ishmael_category(&context(55), &missing).unwrap_err(),
            ClosureError::MissingInput { field: "QVOLI" }
        );
    }

    #[test]
    fn orientation_defaults_and_explicit_overrides_are_distinct() {
        let less_rimed = close_p3_category(
            &context(50),
            &P3CategoryInput::category1(1.0e-4, 1.0e6, 0.0, 0.0),
        )
        .unwrap();
        assert_eq!(
            less_rimed.orientation().gaussian_parameters(),
            Some((0.0, 10.0, DEFAULT_GAUSSIAN_QUADRATURE_POINTS))
        );
        let fully_rimed = close_p3_category(
            &context(50),
            &P3CategoryInput::category1(1.0e-4, 1.0e6, 1.0e-4, 2.5e-7),
        )
        .unwrap();
        assert_eq!(
            fully_rimed.orientation().gaussian_parameters(),
            Some((0.0, 40.0, DEFAULT_GAUSSIAN_QUADRATURE_POINTS))
        );

        for (category, sigma) in [
            (IshmaelIceCategory::Planar, 10.0),
            (IshmaelIceCategory::Columnar, 10.0),
            (IshmaelIceCategory::Aggregate, 40.0),
        ] {
            let closed = close_ishmael_category(&context(55), &ishmael_input(category)).unwrap();
            assert_eq!(
                closed.orientation().gaussian_parameters(),
                Some((0.0, sigma, DEFAULT_GAUSSIAN_QUADRATURE_POINTS))
            );
            assert_eq!(
                closed.orientation().provenance().kind(),
                PropertySourceKind::Assumed
            );
        }

        let aligned_context = context(55).with_orientation(OrientationDefinition::Aligned);
        let aligned = close_ishmael_category(
            &aligned_context,
            &ishmael_input(IshmaelIceCategory::Aggregate),
        )
        .unwrap();
        assert_eq!(
            aligned.orientation().gaussian_parameters(),
            Some((0.0, 0.0, 1))
        );

        let isotropic_context = context(55).with_orientation(OrientationDefinition::Isotropic);
        let isotropic = close_ishmael_category(
            &isotropic_context,
            &ishmael_input(IshmaelIceCategory::Aggregate),
        )
        .unwrap();
        assert!(matches!(
            isotropic.orientation().model(),
            OrientationModel::Isotropic {
                quadrature_points: ISOTROPIC_QUADRATURE_POINTS
            }
        ));
        assert!(isotropic.orientation().gaussian_parameters().is_none());
    }

    #[test]
    fn conventional_closure_requires_number_or_direct_size() {
        let input = ConventionalCategoryInput::new(ConventionalHydrometeor::Rain, 1.0e-4, None);
        assert_eq!(
            close_conventional_category(&context(10), &input).unwrap_err(),
            ClosureError::MissingInput {
                field: "number concentration or D_HYDROMETEOR"
            }
        );
        let diagnosed = input.with_characteristic_diameter_m(1.0e-3);
        let closed = close_conventional_category(&context(10), &diagnosed).unwrap();
        assert_close(closed.characteristic_diameter_m().value(), 1.0e-3);
        assert_eq!(
            closed.characteristic_diameter_m().provenance().kind(),
            PropertySourceKind::WrfDiagnostic
        );
    }

    fn coexistence_categories() -> (ClosedParticleCategory, Vec<ClosedParticleCategory>) {
        let rain = close_conventional_category(
            &context(10),
            &ConventionalCategoryInput::new(ConventionalHydrometeor::Rain, 8.0e-4, Some(1.0e6)),
        )
        .unwrap();
        let frozen_context = context(52);
        let first = close_p3_category(
            &frozen_context,
            &P3CategoryInput::category1(1.0e-4, 1.0e6, 2.0e-5, 5.0e-8),
        )
        .unwrap();
        let second = close_p3_category(
            &frozen_context,
            &P3CategoryInput::category2(3.0e-4, 1.0e6, 1.2e-4, 3.0e-7),
        )
        .unwrap();
        (rain, vec![first, second])
    }

    #[test]
    fn diagnostic_coexistence_conserves_mass_without_double_counting_rain() {
        let (rain, frozen) = coexistence_categories();
        let input = DiagnosticCoexistenceInput::new(271.65, rain, frozen).unwrap();
        let result = input.diagnose().unwrap();
        assert_eq!(result.model_identifier(), "DiagnosticCoexistenceV1");
        assert!(!result.is_scheme_native());
        assert_close(result.target_wet_fraction(), 0.5);
        assert_close(result.input_rain_mass_kgkg(), 8.0e-4);
        assert_close(result.input_frozen_mass_kgkg(), 4.0e-4);
        assert_close(result.paired_liquid_mass_kgkg(), 4.0e-4);
        assert_close(result.unused_rain_mass_kgkg(), 4.0e-4);
        assert_close(result.wet_fraction(), 0.5);
        assert_close(
            result.paired_liquid_mass_kgkg() + result.unused_rain_mass_kgkg(),
            result.input_rain_mass_kgkg(),
        );
        assert_close(
            result.output_total_mass_kgkg(),
            result.input_total_mass_kgkg(),
        );
        let output_wet_mass = result
            .wet_categories()
            .iter()
            .map(DiagnosticWetCategory::wet_total_mass_kgkg)
            .sum::<f64>();
        assert_close(
            output_wet_mass,
            result.input_frozen_mass_kgkg() + result.paired_liquid_mass_kgkg(),
        );
        for category in result.wet_categories() {
            assert!((0.0..=1.0).contains(&category.wet_fraction()));
            assert_close(category.wet_fraction(), 0.5);
        }
    }

    #[test]
    fn diagnostic_coexistence_preserves_frozen_category_fractions_and_bounds() {
        let (rain, frozen) = coexistence_categories();
        let input = DiagnosticCoexistenceInput::new(271.65, rain, frozen).unwrap();
        let result = diagnose_coexistence(&input).unwrap();
        assert_eq!(result.wet_categories().len(), 2);
        assert_close(result.wet_categories()[0].frozen_category_fraction(), 0.25);
        assert_close(result.wet_categories()[1].frozen_category_fraction(), 0.75);
        assert_close(result.wet_categories()[0].wet_category_fraction(), 0.25);
        assert_close(result.wet_categories()[1].wet_category_fraction(), 0.75);

        let rain_density = input.rain().shape().bulk_density_kg_m3();
        let rain_axis = input.rain().shape().minor_to_major_axis_ratio();
        let rain_speed = input.rain().fall_speed_m_s().value();
        for category in result.wet_categories() {
            let dry = category.source_category();
            let density = category.effective_density_kg_m3().value();
            let axis = category.minor_to_major_axis_ratio().value();
            let speed = category.fall_speed_m_s().value();
            assert!(density >= dry.shape().bulk_density_kg_m3().min(rain_density));
            assert!(density <= dry.shape().bulk_density_kg_m3().max(rain_density));
            assert!(axis >= dry.shape().minor_to_major_axis_ratio().min(rain_axis));
            assert!(axis <= dry.shape().minor_to_major_axis_ratio().max(rain_axis));
            assert!(speed >= dry.fall_speed_m_s().value().min(rain_speed));
            assert!(speed <= dry.fall_speed_m_s().value().max(rain_speed));
            let (_, dry_sigma, _) = dry.orientation().gaussian_parameters().unwrap();
            let (_, wet_sigma, _) = category.canting().effective_gaussian().unwrap();
            assert!(wet_sigma >= 0.0_f64.min(dry_sigma));
            assert!(wet_sigma <= 0.0_f64.max(dry_sigma));
        }
    }

    #[test]
    fn diagnostic_coexistence_envelope_and_liquid_limits_bound_wet_fraction() {
        let (rain, frozen) = coexistence_categories();
        assert!(matches!(
            DiagnosticCoexistenceInput::new(260.0, rain.clone(), frozen.clone()),
            Err(ClosureError::OutsideCoexistenceEnvelope { .. })
        ));
        let cold = DiagnosticCoexistenceInput::new(
            DIAGNOSTIC_COEXISTENCE_COLD_K,
            rain.clone(),
            frozen.clone(),
        )
        .unwrap()
        .diagnose()
        .unwrap();
        assert_close(cold.target_wet_fraction(), 0.0);
        assert_close(cold.paired_liquid_mass_kgkg(), 0.0);
        assert!(
            cold.wet_categories()
                .iter()
                .all(|category| category.wet_fraction() == 0.0)
        );

        let warm = DiagnosticCoexistenceInput::new(DIAGNOSTIC_COEXISTENCE_WARM_K, rain, frozen)
            .unwrap()
            .diagnose()
            .unwrap();
        assert_close(warm.target_wet_fraction(), 1.0);
        assert_close(warm.unused_rain_mass_kgkg(), 0.0);
        assert!(
            warm.wet_categories()
                .iter()
                .all(|category| (0.0..=1.0).contains(&category.wet_fraction()))
        );
    }

    #[test]
    fn mixture_topology_hooks_never_claim_a_lut_or_amplitude_result() {
        assert_eq!(MixtureMetadata::SUPPORTED_TOPOLOGIES.len(), 2);
        for topology in MixtureMetadata::SUPPORTED_TOPOLOGIES {
            let (rain, frozen) = coexistence_categories();
            let result = DiagnosticCoexistenceInput::new(271.65, rain, frozen)
                .unwrap()
                .with_topology(topology)
                .diagnose()
                .unwrap();
            assert_eq!(result.mixture().topology(), topology);
            assert_eq!(
                result.mixture().scattering_status(),
                MixtureScatteringStatus::NotEvaluatedNoLutOrAmplitude
            );
            assert!(result.wet_categories().iter().all(|category| {
                category.mixture().scattering_status()
                    == MixtureScatteringStatus::NotEvaluatedNoLutOrAmplitude
            }));
        }
    }

    #[test]
    fn coexistence_rejects_wrong_phase_roles() {
        let (rain, frozen) = coexistence_categories();
        assert_eq!(
            DiagnosticCoexistenceInput::new(271.65, rain.clone(), vec![]).unwrap_err(),
            ClosureError::NoFrozenCategories
        );
        assert_eq!(
            DiagnosticCoexistenceInput::new(271.65, frozen[0].clone(), frozen.clone()).unwrap_err(),
            ClosureError::RainCategoryRequired
        );
        assert!(matches!(
            DiagnosticCoexistenceInput::new(271.65, rain.clone(), vec![rain]),
            Err(ClosureError::FrozenCategoryRequired { index: 0 })
        ));
    }
}
