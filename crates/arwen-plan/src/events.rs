// SPDX-License-Identifier: Apache-2.0

//! The `gpuwm.run-plan.event.v1` JSONL event stream — field names match
//! gpuwm/runplan.py's emit sites exactly (EVENT_TAGS, RunObserver, and
//! `_execute`'s plan_accepted/resolved_plan/completed/failed payloads).
//!
//! Envelope: `{schema_version, sequence, emitted_unix_ms, event, ...}`,
//! one JSON object per line, appended to `<run_dir>/events.jsonl` and
//! mirrored verbatim to stdout. `sequence` is monotonic AND DENSE,
//! starting at 1 — a gap means a lost or reordered line and the consumer
//! must refuse it (the engine's own reader does; see
//! [`SequenceGate`]).

use serde::{Deserialize, Serialize};

pub const EVENT_SCHEMA: &str = "gpuwm.run-plan.event.v1";

/// The five run stages, in order. `fetch` appears only when the plan
/// declares one; every other stage always emits its pair.
pub const STAGES: [&str; 5] = ["fetch", "prepare", "initialize", "forecast", "finalize"];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunEventEnvelope {
    pub schema_version: String,
    /// Monotonic and dense, starting at 1.
    pub sequence: u64,
    pub emitted_unix_ms: u64,
    #[serde(flatten)]
    pub event: RunEvent,
}

/// One entry of `automatic_resolutions`: a value the pipeline chose on
/// its own. Extra fields (e.g. `exact`, `grid_id`) ride in `extra` so the
/// review sheet can render everything the engine said.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Resolution {
    pub scope: String,
    pub key: String,
    #[serde(default)]
    pub value: serde_json::Value,
    pub basis: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum RunEvent {
    /// First line, always.
    PlanAccepted {
        name: String,
        route: String,
        #[serde(default)]
        plan_source: Option<String>,
        #[serde(default)]
        plan_sha256: Option<String>,
        run_dir: String,
        #[serde(default)]
        manifest_path: Option<String>,
        #[serde(default)]
        events_path: Option<String>,
        #[serde(default)]
        pid: Option<u32>,
        run_id: String,
    },
    /// After the config loads, before any device work.
    ResolvedPlan {
        /// `{"experiment": {...}, "case_data": {...}}` — the objects the
        /// model will actually run.
        configuration: serde_json::Value,
        #[serde(default)]
        automatic_resolutions: Vec<Resolution>,
        #[serde(default)]
        config_sha256: Option<String>,
        #[serde(default)]
        config_source: Option<String>,
        #[serde(default)]
        run_options: Option<serde_json::Value>,
        /// Intent plans: the wizard-written TOML, verbatim — the one
        /// thing the caller never typed, so it must be shown.
        #[serde(default)]
        generated_config: Option<String>,
        #[serde(default)]
        inputs_present: Option<bool>,
        #[serde(default)]
        declared_inputs: Option<serde_json::Value>,
        #[serde(default)]
        domain_size_floor: Option<serde_json::Value>,
    },
    StageStarted {
        stage: String,
        /// The finer pipeline phase that opened the stage.
        #[serde(default)]
        phase: Option<String>,
    },
    StageFinished {
        stage: String,
        #[serde(default)]
        wall_seconds: Option<f64>,
        /// Ordered list of pipeline phases the stage passed through.
        #[serde(default)]
        phases: Vec<String>,
        #[serde(default)]
        receipts: Option<serde_json::Value>,
        /// `"failed"` when the stage was closed by an error path.
        #[serde(default)]
        outcome: Option<String>,
    },
    /// Each outer step.
    ModelProgress {
        #[serde(default)]
        domain: Option<u32>,
        #[serde(default)]
        outer_step: Option<u64>,
        model_seconds: f64,
        #[serde(default)]
        wall_seconds: Option<f64>,
        /// Null (not infinity) when no wall time has elapsed yet.
        #[serde(default)]
        speed_x: Option<f64>,
        #[serde(default)]
        step_ms: Option<f64>,
        #[serde(default)]
        phase: Option<String>,
        #[serde(default)]
        last_checkpoint: Option<String>,
        /// `"stage_progress_file"` when the sample was polled from a
        /// `prepared`-route subprocess's progress file (then `step_ms`
        /// is null) rather than emitted per outer step. The UI labels
        /// polled samples.
        #[serde(default)]
        source: Option<String>,
    },
    /// A wrfout is genuinely durable on disk (fsynced, self-validated,
    /// renamed onto its final name).
    OutputCommitted {
        #[serde(default)]
        domain: Option<u32>,
        /// `datetime.isoformat()` — may be naive (no `Z`); parse as UTC.
        valid_time: String,
        path: String,
    },
    Warning {
        #[serde(default)]
        code: Option<String>,
        message: String,
        #[serde(default)]
        detail: Option<String>,
        /// Anything else the warning carried (phase, stage, …).
        #[serde(default, flatten)]
        extra: serde_json::Map<String, serde_json::Value>,
    },
    /// Last line, exit 0.
    Completed {
        #[serde(default)]
        dry_run: Option<bool>,
        #[serde(default)]
        run_dir: Option<String>,
        #[serde(default)]
        receipt_path: Option<String>,
        #[serde(default)]
        receipts: Option<serde_json::Value>,
        #[serde(default)]
        outputs_committed: Option<u64>,
        #[serde(default)]
        summary: Option<serde_json::Value>,
    },
    /// Last line, nonzero exit.
    Failed {
        #[serde(default)]
        stage: Option<String>,
        #[serde(default)]
        error_class: Option<String>,
        message: String,
        #[serde(default)]
        remedy: Option<String>,
        #[serde(default)]
        run_dir: Option<String>,
        #[serde(default)]
        receipts: Option<serde_json::Value>,
    },
    /// A tag from a newer engine. Ignored, never fatal.
    #[serde(other)]
    Unknown,
}

impl RunEvent {
    /// Terminal events end the stream.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed { .. } | Self::Failed { .. })
    }
}

/// Parse one JSONL line. Blank lines yield `None`; a malformed line is an
/// error the caller reports (a torn final line during a live tail means
/// the writer died mid-flush — the tail layer holds partials back).
pub fn parse_event_line(line: &str) -> Result<Option<RunEventEnvelope>, String> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(line)
        .map(Some)
        .map_err(|error| format!("malformed run event line: {error}"))
}

/// Dense-sequence enforcement, matching the engine's own reader: a gap
/// means a lost or reordered line, never a skipped one, and the stream
/// must be refused from that point.
#[derive(Debug, Default)]
pub struct SequenceGate {
    next: u64,
}

impl SequenceGate {
    pub fn new() -> Self {
        Self { next: 1 }
    }

    /// `Ok` when `sequence` is exactly the next expected value.
    pub fn accept(&mut self, sequence: u64) -> Result<(), String> {
        if self.next == 0 {
            self.next = 1;
        }
        if sequence != self.next {
            return Err(format!(
                "event sequence gap: expected {}, got {sequence} — the stream lost or \
                 reordered a line and cannot be trusted from here",
                self.next
            ));
        }
        self.next += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_example_model_progress_parses_field_for_field() {
        let line = r#"{"schema_version":"gpuwm.run-plan.event.v1","sequence":7,"emitted_unix_ms":1786087223348,"event":"model_progress","domain":1,"outer_step":3,"model_seconds":180.0,"wall_seconds":4.21,"speed_x":42.75,"step_ms":1403.2,"phase":"post-d01-sync"}"#;
        let envelope = parse_event_line(line).unwrap().unwrap();
        assert_eq!(envelope.sequence, 7);
        match envelope.event {
            RunEvent::ModelProgress {
                domain,
                outer_step,
                model_seconds,
                speed_x,
                step_ms,
                phase,
                ..
            } => {
                assert_eq!(domain, Some(1));
                assert_eq!(outer_step, Some(3));
                assert_eq!(model_seconds, 180.0);
                assert_eq!(speed_x, Some(42.75));
                assert_eq!(step_ms, Some(1403.2));
                assert_eq!(phase.as_deref(), Some("post-d01-sync"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn null_speed_is_none_not_a_number() {
        let line = r#"{"schema_version":"gpuwm.run-plan.event.v1","sequence":5,"emitted_unix_ms":1,"event":"model_progress","domain":1,"outer_step":1,"model_seconds":60.0,"wall_seconds":0.0,"speed_x":null,"step_ms":null,"phase":"synchronized-step"}"#;
        let envelope = parse_event_line(line).unwrap().unwrap();
        match envelope.event {
            RunEvent::ModelProgress {
                speed_x, step_ms, ..
            } => {
                assert_eq!(speed_x, None);
                assert_eq!(step_ms, None);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn all_nine_engine_tags_parse() {
        let lines = [
            r#"{"schema_version":"gpuwm.run-plan.event.v1","sequence":1,"emitted_unix_ms":1,"event":"plan_accepted","name":"n","route":"experiment","plan_source":"p.json","plan_sha256":"aa","run_dir":"d","manifest_path":"m","events_path":"e","pid":7,"run_id":"r"}"#,
            r#"{"schema_version":"gpuwm.run-plan.event.v1","sequence":2,"emitted_unix_ms":2,"event":"resolved_plan","configuration":{"experiment":{}},"automatic_resolutions":[{"scope":"execution","key":"execution_mode","value":"in_process","basis":"front_door_contract"}],"config_sha256":"bb","config_source":"s","run_options":{}}"#,
            r#"{"schema_version":"gpuwm.run-plan.event.v1","sequence":3,"emitted_unix_ms":3,"event":"stage_started","stage":"prepare","phase":"prepare-case"}"#,
            r#"{"schema_version":"gpuwm.run-plan.event.v1","sequence":4,"emitted_unix_ms":4,"event":"stage_finished","stage":"prepare","wall_seconds":3.5,"phases":["prepare-case"]}"#,
            r#"{"schema_version":"gpuwm.run-plan.event.v1","sequence":5,"emitted_unix_ms":5,"event":"model_progress","domain":1,"outer_step":1,"model_seconds":60.0,"wall_seconds":1.0,"speed_x":60.0,"step_ms":1000.0,"phase":"synchronized-step"}"#,
            r#"{"schema_version":"gpuwm.run-plan.event.v1","sequence":6,"emitted_unix_ms":6,"event":"output_committed","domain":1,"valid_time":"2026-08-07T13:00:00","path":"wrfout_d01"}"#,
            r#"{"schema_version":"gpuwm.run-plan.event.v1","sequence":7,"emitted_unix_ms":7,"event":"warning","code":"library_warning","message":"m","detail":"why"}"#,
            r#"{"schema_version":"gpuwm.run-plan.event.v1","sequence":8,"emitted_unix_ms":8,"event":"completed","dry_run":false,"run_dir":"d","receipt_path":"r","receipts":{},"outputs_committed":25,"summary":{}}"#,
            r#"{"schema_version":"gpuwm.run-plan.event.v1","sequence":9,"emitted_unix_ms":9,"event":"failed","stage":"prepare","error_class":"ValueError","message":"m","remedy":null,"run_dir":"d","receipts":{}}"#,
        ];
        let mut terminal = 0;
        for line in lines {
            let envelope = parse_event_line(line).unwrap().unwrap();
            assert!(!matches!(envelope.event, RunEvent::Unknown), "{line}");
            if envelope.event.is_terminal() {
                terminal += 1;
            }
        }
        assert_eq!(terminal, 2);
    }

    #[test]
    fn unknown_tag_parses_as_unknown_and_extra_fields_are_tolerated() {
        let line = r#"{"schema_version":"gpuwm.run-plan.event.v1","sequence":9,"emitted_unix_ms":9,"event":"nest_spawned","grid_id":4}"#;
        assert_eq!(
            parse_event_line(line).unwrap().unwrap().event,
            RunEvent::Unknown
        );
        let line = r#"{"schema_version":"gpuwm.run-plan.event.v1","sequence":2,"emitted_unix_ms":3,"event":"warning","code":"unmapped_pipeline_phase","message":"m","phase":"new-phase","stage":"prepare"}"#;
        match parse_event_line(line).unwrap().unwrap().event {
            RunEvent::Warning { extra, .. } => {
                assert_eq!(extra.get("phase"), Some(&serde_json::json!("new-phase")));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn sequence_gate_is_dense_from_one() {
        let mut gate = SequenceGate::new();
        gate.accept(1).unwrap();
        gate.accept(2).unwrap();
        let error = gate.accept(4).unwrap_err();
        assert!(error.contains("expected 3"), "{error}");
    }

    #[test]
    fn blank_lines_are_skipped_and_garbage_is_an_error() {
        assert_eq!(parse_event_line("   ").unwrap(), None);
        assert!(parse_event_line("{not json").is_err());
    }
}
