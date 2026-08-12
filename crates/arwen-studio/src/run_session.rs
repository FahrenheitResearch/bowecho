// SPDX-License-Identifier: Apache-2.0

//! One run, as Studio sees it: the events.jsonl tail folded into pipeline
//! stages, model progress, committed output frames, and warnings, plus
//! the run-progress.json heartbeat for reattach liveness.
//!
//! The SAME type serves a freshly-launched run and a reattached one — the
//! only difference is whether we hold a child handle. Truth lives in the
//! files (heartbeat = current state, events = history); the child handle
//! is only used for optional liveness polling. The event sequence is
//! enforced dense (engine contract): a gap poisons the stream from that
//! point and is surfaced, never smoothed over.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use arwen_map::LambertDomain;
use arwen_plan::events::{Resolution, RunEvent, RunEventEnvelope, SequenceGate};
use arwen_plan::heartbeat::RunProgress;
use arwen_plan::queries::configuration_lambert_geometry;
use arwen_proc::JsonlTail;
use arwen_proc::launcher::LaunchedRun;
use arwen_proc::registry::{LaunchRecord, RunRegistry};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StageStatus {
    Running,
    Ok,
    Failed,
}

#[derive(Clone, Debug)]
pub struct StageState {
    pub id: String,
    pub status: StageStatus,
    pub wall_seconds: Option<f64>,
    /// Ordered pipeline phases seen inside the stage (detail row).
    pub phases: Vec<String>,
    pub outcome: Option<String>,
}

#[derive(Clone, Debug)]
pub struct OutputFrame {
    pub path: PathBuf,
    pub valid_time_utc: chrono::DateTime<chrono::Utc>,
    /// Grid id (multi-domain runs select per-domain lanes, end-game §2.1).
    #[allow(dead_code)]
    pub domain: Option<u32>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Terminal {
    Completed,
    Failed {
        error: String,
        remedy: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ProgressView {
    pub model_seconds: f64,
    /// From the resolved configuration's `experiment.run_seconds`.
    pub run_seconds: Option<f64>,
    pub speed_x: Option<f64>,
    pub outer_step: Option<u64>,
    /// True when the latest sample was polled from a stage progress file
    /// (`prepared` route) rather than emitted per outer step.
    pub polled: bool,
}

impl ProgressView {
    pub fn fraction(&self) -> Option<f64> {
        self.run_seconds
            .filter(|total| *total > 0.0)
            .map(|total| (self.model_seconds / total).clamp(0.0, 1.0))
    }
}

pub struct RunSession {
    /// Registry directory for this run (Studio's own bookkeeping).
    pub dir: PathBuf,
    pub record: LaunchRecord,
    tail: JsonlTail,
    sequence: SequenceGate,
    /// A dense-sequence violation poisons the stream; nothing after it is
    /// applied and the UI shows the reason.
    pub stream_poisoned: Option<String>,
    pub stages: Vec<StageState>,
    pub configuration: Option<serde_json::Value>,
    pub resolutions: Vec<Resolution>,
    /// Root-domain Lambert geometry (outline + hover inverse).
    pub domain: Option<LambertDomain>,
    /// The whole resolved tree (nest outlines, dormant ghosts).
    pub tree: Vec<arwen_plan::queries::ResolvedDomain>,
    /// The relocation trail from `<run_dir>/relocation_receipts.json`
    /// (the runner's own per-move table), parent-cell placements.
    pub trail: Vec<TrailMove>,
    trail_read: Option<Instant>,
    pub run_dir: Option<PathBuf>,
    pub run_id: Option<String>,
    pub progress: ProgressView,
    pub outputs: Vec<OutputFrame>,
    pub warnings: Vec<String>,
    pub stream_errors: Vec<String>,
    pub terminal: Option<Terminal>,
    pub heartbeat: Option<RunProgress>,
    heartbeat_read: Option<Instant>,
    /// Selected valid-time frame; `None` = follow latest.
    pub selected_frame: Option<usize>,
    /// Present only for DieWithStudio launches — HELD, not read: dropping
    /// it closes the kill-on-close Job Object.
    #[allow(dead_code)]
    pub owned_guard: Option<arwen_proc::process::ProcessTreeGuard>,
    /// Held for optional liveness polling; dropping never kills a
    /// SurviveStudio child.
    #[allow(dead_code)]
    pub child: Option<std::process::Child>,
}

impl RunSession {
    pub fn from_launch(launched: LaunchedRun) -> Self {
        let tail = JsonlTail::from_start(RunRegistry::events_path(&launched.dir));
        Self::new(
            launched.dir,
            launched.record,
            tail,
            Some(launched.child),
            launched.guard,
        )
    }

    /// Reattach: replay the whole events file, then keep tailing. The
    /// heartbeat (once run_dir is known) says whether the run is live.
    pub fn reattach(dir: PathBuf, record: LaunchRecord) -> Self {
        let tail = JsonlTail::from_start(RunRegistry::events_path(&dir));
        Self::new(dir, record, tail, None, None)
    }

    fn new(
        dir: PathBuf,
        record: LaunchRecord,
        tail: JsonlTail,
        child: Option<std::process::Child>,
        owned_guard: Option<arwen_proc::process::ProcessTreeGuard>,
    ) -> Self {
        Self {
            dir,
            record,
            tail,
            sequence: SequenceGate::new(),
            stream_poisoned: None,
            stages: Vec::new(),
            configuration: None,
            resolutions: Vec::new(),
            domain: None,
            tree: Vec::new(),
            trail: Vec::new(),
            trail_read: None,
            run_dir: None,
            run_id: None,
            progress: ProgressView::default(),
            outputs: Vec::new(),
            warnings: Vec::new(),
            stream_errors: Vec::new(),
            terminal: None,
            heartbeat: None,
            heartbeat_read: None,
            selected_frame: None,
            owned_guard,
            child,
        }
    }

    pub fn is_finished(&self) -> bool {
        self.terminal.is_some()
    }

    /// Poll the event tail + heartbeat. Returns true when anything
    /// changed (callers repaint).
    pub fn poll(&mut self) -> bool {
        let batch = self.tail.poll();
        let mut changed = !batch.events.is_empty() || !batch.errors.is_empty();
        self.stream_errors.extend(batch.errors);
        for envelope in batch.events {
            if self.stream_poisoned.is_some() {
                break;
            }
            if let Err(error) = self.sequence.accept(envelope.sequence) {
                self.stream_poisoned = Some(error);
                break;
            }
            self.apply(envelope);
        }
        if self.refresh_heartbeat() {
            changed = true;
        }
        if self.refresh_trail() {
            changed = true;
        }
        changed
    }

    /// Read the relocation runner's own receipts file (per-move table),
    /// throttled — it exists only on follow/itinerary runs.
    fn refresh_trail(&mut self) -> bool {
        let due = self
            .trail_read
            .map(|at| at.elapsed() > Duration::from_secs(5))
            .unwrap_or(true);
        if !due {
            return false;
        }
        self.trail_read = Some(Instant::now());
        let Some(run_dir) = &self.run_dir else {
            return false;
        };
        // WHERE THE RECEIPTS ACTUALLY LAND depends on the route. The
        // experiment route's forecast writes them at the run root; the
        // PREPARED TREE route's forecast has its own nested output
        // directory under the chain, so the root path simply does not
        // exist there. Reading only the root meant the map's relocation
        // trail stayed empty for every prepared-route moving nest —
        // silently, because an absent receipts file is also what a
        // still run looks like. Found by matrix cell l11 on the first
        // real prepared-route follow run.
        let path = trail_receipts_path(run_dir);
        let Some(path) = path else {
            return false;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return false;
        };
        let Ok(document) = serde_json::from_str::<serde_json::Value>(&text) else {
            return false;
        };
        let mut trail = Vec::new();
        if let Some(rows) = document.get("receipts").and_then(|value| value.as_array()) {
            for row in rows {
                if row.get("event").and_then(|value| value.as_str()) != Some("relocated") {
                    continue;
                }
                let placement = |key: &str| -> Option<(f64, f64)> {
                    let table = row.get(key)?;
                    Some((
                        table.get("i_parent_start")?.as_f64()?,
                        table.get("j_parent_start")?.as_f64()?,
                    ))
                };
                if let (Some(from), Some(to)) =
                    (placement("placement_from"), placement("placement_to"))
                {
                    trail.push(TrailMove {
                        elapsed_seconds: row
                            .get("elapsed_seconds")
                            .and_then(|value| value.as_f64())
                            .unwrap_or(0.0),
                        grid_id: row
                            .get("grid_id")
                            .and_then(|value| value.as_u64())
                            .unwrap_or(0) as u32,
                        from,
                        to,
                    });
                }
            }
        }
        if trail != self.trail {
            self.trail = trail;
            return true;
        }
        false
    }

    fn apply(&mut self, envelope: RunEventEnvelope) {
        match envelope.event {
            RunEvent::PlanAccepted {
                run_id, run_dir, ..
            } => {
                self.run_id = Some(run_id);
                self.run_dir = Some(PathBuf::from(&run_dir));
                if self.record.run_dir.as_deref() != Some(run_dir.as_str()) {
                    self.record.run_dir = Some(run_dir);
                    let _ = RunRegistry::save_record(&self.dir, &self.record);
                }
            }
            RunEvent::ResolvedPlan {
                configuration,
                automatic_resolutions,
                ..
            } => {
                if let Some(geometry) = configuration_lambert_geometry(&configuration) {
                    self.domain = Some(lambert_from_geometry(&geometry));
                    self.progress.run_seconds = geometry.run_seconds;
                }
                self.tree = arwen_plan::queries::configuration_domain_tree(&configuration);
                self.configuration = Some(configuration);
                self.resolutions = automatic_resolutions;
            }
            RunEvent::StageStarted { stage, phase } => {
                self.stages.push(StageState {
                    id: stage,
                    status: StageStatus::Running,
                    wall_seconds: None,
                    phases: phase.into_iter().collect(),
                    outcome: None,
                });
            }
            RunEvent::StageFinished {
                stage,
                wall_seconds,
                phases,
                outcome,
                ..
            } => {
                if let Some(state) = self.stages.iter_mut().rev().find(|state| state.id == stage) {
                    state.status = if outcome.as_deref() == Some("failed") {
                        StageStatus::Failed
                    } else {
                        StageStatus::Ok
                    };
                    state.wall_seconds = wall_seconds;
                    if !phases.is_empty() {
                        state.phases = phases;
                    }
                    state.outcome = outcome;
                }
            }
            RunEvent::ModelProgress {
                model_seconds,
                speed_x,
                outer_step,
                source,
                ..
            } => {
                self.progress.model_seconds = model_seconds;
                self.progress.speed_x = speed_x;
                self.progress.outer_step = outer_step;
                self.progress.polled = source.as_deref() == Some("stage_progress_file");
            }
            RunEvent::OutputCommitted {
                path,
                valid_time,
                domain,
            } => match parse_valid_time_utc(&valid_time) {
                Some(valid) => self.outputs.push(OutputFrame {
                    path: PathBuf::from(path),
                    valid_time_utc: valid,
                    domain,
                }),
                None => self.stream_errors.push(format!(
                    "output_committed with unparseable valid time {valid_time:?}"
                )),
            },
            RunEvent::Warning { code, message, .. } => {
                self.warnings.push(match code {
                    Some(code) => format!("[{code}] {message}"),
                    None => message,
                });
            }
            RunEvent::Completed { .. } => self.terminal = Some(Terminal::Completed),
            RunEvent::Failed {
                message, remedy, ..
            } => {
                self.terminal = Some(Terminal::Failed {
                    error: message,
                    remedy,
                });
            }
            RunEvent::Unknown => {}
        }
    }

    /// Read run-progress.json at most every 2 s while the run is live.
    fn refresh_heartbeat(&mut self) -> bool {
        if self.terminal.is_some() && self.heartbeat.is_some() {
            return false;
        }
        let Some(run_dir) = &self.run_dir else {
            return false;
        };
        if let Some(read) = self.heartbeat_read
            && read.elapsed() < Duration::from_secs(2)
        {
            return false;
        }
        self.heartbeat_read = Some(Instant::now());
        let path = run_dir.join("run-progress.json");
        match std::fs::read_to_string(&path) {
            Ok(text) => match RunProgress::parse(&text) {
                Ok(progress) => {
                    let changed = self.heartbeat.as_ref() != Some(&progress);
                    self.heartbeat = Some(progress);
                    changed
                }
                Err(_) => false,
            },
            Err(_) => false,
        }
    }

    /// Liveness verdict for the top bar: (label, healthy).
    pub fn liveness(&self) -> (&'static str, bool) {
        if self.stream_poisoned.is_some() {
            return ("event stream broken", false);
        }
        if let Some(terminal) = &self.terminal {
            return match terminal {
                Terminal::Completed => ("completed", true),
                Terminal::Failed { .. } => ("failed", false),
            };
        }
        if let Some(heartbeat) = &self.heartbeat {
            let stale = heartbeat
                .staleness_seconds(chrono::Utc::now())
                .map(|seconds| seconds > 30.0)
                .unwrap_or(false);
            if stale {
                let pid_alive = heartbeat
                    .pid
                    .map(arwen_proc::launcher::pid_is_alive)
                    .unwrap_or(false);
                return if pid_alive {
                    ("running (heartbeat stale)", false)
                } else {
                    ("stopped (no heartbeat, process gone)", false)
                };
            }
            return ("running", true);
        }
        ("starting", true)
    }

    /// The frame the map should show: explicit selection, else the newest.
    pub fn display_frame(&self) -> Option<(usize, &OutputFrame)> {
        match self.selected_frame {
            Some(index) => self.outputs.get(index).map(|frame| (index, frame)),
            None => self
                .outputs
                .len()
                .checked_sub(1)
                .and_then(|index| self.outputs.get(index).map(|frame| (index, frame))),
        }
    }
}

/// The relocation runner's receipts file under a run directory: the
/// run root first (the experiment route writes it there), then a
/// bounded search of the chain's nested forecast output (the prepared
/// tree route's `outdir`).
///
/// Bounded on purpose — depth 6, and the frame/product directories a
/// finished run fills with thousands of files are skipped. This runs on
/// a 5-second throttle behind a live UI, so it must stay cheap on a run
/// directory that is still growing.
fn trail_receipts_path(run_dir: &Path) -> Option<PathBuf> {
    const NAME: &str = "relocation_receipts.json";
    const SKIP: [&str; 5] = ["wrfout", "frames", "products", "render", "data"];
    let direct = run_dir.join(NAME);
    if direct.is_file() {
        return Some(direct);
    }
    let mut stack = vec![(run_dir.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > 6 {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if path.is_dir() {
                if !SKIP.iter().any(|skip| name.starts_with(skip)) {
                    stack.push((path, depth + 1));
                }
            } else if name == NAME {
                return Some(path);
            }
        }
    }
    None
}

/// Matrix access to [`trail_receipts_path`]: cell l11 asserts that the
/// overlay's resolver finds the SAME file the cell found, so a future
/// layout change cannot fix one and silently break the other.
#[cfg(test)]
pub fn trail_receipts_path_for_test(run_dir: &Path) -> Option<PathBuf> {
    trail_receipts_path(run_dir)
}

/// One executed relocation (the runner's receipt row, verbatim
/// placements in 1-based parent cells).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrailMove {
    pub elapsed_seconds: f64,
    pub grid_id: u32,
    pub from: (f64, f64),
    pub to: (f64, f64),
}

/// The engine's resolved geometry as a paintable Lambert domain.
pub fn lambert_from_geometry(geometry: &arwen_plan::queries::ResolvedGeometry) -> LambertDomain {
    LambertDomain {
        nx: geometry.nx,
        ny: geometry.ny,
        dx_m: geometry.dx_m,
        ref_lat: geometry.ref_lat,
        ref_lon: geometry.ref_lon,
        truelat1: geometry.truelat1,
        truelat2: geometry.truelat2,
        stand_lon: geometry.stand_lon,
    }
}

/// `output_committed.valid_time` is `datetime.isoformat()` — usually
/// naive. Naive parses as UTC; an explicit offset is honored.
fn parse_valid_time_utc(text: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    if let Ok(with_zone) = chrono::DateTime::parse_from_rfc3339(text) {
        return Some(with_zone.with_timezone(&chrono::Utc));
    }
    let naive = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f"))
        .ok()?;
    Some(naive.and_utc())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arwen_proc::registry::LAUNCH_SCHEMA;

    fn record() -> LaunchRecord {
        LaunchRecord {
            schema: LAUNCH_SCHEMA.into(),
            plan_sha256: "abc".into(),
            name: "test".into(),
            launched_at_utc: "2026-08-07T12:00:00Z".into(),
            ownership: "survive_studio".into(),
            child_pid: None,
            fixture: true,
            run_dir: None,
            extra: Default::default(),
        }
    }

    fn session_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "arwen-studio-session-{label}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Feed the whole happy-path fixture through a session as a reattach:
    /// five stages resolve in order, 25 frames arrive, geometry
    /// extracted, terminal is Completed, sequence stays dense.
    #[test]
    fn fixture_stream_folds_into_session_state() {
        let dir = session_dir("happy");
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/events.jsonl");
        std::fs::copy(&fixture, RunRegistry::events_path(&dir)).unwrap();

        let mut session = RunSession::reattach(dir.clone(), record());
        assert!(session.poll());

        assert!(session.stream_poisoned.is_none());
        assert_eq!(session.outputs.len(), 25);
        assert_eq!(session.terminal, Some(Terminal::Completed));
        assert_eq!(
            session
                .stages
                .iter()
                .map(|stage| stage.id.as_str())
                .collect::<Vec<_>>(),
            ["fetch", "prepare", "initialize", "forecast", "finalize"]
        );
        assert!(
            session
                .stages
                .iter()
                .all(|stage| stage.status == StageStatus::Ok)
        );
        assert_eq!(session.warnings.len(), 1);
        assert!(session.warnings[0].starts_with("[vram_high_water]"));
        assert!(session.stream_errors.is_empty());
        assert!(!session.resolutions.is_empty());

        let domain = session.domain.expect("geometry from resolved plan");
        assert_eq!(domain.nx, 300);
        assert!((domain.ref_lat - 35.5).abs() < 1e-9);
        assert_eq!(session.progress.run_seconds, Some(21_600.0));
        assert_eq!(session.progress.fraction(), Some(1.0));

        // Follow-latest shows the newest frame; scrubbing selects. The
        // naive valid_time parsed as UTC.
        let (index, frame) = session.display_frame().unwrap();
        assert_eq!(index, 24);
        assert_eq!(
            frame.valid_time_utc.to_rfc3339(),
            "2026-08-07T18:00:00+00:00"
        );
        session.selected_frame = Some(0);
        assert_eq!(session.display_frame().unwrap().0, 0);

        assert!(session.record.run_dir.is_some());
        assert_eq!(session.liveness().0, "completed");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn failure_stream_marks_stage_outcome_and_remedy() {
        let dir = session_dir("fail");
        let fixture =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/events-failure.jsonl");
        std::fs::copy(&fixture, RunRegistry::events_path(&dir)).unwrap();

        let mut session = RunSession::reattach(dir.clone(), record());
        session.poll();
        match &session.terminal {
            Some(Terminal::Failed { error, remedy }) => {
                assert!(error.contains("finite check"));
                assert!(remedy.as_deref().unwrap().contains("gpuwm fetch"));
            }
            other => panic!("{other:?}"),
        }
        let prepare = session
            .stages
            .iter()
            .find(|stage| stage.id == "prepare")
            .unwrap();
        assert_eq!(prepare.status, StageStatus::Failed);
        assert_eq!(session.liveness(), ("failed", false));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A sequence gap poisons the stream loudly instead of smoothing over
    /// a lost line (engine contract: dense, refuse gaps).
    #[test]
    fn sequence_gap_poisons_the_stream() {
        let dir = session_dir("gap");
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/events.jsonl");
        let text = std::fs::read_to_string(&fixture).unwrap();
        // Drop the third line (sequence 3): a real lost line.
        let kept: Vec<&str> = text
            .lines()
            .enumerate()
            .filter_map(|(index, line)| (index != 2).then_some(line))
            .collect();
        std::fs::write(RunRegistry::events_path(&dir), kept.join("\n")).unwrap();

        let mut session = RunSession::reattach(dir.clone(), record());
        session.poll();
        let poisoned = session.stream_poisoned.as_deref().expect("poisoned");
        assert!(poisoned.contains("expected 3"), "{poisoned}");
        // Nothing after the gap was applied.
        assert!(session.terminal.is_none());
        assert_eq!(session.liveness().0, "event stream broken");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn valid_time_parses_naive_and_zoned() {
        assert_eq!(
            parse_valid_time_utc("2026-08-07T13:00:00")
                .unwrap()
                .to_rfc3339(),
            "2026-08-07T13:00:00+00:00"
        );
        assert_eq!(
            parse_valid_time_utc("2026-08-07T13:00:00+00:00")
                .unwrap()
                .to_rfc3339(),
            "2026-08-07T13:00:00+00:00"
        );
        assert!(parse_valid_time_utc("not a time").is_none());
    }
}
