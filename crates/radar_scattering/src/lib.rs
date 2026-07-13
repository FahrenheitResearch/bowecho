//! Non-UI radar-scattering primitives and a fail-closed offline LUT format.
//!
//! The crate deliberately stops at the science/serialization boundary. It
//! does not decode WRF, select a microphysics scheme, or write application
//! products. Its scheme-native PSD support begins only after a caller supplies
//! a typed raw microphysics tuple. In particular, no LUT shipped by this crate
//! is represented as production T-matrix science.

#![forbid(unsafe_code)]

pub mod closure;
mod digest;
mod lut;
mod orientation;
mod output;
mod p3_integration;
mod p3_psd;
mod p3_table;
mod particle;
mod scheme_psd;
mod science;
mod tmatrix_runtime;

pub use closure::{
    ClosedOrientation, ClosedParticleCategory, ClosureContext, ClosureError,
    ConventionalCategoryInput, DIAGNOSTIC_COEXISTENCE_COLD_K, DIAGNOSTIC_COEXISTENCE_WARM_K,
    DiagnosticCantingTransition, DiagnosticCoexistenceInput, DiagnosticCoexistenceResult,
    DiagnosticWetCategory, ICE_MATERIAL_DENSITY_KG_M3, IshmaelCategoryInput, IshmaelDiagnostics,
    IshmaelSourceFields, LIQUID_WATER_DENSITY_KG_M3, MixtureMetadata, MixtureScatteringStatus,
    MixtureTopology, OrientationDefinition, P3Category, P3CategoryInput, PROPERTY_CLOSURE_REVISION,
    PropertyProvenance, PropertySourceKind, SourcedScalar, close_conventional_category,
    close_ishmael_category, close_p3_category, diagnose_coexistence,
};
pub use digest::{DigestError, Sha256Digest};
pub use lut::{
    Axis, AxisCoordinate, AxisKind, GeneratorMetadata, InterpolationError, LUT_MAGIC,
    LUT_SCHEMA_VERSION, LutError, LutHeader, OfflineLut, OutputDescriptor, OutputKind,
    PREPARED_INTERPOLATION_AXIS_SLOTS, PayloadEncoding, PreparedInterpolationPlan, Unit,
};
pub use orientation::{
    BodyFrameScattering, BodyOrientation, Complex64, OrientationError, RadarGeometry,
    RadarJonesMatrix, RadarScattering, SymmetricScatteringTensor, UnitVector3,
    transform_body_to_radar,
};
pub use output::{
    AdditiveScattering, ComplexCovariance, FallSpeedMoments, LinearReflectivity, OutputError,
    PolarAccumulatorQuantities, SpecificAttenuation, SpecificDifferentialPhase,
};
pub use p3_integration::{
    P3_PROJECTED_AREA_EQUIVALENT_OBLATE_REVISION, P3_PROJECTED_AREA_EQUIVALENT_SPHEROID_REVISION,
    P3_SMALL_SPHERE_RAYLEIGH_BRIDGE_REVISION, P3_SPHERICAL_INTEGRATION_REVISION,
    P3_TMATRIX_SOURCE_DOMAIN_PROVENANCE, P3_WRF_LOOKUP_INTEGRATION_BIN_COUNT,
    P3_WRF_LOOKUP_INTEGRATION_BIN_WIDTH_M, P3_WRF_LOOKUP_INTEGRATION_BIN_WIDTH_UM,
    P3_WRF_LOOKUP_INTEGRATION_DOMAIN_REVISION, P3_WRF_LOOKUP_INTEGRATION_FIRST_CENTER_M,
    P3_WRF_LOOKUP_INTEGRATION_FIRST_CENTER_UM, P3_WRF_LOOKUP_INTEGRATION_LAST_CENTER_M,
    P3_WRF_LOOKUP_INTEGRATION_LAST_CENTER_UM, P3_WRF_LOOKUP_INTEGRATION_MAXIMUM_DIMENSION_M,
    P3_WRF_LOOKUP_INTEGRATION_SOURCE_PATH, P3_WRF_LOOKUP_INTEGRATION_UPPER_EDGE_UM,
    P3ParticleScatteringRoute, P3SmallSphereScatteringPolicy, P3TMatrixInSourceOmissionAudit,
    P3TMatrixIntegrationAudit, P3TMatrixIntegrationConfig, P3TMatrixIntegrationConfigError,
    P3TMatrixIntegrationError, P3TMatrixIntegrationResult, P3TMatrixParticleNode,
    P3TMatrixPreparationError, P3TMatrixShapePolicy, P3TMatrixSourceDomainAudit,
    P3TMatrixSourceDomainAuthority, P3TMatrixSourceDomainProvenance, P3TMatrixWeightFractions,
    PreparedP3TMatrixIntegration, integrate_p3_tmatrix_psd, prepare_p3_tmatrix_psd,
};
pub use p3_psd::{
    P3_MODULE_VERSION, P3_MU_RANGE, P3_MULTICATEGORY_DOI, P3_PART_I_DOI, P3_PSD_REVISION,
    P3_RIME_DENSITY_RANGE_KG_M3, P3_SOLID_ICE_DENSITY_KG_M3, P3_TABLE_GENERATOR_VERSION,
    P3_THREE_MOMENT_TABLE_SHA256, P3_THREE_MOMENT_TABLE_VERSION, P3_TRIPLE_MOMENT_CONTEXT_DOI,
    P3_TWO_MOMENT_TABLE_SHA256, P3_TWO_MOMENT_TABLE_VERSION, P3_WRF_NSMALL_PER_KG,
    P3_WRF_QSMALL_KGKG, P3_WRF_RELEASE, P3_WRF_SOURCE_COMMIT, P3IceMomentInput, P3IceMomentOrder,
    P3LookupAxisClamps, P3LookupFailure, P3LookupQuery, P3LookupSolution, P3LookupTableDescriptor,
    P3LookupTableV54, P3MomentClosureAudit, P3NumberLimiterAudit, P3OmissionTailAudit,
    P3ParticleGeometry, P3ParticleRegion, P3PiecewiseParticleLaw, P3PopulationMoments,
    P3ProjectedAreaConsistency, P3Psd, P3PsdError, P3PsdInput, P3PsdProvenance, P3Quadrature,
    P3QuadratureAudit, P3QuadratureConfig, P3QuadratureNode, P3ReconstructionConfig,
    P3ShapeAuthority, P3WrfScheme,
};
pub use p3_table::{
    P3_TABLE_READER_REVISION, P3_THREE_MOMENT_TABLE_ASSET, P3_TWO_MOMENT_TABLE_ASSET,
    P3OfficialTableKind, P3OfficialTableV54, P3TableAssetSpec, P3TableLoadError,
};
pub use particle::{
    ClosureAssumption, ConventionalHydrometeor, ConventionalParticleState, ConventionalProvenance,
    IshmaelIceCategory, IshmaelParticleState, IshmaelProvenance, MicrophysicsFamily,
    P3ParticleState, P3Provenance, ParticleEnvironment, ParticleError, ParticleProvenance,
    ParticleRecord, ParticleShape, ParticleState, ProvenanceError, SourceVariable,
};
pub use scheme_psd::{
    DEFAULT_ADDITIVE_ABSOLUTE_TOLERANCES, ISHMAEL_DELTA_RANGE, ISHMAEL_DENSITY_RANGE_KG_M3,
    ISHMAEL_GAMMA_SHAPE, ISHMAEL_MONOMER_SEMI_AXIS_M, ISHMAEL_PSD_REVISION,
    ISHMAEL_SMALL_SPHERE_RAYLEIGH_BRIDGE_REVISION, ISHMAEL_SOLID_ICE_MATERIAL_CLOSURE_REVISION,
    IshmaelParticleScatteringRoute, IshmaelPsd, IshmaelPsdInput, IshmaelReconstructionAudit,
    IshmaelScatteringParticleNode, IshmaelSmallSphereScatteringPolicy,
    IshmaelSolidIceMaterialClosure, PreparedIshmaelPsdIntegration, PsdError, PsdFallSpeedAuthority,
    PsdFallSpeedProvenance, PsdIntegrationAudit, PsdIntegrationConfig, PsdIntegrationError,
    PsdIntegrationResult, PsdParticleDomain, PsdParticleNode, PsdParticleSupport,
    PsdQuadratureLevel, PsdQuadratureRule, PsdSourceCategory, PsdSpheroidHabit,
    SCHEME_PSD_REVISION, SchemePsdRevision, analytic_gamma_moment, integrate_ishmael_psd,
    prepare_ishmael_psd, prepare_ishmael_psd_with_solid_ice_material_closure,
};
pub use science::{
    EffectiveMediumRule, KernelModel, MeltingModel, OrientationModel, ScienceError,
    ScienceMetadata, TMatrixImplementation, TableValidation, TemporalSampling,
};
pub use tmatrix_runtime::{
    ComplexRefractiveIndex, DensityApplicability, EvaluationError, FallMomentPolicy,
    HomogeneousMaterial, NumberScalingPolicy, PreparedTMatrixLutInterpolation,
    PreparedTMatrixParticleNode, RadarConventionDescriptor, RadarHvConvention,
    RadarViewApplicability, RadarViewGeometry, ResearchTMatrixLut, ScaledScatteringContribution,
    SpheroidConvention, TMatrixEvaluationRequest, TMatrixExecutionDescriptor, TMatrixLoadError,
    TMatrixMaterial, TMatrixOdfConvention, TMatrixParticleCategory, TMatrixParticleNodeQuery,
    TMatrixPopulationRole, TMatrixTableDescriptor, TerminalSpeedPolicy,
};
