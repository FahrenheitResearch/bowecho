//! BowEcho-owned presentation metadata for locally imported WRF runs.
//!
//! rw-store intentionally keeps its run manifest source-agnostic.  Capture
//! producer/grid facts before import discards the WRF global attributes and
//! keep them in one atomic registry at the store root, outside run folders so
//! strict rw-store validation never sees an unknown per-run file.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use app_ui::wrf_scene_inventory::{
    WrfDomainId, WrfProducerIdentity, WrfSceneGroup, parse_wrf_domain_id,
};
use rw_store::atomic::atomic_write_bytes;
use rw_store::run::{RwsRunManifest, validate_store_component};
use rw_store::{RunLock, RwsSourceProvenance};
use serde::{Deserialize, Serialize};
use wrf_core::WrfFile;

const REGISTRY_SCHEMA: &str = "bowecho-wrf-sources/1";
const REGISTRY_FILE: &str = ".bowecho-wrf-sources.json";
const PROVENANCE_LOCK_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) const PRIVATE_WRF_PROVIDER: &str = "private-wrf";
pub(crate) const PRIVATE_ARWEN_PROVIDER: &str = "private-arwen";

fn registry_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct WrfRunSourceMetadata {
    pub producer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producer_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<u16>,
    pub nx: usize,
    pub ny: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dx_m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dy_m: Option<f64>,
}

impl WrfRunSourceMetadata {
    /// Conservative identity for a post-processed WRF file that no longer
    /// exposes the raw producer attributes needed by `wrf-core`. ArWen is
    /// never inferred through this fallback; only an observed GPUWM marker
    /// may select the ArWen identity.
    pub(crate) fn generic_wrf() -> Self {
        Self {
            producer: "wrf".to_owned(),
            producer_version: None,
            domain: None,
            nx: 0,
            ny: 0,
            dx_m: None,
            dy_m: None,
        }
    }

    pub(crate) fn from_scene_group(group: &WrfSceneGroup) -> Self {
        let (producer, producer_version) = producer_fields(&group.key.producer);
        Self {
            producer,
            producer_version,
            domain: Some(group.key.run_domain.domain.0),
            nx: group.key.grid_signature.nx,
            ny: group.key.grid_signature.ny,
            dx_m: millimeters_to_meters(group.key.grid_signature.dx_millimeters),
            dy_m: millimeters_to_meters(group.key.grid_signature.dy_millimeters),
        }
    }

    pub(crate) fn from_wrf_file(file: &WrfFile, path: &Path) -> Self {
        let gpuwm_version = file.global_attr_str("GPUWM_VERSION").ok();
        let producer = WrfProducerIdentity::from_gpuwm_version(gpuwm_version.as_deref());
        let (producer, producer_version) = producer_fields(&producer);
        let domain = file
            .global_attr_i32("GRID_ID")
            .ok()
            .and_then(|value| u16::try_from(value).ok())
            .filter(|value| *value > 0)
            .map(WrfDomainId)
            .or_else(|| parse_wrf_domain_id(path))
            .map(|domain| domain.0);
        Self {
            producer,
            producer_version,
            domain,
            nx: file.nx,
            ny: file.ny,
            dx_m: valid_spacing(file.dx),
            dy_m: valid_spacing(file.dy),
        }
    }

    pub(crate) fn inspect(path: &Path) -> Result<Self, String> {
        let file = WrfFile::open(path)
            .map_err(|error| format!("inspect WRF source {}: {error}", path.display()))?;
        let metadata = Self::from_wrf_file(&file, path);
        file.clear_cache();
        Ok(metadata)
    }

    /// Compact text used above native plots and in exported PNG subtitles.
    pub(crate) fn plot_label(&self) -> String {
        let mut parts = vec![self.producer_label()];
        if let Some(domain) = self.domain {
            parts.push(format!("d{domain:02}"));
        }
        if self.nx > 0 && self.ny > 0 {
            parts.push(format!("{}×{}", self.nx, self.ny));
        }
        match (self.dx_m, self.dy_m) {
            (Some(dx), Some(dy)) if !spacing_matches(dx, dy) => parts.push(format!(
                "Δx {} / Δy {}",
                format_spacing(dx),
                format_spacing(dy)
            )),
            (Some(dx), _) => parts.push(format!("Δx {}", format_spacing(dx))),
            (None, Some(dy)) => parts.push(format!("Δy {}", format_spacing(dy))),
            (None, None) => {}
        }
        parts.join(" | ")
    }

    fn producer_label(&self) -> String {
        match self.producer.as_str() {
            "arwen" => self
                .producer_version
                .as_deref()
                .map(|version| format!("ArWen {version}"))
                .unwrap_or_else(|| "ArWen".to_owned()),
            _ => "WRF".to_owned(),
        }
    }

    /// Sanitized rw-store provenance for an owner-processed local run.
    ///
    /// This labels producer identity only. It is deliberately not a claim
    /// that the owner may redistribute the bytes; the publication workflow
    /// asks for and persists that confirmation separately.
    pub(crate) fn store_provenance(&self) -> Result<RwsSourceProvenance, String> {
        let provider = match self.producer.as_str() {
            "wrf" => PRIVATE_WRF_PROVIDER,
            "arwen" => PRIVATE_ARWEN_PROVIDER,
            other => {
                return Err(format!(
                    "unsupported WRF producer identity '{other}'; expected 'wrf' or 'arwen'"
                ));
            }
        };
        RwsSourceProvenance::new(
            provider,
            vec!["owner-processed".to_owned()],
            vec!["rw-store".to_owned()],
        )
        .map_err(|error| format!("build WRF source provenance: {error}"))
    }
}

/// Attach the producer marker to one just-written WRF/ArWen hour while
/// holding the same advisory run lock used by rw-store writers.
///
/// Convenience rw-store writers predate per-hour provenance, so their WRF
/// call sites use this immediately after `finish`. Publication still rejects
/// an interrupted legacy hour whose marker was never committed.
pub(crate) fn stamp_hour_source_provenance(
    store_root: &Path,
    model: &str,
    run: &str,
    storage_slot: u16,
    metadata: &WrfRunSourceMetadata,
) -> Result<(), String> {
    validate_store_component("WRF model", model).map_err(|error| error.to_string())?;
    validate_store_component("WRF run", run).map_err(|error| error.to_string())?;
    let run_dir = store_root.join(model).join(run);
    let _lock = RunLock::acquire(&run_dir, PROVENANCE_LOCK_TIMEOUT)
        .map_err(|error| format!("lock WRF run for provenance: {error}"))?;
    let manifest_path = run_dir.join("run.json");
    let mut manifest = RwsRunManifest::load_for_run(&manifest_path, model, run)
        .map_err(|error| format!("open WRF run for provenance: {error}"))?;
    let entry = manifest.hours.get_mut(&storage_slot).ok_or_else(|| {
        format!("WRF run manifest has no just-written storage slot {storage_slot}")
    })?;
    entry.source_provenance = vec![metadata.store_provenance()?];
    manifest
        .save(&manifest_path)
        .map_err(|error| format!("save WRF source provenance: {error}"))
}

/// Explicit repair for a legacy processed run whose hour entries predate
/// provenance. This operation never infers or grants redistribution rights;
/// it records only the producer identity already present in BowEcho's
/// external registry. The exact confirmation phrase keeps migration from
/// becoming an accidental side effect of browsing or publishing.
#[allow(dead_code)] // wired only from the explicit generation-publication migration UI
pub(crate) fn migrate_legacy_run_provenance(
    store_root: &Path,
    model: &str,
    run: &str,
    confirmation: &str,
) -> Result<usize, String> {
    const CONFIRMATION: &str = "MIGRATE LEGACY WRF PROVENANCE";
    if confirmation != CONFIRMATION {
        return Err(format!(
            "legacy provenance migration requires the exact confirmation '{CONFIRMATION}'"
        ));
    }
    validate_store_component("WRF model", model).map_err(|error| error.to_string())?;
    validate_store_component("WRF run", run).map_err(|error| error.to_string())?;
    let metadata = read_run_metadata(store_root, model, run)?
        .ok_or_else(|| "legacy WRF run has no reviewed producer registry entry".to_owned())?;
    let provenance = metadata.store_provenance()?;
    let run_dir = store_root.join(model).join(run);
    let _lock = RunLock::acquire(&run_dir, PROVENANCE_LOCK_TIMEOUT)
        .map_err(|error| format!("lock legacy WRF run for provenance migration: {error}"))?;
    let manifest_path = run_dir.join("run.json");
    let mut manifest = RwsRunManifest::load_for_run(&manifest_path, model, run)
        .map_err(|error| format!("open legacy WRF run: {error}"))?;
    let mut migrated = 0usize;
    for entry in manifest.hours.values_mut() {
        if entry.source_provenance.is_empty() {
            entry.source_provenance = vec![provenance.clone()];
            migrated += 1;
        }
    }
    if migrated > 0 {
        manifest
            .save(&manifest_path)
            .map_err(|error| format!("save migrated WRF provenance: {error}"))?;
    }
    Ok(migrated)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct RegistryEntry {
    model: String,
    run: String,
    metadata: WrfRunSourceMetadata,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct RegistryDocument {
    schema: String,
    #[serde(default)]
    entries: Vec<RegistryEntry>,
}

impl Default for RegistryDocument {
    fn default() -> Self {
        Self {
            schema: REGISTRY_SCHEMA.to_owned(),
            entries: Vec::new(),
        }
    }
}

pub(crate) fn write_run_metadata(
    store_root: &Path,
    model: &str,
    run: &str,
    metadata: WrfRunSourceMetadata,
) -> Result<(), String> {
    let _guard = registry_lock()
        .lock()
        .map_err(|_| "WRF source registry lock is poisoned".to_owned())?;
    std::fs::create_dir_all(store_root)
        .map_err(|error| format!("create model store {}: {error}", store_root.display()))?;
    let path = registry_path(store_root);
    let mut document = read_document(&path)?;
    if document.schema != REGISTRY_SCHEMA {
        return Err(format!(
            "unsupported WRF source registry schema '{}'",
            document.schema
        ));
    }
    if let Some(entry) = document
        .entries
        .iter_mut()
        .find(|entry| entry.model == model && entry.run == run)
    {
        entry.metadata = metadata;
    } else {
        document.entries.push(RegistryEntry {
            model: model.to_owned(),
            run: run.to_owned(),
            metadata,
        });
    }
    document
        .entries
        .sort_by(|left, right| (&left.model, &left.run).cmp(&(&right.model, &right.run)));
    let mut bytes = serde_json::to_vec_pretty(&document)
        .map_err(|error| format!("serialize WRF source registry: {error}"))?;
    bytes.push(b'\n');
    atomic_write_bytes(&path, &bytes)
        .map_err(|error| format!("write WRF source registry {}: {error}", path.display()))
}

pub(crate) fn read_run_metadata(
    store_root: &Path,
    model: &str,
    run: &str,
) -> Result<Option<WrfRunSourceMetadata>, String> {
    let _guard = registry_lock()
        .lock()
        .map_err(|_| "WRF source registry lock is poisoned".to_owned())?;
    let path = registry_path(store_root);
    if !path.is_file() {
        return Ok(None);
    }
    let document = read_document(&path)?;
    if document.schema != REGISTRY_SCHEMA {
        return Err(format!(
            "unsupported WRF source registry schema '{}'",
            document.schema
        ));
    }
    Ok(document
        .entries
        .into_iter()
        .find(|entry| entry.model == model && entry.run == run)
        .map(|entry| entry.metadata))
}

fn read_document(path: &Path) -> Result<RegistryDocument, String> {
    if !path.is_file() {
        return Ok(RegistryDocument::default());
    }
    let bytes = std::fs::read(path)
        .map_err(|error| format!("read WRF source registry {}: {error}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse WRF source registry {}: {error}", path.display()))
}

fn registry_path(store_root: &Path) -> PathBuf {
    store_root.join(REGISTRY_FILE)
}

fn producer_fields(producer: &WrfProducerIdentity) -> (String, Option<String>) {
    match producer {
        WrfProducerIdentity::Wrf => ("wrf".to_owned(), None),
        WrfProducerIdentity::Arwen { version } => ("arwen".to_owned(), Some(version.clone())),
    }
}

fn valid_spacing(value: f64) -> Option<f64> {
    (value.is_finite() && value > 0.0).then_some(value)
}

fn millimeters_to_meters(value: Option<u64>) -> Option<f64> {
    value.map(|value| value as f64 / 1_000.0)
}

fn spacing_matches(left: f64, right: f64) -> bool {
    (left - right).abs() <= left.abs().max(right.abs()).max(1.0) * 1.0e-6
}

fn format_spacing(meters: f64) -> String {
    if meters >= 1_000.0 && (meters / 1_000.0).fract().abs() < 1.0e-6 {
        format!("{:.0} km", meters / 1_000.0)
    } else if meters.fract().abs() < 1.0e-6 {
        format!("{meters:.0} m")
    } else {
        format!("{meters:.1} m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arwen_metadata() -> WrfRunSourceMetadata {
        WrfRunSourceMetadata {
            producer: "arwen".to_owned(),
            producer_version: Some("1.5.1".to_owned()),
            domain: Some(2),
            nx: 800,
            ny: 800,
            dx_m: Some(250.0),
            dy_m: Some(250.0),
        }
    }

    #[test]
    fn feedback_v03412_arwen_plot_label_keeps_source_domain_shape_and_resolution() {
        assert_eq!(
            arwen_metadata().plot_label(),
            "ArWen 1.5.1 | d02 | 800×800 | Δx 250 m"
        );
    }

    #[test]
    fn feedback_v03412_wrf_source_registry_round_trips_without_private_paths() {
        let temp = tempfile::tempdir().unwrap();
        let metadata = arwen_metadata();
        write_run_metadata(temp.path(), "wrf", "local_test_d02", metadata.clone()).unwrap();
        assert_eq!(
            read_run_metadata(temp.path(), "wrf", "local_test_d02").unwrap(),
            Some(metadata)
        );
        let raw = std::fs::read_to_string(registry_path(temp.path())).unwrap();
        assert!(!raw.contains("source_path"));
        assert!(!raw.contains("C:\\"));
    }
}
