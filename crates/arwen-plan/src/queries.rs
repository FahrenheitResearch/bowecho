// SPDX-License-Identifier: Apache-2.0

//! Reply documents for the three query modes, matching gpuwm/runplan.py's
//! `resolve_plan`, `estimate_plan` and `probe_environment` builders
//! field-for-field. Where the engine has no measured number the field is
//! null WITH ITS BASIS STATED — Studio renders the basis, never invents a
//! number.

use serde::{Deserialize, Serialize};

pub use crate::events::Resolution;

pub const RESOLVE_SCHEMA: &str = "gpuwm.run-plan.resolved.v1";
pub const ESTIMATE_SCHEMA: &str = "gpuwm.run-plan.estimate.v1";
pub const PROBE_SCHEMA: &str = "gpuwm.run-plan.probe.v1";
pub const CATALOG_SCHEMA: &str = "gpuwm.run-plan.catalog.v1";

// ---------------------------------------------------------------------------
// --catalog → gpuwm.run-plan.catalog.v1
// ---------------------------------------------------------------------------

/// The render-product catalog: the RENDERER's own `--list-products`
/// answer relayed by the engine — names verbatim, never a Studio list.
/// `parse_warning` (the engine could not fully parse the renderer's
/// output) MUST be surfaced when present.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CatalogReport {
    pub schema: String,
    /// Which renderer answered: `"rust"` or `"matplotlib"`.
    #[serde(default)]
    pub engine: Option<String>,
    #[serde(default)]
    pub engine_notice: Option<String>,
    #[serde(default)]
    pub parse_warning: Option<String>,
    /// Group keywords accepted alongside names (all/direct/derived/…).
    #[serde(default)]
    pub group_keywords: Vec<String>,
    /// The token that skips rendering entirely (`"none"`).
    #[serde(default)]
    pub skip_token: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    /// The engine's own sentence describing the `render_products` value.
    #[serde(default)]
    pub spec: Option<String>,
    #[serde(default)]
    pub products: Vec<CatalogProduct>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CatalogProduct {
    pub name: String,
}

// ---------------------------------------------------------------------------
// --probe → gpuwm.run-plan.probe.v1
// ---------------------------------------------------------------------------

/// Device inventory via NVML (poll-safe, no CUDA context) plus route and
/// schema inventories. The readiness half is present only when requested
/// (`--probe` without `--no-readiness`) because it creates a CUDA
/// context — Studio polls with `--no-readiness` and asks for readiness
/// on demand only.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProbeReport {
    pub schema: String,
    #[serde(default)]
    pub gpuwm_version: Option<String>,
    #[serde(default)]
    pub python: Option<String>,
    #[serde(default)]
    pub executable: Option<String>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub devices: Vec<DeviceStatus>,
    #[serde(default)]
    pub device_query_error: Option<String>,
    #[serde(default)]
    pub device_query_basis: Option<String>,
    #[serde(default)]
    pub readiness: Option<Readiness>,
    /// route name → summary.
    #[serde(default)]
    pub routes: std::collections::BTreeMap<String, String>,
    /// document kind → schema id, the engine's own inventory.
    #[serde(default)]
    pub schemas: std::collections::BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeviceStatus {
    #[serde(default)]
    pub index: Option<u32>,
    #[serde(default)]
    pub uuid: Option<String>,
    pub name: String,
    #[serde(default)]
    pub driver_version: Option<String>,
    /// The card's physical total.
    #[serde(default)]
    pub memory_total_bytes: Option<u64>,
    /// Device-wide across every process — free is what a NEW run could
    /// actually claim.
    #[serde(default)]
    pub memory_used_bytes: Option<u64>,
    #[serde(default)]
    pub memory_free_bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Readiness {
    pub collected: bool,
    #[serde(default)]
    pub ready: Option<bool>,
    #[serde(default)]
    pub gaps: Option<u64>,
    #[serde(default)]
    pub blocking_gaps: Option<u64>,
    #[serde(default)]
    pub checks: Option<serde_json::Value>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub basis: Option<String>,
}

impl ProbeReport {
    /// Can a plan launch right now, as far as the poll-safe half knows?
    pub fn devices_ok(&self) -> bool {
        self.device_query_error.is_none() && !self.devices.is_empty()
    }
}

// ---------------------------------------------------------------------------
// --estimate → gpuwm.run-plan.estimate.v1
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EstimateReport {
    pub schema: String,
    #[serde(default)]
    pub plan: Option<serde_json::Value>,
    pub vram: VramEstimate,
    pub disk: DiskEstimate,
    /// What the preparation will write for a MOVING nest — absent on an
    /// engine that does not price one, and present-but-empty for a plan
    /// that moves nothing. HOST/disk only: see [`CorridorEstimate`].
    #[serde(default)]
    pub corridor: Option<CorridorEstimate>,
    pub download: BasisNumber,
    pub wall_time: WallTimeEstimate,
    #[serde(default)]
    pub automatic_resolutions: Vec<Resolution>,
}

/// The statics corridor a moving nest's preparation seals, priced
/// before it is built (`--estimate`'s `corridor` block).
///
/// IT IS NOT VRAM. The engine's own basis says so in as many words —
/// "a corridor is cropped on the host, so the VRAM estimate above is
/// unchanged by it" — and the two arms of a real GFS tree confirm it:
/// the follow config and the identical still config both estimate
/// 6_858_962_954 VRAM bytes, to the byte. Studio shows the corridor
/// BESIDE the VRAM figure and never inside it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CorridorEstimate {
    /// One entry per CHILD domain: run-plan passes `--statics-corridor`
    /// bare, which the preparation reads as "every child".
    #[serde(default)]
    pub domains: Vec<CorridorDomain>,
    #[serde(default)]
    pub host_bytes: Option<u64>,
    #[serde(default)]
    pub host_gib: Option<f64>,
    #[serde(default)]
    pub basis: String,
}

impl CorridorEstimate {
    /// Does this plan actually build a corridor? (`host_bytes` is `0`
    /// with an empty `domains` list when no nest moves — the block is
    /// still emitted, carrying the basis that says why it is zero.)
    pub fn is_priced(&self) -> bool {
        self.host_bytes.unwrap_or(0) > 0
    }
}

/// One child's corridor: its PARENT's full extent at the CHILD's
/// resolution, which is why a modest nest can cost hundreds of MB.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CorridorDomain {
    /// The engine's own label, e.g. `"d02"`.
    #[serde(default)]
    pub domain: String,
    #[serde(default)]
    pub grid_id: Option<u32>,
    #[serde(default)]
    pub parent_id: Option<u32>,
    #[serde(default)]
    pub corridor_nx: Option<u64>,
    #[serde(default)]
    pub corridor_ny: Option<u64>,
    #[serde(default)]
    pub cells: Option<u64>,
    #[serde(default)]
    pub planes_per_cell: Option<u64>,
    #[serde(default)]
    pub bytes_per_cell: Option<u64>,
    #[serde(default)]
    pub host_bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VramEstimate {
    #[serde(default)]
    pub estimate_bytes: Option<u64>,
    #[serde(default)]
    pub estimate_gib: Option<f64>,
    pub basis: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiskEstimate {
    /// Exact per-domain frame counts.
    #[serde(default)]
    pub frames: Vec<DomainFrames>,
    #[serde(default)]
    pub total_frames: Option<u64>,
    /// Null: bytes-per-frame is not measured by the engine.
    #[serde(default)]
    pub bytes: Option<u64>,
    pub basis: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DomainFrames {
    pub domain: u32,
    #[serde(default)]
    pub history_interval_s: Option<f64>,
    pub frames: u64,
    #[serde(default)]
    pub nx: Option<u64>,
    #[serde(default)]
    pub ny: Option<u64>,
    #[serde(default)]
    pub nz: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BasisNumber {
    #[serde(default)]
    pub bytes: Option<u64>,
    pub basis: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WallTimeEstimate {
    /// Null: the engine publishes no measured rate for an arbitrary
    /// configuration; model_progress carries the real one from step one.
    #[serde(default)]
    pub seconds: Option<f64>,
    pub basis: String,
}

// ---------------------------------------------------------------------------
// --resolve → gpuwm.run-plan.resolved.v1
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResolveReport {
    pub schema: String,
    /// `{name, route, source, sha256, config_kind, config_source,
    /// config_sha256, run_dir, fetch_args, run_options}`.
    pub plan: serde_json::Value,
    /// Intent plans: the wizard-written TOML, verbatim. The caller never
    /// typed it — show it.
    #[serde(default)]
    pub generated_config: Option<String>,
    /// `{"experiment": {...}, "case_data": {...}}` — the loaded objects,
    /// not a re-read of the TOML.
    pub configuration: serde_json::Value,
    /// Planning before the data exists: the full declared-input
    /// inventory with per-entry `present` flags.
    #[serde(default)]
    pub declared_inputs: Vec<serde_json::Value>,
    #[serde(default)]
    pub inputs_present: Option<bool>,
    /// The smallest domain the engine will size, DERIVED from the
    /// wizard's own fit bracket at call time — never a copied constant.
    #[serde(default)]
    pub domain_size_floor: Option<serde_json::Value>,
    /// How a MOVING nest gets its statics on the chain this plan
    /// dispatches to, decided before anything is fetched. `null` for a
    /// config that moves no nest — and ABSENT ENTIRELY on an engine
    /// that predates the decision, which is the capability probe
    /// Studio gates the prepared-route moving-nest row on (see
    /// `storm::MovingNest`).
    #[serde(default)]
    pub moving_nest: Option<MovingNestDecision>,
    #[serde(default)]
    pub automatic_resolutions: Vec<Resolution>,
    #[serde(default)]
    pub warnings: Vec<serde_json::Value>,
}

/// `--resolve`'s `moving_nest` record: the chain, how that chain feeds
/// a moving nest, and whether the preparation must seal a corridor.
///
/// The refusal slot the engine carries internally is NOT published —
/// reaching a caller at all means there was none, so a plan that
/// resolves with this record is a plan whose chain can move a nest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MovingNestDecision {
    /// `"experiment"`, `"prepared:go"`, `"prepared:hrrr"`.
    #[serde(default)]
    pub chain: String,
    /// `"statics_corridor"` (the preparation seals child-resolution
    /// statics) or `"case_data_ingest"` (the route holds the geography
    /// source and rebuilds each footprint at move time).
    #[serde(default)]
    pub delivery: Option<String>,
    #[serde(default)]
    pub relocation_grid_id: Option<u32>,
    #[serde(default)]
    pub statics_corridor: bool,
}

/// Pull the root-domain Lambert geometry out of a resolved
/// `configuration` document (`experiment.projection` +
/// `experiment.domains[0].run`). Returns None when the shape is missing
/// pieces (idealized configs have no projection).
pub fn configuration_lambert_geometry(
    configuration: &serde_json::Value,
) -> Option<ResolvedGeometry> {
    let experiment = configuration.get("experiment")?;
    let projection = experiment.get("projection")?;
    let domain = experiment.get("domains")?.get(0)?;
    let run = domain.get("run")?;
    Some(ResolvedGeometry {
        nx: run.get("nx")?.as_u64()? as u32,
        ny: run.get("ny")?.as_u64()? as u32,
        dx_m: run.get("dx")?.as_f64()?,
        ref_lat: projection.get("ref_lat")?.as_f64()?,
        ref_lon: projection.get("ref_lon")?.as_f64()?,
        truelat1: projection.get("truelat1")?.as_f64()?,
        truelat2: projection.get("truelat2")?.as_f64()?,
        stand_lon: projection.get("stand_lon")?.as_f64()?,
        run_seconds: experiment.get("run_seconds").and_then(|v| v.as_f64()),
    })
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResolvedGeometry {
    pub nx: u32,
    pub ny: u32,
    pub dx_m: f64,
    pub ref_lat: f64,
    pub ref_lon: f64,
    pub truelat1: f64,
    pub truelat2: f64,
    pub stand_lon: f64,
    pub run_seconds: Option<f64>,
}

/// One domain of the resolved tree, as the engine fitted it — placement
/// keys verbatim from `experiment.domains[]`. The root carries
/// `parent_id == 0`.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedDomain {
    pub grid_id: u32,
    pub parent_id: u32,
    pub i_parent_start: f64,
    pub j_parent_start: f64,
    pub parent_grid_ratio: f64,
    pub nx: u32,
    pub ny: u32,
    pub dx_m: f64,
    pub history_interval_s: Option<f64>,
    /// The chained per-domain time step (an engine output).
    pub dt_s: Option<f64>,
    /// The dormant-nest declaration (1.8 `spawn = {…}`): trigger name,
    /// threshold (field units) and manual time, verbatim from the
    /// resolved configuration. `None` = ordinary live domain.
    pub spawn_trigger: Option<String>,
    pub spawn_threshold: Option<f64>,
    pub spawn_at_s: Option<f64>,
}

/// Every domain of a resolved `configuration`, parent-before-child (the
/// engine's own storage order). Empty when the shape is missing pieces.
pub fn configuration_domain_tree(configuration: &serde_json::Value) -> Vec<ResolvedDomain> {
    let Some(domains) = configuration
        .get("experiment")
        .and_then(|experiment| experiment.get("domains"))
        .and_then(|domains| domains.as_array())
    else {
        return Vec::new();
    };
    domains
        .iter()
        .filter_map(|domain| {
            let run = domain.get("run")?;
            let spawn = domain.get("spawn").filter(|value| !value.is_null());
            Some(ResolvedDomain {
                grid_id: domain.get("grid_id")?.as_u64()? as u32,
                parent_id: domain.get("parent_id")?.as_u64()? as u32,
                i_parent_start: domain.get("i_parent_start")?.as_f64()?,
                j_parent_start: domain.get("j_parent_start")?.as_f64()?,
                parent_grid_ratio: domain.get("parent_grid_ratio")?.as_f64()?,
                nx: run.get("nx")?.as_u64()? as u32,
                ny: run.get("ny")?.as_u64()? as u32,
                dx_m: run.get("dx")?.as_f64()?,
                history_interval_s: domain.get("history_interval_s").and_then(|v| v.as_f64()),
                dt_s: run.get("dt").and_then(|v| v.as_f64()),
                spawn_trigger: spawn
                    .and_then(|value| value.get("trigger"))
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
                spawn_threshold: spawn
                    .and_then(|value| value.get("threshold"))
                    .and_then(|value| value.as_f64()),
                spawn_at_s: spawn
                    .and_then(|value| value.get("at_s"))
                    .and_then(|value| value.as_f64()),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_reply_matches_the_engine_builder_shape() {
        let text = r#"{
            "schema": "gpuwm.run-plan.probe.v1",
            "gpuwm_version": "1.8.0",
            "python": "3.12.4",
            "executable": "C:/python/python.exe",
            "pid": 4242,
            "devices": [{ "index": 0, "uuid": "GPU-aaaa",
                          "name": "NVIDIA GeForce RTX 5090",
                          "driver_version": "576.02",
                          "memory_total_bytes": 34089730048,
                          "memory_used_bytes": 2951569408,
                          "memory_free_bytes": 31138160640 }],
            "device_query_error": null,
            "device_query_basis": "NVML via nvidia-smi; no CUDA context",
            "readiness": { "collected": false,
                           "basis": "readiness was not requested" },
            "routes": { "experiment": "the config-driven experiment route" },
            "schemas": { "plan": "gpuwm.run-plan.v1",
                         "event": "gpuwm.run-plan.event.v1" }
        }"#;
        let probe: ProbeReport = serde_json::from_str(text).unwrap();
        assert!(probe.devices_ok());
        assert_eq!(probe.devices[0].memory_free_bytes, Some(31_138_160_640));
        assert!(!probe.readiness.unwrap().collected);
        assert!(probe.routes.contains_key("experiment"));
    }

    #[test]
    fn estimate_reply_null_fields_carry_bases() {
        let text = r#"{
            "schema": "gpuwm.run-plan.estimate.v1",
            "plan": { "name": "n" },
            "vram": { "estimate_bytes": 18253611008, "estimate_gib": 17.0,
                      "basis": "gpuwm.core.preflight.estimate_experiment" },
            "disk": { "frames": [ { "domain": 1, "history_interval_s": 900.0,
                                    "frames": 25, "nx": 300, "ny": 240, "nz": 49 } ],
                      "total_frames": 25, "bytes": null,
                      "basis": "frame counts are exact; bytes-per-frame is not measured" },
            "download": { "bytes": null, "basis": "known only to the source mirror" },
            "wall_time": { "seconds": null, "basis": "no measured rate" },
            "automatic_resolutions": []
        }"#;
        let estimate: EstimateReport = serde_json::from_str(text).unwrap();
        assert_eq!(estimate.vram.estimate_bytes, Some(18_253_611_008));
        assert_eq!(estimate.disk.bytes, None);
        assert!(!estimate.disk.basis.is_empty());
        assert_eq!(estimate.disk.frames[0].frames, 25);
        assert_eq!(estimate.wall_time.seconds, None);
    }

    #[test]
    fn domain_tree_extracts_every_domain_with_placement() {
        let configuration = serde_json::json!({
            "experiment": {
                "domains": [
                    { "grid_id": 1, "parent_id": 0, "i_parent_start": 1,
                      "j_parent_start": 1, "parent_grid_ratio": 1,
                      "history_interval_s": 3600.0,
                      "run": { "nx": 204, "ny": 162, "dx": 12000.0, "dt": 60.0 } },
                    { "grid_id": 2, "parent_id": 1, "i_parent_start": 52,
                      "j_parent_start": 42, "parent_grid_ratio": 4,
                      "history_interval_s": 900.0,
                      "spawn": { "trigger": "uh", "threshold": 40.0,
                                 "earliest_s": 3600.0, "latest_s": 36000.0 },
                      "run": { "nx": 408, "ny": 320, "dx": 3000.0, "dt": 15.0 } }
                ]
            }
        });
        let tree = configuration_domain_tree(&configuration);
        assert_eq!(tree.len(), 2);
        assert_eq!(tree[0].parent_id, 0);
        assert_eq!(tree[0].spawn_trigger, None);
        assert_eq!(tree[1].grid_id, 2);
        assert_eq!(tree[1].i_parent_start, 52.0);
        assert_eq!(tree[1].parent_grid_ratio, 4.0);
        assert_eq!(tree[1].dx_m, 3000.0);
        assert_eq!(tree[1].dt_s, Some(15.0));
        // The dormant declaration rides the tree (ghost outlines + badge).
        assert_eq!(tree[1].spawn_trigger.as_deref(), Some("uh"));
        assert_eq!(tree[1].spawn_threshold, Some(40.0));
        assert!(configuration_domain_tree(&serde_json::json!({})).is_empty());
    }

    /// Cross-lane fixture: the REAL engine's `--resolve` reply for an
    /// ERA5 12-3 ladder intent (captured 2026-08-07, engine venv). The
    /// nested tree the map draws comes straight out of these bytes.
    #[test]
    fn real_engine_nested_resolve_reply_yields_the_fitted_tree() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/resolve-nested.json");
        let text = std::fs::read_to_string(path).unwrap();
        let report: ResolveReport = serde_json::from_str(&text).unwrap();
        assert!(
            report.generated_config.is_some(),
            "intent reply carries the TOML"
        );
        let tree = configuration_domain_tree(&report.configuration);
        assert_eq!(tree.len(), 2, "12-3 ladder = root + one nest");
        assert_eq!(tree[0].parent_id, 0);
        assert_eq!(tree[1].parent_grid_ratio, 4.0);
        assert!(tree[1].i_parent_start > 1.0);
        assert_eq!(tree[1].dx_m, 3000.0);
        let root = configuration_lambert_geometry(&report.configuration).unwrap();
        assert_eq!(root.dx_m, 12000.0);
    }

    #[test]
    fn resolved_geometry_extracts_from_the_asdict_shape() {
        let configuration = serde_json::json!({
            "experiment": {
                "name": "n", "run_seconds": 21600.0,
                "projection": { "map_proj": "lambert", "ref_lat": 35.5,
                                 "ref_lon": -97.5, "truelat1": 30.0,
                                 "truelat2": 60.0, "stand_lon": -97.5 },
                "domains": [ { "grid_id": 1,
                                "run": { "nx": 300, "ny": 240, "nz": 49,
                                         "dx": 3000.0, "dy": 3000.0 } } ]
            },
            "case_data": {}
        });
        let geometry = configuration_lambert_geometry(&configuration).unwrap();
        assert_eq!(geometry.nx, 300);
        assert_eq!(geometry.dx_m, 3000.0);
        assert_eq!(geometry.run_seconds, Some(21600.0));
        assert!(configuration_lambert_geometry(&serde_json::json!({})).is_none());
    }
}
