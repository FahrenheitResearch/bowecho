// SPDX-License-Identifier: Apache-2.0

//! The `gpuwm.run-plan.v1` plan document — matches gpuwm/runplan.py
//! (`_TOP_LEVEL_KEYS`, `_CONFIG_KEYS`, `_FETCH_KEYS`, route run_options)
//! and docs/run-plan.md exactly. gpuwm REFUSES unknown keys everywhere in
//! this document ("a dropped key runs a default under the name of your
//! value"), so Studio must never serialize a key the engine has not
//! declared.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const PLAN_SCHEMA: &str = "gpuwm.run-plan.v1";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunPlan {
    /// Must be exactly [`PLAN_SCHEMA`].
    pub schema: String,
    /// This run's label; carried on every event.
    pub name: String,
    /// Which engine front door executes it. Today: `"experiment"`.
    pub route: String,
    /// Exactly one of `{"path": ...}` / `{"inline": "<TOML>"}`.
    pub config: PlanConfig,
    /// `{"args": [...]}` — the argv list `gpuwm fetch` itself takes,
    /// validated by the engine against its real fetch parser.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetch: Option<FetchSpec>,
    /// THE RUN DIRECTORY ITSELF (not a parent). Engine default:
    /// `out/run`. Studio always sets it so it can find the run again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_root: Option<String>,
    #[serde(default, skip_serializing_if = "RunOptions::is_default")]
    pub run_options: RunOptions,
}

/// Externally tagged: `{"path": ...}`, `{"inline": ...}` or
/// `{"intent": {...}}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
// Schema-facing enum: boxing only the intent arm would complicate every
// serializer/caller for a tiny plan object that is never hot-loop allocated.
#[allow(clippy::large_enum_variant)]
pub enum PlanConfig {
    Path(String),
    Inline(String),
    /// The shape of the run, handed to the `gpuwm domain` wizard which
    /// writes the complete TOML. The generated text comes back verbatim
    /// as `generated_config` on `--resolve` / the `resolved_plan` event.
    Intent(IntentConfig),
}

/// `config.intent`: every key is a `gpuwm domain` flag, one to one, and
/// every VALUE is validated by the wizard's own parser — never here.
/// This struct is CLOSED (the engine refuses unknown intent keys) and
/// carries only the flags Studio actually sets; add fields deliberately
/// from the engine's `_INTENT_FLAGS` table when the UI grows a control.
///
/// THERE IS NO nx/ny: domain size is FITTED by the wizard's VRAM
/// estimator from the ladder and the card budget (an output, reported in
/// `automatic_resolutions` with basis `fitted_to_vram_budget`), never an
/// input.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct IntentConfig {
    /// `--point LAT,LON` — the place the domain is sized around.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub point: Option<String>,
    /// `--polygon` — a GeoJSON path (alternative to `point`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub polygon: Option<String>,
    /// `--source` — era5 (experiment route) / gfs (prepared route).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// `--cycle` — required by the wizard; `"latest"` is resolved by the
    /// engine to a concrete cycle before the fetch stage.
    pub cycle: String,
    /// `--hours` — forecast length.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hours: Option<u64>,
    /// `--root-dx` km.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_dx_km: Option<f64>,
    /// `--ladder`, e.g. `"12-3"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ladder: Option<String>,
    /// `--chain` — custom nest refinement ratios, e.g. `"3"` or `"3,3"`,
    /// paired with `root_dx_km` (the wizard's alternative to a preset
    /// ladder; each ratio is validated by the wizard's own parser).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain: Option<String>,
    /// `--vram-gib` — the fit budget the wizard sizes against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vram_gib: Option<f64>,
    /// `--history-interval` — root-domain write cadence, seconds
    /// (default 3600). Must be a whole number of the domain's time
    /// steps; the engine refuses otherwise with its own sentence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_interval_s: Option<u64>,
    /// `--nest-history-interval` — every nest's cadence (default 900).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nest_history_interval_s: Option<u64>,
    /// `--geog-root` — a WPS_GEOG tree staged elsewhere than the
    /// engine's default (`<case-data-root>/WPS_GEOG`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geog_root: Option<String>,
    /// `--physics-profile`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub physics_profile: Option<String>,
    /// `--name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FetchSpec {
    pub args: Vec<String>,
}

/// The experiment route's declared run_options. The engine refuses
/// unknown option keys, so this struct is CLOSED — a new engine option
/// gets added here deliberately, never smuggled through a catch-all.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RunOptions {
    /// GPU index (number) or full `GPU-…` UUID (string); sets
    /// `CUDA_VISIBLE_DEVICES` before any context exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<serde_json::Value>,
    /// Resolve, validate, emit `resolved_plan`, stop before device work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<bool>,
    /// A `gpuwmrst` checkpoint to continue from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart: Option<String>,
    /// A WPS_GEOG tree staged elsewhere than the engine's default —
    /// declared in the released run_options schema (gpuwm 1.8.2 lists
    /// it in `automatic_resolutions`); the path config-path plans need
    /// (intent plans carry it inside the intent instead). Found by the
    /// acceptance matrix: a drawn-root GFS launch refused at prepare
    /// ("the staged WPS_GEOG tree is not usable") because config.path
    /// plans dropped the settings' geog tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geog_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_debug: Option<bool>,
    /// PREPARED route only: what the end-of-run render stage draws — a
    /// comma-separated list of catalog names (verbatim), `"all"`, or
    /// `"none"` to skip rendering entirely. Absent leaves the chain's
    /// default set untouched. The experiment route refuses this key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render_products: Option<String>,
}

impl RunOptions {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

impl RunPlan {
    pub fn new(
        name: impl Into<String>,
        route: impl Into<String>,
        config: PlanConfig,
        output_root: impl Into<String>,
    ) -> Self {
        Self {
            schema: PLAN_SCHEMA.to_string(),
            name: name.into(),
            route: route.into(),
            config,
            fetch: None,
            output_root: Some(output_root.into()),
            run_options: RunOptions::default(),
        }
    }

    /// Serialize the plan exactly as it is submitted to gpuwm.
    pub fn to_json_pretty(&self) -> serde_json::Result<String> {
        let mut text = serde_json::to_string_pretty(self)?;
        text.push('\n');
        Ok(text)
    }
}

/// The run-registry key: SHA-256 of the exact plan bytes submitted to
/// gpuwm (matching the engine's own `plan_sha256` when it reads the same
/// file).
pub fn plan_sha256(plan_bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(plan_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_example_shape_round_trips() {
        // The example from docs/run-plan.md, verbatim key set.
        let text = r#"{
            "schema": "gpuwm.run-plan.v1",
            "name": "overnight-run",
            "route": "experiment",
            "config": { "path": "conus3km.toml" },
            "output_root": "runs/overnight",
            "fetch": { "args": ["--source", "gfs", "--cycle", "2026080700", "--hours", "12", "--out", "data"] },
            "run_options": { "device": 0, "dry_run": false, "restart": null, "health_debug": false }
        }"#;
        let plan: RunPlan = serde_json::from_str(text).unwrap();
        assert_eq!(plan.schema, PLAN_SCHEMA);
        assert_eq!(plan.route, "experiment");
        assert_eq!(plan.config, PlanConfig::Path("conus3km.toml".into()));
        assert_eq!(plan.fetch.as_ref().unwrap().args[1], "gfs");
        assert_eq!(plan.run_options.device, Some(serde_json::json!(0)));

        let back = serde_json::to_value(&plan).unwrap();
        assert_eq!(back["config"]["path"], "conus3km.toml");
        assert_eq!(back["fetch"]["args"][0], "--source");
    }

    #[test]
    fn inline_config_serializes_under_the_engine_key() {
        let plan = RunPlan::new(
            "n",
            "experiment",
            PlanConfig::Inline("[experiment]\n".into()),
            "runs/n",
        );
        let value = serde_json::to_value(&plan).unwrap();
        assert_eq!(value["config"]["inline"], "[experiment]\n");
        // Default run_options are omitted entirely — the engine's schema
        // defaults are the engine's to report via automatic_resolutions.
        assert!(value.get("run_options").is_none());
        assert!(value.get("fetch").is_none());
    }

    #[test]
    fn run_options_never_carries_undeclared_keys() {
        // Serializing must produce ONLY declared keys (the engine refuses
        // unknowns). A device UUID travels as a string.
        let mut options = RunOptions::default();
        options.device = Some(serde_json::json!("GPU-aaaa"));
        options.restart = Some("run/checkpoint.gpuwmrst".into());
        let value = serde_json::to_value(&options).unwrap();
        let keys: Vec<&String> = value.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["device", "restart"]);
    }

    #[test]
    fn intent_config_serializes_under_the_engine_key_with_only_set_flags() {
        let intent = IntentConfig {
            point: Some("35.2,-97.4".into()),
            source: Some("gfs".into()),
            cycle: "latest".into(),
            hours: Some(2),
            root_dx_km: Some(3.0),
            history_interval_s: Some(900),
            ..IntentConfig::default()
        };
        let plan = RunPlan::new("n", "prepared", PlanConfig::Intent(intent), "runs/n");
        let value = serde_json::to_value(&plan).unwrap();
        let intent_value = &value["config"]["intent"];
        assert_eq!(intent_value["point"], "35.2,-97.4");
        assert_eq!(intent_value["cycle"], "latest");
        assert_eq!(intent_value["history_interval_s"], 900);
        // Unset flags are absent, not null — the engine refuses unknown
        // keys and treats present keys as spelled.
        // serde_json orders object keys; only PRESENCE matters to the
        // engine (unknown keys refused, absent keys defaulted).
        let keys: Vec<&String> = intent_value.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            [
                "cycle",
                "history_interval_s",
                "hours",
                "point",
                "root_dx_km",
                "source"
            ]
        );
    }

    #[test]
    fn plan_hash_is_over_submitted_bytes() {
        assert_eq!(
            plan_sha256(b"{}"),
            "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
        );
    }
}
