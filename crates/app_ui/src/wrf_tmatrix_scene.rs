//! Fail-closed property-aware T-matrix cache for one WRF model instant.
//!
//! The cache retains only sparse source-cell indices and nine additive f32
//! components at every validated radar-elevation LUT node.  Query-time
//! elevation interpolation happens component by component; nonlinear radar
//! products are derived only after interpolation.  These research tables are
//! never substituted with the bulk Rayleigh operator when a lookup fails.

use std::mem::size_of;
use std::sync::Arc;

use radar_scattering::{
    AdditiveScattering, AxisKind, ClosedParticleCategory, ConventionalHydrometeor,
    DIAGNOSTIC_COEXISTENCE_COLD_K, DIAGNOSTIC_COEXISTENCE_WARM_K, DiagnosticWetCategory,
    EvaluationError, FallMomentPolicy, ISHMAEL_PSD_REVISION, IshmaelPsd, IshmaelPsdInput,
    MixtureTopology, OrientationDefinition, OrientationModel, OutputError, P3_MODULE_VERSION,
    P3_PROJECTED_AREA_EQUIVALENT_OBLATE_REVISION, P3_PSD_REVISION,
    P3_SPHERICAL_INTEGRATION_REVISION, P3_TABLE_READER_REVISION, P3_WRF_SOURCE_COMMIT,
    P3IceMomentInput, P3LookupTableV54, P3OfficialTableKind, P3OfficialTableV54, P3Psd, P3PsdError,
    P3PsdInput, P3QuadratureNode, P3ReconstructionConfig, P3ShapeAuthority,
    P3TMatrixIntegrationConfig, P3TMatrixIntegrationError, P3TMatrixParticleNode,
    P3TMatrixShapePolicy, P3WrfScheme, ParticleState, PolarAccumulatorQuantities, PsdError,
    PsdFallSpeedProvenance, PsdIntegrationConfig, PsdIntegrationError, PsdParticleNode,
    PsdParticleSupport, PsdSpheroidHabit, RadarViewApplicability, RadarViewGeometry,
    ResearchTMatrixLut, SCHEME_PSD_REVISION, Sha256Digest, SpheroidConvention,
    TMatrixEvaluationRequest, TMatrixMaterial, TMatrixOdfConvention, TMatrixParticleCategory,
    TMatrixParticleNodeQuery, TMatrixPopulationRole, integrate_ishmael_psd,
    integrate_p3_tmatrix_psd,
};
use rayon::prelude::*;
use thiserror::Error;

use crate::wrf_property_reader::{
    ClosedCellCategory, ClosedPropertyCell, ClosedRainState, CoexistenceUnavailable,
    PropertySceneIdentity, RainUnavailableReason, RawPropertyCategory, RawPropertyCell,
    RawPropertyClosureError, RequiredFieldContract, RequiredFieldSignature, SourceFieldProvenance,
    WrfPropertyCategory, WrfPropertyReadError, WrfPropertyScene, close_raw_property_cell,
    close_raw_rain_state,
};
use crate::wrf_temporal::ScenePropertySignature;

pub const PROPERTY_TMATRIX_FREQUENCY_HZ: f64 = 2_800_000_000.0;
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
const ISHMAEL_PSD_MAXIMUM_OMITTED_D6_FRACTION: f64 = 0.001;

// P3 uses the same production omission gates. In strict mode nonspherical P3
// particles count as omitted; projected-area research mode maps them through
// its explicitly versioned equivalent-oblate assumption before this gate.
const P3_MAXIMUM_OMITTED_NUMBER_FRACTION: f64 = 0.999;
const P3_MAXIMUM_OMITTED_MASS_FRACTION: f64 = 0.05;
const P3_MAXIMUM_OMITTED_D6_FRACTION: f64 = 0.001;

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
const RAIN_TEMPERATURE_K: &[f64] = &[250.0, 269.15, 293.15, 313.15];
const RAIN_MINOR_TO_MAJOR: &[f64] = &[0.5, 0.6, 0.7, 0.775, 0.85, 0.925, 1.0];
const PROPERTY_FREQUENCY_HZ: &[f64] = &[2800000000.0];
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
const RAIN_TABLE_ID: &str = "property-rain-sband-pytmatrix-0.3.3-unvalidated-v1";

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
    const fn expected_id(self) -> &'static str {
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

    pub fn projected_area_equivalent_oblate_research(
        table: Arc<P3OfficialTableV54>,
    ) -> Result<Self, radar_scattering::P3TMatrixIntegrationConfigError> {
        Ok(Self {
            table,
            integration: production_p3_integration_config(
                P3TMatrixShapePolicy::ProjectedAreaEquivalentOblateGaussian20ResearchV1,
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
        match raw.microphysics_scheme_id() {
            50..=53 => {
                let p3 = self.p3.as_ref().ok_or_else(|| raw_p3_table_required(raw))?;
                validate_raw_p3_table(raw.microphysics_scheme_id(), p3.table.as_ref())?;
                evaluate_p3_psd_raw_cell(raw, self.tables, self.validated, p3, elevation_deg)
            }
            55 => evaluate_ishmael_psd_raw_cell(raw, self.tables, self.validated, elevation_deg),
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
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WrfTMatrixTableAudit {
    pub role: WrfTMatrixTableRole,
    pub table_id: &'static str,
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
    /// research mode uses the named projected-area-equivalent oblate mapping.
    P3SchemeNativePsdV1 {
        p3_psd_revision: &'static str,
        table_reader_revision: &'static str,
        integration_revision: &'static str,
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
        Self::build_internal(source, tables, WrfTMatrixRainMode::FullProperty, None)
    }

    pub fn build_with_rain_mode(
        source: &WrfPropertyScene,
        tables: WrfTMatrixLutBundle<'_>,
        rain_mode: WrfTMatrixRainMode,
    ) -> Result<Self, WrfTMatrixSceneBuildError> {
        Self::build_internal(source, tables, rain_mode, None)
    }

    pub fn build_with_p3(
        source: &WrfPropertyScene,
        tables: WrfTMatrixLutBundle<'_>,
        p3: WrfTMatrixP3Resources,
    ) -> Result<Self, WrfTMatrixSceneBuildError> {
        Self::build_internal(source, tables, WrfTMatrixRainMode::FullProperty, Some(p3))
    }

    pub fn build_with_p3_and_rain_mode(
        source: &WrfPropertyScene,
        tables: WrfTMatrixLutBundle<'_>,
        p3: WrfTMatrixP3Resources,
        rain_mode: WrfTMatrixRainMode,
    ) -> Result<Self, WrfTMatrixSceneBuildError> {
        Self::build_internal(source, tables, rain_mode, Some(p3))
    }

    fn build_internal(
        source: &WrfPropertyScene,
        tables: WrfTMatrixLutBundle<'_>,
        rain_mode: WrfTMatrixRainMode,
        p3: Option<WrfTMatrixP3Resources>,
    ) -> Result<Self, WrfTMatrixSceneBuildError> {
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
            table_id: role.expected_id(),
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
                frequency_hz: PROPERTY_TMATRIX_FREQUENCY_HZ,
                orientation: OrientationDefinition::Gaussian20Research,
                frozen_scattering: if source.microphysics_scheme_id() == 55 {
                    WrfTMatrixFrozenScatteringAudit::IshmaelSchemeNativePsdDryFrozenV1 {
                        scheme_psd_revision: SCHEME_PSD_REVISION,
                        ishmael_reconstruction_revision: ISHMAEL_PSD_REVISION,
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
                    };
                    WrfTMatrixFrozenScatteringAudit::P3SchemeNativePsdV1 {
                        p3_psd_revision: P3_PSD_REVISION,
                        table_reader_revision: P3_TABLE_READER_REVISION,
                        integration_revision,
                        wrf_source_commit: P3_WRF_SOURCE_COMMIT,
                        p3_module_version: P3_MODULE_VERSION,
                        table_kind: p3.table.kind(),
                        table_sha256: p3.table.descriptor().table_sha256,
                        integration: p3.integration,
                        shape_authority: P3ShapeAuthority::MaximumDimensionAndProjectedAreaOnly,
                        limitation: match p3.integration.shape_policy() {
                            P3TMatrixShapePolicy::StrictShapeAuthoritativeSpheres => {
                                "only P3 regions explicitly defined as spheres are evaluated; nonspherical mass/area-only nodes are omitted under strict budgets"
                            }
                            P3TMatrixShapePolicy::ProjectedAreaEquivalentOblateGaussian20ResearchV1 => {
                                "lambda/mu and PSD weights are exact P3; oblate shape is projected-area-equivalent and Gaussian-20 canting is an external research assumption, not P3-predicted habit or orientation"
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
        ScenePropertySignature {
            microphysics_scheme_id: Some(self.microphysics_scheme_id),
            reflectivity_source: reflectivity_source.into(),
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
            .sum();
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
    radar_elevations_deg: &'a [f64],
    p3_particle_support: PsdParticleSupport,
    p3_fall_speed: PsdFallSpeedProvenance,
    ishmael_psd_config: PsdIntegrationConfig,
    ishmael_particle_support: PsdParticleSupport,
    ishmael_fall_speed: PsdFallSpeedProvenance,
}

fn production_p3_integration_config(
    shape_policy: P3TMatrixShapePolicy,
) -> Result<P3TMatrixIntegrationConfig, radar_scattering::P3TMatrixIntegrationConfigError> {
    P3TMatrixIntegrationConfig::new(
        shape_policy,
        radar_scattering::P3QuadratureConfig::default(),
        P3_MAXIMUM_OMITTED_NUMBER_FRACTION,
        P3_MAXIMUM_OMITTED_MASS_FRACTION,
        P3_MAXIMUM_OMITTED_D6_FRACTION,
    )
}

fn production_ishmael_psd_config() -> PsdIntegrationConfig {
    PsdIntegrationConfig::new(
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
    .expect("the versioned production ISHMAEL PSD config is valid")
}

#[derive(Debug)]
struct CellBuildSummary {
    retained: bool,
    counts: WrfTMatrixAuditCounts,
}

fn build_cell_into(
    source: &WrfPropertyScene,
    cell_index: usize,
    tables: WrfTMatrixLutBundle<'_>,
    validated: ValidatedBundle<'_>,
    p3: Option<&WrfTMatrixP3Resources>,
    rain_mode: WrfTMatrixRainMode,
    output: &mut [f32],
) -> Result<CellBuildSummary, WrfTMatrixSceneBuildError> {
    if source.microphysics_scheme_id() == 55 {
        build_ishmael_psd_cell_into(source, cell_index, tables, validated, rain_mode, output)
    } else {
        build_p3_psd_cell_into(
            source,
            cell_index,
            tables,
            validated,
            p3.expect("P3 resources validated before parallel scene build"),
            rain_mode,
            output,
        )
    }
}

fn build_p3_psd_cell_into(
    source: &WrfPropertyScene,
    cell_index: usize,
    tables: WrfTMatrixLutBundle<'_>,
    validated: ValidatedBundle<'_>,
    p3: &WrfTMatrixP3Resources,
    rain_mode: WrfTMatrixRainMode,
    output: &mut [f32],
) -> Result<CellBuildSummary, WrfTMatrixSceneBuildError> {
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
        let oblate_request =
            evaluation_request(elevation_deg, SpheroidConvention::OblateMinorVertical)?;
        let prolate_request =
            evaluation_request(elevation_deg, SpheroidConvention::ProlateMajorVertical)?;
        let mut additive = AdditiveScattering::default();
        for (category, distribution) in &frozen {
            let p3_input = distribution.input();
            let rime_fraction = p3_input.rime_mass_kgkg / p3_input.total_ice_kgkg;
            let rime_density = if p3_input.rime_mass_kgkg > 0.0 {
                Some(p3_input.rime_mass_kgkg / p3_input.rime_volume_m3_per_kg)
            } else {
                None
            };
            let integration = integrate_p3_tmatrix_psd(
                distribution,
                p3.integration,
                validated.p3_particle_support,
                |node: &P3TMatrixParticleNode| {
                    let (table, request) = match node.habit() {
                        PsdSpheroidHabit::Oblate | PsdSpheroidHabit::Spherical => {
                            (tables.dry_oblate, oblate_request)
                        }
                        PsdSpheroidHabit::Prolate => (tables.dry_prolate, prolate_request),
                    };
                    let positive_down_speed = table.dry_particle_geometry_terminal_speed_m_s(
                        node.equivolume_diameter_m(),
                        node.bulk_density_kg_m3(),
                    )?;
                    let query = TMatrixParticleNodeQuery::new(
                        raw.environment().temperature_k(),
                        node.equivolume_diameter_m(),
                        node.bulk_density_kg_m3(),
                        node.minor_to_major_axis_ratio(),
                        node.habit(),
                        Some(rime_fraction),
                        rime_density,
                        positive_down_speed,
                        validated.p3_fall_speed,
                        gaussian20_orientation(),
                        request,
                    )?;
                    table.evaluate_dry_particle_node_per_m3(&query)
                },
            )
            .map_err(|source| WrfTMatrixSceneBuildError::P3PsdIntegration {
                cell_index,
                elevation_deg,
                category: *category,
                source,
            })?;
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
                let request = evaluation_request(elevation_deg, spheroid)?;
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
                let request =
                    evaluation_request(elevation_deg, SpheroidConvention::OblateMinorVertical)?;
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
                let request = evaluation_request(elevation_deg, spheroid)?;
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
                let request =
                    evaluation_request(elevation_deg, SpheroidConvention::OblateMinorVertical)?;
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

fn build_ishmael_psd_cell_into(
    source: &WrfPropertyScene,
    cell_index: usize,
    tables: WrfTMatrixLutBundle<'_>,
    validated: ValidatedBundle<'_>,
    rain_mode: WrfTMatrixRainMode,
    output: &mut [f32],
) -> Result<CellBuildSummary, WrfTMatrixSceneBuildError> {
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
        let distribution = IshmaelPsd::reconstruct(IshmaelPsdInput::new(
            ishmael_category,
            value.qice_kgkg,
            value.qnice_per_kg,
            value.qvoli_m3_per_kg,
            value.qaoli_m3_per_kg,
            raw.dry_air_density_kg_m3(),
        ))
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
        let oblate_request =
            evaluation_request(elevation_deg, SpheroidConvention::OblateMinorVertical)?;
        let prolate_request =
            evaluation_request(elevation_deg, SpheroidConvention::ProlateMajorVertical)?;
        let mut additive = AdditiveScattering::default();
        for &(category, distribution) in &frozen {
            let integration = integrate_ishmael_psd(
                &distribution,
                validated.ishmael_psd_config,
                validated.ishmael_particle_support,
                validated.ishmael_fall_speed,
                |node: &PsdParticleNode| {
                    let (table, request) = match node.habit() {
                        PsdSpheroidHabit::Oblate | PsdSpheroidHabit::Spherical => {
                            (tables.dry_oblate, oblate_request)
                        }
                        PsdSpheroidHabit::Prolate => (tables.dry_prolate, prolate_request),
                    };
                    let positive_down_speed = table.dry_particle_node_terminal_speed_m_s(node)?;
                    let query = TMatrixParticleNodeQuery::from_psd_node(
                        node,
                        raw.environment().temperature_k(),
                        positive_down_speed,
                        validated.ishmael_fall_speed,
                        gaussian20_orientation(),
                        request,
                    )?;
                    table.evaluate_dry_particle_node_per_m3(&query)
                },
            )
            .map_err(|source| WrfTMatrixSceneBuildError::IshmaelPsdIntegration {
                cell_index,
                elevation_deg,
                category,
                source,
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

fn gaussian20_orientation() -> OrientationModel {
    OrientationModel::GaussianCanting {
        mean_deg: 0.0,
        standard_deviation_deg: 20.0,
        quadrature_points: 50,
    }
}

fn evaluate_p3_psd_raw_cell(
    raw: &RawPropertyCell,
    tables: WrfTMatrixLutBundle<'_>,
    validated: ValidatedBundle<'_>,
    p3: &WrfTMatrixP3Resources,
    elevation_deg: f64,
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

    let oblate_request =
        raw_evaluation_request(elevation_deg, SpheroidConvention::OblateMinorVertical)?;
    let prolate_request =
        raw_evaluation_request(elevation_deg, SpheroidConvention::ProlateMajorVertical)?;
    let mut additive = AdditiveScattering::default();
    for (category, distribution) in &frozen {
        let p3_input = distribution.input();
        let rime_fraction = p3_input.rime_mass_kgkg / p3_input.total_ice_kgkg;
        let rime_density = if p3_input.rime_mass_kgkg > 0.0 {
            Some(p3_input.rime_mass_kgkg / p3_input.rime_volume_m3_per_kg)
        } else {
            None
        };
        let integration = integrate_p3_tmatrix_psd(
            distribution,
            p3.integration,
            validated.p3_particle_support,
            |node: &P3TMatrixParticleNode| {
                let (table, request) = match node.habit() {
                    PsdSpheroidHabit::Oblate | PsdSpheroidHabit::Spherical => {
                        (tables.dry_oblate, oblate_request)
                    }
                    PsdSpheroidHabit::Prolate => (tables.dry_prolate, prolate_request),
                };
                let positive_down_speed = table.dry_particle_geometry_terminal_speed_m_s(
                    node.equivolume_diameter_m(),
                    node.bulk_density_kg_m3(),
                )?;
                let query = TMatrixParticleNodeQuery::new(
                    raw.environment().temperature_k(),
                    node.equivolume_diameter_m(),
                    node.bulk_density_kg_m3(),
                    node.minor_to_major_axis_ratio(),
                    node.habit(),
                    Some(rime_fraction),
                    rime_density,
                    positive_down_speed,
                    validated.p3_fall_speed,
                    gaussian20_orientation(),
                    request,
                )?;
                table.evaluate_dry_particle_node_per_m3(&query)
            },
        )
        .map_err(|source| WrfTMatrixRawEvaluationError::P3PsdIntegration {
            category: *category,
            source,
        })?;
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

#[allow(dead_code)]
fn evaluate_characteristic_raw_cell(
    raw: &RawPropertyCell,
    tables: WrfTMatrixLutBundle<'_>,
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
            let request = raw_evaluation_request(elevation_deg, spheroid)?;
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
            let request =
                raw_evaluation_request(elevation_deg, SpheroidConvention::OblateMinorVertical)?;
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
            let request = raw_evaluation_request(elevation_deg, spheroid)?;
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
            let request =
                raw_evaluation_request(elevation_deg, SpheroidConvention::OblateMinorVertical)?;
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

fn evaluate_ishmael_psd_raw_cell(
    raw: &RawPropertyCell,
    tables: WrfTMatrixLutBundle<'_>,
    validated: ValidatedBundle<'_>,
    elevation_deg: f64,
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
        let distribution = IshmaelPsd::reconstruct(IshmaelPsdInput::new(
            ishmael_category,
            value.qice_kgkg,
            value.qnice_per_kg,
            value.qvoli_m3_per_kg,
            value.qaoli_m3_per_kg,
            raw.dry_air_density_kg_m3(),
        ))
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
    let oblate_request =
        raw_evaluation_request(elevation_deg, SpheroidConvention::OblateMinorVertical)?;
    let prolate_request =
        raw_evaluation_request(elevation_deg, SpheroidConvention::ProlateMajorVertical)?;
    let mut additive = AdditiveScattering::default();
    for &(category, distribution) in &frozen {
        let integration = integrate_ishmael_psd(
            &distribution,
            validated.ishmael_psd_config,
            validated.ishmael_particle_support,
            validated.ishmael_fall_speed,
            |node: &PsdParticleNode| {
                let (table, request) = match node.habit() {
                    PsdSpheroidHabit::Oblate | PsdSpheroidHabit::Spherical => {
                        (tables.dry_oblate, oblate_request)
                    }
                    PsdSpheroidHabit::Prolate => (tables.dry_prolate, prolate_request),
                };
                let positive_down_speed = table.dry_particle_node_terminal_speed_m_s(node)?;
                let query = TMatrixParticleNodeQuery::from_psd_node(
                    node,
                    raw.environment().temperature_k(),
                    positive_down_speed,
                    validated.ishmael_fall_speed,
                    gaussian20_orientation(),
                    request,
                )?;
                table.evaluate_dry_particle_node_per_m3(&query)
            },
        )
        .map_err(
            |source| WrfTMatrixRawEvaluationError::IshmaelPsdIntegration { category, source },
        )?;
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

fn raw_evaluation_request(
    elevation_deg: f64,
    spheroid: SpheroidConvention,
) -> Result<TMatrixEvaluationRequest, WrfTMatrixRawEvaluationError> {
    let view = RadarViewGeometry::new(elevation_deg)
        .map_err(WrfTMatrixRawEvaluationError::EvaluationRequest)?;
    TMatrixEvaluationRequest::new(PROPERTY_TMATRIX_FREQUENCY_HZ, spheroid, view)
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
    elevation_deg: f64,
    spheroid: SpheroidConvention,
) -> Result<TMatrixEvaluationRequest, WrfTMatrixSceneBuildError> {
    let view = RadarViewGeometry::new(elevation_deg)
        .map_err(WrfTMatrixSceneBuildError::EvaluationRequest)?;
    TMatrixEvaluationRequest::new(PROPERTY_TMATRIX_FREQUENCY_HZ, spheroid, view)
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
    // Quantization must still satisfy covariance and fall-moment invariants;
    // never repair an invalid compact tuple by saturation or clamping.
    decode_components(&compact).map_err(|error| match error {
        WrfTMatrixSceneQueryError::Output(source) => CompactScatteringError::RoundTrip(source),
        _ => unreachable!("decoding a fixed nine-component array cannot be a storage error"),
    })?;
    Ok(compact)
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
        if descriptor.table_id() != role.expected_id() {
            return Err(WrfTMatrixBundleError::TableId {
                role,
                expected: role.expected_id(),
                actual: descriptor.table_id().to_owned(),
            });
        }
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
            validate_exact_axis_coordinates(role, axis.kind(), axis.coordinates())?;
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

    for role in [
        WrfTMatrixTableRole::DryOblate,
        WrfTMatrixTableRole::DryProlate,
    ] {
        let table = table_for_role(tables, role);
        if !matches!(
            table.descriptor().material(),
            TMatrixMaterial::SymmetricBruggemanSphericalAirIceMatzler2006V1 { .. }
        ) {
            return Err(WrfTMatrixBundleError::DryMaterial { role });
        }
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
    if reference_frequency != [PROPERTY_TMATRIX_FREQUENCY_HZ] {
        return Err(WrfTMatrixBundleError::FrequencyAxis {
            role: WrfTMatrixTableRole::DryOblate,
            actual: reference_frequency.to_vec(),
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
        if frequency != reference_frequency {
            return Err(WrfTMatrixBundleError::SharedFrequencyAxis { role });
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
        radar_elevations_deg: reference_elevations,
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
        (_, AxisKind::Frequency) => Some(PROPERTY_FREQUENCY_HZ),
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

#[derive(Debug, Error)]
pub enum WrfTMatrixSceneBuildError {
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
    #[error("{role} table id must be {expected:?}, got {actual:?}")]
    TableId {
        role: WrfTMatrixTableRole,
        expected: &'static str,
        actual: String,
    },
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
    #[error("dry-oblate frequency axis must be exactly [2.8e9], got {actual:?}")]
    FrequencyAxis {
        role: WrfTMatrixTableRole,
        actual: Vec<f64>,
    },
    #[error("{role} frequency axis differs from the exact dry-oblate axis")]
    SharedFrequencyAxis { role: WrfTMatrixTableRole },
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
    use crate::wrf_scene_inventory::WrfSourceIdentity;

    fn additive(values: [f64; 9]) -> AdditiveScattering {
        AdditiveScattering::from_components(values).unwrap()
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
                tables: [
                    WrfTMatrixTableAudit {
                        role: WrfTMatrixTableRole::DryOblate,
                        table_id: DRY_OBLATE_TABLE_ID,
                        file_sha256: Sha256Digest::compute(b"a"),
                    },
                    WrfTMatrixTableAudit {
                        role: WrfTMatrixTableRole::DryProlate,
                        table_id: DRY_PROLATE_TABLE_ID,
                        file_sha256: Sha256Digest::compute(b"b"),
                    },
                    WrfTMatrixTableAudit {
                        role: WrfTMatrixTableRole::WetOblate,
                        table_id: WET_OBLATE_TABLE_ID,
                        file_sha256: Sha256Digest::compute(b"c"),
                    },
                    WrfTMatrixTableAudit {
                        role: WrfTMatrixTableRole::WetProlate,
                        table_id: WET_PROLATE_TABLE_ID,
                        file_sha256: Sha256Digest::compute(b"d"),
                    },
                    WrfTMatrixTableAudit {
                        role: WrfTMatrixTableRole::RainStandaloneAndResidual,
                        table_id: RAIN_TABLE_ID,
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
                .map(|&kind| exact_axis_coordinates(role, kind).unwrap().len())
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
        assert_eq!(estimate.provenance_text_bytes, 0);
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
            "property-tmatrix-research-v1"
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
    fn frozen_only_compaction_is_in_place_and_preserves_order() {
        let mut components = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let summaries = vec![
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
}
