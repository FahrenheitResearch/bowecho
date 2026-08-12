// SPDX-License-Identifier: Apache-2.0

//! The fixtures in `fixtures/` ARE the written contract: every one must
//! parse against the arwen-plan types, so the two cannot drift apart.

use std::path::PathBuf;

use arwen_plan::events::{EVENT_SCHEMA, RunEvent, SequenceGate, parse_event_line};
use arwen_plan::heartbeat::RunProgress;
use arwen_plan::manifest::RunManifest;
use arwen_plan::plan::RunPlan;
use arwen_plan::queries::{
    EstimateReport, ProbeReport, ResolveReport, configuration_lambert_geometry,
};

fn fixture(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {path:?}: {error}"))
}

#[test]
fn plan_fixture_parses_and_its_inline_toml_is_valid_toml() {
    let plan: RunPlan = serde_json::from_str(&fixture("plan.json")).unwrap();
    assert_eq!(plan.schema, arwen_plan::plan::PLAN_SCHEMA);
    assert_eq!(plan.route, "experiment");
    assert_eq!(plan.fetch.as_ref().unwrap().args[1], "gfs");
    match &plan.config {
        arwen_plan::plan::PlanConfig::Inline(text) => {
            let value: toml::Table = text.parse().expect("inline config parses as TOML");
            assert_eq!(value["domain"][0]["dx"].as_float(), Some(3_000.0));
        }
        other => panic!("plan fixture should be inline TOML, got {other:?}"),
    }
}

#[test]
fn probe_estimate_resolve_fixtures_parse() {
    let probe: ProbeReport = serde_json::from_str(&fixture("probe.json")).unwrap();
    assert!(probe.devices_ok());
    assert!(probe.routes.contains_key("experiment"));
    assert!(!probe.readiness.unwrap().collected, "poll-safe fixture");
    assert_eq!(
        probe.schemas.get("event").map(String::as_str),
        Some("gpuwm.run-plan.event.v1")
    );

    let estimate: EstimateReport = serde_json::from_str(&fixture("estimate.json")).unwrap();
    assert!(estimate.vram.estimate_bytes.unwrap() > 0);
    assert_eq!(estimate.disk.total_frames, Some(25));
    assert_eq!(estimate.disk.bytes, None, "null with basis, never invented");
    assert!(!estimate.disk.basis.is_empty());
    assert_eq!(estimate.wall_time.seconds, None);

    let resolve: ResolveReport = serde_json::from_str(&fixture("resolve.json")).unwrap();
    assert!(!resolve.automatic_resolutions.is_empty());
    assert!(
        resolve
            .automatic_resolutions
            .iter()
            .any(|resolution| resolution.key == "execution_mode"
                && resolution.value == serde_json::json!("in_process"))
    );
    let geometry = configuration_lambert_geometry(&resolve.configuration).unwrap();
    assert_eq!((geometry.nx, geometry.ny), (300, 240));
    assert_eq!(geometry.run_seconds, Some(21_600.0));
}

/// THE MOVING-NEST CONTRACT, on the run-plan corridor lane's REAL
/// output (`gpuwm run-plan --resolve/--estimate` driven against a
/// wizard-emitted 12-3 GFS config plus a `[relocation]` itinerary).
/// Studio's whole prepared-route moving-nest surface reads these two
/// documents, so they parse here or nothing downstream can be trusted.
#[test]
fn moving_nest_and_corridor_fixtures_parse_with_their_engine_values() {
    let resolve: ResolveReport =
        serde_json::from_str(&fixture("resolve-moving-nest.json")).unwrap();
    let decision = resolve
        .moving_nest
        .as_ref()
        .expect("a follow config resolves WITH a moving_nest record");
    assert_eq!(decision.chain, "prepared:go");
    assert_eq!(decision.delivery.as_deref(), Some("statics_corridor"));
    assert_eq!(decision.relocation_grid_id, Some(2));
    assert!(decision.statics_corridor);
    // The same fact in prose, where a caller that only prints the
    // resolutions still sees it.
    let entry = resolve
        .automatic_resolutions
        .iter()
        .find(|resolution| resolution.key == "statics_corridor")
        .expect("the preparation resolution");
    assert_eq!(entry.scope, "preparation");
    assert_eq!(entry.basis, "relocation_follow");

    // A STILL config on the same engine: the block is still emitted,
    // priced at zero, carrying the basis that says why.
    let still: EstimateReport =
        serde_json::from_str(&fixture("estimate-corridor-still.json")).unwrap();
    let still_corridor = still.corridor.as_ref().unwrap();
    assert!(!still_corridor.is_priced());
    assert!(
        still_corridor
            .basis
            .contains("no statics corridor is prepared")
    );

    let estimate: EstimateReport =
        serde_json::from_str(&fixture("estimate-corridor.json")).unwrap();
    let corridor = estimate
        .corridor
        .as_ref()
        .expect("the follow arm is priced");
    assert!(corridor.is_priced());
    assert_eq!(corridor.host_bytes, Some(410_323_968));
    assert_eq!(corridor.domains.len(), 1, "every CHILD is priced");
    let domain = &corridor.domains[0];
    assert_eq!(domain.domain, "d02");
    // Parent extent at CHILD resolution: 204x162 root, ratio 4.
    assert_eq!(domain.corridor_nx, Some(816));
    assert_eq!(domain.corridor_ny, Some(648));
    assert_eq!(domain.host_bytes, Some(410_323_968));
    // AND IT IS NOT VRAM — held by the engine's own two arms, not by
    // Studio's opinion.
    assert_eq!(
        estimate.vram.estimate_bytes, still.vram.estimate_bytes,
        "a corridor moves no VRAM byte"
    );

    // An engine WITHOUT the decision leaves the field absent, and the
    // types must read that as "said nothing" rather than fail.
    let older: ResolveReport = serde_json::from_str(&fixture("resolve-nested.json")).unwrap();
    assert!(
        older.moving_nest.is_none(),
        "the 1.8.5 capture predates the decision — this IS the capability probe"
    );
    let older_estimate: EstimateReport = serde_json::from_str(&fixture("estimate.json")).unwrap();
    assert!(older_estimate.corridor.is_none());
}

#[test]
fn happy_path_event_stream_is_dense_and_terminates_once() {
    let text = fixture("events.jsonl");
    let mut gate = SequenceGate::new();
    let mut outputs = 0usize;
    let mut terminal = 0usize;
    let mut stages: Vec<String> = Vec::new();
    for line in text.lines() {
        let Some(envelope) = parse_event_line(line).unwrap() else {
            continue;
        };
        assert_eq!(envelope.schema_version, EVENT_SCHEMA);
        gate.accept(envelope.sequence).unwrap();
        match &envelope.event {
            RunEvent::OutputCommitted { valid_time, .. } => {
                outputs += 1;
                // Engine emits naive isoformat — no Z.
                assert!(!valid_time.ends_with('Z'), "{valid_time}");
            }
            RunEvent::StageStarted { stage, .. } => stages.push(stage.clone()),
            RunEvent::Unknown => panic!("fixture must use only known tags: {line}"),
            _ => {}
        }
        if envelope.event.is_terminal() {
            terminal += 1;
        }
    }
    assert_eq!(outputs, 25, "analysis frame + 24 forecast frames");
    assert_eq!(terminal, 1, "exactly one terminal event");
    assert_eq!(
        stages,
        ["fetch", "prepare", "initialize", "forecast", "finalize"],
        "the five stages in order"
    );
}

#[test]
fn failure_event_stream_ends_in_failed_with_remedy() {
    let text = fixture("events-failure.jsonl");
    let last = text
        .lines()
        .filter_map(|line| parse_event_line(line).unwrap())
        .next_back()
        .unwrap();
    match last.event {
        RunEvent::Failed {
            stage,
            error_class,
            message,
            remedy,
            ..
        } => {
            assert_eq!(stage.as_deref(), Some("prepare"));
            assert_eq!(error_class.as_deref(), Some("ValueError"));
            assert!(message.contains("finite check"));
            assert!(remedy.unwrap().contains("gpuwm fetch"));
        }
        other => panic!("expected failed, got {other:?}"),
    }
}

#[test]
fn heartbeat_and_manifest_fixtures_parse() {
    let progress = RunProgress::parse(&fixture("run-progress.json")).unwrap();
    assert_eq!(progress.status, "integrating");
    assert!(progress.last_durable_wrfout.unwrap().contains("wrfout_d01"));

    let manifest = RunManifest::parse(&fixture("run-manifest.json")).unwrap();
    assert_eq!(
        manifest.progress_schema.as_deref(),
        Some("gpuwm.run-progress/v1")
    );
    assert_eq!(
        manifest.failure_capsule_schema.as_deref(),
        Some("gpuwm.failure-capsule/v3")
    );
    assert!(manifest.events_path.unwrap().ends_with("events.jsonl"));
}
