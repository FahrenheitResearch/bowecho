// SPDX-License-Identifier: Apache-2.0

//! Live-document parse proof: point these at REAL engine replies
//! (captured from `gpuwm run-plan --resolve/--estimate/--probe`) and the
//! arwen-plan types must parse them. Ignored by default because the
//! documents live outside the repo; run with:
//!
//! ```text
//! ARWEN_LIVE_RESOLVE=...\live-resolve.json ^
//! ARWEN_LIVE_ESTIMATE=...\live-estimate.json ^
//! ARWEN_LIVE_PROBE=...\live-probe.json ^
//! cargo test -p arwen-plan --test live_documents -- --ignored
//! ```

use arwen_plan::queries::{
    EstimateReport, ProbeReport, ResolveReport, configuration_lambert_geometry,
};

fn read(env: &str) -> String {
    let path = std::env::var(env).unwrap_or_else(|_| panic!("set {env}"));
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {path}: {error}"));
    // PowerShell's utf8 capture prepends a BOM; the real pipe carries
    // none. Tolerate the capture artifact here only.
    text.trim_start_matches('\u{feff}').to_string()
}

#[test]
#[ignore = "needs captured live engine replies (see module doc)"]
fn live_probe_resolve_estimate_parse_through_the_types() {
    let probe: ProbeReport = serde_json::from_str(&read("ARWEN_LIVE_PROBE")).unwrap();
    assert!(probe.devices_ok());
    assert!(probe.routes.contains_key("prepared"));

    let resolve: ResolveReport = serde_json::from_str(&read("ARWEN_LIVE_RESOLVE")).unwrap();
    assert_eq!(resolve.schema, "gpuwm.run-plan.resolved.v1");
    assert_eq!(resolve.plan["config_kind"], "intent");
    let generated = resolve.generated_config.as_deref().expect("generated TOML");
    assert!(generated.contains("[experiment]"), "wizard TOML present");
    let geometry =
        configuration_lambert_geometry(&resolve.configuration).expect("fitted geometry extracts");
    assert!(geometry.nx >= 60 && geometry.ny >= 48, "above the floor");
    assert!(resolve.domain_size_floor.is_some());
    assert!(
        resolve
            .automatic_resolutions
            .iter()
            .any(|resolution| resolution.basis == "fitted_to_vram_budget")
    );

    let estimate: EstimateReport = serde_json::from_str(&read("ARWEN_LIVE_ESTIMATE")).unwrap();
    assert!(estimate.vram.estimate_bytes.unwrap() > 0);
    assert!(!estimate.disk.frames.is_empty());
    println!(
        "live docs OK: fitted {}x{} @ {:.0} m, vram {:.2} GiB, frames {:?}",
        geometry.nx,
        geometry.ny,
        geometry.dx_m,
        estimate.vram.estimate_gib.unwrap_or_default(),
        estimate.disk.frames[0].frames,
    );
}
