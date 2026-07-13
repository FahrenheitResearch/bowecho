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

#[derive(Debug, Clone, PartialEq)]
pub struct Cm1Axis {
    pub name: String,
    pub grid: Cm1AxisGrid,
    pub source_units: Cm1Availability<String>,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cm1Variable {
    pub name: String,
    pub dimensions: Vec<String>,
    pub shape: Vec<usize>,
    pub units: Option<String>,
    pub long_name: Option<String>,
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

#[derive(Debug, Clone, PartialEq)]
pub struct Cm1Inventory {
    pub source_path: PathBuf,
    pub detection: Cm1Detection,
    pub version: Option<String>,
    pub dimensions: BTreeMap<String, usize>,
    pub axes: Cm1Axes,
    pub time: Cm1TimeAxis,
    pub variables: Vec<Cm1Variable>,
    pub motion: Cm1MotionMetadata,
    pub geographic_hints: Cm1GeographicHints,
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

    #[error("fixed-world CM1 placement is unavailable: {0}")]
    PlacementUnavailable(String),
}

/// Inspect a file from disk and return its native CM1 inventory.
pub fn inspect_path(path: impl AsRef<Path>) -> Result<Cm1Inventory, Cm1Error> {
    let path = path.as_ref();
    let nc = netcrust::open(path)?;
    inspect_file(&nc, path)
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

    let official_axes = [
        ("xh", "west-east location of scalar grid points"),
        ("xf", "west-east location of staggered u grid points"),
        ("yh", "south-north location of scalar grid points"),
        ("yf", "south-north location of staggered v grid points"),
        ("zh", "nominal height of scalar grid points"),
        ("zf", "nominal height of staggered w grid points"),
    ];
    let mut axis_count = 0usize;
    let mut described_axis_count = 0usize;
    for (name, official_long_name) in official_axes {
        if let Some(variable) = variable_ci(&variables, name) {
            let dimensions = variable_dimension_names(variable);
            if dimensions.len() == 1 && dimensions[0].eq_ignore_ascii_case(name) {
                axis_count += 1;
                if variable_attr_string_ci(variable, "long_name")
                    .is_some_and(|value| value.eq_ignore_ascii_case(official_long_name))
                {
                    described_axis_count += 1;
                }
            }
        }
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
    let dimensions = nc
        .dimensions()?
        .into_iter()
        .map(|dimension| (dimension.name().to_string(), dimension.len()))
        .collect::<BTreeMap<_, _>>();

    let axes = Cm1Axes {
        xh: read_axis(nc, &variables, "xh", Cm1AxisGrid::ScalarX, "x_units")?,
        xf: read_axis(nc, &variables, "xf", Cm1AxisGrid::StaggeredX, "x_units")?,
        yh: read_axis(nc, &variables, "yh", Cm1AxisGrid::ScalarY, "y_units")?,
        yf: read_axis(nc, &variables, "yf", Cm1AxisGrid::StaggeredY, "y_units")?,
        zh: read_axis(nc, &variables, "zh", Cm1AxisGrid::ScalarZ, "z_units")?,
        zf: read_axis(nc, &variables, "zf", Cm1AxisGrid::StaggeredZ, "z_units")?,
    };
    let time = read_time_axis(nc, &variables)?;
    let inventoried_variables = variables.iter().map(inventory_variable).collect();
    let motion = read_motion(nc, &variables, time.record_count);
    let physical_height_variable = variable_ci(&variables, "zhval")
        .filter(|variable| matches!(classify_variable(variable), Cm1VariableRole::NativeScalar3D))
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
        dimensions,
        axes,
        time,
        variables: inventoried_variables,
        motion,
        geographic_hints: Cm1GeographicHints {
            control_latitude_deg: global_attr_f64_ci(nc, "ctrlat"),
            control_longitude_deg: global_attr_f64_ci(nc, "ctrlon"),
            interpretation: "CM1 documents ctrlat/ctrlon as applying to the entire domain; they are not a map projection or a cell geolocation. World placement requires an explicit user anchor."
                .to_string(),
        },
        physical_height_variable,
    })
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
            .find(|(dimension, _)| dimension.eq_ignore_ascii_case("zh"))
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
        } else if dimension.eq_ignore_ascii_case("zh") {
            selection.push(NcSliceInfoElem::Index(
                selected_level.expect("3-D role has zh") as u64,
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
            .any(|dimension| dimension.eq_ignore_ascii_case("xh"))
        || !remaining_dimensions
            .iter()
            .any(|dimension| dimension.eq_ignore_ascii_case("yh"))
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
    let values = if remaining_dimensions[0].eq_ignore_ascii_case("yh") {
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
    })
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
    grid: Cm1AxisGrid,
    global_unit_name: &str,
) -> Result<Cm1Axis, Cm1Error> {
    let variable = variable_ci(variables, name).ok_or(Cm1Error::MissingAxis(name))?;
    let shape = variable.shape();
    if shape.len() != 1 {
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
            Cm1Availability::Available(values.into_iter().map(|value| value * scale).collect())
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

fn inventory_variable(variable: &NcVariable) -> Cm1Variable {
    Cm1Variable {
        name: variable.name().to_string(),
        dimensions: variable_dimension_names(variable),
        shape: variable.shape(),
        units: variable_attr_string_ci(variable, "units").map(ToOwned::to_owned),
        long_name: variable_attr_string_ci(variable, "long_name").map(ToOwned::to_owned),
        role: classify_variable(variable),
    }
}

fn classify_variable(variable: &NcVariable) -> Cm1VariableRole {
    let name = variable.name().to_ascii_lowercase();
    if matches!(name.as_str(), "xh" | "xf" | "yh" | "yf" | "zh" | "zf") {
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
    if same_dimensions(&lower, &["yh", "xh"]) {
        Cm1VariableRole::NativeScalar2D
    } else if same_dimensions(&lower, &["zh", "yh", "xh"]) {
        Cm1VariableRole::NativeScalar3D
    } else if same_dimensions(&lower, &["zh", "yh", "xf"]) {
        Cm1VariableRole::NativeXStaggered3D
    } else if same_dimensions(&lower, &["zh", "yf", "xh"]) {
        Cm1VariableRole::NativeYStaggered3D
    } else if same_dimensions(&lower, &["zf", "yh", "xh"]) {
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

    #[test]
    fn official_schema_fixture_is_detected_and_inventoried() {
        let inventory = inspect_path(fixture_path()).expect("inspect fixture");
        assert_eq!(
            inventory.detection.confidence,
            Cm1DetectionConfidence::Confirmed
        );
        assert_eq!(inventory.version.as_deref(), Some("cm1r21.1"));
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
    }
}
