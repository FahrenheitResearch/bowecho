// SPDX-License-Identifier: Apache-2.0

//! Sealed-launcher live proof: the SAME sealed environment Studio uses
//! (env_clear + explicit map + Job-Object query ownership) must be able
//! to run the real `gpuwm run-plan`. Ignored by default; run with:
//!
//! ```text
//! ARWEN_LIVE_PYTHON=...\gpuwm-venv\Scripts\python.exe ^
//! [ARWEN_LIVE_PLAN=...\plan.json] ^
//! cargo test -p arwen-proc --test live_launcher -- --ignored --nocapture
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;

use arwen_proc::{ContractSource, LauncherSpec};

fn live_source() -> Option<ContractSource> {
    let program = std::env::var("ARWEN_LIVE_PYTHON").ok()?;
    Some(ContractSource::Live {
        spec: LauncherSpec {
            program: PathBuf::from(program),
            args_prefix: vec![
                "-I".into(),
                "-B".into(),
                "-m".into(),
                "gpuwm.runplan".into(),
            ],
            env: BTreeMap::new(),
            private_runtime: None,
        },
    })
}

#[test]
#[ignore = "needs a live gpuwm venv (set ARWEN_LIVE_PYTHON)"]
fn sealed_probe_reaches_the_real_engine() {
    let source = live_source().expect("set ARWEN_LIVE_PYTHON");
    let probe = source.probe().expect("sealed live probe");
    assert!(probe.devices_ok(), "{:?}", probe.device_query_error);
    assert!(probe.routes.contains_key("prepared"));
    assert!(
        !probe.readiness.expect("readiness section").collected,
        "--no-readiness half only"
    );
    println!(
        "sealed live probe OK: gpuwm {} on {}",
        probe.gpuwm_version.as_deref().unwrap_or("?"),
        probe
            .devices
            .first()
            .map(|device| device.name.as_str())
            .unwrap_or("?")
    );
}

#[test]
#[ignore = "needs a live gpuwm venv + plan (set ARWEN_LIVE_PYTHON, ARWEN_LIVE_PLAN)"]
fn sealed_resolve_and_estimate_answer_an_intent_plan() {
    let source = live_source().expect("set ARWEN_LIVE_PYTHON");
    let plan_path = std::env::var("ARWEN_LIVE_PLAN").expect("set ARWEN_LIVE_PLAN");
    let plan_json = std::fs::read_to_string(plan_path).expect("read plan");

    let resolve = source.resolve(&plan_json).expect("sealed live resolve");
    assert_eq!(resolve.plan["config_kind"], "intent");
    assert!(resolve.generated_config.is_some());
    let geometry = arwen_plan::configuration_lambert_geometry(&resolve.configuration)
        .expect("fitted geometry");

    let estimate = source.estimate(&plan_json).expect("sealed live estimate");
    let vram = estimate.vram.estimate_gib.unwrap_or_default();
    println!(
        "sealed live resolve/estimate OK: fitted {}x{}, vram {vram:.2} GiB",
        geometry.nx, geometry.ny
    );
}
