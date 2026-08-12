// SPDX-License-Identifier: Apache-2.0

//! The right context inspector: intent cards over the draft, the fixed
//! resource strip fed by `--estimate`, the review sheet fed by
//! `--resolve` (every automatic resolution rendered — nothing the engine
//! decides is invisible), the run panel for an active session, the runs
//! list, and the system view.

use eframe::egui;

use arwen_plan::queries::{EstimateReport, ProbeReport, ResolveReport};
use arwen_proc::registry::RunEntry;

use crate::draft::{
    Draft, LADDER_PRESETS, LENGTH_PRESETS, OUTPUT_PRESETS_S, PHYSICS_PROFILES, RESOLUTION_PRESETS,
    SOURCE_PRESETS,
};
use crate::kit;
use crate::run_session::{RunSession, StageStatus, Terminal};
use crate::theme::{maturity_color, theme};

/// What the inspector asks the app to do this frame.
#[derive(Debug, Default)]
pub struct InspectorActions {
    pub open_review: bool,
    pub launch: bool,
    pub close_review: bool,
    pub open_run: Option<usize>,
    pub back_to_design: bool,
    pub refresh_probe: bool,
    pub select_frame: Option<Option<usize>>,
    /// Persist the edited settings and rebuild the contract source.
    pub save_settings: bool,
    /// Open the Advanced config surface (engine-generated TOML editor).
    pub open_advanced: bool,
    /// Open the render-product picker (engine catalog).
    pub open_products: bool,
    /// Write the entered CDS key to ~/.cdsapirc (consented in the UI).
    pub save_cds_key: bool,
    /// Reset one nest's placement keys to the engine's fitted emission
    /// (the "manual" chip's one-click undo). Value = grid_id.
    pub reset_placement: Option<u32>,
    /// Select this domain on the map (inspector row click).
    pub select_nest: Option<u32>,
    /// Typed nx × ny for one domain (grid_id, nx, ny) — rides the same
    /// placement writer as a map resize.
    pub set_domain_size: Option<(u32, u32, u32)>,
    /// The blocked Run button's one-click remedy: back to a single
    /// domain (the surface follows the ladder card by itself).
    pub make_single_domain: bool,
    /// Storm-following / spawn card edits (whole-block config writes).
    pub storm: crate::storm::StormActions,
}

pub struct ReviewSheet {
    pub report: ResolveReport,
    pub plan_json: String,
    /// The ENGINE-fitted root-domain geometry from the resolved
    /// configuration (there is no nx/ny input — the sketch is intent,
    /// this is the answer, and the map draws it).
    pub fitted: Option<arwen_map::LambertDomain>,
    /// The whole fitted tree, parent-before-child (nests drawn on the
    /// map from these engine placements).
    pub tree: Vec<arwen_plan::queries::ResolvedDomain>,
}

/// The line of the engine's refusal that mentions one of `needles` —
/// the engine's own sentence, verbatim, PLACED at the field it names.
/// The full text always remains visible in the resource strip and on
/// hover; this selects for placement, it never rewrites.
fn refusal_line_for<'e>(
    estimate: Option<&'e Result<EstimateReport, String>>,
    needles: &[&str],
) -> Option<&'e str> {
    let Some(Err(error)) = estimate else {
        return None;
    };
    error
        .lines()
        .find(|line| needles.iter().any(|needle| line.contains(needle)))
}

/// Render an engine refusal inline at the offending field.
fn inline_refusal(
    ui: &mut egui::Ui,
    estimate: Option<&Result<EstimateReport, String>>,
    needles: &[&str],
) {
    if let Some(line) = refusal_line_for(estimate, needles) {
        ui.add(egui::Label::new(egui::RichText::new(line).color(theme().alert).size(10.5)).wrap())
            .on_hover_text(line);
    }
}

/// An optional numeric value: "engine default" until overridden. The
/// default's VALUE is the engine's to report (resolutions/hover), never
/// restated as if Studio owned it.
fn optional_value_row(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut Option<f64>,
    default_hint: &str,
    range: std::ops::RangeInclusive<f64>,
    speed: f64,
    suffix: &str,
) {
    kit::row(ui, label, |ui| match value {
        None => {
            if ui
                .button("override")
                .on_hover_text(format!("Currently {default_hint}; click to set a value"))
                .clicked()
            {
                *value = Some(*range.start());
            }
            ui.label(
                egui::RichText::new(default_hint)
                    .color(theme().text_weak)
                    .size(10.5),
            );
        }
        Some(inner) => {
            if ui
                .button("×")
                .on_hover_text(format!("Back to {default_hint}"))
                .clicked()
            {
                *value = None;
                return;
            }
            let mut current = *inner;
            if ui
                .add(
                    egui::DragValue::new(&mut current)
                        .range(range)
                        .speed(speed)
                        .suffix(suffix),
                )
                .changed()
            {
                *value = Some(current);
            }
        }
    });
}

/// The design-time inspector (New view).
#[allow(clippy::too_many_arguments)]
pub fn design_ui(
    ui: &mut egui::Ui,
    draft: &mut Draft,
    probe: Option<&ProbeReport>,
    estimate: Option<&Result<EstimateReport, String>>,
    estimate_pending: bool,
    estimate_stale: bool,
    review: Option<&ReviewSheet>,
    favorite_products: &[String],
    advanced: Option<&crate::advanced::AdvancedState>,
    tree: &[arwen_plan::queries::ResolvedDomain],
    tree_provisional: bool,
    selected_nest: Option<u32>,
    actions: &mut InspectorActions,
) {
    let advanced_model = advanced.map(|state| &state.model);
    // The moving-nest decision for the ACTIVE config, read once: the
    // follow card renders it and the prepared-route row gates on it, and
    // they must never disagree about what the engine said.
    let moving_nest = crate::storm::MovingNest::read(
        advanced_model,
        advanced.and_then(|state| state.resolve.as_ref()),
    );
    kit::section(ui, "Forecast");
    kit::row(ui, "Name", |ui| {
        ui.add(egui::TextEdit::singleline(&mut draft.name).desired_width(ui.available_width()));
    });

    if draft.custom.is_some() {
        ui.add_space(4.0);
        ui.add(
            egui::Label::new(
                egui::RichText::new(
                    "CUSTOM CONFIG ACTIVE — the run submits your edited TOML \
                     (config.path). The intent cards below feed Regenerate \
                     only; open the editor to change the active values.",
                )
                .color(theme().warn)
                .size(10.5),
            )
            .wrap(),
        );
        if ui.button("Open advanced editor").clicked() {
            actions.open_advanced = true;
        }
    }

    kit::section(ui, "Source");
    let chips: Vec<kit::Chip> = SOURCE_PRESETS
        .iter()
        .enumerate()
        .map(|(index, preset)| kit::Chip {
            label: preset.label,
            selected: index == draft.source_index,
            hover: Some(format!(
                "{} · {} route · {} · {}",
                preset.source, preset.route, preset.maturity, preset.note
            )),
            dot: Some(maturity_color(preset.maturity)),
        })
        .collect();
    if let Some(clicked) = kit::chip_grid(ui, &chips) {
        draft.source_index = clicked;
    }
    kit::status_line(ui, draft.source().note);
    if draft.source().source == "era5" {
        if crate::settings::cds_key_present() {
            ui.colored_label(
                theme().live,
                "CDS key present (~/.cdsapirc) — passed to the sealed engine",
            );
        } else {
            ui.colored_label(
                theme().warn,
                "no CDS key — set it under Sys; ERA5 acquisition needs one",
            );
        }
    }
    kit::row(ui, "Cycle", |ui| {
        let latest = draft.cycle.is_none();
        if ui
            .add(egui::Button::selectable(latest, "Latest"))
            .on_hover_text(
                "Newest COMPLETE cycle, resolved by the engine before the \
                 fetch — the concrete cycle it chose shows here and in the \
                 review's automatic resolutions",
            )
            .clicked()
        {
            draft.cycle = None;
        }
        if ui
            .add(egui::Button::selectable(!latest, "Pinned"))
            .on_hover_text("Pin a date + UTC hour (YYYY-MM-DDTHH, the wizard's spelling)")
            .clicked()
            && draft.cycle.is_none()
        {
            // Default pin: most recent synoptic hour.
            let now = chrono::Utc::now().naive_utc();
            let hour = (now.hour() / 6) * 6;
            draft.cycle = now.date().and_hms_opt(hour, 0, 0);
        }
    });
    match draft.cycle {
        Some(cycle) => {
            // The date + hour picker. Every field free; the ENGINE is the
            // availability authority (an unavailable cycle gets the
            // engine's own refusal — at review, or at the fetch stage
            // verbatim in the run panel).
            ui.horizontal(|ui| {
                let (mut year, mut month, mut day, mut hour) =
                    (cycle.year(), cycle.month(), cycle.day(), cycle.hour());
                let mut changed = false;
                changed |= ui
                    .add(
                        egui::DragValue::new(&mut year)
                            .range(1940..=2100)
                            .speed(0.05),
                    )
                    .on_hover_text("Year (UTC)")
                    .changed();
                changed |= ui
                    .add(egui::DragValue::new(&mut month).range(1..=12).speed(0.05))
                    .on_hover_text("Month")
                    .changed();
                changed |= ui
                    .add(egui::DragValue::new(&mut day).range(1..=31).speed(0.05))
                    .on_hover_text("Day")
                    .changed();
                changed |= ui
                    .add(
                        egui::DragValue::new(&mut hour)
                            .range(0..=23)
                            .speed(0.05)
                            .suffix("Z"),
                    )
                    .on_hover_text(
                        "UTC cycle hour (GFS publishes 00/06/12/18, HRRR hourly, \
                         ERA5 any — the engine refuses what a source cannot serve)",
                    )
                    .changed();
                if changed
                    && let Some(date) = chrono::NaiveDate::from_ymd_opt(year, month, day)
                    && let Some(new_cycle) = date.and_hms_opt(hour, 0, 0)
                {
                    draft.cycle = Some(new_cycle);
                }
                for (label, hours) in [("−6h", -6i64), ("+6h", 6i64)] {
                    if ui.button(label).clicked() {
                        draft.cycle = Some(cycle + chrono::Duration::hours(hours));
                    }
                }
            });
            let availability = match draft.source().source {
                "era5" => {
                    "ERA5 is archival: any historical date, published days \
                     behind real time; needs the CDS key (Sys panel)"
                }
                _ => {
                    "GFS/HRRR mirrors keep a retention window of recent \
                     cycles; an out-of-window cycle is refused by the engine \
                     at the fetch stage — the sentence lands verbatim in the \
                     run panel"
                }
            };
            kit::status_line(ui, availability);
        }
        None => {
            if draft.source().source == "era5" {
                ui.colored_label(
                    theme().warn,
                    "ERA5 has no 'latest' (reanalysis, days late) — the engine \
                     refuses it by name; pin a date above",
                );
            } else if let Some(review) = review {
                // The CONCRETE cycle the engine chose for `latest`, from
                // automatic_resolutions — plus its not-the-newest warning
                // when the engine emitted one.
                if let Some(resolution) = review
                    .report
                    .automatic_resolutions
                    .iter()
                    .find(|resolution| resolution.scope == "fetch" && resolution.key == "cycle")
                {
                    let value = match &resolution.value {
                        serde_json::Value::String(text) => text.clone(),
                        other => other.to_string(),
                    };
                    let mut response = ui.label(
                        egui::RichText::new(format!("engine resolved latest → {value}"))
                            .color(theme().accent_text)
                            .size(11.0),
                    );
                    response = response.on_hover_text(&resolution.basis);
                    if let Some(note) = &resolution.note {
                        response.on_hover_text(note);
                    }
                }
                for warning in &review.report.warnings {
                    let text = warning.to_string();
                    if text.contains("latest_cycle_is_not_the_newest") {
                        ui.colored_label(theme().warn, text);
                    }
                }
            }
        }
    }
    inline_refusal(ui, estimate, &["cycle", "latest"]);

    kit::section(ui, "Schedule");
    kit::row(ui, "Length", |ui| {
        // Quick-picks BESIDE the free field (right-to-left layout).
        for hours in LENGTH_PRESETS.iter().rev() {
            let selected = (draft.length_hours - hours).abs() < 1e-9;
            if ui
                .add(egui::Button::selectable(selected, format!("{hours:.0}")))
                .on_hover_text(format!("{hours} hours"))
                .clicked()
            {
                draft.length_hours = *hours;
            }
        }
        ui.add(
            egui::DragValue::new(&mut draft.length_hours)
                .range(0.5..=240.0)
                .speed(0.25)
                .suffix(" h"),
        )
        .on_hover_text("Any length from 30 min to 240 h — free value, not a menu");
    });
    kit::row(ui, "Output every", |ui| {
        for seconds in OUTPUT_PRESETS_S.iter().rev() {
            let selected = draft.history_interval_s == *seconds;
            if ui
                .add(egui::Button::selectable(
                    selected,
                    format!("{}m", seconds / 60),
                ))
                .on_hover_text(format!("{seconds} s"))
                .clicked()
            {
                draft.history_interval_s = *seconds;
            }
        }
        let mut seconds = draft.history_interval_s;
        if ui
            .add(
                egui::DragValue::new(&mut seconds)
                    .range(1..=86_400)
                    .speed(10)
                    .suffix(" s"),
            )
            .on_hover_text(
                "ANY whole number of seconds. The engine requires a whole \
                 number of the domain's time steps and refuses anything else \
                 with its own sentence — shown here.",
            )
            .changed()
        {
            draft.history_interval_s = seconds;
        }
    });
    inline_refusal(ui, estimate, &["history_interval"]);

    kit::section(ui, "Grid");
    kit::row(ui, "Root dx", |ui| {
        for preset in RESOLUTION_PRESETS.iter().rev() {
            let selected = (draft.root_dx_km - preset.dx_km).abs() < 1e-9;
            let mut response = ui.add(egui::Button::selectable(selected, preset.label));
            let center = egui::pos2(response.rect.left() + 6.0, response.rect.center().y);
            ui.painter()
                .circle_filled(center, 2.0, maturity_color(preset.maturity));
            response =
                response.on_hover_text(format!("dx = {} km · {}", preset.dx_km, preset.maturity));
            if response.clicked() {
                draft.set_root_dx_km(preset.dx_km);
            }
        }
        let mut dx = draft.root_dx_km;
        if ui
            .add(
                egui::DragValue::new(&mut dx)
                    .range(0.05..=1_000.0)
                    .speed(0.1)
                    .suffix(" km"),
            )
            .on_hover_text(
                "Free value — the engine's wizard validates its own bracket \
                 and refuses with its own sentence",
            )
            .changed()
        {
            draft.set_root_dx_km(dx);
        }
    });
    inline_refusal(ui, estimate, &["root-dx", "root_dx"]);

    kit::section(ui, "Nests");
    let ladder_chips: Vec<kit::Chip> = LADDER_PRESETS
        .iter()
        .map(|(label, root_dx, ratios)| kit::Chip {
            label,
            selected: (draft.root_dx_km - root_dx).abs() < 1e-9 && draft.nests == *ratios,
            hover: Some(if ratios.is_empty() {
                format!("one domain at {root_dx} km")
            } else {
                format!(
                    "root {root_dx} km, ratios {}",
                    ratios
                        .iter()
                        .map(|ratio| ratio.to_string())
                        .collect::<Vec<_>>()
                        .join("·")
                )
            }),
            dot: None,
        })
        .collect();
    if let Some(clicked) = kit::chip_grid(ui, &ladder_chips) {
        let (_, root_dx, ratios) = LADDER_PRESETS[clicked];
        draft.set_root_dx_km(root_dx);
        draft.nests = ratios.to_vec();
        draft.selected_domain = draft.selected_domain.min(draft.nests.len());
    }
    let mut remove: Option<usize> = None;
    let chain_km = draft.dx_chain_km();
    for (index, ratio) in draft.nests.iter_mut().enumerate() {
        kit::row(ui, &format!("d{:02} ratio", index + 2), |ui| {
            if ui
                .button("×")
                .on_hover_text("Remove this nest (and everything inside it)")
                .clicked()
            {
                remove = Some(index);
            }
            ui.label(
                egui::RichText::new(format!(
                    "→ {} — derived ÷{ratio}",
                    format_dx_km(chain_km[index + 1])
                ))
                .color(theme().text_weak)
                .size(10.5),
            )
            .on_hover_text(
                "Child dx/dt derive exactly from the parent chain — engine \
                 outputs, never hand-typed",
            );
            ui.add(egui::DragValue::new(ratio).range(2..=12).speed(0.05))
                .on_hover_text("Refinement ratio vs the parent domain (any integer ≥ 2)");
        });
    }
    if let Some(index) = remove {
        draft.nests.truncate(index);
        draft.selected_domain = draft.selected_domain.min(draft.nests.len());
    }
    ui.horizontal(|ui| {
        if ui
            .button("+ Add nest")
            .on_hover_text(
                "Adds a child domain one level deeper (default ratio 3). \
                 The engine fits its size to the VRAM budget and centers it \
                 on the drawn point; exact placement lives in the resolved \
                 plan.",
            )
            .clicked()
        {
            draft.nests.push(3);
        }
        if !draft.nests.is_empty() {
            kit::status_line(
                ui,
                &chain_km
                    .iter()
                    .map(|dx| format_dx_km(*dx))
                    .collect::<Vec<_>>()
                    .join(" › "),
            );
        }
    });
    // PER-DOMAIN rows — root and every child: click the id chip to
    // select it on the map; nx × ny are typed here; the auto(fitted)/manual chip and
    // one-click reset ride the same placement writer as a map drag.
    domain_rows(
        ui,
        draft,
        advanced,
        tree,
        tree_provisional,
        selected_nest,
        actions,
    );
    if !draft.nests.is_empty() {
        kit::row(ui, "Nest output", |ui| {
            match draft.nest_history_interval_s {
                None => {
                    if ui
                        .button("override")
                        .on_hover_text("Set one cadence for every nest (free seconds value)")
                        .clicked()
                    {
                        draft.nest_history_interval_s = Some(300);
                    }
                    ui.label(
                        egui::RichText::new("↳ inherits engine default (900 s)")
                            .color(theme().text_weak)
                            .size(10.5),
                    )
                    .on_hover_text(
                        "Nests write more often than the root by default — \
                         the wizard's own 900 s. Override for any value.",
                    );
                }
                Some(seconds) => {
                    if ui
                        .button("×")
                        .on_hover_text("Back to the inherited engine default")
                        .clicked()
                    {
                        draft.nest_history_interval_s = None;
                        return;
                    }
                    ui.label(
                        egui::RichText::new("↳ overrides every nest")
                            .color(theme().accent_text)
                            .size(10.5),
                    );
                    let mut value = seconds;
                    if ui
                        .add(
                            egui::DragValue::new(&mut value)
                                .range(1..=86_400)
                                .speed(10)
                                .suffix(" s"),
                        )
                        .changed()
                    {
                        draft.nest_history_interval_s = Some(value);
                    }
                }
            }
        });
        inline_refusal(
            ui,
            estimate,
            &["nest_history_interval", "nest-history-interval"],
        );
    }

    // Storm-following + dormant-spawn cards (1.8 config surface; state
    // parsed from and written into the Advanced surface's TOML). The
    // follow card is ROUTE-AWARE: the same tables mean a corridor on
    // the GFS prepared chain, live ingest on ERA5, and a refusal on
    // nested HRRR — the card says which, in the engine's own words.
    crate::storm::follow_card_ui(
        ui,
        advanced_model,
        draft.effective_route(),
        draft.effective_source(),
        &moving_nest,
        &mut actions.storm,
    );
    crate::storm::spawn_card_ui(ui, advanced_model, &mut actions.storm);

    kit::section(ui, "Physics");
    // Full-width selectable rows: profile names must be READABLE, and
    // the maturity dot + caveat ride each row.
    {
        let default_selected = draft.physics_profile.is_none();
        if ui
            .add_sized(
                egui::vec2(ui.available_width(), 22.0),
                egui::Button::selectable(default_selected, "Engine default"),
            )
            .on_hover_text(
                "The route's own default profile — reported in the review's \
                 automatic resolutions",
            )
            .clicked()
        {
            draft.physics_profile = None;
        }
        for profile in PHYSICS_PROFILES {
            let selected = draft.physics_profile.as_deref() == Some(profile.id);
            let mut response = ui.add_sized(
                egui::vec2(ui.available_width(), 22.0),
                egui::Button::selectable(selected, profile.short)
                    .wrap_mode(egui::TextWrapMode::Truncate),
            );
            let center = egui::pos2(response.rect.right() - 10.0, response.rect.center().y);
            ui.painter()
                .circle_filled(center, 2.5, maturity_color(profile.maturity));
            response = response.on_hover_text(format!(
                "{}\n{} · {}",
                profile.id, profile.maturity, profile.note
            ));
            if response.clicked() {
                draft.physics_profile = Some(profile.id.to_string());
            }
        }
    }
    if let Some(profile) = &draft.physics_profile {
        kit::status_line(ui, profile);
        if let Some(preset) = PHYSICS_PROFILES.iter().find(|preset| preset.id == *profile) {
            ui.label(
                egui::RichText::new(preset.note)
                    .color(theme().text_weak)
                    .size(10.5),
            );
        }
    }
    inline_refusal(
        ui,
        estimate,
        &["physics-profile", "physics_profile", "profile"],
    );
    if draft.custom.is_some() {
        ui.label(
            egui::RichText::new(
                "custom (experimental) — the active TOML may diverge from any \
                 profile's coherent set; chips describe regeneration only",
            )
            .color(theme().warn)
            .size(10.0),
        );
    }

    kit::section(ui, "Products");
    if draft.effective_route() == "prepared" {
        use crate::draft::RenderMode;
        let modes: [(RenderMode, &str, String); 4] = [
            (
                RenderMode::EngineDefault,
                "Engine",
                "The chain's own default render set, untouched".into(),
            ),
            (RenderMode::All, "All", "Render the whole catalog".into()),
            (
                RenderMode::Favorites,
                "Favs",
                format!("Your starred products ({})", favorite_products.len()),
            ),
            (
                RenderMode::Skip,
                "None",
                "Skip the render stage entirely".into(),
            ),
        ];
        let chips: Vec<kit::Chip> = modes
            .iter()
            .map(|(mode, label, hover)| kit::Chip {
                label,
                selected: draft.render_mode == *mode,
                hover: Some(hover.clone()),
                dot: None,
            })
            .collect();
        if let Some(clicked) = kit::chip_grid(ui, &chips) {
            draft.render_mode = modes[clicked].0;
        }
        if ui
            .button(if draft.render_mode == crate::draft::RenderMode::Custom {
                format!("Choose products… ({} picked)", draft.render_custom.len())
            } else {
                "Choose products…".to_string()
            })
            .on_hover_text(
                "The renderer's own catalog — picking products switches to an \
                 explicit list submitted verbatim as the render_products run \
                 option",
            )
            .clicked()
        {
            actions.open_products = true;
        }
        match draft.render_products_option(favorite_products) {
            Some(value) => kit::status_line(ui, &format!("render_products = {value}")),
            None => kit::status_line(
                ui,
                if draft.render_mode == crate::draft::RenderMode::EngineDefault {
                    "end-of-run render: the chain's default set"
                } else {
                    "selection empty — the key is omitted (engine default set)"
                },
            ),
        }
    } else {
        kit::status_line(
            ui,
            "render selection is a prepared-route run option (GFS/HRRR); \
             the ERA5 experiment route renders its own defaults",
        );
    }

    kit::section(ui, "Domain");
    match &draft.domain {
        Some(domain) => {
            kit::status_line(
                ui,
                &format!(
                    "center {:.3}N {:.3}W · sketch {:.0} × {:.0} km",
                    domain.ref_lat,
                    -domain.ref_lon,
                    domain.width_km(),
                    domain.height_km()
                ),
            );
            ui.label(
                egui::RichText::new(
                    "size is FITTED by the engine to the VRAM budget — \
                     the review shows the fitted grid",
                )
                .weak()
                .size(10.5),
            );
        }
        None => {
            ui.label(egui::RichText::new("Draw a domain on the map (D)").color(theme().text_weak));
        }
    }

    // Advanced: everything else, honestly labelled.
    ui.add_space(6.0);
    egui::CollapsingHeader::new("Advanced settings")
        .default_open(false)
        .show(ui, |ui| {
            optional_value_row(
                ui,
                "VRAM budget",
                &mut draft.vram_gib,
                "engine default (24 GB card class)",
                1.0..=192.0,
                0.5,
                " GiB",
            );
            kit::row(ui, "GPU device", |ui| {
                let label = match draft.device {
                    None => "engine default".to_string(),
                    Some(index) => format!("device {index}"),
                };
                egui::ComboBox::from_id_salt("device_pick")
                    .selected_text(label)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut draft.device, None, "engine default");
                        if let Some(probe) = probe {
                            for device in &probe.devices {
                                if let Some(index) = device.index {
                                    ui.selectable_value(
                                        &mut draft.device,
                                        Some(index),
                                        format!("{index}: {}", device.name),
                                    );
                                }
                            }
                        }
                    });
            });
        });

    resource_strip(ui, probe, estimate, estimate_pending, estimate_stale);

    // KNOWN-UNRUNNABLE COMBO: blocked up front with the reason and the
    // one-click remedy — never accept-and-fail-later (the regression's
    // default-settings GFS 12-3 launched into the engine's refusal).
    let config_domains = advanced.map(|state| {
        let mut count = 0;
        while state
            .model
            .entries
            .iter()
            .any(|entry| entry.table == format!("domain[{count}]"))
        {
            count += 1;
        }
        count
    });
    let route_block = draft.route_block(config_domains, &moving_nest);
    if let Some(block) = &route_block {
        ui.add_space(4.0);
        ui.add(
            egui::Label::new(
                egui::RichText::new(block.as_str())
                    .color(theme().alert)
                    .size(10.5),
            )
            .wrap(),
        );
        // THE REMEDY HAS TO MATCH THE ROW. Dropping the nests does not
        // fix a moving nest the engine cannot feed — it deletes the
        // thing you were trying to move — so the moving-nest row gets
        // the remedy its own sentence promises.
        if matches!(moving_nest, crate::storm::MovingNest::Still) {
            if ui
                .button("Single domain — one click")
                .on_hover_text(
                    "Drop the nests; the config surface follows by itself and \
                     your drawn root stays",
                )
                .clicked()
            {
                actions.make_single_domain = true;
            }
        } else if ui
            .button("Turn storm following off — one click")
            .on_hover_text(
                "Strip [relocation] from the config: the nest stays where \
                 it is and the run goes ahead. Your ladder and geometry \
                 are untouched.",
            )
            .clicked()
        {
            actions.storm.apply_follow = Some(None);
        }
    }

    // What Run means for the forcing data, said BEFORE the launch (the
    // engine's contract: a plan with a fetch block downloads its own;
    // without one the data has to be there). Sizes are the engine's
    // estimate numbers above; nothing invented here.
    if let Some(forcing) = draft.forcing_plan() {
        use crate::draft::ForcingPlan;
        ui.add_space(4.0);
        match &forcing {
            ForcingPlan::RouteFetches { source } => {
                kit::status_line(
                    ui,
                    &format!(
                        "{} chain fetches its own data at launch",
                        source.as_deref().unwrap_or("the route's")
                    ),
                );
            }
            ForcingPlan::OnDisk => {
                kit::status_line(ui, "forcing on disk — no download");
            }
            ForcingPlan::Promote { source, hours, .. } => {
                let window = hours
                    .as_deref()
                    .map(|hours| format!(" · {hours} h window"))
                    .unwrap_or_default();
                kit::status_line(
                    ui,
                    &format!(
                        "will download the forcing first ({}{window}) — \
                         promoted from the config's own [fetch] hints",
                        source.as_deref().unwrap_or("?")
                    ),
                );
                if source.as_deref() == Some("era5") && !crate::settings::cds_key_present() {
                    ui.colored_label(theme().warn, "ERA5 download needs your CDS key — Sys panel");
                }
            }
            ForcingPlan::MissingNoFetch => {
                ui.colored_label(
                    theme().warn,
                    "declared forcing missing and the config has no [fetch] \
                     hints — the engine will refuse at launch",
                );
            }
        }
    }

    ui.add_space(8.0);
    let ready = draft.validate();
    let advanced_reachable = ready.is_ok() || draft.custom.is_some();
    let advanced_hover = if draft.custom.is_some() {
        "The engine-generated TOML, every value editable, engine-validated \
         on each edit"
    } else {
        "Have the engine write the FULL config for this intent (dry run), \
         then edit every exact value — grid, vertical levels, timestep, \
         physics switches, damping, output"
    };
    if ui
        .add_enabled(
            advanced_reachable,
            egui::Button::new("Advanced config (every engine knob)…")
                .min_size(egui::vec2(ui.available_width(), 26.0)),
        )
        .on_hover_text(advanced_hover)
        .on_disabled_hover_text(
            "Draw a domain first — the engine needs the intent to write a config",
        )
        .clicked()
    {
        actions.open_advanced = true;
    }
    match &ready {
        Ok(_) => {
            if ui
                .add_sized(
                    egui::vec2(ui.available_width(), 30.0),
                    egui::Button::new(egui::RichText::new("Review plan").strong()),
                )
                .on_hover_text(
                    "See the resolved plan — every value the engine chose — before launching",
                )
                .clicked()
            {
                actions.open_review = true;
            }
        }
        Err(reason) if draft.custom.is_some() => {
            // Custom plans review on the edited file, not the intent.
            if ui
                .add_sized(
                    egui::vec2(ui.available_width(), 30.0),
                    egui::Button::new(egui::RichText::new("Review plan").strong()),
                )
                .clicked()
            {
                actions.open_review = true;
            }
            let _ = reason;
        }
        Err(reason) => {
            ui.add_enabled(
                false,
                egui::Button::new("Review plan").min_size(egui::vec2(ui.available_width(), 30.0)),
            )
            .on_disabled_hover_text(reason.clone());
        }
    }
    let _ = review;
}

fn format_dx_km(dx_km: f64) -> String {
    if dx_km >= 1.0 {
        format!("{dx_km} km")
    } else {
        format!("{} m", dx_km * 1000.0)
    }
}

struct DomainRowData {
    grid_id: u32,
    nx: u32,
    ny: u32,
    /// `(i, j, parent grid)` for nests; `None` for the root.
    anchor: Option<(String, String, u32)>,
    manual: bool,
    resettable: bool,
}

/// One row per domain (root + every child): a selectable id chip linked
/// to the map selection, TYPED nx × ny size fields riding the same
/// placement writer as a map resize, the auto(fitted)/manual chip and
/// reset. Sources: the working config when it exists, else the displayed
/// (fitted or placeholder) tree — never an empty surface.
#[allow(clippy::too_many_arguments)]
fn domain_rows(
    ui: &mut egui::Ui,
    draft: &Draft,
    advanced: Option<&crate::advanced::AdvancedState>,
    tree: &[arwen_plan::queries::ResolvedDomain],
    tree_provisional: bool,
    selected_nest: Option<u32>,
    actions: &mut InspectorActions,
) {
    let mut rows: Vec<DomainRowData> = Vec::new();
    let floors = advanced
        .map(crate::advanced::floor_hints)
        .unwrap_or_default();
    if let Some(state) = advanced {
        for index in 0.. {
            let value = |key: &str| {
                state
                    .model
                    .domain_value(index, key)
                    .map(str::trim)
                    .map(str::to_string)
            };
            let parse_u32 = |key: &str| value(key).and_then(|v| v.parse::<u32>().ok());
            let Some(grid_id) = parse_u32("grid_id") else {
                break;
            };
            let (Some(nx), Some(ny)) = (parse_u32("nx"), parse_u32("ny")) else {
                continue;
            };
            let parent = parse_u32("parent_id").unwrap_or(0);
            let anchor = (parent != 0).then(|| {
                (
                    value("i_parent_start").unwrap_or_else(|| "?".into()),
                    value("j_parent_start").unwrap_or_else(|| "?".into()),
                    parent,
                )
            });
            rows.push(DomainRowData {
                grid_id,
                nx,
                ny,
                anchor,
                manual: crate::advanced::placement_is_manual(
                    &state.model,
                    &state.base_model,
                    index,
                ),
                resettable: true,
            });
        }
    } else {
        for domain in tree {
            let anchor = (domain.parent_id != 0).then(|| {
                (
                    format!("{:.0}", domain.i_parent_start),
                    format!("{:.0}", domain.j_parent_start),
                    domain.parent_id,
                )
            });
            rows.push(DomainRowData {
                grid_id: domain.grid_id,
                nx: domain.nx,
                ny: domain.ny,
                anchor,
                manual: false,
                resettable: false,
            });
        }
    }
    if rows.is_empty() {
        if draft.domain.is_some() {
            kit::status_line(
                ui,
                "domain sizes appear here the moment the engine fits the tree",
            );
        }
        return;
    }
    // Domains the engine's refusal names (its own sentence per line) —
    // flagged on their rows, mirrored on the map's red outlines.
    let refusals: Vec<(u32, String)> = advanced
        .and_then(|state| match &state.resolve {
            Some(Err(error)) => Some(crate::advanced::refusal_named_domains(error)),
            _ => None,
        })
        .unwrap_or_default();
    for row in &rows {
        ui.horizontal(|ui| {
            ui.set_min_height(crate::theme::ROW_H);
            let chip_hover = match &row.anchor {
                Some((i, j, parent)) => format!(
                    "select d{:02} on the map — anchored ({i}, {j}) in d{parent:02}",
                    row.grid_id
                ),
                None => "the root (parent) domain".to_string(),
            };
            let response = ui
                .add(egui::Button::selectable(
                    selected_nest == Some(row.grid_id) && row.anchor.is_some(),
                    format!("d{:02}", row.grid_id),
                ))
                .on_hover_text(chip_hover);
            if response.clicked() && row.anchor.is_some() {
                actions.select_nest = Some(row.grid_id);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if row.manual && row.resettable {
                    let (label, hover) = if row.anchor.is_none() {
                        (
                            "fit for me",
                            "Give the root size back to the engine's VRAM fit \
                             — the drawn center stays yours",
                        )
                    } else {
                        (
                            "reset to fitted",
                            "Restore the engine's own emitted anchor + size \
                             for this domain",
                        )
                    };
                    if ui.button(label).on_hover_text(hover).clicked() {
                        actions.reset_placement = Some(row.grid_id);
                    }
                    ui.label(
                        egui::RichText::new("manual")
                            .color(theme().accent_text)
                            .size(10.5),
                    )
                    .on_hover_text(
                        "Size/placement set by you (typed or dragged); the \
                         engine ratified it on the last resolve",
                    );
                } else {
                    ui.label(
                        egui::RichText::new(if tree_provisional {
                            "sketch"
                        } else {
                            "auto (fitted)"
                        })
                        .color(theme().text_weak)
                        .size(10.5),
                    )
                    .on_hover_text(if tree_provisional {
                        "A draft placeholder — the engine's fit replaces it \
                         within one resolve"
                    } else {
                        "The engine's own fitted size/placement — type a size \
                         or drag on the map to take it manual"
                    });
                }
                // Typed size: FREE whole integers; the engine's reported
                // floors are the min, its resolve stays the judge.
                let (min_x, min_y) = if row.anchor.is_none() {
                    let (fx, fy) = floors.root.unwrap_or((16, 16));
                    (fx, fy)
                } else {
                    let span = floors.nest_span.unwrap_or(2);
                    (span, span)
                };
                let (mut nx, mut ny) = (row.nx, row.ny);
                let mut changed = false;
                let hover_y = format!("ny — free whole mass points (engine floor ≥ {min_y})");
                changed |= ui
                    .add(egui::DragValue::new(&mut ny).range(min_y..=8192).speed(1))
                    .on_hover_text(hover_y)
                    .changed();
                ui.label(egui::RichText::new("×").color(theme().text_weak));
                let hover_x = format!("nx — free whole mass points (engine floor ≥ {min_x})");
                changed |= ui
                    .add(egui::DragValue::new(&mut nx).range(min_x..=8192).speed(1))
                    .on_hover_text(hover_x)
                    .changed();
                ui.label(
                    egui::RichText::new("size")
                        .color(theme().text_weak)
                        .size(10.0),
                );
                if changed {
                    actions.set_domain_size = Some((row.grid_id, nx, ny));
                }
            });
        });
        // The last root change's mechanical repair of THIS domain
        // (carried along / refit / left for the engine), said on the row.
        if let Some(repair) = advanced.and_then(|state| {
            state
                .repairs
                .iter()
                .find(|repair| repair.grid_id == row.grid_id)
        }) {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(&repair.notice)
                        .color(theme().warn)
                        .size(10.0),
                )
                .wrap(),
            );
        }
        // The engine's refusal line naming this domain, verbatim.
        if let Some((_, line)) = refusals.iter().find(|(grid, _)| *grid == row.grid_id) {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(format!("{line} — see map"))
                        .color(theme().alert)
                        .size(10.0),
                )
                .wrap(),
            );
        }
    }
    kit::status_line(
        ui,
        "click a d0N chip to select it on the map · drag to move, handles \
         to resize, or type sizes here",
    );
    if advanced.is_none() {
        kit::status_line(
            ui,
            "a typed size or drag opens the engine's config surface (one \
             source of truth) and rides its validation",
        );
    }
}

/// The statics-corridor line for the resource strip, or `None` when
/// this plan moves no nest (or the engine does not price one).
///
/// HOST AND DISK, NEVER VRAM. The engine's basis says it outright — "a
/// corridor is cropped on the host, so the VRAM estimate above is
/// unchanged by it" — and the two arms of a real GFS tree agree to the
/// byte, so this line is rendered BESIDE the VRAM figure with the word
/// "host/disk" on it and is never summed into the card's budget. The
/// per-domain extents come from the engine's own `domains[]` because
/// the number is otherwise unexplainable on sight: a 408×320 nest at
/// ratio 4 on a 204×162 root costs an 816×648 corridor, which is the
/// parent's extent at the child's resolution.
///
/// Separated from the widget so the matrix can assert on the exact
/// sentence the user reads.
pub fn corridor_line(estimate: &EstimateReport) -> Option<(String, &str)> {
    let corridor = estimate.corridor.as_ref()?;
    if !corridor.is_priced() {
        return None;
    }
    let bytes = corridor.host_bytes.unwrap_or_default();
    let extents: Vec<String> = corridor
        .domains
        .iter()
        .filter_map(|domain| {
            Some(format!(
                "{} {}×{}",
                domain.domain, domain.corridor_nx?, domain.corridor_ny?
            ))
        })
        .collect();
    let detail = if extents.is_empty() {
        String::new()
    } else {
        format!(" · {}", extents.join(", "))
    };
    Some((
        format!(
            "corridor {} host/disk{detail} — adds no VRAM",
            kit::format_bytes(bytes)
        ),
        corridor.basis.as_str(),
    ))
}

/// The fixed resource strip: engine numbers verbatim, bases on hover,
/// never an invented figure — and never a number POSING as current
/// pricing for a draft it was not computed from. `stale` dims every
/// number and shows the re-pricing spinner; the numbers stay visible
/// (labelled as the previous draft's) until the engine answers.
fn resource_strip(
    ui: &mut egui::Ui,
    probe: Option<&ProbeReport>,
    estimate: Option<&Result<EstimateReport, String>>,
    pending: bool,
    stale: bool,
) {
    kit::section(ui, "Resources");
    if stale {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new().size(12.0));
            let label = if pending {
                "re-pricing for the current draft…"
            } else {
                "numbers below are the PREVIOUS draft's — finish the draft to re-price"
            };
            ui.colored_label(theme().warn, label);
        });
    }
    match estimate {
        None if pending => {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(12.0));
                kit::status_line(ui, "estimating…");
            });
        }
        None => kit::status_line(ui, "estimate appears once a domain exists"),
        Some(Err(error)) => {
            // An errored estimate is a STATE, not a reason to keep old
            // numbers on screen. A refusal that NAMES a domain gets a
            // headline pointing at the map instead of a bare
            // query-failed line; the engine's sentence stays verbatim.
            let color = if stale {
                theme().text_weak
            } else {
                theme().alert
            };
            let named = crate::advanced::refusal_named_domains(error);
            if named.is_empty() {
                ui.colored_label(color, format!("estimate failed: {error}"));
            } else {
                let names: Vec<String> = named
                    .iter()
                    .map(|(grid, _)| format!("d{grid:02}"))
                    .collect();
                ui.colored_label(
                    color,
                    format!(
                        "placement refused — {} outlined on the map",
                        names.join(", ")
                    ),
                );
                for (_, line) in &named {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(line.as_str()).color(color).size(10.0),
                        )
                        .wrap(),
                    );
                }
            }
        }
        Some(Ok(estimate)) => {
            let dim = |color: egui::Color32| if stale { theme().text_weak } else { color };
            // VRAM vs the probed card.
            let free = probe
                .and_then(|probe| probe.devices.first())
                .and_then(|device| device.memory_free_bytes);
            if let Some(vram) = estimate.vram.estimate_bytes {
                let (color, verdict) = match free {
                    Some(free) if vram > free => (theme().alert, " — exceeds free VRAM"),
                    Some(free) if vram as f64 > free as f64 * 0.9 => {
                        (theme().warn, " — tight against free VRAM")
                    }
                    _ => (theme().text, ""),
                };
                let text = match free {
                    Some(free) => format!(
                        "VRAM {} of {} free{verdict}",
                        kit::format_bytes(vram),
                        kit::format_bytes(free)
                    ),
                    None => format!("VRAM {}", kit::format_bytes(vram)),
                };
                ui.colored_label(dim(color), kit::value_text(&text))
                    .on_hover_text(&estimate.vram.basis);
            }
            // MOVING NEST: the statics corridor, beside the VRAM figure
            // and never inside it.
            if let Some((line, basis)) = corridor_line(estimate) {
                ui.colored_label(dim(theme().accent_text), kit::value_text(&line))
                    .on_hover_text(basis);
            }
            // Disk: exact frames, honest null bytes.
            let frames = estimate.disk.total_frames.unwrap_or(0);
            ui.colored_label(
                dim(theme().text),
                kit::value_text(&format!("disk {frames} frames · bytes not measured")),
            )
            .on_hover_text(&estimate.disk.basis);
            // Download + wall time: null with basis.
            let download = match estimate.download.bytes {
                Some(bytes) => format!("download {}", kit::format_bytes(bytes)),
                None => "download —".to_string(),
            };
            ui.colored_label(dim(theme().text), kit::value_text(&download))
                .on_hover_text(&estimate.download.basis);
            let wall = match estimate.wall_time.seconds {
                Some(seconds) => format!("runtime ~{}", kit::format_duration_s(seconds)),
                None => "runtime — (measured from first steps)".to_string(),
            };
            ui.colored_label(dim(theme().text), kit::value_text(&wall))
                .on_hover_text(&estimate.wall_time.basis);
        }
    }
}

/// The review sheet: the RESOLVED plan including every automatic
/// resolution, over the inspector.
pub fn review_sheet_ui(
    ctx: &egui::Context,
    review: &ReviewSheet,
    route_warning: Option<&str>,
    actions: &mut InspectorActions,
) {
    let mut open = true;
    egui::Window::new("Review resolved plan")
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_width(520.0)
        .max_height(560.0)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            let report = &review.report;
            let plan = &report.plan;
            kit::status_line(
                ui,
                &format!(
                    "{} · route {} · run dir {}",
                    plan["name"].as_str().unwrap_or("?"),
                    plan["route"].as_str().unwrap_or("?"),
                    plan["run_dir"].as_str().unwrap_or("?")
                ),
            );
            if let Some(args) = plan["fetch_args"]
                .as_array()
                .or_else(|| plan["fetch"]["args"].as_array())
            {
                let joined = args
                    .iter()
                    .filter_map(|value| value.as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
                kit::status_line(ui, &format!("fetch {joined}"));
            }
            // The ENGINE-fitted grid: the answer to the sketch.
            if let Some(fitted) = &review.fitted {
                ui.label(
                    egui::RichText::new(format!(
                        "fitted {} × {} @ {:.0} m  ·  {:.0} × {:.0} km",
                        fitted.nx,
                        fitted.ny,
                        fitted.dx_m,
                        fitted.width_km(),
                        fitted.height_km()
                    ))
                    .strong()
                    .color(theme().accent_text),
                )
                .on_hover_text(
                    "Domain size is fitted by the wizard's VRAM estimator — \
                     an output, not an input; the map outline shows this grid",
                );
            }
            // The fitted NESTS, placement verbatim from the engine.
            for nest in review.tree.iter().filter(|domain| domain.parent_id != 0) {
                let cadence = nest
                    .history_interval_s
                    .map(|seconds| format!(" · writes every {seconds} s"))
                    .unwrap_or_default();
                let dt = nest
                    .dt_s
                    .map(|dt| format!(" · dt {dt} s (derived)"))
                    .unwrap_or_default();
                ui.label(
                    egui::RichText::new(format!(
                        "└ d{:02} {} × {} @ {:.0} m · anchored ({:.0},{:.0}) in d{:02}{cadence}{dt}",
                        nest.grid_id,
                        nest.nx,
                        nest.ny,
                        nest.dx_m,
                        nest.i_parent_start,
                        nest.j_parent_start,
                        nest.parent_id,
                    ))
                    .color(theme().accent_text),
                )
                .on_hover_text(
                    "Nest size and placement are engine outputs (fitted to the \
                     VRAM budget, centered on the drawn point); the map draws \
                     this outline",
                );
            }
            if report.inputs_present == Some(false) {
                kit::status_line(
                    ui,
                    "inputs not fetched yet — the fetch stage downloads them at run time",
                );
            }
            // The wizard-written config: the one thing the caller never
            // typed, so it must be shown.
            if let Some(generated) = &report.generated_config {
                egui::CollapsingHeader::new("Generated config (wizard-written TOML)")
                    .default_open(false)
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(generated).monospace().size(10.0),
                                )
                                .wrap(),
                            );
                        });
                    });
            }

            kit::section(ui, "Every value the engine chose");
            egui::ScrollArea::vertical()
                .max_height(300.0)
                .show(ui, |ui| {
                    egui::Grid::new("resolutions")
                        .num_columns(3)
                        .striped(true)
                        .min_col_width(60.0)
                        .show(ui, |ui| {
                            for resolution in &report.automatic_resolutions {
                                let scope = match resolution.extra.get("grid_id") {
                                    Some(grid) => format!("{} d{}", resolution.scope, grid),
                                    None => resolution.scope.clone(),
                                };
                                ui.label(kit::value_text(&scope));
                                ui.label(kit::value_text(&resolution.key));
                                let value = match &resolution.value {
                                    serde_json::Value::String(text) => text.clone(),
                                    other => other.to_string(),
                                };
                                let mut response = ui.label(kit::value_text(&value));
                                response = response.on_hover_text(&resolution.basis);
                                if let Some(note) = &resolution.note {
                                    response.on_hover_text(note);
                                }
                                ui.end_row();
                            }
                        });
                });

            if !report.warnings.is_empty() {
                kit::section(ui, "Warnings");
                for warning in &report.warnings {
                    let text = warning["action"]
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| warning.to_string());
                    ui.colored_label(theme().warn, text);
                }
            }

            ui.add_space(8.0);
            if let Some(warning) = route_warning {
                // A KNOWN refusal never gets a launchable button: the
                // reason (the engine's own sentence) and the remedies
                // render where the button is, and the button disables.
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(warning).color(theme().warn).size(10.5),
                    )
                    .wrap(),
                );
            }
            ui.horizontal(|ui| {
                let launch = ui.add_enabled(
                    route_warning.is_none(),
                    egui::Button::new(egui::RichText::new("Launch forecast").strong()),
                );
                let launch = match route_warning {
                    Some(reason) => launch.on_disabled_hover_text(reason),
                    None => launch,
                };
                if launch.clicked() {
                    actions.launch = true;
                }
                if route_warning.is_some()
                    && ui
                        .button("Single domain — one click")
                        .on_hover_text(
                            "Drop the nests; the config surface follows by \
                             itself and your drawn root stays",
                        )
                        .clicked()
                {
                    actions.make_single_domain = true;
                    actions.close_review = true;
                }
                if ui.button("Close").clicked() {
                    actions.close_review = true;
                }
            });
        });
    if !open {
        actions.close_review = true;
    }
}

/// The inspector while a run is selected (running or analyze).
pub fn run_ui(ui: &mut egui::Ui, session: &RunSession, actions: &mut InspectorActions) {
    kit::section(ui, "Run");
    let (liveness, healthy) = session.liveness();
    let color = if healthy { theme().live } else { theme().alert };
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(&session.record.name).strong());
        ui.colored_label(color, liveness);
    });
    kit::status_line(
        ui,
        &format!(
            "launched {} · {} · {}",
            session.record.launched_at_utc,
            session.record.ownership,
            if session.record.fixture {
                "fixture replay"
            } else {
                "live gpuwm"
            }
        ),
    );
    if let Some(poisoned) = &session.stream_poisoned {
        ui.colored_label(theme().alert, poisoned);
    }
    if let Some(Terminal::Failed { error, remedy }) = &session.terminal {
        ui.colored_label(theme().alert, error);
        if let Some(remedy) = remedy {
            ui.colored_label(theme().warn, format!("remedy: {remedy}"));
        }
    }

    kit::section(ui, "Stages");
    for stage in &session.stages {
        let (color, mark) = match stage.status {
            StageStatus::Running => (theme().accent, "▶"),
            StageStatus::Ok => (theme().live, "✓"),
            StageStatus::Failed => (theme().alert, "×"),
        };
        let seconds = stage
            .wall_seconds
            .map(|seconds| format!(" {}", kit::format_duration_s(seconds)))
            .unwrap_or_default();
        let response = ui.colored_label(color, format!("{mark} {}{seconds}", stage.id));
        if !stage.phases.is_empty() {
            response.on_hover_text(stage.phases.join(" → "));
        }
    }

    if session.progress.model_seconds > 0.0 && session.terminal.is_none() {
        kit::section(ui, "Model");
        if let Some(fraction) = session.progress.fraction() {
            ui.add(egui::ProgressBar::new(fraction as f32).show_percentage());
        }
        let speed = session
            .progress
            .speed_x
            .map(|speed| format!("{speed:.1}× realtime"))
            .unwrap_or_else(|| "speed —".into());
        let sampling = if session.progress.polled {
            " · polled from stage progress"
        } else {
            ""
        };
        kit::status_line(
            ui,
            &format!(
                "t+{} · {speed}{sampling}",
                kit::format_duration_s(session.progress.model_seconds)
            ),
        );
    }

    kit::section(ui, "Outputs");
    kit::status_line(ui, &format!("{} frames committed", session.outputs.len()));
    if let Some((index, frame)) = session.display_frame() {
        kit::status_line(
            ui,
            &format!(
                "showing {} ({}/{})",
                frame.valid_time_utc.format("%H:%M:%SZ"),
                index + 1,
                session.outputs.len()
            ),
        );
        if session.selected_frame.is_some() && ui.button("Follow latest").clicked() {
            actions.select_frame = Some(None);
        }
    }

    if !session.warnings.is_empty() {
        kit::section(ui, "Warnings");
        for warning in session.warnings.iter().rev().take(6) {
            ui.colored_label(theme().warn, warning);
        }
    }
    if !session.resolutions.is_empty() {
        egui::CollapsingHeader::new("Every value the engine chose")
            .default_open(false)
            .show(ui, |ui| {
                for resolution in &session.resolutions {
                    kit::status_line(
                        ui,
                        &format!(
                            "{}.{} = {}",
                            resolution.scope, resolution.key, resolution.value
                        ),
                    );
                }
            });
    }

    ui.add_space(8.0);
    if ui.button("New forecast").clicked() {
        actions.back_to_design = true;
    }
}

/// The runs registry list.
pub fn runs_ui(ui: &mut egui::Ui, runs: &[RunEntry], actions: &mut InspectorActions) {
    kit::section(ui, "Runs");
    if runs.is_empty() {
        kit::status_line(ui, "no runs yet — design one on the map");
        return;
    }
    for (index, entry) in runs.iter().enumerate() {
        let label = format!("{}  ·  {}", entry.record.name, entry.record.launched_at_utc);
        if ui
            .add(egui::Button::new(label).min_size(egui::vec2(ui.available_width(), 24.0)))
            .on_hover_text(format!(
                "{} · plan {}",
                entry.record.ownership,
                &entry.record.plan_sha256[..12.min(entry.record.plan_sha256.len())]
            ))
            .clicked()
        {
            actions.open_run = Some(index);
        }
    }
}

/// One editable settings row: label + full-width text field over an
/// optional string.
fn optional_path_row(ui: &mut egui::Ui, label: &str, value: &mut Option<String>) -> bool {
    let mut text = value.clone().unwrap_or_default();
    let changed = kit::row(ui, label, |ui| {
        ui.add(egui::TextEdit::singleline(&mut text).desired_width(ui.available_width()))
            .changed()
    });
    if changed {
        *value = (!text.trim().is_empty()).then(|| text.trim().to_string());
    }
    changed
}

/// The system view: probe truth verbatim, plus the minimal settings
/// surface (engine python, geog root, renderer, fixture/live toggle) —
/// no more hand-edited settings.json.
pub fn system_ui(
    ui: &mut egui::Ui,
    probe: Option<&Result<ProbeReport, String>>,
    settings: &mut crate::settings::StudioSettings,
    actions: &mut InspectorActions,
) {
    kit::section(ui, "Studio");
    kit::status_line(
        ui,
        &format!(
            "build {} — launch via the Desktop shortcut (always newest)",
            crate::app::build_stamp()
        ),
    );
    kit::section(ui, "Engine");
    let live = settings.contract_mode == "live";
    kit::row(ui, "Contract source", |ui| {
        if ui
            .add(egui::Button::selectable(live, "Live"))
            .on_hover_text("Drive the real gpuwm run-plan CLI")
            .clicked()
        {
            settings.contract_mode = "live".into();
        }
        if ui
            .add(egui::Button::selectable(!live, "Fixtures"))
            .on_hover_text("Canned engine replies + replayed runs (no engine needed)")
            .clicked()
        {
            settings.contract_mode = "fixture".into();
        }
    });
    if live {
        kit::row(ui, "Engine python", |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut settings.live_program)
                    .desired_width(ui.available_width()),
            )
            .on_hover_text("python.exe of the venv where gpuwm is installed");
        });
        optional_path_row(ui, "WPS_GEOG tree", &mut settings.geog_root);
        optional_path_row(ui, "Rust renderer", &mut settings.rust_renderer);
        let (renderer, healthy) = settings.renderer_status();
        ui.colored_label(
            if healthy { theme().live } else { theme().warn },
            format!("render engine: {renderer}"),
        )
        .on_hover_text(
            "Which engine draws the run's product PNGs. Per-run confirmation \
             from the event stream is an open engine request; this line is \
             Studio's configuration truth.",
        );
    }
    kit::row(ui, "Output root", |ui| {
        ui.add(
            egui::TextEdit::singleline(&mut settings.output_root)
                .desired_width(ui.available_width()),
        );
    });

    kit::section(ui, "ERA5 credentials");
    if crate::settings::cds_key_present() {
        ui.colored_label(theme().live, "CDS key present (~/.cdsapirc)");
    } else {
        ui.colored_label(theme().warn, "no CDS key — ERA5 acquisition needs one");
    }
    kit::row(ui, "CDS API key", |ui| {
        ui.add(
            egui::TextEdit::singleline(&mut settings.cds_key_entry)
                .password(true)
                .hint_text("paste your CDS API key")
                .desired_width(ui.available_width()),
        );
    });
    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                !settings.cds_key_entry.trim().is_empty(),
                egui::Button::new("Save key"),
            )
            .on_hover_text(
                "Writes ~/.cdsapirc (url + key) — the standard file the \
                 cdsapi library reads",
            )
            .clicked()
        {
            actions.save_cds_key = true;
        }
        ui.label(
            egui::RichText::new("saves to ~/.cdsapirc")
                .color(theme().text_weak)
                .size(10.5),
        );
    });
    ui.label(
        egui::RichText::new(
            "The key is mirrored into the sealed engine environment at each \
             launch (file + CDSAPI_* variables). It is never logged and never \
             written into settings, plans, or event files.",
        )
        .color(theme().text_weak)
        .size(10.0),
    );
    ui.add_space(4.0);
    if ui
        .button("Save settings")
        .on_hover_text("Persist to settings.json and reconnect the engine")
        .clicked()
    {
        actions.save_settings = true;
    }
    match probe {
        None => kit::status_line(ui, "probing…"),
        Some(Err(error)) => {
            ui.colored_label(theme().alert, format!("probe failed: {error}"));
        }
        Some(Ok(probe)) => {
            kit::status_line(
                ui,
                &format!(
                    "gpuwm {} · python {}",
                    probe.gpuwm_version.as_deref().unwrap_or("?"),
                    probe.python.as_deref().unwrap_or("?")
                ),
            );
            kit::section(ui, "GPUs");
            if let Some(error) = &probe.device_query_error {
                ui.colored_label(theme().alert, error);
            }
            for device in &probe.devices {
                ui.label(egui::RichText::new(&device.name).strong());
                let free = device
                    .memory_free_bytes
                    .map(kit::format_bytes)
                    .unwrap_or_else(|| "—".into());
                let total = device
                    .memory_total_bytes
                    .map(kit::format_bytes)
                    .unwrap_or_else(|| "—".into());
                kit::status_line(
                    ui,
                    &format!(
                        "{free} free of {total} · driver {}",
                        device.driver_version.as_deref().unwrap_or("?")
                    ),
                );
            }
            if let Some(basis) = &probe.device_query_basis {
                ui.label(egui::RichText::new(basis).weak().size(10.0));
            }
            kit::section(ui, "Routes");
            for (route, summary) in &probe.routes {
                ui.label(kit::value_text(route)).on_hover_text(summary);
            }
        }
    }
    ui.add_space(6.0);
    if ui.button("Re-probe").clicked() {
        actions.refresh_probe = true;
    }
}

use chrono::{Datelike, Timelike};
