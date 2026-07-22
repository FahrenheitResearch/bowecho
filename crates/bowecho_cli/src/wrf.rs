use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{NaiveDate, NaiveDateTime};
use netcrust::{AttributeValue, DataType};
use serde::{Deserialize, Serialize};

use crate::artifact::{ArtifactManifest, ArtifactStatus, BuildIdentity};
use crate::fs::{expand_input_files, resolve_receipt_path, sha256_file, sha256_reader};
use crate::{CliError, ExitCode, RuntimeContext};

pub const INSPECT_SCHEMA_VERSION: &str = "bowecho.wrf.inspect.v1";
pub const VERIFY_SCHEMA_VERSION: &str = "bowecho.wrf.verify.v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectStatus {
    Complete,
    Partial,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DomainSummary {
    pub domain: String,
    pub valid_times: Vec<String>,
    pub files: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DimensionMetadata {
    pub name: String,
    pub length: usize,
    pub unlimited: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VariableMetadata {
    pub name: String,
    pub data_type: String,
    pub dimensions: Vec<DimensionMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub units: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staggering: Option<String>,
    /// False means raw HDF5 metadata found a dataset omitted from the NetCDF
    /// index. Shape is still reported, but named dimensions/type are unknown.
    pub netcdf_indexed: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GridMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nx: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ny: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nz: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nt: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nx_staggered: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ny_staggered: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nz_staggered: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dx_m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dy_m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vertical_levels: Option<usize>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProjectionMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub map_proj: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wrf_core_projection: Option<String>,
    #[serde(default)]
    pub parameters: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileInspection {
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    pub status: InspectStatus,
    pub readable: bool,
    pub complete: bool,
    /// This first contract validates container metadata, WRF dimensions, and
    /// `Times`; it does not claim every multi-gigabyte payload was decoded.
    pub readability_scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(default)]
    pub valid_times: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_time_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initialization_time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub microphysics_scheme: Option<String>,
    #[serde(default)]
    pub grid: GridMetadata,
    #[serde(default)]
    pub projection: ProjectionMetadata,
    #[serde(default)]
    pub dimensions: Vec<DimensionMetadata>,
    #[serde(default)]
    pub variables: Vec<VariableMetadata>,
    #[serde(default)]
    pub initialization_metadata: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub global_attributes: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub missing_metadata: Vec<String>,
    #[serde(default)]
    pub suspicious_metadata: Vec<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub failures: Vec<String>,
}

impl FileInspection {
    fn failed(path: PathBuf, failure: impl Into<String>) -> Self {
        Self {
            path,
            bytes: None,
            sha256: None,
            status: InspectStatus::Failed,
            readable: false,
            complete: false,
            readability_scope: "none".into(),
            container_format: None,
            domain: None,
            valid_times: vec![],
            valid_time_source: None,
            initialization_time: None,
            model: None,
            microphysics_scheme: None,
            grid: GridMetadata::default(),
            projection: ProjectionMetadata::default(),
            dimensions: vec![],
            variables: vec![],
            initialization_metadata: BTreeMap::new(),
            global_attributes: BTreeMap::new(),
            missing_metadata: vec![],
            suspicious_metadata: vec![],
            warnings: vec![],
            failures: vec![failure.into()],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InspectReport {
    pub schema_version: String,
    pub bowecho: BuildIdentity,
    pub input: PathBuf,
    pub status: InspectStatus,
    #[serde(default)]
    pub domains: Vec<DomainSummary>,
    pub files: Vec<FileInspection>,
}

pub fn inspect(
    input: &Path,
    context: &RuntimeContext,
) -> Result<(InspectReport, ExitCode), CliError> {
    let paths = expand_input_files(input)?;
    let files = paths.into_iter().map(inspect_file).collect::<Vec<_>>();

    let status = if files
        .iter()
        .all(|file| file.status == InspectStatus::Complete)
    {
        InspectStatus::Complete
    } else if files
        .iter()
        .all(|file| file.status == InspectStatus::Failed)
    {
        InspectStatus::Failed
    } else {
        InspectStatus::Partial
    };
    let mut domains = BTreeMap::<String, (BTreeSet<String>, usize)>::new();
    for file in &files {
        if let Some(domain) = &file.domain {
            let entry = domains.entry(domain.clone()).or_default();
            entry.1 += 1;
            entry.0.extend(file.valid_times.iter().cloned());
        }
    }
    let domains = domains
        .into_iter()
        .map(|(domain, (valid_times, files))| DomainSummary {
            domain,
            valid_times: valid_times.into_iter().collect(),
            files,
        })
        .collect();
    let report = InspectReport {
        schema_version: INSPECT_SCHEMA_VERSION.into(),
        bowecho: BuildIdentity {
            version: context.bowecho_version.clone(),
            commit: context.bowecho_commit.clone(),
        },
        input: input.to_path_buf(),
        status,
        domains,
        files,
    };
    let code = if status == InspectStatus::Complete {
        ExitCode::Success
    } else {
        ExitCode::Data
    };
    Ok((report, code))
}

fn inspect_file(path: PathBuf) -> FileInspection {
    let before = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) => {
            return FileInspection::failed(path, format!("cannot stat source: {error}"));
        }
    };
    let hash = match sha256_file(&path) {
        Ok(hash) => hash,
        Err(error) => {
            return FileInspection::failed(path, format!("cannot hash source: {error}"));
        }
    };
    let mut inspection = FileInspection::failed(path.clone(), "metadata inspection did not run");
    inspection.bytes = Some(hash.bytes);
    inspection.sha256 = Some(hash.sha256);
    inspection.failures.clear();
    inspection.readability_scope = "container_metadata_wrf_dimensions_and_times".into();

    let nc = match netcrust::open(&path) {
        Ok(file) => file,
        Err(error) => {
            inspection.failures.push(format!("netcrust open: {error}"));
            return inspection;
        }
    };
    inspection.container_format = Some(format!("{:?}", nc.format()));

    match nc.dimensions() {
        Ok(mut dimensions) => {
            dimensions.sort_by(|left, right| left.name().cmp(right.name()));
            inspection.dimensions = dimensions
                .iter()
                .map(|dimension| DimensionMetadata {
                    name: dimension.name().to_owned(),
                    length: dimension.len(),
                    unlimited: dimension.is_unlimited(),
                })
                .collect();
        }
        Err(error) => inspection
            .failures
            .push(format!("enumerate NetCDF dimensions: {error}")),
    }

    match nc.attributes() {
        Ok(mut attributes) => {
            attributes.sort_by(|left, right| left.name().cmp(right.name()));
            for attribute in attributes {
                inspection.global_attributes.insert(
                    attribute.name().to_owned(),
                    attribute_value_to_json(attribute.value()),
                );
            }
        }
        Err(error) => inspection
            .failures
            .push(format!("enumerate global attributes: {error}")),
    }

    let mut indexed_names = BTreeSet::new();
    match nc.variables() {
        Ok(mut variables) => {
            variables.sort_by(|left, right| left.name().cmp(right.name()));
            for variable in variables {
                indexed_names.insert(variable.name().to_owned());
                inspection.variables.push(VariableMetadata {
                    name: variable.name().to_owned(),
                    data_type: data_type_name(variable.dtype()).into(),
                    dimensions: variable
                        .dimensions()
                        .iter()
                        .map(|dimension| DimensionMetadata {
                            name: dimension.name().to_owned(),
                            length: dimension.len(),
                            unlimited: dimension.is_unlimited(),
                        })
                        .collect(),
                    units: string_variable_attribute(&variable, "units"),
                    description: string_variable_attribute(&variable, "description")
                        .or_else(|| string_variable_attribute(&variable, "long_name")),
                    staggering: variable_staggering(&variable),
                    netcdf_indexed: true,
                });
            }
        }
        Err(error) => inspection
            .failures
            .push(format!("enumerate NetCDF variables: {error}")),
    }

    match nc.hdf5_root_datasets() {
        Ok(mut datasets) => {
            datasets.sort_by(|left, right| left.name().cmp(right.name()));
            for dataset in datasets {
                if indexed_names.contains(dataset.name()) {
                    continue;
                }
                inspection.warnings.push(format!(
                    "raw HDF5 dataset '{}' was omitted from the NetCDF index; named dimensions and type are unavailable",
                    dataset.name()
                ));
                inspection.variables.push(VariableMetadata {
                    name: dataset.name().to_owned(),
                    data_type: "unknown_hdf5_fallback".into(),
                    dimensions: dataset
                        .shape()
                        .iter()
                        .enumerate()
                        .map(|(index, &length)| DimensionMetadata {
                            name: format!("axis_{index}"),
                            length: usize::try_from(length).unwrap_or(usize::MAX),
                            unlimited: index == 0 && dataset.has_leading_record_axis(),
                        })
                        .collect(),
                    units: nc
                        .hdf5_dataset_attribute_string(dataset.name(), "units")
                        .map(clean_string),
                    description: nc
                        .hdf5_dataset_attribute_string(dataset.name(), "description")
                        .map(clean_string),
                    staggering: None,
                    netcdf_indexed: false,
                });
            }
            inspection
                .variables
                .sort_by(|left, right| left.name.cmp(&right.name));
        }
        Err(error) => inspection
            .warnings
            .push(format!("raw HDF5 dataset fallback unavailable: {error}")),
    }

    populate_metadata(&mut inspection);

    match wrf_core::WrfFile::open(&path) {
        Ok(file) => {
            cross_check_wrf_core_grid(&mut inspection, &file);
            match file.times() {
                Ok(times) => {
                    let (valid_times, suspicious) = normalize_times(times);
                    inspection.valid_times = valid_times;
                    inspection.valid_time_source = Some("wrf_core:Times".into());
                    inspection.suspicious_metadata.extend(suspicious);
                }
                Err(error) => inspection
                    .failures
                    .push(format!("wrf-core read Times: {error}")),
            }
            match wrf_core::WrfProjection::from_file(&file) {
                Ok(projection) => {
                    inspection.projection.wrf_core_projection = Some(format!("{projection:?}"));
                }
                Err(error) => inspection
                    .suspicious_metadata
                    .push(format!("wrf-core projection validation: {error}")),
            }
        }
        Err(error) => inspection
            .failures
            .push(format!("wrf-core structural read: {error}")),
    }

    if inspection.valid_times.is_empty()
        && let Some(time) = time_from_filename(&path)
    {
        inspection.valid_times.push(time);
        inspection.valid_time_source = Some("filename_fallback".into());
        inspection
            .warnings
            .push("valid time came from the filename because WRF Times was unreadable".into());
    }
    validate_required_metadata(&mut inspection);

    match fs::metadata(&path) {
        Ok(after)
            if after.len() != before.len() || after.modified().ok() != before.modified().ok() =>
        {
            inspection
                .failures
                .push("source changed while it was being hashed/inspected".into());
        }
        Err(error) => inspection
            .failures
            .push(format!("cannot restat source after inspection: {error}")),
        _ => {}
    }

    inspection.readable = !inspection.dimensions.is_empty() && !inspection.variables.is_empty();
    let valid_times_are_canonical = !inspection.valid_times.is_empty()
        && inspection
            .valid_times
            .iter()
            .all(|time| normalize_wrf_time(time).is_some());
    inspection.complete = inspection.readable
        && inspection.failures.is_empty()
        && inspection.missing_metadata.is_empty()
        && inspection.suspicious_metadata.is_empty()
        && valid_times_are_canonical
        && inspection.valid_time_source.as_deref() == Some("wrf_core:Times");
    inspection.status = if inspection.complete {
        InspectStatus::Complete
    } else if inspection.readable {
        InspectStatus::Partial
    } else {
        InspectStatus::Failed
    };
    inspection
}

fn populate_metadata(inspection: &mut FileInspection) {
    let dimensions = inspection
        .dimensions
        .iter()
        .map(|dimension| (dimension.name.as_str(), dimension.length))
        .collect::<BTreeMap<_, _>>();
    inspection.grid = GridMetadata {
        nx: dimensions.get("west_east").copied(),
        ny: dimensions.get("south_north").copied(),
        nz: dimensions.get("bottom_top").copied(),
        nt: dimensions.get("Time").copied(),
        nx_staggered: dimensions.get("west_east_stag").copied(),
        ny_staggered: dimensions.get("south_north_stag").copied(),
        nz_staggered: dimensions.get("bottom_top_stag").copied(),
        dx_m: json_f64(inspection.global_attributes.get("DX")),
        dy_m: json_f64(inspection.global_attributes.get("DY")),
        vertical_levels: dimensions.get("bottom_top").copied(),
    };

    let map_proj = json_f64(inspection.global_attributes.get("MAP_PROJ")).map(|value| value as i32);
    inspection.projection.map_proj = map_proj;
    inspection.projection.name = map_proj.map(|value| {
        match value {
            1 => "lambert_conformal",
            2 => "polar_stereographic",
            3 => "mercator",
            6 => "latitude_longitude",
            _ => "unknown",
        }
        .to_owned()
    });
    for name in [
        "MAP_PROJ",
        "TRUELAT1",
        "TRUELAT2",
        "STAND_LON",
        "CEN_LAT",
        "CEN_LON",
        "MOAD_CEN_LAT",
        "POLE_LAT",
        "POLE_LON",
        "DX",
        "DY",
    ] {
        if let Some(value) = inspection.global_attributes.get(name) {
            inspection
                .projection
                .parameters
                .insert(name.to_owned(), value.clone());
        }
    }

    for name in [
        "START_DATE",
        "SIMULATION_START_DATE",
        "MODEL_INITIALIZATION_TIME",
        "JULYR",
        "JULDAY",
        "GMT",
        "GRID_ID",
        "PARENT_ID",
        "PARENT_GRID_RATIO",
        "TITLE",
        "MMINLU",
        "MP_PHYSICS",
        "CU_PHYSICS",
        "SF_SURFACE_PHYSICS",
        "RA_LW_PHYSICS",
        "RA_SW_PHYSICS",
    ] {
        if let Some(value) = inspection.global_attributes.get(name) {
            inspection
                .initialization_metadata
                .insert(name.to_owned(), value.clone());
        }
    }

    let raw_initialization_time = [
        "START_DATE",
        "SIMULATION_START_DATE",
        "MODEL_INITIALIZATION_TIME",
    ]
    .into_iter()
    .find_map(|name| json_string(inspection.global_attributes.get(name)));
    inspection.initialization_time = raw_initialization_time
        .as_deref()
        .and_then(normalize_wrf_time);
    if let Some(raw) = raw_initialization_time
        && inspection.initialization_time.is_none()
    {
        inspection
            .suspicious_metadata
            .push(format!("initialization timestamp is invalid: '{raw}'"));
    }
    inspection.model = json_string(inspection.global_attributes.get("TITLE"));
    inspection.microphysics_scheme = json_f64(inspection.global_attributes.get("MP_PHYSICS"))
        .map(|value| format!("WRF MP_PHYSICS={}", value as i32));

    let filename_domain = domain_from_filename(&inspection.path);
    let attribute_domain = json_f64(inspection.global_attributes.get("GRID_ID"))
        .map(|value| format!("d{:02}", value as i32));
    if let (Some(filename), Some(attribute)) = (&filename_domain, &attribute_domain)
        && filename != attribute
    {
        inspection.suspicious_metadata.push(format!(
            "filename domain {filename} disagrees with GRID_ID {attribute}"
        ));
    }
    inspection.domain = attribute_domain.or(filename_domain);

    if inspection
        .grid
        .dx_m
        .is_some_and(|value| !value.is_finite() || value <= 0.0)
    {
        inspection
            .suspicious_metadata
            .push("DX is not a positive finite spacing".into());
    }
    if inspection
        .grid
        .dy_m
        .is_some_and(|value| !value.is_finite() || value <= 0.0)
    {
        inspection
            .suspicious_metadata
            .push("DY is not a positive finite spacing".into());
    }
    if inspection.projection.name.as_deref() == Some("unknown") {
        inspection.suspicious_metadata.push(format!(
            "unsupported MAP_PROJ value {}",
            inspection.projection.map_proj.unwrap_or_default()
        ));
    }
}

fn cross_check_wrf_core_grid(inspection: &mut FileInspection, file: &wrf_core::WrfFile) {
    for (name, listed, core) in [
        ("west_east", inspection.grid.nx, file.nx),
        ("south_north", inspection.grid.ny, file.ny),
        ("bottom_top", inspection.grid.nz, file.nz),
        ("Time", inspection.grid.nt, file.nt),
        ("west_east_stag", inspection.grid.nx_staggered, file.nx_stag),
        (
            "south_north_stag",
            inspection.grid.ny_staggered,
            file.ny_stag,
        ),
        (
            "bottom_top_stag",
            inspection.grid.nz_staggered,
            file.nz_stag,
        ),
    ] {
        if let Some(listed) = listed
            && listed != core
        {
            inspection.suspicious_metadata.push(format!(
                "netcrust {name}={listed} disagrees with wrf-core {name}={core}"
            ));
        }
    }
    if let Some(dx) = inspection.grid.dx_m
        && (dx - file.dx).abs() > f64::EPSILON * dx.abs().max(1.0)
    {
        inspection.suspicious_metadata.push(format!(
            "netcrust DX={dx} disagrees with wrf-core DX={}",
            file.dx
        ));
    }
    if let Some(dy) = inspection.grid.dy_m
        && (dy - file.dy).abs() > f64::EPSILON * dy.abs().max(1.0)
    {
        inspection.suspicious_metadata.push(format!(
            "netcrust DY={dy} disagrees with wrf-core DY={}",
            file.dy
        ));
    }
}

fn validate_required_metadata(inspection: &mut FileInspection) {
    for (name, present) in [
        ("dimension Time", inspection.grid.nt.is_some()),
        ("dimension west_east", inspection.grid.nx.is_some()),
        ("dimension south_north", inspection.grid.ny.is_some()),
        ("dimension bottom_top", inspection.grid.nz.is_some()),
        ("global attribute DX", inspection.grid.dx_m.is_some()),
        ("global attribute DY", inspection.grid.dy_m.is_some()),
        (
            "global attribute MAP_PROJ",
            inspection.projection.map_proj.is_some(),
        ),
        (
            "global attribute START_DATE or SIMULATION_START_DATE",
            inspection.initialization_time.is_some(),
        ),
        (
            "variable Times",
            inspection
                .variables
                .iter()
                .any(|variable| variable.name == "Times"),
        ),
        (
            "variable T",
            inspection
                .variables
                .iter()
                .any(|variable| variable.name == "T"),
        ),
    ] {
        if !present {
            inspection.missing_metadata.push(name.into());
        }
    }
    if let (Some(nt), times) = (inspection.grid.nt, inspection.valid_times.len())
        && times > 0
        && nt != times
    {
        inspection.suspicious_metadata.push(format!(
            "Time dimension has {nt} record(s), but Times yielded {times}"
        ));
    }
    inspection.missing_metadata.sort();
    inspection.missing_metadata.dedup();
    inspection.suspicious_metadata.sort();
    inspection.suspicious_metadata.dedup();
}

fn string_variable_attribute(variable: &netcrust::Variable, name: &str) -> Option<String> {
    variable
        .attribute(name)
        .and_then(|attribute| attribute.as_string())
        .map(|value| clean_string(value.to_owned()))
        .filter(|value| !value.is_empty())
}

fn variable_staggering(variable: &netcrust::Variable) -> Option<String> {
    if let Some(staggering) = string_variable_attribute(variable, "stagger")
        .or_else(|| string_variable_attribute(variable, "staggering"))
        && !staggering.is_empty()
    {
        return Some(staggering);
    }
    let axes = variable
        .dimensions()
        .iter()
        .filter_map(|dimension| match dimension.name() {
            "west_east_stag" => Some("x"),
            "south_north_stag" => Some("y"),
            "bottom_top_stag" => Some("z"),
            _ => None,
        })
        .collect::<Vec<_>>();
    (!axes.is_empty()).then(|| axes.join(","))
}

fn data_type_name(data_type: &DataType) -> &'static str {
    match data_type {
        DataType::I8 => "i8",
        DataType::Char => "char",
        DataType::I16 => "i16",
        DataType::I32 => "i32",
        DataType::F32 => "f32",
        DataType::F64 => "f64",
        DataType::U8 => "u8",
        DataType::U16 => "u16",
        DataType::U32 => "u32",
        DataType::I64 => "i64",
        DataType::U64 => "u64",
        DataType::String => "string",
        DataType::Compound => "compound",
        DataType::Opaque => "opaque",
        DataType::Array => "array",
        DataType::VLen => "vlen",
    }
}

fn attribute_value_to_json(value: &AttributeValue) -> serde_json::Value {
    use AttributeValue::*;
    match value {
        Bytes(values) => serde_json::json!(values),
        Chars(value) => serde_json::Value::String(clean_string(value.clone())),
        Shorts(values) => serde_json::json!(values),
        Ints(values) => serde_json::json!(values),
        Floats(values) => float_array_json(values.iter().map(|&value| value as f64)),
        Doubles(values) => float_array_json(values.iter().copied()),
        UBytes(values) => serde_json::json!(values),
        UShorts(values) => serde_json::json!(values),
        UInts(values) => serde_json::json!(values),
        Int64s(values) => serde_json::json!(values),
        UInt64s(values) => serde_json::json!(values),
        Strings(values) if values.len() == 1 => {
            serde_json::Value::String(clean_string(values[0].clone()))
        }
        Strings(values) => serde_json::Value::Array(
            values
                .iter()
                .cloned()
                .map(clean_string)
                .map(serde_json::Value::String)
                .collect(),
        ),
    }
}

fn float_array_json(values: impl IntoIterator<Item = f64>) -> serde_json::Value {
    let mut values = values
        .into_iter()
        .map(|value| {
            serde_json::Number::from_f64(value)
                .map(serde_json::Value::Number)
                .unwrap_or_else(|| serde_json::Value::String(value.to_string()))
        })
        .collect::<Vec<_>>();
    if values.len() == 1 {
        values.pop().unwrap()
    } else {
        serde_json::Value::Array(values)
    }
}

fn json_f64(value: Option<&serde_json::Value>) -> Option<f64> {
    match value? {
        serde_json::Value::Number(number) => number.as_f64(),
        serde_json::Value::Array(values) => values.first()?.as_f64(),
        _ => None,
    }
}

fn json_string(value: Option<&serde_json::Value>) -> Option<String> {
    match value? {
        serde_json::Value::String(value) => Some(clean_string(value.clone())),
        serde_json::Value::Array(values) => values
            .first()?
            .as_str()
            .map(|value| clean_string(value.into())),
        _ => None,
    }
    .filter(|value| !value.is_empty())
}

fn clean_string(mut value: String) -> String {
    while value.ends_with(['\0', ' ']) {
        value.pop();
    }
    value.trim().to_owned()
}

fn normalize_times(times: Vec<String>) -> (Vec<String>, Vec<String>) {
    let mut normalized = Vec::with_capacity(times.len());
    let mut suspicious = Vec::new();
    for time in times {
        match normalize_wrf_time(&time) {
            Some(time) => normalized.push(time),
            None => {
                suspicious.push(format!("WRF Times contains an invalid timestamp '{time}'"));
                normalized.push(clean_string(time));
            }
        }
    }
    (normalized, suspicious)
}

fn normalize_wrf_time(value: &str) -> Option<String> {
    let value = clean_string(value.to_owned());
    for format in [
        "%Y-%m-%d_%H:%M:%S",
        "%Y-%m-%d_%H_%M_%S",
        "%Y_%m_%d_%H_%M_%S",
        "%Y-%m-%dT%H:%M:%SZ",
    ] {
        if let Ok(time) = NaiveDateTime::parse_from_str(&value, format) {
            return Some(format!("{}Z", time.format("%Y-%m-%dT%H:%M:%S")));
        }
    }
    None
}

fn domain_from_filename(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?.to_ascii_lowercase();
    let start = name.find("wrfout_d")? + "wrfout_d".len();
    let digits = name
        .get(start..)?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    (!digits.is_empty()).then(|| format!("d{digits}"))
}

fn time_from_filename(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let lower = name.to_ascii_lowercase();
    let domain_start = lower.find("wrfout_d")? + "wrfout_d".len();
    let after_domain = name.get(domain_start..)?;
    let offset = after_domain.find(|character: char| !character.is_ascii_digit())?;
    let tail = after_domain.get(offset + 1..)?;
    let parts = tail
        .split(|character: char| !character.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .take(6)
        .map(str::parse::<u32>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    if parts.len() != 6 {
        return None;
    }
    let date = NaiveDate::from_ymd_opt(parts[0] as i32, parts[1], parts[2])?;
    let time = date.and_hms_opt(parts[3], parts[4], parts[5])?;
    Some(format!("{}Z", time.format("%Y-%m-%dT%H:%M:%S")))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptKind {
    Source,
    Artifact,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReceiptVerification {
    pub kind: ReceiptKind,
    pub label: String,
    pub declared_path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_path: Option<PathBuf>,
    pub expected_bytes: u64,
    pub expected_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_sha256: Option<String>,
    pub exists: bool,
    pub size_matches: bool,
    pub sha256_matches: bool,
    pub verified: bool,
    #[serde(default)]
    pub failures: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VerifyReport {
    pub schema_version: String,
    pub artifact_schema_version: String,
    pub artifact_status: ArtifactStatus,
    /// True only for a complete processing receipt with at least one product.
    /// Hash integrity alone must not authorize transfer/deletion of a partial
    /// or empty run.
    pub processing_complete: bool,
    pub manifest_path: PathBuf,
    pub manifest_bytes: u64,
    pub manifest_sha256: String,
    pub verified: bool,
    #[serde(default)]
    pub contract_failures: Vec<String>,
    #[serde(default)]
    pub receipts: Vec<ReceiptVerification>,
}

pub fn verify(manifest_path: &Path) -> Result<(VerifyReport, ExitCode), CliError> {
    const MAX_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;
    let manifest_bytes = fs::metadata(manifest_path)
        .map_err(|error| {
            CliError::input(format!(
                "cannot stat artifact manifest {}: {error}",
                manifest_path.display()
            ))
        })?
        .len();
    if manifest_bytes > MAX_MANIFEST_BYTES {
        return Err(CliError::data(format!(
            "artifact manifest is {} bytes; maximum is {MAX_MANIFEST_BYTES}",
            manifest_bytes
        )));
    }
    let bytes = fs::read(manifest_path).map_err(|error| {
        CliError::input(format!(
            "cannot read artifact manifest {}: {error}",
            manifest_path.display()
        ))
    })?;
    let manifest: ArtifactManifest = serde_json::from_slice(&bytes).map_err(|error| {
        CliError::data(format!(
            "invalid artifact manifest {}: {error}",
            manifest_path.display()
        ))
    })?;
    let manifest_hash = sha256_reader(&bytes[..]).map_err(|error| {
        CliError::input(format!(
            "cannot hash artifact manifest {}: {error}",
            manifest_path.display()
        ))
    })?;
    let mut contract_failures = manifest.validate_contract();
    let processing_complete = manifest.status == ArtifactStatus::Complete
        && !manifest.products.is_empty()
        && manifest.failures.is_empty();
    if manifest.status != ArtifactStatus::Complete {
        contract_failures.push(format!(
            "artifact status is {:?}; only complete manifests pass verification",
            manifest.status
        ));
    }
    if manifest.products.is_empty() {
        contract_failures.push("artifact manifest declares no generated products".into());
    }
    let manifest_directory = manifest_path.parent().unwrap_or_else(|| Path::new("."));
    let mut receipts = Vec::with_capacity(manifest.sources.len() + manifest.products.len());
    let mut declared_artifacts = BTreeSet::new();

    for source in &manifest.sources {
        receipts.push(verify_receipt(
            ReceiptKind::Source,
            source.path.display().to_string(),
            &source.path,
            source.bytes,
            &source.sha256,
            manifest_directory,
            true,
        ));
    }
    for product in &manifest.products {
        let declared = &product.artifact.relative_path;
        if !declared_artifacts.insert(declared.clone()) {
            contract_failures.push(format!(
                "multiple products declare artifact path {}",
                declared.display()
            ));
        }
        let mut receipt = verify_receipt(
            ReceiptKind::Artifact,
            product.name.clone(),
            declared,
            product.artifact.bytes,
            &product.artifact.sha256,
            manifest_directory,
            false,
        );
        verify_artifact_image(product, &mut receipt);
        receipts.push(receipt);
    }
    let verified = contract_failures.is_empty() && receipts.iter().all(|receipt| receipt.verified);
    let report = VerifyReport {
        schema_version: VERIFY_SCHEMA_VERSION.into(),
        artifact_schema_version: manifest.schema_version,
        artifact_status: manifest.status,
        processing_complete,
        manifest_path: manifest_path.to_path_buf(),
        manifest_bytes: manifest_hash.bytes,
        manifest_sha256: manifest_hash.sha256,
        verified,
        contract_failures,
        receipts,
    };
    let code = if report.verified {
        ExitCode::Success
    } else {
        ExitCode::Verification
    };
    Ok((report, code))
}

fn verify_receipt(
    kind: ReceiptKind,
    label: String,
    declared_path: &Path,
    expected_bytes: u64,
    expected_sha256: &str,
    manifest_directory: &Path,
    allow_absolute: bool,
) -> ReceiptVerification {
    let mut receipt = ReceiptVerification {
        kind,
        label,
        declared_path: declared_path.to_path_buf(),
        resolved_path: None,
        expected_bytes,
        expected_sha256: expected_sha256.to_ascii_lowercase(),
        actual_bytes: None,
        actual_sha256: None,
        exists: false,
        size_matches: false,
        sha256_matches: false,
        verified: false,
        failures: vec![],
    };
    let path = match resolve_receipt_path(manifest_directory, declared_path, allow_absolute) {
        Ok(path) => path,
        Err(error) => {
            receipt.failures.push(error);
            return receipt;
        }
    };
    receipt.resolved_path = Some(path.clone());
    if !allow_absolute {
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                receipt
                    .failures
                    .push(format!("cannot inspect declared artifact: {error}"));
                return receipt;
            }
        };
        if metadata.file_type().is_symlink() {
            receipt
                .failures
                .push("artifact receipt resolves through a symbolic link".into());
            return receipt;
        }
        let canonical_root = fs::canonicalize(manifest_directory);
        let canonical_path = fs::canonicalize(&path);
        match (canonical_root, canonical_path) {
            (Ok(root), Ok(resolved)) if resolved.starts_with(&root) => {}
            (Ok(_), Ok(resolved)) => {
                receipt.failures.push(format!(
                    "artifact receipt resolves outside the manifest directory: {}",
                    resolved.display()
                ));
                return receipt;
            }
            (Err(error), _) | (_, Err(error)) => {
                receipt.failures.push(format!(
                    "cannot canonicalize artifact containment boundary: {error}"
                ));
                return receipt;
            }
        }
    }
    receipt.exists = path.is_file();
    if !receipt.exists {
        receipt
            .failures
            .push(format!("declared file does not exist: {}", path.display()));
        return receipt;
    }
    let before = fs::metadata(&path).ok();
    match sha256_file(&path) {
        Ok(actual) => {
            receipt.actual_bytes = Some(actual.bytes);
            receipt.actual_sha256 = Some(actual.sha256.clone());
            receipt.size_matches = actual.bytes == expected_bytes;
            receipt.sha256_matches = actual.sha256.eq_ignore_ascii_case(expected_sha256);
            if !receipt.size_matches {
                receipt.failures.push(format!(
                    "byte count mismatch: expected {expected_bytes}, got {}",
                    actual.bytes
                ));
            }
            if !receipt.sha256_matches {
                receipt.failures.push(format!(
                    "SHA-256 mismatch: expected {}, got {}",
                    expected_sha256, actual.sha256
                ));
            }
            receipt.verified = receipt.size_matches && receipt.sha256_matches;
            let after = fs::metadata(&path).ok();
            if before
                .as_ref()
                .map(|metadata| (metadata.len(), metadata.modified().ok()))
                != after
                    .as_ref()
                    .map(|metadata| (metadata.len(), metadata.modified().ok()))
            {
                receipt
                    .failures
                    .push("file changed while its receipt was being verified".into());
                receipt.verified = false;
            }
        }
        Err(error) => receipt
            .failures
            .push(format!("cannot hash {}: {error}", path.display())),
    }
    receipt
}

fn verify_artifact_image(
    product: &crate::artifact::ProductReceipt,
    receipt: &mut ReceiptVerification,
) {
    if !receipt.verified {
        return;
    }
    let Some(path) = receipt.resolved_path.as_ref() else {
        return;
    };
    let expected_format = match product.artifact.mime_type.as_str() {
        "image/png" => image::ImageFormat::Png,
        "image/webp" => image::ImageFormat::WebP,
        other => {
            receipt.failures.push(format!(
                "unsupported artifact MIME type for image verification: {other}"
            ));
            receipt.verified = false;
            return;
        }
    };
    let reader =
        match image::ImageReader::open(path).and_then(|reader| reader.with_guessed_format()) {
            Ok(reader) => reader,
            Err(error) => {
                receipt
                    .failures
                    .push(format!("cannot inspect artifact image header: {error}"));
                receipt.verified = false;
                return;
            }
        };
    if reader.format() != Some(expected_format) {
        receipt.failures.push(format!(
            "artifact encoding does not match declared MIME type {}",
            product.artifact.mime_type
        ));
        receipt.verified = false;
        return;
    }
    match reader.into_dimensions() {
        Ok((width, height))
            if product.artifact.width == Some(width) && product.artifact.height == Some(height) => {
        }
        Ok((width, height)) => {
            receipt.failures.push(format!(
                "artifact dimensions mismatch: declared {:?}x{:?}, decoded {width}x{height}",
                product.artifact.width, product.artifact.height
            ));
            receipt.verified = false;
        }
        Err(error) => {
            receipt
                .failures
                .push(format!("cannot decode artifact image dimensions: {error}"));
            receipt.verified = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::artifact::{
        ARTIFACT_SCHEMA_VERSION, ArtifactFileReceipt, ArtifactStatus, DerivationMethod,
        ExperimentTrack, FiniteStatistics, GridReceipt, ProductReceipt, ProvenanceHashes,
        SourceReceipt,
    };
    use crate::fs::{sha256_file, write_json_atomic};

    use super::*;

    #[test]
    fn corrupt_explicit_input_is_reported_not_passed() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("wrfout_d01_2026-07-22_00_00_00");
        fs::write(&path, b"truncated-not-netcdf").unwrap();
        let (report, code) = inspect(&path, &RuntimeContext::new("test", "test")).unwrap();
        assert_eq!(code, ExitCode::Data);
        assert_eq!(report.status, InspectStatus::Failed);
        assert!(!report.files[0].readable);
        assert!(report.files[0].failures[0].contains("netcrust open"));
    }

    #[test]
    fn verify_recalculates_source_and_artifact_receipts() {
        let root = tempfile::tempdir().unwrap();
        let source_path = root.path().join("wrfout_d01");
        let artifact_path = root.path().join("plots").join("ref.png");
        fs::create_dir_all(artifact_path.parent().unwrap()).unwrap();
        fs::write(&source_path, b"source").unwrap();
        image::RgbaImage::from_pixel(2, 3, image::Rgba([1, 2, 3, 255]))
            .save(&artifact_path)
            .unwrap();
        let source_hash = sha256_file(&source_path).unwrap();
        let artifact_hash = sha256_file(&artifact_path).unwrap();
        let mut manifest = ArtifactManifest {
            schema_version: ARTIFACT_SCHEMA_VERSION.into(),
            status: ArtifactStatus::Complete,
            case_id: "case".into(),
            run_id: "run".into(),
            member_id: None,
            experiment_track: ExperimentTrack::WrfParity,
            model: "WRF".into(),
            microphysics_scheme: None,
            provenance: ProvenanceHashes::default(),
            bowecho: BuildIdentity {
                version: "test".into(),
                commit: "test".into(),
            },
            sources: vec![SourceReceipt {
                path: PathBuf::from("wrfout_d01"),
                bytes: source_hash.bytes,
                sha256: source_hash.sha256,
                domain: Some("d01".into()),
                initialization_time: None,
                valid_times: vec![],
                grid: GridReceipt::default(),
            }],
            products: vec![ProductReceipt {
                name: "reflectivity".into(),
                domain: Some("d01".into()),
                initialization_time: Some("2026-07-22T00:00:00Z".into()),
                valid_time: Some("2026-07-22T00:00:00Z".into()),
                storage_slot: Some(0),
                lead_seconds: Some(0),
                units: "dBZ".into(),
                source_variables: vec!["REFL_10CM".into()],
                derivation: DerivationMethod::Direct,
                artifact: ArtifactFileReceipt {
                    relative_path: PathBuf::from("plots/ref.png"),
                    mime_type: "image/png".into(),
                    width: Some(2),
                    height: Some(3),
                    bytes: artifact_hash.bytes,
                    sha256: artifact_hash.sha256,
                },
                statistics: FiniteStatistics::default(),
            }],
            warnings: vec![],
            unavailable_products: vec![],
            failures: vec![],
        };
        let manifest_path = root.path().join("artifact-manifest.json");
        write_json_atomic(&manifest_path, &manifest).unwrap();

        let (report, code) = verify(&manifest_path).unwrap();
        assert_eq!(code, ExitCode::Success);
        assert!(report.verified);
        assert_eq!(report.receipts.len(), 2);

        manifest.products[0].artifact.width = Some(99);
        write_json_atomic(&manifest_path, &manifest).unwrap();
        let (report, code) = verify(&manifest_path).unwrap();
        assert_eq!(code, ExitCode::Verification);
        assert!(
            report.receipts[1]
                .failures
                .iter()
                .any(|failure| failure.contains("dimensions mismatch"))
        );

        manifest.products[0].artifact.width = Some(2);
        manifest.products[0].artifact.mime_type = "image/webp".into();
        write_json_atomic(&manifest_path, &manifest).unwrap();
        let (report, code) = verify(&manifest_path).unwrap();
        assert_eq!(code, ExitCode::Verification);
        assert!(
            report.receipts[1]
                .failures
                .iter()
                .any(|failure| failure.contains("encoding does not match"))
        );

        manifest.products[0].artifact.mime_type = "image/png".into();
        write_json_atomic(&manifest_path, &manifest).unwrap();
        fs::write(&artifact_path, b"tampered").unwrap();
        let (report, code) = verify(&manifest_path).unwrap();
        assert_eq!(code, ExitCode::Verification);
        assert!(!report.verified);
        assert!(!report.receipts[1].sha256_matches);
    }

    #[test]
    fn verify_does_not_certify_partial_or_empty_processing_receipts() {
        let root = tempfile::tempdir().unwrap();
        let source_path = root.path().join("wrfout_d01");
        fs::write(&source_path, b"source").unwrap();
        let source_hash = sha256_file(&source_path).unwrap();
        let base = ArtifactManifest {
            schema_version: ARTIFACT_SCHEMA_VERSION.into(),
            status: ArtifactStatus::Partial,
            case_id: "case".into(),
            run_id: "run".into(),
            member_id: None,
            experiment_track: ExperimentTrack::WrfParity,
            model: "WRF".into(),
            microphysics_scheme: None,
            provenance: ProvenanceHashes::default(),
            bowecho: BuildIdentity {
                version: "test".into(),
                commit: "test".into(),
            },
            sources: vec![SourceReceipt {
                path: PathBuf::from("wrfout_d01"),
                bytes: source_hash.bytes,
                sha256: source_hash.sha256,
                domain: Some("d01".into()),
                initialization_time: Some("2026-07-22T00:00:00Z".into()),
                valid_times: vec!["2026-07-22T00:00:00Z".into()],
                grid: GridReceipt::default(),
            }],
            products: vec![],
            warnings: vec![],
            unavailable_products: vec![],
            failures: vec![crate::artifact::FailureRecord {
                stage: "render".into(),
                path: None,
                reason: "no products rendered".into(),
            }],
        };
        let manifest_path = root.path().join("artifact-manifest.json");
        write_json_atomic(&manifest_path, &base).unwrap();

        let (report, code) = verify(&manifest_path).unwrap();
        assert_eq!(code, ExitCode::Verification);
        assert!(!report.verified);
        assert!(!report.processing_complete);
        assert_eq!(report.artifact_status, ArtifactStatus::Partial);
        assert!(
            report
                .contract_failures
                .iter()
                .any(|failure| failure.contains("no generated products"))
        );
    }

    #[test]
    fn filename_extractors_are_strict_and_canonical() {
        let path = Path::new("wrfout_d12_1974-04-03_23_03_00");
        assert_eq!(domain_from_filename(path).as_deref(), Some("d12"));
        assert_eq!(
            time_from_filename(path).as_deref(),
            Some("1974-04-03T23:03:00Z")
        );
    }

    #[test]
    #[ignore = "requires BOWECHO_WRF_FIXTURE pointing at a real local wrfout"]
    fn inspect_real_local_wrf_fixture() {
        let path = PathBuf::from(
            std::env::var_os("BOWECHO_WRF_FIXTURE")
                .expect("set BOWECHO_WRF_FIXTURE to a real wrfout"),
        );
        let (report, _) = inspect(&path, &RuntimeContext::new("test", "test")).unwrap();
        let file = &report.files[0];
        assert!(file.readable, "failures: {:?}", file.failures);
        assert!(file.bytes.is_some_and(|bytes| bytes > 0));
        assert!(file.sha256.as_ref().is_some_and(|hash| hash.len() == 64));
        assert!(!file.variables.is_empty());
        eprintln!(
            "status={:?} domain={:?} times={} vars={} missing={} suspicious={} failures={}",
            file.status,
            file.domain,
            file.valid_times.len(),
            file.variables.len(),
            file.missing_metadata.len(),
            file.suspicious_metadata.len(),
            file.failures.len()
        );
    }
}
