// SPDX-License-Identifier: Apache-2.0

//! The `gpuwm.run-progress/v1` heartbeat gpuwm's supervisor publishes
//! atomically as `run-progress.json` in the run directory.
//!
//! Field set matches gpuwm/supervisor.py `_HEARTBEAT_FIELDS` exactly:
//! schema, run_id, config_digest, pid, started_at_utc, updated_at_utc,
//! status, model_elapsed_seconds, outer_step, last_durable_wrfout,
//! last_checkpoint. Reattach reads THIS file for current state and replays
//! events.jsonl for history — never the pipe.

use serde::{Deserialize, Serialize};

pub const HEARTBEAT_SCHEMA: &str = "gpuwm.run-progress/v1";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunProgress {
    pub schema: String,
    pub run_id: String,
    #[serde(default)]
    pub config_digest: Option<String>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub started_at_utc: Option<String>,
    #[serde(default)]
    pub updated_at_utc: Option<String>,
    /// Supervisor phase string (e.g. running / completed / failed families;
    /// Studio treats it as display text plus the liveness checks below).
    pub status: String,
    #[serde(default)]
    pub model_elapsed_seconds: Option<f64>,
    #[serde(default)]
    pub outer_step: Option<u64>,
    #[serde(default)]
    pub last_durable_wrfout: Option<String>,
    #[serde(default)]
    pub last_checkpoint: Option<String>,
    /// Newer supervisor fields survive a round-trip.
    #[serde(default, flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Where a heartbeat `status` sits in the run-plan STAGE vocabulary.
///
/// The heartbeat's status vocabulary (`integrating` / `complete` /
/// `failed` / `preparing:<phase>`) is gpuwm.supervisor's own and CANNOT
/// be extended by consumers — Studio carries this mapping instead. The
/// phase→stage table MIRRORS gpuwm/runplan.py `_PHASE_STAGES` (the
/// source of truth; update from there, never invent an entry: an
/// unmapped phase is reported as `Unmapped`, exactly as the engine
/// emits an `unmapped_pipeline_phase` warning rather than mis-filing).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HeartbeatStage {
    /// A run-plan stage name (`prepare` / `initialize` / `forecast`).
    Stage(&'static str),
    Completed,
    Failed,
    /// A `preparing:<phase>` whose phase is not in the mirrored table.
    Unmapped(String),
    /// A status string this Studio does not know at all.
    Unknown(String),
}

/// Mirrors gpuwm/runplan.py `_PHASE_STAGES` @ wt-runplan 795999cc8.
const PHASE_STAGES: &[(&str, &str)] = &[
    ("quarantine-wrfout", "prepare"),
    ("resolve-schedule", "prepare"),
    ("prepare-case", "prepare"),
    ("build-domain-tree", "prepare"),
    ("initialize-health-validator", "initialize"),
    ("cold-start-wrfout", "initialize"),
    ("validate-checkpoint", "initialize"),
    ("restore-checkpoint", "initialize"),
    ("restore-tree-checkpoint", "initialize"),
    ("initialize-domain-writers", "initialize"),
    ("initial-health-gate", "initialize"),
];

pub fn interpret_heartbeat_status(status: &str) -> HeartbeatStage {
    match status {
        "integrating" => HeartbeatStage::Stage("forecast"),
        "complete" => HeartbeatStage::Completed,
        "failed" => HeartbeatStage::Failed,
        other => {
            if let Some(phase) = other.strip_prefix("preparing:") {
                for (name, stage) in PHASE_STAGES {
                    if *name == phase {
                        return HeartbeatStage::Stage(stage);
                    }
                }
                HeartbeatStage::Unmapped(phase.to_string())
            } else {
                HeartbeatStage::Unknown(other.to_string())
            }
        }
    }
}

impl RunProgress {
    pub fn parse(text: &str) -> Result<Self, String> {
        let progress: Self = serde_json::from_str(text)
            .map_err(|error| format!("malformed run-progress.json: {error}"))?;
        if progress.schema != HEARTBEAT_SCHEMA {
            return Err(format!(
                "run-progress schema {:?} is not {HEARTBEAT_SCHEMA:?}",
                progress.schema
            ));
        }
        Ok(progress)
    }

    /// Seconds since the supervisor last updated the heartbeat, from the
    /// embedded timestamp. `None` when the timestamp is absent/unparseable
    /// (callers fall back to file mtime).
    pub fn staleness_seconds(&self, now_utc: chrono::DateTime<chrono::Utc>) -> Option<f64> {
        let updated = self.updated_at_utc.as_deref()?;
        let updated = chrono::DateTime::parse_from_rfc3339(updated).ok()?;
        Some((now_utc - updated.with_timezone(&chrono::Utc)).num_milliseconds() as f64 / 1000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "schema": "gpuwm.run-progress/v1",
        "run_id": "afternoon-run-20260807T120000Z",
        "config_digest": "abc123",
        "pid": 4242,
        "started_at_utc": "2026-08-07T12:00:05Z",
        "updated_at_utc": "2026-08-07T12:20:00Z",
        "status": "integrating",
        "model_elapsed_seconds": 3600.0,
        "outer_step": 240,
        "last_durable_wrfout": "wrfout_d01_2026-08-07_13_00_00",
        "last_checkpoint": null
    }"#;

    #[test]
    fn heartbeat_status_maps_through_the_mirrored_table() {
        assert_eq!(
            interpret_heartbeat_status("integrating"),
            HeartbeatStage::Stage("forecast")
        );
        assert_eq!(
            interpret_heartbeat_status("complete"),
            HeartbeatStage::Completed
        );
        assert_eq!(interpret_heartbeat_status("failed"), HeartbeatStage::Failed);
        assert_eq!(
            interpret_heartbeat_status("preparing:prepare-case"),
            HeartbeatStage::Stage("prepare")
        );
        assert_eq!(
            interpret_heartbeat_status("preparing:cold-start-wrfout"),
            HeartbeatStage::Stage("initialize")
        );
        assert_eq!(
            interpret_heartbeat_status("preparing:brand-new-phase"),
            HeartbeatStage::Unmapped("brand-new-phase".into())
        );
        assert_eq!(
            interpret_heartbeat_status("hibernating"),
            HeartbeatStage::Unknown("hibernating".into())
        );
    }

    #[test]
    fn parses_the_supervisor_field_set() {
        let progress = RunProgress::parse(SAMPLE).unwrap();
        assert_eq!(progress.status, "integrating");
        assert_eq!(progress.pid, Some(4242));
        assert_eq!(progress.outer_step, Some(240));
        assert_eq!(
            progress.last_durable_wrfout.as_deref(),
            Some("wrfout_d01_2026-08-07_13_00_00")
        );
        assert_eq!(progress.last_checkpoint, None);
    }

    #[test]
    fn wrong_schema_is_rejected() {
        let text = SAMPLE.replace("gpuwm.run-progress/v1", "gpuwm.run-progress/v2");
        assert!(RunProgress::parse(&text).is_err());
    }

    #[test]
    fn staleness_uses_the_embedded_timestamp() {
        let progress = RunProgress::parse(SAMPLE).unwrap();
        let now = chrono::DateTime::parse_from_rfc3339("2026-08-07T12:20:30Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let staleness = progress.staleness_seconds(now).unwrap();
        assert!((staleness - 30.0).abs() < 0.001, "{staleness}");
    }
}
