//! Non-UI radar-scattering primitives and a fail-closed offline LUT format.
//!
//! The crate deliberately stops at the science/serialization boundary. It
//! does not decode WRF, select a microphysics scheme, integrate a particle
//! size distribution, or write application products. In particular, no LUT
//! shipped by this crate is represented as production T-matrix science.

#![forbid(unsafe_code)]

mod digest;
mod lut;
mod orientation;
mod output;
mod particle;
mod science;

pub use digest::{DigestError, Sha256Digest};
pub use lut::{
    Axis, AxisCoordinate, AxisKind, GeneratorMetadata, InterpolationError, LUT_MAGIC,
    LUT_SCHEMA_VERSION, LutError, LutHeader, OfflineLut, OutputDescriptor, OutputKind,
    PayloadEncoding, Unit,
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
pub use particle::{
    ClosureAssumption, ConventionalHydrometeor, ConventionalParticleState, ConventionalProvenance,
    IshmaelIceCategory, IshmaelParticleState, IshmaelProvenance, MicrophysicsFamily,
    P3ParticleState, P3Provenance, ParticleEnvironment, ParticleError, ParticleProvenance,
    ParticleRecord, ParticleShape, ParticleState, ProvenanceError, SourceVariable,
};
pub use science::{
    EffectiveMediumRule, KernelModel, MeltingModel, OrientationModel, ScienceError,
    ScienceMetadata, TMatrixImplementation, TableValidation, TemporalSampling,
};
