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

fn topology_score(variables: &[NcVariable], topology: TopologySpec) -> (usize, usize) {
    let mut axes = 0usize;
    let mut described = 0usize;
    for (variable_name, dimension_name, official_long_name) in topology.axes() {
        let Some(variable) = variable_ci(variables, variable_name) else {
            continue;
        };
        let dimensions = variable_dimension_names(variable);
        if dimensions.len() != 1 || !dimensions[0].eq_ignore_ascii_case(dimension_name) {
            continue;
        }
        axes += 1;
        if variable_attr_string_ci(variable, "long_name").is_some_and(|value| {
            value.eq_ignore_ascii_case(official_long_name)
                || (matches!(topology.family, Cm1SchemaFamily::LegacyR18R19)
                    && variable_name == "z"
                    && value.to_ascii_lowercase().contains("height"))
        }) {
            described += 1;
        }
    }
    (axes, described)
}

fn select_topology(variables: &[NcVariable]) -> Option<(TopologySpec, usize, usize)> {
    [MODERN_TOPOLOGY, LEGACY_TOPOLOGY, LEGACY_COARDS_TOPOLOGY]
        .into_iter()
        .map(|topology| {
            let (axes, described) = topology_score(variables, topology);
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

    let selected_topology = select_topology(&variables);
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
                value.eq_ignore_ascii_case("time since beginning of simulation")
            })
    });
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
    let topology_spec = select_topology(&variables)
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
    let variable = variable_ci(variables, name).ok_or(Cm1Error::MissingAxis(name))?;
    let shape = variable.shape();
    let dimension_names = variable_dimension_names(variable);
    if shape.len() != 1
        || dimension_names.len() != 1
        || !dimension_names[0].eq_ignore_ascii_case(expected_dimension)
    {
        return Err(Cm1Error::InvalidAxisShape {
            name: variable.name().to_string(),
            shape,
        });
    }
    let values = variable.array_f64()?.into_values();
    if let Some((index, _)) = values
        .iter()
        .enumerate()
        .find(|(_, value)| !value.is_finite())
    {
        return Err(Cm1Error::NonFiniteCoordinate {
            name: variable.name().to_string(),
            index,
        });
    }
    let units = variable_attr_string_ci(variable, "units")
        .map(ToOwned::to_owned)
        .or_else(|| global_attr_string_ci(nc, global_unit_name));
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
        name: variable.name().to_string(),
        grid,
        source_units: units
            .clone()
            .map(Cm1Availability::Available)
            .unwrap_or_else(|| Cm1Availability::Unavailable {
                reason: "no variable or matching global units attribute".to_string(),
            }),
        raw_values: values,
        values_m,
        long_name: variable_attr_string_ci(variable, "long_name").map(ToOwned::to_owned),
    })
}

fn read_time_axis(nc: &NcFile, variables: &[NcVariable]) -> Result<Cm1TimeAxis, Cm1Error> {
    let variable = variable_ci(variables, "time").ok_or(Cm1Error::MissingTime)?;
    let raw = variable.array_f64()?.into_values();
    let units = variable_attr_string_ci(variable, "units").map(ToOwned::to_owned);
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
    let dimension = variable
        .dimensions()
        .first()
        .map(|dimension| dimension.name().to_string())
        .unwrap_or_else(|| "time".to_string());
    Ok(Cm1TimeAxis {
        dimension,
        variable: variable.name().to_string(),
        record_count: variable.shape().first().copied().unwrap_or(0),
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
        let inventory = inspect_path(fixture_path()).expect("inspect fixture");
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
    fn legacy_r19_topology_and_minutes_are_supported() {
        let path = legacy_fixture_path();
        let nc = netcrust::open(&path).expect("open legacy fixture");
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
