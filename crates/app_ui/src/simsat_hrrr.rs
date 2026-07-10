//! HRRR native-grid input acquisition for the embedded SimSat renderer.
//!
//! SimSat needs the full `wrfnat` product rather than BowEcho's smaller
//! pressure/surface ingest products. This module owns that distinction: it
//! discovers compatible local files, gives downloads a stable human-readable
//! location, and delegates the actual resumable stream to `data_source`.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, NaiveDate, Utc};
use data_source::DataSourceError;
use serde_json::Value;
use thiserror::Error;

const HRRR_NOMADS_ROOT: &str = "https://nomads.ncep.noaa.gov/pub/data/nccf/com/hrrr/prod";
const HRRR_AWS_ROOT: &str = "https://noaa-hrrr-bdp-pds.s3.amazonaws.com";
const HRRR_NATIVE_MIN_BYTES: u64 = 16;
const LATEST_SPEC_COUNT: usize = 4;
const RECENT_CYCLE_SEARCH_COUNT: usize = 96;
const HOURLY_CYCLES: [u8; 24] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
];

/// A specific HRRR CONUS native-grid forecast file.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct HrrrNativeSpec {
    pub(crate) date: String,
    pub(crate) cycle: u8,
    pub(crate) forecast_hour: u16,
}

impl HrrrNativeSpec {
    pub(crate) fn new(
        date: impl Into<String>,
        cycle: u8,
        forecast_hour: u16,
    ) -> Result<Self, HrrrNativeSpecError> {
        let spec = Self {
            date: date.into(),
            cycle,
            forecast_hour,
        };
        spec.validate()?;
        Ok(spec)
    }

    /// Validate the date, cycle, and operational HRRR forecast horizon.
    pub(crate) fn validate(&self) -> Result<(), HrrrNativeSpecError> {
        if self.date.len() != 8
            || !self.date.bytes().all(|byte| byte.is_ascii_digit())
            || NaiveDate::parse_from_str(&self.date, "%Y%m%d").is_err()
        {
            return Err(HrrrNativeSpecError::InvalidDate(self.date.clone()));
        }
        if self.cycle >= 24 {
            return Err(HrrrNativeSpecError::InvalidCycle(self.cycle));
        }

        // HRRR's 00/06/12/18z runs extend through f48; intervening hourly
        // cycles publish through f18.
        let max_forecast_hour = if self.cycle % 6 == 0 { 48 } else { 18 };
        if self.forecast_hour > max_forecast_hour {
            return Err(HrrrNativeSpecError::UnsupportedForecastHour {
                cycle: self.cycle,
                forecast_hour: self.forecast_hour,
                max_forecast_hour,
            });
        }
        Ok(())
    }

    /// Canonical NCEP filename used by NOMADS and the public AWS mirror.
    pub(crate) fn filename(&self) -> String {
        format!(
            "hrrr.t{:02}z.wrfnatf{:02}.grib2",
            self.cycle, self.forecast_hour
        )
    }

    /// Stable local destination below the caller's SimSat input root.
    pub(crate) fn cache_path(&self, root: &Path) -> PathBuf {
        root.join(format!("hrrr.{}", self.date))
            .join("conus")
            .join(self.filename())
    }

    /// Candidate mirrors in failover order.
    pub(crate) fn url_candidates(&self) -> Result<[NativeUrlCandidate; 2], HrrrNativeSpecError> {
        self.validate()?;
        let relative = format!("hrrr.{}/conus/{}", self.date, self.filename());
        Ok([
            NativeUrlCandidate {
                source: NativeSource::Nomads,
                url: format!("{HRRR_NOMADS_ROOT}/{relative}"),
            },
            NativeUrlCandidate {
                source: NativeSource::Aws,
                url: format!("{HRRR_AWS_ROOT}/{relative}"),
            },
        ])
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub(crate) enum HrrrNativeSpecError {
    #[error("HRRR date must be a real YYYYMMDD date, got '{0}'")]
    InvalidDate(String),
    #[error("HRRR cycle must be between 00z and 23z, got {0:02}z")]
    InvalidCycle(u8),
    #[error("HRRR {cycle:02}z publishes through f{max_forecast_hour:02}, not f{forecast_hour:02}")]
    UnsupportedForecastHour {
        cycle: u8,
        forecast_hour: u16,
        max_forecast_hour: u16,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum NativeSource {
    Nomads,
    Aws,
}

impl NativeSource {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Nomads => "NOMADS",
            Self::Aws => "NOAA AWS",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeUrlCandidate {
    pub(crate) source: NativeSource,
    pub(crate) url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum NativeInputOrigin {
    HumanNamed,
    RustwxRawCache,
}

impl NativeInputOrigin {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::HumanNamed => "local HRRR file",
            Self::RustwxRawCache => "BowEcho model cache",
        }
    }
}

/// One complete local HRRR native-grid input that SimSat can open directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CachedNativeInput {
    pub(crate) spec: Option<HrrrNativeSpec>,
    pub(crate) path: PathBuf,
    pub(crate) origin: NativeInputOrigin,
    pub(crate) source: Option<NativeSource>,
    pub(crate) source_url: Option<String>,
    pub(crate) bytes: u64,
}

impl CachedNativeInput {
    pub(crate) fn label(&self) -> String {
        let run = self.spec.as_ref().map_or_else(
            || {
                self.path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("HRRR native input")
                    .to_owned()
            },
            |spec| {
                let date = if spec.date.len() == 8
                    && spec.date.bytes().all(|byte| byte.is_ascii_digit())
                {
                    format!(
                        "{}-{}-{}",
                        &spec.date[0..4],
                        &spec.date[4..6],
                        &spec.date[6..8]
                    )
                } else {
                    spec.date.clone()
                };
                format!("{date} {:02}z f{:02}", spec.cycle, spec.forecast_hour)
            },
        );
        let provenance = self
            .source
            .or_else(|| self.source_url.as_deref().and_then(source_from_url))
            .map(NativeSource::label)
            .unwrap_or_else(|| self.origin.label());
        let size_mib = self.bytes as f64 / (1024.0 * 1024.0);
        format!("{run} · {provenance} · {size_mib:.1} MiB")
    }
}

/// Find complete native HRRR files below `root` without following symlinks.
///
/// Two layouts are recognized:
///
/// - human/upstream names (`hrrr.tHHz.wrfnatfFF.grib2`), including the
///   canonical `hrrr.YYYYMMDD/conus/` hierarchy;
/// - rustwx's hashed `_raw_fetch` entries, but only when a valid
///   `fetch_meta.json` proves that `fetch.grib2` is the HRRR native product.
///
/// Pressure/surface products, partial `.download` files, malformed metadata,
/// size mismatches, and truncated GRIB files are intentionally omitted.
pub(crate) fn discover_native_files(root: &Path) -> Vec<CachedNativeInput> {
    let mut found = Vec::new();
    let mut directories = vec![root.to_path_buf()];

    while let Some(directory) = directories.pop() {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                directories.push(entry.path());
                continue;
            }
            if !file_type.is_file() {
                continue;
            }

            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.eq_ignore_ascii_case("fetch_meta.json") {
                if let Some(input) = cached_input_from_fetch_meta(&path) {
                    found.push(input);
                }
            } else if let Some((cycle, forecast_hour)) = parse_native_filename(name)
                && let Some(bytes) = complete_grib_len(&path)
            {
                found.push(CachedNativeInput {
                    spec: human_path_spec(&path, cycle, forecast_hour),
                    path,
                    origin: NativeInputOrigin::HumanNamed,
                    source: None,
                    source_url: None,
                    bytes,
                });
            }
        }
    }

    // A symlink-free walk cannot normally reach one path twice, but metadata
    // and hand-created directory junctions have surprised cache scanners in
    // the past. Stable de-duplication also makes this contract explicit.
    let mut seen = HashSet::new();
    found.retain(|input| seen.insert(input.path.clone()));
    found.sort_by(|left, right| match (&left.spec, &right.spec) {
        (Some(left_spec), Some(right_spec)) => right_spec
            .cmp(left_spec)
            .then_with(|| left.origin.cmp(&right.origin))
            .then_with(|| left.path.cmp(&right.path)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => left.path.cmp(&right.path),
    });
    found
}

fn parse_native_filename(name: &str) -> Option<(u8, u16)> {
    let lower = name.to_ascii_lowercase();
    let remainder = lower.strip_prefix("hrrr.t")?;
    let (cycle, remainder) = remainder.split_once("z.wrfnatf")?;
    let forecast_hour = remainder.strip_suffix(".grib2")?;
    if cycle.len() != 2
        || forecast_hour.len() != 2
        || !cycle.bytes().all(|byte| byte.is_ascii_digit())
        || !forecast_hour.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let cycle = cycle.parse().ok()?;
    let forecast_hour = forecast_hour.parse().ok()?;
    (cycle < 24).then_some((cycle, forecast_hour))
}

fn human_path_spec(path: &Path, cycle: u8, forecast_hour: u16) -> Option<HrrrNativeSpec> {
    for ancestor in path.parent()?.ancestors() {
        let Some(name) = ancestor.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(date) = name
            .to_ascii_lowercase()
            .strip_prefix("hrrr.")
            .map(str::to_owned)
        else {
            continue;
        };
        if date.len() == 8 && date.bytes().all(|byte| byte.is_ascii_digit()) {
            return HrrrNativeSpec::new(date, cycle, forecast_hour).ok();
        }
    }
    None
}

fn cached_input_from_fetch_meta(metadata_path: &Path) -> Option<CachedNativeInput> {
    // Metadata is tiny. Refuse an unexpectedly large file before allocating;
    // it is not a valid rustwx sidecar and should not stall folder discovery.
    if metadata_path.metadata().ok()?.len() > 1024 * 1024 {
        return None;
    }
    let value: Value = serde_json::from_slice(&fs::read(metadata_path).ok()?).ok()?;
    let payload = value.get("payload").unwrap_or(&value);
    let request = payload.get("request")?;

    let model = request.get("model")?.as_str()?.to_ascii_lowercase();
    if model != "hrrr" {
        return None;
    }
    let product = request.get("product")?.as_str()?.to_ascii_lowercase();
    if !matches!(product.as_str(), "nat" | "native" | "wrfnat") {
        return None;
    }

    let cycle_value = request.get("cycle")?;
    let date = cycle_value.get("date_yyyymmdd")?.as_str()?.to_owned();
    let cycle = u8::try_from(cycle_value.get("hour_utc")?.as_u64()?).ok()?;
    let forecast_hour = u16::try_from(request.get("forecast_hour")?.as_u64()?).ok()?;
    let spec = HrrrNativeSpec::new(date, cycle, forecast_hour).ok()?;

    let source_url = payload.get("resolved_url")?.as_str()?.to_owned();
    if !source_url.to_ascii_lowercase().contains("wrfnat") {
        return None;
    }
    if let Some(family) = payload.get("resolved_family").and_then(Value::as_str)
        && !matches!(
            family.to_ascii_lowercase().as_str(),
            "nat" | "native" | "wrfnat"
        )
    {
        return None;
    }

    let expected_bytes = payload.get("bytes_len")?.as_u64()?;
    let data_path = metadata_path.with_file_name("fetch.grib2");
    let bytes = complete_grib_len(&data_path)?;
    if bytes != expected_bytes {
        return None;
    }

    let source = payload
        .get("resolved_source")
        .and_then(Value::as_str)
        .and_then(parse_native_source)
        .or_else(|| source_from_url(&source_url));

    Some(CachedNativeInput {
        spec: Some(spec),
        path: data_path,
        origin: NativeInputOrigin::RustwxRawCache,
        source,
        source_url: Some(source_url),
        bytes,
    })
}

fn parse_native_source(source: &str) -> Option<NativeSource> {
    let lower = source.to_ascii_lowercase();
    if lower.contains("nomads") {
        Some(NativeSource::Nomads)
    } else if lower.contains("aws") || lower.contains("s3") {
        Some(NativeSource::Aws)
    } else {
        None
    }
}

fn source_from_url(url: &str) -> Option<NativeSource> {
    let lower = url.to_ascii_lowercase();
    if lower.contains("nomads.ncep.noaa.gov") {
        Some(NativeSource::Nomads)
    } else if lower.contains("noaa-hrrr-bdp-pds") || lower.contains("amazonaws.com") {
        Some(NativeSource::Aws)
    } else {
        None
    }
}

/// Verify enough of the GRIB envelope to reject corrupt and partial files.
/// HRRR files contain concatenated GRIB2 messages, so a complete file starts
/// with a GRIB2 section 0 and its last message ends with `7777`.
fn complete_grib_len(path: &Path) -> Option<u64> {
    let mut file = File::open(path).ok()?;
    let bytes = file.metadata().ok()?.len();
    if bytes < HRRR_NATIVE_MIN_BYTES {
        return None;
    }
    let mut header = [0_u8; 8];
    file.read_exact(&mut header).ok()?;
    if &header[0..4] != b"GRIB" || header[7] != 2 {
        return None;
    }
    file.seek(SeekFrom::End(-4)).ok()?;
    let mut trailer = [0_u8; 4];
    file.read_exact(&mut trailer).ok()?;
    (&trailer == b"7777").then_some(bytes)
}

/// Fresh plausible HRRR runs for a forecast hour, newest first.
///
/// This is deliberately availability-free: the UI can present useful choices
/// instantly, then `download_native` walks the two upstream mirrors. The same
/// 55-minute publication floor as BowEcho's normal HRRR ingest avoids offering
/// a cycle that has almost certainly not published yet.
pub(crate) fn latest_specs(now: DateTime<Utc>, forecast_hour: u16) -> Vec<HrrrNativeSpec> {
    crate::ingest_worker::recent_cycle_candidates(
        now,
        &HOURLY_CYCLES,
        crate::ingest_worker::publication_lag_minutes(rustwx_core::ModelId::Hrrr),
        RECENT_CYCLE_SEARCH_COUNT,
    )
    .into_iter()
    .filter_map(|(date, cycle)| HrrrNativeSpec::new(date, cycle, forecast_hour).ok())
    .take(LATEST_SPEC_COUNT)
    .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeDownloadState {
    Ready,
    Cancelled,
}

/// A non-error terminal download result. Cancellation is a normal outcome and
/// exposes the retained partial file so a retry can explain that it will
/// resume rather than restart.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // worker transport retains resume/provenance telemetry for the pane
pub(crate) struct NativeDownloadOutcome {
    pub(crate) state: NativeDownloadState,
    pub(crate) spec: HrrrNativeSpec,
    pub(crate) path: PathBuf,
    pub(crate) partial_path: Option<PathBuf>,
    pub(crate) bytes: u64,
    pub(crate) source: Option<NativeSource>,
    pub(crate) resumed: bool,
    pub(crate) cache_hit: bool,
}

impl NativeDownloadOutcome {
    pub(crate) fn is_ready(&self) -> bool {
        self.state == NativeDownloadState::Ready
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.state == NativeDownloadState::Cancelled
    }
}

/// Pure progress states suitable for sending from a worker to egui.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // pure worker-to-UI seam; the first pane consumes message text only
pub(crate) enum NativeDownloadStage {
    CheckingCache,
    Trying {
        source: NativeSource,
        candidate: usize,
        total: usize,
    },
    CandidateFailed {
        source: NativeSource,
        candidate: usize,
        total: usize,
    },
    Ready,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // pure worker-to-UI seam; available to richer progress views
pub(crate) struct NativeDownloadStatus {
    pub(crate) spec: HrrrNativeSpec,
    pub(crate) path: PathBuf,
    pub(crate) stage: NativeDownloadStage,
    pub(crate) message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // retained for per-mirror diagnostics in future UI detail
pub(crate) struct NativeDownloadAttempt {
    pub(crate) source: NativeSource,
    pub(crate) url: String,
    pub(crate) error: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
#[allow(dead_code)] // structured context supplements the user-facing Display message
pub(crate) struct NativeDownloadError {
    pub(crate) spec: HrrrNativeSpec,
    pub(crate) destination: PathBuf,
    pub(crate) attempts: Vec<NativeDownloadAttempt>,
    pub(crate) message: String,
}

impl NativeDownloadError {
    fn local(spec: &HrrrNativeSpec, destination: &Path, message: impl Into<String>) -> Self {
        Self {
            spec: spec.clone(),
            destination: destination.to_owned(),
            attempts: Vec::new(),
            message: message.into(),
        }
    }
}

/// Download one full HRRR native-grid file, retaining a resumable partial on
/// cancellation or a broken stream. NOMADS is attempted first, then NOAA's
/// AWS mirror. A complete canonical file is an offline cache hit.
pub(crate) fn download_native(
    spec: &HrrrNativeSpec,
    root: &Path,
    cancel: &AtomicBool,
) -> Result<NativeDownloadOutcome, NativeDownloadError> {
    download_native_with_status(spec, root, cancel, |_| {})
}

pub(crate) fn download_native_with_status(
    spec: &HrrrNativeSpec,
    root: &Path,
    cancel: &AtomicBool,
    mut on_status: impl FnMut(NativeDownloadStatus),
) -> Result<NativeDownloadOutcome, NativeDownloadError> {
    let destination = spec.cache_path(root);
    spec.validate().map_err(|error| {
        NativeDownloadError::local(spec, &destination, format!("invalid HRRR request: {error}"))
    })?;

    send_status(
        &mut on_status,
        spec,
        &destination,
        NativeDownloadStage::CheckingCache,
        "Checking the local HRRR native-file cache",
    );

    if let Some(bytes) = complete_grib_len(&destination) {
        send_status(
            &mut on_status,
            spec,
            &destination,
            NativeDownloadStage::Ready,
            "Using the complete HRRR native file already on disk",
        );
        return Ok(NativeDownloadOutcome {
            state: NativeDownloadState::Ready,
            spec: spec.clone(),
            path: destination,
            partial_path: None,
            bytes,
            source: None,
            resumed: false,
            cache_hit: true,
        });
    }

    // The generic stream helper uses an equal Content-Length as its fast
    // cache-hit test. Remove an invalid canonical file first so a corrupt file
    // of the expected size can never be mistaken for a valid cache entry.
    if destination.exists() {
        fs::remove_file(&destination).map_err(|error| {
            NativeDownloadError::local(
                spec,
                &destination,
                format!(
                    "could not replace invalid HRRR cache file {}: {error}",
                    destination.display()
                ),
            )
        })?;
    }

    if cancel.load(Ordering::Relaxed) {
        send_status(
            &mut on_status,
            spec,
            &destination,
            NativeDownloadStage::Cancelled,
            "HRRR native download cancelled",
        );
        return Ok(cancelled_outcome(spec, destination, None));
    }

    let candidates = spec.url_candidates().map_err(|error| {
        NativeDownloadError::local(spec, &destination, format!("invalid HRRR request: {error}"))
    })?;
    let mut attempts = Vec::with_capacity(candidates.len());
    for (index, candidate) in candidates.iter().enumerate() {
        let ordinal = index + 1;
        send_status(
            &mut on_status,
            spec,
            &destination,
            NativeDownloadStage::Trying {
                source: candidate.source,
                candidate: ordinal,
                total: candidates.len(),
            },
            format!(
                "Downloading HRRR native data from {}",
                candidate.source.label()
            ),
        );

        match data_source::gdex::download_to_path_with_cancel(&candidate.url, &destination, cancel)
        {
            Ok(outcome) => {
                let Some(bytes) = complete_grib_len(&outcome.path) else {
                    let error =
                        "download completed but the file is not a complete GRIB2 stream".to_owned();
                    if let Err(cleanup_error) = fs::remove_file(&outcome.path) {
                        return Err(NativeDownloadError {
                            spec: spec.clone(),
                            destination: destination.clone(),
                            attempts,
                            message: format!(
                                "{error}; could not remove {}: {cleanup_error}",
                                outcome.path.display()
                            ),
                        });
                    }
                    attempts.push(NativeDownloadAttempt {
                        source: candidate.source,
                        url: candidate.url.clone(),
                        error: error.clone(),
                    });
                    send_status(
                        &mut on_status,
                        spec,
                        &destination,
                        NativeDownloadStage::CandidateFailed {
                            source: candidate.source,
                            candidate: ordinal,
                            total: candidates.len(),
                        },
                        format!("{} returned an invalid file", candidate.source.label()),
                    );
                    continue;
                };

                send_status(
                    &mut on_status,
                    spec,
                    &destination,
                    NativeDownloadStage::Ready,
                    format!(
                        "HRRR native input is ready from {}",
                        candidate.source.label()
                    ),
                );
                return Ok(NativeDownloadOutcome {
                    state: NativeDownloadState::Ready,
                    spec: spec.clone(),
                    path: outcome.path,
                    partial_path: None,
                    bytes,
                    source: Some(candidate.source),
                    resumed: outcome.resumed,
                    cache_hit: outcome.cache_hit,
                });
            }
            Err(DataSourceError::DownloadCancelled { .. }) => {
                send_status(
                    &mut on_status,
                    spec,
                    &destination,
                    NativeDownloadStage::Cancelled,
                    "HRRR native download cancelled; the partial file was kept for resume",
                );
                return Ok(cancelled_outcome(spec, destination, Some(candidate.source)));
            }
            Err(error) => {
                attempts.push(NativeDownloadAttempt {
                    source: candidate.source,
                    url: candidate.url.clone(),
                    error: error.to_string(),
                });
                send_status(
                    &mut on_status,
                    spec,
                    &destination,
                    NativeDownloadStage::CandidateFailed {
                        source: candidate.source,
                        candidate: ordinal,
                        total: candidates.len(),
                    },
                    format!("{} failed: {error}", candidate.source.label()),
                );
            }
        }
    }

    let details = attempts
        .iter()
        .map(|attempt| format!("{}: {}", attempt.source.label(), attempt.error))
        .collect::<Vec<_>>()
        .join("; ");
    Err(NativeDownloadError {
        spec: spec.clone(),
        destination,
        attempts,
        message: format!("all HRRR native download sources failed ({details})"),
    })
}

fn cancelled_outcome(
    spec: &HrrrNativeSpec,
    destination: PathBuf,
    source: Option<NativeSource>,
) -> NativeDownloadOutcome {
    let partial = destination.with_extension("download");
    let bytes = partial
        .metadata()
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    NativeDownloadOutcome {
        state: NativeDownloadState::Cancelled,
        spec: spec.clone(),
        path: destination,
        partial_path: partial.exists().then_some(partial),
        bytes,
        source,
        resumed: false,
        cache_hit: false,
    }
}

fn send_status(
    on_status: &mut impl FnMut(NativeDownloadStatus),
    spec: &HrrrNativeSpec,
    path: &Path,
    stage: NativeDownloadStage,
    message: impl Into<String>,
) {
    on_status(NativeDownloadStatus {
        spec: spec.clone(),
        path: path.to_owned(),
        stage,
        message: message.into(),
    });
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use chrono::TimeZone;
    use serde_json::json;

    use super::*;

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("test clock after Unix epoch")
                .as_nanos();
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "bowecho-simsat-hrrr-{label}-{}-{nanos}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create test directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn fake_grib() -> Vec<u8> {
        let mut bytes = vec![0_u8; 24];
        bytes[0..4].copy_from_slice(b"GRIB");
        bytes[7] = 2;
        bytes[20..24].copy_from_slice(b"7777");
        bytes
    }

    fn write_fake_grib(path: &Path) -> u64 {
        fs::create_dir_all(path.parent().expect("test file parent"))
            .expect("create test file parent");
        let bytes = fake_grib();
        fs::write(path, &bytes).expect("write fake GRIB");
        bytes.len() as u64
    }

    #[test]
    fn spec_validates_operational_horizons() {
        assert!(HrrrNativeSpec::new("20260229", 0, 3).is_err());
        assert!(HrrrNativeSpec::new("20260710", 24, 3).is_err());
        assert!(HrrrNativeSpec::new("20260710", 1, 19).is_err());
        assert!(HrrrNativeSpec::new("20260710", 0, 48).is_ok());
        assert!(HrrrNativeSpec::new("20260710", 0, 49).is_err());
    }

    #[test]
    fn spec_builds_canonical_path_and_ordered_urls() {
        let spec = HrrrNativeSpec::new("20260710", 6, 3).unwrap();
        assert_eq!(spec.filename(), "hrrr.t06z.wrfnatf03.grib2");
        assert_eq!(
            spec.cache_path(Path::new("inputs")),
            Path::new("inputs")
                .join("hrrr.20260710")
                .join("conus")
                .join("hrrr.t06z.wrfnatf03.grib2")
        );
        let candidates = spec.url_candidates().unwrap();
        assert_eq!(candidates[0].source, NativeSource::Nomads);
        assert_eq!(
            candidates[0].url,
            "https://nomads.ncep.noaa.gov/pub/data/nccf/com/hrrr/prod/hrrr.20260710/conus/hrrr.t06z.wrfnatf03.grib2"
        );
        assert_eq!(candidates[1].source, NativeSource::Aws);
        assert_eq!(
            candidates[1].url,
            "https://noaa-hrrr-bdp-pds.s3.amazonaws.com/hrrr.20260710/conus/hrrr.t06z.wrfnatf03.grib2"
        );
    }

    #[test]
    fn latest_specs_use_publication_lag_and_extended_cycles() {
        let now = Utc.with_ymd_and_hms(2026, 7, 10, 10, 30, 0).unwrap();
        let short = latest_specs(now, 3);
        assert_eq!(
            short
                .iter()
                .map(|spec| (spec.date.as_str(), spec.cycle))
                .collect::<Vec<_>>(),
            vec![
                ("20260710", 9),
                ("20260710", 8),
                ("20260710", 7),
                ("20260710", 6)
            ]
        );

        let extended = latest_specs(now, 24);
        assert_eq!(
            extended
                .iter()
                .map(|spec| (spec.date.as_str(), spec.cycle))
                .collect::<Vec<_>>(),
            vec![
                ("20260710", 6),
                ("20260710", 0),
                ("20260709", 18),
                ("20260709", 12)
            ]
        );
    }

    #[test]
    fn discovery_accepts_native_files_and_rejects_wrong_or_incomplete_inputs() {
        let root = TestDir::new("discover");

        let canonical = root
            .path()
            .join("hrrr.20260710/conus/hrrr.t06z.wrfnatf03.grib2");
        write_fake_grib(&canonical);

        let orphan = root.path().join("imports/hrrr.t12z.wrfnatf03.grib2");
        write_fake_grib(&orphan);

        let pressure = root
            .path()
            .join("hrrr.20260710/conus/hrrr.t06z.wrfprsf03.grib2");
        write_fake_grib(&pressure);

        let corrupt = root
            .path()
            .join("hrrr.20260710/conus/hrrr.t06z.wrfnatf04.grib2");
        fs::write(&corrupt, b"GRIB-incomplete").unwrap();

        let partial = HrrrNativeSpec::new("20260710", 6, 5)
            .unwrap()
            .cache_path(root.path())
            .with_extension("download");
        write_fake_grib(&partial);

        let raw_dir = root.path().join("_raw_fetch/aws/abcdef0123456789");
        let raw_data = raw_dir.join("fetch.grib2");
        let raw_bytes = write_fake_grib(&raw_data);
        fs::write(
            raw_dir.join("fetch_meta.json"),
            serde_json::to_vec_pretty(&json!({
                "schema_version": 2,
                "payload": {
                    "request": {
                        "model": "Hrrr",
                        "cycle": {"date_yyyymmdd": "20260710", "hour_utc": 6},
                        "forecast_hour": 4,
                        "product": "nat"
                    },
                    "resolved_source": "Aws",
                    "resolved_url": "https://noaa-hrrr-bdp-pds.s3.amazonaws.com/hrrr.20260710/conus/hrrr.t06z.wrfnatf04.grib2",
                    "resolved_family": "nat",
                    "bytes_len": raw_bytes,
                    "bytes_sha256": "not-needed-for-discovery"
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let wrong_product_dir = root.path().join("_raw_fetch/aws/wrong-product");
        let wrong_product_bytes = write_fake_grib(&wrong_product_dir.join("fetch.grib2"));
        fs::write(
            wrong_product_dir.join("fetch_meta.json"),
            serde_json::to_vec(&json!({
                "request": {
                    "model": "Hrrr",
                    "cycle": {"date_yyyymmdd": "20260710", "hour_utc": 6},
                    "forecast_hour": 4,
                    "product": "prs"
                },
                "resolved_url": "https://example.test/hrrr.t06z.wrfprsf04.grib2",
                "bytes_len": wrong_product_bytes
            }))
            .unwrap(),
        )
        .unwrap();

        let mismatch_dir = root.path().join("_raw_fetch/aws/size-mismatch");
        let mismatch_bytes = write_fake_grib(&mismatch_dir.join("fetch.grib2"));
        fs::write(
            mismatch_dir.join("fetch_meta.json"),
            serde_json::to_vec(&json!({
                "request": {
                    "model": "Hrrr",
                    "cycle": {"date_yyyymmdd": "20260710", "hour_utc": 6},
                    "forecast_hour": 5,
                    "product": "nat"
                },
                "resolved_source": "Aws",
                "resolved_url": "https://example.test/hrrr.t06z.wrfnatf05.grib2",
                "bytes_len": mismatch_bytes + 1
            }))
            .unwrap(),
        )
        .unwrap();

        fs::create_dir_all(root.path().join("_raw_fetch/aws/corrupt")).unwrap();
        fs::write(
            root.path().join("_raw_fetch/aws/corrupt/fetch_meta.json"),
            b"not json",
        )
        .unwrap();

        let found = discover_native_files(root.path());
        assert_eq!(found.len(), 3);
        assert!(found.iter().any(|input| input.path == canonical));
        assert!(found.iter().any(|input| input.path == orphan));
        let raw = found
            .iter()
            .find(|input| input.origin == NativeInputOrigin::RustwxRawCache)
            .expect("raw-cache native file discovered");
        assert_eq!(raw.path, raw_data);
        assert_eq!(raw.source, Some(NativeSource::Aws));
        assert_eq!(raw.spec.as_ref().unwrap().forecast_hour, 4);
        assert!(found.iter().all(|input| input.path != pressure));
        assert!(found.iter().all(|input| input.path != corrupt));
        assert!(found.iter().all(|input| input.path != partial));
    }

    #[test]
    fn cancellation_before_transfer_is_an_offline_terminal_outcome() {
        let root = TestDir::new("cancel");
        let spec = HrrrNativeSpec::new("20260710", 6, 3).unwrap();
        let cancel = AtomicBool::new(true);
        let outcome = download_native(&spec, root.path(), &cancel).unwrap();
        assert!(outcome.is_cancelled());
        assert!(!outcome.path.exists());
        assert!(outcome.partial_path.is_none());
        assert_eq!(outcome.bytes, 0);
    }

    #[test]
    fn complete_canonical_file_is_an_offline_cache_hit() {
        let root = TestDir::new("cache-hit");
        let spec = HrrrNativeSpec::new("20260710", 6, 3).unwrap();
        let path = spec.cache_path(root.path());
        let bytes = write_fake_grib(&path);
        let outcome = download_native(&spec, root.path(), &AtomicBool::new(false)).unwrap();
        assert!(outcome.is_ready());
        assert_eq!(outcome.path, path);
        assert_eq!(outcome.bytes, bytes);
        assert!(outcome.cache_hit);
        assert!(!outcome.resumed);
    }
}
