use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const ARTIFACT_SCHEMA_VERSION: &str = "bowecho.wrf.artifacts.v1";

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactStatus {
    Complete,
    #[default]
    Partial,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExperimentTrack {
    WrfParity,
    GpuwmCorrected,
    SaseExperimental,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct BuildIdentity {
    pub version: String,
    pub commit: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProvenanceHashes {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_manifest_sha256: Option<String>,
    #[serde(default, flatten)]
    pub additional: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GridReceipt {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nx: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ny: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nz: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dx_m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dy_m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection: Option<String>,
    #[serde(default)]
    pub projection_parameters: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SourceReceipt {
    pub path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initialization_time: Option<String>,
    #[serde(default)]
    pub valid_times: Vec<String>,
    #[serde(default)]
    pub grid: GridReceipt,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DerivationMethod {
    Direct,
    Derived,
    Interpolated,
    Unavailable,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FiniteStatistics {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum: Option<f64>,
    pub missing_value_count: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ArtifactFileReceipt {
    /// Path beneath the directory containing `artifact-manifest.json`.
    pub relative_path: PathBuf,
    pub mime_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProductReceipt {
    pub name: String,
    pub units: String,
    #[serde(default)]
    pub source_variables: Vec<String>,
    pub derivation: DerivationMethod,
    pub artifact: ArtifactFileReceipt,
    #[serde(default)]
    pub statistics: FiniteStatistics,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct UnavailableProduct {
    pub name: String,
    pub reason: String,
    #[serde(default)]
    pub source_variables: Vec<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct FailureRecord {
    pub stage: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    pub reason: String,
}

/// Compact, versioned handoff from BowEcho to an external transfer/retention
/// orchestrator. Presence here proves only local processing; it never grants
/// BowEcho permission to delete or upload source wrfout files.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ArtifactManifest {
    pub schema_version: String,
    pub status: ArtifactStatus,
    pub case_id: String,
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_id: Option<String>,
    pub experiment_track: ExperimentTrack,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub microphysics_scheme: Option<String>,
    #[serde(default)]
    pub provenance: ProvenanceHashes,
    pub bowecho: BuildIdentity,
    #[serde(default)]
    pub sources: Vec<SourceReceipt>,
    #[serde(default)]
    pub products: Vec<ProductReceipt>,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub unavailable_products: Vec<UnavailableProduct>,
    #[serde(default)]
    pub failures: Vec<FailureRecord>,
}

impl ArtifactManifest {
    pub fn validate_contract(&self) -> Vec<String> {
        let mut failures = Vec::new();
        if self.schema_version != ARTIFACT_SCHEMA_VERSION {
            failures.push(format!(
                "unsupported schema_version '{}'; expected '{ARTIFACT_SCHEMA_VERSION}'",
                self.schema_version
            ));
        }
        for (label, value) in [
            ("case_id", self.case_id.as_str()),
            ("run_id", self.run_id.as_str()),
            ("model", self.model.as_str()),
            ("bowecho.version", self.bowecho.version.as_str()),
            ("bowecho.commit", self.bowecho.commit.as_str()),
        ] {
            if value.trim().is_empty() {
                failures.push(format!("required field {label} is empty"));
            }
        }
        if self.sources.is_empty() {
            failures.push("manifest declares no source wrfout receipts".into());
        }
        for source in &self.sources {
            if source.path.as_os_str().is_empty() {
                failures.push("source receipt has an empty path".into());
            }
            if !valid_sha256(&source.sha256) {
                failures.push(format!(
                    "source {} has an invalid SHA-256",
                    source.path.display()
                ));
            }
        }
        for product in &self.products {
            if product.name.trim().is_empty() {
                failures.push("product receipt has an empty name".into());
            }
            if product.artifact.relative_path.is_absolute() {
                failures.push(format!(
                    "product {} artifact path must be relative",
                    product.name
                ));
            }
            if !valid_sha256(&product.artifact.sha256) {
                failures.push(format!(
                    "product {} has an invalid artifact SHA-256",
                    product.name
                ));
            }
        }
        failures
    }
}

pub fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_serializes_with_exact_version_and_track_names() {
        let manifest = ArtifactManifest {
            schema_version: ARTIFACT_SCHEMA_VERSION.into(),
            status: ArtifactStatus::Complete,
            case_id: "case".into(),
            run_id: "run".into(),
            member_id: None,
            experiment_track: ExperimentTrack::GpuwmCorrected,
            model: "GPUWM WRF".into(),
            microphysics_scheme: None,
            provenance: ProvenanceHashes::default(),
            bowecho: BuildIdentity {
                version: "0.34.5".into(),
                commit: "abc123".into(),
            },
            sources: vec![SourceReceipt {
                path: "wrfout_d01".into(),
                bytes: 3,
                sha256: "a".repeat(64),
                ..SourceReceipt::default()
            }],
            products: vec![],
            warnings: vec![],
            unavailable_products: vec![],
            failures: vec![],
        };
        let json = serde_json::to_value(&manifest).unwrap();
        assert_eq!(json["schema_version"], ARTIFACT_SCHEMA_VERSION);
        assert_eq!(json["experiment_track"], "gpuwm-corrected");
        assert!(manifest.validate_contract().is_empty());
    }

    #[test]
    fn contract_rejects_wrong_schema_and_bad_hashes() {
        let manifest: ArtifactManifest = serde_json::from_value(serde_json::json!({
            "schema_version": "future",
            "status": "partial",
            "case_id": "case",
            "run_id": "run",
            "experiment_track": "wrf-parity",
            "model": "WRF",
            "bowecho": {"version": "0.34.5", "commit": "abc"},
            "sources": [{"path": "wrfout", "bytes": 1, "sha256": "bad"}]
        }))
        .unwrap();
        assert_eq!(manifest.validate_contract().len(), 2);
    }
}
