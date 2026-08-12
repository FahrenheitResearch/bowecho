// SPDX-License-Identifier: Apache-2.0

//! The Studio application: one persistent map-centered workspace whose
//! design → run → analyze states share a canvas and camera. Chrome lives
//! in egui panels OUTSIDE the egui_tiles tree; the tree owns layout only
//! (BowEcho dock.rs rule). No blocked frames: every query and launch runs
//! through worker slots; the event stream arrives via file tail.

use std::time::{Duration, Instant};

use eframe::egui;
use ui_core::tiles::{TileLayer, TileLayerConfig, TileStyle};
use ui_core::worker_slot::{SlotPoll, WorkerSlot};

use arwen_map::view::MapView;
use arwen_plan::queries::{EstimateReport, ProbeReport, ResolveReport};
use arwen_proc::ContractSource;
use arwen_proc::launcher::RunOwnership;
use arwen_proc::registry::{RunEntry, RunRegistry};

use crate::draft::Draft;
use crate::inspector::{self, InspectorActions, ReviewSheet};
use crate::map_pane::{MapFrame, MapPane};
use crate::model_layer::ModelLayer;
use crate::run_session::RunSession;
use crate::settings::{StudioSettings, tile_cache_dir};
use crate::theme::{self, theme};
use crate::timeline::{self, TimelineActions};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RailView {
    New,
    Runs,
    System,
}

/// The one pane kind v1 ships; cross-sections/soundings/member grids are
/// future pane kinds in this same tree (END-GAME §1).
pub enum PaneKind {
    Map,
}

pub struct StudioApp {
    /// True when BowEcho hosts the Studio surface inside its WRF workspace.
    embedded: bool,
    /// Host policy can permit planning/monitoring while withholding launch.
    /// The same reason reaches the review sheet and the final launch guard.
    host_launch_block: Option<String>,
    settings: StudioSettings,
    contract: Result<ContractSource, String>,
    registry: RunRegistry,
    pub(crate) view: MapView,
    tile_layer: TileLayer,
    tile_style: TileStyle,
    pub(crate) map_pane: MapPane,
    pub(crate) draft: Draft,
    pub(crate) session: Option<RunSession>,
    rail: RailView,
    pub(crate) runs_cache: Vec<RunEntry>,
    runs_cache_at: Option<Instant>,
    probe: Option<Result<ProbeReport, String>>,
    probe_slot: WorkerSlot<Result<ProbeReport, String>>,
    probe_at: Option<Instant>,
    pub(crate) estimate: Option<Result<EstimateReport, String>>,
    estimate_slot: WorkerSlot<Result<EstimateReport, String>>,
    estimate_fingerprint: u64,
    estimate_settle: Option<Instant>,
    /// The fingerprint the DISPLAYED estimate was priced for — the strip
    /// dims + spins whenever the current draft differs (continuous
    /// re-pricing contract; a number must never pose as current pricing
    /// for a draft it was not computed from).
    estimate_shown_fingerprint: u64,
    estimate_inflight_fingerprint: Option<u64>,
    /// Test-visible evidence: how many estimates were spawned, and the
    /// exact plan bytes of the last one.
    pub(crate) estimate_runs: u64,
    pub(crate) last_estimate_plan: Option<String>,
    pub(crate) review: Option<ReviewSheet>,
    review_slot: WorkerSlot<Result<(ResolveReport, String), String>>,
    /// The Advanced config surface (engine-generated TOML, field editor).
    pub(crate) advanced: Option<crate::advanced::AdvancedState>,
    advanced_generate_slot: WorkerSlot<Result<arwen_proc::GeneratedConfig, String>>,
    /// The in-flight generation speaks a SUPERSEDED root rectangle (a
    /// redraw landed while the engine was writing): its result is
    /// dropped on arrival and generation re-runs for the current draft,
    /// queued placements intact — a fast double-redraw never lands the
    /// old center.
    advanced_generate_stale: bool,
    /// The intent facets (source, root dx bits, ladder) the in-flight
    /// generation was spawned for; any of them moving mid-write makes
    /// the landing emission stale (surface and cards never disagree).
    advanced_generate_intent: Option<(String, u64, Vec<u32>)>,
    /// The draft fingerprint whose generation the engine REFUSED (e.g.
    /// ERA5 without a pinned cycle). Queued placements are kept; the
    /// retry fires when the draft changes — never a hammer loop on the
    /// same refusal.
    generation_refused_fingerprint: Option<u64>,
    advanced_resolve_slot: WorkerSlot<Result<ResolveReport, String>>,
    pub(crate) model: ModelLayer,
    status: Option<(String, bool)>,
    tree: egui_tiles::Tree<PaneKind>,
    /// Self-close deadline for `--smoke-seconds` verification runs.
    smoke_deadline: Option<Instant>,
    /// Smoke screenshot state: None = not yet requested.
    smoke_screenshot_requested: Option<Instant>,
    /// `--demo-draft`: seed a real draft (domain + pinned cycle + nest)
    /// for screenshot smokes, and nudge the domain shortly before the
    /// deadline so the strip's TRUE stale/re-pricing state is on screen
    /// when the capture fires. Real pipeline, no faked states.
    demo_draft: bool,
    demo_nudged: bool,
    demo_advanced_requested: bool,
    demo_storm_applied: bool,
    /// `--demo-offcentre`: seed an ERA5 12-3-1, let the engine fit the
    /// tree, then move BOTH inner domains far off-centre through the
    /// real placement writer — the engine re-ratifies and the map paints
    /// its reply (screenshot evidence for the placement wave).
    demo_offcentre: bool,
    demo_offcentre_requested: bool,
    demo_offcentre_applied: bool,
    /// `--demo-drawroot`: THE DRAWN RECTANGLE IS THE DOMAIN, end to end
    /// in the packaged exe — a deliberately wide 2:1 rectangle through
    /// the map pane's own drag→domain function, pushed through the SAME
    /// placement queue a mouse drag fills. The engine writes the config,
    /// the drawn size lands as the manual root, resolve + estimate
    /// answer for that shape (screenshot evidence for the redraw defect).
    demo_drawroot: bool,
    demo_drawroot_applied: bool,
    /// `--demo-run` (with `--demo-drawroot`): after the drawn-root
    /// surface resolves and prices, open the review and LAUNCH — the
    /// full fetch?prepare?forecast chain on the configured engine (the
    /// by-hand sign-off: watch a manual-geometry GFS run proceed).
    demo_drawroot_run: bool,
    demo_drawroot_launched: bool,
    /// `--demo-carry`: THE REGRESSION'S EXACT SEQUENCE in the packaged exe — draw
    /// big (ERA5 12-3), place d02 far EAST, shrink the root through the
    /// same path a handle drag lands, and watch d02 come along (the
    /// carry clamps it back inside the new clearance envelope; its row
    /// says so). Stages: 0 seed → 1 surface requested → 2 gestures
    /// applied → 3 reported.
    demo_carry: bool,
    demo_carry_stage: u8,
    /// When this process started (the newer-build check's baseline).
    exe_started: std::time::SystemTime,
    /// The newer build's folder stamp when one is on disk — the gentle
    /// top-bar nudge to relaunch the Desktop shortcut.
    newer_build: Option<String>,
    newer_build_checked_at: Option<Instant>,
    /// THE CONTINUOUS FIT: a shadow `--resolve` riding the same debounce
    /// as the estimate, so the engine-fitted tree (root + every child)
    /// lives on the map in the NORMAL design flow. the regression's defect: the
    /// tree only existed while the review sheet or Advanced editor was
    /// open — draw a parent, pick 12-3-1, and there was nothing to
    /// select. The fit is display+gesture state; the plan itself is
    /// untouched.
    pub(crate) fit: Option<LiveFit>,
    fit_slot: WorkerSlot<Result<ResolveReport, String>>,
    /// The fingerprint the last fit spawn (or clear) was for.
    fit_fingerprint: u64,
    /// Draft-derived interactive placeholders: painted (and selectable)
    /// the instant a ladder exists, until the first fit answers — a map
    /// where nothing responds is never shown. Sizes/anchors are a
    /// centered sketch, labelled provisional; the ENGINE's fit replaces
    /// them within the resolve round trip.
    placeholder_root: Option<arwen_map::LambertDomain>,
    placeholder_tree: Vec<arwen_plan::queries::ResolvedDomain>,
    /// Placement drops made while no config surface existed yet: the
    /// engine is writing the config (dry run); these apply the moment it
    /// lands — scaled between the drop-time parent grid and the written
    /// config's parent grid, so a drop on a placeholder or review tree
    /// keeps its RELATIVE position through the engine's refit.
    pub(crate) pending_placements: Vec<PendingPlacement>,
    /// The `[relocation]` tables of a surface that is being REGENERATED
    /// for a card move (a source switch, a dx change, a new ladder).
    ///
    /// Manual geometry has always been carried across a regeneration;
    /// the follow tables were not, so arming a moving nest on ERA5 and
    /// then switching to GFS silently produced a still config. That is
    /// exactly the "the tables flow into the prepared route's emission"
    /// half of the moving-nest toggle: the declaration belongs to the
    /// user's intent, not to whichever emission happens to be open.
    pub(crate) pending_follow: Option<crate::storm::FollowSettings>,
    /// The live-fire acceptance driver (`--livefire <mode>`).
    pub(crate) livefire: Option<crate::livefire::LivefireDriver>,
    /// The render-product picker over the engine's catalog.
    pub(crate) products: crate::products::ProductsState,
    catalog_slot: WorkerSlot<Result<arwen_plan::queries::CatalogReport, String>>,
}

/// The engine's fitted answer from the continuous shadow resolve.
pub(crate) struct LiveFit {
    fitted: arwen_map::LambertDomain,
    tree: Vec<arwen_plan::queries::ResolvedDomain>,
    floor: Option<serde_json::Value>,
}

/// A queued map drop plus the parent grid it was made against, for the
/// relative-position rescale when the engine's written config arrives.
pub(crate) struct PendingPlacement {
    edit: crate::map_pane::PlacementEdit,
    parent_span: Option<(u32, u32)>,
}

/// A whole-cell placement after the pending rescale: `(i, j, size)`.
type ScaledPlacement = (i64, i64, Option<(u32, u32)>);

/// The WPS namelist rewritten to the working config's OWN domain
/// geometry: per-domain `i/j_parent_start`, `e_we`/`e_sn` (staggered =
/// mass + 1 — the mapping the prepared route's mismatch dump publishes)
/// and the `[projection]` refs. Every other byte survives; keys the
/// namelist does not carry are never invented; the engine's own
/// cross-check remains the judge of the pair.
pub(crate) fn rewrite_wps_namelist(text: &str, model: &crate::advanced::ConfigModel) -> String {
    let value = |index: usize, key: &str| -> Option<f64> {
        model.domain_value(index, key)?.trim().parse::<f64>().ok()
    };
    let (mut i_list, mut j_list, mut e_we, mut e_sn) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for index in 0.. {
        let table = format!("domain[{index}]");
        if !model.entries.iter().any(|entry| entry.table == table) {
            break;
        }
        let (Some(i), Some(j), Some(nx), Some(ny)) = (
            value(index, "i_parent_start"),
            value(index, "j_parent_start"),
            value(index, "nx"),
            value(index, "ny"),
        ) else {
            continue;
        };
        i_list.push((i.round() as i64).to_string());
        j_list.push((j.round() as i64).to_string());
        e_we.push((nx as i64 + 1).to_string());
        e_sn.push((ny as i64 + 1).to_string());
    }
    let projection = |key: &str| -> Option<String> {
        model
            .entries
            .iter()
            .find(|entry| entry.table == "projection" && entry.key == key)
            .map(|entry| entry.value.trim().to_string())
    };
    let mut writes: Vec<(&str, String)> = vec![
        ("i_parent_start", i_list.join(", ")),
        ("j_parent_start", j_list.join(", ")),
        ("e_we", e_we.join(", ")),
        ("e_sn", e_sn.join(", ")),
    ];
    for key in ["ref_lat", "ref_lon", "stand_lon"] {
        if let Some(projection_value) = projection(key) {
            writes.push((key, projection_value));
        }
    }
    // The root's dx/dy follow a dx-card change (the regression's -74 flow).
    if let Some(root) = model.root_domain_index()
        && let Some(dx) = model
            .domain_value(root, "dx")
            .map(|value| value.trim().to_string())
    {
        writes.push(("dx", dx.clone()));
        writes.push(("dy", dx));
    }
    rewrite_namelist_keys(text, &writes)
}

/// One Fortran-namelist key rewrite: a line whose first token is the key
/// and whose next character is `=` becomes ` key = value,`. Every other
/// byte survives, and a key the file does not carry is never invented.
fn rewrite_namelist_keys(text: &str, writes: &[(&str, String)]) -> String {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        let replaced = writes.iter().find_map(|(key, list)| {
            let after = trimmed.strip_prefix(key)?;
            after
                .trim_start()
                .starts_with('=')
                .then(|| format!(" {key} = {list},"))
        });
        out.push(replaced.unwrap_or_else(|| line.to_string()));
    }
    let mut joined = out.join("\n");
    if text.ends_with('\n') {
        joined.push('\n');
    }
    joined
}

/// Per-domain values off the working config, root first.
fn domain_column(model: &crate::advanced::ConfigModel, key: &str) -> Vec<String> {
    let mut column = Vec::new();
    for index in 0.. {
        let table = format!("domain[{index}]");
        if !model.entries.iter().any(|entry| entry.table == table) {
            break;
        }
        match model.domain_value(index, key) {
            Some(value) => column.push(value.trim().to_string()),
            None => return Vec::new(),
        }
    }
    column
}

fn table_value(model: &crate::advanced::ConfigModel, table: &str, key: &str) -> Option<String> {
    model
        .entries
        .iter()
        .find(|entry| entry.table == table && entry.key == key)
        .map(|entry| entry.value.trim().to_string())
}

/// The WRF `namelist.input` rewritten to the working config's geometry.
/// THE HRRR ROUTE READS THESE FILES INSTEAD OF THE TOML (runplan.py: the
/// HRRR tools "read the four namelist/JSON files the wizard writes beside
/// the config rather than the TOML itself"), so a drawn root that moved
/// only the TOML would have run at the wizard's fitted size and said
/// nothing — matrix-found by cell l09.
pub(crate) fn rewrite_wrf_namelist_input(
    text: &str,
    model: &crate::advanced::ConfigModel,
) -> String {
    let mass_plus_one = |key: &str| -> Vec<String> {
        domain_column(model, key)
            .iter()
            .filter_map(|value| value.parse::<f64>().ok())
            .map(|value| (value as i64 + 1).to_string())
            .collect()
    };
    let mut writes: Vec<(&str, String)> = Vec::new();
    for (key, column) in [
        ("e_we", mass_plus_one("nx")),
        ("e_sn", mass_plus_one("ny")),
        ("i_parent_start", domain_column(model, "i_parent_start")),
        ("j_parent_start", domain_column(model, "j_parent_start")),
        ("dx", domain_column(model, "dx")),
    ] {
        if !column.is_empty() {
            if key == "dx" {
                writes.push(("dy", column.join(", ")));
            }
            writes.push((key, column.join(", ")));
        }
    }
    // The root clock: a dx change moves it, and the route runs on this
    // number rather than the TOML's.
    if let Some(root) = model.root_domain_index()
        && let Some(step) = model.domain_value(root, "time_step")
    {
        writes.push(("time_step", step.trim().to_string()));
    }
    rewrite_namelist_keys(text, &writes)
}

/// The HRRR route's `<stem>.d01-target.json` rewritten to the working
/// config's root geometry. Keys the document does not already carry are
/// never added — the engine's schema stays the engine's.
pub(crate) fn rewrite_target_domain_json(
    text: &str,
    model: &crate::advanced::ConfigModel,
) -> Option<String> {
    let mut document: serde_json::Value = serde_json::from_str(text).ok()?;
    let object = document.as_object_mut()?;
    let root = model.root_domain_index()?;
    let number = |raw: &str| raw.trim().parse::<f64>().ok();
    let mut set = |key: &str, value: Option<f64>| {
        if let (Some(value), Some(slot)) = (value, object.get_mut(key))
            && let Some(number) = serde_json::Number::from_f64(value)
        {
            *slot = if slot.as_i64().is_some() && value.fract() == 0.0 {
                serde_json::Value::from(value as i64)
            } else {
                serde_json::Value::Number(number)
            };
        }
    };
    let domain = |key: &str| model.domain_value(root, key).and_then(|raw| number(&raw));
    set("nx", domain("nx"));
    set("ny", domain("ny"));
    set("dx_m", domain("dx"));
    set("dy_m", domain("dx"));
    set("time_step_seconds", domain("time_step"));
    set(
        "nz",
        table_value(model, "shared", "nz").and_then(|raw| number(&raw)),
    );
    for key in ["ref_lat", "ref_lon", "stand_lon", "truelat1", "truelat2"] {
        set(
            key,
            table_value(model, "projection", key).and_then(|raw| number(&raw)),
        );
    }
    let mut out = serde_json::to_string_pretty(&document).ok()?;
    if text.ends_with('\n') {
        out.push('\n');
    }
    Some(out)
}

/// EVERY side file the engine's routes read BY THE CONFIG'S STEM, kept
/// true to the working config. The prepared route cross-checks the WPS
/// pair and refuses a mismatch; the HRRR route reads its namelist pair
/// and target JSON *instead of* the TOML and would otherwise run a
/// different domain than the one on screen. A file that is not there is
/// not written — the set is whatever the emission produced.
pub(crate) fn sync_config_side_files(
    config_path: &std::path::Path,
    model: &crate::advanced::ConfigModel,
) {
    let side = |suffix: &str| {
        config_path.with_file_name(format!(
            "{}.{suffix}",
            config_path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
        ))
    };
    let rewrite = |path: std::path::PathBuf, with: &dyn Fn(&str) -> Option<String>| {
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        if let Some(synced) = with(&text)
            && synced != text
        {
            let _ = std::fs::write(&path, synced);
        }
    };
    rewrite(side("namelist.wps"), &|text| {
        Some(rewrite_wps_namelist(text, model))
    });
    for suffix in ["namelist.input", "stock.namelist.input"] {
        rewrite(side(suffix), &|text| {
            Some(rewrite_wrf_namelist_input(text, model))
        });
    }
    rewrite(side("d01-target.json"), &|text| {
        rewrite_target_domain_json(text, model)
    });
}

/// Build identity, baked in at compile time (the stale-binary killer:
/// three old-binary confusions in one day).
pub(crate) const BUILD_SHA: &str = env!("ARWEN_BUILD_SHA");
pub(crate) const BUILD_DATE: &str = env!("ARWEN_BUILD_DATE");

pub(crate) fn build_stamp() -> String {
    format!("{BUILD_SHA} · {BUILD_DATE}")
}

/// Is a NEWER BUILD on disk than the running one? Versioned delivery
/// puts every ship in `dist\builds\<stamp>\` — a running exe under
/// `builds\` looks for a lexicographically-younger sibling folder
/// (stamps sort by date). Anywhere else (target\release, the legacy
/// dist folder) fall back to own-file mtime vs process start.
fn newer_build_on_disk(started: std::time::SystemTime) -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let my_dir = exe.parent()?;
    if let Some(builds) = my_dir
        .parent()
        .filter(|parent| parent.file_name().is_some_and(|name| name == "builds"))
    {
        let mine = my_dir.file_name()?.to_string_lossy().into_owned();
        let mut newest: Option<String> = None;
        for entry in std::fs::read_dir(builds).ok()?.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.path().is_dir()
                && name > mine
                && newest.as_ref().map(|seen| name > *seen).unwrap_or(true)
            {
                newest = Some(name);
            }
        }
        return newest;
    }
    let mtime = std::fs::metadata(&exe).ok()?.modified().ok()?;
    (mtime > started + Duration::from_secs(5)).then(|| "a newer exe".into())
}

/// Favorites feed the plan (Favorites render mode), so they join the
/// estimate fingerprint.
fn favorites_hash(favorites: &[String]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::hash::DefaultHasher::new();
    favorites.hash(&mut hasher);
    hasher.finish()
}

impl StudioApp {
    /// Production entry: settings from disk (live/fixture is config).
    pub fn new(ctx: &egui::Context) -> Self {
        Self::with_settings(ctx, StudioSettings::load_or_default())
    }

    /// BowEcho entry point. Engine work remains out-of-process, existing
    /// ArWen settings/runs are reused, and launch stays blocked until the
    /// supervised/cancellable public runner contract is available.
    pub fn embedded(ctx: &egui::Context) -> Self {
        Self::with_settings_inner(
            ctx,
            StudioSettings::load_for_bowecho(),
            false,
            Some(
                "Launching is not enabled in BowEcho yet: the current gpuwm run-plan contract \
                 does not expose a supervised, cancellable run boundary for every GPU route. \
                 Planning, engine resolve/estimate, and monitoring existing runs are available."
                    .into(),
            ),
        )
    }

    /// Explicit-settings entry — tests inject fixture settings so the
    /// suite can never silently depend on this box's settings.json.
    pub fn with_settings(ctx: &egui::Context, settings: StudioSettings) -> Self {
        Self::with_settings_inner(ctx, settings, true, None)
    }

    fn with_settings_inner(
        ctx: &egui::Context,
        settings: StudioSettings,
        configure_global_style: bool,
        host_launch_block: Option<String>,
    ) -> Self {
        if configure_global_style {
            theme::configure_style(ctx);
        }
        let contract = settings.contract_source();
        let registry = RunRegistry::new(RunRegistry::default_root());
        let tile_layer = TileLayer::new(TileLayerConfig {
            cache_dir: Some(tile_cache_dir()),
            max_textures: 220,
            max_workers: 4,
            debug_env: "ARWEN_TILE_DEBUG",
        });
        let tile_style = TileStyle::from_key(&settings.tile_style);
        let tree = egui_tiles::Tree::new_tabs("studio-panes", vec![PaneKind::Map]);

        let mut app = Self {
            embedded: !configure_global_style,
            host_launch_block,
            settings,
            contract,
            registry,
            view: MapView::default(),
            tile_layer,
            tile_style,
            map_pane: MapPane::default(),
            draft: Draft::default(),
            session: None,
            rail: RailView::New,
            runs_cache: Vec::new(),
            runs_cache_at: None,
            probe: None,
            probe_slot: WorkerSlot::idle("probe"),
            probe_at: None,
            estimate: None,
            estimate_slot: WorkerSlot::idle("estimate"),
            estimate_fingerprint: 0,
            estimate_settle: None,
            estimate_shown_fingerprint: 0,
            estimate_inflight_fingerprint: None,
            estimate_runs: 0,
            last_estimate_plan: None,
            review: None,
            review_slot: WorkerSlot::idle("resolve"),
            advanced: None,
            advanced_generate_slot: WorkerSlot::idle("generate-config"),
            advanced_generate_stale: false,
            advanced_generate_intent: None,
            generation_refused_fingerprint: None,
            advanced_resolve_slot: WorkerSlot::idle("advanced-resolve"),
            model: ModelLayer::default(),
            status: None,
            tree,
            smoke_deadline: None,
            smoke_screenshot_requested: None,
            demo_draft: false,
            demo_nudged: false,
            demo_advanced_requested: false,
            demo_storm_applied: false,
            demo_offcentre: false,
            demo_offcentre_requested: false,
            demo_offcentre_applied: false,
            demo_drawroot: false,
            demo_drawroot_applied: false,
            demo_drawroot_run: false,
            demo_drawroot_launched: false,
            demo_carry: false,
            demo_carry_stage: 0,
            exe_started: std::time::SystemTime::now(),
            newer_build: None,
            newer_build_checked_at: None,
            fit: None,
            fit_slot: WorkerSlot::idle("fit"),
            fit_fingerprint: 0,
            placeholder_root: None,
            placeholder_tree: Vec::new(),
            pending_placements: Vec::new(),
            pending_follow: None,
            livefire: None,
            products: crate::products::ProductsState::default(),
            catalog_slot: WorkerSlot::idle("catalog"),
        };
        app.reattach_live_run_if_any();
        app
    }

    /// Startup reattach: the newest registry entry whose heartbeat says
    /// the run is still alive gets a session immediately (heartbeat =
    /// current state, events replay = history).
    fn reattach_live_run_if_any(&mut self) {
        let runs = self.registry.list();
        if let Some(entry) = runs.first() {
            let live = entry
                .record
                .run_dir
                .as_deref()
                .map(|run_dir| {
                    std::fs::read_to_string(std::path::Path::new(run_dir).join("run-progress.json"))
                        .ok()
                        .and_then(|text| arwen_plan::RunProgress::parse(&text).ok())
                        .map(|progress| !matches!(progress.status.as_str(), "complete" | "failed"))
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            if live {
                self.session = Some(RunSession::reattach(
                    entry.dir.clone(),
                    entry.record.clone(),
                ));
                self.status = Some((
                    format!("reattached to running forecast {}", entry.record.name),
                    false,
                ));
            }
        }
        self.runs_cache = runs;
        self.runs_cache_at = Some(Instant::now());
    }

    fn poll_workers(&mut self, ctx: &egui::Context) {
        // Probe: poll-safe half only, every 30 s.
        if let SlotPoll::Ready(result) = self.probe_slot.poll() {
            self.probe = Some(result);
        }
        let due = self
            .probe_at
            .map(|at| at.elapsed() > Duration::from_secs(30))
            .unwrap_or(true);
        if due
            && !self.probe_slot.in_flight()
            && let Ok(contract) = &self.contract
        {
            let contract = contract.clone();
            self.probe_at = Some(Instant::now());
            self.probe_slot.spawn(ctx, move |tx| {
                let _ = tx.send(contract.probe());
            });
        }

        // Estimate: CONTINUOUS re-pricing, debounced on the draft
        // fingerprint. Every plan-affecting change (domain drag/resize/
        // move, ladder/chain, cadence, length, source, profile, advanced
        // edits via custom.rev, favorites) moves the fingerprint and
        // re-runs the engine; the strip shows the stale state meanwhile.
        if let SlotPoll::Ready(result) = self.estimate_slot.poll() {
            self.estimate = Some(result);
            self.estimate_shown_fingerprint =
                self.estimate_inflight_fingerprint.take().unwrap_or(0);
        }
        if self.rail == RailView::New && self.session.is_none() {
            let fingerprint = self.current_estimate_fingerprint();
            if fingerprint != self.estimate_fingerprint {
                self.estimate_fingerprint = fingerprint;
                self.estimate_settle = Some(Instant::now());
            }
            let settled = self
                .estimate_settle
                .map(|at| at.elapsed() > Duration::from_millis(350))
                .unwrap_or(false);
            if settled && !self.estimate_slot.in_flight() {
                match self
                    .draft
                    .to_plan_with_geog(
                        &self.settings.output_root,
                        self.settings.geog_root.as_deref(),
                        &self.settings.favorite_products,
                    )
                    .and_then(|plan| plan.to_json_pretty().map_err(|error| error.to_string()))
                {
                    Ok(plan_json) => {
                        if let Ok(contract) = &self.contract {
                            let contract = contract.clone();
                            self.estimate_settle = None;
                            self.estimate_inflight_fingerprint = Some(fingerprint);
                            self.estimate_runs += 1;
                            self.last_estimate_plan = Some(plan_json.clone());
                            self.estimate_slot.spawn(ctx, move |tx| {
                                let _ = tx.send(contract.estimate(&plan_json));
                            });
                        }
                    }
                    // Not plan-buildable (e.g. domain deleted): stop
                    // debouncing; the strip's stale flag says the shown
                    // numbers are not this draft's.
                    Err(_) => self.estimate_settle = None,
                }
            }
            // The debounce and the in-flight query must complete WITHOUT
            // further user input — this repaint keeps the pipeline alive
            // (the original defect: pricing waited for the next mouse
            // event and read as "calculates once and never again").
            if self.estimate_settle.is_some() || self.estimate_slot.in_flight() {
                ctx.request_repaint_after(Duration::from_millis(120));
            }
        }

        // The continuous fit: the engine's fitted TREE for the current
        // draft, kept current on the estimate's debounce cadence so the
        // map always has children to select/drag in the normal flow.
        // A refused draft keeps the last good fit on screen; the refusal
        // itself rides the estimate strip's inline sentences (same
        // engine, same words).
        if let SlotPoll::Ready(Ok(report)) = self.fit_slot.poll() {
            let fitted = arwen_plan::configuration_lambert_geometry(&report.configuration)
                .map(|geometry| crate::run_session::lambert_from_geometry(&geometry));
            if let Some(fitted) = fitted {
                self.fit = Some(LiveFit {
                    fitted,
                    tree: arwen_plan::queries::configuration_domain_tree(&report.configuration),
                    floor: report.domain_size_floor.clone(),
                });
            }
        }
        if self.rail == RailView::New
            && self.session.is_none()
            && self.draft.custom.is_none()
            // A config surface is being written (draw/drop-initiated):
            // the shadow fit would paint the engine's INTENT-fitted tree
            // over the drawn rectangle for a frame or two — the exact
            // square the drawn size is about to override. Hold off; the
            // surface's own resolve takes over the moment it lands.
            && !self.advanced_generate_slot.in_flight()
            && self.pending_placements.is_empty()
        {
            let fingerprint = self.current_estimate_fingerprint();
            // Spawn once the draft has settled (the estimate's own
            // debounce is the settle signal) and the slot is free.
            if fingerprint != self.fit_fingerprint
                && self.estimate_settle.is_none()
                && !self.fit_slot.in_flight()
            {
                match self
                    .draft
                    .to_plan_with_geog(
                        &self.settings.output_root,
                        self.settings.geog_root.as_deref(),
                        &self.settings.favorite_products,
                    )
                    .and_then(|plan| plan.to_json_pretty().map_err(|error| error.to_string()))
                {
                    Ok(plan_json) => {
                        if let Ok(contract) = &self.contract {
                            let contract = contract.clone();
                            self.fit_fingerprint = fingerprint;
                            self.fit_slot.spawn(ctx, move |tx| {
                                let _ = tx.send(contract.resolve(&plan_json));
                            });
                        }
                    }
                    Err(_) => {
                        // Not plan-buildable (e.g. domain deleted): the
                        // old tree must not linger on an empty sketch.
                        self.fit_fingerprint = fingerprint;
                        self.fit = None;
                    }
                }
            }
            if self.fit_slot.in_flight() {
                ctx.request_repaint_after(Duration::from_millis(150));
            }
        }

        // Resolve (review sheet). The engine's fitted geometry becomes
        // the map outline — the sketch was intent, this is the answer.
        if let SlotPoll::Ready(result) = self.review_slot.poll() {
            match result {
                Ok((report, plan_json)) => {
                    let fitted = arwen_plan::configuration_lambert_geometry(&report.configuration)
                        .map(|geometry| crate::run_session::lambert_from_geometry(&geometry));
                    let tree =
                        arwen_plan::queries::configuration_domain_tree(&report.configuration);
                    self.review = Some(ReviewSheet {
                        report,
                        plan_json,
                        fitted,
                        tree,
                    });
                }
                Err(error) => self.status = Some((format!("resolve failed: {error}"), true)),
            }
        }

        // The render catalog (fetched once, on demand).
        if let SlotPoll::Ready(result) = self.catalog_slot.poll() {
            self.products.fetching = false;
            self.products.catalog = Some(result);
        }

        // Advanced surface: engine generation landing.
        if let SlotPoll::Ready(result) = self.advanced_generate_slot.poll() {
            let spawned_intent = self.advanced_generate_intent.take();
            let intent_moved = spawned_intent
                != Some((
                    self.draft.source().source.to_string(),
                    self.draft.root_dx_km.to_bits(),
                    self.draft.nests.clone(),
                ));
            if self.advanced_generate_stale || intent_moved {
                // The rectangle (or the SOURCE PICKER) changed mid-write:
                // this emission speaks a superseded intent. Drop it and
                // re-generate for the current draft; queued placements
                // stay queued.
                self.advanced_generate_stale = false;
                drop(result);
                self.open_advanced_surface(ctx);
            } else {
                match result {
                    Ok(generated) => {
                        let edited_path = generated.workspace.join("draft-config.toml");
                        // The routes find their side files BY THE CONFIG'S
                        // STEM — <stem>.namelist.wps on every prepared
                        // source, and on HRRR also <stem>.namelist.input,
                        // <stem>.stock.namelist.input and
                        // <stem>.d01-target.json (the engine's own
                        // route_input_paths table). The EDITED copy needs
                        // every one of them under its own stem or the
                        // route refuses before any work: `go` at authority
                        // for the WPS pair, and for HRRR "this config was
                        // not emitted for the HRRR route" (matrix-found by
                        // l09). Copied BY PATTERN rather than by a list of
                        // names, so a side file the engine adds travels
                        // without a Studio release.
                        let emitted_stem = generated
                            .config_path
                            .file_stem()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned();
                        let edited_stem = edited_path
                            .file_stem()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned();
                        if let Ok(entries) = std::fs::read_dir(&generated.workspace) {
                            for entry in entries.flatten() {
                                let name = entry.file_name().to_string_lossy().into_owned();
                                let Some(suffix) = name.strip_prefix(&format!("{emitted_stem}."))
                                else {
                                    continue;
                                };
                                // The config itself is written below, from
                                // the emission's own bytes.
                                if suffix.eq_ignore_ascii_case("toml") {
                                    continue;
                                }
                                let _ = std::fs::copy(
                                    entry.path(),
                                    generated.workspace.join(format!("{edited_stem}.{suffix}")),
                                );
                            }
                        }
                        match std::fs::write(&edited_path, &generated.text) {
                            Ok(()) => {
                                let route = self.draft.source().route.to_string();
                                self.draft.custom = Some(crate::draft::CustomPlanConfig {
                                    config_path: edited_path.to_string_lossy().into_owned(),
                                    route: route.clone(),
                                    source: self.draft.source().source.to_string(),
                                    root_dx_km: self.draft.root_dx_km,
                                    nests: self.draft.nests.clone(),
                                    rev: 0,
                                });
                                self.advanced = Some(crate::advanced::AdvancedState::new(
                                    generated.workspace.clone(),
                                    edited_path,
                                    route,
                                    generated.text,
                                ));
                                self.generation_refused_fingerprint = None;
                                self.status = Some((
                                    "engine wrote the config — every knob is editable".into(),
                                    false,
                                ));
                            }
                            Err(error) => {
                                self.status =
                                    Some((format!("save editable config: {error}"), true));
                            }
                        }
                    }
                    Err(error) => {
                        // The engine's own refusal (e.g. ERA5 needs a pinned
                        // cycle), verbatim. Queued manual geometry is KEPT:
                        // the retry fires when the draft changes (a pinned
                        // cycle, a different source), never on the same
                        // refused fingerprint.
                        self.status = Some((format!("config generation refused: {error}"), true));
                        self.generation_refused_fingerprint =
                            Some(self.current_estimate_fingerprint());
                    }
                }
                // A declared moving nest rides the fresh emission. The
                // named child may not exist in the new shape (a ladder
                // change defines new children), so the target is
                // re-pointed at the first nest or, with no nest at all,
                // the declaration is dropped — out loud, never silently.
                if self.advanced.is_some()
                    && let Some(mut follow) = self.pending_follow.take()
                {
                    let nests = self
                        .advanced
                        .as_ref()
                        .map(|state| crate::storm::nest_domains(&state.model))
                        .unwrap_or_default();
                    let named = nests.iter().any(|(_, grid)| *grid == follow.grid_id);
                    match (named, nests.first()) {
                        (true, _) => {
                            self.apply_config_rewrite(|text| {
                                crate::storm::write_relocation(text, Some(&follow))
                            });
                        }
                        (false, Some((_, grid_id))) => {
                            let moved = format!(
                                "storm following carried to d{:02} — d{:02} is \
                                 not in the new shape",
                                grid_id, follow.grid_id
                            );
                            follow.grid_id = *grid_id;
                            self.apply_config_rewrite(|text| {
                                crate::storm::write_relocation(text, Some(&follow))
                            });
                            self.status = Some((moved, false));
                        }
                        (false, None) => {
                            self.status = Some((
                                "storm following dropped — the new shape has no \
                                 nested domain to move"
                                    .into(),
                                true,
                            ));
                        }
                    }
                }
                // A queued map drop rides the fresh surface immediately —
                // rescaled from its drop-time parent grid into the written
                // config's, so it keeps its relative position through the
                // refit. The review's intent-resolve snapshot no longer
                // matches, so it closes (reopen re-resolves the edited
                // config).
                if self.advanced.is_some() && !self.pending_placements.is_empty() {
                    // Gesture-initiated generation: the surface goes live but
                    // its editor WINDOW stays closed — a drag must not bury
                    // the map under the knob editor.
                    if let Some(state) = &mut self.advanced {
                        state.open = false;
                    }
                    let pending: Vec<_> = self.pending_placements.drain(..).collect();
                    for entry in pending {
                        let crate::map_pane::PlacementEdit::Place {
                            grid_id,
                            i_parent_start,
                            j_parent_start,
                            size,
                        } = entry.edit
                        else {
                            continue;
                        };
                        if let Some((i, j, size)) = self.scale_into_config(
                            grid_id,
                            i_parent_start,
                            j_parent_start,
                            size,
                            entry.parent_span,
                        ) {
                            self.write_nest_placement(grid_id, i, j, size);
                        }
                    }
                    // The preview was in drop-time cells; the config now
                    // carries the (possibly rescaled) truth.
                    self.map_pane.clear_nest_preview();
                    self.review = None;
                }
            }
        }

        // A refused generation left manual geometry queued: retry the
        // moment the draft moves off the refused fingerprint (a pinned
        // cycle, another source) — never a hammer loop on the same
        // refusal.
        if !self.pending_placements.is_empty()
            && self.advanced.is_none()
            && !self.advanced_generate_slot.in_flight()
            && let Some(refused) = self.generation_refused_fingerprint
        {
            let fingerprint = self.current_estimate_fingerprint();
            if fingerprint != refused {
                self.generation_refused_fingerprint = Some(fingerprint);
                self.open_advanced_surface(ctx);
            }
        }

        // Advanced surface: debounced write + engine re-validation.
        if let SlotPoll::Ready(result) = self.advanced_resolve_slot.poll()
            && let Some(state) = &mut self.advanced
        {
            state.resolving = false;
            match result {
                Ok(report) => {
                    state.fitted =
                        arwen_plan::configuration_lambert_geometry(&report.configuration)
                            .map(|geometry| crate::run_session::lambert_from_geometry(&geometry));
                    state.tree =
                        arwen_plan::queries::configuration_domain_tree(&report.configuration);
                    state.resolve = Some(Ok(Box::new(report)));
                }
                Err(error) => state.resolve = Some(Err(error)),
            }
        }
        if let Some(state) = &mut self.advanced
            && let Some(at) = state.dirty_at
            && at.elapsed() > Duration::from_millis(700)
            && !self.advanced_resolve_slot.in_flight()
        {
            state.dirty_at = None;
            if let Err(error) = std::fs::write(&state.config_path, &state.model.text) {
                state.resolve = Some(Err(format!("save edited config: {error}")));
            } else {
                // Every side file beside the config tracks its domain
                // geometry: the prepared route cross-checks the WPS pair
                // and refuses a mismatch (matrix-found: a manual root
                // size stopped `go` at prepare with the geometry dump),
                // and the HRRR route reads its namelist pair + target
                // JSON INSTEAD of the TOML (matrix-found by l09). The
                // mapping is the engine's own published expectation —
                // e_we/e_sn = nx/ny + 1, anchors verbatim, projection
                // refs from [projection]; its cross-check stays judge.
                sync_config_side_files(&state.config_path, &state.model);
                if let Some(custom) = &mut self.draft.custom {
                    custom.rev = state.rev;
                }
                if let Ok(contract) = &self.contract
                    && let Ok(plan) = self.draft.to_plan_with_geog(
                        &self.settings.output_root,
                        self.settings.geog_root.as_deref(),
                        &self.settings.favorite_products,
                    )
                    && let Ok(plan_json) = plan.to_json_pretty()
                {
                    let contract = contract.clone();
                    state.resolving = true;
                    self.advanced_resolve_slot.spawn(ctx, move |tx| {
                        let _ = tx.send(contract.resolve(&plan_json));
                    });
                }
            }
            ctx.request_repaint_after(Duration::from_millis(100));
        } else if self
            .advanced
            .as_ref()
            .is_some_and(|state| state.dirty_at.is_some())
        {
            ctx.request_repaint_after(Duration::from_millis(150));
        }

        // The active run's file tail + heartbeat.
        if let Some(session) = &mut self.session {
            session.poll();
            if !session.is_finished() {
                ctx.request_repaint_after(Duration::from_millis(250));
            }
        }

        // Keep the model field layer pointed at the displayed frame.
        let wanted = self
            .session
            .as_ref()
            .and_then(|session| session.display_frame())
            .map(|(_, frame)| frame.path.clone());
        self.model.drive(ctx, wanted.as_deref());

        // Decoded basemap tiles.
        self.tile_layer.poll(ctx);

        // The stale-binary killer: every 30 s, is a newer build on disk?
        if self
            .newer_build_checked_at
            .map(|at| at.elapsed() > Duration::from_secs(30))
            .unwrap_or(true)
        {
            self.newer_build_checked_at = Some(Instant::now());
            self.newer_build = newer_build_on_disk(self.exe_started);
        }
    }

    /// The ACTIVE config surface's domain count — the shape that
    /// actually launches (`None` = intent plan, ladder is the shape).
    pub(crate) fn active_config_domains(&self) -> Option<usize> {
        let state = self.advanced.as_ref()?;
        let mut count = 0;
        while state
            .model
            .entries
            .iter()
            .any(|entry| entry.table == format!("domain[{count}]"))
        {
            count += 1;
        }
        Some(count)
    }

    /// What the ACTIVE config says about a moving nest, and what the
    /// engine answered about it — the state the follow card renders and
    /// the prepared-route row gates on.
    pub(crate) fn moving_nest(&self) -> crate::storm::MovingNest {
        crate::storm::MovingNest::read(
            self.advanced.as_ref().map(|state| &state.model),
            self.advanced
                .as_ref()
                .and_then(|state| state.resolve.as_ref()),
        )
    }

    /// The known-unrunnable-combo block for the current draft (the Run
    /// button's blocked state; also the last-line launch guard).
    pub(crate) fn route_block(&self) -> Option<String> {
        let engine = self
            .draft
            .route_block(self.active_config_domains(), &self.moving_nest());
        match (self.host_launch_block.as_deref(), engine) {
            (Some(host), Some(engine)) => Some(format!("{host}\n\nEngine route: {engine}")),
            (Some(host), None) => Some(host.to_owned()),
            (None, engine) => engine,
        }
    }

    fn launch(&mut self) {
        // Never a launch into a known refusal, even if the UI raced a
        // regeneration: the same table that blocks the button.
        if let Some(reason) = self.route_block() {
            self.review = None;
            self.status = Some((reason, true));
            return;
        }
        let Some(review) = self.review.take() else {
            return;
        };
        let Ok(contract) = &self.contract else {
            return;
        };
        match contract.launch(
            &self.registry,
            &review.plan_json,
            self.draft.name.trim(),
            RunOwnership::SurviveStudio,
        ) {
            Ok(launched) => {
                let mut session = RunSession::from_launch(launched);
                // The review's resolutions stand in until the run's own
                // resolved_plan event arrives.
                session.resolutions = review.report.automatic_resolutions.clone();
                self.session = Some(session);
                self.runs_cache_at = None;
                self.status = Some((format!("forecast {} launched", self.draft.name), false));
            }
            Err(error) => self.status = Some((format!("launch failed: {error}"), true)),
        }
    }

    fn current_estimate_fingerprint(&self) -> u64 {
        self.draft.fingerprint(&self.settings.output_root)
            ^ favorites_hash(&self.settings.favorite_products)
    }

    /// The current status line's text (matrix cells assert surfaced
    /// refusals through it — a refusal must never be silent).
    #[cfg(test)]
    pub(crate) fn status_text(&self) -> Option<&str> {
        self.status.as_ref().map(|(text, _)| text.as_str())
    }

    /// Matrix cells launch REAL runs: point the registry at the walk's
    /// temp dir so they never land in the box's Runs list.
    #[cfg(test)]
    pub(crate) fn redirect_registry(&mut self, root: std::path::PathBuf) {
        self.registry = RunRegistry::new(root);
        self.runs_cache = Vec::new();
        self.session = None;
    }

    /// Is the DISPLAYED estimate priced for a draft other than the
    /// current one (or a re-price pending/in flight)?
    pub(crate) fn estimate_is_stale(&self) -> bool {
        self.estimate.is_some()
            && (self.estimate_shown_fingerprint != self.current_estimate_fingerprint()
                || self.estimate_slot.in_flight()
                || self.estimate_settle.is_some())
    }

    /// Open (or seed) the Advanced surface: the ENGINE writes its config
    /// into a durable drafts workspace via a dry-run plan; the editor
    /// opens over the emitted file.
    pub(crate) fn open_advanced_surface(&mut self, ctx: &egui::Context) {
        if let Some(state) = &mut self.advanced {
            state.open = true;
            return;
        }
        if self.advanced_generate_slot.in_flight() {
            return;
        }
        let Ok(contract) = &self.contract else {
            return;
        };
        let drafts_root = std::path::Path::new(&self.settings.output_root)
            .parent()
            .map(|parent| parent.join("drafts"))
            .unwrap_or_else(|| std::env::temp_dir().join("arwen-studio-drafts"));
        let workspace = drafts_root.join(format!(
            "{}-{:05}",
            self.draft.name.trim(),
            self.draft.fingerprint(&self.settings.output_root) % 100_000
        ));
        match self
            .draft
            .to_generation_plan(
                &workspace.to_string_lossy(),
                self.settings.geog_root.as_deref(),
            )
            .and_then(|plan| plan.to_json_pretty().map_err(|error| error.to_string()))
        {
            Ok(plan_json) => {
                let contract = contract.clone();
                self.advanced_generate_intent = Some((
                    self.draft.source().source.to_string(),
                    self.draft.root_dx_km.to_bits(),
                    self.draft.nests.clone(),
                ));
                self.status = Some(("engine is writing the config (dry run)…".into(), false));
                self.advanced_generate_slot.spawn(ctx, move |tx| {
                    let _ = tx.send(contract.generate_config(&plan_json, &workspace));
                });
            }
            Err(error) => self.status = Some((error, true)),
        }
    }

    /// Rewrite the Advanced surface's TOML through a whole-block editor
    /// (storm cards); the same debounce re-validates via the engine.
    fn apply_config_rewrite(&mut self, rewrite: impl FnOnce(&str) -> String) {
        if let Some(state) = &mut self.advanced {
            let new_text = rewrite(&state.model.text);
            if new_text != state.model.text {
                state.model = crate::advanced::ConfigModel::parse(&new_text);
                state.dirty_at = Some(Instant::now());
                state.rev += 1;
            }
        }
    }

    /// The engine's domain-size floor data backing the displayed tree:
    /// `(clearance_rows, nest_span_mass_points)`. Engine numbers only —
    /// `None` when no report carries them (no band, no invented floor).
    fn placement_floor(&self) -> (Option<f64>, Option<u32>) {
        let floor = self
            .advanced
            .as_ref()
            .and_then(|state| state.resolve.as_ref())
            .and_then(|result| result.as_ref().ok())
            .and_then(|report| report.domain_size_floor.as_ref())
            .or_else(|| {
                self.review
                    .as_ref()
                    .and_then(|review| review.report.domain_size_floor.as_ref())
            })
            .or_else(|| self.fit.as_ref().and_then(|fit| fit.floor.as_ref()));
        match floor {
            Some(value) => (
                value.get("clearance_rows").and_then(|v| v.as_f64()),
                value
                    .get("nest_span_mass_points")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32),
            ),
            None => (None, None),
        }
    }

    /// What the map paints and gestures against, in priority order:
    /// working-config resolve → open review → the continuous fit →
    /// draft placeholders. The bool is `provisional` (placeholders).
    fn displayed_fit(
        &self,
    ) -> (
        Option<&arwen_map::LambertDomain>,
        &[arwen_plan::queries::ResolvedDomain],
        bool,
    ) {
        if let Some(state) = self
            .advanced
            .as_ref()
            .filter(|state| !state.tree.is_empty())
        {
            return (state.fitted.as_ref(), state.tree.as_slice(), false);
        }
        if let Some(review) = &self.review {
            return (review.fitted.as_ref(), review.tree.as_slice(), false);
        }
        if let Some(fit) = &self.fit {
            return (Some(&fit.fitted), fit.tree.as_slice(), false);
        }
        (
            self.placeholder_root.as_ref(),
            self.placeholder_tree.as_slice(),
            true,
        )
    }

    /// Rebuild the draft-derived placeholder tree (the bridge until the
    /// first fit answers): every child centered in its parent, spanning
    /// half of it — a sketch, labelled as such, replaced by the engine's
    /// own fit within one resolve round trip.
    fn refresh_placeholders(&mut self) {
        self.placeholder_root = None;
        self.placeholder_tree.clear();
        let advanced_live = self
            .advanced
            .as_ref()
            .is_some_and(|state| !state.tree.is_empty());
        if advanced_live || self.review.is_some() || self.fit.is_some() {
            return;
        }
        let Some(domain) = self.draft.domain else {
            return;
        };
        if self.draft.nests.is_empty() {
            return;
        }
        self.placeholder_root = Some(domain);
        let mut tree = vec![arwen_plan::queries::ResolvedDomain {
            grid_id: 1,
            parent_id: 0,
            i_parent_start: 1.0,
            j_parent_start: 1.0,
            parent_grid_ratio: 1.0,
            nx: domain.nx,
            ny: domain.ny,
            dx_m: domain.dx_m,
            history_interval_s: None,
            dt_s: None,
            spawn_trigger: None,
            spawn_threshold: None,
            spawn_at_s: None,
        }];
        let (mut parent_nx, mut parent_ny, mut dx_m) = (domain.nx, domain.ny, domain.dx_m);
        for (level, ratio) in self.draft.nests.iter().enumerate() {
            let ratio = (*ratio).max(1);
            let span_x = (parent_nx / 2).max(2);
            let span_y = (parent_ny / 2).max(2);
            let nx = span_x * ratio;
            let ny = span_y * ratio;
            dx_m /= ratio as f64;
            tree.push(arwen_plan::queries::ResolvedDomain {
                grid_id: level as u32 + 2,
                parent_id: level as u32 + 1,
                i_parent_start: ((parent_nx - span_x) / 2 + 1) as f64,
                j_parent_start: ((parent_ny - span_y) / 2 + 1) as f64,
                parent_grid_ratio: ratio as f64,
                nx,
                ny,
                dx_m,
                history_interval_s: None,
                dt_s: None,
                spawn_trigger: None,
                spawn_threshold: None,
                spawn_at_s: None,
            });
            parent_nx = nx;
            parent_ny = ny;
        }
        self.placeholder_tree = tree;
    }

    /// The parent-grid span a drop on the DISPLAYED tree was made in
    /// (for the relative rescale when the written config arrives).
    fn displayed_parent_span(&self, grid_id: u32) -> Option<(u32, u32)> {
        let (_, tree, _) = self.displayed_fit();
        let nest = tree.iter().find(|nest| nest.grid_id == grid_id)?;
        let parent = tree
            .iter()
            .find(|domain| domain.grid_id == nest.parent_id)?;
        Some((parent.nx, parent.ny))
    }

    /// Rescale a queued drop from its drop-time parent grid into the
    /// written config's parent grid — the drop keeps its RELATIVE
    /// position/extent through the engine's refit. Pure gesture-space
    /// mapping; the engine's resolve stays the judge.
    fn scale_into_config(
        &self,
        grid_id: u32,
        i: i64,
        j: i64,
        size: Option<(u32, u32)>,
        parent_span: Option<(u32, u32)>,
    ) -> Option<ScaledPlacement> {
        let state = self.advanced.as_ref()?;
        let index = state.model.domain_index_for_grid(grid_id)?;
        let Some((from_nx, from_ny)) = parent_span else {
            return Some((i, j, size));
        };
        let parent_grid: u32 = state
            .model
            .domain_value(index, "parent_id")?
            .trim()
            .parse()
            .ok()?;
        if parent_grid == 0 {
            return Some((i, j, size));
        }
        let parent_index = state.model.domain_index_for_grid(parent_grid)?;
        let to_nx: f64 = state
            .model
            .domain_value(parent_index, "nx")?
            .trim()
            .parse()
            .ok()?;
        let to_ny: f64 = state
            .model
            .domain_value(parent_index, "ny")?
            .trim()
            .parse()
            .ok()?;
        let fx = to_nx / from_nx.max(1) as f64;
        let fy = to_ny / from_ny.max(1) as f64;
        if (fx - 1.0).abs() < 1e-9 && (fy - 1.0).abs() < 1e-9 {
            return Some((i, j, size));
        }
        let scaled_size = size.map(|(nx, ny)| {
            (
                ((nx as f64) * fx).round().max(1.0) as u32,
                ((ny as f64) * fy).round().max(1.0) as u32,
            )
        });
        Some((
            ((i as f64) * fx).round().max(1.0) as i64,
            ((j as f64) * fy).round().max(1.0) as i64,
            scaled_size,
        ))
    }

    /// The engine's refusal of the current working config (first line,
    /// full text stays in the Advanced banner) — painted at the map foot
    /// so a refused placement explains itself where the drag happened.
    pub(crate) fn placement_refusal(&self) -> Option<String> {
        match &self.advanced.as_ref()?.resolve {
            Some(Err(error)) => error
                .lines()
                .find(|line| !line.trim().is_empty())
                .map(str::to_string),
            _ => None,
        }
    }

    /// Every domain the current refusal NAMES (the engine spells the
    /// offender as `grid_id = N`), with its sentence — the map paints
    /// each one red with the sentence anchored to it. The working
    /// config's resolve refusal first; the estimate's refusal covers
    /// the window before the debounced resolve answers.
    pub(crate) fn placement_refusal_domains(&self) -> Vec<(u32, String)> {
        if let Some(state) = &self.advanced
            && let Some(Err(error)) = &state.resolve
        {
            let named = crate::advanced::refusal_named_domains(error);
            if !named.is_empty() {
                return named;
            }
        }
        if let Some(Err(error)) = &self.estimate {
            return crate::advanced::refusal_named_domains(error);
        }
        Vec::new()
    }

    /// Spawn search boxes declared in the working config, as the map
    /// paints them: `(spawning grid_id, bounds)`.
    fn config_search_boxes(&self) -> Vec<(u32, [i64; 4])> {
        let Some(state) = &self.advanced else {
            return Vec::new();
        };
        crate::storm::nest_domains(&state.model)
            .into_iter()
            .filter_map(|(domain_index, grid_id)| {
                crate::storm::parse_spawn(&state.model, domain_index)
                    .and_then(|spawn| spawn.search_box)
                    .map(|bounds| (grid_id as u32, bounds))
            })
            .collect()
    }

    /// Is `grid_id` in the tree the map is currently painting?
    fn displayed_tree_has(&self, grid_id: u32) -> bool {
        let (_, tree, _) = self.displayed_fit();
        tree.iter().any(|nest| nest.grid_id == grid_id)
    }

    /// ONE source of truth: a nest drop writes `i_parent_start` /
    /// `j_parent_start` (+ nx/ny on resize) into the working config
    /// exactly as the cards write their blocks; the debounced engine
    /// `--resolve` ratifies. Studio never validates placement itself.
    /// A ROOT size write additionally CARRIES THE CHILDREN (see
    /// [`Self::commit_root_model`]).
    pub(crate) fn write_nest_placement(
        &mut self,
        grid_id: u32,
        i_parent_start: i64,
        j_parent_start: i64,
        size: Option<(u32, u32)>,
    ) {
        let Some(state) = &self.advanced else {
            return;
        };
        let Some(domain_index) = state.model.domain_index_for_grid(grid_id) else {
            self.status = Some((
                format!("the working config has no [[domain]] with grid_id {grid_id}"),
                true,
            ));
            return;
        };
        let is_root = state.model.root_domain_index() == Some(domain_index);
        let new_model =
            state
                .model
                .with_placement(domain_index, i_parent_start, j_parent_start, size);
        if is_root {
            self.commit_root_model(new_model);
            return;
        }
        let Some(state) = &mut self.advanced else {
            return;
        };
        if new_model.text != state.model.text {
            state.model = new_model;
            state.dirty_at = Some(Instant::now());
            state.rev += 1;
        }
        // A direct gesture on a nest supersedes its old carry notice.
        state.repairs.retain(|repair| repair.grid_id != grid_id);
    }

    /// THE CONFIG SURFACE FOLLOWS THE INTENT CARDS — source (the regression's
    /// route-flip: a GFS launch submitted an ERA5-shaped config), root
    /// dx AND ladder (the regression's -74 strand: a 12-3 pick rescaled the
    /// parent's cell grid around cell-indexed children). Any of the
    /// three disagreeing with the active surface regenerates it:
    /// manual root geometry carried (SKETCH dims when dx changed —
    /// footprint-true across the rescale), children carried relative
    /// when the ladder is unchanged, reset to the engine's fresh fit
    /// when it changed. The landing carry then clamps/refits/shrinks
    /// whatever no longer fits, with row notices.
    fn follow_intent_cards(&mut self, ctx: &egui::Context) {
        let picker = self.draft.source().source;
        if self.advanced_generate_slot.in_flight() {
            // Mid-write card moves are handled at landing (the
            // spawned-intent staleness check).
            return;
        }
        let Some(custom) = &self.draft.custom else {
            return;
        };
        let source_changed = custom.source != picker;
        let dx_changed = custom.root_dx_km.to_bits() != self.draft.root_dx_km.to_bits();
        let ladder_changed = custom.nests != self.draft.nests;
        if !(source_changed || dx_changed || ladder_changed) {
            return;
        }
        let from = custom.source.clone();
        let reason = if ladder_changed {
            "ladder changed — children reset to the engine's fresh fit"
        } else if dx_changed {
            "root dx changed — footprint kept, children rescaled"
        } else {
            "source changed — manual geometry carried"
        };
        let mut pending = Vec::new();
        // A declared moving nest survives the regeneration, the same way
        // manual geometry does: the [relocation] tables are the user's
        // intent, and dropping them on a source switch is how a follow
        // config quietly became a still one.
        self.pending_follow = self
            .advanced
            .as_ref()
            .and_then(|state| crate::storm::parse_follow(&state.model));
        if let Some(state) = &self.advanced {
            let value = |index: usize, key: &str| -> Option<f64> {
                state
                    .model
                    .domain_value(index, key)?
                    .trim()
                    .parse::<f64>()
                    .ok()
            };
            for index in 0.. {
                let table = format!("domain[{index}]");
                if !state.model.entries.iter().any(|entry| entry.table == table) {
                    break;
                }
                if !crate::advanced::placement_is_manual(&state.model, &state.base_model, index) {
                    continue;
                }
                let (Some(grid), Some(i), Some(j), Some(nx), Some(ny)) = (
                    value(index, "grid_id"),
                    value(index, "i_parent_start"),
                    value(index, "j_parent_start"),
                    value(index, "nx"),
                    value(index, "ny"),
                ) else {
                    continue;
                };
                let parent = value(index, "parent_id").unwrap_or(0.0) as u32;
                if parent == 0 {
                    // The manual ROOT: footprint-true across a dx change
                    // (the sketch was rescaled with the dx card), config
                    // dims otherwise.
                    let (root_nx, root_ny) = match (dx_changed, self.draft.domain) {
                        (true, Some(sketch)) => (sketch.nx, sketch.ny),
                        _ => (nx as u32, ny as u32),
                    };
                    pending.push(PendingPlacement {
                        edit: crate::map_pane::PlacementEdit::Place {
                            grid_id: grid as u32,
                            i_parent_start: 1,
                            j_parent_start: 1,
                            size: Some((root_nx, root_ny)),
                        },
                        parent_span: None,
                    });
                    continue;
                }
                if ladder_changed {
                    // A new ladder defines new children: manual child
                    // placements do not survive it (the redraw rule).
                    continue;
                }
                let parent_span =
                    state
                        .model
                        .domain_index_for_grid(parent)
                        .and_then(|parent_index| {
                            Some((
                                value(parent_index, "nx")? as u32,
                                value(parent_index, "ny")? as u32,
                            ))
                        });
                pending.push(PendingPlacement {
                    edit: crate::map_pane::PlacementEdit::Place {
                        grid_id: grid as u32,
                        i_parent_start: i as i64,
                        j_parent_start: j as i64,
                        size: Some((nx as u32, ny as u32)),
                    },
                    parent_span,
                });
            }
        }
        self.advanced = None;
        self.draft.custom = None;
        self.review = None;
        self.fit = None;
        self.map_pane.clear_nest_preview();
        self.pending_placements = pending;
        self.status = Some((
            format!("config surface follows the cards ({from} → {picker}): {reason}"),
            false,
        ));
        self.open_advanced_surface(ctx);
    }

    /// Swap in a root-rectangle rewrite, CARRYING THE CHILDREN first:
    /// anchors clamp back into the new legal envelope (whole cells,
    /// inside the clearance band), a nest that no longer fits at its
    /// size refits to the emission's fitted placement with a visible
    /// row notice, and only a nest with no mechanical repair is left
    /// for the engine's refusal (which the map paints red). Bytes are
    /// preserved wherever children still fit unchanged. the regression's strand:
    /// shrinking the root left d02 outside and the engine's clearance
    /// refusal read as a bare estimate failure.
    fn commit_root_model(&mut self, new_model: crate::advanced::ConfigModel) {
        let (floor_clearance, nest_span_floor) = self.placement_floor();
        let Some(state) = &mut self.advanced else {
            return;
        };
        let clearance = floor_clearance
            .or_else(|| crate::advanced::model_clearance_rows(&new_model))
            .unwrap_or(0.0);
        let (repaired, repairs) = crate::advanced::carry_children_with_floor(
            &new_model,
            &state.base_model,
            clearance,
            nest_span_floor,
        );
        if repaired.text != state.model.text {
            state.model = repaired;
            state.dirty_at = Some(Instant::now());
            state.rev += 1;
        }
        if !repairs.is_empty() {
            let names: Vec<String> = repairs
                .iter()
                .map(|repair| format!("d{:02}", repair.grid_id))
                .collect();
            let summary = format!(
                "root change carried {} — the domain rows say how",
                names.join(", ")
            );
            state.repairs = repairs;
            self.status = Some((summary, false));
        }
    }

    /// Map placement gestures → config writes. A drop with no config
    /// surface queues itself and has the engine write one (dry run) —
    /// the drag is never lost, and the placement still rides the same
    /// debounced `--resolve` when the surface lands.
    pub(crate) fn handle_placement_edits(
        &mut self,
        ctx: &egui::Context,
        edits: Vec<crate::map_pane::PlacementEdit>,
    ) {
        use crate::map_pane::PlacementEdit;
        for edit in edits {
            match edit {
                PlacementEdit::Place {
                    grid_id,
                    i_parent_start,
                    j_parent_start,
                    size,
                } => {
                    if self.advanced.is_some() {
                        self.write_nest_placement(grid_id, i_parent_start, j_parent_start, size);
                    } else {
                        let parent_span = self.displayed_parent_span(grid_id);
                        self.pending_placements
                            .push(PendingPlacement { edit, parent_span });
                        self.status = Some((
                            "placement noted — the engine is writing the config \
                             surface to carry it"
                                .into(),
                            false,
                        ));
                        self.open_advanced_surface(ctx);
                    }
                }
                PlacementEdit::RootDrawn { nx, ny } => {
                    // THE DRAWN RECTANGLE IS THE DOMAIN. A completed
                    // (re)draw is a root-rectangle intent change: any
                    // existing surface regenerates for the new rectangle
                    // (children and knob edits reset to the engine's
                    // fresh emission — the redraw confirm says so), and
                    // the drawn size lands as a MANUAL root write the
                    // moment the config arrives. The engine's VRAM fit
                    // becomes the estimate strip's verdict on this
                    // shape, never a silent override of it.
                    if self.advanced_generate_slot.in_flight() {
                        self.advanced_generate_stale = true;
                    }
                    self.advanced = None;
                    self.draft.custom = None;
                    self.review = None;
                    self.fit = None;
                    self.map_pane.clear_nest_preview();
                    self.pending_placements.clear();
                    self.pending_placements.push(PendingPlacement {
                        edit: PlacementEdit::Place {
                            grid_id: 1,
                            i_parent_start: 1,
                            j_parent_start: 1,
                            size: Some((nx, ny)),
                        },
                        parent_span: None,
                    });
                    self.status = Some((
                        format!(
                            "root drawn {nx} × {ny} — the engine is writing \
                             the config; the estimate prices YOUR shape"
                        ),
                        false,
                    ));
                    self.open_advanced_surface(ctx);
                }
                PlacementEdit::RootAdjusted { nx, ny } => {
                    let Some(domain) = self.draft.domain else {
                        continue;
                    };
                    if self.advanced.is_some() {
                        // In place: center + size into the working
                        // config; knob edits keep their bytes and the
                        // CHILDREN COME ALONG (clamp → refit → engine's
                        // refusal on the map). The debounced resolve
                        // re-ratifies the result.
                        let new_model = {
                            let state = self.advanced.as_ref().expect("checked");
                            state
                                .model
                                .with_root_geometry(domain.ref_lat, domain.ref_lon, nx, ny)
                        };
                        self.commit_root_model(new_model);
                    } else {
                        // Same auto-generate as a nest drop: the center
                        // rides the intent, the size queues as a manual
                        // root write (an earlier queued root size is
                        // superseded, not doubled).
                        if self.advanced_generate_slot.in_flight() {
                            self.advanced_generate_stale = true;
                        }
                        self.pending_placements.retain(|pending| {
                            !matches!(pending.edit, PlacementEdit::Place { grid_id: 1, .. })
                        });
                        self.pending_placements.push(PendingPlacement {
                            edit: PlacementEdit::Place {
                                grid_id: 1,
                                i_parent_start: 1,
                                j_parent_start: 1,
                                size: Some((nx, ny)),
                            },
                            parent_span: None,
                        });
                        self.status = Some((
                            "root geometry noted — the engine is writing the \
                             config surface to carry it"
                                .into(),
                            false,
                        ));
                        self.open_advanced_surface(ctx);
                    }
                }
                PlacementEdit::SpawnSearchBox {
                    domain_index,
                    bounds,
                } => {
                    let spawn = self
                        .advanced
                        .as_ref()
                        .and_then(|state| crate::storm::parse_spawn(&state.model, domain_index));
                    if let Some(mut spawn) = spawn {
                        spawn.search_box = Some(bounds);
                        self.apply_config_rewrite(|text| {
                            crate::storm::write_spawn(text, domain_index, Some(&spawn))
                        });
                        self.status = Some((
                            format!(
                                "spawn search box [{}, {}, {}, {}] written — engine validating",
                                bounds[0], bounds[1], bounds[2], bounds[3]
                            ),
                            false,
                        ));
                    }
                }
            }
        }
    }

    pub(crate) fn handle_actions(&mut self, ctx: &egui::Context, mut actions: InspectorActions) {
        if actions.open_advanced || actions.storm.open_advanced {
            self.open_advanced_surface(ctx);
        }
        if let Some(follow) = actions.storm.apply_follow.take() {
            self.apply_config_rewrite(|text| crate::storm::write_relocation(text, follow.as_ref()));
        }
        if let Some((domain_index, spawn)) = actions.storm.apply_spawn.take() {
            self.apply_config_rewrite(|text| {
                crate::storm::write_spawn(text, domain_index, spawn.as_ref())
            });
        }
        if let Some(grid_id) = actions.reset_placement {
            // Reset-to-fitted: restore the engine's own emitted placement
            // keys; the same debounced resolve re-ratifies.
            let fitted = self.advanced.as_ref().and_then(|state| {
                state
                    .model
                    .domain_index_for_grid(grid_id)
                    .and_then(|index| crate::advanced::base_placement(&state.base_model, index))
            });
            if let Some((i, j, nx, ny)) = fitted {
                self.write_nest_placement(grid_id, i, j, Some((nx, ny)));
                self.status = Some((
                    format!("d{grid_id:02} placement reset to the engine's fitted values"),
                    false,
                ));
            }
        }
        if actions.make_single_domain {
            // The blocked Run button's one-click remedy: drop the nests;
            // the surface follows the ladder card by itself (drawn root
            // and manual geometry kept through the regeneration).
            self.draft.nests.clear();
            self.status = Some((
                "single domain — the config surface follows (drawn root kept)".into(),
                false,
            ));
        }
        if let Some(grid_id) = actions.select_nest {
            // Inspector row click ↔ map selection, linked both ways.
            self.map_pane.selected_nest = Some(grid_id);
        }
        if let Some((grid_id, nx, ny)) = actions.set_domain_size {
            // A typed size is a resize: same writer, same validation as
            // a handle drag. Anchor stays where it is — the config's
            // when one exists, else the displayed tree's.
            let anchor = match &self.advanced {
                Some(state) => state
                    .model
                    .domain_index_for_grid(grid_id)
                    .and_then(|index| {
                        let parse = |key: &str| -> Option<i64> {
                            let value = state.model.domain_value(index, key)?.trim();
                            value
                                .parse::<i64>()
                                .ok()
                                .or_else(|| value.parse::<f64>().ok().map(|v| v.round() as i64))
                        };
                        Some((parse("i_parent_start")?, parse("j_parent_start")?))
                    }),
                None => {
                    let (_, tree, _) = self.displayed_fit();
                    tree.iter()
                        .find(|domain| domain.grid_id == grid_id)
                        .map(|domain| {
                            (
                                domain.i_parent_start.round() as i64,
                                domain.j_parent_start.round() as i64,
                            )
                        })
                }
            };
            if let Some((i, j)) = anchor {
                self.handle_placement_edits(
                    ctx,
                    vec![crate::map_pane::PlacementEdit::Place {
                        grid_id,
                        i_parent_start: i,
                        j_parent_start: j,
                        size: Some((nx, ny)),
                    }],
                );
            }
        }
        if let Some((domain_index, grid_id)) = actions.storm.draw_search_box.take() {
            let grid_id = grid_id as u32;
            if self.displayed_tree_has(grid_id) {
                self.map_pane.search_box_arm = Some((domain_index, grid_id));
                self.status = Some((
                    format!(
                        "drag a box inside d{grid_id:02}'s parent to set the spawn \
                         search bounds (Esc cancels)"
                    ),
                    false,
                ));
            } else {
                self.status = Some((
                    "the fitted tree hasn't resolved yet — wait for the engine's \
                     answer, then draw the box"
                        .into(),
                    true,
                ));
            }
        }
        if actions.open_products {
            self.products.open = true;
            if self.products.catalog.is_none()
                && !self.catalog_slot.in_flight()
                && let Ok(contract) = &self.contract
            {
                let contract = contract.clone();
                self.products.fetching = true;
                self.catalog_slot.spawn(ctx, move |tx| {
                    let _ = tx.send(contract.catalog());
                });
            }
        }
        if actions.open_review && !self.review_slot.in_flight() {
            match self
                .draft
                .to_plan_with_geog(
                    &self.settings.output_root,
                    self.settings.geog_root.as_deref(),
                    &self.settings.favorite_products,
                )
                .and_then(|plan| plan.to_json_pretty().map_err(|error| error.to_string()))
            {
                Ok(plan_json) => {
                    if let Ok(contract) = &self.contract {
                        let contract = contract.clone();
                        self.review_slot.spawn(ctx, move |tx| {
                            let result = contract
                                .resolve(&plan_json)
                                .map(|report| (report, plan_json));
                            let _ = tx.send(result);
                        });
                    }
                }
                Err(error) => self.status = Some((error, true)),
            }
        }
        if actions.close_review {
            self.review = None;
        }
        if actions.launch {
            self.launch();
        }
        if let Some(index) = actions.open_run {
            if let Some(entry) = self.runs_cache.get(index) {
                self.session = Some(RunSession::reattach(
                    entry.dir.clone(),
                    entry.record.clone(),
                ));
                self.rail = RailView::New;
            }
        }
        if actions.back_to_design {
            // The run keeps going (SurviveStudio child, truth in files);
            // Studio just stops watching it here. It stays in Runs.
            self.session = None;
            self.draft = Draft::default();
            self.estimate = None;
            self.runs_cache_at = None;
        }
        if actions.refresh_probe {
            self.probe_at = None;
        }
        if actions.save_cds_key {
            match crate::settings::save_cds_key(&self.settings.cds_key_entry) {
                Ok(path) => {
                    self.settings.cds_key_entry.clear();
                    self.status = Some((format!("CDS key saved to {}", path.display()), false));
                }
                Err(error) => {
                    self.status = Some((format!("CDS key not saved: {error}"), true));
                }
            }
        }
        if actions.save_settings {
            match self.settings.save() {
                Ok(()) => {
                    self.contract = self.settings.contract_source();
                    self.probe = None;
                    self.probe_at = None;
                    self.estimate = None;
                    self.estimate_fingerprint = 0;
                    self.status = Some(("settings saved — engine reconnected".into(), false));
                }
                Err(error) => self.status = Some((format!("settings save failed: {error}"), true)),
            }
        }
        if let Some(selection) = actions.select_frame
            && let Some(session) = &mut self.session
        {
            session.selected_frame = selection;
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("top-bar")
            .exact_size(30.0)
            .show_inside(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(
                        egui::RichText::new("ArWen Studio")
                            .strong()
                            .color(theme().text_strong),
                    );
                    ui.label(
                        egui::RichText::new(build_stamp())
                            .color(theme().text_weak)
                            .size(10.0),
                    )
                    .on_hover_text(
                        "This build's git sha and build time — mirrored in \
                         Sys. The Desktop shortcut always launches the newest \
                         build.",
                    );
                    if let Some(newer) = &self.newer_build {
                        ui.colored_label(
                            theme().warn,
                            format!(
                                "newer build exists ({newer}) — close and \
                                 relaunch the Desktop shortcut"
                            ),
                        );
                    }
                    ui.separator();
                    match &self.session {
                        Some(session) => {
                            ui.label(egui::RichText::new(&session.record.name).strong());
                            let (liveness, healthy) = session.liveness();
                            let color = if healthy { theme().live } else { theme().alert };
                            ui.colored_label(color, liveness);
                        }
                        None => {
                            ui.label(
                                egui::RichText::new(&self.draft.name).color(theme().text_weak),
                            );
                            ui.label(egui::RichText::new("designing").color(theme().text_weak));
                            // Continuous re-pricing, visible at the TOP
                            // level; the Resources numbers are dimmed
                            // meanwhile.
                            if self.estimate_is_stale() {
                                ui.add(egui::Spinner::new().size(12.0));
                                ui.colored_label(theme().warn, "re-pricing…").on_hover_text(
                                    "The draft changed; the engine is \
                                         re-estimating VRAM/disk for the current \
                                         shape. Resources numbers stay dimmed \
                                         until it answers.",
                                );
                            }
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Ok(contract) = &self.contract
                            && contract.is_fixture()
                        {
                            ui.colored_label(theme().warn, "FIXTURE MODE")
                                .on_hover_text(
                                    "Engine replies come from the fixtures directory; \
                                     switch contract_mode to \"live\" in settings.json \
                                     when gpuwm run-plan is deployed",
                                );
                        }
                        if let Some(Ok(probe)) = &self.probe
                            && let Some(device) = probe.devices.first()
                        {
                            let free = device
                                .memory_free_bytes
                                .map(crate::kit::format_bytes)
                                .unwrap_or_else(|| "—".into());
                            ui.label(crate::kit::value_text(&format!(
                                "{} · {free} free",
                                device.name
                            )));
                        }
                        if let Some((message, is_error)) = &self.status {
                            let color = if *is_error {
                                theme().alert
                            } else {
                                theme().text_weak
                            };
                            ui.colored_label(color, message);
                        }
                    });
                });
            });
    }

    fn left_rail(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("rail")
            .exact_size(theme::RAIL_W)
            .resizable(false)
            .show_inside(ui, |ui| {
                ui.add_space(6.0);
                for (view, label, hover) in [
                    (RailView::New, "New", "Design a forecast"),
                    (RailView::Runs, "Runs", "Past and running forecasts"),
                    (RailView::System, "Sys", "Engine and GPU status"),
                ] {
                    let selected = self.rail == view;
                    if ui
                        .add_sized(
                            egui::vec2(ui.available_width(), 34.0),
                            egui::Button::selectable(selected, label),
                        )
                        .on_hover_text(hover)
                        .clicked()
                    {
                        self.rail = view;
                        if view == RailView::Runs {
                            self.runs_cache_at = None;
                        }
                    }
                }
            });
    }

    fn inspector_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) -> InspectorActions {
        let mut actions = InspectorActions::default();
        egui::Panel::right("inspector")
            .resizable(true)
            .default_size(theme::INSPECTOR_DEFAULT_WIDTH)
            .size_range(theme::INSPECTOR_MIN_WIDTH..=theme::INSPECTOR_MAX_WIDTH)
            .show_inside(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| match self.rail {
                    RailView::New => match &self.session {
                        Some(session) => inspector::run_ui(ui, session, &mut actions),
                        None => {
                            let pending =
                                self.estimate_slot.in_flight() || self.estimate_settle.is_some();
                            let stale = self.estimate_is_stale();
                            // The displayed tree (same priority as the
                            // map) feeds the per-domain size rows.
                            let advanced_live = self
                                .advanced
                                .as_ref()
                                .filter(|state| !state.tree.is_empty());
                            let (tree, tree_provisional): (
                                &[arwen_plan::queries::ResolvedDomain],
                                bool,
                            ) = if let Some(state) = advanced_live {
                                (state.tree.as_slice(), false)
                            } else if let Some(review) = self.review.as_ref() {
                                (review.tree.as_slice(), false)
                            } else if let Some(fit) = self.fit.as_ref() {
                                (fit.tree.as_slice(), false)
                            } else {
                                (self.placeholder_tree.as_slice(), true)
                            };
                            inspector::design_ui(
                                ui,
                                &mut self.draft,
                                self.probe.as_ref().and_then(|result| result.as_ref().ok()),
                                self.estimate.as_ref(),
                                pending,
                                stale,
                                self.review.as_ref(),
                                &self.settings.favorite_products,
                                self.advanced.as_ref(),
                                tree,
                                tree_provisional,
                                self.map_pane.selected_nest,
                                &mut actions,
                            )
                        }
                    },
                    RailView::Runs => {
                        if self
                            .runs_cache_at
                            .map(|at| at.elapsed() > Duration::from_secs(5))
                            .unwrap_or(true)
                        {
                            self.runs_cache = self.registry.list();
                            self.runs_cache_at = Some(Instant::now());
                        }
                        inspector::runs_ui(ui, &self.runs_cache, &mut actions);
                    }
                    RailView::System => inspector::system_ui(
                        ui,
                        self.probe.as_ref(),
                        &mut self.settings,
                        &mut actions,
                    ),
                });
            });
        if let Some(review) = &self.review {
            let block = self.route_block();
            inspector::review_sheet_ui(ctx, review, block.as_deref(), &mut actions);
        }
        actions
    }

    fn bottom_panel(&mut self, ui: &mut egui::Ui) -> TimelineActions {
        let mut actions = TimelineActions::default();
        egui::Panel::bottom("timeline")
            .exact_size(52.0)
            .show_inside(ui, |ui| {
                timeline::stages_ui(ui, self.session.as_ref());
                timeline::valid_time_ui(ui, self.session.as_ref(), &mut actions);
            });
        actions
    }

    fn central_map(&mut self, ui: &mut egui::Ui) {
        let (clearance_rows, min_span_points) = self.placement_floor();
        let placement_refusal = self.placement_refusal();
        let refusal_domains = self.placement_refusal_domains();
        let search_boxes = self.config_search_boxes();
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme().map_bg))
            .show_inside(ui, |ui| {
                let editable = self.session.is_none() && self.rail == RailView::New;
                // Priority (mirrors displayed_fit; inlined so the borrow
                // checker can split fields): working-config resolve →
                // open review → the continuous fit → draft placeholders.
                let advanced_live = self
                    .advanced
                    .as_ref()
                    .filter(|state| !state.tree.is_empty());
                let (fitted, fitted_tree, tree_provisional) = if let Some(state) = advanced_live {
                    (state.fitted.as_ref(), state.tree.as_slice(), false)
                } else if let Some(review) = self.review.as_ref() {
                    (review.fitted.as_ref(), review.tree.as_slice(), false)
                } else if let Some(fit) = self.fit.as_ref() {
                    (Some(&fit.fitted), fit.tree.as_slice(), false)
                } else {
                    (
                        self.placeholder_root.as_ref(),
                        self.placeholder_tree.as_slice(),
                        true,
                    )
                };
                let mut frame = MapFrame {
                    view: &mut self.view,
                    tiles: &mut self.tile_layer,
                    tile_style: self.tile_style,
                    draft: &mut self.draft,
                    session: self.session.as_ref(),
                    editable,
                    model: Some(&self.model),
                    fitted,
                    fitted_tree,
                    tree_provisional,
                    clearance_rows,
                    min_span_points,
                    placement_refusal,
                    refusal_domains,
                    search_boxes,
                };
                let mut behavior = StudioBehavior {
                    map_pane: &mut self.map_pane,
                    frame: &mut frame,
                };
                self.tree.ui(&mut behavior, ui);
            });
        // Drain the gesture edits AFTER the frame borrows end.
        if !self.map_pane.placement_edits.is_empty() {
            let edits: Vec<_> = self.map_pane.placement_edits.drain(..).collect();
            let ctx = ui.ctx().clone();
            self.handle_placement_edits(&ctx, edits);
        }
    }
}

struct StudioBehavior<'a, 'f> {
    map_pane: &'a mut MapPane,
    frame: &'a mut MapFrame<'f>,
}

impl egui_tiles::Behavior<PaneKind> for StudioBehavior<'_, '_> {
    fn pane_ui(
        &mut self,
        ui: &mut egui::Ui,
        _tile_id: egui_tiles::TileId,
        pane: &mut PaneKind,
    ) -> egui_tiles::UiResponse {
        match pane {
            PaneKind::Map => {
                let rect = self.map_pane.ui(ui, self.frame);
                self.map_pane.overlay_ui(ui, rect, self.frame);
            }
        }
        egui_tiles::UiResponse::None
    }

    fn tab_title_for_pane(&mut self, pane: &PaneKind) -> egui::WidgetText {
        match pane {
            PaneKind::Map => "Map".into(),
        }
    }

    /// One pane = no tab bar; the map owns the whole center.
    fn tab_bar_height(&self, _style: &egui::Style) -> f32 {
        0.0
    }
}

impl StudioApp {
    /// Verification runs: close the window after `seconds` (a smoke run
    /// exits cleanly by itself; nothing external ever kills it).
    pub fn set_smoke_seconds(&mut self, seconds: f64) {
        self.smoke_deadline = Some(Instant::now() + Duration::from_secs_f64(seconds.max(0.5)));
    }

    /// Screenshot smokes: seed a real draft and later nudge it (see
    /// `demo_draft` field docs).
    pub fn set_demo_draft(&mut self) {
        self.demo_draft = true;
    }

    /// Screenshot smokes for the placement wave (see `demo_offcentre`).
    pub fn set_demo_offcentre(&mut self) {
        self.demo_offcentre = true;
    }

    /// Screenshot smokes for the drawn-root wave (see `demo_drawroot`).
    pub fn set_demo_drawroot(&mut self) {
        self.demo_drawroot = true;
    }

    /// `--demo-run`: the drawn-root demo continues into a REAL launch.
    pub fn set_demo_drawroot_run(&mut self) {
        self.demo_drawroot_run = true;
    }

    /// Screenshot smokes for the carry wave (see `demo_carry`).
    pub fn set_demo_carry(&mut self) {
        self.demo_carry = true;
    }

    /// `--demo-carry`: the regression's strand sequence on the live engine —
    /// every step through the production paths (the same writer a drag
    /// lands, the same placement queue a mouse fills).
    fn demo_carry_step(&mut self, ctx: &egui::Context) {
        if !self.demo_carry {
            return;
        }
        if self.draft.domain.is_none() && self.demo_carry_stage == 0 {
            self.view.center_lat = 35.2;
            self.view.center_lon = -97.4;
            self.view.scale = 30.0;
            self.draft.name = format!("carry-{}", chrono::Utc::now().format("%H%M%S"));
            self.draft.source_index = 2; // ERA5 — the nested route
            self.draft.cycle = chrono::NaiveDate::from_ymd_opt(2026, 8, 5)
                .and_then(|date| date.and_hms_opt(0, 0, 0));
            self.draft.root_dx_km = 12.0;
            self.draft.nests = vec![4];
            self.draft.domain = Some(arwen_map::LambertDomain::centered_at(
                35.2, -97.4, 204, 162, 12_000.0,
            ));
        }
        if self.draft.domain.is_some() && self.demo_carry_stage == 0 {
            self.demo_carry_stage = 1;
            self.open_advanced_surface(ctx);
        }
        if self.demo_carry_stage == 1
            && let Some(state) = &self.advanced
            && state.tree.len() == 2
        {
            self.demo_carry_stage = 2;
            let clearance = self.placement_floor().0.unwrap_or(0.0);
            let tree = self.advanced.as_ref().expect("checked").tree.clone();
            let root = tree
                .iter()
                .find(|domain| domain.parent_id == 0)
                .expect("root");
            let nest = tree
                .iter()
                .find(|domain| domain.parent_id != 0)
                .expect("child");
            let ratio = nest.parent_grid_ratio.max(1.0);
            // the regression step 2: d02 to the far east clearance limit.
            let hi_i =
                ((root.nx as f64) - clearance - (nest.nx as f64 - 1.0) / ratio).floor() as i64;
            let j = nest.j_parent_start.round() as i64;
            eprintln!(
                "demo-carry: root {} x {}; d02 {} x {} placed east at i={hi_i}",
                root.nx, root.ny, nest.nx, nest.ny
            );
            self.write_nest_placement(nest.grid_id, hi_i, j, None);
            // the regression step 3: shrink the root by a quarter through the SAME
            // gesture path a west-edge handle drag lands.
            let new_nx = root.nx - root.nx / 4;
            if let Some(fitted) = self.advanced.as_ref().and_then(|state| state.fitted) {
                let mut sketch = fitted;
                sketch.nx = new_nx;
                self.draft.domain = Some(sketch);
            }
            eprintln!("demo-carry: shrinking root {} -> {new_nx}", root.nx);
            self.map_pane
                .placement_edits
                .push(crate::map_pane::PlacementEdit::RootAdjusted {
                    nx: new_nx,
                    ny: root.ny,
                });
            self.map_pane.selected_nest = Some(nest.grid_id);
        }
        if self.demo_carry_stage == 2
            && let Some(state) = &self.advanced
            && !state.repairs.is_empty()
        {
            self.demo_carry_stage = 3;
            let root_nx = state
                .model
                .root_domain_index()
                .and_then(|index| state.model.domain_value(index, "nx"))
                .unwrap_or("?")
                .trim()
                .to_string();
            let d02_i = state
                .model
                .domain_index_for_grid(2)
                .and_then(|index| state.model.domain_value(index, "i_parent_start"))
                .unwrap_or("?")
                .trim()
                .to_string();
            eprintln!("demo-carry: config now root nx={root_nx}, d02 i={d02_i}");
            for repair in &state.repairs {
                eprintln!("demo-carry: notice: {}", repair.notice);
            }
        }
    }

    /// `--demo-drawroot`: the exact code a mouse drag runs — a wide 2:1
    /// rectangle via `domain_from_drag`, then the SAME placement queue
    /// the drag-stop fills. Everything downstream (auto-generate, the
    /// manual root write, the debounced resolve, the re-pricing) is the
    /// production path on the configured (live) engine.
    fn demo_drawroot_step(&mut self, ctx: &egui::Context) {
        if !self.demo_drawroot {
            return;
        }
        // `--demo-run` continuation: review + LAUNCH once the surface
        // resolved and the estimate priced the drawn shape — the real
        // fetch?prepare?forecast chain, through the same actions the
        // buttons emit.
        if self.demo_drawroot_applied {
            if self.demo_drawroot_run && !self.demo_drawroot_launched && self.session.is_none() {
                if self.review.is_some() {
                    self.demo_drawroot_launched = true;
                    eprintln!("demo-run: launching the reviewed plan");
                    let actions = crate::inspector::InspectorActions {
                        launch: true,
                        ..Default::default()
                    };
                    self.handle_actions(ctx, actions);
                } else if self
                    .advanced
                    .as_ref()
                    .is_some_and(|state| matches!(state.resolve, Some(Ok(_))))
                    && matches!(self.estimate, Some(Ok(_)))
                    && !self.estimate_is_stale()
                {
                    let actions = crate::inspector::InspectorActions {
                        open_review: true,
                        ..Default::default()
                    };
                    self.handle_actions(ctx, actions);
                }
            }
            return;
        }
        self.demo_drawroot_applied = true;
        if self.demo_drawroot_run {
            // Short window: the sign-off is the CHAIN (fetch ? prepare ?
            // forecast), not a long forecast.
            self.draft.length_hours = 1.0;
        }
        self.view.center_lat = 35.35;
        self.view.center_lon = -97.4;
        self.view.scale = 56.0;
        self.draft.name = format!("drawroot-{}", chrono::Utc::now().format("%H%M%S"));
        // ~600 km × ~300 km at 3 km — a deliberate 2:1 the fitter would
        // never volunteer (its own answer is ~398 × 320).
        let domain =
            crate::map_pane::domain_from_drag((-100.7, 34.0), (-94.1, 36.7), self.draft.dx_m())
                .expect("demo rectangle is a valid domain");
        let (nx, ny) = (domain.nx, domain.ny);
        eprintln!("demo-drawroot: drew {nx} x {ny} @ {} m", domain.dx_m);
        self.draft.domain = Some(domain);
        self.map_pane
            .placement_edits
            .push(crate::map_pane::PlacementEdit::RootDrawn { nx, ny });
    }

    /// `--demo-offcentre`: the real pipeline end to end — ERA5 12-3-1
    /// intent, engine writes the config (dry run), first resolve fits
    /// the tree, then BOTH inner domains move far off-centre through
    /// the same writer a map drop uses, and the debounced resolve
    /// paints the engine's own answer.
    fn demo_offcentre_step(&mut self, ctx: &egui::Context) {
        if !self.demo_offcentre {
            return;
        }
        if self.draft.domain.is_none() && !self.demo_offcentre_applied {
            self.view.center_lat = 35.2;
            self.view.center_lon = -97.4;
            self.view.scale = 34.0;
            self.draft.name = "offcentre-12-3-1".into();
            self.draft.source_index = 2; // ERA5 — the nested route
            self.draft.cycle = chrono::NaiveDate::from_ymd_opt(2026, 8, 5)
                .and_then(|date| date.and_hms_opt(0, 0, 0));
            self.draft.root_dx_km = 12.0;
            self.draft.nests = vec![4, 3];
            self.draft.domain = Some(arwen_map::LambertDomain::centered_at(
                35.2, -97.4, 170, 136, 12_000.0,
            ));
        }
        if self.draft.domain.is_some() && !self.demo_offcentre_requested {
            self.demo_offcentre_requested = true;
            self.open_advanced_surface(ctx);
        }
        if !self.demo_offcentre_applied
            && let Some(state) = &self.advanced
            && state.tree.len() == 3
        {
            self.demo_offcentre_applied = true;
            let clearance = self.placement_floor().0.unwrap_or(0.0);
            // Targets from the ENGINE's own fitted tree: d02 far SW,
            // d03 far NE — each one cell inside the clamp bounds.
            let tree = self.advanced.as_ref().expect("checked").tree.clone();
            let mut moves: Vec<(u32, i64, i64)> = Vec::new();
            for nest in tree.iter().filter(|nest| nest.parent_id != 0) {
                let Some(parent) = tree.iter().find(|domain| domain.grid_id == nest.parent_id)
                else {
                    continue;
                };
                let lo = (1.0 + clearance).ceil() as i64 + 1;
                let ratio = nest.parent_grid_ratio.max(1.0);
                let hi_i = ((parent.nx as f64) - clearance - (nest.nx as f64 - 1.0) / ratio).floor()
                    as i64
                    - 1;
                let hi_j = ((parent.ny as f64) - clearance - (nest.ny as f64 - 1.0) / ratio).floor()
                    as i64
                    - 1;
                if nest.grid_id == 2 {
                    moves.push((nest.grid_id, lo, lo));
                } else {
                    moves.push((nest.grid_id, hi_i.max(lo), hi_j.max(lo)));
                }
            }
            for (grid_id, i, j) in moves {
                self.write_nest_placement(grid_id, i, j, None);
            }
            // The map is the evidence: editor window out of the way,
            // the deepest nest selected so its handles show.
            if let Some(state) = &mut self.advanced {
                state.open = false;
            }
            self.map_pane.selected_nest = Some(3);
        }
    }

    fn demo_draft_step(&mut self, ctx: &egui::Context) {
        if !self.demo_draft {
            return;
        }
        if self.draft.domain.is_none() && !self.demo_nudged {
            self.view.center_lat = 35.2;
            self.view.center_lon = -97.4;
            self.view.scale = 18.0;
            self.draft.domain = Some(arwen_map::LambertDomain::centered_at(
                35.2, -97.4, 300, 240, 3_000.0,
            ));
            self.draft.cycle = chrono::NaiveDate::from_ymd_opt(2026, 8, 6)
                .and_then(|date| date.and_hms_opt(12, 0, 0));
            self.draft.nests = vec![3];
        }
        // Once the draft exists: open the Advanced surface (real dry-run
        // generation) so the storm cards light up, then write the follow
        // + spawn tables through the REAL card writers — the engine's
        // verdict on them lands in the editor banner.
        if self.draft.domain.is_some() && !self.demo_advanced_requested {
            self.demo_advanced_requested = true;
            self.open_advanced_surface(ctx);
        }
        if self.advanced.is_some() && !self.demo_storm_applied {
            self.demo_storm_applied = true;
            self.apply_config_rewrite(|text| {
                crate::storm::write_relocation(text, Some(&crate::storm::FollowSettings::default()))
            });
            self.apply_config_rewrite(|text| {
                crate::storm::write_spawn(text, 1, Some(&crate::storm::SpawnSettings::default()))
            });
        }
        // Shortly before the deadline: a real domain nudge, so the
        // second (live) estimate is mid-flight when the capture fires
        // and the strip shows the genuine stale state.
        if !self.demo_nudged
            && let Some(deadline) = self.smoke_deadline
            && deadline.saturating_duration_since(Instant::now()) < Duration::from_millis(3_500)
        {
            self.demo_nudged = true;
            if let Some(domain) = &mut self.draft.domain {
                domain.ref_lat += 0.5;
                domain.nx += 40;
            }
        }
    }

    /// The whole frame, headless-testable (no `eframe::Frame` needed).
    pub fn ui_impl(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        if let Some(deadline) = self.smoke_deadline {
            if Instant::now() >= deadline {
                // Evidence lines for verification runs.
                eprintln!(
                    "smoke: contract={} probe={} estimate={} session={}",
                    match &self.contract {
                        Ok(contract) if contract.is_fixture() => "fixture".to_string(),
                        Ok(_) => "live".to_string(),
                        Err(error) => format!("error({error})"),
                    },
                    match &self.probe {
                        Some(Ok(probe)) => format!(
                            "ok({} devices, routes {:?})",
                            probe.devices.len(),
                            probe.routes.keys().collect::<Vec<_>>()
                        ),
                        Some(Err(error)) => format!("err({error})"),
                        None => "pending".into(),
                    },
                    match &self.estimate {
                        Some(Ok(estimate)) => format!(
                            "ok({:.2} GiB)",
                            estimate.vram.estimate_gib.unwrap_or_default()
                        ),
                        Some(Err(error)) => format!("err({error})"),
                        None => "none".into(),
                    },
                    match &self.session {
                        Some(session) => format!(
                            "{} ({} frames)",
                            session.liveness().0,
                            session.outputs.len()
                        ),
                        None => "none".into(),
                    },
                );
                // Screenshot before close (packaging evidence). NEVER
                // early-return here: the frame must still paint, or the
                // capture is a blank canvas (found live). 2 s fallback so
                // a backend without capture still exits.
                match self.smoke_screenshot_requested {
                    None => {
                        self.smoke_screenshot_requested = Some(Instant::now());
                        ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(
                            egui::UserData::default(),
                        ));
                        ctx.request_repaint();
                    }
                    Some(requested) => {
                        let image = ctx.input(|input| {
                            input.events.iter().find_map(|event| match event {
                                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                                _ => None,
                            })
                        });
                        if let Some(image) = image {
                            let path = std::env::temp_dir().join(format!(
                                "arwen-smoke-{}.png",
                                chrono::Utc::now().format("%H%M%S")
                            ));
                            match crate::livefire::save_screenshot(&image, &path) {
                                Ok(()) => eprintln!("smoke: screenshot {}", path.display()),
                                Err(error) => eprintln!("smoke: screenshot failed: {error}"),
                            }
                            eprintln!("smoke: clean self-close after deadline");
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        } else if requested.elapsed() >= Duration::from_secs(2) {
                            eprintln!("smoke: no screenshot arrived; clean self-close");
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        } else {
                            ctx.request_repaint();
                        }
                    }
                }
            } else {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
        }
        self.demo_draft_step(&ctx);
        self.demo_offcentre_step(&ctx);
        self.demo_drawroot_step(&ctx);
        self.demo_carry_step(&ctx);
        self.follow_intent_cards(&ctx);
        self.poll_workers(&ctx);
        // Placeholders refresh before ANY panel reads the displayed tree
        // (the inspector's size rows and the map share it this frame).
        self.refresh_placeholders();
        self.top_bar(ui);
        if self.embedded
            && let Some(reason) = self.host_launch_block.as_deref()
        {
            egui::Panel::top("bowecho-arwen-host-policy").show_inside(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(theme().warn, "Preview integration");
                    ui.weak(reason);
                });
            });
        }
        self.left_rail(ui);
        let mut actions = self.inspector_panel(ui, &ctx);
        let timeline_actions = self.bottom_panel(ui);
        if actions.select_frame.is_none() {
            actions.select_frame = timeline_actions.select_frame;
        }
        self.central_map(ui);
        if self.advanced.as_ref().is_some_and(|state| state.open) {
            let mut advanced_actions = crate::advanced::AdvancedActions::default();
            if let Some(state) = &mut self.advanced {
                crate::advanced::window_ui(&ctx, state, &mut advanced_actions);
            }
            if advanced_actions.discard {
                self.advanced = None;
                self.draft.custom = None;
                self.status = Some(("custom config discarded — intent plan active".into(), false));
            } else if advanced_actions.regenerate {
                self.advanced = None;
                self.draft.custom = None;
                self.open_advanced_surface(&ctx);
            }
        }
        if self.products.open {
            let mut product_actions = crate::products::ProductsActions::default();
            crate::products::window_ui(
                &ctx,
                &mut self.products,
                &mut self.draft,
                &mut self.settings.favorite_products,
                &mut product_actions,
            );
            if product_actions.favorites_changed
                && let Err(error) = self.settings.save()
            {
                self.status = Some((format!("save favorites: {error}"), true));
            }
        }
        if self.livefire.is_some() {
            self.livefire_step(&ctx, &mut actions);
        }
        self.handle_actions(&ctx, actions);
    }
}

impl eframe::App for StudioApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.ui_impl(ui);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test apps NEVER see the box's real registry: a live run on it
    /// reattaches a session and freezes the whole design flow (found
    /// live — the regression's active run turned every gesture test red). Same
    /// isolation the matrix walks use.
    fn test_app(ctx: &egui::Context, settings: crate::settings::StudioSettings) -> StudioApp {
        let mut app = StudioApp::with_settings(ctx, settings);
        app.redirect_registry(
            std::env::temp_dir().join(format!("arwen-test-registry-{}", std::process::id())),
        );
        app
    }

    /// The shell lays out headlessly: theme installs, panels + dock tree +
    /// map canvas render, the probe worker round-trips through the
    /// fixture contract, and the draft inspector produces an estimate
    /// request once a domain exists — several full frames, no panics.
    #[test]
    fn shell_renders_frames_headlessly_and_probe_lands() {
        let ctx = egui::Context::default();
        // Explicit fixture settings: the suite must never depend on this
        // box's settings.json (which may be switched to live).
        let mut app = test_app(&ctx, crate::settings::StudioSettings::default());
        assert!(app.contract.is_ok(), "{:?}", app.contract.as_ref().err());

        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1440.0, 900.0),
            )),
            ..Default::default()
        };
        // Give the domain a value so the estimate path arms too.
        app.draft.domain = Some(arwen_map::LambertDomain::centered_at(
            35.5, -97.5, 300, 240, 3_000.0,
        ));

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut probe_seen = false;
        let mut estimate_seen = false;
        while Instant::now() < deadline && !(probe_seen && estimate_seen) {
            let _ = ctx.run_ui(input(), |ui| app.ui_impl(ui));
            probe_seen |= matches!(app.probe, Some(Ok(_)));
            estimate_seen |= matches!(app.estimate, Some(Ok(_)));
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(probe_seen, "fixture probe never landed");
        assert!(estimate_seen, "fixture estimate never landed");

        // Rail switches render every inspector view without panicking.
        for rail in [RailView::Runs, RailView::System, RailView::New] {
            app.rail = rail;
            let _ = ctx.run_ui(input(), |ui| app.ui_impl(ui));
        }
    }

    /// CONTINUOUS RE-PRICING regression: dragging the domain
    /// twice yields two estimate invocations whose SUBMITTED PLANS carry
    /// the two different domains, with a visible stale state in between
    /// — and the pipeline advances on repaint ticks alone, no synthetic
    /// user input after the change.
    #[test]
    fn estimate_reprices_on_every_domain_change_with_visible_staleness() {
        let ctx = egui::Context::default();
        let mut app = test_app(&ctx, crate::settings::StudioSettings::default());
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1440.0, 900.0),
            )),
            ..Default::default()
        };
        let pump =
            |app: &mut StudioApp, ctx: &egui::Context, until: &dyn Fn(&StudioApp) -> bool| {
                let deadline = Instant::now() + Duration::from_secs(10);
                while Instant::now() < deadline && !until(app) {
                    let _ = ctx.run_ui(input(), |ui| app.ui_impl(ui));
                    std::thread::sleep(Duration::from_millis(25));
                }
            };

        // Draft one domain; the first estimate lands.
        app.draft.domain = Some(arwen_map::LambertDomain::centered_at(
            35.5, -97.5, 300, 240, 3_000.0,
        ));
        pump(&mut app, &ctx, &|app| {
            app.estimate_runs == 1
                && matches!(app.estimate, Some(Ok(_)))
                && !app.estimate_is_stale()
        });
        assert_eq!(app.estimate_runs, 1, "first estimate ran");
        let plan_one = app.last_estimate_plan.clone().expect("plan recorded");
        assert!(plan_one.contains("35.5000,-97.5000"), "{plan_one}");
        assert!(!app.estimate_is_stale(), "fresh estimate is not stale");

        // Drag the domain (resize + move): the shown numbers must go
        // STALE immediately, then a SECOND estimate must run for the
        // new geometry.
        if let Some(domain) = &mut app.draft.domain {
            domain.ref_lat = 38.25;
            domain.ref_lon = -101.0;
            domain.nx = 420;
        }
        let _ = ctx.run_ui(input(), |ui| app.ui_impl(ui));
        assert!(
            app.estimate_is_stale(),
            "estimate must read stale the moment the draft moves"
        );
        pump(&mut app, &ctx, &|app| {
            app.estimate_runs == 2 && !app.estimate_is_stale()
        });
        assert_eq!(app.estimate_runs, 2, "second estimate ran for the drag");
        let plan_two = app.last_estimate_plan.clone().unwrap();
        assert!(plan_two.contains("38.2500,-101.0000"), "{plan_two}");
        assert_ne!(
            plan_one, plan_two,
            "different draft → different submitted plan"
        );

        // A third change — output cadence this time — re-prices again.
        app.draft.history_interval_s = 313;
        let _ = ctx.run_ui(input(), |ui| app.ui_impl(ui));
        assert!(app.estimate_is_stale());
        pump(&mut app, &ctx, &|app| {
            app.estimate_runs == 3 && !app.estimate_is_stale()
        });
        assert_eq!(app.estimate_runs, 3, "cadence change re-priced");
        assert!(
            app.last_estimate_plan
                .as_ref()
                .unwrap()
                .contains("\"history_interval_s\": 313")
        );
    }

    /// The Advanced surface end to end on the fixture contract: the
    /// (fixture) engine writes the config into the drafts workspace,
    /// the editor model exposes real keys, the plan flips to
    /// config.path, and an edit reaches the fingerprint through the
    /// debounce (so estimate/resolve re-query).
    #[test]
    fn advanced_surface_generates_and_rides_config_path() {
        let ctx = egui::Context::default();
        let temp = std::env::temp_dir().join(format!(
            "arwen-adv-test-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let mut settings = crate::settings::StudioSettings::default();
        settings.output_root = temp.join("forecasts").to_string_lossy().into_owned();
        let mut app = test_app(&ctx, settings);
        // ERA5 pinned 12-3: the nested route — the chain makes the
        // fixture emission carry the [[domain]] tables asserted on.
        app.draft.source_index = 2;
        app.draft.nests = vec![4];
        app.draft.cycle =
            chrono::NaiveDate::from_ymd_opt(2026, 8, 5).and_then(|date| date.and_hms_opt(0, 0, 0));
        app.draft.domain = Some(arwen_map::LambertDomain::centered_at(
            35.5, -97.5, 300, 240, 3_000.0,
        ));
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1440.0, 900.0),
            )),
            ..Default::default()
        };

        app.open_advanced_surface(&ctx);
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && app.advanced.is_none() {
            let _ = ctx.run_ui(input(), |ui| app.ui_impl(ui));
            std::thread::sleep(Duration::from_millis(20));
        }
        let state = app
            .advanced
            .as_ref()
            .expect("fixture engine wrote the config");
        assert!(
            state
                .model
                .entries
                .iter()
                .any(|entry| entry.key == "damp_opt"),
            "the engine's emission parsed into editable knobs"
        );
        assert!(
            state
                .model
                .entries
                .iter()
                .any(|entry| entry.table == "domain[1]"),
            "nested [[domain]] entries present"
        );
        let custom = app
            .draft
            .custom
            .as_ref()
            .expect("draft rides the edited file");
        assert!(custom.config_path.ends_with("draft-config.toml"));
        let plan = app.draft.to_plan("X").unwrap();
        match &plan.config {
            arwen_plan::plan::PlanConfig::Path(path) => {
                assert!(path.ends_with("draft-config.toml"), "{path}");
                assert!(std::path::Path::new(path).is_file(), "edited file on disk");
            }
            other => panic!("expected config.path, got {other:?}"),
        }

        // An edit reaches the fingerprint once the debounce consumes it.
        let before = app.draft.fingerprint("r");
        if let Some(state) = &mut app.advanced {
            let index = state
                .model
                .entries
                .iter()
                .position(|entry| entry.key == "damp_opt")
                .unwrap();
            state.model = state.model.with_edit(index, "0");
            state.rev += 1;
            state.dirty_at = Some(Instant::now() - Duration::from_millis(800));
        }
        let _ = ctx.run_ui(input(), |ui| app.ui_impl(ui));
        assert_ne!(before, app.draft.fingerprint("r"), "edit reached the plan");
        let text =
            std::fs::read_to_string(&app.draft.custom.as_ref().unwrap().config_path).unwrap();
        assert!(text.contains("damp_opt = 0"), "edited bytes persisted");
        let _ = std::fs::remove_dir_all(&temp);
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

    fn press(position: egui::Pos2) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(position),
            egui::Event::PointerButton {
                pos: position,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            },
        ]
    }

    fn release(position: egui::Pos2) -> Vec<egui::Event> {
        vec![egui::Event::PointerButton {
            pos: position,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::default(),
        }]
    }

    /// Fixture settings whose output root lives under a fresh temp dir,
    /// so gesture-initiated config generation never writes outside it.
    fn temp_settings(tag: &str) -> (crate::settings::StudioSettings, std::path::PathBuf) {
        let temp = std::env::temp_dir().join(format!(
            "arwen-{tag}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let mut settings = crate::settings::StudioSettings::default();
        settings.output_root = temp.join("forecasts").to_string_lossy().into_owned();
        (settings, temp)
    }

    fn pump_until(
        app: &mut StudioApp,
        ctx: &egui::Context,
        what: &str,
        until: &dyn Fn(&StudioApp) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && !until(app) {
            let _ = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1440.0, 900.0),
                    )),
                    ..Default::default()
                },
                |ui| app.ui_impl(ui),
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(until(app), "timed out waiting for {what}");
    }

    /// Acceptance step 4 via SYNTHESIZED POINTER INPUT through the real
    /// widget pipeline: a primary-button drag across the map canvas in
    /// draw mode creates the Lambert domain, exactly as a mouse would —
    /// and the root-shape persistence regression: the
    /// drawn size auto-generates the config surface and lands there as
    /// the MANUAL root nx/ny. The fitter's own square answer dies here.
    #[test]
    fn synthesized_drag_on_the_map_draws_a_domain() {
        let ctx = egui::Context::default();
        let (settings, temp) = temp_settings("draw");
        let mut app = test_app(&ctx, settings);
        let _ = ctx.run_ui(raw(Vec::new()), |ui| app.ui_impl(ui));

        app.map_pane.draw_mode = true;
        // A deliberately WIDE drag — the aspect the VRAM fitter would
        // never volunteer.
        let start = egui::pos2(300.0, 380.0);
        let end = egui::pos2(1000.0, 580.0);
        let (start_lon, start_lat) = {
            // Geo anchor recorded through the app's own camera; the rect
            // interior is central-canvas territory at this window size.
            let rect = egui::Rect::from_min_max(
                egui::pos2(theme::RAIL_W, 30.0),
                egui::pos2(1440.0 - theme::INSPECTOR_DEFAULT_WIDTH, 900.0 - 52.0),
            );
            app.view.screen_to_lon_lat(rect, start)
        };

        let _ = ctx.run_ui(raw(press(start)), |ui| app.ui_impl(ui));
        // Drag through a midpoint, then to the far corner.
        let _ = ctx.run_ui(
            raw(vec![egui::Event::PointerMoved(egui::pos2(600.0, 450.0))]),
            |ui| app.ui_impl(ui),
        );
        let _ = ctx.run_ui(raw(vec![egui::Event::PointerMoved(end)]), |ui| {
            app.ui_impl(ui)
        });
        let _ = ctx.run_ui(raw(release(end)), |ui| app.ui_impl(ui));

        let domain = app.draft.domain.expect("drag created a domain");
        assert!(!app.map_pane.draw_mode, "draw mode exits after the drag");
        domain.validate().expect("drawn domain is valid");
        // The domain sits where the drag happened: its center is inside
        // the dragged span (loose geo bounds — projection-true).
        assert!(domain.ref_lon > start_lon as f64 - 1.0);
        assert!(domain.ref_lat < start_lat as f64 + 1.0);
        assert!(domain.width_km() > 100.0 && domain.height_km() > 100.0);
        assert!(
            domain.nx as f64 >= domain.ny as f64 * 2.0,
            "the drag was deliberately wide: {} x {}",
            domain.nx,
            domain.ny
        );

        // THE DRAWN RECTANGLE IS THE DOMAIN: the (fixture) engine writes
        // the config surface and the drawn size lands in it as MANUAL
        // root nx/ny — the plan now rides config.path, so the estimate's
        // verdict is on the user's shape.
        pump_until(&mut app, &ctx, "config surface", &|app| {
            app.advanced.is_some()
        });
        let state = app.advanced.as_ref().unwrap();
        assert!(!state.open, "a drag must not bury the map under the editor");
        let (nx_str, ny_str) = (domain.nx.to_string(), domain.ny.to_string());
        assert_eq!(
            state.model.domain_value(0, "nx").map(str::trim),
            Some(nx_str.as_str()),
            "config: {}",
            state.model.text
        );
        assert_eq!(
            state.model.domain_value(0, "ny").map(str::trim),
            Some(ny_str.as_str())
        );
        assert!(
            crate::advanced::placement_is_manual(&state.model, &state.base_model, 0),
            "the drawn root reads MANUAL (the emission's own fit stays the base)"
        );
        assert!(state.dirty_at.is_some(), "engine re-validation armed");
        assert!(app.draft.custom.is_some(), "the plan rides config.path");
        let _ = std::fs::remove_dir_all(&temp);
    }

    /// A 12-3-1 working config + its fitted tree, installed as the
    /// Advanced surface would hold them after generation + resolve —
    /// the engine's shapes (placement keys verbatim, floor data as the
    /// resolve report carries it), Studio-authored values.
    const TREE_121_TOML: &str = "[experiment]\nname = \"offcentre\"\nrun_seconds = 21600.0\n\n\
[projection]\nmap_proj = \"lambert\"\nref_lat = 35.5\nref_lon = -97.5\ntruelat1 = 25.5\n\
truelat2 = 45.5\nstand_lon = -97.5\n\n\
[[domain]]\ngrid_id = 1\nparent_id = 0\ni_parent_start = 1\nj_parent_start = 1\n\
parent_grid_ratio = 1\nnx = 204\nny = 162\ndx = 12000.0\n\n\
[[domain]]\ngrid_id = 2\nparent_id = 1\ni_parent_start = 52\nj_parent_start = 42\n\
parent_grid_ratio = 4\nnx = 408\nny = 320\n\n\
[[domain]]\ngrid_id = 3\nparent_id = 2\ni_parent_start = 150\nj_parent_start = 120\n\
parent_grid_ratio = 3\nnx = 240\nny = 180\n\
spawn = { trigger = \"uh\", threshold = 40.0, earliest_s = 3600.0, latest_s = 21600.0 }\n";

    #[allow(clippy::too_many_arguments)]
    fn resolved(
        grid_id: u32,
        parent_id: u32,
        i: f64,
        j: f64,
        ratio: f64,
        nx: u32,
        ny: u32,
        dx: f64,
    ) -> arwen_plan::queries::ResolvedDomain {
        arwen_plan::queries::ResolvedDomain {
            grid_id,
            parent_id,
            i_parent_start: i,
            j_parent_start: j,
            parent_grid_ratio: ratio,
            nx,
            ny,
            dx_m: dx,
            history_interval_s: None,
            dt_s: None,
            spawn_trigger: (grid_id == 3).then(|| "uh".to_string()),
            spawn_threshold: (grid_id == 3).then_some(40.0),
            spawn_at_s: None,
        }
    }

    /// Install the 12-3-1 state on the app: working TOML on disk, custom
    /// plan riding it, fitted root + tree + engine floor data on the
    /// Advanced surface.
    fn install_121_state(app: &mut StudioApp) -> std::path::PathBuf {
        let temp = std::env::temp_dir().join(format!(
            "arwen-nest-drag-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(&temp).unwrap();
        let config_path = temp.join("draft-config.toml");
        std::fs::write(&config_path, TREE_121_TOML).unwrap();
        let mut state = crate::advanced::AdvancedState::new(
            temp.clone(),
            config_path.clone(),
            "experiment".into(),
            TREE_121_TOML.to_string(),
        );
        state.dirty_at = None; // gesture tests drive the debounce explicitly
        state.open = false; // the editor window must not swallow map input
        let report: arwen_plan::queries::ResolveReport =
            serde_json::from_value(serde_json::json!({
                "schema": "gpuwm.run-plan.resolved.v1",
                "plan": {},
                "configuration": {},
                "domain_size_floor": {
                    "clearance_rows": 10,
                    "nest_span_mass_points": 12,
                    "basis": "test floor data in the engine's reported shape"
                },
                "automatic_resolutions": [],
                "warnings": []
            }))
            .unwrap();
        state.resolve = Some(Ok(Box::new(report)));
        state.fitted = Some(arwen_map::LambertDomain::centered_at(
            35.5, -97.5, 204, 162, 12_000.0,
        ));
        state.tree = vec![
            resolved(1, 0, 1.0, 1.0, 1.0, 204, 162, 12_000.0),
            resolved(2, 1, 52.0, 42.0, 4.0, 408, 320, 3_000.0),
            resolved(3, 2, 150.0, 120.0, 3.0, 240, 180, 1_000.0),
        ];
        app.advanced = Some(state);
        app.draft.custom = Some(crate::draft::CustomPlanConfig {
            config_path: config_path.to_string_lossy().into_owned(),
            route: "experiment".into(),
            source: "era5".into(),
            root_dx_km: 12.0,
            nests: Vec::new(),
            rev: 0,
        });
        // The surface follows the intent cards — keep them in agreement
        // so these gesture tests exercise placement, not regeneration.
        app.draft.source_index = 2;
        app.draft.root_dx_km = 12.0;
        app.draft.cycle =
            chrono::NaiveDate::from_ymd_opt(2026, 8, 5).and_then(|date| date.and_hms_opt(0, 0, 0));
        app.draft.domain = Some(arwen_map::LambertDomain::centered_at(
            35.5, -97.5, 204, 162, 12_000.0,
        ));
        app.view.center_lat = 35.5;
        app.view.center_lon = -97.5;
        app.view.scale = 30.0;
        temp
    }

    /// Screen position of a point given in the PARENT-grid coordinates of
    /// nest `grid_id`, through the app's real camera and canvas rect.
    fn parent_cells_to_screen(app: &StudioApp, grid_id: u32, x: f64, y: f64) -> egui::Pos2 {
        let rect = app.map_pane.last_canvas_rect.expect("canvas laid out");
        let state = app.advanced.as_ref().unwrap();
        let root = state.fitted.as_ref().unwrap();
        // Build the parent chain root→parent of grid_id from the tree.
        let mut chain: Vec<arwen_map::NestPlacement> = Vec::new();
        let mut current = state.tree.iter().find(|n| n.grid_id == grid_id).unwrap();
        loop {
            let parent = state
                .tree
                .iter()
                .find(|n| n.grid_id == current.parent_id)
                .unwrap();
            if parent.parent_id == 0 {
                break;
            }
            chain.push(arwen_map::NestPlacement {
                i_parent_start: parent.i_parent_start,
                j_parent_start: parent.j_parent_start,
                parent_grid_ratio: parent.parent_grid_ratio,
                nx: parent.nx,
                ny: parent.ny,
            });
            current = parent;
        }
        chain.reverse();
        let (gx, gy) = arwen_map::path_grid_to_root(&chain, x, y);
        let (lat, lon) = root.latlon_at_grid(gx, gy);
        app.view.lon_lat_to_screen(rect, lon as f32, lat as f32)
    }

    fn drag_pointer(app: &mut StudioApp, ctx: &egui::Context, from: egui::Pos2, to: egui::Pos2) {
        let _ = ctx.run_ui(raw(press(from)), |ui| app.ui_impl(ui));
        let mid = egui::pos2((from.x + to.x) / 2.0, (from.y + to.y) / 2.0);
        let _ = ctx.run_ui(raw(vec![egui::Event::PointerMoved(mid)]), |ui| {
            app.ui_impl(ui)
        });
        let _ = ctx.run_ui(raw(vec![egui::Event::PointerMoved(to)]), |ui| {
            app.ui_impl(ui)
        });
        let _ = ctx.run_ui(raw(release(to)), |ui| app.ui_impl(ui));
    }

    /// EVERY DOMAIN DIRECTLY PLACEABLE: dragging child 2 of a 12-3-1
    /// (d03) far off-centre through synthesized pointer input writes the
    /// new whole-cell i/j_parent_start into the working config — the one
    /// source of truth the submitted plan rides — and flips the
    /// placement to manual with the debounce armed for the engine.
    #[test]
    fn synthesized_drag_moves_the_inner_nest_of_a_12_3_1_off_centre() {
        let ctx = egui::Context::default();
        let mut app = test_app(&ctx, crate::settings::StudioSettings::default());
        let temp = install_121_state(&mut app);
        let _ = ctx.run_ui(raw(Vec::new()), |ui| app.ui_impl(ui));

        // Press at d03's center; drop so its anchor lands at (20, 40) in
        // d02 cells — far off-centre (fitted was (150, 120)).
        let press_at = parent_cells_to_screen(&app, 3, 150.0 + 119.5 / 3.0, 120.0 + 89.5 / 3.0);
        let target = parent_cells_to_screen(
            &app,
            3,
            150.0 + 119.5 / 3.0 - 130.0,
            120.0 + 89.5 / 3.0 - 80.0,
        );
        drag_pointer(&mut app, &ctx, press_at, target);

        assert_eq!(app.map_pane.selected_nest, Some(3), "drag selected d03");
        let state = app.advanced.as_ref().unwrap();
        assert_eq!(
            state.model.domain_value(2, "i_parent_start").map(str::trim),
            Some("20"),
            "config: {}",
            state.model.text
        );
        assert_eq!(
            state.model.domain_value(2, "j_parent_start").map(str::trim),
            Some("40")
        );
        // The OTHER domains kept their engine-fitted placement.
        assert_eq!(
            state.model.domain_value(1, "i_parent_start").map(str::trim),
            Some("52")
        );
        assert!(crate::advanced::placement_is_manual(
            &state.model,
            &state.base_model,
            2
        ));
        assert!(state.dirty_at.is_some(), "debounced resolve armed");
        assert_eq!(state.rev, 1);

        // Let the debounce consume: the SUBMITTED config file (the plan
        // rides config.path) carries the dragged anchor.
        if let Some(state) = &mut app.advanced {
            state.dirty_at = Some(Instant::now() - Duration::from_millis(900));
        }
        let _ = ctx.run_ui(raw(Vec::new()), |ui| app.ui_impl(ui));
        let submitted =
            std::fs::read_to_string(&app.draft.custom.as_ref().unwrap().config_path).unwrap();
        assert!(submitted.contains("i_parent_start = 20"), "{submitted}");
        assert!(submitted.contains("j_parent_start = 40"));
        match &app.draft.to_plan("X").unwrap().config {
            arwen_plan::plan::PlanConfig::Path(path) => {
                assert!(path.ends_with("draft-config.toml"), "{path}");
            }
            other => panic!("expected config.path, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&temp);
    }

    /// Drag INTO the keepout: the drop clamps at the engine-reported
    /// clearance (never past it), and an engine refusal of the working
    /// config surfaces as the map's inline sentence.
    #[test]
    fn nest_drag_into_the_keepout_clamps_at_the_engine_clearance() {
        let ctx = egui::Context::default();
        let mut app = test_app(&ctx, crate::settings::StudioSettings::default());
        let temp = install_121_state(&mut app);
        let _ = ctx.run_ui(raw(Vec::new()), |ui| app.ui_impl(ui));

        // Press inside d02 but OUTSIDE d03 (root cells): d02 own (60, 60).
        let press_at = parent_cells_to_screen(&app, 2, 52.0 + 59.0 / 4.0, 42.0 + 59.0 / 4.0);
        // Aim the anchor at (-60, -60) — far outside the parent.
        let target = parent_cells_to_screen(
            &app,
            2,
            52.0 + 59.0 / 4.0 - 112.0,
            42.0 + 59.0 / 4.0 - 102.0,
        );
        drag_pointer(&mut app, &ctx, press_at, target);

        let state = app.advanced.as_ref().unwrap();
        // clearance_rows = 10 → the smallest legal anchor is 11.
        assert_eq!(
            state.model.domain_value(1, "i_parent_start").map(str::trim),
            Some("11"),
            "config: {}",
            state.model.text
        );
        assert_eq!(
            state.model.domain_value(1, "j_parent_start").map(str::trim),
            Some("11")
        );

        // The engine's refusal sentence (its words, not Studio's) rides
        // the map frame inline.
        if let Some(state) = &mut app.advanced {
            state.resolve = Some(Err(
                "placement leaves no Davies clearance at the west boundary\nsecond line".into(),
            ));
        }
        assert_eq!(
            app.placement_refusal().as_deref(),
            Some("placement leaves no Davies clearance at the west boundary")
        );
        let _ = std::fs::remove_dir_all(&temp);
    }

    /// The manual chip's one-click undo: reset-to-fitted restores the
    /// engine's emitted placement keys exactly.
    #[test]
    fn reset_to_fitted_restores_the_engine_placement() {
        let ctx = egui::Context::default();
        let mut app = test_app(&ctx, crate::settings::StudioSettings::default());
        let temp = install_121_state(&mut app);
        let _ = ctx.run_ui(raw(Vec::new()), |ui| app.ui_impl(ui));

        // Manual move via the same path a drop takes.
        app.write_nest_placement(2, 20, 95, Some((360, 280)));
        let state = app.advanced.as_ref().unwrap();
        assert!(crate::advanced::placement_is_manual(
            &state.model,
            &state.base_model,
            1
        ));

        let actions = crate::inspector::InspectorActions {
            reset_placement: Some(2),
            ..Default::default()
        };
        app.handle_actions(&ctx, actions);

        let state = app.advanced.as_ref().unwrap();
        assert!(!crate::advanced::placement_is_manual(
            &state.model,
            &state.base_model,
            1
        ));
        assert_eq!(
            state.model.domain_value(1, "i_parent_start").map(str::trim),
            Some("52")
        );
        assert_eq!(
            state.model.domain_value(1, "j_parent_start").map(str::trim),
            Some("42")
        );
        assert_eq!(
            state.model.domain_value(1, "nx").map(str::trim),
            Some("408")
        );
        assert_eq!(
            state.model.domain_value(1, "ny").map(str::trim),
            Some("320")
        );
        let _ = std::fs::remove_dir_all(&temp);
    }

    /// CHILD-SELECTION REGRESSION: draw a parent, pick a
    /// ladder, open NOTHING else. Placeholders respond immediately, the
    /// continuous fit replaces them with the ENGINE's tree, and a plain
    /// click selects the child.
    #[test]
    fn normal_flow_shows_children_and_click_selects_them() {
        let ctx = egui::Context::default();
        let mut app = test_app(&ctx, crate::settings::StudioSettings::default());
        // The flow's state: a drawn parent + a 12-3 ladder. No review, no
        // advanced surface — the original regression state.
        app.view.center_lat = 35.2;
        app.view.center_lon = -97.4;
        app.view.scale = 30.0;
        app.draft.domain = Some(arwen_map::LambertDomain::centered_at(
            35.2, -97.4, 204, 162, 12_000.0,
        ));
        app.draft.root_dx_km = 12.0;
        app.draft.nests = vec![4];

        // Frame 1: the placeholder child is ALREADY on the map — nothing
        // to select is never shown.
        let _ = ctx.run_ui(raw(Vec::new()), |ui| app.ui_impl(ui));
        assert_eq!(app.placeholder_tree.len(), 2, "root + placeholder child");
        assert!(app.fit.is_none(), "engine fit not in yet");

        // The continuous fit lands from the (fixture) engine without any
        // review/advanced interaction — the nested reply, because the
        // draft has a chain.
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && app.fit.is_none() {
            let _ = ctx.run_ui(raw(Vec::new()), |ui| app.ui_impl(ui));
            std::thread::sleep(Duration::from_millis(25));
        }
        let fit = app.fit.as_ref().expect("continuous fit landed");
        assert_eq!(fit.tree.len(), 2, "engine tree: root + d02");
        assert_eq!(fit.tree[1].grid_id, 2);
        assert!(
            app.placeholder_tree.is_empty(),
            "placeholders retire once the engine answers"
        );

        // A plain CLICK on the child selects it — the gesture the regression had
        // nowhere to make.
        let rect = app.map_pane.last_canvas_rect.expect("canvas laid out");
        let (d02_i, d02_j, d02_ratio) = (
            fit.tree[1].i_parent_start,
            fit.tree[1].j_parent_start,
            fit.tree[1].parent_grid_ratio,
        );
        let center_x = d02_i + (fit.tree[1].nx as f64 / 2.0 - 1.0) / d02_ratio;
        let center_y = d02_j + (fit.tree[1].ny as f64 / 2.0 - 1.0) / d02_ratio;
        let (lat, lon) = app
            .fit
            .as_ref()
            .unwrap()
            .fitted
            .latlon_at_grid(center_x, center_y);
        let click_at = app.view.lon_lat_to_screen(rect, lon as f32, lat as f32);
        let _ = ctx.run_ui(raw(press(click_at)), |ui| app.ui_impl(ui));
        let _ = ctx.run_ui(raw(release(click_at)), |ui| app.ui_impl(ui));
        let _ = ctx.run_ui(raw(Vec::new()), |ui| app.ui_impl(ui));
        assert_eq!(
            app.map_pane.selected_nest,
            Some(2),
            "clicking the child selects it in the NORMAL flow"
        );
    }

    /// Click cycling on overlap: smallest first, then outward, so an
    /// inner nest never makes its parent unreachable.
    #[test]
    fn click_cycles_smallest_first_through_the_overlap_stack() {
        let ctx = egui::Context::default();
        let mut app = test_app(&ctx, crate::settings::StudioSettings::default());
        let temp = install_121_state(&mut app);
        let _ = ctx.run_ui(raw(Vec::new()), |ui| app.ui_impl(ui));
        // A point inside d03 (thus also inside d02): d03's center.
        let point = parent_cells_to_screen(&app, 3, 150.0 + 119.5 / 3.0, 120.0 + 89.5 / 3.0);
        let click = |app: &mut StudioApp, ctx: &egui::Context| {
            let _ = ctx.run_ui(raw(press(point)), |ui| app.ui_impl(ui));
            let _ = ctx.run_ui(raw(release(point)), |ui| app.ui_impl(ui));
            let _ = ctx.run_ui(raw(Vec::new()), |ui| app.ui_impl(ui));
        };
        click(&mut app, &ctx);
        assert_eq!(app.map_pane.selected_nest, Some(3), "smallest first");
        click(&mut app, &ctx);
        assert_eq!(app.map_pane.selected_nest, Some(2), "then its parent");
        click(&mut app, &ctx);
        assert_eq!(app.map_pane.selected_nest, Some(3), "and around again");
        let _ = std::fs::remove_dir_all(&temp);
    }

    /// A typed size (the inspector's nx × ny fields) rides the same
    /// writer as a handle drag: config updated, manual chip on, anchor
    /// untouched — and reset-to-fitted restores.
    #[test]
    fn typed_size_writes_the_config_and_reset_restores() {
        let ctx = egui::Context::default();
        let mut app = test_app(&ctx, crate::settings::StudioSettings::default());
        let temp = install_121_state(&mut app);
        let _ = ctx.run_ui(raw(Vec::new()), |ui| app.ui_impl(ui));

        let actions = crate::inspector::InspectorActions {
            set_domain_size: Some((3, 300, 210)),
            ..Default::default()
        };
        app.handle_actions(&ctx, actions);
        let state = app.advanced.as_ref().unwrap();
        assert_eq!(
            state.model.domain_value(2, "nx").map(str::trim),
            Some("300")
        );
        assert_eq!(
            state.model.domain_value(2, "ny").map(str::trim),
            Some("210")
        );
        // The anchor did not move with the typed size.
        assert_eq!(
            state.model.domain_value(2, "i_parent_start").map(str::trim),
            Some("150")
        );
        assert!(crate::advanced::placement_is_manual(
            &state.model,
            &state.base_model,
            2
        ));

        // Root size is typed the same way (grid 1 → domain[0]).
        let actions = crate::inspector::InspectorActions {
            set_domain_size: Some((1, 190, 150)),
            ..Default::default()
        };
        app.handle_actions(&ctx, actions);
        let state = app.advanced.as_ref().unwrap();
        assert_eq!(
            state.model.domain_value(0, "nx").map(str::trim),
            Some("190")
        );

        // Reset-to-fitted undoes the typed size.
        let actions = crate::inspector::InspectorActions {
            reset_placement: Some(3),
            ..Default::default()
        };
        app.handle_actions(&ctx, actions);
        let state = app.advanced.as_ref().unwrap();
        assert_eq!(
            state.model.domain_value(2, "nx").map(str::trim),
            Some("240")
        );
        assert!(!crate::advanced::placement_is_manual(
            &state.model,
            &state.base_model,
            2
        ));
        let _ = std::fs::remove_dir_all(&temp);
    }

    /// Draw mode is a mode you can SEE and LEAVE: it exits by itself
    /// after a completed draw, Esc exits it, and re-arming over an
    /// existing parent asks first.
    #[test]
    fn draw_mode_yields_to_editing_and_rearm_asks_first() {
        let ctx = egui::Context::default();
        let (settings, temp) = temp_settings("rearm");
        let mut app = test_app(&ctx, settings);
        let _ = ctx.run_ui(raw(Vec::new()), |ui| app.ui_impl(ui));

        // Draw a parent exactly like the acceptance test.
        app.map_pane.draw_mode = true;
        let _ = ctx.run_ui(raw(press(egui::pos2(400.0, 300.0))), |ui| app.ui_impl(ui));
        let _ = ctx.run_ui(
            raw(vec![egui::Event::PointerMoved(egui::pos2(800.0, 600.0))]),
            |ui| app.ui_impl(ui),
        );
        let _ = ctx.run_ui(raw(release(egui::pos2(800.0, 600.0))), |ui| app.ui_impl(ui));
        assert!(app.draft.domain.is_some(), "parent drawn");
        assert!(
            !app.map_pane.draw_mode,
            "completing the draw EXITS draw mode — clicks now select"
        );

        // Re-arming with a parent on the canvas asks first.
        app.map_pane.request_draw_mode(true);
        assert!(app.map_pane.confirm_redraw, "replace-confirm shown");
        assert!(!app.map_pane.draw_mode, "not drawing until confirmed");

        // Esc leaves both the confirm and draw mode.
        let _ = ctx.run_ui(
            raw(vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            }]),
            |ui| app.ui_impl(ui),
        );
        assert!(!app.map_pane.confirm_redraw);
        assert!(!app.map_pane.draw_mode);
        let _ = std::fs::remove_dir_all(&temp);
    }

    /// Screen position of a ROOT-grid point through the app's camera
    /// (the root sketch's own projection — handle targets for gestures).
    fn root_grid_to_screen(app: &StudioApp, x: f64, y: f64) -> egui::Pos2 {
        let rect = app.map_pane.last_canvas_rect.expect("canvas laid out");
        let domain = app.draft.domain.as_ref().unwrap();
        let (lat, lon) = domain.latlon_at_grid(x, y);
        app.view.lon_lat_to_screen(rect, lon as f32, lat as f32)
    }

    /// ROOT RESIZE WORKS EVERYWHERE: dragging the root's east-edge
    /// handle writes the resized nx/ny AND the moved center into the
    /// working config IN PLACE — children and knob edits keep their
    /// bytes, the debounced resolve re-ratifies.
    #[test]
    fn root_handle_resize_writes_center_and_size_into_the_config() {
        let ctx = egui::Context::default();
        let mut app = test_app(&ctx, crate::settings::StudioSettings::default());
        let temp = install_121_state(&mut app);
        let _ = ctx.run_ui(raw(Vec::new()), |ui| app.ui_impl(ui));

        // The root sketch is 204 × 162 @ 12 km: grab the east edge
        // midpoint handle and pull it 20 cells further east.
        let from = root_grid_to_screen(&app, 204.5, 81.5);
        let to = root_grid_to_screen(&app, 224.5, 81.5);
        drag_pointer(&mut app, &ctx, from, to);

        let domain = app.draft.domain.unwrap();
        assert!(
            domain.nx > 210,
            "the sketch resized east: {} x {}",
            domain.nx,
            domain.ny
        );
        let state = app.advanced.as_ref().unwrap();
        let nx_str = domain.nx.to_string();
        assert_eq!(
            state.model.domain_value(0, "nx").map(str::trim),
            Some(nx_str.as_str()),
            "config: {}",
            state.model.text
        );
        assert_eq!(
            state.model.domain_value(0, "ny").map(str::trim),
            Some("162")
        );
        // The center followed the sketch into [projection] — dragging
        // one edge recenters the projection midpoint.
        let expected_lon = format!("{:.4}", domain.ref_lon);
        assert!(
            state
                .model
                .text
                .contains(&format!("ref_lon = {expected_lon}")),
            "projection center follows: {}",
            state.model.text
        );
        assert!(
            state
                .model
                .text
                .contains(&format!("stand_lon = {expected_lon}"))
        );
        // Children and the rest of the file: byte-untouched.
        assert_eq!(
            state.model.domain_value(1, "i_parent_start").map(str::trim),
            Some("52")
        );
        assert_eq!(
            state.model.domain_value(1, "nx").map(str::trim),
            Some("408")
        );
        assert!(state.model.text.contains("truelat1 = 25.5"));
        assert!(
            crate::advanced::placement_is_manual(&state.model, &state.base_model, 0),
            "the resized root reads MANUAL"
        );
        assert!(state.dirty_at.is_some(), "debounced resolve armed");
        let _ = std::fs::remove_dir_all(&temp);
    }

    /// REDRAW ALWAYS WORKS: re-arming Draw over an existing parent asks
    /// first; the completed rectangle REPLACES the surface — the config
    /// regenerates for the new rectangle, the drawn size lands manual,
    /// and children RESET TO THE ENGINE'S FIT (the stated decision; the
    /// confirm says so).
    #[test]
    fn redraw_regenerates_the_surface_with_the_drawn_root_manual() {
        let ctx = egui::Context::default();
        let (settings, temp_out) = temp_settings("redraw");
        let mut app = test_app(&ctx, settings);
        let temp = install_121_state(&mut app);
        app.map_pane.selected_nest = Some(3);
        let _ = ctx.run_ui(raw(Vec::new()), |ui| app.ui_impl(ui));

        // Re-arm over the existing parent: asks first, then Replace.
        app.map_pane.request_draw_mode(true);
        assert!(app.map_pane.confirm_redraw, "replace-confirm shown");
        app.map_pane.confirm_redraw = false;
        app.map_pane.draw_mode = true;

        // Draw the replacement rectangle.
        let _ = ctx.run_ui(raw(press(egui::pos2(400.0, 320.0))), |ui| app.ui_impl(ui));
        let _ = ctx.run_ui(
            raw(vec![egui::Event::PointerMoved(egui::pos2(950.0, 520.0))]),
            |ui| app.ui_impl(ui),
        );
        let _ = ctx.run_ui(raw(release(egui::pos2(950.0, 520.0))), |ui| app.ui_impl(ui));
        let domain = app.draft.domain.expect("redraw produced a domain");

        // The old surface is GONE the moment the rectangle lands — the
        // stale 12-3-1 never lingers under the new parent.
        assert!(app.advanced.is_none(), "old surface dropped");
        assert!(
            app.draft.custom.is_none(),
            "plan back on intent while regenerating"
        );
        assert_eq!(
            app.map_pane.selected_nest, None,
            "old tree's selection orphaned"
        );

        // The fresh emission arrives with the drawn size manual and the
        // children the ENGINE fits for the new rectangle (the 12-3-1's
        // d03 is gone — reset, not carried).
        pump_until(&mut app, &ctx, "regenerated surface", &|app| {
            app.advanced.is_some()
        });
        let state = app.advanced.as_ref().unwrap();
        assert!(
            state.base_text.contains("# Emitted by `gpuwm domain`"),
            "base is the engine's fresh emission"
        );
        let nx_str = domain.nx.to_string();
        assert_eq!(
            state.model.domain_value(0, "nx").map(str::trim),
            Some(nx_str.as_str()),
            "config: {}",
            state.model.text
        );
        assert!(crate::advanced::placement_is_manual(
            &state.model,
            &state.base_model,
            0
        ));
        // Children reset to the fresh emission's own fit — and the
        // redrawing draft has NO ladder, so the fresh fit is a single
        // domain (chainless emissions are single, like the wizard's).
        assert_eq!(state.model.domain_index_for_grid(3), None);
        assert_eq!(state.model.domain_index_for_grid(2), None);
        assert!(
            app.draft.custom.is_some(),
            "plan rides the fresh config.path"
        );
        let _ = std::fs::remove_dir_all(&temp);
        let _ = std::fs::remove_dir_all(&temp_out);
    }

    /// Typed root nx/ny in the NORMAL flow (no config surface open):
    /// the size routes through the same auto-generate as a nest drop
    /// and lands as the manual root — never a silent no-op.
    #[test]
    fn typed_root_size_in_the_normal_flow_autogenerates_the_surface() {
        let ctx = egui::Context::default();
        let (settings, temp) = temp_settings("typedroot");
        let mut app = test_app(&ctx, settings);
        app.draft.domain = Some(arwen_map::LambertDomain::centered_at(
            35.5, -97.5, 300, 240, 3_000.0,
        ));
        // The continuous fit gives the root row its engine-fitted
        // numbers first (the state the regression typed into).
        pump_until(&mut app, &ctx, "continuous fit", &|app| app.fit.is_some());

        let actions = crate::inspector::InspectorActions {
            set_domain_size: Some((1, 350, 175)),
            ..Default::default()
        };
        app.handle_actions(&ctx, actions);
        assert!(app.advanced.is_none(), "generation in flight, not done");
        assert_eq!(
            app.pending_placements.len(),
            1,
            "typed size queued, not lost"
        );

        pump_until(&mut app, &ctx, "config surface", &|app| {
            app.advanced.is_some()
        });
        let state = app.advanced.as_ref().unwrap();
        assert_eq!(
            state.model.domain_value(0, "nx").map(str::trim),
            Some("350")
        );
        assert_eq!(
            state.model.domain_value(0, "ny").map(str::trim),
            Some("175")
        );
        assert!(crate::advanced::placement_is_manual(
            &state.model,
            &state.base_model,
            0
        ));
        assert!(state.dirty_at.is_some(), "engine re-validation armed");
        let _ = std::fs::remove_dir_all(&temp);
    }

    /// A second redraw landing while the engine is STILL WRITING the
    /// first rectangle's config: the stale emission is dropped and
    /// generation re-runs for the current rectangle — the final surface
    /// carries the SECOND drawn size, never the superseded one.
    #[test]
    fn double_redraw_mid_generation_lands_the_second_rectangle() {
        let ctx = egui::Context::default();
        let (settings, temp) = temp_settings("stale");
        let mut app = test_app(&ctx, settings);
        let _ = ctx.run_ui(raw(Vec::new()), |ui| app.ui_impl(ui));

        app.draft.domain = Some(arwen_map::LambertDomain::centered_at(
            35.5, -97.5, 300, 150, 3_000.0,
        ));
        app.handle_placement_edits(
            &ctx,
            vec![crate::map_pane::PlacementEdit::RootDrawn { nx: 300, ny: 150 }],
        );
        assert!(
            app.advanced_generate_slot.in_flight(),
            "first generation running"
        );

        // The second rectangle lands before the first emission arrives.
        app.draft.domain = Some(arwen_map::LambertDomain::centered_at(
            36.0, -99.0, 420, 210, 3_000.0,
        ));
        app.handle_placement_edits(
            &ctx,
            vec![crate::map_pane::PlacementEdit::RootDrawn { nx: 420, ny: 210 }],
        );
        assert!(
            app.advanced_generate_stale,
            "in-flight generation marked stale by the second redraw"
        );

        pump_until(&mut app, &ctx, "second surface", &|app| {
            app.advanced.is_some()
        });
        let state = app.advanced.as_ref().unwrap();
        assert_eq!(
            state.model.domain_value(0, "nx").map(str::trim),
            Some("420"),
            "the SECOND rectangle won: {}",
            state.model.text
        );
        assert_eq!(
            state.model.domain_value(0, "ny").map(str::trim),
            Some("210")
        );
        assert!(!app.advanced_generate_stale, "stale flag consumed");
        let _ = std::fs::remove_dir_all(&temp);
    }

    /// A drop made while NO config surface exists (dragging the review
    /// tree): the placement queues, the engine writes the config (dry
    /// run, fixture here), and the queued anchor lands in it the moment
    /// it arrives — the drag is never lost.
    #[test]
    fn deferred_drop_opens_the_config_surface_and_carries_the_placement() {
        let ctx = egui::Context::default();
        let temp = std::env::temp_dir().join(format!(
            "arwen-deferred-drop-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let mut settings = crate::settings::StudioSettings::default();
        settings.output_root = temp.join("forecasts").to_string_lossy().into_owned();
        let mut app = test_app(&ctx, settings);
        // ERA5 pinned 12-3: the nested route, whose chain emission
        // carries the d02 this deferred drop targets.
        app.draft.source_index = 2;
        app.draft.nests = vec![4];
        app.draft.cycle =
            chrono::NaiveDate::from_ymd_opt(2026, 8, 5).and_then(|date| date.and_hms_opt(0, 0, 0));
        app.draft.domain = Some(arwen_map::LambertDomain::centered_at(
            35.5, -97.5, 300, 240, 3_000.0,
        ));
        let _ = ctx.run_ui(raw(Vec::new()), |ui| app.ui_impl(ui));

        // The drop a map gesture would emit for the review tree's d02.
        app.handle_placement_edits(
            &ctx,
            vec![crate::map_pane::PlacementEdit::Place {
                grid_id: 2,
                i_parent_start: 20,
                j_parent_start: 95,
                size: None,
            }],
        );
        assert!(app.advanced.is_none(), "generation is in flight, not done");
        assert_eq!(app.pending_placements.len(), 1, "drop queued, not lost");

        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && app.advanced.is_none() {
            let _ = ctx.run_ui(raw(Vec::new()), |ui| app.ui_impl(ui));
            std::thread::sleep(Duration::from_millis(20));
        }
        let state = app
            .advanced
            .as_ref()
            .expect("fixture engine wrote the config");
        assert!(
            app.pending_placements.is_empty(),
            "queue drained on arrival"
        );
        // The drop was made against the PLACEHOLDER tree (root 300×240)
        // and rescales RELATIVE into the written config's 204×162 parent:
        // i = round(20·204/300) = 14, j = round(95·162/240) = 64.
        assert_eq!(
            state.model.domain_value(1, "i_parent_start").map(str::trim),
            Some("14"),
            "queued drop landed at its relative position: {}",
            state.model.text
        );
        assert_eq!(
            state.model.domain_value(1, "j_parent_start").map(str::trim),
            Some("64")
        );
        assert!(state.dirty_at.is_some(), "engine re-validation armed");
        assert!(app.draft.custom.is_some(), "the plan now rides config.path");
        let _ = std::fs::remove_dir_all(&temp);
    }

    /// STRANDED-CHILD REGRESSION: place d02 far east, shrink the root through the same
    /// path a handle drag lands — the CARRY clamps d02 back inside the
    /// new clearance envelope (size kept, whole cells), d03 rides
    /// along untouched, the row notice says so, and the debounce
    /// re-arms so estimate + resolve revive without user action.
    #[test]
    fn root_shrink_carries_a_stranded_child_back_inside() {
        let ctx = egui::Context::default();
        let mut app = test_app(&ctx, crate::settings::StudioSettings::default());
        let temp = install_121_state(&mut app);
        let _ = ctx.run_ui(raw(Vec::new()), |ui| app.ui_impl(ui));

        // d02 to the far east (legal for the 204-wide root: hi = 92).
        app.write_nest_placement(2, 90, 42, None);
        let rev_before = app.advanced.as_ref().unwrap().rev;

        // Shrink the root 204 → 150 exactly as a west-edge drag lands.
        if let Some(domain) = &mut app.draft.domain {
            domain.nx = 150;
        }
        app.handle_placement_edits(
            &ctx,
            vec![crate::map_pane::PlacementEdit::RootAdjusted { nx: 150, ny: 162 }],
        );

        let state = app.advanced.as_ref().unwrap();
        assert_eq!(
            state.model.domain_value(0, "nx").map(str::trim),
            Some("150")
        );
        // d02 came along: east limit is floor(150 − 10 − 407/4) = 38.
        assert_eq!(
            state.model.domain_value(1, "i_parent_start").map(str::trim),
            Some("38"),
            "config: {}",
            state.model.text
        );
        assert_eq!(
            state.model.domain_value(1, "nx").map(str::trim),
            Some("408"),
            "size kept — a clamp, not a refit"
        );
        // d03 anchors in d02, whose size did not change: byte-untouched.
        assert_eq!(
            state.model.domain_value(2, "i_parent_start").map(str::trim),
            Some("150")
        );
        assert!(
            state
                .repairs
                .iter()
                .any(|repair| repair.grid_id == 2 && repair.notice.contains("carried along")),
            "{:?}",
            state.repairs
        );
        assert!(
            state.dirty_at.is_some() && state.rev > rev_before,
            "the debounce re-arms — estimate and resolve revive without user action"
        );
        let _ = std::fs::remove_dir_all(&temp);
    }

    /// A child too large for the shrunken root REFITS to the emission's
    /// fitted placement, with the coordinator's visible notice.
    #[test]
    fn root_shrink_refits_an_oversized_child_with_notice() {
        let ctx = egui::Context::default();
        let mut app = test_app(&ctx, crate::settings::StudioSettings::default());
        let temp = install_121_state(&mut app);
        let _ = ctx.run_ui(raw(Vec::new()), |ui| app.ui_impl(ui));

        // d02 manually blown up to 600 wide, then the regression's shrink.
        app.write_nest_placement(2, 52, 42, Some((600, 320)));
        if let Some(domain) = &mut app.draft.domain {
            domain.nx = 150;
        }
        app.handle_placement_edits(
            &ctx,
            vec![crate::map_pane::PlacementEdit::RootAdjusted { nx: 150, ny: 162 }],
        );

        let state = app.advanced.as_ref().unwrap();
        assert_eq!(
            state.model.domain_value(1, "nx").map(str::trim),
            Some("408"),
            "refit to the emission's fitted size: {}",
            state.model.text
        );
        assert_eq!(
            state.model.domain_value(1, "i_parent_start").map(str::trim),
            Some("38"),
            "fitted anchor clamped into the new envelope"
        );
        let repair = state
            .repairs
            .iter()
            .find(|repair| repair.grid_id == 2)
            .expect("d02 repair recorded");
        assert!(
            repair.notice.contains("refit — no longer fit at 600×320"),
            "{}",
            repair.notice
        );
        let _ = std::fs::remove_dir_all(&temp);
    }

    /// VIOLATIONS ARE VISIBLE: the engine's refusal names its domain
    /// (`grid_id = N`); the map gets the red-outline list from BOTH
    /// refusal sources (working-config resolve, estimate), and the
    /// canvas paints it without panicking.
    #[test]
    fn refusal_names_the_domain_for_map_and_strip() {
        const SENTENCE: &str = "child domain grid_id = 2 (d02) violates the \
            parent-row clearance rule: west-east high clearance is -33 parent \
            rows but spec_bdy_width + blend_width = 5 + 5 = 10 rows are required.";
        let ctx = egui::Context::default();
        let mut app = test_app(&ctx, crate::settings::StudioSettings::default());
        let temp = install_121_state(&mut app);
        let _ = ctx.run_ui(raw(Vec::new()), |ui| app.ui_impl(ui));

        // The working config's resolve refusal names d02.
        let saved = app.advanced.as_mut().unwrap().resolve.take();
        app.advanced.as_mut().unwrap().resolve = Some(Err(SENTENCE.into()));
        assert_eq!(
            app.placement_refusal_domains(),
            vec![(2, SENTENCE.to_string())]
        );
        // The frame paints the red outline + anchored sentence.
        let _ = ctx.run_ui(raw(Vec::new()), |ui| app.ui_impl(ui));

        // Before the debounced resolve answers, the ESTIMATE's refusal
        // carries the same naming.
        app.advanced.as_mut().unwrap().resolve = saved;
        app.estimate = Some(Err(SENTENCE.into()));
        assert_eq!(
            app.placement_refusal_domains(),
            vec![(2, SENTENCE.to_string())]
        );
        let _ = ctx.run_ui(raw(Vec::new()), |ui| app.ui_impl(ui));
        let _ = std::fs::remove_dir_all(&temp);
    }

    /// SPAWN SEARCH-BOX DRAG: with the spawn card's draw mode armed, a
    /// drag inside the parent writes `spawn.search_box` — snapped to
    /// whole parent cells — through the same machinery.
    #[test]
    fn synthesized_search_box_drag_writes_the_spawn_bounds() {
        let ctx = egui::Context::default();
        let mut app = test_app(&ctx, crate::settings::StudioSettings::default());
        let temp = install_121_state(&mut app);
        let _ = ctx.run_ui(raw(Vec::new()), |ui| app.ui_impl(ui));

        // Arm exactly as the spawn card does (d03 = domain index 2).
        let mut actions = crate::inspector::InspectorActions::default();
        actions.storm.draw_search_box = Some((2, 3));
        app.handle_actions(&ctx, actions);
        assert_eq!(app.map_pane.search_box_arm, Some((2, 3)));

        // Drag a box over d02 cells (60, 60) .. (240, 190).
        let from = parent_cells_to_screen(&app, 3, 60.0, 60.0);
        let to = parent_cells_to_screen(&app, 3, 240.0, 190.0);
        drag_pointer(&mut app, &ctx, from, to);

        assert_eq!(
            app.map_pane.search_box_arm, None,
            "arm consumed by the drop"
        );
        let state = app.advanced.as_ref().unwrap();
        let spawn = crate::storm::parse_spawn(&state.model, 2).expect("spawn kept");
        assert_eq!(
            spawn.search_box,
            Some([60, 60, 240, 190]),
            "{}",
            state.model.text
        );
        assert_eq!(spawn.trigger, "uh", "the rest of the declaration survives");
        assert_eq!(spawn.threshold, Some(40.0));
        assert!(state.dirty_at.is_some(), "engine re-validation armed");
        let _ = std::fs::remove_dir_all(&temp);
    }

    /// Acceptance step 3 via SYNTHESIZED INPUT: click the search box,
    /// type a query through Text events, results appear from the real
    /// corpus, click the first result, and the camera recenters on it.
    #[test]
    fn synthesized_search_types_finds_and_jumps() {
        let ctx = egui::Context::default();
        let mut app = test_app(&ctx, crate::settings::StudioSettings::default());
        let _ = ctx.run_ui(raw(Vec::new()), |ui| app.ui_impl(ui));

        // Aim at the search field where the pane actually laid it out.
        let search_box = app
            .map_pane
            .last_search_rect
            .expect("search field laid out")
            .center();
        let _ = ctx.run_ui(raw(press(search_box)), |ui| app.ui_impl(ui));
        let _ = ctx.run_ui(raw(release(search_box)), |ui| app.ui_impl(ui));

        let _ = ctx.run_ui(raw(vec![egui::Event::Text("Norman ok".into())]), |ui| {
            app.ui_impl(ui)
        });
        let _ = ctx.run_ui(raw(Vec::new()), |ui| app.ui_impl(ui));
        assert!(
            !app.map_pane.search_results.is_empty(),
            "typing produced live results from the corpus (query text landed: {:?})",
            app.map_pane.search_query
        );
        let first = app.map_pane.search_results[0];
        assert_eq!(first.name, "Norman");
        assert_eq!(first.context_label, Some("OK"));

        // Submit with Enter: the camera jumps to the result.
        let _ = ctx.run_ui(
            raw(vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            }]),
            |ui| app.ui_impl(ui),
        );
        let _ = ctx.run_ui(raw(Vec::new()), |ui| app.ui_impl(ui));
        assert!(
            (app.view.center_lat - 35.2).abs() < 0.4 && (app.view.center_lon + 97.4).abs() < 0.4,
            "camera recentered on Norman, got {:.2},{:.2}",
            app.view.center_lat,
            app.view.center_lon
        );
    }
}
