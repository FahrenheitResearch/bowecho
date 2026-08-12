// SPDX-License-Identifier: Apache-2.0

//! Storm-following + dormant-spawn cards: first-class inspector surfaces
//! over the 1.8.0 config tables — `[relocation]`, `[relocation.follow]`,
//! `[[relocation.move]]`, and `spawn = {…}` on a nested `[[domain]]`.
//!
//! ONE source of truth: the Advanced surface's working TOML. The cards
//! PARSE their state out of it and WRITE whole blocks back into it;
//! every write rides the same debounced engine `--resolve`, so the
//! verdict is always the RUNNING engine's own sentence. The box's engine
//! is the released gpuwm 1.8.6, which carries the merged 1.8 tables, so
//! the cards are fully live; any refusal shown is that engine's, not a
//! placeholder. Schema spelled here is 1.8.0's (wt-180 @ ca19d4cf1: RelocationConfig,
//! storm_tracking.FollowConfig, nest_spawn.SpawnConfig) and the emitted
//! text is validated against that tree's REAL loader in tests/live
//! checks — never re-implemented as Studio-side rules.

use crate::advanced::ConfigModel;

/// The engine's tracked-field vocabulary (storm_tracking.TRACKED_FIELDS)
/// with the units its docstrings name. Presentation only — validity is
/// the engine's.
pub const TRACKED_FIELDS: &[(&str, &str)] = &[("uh", "m²/s²"), ("reflectivity", "dBZ")];

/// nest_spawn.SPAWN_TRIGGERS.
pub const SPAWN_TRIGGERS: &[&str] = &["uh", "reflectivity", "time"];

#[derive(Clone, Debug, PartialEq)]
pub struct TrackerSettings {
    pub field: String,
    pub threshold: f64,
    /// dBZ; required by the engine iff field == "uh".
    pub fallback_threshold: Option<f64>,
    pub search_margin_cells: i64,
    /// The dead-band floor: shifts below this are never proposed.
    pub min_shift_cells: i64,
    pub max_shift_cells: i64,
    pub cooldown_seconds: f64,
}

impl Default for TrackerSettings {
    fn default() -> Self {
        // The 1.8.0 worked example's own numbers (follow-2km config) —
        // a starting point, every one editable, engine-ratified.
        Self {
            field: "uh".into(),
            threshold: 25.0,
            fallback_threshold: Some(40.0),
            search_margin_cells: 15,
            min_shift_cells: 2,
            max_shift_cells: 8,
            cooldown_seconds: 900.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MoveRow {
    pub at_seconds: f64,
    pub di_parent_cells: i64,
    pub dj_parent_cells: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FollowSource {
    None,
    Tracker(TrackerSettings),
    Moves(Vec<MoveRow>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct FollowSettings {
    pub grid_id: i64,
    pub max_move_parent_cells: Option<i64>,
    pub min_overlap_fraction: Option<f64>,
    /// None = every complete cycle boundary (the engine's default).
    pub cadence_seconds: Option<f64>,
    pub source: FollowSource,
}

impl Default for FollowSettings {
    fn default() -> Self {
        Self {
            grid_id: 2,
            max_move_parent_cells: Some(8),
            min_overlap_fraction: Some(0.5),
            cadence_seconds: Some(900.0),
            source: FollowSource::Tracker(TrackerSettings::default()),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SpawnSettings {
    pub trigger: String,
    pub threshold: Option<f64>,
    pub earliest_s: Option<f64>,
    pub latest_s: Option<f64>,
    /// trigger = "time" only.
    pub at_s: Option<f64>,
    /// 1-based inclusive parent cells [i_lo, j_lo, i_hi, j_hi].
    pub search_box: Option<[i64; 4]>,
}

impl Default for SpawnSettings {
    fn default() -> Self {
        Self {
            trigger: "uh".into(),
            threshold: Some(40.0),
            earliest_s: Some(3_600.0),
            latest_s: Some(21_600.0),
            at_s: None,
            search_box: None,
        }
    }
}

// ---------------------------------------------------------------------------
// MOVING NESTS: what the active config declares, and what the engine in
// the box says it will do about it.
// ---------------------------------------------------------------------------

/// The engine's own marker in a moving-nest refusal. `PlanError`'s two
/// moving-nest sentences both spell it, and nothing else in the engine's
/// refusal vocabulary does — so a refusal carrying it is attributable to
/// the moving nest rather than to the placement or the schema.
const FOLLOW_REFUSAL_MARKER: &str = "[relocation] follow source";

/// Studio's copy of the engine's follow predicate
/// (`gpuwm.static.corridor.config_declares_follow_source`): enabled
/// `[relocation]` NAMING A SOURCE — a tracker or an itinerary.
///
/// Bounds-only `[relocation]` is not a moving nest. It constrains where
/// the nest may sit and the nest stays put, so none of the corridor
/// machinery applies to it and Studio must not block it.
pub fn declares_follow_source(model: &ConfigModel) -> Option<&'static str> {
    match parse_follow(model)?.source {
        FollowSource::Tracker(_) => Some("tracker"),
        FollowSource::Moves(_) => Some("itinerary"),
        FollowSource::None => None,
    }
}

/// The moving-nest decision state Studio renders and gates on.
///
/// THE CAPABILITY PROBE, and why it is a probe rather than a version
/// number: Studio already re-resolves the working config through the
/// real engine on every edit, and the engine that can drive a moving
/// nest answers that same `--resolve` with a `moving_nest` record. So
/// "does this engine report moving nests?" is read off a document
/// Studio holds anyway — no extra invocation, no `>= 1.8.6` string
/// compare that a rebuilt venv or a lane build could make a lie.
///
/// [`Self::EngineSilent`] is the FAIL-SAFE state: it covers both a
/// pre-decision engine and the moment between writing the follow
/// tables and the engine's answer landing. Both mean "nobody has told
/// Studio this run can move a nest", and on a prepared route both
/// block, because a launch that cannot be justified from a document in
/// hand is the launch that dies at the tree stage minutes later.
#[derive(Clone, Debug, PartialEq)]
pub enum MovingNest {
    /// The active config declares no `[relocation]` follow source.
    Still,
    /// Declared, and the engine said how this chain will feed it.
    Reported(Box<arwen_plan::queries::MovingNestDecision>),
    /// Declared, and the engine REFUSED the config over it — its own
    /// sentence, carried verbatim.
    Refused(String),
    /// Declared, and this engine's `--resolve` says nothing about
    /// moving nests at all.
    EngineSilent,
}

impl MovingNest {
    /// Read the state from the active config surface and the engine's
    /// latest verdict on it. `model` is the working TOML; `resolve` is
    /// the reply (or refusal) for that same text.
    pub fn read(
        model: Option<&ConfigModel>,
        resolve: Option<&Result<Box<arwen_plan::queries::ResolveReport>, String>>,
    ) -> Self {
        let Some(model) = model else {
            return Self::Still;
        };
        if declares_follow_source(model).is_none() {
            return Self::Still;
        }
        match resolve {
            Some(Ok(report)) => match &report.moving_nest {
                Some(decision) => Self::Reported(Box::new(decision.clone())),
                None => Self::EngineSilent,
            },
            Some(Err(error)) if error.contains(FOLLOW_REFUSAL_MARKER) => {
                Self::Refused(engine_sentence(error).to_string())
            }
            // Any OTHER refusal is about something else (a placement, a
            // schema value); the moving-nest state is simply unknown
            // until the config resolves.
            _ => Self::EngineSilent,
        }
    }
}

/// The engine's sentence out of a transport-wrapped error: Studio's
/// launcher prefixes a failed query with its exit status, and the CLI
/// prefixes its own name. Both are stripped; every byte the ENGINE
/// wrote is kept, which is what "verbatim" has to mean.
pub fn engine_sentence(error: &str) -> &str {
    let text = match error.find("gpuwm run-plan: ") {
        Some(position) => &error[position + "gpuwm run-plan: ".len()..],
        None => error,
    };
    text.trim()
}

// ---------------------------------------------------------------------------
// Parse: card state OUT of the working TOML (via the ConfigModel's
// entries — the same line model the Advanced editor shows).
// ---------------------------------------------------------------------------

fn entry_value<'m>(model: &'m ConfigModel, table: &str, key: &str) -> Option<&'m str> {
    model
        .entries
        .iter()
        .find(|entry| entry.table == table && entry.key == key)
        .map(|entry| entry.value.as_str())
}

fn as_i64(value: &str) -> Option<i64> {
    value.trim().parse::<i64>().ok().or_else(|| {
        value
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|v| v.fract() == 0.0)
            .map(|v| v as i64)
    })
}

fn as_f64(value: &str) -> Option<f64> {
    value.trim().parse::<f64>().ok()
}

fn as_string(value: &str) -> String {
    value.trim().trim_matches('"').to_string()
}

/// The `[relocation]` surface currently in the config, when enabled.
pub fn parse_follow(model: &ConfigModel) -> Option<FollowSettings> {
    let enabled = entry_value(model, "relocation", "enabled")?;
    if enabled.trim() != "true" {
        return None;
    }
    let tracker = entry_value(model, "relocation.follow", "field").map(|field| TrackerSettings {
        field: as_string(field),
        threshold: entry_value(model, "relocation.follow", "threshold")
            .and_then(as_f64)
            .unwrap_or(0.0),
        fallback_threshold: entry_value(model, "relocation.follow", "fallback_threshold")
            .and_then(as_f64),
        search_margin_cells: entry_value(model, "relocation.follow", "search_margin_cells")
            .and_then(as_i64)
            .unwrap_or(0),
        min_shift_cells: entry_value(model, "relocation.follow", "min_shift_cells")
            .and_then(as_i64)
            .unwrap_or(1),
        max_shift_cells: entry_value(model, "relocation.follow", "max_shift_cells")
            .and_then(as_i64)
            .unwrap_or(1),
        cooldown_seconds: entry_value(model, "relocation.follow", "cooldown_seconds")
            .and_then(as_f64)
            .unwrap_or(0.0),
    });
    let mut moves = Vec::new();
    for index in 0.. {
        let table = format!("relocation.move[{index}]");
        let Some(at) = entry_value(model, &table, "at_seconds").and_then(as_f64) else {
            break;
        };
        moves.push(MoveRow {
            at_seconds: at,
            di_parent_cells: entry_value(model, &table, "di_parent_cells")
                .and_then(as_i64)
                .unwrap_or(0),
            dj_parent_cells: entry_value(model, &table, "dj_parent_cells")
                .and_then(as_i64)
                .unwrap_or(0),
        });
    }
    let source = match (tracker, moves.is_empty()) {
        (Some(tracker), _) => FollowSource::Tracker(tracker),
        (None, false) => FollowSource::Moves(moves),
        (None, true) => FollowSource::None,
    };
    Some(FollowSettings {
        grid_id: entry_value(model, "relocation", "grid_id")
            .and_then(as_i64)
            .unwrap_or(2),
        max_move_parent_cells: entry_value(model, "relocation", "max_move_parent_cells")
            .and_then(as_i64),
        min_overlap_fraction: entry_value(model, "relocation", "min_overlap_fraction")
            .and_then(as_f64),
        cadence_seconds: entry_value(model, "relocation", "cadence_seconds").and_then(as_f64),
        source,
    })
}

/// The `spawn = {…}` declaration on `[[domain]]` number `domain_index`.
pub fn parse_spawn(model: &ConfigModel, domain_index: usize) -> Option<SpawnSettings> {
    let table = format!("domain[{domain_index}]");
    let value = entry_value(model, &table, "spawn")?;
    let inner = value.trim().strip_prefix('{')?.strip_suffix('}')?;
    let mut settings = SpawnSettings {
        trigger: String::new(),
        threshold: None,
        earliest_s: None,
        latest_s: None,
        at_s: None,
        search_box: None,
    };
    // Split top-level commas (the search_box array nests brackets).
    let mut depth = 0i32;
    let mut part = String::new();
    let mut parts = Vec::new();
    for c in inner.chars() {
        match c {
            '[' => depth += 1,
            ']' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(std::mem::take(&mut part));
                continue;
            }
            _ => {}
        }
        part.push(c);
    }
    parts.push(part);
    for part in parts {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "trigger" => settings.trigger = value.trim_matches('"').to_string(),
            "threshold" => settings.threshold = as_f64(value),
            "earliest_s" => settings.earliest_s = as_f64(value),
            "latest_s" => settings.latest_s = as_f64(value),
            "at_s" => settings.at_s = as_f64(value),
            "search_box" => {
                let numbers: Vec<i64> = value
                    .trim_matches(['[', ']'])
                    .split(',')
                    .filter_map(|piece| as_i64(piece.trim()))
                    .collect();
                if numbers.len() == 4 {
                    settings.search_box = Some([numbers[0], numbers[1], numbers[2], numbers[3]]);
                }
            }
            _ => {}
        }
    }
    (!settings.trigger.is_empty()).then_some(settings)
}

// ---------------------------------------------------------------------------
// Write: whole blocks back into the TOML text (comments outside the
// blocks untouched; the engine loader re-ratifies every write).
// ---------------------------------------------------------------------------

/// Is this line a table header opening `name` or a subtable of it?
fn is_table_header_of(line: &str, name: &str) -> bool {
    let trimmed = line.trim();
    let inner = trimmed
        .strip_prefix("[[")
        .and_then(|rest| rest.strip_suffix("]]"))
        .or_else(|| {
            trimmed
                .strip_prefix('[')
                .and_then(|rest| rest.strip_suffix(']'))
        });
    match inner {
        Some(inner) => {
            let inner = inner.trim();
            inner == name || inner.starts_with(&format!("{name}."))
        }
        None => false,
    }
}

fn is_any_table_header(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with('[') && trimmed.ends_with(']')
}

/// Remove `[relocation]`, `[relocation.follow]` and every
/// `[[relocation.move]]` section (header through the line before the
/// next unrelated header).
pub fn strip_relocation(text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut skipping = false;
    for line in text.lines() {
        if is_table_header_of(line, "relocation") {
            skipping = true;
            continue;
        }
        if skipping && is_any_table_header(line) {
            skipping = false;
        }
        if !skipping {
            out.push(line);
        }
    }
    let mut result = out.join("\n");
    while result.ends_with("\n\n") {
        result.pop();
    }
    result.push('\n');
    result
}

fn format_f64(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{value:.1}")
    } else {
        format!("{value}")
    }
}

/// Append the `[relocation]` tables in the 1.8.0 spelling. `None`
/// settings = strip only (feature off).
pub fn write_relocation(text: &str, settings: Option<&FollowSettings>) -> String {
    let mut result = strip_relocation(text);
    let Some(settings) = settings else {
        return result;
    };
    if !result.ends_with('\n') {
        result.push('\n');
    }
    result.push_str("\n[relocation]\nenabled = true\n");
    result.push_str(&format!("grid_id = {}\n", settings.grid_id));
    if let Some(bound) = settings.max_move_parent_cells {
        result.push_str(&format!("max_move_parent_cells = {bound}\n"));
    }
    if let Some(fraction) = settings.min_overlap_fraction {
        result.push_str(&format!(
            "min_overlap_fraction = {}\n",
            format_f64(fraction)
        ));
    }
    if let Some(cadence) = settings.cadence_seconds {
        result.push_str(&format!("cadence_seconds = {}\n", format_f64(cadence)));
    }
    match &settings.source {
        FollowSource::None => {}
        FollowSource::Tracker(tracker) => {
            result.push_str("\n[relocation.follow]\n");
            result.push_str(&format!("field = \"{}\"\n", tracker.field));
            result.push_str(&format!("threshold = {}\n", format_f64(tracker.threshold)));
            if let Some(fallback) = tracker.fallback_threshold {
                result.push_str(&format!("fallback_threshold = {}\n", format_f64(fallback)));
            }
            result.push_str(&format!(
                "search_margin_cells = {}\n",
                tracker.search_margin_cells
            ));
            result.push_str(&format!("min_shift_cells = {}\n", tracker.min_shift_cells));
            result.push_str(&format!("max_shift_cells = {}\n", tracker.max_shift_cells));
            result.push_str(&format!(
                "cooldown_seconds = {}\n",
                format_f64(tracker.cooldown_seconds)
            ));
        }
        FollowSource::Moves(moves) => {
            for row in moves {
                result.push_str("\n[[relocation.move]]\n");
                result.push_str(&format!("at_seconds = {}\n", format_f64(row.at_seconds)));
                result.push_str(&format!("di_parent_cells = {}\n", row.di_parent_cells));
                result.push_str(&format!("dj_parent_cells = {}\n", row.dj_parent_cells));
            }
        }
    }
    result
}

/// Set or remove the `spawn = {…}` line of `[[domain]]` number
/// `domain_index` (0-based order of appearance).
pub fn write_spawn(text: &str, domain_index: usize, settings: Option<&SpawnSettings>) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut domain_seen = 0usize;
    let mut in_target = false;
    let mut inserted = false;
    let spawn_line = settings.map(|settings| {
        let mut parts = vec![format!("trigger = \"{}\"", settings.trigger)];
        if let Some(at) = settings.at_s {
            parts.push(format!("at_s = {}", format_f64(at)));
        }
        if let Some(threshold) = settings.threshold {
            parts.push(format!("threshold = {}", format_f64(threshold)));
        }
        if let Some(earliest) = settings.earliest_s {
            parts.push(format!("earliest_s = {}", format_f64(earliest)));
        }
        if let Some(latest) = settings.latest_s {
            parts.push(format!("latest_s = {}", format_f64(latest)));
        }
        if let Some(search_box) = settings.search_box {
            parts.push(format!(
                "search_box = [{}, {}, {}, {}]",
                search_box[0], search_box[1], search_box[2], search_box[3]
            ));
        }
        format!("spawn = {{ {} }}", parts.join(", "))
    });
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "[[domain]]" {
            // Close out the previous target block without an existing
            // spawn line: insert before leaving it.
            if in_target
                && !inserted
                && let Some(spawn_line) = &spawn_line
            {
                out.push(spawn_line.clone());
                inserted = true;
            }
            in_target = domain_seen == domain_index;
            domain_seen += 1;
            out.push(line.to_string());
            continue;
        }
        if in_target && is_any_table_header(line) {
            if !inserted && let Some(spawn_line) = &spawn_line {
                out.push(spawn_line.clone());
                inserted = true;
            }
            in_target = false;
        }
        if in_target && trimmed.starts_with("spawn =") {
            if let Some(spawn_line) = &spawn_line {
                out.push(spawn_line.clone());
            }
            inserted = true;
            continue; // old line dropped (removal when settings is None)
        }
        out.push(line.to_string());
    }
    if in_target
        && !inserted
        && let Some(spawn_line) = &spawn_line
    {
        out.push(spawn_line.clone());
    }
    let mut result = out.join("\n");
    result.push('\n');
    result
}

/// A short badge for a spawn declaration ("UH ≥ 40 m²/s²", "t+3600 s").
pub fn spawn_badge(trigger: &str, threshold: Option<f64>, at_s: Option<f64>) -> String {
    match trigger {
        "time" => format!(
            "t+{} s",
            at_s.map(|v| format_f64(v)).unwrap_or_else(|| "?".into())
        ),
        "uh" => format!(
            "UH ≥ {} m²/s²",
            threshold.map(format_f64).unwrap_or_else(|| "?".into())
        ),
        "reflectivity" => format!(
            "Z ≥ {} dBZ",
            threshold.map(format_f64).unwrap_or_else(|| "?".into())
        ),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// The cards.
// ---------------------------------------------------------------------------

use eframe::egui;

use crate::kit;
use crate::theme::theme;

#[derive(Debug, Default)]
pub struct StormActions {
    /// `Some(None)` strips the tables; `Some(Some(...))` writes them.
    pub apply_follow: Option<Option<FollowSettings>>,
    /// `(domain_index, None)` removes that domain's spawn declaration.
    pub apply_spawn: Option<(usize, Option<SpawnSettings>)>,
    /// Arm the map's search-box drag for `(domain_index, grid_id)`: the
    /// next drag inside the spawning nest's parent writes
    /// `spawn.search_box` through the same snapping machinery as nest
    /// placement.
    pub draw_search_box: Option<(usize, i64)>,
    pub open_advanced: bool,
}

/// Nested domains in the working config: (domain_index, grid_id).
pub fn nest_domains(model: &ConfigModel) -> Vec<(usize, i64)> {
    let mut nests = Vec::new();
    for index in 0.. {
        let table = format!("domain[{index}]");
        let Some(grid_id) = entry_value(model, &table, "grid_id").and_then(as_i64) else {
            break;
        };
        let parent = entry_value(model, &table, "parent_id")
            .and_then(as_i64)
            .unwrap_or(0);
        if parent != 0 {
            nests.push((index, grid_id));
        }
    }
    nests
}

fn setup_prompt(ui: &mut egui::Ui, feature: &str, actions: &mut StormActions) {
    ui.label(
        egui::RichText::new(format!(
            "{feature} lives in the engine's config tables. Open the \
             Advanced surface (the engine writes the full config) to arm it."
        ))
        .color(theme().text_weak)
        .size(10.5),
    );
    if ui.button("Set up config surface…").clicked() {
        actions.open_advanced = true;
    }
}

fn engine_gate_note(ui: &mut egui::Ui) {
    ui.label(
        egui::RichText::new(
            "1.8 schema, LIVE on this box's released engine (gpuwm 1.8.6) — \
             every edit is ratified by its own `--resolve`; a refusal shows \
             verbatim in the Advanced editor.",
        )
        .color(theme().text_weak)
        .size(10.0),
    );
}

/// What a moving nest MEANS on this route, in one line, from the
/// engine's own decision — never a Studio guess about the chain.
///
/// The wording follows the engine's vocabulary exactly: `delivery`
/// names the mechanism, `statics_corridor` says whether the
/// preparation pays for one, and the size of that payment is the
/// `corridor` block in the Resources strip (host/disk; it adds no
/// VRAM). A `None` return means the card says nothing extra, which is
/// the honest state for a still config.
pub fn moving_nest_line(state: &MovingNest, route: &str) -> Option<(String, bool)> {
    match state {
        MovingNest::Still => None,
        MovingNest::Reported(decision) => {
            let grid = decision
                .relocation_grid_id
                .map(|grid| format!("d{grid:02}"))
                .unwrap_or_else(|| "the named child".to_string());
            let text = match decision.delivery.as_deref() {
                Some("statics_corridor") => format!(
                    "MOVING NEST armed on {grid} · chain {} · the preparation \
                     seals a statics corridor (its size is in Resources, \
                     host/disk only)",
                    decision.chain
                ),
                Some("case_data_ingest") => format!(
                    "MOVING NEST armed on {grid} · chain {} · this route holds \
                     the geography source, so each footprint's statics are \
                     rebuilt at move time — no corridor to prepare",
                    decision.chain
                ),
                other => format!(
                    "MOVING NEST armed on {grid} · chain {} · delivery {}",
                    decision.chain,
                    other.unwrap_or("(unstated)")
                ),
            };
            Some((text, false))
        }
        MovingNest::Refused(sentence) => Some((sentence.clone(), true)),
        MovingNest::EngineSilent if route == "prepared" => Some((
            "MOVING NEST declared, but this engine reports no moving-nest \
             decision for it: on a prepared route Launch is BLOCKED until \
             the engine update lands (ERA5 runs moving nests today). The \
             block clears by itself when --resolve answers with one."
                .to_string(),
            true,
        )),
        MovingNest::EngineSilent => Some((
            "MOVING NEST declared — the experiment route rebuilds each \
             footprint's statics from the config's own geography source at \
             move time."
                .to_string(),
            false,
        )),
    }
}

/// The STORM FOLLOWING card ([relocation] + [relocation.follow] /
/// [[relocation.move]]).
pub fn follow_card_ui(
    ui: &mut egui::Ui,
    model: Option<&ConfigModel>,
    route: &str,
    source: &str,
    moving_nest: &MovingNest,
    actions: &mut StormActions,
) {
    kit::section(ui, "Storm following");
    let Some(model) = model else {
        setup_prompt(ui, "Storm following", actions);
        return;
    };
    // THE ROUTE THE TABLES ARE ABOUT TO RIDE, said before the toggle:
    // the cards used to be written as if every config were an ERA5 one,
    // which is how a follow config reached a prepared chain that could
    // not feed it.
    if let Some((line, alert)) = moving_nest_line(moving_nest, route) {
        ui.add(
            egui::Label::new(
                egui::RichText::new(line)
                    .color(if alert {
                        theme().alert
                    } else {
                        theme().accent_text
                    })
                    .size(10.5),
            )
            .wrap(),
        );
    } else {
        ui.label(
            egui::RichText::new(format!(
                "on {source} · {route} route — turning this on makes d0N a \
                 MOVING nest and the engine decides how its statics travel"
            ))
            .color(theme().text_weak)
            .size(10.0),
        );
    }
    let nests = nest_domains(model);
    let current = parse_follow(model);
    let mut enabled = current.is_some();
    if ui
        .checkbox(&mut enabled, "follow enabled")
        .on_hover_text(
            "[relocation] enabled = true: the named child may relocate in \
             whole parent cells at cycle boundaries",
        )
        .changed()
    {
        actions.apply_follow = Some(if enabled {
            let mut fresh = FollowSettings::default();
            if let Some((_, grid_id)) = nests.first() {
                fresh.grid_id = *grid_id;
            }
            Some(fresh)
        } else {
            None
        });
        return;
    }
    let Some(mut follow) = current else {
        engine_gate_note(ui);
        return;
    };
    if nests.is_empty() {
        ui.colored_label(
            theme().warn,
            "no nested [[domain]] in the config — a follow needs a child to move",
        );
    }
    let mut changed = false;
    kit::row(ui, "Moving nest", |ui| {
        egui::ComboBox::from_id_salt("follow-grid")
            .selected_text(format!("d{:02}", follow.grid_id))
            .show_ui(ui, |ui| {
                for (_, grid_id) in &nests {
                    changed |= ui
                        .selectable_value(&mut follow.grid_id, *grid_id, format!("d{grid_id:02}"))
                        .changed();
                }
            });
    });
    kit::row(ui, "Cadence", |ui| match follow.cadence_seconds {
        None => {
            if ui
                .button("set")
                .on_hover_text("Consult the follow source every N model seconds")
                .clicked()
            {
                follow.cadence_seconds = Some(900.0);
                changed = true;
            }
            ui.label(
                egui::RichText::new("every complete cycle (engine default)")
                    .color(theme().text_weak)
                    .size(10.5),
            );
        }
        Some(cadence) => {
            if ui.button("×").clicked() {
                follow.cadence_seconds = None;
                changed = true;
            } else {
                let mut value = cadence;
                if ui
                    .add(
                        egui::DragValue::new(&mut value)
                            .range(1.0..=86_400.0)
                            .speed(10)
                            .suffix(" s"),
                    )
                    .changed()
                {
                    follow.cadence_seconds = Some(value);
                    changed = true;
                }
            }
        }
    });
    kit::row(ui, "Max move", |ui| {
        let mut bound = follow.max_move_parent_cells.unwrap_or(8);
        if ui
            .add(
                egui::DragValue::new(&mut bound)
                    .range(1..=512)
                    .suffix(" cells"),
            )
            .on_hover_text("Bound on one relocation event, parent cells")
            .changed()
        {
            follow.max_move_parent_cells = Some(bound);
            changed = true;
        }
    });
    kit::row(ui, "Min overlap", |ui| {
        let mut fraction = follow.min_overlap_fraction.unwrap_or(0.5);
        if ui
            .add(
                egui::DragValue::new(&mut fraction)
                    .range(0.0..=1.0)
                    .speed(0.01),
            )
            .on_hover_text("Fraction of the child's cells a move must keep")
            .changed()
        {
            follow.min_overlap_fraction = Some(fraction);
            changed = true;
        }
    });
    // Follow source: tracker / manual itinerary / none.
    let mode = match &follow.source {
        FollowSource::Tracker(_) => 0,
        FollowSource::Moves(_) => 1,
        FollowSource::None => 2,
    };
    kit::row(ui, "Source", |ui| {
        for (index, label, hover) in [
            (
                2usize,
                "none",
                "mechanism only (moves via API/manual driving)",
            ),
            (
                1,
                "itinerary",
                "[[relocation.move]] rows — when, and how far",
            ),
            (
                0,
                "tracker",
                "[relocation.follow] — the storm tracker chooses",
            ),
        ] {
            if ui
                .add(egui::Button::selectable(mode == index, label))
                .on_hover_text(hover)
                .clicked()
                && mode != index
            {
                follow.source = match index {
                    0 => FollowSource::Tracker(TrackerSettings::default()),
                    1 => FollowSource::Moves(vec![MoveRow {
                        at_seconds: 1_800.0,
                        di_parent_cells: 2,
                        dj_parent_cells: 0,
                    }]),
                    _ => FollowSource::None,
                };
                changed = true;
            }
        }
    });
    match &mut follow.source {
        FollowSource::Tracker(tracker) => {
            kit::row(ui, "Tracked field", |ui| {
                for (field, units) in TRACKED_FIELDS.iter().rev() {
                    let selected = tracker.field == *field;
                    if ui
                        .add(egui::Button::selectable(selected, *field))
                        .on_hover_text(format!("threshold in {units}"))
                        .clicked()
                        && !selected
                    {
                        tracker.field = (*field).to_string();
                        // The engine requires fallback_threshold iff uh.
                        tracker.fallback_threshold = (*field == "uh").then_some(40.0);
                        changed = true;
                    }
                }
            });
            let units = TRACKED_FIELDS
                .iter()
                .find(|(field, _)| *field == tracker.field)
                .map(|(_, units)| *units)
                .unwrap_or("");
            kit::row(ui, "Threshold", |ui| {
                ui.label(
                    egui::RichText::new(units)
                        .color(theme().text_weak)
                        .size(10.5),
                );
                changed |= ui
                    .add(egui::DragValue::new(&mut tracker.threshold).speed(1.0))
                    .changed();
            });
            if tracker.field == "uh" {
                kit::row(ui, "Fallback", |ui| {
                    let mut fallback = tracker.fallback_threshold.unwrap_or(40.0);
                    if ui
                        .add(
                            egui::DragValue::new(&mut fallback)
                                .speed(1.0)
                                .suffix(" dBZ"),
                        )
                        .on_hover_text(
                            "Reflectivity handoff for the pre-rotation window \
                             (required by the engine for field = uh)",
                        )
                        .changed()
                    {
                        tracker.fallback_threshold = Some(fallback);
                        changed = true;
                    }
                });
            }
            kit::row(ui, "Dead-band", |ui| {
                ui.label(
                    egui::RichText::new("max")
                        .color(theme().text_weak)
                        .size(10.5),
                );
                changed |= ui
                    .add(egui::DragValue::new(&mut tracker.max_shift_cells).range(1..=512))
                    .changed();
                ui.label(
                    egui::RichText::new("min")
                        .color(theme().text_weak)
                        .size(10.5),
                );
                changed |= ui
                    .add(egui::DragValue::new(&mut tracker.min_shift_cells).range(1..=512))
                    .on_hover_text("Shifts below this are never proposed (kills chatter)")
                    .changed();
            });
            kit::row(ui, "Cooldown", |ui| {
                changed |= ui
                    .add(
                        egui::DragValue::new(&mut tracker.cooldown_seconds)
                            .range(0.0..=86_400.0)
                            .speed(10)
                            .suffix(" s"),
                    )
                    .changed();
            });
            kit::row(ui, "Search margin", |ui| {
                changed |= ui
                    .add(
                        egui::DragValue::new(&mut tracker.search_margin_cells)
                            .range(0..=512)
                            .suffix(" cells"),
                    )
                    .changed();
            });
        }
        FollowSource::Moves(moves) => {
            let mut remove: Option<usize> = None;
            for (index, row) in moves.iter_mut().enumerate() {
                kit::row(ui, &format!("move {}", index + 1), |ui| {
                    if ui.button("×").clicked() {
                        remove = Some(index);
                    }
                    ui.label(
                        egui::RichText::new("dj")
                            .color(theme().text_weak)
                            .size(10.0),
                    );
                    changed |= ui
                        .add(egui::DragValue::new(&mut row.dj_parent_cells).range(-512..=512))
                        .changed();
                    ui.label(
                        egui::RichText::new("di")
                            .color(theme().text_weak)
                            .size(10.0),
                    );
                    changed |= ui
                        .add(egui::DragValue::new(&mut row.di_parent_cells).range(-512..=512))
                        .changed();
                    changed |= ui
                        .add(
                            egui::DragValue::new(&mut row.at_seconds)
                                .range(1.0..=1e7)
                                .speed(60)
                                .suffix(" s"),
                        )
                        .changed();
                });
            }
            if let Some(index) = remove {
                moves.remove(index);
                changed = true;
            }
            if ui.button("+ Add move").clicked() {
                let at = moves
                    .last()
                    .map(|row| row.at_seconds + 1_800.0)
                    .unwrap_or(1_800.0);
                moves.push(MoveRow {
                    at_seconds: at,
                    di_parent_cells: 2,
                    dj_parent_cells: 0,
                });
                changed = true;
            }
        }
        FollowSource::None => {}
    }
    engine_gate_note(ui);
    if changed {
        actions.apply_follow = Some(Some(follow));
    }
}

/// The DORMANT SPAWN card (`spawn = {…}` per nested [[domain]]).
pub fn spawn_card_ui(ui: &mut egui::Ui, model: Option<&ConfigModel>, actions: &mut StormActions) {
    kit::section(ui, "Dormant spawn");
    let Some(model) = model else {
        setup_prompt(ui, "Spawn-on-trigger", actions);
        return;
    };
    let nests = nest_domains(model);
    if nests.is_empty() {
        kit::status_line(ui, "add a nest — a spawn materializes a CHILD domain");
        return;
    }
    for (domain_index, grid_id) in nests {
        let current = parse_spawn(model, domain_index);
        let mut settings = current.clone();
        let mut changed = false;
        kit::row(ui, &format!("d{grid_id:02}"), |ui| {
            for trigger in SPAWN_TRIGGERS.iter().rev() {
                let selected = settings
                    .as_ref()
                    .map(|value| value.trigger == *trigger)
                    .unwrap_or(false);
                if ui
                    .add(egui::Button::selectable(selected, *trigger))
                    .on_hover_text(match *trigger {
                        "time" => "manual spawn at a fixed model time (at_s)",
                        "uh" => "spawn when updraft helicity exceeds the threshold",
                        _ => "spawn when composite reflectivity exceeds the threshold",
                    })
                    .clicked()
                    && !selected
                {
                    settings = Some(match *trigger {
                        "time" => SpawnSettings {
                            trigger: "time".into(),
                            threshold: None,
                            earliest_s: None,
                            latest_s: None,
                            at_s: Some(3_600.0),
                            search_box: None,
                        },
                        other => SpawnSettings {
                            trigger: other.into(),
                            ..SpawnSettings::default()
                        },
                    });
                    changed = true;
                }
            }
            let live = settings.is_none();
            if ui
                .add(egui::Button::selectable(live, "live"))
                .on_hover_text("Ordinary domain, integrating from t = 0")
                .clicked()
                && !live
            {
                settings = None;
                changed = true;
            }
        });
        if let Some(spawn) = &mut settings {
            if spawn.trigger == "time" {
                kit::row(ui, "Spawn at", |ui| {
                    let mut at = spawn.at_s.unwrap_or(3_600.0);
                    if ui
                        .add(
                            egui::DragValue::new(&mut at)
                                .range(0.0..=1e7)
                                .speed(60)
                                .suffix(" s"),
                        )
                        .changed()
                    {
                        spawn.at_s = Some(at);
                        changed = true;
                    }
                });
            } else {
                let units = if spawn.trigger == "uh" {
                    "m²/s²"
                } else {
                    "dBZ"
                };
                kit::row(ui, "Threshold", |ui| {
                    ui.label(
                        egui::RichText::new(units)
                            .color(theme().text_weak)
                            .size(10.5),
                    );
                    let mut threshold = spawn.threshold.unwrap_or(40.0);
                    if ui
                        .add(egui::DragValue::new(&mut threshold).speed(1.0))
                        .changed()
                    {
                        spawn.threshold = Some(threshold);
                        changed = true;
                    }
                });
                kit::row(ui, "Window", |ui| {
                    let mut latest = spawn.latest_s.unwrap_or(21_600.0);
                    if ui
                        .add(
                            egui::DragValue::new(&mut latest)
                                .range(1.0..=1e7)
                                .speed(60)
                                .suffix(" s"),
                        )
                        .on_hover_text("latest_s — the window closing unfired is an honest outcome")
                        .changed()
                    {
                        spawn.latest_s = Some(latest);
                        changed = true;
                    }
                    ui.label(egui::RichText::new("→").color(theme().text_weak));
                    let mut earliest = spawn.earliest_s.unwrap_or(3_600.0);
                    if ui
                        .add(
                            egui::DragValue::new(&mut earliest)
                                .range(0.0..=1e7)
                                .speed(60)
                                .suffix(" s"),
                        )
                        .on_hover_text("earliest_s")
                        .changed()
                    {
                        spawn.earliest_s = Some(earliest);
                        changed = true;
                    }
                });
                kit::row(ui, "Search box", |ui| {
                    if ui
                        .button("draw on map")
                        .on_hover_text(
                            "Drag a box inside this nest's parent on the map — \
                             snapped to whole parent cells, written as \
                             spawn.search_box (Esc cancels)",
                        )
                        .clicked()
                    {
                        actions.draw_search_box = Some((domain_index, grid_id));
                    }
                    match spawn.search_box {
                        None => {
                            if ui
                                .button("set")
                                .on_hover_text(
                                    "1-based inclusive parent-cell bounds \
                                     [i_lo, j_lo, i_hi, j_hi]; unset defers to \
                                     the declared footprint / the whole parent",
                                )
                                .clicked()
                            {
                                spawn.search_box = Some([60, 60, 240, 190]);
                                changed = true;
                            }
                            ui.label(
                                egui::RichText::new("engine default region")
                                    .color(theme().text_weak)
                                    .size(10.5),
                            );
                        }
                        Some(mut search_box) => {
                            if ui.button("×").clicked() {
                                spawn.search_box = None;
                                changed = true;
                            } else {
                                let mut edited = false;
                                for value in search_box.iter_mut().rev() {
                                    edited |= ui
                                        .add(egui::DragValue::new(value).range(1..=4096))
                                        .changed();
                                }
                                if edited {
                                    spawn.search_box = Some(search_box);
                                    changed = true;
                                }
                            }
                        }
                    }
                });
            }
            kit::status_line(
                ui,
                &format!(
                    "badge: {}",
                    spawn_badge(&spawn.trigger, spawn.threshold, spawn.at_s)
                ),
            );
        }
        if changed {
            actions.apply_spawn = Some((domain_index, settings.clone()));
        }
    }
    engine_gate_note(ui);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures")
                .join(name),
        )
        .unwrap()
    }

    /// A BOUNDS-ONLY `[relocation]` IS NOT A MOVING NEST, and treating
    /// it as one would refuse a runnable config. The engine's predicate
    /// requires a follow SOURCE — a tracker or an itinerary — and
    /// Studio's copy has to agree with it in both directions, or the
    /// prepared-route row blocks configs the engine is happy to run.
    #[test]
    fn the_follow_predicate_matches_the_engines_and_bounds_only_is_still() {
        let base = "[experiment]\nname = \"x\"\n\n[[domain]]\ngrid_id = 1\n";

        let bounds_only = ConfigModel::parse(&format!(
            "{base}\n[relocation]\nenabled = true\ngrid_id = 2\n\
             max_move_parent_cells = 8\n"
        ));
        assert!(parse_follow(&bounds_only).is_some(), "the table IS there");
        assert_eq!(
            declares_follow_source(&bounds_only),
            None,
            "enabled bounds are not a moving nest — the nest stays put"
        );
        assert_eq!(
            MovingNest::read(Some(&bounds_only), None),
            MovingNest::Still,
            "and a still config is never gated, whatever the route"
        );

        // A tracker and an itinerary each make it a moving nest.
        let tracker = write_relocation(base, Some(&FollowSettings::default()));
        assert_eq!(
            declares_follow_source(&ConfigModel::parse(&tracker)),
            Some("tracker")
        );
        let itinerary = write_relocation(
            base,
            Some(&FollowSettings {
                source: FollowSource::Moves(vec![MoveRow {
                    at_seconds: 1_800.0,
                    di_parent_cells: 2,
                    dj_parent_cells: 0,
                }]),
                ..FollowSettings::default()
            }),
        );
        let itinerary = ConfigModel::parse(&itinerary);
        assert_eq!(declares_follow_source(&itinerary), Some("itinerary"));

        // No engine answer yet for a declared moving nest = SILENT, the
        // fail-safe state the prepared-route row blocks on.
        assert_eq!(
            MovingNest::read(Some(&itinerary), None),
            MovingNest::EngineSilent
        );
        // A refusal that is NOT about the moving nest leaves the state
        // unknown rather than putting another stage's sentence on the
        // moving-nest row.
        let placement: Result<Box<arwen_plan::queries::ResolveReport>, String> =
            Err("west-east high clearance is -33 parent rows (grid_id = 2)".into());
        assert_eq!(
            MovingNest::read(Some(&itinerary), Some(&placement)),
            MovingNest::EngineSilent
        );
        // The engine's own moving-nest refusal, transport wrapper and
        // all, comes back as the ENGINE's sentence.
        let refusal: Result<Box<arwen_plan::queries::ResolveReport>, String> = Err(
            "query exited with exit code: 2: gpuwm run-plan: this plan's \
             config declares a [relocation] follow source on d02, and the \
             'prepared:hrrr' chain cannot supply the statics a moving nest \
             needs"
                .into(),
        );
        match MovingNest::read(Some(&itinerary), Some(&refusal)) {
            MovingNest::Refused(sentence) => {
                assert!(
                    sentence.starts_with("this plan's config declares"),
                    "{sentence}"
                );
                assert!(!sentence.contains("query exited"), "{sentence}");
            }
            other => panic!("{other:?}"),
        }
    }

    /// The 1.8.0 tree's OWN worked follow config parses into the card
    /// state (cross-lane fixture: engine-authored bytes).
    #[test]
    fn engine_follow_config_parses_into_card_state() {
        let model = ConfigModel::parse(&fixture("follow-2km-180.toml"));
        let follow = parse_follow(&model).expect("[relocation] enabled");
        assert_eq!(follow.grid_id, 2);
        assert_eq!(follow.max_move_parent_cells, Some(8));
        assert_eq!(follow.min_overlap_fraction, Some(0.5));
        assert_eq!(follow.cadence_seconds, Some(900.0));
        match &follow.source {
            FollowSource::Tracker(tracker) => {
                assert_eq!(tracker.field, "uh");
                assert_eq!(tracker.threshold, 25.0);
                assert_eq!(tracker.fallback_threshold, Some(40.0));
                assert_eq!(tracker.min_shift_cells, 2);
                assert_eq!(tracker.max_shift_cells, 8);
                assert_eq!(tracker.cooldown_seconds, 900.0);
            }
            other => panic!("expected tracker, got {other:?}"),
        }
    }

    /// The 1.8.0 tree's spawn config parses (inline table on [[domain]]).
    #[test]
    fn engine_spawn_config_parses_into_card_state() {
        let text = fixture("spawn-2km-180.toml");
        let model = ConfigModel::parse(&text);
        // The dormant nest is the second [[domain]] in that file.
        let spawn = parse_spawn(&model, 1).expect("spawn declared");
        assert_eq!(spawn.trigger, "uh");
        assert_eq!(spawn.threshold, Some(40.0));
        assert_eq!(spawn.earliest_s, Some(3_600.0));
        assert_eq!(spawn.latest_s, Some(36_000.0));
        assert_eq!(spawn.search_box, Some([60, 60, 240, 190]));
        assert_eq!(spawn.at_s, None);
        assert_eq!(spawn_badge("uh", spawn.threshold, None), "UH ≥ 40.0 m²/s²");
    }

    /// Round trip: write the card state into a plain config, parse it
    /// back identically; strip removes every trace.
    #[test]
    fn follow_write_parse_round_trips_and_strip_removes() {
        let base =
            "[experiment]\nname = \"n\"\n\n[[domain]]\ngrid_id = 1\n\n[[domain]]\ngrid_id = 2\n";
        let settings = FollowSettings::default();
        let written = write_relocation(base, Some(&settings));
        assert!(written.contains("[relocation]"));
        assert!(written.contains("[relocation.follow]"));
        let model = ConfigModel::parse(&written);
        assert_eq!(parse_follow(&model), Some(settings.clone()));
        // Manual itinerary round trip.
        let manual = FollowSettings {
            source: FollowSource::Moves(vec![
                MoveRow {
                    at_seconds: 1800.0,
                    di_parent_cells: 3,
                    dj_parent_cells: -1,
                },
                MoveRow {
                    at_seconds: 3600.0,
                    di_parent_cells: 2,
                    dj_parent_cells: 0,
                },
            ]),
            ..FollowSettings::default()
        };
        let written = write_relocation(base, Some(&manual));
        assert_eq!(written.matches("[[relocation.move]]").count(), 2);
        let model = ConfigModel::parse(&written);
        assert_eq!(parse_follow(&model), Some(manual));
        // Strip removes everything, leaving the rest byte-stable.
        let stripped = write_relocation(&written, None);
        assert!(!stripped.contains("relocation"));
        assert!(stripped.contains("[experiment]"));
        assert!(stripped.contains("grid_id = 2"));
    }

    /// Emit a full config through the card writers for EXTERNAL loader
    /// validation (run by hand; the 1.8.0 tree's real loader is the
    /// judge — see the live check in the work log).
    #[test]
    #[ignore]
    fn emit_sample_for_external_loader_validation() {
        let base = fixture("generated-config.toml");
        let with_follow = write_relocation(&base, Some(&FollowSettings::default()));
        let with_spawn = write_spawn(&with_follow, 1, Some(&SpawnSettings::default()));
        let path = std::env::temp_dir().join("arwen-storm-emitted.toml");
        std::fs::write(&path, &with_spawn).unwrap();
        eprintln!("emitted: {}", path.display());
    }

    #[test]
    fn spawn_write_parse_round_trips_on_the_second_domain() {
        let base = "[[domain]]\ngrid_id = 1\nnx = 100\n\n[[domain]]\ngrid_id = 2\nnx = 200\n\n[fetch]\nsource = \"era5\"\n";
        let settings = SpawnSettings::default();
        let written = write_spawn(base, 1, Some(&settings));
        assert_eq!(written.matches("spawn = {").count(), 1);
        // It landed in the SECOND domain block (before [fetch]).
        let fetch_at = written.find("[fetch]").unwrap();
        let spawn_at = written.find("spawn = {").unwrap();
        let d2_at = written.find("grid_id = 2").unwrap();
        assert!(spawn_at > d2_at && spawn_at < fetch_at);
        let model = ConfigModel::parse(&written);
        assert_eq!(parse_spawn(&model, 1), Some(settings));
        assert_eq!(parse_spawn(&model, 0), None);
        // Replacing updates in place; None removes.
        let time_spawn = SpawnSettings {
            trigger: "time".into(),
            threshold: None,
            earliest_s: None,
            latest_s: None,
            at_s: Some(7200.0),
            search_box: None,
        };
        let replaced = write_spawn(&written, 1, Some(&time_spawn));
        assert_eq!(replaced.matches("spawn = {").count(), 1);
        let model = ConfigModel::parse(&replaced);
        assert_eq!(parse_spawn(&model, 1), Some(time_spawn));
        let removed = write_spawn(&replaced, 1, None);
        assert!(!removed.contains("spawn = {"));
        assert!(removed.contains("nx = 200"));
    }
}
