use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::CliError;
use crate::artifact::{
    ARTIFACT_SCHEMA_VERSION, ArtifactManifest, ArtifactStatus, BuildIdentity, ExperimentTrack,
    ProvenanceHashes,
};
use crate::fs::sha256_reader;

pub const RUN_SCHEMA_VERSION: &str = "bowecho.wrf.run.v1";
const MAX_RUN_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunProvenance {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table_commit: Option<String>,
}

/// Forecast identity and provenance supplied by the CPU-WRF/GPUWM runner.
/// Unknown fields are rejected so a misspelled commit key cannot silently
/// disappear from the artifact chain.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunManifest {
    pub schema_version: String,
    pub case_id: String,
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_id: Option<String>,
    pub experiment_track: ExperimentTrack,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub microphysics_scheme: Option<String>,
    #[serde(default)]
    pub provenance: RunProvenance,
}

impl RunManifest {
    pub fn validate(&self) -> Vec<String> {
        let mut failures = Vec::new();
        if self.schema_version != RUN_SCHEMA_VERSION {
            failures.push(format!(
                "unsupported schema_version '{}'; expected '{RUN_SCHEMA_VERSION}'",
                self.schema_version
            ));
        }
        validate_identifier("case_id", &self.case_id, &mut failures);
        validate_identifier("run_id", &self.run_id, &mut failures);
        if let Some(member_id) = &self.member_id {
            validate_identifier("member_id", member_id, &mut failures);
        }
        validate_text("model", &self.model, 256, &mut failures);
        if let Some(scheme) = &self.microphysics_scheme {
            validate_text("microphysics_scheme", scheme, 256, &mut failures);
        }
        for (name, value) in [
            ("source_commit", self.provenance.source_commit.as_deref()),
            ("config_commit", self.provenance.config_commit.as_deref()),
            ("input_commit", self.provenance.input_commit.as_deref()),
            ("table_commit", self.provenance.table_commit.as_deref()),
        ] {
            if let Some(value) = value
                && !valid_commit_hash(value)
            {
                failures.push(format!(
                    "provenance.{name} must be 7-64 hexadecimal characters"
                ));
            }
        }
        failures
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LoadedRunManifest {
    pub path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
    pub manifest: RunManifest,
}

impl LoadedRunManifest {
    /// Seed the v1 artifact contract. The host adds source/product receipts and
    /// sets the final status after the existing BowEcho renderer completes.
    pub fn artifact_manifest_shell(&self, bowecho: BuildIdentity) -> ArtifactManifest {
        ArtifactManifest {
            schema_version: ARTIFACT_SCHEMA_VERSION.into(),
            status: ArtifactStatus::Partial,
            case_id: self.manifest.case_id.clone(),
            run_id: self.manifest.run_id.clone(),
            member_id: self.manifest.member_id.clone(),
            experiment_track: self.manifest.experiment_track.clone(),
            model: self.manifest.model.clone(),
            microphysics_scheme: self.manifest.microphysics_scheme.clone(),
            provenance: ProvenanceHashes {
                source_commit: self.manifest.provenance.source_commit.clone(),
                config_commit: self.manifest.provenance.config_commit.clone(),
                input_commit: self.manifest.provenance.input_commit.clone(),
                table_commit: self.manifest.provenance.table_commit.clone(),
                run_manifest_sha256: Some(self.sha256.clone()),
                additional: Default::default(),
            },
            bowecho,
            sources: vec![],
            products: vec![],
            warnings: vec![],
            unavailable_products: vec![],
            failures: vec![],
        }
    }
}

pub fn load_run_manifest(path: &Path) -> Result<LoadedRunManifest, CliError> {
    let metadata = fs::metadata(path).map_err(|error| {
        CliError::input(format!(
            "cannot stat run manifest {}: {error}",
            path.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(CliError::input(format!(
            "run manifest is not a regular file: {}",
            path.display()
        )));
    }
    if metadata.len() > MAX_RUN_MANIFEST_BYTES {
        return Err(CliError::data(format!(
            "run manifest is {} bytes; maximum is {MAX_RUN_MANIFEST_BYTES}",
            metadata.len()
        )));
    }
    let bytes = fs::read(path).map_err(|error| {
        CliError::input(format!(
            "cannot read run manifest {}: {error}",
            path.display()
        ))
    })?;
    let receipt = sha256_reader(&bytes[..]).map_err(|error| {
        CliError::input(format!(
            "cannot hash run manifest {}: {error}",
            path.display()
        ))
    })?;
    let manifest: RunManifest = serde_json::from_slice(&bytes).map_err(|error| {
        CliError::data(format!("invalid run manifest {}: {error}", path.display()))
    })?;
    let failures = manifest.validate();
    if !failures.is_empty() {
        return Err(CliError::data(format!(
            "invalid run manifest {}: {}",
            path.display(),
            failures.join("; ")
        )));
    }
    Ok(LoadedRunManifest {
        path: path.to_path_buf(),
        bytes: receipt.bytes,
        sha256: receipt.sha256,
        manifest,
    })
}

fn validate_identifier(name: &str, value: &str, failures: &mut Vec<String>) {
    let bytes = value.as_bytes();
    let valid = (1..=128).contains(&bytes.len())
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        && value != "."
        && value != ".."
        && !value.contains("..")
        && !value.ends_with('.')
        && !is_windows_reserved_identifier(value);
    if !valid {
        failures.push(format!(
            "{name} must be a Windows-safe 1-128 ASCII letters/digits/._- identifier, start alphanumeric, and not contain '..'"
        ));
    }
}

fn is_windows_reserved_identifier(value: &str) -> bool {
    let stem = value
        .split('.')
        .next()
        .unwrap_or(value)
        .to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem.strip_prefix("COM").is_some_and(|number| {
            matches!(number, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
        })
        || stem.strip_prefix("LPT").is_some_and(|number| {
            matches!(number, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
        })
}

fn validate_text(name: &str, value: &str, max_len: usize, failures: &mut Vec<String>) {
    if value.trim().is_empty() || value.len() > max_len || value.chars().any(char::is_control) {
        failures.push(format!(
            "{name} must be non-empty, at most {max_len} bytes, and contain no control characters"
        ));
    }
}

fn valid_commit_hash(value: &str) -> bool {
    (7..=64).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn valid_json() -> serde_json::Value {
        serde_json::json!({
            "schema_version": RUN_SCHEMA_VERSION,
            "case_id": "tornado-1974",
            "run_id": "gpuwm-001",
            "member_id": "m01",
            "experiment_track": "gpuwm-corrected",
            "model": "GPUWM WRF",
            "microphysics_scheme": "P3",
            "provenance": {
                "source_commit": "0123456789abcdef",
                "config_commit": "123456789abcdef0",
                "input_commit": "23456789abcdef01",
                "table_commit": "3456789abcdef012"
            }
        })
    }

    #[test]
    fn loads_valid_run_and_maps_it_into_artifact_v1() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("run.json");
        fs::write(&path, serde_json::to_vec(&valid_json()).unwrap()).unwrap();
        let loaded = load_run_manifest(&path).unwrap();
        let artifact = loaded.artifact_manifest_shell(BuildIdentity {
            version: "0.34.5".into(),
            commit: "abcdef0".into(),
        });
        assert_eq!(artifact.schema_version, ARTIFACT_SCHEMA_VERSION);
        assert_eq!(artifact.case_id, "tornado-1974");
        assert_eq!(artifact.member_id.as_deref(), Some("m01"));
        assert_eq!(artifact.provenance.run_manifest_sha256, Some(loaded.sha256));
    }

    #[test]
    fn rejects_path_traversal_bad_hash_and_unknown_typo() {
        let root = tempfile::tempdir().unwrap();
        let mut value = valid_json();
        value["case_id"] = "../case".into();
        value["provenance"]["source_commit"] = "not-a-hash".into();
        value["provenance"]["source_comit"] = "01234567".into();
        let path = root.path().join("run.json");
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        let error = load_run_manifest(&path).unwrap_err();
        assert_eq!(error.code, crate::ExitCode::Data);
    }

    #[test]
    fn rejects_windows_reserved_or_trailing_dot_identifiers_on_every_platform() {
        for case_id in ["CON", "aux.txt", "case."] {
            let root = tempfile::tempdir().unwrap();
            let mut value = valid_json();
            value["case_id"] = case_id.into();
            let path = root.path().join("run.json");
            fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
            assert_eq!(
                load_run_manifest(&path).unwrap_err().code,
                crate::ExitCode::Data
            );
        }
    }
}
