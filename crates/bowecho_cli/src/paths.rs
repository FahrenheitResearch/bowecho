use std::path::{Path, PathBuf};

use chrono::NaiveDateTime;
use sha2::{Digest, Sha256};

use crate::run_manifest::RunManifest;

const MAX_SLUG_BYTES: usize = 80;

/// Root for one case/run/member beneath the user-selected output directory.
pub fn artifact_run_root(output: &Path, run: &RunManifest) -> PathBuf {
    let mut path = output.join(&run.case_id).join(&run.run_id);
    if let Some(member_id) = &run.member_id {
        path.push(member_id);
    }
    path
}

pub fn artifact_manifest_path(output: &Path, run: &RunManifest) -> PathBuf {
    artifact_run_root(output, run).join("artifact-manifest.json")
}

pub fn watch_journal_path(output: &Path, run: &RunManifest) -> PathBuf {
    artifact_run_root(output, run).join("watch-journal.json")
}

/// Relative product path beneath a run root:
/// `<domain>/<valid-time>/<product>.<extension>`.
pub fn artifact_relative_path(
    domain: &str,
    valid_time: &str,
    product: &str,
    extension: &str,
) -> Result<PathBuf, String> {
    let domain = domain_segment(domain)?;
    let valid_time = valid_time_segment(valid_time)?;
    let product = slug(product);
    let extension = extension.trim_start_matches('.').to_ascii_lowercase();
    if extension.is_empty()
        || extension.len() > 10
        || !extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        return Err("artifact extension must be 1-10 ASCII letters/digits".into());
    }
    Ok(PathBuf::from(domain)
        .join(valid_time)
        .join(format!("{product}.{extension}")))
}

pub fn domain_segment(domain: &str) -> Result<String, String> {
    let domain = domain.trim().to_ascii_lowercase();
    let digits = domain.strip_prefix('d').ok_or_else(|| {
        format!("WRF domain '{domain}' must use d followed by digits (for example d01)")
    })?;
    if digits.is_empty() || digits.len() > 4 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("WRF domain '{domain}' must use 1-4 digits after d"));
    }
    Ok(format!("d{digits}"))
}

pub fn valid_time_segment(value: &str) -> Result<String, String> {
    let value = value.trim();
    for format in [
        "%Y-%m-%dT%H:%M:%SZ",
        "%Y-%m-%d_%H:%M:%S",
        "%Y-%m-%d_%H_%M_%S",
        "%Y_%m_%d_%H_%M_%S",
    ] {
        if let Ok(time) = NaiveDateTime::parse_from_str(value, format) {
            return Ok(time.format("%Y%m%dT%H%M%SZ").to_string());
        }
    }
    Err(format!(
        "invalid WRF valid time '{value}'; expected an ISO/WRF second-resolution timestamp"
    ))
}

/// Deterministic, Windows-safe artifact segment. Canonical ASCII slugs remain
/// readable; any lossy transformation receives a digest suffix so distinct
/// product names cannot silently overwrite one another.
pub fn slug(value: &str) -> String {
    let original = value.trim();
    let mut output = String::new();
    let mut previous_separator = false;
    let mut lossy = false;
    for character in original.chars() {
        if character.is_ascii_alphanumeric() {
            output.push(character.to_ascii_lowercase());
            previous_separator = false;
            if character.is_ascii_uppercase() {
                lossy = true;
            }
        } else if matches!(character, '-' | '_' | '.') {
            output.push(character);
            previous_separator = false;
        } else {
            lossy = true;
            if !previous_separator && !output.is_empty() {
                output.push('-');
                previous_separator = true;
            }
        }
    }
    while output.ends_with(['-', '_', '.']) {
        output.pop();
        lossy = true;
    }
    while output.starts_with(['-', '_', '.']) {
        output.remove(0);
        lossy = true;
    }

    let digest = short_digest(original.as_bytes());
    if output.is_empty() {
        return format!("item-{digest}");
    }
    if is_windows_reserved(&output) {
        output.insert(0, '_');
        lossy = true;
    }
    if output.len() > MAX_SLUG_BYTES {
        output.truncate(MAX_SLUG_BYTES - digest.len() - 1);
        while output.ends_with(['-', '_', '.']) {
            output.pop();
        }
        return format!("{output}-{digest}");
    }
    if lossy {
        format!("{output}-{digest}")
    } else {
        output
    }
}

fn short_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("{:x}", digest)[..10].to_owned()
}

fn is_windows_reserved(value: &str) -> bool {
    let stem = value.split('.').next().unwrap_or(value);
    matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_product_paths_are_stable() {
        assert_eq!(
            artifact_relative_path(
                "D02",
                "2026-07-22T18:30:00Z",
                "composite_reflectivity",
                ".webp"
            )
            .unwrap(),
            PathBuf::from("d02/20260722T183000Z/composite_reflectivity.webp")
        );
    }

    #[test]
    fn lossy_slugs_are_deterministic_and_collision_resistant() {
        let first = slug("2 m temperature");
        assert_eq!(first, slug("2 m temperature"));
        assert_ne!(first, slug("2-m-temperature"));
        assert!(first.starts_with("2-m-temperature-"));
        assert!(slug("CON").starts_with("_con-"));
        assert!(!slug("***").is_empty());
    }

    #[test]
    fn member_is_part_of_run_root() {
        let run: RunManifest = serde_json::from_value(serde_json::json!({
            "schema_version": crate::run_manifest::RUN_SCHEMA_VERSION,
            "case_id": "case",
            "run_id": "run",
            "member_id": "m01",
            "experiment_track": "wrf-parity",
            "model": "WRF"
        }))
        .unwrap();
        assert_eq!(
            artifact_run_root(Path::new("out"), &run),
            PathBuf::from("out/case/run/m01")
        );
        assert_eq!(
            watch_journal_path(Path::new("out"), &run),
            PathBuf::from("out/case/run/m01/watch-journal.json")
        );
    }
}
