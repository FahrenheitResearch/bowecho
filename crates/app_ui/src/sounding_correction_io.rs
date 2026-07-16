//! Pure persistence, interchange, and batch primitives for sounding correction.
//!
//! The live sounding host deliberately remains outside this module. These
//! helpers do not open dialogs, mutate the model store, recompute diagnostics,
//! or capture a rendered sounding. A host can therefore run serialization and
//! batch planning off the UI thread and explicitly decide when to apply a
//! recipe or render a member.

// Some interchange/statistical helpers are intentionally broader than the
// first UI surface so project files and batch exports can evolve without
// changing their core validation or deterministic enumeration semantics.
#![allow(dead_code)]

use std::collections::BTreeSet;
use std::fmt::Write as _;

use rustwx_sounding::{SoundingColumn, SoundingMetadata};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::sounding_correction::{
    CorrectionRecipe, CorrectionResult, MoistureTarget, QcIssue, QcIssueKind, QcSeverity,
    ThermalTarget, WindTarget,
};

pub(crate) const CORRECTION_BUNDLE_FORMAT: &str = "bowecho-sounding-correction-v1";
pub(crate) const CORRECTED_PROFILE_CSV_FORMAT: &str = "bowecho-corrected-sounding-profile-v1";
pub(crate) const SHARPPY_RAW_TEXT_FORMAT: &str = "spc-sharppy-raw-profile";

const MS_TO_KT: f64 = 1.943_844_492_440_604_6;
const RAW_MISSING_THRESHOLD: f64 = -9_000.0;

#[derive(Debug, Error)]
pub(crate) enum CorrectionIoError {
    #[error("correction JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported correction format `{0}`")]
    UnsupportedFormat(String),
    #[error("invalid correction bundle: {0}")]
    InvalidBundle(String),
    #[error("invalid sounding column: {0}")]
    InvalidColumn(String),
    #[error("invalid SPC/SHARPpy RAW sounding: {0}")]
    InvalidRaw(String),
    #[error(
        "correction project belongs to a different source profile (project {project_sha256}, current {current_sha256})"
    )]
    SourceMismatch {
        project_sha256: String,
        current_sha256: String,
    },
    #[error("invalid batch plan: {0}")]
    InvalidBatch(String),
}

/// Host-supplied model/store identity that is not part of `SoundingColumn`.
///
/// All fields are optional because imported profiles and legacy stores may not
/// carry a model run identity. The physical-column fingerprint remains the
/// authoritative guard against silently applying a project to another source.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct CorrectionSourceContext {
    #[serde(default)]
    pub(crate) source_kind: Option<String>,
    #[serde(default)]
    pub(crate) source_identity: Option<String>,
    #[serde(default)]
    pub(crate) model: Option<String>,
    #[serde(default)]
    pub(crate) run: Option<String>,
    #[serde(default)]
    pub(crate) lead: Option<String>,
}

/// Complete source identity exported with a correction recipe.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct CorrectionSourceProvenance {
    #[serde(default)]
    pub(crate) source_kind: Option<String>,
    #[serde(default)]
    pub(crate) source_identity: Option<String>,
    #[serde(default)]
    pub(crate) model: Option<String>,
    #[serde(default)]
    pub(crate) run: Option<String>,
    #[serde(default)]
    pub(crate) lead: Option<String>,
    pub(crate) station_id: String,
    pub(crate) valid_time: String,
    pub(crate) latitude_deg: Option<f64>,
    pub(crate) longitude_deg: Option<f64>,
    pub(crate) elevation_m: Option<f64>,
    pub(crate) sample_method: Option<String>,
    pub(crate) box_radius_lat_deg: Option<f64>,
    pub(crate) box_radius_lon_deg: Option<f64>,
    pub(crate) level_count: usize,
    pub(crate) profile_sha256: String,
}

impl CorrectionSourceProvenance {
    pub(crate) fn from_column(
        column: &SoundingColumn,
        context: CorrectionSourceContext,
    ) -> Result<Self, CorrectionIoError> {
        column
            .validate()
            .map_err(|error| CorrectionIoError::InvalidColumn(error.to_string()))?;
        Ok(Self {
            source_kind: normalized_optional_text(context.source_kind),
            source_identity: normalized_optional_text(context.source_identity),
            model: normalized_optional_text(context.model),
            run: normalized_optional_text(context.run),
            lead: normalized_optional_text(context.lead),
            station_id: column.metadata.station_id.trim().to_owned(),
            valid_time: column.metadata.valid_time.trim().to_owned(),
            latitude_deg: finite_option(column.metadata.latitude_deg),
            longitude_deg: finite_option(column.metadata.longitude_deg),
            elevation_m: finite_option(column.metadata.elevation_m),
            sample_method: normalized_optional_text(column.metadata.sample_method.clone()),
            box_radius_lat_deg: finite_option(column.metadata.box_radius_lat_deg),
            box_radius_lon_deg: finite_option(column.metadata.box_radius_lon_deg),
            level_count: column.len(),
            profile_sha256: sounding_column_sha256(column),
        })
    }

    pub(crate) fn matches_column(&self, column: &SoundingColumn) -> bool {
        self.level_count == column.len() && self.profile_sha256 == sounding_column_sha256(column)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct CorrectionQcRecord {
    pub(crate) severity: String,
    pub(crate) kind: String,
    pub(crate) correction_index: Option<usize>,
    pub(crate) level_index: Option<usize>,
    pub(crate) end_level_index: Option<usize>,
    pub(crate) message: String,
}

impl From<&QcIssue> for CorrectionQcRecord {
    fn from(issue: &QcIssue) -> Self {
        Self {
            severity: qc_severity_key(issue.severity).to_owned(),
            kind: qc_kind_key(issue.kind).to_owned(),
            correction_index: issue.correction_index,
            level_index: issue.level_index,
            end_level_index: issue.end_level_index,
            message: issue.message.clone(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct ConvectiveAdjustmentProvenance {
    pub(crate) attempted: bool,
    pub(crate) applied: bool,
    pub(crate) adjusted_levels: usize,
    pub(crate) mixed_blocks: usize,
    pub(crate) sensible_enthalpy_before_j_kg: f64,
    pub(crate) sensible_enthalpy_after_j_kg: f64,
    pub(crate) relative_enthalpy_residual: f64,
    pub(crate) aborted_reason: Option<String>,
}

/// Provenance for one evaluated project. This records engine output only; it
/// does not claim that diagnostics or an image were rendered by the host.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct CorrectionApplicationProvenance {
    pub(crate) recipe_sha256: String,
    pub(crate) corrected_profile_sha256: String,
    pub(crate) active_correction_levels: usize,
    pub(crate) convective_adjustment_enabled: bool,
    pub(crate) qc: Vec<CorrectionQcRecord>,
    pub(crate) convective_adjustment: ConvectiveAdjustmentProvenance,
}

impl CorrectionApplicationProvenance {
    pub(crate) fn from_result(
        recipe: &CorrectionRecipe,
        result: &CorrectionResult,
    ) -> Result<Self, CorrectionIoError> {
        let report = &result.convective_adjustment;
        Ok(Self {
            recipe_sha256: recipe_sha256(recipe)?,
            corrected_profile_sha256: sounding_column_sha256(&result.column),
            active_correction_levels: recipe.active_level_count(),
            convective_adjustment_enabled: recipe.convective_adjustment.enabled,
            qc: result.issues.iter().map(CorrectionQcRecord::from).collect(),
            convective_adjustment: ConvectiveAdjustmentProvenance {
                attempted: report.attempted,
                applied: report.applied,
                adjusted_levels: report.adjusted_levels,
                mixed_blocks: report.mixed_blocks,
                sensible_enthalpy_before_j_kg: report.sensible_enthalpy_before_j_kg,
                sensible_enthalpy_after_j_kg: report.sensible_enthalpy_after_j_kg,
                relative_enthalpy_residual: report.relative_enthalpy_residual,
                aborted_reason: report.aborted_reason.clone(),
            },
        })
    }
}

/// Versioned, source-bound correction project and optional last-application
/// provenance. The original sounding is deliberately not embedded.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct SoundingCorrectionBundle {
    pub(crate) format: String,
    pub(crate) bowecho_version: String,
    #[serde(default)]
    pub(crate) created_utc: Option<String>,
    pub(crate) source: CorrectionSourceProvenance,
    pub(crate) recipe: CorrectionRecipe,
    #[serde(default)]
    pub(crate) application: Option<CorrectionApplicationProvenance>,
    #[serde(default)]
    pub(crate) notes: Vec<String>,
}

impl SoundingCorrectionBundle {
    pub(crate) fn new(
        source: CorrectionSourceProvenance,
        recipe: CorrectionRecipe,
        created_utc: Option<String>,
    ) -> Self {
        Self {
            format: CORRECTION_BUNDLE_FORMAT.to_owned(),
            bowecho_version: env!("CARGO_PKG_VERSION").to_owned(),
            created_utc: normalized_optional_text(created_utc),
            source,
            recipe,
            application: None,
            notes: Vec::new(),
        }
    }

    pub(crate) fn record_application(
        &mut self,
        result: &CorrectionResult,
    ) -> Result<(), CorrectionIoError> {
        self.application = Some(CorrectionApplicationProvenance::from_result(
            &self.recipe,
            result,
        )?);
        Ok(())
    }

    pub(crate) fn to_json_pretty(&self) -> Result<Vec<u8>, CorrectionIoError> {
        self.validate()?;
        serde_json::to_vec_pretty(self).map_err(Into::into)
    }

    pub(crate) fn from_json(bytes: &[u8]) -> Result<Self, CorrectionIoError> {
        let project: Self = serde_json::from_slice(bytes)?;
        project.validate()?;
        Ok(project)
    }

    pub(crate) fn matches_source(&self, column: &SoundingColumn) -> bool {
        self.source.matches_column(column)
    }

    fn validate(&self) -> Result<(), CorrectionIoError> {
        if self.format != CORRECTION_BUNDLE_FORMAT {
            return Err(CorrectionIoError::UnsupportedFormat(self.format.clone()));
        }
        if self.bowecho_version.trim().is_empty() {
            return Err(CorrectionIoError::InvalidBundle(
                "missing BowEcho version".to_owned(),
            ));
        }
        if self.source.level_count < 2 {
            return Err(CorrectionIoError::InvalidBundle(
                "source level count is below two".to_owned(),
            ));
        }
        if !is_sha256_hex(&self.source.profile_sha256) {
            return Err(CorrectionIoError::InvalidBundle(
                "source profile fingerprint is not a SHA-256 hex digest".to_owned(),
            ));
        }
        if let Some(application) = &self.application
            && (!is_sha256_hex(&application.recipe_sha256)
                || !is_sha256_hex(&application.corrected_profile_sha256))
        {
            return Err(CorrectionIoError::InvalidBundle(
                "application provenance contains an invalid SHA-256 digest".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Parse and validate a project for the exact physical source currently in
/// the editor. Keeping the fingerprint check in the pure IO layer makes it
/// impossible for a caller to accidentally deserialize and apply first.
pub(crate) fn source_bound_bundle_from_json(
    bytes: &[u8],
    current_source: &SoundingColumn,
) -> Result<SoundingCorrectionBundle, CorrectionIoError> {
    let bundle = SoundingCorrectionBundle::from_json(bytes)?;
    if !bundle.matches_source(current_source) {
        return Err(CorrectionIoError::SourceMismatch {
            project_sha256: bundle.source.profile_sha256.clone(),
            current_sha256: sounding_column_sha256(current_source),
        });
    }
    Ok(bundle)
}

/// Export a corrected column as a strict numeric CSV. Profile provenance
/// belongs in the JSON bundle rather than comment lines that break CSV tools.
pub(crate) fn corrected_profile_csv(column: &SoundingColumn) -> Result<String, CorrectionIoError> {
    column
        .validate()
        .map_err(|error| CorrectionIoError::InvalidColumn(error.to_string()))?;
    let surface_height = column.height_m_msl[0];
    let mut output = String::from(
        "pressure_hpa,height_m_msl,height_m_agl,temperature_c,dewpoint_c,u_ms,v_ms,wind_direction_deg,wind_speed_kt,omega_pa_s\n",
    );
    for index in 0..column.len() {
        let (direction_deg, speed_kt) =
            uv_to_direction_speed(column.u_ms[index], column.v_ms[index]);
        let omega = if column.omega_pa_s.is_empty() {
            String::new()
        } else {
            format!("{:.6}", column.omega_pa_s[index])
        };
        writeln!(
            output,
            "{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{}",
            column.pressure_hpa[index],
            column.height_m_msl[index],
            column.height_m_msl[index] - surface_height,
            column.temperature_c[index],
            column.dewpoint_c[index],
            column.u_ms[index],
            column.v_ms[index],
            direction_deg,
            speed_kt,
            omega,
        )
        .expect("writing to String cannot fail");
    }
    Ok(output)
}

/// Parsed subset of the conventional SPC/SHARPpy `%RAW%` interchange.
/// Omega and richer source provenance are not fields in that six-column form.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ImportedRawSounding {
    pub(crate) title: String,
    pub(crate) column: SoundingColumn,
    pub(crate) skipped_missing_rows: usize,
}

/// Export a conventional six-column SPC/SHARPpy `%RAW%` text profile.
pub(crate) fn sharppy_raw_text(
    column: &SoundingColumn,
    title: Option<&str>,
) -> Result<String, CorrectionIoError> {
    column
        .validate()
        .map_err(|error| CorrectionIoError::InvalidColumn(error.to_string()))?;
    let title = title
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(sanitize_single_line)
        .unwrap_or_else(|| default_raw_title(column));
    let mut output = format!(
        "%TITLE%\n{title}\n\n   LEVEL       HGHT       TEMP       DWPT       WDIR       WSPD\n-------------------------------------------------------------------\n%RAW%\n"
    );
    for index in 0..column.len() {
        let (direction_deg, speed_kt) =
            uv_to_direction_speed(column.u_ms[index], column.v_ms[index]);
        writeln!(
            output,
            "{:10.3}, {:10.3}, {:10.3}, {:10.3}, {:10.3}, {:10.3}",
            column.pressure_hpa[index],
            column.height_m_msl[index],
            column.temperature_c[index],
            column.dewpoint_c[index],
            direction_deg,
            speed_kt,
        )
        .expect("writing to String cannot fail");
    }
    output.push_str("%END%\n");
    Ok(output)
}

/// Import the finite six-column subset of SPC/SHARPpy `%RAW%` text.
///
/// Rows containing conventional `-9999`-style missing values are skipped and
/// counted; no interpolation is invented. The remaining column must pass the
/// normal `SoundingColumn` structural and physical validation.
pub(crate) fn parse_sharppy_raw_text(
    input: &str,
) -> Result<ImportedRawSounding, CorrectionIoError> {
    let mut title_mode = false;
    let mut raw_mode = false;
    let mut saw_raw = false;
    let mut saw_end = false;
    let mut title = String::new();
    let mut pressure_hpa = Vec::new();
    let mut height_m_msl = Vec::new();
    let mut temperature_c = Vec::new();
    let mut dewpoint_c = Vec::new();
    let mut u_ms = Vec::new();
    let mut v_ms = Vec::new();
    let mut skipped_missing_rows = 0;

    for (line_index, raw_line) in input.lines().enumerate() {
        let line = raw_line.trim();
        if line.eq_ignore_ascii_case("%TITLE%") {
            title_mode = true;
            continue;
        }
        if line.eq_ignore_ascii_case("%RAW%") {
            raw_mode = true;
            title_mode = false;
            saw_raw = true;
            continue;
        }
        if line.eq_ignore_ascii_case("%END%") {
            saw_end = true;
            break;
        }
        if title_mode && title.is_empty() && !line.is_empty() {
            title = sanitize_single_line(line);
            continue;
        }
        if !raw_mode || line.is_empty() || line.starts_with('#') {
            continue;
        }

        let values = line
            .split(',')
            .map(str::trim)
            .map(str::parse::<f64>)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| {
                CorrectionIoError::InvalidRaw(format!(
                    "line {} is not a six-number comma-separated row: {error}",
                    line_index + 1
                ))
            })?;
        if values.len() != 6 {
            return Err(CorrectionIoError::InvalidRaw(format!(
                "line {} has {} columns; expected LEVEL, HGHT, TEMP, DWPT, WDIR, WSPD",
                line_index + 1,
                values.len()
            )));
        }
        if values
            .iter()
            .any(|value| !value.is_finite() || *value <= RAW_MISSING_THRESHOLD)
        {
            skipped_missing_rows += 1;
            continue;
        }
        let (u, v) = direction_speed_to_uv(values[4], values[5]);
        pressure_hpa.push(values[0]);
        height_m_msl.push(values[1]);
        temperature_c.push(values[2]);
        dewpoint_c.push(values[3]);
        u_ms.push(u);
        v_ms.push(v);
    }

    if !saw_raw {
        return Err(CorrectionIoError::InvalidRaw(
            "missing %RAW% marker".to_owned(),
        ));
    }
    if !saw_end {
        return Err(CorrectionIoError::InvalidRaw(
            "missing %END% marker".to_owned(),
        ));
    }
    let column = SoundingColumn {
        pressure_hpa,
        height_m_msl,
        temperature_c,
        dewpoint_c,
        u_ms,
        v_ms,
        omega_pa_s: Vec::new(),
        metadata: SoundingMetadata {
            station_id: title.clone(),
            ..SoundingMetadata::default()
        },
    };
    column
        .validate()
        .map_err(|error| CorrectionIoError::InvalidRaw(error.to_string()))?;
    Ok(ImportedRawSounding {
        title,
        column,
        skipped_missing_rows,
    })
}

/// Numeric fields that can form a batch dimension for one correction row.
/// Target axes intentionally retain the row's current input coordinate: for
/// example, a dewpoint target cannot silently turn into a mixing-ratio target.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum CorrectionBatchAxisKind {
    LevelHeight,
    ThermalTarget,
    ThermalDepth,
    MoistureTarget,
    MoistureDepth,
    WindDirection,
    WindSpeed,
    WindU,
    WindV,
    WindDepth,
}

impl CorrectionBatchAxisKind {
    pub(crate) const ALL: [Self; 10] = [
        Self::LevelHeight,
        Self::ThermalTarget,
        Self::ThermalDepth,
        Self::MoistureTarget,
        Self::MoistureDepth,
        Self::WindDirection,
        Self::WindSpeed,
        Self::WindU,
        Self::WindV,
        Self::WindDepth,
    ];

    pub(crate) const fn key(self) -> &'static str {
        match self {
            Self::LevelHeight => "level_height_m_agl",
            Self::ThermalTarget => "thermal_target",
            Self::ThermalDepth => "thermal_depth_m",
            Self::MoistureTarget => "moisture_target",
            Self::MoistureDepth => "moisture_depth_m",
            Self::WindDirection => "wind_direction_deg",
            Self::WindSpeed => "wind_speed_kt",
            Self::WindU => "wind_u_ms",
            Self::WindV => "wind_v_ms",
            Self::WindDepth => "wind_depth_m",
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::LevelHeight => "Level height",
            Self::ThermalTarget => "Thermal target",
            Self::ThermalDepth => "Thermal depth",
            Self::MoistureTarget => "Moisture target",
            Self::MoistureDepth => "Moisture depth",
            Self::WindDirection => "Wind direction",
            Self::WindSpeed => "Wind speed",
            Self::WindU => "Wind U",
            Self::WindV => "Wind V",
            Self::WindDepth => "Wind depth",
        }
    }

    pub(crate) const fn unit(self) -> &'static str {
        match self {
            Self::LevelHeight | Self::ThermalDepth | Self::MoistureDepth | Self::WindDepth => "m",
            Self::ThermalTarget | Self::MoistureTarget => "row units",
            Self::WindDirection => "deg",
            Self::WindSpeed => "kt",
            Self::WindU | Self::WindV => "m/s",
        }
    }

    pub(crate) fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.key() == key)
    }
}

fn batch_level(
    recipe: &CorrectionRecipe,
    level_index: usize,
) -> Result<&crate::sounding_correction::CorrectionLevel, CorrectionIoError> {
    recipe.levels.get(level_index).ok_or_else(|| {
        CorrectionIoError::InvalidBatch(format!(
            "correction row {} does not exist",
            level_index + 1
        ))
    })
}

fn batch_level_mut(
    recipe: &mut CorrectionRecipe,
    level_index: usize,
) -> Result<&mut crate::sounding_correction::CorrectionLevel, CorrectionIoError> {
    recipe.levels.get_mut(level_index).ok_or_else(|| {
        CorrectionIoError::InvalidBatch(format!(
            "correction row {} does not exist",
            level_index + 1
        ))
    })
}

fn unavailable_axis(level_index: usize, kind: CorrectionBatchAxisKind) -> CorrectionIoError {
    CorrectionIoError::InvalidBatch(format!(
        "{} is not available for correction row {} in its current mode",
        kind.label(),
        level_index + 1
    ))
}

/// Return the current scalar represented by a typed axis, or an explicit
/// error when that variable/mode is not enabled on the selected row.
pub(crate) fn correction_batch_axis_value(
    recipe: &CorrectionRecipe,
    level_index: usize,
    kind: CorrectionBatchAxisKind,
) -> Result<f64, CorrectionIoError> {
    let level = batch_level(recipe, level_index)?;
    let value = match kind {
        CorrectionBatchAxisKind::LevelHeight => level.target_agl_m,
        CorrectionBatchAxisKind::ThermalTarget => {
            match level.thermal.as_ref().map(|edit| edit.target) {
                Some(
                    ThermalTarget::TemperatureC(value)
                    | ThermalTarget::PotentialTemperatureK(value),
                ) => value,
                None => return Err(unavailable_axis(level_index, kind)),
            }
        }
        CorrectionBatchAxisKind::ThermalDepth => level
            .thermal
            .as_ref()
            .map(|edit| edit.blend.depth_m)
            .ok_or_else(|| unavailable_axis(level_index, kind))?,
        CorrectionBatchAxisKind::MoistureTarget => {
            match level.moisture.as_ref().map(|edit| edit.target) {
                Some(
                    MoistureTarget::DewpointC(value)
                    | MoistureTarget::MixingRatioGKg(value)
                    | MoistureTarget::SpecificHumidityGKg(value),
                ) => value,
                None => return Err(unavailable_axis(level_index, kind)),
            }
        }
        CorrectionBatchAxisKind::MoistureDepth => level
            .moisture
            .as_ref()
            .map(|edit| edit.blend.depth_m)
            .ok_or_else(|| unavailable_axis(level_index, kind))?,
        CorrectionBatchAxisKind::WindDirection => match level.wind.as_ref().map(|edit| edit.target)
        {
            Some(WindTarget::DirectionSpeed { direction_deg, .. }) => direction_deg,
            _ => return Err(unavailable_axis(level_index, kind)),
        },
        CorrectionBatchAxisKind::WindSpeed => match level.wind.as_ref().map(|edit| edit.target) {
            Some(WindTarget::DirectionSpeed { speed_kt, .. }) => speed_kt,
            _ => return Err(unavailable_axis(level_index, kind)),
        },
        CorrectionBatchAxisKind::WindU => match level.wind.as_ref().map(|edit| edit.target) {
            Some(WindTarget::UV { u_ms, .. }) => u_ms,
            _ => return Err(unavailable_axis(level_index, kind)),
        },
        CorrectionBatchAxisKind::WindV => match level.wind.as_ref().map(|edit| edit.target) {
            Some(WindTarget::UV { v_ms, .. }) => v_ms,
            _ => return Err(unavailable_axis(level_index, kind)),
        },
        CorrectionBatchAxisKind::WindDepth => level
            .wind
            .as_ref()
            .map(|edit| edit.blend.depth_m)
            .ok_or_else(|| unavailable_axis(level_index, kind))?,
    };
    if !value.is_finite() {
        return Err(CorrectionIoError::InvalidBatch(format!(
            "{} on correction row {} is not finite",
            kind.label(),
            level_index + 1
        )));
    }
    Ok(value)
}

/// Apply one scalar batch selection without changing the target coordinate or
/// enabling a variable that is absent from the selected correction row.
pub(crate) fn apply_correction_batch_axis(
    recipe: &mut CorrectionRecipe,
    level_index: usize,
    kind: CorrectionBatchAxisKind,
    value: f64,
) -> Result<(), CorrectionIoError> {
    if !value.is_finite() {
        return Err(CorrectionIoError::InvalidBatch(format!(
            "{} selection is not finite",
            kind.label()
        )));
    }
    let level = batch_level_mut(recipe, level_index)?;
    match kind {
        CorrectionBatchAxisKind::LevelHeight => level.target_agl_m = value,
        CorrectionBatchAxisKind::ThermalTarget => {
            match level.thermal.as_mut().map(|edit| &mut edit.target) {
                Some(
                    ThermalTarget::TemperatureC(target)
                    | ThermalTarget::PotentialTemperatureK(target),
                ) => *target = value,
                None => return Err(unavailable_axis(level_index, kind)),
            }
        }
        CorrectionBatchAxisKind::ThermalDepth => match level.thermal.as_mut() {
            Some(edit) => edit.blend.depth_m = value,
            None => return Err(unavailable_axis(level_index, kind)),
        },
        CorrectionBatchAxisKind::MoistureTarget => {
            match level.moisture.as_mut().map(|edit| &mut edit.target) {
                Some(
                    MoistureTarget::DewpointC(target)
                    | MoistureTarget::MixingRatioGKg(target)
                    | MoistureTarget::SpecificHumidityGKg(target),
                ) => *target = value,
                None => return Err(unavailable_axis(level_index, kind)),
            }
        }
        CorrectionBatchAxisKind::MoistureDepth => match level.moisture.as_mut() {
            Some(edit) => edit.blend.depth_m = value,
            None => return Err(unavailable_axis(level_index, kind)),
        },
        CorrectionBatchAxisKind::WindDirection => {
            match level.wind.as_mut().map(|edit| &mut edit.target) {
                Some(WindTarget::DirectionSpeed { direction_deg, .. }) => *direction_deg = value,
                _ => return Err(unavailable_axis(level_index, kind)),
            }
        }
        CorrectionBatchAxisKind::WindSpeed => {
            match level.wind.as_mut().map(|edit| &mut edit.target) {
                Some(WindTarget::DirectionSpeed { speed_kt, .. }) => *speed_kt = value,
                _ => return Err(unavailable_axis(level_index, kind)),
            }
        }
        CorrectionBatchAxisKind::WindU => match level.wind.as_mut().map(|edit| &mut edit.target) {
            Some(WindTarget::UV { u_ms, .. }) => *u_ms = value,
            _ => return Err(unavailable_axis(level_index, kind)),
        },
        CorrectionBatchAxisKind::WindV => match level.wind.as_mut().map(|edit| &mut edit.target) {
            Some(WindTarget::UV { v_ms, .. }) => *v_ms = value,
            _ => return Err(unavailable_axis(level_index, kind)),
        },
        CorrectionBatchAxisKind::WindDepth => match level.wind.as_mut() {
            Some(edit) => edit.blend.depth_m = value,
            None => return Err(unavailable_axis(level_index, kind)),
        },
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub(crate) enum BatchValue {
    Number(f64),
    Text(String),
    Bool(bool),
}

impl BatchValue {
    fn validate(&self, axis_key: &str) -> Result<(), CorrectionIoError> {
        if let Self::Number(value) = self
            && !value.is_finite()
        {
            return Err(CorrectionIoError::InvalidBatch(format!(
                "axis `{axis_key}` contains a non-finite number"
            )));
        }
        Ok(())
    }
}

/// One independent batch dimension. `key` is intentionally generic: the host
/// maps it to a recipe field and owns that type-checked application step.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct BatchAxis {
    pub(crate) key: String,
    pub(crate) label: String,
    #[serde(default)]
    pub(crate) unit: Option<String>,
    pub(crate) values: Vec<BatchValue>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct BatchSelection {
    pub(crate) axis_key: String,
    pub(crate) value: BatchValue,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct BatchMember {
    /// Stable zero-based Cartesian ordinal. The last axis varies fastest.
    pub(crate) ordinal: usize,
    pub(crate) selections: Vec<BatchSelection>,
}

impl BatchMember {
    pub(crate) fn display_id(&self) -> String {
        format!("member-{:04}", self.ordinal + 1)
    }
}

/// Deterministically enumerate the Cartesian product of batch axes.
///
/// Zero axes produces one baseline member. The caller-provided limit is
/// checked before allocation, preventing an accidental combinatorial run.
pub(crate) fn cartesian_batch_members(
    axes: &[BatchAxis],
    max_members: usize,
) -> Result<Vec<BatchMember>, CorrectionIoError> {
    if max_members == 0 {
        return Err(CorrectionIoError::InvalidBatch(
            "member limit must be positive".to_owned(),
        ));
    }
    let mut keys = BTreeSet::new();
    let mut total = 1usize;
    for axis in axes {
        let key = axis.key.trim();
        if key.is_empty() {
            return Err(CorrectionIoError::InvalidBatch(
                "axis key cannot be blank".to_owned(),
            ));
        }
        if !keys.insert(key.to_owned()) {
            return Err(CorrectionIoError::InvalidBatch(format!(
                "duplicate axis key `{key}`"
            )));
        }
        if axis.values.is_empty() {
            return Err(CorrectionIoError::InvalidBatch(format!(
                "axis `{key}` has no values"
            )));
        }
        for value in &axis.values {
            value.validate(key)?;
        }
        total = total.checked_mul(axis.values.len()).ok_or_else(|| {
            CorrectionIoError::InvalidBatch("Cartesian member count overflowed usize".to_owned())
        })?;
        if total > max_members {
            return Err(CorrectionIoError::InvalidBatch(format!(
                "Cartesian plan needs {total} members, above the configured {max_members} limit"
            )));
        }
    }

    let mut members = Vec::with_capacity(total);
    for ordinal in 0..total {
        let mut remainder = ordinal;
        let mut indices = vec![0usize; axes.len()];
        for axis_index in (0..axes.len()).rev() {
            let count = axes[axis_index].values.len();
            indices[axis_index] = remainder % count;
            remainder /= count;
        }
        let selections = axes
            .iter()
            .zip(indices)
            .map(|(axis, value_index)| BatchSelection {
                axis_key: axis.key.trim().to_owned(),
                value: axis.values[value_index].clone(),
            })
            .collect();
        members.push(BatchMember {
            ordinal,
            selections,
        });
    }
    Ok(members)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BatchStatistic {
    Minimum,
    Median,
    Maximum,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct MinMedianMax {
    pub(crate) minimum: f64,
    pub(crate) median: f64,
    pub(crate) maximum: f64,
    pub(crate) finite_count: usize,
    pub(crate) ignored_non_finite: usize,
}

impl MinMedianMax {
    pub(crate) fn value(self, statistic: BatchStatistic) -> f64 {
        match statistic {
            BatchStatistic::Minimum => self.minimum,
            BatchStatistic::Median => self.median,
            BatchStatistic::Maximum => self.maximum,
        }
    }
}

/// Summarize finite members without allowing NaN to poison ordering. The
/// ignored count remains explicit so a batch UI can flag incomplete members.
pub(crate) fn finite_min_median_max(values: &[f64]) -> Option<MinMedianMax> {
    let mut finite = values
        .iter()
        .copied()
        .filter(|value| value.is_finite())
        .collect::<Vec<_>>();
    if finite.is_empty() {
        return None;
    }
    finite.sort_by(f64::total_cmp);
    let middle = finite.len() / 2;
    let median = if finite.len() % 2 == 0 {
        let left = finite[middle - 1];
        let right = finite[middle];
        left + (right - left) * 0.5
    } else {
        finite[middle]
    };
    Some(MinMedianMax {
        minimum: finite[0],
        median,
        maximum: *finite.last().expect("finite input is non-empty"),
        finite_count: finite.len(),
        ignored_non_finite: values.len() - finite.len(),
    })
}

/// Pointwise min/median/max for equal-length batch output vectors. A position
/// containing no finite members is represented as `None`, not an invented zero.
pub(crate) fn pointwise_min_median_max(
    series: &[Vec<f64>],
) -> Result<Vec<Option<MinMedianMax>>, CorrectionIoError> {
    let Some(first) = series.first() else {
        return Ok(Vec::new());
    };
    let width = first.len();
    for (index, values) in series.iter().enumerate() {
        if values.len() != width {
            return Err(CorrectionIoError::InvalidBatch(format!(
                "series {index} has {} values; expected {width}",
                values.len()
            )));
        }
    }
    Ok((0..width)
        .map(|index| {
            let values = series.iter().map(|row| row[index]).collect::<Vec<_>>();
            finite_min_median_max(&values)
        })
        .collect())
}

fn normalized_optional_text(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_owned())
    })
}

fn finite_option(value: Option<f64>) -> Option<f64> {
    value.filter(|value| value.is_finite())
}

fn sanitize_single_line(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_owned()
}

fn default_raw_title(column: &SoundingColumn) -> String {
    let station = column.metadata.station_id.trim();
    let valid = column.metadata.valid_time.trim();
    match (station.is_empty(), valid.is_empty()) {
        (false, false) => format!("{station} {valid}"),
        (false, true) => station.to_owned(),
        (true, false) => valid.to_owned(),
        (true, true) => "BowEcho corrected sounding".to_owned(),
    }
}

fn direction_speed_to_uv(direction_deg: f64, speed_kt: f64) -> (f64, f64) {
    let direction = direction_deg.rem_euclid(360.0).to_radians();
    let speed_ms = speed_kt.max(0.0) / MS_TO_KT;
    (-speed_ms * direction.sin(), -speed_ms * direction.cos())
}

fn uv_to_direction_speed(u_ms: f64, v_ms: f64) -> (f64, f64) {
    let speed_ms = u_ms.hypot(v_ms);
    let direction = if speed_ms <= 1.0e-12 {
        0.0
    } else {
        (-u_ms).atan2(-v_ms).to_degrees().rem_euclid(360.0)
    };
    (direction, speed_ms * MS_TO_KT)
}

fn qc_severity_key(severity: QcSeverity) -> &'static str {
    match severity {
        QcSeverity::Advisory => "advisory",
        QcSeverity::Warning => "warning",
        QcSeverity::Error => "error",
    }
}

fn qc_kind_key(kind: QcIssueKind) -> &'static str {
    match kind {
        QcIssueKind::Structural => "structural",
        QcIssueKind::InvalidTarget => "invalid_target",
        QcIssueKind::InvalidMoisture => "invalid_moisture",
        QcIssueKind::Supersaturation => "supersaturation",
        QcIssueKind::DryStaticInstability => "dry_static_instability",
        QcIssueKind::WindShearKink => "wind_shear_kink",
        QcIssueKind::ConvectiveAdjustmentAborted => "convective_adjustment_aborted",
    }
}

fn recipe_sha256(recipe: &CorrectionRecipe) -> Result<String, CorrectionIoError> {
    let bytes = serde_json::to_vec(recipe)?;
    Ok(sha256_hex(&bytes))
}

fn sounding_column_sha256(column: &SoundingColumn) -> String {
    let mut hash = Sha256::new();
    hash.update(b"bowecho-sounding-column-fingerprint-v1\0");
    update_f64_slice(&mut hash, b"pressure_hpa", &column.pressure_hpa);
    update_f64_slice(&mut hash, b"height_m_msl", &column.height_m_msl);
    update_f64_slice(&mut hash, b"temperature_c", &column.temperature_c);
    update_f64_slice(&mut hash, b"dewpoint_c", &column.dewpoint_c);
    update_f64_slice(&mut hash, b"u_ms", &column.u_ms);
    update_f64_slice(&mut hash, b"v_ms", &column.v_ms);
    update_f64_slice(&mut hash, b"omega_pa_s", &column.omega_pa_s);
    update_text(&mut hash, b"station_id", &column.metadata.station_id);
    update_text(&mut hash, b"valid_time", &column.metadata.valid_time);
    update_optional_f64(&mut hash, b"latitude_deg", column.metadata.latitude_deg);
    update_optional_f64(&mut hash, b"longitude_deg", column.metadata.longitude_deg);
    update_optional_f64(&mut hash, b"elevation_m", column.metadata.elevation_m);
    update_optional_text(
        &mut hash,
        b"sample_method",
        column.metadata.sample_method.as_deref(),
    );
    update_optional_f64(
        &mut hash,
        b"box_radius_lat_deg",
        column.metadata.box_radius_lat_deg,
    );
    update_optional_f64(
        &mut hash,
        b"box_radius_lon_deg",
        column.metadata.box_radius_lon_deg,
    );
    format!("{:x}", hash.finalize())
}

fn update_f64_slice(hash: &mut Sha256, label: &[u8], values: &[f64]) {
    hash.update(label);
    hash.update((values.len() as u64).to_le_bytes());
    for value in values {
        hash.update(value.to_bits().to_le_bytes());
    }
}

fn update_text(hash: &mut Sha256, label: &[u8], value: &str) {
    hash.update(label);
    hash.update((value.len() as u64).to_le_bytes());
    hash.update(value.as_bytes());
}

fn update_optional_text(hash: &mut Sha256, label: &[u8], value: Option<&str>) {
    hash.update(label);
    match value {
        Some(value) => {
            hash.update([1]);
            hash.update((value.len() as u64).to_le_bytes());
            hash.update(value.as_bytes());
        }
        None => hash.update([0]),
    }
}

fn update_optional_f64(hash: &mut Sha256, label: &[u8], value: Option<f64>) {
    hash.update(label);
    match value {
        Some(value) => {
            hash.update([1]);
            hash.update(value.to_bits().to_le_bytes());
        }
        None => hash.update([0]),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sounding_correction::{
        CorrectionLevel, MoistureEdit, MoistureTarget, ThermalEdit, ThermalTarget, WindEdit,
        WindTarget,
    };

    fn test_column() -> SoundingColumn {
        SoundingColumn {
            pressure_hpa: vec![1_000.0, 900.0, 800.0, 700.0],
            height_m_msl: vec![100.0, 1_000.0, 2_000.0, 3_100.0],
            temperature_c: vec![24.0, 17.0, 10.0, 2.0],
            dewpoint_c: vec![18.0, 11.0, 4.0, -5.0],
            u_ms: vec![0.0, -5.0, -10.0, -15.0],
            v_ms: vec![-5.0, -7.0, -9.0, -12.0],
            omega_pa_s: vec![0.0, -0.1, -0.2, -0.1],
            metadata: SoundingMetadata {
                station_id: "TEST".to_owned(),
                valid_time: "2026-07-15T20:00:00Z".to_owned(),
                latitude_deg: Some(35.0),
                longitude_deg: Some(-97.0),
                elevation_m: Some(100.0),
                sample_method: Some("point".to_owned()),
                box_radius_lat_deg: None,
                box_radius_lon_deg: None,
            },
        }
    }

    fn test_recipe() -> CorrectionRecipe {
        let mut recipe = CorrectionRecipe::default();
        let mut level = CorrectionLevel::at_height(0.0);
        level.thermal = Some(ThermalEdit::new(ThermalTarget::PotentialTemperatureK(
            302.0,
        )));
        recipe.levels.push(level);
        recipe
    }

    #[test]
    fn project_json_round_trip_stays_source_bound() {
        let column = test_column();
        let source = CorrectionSourceProvenance::from_column(
            &column,
            CorrectionSourceContext {
                source_kind: Some("model".to_owned()),
                model: Some("HRRR".to_owned()),
                run: Some("20260715_18z".to_owned()),
                lead: Some("f002".to_owned()),
                ..CorrectionSourceContext::default()
            },
        )
        .expect("source provenance");
        let project = SoundingCorrectionBundle::new(
            source,
            test_recipe(),
            Some("2026-07-15T20:30:00Z".to_owned()),
        );
        let bytes = project.to_json_pretty().expect("serialize project");
        let restored = SoundingCorrectionBundle::from_json(&bytes).expect("restore project");
        assert_eq!(restored, project);
        assert!(restored.matches_source(&column));

        let mut changed = column;
        changed.temperature_c[0] += 0.01;
        assert!(!restored.matches_source(&changed));
    }

    #[test]
    fn source_bound_load_rejects_a_different_physical_column() {
        let column = test_column();
        let source =
            CorrectionSourceProvenance::from_column(&column, CorrectionSourceContext::default())
                .expect("source provenance");
        let project = SoundingCorrectionBundle::new(source, test_recipe(), None);
        let bytes = project.to_json_pretty().expect("serialize project");

        let loaded = source_bound_bundle_from_json(&bytes, &column).expect("matching source");
        assert_eq!(loaded.recipe, test_recipe());

        let mut different = column;
        different.u_ms[2] += 0.001;
        let error = source_bound_bundle_from_json(&bytes, &different)
            .expect_err("a different source fingerprint must be rejected");
        assert!(matches!(error, CorrectionIoError::SourceMismatch { .. }));
        assert!(error.to_string().contains("different source profile"));
    }

    #[test]
    fn corrected_csv_is_numeric_and_keeps_optional_omega() {
        let csv = corrected_profile_csv(&test_column()).expect("CSV export");
        let lines = csv.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 5);
        assert!(lines[0].contains("height_m_agl"));
        assert!(lines[0].contains("wind_direction_deg"));
        assert!(lines[1].ends_with(",0.000000"));
        assert!(lines[4].ends_with(",-0.100000"));
    }

    #[test]
    fn sharppy_raw_round_trip_preserves_the_six_conventional_fields() {
        let source = test_column();
        let text = sharppy_raw_text(&source, Some("TEST PROFILE")).expect("RAW export");
        assert!(text.contains("%RAW%"));
        assert!(text.ends_with("%END%\n"));
        let imported = parse_sharppy_raw_text(&text).expect("RAW import");
        assert_eq!(imported.title, "TEST PROFILE");
        assert_eq!(imported.skipped_missing_rows, 0);
        assert_eq!(imported.column.pressure_hpa, source.pressure_hpa);
        assert_eq!(imported.column.height_m_msl, source.height_m_msl);
        for (actual, expected) in imported.column.u_ms.iter().zip(&source.u_ms) {
            assert!((actual - expected).abs() < 5.0e-4);
        }
        for (actual, expected) in imported.column.v_ms.iter().zip(&source.v_ms) {
            assert!((actual - expected).abs() < 5.0e-4);
        }
        assert!(imported.column.omega_pa_s.is_empty());
    }

    #[test]
    fn raw_import_skips_missing_rows_without_interpolation() {
        let input = "%TITLE%\nfixture\n%RAW%\n1000, 0, 20, 10, 180, 10\n900, 1000, -9999, 3, 190, 20\n800, 2000, 5, -1, 200, 30\n%END%\n";
        let imported = parse_sharppy_raw_text(input).expect("parse finite subset");
        assert_eq!(imported.skipped_missing_rows, 1);
        assert_eq!(imported.column.pressure_hpa, vec![1_000.0, 800.0]);
    }

    #[test]
    fn cartesian_members_are_deterministic_and_last_axis_varies_fastest() {
        let axes = vec![
            BatchAxis {
                key: "thermal_target".to_owned(),
                label: "Theta target".to_owned(),
                unit: Some("K".to_owned()),
                values: vec![BatchValue::Number(300.0), BatchValue::Number(302.0)],
            },
            BatchAxis {
                key: "blend_shape".to_owned(),
                label: "Shape".to_owned(),
                unit: None,
                values: vec![
                    BatchValue::Text("linear".to_owned()),
                    BatchValue::Text("top_cosine".to_owned()),
                ],
            },
        ];
        let members = cartesian_batch_members(&axes, 16).expect("Cartesian product");
        assert_eq!(members.len(), 4);
        assert_eq!(members[0].display_id(), "member-0001");
        assert_eq!(
            members[0].selections[1].value,
            BatchValue::Text("linear".to_owned())
        );
        assert_eq!(
            members[1].selections[1].value,
            BatchValue::Text("top_cosine".to_owned())
        );
        assert_eq!(members[2].selections[0].value, BatchValue::Number(302.0));
    }

    #[test]
    fn typed_batch_axes_update_only_the_selected_row_and_coordinate() {
        let mut recipe = CorrectionRecipe::default();
        recipe.levels.push(CorrectionLevel::at_height(100.0));
        let mut selected = CorrectionLevel::at_height(800.0);
        selected.thermal = Some(ThermalEdit::new(ThermalTarget::TemperatureC(14.0)));
        selected.moisture = Some(MoistureEdit::new(MoistureTarget::MixingRatioGKg(8.0)));
        selected.wind = Some(WindEdit::new(WindTarget::DirectionSpeed {
            direction_deg: 210.0,
            speed_kt: 35.0,
        }));
        recipe.levels.push(selected);

        apply_correction_batch_axis(&mut recipe, 1, CorrectionBatchAxisKind::LevelHeight, 950.0)
            .unwrap();
        apply_correction_batch_axis(&mut recipe, 1, CorrectionBatchAxisKind::ThermalTarget, 16.5)
            .unwrap();
        apply_correction_batch_axis(
            &mut recipe,
            1,
            CorrectionBatchAxisKind::MoistureDepth,
            1_250.0,
        )
        .unwrap();
        apply_correction_batch_axis(
            &mut recipe,
            1,
            CorrectionBatchAxisKind::WindDirection,
            235.0,
        )
        .unwrap();
        apply_correction_batch_axis(&mut recipe, 1, CorrectionBatchAxisKind::WindSpeed, 42.0)
            .unwrap();

        assert_eq!(recipe.levels[0].target_agl_m, 100.0);
        assert_eq!(recipe.levels[1].target_agl_m, 950.0);
        assert_eq!(
            recipe.levels[1].thermal.as_ref().unwrap().target,
            ThermalTarget::TemperatureC(16.5)
        );
        assert_eq!(
            recipe.levels[1].moisture.as_ref().unwrap().blend.depth_m,
            1_250.0
        );
        assert_eq!(
            recipe.levels[1].wind.as_ref().unwrap().target,
            WindTarget::DirectionSpeed {
                direction_deg: 235.0,
                speed_kt: 42.0,
            }
        );

        let error =
            apply_correction_batch_axis(&mut recipe, 1, CorrectionBatchAxisKind::WindU, 10.0)
                .expect_err("U is unavailable in direction/speed mode");
        assert!(error.to_string().contains("not available"));

        recipe.levels[1].wind.as_mut().unwrap().target = WindTarget::UV {
            u_ms: -10.0,
            v_ms: 5.0,
        };
        apply_correction_batch_axis(&mut recipe, 1, CorrectionBatchAxisKind::WindV, 7.5).unwrap();
        assert_eq!(
            correction_batch_axis_value(&recipe, 1, CorrectionBatchAxisKind::WindV).unwrap(),
            7.5
        );
    }

    #[test]
    fn cartesian_limit_is_checked_before_allocation() {
        let axes = vec![BatchAxis {
            key: "x".to_owned(),
            label: "X".to_owned(),
            unit: None,
            values: (0..100)
                .map(|value| BatchValue::Number(f64::from(value)))
                .collect(),
        }];
        let error = cartesian_batch_members(&axes, 99).expect_err("limit must fail");
        assert!(error.to_string().contains("above the configured 99 limit"));
    }

    #[test]
    fn finite_statistics_report_ignored_members_and_even_median() {
        let summary =
            finite_min_median_max(&[4.0, f64::NAN, 1.0, 3.0, 2.0]).expect("finite summary");
        assert_eq!(summary.minimum, 1.0);
        assert_eq!(summary.median, 2.5);
        assert_eq!(summary.maximum, 4.0);
        assert_eq!(summary.finite_count, 4);
        assert_eq!(summary.ignored_non_finite, 1);
    }

    #[test]
    fn pointwise_statistics_preserve_all_missing_positions() {
        let values =
            pointwise_min_median_max(&[vec![1.0, f64::NAN, 9.0], vec![3.0, f64::NAN, 5.0]])
                .expect("pointwise summary");
        assert_eq!(values[0].expect("first").median, 2.0);
        assert!(values[1].is_none());
        assert_eq!(values[2].expect("third").minimum, 5.0);
    }
}
