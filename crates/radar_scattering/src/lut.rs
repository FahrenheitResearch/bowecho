use std::collections::{BTreeMap, HashSet};
use std::mem::size_of;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    AdditiveScattering, KernelModel, OutputError, ScienceError, ScienceMetadata, Sha256Digest,
    TMatrixImplementation,
};

pub const LUT_MAGIC: [u8; 8] = *b"BRSLUT01";
pub const LUT_SCHEMA_VERSION: u16 = 1;

const PREFIX_BYTES: usize = 8 + 2 + 4;
const MAX_HEADER_BYTES: usize = 16 * 1024 * 1024;
const MAX_AXES: usize = 16;

/// Fixed number of axis slots carried by [`PreparedInterpolationPlan`].
///
/// Only the prefix reported by
/// [`PreparedInterpolationPlan::active_axis_count`] is active. Remaining
/// offsets and fractions are deterministically zero-filled so plans can be
/// staged as fixed-size host records for a batched accelerator backend.
pub const PREPARED_INTERPOLATION_AXIS_SLOTS: usize = MAX_AXES;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AxisKind {
    EquivolumeDiameter,
    Temperature,
    BulkDensity,
    CondensedVolumeFraction,
    LiquidMassFraction,
    MinorToMajorAxisRatio,
    Frequency,
    RadarElevation,
    CantingAngle,
    RimeMassFraction,
    RimeDensity,
    TimeOffset,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    Meter,
    Kelvin,
    KilogramPerCubicMeter,
    UnitlessFraction,
    Hertz,
    Degree,
    Second,
    LinearReflectivityMillimeter6PerMeter3,
    LinearCovarianceMillimeter6PerMeter3,
    DegreePerKilometer,
    DecibelPerKilometer,
    ReflectivityWeightedMeterPerSecond,
    ReflectivityWeightedMeter2PerSecond2,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Axis {
    kind: AxisKind,
    unit: Unit,
    coordinates: Vec<f64>,
}

impl Axis {
    pub fn new(kind: AxisKind, unit: Unit, coordinates: Vec<f64>) -> Result<Self, LutError> {
        let axis = Self {
            kind,
            unit,
            coordinates,
        };
        axis.validate(0)?;
        Ok(axis)
    }

    #[must_use]
    pub const fn kind(&self) -> AxisKind {
        self.kind
    }

    #[must_use]
    pub const fn unit(&self) -> Unit {
        self.unit
    }

    #[must_use]
    pub fn coordinates(&self) -> &[f64] {
        &self.coordinates
    }

    fn validate(&self, index: usize) -> Result<(), LutError> {
        let expected = axis_unit(self.kind);
        if self.unit != expected {
            return Err(LutError::AxisUnit {
                index,
                kind: self.kind,
                expected,
                actual: self.unit,
            });
        }
        if self.coordinates.is_empty() {
            return Err(LutError::EmptyAxis { index });
        }
        for (coordinate_index, value) in self.coordinates.iter().copied().enumerate() {
            if !value.is_finite() {
                return Err(LutError::NonFiniteAxisCoordinate {
                    axis: index,
                    coordinate: coordinate_index,
                    value,
                });
            }
            if coordinate_index > 0 && self.coordinates[coordinate_index - 1] >= value {
                return Err(LutError::NonIncreasingAxis {
                    axis: index,
                    lower: self.coordinates[coordinate_index - 1],
                    upper: value,
                });
            }
        }
        Ok(())
    }

    fn locate(&self, value: f64) -> Result<Bracket, InterpolationError> {
        let first = self.coordinates[0];
        let last = self.coordinates[self.coordinates.len() - 1];
        if value < first || value > last {
            return Err(InterpolationError::OutsideAxis {
                kind: self.kind,
                value,
                minimum: first,
                maximum: last,
            });
        }
        if self.coordinates.len() == 1 || value == first {
            return Ok(Bracket {
                lower: 0,
                upper: 0,
                fraction: 0.0,
            });
        }
        if value == last {
            let index = self.coordinates.len() - 1;
            return Ok(Bracket {
                lower: index,
                upper: index,
                fraction: 0.0,
            });
        }

        let upper = self
            .coordinates
            .partition_point(|candidate| *candidate < value);
        if self.coordinates[upper] == value {
            return Ok(Bracket {
                lower: upper,
                upper,
                fraction: 0.0,
            });
        }
        let lower = upper - 1;
        let fraction =
            (value - self.coordinates[lower]) / (self.coordinates[upper] - self.coordinates[lower]);
        Ok(Bracket {
            lower,
            upper,
            fraction,
        })
    }
}

fn axis_unit(kind: AxisKind) -> Unit {
    match kind {
        AxisKind::EquivolumeDiameter => Unit::Meter,
        AxisKind::Temperature => Unit::Kelvin,
        AxisKind::BulkDensity | AxisKind::RimeDensity => Unit::KilogramPerCubicMeter,
        AxisKind::LiquidMassFraction
        | AxisKind::CondensedVolumeFraction
        | AxisKind::MinorToMajorAxisRatio
        | AxisKind::RimeMassFraction => Unit::UnitlessFraction,
        AxisKind::Frequency => Unit::Hertz,
        AxisKind::RadarElevation | AxisKind::CantingAngle => Unit::Degree,
        AxisKind::TimeOffset => Unit::Second,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputKind {
    Zh,
    Zv,
    HhVvCovarianceReal,
    HhVvCovarianceImaginary,
    Kdp,
    Ah,
    Av,
    FallSpeedFirstMoment,
    FallSpeedSecondMoment,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputDescriptor {
    kind: OutputKind,
    unit: Unit,
}

impl OutputDescriptor {
    #[must_use]
    pub const fn kind(self) -> OutputKind {
        self.kind
    }

    #[must_use]
    pub const fn unit(self) -> Unit {
        self.unit
    }
}

fn canonical_outputs() -> Vec<OutputDescriptor> {
    use OutputKind::{
        Ah, Av, FallSpeedFirstMoment, FallSpeedSecondMoment, HhVvCovarianceImaginary,
        HhVvCovarianceReal, Kdp, Zh, Zv,
    };
    vec![
        OutputDescriptor {
            kind: Zh,
            unit: Unit::LinearReflectivityMillimeter6PerMeter3,
        },
        OutputDescriptor {
            kind: Zv,
            unit: Unit::LinearReflectivityMillimeter6PerMeter3,
        },
        OutputDescriptor {
            kind: HhVvCovarianceReal,
            unit: Unit::LinearCovarianceMillimeter6PerMeter3,
        },
        OutputDescriptor {
            kind: HhVvCovarianceImaginary,
            unit: Unit::LinearCovarianceMillimeter6PerMeter3,
        },
        OutputDescriptor {
            kind: Kdp,
            unit: Unit::DegreePerKilometer,
        },
        OutputDescriptor {
            kind: Ah,
            unit: Unit::DecibelPerKilometer,
        },
        OutputDescriptor {
            kind: Av,
            unit: Unit::DecibelPerKilometer,
        },
        OutputDescriptor {
            kind: FallSpeedFirstMoment,
            unit: Unit::ReflectivityWeightedMeterPerSecond,
        },
        OutputDescriptor {
            kind: FallSpeedSecondMoment,
            unit: Unit::ReflectivityWeightedMeter2PerSecond2,
        },
    ]
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PayloadEncoding {
    /// f64 little endian, one nine-component output per grid point, with the
    /// last declared axis varying fastest.
    F64LePointMajorLastAxisFastest,
}

/// Auditable generator identity. Package names are canonical lowercase keys.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeneratorMetadata {
    name: String,
    version: String,
    executable: String,
    source_revision: String,
    python_version: Option<String>,
    package_versions: BTreeMap<String, String>,
}

impl GeneratorMetadata {
    pub fn new(
        name: impl Into<String>,
        version: impl Into<String>,
        executable: impl Into<String>,
        source_revision: impl Into<String>,
        python_version: Option<String>,
        package_versions: BTreeMap<String, String>,
    ) -> Result<Self, LutError> {
        let metadata = Self {
            name: name.into(),
            version: version.into(),
            executable: executable.into(),
            source_revision: source_revision.into(),
            python_version,
            package_versions,
        };
        metadata.validate()?;
        Ok(metadata)
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    #[must_use]
    pub fn executable(&self) -> &str {
        &self.executable
    }

    #[must_use]
    pub fn source_revision(&self) -> &str {
        &self.source_revision
    }

    #[must_use]
    pub fn python_version(&self) -> Option<&str> {
        self.python_version.as_deref()
    }

    #[must_use]
    pub const fn package_versions(&self) -> &BTreeMap<String, String> {
        &self.package_versions
    }

    fn validate(&self) -> Result<(), LutError> {
        for (field, value) in [
            ("generator name", self.name.as_str()),
            ("generator version", self.version.as_str()),
            ("generator executable", self.executable.as_str()),
            ("generator source revision", self.source_revision.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(LutError::EmptyMetadata { field });
            }
        }
        if self
            .python_version
            .as_deref()
            .is_some_and(|version| version.trim().is_empty())
        {
            return Err(LutError::EmptyMetadata {
                field: "generator Python version",
            });
        }
        for (package, version) in &self.package_versions {
            if package.trim() != package
                || package.is_empty()
                || package.bytes().any(|byte| byte.is_ascii_uppercase())
            {
                return Err(LutError::NonCanonicalPackageName {
                    package: package.clone(),
                });
            }
            if version.trim().is_empty() {
                return Err(LutError::EmptyPackageVersion {
                    package: package.clone(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LutHeader {
    magic: String,
    schema_version: u16,
    axes: Vec<Axis>,
    outputs: Vec<OutputDescriptor>,
    generator: GeneratorMetadata,
    generator_config_utf8: String,
    config_sha256: Sha256Digest,
    science: ScienceMetadata,
    payload_encoding: PayloadEncoding,
    grid_point_count: u64,
    payload_byte_length: u64,
    payload_sha256: Sha256Digest,
}

impl LutHeader {
    #[must_use]
    pub fn magic(&self) -> &str {
        &self.magic
    }

    #[must_use]
    pub const fn schema_version(&self) -> u16 {
        self.schema_version
    }

    #[must_use]
    pub fn axes(&self) -> &[Axis] {
        &self.axes
    }

    #[must_use]
    pub fn outputs(&self) -> &[OutputDescriptor] {
        &self.outputs
    }

    #[must_use]
    pub const fn generator(&self) -> &GeneratorMetadata {
        &self.generator
    }

    #[must_use]
    pub fn generator_config_utf8(&self) -> &str {
        &self.generator_config_utf8
    }

    #[must_use]
    pub const fn config_sha256(&self) -> Sha256Digest {
        self.config_sha256
    }

    #[must_use]
    pub const fn science(&self) -> &ScienceMetadata {
        &self.science
    }

    #[must_use]
    pub const fn payload_encoding(&self) -> PayloadEncoding {
        self.payload_encoding
    }

    #[must_use]
    pub const fn grid_point_count(&self) -> u64 {
        self.grid_point_count
    }

    #[must_use]
    pub const fn payload_byte_length(&self) -> u64 {
        self.payload_byte_length
    }

    #[must_use]
    pub const fn payload_sha256(&self) -> Sha256Digest {
        self.payload_sha256
    }

    fn validate(&self) -> Result<usize, LutError> {
        if self.magic.as_bytes() != &LUT_MAGIC[..] {
            return Err(LutError::HeaderMagic {
                actual: self.magic.clone(),
            });
        }
        if self.schema_version != LUT_SCHEMA_VERSION {
            return Err(LutError::UnsupportedSchema {
                actual: self.schema_version,
            });
        }
        if self.axes.is_empty() || self.axes.len() > MAX_AXES {
            return Err(LutError::AxisCount {
                actual: self.axes.len(),
                maximum: MAX_AXES,
            });
        }
        let mut kinds = HashSet::new();
        for (index, axis) in self.axes.iter().enumerate() {
            axis.validate(index)?;
            if !kinds.insert(axis.kind) {
                return Err(LutError::DuplicateAxis { kind: axis.kind });
            }
        }
        if self.outputs != canonical_outputs() {
            return Err(LutError::NonCanonicalOutputs);
        }
        self.generator.validate()?;
        self.science.validate_additive_lut_compatibility()?;
        validate_generator_science(&self.generator, &self.science)?;
        let config_value: serde_json::Value = serde_json::from_str(&self.generator_config_utf8)
            .map_err(LutError::GeneratorConfigJson)?;
        if !config_value.is_object() {
            return Err(LutError::GeneratorConfigNotObject);
        }
        let actual_config_digest = Sha256Digest::compute(self.generator_config_utf8.as_bytes());
        if self.config_sha256 != actual_config_digest {
            return Err(LutError::ConfigDigestMismatch {
                expected: self.config_sha256,
                actual: actual_config_digest,
            });
        }

        let points = grid_point_count(&self.axes)?;
        if self.grid_point_count != points as u64 {
            return Err(LutError::GridPointCount {
                header: self.grid_point_count,
                axes: points as u64,
            });
        }
        let expected_payload = points
            .checked_mul(AdditiveScattering::COMPONENT_COUNT)
            .and_then(|values| values.checked_mul(size_of::<f64>()))
            .ok_or(LutError::DimensionOverflow)?;
        if self.payload_byte_length != expected_payload as u64 {
            return Err(LutError::HeaderPayloadLength {
                header: self.payload_byte_length,
                expected: expected_payload as u64,
            });
        }
        Ok(points)
    }
}

fn validate_generator_science(
    generator: &GeneratorMetadata,
    science: &ScienceMetadata,
) -> Result<(), LutError> {
    if matches!(
        science.kernel(),
        KernelModel::TMatrix {
            implementation: TMatrixImplementation::PyTMatrix033
        }
    ) {
        match generator.package_versions.get("pytmatrix") {
            Some(version) if version == "0.3.3" => {}
            actual => {
                return Err(LutError::PyTMatrixVersion {
                    actual: actual.cloned(),
                });
            }
        }
    }
    Ok(())
}

/// A typed query coordinate; LUT queries must supply these in header axis order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AxisCoordinate {
    kind: AxisKind,
    value: f64,
}

impl AxisCoordinate {
    pub fn new(kind: AxisKind, value: f64) -> Result<Self, InterpolationError> {
        if !value.is_finite() {
            return Err(InterpolationError::NonFiniteCoordinate { kind, value });
        }
        Ok(Self { kind, value })
    }

    #[must_use]
    pub const fn kind(self) -> AxisKind {
        self.kind
    }

    #[must_use]
    pub const fn value(self) -> f64 {
        self.value
    }
}

#[derive(Clone, Copy, Debug)]
struct Bracket {
    lower: usize,
    upper: usize,
    fraction: f64,
}

/// A validated, fixed-size multilinear-interpolation record prepared on the
/// CPU.
///
/// `base_point_index` is the all-lower-corner point in the LUT's
/// last-axis-fastest payload. The active prefixes of `upper_point_offsets` and
/// `upper_fractions` contain one entry per non-degenerate bracket, compacted in
/// the LUT header's exact axis order. Corner bit `n` selects the upper point of
/// active entry `n`. Inactive array tails are zero-filled.
///
/// The fixed-width integer fields, fixed arrays, and C field order make this a
/// stable host-side staging layout. Serde serialization remains the portable
/// representation; consumers must not infer native endianness from `repr(C)`.
/// A plan is tied to the [`OfflineLut`] whose axes prepared it.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedInterpolationPlan {
    base_point_index: u64,
    upper_point_offsets: [u64; PREPARED_INTERPOLATION_AXIS_SLOTS],
    upper_fractions: [f64; PREPARED_INTERPOLATION_AXIS_SLOTS],
    corner_count: u32,
    active_axis_count: u32,
}

impl PreparedInterpolationPlan {
    /// All-lower-corner index in the LUT's point-major payload.
    #[must_use]
    pub const fn base_point_index(&self) -> u64 {
        self.base_point_index
    }

    /// Number of active entries in the offset and fraction arrays.
    #[must_use]
    pub const fn active_axis_count(&self) -> u32 {
        self.active_axis_count
    }

    /// Fixed host array of upper-corner point offsets.
    ///
    /// Only `[..active_axis_count()]` is active; the tail is zero-filled.
    #[must_use]
    pub const fn upper_point_offsets(&self) -> &[u64; PREPARED_INTERPOLATION_AXIS_SLOTS] {
        &self.upper_point_offsets
    }

    /// Fixed host array of upper interpolation fractions.
    ///
    /// Entries correspond one-for-one with [`Self::upper_point_offsets`] in
    /// exact LUT axis order. Only `[..active_axis_count()]` is active.
    #[must_use]
    pub const fn upper_fractions(&self) -> &[f64; PREPARED_INTERPOLATION_AXIS_SLOTS] {
        &self.upper_fractions
    }

    /// Number of corners evaluated by this plan (`2^active_axis_count`).
    #[must_use]
    pub const fn corner_count(&self) -> u32 {
        self.corner_count
    }
}

/// Validated, immutable additive-scattering lookup table.
#[derive(Clone, Debug, PartialEq)]
pub struct OfflineLut {
    header: LutHeader,
    values: Vec<AdditiveScattering>,
}

impl OfflineLut {
    pub fn new(
        axes: Vec<Axis>,
        generator: GeneratorMetadata,
        generator_config_utf8: impl Into<String>,
        science: ScienceMetadata,
        values: Vec<AdditiveScattering>,
    ) -> Result<Self, LutError> {
        if axes.is_empty() || axes.len() > MAX_AXES {
            return Err(LutError::AxisCount {
                actual: axes.len(),
                maximum: MAX_AXES,
            });
        }
        let mut kinds = HashSet::new();
        for (index, axis) in axes.iter().enumerate() {
            axis.validate(index)?;
            if !kinds.insert(axis.kind) {
                return Err(LutError::DuplicateAxis { kind: axis.kind });
            }
        }
        let points = grid_point_count(&axes)?;
        if points != values.len() {
            return Err(LutError::ValueCount {
                expected: points,
                actual: values.len(),
            });
        }
        let generator_config_utf8 = generator_config_utf8.into();
        let payload = encode_payload(&values)?;
        let header = LutHeader {
            magic: String::from_utf8(LUT_MAGIC.to_vec()).expect("LUT magic is ASCII"),
            schema_version: LUT_SCHEMA_VERSION,
            axes,
            outputs: canonical_outputs(),
            generator,
            config_sha256: Sha256Digest::compute(generator_config_utf8.as_bytes()),
            generator_config_utf8,
            science,
            payload_encoding: PayloadEncoding::F64LePointMajorLastAxisFastest,
            grid_point_count: points as u64,
            payload_byte_length: payload.len() as u64,
            payload_sha256: Sha256Digest::compute(&payload),
        };
        header.validate()?;
        Ok(Self { header, values })
    }

    #[must_use]
    pub const fn header(&self) -> &LutHeader {
        &self.header
    }

    #[must_use]
    pub fn values(&self) -> &[AdditiveScattering] {
        &self.values
    }

    /// Encode deterministically. Header JSON comes from fixed struct field
    /// order and generator package versions are held in a BTreeMap.
    pub fn to_bytes(&self) -> Result<Vec<u8>, LutError> {
        let points = self.header.validate()?;
        if points != self.values.len() {
            return Err(LutError::ValueCount {
                expected: points,
                actual: self.values.len(),
            });
        }
        let payload = encode_payload(&self.values)?;
        let actual_digest = Sha256Digest::compute(&payload);
        if self.header.payload_sha256 != actual_digest {
            return Err(LutError::PayloadDigestMismatch {
                expected: self.header.payload_sha256,
                actual: actual_digest,
            });
        }
        assemble_file(&self.header, &payload)
    }

    /// Decode and validate magic, schema, all metadata, embedded config hash,
    /// payload length/hash, and every physical additive-output invariant.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, LutError> {
        if bytes.len() < PREFIX_BYTES {
            return Err(LutError::TruncatedPrefix {
                actual: bytes.len(),
            });
        }
        let actual_magic: [u8; 8] = bytes[0..8].try_into().expect("slice length checked");
        if actual_magic != LUT_MAGIC {
            return Err(LutError::FileMagic {
                actual: actual_magic,
            });
        }
        let schema = u16::from_le_bytes(bytes[8..10].try_into().expect("slice length checked"));
        if schema != LUT_SCHEMA_VERSION {
            return Err(LutError::UnsupportedSchema { actual: schema });
        }
        let header_length =
            u32::from_le_bytes(bytes[10..14].try_into().expect("slice length checked")) as usize;
        if header_length == 0 || header_length > MAX_HEADER_BYTES {
            return Err(LutError::HeaderSize {
                actual: header_length,
            });
        }
        let header_end = PREFIX_BYTES
            .checked_add(header_length)
            .ok_or(LutError::DimensionOverflow)?;
        if header_end > bytes.len() {
            return Err(LutError::TruncatedHeader {
                declared: header_length,
                available: bytes.len() - PREFIX_BYTES,
            });
        }
        let header: LutHeader = serde_json::from_slice(&bytes[PREFIX_BYTES..header_end])
            .map_err(LutError::HeaderJson)?;
        if header.schema_version != schema {
            return Err(LutError::SchemaDisagreement {
                prefix: schema,
                header: header.schema_version,
            });
        }
        let points = header.validate()?;
        let payload = &bytes[header_end..];
        if payload.len() as u64 != header.payload_byte_length {
            return Err(LutError::PayloadLength {
                header: header.payload_byte_length,
                actual: payload.len() as u64,
            });
        }
        let actual_digest = Sha256Digest::compute(payload);
        if actual_digest != header.payload_sha256 {
            return Err(LutError::PayloadDigestMismatch {
                expected: header.payload_sha256,
                actual: actual_digest,
            });
        }
        let values = decode_payload(payload, points)?;
        Ok(Self { header, values })
    }

    /// Verify an externally supplied exact generator-config byte sequence.
    pub fn verify_generator_config(&self, exact_utf8: &[u8]) -> Result<(), LutError> {
        let actual = Sha256Digest::compute(exact_utf8);
        if actual == self.header.config_sha256 {
            Ok(())
        } else {
            Err(LutError::ExternalConfigDigestMismatch {
                expected: self.header.config_sha256,
                actual,
            })
        }
    }

    /// Validate and pre-bracket a query for deterministic multilinear
    /// interpolation with no extrapolation.
    ///
    /// Bracketing and typed-axis validation happen entirely on the CPU. The
    /// returned fixed-size plan can be copied into a larger batch without
    /// carrying query-axis metadata into an accelerator kernel.
    pub fn prepare_interpolation(
        &self,
        coordinates: &[AxisCoordinate],
    ) -> Result<PreparedInterpolationPlan, InterpolationError> {
        if coordinates.len() != self.header.axes.len() {
            return Err(InterpolationError::CoordinateCount {
                expected: self.header.axes.len(),
                actual: coordinates.len(),
            });
        }
        let strides = last_axis_fastest_strides(&self.header.axes)?;
        let mut base_point_index = 0_usize;
        let mut upper_point_offsets = [0_u64; PREPARED_INTERPOLATION_AXIS_SLOTS];
        let mut upper_fractions = [0.0_f64; PREPARED_INTERPOLATION_AXIS_SLOTS];
        let mut active_axis_count = 0_usize;
        for (index, (axis, coordinate)) in
            self.header.axes.iter().zip(coordinates.iter()).enumerate()
        {
            if axis.kind != coordinate.kind {
                return Err(InterpolationError::AxisOrder {
                    index,
                    expected: axis.kind,
                    actual: coordinate.kind,
                });
            }
            if !coordinate.value.is_finite() {
                return Err(InterpolationError::NonFiniteCoordinate {
                    kind: coordinate.kind,
                    value: coordinate.value,
                });
            }
            let bracket = axis.locate(coordinate.value)?;
            base_point_index = base_point_index
                .checked_add(
                    bracket
                        .lower
                        .checked_mul(strides[index])
                        .ok_or(InterpolationError::DimensionOverflow)?,
                )
                .ok_or(InterpolationError::DimensionOverflow)?;
            if bracket.lower != bracket.upper {
                let upper_delta = bracket
                    .upper
                    .checked_sub(bracket.lower)
                    .and_then(|delta| delta.checked_mul(strides[index]))
                    .ok_or(InterpolationError::DimensionOverflow)?;
                upper_point_offsets[active_axis_count] = u64::try_from(upper_delta)
                    .map_err(|_| InterpolationError::DimensionOverflow)?;
                upper_fractions[active_axis_count] = bracket.fraction;
                active_axis_count += 1;
            }
        }
        let active_axis_count =
            u32::try_from(active_axis_count).map_err(|_| InterpolationError::DimensionOverflow)?;
        let corner_count = 1_u32
            .checked_shl(active_axis_count)
            .ok_or(InterpolationError::DimensionOverflow)?;
        Ok(PreparedInterpolationPlan {
            base_point_index: u64::try_from(base_point_index)
                .map_err(|_| InterpolationError::DimensionOverflow)?,
            upper_point_offsets,
            upper_fractions,
            corner_count,
            active_axis_count,
        })
    }

    /// Execute a plan returned by [`Self::prepare_interpolation`].
    ///
    /// Corner masks, active axes, weight multiplication, and component
    /// accumulation retain the same ascending order as direct interpolation.
    /// The plan must have been prepared for this table.
    pub fn interpolate_prepared(
        &self,
        plan: &PreparedInterpolationPlan,
    ) -> Result<AdditiveScattering, InterpolationError> {
        let active_axis_count = usize::try_from(plan.active_axis_count)
            .map_err(|_| InterpolationError::DimensionOverflow)?;
        if active_axis_count > PREPARED_INTERPOLATION_AXIS_SLOTS {
            return Err(InterpolationError::InvalidPreparedPlan {
                reason: "active axis count exceeds the fixed host layout",
            });
        }
        let expected_corner_count = 1_u32
            .checked_shl(plan.active_axis_count)
            .ok_or(InterpolationError::DimensionOverflow)?;
        if plan.corner_count != expected_corner_count {
            return Err(InterpolationError::InvalidPreparedPlan {
                reason: "corner count does not equal 2^active_axis_count",
            });
        }
        let base_point_index = usize::try_from(plan.base_point_index)
            .map_err(|_| InterpolationError::DimensionOverflow)?;
        let mut maximum_point_index = base_point_index;
        for active_axis in 0..active_axis_count {
            let fraction = plan.upper_fractions[active_axis];
            if !fraction.is_finite() || !(0.0..=1.0).contains(&fraction) {
                return Err(InterpolationError::InvalidPreparedPlan {
                    reason: "active upper fraction is not finite and within [0, 1]",
                });
            }
            let offset = usize::try_from(plan.upper_point_offsets[active_axis])
                .map_err(|_| InterpolationError::DimensionOverflow)?;
            if offset == 0 {
                return Err(InterpolationError::InvalidPreparedPlan {
                    reason: "active upper point offset is zero",
                });
            }
            maximum_point_index = maximum_point_index
                .checked_add(offset)
                .ok_or(InterpolationError::DimensionOverflow)?;
        }
        if maximum_point_index >= self.values.len() {
            return Err(InterpolationError::InvalidPreparedPlan {
                reason: "prepared point index is outside the LUT payload",
            });
        }

        let mut accumulated = [0.0; AdditiveScattering::COMPONENT_COUNT];

        // Ascending corner masks and fixed axis order make evaluation stable
        // and reproducible across calls/platforms using IEEE-754 f64.
        for corner in 0..plan.corner_count {
            let mut point_index = base_point_index;
            let mut weight = 1.0;
            for active_axis in 0..active_axis_count {
                let upper = ((corner >> active_axis) & 1) == 1;
                let fraction = plan.upper_fractions[active_axis];
                if upper {
                    weight *= fraction;
                    point_index = point_index
                        .checked_add(
                            usize::try_from(plan.upper_point_offsets[active_axis])
                                .map_err(|_| InterpolationError::DimensionOverflow)?,
                        )
                        .ok_or(InterpolationError::DimensionOverflow)?;
                } else {
                    weight *= 1.0 - fraction;
                }
            }
            let components = self
                .values
                .get(point_index)
                .ok_or(InterpolationError::DimensionOverflow)?
                .components();
            for component in 0..AdditiveScattering::COMPONENT_COUNT {
                accumulated[component] += weight * components[component];
            }
        }

        AdditiveScattering::from_components(accumulated)
            .map_err(InterpolationError::InvalidInterpolatedOutput)
    }

    /// Deterministic multilinear interpolation with no extrapolation.
    pub fn interpolate(
        &self,
        coordinates: &[AxisCoordinate],
    ) -> Result<AdditiveScattering, InterpolationError> {
        let plan = self.prepare_interpolation(coordinates)?;
        self.interpolate_prepared(&plan)
    }
}

fn grid_point_count(axes: &[Axis]) -> Result<usize, LutError> {
    axes.iter().try_fold(1_usize, |count, axis| {
        count
            .checked_mul(axis.coordinates.len())
            .ok_or(LutError::DimensionOverflow)
    })
}

fn last_axis_fastest_strides(
    axes: &[Axis],
) -> Result<[usize; PREPARED_INTERPOLATION_AXIS_SLOTS], InterpolationError> {
    let mut strides = [1_usize; PREPARED_INTERPOLATION_AXIS_SLOTS];
    for index in (0..axes.len().saturating_sub(1)).rev() {
        strides[index] = strides[index + 1]
            .checked_mul(axes[index + 1].coordinates.len())
            .ok_or(InterpolationError::DimensionOverflow)?;
    }
    Ok(strides)
}

fn encode_payload(values: &[AdditiveScattering]) -> Result<Vec<u8>, LutError> {
    let capacity = values
        .len()
        .checked_mul(AdditiveScattering::COMPONENT_COUNT)
        .and_then(|components| components.checked_mul(size_of::<f64>()))
        .ok_or(LutError::DimensionOverflow)?;
    let mut payload = Vec::with_capacity(capacity);
    for value in values {
        for component in value.components() {
            payload.extend_from_slice(&component.to_le_bytes());
        }
    }
    Ok(payload)
}

fn decode_payload(payload: &[u8], points: usize) -> Result<Vec<AdditiveScattering>, LutError> {
    let point_bytes = AdditiveScattering::COMPONENT_COUNT * size_of::<f64>();
    let mut values = Vec::with_capacity(points);
    for (point, bytes) in payload.chunks_exact(point_bytes).enumerate() {
        let mut components = [0.0; AdditiveScattering::COMPONENT_COUNT];
        for (index, component) in components.iter_mut().enumerate() {
            let start = index * size_of::<f64>();
            *component = f64::from_le_bytes(
                bytes[start..start + size_of::<f64>()]
                    .try_into()
                    .expect("fixed component width"),
            );
        }
        values.push(
            AdditiveScattering::from_components(components)
                .map_err(|source| LutError::InvalidOutput { point, source })?,
        );
    }
    if values.len() != points || !payload.chunks_exact(point_bytes).remainder().is_empty() {
        return Err(LutError::PayloadLength {
            header: (points * point_bytes) as u64,
            actual: payload.len() as u64,
        });
    }
    Ok(values)
}

fn assemble_file(header: &LutHeader, payload: &[u8]) -> Result<Vec<u8>, LutError> {
    let header_json = serde_json::to_vec(header).map_err(LutError::HeaderJson)?;
    if header_json.is_empty() || header_json.len() > MAX_HEADER_BYTES {
        return Err(LutError::HeaderSize {
            actual: header_json.len(),
        });
    }
    let header_length = u32::try_from(header_json.len()).map_err(|_| LutError::HeaderSize {
        actual: header_json.len(),
    })?;
    let file_capacity = PREFIX_BYTES
        .checked_add(header_json.len())
        .and_then(|length| length.checked_add(payload.len()))
        .ok_or(LutError::DimensionOverflow)?;
    let mut file = Vec::with_capacity(file_capacity);
    file.extend_from_slice(&LUT_MAGIC);
    file.extend_from_slice(&LUT_SCHEMA_VERSION.to_le_bytes());
    file.extend_from_slice(&header_length.to_le_bytes());
    file.extend_from_slice(&header_json);
    file.extend_from_slice(payload);
    Ok(file)
}

#[derive(Debug, Error)]
pub enum LutError {
    #[error("LUT prefix is truncated: expected at least 14 bytes, got {actual}")]
    TruncatedPrefix { actual: usize },
    #[error("invalid LUT file magic {actual:?}")]
    FileMagic { actual: [u8; 8] },
    #[error("unsupported LUT schema version {actual}")]
    UnsupportedSchema { actual: u16 },
    #[error("invalid LUT header length {actual}")]
    HeaderSize { actual: usize },
    #[error("LUT header declares {declared} bytes but only {available} remain")]
    TruncatedHeader { declared: usize, available: usize },
    #[error("LUT header JSON is invalid: {0}")]
    HeaderJson(#[source] serde_json::Error),
    #[error("header magic {actual:?} does not match the schema magic")]
    HeaderMagic { actual: String },
    #[error("prefix schema {prefix} does not match header schema {header}")]
    SchemaDisagreement { prefix: u16, header: u16 },
    #[error("LUT must have 1..={maximum} axes, got {actual}")]
    AxisCount { actual: usize, maximum: usize },
    #[error("axis {index} ({kind:?}) must use {expected:?}, got {actual:?}")]
    AxisUnit {
        index: usize,
        kind: AxisKind,
        expected: Unit,
        actual: Unit,
    },
    #[error("axis {index} has no coordinates")]
    EmptyAxis { index: usize },
    #[error("axis {axis} coordinate {coordinate} is not finite: {value}")]
    NonFiniteAxisCoordinate {
        axis: usize,
        coordinate: usize,
        value: f64,
    },
    #[error("axis {axis} is not strictly increasing at {lower}, {upper}")]
    NonIncreasingAxis { axis: usize, lower: f64, upper: f64 },
    #[error("axis kind {kind:?} appears more than once")]
    DuplicateAxis { kind: AxisKind },
    #[error("output descriptors or units do not match schema-v1 canonical additive outputs")]
    NonCanonicalOutputs,
    #[error("{field} must not be empty")]
    EmptyMetadata { field: &'static str },
    #[error("generator package name {package:?} is not canonical lowercase text")]
    NonCanonicalPackageName { package: String },
    #[error("generator package {package} has an empty version")]
    EmptyPackageVersion { package: String },
    #[error("PyTMatrix tables must identify package pytmatrix version 0.3.3, got {actual:?}")]
    PyTMatrixVersion { actual: Option<String> },
    #[error("generator config is not valid JSON: {0}")]
    GeneratorConfigJson(#[source] serde_json::Error),
    #[error("generator config JSON must be an object")]
    GeneratorConfigNotObject,
    #[error("embedded generator config SHA-256 mismatch: expected {expected}, got {actual}")]
    ConfigDigestMismatch {
        expected: Sha256Digest,
        actual: Sha256Digest,
    },
    #[error("external generator config SHA-256 mismatch: expected {expected}, got {actual}")]
    ExternalConfigDigestMismatch {
        expected: Sha256Digest,
        actual: Sha256Digest,
    },
    #[error("axis dimensions overflow addressable table size")]
    DimensionOverflow,
    #[error("header grid-point count {header} does not match axes {axes}")]
    GridPointCount { header: u64, axes: u64 },
    #[error("header payload length {header} does not match schema-derived length {expected}")]
    HeaderPayloadLength { header: u64, expected: u64 },
    #[error("payload length {actual} does not match header length {header}")]
    PayloadLength { header: u64, actual: u64 },
    #[error("payload SHA-256 mismatch: expected {expected}, got {actual}")]
    PayloadDigestMismatch {
        expected: Sha256Digest,
        actual: Sha256Digest,
    },
    #[error("table has {actual} values but axes require {expected}")]
    ValueCount { expected: usize, actual: usize },
    #[error("invalid additive output at grid point {point}: {source}")]
    InvalidOutput {
        point: usize,
        #[source]
        source: OutputError,
    },
    #[error(transparent)]
    Science(#[from] ScienceError),
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum InterpolationError {
    #[error("coordinate for {kind:?} must be finite, got {value}")]
    NonFiniteCoordinate { kind: AxisKind, value: f64 },
    #[error("expected {expected} interpolation coordinates, got {actual}")]
    CoordinateCount { expected: usize, actual: usize },
    #[error("coordinate {index} must be for {expected:?}, got {actual:?}")]
    AxisOrder {
        index: usize,
        expected: AxisKind,
        actual: AxisKind,
    },
    #[error("{kind:?} coordinate {value} is outside [{minimum}, {maximum}]")]
    OutsideAxis {
        kind: AxisKind,
        value: f64,
        minimum: f64,
        maximum: f64,
    },
    #[error("interpolation dimensions overflow addressable table size")]
    DimensionOverflow,
    #[error("invalid prepared interpolation plan: {reason}")]
    InvalidPreparedPlan { reason: &'static str },
    #[error("interpolation produced an invalid additive output: {0}")]
    InvalidInterpolatedOutput(OutputError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MeltingModel, OrientationModel, TableValidation, TemporalSampling};

    /// Analytic synthetic fixture only. These affine values are chosen to test
    /// serialization/interpolation invariants and are not T-matrix output.
    fn synthetic_node(diameter_m: f64, temperature_k: f64) -> AdditiveScattering {
        let zh = 100.0 + 10.0 * diameter_m + 0.5 * (temperature_k - 260.0);
        AdditiveScattering::from_components([
            zh,
            0.8 * zh,
            0.25 * zh,
            -0.05 * zh,
            1.0 + 0.2 * diameter_m + 0.01 * (temperature_k - 260.0),
            0.1 + 0.01 * diameter_m + 0.001 * (temperature_k - 260.0),
            0.08 + 0.008 * diameter_m + 0.0008 * (temperature_k - 260.0),
            2.0 * zh,
            5.0 * zh,
        ])
        .unwrap()
    }

    fn synthetic_fixture() -> OfflineLut {
        let axes = vec![
            Axis::new(AxisKind::EquivolumeDiameter, Unit::Meter, vec![1.0, 2.0]).unwrap(),
            Axis::new(AxisKind::Temperature, Unit::Kelvin, vec![260.0, 280.0]).unwrap(),
        ];
        let generator = GeneratorMetadata::new(
            "synthetic-unit-test-generator",
            "1",
            "internal-test",
            "synthetic-fixture-v1",
            None,
            BTreeMap::new(),
        )
        .unwrap();
        let science = ScienceMetadata::new(
            KernelModel::SyntheticFixtureOnly,
            OrientationModel::FixedEuler {
                yaw_deg: 0.0,
                pitch_deg: 0.0,
                roll_deg: 0.0,
            },
            MeltingModel::Dry,
            TemporalSampling::Instantaneous,
            TableValidation::SyntheticFixtureOnly,
        )
        .unwrap();
        let values = [1.0, 2.0]
            .into_iter()
            .flat_map(|diameter| {
                [260.0, 280.0]
                    .into_iter()
                    .map(move |temperature| synthetic_node(diameter, temperature))
            })
            .collect();
        OfflineLut::new(
            axes,
            generator,
            r#"{"fixture":"affine-synthetic-v1","physical":false}"#,
            science,
            values,
        )
        .unwrap()
    }

    fn singleton_axis_fixture() -> OfflineLut {
        let axes = vec![
            Axis::new(AxisKind::EquivolumeDiameter, Unit::Meter, vec![1.0, 2.0]).unwrap(),
            Axis::new(AxisKind::Temperature, Unit::Kelvin, vec![270.0]).unwrap(),
            Axis::new(
                AxisKind::BulkDensity,
                Unit::KilogramPerCubicMeter,
                vec![100.0, 200.0, 300.0],
            )
            .unwrap(),
        ];
        let generator = GeneratorMetadata::new(
            "synthetic-unit-test-generator",
            "1",
            "internal-test",
            "singleton-axis-fixture-v1",
            None,
            BTreeMap::new(),
        )
        .unwrap();
        let science = ScienceMetadata::new(
            KernelModel::SyntheticFixtureOnly,
            OrientationModel::FixedEuler {
                yaw_deg: 0.0,
                pitch_deg: 0.0,
                roll_deg: 0.0,
            },
            MeltingModel::Dry,
            TemporalSampling::Instantaneous,
            TableValidation::SyntheticFixtureOnly,
        )
        .unwrap();
        let mut values = Vec::new();
        for diameter in [1.0, 2.0] {
            for temperature in [270.0] {
                for density in [100.0, 200.0, 300.0] {
                    let components = synthetic_node(diameter, temperature).components();
                    let scale = 1.0 + density / 1_000.0;
                    values.push(
                        AdditiveScattering::from_components(
                            components.map(|component| component * scale),
                        )
                        .unwrap(),
                    );
                }
            }
        }
        OfflineLut::new(
            axes,
            generator,
            r#"{"fixture":"singleton-axis-v1","physical":false}"#,
            science,
            values,
        )
        .unwrap()
    }

    // Frozen copy of the pre-plan implementation. Comparing component bits
    // against it protects corner/axis/arithmetic order while the prepared
    // layout becomes the common CPU/CUDA boundary.
    fn legacy_interpolate(
        table: &OfflineLut,
        coordinates: &[AxisCoordinate],
    ) -> Result<AdditiveScattering, InterpolationError> {
        if coordinates.len() != table.header.axes.len() {
            return Err(InterpolationError::CoordinateCount {
                expected: table.header.axes.len(),
                actual: coordinates.len(),
            });
        }
        let mut brackets = Vec::with_capacity(coordinates.len());
        for (index, (axis, coordinate)) in
            table.header.axes.iter().zip(coordinates.iter()).enumerate()
        {
            if axis.kind != coordinate.kind {
                return Err(InterpolationError::AxisOrder {
                    index,
                    expected: axis.kind,
                    actual: coordinate.kind,
                });
            }
            if !coordinate.value.is_finite() {
                return Err(InterpolationError::NonFiniteCoordinate {
                    kind: coordinate.kind,
                    value: coordinate.value,
                });
            }
            brackets.push(axis.locate(coordinate.value)?);
        }

        let strides = last_axis_fastest_strides(&table.header.axes)?;
        let active_axis_count = brackets
            .iter()
            .filter(|bracket| bracket.lower != bracket.upper)
            .count();
        let corner_count = 1_usize
            .checked_shl(active_axis_count as u32)
            .ok_or(InterpolationError::DimensionOverflow)?;
        let mut accumulated = [0.0; AdditiveScattering::COMPONENT_COUNT];
        for corner in 0..corner_count {
            let mut point_index = 0_usize;
            let mut weight = 1.0;
            let mut active_bit = 0_usize;
            for axis_index in 0..brackets.len() {
                let bracket = brackets[axis_index];
                let coordinate_index = if bracket.lower == bracket.upper {
                    bracket.lower
                } else {
                    let upper = ((corner >> active_bit) & 1) == 1;
                    active_bit += 1;
                    if upper {
                        weight *= bracket.fraction;
                        bracket.upper
                    } else {
                        weight *= 1.0 - bracket.fraction;
                        bracket.lower
                    }
                };
                point_index = point_index
                    .checked_add(
                        coordinate_index
                            .checked_mul(strides[axis_index])
                            .ok_or(InterpolationError::DimensionOverflow)?,
                    )
                    .ok_or(InterpolationError::DimensionOverflow)?;
            }
            let components = table
                .values
                .get(point_index)
                .ok_or(InterpolationError::DimensionOverflow)?
                .components();
            for component in 0..AdditiveScattering::COMPONENT_COUNT {
                accumulated[component] += weight * components[component];
            }
        }
        AdditiveScattering::from_components(accumulated)
            .map_err(InterpolationError::InvalidInterpolatedOutput)
    }

    fn assert_bit_identical(actual: AdditiveScattering, expected: AdditiveScattering) {
        for (index, (actual, expected)) in actual
            .components()
            .into_iter()
            .zip(expected.components())
            .enumerate()
        {
            assert_eq!(
                actual.to_bits(),
                expected.to_bits(),
                "component {index}: {actual:.17e} != {expected:.17e}"
            );
        }
    }

    #[test]
    fn synthetic_fixture_round_trip_is_byte_deterministic() {
        let table = synthetic_fixture();
        let first = table.to_bytes().unwrap();
        let second = table.to_bytes().unwrap();
        assert_eq!(first, second);
        let decoded = OfflineLut::from_bytes(&first).unwrap();
        assert_eq!(decoded, table);
        assert_eq!(decoded.header.magic(), "BRSLUT01");
        assert_eq!(decoded.header.schema_version(), 1);
        decoded
            .verify_generator_config(br#"{"fixture":"affine-synthetic-v1","physical":false}"#)
            .unwrap();
    }

    #[test]
    fn synthetic_affine_fixture_interpolates_exactly_in_declared_axis_order() {
        let table = synthetic_fixture();
        let query = [
            AxisCoordinate::new(AxisKind::EquivolumeDiameter, 1.25).unwrap(),
            AxisCoordinate::new(AxisKind::Temperature, 265.0).unwrap(),
        ];
        let actual = table.interpolate(&query).unwrap().components();
        let expected = synthetic_node(1.25, 265.0).components();
        for index in 0..AdditiveScattering::COMPONENT_COUNT {
            assert!(
                (actual[index] - expected[index]).abs() < 1.0e-12,
                "component {index}: {} != {}",
                actual[index],
                expected[index]
            );
        }

        let order_error = table
            .interpolate(&[
                AxisCoordinate::new(AxisKind::Temperature, 265.0).unwrap(),
                AxisCoordinate::new(AxisKind::EquivolumeDiameter, 1.25).unwrap(),
            ])
            .unwrap_err();
        assert!(matches!(
            order_error,
            InterpolationError::AxisOrder { index: 0, .. }
        ));
    }

    #[test]
    fn prepared_plan_has_fixed_serializable_axis_ordered_layout() {
        let table = singleton_axis_fixture();
        let coordinates = [
            AxisCoordinate::new(AxisKind::EquivolumeDiameter, 1.25).unwrap(),
            AxisCoordinate::new(AxisKind::Temperature, 270.0).unwrap(),
            AxisCoordinate::new(AxisKind::BulkDensity, 250.0).unwrap(),
        ];
        let plan = table.prepare_interpolation(&coordinates).unwrap();

        // Last-axis-fastest dimensions [2, 1, 3]: all-lower base is
        // [0, 0, 1], then active diameter and density offsets retain header
        // order while the singleton temperature axis is omitted.
        assert_eq!(plan.base_point_index(), 1);
        assert_eq!(plan.active_axis_count(), 2);
        assert_eq!(plan.corner_count(), 4);
        assert_eq!(&plan.upper_point_offsets()[..2], &[3, 1]);
        assert_eq!(&plan.upper_fractions()[..2], &[0.25, 0.5]);
        assert!(
            plan.upper_point_offsets()[2..]
                .iter()
                .all(|value| *value == 0)
        );
        assert!(
            plan.upper_fractions()[2..]
                .iter()
                .all(|value| *value == 0.0)
        );
        assert_eq!(size_of::<PreparedInterpolationPlan>(), 272);

        let encoded = serde_json::to_vec(&plan).unwrap();
        let decoded: PreparedInterpolationPlan = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, plan);
        assert_bit_identical(
            table.interpolate_prepared(&decoded).unwrap(),
            legacy_interpolate(&table, &coordinates).unwrap(),
        );
    }

    #[test]
    fn prepared_execution_is_bit_identical_to_legacy_corner_order() {
        let table = synthetic_fixture();
        for (diameter, temperature) in [
            (1.0, 260.0),
            (1.0, 267.3),
            (1.37, 260.0),
            (1.37, 267.3),
            (2.0, 267.3),
            (2.0, 280.0),
        ] {
            let coordinates = [
                AxisCoordinate::new(AxisKind::EquivolumeDiameter, diameter).unwrap(),
                AxisCoordinate::new(AxisKind::Temperature, temperature).unwrap(),
            ];
            let expected = legacy_interpolate(&table, &coordinates).unwrap();
            let plan = table.prepare_interpolation(&coordinates).unwrap();
            assert_bit_identical(table.interpolate_prepared(&plan).unwrap(), expected);
            assert_bit_identical(table.interpolate(&coordinates).unwrap(), expected);
        }
    }

    #[test]
    fn prepared_plan_handles_singletons_and_exact_boundaries() {
        let table = singleton_axis_fixture();
        let lower = [
            AxisCoordinate::new(AxisKind::EquivolumeDiameter, 1.0).unwrap(),
            AxisCoordinate::new(AxisKind::Temperature, 270.0).unwrap(),
            AxisCoordinate::new(AxisKind::BulkDensity, 100.0).unwrap(),
        ];
        let upper = [
            AxisCoordinate::new(AxisKind::EquivolumeDiameter, 2.0).unwrap(),
            AxisCoordinate::new(AxisKind::Temperature, 270.0).unwrap(),
            AxisCoordinate::new(AxisKind::BulkDensity, 300.0).unwrap(),
        ];
        for (coordinates, expected_base) in [(&lower[..], 0), (&upper[..], 5)] {
            let plan = table.prepare_interpolation(coordinates).unwrap();
            assert_eq!(plan.base_point_index(), expected_base);
            assert_eq!(plan.active_axis_count(), 0);
            assert_eq!(plan.corner_count(), 1);
            assert_bit_identical(
                table.interpolate_prepared(&plan).unwrap(),
                legacy_interpolate(&table, coordinates).unwrap(),
            );
        }
    }

    #[test]
    fn plan_preparation_preserves_outside_axis_failures() {
        let table = singleton_axis_fixture();
        let outside_density = [
            AxisCoordinate::new(AxisKind::EquivolumeDiameter, 1.5).unwrap(),
            AxisCoordinate::new(AxisKind::Temperature, 270.0).unwrap(),
            AxisCoordinate::new(AxisKind::BulkDensity, 301.0).unwrap(),
        ];
        let expected = InterpolationError::OutsideAxis {
            kind: AxisKind::BulkDensity,
            value: 301.0,
            minimum: 100.0,
            maximum: 300.0,
        };
        assert_eq!(
            table.prepare_interpolation(&outside_density).unwrap_err(),
            expected
        );
        assert_eq!(table.interpolate(&outside_density).unwrap_err(), expected);

        let outside_singleton = [
            AxisCoordinate::new(AxisKind::EquivolumeDiameter, 1.5).unwrap(),
            AxisCoordinate::new(AxisKind::Temperature, 269.0).unwrap(),
            AxisCoordinate::new(AxisKind::BulkDensity, 200.0).unwrap(),
        ];
        assert!(matches!(
            table.prepare_interpolation(&outside_singleton),
            Err(InterpolationError::OutsideAxis {
                kind: AxisKind::Temperature,
                minimum: 270.0,
                maximum: 270.0,
                ..
            })
        ));
    }

    #[test]
    fn interpolation_refuses_extrapolation_and_nonfinite_coordinates() {
        let table = synthetic_fixture();
        assert!(matches!(
            table.interpolate(&[
                AxisCoordinate::new(AxisKind::EquivolumeDiameter, 0.5).unwrap(),
                AxisCoordinate::new(AxisKind::Temperature, 270.0).unwrap(),
            ]),
            Err(InterpolationError::OutsideAxis {
                kind: AxisKind::EquivolumeDiameter,
                ..
            })
        ));
        assert!(matches!(
            AxisCoordinate::new(AxisKind::Temperature, f64::NAN),
            Err(InterpolationError::NonFiniteCoordinate { .. })
        ));
    }

    #[test]
    fn payload_and_external_config_hash_mismatches_fail_closed() {
        let table = synthetic_fixture();
        let mut bytes = table.to_bytes().unwrap();
        *bytes.last_mut().unwrap() ^= 0x01;
        assert!(matches!(
            OfflineLut::from_bytes(&bytes),
            Err(LutError::PayloadDigestMismatch { .. })
        ));
        assert!(matches!(
            table.verify_generator_config(br#"{"fixture":"different"}"#),
            Err(LutError::ExternalConfigDigestMismatch { .. })
        ));
    }

    #[test]
    fn embedded_config_hash_is_recomputed_not_trusted() {
        let table = synthetic_fixture();
        let mut header = table.header.clone();
        header.generator_config_utf8 =
            r#"{"fixture":"affine-synthetic-v2","physical":false}"#.to_owned();
        let payload = encode_payload(&table.values).unwrap();
        let bytes = assemble_file(&header, &payload).unwrap();
        assert!(matches!(
            OfflineLut::from_bytes(&bytes),
            Err(LutError::ConfigDigestMismatch { .. })
        ));
    }

    #[test]
    fn digest_cannot_hide_an_invalid_additive_grid_node() {
        let table = synthetic_fixture();
        let mut header = table.header.clone();
        let mut payload = encode_payload(&table.values).unwrap();
        // Point 0 covariance real is component 2; make it exceed sqrt(ZH*ZV).
        let offset = 2 * size_of::<f64>();
        payload[offset..offset + 8].copy_from_slice(&1.0e9_f64.to_le_bytes());
        header.payload_sha256 = Sha256Digest::compute(&payload);
        let bytes = assemble_file(&header, &payload).unwrap();
        assert!(matches!(
            OfflineLut::from_bytes(&bytes),
            Err(LutError::InvalidOutput { point: 0, .. })
        ));
    }

    #[test]
    fn axes_require_exact_units_and_strict_ordering() {
        assert!(matches!(
            Axis::new(AxisKind::Temperature, Unit::Meter, vec![260.0]),
            Err(LutError::AxisUnit { .. })
        ));
        assert!(matches!(
            Axis::new(AxisKind::Temperature, Unit::Kelvin, vec![270.0, 270.0]),
            Err(LutError::NonIncreasingAxis { .. })
        ));
    }

    #[test]
    fn file_magic_and_schema_are_never_guessed() {
        let mut bytes = synthetic_fixture().to_bytes().unwrap();
        bytes[0] = b'X';
        assert!(matches!(
            OfflineLut::from_bytes(&bytes),
            Err(LutError::FileMagic { .. })
        ));

        let mut bytes = synthetic_fixture().to_bytes().unwrap();
        bytes[8..10].copy_from_slice(&2_u16.to_le_bytes());
        assert!(matches!(
            OfflineLut::from_bytes(&bytes),
            Err(LutError::UnsupportedSchema { actual: 2 })
        ));
    }

    #[test]
    fn redundant_header_contract_rejects_mislabeled_schema_axes_and_outputs() {
        let table = synthetic_fixture();
        let payload = encode_payload(&table.values).unwrap();

        let mut header = table.header.clone();
        header.magic = "NOTALUT!".to_owned();
        assert!(matches!(
            OfflineLut::from_bytes(&assemble_file(&header, &payload).unwrap()),
            Err(LutError::HeaderMagic { .. })
        ));

        let mut header = table.header.clone();
        header.schema_version = 2;
        assert!(matches!(
            OfflineLut::from_bytes(&assemble_file(&header, &payload).unwrap()),
            Err(LutError::SchemaDisagreement {
                prefix: 1,
                header: 2
            })
        ));

        let mut header = table.header.clone();
        header.axes.push(header.axes[0].clone());
        assert!(matches!(
            OfflineLut::from_bytes(&assemble_file(&header, &payload).unwrap()),
            Err(LutError::DuplicateAxis {
                kind: AxisKind::EquivolumeDiameter
            })
        ));

        let mut header = table.header.clone();
        header.outputs[0].unit = Unit::Degree;
        assert!(matches!(
            OfflineLut::from_bytes(&assemble_file(&header, &payload).unwrap()),
            Err(LutError::NonCanonicalOutputs)
        ));
    }

    #[test]
    fn singleton_axis_is_exact_and_does_not_duplicate_corner_weight() {
        let table = OfflineLut::new(
            vec![Axis::new(AxisKind::Frequency, Unit::Hertz, vec![2.8e9]).unwrap()],
            GeneratorMetadata::new(
                "synthetic-unit-test-generator",
                "1",
                "internal-test",
                "singleton-synthetic-fixture-v1",
                None,
                BTreeMap::new(),
            )
            .unwrap(),
            r#"{"fixture":"singleton-synthetic-v1","physical":false}"#,
            ScienceMetadata::new(
                KernelModel::SyntheticFixtureOnly,
                OrientationModel::FixedEuler {
                    yaw_deg: 0.0,
                    pitch_deg: 0.0,
                    roll_deg: 0.0,
                },
                MeltingModel::Dry,
                TemporalSampling::Instantaneous,
                TableValidation::SyntheticFixtureOnly,
            )
            .unwrap(),
            vec![synthetic_node(1.0, 260.0)],
        )
        .unwrap();
        let expected = table.values()[0];
        assert_eq!(
            table
                .interpolate(&[AxisCoordinate::new(AxisKind::Frequency, 2.8e9).unwrap()])
                .unwrap(),
            expected
        );
    }

    #[test]
    fn pytmatrix_kernel_requires_exact_generator_package_version_metadata() {
        // Metadata validation only; no table or physical scattering fixture is
        // created or represented as PyTMatrix output in this test.
        let generator = GeneratorMetadata::new(
            "research-generator",
            "1",
            "generate.py",
            "uncommitted-test-source",
            Some("3.11.9".to_owned()),
            BTreeMap::new(),
        )
        .unwrap();
        let science = ScienceMetadata::new(
            KernelModel::TMatrix {
                implementation: TMatrixImplementation::PyTMatrix033,
            },
            OrientationModel::ExplicitBodyFrame,
            MeltingModel::Dry,
            TemporalSampling::Instantaneous,
            TableValidation::ResearchOnlyUnvalidated,
        )
        .unwrap();
        assert!(matches!(
            validate_generator_science(&generator, &science),
            Err(LutError::PyTMatrixVersion { actual: None })
        ));

        let mut packages = BTreeMap::new();
        packages.insert("pytmatrix".to_owned(), "0.3.3".to_owned());
        let pinned = GeneratorMetadata::new(
            "research-generator",
            "1",
            "generate.py",
            "uncommitted-test-source",
            Some("3.11.9".to_owned()),
            packages,
        )
        .unwrap();
        validate_generator_science(&pinned, &science).unwrap();
    }
}
