// SPDX-License-Identifier: Apache-2.0

//! The fixture-run core: replay a fixture event stream the way the real
//! engine emits it — events.jsonl written in the run directory AND
//! mirrored verbatim to stdout, run-manifest.json written before any
//! work, and a `gpuwm.run-progress/v1` heartbeat maintained with the
//! supervisor's own status vocabulary (`preparing:<phase>` /
//! `integrating` / `complete` / `failed`).
//!
//! This makes the whole run / detach / reattach path exercisable as REAL
//! subprocess behavior before live mode is switched on; Studio-side
//! consumers cannot tell the difference, which is the point.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use arwen_plan::events::RunEvent;
use arwen_plan::heartbeat::HEARTBEAT_SCHEMA;
use arwen_plan::manifest::MANIFEST_SCHEMA;

pub struct ReplayOptions {
    /// Wall-time compression: fixture inter-event gaps are divided by this.
    pub speed: f64,
    /// Hard cap per gap so a fixture can never stall a demo.
    pub max_gap: Duration,
}

impl Default for ReplayOptions {
    fn default() -> Self {
        Self {
            speed: 60.0,
            max_gap: Duration::from_secs(2),
        }
    }
}

/// Replay `fixture_events` into `run_dir` (events.jsonl + manifest +
/// heartbeat) while mirroring every line to `stdout`. Path strings under
/// the fixture's own announced run directory are rewritten under
/// `run_dir` so every self-reference stays true.
pub fn replay_fixture_run(
    fixture_events: &Path,
    run_dir: &Path,
    stdout: &mut dyn Write,
    options: &ReplayOptions,
) -> Result<(), String> {
    let text = std::fs::read_to_string(fixture_events)
        .map_err(|error| format!("read fixture {}: {error}", fixture_events.display()))?;
    std::fs::create_dir_all(run_dir)
        .map_err(|error| format!("create run dir {}: {error}", run_dir.display()))?;

    let lines: Vec<serde_json::Value> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line).map_err(|error| format!("fixture line not JSON: {error}"))
        })
        .collect::<Result<_, _>>()?;

    let fixture_run_dir = lines.iter().find_map(|value| {
        (value["event"] == "plan_accepted").then(|| value["run_dir"].as_str().map(str::to_owned))?
    });
    let new_run_dir = run_dir.to_string_lossy().replace('\\', "/");
    let plan_sha256 = lines
        .iter()
        .find_map(|value| value["plan_sha256"].as_str().map(str::to_owned));

    // The engine writes run-manifest.json before any work starts.
    let manifest = serde_json::json!({
        "schema": MANIFEST_SCHEMA,
        "pid": std::process::id(),
        "started_at_utc": now_utc(),
        "plan_sha256": plan_sha256,
        "run_dir": new_run_dir,
        "outputs_dir": new_run_dir,
        "events_path": format!("{new_run_dir}/events.jsonl"),
        "events_schema": arwen_plan::events::EVENT_SCHEMA,
        "progress_path": format!("{new_run_dir}/run-progress.json"),
        "progress_schema": HEARTBEAT_SCHEMA,
        "failure_capsule_path": format!("{new_run_dir}/failure-capsule.json"),
        "failure_capsule_schema": "gpuwm.failure-capsule/v3",
    });
    std::fs::write(
        run_dir.join("run-manifest.json"),
        serde_json::to_vec_pretty(&manifest).expect("serialize manifest"),
    )
    .map_err(|error| format!("write run-manifest.json: {error}"))?;

    let mut events_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(run_dir.join("events.jsonl"))
        .map_err(|error| format!("open run events.jsonl: {error}"))?;

    let mut heartbeat = HeartbeatState::default();
    let mut previous_ms: Option<u64> = None;
    for mut value in lines {
        if let Some(prefix) = &fixture_run_dir {
            rewrite_path_strings(&mut value, prefix, &new_run_dir);
        }
        let ms = value["emitted_unix_ms"].as_u64().unwrap_or_default();
        if let Some(previous) = previous_ms {
            let gap_ms = ms.saturating_sub(previous) as f64 / options.speed.max(1e-6);
            let gap = Duration::from_millis(gap_ms as u64).min(options.max_gap);
            if !gap.is_zero() {
                std::thread::sleep(gap);
            }
        }
        previous_ms = Some(ms);

        let line = serde_json::to_string(&value)
            .map_err(|error| format!("re-serialize fixture event: {error}"))?;
        writeln!(events_file, "{line}")
            .and_then(|()| events_file.flush())
            .map_err(|error| format!("append run events.jsonl: {error}"))?;
        writeln!(stdout, "{line}").map_err(|error| format!("write event: {error}"))?;
        stdout
            .flush()
            .map_err(|error| format!("flush event: {error}"))?;

        heartbeat.absorb(&value);
        heartbeat.publish(run_dir)?;
    }
    Ok(())
}

fn rewrite_path_strings(value: &mut serde_json::Value, prefix: &str, replacement: &str) {
    match value {
        serde_json::Value::String(text) => {
            if text.starts_with(prefix) {
                *text = format!("{replacement}{}", &text[prefix.len()..]);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                rewrite_path_strings(item, prefix, replacement);
            }
        }
        serde_json::Value::Object(map) => {
            for (_, item) in map.iter_mut() {
                rewrite_path_strings(item, prefix, replacement);
            }
        }
        _ => {}
    }
}

/// Heartbeat synthesis in gpuwm.supervisor's own status vocabulary —
/// `preparing:<phase>`, `integrating`, `complete`, `failed`. Nothing is
/// published until a status exists (the real heartbeat appears once the
/// supervisor starts, not at plan acceptance).
#[derive(Default)]
struct HeartbeatState {
    run_id: String,
    config_digest: Option<String>,
    status: String,
    started_at_utc: Option<String>,
    model_elapsed_seconds: Option<f64>,
    outer_step: Option<u64>,
    last_durable_wrfout: Option<String>,
    last_checkpoint: Option<String>,
}

impl HeartbeatState {
    fn absorb(&mut self, value: &serde_json::Value) {
        let Ok(envelope) =
            serde_json::from_value::<arwen_plan::events::RunEventEnvelope>(value.clone())
        else {
            return;
        };
        match envelope.event {
            RunEvent::PlanAccepted { run_id, .. } => {
                self.run_id = run_id;
                self.started_at_utc = Some(now_utc());
            }
            RunEvent::ResolvedPlan { config_sha256, .. } => {
                self.config_digest = config_sha256;
            }
            RunEvent::StageStarted {
                phase: Some(phase), ..
            } => self.status = format!("preparing:{phase}"),
            RunEvent::StageStarted { phase: None, .. } => {}
            RunEvent::ModelProgress {
                model_seconds,
                outer_step,
                last_checkpoint,
                ..
            } => {
                self.status = "integrating".into();
                self.model_elapsed_seconds = Some(model_seconds);
                self.outer_step = outer_step;
                if last_checkpoint.is_some() {
                    self.last_checkpoint = last_checkpoint;
                }
            }
            RunEvent::OutputCommitted { path, .. } => {
                self.last_durable_wrfout = Some(path);
            }
            RunEvent::Completed { .. } => self.status = "complete".into(),
            RunEvent::Failed { .. } => self.status = "failed".into(),
            _ => {}
        }
    }

    fn publish(&self, run_dir: &Path) -> Result<(), String> {
        if self.run_id.is_empty() || self.status.is_empty() {
            return Ok(());
        }
        let payload = serde_json::json!({
            "schema": HEARTBEAT_SCHEMA,
            "run_id": self.run_id,
            "config_digest": self.config_digest,
            "pid": std::process::id(),
            "started_at_utc": self.started_at_utc,
            "updated_at_utc": now_utc(),
            "status": self.status,
            "model_elapsed_seconds": self.model_elapsed_seconds,
            "outer_step": self.outer_step,
            "last_durable_wrfout": self.last_durable_wrfout,
            "last_checkpoint": self.last_checkpoint,
        });
        let path = run_dir.join("run-progress.json");
        let temp = run_dir.join(format!(".run-progress.tmp.{}", std::process::id()));
        std::fs::write(
            &temp,
            serde_json::to_vec(&payload).expect("serialize heartbeat"),
        )
        .map_err(|error| format!("write heartbeat temp: {error}"))?;
        std::fs::rename(&temp, &path).map_err(|error| format!("publish heartbeat: {error}"))
    }
}

fn now_utc() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arwen_plan::events::parse_event_line;
    use arwen_plan::heartbeat::RunProgress;
    use arwen_plan::manifest::RunManifest;
    use std::path::PathBuf;

    fn fixtures_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
    }

    fn temp_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "arwen-proc-replay-{label}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ))
    }

    #[test]
    fn replays_the_happy_fixture_with_manifest_events_and_heartbeat() {
        let run_dir = temp_dir("happy");
        let mut sink = Vec::new();
        replay_fixture_run(
            &fixtures_dir().join("events.jsonl"),
            &run_dir,
            &mut sink,
            &ReplayOptions {
                speed: 1e9,
                max_gap: Duration::ZERO,
            },
        )
        .unwrap();

        let text = String::from_utf8(sink).unwrap();
        let events: Vec<_> = text
            .lines()
            .filter_map(|line| parse_event_line(line).unwrap())
            .collect();
        assert_eq!(events.len(), 63);
        assert!(events.last().unwrap().event.is_terminal());

        // Self-references rewritten to where the run actually lives.
        let run_dir_text = run_dir.to_string_lossy().replace('\\', "/");
        assert!(text.contains(&format!("\"run_dir\":\"{run_dir_text}\"")));
        assert!(text.contains(&format!("{run_dir_text}/wrfout_d01_")));
        assert!(!text.contains("C:/Forecasts/afternoon-run"));

        // Engine parity: events.jsonl in the run dir mirrors stdout.
        let on_disk = std::fs::read_to_string(run_dir.join("events.jsonl")).unwrap();
        assert_eq!(on_disk, text);

        // The manifest points at the streams it wrote.
        let manifest = RunManifest::parse(
            &std::fs::read_to_string(run_dir.join("run-manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            manifest.events_path.as_deref(),
            Some(format!("{run_dir_text}/events.jsonl").as_str())
        );

        // Heartbeat ends complete in the supervisor vocabulary.
        let progress = RunProgress::parse(
            &std::fs::read_to_string(run_dir.join("run-progress.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(progress.status, "complete");
        assert_eq!(progress.model_elapsed_seconds, Some(21_600.0));
        assert!(
            progress
                .last_durable_wrfout
                .unwrap()
                .starts_with(&run_dir_text)
        );

        std::fs::remove_dir_all(&run_dir).unwrap();
    }

    #[test]
    fn failure_fixture_ends_with_failed_heartbeat() {
        let run_dir = temp_dir("failure");
        let mut sink = Vec::new();
        replay_fixture_run(
            &fixtures_dir().join("events-failure.jsonl"),
            &run_dir,
            &mut sink,
            &ReplayOptions {
                speed: 1e9,
                max_gap: Duration::ZERO,
            },
        )
        .unwrap();
        let progress = RunProgress::parse(
            &std::fs::read_to_string(run_dir.join("run-progress.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(progress.status, "failed");
        std::fs::remove_dir_all(&run_dir).unwrap();
    }
}
