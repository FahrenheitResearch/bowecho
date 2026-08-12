// SPDX-License-Identifier: Apache-2.0

//! The forecast being designed: domain + intent card values, and the
//! mapping from intent to a `gpuwm.run-plan.v1` plan document. This is
//! the ONLY place a plan is authored.
//!
//! Every value here is INTENT — free numbers, not menus. Presets are
//! quick-picks BESIDE free fields, never the only way to a value. The
//! engine ratifies every value through `--resolve`/`--estimate` and its
//! refusal sentences surface inline; Studio never re-validates.

use arwen_map::LambertDomain;
use arwen_plan::plan::{FetchSpec, IntentConfig, PlanConfig, RunPlan};

/// Root grid-spacing quick-picks (km); the dx field itself is free.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResolutionPreset {
    pub label: &'static str,
    pub dx_km: f64,
    pub maturity: &'static str,
}

pub const RESOLUTION_PRESETS: &[ResolutionPreset] = &[
    ResolutionPreset {
        label: "12 km",
        dx_km: 12.0,
        maturity: "certified",
    },
    ResolutionPreset {
        label: "9 km",
        dx_km: 9.0,
        maturity: "supported",
    },
    ResolutionPreset {
        label: "3 km",
        dx_km: 3.0,
        maturity: "certified",
    },
    ResolutionPreset {
        label: "1 km",
        dx_km: 1.0,
        maturity: "experimental",
    },
];

/// Forecast length quick-picks, hours; the field is free.
pub const LENGTH_PRESETS: &[f64] = &[3.0, 6.0, 12.0, 24.0];

/// Output cadence quick-picks, seconds; the field is free (any whole
/// number of seconds — the engine refuses a value that is not a whole
/// number of the domain's steps, with its own sentence, inline).
pub const OUTPUT_PRESETS_S: &[u32] = &[300, 900, 1_800, 3_600];

/// Nest-ladder quick-picks: (label, root dx km, refinement ratios).
/// These mirror the wizard's own presets (12 / 12-3 / 12-3-1 /
/// 12-3-1-0.5) but Studio always submits root_dx + chain, so ANY
/// root/ratio combination is reachable — the presets are a hand, not a
/// wall.
pub const LADDER_PRESETS: &[(&str, f64, &[u32])] = &[
    ("single", 12.0, &[]),
    ("12-3", 12.0, &[4]),
    ("12-3-1", 12.0, &[4, 3]),
    ("12-3-1-0.5", 12.0, &[4, 3, 2]),
];

/// Source presets in user language, each bound to the engine route that
/// executes it (probe's route inventory is the authority). ALL sources
/// are visible with truthful states — nothing hidden because it is
/// inconvenient.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourcePreset {
    pub label: &'static str,
    pub source: &'static str,
    /// The run-plan route this source rides.
    pub route: &'static str,
    pub maturity: &'static str,
    pub note: &'static str,
}

pub const SOURCE_PRESETS: &[SourcePreset] = &[
    SourcePreset {
        label: "GFS",
        source: "gfs",
        route: "prepared",
        maturity: "certified",
        note: "credential-free · single domains AND nested ladders both \
               run end to end (gpuwm 1.8.6 on this box)",
    },
    SourcePreset {
        label: "HRRR",
        source: "hrrr",
        route: "prepared",
        maturity: "supported",
        note: "single domains AND nested ladders both run (gpuwm 1.8.6) · \
               full-radiation nocturnally-valid default · a nested HRRR \
               fetch is ~4.4 GB",
    },
    SourcePreset {
        label: "ERA5",
        source: "era5",
        route: "experiment",
        maturity: "supported",
        note: "runs NESTED today · needs a Copernicus CDS key · no 'latest' cycle",
    },
];

/// Physics-profile presets: the `gpuwm domain --physics-profile` choice
/// list of the deployed engine (gpuwm 1.7.1 venv, captured 2026-08-07
/// from the wizard's own argparse choices + help text). This list is
/// PRESENTATION ONLY — every selection round-trips through the engine's
/// `--resolve`, and an id this engine no longer accepts surfaces the
/// engine's own refusal sentence inline. Maturity words follow the
/// engine's help: Morrison is the gfs/era5 default (full RTE+RRTMGP,
/// nocturnally valid); `*-validation-*` and `*-no-radiation-*` profiles
/// run reduced physics with longwave OFF and are NOT nocturnally valid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProfilePreset {
    pub id: &'static str,
    pub short: &'static str,
    pub maturity: &'static str,
    pub note: &'static str,
}

pub const PHYSICS_PROFILES: &[ProfilePreset] = &[
    ProfilePreset {
        id: "morrison-mp10-ysu-mm5-noah-kf-rte-rrtmgp-v1",
        short: "Morrison",
        maturity: "certified",
        note: "engine default for GFS/ERA5 — full RTE+RRTMGP radiation + \
               Kain-Fritsch, nocturnally valid",
    },
    ProfilePreset {
        id: "nssl2-mp18-ysu-mm5-noah-kf-rte-rrtmgp-validation-candidate-v1",
        short: "NSSL 2-moment",
        maturity: "experimental",
        note: "validation candidate — RTE+RRTMGP radiation + Kain-Fritsch",
    },
    ProfilePreset {
        id: "nssl2-mp18-ysu-mm5-noah-kf-rrtmg-legacy-validation-candidate-v1",
        short: "NSSL 2m · legacy RRTMG",
        maturity: "experimental",
        note: "validation candidate — legacy RRTMG radiation + Kain-Fritsch",
    },
    ProfilePreset {
        id: "thompson-mp8-ysu-mm5-noah-validation-v1",
        short: "Thompson",
        maturity: "experimental",
        note: "validation profile — longwave OFF, NOT nocturnally valid",
    },
    ProfilePreset {
        id: "wsm6-ysu-mm5-noah-no-radiation-v1",
        short: "WSM6",
        maturity: "supported",
        note: "the native-HRRR-validated slice — longwave OFF, NOT \
               nocturnally valid",
    },
    ProfilePreset {
        id: "wsm6-mynn-mynn-noah-no-radiation-implemented-unverified-v1",
        short: "WSM6 · MYNN",
        maturity: "experimental",
        note: "implemented-unverified — MYNN PBL/surface layer, longwave OFF",
    },
    ProfilePreset {
        id: "wsm6-ysu-mm5-ruc-no-radiation-implemented-unverified-v1",
        short: "WSM6 · RUC LSM",
        maturity: "experimental",
        note: "implemented-unverified — RUC land surface, longwave OFF",
    },
    ProfilePreset {
        id: "wsm6-mynn-mynn-ruc-no-radiation-implemented-unverified-v1",
        short: "WSM6 · MYNN · RUC",
        maturity: "experimental",
        note: "implemented-unverified — MYNN PBL + RUC land surface, longwave OFF",
    },
];

/// What the end-of-run render stage draws (prepared route run option
/// `render_products`; the experiment route renders its own defaults and
/// never carries the key).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RenderMode {
    /// Leave the chain's default set exactly as it is (key absent).
    EngineDefault,
    /// `"all"` — the whole catalog.
    All,
    /// `"none"` — skip the render stage entirely.
    Skip,
    /// The user's favorite products (settings), joined verbatim.
    Favorites,
    /// An explicit product list chosen in the picker, joined verbatim.
    Custom,
}

/// The Advanced surface's active custom config: the plan submits this
/// FILE (`config.path`, absolute, side files beside it) instead of the
/// intent. `rev` bumps on every accepted edit so the estimate/resolve
/// debounce sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct CustomPlanConfig {
    pub config_path: String,
    pub route: String,
    /// The SOURCE this config was generated for. THE CONFIG SURFACE
    /// FOLLOWS THE SOURCE PICKER (the regression's route-flip: a GFS launch
    /// submitted an ERA5-shaped config down the experiment route) — a
    /// picker change away from this value regenerates the surface for
    /// the new source, manual geometry carried.
    pub source: String,
    /// The root dx and ladder this config was generated for: THE
    /// SURFACE FOLLOWS THESE TOO (the regression's -74 strand: a 12-3 ladder
    /// pick rescaled the parent's cell grid around cell-indexed
    /// children). A dx change regenerates with the footprint-true root
    /// carried and children rescaled relative; a ladder change
    /// regenerates with children reset to the engine's fresh fit.
    pub root_dx_km: f64,
    pub nests: Vec<u32>,
    pub rev: u64,
}

/// What launching the current CUSTOM config means for its forcing
/// data, per the engine's own contract ("A plan with a [fetch] block
/// downloads its own; without one, the data has to be there before the
/// run starts").
#[derive(Clone, Debug, PartialEq)]
pub enum ForcingPlan {
    /// No `[case_data]` declaration: the route's own chain fetches
    /// natively at launch (GFS/HRRR prepared-route configs).
    RouteFetches { source: Option<String> },
    /// Every declared forcing file is already on disk — no download.
    OnDisk,
    /// Declared forcing missing: the config's `[fetch]` advisory block
    /// is PROMOTED into the plan's executed fetch (`--out` pointed at
    /// the directory the config-relative forcing paths name).
    Promote {
        source: Option<String>,
        hours: Option<String>,
        args: Vec<String>,
    },
    /// Declared forcing missing and no `[fetch]` advisory to promote —
    /// the engine will refuse at launch with its own sentence.
    MissingNoFetch,
}

/// The double-quoted strings inside a raw TOML value (an array or a
/// single string), in order.
fn quoted_strings(raw: &str) -> Vec<String> {
    raw.split('"')
        .enumerate()
        .filter(|(index, _)| index % 2 == 1)
        .map(|(_, part)| part.to_string())
        .collect()
}

/// The forcing decision for a config's TEXT + on-disk location. Pure
/// half of [`Draft::forcing_plan`]; the config's own `[fetch]` keys are
/// the engine's advisory values, promoted verbatim (only `--out` is
/// derived — the directory the `[case_data]` forcing paths resolve to).
pub fn forcing_plan_of(text: &str, config_path: &std::path::Path) -> ForcingPlan {
    let model = crate::advanced::ConfigModel::parse(text);
    let table_value = |table: &str, key: &str| -> Option<String> {
        model
            .entries
            .iter()
            .find(|entry| entry.table == table && entry.key == key)
            .map(|entry| entry.value.trim().trim_matches('"').to_string())
    };
    let fetch_source = table_value("fetch", "source");
    let Some(forcing_raw) = model
        .entries
        .iter()
        .find(|entry| entry.table == "case_data" && entry.key == "forcing")
        .map(|entry| entry.value.clone())
    else {
        return ForcingPlan::RouteFetches {
            source: fetch_source,
        };
    };
    let forcing = quoted_strings(&forcing_raw);
    if forcing.is_empty() {
        return ForcingPlan::RouteFetches {
            source: fetch_source,
        };
    }
    let config_dir = config_path.parent().unwrap_or(std::path::Path::new("."));
    let resolve = |relative: &str| -> std::path::PathBuf {
        let path = std::path::Path::new(relative);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            config_dir.join(path)
        }
    };
    if forcing.iter().all(|entry| resolve(entry).is_file()) {
        return ForcingPlan::OnDisk;
    }
    // Promote the [fetch] advisory into executed args; missing keys are
    // left missing (the engine's fetch parser is the validity judge).
    if model.entries.iter().any(|entry| entry.table == "fetch") {
        let out_dir = resolve(&forcing[0])
            .parent()
            .map(|parent| parent.to_string_lossy().into_owned());
        let mut args = Vec::new();
        for (key, flag) in [
            ("source", "--source"),
            ("cycle", "--cycle"),
            ("hours", "--hours"),
            ("area", "--area"),
            ("cadence", "--cadence"),
        ] {
            if let Some(value) = table_value("fetch", key) {
                args.push(flag.to_string());
                args.push(value);
            }
        }
        if let Some(out_dir) = out_dir {
            args.push("--out".into());
            args.push(out_dir);
        }
        let hours = table_value("fetch", "hours");
        return ForcingPlan::Promote {
            source: fetch_source,
            hours,
            args,
        };
    }
    ForcingPlan::MissingNoFetch
}

#[derive(Clone, Debug, PartialEq)]
pub struct Draft {
    pub name: String,
    pub source_index: usize,
    /// `None` = latest cycle; `Some` = pinned cycle start (UTC).
    pub cycle: Option<chrono::NaiveDateTime>,
    /// FREE value; presets are quick-picks.
    pub length_hours: f64,
    /// Root grid spacing, km — FREE value.
    pub root_dx_km: f64,
    /// Nest refinement ratios below the root, outermost first (empty =
    /// single domain). Submitted as the wizard's `--chain`; each child's
    /// dx/dt derive from the parent — outputs, never typed.
    pub nests: Vec<u32>,
    /// Root wrfout cadence, seconds — FREE value.
    pub history_interval_s: u32,
    /// Every nest's cadence; `None` = inherit the engine default
    /// (the wizard's 900 s), shown with an inheritance marker.
    pub nest_history_interval_s: Option<u32>,
    /// Physics profile id; `None` = the engine's route default.
    pub physics_profile: Option<String>,
    /// VRAM fit budget, GiB; `None` = engine default (24 GB card class).
    pub vram_gib: Option<f64>,
    /// GPU selection for `run_options.device`: `None` = engine default.
    pub device: Option<u32>,
    /// `run_options.dry_run`: resolve, validate, emit resolved_plan,
    /// stop before any device work. Set by verification drivers; not a
    /// v1 inspector control.
    pub dry_run: bool,
    pub domain: Option<LambertDomain>,
    pub advanced_open: bool,
    /// Which ladder member the inspector focuses (0 = root, 1 = first
    /// nest, …). Selection only — settings live above.
    pub selected_domain: usize,
    /// Advanced surface: when set, the plan rides `config.path` with the
    /// edited TOML and the intent cards feed REGENERATE only.
    pub custom: Option<CustomPlanConfig>,
    /// End-of-run render selection (prepared route).
    pub render_mode: RenderMode,
    /// The explicit list behind [`RenderMode::Custom`] — catalog names
    /// and/or the engine's group keywords, submitted verbatim.
    pub render_custom: Vec<String>,
    /// TEST ONLY: force [`Draft::route_block`] to refuse. The blocked
    /// table is EMPTY at gpuwm 1.8.6, and the guard it feeds is the one
    /// that stopped the regression launching into a known refusal — so the guard
    /// keeps its permanent cell (f10) by being driven from here instead
    /// of from a combination that no longer exists. Never compiled into
    /// the shipped binary.
    #[cfg(test)]
    pub force_route_block: Option<String>,
}

impl Default for Draft {
    fn default() -> Self {
        Self {
            name: default_run_name(),
            source_index: 0,
            cycle: None,
            length_hours: 6.0,
            root_dx_km: 3.0,
            nests: Vec::new(),
            history_interval_s: 900,
            nest_history_interval_s: None,
            physics_profile: None,
            vram_gib: None,
            device: None,
            dry_run: false,
            domain: None,
            advanced_open: false,
            selected_domain: 0,
            custom: None,
            render_mode: RenderMode::EngineDefault,
            render_custom: Vec::new(),
            #[cfg(test)]
            force_route_block: None,
        }
    }
}

pub fn default_run_name() -> String {
    format!("forecast-{}", chrono::Utc::now().format("%Y%m%d-%H%M"))
}

impl Draft {
    pub fn dx_m(&self) -> f64 {
        self.root_dx_km * 1000.0
    }

    pub fn source(&self) -> &'static SourcePreset {
        SOURCE_PRESETS
            .get(self.source_index)
            .unwrap_or(&SOURCE_PRESETS[0])
    }

    /// The route the built plan will ride: a custom config's route was
    /// fixed at generation time; otherwise the source preset's.
    pub fn effective_route(&self) -> &str {
        self.custom
            .as_ref()
            .map(|custom| custom.route.as_str())
            .unwrap_or(self.source().route)
    }

    /// What launching the active CUSTOM config means for its forcing
    /// data — `None` for intent plans (the route fetches for itself).
    pub fn forcing_plan(&self) -> Option<ForcingPlan> {
        let custom = self.custom.as_ref()?;
        let text = std::fs::read_to_string(&custom.config_path).ok()?;
        Some(forcing_plan_of(
            &text,
            std::path::Path::new(&custom.config_path),
        ))
    }

    /// The `render_products` run option this draft asks for (prepared
    /// route only — the experiment route refuses the key and renders its
    /// own defaults). Values travel VERBATIM: catalog names, group
    /// keywords, `all`, or the skip token.
    pub fn render_products_option(&self, favorites: &[String]) -> Option<String> {
        if self.effective_route() != "prepared" {
            return None;
        }
        match self.render_mode {
            RenderMode::EngineDefault => None,
            RenderMode::All => Some("all".into()),
            RenderMode::Skip => Some("none".into()),
            RenderMode::Favorites => (!favorites.is_empty()).then(|| favorites.join(",")),
            RenderMode::Custom => {
                (!self.render_custom.is_empty()).then(|| self.render_custom.join(","))
            }
        }
    }

    /// The dx chain the ratios derive (km), root first. The values are
    /// derived exactly as the engine derives them (parent ÷ ratio) and
    /// are labelled derived in the UI — never editable directly.
    pub fn dx_chain_km(&self) -> Vec<f64> {
        let mut chain = vec![self.root_dx_km];
        let mut dx = self.root_dx_km;
        for ratio in &self.nests {
            dx /= (*ratio).max(1) as f64;
            chain.push(dx);
        }
        chain
    }

    /// Keep an existing domain's grid sized to a new root dx (preserve
    /// the physical footprint the user drew).
    pub fn set_root_dx_km(&mut self, dx_km: f64) {
        let old_dx = self.dx_m();
        self.root_dx_km = dx_km.clamp(0.05, 1_000.0);
        let new_dx = self.dx_m();
        if let Some(domain) = &mut self.domain
            && (old_dx - new_dx).abs() > f64::EPSILON
        {
            let scale = old_dx / new_dx;
            domain.nx = ((domain.nx as f64 * scale).round() as u32).max(16);
            domain.ny = ((domain.ny as f64 * scale).round() as u32).max(16);
            domain.dx_m = new_dx;
        }
    }

    /// The SOURCE the built plan will ride: a custom config's source was
    /// fixed at generation time (the surface follows the picker, so the
    /// two normally agree); the picker's otherwise.
    pub fn effective_source(&self) -> &str {
        self.custom
            .as_ref()
            .map(|custom| custom.source.as_str())
            .unwrap_or(self.source().source)
    }

    /// A source×shape combination the CURRENT engine's run door CANNOT
    /// drive: the Run path renders BLOCKED with this exact reason and
    /// the remedies — never a launchable button into a known refusal
    /// (the regression's default-settings launch, 2026-08-08: a GFS 12-3 hit the
    /// engine's "declares 2 domains, and the single-domain runner this
    /// chain drives takes one"). `custom_domains` is the ACTIVE config
    /// surface's domain count when one exists (the shape that actually
    /// launches); the intent ladder otherwise.
    ///
    /// THIS TABLE IS THE ONE FLIP POINT, AND AT gpuwm 1.8.6 IT IS EMPTY:
    /// every source×shape this UI can express runs at the engine. All of
    /// it measured on this box, not read off a changelog.
    ///
    /// - Nested GFS: 1.8.4 fixed engine defect #118 (the GFS route's
    ///   child finalization missed the `GFS_ST000010` soil-temperature
    ///   spelling and had no SST fallback, so any child holding inland
    ///   water aborted at prepare). The 12-3 at 39.0,-103.0 that refused
    ///   four ways on 1.8.3 now prepares in 16 s and forecasts both
    ///   domains. Live cell l08 — which USED to pin the refusal — is now
    ///   the completing walk.
    /// - Nested HRRR: 1.8.4 drives the three-stage chain from run-plan
    ///   (root preparation → `gpuwm.hrrr_hierarchy_direct` → tree
    ///   forecast → render), up to `_MAX_PUBLIC_DOMAINS` = 21, where
    ///   1.8.3 refused at the front door. Live cell l10, which completes
    ///   with frames from both grids. That chain reads all four of the
    ///   route's side files, so Studio carrying and syncing them (see
    ///   `sync_config_side_files`) is a hard prerequisite, not a nicety.
    ///
    ///   THE HIGH-TERRAIN PLACEMENT EDGE IS GONE AT 1.8.5. On 1.8.4 the
    ///   same tree over eastern Colorado (39.0,-103.0) passed both
    ///   preparation stages and was then refused by the tree forecast's
    ///   own input check — "prepared near-surface surface_qv is outside
    ///   the physical range 0.0..0.2" — because HRRR's 1e-5 GRIB2
    ///   quantisation zeroes 2 m specific humidity over high ground and
    ///   METGRID's sixteen_pt operator undershoots that stencil
    ///   negative. 1.8.5 floors the published surface mixing ratio at
    ///   WRF's own `qv_min_value` (the constant the GFS lane's RH path
    ///   already used — which is why nested GFS never showed it). Live
    ///   cell l10 now RUNS at 39.0,-103.0 and is the standing sentinel
    ///   for that floor.
    /// - ERA5: the experiment route, nested since before any of this.
    ///
    /// - MOVING NESTS ON THE PREPARED ROUTES: a row lived here for
    ///   exactly one engine release, and it CLEARED ITSELF. On 1.8.5
    ///   the storm cards' `[relocation]` reached the GFS/HRRR routes,
    ///   resolved clean, and was then refused at the tree-forecast
    ///   stage — after the fetch and both preparation stages, minutes
    ///   in. Studio blocked that up front instead, keyed on a
    ///   CAPABILITY PROBE rather than a version string: the engine cut
    ///   that can drive a moving nest answers `--resolve` with a
    ///   `moving_nest` record, and Studio already holds that reply for
    ///   the working config (`storm::MovingNest`).
    ///
    ///   gpuwm 1.8.6 reports one — on BOTH prepared chains, `prepared:go`
    ///   and `prepared:hrrr`, each sealing a statics corridor — so the
    ///   row opened with no Studio edit of any kind. That is the whole
    ///   point of probing rather than version-gating, and live cell l11
    ///   is the receipt: it was a sentinel that PANICKED on the engine's
    ///   first reported decision, and it is now the completing
    ///   moving-nest run. ERA5 was never gated: its route feeds a moving
    ///   nest from the config's own geography source.
    ///
    ///   THE GATE ITSELF STAYS WIRED. An engine that reports nothing, or
    ///   refuses, still blocks — which is what an older venv, a rolled
    ///   back install, or a future chain with no delivery would be.
    ///
    /// The mechanism STAYS even when it empties again: the next engine
    /// capability that lands ahead of Studio, or behind it, gets a row
    /// here rather than a launch that fails three stages later. The
    /// guard it feeds is covered permanently by cell f10, which drives
    /// it through `force_route_block`.
    pub fn route_block(
        &self,
        custom_domains: Option<usize>,
        moving_nest: &crate::storm::MovingNest,
    ) -> Option<String> {
        #[cfg(test)]
        if let Some(forced) = &self.force_route_block {
            return Some(forced.clone());
        }
        // A moving nest is a CONFIG fact, so this row is judged before
        // the domain-count rows: the count is about which runner takes
        // the tree, and the corridor is about what the preparation
        // seals for it.
        if let Some(block) = self.moving_nest_block(moving_nest) {
            return Some(block);
        }
        let domains = custom_domains.unwrap_or(self.nests.len() + 1);
        if domains <= 1 {
            return None;
        }
        let _ = domains;
        match (self.effective_route(), self.effective_source()) {
            // No shipped combination is refused by gpuwm 1.8.6 on shape
            // alone. Rows go back here the moment one is, with the
            // engine's own sentence and a remedy that names the right
            // stage.
            _ => None,
        }
    }

    /// The moving-nest half of [`Self::route_block`], separated so the
    /// matrix can walk it directly and so the two reasons stay apart:
    /// an engine that REFUSED this config speaks for itself, and an
    /// engine that has never heard of moving nests gets Studio's
    /// sentence about the next cut.
    fn moving_nest_block(&self, moving_nest: &crate::storm::MovingNest) -> Option<String> {
        use crate::storm::MovingNest;
        match moving_nest {
            MovingNest::Still | MovingNest::Reported(_) => None,
            // The engine's PlanError, its words, plus the one remedy
            // Studio can act on in a click.
            MovingNest::Refused(sentence) => Some(format!(
                "{sentence}\n  Remedies: turn storm following OFF for this \
                 run (the nest stays put), or switch the source to ERA5"
            )),
            MovingNest::EngineSilent if self.effective_route() == "prepared" => Some(
                "storm following is ON and this run is on a prepared route: \
                 moving nests on prepared routes need the next engine \
                 update; ERA5 runs them today. This engine's --resolve \
                 reports no moving-nest decision for the config, which \
                 means its preparation seals no statics corridor — the run \
                 would fetch, prepare, and then be refused by the \
                 tree-forecast stage minutes in. \
                 · Remedies: switch the source to ERA5 (its route rebuilds \
                 each footprint's statics at move time), or turn storm \
                 following OFF for this run (the nest stays put). \
                 Studio unblocks this by itself as soon as the engine's \
                 --resolve answers with a moving_nest decision."
                    .to_string(),
            ),
            // ERA5 (the experiment route) runs moving nests today, on
            // this engine, with no corridor to seal.
            MovingNest::EngineSilent => None,
        }
    }

    /// Everything needed before Run/Review is allowed.
    pub fn validate(&self) -> Result<&LambertDomain, String> {
        if self.name.trim().is_empty() {
            return Err("the run needs a name".into());
        }
        let domain = self
            .domain
            .as_ref()
            .ok_or_else(|| "draw a forecast domain on the map first".to_string())?;
        domain.validate()?;
        if !(0.5..=240.0).contains(&self.length_hours) {
            return Err("forecast length must be between 0.5 h and 240 h".into());
        }
        if self.history_interval_s == 0 {
            return Err("output cadence must be positive".into());
        }
        if self.nest_history_interval_s == Some(0) {
            return Err("nest output cadence must be positive".into());
        }
        if !self.root_dx_km.is_finite() || self.root_dx_km <= 0.0 {
            return Err("root dx must be positive".into());
        }
        if self.nests.iter().any(|ratio| *ratio < 2) {
            return Err("nest refinement ratios must be at least 2".into());
        }
        Ok(domain)
    }

    /// The run directory this draft will claim under `output_root`
    /// (`output_root` in the plan IS the run directory).
    pub fn run_dir(&self, output_root: &str) -> String {
        format!(
            "{}/{}",
            output_root.trim_end_matches(['/', '\\']),
            self.name.trim()
        )
    }

    /// Build the plan document: an INTENT plan. The engine's wizard
    /// writes the actual config; there is no nx/ny input — domain size
    /// is FITTED from the ladder and the VRAM budget, and the drawn
    /// rectangle's center + resolution are the intent. The fitted
    /// geometry comes back on `--resolve` and the run outline follows
    /// the resolved plan, never the sketch.
    #[cfg_attr(not(test), allow(dead_code))] // test/API convenience over to_plan_with_geog
    pub fn to_plan(&self, output_root: &str) -> Result<RunPlan, String> {
        self.to_plan_with_geog(output_root, None, &[])
    }

    pub fn to_plan_with_geog(
        &self,
        output_root: &str,
        geog_root: Option<&str>,
        favorite_products: &[String],
    ) -> Result<RunPlan, String> {
        // A custom config plan submits the edited FILE; the intent
        // cards feed regeneration only. The file's route was fixed at
        // generation time (it decides which front door loads it).
        if let Some(custom) = &self.custom {
            if self.name.trim().is_empty() {
                return Err("the run needs a name".into());
            }
            let mut plan = RunPlan::new(
                self.name.trim(),
                custom.route.clone(),
                PlanConfig::Path(custom.config_path.clone()),
                self.run_dir(output_root),
            );
            // Declared [case_data] forcing not on disk: the config's
            // own [fetch] advisory rides the plan as the EXECUTED fetch
            // ("A plan with a [fetch] block downloads its own" — the
            // engine's sentence). On disk → skip, exactly as today.
            if let Some(ForcingPlan::Promote { args, .. }) = self.forcing_plan() {
                plan.fetch = Some(FetchSpec { args });
            }
            // PREPARED-route config plans carry the staged WPS_GEOG
            // through run_options (without it the prepare stage refuses
            // on the engine's default location — matrix-found). The
            // experiment route REFUSES the key: its geog tree lives in
            // the config's own [case_data], written at generation from
            // the same setting. Intent plans carry it inside the intent.
            if custom.route == "prepared" {
                plan.run_options.geog_root = geog_root.map(str::to_string);
            }
            plan.run_options.device = self.device.map(|device| serde_json::json!(device));
            if self.dry_run {
                plan.run_options.dry_run = Some(true);
            }
            plan.run_options.render_products = self.render_products_option(favorite_products);
            return Ok(plan);
        }
        let domain = self.validate()?;
        let source = self.source();
        let intent = IntentConfig {
            point: Some(format!("{:.4},{:.4}", domain.ref_lat, domain.ref_lon)),
            source: Some(source.source.into()),
            // "latest" is resolved by the engine to a concrete cycle
            // BEFORE the fetch stage and recorded in
            // automatic_resolutions. A pinned cycle is spelled the ONE
            // way the wizard accepts — YYYY-MM-DDTHH (UTC): the compact
            // %Y%m%d%H spelling is REFUSED ("--cycle '2026080600' must
            // be YYYY-MM-DDTHH (UTC) or 'latest'", verified live
            // 2026-08-07).
            cycle: match self.cycle {
                None => "latest".to_string(),
                Some(cycle) => cycle.format("%Y-%m-%dT%H").to_string(),
            },
            hours: Some(self.length_hours.ceil().max(1.0) as u64),
            root_dx_km: Some(self.root_dx_km),
            chain: (!self.nests.is_empty()).then(|| {
                self.nests
                    .iter()
                    .map(|ratio| ratio.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            }),
            history_interval_s: Some(self.history_interval_s as u64),
            nest_history_interval_s: self
                .nest_history_interval_s
                .filter(|_| !self.nests.is_empty())
                .map(|seconds| seconds as u64),
            physics_profile: self.physics_profile.clone(),
            vram_gib: self.vram_gib,
            name: Some(self.name.trim().to_string()),
            geog_root: geog_root.map(str::to_string),
            ..IntentConfig::default()
        };
        let mut plan = RunPlan::new(
            self.name.trim(),
            source.route,
            PlanConfig::Intent(intent),
            self.run_dir(output_root),
        );
        plan.run_options.device = self.device.map(|device| serde_json::json!(device));
        if self.dry_run {
            plan.run_options.dry_run = Some(true);
        }
        plan.run_options.render_products = self.render_products_option(favorite_products);
        Ok(plan)
    }

    /// The dry-run plan that makes the ENGINE write its config into a
    /// durable Studio workspace (the Advanced surface's seed): same
    /// intent, `output_root` = the workspace itself, `dry_run` on — the
    /// engine emits `intent-config.toml` + side files and stops before
    /// any device work.
    pub fn to_generation_plan(
        &self,
        workspace: &str,
        geog_root: Option<&str>,
    ) -> Result<RunPlan, String> {
        let mut base = self.clone();
        base.custom = None;
        // Generation is about the config; the render selection rides the
        // REAL run's plan, not the dry generation.
        base.render_mode = RenderMode::EngineDefault;
        let mut plan = base.to_plan_with_geog(workspace, geog_root, &[])?;
        plan.output_root = Some(workspace.to_string());
        plan.run_options.dry_run = Some(true);
        Ok(plan)
    }

    /// A cheap fingerprint of everything that feeds the plan — the
    /// estimate strip re-queries when this changes.
    pub fn fingerprint(&self, output_root: &str) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::hash::DefaultHasher::new();
        output_root.hash(&mut hasher);
        self.name.hash(&mut hasher);
        self.source_index.hash(&mut hasher);
        self.cycle.hash(&mut hasher);
        self.length_hours.to_bits().hash(&mut hasher);
        self.root_dx_km.to_bits().hash(&mut hasher);
        self.nests.hash(&mut hasher);
        self.history_interval_s.hash(&mut hasher);
        self.nest_history_interval_s.hash(&mut hasher);
        self.physics_profile.hash(&mut hasher);
        self.vram_gib.map(f64::to_bits).hash(&mut hasher);
        self.device.hash(&mut hasher);
        self.dry_run.hash(&mut hasher);
        self.render_mode.hash(&mut hasher);
        self.render_custom.hash(&mut hasher);
        if let Some(custom) = &self.custom {
            custom.config_path.hash(&mut hasher);
            custom.route.hash(&mut hasher);
            custom.source.hash(&mut hasher);
            custom.rev.hash(&mut hasher);
        }
        if let Some(domain) = &self.domain {
            domain.nx.hash(&mut hasher);
            domain.ny.hash(&mut hasher);
            domain.dx_m.to_bits().hash(&mut hasher);
            domain.ref_lat.to_bits().hash(&mut hasher);
            domain.ref_lon.to_bits().hash(&mut hasher);
            domain.truelat1.to_bits().hash(&mut hasher);
            domain.truelat2.to_bits().hash(&mut hasher);
            domain.stand_lon.to_bits().hash(&mut hasher);
        }
        hasher.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft_with_domain() -> Draft {
        Draft {
            domain: Some(LambertDomain::centered_at(35.5, -97.5, 300, 240, 3_000.0)),
            ..Draft::default()
        }
    }

    /// The overwhelmingly common state: the config moves no nest, so the
    /// moving-nest row never fires and the rest of the table is what is
    /// under test.
    fn still() -> crate::storm::MovingNest {
        crate::storm::MovingNest::Still
    }

    #[test]
    fn golden_path_plan_is_a_prepared_route_intent() {
        let draft = draft_with_domain();
        let plan = draft.to_plan("C:/Forecasts").unwrap();
        assert_eq!(plan.route, "prepared", "GFS rides the prepared route");
        assert_eq!(
            plan.output_root.as_deref(),
            Some(format!("C:/Forecasts/{}", draft.name).as_str())
        );
        // Intent plans carry no fetch block: the intent's cycle/source
        // drive the route's own fetch stage.
        assert!(plan.fetch.is_none());
        match &plan.config {
            PlanConfig::Intent(intent) => {
                assert_eq!(intent.point.as_deref(), Some("35.5000,-97.5000"));
                assert_eq!(intent.source.as_deref(), Some("gfs"));
                assert_eq!(intent.cycle, "latest");
                assert_eq!(intent.hours, Some(6));
                assert_eq!(intent.root_dx_km, Some(3.0));
                assert_eq!(intent.history_interval_s, Some(900));
                // Single domain: no chain, no nest cadence key at all.
                assert_eq!(intent.chain, None);
                assert_eq!(intent.nest_history_interval_s, None);
                // No nx/ny anywhere: size is the ENGINE's output.
                assert_eq!(intent.vram_gib, None, "budget left to the wizard");
            }
            other => panic!("expected intent, got {other:?}"),
        }
        // No device chosen → run_options omitted from the JSON entirely.
        let value = serde_json::to_value(&plan).unwrap();
        assert!(value.get("run_options").is_none());
    }

    #[test]
    fn era5_maps_to_the_experiment_route_and_pinned_cycle_spells_fetch_style() {
        let mut draft = draft_with_domain();
        draft.source_index = 2; // ERA5
        draft.cycle = chrono::NaiveDate::from_ymd_opt(2026, 8, 7)
            .unwrap()
            .and_hms_opt(6, 0, 0);
        draft.device = Some(0);
        let plan = draft.to_plan("C:/Forecasts").unwrap();
        assert_eq!(plan.route, "experiment");
        assert_eq!(plan.run_options.device, Some(serde_json::json!(0)));
        match &plan.config {
            PlanConfig::Intent(intent) => {
                assert_eq!(intent.source.as_deref(), Some("era5"));
                // The ONE spelling the wizard accepts (its refusal of the
                // compact form is captured verbatim in to_plan_with_geog).
                assert_eq!(intent.cycle, "2026-08-07T06");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn nest_ratios_travel_as_the_wizard_chain_with_cadence_override() {
        let mut draft = draft_with_domain();
        draft.source_index = 2; // ERA5 — the nested route
        draft.root_dx_km = 12.0;
        draft.nests = vec![4, 3];
        draft.history_interval_s = 1_800;
        draft.nest_history_interval_s = Some(300);
        draft.physics_profile = Some("morrison-mp10-ysu-mm5-noah-kf-rte-rrtmgp-v1".into());
        draft.vram_gib = Some(16.0);
        let plan = draft.to_plan("C:/Forecasts").unwrap();
        match &plan.config {
            PlanConfig::Intent(intent) => {
                assert_eq!(intent.root_dx_km, Some(12.0));
                assert_eq!(intent.chain.as_deref(), Some("4,3"));
                assert_eq!(intent.history_interval_s, Some(1_800));
                assert_eq!(intent.nest_history_interval_s, Some(300));
                assert_eq!(
                    intent.physics_profile.as_deref(),
                    Some("morrison-mp10-ysu-mm5-noah-kf-rte-rrtmgp-v1")
                );
                assert_eq!(intent.vram_gib, Some(16.0));
                // No ladder key: Studio always spells root_dx + chain.
                assert_eq!(intent.ladder, None);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(draft.dx_chain_km(), vec![12.0, 3.0, 1.0]);
    }

    #[test]
    fn nest_cadence_is_omitted_for_single_domain_drafts() {
        let mut draft = draft_with_domain();
        draft.nest_history_interval_s = Some(300);
        let plan = draft.to_plan("C:/Forecasts").unwrap();
        match &plan.config {
            PlanConfig::Intent(intent) => {
                assert_eq!(intent.nest_history_interval_s, None);
            }
            other => panic!("{other:?}"),
        }
    }

    /// THE ROUTE TABLE at gpuwm 1.8.6 truth, as MEASURED on this box:
    /// every source×shape this UI can express is runnable. The table is
    /// empty, so what this asserts is that it IS empty — a row that
    /// creeps back in without an engine reason fails here.
    #[test]
    fn known_unrunnable_combos_block_with_reason_and_remedies() {
        let mut draft = draft_with_domain();
        for source_index in 0..SOURCE_PRESETS.len() {
            draft.source_index = source_index;
            let label = SOURCE_PRESETS[source_index].label;
            for nests in [vec![], vec![4], vec![4, 3], vec![4, 3, 2]] {
                draft.nests = nests.clone();
                assert!(
                    draft.route_block(None, &still()).is_none(),
                    "{label} with ladder {nests:?} must be launchable at 1.8.6"
                );
                assert!(draft.route_block(Some(nests.len() + 1), &still()).is_none());
            }
        }
        draft.nests.clear();
        assert!(draft.route_block(Some(1), &still()).is_none());
        assert!(draft.route_block(Some(2), &still()).is_none());
        // A custom surface's own source is what the table WOULD key on;
        // no combination of picker and surface produces a refusal.
        draft.source_index = 0;
        for source in ["gfs", "hrrr", "era5"] {
            draft.custom = Some(CustomPlanConfig {
                config_path: "C:/x/draft-config.toml".into(),
                route: if source == "era5" {
                    "experiment"
                } else {
                    "prepared"
                }
                .into(),
                source: source.into(),
                root_dx_km: 12.0,
                nests: vec![4],
                rev: 1,
            });
            assert!(draft.route_block(Some(2), &still()).is_none(), "{source}");
        }
    }

    /// THE MOVING-NEST ROW, both directions. gpuwm 1.8.6 reports a
    /// decision, so on this box the row is OPEN — and this cell still
    /// walks the shut side, because the gate has to keep working for an
    /// engine that reports nothing (an older venv, a rollback, a future
    /// chain with no delivery). The row is keyed on the engine's OWN
    /// answer, which is why it opened without a Studio release.
    #[test]
    fn a_moving_nest_on_a_prepared_route_is_blocked_until_the_engine_reports_one() {
        use crate::storm::MovingNest;
        use arwen_plan::queries::MovingNestDecision;

        let mut draft = draft_with_domain();
        draft.nests = vec![4];

        // GFS + HRRR (prepared): silent engine → blocked, in words that
        // name the route, the remedy and the unblock condition.
        for source_index in [0usize, 1] {
            draft.source_index = source_index;
            let block = draft
                .route_block(Some(2), &MovingNest::EngineSilent)
                .unwrap_or_else(|| {
                    panic!(
                        "{} + follow must not be launchable while the engine \
                         reports no moving-nest decision",
                        SOURCE_PRESETS[source_index].label
                    )
                });
            assert!(block.contains("need the next engine update"), "{block}");
            assert!(block.contains("ERA5 runs them today"), "{block}");
            assert!(block.contains("switch the source to ERA5"), "{block}");
        }

        // ERA5 (experiment route): moving nests run TODAY, and blocking
        // them would withhold a working feature.
        draft.source_index = 2;
        assert!(
            draft
                .route_block(Some(2), &MovingNest::EngineSilent)
                .is_none(),
            "ERA5 runs moving nests on the box's engine"
        );

        // The engine REPORTS a decision → every route opens, including
        // the prepared ones. This is the flip, and nothing but the
        // engine's answer performs it.
        let decision = MovingNestDecision {
            chain: "prepared:go".into(),
            delivery: Some("statics_corridor".into()),
            relocation_grid_id: Some(2),
            statics_corridor: true,
        };
        for source_index in 0..SOURCE_PRESETS.len() {
            draft.source_index = source_index;
            assert!(
                draft
                    .route_block(Some(2), &MovingNest::Reported(Box::new(decision.clone())))
                    .is_none(),
                "{} unblocks on the engine's own decision",
                SOURCE_PRESETS[source_index].label
            );
        }

        // A single-domain draft with no follow source is untouched by
        // any of this.
        draft.nests.clear();
        draft.source_index = 0;
        assert!(draft.route_block(Some(1), &still()).is_none());
    }

    /// An engine REFUSAL over a moving nest reaches the caller in the
    /// ENGINE's words. Studio adds its remedies after the sentence and
    /// changes not one byte of it — a paraphrase would send a reader to
    /// the wrong stage.
    #[test]
    fn a_moving_nest_refusal_is_carried_verbatim() {
        use crate::storm::MovingNest;

        let engine = "this plan's config declares a [relocation] follow \
                      source on d02, and the 'prepared:hrrr' chain cannot \
                      supply the statics a moving nest needs";
        let mut draft = draft_with_domain();
        draft.source_index = 1;
        draft.nests = vec![4];
        let block = draft
            .route_block(Some(2), &MovingNest::Refused(engine.to_string()))
            .expect("a refused moving nest blocks");
        assert!(block.starts_with(engine), "{block}");
        assert!(block.contains("turn storm following OFF"), "{block}");
    }

    /// THE GUARD the table feeds, kept covered now that the table is
    /// empty [historical regression: a launchable button into a known refusal].
    /// When a row DOES exist, its sentence reaches the caller verbatim.
    #[test]
    fn a_blocked_row_still_reaches_the_caller_verbatim() {
        let mut draft = draft_with_domain();
        assert!(draft.route_block(None, &still()).is_none());
        draft.force_route_block = Some("the engine's own sentence, whatever it is".into());
        assert_eq!(
            draft.route_block(None, &still()).as_deref(),
            Some("the engine's own sentence, whatever it is")
        );
        // A block applies whatever the shape — the table decides, not
        // the domain count.
        assert_eq!(
            draft.route_block(Some(1), &still()).as_deref(),
            Some("the engine's own sentence, whatever it is")
        );
    }

    #[test]
    fn render_selection_travels_verbatim_on_the_prepared_route_only() {
        let mut draft = draft_with_domain();
        // Engine default: the key is ABSENT (never an empty string).
        let plan = draft.to_plan("C:/F").unwrap();
        assert_eq!(plan.run_options.render_products, None);
        // Explicit list, verbatim order.
        draft.render_mode = RenderMode::Custom;
        draft.render_custom = vec![
            "sbcape".into(),
            "1km_reflectivity".into(),
            "windowed".into(),
        ];
        let plan = draft.to_plan("C:/F").unwrap();
        assert_eq!(
            plan.run_options.render_products.as_deref(),
            Some("sbcape,1km_reflectivity,windowed")
        );
        // Skip token.
        draft.render_mode = RenderMode::Skip;
        let plan = draft.to_plan("C:/F").unwrap();
        assert_eq!(plan.run_options.render_products.as_deref(), Some("none"));
        // Favorites come from settings at plan-build time.
        draft.render_mode = RenderMode::Favorites;
        let plan = draft
            .to_plan_with_geog("C:/F", None, &["vpd_2m".to_string()])
            .unwrap();
        assert_eq!(plan.run_options.render_products.as_deref(), Some("vpd_2m"));
        // The experiment route never carries the key (the engine refuses
        // it there).
        draft.source_index = 2; // ERA5
        draft.render_mode = RenderMode::All;
        let plan = draft.to_plan("C:/F").unwrap();
        assert_eq!(plan.run_options.render_products, None);
    }

    #[test]
    fn no_domain_is_a_friendly_error_not_a_plan() {
        let draft = Draft::default();
        let error = draft.to_plan("C:/Forecasts").unwrap_err();
        assert!(error.contains("draw a forecast domain"), "{error}");
    }

    #[test]
    fn resolution_change_preserves_the_drawn_footprint() {
        let mut draft = draft_with_domain();
        let before_km = draft.domain.unwrap().width_km();
        draft.set_root_dx_km(1.0); // 3 km → 1 km
        let domain = draft.domain.unwrap();
        assert_eq!(domain.dx_m, 1_000.0);
        assert!(
            (domain.width_km() - before_km).abs() < 3.0,
            "{}",
            domain.width_km()
        );
        assert_eq!(domain.nx, 900);
    }

    #[test]
    fn fingerprint_tracks_domain_and_knob_edits() {
        let draft_a = draft_with_domain();
        let mut draft_b = draft_a.clone();
        assert_eq!(draft_a.fingerprint("r"), draft_b.fingerprint("r"));
        draft_b.domain.as_mut().unwrap().nx += 1;
        assert_ne!(draft_a.fingerprint("r"), draft_b.fingerprint("r"));
        let mut draft_c = draft_a.clone();
        draft_c.nests = vec![3];
        assert_ne!(draft_a.fingerprint("r"), draft_c.fingerprint("r"));
        let mut draft_d = draft_a.clone();
        draft_d.history_interval_s = 313;
        assert_ne!(draft_a.fingerprint("r"), draft_d.fingerprint("r"));
    }
}
