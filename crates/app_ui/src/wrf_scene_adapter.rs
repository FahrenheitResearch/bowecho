//! WRF-file adapter for the pure scene inventory.
//!
//! This is intentionally an inventory pass: it opens each selected file,
//! records every `(path, timeidx)` scene, and closes it before the heavy radar
//! field reader starts. Internal `Times` is authoritative. Filename fallback
//! remains explicit provenance and no epoch timestamps are manufactured.

use std::fmt::Write as _;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use wrf_core::WrfFile;

use crate::wrf_scene_inventory::{
    WrfDomainId, WrfGridSignature, WrfRunDomain, WrfRunId, WrfScene, WrfSceneGroup,
    WrfSceneInventory, WrfSceneTime, WrfSourceIdentity, parse_wrf_domain_id,
};

const SOURCE_PREFIX_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WrfInventoryNote {
    pub source_name: String,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectedWrfScenes {
    pub group: WrfSceneGroup,
    pub notes: Vec<WrfInventoryNote>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InventoriedWrfPaths {
    pub inventory: WrfSceneInventory,
    pub notes: Vec<WrfInventoryNote>,
}

/// Inventory every selected WRF scene without requiring all inputs to share
/// one run/domain/grid. Callers that can isolate work per compatible group
/// (the headless renderer and forecast watcher) use this entry point; the GUI
/// simulated-radar path retains [`inventory_selected_wrf_paths`] below.
pub fn inventory_wrf_paths(paths: &[PathBuf]) -> Result<InventoriedWrfPaths, String> {
    if paths.is_empty() {
        return Err("No WRF files selected".to_owned());
    }

    let mut scenes = Vec::new();
    let mut notes = Vec::new();
    for path in paths {
        let file = WrfFile::open(path)
            .map_err(|error| format!("Open {} for scene inventory: {error}", display_name(path)))?;
        let source_identity = bounded_source_identity(path)?;
        let run = run_identity(&file, path);
        let domain = domain_identity(&file, path)?;
        let grid_signature = grid_signature(&file)?;
        let times = file.times().unwrap_or_default();
        if times.len() != file.nt {
            notes.push(WrfInventoryNote {
                source_name: display_name(path),
                message: format!(
                    "Times contains {} record(s) for {} model time(s); missing records may only use an explicit filename fallback",
                    times.len(), file.nt
                ),
            });
        }
        for time_index in 0..file.nt {
            let raw_time = times.get(time_index).map(String::as_str);
            let time = WrfSceneTime::from_sources(raw_time, path);
            if !time.is_authoritative() {
                notes.push(WrfInventoryNote {
                    source_name: display_name(path),
                    message: match &time {
                        WrfSceneTime::FilenameFallback { valid_time, .. } => format!(
                            "time {time_index} uses filename fallback {valid_time}; internal WRF Times was unavailable or invalid"
                        ),
                        WrfSceneTime::Unavailable { .. } => format!(
                            "time {time_index} has no valid internal Times record or filename timestamp"
                        ),
                        WrfSceneTime::InternalTimes { .. } => unreachable!(),
                    },
                });
            }
            scenes.push(WrfScene {
                path: path.clone(),
                time_index,
                run_domain: WrfRunDomain {
                    run: run.clone(),
                    domain,
                },
                grid_signature: grid_signature.clone(),
                source_identity: source_identity.clone(),
                time,
            });
        }
        file.clear_cache();
    }

    Ok(InventoriedWrfPaths {
        inventory: WrfSceneInventory::from_scenes(scenes),
        notes,
    })
}

/// Inventory selected WRF files and require one compatible run/domain/grid.
/// Mixed d01/d02 selections, remeshed files, duplicate/restart times, and
/// untimed scenes are surfaced as errors rather than silently merged.
pub fn inventory_selected_wrf_paths(paths: &[PathBuf]) -> Result<SelectedWrfScenes, String> {
    let InventoriedWrfPaths { inventory, notes } = inventory_wrf_paths(paths)?;
    if inventory.groups.len() != 1 {
        let groups = inventory
            .groups
            .iter()
            .map(group_description)
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!(
            "Selected WRF files contain {} incompatible run/domain/grid groups ({groups}). Build one simulated-radar loop per compatible domain.",
            inventory.groups.len()
        ));
    }
    let group = inventory
        .groups
        .into_iter()
        .next()
        .expect("one group checked above");
    validate_group_diagnostics(&group)?;
    Ok(SelectedWrfScenes { group, notes })
}

fn validate_group_diagnostics(group: &WrfSceneGroup) -> Result<(), String> {
    let diagnostics = &group.diagnostics;
    if !diagnostics.unavailable_times.is_empty() {
        let scenes = diagnostics
            .unavailable_times
            .iter()
            .map(|scene| format!("{} time {}", display_name(&scene.path), scene.time_index))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "WRF scene time is unavailable for {scenes}; BowEcho will not fabricate epoch timestamps"
        ));
    }
    if !diagnostics.duplicate_times.is_empty() {
        let duplicates = diagnostics
            .duplicate_times
            .iter()
            .map(|duplicate| {
                format!(
                    "{} ({} scenes)",
                    duplicate.valid_time,
                    duplicate.scenes.len()
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "WRF selection contains duplicate valid times: {duplicates}. Resolve restart overlaps before simulation."
        ));
    }
    if !diagnostics.nonmonotonic_times.is_empty() {
        let issues = diagnostics
            .nonmonotonic_times
            .iter()
            .map(|issue| {
                format!(
                    "{} time {} ({}) follows {} time {} ({})",
                    display_name(&issue.scene.path),
                    issue.scene.time_index,
                    issue.time,
                    display_name(&issue.previous_scene.path),
                    issue.previous_scene.time_index,
                    issue.previous_time,
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!("WRF internal times are nonmonotonic: {issues}"));
    }
    Ok(())
}

fn run_identity(file: &WrfFile, path: &Path) -> WrfRunId {
    for name in ["START_DATE", "SIMULATION_START_DATE"] {
        if let Ok(value) = file.global_attr_str(name) {
            let value = value
                .trim_matches(|character: char| character.is_whitespace() || character == '\0');
            if !value.is_empty() {
                return WrfRunId(value.to_owned());
            }
        }
    }
    // The path is used only to GROUP unknown-run inputs and is never exported.
    // Hashing the parent avoids placing an absolute private path in inventory
    // diagnostics/provenance while still keeping unrelated folders separate.
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    WrfRunId(format!(
        "unknown-run-{}",
        short_hash(parent.to_string_lossy().as_bytes())
    ))
}

fn domain_identity(file: &WrfFile, path: &Path) -> Result<WrfDomainId, String> {
    if let Ok(value) = file.global_attr_i32("GRID_ID") {
        return u16::try_from(value)
            .ok()
            .filter(|value| *value > 0)
            .map(WrfDomainId)
            .ok_or_else(|| format!("{} has invalid GRID_ID {value}", display_name(path)));
    }
    parse_wrf_domain_id(path).ok_or_else(|| {
        format!(
            "{} has no valid GRID_ID attribute or dNN filename component",
            display_name(path)
        )
    })
}

fn grid_signature(file: &WrfFile) -> Result<WrfGridSignature, String> {
    let lat = file
        .xlat(0)
        .map_err(|error| format!("read XLAT for scene inventory: {error}"))?;
    let lon = file
        .xlong(0)
        .map_err(|error| format!("read XLONG for scene inventory: {error}"))?;
    let expected = file.nx.saturating_mul(file.ny);
    if lat.len() != expected || lon.len() != expected {
        return Err(format!(
            "WRF coordinate shape mismatch: expected {expected}, got XLAT {} XLONG {}",
            lat.len(),
            lon.len()
        ));
    }
    Ok(WrfGridSignature::from_meters(
        file.nx,
        file.ny,
        Some(file.nz),
        Some(file.dx),
        Some(file.dy),
        projection_identity(file),
        coordinate_digest(&lat, &lon),
    ))
}

fn projection_identity(file: &WrfFile) -> String {
    let mut identity = String::new();
    for name in ["MAP_PROJ", "PARENT_ID"] {
        let _ = write!(
            identity,
            "{name}={};",
            file.global_attr_i32(name)
                .map(|value| value.to_string())
                .unwrap_or_else(|_| "?".to_owned())
        );
    }
    for name in [
        "TRUELAT1",
        "TRUELAT2",
        "STAND_LON",
        "CEN_LAT",
        "CEN_LON",
        "POLE_LAT",
        "POLE_LON",
    ] {
        let _ = write!(
            identity,
            "{name}={};",
            file.global_attr_f64(name)
                .map(|value| format!("{:016x}", value.to_bits()))
                .unwrap_or_else(|_| "?".to_owned())
        );
    }
    identity
}

/// Stable FNV-1a over exact coordinate f64 bits. This is compatibility
/// identity, not a cryptographic content claim.
fn coordinate_digest(lat: &[f64], lon: &[f64]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for value in lat.iter().chain(lon) {
        for byte in value.to_bits().to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    hash
}

/// Bounded source identity: basename + length + first 64 KiB. It is not a
/// whole-file checksum, but it avoids multi-GiB preflight reads and never
/// exposes the absolute path.
fn bounded_source_identity(path: &Path) -> Result<WrfSourceIdentity, String> {
    let mut hasher = Sha256::new();
    hasher.update(
        path.file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_default()
            .as_bytes(),
    );
    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("stat {} for scene inventory: {error}", display_name(path)))?;
    hasher.update(metadata.len().to_le_bytes());
    let file = File::open(path)
        .map_err(|error| format!("read {} for scene inventory: {error}", display_name(path)))?;
    let mut prefix = file.take(SOURCE_PREFIX_BYTES);
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let count = prefix
            .read(&mut buffer)
            .map_err(|error| format!("hash {} prefix: {error}", display_name(path)))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(WrfSourceIdentity(format!(
        "sha256:{}",
        hex(&hasher.finalize())
    )))
}

fn short_hash(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex(&digest[..8])
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

fn group_description(group: &WrfSceneGroup) -> String {
    format!(
        "run {} {} {}x{}x{} ({} scene(s))",
        group.key.run_domain.run.0,
        group.key.run_domain.domain.label(),
        group.key.grid_signature.nx,
        group.key.grid_signature.ny,
        group.key.grid_signature.nz.unwrap_or(0),
        group.scenes.len(),
    )
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinate_digest_is_order_and_bit_sensitive() {
        let base = coordinate_digest(&[1.0, 2.0], &[3.0, 4.0]);
        assert_eq!(base, coordinate_digest(&[1.0, 2.0], &[3.0, 4.0]));
        assert_ne!(base, coordinate_digest(&[2.0, 1.0], &[3.0, 4.0]));
        assert_ne!(base, coordinate_digest(&[1.0, 2.0], &[3.0, 4.5]));
    }

    #[test]
    fn source_identity_never_contains_the_private_path() {
        let path = std::env::temp_dir().join(format!(
            "bowecho-wrf-scene-id-{}-wrfout_d03_2026-07-12_00_00_00",
            std::process::id()
        ));
        std::fs::write(&path, b"small fixture prefix").unwrap();
        let identity = bounded_source_identity(&path).unwrap();
        assert!(identity.0.starts_with("sha256:"));
        assert_eq!(identity.0.len(), "sha256:".len() + 64);
        assert!(
            !identity
                .0
                .contains(std::env::temp_dir().to_string_lossy().as_ref())
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn projection_and_group_labels_are_bounded_public_text() {
        let run = WrfRunId("2026-07-12_00:00:00".to_owned());
        let domain = WrfDomainId(3);
        assert_eq!(domain.label(), "d03");
        assert!(!run.0.contains("C:\\"));
        assert_eq!(short_hash(b"same"), short_hash(b"same"));
        assert_ne!(short_hash(b"same"), short_hash(b"different"));
    }
}
