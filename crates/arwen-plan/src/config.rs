// SPDX-License-Identifier: Apache-2.0

//! The intent-level config TOML Studio embeds in a plan.
//!
//! Key names and section shapes match gpuwm's existing `configs/*.toml`
//! schema (`[experiment]` / `[projection]` / `[[domain]]`). Studio emits
//! ONLY what the user chose in the inspector; every engine-owned default
//! is resolved by `gpuwm run-plan --resolve` and shown on the review
//! sheet. Studio never writes a key it did not get from the user.
//!
//! OPEN ENGINE REQUEST (docs/END-GAME.md §4): the experiment route's
//! loader currently requires a COMPLETE experiment TOML — including
//! `[case_data]` inputs and the vertical scaffold — so this intent
//! subset is refused by today's engine. Studio needs either an
//! intent-accepting route or an engine-side config generator that closes
//! the gap from (projection + domain + schedule + fetch products) to a
//! full config. Until that lands, live-mode plans use
//! `PlanConfig::Path` to an existing config; the fixture path exercises
//! the intent flow end to end.

use serde::Serialize;

/// Everything the v1 inspector decides. One flat intent struct in, one
/// deterministic TOML string out.
#[derive(Clone, Debug, PartialEq)]
pub struct StudioConfig {
    /// Run name (also `[experiment].name`).
    pub name: String,
    /// Forecast start, UTC, written as a TOML local datetime
    /// (`YYYY-MM-DDTHH:MM:SS`) matching gpuwm's existing configs.
    /// `None` when the user chose the "latest cycle" intent — the engine
    /// resolves the actual start from `fetch.cycle` and reports it as a
    /// resolution row (Studio must never invent a start time).
    pub start_time: Option<chrono::NaiveDateTime>,
    /// Forecast length in seconds.
    pub run_seconds: f64,
    /// Lambert projection (v1 domains are always Lambert).
    pub ref_lat: f64,
    pub ref_lon: f64,
    pub truelat1: f64,
    pub truelat2: f64,
    pub stand_lon: f64,
    /// Mass-grid dimensions.
    pub nx: u32,
    pub ny: u32,
    /// Grid spacing, meters.
    pub dx: f64,
    /// Root-domain model time step, INTEGER seconds (the engine's
    /// DomainConfig carries `time_step` as whole seconds plus rational
    /// correction keys Studio does not set). WRF practice: ~5-6 s per km.
    pub time_step: u32,
    /// wrfout cadence, seconds.
    pub history_interval_s: f64,
}

#[derive(Serialize)]
struct ConfigDoc {
    experiment: ExperimentSection,
    projection: ProjectionSection,
    domain: Vec<DomainSection>,
}

#[derive(Serialize)]
struct ExperimentSection {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_time: Option<toml::value::Datetime>,
    run_seconds: f64,
}

#[derive(Serialize)]
struct ProjectionSection {
    map_proj: &'static str,
    ref_lat: f64,
    ref_lon: f64,
    truelat1: f64,
    truelat2: f64,
    stand_lon: f64,
}

#[derive(Serialize)]
struct DomainSection {
    grid_id: u32,
    parent_id: u32,
    parent_grid_ratio: u32,
    nx: u32,
    ny: u32,
    dx: f64,
    time_step: u32,
    history_interval_s: f64,
}

impl StudioConfig {
    /// Deterministic inline TOML for the plan document.
    pub fn to_toml(&self) -> Result<String, String> {
        let start = self
            .start_time
            .map(|start_time| {
                start_time
                    .format("%Y-%m-%dT%H:%M:%S")
                    .to_string()
                    .parse::<toml::value::Datetime>()
                    .map_err(|error| format!("start_time is not a TOML datetime: {error}"))
            })
            .transpose()?;
        let doc = ConfigDoc {
            experiment: ExperimentSection {
                name: self.name.clone(),
                start_time: start,
                run_seconds: self.run_seconds,
            },
            projection: ProjectionSection {
                map_proj: "lambert",
                ref_lat: self.ref_lat,
                ref_lon: self.ref_lon,
                truelat1: self.truelat1,
                truelat2: self.truelat2,
                stand_lon: self.stand_lon,
            },
            domain: vec![DomainSection {
                grid_id: 1,
                parent_id: 0,
                parent_grid_ratio: 1,
                nx: self.nx,
                ny: self.ny,
                dx: self.dx,
                time_step: self.time_step,
                history_interval_s: self.history_interval_s,
            }],
        };
        toml::to_string(&doc).map_err(|error| format!("serialize config TOML: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> StudioConfig {
        StudioConfig {
            name: "afternoon-run".into(),
            start_time: Some(
                chrono::NaiveDate::from_ymd_opt(2026, 8, 7)
                    .unwrap()
                    .and_hms_opt(12, 0, 0)
                    .unwrap(),
            ),
            run_seconds: 21_600.0,
            ref_lat: 35.5,
            ref_lon: -97.5,
            truelat1: 30.0,
            truelat2: 60.0,
            stand_lon: -97.5,
            nx: 300,
            ny: 240,
            dx: 3_000.0,
            time_step: 15,
            history_interval_s: 900.0,
        }
    }

    #[test]
    fn emits_gpuwm_schema_sections_and_toml_datetime() {
        let text = sample().to_toml().unwrap();
        assert!(text.contains("[experiment]"), "{text}");
        assert!(text.contains("[projection]"), "{text}");
        assert!(text.contains("[[domain]]"), "{text}");
        // TOML local datetime, unquoted — exactly the existing config style.
        assert!(text.contains("start_time = 2026-08-07T12:00:00"), "{text}");
        assert!(text.contains("map_proj = \"lambert\""), "{text}");
        assert!(text.contains("grid_id = 1"), "{text}");

        // The emitted text parses back as TOML with the same values.
        let value: toml::Table = text.parse().unwrap();
        assert_eq!(value["experiment"]["name"].as_str(), Some("afternoon-run"));
        assert_eq!(value["domain"][0]["nx"].as_integer(), Some(300));
        assert_eq!(value["domain"][0]["dx"].as_float(), Some(3_000.0));
    }

    #[test]
    fn latest_cycle_intent_omits_start_time_entirely() {
        let mut config = sample();
        config.start_time = None;
        let text = config.to_toml().unwrap();
        assert!(!text.contains("start_time"), "{text}");
        let value: toml::Table = text.parse().unwrap();
        assert!(value["experiment"].get("start_time").is_none());
    }

    #[test]
    fn emits_only_user_intent_no_engine_defaults() {
        let text = sample().to_toml().unwrap();
        // Physics, vertical scaffold, and numerics are engine-owned; if any
        // of these ever appear here the review sheet stops being the single
        // place engine decisions surface.
        for engine_key in ["mp_physics", "eta_levels", "nz", "p_top", "epssm"] {
            assert!(
                !text.contains(engine_key),
                "engine key {engine_key} leaked: {text}"
            );
        }
    }
}
