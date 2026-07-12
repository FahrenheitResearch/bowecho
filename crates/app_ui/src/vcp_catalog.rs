//! Versioned, renderer-independent WSR-88D scan definitions.
//!
//! The Build 24 baseline below is a transcription of WSR-88D ROC interface
//! control document 2620002AA, Appendix C.  Its rows are physical antenna
//! rotations in source order.  Equal elevations therefore remain separate:
//! collapsing them would erase split cuts and the two fixed MPDA Doppler cuts
//! in VCP 112.
//!
//! PRF values in this module are the source document's numbered PRF *codes*.
//! They are deliberately not converted to frequencies.  On SZCD rows the
//! non-default cells are azimuth rates, while the default cell is a 64-pulse
//! count; [`DopplerPrfValue`] keeps that distinction explicit.

/// Primary-source metadata for this checked catalog revision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScanDefinitionSource {
    pub issuing_organization: &'static str,
    pub document_number: &'static str,
    pub revision: &'static str,
    pub code_identification: &'static str,
    pub issue_date: &'static str,
    pub rda_build: &'static str,
    pub appendix: &'static str,
    pub public_url: &'static str,
}

pub const BUILD_24_SOURCE: ScanDefinitionSource = ScanDefinitionSource {
    issuing_organization: "WSR-88D Radar Operations Center",
    document_number: "2620002AA",
    revision: "AA",
    code_identification: "0WY55",
    issue_date: "2025-08-19",
    rda_build: "24.0",
    appendix: "Appendix C - Volume Coverage Patterns",
    public_url: "https://www.roc.noaa.gov/public-documents/icds/2620002AA.pdf",
};

/// The Build 24 VCPs defined by Appendix C of [`BUILD_24_SOURCE`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u16)]
pub enum Build24Vcp {
    Vcp12 = 12,
    Vcp34 = 34,
    Vcp35 = 35,
    Vcp112 = 112,
    Vcp212 = 212,
    Vcp215 = 215,
}

impl Build24Vcp {
    pub const ALL: [Self; 6] = [
        Self::Vcp12,
        Self::Vcp34,
        Self::Vcp35,
        Self::Vcp112,
        Self::Vcp212,
        Self::Vcp215,
    ];

    pub const fn number(self) -> u16 {
        self as u16
    }
}

impl TryFrom<u16> for Build24Vcp {
    type Error = ();

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            12 => Ok(Self::Vcp12),
            34 => Ok(Self::Vcp34),
            35 => Ok(Self::Vcp35),
            112 => Ok(Self::Vcp112),
            212 => Ok(Self::Vcp212),
            215 => Ok(Self::Vcp215),
            _ => Err(()),
        }
    }
}

/// A scan-strategy identity that does not mislabel old archives or synthetic
/// scans as a current operational definition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScanStrategy {
    /// A checked definition from this Build 24 catalog.
    Build24(Build24Vcp),
    /// A numbered VCP from a different/unknown build.  VCP numbers can be
    /// redefined between builds, so it must not silently use Build 24 rows.
    LegacyVcp { number: u16 },
    /// A research, synthetic, or user-authored scan with no operational VCP
    /// claim.
    Custom { name: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScanRegime {
    ClearAir,
    Precipitation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PulseLength {
    Short,
    Long,
}

/// Appendix C waveform abbreviations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Waveform {
    /// Contiguous surveillance.
    ContiguousSurveillance,
    /// Contiguous Doppler with range ambiguity.
    ContiguousDopplerWithRangeAmbiguity,
    /// Batch surveillance/Doppler pulses within one rotation.
    Batch,
    /// Contiguous Doppler without range ambiguity.
    ContiguousDopplerWithoutRangeAmbiguity,
    /// Contiguous surveillance with SZ-2 phase coding.
    Sz2ContiguousSurveillance,
    /// Contiguous Doppler with SZ-2 phase coding.
    Sz2ContiguousDoppler,
}

impl Waveform {
    pub const fn abbreviation(self) -> &'static str {
        match self {
            Self::ContiguousSurveillance => "CS",
            Self::ContiguousDopplerWithRangeAmbiguity => "CD/W",
            Self::Batch => "B",
            Self::ContiguousDopplerWithoutRangeAmbiguity => "CD/WO",
            Self::Sz2ContiguousSurveillance => "SZCS",
            Self::Sz2ContiguousDoppler => "SZCD",
        }
    }

    /// Expected contribution of this physical row to the logical Level-II
    /// cut.  At split-cut elevations, surveillance supplies reflectivity and
    /// dual-polarization variables while the range-ambiguous Doppler rotation
    /// supplies velocity/spectrum width.  Batch and range-unambiguous Doppler
    /// rotations supply the complete set.
    pub const fn moment_coverage(self) -> MomentCoverage {
        match self {
            Self::ContiguousSurveillance | Self::Sz2ContiguousSurveillance => {
                MomentCoverage::SURVEILLANCE
            }
            Self::ContiguousDopplerWithRangeAmbiguity | Self::Sz2ContiguousDoppler => {
                MomentCoverage::DOPPLER
            }
            Self::Batch | Self::ContiguousDopplerWithoutRangeAmbiguity => MomentCoverage::ALL,
        }
    }
}

/// Base-moment/dual-pol coverage, represented without a dependency on a flag
/// crate so the catalog remains a pure data module.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MomentCoverage(u8);

impl MomentCoverage {
    const REFLECTIVITY_BIT: u8 = 1 << 0;
    const VELOCITY_BIT: u8 = 1 << 1;
    const SPECTRUM_WIDTH_BIT: u8 = 1 << 2;
    const DIFFERENTIAL_REFLECTIVITY_BIT: u8 = 1 << 3;
    const CORRELATION_COEFFICIENT_BIT: u8 = 1 << 4;
    const DIFFERENTIAL_PHASE_BIT: u8 = 1 << 5;

    pub const SURVEILLANCE: Self = Self(
        Self::REFLECTIVITY_BIT
            | Self::DIFFERENTIAL_REFLECTIVITY_BIT
            | Self::CORRELATION_COEFFICIENT_BIT
            | Self::DIFFERENTIAL_PHASE_BIT,
    );
    pub const DOPPLER: Self = Self(Self::VELOCITY_BIT | Self::SPECTRUM_WIDTH_BIT);
    pub const ALL: Self = Self(Self::SURVEILLANCE.0 | Self::DOPPLER.0);

    pub const fn has_reflectivity(self) -> bool {
        self.0 & Self::REFLECTIVITY_BIT != 0
    }

    pub const fn has_velocity(self) -> bool {
        self.0 & Self::VELOCITY_BIT != 0
    }

    pub const fn has_spectrum_width(self) -> bool {
        self.0 & Self::SPECTRUM_WIDTH_BIT != 0
    }

    pub const fn has_differential_reflectivity(self) -> bool {
        self.0 & Self::DIFFERENTIAL_REFLECTIVITY_BIT != 0
    }

    pub const fn has_correlation_coefficient(self) -> bool {
        self.0 & Self::CORRELATION_COEFFICIENT_BIT != 0
    }

    pub const fn has_differential_phase(self) -> bool {
        self.0 & Self::DIFFERENTIAL_PHASE_BIT != 0
    }
}

/// A source-table surveillance PRF code and its pulse count.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SurveillancePrf {
    pub code: u8,
    pub pulse_count: u16,
}

/// What a Doppler PRF table cell means.  No variant is a frequency.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DopplerPrfValue {
    PulseCount(u16),
    /// Appendix C's SZCD non-default-PRF cell value.
    AzimuthRateDegPerSecond(f32),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DopplerPrfCell {
    /// Numbered PRF code from Appendix C (normally 1 for long-pulse rows or
    /// 2..=8 for short-pulse rows).
    pub code: u8,
    pub value: DopplerPrfValue,
    /// Bold/underlined in the source table, or the sole fixed long-pulse cell.
    pub is_default: bool,
}

/// Whether the source allows the row's Doppler PRF code to vary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DopplerPrfPolicy {
    NotApplicable,
    Selectable,
    Fixed,
}

/// One physical Appendix C row, in antenna execution order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PhysicalScanRow {
    pub elevation_deg: f32,
    pub azimuth_rate_deg_per_second: f32,
    /// The table's source period for this rotation, not a measured duration.
    pub source_period_seconds: f32,
    pub waveform: Waveform,
    pub moments: MomentCoverage,
    pub surveillance_prf: Option<SurveillancePrf>,
    pub doppler_prf_policy: DopplerPrfPolicy,
    pub doppler_prfs: &'static [DopplerPrfCell],
}

impl PhysicalScanRow {
    const fn new(
        elevation_deg: f32,
        azimuth_rate_deg_per_second: f32,
        source_period_seconds: f32,
        waveform: Waveform,
        surveillance_prf: Option<SurveillancePrf>,
        doppler_prf_policy: DopplerPrfPolicy,
        doppler_prfs: &'static [DopplerPrfCell],
    ) -> Self {
        Self {
            elevation_deg,
            azimuth_rate_deg_per_second,
            source_period_seconds,
            waveform,
            moments: waveform.moment_coverage(),
            surveillance_prf,
            doppler_prf_policy,
            doppler_prfs,
        }
    }
}

/// Nominal base-pattern cadence.  This is intentionally approximate: it is
/// the sum of Appendix C row periods and excludes antenna transition/volume
/// overhead and any operational adaptation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ApproximateCadence {
    pub seconds: f32,
}

impl ApproximateCadence {
    pub const fn minutes(self) -> f32 {
        self.seconds / 60.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VcpDefinition {
    pub vcp: Build24Vcp,
    pub source: &'static ScanDefinitionSource,
    pub regime: ScanRegime,
    pub pulse_length: PulseLength,
    pub source_figure: &'static str,
    pub nominal_cadence: ApproximateCadence,
    pub rows: &'static [PhysicalScanRow],
}

impl VcpDefinition {
    /// Unique elevations while preserving the first physical occurrence.
    pub fn elevation_ladder_deg(&self) -> Vec<f32> {
        let mut elevations = Vec::new();
        for row in self.rows {
            if elevations.last().copied() != Some(row.elevation_deg) {
                elevations.push(row.elevation_deg);
            }
        }
        elevations
    }
}

const fn s(code: u8, pulse_count: u16) -> SurveillancePrf {
    SurveillancePrf { code, pulse_count }
}

const fn p(code: u8, pulse_count: u16, is_default: bool) -> DopplerPrfCell {
    DopplerPrfCell {
        code,
        value: DopplerPrfValue::PulseCount(pulse_count),
        is_default,
    }
}

const fn a(code: u8, azimuth_rate_deg_per_second: f32) -> DopplerPrfCell {
    DopplerPrfCell {
        code,
        value: DopplerPrfValue::AzimuthRateDegPerSecond(azimuth_rate_deg_per_second),
        is_default: false,
    }
}

const NONE: DopplerPrfPolicy = DopplerPrfPolicy::NotApplicable;
const SELECTABLE: DopplerPrfPolicy = DopplerPrfPolicy::Selectable;
const FIXED: DopplerPrfPolicy = DopplerPrfPolicy::Fixed;
const CS: Waveform = Waveform::ContiguousSurveillance;
const CDW: Waveform = Waveform::ContiguousDopplerWithRangeAmbiguity;
const B: Waveform = Waveform::Batch;
const CDWO: Waveform = Waveform::ContiguousDopplerWithoutRangeAmbiguity;
const SZCS: Waveform = Waveform::Sz2ContiguousSurveillance;
const SZCD: Waveform = Waveform::Sz2ContiguousDoppler;

const VCP_12_ROWS: &[PhysicalScanRow] = &[
    PhysicalScanRow::new(0.5, 21.149, 17.02, CS, Some(s(1, 15)), NONE, &[]),
    PhysicalScanRow::new(
        0.5,
        24.994,
        14.40,
        CDW,
        None,
        SELECTABLE,
        &[
            p(2, 32, false),
            p(3, 34, false),
            p(4, 37, false),
            p(5, 40, true),
            p(6, 43, false),
            p(7, 46, false),
            p(8, 50, false),
        ],
    ),
    PhysicalScanRow::new(0.9, 21.149, 17.02, CS, Some(s(1, 15)), NONE, &[]),
    PhysicalScanRow::new(
        0.9,
        24.994,
        14.40,
        CDW,
        None,
        SELECTABLE,
        &[
            p(2, 32, false),
            p(3, 34, false),
            p(4, 37, false),
            p(5, 40, true),
            p(6, 43, false),
            p(7, 46, false),
            p(8, 50, false),
        ],
    ),
    PhysicalScanRow::new(1.3, 23.031, 15.63, CS, Some(s(2, 15)), NONE, &[]),
    PhysicalScanRow::new(
        1.3,
        25.994,
        14.40,
        CDW,
        None,
        SELECTABLE,
        &[
            p(2, 32, false),
            p(3, 34, false),
            p(4, 37, false),
            p(5, 40, true),
            p(6, 43, false),
            p(7, 46, false),
            p(8, 50, false),
        ],
    ),
    PhysicalScanRow::new(
        1.8,
        25.716,
        14.00,
        B,
        Some(s(3, 3)),
        SELECTABLE,
        &[
            p(2, 23, false),
            p(3, 25, false),
            p(4, 27, false),
            p(5, 29, true),
            p(6, 32, false),
            p(7, 34, false),
            p(8, 37, false),
        ],
    ),
    PhysicalScanRow::new(
        2.4,
        25.934,
        13.88,
        B,
        Some(s(4, 3)),
        SELECTABLE,
        &[
            p(2, 23, false),
            p(3, 25, false),
            p(4, 27, false),
            p(5, 30, true),
            p(6, 32, false),
            p(7, 35, false),
            p(8, 38, false),
        ],
    ),
    PhysicalScanRow::new(
        3.1,
        26.738,
        13.46,
        B,
        Some(s(5, 3)),
        SELECTABLE,
        &[
            p(2, 23, false),
            p(3, 25, false),
            p(4, 27, false),
            p(5, 30, true),
            p(6, 32, false),
            p(7, 35, false),
            p(8, 38, false),
        ],
    ),
    PhysicalScanRow::new(
        4.0,
        27.594,
        13.05,
        B,
        Some(s(6, 3)),
        SELECTABLE,
        &[
            p(2, 23, false),
            p(3, 25, false),
            p(4, 27, false),
            p(5, 30, true),
            p(6, 32, false),
            p(7, 35, false),
            p(8, 38, false),
        ],
    ),
    PhysicalScanRow::new(
        5.1,
        27.665,
        13.01,
        B,
        Some(s(6, 3)),
        SELECTABLE,
        &[
            p(2, 24, false),
            p(3, 26, false),
            p(4, 28, false),
            p(5, 31, true),
            p(6, 33, false),
            p(7, 36, false),
            p(8, 39, false),
        ],
    ),
    PhysicalScanRow::new(
        6.4,
        27.614,
        12.86,
        B,
        Some(s(6, 3)),
        SELECTABLE,
        &[
            p(2, 25, false),
            p(3, 27, false),
            p(4, 29, false),
            p(5, 32, true),
            p(6, 35, false),
            p(7, 37, false),
            p(8, 40, false),
        ],
    ),
    PhysicalScanRow::new(
        8.0,
        28.400,
        12.68,
        CDWO,
        None,
        SELECTABLE,
        &[
            p(2, 28, false),
            p(3, 30, false),
            p(4, 32, false),
            p(5, 35, false),
            p(6, 38, true),
            p(7, 41, false),
            p(8, 44, false),
        ],
    ),
    PhysicalScanRow::new(
        10.0,
        28.807,
        12.50,
        CDWO,
        None,
        SELECTABLE,
        &[
            p(2, 27, false),
            p(3, 29, false),
            p(4, 32, false),
            p(5, 35, false),
            p(6, 37, false),
            p(7, 40, true),
            p(8, 44, false),
        ],
    ),
    PhysicalScanRow::new(
        12.5,
        28.490,
        12.64,
        CDWO,
        None,
        SELECTABLE,
        &[
            p(2, 28, false),
            p(3, 30, false),
            p(4, 32, false),
            p(5, 35, false),
            p(6, 38, false),
            p(7, 41, false),
            p(8, 44, true),
        ],
    ),
    PhysicalScanRow::new(
        15.6,
        28.490,
        12.64,
        CDWO,
        None,
        SELECTABLE,
        &[
            p(2, 28, false),
            p(3, 30, false),
            p(4, 32, false),
            p(5, 35, false),
            p(6, 38, false),
            p(7, 41, false),
            p(8, 44, true),
        ],
    ),
    PhysicalScanRow::new(
        19.5,
        28.490,
        12.64,
        CDWO,
        None,
        SELECTABLE,
        &[
            p(2, 28, false),
            p(3, 30, false),
            p(4, 32, false),
            p(5, 35, false),
            p(6, 38, false),
            p(7, 41, false),
            p(8, 44, true),
        ],
    ),
];

const VCP_212_ROWS: &[PhysicalScanRow] = &[
    PhysicalScanRow::new(0.5, 21.149, 17.02, SZCS, Some(s(1, 15)), NONE, &[]),
    PhysicalScanRow::new(
        0.5,
        17.108,
        21.30,
        SZCD,
        None,
        SELECTABLE,
        &[
            a(2, 12.533),
            a(3, 13.393),
            a(4, 14.468),
            a(5, 15.836),
            p(6, 64, true),
            a(7, 18.455),
            a(8, 20.032),
        ],
    ),
    PhysicalScanRow::new(0.9, 21.149, 17.02, SZCS, Some(s(1, 15)), NONE, &[]),
    PhysicalScanRow::new(
        0.9,
        17.108,
        21.30,
        SZCD,
        None,
        SELECTABLE,
        &[
            a(2, 12.533),
            a(3, 13.393),
            a(4, 14.468),
            a(5, 15.836),
            p(6, 64, true),
            a(7, 18.455),
            a(8, 20.032),
        ],
    ),
    PhysicalScanRow::new(1.3, 23.031, 17.02, SZCS, Some(s(2, 15)), NONE, &[]),
    PhysicalScanRow::new(
        1.3,
        17.108,
        21.30,
        SZCD,
        None,
        SELECTABLE,
        &[
            a(2, 12.533),
            a(3, 13.393),
            a(4, 14.468),
            a(5, 15.836),
            p(6, 64, true),
            a(7, 18.455),
            a(8, 20.032),
        ],
    ),
    PhysicalScanRow::new(
        1.8,
        26.385,
        13.64,
        B,
        Some(s(3, 3)),
        SELECTABLE,
        &[
            p(2, 21, false),
            p(3, 23, false),
            p(4, 26, false),
            p(5, 28, true),
            p(6, 30, false),
            p(7, 32, false),
            p(8, 35, false),
        ],
    ),
    PhysicalScanRow::new(
        2.4,
        27.332,
        13.17,
        B,
        Some(s(4, 3)),
        SELECTABLE,
        &[
            p(2, 22, false),
            p(3, 24, false),
            p(4, 26, false),
            p(5, 28, true),
            p(6, 31, false),
            p(7, 33, false),
            p(8, 36, false),
        ],
    ),
    PhysicalScanRow::new(
        3.1,
        28.227,
        12.75,
        B,
        Some(s(5, 3)),
        SELECTABLE,
        &[
            p(2, 22, false),
            p(3, 24, false),
            p(4, 26, false),
            p(5, 28, true),
            p(6, 31, false),
            p(7, 33, false),
            p(8, 36, false),
        ],
    ),
    PhysicalScanRow::new(
        4.0,
        26.400,
        13.64,
        B,
        Some(s(6, 3)),
        SELECTABLE,
        &[
            p(2, 23, false),
            p(3, 25, false),
            p(4, 27, false),
            p(5, 30, true),
            p(6, 32, false),
            p(7, 35, false),
            p(8, 38, false),
        ],
    ),
    PhysicalScanRow::new(
        5.1,
        26.400,
        13.64,
        B,
        Some(s(6, 3)),
        SELECTABLE,
        &[
            p(2, 24, false),
            p(3, 26, false),
            p(4, 28, false),
            p(5, 31, true),
            p(6, 33, false),
            p(7, 36, false),
            p(8, 39, false),
        ],
    ),
    PhysicalScanRow::new(
        6.4,
        26.400,
        13.64,
        B,
        Some(s(6, 3)),
        SELECTABLE,
        &[
            p(2, 24, false),
            p(3, 26, false),
            p(4, 28, false),
            p(5, 31, true),
            p(6, 33, false),
            p(7, 36, false),
            p(8, 39, false),
        ],
    ),
    PhysicalScanRow::new(
        8.0,
        28.410,
        12.68,
        CDWO,
        None,
        SELECTABLE,
        &[
            p(2, 28, false),
            p(3, 30, false),
            p(4, 32, false),
            p(5, 35, false),
            p(6, 38, true),
            p(7, 41, false),
            p(8, 44, false),
        ],
    ),
    PhysicalScanRow::new(
        10.0,
        28.413,
        12.67,
        CDWO,
        None,
        SELECTABLE,
        &[
            p(2, 28, false),
            p(3, 30, false),
            p(4, 32, false),
            p(5, 35, false),
            p(6, 38, false),
            p(7, 41, true),
            p(8, 45, false),
        ],
    ),
    PhysicalScanRow::new(
        12.5,
        28.740,
        12.53,
        CDWO,
        None,
        SELECTABLE,
        &[
            p(2, 27, false),
            p(3, 29, false),
            p(4, 32, false),
            p(5, 35, false),
            p(6, 38, false),
            p(7, 41, false),
            p(8, 44, true),
        ],
    ),
    PhysicalScanRow::new(
        15.6,
        28.740,
        12.53,
        CDWO,
        None,
        SELECTABLE,
        &[
            p(2, 27, false),
            p(3, 29, false),
            p(4, 32, false),
            p(5, 35, false),
            p(6, 38, false),
            p(7, 41, false),
            p(8, 44, true),
        ],
    ),
    PhysicalScanRow::new(
        19.5,
        28.740,
        12.53,
        CDWO,
        None,
        SELECTABLE,
        &[
            p(2, 27, false),
            p(3, 29, false),
            p(4, 32, false),
            p(5, 35, false),
            p(6, 38, false),
            p(7, 41, false),
            p(8, 44, true),
        ],
    ),
];

const VCP_215_ROWS: &[PhysicalScanRow] = &[
    PhysicalScanRow::new(0.5, 11.460, 31.41, SZCS, Some(s(1, 28)), NONE, &[]),
    PhysicalScanRow::new(
        0.5,
        17.108,
        21.04,
        SZCD,
        None,
        SELECTABLE,
        &[
            a(2, 12.533),
            a(3, 13.393),
            a(4, 14.468),
            a(5, 15.836),
            p(6, 64, true),
            a(7, 18.455),
            a(8, 20.032),
        ],
    ),
    PhysicalScanRow::new(0.9, 13.375, 26.92, SZCS, Some(s(1, 24)), NONE, &[]),
    PhysicalScanRow::new(
        0.9,
        17.108,
        21.04,
        SZCD,
        None,
        SELECTABLE,
        &[
            a(2, 12.533),
            a(3, 13.393),
            a(4, 14.468),
            a(5, 15.836),
            p(6, 64, true),
            a(7, 18.455),
            a(8, 20.032),
        ],
    ),
    PhysicalScanRow::new(1.3, 15.921, 23.54, SZCS, Some(s(1, 22)), NONE, &[]),
    PhysicalScanRow::new(
        1.3,
        17.108,
        21.04,
        SZCD,
        None,
        SELECTABLE,
        &[
            a(2, 12.533),
            a(3, 13.393),
            a(4, 14.468),
            a(5, 15.836),
            p(6, 64, true),
            a(7, 18.455),
            a(8, 20.032),
        ],
    ),
    PhysicalScanRow::new(
        1.8,
        16.771,
        21.47,
        B,
        Some(s(3, 3)),
        SELECTABLE,
        &[
            p(2, 40, false),
            p(3, 42, false),
            p(4, 46, false),
            p(5, 50, true),
            p(6, 54, false),
            p(7, 58, false),
            p(8, 63, false),
        ],
    ),
    PhysicalScanRow::new(
        2.4,
        20.650,
        17.43,
        B,
        Some(s(4, 3)),
        SELECTABLE,
        &[
            p(2, 32, false),
            p(3, 34, false),
            p(4, 37, false),
            p(5, 40, true),
            p(6, 43, false),
            p(7, 47, false),
            p(8, 51, false),
        ],
    ),
    PhysicalScanRow::new(
        3.1,
        19.536,
        18.43,
        B,
        Some(s(5, 5)),
        SELECTABLE,
        &[
            p(2, 32, false),
            p(3, 34, false),
            p(4, 37, false),
            p(5, 40, true),
            p(6, 43, false),
            p(7, 47, false),
            p(8, 51, false),
        ],
    ),
    PhysicalScanRow::new(
        4.0,
        20.232,
        17.79,
        B,
        Some(s(6, 5)),
        SELECTABLE,
        &[
            p(2, 32, false),
            p(3, 34, false),
            p(4, 37, false),
            p(5, 40, true),
            p(6, 44, false),
            p(7, 47, false),
            p(8, 51, false),
        ],
    ),
    PhysicalScanRow::new(
        5.1,
        20.232,
        17.79,
        B,
        Some(s(6, 5)),
        SELECTABLE,
        &[
            p(2, 32, false),
            p(3, 34, false),
            p(4, 37, false),
            p(5, 40, true),
            p(6, 44, false),
            p(7, 47, false),
            p(8, 51, false),
        ],
    ),
    PhysicalScanRow::new(
        6.4,
        20.232,
        17.79,
        B,
        Some(s(6, 5)),
        SELECTABLE,
        &[
            p(2, 32, false),
            p(3, 34, false),
            p(4, 37, false),
            p(5, 40, true),
            p(6, 44, false),
            p(7, 47, false),
            p(8, 51, false),
        ],
    ),
    PhysicalScanRow::new(
        8.0,
        24.864,
        14.48,
        CDWO,
        None,
        SELECTABLE,
        &[
            p(2, 32, false),
            p(3, 34, false),
            p(4, 37, false),
            p(5, 41, false),
            p(6, 44, true),
            p(7, 47, false),
            p(8, 52, false),
        ],
    ),
    PhysicalScanRow::new(
        10.0,
        25.640,
        14.04,
        CDWO,
        None,
        SELECTABLE,
        &[
            p(2, 31, false),
            p(3, 33, false),
            p(4, 36, false),
            p(5, 40, false),
            p(6, 43, false),
            p(7, 46, false),
            p(8, 50, true),
        ],
    ),
    PhysicalScanRow::new(
        12.0,
        25.640,
        14.04,
        CDWO,
        None,
        SELECTABLE,
        &[
            p(2, 31, false),
            p(3, 33, false),
            p(4, 36, false),
            p(5, 40, false),
            p(6, 43, false),
            p(7, 46, false),
            p(8, 50, true),
        ],
    ),
    PhysicalScanRow::new(
        14.0,
        25.640,
        14.04,
        CDWO,
        None,
        SELECTABLE,
        &[
            p(2, 31, false),
            p(3, 33, false),
            p(4, 36, false),
            p(5, 40, false),
            p(6, 43, false),
            p(7, 46, false),
            p(8, 50, true),
        ],
    ),
    PhysicalScanRow::new(
        16.7,
        25.640,
        14.04,
        CDWO,
        None,
        SELECTABLE,
        &[
            p(2, 31, false),
            p(3, 33, false),
            p(4, 36, false),
            p(5, 40, false),
            p(6, 43, false),
            p(7, 46, false),
            p(8, 50, true),
        ],
    ),
    PhysicalScanRow::new(
        19.5,
        25.640,
        14.04,
        CDWO,
        None,
        SELECTABLE,
        &[
            p(2, 31, false),
            p(3, 33, false),
            p(4, 36, false),
            p(5, 40, false),
            p(6, 43, false),
            p(7, 46, false),
            p(8, 50, true),
        ],
    ),
];

const VCP_35_ROWS: &[PhysicalScanRow] = &[
    PhysicalScanRow::new(0.5, 4.966, 72.49, SZCS, Some(s(1, 64)), NONE, &[]),
    PhysicalScanRow::new(
        0.5,
        15.836,
        22.73,
        SZCD,
        None,
        SELECTABLE,
        &[
            a(2, 12.533),
            a(3, 13.393),
            a(4, 14.468),
            p(5, 64, true),
            a(6, 17.108),
            a(7, 18.455),
            a(8, 20.032),
        ],
    ),
    PhysicalScanRow::new(0.9, 4.966, 72.49, SZCS, Some(s(1, 64)), NONE, &[]),
    PhysicalScanRow::new(
        0.9,
        15.836,
        22.73,
        SZCD,
        None,
        SELECTABLE,
        &[
            a(2, 12.533),
            a(3, 13.393),
            a(4, 14.468),
            p(5, 64, true),
            a(6, 17.108),
            a(7, 18.455),
            a(8, 20.032),
        ],
    ),
    PhysicalScanRow::new(1.3, 5.473, 65.78, SZCS, Some(s(2, 64)), NONE, &[]),
    PhysicalScanRow::new(1.3, 15.836, 22.73, SZCD, None, FIXED, &[p(5, 64, true)]),
    PhysicalScanRow::new(
        1.8,
        15.489,
        23.24,
        B,
        Some(s(3, 3)),
        SELECTABLE,
        &[
            p(2, 44, false),
            p(3, 47, false),
            p(4, 50, false),
            p(5, 55, true),
            p(6, 59, false),
            p(7, 64, false),
            p(8, 70, false),
        ],
    ),
    PhysicalScanRow::new(
        2.4,
        17.756,
        20.27,
        B,
        Some(s(4, 3)),
        SELECTABLE,
        &[
            p(2, 38, false),
            p(3, 41, false),
            p(4, 44, false),
            p(5, 48, true),
            p(6, 52, false),
            p(7, 56, false),
            p(8, 61, false),
        ],
    ),
    PhysicalScanRow::new(
        3.1,
        16.926,
        21.27,
        B,
        Some(s(5, 5)),
        SELECTABLE,
        &[
            p(2, 38, false),
            p(3, 41, false),
            p(4, 44, false),
            p(5, 48, true),
            p(6, 52, false),
            p(7, 56, false),
            p(8, 61, false),
        ],
    ),
    PhysicalScanRow::new(
        4.0,
        18.068,
        19.92,
        B,
        Some(s(6, 5)),
        SELECTABLE,
        &[
            p(2, 36, false),
            p(3, 39, false),
            p(4, 42, false),
            p(5, 46, true),
            p(6, 50, false),
            p(7, 54, false),
            p(8, 58, false),
        ],
    ),
    PhysicalScanRow::new(
        5.1,
        18.068,
        19.92,
        B,
        Some(s(6, 5)),
        SELECTABLE,
        &[
            p(2, 36, false),
            p(3, 39, false),
            p(4, 42, false),
            p(5, 46, true),
            p(6, 50, false),
            p(7, 54, false),
            p(8, 58, false),
        ],
    ),
    PhysicalScanRow::new(
        6.4,
        18.068,
        19.92,
        B,
        Some(s(6, 5)),
        SELECTABLE,
        &[
            p(2, 36, false),
            p(3, 39, false),
            p(4, 42, false),
            p(5, 46, true),
            p(6, 50, false),
            p(7, 54, false),
            p(8, 58, false),
        ],
    ),
];

const VCP_112_ROWS: &[PhysicalScanRow] = &[
    PhysicalScanRow::new(0.5, 18.677, 19.29, SZCS, Some(s(1, 17)), NONE, &[]),
    PhysicalScanRow::new(0.5, 20.032, 17.97, SZCD, None, FIXED, &[p(8, 64, true)]),
    PhysicalScanRow::new(0.5, 14.468, 24.88, SZCD, None, FIXED, &[p(4, 64, true)]),
    PhysicalScanRow::new(0.9, 19.842, 18.14, SZCS, Some(s(1, 16)), NONE, &[]),
    PhysicalScanRow::new(0.9, 20.032, 17.97, SZCD, None, FIXED, &[p(8, 64, true)]),
    PhysicalScanRow::new(0.9, 15.836, 22.73, SZCD, None, FIXED, &[p(5, 64, true)]),
    PhysicalScanRow::new(1.3, 21.556, 17.51, SZCS, Some(s(2, 16)), NONE, &[]),
    PhysicalScanRow::new(1.3, 20.032, 17.97, SZCD, None, FIXED, &[p(8, 64, true)]),
    PhysicalScanRow::new(1.3, 17.108, 21.04, SZCD, None, FIXED, &[p(6, 64, true)]),
    PhysicalScanRow::new(
        1.8,
        26.385,
        13.64,
        B,
        Some(s(3, 3)),
        SELECTABLE,
        &[
            p(2, 21, false),
            p(3, 23, false),
            p(4, 26, false),
            p(5, 28, true),
            p(6, 30, false),
            p(7, 32, false),
            p(8, 35, false),
        ],
    ),
    PhysicalScanRow::new(
        2.4,
        27.332,
        13.17,
        B,
        Some(s(4, 3)),
        SELECTABLE,
        &[
            p(2, 22, false),
            p(3, 24, false),
            p(4, 26, false),
            p(5, 28, true),
            p(6, 31, false),
            p(7, 33, false),
            p(8, 36, false),
        ],
    ),
    PhysicalScanRow::new(
        3.1,
        28.227,
        12.75,
        B,
        Some(s(5, 3)),
        SELECTABLE,
        &[
            p(2, 22, false),
            p(3, 24, false),
            p(4, 26, false),
            p(5, 28, true),
            p(6, 31, false),
            p(7, 33, false),
            p(8, 36, false),
        ],
    ),
    PhysicalScanRow::new(
        4.0,
        26.400,
        13.67,
        B,
        Some(s(6, 3)),
        SELECTABLE,
        &[
            p(2, 23, false),
            p(3, 25, false),
            p(4, 27, false),
            p(5, 30, true),
            p(6, 32, false),
            p(7, 35, false),
            p(8, 46, false),
        ],
    ),
    PhysicalScanRow::new(
        5.1,
        26.000,
        13.64,
        B,
        Some(s(6, 3)),
        SELECTABLE,
        &[
            p(2, 24, false),
            p(3, 26, false),
            p(4, 28, false),
            p(5, 31, true),
            p(6, 33, false),
            p(7, 36, false),
            p(8, 59, false),
        ],
    ),
    PhysicalScanRow::new(
        6.4,
        26.400,
        13.64,
        B,
        Some(s(6, 3)),
        SELECTABLE,
        &[
            p(2, 24, false),
            p(3, 26, false),
            p(4, 28, false),
            p(5, 31, true),
            p(6, 33, false),
            p(7, 36, false),
            p(8, 44, false),
        ],
    ),
    PhysicalScanRow::new(
        8.0,
        28.418,
        12.68,
        CDWO,
        None,
        SELECTABLE,
        &[
            p(2, 28, false),
            p(3, 30, false),
            p(4, 32, false),
            p(5, 35, false),
            p(6, 38, true),
            p(7, 41, false),
            p(8, 44, false),
        ],
    ),
    PhysicalScanRow::new(
        10.0,
        28.413,
        12.67,
        CDWO,
        None,
        SELECTABLE,
        &[
            p(2, 28, false),
            p(3, 30, false),
            p(4, 32, false),
            p(5, 35, false),
            p(6, 38, false),
            p(7, 41, true),
            p(8, 44, false),
        ],
    ),
    PhysicalScanRow::new(
        12.5,
        28.740,
        12.67,
        CDWO,
        None,
        SELECTABLE,
        &[
            p(2, 27, false),
            p(3, 29, false),
            p(4, 32, false),
            p(5, 35, false),
            p(6, 38, false),
            p(7, 41, false),
            p(8, 44, true),
        ],
    ),
    PhysicalScanRow::new(
        15.6,
        28.740,
        12.67,
        CDWO,
        None,
        SELECTABLE,
        &[
            p(2, 27, false),
            p(3, 29, false),
            p(4, 32, false),
            p(5, 35, false),
            p(6, 38, false),
            p(7, 41, false),
            p(8, 44, true),
        ],
    ),
    PhysicalScanRow::new(
        19.5,
        28.740,
        12.67,
        CDWO,
        None,
        SELECTABLE,
        &[
            p(2, 27, false),
            p(3, 29, false),
            p(4, 32, false),
            p(5, 35, false),
            p(6, 38, false),
            p(7, 41, false),
            p(8, 44, true),
        ],
    ),
];

const VCP_34_ROWS: &[PhysicalScanRow] = &[
    PhysicalScanRow::new(0.5, 5.043, 71.39, CS, Some(s(1, 63)), NONE, &[]),
    PhysicalScanRow::new(0.5, 8.491, 42.40, CDW, None, FIXED, &[p(1, 52, true)]),
    PhysicalScanRow::new(0.9, 5.043, 71.39, CS, Some(s(1, 63)), NONE, &[]),
    PhysicalScanRow::new(0.9, 8.491, 42.40, CDW, None, FIXED, &[p(1, 52, true)]),
    PhysicalScanRow::new(1.3, 5.445, 66.12, CS, Some(s(2, 63)), NONE, &[]),
    PhysicalScanRow::new(1.3, 8.491, 42.40, CDW, None, FIXED, &[p(1, 52, true)]),
    PhysicalScanRow::new(
        1.8,
        5.880,
        61.22,
        B,
        Some(s(3, 11)),
        FIXED,
        &[p(1, 63, true)],
    ),
    PhysicalScanRow::new(2.4, 8.491, 42.40, CDWO, None, FIXED, &[p(1, 52, true)]),
    PhysicalScanRow::new(3.1, 8.491, 42.40, CDWO, None, FIXED, &[p(1, 52, true)]),
    PhysicalScanRow::new(4.5, 8.491, 42.40, CDWO, None, FIXED, &[p(1, 52, true)]),
];

pub const VCP_12: VcpDefinition = VcpDefinition {
    vcp: Build24Vcp::Vcp12,
    source: &BUILD_24_SOURCE,
    regime: ScanRegime::Precipitation,
    pulse_length: PulseLength::Short,
    source_figure: "Figure C-1",
    nominal_cadence: ApproximateCadence { seconds: 236.23 },
    rows: VCP_12_ROWS,
};

pub const VCP_34: VcpDefinition = VcpDefinition {
    vcp: Build24Vcp::Vcp34,
    source: &BUILD_24_SOURCE,
    regime: ScanRegime::ClearAir,
    pulse_length: PulseLength::Long,
    source_figure: "Figure C-8",
    nominal_cadence: ApproximateCadence { seconds: 524.52 },
    rows: VCP_34_ROWS,
};

pub const VCP_35: VcpDefinition = VcpDefinition {
    vcp: Build24Vcp::Vcp35,
    source: &BUILD_24_SOURCE,
    regime: ScanRegime::ClearAir,
    pulse_length: PulseLength::Short,
    source_figure: "Figure C-6",
    nominal_cadence: ApproximateCadence { seconds: 403.49 },
    rows: VCP_35_ROWS,
};

pub const VCP_112: VcpDefinition = VcpDefinition {
    vcp: Build24Vcp::Vcp112,
    source: &BUILD_24_SOURCE,
    regime: ScanRegime::Precipitation,
    pulse_length: PulseLength::Short,
    source_figure: "Figure C-7",
    nominal_cadence: ApproximateCadence { seconds: 321.37 },
    rows: VCP_112_ROWS,
};

pub const VCP_212: VcpDefinition = VcpDefinition {
    vcp: Build24Vcp::Vcp212,
    source: &BUILD_24_SOURCE,
    regime: ScanRegime::Precipitation,
    pulse_length: PulseLength::Short,
    source_figure: "Figure C-4",
    nominal_cadence: ApproximateCadence { seconds: 258.38 },
    rows: VCP_212_ROWS,
};

pub const VCP_215: VcpDefinition = VcpDefinition {
    vcp: Build24Vcp::Vcp215,
    source: &BUILD_24_SOURCE,
    regime: ScanRegime::Precipitation,
    pulse_length: PulseLength::Short,
    source_figure: "Figure C-5",
    nominal_cadence: ApproximateCadence { seconds: 340.37 },
    rows: VCP_215_ROWS,
};

/// Checked Build 24 definitions, ordered by VCP number.
pub const BUILD_24_DEFINITIONS: [&VcpDefinition; 6] =
    [&VCP_12, &VCP_34, &VCP_35, &VCP_112, &VCP_212, &VCP_215];

pub fn build_24_definition(number: u16) -> Option<&'static VcpDefinition> {
    match number {
        12 => Some(&VCP_12),
        34 => Some(&VCP_34),
        35 => Some(&VCP_35),
        112 => Some(&VCP_112),
        212 => Some(&VCP_212),
        215 => Some(&VCP_215),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn milli(value: f32) -> i32 {
        (value * 1_000.0).round() as i32
    }

    fn coverage_name(coverage: MomentCoverage) -> &'static str {
        if coverage == MomentCoverage::SURVEILLANCE {
            "SURV"
        } else if coverage == MomentCoverage::DOPPLER {
            "DOP"
        } else if coverage == MomentCoverage::ALL {
            "ALL"
        } else {
            "INVALID"
        }
    }

    fn policy_name(policy: DopplerPrfPolicy) -> &'static str {
        match policy {
            DopplerPrfPolicy::NotApplicable => "none",
            DopplerPrfPolicy::Selectable => "selectable",
            DopplerPrfPolicy::Fixed => "fixed",
        }
    }

    fn source_row(row: &PhysicalScanRow) -> String {
        let surveillance = row
            .surveillance_prf
            .map(|prf| format!("S{}:{}", prf.code, prf.pulse_count))
            .unwrap_or_else(|| "-".to_string());
        let doppler = if row.doppler_prfs.is_empty() {
            "-".to_string()
        } else {
            row.doppler_prfs
                .iter()
                .map(|cell| {
                    let value = match cell.value {
                        DopplerPrfValue::PulseCount(count) => format!("p{count}"),
                        DopplerPrfValue::AzimuthRateDegPerSecond(rate) => {
                            format!("a{}", milli(rate))
                        }
                    };
                    format!(
                        "{}:{}{}",
                        cell.code,
                        value,
                        if cell.is_default { "*" } else { "" }
                    )
                })
                .collect::<Vec<_>>()
                .join(",")
        };
        format!(
            "{}|{}|{}|{}|{}|{}|{}|{}",
            milli(row.elevation_deg),
            milli(row.azimuth_rate_deg_per_second),
            milli(row.source_period_seconds),
            row.waveform.abbreviation(),
            coverage_name(row.moments),
            surveillance,
            policy_name(row.doppler_prf_policy),
            doppler,
        )
    }

    fn assert_source_rows(definition: &VcpDefinition, expected: &str) {
        let actual = definition
            .rows
            .iter()
            .map(source_row)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(actual, expected.trim());
    }

    #[test]
    fn source_revision_and_catalog_are_version_locked() {
        assert_eq!(BUILD_24_SOURCE.document_number, "2620002AA");
        assert_eq!(BUILD_24_SOURCE.revision, "AA");
        assert_eq!(BUILD_24_SOURCE.issue_date, "2025-08-19");
        assert_eq!(BUILD_24_SOURCE.rda_build, "24.0");
        assert_eq!(
            BUILD_24_DEFINITIONS.map(|definition| definition.vcp.number()),
            [12, 34, 35, 112, 212, 215]
        );
        assert!(
            BUILD_24_DEFINITIONS
                .iter()
                .all(|definition| definition.source == &BUILD_24_SOURCE)
        );
        assert!(build_24_definition(31).is_none());
    }

    #[test]
    fn cadence_is_the_approximate_sum_of_physical_source_periods() {
        for definition in BUILD_24_DEFINITIONS {
            let sum: f32 = definition
                .rows
                .iter()
                .map(|row| row.source_period_seconds)
                .sum();
            assert!((sum - definition.nominal_cadence.seconds).abs() < 0.02);
        }
    }

    #[test]
    fn source_rows_vcp_12_are_exhaustive() {
        assert_source_rows(
            &VCP_12,
            r#"
500|21149|17020|CS|SURV|S1:15|none|-
500|24994|14400|CD/W|DOP|-|selectable|2:p32,3:p34,4:p37,5:p40*,6:p43,7:p46,8:p50
900|21149|17020|CS|SURV|S1:15|none|-
900|24994|14400|CD/W|DOP|-|selectable|2:p32,3:p34,4:p37,5:p40*,6:p43,7:p46,8:p50
1300|23031|15630|CS|SURV|S2:15|none|-
1300|25994|14400|CD/W|DOP|-|selectable|2:p32,3:p34,4:p37,5:p40*,6:p43,7:p46,8:p50
1800|25716|14000|B|ALL|S3:3|selectable|2:p23,3:p25,4:p27,5:p29*,6:p32,7:p34,8:p37
2400|25934|13880|B|ALL|S4:3|selectable|2:p23,3:p25,4:p27,5:p30*,6:p32,7:p35,8:p38
3100|26738|13460|B|ALL|S5:3|selectable|2:p23,3:p25,4:p27,5:p30*,6:p32,7:p35,8:p38
4000|27594|13050|B|ALL|S6:3|selectable|2:p23,3:p25,4:p27,5:p30*,6:p32,7:p35,8:p38
5100|27665|13010|B|ALL|S6:3|selectable|2:p24,3:p26,4:p28,5:p31*,6:p33,7:p36,8:p39
6400|27614|12860|B|ALL|S6:3|selectable|2:p25,3:p27,4:p29,5:p32*,6:p35,7:p37,8:p40
8000|28400|12680|CD/WO|ALL|-|selectable|2:p28,3:p30,4:p32,5:p35,6:p38*,7:p41,8:p44
10000|28807|12500|CD/WO|ALL|-|selectable|2:p27,3:p29,4:p32,5:p35,6:p37,7:p40*,8:p44
12500|28490|12640|CD/WO|ALL|-|selectable|2:p28,3:p30,4:p32,5:p35,6:p38,7:p41,8:p44*
15600|28490|12640|CD/WO|ALL|-|selectable|2:p28,3:p30,4:p32,5:p35,6:p38,7:p41,8:p44*
19500|28490|12640|CD/WO|ALL|-|selectable|2:p28,3:p30,4:p32,5:p35,6:p38,7:p41,8:p44*"#,
        );
    }

    #[test]
    fn source_rows_vcp_212_are_exhaustive() {
        assert_source_rows(
            &VCP_212,
            r#"
500|21149|17020|SZCS|SURV|S1:15|none|-
500|17108|21300|SZCD|DOP|-|selectable|2:a12533,3:a13393,4:a14468,5:a15836,6:p64*,7:a18455,8:a20032
900|21149|17020|SZCS|SURV|S1:15|none|-
900|17108|21300|SZCD|DOP|-|selectable|2:a12533,3:a13393,4:a14468,5:a15836,6:p64*,7:a18455,8:a20032
1300|23031|17020|SZCS|SURV|S2:15|none|-
1300|17108|21300|SZCD|DOP|-|selectable|2:a12533,3:a13393,4:a14468,5:a15836,6:p64*,7:a18455,8:a20032
1800|26385|13640|B|ALL|S3:3|selectable|2:p21,3:p23,4:p26,5:p28*,6:p30,7:p32,8:p35
2400|27332|13170|B|ALL|S4:3|selectable|2:p22,3:p24,4:p26,5:p28*,6:p31,7:p33,8:p36
3100|28227|12750|B|ALL|S5:3|selectable|2:p22,3:p24,4:p26,5:p28*,6:p31,7:p33,8:p36
4000|26400|13640|B|ALL|S6:3|selectable|2:p23,3:p25,4:p27,5:p30*,6:p32,7:p35,8:p38
5100|26400|13640|B|ALL|S6:3|selectable|2:p24,3:p26,4:p28,5:p31*,6:p33,7:p36,8:p39
6400|26400|13640|B|ALL|S6:3|selectable|2:p24,3:p26,4:p28,5:p31*,6:p33,7:p36,8:p39
8000|28410|12680|CD/WO|ALL|-|selectable|2:p28,3:p30,4:p32,5:p35,6:p38*,7:p41,8:p44
10000|28413|12670|CD/WO|ALL|-|selectable|2:p28,3:p30,4:p32,5:p35,6:p38,7:p41*,8:p45
12500|28740|12530|CD/WO|ALL|-|selectable|2:p27,3:p29,4:p32,5:p35,6:p38,7:p41,8:p44*
15600|28740|12530|CD/WO|ALL|-|selectable|2:p27,3:p29,4:p32,5:p35,6:p38,7:p41,8:p44*
19500|28740|12530|CD/WO|ALL|-|selectable|2:p27,3:p29,4:p32,5:p35,6:p38,7:p41,8:p44*"#,
        );
    }

    #[test]
    fn source_rows_vcp_215_are_exhaustive() {
        assert_source_rows(
            &VCP_215,
            r#"
500|11460|31410|SZCS|SURV|S1:28|none|-
500|17108|21040|SZCD|DOP|-|selectable|2:a12533,3:a13393,4:a14468,5:a15836,6:p64*,7:a18455,8:a20032
900|13375|26920|SZCS|SURV|S1:24|none|-
900|17108|21040|SZCD|DOP|-|selectable|2:a12533,3:a13393,4:a14468,5:a15836,6:p64*,7:a18455,8:a20032
1300|15921|23540|SZCS|SURV|S1:22|none|-
1300|17108|21040|SZCD|DOP|-|selectable|2:a12533,3:a13393,4:a14468,5:a15836,6:p64*,7:a18455,8:a20032
1800|16771|21470|B|ALL|S3:3|selectable|2:p40,3:p42,4:p46,5:p50*,6:p54,7:p58,8:p63
2400|20650|17430|B|ALL|S4:3|selectable|2:p32,3:p34,4:p37,5:p40*,6:p43,7:p47,8:p51
3100|19536|18430|B|ALL|S5:5|selectable|2:p32,3:p34,4:p37,5:p40*,6:p43,7:p47,8:p51
4000|20232|17790|B|ALL|S6:5|selectable|2:p32,3:p34,4:p37,5:p40*,6:p44,7:p47,8:p51
5100|20232|17790|B|ALL|S6:5|selectable|2:p32,3:p34,4:p37,5:p40*,6:p44,7:p47,8:p51
6400|20232|17790|B|ALL|S6:5|selectable|2:p32,3:p34,4:p37,5:p40*,6:p44,7:p47,8:p51
8000|24864|14480|CD/WO|ALL|-|selectable|2:p32,3:p34,4:p37,5:p41,6:p44*,7:p47,8:p52
10000|25640|14040|CD/WO|ALL|-|selectable|2:p31,3:p33,4:p36,5:p40,6:p43,7:p46,8:p50*
12000|25640|14040|CD/WO|ALL|-|selectable|2:p31,3:p33,4:p36,5:p40,6:p43,7:p46,8:p50*
14000|25640|14040|CD/WO|ALL|-|selectable|2:p31,3:p33,4:p36,5:p40,6:p43,7:p46,8:p50*
16700|25640|14040|CD/WO|ALL|-|selectable|2:p31,3:p33,4:p36,5:p40,6:p43,7:p46,8:p50*
19500|25640|14040|CD/WO|ALL|-|selectable|2:p31,3:p33,4:p36,5:p40,6:p43,7:p46,8:p50*"#,
        );
    }

    #[test]
    fn source_rows_vcp_35_are_exhaustive() {
        assert_source_rows(
            &VCP_35,
            r#"
500|4966|72490|SZCS|SURV|S1:64|none|-
500|15836|22730|SZCD|DOP|-|selectable|2:a12533,3:a13393,4:a14468,5:p64*,6:a17108,7:a18455,8:a20032
900|4966|72490|SZCS|SURV|S1:64|none|-
900|15836|22730|SZCD|DOP|-|selectable|2:a12533,3:a13393,4:a14468,5:p64*,6:a17108,7:a18455,8:a20032
1300|5473|65780|SZCS|SURV|S2:64|none|-
1300|15836|22730|SZCD|DOP|-|fixed|5:p64*
1800|15489|23240|B|ALL|S3:3|selectable|2:p44,3:p47,4:p50,5:p55*,6:p59,7:p64,8:p70
2400|17756|20270|B|ALL|S4:3|selectable|2:p38,3:p41,4:p44,5:p48*,6:p52,7:p56,8:p61
3100|16926|21270|B|ALL|S5:5|selectable|2:p38,3:p41,4:p44,5:p48*,6:p52,7:p56,8:p61
4000|18068|19920|B|ALL|S6:5|selectable|2:p36,3:p39,4:p42,5:p46*,6:p50,7:p54,8:p58
5100|18068|19920|B|ALL|S6:5|selectable|2:p36,3:p39,4:p42,5:p46*,6:p50,7:p54,8:p58
6400|18068|19920|B|ALL|S6:5|selectable|2:p36,3:p39,4:p42,5:p46*,6:p50,7:p54,8:p58"#,
        );
    }

    #[test]
    fn source_rows_vcp_112_are_exhaustive() {
        assert_source_rows(
            &VCP_112,
            r#"
500|18677|19290|SZCS|SURV|S1:17|none|-
500|20032|17970|SZCD|DOP|-|fixed|8:p64*
500|14468|24880|SZCD|DOP|-|fixed|4:p64*
900|19842|18140|SZCS|SURV|S1:16|none|-
900|20032|17970|SZCD|DOP|-|fixed|8:p64*
900|15836|22730|SZCD|DOP|-|fixed|5:p64*
1300|21556|17510|SZCS|SURV|S2:16|none|-
1300|20032|17970|SZCD|DOP|-|fixed|8:p64*
1300|17108|21040|SZCD|DOP|-|fixed|6:p64*
1800|26385|13640|B|ALL|S3:3|selectable|2:p21,3:p23,4:p26,5:p28*,6:p30,7:p32,8:p35
2400|27332|13170|B|ALL|S4:3|selectable|2:p22,3:p24,4:p26,5:p28*,6:p31,7:p33,8:p36
3100|28227|12750|B|ALL|S5:3|selectable|2:p22,3:p24,4:p26,5:p28*,6:p31,7:p33,8:p36
4000|26400|13670|B|ALL|S6:3|selectable|2:p23,3:p25,4:p27,5:p30*,6:p32,7:p35,8:p46
5100|26000|13640|B|ALL|S6:3|selectable|2:p24,3:p26,4:p28,5:p31*,6:p33,7:p36,8:p59
6400|26400|13640|B|ALL|S6:3|selectable|2:p24,3:p26,4:p28,5:p31*,6:p33,7:p36,8:p44
8000|28418|12680|CD/WO|ALL|-|selectable|2:p28,3:p30,4:p32,5:p35,6:p38*,7:p41,8:p44
10000|28413|12670|CD/WO|ALL|-|selectable|2:p28,3:p30,4:p32,5:p35,6:p38,7:p41*,8:p44
12500|28740|12670|CD/WO|ALL|-|selectable|2:p27,3:p29,4:p32,5:p35,6:p38,7:p41,8:p44*
15600|28740|12670|CD/WO|ALL|-|selectable|2:p27,3:p29,4:p32,5:p35,6:p38,7:p41,8:p44*
19500|28740|12670|CD/WO|ALL|-|selectable|2:p27,3:p29,4:p32,5:p35,6:p38,7:p41,8:p44*"#,
        );
    }

    #[test]
    fn source_rows_vcp_34_are_exhaustive() {
        assert_source_rows(
            &VCP_34,
            r#"
500|5043|71390|CS|SURV|S1:63|none|-
500|8491|42400|CD/W|DOP|-|fixed|1:p52*
900|5043|71390|CS|SURV|S1:63|none|-
900|8491|42400|CD/W|DOP|-|fixed|1:p52*
1300|5445|66120|CS|SURV|S2:63|none|-
1300|8491|42400|CD/W|DOP|-|fixed|1:p52*
1800|5880|61220|B|ALL|S3:11|fixed|1:p63*
2400|8491|42400|CD/WO|ALL|-|fixed|1:p52*
3100|8491|42400|CD/WO|ALL|-|fixed|1:p52*
4500|8491|42400|CD/WO|ALL|-|fixed|1:p52*"#,
        );
    }

    #[test]
    fn physical_rows_keep_split_cuts_and_unique_ladders_do_not() {
        assert_eq!(VCP_12.rows.len(), 17);
        assert_eq!(VCP_34.rows.len(), 10);
        assert_eq!(VCP_35.rows.len(), 12);
        assert_eq!(VCP_112.rows.len(), 20);
        assert_eq!(VCP_212.rows.len(), 17);
        assert_eq!(VCP_215.rows.len(), 18);
        assert_eq!(VCP_112.elevation_ladder_deg().len(), 14);
        assert_eq!(VCP_215.elevation_ladder_deg().len(), 15);
    }

    #[test]
    fn sz2_cells_are_typed_and_never_misrepresented_as_prf_hz() {
        let row = &VCP_212.rows[1];
        assert_eq!(
            row.doppler_prfs[0].value,
            DopplerPrfValue::AzimuthRateDegPerSecond(12.533)
        );
        assert_eq!(row.doppler_prfs[4].value, DopplerPrfValue::PulseCount(64));
        assert!(row.doppler_prfs[4].is_default);
    }

    #[test]
    fn strategy_identity_requires_explicit_legacy_or_custom_classification() {
        assert_eq!(
            ScanStrategy::Build24(Build24Vcp::Vcp212),
            ScanStrategy::Build24(Build24Vcp::try_from(212).unwrap())
        );
        assert!(Build24Vcp::try_from(21).is_err());
        assert_ne!(
            ScanStrategy::LegacyVcp { number: 21 },
            ScanStrategy::Custom {
                name: "research ladder".to_string()
            }
        );
    }
}
