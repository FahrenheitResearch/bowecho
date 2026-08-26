//! Headless host for `bowecho satellite list/fetch/inspect/render/verify`.
//!
//! Satellite science and presentation stay single-sourced: stored frames are
//! opened by [`crate::sat_worker::load_frame_for_cli`] and rendered by
//! [`crate::sat_plot::SatellitePlotSource`], the same path used by the
//! desktop Satellite window. This module owns only strict CLI orchestration,
//! receipts, and versioned machine contracts.

use std::collections::BTreeSet;
use std::fs::{self, Metadata};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use bowecho_cli::{
    CliError, ExitCode, RuntimeContext, SatelliteArchiveRange, SatelliteArchiveSelector,
    SatelliteFetchOptions, SatelliteFrameSelection, SatelliteListOptions, SatelliteRenderOptions,
};
use chrono::{DateTime, Utc};
use rw_sat::store::frame_file_name;
use rw_store::grid::GridFile;
use rw_store::reader::HourReader;
use rw_store::run::RwsRunManifest;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::sat_plot::{SatellitePlotRaster, SatellitePlotSource};
use crate::sat_worker::{
    IrEnhancement, NativeSatelliteArchiveBounds, NativeSatelliteArchiveCatalog,
    NativeSatelliteArchiveFrame, catalog_native_satellite_archive,
    fetch_native_satellite_archive_frame,
};

pub(crate) const SATELLITE_INSPECT_SCHEMA_VERSION: &str = "bowecho.satellite.inspect.v1";
pub(crate) const SATELLITE_CATALOG_SCHEMA_VERSION: &str = "bowecho.satellite.catalog.v1";
pub(crate) const SATELLITE_FETCH_SCHEMA_VERSION: &str = "bowecho.satellite.fetch.v1";
pub(crate) const SATELLITE_ARTIFACT_SCHEMA_VERSION: &str = "bowecho.satellite.artifacts.v1";
pub(crate) const SATELLITE_VERIFY_SCHEMA_VERSION: &str = "bowecho.satellite.verify.v1";
const RUSTY_WEATHER_COMMIT: &str = "68b74857780e436843cbf599c25ebccb886f7b8a";
const MAX_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SatelliteStatus {
    Complete,
    Partial,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SatelliteRasterKind {
    ObservedScalar,
    ObservedBakedRgb,
    SimulatedScalar,
    SimulatedBakedRgb,
}

impl SatelliteRasterKind {
    fn is_rgb(self) -> bool {
        matches!(self, Self::ObservedBakedRgb | Self::SimulatedBakedRgb)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SatelliteBuildIdentity {
    pub version: String,
    pub commit: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SatelliteArchiveSelectorReport {
    pub source: String,
    pub satellite: String,
    pub product: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sector: Option<String>,
}

impl From<&SatelliteArchiveSelector> for SatelliteArchiveSelectorReport {
    fn from(selector: &SatelliteArchiveSelector) -> Self {
        Self {
            source: selector.source.clone(),
            satellite: selector.satellite.clone(),
            product: selector.product.clone(),
            sector: selector.sector.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SatelliteArchiveRangeReport {
    pub start_utc: String,
    pub end_utc: String,
}

impl From<&SatelliteArchiveRange> for SatelliteArchiveRangeReport {
    fn from(range: &SatelliteArchiveRange) -> Self {
        Self {
            start_utc: archive_time(range.start_utc),
            end_utc: archive_time(range.end_utc),
        }
    }
}

/// Provider-advertised limits. `None` means that provider has no reliable
/// capabilities endpoint for that field; it never means an unbounded archive.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SatelliteProviderBounds {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_time_utc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_time_utc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cadence_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub west_degrees: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub south_degrees: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub east_degrees: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub north_degrees: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SatelliteArchiveFrameReport {
    pub scan_start_utc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan_end_utc: Option<String>,
    /// Exact immutable provider identifiers needed to reacquire this scan.
    /// A composite has one identifier per required native band/segment.
    pub source_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_urls: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SatelliteCatalogReport {
    pub schema_version: String,
    pub status: SatelliteStatus,
    pub bowecho: SatelliteBuildIdentity,
    pub selector: SatelliteArchiveSelectorReport,
    pub requested_range: SatelliteArchiveRangeReport,
    pub result_limit: usize,
    /// True when the provider has additional matching scans beyond `frames`.
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_bounds: Option<SatelliteProviderBounds>,
    pub frame_count: usize,
    #[serde(default)]
    pub frames: Vec<SatelliteArchiveFrameReport>,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub failures: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SatelliteFetchedFrameReport {
    pub scan_start_utc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan_end_utc: Option<String>,
    pub source_ids: Vec<String>,
    pub status: SatelliteStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_directory: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hhmm: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stored_bytes: Option<u64>,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub failures: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SatelliteFetchReport {
    pub schema_version: String,
    pub status: SatelliteStatus,
    pub bowecho: SatelliteBuildIdentity,
    pub selector: SatelliteArchiveSelectorReport,
    pub requested_range: SatelliteArchiveRangeReport,
    pub store_root: PathBuf,
    pub max_frames: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_bounds: Option<SatelliteProviderBounds>,
    pub catalogued_frame_count: usize,
    pub attempted_frame_count: usize,
    pub fetched_frame_count: usize,
    #[serde(default)]
    pub run_directories: Vec<PathBuf>,
    #[serde(default)]
    pub frames: Vec<SatelliteFetchedFrameReport>,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub failures: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ArchiveCliFailureKind {
    Usage,
    Unavailable,
    Data,
    Store,
}

impl ArchiveCliFailureKind {
    const fn exit_code(self) -> ExitCode {
        match self {
            Self::Usage => ExitCode::Usage,
            Self::Unavailable => ExitCode::Unavailable,
            Self::Data | Self::Store => ExitCode::Data,
        }
    }
}

#[derive(Clone, Debug)]
struct ArchiveCliFailure {
    kind: ArchiveCliFailureKind,
    message: String,
}

#[derive(Clone, Debug)]
struct NativeArchiveFrame {
    scan_start_utc: DateTime<Utc>,
    scan_end_utc: Option<DateTime<Utc>>,
    source_ids: Vec<String>,
    source_urls: Vec<String>,
    source_bytes: Option<u64>,
    native: NativeSatelliteArchiveFrame,
}

impl NativeArchiveFrame {
    fn report(&self) -> SatelliteArchiveFrameReport {
        SatelliteArchiveFrameReport {
            scan_start_utc: archive_time(self.scan_start_utc),
            scan_end_utc: self.scan_end_utc.map(archive_time),
            source_ids: self.source_ids.clone(),
            source_urls: self.source_urls.clone(),
            source_bytes: self.source_bytes,
        }
    }
}

#[derive(Clone, Debug)]
struct NativeArchiveCatalog {
    provider_bounds: Option<SatelliteProviderBounds>,
    frames: Vec<NativeArchiveFrame>,
    truncated: bool,
    warnings: Vec<String>,
}

#[derive(Clone, Debug)]
struct NativeFetchedFrame {
    source: NativeArchiveFrame,
    run_directory: PathBuf,
    model: String,
    run: String,
    hhmm: u16,
    stored_bytes: u64,
    warnings: Vec<String>,
}

#[derive(Clone, Debug)]
struct NativeFetchFailure {
    source: NativeArchiveFrame,
    error: String,
}

#[derive(Clone, Debug, Default)]
struct NativeFetchResult {
    stored: Vec<NativeFetchedFrame>,
    failed: Vec<NativeFetchFailure>,
    warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SatelliteVariableInspection {
    pub name: String,
    pub units: String,
    pub kind: String,
    pub selector: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finite_minimum: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finite_maximum: Option<f32>,
    pub finite_count: u64,
    pub missing_count: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SatelliteFrameInspection {
    pub hhmm: u16,
    pub file: String,
    pub bytes: u64,
    pub readable: bool,
    pub nx: usize,
    pub ny: usize,
    pub grid_hash: String,
    pub raster_kind: SatelliteRasterKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub satellite: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instrument: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub product: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sector: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan_start_utc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan_end_utc: Option<String>,
    #[serde(default)]
    pub variables: Vec<SatelliteVariableInspection>,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub failures: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SatelliteInspectReport {
    pub schema_version: String,
    pub status: SatelliteStatus,
    pub bowecho: SatelliteBuildIdentity,
    pub run_directory: PathBuf,
    pub model: String,
    pub run: String,
    pub run_schema: String,
    pub grid_hash: String,
    pub nx: usize,
    pub ny: usize,
    #[serde(default)]
    pub frames: Vec<SatelliteFrameInspection>,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub failures: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SatelliteSourceRole {
    Grid,
    Frame,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SatelliteSourceReceipt {
    pub role: SatelliteSourceRole,
    pub path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hhmm: Option<u16>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SatelliteArtifactReceipt {
    pub relative_path: PathBuf,
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SatelliteFiniteStatistics {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum: Option<f32>,
    pub finite_count: u64,
    pub missing_count: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SatelliteProductReceipt {
    pub hhmm: u16,
    pub variable: String,
    pub units: String,
    pub raster_kind: SatelliteRasterKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub satellite: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instrument: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub product: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sector: Option<String>,
    pub scan_start_utc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan_end_utc: Option<String>,
    pub grid_hash: String,
    pub nx: usize,
    pub ny: usize,
    pub selector: serde_json::Value,
    pub statistics: SatelliteFiniteStatistics,
    pub artifact: SatelliteArtifactReceipt,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SatelliteArtifactManifest {
    pub schema_version: String,
    pub status: SatelliteStatus,
    pub bowecho: SatelliteBuildIdentity,
    pub rusty_weather_commit: String,
    pub processing_identity: String,
    pub model: String,
    pub run: String,
    pub ir_enhancement: String,
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub sources: Vec<SatelliteSourceReceipt>,
    #[serde(default)]
    pub products: Vec<SatelliteProductReceipt>,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub failures: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct SatelliteRenderOutput {
    pub manifest: SatelliteArtifactManifest,
    pub manifest_path: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SatelliteVerificationKind {
    Source,
    Artifact,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SatelliteReceiptVerification {
    pub kind: SatelliteVerificationKind,
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
    pub verified: bool,
    #[serde(default)]
    pub failures: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SatelliteVerifyReport {
    pub schema_version: String,
    pub artifact_schema_version: String,
    pub manifest_path: PathBuf,
    pub manifest_bytes: u64,
    pub manifest_sha256: String,
    pub processing_complete: bool,
    pub verified: bool,
    #[serde(default)]
    pub contract_failures: Vec<String>,
    #[serde(default)]
    pub receipts: Vec<SatelliteReceiptVerification>,
}

struct OpenSatelliteRun {
    directory: PathBuf,
    model: String,
    run: String,
    manifest: RwsRunManifest,
    grid: GridFile,
}

#[derive(Default)]
struct SelectorIdentity {
    provider: Option<String>,
    satellite: Option<String>,
    instrument: Option<String>,
    product: Option<String>,
    sector: Option<String>,
    scan_start_utc: Option<String>,
    scan_end_utc: Option<String>,
}

struct SatelliteProcessingRequest<'a> {
    schema_version: &'a str,
    bowecho: &'a SatelliteBuildIdentity,
    rusty_weather_commit: &'a str,
    model: &'a str,
    run: &'a str,
    ir_enhancement: &'a str,
    width: u32,
    height: u32,
}

impl<'a> SatelliteProcessingRequest<'a> {
    fn from_manifest(manifest: &'a SatelliteArtifactManifest) -> Self {
        Self {
            schema_version: &manifest.schema_version,
            bowecho: &manifest.bowecho,
            rusty_weather_commit: &manifest.rusty_weather_commit,
            model: &manifest.model,
            run: &manifest.run,
            ir_enhancement: &manifest.ir_enhancement,
            width: manifest.width,
            height: manifest.height,
        }
    }
}

pub(crate) fn execute_inspect(
    run_directory: &Path,
    context: &RuntimeContext,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<ExitCode, CliError> {
    let report = inspect_run(run_directory, context)?;
    serde_json::to_writer_pretty(&mut *stdout, &report)
        .map_err(|error| CliError::internal(format!("serialize satellite inspection: {error}")))?;
    writeln!(stdout)
        .map_err(|error| CliError::internal(format!("write satellite inspection: {error}")))?;
    writeln!(
        stderr,
        "BowEcho satellite inspect: {}/{} has {} frame(s), status {:?}",
        report.model,
        report.run,
        report.frames.len(),
        report.status
    )
    .map_err(|error| CliError::internal(format!("write satellite inspection status: {error}")))?;
    Ok(match report.status {
        SatelliteStatus::Complete => ExitCode::Success,
        SatelliteStatus::Partial | SatelliteStatus::Failed => ExitCode::Data,
    })
}

pub(crate) fn execute_list(
    options: &SatelliteListOptions,
    context: &RuntimeContext,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<ExitCode, CliError> {
    let (report, code) = list_archive(options, context);
    serde_json::to_writer_pretty(&mut *stdout, &report)
        .map_err(|error| CliError::internal(format!("serialize satellite catalog: {error}")))?;
    writeln!(stdout)
        .map_err(|error| CliError::internal(format!("write satellite catalog: {error}")))?;
    writeln!(
        stderr,
        "BowEcho satellite list: {} frame(s), truncated={}, status {:?}",
        report.frame_count, report.truncated, report.status
    )
    .map_err(|error| CliError::internal(format!("write satellite catalog status: {error}")))?;
    Ok(code)
}

pub(crate) fn execute_fetch(
    options: &SatelliteFetchOptions,
    context: &RuntimeContext,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<ExitCode, CliError> {
    let (report, code) = fetch_archive(options, context, stderr)?;
    serde_json::to_writer_pretty(&mut *stdout, &report).map_err(|error| {
        CliError::internal(format!("serialize satellite fetch report: {error}"))
    })?;
    writeln!(stdout)
        .map_err(|error| CliError::internal(format!("write satellite fetch report: {error}")))?;
    writeln!(
        stderr,
        "BowEcho satellite fetch: {} of {} frame(s), status {:?}",
        report.fetched_frame_count, report.catalogued_frame_count, report.status
    )
    .map_err(|error| CliError::internal(format!("write satellite fetch status: {error}")))?;
    Ok(code)
}

fn list_archive(
    options: &SatelliteListOptions,
    context: &RuntimeContext,
) -> (SatelliteCatalogReport, ExitCode) {
    match catalog_native_archive(&options.range, options.limit) {
        Ok(catalog) => {
            let frames = catalog
                .frames
                .iter()
                .map(NativeArchiveFrame::report)
                .collect::<Vec<_>>();
            let status = if catalog.warnings.is_empty() {
                SatelliteStatus::Complete
            } else {
                SatelliteStatus::Partial
            };
            let code = if status == SatelliteStatus::Complete {
                ExitCode::Success
            } else {
                ExitCode::Data
            };
            (
                SatelliteCatalogReport {
                    schema_version: SATELLITE_CATALOG_SCHEMA_VERSION.to_owned(),
                    status,
                    bowecho: build_identity(context),
                    selector: (&options.range.selector).into(),
                    requested_range: (&options.range).into(),
                    result_limit: options.limit,
                    truncated: catalog.truncated,
                    provider_bounds: catalog.provider_bounds,
                    frame_count: frames.len(),
                    frames,
                    warnings: catalog.warnings,
                    failures: Vec::new(),
                },
                code,
            )
        }
        Err(error) => (
            failed_catalog_report(options, context, error.message),
            error.kind.exit_code(),
        ),
    }
}

fn fetch_archive(
    options: &SatelliteFetchOptions,
    context: &RuntimeContext,
    stderr: &mut dyn Write,
) -> Result<(SatelliteFetchReport, ExitCode), CliError> {
    let probe_limit = options.max_frames.saturating_add(1);
    let catalog = match catalog_native_archive(&options.range, probe_limit) {
        Ok(catalog) => catalog,
        Err(error) => {
            let code = error.kind.exit_code();
            return Ok((
                failed_fetch_report(options, context, None, 0, error.message),
                code,
            ));
        }
    };

    // This check happens before store-root creation and, more importantly,
    // before the provider fetch adapter is called. A caller can therefore
    // safely use --max-frames as a hard network and disk budget boundary.
    if selection_exceeds_fetch_cap(catalog.frames.len(), catalog.truncated, options.max_frames) {
        let found = if catalog.truncated {
            format!("more than {}", catalog.frames.len())
        } else {
            catalog.frames.len().to_string()
        };
        return Ok((
            failed_fetch_report(
                options,
                context,
                catalog.provider_bounds,
                catalog.frames.len(),
                format!(
                    "satellite archive selection contains {found} frame(s), above --max-frames {}",
                    options.max_frames
                ),
            ),
            ExitCode::Data,
        ));
    }
    if catalog.frames.is_empty() {
        return Ok((
            failed_fetch_report(
                options,
                context,
                catalog.provider_bounds,
                0,
                "satellite archive selection contains no native frames".to_owned(),
            ),
            ExitCode::Data,
        ));
    }

    let store_root = prepare_archive_store_root(&options.store_root)?;
    let fetched =
        match fetch_native_archive(&store_root, &catalog.frames, options.max_frames, stderr) {
            Ok(result) => result,
            Err(error) => {
                let code = error.kind.exit_code();
                return Ok((
                    failed_fetch_report(
                        options,
                        context,
                        catalog.provider_bounds,
                        catalog.frames.len(),
                        error.message,
                    ),
                    code,
                ));
            }
        };

    let mut frames = Vec::with_capacity(fetched.stored.len() + fetched.failed.len());
    let mut run_directories = BTreeSet::new();
    for stored in fetched.stored {
        run_directories.insert(stored.run_directory.clone());
        frames.push(SatelliteFetchedFrameReport {
            scan_start_utc: archive_time(stored.source.scan_start_utc),
            scan_end_utc: stored.source.scan_end_utc.map(archive_time),
            source_ids: stored.source.source_ids,
            status: SatelliteStatus::Complete,
            run_directory: Some(stored.run_directory),
            model: Some(stored.model),
            run: Some(stored.run),
            hhmm: Some(stored.hhmm),
            stored_bytes: Some(stored.stored_bytes),
            warnings: stored.warnings,
            failures: Vec::new(),
        });
    }
    let mut failures = Vec::with_capacity(fetched.failed.len());
    for failed in fetched.failed {
        failures.push(format!(
            "{}: {}",
            archive_time(failed.source.scan_start_utc),
            failed.error
        ));
        frames.push(SatelliteFetchedFrameReport {
            scan_start_utc: archive_time(failed.source.scan_start_utc),
            scan_end_utc: failed.source.scan_end_utc.map(archive_time),
            source_ids: failed.source.source_ids,
            status: SatelliteStatus::Failed,
            run_directory: None,
            model: None,
            run: None,
            hhmm: None,
            stored_bytes: None,
            warnings: Vec::new(),
            failures: vec![failed.error],
        });
    }
    frames.sort_by(|left, right| left.scan_start_utc.cmp(&right.scan_start_utc));
    let fetched_frame_count = frames
        .iter()
        .filter(|frame| frame.status == SatelliteStatus::Complete)
        .count();
    let status = if failures.is_empty() && fetched_frame_count == catalog.frames.len() {
        SatelliteStatus::Complete
    } else if fetched_frame_count == 0 {
        SatelliteStatus::Failed
    } else {
        SatelliteStatus::Partial
    };
    let code = if status == SatelliteStatus::Complete {
        ExitCode::Success
    } else {
        ExitCode::Data
    };
    Ok((
        SatelliteFetchReport {
            schema_version: SATELLITE_FETCH_SCHEMA_VERSION.to_owned(),
            status,
            bowecho: build_identity(context),
            selector: (&options.range.selector).into(),
            requested_range: (&options.range).into(),
            store_root,
            max_frames: options.max_frames,
            provider_bounds: catalog.provider_bounds,
            catalogued_frame_count: catalog.frames.len(),
            attempted_frame_count: frames.len(),
            fetched_frame_count,
            run_directories: run_directories.into_iter().collect(),
            frames,
            warnings: fetched.warnings,
            failures,
        },
        code,
    ))
}

fn failed_catalog_report(
    options: &SatelliteListOptions,
    context: &RuntimeContext,
    failure: String,
) -> SatelliteCatalogReport {
    SatelliteCatalogReport {
        schema_version: SATELLITE_CATALOG_SCHEMA_VERSION.to_owned(),
        status: SatelliteStatus::Failed,
        bowecho: build_identity(context),
        selector: (&options.range.selector).into(),
        requested_range: (&options.range).into(),
        result_limit: options.limit,
        truncated: false,
        provider_bounds: None,
        frame_count: 0,
        frames: Vec::new(),
        warnings: Vec::new(),
        failures: vec![failure],
    }
}

fn failed_fetch_report(
    options: &SatelliteFetchOptions,
    context: &RuntimeContext,
    provider_bounds: Option<SatelliteProviderBounds>,
    catalogued_frame_count: usize,
    failure: String,
) -> SatelliteFetchReport {
    SatelliteFetchReport {
        schema_version: SATELLITE_FETCH_SCHEMA_VERSION.to_owned(),
        status: SatelliteStatus::Failed,
        bowecho: build_identity(context),
        selector: (&options.range.selector).into(),
        requested_range: (&options.range).into(),
        store_root: options.store_root.clone(),
        max_frames: options.max_frames,
        provider_bounds,
        catalogued_frame_count,
        attempted_frame_count: 0,
        fetched_frame_count: 0,
        run_directories: Vec::new(),
        frames: Vec::new(),
        warnings: Vec::new(),
        failures: vec![failure],
    }
}

fn archive_time(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn selection_exceeds_fetch_cap(frame_count: usize, truncated: bool, max_frames: usize) -> bool {
    frame_count > max_frames || truncated
}

fn catalog_native_archive(
    range: &SatelliteArchiveRange,
    limit: usize,
) -> Result<NativeArchiveCatalog, ArchiveCliFailure> {
    let catalog = catalog_native_satellite_archive(
        &range.selector.source,
        &range.selector.satellite,
        &range.selector.product,
        range.selector.sector.as_deref(),
        range.start_utc,
        range.end_utc,
        limit,
    )
    .map_err(|message| ArchiveCliFailure {
        kind: classify_catalog_failure(&message),
        message,
    })?;
    Ok(convert_native_catalog(catalog))
}

fn convert_native_catalog(catalog: NativeSatelliteArchiveCatalog) -> NativeArchiveCatalog {
    NativeArchiveCatalog {
        provider_bounds: catalog.provider_bounds.map(convert_native_bounds),
        frames: catalog
            .frames
            .into_iter()
            .map(|frame| NativeArchiveFrame {
                scan_start_utc: frame.scan_start_utc,
                scan_end_utc: frame.scan_end_utc,
                source_ids: frame.source_ids.clone(),
                source_urls: frame.source_urls.clone(),
                source_bytes: frame.source_bytes,
                native: frame,
            })
            .collect(),
        truncated: catalog.truncated,
        warnings: catalog.warnings,
    }
}

fn convert_native_bounds(bounds: NativeSatelliteArchiveBounds) -> SatelliteProviderBounds {
    SatelliteProviderBounds {
        first_time_utc: bounds.first_time_utc.map(archive_time),
        latest_time_utc: bounds.latest_time_utc.map(archive_time),
        cadence_seconds: bounds.cadence_seconds,
        west_degrees: bounds.west_degrees,
        south_degrees: bounds.south_degrees,
        east_degrees: bounds.east_degrees,
        north_degrees: bounds.north_degrees,
    }
}

fn classify_catalog_failure(message: &str) -> ArchiveCliFailureKind {
    if message.starts_with("unknown ")
        || message.contains("requires --sector")
        || message.contains("supports the full disk sector")
        || message.contains("supports the advertised full-disk")
        || message.contains("result limit must be positive")
        || message.contains("end precedes start")
    {
        ArchiveCliFailureKind::Usage
    } else {
        ArchiveCliFailureKind::Unavailable
    }
}

fn fetch_native_archive(
    store_root: &Path,
    frames: &[NativeArchiveFrame],
    max_frames: usize,
    stderr: &mut dyn Write,
) -> Result<NativeFetchResult, ArchiveCliFailure> {
    if frames.len() > max_frames {
        return Err(ArchiveCliFailure {
            kind: ArchiveCliFailureKind::Data,
            message: format!(
                "satellite archive selection contains {} frame(s), above --max-frames {max_frames}",
                frames.len()
            ),
        });
    }

    let mut result = NativeFetchResult::default();
    for source in frames.iter().cloned() {
        let notes = std::cell::RefCell::new(Vec::new());
        let note = |message: String| notes.borrow_mut().push(message);
        let fetched =
            fetch_native_satellite_archive_frame(store_root, source.native.clone(), &note);
        let frame_notes = notes.into_inner();
        for message in &frame_notes {
            writeln!(stderr, "BowEcho satellite fetch: {message}").map_err(|error| {
                ArchiveCliFailure {
                    kind: ArchiveCliFailureKind::Store,
                    message: format!("write satellite fetch progress: {error}"),
                }
            })?;
        }
        match fetched {
            Ok(stored) => {
                let run_directory = stored.path.parent().ok_or_else(|| ArchiveCliFailure {
                    kind: ArchiveCliFailureKind::Store,
                    message: format!(
                        "stored satellite frame has no run-directory parent: {}",
                        stored.path.display()
                    ),
                })?;
                result.stored.push(NativeFetchedFrame {
                    source,
                    run_directory: run_directory.to_path_buf(),
                    model: stored.model,
                    run: stored.run,
                    hhmm: stored.hhmm,
                    stored_bytes: stored.bytes,
                    warnings: Vec::new(),
                });
            }
            Err(error) => result.failed.push(NativeFetchFailure { source, error }),
        }
    }
    Ok(result)
}

fn prepare_archive_store_root(store_root: &Path) -> Result<PathBuf, CliError> {
    let store_root = absolute_output_path(store_root, "satellite archive store root")?;
    reject_linked_path_components(&store_root, "satellite archive store root")?;
    fs::create_dir_all(&store_root).map_err(|error| {
        CliError::input(format!(
            "create satellite archive store root {}: {error}",
            store_root.display()
        ))
    })?;
    reject_linked_path_components(&store_root, "satellite archive store root")?;
    let metadata = fs::symlink_metadata(&store_root).map_err(|error| {
        CliError::input(format!(
            "inspect satellite archive store root {}: {error}",
            store_root.display()
        ))
    })?;
    if is_link_or_reparse(&metadata) || !metadata.is_dir() {
        return Err(CliError::input(format!(
            "satellite archive store root must be a non-linked directory: {}",
            store_root.display()
        )));
    }
    fs::canonicalize(&store_root).map_err(|error| {
        CliError::input(format!(
            "canonicalize satellite archive store root {}: {error}",
            store_root.display()
        ))
    })
}

pub(crate) fn inspect_run(
    run_directory: &Path,
    context: &RuntimeContext,
) -> Result<SatelliteInspectReport, CliError> {
    let opened = open_run(run_directory)?;
    let mut frames = Vec::with_capacity(opened.manifest.hours.len());
    for (&hhmm, entry) in &opened.manifest.hours {
        frames.push(inspect_frame(&opened, hhmm, &entry.file));
    }
    let mut warnings = Vec::new();
    if frames.is_empty() {
        warnings.push("satellite run declares no frames".to_owned());
    }
    let status = if frames.iter().any(|frame| !frame.failures.is_empty()) {
        SatelliteStatus::Failed
    } else if !warnings.is_empty() || frames.iter().any(|frame| !frame.warnings.is_empty()) {
        SatelliteStatus::Partial
    } else {
        SatelliteStatus::Complete
    };
    Ok(SatelliteInspectReport {
        schema_version: SATELLITE_INSPECT_SCHEMA_VERSION.to_owned(),
        status,
        bowecho: build_identity(context),
        run_directory: opened.directory,
        model: opened.model,
        run: opened.run,
        run_schema: opened.manifest.schema,
        grid_hash: opened.grid.hash,
        nx: opened.grid.nx,
        ny: opened.grid.ny,
        frames,
        warnings,
        failures: Vec::new(),
    })
}

fn open_run(run_directory: &Path) -> Result<OpenSatelliteRun, CliError> {
    let metadata = fs::symlink_metadata(run_directory).map_err(|error| {
        CliError::input(format!(
            "cannot inspect satellite run directory {}: {error}",
            run_directory.display()
        ))
    })?;
    if metadata.file_type().is_symlink() {
        return Err(CliError::input(format!(
            "refusing symlink satellite run directory {}",
            run_directory.display()
        )));
    }
    if !metadata.is_dir() {
        return Err(CliError::input(format!(
            "satellite input is not an rw-store run directory: {}",
            run_directory.display()
        )));
    }
    let directory = fs::canonicalize(run_directory).map_err(|error| {
        CliError::input(format!(
            "cannot resolve satellite run directory {}: {error}",
            run_directory.display()
        ))
    })?;
    let run = directory
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| CliError::input("satellite run path has no valid Unicode run component"))?
        .to_owned();
    let model_directory = directory
        .parent()
        .ok_or_else(|| CliError::input("satellite run path has no model parent"))?;
    let model = model_directory
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| CliError::input("satellite run path has no valid Unicode model component"))?
        .to_owned();
    let manifest_path = directory.join("run.json");
    refuse_symlink_file(&manifest_path, "satellite run manifest")?;
    let manifest = RwsRunManifest::load_for_run(&manifest_path, &model, &run)
        .map_err(|error| CliError::data(format!("open satellite run manifest: {error}")))?;
    let grid_path = directory.join("grid.rwg");
    refuse_symlink_file(&grid_path, "satellite grid")?;
    let grid = GridFile::open(&grid_path)
        .map_err(|error| CliError::data(format!("open satellite grid: {error}")))?;
    if manifest.grid_hash != grid.hash {
        return Err(CliError::data(format!(
            "satellite run grid hash {} does not match grid.rwg {}",
            manifest.grid_hash, grid.hash
        )));
    }
    if manifest.nx != grid.nx || manifest.ny != grid.ny {
        return Err(CliError::data(format!(
            "satellite run declares {}x{}, but grid.rwg is {}x{}",
            manifest.nx, manifest.ny, grid.nx, grid.ny
        )));
    }
    Ok(OpenSatelliteRun {
        directory,
        model,
        run,
        manifest,
        grid,
    })
}

fn inspect_frame(
    opened: &OpenSatelliteRun,
    hhmm: u16,
    declared_file: &str,
) -> SatelliteFrameInspection {
    let mut frame = SatelliteFrameInspection {
        hhmm,
        file: declared_file.to_owned(),
        bytes: 0,
        readable: false,
        nx: 0,
        ny: 0,
        grid_hash: String::new(),
        raster_kind: SatelliteRasterKind::ObservedScalar,
        provider: None,
        satellite: None,
        instrument: None,
        product: None,
        sector: None,
        scan_start_utc: None,
        scan_end_utc: None,
        variables: Vec::new(),
        warnings: Vec::new(),
        failures: Vec::new(),
    };
    if !valid_hhmm(hhmm) {
        frame
            .failures
            .push(format!("frame key {hhmm} is not a valid UTC HHMM value"));
    }
    let expected_file = frame_file_name(hhmm);
    if declared_file != expected_file {
        frame.failures.push(format!(
            "frame {hhmm:04} declares '{declared_file}' instead of '{expected_file}'"
        ));
        // Do not follow a malformed manifest entry outside the run directory.
        // Canonical satellite runs always use the exact tHHMM.rws name.
        return frame;
    }
    let path = opened.directory.join(declared_file);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => frame
            .failures
            .push("frame file is a symbolic link".to_owned()),
        Ok(metadata) if !metadata.is_file() => frame
            .failures
            .push("declared frame is not a regular file".to_owned()),
        Ok(metadata) => frame.bytes = metadata.len(),
        Err(error) => frame
            .failures
            .push(format!("cannot stat declared frame: {error}")),
    }
    let reader = match HourReader::open(&path) {
        Ok(reader) => reader,
        Err(error) => {
            frame
                .failures
                .push(format!("cannot open satellite frame: {error}"));
            return frame;
        }
    };
    let meta = reader.meta();
    frame.nx = meta.nx;
    frame.ny = meta.ny;
    frame.grid_hash.clone_from(&meta.grid_hash);
    for (label, matches) in [
        ("model", meta.model == opened.model),
        ("run", meta.run == opened.run),
        ("frame key", meta.forecast_hour == hhmm),
        ("width", meta.nx == opened.grid.nx),
        ("height", meta.ny == opened.grid.ny),
        ("grid hash", meta.grid_hash == opened.grid.hash),
    ] {
        if !matches {
            frame.failures.push(format!(
                "frame {label} disagrees with its run/grid identity"
            ));
        }
    }
    let declared_variables = opened.manifest.hours[&hhmm]
        .variables
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let stored_variables = meta
        .variables
        .iter()
        .map(|variable| variable.name.clone())
        .collect::<BTreeSet<_>>();
    if declared_variables != stored_variables {
        frame.failures.push(format!(
            "run manifest variables {:?} disagree with frame variables {:?}",
            declared_variables, stored_variables
        ));
    }
    let selector = meta
        .variables
        .iter()
        .find_map(|variable| variable.selector.get("satellite"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let identity = selector_identity(&selector, &mut frame.warnings);
    frame.provider = identity.provider;
    frame.satellite = identity.satellite;
    frame.instrument = identity.instrument;
    frame.product = identity.product;
    frame.sector = identity.sector;
    frame.scan_start_utc = identity.scan_start_utc;
    frame.scan_end_utc = identity.scan_end_utc;
    frame.raster_kind = raster_kind(&opened.model, &stored_variables);
    for variable in &meta.variables {
        let (finite_minimum, finite_maximum, finite_count, missing_count) =
            if variable.kind == "surface2d" {
                match reader.stats_2d(&variable.name) {
                    Ok(stats) => (
                        stats.finite_min,
                        stats.finite_max,
                        stats.finite_count,
                        stats.missing_count,
                    ),
                    Err(error) => {
                        frame.failures.push(format!(
                            "cannot inspect statistics for {}: {error}",
                            variable.name
                        ));
                        (None, None, 0, 0)
                    }
                }
            } else {
                frame.failures.push(format!(
                    "satellite variable {} has unsupported kind '{}'",
                    variable.name, variable.kind
                ));
                (None, None, 0, 0)
            };
        frame.variables.push(SatelliteVariableInspection {
            name: variable.name.clone(),
            units: variable.units.clone(),
            kind: variable.kind.clone(),
            selector: variable.selector.clone(),
            finite_minimum,
            finite_maximum,
            finite_count,
            missing_count,
        });
    }
    if frame.scan_start_utc.is_none() {
        frame.warnings.push(
            "frame has no valid satellite.scan_start_utc; HHMM is only a storage key".to_owned(),
        );
    }
    frame.readable = frame.failures.is_empty();
    frame
}

fn selector_identity(
    satellite: &serde_json::Value,
    warnings: &mut Vec<String>,
) -> SelectorIdentity {
    let get = |key: &str| {
        satellite
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let normalize_time = |label: &str, value: Option<String>, warnings: &mut Vec<String>| {
        value.and_then(|value| match DateTime::parse_from_rfc3339(&value) {
            Ok(time) => Some(
                time.with_timezone(&Utc)
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            ),
            Err(error) => {
                warnings.push(format!("satellite.{label} '{value}' is invalid: {error}"));
                None
            }
        })
    };
    SelectorIdentity {
        provider: get("provider"),
        satellite: get("satellite"),
        instrument: get("instrument"),
        product: get("product"),
        sector: get("sector"),
        scan_start_utc: normalize_time("scan_start_utc", get("scan_start_utc"), warnings),
        scan_end_utc: normalize_time("scan_end_utc", get("scan_end_utc"), warnings),
    }
}

fn raster_kind(model: &str, variables: &BTreeSet<String>) -> SatelliteRasterKind {
    let rgb = ["rgb_r", "rgb_g", "rgb_b"]
        .into_iter()
        .all(|variable| variables.contains(variable));
    match (model == "simsat", rgb) {
        (true, true) => SatelliteRasterKind::SimulatedBakedRgb,
        (true, false) => SatelliteRasterKind::SimulatedScalar,
        (false, true) => SatelliteRasterKind::ObservedBakedRgb,
        (false, false) => SatelliteRasterKind::ObservedScalar,
    }
}

fn valid_hhmm(value: u16) -> bool {
    value <= 2359 && value % 100 < 60
}

fn refuse_symlink_file(path: &Path, label: &str) -> Result<(), CliError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        CliError::input(format!(
            "cannot inspect {label} {}: {error}",
            path.display()
        ))
    })?;
    if metadata.file_type().is_symlink() {
        return Err(CliError::input(format!(
            "refusing symlink {label} {}",
            path.display()
        )));
    }
    if !metadata.is_file() {
        return Err(CliError::input(format!(
            "{label} is not a regular file: {}",
            path.display()
        )));
    }
    Ok(())
}

fn build_identity(context: &RuntimeContext) -> SatelliteBuildIdentity {
    SatelliteBuildIdentity {
        version: context.bowecho_version.clone(),
        commit: context.bowecho_commit.clone(),
    }
}

pub(crate) fn execute_render(
    options: &SatelliteRenderOptions,
    context: &RuntimeContext,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<ExitCode, CliError> {
    let output = render(options, context, stderr)?;
    if options.json {
        serde_json::to_writer(&mut *stdout, &output.manifest).map_err(|error| {
            CliError::internal(format!("serialize satellite artifact manifest: {error}"))
        })?;
        writeln!(stdout).map_err(|error| {
            CliError::internal(format!("write satellite artifact manifest: {error}"))
        })?;
    }
    writeln!(
        stderr,
        "BowEcho satellite render: {} product(s), manifest {}",
        output.manifest.products.len(),
        output.manifest_path.display()
    )
    .map_err(|error| CliError::internal(format!("write satellite render status: {error}")))?;
    Ok(ExitCode::Success)
}

pub(crate) fn render(
    options: &SatelliteRenderOptions,
    context: &RuntimeContext,
    stderr: &mut dyn Write,
) -> Result<SatelliteRenderOutput, CliError> {
    if !(64..=4_096).contains(&options.width) || !(64..=4_096).contains(&options.height) {
        return Err(CliError::usage(
            "satellite render width and height must each be from 64 through 4096",
            bowecho_cli::satellite_help_text(),
        ));
    }
    let enhancement = parse_ir_enhancement(&options.ir_enhancement)?;
    let opened = open_run(&options.run_directory)?;
    let selected = select_frames(&opened.manifest, &options.frames)?;
    let output_root = prepare_output_root(&opened.directory, &options.output_directory)?;

    let mut inspections = Vec::with_capacity(selected.len());
    for &hhmm in &selected {
        let entry = &opened.manifest.hours[&hhmm];
        let inspected = inspect_frame(&opened, hhmm, &entry.file);
        if !inspected.failures.is_empty() {
            return Err(CliError::data(format!(
                "satellite frame {hhmm:04} failed inspection: {}",
                inspected.failures.join("; ")
            )));
        }
        if inspected.scan_start_utc.is_none() {
            return Err(CliError::data(format!(
                "satellite frame {hhmm:04} has no valid satellite.scan_start_utc; exact time is required for a verification artifact"
            )));
        }
        inspections.push(inspected);
    }
    let ir_enhancement_identity = if inspections.iter().all(|frame| frame.raster_kind.is_rgb()) {
        "not_applicable"
    } else {
        enhancement.slug()
    };

    // `run.json` is an active catalog and can legitimately gain later frames
    // after this artifact is created. The immutable verification sources are
    // the selected frame files plus their content-addressed grid.
    let mut sources = vec![source_receipt(
        SatelliteSourceRole::Grid,
        &opened.directory.join("grid.rwg"),
        None,
    )?];
    for &hhmm in &selected {
        sources.push(source_receipt(
            SatelliteSourceRole::Frame,
            &opened.directory.join(&opened.manifest.hours[&hhmm].file),
            Some(hhmm),
        )?);
    }
    let bowecho = build_identity(context);
    let processing_request = SatelliteProcessingRequest {
        schema_version: SATELLITE_ARTIFACT_SCHEMA_VERSION,
        bowecho: &bowecho,
        rusty_weather_commit: RUSTY_WEATHER_COMMIT,
        model: &opened.model,
        run: &opened.run,
        ir_enhancement: ir_enhancement_identity,
        width: options.width,
        height: options.height,
    };
    let processing_identity = processing_identity(&processing_request, &sources)?;
    let artifact_subdirectory = PathBuf::from("satellite")
        .join(&opened.model)
        .join(&opened.run)
        .join(format!(
            "{}x{}_{}-{}",
            options.width,
            options.height,
            ir_enhancement_identity,
            &processing_identity[..12]
        ));
    let artifact_directory = output_root.join(&artifact_subdirectory);
    prepare_output_directory(
        &output_root,
        &artifact_directory,
        "satellite artifact directory",
    )?;
    let staging = tempfile::Builder::new()
        .prefix(".bowecho-satellite-")
        .tempdir_in(&artifact_directory)
        .map_err(|error| {
            CliError::input(format!(
                "create satellite render staging directory under {}: {error}",
                artifact_directory.display()
            ))
        })?;

    let mut products = Vec::with_capacity(selected.len());
    let mut staged = Vec::with_capacity(selected.len());
    for (&hhmm, inspected) in selected.iter().zip(&inspections) {
        writeln!(
            stderr,
            "Rendering satellite {}/{} t{hhmm:04} through the production native plotter...",
            opened.model, opened.run
        )
        .map_err(|error| CliError::internal(format!("write satellite progress: {error}")))?;
        let source = crate::sat_worker::load_frame_for_cli(&opened.directory, hhmm, enhancement)
            .map_err(|error| {
                CliError::data(format!("load satellite frame {hhmm:04} for plot: {error}"))
            })?;
        let (variable, units, selector, statistics) = plot_receipt_data(&source, inspected)?;
        let file_name = format!("t{hhmm:04}-{}.png", safe_artifact_token(&variable));
        let staged_path = staging.path().join(&file_name);
        source
            .save_png(&staged_path, options.width, options.height)
            .map_err(|error| {
                CliError::data(format!("render satellite frame {hhmm:04}: {error}"))
            })?;
        let hash = bowecho_cli::fs::sha256_file(&staged_path).map_err(|error| {
            CliError::input(format!(
                "hash staged satellite artifact {}: {error}",
                staged_path.display()
            ))
        })?;
        let relative_path = artifact_subdirectory.join(&file_name);
        products.push(SatelliteProductReceipt {
            hhmm,
            variable,
            units,
            raster_kind: inspected.raster_kind,
            provider: inspected.provider.clone(),
            satellite: inspected.satellite.clone(),
            instrument: inspected.instrument.clone(),
            product: inspected.product.clone(),
            sector: inspected.sector.clone(),
            scan_start_utc: inspected.scan_start_utc.clone().expect("checked above"),
            scan_end_utc: inspected.scan_end_utc.clone(),
            grid_hash: source.grid.hash.clone(),
            nx: source.nx,
            ny: source.ny,
            selector,
            statistics,
            artifact: SatelliteArtifactReceipt {
                relative_path: relative_path.clone(),
                mime_type: "image/png".to_owned(),
                width: options.width,
                height: options.height,
                bytes: hash.bytes,
                sha256: hash.sha256,
            },
        });
        staged.push((staged_path, output_root.join(relative_path)));
    }

    verify_sources_unchanged(&sources)?;
    for (staged_path, destination) in &staged {
        prepare_output_destination(&output_root, destination, "satellite artifact destination")?;
        bowecho_cli::fs::publish_file_atomic(staged_path, destination).map_err(|error| {
            CliError::input(format!(
                "publish satellite artifact {}: {error}",
                destination.display()
            ))
        })?;
    }
    // Publication is a separate copy-and-rename from the staged file. Rehash
    // every final path only after all atomic publications have completed so a
    // manifest can never attest to bytes that were merely present in staging.
    for ((_, destination), product) in staged.iter().zip(&products) {
        verify_published_artifact(
            &output_root,
            destination,
            product.artifact.bytes,
            &product.artifact.sha256,
        )?;
    }
    let warnings = inspections
        .iter()
        .flat_map(|frame| {
            frame
                .warnings
                .iter()
                .map(move |warning| format!("t{:04}: {warning}", frame.hhmm))
        })
        .collect();
    let manifest = SatelliteArtifactManifest {
        schema_version: SATELLITE_ARTIFACT_SCHEMA_VERSION.to_owned(),
        status: SatelliteStatus::Complete,
        bowecho,
        rusty_weather_commit: RUSTY_WEATHER_COMMIT.to_owned(),
        processing_identity,
        model: opened.model,
        run: opened.run,
        ir_enhancement: ir_enhancement_identity.to_owned(),
        width: options.width,
        height: options.height,
        sources,
        products,
        warnings,
        failures: Vec::new(),
    };
    let failures = validate_artifact_contract(&manifest);
    if !failures.is_empty() {
        return Err(CliError::internal(format!(
            "satellite artifact manifest violates its contract: {}",
            failures.join("; ")
        )));
    }
    let manifest_path = output_root.join("satellite-artifact-manifest.json");
    prepare_output_destination(
        &output_root,
        &manifest_path,
        "satellite artifact manifest destination",
    )?;
    bowecho_cli::fs::write_json_atomic(&manifest_path, &manifest).map_err(|error| {
        CliError::input(format!(
            "publish satellite artifact manifest {}: {error}",
            manifest_path.display()
        ))
    })?;
    prepare_output_destination(
        &output_root,
        &manifest_path,
        "published satellite artifact manifest",
    )?;
    Ok(SatelliteRenderOutput {
        manifest,
        manifest_path,
    })
}

fn parse_ir_enhancement(value: &str) -> Result<IrEnhancement, CliError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "natural" => Ok(IrEnhancement::Natural),
        "cimss" => Ok(IrEnhancement::Cimss),
        "bd" => Ok(IrEnhancement::Bd),
        "avn" => Ok(IrEnhancement::Avn),
        "funktop" => Ok(IrEnhancement::Funktop),
        "rainbow" => Ok(IrEnhancement::Rainbow),
        "gray" => Ok(IrEnhancement::Grayscale),
        other => Err(CliError::usage(
            format!(
                "unsupported satellite IR enhancement '{other}'; expected natural, cimss, bd, avn, funktop, rainbow, or gray"
            ),
            bowecho_cli::satellite_help_text(),
        )),
    }
}

fn select_frames(
    manifest: &RwsRunManifest,
    selection: &SatelliteFrameSelection,
) -> Result<Vec<u16>, CliError> {
    if manifest.hours.is_empty() {
        return Err(CliError::data("satellite run declares no frames"));
    }
    let mut selected = match selection {
        SatelliteFrameSelection::Latest => vec![*manifest.hours.last_key_value().unwrap().0],
        SatelliteFrameSelection::All => manifest.hours.keys().copied().collect(),
        SatelliteFrameSelection::Explicit(frames) => frames.clone(),
    };
    selected.sort_unstable();
    selected.dedup();
    if selected.is_empty() {
        return Err(CliError::usage(
            "satellite render selected no frames",
            bowecho_cli::satellite_help_text(),
        ));
    }
    for hhmm in &selected {
        if !valid_hhmm(*hhmm) {
            return Err(CliError::usage(
                format!("satellite frame {hhmm} is not a valid HHMM value"),
                bowecho_cli::satellite_help_text(),
            ));
        }
        if !manifest.hours.contains_key(hhmm) {
            return Err(CliError::input(format!(
                "satellite run has no t{hhmm:04} frame; available: {}",
                manifest
                    .hours
                    .keys()
                    .map(|frame| format!("{frame:04}"))
                    .collect::<Vec<_>>()
                    .join(",")
            )));
        }
    }
    Ok(selected)
}

fn source_receipt(
    role: SatelliteSourceRole,
    path: &Path,
    hhmm: Option<u16>,
) -> Result<SatelliteSourceReceipt, CliError> {
    refuse_symlink_file(path, "satellite source")?;
    let path = fs::canonicalize(path).map_err(|error| {
        CliError::input(format!(
            "resolve satellite source {}: {error}",
            path.display()
        ))
    })?;
    let hash = bowecho_cli::fs::sha256_file(&path).map_err(|error| {
        CliError::input(format!("hash satellite source {}: {error}", path.display()))
    })?;
    Ok(SatelliteSourceReceipt {
        role,
        path,
        bytes: hash.bytes,
        sha256: hash.sha256,
        hhmm,
    })
}

fn verify_sources_unchanged(sources: &[SatelliteSourceReceipt]) -> Result<(), CliError> {
    for source in sources {
        let hash = bowecho_cli::fs::sha256_file(&source.path).map_err(|error| {
            CliError::input(format!(
                "rehash satellite source {}: {error}",
                source.path.display()
            ))
        })?;
        if hash.bytes != source.bytes || hash.sha256 != source.sha256 {
            return Err(CliError::data(format!(
                "satellite source changed while rendering: {}",
                source.path.display()
            )));
        }
    }
    Ok(())
}

fn processing_identity(
    request: &SatelliteProcessingRequest<'_>,
    sources: &[SatelliteSourceReceipt],
) -> Result<String, CliError> {
    let frames = sources
        .iter()
        .filter(|source| source.role == SatelliteSourceRole::Frame)
        .filter_map(|source| source.hhmm)
        .collect::<Vec<_>>();
    let value = serde_json::json!({
        "schema": request.schema_version,
        "model": request.model,
        "run": request.run,
        "frames": frames,
        "ir_enhancement": request.ir_enhancement,
        "width": request.width,
        "height": request.height,
        "bowecho_version": request.bowecho.version.as_str(),
        "bowecho_commit": request.bowecho.commit.as_str(),
        "rusty_weather_commit": request.rusty_weather_commit,
        "sources": sources.iter().map(|source| serde_json::json!({
            "role": source.role,
            "hhmm": source.hhmm,
            "bytes": source.bytes,
            "sha256": source.sha256,
        })).collect::<Vec<_>>(),
    });
    let bytes = serde_json::to_vec(&value).map_err(|error| {
        CliError::internal(format!("serialize satellite processing identity: {error}"))
    })?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn manifest_processing_identity(manifest: &SatelliteArtifactManifest) -> Result<String, CliError> {
    processing_identity(
        &SatelliteProcessingRequest::from_manifest(manifest),
        &manifest.sources,
    )
}

fn plot_receipt_data(
    source: &SatellitePlotSource,
    inspected: &SatelliteFrameInspection,
) -> Result<(String, String, serde_json::Value, SatelliteFiniteStatistics), CliError> {
    match &source.raster {
        SatellitePlotRaster::Scalar {
            variable,
            units,
            selector,
            values,
            ..
        } => {
            let mut minimum: Option<f32> = None;
            let mut maximum: Option<f32> = None;
            let mut finite_count = 0_u64;
            for value in values.iter().copied().filter(|value| value.is_finite()) {
                minimum = Some(minimum.map_or(value, |current| current.min(value)));
                maximum = Some(maximum.map_or(value, |current| current.max(value)));
                finite_count += 1;
            }
            Ok((
                variable.clone(),
                units.clone(),
                selector.clone(),
                SatelliteFiniteStatistics {
                    minimum,
                    maximum,
                    finite_count,
                    missing_count: values.len() as u64 - finite_count,
                },
            ))
        }
        SatellitePlotRaster::Rgba { .. } => {
            let selector = inspected
                .variables
                .iter()
                .find(|variable| variable.selector.get("satellite").is_some())
                .map(|variable| variable.selector.clone())
                .unwrap_or(serde_json::Value::Null);
            Ok((
                inspected
                    .product
                    .clone()
                    .unwrap_or_else(|| "rgb_composite".to_owned()),
                "rgba8".to_owned(),
                selector,
                SatelliteFiniteStatistics::default(),
            ))
        }
    }
}

fn prepare_output_root(run_directory: &Path, output: &Path) -> Result<PathBuf, CliError> {
    let output = absolute_output_path(output, "satellite output root")?;
    reject_linked_path_components(&output, "satellite output root")?;

    // Check the nearest existing prefix before creating anything. This keeps
    // a nonexistent `run/new-output` from modifying the source tree and also
    // catches case/short-name aliases on platforms whose paths are not a
    // one-to-one lexical representation of filesystem identity.
    let existing = canonical_existing_ancestor(&output, "satellite output root")?;
    if existing.starts_with(run_directory) {
        return Err(CliError::input(format!(
            "satellite output {} must not be inside the canonical source run {}",
            output.display(),
            run_directory.display()
        )));
    }

    fs::create_dir_all(&output).map_err(|error| {
        CliError::input(format!(
            "create satellite output root {}: {error}",
            output.display()
        ))
    })?;
    reject_linked_path_components(&output, "satellite output root")?;
    let metadata = fs::symlink_metadata(&output).map_err(|error| {
        CliError::input(format!(
            "inspect satellite output root {}: {error}",
            output.display()
        ))
    })?;
    if is_link_or_reparse(&metadata) || !metadata.is_dir() {
        return Err(CliError::input(format!(
            "satellite output root must be a non-linked directory: {}",
            output.display()
        )));
    }
    let canonical = fs::canonicalize(&output).map_err(|error| {
        CliError::input(format!(
            "canonicalize satellite output root {}: {error}",
            output.display()
        ))
    })?;
    if canonical.starts_with(run_directory) {
        return Err(CliError::input(format!(
            "satellite output {} resolves inside the canonical source run {}",
            canonical.display(),
            run_directory.display()
        )));
    }
    Ok(canonical)
}

fn prepare_output_directory(
    output_root: &Path,
    directory: &Path,
    label: &str,
) -> Result<PathBuf, CliError> {
    require_stable_output_root(output_root)?;
    let directory = absolute_output_path(directory, label)?;
    if !directory.starts_with(output_root) {
        return Err(CliError::input(format!(
            "{label} escapes satellite output root {}: {}",
            output_root.display(),
            directory.display()
        )));
    }
    reject_linked_path_components(&directory, label)?;
    fs::create_dir_all(&directory).map_err(|error| {
        CliError::input(format!("create {label} {}: {error}", directory.display()))
    })?;
    reject_linked_path_components(&directory, label)?;
    let metadata = fs::symlink_metadata(&directory).map_err(|error| {
        CliError::input(format!("inspect {label} {}: {error}", directory.display()))
    })?;
    if is_link_or_reparse(&metadata) || !metadata.is_dir() {
        return Err(CliError::input(format!(
            "{label} must be a non-linked directory: {}",
            directory.display()
        )));
    }
    let canonical = fs::canonicalize(&directory).map_err(|error| {
        CliError::input(format!(
            "canonicalize {label} {}: {error}",
            directory.display()
        ))
    })?;
    if !canonical.starts_with(output_root) {
        return Err(CliError::input(format!(
            "{label} resolves outside satellite output root {}: {}",
            output_root.display(),
            canonical.display()
        )));
    }
    Ok(canonical)
}

fn prepare_output_destination(
    output_root: &Path,
    destination: &Path,
    label: &str,
) -> Result<(), CliError> {
    let destination = absolute_output_path(destination, label)?;
    if !destination.starts_with(output_root) {
        return Err(CliError::input(format!(
            "{label} escapes satellite output root {}: {}",
            output_root.display(),
            destination.display()
        )));
    }
    let parent = destination.parent().ok_or_else(|| {
        CliError::input(format!("{label} has no parent: {}", destination.display()))
    })?;
    let canonical_parent = prepare_output_directory(output_root, parent, label)?;
    let file_name = destination.file_name().ok_or_else(|| {
        CliError::input(format!(
            "{label} has no file name: {}",
            destination.display()
        ))
    })?;
    if canonical_parent.join(file_name) != destination {
        return Err(CliError::input(format!(
            "{label} aliases a different filesystem destination: {}",
            destination.display()
        )));
    }
    match fs::symlink_metadata(&destination) {
        Ok(metadata) if is_link_or_reparse(&metadata) => Err(CliError::input(format!(
            "refusing linked/reparse {label} {}",
            destination.display()
        ))),
        Ok(metadata) if !metadata.is_file() => Err(CliError::input(format!(
            "{label} is not a regular file: {}",
            destination.display()
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(CliError::input(format!(
            "inspect {label} {}: {error}",
            destination.display()
        ))),
    }
}

fn verify_published_artifact(
    output_root: &Path,
    destination: &Path,
    expected_bytes: u64,
    expected_sha256: &str,
) -> Result<(), CliError> {
    prepare_output_destination(output_root, destination, "published satellite artifact")?;
    let canonical = fs::canonicalize(destination).map_err(|error| {
        CliError::input(format!(
            "canonicalize published satellite artifact {}: {error}",
            destination.display()
        ))
    })?;
    if !canonical.starts_with(output_root) {
        return Err(CliError::input(format!(
            "published satellite artifact resolves outside output root {}: {}",
            output_root.display(),
            canonical.display()
        )));
    }
    let hash = bowecho_cli::fs::sha256_file(&canonical).map_err(|error| {
        CliError::input(format!(
            "rehash published satellite artifact {}: {error}",
            canonical.display()
        ))
    })?;
    if hash.bytes != expected_bytes || hash.sha256 != expected_sha256 {
        return Err(CliError::data(format!(
            "published satellite artifact differs from its staged receipt: expected {expected_bytes} bytes {expected_sha256}, got {} bytes {} at {}",
            hash.bytes,
            hash.sha256,
            canonical.display()
        )));
    }
    Ok(())
}

fn require_stable_output_root(output_root: &Path) -> Result<(), CliError> {
    reject_linked_path_components(output_root, "satellite output root")?;
    let canonical = fs::canonicalize(output_root).map_err(|error| {
        CliError::input(format!(
            "canonicalize satellite output root {}: {error}",
            output_root.display()
        ))
    })?;
    if canonical != output_root {
        return Err(CliError::input(format!(
            "satellite output root changed or aliases another path: {} resolves to {}",
            output_root.display(),
            canonical.display()
        )));
    }
    Ok(())
}

fn canonical_existing_ancestor(path: &Path, label: &str) -> Result<PathBuf, CliError> {
    for ancestor in path.ancestors().filter(|path| !path.as_os_str().is_empty()) {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) => {
                if is_link_or_reparse(&metadata) || !metadata.is_dir() {
                    return Err(CliError::input(format!(
                        "{label} crosses a non-directory or linked/reparse path: {}",
                        ancestor.display()
                    )));
                }
                return fs::canonicalize(ancestor).map_err(|error| {
                    CliError::input(format!(
                        "canonicalize existing {label} ancestor {}: {error}",
                        ancestor.display()
                    ))
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(CliError::input(format!(
                    "inspect {label} ancestor {}: {error}",
                    ancestor.display()
                )));
            }
        }
    }
    Err(CliError::input(format!(
        "{label} has no existing filesystem ancestor: {}",
        path.display()
    )))
}

fn reject_linked_path_components(path: &Path, label: &str) -> Result<(), CliError> {
    let mut ancestors = path
        .ancestors()
        .filter(|path| !path.as_os_str().is_empty())
        .collect::<Vec<_>>();
    ancestors.reverse();
    for ancestor in ancestors {
        let metadata = match fs::symlink_metadata(ancestor) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(CliError::input(format!(
                    "inspect {label} component {}: {error}",
                    ancestor.display()
                )));
            }
        };
        if is_link_or_reparse(&metadata) {
            return Err(CliError::input(format!(
                "refusing {label} through a link/reparse point: {}",
                ancestor.display()
            )));
        }
    }
    Ok(())
}

fn absolute_output_path(path: &Path, label: &str) -> Result<PathBuf, CliError> {
    if path.as_os_str().is_empty() {
        return Err(CliError::input(format!("{label} is empty")));
    }
    // `canonicalize` returns a verbatim (`\\?\`) path on Windows. Rust keeps
    // `..` as a `Normal` component inside verbatim paths, so check both forms
    // instead of relying only on `Component::ParentDir`.
    if path.components().any(|component| {
        component == Component::ParentDir
            || matches!(component, Component::Normal(segment) if segment == "..")
    }) {
        return Err(CliError::input(format!(
            "{label} contains parent traversal: {}",
            path.display()
        )));
    }
    std::path::absolute(path)
        .map_err(|error| CliError::input(format!("resolve {label} {}: {error}", path.display())))
}

fn is_link_or_reparse(metadata: &Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn safe_artifact_token(value: &str) -> String {
    let token = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    let token = token.trim_matches('_');
    if token.is_empty() {
        "satellite_product".to_owned()
    } else {
        token.to_owned()
    }
}

pub(crate) fn execute_verify(
    manifest_path: &Path,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<ExitCode, CliError> {
    let (report, code) = verify(manifest_path)?;
    serde_json::to_writer_pretty(&mut *stdout, &report).map_err(|error| {
        CliError::internal(format!("serialize satellite verification report: {error}"))
    })?;
    writeln!(stdout).map_err(|error| {
        CliError::internal(format!("write satellite verification report: {error}"))
    })?;
    writeln!(
        stderr,
        "BowEcho satellite verify: {} receipt(s), verified={}",
        report.receipts.len(),
        report.verified
    )
    .map_err(|error| CliError::internal(format!("write satellite verify status: {error}")))?;
    Ok(code)
}

pub(crate) fn verify(manifest_path: &Path) -> Result<(SatelliteVerifyReport, ExitCode), CliError> {
    let metadata = fs::symlink_metadata(manifest_path).map_err(|error| {
        CliError::input(format!(
            "cannot inspect satellite artifact manifest {}: {error}",
            manifest_path.display()
        ))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(CliError::input(format!(
            "satellite artifact manifest must be a regular non-symlink file: {}",
            manifest_path.display()
        )));
    }
    if metadata.len() > MAX_MANIFEST_BYTES {
        return Err(CliError::data(format!(
            "satellite artifact manifest is {} bytes; maximum is {MAX_MANIFEST_BYTES}",
            metadata.len()
        )));
    }
    let bytes = fs::read(manifest_path).map_err(|error| {
        CliError::input(format!(
            "cannot read satellite artifact manifest {}: {error}",
            manifest_path.display()
        ))
    })?;
    let manifest: SatelliteArtifactManifest = serde_json::from_slice(&bytes).map_err(|error| {
        CliError::data(format!(
            "invalid satellite artifact manifest {}: {error}",
            manifest_path.display()
        ))
    })?;
    let manifest_hash = bowecho_cli::fs::sha256_reader(&bytes[..]).map_err(|error| {
        CliError::input(format!(
            "cannot hash satellite artifact manifest {}: {error}",
            manifest_path.display()
        ))
    })?;
    let mut contract_failures = validate_artifact_contract(&manifest);
    match manifest_processing_identity(&manifest) {
        Ok(expected) if manifest.processing_identity != expected => {
            contract_failures.push(format!(
                "processing_identity mismatch: manifest declares {}, but its request and source receipts recompute to {expected}",
                manifest.processing_identity
            ));
        }
        Ok(_) => {}
        Err(error) => contract_failures.push(format!(
            "cannot recompute processing_identity from manifest request and source receipts: {error}"
        )),
    }
    let processing_complete = manifest.status == SatelliteStatus::Complete
        && !manifest.products.is_empty()
        && manifest.failures.is_empty();
    if !processing_complete {
        contract_failures.push(
            "only a complete, non-empty satellite manifest without failures can verify".to_owned(),
        );
    }
    let manifest_directory = manifest_path.parent().unwrap_or_else(|| Path::new("."));
    let mut receipts = Vec::with_capacity(manifest.sources.len() + manifest.products.len());
    for source in &manifest.sources {
        receipts.push(verify_receipt(
            SatelliteVerificationKind::Source,
            format!("{:?}", source.role),
            &source.path,
            source.bytes,
            &source.sha256,
            manifest_directory,
            true,
        ));
    }
    for product in &manifest.products {
        let mut receipt = verify_receipt(
            SatelliteVerificationKind::Artifact,
            format!("t{:04}/{}", product.hhmm, product.variable),
            &product.artifact.relative_path,
            product.artifact.bytes,
            &product.artifact.sha256,
            manifest_directory,
            false,
        );
        verify_png_dimensions(
            &mut receipt,
            product.artifact.width,
            product.artifact.height,
            &product.artifact.mime_type,
        );
        receipts.push(receipt);
    }
    let verified = contract_failures.is_empty() && receipts.iter().all(|receipt| receipt.verified);
    let report = SatelliteVerifyReport {
        schema_version: SATELLITE_VERIFY_SCHEMA_VERSION.to_owned(),
        artifact_schema_version: manifest.schema_version,
        manifest_path: manifest_path.to_path_buf(),
        manifest_bytes: manifest_hash.bytes,
        manifest_sha256: manifest_hash.sha256,
        processing_complete,
        verified,
        contract_failures,
        receipts,
    };
    let code = if verified {
        ExitCode::Success
    } else {
        ExitCode::Verification
    };
    Ok((report, code))
}

fn validate_artifact_contract(manifest: &SatelliteArtifactManifest) -> Vec<String> {
    let mut failures = Vec::new();
    if manifest.schema_version != SATELLITE_ARTIFACT_SCHEMA_VERSION {
        failures.push(format!(
            "unsupported schema_version '{}'; expected '{SATELLITE_ARTIFACT_SCHEMA_VERSION}'",
            manifest.schema_version
        ));
    }
    for (label, value) in [
        ("bowecho.version", manifest.bowecho.version.as_str()),
        ("bowecho.commit", manifest.bowecho.commit.as_str()),
        (
            "rusty_weather_commit",
            manifest.rusty_weather_commit.as_str(),
        ),
        ("processing_identity", manifest.processing_identity.as_str()),
        ("model", manifest.model.as_str()),
        ("run", manifest.run.as_str()),
        ("ir_enhancement", manifest.ir_enhancement.as_str()),
    ] {
        if value.trim().is_empty() {
            failures.push(format!("required field {label} is empty"));
        }
    }
    if !valid_sha256(&manifest.processing_identity) {
        failures.push("processing_identity is not a SHA-256 digest".to_owned());
    }
    if !(64..=4_096).contains(&manifest.width) || !(64..=4_096).contains(&manifest.height) {
        failures.push("render dimensions are outside 64..4096".to_owned());
    }
    if manifest.status == SatelliteStatus::Complete && !manifest.failures.is_empty() {
        failures.push("complete manifest contains failure records".to_owned());
    }
    if manifest.sources.is_empty() {
        failures.push("manifest declares no source files".to_owned());
    }
    if manifest.products.is_empty() && manifest.status == SatelliteStatus::Complete {
        failures.push("complete manifest declares no products".to_owned());
    }
    let mut source_paths = BTreeSet::new();
    let mut grids = 0usize;
    let mut source_frames = BTreeSet::new();
    for source in &manifest.sources {
        if !source.path.is_absolute() {
            failures.push(format!(
                "satellite source path must be absolute: {}",
                source.path.display()
            ));
        }
        if source.bytes == 0 {
            failures.push(format!(
                "satellite source {} declares zero bytes",
                source.path.display()
            ));
        }
        if !valid_sha256(&source.sha256) {
            failures.push(format!(
                "satellite source {} has invalid SHA-256",
                source.path.display()
            ));
        }
        if !source_paths.insert(source.path.clone()) {
            failures.push(format!(
                "duplicate satellite source receipt {}",
                source.path.display()
            ));
        }
        match source.role {
            SatelliteSourceRole::Grid => {
                grids += 1;
                if source.hhmm.is_some() {
                    failures.push("grid receipt unexpectedly has hhmm".to_owned());
                }
            }
            SatelliteSourceRole::Frame => match source.hhmm {
                Some(hhmm) if valid_hhmm(hhmm) => {
                    if !source_frames.insert(hhmm) {
                        failures.push(format!("duplicate frame source receipt t{hhmm:04}"));
                    }
                }
                _ => failures.push("frame source receipt has no valid hhmm".to_owned()),
            },
        }
    }
    if grids != 1 {
        failures.push(format!(
            "manifest requires exactly one immutable grid receipt; got {grids}"
        ));
    }
    let mut product_frames = BTreeSet::new();
    let mut artifact_paths = BTreeSet::new();
    for product in &manifest.products {
        if !valid_hhmm(product.hhmm) {
            failures.push(format!("product frame {} is not valid HHMM", product.hhmm));
        }
        if !product_frames.insert(product.hhmm) {
            failures.push(format!("duplicate satellite product t{:04}", product.hhmm));
        }
        if product.variable.trim().is_empty() || product.units.trim().is_empty() {
            failures.push(format!(
                "satellite product t{:04} has empty variable or units",
                product.hhmm
            ));
        }
        if !valid_sha256(&product.grid_hash) || product.nx == 0 || product.ny == 0 {
            failures.push(format!(
                "satellite product t{:04} has invalid grid identity",
                product.hhmm
            ));
        }
        if DateTime::parse_from_rfc3339(&product.scan_start_utc).is_err() {
            failures.push(format!(
                "satellite product t{:04} has invalid scan_start_utc",
                product.hhmm
            ));
        }
        if product
            .scan_end_utc
            .as_deref()
            .is_some_and(|time| DateTime::parse_from_rfc3339(time).is_err())
        {
            failures.push(format!(
                "satellite product t{:04} has invalid scan_end_utc",
                product.hhmm
            ));
        }
        let artifact = &product.artifact;
        if artifact.relative_path.is_absolute()
            || artifact.relative_path.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            failures.push(format!(
                "satellite artifact path must remain beneath its manifest: {}",
                artifact.relative_path.display()
            ));
        }
        if !artifact_paths.insert(artifact.relative_path.clone()) {
            failures.push(format!(
                "duplicate satellite artifact path {}",
                artifact.relative_path.display()
            ));
        }
        if artifact.mime_type != "image/png"
            || artifact.width != manifest.width
            || artifact.height != manifest.height
            || artifact.bytes == 0
            || !valid_sha256(&artifact.sha256)
        {
            failures.push(format!(
                "satellite product t{:04} has an invalid PNG receipt",
                product.hhmm
            ));
        }
        let statistics = &product.statistics;
        if product.raster_kind.is_rgb() {
            if statistics.minimum.is_some()
                || statistics.maximum.is_some()
                || statistics.finite_count != 0
                || statistics.missing_count != 0
            {
                failures.push(format!(
                    "RGB product t{:04} must not claim scalar statistics",
                    product.hhmm
                ));
            }
        } else {
            match (statistics.minimum, statistics.maximum) {
                (Some(minimum), Some(maximum))
                    if minimum.is_finite() && maximum.is_finite() && minimum <= maximum => {}
                (None, None) if statistics.finite_count == 0 => {}
                _ => failures.push(format!(
                    "scalar product t{:04} has inconsistent statistics",
                    product.hhmm
                )),
            }
        }
    }
    if source_frames != product_frames {
        failures.push(format!(
            "frame source receipts {:?} do not match products {:?}",
            source_frames, product_frames
        ));
    }
    failures
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[allow(clippy::too_many_arguments)]
fn verify_receipt(
    kind: SatelliteVerificationKind,
    label: String,
    declared_path: &Path,
    expected_bytes: u64,
    expected_sha256: &str,
    manifest_directory: &Path,
    allow_absolute: bool,
) -> SatelliteReceiptVerification {
    let mut receipt = SatelliteReceiptVerification {
        kind,
        label,
        declared_path: declared_path.to_path_buf(),
        resolved_path: None,
        expected_bytes,
        expected_sha256: expected_sha256.to_ascii_lowercase(),
        actual_bytes: None,
        actual_sha256: None,
        verified: false,
        failures: Vec::new(),
    };
    let path = match bowecho_cli::fs::resolve_receipt_path(
        manifest_directory,
        declared_path,
        allow_absolute,
    ) {
        Ok(path) => path,
        Err(error) => {
            receipt.failures.push(error);
            return receipt;
        }
    };
    receipt.resolved_path = Some(path.clone());
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) => {
            receipt
                .failures
                .push(format!("cannot inspect declared file: {error}"));
            return receipt;
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        receipt
            .failures
            .push("declared receipt is not a regular non-symlink file".to_owned());
        return receipt;
    }
    if !allow_absolute {
        match (
            fs::canonicalize(manifest_directory),
            fs::canonicalize(&path),
        ) {
            (Ok(root), Ok(resolved)) if resolved.starts_with(&root) => {}
            (Ok(_), Ok(resolved)) => {
                receipt.failures.push(format!(
                    "artifact resolves outside its manifest directory: {}",
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
    let before = fs::metadata(&path).ok();
    match bowecho_cli::fs::sha256_file(&path) {
        Ok(actual) => {
            receipt.actual_bytes = Some(actual.bytes);
            receipt.actual_sha256 = Some(actual.sha256.clone());
            if actual.bytes != expected_bytes {
                receipt.failures.push(format!(
                    "byte count mismatch: expected {expected_bytes}, got {}",
                    actual.bytes
                ));
            }
            if !actual.sha256.eq_ignore_ascii_case(expected_sha256) {
                receipt.failures.push(format!(
                    "SHA-256 mismatch: expected {expected_sha256}, got {}",
                    actual.sha256
                ));
            }
            if before
                .as_ref()
                .map(|metadata| (metadata.len(), metadata.modified().ok()))
                != fs::metadata(&path)
                    .ok()
                    .as_ref()
                    .map(|metadata| (metadata.len(), metadata.modified().ok()))
            {
                receipt
                    .failures
                    .push("file changed while its receipt was verified".to_owned());
            }
        }
        Err(error) => receipt
            .failures
            .push(format!("cannot hash {}: {error}", path.display())),
    }
    receipt.verified = receipt.failures.is_empty();
    receipt
}

fn verify_png_dimensions(
    receipt: &mut SatelliteReceiptVerification,
    expected_width: u32,
    expected_height: u32,
    mime_type: &str,
) {
    if !receipt.verified {
        return;
    }
    if mime_type != "image/png" {
        receipt.failures.push(format!(
            "unsupported satellite artifact MIME type {mime_type}"
        ));
        receipt.verified = false;
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
                    .push(format!("cannot inspect satellite PNG header: {error}"));
                receipt.verified = false;
                return;
            }
        };
    if reader.format() != Some(image::ImageFormat::Png) {
        receipt
            .failures
            .push("satellite artifact encoding is not PNG".to_owned());
        receipt.verified = false;
        return;
    }
    // `into_dimensions` only parses enough of the header to report IHDR. A
    // truncated or corrupt IDAT stream can pass that check, so verification
    // must force a complete pixel decode before accepting the artifact.
    match reader.decode() {
        Ok(decoded) if decoded.width() == expected_width && decoded.height() == expected_height => {
        }
        Ok(decoded) => {
            let width = decoded.width();
            let height = decoded.height();
            receipt.failures.push(format!(
                "satellite artifact dimensions mismatch: expected {expected_width}x{expected_height}, got {width}x{height}"
            ));
        }
        Err(error) => receipt
            .failures
            .push(format!("cannot fully decode satellite PNG: {error}")),
    }
    receipt.verified = receipt.failures.is_empty();
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn synthetic_run(root: &Path) -> PathBuf {
        let width = 8;
        let height = 6;
        let mut rgb = Vec::with_capacity(width * height * 3);
        for row in 0..height {
            for column in 0..width {
                rgb.extend_from_slice(&[
                    (column * 28) as u8,
                    (row * 38) as u8,
                    ((row + column) * 16) as u8,
                ]);
            }
        }
        let start = Utc.with_ymd_and_hms(2026, 7, 22, 18, 30, 0).unwrap();
        let written = crate::sat_rgb_store::write_regular_lonlat_rgb_frame(
            root,
            crate::sat_rgb_store::RegularLonLatRgb {
                width,
                height,
                bounds: crate::sat_rgb_store::LonLatBounds {
                    west_deg: -110.0,
                    south_deg: 25.0,
                    east_deg: -80.0,
                    north_deg: 50.0,
                },
                rgb: &rgb,
                alpha: None,
            },
            &crate::sat_rgb_store::RgbSatelliteMetadata {
                source_id: "test".to_owned(),
                provider: "test-provider".to_owned(),
                instrument: "test-imager".to_owned(),
                satellite: "TestSat".to_owned(),
                model: "test_sat".to_owned(),
                product_id: "true_colour".to_owned(),
                product_title: "True Colour".to_owned(),
                sector: "test-domain".to_owned(),
                scan_start_utc: start,
                scan_end_utc: start + chrono::Duration::minutes(10),
                extra_metadata: serde_json::Value::Null,
            },
            1_700_000_000,
        )
        .unwrap();
        root.join(written.model).join(written.run)
    }

    fn context() -> RuntimeContext {
        RuntimeContext::new("0.34.5-test", "abcdef")
    }

    #[test]
    fn inspect_validates_canonical_rgb_run_and_exact_scan_time() {
        let root = tempfile::tempdir().unwrap();
        let run = synthetic_run(root.path());
        let report = inspect_run(&run, &context()).unwrap();
        assert_eq!(report.status, SatelliteStatus::Complete);
        assert_eq!(report.model, "test_sat");
        assert_eq!(report.frames.len(), 1);
        assert_eq!(report.frames[0].hhmm, 1830);
        assert_eq!(
            report.frames[0].raster_kind,
            SatelliteRasterKind::ObservedBakedRgb
        );
        assert_eq!(
            report.frames[0].scan_start_utc.as_deref(),
            Some("2026-07-22T18:30:00Z")
        );
    }

    #[test]
    fn render_uses_native_plot_and_verifier_catches_tampering() {
        let root = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let run = synthetic_run(root.path());
        let options = SatelliteRenderOptions {
            run_directory: run,
            output_directory: output.path().to_path_buf(),
            frames: SatelliteFrameSelection::Explicit(vec![1830]),
            ir_enhancement: "cimss".to_owned(),
            width: 320,
            height: 240,
            json: true,
        };
        let rendered = render(&options, &context(), &mut Vec::new()).unwrap();
        assert_eq!(rendered.manifest.status, SatelliteStatus::Complete);
        assert_eq!(rendered.manifest.products.len(), 1);
        let (verified, code) = verify(&rendered.manifest_path).unwrap();
        assert_eq!(code, ExitCode::Success);
        assert!(verified.verified);

        // Live follow may append to run.json after this exact frame was
        // rendered. Only the immutable grid and selected frame are receipts.
        fs::write(
            options.run_directory.join("run.json"),
            b"live catalog changed after render",
        )
        .unwrap();
        let (verified, code) = verify(&rendered.manifest_path).unwrap();
        assert_eq!(code, ExitCode::Success);
        assert!(verified.verified);

        let artifact = rendered
            .manifest_path
            .parent()
            .unwrap()
            .join(&rendered.manifest.products[0].artifact.relative_path);
        fs::write(&artifact, b"not a PNG anymore").unwrap();
        let (tampered, code) = verify(&rendered.manifest_path).unwrap();
        assert_eq!(code, ExitCode::Verification);
        assert!(!tampered.verified);
    }

    #[test]
    fn output_root_cannot_be_created_beneath_canonical_source_run() {
        let source = tempfile::tempdir().unwrap();
        let run = source.path().join("run");
        fs::create_dir(&run).unwrap();
        let run = fs::canonicalize(run).unwrap();
        let output = run.join("new-output");

        let error = prepare_output_root(&run, &output).unwrap_err();
        assert!(error.message.contains("canonical source run"));
        assert!(!output.exists());
    }

    #[test]
    fn output_destination_rejects_parent_alias_escape() {
        let source = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let source = fs::canonicalize(source.path()).unwrap();
        let output_root = prepare_output_root(&source, output.path()).unwrap();
        let aliased = output_root.join("..").join("escape.png");

        let error =
            prepare_output_destination(&output_root, &aliased, "satellite artifact destination")
                .unwrap_err();
        assert!(
            error.message.contains("parent traversal")
                || error.message.contains("escapes satellite output root")
        );
    }

    #[cfg(unix)]
    #[test]
    fn output_destination_rejects_intermediate_symlink() {
        use std::os::unix::fs::symlink;

        let source = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let source = fs::canonicalize(source.path()).unwrap();
        let output_root = prepare_output_root(&source, output.path()).unwrap();
        symlink(outside.path(), output_root.join("satellite")).unwrap();
        let destination = output_root.join("satellite").join("escape.png");

        let error = prepare_output_destination(
            &output_root,
            &destination,
            "satellite artifact destination",
        )
        .unwrap_err();
        assert!(error.message.contains("link/reparse point"));
        assert!(!outside.path().join("escape.png").exists());
    }

    #[test]
    fn published_artifact_rehash_requires_exact_sha() {
        let source = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let source = fs::canonicalize(source.path()).unwrap();
        let output_root = prepare_output_root(&source, output.path()).unwrap();
        let destination = output_root.join("artifact.png");
        fs::write(&destination, b"expected").unwrap();
        let expected = bowecho_cli::fs::sha256_file(&destination).unwrap();
        fs::write(&destination, b"tampered").unwrap();

        let error =
            verify_published_artifact(&output_root, &destination, expected.bytes, &expected.sha256)
                .unwrap_err();
        assert!(error.message.contains("differs from its staged receipt"));
    }

    #[test]
    fn verifier_recomputes_processing_identity_from_manifest_receipts() {
        let root = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let run = synthetic_run(root.path());
        let options = SatelliteRenderOptions {
            run_directory: run,
            output_directory: output.path().to_path_buf(),
            frames: SatelliteFrameSelection::Explicit(vec![1830]),
            ir_enhancement: "cimss".to_owned(),
            width: 320,
            height: 240,
            json: true,
        };
        let rendered = render(&options, &context(), &mut Vec::new()).unwrap();
        let mut tampered = rendered.manifest;
        tampered.ir_enhancement = "natural".to_owned();
        bowecho_cli::fs::write_json_atomic(&rendered.manifest_path, &tampered).unwrap();

        let (report, code) = verify(&rendered.manifest_path).unwrap();
        assert_eq!(code, ExitCode::Verification);
        assert!(!report.verified);
        assert!(
            report
                .contract_failures
                .iter()
                .any(|failure| failure.contains("processing_identity mismatch"))
        );
    }

    #[test]
    fn png_verification_fully_decodes_pixel_stream() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("truncated.png");
        image::RgbaImage::from_fn(128, 128, |x, y| {
            image::Rgba([
                x.wrapping_mul(17) as u8,
                y.wrapping_mul(29) as u8,
                x.wrapping_add(y).wrapping_mul(11) as u8,
                255,
            ])
        })
        .save(&path)
        .unwrap();
        let mut bytes = fs::read(&path).unwrap();
        bytes.truncate(bytes.len() / 2);
        fs::write(&path, &bytes).unwrap();
        assert_eq!(
            image::ImageReader::open(&path)
                .unwrap()
                .with_guessed_format()
                .unwrap()
                .into_dimensions()
                .unwrap(),
            (128, 128)
        );
        let hash = bowecho_cli::fs::sha256_file(&path).unwrap();
        let mut receipt = SatelliteReceiptVerification {
            kind: SatelliteVerificationKind::Artifact,
            label: "truncated PNG".to_owned(),
            declared_path: PathBuf::from("truncated.png"),
            resolved_path: Some(path),
            expected_bytes: hash.bytes,
            expected_sha256: hash.sha256.clone(),
            actual_bytes: Some(hash.bytes),
            actual_sha256: Some(hash.sha256),
            verified: true,
            failures: Vec::new(),
        };

        verify_png_dimensions(&mut receipt, 128, 128, "image/png");
        assert!(!receipt.verified);
        assert!(
            receipt
                .failures
                .iter()
                .any(|failure| failure.contains("cannot fully decode satellite PNG"))
        );
    }

    #[test]
    fn strict_enhancement_parse_does_not_silently_default() {
        assert_eq!(parse_ir_enhancement("cimss").unwrap(), IrEnhancement::Cimss);
        assert_eq!(parse_ir_enhancement("CIMSS").unwrap(), IrEnhancement::Cimss);
        assert!(parse_ir_enhancement("cmiss").is_err());
    }

    #[test]
    fn archive_bounds_preserve_unknowns_and_exact_times() {
        let bounds = convert_native_bounds(NativeSatelliteArchiveBounds {
            first_time_utc: Some(Utc.with_ymd_and_hms(2026, 7, 22, 18, 0, 0).unwrap()),
            latest_time_utc: Some(Utc.with_ymd_and_hms(2026, 7, 22, 18, 10, 0).unwrap()),
            cadence_seconds: Some(600),
            west_degrees: None,
            south_degrees: None,
            east_degrees: None,
            north_degrees: None,
        });
        assert_eq!(
            bounds.first_time_utc.as_deref(),
            Some("2026-07-22T18:00:00.000Z")
        );
        assert_eq!(
            bounds.latest_time_utc.as_deref(),
            Some("2026-07-22T18:10:00.000Z")
        );
        assert_eq!(bounds.cadence_seconds, Some(600));
        assert_eq!(bounds.west_degrees, None);
    }

    #[test]
    fn fetch_cap_rejects_truncated_or_oversized_catalogs() {
        assert!(!selection_exceeds_fetch_cap(4, false, 4));
        assert!(selection_exceeds_fetch_cap(5, false, 4));
        assert!(selection_exceeds_fetch_cap(4, true, 4));
    }

    #[test]
    fn archive_store_root_is_canonical_and_refuses_files() {
        let parent = tempfile::tempdir().unwrap();
        let fresh = parent.path().join("store");
        let canonical = prepare_archive_store_root(&fresh).unwrap();
        assert_eq!(canonical, fs::canonicalize(&fresh).unwrap());

        let file = parent.path().join("not-a-directory");
        fs::write(&file, b"data").unwrap();
        assert!(prepare_archive_store_root(&file).is_err());
        assert!(prepare_archive_store_root(&parent.path().join("..").join("escape")).is_err());
    }

    #[test]
    fn catalog_failures_distinguish_usage_from_provider_unavailability() {
        assert_eq!(
            classify_catalog_failure("unknown GOES product 'wat'"),
            ArchiveCliFailureKind::Usage
        );
        assert_eq!(
            classify_catalog_failure("list GOES prefix: connection refused"),
            ArchiveCliFailureKind::Unavailable
        );
    }
}
