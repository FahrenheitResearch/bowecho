//! CM1 NetCDF detection, inventory, and native Cartesian-plane reads.
//!
//! The schema in this module follows NCAR CM1's official `writeout_nc.F` at
//! commit `a33cd28c206adb010995f3ffb65aada150d9b1b9`. CM1 output is a local
//! Cartesian grid, not a projected latitude/longitude grid. In particular,
//! the `ctrlat` and `ctrlon` global attributes apply to the whole domain for
//! physics such as radiation and MUST NOT be silently treated as a map
//! projection or as the center of the output grid.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use netcrust::{DataType, File as NcFile, NcSliceInfo, NcSliceInfoElem, Variable as NcVariable};

/// Upstream source that defines the supported native-output schema.
pub const CM1_SCHEMA_SOURCE: &str =
    "https://github.com/NCAR/CM1/blob/a33cd28c206adb010995f3ffb65aada150d9b1b9/src/writeout_nc.F";

/// An available value, or an explicit reason it cannot be provided.
#[derive(Debug, Clone, PartialEq)]
pub enum Cm1Availability<T> {
    Available(T),
    Unavailable { reason: String },
}

impl<T> Cm1Availability<T> {
    pub fn as_ref(&self) -> Cm1Availability<&T> {
        match self {
            Self::Available(value) => Cm1Availability::Available(value),
            Self::Unavailable { reason } => Cm1Availability::Unavailable {
                reason: reason.clone(),
            },
        }
    }

    pub fn available(&self) -> Option<&T> {
        match self {
            Self::Available(value) => Some(value),
            Self::Unavailable { .. } => None,
        }
    }

    pub fn unavailable_reason(&self) -> Option<&str> {
        match self {
            Self::Available(_) => None,
            Self::Unavailable { reason } => Some(reason),
        }
    }
}

/// Strength of the metadata evidence that a file is native CM1 output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cm1DetectionConfidence {
    Confirmed,
    Probable,
    NotCm1,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cm1Detection {
    pub confidence: Cm1DetectionConfidence,
    pub evidence: Vec<String>,
    pub missing_evidence: Vec<String>,
}

impl Cm1Detection {
    pub fn is_cm1(&self) -> bool {
        self.confidence != Cm1DetectionConfidence::NotCm1
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cm1AxisGrid {
    ScalarX,
    StaggeredX,
    ScalarY,
    StaggeredY,
    ScalarZ,
    StaggeredZ,
}

/// Native NetCDF topology family emitted by an official CM1 release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cm1SchemaFamily {
    /// cm1r20.3 and newer: coordinate names are also dimension names
    /// (`xh/yh/zh`, `xf/yf/zf`).
    ModernR20Plus,
    /// cm1r18/r19 native topology: data dimensions are
    /// `ni/nj/nk` and `nip1/njp1/nkp1`, while coordinate variables are
    /// `xh/xf/yh/yf/z/zf`.
    LegacyR18R19,
    /// Optional legacy COARDS writer. Its degree labels are compatibility
    /// labels, not real geographic coordinates.
    LegacyCoards,
}

/// Logical CM1 grid axes mapped to the file's concrete dimension names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cm1GridTopology {
    pub family: Cm1SchemaFamily,
    pub scalar_x_dimension: String,
    pub staggered_x_dimension: String,
    pub scalar_y_dimension: String,
    pub staggered_y_dimension: String,
    pub scalar_z_dimension: String,
    pub staggered_z_dimension: String,
    pub coards_labels_are_non_geographic: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cm1Axis {
    pub name: String,
    pub grid: Cm1AxisGrid,
    pub source_units: Cm1Availability<String>,
    /// Values exactly as stored, retained even when CM1's legacy COARDS
    /// compatibility labels prevent a defensible conversion to metres.
    pub raw_values: Vec<f64>,
    /// Coordinates converted to metres. The raw values are not reinterpreted
    /// when the file omits or uses an unknown unit.
    pub values_m: Cm1Availability<Vec<f64>>,
    pub long_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cm1Axes {
    pub xh: Cm1Axis,
    pub xf: Cm1Axis,
    pub yh: Cm1Axis,
    pub yf: Cm1Axis,
    pub zh: Cm1Axis,
    pub zf: Cm1Axis,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cm1TimeAxis {
    pub dimension: String,
    pub variable: String,
    pub record_count: usize,
    pub source_units: Cm1Availability<String>,
    pub offsets_seconds: Cm1Availability<Vec<f64>>,
    /// RFC 3339 start time when all six official global date/time attributes
    /// are present and valid.
    pub simulation_start_utc: Cm1Availability<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cm1VariableRole {
    Coordinate,
    Time,
    Metadata,
    NativeScalar2D,
    NativeScalar3D,
    NativeXStaggered3D,
    NativeYStaggered3D,
    NativeZStaggered3D,
    Unsupported { reason: String },
}

impl Cm1VariableRole {
    pub fn is_plottable_scalar(&self) -> bool {
        matches!(self, Self::NativeScalar2D | Self::NativeScalar3D)
    }

    /// A field that BowEcho can place on the scalar x/y grid without
    /// inventing scientific semantics. Staggered CM1 vectors are included
    /// only because their official Arakawa-C locations define an exact
    /// adjacent-face average onto the scalar grid.
    pub fn is_horizontal_plane_compatible(&self) -> bool {
        matches!(
            self,
            Self::NativeScalar2D
                | Self::NativeScalar3D
                | Self::NativeXStaggered3D
                | Self::NativeYStaggered3D
                | Self::NativeZStaggered3D
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cm1Variable {
    pub name: String,
    pub dimensions: Vec<String>,
    pub shape: Vec<usize>,
    pub units: Option<String>,
    pub long_name: Option<String>,
    pub missing_value: Option<f64>,
    pub role: Cm1VariableRole,
}

/// Geographic metadata supplied by CM1. These are hints only; CM1 does not
/// define a map projection or latitude/longitude at each cell.
#[derive(Debug, Clone, PartialEq)]
pub struct Cm1GeographicHints {
    pub control_latitude_deg: Option<f64>,
    pub control_longitude_deg: Option<f64>,
    pub interpretation: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Cm1DomainMotion {
    /// No native moving-domain variables are present, or their velocities are
    /// identically zero. A user-supplied geographic anchor remains fixed.
    Static,
    /// Accumulated domain displacement exists as explicit, unit-bearing data.
    ExplicitDisplacement {
        east_m: Vec<f64>,
        north_m: Vec<f64>,
        east_source: String,
        north_source: String,
    },
    /// CM1 reports a moving frame via `umove`/`vmove`, but standard cm1out
    /// does not contain accumulated domain position. We deliberately do not
    /// integrate velocities and pretend that is authoritative geolocation.
    Unresolved { reason: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cm1MotionMetadata {
    pub east_velocity_mps: Cm1Availability<Vec<f64>>,
    pub north_velocity_mps: Cm1Availability<Vec<f64>>,
    pub domain_motion: Cm1DomainMotion,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cm1DiagnosticMotionAttachment {
    pub diagnostic_files_used: Vec<PathBuf>,
    pub matched_times_seconds: Vec<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cm1FileLayout {
    CompleteDomain {
        nx: usize,
        ny: usize,
    },
    /// `output_filetype=3`: one output time and one file per MPI process.
    /// These files must be assembled before plotting or 3-D processing.
    MpiTile {
        local_nx: usize,
        local_ny: usize,
        global_nx: usize,
        global_ny: usize,
        process_index: Option<u32>,
        output_index: Option<u32>,
    },
    Unresolved {
        reason: String,
    },
}

impl Cm1FileLayout {
    pub fn requires_tile_assembly(&self) -> bool {
        matches!(self, Self::MpiTile { .. })
    }
}

/// How a user explicitly chooses to place a local Cartesian CM1 domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cm1PlacementMode {
    /// Preserve the domain's world position. Requires explicit accumulated
    /// displacement for moving domains.
    FixedWorld,
    /// Keep the computational domain pinned to the chosen anchor as it moves.
    FollowDomain,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cm1Placement {
    pub mode: Cm1PlacementMode,
    pub anchor_latitude_deg: f64,
    pub anchor_longitude_deg: f64,
}

/// Latitude/longitude grid created by an explicit BowEcho placement choice.
/// It is not presented as native CM1 geolocation.
#[derive(Debug, Clone, PartialEq)]
pub struct Cm1GeoreferencedGrid {
    pub nx: usize,
    pub ny: usize,
    pub lat_deg: Vec<f32>,
    pub lon_deg: Vec<f32>,
    pub time_index: usize,
    pub placement: Cm1Placement,
    pub provenance: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cm1Inventory {
    pub source_path: PathBuf,
    pub detection: Cm1Detection,
    pub version: Option<String>,
    pub topology: Cm1GridTopology,
    pub dimensions: BTreeMap<String, usize>,
    pub axes: Cm1Axes,
    pub time: Cm1TimeAxis,
    pub variables: Vec<Cm1Variable>,
    pub motion: Cm1MotionMetadata,
    pub file_layout: Cm1FileLayout,
    pub geographic_hints: Cm1GeographicHints,
    /// Official CM1 global sentinel (normally -999999.9), preserved so reads
    /// can normalize it to NaN without inventing per-field fill semantics.
    pub missing_value: Option<f64>,
    /// Actual 3-D model-level height when terrain-following output includes
    /// the official `zhval` field. The 1-D `zh` coordinate remains nominal.
    pub physical_height_variable: Cm1Availability<String>,
}

impl Cm1Inventory {
    pub fn variable(&self, name: &str) -> Option<&Cm1Variable> {
        self.variables
            .iter()
            .find(|variable| variable.name.eq_ignore_ascii_case(name))
    }

    pub fn plottable_scalar_variables(&self) -> impl Iterator<Item = &Cm1Variable> {
        self.variables
            .iter()
            .filter(|variable| variable.role.is_plottable_scalar())
    }

    /// Native scalar fields plus staggered vector components that can be
    /// explicitly averaged onto the CM1 scalar grid.
    pub fn horizontal_plane_variables(&self) -> impl Iterator<Item = &Cm1Variable> {
        self.variables
            .iter()
            .filter(|variable| variable.role.is_horizontal_plane_compatible())
    }

    /// Native-domain offset relative to the user anchor for a particular
    /// output record. Follow-domain is always zero. Fixed-world fails closed
    /// for a moving file that lacks accumulated displacement.
    pub fn placement_offset_m(
        &self,
        mode: Cm1PlacementMode,
        time_index: usize,
    ) -> Result<(f64, f64), Cm1Error> {
        if time_index >= self.time.record_count {
            return Err(Cm1Error::TimeIndex {
                index: time_index,
                count: self.time.record_count,
            });
        }
        match mode {
            Cm1PlacementMode::FollowDomain => Ok((0.0, 0.0)),
            Cm1PlacementMode::FixedWorld => match &self.motion.domain_motion {
                Cm1DomainMotion::Static => Ok((0.0, 0.0)),
                Cm1DomainMotion::ExplicitDisplacement {
                    east_m, north_m, ..
                } => Ok((east_m[time_index], north_m[time_index])),
                Cm1DomainMotion::Unresolved { reason } => {
                    Err(Cm1Error::PlacementUnavailable(reason.clone()))
                }
            },
        }
    }
}

/// One native scalar plane in row-major `[y][x]` order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cm1PlaneTransform {
    NativeScalar,
    /// Adjacent xf faces averaged onto xh.
    DestaggeredX,
    /// Adjacent yf faces averaged onto yh.
    DestaggeredY,
    /// Adjacent zf faces averaged onto zh.
    DestaggeredZ,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cm1NativePlane {
    pub variable: String,
    pub units: Option<String>,
    pub long_name: Option<String>,
    pub time_index: usize,
    pub level_index: Option<usize>,
    pub nominal_level_m: Option<f64>,
    pub nx: usize,
    pub ny: usize,
    pub x_m: Vec<f64>,
    pub y_m: Vec<f64>,
    pub values: Vec<f64>,
    pub transform: Cm1PlaneTransform,
}

/// Physical height sampled at one CM1 scalar-grid column. CM1's official
/// writer calls `zhval` "height on model levels"; BowEcho deliberately does
/// not relabel that quantity as MSL because the native file does not declare
/// a vertical datum.
#[derive(Debug, Clone, PartialEq)]
pub struct Cm1ModelHeightColumn {
    pub variable: String,
    pub source_units: String,
    pub values_m: Vec<f64>,
    pub interpretation: String,
}

/// One efficient native CM1 vertical column on the scalar x/y grid. Values
/// are ordered bottom-to-top in the file's native scalar-z order. Horizontal
/// and vertical staggering is resolved only through the official adjacent-
/// face arithmetic means recorded by `transform`.
#[derive(Debug, Clone, PartialEq)]
pub struct Cm1NativeColumnProfile {
    pub variable: String,
    pub units: Option<String>,
    pub long_name: Option<String>,
    pub time_index: usize,
    pub x_index: usize,
    pub y_index: usize,
    pub local_x_m: f64,
    pub local_y_m: f64,
    pub nominal_level_m: Cm1Availability<Vec<f64>>,
    pub model_level_height_m: Cm1Availability<Cm1ModelHeightColumn>,
    pub values: Vec<f64>,
    pub transform: Cm1PlaneTransform,
    pub provenance: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cm1ThermodynamicField {
    pub variable: String,
    pub units: String,
    pub transform: Cm1PlaneTransform,
    pub interpretation: String,
}

/// Conversion from CM1's grid-relative horizontal velocities into the
/// east/north frame established by BowEcho's explicit local-tangent
/// placement.
#[derive(Debug, Clone, PartialEq)]
pub enum Cm1WindFrameCorrection {
    StationaryDomain,
    AddDomainVelocity {
        east_mps: Vec<f64>,
        north_mps: Vec<f64>,
        provenance: String,
    },
}

impl Cm1WindFrameCorrection {
    pub fn offset_at(&self, time_index: usize) -> Option<(f64, f64)> {
        match self {
            Self::StationaryDomain => Some((0.0, 0.0)),
            Self::AddDomainVelocity {
                east_mps,
                north_mps,
                ..
            } => Some((*east_mps.get(time_index)?, *north_mps.get(time_index)?)),
        }
    }
}

/// Evidence-only readiness report. No approximate alias or unproven unit is
/// silently accepted.
#[derive(Debug, Clone, PartialEq)]
pub struct Cm1ThermodynamicReadiness {
    pub potential_temperature: Cm1Availability<Cm1ThermodynamicField>,
    pub pressure: Cm1Availability<Cm1ThermodynamicField>,
    pub water_vapor_mixing_ratio: Cm1Availability<Cm1ThermodynamicField>,
    pub grid_relative_u: Cm1Availability<Cm1ThermodynamicField>,
    pub grid_relative_v: Cm1Availability<Cm1ThermodynamicField>,
    pub model_level_height: Cm1Availability<Cm1ThermodynamicField>,
    pub wind_frame_correction: Cm1Availability<Cm1WindFrameCorrection>,
    pub sounding_viewer: Cm1Availability<String>,
}

impl Cm1ThermodynamicReadiness {
    pub fn can_derive_native_profile(&self) -> bool {
        self.potential_temperature.available().is_some()
            && self.pressure.available().is_some()
            && self.water_vapor_mixing_ratio.available().is_some()
            && self.grid_relative_u.available().is_some()
            && self.grid_relative_v.available().is_some()
            && self.model_level_height.available().is_some()
            && self.wind_frame_correction.available().is_some()
    }
}

/// Thermodynamic constants selected for converting native CM1 potential
/// temperature and water-vapor mixing ratio. Official native output does not
/// record `testcase`; callers must explicitly opt into these defaults rather
/// than receiving them as an invisible assumption.
#[derive(Debug, Clone, PartialEq)]
pub struct Cm1ThermodynamicConstants {
    pub dry_air_gas_constant_j_kg_k: f64,
    pub dry_air_cp_j_kg_k: f64,
    pub water_vapor_gas_constant_j_kg_k: f64,
    pub reference_pressure_pa: f64,
    pub provenance: String,
}

impl Cm1ThermodynamicConstants {
    pub fn official_defaults() -> Self {
        Self {
            dry_air_gas_constant_j_kg_k: 287.04,
            dry_air_cp_j_kg_k: 1005.7,
            water_vapor_gas_constant_j_kg_k: 461.5,
            reference_pressure_pa: 100_000.0,
            provenance: format!(
                "NCAR CM1 default constants Rd=287.04, Cp=1005.7, Rv=461.5 from constants.F at {}; testcase 4/5 overrides are not identifiable because native output does not store testcase",
                CM1_SCHEMA_SOURCE
            ),
        }
    }

    fn validate(&self) -> Result<(), Cm1Error> {
        let values = [
            self.dry_air_gas_constant_j_kg_k,
            self.dry_air_cp_j_kg_k,
            self.water_vapor_gas_constant_j_kg_k,
            self.reference_pressure_pa,
        ];
        if values
            .iter()
            .any(|value| !value.is_finite() || *value <= 0.0)
            || self.dry_air_gas_constant_j_kg_k >= self.dry_air_cp_j_kg_k
        {
            return Err(Cm1Error::Thermodynamic(
                "thermodynamic constants must be finite and positive, with Rd < Cp".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cm1ThermodynamicColumn {
    pub time_index: usize,
    pub x_index: usize,
    pub y_index: usize,
    pub local_x_m: f64,
    pub local_y_m: f64,
    pub pressure_hpa: Vec<f64>,
    pub model_level_height_m: Vec<f64>,
    pub temperature_c: Vec<f64>,
    pub dewpoint_c: Vec<f64>,
    pub water_vapor_mixing_ratio_kg_kg: Vec<f64>,
    pub u_grid_relative_mps: Vec<f64>,
    pub v_grid_relative_mps: Vec<f64>,
    pub u_east_mps: Vec<f64>,
    pub v_north_mps: Vec<f64>,
    pub invalid_levels: Vec<(usize, String)>,
    pub constants: Cm1ThermodynamicConstants,
    pub provenance: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cm1TerrainPolicy {
    /// Require CM1's official `zs` surface-height field.
    RequireNative,
    /// Explicit user choice for an idealized flat domain when `zs` was not
    /// written. All terrain values become zero in the CM1 model-z datum.
    AssumeFlatModelZero,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cm1RadarFieldSources {
    pub reflectivity: String,
    pub model_height: String,
    pub terrain: String,
    pub u: String,
    pub v: String,
    pub w: String,
}

/// A validated CM1 scalar-grid atmosphere ready for BowEcho's polar radar
/// sampler. Heights and terrain deliberately retain the name `model_z`: CM1
/// does not declare an MSL datum.
#[derive(Debug, Clone, PartialEq)]
pub struct Cm1RadarScene {
    pub nx: usize,
    pub ny: usize,
    pub nz: usize,
    pub lat_deg: Vec<f32>,
    pub lon_deg: Vec<f32>,
    pub model_z_m: Vec<f32>,
    pub dbz: Vec<f32>,
    pub u_east_mps: Vec<f32>,
    pub v_north_mps: Vec<f32>,
    pub w_mps: Vec<f32>,
    pub terrain_model_z_m: Vec<f32>,
    pub dx_m: Option<f64>,
    pub valid_time_utc: DateTime<Utc>,
    pub time_index: usize,
    pub placement: Cm1Placement,
    pub flat_terrain_assumed: bool,
    pub field_sources: Cm1RadarFieldSources,
    pub provenance: String,
}

#[derive(Debug, thiserror::Error)]
pub enum Cm1Error {
    #[error("NetCDF read error: {0}")]
    Netcdf(#[from] netcrust::Error),

    #[error("not a supported native CM1 output file ({evidence})")]
    NotCm1 { evidence: String },

    #[error("CM1 file is missing required coordinate variable {0}")]
    MissingAxis(&'static str),

    #[error("CM1 coordinate variable {name} must be one-dimensional, got shape {shape:?}")]
    InvalidAxisShape { name: String, shape: Vec<usize> },

    #[error("CM1 coordinate variable {name} contains a non-finite value at index {index}")]
    NonFiniteCoordinate { name: String, index: usize },

    #[error("CM1 time variable is missing")]
    MissingTime,

    #[error("CM1 variable {0} was not inventoried")]
    UnknownVariable(String),

    #[error("CM1 variable {name} is not a native scalar field: {reason}")]
    UnsupportedScalar { name: String, reason: String },

    #[error("time index {index} is outside the {count}-record CM1 file")]
    TimeIndex { index: usize, count: usize },

    #[error("vertical level index {index} is outside the {count}-level CM1 field {name}")]
    LevelIndex {
        name: String,
        index: usize,
        count: usize,
    },

    #[error("CM1 native plane {name} retained unexpected dimensions {dimensions:?}")]
    PlaneShape {
        name: String,
        dimensions: Vec<String>,
    },

    #[error("CM1 scalar-grid column index ({x}, {y}) is outside {nx} x {ny}")]
    ColumnIndex {
        x: usize,
        y: usize,
        nx: usize,
        ny: usize,
    },

    #[error("CM1 native column {name} retained unexpected dimensions {dimensions:?}")]
    ColumnShape {
        name: String,
        dimensions: Vec<String>,
    },

    #[error("fixed-world CM1 placement is unavailable: {0}")]
    PlacementUnavailable(String),

    #[error("CM1 diagnostic motion metadata is unusable: {0}")]
    DiagnosticMotion(String),

    #[error("invalid CM1 geographic anchor: {0}")]
    InvalidAnchor(String),

    #[error("CM1 thermodynamic profile unavailable: {0}")]
    Thermodynamic(String),

    #[error("CM1 radar scene unavailable: {0}")]
    RadarScene(String),
}

/// Inspect a file from disk and return its native CM1 inventory.
pub fn inspect_path(path: impl AsRef<Path>) -> Result<Cm1Inventory, Cm1Error> {
    let path = path.as_ref();
    let nc = netcrust::open(path)?;
    inspect_file(&nc, path)
}

#[derive(Debug, Clone, Copy)]
struct TopologySpec {
    family: Cm1SchemaFamily,
    xh_variable: &'static str,
    xh_dimension: &'static str,
    xf_variable: &'static str,
    xf_dimension: &'static str,
    yh_variable: &'static str,
    yh_dimension: &'static str,
    yf_variable: &'static str,
    yf_dimension: &'static str,
    zh_variable: &'static str,
    zh_dimension: &'static str,
    zf_variable: &'static str,
    zf_dimension: &'static str,
    coards_labels_are_non_geographic: bool,
}

impl TopologySpec {
    fn public(self) -> Cm1GridTopology {
        Cm1GridTopology {
            family: self.family,
            scalar_x_dimension: self.xh_dimension.to_string(),
            staggered_x_dimension: self.xf_dimension.to_string(),
            scalar_y_dimension: self.yh_dimension.to_string(),
            staggered_y_dimension: self.yf_dimension.to_string(),
            scalar_z_dimension: self.zh_dimension.to_string(),
            staggered_z_dimension: self.zf_dimension.to_string(),
            coards_labels_are_non_geographic: self.coards_labels_are_non_geographic,
        }
    }

    fn axes(self) -> [(&'static str, &'static str, &'static str); 6] {
        [
            (
                self.xh_variable,
                self.xh_dimension,
                "west-east location of scalar grid points",
            ),
            (
                self.xf_variable,
                self.xf_dimension,
                "west-east location of staggered u grid points",
            ),
            (
                self.yh_variable,
                self.yh_dimension,
                "south-north location of scalar grid points",
            ),
            (
                self.yf_variable,
                self.yf_dimension,
                "south-north location of staggered v grid points",
            ),
            (
                self.zh_variable,
                self.zh_dimension,
                "nominal height of scalar grid points",
            ),
            (
                self.zf_variable,
                self.zf_dimension,
                "nominal height of staggered w grid points",
            ),
        ]
    }
}

const MODERN_TOPOLOGY: TopologySpec = TopologySpec {
    family: Cm1SchemaFamily::ModernR20Plus,
    xh_variable: "xh",
    xh_dimension: "xh",
    xf_variable: "xf",
    xf_dimension: "xf",
    yh_variable: "yh",
    yh_dimension: "yh",
    yf_variable: "yf",
    yf_dimension: "yf",
    zh_variable: "zh",
    zh_dimension: "zh",
    zf_variable: "zf",
    zf_dimension: "zf",
    coards_labels_are_non_geographic: false,
};

const LEGACY_TOPOLOGY: TopologySpec = TopologySpec {
    family: Cm1SchemaFamily::LegacyR18R19,
    xh_variable: "xh",
    xh_dimension: "ni",
    xf_variable: "xf",
    xf_dimension: "nip1",
    yh_variable: "yh",
    yh_dimension: "nj",
    yf_variable: "yf",
    yf_dimension: "njp1",
    zh_variable: "z",
    zh_dimension: "nk",
    zf_variable: "zf",
    zf_dimension: "nkp1",
    coards_labels_are_non_geographic: false,
};

const LEGACY_COARDS_TOPOLOGY: TopologySpec = TopologySpec {
    family: Cm1SchemaFamily::LegacyCoards,
    xh_variable: "ni",
    xh_dimension: "ni",
    xf_variable: "nip1",
    xf_dimension: "nip1",
    yh_variable: "nj",
    yh_dimension: "nj",
    yf_variable: "njp1",
    yf_dimension: "njp1",
    zh_variable: "nk",
    zh_dimension: "nk",
    zf_variable: "nkp1",
    zf_dimension: "nkp1",
    coards_labels_are_non_geographic: true,
};

fn topology_score(nc: &NcFile, variables: &[NcVariable], topology: TopologySpec) -> (usize, usize) {
    let mut axes = 0usize;
    let mut described = 0usize;
    for (variable_name, dimension_name, official_long_name) in topology.axes() {
        if let Some(variable) = variable_ci(variables, variable_name) {
            let dimensions = variable_dimension_names(variable);
            if dimensions.len() == 1 && dimensions[0].eq_ignore_ascii_case(dimension_name) {
                axes += 1;
                if variable_attr_string_ci(variable, "long_name").is_some_and(|value| {
                    value.eq_ignore_ascii_case(official_long_name)
                        || (matches!(topology.family, Cm1SchemaFamily::LegacyR18R19)
                            && variable_name == "z"
                            && value.to_ascii_lowercase().contains("height"))
                }) {
                    described += 1;
                }
                continue;
            }
        }

        // NetCDF-4 represents a coordinate whose variable and dimension have
        // the same name as an HDF5 dimension-scale dataset. `netcdf-reader`
        // currently omits those datasets from its variable index for files
        // written by current CM1, even though the data variables retain the
        // correct named dimensions. Accept only an exact official CM1 axis
        // recovered from netcrust's guarded raw-HDF5 metadata surface.
        if hdf5_axis_metadata(nc, variable_name, dimension_name, official_long_name).is_some() {
            axes += 1;
            described += 1;
        }
    }
    (axes, described)
}

fn select_topology(nc: &NcFile, variables: &[NcVariable]) -> Option<(TopologySpec, usize, usize)> {
    [MODERN_TOPOLOGY, LEGACY_TOPOLOGY, LEGACY_COARDS_TOPOLOGY]
        .into_iter()
        .map(|topology| {
            let (axes, described) = topology_score(nc, variables, topology);
            (topology, axes, described)
        })
        .max_by_key(|(topology, axes, described)| {
            (
                *axes,
                *described,
                matches!(topology.family, Cm1SchemaFamily::ModernR20Plus),
            )
        })
        .filter(|(_, axes, _)| *axes >= 3)
}

/// Detect native CM1 evidence without interpreting any meteorological field.
pub fn detect_file(nc: &NcFile) -> Result<Cm1Detection, Cm1Error> {
    let variables = nc.variables()?;
    let mut evidence = Vec::new();
    let mut missing = Vec::new();

    let version = global_attr_string_ci(nc, "CM1 version");
    let version_is_cm1 = version
        .as_deref()
        .is_some_and(|value| value.trim().to_ascii_lowercase().starts_with("cm1"));
    if let Some(version) = &version {
        evidence.push(format!("global CM1 version={version}"));
    } else {
        missing.push("global CM1 version attribute".to_string());
    }

    let selected_topology = select_topology(nc, &variables);
    let (axis_count, described_axis_count) = selected_topology
        .map(|(_, axes, described)| (axes, described))
        .unwrap_or_default();
    if let Some((topology, _, _)) = selected_topology {
        evidence.push(format!("{:?} native coordinate topology", topology.family));
    }
    if axis_count > 0 {
        evidence.push(format!("{axis_count}/6 native CM1 coordinate axes"));
    }
    if axis_count < 6 {
        missing.push(format!("{} native CM1 coordinate axes", 6 - axis_count));
    }
    if described_axis_count > 0 {
        evidence.push(format!(
            "{described_axis_count}/6 axes carry official CM1 descriptions"
        ));
    }

    let official_time = variable_ci(&variables, "time").is_some_and(|variable| {
        let dimensions = variable_dimension_names(variable);
        dimensions.len() == 1
            && dimensions[0].eq_ignore_ascii_case("time")
            && variable_attr_string_ci(variable, "long_name").is_some_and(|value| {
                value
                    .trim()
                    .eq_ignore_ascii_case("time since beginning of simulation")
            })
    }) || hdf5_time_axis_metadata(nc).is_some();
    if official_time {
        evidence.push("official CM1 time axis".to_string());
    } else {
        missing.push("official CM1 time axis".to_string());
    }

    let cm1_configuration_attrs = ["nx", "ny", "nz", "ptype", "imoist", "iorigin"]
        .into_iter()
        .filter(|name| global_attr_f64_ci(nc, name).is_some())
        .count();
    if cm1_configuration_attrs > 0 {
        evidence.push(format!(
            "{cm1_configuration_attrs}/6 CM1 configuration attributes"
        ));
    }

    let confidence = if version_is_cm1 && axis_count >= 3 && official_time {
        Cm1DetectionConfidence::Confirmed
    } else if axis_count == 6
        && described_axis_count >= 4
        && official_time
        && cm1_configuration_attrs >= 3
    {
        Cm1DetectionConfidence::Probable
    } else {
        Cm1DetectionConfidence::NotCm1
    };
    Ok(Cm1Detection {
        confidence,
        evidence,
        missing_evidence: missing,
    })
}

/// Build the complete metadata inventory for an already-open native CM1 file.
pub fn inspect_file(nc: &NcFile, source_path: &Path) -> Result<Cm1Inventory, Cm1Error> {
    let detection = detect_file(nc)?;
    if !detection.is_cm1() {
        return Err(Cm1Error::NotCm1 {
            evidence: detection.evidence.join("; "),
        });
    }
    let variables = nc.variables()?;
    let topology_spec = select_topology(nc, &variables)
        .map(|(topology, _, _)| topology)
        .ok_or_else(|| Cm1Error::NotCm1 {
            evidence: "no supported CM1 coordinate topology".to_string(),
        })?;
    let topology = topology_spec.public();
    let dimensions = nc
        .dimensions()?
        .into_iter()
        .map(|dimension| (dimension.name().to_string(), dimension.len()))
        .collect::<BTreeMap<_, _>>();

    let axes = Cm1Axes {
        xh: read_axis(
            nc,
            &variables,
            topology_spec.xh_variable,
            topology_spec.xh_dimension,
            Cm1AxisGrid::ScalarX,
            "x_units",
        )?,
        xf: read_axis(
            nc,
            &variables,
            topology_spec.xf_variable,
            topology_spec.xf_dimension,
            Cm1AxisGrid::StaggeredX,
            "x_units",
        )?,
        yh: read_axis(
            nc,
            &variables,
            topology_spec.yh_variable,
            topology_spec.yh_dimension,
            Cm1AxisGrid::ScalarY,
            "y_units",
        )?,
        yf: read_axis(
            nc,
            &variables,
            topology_spec.yf_variable,
            topology_spec.yf_dimension,
            Cm1AxisGrid::StaggeredY,
            "y_units",
        )?,
        zh: read_axis(
            nc,
            &variables,
            topology_spec.zh_variable,
            topology_spec.zh_dimension,
            Cm1AxisGrid::ScalarZ,
            "z_units",
        )?,
        zf: read_axis(
            nc,
            &variables,
            topology_spec.zf_variable,
            topology_spec.zf_dimension,
            Cm1AxisGrid::StaggeredZ,
            "z_units",
        )?,
    };
    let time = read_time_axis(nc, &variables)?;
    let missing_value = global_attr_f64_ci(nc, "missing_value");
    let inventoried_variables = variables
        .iter()
        .map(|variable| inventory_variable(variable, &topology, missing_value))
        .collect();
    let motion = read_motion(nc, &variables, time.record_count);
    let file_layout = classify_file_layout(nc, source_path, &dimensions, &topology);
    let physical_height_variable = variable_ci(&variables, "zhval")
        .filter(|variable| {
            matches!(
                classify_variable(variable, &topology),
                Cm1VariableRole::NativeScalar3D
            )
        })
        .map(|variable| Cm1Availability::Available(variable.name().to_string()))
        .unwrap_or_else(|| Cm1Availability::Unavailable {
            reason:
                "the optional official 3-D `zhval` field was not written; `zh` is nominal height"
                    .to_string(),
        });

    Ok(Cm1Inventory {
        source_path: source_path.to_path_buf(),
        detection,
        version: global_attr_string_ci(nc, "CM1 version"),
        topology,
        dimensions,
        axes,
        time,
        variables: inventoried_variables,
        motion,
        file_layout,
        geographic_hints: Cm1GeographicHints {
            control_latitude_deg: global_attr_f64_ci(nc, "ctrlat"),
            control_longitude_deg: global_attr_f64_ci(nc, "ctrlon"),
            interpretation: "CM1 documents ctrlat/ctrlon as applying to the entire domain; they are not a map projection or a cell geolocation. World placement requires an explicit user anchor."
                .to_string(),
        },
        missing_value,
        physical_height_variable,
    })
}

/// Discover official `cm1out_diag_XXXXXX.nc` files in one directory.
pub fn diagnostic_files_in_folder(folder: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut files = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(is_cm1_diagnostic_filename)
        })
        .collect::<Vec<_>>();
    files.sort();
    files
}

/// Attach exact accumulated moving-domain positions from official CM1
/// diagnostic files. Samples are matched by elapsed model time; this function
/// never integrates velocities and never interpolates across diagnostic times.
pub fn attach_motion_diagnostics(
    inventory: &mut Cm1Inventory,
    diagnostic_paths: &[PathBuf],
) -> Result<Cm1DiagnosticMotionAttachment, Cm1Error> {
    let output_times = inventory.time.offsets_seconds.available().ok_or_else(|| {
        Cm1Error::DiagnosticMotion(
            "the main output time axis cannot be converted to seconds".to_string(),
        )
    })?;
    let mut samples = Vec::new();
    for path in diagnostic_paths {
        samples.push(read_diagnostic_motion_sample(path)?);
    }
    let mut east_m = Vec::with_capacity(output_times.len());
    let mut north_m = Vec::with_capacity(output_times.len());
    let mut east_velocity = Vec::with_capacity(output_times.len());
    let mut north_velocity = Vec::with_capacity(output_times.len());
    let mut used = Vec::with_capacity(output_times.len());
    for &output_time in output_times {
        let matching = samples
            .iter()
            .filter(|sample| elapsed_times_match(sample.time_seconds, output_time))
            .collect::<Vec<_>>();
        let sample = match matching.as_slice() {
            [sample] => *sample,
            [] => {
                return Err(Cm1Error::DiagnosticMotion(format!(
                    "no diagnostic file exactly matches main-output time {output_time} s"
                )));
            }
            _ => {
                return Err(Cm1Error::DiagnosticMotion(format!(
                    "more than one diagnostic file matches main-output time {output_time} s"
                )));
            }
        };
        east_m.push(sample.east_m);
        north_m.push(sample.north_m);
        east_velocity.push(sample.east_velocity_mps);
        north_velocity.push(sample.north_velocity_mps);
        used.push(sample.path.clone());
    }
    let source_list = used
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join("; ");
    inventory.motion.east_velocity_mps = Cm1Availability::Available(east_velocity);
    inventory.motion.north_velocity_mps = Cm1Availability::Available(north_velocity);
    inventory.motion.domain_motion = Cm1DomainMotion::ExplicitDisplacement {
        east_m,
        north_m,
        east_source: format!("domainlocx from {source_list}"),
        north_source: format!("domainlocy from {source_list}"),
    };
    Ok(Cm1DiagnosticMotionAttachment {
        diagnostic_files_used: used,
        matched_times_seconds: output_times.clone(),
    })
}

/// Place the scalar CM1 x/y grid on a spherical Earth using a user-supplied
/// domain-center anchor. CM1 provides no source map projection; this explicit
/// local-tangent placement is BowEcho metadata and retains that provenance.
pub fn georeference_scalar_grid(
    inventory: &Cm1Inventory,
    placement: &Cm1Placement,
    time_index: usize,
) -> Result<Cm1GeoreferencedGrid, Cm1Error> {
    validate_anchor(placement)?;
    let x_m = required_axis_values(&inventory.axes.xh)?;
    let y_m = required_axis_values(&inventory.axes.yh)?;
    let xf_m = required_axis_values(&inventory.axes.xf)?;
    let yf_m = required_axis_values(&inventory.axes.yf)?;
    let x_center_m = axis_bounds_center(xf_m, "xf")?;
    let y_center_m = axis_bounds_center(yf_m, "yf")?;
    let (domain_east_m, domain_north_m) =
        inventory.placement_offset_m(placement.mode, time_index)?;
    let mut lat_deg = Vec::with_capacity(x_m.len() * y_m.len());
    let mut lon_deg = Vec::with_capacity(x_m.len() * y_m.len());
    for &y in y_m {
        for &x in x_m {
            let east_m = x - x_center_m + domain_east_m;
            let north_m = y - y_center_m + domain_north_m;
            let (lat, lon) = spherical_destination(
                placement.anchor_latitude_deg,
                placement.anchor_longitude_deg,
                east_m,
                north_m,
            );
            lat_deg.push(lat as f32);
            lon_deg.push(lon as f32);
        }
    }
    Ok(Cm1GeoreferencedGrid {
        nx: x_m.len(),
        ny: y_m.len(),
        lat_deg,
        lon_deg,
        time_index,
        placement: placement.clone(),
        provenance: format!(
            "BowEcho local-tangent spherical placement; user anchor is CM1 domain center; mode={:?}; source CM1 has no map projection",
            placement.mode
        ),
    })
}

#[derive(Debug, Clone)]
struct DiagnosticMotionSample {
    path: PathBuf,
    time_seconds: f64,
    east_m: f64,
    north_m: f64,
    east_velocity_mps: f64,
    north_velocity_mps: f64,
}

fn is_cm1_diagnostic_filename(name: &str) -> bool {
    let Some(index) = name
        .strip_prefix("cm1out_diag_")
        .and_then(|value| value.strip_suffix(".nc"))
    else {
        return false;
    };
    index.len() == 6 && index.bytes().all(|byte| byte.is_ascii_digit())
}

fn read_diagnostic_motion_sample(path: &Path) -> Result<DiagnosticMotionSample, Cm1Error> {
    let nc = netcrust::open(path)?;
    if !path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(is_cm1_diagnostic_filename)
    {
        return Err(Cm1Error::DiagnosticMotion(format!(
            "{} does not use the official cm1out_diag_XXXXXX.nc name",
            path.display()
        )));
    }
    let time_variable = nc.variable("time").ok_or_else(|| {
        Cm1Error::DiagnosticMotion(format!("{} has no time variable", path.display()))
    })?;
    let time_units = variable_attr_string_ci(&time_variable, "units").ok_or_else(|| {
        Cm1Error::DiagnosticMotion(format!("{} time has no units", path.display()))
    })?;
    let time_scale = time_scale_to_seconds(time_units).ok_or_else(|| {
        Cm1Error::DiagnosticMotion(format!(
            "{} time uses unsupported units `{time_units}`",
            path.display()
        ))
    })?;
    let time_seconds = read_single_numeric_value(&time_variable, path)? * time_scale;
    let east_m = read_diagnostic_scaled_value(&nc, path, "domainlocx", length_scale_to_metres)?;
    let north_m = read_diagnostic_scaled_value(&nc, path, "domainlocy", length_scale_to_metres)?;
    let east_velocity_mps =
        read_diagnostic_scaled_value(&nc, path, "umove", velocity_scale_to_mps)?;
    let north_velocity_mps =
        read_diagnostic_scaled_value(&nc, path, "vmove", velocity_scale_to_mps)?;
    Ok(DiagnosticMotionSample {
        path: path.to_path_buf(),
        time_seconds,
        east_m,
        north_m,
        east_velocity_mps,
        north_velocity_mps,
    })
}

fn read_diagnostic_scaled_value(
    nc: &NcFile,
    path: &Path,
    name: &str,
    unit_scale: fn(&str) -> Option<f64>,
) -> Result<f64, Cm1Error> {
    let variable = nc.variable(name).ok_or_else(|| {
        Cm1Error::DiagnosticMotion(format!("{} has no `{name}` variable", path.display()))
    })?;
    let units = variable_attr_string_ci(&variable, "units").ok_or_else(|| {
        Cm1Error::DiagnosticMotion(format!("{} `{name}` has no units", path.display()))
    })?;
    let scale = unit_scale(units).ok_or_else(|| {
        Cm1Error::DiagnosticMotion(format!(
            "{} `{name}` uses unsupported units `{units}`",
            path.display()
        ))
    })?;
    Ok(read_single_numeric_value(&variable, path)? * scale)
}

fn read_single_numeric_value(variable: &NcVariable, path: &Path) -> Result<f64, Cm1Error> {
    let array = variable.array_f64()?;
    if array.len() != 1 {
        return Err(Cm1Error::DiagnosticMotion(format!(
            "{} `{}` has {} values; official diagnostic files contain exactly one output time",
            path.display(),
            variable.name(),
            array.len()
        )));
    }
    let value = array.values()[0];
    if !value.is_finite() {
        return Err(Cm1Error::DiagnosticMotion(format!(
            "{} `{}` is not finite",
            path.display(),
            variable.name()
        )));
    }
    Ok(value)
}

fn elapsed_times_match(left: f64, right: f64) -> bool {
    let tolerance = 1.0e-3_f64.max(f64::EPSILON * 16.0 * left.abs().max(right.abs()));
    (left - right).abs() <= tolerance
}

fn validate_anchor(placement: &Cm1Placement) -> Result<(), Cm1Error> {
    if !placement.anchor_latitude_deg.is_finite()
        || !(-90.0..=90.0).contains(&placement.anchor_latitude_deg)
    {
        return Err(Cm1Error::InvalidAnchor(format!(
            "latitude {} is outside [-90, 90]",
            placement.anchor_latitude_deg
        )));
    }
    if !placement.anchor_longitude_deg.is_finite()
        || !(-180.0..=180.0).contains(&placement.anchor_longitude_deg)
    {
        return Err(Cm1Error::InvalidAnchor(format!(
            "longitude {} is outside [-180, 180]",
            placement.anchor_longitude_deg
        )));
    }
    Ok(())
}

fn axis_bounds_center(values: &[f64], name: &str) -> Result<f64, Cm1Error> {
    let (Some(first), Some(last)) = (values.first(), values.last()) else {
        return Err(Cm1Error::UnsupportedScalar {
            name: name.to_string(),
            reason: "coordinate axis is empty".to_string(),
        });
    };
    Ok(0.5 * (first + last))
}

fn spherical_destination(
    anchor_lat_deg: f64,
    anchor_lon_deg: f64,
    east_m: f64,
    north_m: f64,
) -> (f64, f64) {
    const MEAN_EARTH_RADIUS_M: f64 = 6_371_008.8;
    let distance_m = east_m.hypot(north_m);
    if distance_m == 0.0 {
        return (anchor_lat_deg, anchor_lon_deg);
    }
    let angular_distance = distance_m / MEAN_EARTH_RADIUS_M;
    let bearing = east_m.atan2(north_m);
    let lat1 = anchor_lat_deg.to_radians();
    let lon1 = anchor_lon_deg.to_radians();
    let lat2 = (lat1.sin() * angular_distance.cos()
        + lat1.cos() * angular_distance.sin() * bearing.cos())
    .clamp(-1.0, 1.0)
    .asin();
    let lon2 = lon1
        + (bearing.sin() * angular_distance.sin() * lat1.cos())
            .atan2(angular_distance.cos() - lat1.sin() * lat2.sin());
    let lon_deg = (lon2.to_degrees() + 540.0).rem_euclid(360.0) - 180.0;
    (lat2.to_degrees(), lon_deg)
}

/// Read any inventoried native scalar field at one time and, for 3-D fields,
/// one nominal model level. Values are normalized to row-major `[y][x]`.
pub fn read_native_scalar_plane(
    nc: &NcFile,
    inventory: &Cm1Inventory,
    variable_name: &str,
    time_index: usize,
    level_index: Option<usize>,
) -> Result<Cm1NativePlane, Cm1Error> {
    if time_index >= inventory.time.record_count {
        return Err(Cm1Error::TimeIndex {
            index: time_index,
            count: inventory.time.record_count,
        });
    }
    let metadata = inventory
        .variable(variable_name)
        .ok_or_else(|| Cm1Error::UnknownVariable(variable_name.to_string()))?;
    let is_3d = match &metadata.role {
        Cm1VariableRole::NativeScalar2D => false,
        Cm1VariableRole::NativeScalar3D => true,
        other => {
            return Err(Cm1Error::UnsupportedScalar {
                name: metadata.name.clone(),
                reason: format!("inventoried role is {other:?}"),
            });
        }
    };
    if nc.variable(&metadata.name).is_none() {
        return Err(Cm1Error::UnknownVariable(metadata.name.clone()));
    }
    let vertical_count = if is_3d {
        metadata
            .dimensions
            .iter()
            .zip(metadata.shape.iter())
            .find(|(dimension, _)| {
                dimension.eq_ignore_ascii_case(&inventory.topology.scalar_z_dimension)
            })
            .map(|(_, &length)| length)
            .unwrap_or(0)
    } else {
        0
    };
    let selected_level = if is_3d {
        let selected = level_index.unwrap_or(0);
        if selected >= vertical_count {
            return Err(Cm1Error::LevelIndex {
                name: metadata.name.clone(),
                index: selected,
                count: vertical_count,
            });
        }
        Some(selected)
    } else {
        None
    };

    let mut selection = Vec::with_capacity(metadata.dimensions.len());
    let mut remaining_dimensions = Vec::new();
    for dimension in &metadata.dimensions {
        if dimension.eq_ignore_ascii_case("time") {
            selection.push(NcSliceInfoElem::Index(time_index as u64));
        } else if dimension.eq_ignore_ascii_case(&inventory.topology.scalar_z_dimension) {
            selection.push(NcSliceInfoElem::Index(
                selected_level.expect("3-D role has a scalar-z dimension") as u64,
            ));
        } else {
            selection.push(NcSliceInfoElem::Slice {
                start: 0,
                end: u64::MAX,
                step: 1,
            });
            remaining_dimensions.push(dimension.clone());
        }
    }
    if remaining_dimensions.len() != 2
        || !remaining_dimensions
            .iter()
            .any(|dimension| dimension.eq_ignore_ascii_case(&inventory.topology.scalar_x_dimension))
        || !remaining_dimensions
            .iter()
            .any(|dimension| dimension.eq_ignore_ascii_case(&inventory.topology.scalar_y_dimension))
    {
        return Err(Cm1Error::PlaneShape {
            name: metadata.name.clone(),
            dimensions: remaining_dimensions,
        });
    }
    let array = nc.read_array_f64_slice(
        &metadata.name,
        &NcSliceInfo {
            selections: selection,
        },
    )?;
    let x_m = required_axis_values(&inventory.axes.xh)?;
    let y_m = required_axis_values(&inventory.axes.yh)?;
    let nx = x_m.len();
    let ny = y_m.len();
    let mut values =
        if remaining_dimensions[0].eq_ignore_ascii_case(&inventory.topology.scalar_y_dimension) {
            if array.shape() != [ny, nx] {
                return Err(Cm1Error::PlaneShape {
                    name: metadata.name.clone(),
                    dimensions: remaining_dimensions,
                });
            }
            array.into_values()
        } else {
            if array.shape() != [nx, ny] {
                return Err(Cm1Error::PlaneShape {
                    name: metadata.name.clone(),
                    dimensions: remaining_dimensions,
                });
            }
            let source = array.into_values();
            let mut transposed = vec![0.0; nx * ny];
            for x in 0..nx {
                for y in 0..ny {
                    transposed[y * nx + x] = source[x * ny + y];
                }
            }
            transposed
        };
    if let Some(missing) = metadata.missing_value {
        for value in &mut values {
            if value.to_bits() == missing.to_bits() || *value == missing {
                *value = f64::NAN;
            }
        }
    }
    let nominal_level_m = selected_level.and_then(|level| {
        inventory
            .axes
            .zh
            .values_m
            .available()
            .and_then(|values| values.get(level).copied())
    });
    Ok(Cm1NativePlane {
        variable: metadata.name.clone(),
        units: metadata.units.clone(),
        long_name: metadata.long_name.clone(),
        time_index,
        level_index: selected_level,
        nominal_level_m,
        nx,
        ny,
        x_m: x_m.to_vec(),
        y_m: y_m.to_vec(),
        values,
        transform: Cm1PlaneTransform::NativeScalar,
    })
}

/// Read a native CM1 field onto the scalar x/y grid. Scalar fields are
/// unchanged. Official x/y/z-staggered 3-D fields are centred with the
/// standard adjacent-face arithmetic mean; that transform is returned in
/// [`Cm1NativePlane::transform`] and must remain visible in provenance/UI.
pub fn read_horizontal_mass_grid_plane(
    nc: &NcFile,
    inventory: &Cm1Inventory,
    variable_name: &str,
    time_index: usize,
    level_index: Option<usize>,
) -> Result<Cm1NativePlane, Cm1Error> {
    let metadata = inventory
        .variable(variable_name)
        .ok_or_else(|| Cm1Error::UnknownVariable(variable_name.to_string()))?;
    match metadata.role {
        Cm1VariableRole::NativeScalar2D | Cm1VariableRole::NativeScalar3D => {
            read_native_scalar_plane(nc, inventory, variable_name, time_index, level_index)
        }
        Cm1VariableRole::NativeXStaggered3D
        | Cm1VariableRole::NativeYStaggered3D
        | Cm1VariableRole::NativeZStaggered3D => read_destaggered_plane(
            nc,
            inventory,
            metadata,
            time_index,
            level_index.unwrap_or(0),
        ),
        ref other => Err(Cm1Error::UnsupportedScalar {
            name: metadata.name.clone(),
            reason: format!("inventoried role is {other:?}"),
        }),
    }
}

/// Read one native 3-D CM1 field as a scalar-grid vertical column without
/// loading a full horizontal plane at every level. This is the first-class
/// profile primitive used by the CM1 UI; it preserves native model levels and
/// does not invent pressure coordinates or a vertical datum.
pub fn read_native_column_profile(
    nc: &NcFile,
    inventory: &Cm1Inventory,
    variable_name: &str,
    time_index: usize,
    x_index: usize,
    y_index: usize,
) -> Result<Cm1NativeColumnProfile, Cm1Error> {
    if time_index >= inventory.time.record_count {
        return Err(Cm1Error::TimeIndex {
            index: time_index,
            count: inventory.time.record_count,
        });
    }
    let x_m = required_axis_values(&inventory.axes.xh)?;
    let y_m = required_axis_values(&inventory.axes.yh)?;
    if x_index >= x_m.len() || y_index >= y_m.len() {
        return Err(Cm1Error::ColumnIndex {
            x: x_index,
            y: y_index,
            nx: x_m.len(),
            ny: y_m.len(),
        });
    }
    let metadata = inventory
        .variable(variable_name)
        .ok_or_else(|| Cm1Error::UnknownVariable(variable_name.to_string()))?;
    let nz = inventory.axes.zh.raw_values.len();
    let (values, transform) = match metadata.role {
        Cm1VariableRole::NativeScalar3D => (
            read_vertical_line(
                nc,
                metadata,
                time_index,
                &inventory.topology.scalar_z_dimension,
                &inventory.topology.scalar_x_dimension,
                x_index,
                &inventory.topology.scalar_y_dimension,
                y_index,
                nz,
            )?,
            Cm1PlaneTransform::NativeScalar,
        ),
        Cm1VariableRole::NativeXStaggered3D => {
            let raw_nx = inventory.axes.xf.raw_values.len();
            if raw_nx != x_m.len().saturating_add(1) {
                return Err(Cm1Error::ColumnShape {
                    name: metadata.name.clone(),
                    dimensions: metadata.dimensions.clone(),
                });
            }
            let west = read_vertical_line(
                nc,
                metadata,
                time_index,
                &inventory.topology.scalar_z_dimension,
                &inventory.topology.staggered_x_dimension,
                x_index,
                &inventory.topology.scalar_y_dimension,
                y_index,
                nz,
            )?;
            let east = read_vertical_line(
                nc,
                metadata,
                time_index,
                &inventory.topology.scalar_z_dimension,
                &inventory.topology.staggered_x_dimension,
                x_index + 1,
                &inventory.topology.scalar_y_dimension,
                y_index,
                nz,
            )?;
            (
                west.into_iter()
                    .zip(east)
                    .map(|(west, east)| 0.5 * (west + east))
                    .collect(),
                Cm1PlaneTransform::DestaggeredX,
            )
        }
        Cm1VariableRole::NativeYStaggered3D => {
            let raw_ny = inventory.axes.yf.raw_values.len();
            if raw_ny != y_m.len().saturating_add(1) {
                return Err(Cm1Error::ColumnShape {
                    name: metadata.name.clone(),
                    dimensions: metadata.dimensions.clone(),
                });
            }
            let south = read_vertical_line(
                nc,
                metadata,
                time_index,
                &inventory.topology.scalar_z_dimension,
                &inventory.topology.scalar_x_dimension,
                x_index,
                &inventory.topology.staggered_y_dimension,
                y_index,
                nz,
            )?;
            let north = read_vertical_line(
                nc,
                metadata,
                time_index,
                &inventory.topology.scalar_z_dimension,
                &inventory.topology.scalar_x_dimension,
                x_index,
                &inventory.topology.staggered_y_dimension,
                y_index + 1,
                nz,
            )?;
            (
                south
                    .into_iter()
                    .zip(north)
                    .map(|(south, north)| 0.5 * (south + north))
                    .collect(),
                Cm1PlaneTransform::DestaggeredY,
            )
        }
        Cm1VariableRole::NativeZStaggered3D => {
            let raw_nz = inventory.axes.zf.raw_values.len();
            if raw_nz != nz.saturating_add(1) {
                return Err(Cm1Error::ColumnShape {
                    name: metadata.name.clone(),
                    dimensions: metadata.dimensions.clone(),
                });
            }
            let faces = read_vertical_line(
                nc,
                metadata,
                time_index,
                &inventory.topology.staggered_z_dimension,
                &inventory.topology.scalar_x_dimension,
                x_index,
                &inventory.topology.scalar_y_dimension,
                y_index,
                raw_nz,
            )?;
            (
                faces
                    .windows(2)
                    .map(|faces| 0.5 * (faces[0] + faces[1]))
                    .collect(),
                Cm1PlaneTransform::DestaggeredZ,
            )
        }
        ref other => {
            return Err(Cm1Error::UnsupportedScalar {
                name: metadata.name.clone(),
                reason: format!("a vertical profile requires a 3-D field, got {other:?}"),
            });
        }
    };
    let nominal_level_m = inventory.axes.zh.values_m.clone();
    if nominal_level_m
        .available()
        .is_some_and(|levels| levels.len() != values.len())
    {
        return Err(Cm1Error::ColumnShape {
            name: metadata.name.clone(),
            dimensions: metadata.dimensions.clone(),
        });
    }
    let model_level_height_m =
        read_model_height_column(nc, inventory, time_index, x_index, y_index, values.len());
    Ok(Cm1NativeColumnProfile {
        variable: metadata.name.clone(),
        units: metadata.units.clone(),
        long_name: metadata.long_name.clone(),
        time_index,
        x_index,
        y_index,
        local_x_m: x_m[x_index],
        local_y_m: y_m[y_index],
        nominal_level_m,
        model_level_height_m,
        values,
        transform,
        provenance: format!(
            "native CM1 scalar-grid column; transform={transform:?}; vertical order and nominal zh preserved; no pressure interpolation; no MSL datum inferred"
        ),
    })
}

/// Report whether one CM1 file contains the exact, unit-bearing fields needed
/// for a meteorological column. This never substitutes perturbation fields or
/// 2-D products for the official total 3-D quantities.
pub fn thermodynamic_readiness(inventory: &Cm1Inventory) -> Cm1ThermodynamicReadiness {
    let potential_temperature = bind_exact_thermodynamic_field(
        inventory,
        "th",
        &[Cm1VariableRole::NativeScalar3D],
        "K",
        "official total potential temperature",
    );
    let pressure = bind_exact_thermodynamic_field(
        inventory,
        "prs",
        &[Cm1VariableRole::NativeScalar3D],
        "Pa",
        "official total pressure",
    );
    let water_vapor_mixing_ratio = bind_exact_thermodynamic_field(
        inventory,
        "qv",
        &[Cm1VariableRole::NativeScalar3D],
        "kg/kg",
        "official water-vapor mixing ratio",
    );
    let grid_relative_u = bind_first_thermodynamic_field(
        inventory,
        &[
            (
                "uinterp",
                &[Cm1VariableRole::NativeScalar3D][..],
                "official scalar-grid u (grid-relative)",
            ),
            (
                "u",
                &[Cm1VariableRole::NativeXStaggered3D][..],
                "official x-staggered u, adjacent-face averaged (grid-relative)",
            ),
        ],
        "m/s",
    );
    let grid_relative_v = bind_first_thermodynamic_field(
        inventory,
        &[
            (
                "vinterp",
                &[Cm1VariableRole::NativeScalar3D][..],
                "official scalar-grid v (grid-relative)",
            ),
            (
                "v",
                &[Cm1VariableRole::NativeYStaggered3D][..],
                "official y-staggered v, adjacent-face averaged (grid-relative)",
            ),
        ],
        "m/s",
    );
    let model_level_height = match &inventory.physical_height_variable {
        Cm1Availability::Available(name) => bind_length_field(
            inventory,
            name,
            "official physical height on model levels; vertical datum is undeclared",
        ),
        Cm1Availability::Unavailable { reason } => Cm1Availability::Unavailable {
            reason: reason.clone(),
        },
    };
    let wind_frame_correction = wind_frame_correction(inventory);
    let core_ready = potential_temperature.available().is_some()
        && pressure.available().is_some()
        && water_vapor_mixing_ratio.available().is_some()
        && grid_relative_u.available().is_some()
        && grid_relative_v.available().is_some()
        && model_level_height.available().is_some()
        && wind_frame_correction.available().is_some();
    let sounding_viewer = Cm1Availability::Unavailable {
        reason: if core_ready {
            "native thermodynamic profile is derivable, but CM1 zhval does not declare an MSL datum and the current Sounding viewer labels its height input MSL; BowEcho will not silently mislabel model-z"
                .to_string()
        } else {
            "one or more required native thermodynamic fields, units, heights, or wind-frame corrections are unavailable"
                .to_string()
        },
    };
    Cm1ThermodynamicReadiness {
        potential_temperature,
        pressure,
        water_vapor_mixing_ratio,
        grid_relative_u,
        grid_relative_v,
        model_level_height,
        wind_frame_correction,
        sounding_viewer,
    }
}

fn wind_frame_correction(inventory: &Cm1Inventory) -> Cm1Availability<Cm1WindFrameCorrection> {
    match &inventory.motion.domain_motion {
        Cm1DomainMotion::Static => {
            Cm1Availability::Available(Cm1WindFrameCorrection::StationaryDomain)
        }
        Cm1DomainMotion::ExplicitDisplacement { .. } | Cm1DomainMotion::Unresolved { .. } => {
            match (
                inventory.motion.east_velocity_mps.available(),
                inventory.motion.north_velocity_mps.available(),
            ) {
                (Some(east_mps), Some(north_mps))
                    if east_mps.len() == inventory.time.record_count
                        && north_mps.len() == inventory.time.record_count =>
                {
                    Cm1Availability::Available(Cm1WindFrameCorrection::AddDomainVelocity {
                        east_mps: east_mps.clone(),
                        north_mps: north_mps.clone(),
                        provenance: "official CM1 umove/vmove added to grid-relative u/v"
                            .to_string(),
                    })
                }
                _ => Cm1Availability::Unavailable {
                    reason: "moving-domain u/v cannot be converted to east/north winds because complete unit-bearing umove/vmove records are unavailable"
                        .to_string(),
                },
            }
        }
    }
}

fn bind_first_thermodynamic_field(
    inventory: &Cm1Inventory,
    candidates: &[(&str, &[Cm1VariableRole], &str)],
    expected_units: &str,
) -> Cm1Availability<Cm1ThermodynamicField> {
    let mut reasons = Vec::new();
    for &(name, roles, interpretation) in candidates {
        match bind_exact_thermodynamic_field(inventory, name, roles, expected_units, interpretation)
        {
            Cm1Availability::Available(field) => return Cm1Availability::Available(field),
            Cm1Availability::Unavailable { reason } => reasons.push(reason),
        }
    }
    Cm1Availability::Unavailable {
        reason: reasons.join("; "),
    }
}

fn bind_exact_thermodynamic_field(
    inventory: &Cm1Inventory,
    name: &str,
    expected_roles: &[Cm1VariableRole],
    expected_units: &str,
    interpretation: &str,
) -> Cm1Availability<Cm1ThermodynamicField> {
    let Some(variable) = inventory.variable(name) else {
        return Cm1Availability::Unavailable {
            reason: format!("required CM1 field `{name}` is absent"),
        };
    };
    if !expected_roles.contains(&variable.role) {
        return Cm1Availability::Unavailable {
            reason: format!(
                "CM1 field `{}` has role {:?}, expected one of {:?}",
                variable.name, variable.role, expected_roles
            ),
        };
    }
    let Some(units) = variable.units.as_deref() else {
        return Cm1Availability::Unavailable {
            reason: format!("CM1 field `{}` has no units", variable.name),
        };
    };
    if !units.trim().eq_ignore_ascii_case(expected_units) {
        return Cm1Availability::Unavailable {
            reason: format!(
                "CM1 field `{}` uses units `{units}`, expected official `{expected_units}`",
                variable.name
            ),
        };
    }
    let transform = match variable.role {
        Cm1VariableRole::NativeScalar3D => Cm1PlaneTransform::NativeScalar,
        Cm1VariableRole::NativeXStaggered3D => Cm1PlaneTransform::DestaggeredX,
        Cm1VariableRole::NativeYStaggered3D => Cm1PlaneTransform::DestaggeredY,
        Cm1VariableRole::NativeZStaggered3D => Cm1PlaneTransform::DestaggeredZ,
        _ => unreachable!("role validated above"),
    };
    Cm1Availability::Available(Cm1ThermodynamicField {
        variable: variable.name.clone(),
        units: units.to_string(),
        transform,
        interpretation: interpretation.to_string(),
    })
}

fn bind_length_field(
    inventory: &Cm1Inventory,
    name: &str,
    interpretation: &str,
) -> Cm1Availability<Cm1ThermodynamicField> {
    let Some(variable) = inventory.variable(name) else {
        return Cm1Availability::Unavailable {
            reason: format!("required CM1 model-height field `{name}` is absent"),
        };
    };
    if variable.role != Cm1VariableRole::NativeScalar3D {
        return Cm1Availability::Unavailable {
            reason: format!(
                "CM1 model-height field `{}` has role {:?}, expected NativeScalar3D",
                variable.name, variable.role
            ),
        };
    }
    let Some(units) = variable.units.as_deref() else {
        return Cm1Availability::Unavailable {
            reason: format!("CM1 model-height field `{}` has no units", variable.name),
        };
    };
    if length_scale_to_metres(units).is_none() {
        return Cm1Availability::Unavailable {
            reason: format!(
                "CM1 model-height field `{}` uses unsupported length units `{units}`",
                variable.name
            ),
        };
    }
    Cm1Availability::Available(Cm1ThermodynamicField {
        variable: variable.name.clone(),
        units: units.to_string(),
        transform: Cm1PlaneTransform::NativeScalar,
        interpretation: interpretation.to_string(),
    })
}

/// Derive a native CM1 thermodynamic column using an explicit constants
/// choice. Invalid levels are preserved as NaNs with per-level reasons; the
/// function never pressure-interpolates or calls the MSL-labeled sounding
/// bridge.
pub fn read_thermodynamic_column(
    nc: &NcFile,
    inventory: &Cm1Inventory,
    time_index: usize,
    x_index: usize,
    y_index: usize,
    constants: Cm1ThermodynamicConstants,
) -> Result<Cm1ThermodynamicColumn, Cm1Error> {
    constants.validate()?;
    let readiness = thermodynamic_readiness(inventory);
    if !readiness.can_derive_native_profile() {
        let unavailable = [
            ("th", readiness.potential_temperature.unavailable_reason()),
            ("prs", readiness.pressure.unavailable_reason()),
            (
                "qv",
                readiness.water_vapor_mixing_ratio.unavailable_reason(),
            ),
            ("u", readiness.grid_relative_u.unavailable_reason()),
            ("v", readiness.grid_relative_v.unavailable_reason()),
            ("zhval", readiness.model_level_height.unavailable_reason()),
            (
                "wind frame",
                readiness.wind_frame_correction.unavailable_reason(),
            ),
        ]
        .into_iter()
        .filter_map(|(name, reason)| reason.map(|reason| format!("{name}: {reason}")))
        .collect::<Vec<_>>();
        return Err(Cm1Error::Thermodynamic(unavailable.join("; ")));
    }
    let read_bound = |binding: &Cm1Availability<Cm1ThermodynamicField>| {
        let field = binding.available().expect("readiness checked");
        read_native_column_profile(nc, inventory, &field.variable, time_index, x_index, y_index)
    };
    let theta = read_bound(&readiness.potential_temperature)?;
    let pressure = read_bound(&readiness.pressure)?;
    let qv = read_bound(&readiness.water_vapor_mixing_ratio)?;
    let u = read_bound(&readiness.grid_relative_u)?;
    let v = read_bound(&readiness.grid_relative_v)?;
    let height = theta
        .model_level_height_m
        .available()
        .ok_or_else(|| {
            Cm1Error::Thermodynamic(
                theta
                    .model_level_height_m
                    .unavailable_reason()
                    .unwrap_or("physical model-level height read failed")
                    .to_string(),
            )
        })?
        .values_m
        .clone();
    let nz = theta.values.len();
    for (name, len) in [
        ("prs", pressure.values.len()),
        ("qv", qv.values.len()),
        ("u", u.values.len()),
        ("v", v.values.len()),
        ("zhval", height.len()),
    ] {
        if len != nz {
            return Err(Cm1Error::Thermodynamic(format!(
                "{name} column has {len} levels while th has {nz}"
            )));
        }
    }
    let (domain_u, domain_v) = readiness
        .wind_frame_correction
        .available()
        .and_then(|correction| correction.offset_at(time_index))
        .ok_or_else(|| {
            Cm1Error::Thermodynamic(format!(
                "no wind-frame correction for output time {time_index}"
            ))
        })?;
    let kappa = constants.dry_air_gas_constant_j_kg_k / constants.dry_air_cp_j_kg_k;
    let epsilon = constants.dry_air_gas_constant_j_kg_k / constants.water_vapor_gas_constant_j_kg_k;
    let mut pressure_hpa = vec![f64::NAN; nz];
    let mut temperature_c = vec![f64::NAN; nz];
    let mut dewpoint_c = vec![f64::NAN; nz];
    let mut qv_output = vec![f64::NAN; nz];
    let mut u_grid = vec![f64::NAN; nz];
    let mut v_grid = vec![f64::NAN; nz];
    let mut u_east = vec![f64::NAN; nz];
    let mut v_north = vec![f64::NAN; nz];
    let mut invalid_levels = Vec::new();
    for level in 0..nz {
        let mut reasons = Vec::new();
        let pressure_pa = pressure.values[level];
        let theta_k = theta.values[level];
        let mixing_ratio = qv.values[level];
        if pressure_pa.is_finite() && pressure_pa > 0.0 {
            pressure_hpa[level] = pressure_pa / 100.0;
        } else {
            reasons.push("pressure is not finite and positive");
        }
        if theta_k.is_finite() && theta_k > 0.0 && pressure_hpa[level].is_finite() {
            temperature_c[level] =
                theta_k * (pressure_pa / constants.reference_pressure_pa).powf(kappa) - 273.15;
        } else {
            reasons.push("potential temperature/pressure cannot produce temperature");
        }
        if mixing_ratio.is_finite() && mixing_ratio > 0.0 && pressure_hpa[level].is_finite() {
            qv_output[level] = mixing_ratio;
            let vapor_pressure_hpa = pressure_hpa[level] * mixing_ratio / (epsilon + mixing_ratio);
            let logarithm = (vapor_pressure_hpa / 6.112).ln();
            let derived = 243.5 * logarithm / (17.67 - logarithm);
            if derived.is_finite() {
                dewpoint_c[level] = derived;
            } else {
                reasons.push("mixing ratio/pressure cannot produce finite dewpoint");
            }
        } else {
            reasons.push("water-vapor mixing ratio is not finite and positive");
        }
        if u.values[level].is_finite() {
            u_grid[level] = u.values[level];
            u_east[level] = u.values[level] + domain_u;
        } else {
            reasons.push("u wind is missing");
        }
        if v.values[level].is_finite() {
            v_grid[level] = v.values[level];
            v_north[level] = v.values[level] + domain_v;
        } else {
            reasons.push("v wind is missing");
        }
        if !height[level].is_finite() {
            reasons.push("physical model-level height is missing");
        }
        if !reasons.is_empty() {
            invalid_levels.push((level, reasons.join("; ")));
        }
    }
    Ok(Cm1ThermodynamicColumn {
        time_index,
        x_index,
        y_index,
        local_x_m: theta.local_x_m,
        local_y_m: theta.local_y_m,
        pressure_hpa,
        model_level_height_m: height,
        temperature_c,
        dewpoint_c,
        water_vapor_mixing_ratio_kg_kg: qv_output,
        u_grid_relative_mps: u_grid,
        v_grid_relative_mps: v_grid,
        u_east_mps: u_east,
        v_north_mps: v_north,
        invalid_levels,
        provenance: format!(
            "native CM1 th/prs/qv column; T=theta*(p/p0)^(Rd/Cp); dewpoint from qv and pressure using epsilon=Rd/Rv and Bolton inversion; {}; model height is zhval with undeclared datum (not labeled MSL); no pressure interpolation",
            constants.provenance
        ),
        constants,
    })
}

pub fn read_radar_scene(
    nc: &NcFile,
    inventory: &Cm1Inventory,
    placement: &Cm1Placement,
    time_index: usize,
    terrain_policy: Cm1TerrainPolicy,
) -> Result<Cm1RadarScene, Cm1Error> {
    read_radar_scene_reporting(
        nc,
        inventory,
        placement,
        time_index,
        terrain_policy,
        &|_| {},
    )
}

/// Build a scalar-grid CM1 atmosphere for the existing polar radar sampler.
/// This first practical adapter is deliberately limited to native 3-D `dbz`;
/// it does not extrude 2-D `cref` and does not synthesize dual-pol fields.
pub fn read_radar_scene_reporting(
    nc: &NcFile,
    inventory: &Cm1Inventory,
    placement: &Cm1Placement,
    time_index: usize,
    terrain_policy: Cm1TerrainPolicy,
    progress: &dyn Fn(&str),
) -> Result<Cm1RadarScene, Cm1Error> {
    match &inventory.file_layout {
        Cm1FileLayout::CompleteDomain { .. } => {}
        Cm1FileLayout::MpiTile { .. } => {
            return Err(Cm1Error::RadarScene(
                "one output_filetype=3 MPI tile cannot become a radar scene; assemble the complete domain first"
                    .to_string(),
            ));
        }
        Cm1FileLayout::Unresolved { reason } => {
            return Err(Cm1Error::RadarScene(format!(
                "file layout is unresolved: {reason}"
            )));
        }
    }
    let nx = inventory.axes.xh.raw_values.len();
    let ny = inventory.axes.yh.raw_values.len();
    let nz = inventory.axes.zh.raw_values.len();
    if nx == 0 || ny == 0 || nz < 2 {
        return Err(Cm1Error::RadarScene(format!(
            "radar sampling requires a nonempty horizontal grid and at least two model levels, got {nx}x{ny}x{nz}"
        )));
    }
    let reflectivity = radar_field(
        inventory,
        "dbz",
        &[Cm1VariableRole::NativeScalar3D],
        "dBZ",
        "official native 3-D reflectivity",
    )?;
    let model_height_name = inventory
        .physical_height_variable
        .available()
        .ok_or_else(|| {
            Cm1Error::RadarScene(
                inventory
                    .physical_height_variable
                    .unavailable_reason()
                    .unwrap_or("official zhval is unavailable")
                    .to_string(),
            )
        })?;
    let model_height = bind_length_field(
        inventory,
        model_height_name,
        "official physical height on model levels; vertical datum is undeclared",
    );
    let model_height = available_radar_field("model height", model_height)?;
    let grid_u = available_radar_field(
        "u wind",
        bind_first_thermodynamic_field(
            inventory,
            &[
                (
                    "uinterp",
                    &[Cm1VariableRole::NativeScalar3D][..],
                    "official scalar-grid u (grid-relative)",
                ),
                (
                    "u",
                    &[Cm1VariableRole::NativeXStaggered3D][..],
                    "official x-staggered u, adjacent-face averaged (grid-relative)",
                ),
            ],
            "m/s",
        ),
    )?;
    let grid_v = available_radar_field(
        "v wind",
        bind_first_thermodynamic_field(
            inventory,
            &[
                (
                    "vinterp",
                    &[Cm1VariableRole::NativeScalar3D][..],
                    "official scalar-grid v (grid-relative)",
                ),
                (
                    "v",
                    &[Cm1VariableRole::NativeYStaggered3D][..],
                    "official y-staggered v, adjacent-face averaged (grid-relative)",
                ),
            ],
            "m/s",
        ),
    )?;
    let vertical_wind = available_radar_field(
        "w wind",
        bind_first_thermodynamic_field(
            inventory,
            &[
                (
                    "winterp",
                    &[Cm1VariableRole::NativeScalar3D][..],
                    "official scalar-grid vertical velocity",
                ),
                (
                    "w",
                    &[Cm1VariableRole::NativeZStaggered3D][..],
                    "official z-staggered w, adjacent-face averaged",
                ),
            ],
            "m/s",
        ),
    )?;
    let correction =
        available_radar_field("wind-frame correction", wind_frame_correction(inventory))?;
    let (domain_u, domain_v) = correction.offset_at(time_index).ok_or_else(|| {
        Cm1Error::RadarScene(format!(
            "wind-frame correction has no record for output time {time_index}"
        ))
    })?;
    let georeferenced = georeference_scalar_grid(inventory, placement, time_index)?;
    let valid_time_utc = radar_valid_time(inventory, time_index)?;

    progress("CM1 radar: reading native 3-D dbz");
    let dbz = read_radar_volume(nc, inventory, &reflectivity, time_index, nz, 1.0)?;
    progress("CM1 radar: reading physical model-level heights");
    let height_scale = length_scale_to_metres(&model_height.units).ok_or_else(|| {
        Cm1Error::RadarScene(format!(
            "model-height units `{}` cannot be converted to metres",
            model_height.units
        ))
    })?;
    let model_z_m = read_radar_volume(nc, inventory, &model_height, time_index, nz, height_scale)?;
    progress("CM1 radar: reading and centering horizontal winds");
    let mut u_east_mps = read_radar_volume(nc, inventory, &grid_u, time_index, nz, 1.0)?;
    let mut v_north_mps = read_radar_volume(nc, inventory, &grid_v, time_index, nz, 1.0)?;
    for value in &mut u_east_mps {
        if value.is_finite() {
            *value += domain_u as f32;
        }
    }
    for value in &mut v_north_mps {
        if value.is_finite() {
            *value += domain_v as f32;
        }
    }
    progress("CM1 radar: reading vertical velocity");
    let w_mps = read_radar_volume(nc, inventory, &vertical_wind, time_index, nz, 1.0)?;
    progress("CM1 radar: resolving terrain in the model-z datum");
    let (terrain_model_z_m, terrain_source, flat_terrain_assumed) =
        read_radar_terrain(nc, inventory, time_index, terrain_policy, nx, ny)?;

    validate_radar_scene_arrays(
        nx,
        ny,
        nz,
        &georeferenced.lat_deg,
        &georeferenced.lon_deg,
        &model_z_m,
        &dbz,
        &u_east_mps,
        &v_north_mps,
        &w_mps,
        &terrain_model_z_m,
    )?;
    let field_sources = Cm1RadarFieldSources {
        reflectivity: format!(
            "{} ({})",
            reflectivity.variable, reflectivity.interpretation
        ),
        model_height: format!(
            "{} ({})",
            model_height.variable, model_height.interpretation
        ),
        terrain: terrain_source,
        u: format!("{} ({})", grid_u.variable, grid_u.interpretation),
        v: format!("{} ({})", grid_v.variable, grid_v.interpretation),
        w: format!(
            "{} ({})",
            vertical_wind.variable, vertical_wind.interpretation
        ),
    };
    let wind_provenance = match correction {
        Cm1WindFrameCorrection::StationaryDomain => {
            "stationary CM1 frame; grid x/y aligned with placed east/north".to_string()
        }
        Cm1WindFrameCorrection::AddDomainVelocity { provenance, .. } => provenance,
    };
    let provenance = format!(
        "CM1 native scalar radar scene; source={}; time_index={time_index}; placement={:?}; {}; model_z and terrain share CM1's undeclared vertical datum and are not labeled MSL; wind_frame={wind_provenance}; native 3-D dbz only; no cref extrusion; no dual-pol synthesis",
        inventory.source_path.display(),
        placement.mode,
        georeferenced.provenance,
    );
    Ok(Cm1RadarScene {
        nx,
        ny,
        nz,
        lat_deg: georeferenced.lat_deg,
        lon_deg: georeferenced.lon_deg,
        model_z_m,
        dbz,
        u_east_mps,
        v_north_mps,
        w_mps,
        terrain_model_z_m,
        dx_m: radar_grid_spacing_m(inventory),
        valid_time_utc,
        time_index,
        placement: placement.clone(),
        flat_terrain_assumed,
        field_sources,
        provenance,
    })
}

fn radar_field(
    inventory: &Cm1Inventory,
    name: &str,
    roles: &[Cm1VariableRole],
    units: &str,
    interpretation: &str,
) -> Result<Cm1ThermodynamicField, Cm1Error> {
    available_radar_field(
        name,
        bind_exact_thermodynamic_field(inventory, name, roles, units, interpretation),
    )
}

fn available_radar_field<T>(label: &str, value: Cm1Availability<T>) -> Result<T, Cm1Error> {
    match value {
        Cm1Availability::Available(value) => Ok(value),
        Cm1Availability::Unavailable { reason } => {
            Err(Cm1Error::RadarScene(format!("{label}: {reason}")))
        }
    }
}

fn read_radar_volume(
    nc: &NcFile,
    inventory: &Cm1Inventory,
    field: &Cm1ThermodynamicField,
    time_index: usize,
    nz: usize,
    scale: f64,
) -> Result<Vec<f32>, Cm1Error> {
    let horizontal_cells = inventory
        .axes
        .xh
        .raw_values
        .len()
        .checked_mul(inventory.axes.yh.raw_values.len())
        .ok_or_else(|| Cm1Error::RadarScene("horizontal grid size overflowed".to_string()))?;
    let capacity = horizontal_cells
        .checked_mul(nz)
        .ok_or_else(|| Cm1Error::RadarScene("3-D radar grid size overflowed".to_string()))?;
    let mut values = Vec::with_capacity(capacity);
    for level in 0..nz {
        let plane = read_horizontal_mass_grid_plane(
            nc,
            inventory,
            &field.variable,
            time_index,
            Some(level),
        )?;
        if plane.values.len() != horizontal_cells || plane.transform != field.transform {
            return Err(Cm1Error::RadarScene(format!(
                "{} level {level} has {} cells and transform {:?}, expected {horizontal_cells} and {:?}",
                field.variable,
                plane.values.len(),
                plane.transform,
                field.transform
            )));
        }
        values.extend(
            plane
                .values
                .into_iter()
                .enumerate()
                .map(|(cell, value)| radar_f32(&field.variable, level, cell, value * scale))
                .collect::<Result<Vec<_>, _>>()?,
        );
    }
    Ok(values)
}

fn radar_f32(name: &str, level: usize, cell: usize, value: f64) -> Result<f32, Cm1Error> {
    if value.is_nan() {
        Ok(f32::NAN)
    } else if value.is_finite() && value >= f64::from(f32::MIN) && value <= f64::from(f32::MAX) {
        Ok(value as f32)
    } else {
        Err(Cm1Error::RadarScene(format!(
            "{name} level {level} cell {cell} cannot be represented as f32: {value}"
        )))
    }
}

fn read_radar_terrain(
    nc: &NcFile,
    inventory: &Cm1Inventory,
    time_index: usize,
    policy: Cm1TerrainPolicy,
    nx: usize,
    ny: usize,
) -> Result<(Vec<f32>, String, bool), Cm1Error> {
    let Some(metadata) = inventory.variable("zs") else {
        return match policy {
            Cm1TerrainPolicy::RequireNative => Err(Cm1Error::RadarScene(
                "official CM1 surface-height field `zs` is absent; explicitly choose flat model-z=0 only for a known idealized flat domain"
                    .to_string(),
            )),
            Cm1TerrainPolicy::AssumeFlatModelZero => Ok((
                vec![0.0; nx * ny],
                "explicit user flat-domain assumption: model-z=0".to_string(),
                true,
            )),
        };
    };
    if metadata.role != Cm1VariableRole::NativeScalar2D {
        return Err(Cm1Error::RadarScene(format!(
            "CM1 zs has role {:?}, expected NativeScalar2D",
            metadata.role
        )));
    }
    let units = metadata
        .units
        .as_deref()
        .ok_or_else(|| Cm1Error::RadarScene("CM1 zs has no length units".to_string()))?;
    let scale = length_scale_to_metres(units).ok_or_else(|| {
        Cm1Error::RadarScene(format!("CM1 zs uses unsupported length units `{units}`"))
    })?;
    let plane = read_native_scalar_plane(nc, inventory, &metadata.name, time_index, None)?;
    let values = plane
        .values
        .into_iter()
        .enumerate()
        .map(|(cell, value)| radar_f32(&metadata.name, 0, cell, value * scale))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((
        values,
        format!("{} native CM1 surface height", metadata.name),
        false,
    ))
}

#[allow(clippy::too_many_arguments)]
fn validate_radar_scene_arrays(
    nx: usize,
    ny: usize,
    nz: usize,
    lat: &[f32],
    lon: &[f32],
    height: &[f32],
    dbz: &[f32],
    u: &[f32],
    v: &[f32],
    w: &[f32],
    terrain: &[f32],
) -> Result<(), Cm1Error> {
    let horizontal = nx
        .checked_mul(ny)
        .ok_or_else(|| Cm1Error::RadarScene("horizontal grid size overflowed".to_string()))?;
    let volume = horizontal
        .checked_mul(nz)
        .ok_or_else(|| Cm1Error::RadarScene("3-D grid size overflowed".to_string()))?;
    for (name, actual, expected) in [
        ("latitude", lat.len(), horizontal),
        ("longitude", lon.len(), horizontal),
        ("terrain", terrain.len(), horizontal),
        ("model height", height.len(), volume),
        ("dbz", dbz.len(), volume),
        ("u", u.len(), volume),
        ("v", v.len(), volume),
        ("w", w.len(), volume),
    ] {
        if actual != expected {
            return Err(Cm1Error::RadarScene(format!(
                "{name} has {actual} values, expected {expected}"
            )));
        }
    }
    if lat.iter().any(|value| !value.is_finite())
        || lon.iter().any(|value| !value.is_finite())
        || terrain.iter().any(|value| !value.is_finite())
    {
        return Err(Cm1Error::RadarScene(
            "geolocation and terrain must be finite at every horizontal cell".to_string(),
        ));
    }
    if !dbz.iter().any(|value| value.is_finite()) {
        return Err(Cm1Error::RadarScene(
            "native 3-D dbz contains no finite samples".to_string(),
        ));
    }
    for (name, values) in [("u", u), ("v", v), ("w", w)] {
        if !values.iter().any(|value| value.is_finite()) {
            return Err(Cm1Error::RadarScene(format!(
                "{name} wind contains no finite samples"
            )));
        }
    }
    for cell in 0..horizontal {
        let mut previous = None;
        for level in 0..nz {
            let value = height[level * horizontal + cell];
            if !value.is_finite() {
                return Err(Cm1Error::RadarScene(format!(
                    "model height is missing at level {level}, horizontal cell {cell}"
                )));
            }
            if previous.is_some_and(|previous| value <= previous) {
                return Err(Cm1Error::RadarScene(format!(
                    "model height is not strictly increasing at level {level}, horizontal cell {cell}"
                )));
            }
            previous = Some(value);
        }
    }
    Ok(())
}

fn radar_valid_time(
    inventory: &Cm1Inventory,
    time_index: usize,
) -> Result<DateTime<Utc>, Cm1Error> {
    let offsets = inventory.time.offsets_seconds.available().ok_or_else(|| {
        Cm1Error::RadarScene(
            inventory
                .time
                .offsets_seconds
                .unavailable_reason()
                .unwrap_or("elapsed time cannot be converted to seconds")
                .to_string(),
        )
    })?;
    let offset = *offsets.get(time_index).ok_or(Cm1Error::TimeIndex {
        index: time_index,
        count: inventory.time.record_count,
    })?;
    let rounded = offset.round();
    if !offset.is_finite()
        || offset < 0.0
        || (offset - rounded).abs() > 1.0e-6
        || rounded > i64::MAX as f64
    {
        return Err(Cm1Error::RadarScene(format!(
            "elapsed time {offset} s is not a finite nonnegative whole second"
        )));
    }
    let start_text = inventory
        .time
        .simulation_start_utc
        .available()
        .ok_or_else(|| {
            Cm1Error::RadarScene(
                inventory
                    .time
                    .simulation_start_utc
                    .unavailable_reason()
                    .unwrap_or("simulation start UTC is unavailable")
                    .to_string(),
            )
        })?;
    let start = DateTime::parse_from_rfc3339(start_text)
        .map_err(|error| {
            Cm1Error::RadarScene(format!("invalid simulation start `{start_text}`: {error}"))
        })?
        .with_timezone(&Utc);
    start
        .checked_add_signed(chrono::TimeDelta::seconds(rounded as i64))
        .ok_or_else(|| Cm1Error::RadarScene("valid UTC overflows chrono range".to_string()))
}

fn radar_grid_spacing_m(inventory: &Cm1Inventory) -> Option<f64> {
    fn uniform_spacing(values: &[f64]) -> Option<f64> {
        let mut differences = values.windows(2).map(|pair| pair[1] - pair[0]);
        let first = differences.next()?;
        if !first.is_finite() || first <= 0.0 {
            return None;
        }
        differences
            .all(|difference| {
                difference.is_finite()
                    && difference > 0.0
                    && (difference - first).abs() <= first.abs().max(1.0) * 1.0e-6
            })
            .then_some(first)
    }
    let dx = uniform_spacing(inventory.axes.xh.values_m.available()?)?;
    let dy = uniform_spacing(inventory.axes.yh.values_m.available()?)?;
    ((dx - dy).abs() <= dx.abs().max(dy.abs()).max(1.0) * 1.0e-6).then_some(0.5 * (dx + dy))
}

fn read_model_height_column(
    nc: &NcFile,
    inventory: &Cm1Inventory,
    time_index: usize,
    x_index: usize,
    y_index: usize,
    expected_levels: usize,
) -> Cm1Availability<Cm1ModelHeightColumn> {
    let variable_name = match &inventory.physical_height_variable {
        Cm1Availability::Available(name) => name,
        Cm1Availability::Unavailable { reason } => {
            return Cm1Availability::Unavailable {
                reason: reason.clone(),
            };
        }
    };
    let Some(metadata) = inventory.variable(variable_name) else {
        return Cm1Availability::Unavailable {
            reason: format!("inventoried physical-height field `{variable_name}` disappeared"),
        };
    };
    let Some(source_units) = metadata.units.clone() else {
        return Cm1Availability::Unavailable {
            reason: format!("`{variable_name}` has no units; physical height cannot be converted"),
        };
    };
    let Some(scale) = length_scale_to_metres(&source_units) else {
        return Cm1Availability::Unavailable {
            reason: format!(
                "`{variable_name}` uses unsupported physical-height units `{source_units}`"
            ),
        };
    };
    let values = match read_vertical_line(
        nc,
        metadata,
        time_index,
        &inventory.topology.scalar_z_dimension,
        &inventory.topology.scalar_x_dimension,
        x_index,
        &inventory.topology.scalar_y_dimension,
        y_index,
        expected_levels,
    ) {
        Ok(values) => values,
        Err(error) => {
            return Cm1Availability::Unavailable {
                reason: format!("cannot read `{variable_name}` at this column: {error}"),
            };
        }
    };
    Cm1Availability::Available(Cm1ModelHeightColumn {
        variable: variable_name.clone(),
        source_units,
        values_m: values.into_iter().map(|value| value * scale).collect(),
        interpretation: "official CM1 height on model levels; vertical datum is not declared, so this is not labeled MSL"
            .to_string(),
    })
}

#[allow(clippy::too_many_arguments)]
fn read_vertical_line(
    nc: &NcFile,
    metadata: &Cm1Variable,
    time_index: usize,
    vertical_dimension: &str,
    x_dimension: &str,
    x_index: usize,
    y_dimension: &str,
    y_index: usize,
    vertical_count: usize,
) -> Result<Vec<f64>, Cm1Error> {
    let mut selection = Vec::with_capacity(metadata.dimensions.len());
    let mut remaining_dimensions = Vec::new();
    for dimension in &metadata.dimensions {
        if dimension.eq_ignore_ascii_case("time") {
            selection.push(NcSliceInfoElem::Index(time_index as u64));
        } else if dimension.eq_ignore_ascii_case(vertical_dimension) {
            selection.push(NcSliceInfoElem::Slice {
                start: 0,
                end: u64::MAX,
                step: 1,
            });
            remaining_dimensions.push(dimension.clone());
        } else if dimension.eq_ignore_ascii_case(x_dimension) {
            selection.push(NcSliceInfoElem::Index(x_index as u64));
        } else if dimension.eq_ignore_ascii_case(y_dimension) {
            selection.push(NcSliceInfoElem::Index(y_index as u64));
        } else {
            return Err(Cm1Error::ColumnShape {
                name: metadata.name.clone(),
                dimensions: metadata.dimensions.clone(),
            });
        }
    }
    if remaining_dimensions.len() != 1
        || !remaining_dimensions[0].eq_ignore_ascii_case(vertical_dimension)
    {
        return Err(Cm1Error::ColumnShape {
            name: metadata.name.clone(),
            dimensions: remaining_dimensions,
        });
    }
    let array = nc.read_array_f64_slice(
        &metadata.name,
        &NcSliceInfo {
            selections: selection,
        },
    )?;
    if array.shape() != [vertical_count] {
        return Err(Cm1Error::ColumnShape {
            name: metadata.name.clone(),
            dimensions: remaining_dimensions,
        });
    }
    let mut values = array.into_values();
    if let Some(missing) = metadata.missing_value {
        for value in &mut values {
            if value.to_bits() == missing.to_bits() || *value == missing {
                *value = f64::NAN;
            }
        }
    }
    Ok(values)
}

fn read_destaggered_plane(
    nc: &NcFile,
    inventory: &Cm1Inventory,
    metadata: &Cm1Variable,
    time_index: usize,
    level_index: usize,
) -> Result<Cm1NativePlane, Cm1Error> {
    if time_index >= inventory.time.record_count {
        return Err(Cm1Error::TimeIndex {
            index: time_index,
            count: inventory.time.record_count,
        });
    }
    let x_m = required_axis_values(&inventory.axes.xh)?;
    let y_m = required_axis_values(&inventory.axes.yh)?;
    let scalar_levels = required_axis_values(&inventory.axes.zh)?;
    if level_index >= scalar_levels.len() {
        return Err(Cm1Error::LevelIndex {
            name: metadata.name.clone(),
            index: level_index,
            count: scalar_levels.len(),
        });
    }
    let nx = x_m.len();
    let ny = y_m.len();
    let (values, transform) = match metadata.role {
        Cm1VariableRole::NativeXStaggered3D => {
            let raw_nx = required_axis_values(&inventory.axes.xf)?.len();
            if raw_nx != nx.saturating_add(1) {
                return Err(Cm1Error::PlaneShape {
                    name: metadata.name.clone(),
                    dimensions: metadata.dimensions.clone(),
                });
            }
            let raw = read_horizontal_slice_row_major(
                nc,
                metadata,
                time_index,
                &inventory.topology.scalar_z_dimension,
                level_index,
                &inventory.topology.staggered_x_dimension,
                &inventory.topology.scalar_y_dimension,
                raw_nx,
                ny,
            )?;
            let mut centred = Vec::with_capacity(nx * ny);
            for y in 0..ny {
                for x in 0..nx {
                    centred.push(0.5 * (raw[y * raw_nx + x] + raw[y * raw_nx + x + 1]));
                }
            }
            (centred, Cm1PlaneTransform::DestaggeredX)
        }
        Cm1VariableRole::NativeYStaggered3D => {
            let raw_ny = required_axis_values(&inventory.axes.yf)?.len();
            if raw_ny != ny.saturating_add(1) {
                return Err(Cm1Error::PlaneShape {
                    name: metadata.name.clone(),
                    dimensions: metadata.dimensions.clone(),
                });
            }
            let raw = read_horizontal_slice_row_major(
                nc,
                metadata,
                time_index,
                &inventory.topology.scalar_z_dimension,
                level_index,
                &inventory.topology.scalar_x_dimension,
                &inventory.topology.staggered_y_dimension,
                nx,
                raw_ny,
            )?;
            let mut centred = Vec::with_capacity(nx * ny);
            for y in 0..ny {
                for x in 0..nx {
                    centred.push(0.5 * (raw[y * nx + x] + raw[(y + 1) * nx + x]));
                }
            }
            (centred, Cm1PlaneTransform::DestaggeredY)
        }
        Cm1VariableRole::NativeZStaggered3D => {
            let raw_levels = required_axis_values(&inventory.axes.zf)?.len();
            if raw_levels != scalar_levels.len().saturating_add(1) {
                return Err(Cm1Error::PlaneShape {
                    name: metadata.name.clone(),
                    dimensions: metadata.dimensions.clone(),
                });
            }
            let below = read_horizontal_slice_row_major(
                nc,
                metadata,
                time_index,
                &inventory.topology.staggered_z_dimension,
                level_index,
                &inventory.topology.scalar_x_dimension,
                &inventory.topology.scalar_y_dimension,
                nx,
                ny,
            )?;
            let above = read_horizontal_slice_row_major(
                nc,
                metadata,
                time_index,
                &inventory.topology.staggered_z_dimension,
                level_index + 1,
                &inventory.topology.scalar_x_dimension,
                &inventory.topology.scalar_y_dimension,
                nx,
                ny,
            )?;
            (
                below
                    .into_iter()
                    .zip(above)
                    .map(|(below, above)| 0.5 * (below + above))
                    .collect(),
                Cm1PlaneTransform::DestaggeredZ,
            )
        }
        _ => unreachable!("caller restricts to staggered roles"),
    };
    Ok(Cm1NativePlane {
        variable: metadata.name.clone(),
        units: metadata.units.clone(),
        long_name: metadata.long_name.clone(),
        time_index,
        level_index: Some(level_index),
        nominal_level_m: Some(scalar_levels[level_index]),
        nx,
        ny,
        x_m: x_m.to_vec(),
        y_m: y_m.to_vec(),
        values,
        transform,
    })
}

#[allow(clippy::too_many_arguments)]
fn read_horizontal_slice_row_major(
    nc: &NcFile,
    metadata: &Cm1Variable,
    time_index: usize,
    vertical_dimension: &str,
    level_index: usize,
    x_dimension: &str,
    y_dimension: &str,
    nx: usize,
    ny: usize,
) -> Result<Vec<f64>, Cm1Error> {
    let mut selection = Vec::with_capacity(metadata.dimensions.len());
    let mut remaining_dimensions = Vec::new();
    for dimension in &metadata.dimensions {
        if dimension.eq_ignore_ascii_case("time") {
            selection.push(NcSliceInfoElem::Index(time_index as u64));
        } else if dimension.eq_ignore_ascii_case(vertical_dimension) {
            selection.push(NcSliceInfoElem::Index(level_index as u64));
        } else {
            selection.push(NcSliceInfoElem::Slice {
                start: 0,
                end: u64::MAX,
                step: 1,
            });
            remaining_dimensions.push(dimension.clone());
        }
    }
    if remaining_dimensions.len() != 2
        || !remaining_dimensions
            .iter()
            .any(|dimension| dimension.eq_ignore_ascii_case(x_dimension))
        || !remaining_dimensions
            .iter()
            .any(|dimension| dimension.eq_ignore_ascii_case(y_dimension))
    {
        return Err(Cm1Error::PlaneShape {
            name: metadata.name.clone(),
            dimensions: remaining_dimensions,
        });
    }
    let array = nc.read_array_f64_slice(
        &metadata.name,
        &NcSliceInfo {
            selections: selection,
        },
    )?;
    let mut values = if remaining_dimensions[0].eq_ignore_ascii_case(y_dimension) {
        if array.shape() != [ny, nx] {
            return Err(Cm1Error::PlaneShape {
                name: metadata.name.clone(),
                dimensions: remaining_dimensions,
            });
        }
        array.into_values()
    } else {
        if array.shape() != [nx, ny] {
            return Err(Cm1Error::PlaneShape {
                name: metadata.name.clone(),
                dimensions: remaining_dimensions,
            });
        }
        let source = array.into_values();
        let mut transposed = vec![0.0; nx * ny];
        for x in 0..nx {
            for y in 0..ny {
                transposed[y * nx + x] = source[x * ny + y];
            }
        }
        transposed
    };
    if let Some(missing) = metadata.missing_value {
        for value in &mut values {
            if value.to_bits() == missing.to_bits() || *value == missing {
                *value = f64::NAN;
            }
        }
    }
    Ok(values)
}

fn required_axis_values(axis: &Cm1Axis) -> Result<&[f64], Cm1Error> {
    match &axis.values_m {
        Cm1Availability::Available(values) => Ok(values),
        Cm1Availability::Unavailable { reason } => Err(Cm1Error::UnsupportedScalar {
            name: axis.name.clone(),
            reason: format!("coordinate conversion unavailable: {reason}"),
        }),
    }
}

fn read_axis(
    nc: &NcFile,
    variables: &[NcVariable],
    name: &'static str,
    expected_dimension: &'static str,
    grid: Cm1AxisGrid,
    global_unit_name: &str,
) -> Result<Cm1Axis, Cm1Error> {
    let official_long_name = match grid {
        Cm1AxisGrid::ScalarX => "west-east location of scalar grid points",
        Cm1AxisGrid::StaggeredX => "west-east location of staggered u grid points",
        Cm1AxisGrid::ScalarY => "south-north location of scalar grid points",
        Cm1AxisGrid::StaggeredY => "south-north location of staggered v grid points",
        Cm1AxisGrid::ScalarZ => "nominal height of scalar grid points",
        Cm1AxisGrid::StaggeredZ => "nominal height of staggered w grid points",
    };
    let (source_name, shape, values, variable_units, long_name) =
        if let Some(variable) = variable_ci(variables, name) {
            let dimensions = variable_dimension_names(variable);
            let shape = variable.shape();
            if shape.len() != 1
                || dimensions.len() != 1
                || !dimensions[0].eq_ignore_ascii_case(expected_dimension)
            {
                return Err(Cm1Error::InvalidAxisShape {
                    name: variable.name().to_string(),
                    shape,
                });
            }
            (
                variable.name().to_string(),
                shape,
                variable.array_f64()?.into_values(),
                variable_attr_string_ci(variable, "units").map(ToOwned::to_owned),
                variable_attr_string_ci(variable, "long_name").map(ToOwned::to_owned),
            )
        } else if let Some(metadata) =
            hdf5_axis_metadata(nc, name, expected_dimension, official_long_name)
        {
            (
                metadata.name.clone(),
                vec![metadata.len],
                nc.read_f64(&metadata.name)?,
                Some(metadata.units),
                Some(metadata.long_name),
            )
        } else {
            return Err(Cm1Error::MissingAxis(name));
        };
    if shape.len() != 1 || values.len() != shape[0] {
        return Err(Cm1Error::InvalidAxisShape {
            name: source_name,
            shape,
        });
    }
    if let Some((index, _)) = values
        .iter()
        .enumerate()
        .find(|(_, value)| !value.is_finite())
    {
        return Err(Cm1Error::NonFiniteCoordinate {
            name: source_name,
            index,
        });
    }
    let units = variable_units.or_else(|| global_attr_string_ci(nc, global_unit_name));
    let values_m = match units.as_deref().and_then(length_scale_to_metres) {
        Some(scale) => {
            Cm1Availability::Available(values.iter().map(|value| value * scale).collect())
        }
        None => Cm1Availability::Unavailable {
            reason: match &units {
                Some(units) => format!("unsupported length unit `{units}`"),
                None => "no coordinate units attribute".to_string(),
            },
        },
    };
    Ok(Cm1Axis {
        name: source_name,
        grid,
        source_units: units
            .clone()
            .map(Cm1Availability::Available)
            .unwrap_or_else(|| Cm1Availability::Unavailable {
                reason: "no variable or matching global units attribute".to_string(),
            }),
        raw_values: values,
        values_m,
        long_name,
    })
}

#[derive(Debug)]
struct Hdf5AxisMetadata {
    name: String,
    len: usize,
    units: String,
    long_name: String,
}

/// Recover an official one-dimensional CM1 coordinate dataset that the
/// NetCDF-4 variable index omitted because it is encoded as an HDF5
/// dimension scale. Exact name, rank, and upstream long-name semantics are
/// required so arbitrary HDF5 datasets cannot become grid coordinates.
fn hdf5_axis_metadata(
    nc: &NcFile,
    expected_name: &str,
    expected_dimension: &str,
    official_long_name: &str,
) -> Option<Hdf5AxisMetadata> {
    // This fallback is specifically for self-named NetCDF-4 coordinate
    // variables. Coordinates whose variable and dimension names differ must
    // remain visible through the normal NetCDF variable index.
    if !expected_name.eq_ignore_ascii_case(expected_dimension) {
        return None;
    }
    if !nc.has_hdf5_dataset(expected_name) {
        return None;
    }
    let dimension = nc.dimension(expected_dimension)?;
    // Look up only the coordinate itself. Enumerating all root datasets asks
    // netcrust to apply its safe full-array allocation ceiling to every data
    // variable; a perfectly valid multi-record CM1 output can exceed that
    // ceiling even though BowEcho reads it one record at a time.
    let coordinate = nc.read_array_f64(expected_name).ok()?;
    if coordinate.shape().len() != 1 || dimension.len() != coordinate.shape()[0] {
        return None;
    }
    let long_name = nc.hdf5_dataset_attribute_string(expected_name, "long_name")?;
    if !long_name.trim().eq_ignore_ascii_case(official_long_name) {
        return None;
    }
    let units = nc.hdf5_dataset_attribute_string(expected_name, "units")?;
    length_scale_to_metres(&units)?;
    Some(Hdf5AxisMetadata {
        name: expected_name.to_string(),
        len: coordinate.shape()[0],
        units,
        long_name,
    })
}

#[derive(Debug)]
struct Hdf5TimeAxisMetadata {
    name: String,
    record_count: usize,
    units: String,
}

/// `netcdf-reader` can omit a valid one-dimensional coordinate dataset from
/// its NetCDF-4 index even while all record variables retain their `time`
/// dimension. Netcrust's guarded raw-HDF5 metadata/data fallback lets us
/// recover that coordinate only when its official CM1 name, rank, long name,
/// and units are all present.
fn hdf5_time_axis_metadata(nc: &NcFile) -> Option<Hdf5TimeAxisMetadata> {
    if !nc.has_hdf5_dataset("time") {
        return None;
    }
    let dimension = nc.dimension("time")?;
    let coordinate = nc.read_array_f64("time").ok()?;
    if coordinate.shape().len() != 1 || dimension.len() != coordinate.shape()[0] {
        return None;
    }
    let long_name = nc.hdf5_dataset_attribute_string("time", "long_name")?;
    if !long_name
        .trim()
        .eq_ignore_ascii_case("time since beginning of simulation")
    {
        return None;
    }
    let units = nc.hdf5_dataset_attribute_string("time", "units")?;
    time_scale_to_seconds(&units)?;
    Some(Hdf5TimeAxisMetadata {
        name: "time".to_string(),
        record_count: coordinate.shape()[0],
        units,
    })
}

fn read_time_axis(nc: &NcFile, variables: &[NcVariable]) -> Result<Cm1TimeAxis, Cm1Error> {
    let (variable_name, dimension, record_count, units, raw) =
        if let Some(variable) = variable_ci(variables, "time") {
            (
                variable.name().to_string(),
                variable
                    .dimensions()
                    .first()
                    .map(|dimension| dimension.name().to_string())
                    .unwrap_or_else(|| "time".to_string()),
                variable.shape().first().copied().unwrap_or(0),
                variable_attr_string_ci(variable, "units").map(ToOwned::to_owned),
                variable.array_f64()?.into_values(),
            )
        } else {
            let metadata = hdf5_time_axis_metadata(nc).ok_or(Cm1Error::MissingTime)?;
            let raw = nc.read_f64(&metadata.name)?;
            (
                metadata.name,
                "time".to_string(),
                metadata.record_count,
                Some(metadata.units),
                raw,
            )
        };
    if raw.len() != record_count {
        return Err(Cm1Error::MissingTime);
    }
    let offsets_seconds = match units.as_deref().and_then(time_scale_to_seconds) {
        Some(scale) => {
            Cm1Availability::Available(raw.into_iter().map(|value| value * scale).collect())
        }
        None => Cm1Availability::Unavailable {
            reason: match &units {
                Some(units) => format!("unsupported elapsed-time unit `{units}`"),
                None => "time variable has no units attribute".to_string(),
            },
        },
    };
    Ok(Cm1TimeAxis {
        dimension,
        variable: variable_name,
        record_count,
        source_units: units
            .clone()
            .map(Cm1Availability::Available)
            .unwrap_or_else(|| Cm1Availability::Unavailable {
                reason: "time variable has no units attribute".to_string(),
            }),
        offsets_seconds,
        simulation_start_utc: simulation_start(nc),
    })
}

fn simulation_start(nc: &NcFile) -> Cm1Availability<String> {
    let required = ["year", "month", "day", "hour", "minute", "second"];
    let mut values = [0i32; 6];
    for (index, name) in required.into_iter().enumerate() {
        let Some(value) = global_attr_integer_ci(nc, name) else {
            return Cm1Availability::Unavailable {
                reason: format!("missing or non-integral global `{name}` attribute"),
            };
        };
        values[index] = value;
    }
    let Some(date) = NaiveDate::from_ymd_opt(values[0], values[1] as u32, values[2] as u32) else {
        return Cm1Availability::Unavailable {
            reason: "invalid CM1 global start date".to_string(),
        };
    };
    let Some(naive) = date.and_hms_opt(values[3] as u32, values[4] as u32, values[5] as u32) else {
        return Cm1Availability::Unavailable {
            reason: "invalid CM1 global start time".to_string(),
        };
    };
    let datetime = DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc);
    Cm1Availability::Available(datetime.to_rfc3339_opts(SecondsFormat::Secs, true))
}

fn inventory_variable(
    variable: &NcVariable,
    topology: &Cm1GridTopology,
    global_missing_value: Option<f64>,
) -> Cm1Variable {
    Cm1Variable {
        name: variable.name().to_string(),
        dimensions: variable_dimension_names(variable),
        shape: variable.shape(),
        units: variable_attr_string_ci(variable, "units").map(ToOwned::to_owned),
        long_name: variable_attr_string_ci(variable, "long_name").map(ToOwned::to_owned),
        missing_value: variable_attr_f64_ci(variable, "_FillValue").or(global_missing_value),
        role: classify_variable(variable, topology),
    }
}

fn classify_variable(variable: &NcVariable, topology: &Cm1GridTopology) -> Cm1VariableRole {
    let name = variable.name().to_ascii_lowercase();
    let coordinate_names: &[&str] = match topology.family {
        Cm1SchemaFamily::ModernR20Plus => &["xh", "xf", "yh", "yf", "zh", "zf"],
        Cm1SchemaFamily::LegacyR18R19 => &["xh", "xf", "yh", "yf", "z", "zf"],
        Cm1SchemaFamily::LegacyCoards => &["ni", "nip1", "nj", "njp1", "nk", "nkp1"],
    };
    if coordinate_names.contains(&name.as_str()) {
        return Cm1VariableRole::Coordinate;
    }
    if name == "time" {
        return Cm1VariableRole::Time;
    }
    if !is_numeric(variable.dtype()) {
        return Cm1VariableRole::Unsupported {
            reason: format!("non-numeric NetCDF type {:?}", variable.dtype()),
        };
    }
    let mut dimensions = variable_dimension_names(variable);
    dimensions.retain(|dimension| !dimension.eq_ignore_ascii_case("time"));
    let lower = dimensions
        .iter()
        .map(|dimension| dimension.to_ascii_lowercase())
        .collect::<Vec<_>>();
    if same_dimensions(
        &lower,
        &[
            topology.scalar_y_dimension.as_str(),
            topology.scalar_x_dimension.as_str(),
        ],
    ) {
        Cm1VariableRole::NativeScalar2D
    } else if same_dimensions(
        &lower,
        &[
            topology.scalar_z_dimension.as_str(),
            topology.scalar_y_dimension.as_str(),
            topology.scalar_x_dimension.as_str(),
        ],
    ) {
        Cm1VariableRole::NativeScalar3D
    } else if same_dimensions(
        &lower,
        &[
            topology.scalar_z_dimension.as_str(),
            topology.scalar_y_dimension.as_str(),
            topology.staggered_x_dimension.as_str(),
        ],
    ) {
        Cm1VariableRole::NativeXStaggered3D
    } else if same_dimensions(
        &lower,
        &[
            topology.scalar_z_dimension.as_str(),
            topology.staggered_y_dimension.as_str(),
            topology.scalar_x_dimension.as_str(),
        ],
    ) {
        Cm1VariableRole::NativeYStaggered3D
    } else if same_dimensions(
        &lower,
        &[
            topology.staggered_z_dimension.as_str(),
            topology.scalar_y_dimension.as_str(),
            topology.scalar_x_dimension.as_str(),
        ],
    ) {
        Cm1VariableRole::NativeZStaggered3D
    } else if lower.len() <= 1 {
        Cm1VariableRole::Metadata
    } else {
        Cm1VariableRole::Unsupported {
            reason: format!(
                "dimensions {:?} do not match an official CM1 native scalar or staggered grid",
                variable_dimension_names(variable)
            ),
        }
    }
}

fn classify_file_layout(
    nc: &NcFile,
    source_path: &Path,
    dimensions: &BTreeMap<String, usize>,
    topology: &Cm1GridTopology,
) -> Cm1FileLayout {
    let local_nx = dimensions
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(&topology.scalar_x_dimension))
        .map(|(_, &length)| length);
    let local_ny = dimensions
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(&topology.scalar_y_dimension))
        .map(|(_, &length)| length);
    let global_nx = global_attr_integer_ci(nc, "nx").and_then(|value| usize::try_from(value).ok());
    let global_ny = global_attr_integer_ci(nc, "ny").and_then(|value| usize::try_from(value).ok());
    let file_indices = parse_cm1_output_filename(source_path);
    match (local_nx, local_ny, global_nx, global_ny) {
        (Some(local_nx), Some(local_ny), Some(global_nx), Some(global_ny))
            if local_nx < global_nx
                || local_ny < global_ny
                || file_indices.as_ref().is_some_and(|indices| indices.is_tile) =>
        {
            Cm1FileLayout::MpiTile {
                local_nx,
                local_ny,
                global_nx,
                global_ny,
                process_index: file_indices.as_ref().and_then(|indices| indices.process_index),
                output_index: file_indices.as_ref().and_then(|indices| indices.output_index),
            }
        }
        (Some(nx), Some(ny), Some(global_nx), Some(global_ny))
            if nx == global_nx && ny == global_ny =>
        {
            Cm1FileLayout::CompleteDomain { nx, ny }
        }
        (Some(nx), Some(ny), None, None) if !file_indices.as_ref().is_some_and(|value| value.is_tile) => {
            Cm1FileLayout::CompleteDomain { nx, ny }
        }
        _ => Cm1FileLayout::Unresolved {
            reason: "cannot prove whether this CM1 file is a complete domain or one output_filetype=3 MPI tile"
                .to_string(),
        },
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedOutputFilename {
    is_tile: bool,
    process_index: Option<u32>,
    output_index: Option<u32>,
}

fn parse_cm1_output_filename(path: &Path) -> Option<ParsedOutputFilename> {
    let mut stem = path.file_stem()?.to_str()?;
    if let Some(without_interpolated) = stem.strip_suffix("_i") {
        stem = without_interpolated;
    }
    let suffix = stem.strip_prefix("cm1out_")?;
    let numbers = suffix.split('_').collect::<Vec<_>>();
    match numbers.as_slice() {
        [output] if output.len() == 6 => Some(ParsedOutputFilename {
            is_tile: false,
            process_index: None,
            output_index: output.parse().ok(),
        }),
        [process, output] if process.len() == 6 && output.len() == 6 => {
            Some(ParsedOutputFilename {
                is_tile: true,
                process_index: process.parse().ok(),
                output_index: output.parse().ok(),
            })
        }
        _ => None,
    }
}

fn read_motion(nc: &NcFile, variables: &[NcVariable], record_count: usize) -> Cm1MotionMetadata {
    let east_velocity_mps = read_series_with_scale(
        variables,
        "umove",
        record_count,
        velocity_scale_to_mps,
        "velocity",
    );
    let north_velocity_mps = read_series_with_scale(
        variables,
        "vmove",
        record_count,
        velocity_scale_to_mps,
        "velocity",
    );
    let east_displacement = read_series_with_scale(
        variables,
        "domainlocx",
        record_count,
        length_scale_to_metres,
        "length",
    );
    let north_displacement = read_series_with_scale(
        variables,
        "domainlocy",
        record_count,
        length_scale_to_metres,
        "length",
    );

    let domain_motion = match (&east_displacement, &north_displacement) {
        (Cm1Availability::Available(east_m), Cm1Availability::Available(north_m)) => {
            Cm1DomainMotion::ExplicitDisplacement {
                east_m: east_m.clone(),
                north_m: north_m.clone(),
                east_source: "domainlocx".to_string(),
                north_source: "domainlocy".to_string(),
            }
        }
        _ => {
            let moving = east_velocity_mps
                .available()
                .into_iter()
                .chain(north_velocity_mps.available())
                .flatten()
                .any(|value| value.abs() > 1.0e-9);
            if moving {
                Cm1DomainMotion::Unresolved {
                    reason: "standard cm1out reports moving-frame umove/vmove but not accumulated domainlocx/domainlocy; choose Follow domain or provide authoritative displacement metadata"
                        .to_string(),
                }
            } else {
                Cm1DomainMotion::Static
            }
        }
    };
    // Keep the signature open for a future reader that can source displacement
    // from a paired restart file without changing the inventory contract.
    let _ = nc;
    Cm1MotionMetadata {
        east_velocity_mps,
        north_velocity_mps,
        domain_motion,
    }
}

fn read_series_with_scale(
    variables: &[NcVariable],
    name: &str,
    record_count: usize,
    unit_scale: fn(&str) -> Option<f64>,
    unit_kind: &str,
) -> Cm1Availability<Vec<f64>> {
    let Some(variable) = variable_ci(variables, name) else {
        return Cm1Availability::Unavailable {
            reason: format!("optional CM1 variable `{name}` is absent"),
        };
    };
    let dimensions = variable_dimension_names(variable);
    if dimensions.len() != 1 || !dimensions[0].eq_ignore_ascii_case("time") {
        return Cm1Availability::Unavailable {
            reason: format!("`{name}` has unexpected dimensions {dimensions:?}"),
        };
    }
    let Some(units) = variable_attr_string_ci(variable, "units") else {
        return Cm1Availability::Unavailable {
            reason: format!("`{name}` has no {unit_kind} units"),
        };
    };
    let Some(scale) = unit_scale(units) else {
        return Cm1Availability::Unavailable {
            reason: format!("`{name}` uses unsupported {unit_kind} units `{units}`"),
        };
    };
    match variable.array_f64() {
        Ok(array) if array.len() == record_count => Cm1Availability::Available(
            array
                .into_values()
                .into_iter()
                .map(|value| value * scale)
                .collect(),
        ),
        Ok(array) => Cm1Availability::Unavailable {
            reason: format!(
                "`{name}` has {} records but the CM1 time axis has {record_count}",
                array.len()
            ),
        },
        Err(error) => Cm1Availability::Unavailable {
            reason: format!("cannot read `{name}`: {error}"),
        },
    }
}

fn same_dimensions(actual: &[String], expected: &[&str]) -> bool {
    actual.len() == expected.len()
        && expected
            .iter()
            .all(|expected| actual.iter().any(|actual| actual == expected))
}

fn is_numeric(dtype: &DataType) -> bool {
    matches!(
        dtype,
        DataType::I8
            | DataType::I16
            | DataType::I32
            | DataType::F32
            | DataType::F64
            | DataType::U8
            | DataType::U16
            | DataType::U32
            | DataType::I64
            | DataType::U64
    )
}

fn length_scale_to_metres(units: &str) -> Option<f64> {
    match normalize_unit(units).as_str() {
        "m" | "meter" | "meters" | "metre" | "metres" => Some(1.0),
        "km" | "kilometer" | "kilometers" | "kilometre" | "kilometres" => Some(1_000.0),
        _ => None,
    }
}

fn time_scale_to_seconds(units: &str) -> Option<f64> {
    match normalize_unit(units).as_str() {
        "s" | "sec" | "second" | "seconds" => Some(1.0),
        "min" | "minute" | "minutes" => Some(60.0),
        "h" | "hr" | "hour" | "hours" => Some(3_600.0),
        _ => None,
    }
}

fn velocity_scale_to_mps(units: &str) -> Option<f64> {
    match normalize_unit(units).as_str() {
        "m/s" | "ms-1" | "m/s^1" => Some(1.0),
        "km/h" | "kmhr-1" => Some(1_000.0 / 3_600.0),
        _ => None,
    }
}

fn normalize_unit(units: &str) -> String {
    units
        .trim()
        .to_ascii_lowercase()
        .replace(' ', "")
        .replace("**", "^")
}

fn variable_dimension_names(variable: &NcVariable) -> Vec<String> {
    variable
        .dimensions()
        .iter()
        .map(|dimension| dimension.name().to_string())
        .collect()
}

fn variable_ci<'a>(variables: &'a [NcVariable], name: &str) -> Option<&'a NcVariable> {
    variables
        .iter()
        .find(|variable| variable.name().eq_ignore_ascii_case(name))
}

fn variable_attr_string_ci<'a>(variable: &'a NcVariable, name: &str) -> Option<&'a str> {
    variable
        .attributes()
        .iter()
        .find(|attribute| attribute.name().eq_ignore_ascii_case(name))
        .and_then(|attribute| attribute.as_string())
}

fn variable_attr_f64_ci(variable: &NcVariable, name: &str) -> Option<f64> {
    variable
        .attributes()
        .iter()
        .find(|attribute| attribute.name().eq_ignore_ascii_case(name))
        .and_then(|attribute| attribute.as_f64())
}

fn global_attr_string_ci(nc: &NcFile, name: &str) -> Option<String> {
    nc.attributes().ok()?.into_iter().find_map(|attribute| {
        attribute
            .name()
            .eq_ignore_ascii_case(name)
            .then(|| attribute.as_string().map(ToOwned::to_owned))
            .flatten()
    })
}

fn global_attr_f64_ci(nc: &NcFile, name: &str) -> Option<f64> {
    nc.attributes().ok()?.into_iter().find_map(|attribute| {
        attribute
            .name()
            .eq_ignore_ascii_case(name)
            .then(|| attribute.as_f64())
            .flatten()
    })
}

fn global_attr_integer_ci(nc: &NcFile, name: &str) -> Option<i32> {
    let value = global_attr_f64_ci(nc, name)?;
    if !value.is_finite() || value < i32::MIN as f64 || value > i32::MAX as f64 {
        return None;
    }
    let rounded = value.round();
    (value - rounded)
        .abs()
        .le(&f64::EPSILON)
        .then_some(rounded as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cm1/cm1out_schema.nc")
    }

    fn legacy_fixture_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cm1/cm1out_legacy_r19.nc")
    }

    #[test]
    fn official_schema_fixture_is_detected_and_inventoried() {
        let path = fixture_path();
        let nc = netcrust::open(&path).expect("open fixture");
        assert!(
            nc.variable("xh").is_none(),
            "modern NetCDF-4 fixture must exercise the dimension-scale fallback"
        );
        assert!(nc.has_hdf5_dataset("xh"));
        let inventory = inspect_file(&nc, &path).expect("inspect fixture");
        assert_eq!(
            inventory.detection.confidence,
            Cm1DetectionConfidence::Confirmed
        );
        assert_eq!(inventory.version.as_deref(), Some("cm1r21.1"));
        assert_eq!(inventory.topology.family, Cm1SchemaFamily::ModernR20Plus);
        assert_eq!(inventory.time.record_count, 2);
        assert_eq!(
            inventory.time.offsets_seconds.available(),
            Some(&vec![0.0, 60.0])
        );
        assert_eq!(
            inventory.time.simulation_start_utc.available(),
            Some(&"2026-05-20T18:30:00Z".to_string())
        );
        assert_eq!(
            inventory.axes.xh.values_m.available(),
            Some(&vec![-1_000.0, 0.0, 1_000.0])
        );
        assert_eq!(
            inventory.variable("custom_scalar").map(|field| &field.role),
            Some(&Cm1VariableRole::NativeScalar3D)
        );
        assert_eq!(
            inventory.variable("cref").map(|field| &field.role),
            Some(&Cm1VariableRole::NativeScalar2D)
        );
        assert_eq!(
            inventory.variable("u").map(|field| &field.role),
            Some(&Cm1VariableRole::NativeXStaggered3D)
        );
        assert_eq!(
            inventory.physical_height_variable.available(),
            Some(&"zhval".to_string())
        );
        assert!(matches!(
            inventory.motion.domain_motion,
            Cm1DomainMotion::Unresolved { .. }
        ));
        assert_eq!(
            inventory.file_layout,
            Cm1FileLayout::CompleteDomain { nx: 3, ny: 2 }
        );
        assert!(
            inventory
                .placement_offset_m(Cm1PlacementMode::FixedWorld, 1)
                .is_err()
        );
        assert_eq!(
            inventory
                .placement_offset_m(Cm1PlacementMode::FollowDomain, 1)
                .expect("follow domain"),
            (0.0, 0.0)
        );
    }

    #[test]
    fn optional_real_modern_hdf5_output_inspects_and_builds_radar_scene() {
        let Some(path) = std::env::var_os("BOWECHO_CM1_R21_FIXTURE").map(PathBuf::from) else {
            return;
        };
        let nc = netcrust::open(&path).expect("open real modern CM1 output");
        let inventory = inspect_file(&nc, &path).expect("inspect real modern CM1 output");
        assert_eq!(inventory.topology.family, Cm1SchemaFamily::ModernR20Plus);
        for field in [
            "dbz", "zhval", "th", "prs", "qv", "uinterp", "vinterp", "winterp",
        ] {
            assert!(inventory.variable(field).is_some(), "missing `{field}`");
        }
        let scene = read_radar_scene(
            &nc,
            &inventory,
            &Cm1Placement {
                mode: Cm1PlacementMode::FollowDomain,
                anchor_latitude_deg: 35.0,
                anchor_longitude_deg: -97.0,
            },
            0,
            Cm1TerrainPolicy::RequireNative,
        )
        .expect("build radar-ready scene from real modern CM1 output");
        assert_eq!(scene.nx, inventory.axes.xh.raw_values.len());
        assert_eq!(scene.ny, inventory.axes.yh.raw_values.len());
        assert_eq!(scene.nz, inventory.axes.zh.raw_values.len());
    }

    #[test]
    fn optional_large_multirecord_modern_output_is_detected_without_root_enumeration() {
        let Some(path) = std::env::var_os("BOWECHO_CM1_R21_LARGE_FIXTURE").map(PathBuf::from)
        else {
            return;
        };
        let nc = netcrust::open(&path).expect("open large modern CM1 output");
        assert!(
            nc.hdf5_root_datasets().is_err(),
            "fixture must exceed netcrust's safe full-array ceiling"
        );
        let inventory = inspect_file(&nc, &path).expect("inspect large modern CM1 output");
        assert_eq!(
            inventory.detection.confidence,
            Cm1DetectionConfidence::Confirmed
        );
        assert_eq!(inventory.topology.family, Cm1SchemaFamily::ModernR20Plus);
        assert_eq!(inventory.time.record_count, 21);
        assert_eq!(inventory.axes.xh.raw_values.len(), 400);
        assert_eq!(inventory.axes.yh.raw_values.len(), 400);
        assert_eq!(inventory.axes.zh.raw_values.len(), 108);
        assert!(inventory.variable("dbz").is_some());
        assert!(inventory.variable("zhval").is_some());
    }

    #[test]
    fn arbitrary_scalar_plane_reads_in_y_x_order() {
        let path = fixture_path();
        let nc = netcrust::open(&path).expect("open fixture");
        let inventory = inspect_file(&nc, &path).expect("inspect fixture");
        let plane = read_native_scalar_plane(&nc, &inventory, "custom_scalar", 1, Some(1))
            .expect("read scalar plane");
        assert_eq!((plane.nx, plane.ny), (3, 2));
        assert_eq!(plane.nominal_level_m, Some(1_500.0));
        assert_eq!(plane.values, vec![110.0, 111.0, 112.0, 113.0, 114.0, 115.0]);
        assert_eq!(plane.transform, Cm1PlaneTransform::NativeScalar);
    }

    #[test]
    fn official_staggered_vectors_are_averaged_onto_scalar_grid() {
        let path = fixture_path();
        let nc = netcrust::open(&path).expect("open fixture");
        let inventory = inspect_file(&nc, &path).expect("inspect fixture");

        let u =
            read_horizontal_mass_grid_plane(&nc, &inventory, "u", 0, Some(0)).expect("destagger u");
        assert_eq!(u.transform, Cm1PlaneTransform::DestaggeredX);
        assert_eq!(u.values, vec![1.0, 3.0, 5.0, 1.0, 3.0, 5.0]);

        let v =
            read_horizontal_mass_grid_plane(&nc, &inventory, "v", 0, Some(0)).expect("destagger v");
        assert_eq!(v.transform, Cm1PlaneTransform::DestaggeredY);
        assert_eq!(v.values, vec![2.0, 2.0, 2.0, 6.0, 6.0, 6.0]);

        let w0 = read_horizontal_mass_grid_plane(&nc, &inventory, "w", 0, Some(0))
            .expect("destagger w level 0");
        assert_eq!(w0.transform, Cm1PlaneTransform::DestaggeredZ);
        assert_eq!(w0.values, vec![5.0; 6]);
        let w1 = read_horizontal_mass_grid_plane(&nc, &inventory, "w", 0, Some(1))
            .expect("destagger w level 1");
        assert_eq!(w1.values, vec![15.0; 6]);
    }

    #[test]
    fn native_columns_preserve_levels_and_exact_staggering() {
        let path = fixture_path();
        let nc = netcrust::open(&path).expect("open fixture");
        let inventory = inspect_file(&nc, &path).expect("inspect fixture");

        let scalar = read_native_column_profile(&nc, &inventory, "custom_scalar", 1, 2, 1)
            .expect("read scalar column");
        assert_eq!(scalar.values, vec![105.0, 115.0]);
        assert_eq!(
            scalar.nominal_level_m.available(),
            Some(&vec![500.0, 1_500.0])
        );
        let physical = scalar
            .model_level_height_m
            .available()
            .expect("physical model heights");
        assert_eq!(physical.variable, "zhval");
        assert_eq!(physical.values_m, vec![500.0, 1_500.0]);
        assert!(physical.interpretation.contains("not labeled MSL"));
        assert_eq!(scalar.transform, Cm1PlaneTransform::NativeScalar);

        let u = read_native_column_profile(&nc, &inventory, "u", 0, 1, 0)
            .expect("read destaggered u column");
        assert_eq!(u.values, vec![3.0, 3.0]);
        assert_eq!(u.transform, Cm1PlaneTransform::DestaggeredX);

        let v = read_native_column_profile(&nc, &inventory, "v", 0, 2, 0)
            .expect("read destaggered v column");
        assert_eq!(v.values, vec![2.0, 2.0]);
        assert_eq!(v.transform, Cm1PlaneTransform::DestaggeredY);

        let w = read_native_column_profile(&nc, &inventory, "w", 0, 0, 1)
            .expect("read destaggered w column");
        assert_eq!(w.values, vec![5.0, 15.0]);
        assert_eq!(w.transform, Cm1PlaneTransform::DestaggeredZ);
    }

    #[test]
    fn native_column_rejects_out_of_domain_location() {
        let path = fixture_path();
        let nc = netcrust::open(&path).expect("open fixture");
        let inventory = inspect_file(&nc, &path).expect("inspect fixture");
        assert!(matches!(
            read_native_column_profile(&nc, &inventory, "custom_scalar", 0, 3, 0),
            Err(Cm1Error::ColumnIndex {
                x: 3,
                y: 0,
                nx: 3,
                ny: 2
            })
        ));
    }

    #[test]
    fn thermodynamic_readiness_requires_exact_native_fields_and_units() {
        let ready = inspect_path(fixture_path()).expect("inspect modern fixture");
        let readiness = thermodynamic_readiness(&ready);
        assert!(readiness.can_derive_native_profile());
        assert_eq!(
            readiness
                .grid_relative_u
                .available()
                .map(|field| field.variable.as_str()),
            Some("uinterp")
        );
        assert!(readiness.sounding_viewer.available().is_none());
        assert!(
            readiness
                .sounding_viewer
                .unavailable_reason()
                .expect("explicit viewer reason")
                .contains("MSL datum")
        );

        let incomplete = inspect_path(legacy_fixture_path()).expect("inspect legacy fixture");
        let readiness = thermodynamic_readiness(&incomplete);
        assert!(!readiness.can_derive_native_profile());
        assert!(
            readiness
                .potential_temperature
                .unavailable_reason()
                .expect("missing th reason")
                .contains("`th` is absent")
        );
    }

    #[test]
    fn thermodynamic_column_uses_explicit_defaults_and_motion_correction() {
        let path = fixture_path();
        let nc = netcrust::open(&path).expect("open fixture");
        let inventory = inspect_file(&nc, &path).expect("inspect fixture");
        let constants = Cm1ThermodynamicConstants::official_defaults();
        let column = read_thermodynamic_column(&nc, &inventory, 0, 1, 0, constants.clone())
            .expect("derive thermodynamic column");

        assert_eq!(column.pressure_hpa, vec![950.0, 800.0]);
        assert_eq!(column.model_level_height_m, vec![500.0, 1_500.0]);
        assert_eq!(column.u_grid_relative_mps, vec![2.0, 3.0]);
        assert_eq!(column.v_grid_relative_mps, vec![4.0, 5.0]);
        assert_eq!(column.u_east_mps, vec![14.5, 15.5]);
        assert_eq!(column.v_north_mps, vec![7.0, 8.0]);
        assert!(column.invalid_levels.is_empty());
        let expected_temperature = 300.0
            * (0.95_f64).powf(constants.dry_air_gas_constant_j_kg_k / constants.dry_air_cp_j_kg_k)
            - 273.15;
        assert!((column.temperature_c[0] - expected_temperature).abs() < 1.0e-10);
        assert!(column.dewpoint_c.iter().all(|value| value.is_finite()));
        assert!(column.provenance.contains("testcase 4/5 overrides"));
        assert!(column.provenance.contains("not labeled MSL"));
    }

    #[test]
    fn native_dbz_scene_has_physical_heights_and_earth_relative_winds() {
        let path = fixture_path();
        let nc = netcrust::open(&path).expect("open fixture");
        let inventory = inspect_file(&nc, &path).expect("inspect fixture");
        let placement = Cm1Placement {
            mode: Cm1PlacementMode::FollowDomain,
            anchor_latitude_deg: 35.0,
            anchor_longitude_deg: -97.0,
        };
        let scene = read_radar_scene(
            &nc,
            &inventory,
            &placement,
            0,
            Cm1TerrainPolicy::RequireNative,
        )
        .expect("build focused CM1 radar scene");

        assert_eq!((scene.nx, scene.ny, scene.nz), (3, 2, 2));
        assert_eq!(scene.model_z_m, [vec![500.0; 6], vec![1_500.0; 6]].concat());
        assert_eq!(scene.terrain_model_z_m, vec![100.0; 6]);
        assert_eq!(scene.u_east_mps, [vec![14.5; 6], vec![15.5; 6]].concat());
        assert_eq!(scene.v_north_mps, [vec![7.0; 6], vec![8.0; 6]].concat());
        assert_eq!(scene.w_mps, [vec![5.0; 6], vec![15.0; 6]].concat());
        assert_eq!(scene.dx_m, Some(1_000.0));
        assert_eq!(
            scene.valid_time_utc.to_rfc3339(),
            "2026-05-20T18:30:00+00:00"
        );
        assert!(!scene.flat_terrain_assumed);
        assert!(scene.field_sources.reflectivity.starts_with("dbz"));
        assert!(scene.provenance.contains("no cref extrusion"));
        assert!(scene.provenance.contains("not labeled MSL"));
    }

    #[test]
    fn flat_radar_terrain_requires_explicit_policy() {
        let path = fixture_path();
        let nc = netcrust::open(&path).expect("open fixture");
        let mut inventory = inspect_file(&nc, &path).expect("inspect fixture");
        inventory
            .variables
            .retain(|variable| !variable.name.eq_ignore_ascii_case("zs"));
        assert!(matches!(
            read_radar_terrain(
                &nc,
                &inventory,
                0,
                Cm1TerrainPolicy::RequireNative,
                3,
                2
            ),
            Err(Cm1Error::RadarScene(reason)) if reason.contains("explicitly choose flat")
        ));
        let (terrain, source, assumed) = read_radar_terrain(
            &nc,
            &inventory,
            0,
            Cm1TerrainPolicy::AssumeFlatModelZero,
            3,
            2,
        )
        .expect("explicit flat terrain");
        assert_eq!(terrain, vec![0.0; 6]);
        assert!(source.contains("explicit user"));
        assert!(assumed);
    }

    #[test]
    fn legacy_r19_topology_and_minutes_are_supported() {
        let path = legacy_fixture_path();
        let nc = netcrust::open(&path).expect("open legacy fixture");
        assert!(
            nc.variable("time").is_none(),
            "NetCDF-4 fixture exercises the guarded raw-HDF5 time fallback"
        );
        assert!(nc.has_hdf5_dataset("time"));
        let inventory = inspect_file(&nc, &path).expect("inspect legacy fixture");
        assert_eq!(inventory.topology.family, Cm1SchemaFamily::LegacyR18R19);
        assert_eq!(inventory.topology.scalar_x_dimension, "ni");
        assert_eq!(inventory.topology.scalar_z_dimension, "nk");
        assert_eq!(
            inventory.time.offsets_seconds.available(),
            Some(&vec![0.0, 60.0])
        );
        assert_eq!(
            inventory
                .variable("custom_legacy")
                .map(|variable| &variable.role),
            Some(&Cm1VariableRole::NativeScalar3D)
        );
        assert_eq!(
            inventory.variable("u").map(|variable| &variable.role),
            Some(&Cm1VariableRole::NativeXStaggered3D)
        );

        let plane = read_native_scalar_plane(&nc, &inventory, "custom_legacy", 1, Some(1))
            .expect("read legacy scalar plane");
        assert_eq!(&plane.values[..5], &[110.0, 111.0, 112.0, 113.0, 114.0]);
        assert!(plane.values[5].is_nan(), "global CM1 sentinel -> NaN");
    }

    #[test]
    fn output_filetype_three_filename_is_a_tile() {
        let parsed = parse_cm1_output_filename(Path::new("cm1out_000017_000042_i.nc"))
            .expect("parse CM1 tile name");
        assert!(parsed.is_tile);
        assert_eq!(parsed.process_index, Some(17));
        assert_eq!(parsed.output_index, Some(42));
    }

    #[test]
    fn official_diagnostics_supply_exact_fixed_world_offsets() {
        let path = fixture_path();
        let nc = netcrust::open(&path).expect("open fixture");
        let mut inventory = inspect_file(&nc, &path).expect("inspect fixture");
        let folder = path.parent().expect("fixture folder");
        let diagnostics = diagnostic_files_in_folder(folder);
        assert_eq!(diagnostics.len(), 2);
        let attachment = attach_motion_diagnostics(&mut inventory, &diagnostics)
            .expect("attach official diagnostics");
        assert_eq!(attachment.matched_times_seconds, vec![0.0, 60.0]);
        assert_eq!(
            inventory
                .placement_offset_m(Cm1PlacementMode::FixedWorld, 1)
                .expect("fixed-world offset"),
            (750.0, 180.0)
        );
        let follow = georeference_scalar_grid(
            &inventory,
            &Cm1Placement {
                mode: Cm1PlacementMode::FollowDomain,
                anchor_latitude_deg: 35.0,
                anchor_longitude_deg: -97.0,
            },
            1,
        )
        .expect("follow-domain grid");
        let fixed = georeference_scalar_grid(
            &inventory,
            &Cm1Placement {
                mode: Cm1PlacementMode::FixedWorld,
                anchor_latitude_deg: 35.0,
                anchor_longitude_deg: -97.0,
            },
            1,
        )
        .expect("fixed-world grid");
        assert_eq!((follow.nx, follow.ny), (3, 2));
        assert_ne!(follow.lat_deg, fixed.lat_deg);
        assert_ne!(follow.lon_deg, fixed.lon_deg);
    }
}
