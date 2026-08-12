// SPDX-License-Identifier: Apache-2.0

//! `<run_dir>/run-manifest.json` (`gpuwm.run-manifest.v1`): written before
//! any work starts, names every stream a consumer may want. Reattach
//! starts here: manifest → heartbeat (current state) → events replay
//! (history) → tail (live detail).

use serde::{Deserialize, Serialize};

pub const MANIFEST_SCHEMA: &str = "gpuwm.run-manifest.v1";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunManifest {
    pub schema: String,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub started_at_utc: Option<String>,
    #[serde(default)]
    pub plan_sha256: Option<String>,
    #[serde(default)]
    pub run_dir: Option<String>,
    #[serde(default)]
    pub outputs_dir: Option<String>,
    #[serde(default)]
    pub events_path: Option<String>,
    /// `gpuwm.run-plan.event.v1`
    #[serde(default)]
    pub events_schema: Option<String>,
    #[serde(default)]
    pub progress_path: Option<String>,
    /// `gpuwm.run-progress/v1` — legacy convention, opaque and exact.
    #[serde(default)]
    pub progress_schema: Option<String>,
    #[serde(default)]
    pub failure_capsule_path: Option<String>,
    /// `gpuwm.failure-capsule/v3` — legacy convention, opaque and exact.
    #[serde(default)]
    pub failure_capsule_schema: Option<String>,
    #[serde(default, flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl RunManifest {
    pub fn parse(text: &str) -> Result<Self, String> {
        let manifest: Self = serde_json::from_str(text)
            .map_err(|error| format!("malformed run-manifest.json: {error}"))?;
        if manifest.schema != MANIFEST_SCHEMA {
            return Err(format!(
                "run-manifest schema {:?} is not {MANIFEST_SCHEMA:?}",
                manifest.schema
            ));
        }
        Ok(manifest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_example_parses() {
        let text = r#"{
            "schema": "gpuwm.run-manifest.v1",
            "pid": 24188,
            "started_at_utc": "2026-08-07T18:00:12.104Z",
            "plan_sha256": "aa",
            "run_dir": "runs/overnight",
            "outputs_dir": "runs/overnight",
            "events_path": "runs/overnight/events.jsonl",
            "events_schema": "gpuwm.run-plan.event.v1",
            "progress_path": "runs/overnight/run-progress.json",
            "progress_schema": "gpuwm.run-progress/v1",
            "failure_capsule_path": "runs/overnight/failure-capsule.json",
            "failure_capsule_schema": "gpuwm.failure-capsule/v3"
        }"#;
        let manifest = RunManifest::parse(text).unwrap();
        assert_eq!(manifest.pid, Some(24188));
        assert_eq!(
            manifest.progress_schema.as_deref(),
            Some("gpuwm.run-progress/v1")
        );
        assert!(RunManifest::parse("{\"schema\":\"other\"}").is_err());
    }
}
