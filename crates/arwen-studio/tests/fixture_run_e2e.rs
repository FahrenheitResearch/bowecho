// SPDX-License-Identifier: Apache-2.0

//! End-to-end launch proof against the REAL built binary: a fixture run
//! launched through the sealed launcher as a genuine detached-capable
//! subprocess, its stdout redirected into the registry's events.jsonl,
//! with engine-parity side effects (run-manifest.json, run-progress.json)
//! in the simulated run directory. This is the mechanism live gpuwm will
//! ride verbatim — only the program name changes.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use arwen_plan::events::{SequenceGate, parse_event_line};
use arwen_plan::heartbeat::RunProgress;
use arwen_plan::manifest::RunManifest;
use arwen_proc::ContractSource;
use arwen_proc::launcher::RunOwnership;
use arwen_proc::registry::RunRegistry;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

#[test]
fn launch_runs_the_real_subprocess_and_leaves_the_full_file_truth() {
    let root = std::env::temp_dir().join(format!(
        "arwen-studio-e2e-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    let registry = RunRegistry::new(root.join("registry"));
    let source = ContractSource::Fixture {
        fixture_dir: fixtures_dir(),
        speed: 1e6,
        runner: Some(PathBuf::from(env!("CARGO_BIN_EXE_arwen-studio"))),
    };
    let plan_json = std::fs::read_to_string(fixtures_dir().join("plan.json")).unwrap();

    let mut launched = source
        .launch(
            &registry,
            &plan_json,
            "e2e-run",
            RunOwnership::SurviveStudio,
        )
        .expect("launch fixture subprocess");
    assert_eq!(launched.record.ownership, "survive_studio");
    assert!(launched.guard.is_none(), "survive mode holds no kill guard");

    // The child is a real process; wait for it (bounded).
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        match launched.child.try_wait().expect("try_wait") {
            Some(status) => break status,
            None => {
                assert!(Instant::now() < deadline, "fixture run never finished");
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };
    assert!(status.success(), "fixture runner exited {status}");

    // Registry truth: the exact plan bytes and a dense event stream ending
    // in completed.
    assert_eq!(
        std::fs::read_to_string(RunRegistry::plan_path(&launched.dir)).unwrap(),
        plan_json
    );
    let events = std::fs::read_to_string(RunRegistry::events_path(&launched.dir)).unwrap();
    let mut gate = SequenceGate::new();
    let mut last = None;
    for line in events.lines() {
        if let Some(envelope) = parse_event_line(line).unwrap() {
            gate.accept(envelope.sequence).unwrap();
            last = Some(envelope);
        }
    }
    let last = last.expect("events present");
    assert!(last.event.is_terminal());

    // Run-dir truth (what reattach reads): manifest → heartbeat.
    let run_dir = launched.dir.join("gpuwm-run");
    let manifest =
        RunManifest::parse(&std::fs::read_to_string(run_dir.join("run-manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest.pid, launched.record.child_pid);
    let progress =
        RunProgress::parse(&std::fs::read_to_string(run_dir.join("run-progress.json")).unwrap())
            .unwrap();
    assert_eq!(progress.status, "complete");
    assert_eq!(progress.model_elapsed_seconds, Some(21_600.0));

    // The launch record was persisted for the runs registry.
    let record = RunRegistry::load_record(&launched.dir).unwrap();
    assert_eq!(record.name, "e2e-run");
    assert!(record.fixture);

    std::fs::remove_dir_all(&root).unwrap();
}
