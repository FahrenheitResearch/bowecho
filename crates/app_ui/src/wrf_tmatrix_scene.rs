//! Property-aware T-matrix cache for one WRF model instant.
//!
//! The cache retains only sparse source-cell indices and nine additive f32
//! components at every validated radar-elevation LUT node.  Query-time
//! elevation interpolation happens component by component; nonlinear radar
//! products are derived only after interpolation. Strict policy is fail-closed.
//! The separately named Hybrid policy may replace a complete frozen cell with
//! the versioned bulk Rayleigh operator only for narrowly classified table-
//! domain/shape omissions or a typed native ISHMAEL source-state mass-closure
//! gap; every such cell and population is audited.

use std::collections::BTreeSet;
use std::f64::consts::PI;
use std::mem::size_of;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use radar_scattering::{
    AdditiveScattering, AxisKind, ClosedParticleCategory, ConventionalHydrometeor,
    DIAGNOSTIC_COEXISTENCE_COLD_K, DIAGNOSTIC_COEXISTENCE_WARM_K, DiagnosticWetCategory,
    EvaluationError, FallMomentPolicy, ICE_MATERIAL_DENSITY_KG_M3, ISHMAEL_PSD_REVISION,
    ISHMAEL_SMALL_SPHERE_RAYLEIGH_BRIDGE_REVISION, ISHMAEL_SOLID_ICE_MATERIAL_CLOSURE_REVISION,
    InterpolationError, IshmaelParticleScatteringRoute, IshmaelPsd, IshmaelPsdInput,
    IshmaelScatteringParticleNode, IshmaelSmallSphereScatteringPolicy, MixtureTopology,
    OrientationDefinition, OrientationModel, OutputError, P3_MODULE_VERSION,
    P3_PROJECTED_AREA_EQUIVALENT_OBLATE_REVISION, P3_PROJECTED_AREA_EQUIVALENT_SPHEROID_REVISION,
    P3_PSD_REVISION, P3_SMALL_SPHERE_RAYLEIGH_BRIDGE_REVISION, P3_SOLID_ICE_DENSITY_KG_M3,
    P3_SPHERICAL_INTEGRATION_REVISION, P3_TABLE_READER_REVISION, P3_WRF_SOURCE_COMMIT,
    P3IceMomentInput, P3LookupTableV54, P3OfficialTableKind, P3OfficialTableV54, P3ParticleRegion,
    P3ParticleScatteringRoute, P3Psd, P3PsdError, P3PsdInput, P3QuadratureNode,
    P3ReconstructionConfig, P3ShapeAuthority, P3SmallSphereScatteringPolicy,
    P3TMatrixIntegrationConfig, P3TMatrixIntegrationError, P3TMatrixIntegrationResult,
    P3TMatrixParticleNode, P3TMatrixShapePolicy, P3WrfScheme, ParticleState,
    PolarAccumulatorQuantities, PreparedIshmaelPsdIntegration, PreparedP3TMatrixIntegration,
    PreparedTMatrixLutInterpolation, PsdError, PsdFallSpeedProvenance, PsdIntegrationConfig,
    PsdIntegrationError, PsdIntegrationResult, PsdParticleNode, PsdParticleSupport,
    PsdQuadratureLevel, PsdSpheroidHabit, RadarViewApplicability, RadarViewGeometry,
    ResearchTMatrixLut, SCHEME_PSD_REVISION, Sha256Digest, SpheroidConvention,
    TMatrixEvaluationRequest, TMatrixMaterial, TMatrixOdfConvention, TMatrixParticleCategory,
    TMatrixPopulationRole, prepare_ishmael_psd_with_solid_ice_material_closure,
    prepare_p3_tmatrix_psd,
};
use rayon::prelude::*;
use simradar_cuda::CudaPreparedTMatrixNode;
use thiserror::Error;

use crate::wrf_tmatrix_cuda::{WrfTMatrixCudaBatchService, WrfTMatrixCudaTableRole};

use crate::wrf_radar_physics::{
    BulkSpeciesInput, HydrometeorKind, PROPERTY_HYBRID_BULK_RAYLEIGH_REVISION,
    property_hybrid_bulk_rayleigh_contribution,
};

use crate::wrf_property_reader::{
    ClosedCellCategory, ClosedPropertyCell, ClosedRainState, CoexistenceUnavailable,
    PropertySceneIdentity, RainUnavailableReason, RawPropertyCategory, RawPropertyCell,
    RawPropertyClosureError, RequiredFieldContract, RequiredFieldSignature, SourceFieldProvenance,
    WrfPropertyCategory, WrfPropertyReadError, WrfPropertyScene, close_raw_property_cell,
    close_raw_rain_state,
};
use crate::wrf_temporal::ScenePropertySignature;

/// Legacy embedded research bundle frequency. Production evaluation derives
/// its exact frequency from the validated five-table bundle instead.
pub const PROPERTY_TMATRIX_FREQUENCY_HZ: f64 = 2_800_000_000.0;
const PROPERTY_TMATRIX_SUPPORTED_FREQUENCIES_HZ: [f64; 3] =
    [2_800_000_000.0, 5_600_000_000.0, 9_400_000_000.0];
pub const PROPERTY_TMATRIX_MIN_ELEVATION_DEG: f64 = -0.5;
pub const PROPERTY_TMATRIX_MAX_ELEVATION_DEG: f64 = 20.0;
pub const MAX_WEIGHTED_PROPERTY_CELLS: usize = 8;

const WEIGHT_SUM_TOLERANCE: f64 = 1.0e-9;
const MAX_PROPERTY_CATEGORIES_PER_CELL: usize = 3;
const PER_WORKER_ALLOCATION_GUARD_BYTES: usize = 16 * 1024;

// Production-v1 ISHMAEL PSD integration. The research dry tables start at
// 50 micrometres, so omitted number may be large even while mass and D6 are
// well represented. Those independently audited thresholds are explicit and
// fail closed; no particle coordinate is clipped onto a table edge.
const ISHMAEL_PSD_MAXIMUM_OMITTED_NUMBER_FRACTION: f64 = 0.999;
const ISHMAEL_PSD_MAXIMUM_OMITTED_MASS_FRACTION: f64 = 0.05;

fn check_cancel(cancel: Option<&AtomicBool>) -> Result<(), ()> {
    if cancel.is_some_and(|cancel| cancel.load(Ordering::Acquire)) {
        Err(())
    } else {
        Ok(())
    }
}

#[derive(Debug)]
enum ParticleFinishError<E> {
    Cancelled,
    Science(E),
}
const ISHMAEL_PSD_MAXIMUM_OMITTED_D6_FRACTION: f64 = 0.001;

// P3 independently gates omitted number, mass, and the official P3
// mass-squared/equivalent-ice-volume-squared radar weight. In strict mode
// nonspherical P3 particles count as omitted; projected-area research mode
// maps them through its explicitly versioned equivalent-oblate assumption
// before this gate.
const P3_MAXIMUM_OMITTED_NUMBER_FRACTION: f64 = 0.999;
const P3_MAXIMUM_OMITTED_MASS_FRACTION: f64 = 0.05;
// The real P3 regression corpus contains a projected-area-equivalent,
// ultra-low-density tail with 0.2123% of N*m^2 radar weight outside the
// current EM table. A 0.25% ceiling bounds its Rayleigh-equivalent impact to
// about 0.011 dB while preserving strict, separately audited number/mass
// gates and avoiding any coordinate clamp or invented table value.
const P3_MAXIMUM_OMITTED_RADAR_WEIGHT_FRACTION: f64 = 0.0025;

// P3 retains an ice category while it melts above freezing, but WRF P3 v4.5.2
// does not predict liquid fraction within that category (the source calls
// that future work). Ambient air temperature is therefore not a valid dry-ice
// material temperature above the freezing/melting point. Keep the native
// PSD/geometry unchanged and diagnose only the phase-constrained particle
// temperature: ice colder than freezing follows the air; melting ice is held
// at 273.15 K. This is a physical phase constraint, not a generic LUT-edge
// clamp.
const ICE_MELTING_TEMPERATURE_K: f64 = 273.15;

// Exact coordinate contract for the versioned five-table research bundle.
// Each frozen role keeps its own solver-complete diameter domain; no renderer
// invariant requires the four phase/shape tables to share a size axis.
const DRY_OBLATE_DIAMETER_M: &[f64] = &[
    0.00005,
    0.00005590169943749475,
    0.0000625,
    0.00006987712429686843,
    0.000078125,
    0.00008734640537108554,
    0.00009765625,
    0.00010918300671385693,
    0.0001220703125,
    0.00013647875839232116,
    0.000152587890625,
    0.00017059844799040144,
    0.00019073486328125,
    0.0002132480599880018,
    0.0002384185791015625,
    0.00026656007498500225,
    0.0002980232238769531,
    0.0003332000937312528,
    0.0003725290298461914,
    0.00041650011716406603,
    0.00046566128730773926,
    0.0005206251464550826,
    0.0005820766091346741,
    0.0006507814330688531,
    0.0007275957614183426,
    0.0008134767913360665,
    0.0009094947017729282,
    0.001016845989170083,
    0.0011368683772161603,
    0.0012710574864626038,
    0.0014210854715202004,
    0.0015888218580782547,
    0.0017763568394002505,
    0.0019860273225978187,
    0.002220446049250313,
    0.002482534153247273,
    0.0027755575615628914,
    0.0031031676915590912,
    0.003469446951953614,
    0.003878959614448864,
    0.004336808689942018,
    0.004848699518061081,
    0.005421010862427522,
    0.00606087439757635,
    0.006776263578034403,
    0.007576092996970438,
    0.008470329472543003,
    0.009470116246213047,
    0.010587911840678754,
    0.01183764530776631,
    0.013234889800848443,
    0.014797056634707886,
    0.016543612251060553,
    0.018496320793384858,
    0.02067951531382569,
    0.02312040099173107,
    0.025849394142282114,
    0.028900501239663836,
    0.03231174267785264,
    0.0361256265495798,
    0.0403896783473158,
    0.04515703318697475,
    0.05048709793414475,
    0.056446291483718436,
    0.06310887241768094,
    0.06672950816259267,
    0.06861699195817748,
    0.0688596010077363,
    0.069_102_210_057_295_12,
    0.069_344_819_106_853_94,
    0.06958742815641276,
    0.069_830_037_205_971_58,
    0.0700726462555304,
    0.070_315_255_305_089_22,
    0.07055786435464804,
    0.07255363548030914,
    0.07460585817834214,
    0.07888609052210117,
    0.08379058453350832,
    0.089,
];
const DRY_PROLATE_DIAMETER_M: &[f64] = &[
    0.00005,
    0.00005590169943749475,
    0.0000625,
    0.00006987712429686843,
    0.000078125,
    0.00008734640537108554,
    0.00009765625,
    0.00010918300671385693,
    0.0001220703125,
    0.00013647875839232116,
    0.000152587890625,
    0.00017059844799040144,
    0.00019073486328125,
    0.0002132480599880018,
    0.0002384185791015625,
    0.00026656007498500225,
    0.0002980232238769531,
    0.0003332000937312528,
    0.0003725290298461914,
    0.00041650011716406603,
    0.00046566128730773926,
    0.0005206251464550826,
    0.0005820766091346741,
    0.0006507814330688531,
    0.0007275957614183426,
    0.0008134767913360665,
    0.0009094947017729282,
    0.001016845989170083,
    0.0011368683772161603,
    0.0012710574864626038,
    0.0014210854715202004,
    0.0015888218580782547,
    0.0017763568394002505,
    0.0019860273225978187,
    0.002220446049250313,
    0.002482534153247273,
    0.0027755575615628914,
    0.0031031676915590912,
    0.003469446951953614,
    0.003878959614448864,
    0.004336808689942018,
    0.004848699518061081,
    0.005421010862427522,
    0.00606087439757635,
    0.006776263578034403,
    0.007576092996970438,
    0.008470329472543003,
    0.009470116246213047,
    0.010587911840678754,
    0.01183764530776631,
    0.013234889800848443,
    0.014797056634707886,
    0.016543612251060553,
    0.018496320793384858,
    0.02067951531382569,
    0.02312040099173107,
    0.025849394142282114,
    0.028900501239663836,
    0.03231174267785264,
];
const WET_OBLATE_DIAMETER_M: &[f64] = &[
    0.00005,
    0.00005590169943749475,
    0.0000625,
    0.00006987712429686843,
    0.000078125,
    0.00008734640537108554,
    0.00009765625,
    0.00010918300671385693,
    0.0001220703125,
    0.00013647875839232116,
    0.000152587890625,
    0.00017059844799040144,
    0.00019073486328125,
    0.0002132480599880018,
    0.0002384185791015625,
    0.00026656007498500225,
    0.0002980232238769531,
    0.0003332000937312528,
    0.0003725290298461914,
    0.00041650011716406603,
    0.00046566128730773926,
    0.0005206251464550826,
    0.0005820766091346741,
    0.0006507814330688531,
    0.0007275957614183426,
    0.0008134767913360665,
    0.0009094947017729282,
    0.001016845989170083,
    0.0011368683772161603,
    0.0012710574864626038,
    0.0014210854715202004,
    0.0015888218580782547,
    0.0017763568394002505,
    0.0019860273225978187,
    0.002220446049250313,
    0.002482534153247273,
    0.0027755575615628914,
    0.0031031676915590912,
    0.003469446951953614,
    0.003878959614448864,
    0.004336808689942018,
    0.004848699518061081,
    0.005421010862427522,
    0.00606087439757635,
    0.006776263578034403,
    0.007576092996970438,
    0.008470329472543003,
    0.009470116246213047,
    0.010587911840678754,
    0.01183764530776631,
    0.013234889800848443,
    0.014089831333721728,
    0.015,
];
const WET_PROLATE_DIAMETER_M: &[f64] = &[
    0.00005,
    0.00005590169943749475,
    0.0000625,
    0.00006987712429686843,
    0.000078125,
    0.00008734640537108554,
    0.00009765625,
    0.00010918300671385693,
    0.0001220703125,
    0.00013647875839232116,
    0.000152587890625,
    0.00017059844799040144,
    0.00019073486328125,
    0.0002132480599880018,
    0.0002384185791015625,
    0.00026656007498500225,
    0.0002980232238769531,
    0.0003332000937312528,
    0.0003725290298461914,
    0.00041650011716406603,
    0.00046566128730773926,
    0.0005206251464550826,
    0.0005820766091346741,
    0.0006507814330688531,
    0.0007275957614183426,
    0.0008134767913360665,
    0.0009094947017729282,
    0.001016845989170083,
    0.0011368683772161603,
    0.0012710574864626038,
    0.0014210854715202004,
    0.0015888218580782547,
    0.0017763568394002505,
    0.0019860273225978187,
    0.002220446049250313,
    0.002482534153247273,
    0.0027755575615628914,
    0.0031031676915590912,
    0.003469446951953614,
    0.003878959614448864,
    0.004336808689942018,
    0.004848699518061081,
    0.005421010862427522,
    0.005844002774921773,
    0.0063,
];
const DRY_TEMPERATURE_K: &[f64] = &[190.0, 230.0, 260.0, 273.15];
const DRY_OBLATE_BULK_DENSITY_KG_M3: &[f64] = &[
    1.5,
    1.5527824088285906,
    1.6055648176571813,
    1.67860891871667,
    1.7516530197761586,
    1.852_736_710_687_566,
    1.9538204015989735,
    2.0937072975983,
    2.233594193597626,
    2.4271797611198536,
    2.620765328642081,
    2.88866298756993,
    3.156560646497779,
    3.527296732754734,
    3.898032819011689,
    4.411_084_131_976_471,
    4.924135444941254,
    5.634_132_824_458_417,
    6.344130203975579,
    7.326_675_743_150_219,
    8.30922128232486,
    9.668_938_605_800_761,
    11.028655929276663,
    12.91033074845455,
    14.792005567632435,
    17.396_002_783_816_22,
    20.0,
    23.697187911261786,
    27.394375822523575,
    32.547670093431734,
    37.70096436433989,
    44.8838402364812,
    52.06671610862251,
    62.078_507_223_647_96,
    72.0902983386734,
    86.0451491693367,
    100.0,
    115.90047818877004,
    131.80095637754007,
    152.82064907669331,
    173.84034177584655,
    201.62739798393494,
    229.41445419202336,
    266.147_650_236_443_6,
    302.8808462808638,
    351.4404231404319,
    400.0,
    457.09209145962353,
    514.1841829192471,
    587.623_873_437_156_6,
    661.0635639550661,
    755.531781977533,
    850.0,
    883.5,
    917.0,
];
const DRY_PROLATE_BULK_DENSITY_KG_M3: &[f64] = &[
    1.5,
    1.6055648176571813,
    1.7516530197761586,
    1.9538204015989735,
    2.233594193597626,
    2.620765328642081,
    3.156560646497779,
    3.898032819011689,
    4.924135444941254,
    6.344130203975579,
    8.30922128232486,
    11.028655929276663,
    14.792005567632435,
    20.0,
    27.394375822523575,
    37.70096436433989,
    52.06671610862251,
    72.0902983386734,
    100.0,
    131.80095637754007,
    173.84034177584655,
    229.41445419202336,
    302.8808462808638,
    400.0,
    514.1841829192471,
    661.0635639550661,
    850.0,
    917.0,
];
const DRY_OBLATE_MINOR_TO_MAJOR: &[f64] = &[0.1, 0.25, 0.4, 0.55, 0.7, 0.85, 1.0];
const FROZEN_COARSE_MINOR_TO_MAJOR: &[f64] = &[0.1, 0.4, 0.7, 1.0];
const WET_TEMPERATURE_K: &[f64] = &[269.15, 273.15, 275.15];
const WET_CONDENSED_VOLUME_FRACTION: &[f64] = &[
    0.0015,
    0.0015652715083118158,
    0.0016565015488769442,
    0.0017840138512088385,
    0.0019622379004941497,
    0.002211341799982819,
    0.002559514524086532,
    0.0030461558254015037,
    0.0037263347703638504,
    0.004677021438182558,
    0.006005796971536168,
    0.0078630276215571,
    0.010458880420465652,
    0.014087106001130823,
    0.019158279863426903,
    0.0262462637194142,
    0.03615314455350763,
    0.05,
    0.06581082626645705,
    0.08675854222183639,
    0.1145121078073485,
    0.15128272300711795,
    0.2,
    0.23293356744814156,
    0.27132670078665116,
    0.31608446017210634,
    0.36826194221150443,
    0.42908915227189964,
    0.5,
    0.651872847953581,
    0.85,
    1.0,
];
const WET_LIQUID_MASS_FRACTION: &[f64] = &[0.0, 0.05, 0.1, 0.15, 0.2, 0.3, 0.4, 0.5, 0.6, 0.98];
const RAIN_DIAMETER_M: &[f64] = &[
    0.000018,
    0.00002,
    0.000022,
    0.0000245,
    0.000027,
    0.00003,
    0.0000332,
    0.0000368,
    0.0000408,
    0.0000451,
    0.00005,
    0.0000549,
    0.0000603,
    0.0000663,
    0.0000728,
    0.00008,
    0.0000885,
    0.000098,
    0.0001084,
    0.00012,
    0.0001328,
    0.000147,
    0.0001626,
    0.00018,
    0.0002,
    0.0002213,
    0.0002449,
    0.0002711,
    0.0003,
    0.000332269902974487,
    0.0003680109614089166,
    0.0004075965548029613,
    0.0004514402257237171,
    0.0005,
    0.000549280271653059,
    0.0006034176336545163,
    0.0006628908034679974,
    0.0007282256812104322,
    0.0008,
    0.0008853455357602572,
    0.0009797958971132711,
    0.0010843224043318137,
    0.0012,
    0.0013280183036403859,
    0.0014696938456699065,
    0.0016264836064977205,
    0.0018,
    0.0019733209007916346,
    0.0021633307652783934,
    0.0023716365635830087,
    0.0026,
    0.0028978978026935596,
    0.0032299275672523706,
    0.0036,
    0.003962312698673552,
    0.004361089422797134,
    0.0048,
    0.005366563145999495,
    0.006,
    0.006480740698407861,
    0.00673536820737033,
    0.006866409356540893,
    0.0068747185511922075,
    0.006_883_027_745_843_522,
    0.006891336940494837,
    0.006899646135146152,
    0.006_907_955_329_797_467,
    0.006_916_264_524_448_781,
    0.006_924_573_719_100_096,
    0.006932882913751411,
    0.006966360627778315,
    0.007,
];
const RAIN_TEMPERATURE_K: &[f64] = &[225.0, 237.5, 250.0, 269.15, 293.15, 313.15];
const RAIN_MINOR_TO_MAJOR: &[f64] = &[0.5, 0.6, 0.7, 0.775, 0.85, 0.925, 1.0];
const FROZEN_RADAR_ELEVATION_DEG: &[f64] = &[-0.5, 0.9, 4.5, 10.0, 20.0];
const RAIN_RADAR_ELEVATION_DEG: &[f64] = &[-0.5, 0.2, 0.9, 2.7, 4.5, 7.25, 10.0, 15.0, 20.0];
const PROPERTY_SOLVER_DDELT: f64 = 0.001;
const DRY_OBLATE_SOLVER_NDGS: u32 = 14;
const DRY_PROLATE_SOLVER_NDGS: u32 = 14;
const WET_OBLATE_SOLVER_NDGS: u32 = 14;
const WET_PROLATE_SOLVER_NDGS: u32 = 14;
const RAIN_SOLVER_NDGS: u32 = 14;

const DRY_OBLATE_TABLE_ID: &str =
    "property-p3-ishmael-dry-oblate-sband-pytmatrix-0.3.3-unvalidated-v1";
const DRY_PROLATE_TABLE_ID: &str =
    "property-p3-ishmael-dry-prolate-sband-pytmatrix-0.3.3-unvalidated-v1";
const WET_OBLATE_TABLE_ID: &str =
    "property-p3-ishmael-wet-oblate-sband-pytmatrix-0.3.3-unvalidated-v1";
const WET_PROLATE_TABLE_ID: &str =
    "property-p3-ishmael-wet-prolate-sband-pytmatrix-0.3.3-unvalidated-v1";
const RAIN_TABLE_ID: &str = "property-rain-sband-pytmatrix-0.3.3-unvalidated-v2";

const DRY_AXIS_KINDS: &[AxisKind] = &[
    AxisKind::EquivolumeDiameter,
    AxisKind::Temperature,
    AxisKind::BulkDensity,
    AxisKind::MinorToMajorAxisRatio,
    AxisKind::Frequency,
    AxisKind::RadarElevation,
];
const WET_AXIS_KINDS: &[AxisKind] = &[
    AxisKind::EquivolumeDiameter,
    AxisKind::Temperature,
    AxisKind::CondensedVolumeFraction,
    AxisKind::LiquidMassFraction,
    AxisKind::MinorToMajorAxisRatio,
    AxisKind::Frequency,
    AxisKind::RadarElevation,
];
const RAIN_AXIS_KINDS: &[AxisKind] = &[
    AxisKind::EquivolumeDiameter,
    AxisKind::Temperature,
    AxisKind::MinorToMajorAxisRatio,
    AxisKind::Frequency,
    AxisKind::RadarElevation,
];

/// Exact role of each reference in the five-table scene bundle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WrfTMatrixTableRole {
    DryOblate,
    DryProlate,
    WetOblate,
    WetProlate,
    RainStandaloneAndResidual,
}

impl WrfTMatrixTableRole {
    const fn legacy_embedded_s_id(self) -> &'static str {
        match self {
            Self::DryOblate => DRY_OBLATE_TABLE_ID,
            Self::DryProlate => DRY_PROLATE_TABLE_ID,
            Self::WetOblate => WET_OBLATE_TABLE_ID,
            Self::WetProlate => WET_PROLATE_TABLE_ID,
            Self::RainStandaloneAndResidual => RAIN_TABLE_ID,
        }
    }
}

impl std::fmt::Display for WrfTMatrixTableRole {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::DryOblate => "dry oblate",
            Self::DryProlate => "dry prolate",
            Self::WetOblate => "wet oblate",
            Self::WetProlate => "wet prolate",
            Self::RainStandaloneAndResidual => "standalone/residual rain",
        })
    }
}

/// Borrowed, typed inputs required to build a property-scattering scene.
#[derive(Clone, Copy)]
pub struct WrfTMatrixLutBundle<'a> {
    pub dry_oblate: &'a ResearchTMatrixLut,
    pub dry_prolate: &'a ResearchTMatrixLut,
    pub wet_oblate: &'a ResearchTMatrixLut,
    pub wet_prolate: &'a ResearchTMatrixLut,
    pub rain_standalone_and_residual: &'a ResearchTMatrixLut,
}

impl<'a> WrfTMatrixLutBundle<'a> {
    #[must_use]
    pub const fn new(
        dry_oblate: &'a ResearchTMatrixLut,
        dry_prolate: &'a ResearchTMatrixLut,
        wet_oblate: &'a ResearchTMatrixLut,
        wet_prolate: &'a ResearchTMatrixLut,
        rain_standalone_and_residual: &'a ResearchTMatrixLut,
    ) -> Self {
        Self {
            dry_oblate,
            dry_prolate,
            wet_oblate,
            wet_prolate,
            rain_standalone_and_residual,
        }
    }

    /// Validate every role, axis, material, orientation, solver, and shared
    /// radar convention before a WRF source scene is read or allocated.
    pub fn validate(self) -> Result<(), WrfTMatrixBundleError> {
        validate_bundle(self).map(|_| ())
    }

    fn entries(self) -> [(WrfTMatrixTableRole, &'a ResearchTMatrixLut); 5] {
        [
            (WrfTMatrixTableRole::DryOblate, self.dry_oblate),
            (WrfTMatrixTableRole::DryProlate, self.dry_prolate),
            (WrfTMatrixTableRole::WetOblate, self.wet_oblate),
            (WrfTMatrixTableRole::WetProlate, self.wet_prolate),
            (
                WrfTMatrixTableRole::RainStandaloneAndResidual,
                self.rain_standalone_and_residual,
            ),
        ]
    }

    fn dry(self, spheroid: SpheroidConvention) -> &'a ResearchTMatrixLut {
        match spheroid {
            SpheroidConvention::OblateMinorVertical => self.dry_oblate,
            SpheroidConvention::ProlateMajorVertical => self.dry_prolate,
        }
    }

    fn wet(self, spheroid: SpheroidConvention) -> &'a ResearchTMatrixLut {
        match spheroid {
            SpheroidConvention::OblateMinorVertical => self.wet_oblate,
            SpheroidConvention::ProlateMajorVertical => self.wet_prolate,
        }
    }
}

/// Exact official P3 table plus an explicit, versioned T-matrix shape policy.
/// Loading/caching the external table remains an application-boundary choice;
/// this constructor never downloads or substitutes an asset.
#[derive(Clone)]
pub struct WrfTMatrixP3Resources {
    pub table: Arc<P3OfficialTableV54>,
    pub integration: P3TMatrixIntegrationConfig,
}

impl WrfTMatrixP3Resources {
    pub fn strict_shape_authoritative(
        table: Arc<P3OfficialTableV54>,
    ) -> Result<Self, radar_scattering::P3TMatrixIntegrationConfigError> {
        Ok(Self {
            table,
            integration: production_p3_integration_config(
                P3TMatrixShapePolicy::StrictShapeAuthoritativeSpheres,
            )?,
        })
    }

    pub fn projected_area_equivalent_spheroid_research(
        table: Arc<P3OfficialTableV54>,
    ) -> Result<Self, radar_scattering::P3TMatrixIntegrationConfigError> {
        Ok(Self {
            table,
            integration: production_p3_integration_config(
                P3TMatrixShapePolicy::ProjectedAreaEquivalentSpheroidGaussian20ResearchV1,
            )?,
        })
    }
}

/// Validated, reusable evaluator for one spatially/temporally blended raw
/// property cell. Construction performs the complete five-table contract gate
/// once; each call then performs exactly one nonlinear closure/PSD integration
/// at the requested radar elevation.
#[derive(Clone)]
pub struct WrfTMatrixRawEvaluator<'a> {
    tables: WrfTMatrixLutBundle<'a>,
    validated: ValidatedBundle<'a>,
    p3: Option<WrfTMatrixP3Resources>,
}

/// One stable-position input to the cut-wide raw-property evaluator.
///
/// Callers retain ownership of the blended raw cell. The batch preserves this
/// slice order through parallel preparation, CUDA role sweeps, CPU replay, and
/// final error selection.
#[derive(Clone, Copy)]
pub struct WrfTMatrixRawBatchRequest<'raw> {
    raw: &'raw RawPropertyCell,
    elevation_deg: f64,
}

impl<'raw> WrfTMatrixRawBatchRequest<'raw> {
    #[must_use]
    pub const fn new(raw: &'raw RawPropertyCell, elevation_deg: f64) -> Self {
        Self { raw, elevation_deg }
    }

    #[must_use]
    pub const fn raw(self) -> &'raw RawPropertyCell {
        self.raw
    }

    #[must_use]
    pub const fn elevation_deg(self) -> f64 {
        self.elevation_deg
    }
}

impl<'a> WrfTMatrixRawEvaluator<'a> {
    pub fn new(tables: WrfTMatrixLutBundle<'a>) -> Result<Self, WrfTMatrixBundleError> {
        Ok(Self {
            tables,
            validated: validate_bundle(tables)?,
            p3: None,
        })
    }

    pub fn new_with_p3(
        tables: WrfTMatrixLutBundle<'a>,
        p3: WrfTMatrixP3Resources,
    ) -> Result<Self, WrfTMatrixBundleError> {
        Ok(Self {
            tables,
            validated: validate_bundle(tables)?,
            p3: Some(p3),
        })
    }

    pub fn evaluate(
        &self,
        raw: &RawPropertyCell,
        elevation_deg: f64,
    ) -> Result<Option<PolarAccumulatorQuantities>, WrfTMatrixRawEvaluationError> {
        self.evaluate_with_cuda(raw, elevation_deg, None)
    }

    /// Evaluate one raw property cell through the same CPU admission and
    /// reduction path while optionally offloading admitted dry LUT nodes to a
    /// job-scoped CUDA batching service. Any CUDA preparation or execution
    /// failure disables that service and replays the complete affected PSD on
    /// the CPU; no partial GPU result is mixed into a category.
    pub fn evaluate_with_cuda(
        &self,
        raw: &RawPropertyCell,
        elevation_deg: f64,
        cuda: Option<&WrfTMatrixCudaBatchService>,
    ) -> Result<Option<PolarAccumulatorQuantities>, WrfTMatrixRawEvaluationError> {
        self.evaluate_with_cuda_and_cancel(raw, elevation_deg, cuda, None)
    }

    /// Cancellation-aware form used by long-running synthetic-radar jobs.
    /// Cancellation is checked before CUDA-to-CPU replay so a user stop does
    /// not masquerade as an accelerator failure or launch expensive replay.
    pub fn evaluate_with_cuda_and_cancel(
        &self,
        raw: &RawPropertyCell,
        elevation_deg: f64,
        cuda: Option<&WrfTMatrixCudaBatchService>,
        cancel: Option<&AtomicBool>,
    ) -> Result<Option<PolarAccumulatorQuantities>, WrfTMatrixRawEvaluationError> {
        check_cancel(cancel).map_err(|()| WrfTMatrixRawEvaluationError::Cancelled)?;
        match raw.microphysics_scheme_id() {
            50..=53 => {
                let p3 = self.p3.as_ref().ok_or_else(|| raw_p3_table_required(raw))?;
                validate_raw_p3_table(raw.microphysics_scheme_id(), p3.table.as_ref())?;
                evaluate_p3_psd_raw_cell(
                    raw,
                    self.tables,
                    self.validated,
                    p3,
                    elevation_deg,
                    cuda,
                    cancel,
                )
            }
            55 => evaluate_ishmael_psd_raw_cell(
                raw,
                self.tables,
                self.validated,
                elevation_deg,
                cuda,
                cancel,
            ),
            scheme_id => Err(WrfTMatrixRawEvaluationError::UnsupportedScheme { scheme_id }),
        }
    }

    /// Prepare many already-blended raw gate samples, evaluate all compatible
    /// dry LUT nodes in two cut-wide CUDA role sweeps, and then finish every
    /// request in the input order on the CPU.
    ///
    /// CUDA answers are not installed until both role sweeps succeed. Any
    /// descriptor or execution failure discards the complete chunk and replays
    /// every retained preparation on the scalar path. Preparation and finish
    /// failures are reported by the first input position, never by Rayon or GPU
    /// completion order.
    pub fn evaluate_batch_with_cuda_and_cancel(
        &self,
        requests: &[WrfTMatrixRawBatchRequest<'_>],
        cuda: Option<&WrfTMatrixCudaBatchService>,
        cancel: Option<&AtomicBool>,
    ) -> Result<Vec<Option<PolarAccumulatorQuantities>>, WrfTMatrixRawBatchError> {
        check_cancel(cancel).map_err(|()| WrfTMatrixRawBatchError::Cancelled)?;
        let prepared = requests
            .par_iter()
            .map(|request| self.prepare_batch_request(*request, cancel))
            .collect::<Vec<_>>();
        // Cancellation is deliberately checked before selecting a science
        // error from the indexed preparation slots.
        check_cancel(cancel).map_err(|()| WrfTMatrixRawBatchError::Cancelled)?;
        let mut ordered = Vec::with_capacity(prepared.len());
        for (request_index, prepared) in prepared.into_iter().enumerate() {
            match prepared {
                Ok(prepared) => ordered.push(prepared),
                Err(WrfTMatrixRawEvaluationError::Cancelled) => {
                    return Err(WrfTMatrixRawBatchError::Cancelled);
                }
                Err(source) => {
                    return Err(WrfTMatrixRawBatchError::Request {
                        request_index,
                        source,
                    });
                }
            }
        }
        finish_raw_batch(ordered, cuda, cancel)
    }

    fn prepare_batch_request<'table>(
        &'table self,
        request: WrfTMatrixRawBatchRequest<'_>,
        cancel: Option<&AtomicBool>,
    ) -> Result<PreparedRawBatchEvaluation<'table>, WrfTMatrixRawEvaluationError> {
        check_cancel(cancel).map_err(|()| WrfTMatrixRawEvaluationError::Cancelled)?;
        match request.raw.microphysics_scheme_id() {
            50..=53 => {
                let p3 = self
                    .p3
                    .as_ref()
                    .ok_or_else(|| raw_p3_table_required(request.raw))?;
                validate_raw_p3_table(request.raw.microphysics_scheme_id(), p3.table.as_ref())?;
                prepare_p3_raw_batch_evaluation(
                    request.raw,
                    self.tables,
                    self.validated,
                    p3,
                    request.elevation_deg,
                    cancel,
                )
            }
            55 => prepare_ishmael_raw_batch_evaluation(
                request.raw,
                self.tables,
                self.validated,
                request.elevation_deg,
                cancel,
            ),
            scheme_id => Err(WrfTMatrixRawEvaluationError::UnsupportedScheme { scheme_id }),
        }
    }
}

/// Whether rain and diagnosed mixed-phase coexistence are required.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WrfTMatrixRainMode {
    /// Require the WRF rain mass and number fields, evaluate standalone rain,
    /// and diagnose wet frozen/rain coexistence inside the exact envelope.
    #[default]
    FullProperty,
    /// Deliberately omit all rain and wet-coexistence scattering. Frozen
    /// categories remain dry. This is explicit, never an automatic fallback.
    FrozenOnly,
}

/// Frozen-scattering runtime policy. Strict mode never falls back. Hybrid is
/// an explicit, versioned cell-level policy and is always visible in scene
/// provenance and audit counts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WrfTMatrixScatteringPolicy {
    #[default]
    StrictFailClosed,
    HybridBulkRayleighV1,
}

impl WrfTMatrixScatteringPolicy {
    #[must_use]
    pub const fn identifier(self) -> &'static str {
        match self {
            Self::StrictFailClosed => "full-property-tmatrix-strict-fail-closed-v1",
            Self::HybridBulkRayleighV1 => PROPERTY_HYBRID_BULK_RAYLEIGH_REVISION,
        }
    }
}

/// Additive-population counts, independent of the number of elevation nodes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WrfTMatrixAuditCounts {
    pub source_cells: u64,
    /// Retained only for reading older audits. New P3 scenes never increment
    /// the characteristic-particle count.
    pub characteristic_frozen_populations: u64,
    /// P3 or ISHMAEL category distributions integrated from native moments.
    pub scheme_native_psd_populations: u64,
    pub dry_frozen_populations: u64,
    pub wet_frozen_populations: u64,
    pub residual_rain_populations: u64,
    pub standalone_rain_populations: u64,
    /// Complete frozen cells rebuilt through the explicit Hybrid policy after
    /// a native table-domain/shape omission or typed ISHMAEL source-state
    /// mass-closure gap.
    pub hybrid_bulk_rayleigh_cells: u64,
    /// Positive native frozen categories represented by those rebuilt cells.
    pub hybrid_bulk_rayleigh_populations: u64,
}

impl WrfTMatrixAuditCounts {
    fn checked_add(self, other: Self) -> Option<Self> {
        Some(Self {
            source_cells: self.source_cells.checked_add(other.source_cells)?,
            characteristic_frozen_populations: self
                .characteristic_frozen_populations
                .checked_add(other.characteristic_frozen_populations)?,
            scheme_native_psd_populations: self
                .scheme_native_psd_populations
                .checked_add(other.scheme_native_psd_populations)?,
            dry_frozen_populations: self
                .dry_frozen_populations
                .checked_add(other.dry_frozen_populations)?,
            wet_frozen_populations: self
                .wet_frozen_populations
                .checked_add(other.wet_frozen_populations)?,
            residual_rain_populations: self
                .residual_rain_populations
                .checked_add(other.residual_rain_populations)?,
            standalone_rain_populations: self
                .standalone_rain_populations
                .checked_add(other.standalone_rain_populations)?,
            hybrid_bulk_rayleigh_cells: self
                .hybrid_bulk_rayleigh_cells
                .checked_add(other.hybrid_bulk_rayleigh_cells)?,
            hybrid_bulk_rayleigh_populations: self
                .hybrid_bulk_rayleigh_populations
                .checked_add(other.hybrid_bulk_rayleigh_populations)?,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WrfTMatrixTableAudit {
    pub role: WrfTMatrixTableRole,
    pub table_id: String,
    pub file_sha256: Sha256Digest,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WrfTMatrixSceneProvenance {
    pub status: &'static str,
    pub frequency_hz: f64,
    pub orientation: OrientationDefinition,
    pub frozen_scattering: WrfTMatrixFrozenScatteringAudit,
    pub fall_moment_policy: WrfTMatrixFallMomentAudit,
    pub rain_mode: WrfTMatrixRainMode,
    pub scattering_policy: WrfTMatrixScatteringPolicy,
    pub tables: [WrfTMatrixTableAudit; 5],
    pub counts: WrfTMatrixAuditCounts,
}

/// Frozen-particle representation used by this source scene.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WrfTMatrixFrozenScatteringAudit {
    /// Legacy audit value retained for serialized/in-memory compatibility.
    /// New P3 production dispatch never emits this variant.
    P3CharacteristicParticleV1,
    /// Lambda/mu and PSD weights come from the exact pinned P3 table. Shape is
    /// separately qualified: strict mode evaluates only scheme-native spheres;
    /// research mode uses the named area/mass spheroid mapping and the declared
    /// exact-small-sphere Rayleigh-limit bridge below the table floor.
    P3SchemeNativePsdV1 {
        p3_psd_revision: &'static str,
        table_reader_revision: &'static str,
        integration_revision: &'static str,
        small_sphere_scattering_revision: &'static str,
        wrf_source_commit: &'static str,
        p3_module_version: &'static str,
        table_kind: P3OfficialTableKind,
        table_sha256: Sha256Digest,
        integration: P3TMatrixIntegrationConfig,
        shape_authority: P3ShapeAuthority,
        limitation: &'static str,
    },
    /// Native ISHMAEL gamma PSD integrated through dry particle-node tables.
    /// Wet PSD allocation is deliberately unavailable rather than replaced by
    /// the old characteristic-particle coexistence approximation.
    IshmaelSchemeNativePsdDryFrozenV1 {
        scheme_psd_revision: &'static str,
        ishmael_reconstruction_revision: &'static str,
        solid_ice_material_closure_revision: &'static str,
        small_sphere_scattering_revision: &'static str,
        authenticated_ice_material_density_kg_m3: f64,
        integration: PsdIntegrationConfig,
        support: PsdParticleSupport,
        fall_speed: PsdFallSpeedProvenance,
    },
}

/// Runtime Doppler fall-moment policies retained with the scene audit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WrfTMatrixFallMomentAudit {
    /// Dry frozen, standalone rain, and residual rain.
    pub closed_category: FallMomentPolicy,
    /// Wet frozen populations diagnosed by DiagnosticCoexistenceV1.
    pub diagnostic_wet_category: FallMomentPolicy,
}

/// Logical retained-memory estimate; allocator slack is intentionally absent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WrfTMatrixSceneMemoryEstimate {
    pub structure_bytes: usize,
    pub source_index_bytes: usize,
    pub dense_row_lookup_bytes: usize,
    pub elevation_axis_bytes: usize,
    pub additive_component_bytes: usize,
    pub source_identity_bytes: usize,
    pub required_field_contract_bytes: usize,
    pub source_field_provenance_bytes: usize,
    pub provenance_text_bytes: usize,
}

impl WrfTMatrixSceneMemoryEstimate {
    #[must_use]
    pub const fn retained_bytes(self) -> usize {
        self.structure_bytes
            .saturating_add(self.source_index_bytes)
            .saturating_add(self.dense_row_lookup_bytes)
            .saturating_add(self.elevation_axis_bytes)
            .saturating_add(self.additive_component_bytes)
            .saturating_add(self.source_identity_bytes)
            .saturating_add(self.required_field_contract_bytes)
            .saturating_add(self.source_field_provenance_bytes)
            .saturating_add(self.provenance_text_bytes)
    }
}

/// Conservative peak for memory owned directly by property-scene building.
///
/// It includes the already-retained [`WrfPropertyScene`], the worst-case flat
/// component buffer (every active source cell retained), dense and sparse
/// lookup storage, FrozenOnly filter summaries, cloned scene metadata, and a
/// per-Rayon-worker allowance for one borrowed output chunk plus transient
/// closed-category/coexistence state. The per-worker allowance deliberately
/// double-counts the borrowed chunk to remain conservative.
///
/// It does not include LUT bytes (borrowed and expected to be resident before
/// this build), allocator bookkeeping/slack, Rayon thread stacks/runtime
/// internals, NetCDF decoder caches, or allocations owned by the caller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WrfTMatrixBuildPeakEstimate {
    pub source_scene_retained_bytes: usize,
    pub active_source_cells: usize,
    pub radar_elevation_nodes: usize,
    pub parallel_workers: usize,
    pub component_buffer_bytes: usize,
    pub dense_lookup_bytes: usize,
    pub sparse_index_bytes: usize,
    pub frozen_filter_summary_bytes: usize,
    pub scene_metadata_bytes: usize,
    pub worker_chunk_and_closure_scratch_bytes: usize,
    pub estimated_peak_bytes: usize,
}

impl WrfTMatrixBuildPeakEstimate {
    #[must_use]
    pub const fn retained_scene_upper_bound_bytes(self) -> usize {
        self.component_buffer_bytes
            .saturating_add(self.dense_lookup_bytes)
            .saturating_add(self.sparse_index_bytes)
            .saturating_add(self.scene_metadata_bytes)
    }
}

/// Sparse precomputed additive scattering for one WRF property scene.
#[derive(Clone, Debug, PartialEq)]
pub struct WrfTMatrixScene {
    source_identity: PropertySceneIdentity,
    microphysics_scheme_id: i32,
    required_field_signature: RequiredFieldSignature,
    source_fields: Vec<SourceFieldProvenance>,
    source_cell_count: usize,
    source_cell_indices: Vec<u32>,
    /// O(1) full-cell to compact-row lookup; u32::MAX means clear/omitted.
    full_cell_to_compact_row: Vec<u32>,
    radar_elevations_deg: Vec<f64>,
    /// Cell-major, then elevation-major, then canonical nine components.
    additive_components: Vec<f32>,
    provenance: WrfTMatrixSceneProvenance,
}

impl WrfTMatrixScene {
    /// Build the complete property scene. Missing rain inputs are errors.
    pub fn build(
        source: &WrfPropertyScene,
        tables: WrfTMatrixLutBundle<'_>,
    ) -> Result<Self, WrfTMatrixSceneBuildError> {
        Self::build_internal(
            source,
            tables,
            WrfTMatrixRainMode::FullProperty,
            WrfTMatrixScatteringPolicy::StrictFailClosed,
            None,
            None,
            None,
        )
    }

    pub fn build_with_rain_mode(
        source: &WrfPropertyScene,
        tables: WrfTMatrixLutBundle<'_>,
        rain_mode: WrfTMatrixRainMode,
    ) -> Result<Self, WrfTMatrixSceneBuildError> {
        Self::build_internal(
            source,
            tables,
            rain_mode,
            WrfTMatrixScatteringPolicy::StrictFailClosed,
            None,
            None,
            None,
        )
    }

    pub fn build_with_rain_mode_and_cuda(
        source: &WrfPropertyScene,
        tables: WrfTMatrixLutBundle<'_>,
        rain_mode: WrfTMatrixRainMode,
        cuda: &WrfTMatrixCudaBatchService,
    ) -> Result<Self, WrfTMatrixSceneBuildError> {
        Self::build_internal(
            source,
            tables,
            rain_mode,
            WrfTMatrixScatteringPolicy::StrictFailClosed,
            None,
            Some(cuda),
            None,
        )
    }

    pub fn build_with_rain_mode_and_cuda_and_cancel(
        source: &WrfPropertyScene,
        tables: WrfTMatrixLutBundle<'_>,
        rain_mode: WrfTMatrixRainMode,
        cuda: &WrfTMatrixCudaBatchService,
        cancel: &AtomicBool,
    ) -> Result<Self, WrfTMatrixSceneBuildError> {
        Self::build_internal(
            source,
            tables,
            rain_mode,
            WrfTMatrixScatteringPolicy::StrictFailClosed,
            None,
            Some(cuda),
            Some(cancel),
        )
    }

    pub fn build_with_rain_mode_and_optional_cuda_and_cancel(
        source: &WrfPropertyScene,
        tables: WrfTMatrixLutBundle<'_>,
        rain_mode: WrfTMatrixRainMode,
        cuda: Option<&WrfTMatrixCudaBatchService>,
        cancel: &AtomicBool,
    ) -> Result<Self, WrfTMatrixSceneBuildError> {
        Self::build_internal(
            source,
            tables,
            rain_mode,
            WrfTMatrixScatteringPolicy::StrictFailClosed,
            None,
            cuda,
            Some(cancel),
        )
    }

    pub fn build_with_p3(
        source: &WrfPropertyScene,
        tables: WrfTMatrixLutBundle<'_>,
        p3: WrfTMatrixP3Resources,
    ) -> Result<Self, WrfTMatrixSceneBuildError> {
        Self::build_internal(
            source,
            tables,
            WrfTMatrixRainMode::FullProperty,
            WrfTMatrixScatteringPolicy::StrictFailClosed,
            Some(p3),
            None,
            None,
        )
    }

    pub fn build_with_p3_and_rain_mode(
        source: &WrfPropertyScene,
        tables: WrfTMatrixLutBundle<'_>,
        p3: WrfTMatrixP3Resources,
        rain_mode: WrfTMatrixRainMode,
    ) -> Result<Self, WrfTMatrixSceneBuildError> {
        Self::build_internal(
            source,
            tables,
            rain_mode,
            WrfTMatrixScatteringPolicy::StrictFailClosed,
            Some(p3),
            None,
            None,
        )
    }

    pub fn build_with_p3_and_rain_mode_and_cuda(
        source: &WrfPropertyScene,
        tables: WrfTMatrixLutBundle<'_>,
        p3: WrfTMatrixP3Resources,
        rain_mode: WrfTMatrixRainMode,
        cuda: &WrfTMatrixCudaBatchService,
    ) -> Result<Self, WrfTMatrixSceneBuildError> {
        Self::build_internal(
            source,
            tables,
            rain_mode,
            WrfTMatrixScatteringPolicy::StrictFailClosed,
            Some(p3),
            Some(cuda),
            None,
        )
    }

    pub fn build_with_p3_and_rain_mode_and_cuda_and_cancel(
        source: &WrfPropertyScene,
        tables: WrfTMatrixLutBundle<'_>,
        p3: WrfTMatrixP3Resources,
        rain_mode: WrfTMatrixRainMode,
        cuda: Option<&WrfTMatrixCudaBatchService>,
        cancel: &AtomicBool,
    ) -> Result<Self, WrfTMatrixSceneBuildError> {
        Self::build_internal(
            source,
            tables,
            rain_mode,
            WrfTMatrixScatteringPolicy::StrictFailClosed,
            Some(p3),
            cuda,
            Some(cancel),
        )
    }

    /// Build with an explicit runtime scattering policy. This is the sole
    /// production seam that can enable the audited Hybrid bulk fallback.
    #[allow(clippy::too_many_arguments)]
    pub fn build_with_scattering_policy(
        source: &WrfPropertyScene,
        tables: WrfTMatrixLutBundle<'_>,
        rain_mode: WrfTMatrixRainMode,
        scattering_policy: WrfTMatrixScatteringPolicy,
        p3: Option<WrfTMatrixP3Resources>,
        cuda: Option<&WrfTMatrixCudaBatchService>,
        cancel: Option<&AtomicBool>,
    ) -> Result<Self, WrfTMatrixSceneBuildError> {
        Self::build_internal(
            source,
            tables,
            rain_mode,
            scattering_policy,
            p3,
            cuda,
            cancel,
        )
    }

    fn build_internal(
        source: &WrfPropertyScene,
        tables: WrfTMatrixLutBundle<'_>,
        rain_mode: WrfTMatrixRainMode,
        scattering_policy: WrfTMatrixScatteringPolicy,
        p3: Option<WrfTMatrixP3Resources>,
        cuda: Option<&WrfTMatrixCudaBatchService>,
        cancel: Option<&AtomicBool>,
    ) -> Result<Self, WrfTMatrixSceneBuildError> {
        check_cancel(cancel).map_err(|()| WrfTMatrixSceneBuildError::Cancelled)?;
        let validated = validate_bundle(tables)?;
        validate_source_shape(source)?;
        let p3 = validate_scene_p3_resources(source.microphysics_scheme_id(), p3)?;
        let elevations = validated.radar_elevations_deg;
        let components_per_cell = elevations
            .len()
            .checked_mul(AdditiveScattering::COMPONENT_COUNT)
            .ok_or(WrfTMatrixSceneBuildError::SizeOverflow)?;
        let total_components = source
            .active_cell_indices()
            .len()
            .checked_mul(components_per_cell)
            .ok_or(WrfTMatrixSceneBuildError::SizeOverflow)?;
        let mut additive_components = vec![0.0_f32; total_components];

        let (source_cell_indices, counts) = match rain_mode {
            WrfTMatrixRainMode::FullProperty => {
                let counts = additive_components
                    .par_chunks_mut(components_per_cell)
                    .zip(source.active_cell_indices().par_iter())
                    .try_fold(
                        WrfTMatrixAuditCounts::default,
                        |counts, (output, &cell_index)| {
                            let summary = build_cell_into(
                                source,
                                cell_index as usize,
                                tables,
                                validated,
                                p3.as_ref(),
                                rain_mode,
                                scattering_policy,
                                cuda,
                                cancel,
                                output,
                            )?;
                            if !summary.retained {
                                return Err(WrfTMatrixSceneBuildError::UnexpectedClearActiveCell {
                                    cell_index: cell_index as usize,
                                });
                            }
                            counts
                                .checked_add(summary.counts)
                                .ok_or(WrfTMatrixSceneBuildError::AuditCountOverflow)
                        },
                    )
                    .try_reduce(WrfTMatrixAuditCounts::default, |left, right| {
                        left.checked_add(right)
                            .ok_or(WrfTMatrixSceneBuildError::AuditCountOverflow)
                    })?;
                (source.active_cell_indices().to_vec(), counts)
            }
            WrfTMatrixRainMode::FrozenOnly => {
                let summaries = additive_components
                    .par_chunks_mut(components_per_cell)
                    .zip(source.active_cell_indices().par_iter())
                    .map(|(output, &cell_index)| {
                        build_cell_into(
                            source,
                            cell_index as usize,
                            tables,
                            validated,
                            p3.as_ref(),
                            rain_mode,
                            scattering_policy,
                            cuda,
                            cancel,
                            output,
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                compact_frozen_only_rows(
                    &mut additive_components,
                    components_per_cell,
                    source.active_cell_indices(),
                    summaries,
                )?
            }
        };
        check_cancel(cancel).map_err(|()| WrfTMatrixSceneBuildError::Cancelled)?;
        let mut full_cell_to_compact_row = vec![u32::MAX; source.cell_count()];
        for (compact_row, &source_cell) in source_cell_indices.iter().enumerate() {
            let compact_row =
                u32::try_from(compact_row).map_err(|_| WrfTMatrixSceneBuildError::SizeOverflow)?;
            if compact_row == u32::MAX {
                return Err(WrfTMatrixSceneBuildError::SizeOverflow);
            }
            full_cell_to_compact_row[source_cell as usize] = compact_row;
        }

        let table_audits = tables.entries().map(|(role, table)| WrfTMatrixTableAudit {
            role,
            table_id: table.descriptor().table_id().to_owned(),
            file_sha256: table.file_sha256(),
        });
        Ok(Self {
            source_identity: source.identity().clone(),
            microphysics_scheme_id: source.microphysics_scheme_id(),
            required_field_signature: source.required_field_signature().clone(),
            source_fields: source.source_fields().to_vec(),
            source_cell_count: source.cell_count(),
            source_cell_indices,
            full_cell_to_compact_row,
            radar_elevations_deg: elevations.to_vec(),
            additive_components,
            provenance: WrfTMatrixSceneProvenance {
                status: "research_only_unvalidated",
                frequency_hz: validated.frequency_hz,
                orientation: OrientationDefinition::Gaussian20Research,
                frozen_scattering: if source.microphysics_scheme_id() == 55 {
                    WrfTMatrixFrozenScatteringAudit::IshmaelSchemeNativePsdDryFrozenV1 {
                        scheme_psd_revision: SCHEME_PSD_REVISION,
                        ishmael_reconstruction_revision: ISHMAEL_PSD_REVISION,
                        solid_ice_material_closure_revision:
                            ISHMAEL_SOLID_ICE_MATERIAL_CLOSURE_REVISION,
                        small_sphere_scattering_revision:
                            ISHMAEL_SMALL_SPHERE_RAYLEIGH_BRIDGE_REVISION,
                        authenticated_ice_material_density_kg_m3: validated
                            .ishmael_ice_material_density_kg_m3,
                        integration: validated.ishmael_psd_config,
                        support: validated.ishmael_particle_support,
                        fall_speed: validated.ishmael_fall_speed,
                    }
                } else {
                    let p3 = p3
                        .as_ref()
                        .expect("P3 resources were validated before scene allocation");
                    let integration_revision = match p3.integration.shape_policy() {
                        P3TMatrixShapePolicy::StrictShapeAuthoritativeSpheres => {
                            P3_SPHERICAL_INTEGRATION_REVISION
                        }
                        P3TMatrixShapePolicy::ProjectedAreaEquivalentOblateGaussian20ResearchV1 => {
                            P3_PROJECTED_AREA_EQUIVALENT_OBLATE_REVISION
                        }
                        P3TMatrixShapePolicy::ProjectedAreaEquivalentSpheroidGaussian20ResearchV1 => {
                            P3_PROJECTED_AREA_EQUIVALENT_SPHEROID_REVISION
                        }
                    };
                    WrfTMatrixFrozenScatteringAudit::P3SchemeNativePsdV1 {
                        p3_psd_revision: P3_PSD_REVISION,
                        table_reader_revision: P3_TABLE_READER_REVISION,
                        integration_revision,
                        small_sphere_scattering_revision:
                            P3_SMALL_SPHERE_RAYLEIGH_BRIDGE_REVISION,
                        wrf_source_commit: P3_WRF_SOURCE_COMMIT,
                        p3_module_version: P3_MODULE_VERSION,
                        table_kind: p3.table.kind(),
                        table_sha256: p3.table.descriptor().table_sha256,
                        integration: p3.integration,
                        shape_authority: P3ShapeAuthority::MaximumDimensionAndProjectedAreaOnly,
                        limitation: match p3.integration.shape_policy() {
                            P3TMatrixShapePolicy::StrictShapeAuthoritativeSpheres => {
                                "analytic P3 closure is retained; radar moments use the pinned v5.4 lookup integration domain through 80 mm with its excluded tail audited separately; only regions explicitly defined as spheres are evaluated and in-source nonspherical nodes are omitted under strict budgets"
                            }
                            P3TMatrixShapePolicy::ProjectedAreaEquivalentOblateGaussian20ResearchV1 => {
                                "lambda/mu and analytic closure are exact P3; radar moments use the pinned v5.4 lookup integration domain through 80 mm with its excluded tail audited separately and without renormalization; oblate shape is projected-area-equivalent and Gaussian-20 canting is an external research assumption, not P3-predicted habit or orientation"
                            }
                            P3TMatrixShapePolicy::ProjectedAreaEquivalentSpheroidGaussian20ResearchV1 => {
                                "lambda/mu and analytic closure are exact P3; radar moments use the pinned v5.4 lookup integration domain through 80 mm with its excluded tail audited separately and without renormalization; the area-equivalent spheroid preserves mass and uses a continuous 900 kg/m3 source-solid-ice construction where the empirical area-law transition cannot define a physical homogeneous volume; a typed projected-area overshoot caused by P3's stale final fixed-point breakpoint is sphere-closed at the true Dmax while its raw area and radar weight remain audited; exact P3 small dense spheres below the 50 um T-matrix floor use the separately versioned Rayleigh-limit bridge; Gaussian-20 canting and nonspherical habit remain external research assumptions"
                            }
                        },
                    }
                },
                fall_moment_policy: WrfTMatrixFallMomentAudit {
                    closed_category:
                        FallMomentPolicy::ClosedCategoryPositiveDownZeroWithinCategoryVariance,
                    diagnostic_wet_category:
                        FallMomentPolicy::DiagnosticWetCategoryPositiveDownZeroWithinCategoryVariance,
                },
                rain_mode,
                scattering_policy,
                tables: table_audits,
                counts,
            },
        })
    }

    /// Estimate the upper-bound peak of memory directly owned by this build
    /// before allocating the output component plane.
    pub fn estimate_build_peak(
        source: &WrfPropertyScene,
        tables: WrfTMatrixLutBundle<'_>,
        rain_mode: WrfTMatrixRainMode,
    ) -> Result<WrfTMatrixBuildPeakEstimate, WrfTMatrixSceneBuildError> {
        Self::estimate_build_peak_internal(source, tables, rain_mode, None)
    }

    pub fn estimate_build_peak_with_p3(
        source: &WrfPropertyScene,
        tables: WrfTMatrixLutBundle<'_>,
        rain_mode: WrfTMatrixRainMode,
        p3: &WrfTMatrixP3Resources,
    ) -> Result<WrfTMatrixBuildPeakEstimate, WrfTMatrixSceneBuildError> {
        Self::estimate_build_peak_internal(source, tables, rain_mode, Some(p3))
    }

    fn estimate_build_peak_internal(
        source: &WrfPropertyScene,
        tables: WrfTMatrixLutBundle<'_>,
        rain_mode: WrfTMatrixRainMode,
        p3: Option<&WrfTMatrixP3Resources>,
    ) -> Result<WrfTMatrixBuildPeakEstimate, WrfTMatrixSceneBuildError> {
        let validated = validate_bundle(tables)?;
        validate_source_shape(source)?;
        validate_scene_p3_resource_ref(source.microphysics_scheme_id(), p3)?;
        estimate_build_peak_from_shape(
            source,
            validated.radar_elevations_deg.len(),
            validated.table_id_text_bytes,
            rain_mode,
            rayon::current_num_threads().max(1),
        )
    }

    #[must_use]
    pub const fn source_identity(&self) -> &PropertySceneIdentity {
        &self.source_identity
    }

    #[must_use]
    pub const fn microphysics_scheme_id(&self) -> i32 {
        self.microphysics_scheme_id
    }

    #[must_use]
    pub const fn required_field_signature(&self) -> &RequiredFieldSignature {
        &self.required_field_signature
    }

    #[must_use]
    pub fn source_fields(&self) -> &[SourceFieldProvenance] {
        &self.source_fields
    }

    /// Temporal compatibility retained after the raw property arrays have
    /// been dropped. The caller supplies its actual renderer/LUT label.
    #[must_use]
    pub fn temporal_signature(
        &self,
        reflectivity_source: impl Into<String>,
    ) -> ScenePropertySignature {
        let reflectivity_source = reflectivity_source.into();
        ScenePropertySignature {
            microphysics_scheme_id: Some(self.microphysics_scheme_id),
            reflectivity_source: format!(
                "{reflectivity_source};tmatrix_frequency_hz={:.0};tmatrix_frequency_bits={:016x}",
                self.provenance.frequency_hz,
                self.provenance.frequency_hz.to_bits()
            ),
            required_raw_fields: self.required_field_signature.field_names(),
        }
    }

    #[must_use]
    pub const fn source_cell_count(&self) -> usize {
        self.source_cell_count
    }

    #[must_use]
    pub fn active_cell_indices(&self) -> &[u32] {
        &self.source_cell_indices
    }

    #[must_use]
    pub fn radar_elevations_deg(&self) -> &[f64] {
        &self.radar_elevations_deg
    }

    #[must_use]
    pub const fn provenance(&self) -> &WrfTMatrixSceneProvenance {
        &self.provenance
    }

    /// Interpolate only additive quantities at an arbitrary in-range beam
    /// elevation. A clear/non-active source cell returns `Ok(None)`.
    pub fn additive_at(
        &self,
        full_cell_index: usize,
        beam_elevation_deg: f64,
    ) -> Result<Option<AdditiveScattering>, WrfTMatrixSceneQueryError> {
        if full_cell_index >= self.source_cell_count {
            return Err(WrfTMatrixSceneQueryError::CellOutOfRange {
                cell_index: full_cell_index,
                cell_count: self.source_cell_count,
            });
        }
        let compact_cell = *self
            .full_cell_to_compact_row
            .get(full_cell_index)
            .ok_or(WrfTMatrixSceneQueryError::CorruptStorage)?;
        if compact_cell == u32::MAX {
            return Ok(None);
        }
        let compact_cell = compact_cell as usize;
        let bracket = elevation_bracket(&self.radar_elevations_deg, beam_elevation_deg)?;
        let lower = self.component_row(compact_cell, bracket.lower)?;
        if bracket.lower == bracket.upper {
            return decode_components(lower).map(Some);
        }
        let upper = self.component_row(compact_cell, bracket.upper)?;
        let mut interpolated = [0.0; AdditiveScattering::COMPONENT_COUNT];
        for component in 0..AdditiveScattering::COMPONENT_COUNT {
            interpolated[component] = f64::from(lower[component])
                + bracket.fraction * (f64::from(upper[component]) - f64::from(lower[component]));
        }
        AdditiveScattering::from_components(interpolated)
            .map(Some)
            .map_err(WrfTMatrixSceneQueryError::Output)
    }

    pub fn polar_at(
        &self,
        full_cell_index: usize,
        beam_elevation_deg: f64,
    ) -> Result<Option<PolarAccumulatorQuantities>, WrfTMatrixSceneQueryError> {
        self.additive_at(full_cell_index, beam_elevation_deg)?
            .map(AdditiveScattering::to_polar_accumulator_quantities)
            .transpose()
            .map_err(WrfTMatrixSceneQueryError::Output)
    }

    /// Interpolate and accumulate one normalized spatial stencil.
    ///
    /// Elevation is bracketed once. Up to eight full-grid cells are then
    /// accumulated directly in the nine additive components and converted to
    /// [`AdditiveScattering`] once. Weights must be finite, nonnegative, and
    /// sum to one within `1e-9`. Clear cells contribute exact zero; their
    /// weight is never redistributed to echoing cells.
    #[inline]
    pub fn weighted_additive_at(
        &self,
        contributions: &[(usize, f64)],
        beam_elevation_deg: f64,
    ) -> Result<AdditiveScattering, WrfTMatrixSceneQueryError> {
        if contributions.is_empty() {
            return Err(WrfTMatrixSceneQueryError::NoWeightedContributions);
        }
        if contributions.len() > MAX_WEIGHTED_PROPERTY_CELLS {
            return Err(WrfTMatrixSceneQueryError::TooManyWeightedContributions {
                actual: contributions.len(),
                maximum: MAX_WEIGHTED_PROPERTY_CELLS,
            });
        }
        let bracket = elevation_bracket(&self.radar_elevations_deg, beam_elevation_deg)?;
        let mut weight_sum = 0.0;
        let mut accumulated = [0.0; AdditiveScattering::COMPONENT_COUNT];
        for (contribution_index, &(full_cell_index, weight)) in contributions.iter().enumerate() {
            if !weight.is_finite() || weight < 0.0 {
                return Err(WrfTMatrixSceneQueryError::InvalidWeight {
                    contribution_index,
                    weight,
                });
            }
            if full_cell_index >= self.source_cell_count {
                return Err(WrfTMatrixSceneQueryError::CellOutOfRange {
                    cell_index: full_cell_index,
                    cell_count: self.source_cell_count,
                });
            }
            weight_sum += weight;
            let compact_cell = *self
                .full_cell_to_compact_row
                .get(full_cell_index)
                .ok_or(WrfTMatrixSceneQueryError::CorruptStorage)?;
            if compact_cell == u32::MAX || weight == 0.0 {
                continue;
            }
            let compact_cell = compact_cell as usize;
            let lower = self.component_row(compact_cell, bracket.lower)?;
            if bracket.lower == bracket.upper {
                for component in 0..AdditiveScattering::COMPONENT_COUNT {
                    accumulated[component] += weight * f64::from(lower[component]);
                }
                continue;
            }
            let upper = self.component_row(compact_cell, bracket.upper)?;
            for component in 0..AdditiveScattering::COMPONENT_COUNT {
                let interpolated = f64::from(lower[component])
                    + bracket.fraction
                        * (f64::from(upper[component]) - f64::from(lower[component]));
                accumulated[component] += weight * interpolated;
            }
        }
        if (weight_sum - 1.0).abs() > WEIGHT_SUM_TOLERANCE {
            return Err(WrfTMatrixSceneQueryError::WeightSum { sum: weight_sum });
        }
        AdditiveScattering::from_components(accumulated).map_err(WrfTMatrixSceneQueryError::Output)
    }

    #[inline]
    pub fn weighted_polar_at(
        &self,
        contributions: &[(usize, f64)],
        beam_elevation_deg: f64,
    ) -> Result<PolarAccumulatorQuantities, WrfTMatrixSceneQueryError> {
        self.weighted_additive_at(contributions, beam_elevation_deg)?
            .to_polar_accumulator_quantities()
            .map_err(WrfTMatrixSceneQueryError::Output)
    }

    #[must_use]
    pub fn memory_estimate(&self) -> WrfTMatrixSceneMemoryEstimate {
        let required_field_contract_bytes = self.required_field_signature.fields.len()
            * size_of::<crate::wrf_property_reader::RequiredFieldContract>();
        let source_field_provenance_bytes =
            self.source_fields.len() * size_of::<SourceFieldProvenance>();
        let provenance_text_bytes = self
            .source_fields
            .iter()
            .map(|field| field.source_units().len())
            .sum::<usize>()
            + self
                .provenance
                .tables
                .iter()
                .map(|table| table.table_id.len())
                .sum::<usize>();
        WrfTMatrixSceneMemoryEstimate {
            structure_bytes: size_of::<Self>(),
            source_index_bytes: self.source_cell_indices.len() * size_of::<u32>(),
            dense_row_lookup_bytes: self.full_cell_to_compact_row.len() * size_of::<u32>(),
            elevation_axis_bytes: self.radar_elevations_deg.len() * size_of::<f64>(),
            additive_component_bytes: self.additive_components.len() * size_of::<f32>(),
            source_identity_bytes: self.source_identity.source_identity.0.len(),
            required_field_contract_bytes,
            source_field_provenance_bytes,
            provenance_text_bytes,
        }
    }

    #[inline]
    fn component_row(
        &self,
        compact_cell: usize,
        elevation_index: usize,
    ) -> Result<&[f32], WrfTMatrixSceneQueryError> {
        let row = compact_cell
            .checked_mul(self.radar_elevations_deg.len())
            .and_then(|value| value.checked_add(elevation_index))
            .and_then(|value| value.checked_mul(AdditiveScattering::COMPONENT_COUNT))
            .ok_or(WrfTMatrixSceneQueryError::CorruptStorage)?;
        let end = row
            .checked_add(AdditiveScattering::COMPONENT_COUNT)
            .ok_or(WrfTMatrixSceneQueryError::CorruptStorage)?;
        self.additive_components
            .get(row..end)
            .ok_or(WrfTMatrixSceneQueryError::CorruptStorage)
    }
}

#[derive(Clone, Copy)]
struct ValidatedBundle<'a> {
    frequency_hz: f64,
    radar_elevations_deg: &'a [f64],
    table_id_text_bytes: usize,
    p3_particle_support: PsdParticleSupport,
    p3_fall_speed: PsdFallSpeedProvenance,
    ishmael_psd_config: PsdIntegrationConfig,
    ishmael_particle_support: PsdParticleSupport,
    ishmael_fall_speed: PsdFallSpeedProvenance,
    ishmael_ice_material_density_kg_m3: f64,
}

fn production_p3_integration_config(
    shape_policy: P3TMatrixShapePolicy,
) -> Result<P3TMatrixIntegrationConfig, radar_scattering::P3TMatrixIntegrationConfigError> {
    P3TMatrixIntegrationConfig::new(
        shape_policy,
        P3SmallSphereScatteringPolicy::RayleighLimitBelowTableDiameterFloorV1,
        radar_scattering::P3QuadratureConfig::default(),
        P3_MAXIMUM_OMITTED_NUMBER_FRACTION,
        P3_MAXIMUM_OMITTED_MASS_FRACTION,
        P3_MAXIMUM_OMITTED_RADAR_WEIGHT_FRACTION,
    )
}

fn production_ishmael_psd_config() -> PsdIntegrationConfig {
    PsdIntegrationConfig::new(
        8,
        512,
        96.0,
        1.0e-10,
        5.0e-8,
        5.0e-3,
        radar_scattering::DEFAULT_ADDITIVE_ABSOLUTE_TOLERANCES,
        ISHMAEL_PSD_MAXIMUM_OMITTED_NUMBER_FRACTION,
        ISHMAEL_PSD_MAXIMUM_OMITTED_MASS_FRACTION,
        ISHMAEL_PSD_MAXIMUM_OMITTED_D6_FRACTION,
    )
    .expect("the versioned production ISHMAEL PSD config is valid")
    .with_small_sphere_scattering_policy(
        IshmaelSmallSphereScatteringPolicy::RayleighLimitBelowTableDiameterFloorV1,
    )
}

#[derive(Debug)]
struct CellBuildSummary {
    retained: bool,
    counts: WrfTMatrixAuditCounts,
}

#[allow(clippy::too_many_arguments)]
fn build_cell_into(
    source: &WrfPropertyScene,
    cell_index: usize,
    tables: WrfTMatrixLutBundle<'_>,
    validated: ValidatedBundle<'_>,
    p3: Option<&WrfTMatrixP3Resources>,
    rain_mode: WrfTMatrixRainMode,
    scattering_policy: WrfTMatrixScatteringPolicy,
    cuda: Option<&WrfTMatrixCudaBatchService>,
    cancel: Option<&AtomicBool>,
    output: &mut [f32],
) -> Result<CellBuildSummary, WrfTMatrixSceneBuildError> {
    check_cancel(cancel).map_err(|()| WrfTMatrixSceneBuildError::Cancelled)?;
    let strict = if source.microphysics_scheme_id() == 55 {
        build_ishmael_psd_cell_into(
            source, cell_index, tables, validated, rain_mode, cuda, cancel, output,
        )
    } else {
        build_p3_psd_cell_into(
            source,
            cell_index,
            tables,
            validated,
            p3.expect("P3 resources validated before parallel scene build"),
            rain_mode,
            cuda,
            cancel,
            output,
        )
    };
    match strict {
        Err(error)
            if scattering_policy == WrfTMatrixScatteringPolicy::HybridBulkRayleighV1
                && hybrid_eligible_unsupported_cell(&error) =>
        {
            build_hybrid_bulk_rayleigh_cell_into(
                source, cell_index, tables, validated, rain_mode, cancel, output,
            )
        }
        result => result,
    }
}

fn hybrid_eligible_unsupported_cell(error: &WrfTMatrixSceneBuildError) -> bool {
    matches!(
        error,
        WrfTMatrixSceneBuildError::P3PsdIntegration {
            source: P3TMatrixIntegrationError::ShapeOrTableOmission { .. },
            ..
        } | WrfTMatrixSceneBuildError::P3PsdIntegration {
            source: P3TMatrixIntegrationError::Evaluation {
                source: EvaluationError::Interpolation(InterpolationError::OutsideAxis { .. }),
                ..
            },
            ..
        } | WrfTMatrixSceneBuildError::IshmaelPsdIntegration {
            source: PsdIntegrationError::Psd(PsdError::DomainOmission { .. }),
            ..
        } | WrfTMatrixSceneBuildError::IshmaelPsdIntegration {
            source: PsdIntegrationError::Psd(PsdError::SourceStateMassClosure { .. }),
            ..
        } | WrfTMatrixSceneBuildError::IshmaelPsdIntegration {
            source: PsdIntegrationError::NodeEvaluation {
                source: EvaluationError::Interpolation(InterpolationError::OutsideAxis { .. }),
                ..
            },
            ..
        }
    )
}

fn hybrid_positive_f32(
    cell_index: usize,
    category: WrfPropertyCategory,
    field: &'static str,
    value: f64,
) -> Result<f32, WrfTMatrixSceneBuildError> {
    let narrowed = value as f32;
    if value.is_finite() && value > 0.0 && narrowed.is_finite() && narrowed > 0.0 {
        Ok(narrowed)
    } else {
        Err(WrfTMatrixSceneBuildError::HybridBulkRayleighMoment {
            cell_index,
            category,
            field,
            value,
        })
    }
}

fn hybrid_bulk_to_additive(
    cell_index: usize,
    category: WrfPropertyCategory,
    input: BulkSpeciesInput,
) -> Result<AdditiveScattering, WrfTMatrixSceneBuildError> {
    let contribution = property_hybrid_bulk_rayleigh_contribution(input);
    if !contribution.zh.is_finite() || contribution.zh <= 0.0 {
        return Err(WrfTMatrixSceneBuildError::HybridBulkRayleighClear {
            cell_index,
            category,
        });
    }
    let zh = f64::from(contribution.zh);
    let fall_speed = f64::from(contribution.fall_speed_mps);
    let fall_variance = f64::from(contribution.fall_speed_variance_m2s2);
    AdditiveScattering::from_components([
        zh,
        f64::from(contribution.zv),
        f64::from(contribution.cov_re),
        f64::from(contribution.cov_im),
        f64::from(contribution.kdp_deg_km),
        f64::from(contribution.ah_db_km),
        f64::from(contribution.av_db_km),
        zh * fall_speed,
        zh * (fall_variance + fall_speed.powi(2)),
    ])
    .map_err(
        |source| WrfTMatrixSceneBuildError::HybridBulkRayleighOutput {
            cell_index,
            category,
            source,
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn build_hybrid_bulk_rayleigh_cell_into(
    source: &WrfPropertyScene,
    cell_index: usize,
    tables: WrfTMatrixLutBundle<'_>,
    validated: ValidatedBundle<'_>,
    rain_mode: WrfTMatrixRainMode,
    cancel: Option<&AtomicBool>,
    output: &mut [f32],
) -> Result<CellBuildSummary, WrfTMatrixSceneBuildError> {
    check_cancel(cancel).map_err(|()| WrfTMatrixSceneBuildError::Cancelled)?;
    let expected_components = validated
        .radar_elevations_deg
        .len()
        .checked_mul(AdditiveScattering::COMPONENT_COUNT)
        .ok_or(WrfTMatrixSceneBuildError::SizeOverflow)?;
    if output.len() != expected_components {
        return Err(WrfTMatrixSceneBuildError::InternalRowLength {
            cell_index,
            expected: expected_components,
            actual: output.len(),
        });
    }
    output.fill(0.0);

    let raw = source
        .raw_cell(cell_index)
        .map_err(|source| WrfTMatrixSceneBuildError::RawSourceCell { cell_index, source })?;
    let temperature_k = raw.environment().temperature_k();
    let air_density_kg_m3 = raw.dry_air_density_kg_m3();
    let mut frozen = AdditiveScattering::default();
    let mut frozen_populations = 0_usize;
    for category in raw.categories() {
        if category.mixing_ratio_kgkg() == 0.0 {
            continue;
        }
        let category_id = category.category();
        let (qice_kgkg, qnice_per_kg, volume_m3_per_kg) = match category {
            RawPropertyCategory::P3(value) => (value.qice_kgkg, value.qnice_per_kg, None),
            RawPropertyCategory::Ishmael(value) => {
                // Hybrid receives the same WRF-source-checked state as the
                // native PSD path. In particular, a transported zero/negative
                // QNICE is projected through the source qnsmall branch before
                // the versioned bulk operator sees it; using the raw number
                // here would make an explicitly supported native source
                // projection fail only after the Hybrid decision.
                let ishmael_category = value.category.ishmael_category().ok_or(
                    WrfTMatrixSceneBuildError::IshmaelCategoryLayout {
                        cell_index,
                        category: value.category,
                    },
                )?;
                let distribution = IshmaelPsd::reconstruct_wrf_source_checked(
                    IshmaelPsdInput::new(
                        ishmael_category,
                        value.qice_kgkg,
                        value.qnice_per_kg,
                        value.qvoli_m3_per_kg,
                        value.qaoli_m3_per_kg,
                        air_density_kg_m3,
                    ),
                    temperature_k,
                )
                .map_err(|source| {
                    WrfTMatrixSceneBuildError::IshmaelPsdReconstruction {
                        cell_index,
                        category: value.category,
                        source,
                    }
                })?;
                let source_checked = distribution.input();
                (
                    source_checked.qice_kgkg(),
                    source_checked.qnice_per_kg(),
                    Some(source_checked.qvoli_m3_per_kg()),
                )
            }
        };
        let input = BulkSpeciesInput {
            kind: HydrometeorKind::CloudIce,
            q_kgkg: hybrid_positive_f32(cell_index, category_id, "frozen mixing ratio", qice_kgkg)?,
            number_per_kg: Some(hybrid_positive_f32(
                cell_index,
                category_id,
                "frozen number concentration",
                qnice_per_kg,
            )?),
            volume_m3_per_kg: volume_m3_per_kg
                .map(|volume| {
                    hybrid_positive_f32(
                        cell_index,
                        category_id,
                        "total frozen particle volume",
                        volume,
                    )
                })
                .transpose()?,
            temperature_k: hybrid_positive_f32(
                cell_index,
                category_id,
                "temperature",
                temperature_k,
            )?,
            air_density_kgm3: hybrid_positive_f32(
                cell_index,
                category_id,
                "dry-air density",
                air_density_kg_m3,
            )?,
        };
        let contribution = hybrid_bulk_to_additive(cell_index, category_id, input)?;
        frozen = frozen.checked_add(contribution).map_err(|source| {
            WrfTMatrixSceneBuildError::HybridBulkRayleighOutput {
                cell_index,
                category: category_id,
                source,
            }
        })?;
        frozen_populations += 1;
    }

    if rain_mode == WrfTMatrixRainMode::FrozenOnly && frozen_populations == 0 {
        return Ok(CellBuildSummary {
            retained: false,
            counts: WrfTMatrixAuditCounts::default(),
        });
    }
    let rain = if rain_mode == WrfTMatrixRainMode::FullProperty {
        match close_raw_rain_state(&raw, OrientationDefinition::Gaussian20Research)
            .map_err(|source| WrfTMatrixSceneBuildError::RawRainClosure { cell_index, source })?
        {
            ClosedRainState::Clear => None,
            ClosedRainState::Closed(rain) => Some(rain),
            ClosedRainState::Unavailable(reason) => {
                return Err(WrfTMatrixSceneBuildError::RainUnavailable { cell_index, reason });
            }
        }
    } else {
        None
    };
    if frozen_populations == 0 && rain.is_none() {
        return Err(WrfTMatrixSceneBuildError::UnexpectedClearActiveCell { cell_index });
    }

    for (elevation_index, &elevation_deg) in validated.radar_elevations_deg.iter().enumerate() {
        check_cancel(cancel).map_err(|()| WrfTMatrixSceneBuildError::Cancelled)?;
        let mut additive = frozen;
        if let Some(rain) = rain.as_deref() {
            let request = evaluation_request(
                validated.frequency_hz,
                elevation_deg,
                SpheroidConvention::OblateMinorVertical,
            )?;
            let rain_contribution = tables
                .rain_standalone_and_residual
                .evaluate(rain, request)
                .map_err(|source| WrfTMatrixSceneBuildError::Evaluation {
                    cell_index,
                    elevation_deg,
                    contribution: WrfTMatrixContribution::StandaloneRain,
                    source,
                })?;
            additive = additive.checked_add(rain_contribution).map_err(|source| {
                WrfTMatrixSceneBuildError::Accumulation {
                    cell_index,
                    elevation_deg,
                    contribution: WrfTMatrixContribution::StandaloneRain,
                    source,
                }
            })?;
        }
        let compact =
            compact_components(additive).map_err(|source| WrfTMatrixSceneBuildError::Compact {
                cell_index,
                elevation_deg,
                source,
            })?;
        let start = elevation_index
            .checked_mul(AdditiveScattering::COMPONENT_COUNT)
            .ok_or(WrfTMatrixSceneBuildError::SizeOverflow)?;
        output[start..start + AdditiveScattering::COMPONENT_COUNT].copy_from_slice(&compact);
    }

    Ok(CellBuildSummary {
        retained: true,
        counts: WrfTMatrixAuditCounts {
            source_cells: 1,
            dry_frozen_populations: usize_to_u64(frozen_populations)?,
            standalone_rain_populations: u64::from(rain.is_some()),
            hybrid_bulk_rayleigh_cells: 1,
            hybrid_bulk_rayleigh_populations: usize_to_u64(frozen_populations)?,
            ..WrfTMatrixAuditCounts::default()
        },
    })
}

#[allow(clippy::too_many_arguments)]
fn build_p3_psd_cell_into(
    source: &WrfPropertyScene,
    cell_index: usize,
    tables: WrfTMatrixLutBundle<'_>,
    validated: ValidatedBundle<'_>,
    p3: &WrfTMatrixP3Resources,
    rain_mode: WrfTMatrixRainMode,
    cuda: Option<&WrfTMatrixCudaBatchService>,
    cancel: Option<&AtomicBool>,
    output: &mut [f32],
) -> Result<CellBuildSummary, WrfTMatrixSceneBuildError> {
    check_cancel(cancel).map_err(|()| WrfTMatrixSceneBuildError::Cancelled)?;
    let elevations = validated.radar_elevations_deg;
    let expected_components = elevations
        .len()
        .checked_mul(AdditiveScattering::COMPONENT_COUNT)
        .ok_or(WrfTMatrixSceneBuildError::SizeOverflow)?;
    if output.len() != expected_components {
        return Err(WrfTMatrixSceneBuildError::InternalRowLength {
            cell_index,
            expected: expected_components,
            actual: output.len(),
        });
    }
    let raw = source
        .raw_cell(cell_index)
        .map_err(|source| WrfTMatrixSceneBuildError::RawSourceCell { cell_index, source })?;
    let scheme = P3WrfScheme::try_from(raw.microphysics_scheme_id()).map_err(|source| {
        WrfTMatrixSceneBuildError::P3PsdReconstruction {
            cell_index,
            category: WrfPropertyCategory::P3(radar_scattering::P3Category::Category1),
            source,
        }
    })?;
    let mut frozen = Vec::with_capacity(raw.categories().len());
    for category in raw.categories() {
        if category.mixing_ratio_kgkg() == 0.0 {
            continue;
        }
        let RawPropertyCategory::P3(value) = category else {
            return Err(WrfTMatrixSceneBuildError::P3CategoryLayout {
                cell_index,
                category: category.category(),
            });
        };
        let input = p3_psd_input(scheme, value, raw.dry_air_density_kg_m3()).ok_or(
            WrfTMatrixSceneBuildError::MissingP3Qzi {
                cell_index,
                category: WrfPropertyCategory::P3(value.category),
            },
        )?;
        let distribution =
            P3Psd::reconstruct(input, p3.table.as_ref(), P3ReconstructionConfig::default())
                .map_err(|source| WrfTMatrixSceneBuildError::P3PsdReconstruction {
                    cell_index,
                    category: WrfPropertyCategory::P3(value.category),
                    source,
                })?;
        frozen.push((WrfPropertyCategory::P3(value.category), distribution));
    }

    if rain_mode == WrfTMatrixRainMode::FrozenOnly && frozen.is_empty() {
        return Ok(CellBuildSummary {
            retained: false,
            counts: WrfTMatrixAuditCounts::default(),
        });
    }
    let rain = if rain_mode == WrfTMatrixRainMode::FullProperty {
        match close_raw_rain_state(&raw, OrientationDefinition::Gaussian20Research)
            .map_err(|source| WrfTMatrixSceneBuildError::RawRainClosure { cell_index, source })?
        {
            ClosedRainState::Clear => None,
            ClosedRainState::Closed(rain) => Some(rain),
            ClosedRainState::Unavailable(reason) => {
                return Err(WrfTMatrixSceneBuildError::RainUnavailable { cell_index, reason });
            }
        }
    } else {
        None
    };
    if frozen.is_empty() && rain.is_none() {
        return Err(WrfTMatrixSceneBuildError::UnexpectedClearActiveCell { cell_index });
    }

    let counts = WrfTMatrixAuditCounts {
        source_cells: 1,
        scheme_native_psd_populations: usize_to_u64(frozen.len())?,
        dry_frozen_populations: usize_to_u64(frozen.len())?,
        standalone_rain_populations: u64::from(rain.is_some()),
        ..WrfTMatrixAuditCounts::default()
    };
    for (elevation_index, &elevation_deg) in elevations.iter().enumerate() {
        check_cancel(cancel).map_err(|()| WrfTMatrixSceneBuildError::Cancelled)?;
        let oblate_request = evaluation_request(
            validated.frequency_hz,
            elevation_deg,
            SpheroidConvention::OblateMinorVertical,
        )?;
        let prolate_request = evaluation_request(
            validated.frequency_hz,
            elevation_deg,
            SpheroidConvention::ProlateMajorVertical,
        )?;
        let mut additive = AdditiveScattering::default();
        for (category, distribution) in &frozen {
            let p3_input = distribution.input();
            let rime_fraction = p3_input.rime_mass_kgkg / p3_input.total_ice_kgkg;
            let rime_density = if p3_input.rime_mass_kgkg > 0.0 {
                Some(p3_input.rime_mass_kgkg / p3_input.rime_volume_m3_per_kg)
            } else {
                None
            };
            let prepared = prepare_p3_particle_integration(
                distribution,
                p3.integration,
                validated.p3_particle_support,
                tables,
                dry_frozen_particle_temperature_k(raw.environment().temperature_k()),
                rime_fraction,
                rime_density,
                validated.p3_fall_speed,
                oblate_request,
                prolate_request,
            )
            .map_err(|source| WrfTMatrixSceneBuildError::P3PsdIntegration {
                cell_index,
                elevation_deg,
                category: *category,
                source,
            })?;
            let integration = finish_p3_particle_integration(&prepared, cuda, cancel).map_err(
                |error| match error {
                    ParticleFinishError::Cancelled => WrfTMatrixSceneBuildError::Cancelled,
                    ParticleFinishError::Science(source) => {
                        WrfTMatrixSceneBuildError::P3PsdIntegration {
                            cell_index,
                            elevation_deg,
                            category: *category,
                            source,
                        }
                    }
                },
            )?;
            additive = additive
                .checked_add(integration.additive())
                .map_err(|source| WrfTMatrixSceneBuildError::Accumulation {
                    cell_index,
                    elevation_deg,
                    contribution: WrfTMatrixContribution::P3SchemeNativePsd,
                    source,
                })?;
        }
        if let Some(rain) = rain.as_deref() {
            let contribution = tables
                .rain_standalone_and_residual
                .evaluate(rain, oblate_request)
                .map_err(|source| WrfTMatrixSceneBuildError::Evaluation {
                    cell_index,
                    elevation_deg,
                    contribution: WrfTMatrixContribution::StandaloneRain,
                    source,
                })?;
            additive = additive.checked_add(contribution).map_err(|source| {
                WrfTMatrixSceneBuildError::Accumulation {
                    cell_index,
                    elevation_deg,
                    contribution: WrfTMatrixContribution::StandaloneRain,
                    source,
                }
            })?;
        }
        let compact =
            compact_components(additive).map_err(|source| WrfTMatrixSceneBuildError::Compact {
                cell_index,
                elevation_deg,
                source,
            })?;
        let start = elevation_index
            .checked_mul(AdditiveScattering::COMPONENT_COUNT)
            .ok_or(WrfTMatrixSceneBuildError::SizeOverflow)?;
        output[start..start + AdditiveScattering::COMPONENT_COUNT].copy_from_slice(&compact);
    }
    Ok(CellBuildSummary {
        retained: true,
        counts,
    })
}

fn p3_psd_input(
    scheme: P3WrfScheme,
    value: &crate::wrf_property_reader::RawP3Category,
    dry_air_density_kg_m3: f64,
) -> Option<P3PsdInput> {
    let moment = match scheme {
        P3WrfScheme::Mp53OneIceTripleMoment => P3IceMomentInput::WrfAdvectedQzi {
            qzi_sqrt_n_times_m6: value.qzi?,
        },
        _ => P3IceMomentInput::TwoMoment,
    };
    Some(P3PsdInput {
        scheme,
        category: value.category,
        total_ice_kgkg: value.qice_kgkg,
        total_number_per_kg: value.qnice_per_kg,
        rime_mass_kgkg: value.qir_kgkg,
        rime_volume_m3_per_kg: value.qib_m3_per_kg,
        dry_air_density_kg_m3,
        moment,
    })
}

#[allow(dead_code)]
fn build_characteristic_cell_into(
    source: &WrfPropertyScene,
    cell_index: usize,
    tables: WrfTMatrixLutBundle<'_>,
    frequency_hz: f64,
    elevations: &[f64],
    rain_mode: WrfTMatrixRainMode,
    output: &mut [f32],
) -> Result<CellBuildSummary, WrfTMatrixSceneBuildError> {
    let expected_components = elevations
        .len()
        .checked_mul(AdditiveScattering::COMPONENT_COUNT)
        .ok_or(WrfTMatrixSceneBuildError::SizeOverflow)?;
    if output.len() != expected_components {
        return Err(WrfTMatrixSceneBuildError::InternalRowLength {
            cell_index,
            expected: expected_components,
            actual: output.len(),
        });
    }
    let closed = source
        .close_cell(cell_index, OrientationDefinition::Gaussian20Research)
        .map_err(|source| WrfTMatrixSceneBuildError::SourceCell { cell_index, source })?;
    let Some(closed) = closed else {
        return if rain_mode == WrfTMatrixRainMode::FrozenOnly {
            Ok(CellBuildSummary {
                retained: false,
                counts: WrfTMatrixAuditCounts::default(),
            })
        } else {
            Err(WrfTMatrixSceneBuildError::UnexpectedClearActiveCell { cell_index })
        };
    };

    let rain = match closed.rain() {
        ClosedRainState::Clear => None,
        ClosedRainState::Closed(rain) => Some(rain.as_ref()),
        ClosedRainState::Unavailable(reason) => match rain_mode {
            WrfTMatrixRainMode::FullProperty => {
                return Err(WrfTMatrixSceneBuildError::RainUnavailable {
                    cell_index,
                    reason: reason.clone(),
                });
            }
            WrfTMatrixRainMode::FrozenOnly => None,
        },
    };
    let has_frozen = !closed.categories().is_empty();
    if rain_mode == WrfTMatrixRainMode::FrozenOnly && !has_frozen {
        return Ok(CellBuildSummary {
            retained: false,
            counts: WrfTMatrixAuditCounts::default(),
        });
    }
    let coexistence = if should_diagnose_wet_coexistence(
        rain_mode,
        has_frozen,
        rain.is_some(),
        closed.environment().temperature_k(),
    ) {
        Some(
            closed
                .diagnose_coexistence(MixtureTopology::HomogeneousMixedPhase)
                .map_err(|source| WrfTMatrixSceneBuildError::Coexistence { cell_index, source })?,
        )
    } else {
        None
    };

    let counts = if let Some(partition) = &coexistence {
        WrfTMatrixAuditCounts {
            source_cells: 1,
            characteristic_frozen_populations: usize_to_u64(
                partition.diagnosis().wet_categories().len(),
            )?,
            wet_frozen_populations: usize_to_u64(partition.diagnosis().wet_categories().len())?,
            residual_rain_populations: u64::from(
                partition.diagnosis().unused_rain_mass_kgkg() > 0.0,
            ),
            ..WrfTMatrixAuditCounts::default()
        }
    } else {
        WrfTMatrixAuditCounts {
            source_cells: 1,
            characteristic_frozen_populations: usize_to_u64(closed.categories().len())?,
            dry_frozen_populations: usize_to_u64(closed.categories().len())?,
            standalone_rain_populations: u64::from(
                rain_mode == WrfTMatrixRainMode::FullProperty && rain.is_some(),
            ),
            ..WrfTMatrixAuditCounts::default()
        }
    };

    for (elevation_index, &elevation_deg) in elevations.iter().enumerate() {
        let mut additive = AdditiveScattering::default();
        if let Some(partition) = &coexistence {
            for wet in partition.diagnosis().wet_categories() {
                let spheroid = spheroid_for_particle(wet.source_category())?;
                let request = evaluation_request(frequency_hz, elevation_deg, spheroid)?;
                let contribution = tables
                    .wet(spheroid)
                    .evaluate_wet_category(wet, request)
                    .map_err(|source| WrfTMatrixSceneBuildError::Evaluation {
                        cell_index,
                        elevation_deg,
                        contribution: WrfTMatrixContribution::WetFrozen,
                        source,
                    })?;
                verify_fall_moment_policy(
                    contribution.fall_moments(),
                    FallMomentPolicy::DiagnosticWetCategoryPositiveDownZeroWithinCategoryVariance,
                    WrfTMatrixContribution::WetFrozen,
                )?;
                additive = additive
                    .checked_add(contribution.additive())
                    .map_err(|source| WrfTMatrixSceneBuildError::Accumulation {
                        cell_index,
                        elevation_deg,
                        contribution: WrfTMatrixContribution::WetFrozen,
                        source,
                    })?;
            }
            let unused_rain_mass = partition.diagnosis().unused_rain_mass_kgkg();
            if unused_rain_mass > 0.0 {
                let rain_source =
                    rain.ok_or(WrfTMatrixSceneBuildError::CoexistenceMissingRainSource {
                        cell_index,
                    })?;
                let request = evaluation_request(
                    frequency_hz,
                    elevation_deg,
                    SpheroidConvention::OblateMinorVertical,
                )?;
                let contribution = tables
                    .rain_standalone_and_residual
                    .evaluate_unused_rain(rain_source, unused_rain_mass, request)
                    .map_err(|source| WrfTMatrixSceneBuildError::Evaluation {
                        cell_index,
                        elevation_deg,
                        contribution: WrfTMatrixContribution::ResidualRain,
                        source,
                    })?;
                verify_fall_moment_policy(
                    contribution.fall_moments(),
                    FallMomentPolicy::ClosedCategoryPositiveDownZeroWithinCategoryVariance,
                    WrfTMatrixContribution::ResidualRain,
                )?;
                additive = additive
                    .checked_add(contribution.additive())
                    .map_err(|source| WrfTMatrixSceneBuildError::Accumulation {
                        cell_index,
                        elevation_deg,
                        contribution: WrfTMatrixContribution::ResidualRain,
                        source,
                    })?;
            }
        } else {
            for category in closed.categories() {
                let spheroid = spheroid_for_characteristic_category(category.category())?;
                let request = evaluation_request(frequency_hz, elevation_deg, spheroid)?;
                let contribution = tables
                    .dry(spheroid)
                    .evaluate(category.closed(), request)
                    .map_err(|source| WrfTMatrixSceneBuildError::Evaluation {
                        cell_index,
                        elevation_deg,
                        contribution: WrfTMatrixContribution::DryFrozen,
                        source,
                    })?;
                additive = additive.checked_add(contribution).map_err(|source| {
                    WrfTMatrixSceneBuildError::Accumulation {
                        cell_index,
                        elevation_deg,
                        contribution: WrfTMatrixContribution::DryFrozen,
                        source,
                    }
                })?;
            }
            if rain_mode == WrfTMatrixRainMode::FullProperty
                && let Some(rain) = rain
            {
                let request = evaluation_request(
                    frequency_hz,
                    elevation_deg,
                    SpheroidConvention::OblateMinorVertical,
                )?;
                let contribution = tables
                    .rain_standalone_and_residual
                    .evaluate(rain, request)
                    .map_err(|source| WrfTMatrixSceneBuildError::Evaluation {
                        cell_index,
                        elevation_deg,
                        contribution: WrfTMatrixContribution::StandaloneRain,
                        source,
                    })?;
                additive = additive.checked_add(contribution).map_err(|source| {
                    WrfTMatrixSceneBuildError::Accumulation {
                        cell_index,
                        elevation_deg,
                        contribution: WrfTMatrixContribution::StandaloneRain,
                        source,
                    }
                })?;
            }
        }
        let compact =
            compact_components(additive).map_err(|source| WrfTMatrixSceneBuildError::Compact {
                cell_index,
                elevation_deg,
                source,
            })?;
        let start = elevation_index
            .checked_mul(AdditiveScattering::COMPONENT_COUNT)
            .ok_or(WrfTMatrixSceneBuildError::SizeOverflow)?;
        output[start..start + AdditiveScattering::COMPONENT_COUNT].copy_from_slice(&compact);
    }
    Ok(CellBuildSummary {
        retained: true,
        counts,
    })
}

#[allow(clippy::too_many_arguments)]
fn build_ishmael_psd_cell_into(
    source: &WrfPropertyScene,
    cell_index: usize,
    tables: WrfTMatrixLutBundle<'_>,
    validated: ValidatedBundle<'_>,
    rain_mode: WrfTMatrixRainMode,
    cuda: Option<&WrfTMatrixCudaBatchService>,
    cancel: Option<&AtomicBool>,
    output: &mut [f32],
) -> Result<CellBuildSummary, WrfTMatrixSceneBuildError> {
    check_cancel(cancel).map_err(|()| WrfTMatrixSceneBuildError::Cancelled)?;
    let elevations = validated.radar_elevations_deg;
    let expected_components = elevations
        .len()
        .checked_mul(AdditiveScattering::COMPONENT_COUNT)
        .ok_or(WrfTMatrixSceneBuildError::SizeOverflow)?;
    if output.len() != expected_components {
        return Err(WrfTMatrixSceneBuildError::InternalRowLength {
            cell_index,
            expected: expected_components,
            actual: output.len(),
        });
    }

    let raw = source
        .raw_cell(cell_index)
        .map_err(|source| WrfTMatrixSceneBuildError::RawSourceCell { cell_index, source })?;
    let mut frozen = Vec::with_capacity(raw.categories().len());
    for category in raw.categories() {
        if category.mixing_ratio_kgkg() == 0.0 {
            continue;
        }
        let RawPropertyCategory::Ishmael(value) = category else {
            return Err(WrfTMatrixSceneBuildError::IshmaelCategoryLayout {
                cell_index,
                category: category.category(),
            });
        };
        let ishmael_category = value.category.ishmael_category().ok_or(
            WrfTMatrixSceneBuildError::IshmaelCategoryLayout {
                cell_index,
                category: value.category,
            },
        )?;
        let distribution = IshmaelPsd::reconstruct_wrf_source_checked(
            IshmaelPsdInput::new(
                ishmael_category,
                value.qice_kgkg,
                value.qnice_per_kg,
                value.qvoli_m3_per_kg,
                value.qaoli_m3_per_kg,
                raw.dry_air_density_kg_m3(),
            ),
            raw.environment().temperature_k(),
        )
        .map_err(
            |source| WrfTMatrixSceneBuildError::IshmaelPsdReconstruction {
                cell_index,
                category: value.category,
                source,
            },
        )?;
        frozen.push((value.category, distribution));
    }

    if rain_mode == WrfTMatrixRainMode::FrozenOnly && frozen.is_empty() {
        return Ok(CellBuildSummary {
            retained: false,
            counts: WrfTMatrixAuditCounts::default(),
        });
    }
    let rain = if rain_mode == WrfTMatrixRainMode::FullProperty {
        match close_raw_rain_state(&raw, OrientationDefinition::Gaussian20Research)
            .map_err(|source| WrfTMatrixSceneBuildError::RawRainClosure { cell_index, source })?
        {
            ClosedRainState::Clear => None,
            ClosedRainState::Closed(rain) => Some(rain),
            ClosedRainState::Unavailable(reason) => {
                return Err(WrfTMatrixSceneBuildError::RainUnavailable { cell_index, reason });
            }
        }
    } else {
        None
    };
    if frozen.is_empty() && rain.is_none() {
        return Err(WrfTMatrixSceneBuildError::UnexpectedClearActiveCell { cell_index });
    }

    let counts = WrfTMatrixAuditCounts {
        source_cells: 1,
        scheme_native_psd_populations: usize_to_u64(frozen.len())?,
        dry_frozen_populations: usize_to_u64(frozen.len())?,
        standalone_rain_populations: u64::from(rain.is_some()),
        ..WrfTMatrixAuditCounts::default()
    };
    for (elevation_index, &elevation_deg) in elevations.iter().enumerate() {
        check_cancel(cancel).map_err(|()| WrfTMatrixSceneBuildError::Cancelled)?;
        let oblate_request = evaluation_request(
            validated.frequency_hz,
            elevation_deg,
            SpheroidConvention::OblateMinorVertical,
        )?;
        let prolate_request = evaluation_request(
            validated.frequency_hz,
            elevation_deg,
            SpheroidConvention::ProlateMajorVertical,
        )?;
        let mut additive = AdditiveScattering::default();
        for &(category, distribution) in &frozen {
            let prepared = prepare_ishmael_particle_integration(
                &distribution,
                validated.ishmael_psd_config,
                validated.ishmael_particle_support,
                validated.ishmael_fall_speed,
                validated.ishmael_ice_material_density_kg_m3,
                tables,
                dry_frozen_particle_temperature_k(raw.environment().temperature_k()),
                oblate_request,
                prolate_request,
            )
            .map_err(|source| WrfTMatrixSceneBuildError::IshmaelPsdIntegration {
                cell_index,
                elevation_deg,
                category,
                source,
            })?;
            let integration = finish_ishmael_particle_integration(&prepared, cuda, cancel)
                .map_err(|error| match error {
                    ParticleFinishError::Cancelled => WrfTMatrixSceneBuildError::Cancelled,
                    ParticleFinishError::Science(source) => {
                        WrfTMatrixSceneBuildError::IshmaelPsdIntegration {
                            cell_index,
                            elevation_deg,
                            category,
                            source,
                        }
                    }
                })?;
            additive = additive
                .checked_add(integration.additive())
                .map_err(|source| WrfTMatrixSceneBuildError::Accumulation {
                    cell_index,
                    elevation_deg,
                    contribution: WrfTMatrixContribution::IshmaelSchemeNativePsd,
                    source,
                })?;
        }
        if let Some(rain) = rain.as_deref() {
            let contribution = tables
                .rain_standalone_and_residual
                .evaluate(rain, oblate_request)
                .map_err(|source| WrfTMatrixSceneBuildError::Evaluation {
                    cell_index,
                    elevation_deg,
                    contribution: WrfTMatrixContribution::StandaloneRain,
                    source,
                })?;
            additive = additive.checked_add(contribution).map_err(|source| {
                WrfTMatrixSceneBuildError::Accumulation {
                    cell_index,
                    elevation_deg,
                    contribution: WrfTMatrixContribution::StandaloneRain,
                    source,
                }
            })?;
        }
        let compact =
            compact_components(additive).map_err(|source| WrfTMatrixSceneBuildError::Compact {
                cell_index,
                elevation_deg,
                source,
            })?;
        let start = elevation_index
            .checked_mul(AdditiveScattering::COMPONENT_COUNT)
            .ok_or(WrfTMatrixSceneBuildError::SizeOverflow)?;
        output[start..start + AdditiveScattering::COMPONENT_COUNT].copy_from_slice(&compact);
    }
    Ok(CellBuildSummary {
        retained: true,
        counts,
    })
}

pub(crate) fn dry_frozen_particle_temperature_k(ambient_temperature_k: f64) -> f64 {
    if ambient_temperature_k > ICE_MELTING_TEMPERATURE_K {
        ICE_MELTING_TEMPERATURE_K
    } else {
        ambient_temperature_k
    }
}

fn gaussian20_orientation() -> OrientationModel {
    OrientationModel::GaussianCanting {
        mean_deg: 0.0,
        standard_deviation_deg: 20.0,
        quadrature_points: 50,
    }
}

fn floor_anchored_rayleigh_sphere(
    anchor_components: [f64; AdditiveScattering::COMPONENT_COUNT],
    diameter_ratio: f64,
    target_speed: f64,
    frequency_hz: f64,
    reference_water_factor_squared: f64,
) -> Result<AdditiveScattering, EvaluationError> {
    if !diameter_ratio.is_finite() || diameter_ratio <= 0.0 || diameter_ratio > 1.0 {
        return Err(EvaluationError::InvalidQuery {
            field: "table-floor Rayleigh bridge diameter ratio",
            value: diameter_ratio,
        });
    }
    let backscatter_scale = diameter_ratio.powi(6);
    let anchor_reflectivity = 0.5 * (anchor_components[0] + anchor_components[1]);
    let reflectivity = anchor_reflectivity * backscatter_scale;
    let anchor_attenuation = 0.5 * (anchor_components[5] + anchor_components[6]);
    let wavelength_mm = 299_792_458.0 / frequency_hz * 1_000.0;
    let backscatter_cross_section_mm2 =
        anchor_reflectivity * PI.powi(5) * reference_water_factor_squared / wavelength_mm.powi(4);
    let scattering_cross_section_mm2 = (2.0 / 3.0) * backscatter_cross_section_mm2;
    let extinction_cross_section_mm2 = anchor_attenuation / 4.343e-3;
    let absorption_cross_section_mm2 = extinction_cross_section_mm2 - scattering_cross_section_mm2;
    let negative_tolerance = 1.0e-10
        * extinction_cross_section_mm2
            .abs()
            .max(scattering_cross_section_mm2.abs())
            .max(f64::MIN_POSITIVE);
    if absorption_cross_section_mm2 < -negative_tolerance {
        return Err(EvaluationError::InvalidQuery {
            field: "table-floor Rayleigh bridge anchor absorption cross section",
            value: absorption_cross_section_mm2,
        });
    }
    let target_extinction_cross_section_mm2 = absorption_cross_section_mm2.max(0.0)
        * diameter_ratio.powi(3)
        + scattering_cross_section_mm2 * backscatter_scale;
    let attenuation = 4.343e-3 * target_extinction_cross_section_mm2;
    AdditiveScattering::from_components([
        reflectivity,
        reflectivity,
        reflectivity,
        0.0,
        0.0,
        attenuation,
        attenuation,
        reflectivity * target_speed,
        reflectivity * target_speed * target_speed,
    ])
    .map_err(EvaluationError::Output)
}

#[allow(clippy::too_many_arguments)]
fn evaluate_table_floor_anchored_rayleigh_sphere(
    table: &ResearchTMatrixLut,
    target_diameter_m: f64,
    target_density_kg_m3: f64,
    target_speed_m_s: f64,
    temperature_k: f64,
    rime_fraction: Option<f64>,
    rime_density_kg_m3: Option<f64>,
    fall_speed: PsdFallSpeedProvenance,
    request: TMatrixEvaluationRequest,
) -> Result<AdditiveScattering, EvaluationError> {
    let domain = table.dry_particle_node_domain()?;
    let anchor_diameter_m = domain.equivolume_diameter_range_m()[0];
    if target_diameter_m >= anchor_diameter_m {
        return Err(EvaluationError::InvalidQuery {
            field: "Rayleigh bridge diameter below table floor",
            value: target_diameter_m,
        });
    }
    let anchor = table.prepare_dry_particle_geometry_lut_interpolation_per_m3(
        temperature_k,
        anchor_diameter_m,
        target_density_kg_m3,
        1.0,
        PsdSpheroidHabit::Spherical,
        rime_fraction,
        rime_density_kg_m3,
        fall_speed,
        gaussian20_orientation(),
        request,
    )?;
    let anchor = table.evaluate_prepared_dry_particle_lut_interpolation_per_m3(&anchor)?;
    let reference_water_factor_squared = table
        .descriptor()
        .radar()
        .reference_water_dielectric_factor_squared;
    floor_anchored_rayleigh_sphere(
        anchor.components(),
        target_diameter_m / anchor_diameter_m,
        target_speed_m_s,
        request.frequency_hz(),
        reference_water_factor_squared,
    )
}

/// CPU-admitted P3 particle work. Table nodes carry the owning table's opaque
/// prepared token, while the below-floor Rayleigh bridge remains an explicit
/// CPU-only route and can never be staged as a LUT node by an accelerator.
enum PreparedP3ParticleEvaluation<'table> {
    TMatrixTable {
        table: &'table ResearchTMatrixLut,
        prepared: PreparedTMatrixLutInterpolation,
    },
    CpuRayleighBridge {
        table: &'table ResearchTMatrixLut,
        node: P3TMatrixParticleNode,
        temperature_k: f64,
        rime_fraction: f64,
        rime_density: Option<f64>,
        fall_speed: PsdFallSpeedProvenance,
        request: TMatrixEvaluationRequest,
    },
}

impl<'table> PreparedP3ParticleEvaluation<'table> {
    fn evaluate_cpu(&self) -> Result<AdditiveScattering, EvaluationError> {
        match self {
            Self::TMatrixTable { table, prepared } => {
                table.evaluate_prepared_dry_particle_lut_interpolation_per_m3(prepared)
            }
            Self::CpuRayleighBridge {
                table,
                node,
                temperature_k,
                rime_fraction,
                rime_density,
                fall_speed,
                request,
            } => evaluate_p3_rayleigh_bridge_node(
                table,
                node,
                *temperature_k,
                *rime_fraction,
                *rime_density,
                *fall_speed,
                *request,
            ),
        }
    }

    fn table_binding(
        &self,
    ) -> Option<(&'table ResearchTMatrixLut, &PreparedTMatrixLutInterpolation)> {
        match self {
            Self::TMatrixTable { table, prepared } => Some((*table, prepared)),
            Self::CpuRayleighBridge { .. } => None,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn prepare_p3_particle_node<'table>(
    table: &'table ResearchTMatrixLut,
    node: &P3TMatrixParticleNode,
    temperature_k: f64,
    rime_fraction: f64,
    rime_density: Option<f64>,
    fall_speed: PsdFallSpeedProvenance,
    request: TMatrixEvaluationRequest,
) -> Result<PreparedP3ParticleEvaluation<'table>, EvaluationError> {
    match node.scattering_route() {
        P3ParticleScatteringRoute::TMatrixTable => {
            let prepared = table.prepare_dry_particle_geometry_lut_interpolation_per_m3(
                temperature_k,
                node.equivolume_diameter_m(),
                node.bulk_density_kg_m3(),
                node.minor_to_major_axis_ratio(),
                node.habit(),
                Some(rime_fraction),
                rime_density,
                fall_speed,
                gaussian20_orientation(),
                request,
            )?;
            Ok(PreparedP3ParticleEvaluation::TMatrixTable { table, prepared })
        }
        P3ParticleScatteringRoute::TableFloorAnchoredSmallDenseSphereRayleighV1 => {
            Ok(PreparedP3ParticleEvaluation::CpuRayleighBridge {
                table,
                node: *node,
                temperature_k,
                rime_fraction,
                rime_density,
                fall_speed,
                request,
            })
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn evaluate_p3_rayleigh_bridge_node(
    table: &ResearchTMatrixLut,
    node: &P3TMatrixParticleNode,
    temperature_k: f64,
    rime_fraction: f64,
    rime_density: Option<f64>,
    fall_speed: PsdFallSpeedProvenance,
    request: TMatrixEvaluationRequest,
) -> Result<AdditiveScattering, EvaluationError> {
    let target_diameter = node.equivolume_diameter_m();
    let target_density = node.bulk_density_kg_m3();
    let target_speed =
        table.dry_particle_geometry_terminal_speed_m_s(target_diameter, target_density)?;

    // This route is selected only for P3's exact spherical 900 kg/m3 ice law
    // below the LUT diameter floor. Anchor the same table material,
    // temperature, frequency and view at its first diameter, then apply the
    // analytic Rayleigh sphere limits: D^6 backscatter/covariance and D^3
    // absorption. No other failed lookup can enter this path.
    if node.habit() != PsdSpheroidHabit::Spherical
        || node.source().particle.region != P3ParticleRegion::SmallDenseSphere
        || target_density.to_bits() != P3_SOLID_ICE_DENSITY_KG_M3.to_bits()
        || node.minor_to_major_axis_ratio().to_bits() != 1.0_f64.to_bits()
    {
        return Err(EvaluationError::InvalidQuery {
            field: "P3 Rayleigh bridge exact small-dense-sphere contract",
            value: node.minor_to_major_axis_ratio(),
        });
    }
    evaluate_table_floor_anchored_rayleigh_sphere(
        table,
        target_diameter,
        target_density,
        target_speed,
        temperature_k,
        Some(rime_fraction),
        rime_density,
        fall_speed,
        request,
    )
}

/// Ordered P3 integration admission for the future batch executor. The
/// original quadrature node indices remain attached to every prepared entry;
/// [`PreparedP3TMatrixIntegration::finish`] still owns population scaling and
/// the exact serial source-order reduction.
#[allow(dead_code)]
struct PreparedP3ParticleIntegration<'table> {
    integration: PreparedP3TMatrixIntegration,
    evaluations: Vec<(usize, PreparedP3ParticleEvaluation<'table>)>,
}

#[allow(dead_code)]
impl<'table> PreparedP3ParticleIntegration<'table> {
    /// Only LUT-backed nodes are exposed to a batch executor. CPU-only
    /// Rayleigh bridge entries are deliberately absent from this iterator.
    fn table_nodes(
        &self,
    ) -> impl Iterator<
        Item = (
            usize,
            usize,
            P3TMatrixParticleNode,
            &'table ResearchTMatrixLut,
            &PreparedTMatrixLutInterpolation,
        ),
    > + '_ {
        self.integration
            .nodes()
            .zip(&self.evaluations)
            .enumerate()
            .filter_map(
                |(position, ((node_index, node), (prepared_index, evaluation)))| {
                    debug_assert_eq!(node_index, *prepared_index);
                    evaluation
                        .table_binding()
                        .map(|(table, prepared)| (position, node_index, node, table, prepared))
                },
            )
    }

    /// Finish through a caller-selected table evaluator while retaining the
    /// scalar CPU Rayleigh route and the library-owned exact reduction.
    fn finish_with_table_evaluator<F>(
        &self,
        mut evaluate_table: F,
    ) -> Result<P3TMatrixIntegrationResult, P3TMatrixIntegrationError<EvaluationError>>
    where
        F: FnMut(
            usize,
            usize,
            &P3TMatrixParticleNode,
            &'table ResearchTMatrixLut,
            &PreparedTMatrixLutInterpolation,
        ) -> Result<AdditiveScattering, EvaluationError>,
    {
        let mut cursor = 0_usize;
        let result = self.integration.finish(|node_index, node| {
            let position = cursor;
            let (prepared_index, evaluation) = self
                .evaluations
                .get(position)
                .expect("prepared P3 workload matches its admitted integration");
            cursor += 1;
            debug_assert_eq!(node_index, *prepared_index);
            match evaluation {
                PreparedP3ParticleEvaluation::TMatrixTable { table, prepared } => {
                    evaluate_table(position, node_index, node, table, prepared)
                }
                PreparedP3ParticleEvaluation::CpuRayleighBridge { .. } => evaluation.evaluate_cpu(),
            }
        });
        debug_assert_eq!(cursor, self.evaluations.len());
        result
    }

    fn finish_cpu(
        &self,
    ) -> Result<P3TMatrixIntegrationResult, P3TMatrixIntegrationError<EvaluationError>> {
        self.finish_with_table_evaluator(|_, _, _, table, prepared| {
            table.evaluate_prepared_dry_particle_lut_interpolation_per_m3(prepared)
        })
    }
}

#[allow(dead_code, clippy::too_many_arguments)]
fn prepare_p3_particle_integration<'table>(
    distribution: &P3Psd,
    config: P3TMatrixIntegrationConfig,
    table_support: PsdParticleSupport,
    tables: WrfTMatrixLutBundle<'table>,
    temperature_k: f64,
    rime_fraction: f64,
    rime_density: Option<f64>,
    fall_speed: PsdFallSpeedProvenance,
    oblate_request: TMatrixEvaluationRequest,
    prolate_request: TMatrixEvaluationRequest,
) -> Result<PreparedP3ParticleIntegration<'table>, P3TMatrixIntegrationError<EvaluationError>> {
    let integration = prepare_p3_tmatrix_psd(distribution, config, table_support)
        .map_err(P3TMatrixIntegrationError::<EvaluationError>::from)?;
    let mut evaluations = Vec::with_capacity(integration.node_count());
    for (node_index, node) in integration.nodes() {
        let (table, request) = match node.habit() {
            PsdSpheroidHabit::Oblate | PsdSpheroidHabit::Spherical => {
                (tables.dry_oblate, oblate_request)
            }
            PsdSpheroidHabit::Prolate => (tables.dry_prolate, prolate_request),
        };
        let evaluation = prepare_p3_particle_node(
            table,
            &node,
            temperature_k,
            rime_fraction,
            rime_density,
            fall_speed,
            request,
        )
        .map_err(|source| P3TMatrixIntegrationError::Evaluation { node_index, source })?;
        evaluations.push((node_index, evaluation));
    }
    Ok(PreparedP3ParticleIntegration {
        integration,
        evaluations,
    })
}

fn evaluate_p3_psd_raw_cell(
    raw: &RawPropertyCell,
    tables: WrfTMatrixLutBundle<'_>,
    validated: ValidatedBundle<'_>,
    p3: &WrfTMatrixP3Resources,
    elevation_deg: f64,
    cuda: Option<&WrfTMatrixCudaBatchService>,
    cancel: Option<&AtomicBool>,
) -> Result<Option<PolarAccumulatorQuantities>, WrfTMatrixRawEvaluationError> {
    let scheme = P3WrfScheme::try_from(raw.microphysics_scheme_id()).map_err(|source| {
        WrfTMatrixRawEvaluationError::P3PsdReconstruction {
            category: WrfPropertyCategory::P3(radar_scattering::P3Category::Category1),
            source,
        }
    })?;
    let mut frozen = Vec::with_capacity(raw.categories().len());
    for category in raw.categories() {
        if category.mixing_ratio_kgkg() == 0.0 {
            continue;
        }
        let RawPropertyCategory::P3(value) = category else {
            return Err(WrfTMatrixRawEvaluationError::P3CategoryLayout(
                category.category(),
            ));
        };
        let input = p3_psd_input(scheme, value, raw.dry_air_density_kg_m3()).ok_or(
            WrfTMatrixRawEvaluationError::MissingP3Qzi(WrfPropertyCategory::P3(value.category)),
        )?;
        let distribution =
            P3Psd::reconstruct(input, p3.table.as_ref(), P3ReconstructionConfig::default())
                .map_err(|source| WrfTMatrixRawEvaluationError::P3PsdReconstruction {
                    category: WrfPropertyCategory::P3(value.category),
                    source,
                })?;
        frozen.push((WrfPropertyCategory::P3(value.category), distribution));
    }
    let rain = match close_raw_rain_state(raw, OrientationDefinition::Gaussian20Research)
        .map_err(WrfTMatrixRawEvaluationError::Closure)?
    {
        ClosedRainState::Clear => None,
        ClosedRainState::Closed(rain) => Some(rain),
        ClosedRainState::Unavailable(reason) => {
            return Err(WrfTMatrixRawEvaluationError::RainUnavailable(reason));
        }
    };
    if frozen.is_empty() && rain.is_none() {
        return Ok(None);
    }

    let oblate_request = raw_evaluation_request(
        validated.frequency_hz,
        elevation_deg,
        SpheroidConvention::OblateMinorVertical,
    )?;
    let prolate_request = raw_evaluation_request(
        validated.frequency_hz,
        elevation_deg,
        SpheroidConvention::ProlateMajorVertical,
    )?;
    let mut additive = AdditiveScattering::default();
    for (category, distribution) in &frozen {
        let p3_input = distribution.input();
        let rime_fraction = p3_input.rime_mass_kgkg / p3_input.total_ice_kgkg;
        let rime_density = if p3_input.rime_mass_kgkg > 0.0 {
            Some(p3_input.rime_mass_kgkg / p3_input.rime_volume_m3_per_kg)
        } else {
            None
        };
        let prepared = prepare_p3_particle_integration(
            distribution,
            p3.integration,
            validated.p3_particle_support,
            tables,
            dry_frozen_particle_temperature_k(raw.environment().temperature_k()),
            rime_fraction,
            rime_density,
            validated.p3_fall_speed,
            oblate_request,
            prolate_request,
        )
        .map_err(|source| WrfTMatrixRawEvaluationError::P3PsdIntegration {
            category: *category,
            source,
        })?;
        let integration = finish_p3_particle_integration(&prepared, cuda, cancel).map_err(
            |error| match error {
                ParticleFinishError::Cancelled => WrfTMatrixRawEvaluationError::Cancelled,
                ParticleFinishError::Science(source) => {
                    WrfTMatrixRawEvaluationError::P3PsdIntegration {
                        category: *category,
                        source,
                    }
                }
            },
        )?;
        additive = additive
            .checked_add(integration.additive())
            .map_err(|source| WrfTMatrixRawEvaluationError::Accumulation {
                contribution: WrfTMatrixContribution::P3SchemeNativePsd,
                source,
            })?;
    }
    if let Some(rain) = rain.as_deref() {
        let contribution = tables
            .rain_standalone_and_residual
            .evaluate(rain, oblate_request)
            .map_err(|source| WrfTMatrixRawEvaluationError::Evaluation {
                contribution: WrfTMatrixContribution::StandaloneRain,
                source,
            })?;
        additive = additive.checked_add(contribution).map_err(|source| {
            WrfTMatrixRawEvaluationError::Accumulation {
                contribution: WrfTMatrixContribution::StandaloneRain,
                source,
            }
        })?;
    }
    additive
        .to_polar_accumulator_quantities()
        .map(Some)
        .map_err(WrfTMatrixRawEvaluationError::Output)
}

/// Execute every LUT-backed node of one P3 category on CUDA and then hand the
/// ordered per-particle answers back to the established CPU population
/// scaling/reduction. A conversion or worker error discards every answer for
/// this category and runs the exact scalar oracle instead.
fn finish_p3_particle_integration(
    prepared: &PreparedP3ParticleIntegration<'_>,
    cuda: Option<&WrfTMatrixCudaBatchService>,
    cancel: Option<&AtomicBool>,
) -> Result<
    P3TMatrixIntegrationResult,
    ParticleFinishError<P3TMatrixIntegrationError<EvaluationError>>,
> {
    check_cancel(cancel).map_err(|()| ParticleFinishError::Cancelled)?;
    let Some(cuda) = cuda else {
        return finish_p3_cpu_cancellable(prepared, cancel);
    };
    if cuda.is_disabled() {
        return finish_p3_cpu_cancellable(prepared, cancel);
    }

    let mut oblate_nodes = Vec::new();
    let mut oblate_indices = Vec::new();
    let mut prolate_nodes = Vec::new();
    let mut prolate_indices = Vec::new();
    for (position, node_index, node, _table, admitted) in prepared.table_nodes() {
        check_cancel(cancel).map_err(|()| ParticleFinishError::Cancelled)?;
        let cuda_node = match CudaPreparedTMatrixNode::new(admitted, 1.0) {
            Ok(cuda_node) => cuda_node,
            Err(error) => {
                check_cancel(cancel).map_err(|()| ParticleFinishError::Cancelled)?;
                cuda.disable_for_cpu_fallback(
                    format!("stage admitted P3 node {node_index} for CUDA: {error}"),
                    0,
                );
                return finish_p3_cpu_cancellable(prepared, cancel);
            }
        };
        match node.habit() {
            PsdSpheroidHabit::Oblate | PsdSpheroidHabit::Spherical => {
                oblate_indices.push(position);
                oblate_nodes.push(cuda_node);
            }
            PsdSpheroidHabit::Prolate => {
                prolate_indices.push(position);
                prolate_nodes.push(cuda_node);
            }
        }
    }

    let mut outputs = vec![None; prepared.evaluations.len()];
    for (role, indices, nodes) in [
        (
            WrfTMatrixCudaTableRole::DryOblate,
            oblate_indices,
            oblate_nodes,
        ),
        (
            WrfTMatrixCudaTableRole::DryProlate,
            prolate_indices,
            prolate_nodes,
        ),
    ] {
        if nodes.is_empty() {
            continue;
        }
        check_cancel(cancel).map_err(|()| ParticleFinishError::Cancelled)?;
        let evaluated = match cuda.evaluate_particles(role, nodes) {
            Ok(evaluated) => evaluated,
            Err(_) => {
                check_cancel(cancel).map_err(|()| ParticleFinishError::Cancelled)?;
                return finish_p3_cpu_cancellable(prepared, cancel);
            }
        };
        check_cancel(cancel).map_err(|()| ParticleFinishError::Cancelled)?;
        debug_assert_eq!(indices.len(), evaluated.len());
        for (position, output) in indices.into_iter().zip(evaluated) {
            outputs[position] = Some(output);
        }
    }

    prepared
        .finish_with_table_evaluator(|position, _, _, _, _| {
            Ok(outputs[position]
                .expect("CUDA returned one ordered answer for every admitted P3 table node"))
        })
        .map_err(ParticleFinishError::Science)
}

fn finish_p3_cpu_cancellable(
    prepared: &PreparedP3ParticleIntegration<'_>,
    cancel: Option<&AtomicBool>,
) -> Result<
    P3TMatrixIntegrationResult,
    ParticleFinishError<P3TMatrixIntegrationError<EvaluationError>>,
> {
    let Some(_) = cancel else {
        return prepared.finish_cpu().map_err(ParticleFinishError::Science);
    };
    let mut outputs = Vec::with_capacity(prepared.evaluations.len());
    for (node_index, evaluation) in &prepared.evaluations {
        check_cancel(cancel).map_err(|()| ParticleFinishError::Cancelled)?;
        let output = evaluation.evaluate_cpu().map_err(|source| {
            ParticleFinishError::Science(P3TMatrixIntegrationError::Evaluation {
                node_index: *node_index,
                source,
            })
        })?;
        outputs.push(output);
    }
    check_cancel(cancel).map_err(|()| ParticleFinishError::Cancelled)?;
    prepared
        .finish_with_table_evaluator(|position, _, _, _, _| Ok(outputs[position]))
        .map_err(ParticleFinishError::Science)
}

#[allow(dead_code)]
fn evaluate_characteristic_raw_cell(
    raw: &RawPropertyCell,
    tables: WrfTMatrixLutBundle<'_>,
    frequency_hz: f64,
    elevation_deg: f64,
) -> Result<Option<PolarAccumulatorQuantities>, WrfTMatrixRawEvaluationError> {
    let closed = close_raw_property_cell(raw, OrientationDefinition::Gaussian20Research)
        .map_err(WrfTMatrixRawEvaluationError::Closure)?;
    let rain = match closed.rain() {
        ClosedRainState::Clear => None,
        ClosedRainState::Closed(rain) => Some(rain.as_ref()),
        ClosedRainState::Unavailable(reason) => {
            return Err(WrfTMatrixRawEvaluationError::RainUnavailable(
                reason.clone(),
            ));
        }
    };
    let has_frozen = !closed.categories().is_empty();
    if !has_frozen && rain.is_none() {
        return Ok(None);
    }
    let coexistence = if should_diagnose_wet_coexistence(
        WrfTMatrixRainMode::FullProperty,
        has_frozen,
        rain.is_some(),
        closed.environment().temperature_k(),
    ) {
        Some(
            closed
                .diagnose_coexistence(MixtureTopology::HomogeneousMixedPhase)
                .map_err(WrfTMatrixRawEvaluationError::Coexistence)?,
        )
    } else {
        None
    };
    let mut additive = AdditiveScattering::default();
    if let Some(partition) = &coexistence {
        for wet in partition.diagnosis().wet_categories() {
            let spheroid = spheroid_for_raw_particle(wet.source_category())?;
            let request = raw_evaluation_request(frequency_hz, elevation_deg, spheroid)?;
            let contribution = tables
                .wet(spheroid)
                .evaluate_wet_category(wet, request)
                .map_err(|source| WrfTMatrixRawEvaluationError::Evaluation {
                    contribution: WrfTMatrixContribution::WetFrozen,
                    source,
                })?;
            verify_raw_fall_moment_policy(
                contribution.fall_moments(),
                FallMomentPolicy::DiagnosticWetCategoryPositiveDownZeroWithinCategoryVariance,
                WrfTMatrixContribution::WetFrozen,
            )?;
            additive = additive
                .checked_add(contribution.additive())
                .map_err(|source| WrfTMatrixRawEvaluationError::Accumulation {
                    contribution: WrfTMatrixContribution::WetFrozen,
                    source,
                })?;
        }
        let unused_rain_mass = partition.diagnosis().unused_rain_mass_kgkg();
        if unused_rain_mass > 0.0 {
            let rain_source = rain.ok_or(WrfTMatrixRawEvaluationError::MissingRainSource)?;
            let request = raw_evaluation_request(
                frequency_hz,
                elevation_deg,
                SpheroidConvention::OblateMinorVertical,
            )?;
            let contribution = tables
                .rain_standalone_and_residual
                .evaluate_unused_rain(rain_source, unused_rain_mass, request)
                .map_err(|source| WrfTMatrixRawEvaluationError::Evaluation {
                    contribution: WrfTMatrixContribution::ResidualRain,
                    source,
                })?;
            verify_raw_fall_moment_policy(
                contribution.fall_moments(),
                FallMomentPolicy::ClosedCategoryPositiveDownZeroWithinCategoryVariance,
                WrfTMatrixContribution::ResidualRain,
            )?;
            additive = additive
                .checked_add(contribution.additive())
                .map_err(|source| WrfTMatrixRawEvaluationError::Accumulation {
                    contribution: WrfTMatrixContribution::ResidualRain,
                    source,
                })?;
        }
    } else {
        for category in closed.categories() {
            let spheroid = spheroid_for_characteristic_category(category.category())
                .map_err(|_| WrfTMatrixRawEvaluationError::IshmaelCharacteristicForbidden)?;
            let request = raw_evaluation_request(frequency_hz, elevation_deg, spheroid)?;
            let contribution = tables
                .dry(spheroid)
                .evaluate(category.closed(), request)
                .map_err(|source| WrfTMatrixRawEvaluationError::Evaluation {
                    contribution: WrfTMatrixContribution::DryFrozen,
                    source,
                })?;
            additive = additive.checked_add(contribution).map_err(|source| {
                WrfTMatrixRawEvaluationError::Accumulation {
                    contribution: WrfTMatrixContribution::DryFrozen,
                    source,
                }
            })?;
        }
        if let Some(rain) = rain {
            let request = raw_evaluation_request(
                frequency_hz,
                elevation_deg,
                SpheroidConvention::OblateMinorVertical,
            )?;
            let contribution = tables
                .rain_standalone_and_residual
                .evaluate(rain, request)
                .map_err(|source| WrfTMatrixRawEvaluationError::Evaluation {
                    contribution: WrfTMatrixContribution::StandaloneRain,
                    source,
                })?;
            additive = additive.checked_add(contribution).map_err(|source| {
                WrfTMatrixRawEvaluationError::Accumulation {
                    contribution: WrfTMatrixContribution::StandaloneRain,
                    source,
                }
            })?;
        }
    }
    additive
        .to_polar_accumulator_quantities()
        .map(Some)
        .map_err(WrfTMatrixRawEvaluationError::Output)
}

enum PreparedIshmaelParticleRoute<'table> {
    TMatrixTable {
        table: &'table ResearchTMatrixLut,
        prepared: PreparedTMatrixLutInterpolation,
    },
    CpuRayleighBridge {
        table: &'table ResearchTMatrixLut,
        scattering_node: IshmaelScatteringParticleNode,
        temperature_k: f64,
        fall_speed: PsdFallSpeedProvenance,
        request: TMatrixEvaluationRequest,
    },
}

struct PreparedIshmaelParticleEvaluation<'table> {
    level: PsdQuadratureLevel,
    node_index: usize,
    role: WrfTMatrixCudaTableRole,
    route: PreparedIshmaelParticleRoute<'table>,
}

impl<'table> PreparedIshmaelParticleEvaluation<'table> {
    fn evaluate_cpu(&self) -> Result<AdditiveScattering, EvaluationError> {
        match &self.route {
            PreparedIshmaelParticleRoute::TMatrixTable { table, prepared } => {
                table.evaluate_prepared_dry_particle_lut_interpolation_per_m3(prepared)
            }
            PreparedIshmaelParticleRoute::CpuRayleighBridge {
                table,
                scattering_node,
                temperature_k,
                fall_speed,
                request,
            } => evaluate_ishmael_rayleigh_bridge_node(
                table,
                *scattering_node,
                *temperature_k,
                *fall_speed,
                *request,
            ),
        }
    }

    fn table_binding(
        &self,
    ) -> Option<(&'table ResearchTMatrixLut, &PreparedTMatrixLutInterpolation)> {
        match &self.route {
            PreparedIshmaelParticleRoute::TMatrixTable { table, prepared } => {
                Some((*table, prepared))
            }
            PreparedIshmaelParticleRoute::CpuRayleighBridge { .. } => None,
        }
    }
}

struct PreparedIshmaelParticleIntegration<'table> {
    integration: PreparedIshmaelPsdIntegration,
    evaluations: Vec<PreparedIshmaelParticleEvaluation<'table>>,
}

impl<'table> PreparedIshmaelParticleIntegration<'table> {
    fn finish_with_table_evaluator<F>(
        &self,
        mut evaluate_table: F,
    ) -> Result<PsdIntegrationResult, PsdIntegrationError<EvaluationError>>
    where
        F: FnMut(
            usize,
            PsdQuadratureLevel,
            usize,
            &PsdParticleNode,
            &'table ResearchTMatrixLut,
            &PreparedTMatrixLutInterpolation,
        ) -> Result<AdditiveScattering, EvaluationError>,
    {
        let mut cursor = 0_usize;
        let result = self.integration.finish(|level, node_index, node| {
            let position = cursor;
            let evaluation = self
                .evaluations
                .get(position)
                .expect("prepared ISHMAEL workload matches both quadrature grids");
            cursor += 1;
            debug_assert_eq!(
                (level, node_index),
                (evaluation.level, evaluation.node_index)
            );
            match &evaluation.route {
                PreparedIshmaelParticleRoute::TMatrixTable { table, prepared } => {
                    evaluate_table(position, level, node_index, node, table, prepared)
                }
                PreparedIshmaelParticleRoute::CpuRayleighBridge { .. } => evaluation.evaluate_cpu(),
            }
        });
        if let Ok(result) = &result {
            let base_node_count = self.integration.node_count(PsdQuadratureLevel::Coarse)
                + self.integration.node_count(PsdQuadratureLevel::Refined);
            let expected = if result.audit().refinement_steps == 0 {
                base_node_count
            } else {
                self.evaluations.len()
            };
            debug_assert_eq!(cursor, expected);
        }
        result
    }

    fn finish_cpu(&self) -> Result<PsdIntegrationResult, PsdIntegrationError<EvaluationError>> {
        self.finish_with_table_evaluator(|_, _, _, _, table, prepared| {
            table.evaluate_prepared_dry_particle_lut_interpolation_per_m3(prepared)
        })
    }
}

fn evaluate_ishmael_rayleigh_bridge_node(
    table: &ResearchTMatrixLut,
    scattering_node: IshmaelScatteringParticleNode,
    temperature_k: f64,
    fall_speed: PsdFallSpeedProvenance,
    request: TMatrixEvaluationRequest,
) -> Result<AdditiveScattering, EvaluationError> {
    let source = scattering_node.source();
    if source.scattering_route()
        != IshmaelParticleScatteringRoute::TableFloorAnchoredExactSphereRayleighV1
        || source.habit() != PsdSpheroidHabit::Spherical
        || scattering_node.habit() != PsdSpheroidHabit::Spherical
        || source.a_semi_axis_m().to_bits() != source.c_semi_axis_m().to_bits()
        || scattering_node.a_semi_axis_m().to_bits() != scattering_node.c_semi_axis_m().to_bits()
        || source.minor_to_major_axis_ratio().to_bits() != 1.0_f64.to_bits()
        || scattering_node.minor_to_major_axis_ratio().to_bits() != 1.0_f64.to_bits()
    {
        return Err(EvaluationError::InvalidQuery {
            field: "ISHMAEL Rayleigh bridge exact-sphere contract",
            value: scattering_node.minor_to_major_axis_ratio(),
        });
    }
    let target_diameter_m = scattering_node.equivolume_diameter_m();
    let target_density_kg_m3 = scattering_node.bulk_density_kg_m3();
    let domain = table.dry_particle_node_domain()?;
    let diameter_range = domain.equivolume_diameter_range_m();
    let density_range = domain.bulk_density_range_kg_m3();
    let ratio_range = domain.minor_to_major_axis_ratio_range();
    if target_diameter_m >= diameter_range[0]
        || target_density_kg_m3 < density_range[0]
        || target_density_kg_m3 > density_range[1]
        || ratio_range[0] > 1.0
        || ratio_range[1] < 1.0
    {
        return Err(EvaluationError::InvalidQuery {
            field: "ISHMAEL Rayleigh bridge table-floor density/diameter contract",
            value: target_diameter_m,
        });
    }
    let target_speed_m_s =
        table.dry_particle_geometry_terminal_speed_m_s(target_diameter_m, target_density_kg_m3)?;
    evaluate_table_floor_anchored_rayleigh_sphere(
        table,
        target_diameter_m,
        target_density_kg_m3,
        target_speed_m_s,
        temperature_k,
        source.rime_mass_fraction(),
        source.rime_density_kg_m3(),
        fall_speed,
        request,
    )
}

#[allow(clippy::too_many_arguments)]
fn prepare_ishmael_particle_integration<'table>(
    distribution: &IshmaelPsd,
    config: PsdIntegrationConfig,
    support: PsdParticleSupport,
    fall_speed: PsdFallSpeedProvenance,
    authenticated_ice_material_density_kg_m3: f64,
    tables: WrfTMatrixLutBundle<'table>,
    temperature_k: f64,
    oblate_request: TMatrixEvaluationRequest,
    prolate_request: TMatrixEvaluationRequest,
) -> Result<PreparedIshmaelParticleIntegration<'table>, PsdIntegrationError<EvaluationError>> {
    let integration = prepare_ishmael_psd_with_solid_ice_material_closure(
        distribution,
        config,
        support,
        fall_speed,
        authenticated_ice_material_density_kg_m3,
    )
    .map_err(PsdIntegrationError::<EvaluationError>::from)?;
    let mut evaluations = Vec::with_capacity(
        integration.node_count(PsdQuadratureLevel::Coarse)
            + integration.node_count(PsdQuadratureLevel::Refined)
            + integration.node_count(PsdQuadratureLevel::AdaptiveRefined),
    );
    for (level, node_index, node) in integration.nodes() {
        let scattering_node = integration.scattering_particle(node);
        let (role, table, request) = match scattering_node.habit() {
            PsdSpheroidHabit::Oblate | PsdSpheroidHabit::Spherical => (
                WrfTMatrixCudaTableRole::DryOblate,
                tables.dry_oblate,
                oblate_request,
            ),
            PsdSpheroidHabit::Prolate => (
                WrfTMatrixCudaTableRole::DryProlate,
                tables.dry_prolate,
                prolate_request,
            ),
        };
        let route = match node.scattering_route() {
            IshmaelParticleScatteringRoute::TMatrixTable => {
                let prepared = table
                    .prepare_dry_particle_geometry_lut_interpolation_per_m3(
                        temperature_k,
                        scattering_node.equivolume_diameter_m(),
                        scattering_node.bulk_density_kg_m3(),
                        scattering_node.minor_to_major_axis_ratio(),
                        scattering_node.habit(),
                        node.rime_mass_fraction(),
                        node.rime_density_kg_m3(),
                        fall_speed,
                        gaussian20_orientation(),
                        request,
                    )
                    .map_err(|source| PsdIntegrationError::NodeEvaluation {
                        level,
                        node_index,
                        source,
                    })?;
                PreparedIshmaelParticleRoute::TMatrixTable { table, prepared }
            }
            IshmaelParticleScatteringRoute::TableFloorAnchoredExactSphereRayleighV1 => {
                PreparedIshmaelParticleRoute::CpuRayleighBridge {
                    table,
                    scattering_node,
                    temperature_k,
                    fall_speed,
                    request,
                }
            }
        };
        evaluations.push(PreparedIshmaelParticleEvaluation {
            level,
            node_index,
            role,
            route,
        });
    }
    Ok(PreparedIshmaelParticleIntegration {
        integration,
        evaluations,
    })
}

fn finish_ishmael_particle_integration(
    prepared: &PreparedIshmaelParticleIntegration<'_>,
    cuda: Option<&WrfTMatrixCudaBatchService>,
    cancel: Option<&AtomicBool>,
) -> Result<PsdIntegrationResult, ParticleFinishError<PsdIntegrationError<EvaluationError>>> {
    check_cancel(cancel).map_err(|()| ParticleFinishError::Cancelled)?;
    let Some(cuda) = cuda else {
        return finish_ishmael_cpu_cancellable(prepared, cancel);
    };
    if cuda.is_disabled() {
        return finish_ishmael_cpu_cancellable(prepared, cancel);
    }

    let mut oblate_positions = Vec::new();
    let mut oblate_nodes = Vec::new();
    let mut prolate_positions = Vec::new();
    let mut prolate_nodes = Vec::new();
    for (position, evaluation) in prepared.evaluations.iter().enumerate() {
        check_cancel(cancel).map_err(|()| ParticleFinishError::Cancelled)?;
        let Some((_table, admitted)) = evaluation.table_binding() else {
            continue;
        };
        let cuda_node = match CudaPreparedTMatrixNode::new(admitted, 1.0) {
            Ok(cuda_node) => cuda_node,
            Err(error) => {
                check_cancel(cancel).map_err(|()| ParticleFinishError::Cancelled)?;
                cuda.disable_for_cpu_fallback(
                    format!(
                        "stage admitted ISHMAEL {:?} node {} for CUDA: {error}",
                        evaluation.level, evaluation.node_index
                    ),
                    0,
                );
                return finish_ishmael_cpu_cancellable(prepared, cancel);
            }
        };
        match evaluation.role {
            WrfTMatrixCudaTableRole::DryOblate => {
                oblate_positions.push(position);
                oblate_nodes.push(cuda_node);
            }
            WrfTMatrixCudaTableRole::DryProlate => {
                prolate_positions.push(position);
                prolate_nodes.push(cuda_node);
            }
        }
    }

    let mut outputs = vec![None; prepared.evaluations.len()];
    for (role, positions, nodes) in [
        (
            WrfTMatrixCudaTableRole::DryOblate,
            oblate_positions,
            oblate_nodes,
        ),
        (
            WrfTMatrixCudaTableRole::DryProlate,
            prolate_positions,
            prolate_nodes,
        ),
    ] {
        if nodes.is_empty() {
            continue;
        }
        check_cancel(cancel).map_err(|()| ParticleFinishError::Cancelled)?;
        let evaluated = match cuda.evaluate_particles(role, nodes) {
            Ok(evaluated) => evaluated,
            Err(_) => {
                check_cancel(cancel).map_err(|()| ParticleFinishError::Cancelled)?;
                return finish_ishmael_cpu_cancellable(prepared, cancel);
            }
        };
        check_cancel(cancel).map_err(|()| ParticleFinishError::Cancelled)?;
        debug_assert_eq!(positions.len(), evaluated.len());
        for (position, output) in positions.into_iter().zip(evaluated) {
            outputs[position] = Some(output);
        }
    }
    if prepared
        .evaluations
        .iter()
        .enumerate()
        .any(|(position, evaluation)| {
            evaluation.table_binding().is_some() && outputs[position].is_none()
        })
    {
        check_cancel(cancel).map_err(|()| ParticleFinishError::Cancelled)?;
        cuda.disable_for_cpu_fallback(
            "CUDA omitted an admitted ISHMAEL particle output".to_owned(),
            prepared.evaluations.len(),
        );
        return finish_ishmael_cpu_cancellable(prepared, cancel);
    }

    prepared
        .finish_with_table_evaluator(|position, _, _, _, _, _| {
            Ok(outputs[position].expect("all ISHMAEL LUT-backed CUDA outputs were checked above"))
        })
        .map_err(ParticleFinishError::Science)
}

fn finish_ishmael_cpu_cancellable(
    prepared: &PreparedIshmaelParticleIntegration<'_>,
    cancel: Option<&AtomicBool>,
) -> Result<PsdIntegrationResult, ParticleFinishError<PsdIntegrationError<EvaluationError>>> {
    let Some(_) = cancel else {
        return prepared.finish_cpu().map_err(ParticleFinishError::Science);
    };
    let mut outputs = Vec::with_capacity(prepared.evaluations.len());
    for evaluation in &prepared.evaluations {
        check_cancel(cancel).map_err(|()| ParticleFinishError::Cancelled)?;
        outputs.push(evaluation.evaluate_cpu().map_err(|source| {
            ParticleFinishError::Science(PsdIntegrationError::NodeEvaluation {
                level: evaluation.level,
                node_index: evaluation.node_index,
                source,
            })
        })?);
    }
    check_cancel(cancel).map_err(|()| ParticleFinishError::Cancelled)?;
    prepared
        .finish_with_table_evaluator(|position, _, _, _, _, _| Ok(outputs[position]))
        .map_err(ParticleFinishError::Science)
}

fn evaluate_ishmael_psd_raw_cell(
    raw: &RawPropertyCell,
    tables: WrfTMatrixLutBundle<'_>,
    validated: ValidatedBundle<'_>,
    elevation_deg: f64,
    cuda: Option<&WrfTMatrixCudaBatchService>,
    cancel: Option<&AtomicBool>,
) -> Result<Option<PolarAccumulatorQuantities>, WrfTMatrixRawEvaluationError> {
    let mut frozen = Vec::with_capacity(raw.categories().len());
    for category in raw.categories() {
        if category.mixing_ratio_kgkg() == 0.0 {
            continue;
        }
        let RawPropertyCategory::Ishmael(value) = category else {
            return Err(WrfTMatrixRawEvaluationError::IshmaelCategoryLayout(
                category.category(),
            ));
        };
        let ishmael_category = value.category.ishmael_category().ok_or(
            WrfTMatrixRawEvaluationError::IshmaelCategoryLayout(value.category),
        )?;
        let distribution = IshmaelPsd::reconstruct_wrf_source_checked(
            IshmaelPsdInput::new(
                ishmael_category,
                value.qice_kgkg,
                value.qnice_per_kg,
                value.qvoli_m3_per_kg,
                value.qaoli_m3_per_kg,
                raw.dry_air_density_kg_m3(),
            ),
            raw.environment().temperature_k(),
        )
        .map_err(
            |source| WrfTMatrixRawEvaluationError::IshmaelPsdReconstruction {
                category: value.category,
                source,
            },
        )?;
        frozen.push((value.category, distribution));
    }
    let rain = match close_raw_rain_state(raw, OrientationDefinition::Gaussian20Research)
        .map_err(WrfTMatrixRawEvaluationError::Closure)?
    {
        ClosedRainState::Clear => None,
        ClosedRainState::Closed(rain) => Some(rain),
        ClosedRainState::Unavailable(reason) => {
            return Err(WrfTMatrixRawEvaluationError::RainUnavailable(reason));
        }
    };
    if frozen.is_empty() && rain.is_none() {
        return Ok(None);
    }
    let oblate_request = raw_evaluation_request(
        validated.frequency_hz,
        elevation_deg,
        SpheroidConvention::OblateMinorVertical,
    )?;
    let prolate_request = raw_evaluation_request(
        validated.frequency_hz,
        elevation_deg,
        SpheroidConvention::ProlateMajorVertical,
    )?;
    let mut additive = AdditiveScattering::default();
    for &(category, distribution) in &frozen {
        let prepared = prepare_ishmael_particle_integration(
            &distribution,
            validated.ishmael_psd_config,
            validated.ishmael_particle_support,
            validated.ishmael_fall_speed,
            validated.ishmael_ice_material_density_kg_m3,
            tables,
            dry_frozen_particle_temperature_k(raw.environment().temperature_k()),
            oblate_request,
            prolate_request,
        )
        .map_err(
            |source| WrfTMatrixRawEvaluationError::IshmaelPsdIntegration { category, source },
        )?;
        let integration =
            finish_ishmael_particle_integration(&prepared, cuda, cancel).map_err(|error| {
                match error {
                    ParticleFinishError::Cancelled => WrfTMatrixRawEvaluationError::Cancelled,
                    ParticleFinishError::Science(source) => {
                        WrfTMatrixRawEvaluationError::IshmaelPsdIntegration { category, source }
                    }
                }
            })?;
        additive = additive
            .checked_add(integration.additive())
            .map_err(|source| WrfTMatrixRawEvaluationError::Accumulation {
                contribution: WrfTMatrixContribution::IshmaelSchemeNativePsd,
                source,
            })?;
    }
    if let Some(rain) = rain.as_deref() {
        let contribution = tables
            .rain_standalone_and_residual
            .evaluate(rain, oblate_request)
            .map_err(|source| WrfTMatrixRawEvaluationError::Evaluation {
                contribution: WrfTMatrixContribution::StandaloneRain,
                source,
            })?;
        additive = additive.checked_add(contribution).map_err(|source| {
            WrfTMatrixRawEvaluationError::Accumulation {
                contribution: WrfTMatrixContribution::StandaloneRain,
                source,
            }
        })?;
    }
    additive
        .to_polar_accumulator_quantities()
        .map(Some)
        .map_err(WrfTMatrixRawEvaluationError::Output)
}

struct PreparedP3RawBatchCategory<'table> {
    category: WrfPropertyCategory,
    integration: PreparedP3ParticleIntegration<'table>,
    cuda_outputs: Vec<Option<AdditiveScattering>>,
}

struct PreparedP3RawBatchEvaluation<'table> {
    categories: Vec<PreparedP3RawBatchCategory<'table>>,
    rain: Option<Box<ClosedParticleCategory>>,
    rain_request: TMatrixEvaluationRequest,
    rain_table: &'table ResearchTMatrixLut,
}

struct PreparedIshmaelRawBatchCategory<'table> {
    category: WrfPropertyCategory,
    integration: PreparedIshmaelParticleIntegration<'table>,
    cuda_outputs: Vec<Option<AdditiveScattering>>,
}

struct PreparedIshmaelRawBatchEvaluation<'table> {
    categories: Vec<PreparedIshmaelRawBatchCategory<'table>>,
    rain: Option<Box<ClosedParticleCategory>>,
    rain_request: TMatrixEvaluationRequest,
    rain_table: &'table ResearchTMatrixLut,
}

enum PreparedRawBatchEvaluation<'table> {
    Clear,
    P3(PreparedP3RawBatchEvaluation<'table>),
    Ishmael(PreparedIshmaelRawBatchEvaluation<'table>),
}

#[derive(Clone, Copy, Debug)]
enum RawBatchNodeLocation {
    P3 {
        request_index: usize,
        category_index: usize,
        particle_position: usize,
    },
    Ishmael {
        request_index: usize,
        category_index: usize,
        particle_position: usize,
    },
}

struct RawBatchRoleSweep {
    locations: Vec<RawBatchNodeLocation>,
    nodes: Vec<CudaPreparedTMatrixNode>,
}

impl RawBatchRoleSweep {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            locations: Vec::with_capacity(capacity),
            nodes: Vec::with_capacity(capacity),
        }
    }
}

fn raw_batch_role_node_counts(prepared: &[PreparedRawBatchEvaluation<'_>]) -> (usize, usize) {
    let mut oblate = 0_usize;
    let mut prolate = 0_usize;
    for request in prepared {
        match request {
            PreparedRawBatchEvaluation::Clear => {}
            PreparedRawBatchEvaluation::P3(request) => {
                for category in &request.categories {
                    for (_, _, node, _, _) in category.integration.table_nodes() {
                        match node.habit() {
                            PsdSpheroidHabit::Oblate | PsdSpheroidHabit::Spherical => {
                                oblate = oblate.saturating_add(1);
                            }
                            PsdSpheroidHabit::Prolate => {
                                prolate = prolate.saturating_add(1);
                            }
                        }
                    }
                }
            }
            PreparedRawBatchEvaluation::Ishmael(request) => {
                for category in &request.categories {
                    for evaluation in &category.integration.evaluations {
                        if evaluation.table_binding().is_none() {
                            continue;
                        }
                        match evaluation.role {
                            WrfTMatrixCudaTableRole::DryOblate => {
                                oblate = oblate.saturating_add(1);
                            }
                            WrfTMatrixCudaTableRole::DryProlate => {
                                prolate = prolate.saturating_add(1);
                            }
                        }
                    }
                }
            }
        }
    }
    (oblate, prolate)
}

#[allow(clippy::too_many_arguments)]
fn prepare_p3_raw_batch_evaluation<'table>(
    raw: &RawPropertyCell,
    tables: WrfTMatrixLutBundle<'table>,
    validated: ValidatedBundle<'table>,
    p3: &WrfTMatrixP3Resources,
    elevation_deg: f64,
    cancel: Option<&AtomicBool>,
) -> Result<PreparedRawBatchEvaluation<'table>, WrfTMatrixRawEvaluationError> {
    let scheme = P3WrfScheme::try_from(raw.microphysics_scheme_id()).map_err(|source| {
        WrfTMatrixRawEvaluationError::P3PsdReconstruction {
            category: WrfPropertyCategory::P3(radar_scattering::P3Category::Category1),
            source,
        }
    })?;
    let mut frozen = Vec::with_capacity(raw.categories().len());
    for category in raw.categories() {
        check_cancel(cancel).map_err(|()| WrfTMatrixRawEvaluationError::Cancelled)?;
        if category.mixing_ratio_kgkg() == 0.0 {
            continue;
        }
        let RawPropertyCategory::P3(value) = category else {
            return Err(WrfTMatrixRawEvaluationError::P3CategoryLayout(
                category.category(),
            ));
        };
        let input = p3_psd_input(scheme, value, raw.dry_air_density_kg_m3()).ok_or(
            WrfTMatrixRawEvaluationError::MissingP3Qzi(WrfPropertyCategory::P3(value.category)),
        )?;
        let distribution =
            P3Psd::reconstruct(input, p3.table.as_ref(), P3ReconstructionConfig::default())
                .map_err(|source| WrfTMatrixRawEvaluationError::P3PsdReconstruction {
                    category: WrfPropertyCategory::P3(value.category),
                    source,
                })?;
        frozen.push((WrfPropertyCategory::P3(value.category), distribution));
    }
    let rain = match close_raw_rain_state(raw, OrientationDefinition::Gaussian20Research)
        .map_err(WrfTMatrixRawEvaluationError::Closure)?
    {
        ClosedRainState::Clear => None,
        ClosedRainState::Closed(rain) => Some(rain),
        ClosedRainState::Unavailable(reason) => {
            return Err(WrfTMatrixRawEvaluationError::RainUnavailable(reason));
        }
    };
    if frozen.is_empty() && rain.is_none() {
        return Ok(PreparedRawBatchEvaluation::Clear);
    }

    let oblate_request = raw_evaluation_request(
        validated.frequency_hz,
        elevation_deg,
        SpheroidConvention::OblateMinorVertical,
    )?;
    let prolate_request = raw_evaluation_request(
        validated.frequency_hz,
        elevation_deg,
        SpheroidConvention::ProlateMajorVertical,
    )?;
    let mut categories = Vec::with_capacity(frozen.len());
    for (category, distribution) in &frozen {
        check_cancel(cancel).map_err(|()| WrfTMatrixRawEvaluationError::Cancelled)?;
        let p3_input = distribution.input();
        let rime_fraction = p3_input.rime_mass_kgkg / p3_input.total_ice_kgkg;
        let rime_density = if p3_input.rime_mass_kgkg > 0.0 {
            Some(p3_input.rime_mass_kgkg / p3_input.rime_volume_m3_per_kg)
        } else {
            None
        };
        let integration = prepare_p3_particle_integration(
            distribution,
            p3.integration,
            validated.p3_particle_support,
            tables,
            dry_frozen_particle_temperature_k(raw.environment().temperature_k()),
            rime_fraction,
            rime_density,
            validated.p3_fall_speed,
            oblate_request,
            prolate_request,
        )
        .map_err(|source| WrfTMatrixRawEvaluationError::P3PsdIntegration {
            category: *category,
            source,
        })?;
        let cuda_outputs = (0..integration.evaluations.len()).map(|_| None).collect();
        categories.push(PreparedP3RawBatchCategory {
            category: *category,
            integration,
            cuda_outputs,
        });
    }
    Ok(PreparedRawBatchEvaluation::P3(
        PreparedP3RawBatchEvaluation {
            categories,
            rain,
            rain_request: oblate_request,
            rain_table: tables.rain_standalone_and_residual,
        },
    ))
}

fn prepare_ishmael_raw_batch_evaluation<'table>(
    raw: &RawPropertyCell,
    tables: WrfTMatrixLutBundle<'table>,
    validated: ValidatedBundle<'table>,
    elevation_deg: f64,
    cancel: Option<&AtomicBool>,
) -> Result<PreparedRawBatchEvaluation<'table>, WrfTMatrixRawEvaluationError> {
    let mut frozen = Vec::with_capacity(raw.categories().len());
    for category in raw.categories() {
        check_cancel(cancel).map_err(|()| WrfTMatrixRawEvaluationError::Cancelled)?;
        if category.mixing_ratio_kgkg() == 0.0 {
            continue;
        }
        let RawPropertyCategory::Ishmael(value) = category else {
            return Err(WrfTMatrixRawEvaluationError::IshmaelCategoryLayout(
                category.category(),
            ));
        };
        let ishmael_category = value.category.ishmael_category().ok_or(
            WrfTMatrixRawEvaluationError::IshmaelCategoryLayout(value.category),
        )?;
        let distribution = IshmaelPsd::reconstruct_wrf_source_checked(
            IshmaelPsdInput::new(
                ishmael_category,
                value.qice_kgkg,
                value.qnice_per_kg,
                value.qvoli_m3_per_kg,
                value.qaoli_m3_per_kg,
                raw.dry_air_density_kg_m3(),
            ),
            raw.environment().temperature_k(),
        )
        .map_err(
            |source| WrfTMatrixRawEvaluationError::IshmaelPsdReconstruction {
                category: value.category,
                source,
            },
        )?;
        frozen.push((value.category, distribution));
    }
    let rain = match close_raw_rain_state(raw, OrientationDefinition::Gaussian20Research)
        .map_err(WrfTMatrixRawEvaluationError::Closure)?
    {
        ClosedRainState::Clear => None,
        ClosedRainState::Closed(rain) => Some(rain),
        ClosedRainState::Unavailable(reason) => {
            return Err(WrfTMatrixRawEvaluationError::RainUnavailable(reason));
        }
    };
    if frozen.is_empty() && rain.is_none() {
        return Ok(PreparedRawBatchEvaluation::Clear);
    }

    let oblate_request = raw_evaluation_request(
        validated.frequency_hz,
        elevation_deg,
        SpheroidConvention::OblateMinorVertical,
    )?;
    let prolate_request = raw_evaluation_request(
        validated.frequency_hz,
        elevation_deg,
        SpheroidConvention::ProlateMajorVertical,
    )?;
    let mut categories = Vec::with_capacity(frozen.len());
    for &(category, distribution) in &frozen {
        check_cancel(cancel).map_err(|()| WrfTMatrixRawEvaluationError::Cancelled)?;
        let integration = prepare_ishmael_particle_integration(
            &distribution,
            validated.ishmael_psd_config,
            validated.ishmael_particle_support,
            validated.ishmael_fall_speed,
            validated.ishmael_ice_material_density_kg_m3,
            tables,
            dry_frozen_particle_temperature_k(raw.environment().temperature_k()),
            oblate_request,
            prolate_request,
        )
        .map_err(
            |source| WrfTMatrixRawEvaluationError::IshmaelPsdIntegration { category, source },
        )?;
        let cuda_outputs = (0..integration.evaluations.len()).map(|_| None).collect();
        categories.push(PreparedIshmaelRawBatchCategory {
            category,
            integration,
            cuda_outputs,
        });
    }
    Ok(PreparedRawBatchEvaluation::Ishmael(
        PreparedIshmaelRawBatchEvaluation {
            categories,
            rain,
            rain_request: oblate_request,
            rain_table: tables.rain_standalone_and_residual,
        },
    ))
}

fn finish_raw_batch(
    mut prepared: Vec<PreparedRawBatchEvaluation<'_>>,
    cuda: Option<&WrfTMatrixCudaBatchService>,
    cancel: Option<&AtomicBool>,
) -> Result<Vec<Option<PolarAccumulatorQuantities>>, WrfTMatrixRawBatchError> {
    check_cancel(cancel).map_err(|()| WrfTMatrixRawBatchError::Cancelled)?;
    let Some(cuda) = cuda else {
        return finish_raw_batch_ordered(prepared, false, cancel);
    };
    if cuda.is_disabled() {
        return finish_raw_batch_ordered(prepared, false, cancel);
    }

    let (oblate_node_count, prolate_node_count) = raw_batch_role_node_counts(&prepared);
    let mut oblate = RawBatchRoleSweep::with_capacity(oblate_node_count);
    let mut prolate = RawBatchRoleSweep::with_capacity(prolate_node_count);
    let staging_failure = 'staging: {
        for (request_index, request) in prepared.iter().enumerate() {
            check_cancel(cancel).map_err(|()| WrfTMatrixRawBatchError::Cancelled)?;
            match request {
                PreparedRawBatchEvaluation::Clear => {}
                PreparedRawBatchEvaluation::P3(request) => {
                    for (category_index, category) in request.categories.iter().enumerate() {
                        for (particle_position, node_index, node, _table, admitted) in
                            category.integration.table_nodes()
                        {
                            check_cancel(cancel)
                                .map_err(|()| WrfTMatrixRawBatchError::Cancelled)?;
                            let cuda_node = match CudaPreparedTMatrixNode::new(admitted, 1.0) {
                                Ok(cuda_node) => cuda_node,
                                Err(error) => {
                                    check_cancel(cancel)
                                        .map_err(|()| WrfTMatrixRawBatchError::Cancelled)?;
                                    break 'staging Some(format!(
                                        "stage ordered raw batch request {request_index}, P3 category {}, node {node_index} for CUDA: {error}",
                                        category.category
                                    ));
                                }
                            };
                            let sweep = match node.habit() {
                                PsdSpheroidHabit::Oblate | PsdSpheroidHabit::Spherical => {
                                    &mut oblate
                                }
                                PsdSpheroidHabit::Prolate => &mut prolate,
                            };
                            sweep.locations.push(RawBatchNodeLocation::P3 {
                                request_index,
                                category_index,
                                particle_position,
                            });
                            sweep.nodes.push(cuda_node);
                        }
                    }
                }
                PreparedRawBatchEvaluation::Ishmael(request) => {
                    for (category_index, category) in request.categories.iter().enumerate() {
                        for (particle_position, evaluation) in
                            category.integration.evaluations.iter().enumerate()
                        {
                            check_cancel(cancel)
                                .map_err(|()| WrfTMatrixRawBatchError::Cancelled)?;
                            let Some((_table, admitted)) = evaluation.table_binding() else {
                                continue;
                            };
                            let cuda_node = match CudaPreparedTMatrixNode::new(admitted, 1.0) {
                                Ok(cuda_node) => cuda_node,
                                Err(error) => {
                                    check_cancel(cancel)
                                        .map_err(|()| WrfTMatrixRawBatchError::Cancelled)?;
                                    break 'staging Some(format!(
                                        "stage ordered raw batch request {request_index}, ISHMAEL category {}, {:?} node {} for CUDA: {error}",
                                        category.category, evaluation.level, evaluation.node_index
                                    ));
                                }
                            };
                            let sweep = match evaluation.role {
                                WrfTMatrixCudaTableRole::DryOblate => &mut oblate,
                                WrfTMatrixCudaTableRole::DryProlate => &mut prolate,
                            };
                            sweep.locations.push(RawBatchNodeLocation::Ishmael {
                                request_index,
                                category_index,
                                particle_position,
                            });
                            sweep.nodes.push(cuda_node);
                        }
                    }
                }
            }
        }
        None
    };
    if let Some(detail) = staging_failure {
        check_cancel(cancel).map_err(|()| WrfTMatrixRawBatchError::Cancelled)?;
        cuda.disable_for_cpu_fallback(detail, 0);
        return finish_raw_batch_ordered(prepared, false, cancel);
    }
    debug_assert_eq!(oblate.locations.len(), oblate_node_count);
    debug_assert_eq!(oblate.nodes.len(), oblate_node_count);
    debug_assert_eq!(prolate.locations.len(), prolate_node_count);
    debug_assert_eq!(prolate.nodes.len(), prolate_node_count);

    let RawBatchRoleSweep {
        locations: oblate_locations,
        nodes: oblate_nodes,
    } = oblate;
    let RawBatchRoleSweep {
        locations: prolate_locations,
        nodes: prolate_nodes,
    } = prolate;
    check_cancel(cancel).map_err(|()| WrfTMatrixRawBatchError::Cancelled)?;
    let oblate_outputs = if oblate_nodes.is_empty() {
        Vec::new()
    } else {
        match cuda.evaluate_particles(WrfTMatrixCudaTableRole::DryOblate, oblate_nodes) {
            Ok(outputs) => outputs,
            Err(_) => {
                check_cancel(cancel).map_err(|()| WrfTMatrixRawBatchError::Cancelled)?;
                return finish_raw_batch_ordered(prepared, false, cancel);
            }
        }
    };
    check_cancel(cancel).map_err(|()| WrfTMatrixRawBatchError::Cancelled)?;
    let prolate_outputs = if prolate_nodes.is_empty() {
        Vec::new()
    } else {
        match cuda.evaluate_particles(WrfTMatrixCudaTableRole::DryProlate, prolate_nodes) {
            Ok(outputs) => outputs,
            Err(_) => {
                check_cancel(cancel).map_err(|()| WrfTMatrixRawBatchError::Cancelled)?;
                return finish_raw_batch_ordered(prepared, false, cancel);
            }
        }
    };
    check_cancel(cancel).map_err(|()| WrfTMatrixRawBatchError::Cancelled)?;
    if oblate_locations.len() != oblate_outputs.len()
        || prolate_locations.len() != prolate_outputs.len()
    {
        cuda.disable_for_cpu_fallback(
            "CUDA omitted an ordered raw-batch particle output".to_owned(),
            oblate_outputs.len().saturating_add(prolate_outputs.len()),
        );
        return finish_raw_batch_ordered(prepared, false, cancel);
    }

    install_raw_batch_outputs(&mut prepared, oblate_locations, oblate_outputs);
    install_raw_batch_outputs(&mut prepared, prolate_locations, prolate_outputs);
    finish_raw_batch_ordered(prepared, true, cancel)
}

fn install_raw_batch_outputs(
    prepared: &mut [PreparedRawBatchEvaluation<'_>],
    locations: Vec<RawBatchNodeLocation>,
    outputs: Vec<AdditiveScattering>,
) {
    for (location, output) in locations.into_iter().zip(outputs) {
        match location {
            RawBatchNodeLocation::P3 {
                request_index,
                category_index,
                particle_position,
            } => {
                let PreparedRawBatchEvaluation::P3(request) = &mut prepared[request_index] else {
                    unreachable!("P3 batch location points at a P3 prepared request")
                };
                request.categories[category_index].cuda_outputs[particle_position] = Some(output);
            }
            RawBatchNodeLocation::Ishmael {
                request_index,
                category_index,
                particle_position,
            } => {
                let PreparedRawBatchEvaluation::Ishmael(request) = &mut prepared[request_index]
                else {
                    unreachable!("ISHMAEL batch location points at an ISHMAEL prepared request")
                };
                request.categories[category_index].cuda_outputs[particle_position] = Some(output);
            }
        }
    }
}

fn finish_raw_batch_ordered(
    prepared: Vec<PreparedRawBatchEvaluation<'_>>,
    use_cuda_outputs: bool,
    cancel: Option<&AtomicBool>,
) -> Result<Vec<Option<PolarAccumulatorQuantities>>, WrfTMatrixRawBatchError> {
    let mut outputs = Vec::with_capacity(prepared.len());
    for (request_index, request) in prepared.into_iter().enumerate() {
        check_cancel(cancel).map_err(|()| WrfTMatrixRawBatchError::Cancelled)?;
        let result = match request {
            PreparedRawBatchEvaluation::Clear => Ok(None),
            PreparedRawBatchEvaluation::P3(request) => {
                finish_prepared_p3_raw_batch(request, use_cuda_outputs, cancel)
            }
            PreparedRawBatchEvaluation::Ishmael(request) => {
                finish_prepared_ishmael_raw_batch(request, use_cuda_outputs, cancel)
            }
        };
        match result {
            Ok(output) => outputs.push(output),
            Err(WrfTMatrixRawEvaluationError::Cancelled) => {
                return Err(WrfTMatrixRawBatchError::Cancelled);
            }
            Err(source) => {
                return Err(WrfTMatrixRawBatchError::Request {
                    request_index,
                    source,
                });
            }
        }
    }
    Ok(outputs)
}

fn finish_prepared_p3_raw_batch(
    request: PreparedP3RawBatchEvaluation<'_>,
    use_cuda_outputs: bool,
    cancel: Option<&AtomicBool>,
) -> Result<Option<PolarAccumulatorQuantities>, WrfTMatrixRawEvaluationError> {
    let mut additive = AdditiveScattering::default();
    for category in &request.categories {
        check_cancel(cancel).map_err(|()| WrfTMatrixRawEvaluationError::Cancelled)?;
        let integration = if use_cuda_outputs {
            category
                .integration
                .finish_with_table_evaluator(|position, _, _, _, _| {
                    Ok(category.cuda_outputs[position]
                        .expect("both CUDA role sweeps populated every P3 table-node output"))
                })
                .map_err(|source| WrfTMatrixRawEvaluationError::P3PsdIntegration {
                    category: category.category,
                    source,
                })?
        } else {
            finish_p3_cpu_cancellable(&category.integration, cancel).map_err(
                |error| match error {
                    ParticleFinishError::Cancelled => WrfTMatrixRawEvaluationError::Cancelled,
                    ParticleFinishError::Science(source) => {
                        WrfTMatrixRawEvaluationError::P3PsdIntegration {
                            category: category.category,
                            source,
                        }
                    }
                },
            )?
        };
        additive = additive
            .checked_add(integration.additive())
            .map_err(|source| WrfTMatrixRawEvaluationError::Accumulation {
                contribution: WrfTMatrixContribution::P3SchemeNativePsd,
                source,
            })?;
    }
    if let Some(rain) = request.rain.as_deref() {
        check_cancel(cancel).map_err(|()| WrfTMatrixRawEvaluationError::Cancelled)?;
        let contribution = request
            .rain_table
            .evaluate(rain, request.rain_request)
            .map_err(|source| WrfTMatrixRawEvaluationError::Evaluation {
                contribution: WrfTMatrixContribution::StandaloneRain,
                source,
            })?;
        additive = additive.checked_add(contribution).map_err(|source| {
            WrfTMatrixRawEvaluationError::Accumulation {
                contribution: WrfTMatrixContribution::StandaloneRain,
                source,
            }
        })?;
    }
    additive
        .to_polar_accumulator_quantities()
        .map(Some)
        .map_err(WrfTMatrixRawEvaluationError::Output)
}

fn finish_prepared_ishmael_raw_batch(
    request: PreparedIshmaelRawBatchEvaluation<'_>,
    use_cuda_outputs: bool,
    cancel: Option<&AtomicBool>,
) -> Result<Option<PolarAccumulatorQuantities>, WrfTMatrixRawEvaluationError> {
    let mut additive = AdditiveScattering::default();
    for category in &request.categories {
        check_cancel(cancel).map_err(|()| WrfTMatrixRawEvaluationError::Cancelled)?;
        let integration = if use_cuda_outputs {
            category
                .integration
                .finish_with_table_evaluator(|position, _, _, _, _, _| {
                    Ok(category.cuda_outputs[position]
                        .expect("both CUDA role sweeps populated every ISHMAEL LUT-backed output"))
                })
                .map_err(
                    |source| WrfTMatrixRawEvaluationError::IshmaelPsdIntegration {
                        category: category.category,
                        source,
                    },
                )?
        } else {
            finish_ishmael_cpu_cancellable(&category.integration, cancel).map_err(|error| {
                match error {
                    ParticleFinishError::Cancelled => WrfTMatrixRawEvaluationError::Cancelled,
                    ParticleFinishError::Science(source) => {
                        WrfTMatrixRawEvaluationError::IshmaelPsdIntegration {
                            category: category.category,
                            source,
                        }
                    }
                }
            })?
        };
        additive = additive
            .checked_add(integration.additive())
            .map_err(|source| WrfTMatrixRawEvaluationError::Accumulation {
                contribution: WrfTMatrixContribution::IshmaelSchemeNativePsd,
                source,
            })?;
    }
    if let Some(rain) = request.rain.as_deref() {
        check_cancel(cancel).map_err(|()| WrfTMatrixRawEvaluationError::Cancelled)?;
        let contribution = request
            .rain_table
            .evaluate(rain, request.rain_request)
            .map_err(|source| WrfTMatrixRawEvaluationError::Evaluation {
                contribution: WrfTMatrixContribution::StandaloneRain,
                source,
            })?;
        additive = additive.checked_add(contribution).map_err(|source| {
            WrfTMatrixRawEvaluationError::Accumulation {
                contribution: WrfTMatrixContribution::StandaloneRain,
                source,
            }
        })?;
    }
    additive
        .to_polar_accumulator_quantities()
        .map(Some)
        .map_err(WrfTMatrixRawEvaluationError::Output)
}

fn raw_evaluation_request(
    frequency_hz: f64,
    elevation_deg: f64,
    spheroid: SpheroidConvention,
) -> Result<TMatrixEvaluationRequest, WrfTMatrixRawEvaluationError> {
    let view = RadarViewGeometry::new(elevation_deg)
        .map_err(WrfTMatrixRawEvaluationError::EvaluationRequest)?;
    TMatrixEvaluationRequest::new(frequency_hz, spheroid, view)
        .map_err(WrfTMatrixRawEvaluationError::EvaluationRequest)
}

fn spheroid_for_raw_particle(
    particle: &ClosedParticleCategory,
) -> Result<SpheroidConvention, WrfTMatrixRawEvaluationError> {
    match particle.record().state() {
        ParticleState::P3(_) => Ok(SpheroidConvention::OblateMinorVertical),
        ParticleState::Ishmael(_) => {
            Err(WrfTMatrixRawEvaluationError::IshmaelCharacteristicForbidden)
        }
        ParticleState::Conventional(_) => Err(WrfTMatrixRawEvaluationError::WetSourceNotFrozen),
    }
}

fn verify_raw_fall_moment_policy(
    actual: FallMomentPolicy,
    expected: FallMomentPolicy,
    contribution: WrfTMatrixContribution,
) -> Result<(), WrfTMatrixRawEvaluationError> {
    if actual == expected {
        Ok(())
    } else {
        Err(WrfTMatrixRawEvaluationError::FallMomentPolicy {
            contribution,
            expected,
            actual,
        })
    }
}

fn compact_frozen_only_rows(
    components: &mut Vec<f32>,
    components_per_cell: usize,
    source_indices: &[u32],
    summaries: Vec<CellBuildSummary>,
) -> Result<(Vec<u32>, WrfTMatrixAuditCounts), WrfTMatrixSceneBuildError> {
    let expected_components = source_indices
        .len()
        .checked_mul(components_per_cell)
        .ok_or(WrfTMatrixSceneBuildError::SizeOverflow)?;
    if components.len() != expected_components {
        return Err(WrfTMatrixSceneBuildError::InternalComponentPlaneLength {
            expected: expected_components,
            actual: components.len(),
        });
    }
    if summaries.len() != source_indices.len() {
        return Err(WrfTMatrixSceneBuildError::InternalSummaryLength {
            expected: source_indices.len(),
            actual: summaries.len(),
        });
    }
    let mut retained_indices = Vec::with_capacity(source_indices.len());
    let mut retained_rows = 0_usize;
    let mut counts = WrfTMatrixAuditCounts::default();
    for (source_row, (source_index, summary)) in
        source_indices.iter().copied().zip(summaries).enumerate()
    {
        if !summary.retained {
            continue;
        }
        let source_start = source_row
            .checked_mul(components_per_cell)
            .ok_or(WrfTMatrixSceneBuildError::SizeOverflow)?;
        let source_end = source_start
            .checked_add(components_per_cell)
            .ok_or(WrfTMatrixSceneBuildError::SizeOverflow)?;
        let destination = retained_rows
            .checked_mul(components_per_cell)
            .ok_or(WrfTMatrixSceneBuildError::SizeOverflow)?;
        if source_start != destination {
            components.copy_within(source_start..source_end, destination);
        }
        retained_indices.push(source_index);
        retained_rows += 1;
        counts = counts
            .checked_add(summary.counts)
            .ok_or(WrfTMatrixSceneBuildError::AuditCountOverflow)?;
    }
    components.truncate(
        retained_rows
            .checked_mul(components_per_cell)
            .ok_or(WrfTMatrixSceneBuildError::SizeOverflow)?,
    );
    Ok((retained_indices, counts))
}

fn required_p3_table_kind(scheme_id: i32) -> Option<P3OfficialTableKind> {
    match scheme_id {
        50..=52 => Some(P3OfficialTableKind::TwoMoment),
        53 => Some(P3OfficialTableKind::ThreeMoment),
        _ => None,
    }
}

fn validate_scene_p3_resources(
    scheme_id: i32,
    p3: Option<WrfTMatrixP3Resources>,
) -> Result<Option<WrfTMatrixP3Resources>, WrfTMatrixSceneBuildError> {
    validate_scene_p3_resource_ref(scheme_id, p3.as_ref())?;
    Ok(p3)
}

fn validate_scene_p3_resource_ref(
    scheme_id: i32,
    p3: Option<&WrfTMatrixP3Resources>,
) -> Result<(), WrfTMatrixSceneBuildError> {
    let Some(expected) = required_p3_table_kind(scheme_id) else {
        return Ok(());
    };
    let resource = p3.ok_or_else(|| {
        let spec = expected.asset_spec();
        WrfTMatrixSceneBuildError::P3TableRequired {
            scheme_id,
            expected_file: spec.file_name,
            source_url: spec.source_url,
        }
    })?;
    if resource.table.kind() != expected {
        return Err(WrfTMatrixSceneBuildError::WrongP3Table {
            scheme_id,
            expected_file: expected.asset_spec().file_name,
            actual: resource.table.kind(),
        });
    }
    Ok(())
}

fn raw_p3_table_required(raw: &RawPropertyCell) -> WrfTMatrixRawEvaluationError {
    let expected = required_p3_table_kind(raw.microphysics_scheme_id())
        .expect("raw P3 dispatcher only requests P3 table contracts");
    let spec = expected.asset_spec();
    WrfTMatrixRawEvaluationError::P3TableRequired {
        scheme_id: raw.microphysics_scheme_id(),
        expected_file: spec.file_name,
        source_url: spec.source_url,
    }
}

fn validate_raw_p3_table(
    scheme_id: i32,
    table: &P3OfficialTableV54,
) -> Result<(), WrfTMatrixRawEvaluationError> {
    let expected = required_p3_table_kind(scheme_id)
        .expect("raw P3 dispatcher only validates P3 table contracts");
    if table.kind() != expected {
        return Err(WrfTMatrixRawEvaluationError::WrongP3Table {
            scheme_id,
            expected_file: expected.asset_spec().file_name,
            actual: table.kind(),
        });
    }
    Ok(())
}

fn validate_source_shape(source: &WrfPropertyScene) -> Result<(), WrfTMatrixSceneBuildError> {
    if source.cell_count() > u32::MAX as usize {
        return Err(WrfTMatrixSceneBuildError::GridTooLarge {
            cell_count: source.cell_count(),
        });
    }
    validate_sparse_indices(source)
}

fn estimate_build_peak_from_shape(
    source: &WrfPropertyScene,
    elevation_nodes: usize,
    table_id_text_bytes: usize,
    rain_mode: WrfTMatrixRainMode,
    parallel_workers: usize,
) -> Result<WrfTMatrixBuildPeakEstimate, WrfTMatrixSceneBuildError> {
    let active_source_cells = source.active_cell_indices().len();
    let components_per_cell =
        checked_product(&[elevation_nodes, AdditiveScattering::COMPONENT_COUNT])?;
    let component_buffer_bytes =
        checked_product(&[active_source_cells, components_per_cell, size_of::<f32>()])?;
    let dense_lookup_bytes = checked_product(&[source.cell_count(), size_of::<u32>()])?;
    let sparse_index_bytes = checked_product(&[active_source_cells, size_of::<u32>()])?;
    let frozen_filter_summary_bytes = if rain_mode == WrfTMatrixRainMode::FrozenOnly {
        checked_product(&[active_source_cells, size_of::<CellBuildSummary>()])?
    } else {
        0
    };

    let required_contract_bytes = checked_product(&[
        source.required_field_signature().fields.len(),
        size_of::<RequiredFieldContract>() + 4 * size_of::<usize>(),
    ])?;
    let source_field_structure_bytes = checked_product(&[
        source.source_fields().len(),
        size_of::<SourceFieldProvenance>(),
    ])?;
    let source_field_text_bytes = checked_sum(
        source
            .source_fields()
            .iter()
            .map(|field| field.source_units().len()),
    )?;
    let elevation_axis_bytes = checked_product(&[elevation_nodes, size_of::<f64>()])?;
    let scene_metadata_bytes = checked_sum([
        size_of::<WrfTMatrixScene>(),
        source.identity().source_identity.0.len(),
        required_contract_bytes,
        source_field_structure_bytes,
        source_field_text_bytes,
        table_id_text_bytes,
        elevation_axis_bytes,
    ])?;

    let closure_source_copy_bytes = checked_product(&[
        MAX_PROPERTY_CATEGORIES_PER_CELL + 1,
        checked_sum([
            source_field_structure_bytes,
            source_field_text_bytes,
            required_contract_bytes,
        ])?,
    ])?;
    let p3_quadrature_scratch_bytes = if (50..=53).contains(&source.microphysics_scheme_id()) {
        checked_product(&[
            MAX_PROPERTY_CATEGORIES_PER_CELL,
            radar_scattering::P3QuadratureConfig::default().maximum_nodes as usize,
            size_of::<P3QuadratureNode>(),
        ])?
    } else {
        0
    };
    let per_worker_scratch = checked_sum([
        checked_product(&[components_per_cell, size_of::<f32>()])?,
        size_of::<ClosedPropertyCell>(),
        checked_product(&[
            MAX_PROPERTY_CATEGORIES_PER_CELL,
            size_of::<ClosedCellCategory>(),
        ])?,
        checked_product(&[
            MAX_PROPERTY_CATEGORIES_PER_CELL,
            size_of::<DiagnosticWetCategory>(),
        ])?,
        closure_source_copy_bytes,
        p3_quadrature_scratch_bytes,
        PER_WORKER_ALLOCATION_GUARD_BYTES,
    ])?;
    let worker_chunk_and_closure_scratch_bytes =
        checked_product(&[parallel_workers.max(1), per_worker_scratch])?;
    let source_scene_retained_bytes = source.memory_estimate().retained_bytes();
    let estimated_peak_bytes = checked_sum([
        source_scene_retained_bytes,
        component_buffer_bytes,
        dense_lookup_bytes,
        sparse_index_bytes,
        frozen_filter_summary_bytes,
        scene_metadata_bytes,
        worker_chunk_and_closure_scratch_bytes,
    ])?;
    Ok(WrfTMatrixBuildPeakEstimate {
        source_scene_retained_bytes,
        active_source_cells,
        radar_elevation_nodes: elevation_nodes,
        parallel_workers: parallel_workers.max(1),
        component_buffer_bytes,
        dense_lookup_bytes,
        sparse_index_bytes,
        frozen_filter_summary_bytes,
        scene_metadata_bytes,
        worker_chunk_and_closure_scratch_bytes,
        estimated_peak_bytes,
    })
}

fn checked_product(values: &[usize]) -> Result<usize, WrfTMatrixSceneBuildError> {
    values.iter().try_fold(1_usize, |product, value| {
        product
            .checked_mul(*value)
            .ok_or(WrfTMatrixSceneBuildError::SizeOverflow)
    })
}

fn checked_sum(
    values: impl IntoIterator<Item = usize>,
) -> Result<usize, WrfTMatrixSceneBuildError> {
    values.into_iter().try_fold(0_usize, |sum, value| {
        sum.checked_add(value)
            .ok_or(WrfTMatrixSceneBuildError::SizeOverflow)
    })
}

fn should_diagnose_wet_coexistence(
    rain_mode: WrfTMatrixRainMode,
    has_frozen: bool,
    has_rain: bool,
    temperature_k: f64,
) -> bool {
    // At the exact cold boundary DiagnosticCoexistenceV1 pairs zero liquid.
    // That is dry frozen plus full standalone rain, not a wet-table query.
    rain_mode == WrfTMatrixRainMode::FullProperty
        && has_frozen
        && has_rain
        && temperature_k > DIAGNOSTIC_COEXISTENCE_COLD_K
        && temperature_k <= DIAGNOSTIC_COEXISTENCE_WARM_K
}

fn verify_fall_moment_policy(
    actual: FallMomentPolicy,
    expected: FallMomentPolicy,
    contribution: WrfTMatrixContribution,
) -> Result<(), WrfTMatrixSceneBuildError> {
    if actual == expected {
        Ok(())
    } else {
        Err(WrfTMatrixSceneBuildError::FallMomentPolicy {
            contribution,
            expected,
            actual,
        })
    }
}

fn evaluation_request(
    frequency_hz: f64,
    elevation_deg: f64,
    spheroid: SpheroidConvention,
) -> Result<TMatrixEvaluationRequest, WrfTMatrixSceneBuildError> {
    let view = RadarViewGeometry::new(elevation_deg)
        .map_err(WrfTMatrixSceneBuildError::EvaluationRequest)?;
    TMatrixEvaluationRequest::new(frequency_hz, spheroid, view)
        .map_err(WrfTMatrixSceneBuildError::EvaluationRequest)
}

fn spheroid_for_characteristic_category(
    category: WrfPropertyCategory,
) -> Result<SpheroidConvention, WrfTMatrixSceneBuildError> {
    match category {
        WrfPropertyCategory::P3(_) => Ok(SpheroidConvention::OblateMinorVertical),
        WrfPropertyCategory::IshmaelPlanar
        | WrfPropertyCategory::IshmaelColumnar
        | WrfPropertyCategory::IshmaelAggregate => {
            Err(WrfTMatrixSceneBuildError::IshmaelCharacteristicForbidden)
        }
    }
}

fn spheroid_for_particle(
    particle: &ClosedParticleCategory,
) -> Result<SpheroidConvention, WrfTMatrixSceneBuildError> {
    match particle.record().state() {
        ParticleState::P3(_) => Ok(SpheroidConvention::OblateMinorVertical),
        ParticleState::Ishmael(_) => Err(WrfTMatrixSceneBuildError::IshmaelCharacteristicForbidden),
        ParticleState::Conventional(_) => Err(WrfTMatrixSceneBuildError::WetSourceNotFrozen),
    }
}

fn compact_components(additive: AdditiveScattering) -> Result<[f32; 9], CompactScatteringError> {
    let values = additive.components();
    let mut compact = [0.0_f32; 9];
    for (index, value) in values.into_iter().enumerate() {
        if value < -(f32::MAX as f64) || value > f32::MAX as f64 {
            return Err(CompactScatteringError::OutsideF32 { index, value });
        }
        let converted = value as f32;
        if !converted.is_finite() {
            return Err(CompactScatteringError::OutsideF32 { index, value });
        }
        compact[index] = converted;
    }
    // Independent nearest-f32 rounding can put an otherwise-valid covariance
    // just outside |C| <= sqrt(ZH*ZV). Keep nearest-f32 ZH/ZV and project along
    // the complex covariance direction before rounding both components inward.
    // `additive` was already validated, so this is directed quantization of a
    // valid source tuple, never repair of invalid evaluator output.
    // Covariance is validated before fall moments, so repair it first in case
    // it masks an independently rounded fall-moment boundary.
    if matches!(
        decode_components(&compact),
        Err(WrfTMatrixSceneQueryError::Output(
            OutputError::CovarianceBound { .. }
        ))
    ) {
        project_compact_covariance(&mut compact);
    }

    // The same independent rounding can put a valid fall-moment triple just
    // outside S*ZH >= F^2. Keep nearest-f32 ZH and F, then move S to the
    // smallest representable f32 on the valid side of that boundary.
    if matches!(
        decode_components(&compact),
        Err(WrfTMatrixSceneQueryError::Output(
            OutputError::NegativeFallSpeedVariance { .. }
        ))
    ) {
        project_compact_second_fall_moment(&mut compact)?;
    }

    // Quantization must still satisfy every covariance and fall-moment
    // invariant. Genuinely invalid compact tuples remain errors.
    decode_components(&compact).map_err(|error| match error {
        WrfTMatrixSceneQueryError::Output(source) => CompactScatteringError::RoundTrip(source),
        _ => unreachable!("decoding a fixed nine-component array cannot be a storage error"),
    })?;
    Ok(compact)
}

fn project_compact_covariance(compact: &mut [f32; AdditiveScattering::COMPONENT_COUNT]) {
    let zh = f64::from(compact[0]);
    let zv = f64::from(compact[1]);
    let re = f64::from(compact[2]);
    let im = f64::from(compact[3]);
    let maximum = zh.sqrt() * zv.sqrt();
    let magnitude = re.hypot(im);
    debug_assert!(magnitude > maximum);
    if maximum == 0.0 {
        compact[2] = 0.0;
        compact[3] = 0.0;
        return;
    }

    let scale = maximum / magnitude;
    compact[2] = round_f32_toward_zero(re * scale);
    compact[3] = round_f32_toward_zero(im * scale);
}

fn round_f32_toward_zero(value: f64) -> f32 {
    let nearest = value as f32;
    if f64::from(nearest).abs() > value.abs() {
        f32::from_bits(nearest.to_bits() - 1)
    } else {
        nearest
    }
}

fn project_compact_second_fall_moment(
    compact: &mut [f32; AdditiveScattering::COMPONENT_COUNT],
) -> Result<(), CompactScatteringError> {
    let zh = f64::from(compact[0]);
    let first = f64::from(compact[7]);
    debug_assert!(zh > 0.0);
    let minimum_second = first * first / zh;
    let mut second = minimum_second as f32;
    if f64::from(second) < minimum_second {
        second = f32::from_bits(second.to_bits() + 1);
    }
    if !second.is_finite() {
        return Err(CompactScatteringError::OutsideF32 {
            index: 8,
            value: minimum_second,
        });
    }
    compact[8] = second;
    Ok(())
}

fn decode_components(values: &[f32]) -> Result<AdditiveScattering, WrfTMatrixSceneQueryError> {
    if values.len() != AdditiveScattering::COMPONENT_COUNT {
        return Err(WrfTMatrixSceneQueryError::CorruptStorage);
    }
    let mut decoded = [0.0; AdditiveScattering::COMPONENT_COUNT];
    for (target, source) in decoded.iter_mut().zip(values) {
        *target = f64::from(*source);
    }
    AdditiveScattering::from_components(decoded).map_err(WrfTMatrixSceneQueryError::Output)
}

fn validate_bundle(
    tables: WrfTMatrixLutBundle<'_>,
) -> Result<ValidatedBundle<'_>, WrfTMatrixBundleError> {
    let entries = tables.entries();
    let legacy_id_matches = entries
        .iter()
        .filter(|(role, table)| table.descriptor().table_id() == role.legacy_embedded_s_id())
        .count();
    if legacy_id_matches != 0 && legacy_id_matches != entries.len() {
        return Err(WrfTMatrixBundleError::MixedLegacyTableIds {
            matching_roles: legacy_id_matches,
        });
    }
    let unique_ids = entries
        .iter()
        .map(|(_, table)| table.descriptor().table_id())
        .collect::<BTreeSet<_>>();
    if unique_ids.len() != entries.len() {
        return Err(WrfTMatrixBundleError::DuplicateTableIds);
    }
    let table_id_text_bytes = entries
        .iter()
        .map(|(_, table)| table.descriptor().table_id().len())
        .sum();

    for (role, table) in tables.entries() {
        let expected_category = if role == WrfTMatrixTableRole::RainStandaloneAndResidual {
            TMatrixParticleCategory::Conventional(ConventionalHydrometeor::Rain)
        } else {
            TMatrixParticleCategory::PropertyAwareFrozenCharacteristicParticle
        };
        let expected_population = match role {
            WrfTMatrixTableRole::DryOblate | WrfTMatrixTableRole::DryProlate => {
                TMatrixPopulationRole::PropertyAwareDryCharacteristicParticle
            }
            WrfTMatrixTableRole::WetOblate | WrfTMatrixTableRole::WetProlate => {
                TMatrixPopulationRole::PropertyAwareWetCharacteristicParticle
            }
            WrfTMatrixTableRole::RainStandaloneAndResidual => {
                TMatrixPopulationRole::ConventionalRainStandaloneAndResidual
            }
        };
        let expected_spheroid = match role {
            WrfTMatrixTableRole::DryProlate | WrfTMatrixTableRole::WetProlate => {
                SpheroidConvention::ProlateMajorVertical
            }
            WrfTMatrixTableRole::DryOblate
            | WrfTMatrixTableRole::WetOblate
            | WrfTMatrixTableRole::RainStandaloneAndResidual => {
                SpheroidConvention::OblateMinorVertical
            }
        };
        let descriptor = table.descriptor();
        if descriptor.category() != expected_category {
            return Err(WrfTMatrixBundleError::Category {
                role,
                expected: expected_category,
                actual: descriptor.category(),
            });
        }
        if descriptor.population_role() != expected_population {
            return Err(WrfTMatrixBundleError::PopulationRole {
                role,
                expected: expected_population,
                actual: descriptor.population_role(),
            });
        }
        if descriptor.spheroid() != expected_spheroid {
            return Err(WrfTMatrixBundleError::Spheroid {
                role,
                expected: expected_spheroid,
                actual: descriptor.spheroid(),
            });
        }
        let actual_kinds = table
            .offline_lut()
            .header()
            .axes()
            .iter()
            .map(|axis| axis.kind())
            .collect::<Vec<_>>();
        let expected_kinds = match role {
            WrfTMatrixTableRole::DryOblate | WrfTMatrixTableRole::DryProlate => DRY_AXIS_KINDS,
            WrfTMatrixTableRole::WetOblate | WrfTMatrixTableRole::WetProlate => WET_AXIS_KINDS,
            WrfTMatrixTableRole::RainStandaloneAndResidual => RAIN_AXIS_KINDS,
        };
        if actual_kinds != expected_kinds {
            return Err(WrfTMatrixBundleError::AxisLayout {
                role,
                expected: expected_kinds.to_vec(),
                actual: actual_kinds,
            });
        }
        for axis in table.offline_lut().header().axes() {
            if axis.kind() != AxisKind::Frequency {
                validate_exact_axis_coordinates(role, axis.kind(), axis.coordinates())?;
            }
        }
        validate_exact_solver(
            role,
            descriptor.radar().solver_ddelt,
            descriptor.radar().solver_ndgs,
        )?;
        if !matches!(
            descriptor.odf(),
            TMatrixOdfConvention::GaussianCanting {
                mean_deg: 0.0,
                standard_deviation_deg: 20.0,
                alpha_quadrature_points: 5,
                beta_quadrature_points: 10,
            }
        ) {
            return Err(WrfTMatrixBundleError::Orientation { role });
        }
    }

    let mut dry_ice_material_density_kg_m3 = None;
    for role in [
        WrfTMatrixTableRole::DryOblate,
        WrfTMatrixTableRole::DryProlate,
    ] {
        let table = table_for_role(tables, role);
        let TMatrixMaterial::SymmetricBruggemanSphericalAirIceMatzler2006V1 {
            ice_material_density_kg_m3,
            ..
        } = table.descriptor().material()
        else {
            return Err(WrfTMatrixBundleError::DryMaterial { role });
        };
        if let Some(reference) = dry_ice_material_density_kg_m3
            && f64::to_bits(reference) != ice_material_density_kg_m3.to_bits()
        {
            return Err(WrfTMatrixBundleError::DryIceMaterialDensityMismatch {
                reference_kg_m3: reference,
                actual_kg_m3: *ice_material_density_kg_m3,
                role,
            });
        }
        dry_ice_material_density_kg_m3 = Some(*ice_material_density_kg_m3);
    }
    let dry_ice_material_density_kg_m3 =
        dry_ice_material_density_kg_m3.expect("both dry table roles were inspected");
    if dry_ice_material_density_kg_m3.to_bits() != ICE_MATERIAL_DENSITY_KG_M3.to_bits() {
        return Err(WrfTMatrixBundleError::DryIceMaterialEndpoint {
            expected_kg_m3: ICE_MATERIAL_DENSITY_KG_M3,
            actual_kg_m3: dry_ice_material_density_kg_m3,
        });
    }
    for role in [
        WrfTMatrixTableRole::WetOblate,
        WrfTMatrixTableRole::WetProlate,
    ] {
        let table = table_for_role(tables, role);
        if !matches!(
            table.descriptor().material(),
            TMatrixMaterial::SymmetricBruggemanSphericalAirIceWaterV1 { .. }
        ) {
            return Err(WrfTMatrixBundleError::WetMaterial { role });
        }
    }
    if !matches!(
        tables.rain_standalone_and_residual.descriptor().material(),
        TMatrixMaterial::TemperatureDependentLiquidWaterLiebe1991 { .. }
    ) {
        return Err(WrfTMatrixBundleError::RainMaterial);
    }

    let reference_radar = tables.dry_oblate.descriptor().radar();
    if reference_radar.view_applicability
        != RadarViewApplicability::PpiElevationAxisMinus05To20AxisymmetricGaussian
    {
        return Err(WrfTMatrixBundleError::RadarApplicability);
    }
    for (role, table) in tables.entries().into_iter().skip(1) {
        if table.descriptor().radar() != reference_radar {
            return Err(WrfTMatrixBundleError::RadarConventionMismatch { role });
        }
    }

    let reference_frequency = axis_coordinates(
        tables.dry_oblate,
        WrfTMatrixTableRole::DryOblate,
        AxisKind::Frequency,
    )?;
    if reference_frequency.len() != 1 {
        return Err(WrfTMatrixBundleError::FrequencyAxisNotSingleton {
            role: WrfTMatrixTableRole::DryOblate,
            actual: reference_frequency.to_vec(),
        });
    }
    let frequency_hz = reference_frequency[0];
    if !PROPERTY_TMATRIX_SUPPORTED_FREQUENCIES_HZ
        .iter()
        .any(|supported| supported.to_bits() == frequency_hz.to_bits())
    {
        return Err(WrfTMatrixBundleError::UnsupportedExactFrequency {
            actual_hz: frequency_hz,
        });
    }
    if legacy_id_matches == entries.len()
        && frequency_hz.to_bits() != PROPERTY_TMATRIX_FREQUENCY_HZ.to_bits()
    {
        return Err(WrfTMatrixBundleError::LegacyTableFrequency {
            expected_hz: PROPERTY_TMATRIX_FREQUENCY_HZ,
            actual_hz: frequency_hz,
        });
    }
    let reference_elevations = axis_coordinates(
        tables.dry_oblate,
        WrfTMatrixTableRole::DryOblate,
        AxisKind::RadarElevation,
    )?;
    if reference_elevations.first().copied() != Some(PROPERTY_TMATRIX_MIN_ELEVATION_DEG)
        || reference_elevations.last().copied() != Some(PROPERTY_TMATRIX_MAX_ELEVATION_DEG)
    {
        return Err(WrfTMatrixBundleError::ElevationRange {
            role: WrfTMatrixTableRole::DryOblate,
            actual: reference_elevations.to_vec(),
        });
    }
    for (role, table) in tables.entries().into_iter().skip(1) {
        let frequency = axis_coordinates(table, role, AxisKind::Frequency)?;
        if frequency.len() != 1 {
            return Err(WrfTMatrixBundleError::FrequencyAxisNotSingleton {
                role,
                actual: frequency.to_vec(),
            });
        }
        if frequency[0].to_bits() != frequency_hz.to_bits() {
            return Err(WrfTMatrixBundleError::SharedFrequencyAxis {
                role,
                expected_hz: frequency_hz,
                actual: frequency.to_vec(),
            });
        }
        let elevations = axis_coordinates(table, role, AxisKind::RadarElevation)?;
        if elevations.first() != reference_elevations.first()
            || elevations.last() != reference_elevations.last()
        {
            return Err(WrfTMatrixBundleError::ElevationRange {
                role,
                actual: elevations.to_vec(),
            });
        }
    }

    let oblate_domain = tables
        .dry_oblate
        .dry_particle_node_domain()
        .map_err(|source| WrfTMatrixBundleError::ParticleNodeBinding {
            role: WrfTMatrixTableRole::DryOblate,
            source,
        })?;
    let prolate_domain = tables
        .dry_prolate
        .dry_particle_node_domain()
        .map_err(|source| WrfTMatrixBundleError::ParticleNodeBinding {
            role: WrfTMatrixTableRole::DryProlate,
            source,
        })?;
    for (role, domain) in [
        (WrfTMatrixTableRole::DryOblate, oblate_domain),
        (WrfTMatrixTableRole::DryProlate, prolate_domain),
    ] {
        let support_endpoint = domain.bulk_density_range_kg_m3()[1];
        if support_endpoint.to_bits() != dry_ice_material_density_kg_m3.to_bits() {
            return Err(WrfTMatrixBundleError::DryIceMaterialSupportEndpoint {
                role,
                material_kg_m3: dry_ice_material_density_kg_m3,
                support_kg_m3: support_endpoint,
            });
        }
    }
    let oblate_fall_speed = tables
        .dry_oblate
        .dry_particle_node_fall_speed_provenance()
        .map_err(|source| WrfTMatrixBundleError::ParticleNodeBinding {
            role: WrfTMatrixTableRole::DryOblate,
            source,
        })?;
    let prolate_fall_speed = tables
        .dry_prolate
        .dry_particle_node_fall_speed_provenance()
        .map_err(|source| WrfTMatrixBundleError::ParticleNodeBinding {
            role: WrfTMatrixTableRole::DryProlate,
            source,
        })?;
    if oblate_fall_speed != prolate_fall_speed {
        return Err(WrfTMatrixBundleError::DryFallSpeedMismatch {
            oblate: oblate_fall_speed,
            prolate: prolate_fall_speed,
        });
    }

    Ok(ValidatedBundle {
        frequency_hz,
        radar_elevations_deg: reference_elevations,
        table_id_text_bytes,
        p3_particle_support: PsdParticleSupport::new(
            Some(oblate_domain),
            Some(prolate_domain),
            Some(oblate_domain),
        ),
        p3_fall_speed: oblate_fall_speed,
        ishmael_psd_config: production_ishmael_psd_config(),
        ishmael_particle_support: PsdParticleSupport::new(
            Some(oblate_domain),
            Some(prolate_domain),
            Some(oblate_domain),
        ),
        ishmael_fall_speed: oblate_fall_speed,
        ishmael_ice_material_density_kg_m3: dry_ice_material_density_kg_m3,
    })
}

fn table_for_role(
    tables: WrfTMatrixLutBundle<'_>,
    role: WrfTMatrixTableRole,
) -> &ResearchTMatrixLut {
    match role {
        WrfTMatrixTableRole::DryOblate => tables.dry_oblate,
        WrfTMatrixTableRole::DryProlate => tables.dry_prolate,
        WrfTMatrixTableRole::WetOblate => tables.wet_oblate,
        WrfTMatrixTableRole::WetProlate => tables.wet_prolate,
        WrfTMatrixTableRole::RainStandaloneAndResidual => tables.rain_standalone_and_residual,
    }
}

fn validate_exact_axis_coordinates(
    role: WrfTMatrixTableRole,
    kind: AxisKind,
    actual: &[f64],
) -> Result<(), WrfTMatrixBundleError> {
    let expected = exact_axis_coordinates(role, kind)
        .ok_or(WrfTMatrixBundleError::MissingCoordinateContract { role, kind })?;
    if actual == expected {
        Ok(())
    } else {
        Err(WrfTMatrixBundleError::AxisCoordinates {
            role,
            kind,
            expected: expected.to_vec(),
            actual: actual.to_vec(),
        })
    }
}

fn exact_axis_coordinates(role: WrfTMatrixTableRole, kind: AxisKind) -> Option<&'static [f64]> {
    match (role, kind) {
        (WrfTMatrixTableRole::DryOblate, AxisKind::EquivolumeDiameter) => {
            Some(DRY_OBLATE_DIAMETER_M)
        }
        (WrfTMatrixTableRole::DryProlate, AxisKind::EquivolumeDiameter) => {
            Some(DRY_PROLATE_DIAMETER_M)
        }
        (WrfTMatrixTableRole::WetOblate, AxisKind::EquivolumeDiameter) => {
            Some(WET_OBLATE_DIAMETER_M)
        }
        (WrfTMatrixTableRole::WetProlate, AxisKind::EquivolumeDiameter) => {
            Some(WET_PROLATE_DIAMETER_M)
        }
        (
            WrfTMatrixTableRole::DryOblate | WrfTMatrixTableRole::DryProlate,
            AxisKind::Temperature,
        ) => Some(DRY_TEMPERATURE_K),
        (WrfTMatrixTableRole::DryOblate, AxisKind::BulkDensity) => {
            Some(DRY_OBLATE_BULK_DENSITY_KG_M3)
        }
        (WrfTMatrixTableRole::DryProlate, AxisKind::BulkDensity) => {
            Some(DRY_PROLATE_BULK_DENSITY_KG_M3)
        }
        (WrfTMatrixTableRole::DryOblate, AxisKind::MinorToMajorAxisRatio) => {
            Some(DRY_OBLATE_MINOR_TO_MAJOR)
        }
        (
            WrfTMatrixTableRole::DryProlate
            | WrfTMatrixTableRole::WetOblate
            | WrfTMatrixTableRole::WetProlate,
            AxisKind::MinorToMajorAxisRatio,
        ) => Some(FROZEN_COARSE_MINOR_TO_MAJOR),
        (
            WrfTMatrixTableRole::WetOblate | WrfTMatrixTableRole::WetProlate,
            AxisKind::Temperature,
        ) => Some(WET_TEMPERATURE_K),
        (
            WrfTMatrixTableRole::WetOblate | WrfTMatrixTableRole::WetProlate,
            AxisKind::CondensedVolumeFraction,
        ) => Some(WET_CONDENSED_VOLUME_FRACTION),
        (
            WrfTMatrixTableRole::WetOblate | WrfTMatrixTableRole::WetProlate,
            AxisKind::LiquidMassFraction,
        ) => Some(WET_LIQUID_MASS_FRACTION),
        (WrfTMatrixTableRole::RainStandaloneAndResidual, AxisKind::EquivolumeDiameter) => {
            Some(RAIN_DIAMETER_M)
        }
        (WrfTMatrixTableRole::RainStandaloneAndResidual, AxisKind::Temperature) => {
            Some(RAIN_TEMPERATURE_K)
        }
        (WrfTMatrixTableRole::RainStandaloneAndResidual, AxisKind::MinorToMajorAxisRatio) => {
            Some(RAIN_MINOR_TO_MAJOR)
        }
        (WrfTMatrixTableRole::RainStandaloneAndResidual, AxisKind::RadarElevation) => {
            Some(RAIN_RADAR_ELEVATION_DEG)
        }
        (_, AxisKind::RadarElevation) => Some(FROZEN_RADAR_ELEVATION_DEG),
        _ => None,
    }
}

fn validate_exact_solver(
    role: WrfTMatrixTableRole,
    actual_ddelt: f64,
    actual_ndgs: u32,
) -> Result<(), WrfTMatrixBundleError> {
    if actual_ddelt != PROPERTY_SOLVER_DDELT {
        return Err(WrfTMatrixBundleError::RadarSolverDdelt {
            role,
            expected: PROPERTY_SOLVER_DDELT,
            actual: actual_ddelt,
        });
    }
    let expected_ndgs = exact_solver_ndgs(role);
    if actual_ndgs != expected_ndgs {
        return Err(WrfTMatrixBundleError::RadarSolverNdgs {
            role,
            expected: expected_ndgs,
            actual: actual_ndgs,
        });
    }
    Ok(())
}

const fn exact_solver_ndgs(role: WrfTMatrixTableRole) -> u32 {
    match role {
        WrfTMatrixTableRole::DryOblate => DRY_OBLATE_SOLVER_NDGS,
        WrfTMatrixTableRole::DryProlate => DRY_PROLATE_SOLVER_NDGS,
        WrfTMatrixTableRole::WetOblate => WET_OBLATE_SOLVER_NDGS,
        WrfTMatrixTableRole::WetProlate => WET_PROLATE_SOLVER_NDGS,
        WrfTMatrixTableRole::RainStandaloneAndResidual => RAIN_SOLVER_NDGS,
    }
}

fn axis_coordinates(
    table: &ResearchTMatrixLut,
    role: WrfTMatrixTableRole,
    kind: AxisKind,
) -> Result<&[f64], WrfTMatrixBundleError> {
    table
        .offline_lut()
        .header()
        .axes()
        .iter()
        .find(|axis| axis.kind() == kind)
        .map(|axis| axis.coordinates())
        .ok_or(WrfTMatrixBundleError::MissingAxis { role, kind })
}

fn validate_sparse_indices(source: &WrfPropertyScene) -> Result<(), WrfTMatrixSceneBuildError> {
    for (position, &index) in source.active_cell_indices().iter().enumerate() {
        if usize::try_from(index).expect("u32 always fits usize") >= source.cell_count()
            || position > 0 && source.active_cell_indices()[position - 1] >= index
        {
            return Err(WrfTMatrixSceneBuildError::InvalidSourceIndex {
                position,
                index,
                cell_count: source.cell_count(),
            });
        }
    }
    Ok(())
}

fn usize_to_u64(value: usize) -> Result<u64, WrfTMatrixSceneBuildError> {
    u64::try_from(value).map_err(|_| WrfTMatrixSceneBuildError::AuditCountOverflow)
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ElevationBracket {
    lower: usize,
    upper: usize,
    fraction: f64,
}

fn elevation_bracket(
    elevations: &[f64],
    elevation_deg: f64,
) -> Result<ElevationBracket, WrfTMatrixSceneQueryError> {
    if !elevation_deg.is_finite() {
        return Err(WrfTMatrixSceneQueryError::NonFiniteElevation { elevation_deg });
    }
    let first = *elevations
        .first()
        .ok_or(WrfTMatrixSceneQueryError::CorruptStorage)?;
    let last = *elevations
        .last()
        .ok_or(WrfTMatrixSceneQueryError::CorruptStorage)?;
    if elevation_deg < first || elevation_deg > last {
        return Err(WrfTMatrixSceneQueryError::ElevationOutsideAxis {
            elevation_deg,
            minimum_deg: first,
            maximum_deg: last,
        });
    }
    let upper = elevations.partition_point(|candidate| *candidate < elevation_deg);
    if upper < elevations.len() && elevations[upper] == elevation_deg {
        return Ok(ElevationBracket {
            lower: upper,
            upper,
            fraction: 0.0,
        });
    }
    if upper == 0 || upper == elevations.len() {
        return Err(WrfTMatrixSceneQueryError::CorruptStorage);
    }
    let lower = upper - 1;
    let fraction = (elevation_deg - elevations[lower]) / (elevations[upper] - elevations[lower]);
    Ok(ElevationBracket {
        lower,
        upper,
        fraction,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WrfTMatrixContribution {
    DryFrozen,
    P3SchemeNativePsd,
    IshmaelSchemeNativePsd,
    WetFrozen,
    ResidualRain,
    StandaloneRain,
}

impl std::fmt::Display for WrfTMatrixContribution {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::DryFrozen => "dry frozen",
            Self::P3SchemeNativePsd => "P3 scheme-native PSD",
            Self::IshmaelSchemeNativePsd => "ISHMAEL scheme-native PSD",
            Self::WetFrozen => "wet frozen",
            Self::ResidualRain => "residual rain",
            Self::StandaloneRain => "standalone rain",
        })
    }
}

#[derive(Debug, Error)]
pub enum WrfTMatrixRawEvaluationError {
    #[error("synthetic-radar T-matrix evaluation cancelled")]
    Cancelled,
    #[error("raw property T-matrix evaluation does not support WRF mp_physics={scheme_id}")]
    UnsupportedScheme { scheme_id: i32 },
    #[error(
        "WRF mp_physics={scheme_id} requires official P3 table {expected_file}; load/cache the hash-qualified external asset from {source_url} and construct the evaluator with new_with_p3"
    )]
    P3TableRequired {
        scheme_id: i32,
        expected_file: &'static str,
        source_url: &'static str,
    },
    #[error(
        "WRF mp_physics={scheme_id} requires official P3 table {expected_file}, but {actual:?} is loaded"
    )]
    WrongP3Table {
        scheme_id: i32,
        expected_file: &'static str,
        actual: P3OfficialTableKind,
    },
    #[error("blended P3 state contains non-P3 category {0}")]
    P3CategoryLayout(WrfPropertyCategory),
    #[error("blended triple-moment P3 state is missing QZI for {0}")]
    MissingP3Qzi(WrfPropertyCategory),
    #[error("reconstruct blended exact native P3 PSD for {category}: {source}")]
    P3PsdReconstruction {
        category: WrfPropertyCategory,
        #[source]
        source: P3PsdError,
    },
    #[error("integrate blended native P3 PSD for {category}: {source}")]
    P3PsdIntegration {
        category: WrfPropertyCategory,
        #[source]
        source: P3TMatrixIntegrationError<EvaluationError>,
    },
    #[error("close blended raw property state: {0}")]
    Closure(#[source] RawPropertyClosureError),
    #[error("full raw property T-matrix evaluation requires rain state: {0}")]
    RainUnavailable(RainUnavailableReason),
    #[error("diagnose blended raw homogeneous mixed-phase coexistence: {0}")]
    Coexistence(#[source] CoexistenceUnavailable),
    #[error("diagnosed raw coexistence lost its closed rain source")]
    MissingRainSource,
    #[error("blended ISHMAEL state contains non-ISHMAEL category {0}")]
    IshmaelCategoryLayout(WrfPropertyCategory),
    #[error("reconstruct blended native ISHMAEL PSD for {category}: {source}")]
    IshmaelPsdReconstruction {
        category: WrfPropertyCategory,
        #[source]
        source: PsdError,
    },
    #[error("integrate blended native ISHMAEL PSD for {category}: {source}")]
    IshmaelPsdIntegration {
        category: WrfPropertyCategory,
        #[source]
        source: PsdIntegrationError<EvaluationError>,
    },
    #[error("construct raw T-matrix evaluation request: {0}")]
    EvaluationRequest(#[source] EvaluationError),
    #[error("evaluate raw {contribution}: {source}")]
    Evaluation {
        contribution: WrfTMatrixContribution,
        #[source]
        source: EvaluationError,
    },
    #[error("accumulate raw {contribution}: {source}")]
    Accumulation {
        contribution: WrfTMatrixContribution,
        #[source]
        source: OutputError,
    },
    #[error("convert raw integrated additive scattering: {0}")]
    Output(#[source] OutputError),
    #[error(
        "runtime fall-moment policy for raw {contribution} must be {expected:?}, got {actual:?}"
    )]
    FallMomentPolicy {
        contribution: WrfTMatrixContribution,
        expected: FallMomentPolicy,
        actual: FallMomentPolicy,
    },
    #[error("raw diagnosed wet category unexpectedly has a conventional source")]
    WetSourceNotFrozen,
    #[error("ISHMAEL characteristic-particle raw evaluation is forbidden; native PSD is required")]
    IshmaelCharacteristicForbidden,
}

/// Stable-position failure from a cut-wide raw-property batch.
#[derive(Debug, Error)]
pub enum WrfTMatrixRawBatchError {
    #[error("synthetic-radar T-matrix raw batch cancelled")]
    Cancelled,
    #[error("raw property batch request {request_index}: {source}")]
    Request {
        request_index: usize,
        #[source]
        source: WrfTMatrixRawEvaluationError,
    },
}

#[derive(Debug, Error)]
pub enum WrfTMatrixSceneBuildError {
    #[error("synthetic-radar T-matrix scene build cancelled")]
    Cancelled,
    #[error(transparent)]
    Bundle(#[from] WrfTMatrixBundleError),
    #[error(
        "property scattering grid has {cell_count} cells; u32 dense-row lookup cannot represent it"
    )]
    GridTooLarge { cell_count: usize },
    #[error(
        "property source index {index} at position {position} is unsorted, duplicated, or outside {cell_count} cells"
    )]
    InvalidSourceIndex {
        position: usize,
        index: u32,
        cell_count: usize,
    },
    #[error(
        "WRF mp_physics={scheme_id} requires official P3 table {expected_file}; load/cache the hash-qualified external asset from {source_url} and call build_with_p3"
    )]
    P3TableRequired {
        scheme_id: i32,
        expected_file: &'static str,
        source_url: &'static str,
    },
    #[error(
        "WRF mp_physics={scheme_id} requires official P3 table {expected_file}, but {actual:?} is loaded"
    )]
    WrongP3Table {
        scheme_id: i32,
        expected_file: &'static str,
        actual: P3OfficialTableKind,
    },
    #[error("close property source cell {cell_index}: {source}")]
    SourceCell {
        cell_index: usize,
        #[source]
        source: WrfPropertyReadError,
    },
    #[error("read raw ISHMAEL source cell {cell_index}: {source}")]
    RawSourceCell {
        cell_index: usize,
        #[source]
        source: WrfPropertyReadError,
    },
    #[error("P3 source cell {cell_index} contains non-P3 category {category}")]
    P3CategoryLayout {
        cell_index: usize,
        category: WrfPropertyCategory,
    },
    #[error("triple-moment P3 source cell {cell_index} is missing QZI for {category}")]
    MissingP3Qzi {
        cell_index: usize,
        category: WrfPropertyCategory,
    },
    #[error("reconstruct exact native P3 PSD for {category} at cell {cell_index}: {source}")]
    P3PsdReconstruction {
        cell_index: usize,
        category: WrfPropertyCategory,
        #[source]
        source: P3PsdError,
    },
    #[error(
        "integrate native P3 PSD for {category} at cell {cell_index}, elevation {elevation_deg} degrees: {source}"
    )]
    P3PsdIntegration {
        cell_index: usize,
        elevation_deg: f64,
        category: WrfPropertyCategory,
        #[source]
        source: P3TMatrixIntegrationError<EvaluationError>,
    },
    #[error("ISHMAEL source cell {cell_index} contains non-ISHMAEL category {category}")]
    IshmaelCategoryLayout {
        cell_index: usize,
        category: WrfPropertyCategory,
    },
    #[error("reconstruct native ISHMAEL PSD for {category} at cell {cell_index}: {source}")]
    IshmaelPsdReconstruction {
        cell_index: usize,
        category: WrfPropertyCategory,
        #[source]
        source: PsdError,
    },
    #[error(
        "integrate native ISHMAEL PSD for {category} at cell {cell_index}, elevation {elevation_deg} degrees: {source}"
    )]
    IshmaelPsdIntegration {
        cell_index: usize,
        elevation_deg: f64,
        category: WrfPropertyCategory,
        #[source]
        source: PsdIntegrationError<EvaluationError>,
    },
    #[error(
        "hybrid bulk Rayleigh input {field} for {category} at cell {cell_index} must remain finite and positive when narrowed to f32, got {value}"
    )]
    HybridBulkRayleighMoment {
        cell_index: usize,
        category: WrfPropertyCategory,
        field: &'static str,
        value: f64,
    },
    #[error(
        "hybrid bulk Rayleigh produced clear scattering for positive {category} at cell {cell_index}"
    )]
    HybridBulkRayleighClear {
        cell_index: usize,
        category: WrfPropertyCategory,
    },
    #[error(
        "form or accumulate hybrid bulk Rayleigh additive output for {category} at cell {cell_index}: {source}"
    )]
    HybridBulkRayleighOutput {
        cell_index: usize,
        category: WrfPropertyCategory,
        #[source]
        source: OutputError,
    },
    #[error("close rain-only state at ISHMAEL cell {cell_index}: {source}")]
    RawRainClosure {
        cell_index: usize,
        #[source]
        source: RawPropertyClosureError,
    },
    #[error("property source marks cell {cell_index} active but closes it as clear")]
    UnexpectedClearActiveCell { cell_index: usize },
    #[error("full property scattering requires rain state at cell {cell_index}: {reason}")]
    RainUnavailable {
        cell_index: usize,
        reason: RainUnavailableReason,
    },
    #[error("diagnose homogeneous mixed-phase coexistence at cell {cell_index}: {source}")]
    Coexistence {
        cell_index: usize,
        #[source]
        source: CoexistenceUnavailable,
    },
    #[error("diagnosed coexistence at cell {cell_index} lost its closed rain source")]
    CoexistenceMissingRainSource { cell_index: usize },
    #[error("construct exact T-matrix evaluation request: {0}")]
    EvaluationRequest(#[source] EvaluationError),
    #[error(
        "evaluate {contribution} at cell {cell_index}, elevation {elevation_deg} degrees: {source}"
    )]
    Evaluation {
        cell_index: usize,
        elevation_deg: f64,
        contribution: WrfTMatrixContribution,
        #[source]
        source: EvaluationError,
    },
    #[error(
        "accumulate {contribution} at cell {cell_index}, elevation {elevation_deg} degrees: {source}"
    )]
    Accumulation {
        cell_index: usize,
        elevation_deg: f64,
        contribution: WrfTMatrixContribution,
        #[source]
        source: OutputError,
    },
    #[error("runtime fall-moment policy for {contribution} must be {expected:?}, got {actual:?}")]
    FallMomentPolicy {
        contribution: WrfTMatrixContribution,
        expected: FallMomentPolicy,
        actual: FallMomentPolicy,
    },
    #[error("compact scattering at cell {cell_index}, elevation {elevation_deg} degrees: {source}")]
    Compact {
        cell_index: usize,
        elevation_deg: f64,
        #[source]
        source: CompactScatteringError,
    },
    #[error("diagnosed wet category unexpectedly has a conventional source")]
    WetSourceNotFrozen,
    #[error("ISHMAEL characteristic-particle evaluation is forbidden; native PSD is required")]
    IshmaelCharacteristicForbidden,
    #[error("scene storage size overflow")]
    SizeOverflow,
    #[error("scene audit-count overflow")]
    AuditCountOverflow,
    #[error("internal compact row for cell {cell_index} has {actual} values, expected {expected}")]
    InternalRowLength {
        cell_index: usize,
        expected: usize,
        actual: usize,
    },
    #[error("internal component plane has {actual} values, expected {expected}")]
    InternalComponentPlaneLength { expected: usize, actual: usize },
    #[error("internal FrozenOnly summary has {actual} rows, expected {expected}")]
    InternalSummaryLength { expected: usize, actual: usize },
}

#[derive(Debug, Error)]
pub enum WrfTMatrixBundleError {
    #[error("bind {role} for scheme-native particle nodes: {source}")]
    ParticleNodeBinding {
        role: WrfTMatrixTableRole,
        #[source]
        source: EvaluationError,
    },
    #[error(
        "dry oblate/prolate particle tables use different terminal-speed laws: oblate={oblate:?}, prolate={prolate:?}"
    )]
    DryFallSpeedMismatch {
        oblate: PsdFallSpeedProvenance,
        prolate: PsdFallSpeedProvenance,
    },
    #[error(
        "bundle mixes {matching_roles} legacy embedded S-band table IDs with external IDs; all five IDs must come from one source family"
    )]
    MixedLegacyTableIds { matching_roles: usize },
    #[error("the five typed table roles must have distinct table IDs")]
    DuplicateTableIds,
    #[error("{role} table category must be {expected:?}, got {actual:?}")]
    Category {
        role: WrfTMatrixTableRole,
        expected: TMatrixParticleCategory,
        actual: TMatrixParticleCategory,
    },
    #[error("{role} population role must be {expected:?}, got {actual:?}")]
    PopulationRole {
        role: WrfTMatrixTableRole,
        expected: TMatrixPopulationRole,
        actual: TMatrixPopulationRole,
    },
    #[error("{role} spheroid convention must be {expected:?}, got {actual:?}")]
    Spheroid {
        role: WrfTMatrixTableRole,
        expected: SpheroidConvention,
        actual: SpheroidConvention,
    },
    #[error("{role} table axis layout must be {expected:?}, got {actual:?}")]
    AxisLayout {
        role: WrfTMatrixTableRole,
        expected: Vec<AxisKind>,
        actual: Vec<AxisKind>,
    },
    #[error("{role} table has no exact coordinate contract for axis {kind:?}")]
    MissingCoordinateContract {
        role: WrfTMatrixTableRole,
        kind: AxisKind,
    },
    #[error("{role} {kind:?} coordinates must be exactly {expected:?}, got {actual:?}")]
    AxisCoordinates {
        role: WrfTMatrixTableRole,
        kind: AxisKind,
        expected: Vec<f64>,
        actual: Vec<f64>,
    },
    #[error("{role} table is missing required axis {kind:?}")]
    MissingAxis {
        role: WrfTMatrixTableRole,
        kind: AxisKind,
    },
    #[error("{role} table must use exact Gaussian20Research 5x10 ODF")]
    Orientation { role: WrfTMatrixTableRole },
    #[error("{role} table must use the property-aware dry air/ice Bruggeman material")]
    DryMaterial { role: WrfTMatrixTableRole },
    #[error(
        "{role} dry-table ice material density {actual_kg_m3} kg m^-3 differs from bundle reference {reference_kg_m3} kg m^-3"
    )]
    DryIceMaterialDensityMismatch {
        role: WrfTMatrixTableRole,
        reference_kg_m3: f64,
        actual_kg_m3: f64,
    },
    #[error(
        "dry-table authenticated solid-ice material endpoint must be exactly {expected_kg_m3} kg m^-3, got {actual_kg_m3} kg m^-3"
    )]
    DryIceMaterialEndpoint {
        expected_kg_m3: f64,
        actual_kg_m3: f64,
    },
    #[error(
        "{role} dry-table support density endpoint {support_kg_m3} kg m^-3 differs from authenticated material endpoint {material_kg_m3} kg m^-3"
    )]
    DryIceMaterialSupportEndpoint {
        role: WrfTMatrixTableRole,
        material_kg_m3: f64,
        support_kg_m3: f64,
    },
    #[error("{role} table must use the property-aware air/ice/water Bruggeman material")]
    WetMaterial { role: WrfTMatrixTableRole },
    #[error("rain table must use temperature-dependent Liebe-1991 liquid water")]
    RainMaterial,
    #[error("bundle must use the exact axisymmetric PPI elevation applicability")]
    RadarApplicability,
    #[error("{role} radar convention/applicability differs from the dry-oblate reference")]
    RadarConventionMismatch { role: WrfTMatrixTableRole },
    #[error("{role} radar solver ddelt must be exactly {expected}, got {actual}")]
    RadarSolverDdelt {
        role: WrfTMatrixTableRole,
        expected: f64,
        actual: f64,
    },
    #[error("{role} radar solver ndgs must be exactly {expected}, got {actual}")]
    RadarSolverNdgs {
        role: WrfTMatrixTableRole,
        expected: u32,
        actual: u32,
    },
    #[error("{role} frequency axis must contain exactly one coordinate, got {actual:?}")]
    FrequencyAxisNotSingleton {
        role: WrfTMatrixTableRole,
        actual: Vec<f64>,
    },
    #[error(
        "unsupported exact T-matrix frequency {actual_hz} Hz; allowed research bands are exactly 2.8, 5.6, or 9.4 GHz and interpolation is forbidden"
    )]
    UnsupportedExactFrequency { actual_hz: f64 },
    #[error(
        "legacy embedded S-band table IDs require exactly {expected_hz} Hz, got {actual_hz} Hz"
    )]
    LegacyTableFrequency { expected_hz: f64, actual_hz: f64 },
    #[error(
        "{role} frequency axis {actual:?} differs from exact shared bundle frequency {expected_hz} Hz"
    )]
    SharedFrequencyAxis {
        role: WrfTMatrixTableRole,
        expected_hz: f64,
        actual: Vec<f64>,
    },
    #[error("dry-oblate elevation axis must span exactly -0.5 through 20 degrees, got {actual:?}")]
    ElevationRange {
        role: WrfTMatrixTableRole,
        actual: Vec<f64>,
    },
    #[error("{role} elevation nodes differ from the exact dry-oblate nodes")]
    SharedElevationAxis { role: WrfTMatrixTableRole },
}

#[derive(Debug, Error)]
pub enum CompactScatteringError {
    #[error("additive component {index} value {value} is outside finite f32 storage")]
    OutsideF32 { index: usize, value: f64 },
    #[error("f32 quantization violates an additive invariant: {0}")]
    RoundTrip(#[source] OutputError),
}

#[derive(Debug, Error)]
pub enum WrfTMatrixSceneQueryError {
    #[error("weighted property query requires at least one contribution")]
    NoWeightedContributions,
    #[error("weighted property query has {actual} contributions; maximum is {maximum}")]
    TooManyWeightedContributions { actual: usize, maximum: usize },
    #[error(
        "weighted property contribution {contribution_index} has invalid nonnegative weight {weight}"
    )]
    InvalidWeight {
        contribution_index: usize,
        weight: f64,
    },
    #[error("weighted property contribution weights sum to {sum}, expected 1")]
    WeightSum { sum: f64 },
    #[error("property scattering cell {cell_index} is outside {cell_count} source cells")]
    CellOutOfRange {
        cell_index: usize,
        cell_count: usize,
    },
    #[error("beam elevation must be finite, got {elevation_deg}")]
    NonFiniteElevation { elevation_deg: f64 },
    #[error(
        "beam elevation {elevation_deg} is outside [{minimum_deg}, {maximum_deg}] degrees; no extrapolation or clamping is permitted"
    )]
    ElevationOutsideAxis {
        elevation_deg: f64,
        minimum_deg: f64,
        maximum_deg: f64,
    },
    #[error("property scattering compact storage is internally inconsistent")]
    CorruptStorage,
    #[error("decode/interpolate additive scattering: {0}")]
    Output(#[source] OutputError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wrf_property_reader::{RawRainState, read_wrf_property_scene};
    use crate::wrf_scene_inventory::WrfSourceIdentity;
    use crate::wrf_tmatrix_assets::{
        PropertyTMatrixTableSourceKind, load_property_tmatrix_tables_exact,
    };
    use crate::wrf_tmatrix_band_assets::S_BAND_RESEARCH_FREQUENCY_HZ;
    use radar_scattering::P3Category;
    use radar_scattering::P3ProjectedAreaConsistency;

    fn additive(values: [f64; 9]) -> AdditiveScattering {
        AdditiveScattering::from_components(values).unwrap()
    }

    fn with_embedded_ishmael_prepared(
        test: impl for<'table> FnOnce(&PreparedIshmaelParticleIntegration<'table>),
    ) {
        let owner = load_property_tmatrix_tables_exact(
            PropertyTMatrixTableSourceKind::LegacyEmbeddedSResearchV1,
            S_BAND_RESEARCH_FREQUENCY_HZ,
        )
        .expect("load authenticated embedded T-matrix tables");
        let tables = owner.borrowed_bundle();
        let validated = validate_bundle(tables).expect("validate embedded table bundle");
        let distribution = IshmaelPsd::reconstruct(IshmaelPsdInput::new(
            radar_scattering::IshmaelIceCategory::Planar,
            1.020_730_388_452_175_2e-3,
            100_000.0,
            6.250_000_000_000_000_5e-9,
            3.125_000_000_000_000_3e-9,
            1.2,
        ))
        .expect("reconstruct fixed ISHMAEL category");
        let oblate_request = evaluation_request(
            validated.frequency_hz,
            0.9,
            SpheroidConvention::OblateMinorVertical,
        )
        .unwrap();
        let prolate_request = evaluation_request(
            validated.frequency_hz,
            0.9,
            SpheroidConvention::ProlateMajorVertical,
        )
        .unwrap();
        let prepared = prepare_ishmael_particle_integration(
            &distribution,
            validated.ishmael_psd_config,
            validated.ishmael_particle_support,
            validated.ishmael_fall_speed,
            validated.ishmael_ice_material_density_kg_m3,
            tables,
            260.0,
            oblate_request,
            prolate_request,
        )
        .expect("prepare embedded-table ISHMAEL category");
        assert!(prepared.integration.node_count(PsdQuadratureLevel::Coarse) > 0);
        assert!(prepared.integration.node_count(PsdQuadratureLevel::Refined) > 0);
        test(&prepared);
    }

    fn with_reported_ishmael_planar_prepared(
        test: impl for<'table> FnOnce(&PreparedIshmaelParticleIntegration<'table>),
    ) {
        let owner = load_property_tmatrix_tables_exact(
            PropertyTMatrixTableSourceKind::LegacyEmbeddedSResearchV1,
            S_BAND_RESEARCH_FREQUENCY_HZ,
        )
        .expect("load authenticated embedded T-matrix tables");
        let tables = owner.borrowed_bundle();
        let validated = validate_bundle(tables).expect("validate embedded table bundle");
        let distribution = IshmaelPsd::reconstruct(IshmaelPsdInput::new(
            radar_scattering::IshmaelIceCategory::Planar,
            1.760_302_043_019_024_2e-10,
            3.285_777_347_628_027e-4,
            4.228_335_533_134_264e-16,
            4.228_335_533_134_264e-16,
            0.957_008_183_002_471_9,
        ))
        .expect("reconstruct the reported cell's native planar PSD");
        let oblate_request = evaluation_request(
            validated.frequency_hz,
            -0.5,
            SpheroidConvention::OblateMinorVertical,
        )
        .unwrap();
        let prolate_request = evaluation_request(
            validated.frequency_hz,
            -0.5,
            SpheroidConvention::ProlateMajorVertical,
        )
        .unwrap();
        let prepared = prepare_ishmael_particle_integration(
            &distribution,
            validated.ishmael_psd_config,
            validated.ishmael_particle_support,
            validated.ishmael_fall_speed,
            validated.ishmael_ice_material_density_kg_m3,
            tables,
            ICE_MELTING_TEMPERATURE_K,
            oblate_request,
            prolate_request,
        )
        .expect("prepare the reported ISHMAEL planar population");
        assert!(
            prepared
                .integration
                .node_count(PsdQuadratureLevel::AdaptiveRefined)
                > 0
        );
        test(&prepared);
    }

    fn with_exact_spherical_ishmael_bridge_prepared(
        test: impl for<'table> FnOnce(&PreparedIshmaelParticleIntegration<'table>),
    ) {
        let owner = load_property_tmatrix_tables_exact(
            PropertyTMatrixTableSourceKind::LegacyEmbeddedSResearchV1,
            S_BAND_RESEARCH_FREQUENCY_HZ,
        )
        .expect("load authenticated embedded T-matrix tables");
        let tables = owner.borrowed_bundle();
        let validated = validate_bundle(tables).expect("validate embedded table bundle");
        let distribution = IshmaelPsd::reconstruct_wrf_source_checked(
            IshmaelPsdInput::new(
                radar_scattering::IshmaelIceCategory::Columnar,
                f64::from(f32::from_bits(0x2ce8_e0d2)),
                f64::from(f32::from_bits(0x3d4a_a53e)),
                f64::from(f32::from_bits(0x23ff_3104)),
                f64::from(f32::from_bits(0x23ff_3104)),
                f64::from(f32::from_bits(0x3f3f_f540)),
            ),
            f64::from(f32::from_bits(0x4382_ce77)),
        )
        .expect("reconstruct exact fixture spherical-columnar source tuple");
        assert_eq!(distribution.a_scale_m().to_bits(), 0x3ee1_4732_a1d5_7f71);
        assert_eq!(
            distribution.c_at_a_scale_m().to_bits(),
            0x3ee1_4732_a1d5_7f71
        );
        assert_eq!(
            distribution.aspect_power_delta().to_bits(),
            1.0_f64.to_bits()
        );
        let oblate_request = evaluation_request(
            validated.frequency_hz,
            -0.5,
            SpheroidConvention::OblateMinorVertical,
        )
        .unwrap();
        let prolate_request = evaluation_request(
            validated.frequency_hz,
            -0.5,
            SpheroidConvention::ProlateMajorVertical,
        )
        .unwrap();
        let prepared = prepare_ishmael_particle_integration(
            &distribution,
            validated.ishmael_psd_config,
            validated.ishmael_particle_support,
            validated.ishmael_fall_speed,
            validated.ishmael_ice_material_density_kg_m3,
            tables,
            f64::from(f32::from_bits(0x4382_ce77)),
            oblate_request,
            prolate_request,
        )
        .expect("prepare exact fixture spherical-columnar bridge workload");
        test(&prepared);
    }

    fn prepare_captured_solid_ice_ishmael<'table>(
        tables: WrfTMatrixLutBundle<'table>,
        validated: ValidatedBundle<'table>,
        category: radar_scattering::IshmaelIceCategory,
        qice_kgkg: f64,
        qnice_per_kg: f64,
        qvoli_qaoli_m3_per_kg: f64,
        dry_air_density_kg_m3: f64,
    ) -> PreparedIshmaelParticleIntegration<'table> {
        let distribution = IshmaelPsd::reconstruct(IshmaelPsdInput::new(
            category,
            qice_kgkg,
            qnice_per_kg,
            qvoli_qaoli_m3_per_kg,
            qvoli_qaoli_m3_per_kg,
            dry_air_density_kg_m3,
        ))
        .expect("reconstruct captured dense spherical ISHMAEL population");
        assert_eq!(distribution.bulk_density_kg_m3(), 920.0);
        let oblate_request = evaluation_request(
            validated.frequency_hz,
            -0.5,
            SpheroidConvention::OblateMinorVertical,
        )
        .unwrap();
        let prolate_request = evaluation_request(
            validated.frequency_hz,
            -0.5,
            SpheroidConvention::ProlateMajorVertical,
        )
        .unwrap();
        prepare_ishmael_particle_integration(
            &distribution,
            validated.ishmael_psd_config,
            validated.ishmael_particle_support,
            validated.ishmael_fall_speed,
            validated.ishmael_ice_material_density_kg_m3,
            tables,
            ICE_MELTING_TEMPERATURE_K,
            oblate_request,
            prolate_request,
        )
        .expect("prepare captured dense spherical ISHMAEL population")
    }

    fn with_captured_solid_ice_ishmael_prepared(
        mut test: impl for<'table> FnMut(&'static str, &PreparedIshmaelParticleIntegration<'table>),
    ) {
        let owner = load_property_tmatrix_tables_exact(
            PropertyTMatrixTableSourceKind::LegacyEmbeddedSResearchV1,
            S_BAND_RESEARCH_FREQUENCY_HZ,
        )
        .expect("load authenticated embedded T-matrix tables");
        let tables = owner.borrowed_bundle();
        let validated = validate_bundle(tables).expect("validate embedded table bundle");
        let planar = prepare_captured_solid_ice_ishmael(
            tables,
            validated,
            radar_scattering::IshmaelIceCategory::Planar,
            1.752_276_403_976_793_5e-7,
            3.476_882_696_151_733_4,
            3.789_174_106_861_442_6e-13,
            1.018_401_503_562_927_2,
        );
        test("cell 3119612 planar", &planar);

        let aggregate = prepare_captured_solid_ice_ishmael(
            tables,
            validated,
            radar_scattering::IshmaelIceCategory::Aggregate,
            1.459_221_854_460_679_4e-9,
            0.015_616_921_707_987_785,
            3.155_463_908_625_943_4e-15,
            0.964_799_642_562_866_2,
        );
        test("cell 3790530 aggregate", &aggregate);
    }

    #[allow(clippy::too_many_arguments)]
    fn embedded_ishmael_raw_batch_case<'table>(
        tables: WrfTMatrixLutBundle<'table>,
        validated: ValidatedBundle<'table>,
        category: WrfPropertyCategory,
        ishmael_category: radar_scattering::IshmaelIceCategory,
        qvoli_m3_per_kg: f64,
        qaoli_m3_per_kg: f64,
        qice_candidates_kgkg: &[f64],
        elevation_deg: f64,
    ) -> (
        PreparedRawBatchEvaluation<'table>,
        Option<PolarAccumulatorQuantities>,
        usize,
        usize,
    ) {
        let oblate_request = evaluation_request(
            validated.frequency_hz,
            elevation_deg,
            SpheroidConvention::OblateMinorVertical,
        )
        .unwrap();
        let prolate_request = evaluation_request(
            validated.frequency_hz,
            elevation_deg,
            SpheroidConvention::ProlateMajorVertical,
        )
        .unwrap();
        let (integration, expected) = qice_candidates_kgkg
            .iter()
            .find_map(|&qice_kgkg| {
                let distribution = IshmaelPsd::reconstruct(IshmaelPsdInput::new(
                    ishmael_category,
                    qice_kgkg,
                    100_000.0,
                    qvoli_m3_per_kg,
                    qaoli_m3_per_kg,
                    1.2,
                ))
                .ok()?;
                let integration = prepare_ishmael_particle_integration(
                    &distribution,
                    validated.ishmael_psd_config,
                    validated.ishmael_particle_support,
                    validated.ishmael_fall_speed,
                    validated.ishmael_ice_material_density_kg_m3,
                    tables,
                    260.0,
                    oblate_request,
                    prolate_request,
                )
                .ok()?;
                let expected = integration
                    .finish_cpu()
                    .ok()?
                    .additive()
                    .to_polar_accumulator_quantities()
                    .map(Some)
                    .ok()?;
                Some((integration, expected))
            })
            .expect("construct a converged embedded-table ISHMAEL batch case");
        let oblate_nodes = integration
            .evaluations
            .iter()
            .filter(|evaluation| {
                evaluation.role == WrfTMatrixCudaTableRole::DryOblate
                    && evaluation.table_binding().is_some()
            })
            .count();
        let prolate_nodes = integration
            .evaluations
            .iter()
            .filter(|evaluation| {
                evaluation.role == WrfTMatrixCudaTableRole::DryProlate
                    && evaluation.table_binding().is_some()
            })
            .count();
        let cuda_outputs = (0..integration.evaluations.len()).map(|_| None).collect();
        (
            PreparedRawBatchEvaluation::Ishmael(PreparedIshmaelRawBatchEvaluation {
                categories: vec![PreparedIshmaelRawBatchCategory {
                    category,
                    integration,
                    cuda_outputs,
                }],
                rain: None,
                rain_request: oblate_request,
                rain_table: tables.rain_standalone_and_residual,
            }),
            expected,
            oblate_nodes,
            prolate_nodes,
        )
    }

    fn with_embedded_ishmael_raw_batch(
        test: impl for<'table> FnOnce(
            Vec<PreparedRawBatchEvaluation<'table>>,
            Vec<Option<PolarAccumulatorQuantities>>,
            usize,
            usize,
        ),
    ) {
        let owner = load_property_tmatrix_tables_exact(
            PropertyTMatrixTableSourceKind::LegacyEmbeddedSResearchV1,
            S_BAND_RESEARCH_FREQUENCY_HZ,
        )
        .expect("load authenticated embedded T-matrix tables");
        let tables = owner.borrowed_bundle();
        let validated = validate_bundle(tables).expect("validate embedded table bundle");
        let oblate = embedded_ishmael_raw_batch_case(
            tables,
            validated,
            WrfPropertyCategory::IshmaelPlanar,
            radar_scattering::IshmaelIceCategory::Planar,
            6.250_000_000_000_000_5e-9,
            3.125_000_000_000_000_3e-9,
            &[1.020_730_388_452_175_2e-3],
            0.9,
        );
        let prolate = embedded_ishmael_raw_batch_case(
            tables,
            validated,
            WrfPropertyCategory::IshmaelColumnar,
            radar_scattering::IshmaelIceCategory::Columnar,
            // Physical near-spherical prolate case: a_n=50 um,
            // c(a_n)=50.5 um. Density candidates retain the unchanged
            // production support and additive-convergence gates.
            1.262_5e-8,
            1.275_125e-8,
            &[2.54e-3, 1.27e-3, 3.81e-3, 5.08e-3, 6.35e-4],
            1.3,
        );
        let sphere = embedded_ishmael_raw_batch_case(
            tables,
            validated,
            WrfPropertyCategory::IshmaelAggregate,
            radar_scattering::IshmaelIceCategory::Aggregate,
            5.592_415_808_136_224e-11,
            5.592_415_808_136_224e-11,
            &[1.337_833_514_336_37e-5],
            0.5,
        );
        let PreparedRawBatchEvaluation::Ishmael(sphere_request) = &sphere.0 else {
            unreachable!("the exact-sphere raw-batch fixture is ISHMAEL")
        };
        assert!(
            sphere_request.categories[0]
                .integration
                .evaluations
                .iter()
                .any(|evaluation| evaluation.table_binding().is_none())
        );
        assert!(
            sphere_request.categories[0]
                .integration
                .evaluations
                .iter()
                .any(|evaluation| evaluation.table_binding().is_some())
        );
        let oblate_nodes = oblate.2.saturating_add(prolate.2).saturating_add(sphere.2);
        let prolate_nodes = oblate.3.saturating_add(prolate.3).saturating_add(sphere.3);
        test(
            vec![oblate.0, prolate.0, sphere.0],
            vec![oblate.1, prolate.1, sphere.1],
            oblate_nodes,
            prolate_nodes,
        );
    }

    fn assert_psd_result_bitwise_eq(actual: PsdIntegrationResult, expected: PsdIntegrationResult) {
        assert_eq!(
            actual.additive().components().map(f64::to_bits),
            expected.additive().components().map(f64::to_bits),
            "integrated additive components differ at the bit level"
        );
        assert_eq!(actual.accumulator(), expected.accumulator());
        assert_eq!(actual.audit(), expected.audit());
    }

    fn polar_accumulator_bits(value: PolarAccumulatorQuantities) -> [u32; 9] {
        [
            value.zh.to_bits(),
            value.zv.to_bits(),
            value.cov_re.to_bits(),
            value.cov_im.to_bits(),
            value.kdp_deg_km.to_bits(),
            value.ah_db_km.to_bits(),
            value.av_db_km.to_bits(),
            value.fall_speed_mps.to_bits(),
            value.fall_speed_variance_m2s2.to_bits(),
        ]
    }

    fn assert_raw_batch_bitwise_eq(
        actual: &[Option<PolarAccumulatorQuantities>],
        expected: &[Option<PolarAccumulatorQuantities>],
    ) {
        assert_eq!(actual.len(), expected.len());
        for (request_index, (actual, expected)) in actual.iter().zip(expected).enumerate() {
            match (actual, expected) {
                (Some(actual), Some(expected)) => assert_eq!(
                    polar_accumulator_bits(*actual),
                    polar_accumulator_bits(*expected),
                    "raw batch request {request_index} differs at the float-bit level"
                ),
                (None, None) => {}
                _ => panic!("raw batch request {request_index} changed clear/echo state"),
            }
        }
    }

    #[test]
    fn exact_spherical_ishmael_cell_uses_cpu_bridge_and_lut_primary_routes() {
        with_exact_spherical_ishmael_bridge_prepared(|prepared| {
            let mut bridge_nodes = 0_usize;
            let mut table_nodes = 0_usize;
            for (evaluation, (_, _, node)) in prepared
                .evaluations
                .iter()
                .zip(prepared.integration.nodes())
            {
                match node.scattering_route() {
                    IshmaelParticleScatteringRoute::TMatrixTable => {
                        table_nodes += 1;
                        assert!(evaluation.table_binding().is_some());
                    }
                    IshmaelParticleScatteringRoute::TableFloorAnchoredExactSphereRayleighV1 => {
                        bridge_nodes += 1;
                        assert!(evaluation.table_binding().is_none());
                        assert_eq!(evaluation.role, WrfTMatrixCudaTableRole::DryOblate);
                    }
                }
            }
            assert!(bridge_nodes > 0);
            assert!(table_nodes > 0);

            let cpu = prepared.finish_cpu().unwrap();
            let mut primary_table_calls = 0_usize;
            let primary = prepared
                .finish_with_table_evaluator(|_, _, _, _, table, interpolation| {
                    primary_table_calls += 1;
                    table.evaluate_prepared_dry_particle_lut_interpolation_per_m3(interpolation)
                })
                .unwrap();
            assert!(primary_table_calls > 0);
            assert!(primary_table_calls < primary.audit().total_nodes_reduced);
            assert_psd_result_bitwise_eq(primary, cpu);
            let audit = primary.audit();
            assert_eq!(
                audit.small_sphere_scattering_revision,
                Some(ISHMAEL_SMALL_SPHERE_RAYLEIGH_BRIDGE_REVISION)
            );
            assert!(audit.small_sphere_rayleigh_bridge_nodes > 0);
            assert!(
                audit.domain_omitted_number_fraction <= ISHMAEL_PSD_MAXIMUM_OMITTED_NUMBER_FRACTION
            );
            assert!(
                audit.domain_omitted_mass_fraction <= ISHMAEL_PSD_MAXIMUM_OMITTED_MASS_FRACTION
            );
            assert!(audit.domain_omitted_d6_fraction <= ISHMAEL_PSD_MAXIMUM_OMITTED_D6_FRACTION);
        });
    }

    #[test]
    fn ishmael_base_success_consumes_only_the_base_ordered_prefix() {
        with_embedded_ishmael_prepared(|prepared| {
            let base_node_count = prepared.integration.node_count(PsdQuadratureLevel::Coarse)
                + prepared.integration.node_count(PsdQuadratureLevel::Refined);
            assert!(base_node_count < prepared.evaluations.len());
            let expected = prepared
                .evaluations
                .iter()
                .take(base_node_count)
                .filter(|evaluation| evaluation.table_binding().is_some())
                .map(|evaluation| (evaluation.level, evaluation.node_index))
                .collect::<Vec<_>>();
            let mut visited = Vec::new();
            let result = prepared
                .finish_with_table_evaluator(|_, level, node_index, _, table, interpolation| {
                    visited.push((level, node_index));
                    table.evaluate_prepared_dry_particle_lut_interpolation_per_m3(interpolation)
                })
                .unwrap();
            assert_eq!(result.audit().refinement_steps, 0);
            assert_eq!(visited, expected);
            assert!(
                visited
                    .iter()
                    .all(|(level, _)| *level != PsdQuadratureLevel::AdaptiveRefined)
            );
        });
    }

    #[test]
    fn ishmael_adaptive_success_consumes_every_distinct_ordered_level() {
        with_reported_ishmael_planar_prepared(|prepared| {
            let expected = prepared
                .evaluations
                .iter()
                .filter(|evaluation| evaluation.table_binding().is_some())
                .map(|evaluation| (evaluation.level, evaluation.node_index))
                .collect::<Vec<_>>();
            let unique = expected.iter().copied().collect::<BTreeSet<_>>();
            assert_eq!(unique.len(), expected.len());
            assert!(
                expected
                    .iter()
                    .any(|(level, _)| { *level == PsdQuadratureLevel::AdaptiveRefined })
            );
            assert!(expected.iter().any(|&(level, index)| {
                level == PsdQuadratureLevel::Refined
                    && expected.contains(&(PsdQuadratureLevel::AdaptiveRefined, index))
            }));

            let mut visited = Vec::new();
            let result = prepared
                .finish_with_table_evaluator(|_, level, node_index, _, table, interpolation| {
                    visited.push((level, node_index));
                    table.evaluate_prepared_dry_particle_lut_interpolation_per_m3(interpolation)
                })
                .unwrap();
            assert_eq!(result.audit().refinement_steps, 1);
            assert_eq!(visited, expected);
        });
    }

    #[test]
    fn captured_ishmael_solid_ice_populations_use_mass_preserving_lut_geometry() {
        with_captured_solid_ice_ishmael_prepared(|label, prepared| {
            let material = prepared.integration.material_closure();
            assert!(material.applied(), "{label}");
            assert_eq!(material.source_bulk_density_kg_m3(), 920.0, "{label}");
            assert_eq!(
                material.scattering_bulk_density_kg_m3(),
                ICE_MATERIAL_DENSITY_KG_M3,
                "{label}"
            );
            assert!(
                material.mass_preserving_source_bulk_density_kg_m3() > 920.0,
                "{label}"
            );
            let expected_scale = (material.mass_preserving_source_bulk_density_kg_m3()
                / ICE_MATERIAL_DENSITY_KG_M3)
                .cbrt();
            assert_eq!(
                material.linear_dimension_scale().to_bits(),
                expected_scale.to_bits(),
                "{label}"
            );
            assert!(!prepared.evaluations.is_empty(), "{label}");
            let mut checked_nodes = 0_usize;
            for (evaluation, (level, node_index, source)) in prepared
                .evaluations
                .iter()
                .zip(prepared.integration.nodes())
            {
                checked_nodes += 1;
                assert_eq!(
                    (evaluation.level, evaluation.node_index),
                    (level, node_index)
                );
                let scattering = prepared.integration.scattering_particle(source);
                assert_eq!(evaluation.role, WrfTMatrixCudaTableRole::DryOblate);
                assert_eq!(source.bulk_density_kg_m3(), 920.0, "{label}");
                assert_eq!(
                    source.mass_preserving_bulk_density_kg_m3().to_bits(),
                    material
                        .mass_preserving_source_bulk_density_kg_m3()
                        .to_bits(),
                    "{label}"
                );
                assert_eq!(
                    scattering.bulk_density_kg_m3(),
                    ICE_MATERIAL_DENSITY_KG_M3,
                    "{label}"
                );
                assert_eq!(scattering.habit(), PsdSpheroidHabit::Spherical);
                assert_eq!(
                    scattering.equivolume_diameter_m().to_bits(),
                    (source.equivolume_diameter_m() * expected_scale).to_bits(),
                    "{label}"
                );
                let mapped_mass = ICE_MATERIAL_DENSITY_KG_M3
                    * (4.0 / 3.0)
                    * PI
                    * scattering.a_semi_axis_m().powi(2)
                    * scattering.c_semi_axis_m();
                let relative_mass_error =
                    (mapped_mass - source.particle_mass_kg()).abs() / source.particle_mass_kg();
                assert!(
                    relative_mass_error <= 6.0e-15,
                    "{label}: {relative_mass_error:e}"
                );
            }
            assert_eq!(checked_nodes, prepared.evaluations.len(), "{label}");

            let result = prepared.finish_cpu().unwrap();
            assert_eq!(result.audit().material_closure, material, "{label}");
            assert_eq!(result.audit().refinement_steps, 0, "{label}");
            let expected_component_bits = match label {
                "cell 3119612 planar" => [
                    4_584_814_127_194_777_157,
                    4_584_814_127_194_777_201,
                    4_584_814_127_194_777_179,
                    13_496_351_942_250_142_541,
                    4_319_305_828_281_550_870,
                    4_486_107_865_789_088_444,
                    4_486_107_865_789_088_443,
                    4_592_725_846_421_563_132,
                    4_601_202_897_700_065_836,
                ],
                "cell 3790530 aggregate" => [
                    4_557_348_784_881_753_620,
                    4_557_348_784_881_753_705,
                    4_557_348_784_881_753_662,
                    13_474_860_641_124_642_613,
                    4_286_781_360_730_856_214,
                    4_454_737_032_576_173_191,
                    4_454_737_032_576_173_190,
                    4_566_661_766_687_248_884,
                    4_576_176_074_442_968_050,
                ],
                _ => panic!("unexpected captured population label {label}"),
            };
            assert_eq!(
                result.additive().components().map(f64::to_bits),
                expected_component_bits,
                "{label}"
            );
            assert_eq!(
                result.audit().small_sphere_scattering_revision,
                Some(ISHMAEL_SMALL_SPHERE_RAYLEIGH_BRIDGE_REVISION),
                "{label}"
            );
            assert!(
                result.audit().small_sphere_rayleigh_bridge_nodes > 0,
                "{label}"
            );
            assert!(
                result.audit().domain_omitted_number_fraction
                    <= ISHMAEL_PSD_MAXIMUM_OMITTED_NUMBER_FRACTION,
                "{label}"
            );
            assert!(
                result.audit().domain_omitted_mass_fraction
                    <= ISHMAEL_PSD_MAXIMUM_OMITTED_MASS_FRACTION,
                "{label}"
            );
            assert!(
                result.audit().domain_omitted_d6_fraction
                    <= ISHMAEL_PSD_MAXIMUM_OMITTED_D6_FRACTION,
                "{label}"
            );
        });
    }

    #[cfg(any(windows, target_os = "linux"))]
    fn assert_ishmael_prepared_gpu_parity(
        prepared: &PreparedIshmaelParticleIntegration<'_>,
        service: &WrfTMatrixCudaBatchService,
        expected_refinement_steps: u8,
    ) {
        let cpu_nodes = prepared
            .evaluations
            .iter()
            .map(|evaluation| evaluation.evaluate_cpu().unwrap())
            .collect::<Vec<_>>();
        let mut gpu_nodes = vec![None; prepared.evaluations.len()];
        for role in [
            WrfTMatrixCudaTableRole::DryOblate,
            WrfTMatrixCudaTableRole::DryProlate,
        ] {
            let positions = prepared
                .evaluations
                .iter()
                .enumerate()
                .filter_map(|(position, evaluation)| {
                    (evaluation.role == role && evaluation.table_binding().is_some())
                        .then_some(position)
                })
                .collect::<Vec<_>>();
            let staged = positions
                .iter()
                .map(|&position| {
                    let evaluation = &prepared.evaluations[position];
                    let (_, admitted) = evaluation
                        .table_binding()
                        .expect("only LUT-backed nodes are staged");
                    CudaPreparedTMatrixNode::new(admitted, 1.0).unwrap()
                })
                .collect::<Vec<_>>();
            if staged.is_empty() {
                continue;
            }
            let evaluated = service.evaluate_particles(role, staged).unwrap();
            for (position, output) in positions.into_iter().zip(evaluated) {
                assert_eq!(
                    output.components().map(f64::to_bits),
                    cpu_nodes[position].components().map(f64::to_bits),
                    "ISHMAEL particle {position} differs between CUDA and scalar CPU"
                );
                gpu_nodes[position] = Some(output);
            }
        }
        assert!(
            prepared
                .evaluations
                .iter()
                .zip(&gpu_nodes)
                .all(
                    |(evaluation, output)| evaluation.table_binding().is_some() == output.is_some()
                )
        );

        let cpu_category = prepared.finish_cpu().unwrap();
        assert_eq!(
            cpu_category.audit().refinement_steps,
            expected_refinement_steps
        );
        let mut visited = Vec::new();
        let gpu_category = prepared
            .finish_with_table_evaluator(|position, level, node_index, _, _, _| {
                visited.push((position, level, node_index));
                Ok(gpu_nodes[position].expect("every LUT-backed prepared node has a GPU result"))
            })
            .unwrap();
        let consumed_evaluations = if expected_refinement_steps == 0 {
            prepared.integration.node_count(PsdQuadratureLevel::Coarse)
                + prepared.integration.node_count(PsdQuadratureLevel::Refined)
        } else {
            prepared.evaluations.len()
        };
        let expected = prepared.evaluations[..consumed_evaluations]
            .iter()
            .enumerate()
            .filter(|(_, evaluation)| evaluation.table_binding().is_some())
            .map(|(position, evaluation)| (position, evaluation.level, evaluation.node_index))
            .collect::<Vec<_>>();
        assert_eq!(visited, expected);
        assert_psd_result_bitwise_eq(gpu_category, cpu_category);
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn embedded_ishmael_prepared_nodes_are_bitwise_equal_on_gpu_and_cpu() {
        let availability = simradar_cuda::probe_cuda_cached();
        let Some(device) = availability.preferred_device() else {
            eprintln!("skipping ISHMAEL CUDA parity test: {availability:?}");
            return;
        };
        let service = WrfTMatrixCudaBatchService::open_with_config(
            device.ordinal,
            crate::wrf_tmatrix_cuda::WrfTMatrixCudaBatchConfig::new(512, 8).unwrap(),
        )
        .expect("open CUDA service for ISHMAEL parity");

        with_embedded_ishmael_prepared(|prepared| {
            assert_ishmael_prepared_gpu_parity(prepared, &service, 0);
        });
        with_reported_ishmael_planar_prepared(|prepared| {
            assert_ishmael_prepared_gpu_parity(prepared, &service, 1);
        });
        with_captured_solid_ice_ishmael_prepared(|_, prepared| {
            assert_ishmael_prepared_gpu_parity(prepared, &service, 0);
        });

        let report = service.report();
        assert!(report.nodes_completed > 0);
        assert_eq!(report.nodes_completed, report.nodes_submitted);
        assert_eq!(report.batches_completed, report.batches_submitted);
        assert_eq!(report.fallback_reason, None);
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn embedded_ishmael_raw_batch_is_bitwise_equal_on_gpu_and_cpu() {
        let availability = simradar_cuda::probe_cuda_cached();
        let Some(device) = availability.preferred_device() else {
            eprintln!("skipping ordered raw-batch CUDA parity test: {availability:?}");
            return;
        };
        let service = WrfTMatrixCudaBatchService::open_with_config(
            device.ordinal,
            crate::wrf_tmatrix_cuda::WrfTMatrixCudaBatchConfig::new(512, 8).unwrap(),
        )
        .expect("open CUDA service for ordered raw-batch parity");

        with_embedded_ishmael_raw_batch(|prepared, expected, oblate_nodes, prolate_nodes| {
            assert!(
                oblate_nodes > 0,
                "fixture must exercise the oblate role sweep"
            );
            assert!(
                prolate_nodes > 0,
                "fixture must exercise the prolate role sweep"
            );
            assert_eq!(
                raw_batch_role_node_counts(&prepared),
                (oblate_nodes, prolate_nodes)
            );
            let actual = finish_raw_batch(prepared, Some(&service), None).unwrap();
            assert_raw_batch_bitwise_eq(&actual, &expected);
        });

        let report = service.report();
        assert_eq!(report.requests_submitted, 2);
        assert_eq!(report.nodes_completed, report.nodes_submitted);
        assert_eq!(report.batches_completed, report.batches_submitted);
        assert_eq!(report.fallback_reason, None);
    }

    #[test]
    fn failed_second_role_discards_first_role_and_replays_whole_raw_batch() {
        let service = WrfTMatrixCudaBatchService::fail_after_successful_calls_for_test(
            1,
            "injected failure after one successful ordered role sweep",
        );
        let mut expected_oblate_nodes = 0_usize;
        let mut expected_prolate_nodes = 0_usize;
        with_embedded_ishmael_raw_batch(|prepared, expected, oblate_nodes, prolate_nodes| {
            assert!(
                oblate_nodes > 0,
                "fixture must exercise the oblate role sweep"
            );
            assert!(
                prolate_nodes > 0,
                "fixture must exercise the prolate role sweep"
            );
            expected_oblate_nodes = oblate_nodes;
            expected_prolate_nodes = prolate_nodes;
            let replayed = finish_raw_batch(prepared, Some(&service), None).unwrap();
            assert_raw_batch_bitwise_eq(&replayed, &expected);
        });

        let report = service.report();
        assert_eq!(report.requests_submitted, 2);
        assert_eq!(report.batches_submitted, 2);
        assert_eq!(report.batches_completed, 1);
        assert_eq!(report.nodes_completed, expected_oblate_nodes as u64);
        let reason = report
            .fallback_reason
            .expect("second role failure permanently disables this job service");
        assert_eq!(reason.failed_batch_sequence, Some(2));
        assert_eq!(reason.discarded_node_count, expected_prolate_nodes);
        assert_eq!(
            reason.detail,
            "injected failure after one successful ordered role sweep"
        );
    }

    #[test]
    fn raw_batch_preparation_errors_are_selected_by_input_position() {
        let owner = load_property_tmatrix_tables_exact(
            PropertyTMatrixTableSourceKind::LegacyEmbeddedSResearchV1,
            S_BAND_RESEARCH_FREQUENCY_HZ,
        )
        .expect("load authenticated embedded T-matrix tables");
        let evaluator = WrfTMatrixRawEvaluator::new(owner.borrowed_bundle()).unwrap();
        let first = RawPropertyCell::unsupported_for_batch_test(54);
        let second = RawPropertyCell::unsupported_for_batch_test(56);
        let requests = [
            WrfTMatrixRawBatchRequest::new(&first, 0.5),
            WrfTMatrixRawBatchRequest::new(&second, 0.5),
        ];

        for _ in 0..16 {
            let error = evaluator
                .evaluate_batch_with_cuda_and_cancel(&requests, None, None)
                .expect_err("unsupported raw schemes must fail");
            assert!(matches!(
                error,
                WrfTMatrixRawBatchError::Request {
                    request_index: 0,
                    source: WrfTMatrixRawEvaluationError::UnsupportedScheme { scheme_id: 54 },
                }
            ));
        }
    }

    #[test]
    fn cancelled_raw_batch_does_not_submit_disable_or_replay() {
        let service = WrfTMatrixCudaBatchService::failing_for_test(
            "a pre-cancelled raw batch must never reach this backend",
        );
        let cancel = AtomicBool::new(true);
        let error = finish_raw_batch(Vec::new(), Some(&service), Some(&cancel))
            .expect_err("pre-cancelled raw batch stops before CUDA or CPU evaluation");
        assert!(matches!(error, WrfTMatrixRawBatchError::Cancelled));
        let report = service.report();
        assert_eq!(report.requests_submitted, 0);
        assert_eq!(report.batches_submitted, 0);
        assert_eq!(report.nodes_submitted, 0);
        assert_eq!(report.fallback_reason, None);
    }

    #[test]
    fn failed_and_disabled_cuda_replay_the_whole_ishmael_category_on_cpu() {
        let service = WrfTMatrixCudaBatchService::failing_for_test(
            "injected deterministic ISHMAEL CUDA launch failure",
        );
        with_embedded_ishmael_prepared(|prepared| {
            let expected = prepared.finish_cpu().unwrap();
            let replayed =
                finish_ishmael_particle_integration(prepared, Some(&service), None).unwrap();
            assert_psd_result_bitwise_eq(replayed, expected);

            let failed_report = service.report();
            let reason = failed_report
                .fallback_reason
                .as_ref()
                .expect("failed batch permanently disables this job service");
            assert_eq!(reason.failed_batch_sequence, Some(1));
            assert_eq!(
                reason.discarded_node_count,
                prepared.evaluations.len(),
                "the failed category batch must be discarded in full"
            );
            assert_eq!(
                reason.detail,
                "injected deterministic ISHMAEL CUDA launch failure"
            );
            assert_eq!(failed_report.batches_submitted, 1);
            assert_eq!(failed_report.batches_completed, 0);
            assert_eq!(failed_report.nodes_completed, 0);

            let replayed_while_disabled =
                finish_ishmael_particle_integration(prepared, Some(&service), None).unwrap();
            assert_psd_result_bitwise_eq(replayed_while_disabled, expected);
            let disabled_report = service.report();
            assert_eq!(
                disabled_report.requests_submitted,
                failed_report.requests_submitted
            );
            assert_eq!(
                disabled_report.batches_submitted,
                failed_report.batches_submitted
            );
            assert_eq!(
                disabled_report.nodes_submitted,
                failed_report.nodes_submitted
            );
            assert_eq!(
                disabled_report.fallback_reason,
                failed_report.fallback_reason
            );
        });
    }

    #[test]
    fn cancelled_cuda_category_does_not_submit_disable_or_cpu_replay() {
        let service = WrfTMatrixCudaBatchService::failing_for_test(
            "a cancelled category must never reach this backend",
        );
        let cancel = AtomicBool::new(true);
        with_embedded_ishmael_prepared(|prepared| {
            let error =
                finish_ishmael_particle_integration(prepared, Some(&service), Some(&cancel))
                    .expect_err("pre-cancelled category stops before CUDA or CPU evaluation");
            assert!(matches!(error, ParticleFinishError::Cancelled));
        });
        let report = service.report();
        assert_eq!(report.requests_submitted, 0);
        assert_eq!(report.batches_submitted, 0);
        assert_eq!(report.nodes_submitted, 0);
        assert_eq!(report.fallback_reason, None);
    }

    #[test]
    fn cancellable_cpu_finish_preserves_exact_ishmael_reduction() {
        let cancel = AtomicBool::new(false);
        with_embedded_ishmael_prepared(|prepared| {
            let expected = prepared.finish_cpu().unwrap();
            let actual =
                finish_ishmael_particle_integration(prepared, None, Some(&cancel)).unwrap();
            assert_psd_result_bitwise_eq(actual, expected);
        });
    }

    #[test]
    fn floor_anchored_rayleigh_sphere_has_exact_floor_and_sphere_limits() {
        let anchor = [64.0, 64.0, 64.0, 0.0, 0.0, 1.0e-6, 1.0e-6, 0.0, 0.0];
        let at_floor =
            floor_anchored_rayleigh_sphere(anchor, 1.0, 2.0, PROPERTY_TMATRIX_FREQUENCY_HZ, 0.93)
                .unwrap();
        let floor_components = at_floor.components();
        assert_eq!(floor_components[0], 64.0);
        assert_eq!(floor_components[1], 64.0);
        assert_eq!(floor_components[2], 64.0);
        assert_eq!(floor_components[3], 0.0);
        assert_eq!(floor_components[4], 0.0);
        assert!((floor_components[5] - 1.0e-6).abs() <= 1.0e-18);
        assert!((floor_components[6] - 1.0e-6).abs() <= 1.0e-18);
        assert_eq!(floor_components[7], 128.0);
        assert_eq!(floor_components[8], 256.0);

        let half_diameter =
            floor_anchored_rayleigh_sphere(anchor, 0.5, 2.0, PROPERTY_TMATRIX_FREQUENCY_HZ, 0.93)
                .unwrap();
        let half_components = half_diameter.components();
        assert_eq!(half_components[0], 1.0);
        assert_eq!(half_components[1], 1.0);
        assert_eq!(half_components[2], 1.0);
        assert_eq!(half_components[3], 0.0);
        assert_eq!(half_components[4], 0.0);
        assert!(half_components[5] > 0.0 && half_components[5] < 1.0e-6);
        assert_eq!(half_components[5], half_components[6]);
        assert_eq!(half_components[7], 2.0);
        assert_eq!(half_components[8], 4.0);
    }

    #[test]
    fn compact_quantization_projects_reported_fall_moment_roundoff() {
        // This valid f64 tuple is the source interval that independently
        // rounds to the exact failing compact tuple reported from the field.
        let source = additive([
            29.762_806_8,
            29.762_806_8,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
            138.382_546,
            643.411_45,
        ]);
        let nearest = source.components().map(|value| value as f32);
        assert_eq!(nearest[0], 29.762_805_938_720_703_f32);
        assert_eq!(nearest[7], 138.382_553_100_585_94_f32);
        assert_eq!(nearest[8], 643.411_437_988_281_3_f32);
        assert!(matches!(
            decode_components(&nearest),
            Err(WrfTMatrixSceneQueryError::Output(
                OutputError::NegativeFallSpeedVariance { .. }
            ))
        ));

        let compact = compact_components(source).expect("directed compact quantization");
        assert_eq!(compact[0], nearest[0]);
        assert_eq!(compact[7], nearest[7]);
        assert_eq!(compact[8], f32::from_bits(nearest[8].to_bits() + 1));
        decode_components(&compact).expect("projected tuple remains physically valid");
    }

    #[test]
    fn compact_quantization_projects_reported_covariance_roundoff() {
        // Exact valid f64 tuple captured from the real mp_physics=50 d02
        // all-active-cell build. Nearest-f32 rounding puts covariance real on
        // the upper ZH lattice point while ZV lands one ULP below it.
        let source = additive([
            129.068_162_268_768_28,
            129.068_149_944_855_15,
            129.068_155_884_295_3,
            4.147_305_158_180_616e-8,
            1.328_771_757_781_215e-5,
            3.618_100_524_704_311_6e-6,
            3.590_836_876_778_43e-6,
            2_810.176_756_199_691,
            61_731.010_819_663_155,
        ]);
        let nearest = source.components().map(|value| value as f32);
        assert_eq!(nearest[0].to_bits(), 0x4301_1173);
        assert_eq!(nearest[1].to_bits(), 0x4301_1172);
        assert_eq!(nearest[2].to_bits(), 0x4301_1173);
        assert_eq!(nearest[3].to_bits(), 0x3332_201a);
        assert!(matches!(
            decode_components(&nearest),
            Err(WrfTMatrixSceneQueryError::Output(
                OutputError::CovarianceBound { .. }
            ))
        ));

        let compact = compact_components(source).expect("directed compact quantization");
        assert_eq!(compact[0], nearest[0]);
        assert_eq!(compact[1], nearest[1]);
        assert_eq!(compact[2].to_bits(), 0x4301_1172);
        assert_eq!(compact[3].to_bits(), 0x3332_2019);
        assert_eq!(compact[4..], nearest[4..]);
        decode_components(&compact).expect("projected tuple remains physically valid");
    }

    fn synthetic_scene() -> WrfTMatrixScene {
        let low = compact_components(additive([
            100.0, 80.0, 70.0, -5.0, -1.0, 0.1, 0.05, 500.0, 2_700.0,
        ]))
        .unwrap();
        let high = compact_components(additive([
            300.0, 240.0, 210.0, 15.0, 3.0, 0.3, 0.15, 1_800.0, 11_100.0,
        ]))
        .unwrap();
        let mut components = Vec::new();
        components.extend(low);
        components.extend(high);
        WrfTMatrixScene {
            source_identity: PropertySceneIdentity {
                source_identity: WrfSourceIdentity("fixture".to_owned()),
                time_index: 2,
            },
            microphysics_scheme_id: 50,
            required_field_signature: RequiredFieldSignature {
                microphysics_scheme_id: 50,
                fields: std::collections::BTreeSet::new(),
            },
            source_fields: Vec::new(),
            source_cell_count: 5,
            source_cell_indices: vec![3],
            full_cell_to_compact_row: vec![u32::MAX, u32::MAX, u32::MAX, 0, u32::MAX],
            radar_elevations_deg: vec![-0.5, 20.0],
            additive_components: components,
            provenance: WrfTMatrixSceneProvenance {
                status: "research_only_unvalidated",
                frequency_hz: PROPERTY_TMATRIX_FREQUENCY_HZ,
                orientation: OrientationDefinition::Gaussian20Research,
                frozen_scattering:
                    WrfTMatrixFrozenScatteringAudit::P3CharacteristicParticleV1,
                fall_moment_policy: WrfTMatrixFallMomentAudit {
                    closed_category:
                        FallMomentPolicy::ClosedCategoryPositiveDownZeroWithinCategoryVariance,
                    diagnostic_wet_category:
                        FallMomentPolicy::DiagnosticWetCategoryPositiveDownZeroWithinCategoryVariance,
                },
                rain_mode: WrfTMatrixRainMode::FullProperty,
                scattering_policy: WrfTMatrixScatteringPolicy::StrictFailClosed,
                tables: [
                    WrfTMatrixTableAudit {
                        role: WrfTMatrixTableRole::DryOblate,
                        table_id: DRY_OBLATE_TABLE_ID.to_owned(),
                        file_sha256: Sha256Digest::compute(b"a"),
                    },
                    WrfTMatrixTableAudit {
                        role: WrfTMatrixTableRole::DryProlate,
                        table_id: DRY_PROLATE_TABLE_ID.to_owned(),
                        file_sha256: Sha256Digest::compute(b"b"),
                    },
                    WrfTMatrixTableAudit {
                        role: WrfTMatrixTableRole::WetOblate,
                        table_id: WET_OBLATE_TABLE_ID.to_owned(),
                        file_sha256: Sha256Digest::compute(b"c"),
                    },
                    WrfTMatrixTableAudit {
                        role: WrfTMatrixTableRole::WetProlate,
                        table_id: WET_PROLATE_TABLE_ID.to_owned(),
                        file_sha256: Sha256Digest::compute(b"d"),
                    },
                    WrfTMatrixTableAudit {
                        role: WrfTMatrixTableRole::RainStandaloneAndResidual,
                        table_id: RAIN_TABLE_ID.to_owned(),
                        file_sha256: Sha256Digest::compute(b"e"),
                    },
                ],
                counts: WrfTMatrixAuditCounts {
                    source_cells: 1,
                    dry_frozen_populations: 1,
                    ..WrfTMatrixAuditCounts::default()
                },
            },
        }
    }

    #[test]
    fn characteristic_dispatch_is_p3_only() {
        assert_eq!(
            spheroid_for_characteristic_category(WrfPropertyCategory::P3(
                radar_scattering::P3Category::Category1
            ))
            .unwrap(),
            SpheroidConvention::OblateMinorVertical
        );
        for category in [
            WrfPropertyCategory::IshmaelPlanar,
            WrfPropertyCategory::IshmaelColumnar,
            WrfPropertyCategory::IshmaelAggregate,
        ] {
            assert!(matches!(
                spheroid_for_characteristic_category(category),
                Err(WrfTMatrixSceneBuildError::IshmaelCharacteristicForbidden)
            ));
        }
    }

    #[test]
    fn production_ishmael_psd_omission_contract_is_independent_by_moment() {
        let config = production_ishmael_psd_config();
        assert_eq!(
            config.revision(),
            radar_scattering::SchemePsdRevision::IshmaelGammaFinalCheckRayleighBridgeV4
        );
        assert_eq!(
            config.small_sphere_scattering_policy(),
            IshmaelSmallSphereScatteringPolicy::RayleighLimitBelowTableDiameterFloorV1
        );
        assert_eq!(
            config.maximum_domain_omitted_number_fraction(),
            ISHMAEL_PSD_MAXIMUM_OMITTED_NUMBER_FRACTION
        );
        assert_eq!(
            config.maximum_domain_omitted_mass_fraction(),
            ISHMAEL_PSD_MAXIMUM_OMITTED_MASS_FRACTION
        );
        assert_eq!(
            config.maximum_domain_omitted_d6_fraction(),
            ISHMAEL_PSD_MAXIMUM_OMITTED_D6_FRACTION
        );
        assert!(
            config.maximum_domain_omitted_d6_fraction()
                < config.maximum_domain_omitted_mass_fraction()
        );
        assert!(
            config.maximum_domain_omitted_mass_fraction()
                < config.maximum_domain_omitted_number_fraction()
        );
    }

    #[test]
    fn every_role_rejects_one_coordinate_of_asset_drift() {
        for role in [
            WrfTMatrixTableRole::DryOblate,
            WrfTMatrixTableRole::DryProlate,
            WrfTMatrixTableRole::WetOblate,
            WrfTMatrixTableRole::WetProlate,
            WrfTMatrixTableRole::RainStandaloneAndResidual,
        ] {
            let kinds = match role {
                WrfTMatrixTableRole::DryOblate | WrfTMatrixTableRole::DryProlate => DRY_AXIS_KINDS,
                WrfTMatrixTableRole::WetOblate | WrfTMatrixTableRole::WetProlate => WET_AXIS_KINDS,
                WrfTMatrixTableRole::RainStandaloneAndResidual => RAIN_AXIS_KINDS,
            };
            for &kind in kinds {
                if kind == AxisKind::Frequency {
                    continue;
                }
                let expected = exact_axis_coordinates(role, kind).unwrap();
                validate_exact_axis_coordinates(role, kind, expected).unwrap();
                let mut drifted = expected.to_vec();
                let last = drifted.last_mut().unwrap();
                *last *= 1.0 + 1.0e-12;
                assert!(matches!(
                    validate_exact_axis_coordinates(role, kind, &drifted),
                    Err(WrfTMatrixBundleError::AxisCoordinates {
                        role: rejected_role,
                        kind: rejected_kind,
                        ..
                    }) if rejected_role == role && rejected_kind == kind
                ));
            }
        }
    }

    #[test]
    fn final_property_bundle_exact_axis_coordinate_counts_are_frozen() {
        for (role, expected_counts) in [
            (WrfTMatrixTableRole::DryOblate, &[80, 4, 55, 7, 1, 5][..]),
            (WrfTMatrixTableRole::DryProlate, &[59, 4, 28, 4, 1, 5][..]),
            (
                WrfTMatrixTableRole::WetOblate,
                &[53, 3, 32, 10, 4, 1, 5][..],
            ),
            (
                WrfTMatrixTableRole::WetProlate,
                &[45, 3, 32, 10, 4, 1, 5][..],
            ),
            (
                WrfTMatrixTableRole::RainStandaloneAndResidual,
                &[44, 4, 7, 1, 9][..],
            ),
        ] {
            let kinds = match role {
                WrfTMatrixTableRole::DryOblate | WrfTMatrixTableRole::DryProlate => DRY_AXIS_KINDS,
                WrfTMatrixTableRole::WetOblate | WrfTMatrixTableRole::WetProlate => WET_AXIS_KINDS,
                WrfTMatrixTableRole::RainStandaloneAndResidual => RAIN_AXIS_KINDS,
            };
            let actual_counts = kinds
                .iter()
                .map(|&kind| {
                    if kind == AxisKind::Frequency {
                        1
                    } else {
                        exact_axis_coordinates(role, kind).unwrap().len()
                    }
                })
                .collect::<Vec<_>>();
            assert_eq!(actual_counts, expected_counts, "{role} coordinate counts");
        }
    }

    #[test]
    fn every_role_requires_exact_solver_descriptor() {
        for role in [
            WrfTMatrixTableRole::DryOblate,
            WrfTMatrixTableRole::DryProlate,
            WrfTMatrixTableRole::WetOblate,
            WrfTMatrixTableRole::WetProlate,
            WrfTMatrixTableRole::RainStandaloneAndResidual,
        ] {
            let expected_ndgs = exact_solver_ndgs(role);
            validate_exact_solver(role, PROPERTY_SOLVER_DDELT, expected_ndgs).unwrap();
            assert!(matches!(
                validate_exact_solver(role, PROPERTY_SOLVER_DDELT + 1.0e-12, expected_ndgs),
                Err(WrfTMatrixBundleError::RadarSolverDdelt { .. })
            ));
            assert!(matches!(
                validate_exact_solver(role, PROPERTY_SOLVER_DDELT, expected_ndgs - 1),
                Err(WrfTMatrixBundleError::RadarSolverNdgs { .. })
            ));
        }
    }

    #[test]
    fn cold_boundary_never_dispatches_zero_liquid_to_wet_table() {
        assert!(!should_diagnose_wet_coexistence(
            WrfTMatrixRainMode::FullProperty,
            true,
            true,
            DIAGNOSTIC_COEXISTENCE_COLD_K,
        ));
        assert!(should_diagnose_wet_coexistence(
            WrfTMatrixRainMode::FullProperty,
            true,
            true,
            DIAGNOSTIC_COEXISTENCE_COLD_K + 1.0e-6,
        ));
        assert!(should_diagnose_wet_coexistence(
            WrfTMatrixRainMode::FullProperty,
            true,
            true,
            DIAGNOSTIC_COEXISTENCE_WARM_K,
        ));
        assert!(!should_diagnose_wet_coexistence(
            WrfTMatrixRainMode::FullProperty,
            true,
            true,
            DIAGNOSTIC_COEXISTENCE_WARM_K + 1.0e-6,
        ));
    }

    #[test]
    fn dry_frozen_temperature_mapping_is_an_upper_only_phase_constraint() {
        assert_eq!(dry_frozen_particle_temperature_k(190.0), 190.0);
        assert_eq!(
            dry_frozen_particle_temperature_k(ICE_MELTING_TEMPERATURE_K),
            ICE_MELTING_TEMPERATURE_K
        );
        assert_eq!(
            dry_frozen_particle_temperature_k(285.722_229),
            ICE_MELTING_TEMPERATURE_K
        );
        assert_eq!(dry_frozen_particle_temperature_k(180.0), 180.0);
        assert!(dry_frozen_particle_temperature_k(f64::NAN).is_nan());
    }

    #[test]
    fn query_interpolates_additive_components_and_preserves_signed_kdp() {
        let scene = synthetic_scene();
        let midpoint = scene.additive_at(3, 9.75).unwrap().unwrap();
        let values = midpoint.components();
        assert_eq!(values[0], 200.0);
        assert_eq!(values[1], 160.0);
        assert_eq!(values[2], 140.0);
        assert_eq!(values[3], 5.0);
        assert_eq!(values[4], 1.0);
        assert!((values[5] - 0.2).abs() < 1.0e-7);
        assert!((values[6] - 0.1).abs() < 1.0e-7);
        assert_eq!(values[7], 1_150.0);
        assert_eq!(values[8], 6_900.0);

        assert_eq!(
            scene.additive_at(3, -0.5).unwrap().unwrap().kdp().get(),
            -1.0
        );
        assert!(scene.additive_at(1, 0.0).unwrap().is_none());
    }

    #[test]
    fn weighted_query_keeps_clear_weight_as_exact_zero_and_signed_kdp() {
        let scene = synthetic_scene();
        let weighted = scene
            .weighted_additive_at(&[(3, 0.25), (1, 0.75)], -0.5)
            .unwrap();
        let values = weighted.components();
        assert_eq!(values[0], 25.0);
        assert_eq!(values[1], 20.0);
        assert_eq!(values[2], 17.5);
        assert_eq!(values[3], -1.25);
        assert_eq!(values[4], -0.25);
        assert!((values[5] - 0.025).abs() < 1.0e-8);
        assert!((values[6] - 0.0125).abs() < 1.0e-8);
        assert_eq!(values[7], 125.0);
        assert_eq!(values[8], 675.0);

        let all_clear = scene
            .weighted_additive_at(&[(0, 0.4), (1, 0.6)], 10.0)
            .unwrap();
        assert_eq!(all_clear, AdditiveScattering::default());
    }

    #[test]
    fn weighted_query_rejects_bad_shape_weights_cells_and_view() {
        let scene = synthetic_scene();
        assert!(matches!(
            scene.weighted_additive_at(&[], 0.0),
            Err(WrfTMatrixSceneQueryError::NoWeightedContributions)
        ));
        let too_many = [(0, 0.125); MAX_WEIGHTED_PROPERTY_CELLS + 1];
        assert!(matches!(
            scene.weighted_additive_at(&too_many, 0.0),
            Err(WrfTMatrixSceneQueryError::TooManyWeightedContributions { .. })
        ));
        assert!(matches!(
            scene.weighted_additive_at(&[(3, -0.1), (1, 1.1)], 0.0),
            Err(WrfTMatrixSceneQueryError::InvalidWeight { .. })
        ));
        assert!(matches!(
            scene.weighted_additive_at(&[(3, f64::NAN), (1, 0.0)], 0.0),
            Err(WrfTMatrixSceneQueryError::InvalidWeight { .. })
        ));
        assert!(matches!(
            scene.weighted_additive_at(&[(3, 0.5), (1, 0.4)], 0.0),
            Err(WrfTMatrixSceneQueryError::WeightSum { .. })
        ));
        assert!(matches!(
            scene.weighted_additive_at(&[(5, 1.0)], 0.0),
            Err(WrfTMatrixSceneQueryError::CellOutOfRange { .. })
        ));
        assert!(matches!(
            scene.weighted_additive_at(&[(3, 1.0)], 20.1),
            Err(WrfTMatrixSceneQueryError::ElevationOutsideAxis { .. })
        ));
    }

    #[test]
    fn query_never_clamps_or_extrapolates() {
        let scene = synthetic_scene();
        assert!(matches!(
            scene.additive_at(3, -0.500_001),
            Err(WrfTMatrixSceneQueryError::ElevationOutsideAxis { .. })
        ));
        assert!(matches!(
            scene.additive_at(3, 20.000_001),
            Err(WrfTMatrixSceneQueryError::ElevationOutsideAxis { .. })
        ));
        assert!(matches!(
            scene.additive_at(3, f64::NAN),
            Err(WrfTMatrixSceneQueryError::NonFiniteElevation { .. })
        ));
        assert!(matches!(
            scene.additive_at(5, 0.0),
            Err(WrfTMatrixSceneQueryError::CellOutOfRange { .. })
        ));
    }

    #[test]
    fn retained_memory_includes_every_owned_vector_and_identity() {
        let scene = synthetic_scene();
        assert_eq!(
            scene.additive_components.len(),
            scene.source_cell_indices.len()
                * scene.radar_elevations_deg.len()
                * AdditiveScattering::COMPONENT_COUNT
        );
        assert_eq!(
            scene.full_cell_to_compact_row.len(),
            scene.source_cell_count
        );
        assert_eq!(scene.full_cell_to_compact_row[3], 0);
        assert_eq!(scene.full_cell_to_compact_row[1], u32::MAX);
        let estimate = scene.memory_estimate();
        assert_eq!(estimate.source_index_bytes, size_of::<u32>());
        assert_eq!(estimate.dense_row_lookup_bytes, 5 * size_of::<u32>());
        assert_eq!(estimate.elevation_axis_bytes, 2 * size_of::<f64>());
        assert_eq!(
            estimate.additive_component_bytes,
            2 * AdditiveScattering::COMPONENT_COUNT * size_of::<f32>()
        );
        assert_eq!(estimate.source_identity_bytes, "fixture".len());
        assert_eq!(estimate.required_field_contract_bytes, 0);
        assert_eq!(estimate.source_field_provenance_bytes, 0);
        assert_eq!(
            estimate.provenance_text_bytes,
            scene
                .provenance
                .tables
                .iter()
                .map(|table| table.table_id.len())
                .sum::<usize>()
        );
        assert_eq!(
            estimate.retained_bytes(),
            estimate.structure_bytes
                + estimate.source_index_bytes
                + estimate.dense_row_lookup_bytes
                + estimate.elevation_axis_bytes
                + estimate.additive_component_bytes
                + estimate.source_identity_bytes
                + estimate.required_field_contract_bytes
                + estimate.source_field_provenance_bytes
                + estimate.provenance_text_bytes
        );
    }

    #[test]
    fn temporal_signature_survives_without_raw_property_arrays() {
        let scene = synthetic_scene();
        let signature = scene.temporal_signature("property-tmatrix-research-v1");
        assert_eq!(signature.microphysics_scheme_id, Some(50));
        assert_eq!(
            signature.reflectivity_source,
            format!(
                "property-tmatrix-research-v1;tmatrix_frequency_hz=2800000000;tmatrix_frequency_bits={:016x}",
                PROPERTY_TMATRIX_FREQUENCY_HZ.to_bits()
            )
        );
        assert!(signature.required_raw_fields.is_empty());
        assert_eq!(scene.source_identity().time_index, 2);
        assert_eq!(scene.source_identity().source_identity.0, "fixture");
    }

    #[test]
    fn audit_count_addition_is_checked() {
        let left = WrfTMatrixAuditCounts {
            source_cells: u64::MAX,
            ..WrfTMatrixAuditCounts::default()
        };
        assert!(
            left.checked_add(WrfTMatrixAuditCounts {
                source_cells: 1,
                ..WrfTMatrixAuditCounts::default()
            })
            .is_none()
        );
    }

    #[test]
    fn frozen_only_compaction_drops_rain_only_rows_and_preserves_order() {
        let mut components = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let summaries = vec![
            // FullProperty keeps this scene-union row for standalone rain;
            // FrozenOnly reports it unretained and compacts it away.
            CellBuildSummary {
                retained: false,
                counts: WrfTMatrixAuditCounts::default(),
            },
            CellBuildSummary {
                retained: true,
                counts: WrfTMatrixAuditCounts {
                    source_cells: 1,
                    dry_frozen_populations: 2,
                    ..WrfTMatrixAuditCounts::default()
                },
            },
            CellBuildSummary {
                retained: true,
                counts: WrfTMatrixAuditCounts {
                    source_cells: 1,
                    dry_frozen_populations: 1,
                    ..WrfTMatrixAuditCounts::default()
                },
            },
        ];
        let (indices, counts) =
            compact_frozen_only_rows(&mut components, 2, &[1, 3, 4], summaries).unwrap();
        assert_eq!(indices, vec![3, 4]);
        assert_eq!(components, vec![3.0, 4.0, 5.0, 6.0]);
        assert_eq!(counts.source_cells, 2);
        assert_eq!(counts.dry_frozen_populations, 3);
    }

    #[test]
    fn reported_ishmael_planar_cell_converges_with_embedded_tables() {
        let Some(wrf_path) = std::env::var_os("BOWECHO_ISHMAEL_FIXTURE") else {
            return;
        };
        let cell = std::env::var("BOWECHO_ISHMAEL_CONVERGENCE_CELL")
            .ok()
            .map(|value| value.parse::<usize>().expect("cell index is an integer"))
            .unwrap_or(3_672_172);

        let file = wrf_core::WrfFile::open(&wrf_path).expect("open ISHMAEL WRF fixture");
        assert_eq!(file.global_attr_i32("MP_PHYSICS").unwrap(), 55);
        let source = read_wrf_property_scene(
            &file,
            WrfSourceIdentity("fixture:ishmael-convergence".to_owned()),
            0,
        )
        .expect("read normalized ISHMAEL property scene");
        if cell == 3_216_000 {
            let audit = source.ishmael_source_state_normalization_audit();
            assert_eq!(
                audit.revision(),
                crate::wrf_property_reader::ISHMAEL_SOURCE_STATE_NORMALIZATION_REVISION
            );
            assert!(audit.positive_sub_qsmall_ice_tuples_cleared() > 0);
            let raw = source.raw_cell(cell).unwrap();
            let RawPropertyCategory::Ishmael(columnar) = &raw.categories()[1] else {
                panic!("exact fixture cell retains the columnar tuple slot")
            };
            assert_eq!(columnar.category, WrfPropertyCategory::IshmaelColumnar);
            assert_eq!(
                (
                    columnar.qice_kgkg.to_bits(),
                    columnar.qnice_per_kg.to_bits(),
                    columnar.qvoli_m3_per_kg.to_bits(),
                    columnar.qaoli_m3_per_kg.to_bits(),
                ),
                (0, 0, 0, 0)
            );
        }
        if cell == 5_554_071 {
            let raw = source.raw_cell(cell).expect("read exact aggregate cell");
            assert_eq!(raw.environment().temperature_k(), 244.504_241_943_359_38);
            let aggregate = raw
                .categories()
                .iter()
                .find_map(|category| match category {
                    RawPropertyCategory::Ishmael(value)
                        if value.category == WrfPropertyCategory::IshmaelAggregate =>
                    {
                        Some(value)
                    }
                    _ => None,
                })
                .expect("exact fixture cell retains the aggregate tuple");
            assert_eq!(
                (
                    aggregate.qice_kgkg,
                    aggregate.qnice_per_kg,
                    aggregate.qvoli_m3_per_kg,
                    aggregate.qaoli_m3_per_kg,
                ),
                (
                    f64::from(f32::from_bits(0x2ed0_f2c7)),
                    f64::from(f32::from_bits(0x3d4b_5185)),
                    f64::from(f32::from_bits(0x27e5_0322)),
                    f64::from(f32::from_bits(0x2685_5dbd)),
                )
            );
            let checked = IshmaelPsd::reconstruct_wrf_source_checked(
                IshmaelPsdInput::new(
                    radar_scattering::IshmaelIceCategory::Aggregate,
                    aggregate.qice_kgkg,
                    aggregate.qnice_per_kg,
                    aggregate.qvoli_m3_per_kg,
                    aggregate.qaoli_m3_per_kg,
                    raw.dry_air_density_kg_m3(),
                ),
                raw.environment().temperature_k(),
            )
            .expect("replay exact WRF cold-aggregate final check");
            let audit = checked.reconstruction_audit();
            assert!(audit.source_aggregate_final_check_applied);
            assert!(audit.source_cold_aggregate_reset_applied);
            assert!(!audit.source_aggregate_size_cap_applied);
            assert!(!audit.source_var_check_small_ice_applied);
            assert!(!audit.source_var_check_large_ice_applied);
            assert_eq!(checked.a_scale_m().to_bits(), 0x3f12_abe1_2cdc_d7cc);
            assert_eq!(checked.c_at_a_scale_m().to_bits(), 0x3ef6_312b_b658_2af7);
            assert_eq!(
                checked.aspect_power_delta().to_bits(),
                0x3fea_167c_939e_a245
            );
            assert_eq!(checked.bulk_density_kg_m3().to_bits(), 50.0_f64.to_bits());
            assert_eq!(
                checked.input().qice_kgkg().to_bits(),
                aggregate.qice_kgkg.to_bits()
            );
            assert_eq!(
                checked.input().qnice_per_kg().to_bits(),
                aggregate.qnice_per_kg.to_bits()
            );
        }
        if cell == 4_776_084 {
            let raw = source
                .raw_cell(cell)
                .expect("read exact spherical columnar cell");
            assert_eq!(
                raw.environment().temperature_k().to_bits(),
                0x4070_59ce_e000_0000
            );
            assert_eq!(raw.dry_air_density_kg_m3().to_bits(), 0x3fe7_fea8_0000_0000);
            let columnar = raw
                .categories()
                .iter()
                .find_map(|category| match category {
                    RawPropertyCategory::Ishmael(value)
                        if value.category == WrfPropertyCategory::IshmaelColumnar =>
                    {
                        Some(value)
                    }
                    _ => None,
                })
                .expect("exact fixture cell retains the columnar tuple");
            assert_eq!(
                (
                    columnar.qice_kgkg.to_bits(),
                    columnar.qnice_per_kg.to_bits(),
                    columnar.qvoli_m3_per_kg.to_bits(),
                    columnar.qaoli_m3_per_kg.to_bits(),
                ),
                (
                    0x3d9d_1c1a_4000_0000,
                    0x3fa9_54a7_c000_0000,
                    0x3c7f_e620_8000_0000,
                    0x3c7f_e620_8000_0000,
                )
            );
            let checked = IshmaelPsd::reconstruct_wrf_source_checked(
                IshmaelPsdInput::new(
                    radar_scattering::IshmaelIceCategory::Columnar,
                    columnar.qice_kgkg,
                    columnar.qnice_per_kg,
                    columnar.qvoli_m3_per_kg,
                    columnar.qaoli_m3_per_kg,
                    raw.dry_air_density_kg_m3(),
                ),
                raw.environment().temperature_k(),
            )
            .expect("replay exact WRF spherical-columnar source state");
            assert_eq!(checked.a_scale_m().to_bits(), 0x3ee1_4732_a1d5_7f71);
            assert_eq!(checked.c_at_a_scale_m().to_bits(), 0x3ee1_4732_a1d5_7f71);
            assert_eq!(checked.aspect_power_delta().to_bits(), 1.0_f64.to_bits());
            assert_eq!(
                checked.bulk_density_kg_m3().to_bits(),
                0x407d_beb2_f540_f757
            );
            let audit = checked.reconstruction_audit();
            assert_eq!(audit.delta_bound_excursion, 0.0);
            assert_eq!(audit.density_bound_excursion_kg_m3, 0.0);
            assert!(!audit.source_density_state_projected);
            assert_eq!(audit.qvoli_source_projection_relative_change, 0.0);
            assert_eq!(audit.qaoli_source_projection_relative_change, 0.0);
            assert_eq!(audit.qnice_source_projection_relative_change, 0.0);
            assert!(!audit.source_aggregate_final_check_applied);
            assert!(!audit.source_cold_aggregate_reset_applied);
            assert!(!audit.source_aggregate_size_cap_applied);
            assert!(!audit.source_number_floor_applied);
            assert!(!audit.source_moment_floor_applied);
            assert!(!audit.source_axis_floor_applied);
            assert!(!audit.source_var_check_small_ice_applied);
            assert!(!audit.source_var_check_large_ice_applied);
        }

        let owner = load_property_tmatrix_tables_exact(
            PropertyTMatrixTableSourceKind::LegacyEmbeddedSResearchV1,
            S_BAND_RESEARCH_FREQUENCY_HZ,
        )
        .expect("load authenticated embedded T-matrix tables");
        let tables = owner.borrowed_bundle();
        let validated = validate_bundle(tables).expect("validate embedded table bundle");
        let mut output = vec![
            0.0_f32;
            validated.radar_elevations_deg.len()
                * AdditiveScattering::COMPONENT_COUNT
        ];
        build_ishmael_psd_cell_into(
            &source,
            cell,
            tables,
            validated,
            WrfTMatrixRainMode::FrozenOnly,
            None,
            None,
            &mut output,
        )
        .unwrap_or_else(|error| panic!("reported ISHMAEL cell {cell} failed: {error}"));
    }

    #[test]
    fn reported_ishmael_domain_omission_is_strictly_rejected_and_hybrid_is_audited() {
        let Some(wrf_path) = std::env::var_os("BOWECHO_ISHMAEL_FIXTURE") else {
            return;
        };
        let cell = std::env::var("BOWECHO_ISHMAEL_HYBRID_CELL")
            .ok()
            .map(|value| value.parse::<usize>().expect("cell index is an integer"))
            .unwrap_or(4_293_607);
        let file = wrf_core::WrfFile::open(&wrf_path).expect("open ISHMAEL WRF fixture");
        assert_eq!(file.global_attr_i32("MP_PHYSICS").unwrap(), 55);
        let source = read_wrf_property_scene(
            &file,
            WrfSourceIdentity("fixture:ishmael-hybrid".to_owned()),
            0,
        )
        .expect("read normalized ISHMAEL property scene");
        let owner = load_property_tmatrix_tables_exact(
            PropertyTMatrixTableSourceKind::LegacyEmbeddedSResearchV1,
            S_BAND_RESEARCH_FREQUENCY_HZ,
        )
        .expect("load authenticated embedded T-matrix tables");
        let tables = owner.borrowed_bundle();
        let validated = validate_bundle(tables).expect("validate embedded table bundle");
        let mut output = vec![
            0.0_f32;
            validated.radar_elevations_deg.len()
                * AdditiveScattering::COMPONENT_COUNT
        ];

        let strict = build_cell_into(
            &source,
            cell,
            tables,
            validated,
            None,
            WrfTMatrixRainMode::FrozenOnly,
            WrfTMatrixScatteringPolicy::StrictFailClosed,
            None,
            None,
            &mut output,
        )
        .expect_err("captured unsupported cell must remain fail-closed in strict mode");
        assert!(matches!(
            strict,
            WrfTMatrixSceneBuildError::IshmaelPsdIntegration {
                source: PsdIntegrationError::Psd(PsdError::DomainOmission { .. }),
                ..
            }
        ));

        let hybrid = build_cell_into(
            &source,
            cell,
            tables,
            validated,
            None,
            WrfTMatrixRainMode::FrozenOnly,
            WrfTMatrixScatteringPolicy::HybridBulkRayleighV1,
            None,
            None,
            &mut output,
        )
        .expect("explicit Hybrid policy rebuilds the captured unsupported cell");
        assert!(hybrid.retained);
        assert_eq!(hybrid.counts.source_cells, 1);
        assert_eq!(hybrid.counts.scheme_native_psd_populations, 0);
        assert_eq!(hybrid.counts.hybrid_bulk_rayleigh_cells, 1);
        assert!(hybrid.counts.hybrid_bulk_rayleigh_populations > 0);
        assert!(output.iter().any(|value| *value > 0.0));
    }

    #[test]
    fn reported_ishmael_source_mass_gap_is_strictly_rejected_and_hybrid_is_audited() {
        let Some(wrf_path) = std::env::var_os("BOWECHO_ISHMAEL_FIXTURE") else {
            return;
        };
        let cell = std::env::var("BOWECHO_ISHMAEL_MASS_GAP_CELL")
            .ok()
            .map(|value| value.parse::<usize>().expect("cell index is an integer"))
            .unwrap_or(5_557_733);
        let file = wrf_core::WrfFile::open(&wrf_path).expect("open ISHMAEL WRF fixture");
        assert_eq!(file.global_attr_i32("MP_PHYSICS").unwrap(), 55);
        let source = read_wrf_property_scene(
            &file,
            WrfSourceIdentity("fixture:ishmael-source-mass-gap".to_owned()),
            0,
        )
        .expect("read normalized ISHMAEL property scene");
        let owner = load_property_tmatrix_tables_exact(
            PropertyTMatrixTableSourceKind::LegacyEmbeddedSResearchV1,
            S_BAND_RESEARCH_FREQUENCY_HZ,
        )
        .expect("load authenticated embedded T-matrix tables");
        let tables = owner.borrowed_bundle();
        let validated = validate_bundle(tables).expect("validate embedded table bundle");
        let mut output = vec![
            0.0_f32;
            validated.radar_elevations_deg.len()
                * AdditiveScattering::COMPONENT_COUNT
        ];

        let strict = build_cell_into(
            &source,
            cell,
            tables,
            validated,
            None,
            WrfTMatrixRainMode::FrozenOnly,
            WrfTMatrixScatteringPolicy::StrictFailClosed,
            None,
            None,
            &mut output,
        )
        .expect_err("captured source-state mass gap must remain fail-closed in strict mode");
        assert!(matches!(
            strict,
            WrfTMatrixSceneBuildError::IshmaelPsdIntegration {
                source: PsdIntegrationError::Psd(PsdError::SourceStateMassClosure { .. }),
                ..
            }
        ));

        let hybrid = build_cell_into(
            &source,
            cell,
            tables,
            validated,
            None,
            WrfTMatrixRainMode::FrozenOnly,
            WrfTMatrixScatteringPolicy::HybridBulkRayleighV1,
            None,
            None,
            &mut output,
        )
        .expect("explicit Hybrid policy rebuilds the captured source-state mass-gap cell");
        assert!(hybrid.retained);
        assert_eq!(hybrid.counts.source_cells, 1);
        assert_eq!(hybrid.counts.scheme_native_psd_populations, 0);
        assert_eq!(hybrid.counts.hybrid_bulk_rayleigh_cells, 1);
        assert!(hybrid.counts.hybrid_bulk_rayleigh_populations > 0);
        assert!(output.iter().any(|value| *value > 0.0));
    }

    #[test]
    fn reported_ishmael_planar_state_adaptively_refines_from_integral_magnitude() {
        let owner = load_property_tmatrix_tables_exact(
            PropertyTMatrixTableSourceKind::LegacyEmbeddedSResearchV1,
            S_BAND_RESEARCH_FREQUENCY_HZ,
        )
        .expect("load authenticated embedded T-matrix tables");
        let tables = owner.borrowed_bundle();
        let validated = validate_bundle(tables).expect("validate embedded table bundle");
        let distribution = IshmaelPsd::reconstruct(IshmaelPsdInput::new(
            radar_scattering::IshmaelIceCategory::Planar,
            1.760_302_043_019_024_2e-10,
            3.285_777_347_628_027e-4,
            4.228_335_533_134_264e-16,
            4.228_335_533_134_264e-16,
            0.957_008_183_002_471_9,
        ))
        .expect("reconstruct the reported cell's native planar PSD");
        let oblate_request = evaluation_request(
            validated.frequency_hz,
            -0.5,
            SpheroidConvention::OblateMinorVertical,
        )
        .unwrap();
        let prolate_request = evaluation_request(
            validated.frequency_hz,
            -0.5,
            SpheroidConvention::ProlateMajorVertical,
        )
        .unwrap();
        let prepare = |config| {
            prepare_ishmael_particle_integration(
                &distribution,
                config,
                validated.ishmael_particle_support,
                validated.ishmael_fall_speed,
                validated.ishmael_ice_material_density_kg_m3,
                tables,
                ICE_MELTING_TEMPERATURE_K,
                oblate_request,
                prolate_request,
            )
            .unwrap()
        };

        let base_only = PsdIntegrationConfig::new(
            8,
            256,
            96.0,
            1.0e-10,
            5.0e-8,
            5.0e-3,
            radar_scattering::DEFAULT_ADDITIVE_ABSOLUTE_TOLERANCES,
            ISHMAEL_PSD_MAXIMUM_OMITTED_NUMBER_FRACTION,
            ISHMAEL_PSD_MAXIMUM_OMITTED_MASS_FRACTION,
            ISHMAEL_PSD_MAXIMUM_OMITTED_D6_FRACTION,
        )
        .unwrap();
        let base_error = prepare(base_only).finish_cpu().unwrap_err();
        let PsdIntegrationError::Psd(PsdError::AdditiveConvergence {
            component,
            coarse_value,
            refined_value,
            magnitude,
            absolute_error,
            relative_error,
            ..
        }) = base_error
        else {
            panic!("reported state should fail only the bounded base comparison: {base_error}");
        };
        assert_eq!(component, 0);
        assert_eq!(
            coarse_value.to_bits(),
            0.000_341_404_441_271_602_17_f64.to_bits()
        );
        assert_eq!(
            refined_value.to_bits(),
            0.000_339_559_017_029_987_14_f64.to_bits()
        );
        assert_eq!(magnitude.to_bits(), coarse_value.to_bits());
        assert_eq!(
            absolute_error.to_bits(),
            0.000_001_845_424_241_615_03_f64.to_bits()
        );
        assert_eq!(
            relative_error.to_bits(),
            0.005_405_390_260_131_134_f64.to_bits()
        );

        let result = prepare(production_ishmael_psd_config())
            .finish_cpu()
            .expect("magnitude-triggered refinement should converge");
        let audit = result.audit();
        assert_eq!(audit.refinement_steps, 1);
        assert_eq!(
            audit.quadrature,
            radar_scattering::PsdQuadratureRule::CompositeGaussLegendre8AdaptiveRefinedV2
        );
        assert!(audit.refined_nodes_evaluated > audit.coarse_nodes_evaluated);
        assert!(
            audit.total_nodes_reduced
                > audit.coarse_nodes_evaluated + audit.refined_nodes_evaluated
        );
    }

    #[test]
    fn real_p3_fixture_reconstructs_every_reader_normalized_property_cell() {
        let (Some(wrf_path), Some(table_path)) = (
            std::env::var_os("BOWECHO_WRF_PROPERTY_FIXTURE"),
            std::env::var_os("BOWECHO_P3_TABLE_FIXTURE"),
        ) else {
            return;
        };

        let file = wrf_core::WrfFile::open(&wrf_path).expect("open BOWECHO_WRF_PROPERTY_FIXTURE");
        let scheme_id = file
            .global_attr_i32("MP_PHYSICS")
            .expect("fixture has MP_PHYSICS");
        let scheme = P3WrfScheme::try_from(scheme_id).expect("fixture uses supported P3 physics");
        let table_kind = required_p3_table_kind(scheme_id).expect("P3 scheme has a table kind");

        let qsmall = f64::from(radar_scattering::P3_WRF_QSMALL_KGKG);
        let bsmall = f64::from(1.0e-15_f32);
        let qice = file.read_var("QICE", 0).expect("read fixture QICE");
        let qnice = file.read_var("QNICE", 0).expect("read fixture QNICE");
        let negative_number_index = qice
            .iter()
            .zip(&qnice)
            .position(|(&mass, &number)| mass >= qsmall && number <= 0.0)
            .expect("fixture has active P3 ice with nonpositive QNICE");
        drop(qnice);

        let qir = file.read_var("QIR", 0).expect("read fixture QIR");
        let qib = file.read_var("QIB", 0).expect("read fixture QIB");
        let small_volume_index = qice
            .iter()
            .zip(&qir)
            .zip(&qib)
            .position(|((&mass, &rime_mass), &rime_volume)| {
                mass >= qsmall && rime_mass >= qsmall && rime_volume < bsmall
            })
            .expect("fixture has active P3 rime below the BIRIM activity floor");
        let low_density_index = qice
            .iter()
            .zip(&qir)
            .zip(&qib)
            .position(|((&mass, &rime_mass), &rime_volume)| {
                mass >= qsmall && rime_volume >= bsmall && rime_mass / rime_volume < 50.0
            })
            .expect("fixture has active P3 rime below the density floor");
        let high_density_index = qice
            .iter()
            .zip(&qir)
            .zip(&qib)
            .position(|((&mass, &rime_mass), &rime_volume)| {
                mass >= qsmall && rime_volume >= bsmall && rime_mass / rime_volume > 900.0
            })
            .expect("fixture has active P3 rime above the density ceiling");

        let bounded_negative_mass_limit = f64::from(1.0e-10_f32);
        let negative_qice_index = qice
            .iter()
            .position(|&value| value < 0.0 && value >= -bounded_negative_mass_limit)
            .expect("fixture contains a bounded negative QICE transport residue");
        assert!(
            qice[negative_qice_index] < 0.0
                && qice[negative_qice_index] >= -bounded_negative_mass_limit,
            "fixture no longer contains a negative QICE residue"
        );
        drop((qice, qir, qib));
        file.clear_cache();

        let scene = read_wrf_property_scene(
            &file,
            WrfSourceIdentity("fixture:p3-production-seam".to_owned()),
            0,
        )
        .expect("read normalized P3 property scene");
        let table = P3OfficialTableV54::load_path(table_kind, table_path)
            .expect("load BOWECHO_P3_TABLE_FIXTURE");

        let inactive = scene
            .raw_cell(negative_qice_index)
            .expect("read original negative-QICE cell");
        assert!(
            inactive
                .categories()
                .iter()
                .all(|category| category.mixing_ratio_kgkg() == 0.0),
            "negative QICE residue must be inactive"
        );

        let reconstruct = |cell_index: usize| {
            let raw = scene.raw_cell(cell_index).expect("read selected P3 cell");
            let value = raw
                .categories()
                .iter()
                .find_map(|category| match category {
                    RawPropertyCategory::P3(value) if value.qice_kgkg > 0.0 => Some(value),
                    _ => None,
                })
                .expect("selected source cell retains active P3 ice");
            let input = p3_psd_input(scheme, value, raw.dry_air_density_kg_m3())
                .expect("supported P3 scheme maps to a native P3 input");
            let psd = P3Psd::reconstruct(input, &table, P3ReconstructionConfig::default())
                .expect("production P3 input reconstructs");
            (input, psd)
        };

        let (negative_number, negative_number_psd) = reconstruct(negative_number_index);
        assert!(negative_number.total_number_per_kg <= 0.0);
        assert!(
            negative_number_psd
                .number_limiter_audit()
                .repaired_total_number_per_kg
                > 0.0
        );

        let (small_volume, _) = reconstruct(small_volume_index);
        assert_eq!(small_volume.rime_mass_kgkg, 0.0);
        assert_eq!(small_volume.rime_volume_m3_per_kg, 0.0);

        let (low_density, _) = reconstruct(low_density_index);
        assert!(
            (low_density.rime_mass_kgkg / low_density.rime_volume_m3_per_kg - 50.0).abs() < 1.0e-3
        );

        let (high_density, _) = reconstruct(high_density_index);
        assert!(
            (high_density.rime_mass_kgkg / high_density.rime_volume_m3_per_kg - 900.0).abs()
                < 1.0e-3
        );

        let source_qrain = file.read_var("QRAIN", 0).expect("read fixture QRAIN");
        let mut negative_rain_cells = 0_usize;
        let mut active_positive_rain_cells = 0_usize;
        for (cell_index, &source_mass) in source_qrain.iter().enumerate() {
            if source_mass < 0.0 {
                negative_rain_cells += 1;
                let raw = scene.raw_cell(cell_index).unwrap_or_else(|error| {
                    panic!("read negative-QRAIN cell {cell_index}: {error}")
                });
                assert_eq!(
                    raw.rain(),
                    &RawRainState::Available {
                        qrain_kgkg: 0.0,
                        qnrain_per_kg: 0.0,
                    },
                    "source QRAIN {source_mass} at cell {cell_index} must be normalized to available clear rain"
                );
            } else if source_mass >= qsmall {
                active_positive_rain_cells += 1;
                let raw = scene.raw_cell(cell_index).unwrap_or_else(|error| {
                    panic!("read active positive-QRAIN cell {cell_index}: {error}")
                });
                match raw.rain() {
                    RawRainState::Available {
                        qrain_kgkg,
                        qnrain_per_kg,
                    } => {
                        assert!(
                            *qrain_kgkg > 0.0,
                            "source QRAIN {source_mass} at cell {cell_index} was cleared"
                        );
                        assert!(
                            *qnrain_per_kg > 0.0,
                            "active rain number at cell {cell_index} was not normalized positive"
                        );
                    }
                    RawRainState::Unavailable(reason) => panic!(
                        "source QRAIN {source_mass} at cell {cell_index} became unavailable: {reason}"
                    ),
                }
            }
        }
        drop(source_qrain);
        file.clear_cache();
        assert!(
            negative_rain_cells > 0,
            "fixture must retain the reported negative QRAIN regression"
        );
        assert!(
            active_positive_rain_cells > 0,
            "fixture must contain active positive rain"
        );

        let reconstruction_config = P3ReconstructionConfig::default();
        let default_tolerance = reconstruction_config.maximum_moment_relative_error;
        let mut reconstructed_categories = 0_usize;
        let mut audited_nonmass_residuals_over_default = 0_usize;
        let mut worst_closure: Option<(f64, usize, P3Category, &'static str, P3PsdInput)> = None;

        for sparse_category in scene.categories() {
            let WrfPropertyCategory::P3(category) = sparse_category.category() else {
                continue;
            };
            for &compact_cell_index in sparse_category.active_cell_indices() {
                let cell_index = compact_cell_index as usize;
                let raw = scene
                    .raw_cell(cell_index)
                    .unwrap_or_else(|error| panic!("read active P3 cell {cell_index}: {error}"));
                let value = raw
                    .categories()
                    .iter()
                    .find_map(|raw_category| match raw_category {
                        RawPropertyCategory::P3(value) if value.category == category => Some(value),
                        _ => None,
                    })
                    .unwrap_or_else(|| {
                        panic!("active P3 cell {cell_index} is missing category {category:?}")
                    });
                let input = p3_psd_input(scheme, value, raw.dry_air_density_kg_m3())
                    .unwrap_or_else(|| {
                        panic!(
                            "production p3_psd_input rejected cell {cell_index}, category {category:?}, raw {value:#?}"
                        )
                    });
                let psd = P3Psd::reconstruct(input, &table, reconstruction_config).unwrap_or_else(
                    |error| {
                        panic!(
                            "production P3 reconstruction failed at cell {cell_index}, category {category:?}, input {input:#?}: {error}"
                        )
                    },
                );
                reconstructed_categories += 1;
                let closure = psd.closure_audit();
                for (moment, error) in [
                    ("number", Some(closure.number_relative_error)),
                    ("mass", Some(closure.mass_relative_error)),
                    ("sixth", closure.sixth_moment_relative_error),
                ] {
                    let Some(error) = error else {
                        continue;
                    };
                    if moment != "mass" && error > default_tolerance {
                        audited_nonmass_residuals_over_default += 1;
                    }
                    if worst_closure
                        .as_ref()
                        .is_none_or(|(worst, ..)| error > *worst)
                    {
                        worst_closure = Some((error, cell_index, category, moment, input));
                    }
                }
            }
        }

        let (worst_error, worst_cell, worst_category, worst_moment, worst_input) =
            worst_closure.expect("fixture contains an active P3 category");
        eprintln!(
            "P3 full-fixture sweep: reconstructed={reconstructed_categories}, negative_rain_cleared={negative_rain_cells}, active_positive_rain={active_positive_rain_cells}, audited_nonmass_residuals_over_default={audited_nonmass_residuals_over_default}, worst={worst_error} ({worst_moment}) at cell {worst_cell}, category {worst_category:?}, input {worst_input:#?}"
        );
    }

    #[test]
    fn reported_p3_low_density_tail_passes_mass_squared_radar_gate() {
        let (Some(wrf_path), Some(table_path)) = (
            std::env::var_os("BOWECHO_WRF_PROPERTY_FIXTURE"),
            std::env::var_os("BOWECHO_P3_TABLE_FIXTURE"),
        ) else {
            return;
        };
        let cell = std::env::var("BOWECHO_P3_SUPPORT_CELL")
            .ok()
            .map(|value| {
                value
                    .parse::<usize>()
                    .expect("BOWECHO_P3_SUPPORT_CELL is an integer")
            })
            .unwrap_or(5_779_317);

        let file = wrf_core::WrfFile::open(&wrf_path).expect("open WRF fixture");
        let scheme_id = file.global_attr_i32("MP_PHYSICS").expect("MP_PHYSICS");
        let scheme = P3WrfScheme::try_from(scheme_id).expect("P3 fixture");
        let table_kind = required_p3_table_kind(scheme_id).expect("P3 table kind");
        let scene = read_wrf_property_scene(
            &file,
            WrfSourceIdentity("fixture:p3-support-diagnostic".to_owned()),
            0,
        )
        .expect("read P3 scene");
        let table = P3OfficialTableV54::load_path(table_kind, table_path).expect("load P3 table");
        let raw = scene.raw_cell(cell).expect("read reported cell");

        for value in raw
            .categories()
            .iter()
            .filter_map(|category| match category {
                RawPropertyCategory::P3(value) if value.qice_kgkg > 0.0 => Some(value),
                _ => None,
            })
        {
            let input =
                p3_psd_input(scheme, value, raw.dry_air_density_kg_m3()).expect("map P3 input");
            let psd = P3Psd::reconstruct(input, &table, P3ReconstructionConfig::default())
                .expect("reconstruct P3 PSD");
            eprintln!(
                "cell={cell} category={:?} lambda={} mu={} input={input:#?}",
                value.category,
                psd.lambda_m_inv(),
                psd.mu()
            );
            {
                let minimum_density_kg_m3 = 1.5;
                let minimum_diameter_m = 50e-6;
                let quadrature = psd
                    .quadrature_with_dimension_breakpoints(
                        radar_scattering::P3QuadratureConfig::default(),
                        &[minimum_diameter_m, 0.089],
                    )
                    .expect("quadrature");
                let closure = psd.closure_audit();
                let mut omitted = [0.0_f64; 3];
                let mut by_diameter = [0.0_f64; 3];
                let mut by_density = [0.0_f64; 3];
                let mut by_ratio = [0.0_f64; 3];
                let mut by_source_area_overshoot = [0.0_f64; 3];
                let mut maximum_raw_area_ratio = f64::NEG_INFINITY;
                let mut minimum_coordinates = [f64::INFINITY; 3];
                let mut maximum_coordinates = [f64::NEG_INFINITY; 3];
                for node in &quadrature.nodes {
                    let raw_area_ratio = 4.0 * node.particle.projected_area_m2
                        / (std::f64::consts::PI * node.particle.maximum_dimension_m.powi(2));
                    maximum_raw_area_ratio = maximum_raw_area_ratio.max(raw_area_ratio);
                    let ratio = raw_area_ratio.min(1.0);
                    let equivolume_diameter_m = node.particle.maximum_dimension_m * ratio.cbrt();
                    let density_kg_m3 = node.particle.mass_kg
                        / (std::f64::consts::PI / 6.0 * equivolume_diameter_m.powi(3));
                    let coordinates = [equivolume_diameter_m, density_kg_m3, ratio];
                    for axis in 0..3 {
                        minimum_coordinates[axis] =
                            minimum_coordinates[axis].min(coordinates[axis]);
                        maximum_coordinates[axis] =
                            maximum_coordinates[axis].max(coordinates[axis]);
                    }
                    let weights = [
                        node.number_concentration_m3,
                        node.number_concentration_m3 * node.particle.mass_kg,
                        node.number_concentration_m3 * node.particle.mass_kg.powi(2),
                    ];
                    let diameter_outside =
                        !(minimum_diameter_m..=0.089).contains(&equivolume_diameter_m);
                    let density_outside = !(minimum_density_kg_m3..=917.0).contains(&density_kg_m3);
                    let ratio_outside = !(0.1..=1.0).contains(&ratio);
                    for moment in 0..3 {
                        if diameter_outside || density_outside || ratio_outside {
                            omitted[moment] += weights[moment];
                        }
                        if diameter_outside {
                            by_diameter[moment] += weights[moment];
                        }
                        if density_outside {
                            by_density[moment] += weights[moment];
                        }
                        if ratio_outside {
                            by_ratio[moment] += weights[moment];
                        }
                        if node.particle.projected_area_consistency()
                            == P3ProjectedAreaConsistency::PinnedFinalCoefficientTransitionOvershoot
                        {
                            by_source_area_overshoot[moment] += weights[moment];
                        }
                    }
                }
                let totals = [
                    closure.reconstructed_number_density_m3,
                    closure.reconstructed_mass_concentration_kg_m3,
                    quadrature
                        .nodes
                        .iter()
                        .map(|node| node.number_concentration_m3 * node.particle.mass_kg.powi(2))
                        .sum(),
                ];
                for values in [
                    &mut omitted,
                    &mut by_diameter,
                    &mut by_density,
                    &mut by_ratio,
                    &mut by_source_area_overshoot,
                ] {
                    for moment in 0..3 {
                        values[moment] /= totals[moment];
                    }
                }
                eprintln!(
                    "minRho={minimum_density_kg_m3:.9e} totals(number,mass,mass2-radar)={totals:?} omitted(number,mass,mass2-radar)={omitted:?} diameter={by_diameter:?} density={by_density:?} ratio={by_ratio:?} source_area_overshoot={by_source_area_overshoot:?} max_raw_area_ratio={maximum_raw_area_ratio} coords_min={minimum_coordinates:?} coords_max={maximum_coordinates:?}"
                );
                assert!(
                    omitted[2] < P3_MAXIMUM_OMITTED_RADAR_WEIGHT_FRACTION,
                    "reported cell {cell} mass-squared radar-weight omission {} must remain below {}",
                    omitted[2],
                    P3_MAXIMUM_OMITTED_RADAR_WEIGHT_FRACTION
                );
            }
        }
    }
}
