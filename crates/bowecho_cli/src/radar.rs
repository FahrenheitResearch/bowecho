//! Deterministic, headless radar inspection, rendering, and verification.
//!
//! This module deliberately uses the same production decoder and polar
//! renderer as the desktop application.  Its PNGs are transparent radar
//! overlays (not screenshots or map composites), so model-verification jobs
//! can geolocate them from the site/range receipts without initializing egui.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use chrono::{DateTime, Duration, Timelike, Utc};
use image::ImageFormat;
use nexrad_io::{
    SupportedVolumeFormat, decode_supported_volume_bytes, sniff_supported_volume_format,
};
use radar_core::{MomentGrid, MomentStorage, MomentType, RadarVolume, RadialStatus};
use render2d::{
    ColorTableFamily, ColorTableSet, V4VolumeSolution, ViewportMomentCache, ViewportRasterOptions,
    composite_reflectivity_grid, dealias_skipped_no_nyquist, dealias_volume_v4,
    viewport_rgba_buffer_len,
};
use serde::{Deserialize, Serialize};

use crate::artifact::{
    ArtifactFileReceipt, ArtifactStatus, BuildIdentity, DerivationMethod, FailureRecord,
    FiniteStatistics,
};
use crate::fs::{
    publish_file_atomic, resolve_receipt_path, sha256_file, sha256_reader, write_json_atomic,
};
use crate::paths::slug;
use crate::{
    CliError, ExitCode, RadarCommand, RadarCutSelection, RadarRenderOptions, RuntimeContext,
};

pub const INSPECT_SCHEMA_VERSION: &str = "bowecho.radar.inspect.v1";
pub const ARTIFACT_SCHEMA_VERSION: &str = "bowecho.radar.artifacts.v1";
pub const VERIFY_SCHEMA_VERSION: &str = "bowecho.radar.verify.v1";
pub const ARTIFACT_MANIFEST_NAME: &str = "radar-artifact-manifest.json";

const MAX_DISCOVERED_INPUTS: usize = 100_000;
const MAX_DISCOVERY_DEPTH: usize = 256;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_PRODUCTS: &[&str] = &["REF", "VEL", "SW", "ZDR", "RHO", "PHI", "KDP"];

/// Serializable mirror of the parser-owned cut selector used in receipts.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RadarCutRequest {
    Lowest,
    All,
    Explicit(Vec<usize>),
}

impl From<&RadarCutSelection> for RadarCutRequest {
    fn from(value: &RadarCutSelection) -> Self {
        match value {
            RadarCutSelection::Lowest => Self::Lowest,
            RadarCutSelection::All => Self::All,
            RadarCutSelection::Explicit(indices) => Self::Explicit(indices.clone()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectStatus {
    Complete,
    Partial,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RadarInspectReport {
    pub schema_version: String,
    pub status: InspectStatus,
    pub bowecho: BuildIdentity,
    #[serde(default)]
    pub files: Vec<RadarInspectFile>,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub failures: Vec<FailureRecord>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RadarInspectFile {
    pub path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
    pub format: String,
    pub readable: bool,
    pub complete: bool,
    pub completeness_evidence: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site: Option<RadarSiteReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vcp: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<RadarMetadataReceipt>,
    #[serde(default)]
    pub cuts: Vec<RadarCutReceipt>,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub failures: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RadarSiteReceipt {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latitude_deg: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub longitude_deg: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elevation_m: Option<f32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RadarMetadataReceipt {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression: Option<String>,
    pub message_count: usize,
    pub decoded_radial_count: usize,
    pub skipped_message_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radar_frequency_mhz: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub beam_width_h_deg: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub beam_width_v_deg: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pulse_width_us: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RadarCutReceipt {
    pub index: usize,
    pub elevation_deg: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elevation_number: Option<u8>,
    pub radial_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_status: Option<String>,
    #[serde(default)]
    pub moments: Vec<RadarMomentReceipt>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RadarMomentReceipt {
    pub id: String,
    pub storage: String,
    pub radial_count: usize,
    pub gate_count: usize,
    pub first_gate_m: i32,
    pub gate_spacing_m: i32,
    pub maximum_range_km: f32,
    pub scale: f32,
    pub offset: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nodata: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range_folded: Option<u16>,
    pub statistics: FiniteStatistics,
}

/// Inspect one file or a deterministic recursive directory snapshot.
pub fn inspect(
    input: &Path,
    context: &RuntimeContext,
) -> Result<(RadarInspectReport, ExitCode), CliError> {
    let files = expand_radar_inputs(&[input.to_path_buf()])?;
    let mut report = RadarInspectReport {
        schema_version: INSPECT_SCHEMA_VERSION.into(),
        status: InspectStatus::Failed,
        bowecho: build_identity(context),
        files: Vec::with_capacity(files.len()),
        warnings: Vec::new(),
        failures: Vec::new(),
    };

    for path in files {
        let hash = sha256_file(&path).map_err(|error| {
            CliError::input(format!(
                "cannot hash radar input {}: {error}",
                path.display()
            ))
        })?;
        let raw = fs::read(&path).map_err(|error| {
            CliError::input(format!(
                "cannot read radar input {}: {error}",
                path.display()
            ))
        })?;
        let format = format_name(sniff_format(&raw)).to_owned();
        match decode_supported_volume_bytes(&raw) {
            Ok(mut volume) => {
                volume.metadata.source_path = Some(path.display().to_string());
                let (complete, evidence) = volume_completeness(&volume, &format);
                let mut warnings = Vec::new();
                if !complete {
                    warnings.push(
                        "source has no end-of-volume marker; it may be a partial live scan".into(),
                    );
                }
                report.files.push(inspect_volume_file(
                    path,
                    hash.bytes,
                    hash.sha256,
                    format,
                    volume,
                    complete,
                    evidence,
                    warnings,
                ));
            }
            Err(reason) => {
                report.failures.push(FailureRecord {
                    stage: "decode".into(),
                    path: Some(path.clone()),
                    reason: reason.clone(),
                });
                report.files.push(RadarInspectFile {
                    path,
                    bytes: hash.bytes,
                    sha256: hash.sha256,
                    format,
                    readable: false,
                    complete: false,
                    completeness_evidence: "decode failed".into(),
                    site: None,
                    volume_time: None,
                    vcp: None,
                    metadata: None,
                    cuts: Vec::new(),
                    warnings: Vec::new(),
                    failures: vec![reason],
                });
            }
        }
    }

    let readable = report.files.iter().filter(|file| file.readable).count();
    report.status =
        if readable == report.files.len() && report.files.iter().all(|file| file.complete) {
            InspectStatus::Complete
        } else if readable > 0 {
            InspectStatus::Partial
        } else {
            InspectStatus::Failed
        };
    let code = if report.status == InspectStatus::Complete {
        ExitCode::Success
    } else {
        ExitCode::Data
    };
    Ok((report, code))
}

#[allow(clippy::too_many_arguments)]
fn inspect_volume_file(
    path: PathBuf,
    bytes: u64,
    sha256: String,
    format: String,
    volume: RadarVolume,
    complete: bool,
    evidence: String,
    warnings: Vec<String>,
) -> RadarInspectFile {
    let cuts = volume
        .cuts
        .iter()
        .enumerate()
        .map(|(index, cut)| {
            let start_time = cut
                .radials
                .iter()
                .map(|radial| radial_time(&volume, radial.time_offset_ms))
                .min()
                .map(format_time);
            let end_time = cut
                .radials
                .iter()
                .map(|radial| radial_time(&volume, radial.time_offset_ms))
                .max()
                .map(format_time);
            RadarCutReceipt {
                index,
                elevation_deg: cut.elevation_deg,
                elevation_number: cut.elevation_number,
                radial_count: cut.radials.len(),
                start_time,
                end_time,
                terminal_status: cut
                    .radials
                    .last()
                    .and_then(|radial| radial.radial_status)
                    .map(|status| format!("{status:?}")),
                moments: cut.moments.values().map(moment_receipt).collect(),
            }
        })
        .collect();
    let metadata = &volume.metadata;
    RadarInspectFile {
        path,
        bytes,
        sha256,
        format,
        readable: true,
        complete,
        completeness_evidence: evidence,
        site: Some(site_receipt(&volume)),
        volume_time: Some(format_time(volume.volume_time)),
        vcp: volume.vcp.as_ref().map(|vcp| vcp.pattern),
        metadata: Some(RadarMetadataReceipt {
            archive_version: metadata.archive_version.clone(),
            compression: metadata.compression.clone(),
            message_count: metadata.message_count,
            decoded_radial_count: metadata.decoded_radial_count,
            skipped_message_count: metadata.skipped_message_count,
            scan_mode: metadata.scan_mode.map(|mode| format!("{mode:?}")),
            radar_frequency_mhz: metadata.radar_frequency_mhz,
            beam_width_h_deg: metadata.beam_width_h_deg,
            beam_width_v_deg: metadata.beam_width_v_deg,
            pulse_width_us: metadata.pulse_width_us,
            scan_name: metadata.scan_name.clone(),
            scan_id: metadata.scan_id.clone(),
        }),
        cuts,
        warnings,
        failures: Vec::new(),
    }
}

fn moment_receipt(grid: &MomentGrid) -> RadarMomentReceipt {
    RadarMomentReceipt {
        id: grid.moment.short_name().to_owned(),
        storage: match &grid.storage {
            MomentStorage::U8(_) => "u8",
            MomentStorage::U16(_) => "u16",
            MomentStorage::F32(_) => "f32",
        }
        .into(),
        radial_count: grid.radial_count(),
        gate_count: grid.gate_range.gate_count,
        first_gate_m: grid.gate_range.first_gate_m,
        gate_spacing_m: grid.gate_range.gate_spacing_m,
        maximum_range_km: grid_max_range_km(grid),
        scale: grid.scale,
        offset: grid.offset,
        nodata: grid.nodata,
        range_folded: grid.range_folded,
        statistics: grid_statistics(grid),
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RadarArtifactManifest {
    pub schema_version: String,
    pub status: ArtifactStatus,
    pub bowecho: BuildIdentity,
    /// SHA-256 of the normalized processing request and source receipts.
    pub processing_identity: String,
    pub cut_selection: RadarCutRequest,
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub requested_products: Vec<String>,
    #[serde(default)]
    pub sources: Vec<RadarSourceReceipt>,
    #[serde(default)]
    pub products: Vec<RadarProductReceipt>,
    #[serde(default)]
    pub unavailable_products: Vec<RadarUnavailableProduct>,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub failures: Vec<FailureRecord>,
}

impl RadarArtifactManifest {
    pub fn validate_contract(&self) -> Vec<String> {
        let mut failures = Vec::new();
        if self.schema_version != ARTIFACT_SCHEMA_VERSION {
            failures.push(format!(
                "unsupported schema_version '{}'; expected '{ARTIFACT_SCHEMA_VERSION}'",
                self.schema_version
            ));
        }
        if self.bowecho.version.trim().is_empty() || self.bowecho.commit.trim().is_empty() {
            failures.push("bowecho version and commit must be non-empty".into());
        }
        if !valid_sha256(&self.processing_identity) {
            failures.push("processing_identity is not a lowercase SHA-256 digest".into());
        }
        if !(64..=4096).contains(&self.width) || !(64..=4096).contains(&self.height) {
            failures.push("render dimensions must each be within 64..=4096".into());
        }
        if self.requested_products.is_empty() {
            failures.push("requested_products is empty".into());
        }
        let mut source_hashes = BTreeSet::new();
        for source in &self.sources {
            if !valid_sha256(&source.sha256) {
                failures.push(format!(
                    "source {} has an invalid SHA-256",
                    source.path.display()
                ));
            }
            source_hashes.insert(source.sha256.as_str());
        }
        let mut paths = BTreeSet::new();
        for product in &self.products {
            if !source_hashes.contains(product.source_sha256.as_str()) {
                failures.push(format!(
                    "product {} references undeclared source {}",
                    product.product, product.source_sha256
                ));
            }
            if product.artifact.mime_type != "image/png" {
                failures.push(format!(
                    "product {} is not declared image/png",
                    product.product
                ));
            }
            if product.artifact.width != Some(self.width)
                || product.artifact.height != Some(self.height)
            {
                failures.push(format!(
                    "product {} dimensions do not match the render contract",
                    product.product
                ));
            }
            if !valid_sha256(&product.artifact.sha256) {
                failures.push(format!(
                    "product {} has an invalid SHA-256",
                    product.product
                ));
            }
            if product.artifact.relative_path.is_absolute()
                || product
                    .artifact
                    .relative_path
                    .components()
                    .any(|component| {
                        matches!(
                            component,
                            Component::ParentDir | Component::RootDir | Component::Prefix(_)
                        )
                    })
            {
                failures.push(format!(
                    "product {} has an unsafe artifact path {}",
                    product.product,
                    product.artifact.relative_path.display()
                ));
            }
            if !paths.insert(product.artifact.relative_path.clone()) {
                failures.push(format!(
                    "artifact path {} is declared more than once",
                    product.artifact.relative_path.display()
                ));
            }
        }
        failures
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RadarSourceReceipt {
    pub path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
    pub format: String,
    pub readable: bool,
    pub complete: bool,
    pub completeness_evidence: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site: Option<RadarSiteReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vcp: Option<u16>,
    pub cut_count: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RadarProductReceipt {
    pub source_sha256: String,
    pub site_id: String,
    pub volume_time: String,
    pub product: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cut_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elevation_deg: Option<f32>,
    pub units: String,
    #[serde(default)]
    pub source_moments: Vec<String>,
    pub derivation: DerivationMethod,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dealias_method: Option<String>,
    pub range_km: f32,
    pub artifact: ArtifactFileReceipt,
    pub statistics: FiniteStatistics,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RadarUnavailableProduct {
    pub source_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site_id: Option<String>,
    pub product: String,
    pub cut_selection: RadarCutRequest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cut_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elevation_deg: Option<f32>,
    pub reason: String,
    #[serde(default)]
    pub source_moments: Vec<String>,
}

struct DecodedSource {
    receipt: RadarSourceReceipt,
    volume: RadarVolume,
}

struct StagedProduct {
    staged_path: PathBuf,
    receipt: RadarProductReceipt,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum ProductSpec {
    Ref,
    Vel,
    Sw,
    Zdr,
    Rho,
    Phi,
    Kdp,
    Dvel,
    Cref,
}

impl ProductSpec {
    fn parse(value: &str) -> Result<Self, CliError> {
        match value.trim().to_ascii_uppercase().as_str() {
            "REF" | "DBZ" => Ok(Self::Ref),
            "VEL" | "VR" => Ok(Self::Vel),
            "SW" => Ok(Self::Sw),
            "ZDR" => Ok(Self::Zdr),
            "RHO" | "RHOHV" | "CC" => Ok(Self::Rho),
            "PHI" | "PHIDP" => Ok(Self::Phi),
            "KDP" => Ok(Self::Kdp),
            "DVEL" | "DEALIASED_VELOCITY" => Ok(Self::Dvel),
            "CREF" | "COMPOSITE_REFLECTIVITY" => Ok(Self::Cref),
            other => Err(CliError::usage(
                format!(
                    "unsupported radar product '{other}'; expected REF, VEL, SW, ZDR, RHO, PHI, KDP, DVEL, or CREF"
                ),
                "",
            )),
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Ref => "REF",
            Self::Vel => "VEL",
            Self::Sw => "SW",
            Self::Zdr => "ZDR",
            Self::Rho => "RHO",
            Self::Phi => "PHI",
            Self::Kdp => "KDP",
            Self::Dvel => "DVEL",
            Self::Cref => "CREF",
        }
    }

    const fn source_moment(self) -> MomentType {
        match self {
            Self::Ref | Self::Cref => MomentType::Reflectivity,
            Self::Vel | Self::Dvel => MomentType::Velocity,
            Self::Sw => MomentType::SpectrumWidth,
            Self::Zdr => MomentType::DifferentialReflectivity,
            Self::Rho => MomentType::CorrelationCoefficient,
            Self::Phi => MomentType::DifferentialPhase,
            Self::Kdp => MomentType::SpecificDifferentialPhase,
        }
    }

    const fn units(self) -> &'static str {
        match self {
            Self::Ref | Self::Cref => "dBZ",
            Self::Vel | Self::Dvel | Self::Sw => "m s-1",
            Self::Zdr => "dB",
            Self::Rho => "1",
            Self::Phi => "degree",
            Self::Kdp => "degree km-1",
        }
    }
}

/// Decode and render radar inputs into hash-receipted transparent PNGs.
///
/// The artifact manifest is written last. Inputs are rehashed after all
/// expensive decoding/rendering and before any PNG is published, preventing a
/// changing live source from receiving a misleading complete receipt.
pub fn render(
    options: &RadarRenderOptions,
    context: &RuntimeContext,
) -> Result<(RadarArtifactManifest, PathBuf, ExitCode), CliError> {
    validate_render_options(options)?;
    let inputs = expand_radar_inputs(std::slice::from_ref(&options.input))?;
    let products = normalized_products(&options.moments)?;
    let output_root = prepare_output_root(&options.output_directory)?;
    let staging = tempfile::Builder::new()
        .prefix(".bowecho-radar-staging-")
        .tempdir_in(&output_root)
        .map_err(|error| {
            CliError::input(format!("cannot create radar staging directory: {error}"))
        })?;
    let staging_root = canonical_path_within_output(&output_root, staging.path())
        .map_err(|error| CliError::input(format!("unsafe radar staging directory: {error}")))?;

    let mut sources = Vec::with_capacity(inputs.len());
    let mut decoded = Vec::new();
    let mut failures = Vec::new();
    for path in inputs {
        let hash = match sha256_file(&path) {
            Ok(hash) => hash,
            Err(error) => {
                failures.push(FailureRecord {
                    stage: "source-hash".into(),
                    path: Some(path),
                    reason: error.to_string(),
                });
                continue;
            }
        };
        let raw = match fs::read(&path) {
            Ok(raw) => raw,
            Err(error) => {
                failures.push(FailureRecord {
                    stage: "source-read".into(),
                    path: Some(path.clone()),
                    reason: error.to_string(),
                });
                sources.push(unreadable_source(
                    path,
                    hash.bytes,
                    hash.sha256,
                    "unknown",
                    "read failed",
                ));
                continue;
            }
        };
        let format = format_name(sniff_format(&raw)).to_owned();
        match decode_supported_volume_bytes(&raw) {
            Ok(mut volume) => {
                volume.metadata.source_path = Some(path.display().to_string());
                let (complete, evidence) = volume_completeness(&volume, &format);
                let receipt = RadarSourceReceipt {
                    path,
                    bytes: hash.bytes,
                    sha256: hash.sha256,
                    format,
                    readable: true,
                    complete,
                    completeness_evidence: evidence,
                    site: Some(site_receipt(&volume)),
                    volume_time: Some(format_time(volume.volume_time)),
                    vcp: volume.vcp.as_ref().map(|vcp| vcp.pattern),
                    cut_count: volume.cuts.len(),
                };
                if !complete {
                    failures.push(FailureRecord {
                        stage: "source-completeness".into(),
                        path: Some(receipt.path.clone()),
                        reason: "Level-II source has no end-of-volume marker; refusing to label a partial live scan complete".into(),
                    });
                } else {
                    decoded.push(DecodedSource {
                        receipt: receipt.clone(),
                        volume,
                    });
                }
                sources.push(receipt);
            }
            Err(reason) => {
                failures.push(FailureRecord {
                    stage: "decode".into(),
                    path: Some(path.clone()),
                    reason: reason.clone(),
                });
                sources.push(unreadable_source(
                    path,
                    hash.bytes,
                    hash.sha256,
                    &format,
                    &reason,
                ));
            }
        }
    }

    sources.sort_by_key(|source| stable_path_key(&source.path));
    decoded.sort_by_key(|frame| stable_path_key(&frame.receipt.path));
    let mut staged_products = Vec::new();
    let mut unavailable_products = Vec::new();
    let tables = ColorTableSet::default();
    for source in &decoded {
        stage_volume_products(
            source,
            &products,
            options,
            &tables,
            &staging_root,
            &mut staged_products,
            &mut unavailable_products,
            &mut failures,
        );
    }

    let mut changed_source = false;
    for source in &sources {
        match sha256_file(&source.path) {
            Ok(now) if now.bytes == source.bytes && now.sha256 == source.sha256 => {}
            Ok(now) => {
                changed_source = true;
                failures.push(FailureRecord {
                    stage: "source-stability".into(),
                    path: Some(source.path.clone()),
                    reason: format!(
                        "source changed during processing (was {} bytes {}, now {} bytes {})",
                        source.bytes, source.sha256, now.bytes, now.sha256
                    ),
                });
            }
            Err(error) => {
                changed_source = true;
                failures.push(FailureRecord {
                    stage: "source-stability".into(),
                    path: Some(source.path.clone()),
                    reason: error.to_string(),
                });
            }
        }
    }

    let mut published_products = Vec::new();
    if !changed_source {
        for staged in staged_products {
            let destination = match prepare_output_destination(
                &output_root,
                &staged.receipt.artifact.relative_path,
            ) {
                Ok(destination) => destination,
                Err(reason) => {
                    failures.push(FailureRecord {
                        stage: "artifact-publish".into(),
                        path: Some(staged.receipt.artifact.relative_path.clone()),
                        reason,
                    });
                    continue;
                }
            };
            match publish_file_atomic(&staged.staged_path, &destination) {
                Ok(()) => match canonical_path_within_output(&output_root, &destination) {
                    Ok(published_path) => match sha256_file(&published_path) {
                        Ok(hash)
                            if hash.bytes == staged.receipt.artifact.bytes
                                && hash.sha256 == staged.receipt.artifact.sha256 =>
                        {
                            published_products.push(staged.receipt)
                        }
                        Ok(hash) => failures.push(FailureRecord {
                            stage: "artifact-publish".into(),
                            path: Some(published_path),
                            reason: format!(
                                "published bytes/hash differ from staging receipt: {} {}",
                                hash.bytes, hash.sha256
                            ),
                        }),
                        Err(error) => failures.push(FailureRecord {
                            stage: "artifact-publish".into(),
                            path: Some(published_path),
                            reason: error.to_string(),
                        }),
                    },
                    Err(reason) => failures.push(FailureRecord {
                        stage: "artifact-publish".into(),
                        path: Some(destination),
                        reason,
                    }),
                },
                Err(error) => failures.push(FailureRecord {
                    stage: "artifact-publish".into(),
                    path: Some(destination),
                    reason: error.to_string(),
                }),
            }
        }
    }
    published_products.sort_by(|a, b| {
        a.source_sha256
            .cmp(&b.source_sha256)
            .then_with(|| a.product.cmp(&b.product))
            .then_with(|| a.cut_index.cmp(&b.cut_index))
    });
    unavailable_products.sort_by(|a, b| {
        a.source_sha256
            .cmp(&b.source_sha256)
            .then_with(|| a.product.cmp(&b.product))
    });

    let status = if failures.is_empty() && !published_products.is_empty() {
        ArtifactStatus::Complete
    } else if !published_products.is_empty() {
        ArtifactStatus::Partial
    } else {
        ArtifactStatus::Failed
    };
    let mut manifest = RadarArtifactManifest {
        schema_version: ARTIFACT_SCHEMA_VERSION.into(),
        status,
        bowecho: build_identity(context),
        processing_identity: String::new(),
        cut_selection: RadarCutRequest::from(&options.cuts),
        width: options.width,
        height: options.height,
        requested_products: products
            .iter()
            .map(|product| product.name().into())
            .collect(),
        sources,
        products: published_products,
        unavailable_products,
        warnings: Vec::new(),
        failures,
    };
    manifest.processing_identity = recompute_processing_identity(&manifest)?;
    let manifest_path = prepare_output_destination(&output_root, Path::new(ARTIFACT_MANIFEST_NAME))
        .map_err(|reason| {
            CliError::input(format!("unsafe radar manifest destination: {reason}"))
        })?;
    write_json_atomic(&manifest_path, &manifest).map_err(|error| {
        CliError::input(format!(
            "cannot write radar artifact manifest {}: {error}",
            manifest_path.display()
        ))
    })?;
    let code = if manifest.status == ArtifactStatus::Complete {
        ExitCode::Success
    } else {
        ExitCode::Data
    };
    Ok((manifest, manifest_path, code))
}

#[allow(clippy::too_many_arguments)]
fn stage_volume_products(
    source: &DecodedSource,
    products: &[ProductSpec],
    options: &RadarRenderOptions,
    tables: &ColorTableSet,
    staging_root: &Path,
    staged: &mut Vec<StagedProduct>,
    unavailable: &mut Vec<RadarUnavailableProduct>,
    failures: &mut Vec<FailureRecord>,
) {
    // DVEL uses the same true whole-volume v4 solve as the desktop app. Build
    // it once per source volume so rendering several cuts never repeats the
    // expensive global solve.
    let v4_solution = products
        .contains(&ProductSpec::Dvel)
        .then(|| dealias_volume_v4(&source.volume, None, None));
    for &product in products {
        if product == ProductSpec::Cref {
            match stage_composite_reflectivity(source, options, tables, staging_root) {
                Ok(Some(product)) => staged.push(product),
                Ok(None) => unavailable.push(unavailable_product(
                    source,
                    product,
                    &options.cuts,
                    None,
                    "no decoded reflectivity cut can support composite reflectivity",
                )),
                Err(reason) => failures.push(FailureRecord {
                    stage: "radar-render".into(),
                    path: Some(source.receipt.path.clone()),
                    reason: format!("CREF: {reason}"),
                }),
            }
            continue;
        }

        let moment = product.source_moment();
        let selected = selected_cut_indices(&source.volume, &options.cuts, &moment);
        if selected.is_empty() {
            unavailable.push(unavailable_product(
                source,
                product,
                &options.cuts,
                explicit_single_cut(&options.cuts),
                &format!(
                    "{} is absent from the requested cut selection",
                    moment.short_name()
                ),
            ));
            continue;
        }
        for cut_index in selected {
            match stage_cut_product(
                source,
                product,
                cut_index,
                options,
                tables,
                staging_root,
                v4_solution.as_ref(),
            ) {
                Ok(product) => staged.push(product),
                Err(reason) => failures.push(FailureRecord {
                    stage: "radar-render".into(),
                    path: Some(source.receipt.path.clone()),
                    reason: format!("{} cut {cut_index}: {reason}", product.name()),
                }),
            }
        }
    }
}

fn stage_cut_product(
    source: &DecodedSource,
    product: ProductSpec,
    cut_index: usize,
    options: &RadarRenderOptions,
    tables: &ColorTableSet,
    staging_root: &Path,
    v4_solution: Option<&V4VolumeSolution>,
) -> Result<StagedProduct, String> {
    let cut = source
        .volume
        .cuts
        .get(cut_index)
        .ok_or_else(|| format!("cut index {cut_index} is out of range"))?;
    let moment = product.source_moment();
    let source_grid = cut
        .moments
        .get(&moment)
        .ok_or_else(|| format!("source moment {} is absent", moment.short_name()))?;
    if source_grid.radial_count() == 0 || source_grid.gate_range.gate_count == 0 {
        return Err(format!(
            "source moment {} has no decoded gates",
            moment.short_name()
        ));
    }

    let (cache, statistics, derivation, dealias_method) = if product == ProductSpec::Dvel {
        let dealiased = v4_solution
            .and_then(|solution| solution.tilt_grid(cut_index))
            .cloned()
            .ok_or_else(|| {
                format!("whole-volume v4 returned no velocity grid for cut {cut_index}")
            })?;
        let statistics = grid_statistics(&dealiased);
        let method = v4_dealias_method_label(dealias_skipped_no_nyquist(cut, source_grid));
        let cache = ViewportMomentCache::new_dealiased_velocity_from_grid_with_color_tables(
            &source.volume,
            cut_index,
            dealiased,
            tables,
        )
        .map_err(|error| error.to_string())?;
        (
            cache,
            statistics,
            DerivationMethod::Derived,
            Some(method.into()),
        )
    } else {
        let cache = ViewportMomentCache::new_with_color_tables(
            &source.volume,
            cut_index,
            moment.clone(),
            tables,
        )
        .map_err(|error| error.to_string())?;
        (
            cache,
            grid_statistics(source_grid),
            DerivationMethod::Direct,
            None,
        )
    };
    let range_km = grid_max_range_km(source_grid).max(1.0);
    stage_cache_png(
        source,
        product,
        Some(cut_index),
        Some(cut.elevation_deg),
        cache,
        range_km,
        statistics,
        derivation,
        dealias_method,
        options,
        staging_root,
    )
}

fn v4_dealias_method_label(pass_through: bool) -> &'static str {
    if pass_through {
        "whole-volume v4 pass-through (source has no usable Nyquist velocity; no temporal/model anchors)"
    } else {
        "BowEcho whole-volume v4 velocity dealiasing (no temporal/model anchors)"
    }
}

fn stage_composite_reflectivity(
    source: &DecodedSource,
    options: &RadarRenderOptions,
    tables: &ColorTableSet,
    staging_root: &Path,
) -> Result<Option<StagedProduct>, String> {
    let Some((cut_index, _)) = source
        .volume
        .cuts
        .iter()
        .enumerate()
        .filter_map(|(index, cut)| {
            cut.moments
                .get(&MomentType::Reflectivity)
                .map(|grid| (index, cut.elevation_deg, grid))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(index, _, grid)| (index, grid))
    else {
        return Ok(None);
    };
    let Some(grid) = composite_reflectivity_grid(&source.volume) else {
        return Ok(None);
    };
    let statistics = grid_statistics(&grid);
    let range_km = grid_max_range_km(&grid).max(1.0);
    let cache = ViewportMomentCache::new_derived(
        &source.volume,
        cut_index,
        grid,
        ColorTableFamily::Reflectivity,
        tables,
    )
    .map_err(|error| error.to_string())?;
    Ok(Some(stage_cache_png(
        source,
        ProductSpec::Cref,
        None,
        None,
        cache,
        range_km,
        statistics,
        DerivationMethod::Derived,
        None,
        options,
        staging_root,
    )?))
}

#[allow(clippy::too_many_arguments)]
fn stage_cache_png(
    source: &DecodedSource,
    product: ProductSpec,
    cut_index: Option<usize>,
    elevation_deg: Option<f32>,
    cache: ViewportMomentCache,
    range_km: f32,
    statistics: FiniteStatistics,
    derivation: DerivationMethod,
    dealias_method: Option<String>,
    options: &RadarRenderOptions,
    staging_root: &Path,
) -> Result<StagedProduct, String> {
    let viewport = viewport_options(options.width, options.height, range_km);
    let mut pixels = vec![0; viewport_rgba_buffer_len(viewport)];
    cache
        .render_moment_rgba_into(&source.volume, viewport, &mut pixels)
        .map_err(|error| error.to_string())?;
    let relative_path = product_relative_path(source, product, cut_index);
    let staged_path = staging_root.join(&relative_path);
    let parent = staged_path
        .parent()
        .ok_or_else(|| "artifact path has no parent".to_owned())?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    image::save_buffer_with_format(
        &staged_path,
        &pixels,
        options.width,
        options.height,
        image::ColorType::Rgba8,
        ImageFormat::Png,
    )
    .map_err(|error| error.to_string())?;
    let hash = sha256_file(&staged_path).map_err(|error| error.to_string())?;
    Ok(StagedProduct {
        staged_path,
        receipt: RadarProductReceipt {
            source_sha256: source.receipt.sha256.clone(),
            site_id: source.volume.site.id.clone(),
            volume_time: format_time(source.volume.volume_time),
            product: product.name().into(),
            cut_index,
            elevation_deg,
            units: product.units().into(),
            source_moments: vec![product.source_moment().short_name().into()],
            derivation,
            dealias_method,
            range_km,
            artifact: ArtifactFileReceipt {
                relative_path,
                mime_type: "image/png".into(),
                width: Some(options.width),
                height: Some(options.height),
                bytes: hash.bytes,
                sha256: hash.sha256,
            },
            statistics,
        },
    })
}

fn viewport_options(width: u32, height: u32, range_km: f32) -> ViewportRasterOptions {
    let radius_px = width.min(height) as f32 / 2.0;
    let km_per_px = range_km / radius_px.max(1.0);
    ViewportRasterOptions {
        width,
        height,
        radar_x_px: (width as f32 - 1.0) / 2.0,
        radar_y_px: (height as f32 - 1.0) / 2.0,
        km_per_px_x: km_per_px,
        km_per_px_y: km_per_px,
        rotation_rad: 0.0,
    }
}

fn selected_cut_indices(
    volume: &RadarVolume,
    selection: &RadarCutSelection,
    moment: &MomentType,
) -> Vec<usize> {
    match selection {
        RadarCutSelection::Lowest => volume
            .cuts
            .iter()
            .enumerate()
            .filter(|(_, cut)| {
                cut.moments
                    .get(moment)
                    .is_some_and(|grid| grid.radial_count() > 0)
            })
            .min_by(|a, b| a.1.elevation_deg.total_cmp(&b.1.elevation_deg))
            .map(|(index, _)| vec![index])
            .unwrap_or_default(),
        RadarCutSelection::All => volume
            .cuts
            .iter()
            .enumerate()
            .filter_map(|(index, cut)| {
                cut.moments
                    .get(moment)
                    .is_some_and(|grid| grid.radial_count() > 0)
                    .then_some(index)
            })
            .collect(),
        RadarCutSelection::Explicit(indices) => indices
            .iter()
            .copied()
            .filter(|&index| {
                volume
                    .cuts
                    .get(index)
                    .and_then(|cut| cut.moments.get(moment))
                    .is_some_and(|grid| grid.radial_count() > 0)
            })
            .collect(),
    }
}

fn explicit_single_cut(selection: &RadarCutSelection) -> Option<usize> {
    match selection {
        RadarCutSelection::Explicit(indices) if indices.len() == 1 => indices.first().copied(),
        _ => None,
    }
}

fn unavailable_product(
    source: &DecodedSource,
    product: ProductSpec,
    selection: &RadarCutSelection,
    cut_index: Option<usize>,
    reason: &str,
) -> RadarUnavailableProduct {
    RadarUnavailableProduct {
        source_sha256: source.receipt.sha256.clone(),
        site_id: Some(source.volume.site.id.clone()),
        product: product.name().into(),
        cut_selection: RadarCutRequest::from(selection),
        cut_index,
        elevation_deg: cut_index
            .and_then(|index| source.volume.cuts.get(index))
            .map(|cut| cut.elevation_deg),
        reason: reason.into(),
        source_moments: vec![product.source_moment().short_name().into()],
    }
}

fn prepare_output_root(path: &Path) -> Result<PathBuf, CliError> {
    fs::create_dir_all(path).map_err(|error| {
        CliError::input(format!(
            "cannot create radar output directory {}: {error}",
            path.display()
        ))
    })?;
    let root = fs::canonicalize(path).map_err(|error| {
        CliError::input(format!(
            "cannot canonicalize radar output directory {}: {error}",
            path.display()
        ))
    })?;
    let metadata = fs::metadata(&root).map_err(|error| {
        CliError::input(format!(
            "cannot inspect radar output directory {}: {error}",
            root.display()
        ))
    })?;
    if !metadata.is_dir() {
        return Err(CliError::input(format!(
            "radar output root is not a directory: {}",
            root.display()
        )));
    }
    Ok(root)
}

/// Resolve a relative artifact path beneath the canonical output root without
/// ever creating directories through a symbolic link or Windows reparse point.
fn prepare_output_destination(output_root: &Path, relative: &Path) -> Result<PathBuf, String> {
    if relative.as_os_str().is_empty() {
        return Err("radar output path is empty".into());
    }
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!(
            "radar output path must be a plain relative path: {}",
            relative.display()
        ));
    }
    let file_name = relative
        .file_name()
        .ok_or_else(|| format!("radar output path has no file name: {}", relative.display()))?;
    let parent_relative = relative.parent().unwrap_or_else(|| Path::new(""));
    let mut parent = output_root.to_path_buf();
    for component in parent_relative.components() {
        let Component::Normal(name) = component else {
            return Err(format!(
                "radar output path has an unsafe parent: {}",
                relative.display()
            ));
        };
        let candidate = parent.join(name);
        match fs::symlink_metadata(&candidate) {
            Ok(metadata) => {
                if is_link_or_reparse_point(&metadata) {
                    return Err(format!(
                        "radar output path traverses a link or reparse point: {}",
                        candidate.display()
                    ));
                }
                if !metadata.is_dir() {
                    return Err(format!(
                        "radar output parent is not a directory: {}",
                        candidate.display()
                    ));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&candidate).map_err(|error| {
                    format!(
                        "cannot create radar output directory {}: {error}",
                        candidate.display()
                    )
                })?;
            }
            Err(error) => {
                return Err(format!(
                    "cannot inspect radar output directory {}: {error}",
                    candidate.display()
                ));
            }
        }
        parent = canonical_path_within_output(output_root, &candidate)?;
    }

    let destination = parent.join(file_name);
    match fs::symlink_metadata(&destination) {
        Ok(metadata) => {
            if is_link_or_reparse_point(&metadata) {
                return Err(format!(
                    "radar output destination is a link or reparse point: {}",
                    destination.display()
                ));
            }
            if !metadata.is_file() {
                return Err(format!(
                    "radar output destination is not a regular file: {}",
                    destination.display()
                ));
            }
            canonical_path_within_output(output_root, &destination)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "cannot inspect radar output destination {}: {error}",
                destination.display()
            ));
        }
    }
    Ok(destination)
}

fn canonical_path_within_output(output_root: &Path, path: &Path) -> Result<PathBuf, String> {
    let resolved = fs::canonicalize(path)
        .map_err(|error| format!("cannot canonicalize {}: {error}", path.display()))?;
    if !resolved.starts_with(output_root) {
        return Err(format!(
            "radar output path resolves outside --out: {}",
            resolved.display()
        ));
    }
    Ok(resolved)
}

fn is_link_or_reparse_point(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn validate_render_options(options: &RadarRenderOptions) -> Result<(), CliError> {
    if !(64..=4096).contains(&options.width) || !(64..=4096).contains(&options.height) {
        return Err(CliError::data(
            "radar image width and height must each be within 64..=4096",
        ));
    }
    if let RadarCutSelection::Explicit(indices) = &options.cuts {
        if indices.is_empty() {
            return Err(CliError::data("explicit radar cut selection is empty"));
        }
        if indices.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(CliError::data(
                "explicit radar cut indices must be sorted and unique",
            ));
        }
    }
    Ok(())
}

fn normalized_products(requested: &[String]) -> Result<Vec<ProductSpec>, CliError> {
    let names = if requested.is_empty() {
        DEFAULT_PRODUCTS
            .iter()
            .map(|name| (*name).to_owned())
            .collect()
    } else {
        requested.to_vec()
    };
    let mut products = BTreeSet::new();
    for name in names {
        products.insert(ProductSpec::parse(&name)?);
    }
    Ok(products.into_iter().collect())
}

fn recompute_processing_identity(manifest: &RadarArtifactManifest) -> Result<String, CliError> {
    let value = serde_json::json!({
        "schema_version": manifest.schema_version.as_str(),
        "bowecho_version": manifest.bowecho.version.as_str(),
        "bowecho_commit": manifest.bowecho.commit.as_str(),
        "source_sha256": manifest.sources.iter().map(|source| source.sha256.as_str()).collect::<Vec<_>>(),
        "products": &manifest.requested_products,
        "cuts": &manifest.cut_selection,
        "width": manifest.width,
        "height": manifest.height,
    });
    let bytes = serde_json::to_vec(&value).map_err(|error| {
        CliError::internal(format!("cannot encode radar processing identity: {error}"))
    })?;
    sha256_reader(&bytes[..])
        .map(|receipt| receipt.sha256)
        .map_err(|error| {
            CliError::internal(format!("cannot hash radar processing identity: {error}"))
        })
}

fn product_relative_path(
    source: &DecodedSource,
    product: ProductSpec,
    cut_index: Option<usize>,
) -> PathBuf {
    let site = slug(if source.volume.site.id.trim().is_empty() {
        "unknown-site"
    } else {
        &source.volume.site.id
    });
    let time = source
        .volume
        .volume_time
        .format("%Y%m%dT%H%M%SZ")
        .to_string();
    let digest = &source.receipt.sha256[..12.min(source.receipt.sha256.len())];
    let cut = cut_index
        .map(|index| format!("cut-{index:03}"))
        .unwrap_or_else(|| "volume".into());
    PathBuf::from(site)
        .join(time)
        .join(digest)
        .join(cut)
        .join(format!("{}.png", slug(product.name())))
}

fn unreadable_source(
    path: PathBuf,
    bytes: u64,
    sha256: String,
    format: &str,
    evidence: &str,
) -> RadarSourceReceipt {
    RadarSourceReceipt {
        path,
        bytes,
        sha256,
        format: format.into(),
        readable: false,
        complete: false,
        completeness_evidence: evidence.into(),
        site: None,
        volume_time: None,
        vcp: None,
        cut_count: 0,
    }
}

fn site_receipt(volume: &RadarVolume) -> RadarSiteReceipt {
    RadarSiteReceipt {
        id: volume.site.id.clone(),
        name: volume.site.name.clone(),
        latitude_deg: finite_option(volume.site.latitude_deg),
        longitude_deg: finite_option(volume.site.longitude_deg),
        elevation_m: finite_option(volume.site.elevation_m),
    }
}

fn finite_option(value: Option<f32>) -> Option<f32> {
    value.filter(|value| value.is_finite())
}

fn grid_statistics(grid: &MomentGrid) -> FiniteStatistics {
    let mut minimum = f64::INFINITY;
    let mut maximum = f64::NEG_INFINITY;
    let mut finite_value_count = 0_u64;
    let mut missing_value_count = 0_u64;
    for row in 0..grid.radial_count() {
        for gate in 0..grid.gate_range.gate_count {
            match grid.scaled_value(row, gate) {
                Some(value) if value.is_finite() => {
                    let value = f64::from(value);
                    minimum = minimum.min(value);
                    maximum = maximum.max(value);
                    finite_value_count = finite_value_count.saturating_add(1);
                }
                _ => missing_value_count = missing_value_count.saturating_add(1),
            }
        }
    }
    let available = finite_value_count > 0;
    FiniteStatistics {
        available,
        minimum: available.then_some(minimum),
        maximum: available.then_some(maximum),
        finite_value_count,
        missing_value_count,
    }
}

fn grid_max_range_km(grid: &MomentGrid) -> f32 {
    ((grid.gate_range.first_gate_m as f64
        + grid.gate_range.gate_spacing_m as f64 * grid.gate_range.gate_count as f64)
        / 1000.0)
        .max(0.0) as f32
}

fn build_identity(context: &RuntimeContext) -> BuildIdentity {
    BuildIdentity {
        version: context.bowecho_version.clone(),
        commit: context.bowecho_commit.clone(),
    }
}

fn format_time(value: DateTime<Utc>) -> String {
    value.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

fn radial_time(volume: &RadarVolume, offset_ms: i32) -> DateTime<Utc> {
    let relative = volume.volume_time + Duration::milliseconds(i64::from(offset_ms));
    if !(0..86_400_000).contains(&offset_ms) {
        return relative;
    }
    let midnight = volume.volume_time
        - Duration::hours(i64::from(volume.volume_time.hour()))
        - Duration::minutes(i64::from(volume.volume_time.minute()))
        - Duration::seconds(i64::from(volume.volume_time.second()))
        - Duration::nanoseconds(i64::from(volume.volume_time.nanosecond()));
    let from_midnight = midnight + Duration::milliseconds(i64::from(offset_ms));
    let relative_distance = (relative - volume.volume_time).num_milliseconds().abs();
    let midnight_distance = (from_midnight - volume.volume_time)
        .num_milliseconds()
        .abs();
    if midnight_distance <= relative_distance {
        from_midnight
    } else {
        relative
    }
}

fn sniff_format(raw: &[u8]) -> SupportedVolumeFormat {
    sniff_supported_volume_format(raw)
}

fn format_name(format: SupportedVolumeFormat) -> &'static str {
    match format {
        SupportedVolumeFormat::Dorade => "DoradeSweep",
        SupportedVolumeFormat::OdimH5 => "OdimH5",
        SupportedVolumeFormat::CfRadial => "CfRadial",
        SupportedVolumeFormat::JmaGrib2Tar => "JmaGrib2Tar",
        SupportedVolumeFormat::NexradLevel2 => "NexradLevel2",
    }
}

fn volume_completeness(volume: &RadarVolume, format: &str) -> (bool, String) {
    if format != "NexradLevel2" {
        let complete = !volume.cuts.is_empty()
            && volume.cuts.iter().any(|cut| {
                !cut.radials.is_empty()
                    && cut
                        .moments
                        .values()
                        .any(|grid| grid.radial_count() > 0 && grid.gate_range.gate_count > 0)
            });
        return (
            complete,
            if complete {
                "decoded non-Level-II container with populated polar geometry"
            } else {
                "decoded container has no populated polar geometry"
            }
            .into(),
        );
    }
    let end_volume = volume.cuts.iter().any(|cut| {
        cut.radials
            .iter()
            .any(|radial| radial.radial_status == Some(RadialStatus::EndVolume))
    });
    (
        end_volume,
        if end_volume {
            "NEXRAD radial status EndVolume"
        } else {
            "no NEXRAD radial status EndVolume"
        }
        .into(),
    )
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn stable_path_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn expand_radar_inputs(inputs: &[PathBuf]) -> Result<Vec<PathBuf>, CliError> {
    if inputs.is_empty() {
        return Err(CliError::input("radar input is required"));
    }
    let mut files = Vec::new();
    for input in inputs {
        let metadata = fs::symlink_metadata(input).map_err(|error| {
            CliError::input(format!(
                "cannot inspect radar input {}: {error}",
                input.display()
            ))
        })?;
        if metadata.file_type().is_symlink() {
            return Err(CliError::input(format!(
                "refusing symlink input {}",
                input.display()
            )));
        }
        if metadata.is_file() {
            files.push(absolute_without_canonicalizing(input)?);
        } else if metadata.is_dir() {
            visit_radar_directory(input, 0, &mut files)?;
        } else {
            return Err(CliError::input(format!(
                "radar input is neither a regular file nor directory: {}",
                input.display()
            )));
        }
    }
    files.sort_by_key(|path| stable_path_key(path));
    files.dedup();
    if files.is_empty() {
        return Err(CliError::input("no radar candidates were found"));
    }
    Ok(files)
}

fn visit_radar_directory(
    directory: &Path,
    depth: usize,
    files: &mut Vec<PathBuf>,
) -> Result<(), CliError> {
    if depth > MAX_DISCOVERY_DEPTH {
        return Err(CliError::input(format!(
            "radar input discovery exceeds depth {MAX_DISCOVERY_DEPTH} at {}",
            directory.display()
        )));
    }
    let mut entries = fs::read_dir(directory)
        .map_err(|error| CliError::input(format!("cannot read {}: {error}", directory.display())))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            CliError::input(format!("cannot enumerate {}: {error}", directory.display()))
        })?;
    entries.sort_by_key(|entry| stable_path_key(&entry.path()));
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| CliError::input(format!("cannot stat {}: {error}", path.display())))?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            visit_radar_directory(&path, depth + 1, files)?;
        } else if metadata.is_file() && is_radar_candidate(&path) {
            files.push(absolute_without_canonicalizing(&path)?);
            if files.len() > MAX_DISCOVERED_INPUTS {
                return Err(CliError::input(format!(
                    "radar input discovery exceeds {MAX_DISCOVERED_INPUTS} files"
                )));
            }
        }
    }
    Ok(())
}

fn is_radar_candidate(path: &Path) -> bool {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(
        extension.as_str(),
        "ar2v" | "l2" | "raw" | "bz2" | "gz" | "zip" | "h5" | "hdf5" | "nc" | "cdf" | "swp" | "tar"
    ) {
        return true;
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if name.len() >= 4
        && name.as_bytes()[0].eq_ignore_ascii_case(&b'k')
        && name.as_bytes()[1..4].iter().all(u8::is_ascii_alphanumeric)
    {
        return true;
    }
    let mut head = [0_u8; 512];
    File::open(path)
        .and_then(|mut file| file.read(&mut head))
        .is_ok_and(|read| looks_like_radar_magic(&head[..read]))
}

fn looks_like_radar_magic(head: &[u8]) -> bool {
    head.starts_with(b"AR2V")
        || head.starts_with(b"BZh")
        || head.starts_with(&[0x1f, 0x8b])
        || head.starts_with(b"PK\x03\x04")
        || head.starts_with(b"CDF\x01")
        || head.starts_with(b"CDF\x02")
        || head.starts_with(&[0x89, b'H', b'D', b'F', 0x0d, 0x0a, 0x1a, 0x0a])
        || head.get(257..262) == Some(b"ustar")
        || matches!(head.get(0..4), Some(b"COMM" | b"SSWB" | b"VOLD" | b"RADD"))
}

fn absolute_without_canonicalizing(path: &Path) -> Result<PathBuf, CliError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|error| CliError::input(format!("cannot resolve current directory: {error}")))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RadarReceiptKind {
    Source,
    Artifact,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RadarReceiptVerification {
    pub kind: RadarReceiptKind,
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
#[serde(deny_unknown_fields)]
pub struct RadarVerifyReport {
    pub schema_version: String,
    pub artifact_schema_version: String,
    pub artifact_status: ArtifactStatus,
    pub processing_complete: bool,
    pub manifest_path: PathBuf,
    pub manifest_bytes: u64,
    pub manifest_sha256: String,
    pub verified: bool,
    #[serde(default)]
    pub contract_failures: Vec<String>,
    #[serde(default)]
    pub receipts: Vec<RadarReceiptVerification>,
}

/// Recalculate every source and artifact receipt in a radar manifest.
pub fn verify(manifest_path: &Path) -> Result<(RadarVerifyReport, ExitCode), CliError> {
    let manifest_size = fs::metadata(manifest_path)
        .map_err(|error| {
            CliError::input(format!(
                "cannot stat radar artifact manifest {}: {error}",
                manifest_path.display()
            ))
        })?
        .len();
    if manifest_size > MAX_MANIFEST_BYTES {
        return Err(CliError::data(format!(
            "radar artifact manifest is {manifest_size} bytes; maximum is {MAX_MANIFEST_BYTES}"
        )));
    }
    let bytes = fs::read(manifest_path).map_err(|error| {
        CliError::input(format!(
            "cannot read radar artifact manifest {}: {error}",
            manifest_path.display()
        ))
    })?;
    let manifest: RadarArtifactManifest = serde_json::from_slice(&bytes).map_err(|error| {
        CliError::data(format!(
            "invalid radar artifact manifest {}: {error}",
            manifest_path.display()
        ))
    })?;
    let manifest_hash = sha256_reader(&bytes[..]).map_err(|error| {
        CliError::input(format!("cannot hash radar artifact manifest: {error}"))
    })?;
    let mut contract_failures = manifest.validate_contract();
    let expected_processing_identity = recompute_processing_identity(&manifest)?;
    if manifest.processing_identity != expected_processing_identity {
        contract_failures.push(format!(
            "processing_identity does not match the manifest request and source receipts: expected {expected_processing_identity}, got {}",
            manifest.processing_identity
        ));
    }
    let processing_complete = manifest.status == ArtifactStatus::Complete
        && !manifest.products.is_empty()
        && manifest.failures.is_empty()
        && manifest
            .sources
            .iter()
            .all(|source| source.readable && source.complete);
    if manifest.status != ArtifactStatus::Complete {
        contract_failures.push(format!(
            "artifact status is {:?}; only complete radar manifests pass verification",
            manifest.status
        ));
    }
    if manifest.products.is_empty() {
        contract_failures.push("radar artifact manifest declares no generated products".into());
    }
    if manifest
        .sources
        .iter()
        .any(|source| !source.readable || !source.complete)
    {
        contract_failures.push("one or more radar sources are unreadable or incomplete".into());
    }
    let manifest_directory = manifest_path.parent().unwrap_or_else(|| Path::new("."));
    let mut receipts = Vec::with_capacity(manifest.sources.len() + manifest.products.len());
    for source in &manifest.sources {
        receipts.push(verify_receipt(
            RadarReceiptKind::Source,
            source.path.display().to_string(),
            &source.path,
            source.bytes,
            &source.sha256,
            manifest_directory,
            true,
        ));
    }
    let mut artifact_paths = BTreeSet::new();
    for product in &manifest.products {
        if !artifact_paths.insert(product.artifact.relative_path.clone()) {
            contract_failures.push(format!(
                "multiple products declare {}",
                product.artifact.relative_path.display()
            ));
        }
        let mut receipt = verify_receipt(
            RadarReceiptKind::Artifact,
            format!(
                "{} cut {}",
                product.product,
                product
                    .cut_index
                    .map_or_else(|| "volume".into(), |index| index.to_string())
            ),
            &product.artifact.relative_path,
            product.artifact.bytes,
            &product.artifact.sha256,
            manifest_directory,
            false,
        );
        verify_png(product, &mut receipt);
        receipts.push(receipt);
    }
    let verified = processing_complete
        && contract_failures.is_empty()
        && receipts.iter().all(|receipt| receipt.verified);
    let report = RadarVerifyReport {
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

#[allow(clippy::too_many_arguments)]
fn verify_receipt(
    kind: RadarReceiptKind,
    label: String,
    declared_path: &Path,
    expected_bytes: u64,
    expected_sha256: &str,
    manifest_directory: &Path,
    allow_absolute: bool,
) -> RadarReceiptVerification {
    let mut receipt = RadarReceiptVerification {
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
        failures: Vec::new(),
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
        match (
            fs::canonicalize(manifest_directory),
            fs::canonicalize(&path),
        ) {
            (Ok(root), Ok(resolved)) if resolved.starts_with(&root) => {}
            (Ok(_), Ok(resolved)) => {
                receipt.failures.push(format!(
                    "artifact receipt resolves outside manifest directory: {}",
                    resolved.display()
                ));
                return receipt;
            }
            (Err(error), _) | (_, Err(error)) => {
                receipt
                    .failures
                    .push(format!("cannot canonicalize artifact boundary: {error}"));
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
    let before = fs::metadata(&path)
        .ok()
        .map(|metadata| (metadata.len(), metadata.modified().ok()));
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
                    "SHA-256 mismatch: expected {expected_sha256}, got {}",
                    actual.sha256
                ));
            }
            let after = fs::metadata(&path)
                .ok()
                .map(|metadata| (metadata.len(), metadata.modified().ok()));
            if before != after {
                receipt
                    .failures
                    .push("file changed while its receipt was verified".into());
            }
            receipt.verified = receipt.failures.is_empty();
        }
        Err(error) => receipt
            .failures
            .push(format!("cannot hash {}: {error}", path.display())),
    }
    receipt
}

fn verify_png(product: &RadarProductReceipt, receipt: &mut RadarReceiptVerification) {
    if !receipt.verified {
        return;
    }
    let Some(path) = receipt.resolved_path.as_ref() else {
        return;
    };
    let reader =
        match image::ImageReader::open(path).and_then(|reader| reader.with_guessed_format()) {
            Ok(reader) => reader,
            Err(error) => {
                receipt
                    .failures
                    .push(format!("cannot inspect PNG header: {error}"));
                receipt.verified = false;
                return;
            }
        };
    if reader.format() != Some(ImageFormat::Png) {
        receipt.failures.push("artifact encoding is not PNG".into());
        receipt.verified = false;
        return;
    }
    match reader.decode() {
        Ok(decoded)
            if product.artifact.width == Some(decoded.width())
                && product.artifact.height == Some(decoded.height()) => {}
        Ok(decoded) => {
            receipt.failures.push(format!(
                "PNG dimensions mismatch: declared {:?}x{:?}, decoded {width}x{height}",
                product.artifact.width,
                product.artifact.height,
                width = decoded.width(),
                height = decoded.height()
            ));
            receipt.verified = false;
        }
        Err(error) => {
            receipt
                .failures
                .push(format!("cannot fully decode PNG: {error}"));
            receipt.verified = false;
        }
    }
}

/// Execute a parsed radar command without initializing the desktop runtime.
pub fn execute(
    command: &RadarCommand,
    context: &RuntimeContext,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<ExitCode, CliError> {
    match command {
        RadarCommand::Inspect { input, json } => {
            let (report, code) = inspect(input, context)?;
            if *json {
                write_json(stdout, &report)?;
            } else {
                writeln!(
                    stdout,
                    "radar inspect: {:?}; {} file(s), {} failure(s)",
                    report.status,
                    report.files.len(),
                    report.failures.len()
                )
                .map_err(output_error)?;
            }
            for failure in &report.failures {
                writeln!(stderr, "{}: {}", failure.stage, failure.reason).map_err(output_error)?;
            }
            Ok(code)
        }
        RadarCommand::Render(options) => {
            let (manifest, path, code) = render(options, context)?;
            if options.json {
                write_json(stdout, &manifest)?;
            } else {
                writeln!(
                    stdout,
                    "radar render: {:?}; {} product(s); {}",
                    manifest.status,
                    manifest.products.len(),
                    path.display()
                )
                .map_err(output_error)?;
            }
            for failure in &manifest.failures {
                writeln!(stderr, "{}: {}", failure.stage, failure.reason).map_err(output_error)?;
            }
            Ok(code)
        }
        RadarCommand::Verify { manifest } => {
            let (report, code) = verify(manifest)?;
            write_json(stdout, &report)?;
            Ok(code)
        }
    }
}

fn write_json(output: &mut dyn Write, value: &impl Serialize) -> Result<(), CliError> {
    serde_json::to_writer_pretty(&mut *output, value)
        .map_err(|error| CliError::internal(format!("cannot write radar JSON: {error}")))?;
    writeln!(output).map_err(output_error)
}

fn output_error(error: std::io::Error) -> CliError {
    CliError::internal(format!("cannot write radar command output: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> RuntimeContext {
        RuntimeContext::new("test", "test-commit")
    }

    fn complete_manifest(directory: &Path) -> RadarArtifactManifest {
        let source_path = directory.join("source.ar2v");
        fs::write(&source_path, b"radar source fixture").unwrap();
        let source_hash = sha256_file(&source_path).unwrap();

        let artifact_path = directory.join("ref.png");
        image::RgbaImage::from_pixel(64, 64, image::Rgba([1, 2, 3, 255]))
            .save(&artifact_path)
            .unwrap();
        let artifact_hash = sha256_file(&artifact_path).unwrap();
        let mut manifest = RadarArtifactManifest {
            schema_version: ARTIFACT_SCHEMA_VERSION.into(),
            status: ArtifactStatus::Complete,
            bowecho: BuildIdentity {
                version: "test".into(),
                commit: "test-commit".into(),
            },
            processing_identity: String::new(),
            cut_selection: RadarCutRequest::Lowest,
            width: 64,
            height: 64,
            requested_products: vec!["REF".into()],
            sources: vec![RadarSourceReceipt {
                path: source_path,
                bytes: source_hash.bytes,
                sha256: source_hash.sha256.clone(),
                format: "NexradLevel2".into(),
                readable: true,
                complete: true,
                completeness_evidence: "test EndVolume".into(),
                site: None,
                volume_time: Some("2026-01-01T00:00:00.000Z".into()),
                vcp: Some(212),
                cut_count: 1,
            }],
            products: vec![RadarProductReceipt {
                source_sha256: source_hash.sha256,
                site_id: "KTLX".into(),
                volume_time: "2026-01-01T00:00:00.000Z".into(),
                product: "REF".into(),
                cut_index: Some(0),
                elevation_deg: Some(0.5),
                units: "dBZ".into(),
                source_moments: vec!["REF".into()],
                derivation: DerivationMethod::Direct,
                dealias_method: None,
                range_km: 460.0,
                artifact: ArtifactFileReceipt {
                    relative_path: PathBuf::from("ref.png"),
                    mime_type: "image/png".into(),
                    width: Some(64),
                    height: Some(64),
                    bytes: artifact_hash.bytes,
                    sha256: artifact_hash.sha256,
                },
                statistics: FiniteStatistics::default(),
            }],
            unavailable_products: Vec::new(),
            warnings: Vec::new(),
            failures: Vec::new(),
        };
        manifest.processing_identity = recompute_processing_identity(&manifest).unwrap();
        manifest
    }

    fn write_manifest(directory: &Path, manifest: &RadarArtifactManifest) -> PathBuf {
        let manifest_path = directory.join(ARTIFACT_MANIFEST_NAME);
        write_json_atomic(&manifest_path, manifest).unwrap();
        manifest_path
    }

    #[test]
    fn verifier_accepts_exact_receipts_and_rejects_tampered_png() {
        let directory = tempfile::tempdir().unwrap();
        let manifest = complete_manifest(directory.path());
        let artifact_path = directory.path().join("ref.png");
        let manifest_path = directory.path().join(ARTIFACT_MANIFEST_NAME);
        write_json_atomic(&manifest_path, &manifest).unwrap();

        let (report, code) = verify(&manifest_path).unwrap();
        assert_eq!(code, ExitCode::Success);
        assert!(report.verified);

        fs::write(&artifact_path, b"tampered").unwrap();
        let (report, code) = verify(&manifest_path).unwrap();
        assert_eq!(code, ExitCode::Verification);
        assert!(!report.verified);
    }

    #[test]
    fn output_destination_rejects_parent_aliases() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("out");
        let output_root = prepare_output_root(&output).unwrap();
        let error =
            prepare_output_destination(&output_root, Path::new("../escaped.png")).unwrap_err();
        assert!(error.contains("plain relative path"));
        assert!(!directory.path().join("escaped.png").exists());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn output_destination_rejects_linked_directory_escape() {
        let output = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let linked = output.path().join("linked");
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), &linked).unwrap();
        #[cfg(windows)]
        if let Err(error) = std::os::windows::fs::symlink_dir(outside.path(), &linked) {
            // Creating Windows directory links can require Developer Mode or
            // SeCreateSymbolicLinkPrivilege; production junctions are caught
            // by the same FILE_ATTRIBUTE_REPARSE_POINT check.
            if error.raw_os_error() == Some(1314) {
                return;
            }
            panic!("cannot create directory link for containment test: {error}");
        }

        let output_root = prepare_output_root(output.path()).unwrap();
        let error =
            prepare_output_destination(&output_root, Path::new("linked/escaped.png")).unwrap_err();
        assert!(error.contains("link or reparse point"));
        assert!(!outside.path().join("escaped.png").exists());
    }

    #[test]
    fn verifier_rejects_processing_identity_mismatch() {
        let directory = tempfile::tempdir().unwrap();
        let mut manifest = complete_manifest(directory.path());
        manifest.cut_selection = RadarCutRequest::All;
        let manifest_path = write_manifest(directory.path(), &manifest);

        let (report, code) = verify(&manifest_path).unwrap();
        assert_eq!(code, ExitCode::Verification);
        assert!(!report.verified);
        assert!(
            report
                .contract_failures
                .iter()
                .any(|failure| failure.contains("processing_identity does not match"))
        );
    }

    #[test]
    fn verifier_fully_decodes_png_payload() {
        let directory = tempfile::tempdir().unwrap();
        let mut manifest = complete_manifest(directory.path());
        let artifact_path = directory.path().join("ref.png");
        let mut png = fs::read(&artifact_path).unwrap();
        let idat_type = png
            .windows(4)
            .position(|window| window == b"IDAT")
            .expect("encoded fixture must contain IDAT");
        let idat_length = u32::from_be_bytes(
            png[idat_type - 4..idat_type]
                .try_into()
                .expect("IDAT length is four bytes"),
        ) as usize;
        png.truncate(idat_type + 4 + idat_length / 2);
        fs::write(&artifact_path, &png).unwrap();
        assert_eq!(
            image::ImageReader::open(&artifact_path)
                .unwrap()
                .with_guessed_format()
                .unwrap()
                .into_dimensions()
                .unwrap(),
            (64, 64),
            "the truncated fixture must still pass a header-only dimension check"
        );
        let artifact_hash = sha256_file(&artifact_path).unwrap();
        manifest.products[0].artifact.bytes = artifact_hash.bytes;
        manifest.products[0].artifact.sha256 = artifact_hash.sha256;
        let manifest_path = write_manifest(directory.path(), &manifest);

        let (report, code) = verify(&manifest_path).unwrap();
        assert_eq!(code, ExitCode::Verification);
        assert!(!report.verified);
        assert!(report.receipts.iter().any(|receipt| {
            receipt
                .failures
                .iter()
                .any(|failure| failure.contains("cannot fully decode PNG"))
        }));
    }

    #[test]
    fn default_product_set_is_native_and_aliases_are_normalized() {
        let products = normalized_products(&[]).unwrap();
        assert_eq!(
            products
                .iter()
                .map(|product| product.name())
                .collect::<Vec<_>>(),
            vec!["REF", "VEL", "SW", "ZDR", "RHO", "PHI", "KDP"]
        );
        assert_eq!(ProductSpec::parse("cc").unwrap(), ProductSpec::Rho);
        assert_eq!(ProductSpec::parse("dbz").unwrap(), ProductSpec::Ref);
    }

    #[test]
    fn dvel_receipt_names_the_true_whole_volume_v4_route() {
        assert_eq!(
            v4_dealias_method_label(false),
            "BowEcho whole-volume v4 velocity dealiasing (no temporal/model anchors)"
        );
        assert_eq!(
            v4_dealias_method_label(true),
            "whole-volume v4 pass-through (source has no usable Nyquist velocity; no temporal/model anchors)"
        );
        assert!(!v4_dealias_method_label(false).contains("region-v4"));
    }

    #[test]
    fn dvel_no_nyquist_route_passes_through_and_declares_no_anchors() {
        let gate_range = radar_core::GateRange {
            first_gate_m: 0,
            gate_spacing_m: 1_000,
            gate_count: 3,
        };
        let mut cut = radar_core::ElevationCut::new(0.5, Some(1));
        cut.radials.push(radar_core::Radial {
            azimuth_deg: 0.0,
            elevation_deg: 0.5,
            time_offset_ms: 0,
            gate_range: gate_range.clone(),
            nyquist_velocity_mps: None,
            radial_status: None,
        });
        let mut velocity = MomentGrid::new_u8(
            MomentType::Velocity,
            gate_range,
            1.0,
            64.0,
            Some(0),
            Some(1),
        );
        velocity
            .push_u8_row_slice(0, &[64, 72, 55])
            .expect("velocity row");
        cut.moments.insert(MomentType::Velocity, velocity);
        let mut volume = RadarVolume::new(
            radar_core::RadarSite::new("TEST"),
            chrono::DateTime::<Utc>::UNIX_EPOCH,
        );
        volume.cuts.push(cut);

        let source = volume.cuts[0]
            .moments
            .get(&MomentType::Velocity)
            .expect("source velocity");
        assert!(dealias_skipped_no_nyquist(&volume.cuts[0], source));
        let solution = dealias_volume_v4(&volume, None, None);
        let output = solution.tilt_grid(0).expect("pass-through v4 grid");
        for gate in 0..3 {
            assert_eq!(output.scaled_value(0, gate), source.scaled_value(0, gate));
        }
        let label = v4_dealias_method_label(true);
        assert!(label.contains("pass-through"));
        assert!(label.contains("no temporal/model anchors"));
    }

    #[test]
    #[ignore = "requires BOWECHO_RADAR_FIXTURE pointing at a real radar volume"]
    fn real_radar_fixture_inspects_renders_and_verifies() {
        let fixture = PathBuf::from(
            std::env::var("BOWECHO_RADAR_FIXTURE")
                .expect("BOWECHO_RADAR_FIXTURE must name a real radar volume"),
        );
        let (inspection, code) = inspect(&fixture, &context()).unwrap();
        assert_eq!(code, ExitCode::Success);
        assert_eq!(inspection.status, InspectStatus::Complete);

        let output = tempfile::tempdir().unwrap();
        let options = RadarRenderOptions {
            input: fixture,
            output_directory: output.path().to_path_buf(),
            moments: vec!["REF".into()],
            cuts: RadarCutSelection::Lowest,
            width: 256,
            height: 256,
            json: false,
        };
        let (manifest, manifest_path, code) = render(&options, &context()).unwrap();
        assert_eq!(code, ExitCode::Success);
        assert_eq!(manifest.status, ArtifactStatus::Complete);
        assert!(!manifest.products.is_empty());
        let (verification, code) = verify(&manifest_path).unwrap();
        assert_eq!(code, ExitCode::Success);
        assert!(verification.verified);
    }
}
