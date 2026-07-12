//! Strict reader and interpolator for the official WRF P3 v5.4 tables.
//!
//! The two official assets are intentionally not embedded in BowEcho.  A
//! caller supplies one of the exact files from the pinned WRF revision; this
//! module verifies its byte length and SHA-256 before parsing any values.  The
//! parser also validates every main-table and collision-table record, even
//! though only the PSD fields needed by [`P3LookupTableV54`] are retained.

use std::fs;
use std::io;
use std::num::{ParseFloatError, ParseIntError};
use std::path::{Path, PathBuf};
use std::str::{Lines, Utf8Error};

use thiserror::Error;

use crate::{
    P3_MODULE_VERSION, P3_TABLE_GENERATOR_VERSION, P3_THREE_MOMENT_TABLE_SHA256,
    P3_THREE_MOMENT_TABLE_VERSION, P3_TWO_MOMENT_TABLE_SHA256, P3_TWO_MOMENT_TABLE_VERSION,
    P3_WRF_SOURCE_COMMIT, P3Category, P3IceMomentOrder, P3LookupAxisClamps, P3LookupFailure,
    P3LookupQuery, P3LookupSolution, P3LookupTableDescriptor, P3LookupTableV54, P3WrfScheme,
    Sha256Digest,
};

pub const P3_TABLE_READER_REVISION: &str = "wrf-p3-v5.4-exact-table-reader-v1";

const MASS_AXIS_SIZE: usize = 50;
const RIME_AXIS_SIZE: usize = 4;
const DENSITY_AXIS_SIZE: usize = 5;
const SHAPE_AXIS_SIZE: usize = 11;
const RAIN_COLLISION_AXIS_SIZE: usize = 30;
const TWO_MOMENT_MAIN_FIELDS: usize = 14;
const THREE_MOMENT_MAIN_FIELDS: usize = 15;
const COLLISION_FIELDS: usize = 2;
const WRF_QSMALL: f32 = 1.0e-14;
const WRF_NSMALL: f32 = 1.0e-16;
const WRF_ZSMALL: f32 = 1.0e-35;
// Preserve the decimal `REAL` literal from the pinned WRF Fortran source.
// Replacing it with Rust's PI spelling would obscure source-parity intent.
#[allow(clippy::approx_constant, clippy::excessive_precision)]
const WRF_PI: f32 = 3.14159265;
const WRF_TRIPLE_MOMENT_ITERATIONS: usize = 5;

const OFFICIAL_LAYOUT: TableLayout = TableLayout {
    shapes: SHAPE_AXIS_SIZE,
    densities: DENSITY_AXIS_SIZE,
    rimes: RIME_AXIS_SIZE,
    masses: MASS_AXIS_SIZE,
    rain_collisions: RAIN_COLLISION_AXIS_SIZE,
};

/// The exact external file required for an official P3 lookup mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct P3TableAssetSpec {
    pub kind: P3OfficialTableKind,
    pub file_name: &'static str,
    pub source_url: &'static str,
    pub expected_bytes: usize,
    pub expected_data_rows: usize,
    pub expected_sha256: &'static str,
    pub table_version: &'static str,
}

pub const P3_TWO_MOMENT_TABLE_ASSET: P3TableAssetSpec = P3TableAssetSpec {
    kind: P3OfficialTableKind::TwoMoment,
    file_name: "p3_lookupTable_1.dat-v5.4_2momI",
    source_url: concat!(
        "https://raw.githubusercontent.com/wrf-model/WRF/",
        "f52c197ed39d12e087d02c50f412d90d418f6186/",
        "run/p3_lookupTable_1.dat-v5.4_2momI"
    ),
    expected_bytes: 1_606_038,
    expected_data_rows: 31_000,
    expected_sha256: P3_TWO_MOMENT_TABLE_SHA256,
    table_version: P3_TWO_MOMENT_TABLE_VERSION,
};

pub const P3_THREE_MOMENT_TABLE_ASSET: P3TableAssetSpec = P3TableAssetSpec {
    kind: P3OfficialTableKind::ThreeMoment,
    file_name: "p3_lookupTable_1.dat-v5.4_3momI",
    source_url: concat!(
        "https://raw.githubusercontent.com/wrf-model/WRF/",
        "f52c197ed39d12e087d02c50f412d90d418f6186/",
        "run/p3_lookupTable_1.dat-v5.4_3momI"
    ),
    expected_bytes: 17_886_038,
    expected_data_rows: 341_000,
    expected_sha256: P3_THREE_MOMENT_TABLE_SHA256,
    table_version: P3_THREE_MOMENT_TABLE_VERSION,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum P3OfficialTableKind {
    TwoMoment,
    ThreeMoment,
}

impl P3OfficialTableKind {
    #[must_use]
    pub const fn asset_spec(self) -> &'static P3TableAssetSpec {
        match self {
            Self::TwoMoment => &P3_TWO_MOMENT_TABLE_ASSET,
            Self::ThreeMoment => &P3_THREE_MOMENT_TABLE_ASSET,
        }
    }

    const fn exact_header(self) -> &'static str {
        match self {
            // The leading space and doubled space after the colon are
            // significant consequences of list-directed Fortran WRITE.
            Self::TwoMoment => " LOOKUP_TABLE_1-version:  5.4_2momI",
            Self::ThreeMoment => " LOOKUP_TABLE_1-version:  5.4_3momI",
        }
    }

    const fn moment_order(self) -> P3IceMomentOrder {
        match self {
            Self::TwoMoment => P3IceMomentOrder::TwoMoment,
            Self::ThreeMoment => P3IceMomentOrder::TripleMomentQzi,
        }
    }
}

/// A parsed, hash-qualified official P3 lookup table.
///
/// Loading is explicit and lazy: constructing an asset specification performs
/// no I/O, and only [`Self::load_path`] or [`Self::load_bytes`] allocates the
/// retained PSD arrays.
#[derive(Clone, Debug)]
pub struct P3OfficialTableV54 {
    kind: P3OfficialTableKind,
    descriptor: P3LookupTableDescriptor,
    layout: TableLayout,
    payload: TablePayload,
}

impl P3OfficialTableV54 {
    pub fn load_from_directory(
        kind: P3OfficialTableKind,
        directory: impl AsRef<Path>,
    ) -> Result<Self, P3TableLoadError> {
        Self::load_path(kind, directory.as_ref().join(kind.asset_spec().file_name))
    }

    pub fn load_path(
        kind: P3OfficialTableKind,
        path: impl AsRef<Path>,
    ) -> Result<Self, P3TableLoadError> {
        let path = path.as_ref();
        let bytes = fs::read(path).map_err(|source| P3TableLoadError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::load_bytes(kind, &bytes)
    }

    pub fn load_bytes(kind: P3OfficialTableKind, bytes: &[u8]) -> Result<Self, P3TableLoadError> {
        let spec = kind.asset_spec();
        if bytes.len() != spec.expected_bytes {
            return Err(P3TableLoadError::ByteLength {
                expected: spec.expected_bytes,
                actual: bytes.len(),
            });
        }
        let digest = Sha256Digest::compute(bytes);
        if digest.to_hex() != spec.expected_sha256 {
            return Err(P3TableLoadError::DigestMismatch {
                expected: spec.expected_sha256.to_owned(),
                actual: digest.to_hex(),
            });
        }
        parse_exact_table(kind, bytes, OFFICIAL_LAYOUT, descriptor(kind, digest))
    }

    #[must_use]
    pub const fn kind(&self) -> P3OfficialTableKind {
        self.kind
    }
}

impl P3LookupTableV54 for P3OfficialTableV54 {
    fn descriptor(&self) -> &P3LookupTableDescriptor {
        &self.descriptor
    }

    fn lookup_psd(&self, query: P3LookupQuery) -> Result<P3LookupSolution, P3LookupFailure> {
        self.validate_query(query)?;
        let mass = mass_axis(f32_checked(
            "mean particle mass",
            query.mean_particle_mass_kg,
        )?)?;
        let rime = rime_axis(f32_checked("rime mass fraction", query.rime_mass_fraction)?)?;
        let density = density_axis(f32_checked("rime density", query.rime_density_kg_m3)?)?;

        match &self.payload {
            TablePayload::TwoMoment { lambda, mu } => {
                let slope = interpolate_three_axes(lambda, self.layout, mass, rime, density);
                let shape = interpolate_three_axes(mu, self.layout, mass, rime, density);
                validate_solution(slope, shape)?;
                Ok(P3LookupSolution {
                    slope_lambda_m_inv: f64::from(slope),
                    shape_mu: f64::from(shape),
                    axis_clamps: P3LookupAxisClamps {
                        normalized_mass: mass.clamped,
                        rime_fraction: rime.clamped,
                        rime_density: density.clamped,
                        shape: false,
                    },
                })
            }
            TablePayload::ThreeMoment {
                mean_density,
                lambda,
                mu,
            } => {
                let number = f32_checked("total ice number", query.total_number_per_kg)?;
                let total_ice = f32_checked("total ice mass", query.total_ice_kgkg)?;
                let sixth = f32_checked(
                    "sixth ice moment",
                    query
                        .sixth_moment_per_kg
                        .expect("triple-moment query validated above"),
                )?
                .max(WRF_ZSMALL);
                let mut third = 6.0 / (200.0 * WRF_PI) * total_ice;
                let mut shape_axis_value = shape_axis(compute_mu_3moment(number, third, sixth)?)?;
                let mut any_shape_clamp = false;

                for iteration in 0..WRF_TRIPLE_MOMENT_ITERATIONS {
                    if iteration != 0 {
                        shape_axis_value = shape_axis(compute_mu_3moment(number, third, sixth)?)?;
                    }
                    any_shape_clamp |= shape_axis_value.clamped;
                    let bulk_density = interpolate_four_axes(
                        mean_density,
                        self.layout,
                        shape_axis_value,
                        mass,
                        rime,
                        density,
                    );
                    if !bulk_density.is_finite() || bulk_density <= 0.0 {
                        return Err(P3LookupFailure::Corrupt(format!(
                            "interpolated P3 bulk density must be finite and positive, got {bulk_density}"
                        )));
                    }
                    third = 6.0 / (bulk_density * WRF_PI) * total_ice;
                    if !third.is_finite() || third <= 1.0e-20 {
                        return Err(P3LookupFailure::OutsideDomain(format!(
                            "P3 third-moment estimate is outside COMPUTE_MU_3MOMENT: {third}"
                        )));
                    }
                }

                let slope = interpolate_four_axes(
                    lambda,
                    self.layout,
                    shape_axis_value,
                    mass,
                    rime,
                    density,
                );
                let shape =
                    interpolate_four_axes(mu, self.layout, shape_axis_value, mass, rime, density);
                validate_solution(slope, shape)?;
                Ok(P3LookupSolution {
                    slope_lambda_m_inv: f64::from(slope),
                    shape_mu: f64::from(shape),
                    axis_clamps: P3LookupAxisClamps {
                        normalized_mass: mass.clamped,
                        rime_fraction: rime.clamped,
                        rime_density: density.clamped,
                        shape: any_shape_clamp,
                    },
                })
            }
        }
    }
}

impl P3OfficialTableV54 {
    fn validate_query(&self, query: P3LookupQuery) -> Result<(), P3LookupFailure> {
        if query.scheme.moment_order() != self.kind.moment_order() {
            return Err(P3LookupFailure::Unsupported(format!(
                "WRF mp_physics={} requires {}, but {:?} is loaded",
                query.scheme.mp_physics(),
                query.scheme.required_table_version(),
                self.kind
            )));
        }
        if query.category == P3Category::Category2
            && query.scheme != P3WrfScheme::Mp52TwoIcePredictedCloudNumber
        {
            return Err(P3LookupFailure::Unsupported(format!(
                "WRF mp_physics={} does not have P3 category 2",
                query.scheme.mp_physics()
            )));
        }
        for (name, value) in [
            ("mean particle mass", query.mean_particle_mass_kg),
            ("total ice number", query.total_number_per_kg),
            ("total ice mass", query.total_ice_kgkg),
        ] {
            if !value.is_finite() || value <= 0.0 {
                return Err(P3LookupFailure::OutsideDomain(format!(
                    "{name} must be finite and positive, got {value}"
                )));
            }
        }
        if !query.rime_mass_fraction.is_finite() || !(0.0..=1.0).contains(&query.rime_mass_fraction)
        {
            return Err(P3LookupFailure::OutsideDomain(format!(
                "rime mass fraction must be within [0, 1], got {}",
                query.rime_mass_fraction
            )));
        }
        if !query.rime_density_kg_m3.is_finite() || query.rime_density_kg_m3 < 0.0 {
            return Err(P3LookupFailure::OutsideDomain(format!(
                "rime density must be finite and nonnegative, got {}",
                query.rime_density_kg_m3
            )));
        }
        let total_ice = f32_checked("total ice mass", query.total_ice_kgkg)?;
        let number = f32_checked("total ice number", query.total_number_per_kg)?;
        if total_ice < WRF_QSMALL {
            return Err(P3LookupFailure::OutsideDomain(format!(
                "total ice mass {total_ice} is below WRF qsmall={WRF_QSMALL}"
            )));
        }
        if number < WRF_NSMALL {
            return Err(P3LookupFailure::OutsideDomain(format!(
                "total ice number {number} is below WRF nsmall={WRF_NSMALL}"
            )));
        }
        match (self.kind, query.sixth_moment_per_kg) {
            (P3OfficialTableKind::TwoMoment, None) => Ok(()),
            (P3OfficialTableKind::TwoMoment, Some(_)) => Err(P3LookupFailure::Unsupported(
                "two-moment P3 lookup cannot consume a sixth moment".to_owned(),
            )),
            (P3OfficialTableKind::ThreeMoment, Some(value)) if value.is_finite() && value > 0.0 => {
                Ok(())
            }
            (P3OfficialTableKind::ThreeMoment, _) => Err(P3LookupFailure::OutsideDomain(
                "triple-moment P3 lookup requires a finite positive sixth moment".to_owned(),
            )),
        }
    }
}

#[derive(Debug, Error)]
pub enum P3TableLoadError {
    #[error("cannot read P3 table {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("P3 table byte length mismatch: expected {expected}, got {actual}")]
    ByteLength { expected: usize, actual: usize },
    #[error("P3 table SHA-256 mismatch: expected {expected}, got {actual}")]
    DigestMismatch { expected: String, actual: String },
    #[error("P3 table is not UTF-8: {source}")]
    Utf8 {
        #[source]
        source: Utf8Error,
    },
    #[error("P3 table header mismatch: expected {expected:?}, got {actual:?}")]
    Header { expected: String, actual: String },
    #[error("P3 table separator mismatch on line 2: expected one space, got {actual:?}")]
    Separator { actual: String },
    #[error("P3 table ended before line {line}, expected {expected}")]
    UnexpectedEof { line: usize, expected: String },
    #[error("P3 table has extra content on line {line}: {actual:?}")]
    ExtraContent { line: usize, actual: String },
    #[error("P3 table line {line} has {actual} tokens, expected {expected} for {record_kind}")]
    TokenCount {
        line: usize,
        record_kind: &'static str,
        expected: usize,
        actual: usize,
    },
    #[error("P3 table line {line} token {position} is not an integer ({token:?}): {source}")]
    InvalidInteger {
        line: usize,
        position: usize,
        token: String,
        #[source]
        source: ParseIntError,
    },
    #[error(
        "P3 table line {line} index {position} is {actual}, expected {expected} for {record_kind}"
    )]
    WrongIndex {
        line: usize,
        record_kind: &'static str,
        position: usize,
        expected: usize,
        actual: usize,
    },
    #[error("P3 table line {line} token {position} is not a float ({token:?}): {source}")]
    InvalidFloat {
        line: usize,
        position: usize,
        token: String,
        #[source]
        source: ParseFloatError,
    },
    #[error("P3 table line {line} token {position} is nonfinite: {token:?}")]
    NonFinite {
        line: usize,
        position: usize,
        token: String,
    },
}

#[derive(Clone, Copy, Debug)]
struct TableLayout {
    shapes: usize,
    densities: usize,
    rimes: usize,
    masses: usize,
    rain_collisions: usize,
}

#[derive(Clone, Debug)]
enum TablePayload {
    TwoMoment {
        lambda: Vec<f32>,
        mu: Vec<f32>,
    },
    ThreeMoment {
        mean_density: Vec<f32>,
        lambda: Vec<f32>,
        mu: Vec<f32>,
    },
}

fn descriptor(kind: P3OfficialTableKind, digest: Sha256Digest) -> P3LookupTableDescriptor {
    P3LookupTableDescriptor {
        wrf_source_commit: P3_WRF_SOURCE_COMMIT.to_owned(),
        p3_module_version: P3_MODULE_VERSION.to_owned(),
        generator_version: P3_TABLE_GENERATOR_VERSION.to_owned(),
        table_version: kind.asset_spec().table_version.to_owned(),
        table_sha256: digest,
    }
}

fn parse_exact_table(
    kind: P3OfficialTableKind,
    bytes: &[u8],
    layout: TableLayout,
    descriptor: P3LookupTableDescriptor,
) -> Result<P3OfficialTableV54, P3TableLoadError> {
    let text = std::str::from_utf8(bytes).map_err(|source| P3TableLoadError::Utf8 { source })?;
    let mut reader = TableLines::new(text);
    let (_, header) = reader.required("version header")?;
    if header != kind.exact_header() {
        return Err(P3TableLoadError::Header {
            expected: kind.exact_header().to_owned(),
            actual: header.to_owned(),
        });
    }
    let (_, separator) = reader.required("one-space separator")?;
    if separator != " " {
        return Err(P3TableLoadError::Separator {
            actual: separator.to_owned(),
        });
    }

    let payload = match kind {
        P3OfficialTableKind::TwoMoment => parse_two_moment_payload(&mut reader, layout)?,
        P3OfficialTableKind::ThreeMoment => parse_three_moment_payload(&mut reader, layout)?,
    };
    reader.finish()?;
    Ok(P3OfficialTableV54 {
        kind,
        descriptor,
        layout,
        payload,
    })
}

fn parse_two_moment_payload(
    reader: &mut TableLines<'_>,
    layout: TableLayout,
) -> Result<TablePayload, P3TableLoadError> {
    let retained = layout.densities * layout.rimes * layout.masses;
    let mut lambda = Vec::with_capacity(retained);
    let mut mu = Vec::with_capacity(retained);
    for density in 1..=layout.densities {
        for rime in 1..=layout.rimes {
            for mass in 1..=layout.masses {
                let (line_number, line) = reader.required("two-moment main record")?;
                let values = parse_record::<TWO_MOMENT_MAIN_FIELDS>(
                    line_number,
                    line,
                    "two-moment main record",
                    &[density, rime, mass],
                )?;
                lambda.push(values[12]);
                mu.push(values[13]);
            }
            parse_collision_block(reader, layout, rime)?;
        }
    }
    Ok(TablePayload::TwoMoment { lambda, mu })
}

fn parse_three_moment_payload(
    reader: &mut TableLines<'_>,
    layout: TableLayout,
) -> Result<TablePayload, P3TableLoadError> {
    let retained = layout.shapes * layout.densities * layout.rimes * layout.masses;
    let mut mean_density = Vec::with_capacity(retained);
    let mut lambda = Vec::with_capacity(retained);
    let mut mu = Vec::with_capacity(retained);
    for shape in 1..=layout.shapes {
        for density in 1..=layout.densities {
            for rime in 1..=layout.rimes {
                for mass in 1..=layout.masses {
                    let (line_number, line) = reader.required("three-moment main record")?;
                    let values = parse_record::<THREE_MOMENT_MAIN_FIELDS>(
                        line_number,
                        line,
                        "three-moment main record",
                        &[shape, density, rime, mass],
                    )?;
                    mean_density.push(values[11]);
                    lambda.push(values[13]);
                    mu.push(values[14]);
                }
                parse_collision_block(reader, layout, rime)?;
            }
        }
    }
    Ok(TablePayload::ThreeMoment {
        mean_density,
        lambda,
        mu,
    })
}

fn parse_collision_block(
    reader: &mut TableLines<'_>,
    layout: TableLayout,
    rime: usize,
) -> Result<(), P3TableLoadError> {
    for mass in 1..=layout.masses {
        for rain in 1..=layout.rain_collisions {
            let (line_number, line) = reader.required("ice-rain collision record")?;
            parse_record::<COLLISION_FIELDS>(
                line_number,
                line,
                "ice-rain collision record",
                &[mass, rain, rime],
            )?;
        }
    }
    Ok(())
}

fn parse_record<const FIELDS: usize>(
    line_number: usize,
    line: &str,
    record_kind: &'static str,
    expected_indices: &[usize],
) -> Result<[f32; FIELDS], P3TableLoadError> {
    let expected_tokens = expected_indices.len() + FIELDS;
    let actual_tokens = line.split_whitespace().count();
    if actual_tokens != expected_tokens {
        return Err(P3TableLoadError::TokenCount {
            line: line_number,
            record_kind,
            expected: expected_tokens,
            actual: actual_tokens,
        });
    }
    let mut tokens = line.split_whitespace();
    for (offset, expected) in expected_indices.iter().copied().enumerate() {
        let token = tokens.next().expect("token count checked above");
        let actual = token
            .parse::<usize>()
            .map_err(|source| P3TableLoadError::InvalidInteger {
                line: line_number,
                position: offset + 1,
                token: token.to_owned(),
                source,
            })?;
        if actual != expected {
            return Err(P3TableLoadError::WrongIndex {
                line: line_number,
                record_kind,
                position: offset + 1,
                expected,
                actual,
            });
        }
    }
    let mut values = [0.0_f32; FIELDS];
    for (offset, value) in values.iter_mut().enumerate() {
        let token = tokens.next().expect("token count checked above");
        let position = expected_indices.len() + offset + 1;
        let parsed = token
            .parse::<f32>()
            .map_err(|source| P3TableLoadError::InvalidFloat {
                line: line_number,
                position,
                token: token.to_owned(),
                source,
            })?;
        if !parsed.is_finite() {
            return Err(P3TableLoadError::NonFinite {
                line: line_number,
                position,
                token: token.to_owned(),
            });
        }
        *value = parsed;
    }
    Ok(values)
}

struct TableLines<'a> {
    lines: Lines<'a>,
    next_line_number: usize,
}

impl<'a> TableLines<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            lines: text.lines(),
            next_line_number: 1,
        }
    }

    fn required(&mut self, expected: &str) -> Result<(usize, &'a str), P3TableLoadError> {
        let line = self.next_line_number;
        self.next_line_number += 1;
        self.lines.next().map(|value| (line, value)).ok_or_else(|| {
            P3TableLoadError::UnexpectedEof {
                line,
                expected: expected.to_owned(),
            }
        })
    }

    fn finish(&mut self) -> Result<(), P3TableLoadError> {
        if let Some(actual) = self.lines.next() {
            return Err(P3TableLoadError::ExtraContent {
                line: self.next_line_number,
                actual: actual.to_owned(),
            });
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
struct LookupAxis {
    lower: usize,
    coordinate: f32,
    clamped: bool,
}

impl LookupAxis {
    fn weight(self) -> f32 {
        self.coordinate - (self.lower + 1) as f32
    }
}

fn lookup_axis(raw: f32, size: usize, name: &str) -> Result<LookupAxis, P3LookupFailure> {
    if !raw.is_finite() {
        return Err(P3LookupFailure::OutsideDomain(format!(
            "{name} table coordinate is nonfinite"
        )));
    }
    let coordinate = raw.clamp(1.0, size as f32);
    let lower_one_based = raw.trunc().clamp(1.0, (size - 1) as f32) as usize;
    Ok(LookupAxis {
        lower: lower_one_based - 1,
        coordinate,
        clamped: coordinate != raw,
    })
}

fn mass_axis(mean_particle_mass_kg: f32) -> Result<LookupAxis, P3LookupFailure> {
    lookup_axis(
        (mean_particle_mass_kg.log10() + 18.0) * 3.444606 - 10.0,
        MASS_AXIS_SIZE,
        "normalized mass",
    )
}

fn rime_axis(rime_fraction: f32) -> Result<LookupAxis, P3LookupFailure> {
    lookup_axis(rime_fraction * 3.0 + 1.0, RIME_AXIS_SIZE, "rime fraction")
}

fn density_axis(rime_density_kg_m3: f32) -> Result<LookupAxis, P3LookupFailure> {
    let raw = if rime_density_kg_m3 <= 650.0 {
        (rime_density_kg_m3 - 50.0) * 0.005 + 1.0
    } else {
        (rime_density_kg_m3 - 650.0) * 0.004 + 4.0
    };
    lookup_axis(raw, DENSITY_AXIS_SIZE, "rime density")
}

fn shape_axis(mu: f32) -> Result<LookupAxis, P3LookupFailure> {
    lookup_axis(mu * 0.5 + 1.0, SHAPE_AXIS_SIZE, "triple-moment shape")
}

fn two_index(layout: TableLayout, density: usize, rime: usize, mass: usize) -> usize {
    (density * layout.rimes + rime) * layout.masses + mass
}

fn three_index(
    layout: TableLayout,
    shape: usize,
    density: usize,
    rime: usize,
    mass: usize,
) -> usize {
    ((shape * layout.densities + density) * layout.rimes + rime) * layout.masses + mass
}

fn lerp(lower: f32, upper: f32, weight: f32) -> f32 {
    lower + weight * (upper - lower)
}

fn interpolate_three_axes(
    values: &[f32],
    layout: TableLayout,
    mass: LookupAxis,
    rime: LookupAxis,
    density: LookupAxis,
) -> f32 {
    let at_density = |density_index| {
        let current_rime = lerp(
            values[two_index(layout, density_index, rime.lower, mass.lower)],
            values[two_index(layout, density_index, rime.lower, mass.lower + 1)],
            mass.weight(),
        );
        let next_rime = lerp(
            values[two_index(layout, density_index, rime.lower + 1, mass.lower)],
            values[two_index(layout, density_index, rime.lower + 1, mass.lower + 1)],
            mass.weight(),
        );
        lerp(current_rime, next_rime, rime.weight())
    };
    lerp(
        at_density(density.lower),
        at_density(density.lower + 1),
        density.weight(),
    )
}

fn interpolate_four_axes(
    values: &[f32],
    layout: TableLayout,
    shape: LookupAxis,
    mass: LookupAxis,
    rime: LookupAxis,
    density: LookupAxis,
) -> f32 {
    let at_shape = |shape_index| {
        let at_density = |density_index| {
            let current_rime = lerp(
                values[three_index(layout, shape_index, density_index, rime.lower, mass.lower)],
                values[three_index(
                    layout,
                    shape_index,
                    density_index,
                    rime.lower,
                    mass.lower + 1,
                )],
                mass.weight(),
            );
            let next_rime = lerp(
                values[three_index(
                    layout,
                    shape_index,
                    density_index,
                    rime.lower + 1,
                    mass.lower,
                )],
                values[three_index(
                    layout,
                    shape_index,
                    density_index,
                    rime.lower + 1,
                    mass.lower + 1,
                )],
                mass.weight(),
            );
            lerp(current_rime, next_rime, rime.weight())
        };
        lerp(
            at_density(density.lower),
            at_density(density.lower + 1),
            density.weight(),
        )
    };
    lerp(
        at_shape(shape.lower),
        at_shape(shape.lower + 1),
        shape.weight(),
    )
}

fn compute_mu_3moment(moment0: f32, moment3: f32, moment6: f32) -> Result<f32, P3LookupFailure> {
    if !moment3.is_finite() || moment3 <= 1.0e-20 {
        return Err(P3LookupFailure::OutsideDomain(format!(
            "COMPUTE_MU_3MOMENT requires M3 > 1e-20, got {moment3}"
        )));
    }
    let g = (moment0 / moment3) * (moment6 / moment3);
    if !g.is_finite() {
        return Err(P3LookupFailure::OutsideDomain(format!(
            "COMPUTE_MU_3MOMENT produced nonfinite G from M0={moment0}, M3={moment3}, M6={moment6}"
        )));
    }
    let g2 = g * g;
    let mu = if g >= 20.0 {
        0.0
    } else if g >= 13.31 {
        3.3638e-3 * g2 - 1.7152e-1 * g + 2.0857
    } else if g >= 7.123 {
        1.5900e-2 * g2 - 4.8202e-1 * g + 4.0108
    } else if g >= 4.200 {
        1.0730e-1 * g2 - 1.7481 * g + 8.4246
    } else if g >= 2.946 {
        5.9070e-1 * g2 - 5.7918 * g + 16.919
    } else if g >= 1.793 {
        4.3966 * g2 - 26.659 * g + 45.477
    } else if g >= 1.405 {
        47.552 * g2 - 179.58 * g + 181.26
    } else if g >= 1.230 {
        308.89 * g2 - 908.54 * g + 689.95
    } else {
        20.0
    };
    if mu.is_finite() {
        Ok(mu)
    } else {
        Err(P3LookupFailure::OutsideDomain(
            "COMPUTE_MU_3MOMENT produced a nonfinite shape".to_owned(),
        ))
    }
}

fn f32_checked(name: &str, value: f64) -> Result<f32, P3LookupFailure> {
    let narrowed = value as f32;
    if narrowed.is_finite() {
        Ok(narrowed)
    } else {
        Err(P3LookupFailure::OutsideDomain(format!(
            "{name} cannot be represented as WRF REAL: {value}"
        )))
    }
}

fn validate_solution(lambda: f32, mu: f32) -> Result<(), P3LookupFailure> {
    if !lambda.is_finite() || lambda <= 0.0 {
        return Err(P3LookupFailure::Corrupt(format!(
            "interpolated P3 lambda must be finite and positive, got {lambda}"
        )));
    }
    if !mu.is_finite() || !(0.0..=20.0).contains(&mu) {
        return Err(P3LookupFailure::Corrupt(format!(
            "interpolated P3 mu must be within [0, 20], got {mu}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;

    fn test_descriptor(kind: P3OfficialTableKind, bytes: &[u8]) -> P3LookupTableDescriptor {
        descriptor(kind, Sha256Digest::compute(bytes))
    }

    fn synthetic_table(kind: P3OfficialTableKind, layout: TableLayout) -> String {
        let mut output = String::new();
        writeln!(output, "{}", kind.exact_header()).unwrap();
        writeln!(output, " ").unwrap();
        match kind {
            P3OfficialTableKind::TwoMoment => {
                for density in 1..=layout.densities {
                    for rime in 1..=layout.rimes {
                        for mass in 1..=layout.masses {
                            write!(output, "{density} {rime} {mass}").unwrap();
                            for field in 0..TWO_MOMENT_MAIN_FIELDS {
                                let value = if field == 12 {
                                    100_000.0 + (density * 100 + rime * 10 + mass) as f32
                                } else if field == 13 {
                                    (density + rime + mass) as f32
                                } else {
                                    101.25 + field as f32
                                };
                                write!(output, " {value}").unwrap();
                            }
                            writeln!(output).unwrap();
                        }
                        write_collision_rows(&mut output, layout, rime);
                    }
                }
            }
            P3OfficialTableKind::ThreeMoment => {
                for shape in 1..=layout.shapes {
                    for density in 1..=layout.densities {
                        for rime in 1..=layout.rimes {
                            for mass in 1..=layout.masses {
                                write!(output, "{shape} {density} {rime} {mass}").unwrap();
                                for field in 0..THREE_MOMENT_MAIN_FIELDS {
                                    let value = match field {
                                        11 => 200.0 + shape as f32,
                                        13 => {
                                            100_000.0
                                                + (shape * 1_000 + density * 100 + rime * 10 + mass)
                                                    as f32
                                        }
                                        14 => (shape + density + rime + mass) as f32,
                                        _ => 101.25 + field as f32,
                                    };
                                    write!(output, " {value}").unwrap();
                                }
                                writeln!(output).unwrap();
                            }
                            write_collision_rows(&mut output, layout, rime);
                        }
                    }
                }
            }
        }
        output
    }

    fn write_collision_rows(output: &mut String, layout: TableLayout, rime: usize) {
        for mass in 1..=layout.masses {
            for rain in 1..=layout.rain_collisions {
                writeln!(output, "{mass} {rain} {rime} -86.446 -97.531").unwrap();
            }
        }
    }

    fn parse_synthetic(
        kind: P3OfficialTableKind,
        text: &str,
        layout: TableLayout,
    ) -> Result<P3OfficialTableV54, P3TableLoadError> {
        parse_exact_table(
            kind,
            text.as_bytes(),
            layout,
            test_descriptor(kind, text.as_bytes()),
        )
    }

    #[test]
    fn external_asset_contract_pins_official_bytes_and_revisions() {
        assert_eq!(P3_TWO_MOMENT_TABLE_ASSET.expected_bytes, 1_606_038);
        assert_eq!(P3_TWO_MOMENT_TABLE_ASSET.expected_data_rows, 31_000);
        assert_eq!(
            P3_TWO_MOMENT_TABLE_ASSET.expected_sha256,
            "be1ab6fb03481e376e47c6c79d808af5d8ab069f2b242931e9c54801bad4ae84"
        );
        assert_eq!(P3_THREE_MOMENT_TABLE_ASSET.expected_bytes, 17_886_038);
        assert_eq!(P3_THREE_MOMENT_TABLE_ASSET.expected_data_rows, 341_000);
        assert_eq!(
            P3_THREE_MOMENT_TABLE_ASSET.expected_sha256,
            "9a3c57ecc09498802c8d7cb3931dbb0200dcf0f51466b3e288120c080271e6dc"
        );
        assert!(
            P3_TWO_MOMENT_TABLE_ASSET
                .source_url
                .contains(P3_WRF_SOURCE_COMMIT)
        );
        assert!(
            P3_THREE_MOMENT_TABLE_ASSET
                .source_url
                .contains(P3_WRF_SOURCE_COMMIT)
        );
    }

    #[test]
    fn official_loader_rejects_wrong_length_and_same_length_wrong_digest() {
        assert!(matches!(
            P3OfficialTableV54::load_bytes(P3OfficialTableKind::TwoMoment, b""),
            Err(P3TableLoadError::ByteLength { .. })
        ));
        let same_length_wrong_bytes = vec![0; P3_TWO_MOMENT_TABLE_ASSET.expected_bytes];
        assert!(matches!(
            P3OfficialTableV54::load_bytes(
                P3OfficialTableKind::TwoMoment,
                &same_length_wrong_bytes
            ),
            Err(P3TableLoadError::DigestMismatch { .. })
        ));
    }

    #[test]
    fn parser_accepts_exact_two_and_three_moment_record_layouts() {
        let two_layout = TableLayout {
            shapes: 1,
            densities: 2,
            rimes: 2,
            masses: 2,
            rain_collisions: 2,
        };
        let two_text = synthetic_table(P3OfficialTableKind::TwoMoment, two_layout);
        let two = parse_synthetic(P3OfficialTableKind::TwoMoment, &two_text, two_layout).unwrap();
        let TablePayload::TwoMoment { lambda, mu } = two.payload else {
            panic!("wrong payload")
        };
        assert_eq!(lambda.len(), 8);
        assert_eq!(lambda[0], 100_111.0);
        assert_eq!(mu[7], 6.0);

        let three_layout = TableLayout {
            shapes: 2,
            ..two_layout
        };
        let three_text = synthetic_table(P3OfficialTableKind::ThreeMoment, three_layout);
        let three =
            parse_synthetic(P3OfficialTableKind::ThreeMoment, &three_text, three_layout).unwrap();
        let TablePayload::ThreeMoment {
            mean_density,
            lambda,
            mu,
        } = three.payload
        else {
            panic!("wrong payload")
        };
        assert_eq!(mean_density.len(), 16);
        assert_eq!(mean_density[0], 201.0);
        assert_eq!(lambda[15], 102_222.0);
        assert_eq!(mu[15], 8.0);
    }

    #[test]
    fn parser_rejects_truncation_extra_content_nonfinite_values_and_wrong_indices() {
        let layout = TableLayout {
            shapes: 1,
            densities: 1,
            rimes: 1,
            masses: 1,
            rain_collisions: 1,
        };
        let exact = synthetic_table(P3OfficialTableKind::TwoMoment, layout);
        let without_final_row = exact.trim_end().rsplit_once('\n').unwrap().0.to_owned() + "\n";
        assert!(matches!(
            parse_synthetic(P3OfficialTableKind::TwoMoment, &without_final_row, layout),
            Err(P3TableLoadError::UnexpectedEof { .. })
        ));

        let extra = exact.clone() + "unexpected\n";
        assert!(matches!(
            parse_synthetic(P3OfficialTableKind::TwoMoment, &extra, layout),
            Err(P3TableLoadError::ExtraContent { .. })
        ));

        let nonfinite = exact.replacen(" 101.25", " NaN", 1);
        assert!(matches!(
            parse_synthetic(P3OfficialTableKind::TwoMoment, &nonfinite, layout),
            Err(P3TableLoadError::NonFinite { .. })
        ));

        let wrong_index = exact.replacen("\n1 1 1 ", "\n1 1 2 ", 1);
        assert!(matches!(
            parse_synthetic(P3OfficialTableKind::TwoMoment, &wrong_index, layout),
            Err(P3TableLoadError::WrongIndex { .. })
        ));
    }

    #[test]
    fn coordinate_maps_retain_wrf_one_based_clamps_and_uneven_density_axis() {
        let rime = rime_axis(0.5).unwrap();
        assert_eq!(rime.lower, 1);
        assert_eq!(rime.coordinate, 2.5);
        assert!(!rime.clamped);

        let low_density = density_axis(0.0).unwrap();
        assert_eq!(low_density.lower, 0);
        assert_eq!(low_density.coordinate, 1.0);
        assert!(low_density.clamped);
        assert_eq!(density_axis(650.0).unwrap().coordinate, 4.0);
        let high_density = density_axis(900.0).unwrap();
        assert_eq!(high_density.lower, 3);
        assert_eq!(high_density.weight(), 1.0);

        let shape = shape_axis(6.0).unwrap();
        assert_eq!(shape.lower, 3);
        assert_eq!(shape.coordinate, 4.0);
    }

    #[test]
    fn interpolation_order_matches_official_two_moment_corner_vector() {
        let layout = TableLayout {
            shapes: 1,
            densities: 4,
            rimes: 3,
            masses: 11,
            rain_collisions: 1,
        };
        let mut lambda = vec![0.0; layout.densities * layout.rimes * layout.masses];
        for density in [2, 3] {
            for rime in [1, 2] {
                lambda[two_index(layout, density, rime, 9)] = 719_070.0;
                lambda[two_index(layout, density, rime, 10)] = 575_080.0;
            }
        }
        let value = interpolate_three_axes(
            &lambda,
            layout,
            LookupAxis {
                lower: 9,
                coordinate: 10.25,
                clamped: false,
            },
            LookupAxis {
                lower: 1,
                coordinate: 2.4,
                clamped: false,
            },
            LookupAxis {
                lower: 2,
                coordinate: 3.6,
                clamped: false,
            },
        );
        assert!((value - 683_072.5).abs() <= 0.01);
    }

    #[test]
    fn interpolation_order_matches_official_three_moment_corner_vector() {
        let layout = TableLayout {
            shapes: 5,
            densities: 4,
            rimes: 3,
            masses: 11,
            rain_collisions: 1,
        };
        let mut lambda = vec![0.0; layout.shapes * layout.densities * layout.rimes * layout.masses];
        let mut mu = lambda.clone();
        let mut mean_density = lambda.clone();
        for density in [2, 3] {
            for rime in [1, 2] {
                lambda[three_index(layout, 3, density, rime, 9)] = 719_070.0;
                lambda[three_index(layout, 3, density, rime, 10)] = 575_080.0;
                lambda[three_index(layout, 4, density, rime, 9)] = 900_290.0;
                lambda[three_index(layout, 4, density, rime, 10)] = 720_010.0;
                for mass in [9, 10] {
                    mu[three_index(layout, 3, density, rime, mass)] = 6.0;
                    mu[three_index(layout, 4, density, rime, mass)] = 8.0;
                    mean_density[three_index(layout, 3, density, rime, mass)] = 900.0;
                    mean_density[three_index(layout, 4, density, rime, mass)] = 900.0;
                }
            }
        }
        let shape = LookupAxis {
            lower: 3,
            coordinate: 4.25,
            clamped: false,
        };
        let mass = LookupAxis {
            lower: 9,
            coordinate: 10.25,
            clamped: false,
        };
        let rime = LookupAxis {
            lower: 1,
            coordinate: 2.4,
            clamped: false,
        };
        let density = LookupAxis {
            lower: 2,
            coordinate: 3.6,
            clamped: false,
        };
        let value = interpolate_four_axes(&lambda, layout, shape, mass, rime, density);
        assert!((f64::from(value) - 726_109.375_f64).abs() <= 0.01);
        assert!(
            (interpolate_four_axes(&mu, layout, shape, mass, rime, density) - 6.5).abs() <= 1e-6
        );
        assert_eq!(
            interpolate_four_axes(&mean_density, layout, shape, mass, rime, density),
            900.0
        );
    }

    #[test]
    fn compute_mu_piecewise_vectors_match_pinned_fortran_real_semantics() {
        for (g, expected) in [
            (20.0, 0.0),
            (15.0, 0.269_755_13),
            (10.0, 0.780_600_1),
            (5.0, 2.366_599_3),
            (3.5, 3.883_775_2),
            (2.0, 9.745_399),
            (1.6, 15.665_112),
            (1.3, 30.872_152),
            (1.2, 20.0),
        ] {
            let actual = compute_mu_3moment(g, 1.0, 1.0).unwrap();
            assert!((actual - expected).abs() <= 2.0e-5, "G={g}");
        }
    }
}
