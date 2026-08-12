// SPDX-License-Identifier: Apache-2.0

//! THE ACCEPTANCE MATRIX: no release ships without this green. Every real
//! user path is walked through the real widget
//! pipeline; every historical defect of this project is a PERMANENT
//! cell here or in a named sibling test.
//!
//! ONE COMMAND runs the whole matrix (fixture cells + LIVE cells against
//! the box's configured engine + the fixture-run e2e):
//!
//!     cargo test -p arwen-studio -- --include-ignored --test-threads=1
//!
//! Cells that cannot be automated honestly are named in the README's
//! rough edges with the manual check described (e.g. ERA5 WITH a CDS key
//! when this box has none) — never silently unwalked.

#![cfg(test)]

use std::time::{Duration, Instant};

use eframe::egui;

use crate::app::StudioApp;
use crate::settings::StudioSettings;

/// One walk: an app on a temp output root (fixture or the box's LIVE
/// engine), driven frame by frame like a user.
struct Walk {
    ctx: egui::Context,
    app: StudioApp,
    temp: std::path::PathBuf,
}

impl Drop for Walk {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.temp);
    }
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "arwen-matrix-{tag}-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ))
}

impl Walk {
    fn fixture(tag: &str) -> Self {
        let temp = temp_dir(tag);
        let mut settings = StudioSettings::default();
        settings.output_root = temp.join("forecasts").to_string_lossy().into_owned();
        let ctx = egui::Context::default();
        let mut app = StudioApp::with_settings(&ctx, settings);
        app.redirect_registry(temp.join("runs"));
        let mut walk = Self { ctx, app, temp };
        walk.frame();
        walk
    }

    /// The box's CONFIGURED engine (settings.json — live on the regression's box),
    /// output root redirected into the temp dir so matrix runs never
    /// touch real forecast folders. Panics when the box is not in live
    /// mode: live cells must never silently downgrade to fixtures.
    fn live(tag: &str) -> Self {
        let temp = temp_dir(tag);
        let mut settings = StudioSettings::load_or_default();
        assert_eq!(
            settings.contract_mode, "live",
            "live matrix cells need the box's engine (settings.json contract_mode)"
        );
        settings.output_root = temp.join("forecasts").to_string_lossy().into_owned();
        let ctx = egui::Context::default();
        let mut app = StudioApp::with_settings(&ctx, settings);
        app.redirect_registry(temp.join("runs"));
        let mut walk = Self { ctx, app, temp };
        walk.frame();
        walk
    }

    fn raw(events: Vec<egui::Event>) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1440.0, 900.0),
            )),
            events,
            ..Default::default()
        }
    }

    fn frame(&mut self) {
        let ctx = self.ctx.clone();
        let _ = ctx.run_ui(Self::raw(Vec::new()), |ui| self.app.ui_impl(ui));
    }

    fn events(&mut self, events: Vec<egui::Event>) {
        let ctx = self.ctx.clone();
        let _ = ctx.run_ui(Self::raw(events), |ui| self.app.ui_impl(ui));
    }

    fn pump_until(&mut self, what: &str, seconds: u64, until: impl Fn(&StudioApp) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        while Instant::now() < deadline && !until(&self.app) {
            self.frame();
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(until(&self.app), "timed out waiting for {what}");
    }

    /// A full pointer drag through the widget pipeline.
    fn drag(&mut self, from: egui::Pos2, to: egui::Pos2) {
        self.events(vec![
            egui::Event::PointerMoved(from),
            egui::Event::PointerButton {
                pos: from,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            },
        ]);
        let mid = egui::pos2((from.x + to.x) / 2.0, (from.y + to.y) / 2.0);
        self.events(vec![egui::Event::PointerMoved(mid)]);
        self.events(vec![egui::Event::PointerMoved(to)]);
        self.events(vec![egui::Event::PointerButton {
            pos: to,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::default(),
        }]);
    }

    fn surface(&self) -> &crate::advanced::AdvancedState {
        self.app.advanced.as_ref().expect("config surface")
    }

    fn config_fetch_source(&self) -> Option<String> {
        let state = self.app.advanced.as_ref()?;
        state
            .model
            .entries
            .iter()
            .find(|entry| entry.table == "fetch" && entry.key == "source")
            .map(|entry| entry.value.trim().trim_matches('"').to_string())
    }

    fn root_value(&self, key: &str) -> Option<String> {
        let state = self.app.advanced.as_ref()?;
        let index = state.model.root_domain_index()?;
        state
            .model
            .domain_value(index, key)
            .map(|value| value.trim().to_string())
    }

    fn domain_value(&self, grid: u32, key: &str) -> Option<String> {
        let state = self.app.advanced.as_ref()?;
        let index = state.model.domain_index_for_grid(grid)?;
        state
            .model
            .domain_value(index, key)
            .map(|value| value.trim().to_string())
    }

    /// The route-flip invariant (the regression's failed GFS launch): surface and
    /// picker never disagree — custom.source == picker, custom.route ==
    /// picker's route, the emitted [fetch].source == picker.
    fn assert_source_invariant(&self) {
        let picker = self.app.draft.source();
        let custom = self.app.draft.custom.as_ref().expect("custom plan");
        assert_eq!(
            custom.source, picker.source,
            "custom.source follows the picker"
        );
        assert_eq!(
            custom.route, picker.route,
            "submitted route follows the picker"
        );
        assert_eq!(
            self.config_fetch_source().as_deref(),
            Some(picker.source),
            "the generated surface's [fetch].source equals the picker"
        );
    }
}

fn pinned(cycle: (i32, u32, u32, u32)) -> Option<chrono::NaiveDateTime> {
    chrono::NaiveDate::from_ymd_opt(cycle.0, cycle.1, cycle.2)
        .and_then(|date| date.and_hms_opt(cycle.3, 0, 0))
}

fn fixtures_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

/// A captured engine document, parsed through the shipping types.
fn fixture_resolve(name: &str) -> arwen_plan::queries::ResolveReport {
    let path = fixtures_dir().join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
}

fn fixture_estimate(name: &str) -> arwen_plan::queries::EstimateReport {
    let path = fixtures_dir().join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
}

// ---------------------------------------------------------------------------
// FIXTURE CELLS (always run)
// ---------------------------------------------------------------------------

/// CELLS sources×invariant [historical: ROUTE-FLIP — the regression's GFS launch
/// demanded era5-combined.grib]: the surface follows the picker across
/// gfs → era5 → hrrr, manual root geometry carried through each
/// regeneration.
#[test]
fn matrix_f01_source_invariant_and_route_follow() {
    let mut walk = Walk::fixture("f01");
    // GFS (default picker): a manual-size root through the same queue a
    // draw fills.
    walk.app.draft.domain = Some(arwen_map::LambertDomain::centered_at(
        35.5, -97.5, 300, 240, 3_000.0,
    ));
    let ctx = walk.ctx.clone();
    walk.app.handle_placement_edits(
        &ctx,
        vec![crate::map_pane::PlacementEdit::RootDrawn { nx: 300, ny: 240 }],
    );
    walk.pump_until("gfs surface", 10, |app| app.advanced.is_some());
    walk.assert_source_invariant();
    assert_eq!(walk.root_value("nx").as_deref(), Some("300"));
    // [matrix-found defect, permanent cell]: the prepared route reads
    // the WPS namelist by the CONFIG's stem — the edited copy must have
    // its own namelist beside it or `go` refuses at authority.
    let config_path =
        std::path::PathBuf::from(&walk.app.draft.custom.as_ref().unwrap().config_path);
    assert!(
        config_path
            .with_file_name("draft-config.namelist.wps")
            .is_file(),
        "edited config carries its namelist"
    );

    // Picker → ERA5 (pinned): the surface REGENERATES for it, manual
    // root carried.
    walk.app.draft.source_index = 2;
    walk.app.draft.cycle = pinned((2026, 8, 5, 0));
    walk.frame();
    walk.pump_until("era5 surface", 10, |app| {
        app.advanced.is_some()
            && app
                .draft
                .custom
                .as_ref()
                .is_some_and(|custom| custom.source == "era5")
    });
    walk.assert_source_invariant();
    assert_eq!(
        walk.root_value("nx").as_deref(),
        Some("300"),
        "manual root size carried through the source change"
    );

    // Picker → HRRR: prepared route again, source followed.
    walk.app.draft.source_index = 1;
    walk.frame();
    walk.pump_until("hrrr surface", 10, |app| {
        app.draft
            .custom
            .as_ref()
            .is_some_and(|custom| custom.source == "hrrr")
    });
    walk.assert_source_invariant();
}

/// CELLS redraw loop ×3 [DRAW-MODE-NEVER-YIELDS regression]: three full
/// redraw loops through real pointer input;
/// every drawn size lands manual, draw mode exits, confirm never
/// sticks.
#[test]
fn matrix_f02_redraw_loop_three_times() {
    let mut walk = Walk::fixture("f02");
    let rects = [
        (egui::pos2(350.0, 350.0), egui::pos2(900.0, 600.0)),
        (egui::pos2(300.0, 300.0), egui::pos2(1000.0, 520.0)),
        (egui::pos2(400.0, 380.0), egui::pos2(850.0, 640.0)),
    ];
    for (round, (from, to)) in rects.iter().enumerate() {
        if round == 0 {
            walk.app.map_pane.draw_mode = true;
        } else {
            walk.app.map_pane.request_draw_mode(true);
            assert!(walk.app.map_pane.confirm_redraw, "redraw asks first");
            walk.app.map_pane.confirm_redraw = false;
            walk.app.map_pane.draw_mode = true;
        }
        walk.drag(*from, *to);
        assert!(
            !walk.app.map_pane.draw_mode,
            "draw mode yields after round {round}"
        );
        assert!(!walk.app.map_pane.confirm_redraw, "no stuck confirm");
        let domain = walk.app.draft.domain.expect("domain drawn");
        walk.pump_until("surface with the drawn size", 10, move |app| {
            app.advanced.as_ref().is_some_and(|state| {
                state
                    .model
                    .root_domain_index()
                    .and_then(|index| state.model.domain_value(index, "nx"))
                    .map(str::trim)
                    == Some(domain.nx.to_string().as_str())
            })
        });
        assert!(
            crate::advanced::placement_is_manual(
                &walk.surface().model,
                &walk.surface().base_model,
                walk.surface().model.root_domain_index().unwrap()
            ),
            "round {round}: drawn root is manual"
        );
    }
}

/// CELLS children placement + root-resize clamp [historical: STRANDED
/// CHILDREN — "west-east high clearance is -33 parent rows"]: place
/// d02 east, shrink the root, d02 comes along; estimate re-arms.
#[test]
fn matrix_f03_children_placement_and_clamp() {
    let mut walk = Walk::fixture("f03");
    walk.app.draft.source_index = 2; // era5 — the nested fixture emission
    walk.app.draft.cycle = pinned((2026, 8, 5, 0));
    walk.app.draft.nests = vec![4];
    walk.app.draft.domain = Some(arwen_map::LambertDomain::centered_at(
        35.2, -97.4, 300, 240, 12_000.0,
    ));
    let ctx = walk.ctx.clone();
    walk.app.handle_placement_edits(
        &ctx,
        vec![crate::map_pane::PlacementEdit::RootDrawn { nx: 300, ny: 240 }],
    );
    walk.pump_until("surface", 10, |app| app.advanced.is_some());
    assert_eq!(
        walk.domain_value(2, "nx").as_deref(),
        Some("408"),
        "fixture d02"
    );

    // d02 far east (root 300 wide, clearance 10, span 101.75 → hi 188).
    walk.app.write_nest_placement(2, 188, 42, None);
    assert_eq!(
        walk.domain_value(2, "i_parent_start").as_deref(),
        Some("188")
    );

    // Shrink the root to 150 through the gesture path: d02 must come
    // along to the new limit 38 — never stranded, never refused.
    if let Some(domain) = &mut walk.app.draft.domain {
        domain.nx = 150;
    }
    let ctx = walk.ctx.clone();
    walk.app.handle_placement_edits(
        &ctx,
        vec![crate::map_pane::PlacementEdit::RootAdjusted { nx: 150, ny: 240 }],
    );
    assert_eq!(walk.root_value("nx").as_deref(), Some("150"));
    assert_eq!(
        walk.domain_value(2, "i_parent_start").as_deref(),
        Some("38"),
        "d02 carried along"
    );
    assert!(
        walk.surface()
            .repairs
            .iter()
            .any(|repair| repair.grid_id == 2 && repair.notice.contains("carried")),
        "{:?}",
        walk.surface().repairs
    );
    assert!(
        walk.surface().dirty_at.is_some(),
        "re-validation armed by itself"
    );
}

/// CELLS knobs + cycle spelling + estimate honesty [historical: STALE
/// ESTIMATE ("calculates once and never again") + CYCLE-FORMAT
/// refusal]: every knob change re-prices; the pinned spelling is the
/// one the wizard accepts; an errored estimate is a surfaced state.
#[test]
fn matrix_f04_estimate_lifecycle_and_cycle_spelling() {
    let mut walk = Walk::fixture("f04");
    walk.app.draft.domain = Some(arwen_map::LambertDomain::centered_at(
        35.5, -97.5, 300, 240, 3_000.0,
    ));
    walk.pump_until("first estimate", 10, |app| {
        matches!(app.estimate, Some(Ok(_))) && !app.estimate_is_stale()
    });
    let runs_before = walk.app.estimate_runs;

    // Cadence + length: each moves the fingerprint and re-prices.
    walk.app.draft.history_interval_s = 313;
    walk.app.draft.length_hours = 12.0;
    walk.frame();
    assert!(
        walk.app.estimate_is_stale(),
        "stale the moment the draft moves"
    );
    walk.pump_until("re-price", 10, move |app| {
        app.estimate_runs > runs_before && !app.estimate_is_stale()
    });
    let plan = walk.app.last_estimate_plan.clone().unwrap();
    assert!(plan.contains("\"history_interval_s\": 313"), "{plan}");
    assert!(plan.contains("\"hours\": 12"), "{plan}");

    // Physics profile rides the plan verbatim.
    walk.app.draft.physics_profile = Some("wsm6-ysu-mm5-noah-no-radiation-v1".into());
    let runs = walk.app.estimate_runs;
    walk.frame();
    walk.pump_until("profile re-price", 10, move |app| {
        app.estimate_runs > runs && !app.estimate_is_stale()
    });
    assert!(
        walk.app
            .last_estimate_plan
            .as_ref()
            .unwrap()
            .contains("wsm6-ysu-mm5-noah-no-radiation-v1")
    );

    // Pinned-cycle spelling: the ONE form the wizard accepts.
    walk.app.draft.cycle = pinned((2026, 8, 6, 12));
    let plan = walk.app.draft.to_plan("X").unwrap();
    let json = plan.to_json_pretty().unwrap();
    assert!(json.contains("\"cycle\": \"2026-08-06T12\""), "{json}");

    // An errored estimate NAMES its domain and is never silent.
    walk.app.estimate = Some(Err(
        "child domain grid_id = 2 (d02) violates the parent-row clearance rule".into(),
    ));
    assert_eq!(
        walk.app
            .placement_refusal_domains()
            .first()
            .map(|(g, _)| *g),
        Some(2)
    );
    walk.frame(); // the strip + map render the named refusal, no panic
}

/// CELLS fetch promotion [MISSING FETCH STAGE regression]: forcing absent → the plan
/// carries the config's own [fetch]; present → skipped; GFS-shaped
/// configs ride the chain's native fetch.
#[test]
fn matrix_f05_fetch_promotion() {
    let temp = temp_dir("f05");
    std::fs::create_dir_all(&temp).unwrap();
    let config_path = temp.join("draft-config.toml");
    let era5_shaped = "\
[experiment]\nname = \"m\"\nrun_seconds = 21600.0\n\n\
[[domain]]\ngrid_id = 1\nparent_id = 0\ni_parent_start = 1\nj_parent_start = 1\n\
parent_grid_ratio = 1\nnx = 204\nny = 162\ndx = 12000.0\n\n\
[fetch]\nsource = \"era5\"\ncycle = \"2026-08-05T00\"\nhours = 6\n\
area = \"23.67,-114.65,46.04,-80.15\"\ncadence = 6\n\n\
[case_data]\nforcing = [\"data/m/era5-combined.grib\"]\nvtable = \"Vtable.ERA5_CDO\"\n";
    std::fs::write(&config_path, era5_shaped).unwrap();
    let mut draft = crate::draft::Draft {
        domain: Some(arwen_map::LambertDomain::centered_at(
            35.5, -97.5, 204, 162, 12_000.0,
        )),
        custom: Some(crate::draft::CustomPlanConfig {
            config_path: config_path.to_string_lossy().into_owned(),
            route: "experiment".into(),
            source: "era5".into(),
            root_dx_km: 12.0,
            nests: Vec::new(),
            rev: 0,
        }),
        ..Default::default()
    };
    draft.source_index = 2;
    draft.root_dx_km = 12.0;

    // Forcing absent → the [fetch] block promotes, --out derived from
    // the [case_data] paths. The EXPERIMENT route never carries
    // run_options.geog_root (the engine refuses the key there; geog
    // rides the config's [case_data] instead — matrix-found).
    let plan = draft
        .to_plan_with_geog(temp.to_string_lossy().as_ref(), Some("C:/geog-tree"), &[])
        .unwrap();
    assert!(
        plan.run_options.geog_root.is_none(),
        "experiment route: no geog key"
    );
    let fetch = plan.fetch.as_ref().expect("fetch promoted");
    let args = fetch.args.join(" ");
    assert!(args.contains("--source era5"), "{args}");
    assert!(args.contains("--cycle 2026-08-05T00"), "{args}");
    assert!(args.contains("--hours 6"), "{args}");
    let expected_out = temp.join("data/m");
    assert!(
        args.contains(expected_out.to_string_lossy().as_ref()),
        "--out resolves to the declared forcing dir: {args}"
    );

    // Forcing on disk → no fetch block (prior run / manual staging).
    std::fs::create_dir_all(&expected_out).unwrap();
    std::fs::write(expected_out.join("era5-combined.grib"), b"x").unwrap();
    let plan = draft.to_plan(temp.to_string_lossy().as_ref()).unwrap();
    assert!(plan.fetch.is_none(), "on-disk forcing skips the download");
    assert_eq!(
        draft.forcing_plan(),
        Some(crate::draft::ForcingPlan::OnDisk)
    );

    // A GFS-shaped config (no [case_data]) rides the chain's own fetch.
    let gfs_shaped = std::fs::read_to_string(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/generated-config-gfs.toml"),
    )
    .unwrap();
    std::fs::write(&config_path, gfs_shaped).unwrap();
    draft.custom.as_mut().unwrap().source = "gfs".into();
    draft.custom.as_mut().unwrap().route = "prepared".into();
    draft.source_index = 0;
    let plan = draft.to_plan(temp.to_string_lossy().as_ref()).unwrap();
    assert!(plan.fetch.is_none());
    assert_eq!(
        draft.forcing_plan(),
        Some(crate::draft::ForcingPlan::RouteFetches {
            source: Some("gfs".into())
        })
    );

    // [matrix-found defect, permanent cell]: config-path plans must
    // carry the staged WPS_GEOG — without it a drawn-root launch
    // refused at prepare ("the staged WPS_GEOG tree is not usable").
    let plan = draft
        .to_plan_with_geog(temp.to_string_lossy().as_ref(), Some("C:/geog-tree"), &[])
        .unwrap();
    assert_eq!(
        plan.run_options.geog_root.as_deref(),
        Some("C:/geog-tree"),
        "config-path plans carry run_options.geog_root"
    );
    let _ = std::fs::remove_dir_all(&temp);
}

/// CELLS product selection (lifecycle's design half; run start /
/// close-reattach / registry reopen are the fixture-run e2e's cells —
/// tests/fixture_run_e2e.rs runs in the same command).
#[test]
fn matrix_f06_product_selection_rides_the_plan() {
    let mut walk = Walk::fixture("f06");
    walk.app.draft.domain = Some(arwen_map::LambertDomain::centered_at(
        35.5, -97.5, 300, 240, 3_000.0,
    ));
    walk.app.draft.render_mode = crate::draft::RenderMode::Custom;
    walk.app.draft.render_custom = vec!["sbcape".into(), "1km_reflectivity".into()];
    let plan = walk.app.draft.to_plan("X").unwrap();
    assert_eq!(
        plan.run_options.render_products.as_deref(),
        Some("sbcape,1km_reflectivity")
    );
    // Skip + engine-default are draft::tests cells
    // (render_selection_travels_verbatim_on_the_prepared_route_only).
}

/// CELLS WPS-pair coherence [matrix-found: manual root size stopped
/// `go` at prepare with "WPS/experiment domain geometry mismatch"]:
/// the namelist beside the config follows every geometry write —
/// staggered dims = mass + 1, anchors verbatim, refs from
/// [projection]; untouched keys byte-identical.
#[test]
fn matrix_f07_wps_namelist_follows_the_config() {
    // The live engine's own emission pair, verbatim.
    let namelist = [
        "&share",
        " wrf_core = 'ARW',",
        " max_dom = 1,",
        " interval_seconds = 10800,",
        " io_form_geogrid = 2,",
        "/",
        "&geogrid",
        " parent_id         = 1,",
        " parent_grid_ratio = 1,",
        " i_parent_start    = 1,",
        " j_parent_start    = 1,",
        " e_we              = 401,",
        " e_sn              = 321,",
        " geog_data_res     = 'default',",
        " dx = 3000,",
        " dy = 3000,",
        " map_proj = 'lambert',",
        " ref_lat   = 35.35,",
        " ref_lon   = -97.4,",
        " truelat1  = 25.35,",
        " truelat2  = 45.35,",
        " stand_lon = -97.4,",
        "/",
    ]
    .join("\n")
        + "\n";
    let namelist = namelist.as_str();
    let config = "[projection]\nmap_proj = \"lambert\"\nref_lat = 36.1000\n\
ref_lon = -98.2000\ntruelat1 = 25.35\ntruelat2 = 45.35\nstand_lon = -98.2000\n\n\
[[domain]]\ngrid_id = 1\nparent_id = 0\ni_parent_start = 1\nj_parent_start = 1\n\
parent_grid_ratio = 1\nnx = 199\nny = 100\ndx = 3000.0\n";
    let model = crate::advanced::ConfigModel::parse(config);
    let synced = crate::app::rewrite_wps_namelist(namelist, &model);
    assert!(synced.contains(" e_we = 200,"), "{synced}");
    assert!(synced.contains(" e_sn = 101,"), "{synced}");
    assert!(synced.contains(" ref_lat = 36.1000,"));
    assert!(synced.contains(" ref_lon = -98.2000,"));
    assert!(synced.contains(" stand_lon = -98.2000,"));
    // Untouched keys survive byte-identical; truelats stay the engine's.
    assert!(synced.contains(" truelat1  = 25.35,"));
    assert!(synced.contains(" interval_seconds = 10800,"));
    assert!(synced.contains(" geog_data_res     = 'default',"));
    // A 12-3 pair: per-domain lists in emission order.
    let nested = [
        "&geogrid",
        " i_parent_start    = 1, 52,",
        " j_parent_start    = 1, 42,",
        " e_we              = 205, 409,",
        " e_sn              = 163, 321,",
        "/",
    ]
    .join("\n")
        + "\n";
    let nested = nested.as_str();
    let config = "[[domain]]\ngrid_id = 1\nparent_id = 0\ni_parent_start = 1\n\
j_parent_start = 1\nparent_grid_ratio = 1\nnx = 150\nny = 162\n\n\
[[domain]]\ngrid_id = 2\nparent_id = 1\ni_parent_start = 38\nj_parent_start = 42\n\
parent_grid_ratio = 4\nnx = 408\nny = 320\n";
    let model = crate::advanced::ConfigModel::parse(config);
    let synced = crate::app::rewrite_wps_namelist(nested, &model);
    assert!(synced.contains(" i_parent_start = 1, 38,"), "{synced}");
    assert!(synced.contains(" e_we = 151, 409,"), "{synced}");
    assert!(synced.contains(" e_sn = 163, 321,"), "{synced}");
}

/// CELLS the route's OTHER side files [matrix-found by live cell l09:
/// "the HRRR route reads ['namelist_input', 'stock_namelist_input',
/// 'target_domain'] beside draft-config.toml ... this config was not
/// emitted for the HRRR route"]. Two halves, both were broken:
/// the edited copy must CARRY every side file the emission wrote, and
/// each must FOLLOW the working config's geometry — the HRRR route reads
/// them INSTEAD of the TOML, so a drawn root that moved only the TOML ran
/// the wizard's fitted size and said nothing.
#[test]
fn matrix_f11_route_side_files_are_carried_and_kept_true() {
    let temp = temp_dir("f11");
    std::fs::create_dir_all(&temp).unwrap();
    let config = "[projection]\nmap_proj = \"lambert\"\nref_lat = 36.1\n\
ref_lon = -98.2\ntruelat1 = 25.35\ntruelat2 = 45.35\nstand_lon = -98.2\n\n\
[shared]\nnz = 49\n\n\
[[domain]]\ngrid_id = 1\nparent_id = 0\ni_parent_start = 1\nj_parent_start = 1\n\
parent_grid_ratio = 1\nnx = 200\nny = 100\ndx = 3000.0\ntime_step = 15\n";
    let model = crate::advanced::ConfigModel::parse(config);

    // The WRF namelist pair: staggered dims, anchors, dx/dy, the clock.
    let namelist_input = [
        "&domains",
        " time_step                           = 18,",
        " time_step_sound                     = 4,",
        " max_dom                             = 1,",
        " e_we                                = 481,",
        " e_sn                                = 385,",
        " e_vert                              = 50,",
        " dx                                  = 3000.0,",
        " dy                                  = 3000.0,",
        " i_parent_start                      = 1,",
        " j_parent_start                      = 1,",
        " num_metgrid_levels                  = 51,",
        "/",
    ]
    .join("\n")
        + "\n";
    let synced = crate::app::rewrite_wrf_namelist_input(&namelist_input, &model);
    assert!(synced.contains(" e_we = 201,"), "{synced}");
    assert!(synced.contains(" e_sn = 101,"), "{synced}");
    assert!(synced.contains(" time_step = 15,"), "{synced}");
    assert!(synced.contains(" dx = 3000.0,"), "{synced}");
    assert!(synced.contains(" dy = 3000.0,"), "{synced}");
    // Keys that are not geometry survive byte-identical, and a key the
    // file does not carry is never invented.
    assert!(
        synced.contains(" time_step_sound                     = 4,"),
        "{synced}"
    );
    assert!(
        synced.contains(" num_metgrid_levels                  = 51,"),
        "{synced}"
    );
    assert!(!synced.contains("ref_lat"), "{synced}");

    // The target-domain JSON: the route's own schema, values only.
    let target = "{\n  \"dx_m\": 3000.0,\n  \"dy_m\": 3000.0,\n  \
\"map_proj\": \"lambert\",\n  \"name\": \"x\",\n  \"nx\": 480,\n  \"ny\": 384,\n  \
\"nz\": 49,\n  \"ref_lat\": 35.35,\n  \"ref_lon\": -97.4,\n  \
\"schema\": \"gpuwm-hrrr-target-domain-v1\",\n  \"stand_lon\": -97.4,\n  \
\"time_step_seconds\": 18,\n  \"truelat1\": 25.35,\n  \"truelat2\": 45.35\n}\n";
    let synced = crate::app::rewrite_target_domain_json(target, &model)
        .expect("the target document rewrites");
    let parsed: serde_json::Value = serde_json::from_str(&synced).unwrap();
    assert_eq!(parsed["nx"], 200);
    assert_eq!(parsed["ny"], 100);
    assert_eq!(parsed["ref_lat"], 36.1);
    assert_eq!(parsed["ref_lon"], -98.2);
    assert_eq!(parsed["stand_lon"], -98.2);
    assert_eq!(parsed["time_step_seconds"], 15);
    assert_eq!(parsed["nz"], 49);
    // Identity keys are the ENGINE's and are never touched.
    assert_eq!(parsed["schema"], "gpuwm-hrrr-target-domain-v1");
    assert_eq!(parsed["name"], "x");
    assert_eq!(parsed["map_proj"], "lambert");

    // sync_config_side_files touches every one it finds, and only those.
    let config_path = temp.join("draft-config.toml");
    std::fs::write(&config_path, config).unwrap();
    for (suffix, body) in [
        ("namelist.input", namelist_input.as_str()),
        ("stock.namelist.input", namelist_input.as_str()),
        ("d01-target.json", target),
    ] {
        std::fs::write(temp.join(format!("draft-config.{suffix}")), body).unwrap();
    }
    std::fs::write(temp.join("unrelated.json"), target).unwrap();
    crate::app::sync_config_side_files(&config_path, &model);
    for suffix in ["namelist.input", "stock.namelist.input"] {
        let text = std::fs::read_to_string(temp.join(format!("draft-config.{suffix}"))).unwrap();
        assert!(text.contains(" e_we = 201,"), "{suffix}: {text}");
    }
    let text = std::fs::read_to_string(temp.join("draft-config.d01-target.json")).unwrap();
    assert!(text.contains("\"nx\": 200"), "{text}");
    assert_eq!(
        std::fs::read_to_string(temp.join("unrelated.json")).unwrap(),
        target,
        "a file that is not this config's side file is never rewritten"
    );
    // A missing side file is simply not written (single-source routes).
    assert!(!temp.join("draft-config.namelist.wps").exists());
    let _ = std::fs::remove_dir_all(&temp);
}

/// CELLS children survive a dx/ladder change [historical: THE REGRESSION'S -74
/// REPEAT, forecast-20260808-0546 — "west-east high clearance is -74
/// parent rows": the 12-3 pick rescaled the parent's cell grid around
/// cell-indexed children and the carry never fired]: the surface
/// FOLLOWS the dx + ladder cards; the landing carry clamps/refits/
/// SHRINKS children into the footprint-true root, with notices.
#[test]
fn matrix_f08_children_survive_dx_and_ladder_changes() {
    let mut walk = Walk::fixture("f08");
    walk.app.draft.source_index = 2; // era5 — the nested fixture emission
    walk.app.draft.cycle = pinned((2026, 8, 5, 0));
    // the regression step 1: draw at 3 km (footprint ~660 × 378 km).
    walk.app.draft.domain = Some(arwen_map::LambertDomain::centered_at(
        35.5, -97.5, 220, 126, 3_000.0,
    ));
    let ctx = walk.ctx.clone();
    walk.app.handle_placement_edits(
        &ctx,
        vec![crate::map_pane::PlacementEdit::RootDrawn { nx: 220, ny: 126 }],
    );
    walk.pump_until("surface", 10, |app| app.advanced.is_some());

    // the regression step 2: pick the 12-3 ladder — dx 12 (sketch keeps the
    // footprint: 220×126 @ 3 km → 55×32 @ 12 km) + a ratio-4 child.
    walk.app.draft.set_root_dx_km(12.0);
    walk.app.draft.nests = vec![4];
    walk.frame(); // follow_intent_cards fires
    walk.pump_until("regenerated surface follows the cards", 10, |app| {
        app.draft
            .custom
            .as_ref()
            .is_some_and(|custom| custom.root_dx_km == 12.0 && custom.nests == vec![4])
            && app.advanced.as_ref().is_some_and(|state| {
                state.model.domain_index_for_grid(2).is_some()
                    && state
                        .model
                        .root_domain_index()
                        .and_then(|index| state.model.domain_value(index, "nx"))
                        .map(str::trim)
                        == Some("55")
            })
    });

    // The child came out INSIDE the envelope — never the regression's strand.
    let parent_nx: f64 = walk.root_value("nx").unwrap().parse().unwrap();
    let parent_ny: f64 = walk.root_value("ny").unwrap().parse().unwrap();
    let i: f64 = walk
        .domain_value(2, "i_parent_start")
        .unwrap()
        .parse()
        .unwrap();
    let j: f64 = walk
        .domain_value(2, "j_parent_start")
        .unwrap()
        .parse()
        .unwrap();
    let nx: f64 = walk.domain_value(2, "nx").unwrap().parse().unwrap();
    let ny: f64 = walk.domain_value(2, "ny").unwrap().parse().unwrap();
    let ratio = 4.0;
    let clearance = 10.0;
    let hi_clear_i = parent_nx - (i + (nx - 1.0) / ratio) - clearance;
    let hi_clear_j = parent_ny - (j + (ny - 1.0) / ratio) - clearance;
    assert!(
        i >= 11.0 && j >= 11.0 && hi_clear_i >= 0.0 && hi_clear_j >= 0.0,
        "child inside the clearance envelope: i={i} j={j} {nx}×{ny} in \
         {parent_nx}×{parent_ny} (hi clearance {hi_clear_i}/{hi_clear_j})"
    );
    assert!(
        walk.surface()
            .repairs
            .iter()
            .any(|repair| repair.grid_id == 2 && repair.notice.contains("shrunk to fit")),
        "the shrink rung said what it did: {:?}",
        walk.surface().repairs
    );
    // The namelist followed the new grid too (dx + staggered dims).
    let config_path =
        std::path::PathBuf::from(&walk.app.draft.custom.as_ref().unwrap().config_path);
    walk.pump_until("namelist sync flushed", 10, move |app| {
        app.advanced
            .as_ref()
            .is_some_and(|state| state.dirty_at.is_none())
            && config_path
                .with_file_name("draft-config.namelist.wps")
                .is_file()
    });
}

/// CELLS the fresh session IS the true default, and a nested surface
/// never masquerades as one [fresh-default regression]: a fresh app starts at the
/// runnable golden path (GFS · latest · single · 3 km, nothing
/// persisted), and dropping the nests regenerates a nested surface
/// back to a single domain — no leak in either direction.
#[test]
fn matrix_f09_fresh_default_is_true_and_nested_surfaces_never_leak() {
    let mut walk = Walk::fixture("f09");
    // The true default: the shape the engine runs end to end today.
    assert_eq!(walk.app.draft.source().source, "gfs");
    assert!(walk.app.draft.nests.is_empty(), "single domain by default");
    assert_eq!(walk.app.draft.root_dx_km, 3.0);
    assert!(walk.app.draft.cycle.is_none(), "latest cycle");
    assert!(walk.app.draft.custom.is_none(), "no persisted surface");
    assert!(walk.app.route_block().is_none(), "the default is runnable");

    // A nested surface exists; the one-click remedy drops the nests and
    // the surface FOLLOWS back to a single domain.
    walk.app.draft.source_index = 2;
    walk.app.draft.cycle = pinned((2026, 8, 5, 0));
    walk.app.draft.nests = vec![4];
    walk.app.draft.domain = Some(arwen_map::LambertDomain::centered_at(
        35.5, -97.5, 300, 240, 3_000.0,
    ));
    let ctx = walk.ctx.clone();
    walk.app.handle_placement_edits(
        &ctx,
        vec![crate::map_pane::PlacementEdit::RootDrawn { nx: 300, ny: 240 }],
    );
    walk.pump_until("nested surface", 10, |app| {
        app.advanced
            .as_ref()
            .is_some_and(|state| state.model.domain_index_for_grid(2).is_some())
    });
    let actions = crate::inspector::InspectorActions {
        make_single_domain: true,
        ..Default::default()
    };
    walk.app.handle_actions(&ctx, actions);
    walk.frame();
    walk.pump_until("surface follows back to single", 10, |app| {
        app.advanced.as_ref().is_some_and(|state| {
            state.model.domain_index_for_grid(2).is_none()
                && state.model.domain_index_for_grid(1).is_some()
        })
    });
    assert!(walk.app.route_block().is_none());
}

/// CELLS a known-unrunnable combo NEVER gets a launchable button
/// [known-refusal regression]: the block
/// carries the engine's sentence + remedies, the launch guard refuses
/// even a raced click, and the remedy makes it runnable again.
///
/// AT gpuwm 1.8.6 THE BLOCKED TABLE IS EMPTY - every source x shape this
/// UI can express runs (live cells l07/l08/l09/l10). So this cell walks
/// two things instead of a real refusal: that nothing shipped is
/// blocked, and that THE GUARD STILL WORKS, driven through the
/// test-only `force_route_block`. The defect was a launch button offered into
/// a known engine refusal, and that guard stays proven after the last row clears.
#[test]
fn matrix_f10_unrunnable_combo_blocks_with_reason_and_remedies() {
    let mut walk = Walk::fixture("f10");
    walk.app.draft.domain = Some(arwen_map::LambertDomain::centered_at(
        35.5, -97.5, 300, 240, 3_000.0,
    ));
    // the regression's original defect shape, GFS 12-3, and every other shipped
    // combination: RUNNABLE at 1.8.6, no button withheld.
    for source_index in 0..crate::draft::SOURCE_PRESETS.len() {
        walk.app.draft.source_index = source_index;
        for nests in [vec![], vec![4], vec![4, 3]] {
            walk.app.draft.nests = nests.clone();
            assert!(
                walk.app.route_block().is_none(),
                "{} with ladder {nests:?} must be launchable at 1.8.6: {:?}",
                walk.app.draft.source().label,
                walk.app.route_block()
            );
        }
    }

    // THE GUARD, with a row that does exist: the Run path renders it and
    // a raced launch refuses on the same table rather than starting.
    walk.app.draft.source_index = 0;
    walk.app.draft.nests = vec![4];
    walk.app.draft.force_route_block = Some(
        "the engine's own refusal sentence · Remedies: Single domain \
         (one click, keeps your drawn root)"
            .into(),
    );
    let block = walk.app.route_block().expect("the forced row blocks");
    assert!(block.contains("Single domain (one click"), "{block}");
    let ctx = walk.ctx.clone();
    let actions = crate::inspector::InspectorActions {
        launch: true,
        ..Default::default()
    };
    walk.app.handle_actions(&ctx, actions);
    assert!(
        walk.app.session.is_none(),
        "no session into a known refusal"
    );
    assert!(
        walk.app
            .status_text()
            .is_some_and(|status| status.contains("the engine's own refusal sentence")),
        "the guard says why, in the engine's words: {:?}",
        walk.app.status_text()
    );
    // And the one-click remedy is still wired to the ladder.
    walk.app.draft.force_route_block = None;
    let actions = crate::inspector::InspectorActions {
        make_single_domain: true,
        ..Default::default()
    };
    walk.app.handle_actions(&ctx, actions);
    assert!(
        walk.app.draft.nests.is_empty(),
        "the remedy drops the nests"
    );
    assert!(walk.app.route_block().is_none());
}

/// CELLS the MOVING-NEST GATE on the prepared routes, both directions
/// [the sharp edge 1.8.4 documented rather than blocked: the storm cards
/// were not route-aware, so arming a follow on GFS/HRRR launched and was
/// refused by the tree-forecast stage after the fetch and both
/// preparation stages — minutes in].
///
/// IT FLIPPED, AND THIS CELL IS WHY THAT COST NOTHING. The row was keyed
/// on a CAPABILITY PROBE and not on a version string, so when gpuwm
/// 1.8.6 began answering `--resolve` with a `moving_nest` record the row
/// opened with no Studio edit. This cell walks BOTH engines by feeding
/// the two REAL replies — `resolve-nested.json` (the box's 1.8.5, no
/// such block) and `resolve-moving-nest.json` (the released 1.8.6, for a
/// GFS prepared follow config) — so the SHUT side stays covered after
/// the flip. It has to: an older venv, a rollback, or a future chain
/// with no delivery all land back in it, and cell l11 (now a completing
/// moving-nest run) only ever walks the open one.
///
/// The refusal arm is a HISTORICAL capture and is kept deliberately.
/// 1.8.6 seals a corridor on `prepared:hrrr` too, so that sentence is
/// not what the box says today — but "the engine's refusal reaches the
/// user verbatim" is a permanent property of the guard, and the engine
/// still refuses any chain with no follow-statics delivery. Testing it
/// on the one real refusal text this project has captured beats testing
/// it on a string Studio invented.
#[test]
fn matrix_f12_moving_nest_on_a_prepared_route_blocks_up_front_and_flips() {
    let mut walk = Walk::fixture("f12");
    walk.app.draft.root_dx_km = 12.0;
    walk.app.draft.nests = vec![4];
    walk.app.draft.domain = Some(arwen_map::LambertDomain::centered_at(
        35.5, -97.5, 300, 240, 12_000.0,
    ));
    let ctx = walk.ctx.clone();
    walk.app.handle_placement_edits(
        &ctx,
        vec![crate::map_pane::PlacementEdit::RootDrawn { nx: 300, ny: 240 }],
    );
    walk.pump_until("nested surface", 10, |app| {
        app.advanced
            .as_ref()
            .is_some_and(|state| state.model.domain_index_for_grid(2).is_some())
    });
    assert!(
        walk.app.route_block().is_none(),
        "a still GFS tree is runnable"
    );

    // THE TOGGLE, through the real card action: follow enabled on the
    // GFS (prepared) surface. The tables land in the config — the cards
    // are not route-gated, and must not be: the declaration is the
    // user's, the consequence is the engine's to state.
    let actions = crate::inspector::InspectorActions {
        storm: crate::storm::StormActions {
            apply_follow: Some(Some(crate::storm::FollowSettings::default())),
            ..Default::default()
        },
        ..Default::default()
    };
    walk.app.handle_actions(&ctx, actions);
    walk.frame();
    let text = &walk.surface().model.text;
    assert!(
        text.contains("[relocation]"),
        "the prepared-route emission carries it"
    );
    assert!(text.contains("[relocation.follow]"), "with a follow SOURCE");
    assert_eq!(
        crate::storm::declares_follow_source(&walk.surface().model),
        Some("tracker"),
        "Studio's predicate agrees with the engine's: enabled + a source"
    );

    // ARM 1 — the box's released engine. Its real reply carries no
    // moving_nest block, so Launch is BLOCKED up front with the honest
    // reason instead of dying at the tree stage minutes in.
    walk.app.advanced.as_mut().unwrap().resolve =
        Some(Ok(Box::new(fixture_resolve("resolve-nested.json"))));
    let block = walk
        .app
        .route_block()
        .expect("a moving nest on a prepared route is not launchable today");
    assert!(block.contains("need the next engine update"), "{block}");
    assert!(block.contains("ERA5 runs them today"), "{block}");
    // And the guard refuses a RACED launch on the same table.
    let actions = crate::inspector::InspectorActions {
        launch: true,
        ..Default::default()
    };
    walk.app.handle_actions(&ctx, actions);
    assert!(
        walk.app.session.is_none(),
        "no session into a known refusal"
    );
    assert!(
        walk.app
            .status_text()
            .is_some_and(|status| status.contains("need the next engine update")),
        "the guard says why: {:?}",
        walk.app.status_text()
    );

    // THE REMEDY THE ROW PROMISES, in one click: strip [relocation] and
    // the run goes ahead with the nest where it is. (Dropping the nests
    // would delete the thing you were trying to move, which is why this
    // row does NOT offer the single-domain remedy.)
    let actions = crate::inspector::InspectorActions {
        storm: crate::storm::StormActions {
            apply_follow: Some(None),
            ..Default::default()
        },
        ..Default::default()
    };
    walk.app.handle_actions(&ctx, actions);
    walk.frame();
    assert!(
        !walk.surface().model.text.contains("[relocation]"),
        "the remedy strips the tables"
    );
    assert!(
        !walk.app.draft.nests.is_empty(),
        "and leaves the ladder alone"
    );
    assert!(
        walk.app.route_block().is_none(),
        "a still tree on GFS is runnable again: {:?}",
        walk.app.route_block()
    );
    // Put it back for the remaining arms.
    let actions = crate::inspector::InspectorActions {
        storm: crate::storm::StormActions {
            apply_follow: Some(Some(crate::storm::FollowSettings::default())),
            ..Default::default()
        },
        ..Default::default()
    };
    walk.app.handle_actions(&ctx, actions);
    walk.frame();

    // ARM 2 — the ENGINE's refusal, when it has one, reaches the user in
    // the engine's own words. Real `gpuwm run-plan` stderr for a
    // nested-HRRR follow plan, byte for byte, captured at 1.8.5. That
    // chain SEALS A CORRIDOR at 1.8.6 and no longer says this — the
    // capture is kept because the verbatim-carriage guard must stay
    // covered by an engine sentence rather than a Studio-invented one.
    let refusal = std::fs::read_to_string(fixtures_dir().join("refusal-moving-nest-hrrr.txt"))
        .expect("the captured refusal");
    walk.app.advanced.as_mut().unwrap().resolve = Some(Err(refusal.clone()));
    let block = walk
        .app
        .route_block()
        .expect("a refused moving nest blocks");
    let engine = crate::storm::engine_sentence(&refusal);
    assert!(
        block.starts_with(engine),
        "the engine's sentence, verbatim and first:\n{block}"
    );
    assert!(
        engine.contains("gpuwm-prepared-tree-forecast's preflight refused it"),
        "the captured refusal is the moving-nest one: {engine}"
    );

    // ARM 3 — THE FLIP. The engine reports a decision for this config →
    // the row opens, with no Studio change of any kind.
    walk.app.advanced.as_mut().unwrap().resolve =
        Some(Ok(Box::new(fixture_resolve("resolve-moving-nest.json"))));
    assert!(
        walk.app.route_block().is_none(),
        "the row clears on the engine's own moving_nest decision: {:?}",
        walk.app.route_block()
    );
    let decision = match walk.app.moving_nest() {
        crate::storm::MovingNest::Reported(decision) => decision,
        other => panic!("the engine's decision must be read back: {other:?}"),
    };
    assert_eq!(decision.chain, "prepared:go");
    assert_eq!(decision.delivery.as_deref(), Some("statics_corridor"));
    assert!(decision.statics_corridor, "the preparation seals one");
    let (line, alert) = crate::storm::moving_nest_line(&walk.app.moving_nest(), "prepared")
        .expect("the card states the decision");
    assert!(!alert, "a reported decision is not an alarm: {line}");
    assert!(line.contains("statics corridor"), "{line}");

    // ERA5 IS UNTOUCHED BY ALL OF IT. The experiment route feeds a
    // moving nest from the config's own geography source and has run
    // them from Studio since the storm cards shipped — blocking it
    // would withhold a working feature.
    walk.app.advanced.as_mut().unwrap().resolve =
        Some(Ok(Box::new(fixture_resolve("resolve-nested.json"))));
    walk.app.draft.source_index = 2;
    if let Some(custom) = walk.app.draft.custom.as_mut() {
        custom.source = "era5".into();
        custom.route = "experiment".into();
    }
    assert!(
        walk.app.route_block().is_none(),
        "ERA5 runs moving nests today: {:?}",
        walk.app.route_block()
    );
}

/// CELLS the UNBLOCKED FLOW, on engine documents: the follow tables ride
/// a prepared-route emission (including ACROSS a source switch, which
/// used to drop them on the floor), the resolve carries the decision,
/// and the corridor cost is rendered from the engine's own `--estimate`
/// BESIDE the VRAM figure — never inside it.
#[test]
fn matrix_f13_prepared_route_follow_emission_and_corridor_cost() {
    let mut walk = Walk::fixture("f13");
    // Arm the follow on ERA5 first, then switch the picker to GFS: the
    // surface regenerates for the prepared route and the declaration
    // has to survive it, exactly as manual geometry does.
    walk.app.draft.source_index = 2;
    walk.app.draft.cycle = pinned((2026, 8, 5, 0));
    walk.app.draft.root_dx_km = 12.0;
    walk.app.draft.nests = vec![4];
    walk.app.draft.domain = Some(arwen_map::LambertDomain::centered_at(
        35.5, -97.5, 300, 240, 12_000.0,
    ));
    let ctx = walk.ctx.clone();
    walk.app.handle_placement_edits(
        &ctx,
        vec![crate::map_pane::PlacementEdit::RootDrawn { nx: 300, ny: 240 }],
    );
    walk.pump_until("era5 nested surface", 10, |app| {
        app.advanced
            .as_ref()
            .is_some_and(|state| state.model.domain_index_for_grid(2).is_some())
    });
    let actions = crate::inspector::InspectorActions {
        storm: crate::storm::StormActions {
            apply_follow: Some(Some(crate::storm::FollowSettings::default())),
            ..Default::default()
        },
        ..Default::default()
    };
    walk.app.handle_actions(&ctx, actions);
    walk.frame();
    assert!(walk.surface().model.text.contains("[relocation.follow]"));

    // THE SOURCE SWITCH: era5 → gfs. The prepared emission must carry
    // the moving nest, or arming one on ERA5 and changing your mind
    // about the source silently produces a still run.
    walk.app.draft.source_index = 0;
    walk.pump_until("prepared surface carries the follow tables", 15, |app| {
        app.draft
            .custom
            .as_ref()
            .is_some_and(|custom| custom.source == "gfs")
            && app
                .advanced
                .as_ref()
                .is_some_and(|state| state.model.text.contains("[relocation.follow]"))
    });
    walk.assert_source_invariant();
    assert_eq!(
        crate::storm::declares_follow_source(&walk.surface().model),
        Some("tracker"),
        "the declaration survived the regeneration into the prepared route"
    );

    // The fixture launcher answers a FOLLOW config with the engine's own
    // follow reply, so the flow above lands a real decision.
    walk.pump_until("the engine's moving-nest decision", 15, |app| {
        matches!(app.moving_nest(), crate::storm::MovingNest::Reported(_))
    });

    // THE CORRIDOR COST, from the engine's `--estimate`. Both arms are
    // real captures of the same GFS tree, one following and one still.
    let follow = fixture_estimate("estimate-corridor.json");
    let still = fixture_estimate("estimate-corridor-still.json");
    let corridor = follow.corridor.as_ref().expect("the follow arm is priced");
    assert!(corridor.is_priced());
    assert_eq!(corridor.host_bytes, Some(410_323_968));
    assert_eq!(corridor.host_gib, Some(0.3821));
    assert_eq!(corridor.domains.len(), 1, "every CHILD is priced");
    assert_eq!(corridor.domains[0].domain, "d02");
    assert_eq!(corridor.domains[0].corridor_nx, Some(816));
    assert_eq!(corridor.domains[0].corridor_ny, Some(648));
    // NOT VRAM, and not by Studio's assertion — by the engine's two
    // arms agreeing to the byte.
    assert_eq!(
        follow.vram.estimate_bytes, still.vram.estimate_bytes,
        "a corridor changes no VRAM figure"
    );
    assert_eq!(follow.vram.estimate_bytes, Some(6_858_962_954));
    let still_corridor = still
        .corridor
        .as_ref()
        .expect("the block is always emitted");
    assert!(
        !still_corridor.is_priced(),
        "zero, with the basis that says why"
    );
    assert!(!still_corridor.basis.is_empty());

    // What the user READS in the Resources strip.
    let (line, basis) =
        crate::inspector::corridor_line(&follow).expect("the strip shows the corridor");
    assert!(line.contains("host/disk"), "{line}");
    assert!(line.contains("adds no VRAM"), "{line}");
    assert!(line.contains("d02 816×648"), "{line}");
    assert!(
        basis.contains("cropped on the host"),
        "the engine's basis on hover"
    );
    assert!(
        crate::inspector::corridor_line(&still).is_none(),
        "a still plan gets no corridor line at all"
    );

    // BOTH PREPARED CHAINS, priced apart. 1.8.6 seals a corridor on
    // `prepared:hrrr` as well, and it is a DIFFERENT corridor — same
    // ladder, different root extent — so a fixture that answered one
    // chain's number for the other would put a wrong figure on screen
    // under a right-looking label.
    let hrrr_resolve = fixture_resolve("resolve-moving-nest-hrrr.json");
    let hrrr_decision = hrrr_resolve
        .moving_nest
        .as_ref()
        .expect("1.8.6 reports a decision on the HRRR chain too");
    assert_eq!(hrrr_decision.chain, "prepared:hrrr");
    assert_eq!(hrrr_decision.delivery.as_deref(), Some("statics_corridor"));
    assert!(hrrr_decision.statics_corridor);
    let hrrr = fixture_estimate("estimate-corridor-hrrr.json");
    let hrrr_corridor = hrrr.corridor.as_ref().expect("the HRRR arm is priced");
    assert_eq!(hrrr_corridor.host_bytes, Some(330_594_624));
    assert_eq!(hrrr_corridor.domains[0].corridor_nx, Some(732));
    assert_eq!(hrrr_corridor.domains[0].corridor_ny, Some(582));
    assert_ne!(
        hrrr_corridor.host_bytes, corridor.host_bytes,
        "the two chains must not be quoted the same number"
    );
    let (hrrr_line, _) = crate::inspector::corridor_line(&hrrr).expect("the strip shows it");
    assert!(hrrr_line.contains("d02 732×582"), "{hrrr_line}");
}

// ---------------------------------------------------------------------------
// LIVE CELLS (#[ignore] — run with --include-ignored on the box whose
// settings.json points at the real engine). Resolves/estimates are the
// REAL engine's; launch cells stop at the plan boundary (the standing
// order's runtime-cost line) — the full GFS fetch→forecast walk is the
// --demo-drawroot --demo-run packaged-exe smoke.
// ---------------------------------------------------------------------------

/// LIVE gfs · drawn manual root · latest cycle: the engine accepts the
/// user's shape, prices it, and the launch plan is route-coherent.
#[test]
#[ignore]
fn matrix_l01_live_gfs_manual_root() {
    let mut walk = Walk::live("l01");
    walk.app.draft.domain = Some(arwen_map::LambertDomain::centered_at(
        35.35, -97.4, 200, 100, 3_000.0,
    ));
    let ctx = walk.ctx.clone();
    walk.app.handle_placement_edits(
        &ctx,
        vec![crate::map_pane::PlacementEdit::RootDrawn { nx: 200, ny: 100 }],
    );
    walk.pump_until("live gfs surface", 120, |app| app.advanced.is_some());
    walk.assert_source_invariant();
    assert_eq!(walk.root_value("nx").as_deref(), Some("200"));
    walk.pump_until("live resolve accepts the 2:1 root", 120, |app| {
        app.advanced
            .as_ref()
            .is_some_and(|state| matches!(state.resolve, Some(Ok(_))))
    });
    walk.pump_until("live estimate priced, not stale", 120, |app| {
        matches!(app.estimate, Some(Ok(_))) && !app.estimate_is_stale()
    });
    let plan = walk.app.draft.to_plan("X").unwrap();
    assert_eq!(plan.route, "prepared");
    assert!(plan.fetch.is_none(), "gfs chain fetches natively");
    // The staged WPS_GEOG rides the SUBMITTED plan whenever the box
    // has one configured (the matrix-found prepare refusal).
    if StudioSettings::load_or_default().geog_root.is_some() {
        assert!(
            walk.app
                .last_estimate_plan
                .as_ref()
                .is_some_and(|plan| plan.contains("geog_root")),
            "the submitted plan carries the staged WPS_GEOG"
        );
    }
}

/// LIVE era5 · 12-3 · pinned · children carry [THE stranded cell]: d02
/// east, root shrunk, the carried config is RE-RATIFIED BY THE ENGINE
/// (zero refusal), and the launch plan carries the promoted fetch.
#[test]
#[ignore]
fn matrix_l02_live_era5_children_carry_and_fetch() {
    let mut walk = Walk::live("l02");
    walk.app.draft.source_index = 2;
    walk.app.draft.cycle = pinned((2026, 8, 5, 0));
    walk.app.draft.root_dx_km = 12.0;
    walk.app.draft.nests = vec![4];
    walk.app.draft.domain = Some(arwen_map::LambertDomain::centered_at(
        35.2, -97.4, 204, 162, 12_000.0,
    ));
    let ctx = walk.ctx.clone();
    walk.app.handle_placement_edits(
        &ctx,
        vec![crate::map_pane::PlacementEdit::RootDrawn { nx: 204, ny: 162 }],
    );
    walk.pump_until("live era5 surface", 180, |app| app.advanced.is_some());
    walk.assert_source_invariant();
    walk.pump_until("first live resolve", 180, |app| {
        app.advanced
            .as_ref()
            .is_some_and(|state| matches!(state.resolve, Some(Ok(_))))
    });

    // d02 east, then shrink the root: the carry must produce a config
    // the ENGINE accepts without user action.
    let d02_nx: f64 = walk.domain_value(2, "nx").unwrap().parse().unwrap();
    let ratio: f64 = walk
        .domain_value(2, "parent_grid_ratio")
        .unwrap()
        .parse()
        .unwrap();
    let clearance = 10.0;
    let hi = (204.0 - clearance - (d02_nx - 1.0) / ratio).floor() as i64;
    let j: i64 = walk
        .domain_value(2, "j_parent_start")
        .unwrap()
        .parse::<f64>()
        .unwrap()
        .round() as i64;
    walk.app.write_nest_placement(2, hi, j, None);
    if let Some(domain) = &mut walk.app.draft.domain {
        domain.nx = 153;
    }
    let ctx = walk.ctx.clone();
    walk.app.handle_placement_edits(
        &ctx,
        vec![crate::map_pane::PlacementEdit::RootAdjusted { nx: 153, ny: 162 }],
    );
    assert!(
        walk.surface()
            .repairs
            .iter()
            .any(|repair| repair.grid_id == 2),
        "carry recorded"
    );
    walk.pump_until("engine re-ratifies the carried config", 180, |app| {
        app.advanced.as_ref().is_some_and(|state| {
            !state.resolving && state.dirty_at.is_none() && matches!(state.resolve, Some(Ok(_)))
        })
    });

    // Forcing absent on this box → the launch plan carries the config's
    // own [fetch]; the CDS precondition is a plan-time notice.
    let plan = walk.app.draft.to_plan("X").unwrap();
    match walk.app.draft.forcing_plan() {
        Some(crate::draft::ForcingPlan::Promote { source, .. }) => {
            assert_eq!(source.as_deref(), Some("era5"));
            let fetch = plan.fetch.expect("fetch promoted into the plan");
            assert!(fetch.args.join(" ").contains("--source era5"));
        }
        Some(crate::draft::ForcingPlan::OnDisk) => {
            assert!(plan.fetch.is_none(), "on-disk forcing skips the fetch");
        }
        other => panic!("unexpected forcing plan: {other:?}"),
    }
}

/// LIVE hrrr generation: either the surface lands source-coherent, or
/// the engine's refusal is SURFACED (status text) — never silence.
#[test]
#[ignore]
fn matrix_l03_live_hrrr_generation() {
    let mut walk = Walk::live("l03");
    walk.app.draft.source_index = 1; // HRRR
    walk.app.draft.domain = Some(arwen_map::LambertDomain::centered_at(
        35.5, -97.5, 200, 150, 3_000.0,
    ));
    let ctx = walk.ctx.clone();
    walk.app.handle_placement_edits(
        &ctx,
        vec![crate::map_pane::PlacementEdit::RootDrawn { nx: 200, ny: 150 }],
    );
    walk.pump_until("hrrr generation verdict", 180, |app| {
        app.advanced.is_some()
            || app
                .status_text()
                .is_some_and(|status| status.contains("refused"))
    });
    if walk.app.advanced.is_some() {
        walk.assert_source_invariant();
        eprintln!("matrix l03: HRRR surface generated + source-coherent");
    } else {
        eprintln!(
            "matrix l03: HRRR generation refused, surfaced verbatim: {:?}",
            walk.app.status_text()
        );
    }
}

/// LIVE out-of-retention pinned cycle [historical: refusal surfacing]:
/// the engine's refusal arrives as a surfaced error state, never a
/// silent nothing.
#[test]
#[ignore]
fn matrix_l04_live_out_of_retention_cycle() {
    let mut walk = Walk::live("l04");
    walk.app.draft.domain = Some(arwen_map::LambertDomain::centered_at(
        35.5, -97.5, 200, 150, 3_000.0,
    ));
    walk.app.draft.cycle = pinned((2020, 1, 1, 0));
    walk.pump_until("engine verdict on the 2020 cycle", 180, |app| {
        matches!(app.estimate, Some(Err(_))) || matches!(app.estimate, Some(Ok(_)))
    });
    match &walk.app.estimate {
        Some(Err(error)) => {
            assert!(
                !error.trim().is_empty(),
                "refusal carries the engine's words"
            );
            eprintln!(
                "matrix l04: engine refusal surfaced: {}",
                error.lines().next().unwrap_or("")
            );
        }
        Some(Ok(_)) => {
            eprintln!(
                "matrix l04: engine accepted the 2020 cycle at estimate time (refusal would land at fetch)"
            );
        }
        None => unreachable!(),
    }
}

/// THE MOST IMPORTANT CELL IN THE MATRIX — the first-run path, walked on every
/// release: a FRESH session, cards untouched
/// (GFS · latest · single · 3 km), draw a rectangle, Run — and the
/// LIVE engine takes it to COMPLETE (fetch → prepare → forecast →
/// finalize). If this cell is red, nothing ships.
#[test]
#[ignore]
fn matrix_l07_live_fresh_default_runs_to_complete() {
    let mut walk = Walk::live("l07");
    assert!(walk.app.route_block().is_none(), "the default is runnable");
    // The one gesture a first run needs: the rectangle.
    walk.app.draft.domain = Some(arwen_map::LambertDomain::centered_at(
        35.35, -97.4, 200, 100, 3_000.0,
    ));
    walk.app.draft.length_hours = 1.0; // shortest preset-shaped window
    let ctx = walk.ctx.clone();
    walk.app.handle_placement_edits(
        &ctx,
        vec![crate::map_pane::PlacementEdit::RootDrawn { nx: 200, ny: 100 }],
    );
    walk.pump_until("surface + resolve + estimate", 180, |app| {
        app.advanced
            .as_ref()
            .is_some_and(|state| matches!(state.resolve, Some(Ok(_))))
            && matches!(app.estimate, Some(Ok(_)))
            && !app.estimate_is_stale()
    });
    let actions = crate::inspector::InspectorActions {
        open_review: true,
        ..Default::default()
    };
    walk.app.handle_actions(&ctx, actions);
    walk.pump_until("review", 60, |app| app.review.is_some());
    let actions = crate::inspector::InspectorActions {
        launch: true,
        ..Default::default()
    };
    walk.app.handle_actions(&ctx, actions);
    walk.pump_until("session", 30, |app| app.session.is_some());
    walk.pump_until("the default RUN COMPLETES on the live engine", 420, |app| {
        app.session.as_ref().is_some_and(|session| {
            session.is_finished() && session.stages.iter().any(|stage| stage.id == "forecast")
        })
    });
    let session = walk.app.session.as_ref().unwrap();
    assert_eq!(
        session.terminal,
        Some(crate::run_session::Terminal::Completed),
        "terminal state is COMPLETED"
    );
    eprintln!(
        "matrix l07: fresh default ran to complete — {} frames",
        session.outputs.len()
    );
}

/// LIVE the regression's -74 flow on the real wizard: draw at 3 km, pick 12-3 —
/// the surface regenerates at 12 km with the chain, the footprint-true
/// root lands manual, the carry makes the child legal, and the ENGINE
/// re-ratifies the result.
#[test]
#[ignore]
fn matrix_l06_live_dx_ladder_change_carries_children() {
    let mut walk = Walk::live("l06");
    walk.app.draft.domain = Some(arwen_map::LambertDomain::centered_at(
        35.5, -97.5, 220, 126, 3_000.0,
    ));
    let ctx = walk.ctx.clone();
    walk.app.handle_placement_edits(
        &ctx,
        vec![crate::map_pane::PlacementEdit::RootDrawn { nx: 220, ny: 126 }],
    );
    walk.pump_until("live 3 km surface", 120, |app| app.advanced.is_some());

    walk.app.draft.set_root_dx_km(12.0);
    walk.app.draft.nests = vec![4];
    walk.frame();
    walk.pump_until("live 12-3 regeneration", 180, |app| {
        app.draft
            .custom
            .as_ref()
            .is_some_and(|custom| custom.nests == vec![4])
            && app
                .advanced
                .as_ref()
                .is_some_and(|state| state.model.domain_index_for_grid(2).is_some())
    });
    walk.pump_until("engine ratifies the carried tree", 180, |app| {
        app.advanced.as_ref().is_some_and(|state| {
            state.dirty_at.is_none() && !state.resolving && matches!(state.resolve, Some(Ok(_)))
        })
    });
    assert_eq!(walk.root_value("dx").as_deref(), Some("12000.0"));
    eprintln!(
        "matrix l06: root {}x{} @12km, d02 {}x{} at ({}, {}) — engine ratified",
        walk.root_value("nx").unwrap_or_default(),
        walk.root_value("ny").unwrap_or_default(),
        walk.domain_value(2, "nx").unwrap_or_default(),
        walk.domain_value(2, "ny").unwrap_or_default(),
        walk.domain_value(2, "i_parent_start").unwrap_or_default(),
        walk.domain_value(2, "j_parent_start").unwrap_or_default(),
    );
}

/// LIVE gfs 12-3 chain: records the released engine's answer to manual
/// trees on the prepared route (the engine question the coordinator
/// named) — surfaced either way, asserted non-silent.
#[test]
#[ignore]
fn matrix_l05_live_gfs_chain_verdict() {
    let mut walk = Walk::live("l05");
    walk.app.draft.root_dx_km = 12.0;
    walk.app.draft.nests = vec![4];
    walk.app.draft.domain = Some(arwen_map::LambertDomain::centered_at(
        35.2, -97.4, 204, 162, 12_000.0,
    ));
    let ctx = walk.ctx.clone();
    walk.app.handle_placement_edits(
        &ctx,
        vec![crate::map_pane::PlacementEdit::RootDrawn { nx: 204, ny: 162 }],
    );
    walk.pump_until("gfs chain generation verdict", 180, |app| {
        app.advanced.is_some()
            || app
                .status_text()
                .is_some_and(|status| status.contains("refused"))
    });
    match walk.app.advanced.as_ref() {
        Some(state) => {
            let domains = state
                .model
                .entries
                .iter()
                .filter(|entry| entry.key == "grid_id")
                .count();
            eprintln!("matrix l05: gfs 12-3 emitted {domains} domain(s)");
            walk.assert_source_invariant();
        }
        None => eprintln!(
            "matrix l05: gfs chain generation refused, surfaced: {:?}",
            walk.app.status_text()
        ),
    }
}

/// ORIGINAL DEFECT SHAPE END TO END — GFS + the 12-3 ladder. Fresh state,
/// draw, pick the ladder, Review, Launch, and
/// the LIVE engine takes BOTH DOMAINS to COMPLETE.
///
/// This cell was written the night before as the opposite: it PINNED the
/// engine's refusal (gpuwm 1.8.3 resolved the tree and then aborted at
/// prepare with rw-wps's `mismatch_landmask_ivgtyp` on any child holding
/// inland water - engine defect #118) and was built to go RED the day a
/// tree prepared, because that red was the signal to flip the route
/// table. gpuwm 1.8.4 fixed #118; the 12-3 at 39.0,-103.0 that refused
/// four ways now prepares in ~16 s and forecasts both grids. The cell is
/// the completing walk now, and the row it guarded is open.
#[test]
#[ignore]
fn matrix_l08_live_nested_gfs_end_to_end() {
    let mut walk = Walk::live("l08");
    // the regression's shape: the 12-3 ladder on GFS, a rectangle drawn on the
    // map, budgeted to the 16 GB class so the tree is a fast one.
    walk.app.draft.root_dx_km = 12.0;
    walk.app.draft.nests = vec![4];
    walk.app.draft.length_hours = 1.0;
    walk.app.draft.vram_gib = Some(12.0);
    walk.app.draft.domain = Some(arwen_map::LambertDomain::centered_at(
        39.0, -103.0, 126, 100, 12_000.0,
    ));
    let ctx = walk.ctx.clone();
    walk.app.handle_placement_edits(
        &ctx,
        vec![crate::map_pane::PlacementEdit::RootDrawn { nx: 126, ny: 100 }],
    );
    walk.pump_until("live nested-gfs surface", 240, |app| app.advanced.is_some());
    walk.assert_source_invariant();
    walk.pump_until("engine ratifies the gfs tree", 240, |app| {
        app.advanced.as_ref().is_some_and(|state| {
            state.dirty_at.is_none()
                && !state.resolving
                && matches!(state.resolve, Some(Ok(_)))
                && state.model.domain_index_for_grid(2).is_some()
        })
    });
    assert_eq!(
        walk.app.active_config_domains(),
        Some(2),
        "a real 2-domain tree"
    );
    assert!(
        walk.app.route_block().is_none(),
        "1.8.6 runs GFS trees; the row must be open: {:?}",
        walk.app.route_block()
    );
    walk.pump_until("estimate priced, not stale", 240, |app| {
        matches!(app.estimate, Some(Ok(_))) && !app.estimate_is_stale()
    });

    let actions = crate::inspector::InspectorActions {
        open_review: true,
        ..Default::default()
    };
    walk.app.handle_actions(&ctx, actions);
    walk.pump_until("review", 120, |app| app.review.is_some());
    let actions = crate::inspector::InspectorActions {
        launch: true,
        ..Default::default()
    };
    walk.app.handle_actions(&ctx, actions);
    walk.pump_until("session", 60, |app| app.session.is_some());
    walk.pump_until(
        "the GFS TREE runs to COMPLETE on the live engine",
        1_800,
        |app| {
            app.session
                .as_ref()
                .is_some_and(|session| session.is_finished())
        },
    );
    let session = walk.app.session.as_ref().unwrap();
    assert_eq!(
        session.terminal,
        Some(crate::run_session::Terminal::Completed),
        "nested GFS must complete on 1.8.6 (engine defect #118 fixed at 1.8.4): stages {:?}\n{}",
        stage_summary(session),
        engine_stderr_tail(session)
    );
    let (root, nest) = frames_by_domain(session);
    eprintln!(
        "matrix l08: nested GFS ran to complete - {} - frames d01 {root}, nests {nest}",
        stage_summary(session).join(" | "),
    );
    // BOTH DOMAINS produced output: a tree that only wrote its root is
    // a single-domain run wearing a ladder.
    assert!(root > 0, "the root committed frames");
    assert!(
        nest > 0,
        "the NEST committed frames - a tree run must write both grids \
         (engine receipt for this shape: domain_count 2, frame_count 10)"
    );
}

/// LIVE NESTED HRRR END TO END - the last row gpuwm 1.8.4 opened. 1.8.3
/// refused this at the front door ("the prepared route runs HRRR
/// single-domain only"); 1.8.4 drives it from run-plan as a THREE-stage
/// chain (root preparation -> `gpuwm.hrrr_hierarchy_direct` -> tree
/// forecast -> render) up to `_MAX_PUBLIC_DOMAINS` = 21.
///
/// The most expensive cell in the matrix by a distance: the HRRR fetch
/// is whole-file, ~4.4 GB, so budget several minutes before the first
/// stage even starts. It earns that twice over. The hierarchy stage
/// reads ALL FOUR of the route's side files (`--root-domain-spec`,
/// `--wps-namelist`, `--namelist-input`, `--stock-wrf-namelist-input`),
/// which is exactly what Studio's editable surface used to drop on the
/// floor - so a green hierarchy here is the standing proof that
/// `sync_config_side_files` keeps the whole set true to the geometry on
/// screen, not just the WPS half.
///
/// PLACEMENT IS THE POINT OF THIS CELL, and the centre below is the
/// REGRESSION SENTINEL for gpuwm 1.8.5's surface-moisture floor. Eastern
/// Colorado (39.0,-103.0 - where l08 runs nested GFS happily) is the
/// placement that 1.8.4 passed through both preparation stages and then
/// refused at the tree forecast's own input check: "prepared near-surface
/// surface_qv is outside the physical range 0.0..0.2". HRRR's GRIB2
/// packing quantises 2 m specific humidity to 1e-5, so over the San Juan
/// Mountains it decodes to exactly zero beside neighbours three orders of
/// magnitude larger; METGRID's overshooting sixteen_pt operator then
/// undershoots that stencil negative, and WRF's `qv_gc = sh_gc/(1-sh_gc)`
/// carries the sign straight into Q2. 1.8.5 floors the PUBLISHED surface
/// mixing ratio at WRF's own `qv_min_value` - the same constant the GFS
/// lane's RH path already used, which is why nested GFS never showed it.
///
/// So this cell is deliberately parked on the ground that used to break.
/// It sat on 35.35,-97.4 for exactly one release (1.8.4), because flat
/// Oklahoma cannot see this defect - single-domain HRRR carried the
/// identical latent bug and went unexposed for the same reason. If this
/// cell ever refuses here again, do NOT move the placement to make it
/// pass: that is the engine's near-surface floor regressing.
///
/// SAY WHAT THIS PROVES AND WHAT IT DOES NOT. The cell runs the LATEST
/// cycle, and whether the floor actually has anything to clamp is a
/// property of that cycle's air, not of the placement. First green run
/// here (cycle 2026-08-08 14Z) read prepared surface_qv min 1.67e-03
/// (d01) / 1.98e-03 (d02) with ZERO cells at or below the 1e-6 floor -
/// so the floor was a no-op and this pass is proof that the PLACEMENT
/// integrates, not that the clamp fires. The clamp arms itself only on
/// a cycle dry enough to quantise to zero aloft (the 09Z cycle that
/// produced the original refusal had two d02 cells at -1.8e-05). That
/// makes this a probabilistic sentinel: it will catch the regression on
/// any cycle that recreates the condition, and it is honest about being
/// silent on the ones that do not. The deterministic version of this
/// test is the engine's own unit pin on the two real 4x4 source
/// stencils; this cell is the end-to-end companion to it, not a
/// replacement.
#[test]
#[ignore]
fn matrix_l10_live_nested_hrrr_end_to_end() {
    let mut walk = Walk::live("l10");
    walk.app.draft.source_index = 1; // HRRR
    walk.app.draft.root_dx_km = 12.0;
    walk.app.draft.nests = vec![4];
    walk.app.draft.length_hours = 1.0;
    walk.app.draft.vram_gib = Some(16.0);
    // Eastern Colorado: the 1.8.5 surface-moisture-floor sentinel. See
    // the doc comment - this centre is load-bearing, not incidental.
    walk.app.draft.domain = Some(arwen_map::LambertDomain::centered_at(
        39.0, -103.0, 136, 108, 12_000.0,
    ));
    let ctx = walk.ctx.clone();
    walk.app.handle_placement_edits(
        &ctx,
        vec![crate::map_pane::PlacementEdit::RootDrawn { nx: 136, ny: 108 }],
    );
    walk.pump_until("live nested-hrrr surface", 300, |app| {
        app.advanced.is_some()
    });
    walk.assert_source_invariant();
    walk.pump_until("engine ratifies the hrrr tree", 300, |app| {
        app.advanced.as_ref().is_some_and(|state| {
            state.dirty_at.is_none()
                && !state.resolving
                && matches!(state.resolve, Some(Ok(_)))
                && state.model.domain_index_for_grid(2).is_some()
        })
    });
    assert_eq!(walk.app.active_config_domains(), Some(2));
    assert!(
        walk.app.route_block().is_none(),
        "1.8.6 drives HRRR trees; the row must be open: {:?}",
        walk.app.route_block()
    );

    // The whole side-file set travelled to the edited copy - the
    // hierarchy stage reads every one of them.
    let config_path = std::path::PathBuf::from(
        &walk
            .app
            .draft
            .custom
            .as_ref()
            .expect("custom plan")
            .config_path,
    );
    for suffix in [
        "namelist.wps",
        "namelist.input",
        "stock.namelist.input",
        "d01-target.json",
    ] {
        let side = config_path.with_file_name(format!(
            "{}.{suffix}",
            config_path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
        ));
        assert!(
            side.is_file(),
            "the HRRR route reads {suffix} beside the config"
        );
    }

    let actions = crate::inspector::InspectorActions {
        open_review: true,
        ..Default::default()
    };
    walk.app.handle_actions(&ctx, actions);
    walk.pump_until("review", 120, |app| app.review.is_some());
    let actions = crate::inspector::InspectorActions {
        launch: true,
        ..Default::default()
    };
    walk.app.handle_actions(&ctx, actions);
    walk.pump_until("session", 60, |app| app.session.is_some());
    // ~4.4 GB of whole-file HRRR plus three stages and a two-grid
    // render: 45 minutes of room, which is slack, not an expectation.
    walk.pump_until(
        "the HRRR TREE runs to COMPLETE on the live engine",
        2_700,
        |app| {
            app.session
                .as_ref()
                .is_some_and(|session| session.is_finished())
        },
    );
    let session = walk.app.session.as_ref().unwrap();
    let stages = stage_summary(session);
    // The preparation stage is what reads Studio's side files; name it
    // so a regression there cannot hide inside a pass.
    assert!(
        session
            .stages
            .iter()
            .any(|seen| seen.id == "prepare" && seen.status == crate::run_session::StageStatus::Ok),
        "the prepare stage must pass - it reads Studio's side files: \
         {stages:?}\n{}",
        engine_stderr_tail(session)
    );
    // The HIERARCHY stage is the one 1.8.4 added, and it is the one that
    // reads the other three side files. It is a stage of the CHAIN, not
    // of the run-plan event vocabulary (which is fetch/prepare/forecast/
    // finalize), so it is asserted where the chain reports it: its own
    // stderr, captured beside the plan. Getting this wrong once cost a
    // 24-minute matrix run on a walk that had actually completed.
    let engine_log = std::fs::read_to_string(session.dir.join("stderr.log"))
        .expect("the launcher captured the chain's stderr");
    assert!(
        engine_log.lines().any(|line| {
            let line = line.trim_start();
            line.starts_with("ok") && line.contains("hierarchy")
        }),
        "the hierarchy stage must pass - it reads the target JSON and both \
         namelists Studio wrote: {stages:?}\n{}",
        engine_stderr_tail(session)
    );
    assert_eq!(
        session.terminal,
        Some(crate::run_session::Terminal::Completed),
        "nested HRRR must complete on 1.8.6 over EASTERN COLORADO - the \
         placement that 1.8.4 refused with \"prepared near-surface \
         surface_qv is outside the physical range 0.0..0.2\". A refusal \
         here is a REGRESSION of the 1.8.5 surface-moisture floor, not a \
         reason to move the cell: stages {stages:?}\n{}",
        engine_stderr_tail(session)
    );
    let (root, nest) = frames_by_domain(session);
    eprintln!(
        "matrix l10: nested HRRR ran to complete - {} - frames d01 {root}, nests {nest}",
        stages.join(" | "),
    );
    assert!(root > 0, "the root committed frames");
    assert!(nest > 0, "the NEST committed frames");
}

/// LIVE HRRR SINGLE DOMAIN, STOCK, END TO END — the user-facing receipt
/// that gpuwm 1.8.3's Windows identity-separator fix landed. 1.8.2 wrote
/// the prepared-cache identity's source digests with backslashed keys and
/// read them back by forward-slash constants, so every native HRRR run on
/// this box died at the forecast handoff with "decode identity omits
/// [...]" on a cache that was in fact complete. Nothing is patched here:
/// fresh state, HRRR, 3 km, draw, Run, COMPLETE.
#[test]
#[ignore]
fn matrix_l09_live_hrrr_single_domain_runs_to_complete() {
    let mut walk = Walk::live("l09");
    walk.app.draft.source_index = 1; // HRRR
    assert_eq!(walk.app.draft.source().source, "hrrr");
    assert!(walk.app.route_block().is_none(), "HRRR single is runnable");
    walk.app.draft.length_hours = 1.0;
    walk.app.draft.domain = Some(arwen_map::LambertDomain::centered_at(
        35.35, -97.4, 200, 100, 3_000.0,
    ));
    let ctx = walk.ctx.clone();
    walk.app.handle_placement_edits(
        &ctx,
        vec![crate::map_pane::PlacementEdit::RootDrawn { nx: 200, ny: 100 }],
    );
    walk.pump_until("hrrr surface + resolve + estimate", 300, |app| {
        app.advanced
            .as_ref()
            .is_some_and(|state| matches!(state.resolve, Some(Ok(_))))
            && matches!(app.estimate, Some(Ok(_)))
            && !app.estimate_is_stale()
    });
    walk.assert_source_invariant();
    assert_eq!(walk.root_value("nx").as_deref(), Some("200"));

    let actions = crate::inspector::InspectorActions {
        open_review: true,
        ..Default::default()
    };
    walk.app.handle_actions(&ctx, actions);
    walk.pump_until("review", 120, |app| app.review.is_some());
    let actions = crate::inspector::InspectorActions {
        launch: true,
        ..Default::default()
    };
    walk.app.handle_actions(&ctx, actions);
    walk.pump_until("session", 60, |app| app.session.is_some());
    walk.pump_until(
        "the STOCK hrrr run COMPLETES on the live engine",
        1_800,
        |app| {
            app.session
                .as_ref()
                .is_some_and(|session| session.is_finished())
        },
    );
    let session = walk.app.session.as_ref().unwrap();
    let stages = stage_summary(session);
    eprintln!(
        "matrix l09: hrrr single-domain stages — {} · {} frame(s)",
        stages.join(" · "),
        session.outputs.len()
    );
    assert_eq!(
        session.terminal,
        Some(crate::run_session::Terminal::Completed),
        "HRRR single-domain must complete STOCK (1.8.2 died at the \
         forecast handoff with \"decode identity omits\"): stages {stages:?}\n{}",
        engine_stderr_tail(session)
    );
    assert!(
        !session.outputs.is_empty(),
        "a completed run committed frames"
    );
}

/// LIVE PREPARED-GFS MOVING NEST, END TO END — the cell the capability
/// sentinel demanded, written the moment the engine could satisfy it.
///
/// WHAT THIS REPLACED. Until gpuwm 1.8.6 this slot held
/// `matrix_l11_live_moving_nest_capability_is_still_absent`: a sentinel
/// that pinned "the released engine reports no moving-nest decision",
/// kept the prepared-route row shut, and was built to PANIC the day
/// `--resolve` answered with one. On 1.8.6 it did exactly that, naming
/// the four assertions below. They are written here unweakened.
///
/// The row it guarded cleared ITSELF: nothing in Studio was edited to
/// open it. `storm::MovingNest::read` probes the resolve reply Studio
/// already holds, so the engine's first reported decision is the whole
/// unblock — which is why the first assertion in this cell is that the
/// block is gone on the LIVE engine, not that some flag was flipped.
///
/// GFS is the chain under test because its fetch is the fast one.
/// HRRR is not untested: 1.8.6 seals a corridor on `prepared:hrrr` too
/// (fixture `resolve-moving-nest-hrrr.json` is that engine's own reply,
/// and cell f13 walks its corridor pricing), and the corridor lane's
/// live 18Z HRRR follow run EXECUTED BOTH ITS MOVES — the deterministic
/// move proof lives there, on the engine side, where a pinned case can
/// guarantee a storm.
///
/// WHAT THIS CELL DOES NOT PROVE, said before the receipts: it runs the
/// LATEST cycle, so whether the tracker finds a storm worth chasing is
/// a property of that cycle's air. It asserts the follow machinery is
/// ARMED and EVALUATING at its cadence, and it PRINTS the move count.
/// It does NOT assert `moves > 0`. That is l10's lesson applied on
/// purpose: a green cell whose greenness depends on the weather is a
/// cell that will lie to you on a quiet morning.
#[test]
#[ignore]
fn matrix_l11_live_prepared_gfs_follow_runs_to_complete() {
    let mut walk = Walk::live("l11");
    // l08's proven shape and ground, plus a moving nest: 12-3 over
    // eastern Colorado, 1 h, budgeted to the 16 GB class.
    walk.app.draft.root_dx_km = 12.0;
    walk.app.draft.nests = vec![4];
    walk.app.draft.length_hours = 1.0;
    walk.app.draft.vram_gib = Some(12.0);
    walk.app.draft.domain = Some(arwen_map::LambertDomain::centered_at(
        39.0, -103.0, 126, 100, 12_000.0,
    ));
    let ctx = walk.ctx.clone();
    walk.app.handle_placement_edits(
        &ctx,
        vec![crate::map_pane::PlacementEdit::RootDrawn { nx: 126, ny: 100 }],
    );
    walk.pump_until("live gfs tree surface", 240, |app| {
        app.advanced
            .as_ref()
            .is_some_and(|state| state.model.domain_index_for_grid(2).is_some())
    });
    walk.assert_source_invariant();

    // THE TOGGLE, through the real card action: a tracker follow on the
    // GFS prepared surface — the combination that was blocked up front
    // one engine release ago.
    let actions = crate::inspector::InspectorActions {
        storm: crate::storm::StormActions {
            apply_follow: Some(Some(crate::storm::FollowSettings::default())),
            ..Default::default()
        },
        ..Default::default()
    };
    walk.app.handle_actions(&ctx, actions);
    walk.pump_until("the engine ratifies the follow config", 300, |app| {
        app.advanced.as_ref().is_some_and(|state| {
            state.dirty_at.is_none() && !state.resolving && matches!(state.resolve, Some(Ok(_)))
        })
    });
    assert_eq!(
        crate::storm::declares_follow_source(&walk.surface().model),
        Some("tracker"),
        "the config really does declare a follow source"
    );

    // (0) THE SELF-CLEAR, on the live engine. The decision is reported,
    // so the prepared-route row is open — with no Studio edit.
    let decision = match walk.app.moving_nest() {
        crate::storm::MovingNest::Reported(decision) => decision,
        other => panic!(
            "gpuwm 1.8.6 must report a moving-nest decision for a prepared \
             GFS follow config; got {other:?}"
        ),
    };
    assert_eq!(decision.chain, "prepared:go");
    assert_eq!(decision.delivery.as_deref(), Some("statics_corridor"));
    assert!(decision.statics_corridor);
    assert!(
        walk.app.route_block().is_none(),
        "the row cleared itself on the engine's answer: {:?}",
        walk.app.route_block()
    );

    // The corridor price Studio SHOWED the user before launch — kept so
    // the artifact on disk can be checked against it afterwards.
    walk.pump_until("estimate priced, not stale", 300, |app| {
        matches!(app.estimate, Some(Ok(_))) && !app.estimate_is_stale()
    });
    let quoted = walk
        .app
        .estimate
        .as_ref()
        .and_then(|result| result.as_ref().ok())
        .and_then(|estimate| estimate.corridor.as_ref())
        .and_then(|corridor| corridor.host_bytes)
        .expect("the live estimate prices the corridor");
    assert!(quoted > 0, "a moving nest's corridor is not free");
    let quoted_line = walk
        .app
        .estimate
        .as_ref()
        .and_then(|result| result.as_ref().ok())
        .and_then(crate::inspector::corridor_line)
        .map(|(line, _)| line)
        .expect("the Resources strip shows it");
    eprintln!("matrix l11: Studio quoted before launch — {quoted_line}");

    // Launch the real thing.
    let actions = crate::inspector::InspectorActions {
        open_review: true,
        ..Default::default()
    };
    walk.app.handle_actions(&ctx, actions);
    walk.pump_until("review", 120, |app| app.review.is_some());
    let actions = crate::inspector::InspectorActions {
        launch: true,
        ..Default::default()
    };
    walk.app.handle_actions(&ctx, actions);
    walk.pump_until("session", 60, |app| app.session.is_some());
    walk.pump_until(
        "the MOVING-NEST run COMPLETES on the live engine",
        2_400,
        |app| {
            app.session
                .as_ref()
                .is_some_and(|session| session.is_finished())
        },
    );
    let session = walk.app.session.as_ref().unwrap();
    let stages = stage_summary(session);

    // (1) COMPLETED, with frames from BOTH grids. Not "resolved clean":
    // resolving clean was never the problem this row was about.
    assert_eq!(
        session.terminal,
        Some(crate::run_session::Terminal::Completed),
        "a prepared GFS FOLLOW run must complete on 1.8.6: stages \
         {stages:?}\n{}",
        engine_stderr_tail(session)
    );
    let (root, nest) = frames_by_domain(session);
    assert!(root > 0, "the root committed frames");
    assert!(
        nest > 0,
        "the MOVING NEST committed frames — a follow run that only writes \
         its root is a static run wearing a [relocation] block"
    );

    // (2) THE PREPARATION ACTUALLY SEALED A CORRIDOR, and it is the one
    // Studio quoted. The engine holds estimate == sealed on its own
    // side; this is the cross-check from the front door, on the bytes
    // that landed in the run's OUTPUT ROOT.
    //
    // `session.run_dir`, not `session.dir`. The first cut of this cell
    // searched `dir` — Studio's own REGISTRY folder, which holds the
    // plan and the captured stderr and nothing the engine wrote — and
    // reported "the preparation did not seal one" about a run that had
    // sealed one perfectly well. The instrument was wrong, not the
    // engine; the two paths are named a line apart in RunSession and
    // this comment is here so the next reader does not repeat it.
    let output_root = session
        .run_dir
        .clone()
        .unwrap_or_else(|| session.dir.clone());
    let receipt =
        find_file(&output_root, "receipt.json", "statics-corridor").unwrap_or_else(|| {
            panic!(
                "no statics-corridor receipt under the run's output root {} \
                 — the preparation did not seal one, so the nest could not \
                 have moved even if the tracker asked: stages {stages:?}",
                output_root.display()
            )
        });
    let sealed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&receipt).unwrap())
            .expect("the corridor receipt parses");
    assert_eq!(
        sealed["status"].as_str(),
        Some("READY"),
        "sealed corridor status: {sealed}"
    );
    let sealed_bytes: u64 = sealed["domains"]
        .as_object()
        .expect("per-domain corridor entries")
        .values()
        .filter_map(|entry| entry["cache"]["bytes"].as_u64())
        .sum();
    eprintln!(
        "matrix l11: quoted {quoted} host bytes, sealed {sealed_bytes} on \
         disk ({})",
        receipt.display()
    );
    // NPZ container headers are the only legal difference (the engine's
    // own basis says so), so this is a tolerance, not an equality.
    let drift = (sealed_bytes as f64 - quoted as f64).abs() / quoted as f64;
    assert!(
        drift < 0.02,
        "the corridor Studio priced ({quoted}) and the corridor the \
         preparation wrote ({sealed_bytes}) must be the same artifact to \
         within container headers; drift {:.3}%",
        drift * 100.0
    );

    // (3) THE FOLLOW MACHINERY WAS ARMED AND EVALUATING. Every receipt
    // row is one cadence evaluation the runner actually performed —
    // `held` when the tracker declined, `relocated` when it moved.
    //
    // WHERE THEY LAND is route-dependent, and finding that out cost
    // this cell its first two live runs. The experiment route writes
    // the receipts at the run root; the PREPARED TREE route's forecast
    // stage has its own nested outdir under the chain. Studio's map
    // trail read only the root, so it would have shown an EMPTY
    // relocation trail for every prepared-route moving nest — silently,
    // because "no receipts file" is also what a still run looks like.
    // That was a product defect this cell found, and it is fixed in
    // `run_session::trail_receipts_path`; the cell resolves the path
    // through THE SAME function so the two can never disagree about
    // where the truth is.
    let receipts_path =
        find_file(&output_root, "relocation_receipts.json", "").unwrap_or_else(|| {
            panic!(
                "no relocation receipts anywhere under {} — the run \
                 completed without the relocation runner ever being \
                 constructed, which means the follow source was silently \
                 ignored",
                output_root.display()
            )
        });
    let receipts: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&receipts_path).unwrap())
            .expect("the receipts parse");
    assert!(
        crate::run_session::trail_receipts_path_for_test(&output_root)
            .is_some_and(|found| found == receipts_path),
        "Studio's own trail reader must find the same receipts file this \
         cell found at {} — an overlay that cannot locate the receipts \
         draws an empty trail over a run that moved",
        receipts_path.display()
    );
    let rows = receipts["receipts"]
        .as_array()
        .expect("a receipts list")
        .clone();
    let evaluations = rows
        .iter()
        .filter(|row| matches!(row["event"].as_str(), Some("held") | Some("relocated")))
        .count();
    assert!(
        evaluations > 0,
        "the tracker must be CONSULTED at the cadence boundaries — zero \
         evaluations means the nest was static and nothing said so: {receipts}"
    );

    // (4) MOVES ARE COUNTED AND PRINTED, NEVER ASSERTED NONZERO. This
    // cell runs the LATEST cycle; whether eastern Colorado holds a storm
    // this hour is not a property of Studio. l10's lesson, on purpose.
    let moves = rows
        .iter()
        .find(|row| row["event"].as_str() == Some("summary"))
        .and_then(|row| row["moves_executed"].as_u64())
        .unwrap_or_else(|| {
            rows.iter()
                .filter(|row| row["event"].as_str() == Some("relocated"))
                .count() as u64
        });
    let trail = session.trail.len();
    eprintln!(
        "matrix l11: prepared GFS MOVING NEST completed — {} — frames d01 \
         {root}, nests {nest} · {evaluations} cadence evaluation(s) · \
         moves_executed {moves} · trail rows {trail}",
        stages.join(" | ")
    );
    if moves == 0 {
        eprintln!(
            "matrix l11: zero moves this cycle. That is a WEATHER result, \
             not a failure: the tracker was consulted {evaluations} time(s) \
             and declined. The deterministic move proof is the engine's \
             own pinned-case suite, not this cell."
        );
    } else {
        assert_eq!(
            trail, moves as usize,
            "every executed move must reach the map's relocation trail"
        );
    }
}

/// First file named `name` under `root` whose parent directory is
/// `parent` (empty `parent` = match on the name alone) — the run's
/// artifact layout is the engine's to choose, so the cell searches for
/// the receipt instead of hardcoding a path that a staging rename would
/// silently invalidate. Hardcoding is exactly what broke the first two
/// runs of this cell.
fn find_file(root: &std::path::Path, name: &str, parent: &str) -> Option<std::path::PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().and_then(|n| n.to_str()) == Some(name)
                && (parent.is_empty()
                    || path
                        .parent()
                        .and_then(|p| p.file_name())
                        .and_then(|n| n.to_str())
                        == Some(parent))
            {
                return Some(path);
            }
        }
    }
    None
}

/// Each stage the run reported, with its wall time — the receipt a live
/// cell prints on success and quotes on failure.
fn stage_summary(session: &crate::run_session::RunSession) -> Vec<String> {
    session
        .stages
        .iter()
        .map(|stage| {
            format!(
                "{}={:?}{}",
                stage.id,
                stage.status,
                stage
                    .wall_seconds
                    .map(|seconds| format!(" ({seconds:.0}s)"))
                    .unwrap_or_default()
            )
        })
        .collect()
}

/// The chain's own last words, from the log the launcher captured beside
/// the plan. A red live cell must show the ENGINE's sentence, not just
/// "stage exited 1" — the event stream names the stage, the chain prints
/// the reason, and a failure report needs both.
fn engine_stderr_tail(session: &crate::run_session::RunSession) -> String {
    match std::fs::read_to_string(session.dir.join("stderr.log")) {
        Ok(text) => {
            let tail: Vec<&str> = text.lines().rev().take(40).collect();
            format!(
                "engine stderr (last {} lines):\n{}",
                tail.len(),
                tail.into_iter().rev().collect::<Vec<_>>().join("\n")
            )
        }
        Err(error) => format!("no captured engine stderr: {error}"),
    }
}

/// Frames the run committed, split root vs nests. A single-domain run
/// tags nothing, so untagged frames count as the root.
fn frames_by_domain(session: &crate::run_session::RunSession) -> (usize, usize) {
    let root = session
        .outputs
        .iter()
        .filter(|frame| frame.domain.unwrap_or(1) == 1)
        .count();
    (root, session.outputs.len() - root)
}
